//! `LAG`／`LEAD` の構文受理と値引数の束縛（SQL-64 (1)・Issue #1880）の結合テスト。
//!
//! `tests/sql30_window.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみを使う）。評価（Issue #1881）は未接続で、解析・束縛・型検査を
//! 通過した文は束縛段の単一ゲートが `0A000` で拒否する。したがって「受理」は「型検査の
//! SQLSTATE（`42804` 等）ではなくゲートの `0A000` に到達すること」で確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
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
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("score", ColumnType::Integer, false),
            ColumnDef::new("ratio", ColumnType::Double, false),
            ColumnDef::new("flag", ColumnType::Boolean, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, id: u64, lang: &str, score: i32) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "test-op-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        Visibility::Public,
        &[
            Value::Text(lang.to_string()),
            Value::Integer(score),
            Value::Double(0.5),
            Value::Bool(true),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn setup(label: &str, rows: u64) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=rows {
        let lang = if id % 2 == 0 { "ja" } else { "en" };
        insert_row(&storage, &ctx, id, lang, id as i32 * 10);
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql(ctx, sql) {
        Ok(_) => panic!("expected {sql:?} to be rejected"),
        Err(e) => e.wire_code(),
    }
}

/// 解析・束縛・型検査を通過した文は、評価未接続のゲート（`0A000`）に到達する。
#[test]
fn accepted_forms_reach_the_not_executable_gate() {
    let (core, ctx, _g) = setup("lag-accept", 4);
    let accepted = [
        "SELECT id, lag(score) OVER (ORDER BY score) FROM docs LIMIT 10",
        "SELECT id, LEAD(score, 2) OVER (PARTITION BY lang ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, 0) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score * 2, 1 + 1, -1) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, score) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, NULL) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, NULL) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(lang, 1, 'none') OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, Lead(score) OVER (ORDER BY id) AS nxt FROM docs LIMIT 10",
        // 擬似列 `id` はオフセットに使える（bigint 扱い）。
        "SELECT id, lag(score, id) OVER (ORDER BY id) FROM docs LIMIT 10",
        // BOOLEAN の裸の列は既定値でも宣言型で統一され、ゲートへ到達する。
        "SELECT id, lag(flag, 1, flag) OVER (ORDER BY id) FROM docs LIMIT 10",
    ];
    for sql in accepted {
        assert_eq!(code(&core, &ctx, sql), "0A000", "sql={sql}");
    }
}

#[test]
fn offset_and_default_type_mismatch_is_42804() {
    let (core, ctx, _g) = setup("lag-type", 2);
    let cases = [
        "SELECT id, lag(score, 1.5) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, lang) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, flag) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, 'x') OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(lang, 1, 0) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, flag) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(flag, 1, score) OVER (ORDER BY id) FROM docs LIMIT 10",
    ];
    for sql in cases {
        assert_eq!(code(&core, &ctx, sql), "42804", "sql={sql}");
    }
}

#[test]
fn argument_count_and_shape_errors() {
    let (core, ctx, _g) = setup("lag-shape", 2);
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag() OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42883"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(score, 1, 0, 0) OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42883"
    );
    for sql in [
        "SELECT id, lag(*) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(DISTINCT score) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag($1) OVER (ORDER BY id) FROM docs LIMIT 10",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "sql={sql}");
    }
}

#[test]
fn nested_window_and_aggregate_in_arguments() {
    let (core, ctx, _g) = setup("lag-nested", 2);
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(row_number() OVER ()) OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42P20"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(count(*)) OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42803"
    );
}

/// `LAG`／`LEAD` を名前表へ加えた副作用: `FILTER` 内の `lag(x) OVER ()` は
/// PostgreSQL と同じ分類（`42P20`）になる。
#[test]
fn lag_inside_filter_is_window_misuse() {
    let (core, ctx, _g) = setup("lag-misuse", 2);
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT count(*) FILTER (WHERE lag(score) OVER () > 1) FROM docs"
        ),
        "42P20"
    );
}

#[test]
fn unsupported_value_types_are_0a000() {
    let path = unique_db_path("lag-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "vecs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(embedding) OVER (ORDER BY id) FROM vecs LIMIT 10"
        ),
        "0A000"
    );
}

/// ゲートは走査より前に効くため、行数の異なるテナントでも同じ SQLSTATE になる。
#[test]
fn gate_is_independent_of_row_counts() {
    let sql = "SELECT id, lag(score) OVER (ORDER BY id) FROM docs LIMIT 10";
    let (core0, ctx, _g0) = setup("lag-empty", 0);
    let (core5, ctx5, _g5) = setup("lag-five", 5);
    assert_eq!(code(&core0, &ctx, sql), "0A000");
    assert_eq!(code(&core5, &ctx5, sql), "0A000");
}

/// ビュー越しでは、非公開列を値・オフセット・既定値で参照しても `0A000`（ゲート）には
/// ならず `42703` で拒否される（ゲートとの差から非公開列の存在が漏れない）。
#[test]
fn view_column_scope_covers_all_arguments() {
    let (core, ctx, _g) = setup("lag-view", 3);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW v AS SELECT id, lang FROM docs",
    )
    .expect("create view");
    let run = |sql: &str| {
        let mut s = SessionState::default();
        core.execute_sql_in_session(&ctx, &mut s, sql)
            .expect_err("rejected")
            .wire_code()
    };
    for sql in [
        "SELECT id, lag(score) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, lag(lang, score) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, lag(lang, 1, score) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, lag(lang, 1, 'x' || score) OVER (ORDER BY id) FROM v LIMIT 10",
    ] {
        assert_eq!(run(sql), "42703", "sql={sql}");
    }
}

/// 式層が束縛できない型の裸の列（NUMERIC・UUID）も既定値で宣言型判定され、
/// 同型ならゲート（`0A000`）、異型なら `42804`、運べない型（BYTEA）は `0A000` になる。
#[test]
fn bare_column_defaults_use_declared_types() {
    let path = unique_db_path("lag-declared");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "typed",
            vec![
                ColumnDef::new(
                    "n1",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n2",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
                ColumnDef::new("u1", ColumnType::Uuid, false),
                ColumnDef::new("b1", ColumnType::Bytea, false),
                ColumnDef::new("i1", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let cases = [
        ("lag(n1, 1, n2)", "0A000"),
        ("lag(u1, 1, u1)", "0A000"),
        ("lag(n1, 1, u1)", "42804"),
        ("lag(i1, 1, n1)", "42804"),
        ("lag(b1, 1, b1)", "0A000"),
        ("lag(i1, 1, b1)", "0A000"),
        ("lag(b1, 1, i1)", "0A000"),
    ];
    for (call, want) in cases {
        let sql = format!("SELECT id, {call} OVER (ORDER BY id) FROM typed LIMIT 10");
        assert_eq!(code(&core, &ctx, &sql), want, "sql={sql}");
    }
}
