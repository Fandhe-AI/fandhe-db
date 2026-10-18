//! `CREATE VIEW v (a, b) AS ...`（列別名リスト。TABLE-41 (1)・TABLE-18・Issue #1659）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/` の TABLE-41・TABLE-18・RLS-10 (b)。
//!
//! `table18_view.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。
//!
//! 検証する契約:
//! 1. 別名は出力列に先頭から順に名前を付け、短いリストの残りは導出名を保つ
//! 2. 別名が出力列より多いと `42601`、リスト内・最終名の重複は `42701`
//! 3. 別名でビューを参照でき、物理名・`tenant_id`／`visibility` は参照できない
//! 4. RLS は参照したセッションの `PolicyContext` で評価される（別名で変わらない）
//! 5. 別名を付けても返る行は変わらない（本文の行数上限に巻き込まれない）
//! 6. 未対応の参照位置は `0A000` で拒否し、DDL 依存ゲートは保守側に倒れる

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
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
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
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

fn insert_row(
    storage: &Storage,
    tenant: &str,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件（ja）。
fn seed(storage: &Storage) {
    insert_row(storage, "alice", 1, "ja", "a1", Visibility::Public);
    insert_row(storage, "alice", 2, "ja", "a2", Visibility::Public);
    insert_row(storage, "alice", 3, "en", "a3", Visibility::Public);
    insert_row(storage, "alice", 4, "ja", "a4", Visibility::Private);
    insert_row(storage, "bob", 5, "ja", "b1", Visibility::Public);
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage);
    (new_core(storage), guard)
}

fn ddl(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), session, sql)
}

fn create_ok(core: &EngineCore, sql: &str) {
    let mut session = allowed_session();
    ddl(core, &mut session, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn create_err(core: &EngineCore, sql: &str) -> String {
    let mut session = allowed_session();
    ddl(core, &mut session, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn q(core: &EngineCore, sql: &str) -> QueryResult {
    query(core, "alice", sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn q_code(core: &EngineCore, sql: &str) -> String {
    query(core, "alice", sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

fn names(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::AliasedId { name }
            | ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. } => name.clone(),
        })
        .collect()
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

// --- 1. 別名による列名・参照 --------------------------------------------------

/// 全列に別名を付けると結果の列名が別名になり、別名で `WHERE`／`ORDER BY` できる。
#[test]
fn full_alias_list_renames_output_and_is_usable_in_queries() {
    let (core, _g) = open_core("t41-full");
    create_ok(
        &core,
        "CREATE VIEW v (doc_id, vec, language, text) AS SELECT * FROM docs",
    );

    let all = q(&core, "SELECT * FROM v LIMIT 100");
    assert_eq!(names(&all), ["doc_id", "vec", "language", "text"]);
    assert_eq!(ids(&all), vec![1, 2, 3, 4, 5]);

    let ja = q(
        &core,
        "SELECT language, text FROM v WHERE language = 'ja' ORDER BY text LIMIT 10",
    );
    assert_eq!(names(&ja), ["language", "text"]);
    assert_eq!(ids(&ja), vec![1, 2, 4, 5]);
    let texts: Vec<&Cell> = ja.rows.iter().map(|r| &r.cells[1]).collect();
    assert_eq!(
        texts,
        [
            &Cell::Text("a1".into()),
            &Cell::Text("a2".into()),
            &Cell::Text("a4".into()),
            &Cell::Text("b1".into())
        ]
    );

    // 別名付きの `id`（先頭の別名）も投影できる。
    let only_id = q(&core, "SELECT doc_id FROM v LIMIT 10");
    assert_eq!(names(&only_id), ["doc_id"]);
    assert_eq!(ids(&only_id), vec![1, 2, 3, 4, 5]);
}

/// 短いリストでは残りの列が導出名のまま。`*` の本文の先頭は `id`。
#[test]
fn short_alias_list_keeps_derived_names_for_remaining_columns() {
    let (core, _g) = open_core("t41-short");
    create_ok(&core, "CREATE VIEW v1 (x) AS SELECT lang, body FROM docs");
    let r = q(&core, "SELECT * FROM v1 LIMIT 10");
    assert_eq!(names(&r), ["x", "body"]);
    assert_eq!(
        q(&core, "SELECT x FROM v1 WHERE x = 'en' LIMIT 10")
            .rows
            .len(),
        1
    );

    create_ok(&core, "CREATE VIEW v2 (x) AS SELECT * FROM docs");
    let r = q(&core, "SELECT * FROM v2 LIMIT 10");
    assert_eq!(names(&r), ["x", "embedding", "lang", "body"]);
    assert!(matches!(r.rows[0].cells[0], Cell::Integer(_)));
}

/// 集計も別名で参照でき、グループキーの出力名は別名のまま。
#[test]
fn aggregate_over_aliased_view_uses_aliases() {
    let (core, _g) = open_core("t41-agg");
    create_ok(
        &core,
        "CREATE VIEW v (doc_id, vec, language, text) AS SELECT * FROM docs",
    );
    let r = q(
        &core,
        "SELECT language, COUNT(*) FROM v GROUP BY language ORDER BY language",
    );
    assert_eq!(names(&r)[0], "language");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0].cells[0], Cell::Text("en".into()));
    assert_eq!(r.rows[0].cells[1], Cell::Integer(1));
    assert_eq!(r.rows[1].cells[0], Cell::Text("ja".into()));
    assert_eq!(r.rows[1].cells[1], Cell::Integer(4));
}

/// 評価後射影形の本文（集計・`LIMIT`）への別名。
#[test]
fn alias_over_buffered_body() {
    let (core, _g) = open_core("t41-buffered");
    create_ok(
        &core,
        "CREATE VIEW vb (l, n) AS SELECT lang, COUNT(*) AS cnt FROM docs GROUP BY lang",
    );
    let r = q(&core, "SELECT * FROM vb LIMIT 10");
    assert_eq!(names(&r), ["l", "n"]);
    assert_eq!(r.rows.len(), 2);
    let r = q(&core, "SELECT l FROM vb WHERE n = 4 LIMIT 10");
    assert_eq!(names(&r), ["l"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[0], Cell::Text("ja".into()));

    create_ok(
        &core,
        "CREATE VIEW vl (a) AS SELECT lang, body FROM docs LIMIT 2",
    );
    let r = q(&core, "SELECT * FROM vl LIMIT 10");
    assert_eq!(names(&r), ["a", "body"]);
    assert_eq!(r.rows.len(), 2);
}

/// 連鎖: 別名付きビューの上のビュー、別名付きの上の別名付き、評価後射影形の上の別名付き。
#[test]
fn chained_views_with_aliases() {
    let (core, _g) = open_core("t41-chain");
    create_ok(
        &core,
        "CREATE VIEW base_v (a, b, c, d) AS SELECT * FROM docs",
    );
    create_ok(
        &core,
        "CREATE VIEW outer_s AS SELECT c, d FROM base_v WHERE c = 'ja'",
    );
    let r = q(&core, "SELECT * FROM outer_s LIMIT 10");
    assert_eq!(names(&r), ["c", "d"]);
    assert_eq!(ids(&r), vec![1, 2, 4, 5]);

    create_ok(
        &core,
        "CREATE VIEW outer_a (language) AS SELECT c, d FROM base_v",
    );
    let r = q(
        &core,
        "SELECT language, d FROM outer_a WHERE language = 'en' LIMIT 10",
    );
    assert_eq!(names(&r), ["language", "d"]);
    assert_eq!(ids(&r), vec![3]);
    // 外側の別名では内側の別名は見えない。
    assert_eq!(q_code(&core, "SELECT c FROM outer_a LIMIT 10"), "42703");

    create_ok(
        &core,
        "CREATE VIEW buf AS SELECT lang, body FROM docs LIMIT 3",
    );
    create_ok(
        &core,
        "CREATE VIEW over_buf (l) AS SELECT lang, body FROM buf",
    );
    let r = q(&core, "SELECT * FROM over_buf LIMIT 10");
    assert_eq!(names(&r), ["l", "body"]);
    assert_eq!(r.rows.len(), 3);
}

// --- 2. 長さ・重複の検査 -------------------------------------------------------

#[test]
fn alias_list_longer_than_output_is_42601() {
    let (core, _g) = open_core("t41-long");
    // 明示列（構造段）
    assert_eq!(
        create_err(
            &core,
            "CREATE VIEW v (a, b, c) AS SELECT lang, body FROM docs"
        ),
        "42601"
    );
    // `*`（id・embedding・lang・body の 4 列）
    assert_eq!(
        create_err(&core, "CREATE VIEW v (a, b, c, d, e) AS SELECT * FROM docs"),
        "42601"
    );
    // 評価後射影形
    assert_eq!(
        create_err(
            &core,
            "CREATE VIEW v (a, b, c) AS SELECT lang, COUNT(*) FROM docs GROUP BY lang"
        ),
        "42601"
    );
    // 構文
    for sql in [
        "CREATE VIEW v () AS SELECT * FROM docs",
        "CREATE VIEW v (a,) AS SELECT * FROM docs",
        "CREATE VIEW v (a b) AS SELECT * FROM docs",
        "CREATE VIEW v (1) AS SELECT * FROM docs",
    ] {
        assert_eq!(create_err(&core, sql), "42601", "{sql}");
    }
    // 何も作られていない。
    assert_eq!(q_code(&core, "SELECT * FROM v LIMIT 1"), "42P01");
}

#[test]
fn duplicate_alias_names_are_42701() {
    let (core, _g) = open_core("t41-dup");
    assert_eq!(
        create_err(&core, "CREATE VIEW v (a, a) AS SELECT * FROM docs"),
        "42701"
    );
    // 別名と、残りの導出名の衝突。
    assert_eq!(
        create_err(&core, "CREATE VIEW v (lang) AS SELECT body, lang FROM docs"),
        "42701"
    );
}

/// 長さの違反でも DDL 権限の無い主体にはカタログ照会より前に `42501`。
#[test]
fn permission_gate_precedes_catalog_dependent_alias_checks() {
    let (core, _g) = open_core("t41-perm");
    let mut denied = SessionState::default();
    let err = ddl(
        &core,
        &mut denied,
        "CREATE VIEW v (a, b, c, d, e) AS SELECT * FROM docs",
    )
    .expect_err("denied");
    assert_eq!(err.wire_code(), "42501");
}

// --- 3. 公開範囲 --------------------------------------------------------------

#[test]
fn physical_and_reserved_columns_are_not_visible_through_aliases() {
    let (core, _g) = open_core("t41-scope");
    create_ok(&core, "CREATE VIEW v (a, b, c, d) AS SELECT * FROM docs");
    for sql in [
        "SELECT lang FROM v LIMIT 10",
        "SELECT a FROM v WHERE lang = 'ja' LIMIT 10",
        "SELECT a FROM v ORDER BY body LIMIT 10",
        "SELECT tenant_id FROM v LIMIT 10",
        "SELECT a FROM v WHERE visibility = 'public' LIMIT 10",
        "SELECT lang, COUNT(*) FROM v GROUP BY lang",
    ] {
        assert_eq!(q_code(&core, sql), "42703", "{sql}");
    }
}

/// 別名を `tenant_id` にしても、外側は物理の予約列ではなく別名の利用者列として束縛される。
#[test]
fn alias_named_like_reserved_column_binds_the_user_column() {
    let (core, _g) = open_core("t41-reserved");
    create_ok(&core, "CREATE VIEW vt (tenant_id) AS SELECT lang FROM docs");
    let r = q(
        &core,
        "SELECT tenant_id FROM vt WHERE tenant_id = 'ja' LIMIT 10",
    );
    assert_eq!(names(&r), ["tenant_id"]);
    assert_eq!(ids(&r), vec![1, 2, 4, 5]);
    let r = query(&core, "bob", "SELECT tenant_id FROM vt LIMIT 10").expect("bob");
    // bob には alice の public 行（1〜3）と自分の public 行（5）が見える。
    assert_eq!(r.rows.len(), 4);
}

/// 別名同士の入れ替え（`id` と利用者列）でも一括置換で正しく写る。
#[test]
fn swapped_alias_names_resolve_in_one_pass() {
    let (core, _g) = open_core("t41-swap");
    create_ok(
        &core,
        "CREATE VIEW vs (lang, body) AS SELECT body, lang FROM docs",
    );
    let r = q(
        &core,
        "SELECT lang, body FROM vs WHERE lang = 'a3' LIMIT 10",
    );
    assert_eq!(names(&r), ["lang", "body"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[0], Cell::Text("a3".into()));
    assert_eq!(r.rows[0].cells[1], Cell::Text("en".into()));
}

// --- 4. RLS -------------------------------------------------------------------

#[test]
fn aliased_view_applies_referencing_session_rls() {
    let (core, _g) = open_core("t41-rls");
    create_ok(
        &core,
        "CREATE VIEW v (doc_id, vec, language, text) AS SELECT * FROM docs",
    );
    for tenant in ["alice", "bob", "carol"] {
        let via_view = query(&core, tenant, "SELECT doc_id FROM v LIMIT 100").expect("view");
        let direct = query(&core, tenant, "SELECT id FROM docs LIMIT 100").expect("direct");
        assert_eq!(ids(&via_view), ids(&direct), "tenant={tenant}");
    }
    // alice の private 行（4）は他テナントに見えない。
    let bob = query(&core, "bob", "SELECT doc_id FROM v LIMIT 100").expect("bob");
    assert!(!ids(&bob).contains(&4));
}

// --- 5. 行が変わらない不変条件 -------------------------------------------------

/// 本文の既定上限（10,000 行）を超える表でも、別名付きの単純形ビューは別名なしの
/// 同等のビューと同じ件数を返す（評価後射影形へ逃がさない）。
#[test]
fn alias_does_not_change_returned_rows_over_the_default_body_cap() {
    let path = unique_db_path("t41-cap");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let total = 10_300u64;
    // 選択的な `rare` 行は 10,000 行目より後ろにだけ置く（本文を評価後に絞り込む経路なら欠ける）。
    for i in 1..=total {
        let lang = if i > 10_100 { "rare" } else { "common" };
        insert_row(&storage, "alice", i, lang, "b", Visibility::Public);
    }
    let core = new_core(storage);
    create_ok(&core, "CREATE VIEW plain AS SELECT * FROM docs");
    create_ok(
        &core,
        "CREATE VIEW aliased (a, b, c, d) AS SELECT * FROM docs",
    );
    let plain = q(
        &core,
        "SELECT id FROM plain WHERE lang = 'rare' LIMIT 10000",
    );
    let aliased = q(&core, "SELECT a FROM aliased WHERE c = 'rare' LIMIT 10000");
    assert_eq!(plain.rows.len(), 200);
    assert_eq!(aliased.rows.len(), plain.rows.len());
}

// --- 6. 未対応の位置・DDL ゲート ----------------------------------------------

#[test]
fn unsupported_positions_fail_closed_without_side_effects() {
    let (core, _g) = open_core("t41-unsupported");
    create_ok(&core, "CREATE VIEW v (a, b, c, d) AS SELECT * FROM docs");
    for sql in [
        // 集合演算の枝
        "SELECT c FROM v UNION SELECT lang FROM docs",
        // CTE
        "WITH x AS (SELECT c FROM v) SELECT * FROM x LIMIT 10",
        // ウィンドウ関数
        "SELECT c, ROW_NUMBER() OVER (ORDER BY d) FROM v LIMIT 10",
        // 疑似列 `id` へ写る別名を述語で参照する
        "SELECT c FROM v WHERE a = '1' LIMIT 10",
    ] {
        let code = q_code(&core, sql);
        assert!(
            code == "0A000" || code == "42601",
            "unexpected code {code} for {sql}"
        );
    }
    // 副作用なし: 通常の参照は引き続き動く。
    assert_eq!(q(&core, "SELECT c FROM v LIMIT 10").rows.len(), 5);
}

#[test]
fn ddl_gates_are_conservative_for_aliased_views() {
    let (core, _g) = open_core("t41-ddl");
    create_ok(&core, "CREATE VIEW v (a, b, c, d) AS SELECT * FROM docs");
    let mut session = allowed_session();
    let err = ddl(
        &core,
        &mut session,
        "ALTER TABLE docs RENAME COLUMN body TO memo",
    )
    .expect_err("rename must be refused while an aliased view reads the table");
    assert_eq!(err.wire_code(), "0A000");
    let err = ddl(&core, &mut session, "ALTER TABLE docs DROP COLUMN body")
        .expect_err("drop must be refused while an aliased view reads the table");
    assert_eq!(err.wire_code(), "2BP01");
    let err = ddl(&core, &mut session, "DROP TABLE docs").expect_err("dependent view");
    assert_eq!(err.wire_code(), "2BP01");
    // 別名のないビューは従来どおり追随できる。
    let (core2, _g2) = open_core("t41-ddl-plain");
    create_ok(&core2, "CREATE VIEW v AS SELECT lang, body FROM docs");
    let mut s2 = allowed_session();
    ddl(
        &core2,
        &mut s2,
        "ALTER TABLE docs RENAME COLUMN body TO memo",
    )
    .expect("plain view follows the rename");
}

/// 再起動（再オープン）後も別名は保たれる。
#[test]
fn aliases_persist_across_reopen() {
    let path = unique_db_path("t41-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed(&storage);
        let core = new_core(storage);
        create_ok(&core, "CREATE VIEW v (a, b, c, d) AS SELECT * FROM docs");
    }
    let core = new_core(Storage::open(&path).expect("reopen"));
    let r = q(&core, "SELECT * FROM v LIMIT 10");
    assert_eq!(names(&r), ["a", "b", "c", "d"]);
}

/// Describe（拡張クエリ）の列メタデータは、単純形・評価後射影形のどちらでも実行結果と一致する。
#[test]
fn describe_matches_execute_for_aliased_views() {
    let (core, _g) = open_core("t41-describe");
    create_ok(&core, "CREATE VIEW v (a, b, c, d) AS SELECT * FROM docs");
    create_ok(
        &core,
        "CREATE VIEW vb (l, n) AS SELECT lang, COUNT(*) AS cnt FROM docs GROUP BY lang",
    );
    create_ok(&core, "CREATE VIEW vs (x) AS SELECT lang, body FROM docs");
    let session = SessionState::default();
    for sql in [
        "SELECT * FROM v LIMIT 10",
        "SELECT c, d FROM v WHERE c = 'ja' LIMIT 10",
        "SELECT * FROM vb LIMIT 10",
        "SELECT n, l FROM vb LIMIT 10",
        "SELECT * FROM vs LIMIT 10",
    ] {
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = q(&core, sql);
        assert_eq!(described, executed.columns, "sql={sql}");
    }
}
/// ALTER TYPE は別名付きビューが読む表では拒否され、評価後射影形ビューへの `ORDER BY` は
/// 別名で動く。
#[test]
fn alter_type_gate_and_order_by_over_buffered_alias() {
    let (core, _g) = open_core("t41-alter-type");
    create_ok(&core, "CREATE VIEW v (a, b, c, d) AS SELECT * FROM docs");
    // 一般変換（行の書き換えを伴う ALTER TYPE）は、別名付きビューが読む表では拒否する。
    let mut session = allowed_session();
    ddl(
        &core,
        &mut session,
        "CREATE TABLE nums (n BIGINT, m BIGINT)",
    )
    .expect("create nums");
    create_ok(&core, "CREATE VIEW vn_star (x, y, z) AS SELECT * FROM nums");
    create_ok(&core, "CREATE VIEW vn_cols (k) AS SELECT n FROM nums");
    let err = ddl(
        &core,
        &mut session,
        "ALTER TABLE nums ALTER COLUMN m TYPE INTEGER",
    )
    .expect_err("alter type must be refused while an aliased view reads the table");
    assert!(
        matches!(err.wire_code(), "0A000" | "2BP01"),
        "code={}",
        err.wire_code()
    );
    for view in ["vn_star", "vn_cols"] {
        ddl(&core, &mut session, &format!("DROP VIEW {view}")).expect("drop view");
    }
    ddl(
        &core,
        &mut session,
        "ALTER TABLE nums ALTER COLUMN m TYPE INTEGER",
    )
    .expect("after the aliased views are gone the conversion is allowed");

    create_ok(
        &core,
        "CREATE VIEW vb (l, n) AS SELECT lang, COUNT(*) AS cnt FROM docs GROUP BY lang",
    );
    let sql = "SELECT l FROM vb ORDER BY n LIMIT 10";
    let r = q(&core, sql);
    assert_eq!(names(&r), ["l"]);
    assert_eq!(r.rows[0].cells[0], Cell::Text("en".into()));
    let parsed = core.parse_sql(sql).expect("parse");
    let described = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect("describe")
        .expect("has columns");
    assert_eq!(described, r.columns);
}

// --- 7. レビュー指摘の回帰（Issue #1659） -------------------------------------

/// `CAST`・`::`・添字の既定出力名は、書き換え前の公開名から導く（隠れた物理名にしない）。
#[test]
fn default_output_name_of_cast_follows_the_public_alias() {
    let (core, _g) = open_core("t41-cast-name");
    create_ok(&core, "CREATE VIEW vc (x) AS SELECT lang FROM docs");
    let r = q(&core, "SELECT CAST(x AS TEXT) FROM vc LIMIT 10");
    assert_eq!(names(&r), ["x"]);
    let r = q(&core, "SELECT x::TEXT FROM vc LIMIT 10");
    assert_eq!(names(&r), ["x"]);
}

/// 評価後射影形の本文: 作成後に基底表へ追加された列が別名と衝突したら、参照時に `42702` で
/// 拒否する（`SELECT *` が同名列を 2 つ返さない。単純形経路と同じ fail-closed）。
#[test]
fn buffered_alias_collision_after_add_column_is_42702() {
    let (core, _g) = open_core("t41-buffered-collision");
    create_ok(
        &core,
        "CREATE VIEW vd (extra) AS SELECT * FROM docs LIMIT 10",
    );
    assert_eq!(names(&q(&core, "SELECT * FROM vd LIMIT 10"))[0], "extra");
    create_ok(&core, "ALTER TABLE docs ADD COLUMN extra TEXT");
    assert_eq!(q_code(&core, "SELECT * FROM vd LIMIT 10"), "42702");
}

/// 実カラム `id` を宣言する表の `SELECT *` は疑似列 `id` と実カラム `id` の 2 列を返す。名前で
/// 疑似列を指せないため、別名付きビューは値を取り違えず `0A000` で拒否する。
#[test]
fn star_over_table_with_declared_id_column_is_rejected_not_misaligned() {
    let path = unique_db_path("t41-declared-id");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "rid",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = new_core(storage);
    assert_eq!(
        create_err(&core, "CREATE VIEW vr (a, b, c) AS SELECT * FROM rid"),
        "0A000"
    );
    assert_eq!(
        create_err(&core, "CREATE VIEW vr (a) AS SELECT * FROM rid"),
        "0A000"
    );
}

/// 実カラム `id` を宣言する表でも、明示列だけの本文は星の展開を行わず別名付きで作成できる。
#[test]
fn explicit_columns_over_table_with_declared_id_column_are_accepted() {
    let path = unique_db_path("t41-declared-id-explicit");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "rid",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = new_core(storage);
    create_ok(&core, "CREATE VIEW vr (x) AS SELECT body FROM rid");
}

/// 評価後射影形ビューの連鎖（星の列並びが必要）を write txn 内の再検証でも解決できる。
#[test]
fn chained_buffered_views_over_aliased_star_view_are_creatable() {
    let (core, _g) = open_core("t41-txn-chain");
    create_ok(&core, "CREATE VIEW a (x) AS SELECT * FROM docs");
    create_ok(&core, "CREATE VIEW b AS SELECT COUNT(*) AS n FROM a");
    create_ok(&core, "CREATE VIEW c AS SELECT COUNT(*) FROM b");
}

/// 別名付きビューの上の LIMIT 付き本文は、作成時と参照時の契約が一致する（保存できれば参照できる）。
#[test]
fn buffered_body_over_aliased_view_is_consistent_between_create_and_read() {
    let (core, _g) = open_core("t41-buffered-over-alias");
    create_ok(&core, "CREATE VIEW a (x) AS SELECT body FROM docs");
    create_ok(&core, "CREATE VIEW b AS SELECT x FROM a LIMIT 2");
    let r = q(&core, "SELECT * FROM b LIMIT 10");
    assert_eq!(names(&r), ["x"]);
}

/// 別名なしビューが別名付きビューを集合演算の枝で読む定義は、参照時と同じく作成時に拒否する
/// （作成できても参照時に `XX000` になる契約の不一致を作らない）。
#[test]
fn set_operation_over_aliased_view_is_rejected_at_create() {
    let (core, _g) = open_core("t41-setop-over-alias");
    create_ok(&core, "CREATE VIEW a (x) AS SELECT body FROM docs");
    let code = create_err(
        &core,
        "CREATE VIEW b AS SELECT x FROM a UNION SELECT body FROM docs",
    );
    assert_eq!(code, "42601");
    // 副作用なし: b は作られていない。
    assert_eq!(q_code(&core, "SELECT * FROM b LIMIT 10"), "42P01");
}
