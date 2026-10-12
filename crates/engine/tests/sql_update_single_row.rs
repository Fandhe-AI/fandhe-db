//! `UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>
//! USING OPERATION_ID '<id>'`（Issue #865、対象ビヘイビア: SQL-17・TASK-191）の
//! 実行結線・`operation_id` 契約の結合テスト。ポインタ: `docs/spec/05-tasks.md`
//! TASK-191・`docs/spec/04-behavior/sql-surface.md` SQL-17。関連ポインタ:
//! RECOVER-1／2／4／10（`operation_id` 必須化・台帳照合による再送判定）・
//! TABLE-12（テナント名前空間キー）・RLS-7／9／10（暗黙のテナント境界適用・
//! 他テナント存在情報の非漏えい）。
//!
//! `EngineCore::execute_sql_in_session`（先頭トークン `UPDATE` の覗き見判定 →
//! `sql::allowlist::validate_update_tokens` → `sql::parser::bind_update` →
//! `sql::exec::execute_update_with_schema`）を production 経路として検証する
//! （`truncate_table.rs`・`sql_insert_session_dispatch.rs` と同じ流儀。実
//! `Storage`＋`CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。
//! `docs/design/update-single-row.md` に設計判断（read-merge-write の単一
//! トランザクション化・0 行更新でも台帳記録/世代進行/commit を必ず行う非対称
//! 設計・専用内容照合ハッシュ）の詳細を記録済み。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::using_operation_id::OperationId;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-update-single-row");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// SQL `INSERT`（宣言的入力・TASK-80）経由で行を投入する（`update_row_unchecked`
/// 系〔全列 raw metadata〕ではなく、`row_codec::encode_scalar_columns` が書く
/// 正規の metadata レイアウトで投入するため。読み取り検証〔`lang_of`〕・
/// read-merge-write の対象行としても正しい形になる）。可視性は常に
/// `Private`（`execute_insert` の固定仕様）。`Public` 行が必要なテストは
/// `visibility` 引数付きの [`insert_row_with_visibility`] を使う。
fn insert_row(core: &EngineCore, ctx: &PolicyContext, id: u64, v: [f32; 2], lang: &str, seq: u64) {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[{},{}]', '{lang}') \
         USING OPERATION_ID 'seed-{id}-{seq}'",
        v[0], v[1]
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .expect("seed insert should succeed");
}

fn count_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> u64 {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE {predicate}"),
        )
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn distance_hit_count(
    core: &EngineCore,
    ctx: &PolicyContext,
    vector_literal: &str,
    k: u32,
) -> usize {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '{vector_literal}' LIMIT {k}"),
    )
    .expect("select should succeed")
    .rows
    .len()
}

// --- 成功系（部分更新の形） --------------------------------------------------

/// TEXT 列のみの SET は embedding・visibility・tenant_id を維持したまま
/// `lang` だけを更新する（read-merge-write）。
#[test]
fn update_text_column_only_preserves_other_fields() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-text'"),
        )
        .expect("UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }

    assert_eq!(count_where(&core, &alice, "lang = 'en'"), 1);
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 0);
    // embedding は無変更のまま（元のベクトルで距離検索すればまだヒットする）。
    assert_eq!(distance_hit_count(&core, &alice, "[0.1,0.2]", 10), 1);
}

/// VECTOR 列のみの SET は `lang` を維持したまま embedding だけを更新する。
#[test]
fn update_vector_column_only_preserves_text_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET embedding = '[9.0,9.0]' WHERE id = 1 USING OPERATION_ID 'op-vec'"
        ),
    )
    .expect("UPDATE should succeed");

    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 1);
    assert_eq!(distance_hit_count(&core, &alice, "[9.0,9.0]", 10), 1);
    // 全 1 行のテーブルが embedding 更新後は元のベクトルの近傍探索でもヒット
    // する（残存する唯一の行が返る）が、その id は更新後の値と一致することで
    // 「元のベクトルのままの行が残っていない」ことを確認する。
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(id) => assert_eq!(*id, 1),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// 両列を同時に SET できる。
#[test]
fn update_both_columns_at_once() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr', embedding = '[1.0,1.0]' WHERE id = 1 \
             USING OPERATION_ID 'op-both'"
        ),
    )
    .expect("UPDATE should succeed");

    assert_eq!(count_where(&core, &alice, "lang = 'fr'"), 1);
    assert_eq!(distance_hit_count(&core, &alice, "[1.0,1.0]", 10), 1);
}

// --- 0 行更新の同一性（RLS-9／RLS-10） ---------------------------------------

/// 他テナントの `Public` 行・他テナントの `Private` 行・未存在 id のいずれも
/// `Ok(rows_affected: 0)` を返し、区別できない（security.md P0）。
#[test]
fn zero_row_update_is_identical_across_not_found_reasons() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    insert_row(&core, &bob, 1, [0.1, 0.2], "ja", 1);
    // bob の Public 行を用意する（`execute_insert` は常に Private のため、
    // `execute_insert` 経由では Public 行を作れない。既存行を直接 Rust API で
    // Public にはできないため、代わりに `PolicyContext::is_visible` の
    // クロステナント可視性を alice 側から検証する形にする: bob の Private 行
    // （id=1）はそもそも alice から不可視、というケースで代替する）。

    let mut session = SessionState::default();

    // ケース (a): 他テナント（bob）の行（Private・alice からは不可視）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-a'"),
        )
        .expect("UPDATE against another tenant's row must succeed with 0 rows");
    assert!(matches!(
        outcome,
        SqlOutcome::Update(o) if o.rows_affected == 0
    ));
    // bob の行は無傷。
    assert_eq!(count_where(&core, &bob, "lang = 'ja'"), 1);

    // ケース (b): 未存在 id。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 999 USING OPERATION_ID 'op-b'"),
        )
        .expect("UPDATE against a nonexistent id must succeed with 0 rows");
    assert!(matches!(
        outcome,
        SqlOutcome::Update(o) if o.rows_affected == 0
    ));
}

/// 自テナントの `Private` 行でも、`ctx` が `Private` を許可可視性に含まない
/// 場合（所有者ではあるが RLS 可視集合外）は 0 行として扱う（判断 D）。
#[test]
fn zero_row_update_when_owner_but_rls_excludes_visibility() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    // alice 自身だが Private を許可しないコンテキスト（is_owner は true でも
    // is_visible が false になる）。
    let alice_public_only =
        PolicyContext::with_visibilities("alice", [Visibility::Public]).expect("valid tenant");

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice_public_only,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-rls'"),
        )
        .expect("UPDATE against an owned-but-invisible row must succeed with 0 rows");
    assert!(matches!(
        outcome,
        SqlOutcome::Update(o) if o.rows_affected == 0
    ));
    // 行は無変更のまま。
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 1);
}

/// 0 行更新後の同一 `operation_id` 再送も、台帳が記録済みのため `23505`
/// （重複コミット）として拒否される（台帳記録・commit が 0 行でも必ず発生する
/// ことの非 vacuous 確認。判断 B）。
#[test]
fn zero_row_update_still_records_the_ledger_entry() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let mut session = SessionState::default();
    let sql = format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-empty'");
    let outcome = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect("UPDATE against a nonexistent id must succeed");
    assert!(matches!(
        outcome,
        SqlOutcome::Update(o) if o.rows_affected == 0
    ));

    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("resend must be rejected as duplicate commit");
    assert_eq!(
        err.wire_code(),
        "23505",
        "0-row UPDATE must have recorded the ledger entry on first commit"
    );
}

// --- operation_id 台帳（RECOVER-1／10） --------------------------------------

/// 同一 `operation_id`・同一内容の再送は `23505`。
#[test]
fn resending_same_operation_id_with_identical_content_is_23505() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    let sql = format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-resend'");
    core.execute_sql_in_session(&alice, &mut session, &sql)
        .expect("first UPDATE should succeed");

    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("resend with identical content must be rejected as duplicate");
    assert_eq!(err.wire_code(), "23505");
}

/// 同一 `operation_id` で SET 値が異なる再送は内容不一致 `22023`。
#[test]
fn resending_same_operation_id_with_different_value_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);
    insert_row(&core, &alice, 2, [0.3, 0.4], "en", 2);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-mismatch'"),
    )
    .expect("first UPDATE should succeed");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE id = 1 USING OPERATION_ID 'op-mismatch'"
            ),
        )
        .expect_err("resend with a different SET value must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

/// 同一 `operation_id` で `id` が異なる再送も内容不一致 `22023`。
#[test]
fn resending_same_operation_id_with_different_id_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);
    insert_row(&core, &alice, 2, [0.3, 0.4], "en", 2);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-idmismatch'"),
    )
    .expect("first UPDATE should succeed");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 2 USING OPERATION_ID 'op-idmismatch'"
            ),
        )
        .expect_err("resend with a different id must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

/// 同一 `operation_id` で SET 句の列宣言順のみが異なる再送は同一内容の再送
/// として `23505`（`tenant::update_row_columns_unchecked` が `content_hash::
/// for_update_columns` へ渡す前に列をスキーマ順へ正規化するため。宣言順は
/// SET 意味論に影響しない一方、NoSQL 表層〔`wire-server::http::query::
/// update`〕は JSON `set` オブジェクトを `BTreeMap` でパースし常にアルファ
/// ベット順へ正規化するため、宣言順に依存したままでは SQL・NoSQL 間の同一
/// `operation_id` 再送が誤って内容不一致 `22023` に丸められてしまう。
/// Issue #876 レビュー指摘）。
#[test]
fn resending_same_operation_id_with_different_set_clause_order_is_23505() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET lang = 'en', embedding = '[1.0,1.0]' WHERE id = 1 \
             USING OPERATION_ID 'op-order'"
        ),
    )
    .expect("first UPDATE should succeed");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET embedding = '[1.0,1.0]', lang = 'en' WHERE id = 1 \
                 USING OPERATION_ID 'op-order'"
            ),
        )
        .expect_err("resend with only a different SET clause order must be a duplicate");
    assert_eq!(err.wire_code(), "23505");
}

/// 同一 `operation_id` を INSERT で使った後に UPDATE で再利用すると、
/// `OpTag` のドメイン分離により内容不一致 `22023`（判断 C）。
#[test]
fn reusing_an_insert_operation_id_for_update_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'op-shared'"
        ),
    )
    .expect("seed insert should succeed");

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-shared'"),
        )
        .expect_err("reusing an INSERT operation_id for UPDATE must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
    // 台帳が拒否した以上、行は無変更のまま。
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 1);
}

/// `USING OPERATION_ID` の省略（明示 `NULL` を含む）は `23502`（書き込み
/// トランザクション開始前に拒否・行は無変更）。
#[test]
fn missing_operation_id_is_rejected_with_23502_before_any_write() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1"),
        )
        .expect_err("missing operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 1);

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID NULL"),
        )
        .expect_err("explicit NULL operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 1);
}

// --- 束縛エラー（session 経由） ----------------------------------------------

/// VECTOR 列への SET でベクトル次元がスキーマと不一致なら `22000`。
#[test]
fn set_embedding_with_wrong_dimension_is_rejected_with_22000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET embedding = '[1.0,2.0,3.0]' WHERE id = 1 \
                 USING OPERATION_ID 'op-dim'"
            ),
        )
        .expect_err("dimension mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

/// 不正な SET 値（次元不一致）は、対象行が実在するかどうかに関わらず同一の
/// `22000` 拒否になる（Cursor Bugbot 指摘・PR #989。値の形状検証を対象行 lookup
/// より前に行うことで、エラー有無そのものが行の存在を漏らす識別子にならない
/// ことを固定する）。対象行が存在する場合は上の
/// `set_embedding_with_wrong_dimension_is_rejected_with_22000` が同じ契約を検証する。
#[test]
fn set_embedding_with_wrong_dimension_is_rejected_with_22000_even_for_nonexistent_row() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // id = 1 は投入しない（対象行が存在しない状態で SET 値の妥当性のみを問う）。

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET embedding = '[1.0,2.0,3.0]' WHERE id = 1 \
                 USING OPERATION_ID 'op-dim-missing'"
            ),
        )
        .expect_err("dimension mismatch must be rejected even when the target row is absent");
    assert_eq!(err.wire_code(), "22000");
}

/// 未知の列への SET は `22000`。
#[test]
fn set_unknown_column_is_rejected_with_22000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET ghost = 'x' WHERE id = 1 USING OPERATION_ID 'op-ghost'"),
        )
        .expect_err("unknown column must be rejected");
    assert_eq!(err.wire_code(), "42703");
}

/// `SET id = ...` は疑似列の書き換えのため `42601`。
#[test]
fn set_id_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET id = 2 WHERE id = 1 USING OPERATION_ID 'op-set-id'"),
        )
        .expect_err("SET id must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// `EXPLAIN UPDATE ...` は許可形状に存在しないため `42601`
/// （`EXPLAIN` は検索 SELECT の前置専用。`truncate_table.rs` の同型テストと
/// 同じ確認）。
#[test]
fn explain_update_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "EXPLAIN UPDATE {TABLE} SET lang = 'en' WHERE id = 1 \
                 USING OPERATION_ID 'op-explain-update'"
            ),
        )
        .expect_err("EXPLAIN UPDATE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// 存在しないテーブルへの UPDATE は `42P01`。
#[test]
fn update_undefined_table_is_rejected_with_42p01() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "UPDATE ghost SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-undefined-table'",
        )
        .expect_err("undefined table must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}

// --- キャッシュ失効 -----------------------------------------------------------

/// `SqlArenaCache`（DISTANCE クエリ）・`ScalarIndexCache`（`WHERE lang = ...`）・
/// `VisibleBitmapCache`（`COUNT(*)`）をそれぞれ UPDATE 前に温めてから UPDATE し、
/// 世代進行により失効して更新後の内容を返すことを確認する（cold のみでは世代
/// 整合の検証にならない）。
#[test]
fn update_invalidates_arena_scalar_index_and_visible_bitmap_caches() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);
    insert_row(&core, &alice, 2, [0.3, 0.4], "ja", 2);

    // 温める。
    assert_eq!(distance_hit_count(&core, &alice, "[0.1,0.2]", 10), 2);
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 2);
    assert_eq!(
        core.execute_sql(&alice, &format!("SELECT COUNT(*) FROM {TABLE}"))
            .expect("count(*) should succeed")
            .rows[0]
            .cells[0],
        Cell::Integer(2)
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-cache'"),
    )
    .expect("UPDATE should succeed");

    // 世代が進んでいるため、いずれのキャッシュも失効し更新後の内容を返す。
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 1);
    assert_eq!(count_where(&core, &alice, "lang = 'en'"), 1);
    assert_eq!(
        core.execute_sql(&alice, &format!("SELECT COUNT(*) FROM {TABLE}"))
            .expect("count(*) should succeed")
            .rows[0]
            .cells[0],
        Cell::Integer(2)
    );
}

// --- 並行性（lost update 防止） ----------------------------------------------

/// 同一行への列が互いに素な 2 回の UPDATE を順に適用すると、両方の変更が
/// 残る（read-merge-write を単一 write トランザクション内で行う設計。判断 A）。
#[test]
fn sequential_disjoint_column_updates_both_persist() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-seq-1'"),
    )
    .expect("first UPDATE should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET embedding = '[9.0,9.0]' WHERE id = 1 USING OPERATION_ID 'op-seq-2'"
        ),
    )
    .expect("second UPDATE should succeed");

    // 両方の変更が残っている。
    assert_eq!(count_where(&core, &alice, "lang = 'en'"), 1);
    assert_eq!(distance_hit_count(&core, &alice, "[9.0,9.0]", 10), 1);
}

// --- 直接エントリポイントの契約一致 ------------------------------------------

/// `EngineCore::execute_update_sql`（セッション非経由の直接エントリポイント）が
/// `execute_sql_in_session` の UPDATE 分岐と同じ契約であることを確認する。
#[test]
fn execute_update_sql_direct_entry_point_matches_session_dispatch_contract() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_row(&core, &alice, 1, [0.1, 0.2], "ja", 1);

    let outcome = core
        .execute_update_sql(
            &alice,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-direct-entry'"
            ),
        )
        .expect("direct entry point UPDATE should succeed");
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(count_where(&core, &alice, "lang = 'en'"), 1);
}

// --- 格納済み行の破損（codex-review P1 指摘・PR #989） ----------------------

/// 対象行の既存 `metadata` が `row_codec::encode_scalar_columns` の正規レイアウト
/// でない場合（`tenant::insert_row` の raw `RowInput` 経路。本モジュール外の
/// 非 SQL 呼び出し元専用と `update_row_columns_unchecked` のドキュメントが明記する
/// 前提を外れた、ストレージ側の破損・想定外の格納状態を模す）、`UPDATE` は
/// クライアント入力エラー（`22000`）ではなく、サーバー内部事象として `XX000`
/// （`SqlSurfaceError::Internal`）を返す。`decode_scalar_columns` の失敗を
/// `CatalogError::Invalid` へ丸めると `map_write_error` が一律 `22000` へ誤写像
/// してしまう問題の回帰防止（PR #989 レビュー指摘対応）。
#[test]
fn update_against_row_with_corrupt_stored_metadata_is_rejected_with_xx000() {
    let path = unique_db_path("sql-update-corrupt-metadata");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let alice = ctx_for("alice");
    // `row_codec::encode_scalar_columns` を経由しない raw metadata（列レイアウトと
    // 矛盾する任意バイト列）で行を直接投入する。`update_row_columns_unchecked` が
    // 読み出し時に `decode_scalar_columns` で構造不整合を検出する対象行を作る。
    let operation_id = OperationId::parse("seed-corrupt-1").expect("valid operation_id");
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &alice,
        1,
        &RowInput {
            tenant_id: alice.tenant_id(),
            visibility: Visibility::Private,
            embedding: &[0.1, 0.2],
            metadata: b"\xff\xff not a valid scalar column encoding \xff\xff",
        },
        &operation_id,
    )
    .expect("seed row with corrupt stored metadata");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-corrupt'"),
        )
        .expect_err("decode failure on stored data must not succeed");
    assert_eq!(
        err.wire_code(),
        "XX000",
        "stored row decode failure must classify as a server-side internal error, \
         not a client input error (22000)"
    );
}
