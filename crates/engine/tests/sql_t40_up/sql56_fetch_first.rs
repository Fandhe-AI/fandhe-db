//! `FETCH FIRST n ROWS ONLY`／`OFFSET n ROWS`／`FETCH FIRST n ROWS WITH TIES` の結合テスト
//! （Issue #1793、対象ビヘイビア: SQL-56 (3)。ポインタ: `docs/spec/05-tasks.md` TASK-298・
//! `docs/spec/04-behavior/sql-surface.md` SQL-56）。
//!
//! `tests/sql56_order_by_nulls.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路）で、次を固定する。
//!
//! - `FETCH FIRST n ROWS ONLY`／`OFFSET m ROWS` が `LIMIT n`／`OFFSET m` と同じ結果・同じ
//!   エラー分類（範囲・上限を含む）になること。
//! - `LIMIT` との併記・`PERCENT`・`ORDER BY` の無い `WITH TIES` などが `42601` であること。
//! - `WITH TIES` の結果が PostgreSQL 17 の意味（最後の行と `ORDER BY` の値が等しい行を加える。
//!   同値行は id 昇順）になること。期待値は表のデータから手計算した固定値。
//! - 同値を加えた件数が結果行数の上限を超えたら `54000`、他テナントの同値行は結果にも成否にも
//!   影響しないこと。
//! - 対応しない形の `WITH TIES` が黙って `ONLY` 扱いにならず `0A000`／`42601` で拒否されること。
//! - 拡張問い合わせ（Parse → Bind → Execute）でも同じ結果になること、カーソル文の `FETCH` が
//!   従来どおり解釈されること。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    )
}

// (id, n, lang)。`ORDER BY n`（ASC・NULL は末尾）の順は 7, 2, 5, 8, 1, 4, 6, 3, 9。
// n の同値グループ: 0=[7]・1=[2]・2=[5,8]・3=[1,4,6]・NULL=[3,9]。
const ROWS: [(u64, Option<i32>, &str); 9] = [
    (1, Some(3), "ja"),
    (2, Some(1), "en"),
    (3, None, "ja"),
    (4, Some(3), "en"),
    (5, Some(2), "ja"),
    (6, Some(3), "fr"),
    (7, Some(0), "en"),
    (8, Some(2), "fr"),
    (9, None, "fr"),
];

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(id: u64) -> OperationId {
    OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id")
}

fn insert_row(
    storage: &Storage,
    ctx: &PolicyContext,
    visibility: Visibility,
    id: u64,
    n: Option<i32>,
    lang: &str,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            n.map(Value::Integer).unwrap_or(Value::Null),
        ],
        &op_id(id),
    )
    .expect("insert row");
}

/// tenant-a に `ROWS`、tenant-b に `n = 3` の同値行を 5 件（tenant-a の結果へ現れてはならない）。
fn build(name: &str, limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for (id, n, lang) in ROWS {
        insert_row(&storage, &ctx, Visibility::Public, id, n, lang);
    }
    let other = ctx_for("tenant-b");
    for k in 0..5u64 {
        insert_row(
            &storage,
            &other,
            Visibility::Private,
            1000 + k,
            Some(3),
            "en",
        );
    }
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_result_limits(l);
    }
    (core, guard)
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    core.execute_sql(ctx, sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// 結果（成功なら行の Debug 表現、失敗なら `wire_code`）。2 つの書き方が同じ振る舞いかの比較用。
fn outcome(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<String, &'static str> {
    match core.execute_sql(ctx, sql) {
        Ok(r) => Ok(format!("{:?}", r.rows)),
        Err(e) => Err(e.wire_code()),
    }
}

fn assert_same(core: &EngineCore, ctx: &PolicyContext, fetch: &str, limit: &str) {
    assert_eq!(
        outcome(core, ctx, fetch),
        outcome(core, ctx, limit),
        "{fetch} vs {limit}"
    );
}

// ---------- ONLY は LIMIT と同じ ----------

#[test]
fn only_is_equivalent_to_limit_on_every_route() {
    let (core, _g) = build("sql56f-only", None);
    let ctx = ctx_for("tenant-a");
    let pairs: [(&str, &str); 14] = [
        (
            "SELECT id FROM docs FETCH FIRST 3 ROWS ONLY",
            "SELECT id FROM docs LIMIT 3",
        ),
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 3 ROWS ONLY",
            "SELECT id FROM docs ORDER BY n LIMIT 3",
        ),
        (
            "SELECT id FROM docs ORDER BY n FETCH NEXT ROW ONLY",
            "SELECT id FROM docs ORDER BY n LIMIT 1",
        ),
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST ROWS ONLY",
            "SELECT id FROM docs ORDER BY n LIMIT 1",
        ),
        (
            "SELECT id FROM docs ORDER BY n OFFSET 2 ROWS FETCH NEXT 3 ROWS ONLY",
            "SELECT id FROM docs ORDER BY n LIMIT 3 OFFSET 2",
        ),
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 3 ROWS ONLY OFFSET 2 ROWS",
            "SELECT id FROM docs ORDER BY n LIMIT 3 OFFSET 2",
        ),
        (
            "SELECT id FROM docs ORDER BY n LIMIT 3 OFFSET 2 ROWS",
            "SELECT id FROM docs ORDER BY n LIMIT 3 OFFSET 2",
        ),
        (
            "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' FETCH FIRST 2 ROWS ONLY",
            "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2",
        ),
        (
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY lang FETCH FIRST 2 ROWS ONLY",
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY lang LIMIT 2",
        ),
        (
            "SELECT DISTINCT lang FROM docs ORDER BY lang FETCH FIRST 2 ROWS ONLY",
            "SELECT DISTINCT lang FROM docs ORDER BY lang LIMIT 2",
        ),
        (
            "SELECT id FROM docs WHERE lang = 'ja' UNION SELECT id FROM docs WHERE lang = 'en' FETCH FIRST 3 ROWS ONLY",
            "SELECT id FROM docs WHERE lang = 'ja' UNION SELECT id FROM docs WHERE lang = 'en' LIMIT 3",
        ),
        (
            "WITH c AS (SELECT id, n FROM docs) SELECT id FROM c ORDER BY n FETCH FIRST 2 ROWS ONLY",
            "WITH c AS (SELECT id, n FROM docs) SELECT id FROM c ORDER BY n LIMIT 2",
        ),
        (
            "SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY n FETCH FIRST 2 ROWS ONLY) ORDER BY id LIMIT 10",
            "SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY n LIMIT 2) ORDER BY id LIMIT 10",
        ),
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 3 ROWS ONLY",
            "select id from docs order by n fetch first 3 rows only",
        ),
    ];
    for (fetch, limit) in pairs {
        assert_same(&core, &ctx, fetch, limit);
        // どちらも失敗する組で「同じ」と見なさないよう、成功することも確認する。
        assert!(
            outcome(&core, &ctx, fetch).is_ok(),
            "{fetch}: {:?}",
            outcome(&core, &ctx, fetch)
        );
    }
    // 結果自体も固定する（`ORDER BY n` の先頭 3 件）。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id FROM docs ORDER BY n FETCH FIRST 3 ROWS ONLY"
        )),
        vec![7, 2, 5]
    );
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id FROM docs ORDER BY n OFFSET 2 ROWS FETCH NEXT 3 ROWS ONLY"
        )),
        vec![5, 8, 1]
    );
}

#[test]
fn row_count_contract_matches_limit() {
    let (core, _g) = build("sql56f-contract", None);
    let ctx = ctx_for("tenant-a");
    for value in ["0", "-1", "1.5", "10001", "99999999999", "'x'"] {
        assert_same(
            &core,
            &ctx,
            &format!("SELECT id FROM docs FETCH FIRST {value} ROWS ONLY"),
            &format!("SELECT id FROM docs LIMIT {value}"),
        );
        assert_same(
            &core,
            &ctx,
            &format!("SELECT id FROM docs ORDER BY n OFFSET {value} ROWS FETCH FIRST 1 ROW ONLY"),
            &format!("SELECT id FROM docs ORDER BY n LIMIT 1 OFFSET {value}"),
        );
        assert_same(
            &core,
            &ctx,
            &format!("SELECT lang, COUNT(*) FROM docs GROUP BY lang FETCH FIRST {value} ROWS ONLY"),
            &format!("SELECT lang, COUNT(*) FROM docs GROUP BY lang LIMIT {value}"),
        );
    }
    assert_eq!(code(&core, &ctx, "SELECT id FROM docs LIMIT 0"), "22000");
    assert_eq!(
        code(&core, &ctx, "SELECT id FROM docs FETCH FIRST 0 ROWS ONLY"),
        "22000"
    );
}

#[test]
fn malformed_forms_are_syntax_errors() {
    let (core, _g) = build("sql56f-syntax", None);
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT id FROM docs LIMIT 1 FETCH FIRST 1 ROWS ONLY",
        "SELECT id FROM docs FETCH FIRST 1 ROWS ONLY LIMIT 1",
        "SELECT id FROM docs FETCH FIRST 1 ROWS ONLY FETCH FIRST 1 ROWS ONLY",
        "SELECT id FROM docs FETCH FIRST 10 PERCENT ROWS ONLY",
        "SELECT id FROM docs FETCH FIRST 1 ROWS",
        "SELECT id FROM docs FETCH FIRST 1 ONLY",
        "SELECT id FROM docs FETCH FIRST 1 ROWS WITH TIES",
        "SELECT id FROM docs FETCH FIRST (1) ROWS ONLY",
        "SELECT id FROM docs FETCH FIRST 1 ROWS ONLY OFFSET 1 ROWS OFFSET 2 ROWS",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

// ---------- WITH TIES ----------

#[test]
fn with_ties_adds_rows_equal_to_the_last_row() {
    let (core, _g) = build("sql56f-ties", None);
    let ctx = ctx_for("tenant-a");
    let cases: [(&str, Vec<u64>); 12] = [
        // 同値なし（n=0 の 1 件、次は n=1）。
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 2 ROWS WITH TIES",
            vec![7, 2],
        ),
        // 最後の行（id 5, n=2）の同値 id 8 が加わる。
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 3 ROWS WITH TIES",
            vec![7, 2, 5, 8],
        ),
        // 最後の行 id 1（n=3）の同値 4, 6 が加わる。
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 5 ROWS WITH TIES",
            vec![7, 2, 5, 8, 1, 4, 6],
        ),
        // 件数省略は 1 件。n=0 は 1 行だけ。
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST ROW WITH TIES",
            vec![7],
        ),
        // OFFSET は同値判定の対象外（OFFSET 後の最後の行で判定）。1 行読み飛ばして 7 を除く。
        (
            "SELECT id FROM docs ORDER BY n OFFSET 1 ROW FETCH NEXT 2 ROWS WITH TIES",
            vec![2, 5, 8],
        ),
        (
            "SELECT id FROM docs ORDER BY n FETCH FIRST 2 ROWS WITH TIES OFFSET 1 ROWS",
            vec![2, 5, 8],
        ),
        // DESC（NULL が先頭）。最初の行の NULL に id 9 が同値。
        (
            "SELECT id FROM docs ORDER BY n DESC FETCH FIRST 1 ROW WITH TIES",
            vec![3, 9],
        ),
        (
            "SELECT id FROM docs ORDER BY n DESC NULLS LAST FETCH FIRST 2 ROWS WITH TIES",
            vec![1, 4, 6],
        ),
        (
            "SELECT id FROM docs ORDER BY n NULLS FIRST FETCH FIRST 1 ROW WITH TIES",
            vec![3, 9],
        ),
        // 序数キー・式キー。
        (
            "SELECT id, n FROM docs ORDER BY 2 FETCH FIRST 3 ROWS WITH TIES",
            vec![7, 2, 5, 8],
        ),
        (
            "SELECT id FROM docs ORDER BY lower(lang) FETCH FIRST 1 ROW WITH TIES",
            vec![2, 4, 7],
        ),
        // 先頭キーが id（同値は生じない）。
        (
            "SELECT id FROM docs ORDER BY id FETCH FIRST 2 ROWS WITH TIES",
            vec![1, 2],
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(ids(&run(&core, &ctx, sql)), expected, "{sql}");
    }
}

#[test]
fn with_ties_on_group_by() {
    let (core, _g) = build("sql56f-ties-group", None);
    let ctx = ctx_for("tenant-a");
    // グループ件数: n=3 が 3 件、n=2 と NULL が 2 件、n=0 と n=1 が 1 件。
    let agg = "SELECT n, COUNT(*) AS c FROM docs GROUP BY n ORDER BY c DESC";
    let one = run(&core, &ctx, &format!("{agg} FETCH FIRST 1 ROW WITH TIES"));
    assert_eq!(one.rows.len(), 1);
    // 2 件目（c=2）に同値のグループがもう 1 つあるため、3 件になる。
    let two = run(&core, &ctx, &format!("{agg} FETCH FIRST 2 ROWS WITH TIES"));
    let three = run(&core, &ctx, &format!("{agg} LIMIT 3"));
    assert_eq!(two.rows.len(), 3);
    assert_eq!(format!("{:?}", two.rows), format!("{:?}", three.rows));
    // OFFSET と併用（c=3 を読み飛ばし、残り 1 件分 + 同値）。
    let off = run(
        &core,
        &ctx,
        &format!("{agg} OFFSET 1 ROW FETCH NEXT 1 ROW WITH TIES"),
    );
    assert_eq!(off.rows.len(), 2);
}

#[test]
fn with_ties_row_cap_and_tenant_isolation() {
    // 結果行数の上限 3。tenant-a の n=3 の同値は 3 行（1・4・6）で、上限ちょうど。
    let (core, _g) = build("sql56f-ties-cap", Some(ResultLimits::new(3)));
    let a = ctx_for("tenant-a");
    let sql = "SELECT id FROM docs ORDER BY n DESC NULLS LAST FETCH FIRST 1 ROW WITH TIES";
    assert_eq!(ids(&run(&core, &a, sql)), vec![1, 4, 6]);
    // tenant-b の同値行（5 件）は tenant-a の結果にも成否にも影響しない。逆に tenant-b 自身は
    // 5 件が上限 3 を超えるため、結果を打ち切らず 54000。
    let b = ctx_for("tenant-b");
    assert_eq!(code(&core, &b, sql), "54000");
    // 上限 2 では tenant-a も超過する。
    let (core2, _g2) = build("sql56f-ties-cap2", Some(ResultLimits::new(2)));
    assert_eq!(code(&core2, &a, sql), "54000");
    // LIMIT 1（ONLY）は上限内。
    assert_eq!(
        ids(&run(
            &core2,
            &a,
            "SELECT id FROM docs ORDER BY n DESC NULLS LAST FETCH FIRST 1 ROW ONLY"
        )),
        vec![1]
    );
}

#[test]
fn unsupported_with_ties_forms_are_rejected_not_downgraded() {
    let (core, _g) = build("sql56f-ties-reject", None);
    let ctx = ctx_for("tenant-a");
    let feature_not_supported = [
        "SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES)",
        "SELECT id FROM (SELECT id, n FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES) s",
        "WITH c AS (SELECT id, n FROM docs) SELECT id FROM c ORDER BY n FETCH FIRST 1 ROW WITH TIES",
        "EXPLAIN SELECT id FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES",
        "CREATE VIEW v AS SELECT id FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES",
        "INSERT INTO docs (embedding, lang) SELECT embedding, lang FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES USING OPERATION_ID 'x'",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES",
    ];
    for sql in feature_not_supported {
        let c = core
            .parse_sql(sql)
            .err()
            .map(|e| e.wire_code())
            .unwrap_or("accepted");
        assert!(c == "0A000" || c == "42601", "{sql}: {c}");
    }
    // 少なくとも次は `0A000`（文種・入れ子の位置が解釈できないことを明示する）。
    for sql in [
        "WITH c AS (SELECT id, n FROM docs) SELECT id FROM c ORDER BY n FETCH FIRST 1 ROW WITH TIES",
        "EXPLAIN SELECT id FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES",
        "SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES)",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
    // 構文が WITH TIES を知らない経路は 42601。
    for sql in [
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' FETCH FIRST 1 ROW WITH TIES",
        "SELECT DISTINCT lang FROM docs ORDER BY lang FETCH FIRST 1 ROW WITH TIES",
        "SELECT id FROM docs WHERE lang = 'ja' UNION SELECT id FROM docs WHERE lang = 'en' FETCH FIRST 1 ROW WITH TIES",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

// ---------- 拡張問い合わせ・カーソル ----------

fn run_prepared(core: &EngineCore, sql: &str, values: &[&str]) -> Result<Vec<u64>, String> {
    let mut session = SessionState::default();
    let prepared = core
        .parse_sql_prepared(sql)
        .map_err(|e| e.wire_code().to_string())?;
    core.describe_prepared_in_session(&session, &prepared)
        .map_err(|e| e.wire_code().to_string())?;
    let binds: Vec<Option<Vec<u8>>> = values.iter().map(|v| Some(v.as_bytes().to_vec())).collect();
    let bound = core
        .bind_prepared(&prepared, &binds)
        .map_err(|e| e.wire_code().to_string())?;
    match core.execute_parsed_in_session(&ctx_for("tenant-a"), &mut session, &bound) {
        Ok(SqlOutcome::Query(r)) => Ok(ids(&r)),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

#[test]
fn prepared_statements_accept_the_same_forms() {
    let (core, _g) = build("sql56f-prepared", None);
    // WITH TIES: 全行が lang = 'en' で同値になるため 3 件すべて。
    assert_eq!(
        run_prepared(
            &core,
            "SELECT id FROM docs WHERE lang = $1 ORDER BY lang FETCH FIRST 1 ROW WITH TIES",
            &["en"]
        ),
        Ok(vec![2, 4, 7])
    );
    assert_eq!(
        run_prepared(
            &core,
            "SELECT id FROM docs WHERE lang = $1 ORDER BY n FETCH FIRST 2 ROWS ONLY",
            &["en"]
        ),
        Ok(vec![7, 2])
    );
    assert_eq!(
        run_prepared(
            &core,
            "SELECT id FROM docs WHERE lang = $1 ORDER BY n OFFSET 1 ROW FETCH NEXT 1 ROW ONLY",
            &["en"]
        ),
        Ok(vec![2])
    );
    // `$n` は件数の位置では `LIMIT` と同じく受理しない。
    assert_eq!(
        run_prepared(
            &core,
            "SELECT id FROM docs FETCH FIRST $1 ROWS ONLY",
            &["1"]
        ),
        run_prepared(&core, "SELECT id FROM docs LIMIT $1", &["1"]),
    );
}

#[test]
fn cursor_fetch_statements_still_parse() {
    let (core, _g) = build("sql56f-cursor", None);
    // 文頭の `FETCH` はカーソル文のまま（書き換えない）。
    for sql in ["FETCH 2 FROM c", "FETCH FORWARD 3 FROM c"] {
        let parsed = core.parse_sql(sql);
        assert!(parsed.is_ok(), "{sql}: {:?}", parsed.err());
    }
    // カーソル文の文法外（件数が数値でない）は従来どおり構文エラー。
    for sql in ["FETCH NEXT FROM c", "FETCH FIRST FROM c"] {
        assert_eq!(
            core.parse_sql(sql).err().map(|e| e.wire_code()),
            Some("42601"),
            "{sql}"
        );
    }
}

/// GROUP BY の `WITH TIES` も、同値グループ追加後の件数に実行時の `ResultLimits` を適用する
/// （超過は黙って返さず `54000`。Issue #1793 のレビュー対応）。
#[test]
fn with_ties_on_group_by_respects_result_limits() {
    // lang ごとの COUNT(*) は ja・en・fr がすべて 3 で、先頭 1 件の同値で 3 グループになる。
    let sql =
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY c FETCH FIRST 1 ROW WITH TIES";
    let (core, _g) = build("sql56f-ties-group-cap", Some(ResultLimits::new(2)));
    assert_eq!(code(&core, &ctx_for("tenant-a"), sql), "54000");
    let (core3, _g3) = build("sql56f-ties-group-cap3", Some(ResultLimits::new(3)));
    assert_eq!(run(&core3, &ctx_for("tenant-a"), sql).rows.len(), 3);
}

/// スカラー `ORDER BY` の `WITH TIES` も同値行追加後に `ResultLimits` で `54000` になる。
#[test]
fn with_ties_on_scalar_respects_result_limits() {
    let sql = "SELECT id FROM docs ORDER BY n DESC NULLS LAST FETCH FIRST 1 ROW WITH TIES";
    let (core, _g) = build("sql56f-ties-scalar-cap", Some(ResultLimits::new(2)));
    assert_eq!(code(&core, &ctx_for("tenant-a"), sql), "54000");
}

/// `public` が見えない `search_path` では、入れ子の `WITH TIES`（`0A000`）より未修飾表の
/// `42P01` を優先する（`42601` → `42P01` → その他の順を保つ）。
#[test]
fn nested_with_ties_keeps_search_path_gate_priority() {
    let (core, _g) = build("sql56f-ties-gate", None);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "SET search_path = tenant_b")
        .expect("set search_path");
    let sql = "SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY n FETCH FIRST 1 ROW WITH TIES)";
    let err = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("must fail");
    assert_eq!(err.wire_code(), "42P01");
    // `public` が見えるセッションでは従来どおり `0A000`。
    assert_eq!(code(&core, &ctx, sql), "0A000");
}
