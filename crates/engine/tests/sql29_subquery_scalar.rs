//! スカラーサブクエリ `<col> <op> (SELECT ...)`・相関サブクエリの `42601`・
//! `IN`／`NOT IN (SELECT ...)` の対象型拡大の結合テスト（Issue #1191・SQL-29 (a)・
//! RLS-10 (b)・TASK-213）。`tests/sql29_subquery.rs` と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql` を
//! production 経路として検証）。期待値はテスト側で素朴に計算した独立オラクル。
//!
//! 対象外（このファイルでは拒否の確認のみ）: 式内の投影位置スカラーサブクエリ・逆向き比較・
//! 式への埋め込み・相関参照・拡張クエリプロトコルの `$n` 併用。

use engine::catalog::{ColumnDef, ColumnType, EnumTypeDef, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
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
        ColumnDef::new("at", ColumnType::Timestamp, true),
        ColumnDef::new("ext", ColumnType::Uuid, true),
        ColumnDef::new("blob", ColumnType::Bytea, true),
        ColumnDef::new("ok", ColumnType::Boolean, true),
        ColumnDef::new("mood", ColumnType::Enum(mood), true),
    ]
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql29-subquery-scalar");
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
    let mut refs_cols = columns(mood);
    // 語彙外ラベルを表現するための TEXT 列（ENUM 対象との値族比較用）。
    refs_cols.push(ColumnDef::new("label", ColumnType::Text, true));
    storage
        .create_table(&TableSchema::new(REFS, refs_cols))
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

/// `INSERT INTO <table> (id, embedding, <cols>) VALUES (...)`。値は SQL リテラル
/// 文字列（クォート込み）で渡す。
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

fn err_code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, SqlSurfaceError) {
    let e = core
        .execute_sql(ctx, sql)
        .expect_err(&format!("sql={sql:?} should be rejected"));
    (e.wire_code().to_string(), e)
}

/// 型ごとの固定データ: items の id 1..=3 が昇順の値、id 4 は全列 NULL。
/// refs の id 1 が 2 番目の値（`v2`）。
struct Fixture {
    col: &'static str,
    v: [&'static str; 3],
}

const FIXTURES: [Fixture; 6] = [
    Fixture {
        col: "name",
        v: ["'a'", "'b'", "'c'"],
    },
    Fixture {
        col: "qty",
        v: ["-5", "10", "300"],
    },
    Fixture {
        col: "price",
        v: ["'1.50'", "'12.50'", "'99.99'"],
    },
    Fixture {
        col: "day",
        v: ["'2024-01-01'", "'2024-06-15'", "'2025-01-01'"],
    },
    Fixture {
        col: "at",
        v: [
            "'2024-01-01 00:00:00'",
            "'2024-06-15 12:30:00'",
            "'2025-01-01 00:00:00'",
        ],
    },
    Fixture {
        col: "ext",
        v: [
            "'00000000-0000-0000-0000-000000000001'",
            "'00000000-0000-0000-0000-000000000002'",
            "'00000000-0000-0000-0000-000000000003'",
        ],
    },
];

fn seed_fixtures(core: &EngineCore, ctx: &PolicyContext) {
    for i in 0..3usize {
        let cols: Vec<(&str, &str)> = FIXTURES.iter().map(|f| (f.col, f.v[i])).collect();
        ins(core, ctx, ITEMS, (i + 1) as u64, &cols);
    }
    ins(core, ctx, ITEMS, 4, &[]);
    let refs: Vec<(&str, &str)> = FIXTURES.iter().map(|f| (f.col, f.v[1])).collect();
    ins(core, ctx, REFS, 1, &refs);
}

/// 各演算子の期待 id 集合（昇順 3 値の中央値と比較。NULL 行の id 4 は常に除外）。
fn expected(op: &str) -> Vec<u64> {
    match op {
        "=" => vec![2],
        "<>" => vec![1, 3],
        "<" => vec![1],
        "<=" => vec![1, 2],
        ">" => vec![3],
        ">=" => vec![2, 3],
        _ => unreachable!(),
    }
}

#[test]
fn scalar_compare_matches_independent_oracle_for_every_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_fixtures(&core, &ctx);
    for f in &FIXTURES {
        for op in ["=", "<>", "<", "<=", ">", ">="] {
            let sql = format!(
                "SELECT id FROM {ITEMS} WHERE {col} {op} (SELECT {col} FROM {REFS} LIMIT 10) LIMIT 100",
                col = f.col
            );
            assert_eq!(ids(&core, &ctx, &sql), expected(op), "sql={sql}");
            // 否定越し: `NOT (a op b)` は反対の演算子と同値（NULL 行は共に除外）。
            let negated = format!(
                "SELECT id FROM {ITEMS} WHERE NOT {col} {op} (SELECT {col} FROM {REFS} LIMIT 10) LIMIT 100",
                col = f.col
            );
            let complement: Vec<u64> = [1u64, 2, 3]
                .into_iter()
                .filter(|i| !expected(op).contains(i))
                .collect();
            assert_eq!(ids(&core, &ctx, &negated), complement, "sql={negated}");
        }
    }
}

#[test]
fn scalar_compare_bytea_boolean_and_enum() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(
        &core,
        &ctx,
        ITEMS,
        1,
        &[("blob", "'\\x01'"), ("ok", "false"), ("mood", "'happy'")],
    );
    ins(
        &core,
        &ctx,
        ITEMS,
        2,
        &[("blob", "'\\x0a'"), ("ok", "true"), ("mood", "'sad'")],
    );
    ins(
        &core,
        &ctx,
        ITEMS,
        3,
        &[("blob", "'\\xff'"), ("ok", "true")],
    );
    ins(&core, &ctx, ITEMS, 4, &[]);
    ins(
        &core,
        &ctx,
        REFS,
        1,
        &[
            ("blob", "'\\x0a'"),
            ("ok", "true"),
            ("mood", "'sad'"),
            ("label", "'zzz'"),
        ],
    );
    let q = |col: &str, op: &str| {
        format!(
            "SELECT id FROM {ITEMS} WHERE {col} {op} (SELECT {col} FROM {REFS} LIMIT 10) LIMIT 100"
        )
    };
    assert_eq!(ids(&core, &ctx, &q("blob", "=")), vec![2]);
    assert_eq!(ids(&core, &ctx, &q("blob", "<>")), vec![1, 3]);
    assert_eq!(ids(&core, &ctx, &q("blob", ">")), vec![3]);
    assert_eq!(ids(&core, &ctx, &q("ok", "=")), vec![2, 3]);
    assert_eq!(ids(&core, &ctx, &q("ok", "<>")), vec![1]);
    assert_eq!(ids(&core, &ctx, &q("mood", "=")), vec![2]);
    assert_eq!(ids(&core, &ctx, &q("mood", "<>")), vec![1]);
    // BOOLEAN・ENUM の範囲比較は既存のリテラル比較と同じく 22000。
    assert_eq!(err_code(&core, &ctx, &q("ok", "<")).0, "22000");
    assert_eq!(err_code(&core, &ctx, &q("mood", "<")).0, "22000");
    // ENUM 語彙外の値（TEXT 側の `zzz`）との比較: `=` は常に偽、`<>` は非 NULL の全行で真。
    let vs_label = |op: &str| {
        format!(
            "SELECT id FROM {ITEMS} WHERE mood {op} (SELECT label FROM {REFS} LIMIT 10) LIMIT 100"
        )
    };
    assert_eq!(ids(&core, &ctx, &vs_label("=")), Vec::<u64>::new());
    assert_eq!(ids(&core, &ctx, &vs_label("<>")), vec![1, 2]);
}

#[test]
fn scalar_compare_with_aggregate_inner() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    for (id, qty) in [(1u64, 1i64), (2, 2), (3, 3), (4, 10)] {
        ins(
            &core,
            &ctx,
            ITEMS,
            id,
            &[("qty", &qty.to_string()), ("ratio", &format!("{qty}.5"))],
        );
    }
    for id in 1..=3u64 {
        ins(
            &core,
            &ctx,
            REFS,
            id,
            &[("qty", &(id as i64 * 2).to_string())],
        );
    }
    let q = |col: &str, op: &str, agg: &str| {
        format!("SELECT id FROM {ITEMS} WHERE {col} {op} (SELECT {agg} FROM {REFS}) LIMIT 100")
    };
    // COUNT(*) = 3。
    assert_eq!(ids(&core, &ctx, &q("qty", "=", "COUNT(*)")), vec![3]);
    // MAX(qty) = 6 / MIN(qty) = 2 / SUM(qty) = 12。
    assert_eq!(ids(&core, &ctx, &q("qty", ">", "MAX(qty)")), vec![4]);
    assert_eq!(ids(&core, &ctx, &q("qty", "<=", "MIN(qty)")), vec![1, 2]);
    assert_eq!(
        ids(&core, &ctx, &q("qty", "<", "SUM(qty)")),
        vec![1, 2, 3, 4]
    );
    // AVG(qty) = 4.0（DOUBLE）を整数列・DOUBLE 列と数値族として比較する。
    assert_eq!(ids(&core, &ctx, &q("qty", ">", "AVG(qty)")), vec![4]);
    assert_eq!(ids(&core, &ctx, &q("ratio", ">=", "AVG(qty)")), vec![4]);
    // 集計内側の WHERE が 0 行（SUM は NULL）→ UNKNOWN で 0 件。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM {ITEMS} WHERE qty = (SELECT SUM(qty) FROM {REFS} WHERE qty > 100) LIMIT 100"
            )
        ),
        Vec::<u64>::new()
    );
    // `GROUP BY` 集計はグループキー＋集計の 2 項目以上になるため、単一列の
    // 契約（投影列数 ≠ 1）で 42601 になる。
    let (code, _) = err_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {ITEMS} WHERE qty = (SELECT qty, COUNT(*) FROM {REFS} GROUP BY qty) LIMIT 100"
        ),
    );
    assert_eq!(code, "42601");
}

#[test]
fn scalar_zero_rows_and_null_value_yield_no_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1")]);
    ins(&core, &ctx, ITEMS, 2, &[]);
    // refs は空（0 行）。どの演算子でも、`NOT` 越しでも 0 件。
    for op in ["=", "<>", "<", ">="] {
        for not in ["", "NOT "] {
            let sql = format!(
                "SELECT id FROM {ITEMS} WHERE {not}qty {op} (SELECT qty FROM {REFS} LIMIT 10) LIMIT 100"
            );
            assert_eq!(ids(&core, &ctx, &sql), Vec::<u64>::new(), "sql={sql}");
        }
    }
    // refs に NULL 値の 1 行 → UNKNOWN。
    ins(&core, &ctx, REFS, 1, &[("name", "'x'")]);
    let sql =
        format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 10) LIMIT 100");
    assert_eq!(ids(&core, &ctx, &sql), Vec::<u64>::new());
}

#[test]
fn scalar_two_rows_is_an_error_but_only_for_visible_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    ins(&core, &a, ITEMS, 1, &[("qty", "5")]);
    ins(&core, &a, REFS, 1, &[("qty", "5")]);
    let sql =
        format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 10) LIMIT 100");
    assert_eq!(ids(&core, &a, &sql), vec![1]);
    // 他テナントの行は tenant-a の内側には見えないため、行数エラーも発火しない。
    ins(&core, &b, REFS, 100, &[("qty", "7")]);
    ins(&core, &b, REFS, 101, &[("qty", "8")]);
    assert_eq!(ids(&core, &a, &sql), vec![1]);
    // 自テナントに 2 行目を足すとエラー（先頭行を採用しない）。
    ins(&core, &a, REFS, 2, &[("qty", "6")]);
    let (code, e) = err_code(&core, &a, &sql);
    assert_eq!(code, "22000");
    assert!(e.to_string().contains("more than one row"));
    // `LIMIT 1` を付ければ 1 行になり成功する。
    let limited =
        format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 1) LIMIT 100");
    assert!(core.execute_sql(&a, &limited).is_ok());
}

#[test]
fn scalar_static_validation_and_unsupported_forms() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1"), ("name", "'a'")]);
    let cases: Vec<(String, &str)> = vec![
        // 投影列数 ≠ 1。
        (format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty, name FROM {REFS} LIMIT 1) LIMIT 100"), "42601"),
        (format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT * FROM {REFS} LIMIT 1) LIMIT 100"), "42601"),
        // 値族の不一致（TEXT 列と整数）。行数に依存しない。
        (format!("SELECT id FROM {ITEMS} WHERE name = (SELECT qty FROM {REFS} LIMIT 1) LIMIT 100"), "22000"),
        // 未知の対象列。
        (format!("SELECT id FROM {ITEMS} WHERE nope = (SELECT qty FROM {REFS} LIMIT 1) LIMIT 100"), "22000"),
        // 逆向き比較・式への埋め込み・投影位置の式内への埋め込み
        // （項目全体の投影位置サブクエリは Issue #1352 で対応。`sql29_projection_subquery.rs`）。
        (format!("SELECT id FROM {ITEMS} WHERE (SELECT qty FROM {REFS} LIMIT 1) < qty LIMIT 100"), "42601"),
        (format!("SELECT id FROM {ITEMS} WHERE qty > (SELECT qty FROM {REFS} LIMIT 1) * 2 LIMIT 100"), "42601"),
        (format!("SELECT (SELECT qty FROM {REFS} LIMIT 1) + 1 FROM {ITEMS} LIMIT 100"), "42601"),
        // ランキング付き検索・集合演算の内側。
        (format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} UNION SELECT qty FROM {REFS}) LIMIT 100"), "42601"),
        // `LIMIT` 省略（Scan 形）。
        (format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS}) LIMIT 100"), "42601"),
    ];
    for (sql, code) in cases {
        assert_eq!(err_code(&core, &ctx, &sql).0, code, "sql={sql}");
    }
}

#[test]
fn scalar_nesting_depth_and_execution_budget() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1")]);
    ins(&core, &ctx, REFS, 1, &[("qty", "1")]);
    // ネスト 2 段（スカラーの内側にスカラー）。
    let nested = format!(
        "SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} WHERE qty = \
         (SELECT qty FROM {REFS} LIMIT 1) LIMIT 1) LIMIT 100"
    );
    assert_eq!(ids(&core, &ctx, &nested), vec![1]);
    // 深さ 5 は 54000。
    let mut deep = format!("SELECT qty FROM {REFS} LIMIT 1");
    for _ in 0..4 {
        deep = format!("SELECT qty FROM {REFS} WHERE qty = ({deep}) LIMIT 1");
    }
    let sql = format!("SELECT id FROM {ITEMS} WHERE qty = ({deep}) LIMIT 100");
    assert_eq!(err_code(&core, &ctx, &sql).0, "54000");
    // 実行回数 16 超（AND で 17 個）は 54000。
    let many = (0..17)
        .map(|_| format!("qty = (SELECT qty FROM {REFS} LIMIT 1)"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("SELECT id FROM {ITEMS} WHERE {many} LIMIT 100");
    assert_eq!(err_code(&core, &ctx, &sql).0, "54000");
}

#[test]
fn correlated_subqueries_are_rejected_with_42601() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(
        &core,
        &ctx,
        ITEMS,
        1,
        &[("qty", "1"), ("outer_only", "'a'")],
    );
    ins(&core, &ctx, REFS, 1, &[("qty", "1"), ("label", "'a'")]);
    let corr = "correlated";
    let cases: Vec<String> = vec![
        // 内側の WHERE が外側だけにある列を参照する（Scalar・IN・EXISTS・NOT 系）。
        format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} WHERE label = outer_only LIMIT 1) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE qty IN (SELECT qty FROM {REFS} WHERE label = outer_only LIMIT 10) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE qty NOT IN (SELECT qty FROM {REFS} WHERE label = outer_only LIMIT 10) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE EXISTS (SELECT id FROM {REFS} WHERE label = outer_only LIMIT 1) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE NOT EXISTS (SELECT id FROM {REFS} WHERE label = outer_only LIMIT 1) LIMIT 100"),
        // 内側の投影が外側だけにある列を参照する。
        format!("SELECT id FROM {ITEMS} WHERE name IN (SELECT outer_only FROM {REFS} LIMIT 10) LIMIT 100"),
        // 内側の集計引数・GROUP BY が外側だけにある列を参照する。
        format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT COUNT(outer_only) FROM {REFS}) LIMIT 100"),
        // ネスト 2 段で最外側の列を参照する。
        format!("SELECT id FROM {ITEMS} WHERE qty IN (SELECT qty FROM {REFS} WHERE qty IN (SELECT qty FROM {REFS} WHERE label = outer_only LIMIT 10) LIMIT 10) LIMIT 100"),
    ];
    for sql in cases {
        let (code, e) = err_code(&core, &ctx, &sql);
        assert_eq!(code, "42601", "sql={sql} err={e:?}");
        assert!(e.to_string().contains(corr), "sql={sql} err={e:?}");
    }
    // どのスコープにも無い名前は従来どおり 22000（unknown column）。
    let (code, _) = err_code(
        &core,
        &ctx,
        &format!("SELECT id FROM {ITEMS} WHERE qty IN (SELECT qty FROM {REFS} WHERE label = nowhere LIMIT 10) LIMIT 100"),
    );
    assert_eq!(code, "22000");
    // 内外に同名の列がある場合は内側を優先して受理する（`name` は両方に有る）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {ITEMS} WHERE qty IN (SELECT qty FROM {REFS} WHERE qty = 1 LIMIT 10) LIMIT 100")
        ),
        vec![1]
    );
    // 修飾参照は内側の構文解析が 42601 で拒否する。
    let (code, _) = err_code(
        &core,
        &ctx,
        &format!("SELECT id FROM {ITEMS} WHERE qty IN (SELECT qty FROM {REFS} WHERE label = items.outer_only LIMIT 10) LIMIT 100"),
    );
    assert_eq!(code, "42601");
}

#[test]
fn in_and_not_in_support_typed_and_integer_targets() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_fixtures(&core, &ctx);
    // refs に 2 行目（v3）を足す: 内側の集合は {v2, v3}。
    let refs: Vec<(&str, &str)> = FIXTURES.iter().map(|f| (f.col, f.v[2])).collect();
    ins(&core, &ctx, REFS, 2, &refs);
    for f in FIXTURES.iter().filter(|f| f.col != "name") {
        let inner = format!("(SELECT {col} FROM {REFS} LIMIT 100)", col = f.col);
        let in_sql = format!(
            "SELECT id FROM {ITEMS} WHERE {} IN {inner} LIMIT 100",
            f.col
        );
        let not_in_sql = format!(
            "SELECT id FROM {ITEMS} WHERE {} NOT IN {inner} LIMIT 100",
            f.col
        );
        assert_eq!(ids(&core, &ctx, &in_sql), vec![2, 3], "sql={in_sql}");
        // NOT IN: 非 NULL で集合外の行のみ（id 4 の NULL 行は UNKNOWN で除外）。
        assert_eq!(ids(&core, &ctx, &not_in_sql), vec![1], "sql={not_in_sql}");
    }
    // BYTEA。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    ins(&core, &ctx, ITEMS, 1, &[("blob", "'\\x01'")]);
    ins(&core, &ctx, ITEMS, 2, &[("blob", "'\\x0a'")]);
    ins(&core, &ctx, ITEMS, 3, &[]);
    ins(&core, &ctx, REFS, 1, &[("blob", "'\\x0a'")]);
    let q = |neg: &str| {
        format!("SELECT id FROM {ITEMS} WHERE blob {neg}IN (SELECT blob FROM {REFS} LIMIT 100) LIMIT 100")
    };
    assert_eq!(ids(&core, &ctx, &q("")), vec![2]);
    assert_eq!(ids(&core, &ctx, &q("NOT ")), vec![1]);
    // 値族の不一致は 22000。
    let (code, _) = err_code(
        &core,
        &ctx,
        &format!("SELECT id FROM {ITEMS} WHERE day IN (SELECT at FROM {REFS} LIMIT 10) LIMIT 100"),
    );
    assert_eq!(code, "22000");
    // 浮動小数列と疑似列 `id` の IN は Issue #1352 で対象化（専用テストが固定する）。
    // 値族の不一致（DOUBLE 対象 × TEXT 投影）は 22000 のまま。
    let (code, _) = err_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {ITEMS} WHERE ratio IN (SELECT name FROM {REFS} LIMIT 10) LIMIT 100"
        ),
    );
    assert_eq!(code, "22000");
}

/// Issue #1352: `REAL`／`DOUBLE PRECISION` 列と疑似列 `id` を対象とする `IN`／`NOT IN`。
/// 期待値は Rust 側で素朴に求めた独立オラクル。
#[test]
fn in_subquery_float_and_row_id_targets() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // items: id 1..=4 の ratio（4 は NULL）。refs: ratio 2.5 と -0.0。
    for (id, v) in [(1u64, "1.5"), (2, "2.5"), (3, "0.0")] {
        ins(&core, &ctx, ITEMS, id, &[("ratio", v)]);
    }
    ins(&core, &ctx, ITEMS, 4, &[]);
    ins(&core, &ctx, REFS, 1, &[("ratio", "2.5"), ("qty", "3")]);
    ins(&core, &ctx, REFS, 2, &[("ratio", "-0.0"), ("qty", "-7")]);
    ins(&core, &ctx, REFS, 3, &[("ratio", "2.5")]);
    let q = |neg: &str| {
        format!(
            "SELECT id FROM {ITEMS} WHERE ratio {neg}IN (SELECT ratio FROM {REFS} LIMIT 100) LIMIT 100"
        )
    };
    // -0.0 と 0.0 は等しい。NULL 行は IN・NOT IN ともに UNKNOWN で除外。
    assert_eq!(ids(&core, &ctx, &q("")), vec![2, 3]);
    assert_eq!(ids(&core, &ctx, &q("NOT ")), vec![1]);

    // 疑似列 `id`: 内側が id・BIGINT 列（負値は一致しない・NULL は除外）。
    let id_in = |inner: &str, neg: &str| {
        format!("SELECT id FROM {ITEMS} WHERE id {neg}IN ({inner}) LIMIT 100")
    };
    assert_eq!(
        ids(
            &core,
            &ctx,
            &id_in(&format!("SELECT id FROM {REFS} LIMIT 100"), "")
        ),
        vec![1, 2, 3]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            &id_in(&format!("SELECT qty FROM {REFS} LIMIT 100"), "")
        ),
        vec![3]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            &id_in(
                &format!("SELECT id FROM {REFS} WHERE id < 3 LIMIT 100"),
                "NOT "
            )
        ),
        vec![3, 4]
    );
    // 内側 0 行は IN で 0 件。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &id_in(
                &format!("SELECT id FROM {REFS} WHERE id > 99 LIMIT 100"),
                ""
            )
        ),
        Vec::<u64>::new()
    );
    // 値族の不一致は内側の行数に関わらず 22000。
    for sql in [
        format!("SELECT id FROM {ITEMS} WHERE name IN (SELECT id FROM {REFS} LIMIT 10) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE ratio IN (SELECT qty FROM {REFS} LIMIT 10) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE id IN (SELECT name FROM {REFS} WHERE id > 99 LIMIT 10) LIMIT 100"),
    ] {
        assert_eq!(err_code(&core, &ctx, &sql).0, "22000", "sql={sql}");
    }
}

/// Issue #1352: 浮動小数・`id` の `IN` でも他テナント行が結果を変えない（RLS-10 (b)）。
#[test]
fn in_subquery_float_and_row_id_ignore_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    ins(&core, &a, ITEMS, 1, &[("ratio", "1.5")]);
    ins(&core, &a, ITEMS, 2, &[("ratio", "2.5")]);
    ins(&core, &a, REFS, 1, &[("ratio", "2.5")]);
    let queries = [
        format!("SELECT id FROM {ITEMS} WHERE ratio IN (SELECT ratio FROM {REFS} LIMIT 100) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE ratio NOT IN (SELECT ratio FROM {REFS} LIMIT 100) LIMIT 100"),
        format!("SELECT id FROM {ITEMS} WHERE id IN (SELECT id FROM {REFS} LIMIT 100) LIMIT 100"),
    ];
    let before: Vec<Vec<u64>> = queries.iter().map(|q| ids(&core, &a, q)).collect();
    assert_eq!(before, vec![vec![2], vec![1], vec![1]]);
    for i in 0..20u64 {
        ins(&core, &b, REFS, 100 + i, &[("ratio", "1.5")]);
        ins(&core, &b, ITEMS, 200 + i, &[("ratio", "2.5")]);
    }
    let after: Vec<Vec<u64>> = queries.iter().map(|q| ids(&core, &a, q)).collect();
    assert_eq!(after, before);
}

#[test]
fn integer_in_and_not_in_distinct_value_caps() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1")]);
    for i in 0..300u64 {
        ins(
            &core,
            &ctx,
            REFS,
            i + 1,
            &[("qty", &(i as i64).to_string())],
        );
    }
    // 300 distinct（> 256 / > 128）はどちらも 54000。
    for neg in ["", "NOT "] {
        let sql = format!(
            "SELECT id FROM {ITEMS} WHERE qty {neg}IN (SELECT qty FROM {REFS} LIMIT 1000) LIMIT 100"
        );
        assert_eq!(err_code(&core, &ctx, &sql).0, "54000", "sql={sql}");
    }
    // 上限内（IN は 256 まで）は受理される。
    let sql = format!(
        "SELECT id FROM {ITEMS} WHERE qty IN (SELECT qty FROM {REFS} WHERE qty < 200 LIMIT 1000) LIMIT 100"
    );
    assert_eq!(ids(&core, &ctx, &sql), vec![1]);
    let sql = format!(
        "SELECT id FROM {ITEMS} WHERE qty NOT IN (SELECT qty FROM {REFS} WHERE qty > 200 LIMIT 1000) LIMIT 100"
    );
    assert_eq!(ids(&core, &ctx, &sql), vec![1]);
}

/// RLS-10 (b): 3 テナントで、他テナントの行（NULL・大量行）が結果・エラー・`wire_code` を
/// 変えない（陽性対照つき）。
#[test]
fn subquery_forms_ignore_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    let c = ctx_for("tenant-c");
    for (id, qty) in [(1u64, 1i64), (2, 2), (3, 3)] {
        ins(&core, &a, ITEMS, id, &[("qty", &qty.to_string())]);
    }
    ins(&core, &a, REFS, 1, &[("qty", "2")]);
    let queries = [
        format!("SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 10) LIMIT 100"),
        format!(
            "SELECT id FROM {ITEMS} WHERE qty NOT IN (SELECT qty FROM {REFS} LIMIT 10) LIMIT 100"
        ),
        format!("SELECT id FROM {ITEMS} WHERE qty > (SELECT COUNT(*) FROM {REFS}) LIMIT 100"),
    ];
    let before: Vec<Vec<u64>> = queries.iter().map(|q| ids(&core, &a, q)).collect();
    assert_eq!(before, vec![vec![2], vec![1, 3], vec![2, 3]]);
    // 他テナントに NULL 行・値の異なる行を大量に追加する。
    for i in 0..40u64 {
        ins(
            &core,
            &b,
            REFS,
            1000 + i,
            &[("qty", &(i as i64 + 10).to_string())],
        );
        ins(&core, &c, REFS, 2000 + i, &[]);
        ins(&core, &b, ITEMS, 3000 + i, &[("qty", "2")]);
    }
    let after: Vec<Vec<u64>> = queries.iter().map(|q| ids(&core, &a, q)).collect();
    assert_eq!(after, before);
    // 陽性対照: tenant-b 自身には自分の refs（40 行）が見えるため、スカラーは行数エラーになる。
    assert_eq!(err_code(&core, &b, &queries[0]).0, "22000");
    // tenant-c の内側は NULL 40 行のみ → NOT IN は真にならず 0 件（自分の items は 0 行）。
    assert_eq!(ids(&core, &c, &queries[1]), Vec::<u64>::new());
}

#[test]
fn scalar_subquery_stays_rejected_in_non_subquery_contexts() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    ins(&core, &ctx, ITEMS, 1, &[("qty", "1")]);
    ins(&core, &ctx, REFS, 1, &[("qty", "1")]);
    for sql in [
        format!("UPDATE {ITEMS} SET name = 'x' WHERE qty = (SELECT qty FROM {REFS} LIMIT 1)"),
        format!("DELETE FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 1)"),
        format!(
            "EXPLAIN SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 1) LIMIT 10"
        ),
        format!(
            "CREATE VIEW v1 AS SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 1)"
        ),
    ] {
        let err = core
            .execute_sql_in_session(&ctx, &mut SessionState::default(), &sql)
            .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql={sql} err={err:?}");
    }
    // 拡張クエリプロトコル: `$n` 併用は 42601、`$n` なしは受理。
    let err = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 1) AND name = $1 LIMIT 10"
        ))
        .expect_err("scalar subquery with $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
    let prepared = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {ITEMS} WHERE qty = (SELECT qty FROM {REFS} LIMIT 1) LIMIT 10"
        ))
        .expect("unparameterized scalar subquery must be accepted");
    assert_eq!(prepared.param_count(), 0);
}
