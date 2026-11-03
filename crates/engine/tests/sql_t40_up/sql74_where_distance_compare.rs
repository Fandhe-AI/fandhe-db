//! `WHERE` 内の距離比較（`<VECTOR 列> <距離演算子> <ベクトル> <比較演算子> <数値>` と
//! `[NOT] BETWEEN`）の構文・束縛契約の結合テスト（SQL-74 (1)・Issue #2035）。
//!
//! 評価は Issue #2036 で結線済みで（値の検証は `sql74_radius_search.rs`）、本ファイルは受理される
//! 構文の範囲と、被演算子の違反が既存の分類で拒否されること（`42703`・`42883`・`22000`・`54000`・
//! `0A000`）を固定する。投影位置など WHERE 以外の距離式は従来どおり `42601` のまま。

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

/// 6 つの比較演算子・`BETWEEN`・4 種別の距離演算子・両向きは受理され、評価まで成功する。
#[test]
fn comparisons_and_between_are_accepted() {
    let (core, mut session, _g) = setup("sql74-gate");
    for op in ["<", "<=", ">", ">=", "=", "<>", "!="] {
        let sql = format!("SELECT id FROM docs WHERE embedding <=> '[1,0,0]' {op} 0.5");
        assert_eq!(code(&core, &mut session, &sql), "ok", "{sql}");
    }
    for kind in ["<=>", "<->", "<#>", "<+>"] {
        let sql = format!("SELECT id FROM docs WHERE embedding {kind} '[1,0,0]' < 0.5");
        assert_eq!(code(&core, &mut session, &sql), "ok", "{sql}");
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
        assert_eq!(code(&core, &mut session, sql), "ok", "{sql}");
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
        ("WHERE embedding <-> '[1,0,0]' < 'abc'", "22P02"),
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

/// 比較の無い距離式や投影位置は従来どおり `42601`（kNN の ORDER BY は #1977 で受理し実行段 `0A000`）。
#[test]
fn other_positions_stay_rejected() {
    let (core, mut session, _g) = setup("sql74-other");
    for sql in [
        "SELECT id FROM docs WHERE embedding <-> '[1,0,0]'",
        "SELECT embedding <-> '[1,0,0]' FROM docs",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42601", "{sql}");
    }
    // Issue #1977: kNN の ORDER BY は 4 種とも構文を通り、カーネル未接続の間は実行段で 0A000。
    assert_eq!(
        code(
            &core,
            &mut session,
            "SELECT id FROM docs ORDER BY embedding <-> '[1,0,0]' LIMIT 1"
        ),
        "0A000"
    );
}

/// DML の WHERE でも距離比較が受理され、該当行だけを書き換える（値の網羅は `sql74_radius_search.rs`）。
#[test]
fn dml_accepts_distance_predicate() {
    let (core, mut session, _g) = setup("sql74-dml");
    for sql in [
        "UPDATE docs SET n = 9 WHERE embedding <-> '[1,0,0]' < 1 USING OPERATION_ID 'u-1'",
        "DELETE FROM docs WHERE embedding <-> '[0,1,0]' < 1 USING OPERATION_ID 'd-1'",
    ] {
        assert_eq!(code(&core, &mut session, sql), "ok", "{sql}");
    }
    assert_eq!(count(&core, &mut session), 2);
}

/// エラー文言にベクトルリテラルの値を含めない。
#[test]
fn error_detail_does_not_echo_literal() {
    let (core, mut session, _g) = setup("sql74-leak");
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs WHERE embedding <-> '[7.654321,0]' < 1",
        )
        .expect_err("dim mismatch");
    assert!(!format!("{err}").contains("7.654321"));
}

/// 拡張クエリの `<=> $1`（Cosine のみ既存の `$n` パターンが受理）は Execute で評価される。
#[test]
fn extended_query_param_is_evaluated() {
    let (core, mut session, _g) = setup("sql74-ext");
    let prepared = core
        .parse_sql_prepared("SELECT id FROM docs WHERE embedding <=> $1 < 0.5")
        .expect("parse");
    let binds = vec![Some(b"[1,0,0]".to_vec())];
    let bound = core.bind_prepared(&prepared, &binds).expect("bind");
    match core
        .execute_parsed_in_session(&ctx(), &mut session, &bound)
        .expect("execute")
    {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 1),
        _ => panic!("expected Query outcome"),
    }
}
