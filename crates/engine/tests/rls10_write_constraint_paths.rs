//! 書き込み経路（UPDATE／DELETE／UPSERT／TRUNCATE／複数行 INSERT／COPY FROM／
//! RETURNING）と制約検査（UNIQUE／PRIMARY KEY／FOREIGN KEY と参照動作）を
//! 横断する RLS 境界の 3 テナント一括機械検証（Issue #1202。ポインタ: RLS-10
//! (a)・(c)、RLS-9、RECOVER-4、TABLE-12・TABLE-16・TABLE-17、ERR-2）。
//!
//! 役割: `tests/rls10_relational_paths.rs`（Issue #931、RLS-10 (b) 読み取り経路）と
//! 対をなし、その対象外だった (a) 書き込み経路と (c) 制約検査を単一のマトリクスで
//! 固定する。各機能のテスト（`sql_update_single_row.rs`・`sql_predicate_dml_exec.rs`・
//! `unique_constraint.rs`・`table17_foreign_key.rs`・`fk_referential_actions.rs`・
//! `copy_from.rs`・`sql_returning.rs`・`truncate_table.rs`）は機能単位で 1〜3 件の
//! 越境テストを持つが、本ファイルは閲覧テナント 3 つ（tenant-a/b/c を順に閲覧側に
//! する）×可視性 2 モード（`PolicyContext::new`／`with_visibilities`）×全経路の統一
//! マトリクスとして、次を固定する。production の可視性判定
//! （`PolicyContext::is_visible`／`is_owner`）はオラクル側で一切呼ばない。
//!
//! - T0 受理ゲート: 各モード・各軸で陽性対照（影響行あり／期待どおりの制約違反）が
//!   実際に発生していること（空虚に通らない）
//! - T1 独立オラクル: テスト側の真実値（期待影響行数・期待 `wire_code`）との照合と、
//!   応答・読み戻しに他テナントのトークンが混入しないこと
//! - T2 応答の不変性: 同一文を baseline（閲覧テナントの行のみ）と flooded
//!   （他 2 テナントの Public／Private 行を衝突を意図して追加）で実行し、応答
//!   （影響行数・RETURNING・`wire_code`・`client_message`）と自テナントの事後の
//!   物理状態が完全一致すること
//! - T3 他テナントの物理不変: flooded の他 2 テナントの行が全テーブルで事前・事後
//!   一致すること（連鎖先の子テーブルを含む。構造同値＋FNV フィンガープリント）
//! - T4 FK／UNIQUE の存在情報秘匿: 他テナントにだけ親・値がある場合の応答が、どこ
//!   にも無い場合（baseline）と一致すること
//! - T5 3 テナント回転と試行総数の固定（ループの空回り防止）
//! - T6 負の対照: 捏造した違反を検査器が検出できること
//!
//! 可視性モードと書き込み対象: `docs` の Private 行（偶数 id）は、`id` 完全一致形の
//! UPDATE では Public のみモードで対象外（0 行。SQL-17・RLS-10 (a) の可視集合どおり）。
//! 述語形 UPDATE／DELETE は現状 Public のみモードでも自テナントの Private 行を対象に
//! するが、これは SQL-19・RLS-10 (a)（候補は可視集合の部分集合）と不一致のため、
//! 該当モードの影響行数は仕様値〜現状値の範囲（[`Count::Between`]）で検査する。
//! いずれの場合も他テナントの行は候補に入らない点は共通で、本ファイルの主張の中心。
//!
//! baseline／flooded の定義: 書き込み候補は `is_owner && is_visible` の行に限られる
//! ため、`rls10_relational_paths.rs`（他テナントの Public 行が読み取りで可視）とは
//! 異なり、baseline は「閲覧テナントの行のみ」、flooded は「それに他 2 テナントの
//! 行（同一 `id`・同一 UNIQUE 値・同一 FK 親キー・同一 `lang`）を加えたもの」とする。
//! 書き込みは状態を変えるため、実行ごとに seed 済みテンプレート DB のファイル複製を
//! 新規に開く（失敗の連鎖と実行時間の両方を避ける）。
//!
//! 対象外: レイテンシ分布の統計的区別不能性（共有 CI では不安定になるため専有環境
//! 計測の担当）、明示トランザクション内の UPDATE／DELETE、NoSQL 表層の越境
//! （同一実行器へ写像されるため本ファイルの不変性で代替。NoSQL 固有の影響行数上限は
//! `crates/wire-server/tests/nosql12_affected_rows_limit.rs` の担当）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 定数・フィクスチャ ----------

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";
const TENANTS: [&str; 3] = [TENANT_A, TENANT_B, TENANT_C];

/// 物理スナップショットの走査対象（全テーブル。連鎖先の子テーブルを含む）。
const TABLES: [&str; 9] = [
    "docs",
    "uniq",
    "pk",
    "parents",
    "children",
    "parents_c",
    "children_c",
    "parents_s",
    "children_s",
];

const DDL: [&str; 9] = [
    "CREATE TABLE docs (lang TEXT, score BIGINT, body TEXT)",
    "CREATE TABLE uniq (code TEXT UNIQUE, a TEXT, b TEXT, UNIQUE (a, b))",
    "CREATE TABLE pk (code TEXT PRIMARY KEY, n BIGINT)",
    "CREATE TABLE parents (name TEXT)",
    "CREATE TABLE children (parent_id BIGINT REFERENCES parents, note TEXT)",
    "CREATE TABLE parents_c (name TEXT)",
    "CREATE TABLE children_c (parent_id BIGINT REFERENCES parents_c ON DELETE CASCADE, note TEXT)",
    "CREATE TABLE parents_s (name TEXT)",
    "CREATE TABLE children_s (parent_id BIGINT REFERENCES parents_s ON DELETE SET NULL, note TEXT)",
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

/// 1 行の seed 定義（テスト側の真実値。production の判定に依存しない）。
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

/// 全テナント共通の seed（閲覧テナントの行そのもの。flooder も同一 `id`・同一値で
/// 衝突する）。`docs` は奇数 id を Public・偶数 id を Private とし、制約系テーブルは
/// 全行 Public（陽性対照が可視性モードに依存しないようにする）。
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
    for (id, code, a, b) in [(1u64, "u1", "x1", "y1"), (2, "u2", "x2", "y2")] {
        rows.push(row("uniq", id, true, vec![text(code), text(a), text(b)]));
    }
    rows.push(row("pk", 1, true, vec![text("k1"), Value::BigInt(1)]));
    for table in ["parents", "parents_c", "parents_s"] {
        rows.push(row(
            table,
            1,
            true,
            vec![text(format!("tok-{t}-{table}-1"))],
        ));
    }
    rows.push(row(
        "parents",
        2,
        true,
        vec![text(format!("tok-{t}-parents-2"))],
    ));
    for (table, id) in [
        ("children", 10u64),
        ("children_c", 10),
        ("children_c", 11),
        ("children_s", 10),
    ] {
        rows.push(row(
            table,
            id,
            true,
            vec![Value::BigInt(1), text(format!("tok-{t}-{table}-{id}"))],
        ));
    }
    rows
}

/// flooder（閲覧側以外の 2 テナント）だけが持つ追加行。閲覧側の書き込みで
/// 「他テナントにだけ存在する id・UNIQUE 値・FK 親」になる。
fn flood_extra_rows(t: &str) -> Vec<SeedRow> {
    let mut rows = Vec::new();
    for id in [9u64, 10, 11, 12, 13, 14] {
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
    rows.push(row("pk", 2, true, vec![text("kf"), Value::BigInt(2)]));
    rows.push(row(
        "parents",
        7,
        true,
        vec![text(format!("tok-{t}-parents-7"))],
    ));
    rows.push(row(
        "children",
        20,
        true,
        vec![Value::BigInt(7), text(format!("tok-{t}-children-20"))],
    ));
    rows.push(row(
        "children",
        21,
        true,
        vec![Value::BigInt(2), text(format!("tok-{t}-children-21"))],
    ));
    rows
}

/// テンプレート DB を作る: DDL（テナント無関係の system ctx）→ seed
/// （`insert_typed_row`。UNIQUE／FK の索引は本経路で保守される）。
fn build_template(viewer: &str, flood: bool) -> PathBuf {
    let path = unique_db_path("rls10-write-template");
    {
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open"),
            Box::new(CpuScalarProvider),
        );
        let sys = ctx_for("sys", true);
        let mut session = SessionState::default();
        session.allow_ddl();
        for ddl in DDL {
            core.execute_sql_in_session(&sys, &mut session, ddl)
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
/// キーは物理キーと同形の `(table, tenant_id, id)`（TABLE-12）。
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

/// 行数＋長さ前置き FNV-1a フィンガープリント（`tenant_breach.rs` と同じ方式。
/// 構造同値では読み落としうるフィールド単位の取りこぼしを独立に検出する）。
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

// ---------- 観測（応答の正規化） ----------

#[derive(Debug, Clone, PartialEq)]
enum Obs {
    Ok {
        affected: Option<u64>,
        /// RETURNING の返却行の `id`（出現順）。RETURNING 以外は `None`。
        returned_ids: Option<Vec<u64>>,
        debug: String,
    },
    Err {
        code: String,
        message: String,
    },
}

fn observe(res: Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>) -> Obs {
    match res {
        Ok(o) => {
            let affected = match &o {
                SqlOutcome::Update(x) => Some(x.rows_affected),
                SqlOutcome::Delete(x) => Some(x.rows_affected),
                SqlOutcome::Insert(x) => Some(x.rows_affected),
                SqlOutcome::Returning(x) => Some(x.rows_affected),
                // TRUNCATE は件数を意図的に露出しない（SQL-22）。
                _ => None,
            };
            let returned_ids = match &o {
                SqlOutcome::Returning(x) => Some(x.result.rows.iter().map(|r| r.id).collect()),
                _ => None,
            };
            Obs::Ok {
                affected,
                returned_ids,
                debug: format!("{o:?}"),
            }
        }
        Err(e) => Obs::Err {
            code: e.wire_code().to_string(),
            message: e.client_message(),
        },
    }
}

fn run_copy(core: &EngineCore, ctx: &PolicyContext, sql: &str, data: &str) -> Obs {
    let session = SessionState::default();
    let plan = match core.begin_copy(ctx, &session, sql) {
        Ok(p) => p,
        Err(e) => return observe(Err(e)),
    };
    let mut cs = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM plan"),
    };
    let res = cs
        .feed(data.as_bytes())
        .and_then(|_| cs.finish())
        .and_then(|batch| core.commit_copy_in(ctx, batch));
    match res {
        Ok(o) => Obs::Ok {
            affected: Some(o.rows_affected),
            returned_ids: None,
            debug: format!("{o:?}"),
        },
        Err(e) => observe(Err(e)),
    }
}

// ---------- 形状 ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Axis {
    Write,
    Constraint,
}

/// 影響行数の期待値。仕様と一致する値は `Exact`、現状が仕様と一致しない値は
/// 仕様値と現状値の両方を含む範囲 `Between(lo, hi)`（両端を含む）で検査する
/// （仕様違反の現状値を回帰条件として固定しない）。
#[derive(Clone, Copy, Debug)]
enum Count {
    Exact(u64),
    Between(u64, u64),
}

impl Count {
    fn admits(self, n: u64) -> bool {
        match self {
            Count::Exact(v) => n == v,
            Count::Between(lo, hi) => lo <= n && n <= hi,
        }
    }
}

/// RETURNING の返却行（`id`）の期待値。仕様と一致する値は出現順の完全一致 `Exact`、
/// 現状が SQL-21 と一致しない値は `Between` で `must ⊆ 返却 ⊆ within` を検査する
/// （`within` は当該文が変更した自テナントの行の `id`、`must` は現状でも必ず返る行の
/// `id`。重複なし・件数は影響行数以下も併せて検査する）。
#[derive(Clone, Copy, Debug)]
enum Ids {
    Exact(&'static [u64]),
    Between {
        must: &'static [u64],
        within: &'static [u64],
    },
}

/// テスト側の真実値。`Affected(priv, pub)` は
/// （Private 込みモード, Public のみモード）での期待影響行数。
#[derive(Clone, Copy)]
enum Expect {
    Affected(u64, u64),
    /// Private 込みモードは完全一致、Public のみモードは [`Count`] で検査する影響行数。
    AffectedPub(u64, Count),
    Err(&'static str),
    /// RETURNING 付き。`affected`・`ids` は（Private 込み, Public のみ）の 2 モード分。
    /// Private 込みモードは SQL-21 と一致するため完全一致で検査する。Public のみモードの
    /// 返却行は現状、閲覧側に可視な行に限られ（自テナントの Private 行・既定可視性で
    /// 投入した行は返らない）、SQL-21（当該文が変更した行そのものを返す）と一致しない
    /// 形状がある。該当形状は完全一致をやめて [`Ids::Between`] で検査し、完全一致の
    /// 検査は #1256（#1250 の是正後）で戻す。
    Returning {
        affected: (u64, Count),
        ids: (&'static [u64], Ids),
    },
    /// TRUNCATE。件数を露出しない成功応答（`affected` なし）で、事後に閲覧側の `docs`
    /// 行が可視性を問わず 0 件になる（他テナントの行は T3 が不変を検査する）。
    Truncated,
}

struct Shape {
    name: &'static str,
    axis: Axis,
    /// `{op}` を operation_id に、`{v}` を閲覧テナント名に置換する SQL。
    sql: &'static str,
    /// `Some(data)` なら COPY FROM STDIN（`sql` は COPY 文）。
    copy_data: Option<&'static str>,
    expect: Expect,
}

const fn w(name: &'static str, sql: &'static str, expect: Expect) -> Shape {
    Shape {
        name,
        axis: Axis::Write,
        sql,
        copy_data: None,
        expect,
    }
}

const fn c(name: &'static str, sql: &'static str, expect: Expect) -> Shape {
    Shape {
        name,
        axis: Axis::Constraint,
        sql,
        copy_data: None,
        expect,
    }
}

fn shapes() -> Vec<Shape> {
    use Expect::*;
    let mut v = vec![
        // ---- (a) 書き込み経路 ----
        w("upd-id-own-public", "UPDATE docs SET score = 999 WHERE id = 1 USING OPERATION_ID '{op}'", Affected(1, 1)),
        w("upd-id-own-private", "UPDATE docs SET score = 999 WHERE id = 2 USING OPERATION_ID '{op}'", Affected(1, 0)),
        w("upd-id-foreign-only", "UPDATE docs SET score = 999 WHERE id = 9 USING OPERATION_ID '{op}'", Affected(0, 0)),
        w("upd-id-missing", "UPDATE docs SET score = 999 WHERE id = 99 USING OPERATION_ID '{op}'", Affected(0, 0)),
        // 述語形の Public のみモードの影響行数は SQL-19・RLS-10 (a) と不一致の現状
        // （自テナントの Private 行も対象になる）のため、仕様値〜現状値の範囲で検査する。
        w("upd-pred-ja", "UPDATE docs SET score = 0 WHERE lang = 'ja' USING OPERATION_ID '{op}'", AffectedPub(2, Count::Between(1, 2))),
        w("upd-pred-none", "UPDATE docs SET score = 0 WHERE lang = 'zz' USING OPERATION_ID '{op}'", Affected(0, 0)),
        // Public のみモードの返却行は SQL-21 と不一致の現状のため、Public のみモードで
        // 可視の変更行（id 1）⊆ 返却 ⊆ 変更行で検査する。完全一致の検査は #1256
        // （#1250 の是正後）で戻す。
        // 述語形の Public のみモードの影響行数も SQL-19・RLS-10 (a) と不一致の現状
        // （自テナントの Private 行も対象になる）のため、仕様値〜現状値の範囲で検査する。
        w("upd-pred-returning", "UPDATE docs SET score = 5 WHERE lang = 'ja' RETURNING * USING OPERATION_ID '{op}'", Returning { affected: (2, Count::Between(1, 2)), ids: (&[1, 2], Ids::Between { must: &[1], within: &[1, 2] }) }),
        w("del-id-own", "DELETE FROM docs WHERE id = 1 USING OPERATION_ID '{op}'", Affected(1, 1)),
        w("del-id-foreign-only", "DELETE FROM docs WHERE id = 9 USING OPERATION_ID '{op}'", Affected(0, 0)),
        // 述語形の Public のみモードの影響行数は SQL-19・RLS-10 (a) と不一致の現状
        // （自テナントの Private 行も対象になる）のため、仕様値〜現状値の範囲で検査する。
        w("del-pred-ja", "DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID '{op}'", AffectedPub(2, Count::Between(1, 2))),
        // 述語形の Public のみモードの影響行数は SQL-19・RLS-10 (a) と不一致の現状
        // （自テナントの Private 行も対象になる）のため、仕様値〜現状値の範囲で検査する。
        w("del-pred-en", "DELETE FROM docs WHERE lang = 'en' USING OPERATION_ID '{op}'", AffectedPub(2, Count::Between(1, 2))),
        w("del-id-returning", "DELETE FROM docs WHERE id = 1 RETURNING * USING OPERATION_ID '{op}'", Returning { affected: (1, Count::Exact(1)), ids: (&[1], Ids::Exact(&[1])) }),
        w("ins-foreign-id", "INSERT INTO docs (id, lang, score, body) VALUES (9, 'ja', 1, 'tok-{v}-new') USING OPERATION_ID '{op}'", Affected(1, 1)),
        w("ins-multi-foreign-ids", "INSERT INTO docs (id, lang, score, body) VALUES (9, 'ja', 1, 'tok-{v}-n1'), (10, 'ja', 2, 'tok-{v}-n2') USING OPERATION_ID '{op}'", Affected(2, 2)),
        w("ins-own-dup-id", "INSERT INTO docs (id, lang, score, body) VALUES (1, 'ja', 1, 'tok-{v}-dup') USING OPERATION_ID '{op}'", Err("23505")),
        // Public のみモードの返却行は SQL-21 と不一致の現状のため部分集合で検査する
        // （現状の返却は 0 行のため下限 `must` は付けられない）。完全一致の検査は
        // #1256（#1250 の是正後）で戻す。
        w("ins-returning", "INSERT INTO docs (id, lang, score, body) VALUES (9, 'ja', 1, 'tok-{v}-new') RETURNING * USING OPERATION_ID '{op}'", Returning { affected: (1, Count::Exact(1)), ids: (&[9], Ids::Between { must: &[], within: &[9] }) }),
        w("upsert-foreign-id-update", "INSERT INTO docs (id, lang, score, body) VALUES (9, 'ja', 1, 'tok-{v}-up') ON CONFLICT (id) DO UPDATE SET score = EXCLUDED.score USING OPERATION_ID '{op}'", Affected(1, 1)),
        w("upsert-own-id-update", "INSERT INTO docs (id, lang, score, body) VALUES (1, 'ja', 7, 'tok-{v}-up') ON CONFLICT (id) DO UPDATE SET score = EXCLUDED.score USING OPERATION_ID '{op}'", Affected(1, 1)),
        w("upsert-foreign-id-nothing", "INSERT INTO docs (id, lang, score, body) VALUES (9, 'ja', 1, 'tok-{v}-up') ON CONFLICT (id) DO NOTHING USING OPERATION_ID '{op}'", Affected(1, 1)),
        w("upsert-own-id-nothing", "INSERT INTO docs (id, lang, score, body) VALUES (1, 'ja', 1, 'tok-{v}-up') ON CONFLICT (id) DO NOTHING USING OPERATION_ID '{op}'", Affected(0, 0)),
        // Public のみモードの返却行は SQL-21 と不一致の現状のため部分集合で検査する
        // （現状の返却は 0 行のため下限 `must` は付けられない）。完全一致の検査は
        // #1256（#1250 の是正後）で戻す。
        w("upsert-returning", "INSERT INTO docs (id, lang, score, body) VALUES (9, 'ja', 1, 'tok-{v}-up') ON CONFLICT (id) DO UPDATE SET score = EXCLUDED.score RETURNING * USING OPERATION_ID '{op}'", Returning { affected: (1, Count::Exact(1)), ids: (&[9], Ids::Between { must: &[], within: &[9] }) }),
        w("truncate", "TRUNCATE TABLE docs USING OPERATION_ID '{op}'", Truncated),
        // ---- (c) 制約検査 ----
        c("uniq-foreign-code", "INSERT INTO uniq (id, code, a, b) VALUES (10, 'shared', 'n1', 'n2') USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("uniq-foreign-private-code", "INSERT INTO uniq (id, code, a, b) VALUES (10, 'fpriv', 'n1', 'n2') USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("uniq-foreign-composite", "INSERT INTO uniq (id, code, a, b) VALUES (10, 'n1', 'p', 'q') USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("uniq-foreign-id", "INSERT INTO uniq (id, code, a, b) VALUES (3, 'n1', 'n1', 'n2') USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("uniq-own-code", "INSERT INTO uniq (id, code, a, b) VALUES (10, 'u1', 'n1', 'n2') USING OPERATION_ID '{op}'", Err("23505")),
        c("uniq-own-composite", "INSERT INTO uniq (id, code, a, b) VALUES (10, 'n1', 'x1', 'y1') USING OPERATION_ID '{op}'", Err("23505")),
        c("uniq-update-foreign-code", "UPDATE uniq SET code = 'shared' WHERE id = 1 USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("uniq-update-own-code", "UPDATE uniq SET code = 'u2' WHERE id = 1 USING OPERATION_ID '{op}'", Err("23505")),
        c("pk-foreign-key", "INSERT INTO pk (id, code, n) VALUES (10, 'kf', 1) USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("pk-own-key", "INSERT INTO pk (id, code, n) VALUES (10, 'k1', 1) USING OPERATION_ID '{op}'", Err("23505")),
        c("fk-child-foreign-only-parent", "INSERT INTO children (id, parent_id, note) VALUES (30, 7, 'tok-{v}-c') USING OPERATION_ID '{op}'", Err("23503")),
        c("fk-child-missing-parent", "INSERT INTO children (id, parent_id, note) VALUES (30, 99, 'tok-{v}-c') USING OPERATION_ID '{op}'", Err("23503")),
        c("fk-child-own-parent", "INSERT INTO children (id, parent_id, note) VALUES (30, 2, 'tok-{v}-c') USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("fk-child-update-foreign-only-parent", "UPDATE children SET parent_id = 7 WHERE id = 10 USING OPERATION_ID '{op}'", Err("23503")),
        c("fk-parent-delete-foreign-child-only", "DELETE FROM parents WHERE id = 2 USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("fk-parent-delete-own-child", "DELETE FROM parents WHERE id = 1 USING OPERATION_ID '{op}'", Err("23503")),
        c("fk-cascade-delete", "DELETE FROM parents_c WHERE id = 1 USING OPERATION_ID '{op}'", Affected(1, 1)),
        c("fk-setnull-delete", "DELETE FROM parents_s WHERE id = 1 USING OPERATION_ID '{op}'", Affected(1, 1)),
    ];
    v.push(Shape {
        name: "copy-foreign-ids",
        axis: Axis::Write,
        sql: "COPY docs (id, lang, score, body) FROM STDIN USING OPERATION_ID '{op}'",
        copy_data: Some("9\tja\t1\ttok-{v}-c9\n10\tja\t2\ttok-{v}-c10\n"),
        expect: Affected(2, 2),
    });
    v
}

// ---------- 実行・レポート ----------

struct Run {
    viewer: &'static str,
    allow_private: bool,
    shape: &'static str,
    axis: Axis,
    expect: Expect,
    base: Obs,
    flood: Obs,
    /// 実行前の閲覧側（自テナント）の物理状態（baseline／flooded の各テンプレート）。
    /// エラー応答時の副作用ゼロ（T1）の比較基準。
    base_own_before: Snapshot,
    flood_own_before: Snapshot,
    base_own_after: Snapshot,
    flood_own_after: Snapshot,
    foreign_before: Snapshot,
    foreign_after: Snapshot,
    /// 閲覧側の全可視行の読み戻し（`SELECT body FROM docs`）に含まれる他テナントの
    /// トークン件数（オラクル側の文字列照合）。
    readback_leaks: usize,
}

fn run_one(
    template: &Path,
    viewer: &str,
    allow_private: bool,
    idx: usize,
    shape: &Shape,
) -> (Obs, Snapshot, usize) {
    let path = unique_db_path("rls10-write-run");
    let _guard = CleanupGuard(path.clone());
    std::fs::copy(template, &path).expect("copy template");
    let ctx = ctx_for(viewer, allow_private);
    let op = format!("w{idx}");
    let sql = shape.sql.replace("{op}", &op).replace("{v}", viewer);
    let obs;
    let mut leaks = 0;
    {
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open run db"),
            Box::new(CpuScalarProvider),
        );
        obs = match shape.copy_data {
            Some(data) => run_copy(&core, &ctx, &sql, &data.replace("{v}", viewer)),
            None => {
                let mut session = SessionState::default();
                observe(core.execute_sql_in_session(&ctx, &mut session, &sql))
            }
        };
        // 読み戻し: 他テナントの Public 行は読み取りで可視（RLS-7）だが、Private 行
        // （docs の偶数 id。テスト側の真実値）は Private 込みモードでも絶対に見えない。
        let all = ctx_for(viewer, true);
        let r = core
            .execute_sql(&all, "SELECT body FROM docs LIMIT 1000")
            .expect("readback");
        let dump = format!("{r:?}");
        for t in TENANTS.iter().filter(|t| **t != viewer) {
            for id in [2, 4] {
                leaks += dump.matches(&format!("tok-{t}-docs-{id}\"")).count();
            }
        }
    }
    let snap = physical_snapshot(&path);
    leaks += match &obs {
        Obs::Ok { debug, .. } => leaked_tokens(debug, viewer),
        Obs::Err { message, .. } => leaked_tokens(message, viewer),
    };
    (obs, snap, leaks)
}

/// `dump` に viewer 以外のテナント名（トークンの一部。他テナント由来の値）が現れる件数。
fn leaked_tokens(dump: &str, viewer: &str) -> usize {
    TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .map(|t| dump.matches(*t).count())
        .sum()
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
            let flood_pre = physical_snapshot(&flood_tpl);
            let foreign_before = split_by_tenant(&flood_pre, viewer, false);
            let base_own_before = split_by_tenant(&physical_snapshot(&base_tpl), viewer, true);
            let flood_own_before = split_by_tenant(&flood_pre, viewer, true);
            for allow_private in [true, false] {
                for (idx, shape) in shapes.iter().enumerate() {
                    let (base, base_snap, base_leaks) =
                        run_one(&base_tpl, viewer, allow_private, idx, shape);
                    let (flood, flood_snap, flood_leaks) =
                        run_one(&flood_tpl, viewer, allow_private, idx, shape);
                    runs.push(Run {
                        viewer,
                        allow_private,
                        shape: shape.name,
                        axis: shape.axis,
                        expect: shape.expect,
                        base,
                        flood,
                        base_own_before: base_own_before.clone(),
                        flood_own_before: flood_own_before.clone(),
                        base_own_after: split_by_tenant(&base_snap, viewer, true),
                        flood_own_after: split_by_tenant(&flood_snap, viewer, true),
                        foreign_before: foreign_before.clone(),
                        foreign_after: split_by_tenant(&flood_snap, viewer, false),
                        readback_leaks: base_leaks + flood_leaks,
                    });
                }
            }
        }
        runs
    })
}

fn label(r: &Run) -> String {
    format!(
        "viewer={} allow_private={} shape={}",
        r.viewer, r.allow_private, r.shape
    )
}

// ---------- テスト ----------

/// T0 受理ゲート: 各（閲覧側, モード, 軸）で陽性対照が実際に発生している。
#[test]
fn t0_positive_controls_are_non_vacuous() {
    for viewer in TENANTS {
        for allow_private in [true, false] {
            for axis in [Axis::Write, Axis::Constraint] {
                let scoped: Vec<&Run> = report()
                    .iter()
                    .filter(|r| {
                        r.viewer == viewer && r.allow_private == allow_private && r.axis == axis
                    })
                    .collect();
                assert!(!scoped.is_empty());
                let hits = scoped
                    .iter()
                    .filter(|r| matches!(&r.flood, Obs::Ok { affected: Some(n), .. } if *n > 0))
                    .count();
                assert!(
                    hits > 0,
                    "no successful write with affected>0: viewer={viewer} allow_private={allow_private} axis={axis:?}"
                );
                if axis == Axis::Constraint {
                    let violations = scoped
                        .iter()
                        .filter(|r| matches!(&r.flood, Obs::Err { code, .. } if code == "23505" || code == "23503"))
                        .count();
                    assert!(
                        violations >= 2,
                        "own-tenant constraint violations must be observed: viewer={viewer} allow_private={allow_private}"
                    );
                }
            }
        }
    }
}

/// T1 独立オラクル: テスト側の真実値（期待影響行数・期待 wire_code）との照合と、
/// 他テナントのトークンの非混入。
#[test]
fn t1_independent_oracle_and_no_foreign_tokens() {
    for r in report() {
        let mode = r.allow_private;
        for (which, obs) in [("baseline", &r.base), ("flooded", &r.flood)] {
            match (r.expect, obs) {
                (Expect::Affected(p, q), Obs::Ok { affected, .. }) => {
                    let want = if mode { p } else { q };
                    assert_eq!(*affected, Some(want), "{which}: {}", label(r));
                }
                (Expect::AffectedPub(p, q), Obs::Ok { affected, .. }) => {
                    let n = affected
                        .unwrap_or_else(|| panic!("{which}: missing affected: {}", label(r)));
                    if mode {
                        assert_eq!(n, p, "{which}: {}", label(r));
                    } else {
                        assert!(
                            q.admits(n),
                            "{which}: affected {n} not in {q:?}: {}",
                            label(r)
                        );
                    }
                }
                (Expect::Affected(..) | Expect::AffectedPub(..), Obs::Err { code, .. }) => {
                    panic!("{which}: unexpected error {code}: {}", label(r))
                }
                (Expect::Err(code), Obs::Err { code: got, .. }) => {
                    assert_eq!(got, code, "{which}: {}", label(r));
                }
                (Expect::Err(code), Obs::Ok { .. }) => {
                    panic!("{which}: expected error {code} but succeeded: {}", label(r))
                }
                (
                    Expect::Returning {
                        affected: (p, q),
                        ids: (ip, iq),
                    },
                    Obs::Ok {
                        affected,
                        returned_ids,
                        ..
                    },
                ) => {
                    let n = affected
                        .unwrap_or_else(|| panic!("{which}: missing affected: {}", label(r)));
                    let got = returned_ids
                        .as_deref()
                        .unwrap_or_else(|| panic!("{which}: RETURNING rows missing: {}", label(r)));
                    let (count, ids) = if mode {
                        (Count::Exact(p), Ids::Exact(ip))
                    } else {
                        (q, iq)
                    };
                    assert!(
                        count.admits(n),
                        "{which}: affected {n} not in {count:?}: {}",
                        label(r)
                    );
                    match ids {
                        Ids::Exact(want) => {
                            assert_eq!(got, want, "{which}: RETURNING rows: {}", label(r));
                        }
                        Ids::Between {
                            must,
                            within: changed,
                        } => {
                            // 返却行は必須行を含み、当該文が変更した自テナントの行の部分集合
                            // （重複なし・件数は影響行数以下）。他テナントの行はトークン検査
                            // （`readback_leaks`）と T3 が別途検出する。
                            assert!(
                                must.iter().all(|id| got.contains(id)),
                                "{which}: RETURNING rows {got:?} miss required {must:?}: {}",
                                label(r)
                            );
                            assert!(
                                got.iter().all(|id| changed.contains(id)),
                                "{which}: RETURNING rows {got:?} not within {changed:?}: {}",
                                label(r)
                            );
                            let mut uniq = got.to_vec();
                            uniq.sort();
                            uniq.dedup();
                            assert_eq!(
                                uniq.len(),
                                got.len(),
                                "{which}: duplicate rows: {}",
                                label(r)
                            );
                            assert!(
                                got.len() as u64 <= n,
                                "{which}: more RETURNING rows than affected: {}",
                                label(r)
                            );
                        }
                    }
                }
                (Expect::Returning { .. }, Obs::Err { code, .. }) => {
                    panic!("{which}: unexpected error {code}: {}", label(r))
                }
                (Expect::Truncated, Obs::Ok { affected, .. }) => {
                    assert_eq!(*affected, None, "{which}: {}", label(r));
                }
                (Expect::Truncated, Obs::Err { code, .. }) => {
                    panic!("{which}: unexpected error {code}: {}", label(r))
                }
            }
        }
        if matches!(r.expect, Expect::Truncated) {
            for (which, own) in [
                ("baseline", &r.base_own_after),
                ("flooded", &r.flood_own_after),
            ] {
                let remaining = own.keys().filter(|(t, _, _)| t == "docs").count();
                assert_eq!(
                    remaining,
                    0,
                    "{which}: own docs rows remain after TRUNCATE: {}",
                    label(r)
                );
            }
        }
        // エラー応答（制約違反・構文エラー等）は副作用ゼロ: 自テナントの物理状態が
        // 実行前と一致する（baseline・flooded の両方。他テナントは T3 が検査する）。
        for (which, obs, before, after) in [
            ("baseline", &r.base, &r.base_own_before, &r.base_own_after),
            ("flooded", &r.flood, &r.flood_own_before, &r.flood_own_after),
        ] {
            if matches!(obs, Obs::Err { .. }) {
                check_unchanged(before, after).unwrap_or_else(|e| {
                    panic!(
                        "{which}: own state changed by a failed statement ({e}): {}",
                        label(r)
                    )
                });
            }
        }
        assert_eq!(r.readback_leaks, 0, "foreign token leaked: {}", label(r));
    }
}

/// T2 応答の不変性: baseline と flooded で応答・自テナントの事後物理状態が完全一致。
#[test]
fn t2_responses_and_own_state_are_invariant_under_flood() {
    for r in report() {
        assert_eq!(
            r.base,
            r.flood,
            "response changed under flood: {}",
            label(r)
        );
        assert_eq!(
            r.base_own_after,
            r.flood_own_after,
            "viewer's own physical state changed under flood: {}",
            label(r)
        );
    }
}

/// T3 他テナントの物理不変: flooded の他 2 テナントの行が全テーブルで不変。
#[test]
fn t3_foreign_tenants_rows_are_physically_untouched() {
    for r in report() {
        assert!(
            !r.foreign_before.is_empty(),
            "foreign snapshot must be non-empty: {}",
            label(r)
        );
        check_unchanged(&r.foreign_before, &r.foreign_after)
            .unwrap_or_else(|e| panic!("{e}: {}", label(r)));
    }
}

/// 連続する ASCII 数字を `<n>` へ置き換える（参照値だけが異なる FK 違反メッセージを
/// 文言単位で比較するための正規化）。
fn normalize_digits(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut in_digits = false;
    for ch in message.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push_str("<n>");
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(ch);
        }
    }
    out
}

/// T4 FK／UNIQUE の存在情報秘匿: 他テナントにだけ親・値がある場合の応答が、どこにも
/// 無い場合（baseline）と一致し、かつ「親がどこにも無い」場合と同一の応答になる。
#[test]
fn t4_constraint_errors_do_not_reveal_foreign_existence() {
    let mut compared = 0;
    for viewer in TENANTS {
        for allow_private in [true, false] {
            let pick = |name: &str| {
                report()
                    .iter()
                    .find(|r| {
                        r.viewer == viewer && r.allow_private == allow_private && r.shape == name
                    })
                    .expect("shape present")
            };
            let foreign_parent = pick("fk-child-foreign-only-parent");
            let missing_parent = pick("fk-child-missing-parent");
            // 親が他テナントにしか無い（flooded）応答 == 親がどこにも無い（baseline）応答。
            assert_eq!(
                foreign_parent.flood,
                foreign_parent.base,
                "{}",
                label(foreign_parent)
            );
            match (&foreign_parent.flood, &missing_parent.flood) {
                (
                    Obs::Err {
                        code: c1,
                        message: m1,
                    },
                    Obs::Err {
                        code: c2,
                        message: m2,
                    },
                ) => {
                    assert_eq!(c1, "23503");
                    assert_eq!(c1, c2);
                    // メッセージが参照値そのものを埋め込む場合は値部分（7 と 99）だけが
                    // 異なりうる。数値を正規化した文言が一致すること（他テナントに親が
                    // ある場合だけ文言が変わらないこと）と、他テナント固有の情報
                    // （テナント名・トークン）を含まないことを保証する。
                    assert_eq!(
                        normalize_digits(m1),
                        normalize_digits(m2),
                        "{}",
                        label(foreign_parent)
                    );
                    assert_eq!(leaked_tokens(m1, viewer), 0);
                    assert_eq!(leaked_tokens(m2, viewer), 0);
                }
                other => panic!("expected FK violations, got {other:?}"),
            }
            for name in [
                "uniq-foreign-code",
                "uniq-foreign-private-code",
                "uniq-foreign-composite",
                "pk-foreign-key",
            ] {
                let r = pick(name);
                assert_eq!(r.flood, r.base, "{}", label(r));
                assert!(
                    matches!(
                        &r.flood,
                        Obs::Ok {
                            affected: Some(1),
                            ..
                        }
                    ),
                    "{}",
                    label(r)
                );
            }
            compared += 6;
        }
    }
    assert_eq!(compared, 36);
}

/// T5 3 テナント回転と試行総数の固定（空回りしていないこと）。
#[test]
fn t5_rotation_covers_every_viewer_mode_and_shape() {
    // 形状の宣言とは独立に固定した形状名の集合（形状の削除・改名を検出する）。
    const EXPECTED_SHAPES: [&str; 41] = [
        "upd-id-own-public",
        "upd-id-own-private",
        "upd-id-foreign-only",
        "upd-id-missing",
        "upd-pred-ja",
        "upd-pred-none",
        "upd-pred-returning",
        "del-id-own",
        "del-id-foreign-only",
        "del-pred-ja",
        "del-pred-en",
        "del-id-returning",
        "ins-foreign-id",
        "ins-multi-foreign-ids",
        "ins-own-dup-id",
        "ins-returning",
        "upsert-foreign-id-update",
        "upsert-own-id-update",
        "upsert-foreign-id-nothing",
        "upsert-own-id-nothing",
        "upsert-returning",
        "truncate",
        "copy-foreign-ids",
        "uniq-foreign-code",
        "uniq-foreign-private-code",
        "uniq-foreign-composite",
        "uniq-foreign-id",
        "uniq-own-code",
        "uniq-own-composite",
        "uniq-update-foreign-code",
        "uniq-update-own-code",
        "pk-foreign-key",
        "pk-own-key",
        "fk-child-foreign-only-parent",
        "fk-child-missing-parent",
        "fk-child-own-parent",
        "fk-child-update-foreign-only-parent",
        "fk-parent-delete-foreign-child-only",
        "fk-parent-delete-own-child",
        "fk-cascade-delete",
        "fk-setnull-delete",
    ];
    let mut want: Vec<&str> = EXPECTED_SHAPES.to_vec();
    want.sort();
    let mut declared: Vec<&str> = shapes().iter().map(|s| s.name).collect();
    declared.sort();
    assert_eq!(declared, want, "declared shape set changed");
    assert_eq!(report().len(), TENANTS.len() * 2 * EXPECTED_SHAPES.len());
    for viewer in TENANTS {
        for allow_private in [true, false] {
            let mut ran: Vec<&str> = report()
                .iter()
                .filter(|r| r.viewer == viewer && r.allow_private == allow_private)
                .map(|r| r.shape)
                .collect();
            ran.sort();
            assert_eq!(ran, want, "viewer={viewer} allow_private={allow_private}");
        }
    }
    // 経路の網羅（書き込み系・制約系の両軸が形状に含まれる）。
    assert_eq!(
        shapes().iter().filter(|s| s.axis == Axis::Write).count(),
        23
    );
    assert_eq!(
        shapes()
            .iter()
            .filter(|s| s.axis == Axis::Constraint)
            .count(),
        18
    );
}

/// T6 負の対照: 検査器が捏造した違反を実際に検出する。
#[test]
fn t6_negative_controls_detect_fabricated_violations() {
    // 他テナント行が 1 バイト書き換わったスナップショットを検出する。
    let r = report()
        .iter()
        .find(|r| !r.foreign_before.is_empty())
        .expect("run");
    let mut tampered = r.foreign_before.clone();
    let key = tampered.keys().next().expect("key").clone();
    if let Some(v) = tampered.get_mut(&key) {
        match v.2.first_mut() {
            Some(b) => *b ^= 0x01,
            None => v.2.push(1),
        }
    }
    assert!(check_unchanged(&r.foreign_before, &tampered).is_err());
    // 他テナント行の削除・追加も検出する。
    let mut removed = r.foreign_before.clone();
    removed.remove(&key);
    assert!(check_unchanged(&r.foreign_before, &removed).is_err());
    // フィンガープリントが内容差分を区別する。
    assert_ne!(fingerprint(&r.foreign_before), fingerprint(&tampered));
    // RETURNING に他テナントのトークンが混入した場合を検出する。
    assert_eq!(leaked_tokens("tok-tenant-a-docs-1", TENANT_A), 0);
    assert_eq!(leaked_tokens("tok-tenant-b-docs-1", TENANT_A), 1);
    assert_eq!(leaked_tokens("tok-tenant-b-x tok-tenant-c-y", TENANT_A), 2);
    // 応答不変性の比較が差分を検出する。
    let a = Obs::Ok {
        affected: Some(1),
        returned_ids: None,
        debug: "x".into(),
    };
    let b = Obs::Ok {
        affected: Some(2),
        returned_ids: None,
        debug: "x".into(),
    };
    assert_ne!(a, b);
    // RETURNING の返却行（id 集合）の差分も検出する。
    let c = Obs::Ok {
        affected: Some(1),
        returned_ids: Some(vec![1]),
        debug: "x".into(),
    };
    let d = Obs::Ok {
        affected: Some(1),
        returned_ids: Some(vec![9]),
        debug: "x".into(),
    };
    assert_ne!(c, d);
    // 範囲検査は範囲外（仕様値・現状値のいずれでもない値）を検出する。
    assert!(Count::Between(1, 2).admits(1) && Count::Between(1, 2).admits(2));
    assert!(!Count::Between(1, 2).admits(0) && !Count::Between(1, 2).admits(3));
    assert!(!Count::Exact(1).admits(2));
    // FK 違反メッセージの正規化は数値だけを吸収し、文言の差は検出する。
    assert_eq!(
        normalize_digits("key (7) missing"),
        normalize_digits("key (99) missing")
    );
    assert_ne!(
        normalize_digits("key (7) missing"),
        normalize_digits("key (7) hidden")
    );
}
