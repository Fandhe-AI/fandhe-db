//! 非集計形の相関スカラーサブクエリ（外側の行ごとの再評価）の結合テスト（SQL-61。
//! `docs/spec/04-behavior/` のポインタのみ。Issue #1847、親 #1845。テナント境界は RLS-10 ポインタ）。
//!
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、期待値は
//! PostgreSQL 17 の意味論から手計算した固定値（各ケースのコメントに根拠）。投影位置
//! （`SELECT ..., (SELECT y ...)`）と WHERE 位置（`col <op> (SELECT y ...)`）の両方で、
//! 該当 0 件は NULL（WHERE では偽）・1 件はその値・2 件以上は `21000` になること、
//! 再評価回数の上限超過が `54000` になること、受理形の外は fail-closed
//! （`42601`／`0A000`）で拒否されること、他テナントの行が値・基数に影響しないことを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::correlated_limits::CorrelatedLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn vec_col() -> ColumnDef {
    ColumnDef::new("embedding", ColumnType::Vector(2), false)
}

fn col(name: &str, ty: ColumnType) -> ColumnDef {
    ColumnDef::new(name, ty, true)
}

fn a_schema() -> TableSchema {
    TableSchema::new(
        "a",
        vec![
            vec_col(),
            col("k", ColumnType::BigInt),
            col("t", ColumnType::Text),
            col("v", ColumnType::Integer),
        ],
    )
}

fn b_schema() -> TableSchema {
    TableSchema::new(
        "b",
        vec![
            vec_col(),
            col("aid", ColumnType::BigInt),
            col("t", ColumnType::Text),
            col("v", ColumnType::Integer),
            col("flag", ColumnType::Boolean),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"));
}

/// `INSERT INTO <table> (id, embedding, <cols>) VALUES (...)`。省略した列は NULL。
fn ins(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, cols: &[(&str, &str)]) {
    let mut names = vec!["id".to_string(), "embedding".to_string()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".to_string()];
    for (c, v) in cols {
        names.push((*c).to_string());
        values.push((*v).to_string());
    }
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {table} ({}) VALUES ({}) USING OPERATION_ID '{table}-{id}'",
            names.join(", "),
            values.join(", ")
        ),
    );
}

fn query(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"))
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let mut v: Vec<u64> = query(core, ctx, sql).rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

/// 数値セルを比較しやすい `i128` へ（`NULL` は `None`）。
fn num(cell: &Cell) -> Option<i128> {
    match cell {
        Cell::Null => None,
        Cell::Integer(n) => Some(i128::from(*n)),
        Cell::SignedInteger(n) => Some(i128::from(*n)),
        other => panic!("unexpected numeric cell {other:?}"),
    }
}

/// `SELECT id, (<サブクエリ>) ... FROM a` の「最後の列」の数値を `id` 昇順で返す。
fn last_col_by_id(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<(u64, Option<i128>)> {
    let mut rows: Vec<(u64, Option<i128>)> = query(core, ctx, sql)
        .rows
        .iter()
        .map(|r| (r.id, num(r.cells.last().expect("cells"))))
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// `(wire_code, message)`。
fn err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql) {
        Ok(_) => panic!("sql={sql:?} should be rejected"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn col_name(meta: &ColumnMeta) -> String {
    match meta {
        ColumnMeta::Id => "id".to_string(),
        ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::AliasedId { name } => name.clone(),
    }
}

fn assert_no_reserved_names(sql: &str, msg: &str) {
    assert!(
        !msg.contains('\u{2}') && !msg.contains('\u{3}'),
        "reserved name leaked: sql={sql:?} msg={msg:?}"
    );
}

fn open_core(name: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    (storage, CleanupGuard(path))
}

fn seed(core: &EngineCore, ctx: &PolicyContext) {
    // a: id1 {k1, t'x', v10}、id2 {k2, t'y', v20}、id3 {k3, t NULL, v30}、id4 {k NULL, t'x', v5}
    ins(core, ctx, "a", 1, &[("k", "1"), ("t", "'x'"), ("v", "10")]);
    ins(core, ctx, "a", 2, &[("k", "2"), ("t", "'y'"), ("v", "20")]);
    ins(core, ctx, "a", 3, &[("k", "3"), ("v", "30")]);
    ins(core, ctx, "a", 4, &[("t", "'x'"), ("v", "5")]);
    // b: id1 {aid1, t'x', v5, flag true}、id2 {aid1, t'z', v50, flag false}、
    //    id3 {aid2, t'y', v20, flag true}、id4 {aid NULL, v1, flag true}
    ins(
        core,
        ctx,
        "b",
        1,
        &[("aid", "1"), ("t", "'x'"), ("v", "5"), ("flag", "true")],
    );
    ins(
        core,
        ctx,
        "b",
        2,
        &[("aid", "1"), ("t", "'z'"), ("v", "50"), ("flag", "false")],
    );
    ins(
        core,
        ctx,
        "b",
        3,
        &[("aid", "2"), ("t", "'y'"), ("v", "20"), ("flag", "true")],
    );
    ins(core, ctx, "b", 4, &[("v", "1"), ("flag", "true")]);
}

fn create_tables(storage: &Storage) {
    for schema in [a_schema(), b_schema()] {
        storage.create_table(&schema).expect("create table");
    }
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let (storage, guard) = open_core("sql61-correlated-scalar-nonagg");
    create_tables(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    (core, ctx, guard)
}

/// 等価キーのみ・一致は `flag = true` の行（a1→b1 の 5、a2→b3 の 20、a3 は無し、a4 はキー NULL）。
const KEY_ONLY: &str = "(SELECT b.v FROM b WHERE b.aid = a.k AND b.flag = true)";

// --- 投影位置 --------------------------------------------------------------------------------

#[test]
fn projection_returns_the_single_match_and_null_for_zero_matches() {
    let (core, ctx, _g) = setup();
    // 一致 0 件（a3: キー 3 の内側行なし、a4: 外側キー NULL）は NULL、1 件はその値。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, {KEY_ONLY} FROM a LIMIT 100")
        ),
        vec![(1, Some(5)), (2, Some(20)), (3, None), (4, None)]
    );
}

#[test]
fn projection_two_or_more_matches_raise_21000_only_for_returned_outer_rows() {
    let (core, ctx, _g) = setup();
    let many = "(SELECT b.v FROM b WHERE b.aid = a.k)";
    // a1 はキー 1 の内側行が 2 行（b1・b2）。出力される外側の行に含まれれば 21000。
    let (code, msg) = err(&core, &ctx, &format!("SELECT id, {many} FROM a LIMIT 100"));
    assert_eq!(code, "21000", "{msg}");
    assert_no_reserved_names("many", &msg);
    // 外側が 0 行ならエラーにならない（PostgreSQL の遅延評価と同じ）。
    assert!(query(
        &core,
        &ctx,
        &format!("SELECT id, {many} FROM a WHERE v > 1000 LIMIT 100")
    )
    .rows
    .is_empty());
    // 外側の WHERE が恒偽（`id < 0`）の場合も同様。
    assert!(query(
        &core,
        &ctx,
        &format!("SELECT id, {many} FROM a WHERE id < 0 LIMIT 100")
    )
    .rows
    .is_empty());
    // 外側の ORDER BY／LIMIT で出力されない行（a1）だけが 2 件以上一致するならエラーにならない。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, {many} FROM a ORDER BY id DESC LIMIT 1")
        ),
        vec![(4, None)]
    );
    // 2 件以上一致する行を除く外側（a2 は 1 件、a3 は 0 件）はエラーにならない。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, {many} FROM a WHERE id >= 2 LIMIT 100")
        ),
        vec![(2, Some(20)), (3, None), (4, None)]
    );
}

#[test]
fn projection_residual_keyless_and_key_only_forms_agree_with_hand_computed_values() {
    let (core, ctx, _g) = setup();
    // 残余あり（非等価）: a1(v10) は b2(50)、a2(v20) は b3(20)、a3 は無し、a4 はキー NULL。
    for sub in [
        "(SELECT b.v FROM b WHERE b.aid = a.k AND b.v >= a.v)",
        // 外側と内側が混ざった OR を含む残余。
        "(SELECT b.v FROM b WHERE b.aid = a.k AND (b.v >= a.v OR a.t = 'zzz'))",
    ] {
        assert_eq!(
            last_col_by_id(&core, &ctx, &format!("SELECT id, {sub} FROM a LIMIT 100")),
            vec![(1, Some(50)), (2, Some(20)), (3, None), (4, None)],
            "{sub}"
        );
    }
    // キーなし（外側の列との等価は内側列 = 外側列の等価キー〔b.v = a.v〕）: a2 は b3(20)、
    // a4(v5) は b1(5)、a1(10)・a3(30) は無し。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.v FROM b WHERE b.v = a.v) FROM a LIMIT 100"
        ),
        vec![(1, None), (2, Some(20)), (3, None), (4, Some(5))]
    );
    // 真のキーなし（非等価だけ）: `b.v < a.v` は a1(10) が b1(5)・b4(1) の 2 件で 21000。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id, (SELECT b.v FROM b WHERE b.v < a.v) FROM a LIMIT 100",
    );
    assert_eq!(code, "21000");
    // 同じ形で外側を a4(v5) に絞れば b4(1) の 1 件だけ。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.v FROM b WHERE b.v < a.v) FROM a WHERE id = 4 LIMIT 100"
        ),
        vec![(4, Some(1))]
    );
}

#[test]
fn projection_inner_limit_offset_and_order_by_rules() {
    let (core, ctx, _g) = setup();
    let one_of = |v: Option<i128>, allowed: &[i128]| match v {
        Some(n) => allowed.contains(&n),
        None => false,
    };
    // `LIMIT 1` は 2 件一致でも 21000 にならず 1 件を採る（ORDER BY なしの選択行は不定）。
    let rows = last_col_by_id(
        &core,
        &ctx,
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k LIMIT 1) FROM a LIMIT 100",
    );
    assert!(one_of(rows[0].1, &[5, 50]), "{rows:?}");
    assert_eq!(rows[1], (2, Some(20)));
    assert_eq!(rows[2], (3, None));
    assert_eq!(rows[3], (4, None));
    // `LIMIT 1 OFFSET 1`: a1 は 2 件目、a2 は 1 件だけなので NULL。
    let rows = last_col_by_id(
        &core,
        &ctx,
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k LIMIT 1 OFFSET 1) FROM a LIMIT 100",
    );
    assert!(one_of(rows[0].1, &[5, 50]), "{rows:?}");
    assert_eq!(rows[1], (2, None));
    // `OFFSET 2` は a1 でも残りが無く NULL。`OFFSET 1`（LIMIT なし）は a1 が残り 1 件で値を返す。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k OFFSET 2) FROM a WHERE id = 1 LIMIT 100"
        ),
        vec![(1, None)]
    );
    let rows = last_col_by_id(
        &core,
        &ctx,
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k OFFSET 1) FROM a WHERE id = 1 LIMIT 100",
    );
    assert!(one_of(rows[0].1, &[5, 50]), "{rows:?}");
    // `LIMIT ALL` は上限なし（2 件一致で 21000）。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k LIMIT ALL) FROM a LIMIT 100",
    );
    assert_eq!(code, "21000");
    // `ORDER BY` だけは一致行の個数に影響しないため受理する。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k AND b.flag = true ORDER BY b.v) \
             FROM a LIMIT 100"
        ),
        vec![(1, Some(5)), (2, Some(20)), (3, None), (4, None)]
    );
    // `ORDER BY` と `LIMIT`／`OFFSET` の併用（外側の行ごとの top-n）は fail-closed。
    for sql in [
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k ORDER BY b.v LIMIT 1) FROM a LIMIT 100",
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k ORDER BY b.v OFFSET 1) FROM a LIMIT 100",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "42601", "sql={sql:?} msg={msg}");
        assert!(msg.contains("correlated"), "sql={sql:?} msg={msg}");
    }
}

#[test]
fn projection_pseudo_id_keys_values_and_column_metadata() {
    let (core, ctx, _g) = setup();
    // 内側の疑似列 `id` をキーに（b.id = a.k）: a1→b1(5)、a2→b2(50)、a3→b3(20)、a4 はキー NULL。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.v FROM b WHERE b.id = a.k) FROM a LIMIT 100"
        ),
        vec![(1, Some(5)), (2, Some(50)), (3, Some(20)), (4, None)]
    );
    // 外側の疑似列 `id` をキーに（b.aid = a.id）: a1→b1・b2 の 2 件で 21000、a2 は b3 の 1 件。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.id) FROM a WHERE id >= 2 LIMIT 100"
        ),
        vec![(2, Some(20)), (3, None), (4, None)]
    );
    // 値が内側の疑似列 `id`（a1→1、a2→3）。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT b.id FROM b WHERE b.aid = a.k AND b.flag = true) FROM a LIMIT 100"
        ),
        vec![(1, Some(1)), (2, Some(3)), (3, None), (4, None)]
    );
    // 列名・位置・別名。
    let r = query(
        &core,
        &ctx,
        "SELECT (SELECT b.v FROM b WHERE b.aid = a.k AND b.flag = true) AS bv, v FROM a \
         WHERE id = 2 LIMIT 100",
    );
    assert_eq!(
        r.columns.iter().map(col_name).collect::<Vec<_>>(),
        vec!["bv".to_string(), "v".to_string()]
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(num(r.rows[0].cells.first().expect("cell")), Some(20));
    assert_eq!(num(r.rows[0].cells.get(1).expect("cell")), Some(20));
    // 隠し列は利用者へ返らない（列数は投影した 2 列だけ）。
    assert_eq!(r.rows[0].cells.len(), 2);
}

#[test]
fn projection_text_values_and_null_outer_key() {
    let (core, ctx, _g) = setup();
    let r = query(
        &core,
        &ctx,
        "SELECT id, (SELECT b.t FROM b WHERE b.aid = a.k AND b.flag = true) FROM a LIMIT 100",
    );
    let mut got: Vec<(u64, Option<String>)> = r
        .rows
        .iter()
        .map(|row| {
            (
                row.id,
                match row.cells.last().expect("cell") {
                    Cell::Null => None,
                    Cell::Text(s) => Some(s.clone()),
                    other => panic!("unexpected cell {other:?}"),
                },
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (1, Some("x".to_string())),
            (2, Some("y".to_string())),
            (3, None),
            (4, None)
        ]
    );
}

// --- WHERE 位置 --------------------------------------------------------------------------------

#[test]
fn where_compares_against_the_single_match_for_every_operator() {
    let (core, ctx, _g) = setup();
    // 比較値は a1→5、a2→20、a3・a4→無し（NULL。比較は偽）。外側の v は 10・20・30・5。
    for (op, expected) in [
        ("=", vec![2_u64]),
        ("<>", vec![1]),
        ("<", vec![]),
        ("<=", vec![2]),
        (">", vec![1]),
        (">=", vec![1, 2]),
    ] {
        assert_eq!(
            ids(
                &core,
                &ctx,
                &format!("SELECT id FROM a WHERE v {op} {KEY_ONLY} LIMIT 100")
            ),
            expected,
            "op {op}"
        );
    }
    // NOT は構文段で演算子が反転され、NULL（一致なし）の行は返らない。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE NOT (v = {KEY_ONLY}) LIMIT 100")
        ),
        vec![1]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v = {KEY_ONLY} OR id = 4 LIMIT 100")
        ),
        vec![2, 4]
    );
    // テキスト同士の比較。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE t = (SELECT b.t FROM b WHERE b.aid = a.k AND b.flag = true) \
             LIMIT 100"
        ),
        vec![1, 2]
    );
}

#[test]
fn where_residual_keyless_and_inner_limit_forms() {
    let (core, ctx, _g) = setup();
    // 残余あり: a1 は b2(50)、a2 は b3(20)。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v = (SELECT b.v FROM b WHERE b.aid = a.k AND b.v >= a.v) \
             LIMIT 100"
        ),
        vec![2]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v <= (SELECT b.v FROM b WHERE b.aid = a.k AND b.v >= a.v) \
             LIMIT 100"
        ),
        vec![1, 2]
    );
    // 等価キー 1 本（b.v = a.v）: a2 と a4 で自分と等しい値が見つかる。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v = (SELECT b.v FROM b WHERE b.v = a.v) LIMIT 100"
        ),
        vec![2, 4]
    );
    // 内側 LIMIT 1 は 2 件一致でも 21000 にならない（a1 の候補は 5 か 50、v10 との `<>` はどちらでも真）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v <> (SELECT b.v FROM b WHERE b.aid = a.k LIMIT 1) LIMIT 100"
        ),
        vec![1]
    );
}

#[test]
fn where_two_or_more_matches_raise_21000_unless_the_outer_has_no_rows() {
    let (core, ctx, _g) = setup();
    let sql = "SELECT id FROM a WHERE v > (SELECT b.v FROM b WHERE b.aid = a.k) LIMIT 100";
    let (code, msg) = err(&core, &ctx, sql);
    assert_eq!(code, "21000", "{msg}");
    assert_no_reserved_names(sql, &msg);
    // 外側の行が無いテナント（他テナントの a/b 行は見えない）ではエラーにならない。
    assert!(query(&core, &ctx_for("t3"), sql).rows.is_empty());
}

#[test]
fn where_static_errors_win_regardless_of_data() {
    let (core, ctx, _g) = setup();
    // 値族の不一致（テキスト列 × 整数の値）は 22000、未知の対象列は 42703。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE t = (SELECT b.v FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(code, "22000");
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE nope = (SELECT b.v FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(code, "42703");
    // 2 つ以上の列を返す内側は静的に拒否する。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE v = (SELECT b.v, b.t FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(code, "42601");
}

#[test]
fn where_pseudo_id_value_and_deferred_nested_errors() {
    let (core, ctx, _g) = setup();
    // 値が内側の疑似列 `id`（a1→1、a2→3）。外側 `id` との等価は a1 だけ真。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE id = (SELECT b.id FROM b WHERE b.aid = a.k AND b.flag = true) \
             LIMIT 100"
        ),
        vec![1]
    );
    // 内側 WHERE の入れ子スカラーサブクエリが 2 行以上（非相関・4 行）で 21000 になる形。
    let nested = "b.v = (SELECT x.v FROM b x)";
    // 投影位置: 外側が 0 行なら内側は評価されずエラーにならず、列メタデータは確定する。
    let empty = query(
        &core,
        &ctx,
        &format!(
            "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k AND {nested}) FROM a \
             WHERE v > 1000 LIMIT 100"
        ),
    );
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns.len(), 2);
    // 外側が 1 行以上なら遅延していた 21000 を返す。
    let (code, msg) = err(
        &core,
        &ctx,
        &format!("SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k AND {nested}) FROM a LIMIT 100"),
    );
    assert_eq!(code, "21000", "{msg}");
    assert_no_reserved_names("deferred", &msg);
    // WHERE 位置は実行時に解決するため 21000 をそのまま返す。
    let (code, _) = err(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM a WHERE v = (SELECT b.v FROM b WHERE b.aid = a.k AND {nested}) \
             LIMIT 100"
        ),
    );
    assert_eq!(code, "21000");
    // 静的エラー（入れ子の値族の不一致 22000）は遅延対象の 21000 より優先する（両位置）。
    let mismatch = "t = (SELECT x.v FROM b x)";
    for sql in [
        format!("SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k AND {mismatch}) FROM a LIMIT 100"),
        format!(
            "SELECT id FROM a WHERE v = (SELECT b.v FROM b WHERE b.aid = a.k AND {mismatch}) \
             LIMIT 100"
        ),
    ] {
        let (code, msg) = err(&core, &ctx, &sql);
        assert_eq!(code, "22000", "sql={sql:?} msg={msg}");
    }
}

// --- テナント境界（RLS-10） ----------------------------------------------------------------------

#[test]
fn other_tenants_rows_never_affect_values_cardinality_or_errors() {
    let (core, t1, _g) = setup();
    let t2 = ctx_for("t2");
    // t2 の行: b にキー 1 を 2 行、a に k=1 と k=2 の行。t1 のキー 1 は flag=true の行が 1 件だけ。
    ins(
        &core,
        &t2,
        "b",
        100,
        &[("aid", "1"), ("v", "1000"), ("flag", "true")],
    );
    ins(
        &core,
        &t2,
        "b",
        101,
        &[("aid", "1"), ("v", "1001"), ("flag", "true")],
    );
    ins(&core, &t2, "a", 300, &[("k", "1"), ("v", "7")]);
    ins(&core, &t2, "a", 301, &[("k", "2"), ("v", "9")]);
    // t1 から見た結果は t2 の行を含めず、21000 も発生しない。
    assert_eq!(
        last_col_by_id(
            &core,
            &t1,
            &format!("SELECT id, {KEY_ONLY} FROM a LIMIT 100")
        ),
        vec![(1, Some(5)), (2, Some(20)), (3, None), (4, None)]
    );
    assert_eq!(
        ids(
            &core,
            &t1,
            &format!("SELECT id FROM a WHERE v = {KEY_ONLY} LIMIT 100")
        ),
        vec![2]
    );
    // t2 は自分の行だけで評価される（a300 はキー 1 に 2 件で 21000、a301 のキー 2 は内側無しで NULL）。
    let (code, _) = err(
        &core,
        &t2,
        &format!("SELECT id, {KEY_ONLY} FROM a LIMIT 100"),
    );
    assert_eq!(code, "21000");
    assert_eq!(
        last_col_by_id(
            &core,
            &t2,
            &format!("SELECT id, {KEY_ONLY} FROM a WHERE id = 301 LIMIT 100")
        ),
        vec![(301, None)]
    );
}

// --- 上限・fail-closed ---------------------------------------------------------------------------

fn setup_with_limits(
    name: &str,
    limits: CorrelatedLimits,
) -> (EngineCore, PolicyContext, CleanupGuard) {
    let (storage, guard) = open_core(name);
    create_tables(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_correlated_limits(limits);
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    (core, ctx, guard)
}

#[test]
fn reevaluation_limit_rejects_with_54000_in_both_positions_and_succeeds_at_the_boundary() {
    // キーなし（`b.v < a.v` だけ）は外側 1 行ごとに再評価 1 回を計上する（内側に行がある外側 4 行）。
    let proj = "SELECT id, (SELECT b.v FROM b WHERE b.v < a.v AND b.flag = false) FROM a LIMIT 100";
    let wher = "SELECT id FROM a WHERE v > (SELECT b.v FROM b WHERE b.v < a.v AND b.flag = false) \
                LIMIT 100";
    // flag=false の内側は b2(50) だけで `50 < a.v` を満たす外側は無い（全行 NULL）。
    let (core, ctx, _g) = setup_with_limits("sql61-nonagg-limit-ok", CorrelatedLimits::new(4));
    assert_eq!(
        last_col_by_id(&core, &ctx, proj),
        vec![(1, None), (2, None), (3, None), (4, None)]
    );
    assert!(ids(&core, &ctx, wher).is_empty());
    let (core, ctx, _g) = setup_with_limits("sql61-nonagg-limit-ng", CorrelatedLimits::new(3));
    for sql in [proj, wher] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "54000", "sql={sql:?} msg={msg}");
        assert_no_reserved_names(sql, &msg);
    }
    // 等価キーのみ（残余なし）はハッシュ引きで再評価を計上しない（上限 1 でも成功する）。
    let (core, ctx, _g) = setup_with_limits("sql61-nonagg-limit-key", CorrelatedLimits::new(1));
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, {KEY_ONLY} FROM a LIMIT 100")
        ),
        vec![(1, Some(5)), (2, Some(20)), (3, None), (4, None)]
    );
}

#[test]
fn unsupported_shapes_are_rejected_fail_closed() {
    let (core, ctx, _g) = setup();
    let rejects = [
        // 式投影・複数列・`*`。
        "SELECT id, (SELECT b.v + 1 FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        "SELECT id, (SELECT b.v, b.t FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        // 投影・ORDER BY の外側参照。
        "SELECT id, (SELECT a.v FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        "SELECT id, (SELECT b.v FROM b WHERE b.aid = a.k ORDER BY a.v) FROM a LIMIT 100",
        // 内側の SELECT DISTINCT。
        "SELECT id, (SELECT DISTINCT b.v FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        // 集計形で残余・キーなし（#1846 の受理形の外）。
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.v < a.v) FROM a LIMIT 100",
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k OR b.v = a.v) FROM a LIMIT 100",
        // 外側が JOIN・カンマ区切り FROM。
        "SELECT a.id, (SELECT b.v FROM b WHERE b.aid = a.k) FROM a, b LIMIT 100",
        "SELECT a.id, (SELECT b.v FROM b WHERE b.aid = a.k) FROM a JOIN b ON a.k = b.aid LIMIT 100",
    ];
    for sql in rejects {
        let (code, msg) = err(&core, &ctx, sql);
        assert!(
            matches!(code.as_str(), "42601" | "0A000" | "42703"),
            "sql={sql:?} code={code} msg={msg}"
        );
        assert_no_reserved_names(sql, &msg);
    }
    // 述語形 UPDATE／DELETE の WHERE と集計 FILTER の相関スカラーは従来どおり拒否する。
    for sql in [
        "UPDATE a SET v = 1 WHERE v < (SELECT b.v FROM b WHERE b.aid = a.k) USING OPERATION_ID 'u-1'",
        "DELETE FROM a WHERE v < (SELECT b.v FROM b WHERE b.aid = a.k) USING OPERATION_ID 'd-1'",
        "SELECT count(*) FILTER (WHERE v < (SELECT b.v FROM b WHERE b.aid = a.k)) FROM a",
    ] {
        let (code, _) = err(&core, &ctx, sql);
        assert!(
            matches!(code.as_str(), "42601" | "0A000" | "42P01"),
            "sql={sql:?} should stay rejected: {code}"
        );
    }
}

#[test]
fn outer_column_type_without_a_scalar_view_is_rejected_with_0a000_in_projection() {
    let (storage, _g) = open_core("sql61-nonagg-outer-type");
    let p = TableSchema::new(
        "p",
        vec![
            vec_col(),
            col("k", ColumnType::BigInt),
            col("tm", ColumnType::Time),
        ],
    );
    let q = TableSchema::new(
        "q",
        vec![
            vec_col(),
            col("k", ColumnType::BigInt),
            col("w", ColumnType::Integer),
            col("tm", ColumnType::Time),
        ],
    );
    storage.create_table(&p).expect("create p");
    storage.create_table(&q).expect("create q");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    let (code, msg) = err(
        &core,
        &ctx,
        "SELECT id, (SELECT q.w FROM q WHERE q.k = p.k AND q.tm > p.tm) FROM p LIMIT 100",
    );
    assert_eq!(code, "0A000", "{msg}");
    assert_no_reserved_names("outer time", &msg);
}
