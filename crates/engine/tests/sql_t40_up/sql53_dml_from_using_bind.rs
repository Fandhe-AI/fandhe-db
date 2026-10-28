//! `UPDATE ... FROM`／`DELETE ... USING` の parser と束縛（SQL-53 (b)・Issue #1754。親 #1753）の
//! 結合テスト。
//!
//! 契約: 構文・表の存在確認・列解決・型照合が `EngineCore` の入口（`parse_sql`・Describe・
//! autocommit／明示トランザクションの実行・拡張クエリの Parse）で一貫して働き、束縛を通った文も
//! 実行腕が `0A000` で止まる（副作用ゼロ。結合の実行は #1755、RLS と `RETURNING` は #1756、
//! 台帳の content_hash は #1757 の担当）。未対応の形は fail-closed（`42601`／`0A000`）で拒否する。
//! 既存の単一表 DML（`DELETE ... USING OPERATION_ID` を含む）は挙動を変えない。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str, require_where: bool) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
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
            ],
        ))
        .expect("create langs");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_require_where_for_dml(require_where);
    (core, path)
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
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
                "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', '{lang}') \
                 USING OPERATION_ID 'doc-{id}'"
            ),
        )
        .expect("seed doc");
    }
    run(
        core,
        "INSERT INTO langs (id, lang, label) VALUES (1, 'ja', 'Japanese') USING OPERATION_ID 'lang-1'",
    )
    .expect("seed lang");
}

fn doc_rows(core: &EngineCore) -> String {
    run(core, "SELECT id, lang FROM docs ORDER BY id LIMIT 10").expect("select docs")
}

fn code(r: Result<impl std::fmt::Debug, SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 束縛まで通る受理形（SQL-53 (b)）。
const ACCEPTED: &[&str] = &[
    "UPDATE docs SET lang = langs.lang FROM langs WHERE docs.lang = langs.lang",
    "UPDATE docs AS d SET lang = l.lang FROM langs l WHERE d.lang = l.lang",
    "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang AND langs.label = 'Japanese'",
    "UPDATE docs SET lang = 'x', n = 1 FROM langs WHERE docs.lang = langs.lang OR (docs.id = langs.id)",
    // 自己結合（別名で公開名を分ける）
    "UPDATE docs SET lang = d2.lang FROM docs AS d2 WHERE docs.id = d2.id",
    // 2 表以上
    "UPDATE docs SET lang = 'x' FROM langs a, langs b WHERE docs.lang = a.lang AND a.lang = b.lang",
    // 対象表の列だけを使う式
    "UPDATE docs SET n = n + 1 FROM langs WHERE docs.lang = langs.lang",
    // 対象表修飾つき列の式は両表に同名列があっても曖昧にならない（Issue #1754 レビュー対応）
    "UPDATE docs SET lang = upper(docs.lang) FROM langs WHERE docs.lang = langs.lang",
    "UPDATE docs SET n = docs.id + 1 FROM langs WHERE docs.lang = langs.lang",
    "UPDATE docs SET lang = upper(docs.lang) FROM docs AS d2 WHERE docs.id = d2.id",
    "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang USING OPERATION_ID 'op-u1'",
    "UPDATE docs SET lang = 'x' FROM langs USING OPERATION_ID NULL",
    "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang;",
    "DELETE FROM docs USING langs WHERE docs.lang = langs.lang",
    "DELETE FROM docs d USING langs AS l WHERE d.lang = l.lang",
    "DELETE FROM docs USING langs a, langs b WHERE docs.lang = a.lang AND a.lang = b.lang",
    "DELETE FROM docs USING docs AS d2 WHERE docs.id = d2.id",
    "DELETE FROM docs USING langs WHERE docs.lang = langs.lang USING OPERATION_ID 'op-d1'",
    "DELETE FROM docs USING langs USING OPERATION_ID 'op-d2'",
    "DELETE FROM docs USING langs",
    // 裸の bool 列 WHERE の後ろに DML の後続句が続く形（同上）
    "UPDATE docs SET lang = 'x' FROM langs WHERE langs.flag USING OPERATION_ID 'op-u3'",
    "UPDATE docs SET lang = 'x' FROM langs WHERE langs.flag;",
    "DELETE FROM docs USING langs WHERE langs.flag USING OPERATION_ID 'op-d3'",
    "DELETE FROM docs USING langs WHERE langs.flag;",
];

/// 受理形は `parse_sql` が専用 variant を返す。
#[test]
fn parse_returns_dedicated_variants() {
    let (core, path) = new_core("sql53-from-parse", false);
    let _g = CleanupGuard(path);
    for sql in ACCEPTED {
        let parsed = core
            .parse_sql(sql)
            .unwrap_or_else(|e| panic!("parse failed for {sql}: {e:?}"));
        let ok = if sql.starts_with("UPDATE") {
            matches!(parsed, ParsedSql::UpdateFrom(_))
        } else {
            matches!(parsed, ParsedSql::DeleteUsing(_))
        };
        assert!(ok, "unexpected variant for {sql}: {parsed:?}");
    }
}

/// 受理形は束縛を通った後に `0A000` で止まり、行・台帳に副作用がない（operation_id も消費しない）。
#[test]
fn accepted_forms_bind_then_stop_with_0a000_without_side_effects() {
    let (core, path) = new_core("sql53-from-exec", false);
    let _g = CleanupGuard(path);
    seed(&core);
    let before = doc_rows(&core);
    for sql in ACCEPTED {
        assert_eq!(code(run(&core, sql)), "0A000", "{sql}");
        assert_eq!(before, doc_rows(&core), "rows changed by {sql}");
    }
    // 拒否された文の operation_id は台帳に残らない（別の文が同じ ID で通常どおり実行できる）。
    for id in ["op-u1", "op-d1", "op-d2"] {
        run(
            &core,
            &format!("UPDATE docs SET n = 7 WHERE lang = 'en' USING OPERATION_ID '{id}'"),
        )
        .unwrap_or_else(|e| panic!("operation_id {id} should be unused: {e:?}"));
    }
}

/// 拒否形と期待コード。
#[test]
fn rejected_forms_fail_closed_with_expected_codes() {
    let (core, path) = new_core("sql53-from-reject", false);
    let _g = CleanupGuard(path);
    seed(&core);
    let cases: &[(&str, &str)] = &[
        // 非修飾列が 2 表に存在
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE lang = 'ja'",
            "42702",
        ),
        (
            "DELETE FROM docs USING langs WHERE lang = 'ja'",
            "42702",
        ),
        ("UPDATE docs SET n = 1 FROM langs WHERE id = langs.id", "42702"),
        // 修飾子が公開名と不一致（別名を付けた後の元の表名での修飾を含む）
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE nope.lang = 'ja'",
            "42P01",
        ),
        (
            "UPDATE docs AS d SET lang = 'x' FROM langs WHERE docs.lang = langs.lang",
            "42P01",
        ),
        (
            "DELETE FROM docs d USING langs WHERE docs.lang = langs.lang",
            "42P01",
        ),
        // 未知の列（42703）
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE langs.nope = 'ja'",
            "42703",
        ),
        (
            "UPDATE docs SET nope = 'x' FROM langs WHERE docs.lang = langs.lang",
            "42703",
        ),
        // SET 列参照の型不一致（text を integer 列へ）
        (
            "UPDATE docs SET n = langs.label FROM langs WHERE docs.lang = langs.lang",
            "42804",
        ),
        // SET 列参照が曖昧
        (
            "UPDATE docs SET lang = lang FROM langs WHERE docs.lang = langs.lang",
            "42702",
        ),
        // 疑似列・重複
        (
            "UPDATE docs SET id = 1 FROM langs WHERE docs.lang = langs.lang",
            "42601",
        ),
        (
            "UPDATE docs SET lang = 'a', lang = 'b' FROM langs WHERE docs.lang = langs.lang",
            "22000",
        ),
        // 公開名の重複
        (
            "UPDATE docs SET lang = 'x' FROM docs WHERE docs.lang = docs.lang",
            "42601",
        ),
        ("DELETE FROM docs USING docs WHERE docs.id = docs.id", "42601"),
        // FROM／USING 側の表が存在しない・対象表が存在しない
        (
            "UPDATE docs SET lang = 'x' FROM nope WHERE docs.lang = nope.lang",
            "42P01",
        ),
        (
            "DELETE FROM docs USING nope WHERE docs.lang = nope.lang",
            "42P01",
        ),
        (
            "UPDATE nope SET lang = 'x' FROM langs WHERE nope.lang = langs.lang",
            "42P01",
        ),
        // RETURNING 併用（#1756）
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang RETURNING id",
            "0A000",
        ),
        (
            "DELETE FROM docs USING langs WHERE docs.lang = langs.lang RETURNING id",
            "0A000",
        ),
        // PARTITIONED 併用
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang \
             USING OPERATION_ID 'p1' PARTITIONED",
            "42601",
        ),
        (
            "DELETE FROM docs USING langs WHERE docs.lang = langs.lang \
             USING OPERATION_ID 'p2' PARTITIONED",
            "42601",
        ),
        // WHERE の対象外構文（サブクエリ・IS NULL・数値リテラル・NOT）
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang IN (SELECT lang FROM langs LIMIT 1)",
            "42601",
        ),
        (
            "UPDATE docs SET lang = 'x' FROM langs WHERE langs.label IS NULL",
            "42601",
        ),
        (
            "DELETE FROM docs USING langs WHERE docs.n = 1",
            "42601",
        ),
        (
            "DELETE FROM docs USING langs WHERE NOT docs.lang = langs.lang",
            "42601",
        ),
        // SET 左辺の修飾・行値代入
        (
            "UPDATE docs SET docs.lang = 'x' FROM langs WHERE docs.lang = langs.lang",
            "42601",
        ),
        (
            "UPDATE docs SET (lang, n) = ('x', 1) FROM langs WHERE docs.lang = langs.lang",
            "0A000",
        ),
        // FROM 表の列を使う式
        (
            "UPDATE docs SET lang = upper(langs.lang) FROM langs WHERE docs.lang = langs.lang",
            "0A000",
        ),
        (
            "UPDATE docs SET lang = upper(label) FROM langs WHERE docs.lang = langs.lang",
            "0A000",
        ),
        // FROM 内の JOIN ... ON
        (
            "UPDATE docs SET lang = 'x' FROM langs JOIN langs b ON langs.lang = b.lang \
             WHERE docs.lang = langs.lang",
            "42601",
        ),
        // 別名付きで FROM／USING 無し
        (
            "UPDATE docs AS d SET lang = 'x' WHERE d.lang = 'en'",
            "42601",
        ),
        ("DELETE FROM docs d WHERE d.lang = 'en'", "42601"),
        // USING に operation_id という名前の表
        ("DELETE FROM docs USING operation_id", "42601"),
        // 参照数の超過（対象表 + 8 個）
        (
            "DELETE FROM docs USING langs a, langs b, langs c, langs d, langs e, langs f, \
             langs g, langs h",
            "54000",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(code(run(&core, sql)), *want, "{sql}");
    }
    // 拒否された文はいずれも行を変えない。
    assert_eq!(
        doc_rows(&core),
        run(&core, "SELECT id, lang FROM docs ORDER BY id LIMIT 10").expect("select")
    );
}

/// 非修飾で曖昧な式中の列は従来どおり `42702`（対象表修飾つきの列とは区別される）。
#[test]
fn unqualified_ambiguous_column_in_set_expr_is_still_42702() {
    let (core, path) = new_core("sql53-from-set-amb", false);
    let _g = CleanupGuard(path);
    seed(&core);
    assert_eq!(
        code(run(
            &core,
            "UPDATE docs SET lang = upper(lang) FROM langs WHERE docs.lang = langs.lang"
        )),
        "42702"
    );
}

/// `using` は予約語ではなく識別子。表名・列名が `using` の単一表 DELETE は FROM／USING 形へ
/// 誤ルーティングされない（Issue #1754 レビュー対応）。
#[test]
fn identifier_named_using_is_not_routed_to_delete_using() {
    // 表名が `using`（存在しない表）は単一表経路の 42P01（FROM／USING 経路の 42601 ではない）。
    let (core0, path0) = new_core("sql53-from-using-table", false);
    let _g0 = CleanupGuard(path0);
    assert_eq!(
        code(core0.parse_sql("DELETE FROM using WHERE id = 1")),
        "42P01"
    );
    let (core, path) = new_core("sql53-from-using-ident", false);
    let _g = CleanupGuard(path);
    let sql = "DELETE FROM docs WHERE using = 1";
    // 単一表 DELETE（述語形）として構文解析できる。FROM／USING 経路なら別 variant になる。
    let parsed = core.parse_sql(sql).expect("single-table delete parses");
    assert!(
        format!("{parsed:?}").starts_with("Delete(Predicate"),
        "{sql}: {parsed:?}"
    );
}

/// 既存の `DELETE ... USING OPERATION_ID`（`WHERE` 省略形）と単一表 DML は挙動を変えない。
#[test]
fn single_table_dml_is_unchanged() {
    let (core, path) = new_core("sql53-from-regress", false);
    let _g = CleanupGuard(path);
    seed(&core);
    run(
        &core,
        "UPDATE docs SET n = 5 WHERE lang = 'ja' USING OPERATION_ID 'r-1'",
    )
    .expect("single-table update");
    run(&core, "DELETE FROM docs USING OPERATION_ID 'r-2'").expect("delete all via op id");
    let out = doc_rows(&core);
    assert!(!out.contains("ja") && !out.contains("en"), "{out}");
    // 式中の `EXTRACT(... FROM ...)` や `IS DISTINCT FROM` は FROM 形と誤判定されない
    // （括弧内・DISTINCT 直後の FROM）。
    let (core, path) = new_core("sql53-from-regress2", false);
    let _g2 = CleanupGuard(path);
    seed(&core);
    run(
        &core,
        "UPDATE docs SET n = 1 WHERE n IS DISTINCT FROM 2 USING OPERATION_ID 'r-3'",
    )
    .map(|_| ())
    .unwrap_or_else(|e| assert_ne!(e.wire_code(), "0A000", "{e:?}"));
}

/// セッションを持たない非セッション入口は FROM／USING 形を従来どおり `42601` で拒否する。
#[test]
fn non_session_entry_points_still_reject() {
    let (core, path) = new_core("sql53-from-nonsession", false);
    let _g = CleanupGuard(path);
    seed(&core);
    for sql in [
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang",
    ] {
        let err = core
            .execute_update_sql(&ctx(), sql)
            .err()
            .or_else(|| core.execute_delete_sql(&ctx(), sql).err())
            .expect("rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

/// `search_path` が `public` を隠すセッションでは、FROM／USING 側の非修飾表も `42P01` のゲートに掛かる。
#[test]
fn search_path_gate_covers_from_and_using_relations() {
    let (core, path) = new_core("sql53-from-searchpath", false);
    let _g = CleanupGuard(path);
    seed(&core);
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(), &mut session, "SET search_path = tenant_b")
        .expect("set path");
    for sql in [
        "UPDATE public.docs SET lang = 'x' FROM langs WHERE docs.lang = langs.lang",
        "DELETE FROM public.docs USING langs WHERE docs.lang = langs.lang",
        "DELETE FROM public.docs USING public.langs, langs b WHERE docs.lang = b.lang",
        // 対象表の別名が式の語（case／when／is）でも USING 直後の非修飾表はゲートに掛かる
        "DELETE FROM public.docs AS case USING langs WHERE case.lang = langs.lang",
        "DELETE FROM public.docs AS when USING langs WHERE when.lang = langs.lang",
        "DELETE FROM public.docs AS is USING langs WHERE is.lang = langs.lang",
    ] {
        let err = core
            .execute_sql_in_session(&ctx(), &mut session, sql)
            .expect_err("gate");
        assert_eq!(err.wire_code(), "42P01", "{sql}");
    }
}

/// Describe は束縛エラーを実行と同じコードで返し、通った文は結果列なし。
#[test]
fn describe_reports_bind_errors_and_no_columns_for_valid_forms() {
    let (core, path) = new_core("sql53-from-describe", false);
    let _g = CleanupGuard(path);
    let session = SessionState::default();
    for sql in ACCEPTED {
        let parsed = core.parse_sql(sql).expect("parse");
        let meta = core
            .describe_parsed_in_session(&session, &parsed)
            .unwrap_or_else(|e| panic!("describe failed for {sql}: {e:?}"));
        assert!(meta.is_none(), "{sql}");
    }
    let parsed = core
        .parse_sql("UPDATE docs SET lang = 'x' FROM langs WHERE lang = 'ja'")
        .expect("parse");
    assert_eq!(
        code(core.describe_parsed_in_session(&session, &parsed)),
        "42702"
    );
}

/// 明示トランザクション内でも束縛の後に `0A000`（書き込みなし）。ROLLBACK で通常どおり抜けられる。
#[test]
fn explicit_transaction_binds_then_fails_with_0a000() {
    let (core, path) = new_core("sql53-from-txn", false);
    let _g = CleanupGuard(path);
    seed(&core);
    let before = doc_rows(&core);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &ctx(),
            &mut session,
            &mut txn,
            "UPDATE docs SET lang = 'x' FROM langs WHERE lang = 'ja'",
        )
        .expect_err("bind error");
    assert_eq!(err.wire_code(), "42702");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &ctx(),
            &mut session,
            &mut txn,
            "DELETE FROM docs USING langs WHERE docs.lang = langs.lang",
        )
        .expect_err("not supported");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, doc_rows(&core));
}

/// 拡張クエリの Parse は `$n` を含む FROM／USING 形を `42601` で拒否する。
#[test]
fn prepared_parse_rejects_bind_parameters() {
    let (core, path) = new_core("sql53-from-prepared", false);
    let _g = CleanupGuard(path);
    for sql in [
        "UPDATE docs SET lang = 'x' FROM langs WHERE docs.lang = $1",
        "DELETE FROM docs USING langs WHERE docs.lang = langs.lang AND langs.label = $1",
    ] {
        assert_eq!(code(core.parse_sql_prepared(sql)), "42601", "{sql}");
    }
}

/// opt-in の `--require-where-for-dml` は FROM／USING 形でも `WHERE` 省略を `42601` で拒否する。
#[test]
fn require_where_for_dml_applies_to_from_and_using_forms() {
    let (core, path) = new_core("sql53-from-require-where", true);
    let _g = CleanupGuard(path);
    for sql in [
        "UPDATE docs SET lang = 'x' FROM langs",
        "DELETE FROM docs USING langs",
    ] {
        assert_eq!(code(core.parse_sql(sql)), "42601", "{sql}");
    }
    // WHERE を書けば通常どおり受理される。
    core.parse_sql("DELETE FROM docs USING langs WHERE docs.lang = langs.lang")
        .expect("with WHERE");
}

/// ビューが関与する FROM／USING 形は `0A000`。
#[test]
fn view_relations_are_rejected_with_0a000() {
    let (core, path) = new_core("sql53-from-view", false);
    let _g = CleanupGuard(path);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE VIEW v AS SELECT id, lang FROM langs",
    )
    .expect("create view");
    for sql in [
        "UPDATE docs SET lang = 'x' FROM v WHERE docs.id = v.id",
        "DELETE FROM docs USING v WHERE docs.id = v.id",
        "DELETE FROM v USING langs WHERE v.id = langs.id",
    ] {
        assert_eq!(code(core.parse_sql(sql)), "0A000", "{sql}");
    }
}

/// SET 右辺が列名 `distinct`（引用形）でも FROM 形（`UpdateFrom`）として解析される
/// （単一表パーサへ落ちた `42601` ではない。Issue #1754 レビュー対応）。
#[test]
fn quoted_distinct_column_still_routes_to_update_from() {
    let (core, path) = new_core("sql53-from-distinct", false);
    let _g = CleanupGuard(path);
    let parsed = core
        .parse_sql("UPDATE docs SET lang = \"distinct\" FROM langs WHERE docs.lang = langs.lang")
        .expect("update from parses");
    assert!(matches!(parsed, ParsedSql::UpdateFrom(_)), "{parsed:?}");
}

/// WHERE 内の列名 `using` は USING 句ではない。`using` 列を持つ表への単一表 DELETE が、
/// 修飾表名＋`search_path` 下でも従来どおり通る（Issue #1754 レビュー対応）。
#[test]
fn where_clause_using_column_is_not_a_using_clause() {
    let (core, path) = new_core("sql53-using-col", false);
    let _g = CleanupGuard(path);
    let parsed = core
        .parse_sql("DELETE FROM public.docs WHERE lang = 'x' OR using IN ('y')")
        .map(|p| format!("{p:?}"));
    if let Ok(p) = &parsed {
        assert!(!p.contains("DeleteUsing"), "{p}");
    }
    if let Err(e) = &parsed {
        assert!(!format!("{e:?}").contains("42P01"), "{e:?}");
    }
}
