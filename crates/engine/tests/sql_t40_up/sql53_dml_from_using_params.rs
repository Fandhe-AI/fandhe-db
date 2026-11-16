//! `UPDATE ... FROM`／`DELETE ... USING` の拡張クエリ `$n` と公開名重複の SQLSTATE
//! （SQL-53 (b)・RLS-10・RECOVER-10・RECOVER-11・Issue #3297。親 #3290）の結合テスト。
//!
//! 契約: Parse（位置規則・型推論・Describe 用ダミー値）→ Bind（置換後の再解析）→ 実行の経路が、
//! 同じ値をリテラルで書いた簡易クエリと同じ結果・同じ `wire_code` になる。結合 WHERE が受理しない
//! 数値位置は `42601` で fail-closed。結合 DML の公開名重複は PG と同じ `42712`。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::params::PreparedParamType;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("c", ColumnType::Integer, true),
                ColumnDef::new("d", ColumnType::Date, true),
            ],
        ))
        .expect("create docs");
    storage
        .create_table(&TableSchema::new(
            "langs",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("label", ColumnType::Text, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("c", ColumnType::Text, true),
                ColumnDef::new("d", ColumnType::Date, true),
            ],
        ))
        .expect("create langs");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ctx() -> PolicyContext {
    ctx_for("tenant-a")
}

fn run(core: &EngineCore, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn seed(core: &EngineCore) {
    for (id, lang) in [(1, "ja"), (2, "en")] {
        run(
            core,
            &format!(
                "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[0.{id},0.1]', '{lang}', {id}) \
                 USING OPERATION_ID 'doc-{id}'"
            ),
        )
        .expect("seed doc");
    }
    run(
        core,
        "INSERT INTO langs (id, lang, label, flag) VALUES (1, 'ja', 'Japanese', true) USING OPERATION_ID 'lang-1'",
    )
    .expect("seed lang");
}

fn doc_rows(core: &EngineCore) -> String {
    run(core, "SELECT id, lang, n FROM docs ORDER BY id LIMIT 10").expect("select docs")
}

fn code<T: std::fmt::Debug>(r: Result<T, SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// Parse → Bind → 実行（autocommit）。値は UTF-8 テキストで渡す。
fn exec_bound(core: &EngineCore, sql: &str, values: &[&str]) -> Result<String, SqlSurfaceError> {
    let prepared = core.parse_sql_prepared(sql)?;
    let vals: Vec<Option<Vec<u8>>> = values.iter().map(|v| Some(v.as_bytes().to_vec())).collect();
    let parsed = core.bind_prepared(&prepared, &vals)?;
    core.execute_parsed_in_session(&ctx(), &mut SessionState::default(), &parsed)
        .map(|o| format!("{o:?}"))
}

/// 拡張クエリと、同じ値を埋めた簡易クエリが、同じ結果（出力・行の状態）になること。
fn assert_same_as_literal(label: &str, ext_sql: &str, values: &[&str], literal_sql: &str) {
    let (a, pa) = new_core(&format!("{label}-ext"));
    let _ga = CleanupGuard(pa);
    let (b, pb) = new_core(&format!("{label}-lit"));
    let _gb = CleanupGuard(pb);
    seed(&a);
    seed(&b);
    let ra = exec_bound(&a, ext_sql, values).expect(ext_sql);
    let rb = run(&b, literal_sql).expect(literal_sql);
    assert_eq!(ra, rb, "{ext_sql}");
    assert_eq!(doc_rows(&a), doc_rows(&b), "{ext_sql}");
}

#[test]
fn set_and_where_params_match_literal_forms() {
    assert_same_as_literal(
        "p3297-set",
        "UPDATE docs SET lang = $1 FROM langs WHERE docs.lang = langs.lang",
        &["zz"],
        "UPDATE docs SET lang = 'zz' FROM langs WHERE docs.lang = langs.lang",
    );
    assert_same_as_literal(
        "p3297-setint",
        "UPDATE docs SET n = $1 FROM langs WHERE docs.lang = langs.lang",
        &["77"],
        "UPDATE docs SET n = 77 FROM langs WHERE docs.lang = langs.lang",
    );
    assert_same_as_literal(
        "p3297-setexpr",
        "UPDATE docs SET n = docs.n + $1 FROM langs WHERE docs.lang = langs.lang",
        &["5"],
        "UPDATE docs SET n = docs.n + 5 FROM langs WHERE docs.lang = langs.lang",
    );
    assert_same_as_literal(
        "p3297-alias",
        "UPDATE docs AS d SET n = d.n + $1 FROM langs l WHERE d.lang = l.lang",
        &["5"],
        "UPDATE docs AS d SET n = d.n + 5 FROM langs l WHERE d.lang = l.lang",
    );
    assert_same_as_literal(
        "p3297-where",
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label = $1",
        &["Japanese"],
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label = 'Japanese'",
    );
    assert_same_as_literal(
        "p3297-bool",
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.flag = $1",
        &["true"],
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.flag = true",
    );
    assert_same_as_literal(
        "p3297-both",
        "UPDATE docs SET lang = $1 FROM langs WHERE docs.lang = langs.lang AND langs.label = $1",
        &["Japanese"],
        "UPDATE docs SET lang = 'Japanese' FROM langs WHERE docs.lang = langs.lang AND langs.label = 'Japanese'",
    );
    assert_same_as_literal(
        "p3297-del",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = $1",
        &["Japanese"],
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = 'Japanese'",
    );
    assert_same_as_literal(
        "p3297-delalias",
        "DELETE FROM docs d USING langs AS l WHERE d.lang = l.lang AND l.label = $1;",
        &["Japanese"],
        "DELETE FROM docs d USING langs AS l WHERE d.lang = l.lang AND l.label = 'Japanese';",
    );
}

/// 公開名は大文字小文字を区別して完全一致で解決する（束縛段と同じ）。別名 `x` と `X` は別の表を指し、
/// `X.c`（FROM 側 Text）の型は先頭の `x`（対象表 Integer）に引き寄せられない（Issue #3297）。
#[test]
fn where_param_type_uses_exact_case_qualifier() {
    let (core, path) = new_core("p3297-exact-case");
    let _g = CleanupGuard(path);
    let prepared = core
        .parse_sql_prepared("DELETE FROM docs x USING langs X WHERE x.lang = X.lang AND X.c = $1")
        .expect("parse");
    let t = prepared.param_types().first().cloned().expect("slot");
    assert!(
        matches!(t, PreparedParamType::Text | PreparedParamType::Column(_)),
        "{t:?}"
    );
}

/// WHERE の修飾付き列の型は修飾子の表で引く（対象表の同名列 `c` は Integer、FROM 側は Text）。
#[test]
fn where_param_type_follows_the_qualifier() {
    let (core, path) = new_core("p3297-qual");
    let _g = CleanupGuard(path);
    let ty = |sql: &str| {
        core.parse_sql_prepared(sql)
            .expect(sql)
            .param_types()
            .first()
            .cloned()
            .expect("slot")
    };
    let t =
        ty("UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.c = $1");
    assert!(
        matches!(t, PreparedParamType::Text | PreparedParamType::Column(_)),
        "{t:?}"
    );
    let t = ty("DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.c = $1");
    assert!(
        matches!(t, PreparedParamType::Text | PreparedParamType::Column(_)),
        "{t:?}"
    );
    // 対象表側の整数列は結合 WHERE が数値を受理しないため Parse で拒否される。
    assert_eq!(
        code(core.parse_sql_prepared(
            "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND docs.c = $1"
        )),
        "42601"
    );
}

#[test]
fn unsupported_positions_are_rejected() {
    let (core, path) = new_core("p3297-rej");
    let _g = CleanupGuard(path);
    for sql in [
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label IN ($1, 'a')",
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label LIKE $1",
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang RETURNING $1",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label IN ($1)",
        // 整数列の WHERE は結合 WHERE が数値を受理しないため（簡易クエリと同じ拒否）。
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND docs.n = $1",
    ] {
        assert_eq!(code(core.parse_sql_prepared(sql)), "42601", "{sql}");
    }
}

#[test]
fn operation_id_param_replays_and_conflicts() {
    let (core, path) = new_core("p3297-opid");
    let _g = CleanupGuard(path);
    seed(&core);
    let sql =
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang USING OPERATION_ID $1";
    exec_bound(&core, sql, &["op-1"]).expect("first");
    assert_eq!(code(exec_bound(&core, sql, &["op-1"])), "23505");
    let sql2 =
        "UPDATE docs SET lang = 'y' FROM langs WHERE docs.lang = langs.lang USING OPERATION_ID $1";
    assert_eq!(code(exec_bound(&core, sql2, &["op-1"])), "22023");
}

#[test]
fn explain_prefix_follows_existing_rules() {
    let (core, path) = new_core("p3297-explain");
    let _g = CleanupGuard(path);
    core.parse_sql_prepared(
        "EXPLAIN UPDATE docs SET lang = 'z' FROM langs WHERE docs.lang = langs.lang AND langs.label = $1",
    )
    .expect("where param accepted");
    for sql in [
        "EXPLAIN UPDATE docs SET lang = $1 FROM langs WHERE docs.lang = langs.lang",
        "EXPLAIN ANALYZE DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = $1",
        "EXPLAIN (ANALYZE) UPDATE docs SET lang = $1 FROM langs WHERE docs.lang = langs.lang",
    ] {
        assert_eq!(code(core.parse_sql_prepared(sql)), "42601", "{sql}");
    }
}

#[test]
fn describe_dummy_does_not_break_parse() {
    let (core, path) = new_core("p3297-describe");
    let _g = CleanupGuard(path);
    core.parse_sql_prepared(
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = $1",
    )
    .expect("parse");
}

/// Describe は `$n` 由来のダミー値（`'0'`）で DATE 列の検証に失敗しない。
#[test]
fn describe_with_date_where_param() {
    let (core, path) = new_core("p3297-describe-date");
    let _g = CleanupGuard(path);
    seed(&core);
    for sql in [
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.d = $1",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.d = $1",
    ] {
        let prepared = core.parse_sql_prepared(sql).expect(sql);
        let ty = prepared.param_types().first().cloned().expect("slot");
        assert!(matches!(ty, PreparedParamType::Column(_)), "{ty:?}");
        core.describe_prepared_in_session(&SessionState::default(), &prepared)
            .expect("describe");
        exec_bound(&core, sql, &["2024-01-02"]).expect(sql);
    }
}

/// SET の `$n` 由来のダミー値は、DATE 列でも Describe の値検証に落ちない（Bind で実値を検証する）。
/// 実リテラルの不正値は従来どおり拒否する。
#[test]
fn describe_with_date_set_param() {
    let (core, path) = new_core("p3297-describe-set-date");
    let _g = CleanupGuard(path);
    seed(&core);
    let sql = "UPDATE docs SET d = $1 FROM langs WHERE docs.lang = langs.lang";
    let prepared = core.parse_sql_prepared(sql).expect(sql);
    core.describe_prepared_in_session(&SessionState::default(), &prepared)
        .expect("describe");
    exec_bound(&core, sql, &["2024-01-02"]).expect(sql);
    // 実値の不正は Bind で拒否される。
    assert!(exec_bound(&core, sql, &["not-a-date"]).is_err());
    // 実リテラルの検証は維持する。
    assert!(run(
        &core,
        "UPDATE docs SET d = 'not-a-date' FROM langs WHERE docs.lang = langs.lang"
    )
    .is_err());
}

/// JOIN 連鎖の後続表の列型も `$n` の型推論に使う（`b.flag` は BOOLEAN）。畳み込み source
/// （2 表以上・JOIN）のソースのみの連言分でも Describe が DATE 列で落ちない。
#[test]
fn join_chain_param_types_and_folded_source_describe() {
    let (core, path) = new_core("p3297-join-chain");
    let _g = CleanupGuard(path);
    seed(&core);
    let sql = "DELETE FROM docs USING langs a JOIN langs b ON a.lang = b.lang \
               WHERE docs.lang = a.lang AND b.flag = $1";
    let prepared = core.parse_sql_prepared(sql).expect(sql);
    let ty = prepared.param_types().first().cloned().expect("slot");
    assert!(matches!(ty, PreparedParamType::Column(_)), "{ty:?}");
    exec_bound(&core, sql, &["true"]).expect(sql);
    for sql in [
        "DELETE FROM docs USING langs a JOIN langs b ON a.lang = b.lang \
         WHERE docs.lang = a.lang AND b.d = $1",
        "UPDATE docs SET lang = 'x' FROM langs a, langs b \
         WHERE docs.lang = a.lang AND a.lang = b.lang AND b.d = $1",
    ] {
        let prepared = core.parse_sql_prepared(sql).expect(sql);
        core.describe_prepared_in_session(&SessionState::default(), &prepared)
            .expect("describe");
        exec_bound(&core, sql, &["2024-01-02"]).expect(sql);
    }
}

/// 結合 DML の公開名重複は `42712`（PG の `duplicate_alias`）。副作用なし。SELECT の JOIN・派生表の
/// 別名重複は据え置きの `42601`（他の結合文のコードを変えない固定）。
#[test]
fn duplicate_exposed_names_are_42712_for_joined_dml_only() {
    let (core, path) = new_core("p3297-dup");
    let _g = CleanupGuard(path);
    seed(&core);
    let before = doc_rows(&core);
    for sql in [
        "UPDATE docs SET lang = 'x' FROM docs WHERE docs.id = docs.id",
        "UPDATE docs AS x SET lang = 'x' FROM langs AS x WHERE x.lang = x.lang",
        "UPDATE docs SET lang = 'x' FROM langs, langs WHERE docs.lang = langs.lang",
        "UPDATE docs SET lang = 'x' FROM langs AS docs WHERE docs.lang = docs.lang",
        "DELETE FROM docs USING docs WHERE docs.id = docs.id",
        "DELETE FROM docs d USING langs d WHERE d.lang = d.lang",
        "DELETE FROM docs USING langs a, langs a WHERE docs.lang = a.lang",
    ] {
        let err = run(&core, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42712", "{sql}");
        let msg = err.to_string();
        assert!(!msg.contains('#'), "internal key leaked: {msg}");
        assert_eq!(code(core.parse_sql(sql)), "42712", "{sql}");
    }
    assert_eq!(before, doc_rows(&core));
    // 他の結合文は従来どおり `42601`。
    for sql in [
        "SELECT * FROM docs, docs LIMIT 1",
        "SELECT * FROM docs JOIN docs ON docs.id = docs.id LIMIT 1",
        "SELECT * FROM docs a JOIN langs a ON a.id = a.id LIMIT 1",
    ] {
        assert_eq!(code(run(&core, sql)), "42601", "{sql}");
    }
}

fn exec_bound_as(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    values: &[&str],
) -> Result<String, SqlSurfaceError> {
    let prepared = core.parse_sql_prepared(sql)?;
    let vals: Vec<Option<Vec<u8>>> = values.iter().map(|v| Some(v.as_bytes().to_vec())).collect();
    let parsed = core.bind_prepared(&prepared, &vals)?;
    core.execute_parsed_in_session(&ctx_for(tenant), &mut SessionState::default(), &parsed)
        .map(|o| format!("{o:?}"))
}

/// RLS-10（P0）: `$n` に他テナントの Private 行にだけ一致する値を束縛しても、結合に入らず件数も
/// 漏れない。拡張クエリと簡易クエリで結果が一致し、他テナントの行は変更されない。
#[test]
fn bound_values_never_reach_other_tenant_rows() {
    let (core, path) = new_core("p3297-rls");
    let _g = CleanupGuard(path);
    seed(&core);
    let bob = ctx_for("bob");
    run_as(
        &core,
        &bob,
        "INSERT INTO docs (id, embedding, lang, n) VALUES (50, '[0.5,0.1]', 'ja', 500) USING OPERATION_ID 'b-doc'",
    );
    run_as(
        &core,
        &bob,
        "INSERT INTO langs (id, lang, label) VALUES (50, 'ja', 'BobOnly') USING OPERATION_ID 'b-lang'",
    );
    let before = doc_rows(&core);
    // bob の source 行にだけ一致する値: alice からは 0 件（件数オラクルにならない）。
    let upd =
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label = $1";
    assert_eq!(
        exec_bound(&core, upd, &["BobOnly"]).expect("update"),
        run(
            &core,
            "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label = 'BobOnly'"
        )
        .expect("literal"),
    );
    assert_eq!(before, doc_rows(&core));
    let del = "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = $1";
    exec_bound(&core, del, &["BobOnly"]).expect("delete");
    assert_eq!(before, doc_rows(&core));
    // bob 自身は自分の行を更新できる（alice の行は変わらない）。
    let out = exec_bound_as(&core, "bob", upd, &["BobOnly"]).expect("bob update");
    assert!(out.contains("rows_affected: 1"), "{out}");
    assert_eq!(before, doc_rows(&core));
}

fn run_as(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .expect(sql);
}

/// Describe 用ダミー値の省略は `$n` 由来の位置だけ。同じ文の実リテラルの不正値は Describe で検出する。
#[test]
fn describe_still_validates_real_literals() {
    let (core, path) = new_core("p3297-describe-real");
    let _g = CleanupGuard(path);
    // `langs.d = $1`（ダミー）は省略され、実リテラル `langs.d = 'bad'` は DATE として検証される。
    let sql = "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.d = 'not-a-date' AND langs.label = $1";
    let err = core
        .parse_sql_prepared(sql)
        .and_then(|p| core.describe_prepared_in_session(&SessionState::default(), &p));
    assert_eq!(code(err), "22007");
}

/// 明示トランザクション上の Parse／Bind／Execute（COMMIT で永続化・ROLLBACK で巻き戻る）。
#[test]
fn bound_statements_in_explicit_transactions() {
    let (core, path) = new_core("p3297-txn");
    let _g = CleanupGuard(path);
    seed(&core);
    let before = doc_rows(&core);
    let sql = "UPDATE docs SET lang = $1 FROM langs WHERE docs.lang = langs.lang";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    let parsed = core
        .bind_prepared(&prepared, &[Some(b"zz".to_vec())])
        .expect("bind");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_parsed_in_txn(&ctx(), &mut session, &mut txn, &parsed)
        .expect("execute");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, doc_rows(&core));
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_parsed_in_txn(&ctx(), &mut session, &mut txn, &parsed)
        .expect("execute");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert!(doc_rows(&core).contains("zz"), "{}", doc_rows(&core));
}
