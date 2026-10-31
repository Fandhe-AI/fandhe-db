//! WASM UDF の RLS 整合・ホスト非アクセスの境界テスト（Issue #1928。ポインタ: EXT-9・EXT-6、
//! RLS 側の観点は RLS-8・RLS-10。`docs/spec/04-behavior/extensions.md`）。
//!
//! 実際の wasmtime でコンパイルしたモジュールを SQL 経路（結果列・`WHERE`・集約・DML・明示 Tx・
//! `HINT ORDER` 全 6 通り）から呼び、「WASM 関数は引数だけを受け取り、RLS で不可視の行の値と
//! ホストの資源へ到達できない」ことを、3 テナント × 可視性 × 経路 × flood で固定する。
//! `wasm-udf` feature 無効では空になる（既定ビルドの拒否は `tests/wasm_udf_feature_off.rs`）。
//! プロセス共有の epoch ティッカー状態に触れるため、他テストへ束ねず単独で置く。
//!
//! オラクルは 2 系統（fresh Store のためモジュール内部のカウンタは使えない）:
//! - トリップワイヤ: 引数ベクトルの先頭が負なら `unreachable` で trap するモジュール。他テナントの
//!   Private 行（毒行）だけ負値で埋めるので、閲覧者の文が 22000 にならなければ毒行では評価されていない。
//!   毒行の所有者が Private 込みで同じ文を実行すると 22000 になること（空振りでないこと）も確認する
//! - 記録ラッパー: 実 backend を包み、渡された `v[0]` を記録して委譲する。記録値は閲覧者から可視の
//!   行の識別値の部分集合かつ非空でなければならない
//!
//! 行の識別値（`embedding[0]`・`v`）はシード時にテスト側が独立に持つ（production の可視性判定は
//! 呼ばない）。期待値の確定に推測は使わず、受理されない位置の拒否は wire_code で固定する。

#![cfg(feature = "wasm-udf")]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::wasm_udf::{
    compile_wasm_udf_backend, SandboxLimits, WasmSqlType, WasmUdfBackend, WasmUdfError,
    WasmUdfSignature,
};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[path = "support/wasm_encoder.rs"]
mod wasm_encoder;
use wasm_encoder::{plain_body, Spec, F32};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

// --- モジュール（WAT 相当をコメントで併記。引数は (ptr, len, s)・ベクトルは offset 1024 から）---

/// `if (f32.load(ptr) < 0) { unreachable }; return f32.load(ptr) * s`
fn trip_module() -> Vec<u8> {
    Spec::new(plain_body(&[
        0x20, 0, 0x2A, 2, 0, 0x43, 0, 0, 0, 0, 0x5D, 0x04, 0x40, 0x00, 0x0B, 0x20, 0, 0x2A, 2, 0,
        0x20, 2, 0x94,
    ]))
    .build()
}

/// `if (f32.load(ptr) < 0) { loop { br 0 } }; return f32.load(ptr) * s`（毒行でだけ fuel を使い切る）
fn spin_on_poison_module() -> Vec<u8> {
    Spec::new(plain_body(&[
        0x20, 0, 0x2A, 2, 0, 0x43, 0, 0, 0, 0, 0x5D, 0x04, 0x40, 0x03, 0x40, 0x0C, 0, 0x0B, 0x0B,
        0x20, 0, 0x2A, 2, 0, 0x20, 2, 0x94,
    ]))
    .build()
}

/// `loop { br 0 }; 0.0`（常に fuel を使い切る）
fn spin_always_module() -> Vec<u8> {
    Spec::new(plain_body(&[0x03, 0x40, 0x0C, 0, 0x0B, 0x43, 0, 0, 0, 0])).build()
}

/// `f(ptr, len, s) = f32.load(ptr) * s`
fn first_times_scalar() -> Vec<u8> {
    Spec::new(plain_body(&[0x20, 0, 0x2A, 2, 0, 0x20, 2, 0x94])).build()
}

/// `f(ptr, len, s) = f32.convert_i32_s(len) * s`
fn len_times_scalar() -> Vec<u8> {
    Spec::new(plain_body(&[0x20, 1, 0xB2, 0x20, 2, 0x94])).build()
}

/// `f32.load(ptr + len * 4)`（ベクトルの直後。ホストが他の値を置かなければ 0.0）
fn load_after_vector_module() -> Vec<u8> {
    Spec::new(plain_body(&[
        0x20, 0, 0x20, 1, 0x41, 4, 0x6C, 0x6A, 0x2A, 2, 0,
    ]))
    .build()
}

/// `f32.load(0)`（作業領域より前。ホストが他の値を置かなければ 0.0）
fn load_zero_module() -> Vec<u8> {
    Spec::new(plain_body(&[0x41, 0, 0x2A, 2, 0])).build()
}

/// `prev = f32.load(0); f32.store(0, f32.load(ptr)); return prev`（呼び出しをまたぐ持ち越しの検出）
fn carry_over_memory_module() -> Vec<u8> {
    let mut body = vec![1, 1, F32];
    body.extend_from_slice(&[
        0x41, 0, 0x2A, 2, 0, 0x21, 3, 0x41, 0, 0x20, 0, 0x2A, 2, 0, 0x38, 2, 0, 0x20, 3,
    ]);
    body.push(0x0B);
    Spec::new(body).build()
}

/// `r = convert(global0); global0 += 1; return r`（mutable global の持ち越しの検出）
fn carry_over_global_module() -> Vec<u8> {
    let mut spec = Spec::new(plain_body(&[
        0x23, 0, 0xB2, 0x23, 0, 0x41, 1, 0x6A, 0x24, 0,
    ]));
    spec.mutable_i32_global = true;
    spec.build()
}

/// `f32.load(131072)`（1 ページ = 64KiB を超える範囲外アクセス）
fn out_of_bounds_module() -> Vec<u8> {
    Spec::new(plain_body(&[0x41, 0x80, 0x80, 0x08, 0x2A, 2, 0])).build()
}

fn sig() -> WasmUdfSignature {
    WasmUdfSignature {
        params: vec![WasmSqlType::Vector, WasmSqlType::Real],
        ret: WasmSqlType::Real,
    }
}

fn register(
    core: &EngineCore,
    session: &mut SessionState,
    name: &str,
    bytes: &[u8],
) -> Result<(), SqlSurfaceError> {
    session.register_wasm_udf_from_module(name, &sig(), bytes, "f", core.wasm_limits())
}

// --- 記録ラッパー ---------------------------------------------------------------------

/// 実 backend を包み、引数ベクトルの先頭要素を記録してから委譲する（テスト専用）。
#[derive(Debug)]
struct Recorder {
    inner: Arc<dyn WasmUdfBackend>,
    seen: Arc<Mutex<Vec<f32>>>,
}

impl WasmUdfBackend for Recorder {
    fn call_vector_scalar(&self, v: &[f32], scalar: f64) -> Result<f64, WasmUdfError> {
        self.seen
            .lock()
            .expect("recorder lock")
            .push(v.first().copied().unwrap_or(f32::NAN));
        self.inner.call_vector_scalar(v, scalar)
    }
}

fn recorder_for(
    core: &EngineCore,
    bytes: &[u8],
) -> (Arc<dyn WasmUdfBackend>, Arc<Mutex<Vec<f32>>>) {
    let inner = compile_wasm_udf_backend(bytes, "f", core.wasm_limits()).expect("compile");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rec: Arc<dyn WasmUdfBackend> = Arc::new(Recorder {
        inner,
        seen: Arc::clone(&seen),
    });
    (rec, seen)
}

// --- フィクスチャ ---------------------------------------------------------------------

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

/// シード時の行の真実（オラクル用。production の可視性判定は呼ばない）。
#[derive(Clone, Copy, Debug)]
struct Truth {
    tenant: &'static str,
    id: u64,
    visibility: Visibility,
    /// 行固有の正の識別値（`embedding[0]` の絶対値であり `v` と同値）。
    ident: i32,
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

fn tenant_idx(tenant: &str) -> i32 {
    TENANTS.iter().position(|t| *t == tenant).expect("known") as i32 + 1
}

fn seed_row(
    storage: &Storage,
    tenant: &'static str,
    id: u64,
    visibility: Visibility,
    poison: bool,
    truths: &mut Vec<Truth>,
) {
    let ident = tenant_idx(tenant) * 100 + id as i32;
    let head = if poison {
        -(ident as f32)
    } else {
        ident as f32
    };
    let op = OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id");
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx_for(tenant, true),
        id,
        visibility,
        &[
            Value::Vector(vec![head, 0.5]),
            Value::Text(tenant.to_string()),
            Value::Integer(ident),
        ],
        &op,
    )
    .expect("seed row");
    truths.push(Truth {
        tenant,
        id,
        visibility,
        ident,
    });
}

/// 各テナントが id=1（Public）・id=2（Private）を持つ。`viewer` 以外のテナントの Private 行は毒行
/// （`embedding[0]` が負）。`flood=true` は `viewer` 以外が毒行の Private 行（id 3..=8）を足した状態。
fn fixture(label: &str, viewer: &str, flood: bool) -> Fixture {
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
        let other = t != viewer;
        seed_row(&storage, t, 1, Visibility::Public, false, &mut truths);
        seed_row(&storage, t, 2, Visibility::Private, other, &mut truths);
        if flood && other {
            for id in 3..=8u64 {
                seed_row(&storage, t, id, Visibility::Private, true, &mut truths);
            }
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    Fixture {
        core,
        truths,
        _guard: guard,
    }
}

// --- 実行ヘルパー ---------------------------------------------------------------------

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    c: &PolicyContext,
    explicit_txn: bool,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    if explicit_txn {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(c, session, &mut txn, "BEGIN")?;
        let o = core.execute_sql_in_txn(c, session, &mut txn, sql)?;
        core.execute_sql_in_txn(c, session, &mut txn, "COMMIT")?;
        Ok(o)
    } else {
        core.execute_sql_in_session(c, session, sql)
    }
}

fn query(
    core: &EngineCore,
    session: &mut SessionState,
    c: &PolicyContext,
    explicit_txn: bool,
    sql: &str,
) -> Result<QueryResult, SqlSurfaceError> {
    match run(core, session, c, explicit_txn, sql)? {
        SqlOutcome::Query(q) => Ok(q),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn float_of(c: &Cell) -> f64 {
    match c {
        Cell::Float(v) => *v,
        other => panic!("expected float cell, got {other:?}"),
    }
}

fn int_of(c: &Cell) -> i64 {
    match c {
        Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        Cell::SignedInteger(v) => *v,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

fn owner_of(c: &Cell) -> String {
    match c {
        Cell::Text(s) => s.clone(),
        other => panic!("expected text cell, got {other:?}"),
    }
}

/// 結果の各行を `(owner, id, 第 2 列)` に正規化する。
fn rows_of(q: &QueryResult) -> Vec<(String, u64, Cell)> {
    q.rows
        .iter()
        .map(|r| (owner_of(&r.cells[0]), r.id, r.cells[1].clone()))
        .collect()
}

const THRESHOLD: i32 = 150;

#[derive(Clone, Copy, Debug)]
enum Pos {
    Column,
    Where,
    Both,
    Aggregate,
}
const POSITIONS: [Pos; 4] = [Pos::Column, Pos::Where, Pos::Both, Pos::Aggregate];

#[derive(Clone, Copy, Debug)]
enum Path {
    Scan,
    Search(Option<&'static str>),
}

const HINTS: [&str; 6] = [
    "RLS, SCALAR, DISTANCE",
    "RLS, DISTANCE, SCALAR",
    "SCALAR, RLS, DISTANCE",
    "SCALAR, DISTANCE, RLS",
    "DISTANCE, RLS, SCALAR",
    "DISTANCE, SCALAR, RLS",
];

fn paths() -> Vec<Path> {
    let mut v = vec![Path::Scan, Path::Search(None)];
    v.extend(HINTS.iter().map(|h| Path::Search(Some(*h))));
    v
}

fn build_sql(pos: Pos, path: Path) -> String {
    let tail = match path {
        Path::Scan => " LIMIT 100".to_string(),
        Path::Search(None) => " ORDER BY embedding <=> '[1.0,0.5]' LIMIT 100".to_string(),
        Path::Search(Some(h)) => {
            format!(" ORDER BY embedding <=> '[1.0,0.5]' LIMIT 100 HINT ORDER({h})")
        }
    };
    match pos {
        Pos::Column => format!("SELECT owner, trip(embedding, 2.0) AS t FROM docs{tail}"),
        Pos::Where => {
            format!("SELECT owner, v FROM docs WHERE trip(embedding, 1.0) > {THRESHOLD}.0{tail}")
        }
        Pos::Both => format!(
            "SELECT owner, trip(embedding, 2.0) AS t FROM docs \
             WHERE trip(embedding, 1.0) > {THRESHOLD}.0{tail}"
        ),
        Pos::Aggregate => {
            format!("SELECT count(*) FROM docs WHERE trip(embedding, 1.0) > {THRESHOLD}.0")
        }
    }
}

/// 観測値の正規化形。Aggregate は件数 1 要素、それ以外は `(owner, id, 値)` の昇順。
#[derive(Debug, PartialEq, Clone)]
enum Observed {
    Rows(Vec<(String, u64, i64)>),
    Count(i64),
    Err(String, String),
}

/// オラクルに従い期待する行集合（閲覧者から可視で、`WHERE` がある位置では識別値が閾値を超える行）。
fn expected_rows(
    truths: &[Truth],
    viewer: &str,
    private: bool,
    pos: Pos,
) -> Vec<(String, u64, i64)> {
    let mut out: Vec<(String, u64, i64)> = truths
        .iter()
        .filter(|t| allowed(t, viewer, private))
        .filter(|t| matches!(pos, Pos::Column) || t.ident > THRESHOLD)
        .map(|t| {
            let value = match pos {
                Pos::Column | Pos::Both => i64::from(t.ident) * 2,
                _ => i64::from(t.ident),
            };
            (t.tenant.to_string(), t.id, value)
        })
        .collect();
    out.sort();
    out
}

/// 実行して正規化する。結果列の値は `ident * 2`（`2.0` を掛ける）または `v`。
fn observe(
    core: &EngineCore,
    session: &mut SessionState,
    c: &PolicyContext,
    explicit_txn: bool,
    pos: Pos,
    path: Path,
) -> Observed {
    let sql = build_sql(pos, path);
    match query(core, session, c, explicit_txn, &sql) {
        Err(e) => Observed::Err(e.wire_code().to_string(), e.to_string()),
        Ok(q) => match pos {
            Pos::Aggregate => {
                assert_eq!(q.rows.len(), 1, "{sql}");
                Observed::Count(int_of(&q.rows[0].cells[0]))
            }
            Pos::Where => {
                let mut r: Vec<_> = rows_of(&q)
                    .into_iter()
                    .map(|(o, id, c)| (o, id, int_of(&c)))
                    .collect();
                r.sort();
                Observed::Rows(r)
            }
            _ => {
                let mut r: Vec<_> = rows_of(&q)
                    .into_iter()
                    .map(|(o, id, c)| (o, id, float_of(&c) as i64))
                    .collect();
                r.sort();
                Observed::Rows(r)
            }
        },
    }
}

fn expected_observed(truths: &[Truth], viewer: &str, private: bool, pos: Pos) -> Observed {
    let rows = expected_rows(truths, viewer, private, pos);
    match pos {
        Pos::Aggregate => Observed::Count(rows.len() as i64),
        _ => Observed::Rows(rows),
    }
}

// --- T1・T2・T8: 位置 × 経路 × 双子オラクル × flood ----------------------------------------

#[test]
fn t1_t2_t8_poison_rows_are_never_evaluated_across_positions_paths_and_floods() {
    let mut total = 0usize;
    let mut non_empty = 0usize;
    // (viewer, pos, path, private, txn) -> flood なしの観測。flood ありと一致しなければならない（T2）。
    let mut baseline: std::collections::HashMap<String, Observed> = Default::default();
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            let fx = fixture("t1", viewer, flood);
            for private in [false, true] {
                let c = ctx_for(viewer, private);
                for pos in POSITIONS {
                    for path in paths() {
                        // 集約は経路を持たない（ORDER BY・HINT が付かない）ため 1 経路で数える。
                        if matches!(pos, Pos::Aggregate) && !matches!(path, Path::Scan) {
                            continue;
                        }
                        for txn in [false, true] {
                            let label =
                                format!("{viewer} private={private} {pos:?} {path:?} txn={txn}");
                            let expected = expected_observed(&fx.truths, viewer, private, pos);

                            // 系統 1: 実モジュールのトリップワイヤ（from_module 経由）。
                            let mut s = SessionState::default();
                            register(&fx.core, &mut s, "trip", &trip_module()).expect("register");
                            let got = observe(&fx.core, &mut s, &c, txn, pos, path);
                            assert_eq!(got, expected, "tripwire: {label}");

                            // 系統 2: 記録ラッパー（register_wasm_udf 経由）。
                            let (rec, seen) = recorder_for(&fx.core, &trip_module());
                            let mut s2 = SessionState::default();
                            s2.register_wasm_udf("trip", rec)
                                .expect("register recorder");
                            let got2 = observe(&fx.core, &mut s2, &c, txn, pos, path);
                            assert_eq!(got2, expected, "recorder: {label}");
                            let visible: BTreeSet<i32> = fx
                                .truths
                                .iter()
                                .filter(|t| allowed(t, viewer, private))
                                .map(|t| t.ident)
                                .collect();
                            let recorded = seen.lock().expect("lock").clone();
                            assert!(!recorded.is_empty(), "recorder saw nothing: {label}");
                            for v in recorded {
                                assert!(
                                    v > 0.0 && visible.contains(&(v as i32)),
                                    "invisible value reached the UDF: {label}"
                                );
                            }

                            // 素の SQL（双子）: 同じ閲覧者が WASM 無しで得る可視集合と一致する。
                            if !matches!(pos, Pos::Aggregate) {
                                let plain = query(
                                    &fx.core,
                                    &mut SessionState::default(),
                                    &c,
                                    txn,
                                    &format!(
                                        "SELECT owner, v FROM docs WHERE v > {THRESHOLD} LIMIT 100"
                                    ),
                                )
                                .expect("plain twin");
                                let mut twin: Vec<(String, u64)> = rows_of(&plain)
                                    .into_iter()
                                    .map(|(o, id, _)| (o, id))
                                    .collect();
                                twin.sort();
                                if !matches!(pos, Pos::Column) {
                                    let mut mine: Vec<(String, u64)> = match &got {
                                        Observed::Rows(r) => {
                                            r.iter().map(|(o, id, _)| (o.clone(), *id)).collect()
                                        }
                                        _ => unreachable!("rows expected"),
                                    };
                                    mine.sort();
                                    assert_eq!(mine, twin, "twin: {label}");
                                }
                            }

                            total += 1;
                            let has = match &got {
                                Observed::Rows(r) => !r.is_empty(),
                                Observed::Count(n) => *n > 0,
                                Observed::Err(..) => false,
                            };
                            if has {
                                non_empty += 1;
                            }
                            // T2: flood 有無で観測値が変わらない。
                            let key = label;
                            if flood {
                                assert_eq!(
                                    baseline.get(&key),
                                    Some(&got),
                                    "flood changed the observation: {key}"
                                );
                            } else {
                                baseline.insert(key, got);
                            }
                        }
                    }
                }
            }
        }
    }
    // 位置 3 × 経路 8 + 集約 1 = 25、× txn 2 × ctx 2 × viewer 3 × flood 2。
    assert_eq!(total, 25 * 2 * 2 * 3 * 2);
    // 全組み合わせで非空（Public 行は常に可視で、閾値を超える識別値の行が存在する）。
    assert_eq!(non_empty, total);
}

/// トリップワイヤが空振りでないこと: 毒行の所有者が Private 込みで同じ文を実行すると 22000 になる。
#[test]
fn tripwire_fires_for_the_owner_of_poison_rows() {
    let fx = fixture("arm", "tenant-a", false);
    for &owner in ["tenant-b", "tenant-c"].iter() {
        for pos in [Pos::Column, Pos::Where, Pos::Both, Pos::Aggregate] {
            for path in [Path::Scan, Path::Search(Some("DISTANCE, SCALAR, RLS"))] {
                let mut s = SessionState::default();
                register(&fx.core, &mut s, "trip", &trip_module()).expect("register");
                let got = observe(&fx.core, &mut s, &ctx_for(owner, true), false, pos, path);
                match got {
                    Observed::Err(code, _) => assert_eq!(code, "22000", "{owner} {pos:?} {path:?}"),
                    other => panic!("tripwire did not fire for {owner} {pos:?}: {other:?}"),
                }
            }
        }
    }
}

// --- T3: DML の不干渉 -----------------------------------------------------------------------

/// 各テナントが所有する行 `(owner, id, v)` を、所有者自身の視点（Private 込み）で全件読む。
fn owned_rows(core: &EngineCore) -> Vec<(String, u64, i64)> {
    let mut out = Vec::new();
    for &t in TENANTS.iter() {
        let r = core
            .execute_sql(&ctx_for(t, true), "SELECT owner, v FROM docs LIMIT 1000")
            .expect("owned read");
        for row in &r.rows {
            let owner = owner_of(&row.cells[0]);
            if owner == t {
                out.push((owner, row.id, int_of(&row.cells[1])));
            }
        }
    }
    out.sort();
    out
}

/// 述語形式の `UPDATE`／`DELETE` の `WHERE` に WASM 呼び出しは置けない（fail-closed の `42601`）。
/// 拒否が可視性・flood・Tx に依存せず固定文言で、どのテナントの行にも副作用を残さないことを固定する。
/// 他テナントの行に WASM の評価が届く経路そのものが無いことの確認でもある。
#[test]
fn t3_dml_with_wasm_predicate_is_rejected_without_side_effects() {
    let mut rejected = 0usize;
    let mut op_seq = 0u32;
    let mut messages: Vec<String> = Vec::new();
    for flood in [false, true] {
        for &viewer in TENANTS.iter() {
            for private in [false, true] {
                for txn in [false, true] {
                    for kind in ["update", "delete"] {
                        op_seq += 1;
                        let fx = fixture("t3", viewer, flood);
                        let before = owned_rows(&fx.core);
                        let sql = match kind {
                            "update" => format!(
                                "UPDATE docs SET v = v + 1000 WHERE trip(embedding, 1.0) > 0.0 \
                                 USING OPERATION_ID 'dml-{op_seq}'"
                            ),
                            _ => format!(
                                "DELETE FROM docs WHERE trip(embedding, 1.0) > 0.0 \
                                 USING OPERATION_ID 'dml-{op_seq}'"
                            ),
                        };
                        let label =
                            format!("{kind} {viewer} private={private} txn={txn} flood={flood}");
                        let (rec, seen) = recorder_for(&fx.core, &trip_module());
                        let mut s = SessionState::default();
                        s.register_wasm_udf("trip", rec).expect("register");
                        let err = run(&fx.core, &mut s, &ctx_for(viewer, private), txn, &sql)
                            .expect_err(&label);
                        assert_eq!(err.wire_code(), "42601", "{label}: {err}");
                        messages.push(err.to_string());
                        assert!(seen.lock().expect("lock").is_empty(), "WASM ran: {label}");
                        assert_eq!(owned_rows(&fx.core), before, "side effect: {label}");
                        rejected += 1;
                    }
                }
            }
        }
    }
    assert_eq!(rejected, 2 * 3 * 2 * 2 * 2);
    assert!(messages.iter().all(|m| m == &messages[0]), "{messages:?}");
}

// --- T4: 名前解決・セッション境界 ---------------------------------------------------------------

#[test]
fn t4_same_name_in_different_sessions_resolves_to_each_sessions_own_module() {
    let fx = fixture("t4", "tenant-a", true);
    let sql = "SELECT owner, wfn(embedding, 2.0) AS t FROM docs \
               ORDER BY embedding <=> '[1.0,0.5]' LIMIT 100";
    let mut a = SessionState::default();
    let mut b = SessionState::default();
    register(&fx.core, &mut a, "wfn", &first_times_scalar()).expect("register a");
    register(&fx.core, &mut b, "wfn", &len_times_scalar()).expect("register b");

    let ra = query(&fx.core, &mut a, &ctx_for("tenant-a", true), false, sql).expect("a");
    assert!(!ra.rows.is_empty());
    for (o, id, c) in rows_of(&ra) {
        let t = fx
            .truths
            .iter()
            .find(|t| t.tenant == o && t.id == id)
            .expect("known row");
        assert_eq!(float_of(&c), f64::from(t.ident) * 2.0, "a {o}/{id}");
    }
    let rb = query(&fx.core, &mut b, &ctx_for("tenant-b", true), false, sql).expect("b");
    assert!(!rb.rows.is_empty());
    for (o, id, c) in rows_of(&rb) {
        // ベクトル長 2 に 2.0 を掛けた値だけ（a の定義の値は現れない）。
        assert_eq!(float_of(&c), 4.0, "b {o}/{id}");
    }
}

#[test]
fn t4_bound_session_is_rejected_for_another_tenant_and_wasm_is_not_called() {
    let fx = fixture("t4b", "tenant-a", false);
    let sql = "SELECT owner, trip(embedding, 2.0) AS t FROM docs LIMIT 100";

    let (rec, seen) = recorder_for(&fx.core, &trip_module());
    let mut s = SessionState::default();
    assert!(s.bind_resolution_tenant("tenant-a"));
    s.register_wasm_udf("trip", rec).expect("register");

    // 別テナントの ctx は拒否され、WASM は 1 度も呼ばれない。
    let err = query(&fx.core, &mut s, &ctx_for("tenant-b", true), false, sql)
        .expect_err("mismatch must be rejected");
    assert_eq!(err.wire_code(), "XX000");
    assert!(!err.to_string().contains("tenant-"), "{err}");
    assert!(seen.lock().expect("lock").is_empty(), "WASM must not run");
    let err = query(&fx.core, &mut s, &ctx_for("tenant-b", true), true, sql)
        .expect_err("mismatch must be rejected in a transaction too");
    assert_eq!(err.wire_code(), "XX000");
    assert!(seen.lock().expect("lock").is_empty(), "WASM must not run");

    // 対照: 束縛先のテナントでは実行でき、WASM が呼ばれる。
    let ok = query(&fx.core, &mut s, &ctx_for("tenant-a", true), false, sql).expect("bound tenant");
    assert!(!ok.rows.is_empty());
    assert!(!seen.lock().expect("lock").is_empty());
}

#[test]
fn t4_rebinding_to_another_tenant_is_fail_closed() {
    let fx = fixture("t4c", "tenant-a", false);
    let sql = "SELECT owner, trip(embedding, 2.0) AS t FROM docs LIMIT 100";

    // 登録後に別テナントへ束縛しようとしても拒否される（bind は元の束縛を保つ）。
    let (rec, seen) = recorder_for(&fx.core, &trip_module());
    let mut s = SessionState::default();
    assert!(s.bind_resolution_tenant("tenant-a"));
    s.register_wasm_udf("trip", rec).expect("register");
    let err = fx
        .core
        .load_session_udfs(&ctx_for("tenant-b", true), &mut s)
        .expect_err("rebinding must be rejected");
    assert_eq!(err.wire_code(), "XX000");
    assert!(!err.to_string().contains("tenant-"), "{err}");
    assert!(!s.bind_resolution_tenant("tenant-b"));
    assert!(seen.lock().expect("lock").is_empty());

    // 未束縛で登録 → tenant-b へロードして束縛 → tenant-a の ctx での実行は拒否される。
    let (rec, seen) = recorder_for(&fx.core, &trip_module());
    let mut s = SessionState::default();
    s.register_wasm_udf("trip", rec).expect("register");
    fx.core
        .load_session_udfs(&ctx_for("tenant-b", true), &mut s)
        .expect("first binding");
    let err = query(&fx.core, &mut s, &ctx_for("tenant-a", true), false, sql)
        .expect_err("other tenant must be rejected");
    assert_eq!(err.wire_code(), "XX000");
    assert!(seen.lock().expect("lock").is_empty(), "WASM must not run");
}

// --- T5: ホスト非アクセス --------------------------------------------------------------------

#[test]
fn t5_host_imports_are_rejected_with_fixed_message_and_registry_unchanged() {
    let fx = fixture("t5a", "tenant-a", false);
    let body = [0x43, 0, 0, 0, 0];
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for (module, field) in [
        ("wasi_snapshot_preview1", "fd_write"),
        ("wasi_snapshot_preview1", "path_open"),
        ("env", "h"),
    ] {
        let mut spec = Spec::new(plain_body(&body));
        spec.func_imports = 1;
        spec.import_module = module;
        spec.import_field = field;
        cases.push(spec.build());
    }
    let mut mem = Spec::new(plain_body(&body));
    mem.memory_import = true;
    cases.push(mem.build());

    for bytes in cases {
        let mut s = SessionState::default();
        let err = register(&fx.core, &mut s, "wfn", &bytes).expect_err("import must be rejected");
        assert_eq!(err.wire_code(), "42P13", "{err}");
        let msg = err.to_string();
        for leaked in ["fd_write", "path_open", "wasi", "env", "mem"] {
            assert!(!msg.contains(leaked), "message leaks {leaked}: {msg}");
        }
        assert!(s.udfs().is_empty(), "registry must stay empty");
        register(&fx.core, &mut s, "wfn", &first_times_scalar()).expect("register after failure");
    }
}

/// 全テナント・全可視性・flood 有無で、`bytes` が返す値がすべて 0.0 であることを確かめる。
fn assert_all_zero(label: &str, bytes: &[u8]) {
    for flood in [false, true] {
        let fx = fixture("t5b", "tenant-a", flood);
        for &t in TENANTS.iter() {
            for private in [false, true] {
                let mut s = SessionState::default();
                register(&fx.core, &mut s, "probe", bytes).expect("register");
                let q = query(
                    &fx.core,
                    &mut s,
                    &ctx_for(t, private),
                    false,
                    "SELECT owner, probe(embedding, 1.0) AS p FROM docs \
                     ORDER BY embedding <=> '[1.0,0.5]' LIMIT 100",
                )
                .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert!(q.rows.len() >= 2, "{label}: rows");
                for (o, id, c) in rows_of(&q) {
                    assert_eq!(float_of(&c), 0.0, "{label} {t} {o}/{id}");
                }
            }
        }
    }
}

#[test]
fn t5_linear_memory_holds_only_the_argument_vector() {
    assert_all_zero("after-vector", &load_after_vector_module());
    assert_all_zero("offset-zero", &load_zero_module());
}

#[test]
fn t5_nothing_is_carried_over_between_rows_or_calls() {
    assert_all_zero("memory-carry-over", &carry_over_memory_module());
    assert_all_zero("global-carry-over", &carry_over_global_module());

    // 同一の compile 済み backend を別テナントの別セッションへ登録しても値が持ち越されない。
    let fx = fixture("t5c", "tenant-a", true);
    let shared = compile_wasm_udf_backend(&carry_over_memory_module(), "f", fx.core.wasm_limits())
        .expect("compile");
    for &t in ["tenant-a", "tenant-b", "tenant-a"].iter() {
        let mut s = SessionState::default();
        s.register_wasm_udf("probe", Arc::clone(&shared))
            .expect("register shared");
        let q = query(
            &fx.core,
            &mut s,
            &ctx_for(t, true),
            false,
            "SELECT owner, probe(embedding, 1.0) AS p FROM docs LIMIT 100",
        )
        .expect("query");
        assert!(!q.rows.is_empty());
        for (_, _, c) in rows_of(&q) {
            assert_eq!(float_of(&c), 0.0, "{t}");
        }
    }
}

#[test]
fn t5_out_of_bounds_access_is_22000_and_sessions_survive() {
    let fx = fixture("t5d", "tenant-a", false);
    let sql_oob = "SELECT owner, oob(embedding, 1.0) AS p FROM docs LIMIT 100";
    let sql_ok = "SELECT owner, wfn(embedding, 1.0) AS p FROM docs LIMIT 100";
    let mut s = SessionState::default();
    register(&fx.core, &mut s, "oob", &out_of_bounds_module()).expect("register");
    register(&fx.core, &mut s, "wfn", &first_times_scalar()).expect("register");
    let c = ctx_for("tenant-a", true);
    let err = query(&fx.core, &mut s, &c, false, sql_oob).expect_err("oob must fail");
    assert_eq!(err.wire_code(), "22000");
    assert!(!query(&fx.core, &mut s, &c, false, sql_ok)
        .expect("same session")
        .rows
        .is_empty());
    let mut other = SessionState::default();
    register(&fx.core, &mut other, "wfn", &first_times_scalar()).expect("register");
    let c_b = ctx_for("tenant-b", true);
    assert!(!query(&fx.core, &mut other, &c_b, false, sql_ok)
        .expect("other session")
        .rows
        .is_empty());
}

// --- T6: 上限 × RLS（決定的な fuel のみ。epoch の時間には依存しない）------------------------------

fn limited(fx_core: EngineCore) -> EngineCore {
    fx_core.with_wasm_limits(SandboxLimits {
        fuel: 10_000,
        ..SandboxLimits::default()
    })
}

#[test]
fn t6_invisible_rows_do_not_consume_the_fuel_budget() {
    let sql = "SELECT owner, spin(embedding, 1.0) AS p FROM docs LIMIT 100";
    for flood in [false, true] {
        let fx = fixture("t6", "tenant-a", flood);
        let core = limited(fx.core);
        let mut s = SessionState::default();
        register(&core, &mut s, "spin", &spin_on_poison_module()).expect("register");
        // 閲覧者は成功する（毒行の無限ループに到達しない）。
        let ok = query(&core, &mut s, &ctx_for("tenant-a", true), false, sql).expect("viewer");
        assert!(!ok.rows.is_empty());
        // 毒行の所有者は 54000（対照: 上限が実際に効いている）。
        let err = query(&core, &mut s, &ctx_for("tenant-b", true), false, sql)
            .expect_err("owner hits the loop");
        assert_eq!(err.wire_code(), "54000");
    }
}

#[test]
fn t6_always_spinning_module_has_the_same_error_regardless_of_flood_and_viewer() {
    let sql = "SELECT owner, spin(embedding, 1.0) AS p FROM docs LIMIT 100";
    let mut seen: Vec<(String, String)> = Vec::new();
    for flood in [false, true] {
        let fx = fixture("t6b", "tenant-a", flood);
        let core = limited(fx.core);
        for &t in TENANTS.iter() {
            let mut s = SessionState::default();
            register(&core, &mut s, "spin", &spin_always_module()).expect("register");
            let err = query(&core, &mut s, &ctx_for(t, false), false, sql).expect_err("limit");
            seen.push((err.wire_code().to_string(), err.to_string()));
        }
    }
    assert!(seen.iter().all(|e| e.0 == "54000"));
    assert!(seen.iter().all(|e| e == &seen[0]), "{seen:?}");
}

// --- T7: エラー文面の不変性 --------------------------------------------------------------------

#[test]
fn t7_error_messages_do_not_depend_on_tenant_or_flood_and_carry_no_tenant_info() {
    // (名前, 発生させる関数) ごとに (code, message) を集め、全条件で同一であることを確かめる。
    let mut trap: Vec<(String, String)> = Vec::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut rejected: Vec<(String, String)> = Vec::new();
    for flood in [false, true] {
        let fx = fixture("t7", "tenant-a", flood);
        for &t in ["tenant-b", "tenant-c"].iter() {
            let c = ctx_for(t, true);
            // 22000: 毒行（tenant-a 視点の fixture で b・c の Private 行）で trip。
            let mut s = SessionState::default();
            register(&fx.core, &mut s, "trip", &trip_module()).expect("register");
            let e = query(
                &fx.core,
                &mut s,
                &c,
                false,
                "SELECT owner, trip(embedding, 1.0) AS p FROM docs LIMIT 100",
            )
            .expect_err("trap");
            trap.push((e.wire_code().to_string(), e.to_string()));
            // 42883: この文脈では未登録の関数。
            let e = query(
                &fx.core,
                &mut SessionState::default(),
                &c,
                false,
                "SELECT owner, trip(embedding, 1.0) AS p FROM docs LIMIT 100",
            )
            .expect_err("missing");
            missing.push((e.wire_code().to_string(), e.to_string()));
            // 42P13: import を持つモジュールの登録。
            let mut spec = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
            spec.func_imports = 1;
            spec.import_module = "wasi_snapshot_preview1";
            spec.import_field = "fd_write";
            let e = register(&fx.core, &mut SessionState::default(), "wfn", &spec.build())
                .expect_err("rejected");
            rejected.push((e.wire_code().to_string(), e.to_string()));
        }
    }
    for (name, list, code) in [
        ("trap", &trap, "22000"),
        ("missing", &missing, "42883"),
        ("rejected", &rejected, "42P13"),
    ] {
        assert!(list.iter().all(|e| e.0 == code), "{name}: {list:?}");
        assert!(list.iter().all(|e| e == &list[0]), "{name}: {list:?}");
        for (_, m) in list {
            for needle in [
                "tenant-a", "tenant-b", "tenant-c", "-201", "-301", "fd_write",
            ] {
                assert!(!m.contains(needle), "{name} leaks {needle}: {m}");
            }
        }
    }
}
