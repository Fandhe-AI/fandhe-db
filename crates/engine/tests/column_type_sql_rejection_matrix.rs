//! SQL 表層（`EngineCore::execute_sql_in_session`／`execute_sql`）における
//! 型横断の拒否マトリクス（Issue #897、対象ビヘイビア: TABLE-6・TABLE-7・
//! TASK-86。関連ポインタ: RLS-7／RLS-9・ERR-1／ERR-2）。
//!
//! `tests/column_type_integer.rs`・`tests/*_column.rs` と同じ流儀（実
//! `Storage`＋`CpuScalarProvider`、`USING OPERATION_ID` 必須化）を踏襲し、
//! 型ごとの (列, 不正リテラル, 期待 `wire_code`) を 1 つの表にまとめて横断検証する。
//! 期待 `wire_code` は本ファイルが新しく決めるものではなく、既存の型別結合
//! テストが固定している現行値をそのまま踏襲する（各ケースにポインタを付記）。
//!
//! 検証観点:
//! - 型ごとの不正リテラルが期待 `wire_code` で拒否される
//! - 拒否後も `COUNT(*)` が変化しない（副作用ゼロ）
//! - 同じ `operation_id` で正しい値を再送すると成功する（台帳に記録されない
//!   ため再送が新規操作として扱われる）
//! - 境界ちょうどのリテラルは受理され、SELECT で読み戻せる
//! - RLS-9: 拒否応答が他テナントの境界値行の有無で変わらない・他テナント行が
//!   自テナントの scan／COUNT から見えない
//!
//! 値の形式不正は `22P02`、日時の書式違反は `22007`（Issue #1187・ERR-6）。
//! 本ファイルは現行の production 応答のみを固定する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ENUM_TYPE: &str = "mood";

fn enum_labels() -> Vec<String> {
    vec!["alpha".to_string(), "beta".to_string()]
}

fn schema(enum_def: std::sync::Arc<engine::catalog::EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("c_integer", ColumnType::Integer, true),
            ColumnDef::new("c_bigint", ColumnType::BigInt, true),
            ColumnDef::new("c_real", ColumnType::Real, true),
            ColumnDef::new("c_double", ColumnType::Double, true),
            ColumnDef::new("c_boolean", ColumnType::Boolean, true),
            ColumnDef::new("c_date", ColumnType::Date, true),
            ColumnDef::new("c_timestamp", ColumnType::Timestamp, true),
            ColumnDef::new(
                "c_numeric",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("c_uuid", ColumnType::Uuid, true),
            ColumnDef::new("c_bytea", ColumnType::Bytea, true),
            ColumnDef::new("c_json", ColumnType::Json, true),
            ColumnDef::new("c_jsonb", ColumnType::Jsonb, true),
            ColumnDef::new("c_enum", ColumnType::Enum(enum_def), true),
            ColumnDef::new(
                "c_array_text",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("column-type-sql-rejection-matrix");
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(ENUM_TYPE, enum_labels())
        .expect("create enum type");
    storage.create_table(&schema(def)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, column: &str, literal: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, {column}) VALUES ({id}, '[0.1,0.2]', {literal}) \
         USING OPERATION_ID '{op_id}'"
    )
}

fn count_star(core: &EngineCore, ctx: &PolicyContext) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count(*) should succeed");
    match result.rows[0].cells[0] {
        Cell::Integer(n) => n,
        ref other => panic!("expected Cell::Integer for COUNT(*), got {other:?}"),
    }
}

/// 1 型分の (列, 不正リテラル, 正当リテラル, 期待 wire_code) ケース。
struct TypeCase {
    label: &'static str,
    column: &'static str,
    invalid_literal: String,
    valid_literal: &'static str,
    expected_wire_code: &'static str,
}

fn cases() -> Vec<TypeCase> {
    vec![
        TypeCase {
            // tests/column_type_integer.rs（i32 範囲超過）と同じ分類。
            label: "integer-overflow",
            column: "c_integer",
            invalid_literal: "2147483648".to_string(),
            valid_literal: "42",
            expected_wire_code: "22003",
        },
        TypeCase {
            // tests/column_type_integer.rs（i64 範囲超過）と同じ分類。
            label: "bigint-overflow",
            column: "c_bigint",
            invalid_literal: "99999999999999999999".to_string(),
            valid_literal: "42",
            expected_wire_code: "22003",
        },
        TypeCase {
            // tests/scalar_types_float_roundtrip.rs（REAL 桁あふれ）と同じ分類。
            label: "real-overflow",
            column: "c_real",
            invalid_literal: format!("4{}", "0".repeat(39)),
            valid_literal: "1.5",
            expected_wire_code: "22003",
        },
        TypeCase {
            label: "double-overflow",
            column: "c_double",
            invalid_literal: format!("1{}", "0".repeat(400)),
            valid_literal: "2.5",
            expected_wire_code: "22003",
        },
        TypeCase {
            // tests/boolean_column.rs（文字列リテラルは形式不正）と同じ分類。
            label: "boolean-string-literal",
            column: "c_boolean",
            invalid_literal: "'true'".to_string(),
            valid_literal: "true",
            expected_wire_code: "22P02",
        },
        TypeCase {
            // tests/datetime_column.rs（暦上不正・範囲外）と同じ分類。
            label: "date-invalid-calendar-day",
            column: "c_date",
            invalid_literal: "'2024-02-30'".to_string(),
            valid_literal: "'2024-01-01'",
            expected_wire_code: "22008",
        },
        TypeCase {
            // tests/datetime_column.rs（時刻の範囲外）と同じ分類。
            label: "timestamp-invalid-time-of-day",
            column: "c_timestamp",
            invalid_literal: "'2024-01-02 24:00:00'".to_string(),
            valid_literal: "'2024-01-01 00:00:00'",
            expected_wire_code: "22008",
        },
        TypeCase {
            // tests/numeric_column.rs（桁あふれ）と同じ分類。
            label: "numeric-overflow",
            column: "c_numeric",
            invalid_literal: "1000.00".to_string(),
            valid_literal: "1.50",
            expected_wire_code: "22003",
        },
        TypeCase {
            // tests/uuid_column.rs（形式不正）と同じ分類。
            label: "uuid-malformed",
            column: "c_uuid",
            invalid_literal: "'not-a-uuid'".to_string(),
            valid_literal: "'00000000-0000-0000-0000-000000000000'",
            expected_wire_code: "22P02",
        },
        TypeCase {
            // tests/bytea_column.rs（16 進不正桁）と同じ分類。
            label: "bytea-malformed-hex",
            column: "c_bytea",
            invalid_literal: "'\\xzz'".to_string(),
            valid_literal: "'\\xdead'",
            expected_wire_code: "22P02",
        },
        TypeCase {
            // tests/json_column.rs（構文不正）と同じ分類。
            label: "json-syntax-error",
            column: "c_json",
            invalid_literal: "'not json'".to_string(),
            valid_literal: "'{}'",
            expected_wire_code: "22P02",
        },
        TypeCase {
            label: "jsonb-syntax-error",
            column: "c_jsonb",
            invalid_literal: "'not json'".to_string(),
            valid_literal: "'{}'",
            expected_wire_code: "22P02",
        },
        TypeCase {
            // tests/enum_column.rs（語彙外ラベル）と同じ分類。
            label: "enum-unknown-label",
            column: "c_enum",
            invalid_literal: "'not-a-label'".to_string(),
            valid_literal: "'alpha'",
            expected_wire_code: "22P02",
        },
        TypeCase {
            // tests/composite_types.rs（要素数上限超過）と同じ分類。
            label: "array-exceeds-max-len",
            column: "c_array_text",
            invalid_literal: "'{a,b,c,d,e}'".to_string(),
            valid_literal: "'{a,b}'",
            expected_wire_code: "54000",
        },
    ]
}

#[test]
fn rejection_matrix_covers_all_scalar_columns_in_schema() {
    // ケース表が schema() の非 embedding 列すべてを一意に覆っていることを、
    // 別途ハードコードした集合とではなく schema() 自身から動的に導出した
    // 集合と突き合わせて固定する（列追加時に本チェックだけを見落とすと
    // ここが確実に落ちるようにする。型追加時のケース表更新漏れを検出する
    // non-vacuous 性の担保）。
    let path = unique_db_path("column-type-sql-rejection-matrix-schema-check");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(ENUM_TYPE, enum_labels())
        .expect("create enum type");
    let mut expected: std::collections::BTreeSet<String> = schema(def)
        .columns
        .iter()
        .filter(|c| !matches!(c.ty, ColumnType::Vector(_)))
        .map(|c| c.name.clone())
        .collect();
    for case in cases() {
        assert!(
            expected.remove(case.column),
            "duplicate or unexpected column in case table: {}",
            case.column
        );
    }
    assert!(
        expected.is_empty(),
        "columns missing from case table: {expected:?}"
    );
}

/// 形式不正の追加ケース（Issue #1187）。1 列 1 ケースの網羅チェックとは別に、
/// 同じ列型の書式違反・文法違反が `22P02`／`22007` で拒否され、副作用が無いことを固定する。
#[test]
fn format_violations_are_rejected_with_22p02_or_22007_and_no_side_effects() {
    let extra: [(&str, &str, &str, &str); 5] = [
        ("integer-malformed", "c_integer", "'x'", "22P02"),
        ("numeric-malformed", "c_numeric", "'abc'", "22P02"),
        ("date-malformed-format", "c_date", "'2024/01/01'", "22007"),
        (
            "timestamp-malformed-format",
            "c_timestamp",
            "'2024-01-02T03:04:05Z'",
            "22007",
        ),
        ("array-malformed", "c_array_text", "'a,b'", "22P02"),
    ];
    for (label, column, literal, expected) in extra {
        let (core, path) = new_core();
        let _guard = CleanupGuard(path);
        let alice = ctx_for("alice");
        let err = core
            .execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(1, column, literal, "op-format"),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), expected, "case {label}");
        assert_eq!(count_star(&core, &alice), 0, "case {label}: side effect");
    }
}

#[test]
fn invalid_literals_are_rejected_with_expected_wire_code_and_no_side_effects() {
    for case in cases() {
        let (core, path) = new_core();
        let _guard = CleanupGuard(path);
        let alice = ctx_for("alice");
        let mut session = SessionState::default();

        let err = core
            .execute_sql_in_session(
                &alice,
                &mut session,
                &insert_sql(1, case.column, &case.invalid_literal, "op-invalid"),
            )
            .unwrap_err();
        assert_eq!(
            err.wire_code(),
            case.expected_wire_code,
            "case {}: unexpected wire_code",
            case.label
        );
        assert_eq!(
            count_star(&core, &alice),
            0,
            "case {}: rejected insert must have no side effects",
            case.label
        );

        // 同じ operation_id で正しい値を再送すると成功する（台帳に記録されて
        // いないため再送は新規操作として受理される）。
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &insert_sql(1, case.column, case.valid_literal, "op-invalid"),
        )
        .unwrap_or_else(|e| {
            panic!(
                "case {}: resend with valid literal should succeed: {e:?}",
                case.label
            )
        });
        assert_eq!(
            count_star(&core, &alice),
            1,
            "case {}: resend should insert exactly one row",
            case.label
        );
    }
}

#[test]
fn boundary_exact_literals_are_accepted_and_read_back() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, c_integer, c_bigint, c_date, c_timestamp) \
             VALUES (1, '[0.1,0.2]', {}, {}, '0001-01-01', '0001-01-01 00:00:00') \
             USING OPERATION_ID 'op-boundary-1'",
            i32::MIN,
            i64::MAX
        ),
    )
    .expect("boundary values at the lower/upper ends should be accepted");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT c_integer, c_bigint FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].cells,
        vec![
            Cell::SignedInteger(i64::from(i32::MIN)),
            Cell::SignedInteger(i64::MAX),
        ]
    );

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, c_integer, c_date, c_timestamp) \
             VALUES (2, '[0.1,0.2]', {}, '9999-12-31', '9999-12-31 23:59:59.999999') \
             USING OPERATION_ID 'op-boundary-2'",
            i32::MAX
        ),
    )
    .expect("boundary values at the other end should be accepted");
    assert_eq!(count_star(&core, &alice), 2);
}

#[test]
fn rejection_response_does_not_depend_on_other_tenant_boundary_row() {
    // RLS-9: 同じ範囲外リテラルの拒否応答（wire_code）が、他テナントの境界値行の
    // 有無によって変わらないことを固定する。integer overflow と enum unknown
    // label の 2 ケースを代表として検証する。
    for case_column in ["c_integer", "c_enum"] {
        let case = cases()
            .into_iter()
            .find(|c| c.column == case_column)
            .expect("case exists");

        // ケース A: 他テナント（bob）の境界値行が存在しない状態。
        let (core_a, path_a) = new_core();
        let _guard_a = CleanupGuard(path_a);
        let alice_a = ctx_for("alice");
        let err_a = core_a
            .execute_sql_in_session(
                &alice_a,
                &mut SessionState::default(),
                &insert_sql(1, case.column, &case.invalid_literal, "op-a"),
            )
            .unwrap_err();

        // ケース B: 他テナント（bob）が同じ列の境界値行を先に持っている状態。
        let (core_b, path_b) = new_core();
        let _guard_b = CleanupGuard(path_b);
        let bob = ctx_for("bob");
        core_b
            .execute_sql_in_session(
                &bob,
                &mut SessionState::default(),
                &insert_sql(999, case.column, case.valid_literal, "op-bob-seed"),
            )
            .expect("bob's boundary row should be accepted");
        let alice_b = ctx_for("alice");
        let err_b = core_b
            .execute_sql_in_session(
                &alice_b,
                &mut SessionState::default(),
                &insert_sql(1, case.column, &case.invalid_literal, "op-a"),
            )
            .unwrap_err();

        assert_eq!(
            err_a.wire_code(),
            err_b.wire_code(),
            "case {}: wire_code must not depend on other tenant's row",
            case.label
        );
        assert_eq!(
            err_a.to_string(),
            err_b.to_string(),
            "case {}: error message must not depend on other tenant's row",
            case.label
        );

        // 他テナント（bob）の行は alice の scan からは見えない。
        let alice_scan = core_b
            .execute_sql(
                &alice_b,
                &format!("SELECT id FROM {TABLE} WHERE id = 999 LIMIT 10"),
            )
            .expect("scan should succeed");
        assert!(
            alice_scan.rows.is_empty(),
            "case {}: alice must not see bob's row",
            case.label
        );
        assert_eq!(
            count_star(&core_b, &alice_b),
            0,
            "case {}: alice's COUNT(*) must not include bob's row",
            case.label
        );
    }
}
