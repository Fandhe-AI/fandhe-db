//! 相関 `EXISTS` の半結合・行ごとの再評価の結合テスト（SQL-61 (2)。`docs/spec/04-behavior/` の
//! ポインタのみ。Issue #1842、親 #1840。テナント境界は RLS-10 ポインタ）。
//!
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、期待値は
//! PostgreSQL 17 の意味論から手計算した固定値（各ケースのコメントに根拠）。等価の相関述語は
//! ハッシュ半結合、非等価・OR 混在・キーにできない型は外側の行ごとの再評価で評価される。
//! 再評価回数・比較回数・内側入力行数の上限超過（`54000`）は、上限を差し替えられる
//! `sql::subquery` 内の単体テストが固定する（本ファイルは既定上限での結果を検証する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ClassifiedError;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn a_schema() -> TableSchema {
    TableSchema::new(
        "a",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("k", ColumnType::BigInt, true),
            ColumnDef::new("t", ColumnType::Text, true),
            ColumnDef::new("v", ColumnType::Integer, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new("only_a", ColumnType::Text, true),
        ],
    )
}

fn b_schema() -> TableSchema {
    TableSchema::new(
        "b",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("aid", ColumnType::BigInt, true),
            ColumnDef::new("t", ColumnType::Text, true),
            ColumnDef::new("v", ColumnType::Integer, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
        ],
    )
}

fn c_schema() -> TableSchema {
    TableSchema::new(
        "c",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("aid", ColumnType::BigInt, true),
        ],
    )
}

fn d_schema() -> TableSchema {
    TableSchema::new(
        "d",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("v", ColumnType::Integer, true),
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

/// 結果の `id` を返す順のまま集める（`ORDER BY` の検証用）。
fn ids_in_order(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let mut v = ids_in_order(core, ctx, sql);
    v.sort_unstable();
    v
}

fn err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, String) {
    match core.execute_sql(ctx, sql) {
        Ok(r) => panic!("sql={sql:?} should be rejected but returned {r:?}"),
        Err(e) => (e.error_class().wire_code().to_string(), e.to_string()),
    }
}

/// 標準フィクスチャ（テナント `t1`）。
///
/// a: id1 {k1, t'x', v10, d1.5}、id2 {k2, t'y', v20, d2.5}、id3 {k3, t NULL, v30, d NULL}、
///    id4 {k NULL, t'x', v5, d NULL}
/// b: id1 {aid1, t'x', v5, d1.5, flag true}、id2 {aid1, t'z', v50, d9.0, flag false}、
///    id3 {aid2, t'y', v20, d2.5, flag true}、id4 {aid NULL, t NULL, v1, flag true}
/// c: id1 {aid1}
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql61-correlated-exists");
    let storage = Storage::open(&path).expect("open storage");
    for schema in [a_schema(), b_schema(), c_schema(), d_schema()] {
        storage.create_table(&schema).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let guard = CleanupGuard(path);
    let ctx = ctx_for("t1");
    ins(
        &core,
        &ctx,
        "a",
        1,
        &[("k", "1"), ("t", "'x'"), ("v", "10"), ("d", "1.5")],
    );
    ins(
        &core,
        &ctx,
        "a",
        2,
        &[("k", "2"), ("t", "'y'"), ("v", "20"), ("d", "2.5")],
    );
    ins(&core, &ctx, "a", 3, &[("k", "3"), ("v", "30")]);
    ins(&core, &ctx, "a", 4, &[("t", "'x'"), ("v", "5")]);
    ins(
        &core,
        &ctx,
        "b",
        1,
        &[
            ("aid", "1"),
            ("t", "'x'"),
            ("v", "5"),
            ("d", "1.5"),
            ("flag", "true"),
        ],
    );
    ins(
        &core,
        &ctx,
        "b",
        2,
        &[
            ("aid", "1"),
            ("t", "'z'"),
            ("v", "50"),
            ("d", "9.0"),
            ("flag", "false"),
        ],
    );
    ins(
        &core,
        &ctx,
        "b",
        3,
        &[
            ("aid", "2"),
            ("t", "'y'"),
            ("v", "20"),
            ("d", "2.5"),
            ("flag", "true"),
        ],
    );
    ins(&core, &ctx, "b", 4, &[("v", "1"), ("flag", "true")]);
    ins(&core, &ctx, "c", 1, &[("aid", "1")]);
    // 2 行ある d はスカラーサブクエリの基数違反（21000）を起こすために使う。
    ins(&core, &ctx, "d", 1, &[("v", "1")]);
    ins(&core, &ctx, "d", 2, &[("v", "2")]);
    (core, ctx, guard)
}

#[test]
fn sql61_equality_semi_join_matches_on_pseudo_id_and_column_keys() {
    let (core, ctx, _g) = setup();
    // b.aid の値集合は {1, 2}（NULL は一致しない）。疑似列 id との等価: a.id が 1,2 の行が真。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.id) LIMIT 100"
        ),
        vec![1, 2]
    );
    // 列キー: a.k が {1, 2}。a4 は k が NULL で偽（EXISTS は UNKNOWN にならない）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1, 2]
    );
    // 左右を入れ替えた等価・非修飾の外側専用列・別名の外側も同じ。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a x WHERE EXISTS (SELECT id FROM b WHERE x.k = b.aid LIMIT 1) LIMIT 100"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE aid = k) LIMIT 100"
        ),
        vec![1, 2]
    );
}

#[test]
fn sql61_semi_join_combines_with_inner_only_conditions_and_composite_keys() {
    let (core, ctx, _g) = setup();
    // 内側だけの条件 flag = true: aid の集合は {1 (b1), 2 (b3)}。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.flag = true) LIMIT 100"
        ),
        vec![1, 2]
    );
    // flag = false は b2 (aid 1) だけ。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.flag = false) LIMIT 100"
        ),
        vec![1]
    );
    // 複合キー (aid, t): a1 (1,'x') は b1、a2 (2,'y') は b3 に一致。a3 は t が NULL で偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.t = a.t) LIMIT 100"
        ),
        vec![1, 2]
    );
    // 外側だけの条件は残余として評価される: a.t = 'y' と一致するのは a2 のみ。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND a.t = 'y') LIMIT 100"
        ),
        vec![2]
    );
}

#[test]
fn sql61_semi_join_has_no_distinct_key_cap() {
    // IN (SELECT ...) の集合照合は 1 サイト 256 値で 54000 になるが、相関 EXISTS のハッシュ半結合は
    // その上限を受け継がない。内側 300 行（aid 1000..1299）・外側 3 行 (k=1000, 1299, 5000)。
    let path = unique_db_path("sql61-correlated-exists-wide");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&a_schema()).expect("a");
    storage.create_table(&b_schema()).expect("b");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _g = CleanupGuard(path);
    let ctx = ctx_for("t1");
    for chunk in 0..5 {
        let rows: Vec<String> = (0..60)
            .map(|i| {
                let n = chunk * 60 + i;
                format!("({}, '[0.1,0.2]', {})", 1 + n, 1000 + n)
            })
            .collect();
        run(
            &core,
            &ctx,
            &format!(
                "INSERT INTO b (id, embedding, aid) VALUES {} USING OPERATION_ID 'wide-b-{chunk}'",
                rows.join(", ")
            ),
        );
    }
    ins(&core, &ctx, "a", 1, &[("k", "1000")]);
    ins(&core, &ctx, "a", 2, &[("k", "1299")]);
    ins(&core, &ctx, "a", 3, &[("k", "5000")]);
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1, 2]
    );
}

#[test]
fn sql61_non_equality_correlations_use_per_row_reevaluation() {
    let (core, ctx, _g) = setup();
    // b.v の集合は {5, 50, 20, 1}。b.v > a.v + 25 は a1 (>35: 50)、a2 (>45: 50)、a4 (>30: 50) が真、
    // a3 (>55) は偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.v > a.v + 25) LIMIT 100"
        ),
        vec![1, 2, 4]
    );
    // 等価キー ＋ 非等価の残余: a1 は aid=1 のうち v > 10 (b2: 50) で真。a2 は aid=2 の b3 が v=20 で
    // 20 > 20 は偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.v > a.v) LIMIT 100"
        ),
        vec![1]
    );
    // 外側と内側が混ざった OR は等価キーに分解できず残余: a1・a2 は aid 一致、a3 は一致なし、
    // a4 は k が NULL のため (NULL OR 偽) は真にならない。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE (b.aid = a.k OR b.t = 'none')) LIMIT 100"
        ),
        vec![1, 2]
    );
    // OR の片側が内側だけの条件でも、外側参照を含む連言は残余（b.t = 'z' の b2 は aid=1）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE (b.aid = a.k OR b.t = 'z') AND b.v > 40) LIMIT 100"
        ),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn sql61_unkeyable_types_fall_back_to_reevaluation_instead_of_failing() {
    let (core, ctx, _g) = setup();
    // DOUBLE は結合キーにできないが PostgreSQL は受理する。a1 (1.5)・a2 (2.5) が b の d と一致し、
    // NULL の a3・a4 は偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.d = a.d) LIMIT 100"
        ),
        vec![1, 2]
    );
    // 型クラスが異なる等価（整数 = 実数）も残余で評価され、b.v = 20 と a.d = 2.5 は不一致、
    // b.aid (1) と a.d (1.5) も不一致。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.d) LIMIT 100"
        ),
        Vec::<u64>::new()
    );
}

#[test]
fn sql61_inner_offset_is_preserved_per_outer_row() {
    let (core, ctx, _g) = setup();
    // OFFSET 1 は「相関で絞った後の件数 > 1」と同値。aid=1 は 2 件 (b1, b2) で真、aid=2 は 1 件で偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k LIMIT 5 OFFSET 1) LIMIT 100"
        ),
        vec![1]
    );
    // 残余を伴う形でも同じ: b.v > a.v - 100 は一致する全行で真なので件数は等価キーだけで決まる。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.v > a.v - 100 LIMIT 5 OFFSET 1) LIMIT 100"
        ),
        vec![1]
    );
    // キー無しの再評価: b.v > a.v を満たす件数は a1: 2 (50, 20)、a2: 1、a3: 1、a4: 2 (50, 20)。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.v > a.v LIMIT 5 OFFSET 1) LIMIT 100"
        ),
        vec![1, 4]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.v > a.v LIMIT 5 OFFSET 2) LIMIT 100"
        ),
        Vec::<u64>::new()
    );
    // LIMIT 1 は真偽に影響しない。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k LIMIT 1) LIMIT 100"
        ),
        vec![1, 2]
    );
}

#[test]
fn sql61_correlated_exists_composes_with_or_aggregate_order_and_limit() {
    let (core, ctx, _g) = setup();
    // OR の分岐内: a.t = 'y' (a2) または EXISTS (a1, a2) → {1, 2}。a4 (t='x', k NULL) は偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE a.t = 'y' OR EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1, 2]
    );
    // AND との併用。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v > 15 AND EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![2]
    );
    // 集計の WHERE。
    let r = core
        .execute_sql(
            &ctx,
            "SELECT COUNT(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)",
        )
        .expect("aggregate");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells, vec![Cell::Integer(2)]);
    // 外側の ORDER BY / LIMIT: 真になる a1 (v10)・a2 (v20) を v 降順で 2 件 → [2, 1]。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) ORDER BY v DESC LIMIT 2"
        ),
        vec![2, 1]
    );
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) ORDER BY v LIMIT 1"
        ),
        vec![1]
    );
}

#[test]
fn sql61_nested_correlated_exists_resolves_inner_level_references() {
    let (core, ctx, _g) = setup();
    // 深さ 2 の EXISTS が 1 つ外側 (b) を参照する形。a1 は aid=1 の b1, b2 のうち c.aid = b.aid=1 が
    // 存在して真。a2 は b3 (aid 2) に対応する c が無く偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND EXISTS (SELECT 1 FROM c WHERE c.aid = b.aid)) LIMIT 100"
        ),
        vec![1]
    );
}

#[test]
fn sql61_other_tenants_rows_are_not_observable_through_the_correlation() {
    // RLS-10: 相関の内側走査も呼び出したテナントの可視行だけ。t2 にだけ b.aid = 1 がある場合、
    // t1 の a(k=1) は真にならない。t1 の可視行の件数が同じなら、t2 の行数を増やしても結果・
    // エラーは変わらない。
    let (core, ctx, _g) = setup();
    let t2 = ctx_for("t2");
    ins(&core, &t2, "a", 100, &[("k", "100")]);
    ins(&core, &t2, "b", 100, &[("aid", "100")]);
    ins(&core, &t2, "b", 101, &[("aid", "1")]);
    ins(&core, &t2, "b", 102, &[("aid", "3")]);
    let sql = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100";
    // t1 の a3 (k=3) は t2 の b.aid=3 を見ない。
    assert_eq!(ids(&core, &ctx, sql), vec![1, 2]);
    // t2 は自分の行だけで判定する（a100 は b100、a(k=100) のみ）。
    assert_eq!(ids(&core, &t2, sql), vec![100]);
    // 再評価経路でも同じ。
    let sql2 = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.v > a.v - 1000) LIMIT 100";
    assert_eq!(ids(&core, &ctx, sql2), vec![1, 2]);
    // 未対応の形のエラーは他テナントの行数に依存しない。
    let unsupported =
        "SELECT id FROM a WHERE t = (SELECT lower(t) FROM b WHERE b.aid = a.k LIMIT 1) LIMIT 100";
    assert_eq!(err(&core, &ctx, unsupported), err(&core, &t2, unsupported));
}

#[test]
fn sql61_unsupported_correlated_forms_stay_fail_closed() {
    let (core, ctx, _g) = setup();
    for sql in [
        // 相関 IN／NOT IN は Issue #1844 で実行できる（`tests/sql61_correlated_in.rs`）。明示
        // `LIMIT`／`OFFSET` 付きは集合の帰属を変えるため fail-closed のまま。非集計形の相関スカラーは
        // Issue #1847 で実行できるため、式投影（単一の裸の列でない形）を固定する。
        "SELECT id FROM a WHERE t IN (SELECT t FROM b WHERE b.aid = a.k LIMIT 5) LIMIT 100",
        "SELECT id FROM a WHERE t NOT IN (SELECT t FROM b WHERE b.aid = a.k LIMIT 5) LIMIT 100",
        "SELECT id FROM a WHERE t = (SELECT lower(t) FROM b WHERE b.aid = a.k LIMIT 1) LIMIT 100",
        // 直近より外側の参照。
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.aid = a.k)) LIMIT 100",
        // WHERE 以外（投影・ORDER BY）の外側参照。
        "SELECT id FROM a WHERE EXISTS (SELECT a.k FROM b WHERE b.aid = a.k) LIMIT 100",
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k ORDER BY a.k LIMIT 1) LIMIT 100",
        // 集計の FILTER は相関を実行しない。
        "SELECT COUNT(*) FILTER (WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)) FROM a",
        "SELECT COUNT(*) FILTER (WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)) FROM a",
        // 直近より外側の参照（否定形でも同じ）。
        "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.aid = a.k)) LIMIT 100",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "42601", "{sql}: {msg}");
        assert!(msg.contains("correlated"), "{sql}: {msg}");
        assert!(!msg.contains('\u{2}'), "{sql}: {msg:?}");
    }
    // 述語形 DELETE／UPDATE の WHERE も相関を実行しない（台帳・content hash への影響を扱わない）。
    for sql in [
        "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = k) USING OPERATION_ID 'x1'",
        "UPDATE a SET t = 'q' WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = k) USING OPERATION_ID 'x2'",
        "DELETE FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.aid = k) USING OPERATION_ID 'x3'",
        "UPDATE a SET t = 'q' WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.aid = k) USING OPERATION_ID 'x4'",
    ] {
        let e = core
            .execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .expect_err("DML correlation must be rejected");
        assert_eq!(e.error_class().wire_code(), "42601", "{sql}: {e}");
        assert!(e.to_string().contains("correlated"), "{sql}: {e}");
    }
    // 拒否された DML は何も変更していない。
    assert_eq!(
        ids(&core, &ctx, "SELECT id FROM a WHERE t = 'q' LIMIT 100"),
        Vec::<u64>::new()
    );
}

#[test]
fn sql61_correlated_exists_sees_rows_committed_before_the_statement() {
    // 文ごとにプローブを作り直す（文をまたいで保持しない）ため、挿入後の再実行は新しい行を反映する。
    let (core, ctx, _g) = setup();
    let sql = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100";
    assert_eq!(ids(&core, &ctx, sql), vec![1, 2]);
    ins(&core, &ctx, "b", 50, &[("aid", "3")]);
    assert_eq!(ids(&core, &ctx, sql), vec![1, 2, 3]);
}

#[test]
fn sql61_residual_binding_errors_do_not_leak_the_reserved_prefix() {
    // 残余の束縛エラー（型の不一致など）の文言に、外側参照の予約内部名（制御文字の接頭辞）を出さない。
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND a.t > b.v) LIMIT 100",
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE a.t = b.v) LIMIT 100",
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.t LIKE a.t) LIMIT 100",
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.flag = a.t) LIMIT 100",
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE a.nope = b.v) LIMIT 100",
    ] {
        // 受理されるか拒否されるかは型規則に従う。拒否されるときだけ文言を検査する。
        if let Err(e) = core.execute_sql(&ctx, sql) {
            let msg = e.to_string();
            assert!(!msg.contains('\u{2}'), "{sql}: {msg:?}");
        }
    }
}

#[test]
fn sql61_group_by_and_secondary_index_paths_decode_the_outer_key_columns() {
    let (core, ctx, _g) = setup();
    // GROUP BY を伴う集計の WHERE: 真になる a1 (t 'x')・a2 (t 'y') が 1 件ずつ。
    let r = core
        .execute_sql(
            &ctx,
            "SELECT t, COUNT(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) GROUP BY t",
        )
        .expect("group by");
    let mut groups: Vec<(Cell, Cell)> = r
        .rows
        .iter()
        .map(|row| (row.cells[0].clone(), row.cells[1].clone()))
        .collect();
    groups.sort_by_key(|(t, _)| format!("{t:?}"));
    assert_eq!(
        groups,
        vec![
            (Cell::Text("x".to_string()), Cell::Integer(1)),
            (Cell::Text("y".to_string()), Cell::Integer(1)),
        ]
    );
    // 外側の `t` に二次索引があっても、索引で絞った候補に対してプローブの外側キー `k` が
    // デコードされる（a1 は t='x' かつ k=1 で真、a4 は t='x' だが k が NULL で偽）。
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE INDEX idx_a_t ON a (t)")
        .expect("create index");
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE t = 'x' AND EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1]
    );
}

#[test]
fn sql61_deferred_error_path_validates_correlated_exists_statically() {
    // 先行する述語がスカラーサブクエリの基数違反（21000）で解決を打ち切ったとき、後続の相関 EXISTS は
    // 実行せずに静的検証される。静的に問題が無ければ元の 21000 を返し、静的な誤りがあればそれを
    // 優先して返す（相関を一律 42601 にしない）。
    let (core, ctx, _g) = setup();
    let ok = "SELECT id FROM a WHERE k IN (SELECT aid FROM b WHERE v = (SELECT v FROM d LIMIT 2) \
              AND EXISTS (SELECT 1 FROM c WHERE c.aid = b.aid)) LIMIT 100";
    assert_eq!(err(&core, &ctx, ok).0, "21000");
    let bad = "SELECT id FROM a WHERE k IN (SELECT aid FROM b WHERE v = (SELECT v FROM d LIMIT 2) \
               AND EXISTS (SELECT 1 FROM c WHERE c.aid = b.aid AND c.zz = 1)) LIMIT 100";
    assert_eq!(err(&core, &ctx, bad).0, "42703");
    // 否定形の相関（Issue #1843）も肯定形と同じ静的検証を共有する。
    let negated =
        "SELECT id FROM a WHERE k IN (SELECT aid FROM b WHERE v = (SELECT v FROM d LIMIT 2) \
                   AND NOT EXISTS (SELECT 1 FROM c WHERE c.aid = b.aid)) LIMIT 100";
    assert_eq!(err(&core, &ctx, negated).0, "21000");
    let negated_bad =
        "SELECT id FROM a WHERE k IN (SELECT aid FROM b WHERE v = (SELECT v FROM d LIMIT 2) \
                   AND NOT EXISTS (SELECT 1 FROM c WHERE c.aid = b.aid AND c.zz = 1)) LIMIT 100";
    assert_eq!(err(&core, &ctx, negated_bad).0, "42703");
}

/// `SELECT id FROM a WHERE <pred> LIMIT 100` の `id` 集合。
fn a_where(core: &EngineCore, ctx: &PolicyContext, pred: &str) -> Vec<u64> {
    ids(
        core,
        ctx,
        &format!("SELECT id FROM a WHERE {pred} LIMIT 100"),
    )
}

#[test]
fn sql61_not_exists_anti_join_matches_postgres_null_semantics() {
    let (core, ctx, _g) = setup();
    // 外側キー k が NULL の a4 は EXISTS が偽（`=` は UNKNOWN）なので NOT EXISTS は真。
    // a3 (k=3) は b.aid に 3 が無い。b4 の NULL aid はどの外側とも一致しない。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)"
        ),
        vec![3, 4]
    );
    // 疑似列 id をキーにする: b.aid の集合は {1, 2}。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.id)"
        ),
        vec![3, 4]
    );
    // 内側だけの条件: flag = false は b2 (aid 1) のみ。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.flag = false)"
        ),
        vec![2, 3, 4]
    );
}

#[test]
fn sql61_not_exists_reevaluation_and_offset_inversion() {
    let (core, ctx, _g) = setup();
    // 残余のみ: 肯定形 [1, 2, 4] の補集合。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.v > a.v + 25)"
        ),
        vec![3]
    );
    // キー ＋ 残余: 肯定形 [1] の補集合。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.v > a.v)"
        ),
        vec![2, 3, 4]
    );
    // OR 混在: a4 は (NULL OR 偽) が UNKNOWN で一致に数えない。肯定形 [1, 2] の補集合。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE (b.aid = a.k OR b.t = 'none'))"
        ),
        vec![3, 4]
    );
    // キーにできない型 (DOUBLE): 肯定形 [1, 2] の補集合（d が NULL の a3・a4 は一致なし）。
    assert_eq!(
        a_where(&core, &ctx, "NOT EXISTS (SELECT 1 FROM b WHERE b.d = a.d)"),
        vec![3, 4]
    );
    // OFFSET 1: 一致が 1 件以下で真。aid=1 は 2 件、aid=2 は 1 件。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k LIMIT 5 OFFSET 1)"
        ),
        vec![2, 3, 4]
    );
    // キー無しの再評価: b.v > a.v の件数は a1: 2、a2: 1、a3: 1、a4: 2。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.v > a.v LIMIT 5 OFFSET 1)"
        ),
        vec![2, 3]
    );
}

#[test]
fn sql61_not_exists_composes_with_or_demorgan_aggregate_order_limit() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "a.t = 'y' OR NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)"
        ),
        vec![2, 3, 4]
    );
    // De Morgan: NOT EXISTS OR t <> 'x'。a1 のみ偽。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT (EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) AND t = 'x')"
        ),
        vec![2, 3, 4]
    );
    let r = core
        .execute_sql(
            &ctx,
            "SELECT COUNT(*) FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)",
        )
        .expect("count");
    assert_eq!(r.rows[0].cells[0], Cell::Integer(2));
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) \
             ORDER BY v DESC LIMIT 1"
        ),
        vec![3]
    );
    // 入れ子: a1 の b1・b2 は c が有る、a2 の b3 (aid 2) には c が無い。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "EXISTS (SELECT 1 FROM b WHERE b.aid = a.k \
             AND NOT EXISTS (SELECT 1 FROM c WHERE c.aid = b.aid))"
        ),
        vec![2]
    );
    // 内側 0 行: 外側キーが NULL の行も含めて全行が真。
    assert_eq!(
        a_where(
            &core,
            &ctx,
            "NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.t = 'none')"
        ),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn sql61_not_exists_does_not_observe_other_tenants_rows() {
    // RLS-10: t2 にだけ b.aid = 3 がある。t1 の a3 は除外されない（他テナントの行は反結合の
    // 除外条件に混ざらない）。
    let (core, ctx, _g) = setup();
    let t2 = ctx_for("t2");
    ins(&core, &t2, "a", 100, &[("k", "100")]);
    ins(&core, &t2, "b", 100, &[("aid", "3")]);
    ins(&core, &t2, "b", 101, &[("aid", "100")]);
    let sql = "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.aid = a.k) LIMIT 100";
    assert_eq!(ids(&core, &ctx, sql), vec![3, 4]);
    // t2 は自テナントの行だけで判定する（a100 は b101 と一致して除外される）。
    assert_eq!(ids(&core, &t2, sql), Vec::<u64>::new());
    let sql2 = "SELECT id FROM a WHERE NOT EXISTS \
                (SELECT 1 FROM b WHERE b.aid = a.k AND b.v > a.v - 1000) LIMIT 100";
    assert_eq!(ids(&core, &ctx, sql2), vec![3, 4]);
}
