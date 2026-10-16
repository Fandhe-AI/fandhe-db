//! ICU 照合（`und-x-icu`・`ja-x-icu`）の比較・`ORDER BY`・`GROUP BY`・`DISTINCT`・`MIN`／`MAX`・
//! 照合の導出と衝突（`42P21`／`42P22`）の結合テスト（TABLE-28、Issue #2190）。
//!
//! 期待順序は PostgreSQL 17.11（ICU 有効ビルド、`postgres:17` イメージ）で採取した固定値:
//! `SELECT string_agg(quote_literal(v), ',' ORDER BY v COLLATE "und-x-icu") FROM p`
//! （`ja-x-icu` も同様）。PG は ICU4C、本実装は ICU4X（`icu_collator`）で、CLDR のバージョン差が
//! 出うるため、値プールは版差の出にくい基本の文字（ASCII の大小・数字・空白・記号・アクセント・
//! かな／カナ・結合文字の NFC／NFD・空文字）を中心にしている。
//!
//! 照合ごとの列（`iu` `und-x-icu`・`iv` `VARCHAR`・`ib` `CHAR`・`ij` `ja-x-icu`・`t0` 指定なし・
//! `tc` `C`・`tp` `POSIX`）に同じ値を入れ、索引経路・全走査・素の形の一致、他テナントの
//! Private 行の非漏えい（RLS）、ICU 照合の結線のない経路が `0A000` で拒否されること
//! （意味論の fail-open がないこと）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ROWS: u64 = 62;
const OTHER_TENANT_ID: u64 = 900;
const CHAR_WIDTH: usize = 8;

/// `und-x-icu` の昇順（PG 17.11 で採取。`e\u{301}`〔NFD〕と `\u{e9}`〔NFC〕は ICU 上で等しく、
/// deterministic collation のバイト列順で NFD が先）。
const UND_ORDER: [&str; 31] = [
    "", "_", "1", "10", "2", "a", "A", "a b", "a-b", "ab", "aB", "Ab", "AB", "b", "B", "e", "E",
    "e\u{301}", "\u{e9}", "\u{c9}", "z", "\u{3042}", "\u{30a2}", "\u{3044}", "\u{30a4}",
    "\u{304b}", "\u{30ab}", "\u{304c}", "\u{30ac}", "\u{5b57}", "\u{6f22}",
];

/// `ja-x-icu` の昇順（PG 17.11 で採取。`und-x-icu` との差は漢字の順序）。
const JA_ORDER: [&str; 31] = [
    "", "_", "1", "10", "2", "a", "A", "a b", "a-b", "ab", "aB", "Ab", "AB", "b", "B", "e", "E",
    "e\u{301}", "\u{e9}", "\u{c9}", "z", "\u{3042}", "\u{30a2}", "\u{3044}", "\u{30a4}",
    "\u{304b}", "\u{30ab}", "\u{304c}", "\u{30ac}", "\u{6f22}", "\u{5b57}",
];

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType, c: Option<Collation>| ColumnDef {
        collation: c,
        ..ColumnDef::new(name, ty, true)
    };
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("iu", ColumnType::Text, Some(Collation::UndIcu)),
            col("iv", ColumnType::Varchar(Some(40)), Some(Collation::UndIcu)),
            col(
                "ib",
                ColumnType::Bpchar(CHAR_WIDTH as u32),
                Some(Collation::UndIcu),
            ),
            col("ij", ColumnType::Text, Some(Collation::JaIcu)),
            col("t0", ColumnType::Text, None),
            col("tc", ColumnType::Text, Some(Collation::C)),
            col("tp", ColumnType::Text, Some(Collation::Posix)),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id % 13 == 0` は NULL、それ以外は値プール（31 値）から 11 の倍数で巡回して選ぶ
/// （31 と 11 は互いに素なので 62 行で各値がちょうど 2 回現れる）。
fn value_of(id: u64) -> Option<&'static str> {
    if id.is_multiple_of(13) {
        None
    } else {
        UND_ORDER.get(((id * 11) % 31) as usize).copied()
    }
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, v: Option<&str>, vis: Visibility) {
    let text = |pad: bool| match v {
        Some(s) if pad => Value::Text(format!("{s:<CHAR_WIDTH$}")),
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    };
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            text(false),
            text(false),
            text(true),
            text(false),
            text(false),
            text(false),
            text(false),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_core_at(path: &std::path::Path, seed: bool) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    if seed {
        storage.create_table(&schema()).expect("create table");
        for id in 1..=ROWS {
            insert_row(&storage, "tenant-a", id, value_of(id), Visibility::Public);
        }
        // 他テナントの Private 行（どの述語にも合い、ICU 順で先頭に来る空文字）。
        insert_row(
            &storage,
            "tenant-b",
            OTHER_TENANT_ID,
            Some(""),
            Visibility::Private,
        );
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn err_of(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    err_of(core, sql).wire_code()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

fn ordered_ids(result: &QueryResult) -> Vec<u64> {
    assert!(
        result.rows.iter().all(|r| r.id != OTHER_TENANT_ID),
        "other tenant row leaked"
    );
    result.rows.iter().map(|r| r.id).collect()
}

fn sorted_ids(result: &QueryResult) -> Vec<u64> {
    let mut v = ordered_ids(result);
    v.sort_unstable();
    v
}

fn text_of(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Text(s) => Some(s.trim_end_matches(' ').to_string()),
        other => panic!("unexpected cell: {other:?}"),
    }
}

/// 値の順位（照合の昇順配列内の位置）。
fn rank(order: &[&str], v: &str) -> usize {
    order
        .iter()
        .position(|x| *x == v)
        .unwrap_or_else(|| panic!("value {v:?} not in the order array"))
}

/// 照合順の昇順（NULL は最後）／降順（NULL は最初）、同値は id 昇順。
fn oracle_order(order: &[&str], desc: bool) -> Vec<u64> {
    let mut rows: Vec<(Option<usize>, u64)> = (1..=ROWS)
        .map(|id| (value_of(id).map(|v| rank(order, v)), id))
        .collect();
    rows.sort_by(|a, b| {
        let key = |x: &Option<usize>| match x {
            Some(r) => (0u8, *r),
            None => (1u8, 0),
        };
        let ord = key(&a.0).cmp(&key(&b.0));
        let ord = if desc { ord.reverse() } else { ord };
        ord.then(a.1.cmp(&b.1))
    });
    rows.into_iter().map(|(_, id)| id).collect()
}

/// バイト列順の昇順（NULL は最後）／降順（NULL は最初）、同値は id 昇順。
fn oracle_byte_order(desc: bool) -> Vec<u64> {
    let mut rows: Vec<(Option<&str>, u64)> = (1..=ROWS).map(|id| (value_of(id), id)).collect();
    rows.sort_by(|a, b| {
        let key = |x: &Option<&str>| match x {
            Some(s) => (0u8, s.as_bytes().to_vec()),
            None => (1u8, Vec::new()),
        };
        let ord = key(&a.0).cmp(&key(&b.0));
        let ord = if desc { ord.reverse() } else { ord };
        ord.then(a.1.cmp(&b.1))
    });
    rows.into_iter().map(|(_, id)| id).collect()
}

fn ids_where(pred: impl Fn(&str) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| value_of(*id).is_some_and(&pred))
        .collect()
}

/// TABLE-28: ICU 照合の列の ORDER BY が PG 17 と同じ順序（昇順・降順・NULL 位置・LIMIT／OFFSET
/// のヒープ経路）になる。既定列（照合指定なし）と `C` 列はバイト列順のまま。
#[test]
fn order_by_icu_columns_matches_pg17_order() {
    let path = unique_db_path("sql-collate-icu-order");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    let cases: [(&str, &[&str]); 4] = [
        ("iu", &UND_ORDER),
        ("iv", &UND_ORDER),
        ("ib", &UND_ORDER),
        ("ij", &JA_ORDER),
    ];
    for (col, order) in cases {
        for (desc, dir) in [(false, "ASC"), (true, "DESC")] {
            let expected = oracle_order(order, desc);
            let got = ordered_ids(&run(
                &core,
                &format!("SELECT id FROM {TABLE} ORDER BY {col} {dir}, id LIMIT 200"),
            ));
            assert_eq!(got, expected, "ORDER BY {col} {dir}");
            // 暗黙の id 昇順の同順位解消（`, id` なし）と LIMIT／OFFSET（ヒープ経路）。
            let limited = ordered_ids(&run(
                &core,
                &format!("SELECT id FROM {TABLE} ORDER BY {col} {dir} LIMIT 9 OFFSET 4"),
            ));
            assert_eq!(
                limited,
                expected[4..13].to_vec(),
                "{col} {dir} LIMIT/OFFSET"
            );
        }
    }
    for col in ["t0", "tc", "tp"] {
        for (desc, dir) in [(false, "ASC"), (true, "DESC")] {
            let got = ordered_ids(&run(
                &core,
                &format!("SELECT id FROM {TABLE} ORDER BY {col} {dir}, id LIMIT 200"),
            ));
            assert_eq!(got, oracle_byte_order(desc), "byte order {col} {dir}");
        }
    }
}

/// TABLE-28: 式の `COLLATE`（最上位）が ORDER BY に効く。列の照合を上書きでき、式キーにも使える。
#[test]
fn order_by_expression_collate() {
    let path = unique_db_path("sql-collate-icu-expr");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    let by = |expr: &str, dir: &str| {
        ordered_ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} ORDER BY {expr} {dir}, id LIMIT 200"),
        ))
    };
    assert_eq!(
        by("t0 COLLATE \"und-x-icu\"", "ASC"),
        oracle_order(&UND_ORDER, false)
    );
    assert_eq!(
        by("tc COLLATE \"ja-x-icu\"", "DESC"),
        oracle_order(&JA_ORDER, true)
    );
    assert_eq!(
        by("iu COLLATE \"ja-x-icu\"", "ASC"),
        oracle_order(&JA_ORDER, false)
    );
    // 明示の `C` は ICU 列の照合を上書きしてバイト列順になる。
    assert_eq!(by("iu COLLATE \"C\"", "ASC"), oracle_byte_order(false));
    // 式キー（`substr` は値を変えない）。
    assert_eq!(
        by("substr(t0, 1) COLLATE \"und-x-icu\"", "ASC"),
        oracle_order(&UND_ORDER, false)
    );
}

fn assert_parity(core: &EngineCore, pred: &str, expected: &[u64]) {
    let vec_sql =
        format!("SELECT id FROM {TABLE} WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200");
    assert_eq!(sorted_ids(&run(core, &vec_sql)), expected, "oracle: {pred}");
    let scan_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred} AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    assert_eq!(
        sorted_ids(&run(core, &scan_sql)),
        expected,
        "full scan: {pred}"
    );
    let plain_sql = format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 200");
    assert_eq!(
        sorted_ids(&run(core, &plain_sql)),
        expected,
        "plain: {pred}"
    );
}

/// TABLE-28: WHERE の比較が列の照合・明示 `COLLATE` に従う。列はバイト列順で比較だけ ICU の形
/// （索引を使わず全走査）も含め、索引の有無で結果が一致する。
#[test]
fn where_comparisons_follow_collation() {
    let path = unique_db_path("sql-collate-icu-where");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    let b_und = rank(&UND_ORDER, "b");
    let a_und = rank(&UND_ORDER, "A");
    for col in ["iu", "iv", "ib"] {
        assert_parity(
            &core,
            &format!("{col} < 'b'"),
            &ids_where(|v| rank(&UND_ORDER, v) < b_und),
        );
        assert_parity(
            &core,
            &format!("{col} >= 'b'"),
            &ids_where(|v| rank(&UND_ORDER, v) >= b_und),
        );
        assert_parity(
            &core,
            &format!("{col} > 'A'"),
            &ids_where(|v| rank(&UND_ORDER, v) > a_und),
        );
        assert_parity(
            &core,
            &format!("'b' > {col}"),
            &ids_where(|v| rank(&UND_ORDER, v) < b_und),
        );
        assert_parity(
            &core,
            &format!("NOT ({col} < 'b')"),
            &ids_where(|v| rank(&UND_ORDER, v) >= b_und),
        );
        assert_parity(&core, &format!("{col} = 'a'"), &ids_where(|v| v == "a"));
    }
    // 列は日本語照合。
    let b_ja = rank(&JA_ORDER, "b");
    assert_parity(
        &core,
        "ij <= 'b'",
        &ids_where(|v| rank(&JA_ORDER, v) <= b_ja),
    );
    // 列はバイト列順（照合なし／`C`）で、比較だけ ICU（`COLLATE`）。
    assert_parity(
        &core,
        "t0 COLLATE \"und-x-icu\" < 'b'",
        &ids_where(|v| rank(&UND_ORDER, v) < b_und),
    );
    assert_parity(
        &core,
        "tc COLLATE \"ja-x-icu\" >= 'b'",
        &ids_where(|v| rank(&JA_ORDER, v) >= b_ja),
    );
    assert_parity(
        &core,
        "'b' COLLATE \"und-x-icu\" > t0",
        &ids_where(|v| rank(&UND_ORDER, v) < b_und),
    );
    // 明示の `C` は ICU 列の照合を上書きする（バイト列順）。
    assert_parity(
        &core,
        "iu COLLATE \"C\" < 'b'",
        &ids_where(|v| v.as_bytes() < "b".as_bytes()),
    );
    // 同じ値を持つ列同士（照合なし×ICU は ICU が勝つ）。
    assert_parity(&core, "iu <= t0", &ids_where(|_| true));
    assert_parity(&core, "iu < t0", &ids_where(|_| false));
    // 既定列は従来どおりバイト列順。
    assert_parity(
        &core,
        "t0 < 'b'",
        &ids_where(|v| v.as_bytes() < "b".as_bytes()),
    );
}

/// TABLE-28: 照合の導出と衝突。明示同士の不一致は `42P21`、暗黙同士の衝突を比較が消費したら
/// `42P22`。明示は暗黙に、暗黙は既定に勝つ。`C` と `POSIX` は別の照合。
#[test]
fn derivation_and_conflicts() {
    let path = unique_db_path("sql-collate-icu-derive");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    let q = |pred: &str| format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 200");
    // 暗黙同士の衝突（42P22）。
    for pred in [
        "iu < ij",
        "iu = ij",
        "iu < tc",
        "ib > tp",
        "tc < tp",
        "tc = tp",
        "iu < iv AND iu < tc",
    ] {
        assert_eq!(code_of(&core, &q(pred)), "42P22", "{pred}");
    }
    // 明示同士の不一致（42P21）。
    for pred in [
        "iu COLLATE \"C\" < ij COLLATE \"ja-x-icu\"",
        "t0 COLLATE \"und-x-icu\" = 'a' COLLATE \"ja-x-icu\"",
        "tc COLLATE \"C\" = tp COLLATE \"POSIX\"",
    ] {
        assert_eq!(code_of(&core, &q(pred)), "42P21", "{pred}");
    }
    // 明示は暗黙に勝つ・同じ暗黙は衝突しない・暗黙は既定に勝つ。
    for pred in [
        "iu COLLATE \"und-x-icu\" <= ij",
        "iu <= ij COLLATE \"und-x-icu\"",
        "iu <= iv",
        "tc <= t0",
        "tp <= t0",
        "iu COLLATE \"C\" <= tc",
    ] {
        assert_eq!(
            sorted_ids(&run(&core, &q(pred))),
            ids_where(|_| true),
            "{pred}"
        );
    }
    // 衝突は空の結果でも束縛時に返す（データに依存しない fail-closed）。
    assert_eq!(code_of(&core, &q("iu < ij AND id < 0")), "42P22");
}

/// TABLE-28: GROUP BY・DISTINCT のキー順序と MIN／MAX が照合に従う。グループの同一性は
/// バイト列の等値（deterministic）。
#[test]
fn group_by_distinct_min_max_follow_collation() {
    let path = unique_db_path("sql-collate-icu-group");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    let present: Vec<&str> = (1..=ROWS).filter_map(value_of).collect();
    let mut want_keys: Vec<&str> = UND_ORDER
        .iter()
        .copied()
        .filter(|v| present.contains(v))
        .collect();
    let mut want_counts: Vec<u64> = want_keys
        .iter()
        .map(|k| present.iter().filter(|p| *p == k).count() as u64)
        .collect();
    // GROUP BY + ORDER BY（グループキーの照合順）。NULL グループは末尾。
    let res = run(
        &core,
        &format!("SELECT iu, COUNT(*) FROM {TABLE} GROUP BY iu ORDER BY iu"),
    );
    let got: Vec<(Option<String>, u64)> = res
        .rows
        .iter()
        .map(|r| {
            (
                text_of(&r.cells[0]),
                match &r.cells[1] {
                    Cell::Integer(n) => *n,
                    other => panic!("count cell: {other:?}"),
                },
            )
        })
        .collect();
    let mut want: Vec<(Option<String>, u64)> = want_keys
        .iter()
        .zip(&want_counts)
        .map(|(k, c)| (Some((*k).to_string()), *c))
        .collect();
    want.push((None, 4));
    assert_eq!(got, want, "GROUP BY iu ORDER BY iu");
    // 降順。
    let res = run(
        &core,
        &format!("SELECT iu, COUNT(*) FROM {TABLE} GROUP BY iu ORDER BY iu DESC"),
    );
    let got_keys: Vec<Option<String>> = res.rows.iter().map(|r| text_of(&r.cells[0])).collect();
    let mut want_desc: Vec<Option<String>> = vec![None];
    want_desc.extend(want_keys.iter().rev().map(|k| Some((*k).to_string())));
    assert_eq!(got_keys, want_desc, "GROUP BY iu ORDER BY iu DESC");
    // 日本語照合の列は漢字の順序が異なる。
    let res = run(
        &core,
        &format!("SELECT ij, COUNT(*) FROM {TABLE} GROUP BY ij ORDER BY ij"),
    );
    let ja_keys: Vec<Option<String>> = res.rows.iter().map(|r| text_of(&r.cells[0])).collect();
    let mut want_ja: Vec<Option<String>> = JA_ORDER
        .iter()
        .filter(|v| present.contains(v))
        .map(|v| Some((*v).to_string()))
        .collect();
    want_ja.push(None);
    assert_eq!(ja_keys, want_ja, "GROUP BY ij ORDER BY ij");
    // DISTINCT + ORDER BY。
    let res = run(
        &core,
        &format!("SELECT DISTINCT iu FROM {TABLE} ORDER BY iu"),
    );
    let got: Vec<Option<String>> = res.rows.iter().map(|r| text_of(&r.cells[0])).collect();
    let mut want_distinct: Vec<Option<String>> =
        want_keys.iter().map(|k| Some((*k).to_string())).collect();
    want_distinct.push(None);
    assert_eq!(got, want_distinct, "DISTINCT iu ORDER BY iu");
    // MIN／MAX。列の照合・`COLLATE` の明示・照合なし。
    let minmax = |expr: &str| {
        let res = run(
            &core,
            &format!("SELECT MIN({expr}), MAX({expr}) FROM {TABLE}"),
        );
        let row = res.rows.first().expect("one row").clone();
        (text_of(&row.cells[0]), text_of(&row.cells[1]))
    };
    assert_eq!(
        minmax("iu"),
        (Some(String::new()), Some("\u{6f22}".to_string()))
    );
    assert_eq!(
        minmax("ij"),
        (Some(String::new()), Some("\u{5b57}".to_string()))
    );
    assert_eq!(
        minmax("iv"),
        (Some(String::new()), Some("\u{6f22}".to_string()))
    );
    assert_eq!(
        minmax("t0 COLLATE \"ja-x-icu\""),
        (Some(String::new()), Some("\u{5b57}".to_string()))
    );
    // バイト列順では最大が `\u{5b57}`（U+5B57 < U+6F22 だが、かな〔U+30xx〕より後ろ）。
    assert_eq!(
        minmax("t0"),
        (Some(String::new()), Some("\u{6f22}".to_string()))
    );
    assert_eq!(
        minmax("iu COLLATE \"C\""),
        (Some(String::new()), Some("\u{6f22}".to_string()))
    );
    // `ORDER BY` のキーは `iu` の照合順で並ぶことを確認済み。余りの変数を使い切る。
    want_keys.clear();
    want_counts.clear();
}

/// TABLE-28: ICU 照合の結線のない経路は黙ってバイト列順で比較せず `0A000` で拒否する
/// （意味論の fail-open がないこと）。
#[test]
fn unwired_paths_are_rejected_fail_closed() {
    let path = unique_db_path("sql-collate-icu-failclosed");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    ddl(
        &core,
        "CREATE VIEW vb AS SELECT iu, t0, id FROM docs ORDER BY id LIMIT 50",
    );
    let cases = [
        // ウィンドウ関数の PARTITION BY／ORDER BY・MIN／MAX。
        format!("SELECT id, ROW_NUMBER() OVER (ORDER BY iu) FROM {TABLE} LIMIT 100"),
        format!(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY iu ORDER BY id) FROM {TABLE} LIMIT 100"
        ),
        format!("SELECT id, MIN(iu) OVER () FROM {TABLE} LIMIT 100"),
        // 集計結果の ORDER BY（MIN／MAX は項目の照合を運ばない）。
        format!("SELECT ij, MIN(iu) AS m FROM {TABLE} GROUP BY ij ORDER BY m"),
        format!("SELECT ij, MAX(ij) AS m FROM {TABLE} GROUP BY ij ORDER BY m DESC"),
        // ICU の列を含む式（照合が式へ運ばれない）。
        format!("SELECT id FROM {TABLE} WHERE lower(iu) < 'b' LIMIT 100"),
        format!("SELECT id FROM {TABLE} ORDER BY lower(iu), id LIMIT 100"),
        format!("SELECT id FROM {TABLE} ORDER BY substr(iu, 1), id LIMIT 100"),
        format!("SELECT id FROM {TABLE} ORDER BY substr(iu COLLATE \"C\", 1), id LIMIT 100"),
        format!("SELECT lower(iu COLLATE \"C\") FROM {TABLE} LIMIT 100"),
        format!("SELECT iu COLLATE \"und-x-icu\" FROM {TABLE} LIMIT 100"),
        format!("SELECT iu || 'x' FROM {TABLE} LIMIT 100"),
        // JOIN は照合を運ばないため、ICU の列の参照を拒否する。
        format!("SELECT a.iu FROM {TABLE} a JOIN {TABLE} b ON a.id = b.id LIMIT 10"),
        format!("SELECT a.id FROM {TABLE} a JOIN {TABLE} b ON a.iu = b.iu LIMIT 10"),
        format!("SELECT a.id FROM {TABLE} a JOIN {TABLE} b ON a.id = b.id ORDER BY a.iu LIMIT 10"),
        // 評価後射影形ビュー（本文に ORDER BY／LIMIT）の外側の並べ替え・絞り込み・集計。
        "SELECT iu FROM vb ORDER BY iu LIMIT 5".to_string(),
        "SELECT iu FROM vb WHERE iu < 'b' LIMIT 5".to_string(),
        "SELECT iu, COUNT(*) AS c FROM vb GROUP BY iu LIMIT 5".to_string(),
        // 照合を持つ列を含む CHECK は永続化しない（照合データの版に結果が依存するため）。
    ];
    for sql in &cases {
        assert_eq!(code_of(&core, sql), "0A000", "{sql}");
    }
    // 結線のある・照合に依存しない形は通る（素通しのビュー、照合なしの列だけの JOIN）。
    run(&core, "SELECT iu FROM vb LIMIT 5");
    run(
        &core,
        &format!("SELECT a.t0 FROM {TABLE} a JOIN {TABLE} b ON a.id = b.id ORDER BY a.t0 LIMIT 3"),
    );
    // 単一リレーションに閉じる WHERE の葉は側スキャンへ押し下げられ、照合に従って絞り込まれる。
    let b = rank(&UND_ORDER, "b");
    let pushed = run(
        &core,
        &format!(
            "SELECT a.id FROM {TABLE} a JOIN {TABLE} b ON a.id = b.id WHERE a.iu < 'b' LIMIT 100"
        ),
    );
    assert_eq!(
        sorted_ids(&pushed),
        ids_where(|v| rank(&UND_ORDER, v) < b),
        "pushed-down leaf in JOIN WHERE"
    );
    // CHECK。
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "CREATE TABLE ck (x TEXT COLLATE \"und-x-icu\", CHECK (x < 'b'))",
        )
        .expect_err("CHECK with an ICU comparison must be rejected");
    // CHECK の TEXT 範囲比較はそもそも受理しない（既存挙動。`22000`）。ICU の列でも変わらない。
    assert_eq!(err.wire_code(), "22000");
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "CREATE TABLE ck2 (x TEXT COLLATE \"und-x-icu\", CHECK (x COLLATE \"C\" = 'a'))",
        )
        .expect_err("COLLATE in CHECK must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}

/// TABLE-28: OR・NOT を含む WHERE と、DML の WHERE が照合に従う。
#[test]
fn boolean_combinations_and_dml_follow_collation() {
    let path = unique_db_path("sql-collate-icu-bool");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    let b = rank(&UND_ORDER, "b");
    let z = rank(&UND_ORDER, "z");
    assert_parity(
        &core,
        "iu < 'b' OR iu > 'z'",
        &ids_where(|v| rank(&UND_ORDER, v) < b || rank(&UND_ORDER, v) > z),
    );
    assert_parity(
        &core,
        "NOT (iu < 'b' OR iu > 'z')",
        &ids_where(|v| rank(&UND_ORDER, v) >= b && rank(&UND_ORDER, v) <= z),
    );
    assert_parity(
        &core,
        "iu >= 'b' AND ij < 'z'",
        &ids_where(|v| rank(&UND_ORDER, v) >= b && rank(&JA_ORDER, v) < rank(&JA_ORDER, "z")),
    );
    // DELETE の WHERE。
    let mut session = SessionState::default();
    let out = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "DELETE FROM docs WHERE iu < 'b' USING OPERATION_ID 'del-icu-1'",
        )
        .expect("delete with ICU comparison");
    let deleted = match out {
        engine::sql::SqlOutcome::Delete(n) => n,
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert_eq!(deleted.rows_affected, {
        ids_where(|v| rank(&UND_ORDER, v) < b).len() as u64
    });
    assert_eq!(
        sorted_ids(&run(&core, &format!("SELECT id FROM {TABLE} LIMIT 200"))),
        ids_where(|v| rank(&UND_ORDER, v) >= b)
            .into_iter()
            .chain((1..=ROWS).filter(|id| value_of(*id).is_none()))
            .collect::<std::collections::BTreeSet<u64>>()
            .into_iter()
            .collect::<Vec<u64>>()
    );
}

/// TABLE-28: `ALTER COLUMN TYPE ... COLLATE` で ICU 照合へ変更した直後と、再起動後に順序が
/// 保たれる。カタログだけの書き換えで、キャッシュは世代で無効化される。
#[test]
fn alter_to_icu_collation_and_restart() {
    let path = unique_db_path("sql-collate-icu-alter");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core_at(&path, true);
        let by_t0 = |core: &EngineCore| {
            ordered_ids(&run(
                core,
                &format!("SELECT id FROM {TABLE} ORDER BY t0, id LIMIT 200"),
            ))
        };
        assert_eq!(by_t0(&core), oracle_byte_order(false));
        ddl(
            &core,
            "ALTER TABLE docs ALTER COLUMN t0 TYPE TEXT COLLATE \"und-x-icu\"",
        );
        assert_eq!(by_t0(&core), oracle_order(&UND_ORDER, false));
        ddl(
            &core,
            "ALTER TABLE docs ALTER COLUMN t0 TYPE TEXT COLLATE \"ja-x-icu\"",
        );
        assert_eq!(by_t0(&core), oracle_order(&JA_ORDER, false));
        ddl(
            &core,
            "ALTER TABLE docs ALTER COLUMN t0 TYPE TEXT COLLATE \"und-x-icu\"",
        );
        assert_eq!(by_t0(&core), oracle_order(&UND_ORDER, false));
    }
    // 再起動後も ICU 照合の列として復元される。
    let core = open_core_at(&path, false);
    assert_eq!(
        ordered_ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} ORDER BY t0, id LIMIT 200")
        )),
        oracle_order(&UND_ORDER, false)
    );
    assert_eq!(
        ordered_ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} ORDER BY iu DESC, id LIMIT 200")
        )),
        oracle_order(&UND_ORDER, true)
    );
    let b = rank(&UND_ORDER, "b");
    assert_parity(&core, "t0 < 'b'", &ids_where(|v| rank(&UND_ORDER, v) < b));
    // 照合を外すとバイト列順へ戻る。
    ddl(&core, "ALTER TABLE docs ALTER COLUMN t0 TYPE TEXT");
    assert_eq!(
        ordered_ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} ORDER BY t0, id LIMIT 200")
        )),
        oracle_byte_order(false)
    );
}

/// TABLE-28: `WITH`（CTE）越しでも照合は黙って落ちない。本文が素通しの形（`ORDER BY`／`LIMIT`
/// なし）なら基底テーブルへ展開されて ICU 順になり、本文に `LIMIT` を持つ形（評価後射影形）は
/// 受理されないか外側の並べ替えを `0A000` で拒否する（バイト列順で黙って並べない）。
#[test]
fn cte_paths_never_fall_back_to_byte_order_silently() {
    let path = unique_db_path("sql-collate-icu-cte");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    assert_eq!(
        ordered_ids(&run(
            &core,
            &format!(
                "WITH v AS (SELECT iu, id FROM {TABLE}) \
                 SELECT id FROM v ORDER BY iu, id LIMIT 200"
            ),
        )),
        oracle_order(&UND_ORDER, false)
    );
    // 本文に `ORDER BY`／`LIMIT` を持つ CTE は構文段が受理しない（`42601`）。評価後射影形として
    // 受理される経路があっても `0A000` になる。どちらでも結果をバイト列順で返さない。
    let code = code_of(
        &core,
        &format!(
            "WITH v AS (SELECT iu, id FROM {TABLE} ORDER BY id LIMIT 50) \
             SELECT iu FROM v ORDER BY iu LIMIT 5"
        ),
    );
    assert!(matches!(code, "0A000" | "42601"), "{code}");
}
