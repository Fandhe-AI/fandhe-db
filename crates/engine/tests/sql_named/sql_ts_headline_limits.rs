//! `ts_headline` の出力バイト長・断片数上限の起動時設定（`EngineCore::with_headline_limits`。
//! SEARCH-14 ポインタ・Issue #2125）の結合テスト。
//!
//! 上限を下げたコアで、(1) 出力が上限を超えたら切り詰めず `54000`・ちょうどなら成功、
//! (2) `MaxFragments` が上限を超えたら `54000`、(3) 簡易クエリ・拡張クエリ（Parse/Bind 相当）の
//! どちらでも下げた上限が効く、(4) 既定のコアはハード上限で動く、ことを固定する。

use engine::core::EngineCore;
use engine::headline_limits::HeadlineLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_with(label: &str, limits: Option<HeadlineLimits>) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_headline_limits(l),
        None => core,
    };
    (guard, core)
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
}

fn text_of(out: SqlOutcome) -> String {
    let SqlOutcome::Query(q) = out else {
        panic!("query expected");
    };
    match q.rows.first().and_then(|r| r.cells.first()) {
        Some(Cell::Text(s)) => s.clone(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

/// 拡張クエリ経路（Parse → Bind → Execute）で `$1`（本文）を束縛して UPDATE で評価する。
fn prepared(core: &EngineCore, sql: &str, value: &str) -> Result<(), SqlSurfaceError> {
    let p = core.parse_sql_prepared(sql)?;
    let parsed = core.bind_prepared(&p, &[Some(value.as_bytes().to_vec())])?;
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx(), &mut s, &parsed)
        .map(|_| ())
}

/// 本文と、既定の強調付き出力（12 バイト）。
const DOC: &str = "a fox";
const OUT: &str = "a <b>fox</b>";

/// AC2: 出力が上限を超えたら切り詰めず `54000`。上限ちょうどは成功する。
#[test]
fn output_over_limit_is_rejected_without_truncation() {
    let (_g, core) = open_with("tshl-bytes", Some(HeadlineLimits::new(OUT.len(), 8)));
    let sql = format!("SELECT ts_headline('{DOC}', 'fox')");
    assert_eq!(text_of(exec(&core, &sql).expect("exact limit")), OUT);
    let (_g2, low) = open_with(
        "tshl-bytes-low",
        Some(HeadlineLimits::new(OUT.len() - 1, 8)),
    );
    assert_eq!(code(&low, &sql), "54000");
    // 区切りを伸ばして超える場合も同じ。
    let sql = format!("SELECT ts_headline('{DOC}', 'fox', 'StartSel=<<<<<<<<<<<<<<<<<<<<')");
    assert_eq!(code(&core, &sql), "54000");
}

/// `MaxFragments` が上限を超えたら `54000`（上限以下は成功）。
#[test]
fn max_fragments_over_limit_is_rejected() {
    let (_g, core) = open_with("tshl-frag", Some(HeadlineLimits::new(1 << 20, 2)));
    let ok_sql =
        "SELECT ts_headline('a fox b fox', 'fox', 'MaxFragments=2, MaxWords=3, MinWords=1')";
    exec(&core, ok_sql).expect("within limit");
    let bad = "SELECT ts_headline('a fox', 'fox', 'MaxFragments=3, MaxWords=3, MinWords=1')";
    assert_eq!(code(&core, bad), "54000");
}

/// 拡張クエリ（Parse/Bind）でも、下げた上限が束縛時に焼き込まれて効く。
#[test]
fn lowered_limit_applies_to_prepared_statements() {
    let sql = "UPDATE docs SET body = ts_headline($1, 'fox') WHERE id = 1 USING OPERATION_ID 'o1'";
    let setup = |core: &EngineCore| {
        exec(core, "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)").expect("ddl");
        exec(
            core,
            &format!("INSERT INTO docs (id, body) VALUES (1, '{DOC}')"),
        )
        .expect("insert");
    };
    let (_g, low) = open_with("tshl-prep-low", Some(HeadlineLimits::new(OUT.len() - 1, 8)));
    setup(&low);
    let err = prepared(&low, sql, DOC).expect_err("over limit");
    assert_eq!(err.wire_code(), "54000", "{err}");
    let (_g2, ok) = open_with("tshl-prep-ok", Some(HeadlineLimits::new(OUT.len(), 8)));
    setup(&ok);
    prepared(&ok, sql, DOC).expect("exact");
    let SqlOutcome::Query(q) = exec(&ok, "SELECT body FROM docs WHERE id = 1").expect("select")
    else {
        panic!("query expected");
    };
    assert_eq!(
        q.rows.first().and_then(|r| r.cells.first()),
        Some(&Cell::Text(OUT.to_string()))
    );
}

/// 既定のコアはハード上限で動く（大きな本文もそのまま強調できる）。
#[test]
fn default_core_uses_hard_limits() {
    let (_g, core) = open_with("tshl-default", None);
    assert_eq!(core.headline_limits(), HeadlineLimits::default());
    let doc = format!("{} fox", "word ".repeat(2000));
    let sql = format!("SELECT ts_headline('{doc}', 'fox', 'HighlightAll=true')");
    let out = text_of(exec(&core, &sql).expect("hard limit"));
    assert!(out.ends_with("<b>fox</b>"));
    assert_eq!(out.len(), doc.len() + "<b></b>".len());
}
