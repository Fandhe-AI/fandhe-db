//! サブクエリ（`IN (SELECT ...)`・`EXISTS (SELECT ...)`）の結合テスト
//! （Issue #927・SQL-29 (a)・RLS-10 (b)・TASK-213）。`tests/sql_where_or.rs`
//! と同じ流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋
//! `CpuScalarProvider`、`EngineCore::execute_sql`／`execute_sql_in_session` を
//! production 経路として検証）。
//!
//! スコープ（実装既定値。`docs/design/sql-subquery.md` 参照）:
//! - 対応: WHERE の `<col> [NOT] IN (SELECT ...)`・`[NOT] EXISTS (SELECT ...)`
//!   （`NOT` 系は Issue #1191）。内側は `SELECT ... FROM <table> [WHERE ...]
//!   LIMIT <n>`（広域取得）のみ。スカラー比較サブクエリ・相関サブクエリの
//!   `42601`・`IN` 対象型の拡大は `tests/sql29_subquery_scalar.rs`。
//! - 対象外（このファイルでは拒否の確認のみ）: 投影位置のサブクエリ・内側が
//!   ランキング付き検索 SELECT・内側 `LIMIT` 省略・拡張クエリプロトコル
//!   （Parse/Bind）経由の `$n` 併用。

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
fn in_subquery_bigint_column_in_target_matches_independent_oracle() {
    // Issue #1191: 整数族（INTEGER/BIGINT）の `IN (SELECT ...)` は、数値リテラル
    // `IN` と同じ式脱糖形（`col = n` の `Or`）へ書き換えて受理する（Issue #927 では
    // `22000` で拒否していた）。独立オラクル（テスト側で素朴に計算した期待 id 集合）と
    // 照合する。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_priority(&core, &ctx, 1, "ja", 10);
    insert_doc_with_priority(&core, &ctx, 2, "en", 20);
    insert_doc_with_priority(&core, &ctx, 3, "fr", -5);
    insert_doc(&core, &ctx, 4, "de"); // priority は NULL
    insert_priority(&core, &ctx, 1, 10);
    insert_priority(&core, &ctx, 2, -5);
    insert_priority(&core, &ctx, 3, 10); // 重複値

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE priority IN (SELECT priority FROM {PRIORITIES} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 3]);
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
    insert_doc(&core, &ctx, 1, "ja");
    // 内側の結果が 0 行でも、対象列（`VECTOR`）の型検証は行数に依存せず必ず働く
    // （PR #1103 追加 codex-review P1 指摘の回帰。Issue #1191 で整数族が対応済みに
    // なったため、非対応型の代表として `VECTOR` を使う）。

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE embedding IN \
             (SELECT embedding FROM {DOCS} WHERE lang = 'none' LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("not supported as a subquery IN target")
    ));
}

#[test]
fn in_subquery_unsupported_type_column_with_null_only_inner_result_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc(&core, &ctx, 1, "ja");

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE embedding IN \
             (SELECT embedding FROM {DOCS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        &err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { detail }
            if detail.contains("not supported as a subquery IN target")
    ));
}

// --- 上限（`IN` の distinct 値数） ---------------------------------------------

// Issue #1165: `IN (SELECT ...)` は distinct 値を 256 件以下のチャンクごとの
// `InList`（既存評価器の集合照合）へ書き換えるため、旧方式の 256 葉上限を
// 超える distinct 値でも成功し、独立オラクルと一致する。
// PR #1103 codex-review P1 指摘（DoS）の趣旨は、文全体で共有する distinct 値
// 予算 `sql::subquery::MAX_SUBQUERY_IN_VALUES`（10,000）で維持する。
#[test]
fn in_subquery_distinct_values_beyond_legacy_leaf_limit_match_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // 旧上限（256）を大きく超え、チャンク境界（256×n）をまたぐ 1,000 件。
    let distinct_count = 1_000u64;
    for i in 0..distinct_count {
        insert_allowed_lang(&core, &ctx, i, &format!("lang{i}"));
    }
    // 一致する doc（lang0・lang255・lang256・lang999）と一致しない doc。
    let hit = ["lang0", "lang255", "lang256", "lang999"];
    let miss = ["lang1000", "other", "en"];
    let mut expected = Vec::new();
    for (i, lang) in hit.iter().chain(miss.iter()).enumerate() {
        let id = (i + 1) as u64;
        insert_doc(&core, &ctx, id, lang);
        if i < hit.len() {
            expected.push(id);
        }
    }

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT {distinct_count}) LIMIT 100"
        ),
    );
    assert_eq!(ids, expected);
}

// 単一の `IN` サブクエリは内側可視行数上限（`MAX_SEARCH_K` = 10,000）まで
// distinct 値を取り込める（約 40 チャンク＝`Or` 分岐）。Scan・Aggregate の
// 両実行アーム（`core.rs` の予算初期化 2 箇所）で独立オラクルと一致する。
#[test]
fn in_subquery_single_site_at_inner_row_limit_matches_oracle_scan_and_aggregate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let distinct_count = 10_000u64;
    for i in 0..distinct_count {
        insert_allowed_lang(&core, &ctx, i, &format!("lang{i}"));
    }
    insert_doc(&core, &ctx, 1, "lang0");
    insert_doc(&core, &ctx, 2, "lang5000");
    insert_doc(&core, &ctx, 3, "lang9999");
    insert_doc(&core, &ctx, 4, "lang10000");
    insert_doc(&core, &ctx, 5, "en");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT {distinct_count}) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 2, 3]);

    let result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT COUNT(*) FROM {DOCS} WHERE lang IN \
                 (SELECT lang FROM {ALLOWED_LANGS} LIMIT {distinct_count})"
            ),
        )
        .expect("aggregate with large IN subquery should succeed");
    let count = result
        .rows
        .first()
        .and_then(|r| r.cells.first())
        .map(|c| format!("{c:?}"))
        .expect("count cell");
    assert!(count.contains('3'), "COUNT(*) must be 3, got {count}");
}

// 内側の distinct 値がちょうど旧上限（256）の前後（256・257 件）でも一致する。
#[test]
fn in_subquery_distinct_values_around_chunk_boundary_match_oracle() {
    for distinct_count in [256u64, 257u64] {
        let (core, path) = new_core();
        let _guard = CleanupGuard(path);
        let ctx = ctx_for("tenant-a");
        for i in 0..distinct_count {
            insert_allowed_lang(&core, &ctx, i, &format!("lang{i}"));
        }
        let last = format!("lang{}", distinct_count - 1);
        insert_doc(&core, &ctx, 1, &last);
        insert_doc(&core, &ctx, 2, "absent");

        let ids = select_ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM {DOCS} WHERE lang IN \
                 (SELECT lang FROM {ALLOWED_LANGS} LIMIT {distinct_count}) LIMIT 100"
            ),
        );
        assert_eq!(ids, vec![1], "distinct_count={distinct_count}");
    }
}

// 文全体で共有する distinct 値予算（`MAX_SUBQUERY_IN_VALUES` = 10,000）を
// 複数の `IN` サイトの合計が超えた場合は `54000`（上限超過の拒否契約の維持）。
#[test]
fn in_subquery_distinct_values_exceeding_statement_budget_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc(&core, &ctx, 1, "en");
    // 各 IN サイトが 5,001 件の distinct 値を取り込み、合計 10,002 > 10,000。
    let per_site = 5_001u64;
    for i in 0..per_site {
        insert_allowed_lang(&core, &ctx, i, &format!("lang{i}"));
    }

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT {per_site}) OR lang IN \
             (SELECT lang FROM {ALLOWED_LANGS} LIMIT {per_site}) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::PayloadTooLarge { .. }
    ));
}

// Cursor Bugbot 指摘（Medium）の回帰テスト: `IN` は集合所属の判定であり、
// 同じ値の内側行が何件あっても distinct 値予算を消費するのは 1 件分だけ
// （重複除去は予算消費より前）。同一値 1,000 件でも成功する。
#[test]
fn in_subquery_duplicate_values_do_not_exhaust_value_budget() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 0, "ja");
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
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) AND lang = $1 LIMIT 100"
        ))
        .expect_err("subquery with $n over the extended query protocol must be rejected");
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

/// PR #1217 レビュー指摘: `$n` を含まないサブクエリ文は従来の `parse_sql` と
/// 同様に拡張クエリの Parse でも受理される（`param_count() == 0`）。
#[test]
fn unparameterized_subquery_over_extended_query_protocol_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed_docs(&core, &ctx_for("tenant-a"));

    let prepared = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
        ))
        .expect("unparameterized subquery must be accepted");
    assert_eq!(prepared.param_count(), 0);
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

// --- NOT EXISTS / NOT IN (SELECT ...)（Issue #1191） ----------------------

/// `NOT EXISTS`: 可視行が 1 件でもあれば常に偽、無ければ常に真
/// （Issue #927 では `0A000` で拒否していた。Issue #1191 で受理）。
#[test]
fn not_exists_subquery_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let sql = format!(
        "SELECT id FROM {DOCS} WHERE NOT EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
    );
    // visits が空 → 常に真。
    assert_eq!(select_ids(&core, &ctx, &sql), vec![1, 2, 3, 4]);
    insert_visit(&core, &ctx, 1, "hit");
    // 可視行あり → 常に偽。
    assert_eq!(select_ids(&core, &ctx, &sql), Vec::<u64>::new());
    // `NOT (EXISTS ...)` と二重否定の正規化。
    assert_eq!(
        select_ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM {DOCS} WHERE NOT (EXISTS (SELECT id FROM {VISITS} LIMIT 1)) LIMIT 100"
            )
        ),
        Vec::<u64>::new()
    );
    assert_eq!(
        select_ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM {DOCS} WHERE NOT NOT EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
            )
        ),
        vec![1, 2, 3, 4]
    );
    // `AND` 併用: 偽側でも他の述語は評価される（結果は空のまま）。
    assert_eq!(
        select_ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM {DOCS} WHERE NOT EXISTS (SELECT id FROM {VISITS} LIMIT 1) \
                 AND lang = 'ja' LIMIT 100"
            )
        ),
        Vec::<u64>::new()
    );
    // `NOT (EXISTS(...) AND lang = 'ja')` は De Morgan で `NOT EXISTS OR lang <> 'ja'`。
    assert_eq!(
        select_ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM {DOCS} WHERE NOT (EXISTS (SELECT id FROM {VISITS} LIMIT 1) \
                 AND lang = 'ja') LIMIT 100"
            )
        ),
        vec![2, 3, 4]
    );
}

/// `NOT IN`: 内側 0 行なら常に真（NULL 行を含む全可視行）・内側に NULL を含めば
/// 真にならない・外側値が NULL の行は除外される（PostgreSQL と同じ三値論理）。
#[test]
fn not_in_subquery_follows_null_semantics() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_priority(&core, &ctx, 1, "ja", 10);
    insert_doc_with_priority(&core, &ctx, 2, "en", 20);
    insert_doc(&core, &ctx, 3, "fr"); // priority は NULL
    let not_in = format!(
        "SELECT id FROM {DOCS} WHERE priority NOT IN (SELECT priority FROM {PRIORITIES} LIMIT 100) LIMIT 100"
    );
    // 内側 0 行: 外側値が NULL の行も含めて常に真。
    assert_eq!(select_ids(&core, &ctx, &not_in), vec![1, 2, 3]);
    // 内側に非 NULL 値: 一致しない非 NULL 行のみ（NULL 行は UNKNOWN で除外）。
    insert_priority(&core, &ctx, 1, 10);
    assert_eq!(select_ids(&core, &ctx, &not_in), vec![2]);
    // 内側に NULL を含む（`priority` 列が NULL の docs 行を内側にする）: 真にならない。
    let not_in_with_null = format!(
        "SELECT id FROM {DOCS} WHERE priority NOT IN (SELECT priority FROM {DOCS} LIMIT 100) LIMIT 100"
    );
    assert_eq!(
        select_ids(&core, &ctx, &not_in_with_null),
        Vec::<u64>::new()
    );
}

/// TEXT 対象の `NOT IN`（後置・前置・`NOT (... IN ...)`・`NOT NOT`）と `Or` 内の配置。
#[test]
fn not_in_subquery_text_forms_match_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");
    insert_allowed_lang(&core, &ctx, 2, "fr");
    let inner = format!("(SELECT lang FROM {ALLOWED_LANGS} LIMIT 100)");

    for sql in [
        format!("SELECT id FROM {DOCS} WHERE lang NOT IN {inner} LIMIT 100"),
        format!("SELECT id FROM {DOCS} WHERE NOT lang IN {inner} LIMIT 100"),
        format!("SELECT id FROM {DOCS} WHERE NOT (lang IN {inner}) LIMIT 100"),
        format!("SELECT id FROM {DOCS} WHERE NOT NOT lang NOT IN {inner} LIMIT 100"),
    ] {
        assert_eq!(select_ids(&core, &ctx, &sql), vec![2, 4], "sql={sql}");
    }
    // `NOT (lang IN (...) OR lang = 'de')` は `lang NOT IN (...) AND lang <> 'de'`。
    assert_eq!(
        select_ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {DOCS} WHERE NOT (lang IN {inner} OR lang = 'de') LIMIT 100")
        ),
        vec![2]
    );
    // `OR` 分岐内の `NOT IN`。
    assert_eq!(
        select_ids(
            &core,
            &ctx,
            &format!("SELECT id FROM {DOCS} WHERE lang = 'ja' OR lang NOT IN {inner} LIMIT 100")
        ),
        vec![1, 2, 4]
    );
}

/// ENUM の語彙外ラベルは NOT IN でも「どの行とも一致しない」として扱う。
#[test]
fn not_in_subquery_enum_and_boolean_targets() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_mood(&core, &ctx, 1, "ja", "happy");
    insert_doc_with_mood(&core, &ctx, 2, "en", "sad");
    insert_doc(&core, &ctx, 3, "fr"); // mood は NULL
    insert_allowed_lang(&core, &ctx, 1, "happy");
    insert_allowed_lang(&core, &ctx, 2, "not-a-label");
    let sql = format!(
        "SELECT id FROM {DOCS} WHERE mood NOT IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
    );
    // 語彙内の `happy` のみ除外、語彙外は無視、NULL 行は UNKNOWN で除外。
    assert_eq!(select_ids(&core, &ctx, &sql), vec![2]);

    // BOOLEAN: 内側 {true} → `active` が false の行のみ（NULL 行は除外）。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_active(&core, &ctx, 1, "ja", true);
    insert_doc_with_active(&core, &ctx, 2, "en", false);
    insert_doc(&core, &ctx, 3, "fr");
    insert_visit_with_flag(&core, &ctx, 1, "a", true);
    let sql = format!(
        "SELECT id FROM {DOCS} WHERE active NOT IN (SELECT flag FROM {VISITS} LIMIT 100) LIMIT 100"
    );
    assert_eq!(select_ids(&core, &ctx, &sql), vec![2]);
    insert_visit_with_flag(&core, &ctx, 2, "b", false);
    assert_eq!(select_ids(&core, &ctx, &sql), Vec::<u64>::new());
}

/// RLS-10 (b): `NOT IN`／`NOT EXISTS` の結果は呼び出しセッションの可視行だけで
/// 決まる（他テナントの行が結果を変えない）。
#[test]
fn not_in_and_not_exists_ignore_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    seed_docs(&core, &a);
    insert_allowed_lang(&core, &a, 1, "ja");
    let not_in = format!(
        "SELECT id FROM {DOCS} WHERE lang NOT IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
    );
    let not_exists = format!(
        "SELECT id FROM {DOCS} WHERE NOT EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
    );
    let before_in = select_ids(&core, &a, &not_in);
    let before_ex = select_ids(&core, &a, &not_exists);
    assert_eq!(before_in, vec![2, 3, 4]);
    assert_eq!(before_ex, vec![1, 2, 3, 4]);
    // 他テナントの allowed_langs（`fr` を含む）・visits を追加しても tenant-a の結果は不変。
    insert_allowed_lang(&core, &b, 100, "fr");
    insert_visit(&core, &b, 100, "other");
    assert_eq!(select_ids(&core, &a, &not_in), before_in);
    assert_eq!(select_ids(&core, &a, &not_exists), before_ex);
}

/// 静的検証（対象列・型）は内側の行数に依存せず `NOT IN` でも働く。
#[test]
fn not_in_subquery_static_validation_is_independent_of_row_count() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    for sql in [
        format!("SELECT id FROM {DOCS} WHERE nope NOT IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"),
        format!("SELECT id FROM {DOCS} WHERE lang NOT IN (SELECT flag FROM {VISITS} LIMIT 100) LIMIT 100"),
        format!("SELECT id FROM {DOCS} WHERE embedding NOT IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"),
    ] {
        let err = expect_error_code(&core, &ctx, &sql);
        assert!(
            matches!(err, engine::sql::allowlist::SqlSurfaceError::InvalidInput { .. }),
            "sql={sql} err={err:?}"
        );
    }
}

/// サブクエリ不許可の文脈（述語形 DML）では `NOT IN`／`NOT EXISTS` も `42601` のまま。
#[test]
fn not_subquery_forms_stay_rejected_in_non_subquery_contexts() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    for sql in [
        format!("UPDATE {DOCS} SET lang = 'x' WHERE NOT EXISTS (SELECT id FROM {VISITS} LIMIT 1)"),
        format!("DELETE FROM {DOCS} WHERE lang NOT IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 1)"),
    ] {
        let err = core
            .execute_sql_in_session(&ctx, &mut SessionState::default(), &sql)
            .expect_err("must be rejected");
        assert!(
            matches!(
                err,
                engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
            ),
            "sql={sql} err={err:?}"
        );
    }
}
