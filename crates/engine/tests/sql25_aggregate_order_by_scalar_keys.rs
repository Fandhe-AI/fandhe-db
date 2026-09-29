//! 集計文のスカラー `ORDER BY`（複数キー・グループキーと集計値の混在・PostgreSQL
//! 既定の NULL 位置）・複数列／非 `TEXT` の `SELECT DISTINCT`・非 `TEXT` 列と疑似列
//! `id` の `GROUP BY` キー（Issue #1185・SQL-25 (a)(c)(d)・TASK-209）の結合テスト。
//!
//! `tests/sql25_multi_group_by.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! 決定的な行生成、Rust 側の独立オラクルとの突き合わせ、`EngineCore::execute_sql`
//! の production 経路）で検証する。RLS 境界（他テナントの Private 行にしか存在しない
//! 非 `TEXT` キー値が結果・件数・並び順へ現れないこと）と `wire_code` のエラー契約
//! （`22000`／`42601`／`54000`）も固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

type LangN = (Option<&'static str>, Option<i32>);
type NMood = (Option<i32>, Option<&'static str>);

const TABLE: &str = "t";
const TENANTS: [&str; 2] = ["tenant-a", "tenant-b"];
const MOODS: [&str; 3] = ["happy", "sad", "neutral"];
const ROWS: u64 = 40;
/// tenant-a の Private 行にだけ存在する非 TEXT キー値（他テナントが見えてはならない）。
const SECRET_N: i32 = 999;
const SECRET_DAY: i32 = 9_999;
const SECRET_ID: u64 = 1_001;

fn schema(mood: std::sync::Arc<engine::catalog::EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 6,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("mood", ColumnType::Enum(mood), true),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("ts", ColumnType::Timestamp, true),
            ColumnDef::new("u", ColumnType::Uuid, true),
        ],
    )
}

/// オラクル用の行の真値（`insert_typed_row` へ渡す値と同じ生成規則から作る）。
#[derive(Clone, Debug)]
struct RowTruth {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: Option<&'static str>,
    n: Option<i32>,
    day: Option<i32>,
    mood: Option<&'static str>,
}

fn truth_for(id: u64) -> RowTruth {
    let tenant = TENANTS[usize::try_from(id % 2).expect("tiny")];
    let visibility = if id.is_multiple_of(5) {
        Visibility::Private
    } else {
        Visibility::Public
    };
    RowTruth {
        id,
        tenant,
        visibility,
        lang: match id % 3 {
            0 => Some("ja"),
            1 => Some("en"),
            _ => None,
        },
        n: if id.is_multiple_of(7) {
            None
        } else {
            Some(i32::try_from(id % 4).expect("tiny") - 1)
        },
        day: if id.is_multiple_of(11) {
            None
        } else {
            Some(i32::try_from(id % 3).expect("tiny") * 10)
        },
        mood: if id.is_multiple_of(9) {
            None
        } else {
            Some(MOODS[usize::try_from(id % 3).expect("tiny")])
        },
    }
}

fn all_truths() -> Vec<RowTruth> {
    let mut rows: Vec<RowTruth> = (1..=ROWS).map(truth_for).collect();
    // tenant-a の Private 行にしか存在しない値を持つ行（RLS 非漏えいの検証用）。
    rows.push(RowTruth {
        id: SECRET_ID,
        tenant: "tenant-a",
        visibility: Visibility::Private,
        lang: Some("secret"),
        n: Some(SECRET_N),
        day: Some(SECRET_DAY),
        mood: Some("sad"),
    });
    rows
}

fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
    v.map(f).unwrap_or(Value::Null)
}

fn values_for(t: &RowTruth) -> Vec<Value> {
    let id = t.id;
    let idn = i64::try_from(id).expect("tiny");
    vec![
        Value::Vector(vec![0.0, 0.0]),
        opt(t.lang, |s| Value::Text(s.to_string())),
        opt(t.n, Value::Integer),
        // BIGINT／REAL／DOUBLE／BOOLEAN／NUMERIC／TIMESTAMP／UUID は id の剰余で
        // 3 値＋NULL（id.is_multiple_of(13)）にする。
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::BigInt((idn % 3) * 5_000_000_000)
        },
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Real(f32::from(u8::try_from(id % 3).expect("tiny")) * 0.5)
        },
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Double(f64::from(u8::try_from(id % 3).expect("tiny")) + 0.25)
        },
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Bool(id.is_multiple_of(2))
        },
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Numeric(Decimal::from_parts(i128::from(id % 3) * 150, 2).expect("decimal"))
        },
        opt(t.mood, |m| Value::Enum(m.to_string())),
        opt(t.day, Value::Date),
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Timestamp((idn % 3) * 1_000_000)
        },
        if id.is_multiple_of(13) {
            Value::Null
        } else {
            Value::Uuid(Uuid::from_bytes(
                [u8::try_from(id % 3).expect("tiny") + 1; 16],
            ))
        },
    ]
}

fn build_core(tag: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    let mood = storage
        .create_enum_type(
            "mood",
            MOODS.iter().map(|m| (*m).to_string()).collect::<Vec<_>>(),
        )
        .expect("create enum type");
    storage.create_table(&schema(mood)).expect("create table");
    for truth in all_truths() {
        let ctx = PolicyContext::with_visibilities(
            truth.tenant,
            [Visibility::Public, Visibility::Private],
        )
        .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            truth.id,
            truth.visibility,
            &values_for(&truth),
            &OperationId::parse(&format!("op-{}", truth.id)).expect("valid operation id"),
        )
        .expect("insert row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// 可視集合（Public 全件＋自テナントの Private。`allow_private` が偽なら Private 除外）。
fn visible(truths: &[RowTruth], tenant: &str, allow_private: bool) -> Vec<RowTruth> {
    truths
        .iter()
        .filter(|t| match t.visibility {
            Visibility::Public => true,
            Visibility::Private => t.tenant == tenant && allow_private,
        })
        .cloned()
        .collect()
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn run_err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    core.execute_sql(ctx, sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn as_count(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn as_n(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::SignedInteger(v) => Some(*v),
        Cell::Null => None,
        other => panic!("expected Cell::SignedInteger or Null, got {other:?}"),
    }
}

fn as_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(v) => Some(v.clone()),
        Cell::Null => None,
        other => panic!("expected Cell::Text or Null, got {other:?}"),
    }
}

/// PostgreSQL 既定の NULL 位置（ASC は NULL 末尾・DESC は NULL 先頭）で 1 キーを比較する。
fn cmp_pg<T: Ord>(a: &Option<T>, b: &Option<T>, desc: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if desc {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if desc {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            if desc {
                y.cmp(x)
            } else {
                x.cmp(y)
            }
        }
    }
}

// --- スカラー ORDER BY（単一キー）: NULL 位置 --------------------------------------

#[test]
fn integer_group_key_orders_with_pg_default_null_positions() {
    let (core, path) = build_core("sql25-agg-order-int");
    let _guard = CleanupGuard(path);
    let truths = all_truths();
    let ctx = ctx_for("tenant-a", true);
    let rows = visible(&truths, "tenant-a", true);

    let mut expected: BTreeMap<Option<i32>, u64> = BTreeMap::new();
    for r in &rows {
        *expected.entry(r.n).or_default() += 1;
    }

    let asc = run(
        &core,
        &ctx,
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY n ASC",
    );
    let mut keys: Vec<Option<i32>> = expected.keys().copied().collect();
    keys.sort_by(|a, b| cmp_pg(a, b, false));
    assert_eq!(asc.rows.len(), keys.len());
    for (row, key) in asc.rows.iter().zip(&keys) {
        assert_eq!(as_n(&row.cells[0]), key.map(i64::from));
        assert_eq!(as_count(&row.cells[1]), expected[key]);
    }
    assert_eq!(as_n(&asc.rows.last().expect("rows").cells[0]), None);

    let desc = run(
        &core,
        &ctx,
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY n DESC",
    );
    keys.sort_by(|a, b| cmp_pg(a, b, true));
    assert_eq!(desc.rows.len(), keys.len());
    for (row, key) in desc.rows.iter().zip(&keys) {
        assert_eq!(as_n(&row.cells[0]), key.map(i64::from));
    }
    assert_eq!(as_n(&desc.rows[0].cells[0]), None, "DESC puts NULL first");

    // ORDER BY を書かない既定順は不変（キー昇順・NULL 末尾）。
    let default = run(&core, &ctx, "SELECT n, COUNT(*) AS c FROM t GROUP BY n");
    assert_eq!(default.rows, asc.rows);
}

// --- 複数キー ORDER BY: グループキーと集計値の混在・同値時はキー昇順 ----------------

#[test]
fn multi_key_order_by_mixes_group_keys_and_aggregates() {
    let (core, path) = build_core("sql25-agg-order-multi");
    let _guard = CleanupGuard(path);
    let truths = all_truths();
    let ctx = ctx_for("tenant-a", true);
    let rows = visible(&truths, "tenant-a", true);

    let mut counts: BTreeMap<(Option<&str>, Option<i32>), u64> = BTreeMap::new();
    for r in &rows {
        *counts.entry((r.lang, r.n)).or_default() += 1;
    }
    let mut expected: Vec<(LangN, u64)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
    expected.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| cmp_pg(&a.0 .0, &b.0 .0, false))
            .then_with(|| cmp_pg(&a.0 .1, &b.0 .1, true))
    });

    let sql = "SELECT lang, n, COUNT(*) AS c FROM t GROUP BY lang, n \
               ORDER BY c DESC, lang ASC, n DESC";
    let got = run(&core, &ctx, sql);
    assert_eq!(got.rows.len(), expected.len());
    for (row, ((lang, n), c)) in got.rows.iter().zip(&expected) {
        assert_eq!(as_text(&row.cells[0]).as_deref(), *lang);
        assert_eq!(as_n(&row.cells[1]), n.map(i64::from));
        assert_eq!(as_count(&row.cells[2]), *c);
    }

    // 決定性: 同じクエリは常に同一の結果を返す。
    for _ in 0..3 {
        assert_eq!(run(&core, &ctx, sql).rows, got.rows);
    }

    // LIMIT／OFFSET はソート確定後に適用される。
    let paged = run(
        &core,
        &ctx,
        "SELECT lang, n, COUNT(*) AS c FROM t GROUP BY lang, n \
         ORDER BY c DESC, lang ASC, n DESC LIMIT 3 OFFSET 1",
    );
    assert_eq!(paged.rows, got.rows[1..4].to_vec());
}

#[test]
fn ties_on_specified_keys_fall_back_to_ascending_group_key() {
    let (core, path) = build_core("sql25-agg-order-tie");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    // 全グループで定数の式は書けないため、集計値が同値になりやすい COUNT で並べ、
    // 同値グループは n の昇順（NULL 末尾）で決まることを確認する。
    let got = run(
        &core,
        &ctx,
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY c ASC",
    );
    let pairs: Vec<(u64, Option<i64>)> = got
        .rows
        .iter()
        .map(|r| (as_count(&r.cells[1]), as_n(&r.cells[0])))
        .collect();
    for w in pairs.windows(2) {
        let (c0, n0) = w[0];
        let (c1, n1) = w[1];
        assert!(c0 <= c1);
        if c0 == c1 {
            assert_ne!(cmp_pg(&n0, &n1, false), Ordering::Greater, "{pairs:?}");
        }
    }
}

#[test]
fn aggregate_order_by_key_count_limit_is_eight() {
    let (core, path) = build_core("sql25-agg-order-limit");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    let keys = |n: usize| vec!["c"; n].join(", ");
    run(
        &core,
        &ctx,
        &format!(
            "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY {}",
            keys(8)
        ),
    );
    assert_eq!(
        run_err(
            &core,
            &ctx,
            &format!(
                "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY {}",
                keys(9)
            )
        ),
        "54000"
    );
}

#[test]
fn aggregate_order_by_rejects_unsupported_forms_and_unknown_names() {
    let (core, path) = build_core("sql25-agg-order-reject");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    for sql in [
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY n NULLS LAST",
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY 1",
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY n, ",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
    assert_eq!(
        run_err(
            &core,
            &ctx,
            "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY c, nope"
        ),
        "22000"
    );
}

// --- GROUP BY キーの型一般化 ---------------------------------------------------------

/// 列ごとの可視行における異なり値数（NULL は 1 グループ）のオラクル。
fn distinct_groups(rows: &[RowTruth], column: &str) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for t in rows {
        let key: Option<String> = match column {
            "id" => Some(t.id.to_string()),
            "n" => t.n.map(|v| v.to_string()),
            "day" => t.day.map(|v| v.to_string()),
            "mood" => t.mood.map(str::to_string),
            "lang" => t.lang.map(str::to_string),
            "big" | "r" | "d" | "flag" | "price" | "ts" | "u" => {
                if t.id.is_multiple_of(13) {
                    None
                } else if column == "flag" {
                    Some((t.id.is_multiple_of(2)).to_string())
                } else {
                    Some((t.id % 3).to_string())
                }
            }
            other => panic!("unknown column {other}"),
        };
        seen.insert(key);
    }
    seen.len()
}

#[test]
fn every_orderable_column_type_and_id_work_as_group_by_key() {
    let (core, path) = build_core("sql25-agg-key-types");
    let _guard = CleanupGuard(path);
    let truths = all_truths();
    let ctx = ctx_for("tenant-a", true);
    let rows = visible(&truths, "tenant-a", true);
    let total = u64::try_from(rows.len()).expect("tiny");

    for column in [
        "n", "big", "r", "d", "flag", "price", "mood", "day", "ts", "u", "id",
    ] {
        let sql = format!("SELECT {column}, COUNT(*) AS c FROM t GROUP BY {column}");
        let got = run(&core, &ctx, &sql);
        assert_eq!(got.rows.len(), distinct_groups(&rows, column), "{column}");
        let sum: u64 = got.rows.iter().map(|r| as_count(&r.cells[1])).sum();
        assert_eq!(sum, total, "{column}");
        // 既定順序はキー昇順・NULL 末尾（NULL は高々 1 行）。
        let null_positions: Vec<usize> = got
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.cells[0], Cell::Null))
            .map(|(i, _)| i)
            .collect();
        assert!(
            null_positions.is_empty() || null_positions == vec![got.rows.len() - 1],
            "{column}: {null_positions:?}"
        );
        // 明示 DESC は NULL 先頭。
        let desc = run(&core, &ctx, &format!("{sql} ORDER BY {column} DESC"));
        assert_eq!(desc.rows.len(), got.rows.len(), "{column}");
        if !null_positions.is_empty() {
            assert!(matches!(desc.rows[0].cells[0], Cell::Null), "{column}");
        }
        // 索引スナップショットを試みない全走査経路でも冷・温で結果が一致する。
        assert_eq!(run(&core, &ctx, &sql).rows, got.rows, "{column} (hot)");
    }
}

#[test]
fn group_by_value_cells_use_the_column_type_representation() {
    let (core, path) = build_core("sql25-agg-key-cells");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    let cell_of = |col: &str| -> Cell {
        run(
            &core,
            &ctx,
            &format!("SELECT {col}, COUNT(*) FROM t WHERE id = 1 GROUP BY {col}"),
        )
        .rows
        .into_iter()
        .next()
        .expect("row")
        .cells
        .into_iter()
        .next()
        .expect("cell")
    };
    assert!(matches!(cell_of("n"), Cell::SignedInteger(_)));
    assert!(matches!(cell_of("big"), Cell::SignedInteger(_)));
    assert!(matches!(cell_of("r"), Cell::Float(_)));
    assert!(matches!(cell_of("d"), Cell::Float(_)));
    assert!(matches!(cell_of("flag"), Cell::Bool(_)));
    assert!(matches!(cell_of("price"), Cell::Numeric(_)));
    assert!(matches!(cell_of("mood"), Cell::Text(_)));
    assert!(matches!(cell_of("day"), Cell::Date(_)));
    assert!(matches!(cell_of("ts"), Cell::Timestamp(_)));
    assert!(matches!(cell_of("u"), Cell::Uuid(_)));
    assert_eq!(cell_of("id"), Cell::Integer(1));
}

#[test]
fn enum_group_keys_order_by_declaration_order_not_label_text() {
    let (core, path) = build_core("sql25-agg-key-enum");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    // 宣言順は happy < sad < neutral（文字列順なら happy < neutral < sad）。
    let got = run(
        &core,
        &ctx,
        "SELECT mood, COUNT(*) AS c FROM t GROUP BY mood ORDER BY mood",
    );
    let labels: Vec<Option<String>> = got.rows.iter().map(|r| as_text(&r.cells[0])).collect();
    assert_eq!(
        labels,
        vec![
            Some("happy".to_string()),
            Some("sad".to_string()),
            Some("neutral".to_string()),
            None
        ]
    );
}

#[test]
fn group_by_with_mixed_typed_keys_having_offset_and_limit() {
    let (core, path) = build_core("sql25-agg-key-mixed");
    let _guard = CleanupGuard(path);
    let truths = all_truths();
    let ctx = ctx_for("tenant-a", true);
    let rows = visible(&truths, "tenant-a", true);

    let mut counts: BTreeMap<(Option<i32>, Option<&str>), u64> = BTreeMap::new();
    for r in &rows {
        *counts.entry((r.n, r.mood)).or_default() += 1;
    }
    let mut expected: Vec<(NMood, u64)> = counts.into_iter().filter(|(_, c)| *c >= 2).collect();
    // 既定順: n 昇順（NULL 末尾）→ mood は宣言順（NULL 末尾）。
    let mood_ord = |m: &Option<&str>| m.map(|l| MOODS.iter().position(|x| *x == l).expect("label"));
    expected.sort_by(|a, b| {
        cmp_pg(&a.0 .0, &b.0 .0, false)
            .then_with(|| cmp_pg(&mood_ord(&a.0 .1), &mood_ord(&b.0 .1), false))
    });
    let got = run(
        &core,
        &ctx,
        "SELECT n, mood, COUNT(*) AS c FROM t GROUP BY n, mood HAVING c >= 2",
    );
    assert_eq!(got.rows.len(), expected.len());
    for (row, ((n, mood), c)) in got.rows.iter().zip(&expected) {
        assert_eq!(as_n(&row.cells[0]), n.map(i64::from));
        assert_eq!(as_text(&row.cells[1]).as_deref(), *mood);
        assert_eq!(as_count(&row.cells[2]), *c);
    }
    let paged = run(
        &core,
        &ctx,
        "SELECT n, mood, COUNT(*) AS c FROM t GROUP BY n, mood HAVING c >= 2 LIMIT 2 OFFSET 1",
    );
    assert_eq!(paged.rows, got.rows[1..3].to_vec());
}

#[test]
fn group_by_vector_column_and_unknown_column_stay_rejected() {
    let (core, path) = build_core("sql25-agg-key-reject");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    assert_eq!(
        run_err(
            &core,
            &ctx,
            "SELECT embedding, COUNT(*) FROM t GROUP BY embedding"
        ),
        "22000"
    );
    assert_eq!(
        run_err(&core, &ctx, "SELECT ghost, COUNT(*) FROM t GROUP BY ghost"),
        "22000"
    );
}

// --- SELECT DISTINCT: 複数列・非 TEXT・id ----------------------------------------------

#[test]
fn select_distinct_supports_multiple_columns_non_text_and_id() {
    let (core, path) = build_core("sql25-distinct-multi");
    let _guard = CleanupGuard(path);
    let truths = all_truths();
    let ctx = ctx_for("tenant-a", true);
    let rows = visible(&truths, "tenant-a", true);

    // 単一の非 TEXT 列。
    let got = run(&core, &ctx, "SELECT DISTINCT n FROM t ORDER BY n");
    assert_eq!(got.rows.len(), distinct_groups(&rows, "n"));
    assert!(matches!(got.rows[0].cells[0], Cell::SignedInteger(_)));

    // 複数列（TEXT と非 TEXT の混在）。
    let mut seen = std::collections::BTreeSet::new();
    for t in &rows {
        seen.insert((t.lang, t.n));
    }
    let got = run(&core, &ctx, "SELECT DISTINCT lang, n FROM t");
    assert_eq!(got.rows.len(), seen.len());
    for row in &got.rows {
        assert!(seen.contains(&(
            as_text(&row.cells[0]).as_deref().map(|s| match s {
                "ja" => "ja",
                "en" => "en",
                "secret" => "secret",
                other => panic!("unexpected lang {other}"),
            }),
            as_n(&row.cells[1]).map(|v| i32::try_from(v).expect("i32")),
        )));
    }

    // 疑似列 id（テナント内で一意なので可視行数と一致）。
    let got = run(&core, &ctx, "SELECT DISTINCT id FROM t");
    assert_eq!(got.rows.len(), rows.len());

    // 同じ列の重複記述と別名。
    let got = run(&core, &ctx, "SELECT DISTINCT n, n AS m FROM t ORDER BY m");
    assert_eq!(got.rows.len(), distinct_groups(&rows, "n"));
    for row in &got.rows {
        assert_eq!(row.cells[0], row.cells[1]);
    }

    // ORDER BY DESC（NULL 先頭）＋ LIMIT。
    let got = run(
        &core,
        &ctx,
        "SELECT DISTINCT day, mood FROM t ORDER BY day DESC, mood ASC LIMIT 2",
    );
    assert_eq!(got.rows.len(), 2);
}

#[test]
fn select_distinct_column_count_limit_and_shape_errors() {
    let (core, path) = build_core("sql25-distinct-errors");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    // 8 列は受理、9 列は 54000。
    let eight = "n, big, r, d, flag, price, mood, day";
    run(&core, &ctx, &format!("SELECT DISTINCT {eight} FROM t"));
    assert_eq!(
        run_err(&core, &ctx, &format!("SELECT DISTINCT {eight}, ts FROM t")),
        "54000"
    );
    assert_eq!(run_err(&core, &ctx, "SELECT DISTINCT * FROM t"), "42601");
    assert_eq!(
        run_err(&core, &ctx, "SELECT DISTINCT n, COUNT(*) FROM t"),
        "42601"
    );
    assert_eq!(
        run_err(&core, &ctx, "SELECT DISTINCT embedding FROM t"),
        "22000"
    );
}

// --- RLS 非漏えい ----------------------------------------------------------------------

#[test]
fn other_tenants_private_non_text_keys_never_appear() {
    let (core, path) = build_core("sql25-agg-rls");
    let _guard = CleanupGuard(path);
    let truths = all_truths();
    // tenant-b は tenant-a の Private 行を見られない（自分の Private も要求しない）。
    let ctx = ctx_for("tenant-b", false);
    let rows = visible(&truths, "tenant-b", false);
    assert!(rows.iter().all(|r| r.n != Some(SECRET_N)));

    let contains_secret = |result: &QueryResult| -> bool {
        result.rows.iter().any(|r| {
            r.cells.iter().any(|c| match c {
                Cell::SignedInteger(v) => *v == i64::from(SECRET_N),
                Cell::Date(v) => *v == SECRET_DAY,
                Cell::Integer(v) => *v == SECRET_ID,
                Cell::Text(s) => s == "secret",
                _ => false,
            })
        })
    };

    for sql in [
        "SELECT n, COUNT(*) FROM t GROUP BY n",
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY n DESC",
        "SELECT day, COUNT(*) FROM t GROUP BY day",
        "SELECT id, COUNT(*) FROM t GROUP BY id",
        "SELECT lang, n, COUNT(*) FROM t GROUP BY lang, n",
        "SELECT DISTINCT n FROM t",
        "SELECT DISTINCT day, n FROM t ORDER BY n DESC",
        "SELECT DISTINCT id FROM t",
    ] {
        let got = run(&core, &ctx, sql);
        assert!(!contains_secret(&got), "{sql}: {got:?}");
    }

    // グループ数・件数・並び順（DESC 先頭の値）にも他テナントの存在が影響しない。
    let got = run(
        &core,
        &ctx,
        "SELECT n, COUNT(*) AS c FROM t GROUP BY n ORDER BY n DESC LIMIT 1",
    );
    assert_ne!(as_n(&got.rows[0].cells[0]), Some(i64::from(SECRET_N)));
    let got = run(&core, &ctx, "SELECT id, COUNT(*) FROM t GROUP BY id");
    assert_eq!(got.rows.len(), rows.len());

    // 所有者自身には見える（対照）。
    let owner = ctx_for("tenant-a", true);
    let got = run(&core, &owner, "SELECT DISTINCT n FROM t");
    assert!(contains_secret(&got));
}

// --- 索引経路との整合・EXPLAIN --------------------------------------------------------

#[test]
fn non_text_group_key_uses_full_scan_and_text_key_still_uses_enumeration() {
    let (core, path) = build_core("sql25-agg-explain");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    let mut session = SessionState::default();
    let mut lines = |sql: &str| -> Vec<String> {
        match core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        {
            SqlOutcome::Explain(result) => result
                .rows
                .iter()
                .map(|r| match &r.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected text, got {other:?}"),
                })
                .collect(),
            other => panic!("expected Explain, got {other:?}"),
        }
    };
    assert_eq!(
        lines("EXPLAIN SELECT n, COUNT(*) FROM t GROUP BY n"),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
    assert_eq!(
        lines("EXPLAIN SELECT id, COUNT(*) FROM t GROUP BY id"),
        vec!["scalar_plan: plain_scan", "access_path: full_scan"]
    );
    assert_eq!(
        lines("EXPLAIN SELECT lang, COUNT(*) FROM t GROUP BY lang"),
        vec![
            "scalar_plan: plain_scan",
            "access_path: scalar_index_group_enumeration"
        ]
    );
}

#[test]
fn text_group_key_result_is_identical_between_cold_and_hot_index_paths() {
    let (core, path) = build_core("sql25-agg-text-index");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a", true);
    let sql = "SELECT lang, COUNT(*) AS c FROM t GROUP BY lang ORDER BY lang DESC";
    let cold = run(&core, &ctx, sql);
    let hot = run(&core, &ctx, sql);
    assert_eq!(cold.rows, hot.rows);
    // Issue #1185: 明示 DESC は NULL 先頭。
    assert!(matches!(cold.rows[0].cells[0], Cell::Null));
}

// --- 浮動小数点キーの同値契約（-0.0 と 0.0） ----------------------------------------------

#[test]
fn double_group_key_treats_negative_zero_and_zero_as_one_group() {
    let path = unique_db_path("sql25-agg-double-zero");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "z",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("d", ColumnType::Double, true),
            ],
        ))
        .expect("create table");
    let ctx = ctx_for("tenant-a", true);
    for (id, d) in (1u64..).zip([0.0f64, -0.0f64, 1.5f64]) {
        engine::tenant::insert_typed_row(
            &storage,
            "z",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Double(d)],
            &OperationId::parse(&format!("op-{id}")).expect("valid operation id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = run(
        &core,
        &ctx,
        "SELECT d, COUNT(*) AS c FROM z GROUP BY d ORDER BY d",
    );
    assert_eq!(got.rows.len(), 2);
    assert_eq!(as_count(&got.rows[0].cells[1]), 2);
    assert_eq!(as_count(&got.rows[1].cells[1]), 1);
}
