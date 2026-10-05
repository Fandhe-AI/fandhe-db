//! `CREATE INDEX` / `DROP INDEX`（索引宣言。TASK-206・INDEX-7・SQL-23、
//! Issue #908）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-206・
//! `docs/spec/04-behavior/indexing.md` INDEX-7・`docs/spec/04-behavior/
//! sql-surface.md` SQL-23・`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `table18_view.rs`・`sql_drop_table.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、`engine::tenant::
//! insert_typed_row` による投入、`EngineCore::execute_sql_in_session` を
//! production 経路として使う）。検証する契約（詳細は
//! `docs/design/index-ddl-declaration.md` 参照）:
//!
//! - DDL 実行権限ゲート（`sql::ddl::require_ddl_permission`）が対象の実在有無を
//!   問わず `42501` を返し、カタログを一切変更しない
//! - 構文の許可形状（`0A000`／`42601`／`54000`）はカタログを参照しない
//! - 名前空間（テーブル・ビュー・索引）の共有と種別不一致（`42P07`／`42809`）
//! - 対象・列の不在（`42P01`／`42703`／`42704`）と種別・列型の不整合（`0A000`）
//! - 明示トランザクション内の索引 DDL は `0A000`
//! - 索引宣言の有無でクエリ結果集合・RLS 境界が変わらない
//! - 宣言の永続化と `DROP TABLE` による一掃

use engine::catalog::{ColumnDef, ColumnType, IndexKind, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("blob", ColumnType::Bytea, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, lang: &str, visibility: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 1.0]),
            Value::Text(lang.to_string()),
            Value::Text(format!("{tenant} body {id}")),
            Value::Null,
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// `docs` テーブルと 3 テナント（公開・非公開混在）の行を用意した DB を開く。
fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    insert_row(&storage, "alice", 1, "ja", Visibility::Public);
    insert_row(&storage, "alice", 2, "en", Visibility::Public);
    insert_row(&storage, "alice", 3, "ja", Visibility::Private);
    insert_row(&storage, "bob", 4, "ja", Visibility::Public);
    insert_row(&storage, "bob", 5, "ja", Visibility::Private);
    insert_row(&storage, "carol", 6, "en", Visibility::Public);
    (path, guard)
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), session, sql)
}

fn run_err(core: &EngineCore, session: &mut SessionState, sql: &str) -> SqlSurfaceError {
    match run(core, session, sql) {
        Ok(outcome) => panic!("{sql} must fail, got {outcome:?}"),
        Err(e) => e,
    }
}

/// 再オープンしたストレージから索引宣言名を列挙する（`core` を drop した後に呼ぶ）。
fn declared_index_names(path: &std::path::Path) -> Vec<String> {
    Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
        .into_iter()
        .map(|d| d.name)
        .collect()
}

// --- 権限（DDL 実行権限ゲート） ---------------------------------------------

/// 権限を持たない既定セッションには、対象テーブル・索引の実在有無を問わず常に
/// 同一の `42501` を返し（存在情報の非漏えい）、カタログを変更しない。
#[test]
fn ddl_permission_denies_regardless_of_existence_and_writes_nothing() {
    let (path, _guard) = open_fixture("index-ddl-perm");
    {
        let core = core_at(&path);
        let mut allowed = allowed_session();
        run(
            &core,
            &mut allowed,
            "CREATE INDEX existing_idx ON docs (lang)",
        )
        .expect("allowed session creates an index");

        let mut denied = SessionState::default();
        let messages: Vec<String> = [
            "CREATE INDEX idx_a ON docs (lang)",
            "CREATE INDEX idx_b ON ghost (lang)",
            "CREATE INDEX existing_idx ON docs (lang)",
            "CREATE INDEX idx_c ON docs USING hnsw (embedding)",
            "DROP INDEX existing_idx",
            "DROP INDEX ghost_idx",
            "DROP INDEX docs",
        ]
        .iter()
        .map(|sql| {
            let err = run_err(&core, &mut denied, sql);
            assert_eq!(err.wire_code(), "42501", "{sql}");
            err.to_string()
        })
        .collect();
        assert!(
            messages.windows(2).all(|w| w[0] == w[1]),
            "the denial message must not depend on the target: {messages:?}"
        );
    }
    assert_eq!(
        declared_index_names(&path),
        vec!["existing_idx".to_string()]
    );
}

// --- 成功系・永続化 -----------------------------------------------------------

#[test]
fn create_and_drop_index_succeed_and_persist_across_reopen() {
    let (path, _guard) = open_fixture("index-ddl-success");
    {
        let core = core_at(&path);
        let mut session = allowed_session();
        assert!(matches!(
            run(
                &core,
                &mut session,
                "CREATE INDEX idx_lang ON docs (lang, id)"
            ),
            Ok(SqlOutcome::CreateIndex(_))
        ));
        assert!(matches!(
            run(
                &core,
                &mut session,
                "create index idx_vec on docs using HNSW (embedding);"
            ),
            Ok(SqlOutcome::CreateIndex(_))
        ));
        assert!(matches!(
            run(&core, &mut session, "CREATE INDEX idx_tmp ON docs (body)"),
            Ok(SqlOutcome::CreateIndex(_))
        ));
        assert!(matches!(
            run(&core, &mut session, "DROP INDEX idx_tmp"),
            Ok(SqlOutcome::DropIndex(_))
        ));
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let defs = storage.list_indexes().expect("list indexes");
    assert_eq!(defs.len(), 2);
    let lang = defs
        .iter()
        .find(|d| d.name == "idx_lang")
        .expect("idx_lang");
    assert_eq!(lang.kind, IndexKind::Scalar);
    assert_eq!(lang.table, TABLE);
    assert_eq!(lang.columns, vec!["lang".to_string(), "id".to_string()]);
    let vec_idx = defs.iter().find(|d| d.name == "idx_vec").expect("idx_vec");
    assert_eq!(vec_idx.kind, IndexKind::Hnsw);
    assert_eq!(vec_idx.columns, vec!["embedding".to_string()]);
}

/// `BYTEA` 列のスカラー宣言（単独・`id` との複合）は受理され永続化され、
/// `DROP INDEX` で消える（Issue #1362・INDEX-7）。
#[test]
fn bytea_scalar_declaration_is_accepted_and_droppable() {
    let (path, _guard) = open_fixture("index-ddl-bytea");
    {
        let core = core_at(&path);
        let mut session = allowed_session();
        for sql in [
            "CREATE INDEX idx_blob ON docs (blob)",
            "CREATE INDEX idx_blob_id ON docs (id, blob)",
        ] {
            assert!(
                matches!(
                    run(&core, &mut session, sql),
                    Ok(SqlOutcome::CreateIndex(_))
                ),
                "{sql}"
            );
        }
        assert!(matches!(
            run(&core, &mut session, "DROP INDEX idx_blob_id"),
            Ok(SqlOutcome::DropIndex(_))
        ));
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let defs = storage.list_indexes().expect("list indexes");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "idx_blob");
    assert_eq!(defs[0].columns, vec!["blob".to_string()]);
}

/// 数値 4 型のスカラー宣言は受理され永続化され、`DROP INDEX` で消える。
/// `USING hnsw` は `0A000` のまま（Issue #1413・INDEX-7）。
#[test]
fn numeric_scalar_declaration_is_accepted_and_droppable() {
    let path = unique_db_path("index-ddl-numeric");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                TABLE,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("qty", ColumnType::Integer, true),
                    ColumnDef::new("total", ColumnType::BigInt, true),
                    ColumnDef::new("ratio", ColumnType::Real, true),
                    ColumnDef::new("score", ColumnType::Double, true),
                ],
            ))
            .expect("create table");
    }
    {
        let core = core_at(&path);
        let mut session = allowed_session();
        for col in ["qty", "total", "ratio", "score"] {
            let sql = format!("CREATE INDEX idx_{col} ON docs ({col})");
            assert!(
                matches!(
                    run(&core, &mut session, &sql),
                    Ok(SqlOutcome::CreateIndex(_))
                ),
                "{sql}"
            );
            let hnsw = format!("CREATE INDEX idx_h_{col} ON docs USING hnsw ({col})");
            let err = run(&core, &mut session, &hnsw).expect_err("hnsw on numeric");
            assert_eq!(err.wire_code(), "0A000", "{hnsw}");
        }
        assert!(matches!(
            run(&core, &mut session, "DROP INDEX idx_total"),
            Ok(SqlOutcome::DropIndex(_))
        ));
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let mut names: Vec<String> = storage
        .list_indexes()
        .expect("list indexes")
        .into_iter()
        .map(|d| d.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["idx_qty", "idx_ratio", "idx_score"]);
}

// --- 構文の許可形状（カタログ非参照） -------------------------------------------

/// 構文検証段の拒否はカタログを参照しないため、対象テーブルが存在しなくても
/// 同じ分類になる（権限の有無にも依存しない）。
#[test]
fn structural_rejections_do_not_consult_the_catalog() {
    let (path, _guard) = open_fixture("index-ddl-structure");
    let core = core_at(&path);
    let too_many: Vec<String> = (0..257).map(|i| format!("c{i}")).collect();
    let too_many_sql = format!("CREATE INDEX idx ON docs ({})", too_many.join(", "));
    let cases: Vec<(String, &str)> = vec![
        (
            "CREATE INDEX idx ON docs USING btree (lang)".into(),
            "0A000",
        ),
        ("CREATE INDEX idx ON docs USING bm25 (body)".into(), "0A000"),
        (
            "CREATE INDEX idx ON docs (lang) WHERE lang = 'ja'".into(),
            "0A000",
        ),
        ("CREATE INDEX idx ON docs (lower(lang))".into(), "0A000"),
        ("CREATE INDEX idx ON docs ('lang')".into(), "0A000"),
        (
            "CREATE INDEX idx ON docs USING hnsw (embedding, lang)".into(),
            "0A000",
        ),
        ("CREATE INDEX idx ON docs (lang, lang)".into(), "42601"),
        ("CREATE INDEX idx ON docs (lang + 1)".into(), "42601"),
        ("CREATE UNIQUE INDEX idx ON docs (lang)".into(), "42601"),
        (
            "CREATE INDEX IF NOT EXISTS idx ON docs (lang)".into(),
            "42601",
        ),
        ("CREATE INDEX idx ON docs (lang DESC)".into(), "42601"),
        ("CREATE INDEX idx ON docs ()".into(), "0A000"),
        ("CREATE INDEX ON docs (lang)".into(), "42601"),
        ("DROP INDEX a, b".into(), "42601"),
        ("DROP INDEX a CASCADE".into(), "42601"),
        ("DROP INDEX IF EXISTS a".into(), "42601"),
        (too_many_sql, "54000"),
    ];
    for (sql, code) in &cases {
        for session in [&mut allowed_session(), &mut SessionState::default()] {
            let err = run_err(&core, session, sql);
            assert_eq!(err.wire_code(), *code, "{sql}");
        }
        // 対象テーブルの不在も同じ分類（カタログ非参照の証跡）。
        let ghost = sql.replace(" docs ", " ghost ");
        let err = run_err(&core, &mut allowed_session(), &ghost);
        assert_eq!(err.wire_code(), *code, "{ghost}");
    }
}

// --- カタログ判定（名前空間・対象・列） ---------------------------------------

#[test]
fn catalog_level_classification_matches_err6() {
    let (path, _guard) = open_fixture("index-ddl-classify");
    let core = core_at(&path);
    let mut s = allowed_session();
    run(
        &core,
        &mut s,
        "CREATE VIEW ja_docs AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create view");
    run(&core, &mut s, "CREATE INDEX idx_lang ON docs (lang)").expect("create index");

    let cases: &[(&str, &str)] = &[
        // 名前空間の共有（テーブル・ビュー・既存索引との衝突）。
        ("CREATE INDEX idx_lang ON docs (body)", "42P07"),
        ("CREATE INDEX docs ON docs (body)", "42P07"),
        ("CREATE INDEX ja_docs ON docs (body)", "42P07"),
        ("CREATE TABLE idx_lang (embedding VECTOR(2))", "42P07"),
        ("CREATE VIEW idx_lang AS SELECT id FROM docs", "42P07"),
        // 対象の種別・存在。
        ("CREATE INDEX idx_v ON ja_docs (id)", "42809"),
        ("CREATE INDEX idx_g ON ghost (lang)", "42P01"),
        // 列の存在・種別整合。
        ("CREATE INDEX idx_m ON docs (missing)", "42703"),
        ("CREATE INDEX idx_m ON docs USING hnsw (missing)", "42703"),
        ("CREATE INDEX idx_e ON docs (embedding)", "0A000"),
        ("CREATE INDEX idx_f ON docs (flag)", "0A000"),
        ("CREATE INDEX idx_h ON docs USING hnsw (lang)", "0A000"),
        ("CREATE INDEX idx_i ON docs USING hnsw (id)", "0A000"),
        // `DROP` の対象不在・種別不一致。
        ("DROP INDEX ghost_idx", "42704"),
        ("DROP INDEX docs", "42809"),
        ("DROP INDEX ja_docs", "42809"),
        ("DROP TABLE idx_lang", "42809"),
        ("DROP VIEW idx_lang", "42809"),
        // 索引名を TABLE 系 DDL の対象にした場合も種別不一致（PR #1054 レビュー
        // 指摘の回帰防止。テーブル不在の `42P01` へ落とさない）。
        ("CREATE INDEX idx_x ON idx_lang (lang)", "42809"),
        ("ALTER TABLE idx_lang ADD COLUMN extra TEXT", "42809"),
        // 対照: 実在しない名前は従来どおり `42P01`。
        ("ALTER TABLE ghost ADD COLUMN extra TEXT", "42P01"),
    ];
    for (sql, code) in cases {
        let err = run_err(&core, &mut s, sql);
        assert_eq!(err.wire_code(), *code, "{sql}");
    }
    // 失敗した DDL はいずれも何も書かない（索引・ビュー・テーブルとも残存）。
    run(&core, &mut s, "DROP INDEX idx_lang").expect("index still exists");
    run(&core, &mut s, "DROP VIEW ja_docs").expect("view still exists");
}

fn visible_count(core: &EngineCore) -> String {
    match run(
        core,
        &mut SessionState::default(),
        "SELECT COUNT(*) FROM docs",
    ) {
        Ok(SqlOutcome::Query(result)) => format!("{:?}", result.rows),
        other => panic!("unexpected {other:?}"),
    }
}

/// 書き込み系 DML（`INSERT`／UPSERT／`UPDATE`／`DELETE`／`TRUNCATE`）の対象に索引名を
/// 指定した場合は、ビューと同じく種別不一致（`42809`）で拒否し、何も書かない
/// （DML は DDL 権限を要しない）。実在しない名前は従来どおり `42P01`。
#[test]
fn write_dml_targeting_an_index_name_is_wrong_object_type() {
    let (path, _guard) = open_fixture("index-ddl-dml-target");
    let core = core_at(&path);
    run(
        &core,
        &mut allowed_session(),
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("create index");
    let before = visible_count(&core);
    for target in ["idx_lang", "ghost"] {
        let expected = if target == "idx_lang" {
            "42809"
        } else {
            "42P01"
        };
        for sql in [
            format!(
                "INSERT INTO {target} (id, embedding, lang, body) VALUES (100, '[1.0, 0.0]', 'ja', 'b') USING OPERATION_ID 'op-ins-{target}'"
            ),
            format!(
                "INSERT INTO {target} (id, embedding, lang, body) VALUES (100, '[1.0, 0.0]', 'ja', 'b') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-ups-{target}'"
            ),
            format!("UPDATE {target} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-upd-{target}'"),
            format!("DELETE FROM {target} WHERE id = 1 USING OPERATION_ID 'op-del-{target}'"),
            format!("TRUNCATE TABLE {target} USING OPERATION_ID 'op-trn-{target}'"),
        ] {
            let err = run_err(&core, &mut SessionState::default(), &sql);
            assert_eq!(err.wire_code(), expected, "{sql}");
        }
    }
    // 対象テーブルの行（alice から見える件数）は変化していない。
    assert_eq!(visible_count(&core), before);
}

// --- 明示トランザクション ---------------------------------------------------

#[test]
fn index_ddl_inside_explicit_transaction_is_rejected() {
    let (path, _guard) = open_fixture("index-ddl-txn");
    {
        let core = core_at(&path);
        let mut session = allowed_session();
        for sql in [
            "CREATE INDEX idx_lang ON docs (lang)",
            "DROP INDEX idx_lang",
        ] {
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(&ctx("alice"), &mut session, &mut txn, "BEGIN")
                .expect("begin");
            let err = core
                .execute_sql_in_txn(&ctx("alice"), &mut session, &mut txn, sql)
                .expect_err("index DDL inside a transaction must be rejected");
            assert_eq!(err.wire_code(), "0A000", "{sql}");
            assert_eq!(txn.status(), TransactionStatus::Failed);
            core.execute_sql_in_txn(&ctx("alice"), &mut session, &mut txn, "ROLLBACK")
                .expect("rollback");
        }
    }
    assert!(declared_index_names(&path).is_empty());
}

// --- 結果集合・RLS 境界の不変 -------------------------------------------------

/// 索引宣言の作成前後・削除後で、テナントごとの検索・述語付き取得・集計の結果が
/// 完全に一致する（宣言はカタログのみを変更し、RLS の暗黙適用は索引の有無に
/// 依存しない）。
#[test]
fn index_declarations_do_not_change_query_results_or_rls() {
    let (path, _guard) = open_fixture("index-ddl-results");
    let core = core_at(&path);
    let queries = [
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0, 1.0]' LIMIT 10",
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[2.0, 1.0]' LIMIT 10",
        "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10",
        "SELECT COUNT(*) FROM docs WHERE lang = 'ja'",
    ];
    let snapshot = |core: &EngineCore| -> Vec<String> {
        let mut out = Vec::new();
        for tenant in ["alice", "bob", "carol"] {
            for sql in &queries {
                let mut session = SessionState::default();
                let outcome = core
                    .execute_sql_in_session(&ctx(tenant), &mut session, sql)
                    .expect("query must succeed");
                out.push(format!("{tenant}|{sql}|{outcome:?}"));
            }
        }
        out
    };
    let before = snapshot(&core);

    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX idx_lang ON docs (lang)").expect("scalar index");
    run(
        &core,
        &mut s,
        "CREATE INDEX idx_vec ON docs USING hnsw (embedding)",
    )
    .expect("hnsw index");
    assert_eq!(
        snapshot(&core),
        before,
        "declaring indexes must not change results"
    );

    run(&core, &mut s, "DROP INDEX idx_lang").expect("drop scalar");
    run(&core, &mut s, "DROP INDEX idx_vec").expect("drop hnsw");
    assert_eq!(
        snapshot(&core),
        before,
        "dropping indexes must not change results"
    );

    // alice の可視集合に bob の非公開行（id=5）が現れないこと（非 vacuous な RLS 確認。
    // 同じ整形で bob 自身には現れることを先に確かめ、照合パターンの有効性を担保する）。
    assert!(before
        .iter()
        .any(|line| line.starts_with("bob|") && line.contains("id: 5,")));
    assert!(before
        .iter()
        .filter(|line| line.starts_with("alice|"))
        .all(|line| !line.contains("id: 5,")));
}

// --- ライフサイクル -----------------------------------------------------------

/// `DROP TABLE` は対象テーブルの索引宣言を一掃するため、同名テーブルの再作成後に
/// 同名索引を作り直せる（残置していれば `42P07` になる）。
#[test]
fn drop_table_clears_index_declarations() {
    let (path, _guard) = open_fixture("index-ddl-drop-table");
    let core = core_at(&path);
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX idx_lang ON docs (lang)").expect("create index");
    run(&core, &mut s, "DROP TABLE docs").expect("drop table");
    run(
        &core,
        &mut s,
        "CREATE TABLE docs (embedding VECTOR(2), lang TEXT)",
    )
    .expect("recreate table");
    run(&core, &mut s, "CREATE INDEX idx_lang ON docs (lang)")
        .expect("recreate index after drop table");
}
