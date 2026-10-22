//! `COPY` の `WITH` オプション構文解析（Issue #1763・WIRE-22・TASK-294 のポインタ）の engine API
//! 層の結合テスト。簡易クエリの `EngineCore::begin_copy` と拡張クエリの Parse
//! （`parse_sql_prepared`）が同じ検証入口を共有し、同一の SQLSTATE を返すこと、旧構文と新構文が
//! 同じ結果になること、TO 側でも非既定オプションを受理して解決済みの `CopyOptions` を
//! `CopyPlan::To` に載せることを固定する。オプションの適用は FROM が Issue #1764、
//! TO が Issue #1765（いずれも適用済みでゲートは無い）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::CopyFormat;
use engine::sql::copy_options::CopyHeader;
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

/// 非既定オプションは両方向とも受理する（FROM は Issue #1764、TO は Issue #1765 で適用済み）。
const NON_DEFAULT_OPTIONS: [&str; 5] = [
    " WITH (HEADER)",
    " CSV HEADER",
    " WITH (FORMAT csv, DELIMITER ';')",
    " WITH (NULL 'x')",
    " WITH (FORMAT csv, QUOTE '''')",
];

#[test]
fn non_default_options_are_accepted_for_from() {
    let (core, _g) = open("copy-opt-from-applied");
    for opt in NON_DEFAULT_OPTIONS {
        accepted_in_both(&core, &format!("{FROM}{opt}"));
    }
}

#[test]
fn non_default_options_are_accepted_for_to_and_carried_in_plan() {
    let (core, _g) = open("copy-opt-to-applied");
    for opt in NON_DEFAULT_OPTIONS {
        accepted_in_both(&core, &format!("{TO}{opt}"));
    }
    let sql =
        format!("{TO} WITH (FORMAT csv, HEADER, DELIMITER ';', NULL 'x', QUOTE '''', ESCAPE '\\')");
    let Ok(CopyPlan::To(format, _, o)) = begin(&core, &sql) else {
        panic!("expected COPY TO plan");
    };
    assert_eq!(format, CopyFormat::Csv);
    assert_eq!(o.header(), CopyHeader::True);
    assert_eq!(o.delimiter(), b';');
    assert_eq!(o.null_string(), "x");
    assert_eq!(o.quote(), b'\'');
    assert_eq!(o.escape(), b'\\');
}
#[test]
fn unsupported_value_options_without_value_are_42601_and_with_value_0a000() {
    let (core, _g) = open("copy-opt-unsupported-value");
    for name in [
        "FORCE_QUOTE",
        "FORCE_NOT_NULL",
        "FORCE_NULL",
        "DEFAULT",
        "ON_ERROR",
        "LOG_VERBOSITY",
    ] {
        let value = match name {
            "FORCE_QUOTE" | "FORCE_NOT_NULL" | "FORCE_NULL" => "(a)",
            "DEFAULT" => "'x'",
            "ON_ERROR" => "ignore",
            _ => "verbose",
        };
        for dir in [FROM, TO] {
            assert_eq!(
                rejected_code(&core, &format!("{dir} WITH (FORMAT csv, {name})")),
                "42601",
                "{dir} {name} (no value)"
            );
            assert_eq!(
                rejected_code(&core, &format!("{dir} WITH (FORMAT csv, {name} {value})")),
                "0A000",
                "{dir} {name} (with value)"
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
    // binary は TO のみ受理（WIRE-23・Issue #1770）。FROM は Issue #1769 まで 42601。
    assert_eq!(
        rejected_code(&core, &format!("{FROM} WITH (FORMAT binary)")),
        "42601"
    );
    accepted_in_both(&core, &format!("{FROM} WITH (ENCODING UTF8)"));
    accepted_in_both(&core, &format!("{TO} WITH (ENCODING utf8)"));
    assert_eq!(
        rejected_code(&core, &format!("{FROM} WITH (ENCODING latin1)")),
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

/// `COPY TO STDOUT` の binary 受理（WIRE-23・Issue #1770）。新構文・旧構文が同じ format へ解決され、
/// 簡易クエリと Parse の分類が一致する。
#[test]
fn binary_is_accepted_for_to_in_new_and_legacy_syntax() {
    let (core, _g) = open("copy-opt-binary-to");
    for opt in [
        " WITH (FORMAT binary)",
        " WITH (FORMAT 'BINARY')",
        " BINARY",
        " WITH BINARY",
        " WITH (FORMAT binary, HEADER false)",
        " WITH (FORMAT binary, ENCODING 'UTF8')",
    ] {
        accepted_in_both(&core, &format!("{TO}{opt}"));
        let Ok(CopyPlan::To(format, ..)) = begin(&core, &format!("{TO}{opt}")) else {
            panic!("expected COPY TO plan: {opt}");
        };
        assert_eq!(format, CopyFormat::Binary, "{opt}");
    }
}

/// binary と併用できないオプションは `0A000`、FROM の binary は従来どおり `42601`。
#[test]
fn binary_conflicting_options_and_from_are_rejected() {
    let (core, _g) = open("copy-opt-binary-conflict");
    for opt in [
        " WITH (FORMAT binary, DELIMITER ',')",
        " WITH (FORMAT binary, NULL 'x')",
        " WITH (FORMAT binary, HEADER)",
        " WITH (FORMAT binary, QUOTE '''')",
        " WITH (FORMAT binary, ESCAPE 'x')",
        " BINARY DELIMITER ','",
    ] {
        assert_eq!(
            rejected_code(&core, &format!("{TO}{opt}")),
            "0A000",
            "{opt}"
        );
    }
    for opt in [" WITH (FORMAT binary)", " BINARY", " WITH BINARY"] {
        assert_eq!(
            rejected_code(&core, &format!("{FROM}{opt}")),
            "42601",
            "{opt}"
        );
    }
}
