//! 複数列 `GROUP BY`（SQL-25 (d)。TASK-167・SQL-14 の単一列実装からの拡張。
//! NoSQL 表層の配列形受理〔NOSQL-16 (b)〕は別 Issue）の結合テスト。
//!
//! `tests/sql_group_by.rs`（単一列版）と同じ流儀（`unique_db_path`＋
//! `CleanupGuard`、決定的擬似乱数 xorshift64*、独立オラクルで可視集合・
//! グループ集計値を手計算し `EngineCore::execute_sql` の結果と突き合わせる）で
//! 複合キー（`lang`, `kind`）の集計を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};
use std::collections::BTreeMap;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const DIM: usize = 4;
const TABLE: &str = "docs";
const TENANTS: [&str; 2] = ["tenant-a", "tenant-b"];
const LANGS: [&str; 3] = ["ja", "en", "fr"];
// `None` を混在させ、複合キーの NULL 成分（末尾配置）を確認する。
const KINDS: [Option<&str>; 3] = [Some("blog"), Some("news"), None];
const ROWS_PER_TENANT: u64 = 24;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("kind", ColumnType::Text, true),
        ],
    )
}

/// 列数上限（[`engine::sql::allowlist::MAX_GROUP_BY_COLUMNS`]）検証用の
/// 全列 TEXT スキーマ。
fn wide_schema(column_count: usize) -> TableSchema {
    let mut columns = vec![ColumnDef::new(
        "embedding",
        ColumnType::Vector(DIM as u32),
        false,
    )];
    for i in 0..column_count {
        columns.push(ColumnDef::new(format!("c{i}"), ColumnType::Text, false));
    }
    TableSchema::new(TABLE, columns)
}

#[derive(Clone)]
struct RowTruth {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: &'static str,
    kind: Option<&'static str>,
}

fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

fn seed_corpus(storage: &Storage) -> Vec<RowTruth> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0xC0FF_EE12_3456_7890);
    let mut truths = Vec::new();
    let mut id = 1u64;
    for &tenant in TENANTS.iter() {
        for i in 0..ROWS_PER_TENANT {
            let visibility = if i % 3 == 0 {
                Visibility::Private
            } else {
                Visibility::Public
            };
            let lang = LANGS[(rng.next_u64() as usize) % LANGS.len()];
            let kind = KINDS[(rng.next_u64() as usize) % KINDS.len()];
            let ctx =
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            engine::tenant::insert_typed_row(
                storage,
                TABLE,
                &ctx,
                id,
                visibility,
                &[
                    Value::Vector(vec![0.0f32; DIM]),
                    Value::Text(lang.to_string()),
                    kind.map(|k| Value::Text(k.to_string()))
                        .unwrap_or(Value::Null),
                ],
                &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                    .expect("valid operation_id"),
            )
            .expect("insert row");
            truths.push(RowTruth {
                id,
                tenant,
                visibility,
                lang,
                kind,
            });
            id += 1;
        }
    }
    truths
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn visible_rows<'a>(
    truths: &'a [RowTruth],
    viewer_tenant: &str,
    allow_private: bool,
) -> Vec<&'a RowTruth> {
    truths
        .iter()
        .filter(|t| is_allowed(t, viewer_tenant, allow_private))
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

fn as_integer(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn as_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(v) => Some(v.clone()),
        Cell::Null => None,
        other => panic!("expected Cell::Text or Cell::Null, got {other:?}"),
    }
}

/// オラクル: `visible` 行を `(lang, kind)` の複合キーでグループ化した
/// `(count, id_sum)`。
fn oracle_groups(visible: &[&RowTruth]) -> BTreeMap<(String, Option<String>), (u64, u64)> {
    let mut groups: BTreeMap<(String, Option<String>), (u64, u64)> = BTreeMap::new();
    for row in visible {
        let key = (row.lang.to_string(), row.kind.map(str::to_string));
        let entry = groups.entry(key).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += row.id;
    }
    groups
}

fn row_map(result: &QueryResult) -> BTreeMap<(String, Option<String>), Vec<Cell>> {
    let mut out = BTreeMap::new();
    for row in &result.rows {
        let lang = as_text(&row.cells[0]).expect("lang column must not be NULL");
        let kind = as_text(&row.cells[1]);
        out.insert((lang, kind), row.cells.clone());
    }
    out
}

// --- 正しさ: 複合キー集計がオラクルと一致（NULL キー成分を含む） -----------------

#[test]
fn multi_column_group_by_matches_independent_oracle() {
    let path = unique_db_path("group-by-multi-basic");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core = new_core(storage);

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);
            let expected = oracle_groups(&visible);

            let result = core
                .execute_sql(
                    &ctx,
                    "SELECT lang, kind, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang, kind",
                )
                .expect("multi-column GROUP BY query should succeed");

            assert_eq!(
                result.rows.len(),
                expected.len(),
                "group count mismatch tenant={tenant} allow_private={allow_private}"
            );
            let rows = row_map(&result);
            for ((lang, kind), (count, id_sum)) in &expected {
                let cells = rows
                    .get(&(lang.clone(), kind.clone()))
                    .unwrap_or_else(|| panic!("missing group {lang:?}/{kind:?}"));
                assert_eq!(
                    as_integer(&cells[2]),
                    *count,
                    "COUNT lang={lang} kind={kind:?}"
                );
                assert_eq!(
                    as_integer(&cells[3]),
                    *id_sum,
                    "SUM(id) lang={lang} kind={kind:?}"
                );
            }
        }
    }
}

// --- 既定順序: (lang, kind) の辞書式昇順、各成分の NULL は末尾 ------------------

#[test]
fn multi_column_group_by_default_order_is_lexicographic_with_nulls_last() {
    let path = unique_db_path("group-by-multi-order");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, kind, COUNT(*) AS n FROM docs GROUP BY lang, kind",
        )
        .expect("query should succeed");

    let keys: Vec<(String, Option<String>)> = result
        .rows
        .iter()
        .map(|row| (as_text(&row.cells[0]).unwrap(), as_text(&row.cells[1])))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_by(|a, b| match a.0.cmp(&b.0) {
        std::cmp::Ordering::Equal => match (&a.1, &b.1) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        },
        other => other,
    });
    assert_eq!(
        keys, sorted,
        "default order must be (lang, kind) lexicographic with NULL last"
    );
}

// --- ORDER BY 2 番目のキーで DESC・LIMIT ---------------------------------------

#[test]
fn multi_column_group_by_order_by_second_key_desc_with_limit() {
    let path = unique_db_path("group-by-multi-orderby");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, kind, COUNT(*) AS n FROM docs GROUP BY lang, kind ORDER BY kind DESC LIMIT 100",
        )
        .expect("query should succeed");

    let kinds: Vec<Option<String>> = result
        .rows
        .iter()
        .map(|row| as_text(&row.cells[1]))
        .collect();
    // NULL は DESC でも末尾のまま。
    let mut last_non_null: Option<String> = None;
    let mut seen_null = false;
    for kind in &kinds {
        match kind {
            Some(k) => {
                assert!(
                    !seen_null,
                    "non-NULL kind must not appear after a NULL kind in DESC order"
                );
                if let Some(prev) = &last_non_null {
                    assert!(prev >= k, "kind must be descending: {prev} then {k}");
                }
                last_non_null = Some(k.clone());
            }
            None => seen_null = true,
        }
    }
}

// --- HAVING（複数述語 AND）が集計結果に適用される -------------------------------

#[test]
fn multi_column_group_by_having_with_multiple_predicates() {
    let path = unique_db_path("group-by-multi-having");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);
    let expected = oracle_groups(&visible);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, kind, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang, kind HAVING n > 1 AND s > 1",
        )
        .expect("query should succeed");

    let expected_count = expected
        .values()
        .filter(|(count, sum)| *count > 1 && *sum > 1)
        .count();
    assert_eq!(result.rows.len(), expected_count);
    for row in &result.rows {
        assert!(as_integer(&row.cells[2]) > 1);
        assert!(as_integer(&row.cells[3]) > 1);
    }
}

// --- RLS 境界: 他テナントにしか存在しないキーの組が結果に一切現れない -------------

#[test]
fn multi_column_group_by_never_reveals_other_tenants_exclusive_group() {
    let path = unique_db_path("group-by-multi-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");

    let writer_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    // tenant-a だけが持つ Private 専用の組 (lang="xx", kind="secret")。
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &writer_ctx,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("xx".to_string()),
            Value::Text("secret".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("op-1").expect("valid op"),
    )
    .expect("insert row");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &writer_ctx,
        2,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
            Value::Text("blog".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("op-2").expect("valid op"),
    )
    .expect("insert row");

    let core = new_core(storage);
    let viewer_ctx = ctx_for("tenant-b", true);
    let result = core
        .execute_sql(
            &viewer_ctx,
            "SELECT lang, kind, COUNT(*) AS n FROM docs GROUP BY lang, kind",
        )
        .expect("query should succeed");

    for row in &result.rows {
        let lang = as_text(&row.cells[0]).unwrap();
        assert_ne!(
            lang, "xx",
            "tenant-b must never see tenant-a's private-only group"
        );
    }
}

// --- 上限: 列数 8 は成功・9 は 54000 ---------------------------------------------

#[test]
fn multi_column_group_by_column_count_at_limit_succeeds_and_over_limit_is_rejected() {
    let path = unique_db_path("group-by-multi-column-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&wide_schema(9))
        .expect("create wide table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Public,
        &{
            let mut values = vec![Value::Vector(vec![0.0f32; DIM])];
            for i in 0..9 {
                values.push(Value::Text(format!("v{i}")));
            }
            values
        },
        &engine::recovery::required_op_id::OperationId::parse("op-1").expect("valid op"),
    )
    .expect("insert row");
    let core = new_core(storage);

    let at_limit_columns: Vec<String> = (0..8).map(|i| format!("c{i}")).collect();
    let at_limit_sql = format!(
        "SELECT {}, COUNT(*) AS n FROM docs GROUP BY {}",
        at_limit_columns[0],
        at_limit_columns.join(", ")
    );
    core.execute_sql(&ctx, &at_limit_sql)
        .expect("8-column GROUP BY must be accepted");

    let over_limit_columns: Vec<String> = (0..9).map(|i| format!("c{i}")).collect();
    let over_limit_sql = format!(
        "SELECT {}, COUNT(*) AS n FROM docs GROUP BY {}",
        over_limit_columns[0],
        over_limit_columns.join(", ")
    );
    let err = core
        .execute_sql(&ctx, &over_limit_sql)
        .expect_err("9-column GROUP BY must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- 重複列は構文層で拒否される --------------------------------------------------

#[test]
fn multi_column_group_by_rejects_duplicate_columns() {
    let path = unique_db_path("group-by-multi-duplicate");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let err = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang, lang",
        )
        .expect_err("duplicate GROUP BY column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
