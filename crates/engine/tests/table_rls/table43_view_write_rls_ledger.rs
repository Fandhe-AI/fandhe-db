//! 更新可能ビューへの書き込み（INSERT／UPDATE／DELETE・UPSERT。TABLE-43・RLS-10。Issue #1674）の
//! テナント境界・可視性・台帳（RECOVER-1／2／3）の総合結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-43。
//!
//! 位置づけ: 書き換え本体の契約は `table43_view_insert.rs`・`table43_view_update_delete.rs`・
//! `table43_view_check_option.rs` が担う。本ファイルはそれらが基本形しか見ていない部分
//! （単一行形・RETURNING・UPSERT の衝突先・可視性の制限・再オープン後の再送・失敗文が台帳に
//! 残らないこと・`LedgerMode`・テナント間の operation_id 名前空間）を補う。
//!
//! 方針: 基表の挙動を推測で固定しない箇所（可視性・UPSERT の衝突先・op 名前空間・
//! `CompareOnlyWithoutLedger`）は、同じ文を基表へ直接送った双子 DB の結果と比べる差分オラクルにする。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::path::Path;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ctx_public(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant")
}

fn op(s: &str) -> OperationId {
    OperationId::parse(s).expect("op id")
}

fn seed(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    // id 1・2 は alice（Public／Private）、id 3 は bob、id 4 は alice の Private 行。
    for (id, tenant, lang, vis) in [
        (1u64, "alice", "ja", Visibility::Public),
        (2, "alice", "en", Visibility::Public),
        (3, "bob", "ja", Visibility::Public),
        (4, "alice", "ja", Visibility::Private),
    ] {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx(tenant),
            id,
            vis,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                Value::Text(format!("body {id}")),
            ],
            &op(&format!("seed-{id}")),
        )
        .expect("seed row");
    }
}

fn core_at(path: &Path, mode: Option<LedgerMode>) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(m) = mode {
        core = core.with_ledger_mode(m);
    }
    core
}

/// 種投入済みの DB を作る。`CleanupGuard` は呼び出し側の `Storage` より先に宣言される。
fn open_seeded(
    label: &str,
    mode: Option<LedgerMode>,
) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        seed(&storage);
    }
    let core = core_at(&path, mode);
    (core, guard, path)
}

fn run_ctx(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(c, &mut session, sql)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    run_ctx(core, &ctx(tenant), sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    run(core, tenant, sql).expect_err(sql)
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    err(core, tenant, sql).wire_code().to_string()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn query_ctx(core: &EngineCore, c: &PolicyContext, sql: &str) -> QueryResult {
    match run_ctx(core, c, sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

/// 基表の `(id, body)` を id 昇順で返す（閲覧テナントと可視性は `c` で決まる）。
fn bodies_ctx(core: &EngineCore, c: &PolicyContext) -> Vec<(u64, String)> {
    let q = query_ctx(core, c, "SELECT id, body FROM docs LIMIT 100");
    let mut v: Vec<(u64, String)> = q
        .rows
        .iter()
        .map(|r| {
            // 書込みで入る body（'a'・'new'・'z' 等）も含め、全 text セルを連結して保持する。
            let body = r
                .cells
                .iter()
                .filter_map(|c| match c {
                    Cell::Text(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("|");
            (r.id, body)
        })
        .collect();
    v.sort();
    v
}

/// bob 自身の行（id 3）だけを返す。Public 行は他テナントからも読めるため、bob の全可視行ではなく
/// 「bob の行が変わっていないか」を見る用途で使う。
fn bob_row(core: &EngineCore) -> Vec<(u64, String)> {
    bodies(core, "bob")
        .into_iter()
        .filter(|(id, _)| *id == 3)
        .collect()
}

fn bodies(core: &EngineCore, tenant: &str) -> Vec<(u64, String)> {
    bodies_ctx(core, &ctx(tenant))
}

/// 影響行数だけを取り出す（INSERT／UPDATE／DELETE／RETURNING のいずれでも）。
fn affected(outcome: &SqlOutcome) -> String {
    match outcome {
        SqlOutcome::Insert(o) => format!("insert:{}", o.rows_affected),
        SqlOutcome::Update(o) => format!("update:{}", o.rows_affected),
        SqlOutcome::Delete(o) => format!("delete:{}", o.rows_affected),
        SqlOutcome::Returning(r) => format!("returning:{}", r.result.rows.len()),
        other => format!("other:{other:?}"),
    }
}

/// 結果の要約（成功なら影響行数、失敗なら wire_code とメッセージ）。差分オラクルの比較用。
fn summary(r: &Result<SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(o) => affected(o),
        Err(e) => format!("err:{}:{e}", e.wire_code()),
    }
}

fn lookup(core: &EngineCore, tenant: &str, table: &str, id: &str) -> LedgerLookup {
    core.operation_recorded(&ctx(tenant), table, &op(id))
        .expect("lookup")
}

const VIEW_JA: &str = "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'";

// ---- テナント境界・RLS ----

/// 単一行形: 他テナントの行（id 3）を狙った UPDATE／DELETE は 0 行で行は変わらず、応答は
/// 存在しない id を狙った場合と同一（存在のオラクルにならない）。
#[test]
fn single_row_form_does_not_leak_other_tenants_rows() {
    let (core, _g, _p) = open_seeded("t43w-single", None);
    ddl(&core, VIEW_JA);
    let before_bob = bodies(&core, "bob");
    for (foreign, missing) in [
        (
            "UPDATE sv SET body = 'new' WHERE id = 3",
            "UPDATE sv SET body = 'new' WHERE id = 999",
        ),
        (
            "DELETE FROM sv WHERE id = 3",
            "DELETE FROM sv WHERE id = 999",
        ),
    ] {
        let a = run(&core, "alice", foreign);
        let b = run(&core, "alice", missing);
        assert_eq!(summary(&a), summary(&b), "{foreign}");
        assert!(summary(&a).ends_with(":0"), "{foreign}");
    }
    assert_eq!(bodies(&core, "bob"), before_bob);
}

/// 述語形 RETURNING: 他テナントの行は返らず変わらない。
#[test]
fn returning_never_exposes_other_tenants_rows() {
    let (core, _g, _p) = open_seeded("t43w-ret", None);
    ddl(&core, VIEW_JA);
    let before_bob = bob_row(&core);
    match ok(
        &core,
        "alice",
        "UPDATE sv SET body = 'new' WHERE true RETURNING *",
    ) {
        SqlOutcome::Returning(r) => {
            // alice の可視な ja 行（id 1・4）だけ。
            let mut got: Vec<u64> = r.result.rows.iter().map(|x| x.id).collect();
            got.sort_unstable();
            assert_eq!(got, vec![1, 4]);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
    match ok(&core, "alice", "DELETE FROM sv WHERE true RETURNING *") {
        SqlOutcome::Returning(r) => {
            assert!(r.result.rows.iter().all(|x| x.id != 3));
            assert_eq!(r.result.rows.len(), 2);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
    assert_eq!(bob_row(&core), before_bob);
}

/// UPSERT の衝突先が他テナントの id でも、他テナントの行に触れず、結果は基表直送と一致する。
#[test]
fn upsert_conflict_target_in_other_tenant_matches_base_table() {
    let (via, _g1, _p1) = open_seeded("t43w-ups-v", None);
    let (direct, _g2, _p2) = open_seeded("t43w-ups-d", None);
    ddl(
        &via,
        "CREATE VIEW sv AS SELECT id, embedding, lang, body FROM docs",
    );
    for conflict in [
        "ON CONFLICT (id) DO NOTHING",
        "ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body",
    ] {
        let tail = format!("VALUES (3, '[9,9]', 'ja', 'new hijack') {conflict} USING OPERATION_ID");
        let r_view = run(
            &via,
            "alice",
            &format!(
                "INSERT INTO sv (id, embedding, lang, body) {tail} 'op-u-{}'",
                conflict.len()
            ),
        );
        let r_direct = run(
            &direct,
            "alice",
            &format!(
                "INSERT INTO docs (id, embedding, lang, body) {tail} 'op-u-{}'",
                conflict.len()
            ),
        );
        assert_eq!(summary(&r_view), summary(&r_direct), "{conflict}");
        assert_eq!(bob_row(&via), bob_row(&direct), "{conflict}");
        assert_eq!(
            bodies(&via, "alice"),
            bodies(&direct, "alice"),
            "{conflict}"
        );
    }
    assert_eq!(bob_row(&via), vec![(3, "body 3".to_string())]);
}

/// 可視性の制限: ビュー作成者の可視性を引き継がない。Public のみの ctx からの書き込みは
/// 同じ ctx で基表へ直送した場合と一致し、Private 行は変わらない。
#[test]
fn view_writes_do_not_inherit_the_creators_visibility() {
    let (via, _g1, _p1) = open_seeded("t43w-vis-v", None);
    let (direct, _g2, _p2) = open_seeded("t43w-vis-d", None);
    ddl(&via, VIEW_JA);
    let narrow = ctx_public("alice");
    for (v, d) in [
        (
            "UPDATE sv SET body = 'new' WHERE true",
            "UPDATE docs SET body = 'new' WHERE lang = 'ja'",
        ),
        (
            "DELETE FROM sv WHERE true",
            "DELETE FROM docs WHERE lang = 'ja'",
        ),
    ] {
        let rv = run_ctx(&via, &narrow, v);
        let rd = run_ctx(&direct, &narrow, d);
        assert_eq!(summary(&rv), summary(&rd), "{v}");
    }
    assert_eq!(bodies(&via, "alice"), bodies(&direct, "alice"));
    // Private 行（id 4）は Public のみの ctx からは変更されない。
    assert!(bodies(&via, "alice").contains(&(4, "body 4".to_string())));
}

/// CHECK OPTION 違反の応答は、他テナントの既存 id・新規 id・UPDATE のいずれでも同一の固定文言で、
/// ビュー名・値・テナントを含まず、テナントを入れ替えても同一。
#[test]
fn check_option_violation_response_is_uniform() {
    let (core, _g, _p) = open_seeded("t43w-chk", None);
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    // 各テナントが自分の行・新規 id に対して違反する文を送る。応答はテナントに依存しない。
    let stmts = [
        (
            "alice",
            "INSERT INTO sv (id, embedding, lang, body) VALUES (77, '[1,1]', 'secret', 'x')",
        ),
        (
            "bob",
            "INSERT INTO sv (id, embedding, lang, body) VALUES (77, '[1,1]', 'secret', 'x')",
        ),
        (
            "alice",
            "INSERT INTO sv (id, embedding, lang, body) VALUES (3, '[1,1]', 'secret', 'x')",
        ),
        ("alice", "UPDATE sv SET lang = 'secret' WHERE id = 1"),
        ("bob", "UPDATE sv SET lang = 'secret' WHERE id = 3"),
    ];
    let mut seen = Vec::new();
    for (tenant, sql) in stmts {
        let e = err(&core, tenant, sql);
        assert_eq!(e.wire_code(), "44000", "{sql}");
        let m = e.to_string();
        assert!(!m.contains("sv") && !m.contains("secret") && !m.contains("alice"));
        seen.push(m);
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    assert_eq!(bob_row(&core), vec![(3, "body 3".to_string())]);
}

/// 拒否の応答（`42703`・`55000`・`0A000`）は閲覧テナントに依存せず、基表の行は変わらない。
#[test]
fn rejections_are_identical_across_tenants_for_update_delete_forms() {
    let (core, _g, _p) = open_seeded("t43w-rej", None);
    ddl(&core, "CREATE VIEW slim AS SELECT id, body FROM docs");
    ddl(&core, "CREATE VIEW lim AS SELECT * FROM docs LIMIT 5");
    let before = (bodies(&core, "alice"), bodies(&core, "bob"));
    for sql in [
        "UPDATE slim SET lang = 'x' WHERE id = 1 RETURNING *",
        "UPDATE slim SET body = 'x' WHERE id = 1 RETURNING lang",
        "DELETE FROM slim WHERE lang = 'ja'",
        "UPDATE lim SET body = 'x' WHERE id = 1",
        "DELETE FROM lim WHERE id = 3 RETURNING *",
    ] {
        let a = err(&core, "alice", sql);
        let b = err(&core, "bob", sql);
        assert_eq!(a.wire_code(), b.wire_code(), "{sql}");
        assert_eq!(a.to_string(), b.to_string(), "{sql}");
        assert!(
            ["42703", "55000", "0A000"].contains(&a.wire_code()),
            "{sql}"
        );
    }
    assert_eq!((bodies(&core, "alice"), bodies(&core, "bob")), before);
}

// ---- 台帳・再送 ----

/// INSERT・UPDATE・DELETE・UPSERT の各 op は、再オープン後も台帳に残り、ビュー経由の再送は
/// `23505`、基表直送・値違いは `22023`（台帳キーは基表名）。
#[test]
fn ledger_survives_reopen_for_every_statement_kind() {
    let (core, g, path) = open_seeded("t43w-reopen", None);
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    let stmts = [
        ("op-r-ins", "INSERT INTO sv (id, embedding, lang, body) VALUES (10, '[1,1]', 'ja', 'a')"),
        ("op-r-upd", "UPDATE sv SET body = 'new' WHERE id = 1"),
        ("op-r-del", "DELETE FROM sv WHERE id = 4"),
        (
            "op-r-ups",
            "INSERT INTO sv (id, embedding, lang, body) VALUES (1, '[1,1]', 'ja', 'z') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body",
        ),
    ];
    for (id, sql) in &stmts {
        ok(&core, "alice", &format!("{sql} USING OPERATION_ID '{id}'"));
    }
    let after_first = bodies(&core, "alice");
    drop(core);

    let core = core_at(&path, None);
    assert_eq!(bodies(&core, "alice"), after_first);
    for (id, sql) in &stmts {
        assert_eq!(
            lookup(&core, "alice", "docs", id),
            LedgerLookup::Recorded,
            "{id}"
        );
        assert_eq!(
            code(&core, "alice", &format!("{sql} USING OPERATION_ID '{id}'")),
            "23505",
            "{id}"
        );
    }
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-r-upd'"
        ),
        "22023"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE sv SET body = 'changed' WHERE id = 1 USING OPERATION_ID 'op-r-upd'"
        ),
        "22023"
    );
    assert_eq!(bodies(&core, "alice"), after_first);
    drop(core);
    drop(g);
}

/// 別ビューの取り違え: v1 で適用した op を同じ値で v2 経由に送ると `22023`（ビュー名ドメイン）。
#[test]
fn same_operation_id_through_a_different_view_is_rejected() {
    let (core, _g, _p) = open_seeded("t43w-views", None);
    ddl(
        &core,
        "CREATE VIEW v1 AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ddl(
        &core,
        "CREATE VIEW v2 AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ok(
        &core,
        "alice",
        "UPDATE v1 SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-x'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE v2 SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-x'"
        ),
        "22023"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE v1 SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-x'"
        ),
        "23505"
    );
}

/// 失敗した文（`44000`・`23505` 以外の拒否・UPSERT 内の違反）は台帳に残らず、直した文を同じ op で
/// 再送すると 1 回だけ適用される。
#[test]
fn failed_statements_leave_no_ledger_entry() {
    let (core, _g, _p) = open_seeded("t43w-fail", None);
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    // UPDATE の違反。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE sv SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-f-1'"
        ),
        "44000"
    );
    assert_eq!(
        lookup(&core, "alice", "docs", "op-f-1"),
        LedgerLookup::NotRecorded
    );
    ok(
        &core,
        "alice",
        "UPDATE sv SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-f-1'",
    );
    assert_eq!(
        lookup(&core, "alice", "docs", "op-f-1"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE sv SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-f-1'"
        ),
        "23505"
    );
    // UPSERT（DO UPDATE）の違反。
    let bad = "INSERT INTO sv (id, embedding, lang, body) VALUES (1, '[1,1]', 'en', 'z') ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang";
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("{bad} USING OPERATION_ID 'op-f-2'")
        ),
        "44000"
    );
    assert_eq!(
        lookup(&core, "alice", "docs", "op-f-2"),
        LedgerLookup::NotRecorded
    );
    // 非公開列の指定（`42703`）も台帳に残らない。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE sv SET hidden = 'x' WHERE id = 1 USING OPERATION_ID 'op-f-3'"
        ),
        "42703"
    );
    assert_eq!(
        lookup(&core, "alice", "docs", "op-f-3"),
        LedgerLookup::NotRecorded
    );
    assert_eq!(bodies(&core, "alice")[0], (1, "new".to_string()));
}

/// 明示トランザクションの ROLLBACK 後は op が未記録で、同じ op で再送すると適用される。
#[test]
fn rollback_leaves_no_ledger_entry() {
    let (core, _g, _p) = open_seeded("t43w-rb", None);
    ddl(&core, VIEW_JA);
    let caller = ctx("alice");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO sv (id, embedding, lang, body) VALUES (20, '[1,1]', 'ja', 'a') USING OPERATION_ID 'op-b-1'",
        "UPDATE sv SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-b-2'",
        "ROLLBACK",
    ] {
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for id in ["op-b-1", "op-b-2"] {
        assert_eq!(
            lookup(&core, "alice", "docs", id),
            LedgerLookup::NotRecorded,
            "{id}"
        );
    }
    assert_eq!(bodies(&core, "alice")[0], (1, "body 1".to_string()));
    ok(
        &core,
        "alice",
        "UPDATE sv SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-b-2'",
    );
    assert_eq!(bodies(&core, "alice")[0], (1, "new".to_string()));
}

/// `Ledgered`: op を省略したビュー経由の書き込みは基表と同じ `23502` で何も書かれない。
/// `CompareOnlyWithoutLedger`: 同一 op の再送の挙動は基表直送と一致する。
#[test]
fn ledger_modes_apply_through_views_like_base_tables() {
    let (core, _g, _p) = open_seeded("t43w-led", Some(LedgerMode::Ledgered));
    ddl(&core, VIEW_JA);
    let before = bodies(&core, "alice");
    for (v, d) in [
        (
            "INSERT INTO sv (id, embedding, lang, body) VALUES (30, '[1,1]', 'ja', 'a')",
            "INSERT INTO docs (id, embedding, lang, body) VALUES (30, '[1,1]', 'ja', 'a')",
        ),
        (
            "UPDATE sv SET body = 'new' WHERE id = 1",
            "UPDATE docs SET body = 'new' WHERE id = 1",
        ),
        (
            "DELETE FROM sv WHERE id = 1",
            "DELETE FROM docs WHERE id = 1",
        ),
    ] {
        let ev = err(&core, "alice", v);
        let ed = err(&core, "alice", d);
        assert_eq!(ev.wire_code(), "23502", "{v}");
        assert_eq!(ev.wire_code(), ed.wire_code(), "{v}");
    }
    assert_eq!(bodies(&core, "alice"), before);

    let (via, _g1, _p1) = open_seeded("t43w-cmp-v", Some(LedgerMode::CompareOnlyWithoutLedger));
    let (direct, _g2, _p2) = open_seeded("t43w-cmp-d", Some(LedgerMode::CompareOnlyWithoutLedger));
    ddl(&via, VIEW_JA);
    for _ in 0..2 {
        let rv = run(
            &via,
            "alice",
            "UPDATE sv SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-c-1'",
        );
        let rd = run(
            &direct,
            "alice",
            "UPDATE docs SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-c-1'",
        );
        assert_eq!(summary(&rv), summary(&rd));
    }
    assert_eq!(bodies(&via, "alice"), bodies(&direct, "alice"));
}

/// テナント間の op 名前空間: alice が使った op を bob が使った結果は基表直送と一致し、
/// alice の台帳エントリは変わらない。
#[test]
fn operation_id_namespace_across_tenants_matches_base_table() {
    let (via, _g1, _p1) = open_seeded("t43w-ns-v", None);
    let (direct, _g2, _p2) = open_seeded("t43w-ns-d", None);
    ddl(&via, VIEW_JA);
    ok(
        &via,
        "alice",
        "UPDATE sv SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-n-1'",
    );
    ok(
        &direct,
        "alice",
        "UPDATE docs SET body = 'new' WHERE id = 1 USING OPERATION_ID 'op-n-1'",
    );
    let rv = run(
        &via,
        "bob",
        "UPDATE sv SET body = 'new' WHERE id = 3 USING OPERATION_ID 'op-n-1'",
    );
    let rd = run(
        &direct,
        "bob",
        "UPDATE docs SET body = 'new' WHERE id = 3 USING OPERATION_ID 'op-n-1'",
    );
    // 影響行数（Ok 時）と wire_code（Err 時）まで基表直送と等価比較する（メッセージ文は除く）。
    let key = |r: &Result<SqlOutcome, SqlSurfaceError>| match r {
        Ok(o) => format!("ok:{}", affected(o)),
        Err(e) => format!("err:{}", e.wire_code()),
    };
    assert_eq!(key(&rv), key(&rd), "{} / {}", summary(&rv), summary(&rd));
    assert_eq!(
        lookup(&via, "alice", "docs", "op-n-1"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        lookup(&via, "alice", "docs", "op-n-1"),
        lookup(&direct, "alice", "docs", "op-n-1")
    );
    assert_eq!(bodies(&via, "alice")[0], (1, "new".to_string()));
}
