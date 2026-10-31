//! PL/pgSQL（`DO` ブロック）経由の読み書きを含む RLS 境界の機械検証（Issue #2229。
//! ポインタ: RLS-10・SQL-68、TABLE-12）。
//!
//! 本体の SQL は呼び出し元と同じ Tx・同じ `PolicyContext` で実行される（Issue #2226）ため、
//! 本体経由の読み書きが素の SQL と同じテナント境界に収まることを、`rls10_gin_index_paths.rs` と
//! 同じ流儀で固定する。`DO` は結果を返さないので、読み取りの観測は「本体が読んだ値を閲覧テナントの
//! 観測表 `probe` へ INSERT する」形で行う。
//!
//! - T1 双子オラクル: 本体経由の観測値が、同じ閲覧者が素の SQL で読んだ値と一致し、さらに
//!   テスト側が持つ行の真実値（production の可視性判定を呼ばない）と一致する
//! - T2 flood 不変性: 他テナントが Private 行を大量に足しても、閲覧者の観測値・自行の事後状態が一致する
//! - T3 書き込みの不干渉: 閲覧者の本体内 `UPDATE`／`DELETE`／`INSERT` の前後で、他テナントが所有する
//!   行は不変で、`ROW_COUNT` は閲覧者が書ける行数と一致する（同一 `id` を他テナントが持っても衝突しない）
//! - T7 エラー文面: 存在しない表を `%ROWTYPE` で参照した `wire_code`・文言が、他テナントの行の有無で
//!   変わらず、テナント名を含まない
//! - T8 試行総数と非空の組み合わせ数を固定し、空振りを防ぐ
//!
//! 実行経路は autocommit と明示 Tx（`BEGIN`／`COMMIT`）、可視性は `PolicyContext::new`（Public のみ）と
//! `with_visibilities`（Private 込み）の両方。`DO` の本体は単一引用リテラルのみ（#2216 待ち）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
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

/// 各テナントが id=1（Public）・id=2（Private）を持つ。`flood=true` は他テナント（`viewer` 以外）が
/// Private 行（`viewer` と同じ id・値を含む）を足した状態（閲覧者からは不可視）。
fn fixture(label: &str, viewer: &str, flood: bool) -> Fixture {
    let path = unique_db_path(label);
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
            for id in 3..=8u64 {
                seed_row(&storage, t, id, Visibility::Private, 20, &mut truths);
            }
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx_for(viewer, true),
        &mut s,
        "CREATE TABLE probe (tag TEXT, v BIGINT)",
    )
    .expect("create probe");
    Fixture {
        core,
        truths,
        _guard: CleanupGuard(path),
    }
}

fn do_lit(body: &str) -> String {
    format!("DO '{}'", body.replace('\'', "''"))
}

/// 本体 `body` を `DO` として、autocommit または明示 Tx（`BEGIN`／`COMMIT`）で実行する。
fn run_do(
    core: &EngineCore,
    c: &PolicyContext,
    explicit_txn: bool,
    body: &str,
) -> Result<(), SqlSurfaceError> {
    let mut s = SessionState::default();
    let sql = do_lit(body);
    if explicit_txn {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(c, &mut s, &mut txn, "BEGIN")?;
        core.execute_sql_in_txn(c, &mut s, &mut txn, &sql)?;
        core.execute_sql_in_txn(c, &mut s, &mut txn, "COMMIT")?;
        Ok(())
    } else {
        core.execute_sql_in_session(c, &mut s, &sql).map(|_| ())
    }
}

fn cell_i64(c: &Cell) -> i64 {
    match c {
        Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        Cell::SignedInteger(v) => *v,
        other => panic!("unexpected cell {other:?}"),
    }
}

/// `probe` の `tag` の値（閲覧者本人の Private 込みの視点で読む。他テナントの `probe` は見えない）。
fn probe_value(core: &EngineCore, viewer: &str, tag: &str) -> i64 {
    let r = core
        .execute_sql(
            &ctx_for(viewer, true),
            &format!("SELECT v FROM probe WHERE tag = '{tag}'"),
        )
        .expect("probe read");
    assert_eq!(r.rows.len(), 1, "exactly one probe row for {tag}");
    cell_i64(&r.rows[0].cells[0])
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

/// 読み取り操作（本体が読んだ値を `probe` へ書く）。`(tag, 本体)`。
fn read_ops() -> [(&'static str, &'static str); 4] {
    [
        (
            "select-into",
            "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; \
             INSERT INTO probe (tag, v) VALUES ('select-into', c); END",
        ),
        (
            "for-loop",
            "DECLARE c bigint := 0; r record; BEGIN \
             FOR r IN SELECT v FROM docs LIMIT 1000 LOOP c := c + 1; END LOOP; \
             INSERT INTO probe (tag, v) VALUES ('for-loop', c); END",
        ),
        (
            "perform",
            "DECLARE c int; BEGIN PERFORM 1 FROM docs; GET DIAGNOSTICS c = ROW_COUNT; \
             INSERT INTO probe (tag, v) VALUES ('perform', c); END",
        ),
        (
            "execute",
            "DECLARE c bigint; BEGIN EXECUTE 'SELECT count(*) FROM docs' INTO c; \
             INSERT INTO probe (tag, v) VALUES ('execute', c); END",
        ),
    ]
}

/// 書き込み操作。`(tag, 本体)`。本体は `ROW_COUNT` を `probe` へ書く。
fn write_ops() -> [(&'static str, &'static str); 3] {
    [
        (
            "update",
            "DECLARE rc int; BEGIN UPDATE docs SET v = 99 WHERE v >= 0; \
             GET DIAGNOSTICS rc = ROW_COUNT; INSERT INTO probe (tag, v) VALUES ('update', rc); END",
        ),
        (
            "delete",
            "DECLARE rc int; BEGIN DELETE FROM docs WHERE v >= 0; \
             GET DIAGNOSTICS rc = ROW_COUNT; INSERT INTO probe (tag, v) VALUES ('delete', rc); END",
        ),
        (
            "insert",
            // 他テナントも同じ id=1 を持つ（物理キーは (tenant, id)。TABLE-12）。
            "DECLARE rc int; BEGIN INSERT INTO docs (id, embedding, owner, v) \
             VALUES (500, '[0.5,0.5]', '@VIEWER@', 5); GET DIAGNOSTICS rc = ROW_COUNT; \
             INSERT INTO probe (tag, v) VALUES ('insert', rc); END",
        ),
    ]
}

#[test]
fn rls10_plpgsql_do_reads_match_plain_sql_and_the_truth_table() {
    let mut trials = 0u64;
    let mut non_empty = 0u64;
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            let fx = fixture("rls10-plpgsql-read", viewer, flood);
            for allow_private in [false, true] {
                for explicit_txn in [false, true] {
                    let c = ctx_for(viewer, allow_private);
                    for (tag, body) in read_ops() {
                        // 同じ `probe` へ tag を変えて書けるよう、経路・モードごとに本体の tag を差し替える。
                        let key = format!("{tag}-{allow_private}-{explicit_txn}");
                        let body = body.replace(&format!("'{tag}'"), &format!("'{key}'"));
                        run_do(&fx.core, &c, explicit_txn, &body).expect("do read");
                        let via_do = probe_value(&fx.core, viewer, &key);
                        // T1: 素の SQL・真実値と一致する。
                        assert_eq!(via_do, plain_count(&fx.core, &c), "{key} twin");
                        let truth = expected_visible(&fx.truths, viewer, allow_private);
                        assert_eq!(via_do, truth, "{key} truth viewer={viewer} flood={flood}");
                        // T2: flood の有無で値は同じ（真実値は flood でも不変＝他テナントの Private は不可視）。
                        assert_eq!(
                            via_do,
                            expected_visible(
                                &fixture_truths_without_flood(),
                                viewer,
                                allow_private
                            ),
                            "{key} flood invariance"
                        );
                        trials += 1;
                        if via_do > 0 {
                            non_empty += 1;
                        }
                    }
                }
            }
        }
    }
    // T8: 試行総数（2 flood × 3 viewer × 2 mode × 2 route × 4 op）と非空の組み合わせ数を固定する。
    assert_eq!(trials, 96);
    assert_eq!(non_empty, 96);
}

/// flood を含まない構成の真実値（T2 の比較対象。flood 行は閲覧者から不可視でなければならない）。
fn fixture_truths_without_flood() -> Vec<Truth> {
    let mut v = Vec::new();
    for &t in TENANTS.iter() {
        v.push(Truth {
            tenant: t,
            visibility: Visibility::Public,
        });
        v.push(Truth {
            tenant: t,
            visibility: Visibility::Private,
        });
    }
    v
}

#[test]
fn rls10_plpgsql_do_writes_never_touch_other_tenants_rows() {
    let mut trials = 0u64;
    let mut effective = 0u64;
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            for allow_private in [false, true] {
                for explicit_txn in [false, true] {
                    for (tag, body) in write_ops() {
                        let fx = fixture("rls10-plpgsql-write", viewer, flood);
                        let before = owned_rows(&fx.core);
                        let c = ctx_for(viewer, allow_private);
                        run_do(
                            &fx.core,
                            &c,
                            explicit_txn,
                            &body.replace("@VIEWER@", viewer),
                        )
                        .expect("do write");
                        let after = owned_rows(&fx.core);
                        let rc = probe_value(&fx.core, viewer, tag);
                        let label = format!(
                            "{tag} viewer={viewer} private={allow_private} txn={explicit_txn} flood={flood}"
                        );
                        // T3: 他テナントが所有する行は不変。
                        let others = |rows: &[(String, u64, i64)]| -> Vec<(String, u64, i64)> {
                            rows.iter().filter(|r| r.0 != viewer).cloned().collect()
                        };
                        assert_eq!(others(&before), others(&after), "{label}: others changed");
                        let mine_before: Vec<_> = before.iter().filter(|r| r.0 == viewer).collect();
                        let mine_after: Vec<_> = after.iter().filter(|r| r.0 == viewer).collect();
                        let writable = expected_writable(&fx.truths, viewer, allow_private);
                        match tag {
                            "update" => {
                                assert_eq!(rc, writable, "{label}: ROW_COUNT");
                                assert_eq!(mine_before.len(), mine_after.len(), "{label}");
                                let changed = mine_after.iter().filter(|r| r.2 == 99).count();
                                assert_eq!(changed as i64, writable, "{label}");
                            }
                            "delete" => {
                                assert_eq!(rc, writable, "{label}: ROW_COUNT");
                                assert_eq!(
                                    mine_before.len() as i64 - mine_after.len() as i64,
                                    writable,
                                    "{label}"
                                );
                            }
                            _ => {
                                assert_eq!(rc, 1, "{label}: ROW_COUNT");
                                assert_eq!(mine_after.len(), mine_before.len() + 1, "{label}");
                            }
                        }
                        trials += 1;
                        if rc > 0 {
                            effective += 1;
                        }
                    }
                }
            }
        }
    }
    // T8: 試行総数（2 flood × 3 viewer × 2 mode × 2 route × 3 op）と、書き込みが効いた組み合わせ数。
    assert_eq!(trials, 72);
    assert_eq!(effective, 72);
}

#[test]
fn rls10_plpgsql_do_error_text_does_not_depend_on_other_tenants_rows() {
    let mut texts = Vec::new();
    for flood in [false, true] {
        let fx = fixture("rls10-plpgsql-err", "tenant-a", flood);
        let c = ctx_for("tenant-a", true);
        let mut s = SessionState::default();
        let e = fx
            .core
            .execute_sql_in_session(
                &c,
                &mut s,
                &do_lit("DECLARE r nosuch%ROWTYPE; BEGIN NULL; END"),
            )
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
