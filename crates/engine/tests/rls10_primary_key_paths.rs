//! PK 分離後（採番方式の表・移行済みの表）の PK 点検索・UPSERT・FK のテナント越境を 3 テナント
//! 一括で機械検証する回帰テスト（Issue #2202。ポインタ: TABLE-23、関連 TABLE-12・TABLE-16・
//! TABLE-17・RLS-9・RLS-10）。
//!
//! 役割: `rls10_relational_paths.rs`／`rls10_write_constraint_paths.rs` は表をすべて従来方式
//! （暗黙の `id` あり。`LegacyExec`）で作るため、採番方式（`RowIdentity::Allocated`。SQL `CREATE
//! TABLE` が作る表）と、既存 DB を開いた時点で移行された表（`MigratedId`）が RLS 境界の対象に
//! 入っていない。本ファイルはその 2 種類の表について、閲覧テナント 3 つ（tenant-a/b/c を順に
//! 閲覧側にする）×可視性 2 モード×実行経路 2 つ（autocommit／明示トランザクション）×全形状の
//! 統一マトリクスで次を固定する。production の可視性判定（`PolicyContext::is_visible`／
//! `is_owner`）はオラクル側で一切呼ばない。
//!
//! - T0 受理ゲート: 各（閲覧側, モード, 実行経路）で陽性対照（影響行あり・期待どおりの制約違反）
//!   が実際に発生していること（空虚に通らない）
//! - T1 独立オラクル: テスト側の真実値（期待件数・期待 `wire_code`）との照合と、応答・読み戻し・
//!   エラー文面に他テナントのトークンや内部 rowid が混入しないこと
//! - T2 応答の不変性: 同一文を baseline（閲覧テナントの行のみ）と flooded（他 2 テナントが同一
//!   PK 値・同一 FK 親キーを持つ行を、閲覧テナントの INSERT の間に挟んで追加したもの）で実行し、
//!   応答と自テナントの事後状態が完全一致すること（rowid 採番がテナント間で混ざっても不変）
//! - T3 他テナントの事後状態が事前と完全一致すること（連鎖先の子表を含む）
//! - T4 FK／PK の存在情報秘匿: 他テナントにだけ親がある場合の応答が、どこにも無い場合と一致
//! - T5 3 テナント回転×2 モード×2 実行経路×全形状の試行総数の固定
//! - T6 負の対照: 捏造した違反を検査器が検出できること
//! - T7 autocommit と明示トランザクション（`BEGIN` → 形状 → `COMMIT`／エラー時 `25P02` →
//!   `ROLLBACK`）の応答・事後状態の一致
//!
//! 可視性軸の判断: 採番方式の表は SQL `INSERT` が可視性を Private に固定し、Rust API の行 API
//! （`insert_typed_row`）は `0A000` で拒否するため Public 行を作れない。このため採番方式の表は
//! `with_visibilities(Public, Private)` を主軸とし、`PolicyContext::new`（Public のみ）では読み取りが
//! 常に 0 行であることと、応答・事後状態が baseline／flooded で不変であることを検査する。Public
//! 行を含む検査は、Rust API で Public／Private 行を seed してから開き直して移行した表で行う。
//!
//! 対象外: NoSQL 表層（`crates/wire-server/tests/table23_nosql_pk_row_address.rs` の担当）、
//! 採番方式の親への `id` 参照 FK（`0A000`。`docs/design/rowid-primary-key-separation.md` §11・§12）、
//! 他テナントの Public 行を含む読み取りの可視性（`rls10_relational_paths.rs` の担当）。

use std::path::PathBuf;
use std::sync::OnceLock;

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 定数・フィクスチャ ----------

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 採番方式の表（SQL `CREATE TABLE`）の DDL。
const DDL: [&str; 9] = [
    "CREATE TABLE pk_text (code TEXT PRIMARY KEY, n BIGINT, body TEXT)",
    "CREATE TABLE pk_comp (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b))",
    "CREATE TABLE pk_id (id BIGINT PRIMARY KEY, v TEXT)",
    "CREATE TABLE nopk (k BIGINT, v TEXT)",
    "CREATE TABLE parents (code TEXT PRIMARY KEY, name TEXT)",
    "CREATE TABLE children (pcode TEXT REFERENCES parents (code), note TEXT)",
    "CREATE TABLE children_c (pcode TEXT REFERENCES parents (code) ON DELETE CASCADE, note TEXT)",
    "CREATE TABLE children_s (pcode TEXT REFERENCES parents (code) ON DELETE SET NULL, note TEXT)",
    "CREATE TABLE comp_children (a TEXT, b INTEGER, note TEXT, \
     FOREIGN KEY (a, b) REFERENCES pk_comp (a, b))",
];

/// 移行済みの表になる従来方式の表（`LegacyExec`。開き直しで `MigratedId` へ移行される）。
const LEGACY_DDL: [&str; 2] = [
    "CREATE TABLE legacy_pk (code TEXT PRIMARY KEY, tag TEXT)",
    "CREATE TABLE legacy_plain (v TEXT)",
];

/// 事後状態の走査: （表, 決定的な `ORDER BY`）。
const SNAP: [(&str, &str); 11] = [
    ("pk_text", "code, n"),
    ("pk_comp", "a, b"),
    ("pk_id", "id"),
    ("nopk", "k, v"),
    ("parents", "code, name"),
    ("children", "pcode, note"),
    ("children_c", "pcode, note"),
    ("children_s", "pcode, note"),
    ("comp_children", "a, b"),
    ("legacy_pk", "code, tag"),
    ("legacy_plain", "id"),
];

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

/// 採番方式の表への seed 文（テナント `t` 分。親が子より先になる順序を保つ）。
/// `flood_extra` は閲覧側以外だけが持つキー（`only-other` 等。閲覧側は持たない）。
fn seed_statements(t: &str, flood_extra: bool) -> Vec<String> {
    let mut v = vec![
        format!("INSERT INTO pk_text (code, n, body) VALUES ('k1', 1, 'tok-{t}-k1')"),
        format!("INSERT INTO pk_text (code, n, body) VALUES ('k2', 2, 'tok-{t}-k2')"),
        format!("INSERT INTO pk_comp (a, b, v) VALUES ('x', 1, 'tok-{t}-x1')"),
        format!("INSERT INTO pk_comp (a, b, v) VALUES ('x', 2, 'tok-{t}-x2')"),
        format!("INSERT INTO pk_id (id, v) VALUES (5, 'tok-{t}-5')"),
        format!("INSERT INTO pk_id (id, v) VALUES (6, 'tok-{t}-6')"),
        format!("INSERT INTO nopk (k, v) VALUES (1, 'tok-{t}-n1')"),
        format!("INSERT INTO nopk (k, v) VALUES (2, 'tok-{t}-n2')"),
        format!("INSERT INTO parents (code, name) VALUES ('p1', 'tok-{t}-p1')"),
        format!("INSERT INTO parents (code, name) VALUES ('p2', 'tok-{t}-p2')"),
        format!("INSERT INTO children (pcode, note) VALUES ('p1', 'tok-{t}-c1')"),
        format!("INSERT INTO children_c (pcode, note) VALUES ('p2', 'tok-{t}-cc')"),
        format!("INSERT INTO children_s (pcode, note) VALUES ('p2', 'tok-{t}-cs')"),
        format!("INSERT INTO comp_children (a, b, note) VALUES ('x', 1, 'tok-{t}-cx')"),
    ];
    if flood_extra {
        v.extend([
            format!("INSERT INTO pk_text (code, n, body) VALUES ('only-other', 9, 'tok-{t}-o')"),
            format!("INSERT INTO pk_comp (a, b, v) VALUES ('only-other', 1, 'tok-{t}-xo')"),
            format!("INSERT INTO pk_id (id, v) VALUES (77, 'tok-{t}-77')"),
            format!("INSERT INTO nopk (k, v) VALUES (99, 'tok-{t}-n99')"),
            format!("INSERT INTO parents (code, name) VALUES ('only-other', 'tok-{t}-po')"),
        ]);
    }
    v
}

/// テンプレート DB: 採番方式の表（SQL）と移行済みの表（Rust API の seed → 開き直し）を作る。
/// flooded では他 2 テナントの行を、閲覧テナントの INSERT の間に 1 文ずつ挟んで入れる。
fn build_template(viewer: &str, flood: bool) -> PathBuf {
    let path = unique_db_path("rls10-pk-template");
    {
        let core = EngineCore::open(&path).expect("open");
        let sys = ctx_for("sys", true);
        let mut s = SessionState::default();
        s.allow_ddl();
        for ddl in DDL {
            core.execute_sql_in_session(&sys, &mut s, ddl)
                .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
        }
        for ddl in LEGACY_DDL {
            core.execute_sql_in_session_legacy(&sys, &mut s, ddl)
                .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
        }
        // 採番方式の表: 閲覧側 1 文ごとに、他テナントの対応する 1 文を挟む。
        let others: Vec<&str> = TENANTS.iter().copied().filter(|t| *t != viewer).collect();
        let own = seed_statements(viewer, false);
        let theirs: Vec<Vec<String>> = others.iter().map(|t| seed_statements(t, true)).collect();
        let mut n = 0u32;
        let mut run = |tenant: &str, sql: &str| {
            n += 1;
            let ctx = ctx_for(tenant, true);
            let mut s = SessionState::default();
            let sql = format!("{sql} USING OPERATION_ID 'seed-{tenant}-{n}'");
            core.execute_sql_in_session(&ctx, &mut s, &sql)
                .unwrap_or_else(|e| panic!("seed {sql}: {e:?}"));
        };
        let longest = theirs
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(own.len());
        for i in 0..longest {
            if let Some(sql) = own.get(i) {
                run(viewer, sql);
            }
            if flood {
                for (t, stmts) in others.iter().zip(&theirs) {
                    if let Some(sql) = stmts.get(i) {
                        run(t, sql);
                    }
                }
            }
        }
    }
    // 移行される表: Public／Private 行を Rust API で seed（閲覧側は Public の k1・id1 と Private の
    // k2・id2。flooded の他テナントは同一 PK・同一 id の Private 行と only-other を持つ）。
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let seed = |t: &str, table: &str, id: u64, public: bool, values: Vec<Value>| {
            let op = OperationId::parse(&format!("lseed-{table}-{t}-{id}")).expect("op id");
            let vis = if public {
                Visibility::Public
            } else {
                Visibility::Private
            };
            engine::tenant::insert_typed_row(
                &storage,
                table,
                &ctx_for(t, true),
                id,
                vis,
                &values,
                &op,
            )
            .unwrap_or_else(|e| match e {
                engine::tenant::TenantWriteError::Catalog(c) => {
                    panic!("legacy seed {table} {t} {id}: {c:?}")
                }
                e => panic!("legacy seed {table} {t} {id}: {e:?}"),
            });
        };
        let text = |s: &str| Value::Text(s.to_string());
        seed(
            viewer,
            "legacy_pk",
            1,
            true,
            vec![text("k1"), text(&format!("tok-{viewer}-lk1"))],
        );
        seed(
            viewer,
            "legacy_pk",
            2,
            false,
            vec![text("k2"), text(&format!("tok-{viewer}-lk2"))],
        );
        seed(
            viewer,
            "legacy_plain",
            1,
            true,
            vec![text(&format!("tok-{viewer}-l1"))],
        );
        seed(
            viewer,
            "legacy_plain",
            2,
            false,
            vec![text(&format!("tok-{viewer}-l2"))],
        );
        if flood {
            for t in TENANTS.iter().filter(|t| **t != viewer) {
                seed(
                    t,
                    "legacy_pk",
                    1,
                    false,
                    vec![text("k1"), text(&format!("tok-{t}-lk1"))],
                );
                seed(
                    t,
                    "legacy_pk",
                    2,
                    false,
                    vec![text("k2"), text(&format!("tok-{t}-lk2"))],
                );
                seed(
                    t,
                    "legacy_pk",
                    3,
                    false,
                    vec![text("only-other"), text(&format!("tok-{t}-lko"))],
                );
                seed(
                    t,
                    "legacy_plain",
                    1,
                    false,
                    vec![text(&format!("tok-{t}-l1"))],
                );
                seed(
                    t,
                    "legacy_plain",
                    2,
                    false,
                    vec![text(&format!("tok-{t}-l2"))],
                );
            }
        }
    }
    // 開き直し（ここで従来方式の表が `MigratedId` へ移行される）。移行後の状態をテンプレートへ永続化。
    drop(EngineCore::open(&path).expect("reopen migrates"));
    path
}

// ---------- 観測 ----------

#[derive(Clone, Debug, PartialEq)]
enum Obs {
    Ok { debug: String, count: Option<u64> },
    Err { code: &'static str, message: String },
}

/// Debug 文字列から `ResultRow` の内部 id（割り当てられた rowid）を除く。flood 側では他テナントの
/// INSERT が間に入り採番が baseline とずれるため、SQL セル値だけを比較対象にする（RLS 判定を採番差で
/// 誤らせない）。`ResultRow { id: N, ` を `ResultRow { ` へ正規化する。
fn strip_row_ids(dbg: &str) -> String {
    const HEAD: &str = "ResultRow { id: ";
    let mut out = String::with_capacity(dbg.len());
    let mut rest = dbg;
    while let Some(pos) = rest.find(HEAD) {
        out.push_str(&rest[..pos]);
        out.push_str("ResultRow { ");
        let after = &rest[pos + HEAD.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        let tail = &after[digits..];
        rest = tail.strip_prefix(", ").unwrap_or(tail);
    }
    out.push_str(rest);
    out
}

fn observe(r: Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>) -> Obs {
    match r {
        Ok(o) => {
            let count = match &o {
                SqlOutcome::Query(q) | SqlOutcome::Explain(q) => Some(q.rows.len() as u64),
                SqlOutcome::Insert(i) => Some(i.rows_affected),
                SqlOutcome::Update(u) => Some(u.rows_affected),
                SqlOutcome::Delete(d) => Some(d.rows_affected),
                SqlOutcome::Returning(r) => Some(r.rows_affected),
                _ => None,
            };
            Obs::Ok {
                debug: strip_row_ids(&format!("{o:?}")),
                count,
            }
        }
        Err(e) => Obs::Err {
            code: e.wire_code(),
            message: e.client_message(),
        },
    }
}

/// 明示トランザクション経路（BEGIN → 形状 → 成功なら COMMIT、失敗なら 25P02 → ROLLBACK）。
fn run_explicit_txn(core: &EngineCore, ctx: &PolicyContext, sql: &str, what: &str) -> Obs {
    let mut txn = core.new_session_transaction();
    let mut session = SessionState::default();
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "BEGIN")
        .unwrap_or_else(|e| panic!("BEGIN ({e:?}): {what}"));
    let obs = observe(core.execute_sql_in_txn(ctx, &mut session, &mut txn, sql));
    match &obs {
        Obs::Ok { .. } => {
            assert_eq!(txn.status(), TransactionStatus::InTransaction, "{what}");
            core.execute_sql_in_txn(ctx, &mut session, &mut txn, "COMMIT")
                .unwrap_or_else(|e| panic!("COMMIT ({e:?}): {what}"));
            assert_eq!(txn.status(), TransactionStatus::Idle, "{what}");
        }
        Obs::Err { .. } => {
            assert_eq!(txn.status(), TransactionStatus::Failed, "{what}");
            let e = core
                .execute_sql_in_txn(ctx, &mut session, &mut txn, "COMMIT")
                .expect_err("COMMIT in a failed transaction must be rejected");
            assert_eq!(e.wire_code(), "25P02", "{what}");
            core.execute_sql_in_txn(ctx, &mut session, &mut txn, "ROLLBACK")
                .unwrap_or_else(|e| panic!("ROLLBACK ({e:?}): {what}"));
            assert_eq!(txn.status(), TransactionStatus::Idle, "{what}");
        }
    }
    obs
}

/// `tenant` の全表の事後状態（そのテナント自身の文脈で読む）。物理 rowid は表に出ない。
///
/// `exclude` を指定すると、そのテナントのトークンを持つ行を除く。閲覧側の Public 行は他テナントからも
/// 読める（RLS-7）ため、「他テナントの行が変わっていないこと」を見るときに閲覧側の行を除外する。
fn tenant_state(core: &EngineCore, tenant: &str, exclude: Option<&str>) -> String {
    let ctx = ctx_for(tenant, true);
    let mut out = String::new();
    for (table, order) in SNAP {
        let sql = format!("SELECT * FROM {table} ORDER BY {order} LIMIT 1000");
        match core.execute_sql(&ctx, &sql) {
            Ok(q) => {
                let rows: Vec<String> = q
                    .rows
                    .iter()
                    .map(|r| format!("{:?}", r.cells))
                    .filter(|r| exclude.is_none_or(|x| !r.contains(&format!("tok-{x}-"))))
                    .collect();
                out.push_str(&format!("{table}:{rows:?}\n"));
            }
            Err(e) => out.push_str(&format!("{table}:ERR {}\n", e.wire_code())),
        }
    }
    out
}

/// `dump` に viewer 以外のテナント名（トークンの一部）が現れる件数。
fn leaked(dump: &str, viewer: &str) -> usize {
    let tenants: usize = TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .map(|t| dump.matches(*t).count())
        .sum();
    tenants
}

/// 成功応答・事後状態に内部 rowid を示す語が現れる件数（エラー文面は、利用者が書いた未知の列名を
/// 引用するため対象にしない。未知列の拒否は `pseudo_*` 形状が固定する）。
fn internal_id_words(dump: &str) -> usize {
    dump.matches("rowid").count() + dump.matches("ctid").count()
}

// ---------- 形状 ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Axis {
    Read,
    Write,
    Constraint,
}

/// テスト側の真実値: 件数（読み取りは返却行数、書き込みは影響行数）／ `wire_code`。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Out {
    N(u64),
    E(&'static str),
}

struct Shape {
    name: &'static str,
    axis: Axis,
    sql: &'static str,
    /// （Private 込みモード, Public のみモード）の期待。
    expect: (Out, Out),
}

const fn sh(name: &'static str, axis: Axis, sql: &'static str, expect: (Out, Out)) -> Shape {
    Shape {
        name,
        axis,
        sql,
        expect,
    }
}

fn shapes() -> Vec<Shape> {
    use Axis::{Constraint as C, Read as R, Write as W};
    use Out::{E, N};
    vec![
        // ---- 読み取り: PK 点検索・他テナントだけが持つキー ----
        sh("read_pk_point", R, "SELECT * FROM pk_text WHERE code = 'k1' ORDER BY code LIMIT 10", (N(1), N(0))),
        sh("read_pk_point_other_only", R, "SELECT * FROM pk_text WHERE code = 'only-other' ORDER BY code LIMIT 10", (N(0), N(0))),
        sh("read_pk_comp_point", R, "SELECT * FROM pk_comp WHERE a = 'x' AND b = 1 ORDER BY a LIMIT 10", (N(1), N(0))),
        sh("read_pk_comp_point_other_only", R, "SELECT * FROM pk_comp WHERE a = 'only-other' AND b = 1 ORDER BY a LIMIT 10", (N(0), N(0))),
        sh("read_pk_id_point", R, "SELECT * FROM pk_id WHERE id = 5 ORDER BY id LIMIT 10", (N(1), N(0))),
        sh("read_pk_id_point_other_only", R, "SELECT * FROM pk_id WHERE id = 77 ORDER BY id LIMIT 10", (N(0), N(0))),
        sh("read_pk_scan_all", R, "SELECT * FROM pk_text ORDER BY code LIMIT 100", (N(2), N(0))),
        sh("read_nopk_pred", R, "SELECT * FROM nopk WHERE k = 1 ORDER BY k LIMIT 10", (N(1), N(0))),
        sh("read_nopk_pred_other_only", R, "SELECT * FROM nopk WHERE k = 99 ORDER BY k LIMIT 10", (N(0), N(0))),
        sh("read_explain_point", R, "EXPLAIN SELECT * FROM pk_text WHERE code = 'k1' ORDER BY code LIMIT 10", (N(2), N(2))),
        sh("read_legacy_pk_point", R, "SELECT * FROM legacy_pk WHERE code = 'k1' ORDER BY code LIMIT 10", (N(1), N(1))),
        sh("read_legacy_pk_private_point", R, "SELECT * FROM legacy_pk WHERE code = 'k2' ORDER BY code LIMIT 10", (N(1), N(0))),
        sh("read_legacy_pk_other_only", R, "SELECT * FROM legacy_pk WHERE code = 'only-other' ORDER BY code LIMIT 10", (N(0), N(0))),
        sh("read_legacy_id_point", R, "SELECT * FROM legacy_plain WHERE id = 2 ORDER BY id LIMIT 10", (N(1), N(0))),
        sh("read_legacy_id_other_only", R, "SELECT * FROM legacy_pk WHERE id = 3 ORDER BY id LIMIT 10", (N(0), N(0))),
        // ---- 書き込み: UPSERT ----
        sh("upsert_pk_update_own", W, "INSERT INTO pk_text (code, n, body) VALUES ('k1', 50, 'new') ON CONFLICT (code) DO UPDATE SET n = EXCLUDED.n RETURNING * USING OPERATION_ID 'w{op}'", (N(1), E("42501"))),
        sh("upsert_pk_other_only_inserts_own", W, "INSERT INTO pk_text (code, n, body) VALUES ('only-other', 50, 'new') ON CONFLICT (code) DO UPDATE SET n = EXCLUDED.n USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("upsert_pk_do_nothing_own", W, "INSERT INTO pk_text (code, n, body) VALUES ('k1', 50, 'new') ON CONFLICT (code) DO NOTHING USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        sh("upsert_pk_do_nothing_other_only", W, "INSERT INTO pk_text (code, n, body) VALUES ('only-other', 50, 'new') ON CONFLICT (code) DO NOTHING USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("upsert_comp_update_own", W, "INSERT INTO pk_comp (a, b, v) VALUES ('x', 1, 'new') ON CONFLICT (a, b) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'w{op}'", (N(1), E("42501"))),
        sh("upsert_comp_other_only_inserts_own", W, "INSERT INTO pk_comp (a, b, v) VALUES ('only-other', 1, 'new') ON CONFLICT (a, b) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("upsert_pk_id_update_own", W, "INSERT INTO pk_id (id, v) VALUES (5, 'new') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'w{op}'", (N(1), E("42501"))),
        sh("upsert_pk_id_other_only_inserts_own", W, "INSERT INTO pk_id (id, v) VALUES (77, 'new') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("upsert_legacy_pk_update_own", W, "INSERT INTO legacy_pk (id, code, tag) VALUES (2, 'k2', 'new') ON CONFLICT (code) DO UPDATE SET tag = EXCLUDED.tag USING OPERATION_ID 'w{op}'", (N(1), E("42501"))),
        sh("upsert_legacy_pk_other_only_inserts_own", W, "INSERT INTO legacy_pk (id, code, tag) VALUES (3, 'only-other', 'new') ON CONFLICT (code) DO UPDATE SET tag = EXCLUDED.tag USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("upsert_legacy_id_update_own", W, "INSERT INTO legacy_plain (id, v) VALUES (2, 'new') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'w{op}'", (N(1), E("42501"))),
        // ---- 書き込み: UPDATE／DELETE ----
        sh("update_pk_point_own", W, "UPDATE pk_text SET n = 100 WHERE code = 'k1' USING OPERATION_ID 'w{op}'", (N(1), N(0))),
        sh("update_pk_point_other_only", W, "UPDATE pk_text SET n = 100 WHERE code = 'only-other' USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        sh("delete_pk_point_own", W, "DELETE FROM pk_text WHERE code = 'k1' USING OPERATION_ID 'w{op}'", (N(1), N(0))),
        sh("delete_pk_point_other_only", W, "DELETE FROM pk_text WHERE code = 'only-other' USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        sh("update_pk_value_to_other_held", W, "UPDATE pk_text SET code = 'only-other' WHERE code = 'k1' USING OPERATION_ID 'w{op}'", (N(1), N(0))),
        sh("update_pk_value_to_own_existing", C, "UPDATE pk_text SET code = 'k2' WHERE code = 'k1' USING OPERATION_ID 'w{op}'", (E("23505"), N(0))),
        sh("insert_pk_duplicate_own", C, "INSERT INTO pk_text (code, n, body) VALUES ('k1', 1, 'dup') USING OPERATION_ID 'w{op}'", (E("23505"), E("23505"))),
        sh("insert_pk_other_held_is_accepted", W, "INSERT INTO pk_text (code, n, body) VALUES ('only-other', 1, 'mine') USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("update_nopk_pred_own", W, "UPDATE nopk SET v = 'new' WHERE k = 1 USING OPERATION_ID 'w{op}'", (N(1), N(0))),
        sh("update_nopk_pred_other_only", W, "UPDATE nopk SET v = 'new' WHERE k = 99 USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        sh("delete_nopk_pred_own", W, "DELETE FROM nopk WHERE k = 1 USING OPERATION_ID 'w{op}'", (N(1), N(0))),
        sh("update_legacy_pk_other_only", W, "UPDATE legacy_pk SET tag = 'x' WHERE code = 'only-other' USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        sh("delete_legacy_id_own", W, "DELETE FROM legacy_plain WHERE id = 1 USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("delete_legacy_id_other_only", W, "DELETE FROM legacy_pk WHERE id = 3 USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        // ---- FK（宣言キー参照） ----
        sh("fk_insert_own_parent", C, "INSERT INTO children (pcode, note) VALUES ('p1', 'n') USING OPERATION_ID 'w{op}'", (N(1), N(1))),
        sh("fk_insert_parent_only_other_has", C, "INSERT INTO children (pcode, note) VALUES ('only-other', 'n') USING OPERATION_ID 'w{op}'", (E("23503"), E("23503"))),
        sh("fk_insert_parent_nowhere", C, "INSERT INTO children (pcode, note) VALUES ('nowhere', 'n') USING OPERATION_ID 'w{op}'", (E("23503"), E("23503"))),
        sh("fk_comp_insert_parent_only_other_has", C, "INSERT INTO comp_children (a, b, note) VALUES ('only-other', 1, 'n') USING OPERATION_ID 'w{op}'", (E("23503"), E("23503"))),
        sh("fk_comp_insert_parent_nowhere", C, "INSERT INTO comp_children (a, b, note) VALUES ('nowhere', 1, 'n') USING OPERATION_ID 'w{op}'", (E("23503"), E("23503"))),
        sh("fk_delete_referenced_parent", C, "DELETE FROM parents WHERE code = 'p1' USING OPERATION_ID 'w{op}'", (E("23503"), N(0))),
        sh("fk_update_referenced_parent_key", C, "UPDATE parents SET code = 'p9' WHERE code = 'p1' USING OPERATION_ID 'w{op}'", (E("23503"), N(0))),
        sh("fk_comp_delete_referenced_parent", C, "DELETE FROM pk_comp WHERE a = 'x' AND b = 1 USING OPERATION_ID 'w{op}'", (E("23503"), N(0))),
        sh("fk_cascade_and_set_null_own_children_only", W, "DELETE FROM parents WHERE code = 'p2' USING OPERATION_ID 'w{op}'", (N(1), N(0))),
        sh("fk_delete_parent_only_other_has", W, "DELETE FROM parents WHERE code = 'only-other' USING OPERATION_ID 'w{op}'", (N(0), N(0))),
        // ---- 内部 rowid が見えない ----
        sh("pseudo_rowid_unknown", R, "SELECT rowid FROM pk_text ORDER BY code LIMIT 10", (E("42703"), E("42703"))),
        sh("pseudo_ctid_unknown", R, "SELECT ctid FROM nopk ORDER BY k LIMIT 10", (E("42703"), E("42703"))),
        sh("pseudo_id_unknown_on_allocated", R, "SELECT id FROM pk_text ORDER BY code LIMIT 10", (E("42703"), E("42703"))),
    ]
}

// ---------- 実行と報告 ----------

#[derive(Clone, Copy, Debug, PartialEq)]
enum ExecPath {
    Autocommit,
    ExplicitTxn,
}

struct Run {
    viewer: &'static str,
    allow_private: bool,
    exec: ExecPath,
    shape: &'static str,
    axis: Axis,
    expect: Out,
    base: Obs,
    flood: Obs,
    base_own_after: String,
    flood_own_after: String,
    foreign_before: String,
    foreign_after: String,
    leaks: usize,
}

fn run_one(
    template: &PathBuf,
    viewer: &str,
    allow_private: bool,
    idx: usize,
    shape: &Shape,
    exec: ExecPath,
) -> (Obs, String, String, usize) {
    let path = unique_db_path("rls10-pk-run");
    let _guard = CleanupGuard(path.clone());
    std::fs::copy(template, &path).expect("copy template");
    let core = EngineCore::open(&path).expect("open run db");
    let ctx = ctx_for(viewer, allow_private);
    let sql = shape.sql.replace("{op}", &format!("{idx}-{viewer}"));
    let obs = match exec {
        ExecPath::Autocommit => {
            let mut session = SessionState::default();
            observe(core.execute_sql_in_session(&ctx, &mut session, &sql))
        }
        ExecPath::ExplicitTxn => run_explicit_txn(
            &core,
            &ctx,
            &sql,
            &format!(
                "viewer={viewer} private={allow_private} shape={}",
                shape.name
            ),
        ),
    };
    let own = tenant_state(&core, viewer, None);
    let foreign: String = TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .map(|t| format!("{t}\n{}", tenant_state(&core, t, Some(viewer))))
        .collect();
    let mut leaks = leaked(&own, viewer) + internal_id_words(&own);
    leaks += match &obs {
        Obs::Ok { debug, .. } => leaked(debug, viewer) + internal_id_words(debug),
        Obs::Err { message, .. } => leaked(message, viewer),
    };
    (obs, own, foreign, leaks)
}

const EXEC_PATHS: [ExecPath; 2] = [ExecPath::Autocommit, ExecPath::ExplicitTxn];

fn report() -> &'static Vec<Run> {
    static REPORT: OnceLock<Vec<Run>> = OnceLock::new();
    REPORT.get_or_init(|| {
        let shapes = shapes();
        let mut runs = Vec::new();
        for viewer in TENANTS {
            let base_tpl = build_template(viewer, false);
            let flood_tpl = build_template(viewer, true);
            let _g1 = CleanupGuard(base_tpl.clone());
            let _g2 = CleanupGuard(flood_tpl.clone());
            let foreign_before = {
                let core = EngineCore::open(&flood_tpl).expect("open flood template");
                TENANTS
                    .iter()
                    .filter(|t| **t != viewer)
                    .map(|t| format!("{t}\n{}", tenant_state(&core, t, Some(viewer))))
                    .collect::<String>()
            };
            for allow_private in [true, false] {
                for exec in EXEC_PATHS {
                    for (idx, shape) in shapes.iter().enumerate() {
                        let (base, base_own, _, l1) =
                            run_one(&base_tpl, viewer, allow_private, idx, shape, exec);
                        let (flood, flood_own, foreign_after, l2) =
                            run_one(&flood_tpl, viewer, allow_private, idx, shape, exec);
                        runs.push(Run {
                            viewer,
                            allow_private,
                            exec,
                            shape: shape.name,
                            axis: shape.axis,
                            expect: if allow_private {
                                shape.expect.0
                            } else {
                                shape.expect.1
                            },
                            base,
                            flood,
                            base_own_after: base_own,
                            flood_own_after: flood_own,
                            foreign_before: foreign_before.clone(),
                            foreign_after,
                            leaks: l1 + l2,
                        });
                    }
                }
            }
        }
        runs
    })
}

fn label(r: &Run) -> String {
    format!(
        "viewer={} allow_private={} path={:?} shape={}",
        r.viewer, r.allow_private, r.exec, r.shape
    )
}

/// 観測が真実値 `expect` を満たすか（独立オラクル）。
fn meets(obs: &Obs, expect: Out) -> bool {
    match (obs, expect) {
        (Obs::Ok { count, .. }, Out::N(n)) => *count == Some(n),
        (Obs::Err { code, .. }, Out::E(c)) => *code == c,
        _ => false,
    }
}

/// baseline と flooded の観測が区別できない（T2 の検査器。T6 の負の対照でも使う）。
fn indistinguishable(base: &Obs, flood: &Obs, base_own: &str, flood_own: &str) -> bool {
    // 読み取りの EXPLAIN 等も含め、応答は完全一致を要求する（Debug 文字列・コード・文面）。
    base == flood && base_own == flood_own
}

// ---------- テスト ----------

/// T0 受理ゲート: 各（閲覧側, モード, 実行経路）で陽性対照が実際に発生している。
#[test]
fn t0_positive_controls_are_non_vacuous() {
    for viewer in TENANTS {
        for allow_private in [true, false] {
            for exec in EXEC_PATHS {
                let scoped: Vec<&Run> = report()
                    .iter()
                    .filter(|r| {
                        r.viewer == viewer && r.allow_private == allow_private && r.exec == exec
                    })
                    .collect();
                assert!(!scoped.is_empty());
                let nonempty_reads = scoped
                    .iter()
                    .filter(|r| {
                        r.axis == Axis::Read
                            && matches!(&r.flood, Obs::Ok { count: Some(n), .. } if *n > 0)
                    })
                    .count();
                let affected_writes = scoped
                    .iter()
                    .filter(|r| {
                        r.axis == Axis::Write
                            && matches!(&r.flood, Obs::Ok { count: Some(n), .. } if *n > 0)
                    })
                    .count();
                let violations = scoped
                    .iter()
                    .filter(|r| matches!(&r.flood, Obs::Err { code, .. } if matches!(*code, "23503" | "23505")))
                    .count();
                if allow_private {
                    assert!(nonempty_reads >= 8, "{viewer} {exec:?}: {nonempty_reads}");
                    assert!(
                        affected_writes >= 15,
                        "{viewer} {exec:?}: {affected_writes}"
                    );
                    assert!(violations >= 8, "{viewer} {exec:?}: {violations}");
                } else {
                    // Public のみモード: 採番方式の表は何も見えない。移行済みの Public 行だけが見える。
                    assert!(nonempty_reads >= 1, "{viewer} {exec:?}: {nonempty_reads}");
                    assert!(violations >= 4, "{viewer} {exec:?}: {violations}");
                }
            }
        }
    }
}

/// T1 独立オラクル: 真実値との照合と、応答・読み戻し・エラー文面への他テナント情報の非混入。
#[test]
fn t1_responses_meet_independent_oracle_and_do_not_leak() {
    for r in report() {
        assert!(
            meets(&r.base, r.expect),
            "{}: baseline {:?} vs {:?}",
            label(r),
            r.base,
            r.expect
        );
        assert!(
            meets(&r.flood, r.expect),
            "{}: flooded {:?} vs {:?}",
            label(r),
            r.flood,
            r.expect
        );
        assert_eq!(
            r.leaks,
            0,
            "{}: leaked foreign tokens or internal row ids",
            label(r)
        );
    }
}

/// T2 応答の不変性: baseline と flooded で応答と自テナントの事後状態が完全一致。
#[test]
fn t2_flood_does_not_change_response_or_own_state() {
    for r in report() {
        assert!(
            indistinguishable(&r.base, &r.flood, &r.base_own_after, &r.flood_own_after),
            "{}: base={:?} flood={:?}",
            label(r),
            r.base,
            r.flood
        );
    }
}

/// T3 他テナントの事後状態が事前と完全一致（連鎖先の子表を含む）。
#[test]
fn t3_foreign_state_is_untouched() {
    for r in report() {
        assert_eq!(r.foreign_before, r.foreign_after, "{}", label(r));
    }
}

/// T4 FK／PK の存在情報秘匿: 他テナントにだけ親がある場合の応答が、どこにも無い場合と一致する。
#[test]
fn t4_fk_and_pk_existence_is_not_revealed() {
    let pairs = [
        (
            "fk_insert_parent_only_other_has",
            "fk_insert_parent_nowhere",
        ),
        (
            "fk_comp_insert_parent_only_other_has",
            "fk_comp_insert_parent_nowhere",
        ),
    ];
    let mut compared = 0;
    for (hidden, nowhere) in pairs {
        for r in report().iter().filter(|r| r.shape == hidden) {
            let twin = report()
                .iter()
                .find(|t| {
                    t.shape == nowhere
                        && t.viewer == r.viewer
                        && t.allow_private == r.allow_private
                        && t.exec == r.exec
                })
                .expect("twin shape");
            assert_eq!(
                r.flood,
                twin.flood,
                "{}: FK response differs from the nowhere case",
                label(r)
            );
            compared += 1;
        }
    }
    assert_eq!(compared, 2 * 3 * 2 * 2);
    // PK: 他テナントだけが持つ値への点検索は、どこにも無い値と同じ 0 行（baseline と一致は T2）。
    for r in report()
        .iter()
        .filter(|r| r.shape == "read_pk_point_other_only")
    {
        assert!(
            matches!(&r.flood, Obs::Ok { count: Some(0), .. }),
            "{}",
            label(r)
        );
    }
}

/// T5 3 テナント回転と試行総数の固定（ループの空回り防止）。
#[test]
fn t5_trial_count_is_pinned() {
    let n_shapes = shapes().len();
    assert_eq!(n_shapes, 53);
    assert_eq!(report().len(), 3 * 2 * 2 * n_shapes);
    for viewer in TENANTS {
        assert_eq!(
            report().iter().filter(|r| r.viewer == viewer).count(),
            2 * 2 * n_shapes
        );
    }
}

/// T6 負の対照: 捏造した違反（他テナント由来の行の混入・他テナントの状態の変化・トークンの漏えい）を
/// 検査器が検出できる。
#[test]
fn t6_negative_controls_are_detected() {
    let r = report()
        .iter()
        .find(|r| r.shape == "read_pk_point" && r.allow_private)
        .expect("shape");
    // 応答に他テナントの行が混ざった状態を捏造する。
    let forged = match &r.flood {
        Obs::Ok { debug, .. } => Obs::Ok {
            debug: format!("{debug}tok-tenant-z"),
            count: Some(2),
        },
        other => other.clone(),
    };
    assert!(!indistinguishable(
        &r.base,
        &forged,
        &r.base_own_after,
        &r.flood_own_after
    ));
    assert!(!meets(&forged, r.expect));
    // 自テナントの事後状態が flood で変わった状態を捏造する。
    let forged_state = format!("{}extra-row", r.flood_own_after);
    assert!(!indistinguishable(
        &r.base,
        &r.flood,
        &r.base_own_after,
        &forged_state
    ));
    // 他テナントのトークンと内部 rowid の語を漏えいとして数える。
    let other = TENANTS.iter().find(|t| **t != r.viewer).expect("other");
    assert!(leaked(&format!("tok-{other}-k1"), r.viewer) > 0);
    assert!(internal_id_words("cells: rowid") > 0);
    assert_eq!(leaked(&format!("tok-{}-k1", r.viewer), r.viewer), 0);
}

/// T7 経路間パリティ: autocommit と明示トランザクションで応答・自テナントの事後状態が一致する。
#[test]
fn t7_autocommit_and_explicit_txn_agree() {
    let mut compared = 0;
    for r in report().iter().filter(|r| r.exec == ExecPath::Autocommit) {
        let t = report()
            .iter()
            .find(|t| {
                t.exec == ExecPath::ExplicitTxn
                    && t.viewer == r.viewer
                    && t.allow_private == r.allow_private
                    && t.shape == r.shape
            })
            .expect("txn twin");
        assert_eq!(r.flood, t.flood, "{}", label(r));
        assert_eq!(r.flood_own_after, t.flood_own_after, "{}", label(r));
        assert_eq!(r.foreign_after, t.foreign_after, "{}", label(r));
        compared += 1;
    }
    assert_eq!(compared, 3 * 2 * shapes().len());
}

/// 移行済みの表と採番方式の表の両方で、宣言キーを参照する FK が他テナントの親を見つけない。
/// （採番方式の子表が、移行済みの表の宣言 PK を参照する形も含む。）
#[test]
fn allocated_child_may_reference_migrated_parent_declared_key_within_tenant() {
    let viewer = TENANTS[0];
    let tpl = build_template(viewer, true);
    let _g = CleanupGuard(tpl.clone());
    let path = unique_db_path("rls10-pk-fk-mixed");
    let _g2 = CleanupGuard(path.clone());
    std::fs::copy(&tpl, &path).expect("copy");
    let core = EngineCore::open(&path).expect("open");
    let ctx = ctx_for(viewer, true);
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut s,
        "CREATE TABLE mixed_children (lcode TEXT REFERENCES legacy_pk (code), note TEXT)",
    )
    .expect("create child of migrated parent");
    let ok = core.execute_sql_in_session(
        &ctx,
        &mut s,
        "INSERT INTO mixed_children (lcode, note) VALUES ('k1', 'n') USING OPERATION_ID 'mx-1'",
    );
    assert!(ok.is_ok(), "{ok:?}");
    // 他テナントだけが持つ親キーは、閲覧側からは存在しない扱い（どこにも無い場合と同一応答）。
    let hidden = observe(core.execute_sql_in_session(
        &ctx,
        &mut s,
        "INSERT INTO mixed_children (lcode, note) VALUES ('only-other', 'n') USING OPERATION_ID 'mx-2'",
    ));
    let nowhere = observe(core.execute_sql_in_session(
        &ctx,
        &mut s,
        "INSERT INTO mixed_children (lcode, note) VALUES ('nowhere', 'n') USING OPERATION_ID 'mx-3'",
    ));
    assert!(
        matches!(&hidden, Obs::Err { code: "23503", .. }),
        "{hidden:?}"
    );
    assert_eq!(hidden, nowhere);
}
