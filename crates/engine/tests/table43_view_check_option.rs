//! ビューの `WITH [CASCADED | LOCAL] CHECK OPTION`（TABLE-43 (2)(3)・ERR-6 の `44000` 行。
//! Issue #1673）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-43。
//!
//! `table43_view_update_delete.rs` と同じ流儀（実 `Storage`＋`EngineCore::execute_sql_in_session`）。
//! 検証する契約: (1) 宣言の受理・永続化・`OR REPLACE` での付け外し (2) 自動更新できない形への宣言は
//! `0A000`・構文の誤りは `42601` (3) ビュー経由の INSERT／UPDATE／UPSERT で、書き込み後の行が
//! 検査対象の述語を満たさなければ `44000` で文全体を拒否し副作用を残さない（NULL も違反・DELETE は
//! 検査しない） (4) 入れ子での LOCAL／CASCADED の意味 (5) 検査の順序（CHECK・UNIQUE が先）
//! (6) テナント境界は基表と同じ。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("body", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    for (id, tenant, lang) in [(1u64, "alice", "ja"), (2, "alice", "en"), (3, "bob", "ja")] {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx(tenant),
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                Value::Text(format!("body {id}")),
                Value::Text(id.to_string()),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("seed row");
    }
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed(&storage);
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    run(core, tenant, sql).expect_err(sql)
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    err(core, tenant, sql).wire_code().to_string()
}

fn ddl_result(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn ddl(core: &EngineCore, sql: &str) {
    ddl_result(core, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn ddl_code(core: &EngineCore, sql: &str) -> String {
    ddl_result(core, sql)
        .expect_err(sql)
        .wire_code()
        .to_string()
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let mut v: Vec<u64> = query(core, tenant, "SELECT id FROM docs LIMIT 100")
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

fn body_of(core: &EngineCore, id: u64) -> String {
    let q = query(
        core,
        "alice",
        &format!("SELECT body FROM docs WHERE id = {id} LIMIT 1"),
    );
    match &q.rows[0].cells[0] {
        Cell::Text(s) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

const OP: &str = "USING OPERATION_ID";

fn insert_sql(view: &str, id: u64, lang: &str, n: i32, op: &str) -> String {
    insert_sql_b(view, id, lang, n, "ok", op)
}

fn insert_sql_b(view: &str, id: u64, lang: &str, n: i32, body: &str, op: &str) -> String {
    format!(
        "INSERT INTO {view} (id, embedding, lang, body, n) VALUES ({id}, '[1,1]', '{lang}', '{body}', '{n}') {OP} '{op}'"
    )
}

/// 宣言: 3 通りの記法を受理し、修飾子の省略は CASCADED として働く。
#[test]
fn declaration_forms_are_accepted() {
    let (core, _g) = open_core("t43c-decl");
    for (name, tail) in [
        ("v_def", "WITH CHECK OPTION"),
        ("v_loc", "WITH LOCAL CHECK OPTION"),
        ("v_cas", "with cascaded check option;"),
    ] {
        ddl(
            &core,
            &format!("CREATE VIEW {name} AS SELECT * FROM docs WHERE lang = 'ja' {tail}"),
        );
        assert_eq!(
            code(&core, "alice", &insert_sql(name, 10, "en", 1, "op-decl")),
            "44000",
            "{name}"
        );
    }
    // 省略した宣言は CASCADED: 下位のビューが無宣言でも、下位の述語まで検査する。
    ddl(
        &core,
        "CREATE VIEW base_v AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ddl(
        &core,
        "CREATE VIEW top_v AS SELECT * FROM base_v WHERE n = '1' WITH CHECK OPTION",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &insert_sql("top_v", 10, "en", 1, "op-decl-2")
        ),
        "44000"
    );
}

/// 宣言の拒否: 自動更新できない形は `0A000`、構文の誤りは `42601`（副作用ゼロ）。
#[test]
fn declaration_rejections() {
    let (core, _g) = open_core("t43c-reject");
    for body in [
        "SELECT * FROM docs WHERE lang = 'ja' LIMIT 5",
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        "SELECT DISTINCT lang FROM docs",
    ] {
        assert_eq!(
            ddl_code(
                &core,
                &format!("CREATE VIEW bad AS {body} WITH CHECK OPTION")
            ),
            "0A000",
            "{body}"
        );
    }
    // 評価後射影形ビューの上の単純形ビュー。
    ddl(&core, "CREATE VIEW lim AS SELECT * FROM docs LIMIT 5");
    assert_eq!(
        ddl_code(
            &core,
            "CREATE VIEW over_lim AS SELECT * FROM lim WITH CHECK OPTION"
        ),
        "0A000"
    );
    // 構文の誤り。
    for tail in [
        "WITH FOO CHECK OPTION",
        "WITH CHECK",
        "CHECK OPTION",
        "WITH CHECK OPTION extra",
        "WITH LOCAL CASCADED CHECK OPTION",
    ] {
        assert_eq!(
            ddl_code(
                &core,
                &format!("CREATE VIEW bad AS SELECT * FROM docs WHERE lang = 'ja' {tail}")
            ),
            "42601",
            "{tail}"
        );
    }
    // MATERIALIZED VIEW には宣言できない。
    assert_eq!(
        ddl_code(
            &core,
            "CREATE MATERIALIZED VIEW mv AS SELECT * FROM docs WITH CHECK OPTION"
        ),
        "42601"
    );
    // どの失敗もビューを残さない。
    assert_eq!(code(&core, "alice", "SELECT * FROM bad LIMIT 1"), "42P01");
}

/// `OR REPLACE` で付け外しでき、旧定義の宣言は引き継がれない。
#[test]
fn or_replace_adds_and_drops_the_option() {
    let (core, _g) = open_core("t43c-replace");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ok(&core, "alice", &insert_sql("sv", 10, "en", 1, "op-r-1"));
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    assert_eq!(
        code(&core, "alice", &insert_sql("sv", 11, "en", 1, "op-r-2")),
        "44000"
    );
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ok(&core, "alice", &insert_sql("sv", 11, "en", 1, "op-r-3"));
}

/// 再オープン後も宣言が残る（永続化）。
#[test]
fn declaration_survives_reopen() {
    let path = unique_db_path("t43c-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        seed(&storage);
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        ddl(
            &core,
            "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH LOCAL CHECK OPTION",
        );
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        code(&core, "alice", &insert_sql("sv", 10, "en", 1, "op-ro")),
        "44000"
    );
}

/// INSERT: 違反は `44000`・固定文言で、ビュー名・行の値を含まず、複数行の 1 行だけの違反でも 0 行。
#[test]
fn insert_violation_is_rejected_atomically() {
    let (core, _g) = open_core("t43c-insert");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    // 適合する行は書ける。
    ok(&core, "alice", &insert_sql("sv", 10, "ja", 1, "op-i-1"));
    assert_eq!(ids(&core, "alice"), vec![1, 2, 3, 10]);
    // 違反。
    let e = err(
        &core,
        "alice",
        &insert_sql("sv", 11, "secret_lang", 1, "op-i-2"),
    );
    assert_eq!(e.wire_code(), "44000");
    let message = e.to_string();
    assert_eq!(message, "new row violates check option for view");
    assert!(!message.contains("sv") && !message.contains("secret_lang"));
    // 複数行のうち 1 行だけ違反 → 全体が拒否される。
    let multi = format!(
        "INSERT INTO sv (id, embedding, lang, body, n) VALUES (20, '[1,1]', 'ja', 'a', '1'), (21, '[1,1]', 'en', 'b', '1') {OP} 'op-i-3'"
    );
    assert_eq!(code(&core, "alice", &multi), "44000");
    assert_eq!(ids(&core, "alice"), vec![1, 2, 3, 10]);
    // 同じ operation_id で適合する文を再送すると適用される（台帳に残っていない）。
    ok(&core, "alice", &insert_sql("sv", 11, "ja", 1, "op-i-2"));
    assert_eq!(ids(&core, "alice"), vec![1, 2, 3, 10, 11]);
}

/// NULL（述語列の省略・NULL 代入）も違反。
#[test]
fn null_predicate_result_is_a_violation() {
    let (core, _g) = open_core("t43c-null");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    let omit =
        format!("INSERT INTO sv (id, embedding, body) VALUES (10, '[1,1]', 'x') {OP} 'op-n-1'");
    assert_eq!(code(&core, "alice", &omit), "44000");
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("UPDATE sv SET lang = NULL WHERE id = 1 {OP} 'op-n-2'")
        ),
        "44000"
    );
    assert_eq!(ids(&core, "alice"), vec![1, 2, 3]);
}

/// UPDATE: ビューの条件の外へ動かす更新は `44000`（単一行形・述語形）。適合する更新は通る。
#[test]
fn update_violation_is_rejected() {
    let (core, _g) = open_core("t43c-update");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("UPDATE sv SET lang = 'en' WHERE id = 1 {OP} 'op-u-1'")
        ),
        "44000"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("UPDATE sv SET lang = 'en' WHERE body LIKE 'body%' {OP} 'op-u-2'")
        ),
        "44000"
    );
    assert_eq!(body_of(&core, 1), "body 1");
    ok(
        &core,
        "alice",
        &format!("UPDATE sv SET body = 'changed' WHERE id = 1 {OP} 'op-u-3'"),
    );
    assert_eq!(body_of(&core, 1), "changed");
}

/// DELETE は検査しない。
#[test]
fn delete_is_not_checked() {
    let (core, _g) = open_core("t43c-delete");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    ok(
        &core,
        "alice",
        &format!("DELETE FROM sv WHERE id = 1 {OP} 'op-d-1'"),
    );
    assert_eq!(ids(&core, "alice"), vec![2, 3]);
}

/// UPSERT: 挿入行の違反と DO UPDATE 後の違反は `44000`。DO NOTHING で衝突した行は検査しない。
#[test]
fn upsert_is_checked() {
    let (core, _g) = open_core("t43c-upsert");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    // 挿入行の違反。
    let ins = format!(
        "INSERT INTO sv (id, embedding, lang, body, n) VALUES (10, '[1,1]', 'en', 'x', '1') ON CONFLICT (id) DO NOTHING {OP} 'op-up-1'"
    );
    assert_eq!(code(&core, "alice", &ins), "44000");
    // DO NOTHING で衝突した行（id 1。違反する値を持つが書かれない）は検査されない。
    let nothing = format!(
        "INSERT INTO sv (id, embedding, lang, body, n) VALUES (1, '[1,1]', 'en', 'x', '1') ON CONFLICT (id) DO NOTHING {OP} 'op-up-2'"
    );
    ok(&core, "alice", &nothing);
    assert_eq!(body_of(&core, 1), "body 1");
    // DO UPDATE 後の行がビューの外へ出る。
    let upd = format!(
        "INSERT INTO sv (id, embedding, lang, body, n) VALUES (1, '[1,1]', 'ja', 'x', '1') ON CONFLICT (id) DO UPDATE SET lang = 'en' {OP} 'op-up-3'"
    );
    assert_eq!(code(&core, "alice", &upd), "44000");
    assert_eq!(body_of(&core, 1), "body 1");
    // 適合する DO UPDATE は通る。
    let good = format!(
        "INSERT INTO sv (id, embedding, lang, body, n) VALUES (1, '[1,1]', 'ja', 'x', '1') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body {OP} 'op-up-4'"
    );
    ok(&core, "alice", &good);
    assert_eq!(body_of(&core, 1), "x");
}

/// 入れ子: 外側 v2（`n = 1`）が内側 v1（`lang = 'ja'`）の上にあるときの LOCAL／CASCADED の意味。
/// 行 A は v1 の条件だけ違反（lang='en', n=1）、行 B は v2 の条件だけ違反（lang='ja', n=2）。
#[test]
fn nested_views_follow_local_and_cascaded_rules() {
    let (core, _g) = open_core("t43c-nested");
    // (v1 の宣言, v2 の宣言, 行 A が拒否されるか, 行 B が拒否されるか)
    let cases: [(&str, &str, bool, bool); 8] = [
        ("", "", false, false),
        ("", "WITH LOCAL CHECK OPTION", false, true),
        ("", "WITH CASCADED CHECK OPTION", true, true),
        ("WITH LOCAL CHECK OPTION", "", true, false),
        (
            "WITH LOCAL CHECK OPTION",
            "WITH LOCAL CHECK OPTION",
            true,
            true,
        ),
        ("WITH CASCADED CHECK OPTION", "", true, false),
        (
            "WITH LOCAL CHECK OPTION",
            "WITH CASCADED CHECK OPTION",
            true,
            true,
        ),
        (
            "WITH CASCADED CHECK OPTION",
            "WITH LOCAL CHECK OPTION",
            true,
            true,
        ),
    ];
    for (i, (o1, o2, a_rejected, b_rejected)) in cases.iter().enumerate() {
        let v1 = format!("v1_{i}");
        let v2 = format!("v2_{i}");
        ddl(
            &core,
            &format!("CREATE VIEW {v1} AS SELECT * FROM docs WHERE lang = 'ja' {o1}"),
        );
        ddl(
            &core,
            &format!("CREATE VIEW {v2} AS SELECT * FROM {v1} WHERE n = '1' {o2}"),
        );
        let base = 100 + (i as u64) * 10;
        for (label, id, lang, n, rejected) in [
            ("A", base, "en", 1, *a_rejected),
            ("B", base + 1, "ja", 2, *b_rejected),
        ] {
            let result = run(
                &core,
                "alice",
                &insert_sql(&v2, id, lang, n, &format!("op-nest-{i}-{label}")),
            );
            match (rejected, result) {
                (true, Err(e)) => assert_eq!(e.wire_code(), "44000", "case {i} row {label}"),
                (false, Ok(_)) => {}
                (rejected, other) => {
                    panic!("case {i} row {label}: rejected={rejected}, got {other:?}")
                }
            }
        }
    }
}

/// 3 段の連鎖: CASCADED が無宣言の中間段を貫き、LOCAL は無宣言の下位を検査しない。
/// 述語は最下位 `lang = 'ja'`・中間 `n = 1`・最上位 `body = 'ok'`。
#[test]
fn three_level_chain() {
    let (core, _g) = open_core("t43c-three");
    ddl(
        &core,
        "CREATE VIEW a1 AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ddl(&core, "CREATE VIEW a2 AS SELECT * FROM a1 WHERE n = '1'");
    ddl(
        &core,
        "CREATE VIEW a3_cas AS SELECT * FROM a2 WHERE body = 'ok' WITH CASCADED CHECK OPTION",
    );
    ddl(
        &core,
        "CREATE VIEW a3_loc AS SELECT * FROM a2 WHERE body = 'ok' WITH LOCAL CHECK OPTION",
    );
    // 最下位の述語（lang）の違反: CASCADED は貫いて拒否、LOCAL は見ない。
    assert_eq!(
        code(&core, "alice", &insert_sql("a3_cas", 10, "en", 1, "op-3-1")),
        "44000"
    );
    ok(&core, "alice", &insert_sql("a3_loc", 11, "en", 1, "op-3-2"));
    // 中間段の述語（n）の違反も同様。
    assert_eq!(
        code(&core, "alice", &insert_sql("a3_cas", 12, "ja", 2, "op-3-3")),
        "44000"
    );
    ok(&core, "alice", &insert_sql("a3_loc", 13, "ja", 2, "op-3-4"));
    // 自身の述語は両方とも検査する。
    assert_eq!(
        code(
            &core,
            "alice",
            &insert_sql_b("a3_loc", 14, "ja", 1, "bad", "op-3-5")
        ),
        "44000"
    );
    // 内側が自分で宣言していれば、外側が無宣言でもその内側の述語は検査する。
    ddl(
        &core,
        "CREATE VIEW b1 AS SELECT * FROM docs WHERE lang = 'ja' WITH LOCAL CHECK OPTION",
    );
    ddl(&core, "CREATE VIEW b2 AS SELECT * FROM b1 WHERE n = '1'");
    ddl(
        &core,
        "CREATE VIEW b3 AS SELECT * FROM b2 WHERE body = 'ok'",
    );
    assert_eq!(
        code(&core, "alice", &insert_sql("b3", 20, "en", 1, "op-3-6")),
        "44000"
    );
    ok(&core, "alice", &insert_sql("b3", 21, "ja", 2, "op-3-7"));
}

/// 下位が列別名つきのとき、上位の述語は別名（公開名）で書かれていても物理列名へ写して検査する。
/// 基底が MATERIALIZED VIEW のビューへの宣言は `0A000`。
#[test]
fn check_predicates_are_mapped_through_lower_layer_aliases() {
    let (core, _g) = open_core("t43c-alias2");
    ddl(
        &core,
        "CREATE VIEW v1 (id, vec, language, text, num) AS SELECT id, embedding, lang, body, n FROM docs",
    );
    ddl(
        &core,
        "CREATE VIEW v2 AS SELECT * FROM v1 WHERE language = 'ja' WITH CHECK OPTION",
    );
    let ins = |id: u64, lang: &str, op: &str| {
        format!(
            "INSERT INTO v2 (id, vec, language, text, num) VALUES ({id}, '[1,1]', '{lang}', 'b', '1') {OP} '{op}'"
        )
    };
    assert_eq!(code(&core, "alice", &ins(10, "en", "op-al2-1")), "44000");
    ok(&core, "alice", &ins(11, "ja", "op-al2-2"));

    ddl(&core, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM docs");
    assert_eq!(
        ddl_code(
            &core,
            "CREATE VIEW over_mv AS SELECT * FROM mv WITH CHECK OPTION"
        ),
        "0A000"
    );
}

/// 列別名つきビューでも検査は物理列名で評価される。
#[test]
fn aliased_view_checks_use_physical_columns() {
    let (core, _g) = open_core("t43c-alias");
    ddl(
        &core,
        "CREATE VIEW sv (id, vec, language, text, num) AS SELECT id, embedding, lang, body, n FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    let ins = |id: u64, lang: &str, op: &str| {
        format!(
            "INSERT INTO sv (id, vec, language, text, num) VALUES ({id}, '[1,1]', '{lang}', 'b', '1') {OP} '{op}'"
        )
    };
    assert_eq!(code(&core, "alice", &ins(10, "en", "op-al-1")), "44000");
    ok(&core, "alice", &ins(11, "ja", "op-al-2"));
}

/// 検査の順序: CHECK 制約・UNIQUE が CHECK OPTION より先に報告される。
#[test]
fn constraint_order_is_check_then_unique_then_check_option() {
    let (core, _g) = open_core("t43c-order");
    ddl(
        &core,
        "ALTER TABLE docs ADD CONSTRAINT n_pos CHECK (body LIKE 'b%')",
    );
    ddl(&core, "CREATE UNIQUE INDEX ux_body ON docs (body)");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    // CHECK 制約と CHECK OPTION の両方に違反 → 23514。
    assert_eq!(
        code(&core, "alice", &insert_sql("sv", 10, "en", -1, "op-o-1")),
        "23514"
    );
    // UNIQUE と CHECK OPTION の両方に違反（body 'b10' を先に別の行で使う）→ 23505。
    ok(
        &core,
        "alice",
        &format!(
            "INSERT INTO docs (id, embedding, lang, body, n) VALUES (10, '[1,1]', 'en', 'bdup', '1') {OP} 'op-o-2'"
        ),
    );
    let dup = format!(
        "INSERT INTO sv (id, embedding, lang, body, n) VALUES (11, '[1,1]', 'en', 'bdup', '1') {OP} 'op-o-3'"
    );
    assert_eq!(code(&core, "alice", &dup), "23505");
}

/// テナント境界: 他テナントの行へ影響せず、エラーに他テナントの情報が含まれない。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, _g) = open_core("t43c-tenant");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    // bob の行 3 は alice の更新で変わらない（0 行更新で違反も起きない）。
    ok(
        &core,
        "alice",
        &format!("UPDATE sv SET lang = 'en' WHERE id = 3 {OP} 'op-t-1'"),
    );
    let q = query(&core, "bob", "SELECT lang FROM docs WHERE id = 3 LIMIT 1");
    assert!(matches!(&q.rows[0].cells[0], Cell::Text(l) if l == "ja"));
    // bob 自身の違反はビュー名・値を含まない固定文言。
    let e = err(&core, "bob", &insert_sql("sv", 30, "en", 1, "op-t-2"));
    assert_eq!(e.wire_code(), "44000");
    assert_eq!(e.to_string(), "new row violates check option for view");
}

/// 明示トランザクション内の違反は文を失敗させ、ROLLBACK 後に痕跡が無い。
#[test]
fn violation_inside_explicit_transaction() {
    let (core, _g) = open_core("t43c-txn");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja' WITH CHECK OPTION",
    );
    let caller = ctx("alice");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut s,
        &mut txn,
        &insert_sql("sv", 10, "ja", 1, "op-x-1"),
    )
    .expect("good insert");
    let e = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            &insert_sql("sv", 11, "en", 1, "op-x-2"),
        )
        .expect_err("violating insert");
    assert_eq!(e.wire_code(), "44000");
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ids(&core, "alice"), vec![1, 2, 3]);
}
