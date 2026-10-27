//! `NATURAL [INNER|LEFT|RIGHT|FULL [OUTER]] JOIN` の結合テスト
//! （Issue #1827。ポインタ: SQL-60 (1)・SQL-28・RLS-10）。
//!
//! `tests/sql60_join_using.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）。
//! 表は主に `CREATE TABLE`（採番方式。疑似列 `id` を持たない）で作り、疑似列 `id` が共通列に
//! なる従来方式の表は `Storage` API で作る。検証対象は (a) 共通列の導出（左列順・完全一致・
//! 0 個なら直積）、(b) USING と同一の統合規則（値・列順・NULL 補完）、(c) 未対応形・型不一致の
//! fail-closed な拒否、(d) テナント境界。期待値は PostgreSQL 17 の結果に合わせた手計算値。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// DDL を許可したセッションで文を実行する（`CREATE TABLE`・`INSERT` の準備用）。
fn exec(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

/// a: (1,a1)(2,a2)(3,a3)(NULL,a-null)、b: (2,b2)(3,b3)(4,b4)(2,b2x)、c: (2,c2)(4,c4)。
/// d(k,j,p)・e(j,k,q): 複数共通列（右表は宣言順が逆）。em: 空表。
fn seeded() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("natural-join");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "CREATE TABLE d (k BIGINT, j TEXT, p TEXT)",
        "CREATE TABLE e (j TEXT, k BIGINT, q TEXT)",
        "CREATE TABLE em (e BIGINT)",
        "CREATE TABLE f (k TEXT, fv TEXT)",
        "CREATE TABLE g (k INTEGER, gv TEXT)",
        "CREATE TABLE h (embedding VECTOR(2))",
        "CREATE TABLE i (embedding VECTOR(2))",
        "CREATE TABLE cs1 (k BIGINT, v TEXT)",
        "CREATE TABLE cs2 (K BIGINT, w TEXT)",
        "CREATE TABLE p (x BIGINT, y TEXT)",
        "CREATE TABLE q (z BIGINT, w TEXT)",
        "CREATE TABLE s (k TEXT, sv TEXT)",
        "CREATE TABLE t (k TEXT, tv TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3'), (NULL, 'a-null')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3'), (4, 'b4'), (2, 'b2x')",
        "INSERT INTO c (k, x) VALUES (2, 'c2'), (4, 'c4')",
        "INSERT INTO d (k, j, p) VALUES (1, 'x', 'd1x'), (1, 'y', 'd1y'), (2, 'x', 'd2x')",
        "INSERT INTO e (j, k, q) VALUES ('x', 1, 'e1x'), ('y', 2, 'e2y'), ('y', 1, 'e1y')",
        "INSERT INTO cs1 (k, v) VALUES (1, 'c1')",
        "INSERT INTO cs2 (K, w) VALUES (1, 'c2')",
        "INSERT INTO p (x, y) VALUES (1, 'p1'), (2, 'p2')",
        "INSERT INTO q (z, w) VALUES (10, 'q1')",
        "INSERT INTO s (k, sv) VALUES ('p', 's-p'), ('q', 's-q')",
        "INSERT INTO t (k, tv) VALUES ('q', 't-q'), ('r', 't-r')",
    ] {
        exec(&core, sql);
    }
    (core, path)
}

fn try_run(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<QueryResult, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    try_run(core, "tenant-a", sql)
        .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}"))
}

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    match try_run(core, "tenant-a", sql) {
        Ok(_) => panic!("must be rejected: sql={sql:?}"),
        Err(e) => assert_eq!(e.wire_code(), wire_code, "sql={sql:?} err={e:?}"),
    }
}

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_of(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(render).collect())
        .collect()
}

fn sorted_rows(result: &QueryResult) -> Vec<Vec<String>> {
    let mut rows = rows_of(result);
    rows.sort();
    rows
}

fn col_names(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::AliasedId { name }
            | ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. }
            | ColumnMeta::Record { name, .. } => name.clone(),
        })
        .collect()
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| (*s).to_string()).collect())
        .collect()
}

// ---------- INNER／LEFT／RIGHT／FULL ----------

#[test]
fn natural_inner_join_merges_common_column_and_orders_star() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    for sql in [
        "SELECT * FROM a NATURAL JOIN b LIMIT 20",
        "SELECT * FROM a NATURAL INNER JOIN b LIMIT 20",
    ] {
        let result = run(&core, sql);
        // 共通列（統合列）→ 左の残り → 右の残り。NULL キーは一致しない。
        assert_eq!(col_names(&result), ["k", "v", "w"], "sql={sql:?}");
        assert_eq!(
            sorted_rows(&result),
            strs(&[&["2", "a2", "b2"], &["2", "a2", "b2x"], &["3", "a3", "b3"]]),
            "sql={sql:?}"
        );
    }
}

#[test]
fn natural_left_right_full_null_pad_and_take_merged_value() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    for sql in [
        "SELECT * FROM a NATURAL LEFT JOIN b LIMIT 20",
        "SELECT * FROM a NATURAL LEFT OUTER JOIN b LIMIT 20",
    ] {
        assert_eq!(
            sorted_rows(&run(&core, sql)),
            strs(&[
                &["1", "a1", "NULL"],
                &["2", "a2", "b2"],
                &["2", "a2", "b2x"],
                &["3", "a3", "b3"],
                &["NULL", "a-null", "NULL"],
            ]),
            "sql={sql:?}"
        );
    }
    // RIGHT の統合列は右の値、FULL は COALESCE(左, 右)。
    assert_eq!(
        sorted_rows(&run(
            &core,
            "SELECT k, v, w, a.k, b.k FROM a NATURAL RIGHT JOIN b LIMIT 20"
        )),
        strs(&[
            &["2", "a2", "b2", "2", "2"],
            &["2", "a2", "b2x", "2", "2"],
            &["3", "a3", "b3", "3", "3"],
            &["4", "NULL", "b4", "NULL", "4"],
        ])
    );
    let full = run(&core, "SELECT * FROM a NATURAL FULL OUTER JOIN b LIMIT 20");
    assert_eq!(col_names(&full), ["k", "v", "w"]);
    assert_eq!(
        sorted_rows(&full),
        strs(&[
            &["1", "a1", "NULL"],
            &["2", "a2", "b2"],
            &["2", "a2", "b2x"],
            &["3", "a3", "b3"],
            &["4", "NULL", "b4"],
            &["NULL", "a-null", "NULL"],
        ])
    );
}

#[test]
fn natural_equals_explicit_using_for_every_join_kind() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    for (l, r, kind, cols) in [
        ("a", "b", "", "k"),
        ("a", "b", "LEFT", "k"),
        ("a", "b", "RIGHT", "k"),
        ("a", "b", "FULL", "k"),
        ("d", "e", "", "k, j"),
        ("d", "e", "FULL", "k, j"),
    ] {
        let natural = run(
            &core,
            &format!("SELECT * FROM {l} NATURAL {kind} JOIN {r} LIMIT 50"),
        );
        let using = run(
            &core,
            &format!("SELECT * FROM {l} {kind} JOIN {r} USING ({cols}) LIMIT 50"),
        );
        assert_eq!(col_names(&natural), col_names(&using), "{l} {kind} {r}");
        assert_eq!(
            format!("{:?}", natural.columns),
            format!("{:?}", using.columns),
            "column metadata must match: {l} {kind} {r}"
        );
        assert_eq!(sorted_rows(&natural), sorted_rows(&using), "{l} {kind} {r}");
    }
}

// ---------- 共通列の導出規則 ----------

#[test]
fn multiple_common_columns_follow_left_declaration_order() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    // 左 d は (k, j, p)、右 e は (j, k, q)。共通列は左の宣言順 k, j。
    let result = run(&core, "SELECT * FROM d NATURAL JOIN e LIMIT 20");
    assert_eq!(col_names(&result), ["k", "j", "p", "q"]);
    assert_eq!(
        sorted_rows(&result),
        strs(&[&["1", "x", "d1x", "e1x"], &["1", "y", "d1y", "e1y"]])
    );
    // 左右を入れ替えると列順は右表（今度は左）の宣言順 j, k になる。
    let swapped = run(&core, "SELECT * FROM e NATURAL JOIN d LIMIT 20");
    assert_eq!(col_names(&swapped), ["j", "k", "q", "p"]);
}

#[test]
fn common_column_names_are_matched_case_sensitively() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    // `k` と `K` は別の列名: 共通列は無く、直積で両方の列が出力される。
    let result = run(&core, "SELECT * FROM cs1 NATURAL JOIN cs2 LIMIT 20");
    assert_eq!(col_names(&result), ["k", "v", "K", "w"]);
    assert_eq!(rows_of(&result), strs(&[&["1", "c1", "1", "c2"]]));
}

#[test]
fn unqualified_name_is_merged_column_and_qualified_names_stay_original() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    let result = run(
        &core,
        "SELECT k, v, w, a.k, b.k FROM a NATURAL LEFT JOIN b LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["1", "a1", "NULL", "1", "NULL"],
            &["2", "a2", "b2", "2", "2"],
            &["2", "a2", "b2x", "2", "2"],
            &["3", "a3", "b3", "3", "3"],
            &["NULL", "a-null", "NULL", "NULL", "NULL"],
        ])
    );
}

// ---------- 共通列 0 個（直積） ----------

#[test]
fn no_common_column_is_cross_product_with_outer_null_padding() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    let inner = run(&core, "SELECT * FROM p NATURAL JOIN q LIMIT 20");
    assert_eq!(col_names(&inner), ["x", "y", "z", "w"]);
    assert_eq!(
        sorted_rows(&inner),
        strs(&[&["1", "p1", "10", "q1"], &["2", "p2", "10", "q1"]])
    );
    // 空表との直積: INNER は 0 行、外部結合は保持側を NULL 補完して残す。
    assert!(run(&core, "SELECT * FROM p NATURAL JOIN em LIMIT 20")
        .rows
        .is_empty());
    assert_eq!(
        sorted_rows(&run(&core, "SELECT * FROM p NATURAL LEFT JOIN em LIMIT 20")),
        strs(&[&["1", "p1", "NULL"], &["2", "p2", "NULL"]])
    );
    assert_eq!(
        sorted_rows(&run(
            &core,
            "SELECT * FROM em NATURAL RIGHT JOIN p LIMIT 20"
        )),
        strs(&[&["NULL", "1", "p1"], &["NULL", "2", "p2"]])
    );
    assert_eq!(
        sorted_rows(&run(&core, "SELECT * FROM p NATURAL FULL JOIN em LIMIT 20")),
        strs(&[&["1", "p1", "NULL"], &["2", "p2", "NULL"]])
    );
}

#[test]
fn no_common_column_cross_product_respects_output_row_limit() {
    let path = unique_db_path("natural-join-cap");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    exec(&core, "CREATE TABLE big1 (m BIGINT)");
    exec(&core, "CREATE TABLE big2 (n BIGINT)");
    for table in ["big1", "big2"] {
        let col = if table == "big1" { "m" } else { "n" };
        // 1 文の複数行 INSERT は 64 行までなので 64 行ずつ分けて投入する。
        for chunk in 0..5 {
            let values: Vec<String> = (0..64).map(|i| format!("({})", chunk * 64 + i)).collect();
            exec(
                &core,
                &format!("INSERT INTO {table} ({col}) VALUES {}", values.join(", ")),
            );
        }
    }
    // 320 x 320 = 10 万 2400 ペアは一致ペア数の上限（10 万）を超える（逐次判定で `54000`）。
    assert_rejected(
        &core,
        "SELECT * FROM big1 NATURAL JOIN big2 LIMIT 5",
        "54000",
    );
}

// ---------- 連鎖・WHERE・混在 ----------

#[test]
fn natural_chain_and_mix_with_on_and_using() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    let chain = run(
        &core,
        "SELECT * FROM a NATURAL JOIN b NATURAL JOIN c LIMIT 20",
    );
    assert_eq!(col_names(&chain), ["k", "v", "w", "x"]);
    assert_eq!(
        sorted_rows(&chain),
        strs(&[&["2", "a2", "b2", "c2"], &["2", "a2", "b2x", "c2"]])
    );
    let mixed_on = run(
        &core,
        "SELECT a.v, b.w, c.x FROM a NATURAL JOIN b JOIN c ON b.k = c.k LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&mixed_on),
        strs(&[&["a2", "b2", "c2"], &["a2", "b2x", "c2"]])
    );
    let mixed_using = run(
        &core,
        "SELECT v, w, x FROM a JOIN b USING (k) NATURAL JOIN c LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&mixed_using),
        sorted_rows(&run(
            &core,
            "SELECT v, w, x FROM a NATURAL JOIN b NATURAL JOIN c LIMIT 20",
        ))
    );
}

#[test]
fn natural_join_with_where_predicate() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    // 右側への述語は LEFT を内部結合へ簡約する（未一致の左行は残らない）。
    let reduced = run(
        &core,
        "SELECT k, v FROM a NATURAL LEFT JOIN b WHERE w = 'b3' LIMIT 20",
    );
    assert_eq!(rows_of(&reduced), strs(&[&["3", "a3"]]));
    let pushed = run(
        &core,
        "SELECT v, w FROM a NATURAL JOIN b WHERE v = 'a2' LIMIT 20",
    );
    assert_eq!(sorted_rows(&pushed), strs(&[&["a2", "b2"], &["a2", "b2x"]]));
}

// ---------- fail-closed な拒否 ----------

#[test]
fn natural_join_type_mismatches_are_rejected_like_on_and_using() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    // 型クラス不一致（BIGINT と TEXT）は ON と同じ `42804`。
    assert_rejected(&core, "SELECT * FROM a NATURAL JOIN f LIMIT 5", "42804");
    // 同クラスで型が完全一致しない組（BIGINT と INTEGER）は USING と同じ `0A000`。
    assert_rejected(&core, "SELECT * FROM a NATURAL JOIN g LIMIT 5", "0A000");
    // 結合キーにできない型（VECTOR）は USING と同じエラーで拒否される。
    let using_err = try_run(
        &core,
        "tenant-a",
        "SELECT * FROM h JOIN i USING (embedding) LIMIT 5",
    )
    .expect_err("VECTOR key must be rejected");
    assert_rejected(
        &core,
        "SELECT * FROM h NATURAL JOIN i LIMIT 5",
        using_err.wire_code(),
    );
}

#[test]
fn natural_join_left_ambiguity_is_rejected() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    // a・b がともに未統合の `k` を持つ状態で c と NATURAL: 左の `k` が曖昧（`42702`）。
    assert_rejected(
        &core,
        "SELECT * FROM a JOIN b ON a.k = b.k NATURAL JOIN c LIMIT 5",
        "42702",
    );
}

#[test]
fn natural_full_merged_column_is_unsupported_in_where_and_next_natural_step() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    for sql in [
        "SELECT sv FROM s NATURAL FULL JOIN t WHERE k = 'p' LIMIT 5",
        "SELECT * FROM a NATURAL FULL JOIN b NATURAL JOIN c LIMIT 5",
    ] {
        assert_rejected(&core, sql, "0A000");
    }
    // 投影と ORDER BY は FULL の統合列でも値を組み立てられる。
    let ordered = run(
        &core,
        "SELECT k, w FROM a NATURAL FULL JOIN b ORDER BY k LIMIT 20",
    );
    assert_eq!(
        rows_of(&ordered).first().map(|r| r[0].clone()),
        Some("1".to_string())
    );
}

#[test]
fn natural_join_syntax_misuse_and_common_column_cap() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);

    for (sql, code) in [
        (
            "SELECT * FROM a NATURAL JOIN b ON a.k = b.k LIMIT 5",
            "42601",
        ),
        ("SELECT * FROM a NATURAL JOIN b USING (k) LIMIT 5", "42601"),
        ("SELECT * FROM a NATURAL CROSS JOIN b LIMIT 5", "42601"),
        ("SELECT * FROM a NATURAL OUTER JOIN b LIMIT 5", "42601"),
        ("SELECT * FROM a NATURAL b LIMIT 5", "42601"),
    ] {
        assert_rejected(&core, sql, code);
    }

    // 共通列 9 個は ON／USING と同じ上限（8）を超えて `54000`、8 個は受理される。
    let cols = |n: usize| -> String {
        (1..=n)
            .map(|i| format!("c{i} BIGINT"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    exec(&core, &format!("CREATE TABLE n9a ({})", cols(9)));
    exec(&core, &format!("CREATE TABLE n9b ({})", cols(9)));
    exec(&core, &format!("CREATE TABLE n8a ({})", cols(8)));
    exec(&core, &format!("CREATE TABLE n8b ({})", cols(8)));
    assert_rejected(&core, "SELECT * FROM n9a NATURAL JOIN n9b LIMIT 5", "54000");
    assert!(run(&core, "SELECT * FROM n8a NATURAL JOIN n8b LIMIT 5")
        .rows
        .is_empty());
}

// ---------- 疑似列 id（従来方式の表）とテナント境界 ----------

fn insert_n(storage: &Storage, table: &str, tenant: &str, id: u64, vis: Visibility, n: &str) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "seed-{table}-{tenant}-{id}"
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        vis,
        &[Value::Text(n.to_string())],
        &op_id,
    )
    .expect("insert row");
}

/// 従来方式の表 la(n)・lb(n)（疑似列 `id` を持つので `id` と `n` が共通列）。
fn legacy_seeded() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("natural-join-legacy");
    let storage = Storage::open(&path).expect("open storage");
    for name in ["la", "lb"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("n", ColumnType::Text, false)],
            ))
            .expect("create table");
    }
    for (id, n) in [(1, "x"), (2, "y")] {
        insert_n(&storage, "la", "tenant-a", id, Visibility::Public, n);
    }
    for (id, n) in [(1, "x"), (2, "z")] {
        insert_n(&storage, "lb", "tenant-a", id, Visibility::Public, n);
    }
    // 他テナントの Private 行（tenant-a からは見えず、結果・NULL 補完・統合値に現れない）。
    insert_n(&storage, "la", "tenant-b", 3, Visibility::Private, "q");
    insert_n(&storage, "lb", "tenant-b", 3, Visibility::Private, "q");
    insert_n(&storage, "la", "tenant-b", 4, Visibility::Private, "only-a");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

#[test]
fn pseudo_id_is_a_common_column_and_join_key() {
    let (core, path) = legacy_seeded();
    let _guard = CleanupGuard(path);

    // `id` と `n` の両方が一致する行だけが結合する（id=2 は n が異なる）。
    let inner = run(&core, "SELECT n FROM la NATURAL JOIN lb LIMIT 20");
    assert_eq!(rows_of(&inner), strs(&[&["x"]]));
    let left = run(&core, "SELECT n FROM la NATURAL LEFT JOIN lb LIMIT 20");
    assert_eq!(sorted_rows(&left), strs(&[&["x"], &["y"]]));
    let star = run(&core, "SELECT * FROM la NATURAL JOIN lb LIMIT 20");
    assert_eq!(col_names(&star), ["id", "n"]);
}

#[test]
fn other_tenant_private_rows_never_surface_through_natural_join() {
    let (core, path) = legacy_seeded();
    let _guard = CleanupGuard(path);

    for sql in [
        "SELECT n FROM la NATURAL JOIN lb LIMIT 20",
        "SELECT n FROM la NATURAL LEFT JOIN lb LIMIT 20",
        "SELECT n FROM la NATURAL RIGHT JOIN lb LIMIT 20",
        "SELECT n FROM la NATURAL FULL JOIN lb LIMIT 20",
    ] {
        let rows = rows_of(&run(&core, sql));
        assert!(
            rows.iter().all(|r| r[0] != "q" && r[0] != "only-a"),
            "other tenant rows must not appear: sql={sql:?} rows={rows:?}"
        );
    }
    // 正の対照: 所有テナントからはその行が結合される。
    let owner = try_run(
        &core,
        "tenant-b",
        "SELECT n FROM la NATURAL JOIN lb LIMIT 20",
    )
    .expect("owner query");
    // 所有テナントには自分の Private 行に加え、tenant-a の Public 行も見える。
    assert_eq!(sorted_rows(&owner), strs(&[&["q"], &["x"]]));
    // 拒否応答は他テナント行の有無で変わらない（スキーマと文だけで決まる）。
    assert_rejected(
        &core,
        "SELECT * FROM la NATURAL JOIN lb ON la.id = lb.id LIMIT 5",
        "42601",
    );
}
