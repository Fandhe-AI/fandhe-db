//! Issue #1674（TABLE-43・親 #1669）のビュー書き込み crash 耐性検査用ツール。
//! `crash_tool_temp_table.rs` と役割・使い方の形は同じで、`scripts/crash_test_view_write.sh` から
//! `write` をバックグラウンド起動され SIGKILL される想定、続けて `verify` で再オープン後に
//! 「ビュー経由の書き込みと台帳エントリが原子的に永続している」ことを検証される想定で作られている。
//!
//! 検査の考え方: 書き込み列は決定的な操作列（`op_spec`）で、各操作は固有の `operation_id` を持つ。
//! 台帳（`EngineCore::operation_recorded`）から「適用済みの接頭辞」を求め、その接頭辞を純粋な
//! モデルで再生した行集合と、実際の表の内容を突き合わせる。接頭辞でなければ（飛びがあれば）
//! 台帳と行が原子的でない証拠、行がモデルと違えば二重適用・欠落・境界の漏れの証拠になる。
//! 本ツールは engine の production 経路（`execute_sql_in_session`／`execute_sql_in_txn`）だけを使う。
//!
//! サブコマンド:
//! - `write <db_path>`: 初回のみ表 `docs`・ビュー `v_ja`（CASCADED CHECK OPTION）・その上の入れ子ビュー
//!   `v_pos`（LOCAL CHECK OPTION）を作り、テナント B の種行を入れる。以降はテナント A で
//!   バッチ k ごとに INSERT・UPDATE（非冪等）・DELETE・明示トランザクション（INSERT＋UPDATE）を
//!   ビュー経由で発行し続ける。各バッチの先頭で CHECK OPTION 違反の INSERT（`44000`）も送る。
//!   起動時は最後に適用済みの操作をビュー経由で再送して `23505`、基表へ直接送って `22023` を確かめる。
//!   ack 後に `COMMITTED applied=<適用済み操作数>` を stdout へ出す（開始同期点・データ消失判定の基準）。
//! - `verify <db_path>`: 読み取りだけで次を検証し `RESULT ok=<bool> applied=<n>` を出す。
//!   失敗時の理由は件数・名前のみで、行の値は出さない。
//!   1. 適用済み操作が接頭辞で、明示トランザクションの 2 操作が両方入っているか両方無い
//!   2. 違反 INSERT の操作 ID が台帳に無い
//!   3. テナント A の行がモデルの再生結果と完全に一致する（二重適用・欠落・CHECK OPTION 違反行なし）
//!   4. テナント B の行が種のままで、A の操作 ID が B の台帳に無い
//!   5. 永続カタログが `docs` のみで、入れ子ビューまで問い合わせられる
//! - `self-check <workdir>`: 負の対照。健全な DB では `verify` 相当が成功し、行の漏れ・二重適用・
//!   欠落・B の改変・台帳の飛びのある DB では失敗することを確かめる（検出できれば exit 0）。

use std::collections::BTreeMap;
use std::io::Write as _;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// この crash ツールは行 id を行の識別に使う従来方式の契約を検証するため、`CREATE TABLE` を従来方式の
// 表作成へ振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;

const TENANT_A: &str = "crash-tool-view-tenant-a";
const TENANT_B: &str = "crash-tool-view-tenant-b";
/// 1 バッチあたりの操作数（INSERT・UPDATE・DELETE・明示トランザクションの 2 文）。
const OPS_PER_BATCH: u64 = 5;
/// テナント B の行 id の起点（A の id は 10 * batch + 0..=4 で、この値より十分小さい）。
const B_ID_BASE: u64 = 1_000_000;
/// write の自走上限の操作数（安全弁）。
const MAX_OPS: u64 = 5 * 1_000;
/// 走査の `LIMIT` 上限。達したら fail-closed に拒否する。
const SCAN_LIMIT: u64 = 10_000;
/// 台帳の先読み範囲（適用済み接頭辞の先が全部未記録であることを確かめる操作数）。
const LOOKAHEAD_OPS: u64 = 2 * OPS_PER_BATCH + 2;

fn main() {
    let mut args = std::env::args().skip(1);
    let (sub, path) = match (args.next(), args.next()) {
        (Some(sub), Some(path)) => (sub, path),
        _ => {
            eprintln!(
                "usage: crash_tool_view_write <write|verify> <db_path> | self-check <workdir>"
            );
            std::process::exit(2);
        }
    };
    let exit_code = match sub.as_str() {
        "write" => report(write_inner(&path)),
        "verify" => run_verify(&path),
        "self-check" => run_self_check(&path),
        other => {
            eprintln!("ERROR: unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(exit_code);
}

fn report(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

fn ctx(tenant: &str) -> Result<PolicyContext, String> {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

fn open_core_with(path: &str, mode: LedgerMode) -> Result<EngineCore, String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode))
}

fn open_core(path: &str) -> Result<EngineCore, String> {
    open_core_with(path, LedgerMode::Ledgered)
}

/// エラー文言へ入れる SQL の先頭（行データを含みうる長い文を丸ごと出さない）。
fn sql_head(sql: &str) -> String {
    sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

fn exec(core: &EngineCore, tenant: &str, ddl: bool, sql: &str) -> Result<(), String> {
    let mut s = SessionState::default();
    if ddl {
        s.allow_ddl();
    }
    core.execute_sql_in_session_legacy(&ctx(tenant)?, &mut s, sql)
        .map(|_| ())
        .map_err(|e| format!("{} failed ({})", sql_head(sql), e.wire_code()))
}

/// `sql` が `want` の `wire_code` で拒否されることを確かめる（成功・別コードは Err）。
fn expect_code(core: &EngineCore, tenant: &str, sql: &str, want: &str) -> Result<(), String> {
    let mut s = SessionState::default();
    match core.execute_sql_in_session_legacy(&ctx(tenant)?, &mut s, sql) {
        Err(e) if e.wire_code() == want => Ok(()),
        Err(e) => Err(format!(
            "{} gave {} (want {want})",
            sql_head(sql),
            e.wire_code()
        )),
        Ok(_) => Err(format!("{} succeeded (want {want})", sql_head(sql))),
    }
}

// ---- 操作列とモデル ---------------------------------------------------------------------------

/// 操作 1 件分の定義。`view_sql` はビュー経由、`base_sql` は基表へ直接送る同内容の文。
struct OpSpec {
    name: String,
    view_sql: String,
    base_sql: String,
}

fn insert_values(ids: &[(u64, u64)], batch: u64) -> String {
    ids.iter()
        .map(|(id, odd)| format!("({id}, 'ja', {batch}, 0, {odd})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 全体の操作番号 `i` の操作（バッチ `i / 5`・スロット `i % 5`）。
fn op_spec(i: u64) -> OpSpec {
    let k = i / OPS_PER_BATCH;
    let slot = i % OPS_PER_BATCH;
    let base = 10 * k;
    let cols = "(id, lang, batch, n, odd)";
    let (name, view_sql, base_sql) = match slot {
        0 => {
            let values =
                insert_values(&[(base, 0), (base + 1, 1), (base + 2, 0), (base + 3, 1)], k);
            let name = format!("ins-{k}");
            (
                name.clone(),
                format!("INSERT INTO v_ja {cols} VALUES {values} USING OPERATION_ID '{name}'"),
                format!("INSERT INTO docs {cols} VALUES {values} USING OPERATION_ID '{name}'"),
            )
        }
        1 => {
            let name = format!("upd-{k}");
            (
                name.clone(),
                format!("UPDATE v_ja SET n = n + 1 WHERE batch = {k} USING OPERATION_ID '{name}'"),
                format!("UPDATE docs SET n = n + 1 WHERE batch = {k} USING OPERATION_ID '{name}'"),
            )
        }
        2 => {
            let name = format!("del-{k}");
            (
                name.clone(),
                format!(
                    "DELETE FROM v_pos WHERE batch = {k} AND odd = 1 USING OPERATION_ID '{name}'"
                ),
                format!(
                    "DELETE FROM docs WHERE batch = {k} AND odd = 1 USING OPERATION_ID '{name}'"
                ),
            )
        }
        3 => {
            let values = insert_values(&[(base + 4, 0)], k);
            let name = format!("txn-{k}-a");
            (
                name.clone(),
                format!("INSERT INTO v_ja {cols} VALUES {values} USING OPERATION_ID '{name}'"),
                format!("INSERT INTO docs {cols} VALUES {values} USING OPERATION_ID '{name}'"),
            )
        }
        _ => {
            let name = format!("txn-{k}-b");
            let id = base + 4;
            (
                name.clone(),
                format!("UPDATE v_pos SET n = n + 1 WHERE id = {id} USING OPERATION_ID '{name}'"),
                format!("UPDATE docs SET n = n + 1 WHERE id = {id} USING OPERATION_ID '{name}'"),
            )
        }
    };
    OpSpec {
        name,
        view_sql,
        base_sql,
    }
}

fn viol_name(k: u64) -> String {
    format!("viol-{k}")
}

fn viol_sql(k: u64) -> String {
    format!(
        "INSERT INTO v_ja (id, lang, batch, n, odd) VALUES ({}, 'en', {k}, 0, 0) \
         USING OPERATION_ID '{}'",
        10 * k + 9,
        viol_name(k)
    )
}

/// モデル上の行。`(batch, n, odd)`（`lang` は常に `ja`）。
type ModelRows = BTreeMap<u64, (i64, i64, i64)>;

/// 先頭 `applied` 件の操作を適用したときのテナント A の行集合。
fn replay(applied: u64) -> ModelRows {
    let mut rows = ModelRows::new();
    for i in 0..applied {
        let k = (i / OPS_PER_BATCH) as i64;
        let base = 10 * (i / OPS_PER_BATCH);
        match i % OPS_PER_BATCH {
            0 => {
                for (id, odd) in [(base, 0), (base + 1, 1), (base + 2, 0), (base + 3, 1)] {
                    rows.insert(id, (k, 0, odd));
                }
            }
            1 => rows
                .values_mut()
                .filter(|v| v.0 == k)
                .for_each(|v| v.1 += 1),
            2 => rows.retain(|_, v| !(v.0 == k && v.2 == 1)),
            3 => {
                rows.insert(base + 4, (k, 0, 0));
            }
            _ => {
                if let Some(v) = rows.get_mut(&(base + 4)) {
                    v.1 += 1;
                }
            }
        }
    }
    rows
}

// ---- 台帳から適用済み接頭辞を求める ---------------------------------------------------------------

fn recorded(core: &EngineCore, tenant: &str, name: &str) -> Result<bool, String> {
    let op = OperationId::parse(name).map_err(|_| "invalid operation id".to_string())?;
    match core
        .operation_recorded(&ctx(tenant)?, "docs", &op)
        .map_err(|e| format!("ledger lookup failed: {e}"))?
    {
        LedgerLookup::Recorded => Ok(true),
        LedgerLookup::NotRecorded => Ok(false),
        _ => Err("ledger is unavailable".to_string()),
    }
}

/// テナント A の台帳から適用済み操作数を求める。接頭辞でない・明示トランザクションが半端・
/// 違反 INSERT が記録されている場合は Err。
fn applied_prefix(core: &EngineCore) -> Result<u64, String> {
    let mut applied = 0;
    while applied < MAX_OPS && recorded(core, TENANT_A, &op_spec(applied).name)? {
        applied += 1;
    }
    if applied % OPS_PER_BATCH == OPS_PER_BATCH - 1 {
        return Err(format!(
            "transaction ops are not atomic: only the first op of the pair is recorded (applied={applied})"
        ));
    }
    for i in applied..applied + LOOKAHEAD_OPS {
        if recorded(core, TENANT_A, &op_spec(i).name)? {
            return Err(format!(
                "ledger has a gap: op {i} is recorded but op {applied} is not"
            ));
        }
    }
    for k in 0..(applied / OPS_PER_BATCH + 3) {
        if recorded(core, TENANT_A, &viol_name(k))? {
            return Err(format!(
                "rejected insert of batch {k} is recorded in the ledger"
            ));
        }
    }
    Ok(applied)
}

// ---- 行の読み出し --------------------------------------------------------------------------------

type ScanRow = (u64, String, i64, i64, i64);

fn signed(cell: Option<&Cell>) -> Result<i64, String> {
    match cell {
        Some(Cell::SignedInteger(v)) => Ok(*v),
        Some(Cell::Integer(v)) => i64::try_from(*v).map_err(|_| "integer overflow".to_string()),
        _ => Err("unexpected cell type".to_string()),
    }
}

fn scan(core: &EngineCore, tenant: &str, table: &str) -> Result<Vec<ScanRow>, String> {
    let result = core
        .execute_sql(
            &ctx(tenant)?,
            &format!("SELECT lang, batch, n, odd FROM {table} LIMIT {SCAN_LIMIT}"),
        )
        .map_err(|e| format!("scan failed ({})", e.wire_code()))?;
    if result.rows.len() as u64 >= SCAN_LIMIT {
        return Err("row count reached this tool's scan limit; reduce sets/iterations".to_string());
    }
    let mut out = Vec::new();
    for row in &result.rows {
        let lang = match row.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            _ => return Err("unexpected lang cell".to_string()),
        };
        out.push((
            row.id,
            lang,
            signed(row.cells.get(1))?,
            signed(row.cells.get(2))?,
            signed(row.cells.get(3))?,
        ));
    }
    out.sort();
    Ok(out)
}

// ---- write -------------------------------------------------------------------------------------

fn init_if_empty(path: &str) -> Result<(), String> {
    let empty = {
        let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
        storage
            .list_tables()
            .map_err(|e| format!("list tables failed: {e}"))?
            .is_empty()
    };
    if !empty {
        return Ok(());
    }
    let core = open_core(path)?;
    init_schema(&core)
}

fn init_schema(core: &EngineCore) -> Result<(), String> {
    exec(
        core,
        TENANT_A,
        true,
        "CREATE TABLE docs (lang TEXT, batch INTEGER, n INTEGER, odd INTEGER)",
    )?;
    exec(
        core,
        TENANT_A,
        true,
        "CREATE VIEW v_ja AS SELECT * FROM docs WHERE lang = 'ja' WITH CASCADED CHECK OPTION",
    )?;
    exec(
        core,
        TENANT_A,
        true,
        "CREATE VIEW v_pos AS SELECT * FROM v_ja WHERE lang = 'ja' WITH LOCAL CHECK OPTION",
    )?;
    // テナント B の種行は A の操作（batch 0・odd 1 を含む）が漏れれば変わる値にしておく。
    exec(
        core,
        TENANT_B,
        false,
        &format!(
            "INSERT INTO docs (id, lang, batch, n, odd) VALUES \
             ({B_ID_BASE}, 'ja', 0, 0, 1), ({}, 'ja', 0, 0, 1) USING OPERATION_ID 'b-seed'",
            B_ID_BASE + 1
        ),
    )
}

fn write_inner(path: &str) -> Result<(), String> {
    init_if_empty(path)?;
    let core = open_core(path)?;
    let mut applied = applied_prefix(&core)?;

    // 再起動時の再送検査: 最後に適用済みの操作は、ビュー経由の再送が 23505、基表への直接送信が 22023。
    if applied > 0 {
        let last = op_spec(applied - 1);
        expect_code(&core, TENANT_A, &last.view_sql, "23505")?;
        expect_code(&core, TENANT_A, &last.base_sql, "22023")?;
    }

    let caller = ctx(TENANT_A)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    while applied < MAX_OPS {
        let k = applied / OPS_PER_BATCH;
        if applied % OPS_PER_BATCH == 0 {
            expect_code(&core, TENANT_A, &viol_sql(k), "44000")?;
        }
        if applied % OPS_PER_BATCH == 3 {
            // 明示トランザクション: 2 文は両方コミットされるか、両方無かったことになる。
            let (a, b) = (op_spec(applied), op_spec(applied + 1));
            let mut s = SessionState::default();
            let mut txn = core.new_session_transaction();
            for sql in ["BEGIN", a.view_sql.as_str(), b.view_sql.as_str()] {
                core.execute_sql_in_txn(&caller, &mut s, &mut txn, sql)
                    .map_err(|e| format!("{} failed ({})", sql_head(sql), e.wire_code()))?;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            core.execute_sql_in_txn(&caller, &mut s, &mut txn, "COMMIT")
                .map_err(|e| format!("COMMIT failed ({})", e.wire_code()))?;
            applied += 2;
        } else {
            exec(&core, TENANT_A, false, &op_spec(applied).view_sql)?;
            applied += 1;
        }
        // 進捗行は commit 成功後にだけ出す（スクリプトの開始同期点・acked 数の根拠）。
        writeln!(handle, "COMMITTED applied={applied}")
            .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // 上限に達したら kill を待つ（スクリプトは exit 137 を期待する）。
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

// ---- verify ------------------------------------------------------------------------------------

fn run_verify(path: &str) -> i32 {
    match verify_inner(path) {
        Ok(applied) => {
            println!("RESULT ok=true applied={applied}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

fn verify_inner(path: &str) -> Result<u64, String> {
    {
        let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
        let tables = storage
            .list_tables()
            .map_err(|e| format!("list tables failed: {e}"))?;
        if tables != vec!["docs".to_string()] {
            return Err(format!(
                "persistent tables are not exactly [docs]: count={}",
                tables.len()
            ));
        }
    }
    let core = open_core(path)?;
    let applied = applied_prefix(&core)?;
    if applied == 0 {
        return Err("no operation is recorded (vacuous)".to_string());
    }

    // 3. テナント A の行がモデルの再生結果と一致する。
    let want = replay(applied);
    let got: BTreeMap<u64, (i64, i64, i64)> = {
        let mut m = BTreeMap::new();
        for (id, lang, batch, n, odd) in scan(&core, TENANT_A, "docs")? {
            if id >= B_ID_BASE {
                continue;
            }
            if lang != "ja" {
                return Err(format!(
                    "row id={id} violates the view predicate (CHECK OPTION bypassed)"
                ));
            }
            m.insert(id, (batch, n, odd));
        }
        m
    };
    if got != want {
        let first = got
            .keys()
            .chain(want.keys())
            .find(|id| got.get(id) != want.get(id))
            .copied();
        return Err(format!(
            "tenant A rows differ from the model: actual={} expected={} first_diff_id={first:?}",
            got.len(),
            want.len()
        ));
    }

    // 4. テナント B は種のままで、A の操作 ID は B の台帳に無い。
    let b_rows: Vec<ScanRow> = scan(&core, TENANT_B, "docs")?
        .into_iter()
        .filter(|r| r.0 >= B_ID_BASE)
        .collect();
    let b_want: Vec<ScanRow> = [B_ID_BASE, B_ID_BASE + 1]
        .iter()
        .map(|id| (*id, "ja".to_string(), 0, 0, 1))
        .collect();
    if b_rows != b_want {
        return Err("tenant B rows differ from the seed".to_string());
    }
    if !recorded(&core, TENANT_B, "b-seed")? {
        return Err("tenant B seed is not recorded".to_string());
    }
    for i in 0..applied + OPS_PER_BATCH {
        if recorded(&core, TENANT_B, &op_spec(i).name)? {
            return Err(format!("tenant A op {i} is recorded in tenant B's ledger"));
        }
    }

    // 5. ビュー（入れ子を含む）が問い合わせられる。
    for view in ["v_ja", "v_pos"] {
        scan(&core, TENANT_A, view).map_err(|_| format!("view {view} is not queryable"))?;
    }
    Ok(applied)
}

// ---- 負の対照 ----------------------------------------------------------------------------------

fn run_self_check(workdir: &str) -> i32 {
    match self_check_inner(workdir) {
        Ok(()) => {
            println!("SELF-CHECK ok");
            0
        }
        Err(reason) => {
            println!("SELF-CHECK failed: {reason}");
            1
        }
    }
}

/// 先頭 `SELF_CHECK_OPS` 件の操作を適用した DB を作り、`mutate`（op 省略を許す構成のコア）で壊す。
const SELF_CHECK_OPS: u64 = 8;

fn build_db(
    workdir: &str,
    name: &str,
    mutate: impl Fn(&EngineCore) -> Result<(), String>,
) -> Result<String, String> {
    let dir = std::path::Path::new(workdir).join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir failed: {e}"))?;
    let path = dir.join("crash_tool.redb");
    let path = path.to_str().ok_or("non-utf8 path")?.to_string();
    {
        let core = open_core(&path)?;
        init_schema(&core)?;
        let mut i = 0;
        while i < SELF_CHECK_OPS {
            if i % OPS_PER_BATCH == 3 {
                let (a, b) = (op_spec(i), op_spec(i + 1));
                let mut s = SessionState::default();
                let mut txn = core.new_session_transaction();
                let caller = ctx(TENANT_A)?;
                for sql in ["BEGIN", a.view_sql.as_str(), b.view_sql.as_str(), "COMMIT"] {
                    core.execute_sql_in_txn(&caller, &mut s, &mut txn, sql)
                        .map_err(|e| format!("{} failed ({})", sql_head(sql), e.wire_code()))?;
                }
                i += 2;
            } else {
                exec(&core, TENANT_A, false, &op_spec(i).view_sql)?;
                i += 1;
            }
        }
    }
    {
        let loose = open_core_with(&path, LedgerMode::Optional)?;
        mutate(&loose)?;
    }
    Ok(path)
}

fn self_check_inner(workdir: &str) -> Result<(), String> {
    let healthy = build_db(workdir, "healthy", |_| Ok(()))?;
    verify_inner(&healthy).map_err(|r| format!("healthy db was rejected: {r}"))?;

    type Mutation = fn(&EngineCore) -> Result<(), String>;
    let cases: [(&str, &str, Mutation); 6] = [
        ("leaked-row", "a row written around the ledger", |c| {
            exec(
                c,
                TENANT_A,
                false,
                "INSERT INTO docs (id, lang, batch, n, odd) VALUES (5000, 'ja', 0, 0, 0)",
            )
        }),
        ("check-bypass", "a row violating the view predicate", |c| {
            exec(
                c,
                TENANT_A,
                false,
                "INSERT INTO docs (id, lang, batch, n, odd) VALUES (5001, 'en', 0, 0, 0)",
            )
        }),
        ("double-apply", "a double-applied update", |c| {
            exec(
                c,
                TENANT_A,
                false,
                "UPDATE docs SET n = n + 1 WHERE batch = 0",
            )
        }),
        ("lost-row", "a row lost without a ledger trace", |c| {
            exec(c, TENANT_A, false, "DELETE FROM docs WHERE id = 0")
        }),
        ("tenant-b", "a changed tenant B row", |c| {
            exec(
                c,
                TENANT_B,
                false,
                &format!("UPDATE docs SET n = 9 WHERE id = {B_ID_BASE}"),
            )
        }),
        ("ledger-gap", "an out-of-order ledger entry", |c| {
            // 適用済み接頭辞（8 件）の先にある操作 ID だけを台帳へ記録する（行の効果も付いてくる）。
            let spec = op_spec(SELF_CHECK_OPS + 2);
            let sql = spec.base_sql;
            exec(c, TENANT_A, false, &sql)
        }),
    ];
    for (name, what, mutate) in cases {
        let path = build_db(workdir, name, mutate)?;
        if verify_inner(&path).is_ok() {
            return Err(format!("{what} was not detected"));
        }
    }
    Ok(())
}
