//! `COPY ... FROM STDIN`／`COPY (...) TO STDOUT`（Issue #939・WIRE-17・
//! TASK-220）の engine API 層の結合テスト。wire-server 側のメッセージ層
//! （CopyIn／CopyOut サブプロトコル）は対象外（`crates/wire-server/tests/
//! wire17_copy.rs` の担当）。`EngineCore::begin_copy`／`commit_copy_in` を
//! wire-server が呼ぶのと同じ形（`feed` を CopyData 相当のチャンクへ分けて
//! 呼び、`finish` → `commit_copy_in` の順）で駆動する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("note", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn read_back_ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let policy = ctx(tenant);
    let mut session = SessionState::default();
    let sql = format!("SELECT id FROM {TABLE} LIMIT 100");
    let outcome = core
        .execute_sql_in_session(&policy, &mut session, &sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result.rows.iter().map(|row| row.id).collect()
}

const EXT_TABLE: &str = "docs_ext";
const EXT_ENUM_TYPE: &str = "mood";

/// `BOOLEAN`／`ARRAY`／`BYTEA`／`ENUM` 列を持つテーブル（Issue #939 レビュー
/// 指摘: `bind_copy_record` の該当 match アームがどのテスト（`copy_from.rs`・
/// `wire17_copy.rs`・`sql/copy.rs` 内 unit test）からもカバーされていなかった
/// ため追加。`tests/{boolean,array,bytea,enum}_column.rs` と同じ列定義流儀）。
fn extended_schema(enum_def: std::sync::Arc<engine::catalog::EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        EXT_TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new("blob", ColumnType::Bytea, true),
            ColumnDef::new("mood", ColumnType::Enum(enum_def), true),
            // `JSON`／`JSONB` 列（Issue #939 レビュー指摘: `bind_copy_record`・
            // `bound_insert_byte_len` の `Value`／`ColumnType` 網羅 match が
            // origin/main の JSON/JSONB 追加（Issue #889）に追随しておらず
            // `E0004`（non-exhaustive patterns）でビルド不能だった。本テーブルで
            // COPY 経由の束縛を固定する）。
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
            // `NUMERIC`／`UUID` 列（PR #1018 レビュー指摘対応時に発覚: origin/main
            // で追加された NUMERIC（Issue #885）・UUID（Issue #887）へ
            // `bind_copy_record`・`bound_insert_byte_len` の網羅 match が追随
            // しておらず `E0004`（non-exhaustive patterns）でビルド不能だった。
            // 本テーブルで COPY 経由の束縛を固定する）。
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("uid", ColumnType::Uuid, true),
            // `REAL`／`DOUBLE PRECISION` 列（Issue #1022 コーディネータ指摘:
            // main で追加された REAL/DOUBLE（Issue #882）へ `bind_copy_record`・
            // `bound_insert_byte_len` の網羅 match が追随しておらず `E0004`
            // でビルド不能だった。本テーブルで COPY 経由の束縛を固定する）。
            ColumnDef::new("score", ColumnType::Real, true),
            ColumnDef::new("weight", ColumnType::Double, true),
        ],
    )
}

fn open_engine_ext(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    let enum_def = storage
        .create_enum_type(EXT_ENUM_TYPE, vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    storage
        .create_table(&extended_schema(enum_def))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

/// `id = 1` の行を投影して `Cell` 列で返す（`BOOLEAN`／`ARRAY`／`BYTEA`／`ENUM`
/// の束縛結果を engine API 層で確定オラクルとして検証するため）。
fn select_ext_cells(core: &EngineCore, tenant: &str, id: u64, columns: &str) -> Vec<Cell> {
    select_cells(core, tenant, EXT_TABLE, id, columns)
}

/// `select_ext_cells` の一般化版（テーブル名を引数化）。Cursor Bugbot 指摘
/// （末尾 `\r`（CRLF ではない単独 CR）の COPY 行終端未除去）の回帰テストで
/// `TABLE`（`docs`）の `note` 列を検証するために追加。
fn select_cells(core: &EngineCore, tenant: &str, table: &str, id: u64, columns: &str) -> Vec<Cell> {
    let policy = ctx(tenant);
    let mut session = SessionState::default();
    let sql = format!("SELECT {columns} FROM {table} WHERE id = {id} LIMIT 1");
    let outcome = core
        .execute_sql_in_session(&policy, &mut session, &sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result
        .rows
        .into_iter()
        .next()
        .expect("row must exist")
        .cells
}

/// wire-server の CopyIn サブプロトコルを模した駆動ヘルパー: `chunks` を順に
/// `feed` へ渡し、最後に `finish` → `commit_copy_in` を行う。
fn run_copy_from(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    chunks: &[&[u8]],
) -> Result<engine::sql::exec::InsertOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let policy = ctx(tenant);
    let session = SessionState::default();
    let plan = core.begin_copy(&policy, &session, sql)?;
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM STDIN plan"),
    };
    for chunk in chunks {
        copy_session.feed(chunk)?;
    }
    let batch = copy_session.finish()?;
    core.commit_copy_in(&policy, batch)
}

// ---------------------------------------------------------------------
// FROM STDIN: text 形式の基本受理・原子性
// ---------------------------------------------------------------------

#[test]
fn copy_from_stdin_text_format_inserts_all_rows_atomically() {
    let (core, path) = open_engine("copy-from-text-basic");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-1'");
    // CopyData 境界をまたいで 1 行を分割し、レコード分割がチャンクを
    // またいでも正しく動作することを併せて確認する。
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0", b"]\tja\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 2);

    let mut ids = read_back_ids(&core, "acme");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn copy_from_stdin_text_format_resolves_null_marker_and_escapes() {
    let (core, path) = open_engine("copy-from-text-null-escape");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN USING OPERATION_ID 'copy-op-2'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tja\t\\N\n2\t[0.0,1.0]\tja\ta\\tb\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn copy_from_stdin_csv_format_distinguishes_null_and_empty_and_quoted_comma() {
    let (core, path) = open_engine("copy-from-csv-basic");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'copy-op-3'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1,\"[1.0,0.0]\",ja,\n2,\"[0.0,1.0]\",\"ja,jp\",\"\"\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 2);
}

/// CopyDone が「CRLF ではなく単独の `\r`」の直後に到達した場合（末尾行が
/// `\n` を伴わずにストリームが終わる）、その `\r` を除去も保持もせず
/// `22000` で fail-closed に拒否することを固定する（PostgreSQL の `COPY`
/// が text 形式の裸 CR を `literal carriage return found in data`、CSV
/// 形式の引用符なしフィールド中の裸 CR を `unquoted carriage return found
/// in data` として拒否する契約に合わせる。codex-review 指摘
/// PRRT_kwDOUAKASM6l0EjU: 「LF の無い CR は行終端と確定できず、無条件の
/// 除去は入力の無言改変になる」との指摘を受け、以前の Cursor Bugbot 対応
/// （`value\r\n` と `value\r` の格納値不一致を解消するための無条件除去）を
/// 撤回し、両レビューを満たす fail-closed 拒否へ置き換えた）。text 形式は
/// `RecordSplitter::finish`、CSV 形式（引用符なしフィールド）は
/// `CsvRecordScanner::finish` の対応する分岐を検証する。引用符付き
/// フィールド中の未終端 CRLF（`AfterQuoteCr` 状態のまま CopyDone）は
/// 既存の `copy_from_stdin_rejects_...` 系テストが fail-closed 拒否を
/// 固定済みで、本テストの対象ではない。
#[test]
fn copy_from_stdin_rejects_lone_trailing_cr_on_final_unterminated_line() {
    let (core, path) = open_engine("copy-from-text-lone-cr-finish");
    let _guard = CleanupGuard(path);

    // text 形式: 末尾行が `\n` を伴わず単独の `\r` で終わる。
    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN USING OPERATION_ID 'copy-op-cr-text'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tja\thello\r"])
        .expect_err("lone trailing CR in text data must be rejected");
    assert_eq!(err.wire_code(), "22000");

    // CSV 形式（引用符なしフィールド）: 同じく末尾行が単独の `\r` で終わる。
    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'copy-op-cr-csv'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"2,\"[0.0,1.0]\",ja,hello\r"])
        .expect_err("lone trailing CR in unquoted CSV data must be rejected");
    assert_eq!(err.wire_code(), "22000");

    // 対照: 正規の CRLF 終端（`\n` を伴う）は引き続き受理され、CR を含まない
    // 値として格納されることを確認する（fail-closed 化が CRLF 対応を
    // 巻き込んでいないことの確認）。
    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN USING OPERATION_ID 'copy-op-crlf-text'"
    );
    let outcome = run_copy_from(&core, "acme", &sql, &[b"3\t[1.0,1.0]\tja\thello\r\n"])
        .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(
        select_cells(&core, "acme", TABLE, 3, "note"),
        vec![Cell::Text("hello".to_string())]
    );

    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'copy-op-crlf-csv'"
    );
    let outcome = run_copy_from(&core, "acme", &sql, &[b"4,\"[0.0,1.0]\",ja,hello\r\n"])
        .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(
        select_cells(&core, "acme", TABLE, 4, "note"),
        vec![Cell::Text("hello".to_string())]
    );
}

/// mid-stream（レコード終端の直前ではない位置）に裸の `\r` が現れ、直後に
/// `\n` が続かない場合も同じ `22000` で拒否し、レコード末尾での挙動と
/// 一貫させることを固定する（codex-review 指摘 PRRT_kwDOUAKASM6l0EjU
/// が求めた一貫性確認）。text 形式はフィールド途中の裸 CR、CSV 形式は
/// カンマ直前・引用符なしフィールド途中の裸 CR をそれぞれ検証する。
#[test]
fn copy_from_stdin_rejects_bare_carriage_return_mid_field() {
    let (core, path) = open_engine("copy-from-bare-cr-mid-field");
    let _guard = CleanupGuard(path);

    // text 形式: フィールド途中に裸の CR（直後は通常の文字）。
    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN USING OPERATION_ID 'copy-op-cr-mid-text'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tja\thel\rlo\n"])
        .expect_err("bare CR not followed by LF in text data must be rejected");
    assert_eq!(err.wire_code(), "22000");

    // CSV 形式: 引用符なしフィールド中の裸 CR の直後がカンマ（フィールド
    // 区切り）であっても、LF が続かない限り拒否する。
    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'copy-op-cr-mid-csv'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"2,\"[0.0,1.0]\",ja,hel\r,lo\n"])
        .expect_err("bare CR immediately followed by a field separator must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn copy_from_stdin_missing_operation_id_is_rejected_before_any_row_is_visible() {
    let (core, path) = open_engine("copy-from-missing-op-id");
    let _guard = CleanupGuard(path);

    let sql = format!("COPY {TABLE} (id, embedding, lang) FROM STDIN");
    let policy = ctx("acme");
    let session = SessionState::default();
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("missing USING OPERATION_ID must be rejected");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn copy_from_stdin_rejects_invalid_row_with_zero_side_effects() {
    let (core, path) = open_engine("copy-from-invalid-row");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-4'");
    let err = run_copy_from(
        &core,
        "acme",
        &sql,
        // 2 行目の embedding 次元が宣言（2）と不一致。
        &[b"1\t[1.0,0.0]\tja\n2\t[1.0,0.0,0.0]\tja\n"],
    )
    .expect_err("dimension mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");

    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_empty_payload_is_rejected() {
    let (core, path) = open_engine("copy-from-empty");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-5'");
    let err = run_copy_from(&core, "acme", &sql, &[b""]).expect_err("empty COPY must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn copy_from_stdin_rejects_batch_over_row_count_limit() {
    let limits = engine::batch_limits::BatchLimits {
        max_files_per_batch: 2,
        ..engine::batch_limits::BatchLimits::default()
    };
    let path = unique_db_path("copy-from-row-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-6'");
    let policy = ctx("acme");
    let session = SessionState::default();
    let plan = core
        .begin_copy(&policy, &session, &sql)
        .expect("begin_copy");
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    };
    // 上限 2 行を超える 3 行目で、CopyDone（`finish`）を待たずに拒否される
    // ことを確認する。
    let err = copy_session
        .feed(b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\tja\n")
        .expect_err("row count over limit must be rejected before CopyDone");
    assert_eq!(err.wire_code(), "54000");

    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_rejects_batch_over_total_byte_limit_before_copy_done() {
    let limits = engine::batch_limits::BatchLimits {
        max_batch_total_bytes: 4,
        ..engine::batch_limits::BatchLimits::default()
    };
    let path = unique_db_path("copy-from-byte-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-7'");
    let policy = ctx("acme");
    let session = SessionState::default();
    let plan = core
        .begin_copy(&policy, &session, &sql)
        .expect("begin_copy");
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    };
    let err = copy_session
        .feed(b"1\t[1.0,0.0]\tja\n")
        .expect_err("raw CopyData byte total over limit must be rejected");
    assert_eq!(err.wire_code(), "54000");

    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_shares_operation_id_ledger_with_sql_insert() {
    let (core, path) = open_engine("copy-from-ledger-share");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let mut session = SessionState::default();
    let insert_sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'shared-op'"
    );
    core.execute_sql_in_session(&policy, &mut session, &insert_sql)
        .expect("INSERT succeeds");

    // 同じ operation_id・同じ内容の COPY 再送は台帳照合により 23505（同一内容
    // の再送）になる（複数行 INSERT・単文 INSERT と台帳キー空間を共有する
    // 契約の固定）。
    let copy_sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'shared-op'");
    let err = run_copy_from(&core, "acme", &copy_sql, &[b"1\t[1.0,0.0]\tja\n"])
        .expect_err("duplicate operation_id with same content must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn copy_from_stdin_rejects_other_tenant_id_conflict_without_leaking_existence() {
    let (core, path) = open_engine("copy-from-tenant-conflict");
    let _guard = CleanupGuard(path);

    let policy_a = ctx("acme");
    let mut session = SessionState::default();
    let insert_sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'tenant-a-op'"
    );
    core.execute_sql_in_session(&policy_a, &mut session, &insert_sql)
        .expect("INSERT succeeds");

    // 別テナントが同じ id=1 を COPY で書き込んでも、物理キーは
    // (tenant_id, id) で名前空間化されているため衝突しない（TABLE-12）。
    let copy_sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'tenant-b-op'");
    let outcome = run_copy_from(&core, "beta", &copy_sql, &[b"1\t[0.0,1.0]\tja\n"])
        .expect("different tenant may use the same row id");
    assert_eq!(outcome.rows_affected, 1);

    let mut acme_ids = read_back_ids(&core, "acme");
    acme_ids.sort_unstable();
    assert_eq!(acme_ids, vec![1]);
}

// ---------------------------------------------------------------------
// TO STDOUT: 広域取得と同一の実行本体・RLS 暗黙適用
// ---------------------------------------------------------------------

#[test]
fn copy_to_stdout_returns_only_own_tenant_rows() {
    let (core, path) = open_engine("copy-to-stdout-basic");
    let _guard = CleanupGuard(path);

    let policy_a = ctx("acme");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &policy_a,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'to-op-1'"
        ),
    )
    .expect("INSERT succeeds");
    let policy_b = ctx("beta");
    core.execute_sql_in_session(
        &policy_b,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.0,1.0]', 'ja') USING OPERATION_ID 'to-op-2'"
        ),
    )
    .expect("INSERT succeeds");

    let sql = format!("COPY (SELECT id FROM {TABLE} LIMIT 100) TO STDOUT");
    let plan = core
        .begin_copy(&policy_a, &session, &sql)
        .expect("begin_copy succeeds");
    let (format, result) = match plan {
        CopyPlan::To(f, r) => (f, r),
        CopyPlan::From(_) => panic!("expected TO plan"),
    };
    assert_eq!(format, engine::sql::allowlist::CopyFormat::Text);
    let ids: Vec<u64> = result.rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn copy_to_stdout_rejects_ranked_select() {
    let (core, path) = open_engine("copy-to-stdout-ranked");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql = format!(
        "COPY (SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10) TO STDOUT"
    );
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("ranked SELECT inside COPY (...) TO STDOUT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn copy_to_stdout_rejects_table_form() {
    let (core, path) = open_engine("copy-to-stdout-table-form");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql = format!("COPY {TABLE} TO STDOUT");
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("table-form COPY TO STDOUT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// ---------------------------------------------------------------------
// `WITH` 句の丸括弧必須化（Issue #939 codex-review 指摘の是正）:
// `WITH` を消費した直後に `(FORMAT ...)` が続かない場合、既定 `Text` として
// 誤受理せず `42601` で拒否することを固定する。
// ---------------------------------------------------------------------

#[test]
fn copy_to_stdout_rejects_with_clause_without_parens() {
    let (core, path) = open_engine("copy-to-stdout-with-no-parens");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql = format!("COPY (SELECT id FROM {TABLE} LIMIT 10) TO STDOUT WITH");
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("COPY ... TO STDOUT WITH（括弧なし）must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn copy_from_stdin_rejects_with_clause_without_parens() {
    let (core, path) = open_engine("copy-from-stdin-with-no-parens");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN WITH USING OPERATION_ID 'copy-with-no-parens'"
    );
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("COPY ... FROM STDIN WITH（括弧なし）must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// ---------------------------------------------------------------------
// FROM STDIN: BOOLEAN／ARRAY／BYTEA／ENUM 列の束縛
// （Issue #939 レビュー指摘: cc232e6 で `bind_copy_record` へ追加した該当
// match アームがどのテストからもカバーされていなかったため追加）
// ---------------------------------------------------------------------

#[test]
fn copy_from_stdin_text_format_binds_boolean_array_bytea_enum_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-text");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, flag, tags, blob, mood) FROM STDIN USING OPERATION_ID 'ext-op-1'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tt\t{ja,en}\t\\\\xdeadbeef\thappy\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "flag, tags, blob, mood");
    assert_eq!(
        cells,
        vec![
            Cell::Bool(true),
            Cell::Array(engine::row_codec::ArrayValue::Text(vec![
                "ja".to_string(),
                "en".to_string()
            ])),
            Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Cell::Text("happy".to_string()),
        ]
    );
}

#[test]
fn copy_from_stdin_csv_format_binds_boolean_array_bytea_enum_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-csv");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, flag, tags, blob, mood) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'ext-op-2'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1,\"[1.0,0.0]\",false,\"{ja,en}\",\\xdeadbeef,sad\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "flag, tags, blob, mood");
    assert_eq!(
        cells,
        vec![
            Cell::Bool(false),
            Cell::Array(engine::row_codec::ArrayValue::Text(vec![
                "ja".to_string(),
                "en".to_string()
            ])),
            Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Cell::Text("sad".to_string()),
        ]
    );
}

/// `JSON`／`JSONB` 列を COPY FROM STDIN で束縛できることを固定する
/// （Issue #939 レビュー指摘。`docs/spec` の JSON 列ビヘイビア ID は
/// TABLE-14・TASK-198・Issue #889 を参照）。`JSON` は入力テキストを
/// そのまま保持し、`JSONB` はキー順を辞書順へ正規化・空白を除去して
/// 保持する契約（`crate::json::canonicalize_jsonb_text`）を COPY 経路でも
/// 維持することを確認する。
#[test]
fn copy_from_stdin_text_format_binds_json_and_jsonb_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-json");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, doc, docb) FROM STDIN USING OPERATION_ID 'ext-op-json-1'"
    );
    // `doc`（JSON）は入力テキストをそのまま保持、`docb`（JSONB）はキー順が
    // 入れ替わって正規化されることを確認する（`{"b":1,"a":2}` → `{"a":2,"b":1}`）。
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\t{ \"b\": 1, \"a\": 2 }\t{\"b\":1,\"a\":2}\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "doc, docb");
    assert_eq!(
        cells,
        vec![
            Cell::Json("{ \"b\": 1, \"a\": 2 }".to_string()),
            Cell::Json("{\"a\":2,\"b\":1}".to_string()),
        ]
    );
}

/// `NUMERIC`／`UUID` 列を COPY FROM STDIN で束縛できることを固定する
/// （PR #1018 レビュー指摘対応時に発覚した origin/main マージ由来の
/// `bind_copy_record`／`bound_insert_byte_len` 網羅漏れの回帰テスト。
/// TABLE-13〔検討中〕・TASK-197・Issue #885・#887）。
#[test]
fn copy_from_stdin_text_format_binds_numeric_and_uuid_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-numeric-uuid");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, price, uid) FROM STDIN USING OPERATION_ID 'ext-op-numeric-uuid-1'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\t123.45\t550e8400-e29b-41d4-a716-446655440000\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "price, uid");
    let Cell::Numeric(decimal) = &cells[0] else {
        panic!("expected Cell::Numeric, got {:?}", cells[0]);
    };
    assert_eq!(decimal.to_string(), "123.45");
    assert_eq!(
        cells[1],
        Cell::Uuid(
            engine::uuid::parse_uuid_text("550e8400-e29b-41d4-a716-446655440000")
                .expect("valid uuid")
        )
    );
}

/// `NUMERIC`（桁あふれ）・`UUID`（不正文法）列の COPY 束縛エラーが、
/// INSERT／UPDATE と同じ `wire_code` 分類で拒否されることを固定する。
#[test]
fn copy_from_stdin_rejects_numeric_overflow_and_malformed_uuid() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-numeric-uuid");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, price) FROM STDIN USING OPERATION_ID 'ext-op-numeric-2'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\t99999999999.99\n"])
        .expect_err("NUMERIC(10,2) integer-part overflow must be rejected");
    assert_eq!(err.wire_code(), "22003");

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, uid) FROM STDIN USING OPERATION_ID 'ext-op-uuid-2'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tnot-a-uuid\n"])
        .expect_err("malformed UUID literal must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

/// `REAL`／`DOUBLE PRECISION` 列を COPY FROM STDIN で束縛できることを固定
/// する（Issue #1022 コーディネータ指摘対応時に発覚した main マージ由来の
/// `bind_copy_record`／`bound_insert_byte_len` 網羅漏れの回帰テスト。
/// TABLE-13・TASK-196・Issue #882）。両列とも投影は `Cell::Float` へ
/// 写像される契約（INSERT／`RETURNING` と共通・`sql/exec.rs`）。
#[test]
fn copy_from_stdin_text_format_binds_real_and_double_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-real-double");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, score, weight) FROM STDIN USING OPERATION_ID 'ext-op-real-double-1'"
    );
    let outcome = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\t1.5\t2.25\n"])
        .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "score, weight");
    assert_eq!(cells, vec![Cell::Float(1.5), Cell::Float(2.25)]);
}

/// `REAL` 列のオーバーフロー（`22003`）・`DOUBLE PRECISION` 列への文字列
/// リテラル相当の不正値（`22P02`）が INSERT と同じ `wire_code` 分類で
/// 拒否されることを固定する。
#[test]
fn copy_from_stdin_rejects_out_of_range_real_and_malformed_double() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-real-double");
    let _guard = CleanupGuard(path);

    let huge = "4".to_string() + &"0".repeat(39);
    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, score) FROM STDIN USING OPERATION_ID 'ext-op-real-2'"
    );
    let err = run_copy_from(
        &core,
        "acme",
        &sql,
        &[format!("1\t[1.0,0.0]\t{huge}\n").as_bytes()],
    )
    .expect_err("oversized REAL literal must be rejected");
    assert_eq!(err.wire_code(), "22003");

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, weight) FROM STDIN USING OPERATION_ID 'ext-op-double-2'"
    );
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tnot-a-number\n"])
        .expect_err("malformed DOUBLE PRECISION literal must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

/// 指数表記の REAL／DOUBLE 値は受理し、アンダーフロー・オーバーフローは `22003`
/// のまま拒否する（Issue #1187。PostgreSQL と整合）。
#[test]
fn copy_from_stdin_accepts_exponent_notation_and_rejects_out_of_range_exponent() {
    let (core, path) = open_engine_ext("copy-from-ext-exponent");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, score, weight) FROM STDIN USING OPERATION_ID 'ext-op-exp-1'"
    );
    run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\t1.5e3\t-2.5E-2\n"])
        .expect("exponent notation must be accepted");

    for (i, (score, weight)) in [("1e-50", "1"), ("1", "1e999")].iter().enumerate() {
        let sql = format!(
            "COPY {EXT_TABLE} (id, embedding, score, weight) FROM STDIN USING OPERATION_ID 'ext-op-exp-bad-{i}'"
        );
        let line = format!("{}\t[1.0,0.0]\t{score}\t{weight}\n", 10 + i);
        let err = run_copy_from(&core, "acme", &sql, &[line.as_bytes()])
            .expect_err("out-of-range exponent must be rejected");
        assert_eq!(err.wire_code(), "22003");
    }
}

#[test]
fn copy_from_stdin_rejects_invalid_boolean_word() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-bool");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {EXT_TABLE} (id, embedding, flag) FROM STDIN USING OPERATION_ID 'ext-op-3'");
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tmaybe\n"])
        .expect_err("invalid boolean word must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn copy_from_stdin_rejects_enum_label_outside_vocabulary() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-enum");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {EXT_TABLE} (id, embedding, mood) FROM STDIN USING OPERATION_ID 'ext-op-4'");
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tangry\n"])
        .expect_err("label outside enum vocabulary must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn copy_from_stdin_rejects_malformed_bytea_hex() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-bytea");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {EXT_TABLE} (id, embedding, blob) FROM STDIN USING OPERATION_ID 'ext-op-5'");
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tnothex\n"])
        .expect_err("malformed bytea hex literal must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

// ---------------------------------------------------------------------
// TO STDOUT: BOOLEAN／ARRAY／BYTEA／ENUM 列の投影（`Cell` 表現の確認。
// text 表現へのエンコード・往復は wire-server 側の
// `wire17_copy_to_stdout_output_round_trips_extended_column_types` が担う）
// ---------------------------------------------------------------------

#[test]
fn copy_to_stdout_projects_boolean_array_bytea_enum_columns() {
    let (core, path) = open_engine_ext("copy-to-stdout-ext");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, flag, tags, blob, mood) FROM STDIN USING OPERATION_ID 'ext-op-6'"
    );
    run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tt\t{ja,en}\t\\\\xdeadbeef\thappy\n"],
    )
    .expect("COPY FROM STDIN succeeds");

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql =
        format!("COPY (SELECT id, flag, tags, blob, mood FROM {EXT_TABLE} LIMIT 10) TO STDOUT");
    let plan = core
        .begin_copy(&policy, &session, &sql)
        .expect("begin_copy succeeds");
    let (_, result) = match plan {
        CopyPlan::To(f, r) => (f, r),
        CopyPlan::From(_) => panic!("expected TO plan"),
    };
    let row = result.rows.into_iter().next().expect("row exists");
    assert_eq!(
        row.cells,
        vec![
            Cell::Integer(1),
            Cell::Bool(true),
            Cell::Array(engine::row_codec::ArrayValue::Text(vec![
                "ja".to_string(),
                "en".to_string()
            ])),
            Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Cell::Text("happy".to_string()),
        ]
    );
}
// ---------------------------------------------------------------------
// Issue #1178・INDEX-4 ②（1 行あたり本文サイズ）・④（生成チャンク数）の境界。
// ①③ は上のテストが固定済み。②④ が CopyDone（`finish`）を待たず `feed` 中に
// 拒否され、副作用が残らないことを固定する。`BatchLimits` は環境変数
// （`VECTOR_DB_BATCH_MAX_*`）由来の既定値の揺らぎを避けるため 4 項目すべて明示する。
// ---------------------------------------------------------------------

/// 上限を明示注入した `EngineCore` を作る。
fn open_engine_with_limits(
    name: &str,
    limits: engine::batch_limits::BatchLimits,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);
    (core, guard)
}

/// 1 行の判定対象量は `VECTOR(2)` の 8 バイト + `lang` の長さ
/// （`id` 疑似列と NULL の `note` は 0）。
const ROW_BODY_LIMIT: usize = 18;

fn body_limit_limits() -> engine::batch_limits::BatchLimits {
    engine::batch_limits::BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: ROW_BODY_LIMIT,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 64,
    }
}

fn chunk_limit_limits() -> engine::batch_limits::BatchLimits {
    // ④ < ① にして ① が先に発火しないようにする。
    engine::batch_limits::BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: 1024 * 1024,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 2,
    }
}

fn copy_sql(op: &str) -> String {
    format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID '{op}'")
}

#[test]
fn copy_from_stdin_rejects_row_over_body_size_limit_before_copy_done() {
    let (core, _guard) = open_engine_with_limits("copy-from-body-over", body_limit_limits());
    let plan = core
        .begin_copy(
            &ctx("acme"),
            &SessionState::default(),
            &copy_sql("copy-op-1178-a"),
        )
        .expect("begin_copy");
    let mut session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    };
    // 3 行目の `lang` が 11 バイト（判定対象量 19 > 18）。
    let err = session
        .feed(b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\televenbytes\n")
        .expect_err("row body over limit must be rejected before CopyDone");
    assert_eq!(err.wire_code(), "54000");
    assert!(
        err.to_string().contains("body size"),
        "unexpected message: {err}"
    );
    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_row_body_at_limit_is_accepted() {
    let (core, _guard) = open_engine_with_limits("copy-from-body-at", body_limit_limits());
    // `lang` が 10 バイト（判定対象量 18 = 上限ちょうど）。
    run_copy_from(
        &core,
        "acme",
        &copy_sql("copy-op-1178-b"),
        &[b"1\t[1.0,0.0]\ttenbytes10\n"],
    )
    .expect("row exactly at body limit must be accepted");
    assert_eq!(read_back_ids(&core, "acme"), vec![1]);
}

#[test]
fn copy_from_stdin_rejects_batch_over_chunk_limit_before_copy_done() {
    let (core, _guard) = open_engine_with_limits("copy-from-chunk-over", chunk_limit_limits());
    let plan = core
        .begin_copy(
            &ctx("acme"),
            &SessionState::default(),
            &copy_sql("copy-op-1178-c"),
        )
        .expect("begin_copy");
    let mut session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    };
    let err = session
        .feed(b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\tja\n")
        .expect_err("chunk count over limit must be rejected before CopyDone");
    assert_eq!(err.wire_code(), "54000");
    assert!(
        err.to_string().contains("batch chunk count"),
        "unexpected message: {err}"
    );
    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_rows_at_chunk_limit_are_accepted() {
    let (core, _guard) = open_engine_with_limits("copy-from-chunk-at", chunk_limit_limits());
    run_copy_from(
        &core,
        "acme",
        &copy_sql("copy-op-1178-d"),
        &[b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n"],
    )
    .expect("rows exactly at chunk limit must be accepted");
    assert_eq!(read_back_ids(&core, "acme"), vec![1, 2]);
}
