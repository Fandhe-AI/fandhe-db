//! 論理の復元（`dump-restore`）の完了時に復元の世代番号が進むことの外形検証
//! （Issue #2451・RECOVER-17・RECOVER-16 ポインタ。方式は `docs/design/logical-dump.md` の第 12 節、
//! 世代の扱いは `docs/design/restore-generation.md`）。
//!
//! ダンプのヘッダが復元元の世代を運び、復元先の世代がそれより大きくなること、世代を読めない・進められない
//! ときは何も作らず公開しないことを、バイナリを子プロセスとして起動して確かめる。ポート・環境変数に
//! 依存せず、一意な一時ディレクトリだけを使う。

use crate::common;
use crate::dump_cli::{
    assert_no_temp_leftovers, code, dir_of, dump_args, read_text, run, s, stderr, Direct,
};

use std::path::Path;

use engine::storage::Storage;

/// 1 表 1 行の DB を作る（`dump` は DB を開いているプロセスがあると拒否するため、作成後に閉じる）。
fn build_db(db: &str) {
    let core = engine::core::EngineCore::open(db).expect("open db");
    let mut a = Direct::new(&core, "tenant-a");
    a.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, b TEXT)");
    a.exec("INSERT INTO t (id, b) VALUES (1, 'x')");
}

fn generation_of(path: &str) -> u64 {
    Storage::open(path)
        .expect("open")
        .restore_generation()
        .expect("read restore generation")
}

/// ダンプ本文のヘッダから世代行の値を読む。
fn header_generation(text: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix("-- restore-generation: "))
        .expect("generation line")
        .parse()
        .expect("generation value")
}

fn dump_to(db: &str, out: &str, extra: &[&str]) {
    let mut args = vec!["--tenant", "tenant-a"];
    args.extend_from_slice(extra);
    let o = run(&dump_args(db, out, &args));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
}

/// 新規 DB のダンプは世代 0 を運び、復元先の世代は 0 より大きい。復元先を再ダンプして復元すると、
/// ダンプ経由でも世代が厳密に増え続ける。
#[test]
fn recover17_logical_restore_advances_the_generation_through_dumps() {
    let fx = common::TempFixtureDir::new("dump-gen-chain");
    let db = fx.db_path_str();
    build_db(&db);
    let dir = dir_of(&db);
    let out1 = s(&dir.join("d1.sql")).to_string();
    dump_to(&db, &out1, &[]);
    assert_eq!(header_generation(&read_text(&out1)), 0);

    let r1 = s(&dir.join("r1.redb")).to_string();
    let o = run(&["dump-restore", "--from", &out1, "--db", &r1]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let g1 = generation_of(&r1);
    assert!(g1 > 0);

    let out2 = s(&dir.join("d2.sql")).to_string();
    dump_to(&r1, &out2, &[]);
    assert_eq!(header_generation(&read_text(&out2)), g1);
    let r2 = s(&dir.join("r2.redb")).to_string();
    let o = run(&["dump-restore", "--from", &out2, "--db", &r2]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(generation_of(&r2) > g1);
    assert_no_temp_leftovers(&dir);
}

/// 復元元が時計より大きい世代を持っていても、論理の復元先の世代は復元元より大きい（時計の下限だけに
/// 依存しない）。ダンプ元の世代は変わらない。
#[test]
fn recover17_logical_restore_exceeds_a_source_generation_ahead_of_the_clock() {
    let fx = common::TempFixtureDir::new("dump-gen-ahead");
    let db = fx.db_path_str();
    build_db(&db);
    let ahead: u64 = 5_000_000_000_000_000_000;
    {
        let mut st = Storage::open(&db).expect("open");
        st.advance_restore_generation_from_source(ahead)
            .expect("advance");
    }
    let source_gen = generation_of(&db);
    assert!(source_gen > ahead);
    let dir = dir_of(&db);
    let out = s(&dir.join("d.sql")).to_string();
    dump_to(&db, &out, &[]);
    assert_eq!(header_generation(&read_text(&out)), source_gen);
    let r = s(&dir.join("r.redb")).to_string();
    let o = run(&["dump-restore", "--from", &out, "--db", &r]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(generation_of(&r) > source_gen);
    assert_eq!(generation_of(&db), source_gen);
}

/// 範囲指定（共有のみ・定義のみ）のダンプにも世代行が載る。
#[test]
fn recover17_every_dump_scope_carries_the_generation_line() {
    let fx = common::TempFixtureDir::new("dump-gen-scopes");
    let db = fx.db_path_str();
    build_db(&db);
    let dir = dir_of(&db);
    let a = s(&dir.join("a.sql")).to_string();
    let b = s(&dir.join("b.sql")).to_string();
    let o = run(&dump_args(&db, &a, &["--shared-only"]));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    dump_to(&db, &b, &["--schema-only"]);
    assert_eq!(header_generation(&read_text(&a)), 0);
    assert_eq!(header_generation(&read_text(&b)), 0);
}

/// ヘッダの世代を書き換えたダンプの扱い。上限ちょうどは engine が進行を拒否して終了コード 1、範囲外・
/// 非数字・旧版は終了コード 4。いずれも何も公開せず、残骸を残さず、エラー文に値を含めない。
#[test]
fn recover17_tampered_generation_publishes_nothing() {
    let fx = common::TempFixtureDir::new("dump-gen-tamper");
    let db = fx.db_path_str();
    build_db(&db);
    let dir = dir_of(&db);
    let out = s(&dir.join("d.sql")).to_string();
    dump_to(&db, &out, &[]);
    let text = read_text(&out);
    let dest = s(&dir.join("dest.redb")).to_string();
    let cases: [(&str, String, i32); 5] = [
        (
            "limit",
            text.replacen("generation: 0", "generation: 9223372036854775807", 1),
            1,
        ),
        (
            "out-of-range",
            text.replacen("generation: 0", "generation: 9223372036854775808", 1),
            4,
        ),
        (
            "not-digits",
            text.replacen("generation: 0", "generation: zero", 1),
            4,
        ),
        (
            "v1",
            text.replacen("dump v2", "dump v1", 1)
                .replacen("-- restore-generation: 0\n", "", 1),
            4,
        ),
        (
            "missing",
            text.replacen("-- restore-generation: 0\n", "", 1),
            4,
        ),
    ];
    for (name, bad, want) in cases {
        let from = s(&dir.join("variant.sql")).to_string();
        std::fs::write(&from, bad).expect("write variant");
        let o = run(&["dump-restore", "--from", &from, "--db", &dest]);
        assert_eq!(code(&o), Some(want), "{name}: stderr: {}", stderr(&o));
        assert!(!Path::new(&dest).exists(), "{name}: published a database");
        assert!(
            !stderr(&o).contains("9223372036854775807"),
            "{name}: leaked value"
        );
        assert_no_temp_leftovers(&dir);
    }
    let o = run(&[
        "dump-restore",
        "--from",
        &out,
        "--db",
        &dest,
        "--expect-tenant",
        "tenant-a",
    ]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
}

/// 世代の値が壊れた DB のダンプは、終了コード 1 で失敗し出力ファイルを作らない。
#[test]
fn recover17_dump_of_a_corrupt_generation_creates_nothing() {
    let fx = common::TempFixtureDir::new("dump-gen-corrupt");
    let db = fx.db_path_str();
    build_db(&db);
    {
        let mut st = Storage::open(&db).expect("open");
        st.advance_restore_generation_from_source(0)
            .expect("advance");
    }
    let g = generation_of(&db).to_le_bytes();
    let mut bytes = std::fs::read(&db).expect("read db");
    let mut patched = 0;
    for pos in 0..bytes.len().saturating_sub(g.len()) {
        if bytes.get(pos..pos + g.len()) == Some(&g[..]) {
            // 上位バイトを立てて i64 の範囲外にする。
            if let Some(b) = bytes.get_mut(pos + 7) {
                *b = 0xff;
            }
            patched += 1;
        }
    }
    assert!(patched >= 1, "generation value not found");
    std::fs::write(&db, &bytes).expect("write corrupted");
    let dir = dir_of(&db);
    let out = s(&dir.join("d.sql")).to_string();
    let o = run(&dump_args(&db, &out, &["--tenant", "tenant-a"]));
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(
        stderr(&o).contains("failed to read the restore generation"),
        "{}",
        stderr(&o)
    );
    assert!(!Path::new(&out).exists());
    assert_no_temp_leftovers(&dir);
}
