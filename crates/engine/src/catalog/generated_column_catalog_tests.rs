//! 生成列（`GENERATED ALWAYS AS (<式>) STORED`）のカタログ永続形 v16 の単体テスト（TABLE-50・
//! Issue #3327）。確認すること: v16 の往復（採番方式・移行済み）、生成列の無い表のバイト列が不変、
//! decode の fail-closed（件数・行の形・識別子・hex・UTF-8・併存禁止）、FK 保持版・軽量スキャナ・
//! 読み取りゲートが v16 を認識すること（P0。漏れると fail-open）、従来方式の表との併存禁止、
//! ALTER の依存検査（参照列の DROP の `2BP01`・RENAME の追随・CASCADE・DROP EXPRESSION。Issue #3331）と
//! 生成式の保持。

use super::{
    catalog_value_references_enum_type_in_table, decode_schema, encode_schema,
    row_mode_is_migrated, ColumnDef, ColumnType, GeneratedColumnDef, RowIdentity, TableSchema,
    CATALOG_FORMAT_VERSION_V16, FK_BEARING_FORMAT_VERSIONS, MAX_GENERATED_EXPRESSION_SQL_LEN,
    MAX_GENERATED_REFERENCED_COLUMNS,
};
use crate::catalog::{is_row_identity_unsupported, CatalogError};
use crate::storage::Storage;
use redb::ReadableDatabase;

fn gen_def(expression: &str, refs: &[&str]) -> GeneratedColumnDef {
    GeneratedColumnDef {
        expression: expression.to_string(),
        referenced_columns: refs.iter().map(|s| s.to_string()).collect(),
    }
}

/// `a INTEGER`・`b INTEGER GENERATED (a * 2)`・`c TEXT` を持つスキーマ。
fn schema_with(identity: RowIdentity) -> TableSchema {
    let mut b = ColumnDef::new("b", ColumnType::Integer, true);
    b.generated = Some(gen_def("a * 2", &["a"]));
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("a", ColumnType::Integer, true),
            b,
            ColumnDef::new("c", ColumnType::Text, true),
        ],
    )
    .with_row_identity(identity)
}

fn text_of(schema: &TableSchema) -> String {
    String::from_utf8(encode_schema(schema).expect("encode")).expect("utf8")
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn v16_roundtrips_allocated_and_migrated() {
    for identity in [RowIdentity::Allocated, RowIdentity::MigratedId] {
        let schema = schema_with(identity);
        let bytes = encode_schema(&schema).expect("encode");
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert!(text.starts_with("v16\n"), "{text}");
        let mode = if identity == RowIdentity::Allocated {
            "allocated"
        } else {
            "migrated"
        };
        assert!(text.contains(&format!("\nrowmode:{mode}\n")), "{text}");
        assert!(text.contains("\ngens:1\n"), "{text}");
        assert!(
            text.contains(&format!("\ngen:b:a:{}\n", hex("a * 2"))),
            "{text}"
        );
        let decoded = decode_schema("docs", &bytes).expect("decode");
        assert_eq!(decoded, schema);
        assert_eq!(decoded.columns[1].generation_expression(), Some("a * 2"));
        assert_eq!(encode_schema(&decoded).expect("re-encode"), bytes);
        // 軽量スキャナ（ENUM 依存判定）も v16 を受理し、依存なしと判定する。
        assert!(
            !catalog_value_references_enum_type_in_table(&bytes, "mood", Some("docs"))
                .expect("valid v16")
        );
    }
}

#[test]
fn schema_without_generated_column_keeps_previous_bytes() {
    let mut schema = schema_with(RowIdentity::Allocated);
    schema.columns[1].generated = None;
    let text = text_of(&schema);
    assert!(text.starts_with("v15\n"), "{text}");
    assert!(!text.contains("gens:"), "{text}");
}

#[test]
fn v16_without_references_roundtrips() {
    let mut schema = schema_with(RowIdentity::Allocated);
    schema.columns[1].generated = Some(gen_def("1 + 1", &[]));
    let bytes = encode_schema(&schema).expect("encode");
    assert_eq!(decode_schema("docs", &bytes).expect("decode"), schema);
}

#[test]
fn decode_rejects_malformed_gens_section() {
    let good = text_of(&schema_with(RowIdentity::Allocated));
    let h = hex("a * 2");
    let gen_line = format!("gen:b:a:{h}\n");
    let big_hex = "61".repeat(MAX_GENERATED_EXPRESSION_SQL_LEN + 1);
    let many_refs = (0..=MAX_GENERATED_REFERENCED_COLUMNS)
        .map(|i| format!("r{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let cases: Vec<(&str, String)> = vec![
        (
            "gens:0",
            good.replace("gens:1\n", "gens:0\n").replace(&gen_line, ""),
        ),
        (
            "count exceeds live columns",
            good.replace("gens:1\n", "gens:4\n"),
        ),
        ("count not a number", good.replace("gens:1\n", "gens:x\n")),
        (
            "missing gens line",
            good.replace(&format!("gens:1\n{gen_line}"), ""),
        ),
        ("missing gen line", good.replace(&gen_line, "")),
        (
            "duplicate generated column",
            good.replace("gens:1\n", "gens:2\n")
                .replace(&gen_line, &format!("{gen_line}{gen_line}")),
        ),
        ("unknown column", good.replace("gen:b:", "gen:zzz:")),
        ("unknown reference", good.replace("gen:b:a:", "gen:b:zzz:")),
        ("referencing itself", good.replace("gen:b:a:", "gen:b:b:")),
        (
            "referencing another generated column",
            good.replace("gens:1\n", "gens:2\n")
                .replace(&gen_line, &format!("{gen_line}gen:c:b:{h}\n")),
        ),
        ("odd hex", good.replace(&h, &h[..h.len() - 1])),
        ("non hex", good.replace(&h, "zz")),
        ("invalid utf8", good.replace(&h, "ff")),
        ("empty expression", good.replace(&h, "")),
        ("too long expression", good.replace(&h, &big_hex)),
        (
            "too many references",
            good.replace("gen:b:a:", &format!("gen:b:{many_refs}:")),
        ),
        (
            "extra field",
            good.replace(&gen_line, &format!("gen:b:a:{h}:x\n")),
        ),
        ("bad prefix", good.replace("gen:b:", "genx:b:")),
        (
            "rowmode legacy",
            good.replace("rowmode:allocated", "rowmode:legacy"),
        ),
    ];
    for (label, text) in cases {
        let r = decode_schema("docs", text.as_bytes());
        assert!(r.is_err(), "{label} must be rejected by decode: {text}");
    }
    // 旧版の値に gens 節が付いても余剰行として拒否される。
    let old = good.replacen("v16", "v15", 1);
    assert!(decode_schema("docs", old.as_bytes()).is_err());
}

/// 軽量スキャナ（ENUM 依存判定）も、壊れた gens 節を「依存なし」へ丸めず `CorruptSchema` にする。
#[test]
fn lightweight_scanner_rejects_malformed_gens_section() {
    let good = text_of(&schema_with(RowIdentity::Allocated));
    let h = hex("a * 2");
    let gen_line = format!("gen:b:a:{h}\n");
    for (label, text) in [
        ("gens:0", good.replace("gens:1\n", "gens:0\n")),
        ("count exceeds", good.replace("gens:1\n", "gens:4\n")),
        ("non hex", good.replace(&h, "zz")),
        ("unknown column", good.replace("gen:b:", "gen:zzz:")),
        (
            "duplicate",
            good.replace("gens:1\n", "gens:2\n")
                .replace(&gen_line, &format!("{gen_line}{gen_line}")),
        ),
        (
            "missing section",
            good.replace(&format!("gens:1\n{gen_line}"), ""),
        ),
    ] {
        let s = catalog_value_references_enum_type_in_table(text.as_bytes(), "mood", Some("docs"));
        assert!(
            matches!(s, Err(CatalogError::CorruptSchema(_))),
            "{label} must be rejected by the lightweight scanner: {s:?}"
        );
    }
}

#[test]
fn validate_rejects_invalid_generated_columns() {
    // 従来方式の表と併存できない。
    assert!(encode_schema(&schema_with(RowIdentity::LegacyId)).is_err());
    // ベクトル系の列は生成列にできない。
    let mut s = schema_with(RowIdentity::Allocated);
    s.columns[2] = {
        let mut c = ColumnDef::new("c", ColumnType::Vector(2), true);
        c.generated = Some(gen_def("a", &["a"]));
        c
    };
    assert!(encode_schema(&s).is_err());
    // DEFAULT と併存できない。
    let mut s = schema_with(RowIdentity::Allocated);
    s.columns[1] = {
        let mut c = ColumnDef::new("b", ColumnType::Integer, true)
            .with_default(super::ColumnDefault::Number("1".to_string()));
        c.generated = Some(gen_def("a * 2", &["a"]));
        c
    };
    assert!(encode_schema(&s).is_err());
    // 参照列が未知・生成列（自分自身）・重複。
    for refs in [&["zzz"][..], &["b"][..], &["c", "c"][..]] {
        let mut s = schema_with(RowIdentity::Allocated);
        s.columns[1].generated = Some(gen_def("a * 2", refs));
        assert!(encode_schema(&s).is_err(), "{refs:?}");
    }
    // 生成列どうしの参照。
    let mut s = schema_with(RowIdentity::Allocated);
    s.columns[2].generated = Some(gen_def("b", &["b"]));
    assert!(encode_schema(&s).is_err(), "generated -> generated");
    // 式が空・上限超過。
    let mut s = schema_with(RowIdentity::Allocated);
    s.columns[1].generated = Some(gen_def("", &["a"]));
    assert!(encode_schema(&s).is_err());
    s.columns[1].generated = Some(gen_def(
        &"x".repeat(MAX_GENERATED_EXPRESSION_SQL_LEN + 1),
        &["a"],
    ));
    assert!(encode_schema(&s).is_err());
}

/// P0: v16 は FK 保持版として登録されている（漏れると DROP TABLE の依存検査が fail-open）。
#[test]
fn v16_is_foreign_key_bearing() {
    assert!(FK_BEARING_FORMAT_VERSIONS.contains(&CATALOG_FORMAT_VERSION_V16));
}

#[test]
fn row_mode_gate_helper_recognizes_v16() {
    let allocated = encode_schema(&schema_with(RowIdentity::Allocated)).expect("encode");
    let migrated = encode_schema(&schema_with(RowIdentity::MigratedId)).expect("encode");
    assert!(!row_mode_is_migrated(&allocated));
    assert!(row_mode_is_migrated(&migrated));
}

fn open_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    (Storage::open(&path).expect("open"), guard)
}

/// P0: 読み取りゲート（`require_legacy_row_identity_read`）が v16 の採番方式の表を v15 と同じく拒否する。
#[test]
// intentional: docs/design/rowid-primary-key-separation.md
fn read_gate_rejects_allocated_v16_table() {
    let (storage, _g) = open_storage("gen-gate");
    storage
        .create_table(&schema_with(RowIdentity::Allocated))
        .expect("create");
    let err = storage
        .get_row_from_table("docs", "t", 1)
        .expect_err("gated get");
    assert!(is_row_identity_unsupported(&err), "{err:?}");
    let err = storage
        .scan_table_page("docs", None, 10)
        .expect_err("gated scan");
    assert!(is_row_identity_unsupported(&err), "{err:?}");
}

#[test]
fn v16_survives_reopen() {
    let path = crate::test_util::temp_db::unique_db_path("gen-reopen");
    let _g = crate::test_util::temp_db::CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage
            .create_table(&schema_with(RowIdentity::Allocated))
            .expect("create");
    }
    let storage = Storage::open(&path).expect("reopen");
    let read = storage.db().begin_read().expect("read");
    let schema = crate::catalog::get_table_schema_in_txn(&read, "docs").expect("resolved");
    assert_eq!(schema.columns[1].generation_expression(), Some("a * 2"));
    assert_eq!(
        schema.columns[1]
            .generated
            .as_ref()
            .map(|g| g.referenced_columns.clone()),
        Some(vec!["a".to_string()])
    );
}

/// 参照列の DROP は `2BP01`、RENAME は式へ追随する（TABLE-50 (3)・Issue #3331）。生成列自身の改名・
/// 参照しない列の DROP は通り、生成式は保たれる。
#[test]
fn alter_guards_protect_generated_columns() {
    let (storage, _g) = open_storage("gen-alter");
    storage
        .create_table(&schema_with(RowIdentity::Allocated))
        .expect("create");
    assert!(matches!(
        storage.alter_table_drop_column("docs", "a"),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    storage
        .alter_table_drop_column("docs", "c")
        .expect("drop c");
    storage
        .alter_table_rename_column("docs", "a", "a2")
        .unwrap_or_else(|_| panic!("rename of a referenced column must follow the expression"));
    {
        let read = storage.db().begin_read().expect("read");
        let schema = crate::catalog::get_table_schema_in_txn(&read, "docs").expect("resolved");
        let b = schema.columns.iter().find(|c| c.name == "b").expect("b");
        let def = b.generated.as_ref().expect("generated");
        assert_eq!(def.referenced_columns, ["a2"]);
        assert!(def.expression.contains("a2"), "{}", def.expression);
        assert!(!def.expression.contains("a "), "{}", def.expression);
    }
    storage
        .alter_table_rename_column("docs", "b", "b2")
        .expect("rename generated column itself");
    storage
        .alter_table_drop_column("docs", "b2")
        .expect("drop generated");
    // 生成列が消えた後は `a2` を参照する依存が無い（最後の 1 列の削除は別の理由で拒否される）。
    assert!(!matches!(
        storage.alter_table_drop_column("docs", "a2"),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
}

/// 行ストアへ直接置いた 1 行（`a = 7`・`b = 14`・`c = 'x'`）の物理バイト列を返す補助。
fn put_row_and_read_bytes(storage: &Storage, schema: &TableSchema) -> Vec<u8> {
    use crate::row_codec::{encode_scalar_columns, Value};
    use crate::storage::{encode_row, RowInput, Visibility};
    let write_txn = storage.begin_write_txn().expect("begin");
    {
        let name = crate::catalog::user_rows_table_name("docs");
        let mut rows = write_txn
            .open_table(crate::catalog::user_rows_table_def(&name))
            .expect("open rows");
        let metadata = encode_scalar_columns(
            schema,
            &[
                Value::Integer(7),
                Value::Integer(14),
                Value::Text("x".to_string()),
            ],
        )
        .expect("encode");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[],
            metadata: &metadata,
        };
        let encoded = encode_row(&row).expect("encode row");
        rows.insert(("tenant-a", 1u64), encoded.as_slice())
            .expect("insert");
    }
    write_txn.commit_raw_for_test().expect("commit");
    read_row_bytes(storage)
}

fn read_row_bytes(storage: &Storage) -> Vec<u8> {
    let read = storage.db().begin_read().expect("read");
    let name = crate::catalog::user_rows_table_name("docs");
    let rows = read
        .open_table(crate::catalog::user_rows_table_def(&name))
        .expect("open rows");
    let guard = rows.get(("tenant-a", 1u64)).expect("get").expect("row");
    guard.value().to_vec()
}

/// `DROP EXPRESSION` は行を書き換えず、値・NOT NULL を保つ。最後の生成列が消えた表は v16 でなくなる。
#[test]
fn drop_expression_keeps_row_bytes_and_leaves_v16_when_last() {
    let (storage, _g) = open_storage("gen-drop-expr");
    let schema = schema_with(RowIdentity::Allocated);
    storage.create_table(&schema).expect("create");
    let before = put_row_and_read_bytes(&storage, &schema);
    let applied = storage
        .alter_table_drop_expression("docs", "b", false)
        .unwrap_or_else(|_| panic!("drop expression"));
    assert_eq!(applied, crate::catalog::AlterApply::Applied);
    assert_eq!(read_row_bytes(&storage), before);
    let after = storage.get_table_schema("docs").expect("schema");
    assert!(after.columns.iter().all(|c| !c.is_generated()));
    assert!(!text_of(&after).starts_with("v16\n"));
    // 通常の列になった後の再実行は `55000`（`IF EXISTS` は no-op）。
    assert!(storage
        .alter_table_drop_expression("docs", "b", false)
        .is_err());
    assert_eq!(
        storage
            .alter_table_drop_expression("docs", "b", true)
            .unwrap_or_else(|_| panic!("if exists")),
        crate::catalog::AlterApply::Unchanged
    );
}

/// `CASCADE` は依存する生成列を連鎖削除し、残る列の行バイト列を変えない。連鎖できない依存があれば
/// 何も削除しない（部分削除なし）。
#[test]
fn cascade_drops_dependent_generated_column_and_keeps_row_bytes() {
    use crate::catalog::DropBehavior;
    let (storage, _g) = open_storage("gen-cascade");
    let schema = schema_with(RowIdentity::Allocated);
    storage.create_table(&schema).expect("create");
    let before = put_row_and_read_bytes(&storage, &schema);
    let cascaded = storage
        .alter_table_drop_column_with_behavior("docs", "a", DropBehavior::Cascade)
        .expect("cascade");
    assert_eq!(cascaded, ["b"]);
    assert_eq!(read_row_bytes(&storage), before);
    let after = storage.get_table_schema("docs").expect("schema");
    let names: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["c"]);
    assert!(!text_of(&after).starts_with("v16\n"));

    // 連鎖先の生成列が PRIMARY KEY の構成列なら全体を拒否し、何も削除しない。
    let mut b = ColumnDef::new("b", ColumnType::Integer, false);
    b.generated = Some(gen_def("a * 2", &["a"]));
    let pk_schema = TableSchema::new(
        "pk_docs",
        vec![ColumnDef::new("a", ColumnType::Integer, true), b],
    )
    .with_row_identity(RowIdentity::Allocated)
    .with_primary_key(vec!["b".to_string()]);
    storage.create_table(&pk_schema).expect("create pk table");
    assert!(matches!(
        storage.alter_table_drop_column_with_behavior("pk_docs", "a", DropBehavior::Cascade),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    let still = storage.get_table_schema("pk_docs").expect("schema");
    assert_eq!(still.columns.len(), 2);
}

/// 論理ダンプのデータ出力（COPY・INSERT）は生成列の列名と値を除く（TABLE-50・Issue #3327。復元は
/// 通常の書き込み経路で行い、生成列は復元時に計算点で再計算される）。書き込みの停止中は生成列を持つ表に
/// 行を書けないため、行は行ストアへ直接置く。
#[test]
fn logical_dump_data_omits_generated_columns() {
    use crate::catalog::ddl_export::{SchemaDdlScope, TableDataFormat, TableDataPart};
    use crate::row_codec::{encode_scalar_columns, Value};
    use crate::storage::{encode_row, RowInput, Visibility};

    let (storage, _g) = open_storage("gen-dump-data");
    let schema = schema_with(RowIdentity::Allocated);
    storage.create_table(&schema).expect("create");
    {
        let write_txn = storage.begin_write_txn().expect("begin");
        {
            let name = crate::catalog::user_rows_table_name("docs");
            let mut rows = write_txn
                .open_table(crate::catalog::user_rows_table_def(&name))
                .expect("open rows");
            let metadata = encode_scalar_columns(
                &schema,
                &[
                    Value::Integer(7),
                    Value::Integer(14),
                    Value::Text("x".to_string()),
                ],
            )
            .expect("encode");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[],
                metadata: &metadata,
            };
            let encoded = encode_row(&row).expect("encode row");
            rows.insert(("tenant-a", 1u64), encoded.as_slice())
                .expect("insert");
        }
        write_txn.commit_raw_for_test().expect("commit");
    }
    for (format, expect) in [
        (
            TableDataFormat::Copy,
            "COPY \"docs\" (\"a\", \"c\") FROM STDIN",
        ),
        (
            TableDataFormat::Insert,
            "INSERT INTO \"docs\" (\"a\", \"c\") VALUES (7, 'x')",
        ),
    ] {
        let mut pieces: Vec<(TableDataPart, String)> = Vec::new();
        storage
            .export_table_data(
                &SchemaDdlScope::shared_and_tenant("tenant-a"),
                format,
                |d| {
                    pieces.push((d.part, d.text.to_string()));
                    Ok::<(), std::convert::Infallible>(())
                },
            )
            .expect("export");
        let joined: Vec<&str> = pieces.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            joined.iter().any(|t| t.contains(expect)),
            "{format:?}: {joined:?}"
        );
        // 生成列の値（14）は出力に現れない。
        assert!(!joined.iter().any(|t| t.contains("14")), "{joined:?}");
    }
}

/// 生成列しか持たない表に行があれば、書き出せる列が無いため固定文言の `Unsupported`（黙って省かない）。
#[test]
// intentional: docs/design/generated-column.md
fn logical_dump_data_rejects_rows_of_a_table_with_only_generated_columns() {
    use crate::catalog::ddl_export::{SchemaDdlError, SchemaDdlScope, TableDataFormat};
    use crate::row_codec::{encode_scalar_columns, Value};
    use crate::storage::{encode_row, RowInput, Visibility};

    let (storage, _g) = open_storage("gen-dump-only");
    let mut only = ColumnDef::new("g", ColumnType::Integer, true);
    only.generated = Some(gen_def("1 + 1", &[]));
    let schema = TableSchema::new("docs", vec![only]).with_row_identity(RowIdentity::Allocated);
    storage.create_table(&schema).expect("create");
    // 行が無ければ何も出さず成功する。
    storage
        .export_table_data(
            &SchemaDdlScope::shared_and_tenant("tenant-a"),
            TableDataFormat::Insert,
            |_| Ok::<(), std::convert::Infallible>(()),
        )
        .expect("empty table exports nothing");
    {
        let write_txn = storage.begin_write_txn().expect("begin");
        {
            let name = crate::catalog::user_rows_table_name("docs");
            let mut rows = write_txn
                .open_table(crate::catalog::user_rows_table_def(&name))
                .expect("open rows");
            let metadata = encode_scalar_columns(&schema, &[Value::Integer(2)]).expect("encode");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[],
                metadata: &metadata,
            };
            let encoded = encode_row(&row).expect("encode row");
            rows.insert(("tenant-a", 1u64), encoded.as_slice())
                .expect("insert");
        }
        write_txn.commit_raw_for_test().expect("commit");
    }
    let err = storage
        .export_table_data(
            &SchemaDdlScope::shared_and_tenant("tenant-a"),
            TableDataFormat::Insert,
            |_| Ok::<(), std::convert::Infallible>(()),
        )
        .expect_err("rows of an only-generated table cannot be dumped");
    assert!(matches!(err, SchemaDdlError::Unsupported { .. }), "{err:?}");
}
/// 全行バックフィルのテスト用に、通常列だけの表 `docs`（`a INTEGER`・`c TEXT`）へ行を直接置く。
fn put_plain_row(storage: &Storage, schema: &TableSchema, tenant: &str, id: u64, a: Option<i32>) {
    use crate::row_codec::{encode_scalar_columns, Value};
    use crate::storage::{encode_row, RowInput, Visibility};
    let write_txn = storage.begin_write_txn().expect("begin");
    {
        let name = crate::catalog::user_rows_table_name("docs");
        let mut rows = write_txn
            .open_table(crate::catalog::user_rows_table_def(&name))
            .expect("open rows");
        let metadata = encode_scalar_columns(
            schema,
            &[
                a.map_or(Value::Null, Value::Integer),
                Value::Text("x".to_string()),
            ],
        )
        .expect("encode");
        let row = RowInput {
            tenant_id: tenant,
            visibility: Visibility::Private,
            embedding: &[],
            metadata: &metadata,
        };
        let encoded = encode_row(&row).expect("encode row");
        rows.insert((tenant, id), encoded.as_slice())
            .expect("insert");
    }
    write_txn.commit_raw_for_test().expect("commit");
}

fn plain_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("a", ColumnType::Integer, true),
            ColumnDef::new("c", ColumnType::Text, true),
        ],
    )
    .with_row_identity(RowIdentity::Allocated)
}

fn raw_rows(storage: &Storage) -> Vec<((String, u64), Vec<u8>)> {
    use redb::ReadableTable;
    let read = storage.db().begin_read().expect("read");
    let name = crate::catalog::user_rows_table_name("docs");
    let rows = read
        .open_table(crate::catalog::user_rows_table_def(&name))
        .expect("open rows");
    rows.iter()
        .expect("iter")
        .map(|e| {
            let (k, v) = e.expect("entry");
            let (t, id) = k.value();
            ((t.to_string(), id), v.value().to_vec())
        })
        .collect()
}

/// 生成列の追加（Issue #3330）: 全テナントの全行が 1 つの write txn で計算され、既存の列の値は変わらない。
#[test]
fn add_generated_column_backfills_all_tenants_in_one_txn() {
    use crate::row_codec::{decode_scalar_columns, Value};
    use crate::storage::decode_row_for_key;
    let (storage, _g) = open_storage("gen-add-backfill");
    let schema = plain_schema();
    storage.create_table(&schema).expect("create");
    put_plain_row(&storage, &schema, "tenant-a", 1, Some(7));
    put_plain_row(&storage, &schema, "tenant-b", 2, Some(3));
    put_plain_row(&storage, &schema, "tenant-b", 3, None);
    let mut g = ColumnDef::new("g", ColumnType::Integer, true);
    g.generated = Some(gen_def("a * 2", &["a"]));
    storage
        .alter_table_in_single_txn("docs", |txn| {
            Storage::alter_table_add_generated_column_in_txn(txn, "docs", g.clone(), 64, false)?;
            Ok(((), true, Vec::new()))
        })
        .unwrap_or_else(|_| panic!("add generated column"));
    let new_schema = storage.get_table_schema("docs").expect("schema");
    assert_eq!(new_schema.columns[2].generation_expression(), Some("a * 2"));
    let rows = raw_rows(&storage);
    assert_eq!(rows.len(), 3);
    let mut seen = Vec::new();
    for ((tenant, id), raw) in &rows {
        let row = decode_row_for_key(
            tenant,
            crate::storage::rowid::RowId::from_legacy_id(*id),
            raw,
        )
        .expect("decode");
        let values = decode_scalar_columns(&new_schema, &row.metadata).expect("values");
        assert_eq!(values[1], Value::Text("x".to_string()));
        seen.push((tenant.clone(), values[0].clone(), values[2].clone()));
    }
    assert_eq!(
        seen,
        [
            (
                "tenant-a".to_string(),
                Value::Integer(7),
                Value::Integer(14)
            ),
            ("tenant-b".to_string(), Value::Integer(3), Value::Integer(6)),
            ("tenant-b".to_string(), Value::Null, Value::Null),
        ]
    );
}

/// 評価エラー（`22012`）は他テナントの行でも txn 全体を破棄し、カタログ・行のバイト列とも変えない。
#[test]
fn add_generated_column_eval_error_leaves_catalog_and_rows_unchanged() {
    let (storage, _g) = open_storage("gen-add-error");
    let schema = plain_schema();
    storage.create_table(&schema).expect("create");
    put_plain_row(&storage, &schema, "tenant-a", 1, Some(5));
    put_plain_row(&storage, &schema, "tenant-b", 2, Some(0));
    let before_rows = raw_rows(&storage);
    let before_gen = storage.table_generation("docs").expect("generation");
    let mut g = ColumnDef::new("g", ColumnType::Integer, true);
    g.generated = Some(gen_def("10 / a", &["a"]));
    let err = storage
        .alter_table_in_single_txn("docs", |txn| {
            Storage::alter_table_add_generated_column_in_txn(txn, "docs", g.clone(), 64, false)?;
            Ok(((), true, Vec::new()))
        })
        .expect_err("division by zero");
    match err {
        crate::catalog::AlterCheckError::Sql(e) => {
            assert_eq!(e.wire_code(), "22012");
            let text = e.to_string();
            assert!(
                !text.contains("tenant-b") && !text.contains("tenant-a"),
                "{text}"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(raw_rows(&storage), before_rows);
    assert_eq!(
        storage.table_generation("docs").expect("generation"),
        before_gen
    );
    assert_eq!(
        storage
            .get_table_schema("docs")
            .expect("schema")
            .columns
            .len(),
        2
    );
}

/// 最下層ガード: カタログだけを書く公開・内部の経路は生成列を受け付けない（行を計算せず列だけが増える
/// 状態を作らせない。fail-closed）。
#[test]
fn catalog_only_add_column_paths_reject_generated_column() {
    let (storage, _g) = open_storage("gen-add-guard");
    storage.create_table(&plain_schema()).expect("create");
    let mut g = ColumnDef::new("g", ColumnType::Integer, true);
    g.generated = Some(gen_def("a * 2", &["a"]));
    assert!(matches!(
        storage.alter_table_add_column("docs", g.clone()),
        Err(CatalogError::Invalid(_))
    ));
    assert!(matches!(
        storage.alter_table_add_column_with_limit("docs", g.clone(), 64),
        Err(CatalogError::Invalid(_))
    ));
    let write_txn = storage.begin_write_txn().expect("begin");
    assert!(matches!(
        Storage::alter_table_add_column_in_txn(&write_txn, "docs", g, 64, false),
        Err(CatalogError::Invalid(_))
    ));
    drop(write_txn);
    assert_eq!(
        storage
            .get_table_schema("docs")
            .expect("schema")
            .columns
            .len(),
        2
    );
}
