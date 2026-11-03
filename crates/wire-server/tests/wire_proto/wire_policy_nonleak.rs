//! pg wire（簡易・拡張クエリ・COPY）で、利用者ポリシー（RLS-13）が「テナント境界の内側で狭めるだけ」であり、
//! 他テナントの行の存在がメッセージ列から推知できないことの横断回帰（Issue #2281）。
//! ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・補助 RLS-12・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-17／WIRE-22。設計記録は
//! `docs/design/row-security-policy-composition.md`「非漏えい試験（#2281）」節。
//!
//! 既存の `wire_policy_copy.rs`（#2280）は COPY の違反系を個別に固定する。本ファイルはそれを書き直さず、
//! 双子サーバー方式を足す。他テナントが Private 行（観測側と同じ id を含む）・自前の RLS・ポリシーを
//! 置いたサーバー（X）と、置いていないサーバー（Y）へ、生バイトクライアントで同じ文の列を流し、
//! 各文が返す全メッセージ（`RowDescription`・`DataRow`・`CommandComplete`・`ErrorResponse`・
//! `CopyInResponse`・`CopyData` 等）がバイト一致することを固定する。期待される要点（タグ・SQLSTATE）は
//! テスト側の独立な表で与え、production の判定は呼ばない。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

/// 観測側の違反行にだけ入れる秘密マーカー。ErrorResponse に載ってはならない。
const SECRET: &str = "SECRETMARKER";

/// 他テナントの行にだけ入れる値。観測側のどのメッセージにも載ってはならない。
const FOREIGN_MARK: &str = "zz_secret_val";

type Msgs = Vec<(u8, Vec<u8>)>;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn sql_ok(core: &EngineCore, tenant: &str, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

/// alice の行（id 1 'a'・2 'b'・3 'a'）とポリシー（`owner = 'a'`）を先に作り（内部 ID を X/Y で揃える）、
/// `foreign` のときだけ bob の Private 行（id 1・2・100・101・200。観測側と同じ id を含む）・bob 自身の
/// RLS とポリシーを置く。接続は alice の認証済みストリーム。
fn connect(label: &str, foreign: bool) -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("owner", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    for (id, owner, n) in [(1, "a", 1), (2, "b", 2), (3, "a", 3)] {
        sql_ok(
            &core,
            TENANT_A,
            &format!("INSERT INTO t (id, owner, n) VALUES ({id}, '{owner}', {n})"),
        );
    }
    for sql in [
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE t FORCE ROW LEVEL SECURITY",
        "CREATE POLICY p ON t FOR ALL USING (owner = 'a') WITH CHECK (owner = 'a')",
    ] {
        sql_ok(&core, TENANT_A, sql);
    }
    if foreign {
        for id in [1u64, 2, 100, 101, 200] {
            sql_ok(
                &core,
                TENANT_B,
                &format!(
                    "INSERT INTO t (id, owner, n) VALUES ({id}, '{FOREIGN_MARK}', {})",
                    1000 + id
                ),
            );
        }
        sql_ok(&core, TENANT_B, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
        sql_ok(
            &core,
            TENANT_B,
            "CREATE POLICY zz_secret_pol ON t FOR ALL USING (true)",
        );
    }
    let users_path = write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        guard,
    )
}

/// ReadyForQuery（`Z`）までの全メッセージを集める（`Z` を含む）。
fn drain(s: &mut TcpStream) -> Msgs {
    let mut out = Vec::new();
    loop {
        let (ty, body) = read_message(s);
        let done = ty == b'Z';
        out.push((ty, body));
        if done {
            return out;
        }
    }
}

fn simple(s: &mut TcpStream, sql: &str) -> Msgs {
    send_simple_query(s, sql);
    drain(s)
}

/// 拡張クエリ（Parse／Bind／Execute／Sync。名前なし）。
fn extended(s: &mut TcpStream, sql: &str) -> Msgs {
    send_length_prefixed_message(s, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(s, b'B', &bind_body("", ""));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', b"");
    drain(s)
}

/// `COPY ... FROM STDIN`。CopyInResponse が来たら `payload` を送って CopyDone し、来なければ（失敗）そのまま集める。
fn copy_in(s: &mut TcpStream, sql: &str, payload: &[u8]) -> Msgs {
    send_simple_query(s, sql);
    let (ty, body) = read_message(s);
    let mut out = vec![(ty, body)];
    if ty != b'G' {
        out.extend(drain(s));
        return out;
    }
    send_length_prefixed_message(s, b'd', payload);
    send_length_prefixed_message(s, b'c', b"");
    out.extend(drain(s));
    out
}

fn text_of(msgs: &Msgs) -> String {
    msgs.iter()
        .map(|(t, b)| format!("{}:{}", *t as char, String::from_utf8_lossy(b)))
        .collect::<Vec<_>>()
        .join("|")
}

/// `id`（numeric）・`owner`（text）・`n`（int4）の binary タプルを組み立てる。
fn binary_stream(rows: &[(u16, &str, i32)]) -> Vec<u8> {
    let mut v = b"PGCOPY\n\xff\r\n\0".to_vec();
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    for (id, owner, n) in rows {
        v.extend_from_slice(&3i16.to_be_bytes());
        let mut num = Vec::new();
        for w in [1i16, 0, 0, 0, *id as i16] {
            num.extend_from_slice(&w.to_be_bytes());
        }
        for f in [num, owner.as_bytes().to_vec(), n.to_be_bytes().to_vec()] {
            v.extend_from_slice(&(f.len() as i32).to_be_bytes());
            v.extend_from_slice(&f);
        }
    }
    v.extend_from_slice(&(-1i16).to_be_bytes());
    v
}

enum Kind {
    Simple,
    Extended,
    CopyIn(Vec<u8>),
}

/// 1 文の実行と、観測側が期待する要点（`want` はメッセージ列の文字列表現に含まれるべき断片）。
struct Step {
    kind: Kind,
    sql: String,
    want: &'static str,
}

fn s(sql: &str, want: &'static str) -> Step {
    Step {
        kind: Kind::Simple,
        sql: sql.to_string(),
        want,
    }
}

fn e(sql: &str, want: &'static str) -> Step {
    Step {
        kind: Kind::Extended,
        sql: sql.to_string(),
        want,
    }
}

fn c(sql: &str, payload: &[u8], want: &'static str) -> Step {
    Step {
        kind: Kind::CopyIn(payload.to_vec()),
        sql: sql.to_string(),
        want,
    }
}

fn script() -> Vec<Step> {
    let bad_bin = binary_stream(&[(110, "a", 1), (111, SECRET, 2)]);
    let ok_bin = binary_stream(&[(120, "a", 1), (121, "a", 2)]);
    vec![
        // 読み取り（簡易・拡張）: 見える行は id 1・3 だけ。他テナントの id は存在しない id と同じ 0 件。
        s("SELECT id, owner, n FROM t ORDER BY id", "SELECT 2"),
        e("SELECT id, owner, n FROM t ORDER BY id", "SELECT 2"),
        s("SELECT COUNT(*) FROM t", "SELECT 1"),
        s("SELECT id FROM t WHERE id = 100", "SELECT 0"),
        s("SELECT id FROM t WHERE id = 2", "SELECT 0"),
        e("SELECT id FROM t WHERE id = 200", "SELECT 0"),
        s("EXPLAIN SELECT id FROM t", "QUERY PLAN"),
        // 書き込み: 他テナントにだけ在る id・隠れた自行は 0 件。他テナントと同じ id の INSERT は成功。
        s(
            "UPDATE t SET n = 9 WHERE id = 100 USING OPERATION_ID 'nl-1'",
            "UPDATE 0",
        ),
        s(
            "UPDATE t SET n = 9 WHERE id = 2 USING OPERATION_ID 'nl-2'",
            "UPDATE 0",
        ),
        s(
            "UPDATE t SET n = 9 WHERE id = 1 RETURNING id USING OPERATION_ID 'nl-3'",
            "UPDATE 1",
        ),
        s(
            "DELETE FROM t WHERE id = 200 USING OPERATION_ID 'nl-4'",
            "DELETE 0",
        ),
        s(
            "INSERT INTO t (id, owner, n) VALUES (100, 'a', 5) USING OPERATION_ID 'nl-5'",
            "INSERT 0 1",
        ),
        // WITH CHECK 違反は 42501、隠れた自行と同じ id は 23505、どちらも拡張クエリで同じ。
        s(
            "INSERT INTO t (id, owner, n) VALUES (60, 'SECRETMARKER', 5) USING OPERATION_ID 'nl-6'",
            "C42501",
        ),
        s(
            "INSERT INTO t (id, owner, n) VALUES (2, 'a', 5) USING OPERATION_ID 'nl-7'",
            "C23505",
        ),
        e(
            "INSERT INTO t (id, owner, n) VALUES (61, 'SECRETMARKER', 5) USING OPERATION_ID 'nl-8'",
            "C42501",
        ),
        // row_security = off は適用表で 42501（他テナントの行の有無に依存しない）。
        s("SET row_security = off", "SET"),
        s("SELECT id FROM t", "C42501"),
        s(
            "INSERT INTO t (id, owner, n) VALUES (62, 'a', 5) USING OPERATION_ID 'nl-9'",
            "C42501",
        ),
        s("SET row_security = on", "SET"),
        // COPY FROM: 他テナントと同じ id を含むバッチは成功、違反を含むバッチは全体が 42501。
        c(
            "COPY t (id, owner, n) FROM STDIN USING OPERATION_ID 'nl-c1'",
            b"101\ta\t5\n102\ta\t6\n",
            "COPY 2",
        ),
        c(
            "COPY t (id, owner, n) FROM STDIN USING OPERATION_ID 'nl-c2'",
            format!("103\ta\t1\n104\t{SECRET}\t1\n").as_bytes(),
            "C42501",
        ),
        c(
            "COPY t (id, owner, n) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'nl-c3'",
            format!("105,a,1\n106,{SECRET},2\n").as_bytes(),
            "C42501",
        ),
        c(
            "COPY t (id, owner, n) FROM STDIN WITH (FORMAT binary) USING OPERATION_ID 'nl-c4'",
            &ok_bin,
            "COPY 2",
        ),
        c(
            "COPY t (id, owner, n) FROM STDIN WITH (FORMAT binary) USING OPERATION_ID 'nl-c5'",
            &bad_bin,
            "C42501",
        ),
        // COPY TO: 表形・括弧形とも SELECT と同じ行集合。
        s("COPY t (id, owner, n) TO STDOUT", "COPY"),
        s("COPY (SELECT id FROM t ORDER BY id) TO STDOUT", "COPY"),
        s("SELECT id, owner, n FROM t ORDER BY id", "SELECT"),
    ]
}

fn run_step(st: &mut TcpStream, step: &Step) -> Msgs {
    match &step.kind {
        Kind::Simple => simple(st, &step.sql),
        Kind::Extended => extended(st, &step.sql),
        Kind::CopyIn(p) => copy_in(st, &step.sql, p),
    }
}

/// 双子サーバー（X: 他テナントの行あり・Y: なし）へ同じ文の列を流し、全メッセージがバイト一致する。
/// 各応答は期待の要点に従い、他テナントの値・テナント ID・違反行の秘密値を含まない。
#[test]
fn wire_messages_are_identical_with_and_without_foreign_tenant_rows() {
    let (mut sx, _gx) = connect("wire-nl-x", true);
    let (mut sy, _gy) = connect("wire-nl-y", false);
    let steps = script();
    let mut compared = 0usize;
    for (i, step) in steps.iter().enumerate() {
        let mx = run_step(&mut sx, step);
        let my = run_step(&mut sy, step);
        assert_eq!(
            mx,
            my,
            "step {i}: {}\nX={}\nY={}",
            step.sql,
            text_of(&mx),
            text_of(&my)
        );
        let text = text_of(&mx);
        assert!(
            text.contains(step.want),
            "step {i}: {}: expected {} in {text}",
            step.sql,
            step.want
        );
        assert!(!text.contains(FOREIGN_MARK), "step {i}: {text}");
        assert!(!text.contains(TENANT_B), "step {i}: {text}");
        if text.contains("C42501") || text.contains("C23505") {
            assert!(!text.contains(SECRET), "step {i}: {text}");
            assert!(!text.contains(TENANT_A), "step {i}: {text}");
        }
        // 各文の最後は ReadyForQuery（接続が同期を保っている）。
        assert_eq!(mx.last().map(|m| m.0), Some(b'Z'), "step {i}");
        compared += 1;
    }
    assert_eq!(compared, steps.len());
    assert!(compared >= 25, "script drifted: {compared}");
}

/// 陽性対照: 他テナントの行が在る X でも、観測側の SELECT・COPY TO が見る行は真実（id 1・3 と、
/// 自分で書いた行）だけ。両サーバーが同じ空振りの応答を返す可能性を排除する。
#[test]
fn visible_rows_match_oracle_with_foreign_rows_present() {
    let (mut sx, _g) = connect("wire-nl-oracle", true);
    let msgs = simple(&mut sx, "SELECT id FROM t ORDER BY id");
    let rows: Vec<String> = msgs
        .iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
        .collect();
    assert_eq!(rows.len(), 2, "{}", text_of(&msgs));
    assert!(rows[0].contains('1') && rows[1].contains('3'));
    let copy = simple(&mut sx, "COPY (SELECT id FROM t ORDER BY id) TO STDOUT");
    let data: Vec<&(u8, Vec<u8>)> = copy.iter().filter(|(t, _)| *t == b'd').collect();
    assert_eq!(data.len(), 2, "{}", text_of(&copy));
}
