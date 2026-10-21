//! Issue #2209（INDEX-8・PERSIST-1・RECOVER。設計は `docs/design/gin-jsonb-index.md` D11・§8.5）の
//! GIN 相当索引クラッシュ耐性回帰テスト用ツール。`crash_tool_unique_index.rs`・
//! `crash_tool_sequence.rs` と役割・使い方の形は同じで、`scripts/crash_test_gin_index.sh`
//! （Makefile の `crash-test-gin-index`・CI の同名ジョブから呼ばれる）が `write` をバックグラウンド
//! 起動して SIGKILL し、続けて `verify` で再オープン後の整合を検証する。
//!
//! GIN 相当索引は行・ポスティング・マーカー・逆引きを同一 write txn で書く。本ツールは公開 API の
//! `EngineCore::execute_sql_in_session`／`execute_sql_in_txn` だけを使い（production の SQL 表層経路）、
//! 複数テナントの共有索引に対してコミット境界の原子性を検査する。
//!
//! サブコマンド:
//! - `write <db_path> <dml|ddl>`:
//!   - `dml`: 表 `g`（`g_ops`・`g_path` の 2 索引を宣言）と GIN なしの双子表 `p` へ、テナントを
//!     交互にして 1 バッチ 1 明示 txn で書き続ける（`BEGIN` → 5 行 INSERT → 先頭行 UPDATE → 2 行目
//!     DELETE → `COMMIT`。UPDATE・DELETE は stale ポスティングの後片付けを通す）。バッチ後に
//!     `COMMITTED batch=<n> rows=<alive>` を出力して flush する（開始同期点と「確認済み commit」の根拠）。
//!   - `ddl`: 3 テナントの行を投入してから `COMMITTED` を出し、その後 `DROP INDEX` → `CREATE INDEX`
//!     （同期構築の 1 txn）を繰り返す。kill が構築・破棄の途中に当たることが狙い。
//! - `verify <db_path> [dml|ddl]`（既定 `dml`）: 読み取り専用。オラクルは (1) `g` と `p` の行集合が
//!   テナントごとに一致する（同じ txn で書くため crash 後も一致する）、(2) 行を持つ全テナント × 宣言済み
//!   の全索引で `verify_gin_index_read_only` が `marker_valid && missing == 0 && stale == 0`（欠落 0 が
//!   D11 の必須オラクル）、(3) 演算子ごとの固定クエリの結果が `g` と `p` でバイト単位に一致し、他テナントの
//!   行が現れない、(4) `ddl` では宣言が残っていれば監査が clean、無ければ `user_gin/<名前>` の
//!   物理テーブルが残っていない（宣言と物理テーブルの原子性）。
//! - `self-check <db_path>`: 負の対照。小さな 2 テナントの DB を作り、片方のテナントのポスティングを raw
//!   redb で全削除して、オラクルが欠落を検出する（他テナントの監査は clean のまま）ことを確かめる。

use std::io::Write as _;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use redb::{ReadableDatabase, ReadableTable, TableHandle};

/// 書き込み・検証で使うテナント（共有索引テーブルに複数テナントのキーを混在させるため 3 つ）。
const TENANTS: [&str; 3] = ["crash-gin-a", "crash-gin-b", "crash-gin-c"];
/// GIN を宣言する表と、GIN を宣言しない双子表（読み取り経路のオラクル）。
const GIN_TABLE: &str = "g";
const PLAIN_TABLE: &str = "p";
/// 宣言する索引（名前, 演算子クラス）。
const INDEXES: [(&str, &str); 2] = [("g_ops", "jsonb_ops"), ("g_path", "jsonb_path_ops")];
/// `SELECT … LIMIT` の上限。SQL 表層の上限と同じ。達したら全件を見られないので fail-closed。
const SCAN_LIMIT: u64 = 10_000;
/// write（dml）の自走上限（安全弁。通常は SIGKILL で止まる）。
const MAX_BATCHES: u64 = 1_500;
/// 1 バッチの INSERT 行数。
const BATCH: u64 = 5;
/// ddl モードで 1 テナントあたり投入する行数。
const DDL_SEED_PER_TENANT: u64 = 300;
/// ddl モードの DROP/CREATE 繰り返し上限（安全弁）。
const MAX_DDL_CYCLES: u64 = 5_000;

type Res<T> = Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["write", path, mode] => run_write(path, mode),
        ["verify", path] => report(verify(path, "dml")),
        ["verify", path, mode] => report(verify(path, mode)),
        ["self-check", path] => run_self_check(path),
        _ => {
            eprintln!("usage: crash_tool_gin_index <write <db> <dml|ddl>|verify <db> [dml|ddl]|self-check <db>>");
            2
        }
    };
    std::process::exit(code);
}

fn report(result: Res<u64>) -> i32 {
    match result {
        Ok(rows) => {
            println!("RESULT ok=true rows={rows}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

// ---------- 共通ヘルパー ----------

fn ctx(tenant: &str) -> Res<PolicyContext> {
    // SQL 表層の INSERT は可視性を Private 固定で書くため、両方を許可する。
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

/// `create` が真なら表 `g`・`p` が無ければ作る（write 用。verify は既存 DB を読み取り専用で開く）。
fn open_core(path: &str, create: bool) -> Res<EngineCore> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    if create {
        for t in [GIN_TABLE, PLAIN_TABLE] {
            // 既存なら失敗するだけなので無視する（作成後の読み書きで実在を確かめる）。
            let _ = storage.create_table(&TableSchema::new(
                t,
                vec![ColumnDef::new("doc", ColumnType::Jsonb, true)],
            ));
        }
    }
    Ok(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ))
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Res<SqlOutcome> {
    let c = ctx(tenant)?;
    let mut s = ddl_session();
    core.execute_sql_in_session(&c, &mut s, sql)
        .map_err(|e| format!("statement failed ({})", e.wire_code()))
}

/// 1 テナントの文を 1 つの明示 txn で実行する（失敗時は ROLLBACK を試みて失敗を返す）。
fn txn(core: &EngineCore, tenant: &str, stmts: &[String]) -> Res<()> {
    let c = ctx(tenant)?;
    let mut s = ddl_session();
    let mut t = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut t, "BEGIN")
        .map_err(|e| format!("begin failed ({})", e.wire_code()))?;
    for sql in stmts {
        if let Err(e) = core.execute_sql_in_txn(&c, &mut s, &mut t, sql) {
            let _ = core.execute_sql_in_txn(&c, &mut s, &mut t, "ROLLBACK");
            return Err(format!("statement in txn failed ({})", e.wire_code()));
        }
    }
    core.execute_sql_in_txn(&c, &mut s, &mut t, "COMMIT")
        .map_err(|e| format!("commit failed ({})", e.wire_code()))?;
    Ok(())
}

/// （行 id, doc 文字列）を `SCAN_LIMIT` 件に達しない範囲で全件読む。
fn rows_of(core: &EngineCore, tenant: &str, sql: &str) -> Res<Vec<(u64, String)>> {
    match exec(core, tenant, sql)? {
        SqlOutcome::Query(q) => {
            if q.rows.len() as u64 >= SCAN_LIMIT {
                return Err("scan limit reached; cannot see all rows".to_string());
            }
            q.rows
                .iter()
                .map(|r| match (r.cells.first(), r.cells.get(1)) {
                    (Some(Cell::Integer(id)), Some(Cell::Json(d) | Cell::Text(d))) => {
                        Ok((*id, d.clone()))
                    }
                    _ => Err("unexpected row shape".to_string()),
                })
                .collect()
        }
        _ => Err("expected a query outcome".to_string()),
    }
}

fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Res<Vec<String>> {
    match exec(core, tenant, &format!("EXPLAIN {sql}"))? {
        SqlOutcome::Explain(r) => Ok(r
            .rows
            .iter()
            .filter_map(|row| match row.cells.first() {
                Some(Cell::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect()),
        _ => Err("expected an explain outcome".to_string()),
    }
}

/// id と所有者から決定的に作る doc（共通キー・テナント固有トークン・入れ子・配列を含む）。
fn doc_for(tenant: &str, id: u64, updated: bool) -> String {
    let upd = if updated { r#","upd":1"# } else { "" };
    format!(
        r#"{{"owner":"{tenant}","shared":true,"tag":"t{}","n":{{"id":{id}}},"list":["a","b{}"]{upd}}}"#,
        id % 7,
        id % 5
    )
}

fn insert_stmts(id: u64, tenant: &str) -> [String; 2] {
    let doc = doc_for(tenant, id, false);
    [GIN_TABLE, PLAIN_TABLE].map(|t| format!("INSERT INTO {t} (id, doc) VALUES ({id}, '{doc}')"))
}

fn alive_and_max(core: &EngineCore) -> Res<(u64, u64)> {
    let mut alive = 0u64;
    let mut max_id = 0u64;
    for t in TENANTS {
        let rows = rows_of(
            core,
            t,
            &format!("SELECT id, doc FROM {PLAIN_TABLE} LIMIT {SCAN_LIMIT}"),
        )?;
        alive += rows.len() as u64;
        max_id = max_id.max(rows.iter().map(|(id, _)| *id).max().unwrap_or(0));
    }
    Ok((alive, max_id))
}

fn declare_indexes(core: &EngineCore) -> Res<()> {
    for (name, class) in INDEXES {
        // 既に宣言済みなら IF NOT EXISTS で no-op。
        exec(
            core,
            "crash-gin-owner",
            &format!("CREATE INDEX IF NOT EXISTS {name} ON {GIN_TABLE} USING gin (doc {class})"),
        )?;
    }
    Ok(())
}

// ---------- write ----------

fn run_write(path: &str, mode: &str) -> i32 {
    let result = match mode {
        "dml" => write_dml(path),
        "ddl" => write_ddl(path),
        _ => Err("unknown mode".to_string()),
    };
    match result {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: write failed: {reason}");
            1
        }
    }
}

fn committed(label: &str, n: u64, alive: u64) {
    println!("COMMITTED {label}={n} rows={alive}");
    let _ = std::io::stdout().flush();
}

fn write_dml(path: &str) -> Res<()> {
    let core = open_core(path, true)?;
    declare_indexes(&core)?;
    let (mut alive, max_id) = alive_and_max(&core)?;
    let mut next_id = max_id + 1;
    for n in 1..=MAX_BATCHES {
        let tenant = TENANTS[(n % TENANTS.len() as u64) as usize];
        let mut stmts = Vec::new();
        for k in 0..BATCH {
            stmts.extend(insert_stmts(next_id + k, tenant));
        }
        // 先頭行の UPDATE（旧エントリの後片付けを通す）と 2 行目の DELETE（逆引きの後片付け）。
        let upd = doc_for(tenant, next_id, true);
        for t in [GIN_TABLE, PLAIN_TABLE] {
            stmts.push(format!("UPDATE {t} SET doc = '{upd}' WHERE id = {next_id}"));
            stmts.push(format!("DELETE FROM {t} WHERE id = {}", next_id + 1));
        }
        txn(&core, tenant, &stmts)?;
        alive += BATCH - 1;
        next_id += BATCH;
        committed("batch", n, alive);
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    Ok(())
}

fn write_ddl(path: &str) -> Res<()> {
    let core = open_core(path, true)?;
    let (mut alive, max_id) = alive_and_max(&core)?;
    if alive == 0 {
        // 先に行を入れ、索引は宣言しない（以降の CREATE INDEX が既存行から構築する）。
        let mut next_id = max_id + 1;
        for t in TENANTS {
            for chunk in 0..(DDL_SEED_PER_TENANT / 50) {
                let mut stmts = Vec::new();
                for k in 0..50 {
                    stmts.extend(insert_stmts(next_id + chunk * 50 + k, t));
                }
                txn(&core, t, &stmts)?;
            }
            next_id += DDL_SEED_PER_TENANT;
        }
        alive = TENANTS.len() as u64 * DDL_SEED_PER_TENANT;
    }
    committed("ddl", 0, alive);
    for n in 1..=MAX_DDL_CYCLES {
        for (name, class) in INDEXES {
            exec(
                &core,
                "crash-gin-owner",
                &format!("DROP INDEX IF EXISTS {name}"),
            )?;
            exec(
                &core,
                "crash-gin-owner",
                &format!("CREATE INDEX {name} ON {GIN_TABLE} USING gin (doc {class})"),
            )?;
        }
        committed("ddl", n, alive);
    }
    Ok(())
}

// ---------- verify ----------

/// 監査オラクル: 行を持つテナントは `marker_valid && missing == 0 && stale == 0`。
fn check_audit(core: &EngineCore, tenant: &str, index: &str, has_rows: bool) -> Res<()> {
    let a = core
        .verify_gin_index_read_only(&ctx(tenant)?, index)
        .map_err(|_| "audit failed".to_string())?;
    if a.missing != 0 {
        return Err(format!(
            "missing postings: index={index} missing={}",
            a.missing
        ));
    }
    if has_rows && !a.marker_valid {
        return Err(format!(
            "marker invalid for a tenant with rows: index={index}"
        ));
    }
    if a.stale != 0 {
        return Err(format!("stale entries: index={index} stale={}", a.stale));
    }
    Ok(())
}

/// 演算子ごとの固定クエリ（`g` 上の WHERE 句）。`max_id` はそのテナントの最終行 id。
fn read_queries(max_id: u64) -> Vec<String> {
    vec![
        r#"doc @> '{"shared":true}'"#.to_string(),
        "doc ? 'shared'".to_string(),
        "doc ?| '{upd,tag}'".to_string(),
        "doc ?& '{shared,list}'".to_string(),
        r#"doc @@ '$.tag == "t3"'"#.to_string(),
        format!(r#"doc @? '$.n ? (@.id == {max_id})'"#),
        format!(r#"doc @> '{{"n":{{"id":{max_id}}}}}'"#),
        r#"doc @> '{"upd":1}'"#.to_string(),
    ]
}

/// 読み取り経路のオラクル。`g` と `p` の結果がバイト単位で一致し、他テナントの行が現れない。
/// `both_declared` なら EXPLAIN が索引経路であることも確かめる。戻り値は実行した問い合わせ数。
fn check_reads(core: &EngineCore, tenant: &str, both_declared: bool) -> Res<usize> {
    let all = rows_of(
        core,
        tenant,
        &format!("SELECT id, doc FROM {PLAIN_TABLE} LIMIT {SCAN_LIMIT}"),
    )?;
    let max_id = all.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let token = format!(r#""owner":"{tenant}""#);
    let mut n = 0;
    for w in read_queries(max_id) {
        let lim = SCAN_LIMIT;
        let sql_g = format!("SELECT id, doc FROM {GIN_TABLE} WHERE {w} LIMIT {lim}");
        let sql_p = format!("SELECT id, doc FROM {PLAIN_TABLE} WHERE {w} LIMIT {lim}");
        let on_g = rows_of(core, tenant, &sql_g)?;
        let on_p = rows_of(core, tenant, &sql_p)?;
        if on_g != on_p {
            return Err("index read differs from the full scan".to_string());
        }
        if on_g.iter().any(|(_, d)| !d.contains(&token)) {
            return Err("a row of another tenant is visible".to_string());
        }
        if both_declared {
            let lines = explain_lines(core, tenant, &sql_g)?;
            if !lines
                .iter()
                .any(|l| l.starts_with("scalar_plan: index_gin index="))
            {
                return Err("read did not use the gin index".to_string());
            }
        }
        n += 1;
    }
    Ok(n)
}

/// 行の健全性。`g` と `p` の行集合が一致する。戻り値は（`g` の行数, 行の有無）。
fn check_rows(core: &EngineCore, tenant: &str) -> Res<usize> {
    let g = rows_of(
        core,
        tenant,
        &format!("SELECT id, doc FROM {GIN_TABLE} LIMIT {SCAN_LIMIT}"),
    )?;
    let p = rows_of(
        core,
        tenant,
        &format!("SELECT id, doc FROM {PLAIN_TABLE} LIMIT {SCAN_LIMIT}"),
    )?;
    let mut g_sorted = g.clone();
    let mut p_sorted = p;
    g_sorted.sort();
    p_sorted.sort();
    if g_sorted != p_sorted {
        return Err("row sets differ between g and p".to_string());
    }
    Ok(g.len())
}

/// raw redb で `user_gin/` から始まる物理テーブル名を列挙する（索引の内部名を決め打ちしない）。
fn physical_gin_tables(path: &str) -> Res<Vec<String>> {
    let db = redb::Database::open(path).map_err(|_| "raw open failed".to_string())?;
    let r = db.begin_read().map_err(|_| "raw read failed".to_string())?;
    let tables = r
        .list_tables()
        .map_err(|_| "list tables failed".to_string())?;
    Ok(tables
        .map(|h| h.name().to_string())
        .filter(|n| n.starts_with("user_gin/"))
        .collect())
}

fn verify(path: &str, mode: &str) -> Res<u64> {
    if mode != "dml" && mode != "ddl" {
        return Err("unknown mode".to_string());
    }
    let core = open_core(path, false)?;
    let mut total = 0u64;
    let mut has_rows = [false; TENANTS.len()];
    for (i, t) in TENANTS.iter().enumerate() {
        let n = check_rows(&core, t)?;
        has_rows[i] = n > 0;
        total += n as u64;
    }
    if total == 0 {
        return Err("no rows".to_string());
    }
    // 宣言の有無: 監査が成功すれば宣言済み。`IndexNotFound` のみ「宣言なし」とみなし、
    // それ以外のエラー（カタログ破損・読み取り失敗等）は検証失敗として伝播する（fail-closed）。
    let mut declared: Vec<bool> = Vec::with_capacity(INDEXES.len());
    for (name, _) in INDEXES.iter() {
        let c = ctx(TENANTS[0])?;
        match core.verify_gin_index_read_only(&c, name) {
            Ok(_) => declared.push(true),
            Err(engine::tenant::TenantWriteError::Catalog(
                engine::catalog::CatalogError::IndexNotFound(_),
            )) => declared.push(false),
            Err(_) => return Err("declaration probe failed".to_string()),
        }
    }
    if mode == "dml" && declared.iter().any(|d| !d) {
        return Err("index declaration lost".to_string());
    }
    for ((name, _), is_declared) in INDEXES.iter().zip(&declared) {
        if !is_declared {
            continue;
        }
        for (i, t) in TENANTS.iter().enumerate() {
            check_audit(&core, t, name, has_rows[i])?;
        }
    }
    let both = declared.iter().all(|d| *d);
    let mut queries = 0;
    for t in TENANTS {
        queries += check_reads(&core, t, both)?;
    }
    if queries == 0 {
        return Err("no read queries executed".to_string());
    }
    drop(core);
    // 宣言と物理テーブルの原子性（宣言が無いのに物理テーブルが残らない／宣言があれば物理テーブルがある）。
    let physical = physical_gin_tables(path)?;
    for ((name, _), is_declared) in INDEXES.iter().zip(&declared) {
        let present = physical.iter().any(|n| *n == format!("user_gin/{name}"));
        if !*is_declared && present {
            return Err("physical gin table remains without a declaration".to_string());
        }
        if *is_declared && !present && total > 0 {
            return Err("declared index has no physical table".to_string());
        }
    }
    Ok(total)
}

// ---------- self-check ----------

fn run_self_check(path: &str) -> i32 {
    match self_check(path) {
        Ok(()) => {
            println!("SELF-CHECK ok=true");
            0
        }
        Err(reason) => {
            println!("SELF-CHECK ok=false reason={reason}");
            1
        }
    }
}

fn self_check(path: &str) -> Res<()> {
    {
        let core = open_core(path, true)?;
        declare_indexes(&core)?;
        for (i, t) in TENANTS.iter().take(2).enumerate() {
            let base = 1 + i as u64 * 10;
            let stmts: Vec<String> = (0..3).flat_map(|k| insert_stmts(base + k, t)).collect();
            txn(&core, t, &stmts)?;
        }
        // 健全な状態ではオラクルが通る（空振りでないことの前提）。
        for t in TENANTS.iter().take(2) {
            for (name, _) in INDEXES {
                check_audit(&core, t, name, true)?;
            }
            check_reads(&core, t, true)?;
        }
    }
    // テナント A のポスティングを全削除する。
    let victim = TENANTS[0];
    let removed = {
        let names = physical_gin_tables(path)?;
        let db = redb::Database::open(path).map_err(|_| "raw open failed".to_string())?;
        let w = db
            .begin_write()
            .map_err(|_| "raw write failed".to_string())?;
        let mut removed = 0usize;
        for n in &names {
            let def: redb::TableDefinition<(&str, &[u8]), &[u8]> = redb::TableDefinition::new(n);
            let mut t = w
                .open_table(def)
                .map_err(|_| "open gin table failed".to_string())?;
            let lo = [0x01u8];
            let hi = [0x02u8];
            let keys: Vec<Vec<u8>> = t
                .range((victim, lo.as_slice())..(victim, hi.as_slice()))
                .map_err(|_| "range failed".to_string())?
                .filter_map(|e| e.ok().map(|(k, _)| k.value().1.to_vec()))
                .collect();
            for k in &keys {
                t.remove((victim, k.as_slice()))
                    .map_err(|_| "remove failed".to_string())?;
                removed += 1;
            }
        }
        drop(w.commit());
        removed
    };
    if removed == 0 {
        return Err("self-check removed nothing (vacuous)".to_string());
    }
    let core = open_core(path, false)?;
    let mut detected_audit = false;
    for (name, _) in INDEXES {
        if check_audit(&core, victim, name, true).is_err() {
            detected_audit = true;
        }
        // 監査がテナント単位であること: 他テナントは clean のまま。
        check_audit(&core, TENANTS[1], name, true)
            .map_err(|_| "audit of an untouched tenant failed".to_string())?;
    }
    if !detected_audit {
        return Err("audit oracle missed deleted postings".to_string());
    }
    if check_reads(&core, victim, true).is_ok() {
        return Err("read oracle missed deleted postings".to_string());
    }
    check_reads(&core, TENANTS[1], true)
        .map_err(|_| "read oracle failed for an untouched tenant".to_string())?;
    Ok(())
}
