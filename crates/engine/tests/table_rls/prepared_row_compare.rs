//! WHERE の行値比較の成分に置いた `$n`（拡張クエリ。対象ビヘイビア: SQL-9・WIRE-12。
//! 親 Issue #3254・本 Issue #3257）の結合テスト。
//!
//! 役割: `EngineCore::parse_sql_prepared`（Parse）・`describe_prepared_in_session`（Describe）・
//! `bind_prepared`（Bind）・`execute_parsed_in_session`（Execute）が、同じ値をリテラルで書いた
//! 簡易クエリと「同じ行集合、または同じ SQLSTATE」になること（第 2 の評価器を作らない設計の証明）、
//! Describe 用の型付きダミーが相手列の型で有効なこと、序数（ダミーフラグ）がずれないこと、
//! 未対応形が fail-closed で拒否されることを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, &'static str> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
        .map_err(|e| e.wire_code())
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    if let Err(c) = run(core, tenant, sql) {
        panic!("{sql} failed: {c}");
    }
}

fn ids_of(o: SqlOutcome) -> Vec<u64> {
    let SqlOutcome::Query(r) = o else {
        panic!("not a query");
    };
    let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

/// Prepared 経路（Parse → Describe → Bind → Execute）の結果。Parse／Describe／Bind／Execute の
/// どこで失敗しても SQLSTATE で返す。
fn prepared_ids(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    values: &[Option<Vec<u8>>],
) -> Result<Vec<u64>, &'static str> {
    let prepared = core.parse_sql_prepared(sql).map_err(|e| e.wire_code())?;
    let session = SessionState::default();
    core.describe_prepared_in_session(&session, &prepared)
        .map_err(|e| e.wire_code())?;
    let bound = core
        .bind_prepared(&prepared, values)
        .map_err(|e| e.wire_code())?;
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx(tenant), &mut s, &bound)
        .map(ids_of)
        .map_err(|e| e.wire_code())
}

fn literal_ids(core: &EngineCore, tenant: &str, sql: &str) -> Result<Vec<u64>, &'static str> {
    run(core, tenant, sql).map(ids_of)
}

fn code_of_parse(core: &EngineCore, sql: &str) -> &'static str {
    match core.parse_sql_prepared(sql) {
        Ok(_) => panic!("{sql} must be rejected at Parse"),
        Err(e) => e.wire_code(),
    }
}

const CREATE_ENUM: &str = "CREATE TYPE mood AS ENUM ('m1', 'm2', 'm3', 'm4')";
const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INTEGER, bg BIGINT, \
     r REAL, f DOUBLE PRECISION, s TEXT, v VARCHAR(20), n NUMERIC(10,2), d DATE, \
     ts TIMESTAMP, tz TIMESTAMPTZ, tm TIME, iv INTERVAL, u UUID, bb BYTEA, m mood, bo BOOLEAN, \
     j JSONB)";

/// alice: id1〜4（列ごとに昇順の値）、bob: id5（id2 と同じ値）。
fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(&core, "alice", CREATE_ENUM);
    ok(&core, "alice", CREATE);
    for (tenant, id, i) in [
        ("alice", 1, 1),
        ("alice", 2, 2),
        ("alice", 3, 3),
        ("alice", 4, 4),
        ("bob", 5, 2),
    ] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO t (id, embedding, a, bg, r, f, s, v, n, d, ts, tz, tm, iv, u, bb, m, bo, j) \
                 VALUES ({id}, '[0.1,0.2]', {i}, {i}0, {i}.5, {i}.25, 's{i}', 'v{i}', {i}.50, \
                 '2024-01-0{i}', '2024-01-0{i} 10:00:00', '2024-01-0{i} 10:00:00+00', '1{i}:00:00', \
                 '{i} hours', '00000000-0000-0000-0000-00000000000{i}', '\\x0{i}', 'm{i}', {bo}, '{{}}') \
                 USING OPERATION_ID 'op{id}'",
                bo = if i % 2 == 0 { "true" } else { "false" },
            ),
        );
    }
    (path, guard, core)
}

/// 列ごとの `(列名, 種別, 行 2 の値の text 表記)`。種別は `q`＝クォート付きリテラル、`n`＝裸のリテラル。
const COLUMNS: [(&str, char, &str); 16] = [
    ("a", 'n', "2"),
    ("bg", 'n', "20"),
    ("r", 'n', "2.5"),
    ("f", 'n', "2.25"),
    ("s", 'q', "s2"),
    ("v", 'q', "v2"),
    ("n", 'q', "2.50"),
    ("d", 'q', "2024-01-02"),
    ("ts", 'q', "2024-01-02 10:00:00"),
    ("tz", 'q', "2024-01-02 10:00:00+00"),
    ("tm", 'q', "12:00:00"),
    ("iv", 'q', "2 hours"),
    ("u", 'q', "00000000-0000-0000-0000-000000000002"),
    ("m", 'q', "m2"),
    ("bo", 'n', "true"),
    ("id", 'n', "2"),
];

fn lit(kind: char, v: &str) -> String {
    if kind == 'q' {
        format!("'{}'", v.replace('\'', "''"))
    } else {
        v.to_string()
    }
}

/// 全演算子 × 全対応型 × 左右の位置で、Prepared の結果がリテラル形と一致する（行集合または SQLSTATE）。
#[test]
fn prepared_row_compare_matches_literal_form_across_types_and_operators() {
    let (_p, _g, core) = setup("prep-row-cmp-parity");
    let mut compared = 0usize;
    for (col, kind, pivot) in COLUMNS {
        for op in ["=", "<>", "<", "<=", ">", ">="] {
            // `$n` を左辺に置く形・右辺に置く形。第 2 成分は `a`（INTEGER）で固定する。
            for left in [false, true] {
                let (prep, literal) = if left {
                    (
                        format!("SELECT id FROM t WHERE ($1, $2) {op} ({col}, a) LIMIT 10"),
                        format!(
                            "SELECT id FROM t WHERE ({}, 2) {op} ({col}, a) LIMIT 10",
                            lit(kind, pivot)
                        ),
                    )
                } else {
                    (
                        format!("SELECT id FROM t WHERE ({col}, a) {op} ($1, $2) LIMIT 10"),
                        format!(
                            "SELECT id FROM t WHERE ({col}, a) {op} ({}, 2) LIMIT 10",
                            lit(kind, pivot)
                        ),
                    )
                };
                let got = prepared_ids(&core, "alice", &prep, &[some(pivot), some("2")]);
                let want = literal_ids(&core, "alice", &literal);
                assert_eq!(got, want, "{prep} <-> {literal}");
                compared += 1;
            }
        }
    }
    assert_eq!(compared, COLUMNS.len() * 12);
}

/// 受理される代表形は実際に行を返す（上のパリティが「全て同じエラー」で空振りしないことの確認）。
#[test]
fn prepared_row_compare_returns_expected_rows_for_representative_types() {
    let (_p, _g, core) = setup("prep-row-cmp-rows");
    for (col, vals, want) in [
        ("a", ["2", "2"], vec![3, 4]),
        ("d", ["2024-01-02", "2"], vec![3, 4]),
        ("ts", ["2024-01-02 10:00:00", "2"], vec![3, 4]),
        ("tz", ["2024-01-02 10:00:00+00", "2"], vec![3, 4]),
        (
            "u",
            ["00000000-0000-0000-0000-000000000002", "2"],
            vec![3, 4],
        ),
        ("n", ["2.50", "2"], vec![3, 4]),
        ("s", ["s2", "2"], vec![3, 4]),
    ] {
        let sql = format!("SELECT id FROM t WHERE ({col}, a) > ($1, $2) LIMIT 10");
        let got = prepared_ids(&core, "alice", &sql, &[some(vals[0]), some(vals[1])]);
        assert_eq!(got, Ok(want), "{sql}");
    }
}

/// キーセットページングの代表形 `(d, id) > ($1, $2)` が Parse・Describe・Bind・Execute のすべてで通る。
#[test]
fn keyset_paging_with_date_and_id_works_end_to_end() {
    let (_p, _g, core) = setup("prep-row-cmp-keyset");
    let sql = "SELECT id FROM t WHERE (d, id) > ($1, $2) ORDER BY id LIMIT 2;";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    assert_eq!(prepared.param_count(), 2);
    let types = prepared.param_types();
    assert_eq!(types.len(), 2);
    let session = SessionState::default();
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe before bind must accept the typed dummy");
    // 同じ準備済み文を別の値・別のテナントで再 Bind できる。
    for (tenant, vals, want) in [
        ("alice", ["2024-01-02", "2"], vec![3, 4]),
        ("alice", ["2024-01-03", "3"], vec![4]),
        ("alice", ["2024-01-01", "0"], vec![1, 2]),
        ("bob", ["2024-01-01", "0"], vec![5]),
    ] {
        let bound = core
            .bind_prepared(&prepared, &[some(vals[0]), some(vals[1])])
            .expect("bind");
        let mut s = SessionState::default();
        let out = core
            .execute_parsed_in_session(&ctx(tenant), &mut s, &bound)
            .expect("execute");
        assert_eq!(ids_of(out), want, "{tenant} {vals:?}");
    }
}

/// Describe（Bind 前）は対応する全型の相手列で成功する。型付きダミーが列型で有効であることの確認。
/// interval・boolean・enum の順序比較などはリテラル形と同じ束縛エラーになりうるため、Describe の成否は
/// 「同じ形を型に合うリテラルで書いた文の Describe」と一致することを見る。
#[test]
fn describe_accepts_typed_dummy_for_every_supported_column_type() {
    let (_p, _g, core) = setup("prep-row-cmp-describe");
    let session = SessionState::default();
    let mut accepted = 0usize;
    for (col, _, _) in COLUMNS {
        let sql = format!("SELECT id FROM t WHERE ({col}, a) < ($1, $2) LIMIT 10");
        let prepared = core
            .parse_sql_prepared(&sql)
            .unwrap_or_else(|e| panic!("{sql}: parse {}", e.wire_code()));
        let dummy = match col {
            "a" | "bg" | "id" => "0",
            "r" | "f" => "0.5",
            "bo" => "true",
            "d" => "'2000-01-01'",
            "ts" => "'2000-01-01 00:00:00'",
            "tz" => "'2000-01-01 00:00:00+00'",
            "tm" => "'00:00:00'",
            "iv" => "'0 seconds'",
            "u" => "'00000000-0000-0000-0000-000000000000'",
            "m" => "'m1'",
            _ => "'0'",
        };
        let literal = core
            .parse_sql(&format!(
                "SELECT id FROM t WHERE ({col}, a) < ({dummy}, 2) LIMIT 10"
            ))
            .and_then(|p| core.describe_parsed_in_session(&session, &p).map(|_| ()))
            .map_err(|e| e.wire_code());
        let described = core
            .describe_prepared_in_session(&session, &prepared)
            .map(|_| ())
            .map_err(|e| e.wire_code());
        assert_eq!(described, literal, "{sql}");
        accepted += usize::from(described.is_ok());
    }
    // 日付・時刻・UUID・数値などは実際に受理される（全てがエラーで一致する空振りを防ぐ）。
    assert!(accepted >= 10, "only {accepted} column types were accepted");
}

/// 型を持たない列（JSONB・ベクトル）の相手は Parse で固定の `0A000`（値を含まない）。
#[test]
fn unsupported_column_types_are_rejected_at_parse_with_0a000() {
    let (_p, _g, core) = setup("prep-row-cmp-unsupported");
    for sql in [
        "SELECT id FROM t WHERE (j, a) = ($1, $2) LIMIT 10",
        "SELECT id FROM t WHERE (a, j) = ($1, $2) LIMIT 10",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert!(!err.to_string().contains('$'), "{err}");
    }
}

/// intentional: docs/design/wire-extended-query-param-binding.md
/// 拒否が残る形（fail-closed）。相手がリテラル・式・修飾列、EXPLAIN 前置、集合演算、
/// 宣言型の無い `$n` 同士、`$n` が式の内側にある形。
#[test]
fn unsupported_shapes_stay_fail_closed() {
    let (_p, _g, core) = setup("prep-row-cmp-reject");
    for (sql, want) in [
        ("SELECT id FROM t WHERE (a, 7) < ($1, $2) LIMIT 10", "42601"),
        (
            "SELECT id FROM t WHERE (a, a + 1) < ($1, $2) LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE (a, t.a) < ($1, $2) LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE (a, bg) < ($1 + 1, $2) LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE (a, bg) < ($1::int, $2) LIMIT 10",
            "42601",
        ),
        (
            "EXPLAIN SELECT id FROM t WHERE (a, bg) < ($1, $2) LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE (a, bg) < ($1, $2) UNION SELECT id FROM t WHERE a = 1 LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE ($1, $2) < ($3, $4) LIMIT 10",
            "42P18",
        ),
        ("SELECT id FROM t WHERE (a, bg) < ($1) LIMIT 10", "42601"),
        (
            "SELECT id FROM t WHERE a IN ((a, bg) < ($1, $2)) LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE lower((a, bg) < ($1, $2)) = 'x' LIMIT 10",
            "42601",
        ),
    ] {
        assert_eq!(code_of_parse(&core, sql), want, "{sql}");
    }
}

/// intentional: docs/design/wire-extended-query-param-binding.md
/// NULL のバインドは、リテラル `NULL` 成分と同じ SQLSTATE になる（スカラー比較 `col < NULL` と同じ規則。
/// 行値の NULL 成分の評価は本 Issue の対象外）。
#[test]
fn null_bind_matches_literal_null_component() {
    let (_p, _g, core) = setup("prep-row-cmp-null");
    let got = prepared_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, bg) < ($1, $2) LIMIT 10",
        &[None, some("5")],
    );
    let want = literal_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, bg) < (NULL, 5) LIMIT 10",
    );
    assert_eq!(got, want);
    assert!(want.is_err(), "literal NULL component is expected to fail");
}

/// 不正な値は Bind でリテラル形と同じ SQLSTATE になり、メッセージに値本文を含めない。
#[test]
fn invalid_values_fail_at_bind_like_the_literal_form() {
    let (_p, _g, core) = setup("prep-row-cmp-invalid");
    let sql = "SELECT id FROM t WHERE (d, a) < ($1, $2) LIMIT 10";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    let bad = "not-a-date-SECRETVALUE";
    let err = core
        .bind_prepared(&prepared, &[some(bad), some("2")])
        .and_then(|b| {
            let mut s = SessionState::default();
            core.execute_parsed_in_session(&ctx("alice"), &mut s, &b)
        })
        .expect_err("invalid date must fail");
    let lit = literal_ids(
        &core,
        "alice",
        &format!("SELECT id FROM t WHERE (d, a) < ('{bad}', 2) LIMIT 10"),
    );
    assert_eq!(Err(err.wire_code()), lit);
    assert!(!err.to_string().contains("SECRETVALUE"), "{err}");

    let sql_int = "SELECT id FROM t WHERE (a, bg) < ($1, $2) LIMIT 10";
    let prepared = core.parse_sql_prepared(sql_int).expect("parse");
    let err = core
        .bind_prepared(&prepared, &[some("12x"), some("2")])
        .expect_err("non-numeric integer must fail at Bind");
    assert_eq!(err.wire_code(), "22P02");
}

/// 値に SQL の断片や `$n` を含めても単一のリテラルのまま扱われる（注入できない）。
#[test]
fn values_with_sql_fragments_stay_a_single_literal() {
    let (_p, _g, core) = setup("prep-row-cmp-inject");
    for v in ["x'), (1", "$2, $3", "s2' OR '1'='1", "'; DROP TABLE t; --"] {
        let got = prepared_ids(
            &core,
            "alice",
            "SELECT id FROM t WHERE (s, a) > ($1, $2) LIMIT 10",
            &[some(v), some("0")],
        );
        let want = literal_ids(
            &core,
            "alice",
            &format!(
                "SELECT id FROM t WHERE (s, a) > ('{}', 0) LIMIT 10",
                v.replace('\'', "''")
            ),
        );
        assert_eq!(got, want, "{v}");
    }
    // 表は無傷。
    assert_eq!(
        literal_ids(&core, "alice", "SELECT id FROM t WHERE a = 1 LIMIT 10"),
        Ok(vec![1])
    );
}

/// 序数（Describe 用ダミーフラグ）の回帰: 行値の `$n` と兄弟の `col = $k`（ENUM）が同居しても、
/// 兄弟の `$k` だけが検証を省かれ、行値内の実リテラル（語彙外ラベル）は Describe で `22P02` のまま。
/// `OR` 分岐・`NOT` 前置でも序数がずれない。
#[test]
fn describe_ordinals_stay_aligned_with_row_compare_params() {
    let (_p, _g, core) = setup("prep-row-cmp-ordinal");
    let session = SessionState::default();
    for sql in [
        "SELECT id FROM t WHERE (a, m) = ($1, $2) AND m = $3 LIMIT 10",
        "SELECT id FROM t WHERE m = $1 AND (a, bg) < ($2, $3) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) < ($1, $2) OR m = $3 LIMIT 10",
        "SELECT id FROM t WHERE NOT (a, bg) < ($1, $2) AND m = $3 LIMIT 10",
    ] {
        let prepared = core
            .parse_sql_prepared(sql)
            .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
        core.describe_prepared_in_session(&session, &prepared)
            .unwrap_or_else(|e| panic!("{sql}: describe {}", e.wire_code()));
    }
    for sql in [
        "SELECT id FROM t WHERE (a, m) = ($1, 'not-a-mood') AND m = $2 LIMIT 10",
        "SELECT id FROM t WHERE m = $1 AND (a, m) = (1, 'not-a-mood') LIMIT 10",
        "SELECT id FROM t WHERE (a, m) = ($1, 'not-a-mood') OR m = $2 LIMIT 10",
    ] {
        // 2 つ目の形は成分に `$n` が無い（既存の序数テストと同じ形）。1 つ目・3 つ目は相手が
        // リテラルのため Parse で 42601 になる。いずれも「検証が省かれて成功する」ことはない。
        let result = core
            .parse_sql_prepared(sql)
            .and_then(|p| core.describe_prepared_in_session(&session, &p).map(|_| ()));
        let c = result
            .expect_err("a real invalid literal must never be accepted")
            .wire_code();
        assert!(c == "22P02" || c == "42601", "{sql}: {c}");
    }
}

/// 述語形 UPDATE／DELETE（`USING OPERATION_ID $k` 併用）も同じ経路で動き、副作用は自テナントの
/// 該当行に限られる。
#[test]
fn predicate_dml_with_row_compare_params_touches_only_own_matching_rows() {
    let (_p, _g, core) = setup("prep-row-cmp-dml");
    let upd = core
        .parse_sql_prepared(
            "UPDATE t SET s = 'changed' WHERE (a, bg) >= ($1, $2) USING OPERATION_ID $3",
        )
        .expect("parse update");
    assert_eq!(upd.param_count(), 3);
    let bound = core
        .bind_prepared(&upd, &[some("3"), some("30"), some("op-upd-1")])
        .expect("bind");
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx("alice"), &mut s, &bound)
        .expect("execute update");
    assert_eq!(
        literal_ids(
            &core,
            "alice",
            "SELECT id FROM t WHERE s = 'changed' LIMIT 10"
        ),
        Ok(vec![3, 4])
    );
    // bob の行（id5。a=2 のため述語に該当しない）と alice の他行は不変。
    assert_eq!(
        literal_ids(
            &core,
            "bob",
            "SELECT id FROM t WHERE s = 'changed' LIMIT 10"
        ),
        Ok(vec![])
    );

    let del = core
        .parse_sql_prepared("DELETE FROM t WHERE (a, bg) = ($1, $2) USING OPERATION_ID $3")
        .expect("parse delete");
    let bound = core
        .bind_prepared(&del, &[some("2"), some("20"), some("op-del-1")])
        .expect("bind");
    core.execute_parsed_in_session(&ctx("alice"), &mut s, &bound)
        .expect("execute delete");
    assert_eq!(
        literal_ids(&core, "alice", "SELECT id FROM t WHERE a = 2 LIMIT 10"),
        Ok(vec![])
    );
    // bob の同値行は消えない。
    assert_eq!(
        literal_ids(&core, "bob", "SELECT id FROM t WHERE a = 2 LIMIT 10"),
        Ok(vec![5])
    );
}

/// kNN・集計の各文種でも受理され、リテラル形と一致する。
#[test]
fn row_compare_params_work_in_knn_and_aggregate_statements() {
    let (_p, _g, core) = setup("prep-row-cmp-stmt-kinds");
    let vals = [some("2"), some("20")];
    let knn = prepared_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, bg) > ($1, $2) ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10",
        &vals,
    );
    let knn_lit = literal_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, bg) > (2, 20) ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10",
    );
    assert_eq!(knn, knn_lit);
    assert_eq!(knn, Ok(vec![3, 4]));

    let prepared = core
        .parse_sql_prepared("SELECT COUNT(*) AS n FROM t WHERE (a, bg) > ($1, $2)")
        .expect("parse aggregate");
    let session = SessionState::default();
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe aggregate");
    let bound = core.bind_prepared(&prepared, &vals).expect("bind");
    let literal = core
        .parse_sql("SELECT COUNT(*) AS n FROM t WHERE (a, bg) > (2, 20)")
        .expect("literal parse");
    assert_eq!(bound, literal);
}

/// 一時表の DATE／UUID 列の行値比較 `$n` も、型付きダミーで Parse／Describe が通る
/// （一時表は一時コアで解決される。永続カタログだけを引いて既定ダミー `'0'` が実リテラルとして
/// 検証され失敗する退行を固定する）。
#[test]
fn row_compare_params_on_temp_table_date_and_uuid_columns_parse_and_describe() {
    let (_p, _g, core) = setup("prep-row-cmp-temp");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TEMP TABLE tt (k INTEGER, d2 DATE, u2 UUID)",
    )
    .expect("create temp table");
    let sql = "SELECT k FROM tt WHERE (d2, u2) > ($1, $2) LIMIT 10";
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&session, sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: parse {}", e.wire_code()));
    core.describe_prepared_in_session(&session, &prepared)
        .unwrap_or_else(|e| panic!("{sql}: describe {}", e.wire_code()));
}

/// スキーマ修飾の UPDATE 対象表（`UPDATE public.t`）の DATE／UUID 列も、型付きダミーで
/// Parse／Describe が通る（対象表の取りこぼしで既定ダミー `'0'` が実リテラルとして検証され
/// 失敗する退行を固定する。引用付き別名の走査は `core/row_compare_relation_names_tests.rs`）。
#[test]
fn row_compare_params_on_schema_qualified_update_parse_and_describe() {
    let (_p, _g, core) = setup("prep-row-cmp-qual");
    let session = SessionState::default();
    let sql = "UPDATE public.t SET s = 'x' WHERE (d, u) > ($1, $2) USING OPERATION_ID $3";
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&session, sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: parse {}", e.wire_code()));
    core.describe_prepared_in_session(&session, &prepared)
        .unwrap_or_else(|e| panic!("{sql}: describe {}", e.wire_code()));
}

/// スキーマ修飾の参照表（`FROM public.t`・`DELETE FROM public.t`）でも、行値比較の `$n` が相手列の型
/// （整数・真偽値）で Parse／Describe／Bind／Execute を通り、リテラル形と同じ行集合になる（Issue #3257）。
#[test]
fn schema_qualified_relation_row_compare_params_use_column_types() {
    let (_p, _g, core) = setup("prep-row-cmp-qualified");
    for (prepared_sql, literal_sql, values) in [
        (
            "SELECT id FROM public.t WHERE (a, bg) < ($1, $2) LIMIT 10",
            "SELECT id FROM public.t WHERE (a, bg) < (3, 30) LIMIT 10",
            vec![some("3"), some("30")],
        ),
        (
            "SELECT id FROM public.t WHERE (bo, a) = ($1, $2) LIMIT 10",
            "SELECT id FROM public.t WHERE (bo, a) = (true, 2) LIMIT 10",
            vec![some("true"), some("2")],
        ),
    ] {
        let got = prepared_ids(&core, "alice", prepared_sql, &values);
        let want = literal_ids(&core, "alice", literal_sql);
        assert!(got.is_ok(), "{prepared_sql}: {got:?}");
        assert_eq!(got, want, "{prepared_sql}");
    }
    // DELETE も Parse／Describe が通る（型は整数）。
    let sql = "DELETE FROM public.t WHERE (a, bg) < ($1, $2)";
    let prepared = core
        .parse_sql_prepared(sql)
        .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
    core.describe_prepared_in_session(&SessionState::default(), &prepared)
        .unwrap_or_else(|e| panic!("{sql}: describe {}", e.wire_code()));
}

/// スキーマ修飾の第 1 表の JOIN／カンマ結合では、行値比較の `$n` を 42601 で拒否する（fail-closed）。
/// JOIN の述語は複数スキャンへ分かれ Describe のダミーフラグ序数が信頼できないため（Issue #3257・PR 指摘）。
#[test]
fn schema_qualified_first_table_join_rejects_row_compare_params() {
    let (_p, _g, core) = setup("prep-row-cmp-qual-join");
    for sql in [
        "SELECT t.id FROM public.t JOIN t AS u ON t.id = u.id WHERE (t.a, t.bg) < ($1, $2) LIMIT 10",
        "SELECT t.id FROM public.t AS x JOIN t ON x.id = t.id WHERE (x.a, x.bg) < ($1, $2) LIMIT 10",
        "SELECT t.id FROM public.t, t AS u WHERE (t.a, t.bg) < ($1, $2) LIMIT 10",
        "SELECT t.id FROM public.t LEFT JOIN t AS u ON t.id = u.id WHERE (t.a, t.bg) < ($1, $2) LIMIT 10",
    ] {
        assert_eq!(code_of_parse(&core, sql), "42601", "{sql}");
    }
}
