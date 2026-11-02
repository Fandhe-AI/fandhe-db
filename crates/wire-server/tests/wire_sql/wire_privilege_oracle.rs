//! 権限エラー応答（ErrorResponse）が他テナントの存在オラクルにならないことの wire 越しの検証
//! （Issue #1972、ポインタ: RLS-12・SQL-40）。
//!
//! engine の in-process 行列は `crates/engine/tests/table_rls/rls12_privilege_existence_oracle.rs` が担う。
//! 本ファイルは、users ファイルのユーザーと同名の LOGIN ロールがテナント内にあるため接続が現在ロールへ
//! 束縛される本番どおりの経路（認証は cleartext。パスワードはダミー）で、ErrorResponse の本体を生で読み、
//! 次を固定する。
//!
//! - フィールド種別の集合が `{S, V, C, M}` 相当の最小集合で、`D`（Detail）・`H`（Hint）等を含まない
//! - 他テナントだけに在る名前 (C) の応答本体が、どこにも無い名前 (B) の双子 DB（別サーバー）と
//!   バイト一致する（表・列・ロール名）
//! - 権限ゲートがカタログ照会より前にある文（`DROP ROLE`・`ALTER ROLE`）は、自テナントに在る (A)・無い (B)・
//!   他テナントだけに在る (C) の 3 者で本体がバイト一致する
//!
//! サーバーはポート 0・一意な一時 DB で独立させる。

use crate::common;
use crate::temp_db;

use std::collections::BTreeSet;
use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

const OBSERVER_TENANT: &str = "tenant-a";
const OWNER_TENANT: &str = "tenant-b";

/// 観測ロール（users ファイルのユーザー `alice` と同名の LOGIN ロール）の ID（作成順）。
const ALICE: u64 = 1;
const FOREIGN_OWNER: u64 = 3;

#[derive(Clone, Copy)]
enum Placement {
    /// 他テナント（tenant-b）に秘密を置く（C）。
    Foreign,
    /// どこにも置かない（B）。
    Nowhere,
    /// 観測側テナント（tenant-a）に置く（A）。
    Own,
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn bound_session(id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if ddl {
        s.allow_ddl();
    }
    s
}

fn setup(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), s, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

/// サーバーを起動する。起動前に engine へ直接 SQL を流して、ロール・自前の表・秘密を作る。
fn serve(label: &str, placement: Placement) -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut op_a = SessionState::default();
    op_a.allow_ddl();
    setup(&core, OBSERVER_TENANT, &mut op_a, "CREATE ROLE alice LOGIN");
    setup(&core, OBSERVER_TENANT, &mut op_a, "CREATE ROLE peer LOGIN");
    let mut op_b = SessionState::default();
    op_b.allow_ddl();
    setup(&core, OWNER_TENANT, &mut op_b, "CREATE ROLE zz_owner");
    // alice 自身の表（所有者として読める。束縛が成立している陽性対照）と同テナントの別ロールの表。
    let mut alice = bound_session(ALICE, true);
    setup(
        &core,
        OBSERVER_TENANT,
        &mut alice,
        "CREATE TABLE mine (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    let mut peer = bound_session(2, true);
    setup(
        &core,
        OBSERVER_TENANT,
        &mut peer,
        "CREATE TABLE peer_t (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    let place = match placement {
        Placement::Foreign => Some((OWNER_TENANT, FOREIGN_OWNER)),
        Placement::Own => Some((OBSERVER_TENANT, 2)),
        Placement::Nowhere => None,
    };
    if let Some((tenant, owner_id)) = place {
        let mut o = bound_session(owner_id, true);
        setup(&core, tenant, &mut o, "CREATE SCHEMA zz_secret_s");
        setup(
            &core,
            tenant,
            &mut o,
            "CREATE TABLE zz_secret_s.zz_secret_t (id BIGINT PRIMARY KEY, zz_secret_c TEXT)",
        );
        let mut op = SessionState::default();
        op.allow_ddl();
        setup(&core, tenant, &mut op, "CREATE ROLE zz_secret_r");
    }
    let users_path = write_user_store_file(&[("alice", OBSERVER_TENANT, "pw-alice")]);
    let store =
        Arc::new(wire_server::auth::UserStore::load_from_file(&users_path).expect("user store"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let limiter = wire_server::limits::ConnectionLimiter::new(16);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            limiter,
            std::time::Duration::from_secs(5),
        );
    });
    (addr, guard)
}

/// 簡易クエリを送り、ErrorResponse の本体（フィールド列）を生で返す。続く ReadyForQuery も読む。
fn error_body(stream: &mut TcpStream, sql: &str) -> Vec<u8> {
    send_simple_query(stream, sql);
    let (ty, body) = read_message(stream);
    assert_eq!(ty, b'E', "{sql}: expected ErrorResponse");
    read_ready_for_query(stream);
    body
}

/// ErrorResponse 本体を (種別, 値) の列へ分解する。
fn fields(body: &[u8]) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some((&ty, tail)) = rest.split_first() {
        if ty == 0 {
            break;
        }
        let end = tail.iter().position(|&b| b == 0).expect("nul-terminated");
        out.push((ty, String::from_utf8_lossy(&tail[..end]).into_owned()));
        rest = &tail[end + 1..];
    }
    out
}

fn code_of(body: &[u8]) -> String {
    fields(body)
        .into_iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, v)| v)
        .expect("C field")
}

/// 応答にテナント ID・他テナントだけの名前が現れない（観測側が文に書いたものを除く）。
fn assert_no_leak(sql: &str, body: &[u8]) {
    for (_, v) in fields(body) {
        assert!(!v.contains("tenant-"), "{sql}: tenant id in {v:?}");
        assert!(!v.contains('#'), "{sql}: internal key marker in {v:?}");
        for w in [
            "zz_secret_s",
            "zz_secret_t",
            "zz_secret_c",
            "zz_secret_r",
            "zz_owner",
        ] {
            assert!(
                !v.contains(w) || sql.contains(w),
                "{sql}: unwritten name {w} in {v:?}"
            );
        }
    }
}

/// フィールド種別が最小集合（`D`・`H` 等の補足を含まない）であること。
fn assert_minimal_fields(sql: &str, body: &[u8]) {
    let kinds: BTreeSet<u8> = fields(body).into_iter().map(|(t, _)| t).collect();
    assert!(
        kinds.contains(&b'C') && kinds.contains(&b'M'),
        "{sql}: {kinds:?}"
    );
    for forbidden in [b'D', b'H', b't', b'c', b'd', b'n'] {
        assert!(
            !kinds.contains(&forbidden),
            "{sql}: unexpected field {} in {kinds:?}",
            forbidden as char
        );
    }
}

/// 双子サーバー（他テナントの秘密あり／なし）で、束縛した観測ロールの応答本体が一致する。
#[test]
fn wire_foreign_names_match_absent_names_for_a_bound_role() {
    let (ax, _gx) = serve("wire-oracle-x", Placement::Foreign);
    let (ay, _gy) = serve("wire-oracle-y", Placement::Nowhere);
    let mut x = authenticate_to_ready_for_query(ax, "alice", "pw-alice");
    let mut y = authenticate_to_ready_for_query(ay, "alice", "pw-alice");
    let mut trials = 0;
    // 束縛の成立: 同テナントの別ロールの表は権限が無く `42501`（未束縛なら読める）。
    for s in [&mut x, &mut y] {
        assert_eq!(code_of(&error_body(s, "SELECT id FROM peer_t")), "42501");
    }
    for (sql, code) in [
        ("SELECT id FROM zz_secret_s.zz_secret_t", "3F000"),
        ("SELECT zz_secret_c FROM zz_secret_s.zz_secret_t", "3F000"),
        (
            "INSERT INTO zz_secret_s.zz_secret_t (id) VALUES (1)",
            "3F000",
        ),
        ("GRANT SELECT ON mine TO zz_secret_r", "42704"),
        ("SET ROLE zz_secret_r", "42704"),
        ("ALTER TABLE mine OWNER TO zz_secret_r", "42704"),
    ] {
        for _ in 0..10 {
            let bx = error_body(&mut x, sql);
            let by = error_body(&mut y, sql);
            assert_eq!(bx, by, "{sql}");
            assert_eq!(code_of(&bx), code, "{sql}");
            assert_minimal_fields(sql, &bx);
            assert_no_leak(sql, &bx);
            trials += 1;
        }
    }
    assert_eq!(trials, 6 * 10, "trial count drifted");
}

/// ゲート前置の文は、自テナントに在る・無い・他テナントだけに在るの 3 者で本体がバイト一致する。
#[test]
fn wire_gate_first_statements_are_identical_for_all_three_cases() {
    let (a, _ga) = serve("wire-oracle-a", Placement::Own);
    let (b, _gb) = serve("wire-oracle-b", Placement::Nowhere);
    let (c, _gc) = serve("wire-oracle-c", Placement::Foreign);
    let mut streams =
        [a, b, c].map(|addr| authenticate_to_ready_for_query(addr, "alice", "pw-alice"));
    let mut trials = 0;
    for sql in ["DROP ROLE zz_secret_r", "ALTER ROLE zz_secret_r NOLOGIN"] {
        let bodies: Vec<Vec<u8>> = streams.iter_mut().map(|s| error_body(s, sql)).collect();
        assert!(bodies.windows(2).all(|w| w[0] == w[1]), "{sql}: {bodies:?}");
        assert_eq!(code_of(&bodies[0]), "42501", "{sql}");
        assert_minimal_fields(sql, &bodies[0]);
        trials += 1;
    }
    // SCHEMA 修飾の表 DDL は SCHEMA の名前解決が先に走るため、自テナントに在る (A) だけ区別が出る
    // （テナント内）。他テナント (C) とどこにも無い (B) は一致する。
    let sql = "DROP TABLE zz_secret_s.zz_secret_t";
    let bodies: Vec<Vec<u8>> = streams.iter_mut().map(|s| error_body(s, sql)).collect();
    assert_eq!(bodies[1], bodies[2], "{sql}");
    assert_minimal_fields(sql, &bodies[1]);
    trials += 1;
    assert_eq!(trials, 3, "trial count drifted");
}

/// 検査器自身への負の対照: 補足フィールド（`D`）や他テナントの名前を含む本体を検出できる。
#[test]
fn wire_leak_checkers_detect_fabricated_violations() {
    let mut body = Vec::new();
    for (t, v) in [(b'S', "ERROR"), (b'C', "42501"), (b'M', "denied")] {
        body.push(t);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    assert_minimal_fields("SELECT 1", &body);
    let mut with_detail = body[..body.len() - 1].to_vec();
    with_detail.push(b'D');
    with_detail.extend_from_slice(b"owned by tenant-b\0\0");
    assert!(std::panic::catch_unwind(|| assert_minimal_fields("SELECT 1", &with_detail)).is_err());
    assert!(std::panic::catch_unwind(|| assert_no_leak("SELECT 1", &with_detail)).is_err());
}
