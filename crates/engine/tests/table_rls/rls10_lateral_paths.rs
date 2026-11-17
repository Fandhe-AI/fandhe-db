//! LATERAL 派生表の再評価経路を横断する RLS 境界の機械検証（Issue #3324。ポインタ: RLS-10 (b)・RLS-13・
//! SQL-60・SQL-61・SQL-62 (7)・TASK-344。設計記録 `docs/design/lateral-derived-table.md` の L15）。
//!
//! 外側の行ごとに内側を再評価する LATERAL は、呼び出したセッションの `PolicyContext`・同じ `read_txn` の
//! 可視行だけから結果を作る。本ファイルは 3 テナントの双子 DB で次を固定する。
//!
//! - **独立オラクル**: 可視行の真実表（production の可視性判定を呼ばない Rust 側の計算）と、コンマ・
//!   CROSS・INNER ON true・LEFT ON true の結果行・件数（NULL 補完を含む）が一致する。外側と内側が同じ
//!   表の場合と別の表の場合、ポリシーなし・あり、`PolicyContext::new`（Public のみ）・
//!   `with_visibilities`（Private 込み）の全組合せ
//! - **X≡Y**: 他テナントが Private 行（同じ `lang` を含む）を置いた DB（X）と置いていない DB（Y）で、
//!   観測側の結果・`EXPLAIN`・拡張クエリの結果・`54000` の境界と文言・エラーの `wire_code`／文言が一致する
//! - **ビュー**: LATERAL ビューは作成者ではなく参照者の可視行で評価される
//! - **拡張クエリ**: `$n` 経路（Parse／Bind／Execute）の結果が簡易クエリと一致する

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::correlated_limits::CorrelatedLimits;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const A: &str = "tenant-a";
const B: &str = "tenant-b";
const C: &str = "tenant-c";
const TENANTS: [&str; 3] = [A, B, C];

const DOCS: &str = "docs";
const OTHER: &str = "other";
const REL: usize = 8;

/// 1 行の真実: (表, 所有テナント, id, lang, 可視性)。
type Row = (&'static str, &'static str, u64, &'static str, Visibility);

/// 観測側（A）の行。Public は全テナントに見え、Private は所有者だけに見える。
const OBSERVER_ROWS: [Row; 7] = [
    (DOCS, A, 1, "ja", Visibility::Public),
    (DOCS, A, 2, "ja", Visibility::Public),
    (DOCS, A, 3, "en", Visibility::Private),
    (DOCS, A, 4, "fr", Visibility::Private),
    (OTHER, A, 11, "ja", Visibility::Public),
    (OTHER, A, 12, "en", Visibility::Private),
    (OTHER, A, 13, "ja", Visibility::Private),
];

/// 他テナントの Private 行（X にだけ在る）。A と同じ `lang`・A の `fr` 行に一致しうる
/// `lang` を含み、漏えいすれば結果の行数・NULL 補完の数が変わる配置にしている。
const FOREIGN_ROWS: [Row; 8] = [
    (DOCS, B, 7, "ja", Visibility::Private),
    (DOCS, B, 5, "fr", Visibility::Private),
    (DOCS, C, 6, "en", Visibility::Private),
    (OTHER, B, 14, "fr", Visibility::Private),
    (OTHER, B, 15, "ja", Visibility::Private),
    (OTHER, C, 16, "fr", Visibility::Private),
    (OTHER, C, 17, "ja", Visibility::Private),
    (OTHER, C, 18, "en", Visibility::Private),
];

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str, with_private: bool) -> PolicyContext {
    if with_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn seed(storage: &Storage, row: &Row) {
    let (table, tenant, id, lang, vis) = *row;
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant, true),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
        ],
        &OperationId::parse(&format!("seed-{table}-{tenant}-{id}")).expect("op id"),
    )
    .expect("seed row");
}

struct World {
    core: EngineCore,
    _g: CleanupGuard,
}

fn setup_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut s = ddl_session();
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant, true), &mut s, sql) {
        panic!("setup {tenant}: {sql}: {} {e}", e.wire_code());
    }
}

/// 双子 DB の片方を作る。観測側 A の表・行・RLS を先に作り（内部 ID を X/Y で揃える）、`foreign` のときだけ
/// 他テナントの Private 行を置く。`policy` のとき A の RLS とポリシー（`other` を `ja` に絞る）を置く
/// （RLS 状態・ポリシーはテナントごとで、B・C には効かない）。
fn world(
    label: &str,
    foreign: bool,
    policy: bool,
    configure: impl FnOnce(EngineCore) -> EngineCore,
) -> World {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage.create_table(&schema(OTHER)).expect("create other");
    for row in &OBSERVER_ROWS {
        seed(&storage, row);
    }
    if foreign {
        for row in &FOREIGN_ROWS {
            seed(&storage, row);
        }
    }
    let core = configure(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    if policy {
        for sql in [
            "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE other ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY p_docs ON docs AS PERMISSIVE FOR SELECT USING (true)",
            "CREATE POLICY p_other ON other AS PERMISSIVE FOR SELECT USING (lang = 'ja')",
        ] {
            setup_as(&core, A, sql);
        }
    }
    World { core, _g: guard }
}

// ---------------------------------------------------------------------------------------------
// 独立オラクル（production の可視性判定は呼ばない）
// ---------------------------------------------------------------------------------------------

/// `tenant` から見える `table` の行 `(id, lang)`（id 昇順）。ポリシーは A の `other` だけを `ja` に絞る。
fn visible(
    table: &str,
    tenant: &str,
    with_private: bool,
    foreign: bool,
    policy: bool,
) -> Vec<(u64, &'static str)> {
    let mut rows: Vec<Row> = OBSERVER_ROWS.to_vec();
    if foreign {
        rows.extend(FOREIGN_ROWS);
    }
    let mut out: Vec<(u64, &'static str)> = rows
        .into_iter()
        .filter(|(t, owner, _, lang, vis)| {
            *t == table
                && match vis {
                    Visibility::Public => true,
                    Visibility::Private => with_private && *owner == tenant,
                }
                && !(policy && tenant == A && table == OTHER && *lang != "ja")
        })
        .map(|(_, _, id, lang, _)| (id, lang))
        .collect();
    out.sort();
    out
}

/// 期待する結果行 `(d.id, t.id)`（`None` は NULL 補完）。
fn expected(
    inner: &str,
    left: bool,
    tenant: &str,
    with_private: bool,
    foreign: bool,
    policy: bool,
) -> Vec<(u64, Option<u64>)> {
    let outer = visible(DOCS, tenant, with_private, foreign, policy);
    let inner_rows = visible(inner, tenant, with_private, foreign, policy);
    let mut out = Vec::new();
    for (d_id, d_lang) in outer {
        let matches: Vec<u64> = inner_rows
            .iter()
            .filter(|(_, l)| *l == d_lang)
            .map(|(id, _)| *id)
            .take(5)
            .collect();
        if matches.is_empty() {
            if left {
                out.push((d_id, None));
            }
        } else {
            out.extend(matches.into_iter().map(|m| (d_id, Some(m))));
        }
    }
    out
}

/// 結合形ごとの文（`{inner}` は内側の表）。
fn forms(inner: &str) -> Vec<(&'static str, bool, String)> {
    let body = format!("(SELECT id FROM {inner} x WHERE x.lang = d.lang ORDER BY x.id LIMIT 5) t");
    let tail = "ORDER BY d.id, t.id LIMIT 100";
    vec![
        (
            "comma",
            false,
            format!("SELECT d.id, t.id FROM docs d, LATERAL {body} {tail}"),
        ),
        (
            "cross",
            false,
            format!("SELECT d.id, t.id FROM docs d CROSS JOIN LATERAL {body} {tail}"),
        ),
        (
            "inner",
            false,
            format!("SELECT d.id, t.id FROM docs d INNER JOIN LATERAL {body} ON true {tail}"),
        ),
        (
            "left",
            true,
            format!("SELECT d.id, t.id FROM docs d LEFT JOIN LATERAL {body} ON true {tail}"),
        ),
    ]
}

fn run(
    core: &EngineCore,
    tenant: &str,
    with_private: bool,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant, with_private), &mut s, sql)
}

fn id_of(c: Option<&Cell>) -> Option<u64> {
    match c {
        Some(Cell::Integer(v)) => Some(*v),
        Some(Cell::SignedInteger(v)) => u64::try_from(*v).ok(),
        Some(Cell::Null) | None => None,
        other => panic!("unexpected id cell {other:?}"),
    }
}

fn pairs(
    core: &EngineCore,
    tenant: &str,
    with_private: bool,
    sql: &str,
) -> Vec<(u64, Option<u64>)> {
    match run(core, tenant, with_private, sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                (
                    id_of(row.cells.first()).unwrap_or_else(|| panic!("{sql}: NULL outer id")),
                    id_of(row.cells.get(1)),
                )
            })
            .collect(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn trace(core: &EngineCore, tenant: &str, with_private: bool, sql: &str) -> String {
    match run(core, tenant, with_private, sql) {
        Ok(o) => format!("OK {o:?}"),
        Err(e) => format!("ERR {} {}", e.wire_code(), e.client_message()),
    }
}

// ---------------------------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------------------------

#[test]
fn lateral_results_match_the_oracle_for_every_tenant_mode_and_policy() {
    for policy in [false, true] {
        let w = world("rls10-lateral-oracle", true, policy, |c| c);
        for with_private in [false, true] {
            for tenant in TENANTS {
                for inner in [OTHER, DOCS] {
                    for (form, left, sql) in forms(inner) {
                        let want = expected(inner, left, tenant, with_private, true, policy);
                        let got = pairs(&w.core, tenant, with_private, &sql);
                        assert_eq!(
                            got, want,
                            "{form} inner={inner} {tenant} private={with_private} policy={policy}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn foreign_private_rows_never_change_the_observers_results() {
    // 陽性対照: 観測側の結果は非空で、LEFT には NULL 補完行がある（A の `fr` 行は内側に一致が無い）。
    let x = world("rls10-lateral-x", true, false, |c| c);
    let y = world("rls10-lateral-y", false, false, |c| c);
    for with_private in [false, true] {
        for inner in [OTHER, DOCS] {
            for (form, left, sql) in forms(inner) {
                let a = trace(&x.core, A, with_private, &sql);
                let b = trace(&y.core, A, with_private, &sql);
                assert_eq!(a, b, "{form} inner={inner} private={with_private}");
                if with_private && left && inner == OTHER {
                    let rows = pairs(&x.core, A, true, &sql);
                    assert!(!rows.is_empty());
                    assert!(rows.iter().any(|(_, t)| t.is_none()), "{rows:?}");
                    // NULL 補完の数は他テナントの `fr` 行で変わらない（A の fr 行 1 つだけ）。
                    assert_eq!(rows.iter().filter(|(_, t)| t.is_none()).count(), 1);
                }
            }
        }
    }
}

#[test]
fn explain_output_is_identical_for_every_tenant_and_with_or_without_foreign_rows() {
    let x = world("rls10-lateral-explain-x", true, false, |c| c);
    let y = world("rls10-lateral-explain-y", false, false, |c| c);
    for (form, _, sql) in forms(OTHER) {
        let explain = format!("EXPLAIN {sql}");
        let base = trace(&y.core, A, true, &explain);
        assert!(base.contains("per_outer_row"), "{form}: {base}");
        for tenant in TENANTS {
            for with_private in [false, true] {
                assert_eq!(
                    base,
                    trace(&x.core, tenant, with_private, &explain),
                    "{form} {tenant}"
                );
            }
        }
    }
}

#[test]
fn cap_boundaries_are_decided_by_the_callers_visible_rows_only() {
    let (_, _, sql) = forms(OTHER).remove(0);
    // 観測側 A（Private 込み）の外側タプルは 4 件・出力は 5 行。他テナントの Private 行があっても境界は動かない。
    for foreign in [false, true] {
        let ok = |c: EngineCore| c.with_correlated_limits(CorrelatedLimits::new(4));
        let ng = |c: EngineCore| c.with_correlated_limits(CorrelatedLimits::new(3));
        let w = world("rls10-lateral-cap-ok", foreign, false, ok);
        assert_eq!(pairs(&w.core, A, true, &sql).len(), 5, "foreign={foreign}");
        let w = world("rls10-lateral-cap-ng", foreign, false, ng);
        let e = run(&w.core, A, true, &sql).expect_err("over the cap");
        assert_eq!(e.wire_code(), "54000", "foreign={foreign}");
    }
    let message = |foreign: bool| {
        let w = world("rls10-lateral-cap-msg", foreign, false, |c| {
            c.with_correlated_limits(CorrelatedLimits::new(3))
        });
        trace(&w.core, A, true, &sql)
    };
    assert_eq!(message(false), message(true));
    for foreign in [false, true] {
        let ok = |c: EngineCore| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 5));
        let ng = |c: EngineCore| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 4));
        let w = world("rls10-lateral-join-ok", foreign, false, ok);
        assert_eq!(pairs(&w.core, A, true, &sql).len(), 5, "foreign={foreign}");
        let w = world("rls10-lateral-join-ng", foreign, false, ng);
        assert_eq!(
            run(&w.core, A, true, &sql)
                .expect_err("over the cap")
                .wire_code(),
            "54000",
            "foreign={foreign}"
        );
    }
    let message = |foreign: bool| {
        let w = world("rls10-lateral-join-msg", foreign, false, |c| {
            c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 4))
        });
        trace(&w.core, A, true, &sql)
    };
    assert_eq!(message(false), message(true));
}

#[test]
fn binding_errors_are_identical_across_tenants_and_hide_internal_names() {
    let x = world("rls10-lateral-err", true, true, |c| c);
    for sql in [
        "SELECT * FROM docs d, LATERAL (SELECT id FROM other x WHERE x.lang = d.nope) t",
        "SELECT * FROM docs d, LATERAL (SELECT id FROM other x WHERE x.lang = z.lang) t",
        "SELECT * FROM docs d RIGHT JOIN LATERAL (SELECT id FROM other x WHERE x.lang = d.lang) t ON true",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM other x ORDER BY d.lang) t",
    ] {
        let base = trace(&x.core, A, true, sql);
        assert!(base.starts_with("ERR "), "{sql}: {base}");
        assert!(!base.contains('\u{2}') && !base.contains("lateral0"), "{sql}: {base}");
        for tenant in [B, C] {
            assert_eq!(base, trace(&x.core, tenant, true, sql), "{sql} ({tenant})");
        }
    }
}

#[test]
fn lateral_view_is_evaluated_for_the_referencer_not_the_creator() {
    let x = world("rls10-lateral-view", true, false, |c| c);
    // 作成者は C。参照者 A・B・C がそれぞれ自分の可視行だけで評価される。
    let body = "SELECT d.id AS did, t.id AS tid FROM docs d, LATERAL (SELECT id FROM other x WHERE x.lang = d.lang ORDER BY x.id LIMIT 5) t LIMIT 100";
    {
        let mut s = ddl_session();
        x.core
            .execute_sql_in_session(
                &ctx(C, true),
                &mut s,
                &format!("CREATE VIEW v_lat AS {body}"),
            )
            .expect("create view");
    }
    for with_private in [false, true] {
        for tenant in TENANTS {
            let want = expected(OTHER, false, tenant, with_private, true, false);
            let got = pairs(
                &x.core,
                tenant,
                with_private,
                "SELECT did, tid FROM v_lat ORDER BY did, tid LIMIT 100",
            );
            assert_eq!(got, want, "{tenant} private={with_private}");
        }
    }
}

#[test]
fn extended_protocol_results_match_the_simple_protocol_per_tenant() {
    let x = world("rls10-lateral-ext", true, false, |c| c);
    let sql_param = "SELECT d.id, t.id FROM docs d, LATERAL (SELECT id FROM other x WHERE x.lang = d.lang AND x.lang = $1 ORDER BY x.id LIMIT 5) t ORDER BY d.id, t.id LIMIT 100";
    let sql_literal = "SELECT d.id, t.id FROM docs d, LATERAL (SELECT id FROM other x WHERE x.lang = d.lang AND x.lang = 'ja' ORDER BY x.id LIMIT 5) t ORDER BY d.id, t.id LIMIT 100";
    for with_private in [false, true] {
        for tenant in TENANTS {
            let prepared = x.core.parse_sql_prepared(sql_param).expect("parse");
            let bound = x
                .core
                .bind_prepared(&prepared, &[Some(b"ja".to_vec())])
                .expect("bind");
            let mut s = SessionState::default();
            let got = match x
                .core
                .execute_parsed_in_session(&ctx(tenant, with_private), &mut s, &bound)
                .expect("execute")
            {
                SqlOutcome::Query(r) => r
                    .rows
                    .iter()
                    .map(|row| (id_of(row.cells.first()), id_of(row.cells.get(1))))
                    .collect::<Vec<_>>(),
                other => panic!("expected Query, got {other:?}"),
            };
            let want = pairs(&x.core, tenant, with_private, sql_literal)
                .into_iter()
                .map(|(d, t)| (Some(d), t))
                .collect::<Vec<_>>();
            assert_eq!(got, want, "{tenant} private={with_private}");
        }
    }
}
