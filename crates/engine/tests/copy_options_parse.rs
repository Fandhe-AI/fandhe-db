//! `COPY` の `WITH` オプション構文解析（Issue #1763・WIRE-22・TASK-294 のポインタ）の engine API
//! 層の結合テスト。簡易クエリの `EngineCore::begin_copy` と拡張クエリの Parse
//! （`parse_sql_prepared`）が同じ検証入口を共有し、同一の SQLSTATE を返すこと、旧構文と新構文が
//! 同じ結果になること、データ経路が未適用のオプションが `0A000` で拒否される（無言の fail-open
//! にならない）ことを固定する。オプションの適用は Issue #1764（FROM）・#1765（TO）の担当。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::CopyFormat;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("ctx")
}

const FROM: &str = "COPY documents (id, embedding, body) FROM STDIN";
const TO: &str = "COPY (SELECT id FROM documents LIMIT 10) TO STDOUT";

fn begin(
    core: &EngineCore,
    sql: &str,
) -> Result<CopyPlan, engine::sql::allowlist::SqlSurfaceError> {
    core.begin_copy(&ctx(), &SessionState::default(), sql)
}

/// 簡易クエリと Parse の SQLSTATE が一致することを確かめつつ、その SQLSTATE を返す。
fn rejected_code(core: &EngineCore, sql: &str) -> &'static str {
    let simple = begin(core, sql).map(|_| ()).expect_err("simple must fail");
    let prepared = core
        .parse_sql_prepared(sql)
        .map(|_| ())
        .expect_err("prepared must fail");
    assert_eq!(simple.wire_code(), prepared.wire_code(), "{sql}");
    simple.wire_code()
}

fn accepted_in_both(core: &EngineCore, sql: &str) {
    begin(core, sql)
        .map(|_| ())
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    core.parse_sql_prepared(sql)
        .map(|_| ())
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[test]
fn legacy_csv_is_accepted_for_from_and_to_like_with_format() {
    let (core, _g) = open("copy-opt-legacy-csv");
    for dir in [FROM, TO] {
        for opt in [
            " CSV",
            " WITH CSV",
            " WITH (FORMAT csv)",
            " WITH (format 'CSV')",
        ] {
            accepted_in_both(&core, &format!("{dir}{opt}"));
        }
    }
    let Ok(CopyPlan::To(format, ..)) = begin(&core, &format!("{TO} CSV")) else {
        panic!("expected COPY TO plan");
    };
    assert_eq!(format, CopyFormat::Csv);
}

#[test]
fn explicit_defaults_are_accepted_for_both_directions() {
    let (core, _g) = open("copy-opt-explicit-defaults");
    for dir in [FROM, TO] {
        accepted_in_both(
            &core,
            &format!("{dir} WITH (FORMAT csv, DELIMITER ',', QUOTE '\"', HEADER false)"),
        );
    }
}

#[test]
fn non_default_options_are_0a000_until_applied() {
    let (core, _g) = open("copy-opt-gate");
    for dir in [FROM, TO] {
        for opt in [
            " WITH (HEADER)",
            " CSV HEADER",
            " WITH (FORMAT csv, DELIMITER ';')",
            " WITH (NULL 'x')",
            " WITH (FORMAT csv, QUOTE '''')",
        ] {
            assert_eq!(
                rejected_code(&core, &format!("{dir}{opt}")),
                "0A000",
                "{dir}{opt}"
            );
        }
    }
}

#[test]
fn classification_matches_between_simple_and_parse() {
    let (core, _g) = open("copy-opt-class");
    let cases = [
        ("WITH (HEADER, HEADER)", "42601"),
        ("WITH (HEADER, FOO 1)", "42601"),
        ("WITH ()", "42601"),
        ("WITH (FORMAT csv,)", "42601"),
        ("WITH (FORMAT csv", "42601"),
        ("WITH (DELIMITER)", "42601"),
        ("WITH", "42601"),
        ("CSV (HEADER)", "42601"),
        ("CSV BINARY", "42601"),
        ("HEADER HEADER", "42601"),
        ("WITH (FORMAT binary)", "42601"),
        ("WITH (FORMAT json)", "22023"),
        ("WITH (HEADER maybe)", "22023"),
        ("WITH (DELIMITER 1)", "22023"),
        ("WITH (QUOTE '''')", "0A000"),
        ("WITH (FORMAT csv, DELIMITER ';;')", "0A000"),
        ("WITH (FORMAT csv, FORCE_QUOTE *)", "0A000"),
        ("WITH (FORMAT csv, FORCE_NOT_NULL (body))", "0A000"),
        ("CSV FORCE QUOTE *", "0A000"),
        ("WITH (FREEZE)", "0A000"),
        ("WITH (ENCODING 'LATIN1')", "0A000"),
    ];
    for dir in [FROM, TO] {
        for (opt, want) in cases {
            assert_eq!(
                rejected_code(&core, &format!("{dir} {opt}")),
                want,
                "{dir} {opt}"
            );
        }
    }
    assert_eq!(
        rejected_code(&core, &format!("{TO} WITH (HEADER MATCH)")),
        "0A000"
    );
    accepted_in_both(&core, &format!("{FROM} WITH (ENCODING 'utf8')"));
    accepted_in_both(&core, &format!("{TO} WITH (ENCODING 'UTF-8')"));
}

#[test]
fn error_messages_do_not_echo_values() {
    let (core, _g) = open("copy-opt-no-echo");
    for sql in [
        format!("{FROM} WITH (FORMAT secretfmt)"),
        format!("{FROM} WITH (DELIMITER 'secretdelim')"),
        format!("{FROM} WITH (HEADER secretval)"),
        format!("{FROM} WITH ('secret')"),
        format!("{FROM} WITH (FORCE_QUOTE ('secret'))"),
        format!("{FROM} DELIMITER secretval"),
        format!("{FROM} FORCE secretval"),
        format!("{FROM} FORCE QUOTE ('secretcol')"),
        format!("{FROM} FORCE NOT secretval"),
    ] {
        let e = begin(&core, &sql).map(|_| ()).expect_err("rejected");
        assert!(!e.to_string().contains("secret"), "{e}");
    }
}

#[test]
fn unknown_option_name_is_truncated_in_message() {
    let (core, _g) = open("copy-opt-trunc");
    let long = "x".repeat(200);
    let e = begin(&core, &format!("{FROM} WITH ({long})"))
        .map(|_| ())
        .expect_err("rejected");
    assert!(e.to_string().len() < 120, "{e}");
}
