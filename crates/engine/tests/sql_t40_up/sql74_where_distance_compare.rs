//! `WHERE` 内の距離比較（`<VECTOR 列> <距離演算子> <ベクトル> <比較演算子> <数値>` と
//! `[NOT] BETWEEN`）の構文・束縛契約の結合テスト（SQL-74 (1)・Issue #2035）。
//!
//! 評価・打ち切り・EXPLAIN の経路表示は Issue #2036 の担当で、本 Issue の範囲は解析と束縛まで。
//! 束縛が通った文は評価が結線されるまで fail-closed の `0A000` で止まり、行を 1 件も返さず、
//! DML は 1 行も書き換えない。被演算子の違反は `0A000` より先に既存の分類で拒否される。
//! 投影位置など WHERE 以外の距離式は従来どおり `42601` のまま。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3), n INTEGER, t TEXT)",
    )
    .expect("create table");
    for (i, v) in ["[1,0,0]", "[0,1,0]", "[0,0,1]"].iter().enumerate() {
        core.execute_sql_in_session(
            &ctx(),
            &mut session,
            &format!(
                "INSERT INTO docs (id, embedding, n, t) VALUES ({i}, '{v}', {i}, 'x') \
                 USING OPERATION_ID 'sql74-{i}'"
            ),
        )
        .expect("insert");
    }
    (core, session, CleanupGuard(path))
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(), session, sql) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

fn count(core: &EngineCore, session: &mut SessionState) -> usize {
    match core
        .execute_sql_in_session(&ctx(), session, "SELECT id FROM docs")
        .expect("select")
    {
        SqlOutcome::Query(r) => r.rows.len(),
        _ => panic!("expected Query outcome"),
    }
}

/// 6 つの比較演算子・`BETWEEN`・4 種別の距離演算子・両向きは束縛まで通り、`0A000` で止まる。
#[test]
fn comparisons_and_between_are_gated_with_0a000() {
    let (core, mut session, _g) = setup("sql74-gate");
    for op in ["<", "<=", ">", ">=", "=", "<>", "!="] {
        let sql = format!("SELECT id FROM docs WHERE embedding <=> '[1,0,0]' {op} 0.5");
        assert_eq!(code(&core, &mut session, &sql), "0A000", "{sql}");
    }
    for kind in ["<=>", "<->", "<#>", "<+>"] {
        let sql = format!("SELECT id FROM docs WHERE embedding {kind} '[1,0,0]' < 0.5");
        assert_eq!(code(&core, &mut session, &sql), "0A000", "{sql}");
    }
    for sql in [
        "SELECT id FROM docs WHERE '[1,0,0]' <-> embedding < 0.5",
        "SELECT id FROM docs WHERE 0.5 > embedding <-> '[1,0,0]'",
        "SELECT id FROM docs WHERE embedding <-> '[1,0,0]' BETWEEN 0.1 AND 0.5",
        "SELECT id FROM docs WHERE embedding <-> '[1,0,0]' NOT BETWEEN 0.1 AND 0.5",
        "SELECT id FROM docs WHERE NOT embedding <-> '[1,0,0]' < 0.5",
        "SELECT id FROM docs WHERE (embedding <-> '[1,0,0]') < 0.5",
        "SELECT id FROM docs WHERE n = 1 AND embedding <-> '[1,0,0]' < 0.5",
        "SELECT id FROM docs WHERE n = 1 OR embedding <-> '[1,0,0]' < 0.5",
        "SELECT id FROM docs WHERE embedding <-> '[1,0,0]' < 0.5 + 0.1",
        "SELECT id FROM docs WHERE embedding <=> '[1,0,0]' < 0.5 \
         ORDER BY embedding <=> '[1,0,0]' LIMIT 2",
        "EXPLAIN SELECT id FROM docs WHERE embedding <-> '[1,0,0]' < 0.5",
    ] {
        assert_eq!(code(&core, &mut session, sql), "0A000", "{sql}");
    }
}

/// 被演算子の違反は `0A000` より先に既存の分類で拒否される。
#[test]
fn operand_violations_use_existing_classification() {
    let (core, mut session, _g) = setup("sql74-operands");
    let cases = [
        ("WHERE nope <-> '[1,0,0]' < 1", "42703"),
        ("WHERE n <-> '[1,0,0]' < 1", "42883"),
        ("WHERE t <-> '[1,0,0]' < 1", "42883"),
        ("WHERE embedding <-> '[1,0]' < 1", "22000"),
        ("WHERE embedding <-> 'abc' < 1", "22000"),
        ("WHERE embedding <-> '[1,0,0]' < 'abc'", "0A000"),
        ("WHERE embedding <-> embedding < 1", "0A000"),
        ("WHERE embedding <-> NULL < 1", "0A000"),
        ("WHERE '[1,0,0]' <-> '[1,0,0]' < 1", "0A000"),
    ];
    for (clause, want) in cases {
        let sql = format!("SELECT id FROM docs {clause}");
        assert_eq!(code(&core, &mut session, &sql), want, "{sql}");
    }
    let long = format!("[{}]", "1,".repeat(40_000));
    let sql = format!("SELECT id FROM docs WHERE embedding <-> '{long}' < 1");
    assert_eq!(code(&core, &mut session, &sql), "54000");
}

/// 比較の無い距離式や WHERE 以外の位置は従来どおり `42601`。
#[test]
fn other_positions_stay_rejected() {
    let (core, mut session, _g) = setup("sql74-other");
    for sql in [
        "SELECT id FROM docs WHERE embedding <-> '[1,0,0]'",
        "SELECT embedding <-> '[1,0,0]' FROM docs",
        "SELECT id FROM docs ORDER BY embedding <-> '[1,0,0]' LIMIT 1",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42601", "{sql}");
    }
}

/// DML は束縛の段階で拒否され、1 行も書き換えない。
#[test]
fn dml_is_rejected_without_writing() {
    let (core, mut session, _g) = setup("sql74-dml");
    let before = count(&core, &mut session);
    for sql in [
        "DELETE FROM docs WHERE embedding <-> '[1,0,0]' < 1 USING OPERATION_ID 'd-1'",
        "UPDATE docs SET n = 9 WHERE embedding <-> '[1,0,0]' < 1 USING OPERATION_ID 'u-1'",
    ] {
        let got = code(&core, &mut session, sql);
        assert!(got == "0A000" || got == "42601", "{sql}: {got}");
    }
    assert_eq!(count(&core, &mut session), before);
}

/// エラー文言にベクトルリテラルの値を含めない。
#[test]
fn error_detail_does_not_echo_literal() {
    let (core, mut session, _g) = setup("sql74-leak");
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs WHERE embedding <-> '[7.654321,0,0]' < 1",
        )
        .expect_err("gated");
    assert!(!format!("{err}").contains("7.654321"));
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs WHERE embedding <-> '[7.654321,0]' < 1",
        )
        .expect_err("dim mismatch");
    assert!(!format!("{err}").contains("7.654321"));
}

/// 拡張クエリの `<=> $1`（Cosine のみ既存の `$n` パターンが受理）は Parse を通り、Execute で `0A000`。
#[test]
fn extended_query_param_is_gated() {
    let (core, mut session, _g) = setup("sql74-ext");
    let prepared = core
        .parse_sql_prepared("SELECT id FROM docs WHERE embedding <=> $1 < 0.5")
        .expect("parse");
    let binds = vec![Some(b"[1,0,0]".to_vec())];
    let bound = core.bind_prepared(&prepared, &binds).expect("bind");
    let err = core
        .execute_parsed_in_session(&ctx(), &mut session, &bound)
        .expect_err("gated");
    assert_eq!(err.wire_code(), "0A000");
}
