//! `engine::storage::Storage` の統合テスト（TASK-140/TASK-141、対象ビヘイビア:
//! PERSIST-1, PERSIST-2, PERSIST-3, PERSIST-4。ポインタ:
//! `docs/spec/04-behavior/persistence.md`）。
//!
//! PERSIST-1・PERSIST-3・PERSIST-4 の検証は `Storage` の公開 API だけでは表現できない
//! トランザクション境界・生バイト列に踏み込むため、テストスコープでのみ `redb` を
//! 直接操作する（`crates/engine/Cargo.toml` の `[dev-dependencies]` 参照）。
//! TASK-142・TASK-143 は本ファイルのスコープ外（ポインタ: `docs/spec/05-tasks.md`）。

// 許可（clippy::disallowed_methods）: 本ファイルは engine の公開 API の外から redb を直接開き、
// 永続内容の検査・破壊・裏口の対照を行う。変更ログの捕捉（`storage::change_log`）を通す
// 対象は engine 製品コードの書き込み点で、外部クレートとしての直接操作は対象外。
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use engine::storage::{RowInput, Storage, StorageError, Visibility};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle};

/// `Storage` 内部の行テーブルと同一の名前・型（キー: `(tenant_id, id)` 複合キー
/// （対象ビヘイビア: TABLE-12）、値: エンコード済みバイト列）。テスト側は行の中身を
/// 解釈しないため、値のエンコード詳細に依存しない。
const ROWS_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("rows");

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    row_with_rls("tenant-a", Visibility::Public, embedding, metadata)
}

fn row_with_rls<'a>(
    tenant_id: &'a str,
    visibility: Visibility,
    embedding: &'a [f32],
    metadata: &'a [u8],
) -> RowInput<'a> {
    RowInput {
        tenant_id,
        visibility,
        embedding,
        metadata,
    }
}

// 対象ビヘイビア: PERSIST-1（詳細は関数名・ポインタ: docs/spec/04-behavior/persistence.md）。
#[test]
fn persist1_uncommitted_write_is_discarded_and_committed_data_survives_reopen() {
    let path = unique_db_path("persist1");
    let _cleanup = CleanupGuard(path.clone());

    // 1. Storage 経由でコミット済みの行を 1 件作る。
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(1, &row(&[1.0, 2.0, 3.0], b"committed"))
            .expect("commit row 1");
    } // ここで Storage（= redb::Database）を drop し、ファイルロックを解放する。

    // 2. 生の redb::Database で同じファイルを再オープンし、書き込みトランザクションを
    //    開始・行を挿入するが、明示的に abort して「commit 前の中断」を再現する。
    {
        let db = Database::open(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            // この raw ハンドルが Storage と同じテーブルを見ていることを確認する
            // （テーブル名が食い違うと、以降の「abort 後に行 2 が消えている」検証が
            // 別テーブルを見ているだけの偽陽性になり得るため）。
            assert!(
                table
                    .get(("tenant-a", 1u64))
                    .expect("get row 1 via raw handle")
                    .is_some(),
                "raw Database handle must see the row committed via Storage"
            );
            table
                .insert(("tenant-a", 2u64), &b""[..])
                .expect("insert into uncommitted txn");
        }
        write_txn.abort().expect("abort uncommitted txn");
    } // Database を drop し、ファイルロックを解放する。

    // 3. Storage で再オープンし、コミット済みの行 1 は無傷、中断した行 2 は存在しないこと
    //    を確認する。
    let storage = Storage::open(&path).expect("reopen storage");
    let committed = storage
        .get("tenant-a", 1)
        .expect("row 1 must survive reopen");
    assert_eq!(committed.embedding, vec![1.0, 2.0, 3.0]);
    assert_eq!(committed.metadata, b"committed");

    // `NotFound` まで確認することで、abort が commit に退行して行 2 が書き込まれてしまう
    // ケースが Codec エラー等の別理由の失敗と混同されて誤って pass するのを防ぐ
    // （`is_err()` だけでは区別できない）。
    let err = storage
        .get("tenant-a", 2)
        .expect_err("aborted write must not be visible after reopen");
    assert!(
        matches!(err, StorageError::NotFound(_)),
        "expected row 2 to be reported as NotFound, got: {err}"
    );
}

// 対象ビヘイビア: PERSIST-2（詳細は関数名・ポインタ: docs/spec/04-behavior/persistence.md）。
// TASK-143 は本テストのスコープ外。
#[test]
fn persist2_incremental_write_preserves_existing_rows() {
    let path = unique_db_path("persist2");
    let _cleanup = CleanupGuard(path.clone());

    let storage = Storage::open(&path).expect("open storage");

    let initial_embeddings: Vec<[f32; 1]> = (0..50u64).map(|i| [i as f32]).collect();
    let initial: Vec<(u64, RowInput<'_>)> = (0..50u64)
        .map(|i| (i, row(&initial_embeddings[i as usize], b"initial")))
        .collect();
    storage.put_batch(&initial).expect("initial batch write");

    let after_initial = storage.scan().expect("scan after initial batch");
    assert_eq!(after_initial.len(), 50);

    // 増分書き込み: 既存範囲に触れず、新規 ID のみを追加する。
    let incremental_embeddings: Vec<[f32; 1]> = (50..80u64).map(|i| [i as f32]).collect();
    let incremental: Vec<(u64, RowInput<'_>)> = (50..80u64)
        .map(|i| {
            (
                i,
                row(&incremental_embeddings[(i - 50) as usize], b"incremental"),
            )
        })
        .collect();
    storage
        .put_batch(&incremental)
        .expect("incremental batch write");

    let after_incremental = storage.scan().expect("scan after incremental batch");
    assert_eq!(after_incremental.len(), 80);

    // 既存データが増分書き込みの影響を受けていないこと（再構築されていないこと）を
    // スポットチェックする。
    let preserved = storage
        .get("tenant-a", 10)
        .expect("original row must remain readable");
    assert_eq!(preserved.embedding, vec![10.0]);
    assert_eq!(preserved.metadata, b"initial");

    let appended = storage
        .get("tenant-a", 65)
        .expect("incrementally written row must be readable");
    assert_eq!(appended.embedding, vec![65.0]);
    assert_eq!(appended.metadata, b"incremental");
}

// 対象ビヘイビア: PERSIST-2（詳細は関数名・ポインタ: docs/spec/04-behavior/persistence.md）。
#[test]
fn persist2_put_batch_discards_whole_transaction_on_mid_batch_encode_failure() {
    let path = unique_db_path("persist2-partial-failure");
    let _cleanup = CleanupGuard(path.clone());

    let storage = Storage::open(&path).expect("open storage");

    // メタデータ長の上限を超える不正な行を 3 件目に混在させる。
    // `engine::storage` の上限定数は非公開のため、実際に超過する値を直接使う。
    const OVERSIZED_METADATA_LEN: usize = 4 * 1024 * 1024 + 1;
    let oversized_metadata = vec![0u8; OVERSIZED_METADATA_LEN];
    let batch = vec![
        (1u64, row(&[1.0], b"ok-1")),
        (2u64, row(&[2.0], b"ok-2")),
        (3u64, row(&[3.0], &oversized_metadata)),
        (4u64, row(&[4.0], b"ok-4")),
    ];

    let result = storage.put_batch(&batch);
    assert!(
        matches!(result, Err(StorageError::Codec(_))),
        "batch containing an oversized row must fail with a codec error, got: {result:?}"
    );

    // トランザクション全体が破棄され、有効な行（1・2・4）も一切反映されていないこと。
    // `NotFound` まで確認することで、「読み取り自体が別の理由で失敗した」ケースと
    // 区別する（`is_err()` だけでは Backend エラー等も誤って合格し得るため）。
    for id in [1u64, 2, 4] {
        let err = storage.get("tenant-a", id).expect_err(&format!(
            "row {id} must not be visible after the batch transaction was discarded"
        ));
        assert!(
            matches!(err, StorageError::NotFound(_)),
            "expected row {id} to be reported as NotFound, got: {err}"
        );
    }
    let scanned = storage.scan().expect("scan after aborted batch");
    assert!(
        scanned.is_empty(),
        "no rows should be committed when the batch transaction is discarded, got: {scanned:?}"
    );
}

// 対象ビヘイビア: PERSIST-4（詳細は関数名・ポインタ: docs/spec/04-behavior/persistence.md）。
#[test]
fn persist4_writes_are_serialized_and_reads_see_snapshot() {
    let path = unique_db_path("persist4");
    let _cleanup = CleanupGuard(path.clone());

    // 既存行を 1 件用意しておく（読み取りスナップショットの比較対象）。
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(1, &row(&[9.0], b"pre-existing"))
            .expect("seed row");
    }

    let db = Arc::new(Database::open(&path).expect("reopen raw database"));

    let (writer_ready_tx, writer_ready_rx) = mpsc::channel::<()>();
    let (release_writer_tx, release_writer_rx) = mpsc::channel::<()>();

    let writer_db = Arc::clone(&db);
    let writer = thread::spawn(move || {
        let write_txn = writer_db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            table
                .insert(("tenant-a", 2u64), &b""[..])
                .expect("insert row 2");
        }
        // 挿入済みだが未コミットである旨をメインスレッドへ通知し、コミット許可を待つ。
        writer_ready_tx.send(()).expect("signal writer ready");
        release_writer_rx
            .recv()
            .expect("wait for commit permission");
        write_txn.commit().expect("commit write txn");
    });

    writer_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer did not signal readiness in time");

    // 書き込み進行中（未コミット）に開始した読み取りは、その時点のスナップショットを
    // 見るべきであり、未コミットの行 2 は見えてはならない。
    {
        let read_txn = db
            .begin_read()
            .expect("begin read txn during pending write");
        let table = read_txn
            .open_table(ROWS_TABLE)
            .expect("open table for read");
        assert!(
            table.get(("tenant-a", 1u64)).expect("get row 1").is_some(),
            "pre-existing committed row must be visible"
        );
        assert!(
            table.get(("tenant-a", 2u64)).expect("get row 2").is_none(),
            "uncommitted row must not be visible to a snapshot reader"
        );
    }

    // 直列化の確認: 別スレッドから 2 本目の書き込みトランザクションを開始しようとしても、
    // 1 本目がコミットするまで `begin_write` は完了しない（排他ロックにより直列化される）
    // ことを確認する。
    //
    // `second_started_tx` の送信はスレッド生成直後（`begin_write` 呼び出し前）に発生する
    // ため、それだけでは「スレッドがまだ `begin_write` に到達していない」可能性と
    // 「`begin_write` がロック待ちでブロックされている」可能性を区別できない
    // （前者でも `!is_finished()` は真になり、偽陽性になり得る）。そこで、
    // `begin_write` が実際に返った直後にのみ真になる `second_acquired` を別途用意し、
    // ロックについての積極的な証拠（「まだ取得できていない」）とする。
    let second_writer_db = Arc::clone(&db);
    let second_acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_acquired_writer = Arc::clone(&second_acquired);
    let (second_started_tx, second_started_rx) = mpsc::channel::<()>();
    let second_writer = thread::spawn(move || {
        second_started_tx
            .send(())
            .expect("signal second writer started");
        let _write_txn = second_writer_db
            .begin_write()
            .expect("begin second write txn (blocks until first commits)");
        second_acquired_writer.store(true, Ordering::SeqCst);
        // 直列化の確認のみが目的のため、2 本目は commit せず drop（abort）する。
    });

    second_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second writer did not start in time");
    // 2 本目が `begin_write` の排他ロック待ちで止まっていることを確認するための短い待機
    // （タイミング計測ではなく、明らかにブロックされていることの確認が目的）。
    thread::sleep(Duration::from_millis(200));
    assert!(
        !second_acquired.load(Ordering::SeqCst),
        "second begin_write must not have acquired the exclusive write lock \
         while the first write txn is still open"
    );

    release_writer_tx
        .send(())
        .expect("release first writer to commit");
    writer.join().expect("writer thread panicked");

    // ロック解放後に 2 本目が実際に進行することを、有限のデッドラインで確認する
    // （リグレッションでロックが解放されないまま止まった場合に、テストが CI の
    // タイムアウトまで無限に `join()` し続けるのではなく、明示的に失敗させるため）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !second_writer.is_finished() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        second_writer.is_finished(),
        "second writer must complete once the first write txn is released \
         (it appears still stuck on begin_write)"
    );
    second_writer.join().expect("second writer thread panicked");
    assert!(
        second_acquired.load(Ordering::SeqCst),
        "second begin_write must have acquired the lock after the first txn was released"
    );

    // コミット後に新たに開始した読み取りは、コミット済みの行 2 を見えるようになる。
    let read_txn = db.begin_read().expect("begin read txn after commit");
    let table = read_txn
        .open_table(ROWS_TABLE)
        .expect("open table for read");
    assert!(
        table
            .get(("tenant-a", 2u64))
            .expect("get row 2 after commit")
            .is_some(),
        "row committed by the first writer must be visible after commit"
    );
}

// デコード fail-closed の統合テスト（不正バイト列を Storage 経由で読んだ場合に、
// 黙殺フォールバックせず Err で拒否されること）。encode/decode 単体の詳細な境界値検証は
// `crates/engine/src/storage.rs` のユニットテストで行う。
#[test]
fn storage_get_rejects_corrupted_row_bytes() {
    let path = unique_db_path("fail-closed");
    let _cleanup = CleanupGuard(path.clone());

    {
        let db = Database::create(&path).expect("create raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            // version バイトのみで、後続のフィールドが一切ないバイト列（意図的な破損）。
            table
                .insert(("tenant-a", 1u64), &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .get("tenant-a", 1)
        .expect_err("malformed row must be rejected, not decoded with defaults");
    // `NotFound` ではなく、デコードそのものが拒否されたことを確認する
    // （テーブル名の取り違え等で行が別テーブルへ書かれてしまった場合、
    // `NotFound` になり得るため、これを区別しないとこのテストは黙殺フォールバックの
    // 有無を検証できない）。
    assert!(
        matches!(err, StorageError::Codec(_)),
        "expected a decode failure (StorageError::Codec), got: {err}"
    );
}

// 対象ビヘイビア: PERSIST-3（詳細はポインタ先を参照: docs/spec/04-behavior/persistence.md）。
// 検証手段: put_batch → 再オープン → get / scan での読み戻し確認。
#[test]
fn persist3_reopen_roundtrip_via_get_and_scan() {
    let path = unique_db_path("persist3-reopen");
    let _cleanup = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage");
        let rows = vec![
            (
                1u64,
                row_with_rls("tenant-a", Visibility::Public, &[1.0, 2.0], b"a-public"),
            ),
            (
                2u64,
                row_with_rls("tenant-a", Visibility::Private, &[3.0], b"a-private"),
            ),
            (
                3u64,
                row_with_rls(
                    "tenant-b",
                    Visibility::Public,
                    &[4.0, 5.0, 6.0],
                    b"b-public",
                ),
            ),
        ];
        storage.put_batch(&rows).expect("seed multi-tenant rows");
    } // Storage を drop し、再オープンで永続化されていることを検証する。

    let storage = Storage::open(&path).expect("reopen storage");

    let row1 = storage
        .get("tenant-a", 1)
        .expect("row 1 must survive reopen");
    assert_eq!(row1.tenant_id, "tenant-a");
    assert_eq!(row1.visibility, Visibility::Public);
    assert_eq!(row1.metadata, b"a-public");

    let row2 = storage
        .get("tenant-a", 2)
        .expect("row 2 must survive reopen");
    assert_eq!(row2.tenant_id, "tenant-a");
    assert_eq!(row2.visibility, Visibility::Private);
    assert_eq!(row2.metadata, b"a-private");

    let row3 = storage
        .get("tenant-b", 3)
        .expect("row 3 must survive reopen");
    assert_eq!(row3.tenant_id, "tenant-b");
    assert_eq!(row3.visibility, Visibility::Public);
    assert_eq!(row3.metadata, b"b-public");

    let mut scanned = storage.scan().expect("scan after reopen");
    scanned.sort_by_key(|r| r.id);
    assert_eq!(scanned.len(), 3);
    assert_eq!(
        scanned
            .iter()
            .map(|r| (r.id, r.tenant_id.as_str(), r.visibility))
            .collect::<Vec<_>>(),
        vec![
            (1, "tenant-a", Visibility::Public),
            (2, "tenant-a", Visibility::Private),
            (3, "tenant-b", Visibility::Public),
        ]
    );
}

// 対象ビヘイビア: PERSIST-3（詳細はポインタ先を参照: docs/spec/04-behavior/persistence.md）。
// 検証手段: raw redb ハンドルでのテーブル構成（list_tables）と
// エンコード済みバイト列のオフセット検査。
#[test]
fn persist3_on_disk_row_entry_layout_via_raw_redb() {
    let path = unique_db_path("persist3-colocated");
    let _cleanup = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(
                1,
                &row_with_rls("tenant-x", Visibility::Private, &[7.0], b"meta"),
            )
            .expect("seed row");
    }

    let db = Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");

    // データベース内に ROWS_TABLE 以外の RLS フィールド専用テーブルが作られていないこと。
    // `list_tables` で実際のテーブル集合を検査することで、「別テーブルに分離されて
    // いない」ことを直接確認する（部分文字列一致による間接証拠ではなく、テーブル構成
    // そのものを見る）。`storage_generation`（TASK-133 P1・全書き込みコミットで単調増加
    // する世代カウンタ。`crate::storage::bump_generation_and_commit` 参照）は RLS
    // フィールドとは無関係の別用途テーブルのため許容する。
    let mut table_names: Vec<String> = read_txn
        .list_tables()
        .expect("list tables")
        .map(|t| t.name().to_string())
        .collect();
    table_names.sort();
    assert_eq!(
        table_names,
        vec!["rows".to_string(), "storage_generation".to_string()],
        "expected only ROWS_TABLE ('rows') and the generation counter table; \
         RLS fields must not live in a separate table"
    );

    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");
    let guard = table
        .get(("tenant-x", 1u64))
        .expect("get row 1")
        .expect("row 1 exists");
    let raw = guard.value();

    // レイアウト: [version(1)][tenant_len(2) le]["tenant-x"(8)][visibility(1)]...
    // 位置指定で tenant_id・visibility の両方が同一エントリのバイト列内にあることを
    // 確認する（部分文字列探索ではなく、実際のオンディスクレイアウトを検査する）。
    let tenant_start = 1 + 2;
    let tenant_end = tenant_start + "tenant-x".len();
    assert_eq!(
        raw.get(tenant_start..tenant_end),
        Some(b"tenant-x".as_slice()),
        "encoded row bytes must contain the tenant_id inline at the expected offset"
    );
    assert_eq!(
        raw.get(tenant_end).copied(),
        Some(0x02u8), // Visibility::Private の永続化コード
        "encoded row bytes must contain the visibility byte immediately after tenant_id"
    );
}
