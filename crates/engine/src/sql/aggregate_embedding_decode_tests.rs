//! 集計の 3 段階デコード（`Fast`／`DimAndScalar`／`Embedding`。TABLE-13・INDEX-5・
//! TASK-199・`docs/design/aggregate-decode-skip.md`）が、新スカラー型 13 種を参照する
//! COUNT／SUM／AVG／MIN／MAX（GROUP BY あり・なし）で埋め込み本体（`Vec<f32>` 化）を
//! 1 回も読まないことを、デコード計数で直接固定する（Issue #1258）。
//!
//! 既存の `tests/decode_tier_scalar_types.rs`（Issue #894）は結果の正しさと
//! `select_decode_tier` の選択結果しか見ておらず、実行時に埋め込みデコードが起きて
//! いないことは観測できない。本モジュールは `storage::decode_probe`（`#[cfg(test)]`・
//! thread_local の計数。走査ループは呼び出しスレッド上で同期実行される）を使って観測する。
//!
//! - 層 A: `sql::aggregate::execute_aggregate` をキャッシュ非経由で直接呼ぶ。3 段階
//!   デコード本体の契約を固定する（単一行・単一キー GROUP BY）
//! - 層 B: production 入口 `EngineCore::execute_sql` の索引 capture を通らない経路
//!   （WHERE なしの単一行集計・複数キー GROUP BY）と、型不整合の拒否（fail-closed）
//!
//! 単一キー・WHERE なしの GROUP BY を `execute_sql` 経由で走らせると、cold cache では
//! スカラー索引／arena スナップショット構築（`capture_scalar_index_snapshot`）が可視行全件の
//! 埋め込みをデコードする。これは 3 段階デコードの契約の外にある設計どおりの挙動の
//! ため、層 B には含めない。陽性対照（`vec_norm(embedding)` 集計）でプローブ自体の
//! 機能と、RLS（RLS-7・RLS-8）で不可視な行が本体デコードへ到達しないことも固定する。

use crate::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{validate_sql, Statement};
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::udf_call::UdfRegistry;
use crate::storage::decode_probe::{self, DecodeCounts};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;
use std::collections::BTreeMap;

const TABLE: &str = "docs";

/// 13 型の列名（`(列名, 受理される集計関数)`）。受理関数は
/// `sql/parser.rs::resolve_aggregate_input` の型別受理に対応する。
const ALL: [&str; 5] = ["COUNT", "SUM", "AVG", "MIN", "MAX"];
const ORDERED: [&str; 3] = ["COUNT", "MIN", "MAX"];
const COUNT_ONLY: [&str; 1] = ["COUNT"];

fn accepted_matrix() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("i", &ALL),
        ("bi", &ALL),
        ("r", &ALL),
        ("d", &ALL),
        ("n", &ALL),
        ("dt", &ORDERED),
        ("ts", &ORDERED),
        ("bo", &COUNT_ONLY),
        ("u", &COUNT_ONLY),
        ("blob", &COUNT_ONLY),
        ("j", &COUNT_ONLY),
        ("m", &COUNT_ONLY),
        ("arr", &COUNT_ONLY),
    ]
}

/// 拒否される `(列名, 関数)`（束縛段で拒否され、走査前に失敗する）。
fn rejected_matrix() -> Vec<(&'static str, &'static str)> {
    let mut v = vec![];
    for col in ["dt", "ts"] {
        v.push((col, "SUM"));
        v.push((col, "AVG"));
    }
    for col in ["bo", "u", "blob", "j", "m", "arr"] {
        for f in ["SUM", "AVG", "MIN", "MAX"] {
            v.push((col, f));
        }
    }
    v
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `docs` テーブルを作り alice の可視 3 行と bob の不可視 1 行を投入して返す。
fn build_fixture(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let mood = storage
        .create_enum_type(
            "mood",
            vec![
                "happy".to_string(),
                "sad".to_string(),
                "neutral".to_string(),
            ],
        )
        .expect("create enum type");
    let n = |name: &str, ty: ColumnType| ColumnDef::new(name, ty, true);
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), true),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("kind", ColumnType::Text, false),
            n("i", ColumnType::Integer),
            n("bi", ColumnType::BigInt),
            n("r", ColumnType::Real),
            n("d", ColumnType::Double),
            n(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
            n("bo", ColumnType::Boolean),
            n("dt", ColumnType::Date),
            n("ts", ColumnType::Timestamp),
            n("u", ColumnType::Uuid),
            n("blob", ColumnType::Bytea),
            n("j", ColumnType::Json),
            n("m", ColumnType::Enum(mood)),
            n(
                "arr",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
            ),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let exec = |ctx: &PolicyContext, sql: String| {
        core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
            .expect("insert should succeed");
    };
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    let full = |id: u64, lang: &str, kind: &str, k: u32, op: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, lang, kind, i, bi, r, d, n, bo, dt, ts, u, \
             blob, j, m, arr) VALUES ({id}, '[0.3,0.4]', '{lang}', '{kind}', {i}, {bi}, {r}.5, \
             {d}.5, {k}.10, {bo}, '2024-0{k}-01', '2024-0{k}-01 00:00:00', \
             '00000000-0000-0000-0000-00000000000{k}', '\\xDEADBEEF', '{{\"a\":{k}}}', \
             'happy', '{{a,b}}') USING OPERATION_ID '{op}'",
            i = k * 10,
            bi = k * 100,
            r = k,
            d = k + 1,
            bo = if k % 2 == 1 { "true" } else { "false" },
        )
    };
    exec(&alice, full(1, "ja", "a", 1, "op-1"));
    exec(&alice, full(2, "ja", "b", 2, "op-2"));
    // id=3: 新型列すべて NULL（COUNT(col) < COUNT(*) を成立させる）。
    exec(
        &alice,
        format!(
            "INSERT INTO {TABLE} (id, embedding, lang, kind) VALUES (3, '[0.3,0.4]', 'en', 'a') \
             USING OPERATION_ID 'op-3'"
        ),
    );
    // bob の行（alice からは不可視）。値は alice の値域と衝突しない大きさにする。
    exec(&bob, full(100, "en", "a", 9, "op-bob"));
    (core, path)
}

fn sql_for(col: &str, func: &str) -> String {
    format!("{func}({col})")
}

fn dbg(cell: &Cell) -> String {
    format!("{cell:?}")
}

/// 単一行集計の期待値（alice の可視 2 値行。k=1・2）。`Debug` 表記で固定する。
fn expected_plain(col: &str, func: &str) -> String {
    if func == "COUNT" {
        return "Integer(2)".to_string();
    }
    let (sum, avg, min, max): (&str, &str, &str, &str) = match col {
        "i" => (
            "SignedInteger(30)",
            "Float(15.0)",
            "SignedInteger(10)",
            "SignedInteger(20)",
        ),
        "bi" => (
            "SignedInteger(300)",
            "Float(150.0)",
            "SignedInteger(100)",
            "SignedInteger(200)",
        ),
        "r" => ("Float(4.0)", "Float(2.0)", "Float(1.5)", "Float(2.5)"),
        "d" => ("Float(6.0)", "Float(3.0)", "Float(2.5)", "Float(3.5)"),
        _ => ("?", "?", "?", "?"),
    };
    match func {
        "SUM" => sum,
        "AVG" => avg,
        "MIN" => min,
        _ => max,
    }
    .to_string()
}

/// 層 A・層 B 共通の結果検査。`Fast`/`DimAndScalar` の結果が alice の可視行のみから
/// 得られていること（bob の k=9 が混入していないこと）を確かめる。
fn assert_plain_cell(col: &str, func: &str, cell: &Cell) {
    let got = dbg(cell);
    let want = expected_plain(col, func);
    if want != "?" {
        assert_eq!(got, want, "{func}({col}) plain oracle");
    } else if func != "COUNT" {
        // NUMERIC/DATE/TIMESTAMP の値表現は個別に固定せず、NULL でないことと
        // bob 由来の値（k=9）を含まないことだけを確かめる。
        assert_ne!(got, "Null", "{func}({col}) must not be NULL");
    }
    assert!(
        !got.contains("2024-09") && !got.contains("9.1"),
        "{func}({col}) leaked invisible row: {got}"
    );
}

fn bind(sql: &str, storage: &Storage, schema: &TableSchema) -> crate::sql::parser::BoundAggregate {
    let stmt = validate_sql(sql, storage).expect("must pass allowlist");
    let agg = match stmt {
        Statement::Aggregate(agg) => agg,
        other => panic!("expected Statement::Aggregate, got {other:?}"),
    };
    crate::sql::parser::bind_aggregate(&agg, schema, &UdfRegistry::default())
        .expect("bind aggregate")
}

/// 集計 1 本を `execute_aggregate` 直呼び出しで実行し、結果と計数の増分を返す。
fn run_layer_a(
    storage: &Storage,
    schema: &TableSchema,
    ctx: &PolicyContext,
    sql: &str,
) -> (crate::sql::exec::QueryResult, DecodeCounts) {
    let bound = bind(sql, storage, schema);
    let read_txn = storage.db().begin_read().expect("begin_read");
    let before = decode_probe::snapshot();
    let result = crate::sql::aggregate::execute_aggregate(&read_txn, ctx, schema, &bound)
        .expect("execute_aggregate");
    (result, decode_probe::snapshot().since(before))
}

fn reopen(core: EngineCore, path: &std::path::Path) -> (Storage, TableSchema) {
    drop(core);
    let storage = Storage::open(path).expect("reopen storage");
    let schema = storage.get_table_schema(TABLE).expect("schema");
    (storage, schema)
}

fn group_map(result: &crate::sql::exec::QueryResult, key_cols: usize) -> BTreeMap<String, Cell> {
    let mut m = BTreeMap::new();
    for row in &result.rows {
        let key: Vec<String> = row.cells[..key_cols]
            .iter()
            .map(|c| match c {
                Cell::Text(s) => s.clone(),
                other => panic!("group key must be text: {other:?}"),
            })
            .collect();
        m.insert(key.join("|"), row.cells[key_cols].clone());
    }
    m
}

fn assert_group_cells(col: &str, func: &str, groups: &BTreeMap<String, Cell>, keys: &[&str]) {
    assert_eq!(
        groups.keys().map(String::as_str).collect::<Vec<_>>(),
        keys,
        "{func}({col}) group keys (bob's group must not leak)"
    );
    // NULL 行のみのグループ（en）: COUNT は 0、それ以外は NULL。
    for (key, cell) in groups {
        let all_null_group = key.starts_with("en");
        if func == "COUNT" {
            let want = if all_null_group {
                "Integer(0)"
            } else if key == "ja" {
                "Integer(2)"
            } else {
                "Integer(1)"
            };
            assert_eq!(dbg(cell), want, "COUNT({col}) group {key}");
        } else if all_null_group {
            assert_eq!(dbg(cell), "Null", "{func}({col}) group {key}");
        } else {
            assert_ne!(dbg(cell), "Null", "{func}({col}) group {key}");
        }
    }
}

// --- 層 A: 3 段階デコード本体の直接観測 -------------------------------------

#[test]
fn aggregate_decode_never_touches_embedding_for_new_scalar_types_single_row() {
    let (core, path) = build_fixture("decode-probe-a-plain");
    let _guard = CleanupGuard(path.clone());
    let (storage, schema) = reopen(core, &path);
    let alice = ctx_for("alice");

    // Fast tier: COUNT(*)。
    let (result, d) = run_layer_a(&storage, &schema, &alice, "SELECT COUNT(*) FROM docs");
    assert_eq!(dbg(&result.rows[0].cells[0]), "Integer(3)");
    assert_eq!(d.embedding, 0, "COUNT(*) must not decode embedding");
    assert!(d.dim_and_metadata > 0, "COUNT(*) must scan rows");

    for (col, funcs) in accepted_matrix() {
        for func in funcs {
            let sql = format!("SELECT {} FROM docs", sql_for(col, func));
            let (result, d) = run_layer_a(&storage, &schema, &alice, &sql);
            assert_eq!(
                d.embedding, 0,
                "{sql}: embedding decoded {} times",
                d.embedding
            );
            assert!(d.dim_and_metadata > 0, "{sql}: no rows scanned");
            assert_plain_cell(col, func, &result.rows[0].cells[0]);
        }
    }
}

#[test]
fn grouped_aggregate_decode_never_touches_embedding_for_new_scalar_types() {
    let (core, path) = build_fixture("decode-probe-a-grouped");
    let _guard = CleanupGuard(path.clone());
    let (storage, schema) = reopen(core, &path);
    let alice = ctx_for("alice");

    let (result, d) = run_layer_a(
        &storage,
        &schema,
        &alice,
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
    );
    let groups = group_map(&result, 1);
    assert_eq!(dbg(&groups["ja"]), "Integer(2)");
    assert_eq!(dbg(&groups["en"]), "Integer(1)");
    assert_eq!(d.embedding, 0, "grouped COUNT(*) must not decode embedding");
    assert!(d.dim_and_metadata > 0);

    for (col, funcs) in accepted_matrix() {
        for func in funcs {
            let sql = format!(
                "SELECT lang, {} FROM docs GROUP BY lang",
                sql_for(col, func)
            );
            let (result, d) = run_layer_a(&storage, &schema, &alice, &sql);
            assert_eq!(
                d.embedding, 0,
                "{sql}: embedding decoded {} times",
                d.embedding
            );
            assert!(d.dim_and_metadata > 0, "{sql}: no rows scanned");
            assert_group_cells(col, func, &group_map(&result, 1), &["en", "ja"]);
        }
    }
}

#[test]
fn embedding_referencing_aggregate_decodes_exactly_visible_rows() {
    let (core, path) = build_fixture("decode-probe-a-positive");
    let _guard = CleanupGuard(path.clone());
    let (storage, schema) = reopen(core, &path);
    let alice = ctx_for("alice");

    // 陽性対照: embedding を参照する集計は Embedding tier になり、alice の可視 3 行だけが
    // 本体デコードされる（bob の行はヘッダでの可視判定で弾かれ本体に到達しない）。
    for sql in [
        "SELECT SUM(vec_norm(embedding)) FROM docs",
        "SELECT lang, SUM(vec_norm(embedding)) FROM docs GROUP BY lang",
    ] {
        let (_result, d) = run_layer_a(&storage, &schema, &alice, sql);
        assert_eq!(
            d.embedding, 3,
            "{sql}: probe must count exactly the visible rows"
        );
    }
}

// --- 層 B: production 入口の直接観測 -----------------------------------------

fn run_layer_b(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> (
    Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError>,
    DecodeCounts,
) {
    let before = decode_probe::snapshot();
    let result = core.execute_sql(ctx, sql);
    (result, decode_probe::snapshot().since(before))
}

#[test]
fn execute_sql_plain_scan_aggregates_never_decode_embedding() {
    let (core, path) = build_fixture("decode-probe-b-plain");
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let (result, d) = run_layer_b(&core, &alice, "SELECT COUNT(*) FROM docs");
    assert_eq!(
        dbg(&result.expect("COUNT(*)").rows[0].cells[0]),
        "Integer(3)"
    );
    assert_eq!(d.embedding, 0, "COUNT(*) must not decode embedding");

    for (col, funcs) in accepted_matrix() {
        for func in funcs {
            let sql = format!("SELECT {} FROM docs", sql_for(col, func));
            let (result, d) = run_layer_b(&core, &alice, &sql);
            let result = result.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            assert_eq!(
                d.embedding, 0,
                "{sql}: embedding decoded {} times",
                d.embedding
            );
            assert_plain_cell(col, func, &result.rows[0].cells[0]);
        }
    }

    // 陽性対照: プローブが production 経路（同一スレッド）でも機能すること。
    let (result, d) = run_layer_b(&core, &alice, "SELECT SUM(vec_norm(embedding)) FROM docs");
    result.expect("positive control");
    assert_eq!(d.embedding, 3, "positive control must count visible rows");
}

#[test]
fn execute_sql_multi_key_group_by_never_decodes_embedding() {
    let (core, path) = build_fixture("decode-probe-b-multi");
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (col, funcs) in accepted_matrix() {
        for func in funcs {
            let sql = format!(
                "SELECT lang, kind, {} FROM docs GROUP BY lang, kind",
                sql_for(col, func)
            );
            let (result, d) = run_layer_b(&core, &alice, &sql);
            let result = result.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            assert_eq!(
                d.embedding, 0,
                "{sql}: embedding decoded {} times",
                d.embedding
            );
            let groups = group_map(&result, 2);
            assert_group_cells_multi(col, func, &groups);
        }
    }

    let (result, d) = run_layer_b(
        &core,
        &alice,
        "SELECT lang, kind, SUM(vec_norm(embedding)) FROM docs GROUP BY lang, kind",
    );
    result.expect("positive control");
    assert_eq!(d.embedding, 3, "positive control must count visible rows");
}

fn assert_group_cells_multi(col: &str, func: &str, groups: &BTreeMap<String, Cell>) {
    assert_eq!(
        groups.keys().map(String::as_str).collect::<Vec<_>>(),
        ["en|a", "ja|a", "ja|b"],
        "{func}({col}) group keys"
    );
    for (key, cell) in groups {
        if key == "en|a" {
            let want = if func == "COUNT" {
                "Integer(0)"
            } else {
                "Null"
            };
            assert_eq!(dbg(cell), want, "{func}({col}) group {key}");
        } else if func == "COUNT" {
            assert_eq!(dbg(cell), "Integer(1)", "COUNT({col}) group {key}");
        } else {
            assert_ne!(dbg(cell), "Null", "{func}({col}) group {key}");
        }
    }
}

#[test]
fn execute_sql_rejected_aggregate_type_combinations_fail_closed_without_decode() {
    let (core, path) = build_fixture("decode-probe-b-rejected");
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (col, func) in rejected_matrix() {
        let sql = format!("SELECT {} FROM docs", sql_for(col, func));
        let (result, d) = run_layer_b(&core, &alice, &sql);
        let err = result.expect_err(&sql);
        // DATE/TIMESTAMP の SUM/AVG は undefined_function（42883）、それ以外は 22000。
        let want = if matches!(col, "dt" | "ts") {
            "42883"
        } else {
            "22000"
        };
        assert_eq!(err.wire_code(), want, "{sql}");
        assert_eq!(d.embedding, 0, "{sql}: rejection must precede any decode");
    }
}
