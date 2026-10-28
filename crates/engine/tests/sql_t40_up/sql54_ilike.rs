//! `WHERE <col> [NOT] ILIKE '<pattern>'` の結合テスト（SQL-54、Issue #1778。
//! ポインタ: `docs/spec/04-behavior/` SQL-54）。畳み込み範囲（ASCII 英字のみ）と
//! ICU 照合列の拒否は ADR `docs/design/like-wildcard-patterns.md` に従う。
//!
//! `tests/sql24_like_patterns.rs`・`tests/sql_collate_icu.rs` と同じ流儀（実 `Storage`＋
//! `CpuScalarProvider`、`engine::tenant::insert_typed_row` による投入）で
//! `EngineCore::execute_sql` を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const CHAR_WIDTH: usize = 5;
const OTHER_TENANT_ID: u64 = 100;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType, c: Option<Collation>| ColumnDef {
        collation: c,
        ..ColumnDef::new(name, ty, true)
    };
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("name", ColumnType::Text, None),
            col("ch", ColumnType::Bpchar(CHAR_WIDTH as u32), None),
            col("iu", ColumnType::Text, Some(Collation::UndIcu)),
            col("tc", ColumnType::Text, Some(Collation::C)),
        ],
    )
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, name: Option<&str>, vis: Visibility) {
    let text = |v: Option<&str>| match v {
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    };
    // `ch` は CHAR(n) の詰め済み表現（末尾空白つき）で保存する。
    let ch = if id == 1 {
        Value::Text(format!("{:<CHAR_WIDTH$}", "Ab"))
    } else {
        Value::Null
    };
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            text(name),
            ch,
            text(name),
            text(name),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

const ROWS: &[(u64, Option<&str>)] = &[
    (1, Some("Alpha")),
    (2, Some("alpha")),
    (3, Some("ALPHABET")),
    (4, Some("beta")),
    (5, Some("Élan")),
    (6, None),
    (7, Some("a_c")),
    (8, Some("abc")),
];

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (id, name) in ROWS {
        insert_row(&storage, "tenant-a", *id, *name, Visibility::Public);
    }
    // 他テナントの Private 行（どの述語にも合う）。
    insert_row(
        &storage,
        "tenant-b",
        OTHER_TENANT_ID,
        Some("alpha"),
        Visibility::Private,
    );
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ids(result: &QueryResult) -> Vec<u64> {
    assert!(
        result.rows.iter().all(|r| r.id != OTHER_TENANT_ID),
        "other tenant row leaked"
    );
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn select(core: &EngineCore, pred: &str) -> Vec<u64> {
    let sql =
        format!("SELECT * FROM docs WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100");
    let result = core
        .execute_sql(&ctx("tenant-a"), &sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    ids(&result)
}

fn code_of_pred(core: &EngineCore, pred: &str) -> &'static str {
    let sql =
        format!("SELECT * FROM docs WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10");
    core.execute_sql(&ctx("tenant-a"), &sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

#[test]
fn ilike_matches_ascii_case_insensitively() {
    let path = unique_db_path("sql54-match");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    assert_eq!(select(&core, "name ILIKE 'alpha'"), vec![1, 2]);
    assert_eq!(select(&core, "name ILIKE 'ALPHA'"), vec![1, 2]);
    assert_eq!(select(&core, "name ILIKE 'al%'"), vec![1, 2, 3]);
    assert_eq!(select(&core, "name ilike '%ET'"), vec![3]);
    assert_eq!(select(&core, "name IlIkE 'A_C'"), vec![7, 8]);
    assert_eq!(select(&core, "name ILIKE 'a\\_c'"), vec![7]);
    // 空パターンは空文字列だけに一致する（空文字列の行は無い）。
    assert_eq!(select(&core, "name ILIKE ''"), Vec::<u64>::new());
    // `%` は非 NULL の全行に一致し、NULL 行（id=6）と他テナントは含まない。
    assert_eq!(select(&core, "name ILIKE '%'"), vec![1, 2, 3, 4, 5, 7, 8]);
}

#[test]
fn ilike_does_not_fold_non_ascii() {
    let path = unique_db_path("sql54-non-ascii");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    assert_eq!(select(&core, "name ILIKE 'Élan'"), vec![5]);
    assert_eq!(select(&core, "name ILIKE 'élan'"), Vec::<u64>::new());
}

#[test]
fn not_ilike_negates_and_excludes_null() {
    let path = unique_db_path("sql54-not");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    assert_eq!(select(&core, "name NOT ILIKE 'alpha%'"), vec![4, 5, 7, 8]);
    assert_eq!(select(&core, "name NOT ILIKE '%'"), Vec::<u64>::new());
    assert_eq!(
        select(&core, "name ILIKE 'alpha' OR name NOT ILIKE 'a%'"),
        vec![1, 2, 4, 5]
    );
}

#[test]
fn pattern_length_limit_applies_to_ilike() {
    let path = unique_db_path("sql54-len");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    let at_limit = "a".repeat(4096);
    assert_eq!(
        select(&core, &format!("name ILIKE '{at_limit}'")),
        Vec::<u64>::new()
    );
    let over = "a".repeat(4097);
    assert_eq!(
        code_of_pred(&core, &format!("name ILIKE '{over}'")),
        "54000"
    );
}

#[test]
fn ilike_result_is_independent_of_scalar_index() {
    let path = unique_db_path("sql54-index");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    let preds = [
        "name ILIKE 'alpha'",
        "name ILIKE 'a%'",
        "name NOT ILIKE 'alpha'",
        "name = 'beta' OR name ILIKE 'a%'",
    ];
    let before: Vec<Vec<u64>> = preds.iter().map(|p| select(&core, p)).collect();
    ddl(&core, "CREATE INDEX idx_name ON docs (name)");
    let after: Vec<Vec<u64>> = preds.iter().map(|p| select(&core, p)).collect();
    assert_eq!(before, after);
    assert_eq!(after.first(), Some(&vec![1, 2]));
}

#[test]
fn char_column_follows_like_and_is_rejected_as_non_text() {
    let path = unique_db_path("sql54-char");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    // `CHAR(n)` 列への `LIKE` が現状 `22000` のため、`ILIKE` も同じ契約（LIKE との対称）。
    assert_eq!(code_of_pred(&core, "ch LIKE 'Ab'"), "22000");
    assert_eq!(code_of_pred(&core, "ch ILIKE 'ab'"), "22000");
}

#[test]
fn icu_collation_is_rejected_and_byte_order_collations_are_accepted() {
    let path = unique_db_path("sql54-icu");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    for pred in ["iu ILIKE 'a%'", "iu NOT ILIKE 'a%'"] {
        assert_eq!(code_of_pred(&core, pred), "0A000", "pred={pred}");
    }
    assert_eq!(select(&core, "tc ILIKE 'alpha'"), vec![1, 2]);
}

#[test]
fn unsupported_ilike_forms_stay_42601() {
    let path = unique_db_path("sql54-unsupported");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    for pred in ["name ILIKE tc", "name ILIKE 1", "name ILIKE 'a' ESCAPE tc"] {
        assert_eq!(code_of_pred(&core, pred), "42601", "pred={pred}");
    }
}

#[test]
fn buffered_projection_view_rejects_ilike_fail_closed() {
    let path = unique_db_path("sql54-view-buffered");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    ddl(
        &core,
        "CREATE VIEW vb AS SELECT iu, name, id FROM docs ORDER BY id LIMIT 50",
    );
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT iu FROM vb WHERE iu ILIKE 'a%' LIMIT 10",
        )
        .expect_err("ILIKE on buffered view must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn simple_view_body_round_trips() {
    let path = unique_db_path("sql54-view-simple");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    ddl(
        &core,
        "CREATE VIEW va AS SELECT * FROM docs WHERE name ILIKE 'alpha%'",
    );
    let result = core
        .execute_sql(&ctx("tenant-a"), "SELECT id, name FROM va LIMIT 100")
        .expect("view select");
    assert_eq!(ids(&result), vec![1, 2, 3]);
}

fn run_dml(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("tenant-a"), &mut SessionState::default(), sql)
}

#[test]
fn dml_with_ilike_is_idempotent_and_distinct_from_like() {
    let path = unique_db_path("sql54-dml");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);

    let del = "DELETE FROM docs WHERE name ILIKE 'ALPHA' USING OPERATION_ID 'op-54-del'";
    match run_dml(&core, del).expect("ILIKE DELETE") {
        SqlOutcome::Delete(d) => assert_eq!(d.rows_affected, 2),
        other => panic!("expected Delete, got {other:?}"),
    }
    assert_eq!(select(&core, "name ILIKE 'alpha%'"), vec![3]);

    // 同一 operation_id・同一内容の再送は冪等。
    let replay = run_dml(&core, del);
    assert!(
        match &replay {
            Ok(SqlOutcome::Delete(d)) => d.rows_affected == 2,
            Err(e) => e.wire_code() == "23505",
            _ => false,
        },
        "replay must be idempotent: {replay:?}"
    );

    // 同一 operation_id で `LIKE` 版へ差し替えた再送は内容不一致（`22023`）。
    let mismatch = run_dml(
        &core,
        "DELETE FROM docs WHERE name LIKE 'ALPHA' USING OPERATION_ID 'op-54-del'",
    )
    .expect_err("LIKE replay with ILIKE operation_id must mismatch");
    assert_eq!(mismatch.wire_code(), "22023");

    // NOT ILIKE の DELETE（残り: 3,4,5,7,8 のうち `a` 始まり以外は 4,5）。
    match run_dml(
        &core,
        "DELETE FROM docs WHERE name NOT ILIKE 'a%' USING OPERATION_ID 'op-54-del2'",
    )
    .expect("NOT ILIKE DELETE")
    {
        SqlOutcome::Delete(d) => assert_eq!(d.rows_affected, 2),
        other => panic!("expected Delete, got {other:?}"),
    }
}
