//! `CREATE TABLE` の `NOT NULL`／`DEFAULT <literal>` 宣言構文（TABLE-16
//! 〔検討中〕・TASK-204、Issue #904）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-204・`docs/design/not-null-default.md`。
//!
//! Issue #904 レビュー指摘（Medium）: 変更・追加された既存テストは「非
//! nullable 列省略」系の `wire_code` 期待値の書き換え・フィクスチャの
//! `ColumnDef::new` 移行のみで、`DEFAULT` 補完そのもの（構文成功・実際の
//! 適用値・カタログ v5 の永続化往復・型不一致／`VECTOR` 禁止／重複宣言／
//! 長さ上限超過の各エラー経路・`ALTER TABLE ADD COLUMN` の拒否）を検証する
//! 新規テストが存在しなかった。本ファイルはその欠落を埋める。
//!
//! `sql_create_table.rs`（TASK-85・SQL-23）・`sql_upsert.rs` と同じ流儀
//! （実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

// --- 構文成功系 ---------------------------------------------------------

#[test]
fn create_table_accepts_not_null_and_default_on_text_column() {
    let (core, path) = new_core("not-null-default-create-ok");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), lang TEXT NOT NULL DEFAULT 'ja')",
        )
        .expect("CREATE TABLE with NOT NULL DEFAULT must succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));
}

#[test]
fn create_table_accepts_default_before_not_null() {
    // 順序自由（TABLE-16）: `DEFAULT` を `NOT NULL` より先に書いても受理される。
    let (core, path) = new_core("not-null-default-order-free");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), lang TEXT DEFAULT 'ja' NOT NULL)",
    )
    .expect("CREATE TABLE with DEFAULT before NOT NULL must succeed");
}

#[test]
fn create_table_accepts_default_and_primary_key_on_the_same_column() {
    // `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）と `DEFAULT`（同 Issue
    // #904）は独立した列制約として共存でき、カタログは両者を持つ v5 で
    // 永続化される（`catalog.rs::encode_decode_roundtrip_preserves_primary_key_and_default_v5`
    // の SQL 表層側の対応。base 取り込みマージで両フォーマットを統合した際の
    // 回帰）。`PRIMARY KEY` 宣言により `code` は `DEFAULT` の有無に関わらず
    // 非 nullable になる（`finalize_primary_key`）。
    let (core, path) = new_core("not-null-default-with-primary-key");
    let _guard = CleanupGuard(path.clone());
    {
        let alice = ctx("alice");
        let mut session = granted_session();

        core.execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), code TEXT DEFAULT 'draft' PRIMARY KEY)",
        )
        .expect("CREATE TABLE with DEFAULT and PRIMARY KEY on the same column must succeed");
    }
    // `core` が保持する `Storage`（延いては redb::Database）を再オープン前に
    // 破棄する（`catalog_v5_roundtrips_default_value_across_reopen` と同じ
    // 単一ライター制約の回避パターン）。
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let schema = storage
        .get_table_schema("docs")
        .expect("get schema after reopen");
    assert_eq!(
        schema.primary_key(),
        Some(&["code".to_string()][..]),
        "PRIMARY KEY must survive a catalog v5 round trip through reopen"
    );
    let code = schema
        .columns
        .iter()
        .find(|c| c.name == "code")
        .expect("code column present");
    assert!(!code.nullable, "PRIMARY KEY column must be non-nullable");
    assert_eq!(
        code.default,
        Some(engine::catalog::ColumnDefault::Text("draft".to_string())),
        "DEFAULT must survive alongside PRIMARY KEY in a catalog v5 round trip"
    );
}

// --- DEFAULT 補完の実値検証 ---------------------------------------------

#[test]
fn insert_omitting_default_column_applies_the_declared_default_value() {
    let (core, path) = new_core("not-null-default-apply");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), lang TEXT NOT NULL DEFAULT 'ja')",
    )
    .expect("create table");

    // `lang` を省略した INSERT は拒否されず、`DEFAULT` の値が実際に補われる。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding) VALUES (1, '[0.1,0.2,0.3,0.4]') USING OPERATION_ID 'op-1'",
    )
    .expect("omitting a column with DEFAULT must succeed");

    let outcome = core
        .execute_sql_in_session(&alice, &mut session, "SELECT lang FROM docs LIMIT 10")
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1);
    match result.rows[0].cells.first() {
        Some(Cell::Text(s)) => assert_eq!(s, "ja", "DEFAULT value must be applied verbatim"),
        other => panic!("expected Cell::Text(\"ja\"), got {other:?}"),
    }

    // 明示的に値を与えた行では `DEFAULT` は適用されない。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.5,0.5,0.5,0.5]', 'en') \
         USING OPERATION_ID 'op-2'",
    )
    .expect("explicit value insert must succeed");
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "SELECT lang FROM docs WHERE id = 2 LIMIT 10",
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    match result.rows[0].cells.first() {
        Some(Cell::Text(s)) => assert_eq!(s, "en"),
        other => panic!("expected Cell::Text(\"en\"), got {other:?}"),
    }
}

#[test]
fn copy_with_explicit_null_marker_does_not_apply_default_and_is_rejected_for_non_nullable_column() {
    // TABLE-16 の確定契約: 明示的な `NULL` には `DEFAULT` を適用しない。
    // `INSERT ... VALUES` の要素パーサーは `NULL` トークンを受理しない
    // （`sql::allowlist::InsertLiteral::Null` のドキュメンテーションコメント
    // 参照）ため、SQL 表層で明示 `NULL` を経由できる経路である `COPY` の
    // テキスト形式 `\N` マーカーで検証する（`sql::copy::bind_copy_record`）。
    // 非 nullable 列であれば `23502`（NotNullViolation）で拒否される。
    let (core, path) = new_core("not-null-default-copy-explicit-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), lang TEXT NOT NULL DEFAULT 'ja')",
    )
    .expect("create table");

    let sql = "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'op-copy-null'";
    let plan = core
        .begin_copy(&alice, &session, sql)
        .expect("begin copy plan");
    let mut copy_session = match plan {
        engine::core::CopyPlan::From(s) => s,
        engine::core::CopyPlan::To(..) => panic!("expected COPY FROM STDIN plan"),
    };
    // `feed` がレコード単位で `bind_copy_record` まで実行するため、NOT NULL
    // 違反はこの時点で検出される（`commit_copy_in` まで遅延しない）。
    let err = copy_session
        .feed(b"1\t[0.1,0.2,0.3,0.4]\t\\N\n")
        .expect_err("explicit \\N on a non-nullable DEFAULT column must be rejected");
    assert_eq!(err.wire_code(), "23502");
}

// --- カタログ v5 の永続化往復（再オープンを含む） ------------------------

#[test]
fn catalog_v5_roundtrips_default_value_across_reopen() {
    let path = unique_db_path("not-null-default-catalog-roundtrip");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&engine::catalog::TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(4), false),
                    ColumnDef::new("lang", ColumnType::Text, false)
                        .with_default(engine::catalog::ColumnDefault::Text("ja".to_string())),
                ],
            ))
            .expect("create table with DEFAULT");
    }
    // 再オープン後もカタログ v5 の `default` フィールドが往復する。
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let schema = storage
            .get_table_schema("docs")
            .expect("get schema after reopen");
        let lang = schema
            .columns
            .iter()
            .find(|c| c.name == "lang")
            .expect("lang column present");
        assert!(!lang.nullable);
        assert_eq!(
            lang.default,
            Some(engine::catalog::ColumnDefault::Text("ja".to_string())),
            "DEFAULT must survive a catalog v5 round trip through reopen"
        );
    }
}

#[test]
fn catalog_v2_v3_bytes_are_unchanged_when_no_column_declares_default() {
    // `DEFAULT` を 1 つも持たないスキーマは v5 を使わず、既存のバイト列表現
    // （v2／v3）のまま不変（docs/design/not-null-default.md「カタログの
    // テキスト形式」節）。往復自体で `default` が常に `None` のまま保たれる
    // ことを確認する（バイト列そのものの厳密な形式検証は既存のゴールデン
    // テスト側が担う）。
    let path = unique_db_path("not-null-default-no-default-unchanged");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&engine::catalog::TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(4), false),
                    ColumnDef::new("lang", ColumnType::Text, true),
                ],
            ))
            .expect("create table without DEFAULT");
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let schema = storage
        .get_table_schema("docs")
        .expect("get schema after reopen");
    assert!(schema.columns.iter().all(|c| c.default.is_none()));
}

// --- エラー経路 -----------------------------------------------------------

#[test]
fn default_type_mismatch_on_text_column_is_rejected() {
    // `TEXT` 列の `DEFAULT` は文字列リテラルのみ受理する（数値は型不一致）。
    let (core, path) = new_core("not-null-default-type-mismatch");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), lang TEXT DEFAULT 42)",
        )
        .expect_err("DEFAULT literal type mismatch must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn default_on_vector_column_is_rejected() {
    let (core, path) = new_core("not-null-default-vector-forbidden");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4) DEFAULT '[0.1,0.2,0.3,0.4]', lang TEXT)",
        )
        .expect_err("DEFAULT on a VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn default_null_literal_is_rejected_as_a_syntax_error() {
    // `DEFAULT NULL` は `expect_literal` が `NULL` トークンを受理しないため、
    // 構造的に構文エラーになる（TABLE-16: 明示 NULL は DEFAULT の対象外）。
    let (core, path) = new_core("not-null-default-default-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), lang TEXT DEFAULT NULL)",
        )
        .expect_err("DEFAULT NULL must be a syntax error");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn duplicate_not_null_constraint_is_rejected() {
    let (core, path) = new_core("not-null-default-dup-not-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), lang TEXT NOT NULL NOT NULL)",
        )
        .expect_err("duplicate NOT NULL must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn duplicate_default_constraint_is_rejected() {
    let (core, path) = new_core("not-null-default-dup-default");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), lang TEXT DEFAULT 'ja' DEFAULT 'en')",
        )
        .expect_err("duplicate DEFAULT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn default_literal_exceeding_length_limit_is_rejected() {
    let (core, path) = new_core("not-null-default-length-limit");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    // `MAX_COLUMN_DEFAULT_LEN`（1024 バイト）を超える文字列リテラル。
    let too_long: String = "a".repeat(engine::catalog::MAX_COLUMN_DEFAULT_LEN + 1);
    let sql = format!("CREATE TABLE docs (embedding VECTOR(4), lang TEXT DEFAULT '{too_long}')");
    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("DEFAULT literal exceeding the length limit must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- ALTER TABLE ADD COLUMN は DEFAULT を受け付ける（Issue #1169） -----------

#[test]
fn alter_table_add_column_accepts_default_and_rejects_unbindable_default() {
    // `DEFAULT` 付きの ADD COLUMN は受理する（既存行は読み出し時に既定値で補う）。
    // SQL 表層を経由しない Rust API でも、束縛できない DEFAULT はカタログへ
    // 永続化する前に拒否する（`catalog::Storage::alter_table_add_column`）。
    let path = unique_db_path("not-null-default-alter-add-column-default");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&engine::catalog::TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        ))
        .expect("create table");

    storage
        .alter_table_add_column(
            "docs",
            ColumnDef::new("lang", ColumnType::Text, true)
                .with_default(engine::catalog::ColumnDefault::Text("ja".to_string())),
        )
        .expect("ADD COLUMN with DEFAULT must be accepted");

    let err = storage
        .alter_table_add_column(
            "docs",
            ColumnDef::new("n", ColumnType::Integer, true)
                .with_default(engine::catalog::ColumnDefault::Number("abc".to_string())),
        )
        .expect_err("unbindable DEFAULT must be rejected");
    assert!(
        matches!(err, engine::catalog::CatalogError::Invalid(ref msg) if msg.contains("DEFAULT")),
        "expected CatalogError::Invalid mentioning DEFAULT, got {err:?}"
    );
}
