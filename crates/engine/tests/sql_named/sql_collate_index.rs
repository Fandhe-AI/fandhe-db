//! 照合込みのスカラー二次索引（TABLE-28、Issue #2191）の結合テスト。
//!
//! 受け入れ条件: (a) 照合ごとの列（指定なし・`C`・`und-x-icu`・`ja-x-icu`）で、索引経路・全走査
//! （`id + 0 > 0` で索引を無効化）・素の形・Rust 側のオラクルの 4 者が一致する。(b) UNIQUE の
//! 重複判定が照合に従う（deterministic な照合では等値がバイト列の等値と一致する）。加えて、
//! 照合が異なる比較・ICU 列の前方一致では索引を使わず、`EXPLAIN` の `scalar_plan:` 行に
//! ` reason=collation_mismatch` が付くこと、`CREATE INDEX ... (col COLLATE "x")` の受理・
//! エラー分類・永続化、起動時 opt-in（宣言の照合が辞書の照合になる）も固定する。

use engine::catalog::{ColumnDef, ColumnType, IndexKind, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ROWS: u64 = 90;
const OTHER_TENANT_ID: u64 = 900;
/// 大小文字・アクセント（NFC と NFD）・かな・空文字・接頭辞を含む値プール。
const POOL: [&str; 9] = [
    "a", "A", "e\u{301}", "\u{e9}", "\u{3042}", "\u{30a2}", "", "z", "ab",
];
/// 索引の照合ごとの列（指定なし・`C`・`und-x-icu`・`ja-x-icu`・`VARCHAR` の `und-x-icu`）。
const COLUMNS: [&str; 5] = ["t0", "tc", "iu", "ij", "iv"];

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType, c: Option<Collation>| ColumnDef {
        collation: c,
        ..ColumnDef::new(name, ty, true)
    };
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("t0", ColumnType::Text, None),
            col("tc", ColumnType::Text, Some(Collation::C)),
            col("iu", ColumnType::Text, Some(Collation::UndIcu)),
            col("ij", ColumnType::Text, Some(Collation::JaIcu)),
            col("iv", ColumnType::Varchar(Some(40)), Some(Collation::UndIcu)),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id % 10 == 0` は NULL、それ以外は値プールから決定的に選ぶ。
fn value_of(id: u64) -> Option<&'static str> {
    if id.is_multiple_of(10) {
        None
    } else {
        POOL.get(((id * 7) % 9) as usize).copied()
    }
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, v: Option<&str>, vis: Visibility) {
    let text = || match v {
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
            text(),
            text(),
            text(),
            text(),
            text(),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn seed(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    for id in 1..=ROWS {
        insert_row(storage, "tenant-a", id, value_of(id), Visibility::Public);
    }
    // 他テナントの Private 行（どの述語にも合う空文字）。
    insert_row(
        storage,
        "tenant-b",
        OTHER_TENANT_ID,
        Some(""),
        Visibility::Private,
    );
}

fn open_core_at(path: &std::path::Path, do_seed: bool) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    if do_seed {
        seed(&storage);
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// 起動時 opt-in（宣言が索引の構築に効く。`hnsw_state.is_some()`）の `EngineCore`。
fn open_declared_core_at(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed(&storage);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(storage, kind)
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

fn ddl_code(core: &EngineCore, sql: &str) -> &'static str {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn sorted_ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    assert!(!v.contains(&OTHER_TENANT_ID), "other tenant row leaked");
    v
}

fn oracle(pred: impl Fn(&str) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| value_of(*id).is_some_and(&pred))
        .collect()
}

fn explain_scalar_line(core: &EngineCore, pred: &str) -> String {
    let mut session = SessionState::default();
    let sql = format!(
        "EXPLAIN SELECT id FROM {TABLE} WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5"
    );
    match core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, &sql)
        .unwrap_or_else(|e| panic!("EXPLAIN must succeed: {sql}: {e:?}"))
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .find_map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => s.strip_prefix("scalar_plan: ").map(str::to_string),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no scalar_plan line: {sql}")),
        other => panic!("expected Explain, got {other:?}"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// 索引経路が使われる（選択度の条件を満たすとき `index_scans` が増える）。
    Index,
    /// 索引経路は使われない（`index_scans` が増えない）。
    NoIndex,
}

/// 述語を索引経路・全走査強制・素の形で実行し、オラクルと一致することを固定する。
fn assert_parity(core: &EngineCore, pred: &str, expected: &[u64], expect: Expect) {
    let vec_sql =
        format!("SELECT id FROM {TABLE} WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200");
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, &vec_sql);
    let hot = run(core, &vec_sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(sorted_ids(&cold), sorted_ids(&hot), "cold/hot: {pred}");
    assert_eq!(sorted_ids(&hot), expected, "oracle: {pred}");
    match expect {
        Expect::Index if !expected.is_empty() && expected.len() * 2 <= ROWS as usize => {
            assert!(after > before, "index not consumed: {pred}");
        }
        Expect::NoIndex => assert_eq!(after, before, "index must not be consumed: {pred}"),
        Expect::Index => {}
    }
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

fn is_icu(col: &str) -> bool {
    matches!(col, "iu" | "ij" | "iv")
}

fn predicate_checks(core: &EngineCore, col: &str) {
    for needle in ["a", "A", "\u{e9}", "e\u{301}", "\u{3042}", "", "zz"] {
        assert_parity(
            core,
            &format!("{col} = '{needle}'"),
            &oracle(|v| v == needle),
            Expect::Index,
        );
    }
    assert_parity(
        core,
        &format!("{col} IN ('a', '\u{e9}', '\u{3042}')"),
        &oracle(|v| v == "a" || v == "\u{e9}" || v == "\u{3042}"),
        Expect::Index,
    );
    // 前方一致: 辞書がバイト列順の列だけが索引を使い、ICU 列は全走査へ縮退する。
    assert_parity(
        core,
        &format!("{col} LIKE 'a%'"),
        &oracle(|v| v.starts_with('a')),
        if is_icu(col) {
            Expect::NoIndex
        } else {
            Expect::Index
        },
    );
    // 完全一致形の LIKE（ワイルドカードなし）は等価として扱われる。
    assert_parity(
        core,
        &format!("{col} LIKE 'a'"),
        &oracle(|v| v == "a"),
        Expect::Index,
    );
}

/// 受け入れ条件 (a): 照合ごとの列で、索引・全走査・素の形・オラクルが一致する。ICU 列の
/// `=`・`IN` は索引を使う（辞書が ICU 照合順）。
#[test]
fn collated_columns_match_index_scan_and_oracle() {
    let path = unique_db_path("sql-collate-index-parity");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    for col in COLUMNS {
        predicate_checks(&core, col);
    }
}

/// 再起動後（同じパスで開き直す）も一致する。
#[test]
fn collated_index_results_survive_restart() {
    let path = unique_db_path("sql-collate-index-restart");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core_at(&path, true);
        predicate_checks(&core, "iu");
    }
    let core = open_core_at(&path, false);
    for col in COLUMNS {
        predicate_checks(&core, col);
    }
}

/// 照合だけの `ALTER`（カタログのみ）で列の照合が変わっても、索引は世代で作り直され
/// 結果が一致する（ICU 列を `C` へ、`C` 列を ICU へ）。
#[test]
fn collation_alter_rebuilds_index_in_the_new_order() {
    let path = unique_db_path("sql-collate-index-alter");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    for col in COLUMNS {
        predicate_checks(&core, col);
    }
    ddl(
        &core,
        "ALTER TABLE docs ALTER COLUMN iu TYPE TEXT COLLATE \"C\"",
    );
    ddl(
        &core,
        "ALTER TABLE docs ALTER COLUMN tc TYPE TEXT COLLATE \"und-x-icu\"",
    );
    for col in ["t0", "ij", "iv"] {
        predicate_checks(&core, col);
    }
    // ALTER した列は新しい照合の辞書で作り直される（結果は同じで、前方一致の可否が入れ替わる）。
    for col in ["iu", "tc"] {
        assert_parity(
            &core,
            &format!("{col} = 'a'"),
            &oracle(|v| v == "a"),
            Expect::Index,
        );
        assert_parity(
            &core,
            &format!("{col} IN ('a', '\u{e9}')"),
            &oracle(|v| v == "a" || v == "\u{e9}"),
            Expect::Index,
        );
    }
    assert_parity(
        &core,
        "iu LIKE 'a%'",
        &oracle(|v| v.starts_with('a')),
        Expect::Index,
    );
    assert_parity(
        &core,
        "tc LIKE 'a%'",
        &oracle(|v| v.starts_with('a')),
        Expect::NoIndex,
    );
    // 前方一致は ALTER 後の照合に従う: `iu` は C になったので索引を使い、`tc` は ICU になった。
    assert_eq!(explain_scalar_line(&core, "iu LIKE 'a%'"), "index_prefix");
    assert_eq!(
        explain_scalar_line(&core, "tc LIKE 'a%'"),
        "plain_scan reason=collation_mismatch"
    );
}

/// 照合が異なる比較は索引を使わず、結果はオラクルと一致し、EXPLAIN に理由が出る。
/// 照合が一致する比較・バイト列順同士の比較には理由が付かない。
#[test]
fn collation_mismatch_falls_back_with_reason() {
    let path = unique_db_path("sql-collate-index-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);

    // 実行結果（式レーン。索引は使わず全走査）。
    for (pred, expected) in [
        ("iu COLLATE \"C\" = 'a'", oracle(|v| v == "a")),
        ("tc COLLATE \"und-x-icu\" = 'a'", oracle(|v| v == "a")),
        ("iu COLLATE \"ja-x-icu\" = 'A'", oracle(|v| v == "A")),
        ("ij COLLATE \"und-x-icu\" = 'a'", oracle(|v| v == "a")),
    ] {
        assert_parity(&core, pred, &expected, Expect::NoIndex);
    }

    // 不一致: 理由付きの plain_scan。
    for pred in [
        "iu COLLATE \"C\" = 'a'",
        "tc COLLATE \"und-x-icu\" = 'a'",
        "ij COLLATE \"und-x-icu\" = 'a'",
        "iu COLLATE \"ja-x-icu\" = 'a'",
        "iu LIKE 'a%'",
    ] {
        assert_eq!(
            explain_scalar_line(&core, pred),
            "plain_scan reason=collation_mismatch",
            "{pred}"
        );
    }
    // 一致: 理由は付かない。
    assert_eq!(explain_scalar_line(&core, "iu = 'a'"), "index_equality");
    assert_eq!(explain_scalar_line(&core, "ij = 'a'"), "index_equality");
    assert_eq!(
        explain_scalar_line(&core, "iu IN ('a', 'A')"),
        "index_in_list"
    );
    assert_eq!(explain_scalar_line(&core, "tc LIKE 'a%'"), "index_prefix");
    assert_eq!(
        explain_scalar_line(&core, "iu COLLATE \"und-x-icu\" = 'a'"),
        "plain_scan"
    );
    assert_eq!(
        explain_scalar_line(&core, "tc COLLATE \"POSIX\" = 'a'"),
        "plain_scan"
    );
    assert_eq!(
        explain_scalar_line(&core, "t0 COLLATE \"C\" = 'a'"),
        "plain_scan"
    );
}

/// EXPLAIN の判定と実行時の経路が一致する（D5）。
#[test]
fn explain_agrees_with_runtime_path() {
    let path = unique_db_path("sql-collate-index-d5");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    for (pred, uses_index) in [
        ("iu = 'a'", true),
        ("ij IN ('a', 'A')", true),
        ("tc = 'a'", true),
        ("iu LIKE 'a%'", false),
        ("tc LIKE 'a%'", true),
    ] {
        let line = explain_scalar_line(&core, pred);
        assert_eq!(line.starts_with("index_"), uses_index, "{pred}: {line}");
        let sql = format!(
            "SELECT id FROM {TABLE} WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
        );
        // 初回は索引の構築（キャッシュミス）なので、温めてから経路を観測する。
        run(&core, &sql);
        let before = core.scalar_index_cache_stats();
        run(&core, &sql);
        let after = core.scalar_index_cache_stats();
        assert_eq!(
            after.index_scans > before.index_scans,
            uses_index,
            "runtime path differs from EXPLAIN: {pred}"
        );
    }
}

/// 起動時 opt-in では、宣言の照合が辞書の照合になる。ICU 列に `C` の宣言を付けると前方一致が
/// 索引を使え、`=` は列の照合（ICU）と索引の照合（C）が異なるため全走査へ縮退する。列の実効
/// 照合と同じ順序の宣言を足すと、辞書はそちらに決まる（宣言の追加順に依らない決定規則）。
#[test]
fn declared_collation_selects_the_dictionary_order() {
    let path = unique_db_path("sql-collate-index-declared");
    let _guard = CleanupGuard(path.clone());
    let core = open_declared_core_at(&path);

    ddl(&core, "CREATE INDEX i_iu_c ON docs (iu COLLATE \"C\")");
    assert_parity(
        &core,
        "iu LIKE 'a%'",
        &oracle(|v| v.starts_with('a')),
        Expect::Index,
    );
    assert_eq!(
        explain_scalar_line(&core, "iu LIKE 'a%'"),
        "index_prefix index=i_iu_c"
    );
    assert_parity(&core, "iu = 'a'", &oracle(|v| v == "a"), Expect::NoIndex);
    assert_eq!(
        explain_scalar_line(&core, "iu = 'a'"),
        "plain_scan reason=collation_mismatch"
    );

    // 列の実効照合（und-x-icu）と同じ順序の宣言があれば、それが辞書の照合になる。
    ddl(
        &core,
        "CREATE INDEX i_iu_u ON docs (iu COLLATE \"und-x-icu\")",
    );
    assert_parity(&core, "iu = 'a'", &oracle(|v| v == "a"), Expect::Index);
    assert_parity(
        &core,
        "iu LIKE 'a%'",
        &oracle(|v| v.starts_with('a')),
        Expect::NoIndex,
    );
    assert_eq!(
        explain_scalar_line(&core, "iu LIKE 'a%'"),
        "plain_scan reason=collation_mismatch"
    );

    // 宣言を外すと、残った宣言の照合（C）へ戻る。
    ddl(&core, "DROP INDEX i_iu_u");
    assert_parity(
        &core,
        "iu LIKE 'a%'",
        &oracle(|v| v.starts_with('a')),
        Expect::Index,
    );
}

/// 起動時 opt-in がなければ、宣言はカタログに記録されるだけで構築に効かない（現行契約）。
#[test]
fn declaration_collation_has_no_effect_without_opt_in() {
    let path = unique_db_path("sql-collate-index-no-optin");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    ddl(&core, "CREATE INDEX i_iu_c ON docs (iu COLLATE \"C\")");
    assert_parity(&core, "iu = 'a'", &oracle(|v| v == "a"), Expect::Index);
    assert_parity(
        &core,
        "iu LIKE 'a%'",
        &oracle(|v| v.starts_with('a')),
        Expect::NoIndex,
    );
    assert_eq!(explain_scalar_line(&core, "iu = 'a'"), "index_equality");
}

/// `CREATE INDEX ... (col COLLATE "x")` の受理・エラー分類・永続化。
#[test]
fn create_index_collate_accepts_and_classifies_errors() {
    let path = unique_db_path("sql-collate-index-ddl");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core_at(&path, true);
        ddl(
            &core,
            "CREATE INDEX i1 ON docs (iu COLLATE \"und-x-icu\", t0)",
        );
        ddl(&core, "CREATE INDEX i2 ON docs (ij COLLATE \"C\")");
        ddl(&core, "CREATE INDEX i3 ON docs (tc COLLATE \"POSIX\", id)");
        ddl(&core, "CREATE INDEX i4 ON docs (iv)");

        for (sql, code) in [
            // 42704: 未知の照合名。
            ("CREATE INDEX x ON docs (t0 COLLATE \"xx\")", "42704"),
            ("CREATE INDEX x ON docs (t0 COLLATE \"en-x-icu\")", "42704"),
            // 42804: 照合を持てない列・USING hnsw。
            ("CREATE INDEX x ON docs (id COLLATE \"C\")", "42804"),
            ("CREATE INDEX x ON docs (embedding COLLATE \"C\")", "42804"),
            (
                "CREATE INDEX x ON docs USING hnsw (embedding COLLATE \"C\")",
                "42804",
            ),
            // 42703: 列不在。
            ("CREATE INDEX x ON docs (zz COLLATE \"C\")", "42703"),
            // 42601: 二重の COLLATE・重複列。
            (
                "CREATE INDEX x ON docs (t0 COLLATE \"C\" COLLATE \"C\")",
                "42601",
            ),
            (
                "CREATE INDEX x ON docs (t0 COLLATE \"C\", t0 COLLATE \"und-x-icu\")",
                "42601",
            ),
            // 0A000: 式索引。
            ("CREATE INDEX x ON docs (lower(t0) COLLATE \"C\")", "0A000"),
        ] {
            assert_eq!(ddl_code(&core, sql), code, "{sql}");
        }
    }

    // 再起動後も宣言（照合込み）が復元される。照合のない宣言は従来の形式のまま。
    let storage = Storage::open(&path).expect("reopen storage");
    let mut defs = storage.list_indexes().expect("list indexes");
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    let seen: Vec<(&str, Vec<Option<Collation>>)> = defs
        .iter()
        .map(|d| (d.name.as_str(), d.collations.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("i1", vec![Some(Collation::UndIcu), None]),
            ("i2", vec![Some(Collation::C)]),
            ("i3", vec![Some(Collation::Posix), None]),
            ("i4", vec![None]),
        ]
    );
    assert!(defs.iter().all(|d| matches!(d.kind, IndexKind::Scalar)));
}

fn insert_unique(core: &EngineCore, id: u64, v: &str) -> Result<(), &'static str> {
    core.execute_insert_sql(
        &ctx("tenant-a"),
        &format!("INSERT INTO u (id, a) VALUES ({id}, '{v}') USING OPERATION_ID 'op-{id}'"),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code())
}

/// 受け入れ条件 (b): UNIQUE（PRIMARY KEY を含む）の重複判定が照合に従う。ICU の deterministic
/// な照合では、照合上は等しく見える値（大小・かな・NFC と NFD）もバイト列が異なれば別の値で、
/// 完全一致の重複だけが `23505`。照合だけの ALTER の前後と再起動後も同じ。
#[test]
fn unique_follows_the_deterministic_collation() {
    let path = unique_db_path("sql-collate-index-unique");
    let _guard = CleanupGuard(path.clone());
    let distinct = [
        "a", "A", "\u{3042}", "\u{30a2}", "\u{e9}", "e\u{301}", "ab", "",
    ];
    {
        let core = open_core_at_unique(&path);
        for (i, v) in distinct.iter().enumerate() {
            insert_unique(&core, i as u64 + 1, v)
                .unwrap_or_else(|c| panic!("distinct value {v:?} must be accepted: {c}"));
        }
        for (i, v) in distinct.iter().enumerate() {
            assert_eq!(
                insert_unique(&core, 100 + i as u64, v),
                Err("23505"),
                "exact duplicate {v:?} must be rejected"
            );
        }
        // 照合だけの ALTER の後も判定は変わらない。
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "ALTER TABLE u ALTER COLUMN a TYPE TEXT COLLATE \"ja-x-icu\"",
        )
        .expect("collation-only alter");
        assert_eq!(insert_unique(&core, 200, "a"), Err("23505"));
        assert_eq!(insert_unique(&core, 201, "b"), Ok(()));
    }
    // 再起動後も同じ。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(insert_unique(&core, 300, "\u{3042}"), Err("23505"));
    assert_eq!(insert_unique(&core, 301, "\u{3044}"), Ok(()));
}

fn open_core_at_unique(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, a TEXT COLLATE \"und-x-icu\" UNIQUE)",
    )
    .expect("create unique table");
    core
}

/// ICU 照合の GROUP BY は照合順を保つ（既存の挙動の回帰確認）。Declared で辞書が ICU 順の
/// C 列は列挙形を使わず `full_scan` になる（`column_groups` はバイト列順の辞書だけを列挙する）。
#[test]
fn group_by_keeps_collation_order_and_skips_icu_dictionary_enumeration() {
    let path = unique_db_path("sql-collate-index-group");
    let _guard = CleanupGuard(path.clone());
    let core = open_declared_core_at(&path);
    let expected_groups = |col: &str| -> Vec<Option<String>> {
        let mut seen: Vec<Option<String>> = (1..=ROWS)
            .map(|id| value_of(id).map(str::to_string))
            .collect();
        seen.sort();
        seen.dedup();
        let _ = col;
        seen
    };
    for col in ["tc", "iu"] {
        let res = run(
            &core,
            &format!("SELECT {col}, COUNT(*) FROM docs GROUP BY {col}"),
        );
        let mut got: Vec<Option<String>> = res
            .rows
            .iter()
            .map(|r| match &r.cells[0] {
                Cell::Null => None,
                Cell::Text(s) => Some(s.clone()),
                other => panic!("unexpected cell: {other:?}"),
            })
            .collect();
        got.sort();
        assert_eq!(got, expected_groups(col), "{col}");
    }
    // C 列に ICU の宣言を付けると、辞書は ICU 順になり列挙形は使えない。
    ddl(
        &core,
        "CREATE INDEX i_tc_u ON docs (tc COLLATE \"und-x-icu\")",
    );
    let mut session = SessionState::default();
    let explain = match core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT tc, COUNT(*) FROM docs GROUP BY tc",
        )
        .expect("explain")
    {
        SqlOutcome::Explain(r) => r,
        other => panic!("expected Explain, got {other:?}"),
    };
    let lines: Vec<String> = explain
        .rows
        .iter()
        .filter_map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        lines.iter().any(|l| l == "access_path: full_scan"),
        "{lines:?}"
    );
    let res = run(&core, "SELECT tc, COUNT(*) FROM docs GROUP BY tc");
    assert_eq!(res.rows.len(), expected_groups("tc").len());
}
