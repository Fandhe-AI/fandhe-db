//! カーソルの向き（`FETCH`）・`MOVE` と NO SCROLL の境界を PostgreSQL 17 の実測と突き合わせる結合テスト
//! （Issue #3337）。
//! ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-27 (1)(2)・WIRE-15、
//! `docs/spec/04-behavior/sql-surface.md` SQL-69 (5)、`docs/spec/05-tasks.md` TASK-346。
//!
//! - `tests/data/cursor_directions_pg17.tsv`: 位置（先頭の前・1・3・5・末尾の後）× 向き × SCROLL／NO SCROLL
//!   × `FETCH`／`MOVE` の PostgreSQL 17.11（Debian 17.11-1.pgdg13+2）実測。列は
//!   `scroll`・`start`（開始位置。`FETCH` で前方向に到達させる）・`stmt`・`result`
//!   （`rows:<ids>`／`move:<n>`／`err:<SQLSTATE>`）・`probe`（実行後の位置の復元用。SCROLL は
//!   `FETCH RELATIVE 0` と続く `FETCH NEXT` の id、NO SCROLL は `FETCH NEXT` の id。`-` は 0 行。
//!   エラー時は `n/a`）。
//! - 再採取の手順（採取スクリプトはリポに置かない）:
//!   1. `docker run --rm -d --name cur -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17`
//!   2. `create table t(id int); insert into t select generate_series(1,5);`
//!   3. ケースごとに新しい `BEGIN` → `DECLARE c {SCROLL|NO SCROLL} CURSOR FOR SELECT id FROM t ORDER BY id`
//!      → 開始位置 → 対象文 → プローブ → `ROLLBACK` を `psql -X -At`（`\set VERBOSITY sqlstate`）で流し、
//!      標準出力と標準エラーを分けて記録する（エラーを savepoint で回復して後続を流さない）。
//! - PG の「指定なし」カーソルは計画次第で後ろ向きを許すため TSV に載せない。本実装は指定なしを
//!   NO SCROLL と同じに扱う（意図した差。`docs/design/sql-cursor.md`）。
//! - テナント越境は 3 テナントで固定する（他テナントの行が行・件数・位置・`wire_code` に影響しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const PG17_TABLE: &str = include_str!("../data/cursor_directions_pg17.tsv");
const TABLE: &str = "documents";

type Txn<'a> = SessionTransaction<'a>;

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor-pg17");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `ids` の各 id を `lang` の印つきで `caller` のテナントへ挿入する。
fn seed_ids(engine: &EngineCore, caller: &PolicyContext, ids: &[u64], lang: &str) {
    let mut session = SessionState::default();
    for id in ids {
        engine
            .execute_sql_in_session(
                caller,
                &mut session,
                &format!(
                    "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[1.0, 0.0]', '{lang}') \
                     USING OPERATION_ID 'seed-{lang}-{id}'"
                ),
            )
            .expect("seed insert");
    }
}

fn open_txn<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    head: &str,
    select: &str,
) -> (SessionState, Txn<'a>) {
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            caller,
            &mut session,
            &mut txn,
            &format!("{head} FOR {select}"),
        )
        .expect("declare");
    (session, txn)
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

/// 文を実行して TSV の `result` 列と同じ表記（`rows:`／`move:`／`err:`）にする。
fn run_result<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut Txn<'a>,
    sql: &str,
) -> String {
    match engine.execute_sql_in_txn(caller, session, txn, sql) {
        Ok(SqlOutcome::Fetch(r)) => {
            let ids: Vec<u64> = r.rows.iter().map(|x| x.id).collect();
            format!("rows:{}", join_ids(&ids))
        }
        Ok(SqlOutcome::Move(n)) => format!("move:{n}"),
        Ok(other) => panic!("{sql}: unexpected outcome {other:?}"),
        Err(e) => format!("err:{}", e.wire_code()),
    }
}

fn probe_id<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut Txn<'a>,
    sql: &str,
) -> String {
    match engine.execute_sql_in_txn(caller, session, txn, sql) {
        Ok(SqlOutcome::Fetch(r)) => r
            .rows
            .first()
            .map_or_else(|| "-".to_string(), |x| x.id.to_string()),
        other => panic!("{sql}: {other:?}"),
    }
}

fn start_sql(start: &str) -> Option<&'static str> {
    match start {
        "0" => None,
        "1" => Some("FETCH 1 FROM c"),
        "3" => Some("FETCH 3 FROM c"),
        "5" => Some("FETCH 5 FROM c"),
        "6" => Some("FETCH ALL FROM c"),
        other => panic!("unknown start {other}"),
    }
}

/// 1 ケースを再現して `(result, probe)` を返す。`scroll_head` は `DECLARE` の修飾語部分。
fn replay<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    scroll_head: &str,
    start: &str,
    stmt: &str,
    probe_relative: bool,
) -> (String, String) {
    let (mut session, mut txn) = open_txn(
        engine,
        caller,
        &format!("DECLARE c {scroll_head} CURSOR"),
        &format!("SELECT id FROM {TABLE} LIMIT 100"),
    );
    if let Some(s) = start_sql(start) {
        engine
            .execute_sql_in_txn(caller, &mut session, &mut txn, s)
            .expect("start position");
    }
    let result = run_result(engine, caller, &mut session, &mut txn, stmt);
    if result.starts_with("err:") {
        assert_eq!(txn.status(), TransactionStatus::Failed, "{stmt}");
        return (result, "n/a".to_string());
    }
    let probe = if probe_relative {
        let a = probe_id(
            engine,
            caller,
            &mut session,
            &mut txn,
            "FETCH RELATIVE 0 FROM c",
        );
        let b = probe_id(engine, caller, &mut session, &mut txn, "FETCH NEXT FROM c");
        format!("{a},{b}")
    } else {
        probe_id(engine, caller, &mut session, &mut txn, "FETCH NEXT FROM c")
    };
    (result, probe)
}

/// TSV の全行（向き × 位置 × 修飾語 × FETCH／MOVE）を、`FROM`／`IN` と末尾 `;` の有無で再現して照合する。
/// 指定なしのカーソルは NO SCROLL と同じ結果になる（意図した差）ことも同時に確かめる。
#[test]
fn directions_match_pg17_measurements() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_ids(&engine, &caller, &[1, 2, 3, 4, 5], "ja");
    let mut n = 0;
    let mut mismatches: Vec<String> = Vec::new();
    for line in PG17_TABLE.lines().filter(|l| !l.is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 5, "{line}");
        let (scroll, start, stmt, want_result, want_probe) =
            (cols[0], cols[1], cols[2], cols[3], cols[4]);
        let head = match scroll {
            "SCROLL" => "SCROLL",
            "NO_SCROLL" => "NO SCROLL",
            other => panic!("unknown scroll {other}"),
        };
        let relative = scroll == "SCROLL";
        for variant in [
            stmt.to_string(),
            format!("{stmt};"),
            stmt.replace(" FROM c", " IN c"),
        ] {
            let (result, probe) = replay(&engine, &caller, head, start, &variant, relative);
            if (result.as_str(), probe.as_str()) != (want_result, want_probe) {
                mismatches.push(format!(
                    "{scroll} start={start} {variant}: got ({result}, {probe}) want ({want_result}, {want_probe})"
                ));
            }
            if !relative {
                // 指定なし（本実装では NO SCROLL と同じ。PG は計画次第のため TSV には載せない）。
                let (result, probe) = replay(&engine, &caller, "", start, &variant, false);
                if (result.as_str(), probe.as_str()) != (want_result, want_probe) {
                    mismatches.push(format!(
                        "unspecified start={start} {variant}: got ({result}, {probe}) want ({want_result}, {want_probe})"
                    ));
                }
            }
        }
        n += 1;
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
    assert_eq!(n, 700, "TSV の行数");
}

/// PG17 の実測と本実装が意図して食い違う形（PG17 の値・本実装の値）。理由は `docs/design/sql-cursor.md`
/// 「PG17 との意図した差」。拒否側は fail-closed で、本実装が受理側へ変えるときは本表を更新する。
const INTENDED_DIFFERENCES: &[(&str, &str, &str, &str)] = &[
    // (DECLARE または FETCH の文, PG17, 本実装, 区分)
    (
        "DECLARE c SCROLL NO SCROLL CURSOR FOR SELECT id FROM documents LIMIT 10",
        "42P11",
        "42601",
        "矛盾する修飾語",
    ),
    (
        "DECLARE c NO SCROLL SCROLL CURSOR FOR SELECT id FROM documents LIMIT 10",
        "42P11",
        "42601",
        "矛盾する修飾語",
    ),
    (
        "DECLARE c INSENSITIVE ASENSITIVE CURSOR FOR SELECT id FROM documents LIMIT 10",
        "42P11",
        "42601",
        "矛盾する修飾語",
    ),
];

/// 意図した差の表を実行して、本実装の値が表のとおりであること。
#[test]
fn intended_differences_from_pg17_are_pinned() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_ids(&engine, &caller, &[1, 2, 3], "ja");
    for (sql, pg17, ours, kind) in INTENDED_DIFFERENCES {
        assert_ne!(pg17, ours, "{kind}: 差でない行は表に置かない");
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), *ours, "{kind}: {sql}");
    }
}

/// カーソル名の重複: PG17 は `42P03`、本実装は `22000`（意図した差。fail-closed）。
#[test]
fn duplicate_cursor_name_differs_from_pg17_by_design() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_ids(&engine, &caller, &[1, 2, 3], "ja");
    let (mut session, mut txn) = open_txn(
        &engine,
        &caller,
        "DECLARE c CURSOR",
        &format!("SELECT id FROM {TABLE} LIMIT 10"),
    );
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect_err("duplicate");
    assert_ne!(err.wire_code(), "42P03");
    assert_eq!(err.wire_code(), "22000");
}

/// `WITH HOLD`（PG17 は受理）は `0A000`、`FROM`／`IN` の省略（PG17 は受理）は `42601`。
/// `BINARY`（PG17 は受理）は意図した `0A000`。
// tracked: #3340, #3333
// intentional: docs/design/sql-cursor.md
#[test]
fn forms_accepted_by_pg17_but_rejected_here_stay_fail_closed() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_ids(&engine, &caller, &[1, 2, 3], "ja");
    for (sql, code) in [
        (
            "DECLARE c CURSOR WITH HOLD FOR SELECT id FROM documents LIMIT 10",
            "0A000",
        ),
        (
            "DECLARE c BINARY CURSOR FOR SELECT id FROM documents LIMIT 10",
            "0A000",
        ),
    ] {
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), code, "{sql}");
    }
    for sql in ["FETCH c", "MOVE c"] {
        let (mut session, mut txn) = open_txn(
            &engine,
            &caller,
            "DECLARE c SCROLL CURSOR",
            &format!("SELECT id FROM {TABLE} LIMIT 10"),
        );
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

// --- 3 テナントの越境 ---------------------------------------------------------------

/// 3 テナントの行（id の範囲・`lang` の印・行数を分ける）。
const TENANTS: [(&str, &[u64], &str); 3] = [
    ("tenant-a", &[1, 2, 3], "ta"),
    ("tenant-b", &[101, 102, 103, 104, 105], "tb"),
    ("tenant-c", &[201, 202, 203, 204, 205, 206, 207], "tc"),
];

fn id_and_mark(result: &engine::sql::exec::QueryResult) -> Vec<(u64, String)> {
    result
        .rows
        .iter()
        .map(|r| {
            let mark = match r.cells.get(1) {
                Some(Cell::Text(s)) => s.clone(),
                other => panic!("lang cell: {other:?}"),
            };
            (r.id, mark)
        })
        .collect()
}

/// 同名 `c` を 3 テナントが同時に持ち、同じ操作列の結果が自テナントの行・件数だけで決まること。
/// 他テナントの行を後から足しても各テナントの結果は完全に一致する。
#[test]
fn three_tenants_see_only_their_own_rows_counts_and_positions() {
    let script: [&str; 8] = [
        "FETCH LAST FROM c",
        "FETCH ABSOLUTE -1 FROM c",
        "FETCH BACKWARD ALL FROM c",
        "FETCH FORWARD ALL FROM c",
        "MOVE BACKWARD ALL FROM c",
        "MOVE FORWARD ALL FROM c",
        "MOVE ABSOLUTE 2 FROM c",
        "FETCH PRIOR FROM c",
    ];
    let run_all = |extra_rows_present: bool| {
        let (engine, path) = new_core();
        let _cleanup = CleanupGuard(path);
        let callers: Vec<PolicyContext> = TENANTS.iter().map(|(t, _, _)| ctx(t)).collect();
        // 他テナントの行の有無を変えるため、先に a だけ・後から b・c を足す 2 通りで流す。
        seed_ids(&engine, &callers[0], TENANTS[0].1, TENANTS[0].2);
        if extra_rows_present {
            for i in 1..3 {
                seed_ids(&engine, &callers[i], TENANTS[i].1, TENANTS[i].2);
            }
        }
        let mut sessions: Vec<(SessionState, Txn<'_>)> = Vec::new();
        for caller in &callers {
            sessions.push(open_txn(
                &engine,
                caller,
                "DECLARE c SCROLL CURSOR",
                &format!("SELECT id, lang FROM {TABLE} LIMIT 100"),
            ));
        }
        // 3 セッションを交互に進める。
        let mut transcript: Vec<Vec<String>> = vec![Vec::new(); 3];
        for sql in script {
            for (i, caller) in callers.iter().enumerate() {
                let (session, txn) = &mut sessions[i];
                let line = match engine.execute_sql_in_txn(caller, session, txn, sql) {
                    Ok(SqlOutcome::Fetch(r)) => {
                        let pairs = id_and_mark(&r);
                        for (id, mark) in &pairs {
                            assert!(
                                TENANTS[i].1.contains(id) && mark == TENANTS[i].2,
                                "tenant {i} saw foreign row ({id}, {mark}) via {sql}"
                            );
                        }
                        format!(
                            "rows:{}",
                            join_ids(&pairs.iter().map(|p| p.0).collect::<Vec<_>>())
                        )
                    }
                    Ok(SqlOutcome::Move(n)) => format!("move:{n}"),
                    other => panic!("{sql}: {other:?}"),
                };
                transcript[i].push(line);
            }
        }
        transcript
    };
    let alone = run_all(false);
    let together = run_all(true);
    // a の結果は b・c の行の有無に依らない。
    assert_eq!(alone[0], together[0]);
    // a の期待値（行数 3）を手計算で固定する。
    assert_eq!(
        together[0],
        [
            "rows:3",
            "rows:3",
            "rows:2,1",
            "rows:1,2,3",
            "move:3",
            "move:3",
            "move:1",
            "rows:1",
        ]
    );
    // b（5 行）・c（7 行）の件数は自分の行数だけで決まる。
    assert_eq!(together[1][4], "move:5");
    assert_eq!(together[2][4], "move:7");
    assert_eq!(together[1][0], "rows:105");
    assert_eq!(together[2][0], "rows:207");
}

/// カーソルを持たない別セッションは、どの向きの `FETCH`／`MOVE` でも不在名と同じ `34000`（存在情報を漏らさない）。
#[test]
fn other_tenant_sessions_get_the_same_34000_for_every_direction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let callers: Vec<PolicyContext> = TENANTS.iter().map(|(t, _, _)| ctx(t)).collect();
    for (i, caller) in callers.iter().enumerate() {
        seed_ids(&engine, caller, TENANTS[i].1, TENANTS[i].2);
    }
    // a だけが `c` を持つ。
    let (_a_session, _a_txn) = open_txn(
        &engine,
        &callers[0],
        "DECLARE c SCROLL CURSOR",
        &format!("SELECT id, lang FROM {TABLE} LIMIT 100"),
    );
    for sql in [
        "FETCH NEXT FROM c",
        "FETCH PRIOR FROM c",
        "FETCH LAST FROM c",
        "FETCH ABSOLUTE 2 FROM c",
        "FETCH BACKWARD ALL FROM c",
        "MOVE BACKWARD ALL FROM c",
        "MOVE ABSOLUTE 2 FROM c",
        "MOVE 0 FROM c",
        "MOVE FORWARD ALL FROM absent_cursor",
    ] {
        for caller in &callers[1..] {
            let mut session = SessionState::default();
            let mut txn = engine.new_session_transaction();
            engine
                .execute_sql_in_txn(caller, &mut session, &mut txn, "BEGIN")
                .expect("begin");
            let err = engine
                .execute_sql_in_txn(caller, &mut session, &mut txn, sql)
                .expect_err(sql);
            assert_eq!(err.wire_code(), "34000", "{sql}");
        }
    }
}
