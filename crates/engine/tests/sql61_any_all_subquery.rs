//! `<col> <op> ANY|SOME|ALL (SELECT ...)`（量化比較のサブクエリ形）の結合テスト
//! （Issue #1855・SQL-61 (3)(4)(5)・RLS-10）。`tests/sql29_subquery_scalar.rs` と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。
//!
//! 期待値は、テスト側に Kleene の三値論理を直接実装した独立オラクルで計算する
//! （内側の集合 `S` が空・NULL のみ・NULL を含む・外側値 NULL を網羅する）。
//!
//! 対象外（このファイルでは拒否の確認のみ）: 式の左辺・相関サブクエリ・内側の集計形・
//! 投影位置／HAVING での量化サブクエリ。

use engine::catalog::{ColumnDef, ColumnType, EnumTypeDef, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};
use std::sync::Arc;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ITEMS: &str = "items";
const REFS: &str = "refs_t";

fn columns(mood: Arc<EnumTypeDef>) -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("embedding", ColumnType::Vector(2), false),
        ColumnDef::new("name", ColumnType::Text, true),
        ColumnDef::new("qty", ColumnType::BigInt, true),
        ColumnDef::new("ratio", ColumnType::Double, true),
        ColumnDef::new(
            "price",
            ColumnType::Numeric {
                precision: 10,
                scale: 2,
            },
            true,
        ),
        ColumnDef::new("day", ColumnType::Date, true),
        ColumnDef::new("ok", ColumnType::Boolean, true),
        ColumnDef::new("mood", ColumnType::Enum(mood), true),
    ]
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql61-any-all-subquery");
    let storage = Storage::open(&path).expect("open storage");
    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("enum");
    let mut items_cols = columns(mood.clone());
    // 外側にだけ存在する列（相関参照の検出テスト用）。
    items_cols.push(ColumnDef::new("outer_only", ColumnType::Text, true));
    storage
        .create_table(&TableSchema::new(ITEMS, items_cols))
        .expect("items");
    storage
        .create_table(&TableSchema::new(REFS, columns(mood)))
        .expect("refs");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ins(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, cols: &[(&str, &str)]) {
    let mut names = vec!["id".to_string(), "embedding".to_string()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".to_string()];
    for (c, v) in cols {
        names.push((*c).to_string());
        values.push((*v).to_string());
    }
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({}) USING OPERATION_ID '{table}-{id}'",
        names.join(", "),
        values.join(", ")
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .unwrap_or_else(|e| panic!("insert failed sql={sql:?}: {e:?}"));
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"));
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn err_code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    core.execute_sql(ctx, sql)
        .expect_err(&format!("sql={sql:?} should be rejected"))
        .wire_code()
        .to_string()
}

/// Kleene の三値論理の真理値。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tri {
    True,
    False,
    Unknown,
}

/// 順序つき値の添字（`0 < 1 < 2`）どうしの比較（どちらかが NULL なら UNKNOWN）。
fn cmp_tri(op: &str, x: Option<usize>, e: Option<usize>) -> Tri {
    let (Some(x), Some(e)) = (x, e) else {
        return Tri::Unknown;
    };
    let t = match op {
        "=" => x == e,
        "<>" => x != e,
        "<" => x < e,
        "<=" => x <= e,
        ">" => x > e,
        ">=" => x >= e,
        other => panic!("unknown op {other}"),
    };
    if t {
        Tri::True
    } else {
        Tri::False
    }
}

/// `x <op> ANY|ALL (S)` の素朴な定義（OR／AND の畳み込み。空集合は ANY=偽・ALL=真）。
fn quantified_oracle(op: &str, all: bool, x: Option<usize>, set: &[Option<usize>]) -> Tri {
    let mut acc = if all { Tri::True } else { Tri::False };
    for e in set {
        let t = cmp_tri(op, x, *e);
        acc = if all {
            match (acc, t) {
                (Tri::False, _) | (_, Tri::False) => Tri::False,
                (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
                _ => Tri::True,
            }
        } else {
            match (acc, t) {
                (Tri::True, _) | (_, Tri::True) => Tri::True,
                (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
                _ => Tri::False,
            }
        };
    }
    acc
}

struct Fixture {
    col: &'static str,
    /// 昇順の 3 値（SQL リテラル）。
    v: [&'static str; 3],
}

const FIXTURES: [Fixture; 5] = [
    Fixture {
        col: "name",
        v: ["'a'", "'b'", "'c'"],
    },
    Fixture {
        col: "qty",
        v: ["-5", "10", "300"],
    },
    Fixture {
        col: "ratio",
        v: ["-1.5", "2.5", "1000.25"],
    },
    Fixture {
        col: "price",
        v: ["'1.50'", "'12.50'", "'99.99'"],
    },
    Fixture {
        col: "day",
        v: ["'2024-01-01'", "'2024-06-15'", "'2025-01-01'"],
    },
];

/// 内側の集合のパターン（`None` は NULL）。空・NULL のみ・NULL 含む・単一値・複数値・重複。
fn sets() -> Vec<Vec<Option<usize>>> {
    vec![
        vec![],
        vec![None],
        vec![Some(1)],
        vec![Some(0), Some(2)],
        vec![Some(1), None],
        vec![Some(0), Some(1), Some(2)],
        vec![Some(0), Some(0)],
        vec![None, None],
    ]
}

const OPS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

/// SQL-61 (3): 6 演算子 × {ANY, SOME, ALL} × 内側の集合パターン × 値族を、独立オラクルと
/// 突き合わせる。`NOT (...)` 形も同じオラクルの否定（偽の行だけ）と一致する。
#[test]
fn quantified_compare_matches_independent_oracle_for_every_type() {
    for fx in &FIXTURES {
        for set in sets() {
            let (core, path) = new_core();
            let _guard = CleanupGuard(path);
            let ctx = ctx_for("tenant-a");
            for (i, v) in fx.v.iter().enumerate() {
                ins(&core, &ctx, ITEMS, (i + 1) as u64, &[(fx.col, v)]);
            }
            // id 4 は対象列が NULL の外側行。
            ins(&core, &ctx, ITEMS, 4, &[]);
            for (n, e) in set.iter().enumerate() {
                let id = 100 + n as u64;
                match e {
                    Some(i) => ins(&core, &ctx, REFS, id, &[(fx.col, fx.v[*i])]),
                    None => ins(&core, &ctx, REFS, id, &[]),
                }
            }
            let outer = [Some(0), Some(1), Some(2), None];
            for op in OPS {
                for (quant, all) in [("ANY", false), ("SOME", false), ("ALL", true)] {
                    let inner = format!("SELECT {} FROM {REFS} LIMIT 100", fx.col);
                    let pred = format!("{} {op} {quant} ({inner})", fx.col);
                    for (negate, want) in [(false, Tri::True), (true, Tri::False)] {
                        let where_clause = if negate {
                            format!("NOT ({pred})")
                        } else {
                            pred.clone()
                        };
                        let sql = format!("SELECT id FROM {ITEMS} WHERE {where_clause} LIMIT 100");
                        let expected: Vec<u64> = outer
                            .iter()
                            .enumerate()
                            .filter(|(_, x)| quantified_oracle(op, all, **x, &set) == want)
                            .map(|(i, _)| (i + 1) as u64)
                            .collect();
                        assert_eq!(
                            ids(&core, &ctx, &sql),
                            expected,
                            "col={} set={set:?} sql={sql}",
                            fx.col
                        );
                    }
                }
            }
        }
    }
}

/// `= ANY` は `IN`、`<> ALL` は `NOT IN`、`NOT (x > ANY S)` は `x <= ALL S` と同じ結果になる。
#[test]
fn quantified_forms_are_equivalent_to_in_and_negations() {
    for set in sets() {
        let (core, path) = new_core();
        let _guard = CleanupGuard(path);
        let ctx = ctx_for("tenant-a");
        for (i, v) in ["-5", "10", "300"].iter().enumerate() {
            ins(&core, &ctx, ITEMS, (i + 1) as u64, &[("qty", v)]);
        }
        ins(&core, &ctx, ITEMS, 4, &[]);
        for (n, e) in set.iter().enumerate() {
            let id = 100 + n as u64;
            match e {
                Some(i) => ins(&core, &ctx, REFS, id, &[("qty", ["-5", "10", "300"][*i])]),
                None => ins(&core, &ctx, REFS, id, &[]),
            }
        }
        let inner = format!("SELECT qty FROM {REFS} LIMIT 100");
        let q = |w: String| {
            ids(
                &core,
                &ctx,
                &format!("SELECT id FROM {ITEMS} WHERE {w} LIMIT 100"),
            )
        };
        assert_eq!(
            q(format!("qty = ANY ({inner})")),
            q(format!("qty IN ({inner})"))
        );
        assert_eq!(
            q(format!("qty <> ALL ({inner})")),
            q(format!("qty NOT IN ({inner})"))
        );
        assert_eq!(
            q(format!("NOT (qty > ANY ({inner}))")),
            q(format!("qty <= ALL ({inner})"))
        );
    }
}

/// 疑似列 `id`・numeric の厳密比較（scale 違いの等価を誤らない）。
#[test]
fn quantified_compare_pseudo_id_and_numeric() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    for id in 1..=4u64 {
        ins(&core, &ctx, ITEMS, id, &[]);
    }
    ins(&core, &ctx, REFS, 2, &[]);
    ins(&core, &ctx, REFS, 3, &[]);
    let q = |w: &str| {
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {ITEMS} WHERE {w} LIMIT 100"),
        )
    };
    let inner = format!("SELECT id FROM {REFS} LIMIT 10");
    assert_eq!(q(&format!("id > ALL ({inner})")), vec![4]);
    assert_eq!(q(&format!("id >= ANY ({inner})")), vec![2, 3, 4]);
    assert_eq!(q(&format!("id < ALL ({inner})")), vec![1]);
    assert_eq!(q(&format!("id <> ALL ({inner})")), vec![1, 4]);
    assert_eq!(q(&format!("id = ANY ({inner})")), vec![2, 3]);

    // numeric 列（scale 2）× numeric の内側。
    let (core, path) = new_core();
    let _guard3 = CleanupGuard(path);
    ins(&core, &ctx, ITEMS, 1, &[("price", "'1.50'")]);
    ins(&core, &ctx, ITEMS, 2, &[("price", "'12.50'")]);
    ins(&core, &ctx, REFS, 1, &[("price", "'1.5'")]);
    ins(&core, &ctx, REFS, 2, &[("price", "'12.50'")]);
    let q = |w: &str| {
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {ITEMS} WHERE {w} LIMIT 100"),
        )
    };
    let inner = format!("SELECT price FROM {REFS} LIMIT 10");
    assert_eq!(q(&format!("price >= ALL ({inner})")), vec![2]);
    assert_eq!(q(&format!("price > ANY ({inner})")), vec![2]);
    assert_eq!(q(&format!("price = ALL ({inner})")), Vec::<u64>::new());
}

/// `= ALL` の境界値が異なっても、外側列の比較型（DOUBLE × BIGINT の f64 変換）で両方と
/// 等しくなる行は、個別比較の連言と同じく残る（Issue #1855 レビュー指摘）。
#[test]
fn quantified_eq_all_follows_outer_comparison_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("ratio", "9007199254740992.0")]);
    ins(&core, &ctx, ITEMS, 2, &[("ratio", "1.0")]);
    ins(&core, &ctx, REFS, 1, &[("qty", "9007199254740992")]);
    ins(&core, &ctx, REFS, 2, &[("qty", "9007199254740993")]);
    let w = format!("ratio = ALL (SELECT qty FROM {REFS} LIMIT 10)");
    let conj = "ratio = 9007199254740992 AND ratio = 9007199254740993";
    let q = |w: &str| {
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {ITEMS} WHERE {w} LIMIT 100"),
        )
    };
    assert_eq!(q(&conj), vec![1]);
    assert_eq!(q(&w), vec![1]);
}

/// SQL-61 (4)(5): 未対応の形・静的エラー・上限は fail-closed の既存コードで拒否する。
#[test]
fn quantified_compare_rejections_are_fail_closed() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1"), ("name", "'a'")]);
    ins(&core, &ctx, REFS, 1, &[("qty", "1"), ("name", "'a'")]);
    let cases: Vec<(String, &str)> = vec![
        // 内側の投影列数 ≠ 1。
        (format!("SELECT id FROM {ITEMS} WHERE qty > ANY (SELECT qty, name FROM {REFS} LIMIT 5) LIMIT 100"), "42601"),
        // 内側の `LIMIT` 省略。
        (format!("SELECT id FROM {ITEMS} WHERE qty > ANY (SELECT qty FROM {REFS}) LIMIT 100"), "42601"),
        // 相関サブクエリ。
        (format!("SELECT id FROM {ITEMS} WHERE qty > ALL (SELECT qty FROM {REFS} WHERE name = outer_only LIMIT 5) LIMIT 100"), "42601"),
        // 内側の集計形。
        (format!("SELECT id FROM {ITEMS} WHERE qty > ANY (SELECT max(qty) FROM {REFS}) LIMIT 100"), "42601"),
        // 値族の不一致・BOOLEAN／ENUM の範囲比較（内側が空でも拒否）。
        (format!("SELECT id FROM {ITEMS} WHERE name > ANY (SELECT qty FROM {REFS} LIMIT 5) LIMIT 100"), "22000"),
        (format!("SELECT id FROM {ITEMS} WHERE ok > ANY (SELECT ok FROM {REFS} LIMIT 5) LIMIT 100"), "22000"),
        (format!("SELECT id FROM {ITEMS} WHERE mood < ALL (SELECT mood FROM {REFS} LIMIT 5) LIMIT 100"), "22000"),
        (format!("SELECT id FROM {ITEMS} WHERE ok > ALL (SELECT ok FROM {REFS} WHERE qty > 99 LIMIT 5) LIMIT 100"), "22000"),
        // 未知の対象列。
        (format!("SELECT id FROM {ITEMS} WHERE nope > ANY (SELECT qty FROM {REFS} LIMIT 5) LIMIT 100"), "42703"),
        // 式の左辺。
        (format!("SELECT id FROM {ITEMS} WHERE qty + 1 > ANY (SELECT qty FROM {REFS} LIMIT 5) LIMIT 100"), "0A000"),
    ];
    for (sql, code) in cases {
        assert_eq!(err_code(&core, &ctx, &sql), code, "sql={sql}");
    }
    // 同じ形でも `= ANY`／`<> ALL`（IN へ委譲）は BOOLEAN で受理される。
    assert!(core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT id FROM {ITEMS} WHERE ok = ANY (SELECT ok FROM {REFS} LIMIT 5) LIMIT 100"
            )
        )
        .is_ok());
    // 列名 `any`／`all` を壊さない（量化比較の構文ではない）。
    assert_eq!(
        err_code(
            &core,
            &ctx,
            &format!("SELECT id FROM {ITEMS} WHERE any > 1 LIMIT 100")
        ),
        "42703"
    );
}

/// SQL-61 (4): 入れ子の深さ・実行回数予算・内側の行数上限は `IN` と同じ `54000`。
#[test]
fn quantified_compare_depth_and_execution_budget() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1")]);
    ins(&core, &ctx, REFS, 1, &[("qty", "1")]);
    let nested = format!(
        "SELECT id FROM {ITEMS} WHERE qty >= ALL (SELECT qty FROM {REFS} WHERE qty <= ANY \
         (SELECT qty FROM {REFS} LIMIT 10) LIMIT 10) LIMIT 100"
    );
    assert_eq!(ids(&core, &ctx, &nested), vec![1]);
    let mut deep = format!("SELECT qty FROM {REFS} LIMIT 10");
    for _ in 0..4 {
        deep = format!("SELECT qty FROM {REFS} WHERE qty >= ANY ({deep}) LIMIT 10");
    }
    let sql = format!("SELECT id FROM {ITEMS} WHERE qty >= ANY ({deep}) LIMIT 100");
    assert_eq!(err_code(&core, &ctx, &sql), "54000");
    let many = (0..17)
        .map(|_| format!("qty >= ANY (SELECT qty FROM {REFS} LIMIT 10)"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("SELECT id FROM {ITEMS} WHERE {many} LIMIT 100");
    assert_eq!(err_code(&core, &ctx, &sql), "54000");
}

/// RLS-10: 内側は呼び出し元の可視行だけで評価する。他テナントの行は境界値・空判定・
/// NULL 判定のいずれにも影響しない。
#[test]
fn quantified_compare_ignores_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    ins(&core, &a, ITEMS, 1, &[("qty", "5")]);
    ins(&core, &a, ITEMS, 2, &[("qty", "50")]);
    ins(&core, &a, REFS, 1, &[("qty", "10")]);
    // 他テナントの行（大きな値・NULL）は自テナントの判定を変えない。
    ins(&core, &b, REFS, 100, &[("qty", "1000")]);
    ins(&core, &b, REFS, 101, &[]);
    let q = |w: &str| {
        ids(
            &core,
            &a,
            &format!("SELECT id FROM {ITEMS} WHERE {w} LIMIT 100"),
        )
    };
    let inner = format!("SELECT qty FROM {REFS} LIMIT 100");
    assert_eq!(q(&format!("qty > ALL ({inner})")), vec![2]);
    assert_eq!(q(&format!("qty < ANY ({inner})")), vec![1]);
    assert_eq!(q(&format!("qty = ALL ({inner})")), Vec::<u64>::new());
    // 自テナントに可視行が 1 つも無いテナントでは `ALL` は常に真、`ANY` は常に偽。
    let c = ctx_for("tenant-c");
    ins(&core, &c, ITEMS, 200, &[("qty", "1")]);
    let qc = |w: &str| {
        ids(
            &core,
            &c,
            &format!("SELECT id FROM {ITEMS} WHERE {w} LIMIT 100"),
        )
    };
    assert_eq!(qc(&format!("qty > ALL ({inner})")), vec![200]);
    assert_eq!(qc(&format!("qty > ANY ({inner})")), Vec::<u64>::new());
    // エラー文言に他テナントの値・存在情報を含めない。
    let e = core
        .execute_sql(
            &a,
            &format!("SELECT id FROM {ITEMS} WHERE qty > ANY (SELECT name FROM {REFS} LIMIT 5) LIMIT 100"),
        )
        .expect_err("family mismatch");
    let msg = e.to_string();
    assert!(!msg.contains("1000") && !msg.contains("tenant-b"), "{msg}");
}

/// 拡張クエリプロトコル: `$n` との併用は Parse 段で `42601`、併用なしは受理（P0: 内側の `$n` が
/// 外側の述語フラグの添字をずらさない）。
#[test]
fn quantified_compare_with_params_is_rejected_at_parse() {
    let (core, _path_guard) = {
        let (c, p) = new_core();
        (c, CleanupGuard(p))
    };
    let err = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {ITEMS} WHERE qty > ANY (SELECT qty FROM {REFS} LIMIT 5) AND name = $1 LIMIT 10"
        ))
        .expect_err("ANY subquery with $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
    let prepared = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {ITEMS} WHERE qty > ALL (SELECT qty FROM {REFS} LIMIT 5) LIMIT 10"
        ))
        .expect("unparameterized ANY/ALL subquery must be accepted");
    assert_eq!(prepared.param_count(), 0);
}

/// 述語形 UPDATE／DELETE の WHERE 内の量化比較は内側の値で対象行が決まる。`USING OPERATION_ID`
/// 付きの同一文の再送は `23505`、量化子だけを変えた別内容は `22023`。
#[test]
fn quantified_compare_executes_in_predicate_dml() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    for (id, q) in [(1u64, "1"), (2, "5"), (3, "9")] {
        ins(&core, &ctx, ITEMS, id, &[("qty", q)]);
    }
    ins(&core, &ctx, REFS, 1, &[("qty", "3")]);
    ins(&core, &ctx, REFS, 2, &[("qty", "6")]);
    let run = |sql: &str| core.execute_sql_in_session(&ctx, &mut SessionState::default(), sql);
    let all = format!("SELECT id FROM {ITEMS} WHERE qty >= -100 LIMIT 100");
    let inner = format!("SELECT qty FROM {REFS} LIMIT 10");

    run(&format!(
        "UPDATE {ITEMS} SET name = 'x' WHERE qty > ALL ({inner})"
    ))
    .expect("update executes");
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {ITEMS} WHERE name = 'x' LIMIT 100")
        ),
        vec![3]
    );
    let op_del = format!("DELETE FROM {ITEMS} WHERE qty < ANY ({inner}) USING OPERATION_ID 'q1'");
    run(&op_del).expect("delete executes");
    assert_eq!(ids(&core, &ctx, &all), vec![3]);
    let err = run(&op_del).expect_err("resend");
    assert_eq!(err.wire_code(), "23505", "{err:?}");
    let changed = format!("DELETE FROM {ITEMS} WHERE qty < ALL ({inner}) USING OPERATION_ID 'q1'");
    let err = run(&changed).expect_err("different content under the same operation id");
    assert_eq!(err.wire_code(), "22023", "{err:?}");
}
