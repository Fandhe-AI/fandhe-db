//! 書き出しのクォート・分類・内部キー変換の単体テスト（RECOVER-16 (1)。親: [`super`]）。
//! カタログ全体の往復は `tests/table_rls/logical_dump_schema.rs` が検査する。

use super::quote::{bare_ident, ident, literal, Namer};
use super::*;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use std::convert::Infallible;

type Err0 = SchemaDdlError<Infallible>;

fn unsupported_kind<T: std::fmt::Debug>(r: Result<T, Err0>) -> &'static str {
    match r {
        Err(SchemaDdlError::Unsupported(u)) => u.kind,
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn identifiers_are_always_double_quoted() {
    assert_eq!(ident::<Infallible>("items").expect("plain"), "\"items\"");
    assert_eq!(
        ident::<Infallible>("Mixed_9").expect("mixed"),
        "\"Mixed_9\""
    );
}

#[test]
fn identifiers_the_lexer_cannot_read_back_are_refused() {
    // 二重引用符・空白・記号・予約語・引用すると意味が変わる語は、連結せず拒否する。
    for bad in [
        "a\"b",
        "a b",
        "a-b",
        "",
        "a.b",
        "a;b",
        "select",
        "ORDER",
        "null",
        "Default",
        "true",
        "current_date",
        "a\0b",
        "1abc",
        "名前",
    ] {
        assert_eq!(
            unsupported_kind(ident::<Infallible>(bad)),
            "identifier",
            "{bad:?}"
        );
        assert!(bare_ident::<Infallible>(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn string_literals_double_single_quotes_and_refuse_nul() {
    assert_eq!(literal::<Infallible>("it's").expect("lit"), "'it''s'");
    assert_eq!(literal::<Infallible>("a\\b\"c").expect("lit"), "'a\\b\"c'");
    // 注入を試みる値は 1 個のリテラルに閉じ込められる。
    let evil = "x'); DROP TABLE t; --";
    let rendered = literal::<Infallible>(evil).expect("lit");
    assert_eq!(rendered, "'x''); DROP TABLE t; --'");
    assert!(unsupported_kind(literal::<Infallible>("a\0b")).starts_with("literal"));
}

fn namer() -> Namer {
    let mut n = Namer::default();
    n.schemas.insert(7, "app".to_string());
    n
}

#[test]
fn relation_keys_render_as_public_or_schema_qualified_names() {
    let n = namer();
    assert_eq!(
        n.qualified::<Infallible>("items").expect("ok"),
        Some("\"items\"".to_string())
    );
    assert_eq!(
        n.qualified::<Infallible>("#7.items").expect("ok"),
        Some("app.items".to_string())
    );
    // 選択したテナントの SCHEMA に無い内部 ID は他テナントのもの: 黙って対象外にする。
    assert_eq!(n.qualified::<Infallible>("#8.items").expect("ok"), None);
    assert!(unsupported_kind(n.qualified_required::<Infallible>("#8.items")) == "relation");
    assert!(n.qualified::<Infallible>("bad key").is_err());
}

#[test]
fn internal_keys_in_stored_bodies_become_qualified_names() {
    let n = namer();
    let out = n
        .externalize::<Infallible>("view", "SELECT a FROM \"#7.items\" WHERE b = '\"#7.x\"'")
        .expect("rewrite");
    // 文字列リテラルの中は書き換えない。
    assert_eq!(out, "SELECT a FROM app.items WHERE b = '\"#7.x\"'");
    assert_eq!(
        n.externalize::<Infallible>("view", "SELECT 1").expect("ok"),
        "SELECT 1"
    );
    // 他テナントの内部 ID・壊れたキーは戻せない。
    for bad in ["SELECT a FROM \"#9.items\"", "SELECT a FROM \"#7.items"] {
        assert!(n.externalize::<Infallible>("view", bad).is_err(), "{bad}");
    }
}

#[test]
fn storage_tables_are_classified_and_unknown_ones_fail_closed() {
    for known in [
        "catalog",
        "sequences",
        "user_rows/t",
        "user_uniq/t",
        "key_index/t/i",
        "matview_rows/m",
        "op_ledger",
    ] {
        assert!(classify_redb_table(known).is_some(), "{known}");
    }
    assert_eq!(
        classify_redb_table("catalog"),
        Some(RedbTableClass::SchemaCatalog)
    );
    assert_eq!(classify_redb_table("rows"), Some(RedbTableClass::NotSchema));
    assert_eq!(classify_redb_table("mystery_catalog"), None);
    // 先頭一致の接頭辞は区切り `/` まで含めて一致させる（`user_rowsX` は未知）。
    assert_eq!(classify_redb_table("user_rowsX"), None);
}

#[test]
fn an_unclassified_storage_table_fails_the_dump_without_emitting_anything() {
    let path = unique_db_path("ddl-export-unknown-table");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    {
        let w = storage.db().begin_write().expect("begin");
        {
            #[allow(clippy::disallowed_methods)]
            let mut t = w
                .open_table(redb::TableDefinition::<&str, u64>::new(
                    "future_catalog_kind",
                ))
                .expect("open");
            t.insert("k", 1).expect("insert");
        }
        w.commit().expect("commit");
    }
    let mut emitted = 0;
    let r = storage.export_schema_ddl(&SchemaDdlScope::shared_only(), |_| {
        emitted += 1;
        Ok::<(), Infallible>(())
    });
    assert_eq!(unsupported_kind(r), "catalog");
    assert_eq!(emitted, 0, "classification runs before the first statement");
}

#[test]
fn every_catalog_and_non_schema_table_name_is_unique() {
    for n in SCHEMA_CATALOG_TABLES {
        assert!(!NOT_SCHEMA_TABLES.contains(n), "{n} is in both lists");
    }
}

#[test]
fn scope_constructors_describe_the_selection() {
    let s = SchemaDdlScope::shared_and_tenant("t1");
    assert!(s.includes_shared());
    assert_eq!(s.tenant(), Some("t1"));
    let s = SchemaDdlScope::shared_only();
    assert!(s.includes_shared() && s.tenant().is_none());
    let s = SchemaDdlScope::tenant_only("t2");
    assert!(!s.includes_shared() && s.tenant() == Some("t2"));
}

#[test]
fn an_invalid_tenant_is_rejected_before_reading_the_catalog() {
    let path = unique_db_path("ddl-export-bad-tenant");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let r = storage.export_schema_ddl(&SchemaDdlScope::shared_and_tenant(""), |_| {
        Ok::<(), Infallible>(())
    });
    assert!(matches!(r, Err(SchemaDdlError::Catalog(_))));
}

#[test]
fn foreign_key_statement_lists_every_option_and_never_concatenates_raw_names() {
    use crate::catalog::{
        ForeignKeyDef, ForeignKeyDeferrability, ForeignKeyMatch, ReferentialAction,
    };
    let path = unique_db_path("ddl-export-fk-render");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let rt = storage.db().begin_read().expect("read");
    let scope = SchemaDdlScope::shared_only();
    let ctx = Ctx {
        rt: &rt,
        scope: &scope,
        namer: namer(),
    };
    let fk = ForeignKeyDef::new(
        vec!["pid".to_string()],
        "parents".to_string(),
        vec!["pk".to_string()],
        ReferentialAction::Cascade,
        ReferentialAction::SetNull,
    )
    .with_name("fk_child_parent".to_string())
    .with_options(
        ForeignKeyMatch::Full,
        ForeignKeyDeferrability::DeferrableInitiallyDeferred,
    );
    let sql = table::render_foreign_key::<Infallible>(&ctx, "#7.child", &fk).expect("render");
    assert_eq!(
        sql,
        "ALTER TABLE app.child ADD CONSTRAINT \"fk_child_parent\" FOREIGN KEY (\"pid\") \
         REFERENCES \"parents\" (\"pk\") MATCH FULL ON DELETE CASCADE ON UPDATE SET NULL \
         DEFERRABLE INITIALLY DEFERRED"
    );
    // 参照先が暗黙 id（旧方式の表）の外部キーは戻せない。
    let to_id = ForeignKeyDef::new(
        vec!["pid".to_string()],
        "parents".to_string(),
        vec!["id".to_string()],
        ReferentialAction::NoAction,
        ReferentialAction::NoAction,
    )
    .with_name("fk2".to_string());
    assert!(to_id.references_parent_id());
    assert_eq!(
        unsupported_kind(table::render_foreign_key::<Infallible>(
            &ctx, "child", &to_id
        )),
        "foreign key"
    );
}

fn dump_sql(s: &Storage, scope: &SchemaDdlScope) -> Result<Vec<String>, Err0> {
    let mut v = Vec::new();
    s.export_schema_ddl(scope, |st| {
        v.push(st.sql.to_string());
        Ok::<(), Infallible>(())
    })?;
    Ok(v)
}

/// カタログ表へ直接 1 行書く（SQL では SCHEMA 配下に作れない種別の行を再現する試験専用）。
fn inject(storage: &Storage, table: &str, key: &str, value: &[u8]) {
    let w = storage.db().begin_write().expect("begin");
    {
        #[allow(clippy::disallowed_methods)]
        let mut t = w
            .open_table(redb::TableDefinition::<&str, &[u8]>::new(table))
            .expect("open");
        t.insert(key, value).expect("insert");
    }
    w.commit().expect("commit");
}

#[test]
fn matviews_and_enums_are_scoped_by_owner_and_never_leak_across_tenants() {
    let path = unique_db_path("ddl-export-scoped-kinds");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage.create_schema("tenant-a", "a_s", false).expect("a");
    storage.create_schema("tenant-b", "b_s", false).expect("b");
    let id_a = storage
        .schema_id("tenant-a", "a_s")
        .expect("id")
        .expect("some");
    let id_b = storage
        .schema_id("tenant-b", "b_s")
        .expect("id")
        .expect("some");
    let scopes = [
        SchemaDdlScope::shared_only(),
        SchemaDdlScope::shared_and_tenant("tenant-a"),
        SchemaDdlScope::tenant_only("tenant-a"),
    ];
    let before: Vec<_> = scopes
        .iter()
        .map(|s| dump_sql(&storage, s).expect("baseline"))
        .collect();
    // テナント B にだけ、SCHEMA 配下の matview と ENUM（中身は読まれない）を置く。
    inject(&storage, "materialized_views", &format!("#{id_b}.mv"), b"x");
    inject(&storage, "enum_types", &format!("#{id_b}.en"), b"x");
    for (s, b) in scopes.iter().zip(&before) {
        assert_eq!(&dump_sql(&storage, s).expect("unchanged by tenant-b"), b);
    }
    // 持ち主の B のダンプは、黙って落とさず失敗する。
    assert_eq!(
        unsupported_kind(dump_sql(&storage, &SchemaDdlScope::tenant_only("tenant-b"))),
        "enum type"
    );
    // A 自身の SCHEMA 配下の matview・ENUM も黙って落とさない。
    inject(&storage, "materialized_views", &format!("#{id_a}.mv"), b"x");
    assert_eq!(
        unsupported_kind(dump_sql(&storage, &SchemaDdlScope::tenant_only("tenant-a"))),
        "materialized view"
    );
    inject(&storage, "enum_types", &format!("#{id_a}.en"), b"x");
    assert_eq!(
        unsupported_kind(dump_sql(&storage, &SchemaDdlScope::tenant_only("tenant-a"))),
        "enum type"
    );
    // 共有分だけの書き出しは、テナント固有の行に影響されない。
    dump_sql(&storage, &SchemaDdlScope::shared_only()).expect("shared only");
}

#[test]
fn dollar_quoting_never_closes_early() {
    for body in [
        "SELECT 1",
        "x$fandhe",
        "x$f",
        "a$fandhe$b",
        "$fandhe$",
        "SELECT '$fandhe1$'",
        "ends with dollar$",
        "$starts",
    ] {
        match access::dollar_quote::<Infallible>(body) {
            Ok(q) => {
                let tag_end = q[1..].find('$').expect("tag") + 2;
                let tag = &q[..tag_end];
                assert!(q.ends_with(tag) && q.starts_with(tag));
                // 開きタグの後で最初に現れるタグが、末尾の閉じタグそのものである。
                assert_eq!(
                    q[tag.len()..].find(tag),
                    Some(q.len() - 2 * tag.len()),
                    "{body:?}"
                );
                assert_eq!(&q[tag.len()..q.len() - tag.len()], body);
            }
            Err(e) => panic!("{e:?}"),
        }
    }
}

#[test]
fn numeric_defaults_must_parse_as_numbers() {
    for ok in ["5", "-5", "1.5", "1e3", "-2.5E-3", "+7"] {
        assert!(table::safe_number::<Infallible>(ok).is_ok(), "{ok}");
    }
    for bad in ["--1", "1-2", "e", "", "1;", "1e", "--", "5 -- x", "inf"] {
        assert!(table::safe_number::<Infallible>(bad).is_err(), "{bad}");
    }
}

#[test]
fn statement_debug_hides_the_sql_text() {
    let st = DdlStatement {
        section: DdlSection::PreData,
        kind: DdlObjectKind::Role,
        sql: "CREATE ROLE r PASSWORD 'SCRAM-SHA-256$secret'",
    };
    let shown = format!("{st:?}");
    assert!(
        !shown.contains("SCRAM") && !shown.contains("secret"),
        "{shown}"
    );
}
