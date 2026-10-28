//! Parse で宣言された型と文脈の型の変換可否の判定（Issue #2437）の結合テスト。
//!
//! ポインタ: WIRE-24 (1)(5)・SQL-41・SQL-45。規則は公開済みの
//! `docs/design/cast-conversion-rules.md` の規則表（代入の位置は恒等・暗黙・代入・入出力代入、
//! 比較の位置は暗黙の変換がどちらかの向きにあること）。変換できない組は代入の位置で
//! `42804`、比較・関数引数の位置で `42883`。実 `Storage` 越しに `parse_sql_prepared_with_declared`
//! を直接呼んで固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::params::DeclaredParamType as D;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE m (id BIGINT PRIMARY KEY, sm SMALLINT, n INTEGER, b BIGINT, r REAL, d DOUBLE PRECISION, \
         nu NUMERIC(10,2), flag BOOLEAN, t TEXT, vc VARCHAR(20), ch CHAR(5), dt DATE, \
         tm TIME, ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, js JSON, jb JSONB, u UUID, \
         bt BYTEA, v VECTOR(3), arr INTEGER[])",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

/// 受理なら `None`、拒否なら SQLSTATE。
fn code(core: &EngineCore, sql: &str, declared: D) -> Option<String> {
    core.parse_sql_prepared_with_declared(sql, &[Some(declared)])
        .err()
        .map(|e| e.wire_code().to_string())
}

fn set(col: &str) -> String {
    format!("UPDATE m SET {col} = $1 WHERE id = 1 USING OPERATION_ID 'op-1'")
}
fn ins(col: &str) -> String {
    format!("INSERT INTO m (id, {col}) VALUES (1, $1) USING OPERATION_ID 'op-1'")
}
fn whr(col: &str) -> String {
    format!("SELECT id FROM m WHERE {col} = $1 LIMIT 5")
}

/// 代入の位置: 変換できない組は `42804`、できる組は受理する。
#[test]
fn assignment_rejects_unconvertible_and_accepts_convertible() {
    let (core, _g) = setup("decl-conv-assign");
    let rejected: [(&str, D); 8] = [
        ("n", D::Text),
        ("n", D::Varchar),
        ("n", D::Bool),
        ("n", D::Date),
        ("flag", D::Int4),
        ("u", D::Text),
        ("dt", D::Int4),
        ("id", D::Text),
    ];
    for (col, ty) in rejected {
        assert_eq!(
            code(&core, &set(col), ty).as_deref(),
            Some("42804"),
            "set {col} {ty:?}"
        );
        assert_eq!(
            code(&core, &ins(col), ty).as_deref(),
            Some("42804"),
            "ins {col} {ty:?}"
        );
    }
    let accepted: [(&str, D); 9] = [
        ("n", D::Int2),
        ("n", D::Int8),
        ("n", D::Float8),
        ("n", D::Numeric),
        ("t", D::Int4),
        ("t", D::Bool),
        ("t", D::Uuid),
        ("d", D::Int4),
        ("tz", D::Date),
    ];
    for (col, ty) in accepted {
        assert_eq!(code(&core, &set(col), ty), None, "set {col} {ty:?}");
        assert_eq!(code(&core, &ins(col), ty), None, "ins {col} {ty:?}");
    }
    // 括弧付き `$n` 単独の式セグメントも代入の位置。
    let sql = "UPDATE m SET n = ($1) WHERE id = 1 USING OPERATION_ID 'op-1'";
    assert_eq!(code(&core, sql, D::Text).as_deref(), Some("42804"));
    assert_eq!(code(&core, sql, D::Int8), None);
}

/// 比較の位置: 暗黙の変換がどちらの向きにもなければ `42883`。型をまたぐ比較は対称に通す。
#[test]
fn comparison_rejects_unconvertible_and_is_symmetric() {
    let (core, _g) = setup("decl-conv-cmp");
    let rejected: [(&str, D); 8] = [
        ("n", D::Text),
        ("n", D::Varchar),
        ("n", D::Bool),
        ("t", D::Int8),
        ("u", D::Text),
        ("dt", D::Text),
        ("id", D::Text),
        ("flag", D::Int4),
    ];
    for (col, ty) in rejected {
        assert_eq!(
            code(&core, &whr(col), ty).as_deref(),
            Some("42883"),
            "{col} {ty:?}"
        );
    }
    let accepted: [(&str, D); 9] = [
        ("n", D::Int8),
        ("n", D::Int2),
        ("n", D::Float4),
        ("n", D::Numeric),
        ("id", D::Int4),
        ("id", D::Int8),
        ("t", D::Varchar),
        ("t", D::Bpchar),
        ("ts", D::Date),
    ];
    for (col, ty) in accepted {
        assert_eq!(code(&core, &whr(col), ty), None, "{col} {ty:?}");
    }
}

/// 式の中: 演算の相手・組み込み関数の文字列引数・`CASE` の条件内の比較。`concat` は任意型を受ける。
#[test]
fn expression_positions_are_checked() {
    let (core, _g) = setup("decl-conv-expr");
    let upd = |expr: &str, col: &str| {
        format!("UPDATE m SET {col} = {expr} WHERE id = 1 USING OPERATION_ID 'op-1'")
    };
    assert_eq!(
        code(&core, &upd("n + $1", "n"), D::Text).as_deref(),
        Some("42883")
    );
    assert_eq!(code(&core, &upd("n + $1", "n"), D::Int8), None);
    assert_eq!(
        code(&core, &upd("upper($1)", "t"), D::Int4).as_deref(),
        Some("42883")
    );
    assert_eq!(code(&core, &upd("upper($1)", "t"), D::Varchar), None);
    assert_eq!(code(&core, &upd("concat(t, $1)", "t"), D::Int4), None);
    assert_eq!(
        code(
            &core,
            &upd("CASE WHEN n = $1 THEN 1 ELSE 2 END", "n"),
            D::Text
        )
        .as_deref(),
        Some("42883"),
    );
}

/// 判定対象外: ベクトル位置は text 系のみ、`USING OPERATION_ID $n` は従来どおり、
/// 未参照番号・推論不能位置の宣言型は受理する。
#[test]
fn out_of_scope_positions_keep_legacy_behavior() {
    let (core, _g) = setup("decl-conv-scope");
    let vec_sql = "SELECT id FROM m ORDER BY v <=> $1 LIMIT 5";
    assert_eq!(code(&core, vec_sql, D::Text), None);
    assert_eq!(code(&core, vec_sql, D::Varchar), None);
    assert_eq!(code(&core, vec_sql, D::Int4).as_deref(), Some("42883"));
    let op = "INSERT INTO m (id, n) VALUES (1, 1) USING OPERATION_ID $1";
    for ty in [D::Int4, D::Bool, D::Uuid, D::Text] {
        assert_eq!(code(&core, op, ty), None, "{ty:?}");
    }
    let unref = "SELECT id FROM m WHERE n = $2 LIMIT 5";
    assert!(core
        .parse_sql_prepared_with_declared(unref, &[Some(D::Text), Some(D::Int4)])
        .is_ok());
}

/// 公告型の echo 不変条件: 各列について、ParameterDescription が公告する型をそのまま
/// 宣言すると代入・比較の位置で受理される。
#[test]
fn announced_types_are_always_accepted() {
    let (core, _g) = setup("decl-conv-echo");
    let cols: [(&str, D); 20] = [
        ("id", D::Numeric),
        ("sm", D::Int2),
        ("n", D::Int4),
        ("b", D::Int8),
        ("r", D::Float4),
        ("d", D::Float8),
        ("nu", D::Numeric),
        ("flag", D::Bool),
        ("t", D::Text),
        ("vc", D::Varchar),
        ("ch", D::Bpchar),
        ("dt", D::Date),
        ("tm", D::Time),
        ("ts", D::Timestamp),
        ("tz", D::TimestampTz),
        ("iv", D::Interval),
        ("js", D::Json),
        ("jb", D::Jsonb),
        ("u", D::Uuid),
        ("bt", D::Bytea),
    ];
    for (col, ty) in cols {
        assert_eq!(code(&core, &whr(col), ty), None, "where {col} {ty:?}");
        if col != "id" {
            assert_eq!(code(&core, &set(col), ty), None, "set {col} {ty:?}");
        }
    }
    // ベクトル・配列列は text で公告される。
    for col in ["v", "arr"] {
        assert_eq!(code(&core, &set(col), D::Text), None, "set {col}");
    }
}

/// 構文・名前解決のエラーは型エラーより先に返り、宣言の有無で応答コードが変わらない。
#[test]
fn syntax_and_name_errors_precede_conversion_errors() {
    let (core, _g) = setup("decl-conv-order");
    let syntax = "SELECT FROM WHERE n = $1";
    let none = core
        .parse_sql_prepared_with_declared(syntax, &[None])
        .err()
        .map(|e| e.wire_code().to_string());
    assert_eq!(code(&core, syntax, D::Text), none);
    for sql in [
        "SELECT id FROM m WHERE no_such_col = $1 LIMIT 5",
        "SELECT id FROM no_such_table WHERE n = $1 LIMIT 5",
    ] {
        let none = core
            .parse_sql_prepared_with_declared(sql, &[None])
            .err()
            .map(|e| e.wire_code().to_string());
        assert_eq!(code(&core, sql, D::Text), none, "{sql}");
    }
}

/// 拒否文言は `$n` と静的な型名だけで、値・表名を含まない。受理した宣言の NULL は
/// 従来どおり束縛できる。
#[test]
fn messages_are_static_and_accepted_null_binds() {
    let (core, _g) = setup("decl-conv-msg");
    let err = core
        .parse_sql_prepared_with_declared(&whr("n"), &[Some(D::Text)])
        .expect_err("mismatch");
    let text = format!("{err:?}");
    assert!(text.contains("$1") && text.contains("text") && text.contains("integer"));
    assert!(!text.contains(" m ") && !text.contains("'m'"), "{text}");
    let prepared = core
        .parse_sql_prepared_with_declared(&whr("n"), &[Some(D::Int8)])
        .expect("accepted");
    assert!(core.bind_prepared(&prepared, &[None]).is_ok());
}
