//! `REFRESH MATERIALIZED VIEW` の結合テスト（TABLE-42 (1)(3)(4)・Issue #1665）。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-42。
//!
//! `table42_create_materialized_view.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。
//!
//! 検証する契約:
//! 1. REFRESH は全文脈の実体を同じスナップショットから作り直し、他テナントの行・件数を混ぜない
//! 2. 失敗（実行者の文脈の行数超過・書き込みロック待ち超過・列形状のずれ）では旧実体が残る
//! 3. 他の文脈の失敗は実行者へ返さず、その文脈は `55000`（fail-closed）になる
//! 4. `WITH NO DATA` は未実体化（`55000`）にし、以後の REFRESH で再実体化できる
//! 5. 権限ゲートが先（`42501`）、明示トランザクション内は `0A000`、種別違いは `42809`
//! 6. `CONCURRENTLY` は TABLE-42 (4) の前提検査（未実体化 `0A000`／適格な UNIQUE 索引なし `55000`）を
//!    通れば非 CONCURRENTLY と同結果（Issue #1666）、構文違反は `42601`、再オープン後も新実体

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
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

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn insert_row(
    storage: &Storage,
    tenant: &str,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice: public 3（ja 2・en 1）・private 1（ja）。bob: public 1（ja）・private 2（ja・en）。
/// carol: private 1（ja）のみ。
fn seed(storage: &Storage) {
    insert_row(storage, "alice", 1, "ja", "a1", Visibility::Public);
    insert_row(storage, "alice", 2, "ja", "a2", Visibility::Public);
    insert_row(storage, "alice", 3, "en", "a3", Visibility::Public);
    insert_row(storage, "alice", 4, "ja", "a4", Visibility::Private);
    insert_row(storage, "bob", 5, "ja", "b1", Visibility::Public);
    insert_row(storage, "bob", 6, "ja", "b2", Visibility::Private);
    insert_row(storage, "bob", 7, "en", "b3", Visibility::Private);
    insert_row(storage, "carol", 8, "ja", "c1", Visibility::Private);
}

fn open_storage(label: &str) -> (Storage, std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage);
    (storage, path, guard)
}

fn all_contexts() -> Vec<PolicyContext> {
    vec![ctx("alice"), ctx("bob"), ctx("carol")]
}

/// 実体化対象を alice・bob・carol に注入した core。
fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let (storage, _path, guard) = open_storage(label);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(all_contexts());
    (core, guard)
}

fn ddl(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn create_ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    let mut session = allowed_session();
    ddl(core, "alice", &mut session, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn query_with(
    core: &EngineCore,
    context: &PolicyContext,
    sql: &str,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(context, &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    query_with(core, &ctx(tenant), sql)
}

fn err_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query(core, tenant, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

/// セル値の正規形。実体は列型ごとに保存するため、疑似列 `id`（`Cell::Integer`）は numeric、
/// `count(*)` は `SignedInteger` で戻る。wire 上のテキストは同じなので、整数値は 1 つの正規形へ寄せる。
fn canon(cell: &Cell) -> String {
    match cell {
        Cell::Integer(u) => format!("int:{u}"),
        Cell::SignedInteger(i) => format!("int:{i}"),
        Cell::Numeric(d) if d.scale() == 0 => format!("int:{}", d.unscaled()),
        other => format!("{other:?}"),
    }
}

fn cells(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(canon).collect())
        .collect()
}

/// 各テナントで `mv_sql`（matview の読み取り）と `direct_sql`（本文の直接実行）の結果が一致する。
fn assert_matches_direct(core: &EngineCore, mv_sql: &str, direct_sql: &str) {
    for tenant in ["alice", "bob", "carol"] {
        let via_mv = query(core, tenant, mv_sql).unwrap_or_else(|e| panic!("{mv_sql}: {e:?}"));
        let direct =
            query(core, tenant, direct_sql).unwrap_or_else(|e| panic!("{direct_sql}: {e:?}"));
        assert_eq!(
            cells(&via_mv),
            cells(&direct),
            "tenant {tenant}: {mv_sql} vs {direct_sql}"
        );
    }
}

fn refresh(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = allowed_session();
    ddl(core, tenant, &mut session, sql)
}

fn refresh_ok(core: &EngineCore, tenant: &str, sql: &str) -> bool {
    match refresh(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::RefreshMaterializedView(o) => o.populated(),
        other => panic!("expected RefreshMaterializedView, got {other:?}"),
    }
}

fn refresh_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    refresh(core, tenant, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

fn exec_ok(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = allowed_session();
    ddl(core, tenant, &mut session, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn insert_sql(tenant: &str, id: u64, body: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang, body) VALUES ({id}, '[1.0, 0.0]', 'ja', '{body}') \
         USING OPERATION_ID 'op-{tenant}-{id}'"
    )
}

fn bodies(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = cells(&query(core, tenant, sql).unwrap_or_else(|e| panic!("{e:?}")))
        .into_iter()
        .map(|r| r.join(","))
        .collect();
    v.sort();
    v
}

fn has(core: &EngineCore, tenant: &str, sql: &str, needle: &str) -> bool {
    bodies(core, tenant, sql).iter().any(|b| b.contains(needle))
}

/// TABLE-42 (3): 基表の変更は REFRESH までは見えず、REFRESH 後は全テナントで同時点の直接実行と一致する。
#[test]
fn refresh_reflects_base_changes_for_every_tenant() {
    let (core, _g) = open_core("t42r-reflect");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    exec_ok(&core, "alice", &insert_sql("alice", 100, "a-new"));
    exec_ok(&core, "bob", &insert_sql("bob", 101, "b-new"));
    let q = "SELECT body FROM mv LIMIT 100";
    // REFRESH 前は旧実体。
    assert!(!has(&core, "alice", q, "a-new"));
    assert!(refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv"));
    assert_matches_direct(&core, q, "SELECT body FROM docs LIMIT 100");
    assert!(has(&core, "alice", q, "a-new"));
    // bob の新しい行は bob にだけ見える（alice の REFRESH でも文脈は混ざらない）。
    assert!(has(&core, "bob", q, "b-new"));
    assert!(!has(&core, "alice", q, "b-new"));
}

/// 応答は件数を持たない（他文脈の件数を運べない）。持つのは `populated` だけ。
#[test]
fn refresh_outcome_carries_only_populated() {
    let (core, _g) = open_core("t42r-outcome");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    let out = refresh(&core, "alice", "REFRESH MATERIALIZED VIEW mv WITH DATA;").unwrap();
    assert!(matches!(&out, SqlOutcome::RefreshMaterializedView(o) if o.populated()));
}

/// 受入基準: 実行者の文脈が行数上限を超えたら 54000 で、全テナントの旧実体が残る。
#[test]
fn refresher_row_limit_overflow_is_54000_and_keeps_old_entities() {
    let (storage, _p, _g) = open_storage("t42r-limit-own");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(5))
        .with_materialization_contexts(all_contexts());
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    let q = "SELECT body FROM mv LIMIT 5";
    let before_alice = bodies(&core, "alice", q);
    let before_carol = bodies(&core, "carol", q);
    // alice の可視行を 5 -> 6 に増やす（上限 5 を超過）。
    exec_ok(&core, "alice", &insert_sql("alice", 100, "a-new"));
    assert_eq!(
        refresh_code(&core, "alice", "REFRESH MATERIALIZED VIEW mv"),
        "54000"
    );
    assert_eq!(bodies(&core, "alice", q), before_alice);
    assert_eq!(bodies(&core, "carol", q), before_carol);
}

/// 他の文脈の失敗は実行者へ返さず、その文脈の読み取りは 55000（fail-closed）になる。
#[test]
fn other_context_failure_is_hidden_and_that_context_becomes_unpopulated() {
    let (storage, _p, _g) = open_storage("t42r-limit-other");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(6))
        .with_materialization_contexts(all_contexts());
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    let q = "SELECT body FROM mv LIMIT 6";
    assert_eq!(bodies(&core, "bob", q).len(), 6);
    // bob の可視行を 6 -> 7（上限 6 を超過）、alice は 5 -> 6（上限ちょうど）。
    exec_ok(&core, "bob", &insert_sql("bob", 100, "b-new"));
    exec_ok(&core, "alice", &insert_sql("alice", 101, "a-new"));
    assert!(refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv"));
    assert!(has(&core, "alice", q, "a-new"));
    assert_eq!(err_code(&core, "bob", q), "55000");
    // bob 自身の REFRESH は 54000 で、bob の状態は変わらない。
    assert_eq!(
        refresh_code(&core, "bob", "REFRESH MATERIALIZED VIEW mv"),
        "54000"
    );
    assert_eq!(err_code(&core, "bob", q), "55000");
}

/// 他文脈の行符号化失敗（256 バイトのテナント ID は row_codec の長さ上限を超える）も実行者へ返さない。
/// 長い ID の文脈の結果が空でも非空でも、実行者の REFRESH は同一（成功）で、その文脈は未実体化（55000）。
#[test]
fn other_context_encode_failure_is_hidden_regardless_of_emptiness() {
    let long_tenant = "t".repeat(256);
    for (label, seed_long) in [("t42r-enc-empty", false), ("t42r-enc-rows", true)] {
        let (storage, _p, _g) = open_storage(label);
        if seed_long {
            insert_row(
                &storage,
                &long_tenant,
                50,
                "ja",
                "long1",
                Visibility::Private,
            );
        }
        let mut contexts = all_contexts();
        contexts.push(ctx(&long_tenant));
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_materialization_contexts(contexts);
        create_ok(
            &core,
            "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs WITH NO DATA",
        );
        assert!(refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv"));
        assert!(has(&core, "alice", "SELECT body FROM mv LIMIT 6", "a1"));
        assert!(!has(&core, "alice", "SELECT body FROM mv LIMIT 6", "long1"));
        assert_eq!(
            err_code(&core, &long_tenant, "SELECT body FROM mv LIMIT 6"),
            "55000"
        );
    }
}

/// TABLE-42 (1): WITH NO DATA で未実体化（55000）になり、続く REFRESH で再実体化できる。
/// CREATE ... WITH NO DATA の matview も REFRESH で実体化できる。
#[test]
fn with_no_data_makes_reads_55000_and_refresh_repopulates() {
    let (core, _g) = open_core("t42r-nodata");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    assert!(!refresh_ok(
        &core,
        "alice",
        "REFRESH MATERIALIZED VIEW mv WITH NO DATA"
    ));
    for t in ["alice", "bob", "carol"] {
        assert_eq!(err_code(&core, t, "SELECT body FROM mv LIMIT 5"), "55000");
    }
    assert!(refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv"));
    assert_matches_direct(
        &core,
        "SELECT body FROM mv LIMIT 100",
        "SELECT body FROM docs LIMIT 100",
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv2 AS SELECT body FROM docs WITH NO DATA",
    );
    assert_eq!(
        err_code(&core, "alice", "SELECT body FROM mv2 LIMIT 5"),
        "55000"
    );
    assert!(refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv2"));
    assert_matches_direct(
        &core,
        "SELECT body FROM mv2 LIMIT 100",
        "SELECT body FROM docs LIMIT 100",
    );
}

#[test]
fn gate_order_and_transaction_rejection() {
    let (core, _g) = open_core("t42r-gate");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    for sql in [
        "REFRESH MATERIALIZED VIEW mv",
        "REFRESH MATERIALIZED VIEW missing_mv",
    ] {
        let mut session = SessionState::default();
        let e = ddl(&core, "alice", &mut session, sql).expect_err("denied");
        assert_eq!(e.wire_code(), "42501", "{sql}");
    }
    let caller = ctx("alice");
    let mut session = allowed_session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "REFRESH MATERIALIZED VIEW mv",
        )
        .expect_err("refresh in txn");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn refresh_on_wrong_object_kinds() {
    let (core, _g) = open_core("t42r-kinds");
    create_ok(&core, "CREATE VIEW v AS SELECT body FROM docs");
    create_ok(&core, "CREATE INDEX idx_lang ON docs (lang)");
    assert_eq!(
        refresh_code(&core, "alice", "REFRESH MATERIALIZED VIEW nope"),
        "42P01"
    );
    for name in ["docs", "v", "idx_lang"] {
        assert_eq!(
            refresh_code(&core, "alice", &format!("REFRESH MATERIALIZED VIEW {name}")),
            "42809",
            "{name}"
        );
    }
}

/// CONCURRENTLY の前提（TABLE-42 (4)・Issue #1666）: 適格な UNIQUE 索引が無ければ 55000 で、旧実体は残る。
#[test]
fn concurrently_without_unique_index_is_55000_and_keeps_old_entity() {
    let (core, _g) = open_core("t42r-conc-noidx");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    let q = "SELECT body FROM mv LIMIT 100";
    exec_ok(&core, "alice", &insert_sql("alice", 100, "a-new"));
    for round in 0..2 {
        assert_eq!(
            refresh_code(&core, "alice", "REFRESH MATERIALIZED VIEW CONCURRENTLY mv"),
            "55000"
        );
        assert_eq!(bodies(&core, "alice", q).len(), 5);
        assert!(!has(&core, "alice", q, "a-new"));
        // 非 UNIQUE 索引だけでは前提を満たさない。
        if round == 0 {
            exec_ok(&core, "alice", "CREATE INDEX mv_body_idx ON mv (body)");
        }
    }
}

/// 未実体化は索引の有無より先に 0A000。読み取りは 55000 のまま。
#[test]
fn concurrently_on_unpopulated_is_0a000() {
    let (core, _g) = open_core("t42r-conc-unpop");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv0 AS SELECT body FROM docs WITH NO DATA",
    );
    let sql = "REFRESH MATERIALIZED VIEW CONCURRENTLY mv0";
    assert_eq!(refresh_code(&core, "alice", sql), "0A000");
    exec_ok(&core, "alice", "CREATE UNIQUE INDEX mv0_body ON mv0 (body)");
    assert_eq!(refresh_code(&core, "alice", sql), "0A000");
    assert_eq!(
        err_code(&core, "alice", "SELECT body FROM mv0 LIMIT 5"),
        "55000"
    );
}

/// 前提を満たせば非 CONCURRENTLY と同じ結果になる。
#[test]
fn concurrently_with_unique_index_refreshes_like_plain() {
    let (core, _g) = open_core("t42r-conc-ok");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    exec_ok(&core, "alice", "CREATE UNIQUE INDEX mv_body ON mv (body)");
    exec_ok(&core, "alice", &insert_sql("alice", 100, "a-new"));
    exec_ok(&core, "bob", &insert_sql("bob", 101, "b-new"));
    let q = "SELECT body FROM mv LIMIT 100";
    assert!(refresh_ok(
        &core,
        "alice",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv"
    ));
    assert_matches_direct(&core, q, "SELECT body FROM docs LIMIT 100");
    assert!(has(&core, "alice", q, "a-new"));
    assert!(has(&core, "bob", q, "b-new"));
    assert!(!has(&core, "alice", q, "b-new"));
    assert!(refresh_ok(
        &core,
        "alice",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv WITH DATA"
    ));
}

/// 実行者の文脈に重複があれば 23505 で旧実体が残る（UNIQUE の強制は非 CONCURRENTLY と同一）。
#[test]
fn concurrently_duplicate_in_refresher_context_is_23505_and_keeps_old_entity() {
    let (core, _g) = open_core("t42r-conc-dup");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    exec_ok(&core, "alice", "CREATE UNIQUE INDEX mv_body ON mv (body)");
    exec_ok(&core, "alice", &insert_sql("alice", 100, "a1"));
    assert_eq!(
        refresh_code(&core, "alice", "REFRESH MATERIALIZED VIEW CONCURRENTLY mv"),
        "23505"
    );
    assert_eq!(
        bodies(&core, "alice", "SELECT body FROM mv LIMIT 100").len(),
        5
    );
}

/// 他文脈だけの重複は実行者へ返さず、その文脈は未実体化（55000）になる。
#[test]
fn concurrently_duplicate_only_in_other_context_is_hidden() {
    let (core, _g) = open_core("t42r-conc-dup-other");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    exec_ok(&core, "alice", "CREATE UNIQUE INDEX mv_body ON mv (body)");
    exec_ok(&core, "carol", &insert_sql("carol", 100, "c1"));
    let q = "SELECT body FROM mv LIMIT 100";
    assert!(refresh_ok(
        &core,
        "alice",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv"
    ));
    assert_eq!(err_code(&core, "carol", q), "55000");
    assert_eq!(bodies(&core, "alice", q).len(), 5);
}

/// 種別違い・不在・権限ゲートは CONCURRENTLY でも前提検査より前に効く。
#[test]
fn concurrently_keeps_earlier_gates() {
    let (core, _g) = open_core("t42r-conc-gates");
    create_ok(&core, "CREATE VIEW v AS SELECT body FROM docs");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    assert_eq!(
        refresh_code(
            &core,
            "alice",
            "REFRESH MATERIALIZED VIEW CONCURRENTLY nope"
        ),
        "42P01"
    );
    for name in ["docs", "v"] {
        assert_eq!(
            refresh_code(
                &core,
                "alice",
                &format!("REFRESH MATERIALIZED VIEW CONCURRENTLY {name}")
            ),
            "42809",
            "{name}"
        );
    }
    let mut session = SessionState::default();
    let e = ddl(
        &core,
        "alice",
        &mut session,
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv",
    )
    .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
}
#[test]
fn syntax_errors_are_42601() {
    let (core, _g) = open_core("t42r-syntax");
    for sql in [
        "REFRESH VIEW mv",
        "REFRESH MATERIALIZED VIEW",
        "REFRESH MATERIALIZED VIEW mv WITH DATA extra",
        "REFRESH MATERIALIZED VIEW mv WITH",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv WITH NO DATA",
    ] {
        assert_eq!(refresh_code(&core, "alice", sql), "42601", "{sql}");
    }
}

/// `SELECT *` の matview は基表への ADD COLUMN で出力列がずれる。0A000 で拒否し旧実体を残す。
#[test]
fn source_column_drift_is_0a000_and_keeps_old_entity() {
    let (core, _g) = open_core("t42r-drift");
    create_ok(&core, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM docs");
    let q = "SELECT body FROM mv LIMIT 100";
    let before = bodies(&core, "alice", q);
    exec_ok(&core, "alice", "ALTER TABLE docs ADD COLUMN extra TEXT");
    assert_eq!(
        refresh_code(&core, "alice", "REFRESH MATERIALIZED VIEW mv"),
        "0A000"
    );
    assert_eq!(bodies(&core, "alice", q), before);
}

/// 内側の matview を更新せずに外側を REFRESH すると、内側の現在の実体が反映される（CASCADE なし）。
#[test]
fn nested_matview_refresh_reads_inner_current_entity_without_cascade() {
    let (core, _g) = open_core("t42r-nested");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_in AS SELECT body FROM docs",
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_out AS SELECT body FROM mv_in",
    );
    let q = "SELECT body FROM mv_out LIMIT 100";
    exec_ok(&core, "alice", &insert_sql("alice", 100, "a-new"));
    // 内側が古いままの外側 REFRESH は新しい行を取り込まない。
    refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv_out");
    assert!(!has(&core, "alice", q, "a-new"));
    // 内側を REFRESH しても外側は変わらない。
    refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv_in");
    assert!(!has(&core, "alice", q, "a-new"));
    refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv_out");
    assert!(has(&core, "alice", q, "a-new"));
}

#[test]
fn refresh_survives_reopen() {
    let (storage, path, _g) = open_storage("t42r-reopen");
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_materialization_contexts(all_contexts());
        create_ok(
            &core,
            "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
        );
        exec_ok(&core, "bob", &insert_sql("bob", 100, "b-new"));
        refresh_ok(&core, "alice", "REFRESH MATERIALIZED VIEW mv");
    }
    let reopened = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(reopened, Box::new(CpuScalarProvider));
    assert!(has(&core, "bob", "SELECT body FROM mv LIMIT 100", "b-new"));
}

/// 書き込みゲートを別セッションが保持している間の REFRESH は 55P03 で、旧実体は読め、
/// 読み取りはブロックされない。
#[test]
fn writer_gate_contention_is_55p03_and_keeps_old_entity() {
    let path = unique_db_path("t42r-lock");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path)
        .expect("open")
        .with_write_lock_wait(std::time::Duration::from_millis(200));
    storage.create_table(&schema()).expect("table");
    seed(&storage);
    let core = std::sync::Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_materialization_contexts(all_contexts()),
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    let q = "SELECT body FROM mv LIMIT 100";
    let before = bodies(&core, "alice", q);

    let (began_tx, began_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder_core = std::sync::Arc::clone(&core);
    let holder = std::thread::spawn(move || {
        let caller = ctx("bob");
        let mut session = SessionState::default();
        let mut txn = holder_core.new_session_transaction();
        holder_core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        // writer は最初の書き込み文で取得される（Issue #2400）ため、書き込みを 1 文入れて握る。
        holder_core
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "INSERT INTO docs (id, embedding, lang, body) VALUES (900, '[1.0,0.0]', 'ja', 'h1') \
                 USING OPERATION_ID 'op-t42-holder'",
            )
            .expect("first write holds writer");
        began_tx.send(()).expect("notify");
        release_rx.recv().expect("release");
        holder_core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    });
    began_rx.recv().expect("began");
    assert_eq!(
        refresh_code(&core, "alice", "REFRESH MATERIALIZED VIEW mv"),
        "55P03"
    );
    assert_eq!(bodies(&core, "alice", q), before);
    release_tx.send(()).expect("release");
    holder.join().expect("join");
}
