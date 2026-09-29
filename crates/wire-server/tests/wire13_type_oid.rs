//! `RowDescription` の型 OID 写像の拡張（WIRE-13・TASK-200・Issue #895）が
//! 実際の wire 経路（生バイトクライアント。`tests/common`）で観測できることを
//! 検証する層 A 結合テスト。
//!
//! 単体レベルの写像（`column_wire_type`／`WireType::oid`／`typlen`／
//! `pg_type_name`）は `crates/wire-server/src/result_encoder.rs` の
//! `mod tests` が既に固定しているため、本ファイルは「実際に TCP 経由で
//! 送出される `RowDescription` バイト列」と「値のテキスト表現が変更前と
//! 一致すること」「RLS 相当のテナント境界に影響しないこと」の 3 点に
//! 範囲を絞る（`wire14_binary_format.rs`・`wire1_simple_query.rs` と同方針）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{ArrayValue, Value};
use engine::storage::{Storage, Visibility};
use engine::uuid::parse_uuid_text;

use common::*;

/// 本テストが対象とする列（集計・式列は Issue #1173 の `aggregate_and_expression_columns_*`
/// が別途固定する）。`(列名, 期待 OID, 期待 typlen)`。
/// `WireType` の `oid()`／`typlen()` とは独立に、このテスト側だけで表を
/// 持つ（`result_encoder::WireType` は `pub(crate)` のため crate 外からは
/// 参照できない――独立表との一致自体が「単一情報源を共有する」契約の
/// 観測可能な検証になる）。
fn expected_types() -> Vec<(&'static str, i32, i16)> {
    vec![
        ("id", 1700, -1),
        ("embedding", 25, -1),
        ("flag", 16, 1),
        ("n", 23, 4),
        ("b", 20, 8),
        ("r", 700, 4),
        ("d", 701, 8),
        ("day", 1082, 4),
        ("ts", 1114, 8),
        ("blob", 17, -1),
        ("uid", 2950, 16),
        ("doc", 114, -1),
        ("docb", 3802, -1),
        ("price", 1700, -1),
        ("tags", 25, -1),
        ("mood", 25, -1),
    ]
}

/// 全対象型の列を持つ `typed_probe` テーブルを 1 行だけ投入した `EngineCore`
/// を新設する。書き込みは engine 型付き API（`tenant::insert_typed_row`）を
/// 直接使い、SQL リテラル構文の正しさには依存しない（本テストの関心は
/// `RowDescription` の型公告であり、リテラルパースは各型専用の
/// `wire_*_column.rs` が別途固定済み）。
///
/// `id=1` は tenant-a の `Visibility::Private` 行とする。wire 認証経由の
/// `PolicyContext` は自テナントの `Private` を可視にする（RLS-11・
/// TASK-195。read-your-writes）ため、alice（tenant-a）には見え、bob
/// （tenant-b）には見えない——本テストの RLS 回帰チェックに使う。
fn new_core_typed_probe() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire13-type-oid");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    let tags = ArrayType::new(ArrayElemType::Text, 4).expect("valid array type");

    storage
        .create_table(&TableSchema::new(
            "typed_probe",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("flag", ColumnType::Boolean, false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("r", ColumnType::Real, false),
                ColumnDef::new("d", ColumnType::Double, false),
                ColumnDef::new("day", ColumnType::Date, false),
                ColumnDef::new("ts", ColumnType::Timestamp, false),
                ColumnDef::new("blob", ColumnType::Bytea, false),
                ColumnDef::new("uid", ColumnType::Uuid, false),
                ColumnDef::new("doc", ColumnType::Json, false),
                ColumnDef::new("docb", ColumnType::Jsonb, false),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    false,
                ),
                ColumnDef::new("tags", ColumnType::Array(tags), false),
                ColumnDef::new("mood", ColumnType::Enum(mood), false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let op_id = OperationId::parse("op-wire13-typed-probe-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        "typed_probe",
        &ctx,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Bool(true),
            Value::Integer(42),
            Value::BigInt(9_000_000_000),
            Value::Real(1.5),
            Value::Double(2.5),
            Value::Date(0),
            Value::Timestamp(0),
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            Value::Uuid(
                parse_uuid_text("12345678-9abc-def0-1234-56789abcdef0").expect("valid uuid"),
            ),
            Value::Json("{\"a\":1}".to_string()),
            Value::Json("{\"a\":1}".to_string()),
            Value::Numeric(Decimal::from_parts(12345, 2).expect("valid decimal")),
            Value::Array(ArrayValue::Text(vec![
                Some("a".to_string()),
                Some("b".to_string()),
            ])),
            Value::Enum("happy".to_string()),
        ],
        &op_id,
    )
    .expect("insert typed row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

const SELECT_ALL: &str = "SELECT id, embedding, flag, n, b, r, d, day, ts, blob, uid, doc, docb, price, tags, mood FROM typed_probe LIMIT 10";

/// 受入基準 1〜3: 新規スカラー型が PostgreSQL 組み込み OID・typlen で公告され、
/// 既存の `id`（`numeric`）・`VECTOR`（`text`）の公告は不変であることを、
/// 実際の `RowDescription` バイト列から固定する。
#[test]
fn row_description_announces_builtin_oids_for_all_typed_columns() {
    let (core, _guard) = new_core_typed_probe();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, SELECT_ALL);
    let columns = read_row_description_with_types(&mut stream);
    let expected = expected_types();
    assert_eq!(columns.len(), expected.len());
    for ((name, oid, typlen), (expected_name, expected_oid, expected_typlen)) in
        columns.iter().zip(expected.iter())
    {
        assert_eq!(name, expected_name);
        assert_eq!(oid, expected_oid, "oid mismatch for column={name}");
        assert_eq!(typlen, expected_typlen, "typlen mismatch for column={name}");
    }

    let _row = read_data_row(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 値の往復（`DataRow` のテキスト表現）が本 Issue の前後で不変であることを
/// 固定する（変わるのは `RowDescription` の OID・typlen のみで、値表現は
/// 各専用 `wire_*_column.rs` が定める既存契約のまま）。
#[test]
fn data_row_text_values_are_unchanged_by_new_oid_announcements() {
    let (core, _guard) = new_core_typed_probe();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, SELECT_ALL);
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    let names = [
        "id",
        "embedding",
        "flag",
        "n",
        "b",
        "r",
        "d",
        "day",
        "ts",
        "blob",
        "uid",
        "doc",
        "docb",
        "price",
        "tags",
        "mood",
    ];
    assert_eq!(row.len(), names.len());
    let by_name = |name: &str| -> Option<String> {
        names
            .iter()
            .position(|n| *n == name)
            .and_then(|i| row.get(i).cloned().flatten())
    };
    assert_eq!(by_name("id").as_deref(), Some("1"));
    assert_eq!(by_name("embedding").as_deref(), Some("[1,0]"));
    assert_eq!(by_name("flag").as_deref(), Some("t"));
    assert_eq!(by_name("n").as_deref(), Some("42"));
    assert_eq!(by_name("b").as_deref(), Some("9000000000"));
    assert_eq!(by_name("r").as_deref(), Some("1.5"));
    assert_eq!(by_name("d").as_deref(), Some("2.5"));
    assert_eq!(by_name("day").as_deref(), Some("1970-01-01"));
    assert_eq!(by_name("ts").as_deref(), Some("1970-01-01 00:00:00"));
    assert_eq!(by_name("blob").as_deref(), Some("\\xdeadbeef"));
    assert_eq!(
        by_name("uid").as_deref(),
        Some("12345678-9abc-def0-1234-56789abcdef0")
    );
    assert_eq!(by_name("doc").as_deref(), Some("{\"a\":1}"));
    assert_eq!(by_name("docb").as_deref(), Some("{\"a\":1}"));
    assert_eq!(by_name("price").as_deref(), Some("123.45"));
    assert_eq!(by_name("tags").as_deref(), Some("{a,b}"));
    assert_eq!(by_name("mood").as_deref(), Some("happy"));

    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// RLS 回帰保護: 型 OID 公告の拡張がテナント境界に影響しないことを固定する。
/// `id=1` は tenant-a の `Private` 行のため、他テナント（tenant-b）からの
/// 同一 `SELECT` は `RowDescription`（型公告）は同一のまま `DataRow` が
/// 0 件になることを確認する。
#[test]
fn other_tenant_sees_same_row_description_but_zero_rows() {
    let (core, _guard) = new_core_typed_probe();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    send_simple_query(&mut stream, SELECT_ALL);
    let columns = read_row_description_with_types(&mut stream);
    let expected = expected_types();
    assert_eq!(columns.len(), expected.len());
    for ((name, oid, typlen), (expected_name, expected_oid, expected_typlen)) in
        columns.iter().zip(expected.iter())
    {
        assert_eq!(name, expected_name);
        assert_eq!(oid, expected_oid, "oid mismatch for column={name}");
        assert_eq!(typlen, expected_typlen, "typlen mismatch for column={name}");
    }

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "other tenant must not see the Private row");
    read_ready_for_query(&mut stream);
}
/// 集計・式列（Issue #1173・WIRE-13）の `RowDescription` OID を入力型に応じて
/// 固定するための SQL。列の別名と `expected_aggregate_oids` が 1 対 1 対応する。
const AGGREGATE_SQL: &str = "SELECT COUNT(*) AS c_all, SUM(n) AS s_n, SUM(b) AS s_b, \
     AVG(n) AS a_n, MIN(n) AS m_n, MAX(b) AS x_b, SUM(r) AS s_r, MIN(r) AS m_r, \
     AVG(r) AS a_r, SUM(d) AS s_d, SUM(price) AS s_p, AVG(price) AS a_p, \
     MIN(day) AS m_day, MAX(ts) AS x_ts, SUM(id) AS s_id, AVG(id) AS a_id \
     FROM typed_probe";

fn expected_aggregate_oids() -> Vec<(&'static str, i32)> {
    vec![
        ("c_all", 20),
        ("s_n", 20),
        ("s_b", 20),
        ("a_n", 701),
        ("m_n", 23),
        ("x_b", 20),
        ("s_r", 700),
        ("m_r", 700),
        ("a_r", 701),
        ("s_d", 701),
        ("s_p", 1700),
        ("a_p", 1700),
        ("m_day", 1082),
        ("x_ts", 1114),
        ("s_id", 1700),
        ("a_id", 701),
    ]
}

#[test]
fn aggregate_and_expression_columns_announce_typed_oids() {
    let (core, _guard) = new_core_typed_probe();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, AGGREGATE_SQL);
    let columns = read_row_description_with_oids(&mut stream);
    let expected = expected_aggregate_oids();
    assert_eq!(columns.len(), expected.len());
    for ((name, oid), (expected_name, expected_oid)) in columns.iter().zip(expected.iter()) {
        assert_eq!(name, expected_name);
        assert_eq!(oid, expected_oid, "oid mismatch for column={name}");
    }
    let row = read_data_row(&mut stream);
    // 値: 1 行のみ（tenant-a の Private 行）。`SUM(REAL)` は real で、`1.5` は
    // f32 の最短往復表記で出る。
    assert_eq!(row.first().cloned().flatten().as_deref(), Some("1"));
    assert_eq!(row.get(6).cloned().flatten().as_deref(), Some("1.5"));
    assert_eq!(row.get(8).cloned().flatten().as_deref(), Some("1.5"));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 式列: `vec_norm`（スカラー式）→ float8、ベクトル式 → text（静的型なし）。
    send_simple_query(
        &mut stream,
        "SELECT id, vec_norm(embedding) AS nrm, vec_div(embedding, 2.0) AS half \
         FROM typed_probe LIMIT 10",
    );
    let columns = read_row_description_with_oids(&mut stream);
    assert_eq!(
        columns,
        vec![
            ("id".to_string(), 1700),
            ("nrm".to_string(), 701),
            ("half".to_string(), 25),
        ]
    );
    let _row = read_data_row(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// テナント境界の非退行（Issue #1173）: 他テナントの集計は型公告が同一のまま、
/// 自テナントの `Private` 行を含まない結果（`COUNT` = 0・`SUM` = NULL）になる。
#[test]
fn other_tenant_aggregate_has_same_oids_but_sees_no_rows() {
    let (core, _guard) = new_core_typed_probe();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    send_simple_query(&mut stream, AGGREGATE_SQL);
    let columns = read_row_description_with_oids(&mut stream);
    let expected = expected_aggregate_oids();
    assert_eq!(columns.len(), expected.len());
    for ((name, oid), (expected_name, expected_oid)) in columns.iter().zip(expected.iter()) {
        assert_eq!(name, expected_name);
        assert_eq!(oid, expected_oid, "oid mismatch for column={name}");
    }
    let row = read_data_row(&mut stream);
    assert_eq!(row.first().cloned().flatten().as_deref(), Some("0"));
    assert_eq!(
        row.get(1).cloned().flatten(),
        None,
        "SUM over no rows is NULL"
    );
    assert_eq!(
        row.get(6).cloned().flatten(),
        None,
        "SUM(REAL) over no rows is NULL"
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}
