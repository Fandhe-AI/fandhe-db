//! `ARRAY` 列型（TABLE-14・TASK-198、Issue #888）の簡易クエリプロトコル経由
//! （生バイトクライアント）検証（層 A。ポインタ: `docs/spec/05-tasks.md`
//! TASK-198・`docs/spec/04-behavior/data-model.md` TABLE-14・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-13）。
//!
//! 配列列そのものの符号化・意味論は `crates/engine/tests/composite_types.rs`
//! が確定オラクルとして検証済みのため、本ファイルは同じ規則が **wire
//! フレーミング** 越しに観測できることの確認に徹する
//! （`wire_delete_single_row.rs` と同じ流儀）。Issue #1193 で要素型の拡大・NULL 要素・
//! 配列列の等価述語（`=`／`IN`／`IS [NOT] NULL`）を追加した（NOSQL-17・WIRE-13）。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_array_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-array-column-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "tags",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                    true,
                ),
                ColumnDef::new(
                    "nums",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Integer, 4).expect("array ty")),
                    true,
                ),
                ColumnDef::new(
                    "stamps",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Timestamp, 4).expect("array ty"),
                    ),
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_users(
    core: Arc<EngineCore>,
    users: &[(&str, &str, &str)],
) -> Vec<std::net::SocketAddr> {
    let users_path = write_user_store_file(users);
    vec![spawn_server_with_engine(&users_path, core)]
}

/// INSERT・SELECT を simple query 経由で往復し、`RowDescription` の列名と
/// `DataRow` の PostgreSQL 配列テキスト表現（引用・エスケープを含む）を確認する。
#[test]
fn wire_array_column_insert_and_select_roundtrip() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        r#"INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{a,"b c",""}') USING OPERATION_ID 'op-1'"#,
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags FROM docs WHERE id = 1 LIMIT 1",
    );
    let names = read_row_description(&mut stream);
    assert_eq!(names, vec!["id".to_string(), "tags".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(row[1].as_deref(), Some(r#"{a,"b c",""}"#));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 空配列と NULL 列が区別されて往復すること。
#[test]
fn wire_array_column_distinguishes_null_and_empty_array() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{}') USING OPERATION_ID 'op-1'",
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding) VALUES (2, '[0.1,0.2]') USING OPERATION_ID 'op-2'",
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags FROM docs WHERE id = 1 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1].as_deref(), Some("{}"));
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags FROM docs WHERE id = 2 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1], None);
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 要素数がスキーマの `max_len` を超過する INSERT は `54000`（`PayloadTooLarge`）。
#[test]
fn wire_array_literal_exceeding_max_len_is_rejected_with_54000() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    // tags は max_len=4。5 要素は超過。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{a,b,c,d,e}') USING OPERATION_ID 'op-1'",
    );
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);
}

/// 閉じていない引用は形式不正の `22P02`（NULL 要素は受理される。Issue #1193）。
#[test]
fn wire_array_literal_format_violations_are_rejected_with_expected_codes() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        r#"INSERT INTO docs (id, embedding, tags) VALUES (2, '[0.1,0.2]', '{"a}') USING OPERATION_ID 'op-2'"#,
    );
    expect_error_response_with_sqlstate(&mut stream, "22P02");
    read_ready_for_query(&mut stream);

    // 新しい要素型の要素エラーはスカラー列と同じ分類（整数あふれ 22003）。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, nums) VALUES (3, '[0.1,0.2]', '{2147483648}') USING OPERATION_ID 'op-3'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);
}

/// 新しい要素型と NULL 要素が wire 越しに往復し、text 表現は `NULL`（引用なし）と
/// 文字列 `"NULL"`（引用あり）を区別する。`RowDescription` の型公告は他の配列列と
/// 同じ text のまま（WIRE-13）。
#[test]
fn wire_array_new_element_types_and_null_elements_roundtrip() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        r#"INSERT INTO docs (id, embedding, tags, nums, stamps) VALUES (1, '[0.1,0.2]', '{a,NULL,"NULL"}', '{1,NULL,-3}', '{"1970-01-01 00:00:01",NULL}') USING OPERATION_ID 'op-1'"#,
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags, nums, stamps FROM docs WHERE id = 1 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1].as_deref(), Some(r#"{a,NULL,"NULL"}"#));
    assert_eq!(row[2].as_deref(), Some("{1,NULL,-3}"));
    assert_eq!(row[3].as_deref(), Some(r#"{"1970-01-01 00:00:01",NULL}"#));
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// `WHERE` の配列列に対する `=`・`IN`・`IS [NOT] NULL` が wire 越しに受理される
/// （Issue #1193）。等価の意味論は engine の `composite_types.rs` が確定オラクル。
#[test]
fn wire_where_array_equality_in_and_is_null_are_accepted() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    for (id, tags) in [(1u32, Some("{a,NULL}")), (2, Some("{b}")), (3, None)] {
        let sql = match tags {
            Some(t) => format!(
                "INSERT INTO docs (id, embedding, tags) VALUES ({id}, '[0.1,0.2]', '{t}') USING OPERATION_ID 'op-{id}'"
            ),
            None => format!(
                "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2]') USING OPERATION_ID 'op-{id}'"
            ),
        };
        send_simple_query(&mut stream, &sql);
        let _ = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }

    for (predicate, expected) in [
        ("tags = '{a,NULL}'", vec!["1"]),
        ("tags IN ('{b}','{x}')", vec!["2"]),
        ("tags IS NULL", vec!["3"]),
        ("tags IS NOT NULL", vec!["1", "2"]),
    ] {
        send_simple_query(
            &mut stream,
            &format!("SELECT id FROM docs WHERE {predicate} LIMIT 10"),
        );
        let _ = read_row_description(&mut stream);
        let mut ids: Vec<String> = Vec::new();
        for _ in 0..expected.len() {
            let row = read_data_row(&mut stream);
            ids.push(row[0].clone().expect("id"));
        }
        ids.sort();
        assert_eq!(ids, expected, "predicate: {predicate}");
        let _ = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }
}

/// 配列リテラルとして解釈できない右辺は書き込みと同じ分類（`22P02`）で拒否される
/// （Issue #1193。旧契約の型不一致 `22000` からの変更）。
#[test]
fn wire_where_array_column_with_malformed_literal_is_rejected() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, "SELECT id FROM docs WHERE tags = 'x' LIMIT 10");
    expect_error_response_with_sqlstate(&mut stream, "22P02");
    read_ready_for_query(&mut stream);
}

/// Issue #1357: `NUMERIC`・`BYTEA`・`JSON`・`JSONB`・`ENUM` 要素の配列が wire 越しに
/// 往復する。`RowDescription` の型公告は他の配列列と同じ text のまま（WIRE-13）で、
/// 配列テキストは PostgreSQL と同形（`BYTEA` は `"\\x0102"`、JSON は引用・エスケープ）。
/// 描画した配列テキストを `WHERE` の右辺へそのまま使うと同じ行に一致する（往復）。
#[test]
fn wire_array_extended_element_types_roundtrip() {
    let path = temp_db::unique_db_path("wire-array-extended");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    let arr = |elem| ColumnType::Array(ArrayType::new(elem, 4).expect("array ty"));
    storage
        .create_table(&TableSchema::new(
            "ext",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "nums",
                    arr(ArrayElemType::Numeric {
                        precision: 5,
                        scale: 2,
                    }),
                    true,
                ),
                ColumnDef::new("blobs", arr(ArrayElemType::Bytea), true),
                ColumnDef::new("js", arr(ArrayElemType::Json), true),
                ColumnDef::new("jb", arr(ArrayElemType::Jsonb), true),
                ColumnDef::new(
                    "moods",
                    ColumnType::Array(ArrayType::new_enum(mood, 4).expect("array ty")),
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        r#"INSERT INTO ext (id, embedding, nums, blobs, js, jb, moods) VALUES (1, '[0.1,0.2]', '{1.5,NULL,-3.25}', '{"\\x0102",NULL,"\\x"}', '{"{\"a\":1}",NULL}', '{"{ \"b\" : 1, \"a\" : 2 }"}', '{happy,NULL}') USING OPERATION_ID 'op-1'"#,
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT nums, blobs, js, jb, moods FROM ext WHERE id = 1 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("{1.50,NULL,-3.25}"));
    assert_eq!(row[1].as_deref(), Some(r#"{"\\x0102",NULL,"\\x"}"#));
    assert_eq!(row[2].as_deref(), Some(r#"{"{\"a\":1}",NULL}"#));
    assert_eq!(row[3].as_deref(), Some(r#"{"{\"a\":2,\"b\":1}"}"#));
    assert_eq!(row[4].as_deref(), Some("{happy,NULL}"));
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 描画した配列テキストを右辺へそのまま使うと同じ行に一致する。
    for predicate in [
        "nums = '{1.50,NULL,-3.25}'",
        r#"blobs = '{"\\x0102",NULL,"\\x"}'"#,
        r#"js = '{"{\"a\":1}",NULL}'"#,
        r#"jb = '{"{\"a\":2,\"b\":1}"}'"#,
        "moods = '{happy,NULL}'",
    ] {
        send_simple_query(
            &mut stream,
            &format!("SELECT id FROM ext WHERE {predicate} LIMIT 10"),
        );
        let _ = read_row_description(&mut stream);
        let row = read_data_row(&mut stream);
        assert_eq!(row[0].as_deref(), Some("1"), "predicate: {predicate}");
        let _ = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }

    // 要素の形式不正は書き込み前に 22P02、NUMERIC の桁あふれは 22003、
    // 語彙外 ENUM は 22P02、要素数超過は 54000。
    for (column, literal, sqlstate) in [
        ("nums", "{abc}", "22P02"),
        ("nums", "{123456}", "22003"),
        ("blobs", "{zz}", "22P02"),
        ("js", r#"{"{bad"}"#, "22P02"),
        ("moods", "{ecstatic}", "22P02"),
        ("moods", "{happy,sad,happy,sad,happy}", "54000"),
    ] {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO ext (id, embedding, {column}) VALUES (9, '[0.1,0.2]', '{literal}') USING OPERATION_ID 'bad-{column}'"
            ),
        );
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
}
/// 多次元配列（TABLE-35 (2)・Issue #1605）が simple query 経由で往復する。`DataRow` は入れ子の
/// PostgreSQL 配列テキストで、`RowDescription` は配列型 OID（`int4[]`=1007。Issue #1606）で公告する。
/// 不整列リテラルは `22P02`。
#[test]
fn wire_multi_dimensional_array_roundtrips_and_rejects_ragged_literals() {
    let path = temp_db::unique_db_path("wire-array-column-multidim");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "grid",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "m",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Integer, 16)
                            .and_then(|a| a.with_ndims(2))
                            .expect("array ty"),
                    ),
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO grid (id, embedding, m) VALUES (1, '[0.1,0.2]', '{{1,2},{3,NULL}}') \
         USING OPERATION_ID 'op-m1'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id, m FROM grid WHERE id = 1 LIMIT 1");
    let names = read_row_description(&mut stream);
    assert_eq!(names, vec!["id".to_string(), "m".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1].as_deref(), Some("{{1,2},{3,NULL}}"));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO grid (id, embedding, m) VALUES (2, '[0.1,0.2]', '{{1,2},{3}}') \
         USING OPERATION_ID 'op-m2'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22P02");
    read_ready_for_query(&mut stream);
}
/// Issue #3229（要素型追加の土台）の回帰: 既存の全 13 種の配列要素型が、末尾 `;` 付きの単独
/// 簡易クエリ（psql と同じ送り方）で往復し、`RowDescription` が要素型ごとの配列 OID を公告する
/// （TABLE-14・TABLE-35・WIRE-13）。別テナントからは 0 行で、OID は同じ（RLS 相当の境界は不変）。
#[test]
fn wire_all_existing_array_elem_types_roundtrip_with_oids_and_tenant_isolation() {
    // (列名, 要素型, 期待 OID, 挿入リテラル, 期待テキスト)
    let cases: Vec<(&str, ArrayElemType, i32, &str, &str)> = vec![
        ("c_text", ArrayElemType::Text, 1009, "{a,b}", "{a,b}"),
        ("c_bool", ArrayElemType::Bool, 1000, "{t,f}", "{t,f}"),
        ("c_int", ArrayElemType::Integer, 1007, "{1,2}", "{1,2}"),
        ("c_big", ArrayElemType::BigInt, 1016, "{3,4}", "{3,4}"),
        ("c_real", ArrayElemType::Real, 1021, "{1.5}", "{1.5}"),
        ("c_dbl", ArrayElemType::Double, 1022, "{2.5}", "{2.5}"),
        (
            "c_date",
            ArrayElemType::Date,
            1182,
            "{2024-01-02}",
            "{2024-01-02}",
        ),
        (
            "c_ts",
            ArrayElemType::Timestamp,
            1115,
            r#"{"2024-01-02 03:04:05"}"#,
            r#"{"2024-01-02 03:04:05"}"#,
        ),
        (
            "c_uuid",
            ArrayElemType::Uuid,
            2951,
            "{00000000-0000-0000-0000-000000000001}",
            "{00000000-0000-0000-0000-000000000001}",
        ),
        (
            "c_num",
            ArrayElemType::Numeric {
                precision: 10,
                scale: 2,
            },
            1231,
            "{1.5}",
            "{1.50}",
        ),
        (
            "c_bytea",
            ArrayElemType::Bytea,
            1001,
            r#"{"\\x0102"}"#,
            r#"{"\\x0102"}"#,
        ),
        (
            "c_json",
            ArrayElemType::Json,
            199,
            r#"{"{\"a\":1}"}"#,
            r#"{"{\"a\":1}"}"#,
        ),
        (
            "c_jsonb",
            ArrayElemType::Jsonb,
            3807,
            r#"{"{\"a\":1}"}"#,
            r#"{"{\"a\":1}"}"#,
        ),
    ];

    let path = temp_db::unique_db_path("wire-array-all-elem-types");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mut columns = vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)];
    for (name, elem, ..) in &cases {
        columns.push(ColumnDef::new(
            *name,
            ColumnType::Array(ArrayType::new(*elem, 4).expect("array ty")),
            true,
        ));
    }
    storage
        .create_table(&TableSchema::new("allarr", columns))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addrs = spawn_with_users(
        core,
        &[
            ("alice", "tenant-alice", "pw-alice"),
            ("bob", "tenant-bob", "pw-bob"),
        ],
    );
    let names: Vec<&str> = cases.iter().map(|c| c.0).collect();
    let select = format!(
        "SELECT {} FROM allarr WHERE id = 1 LIMIT 1;",
        names.join(", ")
    );

    let mut alice = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");
    let insert = format!(
        "INSERT INTO allarr (id, embedding, {}) VALUES (1, '[0.1,0.2]', {}) USING OPERATION_ID 'op-all'",
        names.join(", "),
        cases
            .iter()
            .map(|c| format!("'{}'", c.3))
            .collect::<Vec<_>>()
            .join(", ")
    );
    send_simple_query(&mut alice, &insert);
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, &select);
    let desc = read_row_description_with_oids(&mut alice);
    assert_eq!(desc.len(), cases.len());
    for ((name, oid), case) in desc.iter().zip(&cases) {
        assert_eq!(name, case.0);
        assert_eq!(*oid, case.2, "OID of {name}");
    }
    let row = read_data_row(&mut alice);
    for (cell, case) in row.iter().zip(&cases) {
        assert_eq!(cell.as_deref(), Some(case.4), "value of {}", case.0);
    }
    let _ = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    // 別テナントは同じ OID を見るが行は見えない。
    let mut bob = authenticate_to_ready_for_query(addrs[0], "bob", "pw-bob");
    send_simple_query(&mut bob, &select);
    let desc_bob = read_row_description_with_oids(&mut bob);
    assert_eq!(desc_bob, desc);
    assert_eq!(read_command_complete(&mut bob), "SELECT 0");
    read_ready_for_query(&mut bob);
}
