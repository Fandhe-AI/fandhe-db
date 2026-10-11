//! 1 文あたりの総走査行数上限（`--max-scanned-rows`）の起動時設定
//! （Issue #1549。ポインタ: `docs/spec/04-behavior/` SQL-46、テナント境界は
//! RLS-9・RLS-10）の結合テスト。
//!
//! `EngineCore::with_scan_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server と NoSQL 表層が共有する実行入口）を production 経路として検証する。
//! 確認する契約:
//!
//! - 設定値ちょうどの走査行数は成功し、超過は従来と同じ `54000` で拒否され副作用ゼロ
//! - 同じデータに対し、設定を上げる／未注入（既定）なら成功し、下げると失敗する
//! - 他テナントの行数は成否に影響しない（対象テナント所有行だけを数える）
//! - 未注入の既定はハード上限（1,000,000）で、現行挙動と同一

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::scan_limits::ScanLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
/// 試験用の小さい設定値 N。
const N: usize = 10;

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

fn core_with(name: &str, limits: Option<ScanLimits>) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_scan_limits(l),
        None => core,
    };
    (core, path)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(core: &EngineCore, ctx: &PolicyContext, n: usize) {
    for id in 1..=n {
        let mut s = SessionState::default();
        core.execute_sql_in_session(
            ctx,
            &mut s,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 ({id}, '[0.1,0.2]', 'ja', 'b') USING OPERATION_ID 'seed-{}-{id}'",
                ctx.tenant_id()
            ),
        )
        .expect("seed row");
    }
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(ctx, &mut s, sql)
}

fn count_star(core: &EngineCore, ctx: &PolicyContext) -> u64 {
    let r = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count");
    match &r.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Integer, got {other:?}"),
    }
}

fn delete_sql(op: &str) -> String {
    format!("DELETE FROM {TABLE} WHERE lang = 'zzz-nomatch' USING OPERATION_ID '{op}'")
}

fn update_sql(op: &str) -> String {
    format!("UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'zzz-nomatch' USING OPERATION_ID '{op}'")
}

fn delete_returning_sql(op: &str) -> String {
    format!("DELETE FROM {TABLE} WHERE lang = 'zzz-nomatch' RETURNING id USING OPERATION_ID '{op}'")
}

fn update_returning_sql(op: &str) -> String {
    format!(
        "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'zzz-nomatch' RETURNING id USING OPERATION_ID '{op}'"
    )
}

#[test]
fn default_is_hard_limit_and_matches_uninjected_core() {
    let (core, path) = core_with("scan-limits-default", None);
    let _g = CleanupGuard(path);
    assert_eq!(core.scan_limits(), ScanLimits::default());
    assert_eq!(core.scan_limits().max_scanned_rows(), 1_000_000);
}

/// 述語形 DELETE／UPDATE（RETURNING あり・なし）: 上限ちょうどは成功、+1 は `54000`
/// で行も台帳も変わらない（同一 operation_id の再送も同じ `54000`）。
#[test]
fn predicate_dml_boundary_exact_ok_and_over_rejected_without_side_effects() {
    type Builder = fn(&str) -> String;
    let builders: [(&str, Builder); 4] = [
        ("delete", delete_sql),
        ("update", update_sql),
        ("delete-returning", delete_returning_sql),
        ("update-returning", update_returning_sql),
    ];
    for (name, build) in builders {
        let alice = ctx_for("alice");

        // 上限ちょうど（N 行）: 成功（一致 0 件）。
        let (core, path) = core_with(
            &format!("scan-limits-exact-{name}"),
            Some(ScanLimits::new(N)),
        );
        let _g = CleanupGuard(path);
        seed(&core, &alice, N);
        run(&core, &alice, &build("op-exact")).unwrap_or_else(|e| panic!("{name}: exact: {e:?}"));
        assert_eq!(count_star(&core, &alice), N as u64);

        // 上限 +1（N+1 行）: 54000・副作用ゼロ。
        let (core, path) = core_with(
            &format!("scan-limits-over-{name}"),
            Some(ScanLimits::new(N)),
        );
        let _g2 = CleanupGuard(path);
        seed(&core, &alice, N + 1);
        let err = run(&core, &alice, &build("op-over")).expect_err("over limit");
        assert_eq!(err.wire_code(), "54000", "{name}");
        assert_eq!(count_star(&core, &alice), (N + 1) as u64, "{name}");
        let again = run(&core, &alice, &build("op-over")).expect_err("resend");
        assert_eq!(again.wire_code(), "54000", "{name}: ledger must stay empty");
    }
}

/// 同じデータ（N+1 行）に対し、設定を上げる・未注入（既定）なら成功し、下げると失敗する。
#[test]
fn raising_and_lowering_the_limit_changes_outcome_on_same_data() {
    let alice = ctx_for("alice");
    for (label, limits, expect_ok) in [
        ("lowered", Some(ScanLimits::new(N)), false),
        ("raised", Some(ScanLimits::new(N + 1)), true),
        ("default", None, true),
        ("lowest", Some(ScanLimits::new(1)), false),
    ] {
        let (core, path) = core_with(&format!("scan-limits-vary-{label}"), limits);
        let _g = CleanupGuard(path);
        seed(&core, &alice, N + 1);
        let r = run(&core, &alice, &delete_sql("op-vary"));
        match (expect_ok, r) {
            (true, Ok(_)) => {}
            (false, Err(e)) => assert_eq!(e.wire_code(), "54000", "{label}"),
            (want, got) => panic!("{label}: expected ok={want}, got {got:?}"),
        }
    }
}

/// RLS-9・RLS-10 の観点: 他テナント（bob）が上限を大きく超える行を持っていても、
/// 対象テナント（alice）が上限以下なら成功する（他テナントの行数は応答に表れない）。
#[test]
fn other_tenant_rows_do_not_count_toward_the_limit() {
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    let (core, path) = core_with("scan-limits-tenant", Some(ScanLimits::new(N)));
    let _g = CleanupGuard(path);
    seed(&core, &bob, N * 3);
    seed(&core, &alice, N);
    run(&core, &alice, &delete_sql("op-tenant-d")).expect("alice delete within limit");
    run(&core, &alice, &update_sql("op-tenant-u")).expect("alice update within limit");
    assert_eq!(count_star(&core, &bob), (N * 3) as u64);
}
/// ファイル形 INSERT の置換前走査が上限を超えた場合も、述語形 DML と同じ固定文言の
/// `54000` になる（`XX000` へ丸められない。codex-review P1 指摘・PR #2316）。
/// 上限 1 で自テナント行が 2 件ある状態の 3 件目の投入が再現条件。
#[test]
fn file_insert_replace_scan_over_limit_is_54000() {
    use engine::chunking::ChunkingConfig;
    use engine::embedding::HashingEmbedder;
    use engine::incremental::IncrementalConfig;

    let path = unique_db_path("scan-limits-file-insert");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(16).expect("valid dim")))
        .with_incremental_config(IncrementalConfig {
            chunking: ChunkingConfig {
                lines_per_chunk: 10,
                max_markdown_section_chars: None,
            },
            ..IncrementalConfig::default()
        })
        .with_scan_limits(ScanLimits::new(1));
    let alice = ctx_for("alice");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(
        &alice,
        &mut ddl,
        "CREATE TABLE docs (embedding VECTOR(16), path TEXT, body TEXT)",
    )
    .expect("create table");
    let insert = |p: &str, op: &str| {
        core.execute_insert_sql(
            &alice,
            &format!(
                "INSERT INTO docs (path, body) VALUES ('{p}', 'hello') USING OPERATION_ID '{op}'"
            ),
        )
    };
    insert("a.md", "op-a").expect("first file: scans 0 rows");
    insert("b.md", "op-b").expect("second file: scans 1 row (== limit)");
    let err = insert("c.md", "op-c").expect_err("third file: scans 2 rows (> limit)");
    assert_eq!(err.wire_code(), "54000");
}
