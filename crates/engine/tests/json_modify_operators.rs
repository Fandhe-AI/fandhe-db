//! jsonb の連結・削除演算子 `||` `-` `#-`（対象ビヘイビア: TABLE-25。Issue #2181）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25（関連: TABLE-14・SQL-24・SQL-42）。
//!
//! `tests/json_containment_operators.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。固定する契約: PostgreSQL の
//! 連結・削除規則（浅いマージ・文字列要素削除・負の添字・パス削除）、NULL の伝播、
//! 不正な組み合わせの SQLSTATE（22023／42883／0A000）、深さ上限の 54000、CHECK 本体の拒否、
//! テナント境界、DML の再送時の内容検査。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "modifydocs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("json-modify-operators");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `docs` の各要素を id = 1.. で挿入する（`None` は `docb` が NULL）。`n` は 1。
fn seed(core: &EngineCore, ctx: &PolicyContext, docs: &[Option<&str>]) {
    for (i, d) in docs.iter().enumerate() {
        let id = i + 1;
        let docb = match d {
            Some(d) => format!("'{d}'"),
            None => "NULL".to_string(),
        };
        core.execute_sql_in_session(
            ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n) \
                 VALUES ({id}, '[0.1,0.2]', 'k', '{{}}', {docb}, 1) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .expect("insert should succeed");
    }
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql} should succeed: {}", e.wire_code()));
    result
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Integer(v) => *v,
            Cell::SignedInteger(v) => u64::try_from(*v).expect("non-negative"),
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect()
}

fn matching(core: &EngineCore, ctx: &PolicyContext, pred: &str) -> Vec<u64> {
    let mut v = ids(
        core,
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 100"),
    );
    v.sort_unstable();
    v
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    core.execute_sql(ctx, sql)
        .expect_err("statement should fail")
        .wire_code()
        .to_string()
}

fn select_code(core: &EngineCore, ctx: &PolicyContext, expr: &str) -> String {
    code(
        core,
        ctx,
        &format!("SELECT {expr} FROM {TABLE} WHERE id = 1 LIMIT 1"),
    )
}

fn cell(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .unwrap_or_else(|e| panic!("select {expr} should succeed: {}", e.wire_code()));
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

fn json(s: &str) -> Cell {
    Cell::Json(s.to_string())
}

#[test]
fn concat_follows_postgres_rules() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"a":1,"b":{"x":1}}"#),
            Some(r#"[1,2]"#),
            Some(r#"5"#),
            Some(r#"{}"#),
            Some(r#"null"#),
            None,
        ],
    );
    // オブジェクト同士は浅いマージ。重複キーは右辺が勝ち、入れ子は置き換え。
    assert_eq!(
        cell(&core, &alice, r#"docb || '{"b":{"y":2},"c":3}'"#, 1),
        json(r#"{"a":1,"b":{"y":2},"c":3}"#)
    );
    // 配列同士は連結。
    assert_eq!(cell(&core, &alice, "docb || '[3]'", 2), json("[1,2,3]"));
    // 配列でない側は 1 要素配列とみなす。
    assert_eq!(cell(&core, &alice, "docb || '1'", 2), json("[1,2,1]"));
    assert_eq!(cell(&core, &alice, "docb || '[1]'", 3), json("[5,1]"));
    assert_eq!(cell(&core, &alice, "docb || '2'", 3), json("[5,2]"));
    assert_eq!(
        cell(&core, &alice, r#"docb || '[2]'"#, 1),
        json(r#"[{"a":1,"b":{"x":1}},2]"#)
    );
    assert_eq!(cell(&core, &alice, "docb || '{}'", 4), json("{}"));
    assert_eq!(cell(&core, &alice, "docb || '[]'", 4), json("[{}]"));
    // JSON の null スカラー。
    assert_eq!(
        cell(&core, &alice, "docb || 'null'", 5),
        json("[null,null]")
    );
    // リテラルを左辺に置く形。
    assert_eq!(
        matching(
            &core,
            &alice,
            "docb = '[1,2]' AND '[0]' || docb = '[0,1,2]'"
        ),
        vec![2]
    );
    // 連鎖は左結合。
    assert_eq!(
        cell(&core, &alice, r#"docb || '{"x":1}' || '{"y":2}'"#, 4),
        json(r#"{"x":1,"y":2}"#)
    );
    // SQL の NULL は伝播する。
    assert_eq!(cell(&core, &alice, "docb || '{}'", 6), Cell::Null);
    // 結果を比較・述語に使える（`||` は `->` と同じ段で左結合）。
    assert_eq!(
        matching(
            &core,
            &alice,
            r#"docb || '{"z":1}' = '{"a":1,"b":{"x":1},"z":1}'"#
        ),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, r#"docb || '{"z":1}' ? 'z'"#),
        vec![1, 4]
    );
    assert_eq!(
        cell(
            &core,
            &alice,
            r#"docb || '{"z":{"q":7}}' -> 'z' ->> 'q'"#,
            4
        ),
        Cell::Text("7".to_string())
    );
}

#[test]
fn delete_by_key_follows_postgres_rules() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"a":1,"b":{"a":2}}"#),
            Some(r#"["a","b","a",1,{"a":1}]"#),
            Some(r#"7"#),
            Some(r#"{}"#),
            None,
        ],
    );
    assert_eq!(
        cell(&core, &alice, "docb - 'a'", 1),
        json(r#"{"b":{"a":2}}"#)
    );
    assert_eq!(
        cell(&core, &alice, "docb - 'zz'", 1),
        json(r#"{"a":1,"b":{"a":2}}"#)
    );
    // 配列は同じ文字列要素をすべて削除する。数値の 1 は '1' では消えない。
    assert_eq!(
        cell(&core, &alice, "docb - 'a'", 2),
        json(r#"["b",1,{"a":1}]"#)
    );
    assert_eq!(
        cell(&core, &alice, "docb - '1'", 2),
        json(r#"["a","b","a",1,{"a":1}]"#)
    );
    assert_eq!(cell(&core, &alice, "docb - 'a'", 4), json("{}"));
    assert_eq!(cell(&core, &alice, "docb - 'a'", 5), Cell::Null);
    // 連鎖と、`-` の後ろの `->`（加減算の段のほうが強い）。
    assert_eq!(cell(&core, &alice, "docb - 'a' - 'b'", 1), json("{}"));
    assert_eq!(
        cell(&core, &alice, "docb - 'a' -> 'b'", 1),
        json(r#"{"a":2}"#)
    );
    // スカラーからの削除は 22023。
    assert_eq!(select_code_for(&core, &alice, "docb - 'a'", 3), "22023");
}

fn select_code_for(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> String {
    code(
        core,
        ctx,
        &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
    )
}

#[test]
fn delete_by_index_follows_postgres_rules() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"[10,20,30]"#),
            Some(r#"{"a":1}"#),
            Some(r#""s""#),
            Some(r#"[]"#),
        ],
    );
    assert_eq!(cell(&core, &alice, "docb - 0", 1), json("[20,30]"));
    assert_eq!(cell(&core, &alice, "docb - 2", 1), json("[10,20]"));
    assert_eq!(cell(&core, &alice, "docb - -1", 1), json("[10,20]"));
    assert_eq!(cell(&core, &alice, "docb - -3", 1), json("[20,30]"));
    // 範囲外は変化なし。
    assert_eq!(cell(&core, &alice, "docb - 3", 1), json("[10,20,30]"));
    assert_eq!(cell(&core, &alice, "docb - -4", 1), json("[10,20,30]"));
    assert_eq!(cell(&core, &alice, "docb - 0", 4), json("[]"));
    // int4 の式（列・四則演算）でも削除できる。n = 1。
    assert_eq!(cell(&core, &alice, "docb - n", 1), json("[10,30]"));
    assert_eq!(cell(&core, &alice, "docb - (n + 1)", 1), json("[10,20]"));
    // オブジェクト・スカラーは 22023。
    assert_eq!(select_code_for(&core, &alice, "docb - 0", 2), "22023");
    assert_eq!(select_code_for(&core, &alice, "docb - 0", 3), "22023");
    // bigint 範囲の整数・小数は 42883。
    assert_eq!(
        select_code_for(&core, &alice, "docb - 4294967296", 1),
        "42883"
    );
    assert_eq!(select_code_for(&core, &alice, "docb - 1.5", 1), "42883");
}

#[test]
fn delete_path_follows_postgres_rules() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(
        &core,
        &alice,
        &[
            Some(r#"{"a":{"b":1,"c":[1,2,3]},"d":[{"x":1,"y":2}]}"#),
            Some(r#"3"#),
            Some(r#"[1,2]"#),
            None,
        ],
    );
    assert_eq!(
        cell(&core, &alice, "docb #- '{a,b}'", 1),
        json(r#"{"a":{"c":[1,2,3]},"d":[{"x":1,"y":2}]}"#)
    );
    assert_eq!(
        cell(&core, &alice, "docb #- '{a,c,1}'", 1),
        json(r#"{"a":{"b":1,"c":[1,3]},"d":[{"x":1,"y":2}]}"#)
    );
    assert_eq!(
        cell(&core, &alice, "docb #- '{a,c,-1}'", 1),
        json(r#"{"a":{"b":1,"c":[1,2]},"d":[{"x":1,"y":2}]}"#)
    );
    assert_eq!(
        cell(&core, &alice, "docb #- '{d,0,x}'", 1),
        json(r#"{"a":{"b":1,"c":[1,2,3]},"d":[{"y":2}]}"#)
    );
    // 存在しないパス・スカラーを経由するパス・空のパス・範囲外の添字は変化なし。
    let whole = json(r#"{"a":{"b":1,"c":[1,2,3]},"d":[{"x":1,"y":2}]}"#);
    assert_eq!(cell(&core, &alice, "docb #- '{z,y}'", 1), whole);
    assert_eq!(cell(&core, &alice, "docb #- '{a,b,q}'", 1), whole);
    assert_eq!(cell(&core, &alice, "docb #- '{}'", 1), whole);
    assert_eq!(cell(&core, &alice, "docb #- '{a,c,9}'", 1), whole);
    assert_eq!(cell(&core, &alice, "docb #- '{0}'", 3), json("[2]"));
    assert_eq!(cell(&core, &alice, "docb #- '{a}'", 4), Cell::Null);
    // 配列の段の非整数は 22023、ルートがスカラーなら 22023。
    assert_eq!(
        select_code_for(&core, &alice, "docb #- '{a,c,x}'", 1),
        "22023"
    );
    assert_eq!(select_code_for(&core, &alice, "docb #- '{a}'", 2), "22023");
    // NULL 要素と非リテラルの右辺は 0A000（fail-closed）。
    assert_eq!(
        select_code_for(&core, &alice, "docb #- '{a,NULL}'", 1),
        "0A000"
    );
    assert_eq!(select_code_for(&core, &alice, "docb #- lang", 1), "0A000");
}

#[test]
fn type_errors_use_fixed_sqlstates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"a":1}"#)]);
    // 文字列連結は未実装（PostgreSQL では text の連結）。
    assert_eq!(select_code(&core, &alice, "lang || 'x'"), "0A000");
    assert_eq!(select_code(&core, &alice, "docb || lang"), "0A000");
    assert_eq!(select_code(&core, &alice, "doc || '{}'"), "0A000");
    // jsonb と数値は演算子なし。リテラル同士も 42883。
    assert_eq!(select_code(&core, &alice, "docb || n"), "42883");
    assert_eq!(
        code(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE '{{}}' || '{{}}' = docb LIMIT 1")
        ),
        "42883"
    );
    // json 列は jsonb 専用の `-`・`#-` を適用できない。
    assert_eq!(select_code(&core, &alice, "doc - 'a'"), "42883");
    assert_eq!(select_code(&core, &alice, "doc - 1"), "42883");
    assert_eq!(select_code(&core, &alice, "doc #- '{a}'"), "42883");
    // 不正な jsonb リテラルは 22P02。
    assert_eq!(select_code(&core, &alice, "docb || '{'"), "22P02");
    // 既存の算術は影響を受けない。
    assert_eq!(cell(&core, &alice, "n - 1", 1), Cell::SignedInteger(0));
}

#[test]
fn result_exceeding_depth_limit_is_rejected_with_54000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 深さ 16 のオブジェクト（上限ちょうど）。
    let deep = format!("{}{{}}{}", "{\"a\":".repeat(15), "}".repeat(15));
    seed(&core, &alice, &[Some(&deep)]);
    assert_eq!(select_code(&core, &alice, "docb || '1'"), "54000");
    // 配列へ包まれない結果（同種のオブジェクト）は上限内。
    assert!(core
        .execute_sql(
            &alice,
            &format!("SELECT docb || '{{\"b\":1}}' FROM {TABLE} WHERE id = 1 LIMIT 1")
        )
        .is_ok());
}

#[test]
fn check_body_rejects_the_new_operators() {
    let (core, _path_guard) = {
        let (c, p) = new_core();
        (c, CleanupGuard(p))
    };
    let alice = ctx_for("alice");
    let run = |sql: &str| {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&alice, &mut session, sql)
    };
    for (i, body) in [
        "docb || '{}' = docb",
        "docb - 'k' = docb",
        "docb - 1 = docb",
        "docb - 'a' - 'b' = docb",
        "(docb - 'k') -> 'a' = docb",
        "docb #- '{k}' = docb",
    ]
    .iter()
    .enumerate()
    {
        let err = run(&format!(
            "CREATE TABLE chk{i} (embedding VECTOR(2), docb JSONB, n INT, CHECK ({body}))"
        ))
        .map(|_| ())
        .expect_err("CHECK with a jsonb modify operator must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{body}: {err:?}");
    }
    // 数値型の '-' は従来どおり CHECK で使える（対照）。
    run("CREATE TABLE chk_ok (embedding VECTOR(2), docb JSONB, n INT, CHECK (n - 1 >= 0))")
        .expect("numeric minus in CHECK is still accepted");
}

#[test]
fn other_tenants_rows_never_appear_in_results_or_errors() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    seed(&core, &alice, &[Some(r#"{"a":1}"#)]);
    assert_eq!(
        matching(&core, &bob, r#"docb || '{"z":1}' ? 'z'"#),
        Vec::<u64>::new()
    );
    let result = core
        .execute_sql(
            &bob,
            &format!("SELECT docb - 'a' FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .expect("bob sees no rows");
    assert!(result.rows.is_empty());
}

#[test]
fn dml_where_with_modify_operators_acts_on_matching_rows_and_resend_is_content_checked() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(r#"{"k":1}"#), Some(r#"{"j":1}"#)]);
    let run = |sql: &str| core.execute_sql_in_session(&alice, &mut SessionState::default(), sql);
    let upd = |pred: &str| {
        format!("UPDATE {TABLE} SET n = 5 WHERE {pred} USING OPERATION_ID 'upd-modify'")
    };
    run(&upd("docb || '{\"z\":1}' ? 'k'")).expect("first update succeeds");
    assert_eq!(matching(&core, &alice, "n = 5"), vec![1]);
    assert_eq!(
        run(&upd("docb || '{\"z\":1}' ? 'k'"))
            .expect_err("resend")
            .wire_code(),
        "23505"
    );
    // 演算子だけを変えた再送（`||` → `#-`、`-`）は内容不一致の 22023。
    assert_eq!(
        run(&upd("docb #- '{z}' ? 'k'"))
            .expect_err("different op")
            .wire_code(),
        "22023"
    );
    assert_eq!(
        run(&upd("docb - 'z' ? 'k'"))
            .expect_err("different op")
            .wire_code(),
        "22023"
    );
    run(&format!(
        "DELETE FROM {TABLE} WHERE docb - 'j' = '{{}}' USING OPERATION_ID 'del-modify'"
    ))
    .expect("delete with a delete operator succeeds");
    assert_eq!(matching(&core, &alice, "docb ? 'k'"), vec![1]);
    assert_eq!(matching(&core, &alice, "docb ? 'j'"), Vec::<u64>::new());
}
