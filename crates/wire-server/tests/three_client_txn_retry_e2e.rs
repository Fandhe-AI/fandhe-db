//! Issue #2412（対象ビヘイビア: SQL-31・SQL-19・ERR-7、TASK-335、親 #2408）の層 B:
//! 無改造の実クライアント 3 種（psql／Python `psycopg`／Node.js `pg`）から `wire-server`
//! バイナリへ実接続し、**明示トランザクション内の述語形書き込み**と **40001 の再試行**を
//! 簡易クエリ・拡張クエリの両方で検証する統合テスト。
//!
//! 責務境界: writer（単一ライタ）の遅延取得・取得時の古さ判定（40001）・取得後の述語評価
//! （最新の確定状態）そのものは、engine の `sql31_*` 系と層 A の
//! `tests/wire19_deferred_writer_status.rs`（生バイトのクライアント。常時 `make ci`）が
//! 確定オラクルとして検証済みである。本ファイルは同じ挙動が無改造クライアントの API
//! 越しにも観測できることの代表ケース確認に徹する（`docs/design/explicit-transaction.md`
//! 1a、`docs/design/three-client-e2e-harness.md` の本 Issue の節を参照）。
//!
//! シナリオのモデル: 「主セッション（シナリオ全体で 1 接続を開いたまま）＋主セッションの
//! 文の合間に新しい接続で実行して閉じる他セッション群」。他セッション群は主セッションの
//! 次の文より前に完了するので全体が逐次で決定的になり、スレッド・sleep・時間の閾値は使わ
//! ない（`55P03` を意図して観測する S2c だけは `--write-lock-wait-ms 100`＝下限値の待機が
//! 加わるが、保持側は解放しないので結果は決定的）。他セッションも**同じクライアント**で
//! 駆動する（層 B の純度のため）。
//! - psycopg／pg: `three_client/psycopg_txn_steps.py`・`pg_txn_steps.js` が 1 プロセス内で
//!   両セッションを開く。
//! - psql: 一時ディレクトリへ主スクリプトと群ごとのスクリプトを生成し、主スクリプトの
//!   `\!` から群を同期実行する。
//!
//! ツール未検出・クライアントの想定外終了・期待外の SQLSTATE・出力マーカーの欠落／重複は
//! `panic!` で失敗させ silent skip はしない。ローカル・Docker 開発コンテナには実クライ
//! アントが無いため本体テストは `#[ignore]` とし `make e2e-three-client` から実行する
//! （シナリオ定義・解析器・リテラル展開のガードのみ常時実行）。psql は `\bind`
//! （PostgreSQL 16 以降）を要する。

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

// --- 共通ヘルパー（three_client_extended_e2e.rs と同型の簡略版） -------------------

fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `wire-server` 子プロセスのハンドル。Drop で kill する。
struct ServerGuard {
    child: Child,
    port: u16,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 書き込みロック待機の上限（ms。`--write-lock-wait-ms` の下限値）。S2c の `55P03` を
/// 短時間で確定させるために使う。他のシナリオは待機に依存しない。
const WRITE_LOCK_WAIT_MS: &str = "100";

/// listen 行からポートを得る。stderr は EOF まで読み続ける（読み手が閉じるとサーバー側の
/// 診断出力が失敗しうるため）。
fn spawn_wire_server(users_path: &Path, db_path: &Path) -> ServerGuard {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path.to_str().expect("utf-8 path"),
            "--db",
            db_path.to_str().expect("utf-8 path"),
            "--bind",
            "127.0.0.1:0",
            "--write-lock-wait-ms",
            WRITE_LOCK_WAIT_MS,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server binary (built by `cargo test`)");

    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let _ = tx.send(std::mem::take(&mut line));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut port: Option<u16> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(addr) = line.trim().strip_prefix("wire-server: listening on ") {
                    if let Ok(addr) = addr.parse::<std::net::SocketAddr>() {
                        port = Some(addr.port());
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("wire-server did not report a listening port within the deadline");
    };
    ServerGuard { child, port }
}

/// ダミー資格情報（`pw-<user>`）。本ファイルの 2 ユーザーは別テナントに属する。
const USERS: [(&str, &str, &str); 2] = [
    ("alice", "tenant-a", "pw-alice"),
    ("bob", "tenant-b", "pw-bob"),
];

fn write_users_file(path: &Path) {
    use wire_server::auth::argon2id;
    let salt = b"0123456789abcdef";
    let mut content = String::new();
    for (user, tenant, pw) in USERS {
        let phc = argon2id::encode_phc(pw.as_bytes(), salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{user}:{tenant}:{phc}\n"));
    }
    std::fs::write(path, content).expect("write users file");
}

fn password_of(user: &str) -> &'static str {
    USERS
        .iter()
        .find(|(u, _, _)| *u == user)
        .map(|(_, _, pw)| *pw)
        .unwrap_or_else(|| panic!("unknown test user"))
}

fn op(s: &str) -> OperationId {
    OperationId::parse(s).expect("valid operation_id")
}

// --- seed ---------------------------------------------------------------------

/// seed の 1 行: (行 id, テナント, 鍵 `k`, 初期値 `bal`)。SQL の INSERT は常に Private で
/// 書かれるため、seed も Private で投入する。bob の鍵 `b` は alice の `b` と同じ鍵で、
/// alice の述語が他テナントの同じ鍵の行に触れないことの確認に使う（S2b の最終確認）。
const SEED_ROWS: [(u64, &str, &str, i64); 10] = [
    (1, "tenant-a", "p", 15),
    (2, "tenant-a", "q", 1),
    (3, "tenant-a", "a", 15),
    (4, "tenant-a", "b", 15),
    (5, "tenant-a", "c", 15),
    (6, "tenant-a", "y", 15),
    (7, "tenant-a", "t", 15),
    (101, "tenant-b", "b", 100),
    (102, "tenant-b", "x", 100),
    (103, "tenant-b", "t", 100),
];

/// 一時 DB に `acct (k TEXT, bal BIGINT)` と [`SEED_ROWS`] を作る。サーバー起動前に
/// `Storage` を drop してファイルを解放する。
fn seed_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-txn-retry-e2e");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "acct",
            vec![
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("bal", ColumnType::BigInt, false),
            ],
        ))
        .expect("create acct");
    for (id, tenant, k, bal) in SEED_ROWS {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "acct",
            &ctx,
            id,
            Visibility::Private,
            &[Value::Text(k.to_string()), Value::BigInt(bal)],
            &op(&format!("seed-acct-{id}")),
        )
        .expect("insert acct row");
    }
    drop(storage);
    (path, guard)
}

// --- シナリオ定義 ---------------------------------------------------------------

const BEGIN: &str = "BEGIN";
const COMMIT: &str = "COMMIT";
const ROLLBACK: &str = "ROLLBACK";
const SEL_BAL: &str = "SELECT bal FROM acct WHERE k = $1 LIMIT 10";
const SEL_KB: &str = "SELECT k, bal FROM acct WHERE k = $1 LIMIT 10";
/// 共通の述語形書き込み（条件付きの減算）。
const UPD: &str = "UPDATE acct SET bal = bal - 10 WHERE k = $1 AND bal >= 10 USING OPERATION_ID $2";
const DEL: &str = "DELETE FROM acct WHERE k = $1 AND bal < 10 USING OPERATION_ID $2";
const INC: &str = "UPDATE acct SET bal = bal + 1 WHERE k = $1 USING OPERATION_ID $2";

/// 文の実行者。主セッションは alice 固定。他セッション群は (群番号, ユーザー)。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sess {
    Main,
    Other(u32, &'static str),
}

/// 期待する結果。`Err` は SQLSTATE。
#[derive(Clone, Debug)]
enum Expect {
    Tag(&'static str),
    Rows(&'static [&'static str]),
    Err(&'static str),
}

#[derive(Clone, Debug)]
struct Step {
    scenario: &'static str,
    sess: Sess,
    sql: &'static str,
    params: Vec<String>,
    expect: Expect,
}

fn st(
    scenario: &'static str,
    sess: Sess,
    sql: &'static str,
    params: &[&str],
    expect: Expect,
) -> Step {
    Step {
        scenario,
        sess,
        sql,
        params: params.iter().map(|s| s.to_string()).collect(),
        expect,
    }
}

/// S1〜S4 の全ステップ（通し番号がそのまま出力マーカーの idx になる）。
fn scenarios() -> Vec<Step> {
    use Expect::{Err, Rows, Tag};
    let m = Sess::Main;
    let mut v = Vec::new();

    // S1: 述語形書き込みの rollback と commit（主セッションだけ）。同じ operation_id を
    // 2 周で使い、rollback 側が台帳へ何も残さないこと（commit 側が 23505 にならない）も示す。
    let s = "S1";
    for (end, end_tag, p_after, q_after) in [
        (ROLLBACK, "ROLLBACK", "p|15", Some("q|1")),
        (COMMIT, "COMMIT", "p|5", None),
    ] {
        v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
        v.push(st(s, m, UPD, &["p", "s1-upd"], Tag("UPDATE 1")));
        v.push(st(s, m, DEL, &["q", "s1-del"], Tag("DELETE 1")));
        v.push(st(s, m, end, &[], Tag(end_tag)));
        v.push(st(
            s,
            m,
            SEL_KB,
            &["p"],
            Rows(if p_after == "p|15" {
                &["p|15"]
            } else {
                &["p|5"]
            }),
        ));
        v.push(st(
            s,
            m,
            SEL_KB,
            &["q"],
            Rows(if q_after.is_some() { &["q|1"] } else { &[] }),
        ));
    }

    // S2a: 並行する条件付き更新（両方とも読まずに書く）。主の読み取り集合が空なので 40001 に
    // はならず、述語は取得時点の最新状態で評価されて UPDATE 0 になる（二重に引かれない）。
    let s = "S2a";
    let g = Sess::Other(1, "alice");
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, g, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, g, UPD, &["a", "s2a-g"], Tag("UPDATE 1")));
    v.push(st(s, g, COMMIT, &[], Tag("COMMIT")));
    v.push(st(s, m, UPD, &["a", "s2a-m"], Tag("UPDATE 0")));
    v.push(st(s, m, COMMIT, &[], Tag("COMMIT")));
    v.push(st(s, m, SEL_KB, &["a"], Rows(&["a|5"])));

    // S2b: 並行する条件付き更新（両方とも読んでから書く）。主は 40001 で弾かれ、
    // 25P02 → ROLLBACK → 再試行（最新状態で UPDATE 0）の順に回復する。
    let s = "S2b";
    let g = Sess::Other(2, "alice");
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, m, SEL_BAL, &["b"], Rows(&["15"])));
    v.push(st(s, g, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, g, SEL_BAL, &["b"], Rows(&["15"])));
    v.push(st(s, g, UPD, &["b", "s2b-g"], Tag("UPDATE 1")));
    v.push(st(s, g, COMMIT, &[], Tag("COMMIT")));
    v.push(st(s, m, UPD, &["b", "s2b-m"], Err("40001")));
    v.push(st(s, m, SEL_BAL, &["b"], Err("25P02")));
    v.push(st(s, m, ROLLBACK, &[], Tag("ROLLBACK")));
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, m, SEL_BAL, &["b"], Rows(&["5"])));
    v.push(st(s, m, UPD, &["b", "s2b-m"], Tag("UPDATE 0")));
    v.push(st(s, m, COMMIT, &[], Tag("COMMIT")));
    // bob の同じ鍵の行は alice の述語の影響を受けない（別テナントの bob 自身の接続で確認）。
    let b = Sess::Other(3, "bob");
    v.push(st(s, b, SEL_BAL, &["b"], Rows(&["100"])));

    // S2c: 相手が writer を保持中。他セッションの UPDATE は待機上限で 55P03 になり、
    // 保持側の COMMIT 後の再試行は最新状態で UPDATE 0 になる。
    let s = "S2c";
    let g = Sess::Other(4, "alice");
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, m, UPD, &["c", "s2c-m"], Tag("UPDATE 1")));
    v.push(st(s, g, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, g, UPD, &["c", "s2c-g"], Err("55P03")));
    v.push(st(s, g, ROLLBACK, &[], Tag("ROLLBACK")));
    v.push(st(s, m, COMMIT, &[], Tag("COMMIT")));
    let g = Sess::Other(5, "alice");
    v.push(st(s, g, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, g, UPD, &["c", "s2c-g"], Tag("UPDATE 0")));
    v.push(st(s, g, COMMIT, &[], Tag("COMMIT")));
    v.push(st(s, m, SEL_KB, &["c"], Rows(&["c|5"])));

    // S3: 読んでから書く形の 40001 と、同じ operation_id での再試行。
    let s = "S3";
    let g = Sess::Other(6, "alice");
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, m, SEL_BAL, &["y"], Rows(&["15"])));
    v.push(st(s, g, INC, &["y", "s3-inc"], Tag("UPDATE 1")));
    v.push(st(s, m, UPD, &["y", "s3-retry"], Err("40001")));
    v.push(st(s, m, SEL_BAL, &["y"], Err("25P02")));
    v.push(st(s, m, ROLLBACK, &[], Tag("ROLLBACK")));
    v.push(st(s, m, SEL_BAL, &["y"], Rows(&["16"])));
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, m, SEL_BAL, &["y"], Rows(&["16"])));
    v.push(st(s, m, UPD, &["y", "s3-retry"], Tag("UPDATE 1")));
    v.push(st(s, m, COMMIT, &[], Tag("COMMIT")));
    v.push(st(s, m, SEL_BAL, &["y"], Rows(&["6"])));

    // S4: テナント境界の陰性の対照。他テナント（bob）の Private 行だけの commit では
    // 40001 にならない。
    let s = "S4";
    let g = Sess::Other(7, "bob");
    v.push(st(s, m, BEGIN, &[], Tag("BEGIN")));
    v.push(st(s, m, SEL_BAL, &["t"], Rows(&["15"])));
    v.push(st(s, g, INC, &["t", "s4-bob"], Tag("UPDATE 1")));
    v.push(st(s, m, UPD, &["t", "s4-m"], Tag("UPDATE 1")));
    v.push(st(s, m, COMMIT, &[], Tag("COMMIT")));
    v.push(st(s, m, SEL_BAL, &["t"], Rows(&["5"])));
    // bob の t は自分の INC の結果（101）のまま。alice の UPDATE の影響は無い。
    let g = Sess::Other(8, "bob");
    v.push(st(s, g, SEL_BAL, &["t"], Rows(&["101"])));
    v
}

// --- 簡易モードのリテラル展開 ----------------------------------------------------

/// 簡易クエリ用に `$n` を単引用符リテラルへ展開する。パラメータはファイル内の定数だけで、
/// `[a-z0-9-]` に収まること（インジェクションの余地が無いこと）を検証して拒否する。
fn expand_literals(sql: &str, params: &[String]) -> String {
    let mut out = sql.to_string();
    for (i, p) in params.iter().enumerate().rev() {
        assert!(
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "test parameter outside [a-z0-9-]"
        );
        out = out.replace(&format!("${}", i + 1), &format!("'{p}'"));
    }
    assert!(!out.contains('$'), "unexpanded placeholder");
    out
}

// --- JSON（最小エンコーダ。依存を追加しない） -----------------------------------------

fn json_str(s: &str) -> String {
    assert!(
        !s.contains('\\') && !s.chars().any(|c| c.is_control()),
        "unsupported character in JSON string"
    );
    format!("\"{}\"", s.replace('"', "\\\""))
}

fn session_key(sess: Sess) -> String {
    match sess {
        Sess::Main => "main".to_string(),
        Sess::Other(n, _) => format!("g{n}"),
    }
}

fn session_user(sess: Sess) -> &'static str {
    match sess {
        Sess::Main => "alice",
        Sess::Other(_, u) => u,
    }
}

/// 後続に同じ他セッション群の文が無いとき、その文の実行後に接続を閉じる。
fn closes_after(steps: &[Step], i: usize) -> bool {
    match steps[i].sess {
        Sess::Main => false,
        s => !steps[i + 1..].iter().any(|n| n.sess == s),
    }
}

/// モードに応じた送信 SQL とパラメータ（簡易はリテラル展開済みで params 空）。
fn wire_form(step: &Step, extended: bool) -> (String, Vec<String>) {
    if extended {
        (step.sql.to_string(), step.params.clone())
    } else {
        (expand_literals(step.sql, &step.params), Vec::new())
    }
}

fn steps_json(steps: &[Step], extended: bool) -> String {
    let mut out = String::from("[");
    for (i, step) in steps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (sql, params) = wire_form(step, extended);
        let ps: Vec<String> = params.iter().map(|p| json_str(p)).collect();
        out.push_str(&format!(
            "{{\"s\":{},\"u\":{},\"sql\":{},\"p\":[{}],\"close\":{}}}",
            json_str(&session_key(step.sess)),
            json_str(session_user(step.sess)),
            json_str(&sql),
            ps.join(","),
            closes_after(steps, i)
        ));
    }
    out.push(']');
    out
}

fn credentials_json() -> String {
    let items: Vec<String> = USERS
        .iter()
        .map(|(u, _, pw)| format!("{}:{}", json_str(u), json_str(pw)))
        .collect();
    format!("{{{}}}", items.join(","))
}

// --- 出力の解析と照合 -------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Obs {
    body: Vec<String>,
    sqlstate: String,
    msg: Option<String>,
}

/// 3 クライアント共通の出力（結果行・タグ行と `@@<idx> <SQLSTATE>`・`@@msg <idx> <本文>`）を
/// 解析して `into` へ足す。同じ idx のマーカーが 2 回出たら panic する（fail-closed）。
fn parse_output(out: &str, into: &mut BTreeMap<usize, Obs>) {
    let mut body: Vec<String> = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("@@msg ") {
            let (idx, text) = rest.split_once(' ').unwrap_or((rest, ""));
            let idx: usize = idx.parse().expect("numeric marker idx");
            let obs = into
                .get_mut(&idx)
                .expect("msg marker without sqlstate marker");
            assert!(obs.msg.is_none(), "duplicate msg marker");
            obs.msg = Some(text.to_string());
        } else if let Some(rest) = line.strip_prefix("@@") {
            let (idx, code) = rest.split_once(' ').expect("marker has a sqlstate");
            let idx: usize = idx.parse().expect("numeric marker idx");
            let prev = into.insert(
                idx,
                Obs {
                    body: std::mem::take(&mut body),
                    sqlstate: code.trim().to_string(),
                    msg: None,
                },
            );
            assert!(prev.is_none(), "duplicate marker for a step");
        } else {
            body.push(line.to_string());
        }
    }
    assert!(body.is_empty(), "output after the last marker");
}

/// エラー本文に含めてはならない語（他テナント・内部名の漏えい検査。ERR-7）。
const FORBIDDEN_IN_MESSAGE: [&str; 5] = ["acct", "tenant", "alice", "bob", "pw-"];

/// 期待値と観測の照合。失敗時はクライアント・モード・シナリオ・idx のみを panic へ出す
/// （SQL 本文・資格情報は出さない）。
fn check(tag: &str, steps: &[Step], obs: &BTreeMap<usize, Obs>) -> BTreeMap<String, usize> {
    assert_eq!(obs.len(), steps.len(), "{tag}: marker count mismatch");
    let mut seen_errors: BTreeMap<String, usize> = BTreeMap::new();
    for (i, step) in steps.iter().enumerate() {
        let o = obs
            .get(&i)
            .unwrap_or_else(|| panic!("{tag}: missing marker for step {i} ({})", step.scenario));
        let here = format!("{tag}: {} step {i}", step.scenario);
        match &step.expect {
            Expect::Tag(t) => {
                assert_eq!(
                    o.sqlstate, "00000",
                    "{here}: unexpected sqlstate (server message: {:?})",
                    o.msg
                );
                assert_eq!(o.body, vec![t.to_string()], "{here}: command tag");
            }
            Expect::Rows(rows) => {
                assert_eq!(
                    o.sqlstate, "00000",
                    "{here}: unexpected sqlstate (server message: {:?})",
                    o.msg
                );
                let want: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
                assert_eq!(o.body, want, "{here}: rows");
            }
            Expect::Err(code) => {
                assert_eq!(o.sqlstate, *code, "{here}: sqlstate");
                assert!(o.body.is_empty(), "{here}: rows on an error step");
                let msg = o.msg.as_deref().unwrap_or("");
                for w in FORBIDDEN_IN_MESSAGE {
                    assert!(
                        !msg.contains(w),
                        "{here}: error message leaks an internal name"
                    );
                }
                *seen_errors.entry((*code).to_string()).or_insert(0) += 1;
            }
        }
    }
    seen_errors
}

// --- クライアント別の駆動 ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Client {
    Psql,
    Psycopg,
    Pg,
}

impl Client {
    fn name(self) -> &'static str {
        match self {
            Client::Psql => "psql",
            Client::Psycopg => "psycopg",
            Client::Pg => "pg",
        }
    }
}

fn client_version(client: Client) -> String {
    let output = match client {
        Client::Psql => Command::new(resolve_tool("PSQL_BIN", "psql"))
            .arg("--version")
            .output(),
        Client::Psycopg => Command::new(resolve_tool("PYTHON_BIN", "python3"))
            .args(["-c", "import psycopg; print(psycopg.__version__)"])
            .output(),
        Client::Pg => Command::new(resolve_tool("NODE_BIN", "node"))
            .args(["-e", "console.log(require('pg/package.json').version)"])
            .output(),
    };
    let text = output
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let text = match client {
        Client::Psql => text
            .split_whitespace()
            .nth(2)
            .unwrap_or("unknown")
            .to_string(),
        _ => text,
    };
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(40)
        .collect();
    cleaned.replace(' ', "_")
}

fn run_script_client(client: Client, port: u16, steps: &[Step], extended: bool) -> String {
    let (bin, script) = match client {
        Client::Psycopg => (
            resolve_tool("PYTHON_BIN", "python3"),
            "tests/three_client/psycopg_txn_steps.py",
        ),
        Client::Pg => (
            resolve_tool("NODE_BIN", "node"),
            "tests/three_client/pg_txn_steps.js",
        ),
        Client::Psql => unreachable!("psql has its own driver"),
    };
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(script);
    let output = Command::new(&bin)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_CREDENTIALS", credentials_json())
        .env(
            "WIRE_TXN_MODE",
            if extended { "extended" } else { "simple" },
        )
        .env("WIRE_TXN_STEPS", steps_json(steps, extended))
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));
    assert!(
        output.status.success(),
        "{} driver exited abnormally: {}",
        client.name(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `\!` の行へ埋め込むパスの検査。シェルの単引用符と psql の変数展開に衝突する文字
/// （`'`・`:`・空白・制御文字など）を含むパスは拒否する（fail-closed）。
fn validate_shell_path(p: &str) -> Result<(), String> {
    if !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-'))
    {
        Ok(())
    } else {
        Err("path contains characters unsafe for the psql shell escape".to_string())
    }
}

/// psql の 1 文ぶんのスクリプト行。簡易は `<sql>;`、拡張は `<sql> \bind '<p>'... \g`。
fn psql_statement(step: &Step, extended: bool, idx: usize) -> String {
    let (sql, params) = wire_form(step, extended);
    let mut line = sql;
    if extended {
        line.push_str(" \\bind");
        for p in &params {
            assert!(
                !p.contains('\'') && !p.contains('\\'),
                "unsupported character in test parameter"
            );
            line.push_str(&format!(" '{p}'"));
        }
        line.push_str(" \\g");
    } else {
        // `;` を付けると SQL 本文の末尾に含まれて送られるため、`\g` で送る。
        line.push_str(" \\g");
    }
    line.push_str(&format!("\n\\echo @@{idx} :SQLSTATE\n"));
    line.push_str(&format!("\\echo @@msg {idx} :LAST_ERROR_MESSAGE\n"));
    line
}

fn psql_connect_args(port: u16, user: &str) -> String {
    format!("-h 127.0.0.1 -p {port} -U {user} -d irrelevant-db-name -X -w -At -F '|'")
}

fn run_psql(port: u16, steps: &[Step], extended: bool) -> String {
    let psql = resolve_tool("PSQL_BIN", "psql");
    validate_shell_path(&psql).expect("PSQL_BIN must be a plain path");
    let dir = temp_db::TempDir::new("three-client-txn-retry-psql");
    let dir_s = dir.path().to_str().expect("utf-8 path").to_string();
    validate_shell_path(&dir_s).expect("temp dir path");

    // 群ごとのスクリプトと、主スクリプト（群は最初の文の位置で `\!` 同期実行する）。
    let mut main_sql = String::new();
    let mut group_sql: BTreeMap<u32, String> = BTreeMap::new();
    let mut group_user: BTreeMap<u32, &'static str> = BTreeMap::new();
    let mut group_started: Vec<u32> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let stmt = psql_statement(step, extended, i);
        match step.sess {
            Sess::Main => main_sql.push_str(&stmt),
            Sess::Other(n, user) => {
                group_sql.entry(n).or_default().push_str(&stmt);
                group_user.insert(n, user);
                // 群の文は主スクリプトの次の文より前に完了させるため、群の最後の文に
                // 到達した位置（後続に同じ群が無い位置）で一括実行する。
                if closes_after(steps, i) && !group_started.contains(&n) {
                    group_started.push(n);
                    let user = group_user[&n];
                    main_sql.push_str(&format!(
                        "\\! PGPASSWORD='{pw}' '{psql}' {args} -f '{dir_s}/group-{n}.sql' \
                         > '{dir_s}/group-{n}.out' 2> '{dir_s}/group-{n}.err'\n",
                        pw = password_of(user),
                        args = psql_connect_args(port, user),
                    ));
                }
            }
        }
    }
    for (n, sql) in &group_sql {
        std::fs::write(dir.path().join(format!("group-{n}.sql")), sql).expect("write group script");
    }
    std::fs::write(dir.path().join("main.sql"), &main_sql).expect("write main script");

    let output = Command::new(&psql)
        .env("PGPASSWORD", password_of("alice"))
        .args(
            psql_connect_args(port, "alice")
                .split(' ')
                .map(|a| a.trim_matches('\'')),
        )
        .arg("-f")
        .arg(dir.path().join("main.sql"))
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql} (set PSQL_BIN): {e}"));
    assert!(
        output.status.success(),
        "psql exited abnormally: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // 主と群の出力を、全体の通し idx のマーカーで 1 つの出力へ合流する（行の順序は
    // 解析器が idx 単位で扱うので、ファイルごとの連結でよい）。
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    for n in group_sql.keys() {
        let out = std::fs::read_to_string(dir.path().join(format!("group-{n}.out")))
            .unwrap_or_else(|_| panic!("psql group {n} produced no output file"));
        combined.push_str(&out);
    }
    combined
}

// --- 本体 -------------------------------------------------------------------------

fn run_all(extended: bool) {
    let steps = scenarios();
    let mode = if extended { "extended" } else { "simple" };
    let mut records = Vec::new();
    for client in [Client::Psql, Client::Psycopg, Client::Pg] {
        let (db, _guard) = seed_db();
        let users = temp_db::unique_db_path("three-client-txn-retry-users");
        let _users_guard = temp_db::CleanupGuard(users.clone());
        write_users_file(&users);
        let server = spawn_wire_server(&users, &db);

        let out = match client {
            Client::Psql => run_psql(server.port, &steps, extended),
            c => run_script_client(c, server.port, &steps, extended),
        };
        let mut obs = BTreeMap::new();
        parse_output(&out, &mut obs);
        let tag = format!("{} {mode}", client.name());
        let errors = check(&tag, &steps, &obs);
        let errs: Vec<String> = errors.iter().map(|(c, n)| format!("{c}x{n}")).collect();
        let record = format!(
            "[e2e-record] three-client-txn-retry client={} version={} mode={mode} steps={} errors={}",
            client.name(),
            client_version(client),
            steps.len(),
            errs.join(",")
        );
        for w in ["pw-", "tenant", "SELECT", "UPDATE acct"] {
            assert!(!record.contains(w), "record leaks forbidden word");
        }
        records.push(record);
    }
    for r in records {
        println!("{r}");
    }
}

#[test]
#[ignore = "requires psql (>=16), python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_predicate_writes_and_40001_retry_in_explicit_transaction_simple() {
    run_all(false);
}

#[test]
#[ignore = "requires psql (>=16), python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_predicate_writes_and_40001_retry_in_explicit_transaction_extended() {
    run_all(true);
}

// --- 常時実行のガード（実クライアント不要） ----------------------------------------------

#[test]
fn json_encoder_rejects_backslash_and_control_chars() {
    assert_eq!(json_str("a\"b"), "\"a\\\"b\"");
    for bad in ["a\\b", "a\nb", "a\u{0}b"] {
        let r = std::panic::catch_unwind(|| json_str(bad));
        assert!(r.is_err(), "must reject {bad:?}");
    }
}

#[test]
fn scenario_params_stay_in_literal_safe_alphabet() {
    let steps = scenarios();
    assert!(!steps.is_empty());
    for step in &steps {
        let expanded = expand_literals(step.sql, &step.params);
        assert!(!expanded.contains('$'));
        for p in &step.params {
            assert!(p
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        }
        // 3 クライアントとも JSON 化できる（`\` や制御文字を含まない）。
        let _ = steps_json(&steps, false);
        let _ = steps_json(&steps, true);
    }
    // 他セッション群の文は連続している（psql の `\!` 一括実行の前提）。
    for (i, step) in steps.iter().enumerate() {
        if let Sess::Other(..) = step.sess {
            if i > 0 && steps[i - 1].sess != step.sess {
                assert!(
                    !steps[..i - 1].iter().any(|n| n.sess == step.sess),
                    "group steps must be contiguous"
                );
            }
        }
    }
    // 他セッション群の接続は最後の文で閉じる。主セッションは閉じない。
    for (i, step) in steps.iter().enumerate() {
        if step.sess == Sess::Main {
            assert!(!closes_after(&steps, i));
        }
    }
}

#[test]
fn shell_path_validator_rejects_unsafe_characters() {
    assert!(validate_shell_path("/tmp/fandhe-db-x_1.2/y").is_ok());
    for bad in [
        "",
        "/tmp/a b",
        "/tmp/a'b",
        "/tmp/a:b",
        "/tmp/a\nb",
        "/tmp/$x",
    ] {
        assert!(validate_shell_path(bad).is_err(), "must reject {bad:?}");
    }
}

#[test]
fn parser_reads_markers_and_detects_duplicates() {
    let mut obs = BTreeMap::new();
    parse_output(
        "BEGIN\n@@0 00000\n@@1 40001\n@@msg 1 could not serialize\nr|1\n@@2 00000\n",
        &mut obs,
    );
    assert_eq!(obs[&0].body, vec!["BEGIN"]);
    assert_eq!(obs[&1].sqlstate, "40001");
    assert_eq!(obs[&1].msg.as_deref(), Some("could not serialize"));
    assert_eq!(obs[&2].body, vec!["r|1"]);
    let dup = std::panic::catch_unwind(|| {
        let mut o = BTreeMap::new();
        parse_output("@@0 00000\n@@0 00000\n", &mut o);
    });
    assert!(dup.is_err(), "duplicate marker must panic");
    let trailing = std::panic::catch_unwind(|| {
        let mut o = BTreeMap::new();
        parse_output("@@0 00000\nstray\n", &mut o);
    });
    assert!(trailing.is_err(), "output after the last marker must panic");
}

#[test]
fn checker_fails_closed_on_missing_marker_and_wrong_sqlstate() {
    let steps = [st("T", Sess::Main, UPD, &["a", "x"], Expect::Err("40001"))];
    let missing = std::panic::catch_unwind(|| {
        check("t", &[steps[0].clone()], &BTreeMap::new());
    });
    assert!(missing.is_err(), "missing marker must panic");
    let mut obs = BTreeMap::new();
    parse_output("UPDATE 1\n@@0 00000\n", &mut obs);
    let wrong = std::panic::catch_unwind(move || {
        check("t", &[steps[0].clone()], &obs);
    });
    assert!(wrong.is_err(), "wrong sqlstate must panic");
}
