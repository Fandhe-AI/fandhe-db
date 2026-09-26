//! `engine::sql::allowlist` の統合テスト（TASK-74・SQL-8 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md）。
//!
//! `tests/catalog.rs` と同じ流儀（`unique_db_path` / `CleanupGuard`）で実 `Storage`
//! 上にテーブルを構築し、`impl TableLookup for Storage` を介した実カタログ照会付きの
//! `validate_statement` を検証する（`sql::allowlist` モジュール内の単体テストは
//! storage 非依存のフェイクを使うため、本ファイルは実カタログとの結合を確認する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::sql::allowlist::validate_statement;
use engine::storage::Storage;

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_storage_with_documents_table(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create documents table");
    (storage, guard)
}

#[test]
fn accepts_basic_select_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("basic-select");
    let stmt = validate_statement(
        "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect("basic shape against a real table must be accepted");
    assert_eq!(stmt.table_name(), "documents");
}

#[test]
fn accepts_select_with_where_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("where-clause");
    validate_statement(
        "SELECT * FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect("WHERE clause shape against a real table must be accepted");
}

#[test]
fn rejects_order_by_function_call_with_unknown_name_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("unknown-function");
    let err = validate_statement(
        "SELECT * FROM documents ORDER BY attacker_controlled(embedding) LIMIT 10",
        &storage,
    )
    .expect_err("unknown ORDER BY function name must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn rejects_where_predicate_call_with_unknown_name_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("unknown-predicate");
    let err = validate_statement(
        "SELECT * FROM documents WHERE unknown() ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect_err("unknown WHERE predicate name must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn rejects_table_absent_from_real_catalog_with_42p01() {
    let (storage, _guard) = open_storage_with_documents_table("undefined-table");
    let err = validate_statement(
        "SELECT * FROM nonexistent ORDER BY embedding <=> '[0.1]' LIMIT 10",
        &storage,
    )
    .expect_err("table absent from the real catalog must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn rejects_unsupported_syntax_before_touching_the_catalog() {
    // 未対応構文（GROUP BY）は、FROM に実在テーブルを指定していてもカタログ照会に
    // 先立って 42601 で拒否される（決定的な検証順序: 構造判定 → カタログ存在確認）。
    let (storage, _guard) = open_storage_with_documents_table("unsupported-syntax");
    let err = validate_statement(
        "SELECT * FROM documents GROUP BY lang ORDER BY embedding <=> '[0.1]' LIMIT 10",
        &storage,
    )
    .expect_err("unsupported syntax must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn never_silently_executes_an_unrecognized_where_condition_as_unfiltered_top_k() {
    // 未対応 WHERE 条件が黙殺されず明示的に拒否されることを固定する（SQL-8）。
    // TASK-79（SQL-9）で `<expr> <cmp> <expr>` 形の式述語（`1 = 1` 等の数値比較を
    // 含む）を正式に受理するようになったため、旧 `1 = 1` はもはや「未対応構文」の
    // 例として不適切になった（意図した仕様拡張であり、非回帰ではない）。
    // TASK-208・SQL-24（Issue #912）で `OR` 結合・括弧グルーピングも正式に受理
    // するようになったため、旧 `lang = 'ja' OR lang = 'en'` も同様に不適切になった
    // （こちらも意図した仕様拡張）。本テストの意図（許可形状に無い構文を黙って
    // 通過させない）を保つため、依然としてスコープ外のままの構文（サブクエリ）へ
    // 置き換える。
    let (storage, _guard) = open_storage_with_documents_table("no-silent-fallback");
    let err = validate_statement(
        "SELECT * FROM documents WHERE id = (SELECT 1) ORDER BY embedding <=> '[0.1]' LIMIT 10",
        &storage,
    )
    .expect_err("unrecognized WHERE condition must be rejected, never silently dropped");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn same_input_yields_same_wire_code_across_repeated_calls_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("determinism");
    let sql = "SELECT * FROM documents JOIN other ON documents.id = other.id ORDER BY embedding <=> '[0.1]' LIMIT 10";
    let first = validate_statement(sql, &storage).unwrap_err().wire_code();
    let second = validate_statement(sql, &storage).unwrap_err().wire_code();
    assert_eq!(first, second);
}

// --- TASK-161（SQL-12: `USING MODE`）実カタログ結合 ------------------------------

#[test]
fn accepts_using_mode_clause_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("using-mode-accept");
    let stmt = validate_statement(
        "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10 USING MODE 'precision'",
        &storage,
    )
    .expect("USING MODE clause against a real table must be accepted");
    assert_eq!(stmt.search_mode(), Some("precision"));
}

// --- TASK-147（EXT-3: `WHERE <col> LIKE '<prefix>%'`）実カタログ結合 --------------

#[test]
fn accepts_where_like_prefix_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("like-accept");
    validate_statement(
        "SELECT * FROM documents WHERE lang LIKE 'j%' ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect("LIKE prefix shape against a real table must be accepted");
}

// SQL-24（TASK-208 ポインタ）で `NOT LIKE` は `Not(Prefix)` として受理する
// ようになったため、`ILIKE` のみを不正形として固定する（`NOT LIKE` の受理は
// `accepts_not_like_as_negated_prefix_against_real_catalog` が別途固定する）。
#[test]
fn rejects_ilike_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("like-reject");
    let ilike = validate_statement(
        "SELECT * FROM documents WHERE lang ILIKE 'j%' ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect_err("ILIKE must be rejected");
    assert_eq!(ilike.wire_code(), "42601");
}

/// SQL-24（TASK-208 ポインタ）: `<col> NOT LIKE '<lit>'` は `Not(Prefix)` として
/// 実カタログ結合でも受理される（`sql::allowlist::tests::
/// accepts_not_like_as_negated_prefix` の storage 非依存版との対）。
#[test]
fn accepts_not_like_as_negated_prefix_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("not-like-accept");
    let stmt = validate_statement(
        "SELECT * FROM documents WHERE lang NOT LIKE 'j%' ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect("NOT LIKE against a real table must be accepted as Not(Prefix)");
    assert_eq!(
        stmt.where_predicates(),
        vec![engine::sql::allowlist::WherePredicate::Not(Box::new(
            engine::sql::allowlist::WherePredicate::Prefix {
                column: "lang".to_string(),
                pattern: "j%".to_string(),
            }
        ))]
    );
}

#[test]
fn rejects_using_mode_on_insert_statement_against_real_catalog() {
    // 書き込み系文への `USING MODE` 付与は SQL-8 の許可リスト検証で構文エラーとして
    // 拒否する（SQL-12 の R6）。`INSERT` 自体が本モジュールの許可形状に存在しないため、
    // `USING MODE` の有無に関わらず先頭キーワードの時点で拒否される。
    let (storage, _guard) = open_storage_with_documents_table("using-mode-insert-rejected");
    let err = validate_statement(
        "INSERT INTO documents (embedding) VALUES ('[0.1,0.2,0.3,0.4]') USING MODE 'recall'",
        &storage,
    )
    .expect_err("USING MODE on a write statement must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
