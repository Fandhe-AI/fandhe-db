//! `INSERT ... VALUES` と列 `DEFAULT` の定数式受理（SQL-45・Issue #1543）の結合テスト。
//!
//! 構文段で畳み込まれたリテラルが既存の束縛・格納経路を通ることを、実 `Storage` で
//! 端到端に固定する。`sql_insert_null_literal.rs` と同じ流儀（`unique_db_path`／
//! `CleanupGuard`）。ポインタ: `docs/design/implementation-status.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), n INTEGER, t TEXT, \
         d INTEGER DEFAULT 1+1, s TEXT DEFAULT upper('ab'))",
    )
    .expect("create table with constant-expression defaults");
    (core, path, session)
}

const VEC: &str = "'[0.1,0.2,0.3,0.4]'";

fn insert(core: &EngineCore, cols: &str, vals: &str, op: &str) -> Result<(), String> {
    core.execute_insert_sql(
        &ctx(),
        &format!("INSERT INTO docs ({cols}) VALUES {vals} USING OPERATION_ID '{op}'"),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

fn select(core: &EngineCore, session: &mut SessionState, sql: &str) -> Vec<Vec<Cell>> {
    match core
        .execute_sql_in_session(&ctx(), session, sql)
        .expect("select ok")
    {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        _ => panic!("expected Query outcome"),
    }
}

fn dbg(c: &Cell) -> String {
    format!("{c:?}")
}

#[test]
fn values_accept_constant_expressions() {
    let (core, path, mut session) = setup("sql45-values");
    let _guard = CleanupGuard(path);
    insert(
        &core,
        "id, embedding, n, t",
        &format!("(1, {VEC}, 1+1, upper('a'))"),
        "op-1",
    )
    .expect("constant expressions");
    let rows = select(&core, &mut session, "SELECT n, t FROM docs LIMIT 10");
    assert_eq!(rows.len(), 1);
    assert!(dbg(&rows[0][0]).contains('2'), "{:?}", rows[0][0]);
    assert!(dbg(&rows[0][1]).contains('A'), "{:?}", rows[0][1]);
}

#[test]
fn defaults_accept_constant_expressions() {
    let (core, path, mut session) = setup("sql45-default");
    let _guard = CleanupGuard(path);
    insert(&core, "id, embedding", &format!("(1, {VEC})"), "op-1").expect("omit defaults");
    let rows = select(&core, &mut session, "SELECT d, s FROM docs LIMIT 10");
    assert_eq!(rows.len(), 1);
    assert!(dbg(&rows[0][0]).contains('2'), "{:?}", rows[0][0]);
    assert!(dbg(&rows[0][1]).contains("AB"), "{:?}", rows[0][1]);
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "ALTER TABLE docs ADD COLUMN e INTEGER DEFAULT 2*3",
    )
    .expect("alter add column with constant default");
    let rows = select(&core, &mut session, "SELECT e FROM docs LIMIT 10");
    assert!(dbg(&rows[0][0]).contains('6'), "{:?}", rows[0][0]);
}

#[test]
fn column_references_and_boundaries_are_rejected() {
    let (core, path, mut session) = setup("sql45-reject");
    let _guard = CleanupGuard(path);
    let cols = "id, embedding, n";
    assert_eq!(
        insert(&core, cols, &format!("(1, {VEC}, n)"), "op-1").unwrap_err(),
        "42703"
    );
    assert_eq!(
        insert(&core, cols, &format!("(1, {VEC}, id + 1)"), "op-2").unwrap_err(),
        "42703"
    );
    assert_eq!(
        insert(&core, cols, &format!("(1, {VEC}, 1/0)"), "op-3").unwrap_err(),
        "22012"
    );
    assert_eq!(
        insert(&core, cols, &format!("(1, {VEC}, 2147483647+1)"), "op-4").unwrap_err(),
        "22003"
    );
    // 多行の一部が失敗したら全体を拒否する。
    assert!(insert(
        &core,
        cols,
        &format!("(1, {VEC}, 1+1), (2, {VEC}, n)"),
        "op-5"
    )
    .is_err());
    assert!(select(&core, &mut session, "SELECT n FROM docs LIMIT 10").is_empty());
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "CREATE TABLE bad (embedding VECTOR(4), n INTEGER DEFAULT n + 1)",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42703");
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            // 単独の `now()` は時刻 DEFAULT（Issue #1587）として受理されるが、定数式の中の時刻関数は拒否する。
            "CREATE TABLE bad2 (embedding VECTOR(4), n TIMESTAMP DEFAULT now() + 1)",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

/// レビュー指摘: NULL 伝播は反対側の評価エラーを消さず、関数引数の NULL も定数として扱う。
#[test]
fn null_operand_keeps_eval_errors_and_function_null_args() {
    let (core, path, mut session) = setup("sql45-null-err");
    let _guard = CleanupGuard(path);
    let cols = "id, embedding, n";
    for (i, v) in ["NULL + (1/0)", "(1/0) * NULL", "COALESCE(NULL + (1/0), 5)"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            insert(
                &core,
                cols,
                &format!("(1, {VEC}, {v})"),
                &format!("op-e{i}")
            )
            .unwrap_err(),
            "22012",
            "{v}"
        );
    }
    // 選ばれない分岐の評価エラーは従来どおり出ない。
    insert(
        &core,
        "id, embedding, t",
        &format!("(1, {VEC}, upper(NULL))"),
        "op-1",
    )
    .expect("upper(NULL) is NULL");
    insert(
        &core,
        cols,
        &format!("(2, {VEC}, COALESCE(abs(NULL), CASE WHEN 1 = 1 THEN 3 ELSE NULL + (1/0) END))"),
        "op-2",
    )
    .expect("function NULL arg and unselected branch");
    let rows = select(
        &core,
        &mut session,
        "SELECT n FROM docs WHERE id = 2 LIMIT 10",
    );
    assert!(dbg(&rows[0][0]).contains('3'), "{:?}", rows[0][0]);
}

/// 単項 `+` 付き数値・`POSITION` の NULL 引数・`CASE WHEN` の NULL オペランドが
/// 定数式として畳み込まれる（レビュー指摘対応）。
#[test]
fn unary_plus_position_null_and_case_when_null_fold() {
    let (core, path, _session) = setup("sql45-plus-null");
    let _guard = CleanupGuard(path);
    let cols = "id, embedding, n";
    for (i, v) in [
        "+1+1",
        "2*+1",
        "(+1)+1",
        "POSITION(NULL IN 'x')",
        "CASE WHEN NULL = 1 THEN 2 ELSE 3 END",
    ]
    .iter()
    .enumerate()
    {
        insert(
            &core,
            cols,
            &format!("({}, {VEC}, {v})", i + 1),
            &format!("op-p{i}"),
        )
        .unwrap_or_else(|e| panic!("{v}: {e}"));
    }
    assert_eq!(
        insert(&core, cols, &format!("(9, {VEC}, +n)"), "op-p9").unwrap_err(),
        "42601"
    );
}
/// Issue #1546: VALUES 要素の式内 `$n` は演算の相手・関数引数から型が決まり、
/// Parse → Bind → Execute で定数畳み込みを経て格納される。決められない `$n` は `42P18`。
#[test]
fn extended_query_infers_param_types_inside_values_expressions() {
    use engine::catalog::ColumnType;
    use engine::sql::exec::ColumnMeta;
    use engine::sql::params::PreparedParamType;

    let (core, path, mut session) = setup("sql45-values-param");
    let _guard = CleanupGuard(path);
    let sql = "INSERT INTO docs (id, embedding, n, t) VALUES (1, $1, 1 + $2, upper($3)) \
               USING OPERATION_ID $4";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    assert_eq!(prepared.param_count(), 4);
    assert!(matches!(
        prepared.param_types().get(1),
        Some(PreparedParamType::Column(ColumnMeta::Scalar {
            ty: ColumnType::Integer,
            ..
        }))
    ));
    assert_eq!(
        prepared.param_types().get(2),
        Some(&PreparedParamType::Text)
    );
    let values = [
        Some(b"[0.1,0.2,0.3,0.4]".to_vec()),
        Some(b"41".to_vec()),
        Some(b"zz".to_vec()),
        Some(b"op-p1".to_vec()),
    ];
    let parsed = core.bind_prepared(&prepared, &values).expect("bind");
    core.execute_parsed_in_session(&ctx(), &mut session, &parsed)
        .expect("execute");
    let rows = select(&core, &mut session, "SELECT n, t FROM docs LIMIT 10");
    assert_eq!(rows.len(), 1);
    assert!(dbg(&rows[0][0]).contains("42"), "{:?}", rows[0][0]);
    assert!(dbg(&rows[0][1]).contains("ZZ"), "{:?}", rows[0][1]);

    // 不正な整数は式経路でも `22P02`。
    let bad = [
        Some(b"[0.1,0.2,0.3,0.4]".to_vec()),
        Some(b"x".to_vec()),
        Some(b"a".to_vec()),
        Some(b"op-p2".to_vec()),
    ];
    assert_eq!(
        core.bind_prepared(&prepared, &bad)
            .map(|_| ())
            .unwrap_err()
            .wire_code(),
        "22P02"
    );

    for sql in [
        "INSERT INTO docs (id, n) VALUES (2, $1 + $2) USING OPERATION_ID 'x'",
        "INSERT INTO docs (id, n) VALUES (2, COALESCE($1, $2)) USING OPERATION_ID 'x'",
    ] {
        assert_eq!(
            core.parse_sql_prepared(sql)
                .map(|_| ())
                .unwrap_err()
                .wire_code(),
            "42P18",
            "{sql}"
        );
    }
}
