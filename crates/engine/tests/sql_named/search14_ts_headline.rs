//! `ts_headline`（SEARCH-14 ポインタ: `docs/spec/04-behavior/search.md`。Issue #2125）の SQL 結合テスト。
//!
//! 固定する契約（詳細は `docs/design/ts-headline-limits-cli.md`）:
//!
//! - ASCII の英字・空白・基本的な句読点だけのコーパスでは、PostgreSQL 17 の
//!   `ts_headline('simple', ...)` と出力が一致する（エスケープなし）。各期待値の直上に、生成に使った
//!   SQL の形をコメントで残してある（CI は PostgreSQL に依存しない）
//! - CJK では一致箇所・断片の境界が文字境界に揃う。語数は文字単位
//! - 未知のオプション・不正な値は `22023`、書式エラーは `42601`、上限超過は切り詰めず `54000`
//! - 引数のどれかが NULL なら NULL。`ts_headline` は組み込み名として予約され、式索引には置けない
//! - RLS を暗黙適用した後の可視行だけが投影される（網羅的な越境検証は #2126）

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

fn open(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (guard, core)
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    exec_as(core, "alice", sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

/// SQL 文字列リテラル（単一引用符を二重化）。
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 1 行 1 列の結果を `Option<String>` で返す（NULL は `None`）。
fn one(core: &EngineCore, sql: &str) -> Option<String> {
    let SqlOutcome::Query(q) = exec(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) else {
        panic!("query expected: {sql}");
    };
    assert_eq!(q.rows.len(), 1, "{sql}");
    match q.rows.first().and_then(|r| r.cells.first()) {
        Some(Cell::Text(s)) => Some(s.clone()),
        Some(Cell::Null) => None,
        other => panic!("unexpected cell {other:?} for {sql}"),
    }
}

fn call_sql(doc: &str, query: &str, opts: &str) -> String {
    if opts.is_empty() {
        format!("SELECT ts_headline({}, {})", lit(doc), lit(query))
    } else {
        format!(
            "SELECT ts_headline({}, {}, {})",
            lit(doc),
            lit(query),
            lit(opts)
        )
    }
}

fn headline(core: &EngineCore, doc: &str, query: &str, opts: &str) -> String {
    let sql = call_sql(doc, query, opts);
    one(core, &sql).unwrap_or_else(|| panic!("unexpected NULL for {sql}"))
}

const LONG: &str = "Rust is a systems language. It runs fast, prevents segfaults, and guarantees thread safety. Many teams adopt Rust for databases, networking, and embedded work. A vector database stores embeddings and answers nearest neighbor queries. Sparse search finds exact words, while dense search finds similar meaning. Hybrid search combines both, and highlight shows where the words matched.";

/// `(本文, 検索語, オプション, 期待値)`。期待値は PostgreSQL 17 の
/// `ts_headline('simple', 本文, to_tsquery('simple', 'w1 | w2 ...'), オプション)` の出力。
const GOLDEN: &[(&str, &str, &str, &str)] = &[
    // SELECT ts_headline('simple', doc, to_tsquery('simple', 'fox'));
    (
        "the quick brown fox",
        "fox",
        "",
        "the quick brown <b>fox</b>",
    ),
    // 一致語ごとに個別に囲む。
    (
        "foo foo bar",
        "foo",
        "",
        "<b>foo</b> <b>foo</b> bar",
    ),
    // 大小文字を区別せず、本文の綴りをそのまま出す。
    (
        "The Quick brown fox jumps over the lazy dog",
        "FOX dog",
        "",
        "The Quick brown <b>fox</b> jumps over the lazy <b>dog</b>",
    ),
    // StartSel の二重引用符内で "" が " になる。
    (
        "the quick brown fox",
        "fox",
        r#"StartSel="<em class=""x"">", StopSel=</em>"#,
        r#"the quick brown <em class="x">fox</em>"#,
    ),
    (
        "the quick brown fox",
        "quick",
        "StartSel='[[', StopSel=']]'",
        "the [[quick]] brown fox",
    ),
    // MaxWords=8, MinWords=4
    (
        LONG,
        "database",
        "MaxWords=8, MinWords=4",
        "<b>database</b> stores embeddings and answers",
    ),
    (
        LONG,
        "sparse dense",
        "MaxWords=12, MinWords=5, ShortWord=4",
        "<b>Sparse</b> search finds exact words",
    ),
    (
        LONG,
        "highlight",
        "ShortWord=0",
        "search finds similar meaning. Hybrid search combines both, and <b>highlight</b> shows where the words matched.",
    ),
    // HighlightAll が真なら MinWords >= MaxWords でも検査せず、本文全体を出す。
    (
        LONG,
        "rust",
        "HighlightAll=true, MinWords=50, MaxWords=10",
        "<b>Rust</b> is a systems language. It runs fast, prevents segfaults, and guarantees thread safety. Many teams adopt <b>Rust</b> for databases, networking, and embedded work. A vector database stores embeddings and answers nearest neighbor queries. Sparse search finds exact words, while dense search finds similar meaning. Hybrid search combines both, and highlight shows where the words matched.",
    ),
    (
        LONG,
        "rust search",
        "MaxFragments=2, MaxWords=6, MinWords=3",
        "<b>Rust</b> is a systems language ... teams adopt <b>Rust</b> for databases, networking",
    ),
    (
        LONG,
        "rust search words",
        "MaxFragments=3, MaxWords=5, MinWords=2, FragmentDelimiter=' // '",
        "<b>Rust</b> is a systems language // teams adopt <b>Rust</b> for databases // queries. Sparse <b>search</b> finds exact",
    ),
    // 一致なしは先頭から MinWords 語。
    (
        LONG,
        "nothing",
        "MinWords=5, MaxWords=9",
        "Rust is a systems language",
    ),
    // 空の検索語も先頭から MinWords 語。
    (LONG, "", "MinWords=3, MaxWords=9", "Rust is a"),
    (
        "Alpha, beta; gamma: delta! epsilon? zeta. eta",
        "gamma",
        "MaxWords=4, MinWords=2, ShortWord=1",
        "<b>gamma</b>: delta",
    ),
    // 同じオプションは後勝ち。
    (
        LONG,
        "meaning",
        "MaxWords=30, MaxWords=7, MinWords=2",
        "<b>meaning</b>. Hybrid",
    ),
    (
        LONG,
        "words",
        "MaxFragments=1, MaxWords=6, MinWords=2",
        "finds exact <b>words</b>, while dense search",
    ),
    // MaxFragments != 0 のとき HighlightAll は選択に効かない。
    (
        LONG,
        "vector hybrid",
        "HighlightAll=yes, MaxFragments=2, MaxWords=5, MinWords=2",
        "work. A <b>vector</b> database stores ... similar meaning. <b>Hybrid</b> search combines",
    ),
];

/// AC1: オプション指定ごとの出力が PostgreSQL 17 の `ts_headline` と同じ（エスケープなし）。
#[test]
fn golden_matches_postgresql_17() {
    let (_g, core) = open("search14-golden");
    for (doc, query, opts, expected) in GOLDEN {
        assert_eq!(
            headline(&core, doc, query, opts),
            *expected,
            "doc={doc:?} query={query:?} opts={opts:?}"
        );
    }
}

/// HTML などはエスケープしない（PostgreSQL と同じ）。表示側の責務。
#[test]
fn html_is_not_escaped() {
    let (_g, core) = open("search14-noescape");
    assert_eq!(
        headline(&core, "<script>alert(1)</script> fox & hound", "fox", ""),
        "<script>alert(1)</script> <b>fox</b> & hound"
    );
}

/// 本文のカラム値に対しても動き、他の式と組み合わせられる。
#[test]
fn works_on_column_values() {
    let (_g, core) = open("search14-column");
    ok(
        &core,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    ok(
        &core,
        "INSERT INTO docs (id, body) VALUES (1, 'the quick brown fox')",
    );
    assert_eq!(
        one(
            &core,
            "SELECT ts_headline(body, 'quick', 'StartSel=<<, StopSel=>>') FROM docs"
        ),
        Some("the <<quick>> brown fox".to_string())
    );
    assert_eq!(
        one(&core, "SELECT upper(ts_headline(body, 'fox')) FROM docs"),
        Some("THE QUICK BROWN <B>FOX</B>".to_string())
    );
}

/// 引数のどれかが NULL なら NULL（STRICT）。
#[test]
fn null_arguments_propagate() {
    let (_g, core) = open("search14-null");
    for sql in [
        "SELECT ts_headline(NULL, 'fox')",
        "SELECT ts_headline('a fox', NULL)",
        "SELECT ts_headline('a fox', 'fox', NULL)",
        "SELECT ts_headline(NULL, NULL, NULL)",
    ] {
        assert_eq!(one(&core, sql), None, "{sql}");
    }
}

/// CJK: 連続する一致は 1 組で囲み、助詞は語として数える。
#[test]
fn cjk_matches_are_wrapped_on_char_boundaries() {
    let (_g, core) = open("search14-cjk");
    assert_eq!(
        headline(&core, "東京都に行く", "東京都", ""),
        "<b>東京都</b>に行く"
    );
    assert_eq!(
        headline(&core, "Rust で東京タワーへ行く", "タワー", ""),
        "Rust で東京<b>タワー</b>へ行く"
    );
}

/// CJK: MaxWords は文字数で効き、どのオプションでも出力が文字境界で切れる。
#[test]
fn cjk_word_counts_are_in_characters() {
    let (_g, core) = open("search14-cjk-words");
    let doc = "東京都は日本の首都です。大阪府は西日本の中心です。京都府は古い都です。";
    for (query, opts) in [
        ("大阪府", "MaxWords=6, MinWords=2, ShortWord=0"),
        ("京都府", "MaxWords=4, MinWords=2, ShortWord=0"),
        (
            "西日本",
            "MaxFragments=2, MaxWords=5, MinWords=2, ShortWord=0",
        ),
        (
            "首都 中心",
            "MaxFragments=3, MaxWords=3, MinWords=1, ShortWord=0",
        ),
    ] {
        let out = headline(&core, doc, query, opts);
        let plain = out.replace("<b>", "").replace("</b>", "");
        for part in plain.split(" ... ").filter(|p| !p.is_empty()) {
            assert!(
                doc.contains(part),
                "{part:?} must be a substring of the body"
            );
        }
        assert!(out.contains("<b>"), "{query:?} {opts:?} -> {out:?}");
    }
    // MinWords=5 まで文字単位で伸びる（一致の 3 文字に隣の 2 文字が加わる）。
    let out = headline(&core, doc, "大阪府", "MaxWords=8, MinWords=5, ShortWord=0");
    assert_eq!(
        out.replace("<b>", "").replace("</b>", "").chars().count(),
        5,
        "{out}"
    );
}

/// 22023（未知・不正）／42601（書式）。文言に入力の値を含めない。
#[test]
fn option_errors() {
    let (_g, core) = open("search14-errors");
    let cases: &[(&str, &str)] = &[
        ("Bogus=1", "22023"),
        ("MaxWords=abc", "22023"),
        ("MaxWords=1.5", "22023"),
        ("MaxWords=2147483648", "22023"),
        ("MaxWords=-1", "22023"),
        ("MinWords=0", "22023"),
        ("MinWords=40", "22023"),
        ("MaxWords=3, MinWords=3", "22023"),
        ("ShortWord=-1", "22023"),
        ("MaxFragments=-1", "22023"),
        ("MaxWords", "42601"),
        ("MaxWords=", "42601"),
        ("StartSel='unterminated", "42601"),
        ("=5", "42601"),
        ("foo bar=1", "42601"),
        ("StartSel='a'b", "42601"),
    ];
    for (opts, expected) in cases {
        let sql = call_sql("a fox", "fox", opts);
        let err = exec(&core, &sql).expect_err(&sql);
        assert_eq!(err.wire_code(), *expected, "{opts}");
        let msg = err.to_string();
        assert!(
            !msg.contains("abc") && !msg.contains("fox"),
            "{opts}: {msg}"
        );
    }
    // HighlightAll が真なら語数の整合性は検査しない。真偽値以外の文字列は偽。
    assert_eq!(
        headline(&core, "a fox", "fox", "HighlightAll=1, MinWords=0"),
        "a <b>fox</b>"
    );
    assert_eq!(
        headline(&core, "a fox", "fox", "HighlightAll=maybe"),
        "a <b>fox</b>"
    );
    // 区切り文字列が 32767 バイトを超えたら 22023。
    let long = "x".repeat(32_768);
    let sql = call_sql("a fox", "fox", &format!("StartSel={long}"));
    assert_eq!(code(&core, &sql), "22023");
}

/// 関数解決・型・予約名。
#[test]
fn resolution_and_reserved_name() {
    let (_g, core) = open("search14-resolution");
    assert_eq!(code(&core, "SELECT ts_headline('a')"), "42883");
    assert_eq!(
        code(&core, "SELECT ts_headline('a', 'b', 'c', 'd')"),
        "42883"
    );
    assert_eq!(code(&core, "SELECT ts_headline(1, 'b')"), "42804");
    assert_eq!(code(&core, "SELECT ts_headline('a', 2)"), "42804");
    assert_eq!(
        code(
            &core,
            "CREATE FUNCTION ts_headline(x DOUBLE PRECISION) RETURNS DOUBLE PRECISION AS x"
        ),
        "42723"
    );
}

/// 検索語の上限超過は `22000`（既存の疎検索と同じ写像）。
#[test]
fn oversized_query_is_rejected() {
    let (_g, core) = open("search14-query-limit");
    let query = "word ".repeat(10_000);
    let sql = call_sql("a word", &query, "");
    assert_eq!(code(&core, &sql), "22000");
}

/// 式索引には置けない（STABLE。`42P17`）。
#[test]
fn not_allowed_in_expression_index() {
    let (_g, core) = open("search14-index");
    ok(
        &core,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    assert_eq!(
        code(&core, "CREATE INDEX i ON docs ((ts_headline(body, 'fox')))"),
        "42P17"
    );
}

/// RLS を暗黙適用した後の可視行だけが投影される（網羅的な越境検証は #2126）。
#[test]
fn other_tenant_body_is_not_projected() {
    let (_g, core) = open("search14-rls");
    ok(
        &core,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    ok(
        &core,
        "INSERT INTO docs (id, body) VALUES (1, 'alice secret fox')",
    );
    match exec_as(&core, "bob", "SELECT ts_headline(body, 'fox') FROM docs") {
        Ok(SqlOutcome::Query(q)) => {
            assert!(q.rows.is_empty(), "bob must not see alice's rows");
        }
        Ok(other) => panic!("unexpected outcome {other:?}"),
        Err(e) => assert!(!e.to_string().contains("secret"), "{e}"),
    }
}
