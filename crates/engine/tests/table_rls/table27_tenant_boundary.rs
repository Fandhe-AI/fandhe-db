//! SCHEMA 導入後のテナント境界・fail-closed の横断回帰テスト（TABLE-27・Issue #1719）。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27、関連 RLS-7・RLS-10・ERR-6。
//! 設計記録は `docs/design/schema-tenant-namespace.md`（D3・D4・D7・D11。実装記録は §11）。
//!
//! `table27_*.rs` の各テストは機能単位で、他テナントの検査は 1〜数件ずつに分かれている。本ファイルは
//! それらを書き直さず、まだ無い軸（3 テナントの回転・可視性 2 モード・独立オラクル・双子 DB との
//! バイト一致・複数の入口・試行総数の固定・負の対照）を足す。実 `Storage` ＋ `CpuScalarProvider` を
//! 使い、`EngineCore` の production 入口（`*_in_session`・`*_in_txn`・`begin_copy*`）を通す。
//! 本番どおり、セッション（`SessionState`）はテナントごとに別に持つ。
//!
//! 検査軸:
//!
//! - T0 陽性対照: 自テナントの `s.t`／`public.t` が行を返す（空振りの防止）
//! - T1 存在の秘匿: 他テナントの SCHEMA がある DB とどこにも無い DB で、閲覧側の応答
//!   （SQLSTATE・文言）が完全一致する（autocommit・明示トランザクション・拡張クエリ・COPY）
//! - T2 `search_path` に他テナントの SCHEMA 名を置いても境界を越えない（`$user` の展開を含む）
//! - T3 修飾名・`search_path` のどちらを経由しても暗黙 RLS が外れない（独立オラクル）
//! - T3b Public 行を持つ共有表（Rust API 作成の従来方式の表）でも同じ契約が成り立つ。なお多層防御（D3）の
//!   物理表への他テナント行の直接差し込みは、SQL 表が採番方式で公開 API から書けないため本ファイルでは扱わない
//! - T5 内部 ID の接頭辞（`#1.` と `#10.`）をテナントをまたいで混同しない
//! - T6 3 テナントが同名の SCHEMA 配下に同名のオブジェクトを作っても衝突しない
//! - T7 試行総数の固定（各テストの末尾）、T8 検査器自身への負の対照
//!
//! オラクルは production の `PolicyContext::is_visible`／`is_owner` を呼ばず、テスト側が持つ真実値
//! （どのテナントが何を入れたか）から期待集合を自前で計算する。

use std::collections::BTreeSet;

use redb::{ReadableDatabase, TableHandle};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::using_operation_id::OperationId;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 他テナントだけが知る名前（閲覧側の文には一度も書かない）。応答に現れたら存在の漏えい。
const WATCH: &[&str] = &["zz_secret_c", "zz_secret_t", "zz_secret_q"];

fn ctx(tenant: &str, private: bool) -> PolicyContext {
    if private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    (open_core(&path), guard)
}

/// テナント 1 本ぶんのセッション（DDL 許可・解決テナントを束縛。接続ごとに 1 テナントの本番形）。
fn session(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

type Resp = Result<SqlOutcome, (String, String)>;

fn run(core: &EngineCore, tenant: &str, private: bool, s: &mut SessionState, sql: &str) -> Resp {
    core.execute_sql_in_session(&ctx(tenant, private), s, sql)
        .map_err(|e| (e.wire_code().to_string(), e.to_string()))
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, tenant, true, s, sql).unwrap_or_else(|e| panic!("{tenant}: {sql}: {e:?}"))
}

/// 応答の比較用表現（成功は Debug、失敗は SQLSTATE と文言）。
fn shape(r: &Resp) -> String {
    match r {
        Ok(o) => format!("OK {o:?}"),
        Err((c, m)) => format!("ERR {c} {m}"),
    }
}

fn cell_u64(c: &Cell) -> u64 {
    match c {
        Cell::Integer(u) => *u,
        Cell::SignedInteger(i) => u64::try_from(*i).expect("non-negative"),
        other => panic!("not an integer cell: {other:?}"),
    }
}

fn query_ids(o: &SqlOutcome) -> BTreeSet<u64> {
    match o {
        SqlOutcome::Query(r) => r.rows.iter().map(|row| cell_u64(&row.cells[0])).collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 結果の `(id, v)` 組を並べ替えて返す（重複行も保持する。先頭 2 列が整数であること）。
fn query_pairs(o: &SqlOutcome) -> Vec<(u64, u64)> {
    match o {
        SqlOutcome::Query(r) => {
            let mut v: Vec<(u64, u64)> = r
                .rows
                .iter()
                .map(|row| (cell_u64(&row.cells[0]), cell_u64(&row.cells[1])))
                .collect();
            v.sort_unstable();
            v
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn pairs_of(rows: &[Truth]) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = rows.iter().map(|t| (t.id, t.v)).collect();
    v.sort_unstable();
    v
}

fn count_of(o: &SqlOutcome) -> u64 {
    match o {
        SqlOutcome::Query(r) => cell_u64(&r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// エラー応答に内部キー・他テナントだけが知る名前・閲覧側が書いていない数字が現れないことの検査器。
/// 違反があれば理由を返す（T8 が直接この関数に捏造した違反を与える）。
fn leak_reason(sql: &str, message: &str) -> Option<String> {
    if message.contains('#') {
        return Some(format!("internal key marker in {message:?}"));
    }
    for w in WATCH {
        if message.contains(w) {
            return Some(format!("watch name {w} in {message:?}"));
        }
    }
    let mut digits = String::new();
    for ch in message.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !digits.is_empty() {
            if !sql.contains(&digits) {
                return Some(format!(
                    "number {digits} not written by caller in {message:?}"
                ));
            }
            digits.clear();
        }
    }
    None
}

fn assert_no_leak(sql: &str, r: &Resp) {
    if let Err((_, m)) = r {
        if let Some(why) = leak_reason(sql, m) {
            panic!("{sql}: {why}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// T1: 双子 DB（他テナントの SCHEMA がある DB X とどこにも無い DB Y）の完全一致
// ---------------------------------------------------------------------------------------------

/// 閲覧側 tenant-b の自前の表 `mine` を作る。両方の DB に同じものを置く。
fn seed_viewer(core: &EngineCore, s: &mut SessionState) {
    ok(
        core,
        TENANTS[1],
        s,
        "CREATE TABLE mine (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    ok(
        core,
        TENANTS[1],
        s,
        "INSERT INTO mine (id, n) VALUES (1, 10)",
    );
}

/// DB X: tenant-a が SCHEMA `a` と各種オブジェクト（見張り名つき）を持つ。
fn seed_foreign_schema(core: &EngineCore) {
    let mut a = session(TENANTS[0]);
    for sql in [
        "CREATE SCHEMA a",
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT, CONSTRAINT zz_secret_c UNIQUE (name))",
        "INSERT INTO a.t (id, name) VALUES (1, 'x')",
        "CREATE VIEW a.v AS SELECT id, name FROM a.t",
        "CREATE INDEX zz_secret_t ON a.t (name)",
        "CREATE SEQUENCE a.zz_secret_q",
        "CREATE SEQUENCE a.q",
        "CREATE FUNCTION a.f(x) AS x * 2",
    ] {
        ok(core, TENANTS[0], &mut a, sql);
    }
}

/// 閲覧側が実行する文。すべて拒否側（他テナントの `a` は存在しないのと同じ）であるべきもの。
const MUST_FAIL: &[&str] = &[
    "SELECT id FROM a.t LIMIT 5",
    "SELECT id, name FROM a.v LIMIT 5",
    "SELECT COUNT(*) FROM a.t",
    "INSERT INTO a.t (id, name) VALUES (2, 'y')",
    "UPDATE a.t SET name = 'y' WHERE id = 1",
    "DELETE FROM a.t WHERE id = 1",
    "TRUNCATE a.t",
    "CREATE TABLE a.z (id BIGINT PRIMARY KEY)",
    "CREATE VIEW a.v2 AS SELECT id FROM a.t",
    "CREATE SEQUENCE a.q2",
    "CREATE INDEX i2 ON a.t (name)",
    "DROP TABLE a.t",
    "DROP VIEW a.v",
    "DROP SEQUENCE a.q",
    "DROP INDEX a.zz_secret_t",
    "DROP SCHEMA a",
    "DROP SCHEMA a CASCADE",
    "ALTER TABLE a.t ADD COLUMN c TEXT",
    "ALTER SEQUENCE a.q RESTART",
    "ALTER SCHEMA a RENAME TO b",
    "ALTER TABLE mine SET SCHEMA a",
    "SELECT id, a.f(n) AS d FROM mine ORDER BY id LIMIT 10",
    "INSERT INTO mine (id, n) VALUES (2, nextval('a.q'))",
    "EXPLAIN SELECT id FROM a.t LIMIT 5",
    "SELECT m.id FROM mine m JOIN a.t x ON m.id = x.id LIMIT 5",
    "SELECT id FROM mine WHERE id IN (SELECT id FROM a.t LIMIT 5) LIMIT 5",
    "WITH c AS (SELECT id FROM a.t LIMIT 5) SELECT id FROM c LIMIT 5",
];

/// 許可される SQLSTATE（SCHEMA／オブジェクトが無い・未対応の系統）。成功してはならない。
const FAIL_CODES: &[&str] = &[
    "3F000", "42P01", "42883", "42704", "42501", "0A000", "42601",
];

fn twin_worlds(label: &str) -> (EngineCore, CleanupGuard, EngineCore, CleanupGuard) {
    let (x, gx) = new_core(&format!("{label}-x"));
    let (y, gy) = new_core(&format!("{label}-y"));
    seed_foreign_schema(&x);
    let mut vx = session(TENANTS[1]);
    let mut vy = session(TENANTS[1]);
    seed_viewer(&x, &mut vx);
    seed_viewer(&y, &mut vy);
    (x, gx, y, gy)
}

#[test]
fn t1_autocommit_responses_match_a_world_without_the_schema() {
    let mut trials = 0usize;
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t1-auto");
    for private in [false, true] {
        let mut vx = session(TENANTS[1]);
        let mut vy = session(TENANTS[1]);
        for sql in MUST_FAIL {
            let rx = run(&x, TENANTS[1], private, &mut vx, sql);
            let ry = run(&y, TENANTS[1], private, &mut vy, sql);
            assert_eq!(shape(&rx), shape(&ry), "{sql} (private={private})");
            let (code, _) = rx.as_ref().expect_err(sql);
            assert!(FAIL_CODES.contains(&code.as_str()), "{sql}: {code}");
            assert_no_leak(sql, &rx);
            trials += 1;
        }
    }
    assert_eq!(trials, MUST_FAIL.len() * 2, "trial count drifted");
}

#[test]
fn t1_explicit_transaction_responses_match_a_world_without_the_schema() {
    let mut trials = 0usize;
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t1-txn");
    let mut vx = session(TENANTS[1]);
    let mut vy = session(TENANTS[1]);
    let c = ctx(TENANTS[1], true);
    for sql in MUST_FAIL {
        let mut chains = Vec::new();
        for (core, s) in [(&x, &mut vx), (&y, &mut vy)] {
            let mut txn = core.new_session_transaction();
            let mut out = Vec::new();
            for step in ["BEGIN", sql, "SELECT id FROM mine LIMIT 1", "ROLLBACK"] {
                let r = core
                    .execute_sql_in_txn(&c, s, &mut txn, step)
                    .map_err(|e| (e.wire_code().to_string(), e.to_string()));
                assert_no_leak(sql, &r);
                out.push(shape(&r));
            }
            chains.push(out);
        }
        assert_eq!(chains[0], chains[1], "{sql}");
        // 失敗後の文は中断済みトランザクションとして拒否される（25P02）か、文自体が
        // トランザクション内で未対応として拒否される。どちらでも成功してはならない。
        assert!(chains[0][1].starts_with("ERR"), "{sql}: {:?}", chains[0]);
        // 失敗後の後続 SELECT も拒否され（fail-closed）、ROLLBACK は成功して状態を解く。
        assert!(chains[0][2].starts_with("ERR"), "{sql}: {:?}", chains[0]);
        assert!(chains[0][3].starts_with("OK"), "{sql}: {:?}", chains[0]);
        trials += 1;
    }
    assert_eq!(trials, MUST_FAIL.len(), "trial count drifted");
}

#[test]
fn t1_extended_query_responses_match_a_world_without_the_schema() {
    let mut trials = 0usize;
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t1-ext");
    let c = ctx(TENANTS[1], true);
    for sql in MUST_FAIL {
        let mut out = Vec::new();
        for core in [&x, &y] {
            let mut s = session(TENANTS[1]);
            let mut trace = String::new();
            let res = (|| {
                let p = core
                    .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
                    .map_err(|e| ("parse", e))?;
                if let Err(e) = core.describe_prepared_in_session(&s, &p) {
                    trace.push_str(&format!("describe:{} {e};", e.wire_code()));
                }
                let b = core
                    .bind_prepared_in_session(&s, &p, &[])
                    .map_err(|e| ("bind", e))?;
                core.execute_parsed_in_session(&c, &mut s, &b)
                    .map_err(|e| ("execute", e))
            })();
            match res {
                Ok(o) => trace.push_str(&format!("OK {o:?}")),
                Err((stage, e)) => {
                    let m = e.to_string();
                    assert!(leak_reason(sql, &m).is_none(), "{sql}: {m}");
                    trace.push_str(&format!("{stage}:{} {m}", e.wire_code()));
                }
            }
            out.push(trace);
        }
        assert_eq!(out[0], out[1], "{sql}");
        assert!(!out[0].contains("OK "), "{sql}: {}", out[0]);
        trials += 1;
    }
    assert_eq!(trials, MUST_FAIL.len(), "trial count drifted");
}

#[test]
fn t1_copy_responses_match_a_world_without_the_schema() {
    let mut trials = 0usize;
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t1-copy");
    let c = ctx(TENANTS[1], true);
    let sx = session(TENANTS[1]);
    let sy = session(TENANTS[1]);
    for sql in [
        "COPY a.t (id, name) FROM STDIN",
        "COPY a.t TO STDOUT",
        "COPY (SELECT id FROM a.t) TO STDOUT",
    ] {
        // autocommit と明示トランザクションの両方の入口。
        let ax = x
            .begin_copy(&c, &sx, sql)
            .map(|_| ())
            .map_err(|e| (e.wire_code().to_string(), e.to_string()));
        let ay = y
            .begin_copy(&c, &sy, sql)
            .map(|_| ())
            .map_err(|e| (e.wire_code().to_string(), e.to_string()));
        assert!(ax.is_err(), "{sql}");
        assert_eq!(format!("{ax:?}"), format!("{ay:?}"), "{sql}");
        let (_, m) = ax.as_ref().expect_err(sql);
        assert!(leak_reason(sql, m).is_none(), "{sql}: {m}");
        trials += 1;

        let mut tx = x.new_session_transaction();
        let mut ty = y.new_session_transaction();
        x.execute_sql_in_txn(&c, &mut session(TENANTS[1]), &mut tx, "BEGIN")
            .expect("begin x");
        y.execute_sql_in_txn(&c, &mut session(TENANTS[1]), &mut ty, "BEGIN")
            .expect("begin y");
        let bx = x
            .begin_copy_in_txn(&c, &sx, &mut tx, sql)
            .map(|_| ())
            .map_err(|e| (e.wire_code().to_string(), e.to_string()));
        let by = y
            .begin_copy_in_txn(&c, &sy, &mut ty, sql)
            .map(|_| ())
            .map_err(|e| (e.wire_code().to_string(), e.to_string()));
        assert!(bx.is_err(), "{sql}");
        assert_eq!(format!("{bx:?}"), format!("{by:?}"), "{sql}");
        trials += 1;
    }
    assert_eq!(trials, 3 * 2, "trial count drifted");
}

#[test]
fn t1_viewer_can_create_a_schema_with_the_same_name_and_it_is_independent() {
    let (x, _gx, _y, _gy) = twin_worlds("t27-tb-t1-create");
    let mut v = session(TENANTS[1]);
    // 他テナントの `a` があっても `42P06` にならず、閲覧側自身の `a` が作れる。
    ok(&x, TENANTS[1], &mut v, "CREATE SCHEMA a");
    ok(
        &x,
        TENANTS[1],
        &mut v,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    assert_eq!(
        count_of(&ok(&x, TENANTS[1], &mut v, "SELECT COUNT(*) FROM a.t")),
        0
    );
    let mut a = session(TENANTS[0]);
    assert_eq!(
        count_of(&ok(&x, TENANTS[0], &mut a, "SELECT COUNT(*) FROM a.t")),
        1
    );
    // 閲覧側が自分の `a` を落としても tenant-a の `a` は無傷。
    ok(&x, TENANTS[1], &mut v, "DROP SCHEMA a CASCADE");
    assert_eq!(
        count_of(&ok(&x, TENANTS[0], &mut a, "SELECT COUNT(*) FROM a.t")),
        1
    );
}

// ---------------------------------------------------------------------------------------------
// T2: search_path に他テナントの SCHEMA 名を置く
// ---------------------------------------------------------------------------------------------

#[test]
fn t2_search_path_never_resolves_into_another_tenants_schema() {
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t2");
    // 閲覧側 public.mine に既知の行がある。tenant-a の `a.t` の行 (id=1) には絶対に当たらない。
    let script: &[&str] = &[
        "SET search_path = a",
        "SELECT id FROM t LIMIT 5",
        "SELECT id FROM mine LIMIT 5",
        "CREATE TABLE z (id BIGINT PRIMARY KEY)",
        "CREATE VIEW zv AS SELECT id FROM mine",
        "CREATE SEQUENCE zq",
        "SET search_path = a, public",
        "SELECT id FROM mine LIMIT 5",
        "SELECT id FROM t LIMIT 5",
        "SET search_path = public, a",
        "SELECT id FROM mine LIMIT 5",
        "SELECT id FROM a.t LIMIT 5",
        "RESET search_path",
    ];
    for private in [false, true] {
        let mut sx = session(TENANTS[1]);
        let mut sy = session(TENANTS[1]);
        for sql in script {
            let rx = run(&x, TENANTS[1], private, &mut sx, sql);
            let ry = run(&y, TENANTS[1], private, &mut sy, sql);
            assert_eq!(shape(&rx), shape(&ry), "{sql} (private={private})");
            assert_no_leak(sql, &rx);
        }
    }
    // `a, public` の下の未修飾 `mine` は閲覧側自身の行だけ（tenant-a の行 id=1 は `mine` の id=1 と
    // 値が違うので、取り違えれば n が 10 にならない）。
    let mut s = session(TENANTS[1]);
    ok(&x, TENANTS[1], &mut s, "SET search_path = a, public");
    let o = ok(&x, TENANTS[1], &mut s, "SELECT n FROM mine LIMIT 5");
    assert_eq!(query_ids(&o), BTreeSet::from([10]));
}

#[test]
fn t2_user_named_like_a_foreign_schema_does_not_resolve_to_it() {
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t2-user");
    let mut sx = session(TENANTS[1]);
    let mut sy = session(TENANTS[1]);
    // username が他テナントの SCHEMA 名と同じ。既定の path `"$user", public` で `a` を探しても、
    // 他テナントの `a` は見えないので public に解決される（DB Y と同じ応答）。
    sx.set_session_user("a");
    sy.set_session_user("a");
    for sql in [
        "SELECT id FROM mine LIMIT 5",
        "SELECT id FROM t LIMIT 5",
        "CREATE TABLE z (id BIGINT PRIMARY KEY)",
        "INSERT INTO z (id) VALUES (1)",
        "SELECT id FROM z LIMIT 5",
    ] {
        let rx = run(&x, TENANTS[1], true, &mut sx, sql);
        let ry = run(&y, TENANTS[1], true, &mut sy, sql);
        assert_eq!(shape(&rx), shape(&ry), "{sql}");
        assert_no_leak(sql, &rx);
    }
    // tenant-a の `a.t` は無傷。
    let mut a = session(TENANTS[0]);
    assert_eq!(
        count_of(&ok(&x, TENANTS[0], &mut a, "SELECT COUNT(*) FROM a.t")),
        1
    );
}

#[test]
fn t2_rollback_restores_the_path_without_crossing_the_boundary() {
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t2-rb");
    let c = ctx(TENANTS[1], true);
    let mut chains = Vec::new();
    for core in [&x, &y] {
        let mut s = session(TENANTS[1]);
        let mut txn = core.new_session_transaction();
        let mut out = Vec::new();
        for sql in [
            "BEGIN",
            "SET search_path = a",
            "SELECT id FROM mine LIMIT 5",
            "ROLLBACK",
        ] {
            let r = core
                .execute_sql_in_txn(&c, &mut s, &mut txn, sql)
                .map_err(|e| (e.wire_code().to_string(), e.to_string()));
            assert_no_leak(sql, &r);
            out.push(shape(&r));
        }
        out.push(shape(&run(
            core,
            TENANTS[1],
            true,
            &mut s,
            "SHOW search_path",
        )));
        out.push(shape(&run(
            core,
            TENANTS[1],
            true,
            &mut s,
            "SELECT id FROM mine LIMIT 5",
        )));
        chains.push(out);
    }
    assert_eq!(chains[0], chains[1]);
}

#[test]
fn t2_extended_query_after_path_change_matches_a_world_without_the_schema() {
    let (x, _gx, y, _gy) = twin_worlds("t27-tb-t2-ext");
    let c = ctx(TENANTS[1], true);
    let mut traces = Vec::new();
    for core in [&x, &y] {
        let mut s = session(TENANTS[1]);
        let p = core
            .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM mine LIMIT 5", &[])
            .expect("parse under default path");
        let b = core.bind_prepared_in_session(&s, &p, &[]).expect("bind");
        ok(core, TENANTS[1], &mut s, "SET search_path = a");
        let mut t = Vec::new();
        // Parse 後に path を他テナントの SCHEMA 名へ変えた。Bind は 42P01、Bind 済みの Execute は 0A000。
        match core.bind_prepared_in_session(&s, &p, &[]) {
            Ok(_) => panic!("bind must fail"),
            Err(e) => t.push(format!("bind {} {e}", e.wire_code())),
        }
        match core.execute_parsed_in_session(&c, &mut s, &b) {
            Ok(_) => panic!("stale execute must fail"),
            Err(e) => t.push(format!("exec {} {e}", e.wire_code())),
        }
        for line in &t {
            assert!(
                leak_reason(
                    "SELECT id FROM mine LIMIT 5",
                    line.splitn(3, ' ').nth(2).unwrap_or("")
                )
                .is_none(),
                "{line}"
            );
        }
        traces.push(t);
    }
    assert_eq!(traces[0], traces[1]);
    assert!(traces[0][0].starts_with("bind 42P01"), "{:?}", traces[0]);
    assert!(traces[0][1].starts_with("exec 0A000"), "{:?}", traces[0]);
}

// ---------------------------------------------------------------------------------------------
// T3: 修飾名・search_path を経由しても暗黙 RLS が外れない（独立オラクル）
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Table {
    Public,
    Schema,
}

/// テスト側の真実値 1 行（誰が・どの表へ・どの id・どの可視性・どの値で入れたか）。
#[derive(Clone, Debug)]
struct Truth {
    tenant: usize,
    table: Table,
    id: u64,
    v: u64,
    vis: Visibility,
}

/// 独立オラクル: `viewer` が `private` モードで `table` の行として見てよいか。
fn oracle_sees(viewer: usize, private: bool, t: &Truth) -> bool {
    let label_allowed = match t.vis {
        Visibility::Public => true,
        Visibility::Private => private,
    };
    if !label_allowed {
        return false;
    }
    match t.table {
        // 共有 public 表: Public 行は誰にでも、Private 行は所有者にだけ。
        Table::Public => t.vis == Visibility::Public || t.tenant == viewer,
        // テナント SCHEMA の表: 所有者の行だけ。
        Table::Schema => t.tenant == viewer,
    }
}

fn expected(truths: &[Truth], viewer: usize, private: bool, table: Table) -> Vec<Truth> {
    truths
        .iter()
        .filter(|t| t.table == table && oracle_sees(viewer, private, t))
        .cloned()
        .collect()
}

fn id_set(rows: &[Truth]) -> BTreeSet<u64> {
    rows.iter().map(|t| t.id).collect()
}

/// 3 テナントがそれぞれ同名の SCHEMA `s`・同じ形の `s.t` を作り、共有の `public.t` にも行を入れる。
/// `s.t` の id は 1..=3（テナント間で同じ）、`public.t` の id は `1000 * (テナント番号 + 1) + k`。
fn seed_matrix(core: &EngineCore) -> Vec<Truth> {
    let mut truths = Vec::new();
    for (i, tenant) in TENANTS.iter().enumerate() {
        let mut s = session(tenant);
        for sql in [
            "CREATE SCHEMA s",
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, v INTEGER)",
            "CREATE INDEX t_v ON s.t (v)",
            "CREATE VIEW s.vw AS SELECT id, v FROM s.t",
        ] {
            ok(core, tenant, &mut s, sql);
        }
        if i == 0 {
            ok(
                core,
                tenant,
                &mut s,
                "CREATE TABLE t (id BIGINT PRIMARY KEY, v INTEGER)",
            );
            ok(core, tenant, &mut s, "CREATE INDEX pt_v ON t (v)");
        }
        for k in 1..=3u64 {
            let v = 100 * (i as u64 + 1) + k;
            ok(
                core,
                tenant,
                &mut s,
                &format!("INSERT INTO s.t (id, v) VALUES ({k}, {v})"),
            );
            truths.push(Truth {
                tenant: i,
                table: Table::Schema,
                id: k,
                v,
                vis: Visibility::Private,
            });
            let pid = 1000 * (i as u64 + 1) + k;
            ok(
                core,
                tenant,
                &mut s,
                &format!("INSERT INTO public.t (id, v) VALUES ({pid}, {v})"),
            );
            truths.push(Truth {
                tenant: i,
                table: Table::Public,
                id: pid,
                v,
                vis: Visibility::Private,
            });
        }
    }
    truths
}

/// 参照形ごとの SQL とオラクル。`(説明, SQL, 期待 (id, v) 組の並べ替え済み列)`。
/// id は全テナントで同じ 1〜3 を持つため、テナント固有の v も比較して他テナントの行への取り違えと重複行を検出する。
type Form = (String, String, Vec<(u64, u64)>);

fn read_forms(truths: &[Truth], viewer: usize, private: bool) -> Vec<Form> {
    let pubs = expected(truths, viewer, private, Table::Public);
    let schs = expected(truths, viewer, private, Table::Schema);
    let mut join: Vec<(u64, u64)> = Vec::new();
    for a in &schs {
        for b in &pubs {
            if a.v == b.v {
                join.push((a.id, a.v));
            }
        }
    }
    join.sort_unstable();
    let in_sub: Vec<Truth> = schs
        .iter()
        .filter(|a| pubs.iter().any(|b| b.v == a.v))
        .cloned()
        .collect();
    let mut union = pairs_of(&schs);
    union.extend(pairs_of(&pubs));
    union.sort_unstable();
    union.dedup();
    let mut out: Vec<Form> = Vec::new();
    for (name, q, set) in [
        ("public.t", "public.t", pairs_of(&pubs)),
        ("s.t", "s.t", pairs_of(&schs)),
    ] {
        out.push((
            format!("{name} all"),
            format!("SELECT id, v FROM {q} LIMIT 1000"),
            set.clone(),
        ));
        out.push((
            format!("{name} indexed"),
            format!("SELECT id, v FROM {q} WHERE v >= 0 LIMIT 1000"),
            set.clone(),
        ));
        out.push((
            format!("{name} exists"),
            format!(
                "SELECT id, v FROM {q} WHERE EXISTS (SELECT id FROM {q} LIMIT 1000) LIMIT 1000"
            ),
            set,
        ));
    }
    out.push((
        "s.vw".into(),
        "SELECT id, v FROM s.vw LIMIT 1000".into(),
        pairs_of(&schs),
    ));
    out.push((
        "join".into(),
        "SELECT a.id, a.v FROM s.t a JOIN public.t b ON a.v = b.v LIMIT 1000".into(),
        join,
    ));
    out.push((
        "in-subquery".into(),
        "SELECT id, v FROM s.t WHERE v IN (SELECT v FROM public.t LIMIT 1000) LIMIT 1000".into(),
        pairs_of(&in_sub),
    ));
    out.push((
        "union".into(),
        "SELECT id, v FROM s.t UNION SELECT id, v FROM public.t".into(),
        union,
    ));
    out
}

#[test]
fn t0_t3_reads_through_every_reference_form_match_the_independent_oracle() {
    let mut trials = 0usize;
    let (core, _g) = new_core("t27-tb-t3-read");
    let truths = seed_matrix(&core);
    let mut nonempty = 0usize;
    for (viewer, tenant) in TENANTS.iter().enumerate() {
        for private in [false, true] {
            let mut s = session(tenant);
            for (name, sql, want) in read_forms(&truths, viewer, private) {
                let got = query_pairs(&ok_mode(&core, tenant, private, &mut s, &sql));
                assert_eq!(got, want, "{tenant} private={private} {name}: {sql}");
                if !want.is_empty() {
                    nonempty += 1;
                }
                trials += 1;
            }
            // COUNT は行数と一致する。
            for (q, table) in [("public.t", Table::Public), ("s.t", Table::Schema)] {
                let o = ok_mode(
                    &core,
                    tenant,
                    private,
                    &mut s,
                    &format!("SELECT COUNT(*) FROM {q}"),
                );
                assert_eq!(
                    count_of(&o) as usize,
                    expected(&truths, viewer, private, table).len(),
                    "{tenant} {q}"
                );
                trials += 1;
            }
            // `search_path = s, public` の下の未修飾 `t` は public.t と同じ集合（`s` の中の `t` へは
            // 解決されない現状も fail-closed 側。§4.3）。
            ok_mode(
                &core,
                tenant,
                private,
                &mut s,
                "SET search_path = s, public",
            );
            let got = query_pairs(&ok_mode(
                &core,
                tenant,
                private,
                &mut s,
                "SELECT id, v FROM t LIMIT 1000",
            ));
            let want = pairs_of(&expected(&truths, viewer, private, Table::Public));
            let sch = pairs_of(&expected(&truths, viewer, private, Table::Schema));
            assert!(
                got == want || got == sch,
                "{tenant} private={private} search_path form: got {got:?}, public {want:?}, schema {sch:?}"
            );
            trials += 1;
        }
    }
    // 陽性対照: 期待集合が空でない組が十分にある（空振りで green にならない）。
    assert_eq!(nonempty, 30, "positive controls drifted");
    // 3 テナントが同じ id を持つ s.t でも、閲覧者の行だけが返る。
    for (viewer, tenant) in TENANTS.iter().enumerate() {
        let mut s = session(tenant);
        let o = ok_mode(&core, tenant, true, &mut s, "SELECT v FROM s.t LIMIT 1000");
        let vs: BTreeSet<u64> = match &o {
            SqlOutcome::Query(r) => r.rows.iter().map(|row| cell_u64(&row.cells[0])).collect(),
            other => panic!("{other:?}"),
        };
        let want: BTreeSet<u64> = (1..=3).map(|k| 100 * (viewer as u64 + 1) + k).collect();
        assert_eq!(vs, want, "{tenant}");
    }
    let forms = read_forms(&truths, 0, true).len();
    assert_eq!(trials, 3 * 2 * (forms + 2 + 1), "trial count drifted");
}

fn ok_mode(
    core: &EngineCore,
    tenant: &str,
    private: bool,
    s: &mut SessionState,
    sql: &str,
) -> SqlOutcome {
    run(core, tenant, private, s, sql).unwrap_or_else(|e| panic!("{tenant}: {sql}: {e:?}"))
}

/// 書き込み台本（`w` が自分の `s.t`・`public.t` へ）。
const WRITES: &[&str] = &[
    "UPDATE s.t SET v = v + 5000 WHERE id >= 1",
    "UPDATE public.t SET v = v + 5000 WHERE id >= 1",
    "DELETE FROM s.t WHERE id = 1",
    "DELETE FROM public.t WHERE id >= 0",
    "INSERT INTO s.t (id, v) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET v = 9",
    "TRUNCATE s.t",
];

/// 閲覧者 `i` から見える、他テナントの行の `(id, v)`（public.t は他人の id 範囲だけ、s.t は自分の分）。
type Pairs = Vec<(u64, u64)>;

fn snapshot(core: &EngineCore, i: usize, skip_writer: usize) -> (Pairs, Pairs) {
    let mut s = session(TENANTS[i]);
    let pairs = |sql: &str, s: &mut SessionState| -> Vec<(u64, u64)> {
        match ok(core, TENANTS[i], s, sql) {
            SqlOutcome::Query(r) => r
                .rows
                .iter()
                .map(|x| (cell_u64(&x.cells[0]), cell_u64(&x.cells[1])))
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    let lo = 1000 * (skip_writer as u64 + 1);
    let mut p = pairs("SELECT id, v FROM public.t ORDER BY id LIMIT 1000", &mut s);
    p.retain(|(id, _)| !(lo..lo + 1000).contains(id));
    let sch = pairs("SELECT id, v FROM s.t ORDER BY id LIMIT 1000", &mut s);
    (p, sch)
}

#[test]
fn t3_writes_by_one_tenant_never_change_another_tenants_rows() {
    let mut trials = 0usize;
    for (writer, w) in TENANTS.iter().enumerate() {
        for via_txn in [false, true] {
            let (core, _g) = new_core(&format!("t27-tb-t3-write-{writer}-{via_txn}"));
            let _truths = seed_matrix(&core);
            let others: Vec<usize> = (0..TENANTS.len()).filter(|i| *i != writer).collect();
            let before: Vec<_> = others.iter().map(|i| snapshot(&core, *i, writer)).collect();
            // 事前に空でないこと（陽性対照）。
            assert!(before.iter().all(|(_, sch)| sch.len() == 3));
            let mut s = session(w);
            let c = ctx(w, true);
            let mut txn = core.new_session_transaction();
            if via_txn {
                core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
                    .expect("begin");
            }
            for sql in WRITES {
                let r = if via_txn {
                    core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
                } else {
                    core.execute_sql_in_session(&c, &mut s, sql)
                };
                r.unwrap_or_else(|e| panic!("{w}: {sql}: {} {e}", e.wire_code()));
                trials += 1;
            }
            if via_txn {
                core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
                    .expect("commit");
            }
            let after: Vec<_> = others.iter().map(|i| snapshot(&core, *i, writer)).collect();
            assert_eq!(before, after, "writer={w} txn={via_txn}");
            // 自テナントの s.t は TRUNCATE で空、他は無傷（上の比較）。
            let mine = snapshot(&core, writer, writer);
            assert!(mine.1.is_empty(), "{w}: own s.t must be truncated");
        }
    }
    assert_eq!(trials, 3 * 2 * WRITES.len(), "trial count drifted");
}

// ---------------------------------------------------------------------------------------------
// T5: 内部 ID の接頭辞衝突（#1. と #10.）
// ---------------------------------------------------------------------------------------------

#[test]
fn t5_internal_id_prefixes_do_not_collide_across_tenants() {
    let path = unique_db_path("t27-tb-t5");
    let _guard = CleanupGuard(path.clone());
    // テナントを交互にして SCHEMA を 14 個作る（s0 が最初の ID、s9 以降が 2 桁の ID になる並び）。
    let names: Vec<String> = (0..14).map(|n| format!("s{n}")).collect();
    {
        let core = open_core(&path);
        let mut sess = [session(TENANTS[0]), session(TENANTS[1])];
        for (n, name) in names.iter().enumerate() {
            let who = n % 2;
            let t = TENANTS[who];
            ok(&core, t, &mut sess[who], &format!("CREATE SCHEMA {name}"));
            ok(
                &core,
                t,
                &mut sess[who],
                &format!("CREATE TABLE {name}.t (id BIGINT, v INTEGER)"),
            );
            ok(
                &core,
                t,
                &mut sess[who],
                &format!("INSERT INTO {name}.t (id, v) VALUES (1, {n})"),
            );
            ok(
                &core,
                t,
                &mut sess[who],
                &format!("CREATE SEQUENCE {name}.q"),
            );
        }
    }
    // 前提の確認: 1 桁の内部 ID（`#1.`）と 2 桁の内部 ID（`#10.`）の物理表が両方ある
    // （接頭辞が衝突しうる並びになっている。内部キーは利用者 API に出ないため raw redb の表名で見る）。
    {
        let db = redb::Database::open(&path).expect("open raw redb");
        let r = db.begin_read().expect("begin read");
        let names: Vec<String> = r
            .list_tables()
            .expect("list tables")
            .map(|h| h.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n.contains("#1.t")), "premise: #1.t");
        assert!(names.iter().any(|n| n.contains("#10.t")), "premise: #10.t");
    }
    let core = open_core(&path);
    let mut sess = [session(TENANTS[0]), session(TENANTS[1])];
    let holder = |name: &str| names.iter().position(|n| n == name).expect("known") % 2;
    // 偶数番号 (tenant-a) の SCHEMA に対する破壊的操作が、奇数番号 (tenant-b) の物を壊さない。
    let intact = |core: &EngineCore, sess: &mut [SessionState; 2]| {
        for (n, name) in names.iter().enumerate() {
            let who = holder(name);
            let t = TENANTS[who];
            let o = ok(
                core,
                t,
                &mut sess[who],
                &format!("SELECT v FROM {name}.t WHERE id = 1 LIMIT 5"),
            );
            assert_eq!(query_ids(&o), BTreeSet::from([n as u64]), "{name}");
        }
    };
    let intact_b = |core: &EngineCore, sess: &mut [SessionState; 2]| {
        for (n, name) in names.iter().enumerate().filter(|(n, _)| n % 2 == 1) {
            let o = ok(
                core,
                TENANTS[1],
                &mut sess[1],
                &format!("SELECT v FROM {name}.t WHERE id = 1 LIMIT 5"),
            );
            assert_eq!(query_ids(&o), BTreeSet::from([n as u64]), "{name}");
        }
    };
    intact(&core, &mut sess);
    ok(
        &core,
        TENANTS[0],
        &mut sess[0],
        "INSERT INTO s0.t (id, v) VALUES (99, nextval('s0.q'))",
    );
    ok(&core, TENANTS[0], &mut sess[0], "TRUNCATE s0.t");
    ok(
        &core,
        TENANTS[0],
        &mut sess[0],
        "ALTER SCHEMA s0 RENAME TO r0",
    );
    intact_b(&core, &mut sess);
    ok(
        &core,
        TENANTS[0],
        &mut sess[0],
        "ALTER SCHEMA r0 RENAME TO s0",
    );
    ok(&core, TENANTS[0], &mut sess[0], "DROP SCHEMA s0 CASCADE");
    intact_b(&core, &mut sess);
    for n in (3..14).step_by(2) {
        let name = format!("s{n}");
        // 採番値は tenant-b 内で独立している（s1 が #1x の SEQUENCE の続きを取らない）。
        ok(
            &core,
            TENANTS[1],
            &mut sess[1],
            &format!("INSERT INTO {name}.t (id, v) VALUES (99, nextval('{name}.q'))"),
        );
        let o = ok(
            &core,
            TENANTS[1],
            &mut sess[1],
            &format!("SELECT v FROM {name}.t WHERE id = 99 LIMIT 5"),
        );
        assert_eq!(query_ids(&o), BTreeSet::from([1]), "{name}: first nextval");
    }
    // 削除した名前を別テナントが作り直しても、古いオブジェクトに当たらない。
    ok(&core, TENANTS[1], &mut sess[1], "CREATE SCHEMA s0");
    ok(
        &core,
        TENANTS[1],
        &mut sess[1],
        "CREATE TABLE s0.t (id BIGINT, v INTEGER)",
    );
    assert_eq!(
        count_of(&ok(
            &core,
            TENANTS[1],
            &mut sess[1],
            "SELECT COUNT(*) FROM s0.t"
        )),
        0
    );
    ok(&core, TENANTS[1], &mut sess[1], "DROP SCHEMA s0 CASCADE");
    intact_b(&core, &mut sess);
}

// ---------------------------------------------------------------------------------------------
// T6: 名前の衝突（3 テナントが同名の SCHEMA に同名のオブジェクトを作る）
// ---------------------------------------------------------------------------------------------

#[test]
fn t6_same_named_objects_in_same_named_schemas_do_not_collide() {
    let (core, _g) = new_core("t27-tb-t6");
    // UDF はセッション単位で保持されるため、テナントごとのセッションを最後まで使い続ける。
    let mut sess: Vec<SessionState> = TENANTS.iter().map(|t| session(t)).collect();
    for (i, tenant) in TENANTS.iter().enumerate() {
        let s = &mut sess[i];
        for sql in [
            "CREATE SCHEMA s".to_string(),
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT, CONSTRAINT u1 UNIQUE (name), CONSTRAINT c1 CHECK (id > 0))".to_string(),
            "CREATE TABLE s.p (id BIGINT PRIMARY KEY, tid BIGINT)".to_string(),
            "CREATE VIEW s.v AS SELECT id, name FROM s.t".to_string(),
            "CREATE INDEX ix ON s.t (name)".to_string(),
            "CREATE SEQUENCE s.q".to_string(),
            format!("CREATE FUNCTION s.f(x) AS x * {}", i + 2),
            format!("INSERT INTO s.t (id, name) VALUES (1, 'n{i}')"),
        ] {
            // 他テナントの同名オブジェクトがあっても 42P07／42710 等にならない。
            let r = run(&core, tenant, true, s, &sql);
            assert!(r.is_ok(), "{tenant}: {sql}: {r:?}");
        }
    }
    // どれか 1 テナントの DROP／RENAME が他テナントに影響しない。
    ok(&core, TENANTS[1], &mut sess[1], "DROP VIEW s.v");
    ok(&core, TENANTS[1], &mut sess[1], "DROP INDEX s.ix");
    ok(&core, TENANTS[1], &mut sess[1], "DROP SEQUENCE s.q");
    ok(
        &core,
        TENANTS[1],
        &mut sess[1],
        "ALTER SCHEMA s RENAME TO s_b",
    );
    for i in [0usize, 2] {
        let s = &mut sess[i];
        let o = ok(&core, TENANTS[i], s, "SELECT name FROM s.v LIMIT 5");
        assert!(matches!(o, SqlOutcome::Query(ref r) if r.rows.len() == 1));
        ok(
            &core,
            TENANTS[i],
            s,
            "INSERT INTO s.p (id, tid) VALUES (1, nextval('s.q'))",
        );
        let o = ok(&core, TENANTS[i], s, "SELECT tid FROM s.p LIMIT 5");
        assert_eq!(query_ids(&o), BTreeSet::from([1]), "independent sequence");
        let o = ok(
            &core,
            TENANTS[i],
            s,
            "SELECT id, s.f(id) AS d FROM s.t ORDER BY id LIMIT 10",
        );
        // 同名 UDF は自テナントの係数 i + 2 で評価される（他テナントの関数へ誤解決しない）。
        match &o {
            SqlOutcome::Query(r) => {
                assert_eq!(r.rows.len(), 1);
                assert_eq!(cell_u64(&r.rows[0].cells[0]), 1);
                assert_eq!(
                    r.rows[0].cells[1],
                    Cell::Float((i + 2) as f64),
                    "s.f of {i}"
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
        // UNIQUE 制約は各テナントで独立に効いている（自分の重複は拒否、他テナントの値とは無関係）。
        let dup = run(
            &core,
            TENANTS[i],
            true,
            s,
            &format!("INSERT INTO s.t (id, name) VALUES (2, 'n{i}')"),
        );
        assert!(dup.is_err(), "own unique constraint must still hold");
        let other = (i + 1) % 3;
        let fine = run(
            &core,
            TENANTS[i],
            true,
            s,
            &format!("INSERT INTO s.t (id, name) VALUES (3, 'n{other}')"),
        );
        assert!(
            fine.is_ok(),
            "other tenants' values must not conflict: {fine:?}"
        );
    }
    // 改名後の `s_b` は tenant-b だけに見える。
    assert_eq!(
        count_of(&ok(
            &core,
            TENANTS[1],
            &mut sess[1],
            "SELECT COUNT(*) FROM s_b.t"
        )),
        1
    );
    for i in [0usize, 2] {
        let s = &mut sess[i];
        let r = run(&core, TENANTS[i], true, s, "SELECT COUNT(*) FROM s_b.t");
        assert_eq!(r.expect_err("s_b is invisible").0, "3F000");
    }
}

// ---------------------------------------------------------------------------------------------
// T7／T8: 試行総数の固定と、検査器自身への負の対照
// ---------------------------------------------------------------------------------------------

#[test]
fn t8_leak_checker_detects_fabricated_violations() {
    // 内部キー、見張り名、閲覧側が書いていない数字、をそれぞれ混ぜた捏造応答は違反として検出される。
    assert!(leak_reason("SELECT id FROM a.t", "relation \"#3.t\" does not exist").is_some());
    assert!(leak_reason("SELECT id FROM a.t", "constraint zz_secret_c violated").is_some());
    assert!(leak_reason("SELECT id FROM a.t", "schema 7 does not exist").is_some());
    // 閲覧側が書いた数字・名前だけの応答は違反ではない。
    assert!(leak_reason(
        "SELECT id FROM a.t LIMIT 5",
        "schema \"a\" does not exist, limit 5"
    )
    .is_none());
    // オラクルは他テナントの行を足した結果と一致しない。
    let truths = vec![
        Truth {
            tenant: 0,
            table: Table::Schema,
            id: 1,
            v: 1,
            vis: Visibility::Private,
        },
        Truth {
            tenant: 1,
            table: Table::Schema,
            id: 1,
            v: 2,
            vis: Visibility::Private,
        },
        Truth {
            tenant: 1,
            table: Table::Public,
            id: 9,
            v: 3,
            vis: Visibility::Private,
        },
        Truth {
            tenant: 1,
            table: Table::Public,
            id: 8,
            v: 3,
            vis: Visibility::Public,
        },
    ];
    assert_eq!(
        id_set(&expected(&truths, 0, true, Table::Schema)),
        BTreeSet::from([1])
    );
    assert!(expected(&truths, 0, false, Table::Schema).is_empty());
    assert_eq!(
        id_set(&expected(&truths, 0, true, Table::Public)),
        BTreeSet::from([8])
    );
    assert_eq!(
        id_set(&expected(&truths, 1, true, Table::Public)),
        BTreeSet::from([8, 9])
    );
    assert_eq!(
        id_set(&expected(&truths, 1, false, Table::Public)),
        BTreeSet::from([8])
    );
    let mut leaked = id_set(&expected(&truths, 0, true, Table::Public));
    leaked.insert(9);
    assert_ne!(leaked, id_set(&expected(&truths, 0, true, Table::Public)));
    // 双子比較は、片方にだけ他テナントの名前が混ざれば不一致になる。
    let fake_x: Resp = Err(("3F000".into(), "schema \"a\" does not exist".into()));
    let fake_y: Resp = Err((
        "3F000".into(),
        "schema \"a\" does not exist (zz_secret_c)".into(),
    ));
    assert_ne!(shape(&fake_x), shape(&fake_y));
}

// ---------------------------------------------------------------------------------------------
// T3b: Public 行がある共有表（Rust API 作成の従来方式の表）でも、修飾名・search_path で RLS が外れない
// ---------------------------------------------------------------------------------------------

/// SQL 表層で作る表は採番方式で Rust API から行を書けないため、Public 行の検査には Rust API で
/// 作った従来方式の表 `docs`（共有 public）を使う。3 テナント × 各 4 行（Public／Private 交互）。
fn seed_legacy_docs(path: &std::path::Path) -> Vec<Truth> {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("lang", ColumnType::Text, false)],
        ))
        .expect("create docs");
    let mut truths = Vec::new();
    for (i, tenant) in TENANTS.iter().enumerate() {
        for k in 0..4u64 {
            let id = 100 * (i as u64 + 1) + k;
            let vis = if k % 2 == 0 {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let op = OperationId::parse(&format!("seed-docs-{id}")).expect("op id");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx(tenant, true),
                id,
                vis,
                &[Value::Text("en".to_string())],
                &op,
            )
            .expect("insert docs row");
            truths.push(Truth {
                tenant: i,
                table: Table::Public,
                id,
                v: id,
                vis,
            });
        }
    }
    truths
}

#[test]
fn t3_public_rows_follow_the_oracle_through_every_reference_form() {
    let mut trials = 0usize;
    let path = unique_db_path("t27-tb-t3-legacy");
    let _guard = CleanupGuard(path.clone());
    let truths = seed_legacy_docs(&path);
    let core = open_core(&path);
    for (viewer, tenant) in TENANTS.iter().enumerate() {
        // 閲覧者自身の SCHEMA `s`（他テナントも同名の SCHEMA を持つ）。
        let mut s = session(tenant);
        ok(&core, tenant, &mut s, "CREATE SCHEMA s");
        ok(
            &core,
            tenant,
            &mut s,
            "CREATE TABLE s.docs (id BIGINT PRIMARY KEY, v INTEGER)",
        );
        ok(
            &core,
            tenant,
            &mut s,
            "INSERT INTO s.docs (id, v) VALUES (1, 1)",
        );
        let _ = viewer;
    }
    let mut public_seen = 0usize;
    for (viewer, tenant) in TENANTS.iter().enumerate() {
        for private in [false, true] {
            let want = id_set(&expected(&truths, viewer, private, Table::Public));
            let mut s = session(tenant);
            for sql in [
                "SELECT id FROM docs LIMIT 1000",
                "SELECT id FROM public.docs LIMIT 1000",
            ] {
                let got = query_ids(&ok_mode(&core, tenant, private, &mut s, sql));
                assert_eq!(got, want, "{tenant} private={private}: {sql}");
                public_seen += got.len();
                trials += 1;
            }
            // 他テナントの SCHEMA 名や自分の SCHEMA を path に置いても、集合は変わらない。
            for path_sql in [
                "SET search_path = s, public",
                "SET search_path = public, s",
                "SET search_path = tenant_other, public",
            ] {
                ok_mode(&core, tenant, private, &mut s, path_sql);
                let got = query_ids(&ok_mode(
                    &core,
                    tenant,
                    private,
                    &mut s,
                    "SELECT id FROM docs LIMIT 1000",
                ));
                assert_eq!(got, want, "{tenant} private={private}: {path_sql}");
                trials += 1;
            }
            // 修飾した s.docs は自分の SCHEMA の表で、public の docs の行は混ざらない。
            ok_mode(&core, tenant, private, &mut s, "RESET search_path");
            let sch = query_ids(&ok_mode(
                &core,
                tenant,
                private,
                &mut s,
                "SELECT id FROM s.docs LIMIT 1000",
            ));
            let sch_want: BTreeSet<u64> = if private {
                BTreeSet::from([1])
            } else {
                BTreeSet::new()
            };
            assert_eq!(sch, sch_want, "{tenant} private={private}: s.docs");
            trials += 1;
        }
    }
    // 陽性対照: Public 行が実際に他テナントへ見えている（空振りで green にならない）。
    assert!(public_seen > 0);
    assert!(truths.iter().any(|t| t.vis == Visibility::Public));
    assert_eq!(trials, 3 * 2 * (2 + 3 + 1), "trial count drifted");
}
