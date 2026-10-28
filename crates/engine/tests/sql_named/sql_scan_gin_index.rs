//! jsonb の包含・キー存在・jsonpath 演算子を GIN 相当索引の候補探査で実行する広域取得の結合テスト
//! （Issue #2208・INDEX-8。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-7・INDEX-8、
//! `data-model.md` TABLE-12、`rls.md` RLS-9・RLS-10、`sql-surface.md` SQL-6・SQL-27。設計は
//! `docs/design/gin-jsonb-index.md` §9）。
//!
//! 検証の軸は 2 つ。(1) 索引化できる形のときだけ `EXPLAIN` が `index_gin index=<名>`／
//! `gin_index_candidates` になり、それ以外は従来どおり `plain_scan`／`full_scan` のまま変わらないこと。
//! (2) どの経路でも結果（行・順序・`LIMIT`／`OFFSET` の数え方）が、GIN を宣言しない同形の表の全走査と
//! バイト単位で一致すること（偽陰性・stale・縮退・テナント境界・トランザクションを含む）。
//! 実際に索引経路を通ったことの構造的証拠（探査回数・縮退回数）は `src/sql/gin_scan.rs` の単体テストが
//! 計測フックで示す。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn public_only(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run(core: &EngineCore, c: &PolicyContext, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(c, s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn outcome_rows(sql: &str, outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Query(QueryResult { rows, .. }) => {
            rows.iter().map(|r| format!("{:?}", r.cells)).collect()
        }
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn rows_of(core: &EngineCore, c: &PolicyContext, sql: &str) -> Vec<String> {
    let mut s = SessionState::default();
    outcome_rows(sql, run(core, c, &mut s, sql))
}

fn explain_lines(core: &EngineCore, c: &PolicyContext, sql: &str) -> Vec<String> {
    let mut s = SessionState::default();
    match run(core, c, &mut s, &format!("EXPLAIN {sql}")) {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|r| match &r.cells[0] {
                Cell::Text(t) => t.clone(),
                other => panic!("expected text cell, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn assert_gin(core: &EngineCore, c: &PolicyContext, sql: &str, index: &str) {
    let lines = explain_lines(core, c, sql);
    assert!(
        lines
            .iter()
            .any(|l| *l == format!("scalar_plan: index_gin index={index}")),
        "{sql}: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "access_path: gin_index_candidates"),
        "{sql}: {lines:?}"
    );
}

fn assert_full_scan(core: &EngineCore, c: &PolicyContext, sql: &str) {
    let lines = explain_lines(core, c, sql);
    assert!(
        lines.iter().any(|l| l == "scalar_plan: plain_scan"),
        "{sql}: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "access_path: full_scan"),
        "{sql}: {lines:?}"
    );
}

fn jsonb_table(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("doc", ColumnType::Jsonb, true)])
}

/// `g`（`jsonb_ops` の `g_ops` と `jsonb_path_ops` の `g_path` を宣言）、`p`（GIN なし）、
/// `po`（`jsonb_path_ops` のみ）、`oo`（`jsonb_ops` のみ）へ同じ行を同じ id で入れる。
struct Fixture {
    core: EngineCore,
    _guard: CleanupGuard,
}

const DOCS: &[&str] = &[
    r#"{"a":1,"b":"x"}"#,
    r#"{"a":1.0,"b":"y"}"#,
    r#"{"a":10e-1}"#,
    r#"{"a":2,"b":"x","c":[1,2,3]}"#,
    r#"{"a":{"n":"p"},"b":"z"}"#,
    r#"{"a":[{"n":"p"},{"n":"q"}]}"#,
    r#"["x","y"]"#,
    r#"["bar"]"#,
    r#""bar""#,
    r#"{"c":[3]}"#,
    r#"{"a":"bar"}"#,
    r#"{}"#,
    r#"[]"#,
    r#"{"k":{"deep":{"er":true}}}"#,
];

const TABLES: [&str; 4] = ["g", "p", "po", "oo"];

fn fixture(label: &str) -> Fixture {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in TABLES {
        storage.create_table(&jsonb_table(t)).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = ddl_session();
    let alice = ctx("alice");
    for sql in [
        "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
        "CREATE INDEX g_path ON g USING gin (doc jsonb_path_ops)",
        "CREATE INDEX po_path ON po USING gin (doc jsonb_path_ops)",
        "CREATE INDEX oo_ops ON oo USING gin (doc jsonb_ops)",
    ] {
        run(&core, &alice, &mut s, sql);
    }
    let f = Fixture {
        core,
        _guard: guard,
    };
    for (i, d) in DOCS.iter().enumerate() {
        f.insert("alice", i as u64 + 1, d);
    }
    f
}

impl Fixture {
    fn insert(&self, tenant: &str, id: u64, doc: &str) {
        let c = ctx(tenant);
        let mut s = SessionState::default();
        for t in TABLES {
            run(
                &self.core,
                &c,
                &mut s,
                &format!("INSERT INTO {t} (id, doc) VALUES ({id}, '{doc}')"),
            );
        }
    }

    /// `g`・`po`・`oo` と GIN なしの `p` で同じ SELECT を実行し、全表の結果が一致することを確認して返す。
    fn same(&self, c: &PolicyContext, where_and_tail: &str) -> Vec<String> {
        let plain = rows_of(
            &self.core,
            c,
            &format!("SELECT id, doc FROM p WHERE {where_and_tail}"),
        );
        for t in ["g", "po", "oo"] {
            let got = rows_of(
                &self.core,
                c,
                &format!("SELECT id, doc FROM {t} WHERE {where_and_tail}"),
            );
            assert_eq!(got, plain, "{t}: {where_and_tail}");
        }
        plain
    }
}

// --- 結果の一致と EXPLAIN ----------------------------------------------------

#[test]
fn every_operator_matches_the_full_scan_and_explain_names_the_index() {
    let f = fixture("gin-scan-match");
    let c = ctx("alice");
    // (WHERE 句, `g` で選ばれる索引名)
    let indexed: &[(&str, &str)] = &[
        (r#"doc @> '{"a":1}'"#, "g_path"),
        (r#"doc @> '{"a":1.0}'"#, "g_path"),
        (r#"doc @> '{"a":{"n":"p"}}'"#, "g_path"),
        (r#"doc @> '{"a":[{"n":"q"}]}'"#, "g_path"),
        (r#"doc @> '["x"]'"#, "g_path"),
        (r#"doc @> '"bar"'"#, "g_path"),
        (r#"doc @> '{"c":[1,3]}'"#, "g_path"),
        // 葉が無い問い合わせは `jsonb_path_ops` では索引化できず、`jsonb_ops`（キー）が選ばれる。
        (r#"doc @> '{"k":{"deep":{}}}'"#, "g_ops"),
        ("doc ? 'b'", "g_ops"),
        ("doc ? 'bar'", "g_ops"),
        ("doc ?| '{b,c,x}'", "g_ops"),
        ("doc ?& '{a,b}'", "g_ops"),
        (r#"doc @@ '$.a == 1'"#, "g_path"),
        (r#"doc @@ '$.b == "x"'"#, "g_path"),
        (r#"doc @? '$.a ? (@.n == "p")'"#, "g_path"),
        (r#"doc @> '{"a":1}' AND doc ? 'b'"#, "g_path"),
    ];
    for (w, index) in indexed {
        let sql = format!("SELECT id, doc FROM g WHERE {w} LIMIT 100");
        assert_gin(&f.core, &c, &sql, index);
        f.same(&c, &format!("{w} LIMIT 100"));
    }
    // 結果が空でないこと（検証が空振りしていない）。
    assert_eq!(f.same(&c, r#"doc @> '{"a":1}' LIMIT 100"#).len(), 3);
    assert_eq!(f.same(&c, "doc ?& '{a,b}' LIMIT 100").len(), 4);
    assert_eq!(f.same(&c, r#"doc @> '"bar"' LIMIT 100"#).len(), 2);
    // `jsonb_ops` だけの表と `jsonb_path_ops` だけの表で、使える演算子が分かれる。
    assert_gin(
        &f.core,
        &c,
        r#"SELECT id FROM oo WHERE doc @> '{"a":1}' LIMIT 10"#,
        "oo_ops",
    );
    assert_gin(
        &f.core,
        &c,
        "SELECT id FROM oo WHERE doc ? 'a' LIMIT 10",
        "oo_ops",
    );
    assert_gin(
        &f.core,
        &c,
        r#"SELECT id FROM po WHERE doc @@ '$.a == 1' LIMIT 10"#,
        "po_path",
    );
    assert_full_scan(&f.core, &c, "SELECT id FROM po WHERE doc ? 'a' LIMIT 10");
    assert_full_scan(
        &f.core,
        &c,
        r#"SELECT id FROM oo WHERE doc @@ '$.a == 1' LIMIT 10"#,
    );
    // GIN を宣言しない表は従来どおり。
    assert_full_scan(
        &f.core,
        &c,
        r#"SELECT id FROM p WHERE doc @> '{"a":1}' LIMIT 10"#,
    );
}

#[test]
fn non_indexable_shapes_keep_the_full_scan_and_the_same_results() {
    let f = fixture("gin-scan-fallback");
    let c = ctx("alice");
    for w in [
        // 空集合（「索引から全行」を返す経路を作らない）
        "doc @> '{}'",
        "doc @> '[]'",
        "doc ?& '{}'",
        "doc ?| '{}'",
        // 逆向き・左辺リテラル
        r#"doc <@ '{"a":1,"b":"x"}'"#,
        r#"'{"a":1}' @> doc"#,
        // 許可形でない jsonpath・関数形
        r#"doc @@ '$.a > 1'"#,
        r#"doc @@ 'strict $.a == 1'"#,
        r#"doc @@ '$.a[0] == 1'"#,
        r#"doc @@ '$.a == null'"#,
        r#"doc @? '$.a.n'"#,
        r#"doc @? '$.a[*] ? (@.n == "p")'"#,
        r#"jsonb_path_exists(doc, '$.a ? (@ == 1)')"#,
        r#"jsonb_path_match(doc, '$.a == 1')"#,
        // OR（or_filters）
        r#"doc @> '{"a":1}' OR doc ? 'b'"#,
    ] {
        assert_full_scan(
            &f.core,
            &c,
            &format!("SELECT id, doc FROM g WHERE {w} LIMIT 100"),
        );
        f.same(&c, &format!("{w} LIMIT 100"));
    }
    // ORDER BY 付きは対象外のまま（順序付き経路は索引を使わない）。
    let ordered = r#"SELECT id, doc FROM g WHERE doc @> '{"a":1}' ORDER BY id LIMIT 100"#;
    assert_full_scan(&f.core, &c, ordered);
    assert_eq!(
        rows_of(&f.core, &c, ordered),
        rows_of(
            &f.core,
            &c,
            r#"SELECT id, doc FROM p WHERE doc @> '{"a":1}' ORDER BY id LIMIT 100"#
        )
    );
}

#[test]
fn limit_and_offset_count_rows_like_the_full_scan() {
    let f = fixture("gin-scan-limit");
    let c = ctx("alice");
    for n in 20..80u64 {
        let doc = match n % 4 {
            0 => format!(r#"{{"a":{},"b":"x"}}"#, n % 3),
            1 => format!(r#"{{"b":"x","k{}":1}}"#, n % 5),
            2 => r#"["x",1]"#.to_string(),
            _ => r#"{"a":1,"c":[3]}"#.to_string(),
        };
        f.insert("alice", n, &doc);
    }
    for w in [
        r#"doc @> '{"a":1}'"#,
        "doc ?| '{k0,k1,k2,c}'",
        "doc ?& '{a,b}'",
        r#"doc @@ '$.a == 1'"#,
    ] {
        for tail in [
            "LIMIT 100",
            "LIMIT 3",
            "LIMIT 3 OFFSET 2",
            "LIMIT 1 OFFSET 5",
            "LIMIT 100 OFFSET 7",
            "LIMIT 100 OFFSET 1000",
        ] {
            f.same(&c, &format!("{w} {tail}"));
        }
    }
}

// --- 偽陰性の回帰・stale・縮退 ---------------------------------------------------

#[test]
fn number_notation_top_level_string_and_array_transparency_are_found() {
    let f = fixture("gin-scan-false-negative");
    let c = ctx("alice");
    // `{"a":1.0}`・`{"a":10e-1}` で `{"a":1}` が見つかる（数値は値として正規化）。
    assert_eq!(f.same(&c, r#"doc @> '{"a":1}' LIMIT 100"#).len(), 3);
    // `["bar"] @> "bar"`（最上位の文字列スカラーは配列要素として包含される）。
    assert!(f
        .same(&c, r#"doc @> '"bar"' LIMIT 100"#)
        .iter()
        .any(|r| r.contains("bar")));
    // `jsonb_path_ops` は配列を透過する（`{"a":[{"n":"q"}]}` に `{"a":{"n":"q"}}` は含まれないが
    // 候補にはなり、再検査で除かれる）。
    f.same(&c, r#"doc @> '{"a":{"n":"q"}}' LIMIT 100"#);
    // 入れ子のキーは `?` の対象ではない（索引に入れてあっても再検査で除かれる）。
    assert!(f.same(&c, "doc ? 'deep' LIMIT 100").is_empty());
    assert!(f.same(&c, "doc ? 'n' LIMIT 100").is_empty());
}

#[test]
fn update_and_delete_leave_stale_postings_that_are_rechecked() {
    let f = fixture("gin-scan-stale");
    let c = ctx("alice");
    let mut s = SessionState::default();
    for t in TABLES {
        run(
            &f.core,
            &c,
            &mut s,
            &format!(r#"UPDATE {t} SET doc = '{{"a":9,"z":1}}' WHERE id = 1"#),
        );
        run(
            &f.core,
            &c,
            &mut s,
            &format!("DELETE FROM {t} WHERE id = 2"),
        );
    }
    assert_eq!(f.same(&c, r#"doc @> '{"a":1}' LIMIT 100"#).len(), 1);
    assert_eq!(f.same(&c, r#"doc @> '{"a":9}' LIMIT 100"#).len(), 1);
    assert_eq!(f.same(&c, "doc ? 'z' LIMIT 100").len(), 1);
    assert_eq!(f.same(&c, "doc ?& '{a,b}' LIMIT 100").len(), 2);
}

#[test]
fn a_stale_marker_degrades_to_the_row_range_scan_with_identical_results() {
    let f = fixture("gin-scan-marker");
    let c = ctx("alice");
    let mut s = ddl_session();
    // RENAME COLUMN はシグネチャ（列名を含む）を不一致にする。次の書き込みまで遅延再構築されない。
    for t in TABLES {
        run(
            &f.core,
            &c,
            &mut s,
            &format!("ALTER TABLE {t} RENAME COLUMN doc TO body"),
        );
    }
    for w in [
        r#"body @> '{"a":1}' LIMIT 100"#,
        "body ?& '{a,b}' LIMIT 100",
        "body ?| '{b,c}' LIMIT 100",
    ] {
        let want = rows_of(&f.core, &c, &format!("SELECT id, body FROM p WHERE {w}"));
        assert!(!want.is_empty());
        for t in ["g", "po", "oo"] {
            assert_eq!(
                rows_of(&f.core, &c, &format!("SELECT id, body FROM {t} WHERE {w}")),
                want,
                "{t}: {w}"
            );
        }
    }
}

// --- テナント境界 ---------------------------------------------------------------

#[test]
fn other_tenants_rows_are_filtered_by_visibility_exactly_like_the_full_scan() {
    let path = unique_db_path("gin-scan-tenants");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in ["g", "p"] {
        storage.create_table(&jsonb_table(t)).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = ddl_session();
    run(
        &core,
        &ctx("owner"),
        &mut s,
        "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
    );
    run(
        &core,
        &ctx("owner"),
        &mut s,
        "CREATE INDEX g_path ON g USING gin (doc jsonb_path_ops)",
    );
    // 各テナントの Private 行（SQL の INSERT は可視性を Private に固定する）。
    for (tenant, id) in [("alice", 1), ("bob", 2), ("carol", 3), ("dave", 4)] {
        for t in ["g", "p"] {
            run(
                &core,
                &ctx(tenant),
                &mut s,
                &format!(
                    r#"INSERT INTO {t} (id, doc) VALUES ({id}, '{{"a":1,"who":"{tenant}"}}')"#
                ),
            );
        }
    }
    let ids = |c: &PolicyContext, t: &str, w: &str| -> Vec<String> {
        rows_of(
            &core,
            c,
            &format!("SELECT id, doc FROM {t} WHERE {w} LIMIT 100"),
        )
    };
    for w in [
        r#"doc @> '{"a":1}'"#,
        "doc ? 'who'",
        "doc ?& '{a,who}'",
        r#"doc @@ '$.a == 1'"#,
    ] {
        for c in [
            public_only("alice"),
            public_only("erin"),
            ctx("alice"),
            ctx("bob"),
            ctx("erin"),
        ] {
            assert_eq!(ids(&c, "g", w), ids(&c, "p", w), "{w}");
        }
        // 自分の Private 行だけが見え、他テナントの Private 行は現れない。
        assert_eq!(ids(&ctx("bob"), "g", w).len(), 1, "{w}");
        assert!(ids(&public_only("bob"), "g", w).is_empty(), "{w}");
        assert!(ids(&ctx("erin"), "g", w).is_empty(), "{w}");
    }
    // 他テナントの Private 行にだけ存在する値の問い合わせは、存在しない値と応答が同一。
    assert_eq!(
        ids(&ctx("erin"), "g", r#"doc @> '{"who":"alice"}'"#),
        ids(&ctx("erin"), "g", r#"doc @> '{"who":"nobody"}'"#)
    );
    assert_eq!(
        ids(&ctx("erin"), "g", r#"doc @@ '$.who == "alice"'"#),
        ids(&ctx("erin"), "g", r#"doc @@ '$.who == "nobody"'"#)
    );
}

#[test]
fn public_rows_written_through_the_rust_api_are_found_across_tenants() {
    let path = unique_db_path("gin-scan-public");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in ["g", "p"] {
        storage.create_table(&jsonb_table(t)).expect("create table");
    }
    for table in ["g", "p"] {
        for (id, tenant, vis, doc) in [
            (1u64, "alice", Visibility::Public, r#"{"a":1}"#),
            (2, "bob", Visibility::Private, r#"{"a":1}"#),
            (3, "carol", Visibility::Public, r#"{"a":1,"b":2}"#),
            (4, "dave", Visibility::Public, r#"{"z":1}"#),
        ] {
            let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
                "{table}-gin-op-{id}"
            ))
            .expect("op id");
            engine::tenant::insert_typed_row(
                &storage,
                table,
                &ctx(tenant),
                id,
                vis,
                &[engine::row_codec::Value::Json(doc.to_string())],
                &op_id,
            )
            .expect("insert");
        }
    }
    // 行を入れた後で索引を宣言する（`CREATE INDEX` の初回構築が全テナント・全可視性の行を対象にする）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = ddl_session();
    run(
        &core,
        &ctx("owner"),
        &mut s,
        "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
    );
    for (c, expected) in [
        (public_only("alice"), 2usize),
        (public_only("erin"), 2),
        (public_only("bob"), 2),
        (ctx("bob"), 3),
        (ctx("alice"), 2),
    ] {
        for w in [r#"doc @> '{"a":1}'"#, "doc ? 'a'", "doc ?| '{a,z}'"] {
            let on_g = rows_of(
                &core,
                &c,
                &format!("SELECT id, doc FROM g WHERE {w} LIMIT 100"),
            );
            let on_p = rows_of(
                &core,
                &c,
                &format!("SELECT id, doc FROM p WHERE {w} LIMIT 100"),
            );
            assert_eq!(on_g, on_p, "{w}");
        }
        assert_eq!(
            rows_of(
                &core,
                &c,
                r#"SELECT id FROM g WHERE doc @> '{"a":1}' LIMIT 100"#
            )
            .len(),
            expected
        );
    }
}

// --- 経路の優先順位・トランザクション・一時表 -----------------------------------------

#[test]
fn primary_key_point_lookup_takes_precedence_over_gin() {
    let path = unique_db_path("gin-scan-pk");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&jsonb_table("seed")).expect("seed");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = ddl_session();
    let c = ctx("alice");
    run(
        &core,
        &c,
        &mut s,
        "CREATE TABLE k (code TEXT NOT NULL PRIMARY KEY, doc JSONB)",
    );
    run(&core, &c, &mut s, "CREATE INDEX k_ops ON k USING gin (doc)");
    for (i, code) in ["a", "b", "c"].iter().enumerate() {
        run(
            &core,
            &c,
            &mut s,
            &format!(r#"INSERT INTO k (code, doc) VALUES ('{code}', '{{"n":{i}}}')"#),
        );
    }
    let both = r#"SELECT code FROM k WHERE code = 'b' AND doc @> '{"n":1}' LIMIT 10"#;
    let lines = explain_lines(&core, &c, both);
    assert!(
        lines.iter().any(|l| l == "scalar_plan: index_primary_key"),
        "{lines:?}"
    );
    assert_eq!(rows_of(&core, &c, both).len(), 1);
    // PK の等価が無ければ GIN になる。
    let only_gin = r#"SELECT code FROM k WHERE doc @> '{"n":1}' LIMIT 10"#;
    assert_gin(&core, &c, only_gin, "k_ops");
    assert_eq!(rows_of(&core, &c, only_gin).len(), 1);
}

#[test]
fn rows_inserted_in_the_same_transaction_are_found_through_the_index() {
    let f = fixture("gin-scan-txn");
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = f.core.new_session_transaction();
    let mut tx = |sql: &str| f.core.execute_sql_in_txn(&c, &mut session, &mut txn, sql);
    tx("BEGIN").expect("begin");
    tx(r#"INSERT INTO g (id, doc) VALUES (900, '{"txn":1,"a":1}')"#).expect("insert");
    let sql = r#"SELECT id FROM g WHERE doc @> '{"txn":1}' LIMIT 10"#;
    let inside = outcome_rows(sql, tx(sql).expect("select in txn"));
    assert_eq!(inside.len(), 1);
    let sql_key = "SELECT id FROM g WHERE doc ? 'txn' LIMIT 10";
    assert_eq!(
        outcome_rows(sql_key, tx(sql_key).expect("select in txn")).len(),
        1
    );
    tx("ROLLBACK").expect("rollback");
    assert!(rows_of(&f.core, &c, sql).is_empty());
}

#[test]
fn a_select_in_a_transaction_does_not_require_or_create_the_index_table() {
    let path = unique_db_path("gin-scan-empty-index");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&jsonb_table("e")).expect("create");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = ddl_session();
    let c = ctx("alice");
    // 行が 1 件も無い状態で索引だけを宣言する（物理テーブルは未作成のまま）。
    run(&core, &c, &mut s, "CREATE INDEX e_ops ON e USING gin (doc)");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut tx = |sql: &str| core.execute_sql_in_txn(&c, &mut session, &mut txn, sql);
    tx("BEGIN").expect("begin");
    let sql = r#"SELECT id FROM e WHERE doc @> '{"a":1}' LIMIT 10"#;
    assert!(outcome_rows(sql, tx(sql).expect("select")).is_empty());
    tx("COMMIT").expect("commit");
    assert!(rows_of(&core, &c, sql).is_empty());
}

#[test]
fn temp_table_with_gin_matches_the_full_scan() {
    let path = unique_db_path("gin-scan-temp");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&jsonb_table("seed")).expect("seed");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = ddl_session();
    let c = ctx("alice");
    run(
        &core,
        &c,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, doc JSONB)",
    );
    run(
        &core,
        &c,
        &mut s,
        "CREATE TEMP TABLE tmp_plain (id BIGINT PRIMARY KEY, doc JSONB)",
    );
    run(&core, &c, &mut s, "CREATE INDEX tg ON tmp USING gin (doc)");
    for (i, d) in DOCS.iter().enumerate() {
        for t in ["tmp", "tmp_plain"] {
            run(
                &core,
                &c,
                &mut s,
                &format!("INSERT INTO {t} (id, doc) VALUES ({}, '{d}')", i + 1),
            );
        }
    }
    for w in [
        r#"doc @> '{"a":1}'"#,
        "doc ? 'b'",
        "doc ?& '{a,b}'",
        "doc ?| '{b,c}'",
    ] {
        let a = outcome_rows(
            w,
            run(
                &core,
                &c,
                &mut s,
                &format!("SELECT id, doc FROM tmp WHERE {w} LIMIT 100"),
            ),
        );
        let b = outcome_rows(
            w,
            run(
                &core,
                &c,
                &mut s,
                &format!("SELECT id, doc FROM tmp_plain WHERE {w} LIMIT 100"),
            ),
        );
        assert_eq!(a, b, "{w}");
        assert!(!a.is_empty(), "{w}");
    }
}
