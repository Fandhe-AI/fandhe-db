//! `engine::declarative_filter`／SQL 表層 `WHERE <col> LIKE '<prefix>%'` の結合テスト
//! （TASK-147、対象ビヘイビア: EXT-3。ポインタ: `docs/spec/05-tasks.md` TASK-147・
//! `docs/spec/04-behavior/extensions.md` EXT-3）。
//!
//! `tests/sql_surface.rs`・`tests/sql_udf_call.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`engine::tenant::insert_typed_row`
//! による投入、production 判定関数に依存しない独立オラクル）で実 `Storage` 上に
//! テーブルを構築し、`EngineCore::execute_sql`（SQL 経由）と
//! `declarative_filter::DeclarativeFilter`（Rust API 直接呼び出し）の両方を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::declarative_filter::DeclarativeFilter;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::{ScalarRef, Value};
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// `embedding VECTOR(2)`・`path TEXT`・`kind TEXT`・`lang TEXT` を持つテーブルへ
/// `ALTER TABLE ADD COLUMN` で nullable な `tag TEXT` を追加した上で、単一テナント
/// （`tenant-a`・`Public`）の行を投入する。
fn setup_single_tenant_table(storage: &Storage, rows: &[(u64, [f32; 2], &str, &str, &str)]) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("kind", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    storage
        .alter_table_add_column("docs", ColumnDef::new("tag", ColumnType::Text, true))
        .expect("add nullable tag column");

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, emb, path, kind, lang) in rows {
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation id");
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text((*path).to_string()),
                Value::Text((*kind).to_string()),
                Value::Text((*lang).to_string()),
                Value::Null,
            ],
            &op_id,
        )
        .expect("insert row");
    }
}

fn schema_docs() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("tag", ColumnType::Text, true),
        ],
    )
}

// --- EXT-3: 任意列への等価条件（SQL-2 は `lang` 固定の実装例だったことの汎用化確認） ---

#[test]
fn ext3_equality_on_arbitrary_text_column() {
    let path = unique_db_path("ext3-equality-arbitrary-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, [1.0, 0.0], "src/a.rs", "code", "ja"),
            (2, [0.9, 0.1], "docs/a.md", "doc", "ja"),
            (3, [0.0, 1.0], "src/b.rs", "code", "en"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE kind = 'code' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("equality on a non-`lang` column should succeed");
    assert_eq!(result_ids(&result), vec![1, 3]);
}

// --- EXT-3: 前方一致条件（under-fetch なく `limit` 件を正確に返す） ------------------

#[test]
fn ext3_prefix_filter_excludes_non_matching_without_under_fetch() {
    let path = unique_db_path("ext3-prefix-no-under-fetch");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            // 距離順位が高いが不一致（先頭）の行を混ぜて、事前フィルタ（既定順）が
            // `limit` を正確に満たすことを確認する（SQL-2 の同構造テストに倣う）。
            (1, [1.0, 0.0], "other/x.rs", "code", "ja"),
            (2, [0.99, 0.0], "src/a.rs", "code", "ja"),
            (3, [0.98, 0.0], "src/b.rs", "code", "ja"),
            (4, [0.97, 0.0], "src/c.rs", "code", "ja"),
            (5, [0.5, 0.5], "src/d.rs", "code", "ja"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'src/%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2",
        )
        .expect("prefix filter should succeed");
    assert_eq!(result_ids(&result), vec![2, 3]);
}

// --- EXT-3: 前方一致はバイト前方一致・大文字小文字区別 -----------------------------

#[test]
fn ext3_prefix_is_byte_prefix_and_case_sensitive() {
    let path = unique_db_path("ext3-prefix-case-sensitive");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, [1.0, 0.0], "src/a.rs", "code", "ja"),
            (2, [0.9, 0.1], "Src/b.rs", "code", "ja"),
            (3, [0.8, 0.2], "srcx/c.rs", "code", "ja"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'Src/%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("prefix query should succeed");
    assert_eq!(result_ids(&result), vec![2]);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'src%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("prefix query should succeed");
    let mut ids = result_ids(&result);
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3]);
}

// --- EXT-3: マルチバイト prefix は文字境界で安全 ------------------------------------

#[test]
fn ext3_multibyte_prefix_matches_on_char_boundary() {
    let path = unique_db_path("ext3-multibyte-prefix");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, [1.0, 0.0], "日本語/doc.md", "doc", "ja"),
            (2, [0.9, 0.1], "語/doc.md", "doc", "ja"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE path LIKE '日本語/%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("multibyte prefix query should succeed");
    assert_eq!(result_ids(&result), vec![1]);
}

// --- EXT-3: 等価・前方一致・式述語の AND 結合 --------------------------------------

#[test]
fn ext3_combined_equality_and_prefix_and_udf_predicate() {
    let path = unique_db_path("ext3-combined-predicates");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, [1.0, 0.0], "src/a.rs", "code", "ja"),
            (2, [0.9, 0.1], "src/b.rs", "doc", "ja"),
            (3, [0.8, 0.2], "src/c.rs", "code", "en"),
            (4, [0.7, 0.3], "other/d.rs", "code", "ja"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE kind = 'code' AND path LIKE 'src/%' AND vec_norm(embedding) > 0.85 \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("combined predicates should succeed");
    assert_eq!(result_ids(&result), vec![1]);
}

// --- EXT-3: NULL 列は等価・前方一致とも不一致 ---------------------------------------

#[test]
fn ext3_null_column_never_matches() {
    let path = unique_db_path("ext3-null-never-matches");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    // `tag` は常に NULL で投入される（`setup_single_tenant_table` 参照）。
    setup_single_tenant_table(&storage, &[(1, [1.0, 0.0], "src/a.rs", "code", "ja")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let eq = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE tag = 'x' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("equality against a NULL column should succeed with 0 rows");
    assert!(result_ids(&eq).is_empty());

    let prefix = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE tag LIKE 'x%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("prefix match against a NULL column should succeed with 0 rows");
    assert!(result_ids(&prefix).is_empty());
}

// --- EXT-3: `HINT ORDER(DISTANCE, SCALAR, RLS)` 下でも事後適用が正確 -----------------

#[test]
fn ext3_prefix_postfilter_under_distance_first_hint_order() {
    let path = unique_db_path("ext3-prefix-distance-first");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(
        &storage,
        &[
            (1, [1.0, 0.0], "other/x.rs", "code", "ja"),
            (2, [0.99, 0.0], "src/a.rs", "code", "ja"),
            (3, [0.98, 0.0], "src/b.rs", "code", "ja"),
        ],
    );
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE path LIKE 'src/%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10 \
             HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect("DISTANCE-first HINT ORDER should still apply the prefix filter");
    let mut ids = result_ids(&result);
    ids.sort_unstable();
    assert_eq!(ids, vec![2, 3]);
}

// --- EXT-3: RLS はメタデータフィルタより先に強制される -------------------------------

#[test]
fn ext3_rls_is_enforced_before_metadata_filter() {
    let path = unique_db_path("ext3-rls-enforced");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let rows: [(u64, &str, &str, Visibility); 2] = [
        (1, "tenant-a", "src/a.rs", Visibility::Public),
        (2, "tenant-b", "src/b.rs", Visibility::Private),
    ];
    for (id, tenant, path_val, visibility) in rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            visibility,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(path_val.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // tenant-b の Private 行（id=2）は path が一致していても可視化されない（反復して
    // 漏えい 0 件を固定する）。
    for _ in 0..20 {
        let result = core
            .execute_sql(
                &ctx,
                "SELECT * FROM docs WHERE path LIKE 'src/%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
            )
            .expect("RLS-scoped prefix query should succeed");
        assert_eq!(result_ids(&result), vec![1]);
    }
}

// --- EXT-3: 不正な LIKE パターン形状は 22000 -----------------------------------------

#[test]
fn ext3_rejects_invalid_prefix_patterns_with_22000() {
    let path = unique_db_path("ext3-invalid-prefix-patterns");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(&storage, &[(1, [1.0, 0.0], "src/a.rs", "code", "ja")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for pattern in ["abc", "%", "a%b%", "a_%", "a\\%", "%abc"] {
        let sql = format!(
            "SELECT * FROM docs WHERE path LIKE '{pattern}' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        );
        let err = core.execute_sql(&ctx, &sql).unwrap_err_or_else_panic(&sql);
        assert_eq!(err.wire_code(), "22000", "pattern={pattern:?}");
    }
}

/// `Result::unwrap_err` の失敗時パニックメッセージへ元の SQL を含める薄いヘルパ
/// （テスト専用。`unwrap_err` 単体だと `pattern` ごとの失敗箇所が分かりにくいため）。
trait UnwrapErrOrPanic<E> {
    fn unwrap_err_or_else_panic(self, sql: &str) -> E;
}

impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<E> for Result<T, E> {
    fn unwrap_err_or_else_panic(self, sql: &str) -> E {
        match self {
            Ok(v) => panic!("expected {sql:?} to be rejected, got {v:?}"),
            Err(e) => e,
        }
    }
}

// --- EXT-3: VECTOR 列・未知列の指定は 22000 ------------------------------------------

#[test]
fn ext3_rejects_vector_column_and_unknown_column_with_22000() {
    let path = unique_db_path("ext3-vector-unknown-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(&storage, &[(1, [1.0, 0.0], "src/a.rs", "code", "ja")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let vector_col = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE embedding LIKE 'x%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect_err("LIKE against a VECTOR column must be rejected");
    assert_eq!(vector_col.wire_code(), "22000");

    let unknown_col = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE nope = 'x' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect_err("equality against an unknown column must be rejected");
    assert_eq!(unknown_col.wire_code(), "22000");
}

// --- EXT-3: 未対応の LIKE 形状は 42601 -----------------------------------------------

#[test]
fn ext3_rejects_unsupported_like_forms_with_42601() {
    let path = unique_db_path("ext3-unsupported-like-forms");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(&storage, &[(1, [1.0, 0.0], "src/a.rs", "code", "ja")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // `NOT LIKE 'src/%'` は SQL-24（TASK-208 ポインタ）で `Not(Prefix)` として
    // 受理するようになったため、本テストの対象からは外した（回帰は
    // `tests/sql24_in_between_null_not.rs`／`sql::allowlist::tests::
    // accepts_not_like_as_negated_prefix` が固定する）。
    for sql in [
        "SELECT * FROM docs WHERE path ILIKE 'src/%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        "SELECT * FROM docs WHERE path LIKE kind ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        "SELECT * FROM docs WHERE path LIKE 1 ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
    ] {
        let err = core.execute_sql(&ctx, sql).unwrap_err_or_else_panic(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql:?}");
    }
}

// --- EXT-3: 同一入力は決定的に同一 wire_code -----------------------------------------

#[test]
fn ext3_wire_code_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("ext3-deterministic-wire-code");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_single_tenant_table(&storage, &[(1, [1.0, 0.0], "src/a.rs", "code", "ja")]);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = "SELECT * FROM docs WHERE path LIKE '%' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";
    let first = core.execute_sql(&ctx, sql).unwrap_err().wire_code();
    let second = core.execute_sql(&ctx, sql).unwrap_err().wire_code();
    assert_eq!(first, second);
}

// --- EXT-3: Rust API を SQL を介さず直接使う（汎用 API としての利用形） --------------

#[test]
fn ext3_rust_api_bind_and_match_directly() {
    let schema = schema_docs();
    let filter = DeclarativeFilter::starts_with("path", "src/")
        .bind(&schema)
        .expect("bind should succeed for a TEXT column");
    assert!(filter.matches(Some(ScalarRef::Text("src/lib.rs"))));
    assert!(!filter.matches(Some(ScalarRef::Text("lib.rs"))));
    assert!(!filter.matches(None));

    let eq = DeclarativeFilter::equals("kind", "code")
        .bind(&schema)
        .expect("bind should succeed for a TEXT column");
    assert!(eq.matches(Some(ScalarRef::Text("code"))));
    assert!(!eq.matches(Some(ScalarRef::Text("doc"))));
}
