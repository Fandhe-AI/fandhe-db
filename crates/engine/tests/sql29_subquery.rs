//! サブクエリ（`IN (SELECT ...)`・`EXISTS (SELECT ...)`）の結合テスト
//! （Issue #927・SQL-29 (a)・RLS-10 (b)・TASK-213）。`tests/sql_where_or.rs`
//! と同じ流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋
//! `CpuScalarProvider`、`EngineCore::execute_sql`／`execute_sql_in_session` を
//! production 経路として検証）。
//!
//! スコープ（実装既定値。`docs/design/sql-subquery.md` 参照）:
//! - 対応: WHERE の `<col> IN (SELECT ...)`・`EXISTS (SELECT ...)`。内側は
//!   `SELECT ... FROM <table> [WHERE ...] LIMIT <n>`（広域取得）のみ。
//! - 対象外（このファイルでは拒否の確認のみ）: スカラー比較サブクエリ・投影
//!   位置のサブクエリ・内側が集計／ランキング付き検索 SELECT・内側 `LIMIT`
//!   省略・相関サブクエリ・拡張クエリプロトコル（Parse/Bind）経由・
//!   `NOT IN`/`NOT EXISTS`。

use engine::catalog::{ColumnDef, ColumnType, EnumTypeDef, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};
use std::sync::Arc;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";
const ALLOWED_LANGS: &str = "allowed_langs";
const VISITS: &str = "visits";

const ENUM_TYPE: &str = "mood";

fn enum_labels() -> Vec<String> {
    vec!["happy".to_string(), "sad".to_string()]
}

fn docs_schema(mood_def: Arc<EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        DOCS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            // INTEGER/BIGINT 列を対象にした `IN (SELECT ...)`（レビュー指摘対応。
            // `sql::subquery::cell_to_equality_predicate` の `Cell::SignedInteger`
            // 分岐が `WherePredicate::Equality` 経由で常に失敗していたバグの
            // 回帰テスト用。`insert_doc` は指定しないため NULL 許容にする）。
            ColumnDef::new("priority", ColumnType::BigInt, true),
            // ENUM 列を対象にした `IN (SELECT ...)`（PR #1103 追加 codex-review
            // P1 指摘対応。`sql::subquery::validate_in_target_column` は ENUM
            // 列を対象として許可するが、内側の投影値が語彙外のラベルを含む
            // 場合の回帰テスト用。`insert_doc` は指定しないため NULL 許容
            // にする）。
            ColumnDef::new("mood", ColumnType::Enum(mood_def), true),
            // BOOLEAN 列を対象にした `IN (SELECT ...)`（PR #1103 再々レビュー
            // codex-review P1 指摘対応: 対象列・内側投影列の型組合せ検証の
            // 正当な組合せ側〔BOOLEAN↔BOOLEAN〕の回帰テスト用。`insert_doc`
            // は指定しないため NULL 許容にする）。
            ColumnDef::new("active", ColumnType::Boolean, true),
        ],
    )
}

fn allowed_langs_schema() -> TableSchema {
    TableSchema::new(
        ALLOWED_LANGS,
        vec![ColumnDef::new("lang", ColumnType::Text, false)],
    )
}

const PRIORITIES: &str = "priorities";

fn priorities_schema() -> TableSchema {
    TableSchema::new(
        PRIORITIES,
        vec![ColumnDef::new("priority", ColumnType::BigInt, false)],
    )
}

fn visits_schema() -> TableSchema {
    TableSchema::new(
        VISITS,
        vec![
            ColumnDef::new("note", ColumnType::Text, false),
            // BOOLEAN↔BOOLEAN の正当な組合せ検証用（PR #1103 再々レビュー
            // codex-review P1 指摘対応）。`insert_visit` は指定しないため
            // NULL 許容にする。
            ColumnDef::new("flag", ColumnType::Boolean, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql29-subquery");
    let storage = Storage::open(&path).expect("open storage");
    let mood_def = storage
        .create_enum_type(ENUM_TYPE, enum_labels())
        .expect("create mood enum type");
    storage
        .create_table(&docs_schema(mood_def))
        .expect("create docs");
    storage
        .create_table(&allowed_langs_schema())
        .expect("create allowed_langs");
    storage
        .create_table(&visits_schema())
        .expect("create visits");
    storage
        .create_table(&priorities_schema())
        .expect("create priorities");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_doc(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {DOCS} (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', '{lang}') \
             USING OPERATION_ID 'doc-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert doc id={id} should succeed: {e:?}"));
}

fn insert_doc_with_priority(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    priority: i64,
) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {DOCS} (id, embedding, lang, priority) VALUES \
             ({id}, '[0.{id},0.1]', '{lang}', {priority}) USING OPERATION_ID 'doc-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert doc id={id} should succeed: {e:?}"));
}

fn insert_doc_with_mood(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, mood: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {DOCS} (id, embedding, lang, mood) VALUES \
             ({id}, '[0.{id},0.1]', '{lang}', '{mood}') USING OPERATION_ID 'doc-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert doc id={id} should succeed: {e:?}"));
}

fn insert_priority(core: &EngineCore, ctx: &PolicyContext, id: u64, priority: i64) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {PRIORITIES} (id, priority) VALUES ({id}, {priority}) \
             USING OPERATION_ID 'priority-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert priority id={id} should succeed: {e:?}"));
}

fn insert_allowed_lang(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {ALLOWED_LANGS} (id, lang) VALUES ({id}, '{lang}') \
             USING OPERATION_ID 'lang-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert allowed_lang id={id} should succeed: {e:?}"));
}

fn insert_visit(core: &EngineCore, ctx: &PolicyContext, id: u64, note: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {VISITS} (id, note) VALUES ({id}, '{note}') USING OPERATION_ID 'visit-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert visit id={id} should succeed: {e:?}"));
}

fn insert_visit_with_flag(core: &EngineCore, ctx: &PolicyContext, id: u64, note: &str, flag: bool) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {VISITS} (id, note, flag) VALUES ({id}, '{note}', {flag}) \
             USING OPERATION_ID 'visit-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert visit id={id} should succeed: {e:?}"));
}

fn insert_doc_with_active(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    active: bool,
) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {DOCS} (id, embedding, lang, active) VALUES \
             ({id}, '[0.{id},0.1]', '{lang}', {active}) USING OPERATION_ID 'doc-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert doc id={id} should succeed: {e:?}"));
}

fn seed_docs(core: &EngineCore, ctx: &PolicyContext) {
    insert_doc(core, ctx, 1, "ja");
    insert_doc(core, ctx, 2, "en");
    insert_doc(core, ctx, 3, "fr");
    insert_doc(core, ctx, 4, "de");
}

fn select_ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn expect_error_code(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> engine::sql::allowlist::SqlSurfaceError {
    core.execute_sql(ctx, sql)
        .expect_err(&format!("sql={sql:?} should be rejected"))
}

// --- IN (SELECT ...) -------------------------------------------------------

#[test]
fn in_subquery_text_column_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");
    insert_allowed_lang(&core, &ctx, 2, "fr");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn in_subquery_bigint_column_in_target_is_rejected() {
    // レビュー指摘の回帰テスト（Issue #927 push 前 Review）: `sql::subquery::
    // cell_to_equality_predicate` が以前は `BIGINT`/`INTEGER` 列（`Cell::
    // SignedInteger`）を `WherePredicate::Equality`（`TEXT`/`ENUM` 列専用）へ
    // 変換していたため、`<BIGINT/INTEGER 列> IN (SELECT ...)` は常に「TEXT
    // 列でない」で拒否されていた（ドキュメント上の「対応済み」表明と実装が
    // 矛盾する機能バグ）。`INTEGER`/`BIGINT` 列の等価比較自体がこのリポでは
    // まだ実装されていない（レーン A。`sql::udf_call::bind_expr_in` 参照）
    // ため、実装を追加するのではなく `IN` 対象値の対応型を `TEXT`/`BOOLEAN`
    // のみへ縮小し、`INTEGER`/`BIGINT` は明示的に `22000` へ倒したことを
    // 検証する（`docs/design/sql-subquery.md`「`IN` 対象値の型」節参照）。
    // `detail` の内容まで検査するのは、修正前も `Equality` 束縛の「TEXT 列
    // でない」という別経路の偶発的な `InvalidInput` で同じテストが素通り
    // してしまい、レビュー指摘（テストカバレッジなし）への回帰テストとして
    // 機能しなくなるのを防ぐため（`cell_to_equality_predicate` の明示的な
    // 拒否理由であることをこのアサーションで固定する）。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_priority(&core, &ctx, 1, "ja", 10);
    insert_priority(&core, &ctx, 1, 10);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE priority IN (SELECT priority FROM {PRIORITIES} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("subquery IN target")
    ));
}

// 疑似列 `id` を対象にした `IN`／`EXISTS`（`id IN (SELECT id FROM ...)`）は
// 対象外（このリポの既存 WHERE 等価述語〔`WherePredicate::Equality`〕自体が
// 疑似列 `id` を対象にしていないため。`sql::subquery::cell_to_equality_predicate`
// の `Cell::Integer` 分岐は将来の `id` 対応拡張に備えた到達可能コードとして
// 残す）。内側の投影に `id` を書くこと自体は他列と同様に受理される
// （`in_subquery_multi_column_projection_is_rejected` 参照）。

#[test]
fn in_subquery_empty_result_matches_no_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // allowed_langs は空のまま。

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(ids.is_empty());
}

#[test]
fn in_subquery_inside_or_branch_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang = 'de' OR lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 4]);
}

#[test]
fn in_subquery_multi_column_projection_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE id IN (SELECT id, lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

// PR #1103 追加 codex-review P1 指摘の自己点検（EXISTS 側の資源上限修正と
// 同種の問題が IN 側にも無いかの確認）: `IN (SELECT ...)` は投影列が
// ちょうど 1 列であることを要求する契約だが、以前はこれを実行結果からしか
// 検査しておらず、`SELECT *` のような不正な内側クエリでも束縛・全件走査を
// 最後まで終えてから拒否していた。`sql::subquery::execute_inner_scan` は
// 投影列数を実行前（束縛・走査より前）に静的検証するようになった
// （`crates/engine/src/sql/subquery.rs` の単体テスト
// `execute_inner_scan_existence_only_caps_projection_and_row_count` 等
// 参照）。ここでは SQL 表層から見た拒否自体（`SELECT *` を含む）が
// 変わらないことを固定する。
#[test]
fn in_subquery_select_star_projection_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT * FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

// PR #1103 codex-review P1 指摘の回帰テスト（2 スレッド・同一趣旨）:
// `<col> IN (SELECT ...)` の対象列 `<col>` が存在しない・非対応型の場合でも、
// 内側サブクエリの結果が 0 行または NULL のみだと、以前は列名・型検証を
// 一切通らずに空の `Or`（常に偽）へ静かに書き換わり「空結果で成功」して
// いた（列名・型検証は内側の結果行から変換された葉が実際に束縛される時点
// でしか働かなかったため）。内側の結果行数・NULL 有無に関わらず必ず
// `22000` で拒否されることを固定する。

#[test]
fn in_subquery_unknown_column_with_empty_inner_result_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // allowed_langs は空のまま（内側の結果が 0 行）。

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE nonexistent_col IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("unknown column")
    ));
}

#[test]
fn in_subquery_unknown_column_with_null_only_inner_result_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // `priority` は NULL 許容の BIGINT 列（`docs_schema` 参照）。ここでは
    // 明示せず NULL のままにする（内側の結果は NULL のみ）。
    insert_doc(&core, &ctx, 1, "ja");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE nonexistent_col IN \
             (SELECT priority FROM {DOCS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("unknown column")
    ));
}

#[test]
fn in_subquery_unsupported_type_column_with_empty_inner_result_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_priority(&core, &ctx, 1, "ja", 10);
    // priorities は空のまま（内側の結果が 0 行）。修正前はここで `priority`
    // （`BIGINT`）が対象列として一切検証されず、空結果で成功していた。

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE priority IN \
             (SELECT priority FROM {PRIORITIES} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("is not a TEXT/ENUM/BOOLEAN column")
    ));
}

#[test]
fn in_subquery_unsupported_type_column_with_null_only_inner_result_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // `priority` は明示せず NULL のままにする（内側の結果は NULL のみ）。
    insert_doc(&core, &ctx, 1, "ja");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE priority IN \
             (SELECT priority FROM {DOCS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("is not a TEXT/ENUM/BOOLEAN column")
    ));
}

// --- 上限（`IN` 展開の葉数） --------------------------------------------------

// PR #1103 codex-review P1 指摘の回帰テスト（3 スレッド目）: 内側の各行を
// `WherePredicate::Equality` 葉へ展開して `Or` に束ねる件数は、構文解析段の
// `MAX_WHERE_LEAVES`（通常の `WHERE` 述語の葉数上限）とは独立の経路で
// 生成されるため、内側最大可視行数×内側実行回数上限の組合せだけでは通常の
// `WHERE` 述語数上限より大きい評価コストを 1 文から発生させられた
// （DoS。`docs/design/sql-subquery.md` 参照）。
// `sql::subquery::MAX_SUBQUERY_IN_LEAVES` で頭打ちにされることを固定する。
#[test]
fn in_subquery_expansion_leaf_count_exceeds_limit_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc(&core, &ctx, 1, "en");
    // `sql::subquery::MAX_SUBQUERY_IN_LEAVES`（256）を超える件数の異なる
    // `lang` 値を用意し、`IN` の展開がそれをすべて葉へ変換しようとした時点で
    // 資源上限エラーになることを確認する。
    let leaf_count = 300u64;
    for i in 0..leaf_count {
        insert_allowed_lang(&core, &ctx, i, &format!("lang{i}"));
    }

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT {leaf_count}) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::PayloadTooLarge { .. }
    ));
}

// Cursor Bugbot 指摘（Medium）の回帰テスト: `IN` は集合所属の判定であり、
// 同じ値の内側行が何件あっても展開後の述語は 1 個で足りる。以前は内側の
// 行ごとに無条件で葉を積み葉予算（`MAX_SUBQUERY_IN_LEAVES` = 256）を
// 消費していたため、同一ラベルを大量に持つ参照テーブルを `IN` の内側に
// 指定するだけで、実質的な distinct 値が 1 個しかなくても資源上限
// エラー（54000）になり得た（多数の重複値 × `LIMIT` 256 超で誤失敗）。
// 同一値 1,000 件でも成功し、distinct 値 257 件（256 を 1 件超過）でのみ
// 資源上限エラーになることを固定する。

#[test]
fn in_subquery_duplicate_values_do_not_exhaust_leaf_budget() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 0, "ja");
    // `sql::subquery::MAX_SUBQUERY_IN_LEAVES`（256）を大幅に超える件数の
    // 同一値（"ja"）を用意する。重複排除が働かなければ、以前の実装では
    // 257 件目で資源上限エラーになっていた。
    for i in 1..1_000u64 {
        insert_allowed_lang(&core, &ctx, i, "ja");
    }

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT 1000) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1]);
}

#[test]
fn in_subquery_distinct_values_exceeding_limit_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc(&core, &ctx, 1, "en");
    // `sql::subquery::MAX_SUBQUERY_IN_LEAVES`（256）をちょうど 1 件超える
    // distinct 値（257 件）を用意する。重複排除後の distinct 数自体が
    // 上限を超えるため、資源上限エラーになることを確認する（重複排除の
    // 有無に関わらず拒否されるべきケース。上の
    // `in_subquery_duplicate_values_do_not_exhaust_leaf_budget` と対比）。
    let distinct_count = 257u64;
    for i in 0..distinct_count {
        insert_allowed_lang(&core, &ctx, i, &format!("lang{i}"));
    }

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT {distinct_count}) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::PayloadTooLarge { .. }
    ));
}

// PR #1103 追加 codex-review P1 指摘の回帰テスト: 対象列が ENUM の場合、
// 内側の投影値に語彙外のラベルが混ざっていても、それは「一致しない値」
// として展開対象から除外するだけで、文全体を失敗させてはならない
// （除外せず `Equality` 葉として残すと、後段の `DeclarativeFilter::bind`
// が語彙外ラベルを `22000` で拒否し、`IN` が本来「照合不一致」になる
// べき場面で文全体が失敗してしまっていた）。`mood` 列の語彙は
// `enum_labels()`（"happy"・"sad"）。

#[test]
fn in_subquery_enum_target_mixed_vocabulary_matches_only_valid_labels() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_mood(&core, &ctx, 1, "ja", "happy");
    insert_doc_with_mood(&core, &ctx, 2, "en", "sad");
    // 内側の投影値は語彙内（"happy"）・語彙外（"other"）が混在する。
    insert_visit(&core, &ctx, 1, "happy");
    insert_visit(&core, &ctx, 2, "other");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE mood IN (SELECT note FROM {VISITS} LIMIT 100) LIMIT 100"
        ),
    );
    // 語彙外の "other" は展開対象から除外され、"happy" のみが一致する
    // （id=2 の "sad" は一致しない＝エラーにはならず単に不一致）。
    assert_eq!(ids, vec![1]);
}

#[test]
fn in_subquery_enum_target_all_out_of_vocabulary_matches_no_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_mood(&core, &ctx, 1, "ja", "happy");
    insert_doc_with_mood(&core, &ctx, 2, "en", "sad");
    // 内側の投影値はすべて語彙外。
    insert_visit(&core, &ctx, 1, "other");
    insert_visit(&core, &ctx, 2, "unknown");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE mood IN (SELECT note FROM {VISITS} LIMIT 100) LIMIT 100"
        ),
    );
    // 全値が語彙外 ＝ 空の Or（常に偽）。エラーにはならない。
    assert!(ids.is_empty());
}

#[test]
fn in_subquery_enum_target_within_vocabulary_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_mood(&core, &ctx, 1, "ja", "happy");
    insert_doc_with_mood(&core, &ctx, 2, "en", "sad");
    insert_doc_with_mood(&core, &ctx, 3, "fr", "happy");
    // 内側の投影値はすべて語彙内。
    insert_visit(&core, &ctx, 1, "happy");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE mood IN (SELECT note FROM {VISITS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 3]);
}

// PR #1103 再々レビュー codex-review P1 指摘の回帰テスト: `<TEXT 列> IN
// (SELECT id FROM ...)`（内側投影が疑似列 `id`＝`Cell::Integer`）は、以前は
// `Cell::Integer` を無条件に文字列化して `WherePredicate::Equality` へ
// 変換していたため、外側 TEXT 列の値が `id` の文字列表現と偶然一致する
// 行が誤って一致してしまっていた（型の異なる値の暗黙同一視）。内側の
// 結果行数に関わらず、対象列・内側投影列の型組合せを検証し `22000` で
// 拒否することを固定する。

#[test]
fn in_subquery_text_target_against_id_projection_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // 型混同の再現条件: 外側 TEXT 列 `lang` の値が疑似列 `id` の文字列表現
    // （"1"）と偶然一致する行を用意する。
    insert_doc(&core, &ctx, 1, "1");
    insert_allowed_lang(&core, &ctx, 1, "ja");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT id FROM {ALLOWED_LANGS} LIMIT 1) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("subquery projection type is not compatible")
    ));
}

#[test]
fn in_subquery_text_target_against_id_projection_with_empty_inner_result_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc(&core, &ctx, 1, "ja");
    // allowed_langs は空のまま（内側の結果が 0 行）。組合せ検証は内側の
    // 結果行数に依存しないため、0 行でも拒否される。

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT id FROM {ALLOWED_LANGS} LIMIT 1) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("subquery projection type is not compatible")
    ));
}

#[test]
fn in_subquery_boolean_target_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_active(&core, &ctx, 1, "ja", true);
    insert_doc_with_active(&core, &ctx, 2, "en", false);
    insert_doc_with_active(&core, &ctx, 3, "fr", true);
    // 内側投影は BOOLEAN 列（正当な組合せ: BOOLEAN↔BOOLEAN）。
    insert_visit_with_flag(&core, &ctx, 1, "hit", true);

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE active IN (SELECT flag FROM {VISITS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 3]);
}

// --- EXISTS (SELECT ...) ----------------------------------------------------

#[test]
fn exists_subquery_true_keeps_all_visible_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_visit(&core, &ctx, 1, "hit");

    let ids = select_ids(
        &core,
        &ctx,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert_eq!(ids, vec![1, 2, 3, 4]);
}

#[test]
fn exists_subquery_false_excludes_all_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits は空のまま。

    let ids = select_ids(
        &core,
        &ctx,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert!(ids.is_empty());
}

#[test]
fn exists_subquery_combined_with_and_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_visit(&core, &ctx, 1, "hit");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang = 'ja' AND EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1]);
}

// PR #1103 追加 codex-review P1 指摘の回帰テスト: `EXISTS (SELECT ...)` は
// 可視行が 1 件以上存在するかどうかしか使わないため、内側を実質 `LIMIT 1`・
// 投影不要で評価する（`sql::subquery::InnerScanIntent::ExistenceOnly`）。
// 以前はユーザー指定の投影（`SELECT *` 等）・`LIMIT` をそのまま使っていた
// ため、幅広い投影×大きい `LIMIT` の組合せでは可視行があっても
// `execute_scan` の結果バイト上限に達し `EXISTS` 文全体が失敗しえた。
// 大きな TEXT 列を持つ内側 `SELECT *` で、この経路（真・偽・RLS 境界）が
// 資源上限に当たらず正しく動作することを固定する。

fn large_text_value() -> String {
    // 単一セルとしては大きいが、テスト実行時間・メモリを圧迫しない範囲
    // （数百 KB オーダー）の TEXT 値。行数×投影列数に比例してバイト予算を
    // 消費する旧実装では、`LIMIT` が大きいほど資源上限へ近づく設計だった
    // ことの再現に十分な大きさ。
    "x".repeat(500_000)
}

#[test]
fn exists_subquery_select_star_with_large_text_column_succeeds_when_visible_row_exists() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_visit(&core, &ctx, 1, &large_text_value());

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT * FROM {VISITS} LIMIT 9999) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 2, 3, 4]);
}

#[test]
fn exists_subquery_select_star_with_large_text_column_is_false_without_visible_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits は空のまま（可視行なし）。

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT * FROM {VISITS} LIMIT 9999) LIMIT 100"
        ),
    );
    assert!(ids.is_empty());
}

#[test]
fn exists_subquery_select_star_with_large_text_column_is_false_for_other_tenant_only() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    seed_docs(&core, &ctx_a);
    // tenant-b だけが（大きな TEXT 値を持つ）visits 行を持つ。
    insert_visit(&core, &ctx_b, 1, &large_text_value());

    let ids = select_ids(
        &core,
        &ctx_a,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT * FROM {VISITS} LIMIT 9999) LIMIT 100"
        ),
    );
    assert!(
        ids.is_empty(),
        "tenant-a must not observe tenant-b's visits row via EXISTS"
    );
}

// PR #1103 再々レビュー codex-review P1 指摘の回帰テスト: `EXISTS` の内側を
// `InnerScanIntent::ExistenceOnly`（投影を空へ差し替え）で評価する際、投影の
// 差し替えは元の投影を束縛・検証した**後**に行う。差し替えを先に行うと、
// `EXISTS (SELECT <存在しない列> FROM ... LIMIT 1)` のような不正な内側
// クエリが、実際には使わないという理由だけで列検証をすり抜け、可視行の
// 有無だけで成否が決まってしまう（列検証・エラー契約を破る）。可視行の
// 有無に関わらず（0 行・1 行以上のいずれでも）、通常の `SELECT` の
// 未知列と同じ `22000`（`unknown column`）で拒否されることを固定する。

#[test]
fn exists_subquery_unknown_column_projection_is_rejected_without_visible_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits は空のまま（可視行なし）。

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT nonexistent_col FROM {VISITS} LIMIT 1) \
             LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("unknown column")
    ));
}

#[test]
fn exists_subquery_unknown_column_projection_is_rejected_with_visible_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits に可視行が 1 件存在する（行の有無だけでは成功しないことの確認）。
    insert_visit(&core, &ctx, 1, "hit");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT nonexistent_col FROM {VISITS} LIMIT 1) \
             LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("unknown column")
    ));
}

// --- RLS 境界（RECOVER-4 と同型: テナント越境なし） -------------------------

#[test]
fn in_subquery_only_sees_own_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    seed_docs(&core, &ctx_a);
    seed_docs(&core, &ctx_b);
    // tenant-b だけが 'ja' を allowed_langs に持つ。
    insert_allowed_lang(&core, &ctx_b, 1, "ja");

    let ids_a = select_ids(
        &core,
        &ctx_a,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(
        ids_a.is_empty(),
        "tenant-a must not see tenant-b's allowed_langs rows via IN subquery"
    );

    let ids_b = select_ids(
        &core,
        &ctx_b,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids_b, vec![1]);
}

#[test]
fn exists_subquery_only_sees_own_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    seed_docs(&core, &ctx_a);
    seed_docs(&core, &ctx_b);
    // tenant-b だけが visits を持つ。
    insert_visit(&core, &ctx_b, 1, "hit");

    let ids_a = select_ids(
        &core,
        &ctx_a,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert!(
        ids_a.is_empty(),
        "tenant-a must not observe tenant-b's visits row via EXISTS"
    );

    let ids_b = select_ids(
        &core,
        &ctx_b,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert_eq!(ids_b, vec![1, 2, 3, 4]);
}

// --- 上限（ネスト深さ） ------------------------------------------------------

#[test]
fn subquery_nesting_within_limit_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");

    // 深さ 2（最外側 SELECT=1、その WHERE の IN サブクエリ=2）。
    let sql = format!(
        "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100) LIMIT 100"
    );
    let ids = select_ids(&core, &ctx, &sql);
    assert_eq!(ids, vec![1]);
}

#[test]
fn subquery_nesting_beyond_limit_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    // 深さ 6（`MAX_SUBQUERY_DEPTH` = 4 を超える）。
    let mut sql = format!("SELECT lang FROM {ALLOWED_LANGS} LIMIT 100");
    for _ in 0..6 {
        sql = format!("SELECT lang FROM {ALLOWED_LANGS} WHERE lang IN ({sql}) LIMIT 100");
    }
    sql = format!("SELECT id FROM {DOCS} WHERE lang IN ({sql}) LIMIT 100");

    let err = expect_error_code(&core, &ctx, &sql);
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::PayloadTooLarge { .. }
    ));
}

// --- 文脈の拒否（fail-closed） -----------------------------------------------

#[test]
fn exists_subquery_in_ranked_select_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) \
             ORDER BY embedding <=> '[0.1,0.1]' LIMIT 10"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

#[test]
fn in_subquery_in_update_where_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "UPDATE {DOCS} SET lang = 'xx' WHERE id IN (SELECT id FROM {ALLOWED_LANGS} LIMIT 100) \
                 USING OPERATION_ID 'update-subquery'"
            ),
        )
        .expect_err("subquery in UPDATE WHERE must be rejected");
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

#[test]
fn exists_subquery_over_extended_query_protocol_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed_docs(&core, &ctx_for("tenant-a"));

    let err = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
        ))
        .expect_err("subquery over the extended query protocol must be rejected");
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

#[test]
fn inner_subquery_without_limit_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS}) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

// Cursor Bugbot 指摘の回帰テスト: ウィンドウ関数（SQL-30・TASK-214、
// Issue #930）を含む内側は `IN`／`EXISTS` いずれも一律拒否する。
// `EXISTS` の `InnerScanIntent::ExistenceOnly` は投影・`LIMIT` だけを
// 差し替える設計であり、ウィンドウ項目を差し替えずに残すと
// `sql::scan::execute_scan` が `sql::window::execute_window_scan`
// （`LIMIT` による早期終了なしに可視行を全件 materialize する）へ分岐して
// しまい、可視行があっても資源上限で `EXISTS` 全体が失敗しうる（すでに
// 塞いだ「投影・`LIMIT` をそのまま使う」問題のウィンドウ関数版）。
// サブクエリとウィンドウ関数の組合せは設計上未検証のため fail-closed に
// 拒否する（`docs/design/sql-subquery.md` 対象外節参照）。

#[test]
fn in_subquery_with_window_function_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang, ROW_NUMBER() OVER () FROM {ALLOWED_LANGS} LIMIT 10) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { detail }
            if detail.contains("window functions")
    ));
}

#[test]
fn exists_subquery_with_window_function_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits に可視行を用意する（可視行があっても拒否されることの確認）。
    insert_visit(&core, &ctx, 1, "hit");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS \
             (SELECT note, ROW_NUMBER() OVER () FROM {VISITS} LIMIT 10) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { detail }
            if detail.contains("window functions")
    ));
}

// --- NOT EXISTS / NOT IN (SELECT ...)（Cursor Bugbot Medium 指摘対応） ------

/// `docs/design/sql-subquery.md`「スコープ（当初計画との差分）」は `NOT IN`・
/// `NOT EXISTS` を「非対応（文法自体が `NOT` を持たない）」と明記している。
/// 前置 `NOT`（`sql::allowlist::Parser::parse_where_leaf`）が構造的に確定した
/// `EXISTS (SELECT ...)`（`WherePredicate::Exists`）をそのまま `Not` で包むと、
/// 束縛段（`sql::parser::declarative_leaf_to_filter`）がこれを宣言的フィルタ
/// として扱えず `Internal` エラー（内部実装詳細の漏えい）になってしまう回帰が
/// あった。構文段で `0A000`（`FeatureNotSupported`）として明示的に拒否する。
#[test]
fn not_exists_subquery_is_rejected_with_feature_not_supported() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits に可視行を用意する（可視行があっても構文段で拒否されることの確認。
    // 「実行時に false 相当になるから受理してよい」という誤った緩和を防ぐ）。
    insert_visit(&core, &ctx, 1, "hit");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE NOT EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
        ),
    );
    assert!(
        matches!(
            &err,
            engine::sql::allowlist::SqlSurfaceError::FeatureNotSupported { .. }
        ),
        "expected FeatureNotSupported (0A000), got {err:?}"
    );
}

/// [`not_exists_subquery_is_rejected_with_feature_not_supported`] の
/// `IN (SELECT ...)` 版。同じ理由（`WherePredicate::InSubquery` を `Not` で
/// 包むと束縛段で `Internal` エラーになっていた）で `0A000` を固定する。
#[test]
fn not_in_subquery_is_rejected_with_feature_not_supported() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang NOT IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(
        matches!(
            &err,
            engine::sql::allowlist::SqlSurfaceError::FeatureNotSupported { .. }
        ),
        "expected FeatureNotSupported (0A000), got {err:?}"
    );
}
