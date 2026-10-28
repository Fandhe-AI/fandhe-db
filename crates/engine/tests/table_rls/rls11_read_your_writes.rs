//! RLS-11（TASK-195。read-your-writes）の engine API 結合テスト。
//!
//! Issue #973（PR #977）で wire-server の認証導出点（`auth.rs::
//! session_policy_context`）が返す `PolicyContext` の許可可視性を「`Public`
//! のみ」から「`Public` ＋ 自テナントの `Private`」へ切り替えた。本ファイルは
//! その契約を engine API（wire を経由しない `EngineCore` 直接呼び出し）の
//! 側から 3 テナント × {同一セッション, 同一テナント別セッション, 他テナント}
//! の行列として固定する。
//!
//! `session_ctx` は wire-server の認証導出点が返す形（`PolicyContext::
//! with_visibilities(tenant, [Public, Private])`）を模したテスト側ヘルパー
//! であり、engine の既定（[`PolicyContext::new`]。`Public` のみ）ではない
//! ことに注意する。両者の違いそのものも
//! [`engine_default_policy_context_still_hides_own_private_rows`] で固定する
//! （#973「engine 既定は無変更」の機械検証）。
//!
//! 世代整合キャッシュ（`SqlArenaCache`〔#363〕・`SparseIndexCache`
//! 〔#357〕・`VisibleBitmapCache`〔#478〕・`ScalarIndexCache`〔#473〕）が
//! `INSERT` 後の再読み取りでも正しく失効することを、5 種の読み取り形状
//! （dense ORDER BY・hybrid ORDER BY・bare scan・COUNT(*)・WHERE 付き
//! dense ORDER BY）でウォームしたうえで検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// seed 時点での行の真実（オラクル用。production の可視性判定は一切通さない）。
struct RowTruth {
    id: u64,
    tenant: &'static str,
}

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

/// 3 テナントそれぞれに `Public` 行を 1 件ずつ投入する（id=1..=3）。
fn seed(storage: &Storage) -> Vec<RowTruth> {
    let mut truth = Vec::new();
    for (idx, tenant) in TENANTS.iter().enumerate() {
        let id = (idx as u64) + 1;
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let op_id = OperationId::parse(&format!("rls11-seed-{id}")).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text("ja".to_string()),
                Value::Text("seed body".to_string()),
            ],
            &op_id,
        )
        .expect("insert seed row");
        truth.push(RowTruth { id, tenant });
    }
    truth
}

/// wire-server の認証導出点（`auth.rs::session_policy_context`）が返す形
/// （`Public` ＋ 自テナントの `Private`。RLS-11・TASK-195）を模した ctx。
/// engine の既定 [`PolicyContext::new`]（`Public` のみ）とは異なる。
fn session_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 読み取り形状 (a)〜(e): それぞれ別の世代整合キャッシュを踏む。
/// `LIMIT` は本テストの最大可視行数（seed 3 + 挿入 3 = 6）を上回る 20 に
/// 固定し、越境・非表示が `LIMIT` の打ち切りに隠れないようにする。
const DENSE_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20";
/// 疎側項は挿入行の `body` にのみ現れる語（'own-insert-marker'）を使う。
/// 疎側が実際にスコアへ寄与する形にすることで、`SparseIndexCache` の
/// 失効漏れ（疎索引が古いまま返り挿入行が候補にすら入らない）を検出
/// できるようにする（語彙不一致の疎側項では疎索引が寄与せず密のみへ
/// 縮退し、キャッシュ失効漏れを見逃す）。
const HYBRID_SQL: &str = "SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'own-insert-marker') LIMIT 20";
const SCAN_SQL: &str = "SELECT id FROM docs LIMIT 20";
const COUNT_SQL: &str = "SELECT COUNT(*) AS n FROM docs";
const WHERE_DENSE_SQL: &str =
    "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20";
/// (a)〜(d) それぞれ個別の `id` 集合と、`COUNT(*)` の件数を保持する。
/// 形状ごとに独立してアサートすることで、特定の世代整合キャッシュ
/// （例: `SparseIndexCache` の失効漏れで hybrid だけ挿入行を返さない）
/// だけが失効漏れを起こすケースを、他形状の結果に隠さず検出できる。
struct ShapeResults {
    dense: std::collections::BTreeSet<String>,
    hybrid: std::collections::BTreeSet<String>,
    scan: std::collections::BTreeSet<String>,
    where_dense: std::collections::BTreeSet<String>,
    count: u64,
}

impl ShapeResults {
    /// 4 形状すべてが `id` を含むことを確認する（1 つでも欠ければ
    /// どの形状のキャッシュが失効漏れかを assert メッセージで特定できる）。
    fn assert_all_contain(&self, id: &str, tenant: &str) {
        assert!(
            self.dense.contains(id),
            "tenant {tenant}: dense shape must observe id={id}, got {:?}",
            self.dense
        );
        assert!(
            self.hybrid.contains(id),
            "tenant {tenant}: hybrid shape must observe id={id}, got {:?}",
            self.hybrid
        );
        assert!(
            self.scan.contains(id),
            "tenant {tenant}: scan shape must observe id={id}, got {:?}",
            self.scan
        );
        assert!(
            self.where_dense.contains(id),
            "tenant {tenant}: WHERE dense shape must observe id={id}, got {:?}",
            self.where_dense
        );
    }

    /// 4 形状すべてが `id` を含まないことを確認する。
    fn assert_all_lack(&self, id: &str, tenant: &str) {
        assert!(
            !self.dense.contains(id),
            "tenant {tenant}: dense shape must NOT observe id={id}, got {:?}",
            self.dense
        );
        assert!(
            !self.hybrid.contains(id),
            "tenant {tenant}: hybrid shape must NOT observe id={id}, got {:?}",
            self.hybrid
        );
        assert!(
            !self.scan.contains(id),
            "tenant {tenant}: scan shape must NOT observe id={id}, got {:?}",
            self.scan
        );
        assert!(
            !self.where_dense.contains(id),
            "tenant {tenant}: WHERE dense shape must NOT observe id={id}, got {:?}",
            self.where_dense
        );
    }
}

/// (a)〜(d) をそれぞれ独立に実行し、形状ごとの `id` 列の集合を返す
/// （`WHERE lang = 'ja'` は挿入行にも `lang='ja'` を持たせるため base 集合
/// と一致する）。`COUNT(*)` は件数のみ別途返す。
fn run_all_shapes(
    core: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
) -> ShapeResults {
    let run_ids =
        |core: &EngineCore, ctx: &PolicyContext, session: &mut SessionState, sql: &str| {
            let outcome = core
                .execute_sql_in_session(ctx, session, sql)
                .unwrap_or_else(|e| panic!("sql {sql:?} must succeed: {e:?}"));
            let SqlOutcome::Query(result) = outcome else {
                panic!("expected Query outcome for {sql:?}, got {outcome:?}");
            };
            result
                .rows
                .into_iter()
                .map(|row| row.id.to_string())
                .collect::<std::collections::BTreeSet<String>>()
        };

    let dense = run_ids(core, ctx, session, DENSE_SQL);
    let hybrid = run_ids(core, ctx, session, HYBRID_SQL);
    let scan = run_ids(core, ctx, session, SCAN_SQL);
    let where_dense = run_ids(core, ctx, session, WHERE_DENSE_SQL);

    let outcome = core
        .execute_sql_in_session(ctx, session, COUNT_SQL)
        .expect("count sql must succeed");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome for COUNT_SQL, got {outcome:?}");
    };
    let count = match &result.rows[0].cells[0] {
        engine::sql::exec::Cell::Integer(n) => *n,
        other => panic!("expected Integer COUNT(*) cell, got {other:?}"),
    };
    ShapeResults {
        dense,
        hybrid,
        scan,
        where_dense,
        count,
    }
}

fn new_core(path: &std::path::Path) -> (EngineCore, Vec<RowTruth>) {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let truth = seed(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, truth)
}

/// RLS-11 の中核契約: 自テナントの `Private` 行は (1) 書いた本人の同一
/// セッション、(2) 同一テナントの新規セッション（新規構築した ctx・
/// `SessionState`）、いずれからも `INSERT` 直後に可視になる。世代整合
/// キャッシュをウォームしてから `INSERT` するため、キャッシュ失効漏れが
/// あれば本テストが検出する。
#[test]
fn rls11_own_private_row_is_visible_in_same_and_other_session_of_same_tenant_after_cache_warm() {
    let path = unique_db_path("rls11-own-visible");
    let _guard = CleanupGuard(path.clone());
    let (core, truth) = new_core(&path);

    for tenant in TENANTS {
        let ctx = session_ctx(tenant);
        let mut session = SessionState::default();

        // ウォーム: 挿入前に (a)〜(d) を一度実行する（seed の Public 3 行の
        // うち自テナント分だけが見える）。
        let before = run_all_shapes(&core, &ctx, &mut session);
        let own_seed_id: String = truth
            .iter()
            .find(|r| r.tenant == tenant)
            .expect("seed row for tenant")
            .id
            .to_string();
        before.assert_all_contain(&own_seed_id, tenant);
        assert_eq!(before.count, 3, "3 テナントの Public 行が全員に可視のはず");

        // INSERT（自身の Private 行。挿入 id はテナントごとに一意にする）。
        let insert_id = 100 + TENANTS.iter().position(|t| *t == tenant).unwrap() as u64;
        let op_id = OperationId::parse(&format!("rls11-insert-{tenant}")).expect("valid op id");
        let insert_sql = format!(
            "INSERT INTO docs (id, embedding, lang, body) VALUES \
             ({insert_id}, '[1.0,0.0]', 'ja', 'own-insert-marker text') \
             USING OPERATION_ID '{}'",
            op_id.as_str()
        );
        let outcome = core
            .execute_sql_in_session(&ctx, &mut session, &insert_sql)
            .expect("insert must succeed");
        match outcome {
            SqlOutcome::Insert(insert_outcome) => assert_eq!(insert_outcome.rows_affected, 1),
            other => panic!("expected Insert outcome, got {other:?}"),
        }

        // (1) 同一セッションで直ちに読み戻せる。形状ごとに個別確認すること
        // で、特定形状（例: hybrid のみ SparseIndexCache 失効漏れ）を
        // 他形状の結果に隠さず検出する。
        let same_session = run_all_shapes(&core, &ctx, &mut session);
        same_session.assert_all_contain(&insert_id.to_string(), tenant);
        assert_eq!(
            same_session.count, 4,
            "tenant {tenant}: own Private 行 1 件が加わるはず"
        );

        // (2) 同一テナントの新規セッション（新規構築した ctx・
        //     SessionState）でも同じ集合が見える。新規構築した ctx は
        //     `(table, ctx)` キャッシュキーとしては (1) と等価であり、
        //     ウォーム済みエントリを踏むため「失効が効いている」ことの
        //     強い証跡になる。形状ごとに個別比較する。
        let other_ctx = session_ctx(tenant);
        let mut other_session_state = SessionState::default();
        let other_session = run_all_shapes(&core, &other_ctx, &mut other_session_state);
        assert_eq!(
            other_session.dense, same_session.dense,
            "tenant {tenant}: fresh same-tenant session must see the identical dense set"
        );
        assert_eq!(
            other_session.hybrid, same_session.hybrid,
            "tenant {tenant}: fresh same-tenant session must see the identical hybrid set"
        );
        assert_eq!(
            other_session.scan, same_session.scan,
            "tenant {tenant}: fresh same-tenant session must see the identical scan set"
        );
        assert_eq!(
            other_session.where_dense, same_session.where_dense,
            "tenant {tenant}: fresh same-tenant session must see the identical WHERE dense set"
        );
        assert_eq!(other_session.count, same_session.count);
    }
}

/// RLS-11 の境界: 他テナントは前段でどのテナントが何を書いても一切見えず、
/// `WHERE` 述語での絞り込みも 0 件（エラーではない）のまま——ScalarIndex／
/// PrefilterCache 経路でも越境しないことを確認する。
#[test]
fn rls11_other_tenant_never_observes_private_row_across_all_read_shapes() {
    let path = unique_db_path("rls11-other-hidden");
    let _guard = CleanupGuard(path.clone());
    let (core, _truth) = new_core(&path);

    // 全テナントがまず自身の Private 行を書く（他テナント検証の前段）。
    for (idx, tenant) in TENANTS.iter().enumerate() {
        let ctx = session_ctx(tenant);
        let mut session = SessionState::default();
        let insert_id = 200 + idx as u64;
        let op_id =
            OperationId::parse(&format!("rls11-other-insert-{tenant}")).expect("valid op id");
        let marker_lang = format!("xx-{tenant}");
        let insert_sql = format!(
            "INSERT INTO docs (id, embedding, lang, body) VALUES \
             ({insert_id}, '[1.0,0.0]', '{marker_lang}', 'own-insert-marker text') \
             USING OPERATION_ID '{}'",
            op_id.as_str()
        );
        core.execute_sql_in_session(&ctx, &mut session, &insert_sql)
            .expect("insert must succeed");
    }

    // 他テナントから見ると、いずれの挿入 id も一切見えない。
    for (idx, tenant) in TENANTS.iter().enumerate() {
        let insert_id = 200 + idx as u64;
        let marker_lang = format!("xx-{tenant}");
        for other_tenant in TENANTS {
            if other_tenant == *tenant {
                continue;
            }
            let ctx = session_ctx(other_tenant);
            let mut session = SessionState::default();
            let results = run_all_shapes(&core, &ctx, &mut session);
            results.assert_all_lack(&insert_id.to_string(), other_tenant);

            // 述語一致の WHERE でも 0 件（エラーではない）。
            let where_sql = format!(
                "SELECT id FROM docs WHERE lang = '{marker_lang}' \
                 ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20"
            );
            let outcome = core
                .execute_sql_in_session(&ctx, &mut session, &where_sql)
                .expect(
                    "predicate-matching SELECT for another tenant's marker must succeed \
                         with zero rows, not error",
                );
            let SqlOutcome::Query(result) = outcome else {
                panic!("expected Query outcome, got {outcome:?}");
            };
            assert!(
                result.rows.is_empty(),
                "tenant {other_tenant} must see zero rows for tenant {tenant}'s marker \
                 predicate, got {:?}",
                result.rows
            );
        }
    }
}

/// #973 の「engine 既定 `PolicyContext::new` は無変更」を機械検証する:
/// wire-server の認証導出点を経由しない生の `PolicyContext::new` では、
/// 自テナントであっても `Private` 行は不可視のままである。
#[test]
fn engine_default_policy_context_still_hides_own_private_rows() {
    let path = unique_db_path("rls11-engine-default-unchanged");
    let _guard = CleanupGuard(path.clone());
    let (core, _truth) = new_core(&path);

    let tenant = "tenant-a";
    let write_ctx = session_ctx(tenant);
    let mut write_session = SessionState::default();
    let op_id = OperationId::parse("rls11-default-ctx-insert").expect("valid op id");
    core.execute_sql_in_session(
        &write_ctx,
        &mut write_session,
        &format!(
            "INSERT INTO docs (id, embedding, lang, body) VALUES \
             (300, '[1.0,0.0]', 'ja', 'own-insert-marker text') \
             USING OPERATION_ID '{}'",
            op_id.as_str()
        ),
    )
    .expect("insert must succeed");

    // engine 既定の PolicyContext::new（`Public` のみ許可）では、書いた本人
    // の同一テナントであっても Private 行は見えない。
    let default_ctx = PolicyContext::new(tenant).expect("valid tenant");
    let mut default_session = SessionState::default();
    let results = run_all_shapes(&core, &default_ctx, &mut default_session);
    results.assert_all_lack("300", tenant);
    assert_eq!(
        results.count, 3,
        "engine 既定 PolicyContext::new must only see the 3 shared Public seed rows \
         (unaffected by the Private insert)"
    );
}
