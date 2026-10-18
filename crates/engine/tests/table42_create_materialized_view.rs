//! `CREATE MATERIALIZED VIEW` の構文と初回実体化（TABLE-42・Issue #1664）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-42・TABLE-36・RLS-10 (b)。
//!
//! `table41_view_or_replace.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。
//!
//! 検証する契約:
//! 1. `WITH DATA` で作った matview を各テナントのセッションで読むと、そのテナントの文脈で本文を
//!    基表へ直接実行した結果と一致する（単純形・列別名・集計・LIMIT 付き評価後射影形）
//! 2. 他テナントの Private 行は実体に混ざらず、タグ用の行数にも他文脈の件数が現れない
//! 3. 実体化対象外の文脈・`WITH NO DATA` は `55000`、注入集合が未設定なら作成者の文脈だけ
//! 4. `IF NOT EXISTS`・名前衝突（`42P07`）、文脈ごとの行数上限（`54000`、失敗時は何も作らない）
//! 5. DDL 権限ゲートが先（`42501`）、明示トランザクション内 DDL は `0A000`
//! 6. 書き込み系は `42809`、列型の確定（型なし・ENUM・別名超過・重複・ICU 照合）の拒否
//! 7. 再オープン後も実体が残る、matview の上の matview、列 DDL の暫定ガード

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

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

fn public_only_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
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

fn create_err(core: &EngineCore, sql: &str) -> String {
    let mut session = allowed_session();
    ddl(core, "alice", &mut session, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
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

fn tag_rows(outcome: &SqlOutcome) -> (bool, bool, Option<u64>) {
    match outcome {
        SqlOutcome::CreateMaterializedView(o) => (o.skipped(), o.populated(), o.own_rows()),
        other => panic!("expected CreateMaterializedView, got {other:?}"),
    }
}

#[test]
fn with_data_matches_direct_execution_for_every_tenant() {
    let (core, _g) = open_core("t42-direct");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_all AS SELECT * FROM docs",
    );
    assert_matches_direct(
        &core,
        "SELECT * FROM mv_all LIMIT 100",
        "SELECT * FROM docs LIMIT 100",
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_ja AS SELECT lang, body FROM docs WHERE lang = 'ja'",
    );
    assert_matches_direct(
        &core,
        "SELECT * FROM mv_ja LIMIT 100",
        "SELECT lang, body FROM docs WHERE lang = 'ja' LIMIT 100",
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_alias (l, b) AS SELECT lang, body FROM docs",
    );
    assert_matches_direct(
        &core,
        "SELECT l, b FROM mv_alias LIMIT 100",
        "SELECT lang, body FROM docs LIMIT 100",
    );
    // 外側の WHERE・ORDER BY・射影・LIMIT は評価後射影形ビューの外側処理に乗る。
    assert_matches_direct(
        &core,
        "SELECT b FROM mv_alias WHERE l = 'en' ORDER BY b DESC LIMIT 10",
        "SELECT body FROM docs WHERE lang = 'en' ORDER BY body DESC LIMIT 10",
    );
}

#[test]
fn buffered_body_forms_are_materialized_per_context() {
    let (core, _g) = open_core("t42-buffered");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_cnt AS SELECT lang, count(*) AS n FROM docs GROUP BY lang",
    );
    assert_matches_direct(
        &core,
        "SELECT lang, n FROM mv_cnt ORDER BY lang LIMIT 10",
        "SELECT lang, count(*) AS n FROM docs GROUP BY lang ORDER BY lang LIMIT 10",
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_lim AS SELECT body FROM docs ORDER BY body LIMIT 2",
    );
    assert_matches_direct(
        &core,
        "SELECT body FROM mv_lim LIMIT 10",
        "SELECT body FROM docs ORDER BY body LIMIT 2",
    );
}

#[test]
fn tenants_do_not_see_each_others_private_rows_or_counts() {
    let (core, _g) = open_core("t42-boundary");
    let outcome = create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_b AS SELECT body FROM docs",
    );
    // alice の文脈: public 4（a1 a2 a3 b1）+ 自分の private 1（a4）= 5 行だけがタグ用の行数。
    let (skipped, populated, own) = tag_rows(&outcome);
    assert!(!skipped && populated);
    assert_eq!(own, Some(5));
    let bob = query(&core, "bob", "SELECT body FROM mv_b LIMIT 100").unwrap();
    let bodies: Vec<String> = bob
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Text(t) => t.clone(),
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    assert!(bodies.contains(&"b2".to_string()) && bodies.contains(&"b3".to_string()));
    assert!(!bodies.contains(&"a4".to_string()), "alice private leaked");
    assert!(!bodies.contains(&"c1".to_string()), "carol private leaked");
}

#[test]
fn creator_row_count_is_zero_when_creator_context_is_not_materialized() {
    let (storage, _p, _g) = open_storage("t42-own-zero");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(vec![ctx("bob")]);
    let outcome = create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_z AS SELECT body FROM docs",
    );
    assert_eq!(tag_rows(&outcome), (false, true, None));
    assert!(query(&core, "bob", "SELECT body FROM mv_z LIMIT 10").is_ok());
    assert_eq!(
        err_code(&core, "alice", "SELECT body FROM mv_z LIMIT 10"),
        "55000"
    );
}

#[test]
fn contexts_outside_the_injected_set_are_not_populated() {
    let (core, _g) = open_core("t42-outside");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_o AS SELECT body FROM docs",
    );
    // 注入集合に無い文脈（Public のみ・未知テナント）は `55000`（固定応答）。
    let public = query_with(
        &core,
        &public_only_ctx("alice"),
        "SELECT body FROM mv_o LIMIT 10",
    )
    .expect_err("public-only context is not materialized");
    assert_eq!(public.wire_code(), "55000");
    assert_eq!(
        err_code(&core, "dave", "SELECT body FROM mv_o LIMIT 10"),
        "55000"
    );
    // 固定文言（名前・件数・他文脈の情報を含まない）。
    assert!(!public.to_string().contains("mv_o"));
    assert_eq!(
        public.client_message(),
        "materialized view has not been populated"
    );
}

#[test]
fn unset_contexts_fall_back_to_creator_only() {
    let (storage, _p, _g) = open_storage("t42-fallback");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let outcome = create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_f AS SELECT body FROM docs",
    );
    assert_eq!(tag_rows(&outcome), (false, true, Some(5)));
    assert!(query(&core, "alice", "SELECT body FROM mv_f LIMIT 10").is_ok());
    assert_eq!(
        err_code(&core, "bob", "SELECT body FROM mv_f LIMIT 10"),
        "55000"
    );
}

#[test]
fn with_no_data_defines_but_is_not_readable() {
    let (core, _g) = open_core("t42-nodata");
    let outcome = create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_n AS SELECT body FROM docs WITH NO DATA;",
    );
    assert_eq!(tag_rows(&outcome), (false, false, None));
    for tenant in ["alice", "bob", "carol"] {
        assert_eq!(
            err_code(&core, tenant, "SELECT body FROM mv_n LIMIT 10"),
            "55000"
        );
    }
    // 定義は存在する（名前の衝突になる）。
    assert_eq!(
        create_err(
            &core,
            "CREATE MATERIALIZED VIEW mv_n AS SELECT body FROM docs"
        ),
        "42P07"
    );
    // `WITH DATA` の明示も同じ既定。
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_d AS SELECT body FROM docs WITH DATA",
    );
    assert!(query(&core, "alice", "SELECT body FROM mv_d LIMIT 10").is_ok());
}

#[test]
fn if_not_exists_is_a_noop_for_any_relation_kind() {
    let (core, _g) = open_core("t42-ine");
    create_ok(&core, "CREATE VIEW v1 AS SELECT body FROM docs");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_i AS SELECT body FROM docs",
    );
    for name in ["docs", "v1", "mv_i"] {
        let outcome = create_ok(
            &core,
            &format!("CREATE MATERIALIZED VIEW IF NOT EXISTS {name} AS SELECT lang FROM docs"),
        );
        assert_eq!(tag_rows(&outcome), (true, false, None), "{name}");
        assert_eq!(
            create_err(
                &core,
                &format!("CREATE MATERIALIZED VIEW {name} AS SELECT lang FROM docs")
            ),
            "42P07",
            "{name}"
        );
    }
    // 既存の matview の列は変わらない（body 列のまま）。
    let r = query(&core, "alice", "SELECT body FROM mv_i LIMIT 1").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn row_limit_is_per_context_and_other_context_failure_is_hidden() {
    let (storage, _p, _g) = open_storage("t42-limit");
    // alice の可視行は 5 行、bob は 6 行、carol は 5 行。上限 5 なら bob だけが超過する。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(5))
        .with_materialization_contexts(all_contexts());
    // 作成者以外の文脈の超過は作成者へ報告されない（成否の差による一致件数の探りを防ぐ）。
    let outcome = create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_l AS SELECT body FROM docs",
    );
    assert_eq!(tag_rows(&outcome), (false, true, Some(5)));
    // 超過した文脈（bob）は実体を持たず、空の正常結果ではなく 55000 で拒否される。
    assert_eq!(
        err_code(&core, "bob", "SELECT body FROM mv_l LIMIT 5"),
        "55000"
    );
    let r = query(&core, "alice", "SELECT body FROM mv_l LIMIT 5").unwrap();
    assert_eq!(r.rows.len(), 5);
    // 作成者自身の文脈が超過するなら従来どおり 54000 で何も作られない。
    let (storage3, _p3, _g3) = open_storage("t42-limit-own");
    let core3 = EngineCore::from_storage(storage3, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(4))
        .with_materialization_contexts(all_contexts());
    assert_eq!(
        create_err(
            &core3,
            "CREATE MATERIALIZED VIEW mv_l AS SELECT body FROM docs"
        ),
        "54000"
    );
    assert_eq!(
        err_code(&core3, "alice", "SELECT body FROM mv_l LIMIT 1"),
        "42P01"
    );
    // 合計では数えない: alice だけを対象にすれば（5 行 = 上限ちょうど）成功する。
    let (storage2, _p2, _g2) = open_storage("t42-limit-ok");
    let core2 = EngineCore::from_storage(storage2, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(5))
        .with_materialization_contexts(vec![ctx("alice"), ctx("carol")]);
    let outcome = create_ok(
        &core2,
        "CREATE MATERIALIZED VIEW mv_l AS SELECT body FROM docs",
    );
    assert_eq!(tag_rows(&outcome), (false, true, Some(5)));
}

#[test]
fn row_limit_at_the_hard_cap_still_creates() {
    let (storage, _p, _g) = open_storage("t42-limit-cap");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(usize::MAX));
    // 上限をハード上限に設定しても作成は成功する（8 行は上限未満。内部の取得上限は +1 で
    // ハード上限を超えないよう 1 つ手前に丸めている）。
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_c AS SELECT body FROM docs",
    );
}

#[test]
fn ddl_permission_gate_comes_first_and_transactions_are_rejected() {
    let (core, _g) = open_core("t42-gate");
    for sql in [
        "CREATE MATERIALIZED VIEW mv_g AS SELECT body FROM docs",
        "CREATE MATERIALIZED VIEW docs AS SELECT body FROM docs",
        "CREATE MATERIALIZED VIEW mv_g AS SELECT body FROM missing_table",
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
            "CREATE MATERIALIZED VIEW mv_g AS SELECT body FROM docs",
        )
        .expect_err("ddl in txn");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn writes_to_a_materialized_view_are_wrong_object_type() {
    let (core, _g) = open_core("t42-writes");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_w AS SELECT lang, body FROM docs",
    );
    for sql in [
        "INSERT INTO mv_w (lang, body) VALUES ('x', 'y') USING OPERATION_ID 'op-mv-1'",
        "UPDATE mv_w SET body = 'z' WHERE lang = 'ja' USING OPERATION_ID 'op-mv-2'",
        "DELETE FROM mv_w WHERE lang = 'ja' USING OPERATION_ID 'op-mv-3'",
        "TRUNCATE TABLE mv_w USING OPERATION_ID 'op-mv-4'",
    ] {
        let mut session = allowed_session();
        let e = ddl(&core, "alice", &mut session, sql).expect_err("must fail");
        assert_eq!(e.wire_code(), "42809", "{sql}");
    }
    // COPY FROM も同じく `42809`。
    let copy = core
        .begin_copy(
            &ctx("alice"),
            &SessionState::default(),
            "COPY mv_w (lang, body) FROM STDIN USING OPERATION_ID 'op-mv-5'",
        )
        .expect_err("COPY into a materialized view must fail");
    assert_eq!(copy.wire_code(), "42809");
    let mut session = allowed_session();
    let e = ddl(
        &core,
        "alice",
        &mut session,
        "ALTER TABLE mv_w ADD COLUMN extra TEXT",
    )
    .expect_err("must fail");
    assert_eq!(e.wire_code(), "42809");
}

#[test]
fn column_derivation_rejects_unsupported_shapes() {
    let (core, _g) = open_core("t42-columns");
    // 別名が出力列より多い。
    assert_eq!(
        create_err(
            &core,
            "CREATE MATERIALIZED VIEW m1 (a, b, c) AS SELECT lang, body FROM docs"
        ),
        "42601"
    );
    // 最終的な列名の重複。
    assert_eq!(
        create_err(
            &core,
            "CREATE MATERIALIZED VIEW m2 (body) AS SELECT lang, body FROM docs"
        ),
        "42701"
    );
    // 構文の拒否。
    for sql in [
        "CREATE OR REPLACE MATERIALIZED VIEW m3 AS SELECT body FROM docs",
        "CREATE TEMP MATERIALIZED VIEW m3 AS SELECT body FROM docs",
        "CREATE MATERIALIZED VIEW m3 WITH (fillfactor = 70) AS SELECT body FROM docs",
        "CREATE MATERIALIZED VIEW m3 TABLESPACE ts AS SELECT body FROM docs",
        "CREATE MATERIALIZED VIEW m3 AS SELECT body FROM docs DATA",
        "CREATE MATERIALIZED VIEW m3 AS SELECT body FROM docs NO DATA",
        "CREATE MATERIALIZED VIEW m3 AS SELECT body FROM docs WITH",
        "CREATE MATERIALIZED VIEW m3 AS",
        "CREATE MATERIALIZED VIEW m3 AS SELECT body FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 3",
    ] {
        assert_eq!(create_err(&core, sql), "42601", "{sql}");
    }
    // 参照先が無い。
    assert_eq!(
        create_err(&core, "CREATE MATERIALIZED VIEW m4 AS SELECT a FROM nope"),
        "42P01"
    );
}

#[test]
fn id_pseudo_column_roundtrips_as_numeric() {
    let (core, _g) = open_core("t42-id");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_id AS SELECT id, body FROM docs",
    );
    assert_matches_direct(
        &core,
        "SELECT id, body FROM mv_id LIMIT 100",
        "SELECT id, body FROM docs LIMIT 100",
    );
}

#[test]
fn definitions_and_entities_survive_reopen() {
    let (storage, path, _g) = open_storage("t42-reopen");
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_materialization_contexts(all_contexts());
        create_ok(
            &core,
            "CREATE MATERIALIZED VIEW mv_r AS SELECT lang, body FROM docs",
        );
        let before = query(&core, "bob", "SELECT lang, body FROM mv_r LIMIT 100").unwrap();
        assert_eq!(before.rows.len(), 6);
    }
    let reopened = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(reopened, Box::new(CpuScalarProvider));
    let after = query(&core, "bob", "SELECT lang, body FROM mv_r LIMIT 100").unwrap();
    assert_eq!(after.rows.len(), 6);
    assert_eq!(
        err_code(&core, "dave", "SELECT lang FROM mv_r LIMIT 1"),
        "55000"
    );
}

#[test]
fn materialized_view_over_materialized_view_reads_inner_entities_per_context() {
    let (core, _g) = open_core("t42-nested");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_in AS SELECT lang, body FROM docs",
    );
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_out AS SELECT body FROM mv_in WHERE lang = 'en'",
    );
    assert_matches_direct(
        &core,
        "SELECT body FROM mv_out LIMIT 100",
        "SELECT body FROM docs WHERE lang = 'en' LIMIT 100",
    );
    // 参照される matview は削除できない（#1663 のガード）ため、基表の列 DDL も拒否される。
}

#[test]
fn column_ddl_on_a_base_table_is_guarded_while_a_matview_depends_on_it() {
    let (core, _g) = open_core("t42-column-ddl");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_cd AS SELECT body FROM docs",
    );
    let cases = [
        ("ALTER TABLE docs DROP COLUMN lang", "2BP01"),
        (
            "ALTER TABLE docs ALTER COLUMN lang TYPE VARCHAR(10)",
            "0A000",
        ),
        ("ALTER TABLE docs RENAME COLUMN lang TO language", "0A000"),
    ];
    for (sql, code) in cases {
        let mut session = allowed_session();
        let e = ddl(&core, "alice", &mut session, sql).expect_err("must fail");
        assert_eq!(e.wire_code(), code, "{sql}");
    }
    // 状態は変わらない。
    assert!(query(&core, "alice", "SELECT lang FROM docs LIMIT 1").is_ok());
    // matview 越しの読み取りも無傷。
    assert_eq!(
        query(&core, "alice", "SELECT body FROM mv_cd LIMIT 100")
            .unwrap()
            .rows
            .len(),
        5
    );
}

#[test]
fn collated_source_columns_are_rejected() {
    let (storage, _p, _g) = open_storage("t42-collation");
    let mut icu = ColumnDef::new("name", ColumnType::Text, false);
    icu.collation = Some(engine::collation::Collation::UndIcu);
    storage
        .create_table(&TableSchema::new("icu_t", vec![icu]))
        .expect("create icu table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        create_err(
            &core,
            "CREATE MATERIALIZED VIEW m_icu AS SELECT name FROM icu_t"
        ),
        "0A000"
    );
}
// --- 列型の往復 ---------------------------------------------------------------------

fn typed_schema() -> TableSchema {
    use engine::catalog::{ArrayElemType, ArrayType};
    TableSchema::new(
        "typed",
        vec![
            ColumnDef::new("c_small", ColumnType::SmallInt, true),
            ColumnDef::new("c_int", ColumnType::Integer, true),
            ColumnDef::new("c_big", ColumnType::BigInt, true),
            ColumnDef::new("c_real", ColumnType::Real, true),
            ColumnDef::new("c_dbl", ColumnType::Double, true),
            ColumnDef::new("c_bool", ColumnType::Boolean, true),
            ColumnDef::new("c_date", ColumnType::Date, true),
            ColumnDef::new("c_ts", ColumnType::Timestamp, true),
            ColumnDef::new(
                "c_num",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("c_json", ColumnType::Jsonb, true),
            ColumnDef::new("c_bytes", ColumnType::Bytea, true),
            ColumnDef::new(
                "c_arr",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
        ],
    )
}

fn typed_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&typed_schema()).expect("create typed");
    for (id, n) in [(1u64, 10i32), (2, 20), (3, 30)] {
        engine::tenant::insert_typed_row(
            &storage,
            "typed",
            &ctx("alice"),
            id,
            Visibility::Private,
            &[
                Value::Integer(n),
                Value::Integer(n * 2),
                Value::BigInt(i64::from(n) * 3),
                Value::Real(n as f32 + 0.5),
                Value::Double(f64::from(n) + 0.25),
                Value::Bool(n % 20 == 0),
                Value::Date(19_000 + n),
                Value::Timestamp(1_000_000 * i64::from(n)),
                Value::Numeric(
                    engine::numeric::Decimal::from_parts(i128::from(n) * 100 + 5, 2).unwrap(),
                ),
                Value::Json("{\"k\":1}".to_string()),
                Value::Bytes(vec![n as u8, 1]),
                Value::Array(engine::row_codec::ArrayValue::Text(vec![
                    Some("x".to_string()),
                    None,
                ])),
            ],
            &OperationId::parse(&format!("typed-{id}")).expect("op id"),
        )
        .expect("insert typed row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(vec![ctx("alice")]);
    (core, guard)
}

fn assert_matches_direct_for(core: &EngineCore, mv_sql: &str, direct_sql: &str) {
    let via_mv = query(core, "alice", mv_sql).unwrap_or_else(|e| panic!("{mv_sql}: {e:?}"));
    let direct = query(core, "alice", direct_sql).unwrap_or_else(|e| panic!("{direct_sql}: {e:?}"));
    assert_eq!(cells(&via_mv), cells(&direct), "{mv_sql} vs {direct_sql}");
}

#[test]
fn scalar_column_types_roundtrip_through_the_entity() {
    let (core, _g) = typed_core("t42-types");
    create_ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_t AS SELECT * FROM typed",
    );
    assert_matches_direct_for(
        &core,
        "SELECT * FROM mv_t LIMIT 100",
        "SELECT * FROM typed LIMIT 100",
    );
}

#[test]
fn aggregate_result_columns_roundtrip_through_the_entity() {
    let (core, _g) = typed_core("t42-agg-types");
    for (i, body) in [
        "SELECT count(*) AS n FROM typed",
        "SELECT sum(c_int) AS s, avg(c_int) AS a, min(c_int) AS lo, max(c_int) AS hi FROM typed",
        "SELECT sum(c_big) AS s, sum(c_dbl) AS d, avg(c_num) AS an, sum(c_num) AS sn FROM typed",
        "SELECT c_bool, count(*) AS n FROM typed GROUP BY c_bool",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("mv_agg{i}");
        create_ok(&core, &format!("CREATE MATERIALIZED VIEW {name} AS {body}"));
        let grouped = body.contains("GROUP BY");
        let mv_sql = if grouped {
            format!("SELECT * FROM {name} ORDER BY c_bool LIMIT 10")
        } else {
            format!("SELECT * FROM {name} LIMIT 10")
        };
        let direct_sql = if grouped {
            format!("{body} ORDER BY c_bool LIMIT 10")
        } else {
            body.to_string()
        };
        assert_matches_direct_for(&core, &mv_sql, &direct_sql);
    }
}
