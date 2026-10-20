//! 多次元配列の型表現・行 codec・リテラル解析（対象ビヘイビア: TABLE-35。Issue #1605）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価 → 行コーデック）
//! を production 経路として、`<型>[][]` の宣言、`'{{1,2},{3,4}}'` の入れ子リテラル、次元の揃い方の
//! 検証（不整列は fail-closed の `22P02`）、再オープン後のカタログ・値の復元、次元込みの等価判定・
//! UNIQUE・集合演算、テナント境界を固定する。

use engine::catalog::{ColumnType, MAX_ARRAY_DIMS};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::ArrayValue as V;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

/// 失敗を前提にした SQL の `wire_code`。成功してしまった場合は SQL を添えて落とす。
fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn select(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} failed: {e:?}"))
}

fn first_cell(core: &EngineCore, tenant: &str, sql: &str) -> Cell {
    select(core, tenant, sql)
        .rows
        .first()
        .and_then(|row| row.cells.first())
        .cloned()
        .unwrap_or_else(|| panic!("{sql} returned no cell"))
}

/// 先頭セルの配列値を PostgreSQL テキスト形式で返す。
fn text_of(core: &EngineCore, sql: &str) -> String {
    match first_cell(core, "alice", sql) {
        Cell::Array(a) => a.to_pg_text(),
        other => panic!("{sql}: expected an array, got {other:?}"),
    }
}

fn row_count(core: &EngineCore, tenant: &str, table: &str) -> usize {
    select(core, tenant, &format!("SELECT id FROM {table} LIMIT 1000"))
        .rows
        .len()
}

fn ids(core: &EngineCore, tenant: &str, predicate: &str) -> Vec<u64> {
    let sql = format!("SELECT id FROM t WHERE {predicate} LIMIT 100");
    let mut v: Vec<u64> = select(core, tenant, &sql)
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2) NOT NULL, m INTEGER[][], \
    s TEXT[], c TEXT[][][8], u INTEGER[][] UNIQUE)";

fn insert(core: &EngineCore, tenant: &str, id: u64, cols: &str, vals: &str) {
    ok(
        core,
        tenant,
        &format!(
            "INSERT INTO t (id, embedding, {cols}) VALUES ({id}, '[0.1,0.2]', {vals}) \
             USING OPERATION_ID 'op-{tenant}-{id}'"
        ),
    );
}

#[test]
fn declares_stores_and_restores_multi_dimensional_arrays_after_reopen() {
    let path = unique_db_path("md-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        ok(&core, "alice", CREATE);
        insert(&core, "alice", 1, "m, s", "'{{1,2},{3,4}}', '{a,b}'");
        insert(&core, "alice", 2, "m", "'{{1,NULL,3},{4,5,6}}'");
        insert(&core, "alice", 3, "m", "'{}'");
        insert(&core, "alice", 4, "m", "'{{{1,2},{3,4}},{{5,6},{7,8}}}'");
        insert(&core, "alice", 5, "c", "'{{{x}},{{y}}}'");
        insert(&core, "alice", 6, "m", "NULL");
        assert_eq!(
            text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
            "{{1,2},{3,4}}"
        );
        assert_eq!(
            text_of(&core, "SELECT m FROM t WHERE id = 2 LIMIT 1"),
            "{{1,NULL,3},{4,5,6}}"
        );
    }
    // 再オープン後もカタログ（宣言次元数）と値が復元される。
    let storage = Storage::open(&path).expect("reopen storage");
    let schema = storage.get_table_schema("t").expect("schema");
    let ndims = |name: &str| match &schema
        .columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("column {name}"))
        .ty
    {
        ColumnType::Array(a) => a.ndims(),
        other => panic!("{name}: expected array, got {other:?}"),
    };
    assert_eq!(ndims("m"), 2);
    assert_eq!(ndims("s"), 1);
    assert_eq!(ndims("c"), 3);
    drop(storage);
    let core = core_at(&path);
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 2 LIMIT 1"),
        "{{1,NULL,3},{4,5,6}}"
    );
    assert_eq!(text_of(&core, "SELECT m FROM t WHERE id = 3 LIMIT 1"), "{}");
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 4 LIMIT 1"),
        "{{{1,2},{3,4}},{{5,6},{7,8}}}"
    );
    assert_eq!(
        text_of(&core, "SELECT c FROM t WHERE id = 5 LIMIT 1"),
        "{{{x}},{{y}}}"
    );
    assert_eq!(
        text_of(&core, "SELECT s FROM t WHERE id = 1 LIMIT 1"),
        "{a,b}"
    );
    // 再オープン後に UPDATE しても次元が保たれる。
    ok(
        &core,
        "alice",
        "UPDATE t SET s = '{z}' WHERE id = 1 USING OPERATION_ID 'u-1'",
    );
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
        "{{1,2},{3,4}}"
    );
}

/// 次元数の宣言は格納できる値を制限しない（TABLE-35 (2)。PostgreSQL と同じ）。
#[test]
fn declared_dimensions_do_not_restrict_stored_values() {
    let path = unique_db_path("md-decl");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    insert(&core, "alice", 1, "s", "'{{a,b},{c,d}}'");
    insert(&core, "alice", 2, "m", "'{1,2,3}'");
    assert_eq!(
        text_of(&core, "SELECT s FROM t WHERE id = 1 LIMIT 1"),
        "{{a,b},{c,d}}"
    );
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 2 LIMIT 1"),
        "{1,2,3}"
    );
}

/// 不整列リテラルは INSERT・UPDATE SET・CAST のいずれでも `22P02` で拒否され、書き込まれない
/// （fail-closed）。
#[test]
fn ragged_literals_are_rejected_without_side_effects() {
    let path = unique_db_path("md-ragged");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    insert(&core, "alice", 1, "m", "'{{1,2},{3,4}}'");
    for lit in [
        "{{1,2},{3}}",
        "{1,{2}}",
        "{{1},2}",
        "{{1},NULL}",
        "{{1,2},{3,4}",
        "{{}}",
    ] {
        assert_eq!(
            code(
                &core,
                &format!(
                    "INSERT INTO t (id, embedding, m) VALUES (9, '[0.1,0.2]', '{lit}') \
                     USING OPERATION_ID 'bad-{}'",
                    lit.len()
                )
            ),
            "22P02",
            "{lit}"
        );
        assert_eq!(
            code(
                &core,
                &format!("UPDATE t SET m = '{lit}' WHERE id = 1 USING OPERATION_ID 'badu'")
            ),
            "22P02",
            "{lit}"
        );
        assert_eq!(
            code(&core, &format!("SELECT CAST('{lit}' AS int[])")),
            "22P02",
            "{lit}"
        );
    }
    assert_eq!(row_count(&core, "alice", "t"), 1);
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
        "{{1,2},{3,4}}"
    );
}

#[test]
fn element_count_and_dimension_limits_are_54000() {
    let path = unique_db_path("md-limits");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    // `c` は `[8]`（総要素数の上限 8）。9 要素は 54000、8 要素は受理。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, embedding, c) VALUES (1, '[0.1,0.2]', \
             '{{{1,2,3}},{{4,5,6}},{{7,8,9}}}') USING OPERATION_ID 'o1'"
        ),
        "54000"
    );
    insert(&core, "alice", 2, "c", "'{{{1,2},{3,4}},{{5,6},{7,8}}}'");
    // 7 次元のリテラルは 54000、7 次元の宣言も 54000。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, embedding, m) VALUES (3, '[0.1,0.2]', '{{{{{{{1}}}}}}}') \
             USING OPERATION_ID 'o3'"
        ),
        "54000"
    );
    let dims = "[]".repeat(usize::from(MAX_ARRAY_DIMS) + 1);
    assert_eq!(
        code(&core, &format!("CREATE TABLE d (x INTEGER{dims})")),
        "54000"
    );
    ok(
        &core,
        "alice",
        &format!(
            "CREATE TABLE d (embedding VECTOR(2) NOT NULL, x INTEGER{})",
            "[]".repeat(usize::from(MAX_ARRAY_DIMS))
        ),
    );
    assert_eq!(row_count(&core, "alice", "t"), 1);
}

/// 次元込みの等価判定・UNIQUE・集合演算: 要素列が同じでも次元が違えば別の値。
#[test]
fn dimensions_participate_in_equality_unique_and_set_operations() {
    let path = unique_db_path("md-eq");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    insert(&core, "alice", 1, "m, u", "'{1,2,3,4}', '{1,2,3,4}'");
    insert(
        &core,
        "alice",
        2,
        "m, u",
        "'{{1,2},{3,4}}', '{{1,2},{3,4}}'",
    );
    insert(&core, "alice", 3, "m, u", "'{{1,2,3,4}}', '{{1,2,3,4}}'");
    insert(
        &core,
        "alice",
        4,
        "m, u",
        "'{{1},{2},{3},{4}}', '{{1},{2},{3},{4}}'",
    );
    assert_eq!(ids(&core, "alice", "m = '{{1,2},{3,4}}'"), vec![2]);
    assert_eq!(ids(&core, "alice", "m = '{1,2,3,4}'"), vec![1]);
    assert_eq!(ids(&core, "alice", "m = '{{1,2,3,4}}'"), vec![3]);
    // 同じ値の再挿入は UNIQUE 違反、次元違いは共存できる（4 行とも入っている）。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, embedding, u) VALUES (5, '[0.1,0.2]', '{{1,2},{3,4}}') \
             USING OPERATION_ID 'dup'"
        ),
        "23505"
    );
    assert_eq!(row_count(&core, "alice", "t"), 4);
    // 集合演算は 4 値を別の行として扱う。
    let union = select(
        &core,
        "alice",
        "SELECT m FROM t UNION SELECT m FROM t LIMIT 100",
    );
    assert_eq!(union.rows.len(), 4);
}

#[test]
fn subscript_slice_and_cast_on_multi_dimensional_values() {
    let path = unique_db_path("md-ops");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    insert(&core, "alice", 1, "m", "'{{1,2},{3,4},{5,6}}'");
    // 多次元値への単一添字は NULL。
    assert!(matches!(
        first_cell(&core, "alice", "SELECT m[1] FROM t WHERE id = 1 LIMIT 1"),
        Cell::Null
    ));
    // スライスは第 1 次元に沿って切り出す。
    assert_eq!(
        text_of(&core, "SELECT m[2:3] FROM t WHERE id = 1 LIMIT 1"),
        "{{3,4},{5,6}}"
    );
    assert_eq!(
        text_of(&core, "SELECT m[4:5] FROM t WHERE id = 1 LIMIT 1"),
        "{}"
    );
    // CAST は次元を保つ。`::text` との往復も同じ値へ戻る。
    assert_eq!(
        text_of(&core, "SELECT CAST('{{1,2},{3,4}}' AS int[][])"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        text_of(
            &core,
            "SELECT CAST(m AS bigint[]) FROM t WHERE id = 1 LIMIT 1"
        ),
        "{{1,2},{3,4},{5,6}}"
    );
    let text = match first_cell(
        &core,
        "alice",
        "SELECT CAST(m AS text) FROM t WHERE id = 1 LIMIT 1",
    ) {
        Cell::Text(s) => s,
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(text, "{{1,2},{3,4},{5,6}}");
    assert_eq!(
        text_of(&core, &format!("SELECT CAST('{text}' AS int[])")),
        text
    );
    // 入れ子の ARRAY[...] と多重添字は従来どおり構文段で拒否する。
    assert_eq!(code(&core, "SELECT ARRAY[[1,2],[3,4]]"), "0A000");
    assert_eq!(
        code(&core, "SELECT m[1][1] FROM t WHERE id = 1 LIMIT 1"),
        "0A000"
    );
}

#[test]
fn multi_dimensional_rows_are_tenant_isolated() {
    let path = unique_db_path("md-tenant");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    insert(&core, "alice", 1, "m", "'{{1,2},{3,4}}'");
    insert(&core, "bob", 2, "m", "'{{9,9},{9,9}}'");
    assert_eq!(ids(&core, "alice", "m = '{{1,2},{3,4}}'"), vec![1]);
    assert_eq!(ids(&core, "bob", "m = '{{1,2},{3,4}}'"), Vec::<u64>::new());
    assert_eq!(ids(&core, "bob", "m = '{{9,9},{9,9}}'"), vec![2]);
    assert_eq!(row_count(&core, "alice", "t"), 1);
    assert_eq!(row_count(&core, "bob", "t"), 1);
}

#[test]
fn multi_dimensional_array_value_helpers_agree_with_sql_surface() {
    // SQL の結果は `with_dims` で組んだ値と一致する（値表現の単一の構築経路）。
    let path = unique_db_path("md-value");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    let got = match first_cell(&core, "alice", "SELECT CAST('{{1,2},{3,4}}' AS int[][])") {
        Cell::Array(a) => a,
        other => panic!("expected array, got {other:?}"),
    };
    let expected = V::with_dims(
        V::Integer(vec![Some(1), Some(2), Some(3), Some(4)]),
        vec![2, 2],
    )
    .expect("valid dims");
    assert_eq!(got, expected);
}
/// 成功する `UPDATE ... SET` と UPSERT（`ON CONFLICT`）が多次元値を次元込みで書く。SET の事前検証
/// （`tenant::validate_set_assignments` が共有する `scalar_array_entry_len`）と実エンコードの
/// 乖離がないこと、UNIQUE 対象キー（`unique_key_from_values` の次元マーカー）が実書き込みで
/// 1 次元値と区別されることを確認する。
#[test]
fn update_set_and_upsert_write_multi_dimensional_values() {
    let path = unique_db_path("md-write");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    insert(
        &core,
        "alice",
        1,
        "m, u",
        "'{{1,2},{3,4}}', '{{1,2},{3,4}}'",
    );
    ok(
        &core,
        "alice",
        "UPDATE t SET m = '{{5,6},{7,NULL}}' WHERE id = 1 USING OPERATION_ID 'md-upd-1'",
    );
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
        "{{5,6},{7,NULL}}"
    );
    // UNIQUE 対象が同じ値なら DO UPDATE が既存行を更新する（行は増えない）。
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, u, m) VALUES (2, '[0.1,0.2]', '{{1,2},{3,4}}', \
         '{{9,9},{9,9}}') ON CONFLICT (u) DO UPDATE SET m = EXCLUDED.m \
         USING OPERATION_ID 'md-ups-1'",
    );
    assert_eq!(row_count(&core, "alice", "t"), 1);
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
        "{{9,9},{9,9}}"
    );
    // 要素列が同じでも次元の違う値は別キーなので、衝突せず新しい行になる。
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, u, m) VALUES (3, '[0.1,0.2]', '{1,2,3,4}', \
         '{1}') ON CONFLICT (u) DO UPDATE SET m = EXCLUDED.m USING OPERATION_ID 'md-ups-2'",
    );
    assert_eq!(row_count(&core, "alice", "t"), 2);
}

/// `COPY ... FROM STDIN` の多次元リテラル: 揃った値は入り、不整列は `22P02` で書き込みなし。
#[test]
fn copy_from_stdin_accepts_aligned_and_rejects_ragged_multi_dimensional_literals() {
    let path = unique_db_path("md-copy");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    let copy = |op: &str, data: &[u8]| {
        let policy = ctx("alice");
        let session = SessionState::default();
        let sql = format!("COPY t (id, embedding, m) FROM STDIN USING OPERATION_ID '{op}'");
        let plan = core.begin_copy(&policy, &session, &sql)?;
        let mut copy_session = match plan {
            CopyPlan::From(s) => s,
            CopyPlan::To(..) => panic!("expected COPY FROM STDIN plan"),
        };
        copy_session.feed(data)?;
        let batch = copy_session.finish()?;
        core.commit_copy_in(&policy, batch)
    };
    let outcome = copy("md-copy-1", b"1\t[0.1,0.2]\t{{1,2},{3,4}}\n").expect("aligned COPY");
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(
        text_of(&core, "SELECT m FROM t WHERE id = 1 LIMIT 1"),
        "{{1,2},{3,4}}"
    );
    let err = copy(
        "md-copy-2",
        b"2\t[0.1,0.2]\t{{1,2},{3,4}}\n3\t[0.1,0.2]\t{{1,2},{3}}\n",
    )
    .expect_err("ragged COPY must fail");
    assert_eq!(err.wire_code(), "22P02");
    assert_eq!(row_count(&core, "alice", "t"), 1);
}
