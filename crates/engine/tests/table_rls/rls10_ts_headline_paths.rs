//! `ts_headline`（highlight・snippet）経由のテナント越境を、機械的なマトリクスで固定する回帰テスト
//! （Issue #2126。ポインタ: SEARCH-14・RLS-8、関連 RLS-10・RLS-12 (4)・SQL-40。設計は
//! `docs/design/ts-headline-limits-cli.md`）。
//!
//! 役割: `ts_headline` は行の本文から抜粋を作る投影関数であり、不可視行の本文が評価・投影に
//! 到達すればそのままテナント越境になる。`search14_ts_headline.rs`（SQL 結合テストのスモーク）を
//! 補い、閲覧テナント 3 つ × 可視性 2 モード × 実行経路 2 つ（autocommit／明示トランザクション）×
//! 読み取り形状で統一的に検査する。production の可視性判定（`PolicyContext::is_visible` 等）は
//! オラクル側で呼ばない（テスト側が持つ真実値だけで期待集合を計算する）。
//!
//! - T0 陽性対照: 自テナントの可視行の抜粋に `<b>fox</b>` と自分のカナリアが現れ、期待集合が空でない
//! - T1 独立オラクル: 応答に現れるカナリア集合 = 真実値から計算した期待集合（不可視行のカナリアは 0 件）
//! - T2 flood 不変性: 他テナントの Private 行の有無で応答が完全に一致する
//! - T3 エラーの不変性と秘匿: オプション不正（`22023`）・書式エラー（`42601`）の応答が flood の有無で
//!   変わらず、文面にカナリア・テナント名を含まない
//! - T4 データ依存カナリア: 出力上限を下げたコアで、他テナントの不可視行にだけ上限超過の本文を
//!   置いても閲覧側は成功し結果が不変（不可視行で評価が走れば `54000` になる）。陽性対照として
//!   可視行に同じ本文を置くと `54000`
//! - T5 列の権限（SQL-40）: 付与の無い列への `ts_headline` は `42501` で、応答は他テナントの行の
//!   有無で変わらない
//! - T6 負の対照と試行総数の固定（空振り防止）

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::headline_limits::HeadlineLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 上限を下げたコアの出力バイト上限・断片数上限（T4）。
const SMALL_OUTPUT: usize = 200;
const SMALL_FRAGMENTS: usize = 4;

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Auto,
    Txn,
}

/// 実行結果（成功時は全セルの Debug 文字列の列、失敗時は wire_code とクライアント向け文面）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Out {
    Rows(Vec<String>),
    Err { code: String, message: String },
}

fn to_out(res: Result<SqlOutcome, SqlSurfaceError>, sql: &str) -> Out {
    match res {
        Ok(SqlOutcome::Query(q)) => {
            Out::Rows(q.rows.iter().map(|r| format!("{:?}", r.cells)).collect())
        }
        Ok(other) => panic!("{sql}: expected Query, got {other:?}"),
        Err(e) => Out::Err {
            code: e.wire_code().to_string(),
            message: e.client_message(),
        },
    }
}

fn run(core: &EngineCore, c: &PolicyContext, path: Path, sql: &str) -> Out {
    let res = match path {
        Path::Auto => {
            let mut s = SessionState::default();
            core.execute_sql_in_session(c, &mut s, sql)
        }
        Path::Txn => {
            let mut s = SessionState::default();
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(c, &mut s, &mut txn, "BEGIN")
                .expect("begin");
            let r = core.execute_sql_in_txn(c, &mut s, &mut txn, sql);
            let _ = core.execute_sql_in_txn(c, &mut s, &mut txn, "ROLLBACK");
            r
        }
    };
    to_out(res, sql)
}

// ---------- フィクスチャ ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Truth {
    tenant: &'static str,
    id: u64,
    visibility: Visibility,
}

/// 独立オラクル: production の可視性判定を呼ばずに可視かを決める。
fn is_allowed(t: &Truth, viewer: &str, allow_private: bool) -> bool {
    match t.visibility {
        Visibility::Public => true,
        Visibility::Private => t.tenant == viewer && allow_private,
    }
}

fn canary(t: &Truth) -> String {
    let v = match t.visibility {
        Visibility::Public => "pub",
        Visibility::Private => "priv",
    };
    format!("canary_{}_{}_{v}", t.tenant.replace('-', "_"), t.id)
}

/// id 1 は Public・一致語あり、id 2 は Private・一致語あり、id 3 は Public・一致語なし。
fn base_rows(tenant: &'static str) -> Vec<Truth> {
    vec![
        Truth {
            tenant,
            id: 1,
            visibility: Visibility::Public,
        },
        Truth {
            tenant,
            id: 2,
            visibility: Visibility::Private,
        },
        Truth {
            tenant,
            id: 3,
            visibility: Visibility::Public,
        },
    ]
}

fn has_fox(t: &Truth) -> bool {
    t.id != 3
}

fn body_of(t: &Truth) -> String {
    if has_fox(t) {
        format!("the quick fox {}", canary(t))
    } else {
        format!("a lazy dog {}", canary(t))
    }
}

/// T4 で置く巨大本文（一致語と大量の語。`HighlightAll=true` なら出力が上限を超える）。
fn huge_body(t: &Truth) -> String {
    format!("fox {} {}", canary(t), "word ".repeat(400))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Big {
    /// 巨大本文を置かない。
    None,
    /// 他テナント（tenant-b）の Private 行（id 9）だけが巨大本文を持つ。
    OtherPrivate,
    /// 閲覧テナント自身の Public 行（id 9）が巨大本文を持つ（陽性対照）。
    OwnPublic,
}

struct Fx {
    core: EngineCore,
    _guard: CleanupGuard,
    truths: Vec<Truth>,
}

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("secret", ColumnType::Text, false),
        ],
    )
}

fn insert(storage: &Storage, table: &str, t: &Truth, body: &str, seq: &mut u64) {
    *seq += 1;
    let op = OperationId::parse(&format!("th-rls-{table}-{seq}")).expect("op id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx_for(t.tenant, true),
        t.id,
        t.visibility,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text(body.to_string()),
            Value::Text(canary(t)),
        ],
        &op,
    )
    .unwrap_or_else(|e| panic!("insert {table} {t:?}: {} {e}", e.wire_code()));
}

/// `flood=false` は閲覧側の行と他テナントの Public 行だけ（baseline）。`flood=true` は他テナントの
/// Private 行（id 2）を足す。baseline と flooded の差は「他テナントの不可視行」だけ。
fn build(
    label: &str,
    viewer: &'static str,
    flood: bool,
    big: Big,
    limits: Option<HeadlineLimits>,
) -> Fx {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in ["docs", "notes"] {
        storage.create_table(&schema(t)).expect("create table");
    }
    let mut truths = Vec::new();
    let mut seq = 0u64;
    for &tenant in TENANTS.iter() {
        for t in base_rows(tenant) {
            let hidden_foreign = t.tenant != viewer && t.visibility == Visibility::Private;
            if hidden_foreign && !flood {
                continue;
            }
            for table in ["docs", "notes"] {
                insert(&storage, table, &t, &body_of(&t), &mut seq);
            }
            truths.push(t);
        }
    }
    let huge = match big {
        Big::None => None,
        Big::OtherPrivate => Some(Truth {
            tenant: "tenant-b",
            id: 9,
            visibility: Visibility::Private,
        }),
        Big::OwnPublic => Some(Truth {
            tenant: viewer,
            id: 9,
            visibility: Visibility::Public,
        }),
    };
    if let Some(t) = huge {
        insert(&storage, "docs", &t, &huge_body(&t), &mut seq);
        truths.push(t);
    }
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_headline_limits(l);
    }
    Fx {
        core,
        _guard: guard,
        truths,
    }
}

// ---------- 形状 ----------

struct Shape {
    name: &'static str,
    sql: &'static str,
    /// 期待集合の範囲: true なら一致語を持つ行だけ、false なら表の全可視行。
    match_only: bool,
}

/// 射影に関数呼び出しを置けない形（JOIN・派生表・CTE。現状は構文として未対応で 42601）。
/// 受理されない形でも、flood の有無で応答が変わらず文面が秘匿されることを契約として固定する。
const REJECTED_SQL: [&str; 3] = [
    "SELECT docs.id, ts_headline(notes.body, 'fox') FROM docs JOIN notes ON docs.id = notes.id",
    "SELECT id, h FROM (SELECT id, ts_headline(body, 'fox') AS h FROM docs) s",
    "WITH s AS (SELECT id, ts_headline(body, 'fox') AS h FROM docs) SELECT id, h FROM s",
];

fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            name: "plain",
            sql: "SELECT id, ts_headline(body, 'fox') FROM docs",
            match_only: false,
        },
        Shape {
            name: "where",
            sql: "SELECT id, ts_headline(body, 'fox') FROM docs WHERE body LIKE '%fox%'",
            match_only: true,
        },
        Shape {
            name: "options",
            sql: "SELECT id, ts_headline(body, 'fox', 'HighlightAll=true, MaxFragments=2') FROM docs",
            match_only: false,
        },
        Shape {
            name: "vector_search",
            sql: "SELECT id, ts_headline(body, 'fox') FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20",
            match_only: false,
        },
        Shape {
            name: "hybrid_search",
            sql: "SELECT id, ts_headline(body, 'fox') FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'fox') LIMIT 20",
            match_only: false,
        },
        Shape {
            name: "nested_fn",
            sql: "SELECT id, upper(ts_headline(body, 'fox')) FROM docs",
            match_only: false,
        },
        Shape {
            name: "in_where",
            sql: "SELECT id FROM docs WHERE ts_headline(body, 'fox') LIKE '%<b>%'",
            match_only: true,
        },
    ]
}

/// 応答中のカナリア（`canary_...`）を集める。
fn canaries_in(rows: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for r in rows {
        let lowered = r.to_ascii_lowercase();
        let mut rest = lowered.as_str();
        while let Some(p) = rest.find("canary_") {
            let tail = &rest[p..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            out.insert(tail[..end].to_string());
            rest = &tail[end..];
        }
    }
    out
}

fn expected_canaries(
    fx: &Fx,
    viewer: &str,
    allow_private: bool,
    match_only: bool,
) -> BTreeSet<String> {
    fx.truths
        .iter()
        .filter(|t| {
            t.id != 9 && is_allowed(t, viewer, allow_private) && (!match_only || has_fox(t))
        })
        .map(canary)
        .collect()
}

/// T1: 期待集合との一致・不可視行のカナリアが 0 件・行数の一致。
fn check_rows(fx: &Fx, viewer: &str, allow_private: bool, shape: &Shape, rows: &[String]) {
    let expected = expected_canaries(fx, viewer, allow_private, shape.match_only);
    // `in_where` は id のみ返すためカナリアは現れない。行数だけで検査する。
    if shape.name != "in_where" {
        assert_eq!(
            canaries_in(rows),
            expected,
            "{} viewer={viewer} private={allow_private}",
            shape.name
        );
    }
    assert_eq!(
        rows.len(),
        expected.len(),
        "{} row count viewer={viewer} private={allow_private}: {rows:?}",
        shape.name
    );
    for t in fx
        .truths
        .iter()
        .filter(|t| !is_allowed(t, viewer, allow_private))
    {
        let forbidden = canary(t);
        assert!(
            !rows
                .iter()
                .any(|r| r.to_ascii_lowercase().contains(&forbidden)),
            "{}: forbidden canary {forbidden} leaked to {viewer}",
            shape.name
        );
    }
}

fn rows_of(out: Out, sql: &str) -> Vec<String> {
    match out {
        Out::Rows(r) => r,
        Out::Err { code, message } => panic!("{sql}: {code} {message}"),
    }
}

fn leak_check(message: &str) {
    assert!(!message.contains("canary_"), "{message}");
    for t in TENANTS {
        assert!(!message.contains(t), "{message}");
    }
}

// ---------- テスト ----------

#[test]
fn ts_headline_rls_matrix() {
    let mut attempts = 0usize;
    let mut nonempty = 0usize;
    for (vi, viewer) in TENANTS.iter().copied().enumerate() {
        let base = build(&format!("th-rls-base-{vi}"), viewer, false, Big::None, None);
        let flood = build(&format!("th-rls-flood-{vi}"), viewer, true, Big::None, None);
        for allow_private in [false, true] {
            let c = ctx_for(viewer, allow_private);
            for path in [Path::Auto, Path::Txn] {
                for shape in shapes() {
                    attempts += 1;
                    let b = run(&base.core, &c, path, shape.sql);
                    let f = run(&flood.core, &c, path, shape.sql);
                    // T2: flood 不変性（バイト単位）。
                    assert_eq!(b, f, "{} flood invariance viewer={viewer}", shape.name);
                    let rows = rows_of(f, shape.sql);
                    check_rows(&flood, viewer, allow_private, &shape, &rows);
                    // T0: 自テナントの可視行の抜粋に一致語の強調と自分のカナリアが現れる。
                    let own: Vec<&Truth> = flood
                        .truths
                        .iter()
                        .filter(|t| {
                            t.tenant == viewer && is_allowed(t, viewer, allow_private) && has_fox(t)
                        })
                        .collect();
                    if !own.is_empty() {
                        nonempty += 1;
                        if shape.name != "in_where" && shape.name != "nested_fn" {
                            assert!(
                                rows.iter().any(|r| r.contains("<b>fox</b>")),
                                "{}: {rows:?}",
                                shape.name
                            );
                        }
                        for t in own {
                            if shape.name != "in_where" {
                                assert!(
                                    rows.iter()
                                        .any(|r| r.to_ascii_lowercase().contains(&canary(t))),
                                    "{}: own {t:?}",
                                    shape.name
                                );
                            }
                        }
                    }
                }
                // 射影に関数呼び出しを置けない形は現状 42601 で fail-closed。
                for sql in REJECTED_SQL {
                    attempts += 1;
                    let jb = run(&base.core, &c, path, sql);
                    let jf = run(&flood.core, &c, path, sql);
                    assert_eq!(jb, jf, "{sql}: flood invariance viewer={viewer}");
                    match jf {
                        Out::Err { code, message } => {
                            assert_eq!(code, "42601", "{sql}");
                            leak_check(&message);
                        }
                        Out::Rows(r) => panic!("{sql} must stay rejected: {r:?}"),
                    }
                }
            }
        }
    }
    // T6: 試行総数の固定（形状 10 × 閲覧 3 × モード 2 × 経路 2）。
    assert_eq!(attempts, 10 * 3 * 2 * 2);
    assert!(
        nonempty >= attempts / 2,
        "positive controls too few: {nonempty}"
    );
}

/// T3: オプション不正・書式エラーの応答が flood の有無で変わらず、文面が秘匿されている。
#[test]
fn ts_headline_errors_are_invariant_and_clean() {
    let cases: &[(&str, &str)] = &[
        (
            "SELECT ts_headline(body, 'fox', 'Bogus=1') FROM docs",
            "22023",
        ),
        (
            "SELECT ts_headline(body, 'fox', 'MaxWords') FROM docs",
            "42601",
        ),
        (
            "SELECT ts_headline(body, 'fox', 'MaxFragments=-1') FROM docs",
            "22023",
        ),
    ];
    for (vi, viewer) in TENANTS.iter().copied().enumerate() {
        let base = build(&format!("th-err-base-{vi}"), viewer, false, Big::None, None);
        let flood = build(&format!("th-err-flood-{vi}"), viewer, true, Big::None, None);
        for allow_private in [false, true] {
            let c = ctx_for(viewer, allow_private);
            for path in [Path::Auto, Path::Txn] {
                for (sql, want) in cases {
                    let b = run(&base.core, &c, path, sql);
                    let f = run(&flood.core, &c, path, sql);
                    assert_eq!(b, f, "{sql}");
                    match f {
                        Out::Err { code, message } => {
                            assert_eq!(&code, want, "{sql}");
                            leak_check(&message);
                        }
                        Out::Rows(r) => panic!("{sql} must fail, got {r:?}"),
                    }
                }
            }
        }
    }
}

/// T4: 上限を下げたコアで、不可視行にだけ超過本文があっても閲覧側は成功し結果が不変。
#[test]
fn ts_headline_limits_are_not_evaluated_on_invisible_rows() {
    let sql = "SELECT id, ts_headline(body, 'fox', 'HighlightAll=true') FROM docs";
    let limits = HeadlineLimits::new(SMALL_OUTPUT, SMALL_FRAGMENTS);
    let mut checked = 0usize;
    for (vi, viewer) in TENANTS.iter().copied().enumerate() {
        // 巨大行は tenant-b の Private 行。閲覧側が tenant-b だと自分の行になるため対象外。
        if viewer == "tenant-b" {
            continue;
        }
        let clean = build(
            &format!("th-lim-clean-{vi}"),
            viewer,
            true,
            Big::None,
            Some(limits),
        );
        let hidden = build(
            &format!("th-lim-hidden-{vi}"),
            viewer,
            true,
            Big::OtherPrivate,
            Some(limits),
        );
        let own = build(
            &format!("th-lim-own-{vi}"),
            viewer,
            true,
            Big::OwnPublic,
            Some(limits),
        );
        for allow_private in [false, true] {
            let c = ctx_for(viewer, allow_private);
            for path in [Path::Auto, Path::Txn] {
                let a = run(&clean.core, &c, path, sql);
                let b = run(&hidden.core, &c, path, sql);
                assert!(matches!(a, Out::Rows(_)), "{a:?}");
                assert_eq!(a, b, "hidden oversize row must not change the response");
                match run(&own.core, &c, path, sql) {
                    Out::Err { code, message } => {
                        assert_eq!(code, "54000");
                        leak_check(&message);
                    }
                    Out::Rows(r) => {
                        panic!("visible oversize row must fail with 54000, got {r:?}")
                    }
                }
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 2 * 2 * 2);
}

/// T5: 列の権限（SQL-40）。付与の無い列の `ts_headline` は `42501`。他テナントの行の有無で不変。
#[test]
fn ts_headline_respects_column_privileges() {
    fn bound(tenant: &str, id: u64) -> SessionState {
        let mut s = SessionState::default();
        s.bind_role_ids(id, id);
        assert!(s.bind_resolution_tenant(tenant));
        s
    }
    fn operator(tenant: &str) -> SessionState {
        let mut s = SessionState::default();
        s.allow_ddl();
        assert!(s.bind_resolution_tenant(tenant));
        s
    }
    let viewer = "tenant-a";
    let mut results: Vec<(Out, Out)> = Vec::new();
    for flood in [false, true] {
        let fx = build(&format!("th-priv-{flood}"), viewer, flood, Big::None, None);
        let c = ctx_for(viewer, true);
        let setup = |sql: &str, mut s: SessionState| {
            fx.core
                .execute_sql_in_session(&c, &mut s, sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        };
        setup("CREATE ROLE adm", operator(viewer));
        setup("CREATE ROLE reader", operator(viewer));
        let mut admin = bound(viewer, 1);
        admin.allow_ddl();
        setup("GRANT SELECT (body) ON docs TO reader", admin);
        let read = |sql: &str| {
            let mut s = bound(viewer, 2);
            to_out(fx.core.execute_sql_in_session(&c, &mut s, sql), sql)
        };
        let granted = read("SELECT ts_headline(body, 'fox') FROM docs");
        let denied = read("SELECT ts_headline(secret, 'fox') FROM docs");
        match &granted {
            Out::Rows(r) => assert!(!r.is_empty()),
            other => panic!("granted column must succeed: {other:?}"),
        }
        match &denied {
            Out::Err { code, message } => {
                assert_eq!(code, "42501");
                leak_check(message);
            }
            other => panic!("ungranted column must fail with 42501: {other:?}"),
        }
        results.push((granted, denied));
    }
    assert_eq!(results[0].1, results[1].1);
    assert_eq!(results[0].0, results[1].0);
}

/// T6 負の対照: 捏造した違反（不可視行のカナリア・欠落行）を検査ヘルパーが検出する。
#[test]
fn checker_detects_fabricated_violations() {
    let fx = build("th-neg", "tenant-a", true, Big::None, None);
    let shapes = shapes();
    let shape = &shapes[0];
    let ok = rows_of(
        run(&fx.core, &ctx_for("tenant-a", true), Path::Auto, shape.sql),
        shape.sql,
    );
    let leaked: Vec<String> = ok
        .iter()
        .cloned()
        .chain(std::iter::once("canary_tenant_b_2_priv".to_string()))
        .collect();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_rows(&fx, "tenant-a", true, shape, &leaked)
    }));
    assert!(r.is_err(), "leak must be detected");
    let short: Vec<String> = ok.iter().skip(1).cloned().collect();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_rows(&fx, "tenant-a", true, shape, &short)
    }));
    assert!(r.is_err(), "missing row must be detected");
}
