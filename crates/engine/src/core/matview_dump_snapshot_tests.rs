//! 論理ダンプのマテリアライズド VIEW 列構成検査（Issue #3097）が、出力と同じ読み取りスナップショットから
//! 基表を解決することのクレート内テスト。
//!
//! `EngineCore::check_matview_columns_for_dump` は `Storage` の書き出し入口からスナップショットと定義を
//! 受け取る。ここでは書き出しの途中に別 writer が基表の列を変えた状況を、スナップショットを先に開いて
//! から `ALTER TABLE` を実行することで再現し、再導出が現在のカタログではなくスナップショットを読む
//! ことを固定する（fail-closed の契約。現在のカタログを読むと旧本文・旧基表の組を取り違える）。

use super::EngineCore;
use crate::catalog::ddl_export::SchemaDdlError;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;

const TENANT: &str = "tenant-a";

fn run(core: &EngineCore, session: &mut SessionState, sql: &str) {
    let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    core.execute_sql_in_session(&ctx, session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// スナップショット取得後に基表へ列を足しても、そのスナップショットに対する検査は通り、
/// 新しいスナップショットに対する検査はずれとして拒否する。
#[test]
fn matview_column_check_reads_the_dump_snapshot_not_the_current_catalog() {
    let path = unique_db_path("matview-dump-snapshot");
    let _guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        "CREATE TABLE base (a INTEGER PRIMARY KEY)",
        "CREATE MATERIALIZED VIEW mv AS SELECT * FROM base",
    ] {
        run(&core, &mut session, sql);
    }

    let old_snap = core.storage.db().begin_read().expect("begin read");
    let def = Storage::materialized_view_definition_in(&old_snap, "mv")
        .expect("read definition")
        .expect("definition exists");

    // 別 writer 相当: スナップショットの後で基表の列を増やす（`SELECT *` の出力列が変わる）。
    run(&core, &mut session, "ALTER TABLE base ADD COLUMN b TEXT");

    let check = |snap: &redb::ReadTransaction| {
        core.check_matview_columns_for_dump::<std::convert::Infallible>(
            snap,
            Some(TENANT),
            std::slice::from_ref(&def),
        )
    };
    check(&old_snap).expect("snapshot-consistent definition must pass");

    let new_snap = core.storage.db().begin_read().expect("begin read");
    let err = check(&new_snap).expect_err("drift against the newer snapshot must be rejected");
    assert!(
        matches!(&err, SchemaDdlError::Unsupported(u) if u.kind == "materialized view"),
        "{err:?}"
    );
}
