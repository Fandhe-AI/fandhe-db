//! Issue #479: `sql::visible_cache::VisibleBitmapCache`（Issue #478）の
//! 非漏えいを **対照 DB（オラクル）方式** で固定する。`tests/sql_visible_cache.rs`
//! の `cache_key_never_leaks_across_tenants` は `COUNT(*)` の件数のみ・単一
//! シナリオで非漏えいを確認しているが、本ファイルは
//! `SUM`/`AVG`/`MIN`/`MAX(id)`（他テナントの 1 行が混入するだけで値が変わる
//! 集計）・複数文脈の交互ホット状態・失効後・セッション経由（wire の入口）
//! まで、「その文脈で不可視な行が物理的に存在しない DB」との結果一致を
//! 機械検証する（`tests/sparse_cache_recall.rs` の統計縮約オラクルと同じ
//! 流儀）。
//!
//! 非 vacuous ガード: cold 呼び出しは新規 `EngineCore`（空キャッシュ）で
//! `misses` が +1 することを、hot 呼び出しは同一 `EngineCore` の 2 回目で
//! `hits` が +1 することをそれぞれ確認してから対照 DB と突き合わせる
//! （ミス経路への空振りで非漏えいが「たまたま」通ってしまうのを防ぐ。
//! `tests/sql_visible_cache.rs::id_aggregates_match_between_cold_and_hot_cache`
//! の codex-review 指摘対応を踏襲）。
//!
//! `AVG(id)` は `Accumulator::IdAvg`（`sql/aggregate.rs`）が `u64` の厳密な
//! 総和を保持し、`finish()` で 1 回だけ `sum as f64 / count as f64` を計算する
//! （途中を f64 で逐次加算しない）ため、可視 `id` を観測する順序に関わらず
//! 得られる `f64` はビット一致する。対照 DB 側でも同じ可視 `id` 集合を観測する
//! ため、本ファイルの比較は近似ではなく `assert_eq!` の完全一致で行う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

/// Fast tier（`GROUP BY` なし・`WHERE` なし・`COUNT`/`SUM`/`AVG`/`MIN`/`MAX(id)`）
/// が尽くす集計文の全形状。単項 5 種 + crossdb `agg_multi` と同形の複合 1 種。
const AGGREGATE_QUERIES: &[&str] = &[
    "SELECT COUNT(*) FROM docs",
    "SELECT COUNT(id) FROM docs",
    "SELECT SUM(id) FROM docs",
    "SELECT AVG(id) FROM docs",
    "SELECT MIN(id) FROM docs",
    "SELECT MAX(id) FROM docs",
    "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs",
];

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn ctx_public_only(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn ctx_with_private(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

/// `(tenant_id, id)` 昇順の物理走査順を再現するため、呼び出し順は常に
/// `id` 昇順で揃えて呼ぶ（`sql::visible_cache::VisibleSnapshot` のモジュール
/// ドキュメント「物理走査順」契約。呼び出し側がこの順序を崩すと `AVG` 以外の
/// 一部集計は影響しないが、対照 DB との走査順一致という前提そのものが崩れる
/// ため揃えておく）。
fn insert_row(core: &EngineCore, ctx: &PolicyContext, id: u64, visibility: Visibility, seq: u64) {
    core.insert_row(
        ctx,
        TABLE,
        id,
        &RowInput {
            tenant_id: ctx.tenant_id(),
            visibility,
            embedding: &[0.0f32, 0.0f32],
            metadata: &[],
        },
        Some(&op_id(&format!("test-op-{seq}"))),
    )
    .expect("insert row");
}

/// 実 DB・対照 DB 両方に対して同じコーパスを構築するための投入計画。
/// `(tenant, visibility, id)` の並びで、呼び出し順が `id` 昇順になるよう
/// 事前に用意する。
struct RowSpec {
    tenant: &'static str,
    visibility: Visibility,
    id: u64,
}

/// 4 群（tenant-a Public・tenant-a Private・tenant-b Public・tenant-b Private）を
/// id 帯域で分けて投入する。id 昇順で構築する（物理走査順を単純にするため）。
fn corpus() -> Vec<RowSpec> {
    let mut rows = Vec::new();
    for id in 1..=10u64 {
        rows.push(RowSpec {
            tenant: "tenant-a",
            visibility: Visibility::Public,
            id,
        });
    }
    for id in 1001..=1010u64 {
        rows.push(RowSpec {
            tenant: "tenant-a",
            visibility: Visibility::Private,
            id,
        });
    }
    for id in 2001..=2010u64 {
        rows.push(RowSpec {
            tenant: "tenant-b",
            visibility: Visibility::Public,
            id,
        });
    }
    for id in 3001..=3010u64 {
        rows.push(RowSpec {
            tenant: "tenant-b",
            visibility: Visibility::Private,
            id,
        });
    }
    rows
}

fn ctx_for_tenant(spec_tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        ctx_with_private(spec_tenant)
    } else {
        ctx_public_only(spec_tenant)
    }
}

fn is_visible_to(spec: &RowSpec, viewer_tenant: &str, allow_private: bool) -> bool {
    match spec.visibility {
        Visibility::Public => true,
        Visibility::Private => allow_private && spec.tenant == viewer_tenant,
    }
}

/// メイン DB へ全行を物理投入する（RLS 判定はクエリ時点で行われる）。
fn build_main_db(core: &EngineCore) {
    for (seq, spec) in corpus().into_iter().enumerate() {
        let owner_ctx = ctx_for_tenant(spec.tenant, true);
        insert_row(core, &owner_ctx, spec.id, spec.visibility, seq as u64);
    }
}

/// 対照 DB: 指定した文脈（`viewer_tenant`・`allow_private`）から見て可視な行
/// **だけ** を、同一 id・同一 visibility・同一 tenant・同一投入順で物理投入
/// する。不可視行はこの DB に一切存在しない。
fn build_oracle_db(core: &EngineCore, viewer_tenant: &str, allow_private: bool) {
    // `seq`（`operation_id` 生成用）は一意性のみを要求し連番である必要は
    // ないため、スキップした行の分だけ歯抜けになっても問題ない
    // （`enumerate()` の添字をそのまま使う）。
    for (seq, spec) in corpus().into_iter().enumerate() {
        if !is_visible_to(&spec, viewer_tenant, allow_private) {
            continue;
        }
        let owner_ctx = ctx_for_tenant(spec.tenant, true);
        insert_row(core, &owner_ctx, spec.id, spec.visibility, seq as u64);
    }
}

/// `CleanupGuard` を `EngineCore`（内部で `Storage` を保持）より先に返す
/// （タプルの戻り順ではなく、呼び出し側の束縛順が `Drop` の逆順実行を決める。
/// `temp_db.rs` の契約どおり `(guard, core)` の順で受けてもらう前提の
/// シグネチャにしておくことで、呼び出し側の書き間違いを構造的に防ぐ）。
fn open_core(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (guard, new_core(storage))
}

fn run_query(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql).expect("query should succeed")
}

/// cold（新規 `EngineCore`）で 1 回だけクエリを実行し、真にキャッシュ未経由の
/// 通常走査であったこと（`misses` が +1）を確認したうえで結果を返す。
fn cold_result(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    let before = core.visible_bitmap_cache_stats();
    let result = run_query(core, ctx, sql);
    let after = core.visible_bitmap_cache_stats();
    assert_eq!(
        after.misses,
        before.misses + 1,
        "cold query for `{sql}` must be a genuine cache miss (non-vacuous guard)"
    );
    result
}

/// 対照 DB 方式: 全集計形状について、cold（新規 `EngineCore` ごと・真のミス
/// 経路）の実 DB 結果が対照 DB の結果と完全一致することを確認する
/// （RLS-7・RLS-8。単一トランザクション内の複数文脈を跨いだ非漏えい）。
#[test]
fn cold_aggregate_matches_oracle_db_across_contexts() {
    let (_main_guard, main_core) = open_core("visible-cache-oracle-cold-main");
    build_main_db(&main_core);

    for (viewer_tenant, allow_private) in
        [("tenant-a", false), ("tenant-a", true), ("tenant-b", false)]
    {
        let viewer_ctx = ctx_for_tenant(viewer_tenant, allow_private);
        for sql in AGGREGATE_QUERIES {
            // 実 DB 側は毎回新規 `EngineCore`（cold）で実行し、キャッシュ
            // ヒットに紛れ込んだ「本来 hot 経路で得たはずの正しい値」を cold
            // 経路の検証だと誤認しないようにする。
            let (_fresh_guard, fresh_main_core) = open_core("visible-cache-oracle-cold-main-run");
            build_main_db(&fresh_main_core);
            let actual = cold_result(&fresh_main_core, &viewer_ctx, sql);

            let (_oracle_guard, oracle_core) = open_core("visible-cache-oracle-cold-oracle-run");
            build_oracle_db(&oracle_core, viewer_tenant, allow_private);
            let expected = run_query(&oracle_core, &viewer_ctx, sql);

            assert_eq!(
                actual.rows, expected.rows,
                "cold `{sql}` for tenant={viewer_tenant} allow_private={allow_private} \
                 must match a DB where invisible rows are physically absent"
            );
        }
    }
    // `main_core`／`build_main_db` は共有コーパス構成の対称性を示すために
    // 構築するが、上のループでは各ケースを独立した新規 `EngineCore` で検証
    // する（cold の非 vacuous 性を保つため）。共有インスタンス自体は本テスト
    // では未使用のまま drop する。
    drop(main_core);
}

/// hot（同一 `EngineCore` の 2 回目・`hits` が +1）でも対照 DB と一致する
/// ことを確認する。cold と hot で得られる値が食い違えばキャッシュ構築時に
/// 他テナントの行が紛れ込んだ証拠になる。
#[test]
fn hot_aggregate_matches_oracle_db_across_contexts() {
    for (viewer_tenant, allow_private) in
        [("tenant-a", false), ("tenant-a", true), ("tenant-b", false)]
    {
        let viewer_ctx = ctx_for_tenant(viewer_tenant, allow_private);
        for sql in AGGREGATE_QUERIES {
            let (_main_guard, main_core) = open_core("visible-cache-oracle-hot-main");
            build_main_db(&main_core);

            let before = main_core.visible_bitmap_cache_stats();
            let _warm = run_query(&main_core, &viewer_ctx, sql);
            let after_first = main_core.visible_bitmap_cache_stats();
            assert_eq!(
                after_first.misses,
                before.misses + 1,
                "first call for `{sql}` must populate the cache"
            );

            let actual = run_query(&main_core, &viewer_ctx, sql);
            let after_second = main_core.visible_bitmap_cache_stats();
            assert_eq!(
                after_second.hits,
                after_first.hits + 1,
                "second call for `{sql}` must hit the cache populated by the first (non-vacuous guard)"
            );

            let (_oracle_guard, oracle_core) = open_core("visible-cache-oracle-hot-oracle");
            build_oracle_db(&oracle_core, viewer_tenant, allow_private);
            let expected = run_query(&oracle_core, &viewer_ctx, sql);

            assert_eq!(
                actual.rows, expected.rows,
                "hot `{sql}` for tenant={viewer_tenant} allow_private={allow_private} \
                 must match a DB where invisible rows are physically absent"
            );
        }
    }
}

/// 単一 `EngineCore` を ctx1（tenant-a Public のみ）→ ctx3（tenant-b Public
/// のみ）→ ctx2（tenant-a Public+Private）→ ctx1 → ctx3 の順に交互実行する。
/// 各文脈のエントリが他文脈のエントリと同居した状態でも、それぞれが自身の
/// 対照 DB と一致し続けることを確認する（複数エントリ常駐下の非漏えい）。
#[test]
fn interleaved_hot_contexts_never_cross_contaminate() {
    let (_main_guard, main_core) = open_core("visible-cache-oracle-interleaved");
    build_main_db(&main_core);

    let ctx1 = ctx_for_tenant("tenant-a", false);
    let ctx2 = ctx_for_tenant("tenant-a", true);
    let ctx3 = ctx_for_tenant("tenant-b", false);

    let oracle = |viewer_tenant: &str, allow_private: bool, sql: &str| -> QueryResult {
        let (_guard, oracle_core) = open_core("visible-cache-oracle-interleaved-oracle");
        build_oracle_db(&oracle_core, viewer_tenant, allow_private);
        run_query(
            &oracle_core,
            &ctx_for_tenant(viewer_tenant, allow_private),
            sql,
        )
    };

    let sql = "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs";

    let before = main_core.visible_bitmap_cache_stats();

    // ctx1: 初回ミス。
    let r1 = run_query(&main_core, &ctx1, sql);
    assert_eq!(r1.rows, oracle("tenant-a", false, sql).rows);

    // ctx3: 初回ミス（ctx1 と別キー）。
    let r3 = run_query(&main_core, &ctx3, sql);
    assert_eq!(r3.rows, oracle("tenant-b", false, sql).rows);

    // ctx2: 初回ミス（ctx1 と同一テナントだが可視性が異なるため別キー）。
    let r2 = run_query(&main_core, &ctx2, sql);
    assert_eq!(r2.rows, oracle("tenant-a", true, sql).rows);

    let after_three_misses = main_core.visible_bitmap_cache_stats();
    assert_eq!(
        after_three_misses.misses,
        before.misses + 3,
        "three distinct (table, ctx) keys must each miss exactly once"
    );

    // ctx1 を再訪: ctx3・ctx2 のエントリが常駐した状態でもヒットし、自身の
    // 対照 DB と一致し続ける（他エントリからの汚染がないことの確認）。
    let r1_again = run_query(&main_core, &ctx1, sql);
    assert_eq!(
        r1_again.rows, r1.rows,
        "ctx1 hot result must equal its own cold result"
    );
    assert_eq!(r1_again.rows, oracle("tenant-a", false, sql).rows);

    // ctx3 を再訪。
    let r3_again = run_query(&main_core, &ctx3, sql);
    assert_eq!(r3_again.rows, r3.rows);
    assert_eq!(r3_again.rows, oracle("tenant-b", false, sql).rows);

    let after_all_hits = main_core.visible_bitmap_cache_stats();
    assert_eq!(
        after_all_hits.hits,
        after_three_misses.hits + 2,
        "the two revisits must hit, not miss"
    );
    assert_eq!(
        after_all_hits.misses, after_three_misses.misses,
        "no additional misses should occur once all three keys are warm"
    );
}

/// tenant-b（ctx3）を hot にした後、メイン DB にのみ tenant-a の Private 行を
/// 追加する（対照 DB は不変）。ctx3 の再訪は世代進行により失効
/// （`stale_evictions` 増加）し再構築されるが、tenant-a の新規 Private 行は
/// ctx3 の対照 DB（更新なし）と一致したまま漏れ込まない。あわせて ctx2
/// （tenant-a Private 可視）は新規行を反映した対照 DB と一致することを確認
/// する。
#[test]
fn stale_eviction_after_unrelated_tenant_write_does_not_leak() {
    let (_main_guard, main_core) = open_core("visible-cache-oracle-stale");
    build_main_db(&main_core);

    let ctx2 = ctx_for_tenant("tenant-a", true);
    let ctx3 = ctx_for_tenant("tenant-b", false);
    let sql = "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs";

    // ctx3 を hot にする。
    let ctx3_before_write = run_query(&main_core, &ctx3, sql);
    let (_g, oracle3_before) = open_core("visible-cache-oracle-stale-oracle3-before");
    build_oracle_db(&oracle3_before, "tenant-b", false);
    assert_eq!(
        ctx3_before_write.rows,
        run_query(&oracle3_before, &ctx3, sql).rows
    );
    let _ctx3_hot = run_query(&main_core, &ctx3, sql);

    // メイン DB にのみ tenant-a の新規 Private 行を追加する（id 帯域 1011.. を
    // 使い、既存の corpus() の走査順を崩さない）。
    let extra_ids = [1011u64, 1012u64];
    for (offset, id) in extra_ids.iter().enumerate() {
        insert_row(
            &main_core,
            &ctx_for_tenant("tenant-a", true),
            *id,
            Visibility::Private,
            9_000 + offset as u64,
        );
    }

    let stats_after_write = main_core.visible_bitmap_cache_stats();

    // ctx3 の再訪: 世代進行により失効。tenant-a の Private 行は ctx3 からは
    // 常に不可視のため、対照 DB（不変）と一致し続ける。
    let ctx3_after_write = run_query(&main_core, &ctx3, sql);
    let stats_after_ctx3_revisit = main_core.visible_bitmap_cache_stats();
    assert!(
        stats_after_ctx3_revisit.stale_evictions > stats_after_write.stale_evictions,
        "write to the table must evict ctx3's now-stale cache entry on next lookup"
    );
    assert_eq!(
        ctx3_after_write.rows,
        run_query(&oracle3_before, &ctx3, sql).rows,
        "tenant-a's newly inserted private rows must never appear in tenant-b's view"
    );

    // ctx2（tenant-a Private 可視）は新規行を反映した対照 DB と一致する。
    let ctx2_result = run_query(&main_core, &ctx2, sql);
    let (_g2, oracle2_after) = open_core("visible-cache-oracle-stale-oracle2-after");
    build_oracle_db(&oracle2_after, "tenant-a", true);
    for (offset, id) in extra_ids.iter().enumerate() {
        insert_row(
            &oracle2_after,
            &ctx_for_tenant("tenant-a", true),
            *id,
            Visibility::Private,
            9_100 + offset as u64,
        );
    }
    assert_eq!(ctx2_result.rows, run_query(&oracle2_after, &ctx2, sql).rows);
}

/// セッション経由（wire の入口。`EngineCore::execute_sql_in_session`）でも
/// 対照 DB と一致することを確認する。ここでは ctx1（tenant-a）・ctx3
/// （tenant-b）相当のみを確認する（ctx2 相当の「明示的に Private 可視の
/// `PolicyContext` を構築して問い合わせる」経路は本テストの対象外。
/// production の wire-server 認証導出点（`auth.rs::session_policy_context`）
/// が実際に返す `PolicyContext`〔`Public` ＋ 自テナントの `Private`。
/// RLS-11・TASK-195〕とキャッシュ整合の統合検証は
/// `crates/wire-server/tests/rls11_read_your_writes.rs` に委ねる）。
#[test]
fn session_entrypoint_matches_oracle_db() {
    // キャッシュキーは `(table, PolicyContext)` のみで集計式を含まない
    // （`tests/sql_visible_cache.rs::id_aggregates_match_between_cold_and_hot_cache`
    // のコメント参照）。同一 tenant で 2 種の集計文を検証するため、各ケース
    // ごとに新規 `EngineCore`（空キャッシュ）を使い、cold 呼び出しが真に
    // ミス経路であることを保証する。
    for (viewer_tenant, sql) in [
        ("tenant-a", "SELECT COUNT(*) FROM docs"),
        ("tenant-a", "SELECT SUM(id) FROM docs"),
        ("tenant-b", "SELECT COUNT(*) FROM docs"),
    ] {
        let (_main_guard, main_core) = open_core("visible-cache-oracle-session");
        build_main_db(&main_core);
        let ctx = ctx_public_only(viewer_tenant);
        let mut session = SessionState::default();

        let before = main_core.visible_bitmap_cache_stats();
        let cold_outcome = main_core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .expect("cold session query should succeed");
        let SqlOutcome::Query(cold_result) = cold_outcome else {
            panic!("expected Query outcome");
        };
        let after_cold = main_core.visible_bitmap_cache_stats();
        assert_eq!(after_cold.misses, before.misses + 1);

        let hot_outcome = main_core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .expect("hot session query should succeed");
        let SqlOutcome::Query(hot_result) = hot_outcome else {
            panic!("expected Query outcome");
        };
        let after_hot = main_core.visible_bitmap_cache_stats();
        assert_eq!(after_hot.hits, after_cold.hits + 1);
        assert_eq!(cold_result.rows, hot_result.rows);

        let (_guard, oracle_core) = open_core("visible-cache-oracle-session-oracle");
        build_oracle_db(&oracle_core, viewer_tenant, false);
        let expected = run_query(&oracle_core, &ctx, sql);
        assert_eq!(
            hot_result.rows, expected.rows,
            "session entrypoint for `{sql}` (tenant={viewer_tenant}) must match the oracle DB"
        );
    }
}
