//! データ変更 CTE の 1 文内の可視性・同一行の多重変更・失敗時の原子性の結合テスト
//! （SQL-63、Issue #1877）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-63 (1)(2)(4)・
//! RECOVER-1・RECOVER-11・`docs/spec/04-behavior/rls.md` RLS-10 (b)。
//!
//! 全副文と主クエリが文の開始時点の状態を読み（互いの変更は `RETURNING` を通したときだけ見える）、
//! 同じ行を複数の副文が変更する場合は `WITH` の定義順に実行して後の副文が先の副文の書いた行に触れず、
//! どこかでエラーになれば文全体が取り消されて副作用が残らないことを、production 経路
//! （`EngineCore::execute_sql_in_session`／`execute_sql_in_txn`）で固定する。
//! RLS・権限の境界テストの網羅は #1878 の担当（本ファイルは 2 テナントでの同一応答までを確かめる）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};
// 疑似列 `id`・従来方式の表の契約（FK の参照アクション）を使うため、`CREATE TABLE` は従来方式の表作成へ振り向ける。
use crate::legacy_table;
use legacy_table::LegacyExec as _;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation id")
}

fn seed(storage: &Storage, tenant: &str, id: u64, lang: &str) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        Visibility::Private,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(format!("{tenant} {id}")),
        ],
        &op(&format!("seed-{id}")),
    )
    .expect("insert row");
}

/// alice: (1, ja)・(2, en)、bob: (3, ja) を投入した DB を開く。
fn open_with(
    name: &str,
    configure: impl FnOnce(EngineCore) -> EngineCore,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "alice", 1, "ja");
    seed(&storage, "alice", 2, "en");
    seed(&storage, "bob", 3, "ja");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (configure(core), guard)
}

fn open_seeded() -> (EngineCore, CleanupGuard) {
    open_with("sql63-dm-cte-sem", |c| c)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn query_rows(outcome: SqlOutcome) -> Vec<Vec<String>> {
    match outcome {
        SqlOutcome::Query(result) => result
            .rows
            .iter()
            .map(|r| r.cells.iter().map(|c| format!("{c:?}")).collect())
            .collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn ok_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    query_rows(run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}")))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

/// セルの Debug 表現から値だけを取り出す（`Integer(7)` → `7`、`Text("ja")` → `ja`）。
fn cell_value(cell: &str) -> String {
    match (cell.find('"'), cell.rfind('"')) {
        (Some(a), Some(b)) if b > a => cell[a + 1..b].to_string(),
        _ => cell.chars().filter(char::is_ascii_digit).collect(),
    }
}

/// テナントの可視行を「id=lang」の文字列の昇順で返す（書き込みの観測用）。
fn state(core: &EngineCore, tenant: &str) -> Vec<String> {
    let mut rows: Vec<String> = ok_rows(core, tenant, "SELECT id, lang FROM docs LIMIT 100")
        .into_iter()
        .map(|r| {
            r.iter()
                .map(|c| cell_value(c))
                .collect::<Vec<_>>()
                .join("=")
        })
        .collect();
    rows.sort();
    rows
}

/// 結果行の最初の列を数値として昇順に返す。
fn ids(rows: &[Vec<String>]) -> Vec<u64> {
    let mut out: Vec<u64> = rows
        .iter()
        .map(|r| {
            r[0].chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .expect("numeric id")
        })
        .collect();
    out.sort_unstable();
    out
}

fn insert_sub(name: &str, id: u64, lang: &str) -> String {
    format!(
        "{name} AS (INSERT INTO docs (id, embedding, lang, body) \
         VALUES ({id}, '[1,2]', '{lang}', 'b') RETURNING id)"
    )
}

// ---- 可視性 ----

#[test]
fn substatements_do_not_see_each_others_changes() {
    let (core, _g) = open_seeded();
    // a が ja を削除し、b が en を ja へ更新する。b は a の削除を、a は b の更新を見ない。
    let rows = ok_rows(
        &core,
        "alice",
        "WITH a AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id), \
         b AS (UPDATE docs SET lang = 'ja' WHERE lang = 'en' RETURNING id) \
         SELECT id FROM b LIMIT 10",
    );
    assert_eq!(ids(&rows), vec![2]);
    // 最終状態: id=1 は削除、id=2 は ja に更新（a が b の更新後の行を消さない）。
    assert_eq!(state(&core, "alice"), vec!["2=ja"]);
    assert_eq!(state(&core, "bob"), vec!["3=ja"]);
}

#[test]
fn both_returning_results_come_from_the_statement_start_state() {
    let (core, _g) = open_seeded();
    // 逆順（更新が先、削除が後）でも、後の削除は先の更新の結果（ja）を見ない。
    let rows = ok_rows(
        &core,
        "alice",
        "WITH a AS (UPDATE docs SET lang = 'ja' WHERE lang = 'en' RETURNING id), \
         b AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM b LIMIT 10",
    );
    assert_eq!(ids(&rows), vec![1]);
    assert_eq!(state(&core, "alice"), vec!["2=ja"]);
    let rows = ok_rows(
        &core,
        "alice",
        "WITH a AS (UPDATE docs SET lang = 'en' WHERE id = 2 RETURNING id), \
         b AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM a LIMIT 10",
    );
    assert_eq!(ids(&rows), vec![2]);
    assert_eq!(state(&core, "alice"), vec!["2=en"]);
}

#[test]
fn where_subqueries_are_resolved_against_the_statement_start_state() {
    let (core, _g) = open_seeded();
    // a が id=2 の lang を 'zz' にする。開始時点では 'zz' の行は無いため、b の EXISTS は偽で何も消えない。
    let rows = ok_rows(
        &core,
        "alice",
        "WITH a AS (UPDATE docs SET lang = 'zz' WHERE id = 2 RETURNING id), \
         b AS (DELETE FROM docs WHERE lang = 'ja' \
               AND EXISTS (SELECT id FROM docs WHERE lang = 'zz') RETURNING id) \
         SELECT id FROM b LIMIT 10",
    );
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(state(&core, "alice"), vec!["1=ja", "2=zz"]);
}

#[test]
fn main_query_on_a_table_reads_the_state_before_the_writes() {
    let (core, _g) = open_seeded();
    // 主クエリは副文の削除より前の状態を返す。文の後には削除が反映されている。
    let rows = ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
         SELECT id FROM docs LIMIT 10",
    );
    assert_eq!(ids(&rows), vec![1, 2]);
    assert_eq!(state(&core, "alice"), vec!["2=en"]);
    // 集計の主クエリ。
    let (core, _g) = open_seeded();
    let counted = ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT count(*) FROM docs",
    );
    assert_eq!(counted.len(), 1);
    assert!(counted[0][0].contains('2'), "{counted:?}");
    assert_eq!(state(&core, "alice"), vec!["2=en"]);
}

#[test]
fn unreferenced_substatements_are_executed() {
    let (core, _g) = open_seeded();
    // RETURNING なし・参照なしの副文も実行される（主クエリは実表を読む）。
    let rows = ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT id FROM docs LIMIT 10",
    );
    assert_eq!(ids(&rows), vec![1, 2]);
    assert_eq!(state(&core, "alice"), vec!["2=en"]);
    // RETURNING あり・参照なし、RETURNING なしの INSERT と UPDATE。
    let sql = format!(
        "WITH a AS (UPDATE docs SET lang = 'up' WHERE id = 2 RETURNING id), \
         b AS (DELETE FROM docs WHERE id = 99), {} \
         SELECT id FROM docs LIMIT 10",
        insert_sub("c", 7, "new").replace(" RETURNING id", "")
    );
    let rows = ok_rows(&core, "alice", &sql);
    assert_eq!(ids(&rows), vec![2]);
    assert_eq!(state(&core, "alice"), vec!["2=up", "7=new"]);
}

// ---- 同じ行の多重変更 ----

#[test]
fn a_later_substatement_does_not_touch_rows_changed_by_an_earlier_one() {
    // 述語形（lang）と単一行（id 指定）の両方、更新→更新・更新→削除・削除→更新。
    for (first, second, expected_state, second_ids) in [
        (
            "UPDATE docs SET lang = 'x' WHERE id = 1",
            "UPDATE docs SET lang = 'y' WHERE lang = 'ja'",
            vec!["1=x", "2=en"],
            0usize,
        ),
        (
            "UPDATE docs SET lang = 'x' WHERE id = 1",
            "UPDATE docs SET lang = 'y' WHERE id = 1",
            vec!["1=x", "2=en"],
            0,
        ),
        (
            "UPDATE docs SET lang = 'x' WHERE lang = 'ja'",
            "DELETE FROM docs WHERE id > 0",
            vec!["1=x"],
            1,
        ),
        (
            "UPDATE docs SET lang = 'x' WHERE id = 1",
            "DELETE FROM docs WHERE id = 1",
            vec!["1=x", "2=en"],
            0,
        ),
        (
            "DELETE FROM docs WHERE id = 1",
            "UPDATE docs SET lang = 'y' WHERE id > 0",
            vec!["2=y"],
            1,
        ),
        (
            "DELETE FROM docs WHERE lang = 'ja'",
            "UPDATE docs SET lang = 'y' WHERE id = 1",
            vec!["2=en"],
            0,
        ),
    ] {
        let (core, _g) = open_seeded();
        let sql = format!(
            "WITH a AS ({first} RETURNING id), b AS ({second} RETURNING id) \
             SELECT id FROM b LIMIT 10"
        );
        let rows = ok_rows(&core, "alice", &sql);
        assert_eq!(rows.len(), second_ids, "sql={sql} rows={rows:?}");
        assert_eq!(state(&core, "alice"), expected_state, "sql={sql}");
        assert_eq!(state(&core, "bob"), vec!["3=ja"], "sql={sql}");
    }
}

#[test]
fn rows_inserted_by_an_earlier_substatement_are_not_targeted_later() {
    let (core, _g) = open_seeded();
    // 先の INSERT の行は、後の DELETE（全行の述語）の対象にならず残る。
    let sql = format!(
        "WITH {}, b AS (DELETE FROM docs WHERE id > 0 RETURNING id) SELECT id FROM b LIMIT 10",
        insert_sub("a", 10, "zz")
    );
    let rows = ok_rows(&core, "alice", &sql);
    assert_eq!(ids(&rows), vec![1, 2]);
    assert_eq!(state(&core, "alice"), vec!["10=zz"]);
    // 先の INSERT の行は後の UPDATE の対象にもならない。
    let sql = format!(
        "WITH {}, b AS (UPDATE docs SET lang = 'q' WHERE id = 11 RETURNING id) \
         SELECT id FROM b LIMIT 10",
        insert_sub("a", 11, "zz")
    );
    assert!(ok_rows(&core, "alice", &sql).is_empty());
    assert_eq!(state(&core, "alice"), vec!["10=zz", "11=zz"]);
}

#[test]
fn insert_may_reuse_an_id_deleted_by_an_earlier_substatement() {
    let (core, _g) = open_seeded();
    // 削除は反映済みなので、同じ id の挿入は成功する。
    let sql = format!(
        "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), {} SELECT id FROM a LIMIT 10",
        insert_sub("b", 1, "re")
    );
    assert_eq!(ids(&ok_rows(&core, "alice", &sql)), vec![1]);
    assert_eq!(state(&core, "alice"), vec!["1=re", "2=en"]);
}

#[test]
fn two_inserts_with_the_same_id_violate_uniqueness_and_roll_back() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    let sql = format!(
        "WITH {}, {} SELECT id FROM a LIMIT 10",
        insert_sub("a", 30, "x"),
        insert_sub("b", 30, "y")
    );
    assert_eq!(code(&core, "alice", &sql), "23505");
    assert_eq!(state(&core, "alice"), before);
}

// ---- 原子性 ----

#[test]
fn a_failing_later_substatement_discards_the_earlier_writes() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    // a は id=2 を削除し、b は既存の id=1 と衝突する INSERT。文全体が取り消される。
    let sql = format!(
        "WITH a AS (DELETE FROM docs WHERE id = 2 RETURNING id), {} SELECT id FROM a LIMIT 10",
        insert_sub("b", 1, "dup")
    );
    assert_eq!(code(&core, "alice", &sql), "23505");
    assert_eq!(state(&core, "alice"), before);
    assert_eq!(state(&core, "bob"), vec!["3=ja"]);
}

#[test]
fn a_main_query_failure_after_several_writes_discards_all_of_them() {
    // 主クエリの結果行が上限（1 行）を超えて `54000` になる。副文 2 つの書き込みはどちらも残らない。
    let (core, _g) = open_with("sql63-dm-cte-sem-cap", |c| {
        c.with_result_limits(ResultLimits::new(1))
    });
    // 結果行の上限が 1 のため、観測は件数と主キー指定の 1 行読みで行う。
    let observe = |core: &EngineCore| {
        (
            ok_rows(core, "alice", "SELECT count(*) FROM docs"),
            ok_rows(core, "alice", "SELECT id FROM docs WHERE id = 1 LIMIT 1").len(),
            ok_rows(core, "alice", "SELECT id FROM docs WHERE id = 40 LIMIT 1").len(),
        )
    };
    let before = observe(&core);
    let sql = format!(
        "WITH {}, b AS (DELETE FROM docs WHERE id > 0 RETURNING id) SELECT id FROM b",
        insert_sub("a", 40, "x")
    );
    assert_eq!(code(&core, "alice", &sql), "54000");
    assert_eq!(observe(&core), before);
    // 主クエリが実表を読む形の上限超過は、副文を 1 つも実行する前に拒否される。
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH d AS (DELETE FROM docs WHERE id > 0 RETURNING id) SELECT id FROM docs"
        ),
        "54000"
    );
    assert_eq!(observe(&core), before);
}

#[test]
fn affected_row_limit_is_enforced_per_substatement() {
    use engine::sql::parser::DmlLimits;
    let (core, _g) = open_with("sql63-dm-cte-sem-affected", |c| {
        c.with_dml_limits(DmlLimits {
            max_affected_rows: std::num::NonZeroUsize::new(1),
            max_insert_rows_per_statement: std::num::NonZeroUsize::new(1000),
        })
    });
    for id in [5, 6] {
        run(
            &core,
            "alice",
            &format!(
                "INSERT INTO docs (id, embedding, lang, body) VALUES ({id}, '[1,2]', 'en', 'b')"
            ),
        )
        .expect("seed");
    }
    let before = state(&core, "alice");
    // 2 つ目の副文だけが 1 行を超える（3 行を更新）。1 つ目の削除（1 行）も取り消される。
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH a AS (DELETE FROM docs WHERE id = 1 RETURNING id), \
             b AS (UPDATE docs SET lang = 'x' WHERE id > 1 RETURNING id) \
             SELECT id FROM a LIMIT 10"
        ),
        "54000"
    );
    assert_eq!(state(&core, "alice"), before);
}

#[test]
fn explicit_transaction_failure_marks_it_failed_and_commit_keeps_nothing() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    let sql = format!(
        "WITH a AS (DELETE FROM docs WHERE id = 2 RETURNING id), {} SELECT id FROM a LIMIT 10",
        insert_sub("b", 1, "dup")
    );
    let err = tx.run(&sql).expect_err("duplicate id");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(tx.txn.status(), TransactionStatus::Failed);
    tx.ok("ROLLBACK");
    assert_eq!(state(&core, "alice"), before);
}

#[test]
fn explicit_transaction_sees_its_own_uncommitted_rows_in_the_statement() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("INSERT INTO docs (id, embedding, lang, body) VALUES (30, '[1,2]', 'ja', 'b')");
    // 先行文の未 commit の行は、主クエリにも副文の候補にも見える。
    let rows = query_rows(tx.ok(
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM docs LIMIT 10",
    ));
    assert_eq!(ids(&rows), vec![1, 2, 30]);
    let inside = query_rows(tx.ok("SELECT id FROM docs LIMIT 100"));
    assert_eq!(ids(&inside), vec![2]);
    tx.ok("ROLLBACK");
    assert_eq!(state(&core, "alice"), before);
}

// ---- 台帳 ----

#[test]
fn several_substatements_record_one_ledger_entry_and_resend_runs_nothing() {
    let (core, _g) = open_seeded();
    let sql = "WITH a AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id), \
               b AS (UPDATE docs SET lang = 'x' WHERE lang = 'en') \
               SELECT id FROM a LIMIT 10 USING OPERATION_ID 'dm-multi'";
    assert_eq!(ids(&ok_rows(&core, "alice", sql)), vec![1]);
    assert!(matches!(
        core.operation_recorded(&ctx("alice"), TABLE, &op("dm-multi"))
            .expect("lookup"),
        LedgerLookup::Recorded
    ));
    let after = state(&core, "alice");
    assert_eq!(after, vec!["2=x"]);
    // 再送は何も実行せず 23505。
    run(
        &core,
        "alice",
        "INSERT INTO docs (id, embedding, lang, body) VALUES (50, '[1,2]', 'ja', 'b')",
    )
    .expect("seed");
    assert_eq!(code(&core, "alice", sql), "23505");
    assert_eq!(state(&core, "alice"), vec!["2=x", "50=ja"]);
}

#[test]
fn ledgered_mode_accepts_substatements_with_where_subqueries() {
    let (core, _g) = open_with("sql63-dm-cte-sem-ledgered", |c| {
        c.with_ledger_mode(LedgerMode::Ledgered)
    });
    // 副文の WHERE のサブクエリの事前解決が、副文用の台帳モード（省略可）で行われる。
    let rows = ok_rows(
        &core,
        "alice",
        "WITH a AS (DELETE FROM docs WHERE id IN (SELECT id FROM docs WHERE lang = 'ja') \
         RETURNING id) SELECT id FROM a LIMIT 10 USING OPERATION_ID 'dm-sub-1'",
    );
    assert_eq!(ids(&rows), vec![1]);
    assert_eq!(state(&core, "alice"), vec!["2=en"]);
}

// ---- 形のゲートと記述 ----

#[test]
fn gated_shapes_fail_before_any_write_with_the_same_response_for_every_tenant() {
    let (core, _g) = open_seeded();
    let before = (state(&core, "alice"), state(&core, "bob"));
    for sql in [
        // ON CONFLICT 付き INSERT 副文が他の副文と同じ表を対象にする
        "WITH a AS (INSERT INTO docs (id, embedding, lang, body) \
         VALUES (1, '[1,2]', 'zz', 'b') ON CONFLICT (id) DO NOTHING RETURNING id), \
         b AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) SELECT id FROM b LIMIT 10",
        // JOIN の主クエリ
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
         SELECT d.id FROM d JOIN docs ON d.id = docs.id LIMIT 10",
        // RETURNING なしの副文を参照
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT id FROM d LIMIT 10",
    ] {
        let a = run(&core, "alice", sql).expect_err("alice must stop");
        let b = run(&core, "bob", sql).expect_err("bob must stop");
        assert_eq!(a.wire_code(), "0A000", "sql={sql}");
        assert_eq!(a.to_string(), b.to_string(), "sql={sql}");
    }
    assert_eq!((state(&core, "alice"), state(&core, "bob")), before);
}

#[test]
fn a_main_query_reading_a_temp_table_never_writes() {
    let (core, _g) = open_seeded();
    let mut session = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (n BIGINT)")
        .expect("create temp table");
    let before = state(&core, "alice");
    let err = core
        .execute_sql_in_session(
            &c,
            &mut session,
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT n FROM tt LIMIT 10",
        )
        .expect_err("temp table in the main query is not supported");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(state(&core, "alice"), before);
}

#[test]
fn several_substatements_on_a_table_with_referential_actions_are_rejected() {
    let (core, _g) = open_seeded();
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("alice");
    for ddl in [
        "CREATE TABLE par (k BIGINT PRIMARY KEY)",
        "CREATE TABLE kid (pk BIGINT REFERENCES par (k) ON DELETE CASCADE)",
    ] {
        core.execute_sql_in_session_legacy(&c, &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    core.execute_sql_in_session(&c, &mut session, "INSERT INTO par (id, k) VALUES (1, 10)")
        .expect("parent row");
    core.execute_sql_in_session(&c, &mut session, "INSERT INTO kid (id, pk) VALUES (1, 10)")
        .expect("child row");
    let err = core
        .execute_sql_in_session(
            &c,
            &mut session,
            "WITH a AS (DELETE FROM par WHERE k = 10 RETURNING id), \
             b AS (UPDATE kid SET pk = 10 WHERE id = 1 RETURNING id) SELECT id FROM a LIMIT 10",
        )
        .expect_err("referential actions need an ordering guarantee");
    assert_eq!(err.wire_code(), "0A000");
    let rows = core
        .execute_sql_in_session(&c, &mut session, "SELECT id FROM par LIMIT 10")
        .expect("parent row survives");
    assert_eq!(query_rows(rows).len(), 1);
    // 副文が 1 つなら従来どおり実行される（参照アクションの子行も同じ txn で消える）。
    core.execute_sql_in_session(
        &c,
        &mut session,
        "WITH a AS (DELETE FROM par WHERE k = 10 RETURNING id) SELECT id FROM a LIMIT 10",
    )
    .expect("single substatement still runs");
    let rows = core
        .execute_sql_in_session(&c, &mut session, "SELECT id FROM kid LIMIT 10")
        .expect("child rows");
    assert!(query_rows(rows).is_empty());
}

#[test]
fn describe_of_a_table_reading_main_query_matches_execute_and_writes_nothing() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    let sql = "WITH a AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id), \
               b AS (UPDATE docs SET lang = 'x' WHERE id = 2) SELECT id, lang FROM docs LIMIT 10";
    let session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse must not write");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(state(&core, "alice"), before, "describe must not write");
    let outcome = run(&core, "alice", sql).expect("execute");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query");
    };
    assert_eq!(described.len(), result.columns.len());
    assert_eq!(format!("{described:?}"), format!("{:?}", result.columns));
}

#[test]
fn writes_only_affect_the_session_tenant_with_several_substatements() {
    let (core, _g) = open_seeded();
    let bob_before = state(&core, "bob");
    let rows = ok_rows(
        &core,
        "alice",
        "WITH a AS (DELETE FROM docs WHERE id > 0 RETURNING id), \
         b AS (UPDATE docs SET lang = 'x' WHERE id > 0 RETURNING id) SELECT id FROM a LIMIT 100",
    );
    assert_eq!(ids(&rows), vec![1, 2]);
    assert!(state(&core, "alice").is_empty());
    assert_eq!(state(&core, "bob"), bob_before);
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    fn new(core: &'e EngineCore, tenant: &str) -> Self {
        Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed in the transaction: {e:?}"))
    }
}
