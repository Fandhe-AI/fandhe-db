//! 層 A 結合テスト（TASK-181・NOSQL-11。Issue #762）: `engine::core::EngineCore`
//! の実 `QueryResult`（`SELECT` 経由）を `wire_server::http::query::response::
//! encode` へ通し、`wire_server::result_encoder::encode_row_description` が
//! 公告する OID との型名一致・`row_count`・RLS 境界を公開 API のみで固定する。
//!
//! `tests/sql_scan_public_api.rs`（Issue #726）と同じ `EngineCore::from_storage`
//! セットアップの流儀を使う。本ファイルは wire-server クレート外から見える
//! 公開関数・型のみを経由する（`http::query::response::encode` の実呼び出し元＝
//! 後続の接続ハンドラ〔#747／#758 以降〕が使う形を再現する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::storage::{Storage, Visibility};
use wire_server::http::query::response::encode;
use wire_server::result_encoder::encode_row_description;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const DIM: usize = 3;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// tenant-a に公開行、tenant-b に非公開（`Visibility::Private`）行を投入する
/// （`tests/sql_scan_public_api.rs::seed_two_tenants` と同型。RLS 境界確認用）。
fn seed_two_tenants(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");

    for id in 1..=3u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-a-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }
    for id in 101..=102u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-b-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-b row");
    }
}

/// `RowDescription`（wire バイト列）から各フィールドの `(name, type_oid)` を
/// 読み取る（NOSQL-11 の「同一の対応表を共有する」契約を、`unwrap`/`expect`
/// を使わず添字アクセスもしないテスト専用パーサで検証するためのヘルパー）。
fn row_description_fields(columns: &[ColumnMeta]) -> Vec<(String, i32)> {
    let msg = encode_row_description(columns).expect("encode_row_description");
    let mut fields = Vec::new();
    // body は 'T'(1) + length(4) + field_count(2) の直後から始まる。
    let mut cursor = 1 + 4 + 2;
    for _ in columns {
        let remaining = msg.get(cursor..).expect("row description truncated");
        let name_end = remaining
            .iter()
            .position(|&b| b == 0)
            .expect("NUL terminator");
        let name_bytes = remaining.get(..name_end).expect("name bytes");
        let name = std::str::from_utf8(name_bytes)
            .expect("utf8 name")
            .to_string();
        cursor += name_end + 1; // NUL を含めて読み飛ばす
        cursor += 4 + 2; // table_oid + attnum
        let oid_bytes: [u8; 4] = msg
            .get(cursor..cursor + 4)
            .expect("type_oid bytes")
            .try_into()
            .expect("4 bytes");
        let oid = i32::from_be_bytes(oid_bytes);
        cursor += 4 + 2 + 4 + 2; // type_oid + typlen + typmod + format
        fields.push((name, oid));
    }
    fields
}

/// OID → `columns[].type` の固定表（NoSQL 表層側テストとして独立に持つ。
/// `wire_server::result_encoder::WireType` は `pub(crate)` のため crate 外から
/// は参照できない――この独立表との一致自体が「単一情報源を共有する」契約の
/// 観測可能な検証になる）。
fn expected_json_type_for_oid(oid: i32) -> &'static str {
    match oid {
        1700 => "numeric",
        25 => "text",
        23 => "int4",
        20 => "int8",
        // WIRE-13・TASK-200・Issue #895 で追加された型（この固定表を独立表
        // として維持する契約上、新型を使う fixture が追加されたら本表も
        // 更新が必要になる。忘れた場合は fail-closed に `panic!` する）。
        16 => "bool",
        700 => "float4",
        701 => "float8",
        1082 => "date",
        1114 => "timestamp",
        17 => "bytea",
        2950 => "uuid",
        114 => "json",
        3802 => "jsonb",
        other => panic!("unexpected type oid: {other}"),
    }
}

#[test]
fn row_description_oid_and_nosql_json_type_for_star_projection() {
    let path = unique_db_path("nosql11-response-schema-star");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = ctx_for("tenant-a");

    // `SELECT * FROM docs LIMIT n` は Id・Scalar(Vector)・Scalar(Text) の
    // 3 種 `ColumnMeta` を一度に得られる（`tests/sql_scan.rs::
    // scan_star_projection_includes_id_vector_and_text_cells` と同型）。
    let result = core
        .execute_sql(&ctx_a, "SELECT * FROM docs LIMIT 10")
        .expect("scan should succeed");
    assert_eq!(result.columns.len(), 3);

    let fields = row_description_fields(&result.columns);
    let body = encode(&result).expect("encode");
    let parsed = engine::json::parse_json(&body).expect("valid JSON");
    let engine::json::JsonValue::Object(top) = parsed else {
        panic!("top level must be an object");
    };
    let engine::json::JsonValue::Array(json_columns) = &top["columns"] else {
        panic!("columns must be an array");
    };
    assert_eq!(json_columns.len(), fields.len());

    // NoSQL 表層の `columns[].type`（Issue #896・NOSQL-17）は SQL wire
    // `RowDescription` の OID 写像（Issue #895）から意図的に独立しており、
    // `VECTOR` 列は SQL wire 側が `text`（OID 25、後方互換のため）で公告する
    // 一方 NoSQL 側は型情報をそのまま伝えるため `"vector"` を返す（両者は
    // ここでのみ乖離し、`id`／`TEXT` 列は引き続き一致する）。
    let expected_json_type: [&str; 3] = ["numeric", "vector", "text"];
    for (i, (name, oid)) in fields.iter().enumerate() {
        let engine::json::JsonValue::Object(col_obj) = &json_columns[i] else {
            panic!("column must be an object");
        };
        let engine::json::JsonValue::String(json_name) = &col_obj["name"] else {
            panic!("name must be a string");
        };
        assert_eq!(json_name, name, "column index={i}");
        let engine::json::JsonValue::String(json_type) = &col_obj["type"] else {
            panic!("type must be a string");
        };
        assert_eq!(
            json_type, expected_json_type[i],
            "column index={i} name={name}"
        );
        // OID 自体は Issue #895 の既存契約のまま非 vacuous であることを確認する
        // （どのケースでも既知の OID を公告する）。
        assert!(matches!(oid, 1700 | 25), "unexpected oid: {oid}");
    }
}

#[test]
fn row_description_oid_and_json_type_agree_for_computed_column() {
    let path = unique_db_path("nosql11-response-schema-computed");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = ctx_for("tenant-a");

    // `vec_norm(embedding) AS n` は組み込み UDF（`CREATE FUNCTION` 不要）による
    // `Computed` 列を生む（`tests/sql_udf_call.rs` と同型のクエリ）。
    let result = core
        .execute_sql(
            &ctx_a,
            "SELECT id, vec_norm(embedding) AS n FROM docs \
             WHERE vec_norm(embedding) > 0.0 \
             ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
        )
        .expect("select should succeed");
    assert!(
        matches!(result.columns.get(1), Some(ColumnMeta::Computed { .. })),
        "second column must be Computed: {:?}",
        result.columns
    );

    let fields = row_description_fields(&result.columns);
    let body = encode(&result).expect("encode");
    let parsed = engine::json::parse_json(&body).expect("valid JSON");
    let engine::json::JsonValue::Object(top) = parsed else {
        panic!("top level must be an object");
    };
    let engine::json::JsonValue::Array(json_columns) = &top["columns"] else {
        panic!("columns must be an array");
    };

    // Issue #1173: wire 側 `RowDescription` は式列（`vec_norm`）の静的な結果型
    // （float8＝OID 701）を公告する一方、NoSQL 表層の JSON `columns[].type` は
    // NOSQL-11 の契約に従い計算列を "text" に固定する（意図的な非対称）。
    // `id` 列（numeric 1700）は従来どおり OID と JSON 型名が一致する。
    for (i, (name, oid)) in fields.iter().enumerate() {
        let engine::json::JsonValue::Object(col_obj) = &json_columns[i] else {
            panic!("column must be an object");
        };
        let engine::json::JsonValue::String(json_type) = &col_obj["type"] else {
            panic!("type must be a string");
        };
        if name == "n" {
            assert_eq!(*oid, 701, "computed column must announce float8");
            assert_eq!(json_type, "text", "computed column JSON type is fixed");
        } else {
            assert_eq!(
                json_type,
                expected_json_type_for_oid(*oid),
                "column index={i} name={name}"
            );
        }
    }

    // `row_count` は `rows.len()` と常に一致する。
    let engine::json::JsonValue::Number(row_count) = &top["row_count"] else {
        panic!("row_count must be a number");
    };
    assert_eq!(row_count.as_f64(), result.rows.len() as f64);
}

#[test]
fn row_count_tracks_rows_len_below_and_above_limit() {
    let path = unique_db_path("nosql11-response-schema-row-count");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = ctx_for("tenant-a");

    // tenant-a の可視行は 3 件（id 1..=3）。LIMIT を行数未満・行数超過の
    // 両方で試し、`row_count == rows.len()` が常に成立することを確認する。
    for limit in [1usize, 100] {
        let result = core
            .execute_sql(&ctx_a, &format!("SELECT id FROM docs LIMIT {limit}"))
            .expect("scan should succeed");
        let body = encode(&result).expect("encode");
        let parsed = engine::json::parse_json(&body).expect("valid JSON");
        let engine::json::JsonValue::Object(top) = parsed else {
            panic!("top level must be an object");
        };
        let engine::json::JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        let engine::json::JsonValue::Number(row_count) = &top["row_count"] else {
            panic!("row_count must be a number");
        };
        assert_eq!(rows.len(), result.rows.len(), "limit={limit}");
        assert_eq!(
            row_count.as_f64(),
            result.rows.len() as f64,
            "limit={limit}"
        );
        assert!(result.rows.len() <= limit, "limit={limit}");
    }
}

#[test]
fn null_cell_from_alter_table_added_column_encodes_as_json_null() {
    let path = unique_db_path("nosql11-response-schema-null-cell");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    // TABLE-5: `ALTER TABLE ADD COLUMN` は暗黙 nullable で既存行に対しては
    // `Cell::Null` として読める（`tests/catalog.rs::
    // table5_alter_table_add_column_preserves_existing_row_bytes` 系と同型）。
    storage
        .alter_table_add_column(TABLE, ColumnDef::new("tag", ColumnType::Text, true))
        .expect("alter_table_add_column");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx_a, "SELECT id, tag FROM docs LIMIT 10")
        .expect("scan should succeed");
    assert!(!result.rows.is_empty());
    assert!(
        result
            .rows
            .iter()
            .all(|row| matches!(row.cells.get(1), Some(Cell::Null))),
        "newly added column must decode as Cell::Null for pre-existing rows"
    );

    let body = encode(&result).expect("encode");
    let parsed = engine::json::parse_json(&body).expect("valid JSON");
    let engine::json::JsonValue::Object(top) = parsed else {
        panic!("top level must be an object");
    };
    let engine::json::JsonValue::Array(rows) = &top["rows"] else {
        panic!("rows must be an array");
    };
    for row in rows {
        let engine::json::JsonValue::Array(cells) = row else {
            panic!("row must be an array");
        };
        assert_eq!(cells.get(1), Some(&engine::json::JsonValue::Null));
    }
}

#[test]
fn tenant_b_rows_never_appear_in_encoded_body_for_tenant_a_context() {
    let path = unique_db_path("nosql11-response-schema-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx_a, "SELECT * FROM docs LIMIT 100")
        .expect("scan should succeed");
    for row in &result.rows {
        assert!(
            (1..=3).contains(&row.id),
            "tenant-b の非公開行（id 101..=102）が漏れている: {}",
            row.id
        );
    }

    let body = encode(&result).expect("encode");
    // tenant-b の行 ID（101・102）が応答本文の数値として一切現れないこと
    // （エンコーダが `QueryResult` 外の情報を付加しない契約の観測）。
    assert!(!body.contains("101"));
    assert!(!body.contains("102"));
}
