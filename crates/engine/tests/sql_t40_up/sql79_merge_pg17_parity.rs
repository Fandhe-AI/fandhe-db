//! `MERGE` の WHEN 句の組み合わせを PostgreSQL 17 の実測と突き合わせる結合テスト
//! （SQL-79・Issue #3358。親 #3349、設計記録 `docs/design/sql-merge.md` §14）。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-79、`docs/spec/05-tasks.md` TASK-352。
//!
//! - `tests/data/merge_when_pg17.tsv`: PostgreSQL 17.12（Debian 17.12-1.pgdg13+1）の実測。列は
//!   `id`・`world`（`base`／`dup`）・`stmt`・`result`（`merge:<n>`／`err:<SQLSTATE>`）・`state`（実行後の
//!   対象 `t` の `k:x` をソートして空白区切り。NULL は `NULL`。エラー時は `n/a`）・`returning`
//!   （`RETURNING` 行をソートして `;` 区切り。無ければ `-`）。
//! - 世界: `t(k INTEGER, x INTEGER)` = {(1,10),(2,20),(3,30),(5,50),(NULL,60)}、
//!   `s(k INTEGER, v INTEGER)` = {(1,100),(2,200),(4,400),(6,600),(NULL,700)}。`dup` は `s` に (2,250) を足す。
//!   制約・索引は持たせない（適用順に依存しない入力にするため。設計記録 D6）。
//! - ケース: 句 16 種（MATCHED／NOT MATCHED／NOT MATCHED BY SOURCE × 条件の有無 × 操作）の長さ 1 と 2 の
//!   全順列を `base`・`dup` の両方、長さ 3（M→T→S 各 1 句）を `RETURNING` なし・ありで `base` に流す。
//! - 再採取の手順（採取スクリプトはリポに置かない）:
//!   1. `docker run --rm -d --name <一意名> -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17`
//!   2. 上記の世界を DDL と INSERT で作る。
//!   3. ケースごとに `BEGIN` → 対象文 → `SELECT k, x FROM t` → `ROLLBACK`（psycopg の既定の Tx）で流し、
//!      `rowcount`・`RETURNING` 行・`sqlstate` を記録する。
//! - 比較するのは件数・最終状態・`RETURNING` 行（多重集合）・SQLSTATE。文言は比較しない。
//! - 全ケースを明示 Tx（BEGIN → MERGE → SELECT → ROLLBACK）で流し、末尾 `;` の有無でも同じ結果になることを確かめる。
//!   失敗したケースは ROLLBACK 後の状態が初期状態のままであることも確かめる。
//! - 意図した差・未解消の差は [`INTENDED_DIFFS`] に明示する（engine 側の現行値も固定する）。

use std::collections::BTreeMap;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const PG17_TABLE: &str = include_str!("../data/merge_when_pg17.tsv");
const PG17_BOUNDARIES: &str = include_str!("../data/merge_boundaries_pg17.tsv");

/// 意図した差の除外表（ケース id → 設計記録の § と engine 側の期待値）。除外したケースは PG17 と
/// 異なる値になることも検査する（差が消えたら気づく）。現時点で除外は無い。
const INTENDED_DIFFS: &[(&str, &str, &str)] = &[
    // MERGE 固有ではなく UPDATE 等と共通の構文層の SQLSTATE 差（fail-closed の拒否であることは同じ）。
    // 統一する場合は UPDATE の SET 左辺の拒否コードと同時に直す必要があるため本 Issue では未解消。
    (
        "syn-set-qualified-lhs",
        "sql-merge.md §14 未解消: 修飾付き SET 左辺（PG17 は 42703）",
        "err:42601",
    ),
    (
        "syn-aggregate-in-cond",
        "sql-merge.md §14 未解消: WHEN 条件内の集約（PG17 は 42803）",
        "err:42601",
    ),
];

type Txn<'a> = SessionTransaction<'a>;

struct Case {
    id: String,
    world: String,
    stmt: String,
    result: String,
    state: String,
    returning: String,
}

fn parse_table() -> Vec<Case> {
    let mut cases = Vec::new();
    for line in PG17_TABLE.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 6, "bad column count: {line}");
        cases.push(Case {
            id: cols[0].to_string(),
            world: cols[1].to_string(),
            stmt: cols[2].to_string(),
            result: cols[3].to_string(),
            state: cols[4].to_string(),
            returning: cols[5].to_string(),
        });
    }
    cases
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(core: &EngineCore, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn new_world(world: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql79-merge-pg17");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    exec(&core, "CREATE TABLE t (k INTEGER, x INTEGER)");
    exec(&core, "CREATE TABLE s (k INTEGER, v INTEGER)");
    exec(
        &core,
        "INSERT INTO t (k, x) VALUES (1,10),(2,20),(3,30),(5,50),(NULL,60)",
    );
    exec(
        &core,
        "INSERT INTO s (k, v) VALUES (1,100),(2,200),(4,400),(6,600),(NULL,700)",
    );
    exec(&core, "CREATE VIEW tv AS SELECT k, x FROM t WHERE x > 15");
    exec(
        &core,
        "CREATE VIEW av AS SELECT k, count(*) AS c FROM t GROUP BY k",
    );
    if world == "dup" {
        exec(&core, "INSERT INTO s (k, v) VALUES (2,250)");
    }
    (core, CleanupGuard(path))
}

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::SignedInteger(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn column_name(c: &ColumnMeta) -> String {
    match c {
        ColumnMeta::Id => "id".to_string(),
        ColumnMeta::AliasedId { name }
        | ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::Record { name, .. } => name.clone(),
    }
}

fn state_of<'a>(core: &'a EngineCore, session: &mut SessionState, txn: &mut Txn<'a>) -> String {
    match core
        .execute_sql_in_txn(&ctx(), session, txn, "SELECT k, x FROM t LIMIT 100")
        .expect("state select")
    {
        SqlOutcome::Query(r) => {
            let mut v: Vec<String> = r
                .rows
                .iter()
                .map(|row| format!("{}:{}", cell(&row.cells[0]), cell(&row.cells[1])))
                .collect();
            v.sort();
            if v.is_empty() {
                "-".to_string()
            } else {
                v.join(" ")
            }
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// 1 ケースを BEGIN → 対象文 → 状態の SELECT → ROLLBACK で流し、TSV と同じ表記
/// `(result, state, returning)` にする。
fn replay(core: &EngineCore, stmt: &str) -> (String, String, String, String) {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, stmt);
    let triple = match out {
        Ok(SqlOutcome::Merge(o)) => (
            format!("merge:{}", o.rows_affected),
            state_of(core, &mut session, &mut txn),
            "-".to_string(),
            "-".to_string(),
        ),
        Ok(SqlOutcome::Returning(o)) => {
            let mut rows: Vec<String> = o
                .result
                .rows
                .iter()
                .map(|r| r.cells.iter().map(cell).collect::<Vec<_>>().join(","))
                .collect();
            rows.sort();
            let ret = if rows.is_empty() {
                "-".to_string()
            } else {
                rows.join(";")
            };
            (
                format!("merge:{}", o.rows_affected),
                state_of(core, &mut session, &mut txn),
                ret,
                o.result
                    .columns
                    .iter()
                    .map(column_name)
                    .collect::<Vec<_>>()
                    .join(","),
            )
        }
        Ok(other) => panic!("{stmt}: unexpected outcome {other:?}"),
        Err(e) => (
            format!("err:{}", e.wire_code()),
            "n/a".to_string(),
            "-".to_string(),
            if e.wire_code() == "21000" {
                e.client_message()
            } else {
                "-".to_string()
            },
        ),
    };
    let _ = core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK");
    triple
}

/// 全ケースの照合。不一致は集めて 1 回で panic する。
#[test]
fn when_combinations_match_pg17_measurements() {
    let cases = parse_table();
    assert!(cases.len() >= 800, "table too small: {}", cases.len());
    let mut worlds: BTreeMap<String, (EngineCore, CleanupGuard)> = BTreeMap::new();
    for w in ["base", "dup"] {
        worlds.insert(w.to_string(), new_world(w));
    }
    let mut mismatches = Vec::new();
    let mut seen_codes: BTreeMap<String, usize> = BTreeMap::new();
    for c in &cases {
        let (core, _) = worlds.get(&c.world).expect("world");
        let seed_state = {
            let mut session = SessionState::default();
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
                .expect("begin");
            let s = state_of(core, &mut session, &mut txn);
            let _ = core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK");
            s
        };
        let got = replay(core, &c.stmt);
        let got_semi = replay(core, &format!("{};", c.stmt));
        let (got, got_semi) = ((got.0, got.1, got.2), (got_semi.0, got_semi.1, got_semi.2));
        if got != got_semi {
            mismatches.push(format!(
                "{}: trailing ';' changes result: {got:?} vs {got_semi:?}",
                c.id
            ));
        }
        let want = (c.result.clone(), c.state.clone(), c.returning.clone());
        *seen_codes.entry(c.result.clone()).or_default() += 1;
        if let Some((_, section, expect)) = INTENDED_DIFFS.iter().find(|(id, _, _)| *id == c.id) {
            if got == want {
                mismatches.push(format!("{}: intended diff ({section}) disappeared", c.id));
            }
            if got.0 != *expect {
                mismatches.push(format!("{}: engine value drifted: {}", c.id, got.0));
            }
            continue;
        }
        if got != want {
            mismatches.push(format!(
                "{}: {}\n  pg17: {want:?}\n  got:  {got:?}",
                c.id, c.stmt
            ));
        }
        // 失敗・成功のどちらでも ROLLBACK 後は初期状態に戻る。
        let after = {
            let mut session = SessionState::default();
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
                .expect("begin");
            let s = state_of(core, &mut session, &mut txn);
            let _ = core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK");
            s
        };
        if after != seed_state {
            mismatches.push(format!("{}: state leaked past ROLLBACK", c.id));
        }
    }
    // vacuous でないこと: 成功・各種エラーが表に含まれる。
    for code in ["err:21000", "err:42601", "merge:0", "merge:3"] {
        assert!(seen_codes.contains_key(code), "table lacks {code}");
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches of {}:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches.join("\n")
    );
}

/// autocommit（暗黙 Tx）でも件数が TSV と一致する代表ケース。
#[test]
fn autocommit_row_counts_match_pg17_for_representative_cases() {
    let cases = parse_table();
    let mut checked = 0usize;
    for c in cases.iter().filter(|c| c.world == "base").step_by(37) {
        if !c.result.starts_with("merge:") {
            continue;
        }
        let (core, _g) = new_world("base");
        let out = core
            .execute_sql_in_session(&ctx(), &mut SessionState::default(), &c.stmt)
            .unwrap_or_else(|e| panic!("{}: {e:?}", c.stmt));
        let n = match out {
            SqlOutcome::Merge(o) => o.rows_affected,
            SqlOutcome::Returning(o) => o.rows_affected,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(format!("merge:{n}"), c.result, "{}", c.id);
        checked += 1;
    }
    assert!(checked >= 5, "too few autocommit cases: {checked}");
}

/// 確認項目の表（D2・D4・D5・D13・構文の拒否・ビュー対象）の照合。列は `id`・`world`・`stmt`・`result`・
/// `state`・`returning`・`extra`（`RETURNING` の列名の並び。`21000` は文言。無ければ `-`）。
/// `merge_boundaries_pg17.tsv` は `merge_when_pg17.tsv` と同じ採取環境・同じ世界に、`CREATE VIEW tv AS
/// SELECT k, x FROM t WHERE x > 15`（自動更新可能）と `av`（集約ビュー）を足して採取した。
#[test]
fn boundaries_match_pg17_measurements() {
    let mut worlds: BTreeMap<String, (EngineCore, CleanupGuard)> = BTreeMap::new();
    for w in ["base", "dup"] {
        worlds.insert(w.to_string(), new_world(w));
    }
    let mut mismatches = Vec::new();
    let mut n = 0usize;
    let mut intended_seen = 0usize;
    for line in PG17_BOUNDARIES.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 7, "bad column count: {line}");
        n += 1;
        let (id, world, stmt) = (cols[0], cols[1], cols[2]);
        let want = (
            cols[3].to_string(),
            cols[4].to_string(),
            cols[5].to_string(),
            cols[6].to_string(),
        );
        let (core, _) = worlds.get(world).expect("world");
        let got = replay(core, stmt);
        if let Some((_, section, expect)) = INTENDED_DIFFS.iter().find(|(i, _, _)| *i == id) {
            intended_seen += 1;
            if got == want {
                mismatches.push(format!("{id}: intended diff ({section}) disappeared"));
            }
            if got.0 != *expect {
                mismatches.push(format!("{id}: engine value drifted: {}", got.0));
            }
            continue;
        }
        if got != want {
            mismatches.push(format!("{id}: {stmt}\n  pg17: {want:?}\n  got:  {got:?}"));
        }
        let semi = replay(core, &format!("{stmt};"));
        if semi != got {
            mismatches.push(format!("{id}: trailing ';' changes result"));
        }
    }
    assert!(n >= 40, "table too small: {n}");
    assert_eq!(intended_seen, INTENDED_DIFFS.len(), "unused intended diffs");
    assert!(
        mismatches.is_empty(),
        "{} mismatches of {n}:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
