//! 配列列の要素型 `NUMERIC`・`BYTEA`・`ENUM`・`JSON`・`JSONB`（Issue #1357。ポインタ:
//! TABLE-14・NOSQL-17・TASK-198、関連 TABLE-6・TABLE-13・ERR-1/2/4/6）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → DDL 権限ゲート →
//! 型解決・`Storage` 反映 → 束縛 → 行コーデック）を production 経路として検証する。
//! 宣言 → 書き込み → 再起動 → 読み出しの往復、NULL 要素と等価述語、`JSON` 要素の値等価、
//! UNIQUE・`DROP TYPE`／`ALTER TYPE` の依存判定（`enum[]` 列）、エラー分類、テナント分離を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::row_codec::ArrayValue as V;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn core_at(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut allowed(), sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, "alice", sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn ids(core: &EngineCore, tenant: &str, table: &str, predicate: &str) -> Vec<u64> {
    let sql = format!("SELECT id FROM {table} WHERE {predicate} LIMIT 100");
    let mut v: Vec<u64> = core
        .execute_sql(&ctx(tenant), &sql)
        .unwrap_or_else(|e| panic!("{sql} failed: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

const CREATE_ALL: &str = "CREATE TABLE ext (embedding VECTOR(2) NOT NULL, \
    nums NUMERIC(5,2)[4], blobs BYTEA[4], js JSON[4], jb JSONB[4], moods mood[4])";

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        ok(&core, "CREATE TYPE mood AS ENUM ('happy', 'sad', 'angry')");
        ok(&core, CREATE_ALL);
    }
    (path, guard)
}

const ROW1: &str = "INSERT INTO ext (id, embedding, nums, blobs, js, jb, moods) VALUES \
    (1, '[0.1,0.2]', '{1.5,NULL,-3.25}', '{\"\\\\x0102\",NULL,\"\\\\x\"}', \
     '{\"{ \\\"b\\\" : 1, \\\"a\\\" : [1] }\",NULL}', '{\"{ \\\"b\\\" : 1, \\\"a\\\" : [1] }\",NULL}', \
     '{happy,NULL,angry}') USING OPERATION_ID 'ext-1'";

#[test]
fn declare_write_restart_read_roundtrips_every_new_element_type() {
    let (path, _g) = setup("ext-roundtrip");
    {
        let core = core_at(&path);
        ok(&core, ROW1);
    }
    let core = core_at(&path);
    let result = core
        .execute_sql(
            &ctx("alice"),
            "SELECT nums, blobs, js, jb, moods FROM ext WHERE id = 1 LIMIT 1",
        )
        .expect("select");
    let cells: Vec<&Cell> = result.rows[0].cells.iter().collect();
    let array = |i: usize| match cells[i] {
        Cell::Array(v) => v.clone(),
        other => panic!("expected array at {i}, got {other:?}"),
    };
    assert_eq!(
        array(0),
        V::Numeric(vec![
            Some(Decimal::from_parts(150, 2).expect("d")),
            None,
            Some(Decimal::from_parts(-325, 2).expect("d")),
        ])
    );
    assert_eq!(
        array(1),
        V::Bytea(vec![Some(vec![1, 2]), None, Some(vec![])])
    );
    // JSON は入力テキストを保持し、JSONB は正規化する。
    assert_eq!(
        array(2),
        V::Json(vec![Some(r#"{ "b" : 1, "a" : [1] }"#.to_string()), None])
    );
    assert_eq!(
        array(3),
        V::Jsonb(vec![Some(r#"{"a":[1],"b":1}"#.to_string()), None])
    );
    assert_eq!(
        array(4),
        V::Enum(vec![
            Some("happy".to_string()),
            None,
            Some("angry".to_string())
        ])
    );
}

#[test]
fn equality_in_and_is_null_work_with_null_elements() {
    let (path, _g) = setup("ext-equality");
    let core = core_at(&path);
    ok(&core, ROW1);
    ok(
        &core,
        "INSERT INTO ext (id, embedding) VALUES (2, '[0.1,0.2]') USING OPERATION_ID 'ext-2'",
    );
    let q = |p: &str| ids(&core, "alice", "ext", p);
    // NUMERIC は位取りが違う表記でも列の位取りへ正規化されて等価。
    assert_eq!(q("nums = '{1.50,NULL,-3.25}'"), vec![1]);
    assert_eq!(q("nums = '{1.5,-3.25}'"), Vec::<u64>::new());
    assert_eq!(q("nums IN ('{9}','{1.5,NULL,-3.25}')"), vec![1]);
    assert_eq!(q("blobs = '{\"\\\\x0102\",NULL,\"\\\\x\"}'"), vec![1]);
    assert_eq!(q("moods = '{happy,NULL,angry}'"), vec![1]);
    assert_eq!(q("moods = '{happy,angry}'"), Vec::<u64>::new());
    assert_eq!(q("nums IS NULL"), vec![2]);
    assert_eq!(q("nums IS NOT NULL AND moods IS NOT NULL"), vec![1]);
    // 三値論理: 列 NULL の行は NOT でも一致しない。
    assert_eq!(q("NOT moods = '{sad}'"), vec![1]);
}

#[test]
fn json_elements_compare_by_value_like_scalar_json_columns() {
    let (path, _g) = setup("ext-json-eq");
    let core = core_at(&path);
    ok(&core, ROW1);
    let q = |p: &str| ids(&core, "alice", "ext", p);
    // 空白・キー順・数値表記（1 と 1.0）が違っても値として等価。
    assert_eq!(
        q("js = '{\"{\\\"a\\\":[1.0],\\\"b\\\":1}\",NULL}'"),
        vec![1]
    );
    assert_eq!(
        q("jb = '{\"{\\\"a\\\":[1.0],\\\"b\\\":1}\",NULL}'"),
        vec![1]
    );
    assert_eq!(
        q("js = '{\"{\\\"a\\\":[2],\\\"b\\\":1}\",NULL}'"),
        Vec::<u64>::new()
    );
}

#[test]
fn literal_errors_use_scalar_column_classes_and_precede_writes() {
    let (path, _g) = setup("ext-errors");
    let core = core_at(&path);
    let attempt = |column: &str, literal: &str| {
        code(
            &core,
            &format!(
                "INSERT INTO ext (id, embedding, {column}) VALUES (9, '[0.1,0.2]', '{literal}') \
                 USING OPERATION_ID 'err-{column}'"
            ),
        )
    };
    assert_eq!(attempt("nums", "{abc}"), "22P02");
    assert_eq!(attempt("nums", "{123456}"), "22003");
    assert_eq!(attempt("nums", "{1,2,3,4,5}"), "54000");
    assert_eq!(attempt("blobs", "{zz}"), "22P02");
    assert_eq!(attempt("blobs", "{\\x0}"), "22P02");
    assert_eq!(attempt("js", "{\"{not json\"}"), "22P02");
    assert_eq!(attempt("jb", "{\"[1,\"}"), "22P02");
    assert_eq!(attempt("moods", "{ecstatic}"), "22P02");
    assert_eq!(attempt("moods", "{happy,sad,angry,happy,sad}"), "54000");
    // 拒否された書き込みは行を残さない。
    assert!(ids(&core, "alice", "ext", "nums IS NULL").is_empty());
    // WHERE の右辺も同じ分類。
    let err = core
        .execute_sql(
            &ctx("alice"),
            "SELECT id FROM ext WHERE moods = '{ecstatic}' LIMIT 1",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn update_and_tenant_isolation_apply_to_new_element_types() {
    let (path, _g) = setup("ext-update-rls");
    let core = core_at(&path);
    ok(&core, ROW1);
    ok(
        &core,
        "UPDATE ext SET moods = '{sad}' WHERE id = 1 USING OPERATION_ID 'upd-1'",
    );
    assert_eq!(ids(&core, "alice", "ext", "moods = '{sad}'"), vec![1]);
    // 他テナントからは見えない。
    assert!(ids(&core, "bob", "ext", "moods = '{sad}'").is_empty());
    assert!(ids(&core, "bob", "ext", "nums IS NOT NULL").is_empty());
}

#[test]
fn unique_constraint_on_new_array_types_rejects_value_equal_duplicates() {
    let path = unique_db_path("ext-unique");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "CREATE TYPE mood AS ENUM ('happy', 'sad')");
    ok(
        &core,
        "CREATE TABLE u (embedding VECTOR(2) NOT NULL, \
         js JSON[4] UNIQUE, nums NUMERIC(5,2)[4] UNIQUE, moods mood[4] UNIQUE, b BYTEA[4] UNIQUE)",
    );
    let insert = |id: u64, column: &str, literal: &str| {
        run(
            &core,
            "alice",
            &format!(
                "INSERT INTO u (id, embedding, {column}) VALUES ({id}, '[0.1,0.2]', '{literal}') \
                 USING OPERATION_ID 'u-{id}'"
            ),
        )
    };
    insert(1, "js", "{\"{\\\"a\\\":1}\"}").expect("first json");
    // 空白・数値表記が違うだけの JSON は値として重複する。
    let dup = insert(2, "js", "{\"{ \\\"a\\\" : 1.0 }\"}").unwrap_err();
    assert_eq!(dup.wire_code(), "23505");
    insert(3, "js", "{\"{\\\"a\\\":2}\"}").expect("different value");
    insert(4, "nums", "{1.5}").expect("first numeric");
    assert_eq!(
        insert(5, "nums", "{1.50}").unwrap_err().wire_code(),
        "23505"
    );
    insert(6, "moods", "{happy,NULL}").expect("first enum");
    assert_eq!(
        insert(7, "moods", "{happy,NULL}").unwrap_err().wire_code(),
        "23505"
    );
    insert(8, "moods", "{NULL,happy}").expect("NULL position differs");
    insert(9, "b", "{\"\\\\x01\"}").expect("first bytea");
    assert_eq!(
        insert(10, "b", "{\"\\\\x01\"}").unwrap_err().wire_code(),
        "23505"
    );
    // 別テナントの同値は違反にならない。
    run(
        &core,
        "bob",
        "INSERT INTO u (id, embedding, js) VALUES (11, '[0.1,0.2]', '{\"{\\\"a\\\":1}\"}') \
         USING OPERATION_ID 'u-bob'",
    )
    .expect("other tenant may reuse the value");
}

#[test]
fn drop_type_sees_enum_array_dependency_and_alter_type_extends_vocabulary() {
    let path = unique_db_path("ext-enum-deps");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        ok(&core, "CREATE TYPE mood AS ENUM ('happy', 'sad')");
        // `mood[]` 列だけが依存として残る（スカラー ENUM 列は無い）。
        ok(
            &core,
            "CREATE TABLE e (embedding VECTOR(2) NOT NULL, moods mood[4])",
        );
        assert_eq!(code(&core, "DROP TYPE mood"), "2BP01");
        assert_eq!(
            code(
                &core,
                "INSERT INTO e (id, embedding, moods) VALUES (1, '[0.1,0.2]', '{excited}') \
                 USING OPERATION_ID 'e-1'"
            ),
            "22P02"
        );
    }
    // ALTER TYPE ADD VALUE（Storage API。`enum_column.rs` と同じく再オープンして DDL を行う）。
    {
        let storage = Storage::open(&path).expect("reopen for DDL");
        storage
            .alter_enum_type_add_value("mood", "excited".to_string())
            .expect("ADD VALUE");
    }
    let core = core_at(&path);
    // 新ラベルが `mood[]` 列へ書ける（テーブル世代が進み語彙が再解決される）。
    ok(
        &core,
        "INSERT INTO e (id, embedding, moods) VALUES (1, '[0.1,0.2]', '{excited,happy}') \
         USING OPERATION_ID 'e-1b'",
    );
    assert_eq!(
        ids(&core, "alice", "e", "moods = '{excited,happy}'"),
        vec![1]
    );

    // 依存列を DROP すれば（墓標は TEXT[] へ正規化され依存に数えない）DROP TYPE が通る。
    ok(&core, "ALTER TABLE e DROP COLUMN moods");
    ok(&core, "DROP TYPE mood");
}

#[test]
fn add_column_accepts_new_element_types_and_alter_column_type_still_rejects_arrays() {
    let path = unique_db_path("ext-add-column");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "CREATE TYPE mood AS ENUM ('happy')");
    ok(&core, "CREATE TABLE a (embedding VECTOR(2) NOT NULL)");
    for sql in [
        "ALTER TABLE a ADD COLUMN n NUMERIC(5,2)[]",
        "ALTER TABLE a ADD COLUMN b BYTEA[3]",
        "ALTER TABLE a ADD COLUMN j JSON[]",
        "ALTER TABLE a ADD COLUMN jb JSONB[]",
        "ALTER TABLE a ADD COLUMN m mood[2]",
    ] {
        ok(&core, sql);
    }
    assert_eq!(code(&core, "ALTER TABLE a ADD COLUMN x no_such[]"), "42601");
    assert_eq!(
        code(&core, "ALTER TABLE a ADD COLUMN x NUMERIC(5,9)[]"),
        "42601"
    );
    // 配列 DEFAULT は受理される（Issue #1374）。要素不正は INSERT と同じ 22P02。
    ok(
        &core,
        "ALTER TABLE a ADD COLUMN xd BYTEA[] DEFAULT '{\"\\\\x01\"}'",
    );
    assert_eq!(
        code(&core, "ALTER TABLE a ADD COLUMN xe BYTEA[] DEFAULT '{zz}'"),
        "22P02"
    );
    // `NUMERIC(p,s)[]` の精度拡大を含め、配列の型変更は 42804 のまま。
    assert_eq!(
        code(&core, "ALTER TABLE a ALTER COLUMN n TYPE NUMERIC(10,2)[]"),
        "42804"
    );
    // 追加列へ書いて読める（既存行の補完は NULL）。
    ok(
        &core,
        "INSERT INTO a (id, embedding, n, b, m) VALUES \
         (1, '[0.1,0.2]', '{1.25}', '{\"\\\\x00\"}', '{happy}') USING OPERATION_ID 'a-1'",
    );
    assert_eq!(ids(&core, "alice", "a", "n = '{1.25}'"), vec![1]);
}

#[test]
fn ddl_permission_gate_precedes_enum_array_type_resolution() {
    let path = unique_db_path("ext-perm");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "CREATE TYPE mood AS ENUM ('a')");
    let mut denied = SessionState::default();
    // 型が存在しても存在しなくても同じ 42501（存在オラクルにならない）。
    for sql in [
        "CREATE TABLE p (m mood[])",
        "CREATE TABLE p (m no_such_type[])",
    ] {
        let err = core
            .execute_sql_in_session(&ctx("alice"), &mut denied, sql)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42501", "{sql}");
    }
    // 権限があっても未登録の ENUM 要素型は 42601。
    assert_eq!(code(&core, "CREATE TABLE p (m no_such_type[])"), "42601");
}

/// 集合演算（`UNION`）の行キーも値等価の正準ペイロードを使う（Issue #1357 D4）。
/// 空白・数値表記だけが違う JSON 配列は 1 行に畳まれ、`NUMERIC` 配列でも
/// 値が違えば別行のまま残る。
#[test]
fn union_dedups_by_value_equality_for_json_and_numeric_arrays() {
    let path = unique_db_path("ext-union");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "CREATE TABLE s1 (embedding VECTOR(2) NOT NULL, js JSON[2], nums NUMERIC(5,2)[2])",
    );
    ok(
        &core,
        "CREATE TABLE s2 (embedding VECTOR(2) NOT NULL, js JSON[2], nums NUMERIC(5,2)[2])",
    );
    ok(
        &core,
        "INSERT INTO s1 (id, embedding, js, nums) VALUES \
         (1, '[0.1,0.2]', '{\"{\\\"a\\\":1}\"}', '{1.00}') USING OPERATION_ID 's1-1'",
    );
    ok(
        &core,
        "INSERT INTO s2 (id, embedding, js, nums) VALUES \
         (1, '[0.1,0.2]', '{\"{ \\\"a\\\" : 1.0 }\"}', '{1.0}') USING OPERATION_ID 's2-1'",
    );
    ok(
        &core,
        "INSERT INTO s2 (id, embedding, js, nums) VALUES \
         (2, '[0.1,0.2]', '{\"{\\\"a\\\":2}\"}', '{1.01}') USING OPERATION_ID 's2-2'",
    );
    let count = |sql: &str| {
        core.execute_sql(&ctx("alice"), sql)
            .unwrap_or_else(|e| panic!("{sql} failed: {e:?}"))
            .rows
            .len()
    };
    // 値として等価な 1 行は畳まれ、値の違う 1 行が残る。
    assert_eq!(
        count("SELECT js FROM s1 UNION SELECT js FROM s2 LIMIT 10"),
        2
    );
    assert_eq!(
        count("SELECT js FROM s1 UNION ALL SELECT js FROM s2 LIMIT 10"),
        3
    );
    // 表記が違っても 1.00 = 1.0（同じ位取りへ正規化）は同値、1.01 は別値。
    assert_eq!(
        count("SELECT nums FROM s1 UNION SELECT nums FROM s2 LIMIT 10"),
        2
    );
}
