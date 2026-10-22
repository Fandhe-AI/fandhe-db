//! `COPY ... FROM STDIN` の `WITH` オプション適用（HEADER・DELIMITER・NULL・QUOTE・ESCAPE。
//! Issue #1764・WIRE-22・WIRE-17・INDEX-4 のポインタ）の engine API 層の結合テスト。
//! `EngineCore::begin_copy`／`CopyInSession::feed`／`finish`／`commit_copy_in` を wire-server の
//! CopyIn ハンドラと同じ順で駆動する。オプション付き COPY TO は Issue #1765 の担当のため、
//! 往復は「PostgreSQL 17 が当該オプションで出力するのと同じバイト列」を入力として取り込み、
//! SELECT で読み戻した値と一致することで確かめる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const COLS: &str = "(id, embedding, lang, note)";

fn open(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("note", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("acme", [Visibility::Public, Visibility::Private])
        .expect("ctx")
}

fn begin_session(
    core: &EngineCore,
    sql: &str,
) -> Result<engine::sql::copy::CopyInSession, SqlSurfaceError> {
    match core.begin_copy(&ctx(), &SessionState::default(), sql)? {
        CopyPlan::From(s) => Ok(s),
        CopyPlan::To(..) => panic!("expected COPY FROM plan"),
    }
}

/// `chunks` を順に取り込み、commit した行数を返す。
fn copy_in(core: &EngineCore, options: &str, chunks: &[&[u8]]) -> Result<u64, SqlSurfaceError> {
    let sql = format!("COPY docs {COLS} FROM STDIN{options}");
    let mut s = begin_session(core, &sql)?;
    for c in chunks {
        s.feed(c)?;
    }
    let batch = s.finish()?;
    Ok(core.commit_copy_in(&ctx(), batch)?.rows_affected)
}

fn note_of(core: &EngineCore, id: u64) -> Cell {
    let mut session = SessionState::default();
    let sql = format!("SELECT note FROM docs WHERE id = {id} LIMIT 1");
    let SqlOutcome::Query(r) = core
        .execute_sql_in_session(&ctx(), &mut session, &sql)
        .expect("select")
    else {
        panic!("expected Query");
    };
    r.rows
        .into_iter()
        .next()
        .expect("row")
        .cells
        .into_iter()
        .next()
        .expect("cell")
}

fn row_count(core: &EngineCore) -> usize {
    let mut session = SessionState::default();
    let SqlOutcome::Query(r) = core
        .execute_sql_in_session(&ctx(), &mut session, "SELECT id FROM docs LIMIT 100")
        .expect("select")
    else {
        panic!("expected Query");
    };
    r.rows.len()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

#[test]
fn csv_custom_delimiter_quote_escape_null_and_header() {
    let (core, _g) = open("copy-from-opt-csv-all");
    // ヘッダ行 + 区切り `;`・引用 `'`・エスケープ `\`・NULL 文字列 `NULL`。
    let data: &[u8] = b"id;embedding;lang;note\n\
        1;[1.0,0.0];ja;NULL\n\
        2;[0.0,1.0];ja;\n\
        3;[0.5,0.5];ja;'it\\'s;ok'\n\
        4;[0.5,0.5];ja;'NULL'\n\
        5;[0.5,0.5];ja;'multi\nline'\n";
    let n = copy_in(
        &core,
        " WITH (FORMAT csv, HEADER, DELIMITER ';', QUOTE '''', ESCAPE '\\', NULL 'NULL')",
        &[data],
    )
    .expect("copy ok");
    assert_eq!(n, 5, "header row must not be counted");
    assert_eq!(note_of(&core, 1), Cell::Null);
    assert_eq!(note_of(&core, 2), text(""));
    assert_eq!(note_of(&core, 3), text("it's;ok"));
    assert_eq!(note_of(&core, 4), text("NULL"));
    assert_eq!(note_of(&core, 5), text("multi\nline"));
}

#[test]
fn csv_legacy_header_and_chunk_boundaries_everywhere() {
    let (core, _g) = open("copy-from-opt-csv-bytewise");
    let data: &[u8] = b"\"id\",embedding,lang,note\n1,\"[1.0,0.0]\",ja,\"a\"\"b\"\n";
    let chunks: Vec<&[u8]> = (0..data.len()).map(|i| &data[i..=i]).collect();
    let n = copy_in(&core, " CSV HEADER", &chunks).expect("copy ok");
    assert_eq!(n, 1);
    assert_eq!(note_of(&core, 1), text("a\"b"));
}

#[test]
fn csv_escape_state_survives_chunk_boundary() {
    let (core, _g) = open("copy-from-opt-csv-esc-chunk");
    let data: &[u8] = b"1,\"[1.0,0.0]\",ja,\"x\\\"y\\\\z\"\n";
    let chunks: Vec<&[u8]> = (0..data.len()).map(|i| &data[i..=i]).collect();
    copy_in(&core, " WITH (FORMAT csv, ESCAPE '\\')", &chunks).expect("copy ok");
    assert_eq!(note_of(&core, 1), text("x\"y\\z"));
}

#[test]
fn text_custom_delimiter_null_and_escaped_delimiter() {
    let (core, _g) = open("copy-from-opt-text");
    let data: &[u8] = b"id|embedding|lang|note\n\
        1|[1.0,0.0]|ja|\n\
        2|[0.0,1.0]|ja|a\\|b\n";
    let n = copy_in(&core, " WITH (DELIMITER '|', NULL '', HEADER)", &[data]).expect("copy ok");
    assert_eq!(n, 2);
    // NULL '' のため空欄は NULL、`\|` は区切り文字そのものとして受理する。
    assert_eq!(note_of(&core, 1), Cell::Null);
    assert_eq!(note_of(&core, 2), text("a|b"));
}

#[test]
fn text_backslash_tab_is_literal_tab_with_default_delimiter() {
    let (core, _g) = open("copy-from-opt-text-tab");
    let n = copy_in(&core, "", &[b"1\t[1.0,0.0]\tja\ta\\\tb\n"]).expect("copy ok");
    assert_eq!(n, 1);
    assert_eq!(note_of(&core, 1), text("a\tb"));
}

#[test]
fn header_true_skips_header_and_matches_default_roundtrip() {
    let (core, _g) = open("copy-from-opt-header-true");
    // ヘッダの内容は検査しない（列名と無関係でもよい）。
    let n = copy_in(
        &core,
        " WITH (HEADER true)",
        &[b"whatever\there\n1\t[1.0,0.0]\tja\tn\n"],
    )
    .expect("copy ok");
    assert_eq!(n, 1);
}

#[test]
fn header_match_accepts_exact_and_rejects_mismatch_with_zero_side_effects() {
    let (core, _g) = open("copy-from-opt-header-match");
    let ok = copy_in(
        &core,
        " WITH (FORMAT csv, HEADER MATCH)",
        &[b"id,embedding,lang,note\n1,\"[1.0,0.0]\",ja,x\n"],
    )
    .expect("match ok");
    assert_eq!(ok, 1);

    for bad in [
        &b"id,embedding,lang\n2,\"[1.0,0.0]\",ja,x\n"[..],
        &b"id,embedding,lang,other\n2,\"[1.0,0.0]\",ja,x\n"[..],
        &b"ID,embedding,lang,note\n2,\"[1.0,0.0]\",ja,x\n"[..],
        &b",embedding,lang,note\n2,\"[1.0,0.0]\",ja,x\n"[..],
    ] {
        let e = copy_in(&core, " WITH (FORMAT csv, HEADER MATCH)", &[bad]).expect_err("reject");
        assert_eq!(e.wire_code(), "22000");
        assert!(!e.to_string().contains("other"));
    }
    assert_eq!(row_count(&core), 1, "rejected COPY must leave no rows");
}

#[test]
fn header_only_stream_is_rejected_as_no_data_rows() {
    let (core, _g) = open("copy-from-opt-header-only");
    let e = copy_in(&core, " WITH (HEADER)", &[b"a\tb\tc\td\n"]).expect_err("reject");
    assert_eq!(e.wire_code(), "22000");
}

#[test]
fn invalid_csv_inputs_are_22000() {
    let (core, _g) = open("copy-from-opt-csv-invalid");
    let opts = " WITH (FORMAT csv, ESCAPE '\\')";
    for bad in [
        // 引用の未終端。
        &b"1,\"[1.0,0.0]\",ja,\"abc\n"[..],
        // ESCAPE 直後でストリームが終わる。
        &b"1,\"[1.0,0.0]\",ja,\"abc\\"[..],
        // `ESCAPE != QUOTE` での閉じ引用符直後の引用符。
        &b"1,\"[1.0,0.0]\",ja,\"a\"\"b\"\n"[..],
    ] {
        let e = copy_in(&core, opts, &[bad]).expect_err("reject");
        assert_eq!(e.wire_code(), "22000", "{:?}", String::from_utf8_lossy(bad));
    }
    // text の未知エスケープ・末尾の孤立バックスラッシュ。
    for bad in [
        &b"1\t[1.0,0.0]\tja\ta\\q\n"[..],
        &b"1\t[1.0,0.0]\tja\ta\\\n"[..],
    ] {
        let e = copy_in(&core, " WITH (DELIMITER '|')", &[bad]).expect_err("reject");
        assert_eq!(e.wire_code(), "22000");
    }
    assert_eq!(row_count(&core), 0);
}

#[test]
fn csv_quote_or_escape_newline_is_0a000_before_copy_in_response() {
    let (core, _g) = open("copy-from-opt-quote-nl");
    for opt in [
        " WITH (FORMAT csv, QUOTE E'\\n')",
        " WITH (FORMAT csv, ESCAPE '\r')",
    ] {
        // E'' 文字列が未対応でも拒否側（構文・未対応）に倒れること。
        let sql = format!("COPY docs {COLS} FROM STDIN{opt}");
        assert!(begin_session(&core, &sql).is_err(), "{opt}");
    }
    let sql = format!("COPY docs {COLS} FROM STDIN WITH (FORMAT csv, QUOTE '\n')");
    let e = begin_session(&core, &sql).expect_err("reject");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn header_row_is_not_counted_as_a_data_row() {
    // ③（生 CopyData バイト量）にヘッダ行が計上されること、①（行数）に含まれないことを、
    // 既定上限の大きさに依存せず確認する: ヘッダ込みで同じストリームが HEADER 無しでも
    // 同じ総バイト量で判定される（ヘッダ有りの方が行数は 1 少ない）。
    let (core, _g) = open("copy-from-opt-header-limits");
    let n = copy_in(
        &core,
        " WITH (HEADER)",
        &[
            b"h\th\th\th\n1\t[1.0,0.0]\tja\tx\n",
            b"2\t[1.0,0.0]\tja\ty\n",
        ],
    )
    .expect("copy ok");
    assert_eq!(n, 2);
}
