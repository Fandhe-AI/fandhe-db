//! `wire-server verify` / `wire-server restore` サブコマンドの外形検証（Issue #2156・
//! RECOVER-13 (3)(6)(7) ポインタ。親 #2154）。バイナリを子プロセスとして起動し、終了コード・
//! 出力・ファイルの状態を確かめる。サーバーを起動するテストはポート 0 と一意な一時ディレクトリを使う。
//! 壊れた DB は wire-server が redb に直接依存しないため、engine の公開 API で 1 行だけ書いた
//! ファイルのバイト列を直接書き換えて作る。

use crate::common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use engine::storage::{RowInput, Storage, Visibility};

use super::backup_cli::{
    backup, count_rows, parent_of, spawn_server, write_user_store_with_alice, ChildGuard,
};

const SECRET_NOTE: &str = "secret-payload-qwxz";

fn verify(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("verify")
        .args(args)
        .output()
        .expect("run verify")
}

fn restore(from: &str, db: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["restore", "--from", from, "--db", db])
        .output()
        .expect("run restore")
}

pub(super) fn code(o: &Output) -> Option<i32> {
    o.status.code()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub(super) fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// 標準出力と標準エラーに、行の値・テナント名・ユーザー名・パスが現れないことを確かめる。
fn assert_no_leak(o: &Output, extra: &[&str]) {
    let all = format!("{}{}", stdout(o), stderr(o));
    for needle in [SECRET_NOTE, "tenant-a", "alice", "tnt-secret"]
        .iter()
        .chain(extra.iter())
    {
        assert!(!all.contains(needle), "output leaked {needle:?}: {all}");
    }
}

fn exec(s: &mut std::net::TcpStream, sql: &str) {
    common::send_simple_query(s, sql);
    let _ = common::read_command_complete(s);
    common::read_ready_for_query(s);
}

/// サーバーで表と 3 行を作り（行の値に秘匿対象の文字列を含める）、サーバーを停止する。
fn populate(fx: &common::TempFixtureDir) -> (String, String) {
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let mut a = spawn_server(&users, &db);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    let mut s = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    exec(
        &mut s,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3), note TEXT)",
    );
    for id in 1..=3 {
        exec(
            &mut s,
            &format!(
                "INSERT INTO docs (id, embedding, note) VALUES ({id}, '[0.1,0.2,0.3]', '{SECRET_NOTE}')"
            ),
        );
    }
    drop(s);
    drop(a);
    (users, db)
}

pub(super) fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".fandhe-"))
        .collect()
}

/// `rows` 表に 1 行だけ書いた DB を作り、行ヘッダの版バイトを壊して復号不能にする。
fn write_db_with_undecodable_row(path: &Path) {
    const MARKER: &[u8; 32] = b"MARKER-0123456789-ABCDEFGHIJKLMN";
    {
        let storage = Storage::open(path).expect("open");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tnt",
                    visibility: Visibility::Public,
                    embedding: &[0.5],
                    metadata: MARKER,
                },
            )
            .expect("put");
    }
    let mut bytes = std::fs::read(path).expect("read db");
    // 行の版バイトは、メタデータの手前 `1+2+len(tnt)+1+4+4+4` バイトにある。
    let back = 1 + 2 + 3 + 1 + 4 + 4 + 4;
    let mut patched = 0;
    for pos in 0..bytes.len().saturating_sub(MARKER.len()) {
        if bytes.get(pos..pos + MARKER.len()) == Some(&MARKER[..]) {
            let v = pos.checked_sub(back).expect("version offset");
            assert_eq!(bytes[v], 2, "row format version byte");
            bytes[v] = 0xEE;
            patched += 1;
        }
    }
    assert!(patched >= 1, "marker not found");
    std::fs::write(path, bytes).expect("write corrupted db");
}

/// 正常な DB は 0（報告に値・テナント名なし）。稼働中の DB は 1 で拒否し、サーバーは無傷。
#[test]
fn verify_consistent_db_exits_zero_and_rejects_running_db() {
    let fx = common::TempFixtureDir::new("verify-cli");
    let (users, db) = populate(&fx);

    let mut a = spawn_server(&users, &db);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    let mut s = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let o = verify(&["--db", &db]);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("in use"), "stderr: {}", stderr(&o));
    assert_eq!(count_rows(&mut s), 3);
    drop(s);
    drop(a);

    let o = verify(&["--db", &db]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with("status=consistent\n"), "stdout: {out}");
    assert!(out.contains("error_total=0\n"));
    assert!(out.contains("findings_truncated=false\n"));
    assert_no_leak(&o, &[&db]);
}

/// 行の復号不能は 4（矛盾）で、種類・表・件数だけが出る元ファイルは変更しない（コピーを検査する。バイト列の同一性は専用のテストで固定する）。
#[test]
fn verify_corrupted_db_exits_with_inconsistent_code() {
    let fx = common::TempFixtureDir::new("verify-cli-corrupt");
    let db = fx.db_path_str();
    write_db_with_undecodable_row(Path::new(&db));

    let o = verify(&["--db", &db]);
    assert_eq!(code(&o), Some(4), "stderr: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with("status=inconsistent\n"), "stdout: {out}");
    assert!(
        out.contains("finding code=row_undecodable severity=error table=rows count=1\n"),
        "stdout: {out}"
    );
    assert_no_leak(&o, &["MARKER-0123456789"]);
}

/// 3 行を書いて正常に閉じた DB を作る。
pub(super) fn write_clean_db(path: &Path) {
    let storage = Storage::open(path).expect("open");
    for id in 1..=3 {
        storage
            .put(
                id,
                &RowInput {
                    tenant_id: "tnt",
                    visibility: Visibility::Public,
                    embedding: &[0.5],
                    metadata: b"m",
                },
            )
            .expect("put");
    }
}

/// verify は元ファイルを変更しない（読み取り専用で開いて直接検査し、書き込みを要する状態だけ
/// コピーを検査する。Issue #3074）。サーバー停止後の DB と壊れた DB の両方で、検査前後のバイト列が
/// 同一で、一時コピーも残らない。`--memory-budget-bytes` 指定時も同じ。異常終了後の DB が
/// コピー経路になることは `verify_falls_back_to_copy_for_crashed_db_without_writing` で固定する。
#[test]
fn verify_never_modifies_the_original_file() {
    let fx = common::TempFixtureDir::new("verify-cli-readonly");
    let (_users, db) = populate(&fx);
    let dir = parent_of(&db).to_path_buf();
    let before = std::fs::read(&db).expect("read");
    for args in [
        vec!["--db", db.as_str()],
        vec!["--db", db.as_str(), "--memory-budget-bytes", "1"],
    ] {
        let o = verify(&args);
        assert!(
            matches!(code(&o), Some(0) | Some(5)),
            "stderr: {}",
            stderr(&o)
        );
        assert_eq!(std::fs::read(&db).expect("read"), before);
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        // populate のサーバーは強制終了されるため、読み取り専用では開けずコピー経路になる。
        assert!(stdout(&o).ends_with("verify_mode=copy\n"), "{}", stdout(&o));
    }

    // 正常に閉じた DB は、コピーを作らず読み取り専用で直接検査する（予算指定時も同じ）。
    let clean = dir.join("clean.redb");
    write_clean_db(&clean);
    let clean_before = std::fs::read(&clean).expect("read");
    for extra in [&[][..], &["--memory-budget-bytes", "1"][..]] {
        let mut args = vec!["--db", clean.to_str().expect("utf-8 path")];
        args.extend_from_slice(extra);
        let o = verify(&args);
        assert!(
            matches!(code(&o), Some(0) | Some(5)),
            "stderr: {}",
            stderr(&o)
        );
        assert!(
            stdout(&o).ends_with("verify_mode=read_only\n"),
            "{}",
            stdout(&o)
        );
        assert_eq!(std::fs::read(&clean).expect("read"), clean_before);
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
    }

    let bad = dir.join("bad.redb");
    write_db_with_undecodable_row(&bad);
    let bad_before = std::fs::read(&bad).expect("read");
    let o = verify(&["--db", bad.to_str().expect("utf-8 path")]);
    assert_eq!(code(&o), Some(4), "stderr: {}", stderr(&o));
    assert!(
        stdout(&o).contains("verify_mode=read_only\n"),
        "{}",
        stdout(&o)
    );
    assert_eq!(std::fs::read(&bad).expect("read"), bad_before);
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));

    // DB として開けないファイルでも一時コピーは残らず、元ファイルは不変
    let junk = dir.join("junk.redb");
    std::fs::write(&junk, b"not a database").expect("write");
    let o = verify(&["--db", junk.to_str().expect("utf-8 path")]);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert_eq!(std::fs::read(&junk).expect("read"), b"not a database");
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
}

/// 異常終了後の DB（開いたまま複製した、正常に閉じていないファイル）は読み取り専用では開けないため
/// コピー経路で検査し、元ファイルは書かず、一時コピーも残さない（Issue #3074）。
#[test]
fn verify_falls_back_to_copy_for_crashed_db_without_writing() {
    let fx = common::TempFixtureDir::new("verify-cli-crashed");
    let live = fx.db_path_str();
    let storage = Storage::open(&live).expect("open");
    for id in 1..=3 {
        storage
            .put(
                id,
                &RowInput {
                    tenant_id: "tnt",
                    visibility: Visibility::Public,
                    embedding: &[0.5],
                    metadata: b"m",
                },
            )
            .expect("put");
    }
    let dir = parent_of(&live).to_path_buf();
    let crashed = dir.join("crashed.redb");
    std::fs::copy(&live, &crashed).expect("copy while open");
    drop(storage);
    let before = std::fs::read(&crashed).expect("read");
    let o = verify(&["--db", crashed.to_str().expect("utf-8 path")]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(stdout(&o).ends_with("verify_mode=copy\n"), "{}", stdout(&o));
    assert_eq!(std::fs::read(&crashed).expect("read"), before);
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
}

/// 検査できない項目だけのとき 5（整合とは報告しない）。
#[test]
fn verify_incomplete_only_exits_with_incomplete_code() {
    let fx = common::TempFixtureDir::new("verify-cli-incomplete");
    let (_users, db) = populate(&fx);
    let o = verify(&["--db", &db, "--memory-budget-bytes", "1"]);
    assert_eq!(
        code(&o),
        Some(5),
        "stdout: {} stderr: {}",
        stdout(&o),
        stderr(&o)
    );
    let out = stdout(&o);
    assert!(out.starts_with("status=incomplete\n"), "stdout: {out}");
    assert!(out.contains("code=check_budget_exceeded severity=incomplete"));
    assert_no_leak(&o, &[]);
}

/// 不正な引数・パスは DB を開く前に 2 で拒否される。空ファイルを DB として初期化しない。
#[test]
fn verify_rejects_invalid_arguments_and_paths() {
    let fx = common::TempFixtureDir::new("verify-cli-invalid");
    let db = fx.db_path_str();
    let dir = parent_of(&db).to_path_buf();
    let missing = dir.join("nodb.redb");
    let empty = dir.join("empty.redb");
    std::fs::write(&empty, b"").expect("write empty");
    std::fs::write(&db, b"not a database").expect("write dummy db");

    let missing_s = missing.to_str().expect("utf-8 path");
    let empty_s = empty.to_str().expect("utf-8 path");
    let dir_s = dir.to_str().expect("utf-8 path");
    let cases: Vec<Vec<&str>> = vec![
        vec![],
        vec!["--db"],
        vec!["--bogus"],
        vec!["--db", missing_s],
        vec!["--db", empty_s],
        vec!["--db", dir_s],
        vec!["--db", ""],
        vec!["--db", &db, "--db", &db],
        vec!["--db", &db, "--memory-budget-bytes", "0"],
        vec!["--db", &db, "--memory-budget-bytes", "x"],
    ];
    for args in cases {
        let o = verify(&args);
        assert_eq!(code(&o), Some(2), "args={args:?} stderr: {}", stderr(&o));
        assert!(stdout(&o).is_empty());
    }
    assert!(!missing.exists());
    assert_eq!(std::fs::metadata(&empty).expect("meta").len(), 0);

    // DB として開けないファイルは実行時の失敗（1）
    let o = verify(&["--db", &db]);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
}

/// Issue #3094（RECOVER-13・RECOVER-14）: restore は復元先の親に残った自分の一時名の残骸だけを掃除し、
/// 形式に一致しないもの・権限が想定外のものには触れない。報告は stderr で、終了コードは変わらない。
#[test]
fn restore_sweeps_stale_restore_temp_names_only() {
    use std::os::unix::fs::PermissionsExt;
    let fx = common::TempFixtureDir::new("restore-sweep");
    let (_users, db) = populate(&fx);
    let dir = parent_of(&db).to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod dir");
    let snap = dir.join("snap.redb");
    let snap_s = snap.to_str().expect("utf-8 path").to_string();
    let o = backup(&db, &snap_s);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));

    let stale = ".fandhe-restore-4242-7.tmp";
    let wide = ".fandhe-restore-4242-8.tmp";
    let decoy = ".fandhe-restore-decoy.tmp";
    for (name, mode) in [(stale, 0o600), (wide, 0o644), (decoy, 0o600)] {
        let p = dir.join(name);
        std::fs::write(&p, b"x").expect("write");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
    let restored = dir.join("restored.redb");
    let restored_s = restored.to_str().expect("utf-8 path").to_string();
    let o = restore(&snap_s, &restored_s);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("removed 1 stale temporary file(s)") && err.contains(stale),
        "{err}"
    );
    assert!(err.contains("left 1 file(s)"), "{err}");
    assert!(!dir.join(stale).exists());
    assert!(dir.join(wide).exists() && dir.join(decoy).exists());
    assert!(restored.exists());
}

/// Issue #3094: 旧形式の残骸（`<pid>-0..16`）が多数あっても、掃除の所有権プローブと一時コピーの作成が
/// 名前衝突で失敗せず、掃除と復元の両方が成功する（プローブ名・一時名に nonce を持つため）。
#[test]
fn restore_succeeds_with_many_legacy_leftovers_for_same_pid() {
    use std::os::unix::fs::PermissionsExt;
    let fx = common::TempFixtureDir::new("restore-sweep-many");
    let (_users, db) = populate(&fx);
    let dir = parent_of(&db).to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod dir");
    let snap = dir.join("snap.redb");
    let snap_s = snap.to_str().expect("utf-8 path").to_string();
    let o = backup(&db, &snap_s);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    for seq in 0..=16 {
        let p = dir.join(format!(".fandhe-restore-1-{seq}.tmp"));
        std::fs::write(&p, b"x").expect("write");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let restored = dir.join("restored.redb");
    let restored_s = restored.to_str().expect("utf-8 path").to_string();
    let o = restore(&snap_s, &restored_s);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(
        stderr(&o).contains("removed 17 stale temporary file(s)"),
        "{}",
        stderr(&o)
    );
    assert!(restored.exists());
}

/// Issue #3094: `--from` が一時名の実体へのシンボリックリンクでも、掃除は復元元の実体を消さない。
#[test]
fn restore_sweep_keeps_source_reached_through_symlink_to_temp_name() {
    use std::os::unix::fs::PermissionsExt;
    let fx = common::TempFixtureDir::new("restore-sweep-alias");
    let (_users, db) = populate(&fx);
    let dir = parent_of(&db).to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod dir");
    let snap = dir.join("snap.redb");
    let o = backup(&db, snap.to_str().expect("utf-8 path"));
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let real = dir.join(".fandhe-restore-4242-7.tmp");
    std::fs::rename(&snap, &real).expect("rename");
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let alias = dir.join("backup.redb");
    std::os::unix::fs::symlink(&real, &alias).expect("symlink");
    let restored = dir.join("restored.redb");
    let o = restore(
        alias.to_str().expect("utf-8 path"),
        restored.to_str().expect("utf-8 path"),
    );
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(real.exists(), "source entity must survive the sweep");
    assert!(restored.exists());
}

/// 復元の正常系: 元のバックアップは不変、復元先から別のサーバーが起動してデータを読める。
/// 既存の復元先（ファイル・ディレクトリ・リンク）は書かずに 2 で拒否する。
#[test]
fn restore_publishes_verified_copy_and_refuses_existing_destination() {
    let fx = common::TempFixtureDir::new("restore-cli");
    let (users, db) = populate(&fx);
    let dir = parent_of(&db).to_path_buf();
    let snap = dir.join("snap.redb");
    let snap_s = snap.to_str().expect("utf-8 path").to_string();
    let o = backup(&db, &snap_s);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let snap_before = std::fs::read(&snap).expect("read snap");

    let restored = dir.join("restored.redb");
    let restored_s = restored.to_str().expect("utf-8 path").to_string();
    let o = restore(&snap_s, &restored_s);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(
        stdout(&o).starts_with("restore written: "),
        "{}",
        stdout(&o)
    );
    assert_no_leak(&o, &[&snap_s, &restored_s]);
    assert_eq!(std::fs::read(&snap).expect("read snap"), snap_before);
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&restored)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let restored_before = std::fs::read(&restored).expect("read restored");

    // 既存の復元先は拒否（ファイル・ディレクトリ・壊れたリンク）。内容は変わらない
    let o = restore(&snap_s, &restored_s);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    assert_eq!(std::fs::read(&restored).expect("read"), restored_before);
    let o = restore(&snap_s, dir.to_str().expect("utf-8 path"));
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    let link = dir.join("link.redb");
    let target = dir.join("link-target.redb");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let o = restore(&snap_s, link.to_str().expect("utf-8 path"));
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    assert!(!target.exists());

    // 復元した DB の整合検査が通り、サーバーが起動してデータが読める
    let o = verify(&["--db", &restored_s]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    let mut b = spawn_server(&users, &restored_s);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut b.0, Duration::from_secs(10));
    let mut s = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    assert_eq!(count_rows(&mut s), 3);
}

/// 壊れたバックアップは公開されず、一時ファイルも残らず、元ファイルは変わらない。
#[test]
fn restore_refuses_broken_backups_without_publishing() {
    let fx = common::TempFixtureDir::new("restore-cli-broken");
    let db = fx.db_path_str();
    let dir = parent_of(&db).to_path_buf();
    let dest = dir.join("restored.redb");
    let dest_s = dest.to_str().expect("utf-8 path");

    // DB として開けないファイル: 実行時の失敗（1）
    let garbage = dir.join("garbage.redb");
    std::fs::write(&garbage, b"this is not a redb file").expect("write garbage");
    let garbage_before = std::fs::read(&garbage).expect("read");
    let o = restore(garbage.to_str().expect("utf-8 path"), dest_s);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(!dest.exists());
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
    assert_eq!(std::fs::read(&garbage).expect("read"), garbage_before);

    // 開けるが行が復号できない: 矛盾（4）。報告は標準エラーに出て、値は含まれない
    let bad = dir.join("bad.redb");
    write_db_with_undecodable_row(&bad);
    let bad_before = std::fs::read(&bad).expect("read");
    let o = restore(bad.to_str().expect("utf-8 path"), dest_s);
    assert_eq!(code(&o), Some(4), "stderr: {}", stderr(&o));
    assert!(stdout(&o).is_empty());
    assert!(
        stderr(&o).contains("code=row_undecodable"),
        "{}",
        stderr(&o)
    );
    assert_no_leak(&o, &["MARKER-0123456789"]);
    assert!(!dest.exists());
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
    assert_eq!(std::fs::read(&bad).expect("read"), bad_before);
}

/// 稼働中の DB を復元元に指定すると、コピーせず 1 で拒否する。
#[test]
fn restore_refuses_a_running_database_as_source() {
    let fx = common::TempFixtureDir::new("restore-cli-running");
    let (users, db) = populate(&fx);
    let dir = parent_of(&db).to_path_buf();
    let dest = dir.join("restored.redb");
    let mut a: ChildGuard = spawn_server(&users, &db);
    let (_addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    let o = restore(&db, dest.to_str().expect("utf-8 path"));
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("in use"), "stderr: {}", stderr(&o));
    assert!(!dest.exists());
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
}

/// parse 後に長さ 0 へ差し替わった復元元は、空 DB を公開せず 2 で拒否する（`restore()` の再確認）。
#[test]
fn restore_function_refuses_zero_length_source() {
    use wire_server::restore_opt::{restore, RestoreError};
    let fx = common::TempFixtureDir::new("restore-zero-src");
    let dir = parent_of(&fx.db_path_str()).to_path_buf();
    let src = dir.join("empty-src.redb");
    std::fs::write(&src, b"").expect("write empty");
    let dest = dir.join("restored.redb");
    let r = restore(&src, &dest, &mut None);
    assert!(matches!(r, Err(RestoreError::InvalidSource)), "{r:?}");
    assert!(!dest.exists());
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
}

/// 不正な引数・パスは何も作らず 2 で拒否される。
#[test]
fn restore_rejects_invalid_arguments_and_paths() {
    let fx = common::TempFixtureDir::new("restore-cli-invalid");
    let db = fx.db_path_str();
    let dir = parent_of(&db).to_path_buf();
    std::fs::write(&db, b"not a database").expect("write dummy");
    let empty = dir.join("empty.redb");
    std::fs::write(&empty, b"").expect("write empty");
    let s = |p: std::path::PathBuf| p.to_str().expect("utf-8 path").to_string();
    let new_ok = s(dir.join("new.redb"));
    let cases: Vec<Vec<String>> = vec![
        vec![],
        vec!["restore".into()],
        vec!["--from".into(), db.clone()],
        vec!["--db".into(), new_ok.clone()],
        vec!["--bogus".into()],
        // 復元元が無い・空・ディレクトリ
        vec![
            "--from".into(),
            s(dir.join("nope.redb")),
            "--db".into(),
            new_ok.clone(),
        ],
        vec!["--from".into(), s(empty), "--db".into(), new_ok.clone()],
        vec![
            "--from".into(),
            s(dir.clone()),
            "--db".into(),
            new_ok.clone(),
        ],
        // 復元先が復元元自身・親ディレクトリ成分・親が無い・空
        vec!["--from".into(), db.clone(), "--db".into(), db.clone()],
        vec![
            "--from".into(),
            db.clone(),
            "--db".into(),
            s(dir.join("a/../x.redb")),
        ],
        vec![
            "--from".into(),
            db.clone(),
            "--db".into(),
            s(dir.join("missing/x.redb")),
        ],
        vec!["--from".into(), db.clone(), "--db".into(), String::new()],
        // 重複指定
        vec![
            "--from".into(),
            db.clone(),
            "--from".into(),
            db.clone(),
            "--db".into(),
            new_ok.clone(),
        ],
    ];
    for args in cases {
        let o = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .arg("restore")
            .args(&args)
            .output()
            .expect("run restore");
        assert_eq!(code(&o), Some(2), "args={args:?} stderr: {}", stderr(&o));
        assert!(stdout(&o).is_empty());
    }
    assert!(!dir.join("new.redb").exists());
    assert!(leftovers(&dir).is_empty());
}

/// 復元先の復元の世代番号を読む（復元後の DB を engine の公開 API で開く）。
pub(super) fn generation_of(path: &Path) -> u64 {
    Storage::open(path)
        .expect("open restored")
        .restore_generation()
        .expect("read restore generation")
}

/// 物理の復元の完了時に世代が進み（復元元より大きい）、通常の運用条件では同じ元からの繰り返しでも
/// 前回より大きい（復元間で共有する採番状態は持たないので、繰り返しの増加は実行環境が通常どおり進む前提）。
/// 復元元は変わらない。復元した DB を元にした次の復元はさらに大きい（RECOVER-17・RECOVER-13 (3)。
/// Issue #2448）。
#[test]
fn recover17_physical_restore_advances_the_generation_monotonically() {
    let fx = common::TempFixtureDir::new("restore-cli-generation");
    let db = fx.db_path_str();
    let dir = parent_of(&db).to_path_buf();
    let src = dir.join("src.redb");
    write_clean_db(&src);
    let src_s = src.to_str().expect("utf-8 path").to_string();
    let src_before = std::fs::read(&src).expect("read src");

    // 新規作成した DB の世代は 0（復元元を開く前に内容を控えたので、読み取りの open で変わらない）。
    let mut previous = 0;
    let mut paths = Vec::new();
    for i in 0..3 {
        let dest = dir.join(format!("restored-{i}.redb"));
        let dest_s = dest.to_str().expect("utf-8 path").to_string();
        let o = restore(&src_s, &dest_s);
        assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
        let g = generation_of(&dest);
        assert!(g > previous, "restore {i}: {g} must exceed {previous}");
        previous = g;
        paths.push(dest);
    }
    // 復元元のファイルの内容は変わらない（世代は復元先にだけ書く）。
    assert_eq!(std::fs::read(&src).expect("read src"), src_before);
    assert_eq!(generation_of(&src), 0, "a new database starts at 0");

    // 復元した DB からの復元は、その世代より大きい。
    let chained = dir.join("chained.redb");
    let o = restore(
        paths[0].to_str().expect("utf-8 path"),
        chained.to_str().expect("utf-8 path"),
    );
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(generation_of(&chained) > generation_of(&paths[0]));
    // 世代を進めた復元先の整合検査が通る。
    let o = verify(&["--db", chained.to_str().expect("utf-8 path")]);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
}

/// 復元元の世代の値が壊れていると、復元は終了コード 1 で失敗し、宛先も残骸も残さない
/// (RECOVER-17・Issue #2448。壊れた値を黙って受け入れない)。値のバイト列を直接書き換えて作る。
#[test]
fn recover17_restore_with_a_corrupt_source_generation_publishes_nothing() {
    let fx = common::TempFixtureDir::new("restore-cli-generation-corrupt");
    let db = fx.db_path_str();
    let dir = parent_of(&db).to_path_buf();
    let src = dir.join("src.redb");
    write_clean_db(&src);
    {
        let mut s = Storage::open(&src).expect("open");
        s.advance_restore_generation().expect("advance");
    }
    let g = generation_of(&src).to_le_bytes();
    let mut bytes = std::fs::read(&src).expect("read src");
    let mut patched = 0;
    for pos in 0..bytes.len().saturating_sub(g.len()) {
        if bytes.get(pos..pos + g.len()) == Some(&g[..]) {
            // 上位バイトを立てて i64 の範囲外にする。
            bytes[pos + 7] = 0xff;
            patched += 1;
        }
    }
    assert!(patched >= 1, "generation value not found");
    std::fs::write(&src, &bytes).expect("write corrupted");

    let dest = dir.join("restored.redb");
    let o = restore(
        src.to_str().expect("utf-8 path"),
        dest.to_str().expect("utf-8 path"),
    );
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(
        stderr(&o).contains("restore generation could not be advanced"),
        "{}",
        stderr(&o)
    );
    assert!(!dest.exists());
    assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
    assert_eq!(std::fs::read(&src).expect("read src"), bytes);
}
