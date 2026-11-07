//! Issue #2417（親 TASK-332。ビヘイビア PERSIST-1・TABLE-10。ポインタ: `docs/spec/04-behavior/`）の
//! 「1 文の DDL の実行」クラッシュ耐性回帰テスト用ツール。`scripts/crash_test_ddl.sh`
//! （Makefile の `crash-test-ddl`・CI の `crash-test-cross-table` ジョブの 1 step から呼ばれる）が
//! `write` をバックグラウンド起動して SIGKILL し、続けて `verify` で再オープン後の整合を検証する。
//!
//! 既存の crash 道具のうち DDL を扱うのは `crash_tool_gin_index.rs` の `ddl` モード（GIN 索引の
//! 作り直し）だけで、`CREATE TABLE`・`ALTER TABLE ADD COLUMN`・スカラー／一意の `CREATE INDEX`・
//! `DROP TABLE` はどの道具も実行も整合性検査もしていなかった。本ツールは production の SQL 表層
//! （`EngineCore::execute_sql_in_session`）だけで DDL を 1 文ずつ autocommit で実行し、kill 後の
//! カタログ・行・索引宣言・物理表が「文の前」か「文の後」のどちらか一方に完全一致することを確かめる。
//!
//! 文の列（ラウンド r ごとに表 `dd_<r>`。決定的。適用済みの段数 n から期待状態が一意に決まる）:
//! 0 `CREATE TABLE` / 1 `INSERT`（SEED 行）/ 2 `ALTER TABLE ADD COLUMN c DEFAULT <r>` /
//! 3 `CREATE INDEX`（スカラー）/ 4 `CREATE UNIQUE INDEX` / 5 `DROP TABLE dd_<r-1>`（r >= 1 のみ）。
//!
//! サブコマンド:
//! - `write <db_path>`: 現在の n を推定して（一致する n がちょうど 1 つでなければ開始を拒否する）
//!   続きの文を 1 文ずつ、実行直前に `STMT` 行を出し、成功のたびに `COMMITTED batch=<n> rows=<n>` を出力して flush する。
//! - `verify <db_path>`: `RESULT ok=true rows=<n> rounds=<R>` または `RESULT ok=false reason=<...>`。
//!   カタログ・行・索引宣言から推定した n の期待状態が完全一致し、かつ一致する n がちょうど 1 つ
//!   であること（0 件は部分的な DDL・破損、2 件以上はオラクルが弱すぎる）。一意索引は読み取り専用
//!   検証 `verify_unique_index_read_only` を重複プローブより先に通す（書き込み経路は遅延バックフィルで
//!   索引を黙って作り直すため）。DROP 済みの表は raw redb にセグメントが一致する物理表が残らない
//!   ことまで見る。他テナントからは行が見えない。n = 0（空の DB）は空虚な成功として失敗にする。
//! - `self-check <workdir>`: 負の対照。到達しうる全状態を受理して n が一意に決まることと、ステップ列から
//!   は到達できない状態（索引の飛ばし・行の過不足・DROP 残骸・DROP 欠落・既定値違い・一意索引の物理
//!   構造欠落・空 DB）を拒否することを確かめる。
//!
//! Issue #2418（SQL-23 を併せて参照）: `write` は各文の `exec` 直前に `STMT step=<s> stage=<g>` を出す。
//! kill 位置の分類（最後の進行行が STMT なら文の実行中）と、実行中なら n が ack 済み n か +1、文の間なら
//! 完全一致の検査、DDL 段の実行中の中断を 1 度も観測しない実行の失敗は `scripts/crash_test_ddl.sh` が行う。
//!
//! 対象外（兄弟 Issue）: DDL の途中のエラーで何も残らないことと writer 待機の `55P03` は #2419 が担当する。
//! 文の内部（commit 直前・直後）で決定的に止める同期点は設けない（確率的な kill 位置の分類に留める）。
//!
//! 失敗メッセージには `wire_code`・表名・件数だけを含め、行の値・他テナントの存在情報は出さない。

// 許可（clippy::disallowed_methods）: 本ファイルは engine の公開 API の外から redb を直接開き、
// 永続内容の検査・破壊・裏口の対照を行う。変更ログの捕捉（`storage::change_log`）を通す
// 対象は engine 製品コードの書き込み点で、外部クレートとしての直接操作は対象外。
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use engine::catalog::{ColumnDef, ColumnDefault, ColumnType};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use redb::{ReadableDatabase, TableHandle};

/// DDL を実行するテナントと、境界確認用の別テナント。
const TENANT_A: &str = "crash-ddl-a";
const TENANT_B: &str = "crash-ddl-b";
/// 表 `dd_<r>` へ投入する行数（k = 1..=SEED）。
const SEED: i64 = 8;
/// 自走するラウンド数の上限（安全弁。通常は SIGKILL で止まる。DoS 対策で表・行の増加も抑える）。
const MAX_ROUNDS: u64 = 30_000;
/// ステップ総数の上限（ラウンド 0 は 5 段、以降は 6 段）。
const MAX_STEPS: u64 = 5 + 6 * (MAX_ROUNDS - 1);
/// `SELECT … LIMIT` の上限。達したら全件を見られないので fail-closed。
const SCAN_LIMIT: u64 = 10_000;
/// 文と文の間の待機（マイクロ秒）。1 文の実行は 0.1ms 前後と短く、待機が長いと kill の大半が文と文の
/// 間に当たる（#2418）。短くして実行中の割合を上げ、かつ MAX_STEPS に達して自走終了しない範囲に抑える。
const PAUSE_US: u64 = 100;

type Res<T> = Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["write", path] => run_write(path),
        ["verify", path] => run_verify(path),
        ["self-check", dir] => run_self_check(dir),
        _ => {
            eprintln!("usage: crash_tool_ddl <write <db>|verify <db>|self-check <workdir>>");
            2
        }
    };
    std::process::exit(code);
}

// ---------- ステップ列と期待状態（write・verify・self-check で共有する純関数） ----------

/// 表 1 つの期待状態。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Tbl {
    rows: bool,
    col_c: bool,
    sidx: bool,
    uidx: bool,
}

/// ラウンド番号 → 表の状態（生きている表だけ）。
type State = BTreeMap<u64, Tbl>;

/// ステップ番号（0 起点）→（ラウンド, 段）。ラウンド 0 は 5 段、以降は 6 段。
fn locate(step: u64) -> (u64, u64) {
    if step < 5 {
        (0, step)
    } else {
        let t = step - 5;
        (1 + t / 6, t % 6)
    }
}

/// ステップ `step` を状態へ適用する（期待状態の導出）。`created` は作成済み（DROP 済みを含む）のラウンド。
fn apply(state: &mut State, created: &mut BTreeSet<u64>, step: u64) {
    let (r, stage) = locate(step);
    match stage {
        0 => {
            state.insert(r, Tbl::default());
            created.insert(r);
        }
        5 => {
            state.remove(&r.saturating_sub(1));
        }
        s => {
            if let Some(t) = state.get_mut(&r) {
                match s {
                    1 => t.rows = true,
                    2 => t.col_c = true,
                    3 => t.sidx = true,
                    _ => t.uidx = true,
                }
            }
        }
    }
}

/// 適用済みの段数 n の期待状態と、作成済みのラウンド集合。
fn expected(n: u64) -> (State, BTreeSet<u64>) {
    let mut st = State::new();
    let mut created = BTreeSet::new();
    for s in 0..n {
        apply(&mut st, &mut created, s);
    }
    (st, created)
}

/// ステップ `step` で実行する 1 文。
fn step_sql(step: u64) -> String {
    let (r, stage) = locate(step);
    match stage {
        0 => format!("CREATE TABLE dd_{r} (k BIGINT, v TEXT)"),
        1 => {
            let vals: Vec<String> = (1..=SEED).map(|k| format!("({k}, 'r{r}-{k}')")).collect();
            format!("INSERT INTO dd_{r} (k, v) VALUES {}", vals.join(", "))
        }
        2 => format!("ALTER TABLE dd_{r} ADD COLUMN c BIGINT DEFAULT {r}"),
        3 => format!("CREATE INDEX dd_{r}_v ON dd_{r} (v)"),
        4 => format!("CREATE UNIQUE INDEX dd_{r}_k ON dd_{r} (k)"),
        _ => format!("DROP TABLE dd_{}", r.saturating_sub(1)),
    }
}

/// `dd_<数字>` の表名からラウンド番号を取り出す（それ以外は `None`）。
fn parse_round(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("dd_")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

// ---------- 共通ヘルパー ----------

fn ctx(tenant: &str) -> Res<PolicyContext> {
    // SQL 表層の INSERT は可視性を Private 固定で書くため、両方を許可する。
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

fn open_core(path: &str) -> Res<EngineCore> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ))
}

/// 文を実行する。失敗は `wire_code` だけを返す（文の中身・行データは含めない）。
fn try_exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let c = ctx(tenant).map_err(|_| "ctx".to_string())?;
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&c, &mut s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Res<SqlOutcome> {
    try_exec(core, tenant, sql).map_err(|code| {
        let head: String = sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
        format!("statement failed ({code}): {head}")
    })
}

/// 期待する `wire_code` で失敗することを確かめる。成功・別コードは失敗にする（握りつぶさない）。
fn expect_code(core: &EngineCore, tenant: &str, sql: &str, want: &str, what: &str) -> Res<()> {
    match try_exec(core, tenant, sql) {
        Err(code) if code == want => Ok(()),
        Err(code) => Err(format!("{what}: unexpected wire_code {code} (want {want})")),
        Ok(_) => Err(format!("{what}: statement was wrongly accepted")),
    }
}

fn int_of(c: &Cell) -> Option<i64> {
    match c {
        Cell::SignedInteger(i) => Some(*i),
        Cell::Integer(u) => i64::try_from(*u).ok(),
        _ => None,
    }
}

fn text_of(c: &Cell) -> Option<&str> {
    match c {
        Cell::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

/// `SELECT` の結果の行（セル列）を返す。上限に達したら fail-closed。
fn select(core: &EngineCore, tenant: &str, sql: &str) -> Res<Vec<Vec<Cell>>> {
    match exec(core, tenant, sql)? {
        SqlOutcome::Query(q) => {
            if q.rows.len() as u64 >= SCAN_LIMIT {
                return Err("scan limit reached; cannot see all rows".to_string());
            }
            Ok(q.rows.into_iter().map(|r| r.cells).collect())
        }
        _ => Err("expected a query outcome".to_string()),
    }
}

// ---------- write ----------

fn committed(n: u64) {
    println!("COMMITTED batch={n} rows={n}");
    let _ = std::io::stdout().flush();
}

/// 文の実行開始マーカー `STMT step=<s> stage=<g>` を出して flush する（ack ではない）。
/// `scripts/crash_test_ddl.sh` が write.log の最後の進行行から kill 位置（文の実行中か、文と文の間か）
/// を分類するために使う。段番号のみでテナント ID・行の値は含めない。flush 失敗は続行しない。
fn in_stmt(step: u64) -> Res<()> {
    let (_, stage) = locate(step);
    let mut out = std::io::stdout();
    writeln!(out, "STMT step={step} stage={stage}").map_err(|e| format!("emit failed: {e}"))?;
    out.flush().map_err(|e| format!("flush failed: {e}"))
}

fn run_write(path: &str) -> i32 {
    match write(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: write failed: {reason}");
            1
        }
    }
}

fn write(path: &str) -> Res<()> {
    let n0 = {
        let cat = Catalog::read(path)?;
        let core = open_core(path)?;
        infer(&core, &cat)?
    };
    let core = open_core(path)?;
    for step in n0..MAX_STEPS {
        // 開始マーカーは `exec` の直前に出す（間に待機を入れない。入れると「文の開始前」の kill を
        // 実行中と数えてしまい、空洞化する）。待機は COMMITTED の後ろ（次の STMT の前）に置く。
        in_stmt(step)?;
        exec(&core, TENANT_A, &step_sql(step))?;
        committed(step + 1);
        std::thread::sleep(std::time::Duration::from_micros(PAUSE_US));
    }
    Ok(())
}

// ---------- 観測（カタログ・行・索引宣言） ----------

/// 索引宣言 1 件の観測値。
struct IdxInfo {
    name: String,
    table: String,
    scalar: bool,
    unique: bool,
    columns: Vec<String>,
}

/// カタログの観測値（`Storage` を開いて読み、すぐ閉じる。`EngineCore` と同時には開かない）。
struct Catalog {
    tables: Vec<String>,
    columns: BTreeMap<String, Vec<String>>,
    /// 追加列 `c` の列型・INSERT 用 DEFAULT・読み出し補完値（表名 → 定義）。列名だけでは
    /// `ALTER COLUMN DROP DEFAULT` 等による DEFAULT の欠落を見逃すため別に保持する。
    col_c_def: BTreeMap<String, ColumnDef>,
    indexes: Vec<IdxInfo>,
    /// raw redb の物理表名（一意索引の物理構造 `user_uniq/<表>` の有無の判定に使う）。
    physical: BTreeSet<String>,
}

impl Catalog {
    fn read(path: &str) -> Res<Catalog> {
        let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
        let tables = storage
            .list_tables()
            .map_err(|e| format!("list tables failed: {e}"))?;
        let mut columns = BTreeMap::new();
        let mut col_c_def = BTreeMap::new();
        for t in &tables {
            let schema = storage
                .get_table_schema(t)
                .map_err(|e| format!("get schema failed: table={t}: {e}"))?;
            columns.insert(
                t.clone(),
                schema.columns.iter().map(|c| c.name.clone()).collect(),
            );
            if let Some(c) = schema.columns.iter().find(|c| c.name == "c") {
                col_c_def.insert(t.clone(), c.clone());
            }
        }
        let indexes = storage
            .list_indexes()
            .map_err(|e| format!("list indexes failed: {e}"))?
            .into_iter()
            .map(|d| IdxInfo {
                name: d.name,
                table: d.table,
                scalar: matches!(d.kind, engine::catalog::IndexKind::Scalar),
                unique: d.unique,
                columns: d.columns,
            })
            .collect();
        drop(storage);
        // Storage を閉じてから raw redb の物理表名を読む。
        let physical = physical_tables(path)?;
        Ok(Catalog {
            tables,
            columns,
            col_c_def,
            indexes,
            physical,
        })
    }
}

/// カタログと行から観測状態を組む。期待に無い表・列・索引・行は即失敗にする（fail-closed）。
fn observe(core: &EngineCore, cat: &Catalog) -> Res<State> {
    let mut st = State::new();
    for name in &cat.tables {
        let r = parse_round(name).ok_or_else(|| format!("unexpected table in catalog: {name}"))?;
        let cols = cat.columns.get(name).map(Vec::as_slice).unwrap_or(&[]);
        let col_c = match cols {
            [k, v] if k == "k" && v == "v" => false,
            [k, v, c] if k == "k" && v == "v" && c == "c" => true,
            _ => {
                return Err(format!(
                    "unexpected columns: table={name} count={}",
                    cols.len()
                ))
            }
        };
        if col_c {
            // 追加列 c の列型・DEFAULT・補完値が期待（BIGINT・DEFAULT <r>）に完全一致する。
            let want = Some(ColumnDefault::Number(r.to_string()));
            let ok = cat.col_c_def.get(name).is_some_and(|d| {
                d.ty == ColumnType::BigInt && d.default_expr == want && d.fill_value == want
            });
            if !ok {
                return Err(format!("column c definition mismatch: table={name}"));
            }
        }
        let proj = if col_c { "k, v, c" } else { "k, v" };
        let rows = select(
            core,
            TENANT_A,
            &format!("SELECT {proj} FROM {name} ORDER BY k LIMIT {SCAN_LIMIT}"),
        )?;
        let has_rows = match rows.len() {
            0 => false,
            n if n as i64 == SEED => {
                for (i, cells) in rows.iter().enumerate() {
                    let k = i64::try_from(i).map_err(|_| "index overflow".to_string())? + 1;
                    let ok_k = cells.first().and_then(int_of) == Some(k);
                    let ok_v = cells.get(1).and_then(text_of) == Some(format!("r{r}-{k}").as_str());
                    let ok_c = !col_c
                        || cells.get(2).and_then(int_of)
                            == Some(i64::try_from(r).map_err(|_| "round overflow".to_string())?);
                    if !(ok_k && ok_v && ok_c) {
                        return Err(format!("row content mismatch: table={name} k={k}"));
                    }
                }
                true
            }
            n => return Err(format!("unexpected row count: table={name} count={n}")),
        };
        if !col_c {
            expect_code(
                core,
                TENANT_A,
                &format!("SELECT c FROM {name} LIMIT 1"),
                "42703",
                &format!("column c probe on {name}"),
            )?;
        }
        st.insert(
            r,
            Tbl {
                rows: has_rows,
                col_c,
                sidx: false,
                uidx: false,
            },
        );
    }
    for ix in &cat.indexes {
        let r = parse_round(&ix.table)
            .filter(|r| st.contains_key(r))
            .ok_or_else(|| format!("index of a missing table: table={}", ix.table))?;
        let is_v = ix.name == format!("dd_{r}_v") && ix.scalar && !ix.unique && ix.columns == ["v"];
        let is_k = ix.name == format!("dd_{r}_k") && ix.unique && ix.columns == ["k"];
        let t = st
            .get_mut(&r)
            .ok_or_else(|| "state lookup failed".to_string())?;
        if is_v && !t.sidx {
            t.sidx = true;
        } else if is_k && !t.uidx {
            t.uidx = true;
        } else {
            return Err(format!("unexpected index declaration: name={}", ix.name));
        }
    }
    Ok(st)
}

/// 観測状態と一致する n をちょうど 1 つ特定する。0 件・2 件以上は失敗（fail-closed）。
fn infer(core: &EngineCore, cat: &Catalog) -> Res<u64> {
    let obs = observe(core, cat)?;
    let mut st = State::new();
    let mut created = BTreeSet::new();
    let mut hits = Vec::new();
    if st == obs {
        hits.push(0u64);
    }
    for s in 0..MAX_STEPS {
        apply(&mut st, &mut created, s);
        if st == obs {
            hits.push(s + 1);
        }
    }
    match hits.as_slice() {
        [n] => Ok(*n),
        [] => Err(format!(
            "state matches no step count (partial DDL or corruption): tables={}",
            obs.len()
        )),
        _ => Err("state matches multiple step counts (oracle too weak)".to_string()),
    }
}

// ---------- verify ----------

fn run_verify(path: &str) -> i32 {
    match verify(path) {
        Ok((n, rounds)) => {
            println!("RESULT ok=true rows={n} rounds={rounds}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

/// raw redb の物理表名の一覧。
fn physical_tables(path: &str) -> Res<BTreeSet<String>> {
    let db = redb::Database::open(path).map_err(|_| "raw open failed".to_string())?;
    let r = db.begin_read().map_err(|_| "raw read failed".to_string())?;
    let tables = r
        .list_tables()
        .map_err(|_| "raw list tables failed".to_string())?;
    Ok(tables.map(|h| h.name().to_string()).collect())
}

/// 物理表名のセグメント（`/` 区切り）が `dd_<r>`・`dd_<r>_v`・`dd_<r>_k` のとき `<r>` を返す。
/// 接頭辞を決め打ちしない（`dd_1` と `dd_10` を取り違えない）。
fn seg_round(seg: &str) -> Option<u64> {
    let rest = seg.strip_prefix("dd_")?;
    let num = rest
        .strip_suffix("_v")
        .or_else(|| rest.strip_suffix("_k"))
        .unwrap_or(rest);
    num.parse().ok()
}

/// raw redb の物理表名のうち、生存表（`live`）に属さない `dd_<r>` を指すものを返す。
/// DROP 済みの表の残骸に加え、カタログ未登録の表（CREATE TABLE の部分適用の残骸）も拒否する。
fn raw_residue(path: &str, live: &BTreeSet<u64>) -> Res<Vec<String>> {
    let tables = physical_tables(path)?;
    Ok(tables
        .into_iter()
        .filter(|name| {
            name.split('/')
                .filter_map(seg_round)
                .any(|r| !live.contains(&r))
        })
        .collect())
}

/// 戻り値は（適用済みの段数 n, 作成済みのラウンド数）。
fn verify(path: &str) -> Res<(u64, u64)> {
    let cat = Catalog::read(path)?;
    let (n, created) = {
        let core = open_core(path)?;
        let n = infer(&core, &cat)?;
        if n == 0 {
            return Err("no DDL applied (vacuous)".to_string());
        }
        let (live, created) = expected(n);
        // 読み取り専用の一意索引検証を、あらゆる書き込み系プローブより先に通す。物理構造
        // `user_uniq/<表>` は宣言後の最初の書き込みで遅延構築されるため、DDL 直後で未構築の
        // 間は検証対象外（宣言の有無と重複プローブの `23505` が宣言の生存を保証する）。
        for (r, t) in &live {
            if t.uidx && cat.physical.contains(&format!("user_uniq/dd_{r}")) {
                core.verify_unique_index_read_only(&ctx(TENANT_A)?, &format!("dd_{r}"))
                    .map_err(|e| format!("unique index verification failed: table=dd_{r}: {e}"))?;
            }
        }
        for (r, t) in &live {
            let name = format!("dd_{r}");
            if t.sidx {
                // 索引が効くクエリの結果が全走査（期待）と一致する。
                let rows = select(
                    &core,
                    TENANT_A,
                    &format!("SELECT k FROM {name} WHERE v = 'r{r}-3' LIMIT {SCAN_LIMIT}"),
                )?;
                if rows.len() != 1
                    || rows.first().and_then(|c| c.first()).and_then(int_of) != Some(3)
                {
                    return Err(format!(
                        "scalar index read differs from the full scan: table={name}"
                    ));
                }
            }
            if t.uidx {
                expect_code(
                    &core,
                    TENANT_A,
                    &format!("INSERT INTO {name} (k, v) VALUES (3, 'dup')"),
                    "23505",
                    &format!("duplicate probe on {name}"),
                )?;
                let after = select(
                    &core,
                    TENANT_A,
                    &format!("SELECT k FROM {name} LIMIT {SCAN_LIMIT}"),
                )?;
                if after.len() as i64 != SEED {
                    return Err(format!(
                        "row count changed by a rejected insert: table={name}"
                    ));
                }
            }
            // テナント境界: 別テナントからは行が見えない。
            let other = select(
                &core,
                TENANT_B,
                &format!("SELECT k FROM {name} LIMIT {SCAN_LIMIT}"),
            )?;
            if !other.is_empty() {
                return Err(format!(
                    "rows visible to another tenant: table={name} count={}",
                    other.len()
                ));
            }
        }
        // DROP 済みの表は参照できない。
        for j in created.iter().filter(|j| !live.contains_key(j)) {
            expect_code(
                &core,
                TENANT_A,
                &format!("SELECT k FROM dd_{j} LIMIT 1"),
                "42P01",
                &format!("dropped table probe dd_{j}"),
            )?;
        }
        (n, created)
    };
    // EngineCore を閉じてから raw redb を読む（DROP 済みの表の物理表の残骸）。
    let (live, _) = expected(n);
    let live_rounds: BTreeSet<u64> = live.keys().copied().collect();
    let residue = raw_residue(path, &live_rounds)?;
    if !residue.is_empty() {
        return Err(format!(
            "physical tables of dropped or unregistered tables remain: count={}",
            residue.len()
        ));
    }
    Ok((n, created.len() as u64))
}

// ---------- self-check ----------

fn run_self_check(dir: &str) -> i32 {
    match self_check(dir) {
        Ok(()) => {
            println!("SELF-CHECK ok");
            0
        }
        Err(reason) => {
            println!("SELF-CHECK ok=false reason={reason}");
            1
        }
    }
}

fn run_sqls(path: &str, sqls: &[String]) -> Res<()> {
    let core = open_core(path)?;
    for s in sqls {
        exec(&core, TENANT_A, s)?;
    }
    Ok(())
}

fn raw_create(path: &str, name: &str) -> Res<()> {
    let db = redb::Database::open(path).map_err(|_| "raw open failed".to_string())?;
    let w = db
        .begin_write()
        .map_err(|_| "raw write failed".to_string())?;
    {
        let def: redb::TableDefinition<&str, &str> = redb::TableDefinition::new(name);
        w.open_table(def)
            .map_err(|_| "raw create table failed".to_string())?;
    }
    w.commit().map_err(|_| "raw commit failed".to_string())
}

fn steps(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(step_sql).collect()
}

fn self_check(dir: &str) -> Res<()> {
    // 正例: 到達しうる全状態（3 ラウンド分）を 1 段ずつ進め、各段の直後にちょうどその n で通る。
    let pos = format!("{dir}/positive.redb");
    let total = 5 + 6 + 6;
    for s in 0..total {
        run_sqls(&pos, &[step_sql(s)])?;
        let (n, _) = verify(&pos).map_err(|e| format!("positive step {s} rejected: {e}"))?;
        if n != s + 1 {
            return Err(format!("positive step {s} inferred n={n}"));
        }
    }

    // 正例（物理構造あり）: 宣言後に書き込みが 1 度通ると `user_uniq/dd_0` が構築される。行集合は
    // 元へ戻すので状態は n = 5 のまま。読み取り専用の一意索引検証が実際に走ることを確かめる。
    let phys = format!("{dir}/positive-physical.redb");
    let mut v = steps(0..5);
    v.push("INSERT INTO dd_0 (k, v) VALUES (100, 'x')".to_string());
    v.push("DELETE FROM dd_0 WHERE k = 100".to_string());
    run_sqls(&phys, &v)?;
    if !Catalog::read(&phys)?.physical.contains("user_uniq/dd_0") {
        return Err("unique index physical table was not built (vacuous)".to_string());
    }
    match verify(&phys) {
        Ok((5, _)) => {}
        Ok((n, _)) => return Err(format!("positive-physical inferred n={n}")),
        Err(e) => return Err(format!("positive-physical rejected: {e}")),
    }

    // 負例: ステップ列からは到達できない状態は必ず拒否される。
    let neg = |name: &str, build: &dyn Fn(&str) -> Res<()>| -> Res<()> {
        let p = format!("{dir}/neg-{name}.redb");
        build(&p)?;
        match verify(&p) {
            Ok(_) => Err(format!("oracle accepted an unreachable state: {name}")),
            Err(reason) => {
                println!("NEGATIVE {name}: rejected ({reason})");
                Ok(())
            }
        }
    };
    neg("skip-scalar-index", &|p| {
        let mut v = steps(0..3);
        v.push(step_sql(4));
        run_sqls(p, &v)
    })?;
    neg("extra-row", &|p| {
        let mut v = steps(0..5);
        v.push("INSERT INTO dd_0 (k, v) VALUES (100, 'x')".to_string());
        run_sqls(p, &v)
    })?;
    neg("missing-row", &|p| {
        let mut v = steps(0..5);
        v.push("DELETE FROM dd_0 WHERE k = 1".to_string());
        run_sqls(p, &v)
    })?;
    neg("drop-residue", &|p| {
        run_sqls(p, &steps(0..11))?;
        raw_create(p, "user_rows/dd_0")
    })?;
    neg("unregistered-table-residue", &|p| {
        run_sqls(p, &steps(0..5))?;
        raw_create(p, "user_rows/dd_1")
    })?;
    neg("missing-drop", &|p| {
        let mut v = steps(0..10);
        v.push("CREATE TABLE dd_2 (k BIGINT, v TEXT)".to_string());
        run_sqls(p, &v)
    })?;
    neg("wrong-default", &|p| {
        let mut v = steps(0..5);
        v[2] = "ALTER TABLE dd_0 ADD COLUMN c BIGINT DEFAULT 1".to_string();
        run_sqls(p, &v)
    })?;
    neg("dropped-default", &|p| {
        let mut v = steps(0..5);
        v.push("ALTER TABLE dd_0 ALTER COLUMN c DROP DEFAULT".to_string());
        run_sqls(p, &v)
    })?;
    neg("extra-index", &|p| {
        let mut v = steps(0..5);
        v.push("CREATE INDEX dd_0_x ON dd_0 (v)".to_string());
        run_sqls(p, &v)
    })?;
    neg("empty-db", &|p| {
        drop(open_core(p)?);
        Ok(())
    })?;
    Ok(())
}
