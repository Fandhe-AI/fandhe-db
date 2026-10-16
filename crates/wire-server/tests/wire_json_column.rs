//! `JSON`／`JSONB` 列型（TABLE-14・TASK-198、Issue #889）の wire・NoSQL 結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-198・`docs/spec/04-behavior/
//! nosql.md` NOSQL-8・NOSQL-17。
//!
//! `crates/engine/tests/json_column.rs`（engine API 直接）とは独立に、
//! **wire フレーミング越し**の観測に徹する（`wire_bytea_column.rs` と同じ
//! 役割分担）:
//! - SQL 表層（生バイトクライアント）: テキスト表現の往復一致・バイナリ
//!   形式指定時の非対応拒否（`0A000`）
//! - NoSQL 表層（HTTP）: native JSON 表現での insert/update・SQL/NoSQL 間
//!   パリティ（JSONB は正規化のため一致・JSON は非対称）・拒否経路

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
        ],
    )
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-json-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

// --- SQL 表層: テキスト表現の往復 ----------------------------------------------

#[test]
fn wire_text_select_returns_stored_text_and_round_trips() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, doc, docb) VALUES (1, '[0.1,0.2,0.3]', '{"b":2,"a":1}', '{"b":2,"a":1}') USING OPERATION_ID 'op-insert-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT doc, docb FROM docs WHERE id = 1 LIMIT 1",
    );
    common::read_row_description(&mut alice);
    let row = common::read_data_row(&mut alice);
    // `doc`（JSON）は入力テキストをそのまま保持、`docb`（JSONB）はキー順が
    // 辞書順へ正規化される。
    assert_eq!(
        row,
        vec![
            Some(r#"{"b":2,"a":1}"#.to_string()),
            Some(r#"{"a":1,"b":2}"#.to_string()),
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // 読み出したテキストをそのまま再度 INSERT すると、同じ格納テキストになる
    // （wire テキスト表現の往復一致）。
    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, doc, docb) VALUES (2, '[0.1,0.2,0.3]', '{"b":2,"a":1}', '{"a":1,"b":2}') USING OPERATION_ID 'op-insert-2'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT doc, docb FROM docs WHERE id = 2 LIMIT 1",
    );
    common::read_row_description(&mut alice);
    let row2 = common::read_data_row(&mut alice);
    assert_eq!(row2, row);
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);
}

/// Issue #1596（TABLE-25）: jsonb のパス演算子の結果列は、`->`・`#>` が jsonb（OID 3802）、
/// `->>`・`#>>` が text（OID 25）として RowDescription に載り、値は text 形式で返る。
#[test]
fn wire_json_path_operators_advertise_jsonb_and_text_oids() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, docb) VALUES (1, '[0.1,0.2,0.3]', '{"a":{"b":[1,"x"]}}') USING OPERATION_ID 'op-path-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT docb -> 'a' AS j, docb -> 'a' ->> 'zz' AS t, docb #> '{a,b,1}' AS pj, docb #>> '{a,b,1}' AS pt \
         FROM docs WHERE id = 1 LIMIT 1",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(
        columns,
        vec![
            ("j".to_string(), 3802),
            ("t".to_string(), 25),
            ("pj".to_string(), 3802),
            ("pt".to_string(), 25),
        ]
    );
    let row = common::read_data_row(&mut alice);
    assert_eq!(
        row,
        vec![
            Some(r#"{"b":[1,"x"]}"#.to_string()),
            None,
            Some(r#""x""#.to_string()),
            Some("x".to_string()),
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);
}

/// Issue #2181（TABLE-25）: 連結・削除演算子の結果列は jsonb（OID 3802）として RowDescription に載り、
/// 値は正規化テキスト。スカラーからの削除は 22023、json 列への適用は 42883 の ErrorResponse。
#[test]
fn wire_jsonb_modify_operators_advertise_jsonb_oid_and_map_errors() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    for (id, doc) in [(1, r#"{"a":1,"b":[1,2]}"#), (2, "5")] {
        common::send_simple_query(
            &mut alice,
            &format!(
                "INSERT INTO docs (id, embedding, docb, doc) VALUES ({id}, '[0.1,0.2,0.3]', '{doc}', '{{}}') USING OPERATION_ID 'op-m-{id}'"
            ),
        );
        assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
        common::read_ready_for_query(&mut alice);
    }

    common::send_simple_query(
        &mut alice,
        "SELECT docb || '{\"c\":3}' AS cc, docb - 'a' AS dk, (docb -> 'b') - 0 AS di, docb #- '{b,1}' AS dp \
         FROM docs WHERE id = 1 LIMIT 1",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(
        columns,
        vec![
            ("cc".to_string(), 3802),
            ("dk".to_string(), 3802),
            ("di".to_string(), 3802),
            ("dp".to_string(), 3802),
        ]
    );
    assert_eq!(
        common::read_data_row(&mut alice),
        vec![
            Some(r#"{"a":1,"b":[1,2],"c":3}"#.to_string()),
            Some(r#"{"b":[1,2]}"#.to_string()),
            Some("[2]".to_string()),
            Some(r#"{"a":1,"b":[1]}"#.to_string()),
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT docb - 'a' FROM docs WHERE id = 2 LIMIT 1",
    );
    common::expect_error_response_with_sqlstate(&mut alice, "22023");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT doc - 'a' FROM docs WHERE id = 1 LIMIT 1",
    );
    common::expect_error_response_with_sqlstate(&mut alice, "42883");
    common::read_ready_for_query(&mut alice);
}

/// Issue #1597（TABLE-25）: 簡易クエリで jsonb の述語（暗黙の jsonb リテラル解釈）と
/// ORDER BY（式キー）が往復し、RowDescription は OID 3802・値は正規化テキストで返る。
#[test]
fn wire_jsonb_where_and_order_by_round_trip() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    for (id, doc) in [
        (1, r#"{"k":3}"#),
        (2, r#"{"k":1.0}"#),
        (3, r#"{"k":2}"#),
        (4, r#"{"k":"s"}"#),
    ] {
        common::send_simple_query(
            &mut alice,
            &format!(
                "INSERT INTO docs (id, embedding, docb) VALUES ({id}, '[0.1,0.2,0.3]', '{doc}') USING OPERATION_ID 'op-j-{id}'"
            ),
        );
        assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
        common::read_ready_for_query(&mut alice);
    }

    common::send_simple_query(
        &mut alice,
        "SELECT docb -> 'k' AS v FROM docs WHERE docb -> 'k' >= '1' ORDER BY docb -> 'k' DESC LIMIT 10",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(columns, vec![("v".to_string(), 3802)]);
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(common::read_data_row(&mut alice));
    }
    assert_eq!(
        got,
        vec![
            vec![Some("3".to_string())],
            vec![Some("2".to_string())],
            vec![Some("1.0".to_string())],
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);
}

/// Issue #1598（TABLE-25）: 包含・存在演算子の結果列は bool（OID 16）として RowDescription に載り、
/// 値は `t`／`f`、NULL 列は NULL。WHERE の裸の述語も簡易クエリで往復する。
#[test]
fn wire_jsonb_containment_operators_advertise_bool_oid_and_filter() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    for (id, doc) in [(1, r#"{"a":1,"b":[1,2]}"#), (2, r#"["a","c"]"#)] {
        common::send_simple_query(
            &mut alice,
            &format!(
                "INSERT INTO docs (id, embedding, docb) VALUES ({id}, '[0.1,0.2,0.3]', '{doc}') USING OPERATION_ID 'op-c-{id}'"
            ),
        );
        assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
        common::read_ready_for_query(&mut alice);
    }
    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding) VALUES (3, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-c-3'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT docb @> '{\"a\":1}' AS c, docb <@ '{\"a\":1,\"b\":[1,2],\"z\":0}' AS cb, docb ? 'a' AS e, \
         docb ?| '{x,b}' AS ea, docb ?& '{a,b}' AS eb FROM docs WHERE id = 1 LIMIT 10",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(
        columns,
        vec![
            ("c".to_string(), 16),
            ("cb".to_string(), 16),
            ("e".to_string(), 16),
            ("ea".to_string(), 16),
            ("eb".to_string(), 16),
        ]
    );
    let t = || Some("t".to_string());
    assert_eq!(
        common::read_data_row(&mut alice),
        vec![t(), t(), t(), t(), t()]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // NULL 列（id = 3）は NULL。
    common::send_simple_query(
        &mut alice,
        "SELECT docb ? 'a' AS e FROM docs WHERE id = 3 LIMIT 10",
    );
    common::read_row_description(&mut alice);
    assert_eq!(common::read_data_row(&mut alice), vec![None]);
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT id FROM docs WHERE docb ? 'a' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 10",
    );
    common::read_row_description(&mut alice);
    let mut got = vec![
        common::read_data_row(&mut alice),
        common::read_data_row(&mut alice),
    ];
    got.sort();
    assert_eq!(
        got,
        vec![vec![Some("1".to_string())], vec![Some("2".to_string())]]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);
}

/// Issue #1599（TABLE-25）: jsonb 関数の結果列は `jsonb_typeof`・`jsonb_extract_path_text` が
/// text（OID 25）、`jsonb_array_length` が int4（OID 23）、jsonb を返す関数が jsonb（OID 3802）と
/// して RowDescription に載り、代表的なエラーは固定の SQLSTATE（`22023`・`22004`）で返る。
#[test]
fn wire_jsonb_functions_advertise_type_oids_and_report_sqlstates() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, docb) VALUES (1, '[0.1,0.2,0.3]', '{"a":[1,2,3]}') USING OPERATION_ID 'op-fn-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT jsonb_typeof(docb) AS t, jsonb_array_length(docb -> 'a') AS n, \
         jsonb_extract_path_text(docb, 'a', '0') AS x, jsonb_extract_path(docb, 'a') AS j, \
         jsonb_build_object('k', id) AS o, jsonb_build_array(1, NULL) AS arr, to_jsonb(id) AS tj \
         FROM docs WHERE id = 1 LIMIT 1",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(
        columns,
        vec![
            ("t".to_string(), 25),
            ("n".to_string(), 23),
            ("x".to_string(), 25),
            ("j".to_string(), 3802),
            ("o".to_string(), 3802),
            ("arr".to_string(), 3802),
            ("tj".to_string(), 3802),
        ]
    );
    assert_eq!(
        common::read_data_row(&mut alice),
        vec![
            Some("object".to_string()),
            Some("3".to_string()),
            Some("1".to_string()),
            Some("[1,2,3]".to_string()),
            Some(r#"{"k":1}"#.to_string()),
            Some("[1,null]".to_string()),
            Some("1".to_string()),
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // オブジェクトへの array_length は 22023、NULL キーは 22004。
    common::send_simple_query(
        &mut alice,
        "SELECT jsonb_array_length(docb) FROM docs WHERE id = 1 LIMIT 1",
    );
    common::expect_error_response_with_sqlstate(&mut alice, "22023");
    common::read_ready_for_query(&mut alice);
    common::send_simple_query(
        &mut alice,
        "SELECT jsonb_build_object(NULL, 1) FROM docs WHERE id = 1 LIMIT 1",
    );
    common::expect_error_response_with_sqlstate(&mut alice, "22004");
    common::read_ready_for_query(&mut alice);
}

/// Issue #2185（TABLE-25）: `jsonb_path_query_array`／`_first` の結果列は jsonb（OID 3802）、
/// `jsonb_path_exists`／`_match` は boolean（OID 16）として RowDescription に載り、jsonpath の
/// 評価エラー（`2203A`）・束縛時エラー（`22P05`・`42601`）は ErrorResponse の SQLSTATE で届く。
#[test]
fn wire_jsonb_path_functions_advertise_oids_and_report_sqlstates() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, docb) VALUES (1, '[0.1,0.2,0.3]', '{"a":[1,2,3]}') USING OPERATION_ID 'op-jp-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT jsonb_path_query_array(docb, '$.a[*]') AS qa, \
         jsonb_path_query_first(docb, '$.a[*]') AS qf, \
         jsonb_path_exists(docb, '$.a') AS ex, \
         jsonb_path_match(docb, '$.a[0] == 1') AS m \
         FROM docs WHERE id = 1 LIMIT 1",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(
        columns,
        vec![
            ("qa".to_string(), 3802),
            ("qf".to_string(), 3802),
            ("ex".to_string(), 16),
            ("m".to_string(), 16),
        ]
    );
    assert_eq!(
        common::read_data_row(&mut alice),
        vec![
            Some("[1,2,3]".to_string()),
            Some("1".to_string()),
            Some("t".to_string()),
            Some("t".to_string()),
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    for (sql, sqlstate) in [
        (
            "SELECT jsonb_path_query_array(docb, 'strict $.zzz') FROM docs WHERE id = 1 LIMIT 1",
            "2203A",
        ),
        (
            r#"SELECT jsonb_path_exists(docb, '"\u0000"') FROM docs WHERE id = 1 LIMIT 1"#,
            "22P05",
        ),
        (
            "SELECT jsonb_path_exists(docb, '$.') FROM docs WHERE id = 1 LIMIT 1",
            "42601",
        ),
    ] {
        common::send_simple_query(&mut alice, sql);
        common::expect_error_response_with_sqlstate(&mut alice, sqlstate);
        common::read_ready_for_query(&mut alice);
    }
}

/// Issue #1598: 拡張クエリプロトコルでも `?`／`?|`／`?&`／`<@`／`@>` を含む SQL が Parse できる
/// （`?` を `$n` と取り違えない）。結果は simple query と同じ `t`。
#[test]
fn wire_jsonb_containment_operators_work_over_extended_query() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, docb) VALUES (1, '[0.1,0.2,0.3]', '{"a":1,"b":2}') USING OPERATION_ID 'op-e-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    for (i, expr) in [
        "docb ? 'a'",
        "docb ?| '{z,b}'",
        "docb ?& '{a,b}'",
        "docb <@ '{\"a\":1,\"b\":2,\"c\":3}'",
        "docb @> '{\"a\":1}'",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("s{i}");
        let portal = format!("p{i}");
        let sql = format!("SELECT {expr} FROM docs WHERE id = 1 LIMIT 1");
        common::send_length_prefixed_message(&mut alice, b'P', &common::parse_body(&name, &sql, 0));
        common::send_length_prefixed_message(&mut alice, b'B', &common::bind_body(&portal, &name));
        common::send_length_prefixed_message(&mut alice, b'E', &common::execute_body(&portal, 0));
        common::send_length_prefixed_message(&mut alice, b'S', &[]);
        let mut value = None;
        loop {
            let (kind, body) = common::read_message(&mut alice);
            match kind {
                b'1' | b'2' | b'C' => {}
                b'D' => {
                    // DataRow: i16 列数、i32 長さ、値のバイト列（1 列 1 行）。
                    assert_eq!(body.get(..2), Some(&[0u8, 1][..]));
                    assert_eq!(body.get(2..6), Some(&1i32.to_be_bytes()[..]));
                    value = body.get(6..).map(|b| b.to_vec());
                }
                b'Z' => break,
                other => panic!("unexpected message {other:?} for {expr}"),
            }
        }
        assert_eq!(value, Some(b"t".to_vec()), "{expr}");
    }
}
#[test]
fn wire_text_null_and_json_null_literal_are_distinguishable() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, doc) VALUES (1, '[0.1,0.2,0.3]', 'null') \
         USING OPERATION_ID 'op-json-null'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding) VALUES (2, '[0.1,0.2,0.3]') \
         USING OPERATION_ID 'op-sql-null'",
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(&mut alice, "SELECT id, doc FROM docs LIMIT 100");
    common::read_row_description(&mut alice);
    let mut rows = Vec::new();
    for _ in 0..2 {
        rows.push(common::read_data_row(&mut alice));
    }
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // JSON の `null` は非 NULL の値（テキスト "null"）、SQL NULL は `None`
    // （-1 長）として区別される。
    let by_id: std::collections::BTreeMap<String, Option<String>> = rows
        .into_iter()
        .map(|r| {
            let id = r[0].clone().expect("id must not be NULL");
            (id, r[1].clone())
        })
        .collect();
    assert_eq!(by_id.get("1"), Some(&Some("null".to_string())));
    assert_eq!(by_id.get("2"), Some(&None));
}

// バイナリ結果形式指定時の非対応拒否（`0A000`。WIRE-14。BYTEA と同区分）は
// `wire14_binary_format.rs::json_column_binary_request_is_rejected_as_feature_not_supported`
// が `validate_binary_formats` を直接呼ぶ形で固定する（拡張クエリプロトコルの
// 生バイト構築ヘルパーがこのテストファイル群に無いため、同テストファイルの
// 既存パターン〔BYTEA・VECTOR・Computed 列と同型〕に揃えた）。

// --- NoSQL 表層: native JSON 表現の insert/update・SQL とのパリティ・拒否経路 ---

struct Both {
    http_addr: std::net::SocketAddr,
    token: String,
}

fn spawn_both(core: Arc<EngineCore>) -> (Both, std::net::TcpStream) {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr = http_common::spawn_router_listener_with_engine(
        &users_path,
        SessionStore::new(),
        core.clone(),
    );
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let sql_addr = common::spawn_server_with_engine(&users_path, core);
    let sql_stream = common::authenticate_to_ready_for_query(sql_addr, "alice", "pw-alice");

    (Both { http_addr, token }, sql_stream)
}

fn query(both: &Both, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", both.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        both.http_addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn json_body(resp: &HttpResponse) -> JsonValue {
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    parse_json(&text).expect("response body must be valid json")
}

#[test]
fn nosql_insert_native_json_round_trips_through_scan() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"doc":{"a":1,"b":[1,2,3]}}],"operation_id":"op-nosql-insert-1"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "insert must succeed: {resp:?}");

    let scan_body = br#"{"op":"scan","table":"docs","columns":["doc"],"limit":100}"#;
    let resp = query(&both, scan_body);
    assert_eq!(resp.status, 200, "scan must succeed: {resp:?}");
    let JsonValue::Object(obj) = json_body(&resp) else {
        panic!("expected object body");
    };
    let JsonValue::Array(rows) = obj.get("rows").expect("rows field").clone() else {
        panic!("expected rows array");
    };
    assert_eq!(rows.len(), 1);
    let JsonValue::Array(row0) = &rows[0] else {
        panic!("expected row array");
    };
    // native JSON 値として返る（base64 化されない）。
    match &row0[0] {
        JsonValue::Object(m) => {
            assert_eq!(
                m.get("a"),
                Some(&JsonValue::Number(engine::json::JsonNumber::PosInt(1)))
            );
        }
        other => panic!("expected JSON object, got {other:?}"),
    }
}

#[test]
fn nosql_update_set_jsonb_matches_sql_read_back() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"op-seed"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");

    let update_body = br#"{"op":"update","table":"docs","where":{"id":1},"set":{"docb":{"b":2,"a":1}},"operation_id":"op-nosql-update-1"}"#;
    let resp = query(&both, update_body);
    assert_eq!(resp.status, 200, "update must succeed: {resp:?}");

    common::send_simple_query(&mut sql, "SELECT docb FROM docs WHERE id = 1 LIMIT 1");
    common::read_row_description(&mut sql);
    let row = common::read_data_row(&mut sql);
    // JSONB は正規化されるため、NoSQL のキー順（b,a）に関わらず辞書順で
    // SQL 表層から読み出せる。
    assert_eq!(row, vec![Some(r#"{"a":1,"b":2}"#.to_string())]);
    common::read_command_complete(&mut sql);
    common::read_ready_for_query(&mut sql);
}

/// nullable な `JSON`／`JSONB` 列は NoSQL `update` op から JSON `null` で
/// SQL `NULL` へ更新できる（PR #1014 レビュー指摘対応。INSERT 経路
/// （`nosql_insert_native_json_round_trips_through_scan` 等）・design doc
/// `docs/design/column-type-extension.md`「#889 追記」節「JSON `null` は
/// nullable 列なら `NULL`」と表層間・操作間で契約を揃える）。
#[test]
fn nosql_update_set_json_null_on_nullable_column_clears_to_sql_null() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"doc":{"a":1},"docb":{"a":1}}],"operation_id":"op-seed-null"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");

    let update_body = br#"{"op":"update","table":"docs","where":{"id":1},"set":{"doc":null,"docb":null},"operation_id":"op-nosql-update-null"}"#;
    let resp = query(&both, update_body);
    assert_eq!(resp.status, 200, "update to NULL must succeed: {resp:?}");

    common::send_simple_query(&mut sql, "SELECT doc, docb FROM docs WHERE id = 1 LIMIT 1");
    common::read_row_description(&mut sql);
    let row = common::read_data_row(&mut sql);
    assert_eq!(row, vec![None, None]);
    common::read_command_complete(&mut sql);
    common::read_ready_for_query(&mut sql);
}

/// 非 nullable な `JSON`／`JSONB` 列への `null` は fail-closed に拒否される
/// （Issue #896 で判定順序が変わり、wire 層の型不一致検査（`42601`）ではなく
/// `bind_update`（engine 側の単一情報源）の nullable 判定（`22000`）で拒否
/// されるようになった。`docs/design/nosql-typed-json-binding.md`「null の
/// 扱い」節参照。拒否されること自体・fail-closed であることは不変）。
#[test]
fn nosql_update_set_json_null_on_non_nullable_column_is_rejected() {
    let path = temp_db::unique_db_path("wire-json-column-non-nullable");
    let guard = temp_db::CleanupGuard(path.clone());
    let non_nullable_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("doc", ColumnType::Json, false),
        ],
    );
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&non_nullable_schema)
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let (both, _sql) = spawn_both(core);
    let _guard = guard;

    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"doc":{"a":1}}],"operation_id":"op-seed-non-nullable"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");

    let update_body = br#"{"op":"update","table":"docs","where":{"id":1},"set":{"doc":null},"operation_id":"op-nosql-update-reject-null"}"#;
    let resp = query(&both, update_body);
    // TABLE-16・TASK-204、Issue #904: NOT NULL 違反は `23502`
    // （`NotNullViolation`）へ写像する（旧 `22000` から契約変更）。
    assert_eq!(http_common::wire_code_of(&resp), "23502", "resp: {resp:?}");
}

#[test]
fn nosql_insert_rejects_scalar_json_and_malformed_object_with_42601() {
    let (core, _guard) = new_core();
    let (both, _sql) = spawn_both(core);

    for (label, doc_json) in [
        ("number", "123"),
        ("string", "\"hello\""),
        ("boolean", "true"),
    ] {
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[0.1,0.2,0.3],"doc":{doc_json}}}],"operation_id":"op-reject-{label}"}}"#
        );
        let resp = query(&both, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "case: {label}, resp: {resp:?}"
        );
    }
}

// --- SQL⇄NoSQL 表層横断の operation_id パリティ（JSONB 正規化・JSON 非対称） ---

#[test]
fn json_operation_id_resend_via_nosql_after_sql_seed_is_content_mismatch_when_whitespace_differs() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);

    // SQL 経由で非正規の空白を含む JSON テキストを書き込む。
    common::send_simple_query(
        &mut sql,
        r#"INSERT INTO docs (id, embedding, doc) VALUES (1, '[0.1,0.2,0.3]', '{"a":1}') USING OPERATION_ID 'op-cross'"#,
    );
    assert_eq!(common::read_command_complete(&mut sql), "INSERT 0 1");
    common::read_ready_for_query(&mut sql);

    // NoSQL 経由で同一 operation_id を再送する。NoSQL 側は常に正規化テキスト
    // （空白なし）を格納するため、SQL 側が書いた同じ意味内容でも
    // テキスト表現としては一致し内容一致（`23505`）になる（JSON 列は SQL 表層が
    // 書いた入力テキストがすでに正規形〔空白なし〕だったケース）。
    let insert_body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"doc":{"a":1}}],"operation_id":"op-cross"}"#;
    let resp = query(&both, insert_body);
    assert_eq!(http_common::wire_code_of(&resp), "23505", "resp: {resp:?}");
}

// --- Issue #2180（TABLE-25）: `json`（原文保存）の演算子・関数と重複キー -----------

/// `json` 列のパス演算子・関数の結果列は、`->`・`#>`・`to_json` が json（OID 114）、`->>`・
/// `#>>` が text（OID 25）、`json_array_length` が int4（OID 23）として RowDescription に載り、
/// 値は原文の部分テキスト（空白・数値表記・重複キーの最後の値）で返る。
#[test]
fn wire_json_text_operators_advertise_json_oids_and_return_original_text() {
    let (core, _guard) = new_core();
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = common::spawn_server_with_engine(&users_path, core);
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut alice,
        r#"INSERT INTO docs (id, embedding, doc) VALUES (1, '[0.1,0.2,0.3]', ' {"a" : [ 1 , 2 ], "a" : [ 3 , 4 ], "d":1.0e2} ') USING OPERATION_ID 'op-json-text-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
    common::read_ready_for_query(&mut alice);

    common::send_simple_query(
        &mut alice,
        "SELECT doc -> 'a' AS j, doc ->> 'd' AS t, doc #> '{a,0}' AS pj, doc #>> '{a,1}' AS pt, \
         json_array_length(doc -> 'a') AS n, to_json(doc -> 'd') AS tj \
         FROM docs WHERE id = 1 LIMIT 1",
    );
    let columns = common::read_row_description_with_oids(&mut alice);
    assert_eq!(
        columns,
        vec![
            ("j".to_string(), 114),
            ("t".to_string(), 25),
            ("pj".to_string(), 114),
            ("pt".to_string(), 25),
            ("n".to_string(), 23),
            ("tj".to_string(), 114),
        ]
    );
    let row = common::read_data_row(&mut alice);
    assert_eq!(
        row,
        vec![
            Some("[ 3 , 4 ]".to_string()),
            Some("1.0e2".to_string()),
            Some("3".to_string()),
            Some("4".to_string()),
            Some("2".to_string()),
            Some("1.0e2".to_string()),
        ]
    );
    common::read_command_complete(&mut alice);
    common::read_ready_for_query(&mut alice);

    // jsonb 専用演算子は json に対して `42883`。
    common::send_simple_query(&mut alice, "SELECT id FROM docs WHERE doc @> '{}' LIMIT 1");
    common::expect_error_response_with_sqlstate(&mut alice, "42883");
    common::read_ready_for_query(&mut alice);
}

/// 重複キーを含む `json` 行を NoSQL scan で読むと 200 になり、応答は重複キーを最後の値へ畳んだ
/// 正規形になる（生テキストは連結しない。SQL は原文を返す）。
#[test]
fn nosql_scan_of_json_row_with_duplicate_keys_returns_last_value() {
    let (core, _guard) = new_core();
    let (both, mut sql) = spawn_both(core);

    common::send_simple_query(
        &mut sql,
        r#"INSERT INTO docs (id, embedding, doc) VALUES (1, '[0.1,0.2,0.3]', '{"a":1, "a" : 2}') USING OPERATION_ID 'op-dup-1'"#,
    );
    assert_eq!(common::read_command_complete(&mut sql), "INSERT 0 1");
    common::read_ready_for_query(&mut sql);

    let scan_body = br#"{"op":"scan","table":"docs","columns":["doc"],"limit":100}"#;
    let resp = query(&both, scan_body);
    assert_eq!(resp.status, 200, "scan must succeed: {resp:?}");
    let JsonValue::Object(obj) = json_body(&resp) else {
        panic!("expected object body");
    };
    let JsonValue::Array(rows) = obj.get("rows").expect("rows field").clone() else {
        panic!("expected rows array");
    };
    let JsonValue::Array(row0) = &rows[0] else {
        panic!("expected row array");
    };
    match &row0[0] {
        JsonValue::Object(m) => {
            assert_eq!(m.len(), 1);
            assert_eq!(
                m.get("a"),
                Some(&JsonValue::Number(engine::json::JsonNumber::PosInt(2)))
            );
        }
        other => panic!("expected JSON object, got {other:?}"),
    }
}
