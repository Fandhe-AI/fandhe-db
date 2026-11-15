//! `VARCHAR[]`・`VARCHAR(n)[]`・`CHAR(n)[]` 配列要素型（`ArrayElemType::Varchar`／`Bpchar`。
//! Issue #3231）の結合テスト。ポインタ: TABLE-14・TABLE-31・TABLE-35。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 宣言（CREATE TABLE・ADD COLUMN）が
//! 受理されカタログ型が再オープン後も保たれること、(2) 代入（INSERT・UPDATE 等）が要素ごとに
//! スカラー `VARCHAR(n)`／`CHAR(n)` と同じ長さ検査（`22001`・末尾空白の切り詰め・空白詰め）を
//! 行うこと、(3) 明示 CAST が黙って切り詰めること、(4) 比較で `CHAR(n)` の末尾空白が
//! 無視されること、(5) UNIQUE・RLS・fail-closed を固定する。wire 公告は wire-server 側のテストが担う。

use engine::catalog::{ArrayElemType, ColumnType};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        path,
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run_as(core, "alice", sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn err(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    run_as(core, "alice", sql).expect_err(&format!("{sql} must fail"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    err(core, sql).wire_code().to_string()
}

/// 先頭行・先頭セルの表示用テキスト（配列は PostgreSQL テキスト形式。NULL は `NULL`）。
fn cell_text(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let result = core
        .execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    let cell = result
        .rows
        .first()
        .and_then(|r| r.cells.first())
        .unwrap_or_else(|| panic!("{sql} returned no cell"));
    match cell {
        Cell::Array(a) => a.to_pg_text(),
        Cell::Text(t) => t.clone(),
        Cell::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn txt(core: &EngineCore, sql: &str) -> String {
    cell_text(core, "alice", sql)
}

fn row_count(core: &EngineCore, tenant: &str, sql: &str) -> usize {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .len()
}

fn column_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

fn array_elem(ty: &ColumnType) -> ArrayElemType {
    match ty {
        ColumnType::Array(a) => a.elem(),
        other => panic!("expected an array column, got {other:?}"),
    }
}

fn setup(core: &EngineCore) {
    ok(
        core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(3)[], c CHAR(3)[], u VARCHAR[], tx TEXT[])",
    );
}

#[test]
fn declarations_are_accepted_and_persist_across_reopen() {
    let (path, _g, core) = open("ace-decl");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a VARCHAR[], b VARCHAR(3)[], \
         c CHARACTER VARYING(3)[2], d CHAR(3)[], e CHAR(3)[][], f CHARACTER(4)[], g TEXT[])",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN h VARCHAR(5)[]");
    ok(&core, "ALTER TABLE t ADD COLUMN i CHAR(2)[]");
    drop(core);
    assert_eq!(
        array_elem(&column_type(&path, "t", "a")),
        ArrayElemType::Varchar(None)
    );
    assert_eq!(
        array_elem(&column_type(&path, "t", "b")),
        ArrayElemType::Varchar(Some(3))
    );
    assert_eq!(
        array_elem(&column_type(&path, "t", "c")),
        ArrayElemType::Varchar(Some(3))
    );
    assert_eq!(
        array_elem(&column_type(&path, "t", "d")),
        ArrayElemType::Bpchar(3)
    );
    match column_type(&path, "t", "e") {
        ColumnType::Array(a) => {
            assert_eq!(a.elem(), ArrayElemType::Bpchar(3));
            assert_eq!(a.ndims(), 2);
        }
        other => panic!("expected an array, got {other:?}"),
    }
    assert_eq!(
        array_elem(&column_type(&path, "t", "f")),
        ArrayElemType::Bpchar(4)
    );
    assert_eq!(
        array_elem(&column_type(&path, "t", "g")),
        ArrayElemType::Text
    );
    assert_eq!(
        array_elem(&column_type(&path, "t", "h")),
        ArrayElemType::Varchar(Some(5))
    );
    assert_eq!(
        array_elem(&column_type(&path, "t", "i")),
        ArrayElemType::Bpchar(2)
    );
    // 再オープン後にも書き込み・読み出しできる。
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    ok(
        &core,
        "INSERT INTO t (id, b, d) VALUES (1, '{abc}', '{a}') USING OPERATION_ID 'r1'",
    );
    assert_eq!(txt(&core, "SELECT b FROM t WHERE id = 1 LIMIT 1"), "{abc}");
    assert_eq!(
        txt(&core, "SELECT d FROM t WHERE id = 1 LIMIT 1"),
        "{\"a  \"}"
    );
}

#[test]
fn unsupported_declarations_stay_fail_closed() {
    let (_p, _g, core) = open("ace-reject");
    // 長さなしの CHAR は要素型でも 0A000 のまま。
    assert_eq!(
        code(&core, "CREATE TABLE u (id BIGINT PRIMARY KEY, a CHAR[])"),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            "CREATE TABLE u (id BIGINT PRIMARY KEY, a CHARACTER[])"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            "CREATE TABLE u (id BIGINT PRIMARY KEY, a VARCHAR(0)[])"
        ),
        "22023"
    );
    // 未対応の隣接要素型は従来どおり拒否する。
    assert_eq!(
        code(
            &core,
            "CREATE TABLE u (id BIGINT PRIMARY KEY, a TIMESTAMPTZ[])"
        ),
        "0A000"
    );
}

#[test]
fn assignment_applies_element_length_rules() {
    let (_p, _g, core) = open("ace-assign");
    setup(&core);
    // 収まる値・NULL 要素。
    ok(
        &core,
        "INSERT INTO t (id, v, c) VALUES (1, '{ab,NULL,abc}', '{a,NULL}') USING OPERATION_ID 'a1'",
    );
    assert_eq!(
        txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"),
        "{ab,NULL,abc}"
    );
    // CHAR(n) は空白詰め。
    assert_eq!(
        txt(&core, "SELECT c FROM t WHERE id = 1 LIMIT 1"),
        "{\"a  \",NULL}"
    );
    // 超過は 22001。値は文言へ反射しない。
    let e = err(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '{abcd}') USING OPERATION_ID 'a2'",
    );
    assert_eq!(e.wire_code(), "22001");
    assert!(!e.to_string().contains("abcd"), "{e}");
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, c) VALUES (2, '{abcd}') USING OPERATION_ID 'a3'"
        ),
        "22001"
    );
    // 超過部分が半角空白のみなら切り詰める。
    ok(
        &core,
        "INSERT INTO t (id, v, c) VALUES (3, '{\"ab    \"}', '{\"a      \"}') USING OPERATION_ID 'a4'",
    );
    assert_eq!(
        txt(&core, "SELECT v FROM t WHERE id = 3 LIMIT 1"),
        "{\"ab \"}"
    );
    assert_eq!(
        txt(&core, "SELECT c FROM t WHERE id = 3 LIMIT 1"),
        "{\"a  \"}"
    );
    // ARRAY[...] 式。
    ok(
        &core,
        "INSERT INTO t (id, v, c) VALUES (4, ARRAY['x','yy'], ARRAY['p']) USING OPERATION_ID 'a5'",
    );
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 4 LIMIT 1"), "{x,yy}");
    assert_eq!(
        txt(&core, "SELECT c FROM t WHERE id = 4 LIMIT 1"),
        "{\"p  \"}"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v) VALUES (5, ARRAY['toolong']) USING OPERATION_ID 'a6'"
        ),
        "22001"
    );
    // 長さなしの VARCHAR[] は TEXT[] と同じ（長さ制限なし）。
    ok(
        &core,
        "INSERT INTO t (id, u) VALUES (6, '{a long long value}') USING OPERATION_ID 'a7'",
    );
    assert_eq!(
        txt(&core, "SELECT u FROM t WHERE id = 6 LIMIT 1"),
        "{\"a long long value\"}"
    );
    // 要素数の上限 `[N]` 超過は 54000。
    ok(
        &core,
        "CREATE TABLE lim (id BIGINT PRIMARY KEY, a VARCHAR(3)[2])",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO lim (id, a) VALUES (1, '{a,b,c}') USING OPERATION_ID 'l1'"
        ),
        "54000"
    );
}

#[test]
fn update_and_expressions_normalize_elements() {
    let (_p, _g, core) = open("ace-update");
    setup(&core);
    ok(
        &core,
        "INSERT INTO t (id, v, c, tx) VALUES (1, '{a}', '{b}', '{longer,x}') USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "UPDATE t SET v = '{zz}', c = '{q}' WHERE id = 1 USING OPERATION_ID 'u2'",
    );
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"), "{zz}");
    assert_eq!(
        txt(&core, "SELECT c FROM t WHERE id = 1 LIMIT 1"),
        "{\"q  \"}"
    );
    // 式の結果（array_append）も列の格納形へ正規化される。
    ok(
        &core,
        "UPDATE t SET v = array_append(v, 'k'), c = array_append(c, 'r') WHERE id = 1 USING OPERATION_ID 'u3'",
    );
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"), "{zz,k}");
    assert_eq!(
        txt(&core, "SELECT c FROM t WHERE id = 1 LIMIT 1"),
        "{\"q  \",\"r  \"}"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = array_append(v, 'toolong') WHERE id = 1 USING OPERATION_ID 'u4'"
        ),
        "22001"
    );
    // 長い TEXT[] 列からの代入も要素ごとに検査される。
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = tx WHERE id = 1 USING OPERATION_ID 'u5'"
        ),
        "22001"
    );
    // 失敗した UPDATE は値を変えない。
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"), "{zz,k}");
}

#[test]
fn explicit_casts_truncate_and_pad_silently() {
    let (_p, _g, core) = open("ace-cast");
    setup(&core);
    assert_eq!(
        txt(&core, "SELECT '{abcd,ef}'::varchar(3)[] LIMIT 1"),
        "{abc,ef}"
    );
    assert_eq!(txt(&core, "SELECT '{ab}'::char(3)[] LIMIT 1"), "{\"ab \"}");
    assert_eq!(txt(&core, "SELECT '{abcde}'::char(3)[] LIMIT 1"), "{abc}");
    assert_eq!(
        txt(
            &core,
            "SELECT CAST('{a,b}' AS character varying(2)[]) LIMIT 1"
        ),
        "{a,b}"
    );
    assert_eq!(txt(&core, "SELECT '{a,b}'::varchar[] LIMIT 1"), "{a,b}");
    ok(
        &core,
        "INSERT INTO t (id, v, c, tx) VALUES (1, '{ab}', '{a}', '{abcdef}') USING OPERATION_ID 'c1'",
    );
    // CHAR(n)[] → TEXT[] は各要素の末尾空白を除く。
    assert_eq!(
        txt(
            &core,
            "SELECT CAST(c AS text[]) FROM t WHERE id = 1 LIMIT 1"
        ),
        "{a}"
    );
    assert_eq!(
        txt(
            &core,
            "SELECT CAST(tx AS char(2)[]) FROM t WHERE id = 1 LIMIT 1"
        ),
        "{ab}"
    );
    assert_eq!(
        txt(
            &core,
            "SELECT CAST(tx AS varchar(3)[]) FROM t WHERE id = 1 LIMIT 1"
        ),
        "{abc}"
    );
}

#[test]
fn char_padding_is_bounded_before_allocation() {
    let (_p, _g, core) = open("ace-dos");
    ok(
        &core,
        "CREATE TABLE big (id BIGINT PRIMARY KEY, a CHAR(10485760)[])",
    );
    // 数要素の空白詰めが配列ペイロード上限を超える場合は確保前に 54000。
    for sql in [
        "INSERT INTO big (id, a) VALUES (1, '{a,b,c,d,e}') USING OPERATION_ID 'd1'",
        "SELECT '{a,b,c,d,e}'::char(10485760)[] LIMIT 1",
    ] {
        assert_eq!(code(&core, sql), "54000", "{sql}");
    }
    // NULL 要素は空白詰めされないため確保量に数えない。
    ok(
        &core,
        "INSERT INTO big (id, a) VALUES (2, '{NULL,NULL}') USING OPERATION_ID 'd2'",
    );
    assert_eq!(
        txt(&core, "SELECT '{NULL,NULL}'::char(10485760)[] LIMIT 1"),
        "{NULL,NULL}"
    );
}

#[test]
fn subscripts_and_slices_return_element_types() {
    let (_p, _g, core) = open("ace-subscript");
    setup(&core);
    ok(
        &core,
        "INSERT INTO t (id, v, c) VALUES (1, '{ab,cd,ef}', '{x,y,z}') USING OPERATION_ID 's1'",
    );
    assert_eq!(txt(&core, "SELECT v[1] FROM t WHERE id = 1 LIMIT 1"), "ab");
    // 添字の結果は空白詰めのまま（PostgreSQL の bpchar と同じ）。
    assert_eq!(txt(&core, "SELECT c[2] FROM t WHERE id = 1 LIMIT 1"), "y  ");
    assert_eq!(
        txt(&core, "SELECT v[2:3] FROM t WHERE id = 1 LIMIT 1"),
        "{cd,ef}"
    );
}

#[test]
fn comparisons_ignore_char_padding_and_mix_with_text() {
    let (_p, _g, core) = open("ace-compare");
    setup(&core);
    ok(
        &core,
        "INSERT INTO t (id, v, c, tx) VALUES (1, '{ab,cd}', '{a,b}', '{ab,cd}') USING OPERATION_ID 'k1'",
    );
    for (sql, want) in [
        ("SELECT id FROM t WHERE c = '{a,b}' LIMIT 10", 1),
        (
            "SELECT id FROM t WHERE c = ARRAY['a','b']::text[] LIMIT 10",
            1,
        ),
        ("SELECT id FROM t WHERE c <> '{a,c}' LIMIT 10", 1),
        ("SELECT id FROM t WHERE c = '{a,c}' LIMIT 10", 0),
        ("SELECT id FROM t WHERE 'a' = ANY(c) LIMIT 10", 1),
        ("SELECT id FROM t WHERE 'z' = ANY(c) LIMIT 10", 0),
        ("SELECT id FROM t WHERE c @> ARRAY['a'] LIMIT 10", 1),
        (
            "SELECT id FROM t WHERE v = ARRAY['ab','cd']::text[] LIMIT 10",
            1,
        ),
        ("SELECT id FROM t WHERE v = tx LIMIT 10", 1),
        ("SELECT id FROM t WHERE 'ab' = ANY(v) LIMIT 10", 1),
        ("SELECT id FROM t WHERE v && ARRAY['cd'] LIMIT 10", 1),
        // 型なしリテラル側が空白詰め済みでも等しい（Issue #3231）。
        ("SELECT id FROM t WHERE c = '{\"a  \",b}' LIMIT 10", 1),
        ("SELECT id FROM t WHERE '{\"a  \",b}' = c LIMIT 10", 1),
        ("SELECT id FROM t WHERE c @> '{\"a  \"}' LIMIT 10", 1),
        ("SELECT id FROM t WHERE c && '{\"b  \"}' LIMIT 10", 1),
    ] {
        assert_eq!(row_count(&core, "alice", sql), want, "{sql}");
    }
}

#[test]
fn unique_keys_treat_char_padding_as_equal() {
    let (_p, _g, core) = open("ace-unique");
    ok(
        &core,
        "CREATE TABLE uq (id BIGINT PRIMARY KEY, c CHAR(3)[] UNIQUE, v VARCHAR(3)[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO uq (id, c, v) VALUES (1, '{a}', '{a}') USING OPERATION_ID 'q1'",
    );
    // `{a}` と `{"a  "}` は同じ CHAR(3)[] 値。
    assert_eq!(
        code(
            &core,
            "INSERT INTO uq (id, c, v) VALUES (2, '{\"a  \"}', '{b}') USING OPERATION_ID 'q2'"
        ),
        "23505"
    );
    // VARCHAR(3)[] は空白を区別する。
    ok(
        &core,
        "INSERT INTO uq (id, c, v) VALUES (3, '{b}', '{\"a \"}') USING OPERATION_ID 'q3'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO uq (id, c, v) VALUES (4, '{c}', '{a}') USING OPERATION_ID 'q4'"
        ),
        "23505"
    );
    assert_eq!(row_count(&core, "alice", "SELECT id FROM uq LIMIT 10"), 2);
}

#[test]
fn rls_keeps_tenants_apart() {
    let (_p, _g, core) = open("ace-rls");
    setup(&core);
    ok(
        &core,
        "INSERT INTO t (id, v, c) VALUES (1, '{sec}', '{a}') USING OPERATION_ID 'z1'",
    );
    // 他テナントからは行が見えない。
    assert_eq!(row_count(&core, "bob", "SELECT id FROM t LIMIT 10"), 0);
    if let Ok(o) = run_as(
        &core,
        "bob",
        "UPDATE t SET v = '{x}' WHERE id = 1 USING OPERATION_ID 'z2'",
    ) {
        assert!(!format!("{o:?}").contains("sec"));
    }
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"), "{sec}");
    // 超過エラーは他テナントの値を含まない。
    let e = run_as(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (9, '{toolong}') USING OPERATION_ID 'z3'",
    )
    .expect_err("too long");
    assert_eq!(e.wire_code(), "22001");
    assert!(!e.to_string().contains("sec"));
}

#[test]
fn dropped_array_column_keeps_table_writable() {
    let (path, _g, core) = open("ace-drop");
    setup(&core);
    ok(
        &core,
        "INSERT INTO t (id, v, c) VALUES (1, '{a}', '{b}') USING OPERATION_ID 'x1'",
    );
    ok(&core, "ALTER TABLE t DROP COLUMN v");
    ok(&core, "ALTER TABLE t DROP COLUMN c");
    ok(
        &core,
        "INSERT INTO t (id, tx) VALUES (2, '{k}') USING OPERATION_ID 'x2'",
    );
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(row_count(&core, "alice", "SELECT id FROM t LIMIT 10"), 2);
}

#[test]
fn alter_column_type_to_char_arrays_is_rejected_like_text_arrays() {
    let (_p, _g, core) = open("ace-alter");
    setup(&core);
    ok(&core, "CREATE TABLE i (id BIGINT PRIMARY KEY, a INT)");
    assert_eq!(
        code(&core, "ALTER TABLE i ALTER COLUMN a TYPE VARCHAR[]"),
        code(&core, "ALTER TABLE i ALTER COLUMN a TYPE TEXT[]")
    );
}

#[test]
fn other_write_paths_enforce_element_lengths() {
    let (_p, _g, core) = open("ace-paths");
    setup(&core);
    ok(&core, "CREATE TABLE src (id BIGINT PRIMARY KEY, tx TEXT[])");
    ok(
        &core,
        "INSERT INTO src (id, tx) VALUES (1, '{ab}'), (2, '{toolong}') USING OPERATION_ID 'p0'",
    );
    // INSERT ... SELECT。
    ok(
        &core,
        "INSERT INTO t (id, v, c) SELECT id, tx, tx FROM src WHERE id = 1 USING OPERATION_ID 'p1'",
    );
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"), "{ab}");
    assert_eq!(
        txt(&core, "SELECT c FROM t WHERE id = 1 LIMIT 1"),
        "{\"ab \"}"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v) SELECT id, tx FROM src WHERE id = 2 USING OPERATION_ID 'p2'"
        ),
        "22001"
    );
    // UPSERT の EXCLUDED 代入。
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '{zz}') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'p3'",
    );
    assert_eq!(txt(&core, "SELECT v FROM t WHERE id = 1 LIMIT 1"), "{zz}");
    // DEFAULT。
    ok(
        &core,
        "CREATE TABLE df (id BIGINT PRIMARY KEY, c CHAR(3)[] DEFAULT '{a}', v VARCHAR(2)[] DEFAULT '{x}')",
    );
    ok(
        &core,
        "INSERT INTO df (id) VALUES (1) USING OPERATION_ID 'p4'",
    );
    assert_eq!(
        txt(&core, "SELECT c FROM df WHERE id = 1 LIMIT 1"),
        "{\"a  \"}"
    );
    assert_eq!(txt(&core, "SELECT v FROM df WHERE id = 1 LIMIT 1"), "{x}");
    // ALTER COLUMN TYPE（TEXT[] → VARCHAR(n)[]）は代入変換で、長すぎる行があれば 22001 で失敗する。
    assert_eq!(
        code(&core, "ALTER TABLE src ALTER COLUMN tx TYPE VARCHAR(3)[]"),
        "22001"
    );
    ok(
        &core,
        "DELETE FROM src WHERE id = 2 USING OPERATION_ID 'p5'",
    );
    ok(&core, "ALTER TABLE src ALTER COLUMN tx TYPE CHAR(3)[]");
    assert_eq!(
        txt(&core, "SELECT tx FROM src WHERE id = 1 LIMIT 1"),
        "{\"ab \"}"
    );
}
