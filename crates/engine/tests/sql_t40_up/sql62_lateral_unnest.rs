//! `LATERAL unnest(<左側の配列列>) [WITH ORDINALITY]` の結合テスト（SQL-62・SQL-67・Issue #3360、親 #3319）。
//! ポインタ: TASK-344・RLS-10。設計記録 `docs/design/lateral-derived-table.md`（L18〜L24）。
//!
//! 期待値は本テストが固定値で持つ。alice の可視行（4 行）と同じ表を PostgreSQL 17.12 の実機へ作り、
//! 各問い合わせの結果行・列名・SQLSTATE を照合済み（順序は外側の `ORDER BY` で一意にした形だけを使う）。
//! bob・carol の期待値は、alice の表から他テナントの行を除き自テナントの公開行を足した表を同じ実機で
//! 照合した。PostgreSQL が受理するが本実装が受理しない形（`LATERAL` を付けない後続位置の関数・式や複数の
//! 引数・`unnest` 以外の集合返却関数・相関の `USING`）は固定文言の `0A000` のままにする（受け皿 Issue は
//! 未起票のため追跡注記は付けず、各テストの本文に理由を書く）。`ROWS FROM` だけは #3160 を追跡注記にする。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::correlated_limits::CorrelatedLimits;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{ArrayValue, Value};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const REL: usize = 8;

fn arr_ty(elem: ArrayElemType) -> ColumnType {
    ColumnType::Array(ArrayType::new(elem, 16).expect("array type"))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("name", ColumnType::Text, false),
            ColumnDef::new("arr", arr_ty(ArrayElemType::Integer), true),
            ColumnDef::new("tags", arr_ty(ArrayElemType::Text), true),
            ColumnDef::new("m", arr_ty(ArrayElemType::Integer), true),
            ColumnDef::new("b", arr_ty(ArrayElemType::BigInt), true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ints(v: &[Option<i32>]) -> Value {
    Value::Array(ArrayValue::Integer(v.to_vec()))
}

fn texts(v: &[Option<&str>]) -> Value {
    Value::Array(ArrayValue::Text(
        v.iter().map(|s| s.map(str::to_string)).collect(),
    ))
}

fn bigs(v: &[Option<i64>]) -> Value {
    Value::Array(ArrayValue::BigInt(v.to_vec()))
}

/// 2 次元の整数配列（行優先の平坦な要素と各次元の長さ）。
fn matrix(flat: &[i32], dims: &[u32]) -> Value {
    Value::Array(
        ArrayValue::with_dims(
            ArrayValue::Integer(flat.iter().map(|x| Some(*x)).collect()),
            dims.to_vec(),
        )
        .expect("dims"),
    )
}

struct Seed<'a> {
    id: u64,
    tenant: &'a str,
    name: &'a str,
    arr: Value,
    tags: Value,
    m: Value,
    b: Value,
    visibility: Visibility,
}

fn insert_row(storage: &Storage, r: Seed<'_>) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(r.tenant),
        r.id,
        r.visibility,
        &[
            Value::Vector(vec![r.id as f32, 0.0]),
            Value::Text(r.name.to_string()),
            r.arr,
            r.tags,
            r.m,
            r.b,
        ],
        &OperationId::parse(&format!("seed-{}", r.id)).expect("valid operation id"),
    )
    .expect("insert row");
}

/// 公開行（1〜3）は全テナントから見え、private 行（alice の 4・bob の 5・carol の 7）は持ち主だけが見る。
/// alice の可視行 1〜4 は PostgreSQL 17.12 に同じ値で作って照合した。
fn seed(storage: &Storage) {
    let rows = vec![
        Seed {
            id: 1,
            tenant: "alice",
            name: "a",
            arr: ints(&[Some(10), Some(20), Some(30)]),
            tags: texts(&[Some("x"), Some("y")]),
            m: matrix(&[1, 2, 3, 4], &[2, 2]),
            b: bigs(&[Some(5), Some(6)]),
            visibility: Visibility::Public,
        },
        Seed {
            id: 2,
            tenant: "alice",
            name: "b",
            arr: ints(&[]),
            tags: texts(&[Some("z")]),
            m: Value::Null,
            b: Value::Null,
            visibility: Visibility::Public,
        },
        Seed {
            id: 3,
            tenant: "alice",
            name: "c",
            arr: Value::Null,
            tags: Value::Null,
            m: matrix(&[7, 8], &[2, 1]),
            b: bigs(&[]),
            visibility: Visibility::Public,
        },
        Seed {
            id: 4,
            tenant: "alice",
            name: "d",
            arr: ints(&[Some(7), None, Some(9)]),
            tags: texts(&[None, Some("q r")]),
            m: ints(&[Some(9)]),
            b: bigs(&[Some(1)]),
            visibility: Visibility::Private,
        },
        Seed {
            id: 5,
            tenant: "bob",
            name: "e",
            arr: ints(&[Some(100), Some(200)]),
            tags: texts(&[Some("bob")]),
            m: Value::Null,
            b: Value::Null,
            visibility: Visibility::Private,
        },
        Seed {
            id: 7,
            tenant: "carol",
            name: "f",
            arr: ints(&[Some(1000)]),
            tags: Value::Null,
            m: Value::Null,
            b: Value::Null,
            visibility: Visibility::Private,
        },
    ];
    for r in rows {
        insert_row(storage, r);
    }
}

fn open_with(f: impl FnOnce(EngineCore) -> EngineCore) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-lateral-unnest");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage);
    (
        f(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn open() -> (EngineCore, CleanupGuard) {
    open_with(|c| c)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
        .into_iter()
        .map(|r| r.cells)
        .collect()
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    run(core, tenant, sql).expect_err(sql).wire_code()
}

fn col_names(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Scalar { name, .. } | ColumnMeta::Computed { name, .. } => name.clone(),
            ColumnMeta::Id => "id".to_string(),
            other => panic!("unexpected column meta {other:?}"),
        })
        .collect()
}

fn i(n: i64) -> Cell {
    Cell::SignedInteger(n)
}

fn t(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn null() -> Cell {
    Cell::Null
}

const ALICE_ORDINALITY: fn() -> Vec<Vec<Cell>> = || {
    vec![
        vec![t("a"), i(10), i(1)],
        vec![t("a"), i(20), i(2)],
        vec![t("a"), i(30), i(3)],
        vec![t("d"), i(7), i(1)],
        vec![t("d"), null(), i(2)],
        vec![t("d"), i(9), i(3)],
    ]
};

const ORD: &str = "unnest(d.arr) WITH ORDINALITY AS u(v, n)";

/// 受理する構文（CROSS・コンマ・`[INNER] JOIN ... ON true`）が同じ結果を返す。期待値は PostgreSQL 17.12 と
/// 照合済み（要素の NULL は値 NULL の行・NULL 配列と空配列は 0 行・序数は外側の行ごとに 1 から）。
#[test]
fn inner_forms_return_one_row_per_element_with_a_restarted_ordinality() {
    let (core, _g) = open();
    for from in [
        format!("docs d CROSS JOIN LATERAL {ORD}"),
        format!("docs d, LATERAL {ORD}"),
        format!("docs d JOIN LATERAL {ORD} ON true"),
        format!("docs d INNER JOIN LATERAL {ORD} ON true"),
    ] {
        let sql = format!("SELECT d.name, u.v, u.n FROM {from} ORDER BY d.name, u.n");
        assert_eq!(rows(&core, "alice", &sql), ALICE_ORDINALITY(), "{sql}");
        // psql が末尾 `;` 付きで単独送信する形も同じ結果。
        let with_semicolon = format!("{sql};");
        assert_eq!(
            rows(&core, "alice", &with_semicolon),
            ALICE_ORDINALITY(),
            "{with_semicolon}"
        );
    }
}

/// ON 条件・WHERE は展開後の行に効く（JOIN の述語は列どうしと文字列リテラルとの比較が書ける範囲）。
#[test]
fn inner_join_on_condition_filters_after_expansion() {
    let (core, _g) = open();
    let expected = vec![
        vec![t("a"), i(10), i(1)],
        vec![t("a"), i(20), i(2)],
        vec![t("a"), i(30), i(3)],
        vec![t("d"), i(7), i(1)],
        vec![t("d"), i(9), i(3)],
    ];
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d INNER JOIN LATERAL \
             unnest(d.arr) WITH ORDINALITY AS u(v, n) ON u.v > u.n ORDER BY d.name, u.n"
        ),
        expected
    );
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
             unnest(d.arr) WITH ORDINALITY AS u(v, n) WHERE u.v > u.n ORDER BY d.name, u.n"
        ),
        expected
    );
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
             unnest(d.tags) WITH ORDINALITY AS u(v, n) WHERE u.v = 'x' ORDER BY d.name, u.n"
        ),
        vec![vec![t("a"), t("x"), i(1)]]
    );
    // 外側の行 id との比較（整数の行 id と bigint の序数）。
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
             unnest(d.arr) WITH ORDINALITY AS u(v, n) WHERE u.n = d.id ORDER BY d.name, u.n"
        ),
        vec![vec![t("a"), i(10), i(1)]]
    );
}

/// `LEFT JOIN LATERAL ... ON true`: NULL 配列・空配列の外側行が NULL 補完（序数も NULL）で残る。
#[test]
fn left_join_keeps_outer_rows_with_null_or_empty_arrays() {
    let (core, _g) = open();
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d LEFT JOIN LATERAL \
             unnest(d.arr) WITH ORDINALITY AS u(v, n) ON true ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), i(10), i(1)],
            vec![t("a"), i(20), i(2)],
            vec![t("a"), i(30), i(3)],
            vec![t("b"), null(), null()],
            vec![t("c"), null(), null()],
            vec![t("d"), i(7), i(1)],
            vec![t("d"), null(), i(2)],
            vec![t("d"), i(9), i(3)],
        ]
    );
    // ON 条件は展開の後に効き、一致の無い外側の行は NULL 補完で残る（`LEFT OUTER` の綴りも同じ）。
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d LEFT OUTER JOIN LATERAL \
             unnest(d.arr) WITH ORDINALITY AS u(v, n) ON u.v > u.n ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), i(10), i(1)],
            vec![t("a"), i(20), i(2)],
            vec![t("a"), i(30), i(3)],
            vec![t("b"), null(), null()],
            vec![t("c"), null(), null()],
            vec![t("d"), i(7), i(1)],
            vec![t("d"), i(9), i(3)],
        ]
    );
    // 一致が 1 件も無い外側の行（要素が全て条件を満たさない）も NULL 補完で残る。
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d LEFT JOIN LATERAL \
             unnest(d.tags) WITH ORDINALITY AS u(v, n) ON u.v = 'q r' ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), null(), null()],
            vec![t("b"), null(), null()],
            vec![t("c"), null(), null()],
            vec![t("d"), t("q r"), i(2)],
        ]
    );
    // 外側の行 id との比較（一致の無い外側の行は NULL 補完）。
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d LEFT JOIN LATERAL \
             unnest(d.arr) WITH ORDINALITY AS u(v, n) ON u.n = d.id ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), i(10), i(1)],
            vec![t("b"), null(), null()],
            vec![t("c"), null(), null()],
            vec![t("d"), null(), null()],
        ]
    );
    // 序数なし。NULL は昇順で最後。
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v FROM docs d LEFT JOIN LATERAL unnest(d.arr) AS u(v) ON true \
             ORDER BY d.name, u.v"
        ),
        vec![
            vec![t("a"), i(10)],
            vec![t("a"), i(20)],
            vec![t("a"), i(30)],
            vec![t("b"), null()],
            vec![t("c"), null()],
            vec![t("d"), i(7)],
            vec![t("d"), i(9)],
            vec![t("d"), null()],
        ]
    );
}

/// 多次元配列は行優先に平坦化する。要素型は `text`・`bigint` も扱う。
#[test]
fn multi_dimensional_arrays_flatten_and_other_element_types_work() {
    let (core, _g) = open();
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
             unnest(d.m) WITH ORDINALITY AS u(v, n) ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), i(1), i(1)],
            vec![t("a"), i(2), i(2)],
            vec![t("a"), i(3), i(3)],
            vec![t("a"), i(4), i(4)],
            vec![t("c"), i(7), i(1)],
            vec![t("c"), i(8), i(2)],
            vec![t("d"), i(9), i(1)],
        ]
    );
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
             unnest(d.tags) WITH ORDINALITY AS u(v, n) ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), t("x"), i(1)],
            vec![t("a"), t("y"), i(2)],
            vec![t("b"), t("z"), i(1)],
            vec![t("d"), null(), i(1)],
            vec![t("d"), t("q r"), i(2)],
        ]
    );
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
             unnest(d.b) WITH ORDINALITY AS u(v, n) ORDER BY d.name, u.n"
        ),
        vec![
            vec![t("a"), i(5), i(1)],
            vec![t("a"), i(6), i(2)],
            vec![t("d"), i(1), i(1)],
        ]
    );
}

/// 件数・集計・連鎖した 2 つの `LATERAL unnest` は展開後の行に効く（NULL 要素も 1 行として数える）。
#[test]
fn count_and_chained_unnests_see_expanded_rows() {
    let (core, _g) = open();
    let count = |sql: &str| -> i64 {
        match rows(&core, "alice", sql).first().and_then(|r| r.first()) {
            Some(Cell::Integer(n)) => i64::try_from(*n).expect("count"),
            Some(Cell::SignedInteger(n)) => *n,
            other => panic!("count: {other:?}"),
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM docs d CROSS JOIN LATERAL unnest(d.arr) AS u(v)"),
        6
    );
    // 同じ外側の行から 2 つの配列を展開する（外側の行ごとの直積）。
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT d.name, u.v, w.v FROM docs d CROSS JOIN LATERAL unnest(d.arr) AS u(v) \
             CROSS JOIN LATERAL unnest(d.b) AS w(v) ORDER BY d.name, u.v, w.v"
        ),
        vec![
            vec![t("a"), i(10), i(5)],
            vec![t("a"), i(10), i(6)],
            vec![t("a"), i(20), i(5)],
            vec![t("a"), i(20), i(6)],
            vec![t("a"), i(30), i(5)],
            vec![t("a"), i(30), i(6)],
            vec![t("d"), i(7), i(1)],
            vec![t("d"), i(9), i(1)],
            vec![t("d"), null(), i(1)],
        ]
    );
}

/// 別名なしの `unnest` を同じ FROM 内で連鎖させても重複テーブル名として拒否しない（PostgreSQL は許す）。
#[test]
fn chained_unnests_without_alias_are_accepted() {
    let (core, _g) = open();
    let n = rows(
        &core,
        "alice",
        "SELECT count(*) FROM docs d CROSS JOIN LATERAL unnest(d.arr) CROSS JOIN LATERAL unnest(d.b)",
    );
    assert_eq!(n, vec![vec![Cell::Integer(9)]]);
    let n = rows(
        &core,
        "alice",
        "SELECT count(*) FROM docs d, LATERAL unnest(d.arr), LATERAL unnest(d.b)",
    );
    assert_eq!(n, vec![vec![Cell::Integer(9)]]);
}

/// 左側が CTE・派生表・自己結合の relation でも、引数の列から配列を展開できる。
#[test]
fn left_relation_may_be_a_cte_a_derived_table_or_a_self_join() {
    let (core, _g) = open();
    let expected = vec![
        vec![t("a"), i(10)],
        vec![t("a"), i(20)],
        vec![t("a"), i(30)],
        vec![t("d"), i(7)],
        vec![t("d"), i(9)],
        vec![t("d"), null()],
    ];
    for sql in [
        "WITH c AS (SELECT name, arr FROM docs) SELECT c.name, u.v FROM c \
         CROSS JOIN LATERAL unnest(c.arr) AS u(v) ORDER BY c.name, u.v",
        "SELECT x.name, u.v FROM (SELECT name, arr FROM docs) x \
         CROSS JOIN LATERAL unnest(x.arr) AS u(v) ORDER BY x.name, u.v",
        "SELECT d1.name, u.v FROM docs d1 JOIN docs d2 ON d1.name = d2.name \
         CROSS JOIN LATERAL unnest(d2.arr) AS u(v) ORDER BY d1.name, u.v",
        // 非修飾の引数名は左側にだけある列として解決する。
        "SELECT d.name, u.v FROM docs d CROSS JOIN LATERAL unnest(arr) AS u(v) \
         ORDER BY d.name, u.v",
    ] {
        assert_eq!(rows(&core, "alice", sql), expected, "{sql}");
    }
}

/// 公開名（relation 名・列名）は PostgreSQL 17.12 と同じ。別名なしは `unnest`、別名だけなら値の列が別名
/// （序数は `ordinality`）、列名リストで改名する。列名リストが列数を超えたら `42P10`。
#[test]
fn output_column_names_follow_alias_rules() {
    let (core, _g) = open();
    // `SELECT *` の末尾が関数 relation の列（`width` 個。序数ありは 2、なしは 1）。
    let names = |from: &str| {
        let all = col_names(
            &core,
            "alice",
            &format!("SELECT * FROM docs d, LATERAL {from}"),
        );
        let width = if from.contains("ORDINALITY") { 2 } else { 1 };
        all.get(all.len().saturating_sub(width)..)
            .unwrap_or(&[])
            .to_vec()
    };
    assert_eq!(
        names("unnest(d.arr) WITH ORDINALITY AS u(v, n)"),
        vec!["v", "n"]
    );
    assert_eq!(
        names("unnest(d.arr) WITH ORDINALITY AS u"),
        vec!["u", "ordinality"]
    );
    assert_eq!(
        names("unnest(d.arr) WITH ORDINALITY AS u(x)"),
        vec!["x", "ordinality"]
    );
    assert_eq!(names("unnest(d.arr) AS u"), vec!["u"]);
    assert_eq!(names("unnest(d.arr) AS u(x)"), vec!["x"]);
    assert_eq!(names("unnest(d.arr)"), vec!["unnest"]);
    assert_eq!(
        names("unnest(d.arr) WITH ORDINALITY"),
        vec!["unnest", "ordinality"]
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT d.name FROM docs d, LATERAL unnest(d.arr) WITH ORDINALITY AS u(a, b, c)"
        ),
        "42P10"
    );
}

/// Describe は行を評価せず、Execute と同じ列名・列型を公告する（序数は `bigint`）。
#[test]
fn describe_matches_execute_columns() {
    let (core, _g) = open();
    let sql = format!("SELECT u.v, u.n FROM docs d CROSS JOIN LATERAL {ORD}");
    let parsed = core.parse_sql(&sql).expect("parse");
    let session = SessionState::default();
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("columns");
    let executed = run(&core, "alice", &sql).expect("execute").columns;
    assert_eq!(described, executed);
    assert!(matches!(
        described.first(),
        Some(ColumnMeta::Scalar {
            ty: ColumnType::Integer,
            ..
        })
    ));
    assert!(matches!(
        described.get(1),
        Some(ColumnMeta::Scalar {
            ty: ColumnType::BigInt,
            ..
        })
    ));
    // 非配列列は Describe でも Execute と同じ `42883`。
    let bad = core
        .parse_sql("SELECT u.v FROM docs d CROSS JOIN LATERAL unnest(d.name) AS u(v)")
        .expect("parse");
    let e = core
        .describe_parsed_in_session(&session, &bad)
        .expect_err("non-array column");
    assert_eq!(e.wire_code(), "42883");
}

/// 3 テナントの越境: 他テナントの行・配列要素は結果にも件数にも現れない（ポリシーなしの可視性でも同じ）。
/// bob は alice の public 行（1〜3）と自分の行（5）、carol は public 行（1〜3）と自分の行（7）だけを見る。
#[test]
fn tenants_never_see_each_others_elements_or_counts() {
    let (core, _g) = open();
    let sql = "SELECT d.name, u.v, u.n FROM docs d CROSS JOIN LATERAL \
               unnest(d.arr) WITH ORDINALITY AS u(v, n) ORDER BY d.name, u.n";
    assert_eq!(
        rows(&core, "bob", sql),
        vec![
            vec![t("a"), i(10), i(1)],
            vec![t("a"), i(20), i(2)],
            vec![t("a"), i(30), i(3)],
            vec![t("e"), i(100), i(1)],
            vec![t("e"), i(200), i(2)],
        ]
    );
    assert_eq!(
        rows(&core, "carol", sql),
        vec![
            vec![t("a"), i(10), i(1)],
            vec![t("a"), i(20), i(2)],
            vec![t("a"), i(30), i(3)],
            vec![t("f"), i(1000), i(1)],
        ]
    );
    let count_sql = "SELECT count(*) FROM docs d CROSS JOIN LATERAL unnest(d.arr) AS u(v)";
    let count = |tenant: &str| match rows(&core, tenant, count_sql)
        .first()
        .and_then(|r| r.first())
    {
        Some(Cell::Integer(n)) => i64::try_from(*n).expect("count"),
        Some(Cell::SignedInteger(n)) => *n,
        other => panic!("count: {other:?}"),
    };
    assert_eq!(count("alice"), 6);
    assert_eq!(count("bob"), 5);
    assert_eq!(count("carol"), 4);
    // 他テナントの private 行（alice の id 4）の NULL 要素の行も bob・carol の結果に現れない。
    let all = "SELECT d.name, u.v FROM docs d LEFT JOIN LATERAL unnest(d.arr) AS u(v) ON true";
    for tenant in ["bob", "carol"] {
        assert!(rows(&core, tenant, all)
            .iter()
            .all(|r| !matches!(r.first(), Some(Cell::Text(n)) if n == "d")));
    }
}

/// 出力行の上限・再評価の上限は評価の前に `54000`（部分結果なし）。判定は可視行だけで行う。
#[test]
fn caps_reject_with_54000_and_count_only_visible_rows() {
    let sql = "SELECT d.name, u.v FROM docs d CROSS JOIN LATERAL unnest(d.arr) AS u(v)";
    // alice の結果は 6 行（ちょうどは通り、5 行は超過）。
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 6)));
    assert_eq!(rows(&core, "alice", sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 5)));
    assert_eq!(code(&core, "alice", sql), "54000");
    // bob の可視の要素は 5 件・carol は 4 件で、同じ上限でも通る（他テナントの要素は数えない）。
    assert_eq!(rows(&core, "bob", sql).len(), 5);
    assert_eq!(rows(&core, "carol", sql).len(), 4);
    // 外側の入力行の上限（alice の `docs d` は 4 行）。
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 3, 1_000)));
    assert_eq!(code(&core, "alice", sql), "54000");
    // 再評価の上限（通過した外側タプル 4 件。ちょうどは通り 1 件でも超えれば `54000`）。
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(4)));
    assert_eq!(rows(&core, "alice", sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(3)));
    assert_eq!(code(&core, "alice", sql), "54000");
    // 部分結果・副作用なし。
    assert_eq!(rows(&core, "alice", "SELECT * FROM docs").len(), 4);
}

/// 未対応の形は固定の `0A000`（`unnest` 以外の集合返却関数・複数の引数・式や定数の引数）。
/// PostgreSQL は受理するが本実装は未対応のため追跡注記は付けず、理由はここに書く。
#[test]
fn unsupported_function_forms_are_rejected_with_a_fixed_0a000() {
    let (core, _g) = open();
    for from in [
        "LATERAL generate_series(1, d.id) AS u(v)",
        "LATERAL unnest(d.arr, d.tags) AS u(v, w)",
        "LATERAL unnest(ARRAY[1, 2]) AS u(v)",
        "LATERAL unnest(d.arr || d.arr) AS u(v)",
        "LATERAL f(d.id) AS u(v)",
    ] {
        let sql = format!("SELECT d.name FROM docs d CROSS JOIN {from}");
        assert_eq!(code(&core, "alice", &sql), "0A000", "{sql}");
    }
    // `LATERAL` を付けない後続位置の関数は従来どおり構文誤り（`sql60_comma_join.rs` の契約）。
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT d.name FROM docs d, unnest(d.arr) AS u(v)"
        ),
        "42601"
    );
    // 相関の `USING`（PostgreSQL は受理）は固定文言の `0A000`。
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT d.name FROM docs d JOIN LATERAL unnest(d.arr) AS u(id) USING (id)"
        ),
        "0A000"
    );
}

/// 束縛エラー: 非配列は `42883`、引数なしも `42883`、未知の修飾子は `42P01`、未知列は `42703`、
/// RIGHT／FULL と左側の参照は `42P10`、`WITH` の後が `ORDINALITY` 以外は `42601`（PostgreSQL 17.12 と照合済み）。
#[test]
fn binding_errors_follow_postgres_sqlstates() {
    let (core, _g) = open();
    let cases = [
        ("CROSS JOIN LATERAL unnest(d.name) AS u(v)", "42883"),
        ("CROSS JOIN LATERAL unnest(d.id) AS u(v)", "42883"),
        ("CROSS JOIN LATERAL unnest() AS u(v)", "42883"),
        ("CROSS JOIN LATERAL unnest(zz.arr) AS u(v)", "42P01"),
        ("CROSS JOIN LATERAL unnest(d.nope) AS u(v)", "42703"),
        ("RIGHT JOIN LATERAL unnest(d.arr) AS u(v) ON true", "42P10"),
        ("FULL JOIN LATERAL unnest(d.arr) AS u(v) ON true", "42P10"),
        ("CROSS JOIN LATERAL unnest(d.arr) WITH foo AS u(v)", "42601"),
    ];
    for (tail, expected) in cases {
        let sql = format!("SELECT d.name FROM docs d {tail}");
        assert_eq!(code(&core, "alice", &sql), expected, "{sql}");
    }
}

/// `ROWS FROM (...)` は未対応で固定の `0A000`（LATERAL 位置・単独位置とも。Issue #3360 では対象外）。
/// tracked: #3160
#[test]
fn rows_from_is_rejected_with_a_fixed_0a000() {
    let (core, _g) = open();
    for sql in [
        "SELECT d.name FROM docs d CROSS JOIN LATERAL ROWS FROM (unnest(d.arr)) AS u(v)",
        "SELECT d.name FROM docs d, LATERAL ROWS FROM (unnest(d.arr))",
    ] {
        assert_eq!(code(&core, "alice", sql), "0A000", "{sql}");
    }
}

/// EXPLAIN・`$n`・ビュー本文の中の `LATERAL unnest` は #3324 の範囲で、従来どおり拒否される。
/// tracked: #3324
#[test]
fn explain_is_rejected_until_the_lateral_explain_issue() {
    let (core, _g) = open();
    assert_eq!(
        code(
            &core,
            "alice",
            "EXPLAIN SELECT d.name FROM docs d CROSS JOIN LATERAL unnest(d.arr) AS u(v)"
        ),
        "0A000"
    );
}
