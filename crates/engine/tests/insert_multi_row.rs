//! 複数行 `INSERT ... VALUES (...), (...), ...`（SQL-16、TASK-190）の受理・実行契約を
//! 固定する結合テスト。行形（`id`/`VECTOR` 列を含む形）の複数行対応のみを対象とし、
//! ファイル形（`path`/`body` 列）への複数行 `VALUES` 併用は非対応（`42601`）で
//! あることも合わせて固定する。
//!
//! 実行本体は NoSQL 表層の `rows[]`（NOSQL-6・TASK-178）と同じ
//! `sql::exec::execute_insert_batch_with_schema` を共有するため（第 2 の書き込み
//! 経路を作らない設計。`tests/sql_insert_batch_public_api.rs` が実行契約
//! （`operation_id` 必須化・台帳照合・TABLE-12・RLS-9）を既に固定済み）、本ファイルは
//! SQL 表層固有の関心事（複数行 `VALUES` の構文受理・行順保持・1 文あたりの行数上限・
//! 内容照合ハッシュの行順依存性（RECOVER-11(a)）・単一行の既存挙動が変わらないこと・
//! `self.batch_limits`（INDEX-4）が NoSQL 表層 `rows[]` と同一上限を共有すること
//! （Issue #860 SQL/NoSQL 機能パリティ）に限定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::validate_insert;
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_insert_form, BoundInsertForm};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn row_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn file_schema() -> TableSchema {
    TableSchema::new(
        "chunks",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// `n` 行分の `(id, embedding, lang)` 行形 `VALUES` 文を組み立てる。
fn multi_row_sql(table: &str, ids: &[u64], op_id: &str) -> String {
    let rows: Vec<String> = ids
        .iter()
        .map(|id| format!("({id}, '[{}.0,0.0]', 'ja')", id))
        .collect();
    format!(
        "INSERT INTO {table} (id, embedding, lang) VALUES {} USING OPERATION_ID '{op_id}'",
        rows.join(", ")
    )
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn open_engine_with_limits(
    name: &str,
    limits: engine::batch_limits::BatchLimits,
) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);
    (core, path)
}

/// `core` を `tenant` の可視範囲（`Public`＋`Private`）で読み戻し、可視な行
/// `id` の一覧を返す（副作用ゼロ・原子性の確定オラクル。
/// `nosql6_insert.rs::read_back_ids` と同じ設計）。
fn read_back_ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let policy = ctx(tenant);
    let mut session = SessionState::default();
    let sql = format!("SELECT id FROM {TABLE} LIMIT 100");
    let outcome = core
        .execute_sql_in_session(&policy, &mut session, &sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result.rows.iter().map(|row| row.id).collect()
}

/// `core.operation_recorded` の薄いラッパー（tenant 固定なし。
/// `nosql6_insert.rs::ledger_state` と同じ設計）。
fn ledger_state(core: &EngineCore, tenant: &str, op_id: &str) -> LedgerLookup {
    let policy = ctx(tenant);
    let parsed = OperationId::parse(op_id).expect("valid operation_id");
    core.operation_recorded(&policy, TABLE, &parsed)
        .expect("ledger lookup must not error")
}

// ---------------------------------------------------------------------
// 受け入れ条件①: 行順保持（構造束縛レベル）
// ---------------------------------------------------------------------

#[test]
fn bind_insert_form_multi_row_preserves_row_order() {
    let path = unique_db_path("insert-multi-row-order");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");

    // 明示的に非ソート順（3, 1, 4, 5）で並べ、束縛結果がこの投入順のまま
    // 保持されることを確認する（RECOVER-11(a)・投入順のまま連結される契約の前提）。
    let sql = multi_row_sql(TABLE, &[3, 1, 4, 5], "op-order");
    let stmt =
        validate_insert(&sql, &storage, LedgerMode::Ledgered).expect("validate_insert succeeds");
    let schema = storage.get_table_schema(TABLE).expect("schema");
    let bound = bind_insert_form(&stmt, &schema).expect("bind_insert_form succeeds");
    match bound {
        BoundInsertForm::RowBatch(bounds) => {
            let ids: Vec<u64> = bounds.iter().map(|b| b.id).collect();
            assert_eq!(ids, vec![3, 1, 4, 5]);
        }
        other => panic!("expected RowBatch, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 受け入れ条件④: 単一行の既存挙動が変わらないこと
// ---------------------------------------------------------------------

#[test]
fn bind_insert_form_single_row_values_still_returns_row_variant() {
    let path = unique_db_path("insert-multi-row-single-unchanged");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");

    let sql = multi_row_sql(TABLE, &[1], "op-single");
    let stmt =
        validate_insert(&sql, &storage, LedgerMode::Ledgered).expect("validate_insert succeeds");
    let schema = storage.get_table_schema(TABLE).expect("schema");
    let bound = bind_insert_form(&stmt, &schema).expect("bind_insert_form succeeds");
    match bound {
        BoundInsertForm::Row(b) => assert_eq!(b.id, 1),
        other => panic!("single-row VALUES must still bind to Row, got {other:?}"),
    }
}

#[test]
fn execute_insert_sql_single_row_behavior_is_unchanged() {
    let (core, path) = open_engine("insert-multi-row-single-exec-unchanged");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1], "op-exec-single");
    let outcome = core
        .execute_insert_sql(&policy, &sql)
        .expect("single-row insert succeeds");
    assert_eq!(outcome.rows_affected, 1);

    // 同一内容の再送は単一行 INSERT と同じ 23505（台帳ハッシュ空間の共有）。
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("resend must fail");
    assert_eq!(err.wire_code(), "23505");
}

// ---------------------------------------------------------------------
// 受け入れ条件②: 行数上限超過 → 54000・副作用ゼロ
// ---------------------------------------------------------------------

/// Issue #997・オーナー判断の改訂（2026-09-27、汎用 RDB との整合）: 既定
/// （`--max-insert-rows` 未指定＝`DmlLimits::default().max_insert_rows_per_statement
/// == None`）では構文段の行数上限判定を行わない。本テストは
/// `batch_limits.max_files_per_batch`（既定 64。Issue #860）を明示的に引き上げて
/// 構文段が上限なしで通過することを固定したうえで、`dml_limits` を明示設定
/// （`Some(1_000)`）した場合にのみ旧既定値相当の `54000`・副作用ゼロが成立する
/// ことを確認する（BREAKING CHANGE: 既定構成のみでは 2,000 行の複数行 `VALUES`
/// はもはや構文段では拒否されない）。
#[test]
fn multi_row_insert_exceeding_configured_row_limit_is_rejected_with_54000_and_no_side_effects() {
    let path = unique_db_path("insert-multi-row-configured-limit");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_dml_limits(engine::sql::parser::DmlLimits {
            max_insert_rows_per_statement: Some(
                std::num::NonZeroUsize::new(1_000).expect("1_000 is nonzero"),
            ),
            ..engine::sql::parser::DmlLimits::default()
        })
        .with_batch_limits(engine::batch_limits::BatchLimits {
            max_files_per_batch: 2_000,
            ..engine::batch_limits::BatchLimits::default()
        });
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    // 「明らかに超過する規模」を投入する（`tests/sql_group_by.rs::
    // group_count_over_max_groups_is_rejected_as_payload_too_large` と同じ方針）。
    // 行数上限の判定は構文解析段階（テーブル存在確認より前）で完結するため、
    // テーブル名の実在有無に関係なく判定される。
    let ids: Vec<u64> = (1..=2_000).collect();
    let sql = multi_row_sql(TABLE, &ids, "op-over-limit");

    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("row count over the configured limit must be rejected");
    assert_eq!(err.wire_code(), "54000");

    // 副作用ゼロ: 台帳・行のいずれも書き込まれていない（同じ operation_id で
    // 少数行の INSERT を送っても衝突せず成功することで、先の拒否が台帳に
    // 何も残していないことを確認する）。
    let follow_up_sql = multi_row_sql(TABLE, &[1, 2], "op-over-limit");
    let outcome = core
        .execute_insert_sql(&policy, &follow_up_sql)
        .expect("operation_id must be unused after the rejected over-limit statement");
    assert_eq!(outcome.rows_affected, 2);
}

// ---------------------------------------------------------------------
// Issue #997: `--max-insert-rows`（`EngineCore::dml_limits.
// max_insert_rows_per_statement`）による行数上限の起動時設定。オーナー判断の
// 改訂（2026-09-27、汎用 RDB との整合）により既定は上限なし（`None`）。
// ---------------------------------------------------------------------

/// 既定（`--max-insert-rows` 未指定）では複数行 `VALUES` の行数に上限がない
/// （`batch_limits.max_files_per_batch` を明示的に引き上げて構文段の判定のみを
/// 切り分ける。旧既定値・1,000 行を明らかに超える規模で成功することを固定する。
/// BREAKING CHANGE の外部観測）。
#[test]
fn multi_row_insert_default_has_no_row_count_cap() {
    let path = unique_db_path("insert-multi-row-no-cap");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(
        engine::batch_limits::BatchLimits {
            max_files_per_batch: 1_500,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let ids: Vec<u64> = (1..=1_200).collect();
    let sql = multi_row_sql(TABLE, &ids, "op-no-cap");
    let outcome = core
        .execute_insert_sql(&policy, &sql)
        .expect("no default row count cap must accept rows beyond the old default");
    assert_eq!(outcome.rows_affected, 1_200);
}

/// 明示指定した値へ絞った構成では、その行数以下でも構文段
/// （`sql::allowlist::Parser::parse_insert`）で `54000`（副作用ゼロ）になる。
/// `batch_limits.max_files_per_batch`（既定 64）はここでは変更しないため、
/// 本テストが検出する拒否が `dml_limits`（本 Issue が新設した経路）由来であり
/// `batch_limits`（既存の別上限。Issue #860）由来でないことを、行数を
/// `batch_limits` の既定値未満（3 行）に保つことで切り分ける。
#[test]
fn multi_row_insert_respects_configured_lower_insert_row_limit() {
    let path = unique_db_path("insert-multi-row-lower-dml-limit");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_dml_limits(
        engine::sql::parser::DmlLimits {
            max_insert_rows_per_statement: Some(
                std::num::NonZeroUsize::new(2).expect("2 is nonzero"),
            ),
            ..engine::sql::parser::DmlLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2, 3], "op-lower-insert-limit");
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("row count over the configured lower limit must be rejected");
    assert_eq!(err.wire_code(), "54000");

    // 副作用ゼロ: 台帳未消費（同一 operation_id が上限内の行数で再送できる）。
    let follow_up_sql = multi_row_sql(TABLE, &[1, 2], "op-lower-insert-limit");
    let outcome = core
        .execute_insert_sql(&policy, &follow_up_sql)
        .expect("operation_id must be unused after the rejected over-limit statement");
    assert_eq!(outcome.rows_affected, 2);
}

/// 明示指定した値へ引き上げた構成では、旧既定値超過の行数でも 1 文の複数行
/// `VALUES` として成功する。`batch_limits.max_files_per_batch`（既定 64）も
/// 同時に引き上げないと Issue #860 の別上限で `54000` になる
/// （`multi_row_insert_over_batch_limits_row_count_is_rejected_with_54000` が
/// 示す既存契約）ため、本テストは両方を引き上げて `dml_limits` 側の引き上げが
/// 実際に効いていることを確認する。
#[test]
fn multi_row_insert_respects_configured_higher_insert_row_limit() {
    let path = unique_db_path("insert-multi-row-higher-dml-limit");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_dml_limits(engine::sql::parser::DmlLimits {
            max_insert_rows_per_statement: Some(
                std::num::NonZeroUsize::new(1_500).expect("1_500 is nonzero"),
            ),
            ..engine::sql::parser::DmlLimits::default()
        })
        .with_batch_limits(engine::batch_limits::BatchLimits {
            max_files_per_batch: 1_500,
            ..engine::batch_limits::BatchLimits::default()
        });
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let ids: Vec<u64> = (1..=1_200).collect();
    let sql = multi_row_sql(TABLE, &ids, "op-higher-insert-limit");
    let outcome = core
        .execute_insert_sql(&policy, &sql)
        .expect("row count within the configured higher limit must succeed");
    assert_eq!(outcome.rows_affected, 1_200);
}

// ---------------------------------------------------------------------
// 受け入れ条件③: USING OPERATION_ID 1 つ・台帳照合（RECOVER-11(a) 行順依存性を含む）
// ---------------------------------------------------------------------

#[test]
fn multi_row_insert_writes_all_rows_under_one_operation_id() {
    let (core, path) = open_engine("insert-multi-row-writes-all");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2, 3], "op-write-all");
    let outcome = core
        .execute_insert_sql(&policy, &sql)
        .expect("multi-row insert succeeds");
    assert_eq!(outcome.rows_affected, 3);
}

#[test]
fn multi_row_insert_resend_same_content_is_23505() {
    let (core, path) = open_engine("insert-multi-row-resend-same");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2, 3], "op-resend-same");
    core.execute_insert_sql(&policy, &sql)
        .expect("first insert succeeds");

    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("identical resend must fail");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn multi_row_insert_resend_different_row_order_is_content_mismatch() {
    // RECOVER-11(a): 内容照合ハッシュは全行の正規化バイト列を「行順のまま」
    // 連結する契約のため、同一 operation_id・同一行集合でも行順が異なれば
    // 内容不一致として 22023 になる（23505 にはならない）。
    let (core, path) = open_engine("insert-multi-row-resend-reordered");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let first_sql = multi_row_sql(TABLE, &[1, 2, 3], "op-reorder");
    core.execute_insert_sql(&policy, &first_sql)
        .expect("first insert succeeds");

    // 別テーブルへの影響を避けるため id 集合は同一のまま順序だけを入れ替える。
    // 実書き込みは行われない想定（再送は台帳照合で拒否される）。
    let reordered_sql = multi_row_sql(TABLE, &[2, 1, 3], "op-reorder");
    let err = core
        .execute_insert_sql(&policy, &reordered_sql)
        .expect_err("reordered resend must be classified as content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn multi_row_insert_resend_different_content_is_22023() {
    let (core, path) = open_engine("insert-multi-row-resend-diff-content");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let first_sql = multi_row_sql(TABLE, &[1, 2, 3], "op-diff");
    core.execute_insert_sql(&policy, &first_sql)
        .expect("first insert succeeds");

    let second_sql = multi_row_sql(TABLE, &[4, 5, 6], "op-diff");
    let err = core
        .execute_insert_sql(&policy, &second_sql)
        .expect_err("different content resend must fail");
    assert_eq!(err.wire_code(), "22023");
}

// ---------------------------------------------------------------------
// TABLE-12・RLS-9: バッチ内 id 重複・テナント境界
// ---------------------------------------------------------------------

#[test]
fn multi_row_insert_rejects_duplicate_id_within_batch() {
    let (core, path) = open_engine("insert-multi-row-dup-id");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 1, 2], "op-dup-id");
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("duplicate id within the batch must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 行ストア・台帳のいずれも変更されていない（単一 write
    // トランザクション内で id 重複検出が行ストアへ触れる前に完結する契約。
    // `tenant::insert_typed_rows_unchecked` ドキュメント参照）。
    assert!(
        read_back_ids(&core, "tenant-a").is_empty(),
        "no row must persist after a within-batch id conflict"
    );
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-dup-id"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn multi_row_insert_id_collision_with_existing_row_rejects_whole_statement() {
    // 受け入れ基準③: バッチ内の 1 行（k>0）が既存テナント行と衝突する場合でも
    // 文全体が単一 write トランザクションのまま原子的に拒否され、他の行が
    // 部分的に永続しないことを固定する（TABLE-12・RECOVER-10）。
    let (core, path) = open_engine("insert-multi-row-existing-collision");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    // id=2 を先に単独 INSERT しておき、後続バッチの 2 行目（k=1）と衝突させる。
    let seed_sql = multi_row_sql(TABLE, &[2], "op-seed");
    core.execute_insert_sql(&policy, &seed_sql)
        .expect("seed insert succeeds");

    let sql = multi_row_sql(TABLE, &[1, 2, 3], "op-collide");
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("collision with an existing row must reject the whole statement");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: id=1・id=3 は永続されず、可視な行は seed の id=2 のみ。
    let mut ids = read_back_ids(&core, "tenant-a");
    ids.sort_unstable();
    assert_eq!(ids, vec![2], "only the pre-seeded row must remain visible");
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-collide"),
        LedgerLookup::NotRecorded
    );

    // 衝突を解消（id=2 を除外）したうえで同じ operation_id を再送すると成功する
    // （台帳が拒否時点で未記録のままだったことの逆方向確認）。
    let retry_sql = multi_row_sql(TABLE, &[1, 3], "op-collide");
    let outcome = core
        .execute_insert_sql(&policy, &retry_sql)
        .expect("retry with the same operation_id must succeed once the conflict is resolved");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn multi_row_insert_other_tenant_same_id_succeeds() {
    let path = unique_db_path("insert-multi-row-other-tenant");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&row_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");

    let sql = multi_row_sql(TABLE, &[1, 2, 3], "op-shared-ids");
    core.execute_insert_sql(&tenant_a, &sql)
        .expect("tenant-a multi-row insert succeeds");
    core.execute_insert_sql(&tenant_b, &sql)
        .expect("tenant-b multi-row insert with same ids/operation_id must also succeed");
}

// ---------------------------------------------------------------------
// ファイル形との非併用（42601）・列数不一致行（42601）
// ---------------------------------------------------------------------

#[test]
fn multi_row_values_on_file_form_table_is_rejected_42601() {
    let path = unique_db_path("insert-multi-row-file-form-rejected");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&file_schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let policy = ctx("tenant-a");

    let sql = "INSERT INTO chunks (path, body) VALUES ('a.txt', 'hello'), ('b.txt', 'world') \
               USING OPERATION_ID 'op-file-multi'";
    let err = core
        .execute_insert_sql(&policy, sql)
        .expect_err("multi-row VALUES must not be accepted for file-form INSERT");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn multi_row_insert_rejects_row_with_mismatched_literal_count() {
    let (core, path) = open_engine("insert-multi-row-literal-count-mismatch");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    // 2 行目のリテラル数が列数（3）と一致しない。
    let sql =
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja'), (2, '[2.0,0.0]') \
               USING OPERATION_ID 'op-mismatch'";
    let err = core
        .execute_insert_sql(&policy, sql)
        .expect_err("row with mismatched literal count must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// ---------------------------------------------------------------------
// Issue #860（SQL/NoSQL 機能パリティ）: `self.batch_limits`（INDEX-4）が
// NoSQL 表層 `rows[]`（`execute_bound_insert_in_session`）と同一上限を共有する。
// ---------------------------------------------------------------------

#[test]
fn multi_row_insert_over_batch_limits_row_count_is_rejected_with_54000() {
    // `sql::parser::MAX_INSERT_ROWS_PER_STATEMENT`（実装既定値 1,000）を超えない
    // 行数（3 行）でも、運用者が絞った `self.batch_limits.max_files_per_batch`
    // （ここでは 2）を超えれば NoSQL 表層と同じ `54000` で拒否される
    // （`sql_insert_batch_public_api.rs::execute_bound_insert_in_session_row_count_limit_precedes_bind`
    // と同型の上限。SQL 表層の複数行 VALUES が `batch_limits` を迂回しないことを固定）。
    let (core, path) = open_engine_with_limits(
        "insert-multi-row-batch-limits-row-count",
        engine::batch_limits::BatchLimits {
            max_files_per_batch: 2,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2, 3], "op-batch-limit-rows");
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("row count over batch_limits.max_files_per_batch must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn multi_row_insert_within_batch_limits_row_count_succeeds() {
    // 上限ちょうど（2 行）は成功する（境界値確認）。
    let (core, path) = open_engine_with_limits(
        "insert-multi-row-batch-limits-row-count-ok",
        engine::batch_limits::BatchLimits {
            max_files_per_batch: 2,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2], "op-batch-limit-rows-ok");
    let outcome = core
        .execute_insert_sql(&policy, &sql)
        .expect("row count at the batch_limits boundary must succeed");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn multi_row_insert_over_batch_limits_total_bytes_is_rejected_with_54000() {
    let (core, path) = open_engine_with_limits(
        "insert-multi-row-batch-limits-bytes",
        engine::batch_limits::BatchLimits {
            max_batch_total_bytes: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2], "op-batch-limit-bytes");
    let err = core.execute_insert_sql(&policy, &sql).expect_err(
        "batch total byte size over batch_limits.max_batch_total_bytes must be rejected",
    );
    assert_eq!(err.wire_code(), "54000");
}

// ---------------------------------------------------------------------
// INDEX-4 ②（1 行あたりバイト量）・④（バッチ総チャンク数）: SQL 表層の
// 複数行 VALUES が NoSQL 表層 rows[]（NOSQL-6・判定6）と同一の判定本体
// （`EngineCore::validate_insert_batch_byte_and_chunk_limits`）を共有すること
// の確定化（Issue #863。`tests/sql_insert_batch_public_api.rs`・
// `nosql6_insert.rs` の対応する ②④ テストと同型）。
// ---------------------------------------------------------------------

#[test]
fn multi_row_insert_over_batch_limits_row_body_bytes_is_rejected_with_54000_and_no_side_effects() {
    let (core, path) = open_engine_with_limits(
        "insert-multi-row-batch-limits-row-body",
        engine::batch_limits::BatchLimits {
            max_file_body_bytes: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let sql = multi_row_sql(TABLE, &[1, 2], "op-batch-limit-row-body");
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("single row byte size over batch_limits.max_file_body_bytes must be rejected");
    assert_eq!(err.wire_code(), "54000");

    // 副作用ゼロ: 書き込み txn へ到達しないため行・台帳とも変更されない。
    assert!(read_back_ids(&core, "tenant-a").is_empty());
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-batch-limit-row-body"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn multi_row_insert_over_batch_limits_chunk_total_is_rejected_with_54000_and_no_side_effects() {
    let (core, path) = open_engine_with_limits(
        "insert-multi-row-batch-limits-chunks",
        engine::batch_limits::BatchLimits {
            max_batch_chunks: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    // 行形は「1 行 = 1 チャンク」に読み替える契約（モジュール doc「既知の制約」
    // 参照）。2 行の投入で `max_batch_chunks = 1` を超過させる。
    let sql = multi_row_sql(TABLE, &[1, 2], "op-batch-limit-chunks");
    let err = core.execute_insert_sql(&policy, &sql).expect_err(
        "batch row count (as chunk count) over batch_limits.max_batch_chunks must be rejected",
    );
    assert_eq!(err.wire_code(), "54000");

    assert!(read_back_ids(&core, "tenant-a").is_empty());
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-batch-limit-chunks"),
        LedgerLookup::NotRecorded
    );
}

// ---------------------------------------------------------------------
// 受け入れ条件④（台帳キー空間の共有）: 複数行 → 単一行の逆方向再送判定
// ---------------------------------------------------------------------

#[test]
fn multi_row_insert_then_single_row_same_operation_id_is_content_mismatch() {
    // 複数行 INSERT で記録した operation_id へ、単一行 INSERT で同じ
    // operation_id を再送すると内容不一致（22023）になる（単一行と複数行が
    // 同一の `(tenant, table, operation_id)` 台帳キー空間を共有することの
    // 逆方向確認。`execute_insert_sql_single_row_behavior_is_unchanged` の
    // 順方向確認と対になる）。
    let (core, path) = open_engine("insert-multi-row-then-single-row-key-space");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let multi_sql = multi_row_sql(TABLE, &[1, 2], "op-key-space");
    core.execute_insert_sql(&policy, &multi_sql)
        .expect("multi-row insert succeeds");

    let single_sql = multi_row_sql(TABLE, &[3], "op-key-space");
    let err = core
        .execute_insert_sql(&policy, &single_sql)
        .expect_err("single-row resend with the same operation_id must fail");
    assert_eq!(err.wire_code(), "22023");

    // 副作用ゼロ: id=3 は永続されない。
    let mut ids = read_back_ids(&core, "tenant-a");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}
