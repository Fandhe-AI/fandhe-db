//! データ変更 CTE（`WITH x AS (INSERT/UPDATE/DELETE ... [RETURNING ...]) <主クエリ>`）の
//! RLS・権限境界の 3 テナント一括機械検証（Issue #1878。ポインタ: RLS-10・SQL-63。
//! 関連: SQL-40〔未実装・対象外〕・SQL-23・RECOVER-1・RECOVER-11・TABLE-27）。
//!
//! 役割: `tests/rls10_write_constraint_paths.rs`（単独 DML の書き込み経路マトリクス）と対をなし、
//! データ変更 CTE に固有の読み取り点（副文の `WHERE` サブクエリの事前解決・主クエリの実体化）と
//! 文内変更集合・台帳・DDL 権限ゲートが、他テナントの行に左右されないことを固定する。
//! `sql63_data_modifying_cte_{syntax,exec,semantics}.rs` は 2 テナントでの同一応答までを確かめる
//! 機能テストで、本ファイルは閲覧テナント 3 つ（tenant-a/b/c を順に閲覧側にする）×可視性 2 モード
//! （`PolicyContext::new`／`with_visibilities`）×実行経路 2 つ（autocommit／明示トランザクション）×
//! 形の統一マトリクスとして次を固定する。production の可視性判定は期待値側で呼ばない。
//!
//! - T0 受理ゲート: 各モードで陽性対照（行を返す・期待どおりの制約違反）が実際に発生する
//! - T1 独立オラクル: 返却 id・`wire_code` をテスト側の真実値と照合し、単独 DML との差分照合
//!   （副文の本文を単独の DML として同じモードで実行した結果と一致）を行い、他テナントの
//!   トークンが応答に混入しないこと
//! - T2 baseline／flooded の完全一致: 応答（行・`wire_code`・`client_message`）と自テナントの
//!   事後の物理状態が一致する。他テナントにだけ値がある場合は「どこにも無い場合」と同じ応答
//!   （存在情報を漏らさない）になる
//! - T3 他テナントの物理不変: flooded の他 2 テナントの行が全表で事前・事後に一致する
//! - T5 行数上限のカナリア: 副文ごとの影響行数上限・主クエリの結果行数上限が他テナントの行で
//!   変わらない
//! - T6 負の対照: 捏造した違反を検査器が検出できる
//! - T7 経路間一致: autocommit と明示トランザクションで応答と自テナントの事後状態が一致する
//!
//! 追加の軸: P1 DDL 権限の有無でデータ変更 CTE の応答・対象行が変わらない（DDL 権限は RLS を
//! 広げない）、P2 DDL の形をした文の中のデータ変更 CTE は書き込みを起こさない、P3 可視性モード
//! （Public のみ／Public＋Private）が書き込み対象の軸になる、台帳キーはテナントごと、他テナントの
//! SCHEMA 修飾名は存在しない SCHEMA と同じ応答。
//!
//! 読み取りの注意: 主クエリが `docs` を読む形は、baseline／flooded それぞれの返却 body 集合を
//! seed と可視性モードから算出した期待集合と照合し、他テナントの Private トークン非混入も検査する。副文の
//! `WHERE` サブクエリが読む `tags` は、他テナントの追加行を Private にして応答の完全一致を保つ。
//!
//! 対象外: SQL-40（オブジェクト単位の権限を副文ごとに検査すること。engine に実装が無い。実装時に
//! 本マトリクスへセルを追加する）、wire-server 経由・NoSQL 表層の越境、レイテンシの統計的区別不能性。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// 疑似列 `id`・従来方式の表の契約（FK の参照アクション）を使うため、`CREATE TABLE` は従来方式へ振り向ける。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 定数・フィクスチャ ----------

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";
const TENANTS: [&str; 3] = [TENANT_A, TENANT_B, TENANT_C];

/// 物理スナップショットの走査対象（全表。FK の連鎖先を含む）。
const TABLES: [&str; 7] = [
    "docs",
    "tags",
    "uniq",
    "parents",
    "children",
    "parents_c",
    "children_c",
];

const DDL: [&str; 7] = [
    "CREATE TABLE docs (lang TEXT, score BIGINT, body TEXT)",
    "CREATE TABLE tags (lang TEXT)",
    "CREATE TABLE uniq (code TEXT UNIQUE, a TEXT, b TEXT)",
    "CREATE TABLE parents (name TEXT)",
    "CREATE TABLE children (parent_id BIGINT REFERENCES parents, note TEXT)",
    "CREATE TABLE parents_c (name TEXT)",
    "CREATE TABLE children_c (parent_id BIGINT REFERENCES parents_c ON DELETE CASCADE, note TEXT)",
];

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

/// 1 行の seed 定義（テスト側の真実値）。
struct SeedRow {
    table: &'static str,
    id: u64,
    public: bool,
    values: Vec<Value>,
}

fn row(table: &'static str, id: u64, public: bool, values: Vec<Value>) -> SeedRow {
    SeedRow {
        table,
        id,
        public,
        values,
    }
}

/// 全テナント共通の seed。`docs` は奇数 id を Public・偶数 id を Private（1,2 が ja・3,4 が en）。
/// 他表は全行 Public（陽性対照が可視性モードに依存しない）。
fn common_rows(t: &str) -> Vec<SeedRow> {
    let mut rows = Vec::new();
    for id in 1..=4u64 {
        rows.push(row(
            "docs",
            id,
            id % 2 == 1,
            vec![
                text(if id <= 2 { "ja" } else { "en" }),
                Value::BigInt(id as i64 * 10),
                text(format!("tok-{t}-docs-{id}")),
            ],
        ));
    }
    rows.push(row("tags", 1, true, vec![text("ja")]));
    for (id, code, a, b) in [(1u64, "u1", "x1", "y1"), (2, "u2", "x2", "y2")] {
        rows.push(row("uniq", id, true, vec![text(code), text(a), text(b)]));
    }
    for id in [1u64, 2] {
        rows.push(row(
            "parents",
            id,
            true,
            vec![text(format!("tok-{t}-parents-{id}"))],
        ));
    }
    rows.push(row(
        "parents_c",
        1,
        true,
        vec![text(format!("tok-{t}-parents_c-1"))],
    ));
    rows.push(row(
        "children",
        10,
        true,
        vec![Value::BigInt(1), text(format!("tok-{t}-children-10"))],
    ));
    rows.push(row(
        "children_c",
        10,
        true,
        vec![Value::BigInt(1), text(format!("tok-{t}-children_c-10"))],
    ));
    rows
}

/// 他テナントだけが持つ追加行（閲覧側の書き込みで「他テナントにだけ存在する」値になる）。
/// `tags` の追加行は Private にして、副文の `WHERE` サブクエリの読み取りに現れないことを固定する。
fn flood_extra_rows(t: &str) -> Vec<SeedRow> {
    let mut rows = Vec::new();
    for id in [9u64, 10, 11, 12] {
        rows.push(row(
            "docs",
            id,
            true,
            vec![
                text("ja"),
                Value::BigInt(id as i64),
                text(format!("tok-{t}-docs-{id}")),
            ],
        ));
    }
    rows.push(row("tags", 5, false, vec![text("en")]));
    rows.push(row(
        "uniq",
        3,
        true,
        vec![text("shared"), text("p"), text("q")],
    ));
    rows.push(row(
        "uniq",
        4,
        false,
        vec![text("fpriv"), text("pp"), text("qq")],
    ));
    rows.push(row(
        "parents",
        7,
        true,
        vec![text(format!("tok-{t}-parents-7"))],
    ));
    rows
}

/// テンプレート DB を作る（DDL は system ctx、seed は `insert_typed_row`）。
fn build_template(viewer: &str, flood: bool) -> PathBuf {
    let path = unique_db_path("rls10-dmcte-template");
    {
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open"),
            Box::new(CpuScalarProvider),
        );
        let sys = ctx_for("sys", true);
        let mut session = SessionState::default();
        session.allow_ddl();
        for ddl in DDL {
            core.execute_sql_in_session_legacy(&sys, &mut session, ddl)
                .unwrap_or_else(|e| panic!("{ddl} must succeed: {e:?}"));
        }
    }
    let storage = Storage::open(&path).expect("reopen");
    let mut tenants: Vec<(&str, Vec<SeedRow>)> = vec![(viewer, common_rows(viewer))];
    if flood {
        for t in TENANTS.iter().filter(|t| **t != viewer) {
            let mut rows = common_rows(t);
            rows.extend(flood_extra_rows(t));
            tenants.push((t, rows));
        }
    }
    for (t, rows) in tenants {
        let ctx = ctx_for(t, true);
        for r in rows {
            let op = OperationId::parse(&format!("seed-{}-{t}-{}", r.table, r.id)).expect("op id");
            engine::tenant::insert_typed_row(
                &storage,
                r.table,
                &ctx,
                r.id,
                if r.public {
                    Visibility::Public
                } else {
                    Visibility::Private
                },
                &r.values,
                &op,
            )
            .unwrap_or_else(|e| panic!("seed {} {t} {}: {e:?}", r.table, r.id));
        }
    }
    path
}

// ---------- 物理スナップショット（RLS に依存しない） ----------

type SnapValue = (bool, Vec<f32>, Vec<u8>);
/// キーは物理キーと同形の `(table, tenant_id, id)`。
type Snapshot = BTreeMap<(String, String, u64), SnapValue>;

fn physical_snapshot(path: &Path) -> Snapshot {
    let storage = Storage::open(path).expect("open for snapshot");
    let mut out = Snapshot::new();
    for table in TABLES {
        let mut after: Option<(String, u64)> = None;
        loop {
            let (page, next) = storage
                .scan_table_page(
                    table,
                    after.as_ref().map(|(t, id)| (t.as_str(), *id)),
                    10_000,
                )
                .expect("scan_table_page");
            if page.is_empty() && next.is_none() {
                break;
            }
            for r in page {
                out.insert(
                    (table.to_string(), r.tenant_id.clone(), r.id),
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
    }
    out
}

fn split_by_tenant(snap: &Snapshot, tenant: &str, keep: bool) -> Snapshot {
    snap.iter()
        .filter(|((_, t, _), _)| (t == tenant) == keep)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(hash, |h, b| (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME))
}

/// 行数＋長さ前置き FNV-1a フィンガープリント（構造同値では読み落としうる差を独立に検出する）。
fn fingerprint(snap: &Snapshot) -> (usize, u64) {
    let mut h = fnv(FNV_OFFSET_BASIS, &(snap.len() as u64).to_le_bytes());
    for ((table, tenant, id), (public, emb, meta)) in snap {
        for s in [table.as_bytes(), tenant.as_bytes()] {
            h = fnv(h, &(s.len() as u64).to_le_bytes());
            h = fnv(h, s);
        }
        h = fnv(h, &id.to_le_bytes());
        h = fnv(h, &[u8::from(*public)]);
        h = fnv(h, &(emb.len() as u64).to_le_bytes());
        for v in emb {
            h = fnv(h, &v.to_bits().to_le_bytes());
        }
        h = fnv(h, &(meta.len() as u64).to_le_bytes());
        h = fnv(h, meta);
    }
    (snap.len(), h)
}

/// 事前・事後の不変を構造同値とフィンガープリントの両方で検査する（違反は `Err`）。
fn check_unchanged(before: &Snapshot, after: &Snapshot) -> Result<(), String> {
    if before != after {
        return Err("snapshot differs (structural)".to_string());
    }
    if fingerprint(before) != fingerprint(after) {
        return Err("snapshot differs (fingerprint)".to_string());
    }
    Ok(())
}

/// 応答の全テキストに、閲覧側以外のテナントの Private 行トークン（`docs` の偶数 id）が現れる件数。
fn private_token_leaks(dump: &str, viewer: &str) -> usize {
    TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .map(|t| {
            [2, 4]
                .iter()
                .map(|id| dump.matches(&format!("tok-{t}-docs-{id}\"")).count())
                .sum::<usize>()
        })
        .sum()
}

/// 応答の全テキストに現れる `docs` 行トークン（テナント名, id）の集合。
fn docs_tokens(dump: &str) -> std::collections::BTreeSet<(&'static str, u64)> {
    let mut set = std::collections::BTreeSet::new();
    for t in TENANTS {
        for id in 1..=12u64 {
            if dump.contains(&format!("tok-{t}-docs-{id}\"")) {
                set.insert((t, id));
            }
        }
    }
    set
}

/// 主クエリが `docs` を読む形の期待集合（seed と可視性モードから算出する真実値）。
/// 自テナントは Public＋Private 込みなら全 4 行・Public のみなら奇数 id のみ。他テナントは
/// flooded のときだけ存在し、可視なのは Public 行（共通 seed の奇数 id と追加 9〜12）のみ（RLS-7）。
/// 主クエリは副文の変更を見ない（文開始時点の状態を読む）ため、副文の削除は差し引かない。
fn expected_docs(
    viewer: &str,
    allow_private: bool,
    flood: bool,
) -> std::collections::BTreeSet<(&'static str, u64)> {
    let mut set = std::collections::BTreeSet::new();
    for t in TENANTS {
        let ids: Vec<u64> = if t == viewer {
            (1..=4).collect()
        } else if flood {
            vec![1, 2, 3, 4, 9, 10, 11, 12]
        } else {
            Vec::new()
        };
        for id in ids {
            let public = id % 2 == 1 || id >= 9;
            if public || (t == viewer && allow_private) {
                set.insert((t, id));
            }
        }
    }
    set
}

/// 応答（成功の行・エラーメッセージ）に閲覧側以外のテナント名が現れる件数。
fn tenant_name_leaks(dump: &str, viewer: &str) -> usize {
    TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .map(|t| dump.matches(*t).count())
        .sum()
}

// ---------- 観測（応答の正規化） ----------

#[derive(Debug, Clone, PartialEq)]
enum Obs {
    Ok {
        /// 返却行の第 1 列（id）の数値を昇順に並べたもの（行が無ければ空）。
        ids: Vec<u64>,
        debug: String,
    },
    Err {
        code: String,
        message: String,
    },
}

fn digits(cell: &str) -> u64 {
    cell.chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn observe(res: Result<SqlOutcome, SqlSurfaceError>) -> Obs {
    match res {
        Ok(o) => {
            let mut ids: Vec<u64> = match &o {
                SqlOutcome::Query(q) => q
                    .rows
                    .iter()
                    .map(|r| {
                        digits(
                            &r.cells
                                .first()
                                .map(|c| format!("{c:?}"))
                                .unwrap_or_default(),
                        )
                    })
                    .collect(),
                SqlOutcome::Returning(x) => x.result.rows.iter().map(|r| r.id).collect(),
                _ => Vec::new(),
            };
            ids.sort_unstable();
            Obs::Ok {
                ids,
                debug: format!("{o:?}"),
            }
        }
        Err(e) => Obs::Err {
            code: e.wire_code().to_string(),
            message: e.client_message(),
        },
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ExecPath {
    Autocommit,
    ExplicitTxn,
}

const EXEC_PATHS: [ExecPath; 2] = [ExecPath::Autocommit, ExecPath::ExplicitTxn];

/// 明示トランザクションで 1 文を実行する。成功時は同一トランザクション内で読み戻して `COMMIT`、
/// エラー時は `Failed` 遷移・`COMMIT` の `25P02`・`ROLLBACK` を表明する。戻り値の 2 つ目は
/// トランザクション内読み戻しに現れた他テナントの Private トークン件数。
fn run_explicit_txn(
    core: &EngineCore,
    ctx: &PolicyContext,
    viewer: &str,
    ddl: bool,
    sql: &str,
    what: &str,
) -> (Obs, usize) {
    let mut txn: SessionTransaction = core.new_session_transaction();
    let mut session = SessionState::default();
    if ddl {
        session.allow_ddl();
    }
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "BEGIN")
        .unwrap_or_else(|e| panic!("BEGIN must succeed ({e:?}): {what}"));
    assert_eq!(txn.status(), TransactionStatus::InTransaction, "{what}");
    let obs = observe(core.execute_sql_in_txn(ctx, &mut session, &mut txn, sql));
    let mut leaks = 0;
    match &obs {
        Obs::Ok { .. } => {
            assert_eq!(txn.status(), TransactionStatus::InTransaction, "{what}");
            let r = core
                .execute_sql_in_txn(
                    ctx,
                    &mut session,
                    &mut txn,
                    "SELECT body FROM docs LIMIT 1000",
                )
                .unwrap_or_else(|e| panic!("in-txn readback failed ({e:?}): {what}"));
            leaks += private_token_leaks(&format!("{r:?}"), viewer);
            core.execute_sql_in_txn(ctx, &mut session, &mut txn, "COMMIT")
                .unwrap_or_else(|e| panic!("COMMIT must succeed ({e:?}): {what}"));
            assert_eq!(txn.status(), TransactionStatus::Idle, "{what}");
        }
        Obs::Err { .. } => {
            assert_eq!(txn.status(), TransactionStatus::Failed, "{what}");
            let e = core
                .execute_sql_in_txn(ctx, &mut session, &mut txn, "COMMIT")
                .expect_err("COMMIT in a failed transaction must be rejected");
            assert_eq!(e.wire_code(), "25P02", "{what}");
            assert_eq!(txn.status(), TransactionStatus::Failed, "{what}");
            core.execute_sql_in_txn(ctx, &mut session, &mut txn, "ROLLBACK")
                .unwrap_or_else(|e| panic!("ROLLBACK must succeed ({e:?}): {what}"));
            assert_eq!(txn.status(), TransactionStatus::Idle, "{what}");
        }
    }
    (obs, leaks)
}

// ---------- 形 ----------

/// テスト側の真実値。`priv_mode`／`pub_mode` は（Public＋Private, Public のみ）での期待。
#[derive(Clone, Copy, Debug)]
enum Out {
    /// 成功し、返却 id（昇順）がこれと一致する。
    Ids(&'static [u64]),
    /// `wire_code` がこれのエラー。
    Code(&'static str),
}

struct Shape {
    name: &'static str,
    /// データ変更 CTE（`{v}` は閲覧テナント名に置換）。
    sql: &'static str,
    /// 副文の本文を単独の DML（RETURNING id 付き）にしたもの。差分照合に使う。
    alone: Option<&'static str>,
    priv_mode: Out,
    pub_mode: Out,
    /// 主クエリが `docs` 等の共有可視な表を読む形（他テナントの Public 行が読み取りで可視なため
    /// 応答の完全一致ではなく Private トークン非混入で検査する）。
    shared_read: bool,
}

const fn s(
    name: &'static str,
    sql: &'static str,
    alone: Option<&'static str>,
    priv_mode: Out,
    pub_mode: Out,
) -> Shape {
    Shape {
        name,
        sql,
        alone,
        priv_mode,
        pub_mode,
        shared_read: false,
    }
}

fn shapes() -> Vec<Shape> {
    use Out::*;
    let ins = |id: u64, extra: &str| -> String {
        format!(
            "INSERT INTO docs (id, lang, score, body) VALUES ({id}, 'ja', 1, 'tok-{{v}}-n{id}'){extra}"
        )
    };
    // &'static str が要るため、format した文字列は leak する（テスト専用の定数テーブル）。
    let leak = |x: String| -> &'static str { Box::leak(x.into_boxed_str()) };
    let ins20 = leak(format!(
        "WITH d AS ({} RETURNING id) SELECT id FROM d LIMIT 100",
        ins(20, "")
    ));
    let ins20_alone = leak(format!("{} RETURNING id", ins(20, "")));
    let up9 = leak(format!(
        "WITH d AS ({} ON CONFLICT (id) DO UPDATE SET score = 5 RETURNING id) SELECT id FROM d LIMIT 100",
        ins(9, "")
    ));
    let up9_alone = leak(format!(
        "{} ON CONFLICT (id) DO UPDATE SET score = 5 RETURNING id",
        ins(9, "")
    ));
    let up1 = leak(format!(
        "WITH d AS ({} ON CONFLICT (id) DO UPDATE SET score = 5 RETURNING id) SELECT id FROM d LIMIT 100",
        ins(1, "")
    ));
    let up1_alone = leak(format!(
        "{} ON CONFLICT (id) DO UPDATE SET score = 5 RETURNING id",
        ins(1, "")
    ));
    let up2 = leak(format!(
        "WITH d AS ({} ON CONFLICT (id) DO UPDATE SET score = 5 RETURNING id) SELECT id FROM d LIMIT 100",
        ins(2, "")
    ));
    let up2_alone = leak(format!(
        "{} ON CONFLICT (id) DO UPDATE SET score = 5 RETURNING id",
        ins(2, "")
    ));
    let nothing9 = leak(format!(
        "WITH d AS ({} ON CONFLICT (id) DO NOTHING RETURNING id) SELECT id FROM d LIMIT 100",
        ins(9, "")
    ));
    let nothing9_alone = leak(format!(
        "{} ON CONFLICT (id) DO NOTHING RETURNING id",
        ins(9, "")
    ));
    let nothing1 = leak(format!(
        "WITH d AS ({} ON CONFLICT (id) DO NOTHING RETURNING id) SELECT id FROM d LIMIT 100",
        ins(1, "")
    ));
    let mut v = vec![
        // ---- S1 単一の副文（単独 DML との差分照合つき） ----
        s("upd-pred-ja", "WITH d AS (UPDATE docs SET score = 0 WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 100", Some("UPDATE docs SET score = 0 WHERE lang = 'ja' RETURNING id"), Ids(&[1, 2]), Ids(&[1])),
        s("del-pred-ja", "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 100", Some("DELETE FROM docs WHERE lang = 'ja' RETURNING id"), Ids(&[1, 2]), Ids(&[1])),
        s("upd-id-own-private", "WITH d AS (UPDATE docs SET score = 0 WHERE id = 2 RETURNING id) SELECT id FROM d LIMIT 100", Some("UPDATE docs SET score = 0 WHERE id = 2 RETURNING id"), Ids(&[2]), Ids(&[])),
        s("upd-id-foreign-only", "WITH d AS (UPDATE docs SET score = 0 WHERE id = 9 RETURNING id) SELECT id FROM d LIMIT 100", Some("UPDATE docs SET score = 0 WHERE id = 9 RETURNING id"), Ids(&[]), Ids(&[])),
        s("del-all", "WITH d AS (DELETE FROM docs WHERE id > 0 RETURNING id) SELECT id FROM d LIMIT 100", Some("DELETE FROM docs WHERE id > 0 RETURNING id"), Ids(&[1, 2, 3, 4]), Ids(&[1, 3])),
        s("ins-new-id", ins20, Some(ins20_alone), Ids(&[20]), Code("XX000")),
        // ---- S2 段 A（副文の WHERE サブクエリ）。他テナントの Private な tags 行（'en'）が混ざらない ----
        s("del-in-tags", "WITH d AS (DELETE FROM docs WHERE lang IN (SELECT lang FROM tags) RETURNING id) SELECT id FROM d LIMIT 100", Some("DELETE FROM docs WHERE lang IN (SELECT lang FROM tags) RETURNING id"), Ids(&[1, 2]), Ids(&[1])),
        s("upd-exists-en-tag", "WITH d AS (UPDATE docs SET score = 0 WHERE EXISTS (SELECT lang FROM tags WHERE lang = 'en') RETURNING id) SELECT id FROM d LIMIT 100", Some("UPDATE docs SET score = 0 WHERE EXISTS (SELECT lang FROM tags WHERE lang = 'en') RETURNING id"), Ids(&[]), Ids(&[])),
        // ---- S4 複数の副文・文内変更集合・参照されない副文 ----
        s("del-then-upd-same-id", "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), b AS (UPDATE docs SET score = 7 WHERE id = 1 RETURNING id) SELECT id FROM b LIMIT 100", None, Ids(&[]), Ids(&[])),
        s("del-then-upd-all", "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), b AS (UPDATE docs SET score = 7 WHERE id > 0 RETURNING id) SELECT id FROM b LIMIT 100", None, Ids(&[2, 3, 4]), Ids(&[3])),
        s("ins-then-upd-new", "WITH a AS (INSERT INTO docs (id, lang, score, body) VALUES (21, 'ja', 1, 'tok-{v}-n21') RETURNING id), b AS (UPDATE docs SET score = 7 WHERE id = 21 RETURNING id) SELECT id FROM b LIMIT 100", None, Ids(&[]), Code("XX000")),
        s("unreferenced-substatements", "WITH a AS (DELETE FROM docs WHERE id = 1), b AS (UPDATE docs SET score = 9 WHERE id = 3 RETURNING id) SELECT id FROM b LIMIT 100", None, Ids(&[3]), Ids(&[3])),
        // ---- S5 UPSERT の副文 ----
        s("upsert-own-public", up1, Some(up1_alone), Ids(&[1]), Ids(&[1])),
        s("upsert-own-private", up2, Some(up2_alone), Ids(&[2]), Code("42501")),
        s("upsert-foreign-only", up9, Some(up9_alone), Ids(&[9]), Code("XX000")),
        s("upsert-nothing-foreign-only", nothing9, Some(nothing9_alone), Ids(&[9]), Code("XX000")),
        s("upsert-nothing-own", nothing1, None, Ids(&[]), Ids(&[])),
        // ---- S6 制約 ----
        s("uniq-foreign-code", "WITH d AS (INSERT INTO uniq (id, code, a, b) VALUES (30, 'shared', 'n1', 'n2') RETURNING id) SELECT id FROM d LIMIT 100", Some("INSERT INTO uniq (id, code, a, b) VALUES (30, 'shared', 'n1', 'n2') RETURNING id"), Ids(&[30]), Code("XX000")),
        s("uniq-foreign-private-code", "WITH d AS (INSERT INTO uniq (id, code, a, b) VALUES (30, 'fpriv', 'n1', 'n2') RETURNING id) SELECT id FROM d LIMIT 100", Some("INSERT INTO uniq (id, code, a, b) VALUES (30, 'fpriv', 'n1', 'n2') RETURNING id"), Ids(&[30]), Code("XX000")),
        s("uniq-own-code", "WITH d AS (INSERT INTO uniq (id, code, a, b) VALUES (30, 'u1', 'n1', 'n2') RETURNING id) SELECT id FROM d LIMIT 100", Some("INSERT INTO uniq (id, code, a, b) VALUES (30, 'u1', 'n1', 'n2') RETURNING id"), Code("23505"), Code("XX000")),
        s("fk-foreign-only-parent", "WITH d AS (INSERT INTO children (id, parent_id, note) VALUES (30, 7, 'n') RETURNING id) SELECT id FROM d LIMIT 100", Some("INSERT INTO children (id, parent_id, note) VALUES (30, 7, 'n') RETURNING id"), Code("23503"), Code("XX000")),
        s("fk-own-parent", "WITH d AS (INSERT INTO children (id, parent_id, note) VALUES (30, 2, 'n') RETURNING id) SELECT id FROM d LIMIT 100", Some("INSERT INTO children (id, parent_id, note) VALUES (30, 2, 'n') RETURNING id"), Ids(&[30]), Code("XX000")),
        s("two-inserts-same-id", "WITH a AS (INSERT INTO docs (id, lang, score, body) VALUES (40, 'ja', 1, 'x') RETURNING id), b AS (INSERT INTO docs (id, lang, score, body) VALUES (40, 'ja', 1, 'y') RETURNING id) SELECT id FROM a LIMIT 100", None, Code("23505"), Code("XX000")),
        // ---- S7 原子性（先の副文の書き込みが後の副文のエラーで取り消される） ----
        s("atomic-del-then-dup-insert", "WITH a AS (DELETE FROM docs WHERE id = 2 RETURNING id), b AS (INSERT INTO docs (id, lang, score, body) VALUES (3, 'ja', 1, 'x') RETURNING id) SELECT id FROM a LIMIT 100", None, Code("23505"), Code("XX000")),
        s("atomic-del-then-uniq-dup", "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), b AS (INSERT INTO uniq (id, code, a, b) VALUES (30, 'u1', 'n1', 'n2') RETURNING id) SELECT id FROM a LIMIT 100", None, Code("23505"), Code("XX000")),
        // 同じ表を対象にする ON CONFLICT 付き副文は、先の副文との組み合わせがゲートで拒否される。
        s("gate-upsert-after-delete-same-table", "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), b AS (INSERT INTO docs (id, lang, score, body) VALUES (2, 'ja', 1, 'x') ON CONFLICT (id) DO UPDATE SET score = 1 RETURNING id) SELECT id FROM a LIMIT 100", None, Code("0A000"), Code("0A000")),
        // ---- S8 ゲートと静的エラー（書き込み前に拒否） ----
        s("gate-join-main", "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT d.id FROM d JOIN docs ON d.id = docs.id LIMIT 100", None, Code("0A000"), Code("0A000")),
        s("gate-no-returning-ref", "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT id FROM d LIMIT 100", None, Code("0A000"), Code("0A000")),
        s("gate-onconflict-same-table", "WITH a AS (INSERT INTO docs (id, lang, score, body) VALUES (1, 'ja', 1, 'x') ON CONFLICT (id) DO NOTHING RETURNING id), b AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) SELECT id FROM b LIMIT 100", None, Code("0A000"), Code("0A000")),
        s("gate-referential-action-multi", "WITH a AS (DELETE FROM parents_c WHERE id = 1 RETURNING id), b AS (UPDATE children_c SET parent_id = 1 WHERE id = 10 RETURNING id) SELECT id FROM a LIMIT 100", None, Code("0A000"), Code("0A000")),
        s("no-such-table", "WITH d AS (DELETE FROM nosuch WHERE id = 1 RETURNING id) SELECT id FROM d LIMIT 100", None, Code("42P01"), Code("42P01")),
        s("no-such-column", "WITH d AS (UPDATE docs SET nocol = 1 WHERE id = 1 RETURNING id) SELECT id FROM d LIMIT 100", None, Code("42703"), Code("42703")),
        s("syntax-error", "WITH d AS (DELETE FROM docs WHERE RETURNING id) SELECT id FROM d LIMIT 100", None, Code("42601"), Code("42601")),
    ];
    // ---- S3 段 B（主クエリが共有可視な表を読む形）。応答は完全一致でなく Private トークン非混入で検査 ----
    for (name, sql) in [
        (
            "main-reads-docs",
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT body FROM docs LIMIT 1000",
        ),
        (
            "main-reads-docs-with-returning-sub",
            "WITH d AS (UPDATE docs SET score = 1 WHERE id = 2 RETURNING id) SELECT body FROM docs LIMIT 1000",
        ),
        (
            "main-reads-select-cte",
            "WITH s AS (SELECT body FROM docs), d AS (DELETE FROM docs WHERE id = 1 RETURNING id) SELECT body FROM s",
        ),
    ] {
        v.push(Shape {
            name,
            sql,
            alone: None,
            priv_mode: Out::Ids(&[]),
            pub_mode: Out::Ids(&[]),
            shared_read: true,
        });
    }
    v
}

// ---------- 実行・レポート ----------

struct Cell {
    obs: Obs,
    own_before: Snapshot,
    own_after: Snapshot,
    foreign_before: Snapshot,
    foreign_after: Snapshot,
    leaks: usize,
}

/// テンプレートの複製を開いて 1 文を実行し、事前・事後の物理状態と漏えい件数を返す。
fn run_cell(
    template: &Path,
    viewer: &str,
    allow_private: bool,
    ddl: bool,
    sql: &str,
    exec: ExecPath,
    configure: impl FnOnce(EngineCore) -> EngineCore,
) -> Cell {
    let path = unique_db_path("rls10-dmcte-run");
    let _guard = CleanupGuard(path.clone());
    std::fs::copy(template, &path).expect("copy template");
    let pre = physical_snapshot(&path);
    let ctx = ctx_for(viewer, allow_private);
    let sql = sql.replace("{v}", viewer);
    let mut leaks = 0;
    let obs;
    {
        let core = configure(EngineCore::from_storage(
            Storage::open(&path).expect("open run db"),
            Box::new(CpuScalarProvider),
        ));
        obs = match exec {
            ExecPath::Autocommit => {
                let mut session = SessionState::default();
                if ddl {
                    session.allow_ddl();
                }
                observe(core.execute_sql_in_session(&ctx, &mut session, &sql))
            }
            ExecPath::ExplicitTxn => {
                let what = format!("viewer={viewer} allow_private={allow_private} sql={sql}");
                let (o, l) = run_explicit_txn(&core, &ctx, viewer, ddl, &sql, &what);
                leaks += l;
                o
            }
        };
        let all = ctx_for(viewer, true);
        // 結果行数の上限を絞った構成では読み戻しが `54000` 等で拒否されうるため、成功時だけ走査する。
        if let Ok(r) = core.execute_sql(&all, "SELECT body FROM docs LIMIT 1000") {
            leaks += private_token_leaks(&format!("{r:?}"), viewer);
        }
    }
    let post = physical_snapshot(&path);
    leaks += match &obs {
        Obs::Ok { debug, .. } => private_token_leaks(debug, viewer),
        Obs::Err { message, .. } => tenant_name_leaks(message, viewer),
    };
    Cell {
        obs,
        own_before: split_by_tenant(&pre, viewer, true),
        own_after: split_by_tenant(&post, viewer, true),
        foreign_before: split_by_tenant(&pre, viewer, false),
        foreign_after: split_by_tenant(&post, viewer, false),
        leaks,
    }
}

struct Run {
    viewer: &'static str,
    allow_private: bool,
    exec: ExecPath,
    shape: &'static str,
    shared_read: bool,
    want: Out,
    base: Cell,
    flood: Cell,
    /// 単独 DML との差分照合用（autocommit のみ。`alone` がある形）。
    alone_base: Option<Cell>,
    alone_flood: Option<Cell>,
}

fn label(r: &Run) -> String {
    format!(
        "viewer={} allow_private={} path={:?} shape={}",
        r.viewer, r.allow_private, r.exec, r.shape
    )
}

/// 総試行数（閲覧側 3 × モード 2 × 経路 2 × 形）。ループの空回り防止。
fn expected_run_count() -> usize {
    3 * 2 * EXEC_PATHS.len() * shapes().len()
}

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
            for allow_private in [true, false] {
                for exec in EXEC_PATHS {
                    for shape in &shapes {
                        let go = |tpl: &Path, sql: &str, e: ExecPath| {
                            run_cell(tpl, viewer, allow_private, false, sql, e, |c| c)
                        };
                        let (alone_base, alone_flood) = match (shape.alone, exec) {
                            (Some(a), ExecPath::Autocommit) => (
                                Some(go(&base_tpl, a, ExecPath::Autocommit)),
                                Some(go(&flood_tpl, a, ExecPath::Autocommit)),
                            ),
                            _ => (None, None),
                        };
                        runs.push(Run {
                            viewer,
                            allow_private,
                            exec,
                            shape: shape.name,
                            shared_read: shape.shared_read,
                            want: if allow_private {
                                shape.priv_mode
                            } else {
                                shape.pub_mode
                            },
                            base: go(&base_tpl, shape.sql, exec),
                            flood: go(&flood_tpl, shape.sql, exec),
                            alone_base,
                            alone_flood,
                        });
                    }
                }
            }
        }
        runs
    })
}

// ---------- マトリクスのテスト ----------

#[test]
fn run_count_is_fixed() {
    assert_eq!(report().len(), expected_run_count());
}

/// T0 受理ゲート: 各（閲覧側, モード, 経路）で陽性対照が実際に発生する。
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
                let hits = scoped
                    .iter()
                    .filter(|r| matches!(&r.flood.obs, Obs::Ok { ids, .. } if !ids.is_empty()))
                    .count();
                assert!(
                    hits >= 5,
                    "too few successful writes ({hits}): {viewer} {allow_private} {exec:?}"
                );
                // Public のみモードの INSERT 副文は制約検査より前に `XX000` で拒否される（単独 DML と同じ）。
                let violation_codes: &[&str] = if allow_private {
                    &["23505", "23503"]
                } else {
                    &["XX000"]
                };
                let violations = scoped
                    .iter()
                    .filter(|r| matches!(&r.flood.obs, Obs::Err { code, .. } if violation_codes.contains(&code.as_str())))
                    .count();
                assert!(
                    violations >= 2,
                    "constraint violations not observed: {viewer} {allow_private} {exec:?}"
                );
                let gates = scoped
                    .iter()
                    .filter(|r| matches!(&r.flood.obs, Obs::Err { code, .. } if code == "0A000"))
                    .count();
                assert!(
                    gates >= 3,
                    "gates not observed: {viewer} {allow_private} {exec:?}"
                );
            }
        }
    }
}

/// T1 独立オラクル: 真実値との照合・他テナントのトークン非混入・失敗時の副作用ゼロ。
#[test]
fn t1_independent_oracle_and_no_foreign_tokens() {
    let mut bad = std::collections::BTreeSet::new();
    for r in report() {
        for (which, cell) in [("baseline", &r.base), ("flooded", &r.flood)] {
            if !r.shared_read {
                let ok = match (r.want, &cell.obs) {
                    (Out::Ids(want), Obs::Ok { ids, .. }) => ids.as_slice() == want,
                    (Out::Code(want), Obs::Err { code, .. }) => code == want,
                    _ => false,
                };
                if !ok {
                    bad.insert(format!(
                        "{which} private={} shape={} want={:?} got={:?}",
                        r.allow_private,
                        r.shape,
                        r.want,
                        match &cell.obs {
                            Obs::Ok { ids, .. } => format!("{ids:?}"),
                            Obs::Err { code, .. } => code.clone(),
                        }
                    ));
                }
            } else {
                let Obs::Ok { debug, .. } = &cell.obs else {
                    panic!("{which}: {}: {:?}", label(r), cell.obs);
                };
                // 返却内容を seed と可視性モードから算出した期待集合と照合する（空返却や
                // Public のみモードでの自テナント Private 行の返却を検出する）。
                assert_eq!(
                    docs_tokens(debug),
                    expected_docs(r.viewer, r.allow_private, which == "flooded"),
                    "{which}: {}",
                    label(r)
                );
            }
            if matches!(cell.obs, Obs::Err { .. }) {
                check_unchanged(&cell.own_before, &cell.own_after).unwrap_or_else(|e| {
                    panic!(
                        "{which}: own state changed by a failed statement ({e}): {}",
                        label(r)
                    )
                });
            }
            assert_eq!(cell.leaks, 0, "{which}: foreign token leaked: {}", label(r));
        }
    }
    assert!(
        bad.is_empty(),
        "oracle mismatches:\n{}",
        bad.into_iter().collect::<Vec<_>>().join("\n")
    );
}

/// T1 差分照合: 副文の本文を単独 DML として同じモードで実行した結果と一致する
/// （返却 id・`wire_code`・自テナントの事後の物理状態）。
#[test]
fn substatements_match_the_standalone_dml() {
    let mut compared = 0;
    for r in report() {
        let (Some(ab), Some(af)) = (&r.alone_base, &r.alone_flood) else {
            continue;
        };
        for (which, cte, alone) in [("baseline", &r.base, ab), ("flooded", &r.flood, af)] {
            match (&cte.obs, &alone.obs) {
                (Obs::Ok { ids: a, .. }, Obs::Ok { ids: b, .. }) => {
                    assert_eq!(a, b, "{which}: ids: {}", label(r))
                }
                (Obs::Err { code: a, .. }, Obs::Err { code: b, .. }) => {
                    assert_eq!(a, b, "{which}: code: {}", label(r))
                }
                (a, b) => panic!("{which}: CTE {a:?} vs standalone {b:?}: {}", label(r)),
            }
            assert_eq!(
                cte.own_after,
                alone.own_after,
                "{which}: own state: {}",
                label(r)
            );
            compared += 1;
        }
    }
    assert!(compared >= 3 * 2 * 2 * 15, "compared={compared}");
}

/// T2 baseline／flooded の完全一致（応答・自テナントの事後状態）。他テナントにだけ値がある場合も
/// どこにも無い場合と同じ応答になる（存在情報を漏らさない）。
#[test]
fn t2_responses_do_not_depend_on_other_tenants() {
    for r in report() {
        if r.shared_read {
            // 読み取りで可視な他テナントの Public 行が応答に入りうるため、自テナントの事後状態だけを比べる。
            assert_eq!(r.base.own_after, r.flood.own_after, "{}", label(r));
            continue;
        }
        assert_eq!(r.base.obs, r.flood.obs, "{}", label(r));
        assert_eq!(r.base.own_after, r.flood.own_after, "{}", label(r));
    }
}

/// T3 他テナントの物理不変（全表・構造同値＋フィンガープリント）。
#[test]
fn t3_foreign_rows_are_physically_unchanged() {
    for r in report() {
        check_unchanged(&r.flood.foreign_before, &r.flood.foreign_after)
            .unwrap_or_else(|e| panic!("foreign rows changed ({e}): {}", label(r)));
        assert!(!r.flood.foreign_before.is_empty(), "{}", label(r));
    }
}

/// T7 経路間一致: autocommit と明示トランザクションで応答と自テナントの事後状態が一致する。
#[test]
fn t7_paths_agree() {
    let mut checked = 0;
    for a in report().iter().filter(|r| r.exec == ExecPath::Autocommit) {
        let b = report()
            .iter()
            .find(|r| {
                r.exec == ExecPath::ExplicitTxn
                    && r.viewer == a.viewer
                    && r.allow_private == a.allow_private
                    && r.shape == a.shape
            })
            .expect("paired run");
        assert_eq!(a.flood.obs, b.flood.obs, "{}", label(a));
        assert_eq!(a.flood.own_after, b.flood.own_after, "{}", label(a));
        checked += 1;
    }
    assert_eq!(checked, expected_run_count() / 2);
}

// ---------- 個別シナリオ ----------

fn open_run(template: &Path) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("rls10-dmcte-scn");
    let guard = CleanupGuard(path.clone());
    std::fs::copy(template, &path).expect("copy template");
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open"),
        Box::new(CpuScalarProvider),
    );
    (core, guard)
}

fn run_sql(core: &EngineCore, tenant: &str, allow_private: bool, ddl: bool, sql: &str) -> Obs {
    let mut session = SessionState::default();
    if ddl {
        session.allow_ddl();
    }
    observe(core.execute_sql_in_session(&ctx_for(tenant, allow_private), &mut session, sql))
}

/// P1: DDL 権限の有無でデータ変更 CTE の応答・対象行・事後状態が変わらない（DDL 権限は RLS を広げない）。
#[test]
fn p1_ddl_permission_does_not_widen_data_modifying_ctes() {
    let tpl = build_template(TENANT_A, true);
    let _g = CleanupGuard(tpl.clone());
    let mut compared = 0;
    for allow_private in [true, false] {
        for shape in shapes().iter().filter(|s| !s.shared_read) {
            let cells: Vec<Cell> = [false, true]
                .iter()
                .map(|ddl| {
                    run_cell(
                        &tpl,
                        TENANT_A,
                        allow_private,
                        *ddl,
                        shape.sql,
                        ExecPath::Autocommit,
                        |c| c,
                    )
                })
                .collect();
            assert_eq!(
                cells[0].obs, cells[1].obs,
                "{} private={allow_private}",
                shape.name
            );
            assert_eq!(cells[0].own_after, cells[1].own_after, "{}", shape.name);
            assert_eq!(
                cells[0].foreign_after, cells[1].foreign_after,
                "{}",
                shape.name
            );
            check_unchanged(&cells[1].foreign_before, &cells[1].foreign_after)
                .expect("foreign unchanged");
            compared += 1;
        }
    }
    assert!(compared >= 2 * 30);
}

/// P2: DDL の形をした文の中のデータ変更 CTE は書き込みを起こさず、応答は閲覧側以外のテナントの
/// 行・表の有無に左右されない。
#[test]
fn p2_ddl_shaped_statements_never_write() {
    let body = "WITH d AS (DELETE FROM docs WHERE id = 1 RETURNING id) SELECT id FROM d";
    let stmts = [
        format!("CREATE VIEW dv AS {body}"),
        format!("CREATE TABLE ct AS {body}"),
        format!("CREATE MATERIALIZED VIEW mv AS {body}"),
        "WITH d AS (DROP TABLE docs) SELECT 1".to_string(),
        "WITH d AS (CREATE TABLE zz (n BIGINT)) SELECT 1".to_string(),
        "WITH d AS (TRUNCATE docs) SELECT 1".to_string(),
    ];
    for viewer in TENANTS {
        let base_tpl = build_template(viewer, false);
        let flood_tpl = build_template(viewer, true);
        let _g1 = CleanupGuard(base_tpl.clone());
        let _g2 = CleanupGuard(flood_tpl.clone());
        for allow_private in [true, false] {
            for ddl in [false, true] {
                for sql in &stmts {
                    let b = run_cell(
                        &base_tpl,
                        viewer,
                        allow_private,
                        ddl,
                        sql,
                        ExecPath::Autocommit,
                        |c| c,
                    );
                    let f = run_cell(
                        &flood_tpl,
                        viewer,
                        allow_private,
                        ddl,
                        sql,
                        ExecPath::Autocommit,
                        |c| c,
                    );
                    let what =
                        format!("viewer={viewer} private={allow_private} ddl={ddl} sql={sql}");
                    // 書き込みゼロ（自テナント・他テナントとも）。
                    check_unchanged(&b.own_before, &b.own_after)
                        .unwrap_or_else(|e| panic!("{e}: {what}"));
                    check_unchanged(&f.own_before, &f.own_after)
                        .unwrap_or_else(|e| panic!("{e}: {what}"));
                    check_unchanged(&f.foreign_before, &f.foreign_after)
                        .unwrap_or_else(|e| panic!("{e}: {what}"));
                    // 応答は他テナントの行・表の有無で変わらない。
                    assert_eq!(b.obs, f.obs, "{what}");
                    // 成功しない（DDL の中のデータ変更 CTE は受理されない）。
                    assert!(matches!(b.obs, Obs::Err { .. }), "{what}: {:?}", b.obs);
                    assert_eq!(f.leaks, 0, "{what}");
                }
            }
        }
    }
}

/// T5 行数上限のカナリア: 影響行数の上限を自テナントの該当件数ちょうどにすると flooded でも成功し、
/// 1 少なくすると `54000`。上限は副文ごとに判定される。主クエリの結果行数上限も同様。
#[test]
fn t5_row_limits_are_not_moved_by_other_tenants() {
    let sql =
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 100";
    let sql_nolimit =
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d";
    let limits = |n: usize| DmlLimits {
        max_affected_rows: std::num::NonZeroUsize::new(n),
        max_insert_rows_per_statement: std::num::NonZeroUsize::new(1000),
    };
    for viewer in TENANTS {
        let base_tpl = build_template(viewer, false);
        let flood_tpl = build_template(viewer, true);
        let _g1 = CleanupGuard(base_tpl.clone());
        let _g2 = CleanupGuard(flood_tpl.clone());
        for tpl in [&base_tpl, &flood_tpl] {
            // 自テナントの ja は 2 行（Public＋Private 込み）。
            let ok = run_cell(tpl, viewer, true, false, sql, ExecPath::Autocommit, |c| {
                c.with_dml_limits(limits(2))
            });
            assert!(
                matches!(&ok.obs, Obs::Ok { ids, .. } if ids == &[1, 2]),
                "{viewer}: {:?}",
                ok.obs
            );
            let over = run_cell(tpl, viewer, true, false, sql, ExecPath::Autocommit, |c| {
                c.with_dml_limits(limits(1))
            });
            assert!(
                matches!(&over.obs, Obs::Err { code, .. } if code == "54000"),
                "{viewer}: {:?}",
                over.obs
            );
            check_unchanged(&over.own_before, &over.own_after).expect("rolled back");
            // 副文ごとの判定: 2 つ目だけが 1 行を超える。1 つ目の書き込みも取り消される。
            let two = "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), \
                       b AS (UPDATE docs SET score = 3 WHERE id > 1 RETURNING id) SELECT id FROM a LIMIT 100";
            let per = run_cell(tpl, viewer, true, false, two, ExecPath::Autocommit, |c| {
                c.with_dml_limits(limits(1))
            });
            assert!(
                matches!(&per.obs, Obs::Err { code, .. } if code == "54000"),
                "{viewer}: {:?}",
                per.obs
            );
            check_unchanged(&per.own_before, &per.own_after).expect("rolled back");
            // 主クエリの結果行数上限（ResultLimits）。
            let main_ok = run_cell(
                tpl,
                viewer,
                true,
                false,
                sql_nolimit,
                ExecPath::Autocommit,
                |c| c.with_result_limits(ResultLimits::new(2)),
            );
            assert!(
                matches!(main_ok.obs, Obs::Ok { .. }),
                "{viewer}: {:?}",
                main_ok.obs
            );
            let main_over = run_cell(
                tpl,
                viewer,
                true,
                false,
                sql_nolimit,
                ExecPath::Autocommit,
                |c| c.with_result_limits(ResultLimits::new(1)),
            );
            assert!(
                matches!(&main_over.obs, Obs::Err { code, .. } if code == "54000"),
                "{viewer}: {:?}",
                main_over.obs
            );
            check_unchanged(&main_over.own_before, &main_over.own_after).expect("rolled back");
        }
    }
}

/// 台帳キーは `(tenant, table, operation_id)`: 他テナントが使った operation_id を使っても成功し、
/// 応答は未使用のときと同一。同じテナントの再送は `23505` で何も実行しない。
#[test]
fn ledger_keys_are_per_tenant() {
    let sql = "WITH d AS (UPDATE docs SET score = 99 WHERE lang = 'ja' RETURNING id) \
               SELECT id FROM d LIMIT 100 USING OPERATION_ID 'op-shared'";
    let tpl = build_template(TENANT_B, true);
    let _g = CleanupGuard(tpl.clone());
    let (core1, _g1) = open_run(&tpl);
    let (core2, _g2) = open_run(&tpl);
    // core1: tenant-a が先に使う。core2: tenant-a は使わない。
    let a = run_sql(&core1, TENANT_A, true, false, sql);
    assert!(
        matches!(&a, Obs::Ok { ids, .. } if ids == &[1, 2, 9, 10, 11, 12]),
        "{a:?}"
    );
    let b_after_a = run_sql(&core1, TENANT_B, true, false, sql);
    let b_alone = run_sql(&core2, TENANT_B, true, false, sql);
    assert!(
        matches!(&b_alone, Obs::Ok { ids, .. } if ids == &[1, 2]),
        "{b_alone:?}"
    );
    assert_eq!(b_after_a, b_alone);
    // 同じテナントの再送は 23505。
    let again = run_sql(&core1, TENANT_B, true, false, sql);
    assert!(
        matches!(&again, Obs::Err { code, .. } if code == "23505"),
        "{again:?}"
    );
    // 別の内容での再利用は 22023。
    let other = "WITH d AS (UPDATE docs SET score = 1 WHERE lang = 'en' RETURNING id) \
                 SELECT id FROM d LIMIT 100 USING OPERATION_ID 'op-shared'";
    let reuse = run_sql(&core1, TENANT_B, true, false, other);
    assert!(
        matches!(&reuse, Obs::Err { code, .. } if code == "22023"),
        "{reuse:?}"
    );
}

/// SCHEMA 修飾: 他テナントが作った SCHEMA の表へのデータ変更 CTE は、存在しない SCHEMA と同じ応答で、
/// 所有テナントの行は変わらない。
#[test]
fn foreign_schema_qualified_targets_look_like_missing_schemas() {
    let tpl = build_template(TENANT_B, true);
    let _g = CleanupGuard(tpl.clone());
    let (core, _g1) = open_run(&tpl);
    let mut owner = SessionState::default();
    owner.allow_ddl();
    let a = ctx_for(TENANT_A, true);
    for ddl in [
        "CREATE SCHEMA sa",
        "CREATE TABLE sa.t (id BIGINT PRIMARY KEY, name TEXT)",
        "INSERT INTO sa.t (id, name) VALUES (1, 'x')",
    ] {
        core.execute_sql_in_session(&a, &mut owner, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    let before = physical_snapshot_of(&core);
    let probe = |schema: &str| {
        run_sql(
            &core,
            TENANT_B,
            true,
            false,
            &format!("WITH d AS (DELETE FROM {schema}.t WHERE id > 0 RETURNING id) SELECT id FROM d LIMIT 100"),
        )
    };
    let foreign = probe("sa");
    let missing = probe("nosuch");
    assert!(matches!(&foreign, Obs::Err { .. }), "{foreign:?}");
    assert_eq!(
        foreign.clone().code_and_shape(),
        missing.clone().code_and_shape()
    );
    assert_eq!(before, physical_snapshot_of(&core));
}

impl Obs {
    /// スキーマ名を除いた応答の形（`wire_code` と、スキーマ名を伏せたメッセージ）。
    fn code_and_shape(self) -> (String, String) {
        match self {
            Obs::Err { code, message } => (code, message.replace("sa", "X").replace("nosuch", "X")),
            Obs::Ok { debug, .. } => ("ok".to_string(), debug),
        }
    }
}

fn physical_snapshot_of(core: &EngineCore) -> usize {
    // 表が増減しないこと（カタログ不変）の代理として、`t` の可視行数を数える。
    match core.execute_sql(&ctx_for(TENANT_A, true), "SELECT id FROM sa.t LIMIT 100") {
        Ok(q) => q.rows.len(),
        _ => 0,
    }
}

/// Describe: `parse_sql` → `describe_parsed_in_session` は書き込みを起こさず、列メタデータは
/// 閲覧側以外のテナントの行に左右されない。
#[test]
fn describe_never_writes_and_ignores_other_tenants() {
    let sql =
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 100";
    let mut metas = Vec::new();
    for flood in [false, true] {
        let tpl = build_template(TENANT_A, flood);
        let _g = CleanupGuard(tpl.clone());
        let (core, _guard) = open_run(&tpl);
        let before = visible_docs(&core);
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse must not write");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("columns");
        assert_eq!(before, visible_docs(&core), "describe must not write");
        metas.push(format!("{described:?}"));
    }
    assert_eq!(metas[0], metas[1]);
}

/// 閲覧側 tenant-a の可視 `docs` 行数（Describe が書き込まないことの観測用）。
fn visible_docs(core: &EngineCore) -> usize {
    core.execute_sql(&ctx_for(TENANT_A, true), "SELECT id FROM docs LIMIT 1000")
        .expect("readback")
        .rows
        .len()
}

// ---------- T6 負の対照 ----------

/// 捏造した違反を検査器が検出できる（検査器が常に通るだけのものでないことの確認）。
#[test]
fn t6_negative_controls_are_detected() {
    let tpl = build_template(TENANT_A, true);
    let _g = CleanupGuard(tpl.clone());
    let snap = physical_snapshot(&tpl);
    // 他テナント行の書き換え（構造）。
    let mut tampered = snap.clone();
    let key = tampered
        .keys()
        .find(|(_, t, _)| t != TENANT_A)
        .cloned()
        .expect("foreign row");
    tampered.get_mut(&key).expect("row").2.push(1);
    assert!(check_unchanged(&snap, &tampered).is_err());
    // 他テナント行の削除。
    let mut removed = snap.clone();
    removed.remove(&key);
    assert!(check_unchanged(&snap, &removed).is_err());
    assert_ne!(fingerprint(&snap), fingerprint(&tampered));
    // トークン混入の検出。
    assert_eq!(
        private_token_leaks("Text(\"tok-tenant-b-docs-2\")", TENANT_A),
        1
    );
    assert_eq!(
        private_token_leaks("Text(\"tok-tenant-a-docs-2\")", TENANT_A),
        0
    );
    assert_eq!(tenant_name_leaks("relation of tenant-b", TENANT_A), 1);
    // baseline と flooded で応答が違えば検出される。
    let a = Obs::Ok {
        ids: vec![1],
        debug: String::new(),
    };
    let b = Obs::Ok {
        ids: vec![1, 9],
        debug: String::new(),
    };
    assert_ne!(a, b);
}
