//! `RETURNING` の返却行数・影響行数・物理的に変わった行の 3 者一致を、全 DML 経路
//! ×可視性 2 モードのマトリクスで固定する結合テスト（Issue #1255。ポインタ:
//! SQL-21・RLS-7〜RLS-11・TASK-193。親 #1250、前提の是正は #1252／#1253／#1254）。
//!
//! 役割: `sql_returning.rs`（機能単位）・`sql_upsert.rs`（UPSERT 単体）・
//! `rls10_write_constraint_paths.rs`（3 テナント越境の統一マトリクス）が個別に持つ
//! 契約を、「`PolicyContext::new`（Public のみ）で engine を直接呼んでも返却と影響行数が
//! ずれない」という 1 点に絞って横断固定する。engine 直呼びの `EngineCore::
//! execute_sql_in_session`／`execute_sql_in_txn` が production 経路そのもので、
//! wire-server の接続ハンドラはこの入口を呼ぶ（認証経路は RLS-11 により Public＋Private
//! なので、本テストの A モードが代替する）。
//!
//! 検査の骨子（独立オラクル）: `rows_affected` と `result.rows.len()` は同じ書き込み経路
//! から出るため、両者の比較だけでは「同じように誤る」退行を検出できない。そこで
//! 事前・事後の物理スナップショット（`Storage::scan_table_page`。RLS に依存しない）の
//! 差分を真の値とし、期待値（影響行数・`id` 集合・`wire_code`）はテスト側の定数で持つ。
//!
//! - 成功セル: `rows_affected == 返却行数 == 物理変更行数`、返却 `id` 集合 == 物理変更
//!   `id` 集合（順序が定義された経路は順序も照合）
//! - エラーセル（fail-closed）: `wire_code` 一致・物理変更 0・台帳未消費（同じ
//!   `operation_id` を Public＋Private モードで再実行すると成功し、成功セルの判定を満たす）
//! - 全セル: 他テナントの行は返却にも物理状態にも現れない（同一 id・同一 lang の他テナント
//!   Public 行を置き、越境を実効的に検出する）
//!
//! 明示トランザクション内の `RETURNING` 付き UPDATE・述語形 DELETE・UPSERT は `0A000`
//! で拒否される既知の未対応領域のため、マトリクスには入れない。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const VIEWER: &str = "tenant-a";
const OTHER: &str = "tenant-b";
/// 総セル数（空振り防止。セルを増減したら本定数も更新する）。
const CELL_COUNT: usize = 23;

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

// ---------- フィクスチャ（テスト側の真実値） ----------

/// `(id, public, lang)`。閲覧側・他テナントの共通行（同一 id・同一 lang）。
const COMMON: [(u64, bool, &str); 5] = [
    (1, false, "ja"),
    (2, false, "ja"),
    (5, true, "ja"),
    (6, true, "ja"),
    (7, false, "en"),
];
/// 他テナントだけが持つ行（閲覧側から見て「他テナントだけに存在する id」）。
const OTHER_ONLY: (u64, bool, &str) = (9, true, "ja");

fn token(tenant: &str, id: u64) -> String {
    format!("tok-{tenant}-{id}")
}

fn seed(path: &Path) {
    let storage = Storage::open(path).expect("open");
    for tenant in [VIEWER, OTHER] {
        let ctx = ctx_for(tenant, true);
        let mut rows: Vec<(u64, bool, &str)> = COMMON.to_vec();
        if tenant == OTHER {
            rows.push(OTHER_ONLY);
        }
        for (id, public, lang) in rows {
            engine::tenant::insert_typed_row(
                &storage,
                TABLE,
                &ctx,
                id,
                if public {
                    Visibility::Public
                } else {
                    Visibility::Private
                },
                &[
                    Value::Vector(vec![0.1, 0.2]),
                    Value::Text(lang.to_string()),
                    Value::Text(token(tenant, id)),
                ],
                &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id"),
            )
            .unwrap_or_else(|e| panic!("seed {tenant} {id}: {e:?}"));
        }
    }
}

// ---------- 物理スナップショット ----------

type SnapValue = (bool, Vec<f32>, Vec<u8>);
type Snapshot = BTreeMap<(String, u64), SnapValue>;

fn snapshot(path: &Path) -> Snapshot {
    let storage = Storage::open(path).expect("open for snapshot");
    let mut out = Snapshot::new();
    let mut after: Option<(String, u64)> = None;
    loop {
        let (page, next) = storage
            .scan_table_page(
                TABLE,
                after.as_ref().map(|(t, id)| (t.as_str(), *id)),
                10_000,
            )
            .expect("scan_table_page");
        for r in page {
            out.insert(
                (r.tenant_id.clone(), r.id),
                (
                    matches!(r.visibility, Visibility::Public),
                    r.embedding.clone(),
                    r.metadata.clone(),
                ),
            );
        }
        match next {
            Some(c) => after = Some(c),
            None => break,
        }
    }
    out
}

fn partition(snap: &Snapshot, tenant: &str) -> Snapshot {
    snap.iter()
        .filter(|((t, _), _)| t == tenant)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// 閲覧テナントで物理的に変わった（追加・削除・値変化した）`id` 集合。
fn changed_ids(before: &Snapshot, after: &Snapshot) -> BTreeSet<u64> {
    let keys: BTreeSet<&(String, u64)> = before
        .keys()
        .chain(after.keys())
        .filter(|(t, _)| t == VIEWER)
        .collect();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|(_, id)| *id)
        .collect()
}

// ---------- セル定義 ----------

#[derive(Clone, Copy)]
enum Expect {
    /// `(影響行数, 返却 id（期待順）)`。順序は常に照合する（INSERT／UPSERT は VALUES 順、
    /// 述語形 UPDATE／DELETE は id 昇順で、いずれも定義済み）。
    Ok(u64, &'static [u64]),
    Err(&'static str),
}

struct Cell {
    name: &'static str,
    /// `{op}` は `operation_id` に置換される。
    sql: &'static str,
    txn: bool,
    public_only: Expect,
    with_private: Expect,
}

const fn ok(ids: &'static [u64]) -> Expect {
    Expect::Ok(ids.len() as u64, ids)
}

fn cells() -> Vec<Cell> {
    vec![
        Cell {
            name: "INSERT single",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (10, '[0.3,0.4]', 'ja', 'ins-10') RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10]),
        },
        Cell {
            name: "INSERT multi",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (10, '[0.3,0.4]', 'ja', 'ins-10'), (11, '[0.3,0.4]', 'ja', 'ins-11') RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10, 11]),
        },
        Cell {
            name: "UPSERT new DO NOTHING",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (10, '[0.3,0.4]', 'ja', 'ins-10') ON CONFLICT (id) DO NOTHING RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10]),
        },
        Cell {
            name: "UPSERT new DO UPDATE",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (10, '[0.3,0.4]', 'ja', 'ins-10') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10]),
        },
        Cell {
            name: "UPSERT conflict DO UPDATE own Public",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (5, '[0.3,0.4]', 'ja', 'upd-5') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[5]),
            with_private: ok(&[5]),
        },
        Cell {
            name: "UPSERT conflict DO UPDATE own Private",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (1, '[0.3,0.4]', 'ja', 'upd-1') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("42501"),
            with_private: ok(&[1]),
        },
        Cell {
            name: "UPSERT conflict DO NOTHING own Public",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (5, '[0.3,0.4]', 'ja', 'ins-5') ON CONFLICT (id) DO NOTHING RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[]),
            with_private: ok(&[]),
        },
        Cell {
            name: "UPSERT conflict DO NOTHING own Private",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (1, '[0.3,0.4]', 'ja', 'ins-1') ON CONFLICT (id) DO NOTHING RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[]),
            with_private: ok(&[]),
        },
        Cell {
            name: "UPSERT mixed DO UPDATE",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (5, '[0.3,0.4]', 'ja', 'upd-5'), (10, '[0.3,0.4]', 'ja', 'upd-10') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[5, 10]),
        },
        Cell {
            name: "UPSERT mixed DO NOTHING",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (5, '[0.3,0.4]', 'ja', 'ins-5'), (10, '[0.3,0.4]', 'ja', 'ins-10') ON CONFLICT (id) DO NOTHING RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10]),
        },
        Cell {
            name: "UPSERT id only in other tenant",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (9, '[0.3,0.4]', 'ja', 'ins-9') ON CONFLICT (id) DO NOTHING RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[9]),
        },
        Cell {
            name: "UPDATE by id own Public",
            sql: "UPDATE documents SET body = 'upd-5' WHERE id = 5 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[5]),
            with_private: ok(&[5]),
        },
        Cell {
            name: "UPDATE by id own Private",
            sql: "UPDATE documents SET body = 'upd-1' WHERE id = 1 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[]),
            with_private: ok(&[1]),
        },
        Cell {
            name: "UPDATE by id other tenant only",
            sql: "UPDATE documents SET body = 'upd-9' WHERE id = 9 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[]),
            with_private: ok(&[]),
        },
        Cell {
            name: "UPDATE predicate",
            sql: "UPDATE documents SET body = 'upd-pred' WHERE lang = 'ja' RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[5, 6]),
            with_private: ok(&[1, 2, 5, 6]),
        },
        Cell {
            name: "DELETE by id own Private",
            sql: "DELETE FROM documents WHERE id = 1 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[]),
            with_private: ok(&[1]),
        },
        Cell {
            name: "DELETE by id own Public",
            sql: "DELETE FROM documents WHERE id = 5 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[5]),
            with_private: ok(&[5]),
        },
        Cell {
            name: "DELETE by id other tenant only",
            sql: "DELETE FROM documents WHERE id = 9 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[]),
            with_private: ok(&[]),
        },
        Cell {
            name: "DELETE predicate",
            sql: "DELETE FROM documents WHERE lang = 'ja' RETURNING id, body USING OPERATION_ID '{op}'",
            txn: false,
            public_only: ok(&[5, 6]),
            with_private: ok(&[1, 2, 5, 6]),
        },
        Cell {
            name: "TXN INSERT single",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (10, '[0.3,0.4]', 'ja', 'ins-10') RETURNING id, body USING OPERATION_ID '{op}'",
            txn: true,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10]),
        },
        Cell {
            name: "TXN INSERT multi",
            sql: "INSERT INTO documents (id, embedding, lang, body) VALUES (10, '[0.3,0.4]', 'ja', 'ins-10'), (11, '[0.3,0.4]', 'ja', 'ins-11') RETURNING id, body USING OPERATION_ID '{op}'",
            txn: true,
            public_only: Expect::Err("XX000"),
            with_private: ok(&[10, 11]),
        },
        Cell {
            name: "TXN DELETE by id own Private",
            sql: "DELETE FROM documents WHERE id = 1 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: true,
            public_only: ok(&[]),
            with_private: ok(&[1]),
        },
        Cell {
            name: "TXN DELETE by id own Public",
            sql: "DELETE FROM documents WHERE id = 5 RETURNING id, body USING OPERATION_ID '{op}'",
            txn: true,
            public_only: ok(&[5]),
            with_private: ok(&[5]),
        },
    ]
}

// ---------- 実行と観測 ----------

#[derive(Debug, Clone)]
enum Obs {
    Ok {
        affected: u64,
        ids: Vec<u64>,
        debug: String,
    },
    Err {
        code: String,
        message: String,
    },
}

fn observe(res: Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>) -> Obs {
    match res {
        Ok(SqlOutcome::Returning(x)) => Obs::Ok {
            affected: x.rows_affected,
            ids: x.result.rows.iter().map(|r| r.id).collect(),
            debug: format!("{x:?}"),
        },
        Ok(other) => panic!("expected SqlOutcome::Returning, got {other:?}"),
        Err(e) => Obs::Err {
            code: e.wire_code().to_string(),
            message: e.client_message(),
        },
    }
}

/// 1 文を新しい `EngineCore` で実行する（戻る時点でストレージのロックは解放済み）。
fn exec(path: &Path, allow_private: bool, sql: &str, txn: bool) -> Obs {
    let storage = Storage::open(path).expect("open");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for(VIEWER, allow_private);
    let mut session = SessionState::default();
    if !txn {
        return observe(core.execute_sql_in_session(&ctx, &mut session, sql));
    }
    let mut tx = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut tx, "BEGIN")
        .expect("BEGIN");
    let obs = observe(core.execute_sql_in_txn(&ctx, &mut session, &mut tx, sql));
    match &obs {
        Obs::Ok { .. } => {
            core.execute_sql_in_txn(&ctx, &mut session, &mut tx, "COMMIT")
                .expect("COMMIT");
        }
        Obs::Err { .. } => {
            assert_eq!(tx.status(), TransactionStatus::Failed, "txn must be Failed");
            core.execute_sql_in_txn(&ctx, &mut session, &mut tx, "ROLLBACK")
                .expect("ROLLBACK");
        }
    }
    drop(tx);
    obs
}

struct Run {
    cell: usize,
    allow_private: bool,
    obs: Obs,
    before: Snapshot,
    after: Snapshot,
    /// エラーセルのみ: 同じ `operation_id` を Public＋Private で再実行した結果。
    rerun: Option<(Obs, Snapshot)>,
}

fn expect_of(cell: &Cell, allow_private: bool) -> Expect {
    if allow_private {
        cell.with_private
    } else {
        cell.public_only
    }
}

fn run_all() -> &'static Vec<Run> {
    static RUNS: OnceLock<Vec<Run>> = OnceLock::new();
    RUNS.get_or_init(|| {
        let mut runs = Vec::new();
        for (i, cell) in cells().iter().enumerate() {
            for allow_private in [false, true] {
                let path = unique_db_path("sql21-returning-parity");
                let _guard = CleanupGuard(path.clone());
                Storage::open(&path)
                    .expect("open")
                    .create_table(&schema())
                    .expect("create table");
                seed(&path);
                let before = snapshot(&path);
                let sql = cell.sql.replace("{op}", &format!("op-cell-{i}"));
                let obs = exec(&path, allow_private, &sql, cell.txn);
                let after = snapshot(&path);
                let rerun = matches!(obs, Obs::Err { .. }).then(|| {
                    let o = exec(&path, true, &sql, false);
                    (o, snapshot(&path))
                });
                runs.push(Run {
                    cell: i,
                    allow_private,
                    obs,
                    before,
                    after,
                    rerun,
                });
            }
        }
        runs
    })
}

fn label(run: &Run) -> String {
    format!(
        "[{} / {}]",
        cells()[run.cell].name,
        if run.allow_private {
            "Public+Private"
        } else {
            "Public only"
        }
    )
}

/// 成功応答の 3 者一致（返却・影響行数・物理差分）を検査する。
fn assert_ok_parity(
    label: &str,
    obs: &Obs,
    before: &Snapshot,
    after: &Snapshot,
    want: (u64, &[u64]),
) {
    let Obs::Ok { affected, ids, .. } = obs else {
        panic!("{label}: expected success, got {obs:?}");
    };
    let physical = changed_ids(before, after);
    let returned: BTreeSet<u64> = ids.iter().copied().collect();
    assert_eq!(*affected, want.0, "{label}: rows_affected");
    assert_eq!(
        ids.len() as u64,
        *affected,
        "{label}: returned rows != rows_affected"
    );
    assert_eq!(
        physical.len() as u64,
        *affected,
        "{label}: physical changes != rows_affected"
    );
    assert_eq!(
        returned, physical,
        "{label}: returned ids != physically changed ids"
    );
    let want_set: BTreeSet<u64> = want.1.iter().copied().collect();
    assert_eq!(returned, want_set, "{label}: returned ids != expected ids");
    assert_eq!(ids.as_slice(), want.1, "{label}: returned order");
}

#[test]
fn parity_positive_controls_are_non_vacuous() {
    assert_eq!(cells().len(), CELL_COUNT);
    let runs = run_all();
    assert_eq!(runs.len(), CELL_COUNT * 2, "every cell runs in both modes");
    for allow_private in [false, true] {
        let mode: Vec<&Run> = runs
            .iter()
            .filter(|r| r.allow_private == allow_private)
            .collect();
        let ok_rows = mode
            .iter()
            .filter(|r| matches!(r.obs, Obs::Ok { affected, .. } if affected > 0))
            .count();
        assert!(
            ok_rows >= 1,
            "mode private={allow_private}: no success cell with rows"
        );
    }
    let public_errs = runs
        .iter()
        .filter(|r| !r.allow_private && matches!(r.obs, Obs::Err { .. }))
        .count();
    assert!(public_errs >= 1, "Public-only mode must have error cells");
    for r in runs {
        assert!(
            !partition(&r.before, OTHER).is_empty(),
            "{}: other tenant partition must be non-empty",
            label(r)
        );
    }
}

#[test]
fn parity_rows_affected_equals_returned_rows_and_physical_changes() {
    for r in run_all() {
        let cell = &cells()[r.cell];
        if let Expect::Ok(n, ids) = expect_of(cell, r.allow_private) {
            assert_ok_parity(&label(r), &r.obs, &r.before, &r.after, (n, ids));
            // 更新した行の可視性は変えない（Public 行が Private に落ちると返却と不整合になる）。
            for (k, (vis_before, _, _)) in &r.before {
                if let Some((vis_after, _, _)) = r.after.get(k) {
                    assert_eq!(
                        vis_before,
                        vis_after,
                        "{}: visibility of {k:?} changed",
                        label(r)
                    );
                }
            }
        }
    }
}

#[test]
fn parity_error_cells_change_nothing_and_do_not_consume_ledger() {
    let mut seen = 0;
    for r in run_all() {
        let cell = &cells()[r.cell];
        let Expect::Err(code) = expect_of(cell, r.allow_private) else {
            assert!(r.rerun.is_none(), "{}: unexpected rerun", label(r));
            continue;
        };
        seen += 1;
        let Obs::Err { code: got, message } = &r.obs else {
            panic!("{}: expected error {code}, got {:?}", label(r), r.obs);
        };
        assert_eq!(got, code, "{}: wire_code", label(r));
        assert_eq!(
            r.before,
            r.after,
            "{}: aborted statement must change nothing",
            label(r)
        );
        assert!(
            !message.contains(&format!("tok-{OTHER}-"))
                && !message.contains(&format!("tok-{VIEWER}-")),
            "{}: client_message leaks row values: {message}",
            label(r)
        );
        // 台帳未消費: 同じ operation_id の再実行が成功し、成功セルの判定を満たす。
        let (again, after_again) = r.rerun.as_ref().expect("error cell must be re-run");
        match cell.with_private {
            Expect::Ok(n, ids) => assert_ok_parity(
                &format!("{} rerun", label(r)),
                again,
                &r.after,
                after_again,
                (n, ids),
            ),
            Expect::Err(_) => panic!("{}: rerun expectation must be a success", label(r)),
        }
    }
    assert!(seen >= 1, "no error cell was exercised");
}

#[test]
fn parity_no_foreign_tenant_rows_returned_or_modified() {
    for r in run_all() {
        let foreign = format!("tok-{OTHER}-");
        let own_private = [1u64, 2, 7].map(|id| token(VIEWER, id));
        let mut texts = vec![match &r.obs {
            Obs::Ok { debug, .. } => debug.clone(),
            Obs::Err { message, .. } => message.clone(),
        }];
        if let Some((Obs::Ok { debug, .. }, _)) = &r.rerun {
            texts.push(debug.clone());
        }
        for t in &texts[..1] {
            assert!(
                !t.contains(&foreign),
                "{}: foreign tenant token in response: {t}",
                label(r)
            );
            if !r.allow_private {
                for p in &own_private {
                    assert!(
                        !t.contains(p),
                        "{}: Private row value in Public-only response: {t}",
                        label(r)
                    );
                }
            }
        }
        if let Some(t) = texts.get(1) {
            assert!(
                !t.contains(&foreign),
                "{}: foreign tenant token in re-run response: {t}",
                label(r)
            );
        }
        assert_eq!(
            partition(&r.before, OTHER),
            partition(&r.after, OTHER),
            "{}: other tenant rows must be physically unchanged",
            label(r)
        );
        if let Some((_, after_again)) = &r.rerun {
            assert_eq!(
                partition(&r.before, OTHER),
                partition(after_again, OTHER),
                "{}: other tenant rows changed by re-run",
                label(r)
            );
        }
    }
}
