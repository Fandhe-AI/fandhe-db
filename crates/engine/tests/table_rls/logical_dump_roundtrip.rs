//! 論理ダンプの往復試験（RECOVER-16・Issue #2163。ポインタ: `docs/spec/04-behavior/recovery.md` の
//! RECOVER-16、設計メモ: `docs/design/logical-dump.md` 第 13 節）。
//!
//! 個別の書き出し（#2160 の `logical_dump_schema`・#2161 の `logical_dump_data`）が部分ごとに確かめる
//! 契約を、全列型・制約・索引・VIEW・ENUM・採番・SCHEMA・関数・トリガ・ポリシーを併せ持つ 1 つの
//! fixture（`dump_fixture`）で通して確かめる。検査する契約:
//! 1. `export_logical_dump` の出力を空 DB へ流すと、再度の出力が元とバイト単位で一致し、全表・VIEW の値と
//!    採番の次の値が一致する
//! 2. 外部キー（単一・複合・自己参照・SCHEMA 越し・各種動作）も往復し、復元後も参照整合性が効く。参照先が
//!    `id` 列の外部キーは SQL 表層が拒否する。従来方式の表が外部キーを持つ DB は書き出しが
//!    `Unsupported` で全体を失敗させる（黙って省かない）
//! 3. COPY 形式と INSERT 形式のどちらでも往復する（`BY DEFAULT` の IDENTITY は INSERT でも明示値で戻る）

use engine::catalog::ddl_export::{
    LogicalDumpItem, SchemaDdlError, SchemaDdlScope, TableDataFormat, TableDataPart,
};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[path = "../../src/test_util/dump_fixture.rs"]
mod dump_fixture;

const TENANT: &str = "tenant-a";

type Infallible = std::convert::Infallible;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(TENANT), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

/// 出力の断片（スキーマ文・データ断片）。
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Ddl(String),
    Data(TableDataPart, String, String),
}

fn dump(
    core: &EngineCore,
    format: TableDataFormat,
) -> Result<Vec<Piece>, SchemaDdlError<Infallible>> {
    let mut out = Vec::new();
    core.export_logical_dump(&SchemaDdlScope::shared_and_tenant(TENANT), format, |item| {
        out.push(match item {
            LogicalDumpItem::Schema(st) => Piece::Ddl(st.sql.to_string()),
            LogicalDumpItem::Data(d) => {
                Piece::Data(d.part, d.tenant.to_string(), d.text.to_string())
            }
        });
        Ok::<(), Infallible>(())
    })?;
    Ok(out)
}

/// 断片を空 DB へ順に流し込む。COPY はヘッダから終端行までを 1 つの COPY として駆動する。
fn replay(core: &EngineCore, pieces: &[Piece]) {
    let mut copy: Option<(String, String, Vec<u8>)> = None;
    for p in pieces {
        match p {
            Piece::Ddl(sql) if sql.starts_with("RESTORE ") => core
                .apply_restore_directive(TENANT, sql)
                .unwrap_or_else(|e| panic!("restore directive must apply: {e}")),
            Piece::Ddl(sql) => ok(core, sql),
            Piece::Data(TableDataPart::Insert, _, sql) => ok(core, sql),
            Piece::Data(TableDataPart::CopyHeader, tenant, sql) => {
                copy = Some((tenant.clone(), sql.clone(), Vec::new()));
            }
            Piece::Data(TableDataPart::CopyRow, _, line) => {
                let (_, _, buf) = copy.as_mut().expect("header before rows");
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            }
            Piece::Data(TableDataPart::CopyEnd, _, _) => {
                let (tenant, sql, buf) = copy.take().expect("header before end");
                let policy = ctx(&tenant);
                let CopyPlan::From(mut s) = core
                    .begin_copy(&policy, &SessionState::default(), &sql)
                    .unwrap_or_else(|e| panic!("{sql} must begin: {e}"))
                else {
                    panic!("expected COPY FROM");
                };
                s.feed(&buf).expect("feed");
                let batch = s.finish().expect("finish");
                core.commit_copy_in(&policy, batch).expect("commit copy");
            }
        }
    }
    assert!(copy.is_none(), "unterminated COPY");
}

fn select(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

/// fixture を作った元 DB と、その出力を流した復元先 DB を返す（一時ファイルは guard が消す）。
struct Pair {
    src: EngineCore,
    dst: EngineCore,
    pieces: Vec<Piece>,
    _g: [CleanupGuard; 2],
}

fn round_trip(tag: &str, format: TableDataFormat) -> Pair {
    let p1 = unique_db_path(&format!("dump-rt-full-src-{tag}"));
    let g1 = CleanupGuard(p1.clone());
    let p2 = unique_db_path(&format!("dump-rt-full-dst-{tag}"));
    let g2 = CleanupGuard(p2.clone());
    let src = open_core(&p1);
    for sql in dump_fixture::SETUP {
        ok(&src, sql);
    }
    let pieces = dump(&src, format).expect("dump");
    let dst = open_core(&p2);
    replay(&dst, &pieces);
    Pair {
        src,
        dst,
        pieces,
        _g: [g1, g2],
    }
}

/// 定義とデータが往復で完全に戻る（再ダンプのバイト一致・全表と VIEW の値・採番の次の値）。
fn assert_full_round_trip(tag: &str, format: TableDataFormat) {
    let pair = round_trip(tag, format);
    let again = dump(&pair.dst, format).expect("redump");
    assert_eq!(pair.pieces, again, "re-dump must match byte for byte");

    // 比較が空振りしないよう、期待する断片が実際に出ていることを固定する。
    let ddl: String = pair
        .pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Ddl(s) => Some(s.as_str()),
            Piece::Data(..) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "CREATE TYPE",
        "CREATE SEQUENCE",
        "CREATE SCHEMA",
        "CREATE VIEW",
        "CREATE UNIQUE INDEX",
        "CREATE INDEX",
        "CREATE FUNCTION",
        "CREATE TRIGGER",
        "CREATE POLICY",
        "CREATE ROLE",
        "ROW LEVEL SECURITY",
        "pair_pk",
        "pair_uq",
        "parent_pair",
        "setval",
    ] {
        assert!(ddl.contains(needle), "{needle} must appear in the dump");
    }

    for sql in dump_fixture::READBACKS {
        let want = select(&pair.src, sql);
        assert!(!want.is_empty(), "{sql} must return rows in the source");
        assert_eq!(want, select(&pair.dst, sql), "{sql} differs after restore");
    }

    // 往復後の最初の採番が元と同じ値になる（setval・IDENTITY・SERIAL の次の値が戻っている）。
    for sql in dump_fixture::NEXT_VALUE_PROBES {
        ok(&pair.src, sql);
        ok(&pair.dst, sql);
    }
    for sql in dump_fixture::NEXT_VALUE_READBACKS {
        let want = select(&pair.src, sql);
        assert_eq!(want.len(), 1, "{sql}");
        assert_eq!(want, select(&pair.dst, sql), "{sql} differs after restore");
    }
}

#[test]
fn full_fixture_round_trips_through_copy() {
    assert_full_round_trip("copy", TableDataFormat::Copy);
}

#[test]
fn full_fixture_round_trips_through_insert() {
    assert_full_round_trip("insert", TableDataFormat::Insert);
}

/// 失敗の SQLSTATE と文言の識別語を直接比べる（元と復元先の一致だけでは、意図しない理由での失敗を
/// 見逃す）。
fn assert_fails_with(core: &EngineCore, label: &str, sql: &str, state: &str, needle: &str) {
    let err = run(core, sql).map(|_| ()).expect_err(sql);
    assert_eq!(err.wire_code(), state, "{label}: {sql}: {err}");
    assert!(err.to_string().contains(needle), "{label}: {sql}: {err}");
}

/// 復元した DB の制約・既定値が実際に効いている（定義が文面だけでなく機能として戻る）。
/// 各ケースは意図した制約の SQLSTATE で、元の DB と復元先の両方で失敗する。
#[test]
fn restored_constraints_and_defaults_are_enforced() {
    let pair = round_trip("enforce", TableDataFormat::Copy);
    for (sql, state, needle) in [
        // 主キー・UNIQUE（単一・複合・名前付き）・CHECK・NOT NULL。
        (
            "INSERT INTO parent (id, name) VALUES (1, 'dup-pk')",
            "23505",
            "unique",
        ),
        (
            "INSERT INTO parent (id, name) VALUES (10, 'a')",
            "23505",
            "unique",
        ),
        (
            "INSERT INTO parent (id, name, score) VALUES (11, 'neg', -1)",
            "23514",
            "parent_check",
        ),
        (
            "INSERT INTO parent (id, name, score, flag) VALUES (12, 'pairdup', 2, TRUE)",
            "23505",
            "unique",
        ),
        // 既定の name（'x''y'）は既存の行と衝突しないが、2 行目の既定は衝突する。下の成功ケースの後。
        (
            "INSERT INTO pair (a, b) VALUES (0, 'q')",
            "23514",
            "pair_ck",
        ),
        ("INSERT INTO pair (a, b) VALUES (9, 'x')", "23505", "unique"),
        (
            "INSERT INTO app.items (iid, label) VALUES (9, 'first')",
            "23505",
            "unique",
        ),
        (
            "INSERT INTO app.items (label) VALUES ('nokey')",
            "23502",
            "not-null",
        ),
        // 外部キー（列制約・表制約・複合・SCHEMA 配下・自己参照）と参照される側の削除。
        (
            "INSERT INTO member (mid, org_oid) VALUES (9, 99)",
            "23503",
            "foreign key",
        ),
        (
            "INSERT INTO member (mid, org_code) VALUES (9, 'nope')",
            "23503",
            "foreign key",
        ),
        (
            "INSERT INTO member (mid, region, seq) VALUES (9, 'eu', 2)",
            "23503",
            "foreign key",
        ),
        (
            "INSERT INTO app.notes (nid, item_label) VALUES (9, 'nope')",
            "23503",
            "foreign key",
        ),
        (
            "INSERT INTO node (nid, up) VALUES (9, 99)",
            "23503",
            "foreign key",
        ),
        ("DELETE FROM node WHERE nid = 1", "23503", "foreign key"),
    ] {
        assert_fails_with(&pair.src, "src", sql, state, needle);
        assert_fails_with(&pair.dst, "dst", sql, state, needle);
    }

    // 既定値そのものが復元後に入る: 省略した列の既定値が元の DB と一致する。
    let probe = "INSERT INTO parent (id) VALUES (13)";
    ok(&pair.src, probe);
    ok(&pair.dst, probe);
    let read = "SELECT name, score, flag, m FROM parent WHERE id = 13";
    let defaults = select(&pair.dst, read);
    assert_eq!(defaults, select(&pair.src, read));
    assert_eq!(
        defaults,
        vec![vec![
            Cell::Text("x'y".to_string()),
            Cell::Float(1.5),
            Cell::Bool(true),
            Cell::Text("ok".to_string())
        ]]
    );
    // 既定の name は UNIQUE なので、もう 1 行を既定で足すと衝突する（既定値が効いたうえでの制約）。
    assert_fails_with(
        &pair.src,
        "src",
        "INSERT INTO parent (id) VALUES (14)",
        "23505",
        "unique",
    );
    assert_fails_with(
        &pair.dst,
        "dst",
        "INSERT INTO parent (id) VALUES (14)",
        "23505",
        "unique",
    );
}

/// 参照先の列名が `id` の外部キーは、SQL 表層が暗黙 id への参照として扱い `0A000` で拒否する
/// （列 `id` を明示した表でも同じ）。そのためダンプには現れない外部キーの形で、fixture は参照先を
/// `id` 以外の列にしている。受理されるようになったら、fixture に加えて往復させる（設計メモ第 13 節）。
#[test]
fn foreign_key_to_a_column_named_id_is_rejected_so_it_never_reaches_the_dump() {
    let p = unique_db_path("dump-rt-fk-id");
    let _g = CleanupGuard(p.clone());
    let core = open_core(&p);
    ok(
        &core,
        "CREATE TABLE par (id BIGINT PRIMARY KEY, code TEXT, UNIQUE (code))",
    );
    ok(
        &core,
        "CREATE TABLE ch (cid BIGINT PRIMARY KEY, pid BIGINT)",
    );
    for sql in [
        "ALTER TABLE ch ADD CONSTRAINT fk1 FOREIGN KEY (pid) REFERENCES par (id) ON DELETE CASCADE",
        "CREATE TABLE ch2 (cid BIGINT PRIMARY KEY, pid BIGINT REFERENCES par (id))",
    ] {
        let err = run(&core, sql).map(|_| ()).expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
}

/// 従来方式（暗黙 id）の表が外部キーを持つ DB は、書き出しが `Unsupported` で全体を失敗させる。
/// 外部キーだけを黙って落として復元後に参照整合性が消える、という状態にしない（この経路では暗黙 id の
/// 表そのものが先に拒否される。外部キー文の組み立ては `ddl_export/tests.rs` の単体試験が担う）。
#[test]
fn dump_fails_closed_when_a_legacy_table_carries_a_foreign_key() {
    let p = unique_db_path("dump-rt-fk-legacy");
    let _g = CleanupGuard(p.clone());
    let core = open_core(&p);
    let policy = ctx(TENANT);
    crate::legacy_table::create_legacy_table(
        &core,
        &policy,
        "CREATE TABLE legacy_parent (code TEXT, UNIQUE (code))",
    )
    .expect("create legacy parent");
    crate::legacy_table::create_legacy_table(
        &core,
        &policy,
        "CREATE TABLE legacy_child (pid TEXT REFERENCES legacy_parent (code) ON DELETE CASCADE)",
    )
    .expect("create legacy child with a foreign key");
    let err = dump(&core, TableDataFormat::Copy)
        .expect_err("legacy tables with foreign keys cannot be written as SQL");
    match err {
        SchemaDdlError::Unsupported(u) => {
            assert!(!u.reason.contains("legacy_"), "reason must not name tables");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}
