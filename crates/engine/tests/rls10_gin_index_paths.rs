//! GIN 相当索引の共有物理テーブルを経由したテナント越境を、機械的なマトリクスで固定する回帰テスト
//! （Issue #2209。ポインタ: INDEX-8、関連 TABLE-12・RLS-9・RLS-10・SQL-27。設計は
//! `docs/design/gin-jsonb-index.md` D10・§9.6）。
//!
//! 役割: GIN 相当索引は全テナントのポスティングを 1 本の物理テーブル `user_gin/{索引名}`（キーは
//! `(tenant_id, subkey)`）に持つ。`sql_gin_index_dml.rs`（DML 経路ごとの監査）と
//! `sql_scan_gin_index.rs`（演算子ごとの結果一致）が個別に確かめる境界を、本ファイルが
//! 閲覧テナント 3 つ × 可視性 2 モード × 実行経路 2 つ（autocommit／明示トランザクション）× 演算子で
//! 統一的に検査する。production の可視性判定（`PolicyContext::is_visible` 等）はオラクル側で呼ばない。
//!
//! - T0 陽性対照: EXPLAIN が `index_gin`／`gin_index_candidates` を示す（索引経路を実際に通る）。
//!   期待集合が空でない組み合わせが十分にある
//! - T1 独立オラクル: 結果が GIN なしの双子表 `p` とバイト単位で一致し、かつテスト側が持つ真実値
//!   （自テナントの行と可視な他テナントの Public 行）と一致する。他テナントの Private 行が現れない
//! - T2 flood 不変性: 他 2 テナントが同じキー・同じ id の行を足しても、閲覧側の応答・EXPLAIN・
//!   事後状態・監査結果が完全に一致する
//! - T3 閲覧テナントの DML（INSERT・UPDATE・DELETE・UPSERT・TRUNCATE）の前後で、他テナントの
//!   事後状態（行と監査結果）が変わらない
//! - T4 マーカー状態の混在（`RENAME COLUMN` の後に 1 テナントだけ書き込む）でも、結果が `p` と一致
//! - T5 他テナントの UPDATE／DELETE の後に、閲覧側へその旧値が現れない（stale ポスティング）
//! - T6 多層防御: 自テナントの範囲へ他テナントの行・存在しない行を指す偽装ポスティングを raw redb
//!   で差し込んでも、結果は `p` と一致する
//! - T7 エラー文面: 共有 DDL（`CREATE INDEX`）の `54000` が他テナントの値に起因しても、文面は
//!   自分の行が原因の場合と同一で、テナント・値・件数を含まない
//! - T8 試行総数の固定（空振り防止）と負の対照（捏造した違反を検査器が検出する）

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, GinIndexAudit, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use redb::{ReadableDatabase, ReadableTable, TableHandle};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const INDEXES: [&str; 2] = ["g_ops", "g_path"];

// ---------- 基本ヘルパー ----------

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
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

fn jsonb_table(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("doc", ColumnType::Jsonb, true)])
}

fn run(
    core: &EngineCore,
    c: &PolicyContext,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(c, s, sql)
}

fn run_ok(core: &EngineCore, c: &PolicyContext, sql: &str) {
    let mut s = ddl_session();
    if let Err(e) = run(core, c, &mut s, sql) {
        panic!("{sql}: {} {e:?}", e.wire_code());
    }
}

/// 実行経路。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Auto,
    Txn,
}

/// `sql` を経路 `path` で実行して結果を返す（明示トランザクションは `BEGIN` → 文 → `COMMIT`）。
fn exec(core: &EngineCore, c: &PolicyContext, path: Path, sql: &str) -> SqlOutcome {
    match path {
        Path::Auto => {
            let mut s = SessionState::default();
            run(core, c, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
        }
        Path::Txn => {
            let mut s = SessionState::default();
            let mut txn = core.new_session_transaction();
            let mut tx = |q: &str| core.execute_sql_in_txn(c, &mut s, &mut txn, q);
            tx("BEGIN").expect("begin");
            let out = tx(sql).unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
            tx("COMMIT").expect("commit");
            out
        }
    }
}

/// 1 行の観測: 行 id と、doc 文字列から取り出した所有テナントトークン。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Seen {
    owner: String,
    id: u64,
}

/// doc 文字列中の `"owner":"<token>"` を取り出す（無ければ空文字）。
fn owner_of(doc: &str) -> String {
    let marker = "\"owner\":\"";
    match doc.find(marker) {
        Some(p) => {
            let rest = &doc[p + marker.len()..];
            rest.split('"').next().unwrap_or("").to_string()
        }
        None => String::new(),
    }
}

/// 応答（バイト単位の比較用の文字列列）と、構造化した観測。
struct Obs {
    raw: Vec<String>,
    seen: Vec<Seen>,
}

fn select(core: &EngineCore, c: &PolicyContext, path: Path, sql: &str) -> Obs {
    match exec(core, c, path, sql) {
        SqlOutcome::Query(q) => {
            let mut raw = Vec::new();
            let mut seen = Vec::new();
            for r in &q.rows {
                raw.push(format!("{:?}", r.cells));
                let id = match r.cells.first() {
                    Some(Cell::Integer(n)) => *n,
                    other => panic!("expected id cell, got {other:?}"),
                };
                let owner = match r.cells.get(1) {
                    Some(Cell::Json(t)) | Some(Cell::Text(t)) => owner_of(t),
                    other => panic!("expected doc cell, got {other:?}"),
                };
                seen.push(Seen { owner, id });
            }
            Obs { raw, seen }
        }
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn explain(core: &EngineCore, c: &PolicyContext, path: Path, sql: &str) -> Vec<String> {
    match exec(core, c, path, &format!("EXPLAIN {sql}")) {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|r| match &r.cells[0] {
                Cell::Text(t) => t.clone(),
                other => panic!("expected text cell, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn assert_gin(lines: &[String], index: &str, sql: &str) {
    assert!(
        lines
            .iter()
            .any(|l| *l == format!("scalar_plan: index_gin index={index}")),
        "{sql}: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "access_path: gin_index_candidates"),
        "{sql}: {lines:?}"
    );
}

fn audit(core: &EngineCore, tenant: &str, index: &str) -> GinIndexAudit {
    core.verify_gin_index_read_only(&ctx_for(tenant, true), index)
        .unwrap_or_else(|e| panic!("audit {index}: {e:?}"))
}

fn audits(core: &EngineCore, tenant: &str) -> Vec<GinIndexAudit> {
    INDEXES.iter().map(|i| audit(core, tenant, i)).collect()
}

fn assert_clean(core: &EngineCore, tenant: &str) {
    for i in INDEXES {
        let a = audit(core, tenant, i);
        assert!(
            a.marker_valid && a.missing == 0 && a.stale == 0,
            "{i}: {a:?}"
        );
    }
}

// ---------- フィクスチャ ----------

/// 所有テナント `o` の id 別の doc。共通のキー・値・経路に所有者トークンを足す。
fn doc_for(owner: &str, id: u64) -> String {
    match id {
        1 => format!(r#"{{"owner":"{owner}","a":1,"tags":["x"],"k":{{"n":"p"}}}}"#),
        2 => format!(r#"{{"owner":"{owner}","a":2,"tags":["y"]}}"#),
        3 => format!(r#"{{"owner":"{owner}","a":1,"k":{{"n":"p"}},"z":"v"}}"#),
        4 => format!(r#"{{"owner":"{owner}","tags":["x","y"]}}"#),
        _ => format!(r#"{{"owner":"{owner}","a":1,"extra":{id}}}"#),
    }
}

/// 他テナントの入れ方。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Others {
    /// 閲覧テナントの行だけ（baseline）。
    None,
    /// 他テナントも同じキー・同じ id の行を Private で持つ（flood）。
    AllPrivate,
    /// 他テナントは id 3 を Public、残りを Private で持つ（実運用に近い形）。
    Realistic,
}

struct Fx {
    core: EngineCore,
    _guard: CleanupGuard,
    path: std::path::PathBuf,
    /// 投入した行の全体（所有者, id, 可視性）。オラクルの真実値の元。
    rows: Vec<(String, u64, Visibility)>,
}

impl Fx {
    /// コアを閉じて（ファイルロックを解放して）パスとガードを返す。
    fn close(self) -> (std::path::PathBuf, CleanupGuard) {
        drop(self.core);
        (self.path, self._guard)
    }
}

/// `g`（`g_ops`・`g_path` を宣言）と `p`（GIN なし）へ同じ行を同じ id で入れる。
/// Public 行は Rust API でしか作れないため、`Storage` 経由の seed を先に行い、索引宣言は後から行う
/// （`CREATE INDEX` の初回構築が全テナント・全可視性の行を対象にする）。他テナントの行は閲覧側の
/// 1 行ごとに挟んで入れる。
fn build(label: &str, viewer: &str, others: Others) -> Fx {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in ["g", "p"] {
        storage.create_table(&jsonb_table(t)).expect("create table");
    }
    let mut rows = Vec::new();
    let mut seq = 0u64;
    let other_tenants: Vec<&str> = TENANTS.iter().copied().filter(|t| *t != viewer).collect();
    {
        // Public 行は `Storage` 直接。SQL の INSERT 用に後で `EngineCore` へ移す。
        for id in 1..=4u64 {
            let own_public = id == 3;
            let vis = |public: bool| {
                if public {
                    Visibility::Public
                } else {
                    Visibility::Private
                }
            };
            insert_via_storage(&storage, viewer, id, own_public, &mut seq);
            rows.push((viewer.to_string(), id, vis(own_public)));
            if others != Others::None {
                for t in &other_tenants {
                    let public = others == Others::Realistic && id == 3;
                    insert_via_storage(&storage, t, id, public, &mut seq);
                    rows.push((t.to_string(), id, vis(public)));
                }
            }
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let owner = ctx_for("owner", true);
    run_ok(
        &core,
        &owner,
        "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
    );
    run_ok(
        &core,
        &owner,
        "CREATE INDEX g_path ON g USING gin (doc jsonb_path_ops)",
    );
    Fx {
        core,
        _guard: guard,
        path,
        rows,
    }
}

/// `Storage` の行 API で `tenant` の行を `g`・`p` へ入れる（Public／Private の両方を作れる）。
fn insert_via_storage(storage: &Storage, tenant: &str, id: u64, public: bool, seq: &mut u64) {
    for table in ["g", "p"] {
        *seq += 1;
        let op = OperationId::parse(&format!("gin-rls-{table}-{seq}")).expect("op id");
        engine::tenant::insert_typed_row(
            storage,
            table,
            &ctx_for(tenant, true),
            id,
            if public {
                Visibility::Public
            } else {
                Visibility::Private
            },
            &[engine::row_codec::Value::Json(
                engine::json::canonicalize_jsonb_text(&doc_for(tenant, id))
                    .expect("canonical jsonb"),
            )],
            &op,
        )
        .unwrap_or_else(|e| {
            panic!(
                "insert row {table} {tenant} {id} {public}: {} {}",
                e.wire_code(),
                e
            )
        });
    }
}

// ---------- 問い合わせ ----------

/// 1 つの問い合わせ形状。`ids` は「どの id の行が一致するか」（全テナント共通の doc 形状）、
/// `only_owner` は所有者トークンでの絞り込み（`@> {"owner": ..}` 形）。
struct Q {
    w: String,
    index: &'static str,
    ids: Vec<u64>,
    only_owner: Option<String>,
}

fn queries(col: &str, viewer: &str, other: &str) -> Vec<Q> {
    let q = |w: String, index, ids: &[u64], only_owner: Option<&str>| Q {
        w,
        index,
        ids: ids.to_vec(),
        only_owner: only_owner.map(str::to_string),
    };
    vec![
        q(format!(r#"{col} @> '{{"a":1}}'"#), "g_path", &[1, 3], None),
        q(format!("{col} ? 'owner'"), "g_ops", &[1, 2, 3, 4], None),
        q(format!("{col} ? 'tags'"), "g_ops", &[1, 2, 4], None),
        q(
            format!("{col} ?| '{{z,tags}}'"),
            "g_ops",
            &[1, 2, 3, 4],
            None,
        ),
        q(format!("{col} ?& '{{a,tags}}'"), "g_ops", &[1, 2], None),
        q(format!(r#"{col} @@ '$.a == 1'"#), "g_path", &[1, 3], None),
        q(
            format!(r#"{col} @? '$.k ? (@.n == "p")'"#),
            "g_path",
            &[1, 3],
            None,
        ),
        q(
            format!(r#"{col} @> '{{"owner":"{viewer}"}}'"#),
            "g_path",
            &[1, 2, 3, 4],
            Some(viewer),
        ),
        q(
            format!(r#"{col} @> '{{"owner":"{other}"}}'"#),
            "g_path",
            &[1, 2, 3, 4],
            Some(other),
        ),
    ]
}

/// テスト側の真実値: 自テナントの行（モードで見える可視性）と他テナントの Public 行のうち、
/// 問い合わせ形状に一致するもの。
fn expected(
    rows: &[(String, u64, Visibility)],
    viewer: &str,
    private: bool,
    q: &Q,
) -> BTreeSet<Seen> {
    rows.iter()
        .filter(|(owner, id, vis)| {
            let visible = if owner == viewer {
                *vis == Visibility::Public || private
            } else {
                *vis == Visibility::Public
            };
            visible && q.ids.contains(id) && q.only_owner.as_ref().is_none_or(|o| o == owner)
        })
        .map(|(owner, id, _)| Seen {
            owner: owner.clone(),
            id: *id,
        })
        .collect()
}

/// 他テナント由来の行が `viewer` に見えている（かつ Public でない）違反を数える検査器。
fn foreign_private_rows(seen: &[Seen], rows: &[(String, u64, Visibility)], viewer: &str) -> usize {
    seen.iter()
        .filter(|s| {
            s.owner != viewer
                && !rows
                    .iter()
                    .any(|(o, i, v)| *o == s.owner && *i == s.id && *v == Visibility::Public)
        })
        .count()
}

fn other_of<'a>(viewer: &str, skip: usize) -> &'a str {
    TENANTS
        .iter()
        .copied()
        .filter(|t| *t != viewer)
        .nth(skip)
        .expect("other tenant")
}

// ---------- T0・T1・T8: 越境マトリクス ----------

#[test]
fn matrix_matches_the_twin_table_and_the_independent_oracle() {
    let mut combos = 0usize;
    let mut non_empty = 0usize;
    for viewer in TENANTS {
        let fx = build("gin-rls-matrix", viewer, Others::Realistic);
        for private in [true, false] {
            let c = ctx_for(viewer, private);
            for path in [Path::Auto, Path::Txn] {
                for q in queries("doc", viewer, other_of(viewer, 0)) {
                    let sql_g = format!("SELECT id, doc FROM g WHERE {} LIMIT 100", q.w);
                    let sql_p = format!("SELECT id, doc FROM p WHERE {} LIMIT 100", q.w);
                    // T0: 索引経路を実際に通っている。
                    assert_gin(&explain(&fx.core, &c, path, &sql_g), q.index, &sql_g);
                    // T1: 双子表と一致。
                    let on_g = select(&fx.core, &c, path, &sql_g);
                    let on_p = select(&fx.core, &c, path, &sql_p);
                    assert_eq!(on_g.raw, on_p.raw, "{viewer} {path:?} {}", q.w);
                    // T1: 独立オラクルと一致し、他テナントの Private 行が現れない。
                    let want = expected(&fx.rows, viewer, private, &q);
                    let got: BTreeSet<Seen> = on_g.seen.iter().cloned().collect();
                    assert_eq!(got, want, "{viewer} private={private} {path:?} {}", q.w);
                    assert_eq!(on_g.seen.len(), got.len(), "no duplicate rows: {}", q.w);
                    assert_eq!(foreign_private_rows(&on_g.seen, &fx.rows, viewer), 0);
                    combos += 1;
                    if !want.is_empty() {
                        non_empty += 1;
                    }
                }
            }
        }
    }
    // T8: 試行総数の固定（3 閲覧側 × 2 モード × 2 経路 × 9 形状）と、空振りでない組み合わせの下限。
    assert_eq!(combos, 3 * 2 * 2 * 9);
    assert!(
        non_empty >= 90,
        "too many vacuous combinations: {non_empty}"
    );
}

// ---------- T2: flood 不変性 ----------

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    responses: Vec<(Vec<String>, Vec<String>)>,
    own_rows: Vec<String>,
    audits: Vec<GinIndexAudit>,
}

fn snapshot(fx: &Fx, viewer: &str) -> Snapshot {
    let mut responses = Vec::new();
    for private in [true, false] {
        let c = ctx_for(viewer, private);
        for path in [Path::Auto, Path::Txn] {
            for q in queries("doc", viewer, other_of(viewer, 0)) {
                // 所有者トークンが他テナントの問い合わせは flood の有無で意味が変わらない（空）ため含める。
                let sql = format!("SELECT id, doc FROM g WHERE {} LIMIT 100", q.w);
                // 他テナントの Public 行は flood 側に無い（AllPrivate）ため、可視な自分の行だけを比べる。
                let obs = select(&fx.core, &c, path, &sql);
                let own: Vec<String> = obs
                    .raw
                    .iter()
                    .zip(&obs.seen)
                    .filter(|(_, s)| s.owner == viewer)
                    .map(|(r, _)| r.clone())
                    .collect();
                assert_eq!(own.len(), obs.raw.len(), "only own rows are visible: {sql}");
                responses.push((own, explain(&fx.core, &c, path, &sql)));
            }
        }
    }
    let own_rows = select(
        &fx.core,
        &ctx_for(viewer, true),
        Path::Auto,
        "SELECT id, doc FROM g LIMIT 1000",
    )
    .raw;
    Snapshot {
        responses,
        own_rows,
        audits: audits(&fx.core, viewer),
    }
}

#[test]
fn other_tenants_flood_does_not_change_the_viewers_responses() {
    for viewer in TENANTS {
        let base = build("gin-rls-base", viewer, Others::None);
        let flooded = build("gin-rls-flood", viewer, Others::AllPrivate);
        let a = snapshot(&base, viewer);
        let b = snapshot(&flooded, viewer);
        assert!(a.responses.iter().any(|(rows, _)| !rows.is_empty()));
        assert_eq!(a, b, "flood changed the observations of {viewer}");
    }
}

// ---------- T3: 閲覧側の DML が他テナントの状態を変えない ----------

fn others_state(fx: &Fx, viewer: &str) -> Vec<(String, Vec<String>, Vec<GinIndexAudit>)> {
    TENANTS
        .iter()
        .filter(|t| **t != viewer)
        .map(|t| {
            let obs = select(
                &fx.core,
                &ctx_for(t, true),
                Path::Auto,
                "SELECT id, doc FROM g LIMIT 1000",
            );
            // 他テナントの Public 行は閲覧側の DML で変わりうるため、自分の所有行だけを比べる。
            let own: Vec<String> = obs
                .raw
                .iter()
                .zip(&obs.seen)
                .filter(|(_, s)| s.owner == *t)
                .map(|(r, _)| r.clone())
                .collect();
            (t.to_string(), own, audits(&fx.core, t))
        })
        .collect()
}

#[test]
fn viewers_dml_leaves_other_tenants_state_untouched() {
    for viewer in TENANTS {
        let fx = build("gin-rls-dml", viewer, Others::Realistic);
        let c = ctx_for(viewer, true);
        let before = others_state(&fx, viewer);
        assert!(before.iter().all(|(_, rows, _)| !rows.is_empty()));
        let new_doc = |id: u64| doc_for(viewer, 40 + id);
        let stmts = [
            format!("INSERT INTO g (id, doc) VALUES (50, '{}')", new_doc(10)),
            format!("UPDATE g SET doc = '{}' WHERE id = 1", new_doc(1)),
            "DELETE FROM g WHERE id = 2".to_string(),
            format!(
                "INSERT INTO g (id, doc) VALUES (4, '{}') ON CONFLICT (id) DO UPDATE SET doc = EXCLUDED.doc",
                new_doc(4)
            ),
            format!("UPDATE g SET doc = '{}' WHERE id = 3", new_doc(3)),
            "TRUNCATE g".to_string(),
        ];
        for sql in &stmts {
            run_ok(&fx.core, &c, sql);
            assert_eq!(others_state(&fx, viewer), before, "{viewer}: {sql}");
            assert_clean(&fx.core, viewer);
        }
    }
}

// ---------- T4: マーカー状態の混在 ----------

#[test]
fn mixed_marker_states_never_change_results() {
    for writer in TENANTS {
        let fx = build("gin-rls-marker", writer, Others::Realistic);
        let owner = ctx_for("owner", true);
        for t in ["g", "p"] {
            run_ok(
                &fx.core,
                &owner,
                &format!("ALTER TABLE {t} RENAME COLUMN doc TO payload"),
            );
        }
        // RENAME 直後は全テナントのマーカーが不一致（縮退経路）。
        assert!(TENANTS
            .iter()
            .all(|t| !audit(&fx.core, t, "g_ops").marker_valid));
        // 1 テナントだけが書き込み、そのマーカーを再構築する。
        let wc = ctx_for(writer, true);
        for t in ["g", "p"] {
            run_ok(
                &fx.core,
                &wc,
                &format!(
                    "INSERT INTO {t} (id, payload) VALUES (200, '{}')",
                    doc_for(writer, 200)
                ),
            );
        }
        assert!(audit(&fx.core, writer, "g_ops").marker_valid);
        let invalid = TENANTS
            .iter()
            .filter(|t| !audit(&fx.core, t, "g_ops").marker_valid)
            .count();
        assert_eq!(invalid, 2, "valid / invalid markers must be mixed");
        for viewer in TENANTS {
            for private in [true, false] {
                let c = ctx_for(viewer, private);
                for path in [Path::Auto, Path::Txn] {
                    for q in queries("payload", viewer, other_of(viewer, 0)) {
                        let on_g = select(
                            &fx.core,
                            &c,
                            path,
                            &format!("SELECT id, payload FROM g WHERE {} LIMIT 100", q.w),
                        );
                        let on_p = select(
                            &fx.core,
                            &c,
                            path,
                            &format!("SELECT id, payload FROM p WHERE {} LIMIT 100", q.w),
                        );
                        assert_eq!(on_g.raw, on_p.raw, "{writer}/{viewer} {}", q.w);
                    }
                }
            }
        }
    }
}

// ---------- T5: 他テナントの stale ポスティング ----------

#[test]
fn other_tenants_updates_and_deletes_leave_no_old_values_visible() {
    let fx = build("gin-rls-stale", "tenant-a", Others::Realistic);
    let b = ctx_for("tenant-b", true);
    for t in ["g", "p"] {
        // Public 行（id 3）の値変更と、Private 行（id 1）の削除。
        run_ok(
            &fx.core,
            &b,
            &format!(r#"UPDATE {t} SET doc = '{{"owner":"tenant-b","a":9}}' WHERE id = 3"#),
        );
        run_ok(&fx.core, &b, &format!("DELETE FROM {t} WHERE id = 1"));
    }
    for viewer in ["tenant-a", "tenant-c"] {
        for private in [true, false] {
            let c = ctx_for(viewer, private);
            for path in [Path::Auto, Path::Txn] {
                for q in queries("doc", viewer, "tenant-b") {
                    let on_g = select(
                        &fx.core,
                        &c,
                        path,
                        &format!("SELECT id, doc FROM g WHERE {} LIMIT 100", q.w),
                    );
                    let on_p = select(
                        &fx.core,
                        &c,
                        path,
                        &format!("SELECT id, doc FROM p WHERE {} LIMIT 100", q.w),
                    );
                    assert_eq!(on_g.raw, on_p.raw, "{viewer} {}", q.w);
                }
                let old = select(
                    &fx.core,
                    &c,
                    path,
                    r#"SELECT id, doc FROM g WHERE doc @> '{"a":1}' LIMIT 100"#,
                );
                assert!(
                    !old.seen
                        .iter()
                        .any(|s| s.owner == "tenant-b" && (s.id == 3 || s.id == 1)),
                    "old value of tenant-b leaked: {:?}",
                    old.seen
                );
            }
        }
    }
    assert_clean(&fx.core, "tenant-b");
}

// ---------- T6: 偽装ポスティング（多層防御） ----------

const POSTING_TAG: u8 = 0x01;
type GinDef<'a> = redb::TableDefinition<'a, (&'static str, &'static [u8]), &'static [u8]>;

/// raw redb を開き、`user_gin/` で始まるすべての物理テーブルについて `f` を適用する
/// （索引の内部名を決め打ちしない）。
fn with_gin_tables(
    path: &std::path::Path,
    f: &mut dyn FnMut(&redb::WriteTransaction, &str),
) -> usize {
    let db = redb::Database::open(path).expect("open raw redb");
    let names: Vec<String> = {
        let r = db.begin_read().expect("begin read");
        r.list_tables()
            .expect("list tables")
            .map(|h| h.name().to_string())
            .filter(|n| n.starts_with("user_gin/"))
            .collect()
    };
    let w = db.begin_write().expect("begin write");
    for n in &names {
        f(&w, n);
    }
    w.commit().expect("commit");
    names.len()
}

/// テナント `tenant` のポスティングキー（`[0x01][len][entry][rowid]`）を全て集める。
fn postings_of(
    table: &redb::Table<'_, (&'static str, &'static [u8]), &'static [u8]>,
    tenant: &str,
) -> Vec<Vec<u8>> {
    let lo = [POSTING_TAG];
    let hi = [POSTING_TAG + 1];
    table
        .range((tenant, lo.as_slice())..(tenant, hi.as_slice()))
        .expect("range")
        .map(|e| e.expect("entry").0.value().1.to_vec())
        .collect()
}

#[test]
fn forged_postings_in_the_viewers_range_never_surface_other_tenants_rows() {
    let viewer = "tenant-a";
    let fx = build("gin-rls-forge", viewer, Others::Realistic);
    // tenant-b にだけ存在する Private 行（rowid = id 100）。
    for t in ["g", "p"] {
        run_ok(
            &fx.core,
            &ctx_for("tenant-b", true),
            &format!(
                "INSERT INTO {t} (id, doc) VALUES (100, '{}')",
                doc_for("tenant-b", 100)
            ),
        );
    }
    let (path, _guard) = fx.close();
    let mut forged = 0usize;
    let tables = with_gin_tables(&path, &mut |w, name| {
        let mut t = w.open_table(GinDef::new(name)).expect("open gin table");
        for key in postings_of(&t, viewer) {
            let split = key.len() - 8;
            for rowid in [100u64, 9999u64] {
                let mut k = key[..split].to_vec();
                k.extend_from_slice(&rowid.to_be_bytes());
                t.insert((viewer, k.as_slice()), [].as_slice())
                    .expect("insert");
                forged += 1;
            }
        }
    });
    assert!(tables >= 2 && forged > 0, "forgery must not be vacuous");
    let core = EngineCore::open(&path).expect("reopen");
    for private in [true, false] {
        let c = ctx_for(viewer, private);
        for path_kind in [Path::Auto, Path::Txn] {
            for q in queries("doc", viewer, "tenant-b") {
                let on_g = select(
                    &core,
                    &c,
                    path_kind,
                    &format!("SELECT id, doc FROM g WHERE {} LIMIT 100", q.w),
                );
                let on_p = select(
                    &core,
                    &c,
                    path_kind,
                    &format!("SELECT id, doc FROM p WHERE {} LIMIT 100", q.w),
                );
                assert_eq!(on_g.raw, on_p.raw, "{}", q.w);
                assert!(
                    !on_g
                        .seen
                        .iter()
                        .any(|s| s.owner == "tenant-b" && s.id == 100),
                    "forged posting surfaced a foreign row: {}",
                    q.w
                );
            }
        }
    }
    drop(core);
}

// ---------- T7: エラー文面 ----------

fn big_doc() -> String {
    format!(r#"{{"{}":1}}"#, "k".repeat(4_097))
}

fn create_index_error(writer: &str, creator: &str) -> SqlSurfaceError {
    let path = unique_db_path("gin-rls-leak");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&jsonb_table("g"))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_ok(
        &core,
        &ctx_for(creator, true),
        &format!(
            "INSERT INTO g (id, doc) VALUES (1, '{}')",
            doc_for(creator, 1)
        ),
    );
    run_ok(
        &core,
        &ctx_for(writer, true),
        &format!("INSERT INTO g (id, doc) VALUES (2, '{}')", big_doc()),
    );
    let mut s = ddl_session();
    match run(
        &core,
        &ctx_for(creator, true),
        &mut s,
        "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
    ) {
        Ok(o) => panic!("CREATE INDEX must fail, got {o:?}"),
        Err(e) => e,
    }
}

#[test]
fn error_text_does_not_reveal_other_tenants_data() {
    // 共有 DDL は全テナントを走査するため、他テナントの行が原因でも 54000 になる（D10 が許容）。
    // 文面は自分の行が原因の場合とバイト単位で同一で、テナント・値・件数を含まない。
    let foreign = create_index_error("tenant-b", "tenant-a");
    let own = create_index_error("tenant-a", "tenant-a");
    assert_eq!(foreign.wire_code(), "54000");
    assert_eq!(own.wire_code(), "54000");
    assert_eq!(foreign.to_string(), own.to_string());
    let msg = foreign.to_string();
    assert!(!msg.contains("tenant") && !msg.contains("kkkk"), "{msg}");

    // 書き込み時の 54000 は、他テナントの状態（行の有無）で応答が変わらない。
    let write_err = |with_others: bool| -> String {
        let fx = build(
            "gin-rls-write-err",
            "tenant-a",
            if with_others {
                Others::AllPrivate
            } else {
                Others::None
            },
        );
        let mut s = SessionState::default();
        let e = run(
            &fx.core,
            &ctx_for("tenant-a", true),
            &mut s,
            &format!("INSERT INTO g (id, doc) VALUES (77, '{}')", big_doc()),
        )
        .expect_err("oversized key must be rejected");
        assert_eq!(e.wire_code(), "54000");
        e.to_string()
    };
    assert_eq!(write_err(false), write_err(true));
}

// ---------- T8: 負の対照 ----------

#[test]
fn the_oracle_detects_fabricated_violations() {
    let rows = vec![
        ("tenant-a".to_string(), 1, Visibility::Private),
        ("tenant-b".to_string(), 1, Visibility::Private),
        ("tenant-b".to_string(), 3, Visibility::Public),
    ];
    let clean = vec![
        Seen {
            owner: "tenant-a".into(),
            id: 1,
        },
        Seen {
            owner: "tenant-b".into(),
            id: 3,
        },
    ];
    assert_eq!(foreign_private_rows(&clean, &rows, "tenant-a"), 0);
    // 他テナントの Private 行が混入したら検出する。
    let mut leaked = clean.clone();
    leaked.push(Seen {
        owner: "tenant-b".into(),
        id: 1,
    });
    assert_eq!(foreign_private_rows(&leaked, &rows, "tenant-a"), 1);
    // 期待集合との差も検出する（欠落・余剰）。
    let q = Q {
        w: String::new(),
        index: "g_ops",
        ids: vec![1, 3],
        only_owner: None,
    };
    let want = expected(&rows, "tenant-a", true, &q);
    assert_eq!(want.len(), 2);
    assert_ne!(want, expected(&rows, "tenant-a", false, &q));
    // 偽装ポスティングの差し込みが空振りしないこと（キーの分解が rowid 8 バイトで終わる）。
    let key = [POSTING_TAG, 0, 1, b'a', 0, 0, 0, 0, 0, 0, 0, 7];
    assert_eq!(&key[key.len() - 8..], &7u64.to_be_bytes());
}
