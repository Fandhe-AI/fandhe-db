//! `LANGUAGE sql` 関数経由の読み書きを含む RLS 境界の機械検証（Issue #2221。
//! ポインタ: RLS-10・SQL-67 (5)、TABLE-12）。
//!
//! 関数本体の SQL は呼び出し元と同じ Tx・同じ `PolicyContext` で実行される（Issue #2218・#2220）。
//! 関数経由の読み書きが素の SQL と同じテナント境界に収まることを、`rls10_plpgsql_paths.rs` と
//! 同じ流儀（3 テナント × 可視性 × 経路 × flood の固定試行数）で固定する。
//!
//! - T1 双子オラクル: 関数経由の観測値が、同じ閲覧者が素の SQL で読んだ値、およびテスト側が持つ
//!   行の真実値（production の可視性判定を呼ばない）と一致する
//! - T2 flood 不変性: 他テナントが Private 行を大量に足しても観測値が変わらない
//! - T3 書き込みの不干渉: 閲覧者の関数内 `UPDATE`／`DELETE`／`INSERT` の前後で他テナントの行は不変で、
//!   影響行数は閲覧者が書ける行数と一致する（同一 `id` を他テナントが持っても衝突しない）
//! - T4 名前解決: 他テナントが同名・別本体の関数を持っていても、呼び出し結果は閲覧者自身の定義だけで決まる
//! - T7 エラー文面: 失敗した関数・他テナントの関数・存在しない関数の `wire_code` と文言が、他テナントの
//!   行の有無で変わらず、テナント名を含まない。失敗した書き込みは副作用ゼロ
//! - T8 試行総数と非空の組み合わせ数を固定し、空振りを防ぐ
//!
//! 呼び出しは DDL 権限の無い別セッション（カタログから `load_session_udfs`）で行う。経路は autocommit と
//! 明示 Tx、可視性は `PolicyContext::new`（Public のみ）と `with_visibilities`（Private 込み）の両方。
//! 呼び出し位置は実装済みの形（`SELECT f()`・`WHERE f()`・`FROM f()`）に限る。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
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

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

/// シード時の行の真実（オラクル用。production の可視性判定は呼ばない）。
#[derive(Clone, Copy)]
struct Truth {
    tenant: &'static str,
    visibility: Visibility,
}

fn allowed(row: &Truth, viewer: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer && allow_private,
    }
}

fn expected_visible(truths: &[Truth], viewer: &str, allow_private: bool) -> i64 {
    truths
        .iter()
        .filter(|t| allowed(t, viewer, allow_private))
        .count() as i64
}

fn expected_writable(truths: &[Truth], viewer: &str, allow_private: bool) -> i64 {
    truths
        .iter()
        .filter(|t| t.tenant == viewer && allowed(t, viewer, allow_private))
        .count() as i64
}

/// flood を含まない構成の真実値（T2 の比較対象。flood 行は閲覧者から不可視でなければならない）。
fn truths_without_flood() -> Vec<Truth> {
    let mut v = Vec::new();
    for &t in TENANTS.iter() {
        for visibility in [Visibility::Public, Visibility::Private] {
            v.push(Truth {
                tenant: t,
                visibility,
            });
        }
    }
    v
}

struct Fixture {
    core: EngineCore,
    truths: Vec<Truth>,
    _guard: CleanupGuard,
}

fn seed_row(
    storage: &Storage,
    tenant: &'static str,
    id: u64,
    visibility: Visibility,
    v: i32,
    truths: &mut Vec<Truth>,
) {
    let ctx = ctx_for(tenant, true);
    let op = OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id");
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text(tenant.to_string()),
            Value::Integer(v),
        ],
        &op,
    )
    .expect("seed row");
    truths.push(Truth { tenant, visibility });
}

/// 関数の定義。`real` は閲覧者（定義テナントが閲覧者のとき）の本体、`decoy` は他テナントが同名で持つ
/// 別本体（呼ばれたら値が変わるので名前解決のテナント越えを検知できる）。
struct FnDef {
    name: &'static str,
    returns: &'static str,
    language: &'static str,
    real: String,
    decoy: String,
}

fn create_fn(core: &EngineCore, tenant: &str, def: &FnDef, body: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    let sql = format!(
        "CREATE FUNCTION {}() RETURNS {} LANGUAGE {} AS $${body}$$",
        def.name, def.returns, def.language
    );
    core.execute_sql_in_session(&ctx_for(tenant, true), &mut s, &sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// 各テナントが id=1（Public）・id=2（Private）を持つ。`flood=true` は他テナント（`viewer` 以外）が
/// Private 行（`viewer` と同じ id を含む）を足した状態。全テナントが `defs` を同名で定義する。
fn fixture(label: &str, viewer: &str, flood: bool, defs: &[FnDef]) -> Fixture {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("owner", ColumnType::Text, false),
                ColumnDef::new("v", ColumnType::Integer, false),
            ],
        ))
        .expect("create docs");
    let mut truths = Vec::new();
    for &t in TENANTS.iter() {
        seed_row(&storage, t, 1, Visibility::Public, 10, &mut truths);
        seed_row(&storage, t, 2, Visibility::Private, 20, &mut truths);
        if flood && t != viewer {
            // id=500 は書き込み操作の INSERT が使う id（物理キーは (tenant, id)。TABLE-12）。
            for id in (3..=8u64).chain([500]) {
                seed_row(&storage, t, id, Visibility::Private, 20, &mut truths);
            }
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for &t in TENANTS.iter() {
        for def in defs {
            let body = if t == viewer { &def.real } else { &def.decoy };
            create_fn(&core, t, def, body);
        }
    }
    Fixture {
        core,
        truths,
        _guard: guard,
    }
}

fn cell_i64(c: &Cell) -> i64 {
    match c {
        Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        Cell::SignedInteger(v) => *v,
        other => panic!("unexpected cell {other:?}"),
    }
}

type Rows = Vec<Vec<Cell>>;

/// 呼び出し専用セッション（DDL 権限なし・カタログからロード）で `sql` を実行する。
fn call(
    core: &EngineCore,
    c: &PolicyContext,
    explicit_txn: bool,
    sql: &str,
) -> Result<Rows, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.load_session_udfs(c, &mut s)?;
    let outcome = if explicit_txn {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(c, &mut s, &mut txn, "BEGIN")?;
        let o = core.execute_sql_in_txn(c, &mut s, &mut txn, sql)?;
        core.execute_sql_in_txn(c, &mut s, &mut txn, "COMMIT")?;
        o
    } else {
        core.execute_sql_in_session(c, &mut s, sql)?
    };
    match outcome {
        SqlOutcome::Query(q) => Ok(q.rows.into_iter().map(|r| r.cells).collect()),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn single_i64(rows: &Rows, label: &str) -> i64 {
    assert_eq!(rows.len(), 1, "{label}: one row");
    cell_i64(&rows[0][0])
}

/// 素の SQL で同じ閲覧者が読んだ行数（双子オラクル）。
fn plain_count(core: &EngineCore, c: &PolicyContext) -> i64 {
    let r = core
        .execute_sql(c, "SELECT count(*) FROM docs")
        .expect("plain count");
    cell_i64(&r.rows[0].cells[0])
}

/// 各テナントが所有する行 `(owner, id, v)` を、所有者自身の視点（Private 込み）で全件読む。
fn owned_rows(core: &EngineCore) -> Vec<(String, u64, i64)> {
    let mut out = Vec::new();
    for &t in TENANTS.iter() {
        let r = core
            .execute_sql(&ctx_for(t, true), "SELECT owner, v FROM docs LIMIT 1000")
            .expect("owned read");
        for row in &r.rows {
            let owner = match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("unexpected owner cell {other:?}"),
            };
            if owner == t {
                out.push((owner, row.id, cell_i64(&row.cells[1])));
            }
        }
    }
    out.sort();
    out
}

fn read_defs() -> Vec<FnDef> {
    let decoy_int = "SELECT -1".to_string();
    vec![
        FnDef {
            name: "r_count",
            returns: "bigint",
            language: "sql",
            real: "SELECT count(*) FROM docs".to_string(),
            decoy: decoy_int.clone(),
        },
        FnDef {
            name: "r_nested",
            returns: "bigint",
            language: "sql",
            real: "SELECT r_count()".to_string(),
            decoy: decoy_int.clone(),
        },
        FnDef {
            name: "r_pl",
            returns: "bigint",
            language: "plpgsql",
            real: "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; RETURN c; END"
                .to_string(),
            decoy: "BEGIN RETURN -1; END".to_string(),
        },
        FnDef {
            name: "r_mixed",
            returns: "bigint",
            language: "sql",
            real: "SELECT r_pl()".to_string(),
            decoy: decoy_int.clone(),
        },
        FnDef {
            name: "r_rows",
            returns: "SETOF int",
            language: "sql",
            real: "SELECT v FROM docs LIMIT 1000".to_string(),
            decoy: decoy_int,
        },
        FnDef {
            name: "r_tab",
            returns: "TABLE(o text, v int)",
            language: "sql",
            real: "SELECT owner, v FROM docs LIMIT 1000".to_string(),
            decoy: "SELECT 'decoy', -1".to_string(),
        },
    ]
}

const READ_OPS: [&str; 6] = [
    "scalar",
    "nested-sql",
    "sql-to-plpgsql",
    "where",
    "from-setof",
    "from-table",
];

/// 読み取り操作 `op` を実行して観測値（見えた行数）を返す。`from-table` は行単位の可視性も検査する。
fn observe(
    core: &EngineCore,
    c: &PolicyContext,
    viewer: &str,
    allow_private: bool,
    explicit_txn: bool,
    op: &str,
    truth: i64,
) -> i64 {
    let label = format!("{op} viewer={viewer} private={allow_private} txn={explicit_txn}");
    let scalar = |sql: &str| single_i64(&call(core, c, explicit_txn, sql).expect(&label), &label);
    match op {
        "scalar" => scalar("SELECT r_count()"),
        "nested-sql" => scalar("SELECT r_nested()"),
        "sql-to-plpgsql" => scalar("SELECT r_mixed()"),
        "where" => {
            // 真実値ちょうどでだけ 1 行返り、±1 では 0 行（比較値が観測値そのものにならないよう両側を確かめる）。
            let n = |k: i64| {
                call(
                    core,
                    c,
                    explicit_txn,
                    &format!("SELECT 1 WHERE r_count() = {k}"),
                )
                .expect(&label)
                .len()
            };
            assert_eq!(n(truth + 1), 0, "{label}: truth+1");
            assert_eq!(n(truth - 1), 0, "{label}: truth-1");
            n(truth) as i64 * truth
        }
        "from-setof" => scalar("SELECT count(*) FROM r_rows()"),
        "from-table" => {
            let rows = call(core, c, explicit_txn, "SELECT o, v FROM r_tab()").expect(&label);
            for r in &rows {
                let owner = match &r[0] {
                    Cell::Text(s) => s.as_str(),
                    other => panic!("{label}: owner cell {other:?}"),
                };
                let v = cell_i64(&r[1]);
                // 自テナント以外の行は Public（v=10）のみ。Private 不許可の閲覧者は自行も Public のみ。
                if owner != viewer || !allow_private {
                    assert_eq!(v, 10, "{label}: invisible row leaked (owner={owner})");
                }
                assert!(TENANTS.contains(&owner), "{label}: decoy row leaked");
            }
            rows.len() as i64
        }
        other => panic!("unknown op {other}"),
    }
}

#[test]
fn rls10_sql_function_reads_match_plain_sql_and_the_truth_table() {
    let mut trials = 0u64;
    let mut non_empty = 0u64;
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            let fx = fixture("rls10-sqlfn-read", viewer, flood, &read_defs());
            for allow_private in [false, true] {
                for explicit_txn in [false, true] {
                    let c = ctx_for(viewer, allow_private);
                    for op in READ_OPS {
                        let truth = expected_visible(&fx.truths, viewer, allow_private);
                        let via_fn =
                            observe(&fx.core, &c, viewer, allow_private, explicit_txn, op, truth);
                        let key = format!(
                            "{op} viewer={viewer} private={allow_private} txn={explicit_txn} flood={flood}"
                        );
                        // T1: 素の SQL・真実値と一致する。
                        assert_eq!(via_fn, plain_count(&fx.core, &c), "{key}: twin");
                        assert_eq!(via_fn, truth, "{key}: truth");
                        // T2: flood の有無で値は同じ。
                        assert_eq!(
                            via_fn,
                            expected_visible(&truths_without_flood(), viewer, allow_private),
                            "{key}: flood invariance"
                        );
                        trials += 1;
                        if via_fn > 0 {
                            non_empty += 1;
                        }
                    }
                }
            }
        }
    }
    // T8: 2 flood × 3 viewer × 2 mode × 2 route × 6 op。
    assert_eq!(trials, 144);
    assert_eq!(non_empty, 144);
}

fn write_defs(viewer: &str) -> Vec<FnDef> {
    let decoy = "SELECT -1".to_string();
    vec![
        FnDef {
            name: "w_update",
            returns: "SETOF int",
            language: "sql",
            real: "UPDATE docs SET v = 99 WHERE v >= 0 RETURNING v".to_string(),
            decoy: decoy.clone(),
        },
        FnDef {
            name: "w_delete",
            returns: "SETOF int",
            language: "sql",
            real: "DELETE FROM docs WHERE v >= 0 RETURNING v".to_string(),
            decoy: decoy.clone(),
        },
        FnDef {
            name: "w_insert",
            returns: "SETOF int",
            language: "sql",
            real: format!(
                "INSERT INTO docs (id, embedding, owner, v) \
                 VALUES (500, '[0.5,0.5]', '{viewer}', 5) RETURNING v"
            ),
            decoy: decoy.clone(),
        },
        FnDef {
            name: "w_scalar",
            returns: "bigint",
            language: "sql",
            real: "UPDATE docs SET v = 99 WHERE v >= 0; \
                   SELECT count(*) FROM docs WHERE v = 99"
                .to_string(),
            decoy,
        },
    ]
}

const WRITE_OPS: [(&str, &str); 4] = [
    ("update", "SELECT * FROM w_update()"),
    ("delete", "SELECT * FROM w_delete()"),
    ("insert", "SELECT * FROM w_insert()"),
    ("scalar-update", "SELECT w_scalar()"),
];

#[test]
fn rls10_sql_function_writes_never_touch_other_tenants_rows() {
    let mut trials = 0u64;
    let mut effective = 0u64;
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            for allow_private in [false, true] {
                for explicit_txn in [false, true] {
                    for (tag, sql) in WRITE_OPS {
                        let fx = fixture("rls10-sqlfn-write", viewer, flood, &write_defs(viewer));
                        let before = owned_rows(&fx.core);
                        let c = ctx_for(viewer, allow_private);
                        let label = format!(
                            "{tag} viewer={viewer} private={allow_private} txn={explicit_txn} flood={flood}"
                        );
                        let result = call(&fx.core, &c, explicit_txn, sql);
                        if tag == "insert" && !allow_private {
                            // Private 不許可の文脈では、新規行（既定 Private）を RETURNING で読み返せず
                            // fail-closed で失敗する。素の SQL と同じ失敗で、行は 1 件も増えない。
                            let e = result.expect_err(&label);
                            let plain = fx
                                .core
                                .execute_sql_in_session(
                                    &c,
                                    &mut SessionState::default(),
                                    &format!(
                                        "INSERT INTO docs (id, embedding, owner, v) \
                                         VALUES (500, '[0.5,0.5]', '{viewer}', 5) RETURNING v"
                                    ),
                                )
                                .expect_err(&label);
                            assert_eq!(e.wire_code(), plain.wire_code(), "{label}: twin code");
                            assert_eq!(e.to_string(), plain.to_string(), "{label}: twin text");
                            assert_eq!(owned_rows(&fx.core), before, "{label}: rows changed");
                            trials += 1;
                            continue;
                        }
                        let rows = result.unwrap_or_else(|e| panic!("{label}: {e:?}"));
                        let after = owned_rows(&fx.core);
                        // T3: 他テナントが所有する行は不変。
                        let others = |r: &[(String, u64, i64)]| -> Vec<(String, u64, i64)> {
                            r.iter().filter(|x| x.0 != viewer).cloned().collect()
                        };
                        assert_eq!(others(&before), others(&after), "{label}: others changed");
                        let mine_before: Vec<_> = before.iter().filter(|r| r.0 == viewer).collect();
                        let mine_after: Vec<_> = after.iter().filter(|r| r.0 == viewer).collect();
                        let writable = expected_writable(&fx.truths, viewer, allow_private);
                        let affected = if tag == "scalar-update" {
                            single_i64(&rows, &label)
                        } else {
                            rows.len() as i64
                        };
                        match tag {
                            "update" | "scalar-update" => {
                                assert_eq!(affected, writable, "{label}: affected");
                                assert_eq!(mine_before.len(), mine_after.len(), "{label}");
                                let changed = mine_after.iter().filter(|r| r.2 == 99).count();
                                assert_eq!(changed as i64, writable, "{label}");
                            }
                            "delete" => {
                                assert_eq!(affected, writable, "{label}: affected");
                                assert_eq!(
                                    mine_before.len() as i64 - mine_after.len() as i64,
                                    writable,
                                    "{label}"
                                );
                            }
                            _ => {
                                assert_eq!(affected, 1, "{label}: affected");
                                assert_eq!(mine_after.len(), mine_before.len() + 1, "{label}");
                            }
                        }
                        trials += 1;
                        if affected > 0 {
                            effective += 1;
                        }
                    }
                }
            }
        }
    }
    // T8: 2 flood × 3 viewer × 2 mode × 2 route × 4 op。
    assert_eq!(trials, 96);
    // Private 不許可の INSERT（2 flood × 3 viewer × 2 route = 12 件）は fail-closed の失敗で、効いた書き込みではない。
    assert_eq!(effective, 84);
}

/// 失敗する関数は書き込みごと取り消され、エラー文面は他テナントの行の有無・閲覧者で変わらない。
/// 他テナントだけが持つ関数・存在しない関数は同じ `42883` と同じ文言になる（存在を漏らさない）。
#[test]
fn rls10_sql_function_errors_do_not_leak_and_failed_writes_leave_nothing() {
    let defs = || {
        vec![
            FnDef {
                name: "w_fail",
                returns: "int",
                language: "sql",
                real: "UPDATE docs SET v = 99 WHERE v >= 0; SELECT 1 / 0".to_string(),
                decoy: "SELECT -1".to_string(),
            },
            FnDef {
                name: "r_missing_table",
                returns: "bigint",
                language: "sql",
                real: "SELECT count(*) FROM no_such_table".to_string(),
                decoy: "SELECT -1".to_string(),
            },
        ]
    };
    let mut fail_texts = Vec::new();
    let mut missing_fn_texts = Vec::new();
    let mut missing_tbl_texts = Vec::new();
    let mut missing_set_texts = Vec::new();
    let mut checks = 0u64;
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            let fx = fixture("rls10-sqlfn-err", viewer, flood, &defs());
            // 閲覧者以外の 1 テナントだけが持つ関数（閲覧者からは不在と区別できてはならない）。
            let other = TENANTS.iter().find(|t| **t != viewer).expect("other");
            let mut ddl = SessionState::default();
            ddl.allow_ddl();
            for sql in [
                "CREATE FUNCTION only_other() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
                "CREATE FUNCTION only_other_set() RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 $$",
            ] {
                fx.core
                    .execute_sql_in_session(&ctx_for(other, true), &mut ddl, sql)
                    .expect("other tenant function");
            }
            for allow_private in [false, true] {
                for explicit_txn in [false, true] {
                    let c = ctx_for(viewer, allow_private);
                    let before = owned_rows(&fx.core);
                    let e = call(&fx.core, &c, explicit_txn, "SELECT w_fail()").expect_err("fail");
                    assert_eq!(e.wire_code(), "22012");
                    assert_eq!(owned_rows(&fx.core), before, "failed write left rows");
                    fail_texts.push(e.to_string());

                    let e = call(&fx.core, &c, explicit_txn, "SELECT r_missing_table()")
                        .expect_err("missing table");
                    assert_eq!(e.wire_code(), "42P01");
                    missing_tbl_texts.push(e.to_string());

                    for (other_sql, absent_sql) in [
                        ("SELECT only_other()", "SELECT nosuch_fn()"),
                        (
                            "SELECT * FROM only_other_set()",
                            "SELECT * FROM nosuch_set()",
                        ),
                    ] {
                        let eo = call(&fx.core, &c, explicit_txn, other_sql).expect_err("other");
                        let ea = call(&fx.core, &c, explicit_txn, absent_sql).expect_err("absent");
                        assert_eq!(eo.wire_code(), "42883");
                        assert_eq!(ea.wire_code(), "42883");
                        // 文言は呼び出した名前だけを含む。名前を除けば他テナントの関数と不在の関数は同一。
                        let norm = |e: &SqlSurfaceError, n: &str| e.to_string().replace(n, "<fn>");
                        let (on, an) = if other_sql.contains("_set") {
                            ("only_other_set", "nosuch_set")
                        } else {
                            ("only_other", "nosuch_fn")
                        };
                        assert_eq!(norm(&eo, on), norm(&ea, an), "existence leaked");
                        if other_sql.contains("_set") {
                            missing_set_texts.push(norm(&eo, on));
                        } else {
                            missing_fn_texts.push(norm(&eo, on));
                        }
                    }
                    checks += 1;
                }
            }
        }
    }
    for texts in [
        &fail_texts,
        &missing_tbl_texts,
        &missing_fn_texts,
        &missing_set_texts,
    ] {
        assert!(!texts.is_empty());
        for t in texts {
            for tenant in TENANTS {
                assert!(!t.contains(tenant), "tenant name leaked: {t}");
            }
        }
        assert!(
            texts.iter().all(|t| t == &texts[0]),
            "text varies: {texts:?}"
        );
    }
    // T8: 2 flood × 3 viewer × 2 mode × 2 route。
    assert_eq!(checks, 24);
}
