//! `SELECT DISTINCT ON` の決定性・上限・テナント越境の結合テスト（Issue #3262）。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-25（関連: RLS-7・RLS-8）。
//! 契約の根拠は `docs/design/select-distinct-on.md` §6。
//!
//! `EngineCore::execute_sql` → `sql::scan` 経路 (B) の代表行選択を、`PolicyContext::is_visible` を
//! 呼ばない独立オラクルと突き合わせる。既存コーパス（`sql25_distinct.rs`）は id がテナント順に
//! 単調増加するため「走査順」と「id 順」を区別できない。ここでは id がテナント間で交互・同一 id が
//! 複数テナントに存在・他テナントの private 行が最小 id を持つ、という専用フィクスチャを使う。
//! オラクルは安定ソートのみを使い、不安定ソート API を持ち込まない（sort-determinism-check と同じ規約）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 2;
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

type Truth = (&'static str, u64, Visibility, Option<&'static str>);

/// (テナント, id, 可視性, grp)。性質:
/// - grp "k": 最小 id (1) は tenant-c の private（他者から不可視）。id 5 は tenant-a・tenant-b に重複。
/// - grp "m": 走査順で後ろの tenant-c の public 行が最小 id。
/// - grp "p": tenant-b の private だけに存在（他者のグループ数に数えない）。
/// - grp "q": 同一 id 9 が tenant-a・tenant-b の public に存在（tenant バイト順で a が代表）。
/// - NULL: 複数テナントにまたがる。
const ROWS: [Truth; 13] = [
    ("tenant-a", 5, Visibility::Public, Some("k")),
    ("tenant-b", 5, Visibility::Public, Some("k")),
    ("tenant-b", 2, Visibility::Public, Some("k")),
    ("tenant-c", 1, Visibility::Private, Some("k")),
    ("tenant-a", 7, Visibility::Public, Some("m")),
    ("tenant-c", 3, Visibility::Public, Some("m")),
    ("tenant-b", 4, Visibility::Private, Some("p")),
    ("tenant-a", 9, Visibility::Public, Some("q")),
    ("tenant-b", 9, Visibility::Public, Some("q")),
    ("tenant-a", 11, Visibility::Public, None),
    ("tenant-c", 6, Visibility::Public, None),
    ("tenant-b", 8, Visibility::Private, None),
    ("tenant-c", 12, Visibility::Public, Some("m")),
];

fn schema() -> TableSchema {
    TableSchema::new(
        "dv",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("grp", ColumnType::Text, true),
            ColumnDef::new("owner", ColumnType::Text, false),
        ],
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

/// `rows` の順に挿入した core を作る（`cap` 指定時は結果行数上限を設定する）。
fn build(label: &str, rows: &[Truth], cap: Option<usize>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (n, (tenant, id, vis, grp)) in rows.iter().enumerate() {
        let ctx = ctx_for(tenant, true);
        engine::tenant::insert_typed_row(
            &storage,
            "dv",
            &ctx,
            *id,
            *vis,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                grp.map(|g| Value::Text(g.to_string()))
                    .unwrap_or(Value::Null),
                Value::Text((*tenant).to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{n}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(cap) = cap {
        core = core.with_result_limits(ResultLimits::new(cap));
    }
    (core, guard)
}

fn visible(rows: &[Truth], viewer: &str, allow_private: bool) -> Vec<Truth> {
    rows.iter()
        .copied()
        .filter(|(t, _, vis, _)| match vis {
            Visibility::Public => true,
            Visibility::Private => *t == viewer && allow_private,
        })
        .collect()
}

type Rep = (Option<String>, u64, String);

/// 独立オラクル: (grp ASC NULLS LAST, id, tenant バイト順) の安定ソート後にグループ先頭だけを残す。
fn oracle(visible: &[Truth]) -> Vec<Rep> {
    let mut v = visible.to_vec();
    v.sort_by(|a, b| {
        let ka = (a.3.is_none(), a.3, a.1, a.0.as_bytes());
        let kb = (b.3.is_none(), b.3, b.1, b.0.as_bytes());
        ka.cmp(&kb)
    });
    let mut out: Vec<Rep> = Vec::new();
    let mut last: Option<Option<&str>> = None;
    for (t, id, _, g) in v {
        if last != Some(g) {
            out.push((g.map(str::to_string), id, t.to_string()));
            last = Some(g);
        }
    }
    out
}

fn reps(result: &QueryResult) -> Vec<Rep> {
    result
        .rows
        .iter()
        .map(|r| {
            let g = match &r.cells[0] {
                Cell::Text(s) => Some(s.clone()),
                Cell::Null => None,
                o => panic!("grp: {o:?}"),
            };
            let id = match &r.cells[1] {
                Cell::Integer(n) => *n,
                o => panic!("id: {o:?}"),
            };
            let owner = match &r.cells[2] {
                Cell::Text(s) => s.clone(),
                o => panic!("owner: {o:?}"),
            };
            (g, id, owner)
        })
        .collect()
}

const BASE: &str = "SELECT DISTINCT ON (grp) grp, id, owner FROM dv";

/// ORDER BY なしの代表行は「グループ内で (id, tenant) 最小の可視行」、グループ順は ON キー ASC（NULLS LAST）。
/// 末尾 `;` 付きの単独送信でも同じ。
#[test]
fn distinct_on_without_order_by_picks_min_id_then_tenant_per_group() {
    let (core, _g) = build("sql25-don-det", &ROWS, None);
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let want = oracle(&visible(&ROWS, tenant, allow_private));
            for sql in [BASE.to_string(), format!("{BASE};")] {
                let got = reps(&core.execute_sql(&ctx, &sql).expect("ok"));
                assert_eq!(got, want, "{tenant} {allow_private} {sql}");
            }
        }
    }
    // tie-break が tenant バイト順であること（id 9 は a・b の両方に存在）を固定値でも確認する。
    let got = reps(
        &core
            .execute_sql(&ctx_for("tenant-b", true), BASE)
            .expect("ok"),
    );
    assert!(got.contains(&(Some("q".into()), 9, "tenant-a".into())));
}

/// 後続キー無しの ORDER BY でも同点は (id, tenant) 最小で決まる。DESC・NULLS FIRST はグループ順だけを変える。
#[test]
fn distinct_on_order_by_on_key_only_breaks_ties_by_id_then_tenant() {
    let (core, _g) = build("sql25-don-ob", &ROWS, None);
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let asc = oracle(&visible(&ROWS, tenant, allow_private));
            let mut desc = asc.clone();
            desc.reverse();
            let mut nulls_first = asc.clone();
            if let Some(n) = nulls_first.pop_if(|r| r.0.is_none()) {
                nulls_first.insert(0, n);
            }
            for (suffix, want) in [
                ("ORDER BY grp", &asc),
                ("ORDER BY grp DESC", &desc),
                ("ORDER BY grp NULLS FIRST", &nulls_first),
            ] {
                let sql = format!("{BASE} {suffix}");
                let got = reps(&core.execute_sql(&ctx, &sql).expect("ok"));
                assert_eq!(&got, want, "{tenant} {allow_private} {sql}");
            }
        }
    }
}

/// 同一 core で繰り返してもビット一致し、挿入順を逆にした別 DB でも同一結果になる。
#[test]
fn distinct_on_without_order_by_is_repeatable_and_insert_order_independent() {
    let (core, _g1) = build("sql25-don-rep1", &ROWS, None);
    let mut reversed = ROWS;
    reversed.reverse();
    let (core2, _g2) = build("sql25-don-rep2", &reversed, None);
    for tenant in TENANTS {
        let ctx = ctx_for(tenant, true);
        let first = reps(&core.execute_sql(&ctx, BASE).expect("ok"));
        for _ in 0..20 {
            assert_eq!(reps(&core.execute_sql(&ctx, BASE).expect("ok")), first);
        }
        assert_eq!(reps(&core2.execute_sql(&ctx, BASE).expect("ok")), first);
    }
}

/// 他テナントの private 行は代表にならず、グループ数にも数えられない。private 行を除いた対照 DB と一致する。
#[test]
fn distinct_on_without_order_by_ignores_invisible_rows_of_other_tenants() {
    let (core, _g1) = build("sql25-don-inv1", &ROWS, None);
    for tenant in TENANTS {
        let others_private_removed: Vec<Truth> = ROWS
            .iter()
            .copied()
            .filter(|(t, _, vis, _)| *vis == Visibility::Public || *t == tenant)
            .collect();
        let (control, _g2) = build("sql25-don-inv2", &others_private_removed, None);
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let got = reps(&core.execute_sql(&ctx, BASE).expect("ok"));
            assert_eq!(got, reps(&control.execute_sql(&ctx, BASE).expect("ok")));
            for (g, id, owner) in &got {
                let src = ROWS
                    .iter()
                    .find(|r| r.3 == g.as_deref() && r.1 == *id && r.0 == owner)
                    .expect("row exists");
                assert!(
                    src.2 == Visibility::Public || (src.0 == tenant && allow_private),
                    "invisible row became a representative"
                );
            }
        }
    }
    // 他テナントの private だけのグループ "p" は tenant-a から見えない。
    let got = reps(
        &core
            .execute_sql(&ctx_for("tenant-a", true), BASE)
            .expect("ok"),
    );
    assert!(got.iter().all(|r| r.0.as_deref() != Some("p")));
}

/// LIMIT/OFFSET の全組み合わせがオラクルのスライスと一致する（満杯時の追い出しを含む。LIMIT 0 は仕様上 22023 なので 1 から）。
#[test]
fn distinct_on_limit_offset_pages_match_oracle_for_all_bounds() {
    let (core, _g) = build("sql25-don-page", &ROWS, None);
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let want = oracle(&visible(&ROWS, tenant, allow_private));
            let g = want.len();
            for order in ["", " ORDER BY grp"] {
                for limit in 1..=g + 1 {
                    for offset in 0..=g + 1 {
                        let sql = format!("{BASE}{order} LIMIT {limit} OFFSET {offset}");
                        let expected: Vec<Rep> =
                            want.iter().skip(offset).take(limit).cloned().collect();
                        let got = reps(&core.execute_sql(&ctx, &sql).expect("ok"));
                        assert_eq!(got, expected, "{tenant} {allow_private} {sql}");
                    }
                }
            }
        }
    }
}

/// LIMIT 省略・LIMIT ALL の上限はグループ数に掛かる（入力行数ではない）。
#[test]
fn distinct_on_limit_all_and_omitted_apply_cap_to_groups() {
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let g = oracle(&visible(&ROWS, tenant, allow_private)).len();
            let rows = visible(&ROWS, tenant, allow_private).len();
            assert!(rows > g);
            let ctx = ctx_for(tenant, allow_private);
            let (ok_core, _g1) = build("sql25-don-cap-ok", &ROWS, Some(g));
            let (ng_core, _g2) = build("sql25-don-cap-ng", &ROWS, Some(g - 1));
            for sql in [BASE.to_string(), format!("{BASE} LIMIT ALL")] {
                assert_eq!(
                    ok_core
                        .execute_sql(&ctx, &sql)
                        .expect("within cap")
                        .rows
                        .len(),
                    g,
                    "{sql}"
                );
                let err = ng_core.execute_sql(&ctx, &sql).expect_err("over cap");
                assert_eq!(err.wire_code(), "54000", "{sql}");
            }
        }
    }
}

/// 上限の判定は他テナントの不可視グループに影響されず、エラー文言は全テナントで同一（件数・値・テナント名を含まない）。
#[test]
fn distinct_on_cap_is_not_affected_by_invisible_groups() {
    // tenant-b の private だけのグループを多数足す。
    let names = ["z1", "z2", "z3", "z4", "z5", "z6"];
    let mut rows: Vec<Truth> = ROWS.to_vec();
    for (i, n) in names.iter().enumerate() {
        rows.push(("tenant-b", 100 + i as u64, Visibility::Private, Some(n)));
    }
    let g_a = oracle(&visible(&rows, "tenant-a", true)).len();
    let (core, _g) = build("sql25-don-cap-inv", &rows, Some(g_a));
    let ctx_a = ctx_for("tenant-a", true);
    assert_eq!(core.execute_sql(&ctx_a, BASE).expect("ok").rows.len(), g_a);
    let (tight, _g2) = build("sql25-don-cap-msg", &rows, Some(1));
    let mut seen: Option<String> = None;
    for tenant in TENANTS {
        let err = tight
            .execute_sql(&ctx_for(tenant, true), BASE)
            .expect_err("over cap");
        assert_eq!(err.wire_code(), "54000");
        let msg = err.to_string();
        assert!(!msg.contains("tenant") && !msg.contains("z1"), "{msg}");
        match &seen {
            Some(p) => assert_eq!(p, &msg),
            None => seen = Some(msg),
        }
    }
}

/// ORDER BY なしの未対応形も fail-closed（0A000／42601）で、結果は全テナントで同一。
#[test]
fn distinct_on_unsupported_forms_remain_fail_closed_without_order_by() {
    let (core, _g) = build("sql25-don-fc", &ROWS, None);
    for sql in [
        "SELECT DISTINCT ON (grp) grp FROM dv FOR UPDATE",
        "EXPLAIN SELECT DISTINCT ON (grp) grp FROM dv",
        "CREATE VIEW v AS SELECT DISTINCT ON (grp) grp FROM dv",
        "SELECT DISTINCT ON (grp) grp, COUNT(*) FROM dv GROUP BY grp",
        "SELECT DISTINCT ON (grp) grp, row_number() OVER () FROM dv",
        "SELECT grp FROM dv WHERE id IN (SELECT DISTINCT ON (grp) id FROM dv)",
    ] {
        let mut seen: Option<(String, String)> = None;
        for tenant in TENANTS {
            for allow_private in [false, true] {
                let err = core
                    .execute_sql(&ctx_for(tenant, allow_private), sql)
                    .expect_err(sql);
                let outcome = (err.wire_code().to_string(), err.to_string());
                assert!(
                    matches!(outcome.0.as_str(), "0A000" | "42601"),
                    "{sql}: {}",
                    outcome.0
                );
                match &seen {
                    Some(p) => assert_eq!(p, &outcome, "{sql}"),
                    None => seen = Some(outcome),
                }
            }
        }
    }
}
