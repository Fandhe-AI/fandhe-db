//! `wire-server dump`／`dump-restore` の往復と壊れたダンプの拒否（Issue #2163・RECOVER-16 ポインタ。
//! 方式は `docs/design/logical-dump.md` の第 12・13 節）。
//!
//! 個別の契約（枠の順序・ヘッダ整合・改ざん・テナント境界）は `dump_cli.rs` が確かめる。ここでは
//! engine の往復試験（`table_rls/logical_dump_roundtrip.rs`）と同じ網羅 fixture（`dump_fixture`）を
//! CLI の経路で往復させ、同じ fixture のダンプを各枠の境界で切り詰める・1 バイト反転させる・枠を
//! 入れ替える・枠を重複させた入力がすべて拒否され、何も公開されず一時名が残らないことを確かめる。

use crate::common;
use crate::dump_cli::{
    assert_no_temp_leftovers, code, dir_of, dump_args, read_text, run, s, stderr, Direct,
};
use crate::dump_fixture;

use std::path::Path;

/// fixture を作った DB を `db` に書く（`dump` は DB を開いているプロセスがあると拒否するため、
/// 作成後に閉じてから返す）。
fn build_fixture_db(db: &str) {
    let core = engine::core::EngineCore::open(db).expect("open db");
    let mut a = Direct::new(&core, "tenant-a");
    for sql in dump_fixture::SETUP {
        a.exec(sql);
    }
}

/// 全表・VIEW の値を読み出す（`Cell` の `Debug` 表記。往復後の比較用）。
fn read_all(db: &str) -> Vec<Vec<Vec<String>>> {
    let core = engine::core::EngineCore::open(db).expect("open db");
    let mut a = Direct::new(&core, "tenant-a");
    dump_fixture::READBACKS.iter().map(|q| a.rows(q)).collect()
}

fn round_trip_through_cli(label: &str, format: &str) {
    let fx = common::TempFixtureDir::new(label);
    let db = fx.db_path_str();
    build_fixture_db(&db);
    let dir = dir_of(&db);
    let out = s(&dir.join("a.sql")).to_string();
    let o = run(&dump_args(
        &db,
        &out,
        &["--tenant", "tenant-a", "--format", format],
    ));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&out);
    for kind in [
        "schema",
        "role",
        "enum-type",
        "sequence",
        "sequence-value",
        "table",
        "foreign-key",
        "index",
        "view",
        "materialized-view",
        "materialized-view-refresh",
        "function",
        "trigger",
        "row-security",
        "policy",
    ] {
        assert!(
            text.contains(&format!("-- @stmt {kind} ")),
            "dump lacks kind {kind}"
        );
    }

    let restored = s(&dir.join("restored.redb")).to_string();
    let o = run(&["dump-restore", "--from", &out, "--db", &restored]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    // 定義とデータ: 復元先の再ダンプが元のダンプとバイト単位で一致する。
    let again = s(&dir.join("again.sql")).to_string();
    let o = run(&dump_args(
        &restored,
        &again,
        &["--tenant", "tenant-a", "--format", format],
    ));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(read_text(&again), text);
    // 全表・VIEW の値も一致する（比較が空振りしないよう、空でないことを確認する）。
    let want = read_all(&db);
    assert!(want.iter().all(|rows| !rows.is_empty()));
    assert_eq!(want, read_all(&restored));
    assert_no_temp_leftovers(&dir);
}

/// COPY 形式（既定）で、網羅 fixture が CLI の経路で往復する。
#[test]
fn full_fixture_round_trips_through_cli_copy() {
    round_trip_through_cli("dump-rt-cli-copy", "copy");
}

/// INSERT 形式でも同じ fixture が往復する。
#[test]
fn full_fixture_round_trips_through_cli_insert() {
    round_trip_through_cli("dump-rt-cli-insert", "insert");
}

/// ダンプファイルの 1 枠（見出し行から本文末尾、COPY は終端行まで）のバイト範囲と種別。
#[derive(Debug, Clone)]
struct Frame {
    start: usize,
    end: usize,
    /// `@stmt` の種別語、COPY の枠は `copy`。
    kind: String,
}

/// ダンプ全体を、ヘッダの長さ・枠の列・終端行の開始位置に分解する。
fn parse_frames(text: &str) -> (usize, Vec<Frame>, usize) {
    let bytes = text.as_bytes();
    let body_mark = "-- body\n";
    let header_end = text.find(body_mark).expect("body mark") + body_mark.len();
    let mut pos = header_end;
    let mut frames = Vec::new();
    loop {
        let nl = text[pos..].find('\n').expect("marker line") + pos;
        let line = &text[pos..nl];
        if line == "-- end" {
            return (header_end, frames, pos);
        }
        let mut it = line.split(' ');
        assert_eq!(it.next(), Some("--"));
        let tag = it.next().expect("tag");
        let (kind, len) = match tag {
            "@stmt" => {
                let kind = it.next().expect("kind").to_string();
                (kind, it.next().expect("len").parse::<usize>().expect("len"))
            }
            "@copy" => (
                "copy".to_string(),
                it.next().expect("len").parse::<usize>().expect("len"),
            ),
            other => panic!("unexpected marker {other}"),
        };
        let start = pos;
        // 見出し行 + 本文 + ";\n"
        let mut end = nl + 1 + len + 2;
        if tag == "@copy" {
            // データ行は、終端行 `\.` まで（COPY のデータ行は改行を含まない）。
            loop {
                let l_end = text[end..].find('\n').expect("copy line") + end;
                let is_end = &text[end..l_end] == "\\.";
                end = l_end + 1;
                if is_end {
                    break;
                }
            }
        }
        assert!(end <= bytes.len());
        frames.push(Frame { start, end, kind });
        pos = end;
    }
}

/// 拒否の理由。終了コードと標準エラーの識別語を 1 組に固定し、別の理由での拒否を合格にしない。
#[derive(Clone, Copy)]
enum Reject {
    /// 途中でファイルが終わっている（終了コード 4）。
    Truncated,
    /// 枠・終端の形式違反（終了コード 4）。
    Malformed,
    /// ヘッダの形式違反（終了コード 4）。
    Header,
    /// 許可表にない文、または枠の順序違反（終了コード 4）。
    NotAllowed,
    /// 文の実行失敗（終了コード 1）。値は SQLSTATE。
    Apply(&'static str),
}

impl Reject {
    fn expected(self) -> (i32, String) {
        match self {
            Reject::Truncated => (4, "dump file is truncated".into()),
            Reject::Malformed => (4, "dump file is malformed".into()),
            Reject::Header => (4, "dump file header is invalid".into()),
            Reject::NotAllowed => (4, "is not an allowed statement".into()),
            Reject::Apply(state) => (1, format!("(sqlstate {state})")),
        }
    }
}

/// 壊れた入力で `dump-restore` を試し、`why` の理由（終了コードと標準エラーの識別語）で拒否される・
/// 何も公開されない・一時名が残らないことを確かめる。
fn assert_rejected(dir: &Path, bytes: &[u8], name: &str, why: Reject) {
    let from = s(&dir.join("variant.sql")).to_string();
    let dest = s(&dir.join("dest.redb")).to_string();
    std::fs::write(&from, bytes).expect("write variant");
    let o = run(&["dump-restore", "--from", &from, "--db", &dest]);
    let (want_code, needle) = why.expected();
    assert_eq!(code(&o), Some(want_code), "{name}: stderr: {}", stderr(&o));
    assert!(
        stderr(&o).contains(&needle),
        "{name}: stderr lacks {needle:?}: {}",
        stderr(&o)
    );
    assert!(!Path::new(&dest).exists(), "{name}: published a database");
    assert_no_temp_leftovers(dir);
}

/// 同じ名前を再び作る枠の重複が、種別ごとに返す SQLSTATE。
fn duplicate_sqlstate(kind: &str) -> &'static str {
    match kind {
        "schema" => "42P06",
        "table" | "sequence" | "index" | "view" | "materialized-view" => "42P07",
        "role" | "enum-type" | "foreign-key" | "trigger" | "policy" => "42710",
        "function" => "42723",
        other => panic!("no expected SQLSTATE for duplicated {other}"),
    }
}

/// 網羅 fixture のダンプを壊した入力は、すべて拒否される。
/// 1. 各枠の境界（と境界の 1 バイト手前）で切り詰める
/// 2. 枠の見出し・長さ・種別・本文の終端・COPY の終端・終端行、ヘッダの各行の 1 バイトを反転させる
/// 3. 枠の入れ替え（依存先より前へ出す・後段をデータより前へ出す・COPY を表より前へ出す）
/// 4. 枠の重複（種別ごとの先頭の枠・COPY／INSERT のデータ・ヘッダ）
#[test]
fn damaged_full_fixture_dumps_are_rejected_and_publish_nothing() {
    let fx = common::TempFixtureDir::new("dump-rt-damaged");
    let db = fx.db_path_str();
    build_fixture_db(&db);
    let dir = dir_of(&db);
    let good = s(&dir.join("good.sql")).to_string();
    let o = run(&dump_args(&db, &good, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let text = read_text(&good);
    let (header_end, frames, end_pos) = parse_frames(&text);
    assert!(frames.len() > 30, "fixture must produce many frames");

    // 元のダンプは受理される（以降の拒否が、環境の問題でないことの対照）。
    let ok_dest = s(&dir.join("ok.redb")).to_string();
    let o = run(&["dump-restore", "--from", &good, "--db", &ok_dest]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));

    // 1. 切り詰め: ヘッダ直後・各枠の境界（直前の枠の末尾）・境界の 1 バイト手前・終端行の手前。
    let mut cuts = vec![header_end, end_pos, end_pos - 1, end_pos + 3];
    for f in &frames {
        cuts.push(f.start);
        cuts.push(f.end - 1);
    }
    cuts.sort_unstable();
    cuts.dedup();
    for cut in cuts {
        assert_rejected(
            &dir,
            &text.as_bytes()[..cut],
            &format!("truncate@{cut}"),
            Reject::Truncated,
        );
    }

    // 2. 1 バイト反転。ヘッダの各行の先頭と、種別ごとの先頭の枠の構造部分。
    let mut flips: Vec<(String, usize, u8, Reject)> = Vec::new();
    let mut line_start = 0;
    for line in text[..header_end].split_inclusive('\n') {
        flips.push((
            format!("header-line@{line_start}"),
            line_start + 3,
            b'#',
            Reject::Header,
        ));
        line_start += line.len();
    }
    let first_of_kind = first_frame_of_each_kind(&frames);
    for f in &first_of_kind {
        let marker_end = f.start + text[f.start..].find('\n').expect("marker");
        let name = |what: &str| format!("{what}@{}:{}", f.kind, f.start);
        // 見出しの `@`、本文の終端の `;`。
        flips.push((name("at"), f.start + 3, b'#', Reject::Malformed));
        flips.push((
            name("semicolon"),
            semicolon_pos(&text, f, marker_end),
            b',',
            Reject::Malformed,
        ));
        // `@stmt` の種別語の先頭と、長さの末尾の桁。
        let marker = &text[f.start..marker_end];
        if marker.starts_with("-- @stmt ") {
            flips.push((
                name("kind"),
                f.start + "-- @stmt ".len(),
                b'X',
                Reject::Malformed,
            ));
        }
        let last_digit = marker_end - 1;
        let d = text.as_bytes()[last_digit];
        flips.push((
            name("length"),
            last_digit,
            if d == b'9' { b'8' } else { d + 1 },
            Reject::Malformed,
        ));
    }
    flips.push((
        "end-marker".to_string(),
        end_pos + 4,
        b'D',
        Reject::Malformed,
    ));
    for f in first_of_kind.iter().filter(|f| f.kind == "copy") {
        // COPY の終端行 `\.` を `\,` に。
        let dot = f.end - 2;
        assert_eq!(text.as_bytes()[dot], b'.');
        // 終端を失った COPY は、続く枠の見出し行をデータ行として読み、列数の不一致（22000）で失敗する。
        flips.push((
            format!("copy-end@{}", f.start),
            dot,
            b',',
            Reject::Apply("22000"),
        ));
    }
    for (name, pos, byte, why) in flips {
        let mut bytes = text.clone().into_bytes();
        assert_ne!(bytes[pos], byte, "{name}: flip must change the byte");
        bytes[pos] = byte;
        assert_rejected(&dir, &bytes, &format!("flip {name}"), why);
    }

    // 3. 枠の入れ替え。
    let kind_first = |k: &str| {
        frames
            .iter()
            .find(|f| f.kind == k)
            .unwrap_or_else(|| panic!("fixture lacks {k}"))
            .clone()
    };
    let slice = |f: &Frame| text[f.start..f.end].to_string();
    let first_table = kind_first("table");
    let moves: Vec<(&str, Frame)> = vec![
        ("index-before-table", kind_first("index")),
        ("view-before-table", kind_first("view")),
        (
            "materialized-view-before-table",
            kind_first("materialized-view"),
        ),
        ("foreign-key-before-table", kind_first("foreign-key")),
        ("copy-before-table", kind_first("copy")),
        ("trigger-before-table", kind_first("trigger")),
        ("policy-before-table", kind_first("policy")),
    ];
    for (name, moved) in &moves {
        let mut s2 = String::new();
        s2.push_str(&text[..first_table.start]);
        s2.push_str(&slice(moved));
        s2.push_str(&text[first_table.start..moved.start]);
        s2.push_str(&text[moved.end..]);
        assert_rejected(&dir, s2.as_bytes(), name, Reject::Apply("42P01"));
    }
    // 後段（最後の枠）を最初のデータ枠より前へ出す。
    let first_data = frames
        .iter()
        .find(|f| f.kind == "copy" || f.kind == "insert")
        .expect("data frame")
        .clone();
    let last = frames.last().expect("last frame").clone();
    let mut s2 = String::new();
    s2.push_str(&text[..first_data.start]);
    s2.push_str(&slice(&last));
    s2.push_str(&text[first_data.start..last.start]);
    s2.push_str(&text[last.end..]);
    assert_rejected(&dir, s2.as_bytes(), "post-before-data", Reject::NotAllowed);
    // トリガをその関数より前へ。
    let func = kind_first("function");
    let trig = kind_first("trigger");
    let mut s2 = String::new();
    s2.push_str(&text[..func.start]);
    s2.push_str(&slice(&trig));
    s2.push_str(&text[func.start..trig.start]);
    s2.push_str(&text[trig.end..]);
    assert_rejected(
        &dir,
        s2.as_bytes(),
        "trigger-before-function",
        Reject::Apply("42883"),
    );

    // 4. 枠の重複。すでにある名前を再び作る枠は拒否される。`sequence-value`（`setval`）と
    // `row-security`（`ENABLE`／`FORCE`）は再実行しても同じ状態になる文なので対象外にする。
    // データの枠（`copy`・`insert`）は、次の主キー違反の試験で表名を指定して確かめる。
    for f in &first_of_kind {
        if matches!(
            f.kind.as_str(),
            "sequence-value" | "materialized-view-refresh" | "row-security" | "copy" | "insert"
        ) {
            continue;
        }
        let mut s2 = String::new();
        s2.push_str(&text[..f.end]);
        s2.push_str(&slice(f));
        s2.push_str(&text[f.end..]);
        assert_rejected(
            &dir,
            s2.as_bytes(),
            &format!("duplicate {}", f.kind),
            Reject::Apply(duplicate_sqlstate(&f.kind)),
        );
    }
    // 主キーを持つ表（`child`）のデータの重複は、主キー違反（23505）で拒否される。COPY の枠は表名で
    // 指定する。INSERT 形式のダンプでも、先頭の INSERT 枠（`child` 表）の重複を同様に確かめる。
    let f = frames
        .iter()
        .find(|f| f.kind == "copy" && text[f.start..f.end].contains("COPY \"child\" ("))
        .expect("COPY frame of the child table")
        .clone();
    let mut s2 = String::new();
    s2.push_str(&text[..f.end]);
    s2.push_str(&slice(&f));
    s2.push_str(&text[f.end..]);
    assert_rejected(
        &dir,
        s2.as_bytes(),
        "duplicate copy data",
        Reject::Apply("23505"),
    );
    let ins = s(&dir.join("insert.sql")).to_string();
    let o = run(&dump_args(
        &db,
        &ins,
        &["--tenant", "tenant-a", "--format", "insert"],
    ));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let itext = read_text(&ins);
    let (_, iframes, _) = parse_frames(&itext);
    let f = iframes
        .iter()
        .find(|f| f.kind == "insert" && itext[f.start..f.end].contains("INSERT INTO \"child\" ("))
        .expect("insert frame")
        .clone();
    let mut s2 = String::new();
    s2.push_str(&itext[..f.end]);
    s2.push_str(&itext[f.start..f.end]);
    s2.push_str(&itext[f.end..]);
    assert_rejected(
        &dir,
        s2.as_bytes(),
        "duplicate insert data",
        Reject::Apply("23505"),
    );
    // ヘッダの重複（先頭が 2 回）。
    let mut s2 = text[..header_end].to_string();
    s2.push_str(&text);
    assert_rejected(&dir, s2.as_bytes(), "duplicate header", Reject::Malformed);
    // 終端行の重複・内容の連結（同じダンプを 2 つ連結）。
    let mut s2 = text.clone();
    s2.push_str(&text);
    assert_rejected(&dir, s2.as_bytes(), "concatenated dumps", Reject::Malformed);
}

/// 種別ごとに最初の枠を、出現順に返す。
fn first_frame_of_each_kind(frames: &[Frame]) -> Vec<Frame> {
    let mut seen = std::collections::BTreeSet::new();
    frames
        .iter()
        .filter(|f| seen.insert(f.kind.clone()))
        .cloned()
        .collect()
}

/// 枠の見出し行の次から数えた本文の末尾にある `;` の位置（`@stmt`・`@copy` とも見出しの長さが本文長）。
fn semicolon_pos(text: &str, f: &Frame, marker_end: usize) -> usize {
    let len: usize = text[f.start..marker_end]
        .rsplit(' ')
        .next()
        .expect("len")
        .parse()
        .expect("len");
    let pos = marker_end + 1 + len;
    assert_eq!(text.as_bytes()[pos], b';');
    pos
}
