//! PL/pgSQL 関数本体（crate 内入口 `run_plpgsql_*`）の RLS 境界・資源上限の検証（Issue #2229。
//! ポインタ: RLS-10・SQL-68、TABLE-12）。
//!
//! `plpgsql_host_tests.rs` の個別ケースを、3 テナント × 可視性（Public のみ／Private 込み）×
//! 実行経路（autocommit／明示 Tx）× flood（他テナントの Private 行）の網羅へ拡張し、真実値との照合
//! （production の可視性判定を呼ばない）・他テナント行の不変・揮発性ゲート・上限超過の fail-closed
//! （部分永続なし・上限ちょうどは成功）を固定する。公開 API から関数本体へ到達する経路は
//! #2217・#2218 待ちのため、`DO` 経由の検証は `tests/table_rls/rls10_plpgsql_paths.rs`・
//! `tests/sql_t40_up/sql68_do_limits_ledger.rs` が担う。

use super::plpgsql_host::{PlCallOptions, PlInvocation};
use super::plpgsql_host_tests::{as_i64, call, ctx};
use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::plpgsql::ast::{ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::interp::{CallOutcome, ReturnSpec};
use crate::plpgsql::parse_body;
use crate::plpgsql_limits::PlpgsqlLimits;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        ctx(tenant)
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

#[derive(Clone, Copy)]
struct Truth {
    tenant: &'static str,
    visibility: Visibility,
}

fn allowed(t: &Truth, viewer: &str, allow_private: bool) -> bool {
    match t.visibility {
        Visibility::Public => true,
        Visibility::Private => t.tenant == viewer && allow_private,
    }
}

fn seed_row(
    storage: &Storage,
    tenant: &'static str,
    id: u64,
    visibility: Visibility,
    truths: &mut Vec<Truth>,
) {
    let op = OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id");
    crate::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text(tenant.to_string()),
            Value::Integer(10),
            Value::Null,
            Value::Null,
        ],
        &op,
    )
    .expect("seed row");
    truths.push(Truth { tenant, visibility });
}

/// 各テナントが id=1（Public）・id=2（Private）を持つ。`flood` は `viewer` 以外が Private 行を足す。
fn fixture(
    label: &str,
    viewer: &str,
    flood: bool,
    limits: Option<PlpgsqlLimits>,
) -> (EngineCore, Vec<Truth>, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("f", ColumnType::Double, true),
            ],
        ))
        .expect("create table");
    let mut truths = Vec::new();
    for &t in TENANTS.iter() {
        seed_row(&storage, t, 1, Visibility::Public, &mut truths);
        seed_row(&storage, t, 2, Visibility::Private, &mut truths);
        if flood && t != viewer {
            for id in 3..=8u64 {
                seed_row(&storage, t, id, Visibility::Private, &mut truths);
            }
        }
    }
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_plpgsql_limits(l);
    }
    (core, truths, CleanupGuard(path))
}

fn visible(truths: &[Truth], viewer: &str, allow_private: bool) -> i64 {
    truths
        .iter()
        .filter(|t| allowed(t, viewer, allow_private))
        .count() as i64
}

fn writable(truths: &[Truth], viewer: &str, allow_private: bool) -> i64 {
    truths
        .iter()
        .filter(|t| t.tenant == viewer && allowed(t, viewer, allow_private))
        .count() as i64
}

/// 素の SQL で数えた、`c` から見える行数。
fn plain_count(core: &EngineCore, c: &PolicyContext) -> i64 {
    let mut s = SessionState::default();
    let SqlOutcome::Query(q) = core
        .execute_sql_in_session(c, &mut s, "SELECT COUNT(*) FROM docs")
        .expect("count")
    else {
        panic!("expected query");
    };
    match q.rows[0].cells[0] {
        Cell::Integer(v) => i64::try_from(v).expect("fits"),
        ref o => panic!("unexpected {o:?}"),
    }
}

/// `t` が所有する行数（`lang` にシード時の所有者名を入れてあるため、所有者自身の視点で数える）。
fn owned_by(core: &EngineCore, t: &str) -> i64 {
    let mut s = SessionState::default();
    let SqlOutcome::Query(q) = core
        .execute_sql_in_session(
            &ctx(t),
            &mut s,
            &format!("SELECT COUNT(*) FROM docs WHERE lang = '{t}'"),
        )
        .expect("owned count")
    else {
        panic!("expected query");
    };
    match q.rows[0].cells[0] {
        Cell::Integer(v) => i64::try_from(v).expect("fits"),
        ref o => panic!("unexpected {o:?}"),
    }
}

/// 各テナントが所有する行 `(owner, id, n)` を、所有者自身の視点（Private 込み）で全件読んで並べ替える。
fn owned_rows(core: &EngineCore) -> Vec<(String, u64, i64)> {
    let mut out = Vec::new();
    for &t in TENANTS.iter() {
        let mut s = SessionState::default();
        let SqlOutcome::Query(q) = core
            .execute_sql_in_session(&ctx(t), &mut s, "SELECT lang, n FROM docs LIMIT 1000")
            .expect("owned rows")
        else {
            panic!("expected query");
        };
        for row in &q.rows {
            let (Cell::Text(owner), Cell::SignedInteger(n)) = (&row.cells[0], &row.cells[1]) else {
                panic!("unexpected cells {:?}", row.cells);
            };
            if owner == t {
                out.push((owner.clone(), row.id, *n));
            }
        }
    }
    out.sort();
    out
}

fn owned_total(core: &EngineCore) -> i64 {
    TENANTS.iter().map(|t| owned_by(core, t)).sum()
}

/// 関数本体を、autocommit または明示 Tx（`BEGIN`／`COMMIT`）で実行する。
fn run_body(
    core: &EngineCore,
    c: &PolicyContext,
    explicit_txn: bool,
    src: &str,
) -> Result<CallOutcome, SqlSurfaceError> {
    let invocation = call(src);
    if explicit_txn {
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(c, &mut s, &mut txn, "BEGIN")?;
        let out = invocation.in_txn(core, c, &mut s, &mut txn)?;
        core.execute_sql_in_txn(c, &mut s, &mut txn, "COMMIT")?;
        Ok(out)
    } else {
        invocation.autocommit(core, c)
    }
}

#[test]
fn function_body_reads_and_writes_follow_the_truth_table() {
    let mut trials = 0u64;
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            for allow_private in [false, true] {
                for explicit_txn in [false, true] {
                    let label = format!(
                        "viewer={viewer} private={allow_private} txn={explicit_txn} flood={flood}"
                    );
                    // 読み取り: SELECT INTO・FOR・PERFORM が真実値・素の SQL と一致する。
                    let (core, truths, _g) = fixture("plpgsql-bnd-read", viewer, flood, None);
                    let c = ctx_for(viewer, allow_private);
                    let expect = visible(&truths, viewer, allow_private);
                    assert_eq!(plain_count(&core, &c), expect, "{label}: twin");
                    for src in [
                        "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; RETURN c; END",
                        "DECLARE c bigint := 0; r record; BEGIN \
                         FOR r IN SELECT n FROM docs LIMIT 1000 LOOP c := c + 1; END LOOP; \
                         RETURN c; END",
                        "DECLARE c bigint; BEGIN PERFORM 1 FROM docs; \
                         GET DIAGNOSTICS c = ROW_COUNT; RETURN c; END",
                    ] {
                        let out = run_body(&core, &c, explicit_txn, src).expect("read body");
                        assert_eq!(as_i64(&out), expect, "{label}: {src}");
                        trials += 1;
                    }
                    // 書き込み: ROW_COUNT は自テナントが書ける行数、他テナントの行は不変。
                    for (verb, src) in [
                        (
                            "update",
                            "DECLARE rc int; BEGIN UPDATE docs SET n = 99 WHERE n >= 0; \
                             GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END",
                        ),
                        (
                            "delete",
                            "DECLARE rc int; BEGIN DELETE FROM docs WHERE n >= 0; \
                             GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END",
                        ),
                    ] {
                        let (core, truths, _g) = fixture("plpgsql-bnd-write", viewer, flood, None);
                        let before = owned_rows(&core);
                        let out = run_body(&core, &c, explicit_txn, src).expect("write body");
                        let w = writable(&truths, viewer, allow_private);
                        assert_eq!(as_i64(&out), w, "{label}: {verb} ROW_COUNT");
                        let after = owned_rows(&core);
                        // 他テナントが所有する行は（所有者・行 ID・n とも）1 行も変わらない。
                        let others = |rows: &[(String, u64, i64)]| -> Vec<(String, u64, i64)> {
                            rows.iter().filter(|r| r.0 != viewer).cloned().collect()
                        };
                        assert_eq!(
                            others(&before),
                            others(&after),
                            "{label}: {verb} touched others"
                        );
                        let mine = |rows: &[(String, u64, i64)]| -> Vec<(String, u64, i64)> {
                            rows.iter().filter(|r| r.0 == viewer).cloned().collect()
                        };
                        let (mine_before, mine_after) = (mine(&before), mine(&after));
                        if verb == "delete" {
                            assert_eq!(
                                mine_before.len() as i64 - mine_after.len() as i64,
                                w,
                                "{label}: {verb} own rows"
                            );
                            assert!(
                                mine_after.iter().all(|r| mine_before.contains(r)),
                                "{label}: {verb} survivors unchanged"
                            );
                        } else {
                            let ids = |rows: &[(String, u64, i64)]| -> Vec<u64> {
                                rows.iter().map(|r| r.1).collect()
                            };
                            assert_eq!(ids(&mine_before), ids(&mine_after), "{label}: {verb} ids");
                            let updated = mine_after.iter().filter(|r| r.2 == 99).count() as i64;
                            assert_eq!(updated, w, "{label}: {verb} rows with n = 99");
                            assert!(
                                mine_after.iter().all(|r| r.2 == 99 || r.2 == 10),
                                "{label}: {verb} unexpected values"
                            );
                        }
                        trials += 1;
                    }
                }
            }
        }
    }
    // 試行総数（2 flood × 3 viewer × 2 mode × 2 route × (3 read + 2 write)）を固定して空振りを防ぐ。
    assert_eq!(trials, 120);
}

#[test]
fn set_returning_body_only_returns_visible_rows() {
    for &viewer in TENANTS.iter() {
        for allow_private in [false, true] {
            let (core, truths, _g) = fixture("plpgsql-bnd-setof", viewer, true, None);
            let c = ctx_for(viewer, allow_private);
            let out = call("BEGIN RETURN QUERY SELECT id FROM docs LIMIT 100; END")
                .set_of(ColumnType::BigInt)
                .autocommit(&core, &c)
                .expect("return query");
            let CallOutcome::Rows(rows) = out else {
                panic!("expected rows");
            };
            assert_eq!(rows.len() as i64, visible(&truths, viewer, allow_private));
        }
    }
}

#[test]
fn read_only_function_cannot_write_across_tenants_and_leaves_no_trace() {
    for &viewer in TENANTS.iter() {
        let (core, _t, _g) = fixture("plpgsql-bnd-ro", viewer, true, None);
        let c = ctx_for(viewer, true);
        let before = owned_total(&core);
        // 他テナントの行を指す UPDATE／DELETE は、揮発性ゲートで本体の実行前に拒否される。
        for src in [
            "BEGIN UPDATE docs SET n = 1 WHERE id = 1; RETURN 1; END",
            "BEGIN DELETE FROM docs WHERE id = 1; RETURN 1; END",
        ] {
            let e = call(src)
                .read_only()
                .autocommit(&core, &c)
                .expect_err("read-only function");
            assert_eq!(e.wire_code(), "0A000", "{src}");
        }
        assert_eq!(owned_total(&core), before);
    }
}

#[test]
fn missing_relation_error_does_not_depend_on_other_tenants_rows() {
    let mut texts = Vec::new();
    for flood in [false, true] {
        let (core, _t, _g) = fixture("plpgsql-bnd-err", "tenant-a", flood, None);
        let e = call("DECLARE r nosuch%ROWTYPE; BEGIN RETURN 1; END")
            .autocommit(&core, &ctx("tenant-a"))
            .expect_err("missing relation");
        assert_eq!(e.wire_code(), "42P01");
        let text = e.to_string();
        for t in TENANTS {
            assert!(!text.contains(t), "tenant name leaked: {text}");
        }
        texts.push(text);
    }
    assert_eq!(texts[0], texts[1]);
}

// ---- 資源上限（関数本体の入口）----

fn small() -> PlpgsqlLimits {
    // 文数 6・ループ 4・入れ子 3・本体 400 バイト・呼び出し深さ 2。
    PlpgsqlLimits::new(6, 4, 3, 400, 2)
}

/// 核の上限で字句・構文段の上限も揃えて本体を準備し、実行する（`Call::prepared` は既定上限）。
fn run_with_core_limits(
    core: &EngineCore,
    c: &PolicyContext,
    src: &str,
    explicit_txn: bool,
) -> Result<CallOutcome, SqlSurfaceError> {
    let l = core.plpgsql_limits();
    let opts = ParseOptions::new(Vec::new(), ReturnShape::Scalar)
        .with_limits(l.max_nesting_depth(), l.max_body_bytes());
    let mut body = parse_body(src, &opts)?;
    validate_embedded(&mut body, core)?;
    let ret = ReturnSpec {
        shape: ReturnShape::Scalar,
        ty: Some(ColumnType::Integer),
    };
    let inv = || PlInvocation {
        body: &body,
        args: Vec::new(),
        ret: &ret,
        opts: PlCallOptions { may_write: true },
    };
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    if explicit_txn {
        core.execute_sql_in_txn(c, &mut s, &mut txn, "BEGIN")?;
        let out = core.run_plpgsql_in_txn(c, &mut s, &mut txn, inv())?;
        core.execute_sql_in_txn(c, &mut s, &mut txn, "COMMIT")?;
        Ok(out)
    } else {
        core.run_plpgsql_autocommit(c, &mut s, &mut txn, inv())
    }
}

#[test]
fn function_limits_fail_closed_without_partial_persistence() {
    for explicit_txn in [false, true] {
        let (core, _t, _g) = fixture("plpgsql-bnd-lim", "tenant-a", false, Some(small()));
        let c = ctx("tenant-a");
        let own = plain_count(&core, &c);
        // 「文数」「ループ」「入れ子」「本体長」の超過は、いずれも書き込みを 1 行も残さない。
        // 同一 id への INSERT は 2 回目以降が重複になりうるため、行を残さないことは件数で見る。
        let over_stmts = format!(
            "BEGIN {} RETURN 1; END",
            "UPDATE docs SET n = 5 WHERE id = 1; ".repeat(7)
        );
        let over_loop =
            "BEGIN FOR i IN 1..5 LOOP UPDATE docs SET n = 5 WHERE id = 1; END LOOP; RETURN 1; END";
        let over_nest = "BEGIN BEGIN BEGIN BEGIN UPDATE docs SET n = 5 WHERE id = 1; END; END; END; END; RETURN 1; END";
        let over_body = format!("BEGIN {} RETURN 1; END", " ".repeat(500));
        for (src, code) in [
            (over_stmts.as_str(), "54000"),
            (over_loop, "54000"),
            (over_nest, "54001"),
            (over_body.as_str(), "54000"),
        ] {
            let e = run_with_core_limits(&core, &c, src, explicit_txn).expect_err(src);
            assert_eq!(e.wire_code(), code, "{src}");
            assert_eq!(plain_count(&core, &c), own, "no partial rows: {src}");
            // 変更は残らない（n は 10 のまま）。
            let mut s = SessionState::default();
            let SqlOutcome::Query(q) = core
                .execute_sql_in_session(&c, &mut s, "SELECT n FROM docs WHERE id = 1")
                .expect("read")
            else {
                panic!("expected query");
            };
            assert!(
                q.rows
                    .iter()
                    .all(|r| matches!(r.cells[0], Cell::SignedInteger(10) | Cell::Integer(10))),
                "{src}"
            );
        }
        // 上限ちょうどの文数（6）・ループ（4）は成功する（陽性対照）。
        let exact = format!(
            "BEGIN {} RETURN 1; END",
            "UPDATE docs SET n = 10 WHERE id = 1; ".repeat(5)
        );
        run_with_core_limits(&core, &c, &exact, explicit_txn).expect("exactly the statement limit");
        let exact_loop =
            "BEGIN FOR i IN 1..4 LOOP UPDATE docs SET n = 10 WHERE id = 1; END LOOP; RETURN 1; END";
        run_with_core_limits(&core, &c, exact_loop, explicit_txn).expect("exactly the loop limit");
    }
}
