//! `EXPLAIN (FORMAT JSON)` の結合テスト（Issue #2059・SQL-76 (1)(6) ポインタ）。
//!
//! 計画の木を PostgreSQL 互換の JSON（最上位は配列、要素は `{"Plan": ...}`、各節は `Node Type` と
//! 子の `Plans`）で返す。TEXT と同じ情報を持つこと、`FORMAT TEXT`／オプションなしが従来出力と
//! バイト単位で一致すること、オプションの誤りが固定の `wire_code` で拒否されること、実行せず
//! 副作用が無いこと、Describe が実行せず `json` 型を返すことを検証する。

use std::collections::BTreeMap;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, k BIGINT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO b (k, w) VALUES (1, 'b1'), (2, 'b2')",
        "INSERT INTO pt (id, k) VALUES (1, 2), (2, 3)",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn result_of(core: &EngineCore, tenant: &str, sql: &str) -> engine::sql::exec::QueryResult {
    match run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Explain(r) | SqlOutcome::Query(r) => r,
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn text_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    result_of(core, tenant, sql)
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

fn json_text(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let r = result_of(core, tenant, sql);
    assert_eq!(r.rows.len(), 1, "{sql}: JSON must be one row");
    assert_eq!(r.rows[0].cells.len(), 1, "{sql}: JSON must be one column");
    assert!(
        matches!(
            r.columns.as_slice(),
            [ColumnMeta::Computed {
                ty: Some(engine::catalog::ColumnType::Json),
                ..
            }]
        ),
        "{sql}: column must be json: {:?}",
        r.columns
    );
    match &r.rows[0].cells[0] {
        Cell::Json(s) => s.clone(),
        other => panic!("expected Json cell, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

/// JSON の節を前順に平らにする（`Node Type` と属性の `key: value` を TEXT の行と同じ順に並べる）。
fn flatten(node: &BTreeMap<String, JsonValue>, depth: usize, out: &mut Vec<String>) {
    // BTreeMap は順序を失うため、属性の並びは「キー名の集合」で比較する。節の見出しは別に取る。
    let nt = match node.get("Node Type") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("Node Type missing: {other:?}"),
    };
    let mut attrs: Vec<String> = node
        .iter()
        .filter(|(k, _)| *k != "Node Type" && *k != "Plans")
        .map(|(k, v)| match v {
            JsonValue::String(s) => format!("{k}: {s}"),
            other => panic!("attribute must stay a string: {other:?}"),
        })
        .collect();
    attrs.sort();
    out.push(format!("{depth}|{nt}|{}", attrs.join(" ; ")));
    if let Some(JsonValue::Array(children)) = node.get("Plans") {
        for c in children {
            match c {
                JsonValue::Object(o) => flatten(o, depth + 1, out),
                other => panic!("child must be an object: {other:?}"),
            }
        }
    }
}

/// TEXT の字下げ規則を読んで同じ平坦形にする（独立した実装で JSON 側と突き合わせる）。
fn flatten_text(lines: &[String]) -> Vec<String> {
    let mut nodes: Vec<(usize, String, Vec<String>)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let indent = l.len() - l.trim_start().len();
        let body = l.trim_start();
        if let Some(stage) = body.strip_prefix("-> ") {
            nodes.push((indent / 2 + 1, stage.to_string(), Vec::new()));
        } else if i == 0 && !body.contains(": ") {
            nodes.push((0, body.to_string(), Vec::new()));
        } else {
            let depth = if indent == 0 { 0 } else { (indent - 1) / 2 };
            if nodes.last().map(|n| n.0) != Some(depth) {
                nodes.push((depth, "result".to_string(), Vec::new()));
            }
            nodes.last_mut().unwrap().2.push(body.to_string());
        }
    }
    nodes
        .into_iter()
        .map(|(d, nt, mut attrs)| {
            attrs.sort();
            format!("{d}|{nt}|{}", attrs.join(" ; "))
        })
        .collect()
}

fn assert_json_matches_text(core: &EngineCore, sql_body: &str) {
    let text = text_lines(core, "tenant-a", &format!("EXPLAIN {sql_body}"));
    let json = json_text(
        core,
        "tenant-a",
        &format!("EXPLAIN (FORMAT JSON) {sql_body}"),
    );
    let parsed =
        parse_json(&json).unwrap_or_else(|e| panic!("{sql_body}: invalid JSON: {e:?}\n{json}"));
    let JsonValue::Array(top) = parsed else {
        panic!("{sql_body}: top level must be an array");
    };
    assert_eq!(top.len(), 1);
    let JsonValue::Object(wrapper) = &top[0] else {
        panic!("{sql_body}: element must be an object");
    };
    let Some(JsonValue::Object(plan)) = wrapper.get("Plan") else {
        panic!("{sql_body}: Plan missing");
    };
    let mut got = Vec::new();
    flatten(plan, 0, &mut got);
    assert_eq!(got, flatten_text(&text), "{sql_body}\n{json}");
}

/// TEXT は内側計画を葉の行として載せる（従来出力の維持）一方、JSON は木へ戻して載せるため、
/// 複合計画の `INSERT ... SELECT` は構造の一致ではなく変換成功と節点の存在を確かめる。
#[test]
fn insert_select_compound_inner_plan_converts_to_json() {
    let (core, _g) = seeded("x76-ins-sel");
    for (body, needles) in [
        (
            "INSERT INTO a (k) SELECT a.k FROM a JOIN b ON a.k = b.k LIMIT 10",
            ["\"limit\"", "\"join\""],
        ),
        (
            "INSERT INTO a (k) SELECT k FROM b UNION SELECT k FROM c",
            ["\"select\"", "\"scan\""],
        ),
    ] {
        let json = json_text(&core, "tenant-a", &format!("EXPLAIN (FORMAT JSON) {body}"));
        parse_json(&json).unwrap_or_else(|e| panic!("{body}: invalid JSON: {e:?}\n{json}"));
        for n in needles {
            assert!(json.contains(n), "{body}: {n} missing\n{json}");
        }
    }
}

#[test]
fn json_matches_text_for_every_plan_shape() {
    // SQL-76 (6)
    let (core, _g) = seeded("x76-shapes");
    for body in [
        "SELECT * FROM a",
        "SELECT k FROM a WHERE k = 1 LIMIT 5",
        "SELECT k, COUNT(*) FROM a GROUP BY k",
        "SELECT a.k, b.w FROM a JOIN b ON a.k = b.k ORDER BY a.k LIMIT 10",
        "SELECT * FROM a LEFT JOIN b ON a.k = b.k LIMIT 10",
        "SELECT k FROM a UNION SELECT k FROM b",
        "SELECT k FROM a UNION SELECT k FROM b INTERSECT SELECT k FROM c LIMIT 5",
        "(SELECT k FROM a UNION SELECT k FROM b) INTERSECT SELECT k FROM c",
        "WITH w AS (SELECT k FROM a ORDER BY k LIMIT 2) SELECT k FROM w",
        "WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w",
        "WITH RECURSIVE w AS (SELECT k FROM a) SELECT k FROM w",
        "SELECT k FROM (SELECT k FROM a ORDER BY k LIMIT 2) t",
        "SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
        "SELECT k FROM a WHERE k IN (SELECT k FROM b) OR EXISTS (SELECT 1 FROM c)",
        "INSERT INTO a (k) VALUES (1)",
        "INSERT INTO a (k) SELECT 1",
        "DELETE FROM a WHERE k > 1",
        "UPDATE a SET v = 'z' WHERE k > 1",
        "DELETE FROM a WHERE k > 1 USING OPERATION_ID 'p' PARTITIONED CHUNK 2",
        "UPDATE a SET v = 'z' WHERE k > 1 USING OPERATION_ID 'p' PARTITIONED CHUNK 2",
    ] {
        assert_json_matches_text(&core, body);
    }
}

#[test]
fn text_format_and_no_option_are_byte_identical() {
    // SQL-76 (1)
    let (core, _g) = seeded("x76-text");
    for body in ["SELECT * FROM a", "SELECT a.k FROM a JOIN b ON a.k = b.k"] {
        let plain = text_lines(&core, "tenant-a", &format!("EXPLAIN {body}"));
        for opt in ["FORMAT TEXT", "format text", "FORMAT JSON, FORMAT TEXT"] {
            assert_eq!(
                text_lines(&core, "tenant-a", &format!("EXPLAIN ({opt}) {body}")),
                plain,
                "{opt}"
            );
        }
    }
}

#[test]
fn json_keyword_case_and_last_option_wins() {
    let (core, _g) = seeded("x76-case");
    let a = json_text(&core, "tenant-a", "EXPLAIN (FORMAT JSON) SELECT * FROM a");
    assert_eq!(
        a,
        json_text(&core, "tenant-a", "explain (format json) SELECT * FROM a")
    );
    assert_eq!(
        a,
        json_text(
            &core,
            "tenant-a",
            "EXPLAIN (FORMAT TEXT, FORMAT JSON) SELECT * FROM a"
        )
    );
    // 整形は 2 スペース字下げの PostgreSQL 形式。
    assert!(
        a.starts_with("[\n  {\n    \"Plan\": {\n      \"Node Type\": "),
        "{a}"
    );
}

#[test]
fn option_errors_use_fixed_codes() {
    // SQL-76 (1)
    let (core, _g) = seeded("x76-errors");
    for (sql, want) in [
        ("EXPLAIN (FORMAT XML) SELECT * FROM a", "0A000"),
        ("EXPLAIN (FORMAT YAML) SELECT * FROM a", "0A000"),
        ("EXPLAIN (FORMAT CSV) SELECT * FROM a", "22023"),
        ("EXPLAIN (FORMAT) SELECT * FROM a", "42601"),
        ("EXPLAIN (ANALYZE) SELECT * FROM a", "0A000"),
        ("EXPLAIN (VERBOSE) SELECT * FROM a", "0A000"),
        ("EXPLAIN (BUFFERS) SELECT * FROM a", "0A000"),
        ("EXPLAIN (BOGUS) SELECT * FROM a", "42601"),
        ("EXPLAIN () SELECT * FROM a", "42601"),
        ("EXPLAIN (FORMAT JSON,) SELECT * FROM a", "42601"),
        ("EXPLAIN (FORMAT JSON SELECT * FROM a", "42601"),
        ("EXPLAIN (FORMAT JSON)", "42601"),
        // 対象文の誤りはオプションなしと同じコード。
        (
            "EXPLAIN (FORMAT JSON) SELECT * FROM missing",
            &code(&core, "EXPLAIN SELECT * FROM missing"),
        ),
        // オプション群の繰り返しは構文誤り（再帰の増幅を防ぐ）。
        (
            "EXPLAIN (FORMAT TEXT) (FORMAT TEXT) SELECT * FROM a",
            "42601",
        ),
        (
            "EXPLAIN (FORMAT JSON) SELECT * FROM a WHERE k = $1",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    let many = vec!["FORMAT TEXT"; 33].join(", ");
    assert_eq!(
        code(&core, &format!("EXPLAIN ({many}) SELECT * FROM a")),
        "42601"
    );
}

#[test]
fn parenthesized_set_operation_is_not_an_option_list() {
    let (core, _g) = seeded("x76-paren");
    let plain = text_lines(
        &core,
        "tenant-a",
        "EXPLAIN (SELECT k FROM a UNION SELECT k FROM b) INTERSECT SELECT k FROM c",
    );
    assert!(!plain.is_empty());
}

#[test]
fn explain_json_has_no_side_effects() {
    let (core, _g) = seeded("x76-sidefx");
    let snap = |c: &EngineCore| format!("{:?}", run(c, "tenant-a", "SELECT * FROM a").unwrap());
    let before = snap(&core);
    for sql in [
        "EXPLAIN (FORMAT JSON) DELETE FROM a WHERE k > 0",
        "EXPLAIN (FORMAT JSON) UPDATE a SET v = 'z' WHERE k > 0",
        "EXPLAIN (FORMAT JSON) DELETE FROM a WHERE k > 0 USING OPERATION_ID 'p' PARTITIONED CHUNK 2",
    ] {
        json_text(&core, "tenant-a", sql);
    }
    assert_eq!(before, snap(&core));
}

#[test]
fn describe_returns_json_column_without_executing() {
    let (core, _g) = seeded("x76-describe");
    let session = SessionState::default();
    for (sql, want_json) in [
        ("EXPLAIN (FORMAT JSON) SELECT * FROM a", true),
        ("EXPLAIN (FORMAT JSON) DELETE FROM a WHERE k > 1", true),
        ("EXPLAIN (FORMAT TEXT) SELECT * FROM a", false),
        ("EXPLAIN SELECT * FROM a", false),
    ] {
        let prepared = core
            .parse_sql_prepared_with_declared_in_session(&session, sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let parsed = prepared.parsed_if_unparameterized().expect("no params");
        let cols = core
            .describe_parsed_in_session(&session, parsed)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
            .expect("columns");
        let is_json = matches!(
            cols.as_slice(),
            [ColumnMeta::Computed {
                ty: Some(engine::catalog::ColumnType::Json),
                ..
            }]
        );
        assert_eq!(is_json, want_json, "{sql}: {cols:?}");
    }
}

#[test]
fn json_in_explicit_transaction_matches_autocommit() {
    let (core, _g) = seeded("x76-txn");
    let auto = json_text(&core, "tenant-a", "EXPLAIN (FORMAT JSON) SELECT * FROM a");
    let mut session = SessionState::default();
    let c = ctx("tenant-a");
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .unwrap();
    let out = core
        .execute_sql_in_txn(
            &c,
            &mut session,
            &mut txn,
            "EXPLAIN (FORMAT JSON) SELECT * FROM a",
        )
        .unwrap();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .unwrap();
    let SqlOutcome::Explain(r) = out else {
        panic!("expected Explain");
    };
    assert!(matches!(&r.rows[0].cells[0], Cell::Json(s) if *s == auto));
}

#[test]
fn explain_json_requires_a_session() {
    // セッションなしの入口では従来どおり EXPLAIN は拒否される。
    let (core, _g) = seeded("x76-nosession");
    let plain = core.execute_sql(&ctx("tenant-a"), "EXPLAIN SELECT * FROM a");
    let json = core.execute_sql(&ctx("tenant-a"), "EXPLAIN (FORMAT JSON) SELECT * FROM a");
    assert!(plain.is_err());
    assert_eq!(
        plain.unwrap_err().wire_code(),
        json.unwrap_err().wire_code()
    );
}

#[test]
fn tenant_schema_keys_never_appear_in_json() {
    // 内部キー（`#<id>.`）は JSON でも表示名へ戻る（または現れない）。
    let (core, _g) = seeded("x76-schema");
    exec_as(&core, "tenant-a", "CREATE SCHEMA s1");
    exec_as(&core, "tenant-a", "CREATE TABLE s1.t (k BIGINT)");
    for body in [
        "SELECT * FROM s1.t",
        "SELECT * FROM s1.t x JOIN a ON x.k = a.k",
        "DELETE FROM s1.t WHERE k > 1",
    ] {
        let json = json_text(&core, "tenant-a", &format!("EXPLAIN (FORMAT JSON) {body}"));
        parse_json(&json).expect("valid JSON");
        assert!(!json.contains('#'), "{body}: {json}");
        assert_json_matches_text(&core, body);
    }
}

/// 一時表に対する `EXPLAIN (FORMAT JSON)` も TEXT と同様に受理され、JSON 1 行を返す
/// （SQL-76 (1)(6)。一時表の文は parse 時に一時コアへ振り分けられる）。
#[test]
fn explain_json_accepts_temp_table() {
    let (core, _g) = seeded("x76-temp");
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("tenant-a");
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (a BIGINT)")
        .expect("create temp");
    let text = core
        .execute_sql_in_session(&c, &mut session, "EXPLAIN SELECT a FROM tt")
        .expect("text explain");
    let json = core
        .execute_sql_in_session(&c, &mut session, "EXPLAIN (FORMAT JSON) SELECT a FROM tt")
        .expect("json explain on temp table must be accepted");
    let (SqlOutcome::Explain(t) | SqlOutcome::Query(t)) = text else {
        panic!("expected explain result");
    };
    let (SqlOutcome::Explain(j) | SqlOutcome::Query(j)) = json else {
        panic!("expected explain result");
    };
    assert!(!t.rows.is_empty());
    assert_eq!(j.rows.len(), 1);
    assert!(matches!(&j.rows[0].cells[0], Cell::Json(_)));
}

/// 一時表に対する準備済みの `EXPLAIN (FORMAT JSON)` も振り分けられる（`0A000` にならない）。
#[test]
fn explain_json_on_temp_table_is_routable_when_prepared() {
    let (core, _g) = seeded("x76-temp-prep");
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("tenant-a");
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (a BIGINT)")
        .expect("create temp");
    core.parse_sql_prepared_with_declared_in_session(
        &session,
        "EXPLAIN (FORMAT JSON) SELECT a FROM tt",
        &[],
    )
    .expect("prepare must not be rejected with 0A000");
}
