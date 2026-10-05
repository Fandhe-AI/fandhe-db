//! 比較述語での文字列リテラル（PostgreSQL の unknown 型）の種別不一致の結合テスト
//! （Issue #1408。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・
//! `docs/spec/04-behavior/errors.md` ERR-2・ERR-6）。
//!
//! 数値列（INTEGER／BIGINT／REAL／DOUBLE）× 文字列リテラルは列の型として解釈して受理し、
//! 数値リテラル形と同じ行集合を返すこと（等価性オラクル）、解釈できなければ `22P02`、
//! 比較演算子が存在しない組み合わせは `42883` になることを、実 `Storage`＋`CpuScalarProvider`
//! の production 経路（`EngineCore::execute_sql`）で固定する。テナント境界（他テナント行が
//! 結果・件数に現れないこと、エラーがリテラル本文を反射しないこと）も確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("ext_id", ColumnType::Uuid, true),
            ColumnDef::new("blob", ColumnType::Bytea, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql24-literal-kind-coercion");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed, got {e:?}"));
}

/// 行を 1 件挿入する。`extra` は `(列名, SQL リテラル)` の組（省略した nullable 列は NULL）。
fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, extra: &[(&str, &str)], op: &str) {
    let mut columns = vec!["id".to_string(), "embedding".into(), "lang".into()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".into(), "'ja'".into()];
    for (c, v) in extra {
        columns.push((*c).to_string());
        values.push((*v).to_string());
    }
    exec(
        core,
        ctx,
        &format!(
            "INSERT INTO {TABLE} ({}) VALUES ({}) USING OPERATION_ID '{op}'",
            columns.join(", "),
            values.join(", ")
        ),
    );
}

fn seed(core: &EngineCore, alice: &PolicyContext, bob: &PolicyContext) {
    insert(
        core,
        alice,
        1,
        &[
            ("qty", "1"),
            ("big", "1"),
            ("r", "1.5"),
            ("d", "1.5"),
            ("flag", "true"),
        ],
        "op-1",
    );
    insert(
        core,
        alice,
        2,
        &[
            ("qty", "2"),
            ("big", "2"),
            ("r", "2.5"),
            ("d", "2.5"),
            ("flag", "false"),
        ],
        "op-2",
    );
    // 数値・真偽値列がすべて NULL の行（三値論理による fail-open の検出用）。
    insert(core, alice, 3, &[], "op-3");
    insert(
        core,
        alice,
        4,
        &[("qty", "10"), ("big", "10"), ("r", "10.5"), ("d", "10.5")],
        "op-4",
    );
    // 他テナントの行（alice の結果・件数に現れてはならない）。
    insert(
        core,
        bob,
        9,
        &[("qty", "1"), ("big", "1"), ("flag", "true")],
        "op-9",
    );
}

fn ids(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("query {where_clause:?} should succeed, got {e:?}"));
    let mut out: Vec<u64> = result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn code(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> String {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
    )
    .expect_err(&format!("query {where_clause:?} must be rejected"))
    .wire_code()
    .to_string()
}

#[test]
fn string_literal_matches_numeric_literal_for_every_numeric_column_and_operator() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for (col, lit) in [("qty", "2"), ("big", "2"), ("r", "2.5"), ("d", "2.5")] {
        for op in ["=", "<", "<=", ">", ">="] {
            let numeric = ids(&core, &alice, &format!("{col} {op} {lit}"));
            let quoted = ids(&core, &alice, &format!("{col} {op} '{lit}'"));
            assert_eq!(numeric, quoted, "{col} {op} {lit}");
        }
    }
    assert_eq!(ids(&core, &alice, "qty > '1'"), vec![2, 4]);
    // 逆向き（`'1' < qty`）も同じ結果。
    assert_eq!(ids(&core, &alice, "'1' < qty"), vec![2, 4]);
    assert_eq!(ids(&core, &alice, "'2' = qty"), vec![2]);
}

#[test]
fn string_literal_in_between_not_or_follow_numeric_form_without_null_leak() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let pairs = [
        ("qty IN ('1','2')", "qty IN (1, 2)"),
        ("qty NOT IN ('1','2')", "qty NOT IN (1, 2)"),
        ("qty BETWEEN '1' AND '2'", "qty BETWEEN 1 AND 2"),
        ("qty NOT BETWEEN '1' AND '2'", "qty NOT BETWEEN 1 AND 2"),
        ("NOT qty = '1'", "NOT qty = 1"),
        ("qty = '1' OR qty = '10'", "qty = 1 OR qty = 10"),
        ("NOT (qty = '1' OR qty = '2')", "NOT (qty = 1 OR qty = 2)"),
    ];
    for (quoted, numeric) in pairs {
        assert_eq!(
            ids(&core, &alice, quoted),
            ids(&core, &alice, numeric),
            "{quoted}"
        );
    }
    // NULL 行（id 3）は否定越しでも一致しない。
    assert_eq!(ids(&core, &alice, "NOT qty = '1'"), vec![2, 4]);
    assert_eq!(ids(&core, &alice, "qty NOT IN ('1','2')"), vec![4]);
}

#[test]
fn string_literal_input_grammar_and_error_codes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    assert_eq!(ids(&core, &alice, "qty = ' 1 '"), vec![1]);
    assert_eq!(ids(&core, &alice, "qty = '+1'"), vec![1]);
    assert_eq!(ids(&core, &alice, "qty > '-1'"), vec![1, 2, 4]);
    assert_eq!(ids(&core, &alice, "r = '2.5'"), vec![2]);
    for bad in ["''", "'abc'", "'1.5'", "'1e3'"] {
        assert_eq!(
            code(&core, &alice, &format!("qty = {bad}")),
            "22P02",
            "{bad}"
        );
    }
    // INTEGER の範囲外・BIGINT の厳密表現範囲外は数値リテラル形と同じ `22003`。
    assert_eq!(code(&core, &alice, "qty = '3000000000'"), "22003");
    assert_eq!(code(&core, &alice, "big = '9007199254740993'"), "22003");
    assert_eq!(code(&core, &alice, "big = 9007199254740993"), "22003");
    // REAL／DOUBLE 列でも、2^53 超の整数字面は丸め前に数値リテラル形と同じ `22003` で拒否する。
    for col in ["r", "d"] {
        for lit in [
            "9007199254740993",
            "9007199254740993.0",
            "9.007199254740993e15",
        ] {
            assert_eq!(
                code(&core, &alice, &format!("{col} = '{lit}'")),
                "22003",
                "{col} {lit}"
            );
            assert_eq!(
                code(&core, &alice, &format!("{col} = {lit}")),
                "22003",
                "{col} {lit} numeric"
            );
        }
    }
    // エラーはリテラル本文を反射しない。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE qty = 'secret-literal' LIMIT 1"),
        )
        .unwrap_err();
    assert!(!format!("{err:?}").contains("secret-literal"));
}

#[test]
fn real_double_string_literal_trailing_dot_matches_numeric_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    // 数値リテラルとして受理される末尾ドット形は、文字列リテラルでも同じ値へ束縛する。
    for col in ["r", "d"] {
        for lit in ["1.", "2.", "10."] {
            for op in ["=", "<", ">="] {
                assert_eq!(
                    ids(&core, &alice, &format!("{col} {op} {lit}")),
                    ids(&core, &alice, &format!("{col} {op} '{lit}'")),
                    "{col} {op} {lit}"
                );
            }
        }
        assert_eq!(ids(&core, &alice, &format!("{col} > ' 2. '")), vec![2, 4]);
        assert_eq!(ids(&core, &alice, &format!("{col} > '+2.'")), vec![2, 4]);
        assert_eq!(
            ids(&core, &alice, &format!("{col} = '.5e1'")),
            Vec::<u64>::new()
        );
        assert_eq!(code(&core, &alice, &format!("{col} = '.'")), "22P02");
        assert_eq!(code(&core, &alice, &format!("{col} = 'inf'")), "22P02");
        // 符号の重ね掛けは不正形（整数列と同じ 22P02）。
        for bad in ["+-1.5", "-+1.5", "--1.5", "++1.5"] {
            assert_eq!(
                code(&core, &alice, &format!("{col} = '{bad}'")),
                "22P02",
                "{col} {bad}"
            );
        }
    }
}

/// 公開上限（`MAX_IN_LIST_ITEMS` = 256）ちょうどの数値列 `IN`／`NOT IN`（文字列リテラル）は
/// 式ノード予算で拒否されず、数値リテラル形の補集合と一致する（codex P1・PR #1420）。
#[test]
fn string_literal_not_in_at_published_limit_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let list = (1000..1256)
        .map(|i| format!("'{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let all = ids(&core, &alice, "qty IS NOT NULL OR qty IS NULL");
    assert_eq!(
        ids(&core, &alice, &format!("qty IN ({list})")),
        Vec::<u64>::new()
    );
    let not_in = ids(&core, &alice, &format!("qty NOT IN ({list})"));
    let not_null = ids(&core, &alice, "qty IS NOT NULL");
    assert_eq!(not_in, not_null);
    assert!(all.len() >= not_in.len());
}

#[test]
fn ranked_search_with_string_literal_matches_numeric_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    // 距離付き検索（`ORDER BY embedding <=> ...`）の SCALAR 段でも同じ結果になる
    // （数値索引の有無による差は `scalar_index_numeric.rs` が数値リテラル形で固定する）。
    let ranked = |core: &EngineCore, w: &str| {
        let r = core
            .execute_sql(
                &alice,
                &format!(
                    "SELECT id FROM {TABLE} WHERE {w} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 100"
                ),
            )
            .unwrap_or_else(|e| panic!("{w:?} should succeed, got {e:?}"));
        let mut v: Vec<u64> = r
            .rows
            .iter()
            .map(|r| match r.cells.first() {
                Some(Cell::Integer(v)) => *v,
                other => panic!("unexpected id cell {other:?}"),
            })
            .collect();
        v.sort_unstable();
        v
    };
    let before = ranked(&core, "qty > '1'");
    assert_eq!(before, ranked(&core, "qty > 1"));
    assert_eq!(ranked(&core, "qty = '10'"), vec![4]);
}

#[test]
fn boolean_column_string_literal_equality() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for t in ["t", "TRUE", " yes ", "on", "1"] {
        assert_eq!(ids(&core, &alice, &format!("flag = '{t}'")), vec![1], "{t}");
    }
    for f in ["f", "off", "0", "no"] {
        assert_eq!(ids(&core, &alice, &format!("flag = '{f}'")), vec![2], "{f}");
    }
    for bad in ["x", "o", ""] {
        assert_eq!(
            code(&core, &alice, &format!("flag = '{bad}'")),
            "22P02",
            "{bad}"
        );
    }
    // NULL 行（id 3）は否定越しでも一致しない。
    assert_eq!(ids(&core, &alice, "NOT flag = 't'"), vec![2]);
}

#[test]
fn operatorless_type_combinations_are_42883() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for col in ["lang", "day", "embedding", "flag", "ext_id", "blob"] {
        for clause in [
            format!("{col} = 1"),
            format!("{col} > 5"),
            format!("1 < {col}"),
        ] {
            assert_eq!(code(&core, &alice, &clause), "42883", "{clause}");
        }
    }
    // NUMERIC 列 × 数値リテラルは本 Issue の対象外（従来の拒否を維持）。
    assert_eq!(code(&core, &alice, "price = 1"), "22000");
}

#[test]
fn predicate_dml_with_string_literal_matches_numeric_form_and_respects_tenant() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    exec(
        &core,
        &alice,
        "UPDATE docs SET lang = 'en' WHERE qty = '1' USING OPERATION_ID 'upd-1'",
    );
    assert_eq!(ids(&core, &alice, "lang = 'en'"), vec![1]);
    exec(
        &core,
        &alice,
        "DELETE FROM docs WHERE qty > '1' USING OPERATION_ID 'del-1'",
    );
    assert_eq!(ids(&core, &alice, "qty > 0"), vec![1]);
    // 他テナントの行は alice の DML・結果のいずれにも影響しない。
    assert_eq!(ids(&core, &bob, "qty = '1'"), vec![9]);
}
