//! `VARCHAR(n)`／`CHAR(n)` の宣言（`ColumnType::Varchar(Some(n))`・`ColumnType::Bpchar(n)`）の
//! 結合テスト。ポインタ: TABLE-31・Issue #1563・#1564（長さ超過 22001・空白の切り詰め。兄弟: #1565 wire typmod・
//! #1566 CHAR の空白詰めと比較。#1567 の ALTER TYPE／FK／UNIQUE もここで固定する）。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 長さと種別がカタログ param として
//! 永続化され再起動後に復元されること、(2) n=0・上限超過・u32 あふれが fail-closed に拒否され副作用が
//! 残らないこと、(3) n 超過の書き込みが永続化されないこと、(4) CHAR(n) の未確定意味論が拒否側に
//! 倒れていること、を固定する。

use engine::catalog::ColumnType;
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_of(tenant: &str) -> PolicyContext {
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

fn exec_as(tenant: &str, core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_of(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec_as("alice", core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec_as("alice", core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
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

fn table_exists(path: &std::path::Path, table: &str) -> bool {
    Storage::open(path)
        .expect("reopen")
        .get_table_schema(table)
        .is_ok()
}

fn select_count(core: &EngineCore, sql: &str) -> usize {
    core.execute_sql(&ctx_of("alice"), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .len()
}

fn texts(core: &EngineCore, sql: &str) -> Vec<Option<String>> {
    core.execute_sql(&ctx_of("alice"), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(engine::sql::exec::Cell::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn ids(core: &EngineCore, sql: &str) -> Vec<u64> {
    core.execute_sql(&ctx_of("alice"), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

/// 受け入れ条件 1: 再起動後も長さと種別（varying か fixed か）が復元される。
#[test]
fn length_and_kind_are_restored_after_reopen() {
    let (path, _g, core) = open("cl-persist");
    ok(
        &core,
        "CREATE TABLE t (a VARCHAR(10), b CHARACTER VARYING(3), c CHAR(5), d CHARACTER(1), \
         e VARCHAR, f TEXT)",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN x VARCHAR(4)");
    ok(&core, "ALTER TABLE t ADD COLUMN y CHAR(7)");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Varchar(Some(10)));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(Some(3)));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bpchar(5));
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Bpchar(1));
    assert_eq!(column_type(&path, "t", "e"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "f"), ColumnType::Text);
    assert_eq!(column_type(&path, "t", "x"), ColumnType::Varchar(Some(4)));
    assert_eq!(column_type(&path, "t", "y"), ColumnType::Bpchar(7));
}

/// 受け入れ条件 2: n=0・上限超過・u32 あふれは `22023`、形式不正は `42601`、長さなしの
/// `CHAR` と配列形は `0A000`。拒否後にテーブルは作られない。
#[test]
fn invalid_lengths_are_rejected_fail_closed_without_side_effects() {
    let (path, _g, core) = open("cl-reject");
    for sql in [
        "CREATE TABLE t (a VARCHAR(0))",
        "CREATE TABLE t (a CHAR(0))",
        "CREATE TABLE t (a CHARACTER VARYING(0))",
        "CREATE TABLE t (a VARCHAR(10485761))",
        "CREATE TABLE t (a CHAR(4294967296))",
        "CREATE TABLE t (a VARCHAR(99999999999999999999))",
    ] {
        assert_eq!(code(&core, sql), "22023", "{sql}");
    }
    for sql in [
        "CREATE TABLE t (a VARCHAR())",
        "CREATE TABLE t (a VARCHAR(01))",
        "CREATE TABLE t (a CHAR(1.5))",
        "CREATE TABLE t (a CHAR(-1))",
        "CREATE TABLE t (a VARCHAR(3)",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    for sql in [
        "CREATE TABLE t (a CHAR)",
        "CREATE TABLE t (a CHARACTER)",
        "CREATE TABLE t (a VARCHAR(3)[])",
        "CREATE TABLE t (a CHAR(3)[])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    ok(&core, "CREATE TABLE base (a INT)");
    assert_eq!(
        code(&core, "ALTER TABLE base ADD COLUMN b VARCHAR(0)"),
        "22023"
    );
    assert_eq!(
        code(&core, "ALTER TABLE base ADD COLUMN b CHAR(10485761)"),
        "22023"
    );
    drop(core);
    assert!(
        !table_exists(&path, "t"),
        "rejected DDL must leave no table"
    );
    assert_eq!(column_type(&path, "base", "a"), ColumnType::Integer);
    assert!(Storage::open(&path)
        .expect("reopen")
        .get_table_schema("base")
        .expect("schema")
        .columns
        .iter()
        .all(|c| c.name != "b"));
}

/// 上限ちょうど（10485760）は受理する。
#[test]
fn maximum_length_is_accepted() {
    let (path, _g, core) = open("cl-max");
    ok(
        &core,
        "CREATE TABLE t (a VARCHAR(10485760), b CHAR(10485760))",
    );
    drop(core);
    assert_eq!(
        column_type(&path, "t", "a"),
        ColumnType::Varchar(Some(10_485_760))
    );
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Bpchar(10_485_760));
}

/// n 超過の書き込みは束縛段で `22001` になり永続化されない。長さは文字数で数える
/// （Issue #1564。codec は最後の砦）。
#[test]
fn over_length_writes_are_rejected_and_not_persisted() {
    let (_p, _g, core) = open("cl-overflow");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, 'abc') USING OPERATION_ID 'o1'",
    );
    // 3 文字（9 バイト）は受理される。
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '日本語') USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (3, '') USING OPERATION_ID 'o3'",
    );
    let c = code(
        &core,
        "INSERT INTO t (id, v) VALUES (4, 'abcd') USING OPERATION_ID 'o4'",
    );
    assert_eq!(c, "22001");
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 3);
    // UPDATE の SET でも超過は 22001 で拒否され、既存行は不変。
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = 'abcd' WHERE id = 1 USING OPERATION_ID 'o5'"
        ),
        "22001"
    );
    assert_eq!(
        select_count(&core, "SELECT id FROM t WHERE v = 'abc' LIMIT 10"),
        1
    );
}

/// 一致行 0 件の UPDATE でも n 超過の SET 値は対象行探索前に拒否され、
/// `operation_id` を消費しない（同じ ID で有効な値を再送できる）。
#[test]
fn over_length_update_set_is_rejected_even_with_zero_matching_rows() {
    let (_p, _g, core) = open("cl-update-zero");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, 'abc') USING OPERATION_ID 'o1'",
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = 'abcd' WHERE id = 999 USING OPERATION_ID 'o2'"
        ),
        "22001"
    );
    // 拒否で operation_id が消費されていなければ、同 ID で有効な更新が成功する。
    ok(
        &core,
        "UPDATE t SET v = 'xyz' WHERE id = 1 USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        select_count(&core, "SELECT id FROM t WHERE v = 'xyz' LIMIT 10"),
        1
    );
}

/// `VARCHAR(n)` は TEXT と同じ述語・索引・PK 経路で扱える（値表現を共有するため）。
#[test]
fn bounded_varchar_behaves_like_text_for_predicates_and_keys() {
    let (_p, _g, core) = open("cl-varchar-text");
    ok(
        &core,
        "CREATE TABLE t (k VARCHAR(8) PRIMARY KEY, v VARCHAR(4))",
    );
    ok(
        &core,
        "INSERT INTO t (id, k, v) VALUES (1, 'apple', 'red') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, k, v) VALUES (2, 'banana', 'yel') USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        select_count(&core, "SELECT k FROM t WHERE k = 'apple' LIMIT 10"),
        1
    );
    assert_eq!(
        select_count(&core, "SELECT k FROM t WHERE k LIKE 'ban%' LIMIT 10"),
        1
    );
    assert_eq!(
        select_count(&core, "SELECT k FROM t ORDER BY k LIMIT 10"),
        2
    );
    ok(&core, "CREATE INDEX t_v ON t (v)");
}

/// `CHAR VARYING`／`CHAR VARYING(n)` は `CHARACTER VARYING` と同じ VARCHAR の綴り。
/// `VARYING` を伴わない bare `CHAR` は従来どおり `0A000`。
#[test]
fn char_varying_is_accepted_as_varchar_and_bare_char_stays_rejected() {
    let (path, _g, core) = open("cl-char-varying");
    ok(
        &core,
        "CREATE TABLE t (a CHAR VARYING(6), b char varying, c CHAR(2))",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Varchar(Some(6)));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bpchar(2));

    let (_p2, _g2, core) = open("cl-char-bare");
    assert_eq!(code(&core, "CREATE TABLE u (a CHAR)"), "0A000");
}

/// `CHAR(n)` の書き込みは n 文字へ半角空白で詰めて保存され、SELECT は詰めた値を返す
/// （TABLE-31・Issue #1566）。マルチバイトは文字数で詰める。
#[test]
fn char_values_are_blank_padded_on_write() {
    let (_p, _g, core) = open("cl-bpchar-pad");
    ok(&core, "CREATE TABLE t (c CHAR(3), n INT)");
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (1, 'a', 1) USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (2, '日', 2) USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (3, 'abc', 3) USING OPERATION_ID 'o3'",
    );
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (4, NULL, 4) USING OPERATION_ID 'o4'",
    );
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (5, 5) USING OPERATION_ID 'o5'",
    );
    let got = texts(&core, "SELECT c FROM t ORDER BY id LIMIT 10");
    assert_eq!(
        got,
        vec![
            Some("a  ".to_string()),
            Some("日  ".to_string()),
            Some("abc".to_string()),
            None,
            None
        ]
    );
    // UPDATE（リテラル・式）・UPSERT も詰めて保存する。
    ok(
        &core,
        "UPDATE t SET c = 'z' WHERE id = 1 USING OPERATION_ID 'o6'",
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("z  ".to_string())]
    );
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (3, 'q', 9) ON CONFLICT (id) DO UPDATE SET c = 'y' \
         USING OPERATION_ID 'o7'",
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 3 LIMIT 10"),
        vec![Some("y  ".to_string())]
    );
    // n 超過は従来どおり 22001。超過部分が空白のみなら切り詰めてから詰める。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, c, n) VALUES (7, 'abcd', 7) USING OPERATION_ID 'o8'"
        ),
        "22001"
    );
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (8, 'ab    ', 8) USING OPERATION_ID 'o9'",
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 8 LIMIT 10"),
        vec![Some("ab ".to_string())]
    );
}

/// 受け入れ条件 1・2: 末尾の半角空白は比較で意味を持たず、索引の有無で結果が一致する。
#[test]
fn char_comparison_ignores_trailing_spaces_with_and_without_index() {
    for indexed in [false, true] {
        let (_p, _g, core) = open(if indexed {
            "cl-bpchar-cmp-ix"
        } else {
            "cl-bpchar-cmp"
        });
        ok(&core, "CREATE TABLE t (c CHAR(3), n INT)");
        if indexed {
            ok(&core, "CREATE INDEX t_c ON t (c)");
        }
        for (i, v) in ["a", "ab", "abc", "b"].iter().enumerate() {
            ok(
                &core,
                &format!(
                    "INSERT INTO t (id, c, n) VALUES ({}, '{v}', {}) USING OPERATION_ID 'o{i}'",
                    i + 1,
                    i + 1
                ),
            );
        }
        for lit in ["a", "a  ", "a "] {
            assert_eq!(
                ids(
                    &core,
                    &format!("SELECT id FROM t WHERE c = '{lit}' LIMIT 10")
                ),
                vec![1],
                "indexed={indexed} {lit:?}"
            );
        }
        assert_eq!(
            ids(&core, "SELECT id FROM t WHERE c = 'abcd' LIMIT 10"),
            Vec::<u64>::new()
        );
        assert_eq!(
            ids(
                &core,
                "SELECT id FROM t WHERE c IN ('a ', 'b') ORDER BY id LIMIT 10"
            ),
            vec![1, 4]
        );
        assert_eq!(
            ids(
                &core,
                "SELECT id FROM t WHERE c NOT IN ('a ', 'b') ORDER BY id LIMIT 10"
            ),
            vec![2, 3]
        );
        assert_eq!(
            ids(
                &core,
                "SELECT id FROM t WHERE c <> 'a' ORDER BY id LIMIT 10"
            ),
            vec![2, 3, 4]
        );
        assert_eq!(
            ids(
                &core,
                "SELECT id FROM t WHERE c = 'a' OR c = 'b ' ORDER BY id LIMIT 10"
            ),
            vec![1, 4]
        );
        // 範囲比較（式レーン）。
        assert_eq!(
            ids(&core, "SELECT id FROM t WHERE c < 'b' ORDER BY id LIMIT 10"),
            vec![1, 2, 3]
        );
        assert_eq!(
            ids(
                &core,
                "SELECT id FROM t WHERE c >= 'ab  ' ORDER BY id LIMIT 10"
            ),
            vec![2, 3, 4]
        );
        // UPDATE・DELETE の後も一致する。
        ok(
            &core,
            "UPDATE t SET c = 'x' WHERE id = 1 USING OPERATION_ID 'u1'",
        );
        assert_eq!(
            ids(&core, "SELECT id FROM t WHERE c = 'x  ' LIMIT 10"),
            vec![1]
        );
        assert_eq!(
            ids(&core, "SELECT id FROM t WHERE c = 'a' LIMIT 10"),
            Vec::<u64>::new()
        );
        ok(&core, "DELETE FROM t WHERE id = 2 USING OPERATION_ID 'd1'");
        assert_eq!(
            ids(&core, "SELECT id FROM t WHERE c = 'ab' LIMIT 10"),
            Vec::<u64>::new()
        );
    }
}

/// 式レーン: CHAR 列は TEXT へ変換した値（末尾空白なし）として振る舞う。
#[test]
fn char_in_expressions_behaves_as_trimmed_text() {
    let (_p, _g, core) = open("cl-bpchar-expr");
    ok(&core, "CREATE TABLE t (c CHAR(5), v TEXT)");
    ok(
        &core,
        "INSERT INTO t (id, c, v) VALUES (1, 'ab', 'ab') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, c, v) VALUES (2, 'ab', 'ab ') USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE length(c) = 2 ORDER BY id LIMIT 10"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids(&core, "SELECT id FROM t WHERE c = v ORDER BY id LIMIT 10"),
        vec![1]
    );
    assert_eq!(
        texts(&core, "SELECT concat(c, 'x') FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("abx".to_string())]
    );
}

/// GROUP BY・DISTINCT・COUNT(DISTINCT)・ORDER BY は rtrim 比較で、出力は詰めた値。
#[test]
fn char_group_by_distinct_and_order_by_use_rtrim_semantics() {
    let (_p, _g, core) = open("cl-bpchar-group");
    ok(&core, "CREATE TABLE t (c CHAR(3))");
    for (i, v) in ["a", "a ", "a  ", "b", "a\t"].iter().enumerate() {
        ok(
            &core,
            &format!(
                "INSERT INTO t (id, c) VALUES ({}, '{v}') USING OPERATION_ID 'o{i}'",
                i + 1
            ),
        );
    }
    let r = core
        .execute_sql(
            &ctx_of("alice"),
            "SELECT c, count(*) FROM t GROUP BY c ORDER BY c LIMIT 10",
        )
        .expect("group by");
    assert_eq!(r.rows.len(), 3);
    let keys: Vec<String> = r
        .rows
        .iter()
        .filter_map(|row| match row.cells.first() {
            Some(engine::sql::exec::Cell::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    // rtrim 比較順: 'a' < 'a\t' < 'b'。出力は n 文字へ詰めた値。
    assert_eq!(
        keys,
        vec!["a  ".to_string(), "a\t ".to_string(), "b  ".to_string()]
    );
    assert_eq!(select_count(&core, "SELECT DISTINCT c FROM t LIMIT 10"), 3);
    let r = core
        .execute_sql(&ctx_of("alice"), "SELECT count(DISTINCT c) FROM t")
        .expect("count distinct");
    assert_eq!(
        r.rows.first().and_then(|row| row.cells.first()),
        Some(&engine::sql::exec::Cell::Integer(3))
    );
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY c, id LIMIT 10"),
        vec![1, 2, 3, 5, 4]
    );
}

/// 今回も拒否のまま残す形（PK／UNIQUE／LIKE／JOIN／MIN・MAX 等）は fail-closed。
#[test]
fn char_unsupported_forms_stay_fail_closed() {
    let (_p, _g, core) = open("cl-bpchar-closed");
    ok(&core, "CREATE TABLE t (c CHAR(3), n INT)");
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (1, 'ab', 1) USING OPERATION_ID 'o1'",
    );
    for sql in [
        "SELECT id FROM t WHERE c LIKE 'a%' LIMIT 10",
        "SELECT max(c) FROM t",
        "SELECT min(c) FROM t",
    ] {
        assert!(
            exec_as("alice", &core, sql).is_err(),
            "{sql} must be rejected while it is out of scope"
        );
    }
}

/// DEFAULT は n 文字へ詰めた形で保持され、ADD COLUMN の補完行も詰めた値になる。
#[test]
fn char_default_is_padded() {
    let (_p, _g, core) = open("cl-bpchar-default");
    ok(&core, "CREATE TABLE t (n INT, c CHAR(3) DEFAULT 'a')");
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (1, 1) USING OPERATION_ID 'o1'",
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("a  ".to_string())]
    );
    ok(&core, "ALTER TABLE t ADD COLUMN d CHAR(2) DEFAULT 'x'");
    assert_eq!(
        texts(&core, "SELECT d FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("x ".to_string())]
    );
    assert_eq!(
        code(&core, "CREATE TABLE u (c CHAR(3) DEFAULT 'abcd')"),
        "22001"
    );
}

/// 詰め後の長さがエンジンの TEXT 上限を超える CHAR(n) は 54000 で、表は変わらない。
#[test]
fn char_padding_beyond_engine_text_limit_is_rejected() {
    let (_p, _g, core) = open("cl-bpchar-limit");
    ok(&core, "CREATE TABLE t (c CHAR(10485760))");
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, c) VALUES (1, 'a') USING OPERATION_ID 'o1'"
        ),
        "54000"
    );
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 0);
}

/// `ALTER COLUMN TYPE` の文字列系の変換行列（TABLE-19・TABLE-31・Issue #1567）。
/// 拡大はそのまま、`CHAR(n)` は空白詰め・rtrim の変換、同一型への変更は従来どおり `42804`。
#[test]
fn alter_column_type_char_family_matrix() {
    let (path, _g, core) = open("cl-alter");
    ok(
        &core,
        "CREATE TABLE t (a TEXT, b VARCHAR(5), c CHAR(5), d VARCHAR(3), e VARCHAR)",
    );
    ok(
        &core,
        "INSERT INTO t (id, a, b, c, d) VALUES (1, 'ab', 'abc', 'ab', 'xy') \
         USING OPERATION_ID 'o1'",
    );
    let sel = |col: &str| texts(&core, &format!("SELECT {col} FROM t WHERE id = 1 LIMIT 10"));
    // 拡大（カタログのみ）。
    ok(&core, "ALTER TABLE t ALTER COLUMN d TYPE VARCHAR(10)");
    ok(&core, "ALTER TABLE t ALTER COLUMN d TYPE TEXT");
    assert_eq!(sel("d"), vec![Some("xy".to_string())]);
    // TEXT -> CHAR(3): 空白詰め。
    ok(&core, "ALTER TABLE t ALTER COLUMN a TYPE CHAR(3)");
    assert_eq!(sel("a"), vec![Some("ab ".to_string())]);
    // CHAR(5) -> TEXT: rtrim。
    ok(&core, "ALTER TABLE t ALTER COLUMN c TYPE TEXT");
    assert_eq!(sel("c"), vec![Some("ab".to_string())]);
    // VARCHAR(5) -> CHAR(8) -> VARCHAR(4)。
    ok(&core, "ALTER TABLE t ALTER COLUMN b TYPE CHAR(8)");
    assert_eq!(sel("b"), vec![Some("abc     ".to_string())]);
    ok(&core, "ALTER TABLE t ALTER COLUMN b TYPE VARCHAR(4)");
    assert_eq!(sel("b"), vec![Some("abc".to_string())]);
    // CHAR(3) -> CHAR(5)。
    ok(&core, "ALTER TABLE t ALTER COLUMN a TYPE CHAR(5)");
    assert_eq!(sel("a"), vec![Some("ab   ".to_string())]);
    // 変換後も比較は rtrim 意味論で、索引の有無に関わらず一致する。
    assert_eq!(
        select_count(&core, "SELECT id FROM t WHERE a = 'ab' LIMIT 10"),
        1
    );
    ok(&core, "CREATE INDEX t_a ON t (a)");
    assert_eq!(
        select_count(&core, "SELECT id FROM t WHERE a = 'ab ' LIMIT 10"),
        1
    );
    // 同一型への変更は従来どおり拒否する。
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN a TYPE CHAR(5)"),
        "42804"
    );
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN b TYPE VARCHAR(4)"),
        "42804"
    );
    ok(&core, "ALTER TABLE t ALTER COLUMN e TYPE TEXT");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Bpchar(5));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(Some(4)));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Text);
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Text);
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    assert_eq!(
        texts(&core, "SELECT a FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("ab   ".to_string())]
    );
}

/// 縮小で超過行が 1 行でもあれば `22001` で失敗し、型・行・件数は一切変わらない（受入条件）。
/// 超過部分が半角空白のみの値は切り詰めて受理する。
#[test]
fn alter_column_type_shrink_with_excess_rows_is_rejected_atomically() {
    let (path, _g, core) = open("cl-alter-shrink");
    ok(&core, "CREATE TABLE t (a TEXT, b VARCHAR(10), c CHAR(5))");
    ok(
        &core,
        "INSERT INTO t (id, a, b, c) VALUES (1, 'ab', 'ab', 'ab') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, a, b, c) VALUES (2, 'abcdef', 'abcdef', 'abc') USING OPERATION_ID 'o2'",
    );
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TYPE VARCHAR(3)",
        "ALTER TABLE t ALTER COLUMN a TYPE CHAR(3)",
        "ALTER TABLE t ALTER COLUMN b TYPE VARCHAR(3)",
        "ALTER TABLE t ALTER COLUMN b TYPE CHAR(3)",
        "ALTER TABLE t ALTER COLUMN c TYPE CHAR(2)",
        "ALTER TABLE t ALTER COLUMN c TYPE VARCHAR(2)",
    ] {
        assert_eq!(code(&core, sql), "22001", "{sql}");
    }
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Text);
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(Some(10)));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bpchar(5));
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    assert_eq!(
        texts(&core, "SELECT a FROM t LIMIT 10"),
        vec![Some("ab".to_string()), Some("abcdef".to_string())]
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t LIMIT 10"),
        vec![Some("ab   ".to_string()), Some("abc  ".to_string())]
    );
    // 超過が空白だけなら切り詰めて受理する。
    ok(&core, "CREATE TABLE s (a TEXT)");
    ok(
        &core,
        "INSERT INTO s (id, a) VALUES (1, 'ab     ') USING OPERATION_ID 'o3'",
    );
    ok(&core, "ALTER TABLE s ALTER COLUMN a TYPE VARCHAR(3)");
    assert_eq!(
        texts(&core, "SELECT a FROM s LIMIT 10"),
        vec![Some("ab ".to_string())]
    );
}

/// 列 DEFAULT も新しい型へ変換され、収まらなければ `22001`（行が無くても検査する）。
#[test]
fn alter_column_type_converts_column_default() {
    let (_p, _g, core) = open("cl-alter-default");
    ok(&core, "CREATE TABLE t (n INT, c TEXT DEFAULT 'a')");
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (1, 1) USING OPERATION_ID 'o1'",
    );
    ok(&core, "ALTER TABLE t ALTER COLUMN c TYPE CHAR(3)");
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("a  ".to_string())]
    );
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (2, 2) USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 2 LIMIT 10"),
        vec![Some("a  ".to_string())]
    );
    // ADD COLUMN の補完行（行バイトを持たない）も新しい型の値になる。
    ok(&core, "ALTER TABLE t ADD COLUMN d TEXT DEFAULT 'x'");
    ok(&core, "ALTER TABLE t ALTER COLUMN d TYPE CHAR(2)");
    assert_eq!(
        texts(&core, "SELECT d FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("x ".to_string())]
    );
    ok(&core, "ALTER TABLE t ALTER COLUMN c TYPE TEXT");
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("a".to_string())]
    );
    ok(&core, "CREATE TABLE u (c TEXT DEFAULT 'abcd')");
    assert_eq!(
        code(&core, "ALTER TABLE u ALTER COLUMN c TYPE VARCHAR(3)"),
        "22001"
    );
}

/// 変換で値が同一になる行（`'ab'` と `'ab '` が `CHAR(3)` で同値）がある UNIQUE 列の変換は
/// `23505` で表は不変。別テナントの同値は重複としない。
#[test]
fn alter_column_type_rechecks_unique_after_conversion() {
    let (path, _g, core) = open("cl-alter-uniq");
    ok(&core, "CREATE TABLE t (u TEXT UNIQUE)");
    ok(
        &core,
        "INSERT INTO t (id, u) VALUES (1, 'ab') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, u) VALUES (2, 'ab ') USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN u TYPE CHAR(3)"),
        "23505"
    );
    assert_eq!(
        texts(&core, "SELECT u FROM t LIMIT 10"),
        vec![Some("ab".to_string()), Some("ab ".to_string())]
    );
    // テナントをまたぐ同値は重複ではない。
    ok(&core, "CREATE TABLE t2 (u TEXT UNIQUE)");
    ok(
        &core,
        "INSERT INTO t2 (id, u) VALUES (1, 'ab') USING OPERATION_ID 'o3'",
    );
    exec_as(
        "bob",
        &core,
        "INSERT INTO t2 (id, u) VALUES (1, 'ab ') USING OPERATION_ID 'o4'",
    )
    .expect("bob insert");
    ok(&core, "ALTER TABLE t2 ALTER COLUMN u TYPE CHAR(3)");
    drop(core);
    assert_eq!(column_type(&path, "t", "u"), ColumnType::Text);
    assert_eq!(column_type(&path, "t2", "u"), ColumnType::Bpchar(3));
}

/// `CHAR(n)` の PRIMARY KEY・UNIQUE は末尾空白違いを同値として一意性を検査する。
#[test]
fn char_primary_key_and_unique_use_padded_identity() {
    let (_p, _g, core) = open("cl-bpchar-key");
    ok(&core, "CREATE TABLE p (c CHAR(3) PRIMARY KEY)");
    ok(
        &core,
        "INSERT INTO p (id, c) VALUES (1, 'a') USING OPERATION_ID 'o1'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO p (id, c) VALUES (2, 'a ') USING OPERATION_ID 'o2'"
        ),
        "23505"
    );
    ok(
        &core,
        "INSERT INTO p (id, c) VALUES (2, 'b') USING OPERATION_ID 'o3'",
    );
    assert_eq!(select_count(&core, "SELECT id FROM p LIMIT 10"), 2);

    ok(&core, "CREATE TABLE u (c CHAR(3) UNIQUE, n INT)");
    ok(
        &core,
        "INSERT INTO u (id, c, n) VALUES (1, 'a', 1) USING OPERATION_ID 'o5'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, c, n) VALUES (2, 'a', 2) USING OPERATION_ID 'o6'"
        ),
        "23505"
    );
    // UPSERT の衝突検出も末尾空白違いを同値として扱う（DO NOTHING で行は増えない）。
    ok(
        &core,
        "INSERT INTO u (id, c, n) VALUES (9, 'a  ', 9) ON CONFLICT (c) DO NOTHING \
         USING OPERATION_ID 'o5b'",
    );
    assert_eq!(select_count(&core, "SELECT id FROM u LIMIT 10"), 1);
    // NULL は区別される（NULLS DISTINCT）。
    ok(
        &core,
        "INSERT INTO u (id, n) VALUES (3, 3) USING OPERATION_ID 'o7'",
    );
    ok(
        &core,
        "INSERT INTO u (id, n) VALUES (4, 4) USING OPERATION_ID 'o8'",
    );
    // 末尾空白だけ違う値を持つ TEXT 列への ADD UNIQUE は成立するが、CHAR 列では重複になる。
    ok(&core, "CREATE TABLE w (c CHAR(3))");
    ok(
        &core,
        "INSERT INTO w (id, c) VALUES (1, 'a') USING OPERATION_ID 'o9'",
    );
    ok(
        &core,
        "INSERT INTO w (id, c) VALUES (2, 'a ') USING OPERATION_ID 'o10'",
    );
    assert!(exec_as("alice", &core, "ALTER TABLE w ADD UNIQUE (c)").is_err());
}

/// FK の型照合: `CHAR(n)` の参照元は `CHAR(m)`・`TEXT`・`VARCHAR` の参照先を許し、
/// 逆（`TEXT`／`VARCHAR` の参照元で `CHAR` の参照先）は宣言時に拒否する。
#[test]
fn char_foreign_key_type_matrix_and_integrity() {
    let (path, _g, core) = open("cl-bpchar-fk");
    ok(&core, "CREATE TABLE p (code CHAR(5) PRIMARY KEY)");
    ok(&core, "CREATE TABLE c (ref CHAR(3) REFERENCES p (code))");
    ok(
        &core,
        "INSERT INTO p (id, code) VALUES (1, 'ab') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO c (id, ref) VALUES (1, 'ab') USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "INSERT INTO c (id, ref) VALUES (2, 'ab ') USING OPERATION_ID 'o3'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO c (id, ref) VALUES (3, 'zz') USING OPERATION_ID 'o4'"
        ),
        "23503"
    );
    assert_eq!(
        code(&core, "DELETE FROM p WHERE id = 1 USING OPERATION_ID 'o5'"),
        "23503"
    );
    // TEXT の親: 末尾空白付きの親値は CHAR の子に一致しない。
    ok(&core, "CREATE TABLE tp (code TEXT PRIMARY KEY)");
    ok(&core, "CREATE TABLE tc (ref CHAR(3) REFERENCES tp (code))");
    ok(
        &core,
        "INSERT INTO tp (id, code) VALUES (1, 'ab') USING OPERATION_ID 'o6'",
    );
    ok(
        &core,
        "INSERT INTO tp (id, code) VALUES (2, 'cd ') USING OPERATION_ID 'o7'",
    );
    ok(
        &core,
        "INSERT INTO tc (id, ref) VALUES (1, 'ab') USING OPERATION_ID 'o8'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO tc (id, ref) VALUES (2, 'cd') USING OPERATION_ID 'o9'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            "DELETE FROM tp WHERE id = 1 USING OPERATION_ID 'o10'"
        ),
        "23503"
    );
    ok(
        &core,
        "DELETE FROM tp WHERE id = 2 USING OPERATION_ID 'o11'",
    );
    // TEXT の参照元で CHAR の参照先は宣言時に拒否する。
    assert_eq!(
        code(&core, "CREATE TABLE bad (ref TEXT REFERENCES p (code))"),
        "42830"
    );
    drop(core);
    assert!(!table_exists(&path, "bad"));
}

/// ON DELETE CASCADE と ON UPDATE CASCADE は `CHAR` の子を正しく見つけ、子の長さへ詰め直す。
#[test]
fn char_foreign_key_cascade_repads_child_values() {
    let (_p, _g, core) = open("cl-bpchar-fk-cascade");
    ok(&core, "CREATE TABLE p (code CHAR(5) PRIMARY KEY)");
    ok(
        &core,
        "CREATE TABLE c (ref CHAR(3) REFERENCES p (code) ON DELETE CASCADE ON UPDATE CASCADE)",
    );
    ok(
        &core,
        "INSERT INTO p (id, code) VALUES (1, 'ab') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO c (id, ref) VALUES (1, 'ab') USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "UPDATE p SET code = 'xy' WHERE id = 1 USING OPERATION_ID 'o3'",
    );
    assert_eq!(
        texts(&core, "SELECT ref FROM c LIMIT 10"),
        vec![Some("xy ".to_string())]
    );
    // 子の長さ（3）を超える親の新値は fail-closed（23503）で、表は変わらない。
    assert_eq!(
        code(
            &core,
            "UPDATE p SET code = 'wxyz' WHERE id = 1 USING OPERATION_ID 'o4'"
        ),
        "23503"
    );
    assert_eq!(
        texts(&core, "SELECT code FROM p LIMIT 10"),
        vec![Some("xy   ".to_string())]
    );
    ok(&core, "DELETE FROM p WHERE id = 1 USING OPERATION_ID 'o5'");
    assert_eq!(select_count(&core, "SELECT id FROM c LIMIT 10"), 0);
}

/// ALTER によって FK の型の組が拒否側になる場合（親の列を `TEXT`→`CHAR`）は `42804` で表は不変。
/// 子の列を `TEXT`→`CHAR` に変えて一致関係が崩れる場合は `23503` で表は不変。
#[test]
fn alter_column_type_rechecks_foreign_keys() {
    let (path, _g, core) = open("cl-alter-fk");
    ok(&core, "CREATE TABLE p (code TEXT PRIMARY KEY)");
    ok(&core, "CREATE TABLE c (ref TEXT REFERENCES p (code))");
    ok(
        &core,
        "INSERT INTO p (id, code) VALUES (1, 'ab ') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO c (id, ref) VALUES (1, 'ab ') USING OPERATION_ID 'o2'",
    );
    // 親を CHAR にすると TEXT の子 -> CHAR の親になり拒否される。
    assert_eq!(
        code(&core, "ALTER TABLE p ALTER COLUMN code TYPE CHAR(3)"),
        "42804"
    );
    // 子を CHAR(3) にすると 'ab ' は rtrim で 'ab' となり、親の 'ab ' に一致しなくなる。
    assert_eq!(
        code(&core, "ALTER TABLE c ALTER COLUMN ref TYPE CHAR(3)"),
        "23503"
    );
    drop(core);
    assert_eq!(column_type(&path, "p", "code"), ColumnType::Text);
    assert_eq!(column_type(&path, "c", "ref"), ColumnType::Text);
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    assert_eq!(
        texts(&core, "SELECT ref FROM c LIMIT 10"),
        vec![Some("ab ".to_string())]
    );
    // 一致関係が保たれるなら受理される（子の値が親と rtrim 後に一致）。
    ok(
        &core,
        "INSERT INTO p (id, code) VALUES (2, 'cd') USING OPERATION_ID 'o3'",
    );
    ok(&core, "CREATE TABLE c2 (ref TEXT REFERENCES p (code))");
    ok(
        &core,
        "INSERT INTO c2 (id, ref) VALUES (1, 'cd') USING OPERATION_ID 'o4'",
    );
    ok(&core, "ALTER TABLE c2 ALTER COLUMN ref TYPE CHAR(4)");
    assert_eq!(
        code(&core, "DELETE FROM p WHERE id = 2 USING OPERATION_ID 'o5'"),
        "23503"
    );
}

/// 拒否・失敗のエラー文言は型と n だけの固定文言で、値・テナント名を含まない。
#[test]
fn alter_column_type_errors_do_not_leak_values() {
    let (_p, _g, core) = open("cl-alter-leak");
    ok(&core, "CREATE TABLE t (a TEXT UNIQUE)");
    ok(
        &core,
        "INSERT INTO t (id, a) VALUES (1, 'secret-value') USING OPERATION_ID 'o1'",
    );
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TYPE VARCHAR(3)",
        "ALTER TABLE t ALTER COLUMN a TYPE CHAR(3)",
    ] {
        let e = exec_as("alice", &core, sql).expect_err("must fail");
        assert_eq!(e.wire_code(), "22001");
        let msg = e.to_string();
        assert!(
            !msg.contains("secret-value") && !msg.contains("alice"),
            "{msg}"
        );
    }
}

/// `VARCHAR(n)` の DEFAULT は文字数が n 以下であること。
#[test]
fn varchar_default_longer_than_length_is_rejected() {
    let (path, _g, core) = open("cl-default");
    ok(&core, "CREATE TABLE ok_t (v VARCHAR(3) DEFAULT 'abc')");
    assert_eq!(
        code(&core, "CREATE TABLE bad_t (v VARCHAR(3) DEFAULT 'abcd')"),
        "22001"
    );
    drop(core);
    assert!(table_exists(&path, "ok_t"));
    assert!(!table_exists(&path, "bad_t"));
}

/// テナント境界: 別テナントのセッションでも同じ DDL 結果になり、エラーに他テナントの
/// 情報（テナント名・表の存在）が含まれない。
#[test]
fn errors_do_not_leak_tenant_information() {
    let (_p, _g, core) = open("cl-tenant");
    ok(&core, "CREATE TABLE secret_t (v VARCHAR(3))");
    let e = exec_as("bob", &core, "CREATE TABLE u (a VARCHAR(0))").expect_err("must fail");
    assert_eq!(e.wire_code(), "22023");
    let msg = e.to_string();
    assert!(
        !msg.contains("alice") && !msg.contains("bob") && !msg.contains("secret_t"),
        "{msg}"
    );
}

fn count_where(core: &EngineCore, table: &str, v: &str) -> usize {
    select_count(
        core,
        &format!("SELECT id FROM {table} WHERE v = '{v}' LIMIT 10"),
    )
}

/// 超過部分が半角空白のみなら n 文字へ切り詰めて受理する（INSERT・UPDATE・UPSERT。
/// タブ・全角空白は対象外）。
#[test]
fn trailing_spaces_beyond_length_are_truncated() {
    let (_p, _g, core) = open("cl-trim");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, 'abc   ') USING OPERATION_ID 'o1'",
    );
    assert_eq!(count_where(&core, "t", "abc"), 1);
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, 'x') USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "UPDATE t SET v = 'xyz  ' WHERE id = 2 USING OPERATION_ID 'o3'",
    );
    assert_eq!(count_where(&core, "t", "xyz"), 1);
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (3, 'pqr  ') ON CONFLICT (id) DO UPDATE SET v = 'zzz   ' \
         USING OPERATION_ID 'o4'",
    );
    assert_eq!(count_where(&core, "t", "pqr"), 1);
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (3, 'q') ON CONFLICT (id) DO UPDATE SET v = 'zzz   ' \
         USING OPERATION_ID 'o5'",
    );
    assert_eq!(count_where(&core, "t", "zzz"), 1);
    // 空白以外が超過部分に含まれる・空白以外の空白類は 22001。
    for lit in ["ab c", "abc\t", "abc\u{3000}", "abc  d"] {
        let sql = format!("INSERT INTO t (id, v) VALUES (9, '{lit}') USING OPERATION_ID 'o9'");
        assert_eq!(code(&core, &sql), "22001", "{lit:?}");
    }
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 3);
}

/// SET 式の評価結果・UPSERT の DO UPDATE（リテラル・EXCLUDED）の超過は 22001 で全行不変。
#[test]
fn runtime_evaluated_assignments_are_rejected_with_22001() {
    let (_p, _g, core) = open("cl-runtime");
    ok(&core, "CREATE TABLE t (v VARCHAR(3), w TEXT)");
    ok(
        &core,
        "INSERT INTO t (id, v, w) VALUES (1, 'abc', 'longtext') USING OPERATION_ID 'o1'",
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = upper(w) WHERE id = 1 USING OPERATION_ID 'o2'"
        ),
        "22001"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v, w) VALUES (1, 'a', 'longtext') ON CONFLICT (id) \
             DO UPDATE SET v = EXCLUDED.w USING OPERATION_ID 'o3'"
        ),
        "22001"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v, w) VALUES (1, 'a', 'b') ON CONFLICT (id) \
             DO UPDATE SET v = 'toolong' USING OPERATION_ID 'o4'"
        ),
        "22001"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v, w) VALUES (2, 'toolong', 'b') ON CONFLICT (id) \
             DO UPDATE SET v = 'ok' USING OPERATION_ID 'o5'"
        ),
        "22001"
    );
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 1);
    assert_eq!(count_where(&core, "t", "abc"), 1);
}

/// COPY FROM は 1 行でも超過すると 22001 で全体を拒否（0 行）。空白のみの超過は切り詰める。
#[test]
fn copy_from_over_length_is_rejected_atomically() {
    let (_p, _g, core) = open("cl-copy");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    let run = |sql: &str, payload: &[u8]| {
        let policy = ctx_of("alice");
        let session = SessionState::default();
        let plan = core.begin_copy(&policy, &session, sql)?;
        let mut cs = match plan {
            CopyPlan::From(s) => s,
            CopyPlan::To(..) => panic!("expected COPY FROM"),
        };
        cs.feed(payload)?;
        let batch = cs.finish()?;
        core.commit_copy_in(&policy, batch)
    };
    let err = run(
        "COPY t (id, v) FROM STDIN USING OPERATION_ID 'c1'",
        b"1\tabc\n2\tabcd\n",
    )
    .expect_err("over-length COPY must fail");
    assert_eq!(err.wire_code(), "22001");
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 0);
    run(
        "COPY t (id, v) FROM STDIN USING OPERATION_ID 'c2'",
        b"1\tabc  \n",
    )
    .expect("trailing spaces are truncated");
    assert_eq!(count_where(&core, "t", "abc"), 1);
}

/// DEFAULT: 超過は DDL ごと 22001 で不変、空白のみの超過は切り詰めて保存され補完行に使われる。
#[test]
fn defaults_are_checked_at_definition_time() {
    let (path, _g, core) = open("cl-default22001");
    assert_eq!(
        code(&core, "CREATE TABLE a (v VARCHAR(3) DEFAULT 'abcd')"),
        "22001"
    );
    ok(&core, "CREATE TABLE t (id2 INT)");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ADD COLUMN v VARCHAR(3) DEFAULT 'abcd'"
        ),
        "22001"
    );
    ok(
        &core,
        "CREATE TABLE b (n INT, v VARCHAR(3) DEFAULT 'ab   ')",
    );
    ok(
        &core,
        "INSERT INTO b (id, n) VALUES (1, 1) USING OPERATION_ID 'o1'",
    );
    assert_eq!(count_where(&core, "b", "ab "), 1);
    drop(core);
    assert!(!table_exists(&path, "a"));
}

/// 明示トランザクション内の超過でも 22001 になり、その文の副作用は残らない。
#[test]
fn over_length_in_explicit_transaction_leaves_no_side_effect() {
    let (_p, _g, core) = open("cl-txn");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let ctx = ctx_of("alice");
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &ctx,
            &mut session,
            &mut txn,
            "INSERT INTO t (id, v) VALUES (1, 'abcd') USING OPERATION_ID 'o1'",
        )
        .expect_err("must fail");
    assert_eq!(e.wire_code(), "22001");
    let _ = core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 0);
}

/// 22001 の文言は型と n だけの固定文言で、値を含まない。
#[test]
fn truncation_message_does_not_reflect_values() {
    let (_p, _g, core) = open("cl-msg");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    let e = exec_as(
        "alice",
        &core,
        "INSERT INTO t (id, v) VALUES (1, 'secretvalue') USING OPERATION_ID 'o1'",
    )
    .expect_err("must fail");
    assert_eq!(e.wire_code(), "22001");
    let msg = e.to_string();
    assert!(msg.contains("character varying(3)"), "{msg}");
    assert!(!msg.contains("secretvalue"), "{msg}");
}

/// CHAR(n) 列への SET 式代入は束縛段で受理され、評価結果が空白詰めされて保存される
/// （TABLE-31・Issue #1566）。
#[test]
fn char_column_accepts_set_expression_and_pads() {
    let (_p, _g, core) = open("cl-set-expr");
    ok(&core, "CREATE TABLE t (c CHAR(3))");
    ok(
        &core,
        "INSERT INTO t (id, c) VALUES (1, 'AB') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "UPDATE t SET c = lower(c) WHERE id = 1 USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        texts(&core, "SELECT c FROM t WHERE id = 1 LIMIT 10"),
        vec![Some("ab ".to_string())]
    );
}
