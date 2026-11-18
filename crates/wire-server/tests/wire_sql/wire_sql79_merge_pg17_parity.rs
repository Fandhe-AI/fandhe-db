//! `MERGE` の PostgreSQL 17 突き合わせの代表ケースを pg wire v3 越しに固定する層 A の結合テスト
//! （SQL-79・WIRE-15・Issue #3358。親 #3349、設計記録 `docs/design/sql-merge.md` §14）。
//!
//! 期待値は engine 側の `crates/engine/tests/data/merge_when_pg17.tsv`（PostgreSQL 17.12〔Debian 17.12-1.pgdg13+1〕
//! の実測。ケース id は各ケースのコメントに付ける）と同じ値。wire-server から engine の TSV を
//! `include_str!` しない（クレートをまたぐ参照を避ける）ため、代表ケースを直書きしている。
//! ParameterDescription の OID は PG17 の `describe_prepared` 実測（`INTEGER` 列へ束縛した `$n` は
//! すべて int4〔23〕、`merge_action()` は text〔25〕）。
//!
//! 送り方は次の 3 通り（すべて明示 Tx の BEGIN → 対象文 → ROLLBACK で流し、状態を戻す）:
//! 簡易クエリの末尾 `;` なし、同じく末尾 `;` あり（psql 流の単独送信）、拡張クエリ（Parse／Bind／Execute／Sync）。
//! 3 テナント（alice／bob／carol）が同じ世界を持ち、同じ MERGE が同じ結果になること、拒否応答の
//! 本文がテナントによらずバイト一致し、テナント id・他テナントの値を含まないことも固定する。
//! サーバーはポート 0・一意な一時ディレクトリで独立させ、時間計測は使わない。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

const TENANTS: [(&str, &str); 3] = [
    ("alice", "tenant-a"),
    ("bob", "tenant-b"),
    ("carol", "tenant-c"),
];
const PASSWORD: &str = "correct-horse";

fn start(dup: bool) -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge-pg17");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    let ctx_a = ctx("tenant-a");
    for sql in [
        "CREATE TABLE t (k INTEGER, x INTEGER)",
        "CREATE TABLE s (k INTEGER, v INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx_a, &mut ddl, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (_, tenant) in TENANTS {
        let c = ctx(tenant);
        let mut seed = vec![
            "INSERT INTO t (k, x) VALUES (1,10),(2,20),(3,30),(5,50),(NULL,60)",
            "INSERT INTO s (k, v) VALUES (1,100),(2,200),(4,400),(6,600),(NULL,700)",
        ];
        if dup {
            seed.push("INSERT INTO s (k, v) VALUES (2,250)");
        }
        for sql in seed {
            core.execute_sql_in_session(&c, &mut SessionState::default(), sql)
                .unwrap_or_else(|e| panic!("{tenant}: {sql}: {e}"));
        }
    }
    let users: Vec<(&str, &str, &str)> = TENANTS.iter().map(|(u, t)| (*u, *t, PASSWORD)).collect();
    let users_path = write_user_store_file(&users);
    let addr = spawn_server_with_engine(&users_path, core);
    (addr, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("tenant")
}

fn client(addr: SocketAddr, user: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, user, PASSWORD)
}

/// 1 往復（`Z` まで）の観測。
#[derive(Debug, Default, PartialEq)]
struct Round {
    columns: Vec<String>,
    rows: Vec<String>,
    param_oids: Vec<u32>,
    tag: Option<String>,
    code: Option<String>,
    message: Option<String>,
    status: u8,
}

fn read_round(s: &mut TcpStream) -> Round {
    let mut r = Round::default();
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'T' => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                let mut at = 2;
                for _ in 0..n {
                    let end = body[at..].iter().position(|b| *b == 0).expect("name end");
                    r.columns
                        .push(String::from_utf8_lossy(&body[at..at + end]).to_string());
                    at += end + 1 + 18;
                }
            }
            b'D' => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                let mut at = 2;
                let mut cells = Vec::new();
                for _ in 0..n {
                    let len =
                        i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                    at += 4;
                    if len < 0 {
                        cells.push("NULL".to_string());
                    } else {
                        let len = usize::try_from(len).expect("len");
                        cells.push(String::from_utf8_lossy(&body[at..at + len]).to_string());
                        at += len;
                    }
                }
                r.rows.push(cells.join(","));
            }
            b't' => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                for i in 0..n {
                    let at = 2 + i * 4;
                    r.param_oids.push(u32::from_be_bytes([
                        body[at],
                        body[at + 1],
                        body[at + 2],
                        body[at + 3],
                    ]));
                }
            }
            b'C' => {
                r.tag = Some(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                )
            }
            b'E' => {
                let text = String::from_utf8_lossy(&body).to_string();
                for f in text.split('\0') {
                    if let Some(c) = f.strip_prefix('C') {
                        r.code = Some(c.to_string());
                    } else if let Some(m) = f.strip_prefix('M') {
                        r.message = Some(m.to_string());
                    }
                }
            }
            b'Z' => {
                r.status = body.first().copied().unwrap_or(0);
                r.rows.sort();
                return r;
            }
            _ => {}
        }
    }
}

fn simple(s: &mut TcpStream, sql: &str) -> Round {
    send_simple_query(s, sql);
    read_round(s)
}

fn bind_with_text_params(values: &[&str]) -> Vec<u8> {
    let mut body = vec![0u8, 0u8]; // portal "" と statement ""
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&i16::try_from(values.len()).expect("count").to_be_bytes());
    for v in values {
        body.extend_from_slice(&i32::try_from(v.len()).expect("len").to_be_bytes());
        body.extend_from_slice(v.as_bytes());
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn extended(s: &mut TcpStream, sql: &str, params: &[&str]) -> Round {
    send_length_prefixed_message(s, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(s, b'B', &bind_with_text_params(params));
    // Describe（ポータル）。RETURNING の列を RowDescription として観測する。
    send_length_prefixed_message(s, b'D', b"P\0");
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', &[]);
    read_round(s)
}

#[derive(Clone, Copy)]
enum Mode {
    Simple,
    SimpleSemicolon,
    Extended,
}

/// 明示 Tx（BEGIN → 対象文 → ROLLBACK）で流し、対象文の観測と ROLLBACK 後の `t` の内容を返す。
fn run_case(s: &mut TcpStream, sql: &str, mode: Mode) -> (Round, String) {
    assert_eq!(simple(s, "BEGIN").tag.as_deref(), Some("BEGIN"));
    let r = match mode {
        Mode::Simple => simple(s, sql),
        Mode::SimpleSemicolon => simple(s, &format!("{sql};")),
        Mode::Extended => extended(s, sql, &[]),
    };
    // 成功なら `T`、失敗なら `E`（拡張クエリは Sync でトランザクションの状態を持ち越す）。
    let want = if r.code.is_some() { b'E' } else { b'T' };
    assert_eq!(r.status, want, "{sql}: {r:?}");
    assert_eq!(simple(s, "ROLLBACK").tag.as_deref(), Some("ROLLBACK"));
    let after = simple(s, "SELECT k, x FROM t");
    (r, after.rows.join(";"))
}

const BASE_T: &str = "1,10;2,20;3,30;5,50;NULL,60";
const ON: &str = "MERGE INTO t USING s ON t.k = s.k";

// merge_when_pg17.tsv base-0547 / base-0548（長さ 3。件数 8・RETURNING の行と列名）。
#[test]
fn three_clause_merge_tag_and_returning_match_pg17_for_every_tenant_and_mode() {
    let (addr, _g) = start(false);
    let plain = format!(
        "{ON} WHEN MATCHED THEN UPDATE SET x = s.v \
         WHEN NOT MATCHED THEN INSERT (k, x) VALUES (s.k, s.v) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE"
    );
    let returning = format!("{plain} RETURNING merge_action(), t.k, t.x, s.k, s.v");
    let want_rows = [
        "DELETE,3,30,NULL,NULL",
        "DELETE,5,50,NULL,NULL",
        "DELETE,NULL,60,NULL,NULL",
        "INSERT,4,400,4,400",
        "INSERT,6,600,6,600",
        "INSERT,NULL,700,NULL,700",
        "UPDATE,1,100,1,100",
        "UPDATE,2,200,2,200",
    ];
    for (user, _) in TENANTS {
        let mut s = client(addr, user);
        for mode in [Mode::Simple, Mode::SimpleSemicolon, Mode::Extended] {
            let (r, after) = run_case(&mut s, &plain, mode);
            assert_eq!(r.code, None, "{user}: {r:?}");
            assert_eq!(r.tag.as_deref(), Some("MERGE 8"), "{user}");
            assert_eq!(after, BASE_T, "{user}: ROLLBACK restores the table");
            let (r, _) = run_case(&mut s, &returning, mode);
            assert_eq!(r.tag.as_deref(), Some("MERGE 8"), "{user}: {r:?}");
            assert_eq!(r.columns, ["merge_action", "k", "x", "k", "v"], "{user}");
            assert_eq!(r.rows, want_rows, "{user}");
        }
    }
}

// base-0033（条件なしの MATCHED の後の同種の句は 42601）・d4（反対側の参照は 42P01、反対側にしか無い非修飾の列は
// 42703）・merge_action() の外での使用（42601）。拒否応答の本文はテナントによらずバイト一致し、
// テナント id・他テナントの値を含まない。
#[test]
fn rejections_match_pg17_sqlstates_and_are_byte_identical_across_tenants() {
    let (addr, _g) = start(false);
    let cases = [
        (
            format!("{ON} WHEN MATCHED THEN DELETE WHEN MATCHED THEN UPDATE SET x = s.v"),
            "42601",
        ),
        (
            format!("{ON} WHEN NOT MATCHED AND t.x > 1 THEN DO NOTHING"),
            "42P01",
        ),
        (
            format!("{ON} WHEN NOT MATCHED BY SOURCE AND s.v > 1 THEN DELETE"),
            "42P01",
        ),
        (
            format!("{ON} WHEN NOT MATCHED AND x > 1 THEN DO NOTHING"),
            "42703",
        ),
        ("SELECT merge_action() FROM t".to_string(), "42601"),
    ];
    for (sql, code) in &cases {
        for mode in [Mode::Simple, Mode::SimpleSemicolon, Mode::Extended] {
            let mut messages = Vec::new();
            for (user, tenant) in TENANTS {
                let mut s = client(addr, user);
                let (r, after) = run_case(&mut s, sql, mode);
                assert_eq!(r.code.as_deref(), Some(*code), "{user}: {sql}");
                assert_eq!(after, BASE_T, "{user}: {sql}");
                let msg = r.message.clone().unwrap_or_default();
                for leak in [user, tenant] {
                    assert!(!msg.contains(leak), "{user}: leaked {leak} in {msg}");
                }
                messages.push(msg);
            }
            assert!(
                messages.windows(2).all(|w| w[0] == w[1]),
                "{sql}: messages differ across tenants: {messages:?}"
            );
        }
    }
}

// dup-0273・dup-0346（同じ対象行が 2 つ以上の元に一致すると 21000。文言も PG17 と同じ）。
// 3 テナントとも同じ元の重複を持つが、互いの重複は影響しない。
#[test]
fn multiple_match_is_21000_with_the_pg17_message() {
    let (addr, _g) = start(true);
    let sqls = [
        format!("{ON} WHEN MATCHED THEN UPDATE SET x = s.v"),
        format!(
            "{ON} WHEN MATCHED AND s.v > 150 THEN UPDATE SET x = s.v \
             WHEN NOT MATCHED AND s.v > 450 THEN DO NOTHING"
        ),
    ];
    for sql in &sqls {
        for mode in [Mode::Simple, Mode::SimpleSemicolon, Mode::Extended] {
            for (user, _) in TENANTS {
                let mut s = client(addr, user);
                let (r, after) = run_case(&mut s, sql, mode);
                assert_eq!(r.code.as_deref(), Some("21000"), "{user}: {sql}");
                assert_eq!(
                    r.message.as_deref(),
                    Some("MERGE command cannot affect row a second time"),
                    "{user}"
                );
                assert_eq!(after, BASE_T, "{user}");
            }
        }
    }
    // dup-0515: 重複があっても、一致した対象行に作用する句が無ければ 21000 にならない。
    let mut s = client(addr, "alice");
    let (r, after) = run_case(
        &mut s,
        &format!(
            "{ON} WHEN NOT MATCHED BY SOURCE AND t.x > 25 THEN DELETE WHEN MATCHED THEN DO NOTHING"
        ),
        Mode::Simple,
    );
    assert_eq!(r.tag.as_deref(), Some("MERGE 3"), "{r:?}");
    assert_eq!(after, BASE_T);
}

// 簡易クエリ・拡張クエリとも、autocommit の MERGE は ReadyForQuery `I` で終わり、3 テナントが同じ文を流すと
// それぞれ自分の行だけが変わる（件数は PG17 の base-0074 と同じ `MERGE 1`）。
#[test]
fn autocommit_merge_changes_only_the_senders_rows() {
    let (addr, _g) = start(false);
    let sql = format!(
        "{ON} WHEN MATCHED AND s.v > 150 THEN UPDATE SET x = s.v \
         WHEN NOT MATCHED AND s.v > 450 THEN DO NOTHING"
    );
    let mut alice = client(addr, "alice");
    let mut bob = client(addr, "bob");
    let mut carol = client(addr, "carol");
    let r = simple(&mut alice, &format!("{sql};"));
    assert_eq!(r.tag.as_deref(), Some("MERGE 1"), "{r:?}");
    assert_eq!(r.status, b'I');
    assert_eq!(
        simple(&mut bob, "SELECT k, x FROM t").rows.join(";"),
        BASE_T
    );
    assert_eq!(
        simple(&mut carol, "SELECT k, x FROM t").rows.join(";"),
        BASE_T
    );
    assert_eq!(
        simple(&mut alice, "SELECT k, x FROM t").rows.join(";"),
        "1,10;2,200;3,30;5,50;NULL,60"
    );
    let r = extended(&mut bob, &sql, &[]);
    assert_eq!(r.tag.as_deref(), Some("MERGE 1"), "{r:?}");
    assert_eq!(r.status, b'I');
    assert_eq!(
        simple(&mut carol, "SELECT k, x FROM t").rows.join(";"),
        BASE_T
    );
}

// ParameterDescription: INTEGER 列へ束縛した値位置の `$n` は PG17 では int4（23）。条件内の `$n`
// （PG17 は int4 で受理）は本実装では未対応で、`42601` の fail-closed 拒否（設計記録 §14 の未解消の差）。
#[test]
fn parameter_description_oids_match_pg17() {
    let (addr, _g) = start(false);
    let mut s = client(addr, "alice");
    for (n, (sql, want)) in [
        (
            format!("{ON} WHEN MATCHED THEN UPDATE SET x = $1"),
            vec![23u32],
        ),
        (
            format!(
                "{ON} WHEN MATCHED THEN UPDATE SET x = $1 WHEN NOT MATCHED THEN INSERT (x, k) VALUES ($2, $3)"
            ),
            vec![23, 23, 23],
        ),
        (
            format!("{ON} WHEN NOT MATCHED THEN INSERT (k, x) VALUES ($1, $2)"),
            vec![23, 23],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("d{n}");
        send_length_prefixed_message(&mut s, b'P', &parse_body(&name, &sql, 0));
        send_length_prefixed_message(&mut s, b'D', format!("S{name}\0").as_bytes());
        send_length_prefixed_message(&mut s, b'S', &[]);
        let r = read_round(&mut s);
        assert_eq!(r.code, None, "{sql}: {r:?}");
        assert_eq!(r.param_oids, want, "{sql}");
    }
    for sql in [
        format!("{ON} WHEN MATCHED AND s.v > $1 THEN UPDATE SET x = $2"),
        format!("{ON} WHEN NOT MATCHED BY SOURCE AND t.x > $1 THEN DELETE"),
    ] {
        send_length_prefixed_message(&mut s, b'P', &parse_body("cc", &sql, 0));
        send_length_prefixed_message(&mut s, b'D', b"Scc\0");
        send_length_prefixed_message(&mut s, b'S', &[]);
        let r = read_round(&mut s);
        assert_eq!(r.code.as_deref(), Some("42601"), "{sql}: {r:?}");
        assert!(r.param_oids.is_empty(), "{sql}");
    }
}
