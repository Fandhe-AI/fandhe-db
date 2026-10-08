//! 電源断シミュレーションによるクラッシュ耐性の再検証テスト（TASK-145、ポインタ:
//! `docs/spec/05-tasks.md`）。関連ビヘイビア: PERSIST-1・PERSIST-3（ポインタ:
//! `docs/spec/04-behavior/persistence.md`）。検証対象の契約内容は上記ポインタ先を
//! 参照。詳細は `docs/design/crash-tolerance-reverification.md` を参照。
//!
//! 実機の電源断は CI で再現できないため、`redb::StorageBackend`（`redb =4.2.0`。
//! `crates/engine/Cargo.toml` の既承認済み依存。新規依存は追加しない）をテストスコープで
//! 実装し、OS の page cache（sync 前の書き込みがメモリ上にのみ存在し、電源断で消失し得る
//! 状態）を決定論的にシミュレーションする（[`PowerLossBackend`]）。乱数は `rand` 等を
//! 追加せず、固定シードの xorshift（[`xorshift64`]）で代替する（dependency-policy.md 準拠）。
//!
//! ## モデルの限界（レポート `docs/design/crash-tolerance-reverification.md` 参照）
//!
//! 本シミュレーションはユーザー空間での近似であり、以下は対象外:
//! - 実デバイス（SSD/HDD）のファームウェアレベルの書き込みキャッシュ・並べ替え
//! - OS カーネルの page cache の実際の write-back 順序（本モデルは「発行順の任意部分集合が
//!   反映される」という単純化で近似する）
//! - `set_len`（ファイル長変更）はメタデータ操作として即時 durable 化する単純化を置く
//!   （多くのファイルシステムでメタデータジャーナリングは別経路のため、単純化として妥当と判断）
//! - シナリオ 3（[`run_partial_writeback_search`]）が探索する「部分集合」は、abort された
//!   トランザクションの `write()` ログに限られる。`redb` は copy-on-write B-tree のため、
//!   コミットに到達しなかったトランザクションの書き込みは新規割当ページ（既存のコミット済み
//!   ルートが一切参照しない領域）にしか行かれない。したがって本モデルの部分集合探索は
//!   **構造的に** コミット済みツリーへ干渉できず、`reopen_from_image` の `Err` 分岐へは
//!   到達し得ない（これは `redb` の頑健性の実証ではなく、本ハーネスの探索空間の限界である）。
//!   ハーネス自体が拒否を検出できることは、コミット済み像への直接バイト破損を用いた
//!   `power_loss_scenario3_corrupted_durable_image_is_rejected`（否定コントロール）で
//!   別途確認する。
//!
//! `Storage`（`crates/engine/src/storage.rs`）の公開 API（`Storage::open`）は
//! `redb::Database::create` 固定で `StorageBackend` を差し替えられないため、
//! バックエンド差し替え済みの `redb::Database` 自体は `redb::Builder::create_with_backend`
//! で raw に開く（本ファイルのシナリオ 1・2・3 はこの raw `redb::Database` を直接操作
//! する。`tests/persistence.rs` と同じ前提）。本番の `Storage::put`/`Storage::get`
//! （＝実際の `encode_row`/`decode_row`）経由での電源断シナリオ（旧シナリオ 4）は
//! `crates/engine/src/storage.rs` 内の `#[cfg(test)]` ユニットテストへ移設した
//! （`Storage` の private フィールドへ crate 内から直接アクセスすることで、
//! 公開 API へバックエンド差し替え用のコンストラクタを増やさないため）。
//! `BackendState`/`PowerLossBackend` の基本実装（commit/sync 契約のモデル本体）は
//! 両テストで共有する必要があるため `crate::storage::power_loss_model` へ分離し、
//! 本ファイルは `#[path]` で同一ソースを取り込んだ上で、部分 write-back 固有の
//! 探索（`apply_subset`/`crash_image`）だけを拡張 `impl` として追加している。

use redb::{Database, ReadableDatabase, TableDefinition};

/// テスト対象のテーブル定義（`crates/engine/src/storage.rs` の `ROWS_TABLE` と同一の
/// キー・値型 `(tenant_id, id)` 複合キー（対象ビヘイビア: TABLE-12）。本ファイルは
/// 行の中身を解釈しないため、値のエンコード詳細に依存しない。物理キー形式を production
/// と揃えておくことで、本ファイルの電源断像が実際の `Storage` の on-disk レイアウトを
/// 正しく模していることを保つ（Issue #206）。
const ROWS_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("rows");

/// 本ファイル全体で使う固定テナント識別子（RLS 相当のテナント境界判定経路には
/// 踏み込まないため単一値で足りる。`crash_tool.rs::CRASH_TOOL_TENANT_ID` と同方針）。
const POWER_LOSS_TENANT_ID: &str = "power-loss-tenant";

// `BackendState`/`PowerLossBackend`（commit/sync 契約のモデル本体）は
// `crates/engine/src/storage.rs` の `#[cfg(test)]` ユニットテストと共有するため、
// 同一ソースを `#[path]` でこのテストバイナリへも取り込む（`crate::storage::power_loss_model`
// 参照）。ここでは部分 write-back 固有の探索（`apply_subset`/`crash_image`）だけを
// 拡張 `impl` として追加する。
#[path = "../src/storage/power_loss_model.rs"]
mod power_loss_model;
use power_loss_model::{BackendState, PowerLossBackend};

impl BackendState {
    /// `log` のうち `indices` に含まれるものだけを、記録順（`indices` は昇順を期待）で
    /// 基底バッファ（`durable` を `len` にリサイズしたもの）へ適用した結果を返す。
    /// 全 `indices` を渡せば `current()` と同じ「現在の見かけ上の状態」になり、空を渡せば
    /// 「直近 sync 時点のまま」の電源断像になる（部分 write-back の並べ替え近似。
    /// シナリオ 2・3 専用）。
    fn apply_subset(&self, indices: &[usize]) -> Vec<u8> {
        let mut buf = self.durable.clone();
        buf.resize(self.len, 0);
        for &i in indices {
            let (offset, data) = &self.log[i];
            let start = *offset as usize;
            let end = start + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[start..end].copy_from_slice(data);
        }
        buf
    }
}

impl PowerLossBackend {
    /// 指定した部分集合（`log` へのインデックス。昇順であること）だけを直近 sync 像へ
    /// 適用した「電源断時点の像」を返す（シナリオ 2・3 専用）。
    fn crash_image(&self, subset: &[usize]) -> Vec<u8> {
        self.state
            .lock()
            .expect("lock poisoned")
            .apply_subset(subset)
    }
}

/// テスト内蔵の固定シード xorshift64（`rand` 等の新規依存を追加しないための代替。
/// dependency-policy.md 準拠）。CI 用の小規模な部分集合探索にのみ使う。
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        // シード 0 は xorshift の不動点になるため回避する。
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// `total` 件の書き込みログのうち、確率的に選んだ部分集合（昇順インデックス列）を返す。
/// page cache の write-back が「発行順の任意部分集合だけが反映される」ことの近似
/// （モジュール doc「モデルの限界」参照）。
fn random_subset(rng: &mut Xorshift64, total: usize) -> Vec<usize> {
    (0..total)
        .filter(|_| rng.next_u64().is_multiple_of(2))
        .collect()
}

/// 新規（空）の [`PowerLossBackend`] 上に `redb::Database` を開く。
///
/// `cache_size` は既定（1GiB）のままだと、小規模なテストデータではページが
/// `redb` 内部の write buffer に留まり続け、`commit()`（＝ `sync_data()`）まで
/// `StorageBackend::write()` が一切呼ばれない（トランザクション全体がメモリ内で
/// 完結する）。これはシナリオ 2・3 が意図する「commit 前にページの一部が
/// バックエンドへ書き戻される」状況を作れないため、`set_cache_size` で
/// 小さな値に設定し、write buffer からの追い出し（eviction）を確実に起こす。
fn open_fresh() -> (PowerLossBackend, Database) {
    let backend = PowerLossBackend::new();
    let mut builder = redb::Builder::new();
    builder.set_cache_size(4096 * 4);
    let db = builder
        .create_with_backend(backend.clone())
        .expect("open fresh database on PowerLossBackend");
    (backend, db)
}

/// 指定バイト列を初期像として `redb::Database` を開き直す（「電源断後の再起動」に相当）。
/// 破損像で開けない場合は `Err` をそのまま返す（fail-closed の判定はテスト側で行う）。
fn reopen_from_image(image: Vec<u8>) -> Result<Database, redb::DatabaseError> {
    redb::Builder::new().create_with_backend(PowerLossBackend::from_bytes(image))
}

fn insert_row(db: &Database, id: u64, value: &[u8]) {
    let write_txn = db.begin_write().expect("begin write txn");
    {
        let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
        table
            .insert((POWER_LOSS_TENANT_ID, id), value)
            .expect("insert row");
    }
    write_txn.commit().expect("commit write txn");
}

fn get_row(db: &Database, id: u64) -> Option<Vec<u8>> {
    let read_txn = db.begin_read().expect("begin read txn");
    let table = match read_txn.open_table(ROWS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return None,
        Err(e) => panic!("unexpected open_table error: {e}"),
    };
    table
        .get((POWER_LOSS_TENANT_ID, id))
        .expect("get row")
        .map(|guard| guard.value().to_vec())
}

// シナリオ 1（対応する契約: PERSIST-1・PERSIST-3。ポインタ: `docs/spec/04-behavior/persistence.md`）。
//
// `Storage::put`（≒ 本テストの `insert_row`）が返った時点で `redb` の既定 durability
// （`Durability::Immediate`）により `sync_data()` まで完了しているため、`durable_snapshot()`
// をそのまま電源断像として使えばよい（`log` は commit 完了後には空になっている）。
#[test]
fn power_loss_scenario1_committed_data_survives_crash_after_commit_response() {
    let (backend, db) = open_fresh();
    insert_row(&db, 1, b"committed-a");
    insert_row(&db, 2, b"committed-b");

    assert_eq!(
        backend.pending_write_count(),
        0,
        "Immediate durability の commit 完了後は log が sync 済みで空になっているはず\
         （観測: Storage の commit は応答前に sync している）"
    );

    let crash_image = backend.durable_snapshot();
    drop(db);

    let recovered = reopen_from_image(crash_image).expect("reopen after crash must succeed");
    assert_eq!(get_row(&recovered, 1), Some(b"committed-a".to_vec()));
    assert_eq!(get_row(&recovered, 2), Some(b"committed-b".to_vec()));
}

// シナリオ 2（対応する契約: PERSIST-1・PERSIST-3。ポインタ: `docs/spec/04-behavior/persistence.md`）。
#[test]
fn power_loss_scenario2_mid_transaction_crash_discards_whole_transaction() {
    let (backend, db) = open_fresh();
    insert_row(&db, 1, b"pre-existing");

    // 最終 sync（コミット）に到達しない書き込みを発生させる。`open_fresh` で cache_size を
    // 小さく設定しているため、十分な件数を書けば `redb` 内部の write buffer から
    // 追い出しが発生し、commit 前でも `StorageBackend::write()` 経由で `log` に記録される
    // （それでも `commit()` を呼ばない＝ Immediate durability の最終 sync には到達しないため、
    // 電源断時点で `durable` には反映されない）。
    let write_txn = db.begin_write().expect("begin write txn");
    {
        let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
        for i in 0..64u64 {
            table
                .insert((POWER_LOSS_TENANT_ID, 1_000 + i), &[0u8; 512][..])
                .expect("insert uncommitted row");
        }
        table
            .insert((POWER_LOSS_TENANT_ID, 2u64), &b"never-committed"[..])
            .expect("insert uncommitted row");
    }

    // 電源断像は、トランザクションが commit も abort もしていない「途中」の時点で
    // 採取する。先に `write_txn.abort()` を呼んでから採取すると、abort（正常な
    // ロールバック処理）がバックエンドへ行う後始末の書き込みが電源断像に混入し得るため、
    // 「トランザクション途中（最終 sync 前）の電源断」と「正常終了（abort 完了）後」を
    // 混同することになる（指摘: abort 完了後の状態はクラッシュ像ではない）。
    let pending = backend.pending_write_count();
    assert!(
        pending > 0,
        "未コミットのページ書き込みが log に記録されているはず \
         （0 件だと本テストは何も検証していないことになる）"
    );

    // 「電源断時点の像」として、abort された txn が実際にバックエンドへ書き戻した
    // ページをすべて反映した像（crash_image(全 indices)）を使う。こうすることで、
    // `open_fresh` の `set_cache_size` 縮小によって発生させた部分 write-back が
    // 実際に電源断像へ混入していることを `assert_ne!` で確認したうえで、それでも
    // なお未コミット txn の内容は読めない（commit していないためルートページが
    // 更新されておらず、不到達）ことを検証する。
    //
    // 比較対象のベースラインには pre_txn_image（未コミット txn 開始前のスナップショット）
    // ではなく `crash_image(&[])`（write() log を 1 件も適用しない像）を使う。
    // `set_len` はモデルの単純化により `durable` を即時更新する（モジュール doc「モデルの限界」
    // 参照）ため、txn 内でのファイル成長（growth）だけでも pre_txn_image とは長さが変わり、
    // 実際に `write()` されたページ内容が反映されたかどうかに関わらず `assert_ne!` が
    // 成立してしまう（＝ growth のみを検出して「write-back が混入した」と誤認する）。
    // `crash_image(&[])` は同じ growth 後の長さ・durable を起点にしつつ log 適用のみを
    // 0 件にした像なので、これとの差分は growth ではなく `write()` ログの内容適用に
    // 起因することが保証される。
    let all_indices: Vec<usize> = (0..pending).collect();
    let no_writeback_baseline = backend.crash_image(&[]);
    let crash_image_with_partial_writeback = backend.crash_image(&all_indices);
    assert_ne!(
        crash_image_with_partial_writeback, no_writeback_baseline,
        "部分 write-back（write() ログの適用）が電源断像へ実際に混入していなければ、\
         本テストは commit 前のページ write-back を検証したことにならない \
         （growth のみに起因する差分ではないことを、同じ growth 後の長さを持つ \
         no_writeback_baseline との比較で担保する）"
    );

    // 電源断像の採取が完了したので、ここでテストプロセスのリソース解放として
    // abort する（採取済みの `crash_image_with_partial_writeback` は abort より前の
    // スナップショットなので、この呼び出しが assertion に影響することはない）。
    write_txn.abort().expect("abort uncommitted txn");

    drop(db);
    let recovered = reopen_from_image(crash_image_with_partial_writeback)
        .expect("reopen after mid-transaction crash must succeed");
    assert_eq!(
        get_row(&recovered, 1),
        Some(b"pre-existing".to_vec()),
        "電源断前にコミット済みだった行は無傷でなければならない"
    );
    assert_eq!(
        get_row(&recovered, 2),
        None,
        "commit に到達しなかったトランザクションの内容は電源断後に見えてはならない \
         （部分 write-back 像を使っても、ルートページは未コミットのため不到達）"
    );
    for i in 0..64u64 {
        assert_eq!(
            get_row(&recovered, 1_000 + i),
            None,
            "id={}: commit に到達しなかった未コミット行が部分 write-back \
             （他の行の write() 混入）によって可視化されてはならない",
            1_000 + i
        );
    }
}

// シナリオ 3（対応する契約: PERSIST-1・PERSIST-3。ポインタ: `docs/spec/04-behavior/persistence.md`）。
// 部分 write-back 像（page cache の書き戻し順序不定の近似）からの再オープン結果を検証する。
// 合否基準は本モジュール冒頭の TASK-145/PERSIST-1/PERSIST-3 ポインタ・security.md
// 「不安全な設計」（fail-closed 原則）を参照し、本コメントには転記しない。
//
// CI 実行分は固定シード・有限反復（数秒以内）に収める。より広い探索は
// `power_loss_scenario3_partial_writeback_extended_search`（`#[ignore]`）で行う。
#[test]
fn power_loss_scenario3_partial_writeback_is_either_consistent_or_rejected() {
    run_partial_writeback_search(32, 64);
}

// シナリオ 3 の拡張反復（ローカル実行用。CI では実行しない）。
// `cargo test -p fandhe-db-engine -- --ignored` で実行し、結果は
// `docs/design/crash-tolerance-reverification.md` に記録する。
#[test]
#[ignore = "拡張反復のためローカル実行用（CI では #[ignore] を外さない）"]
fn power_loss_scenario3_partial_writeback_extended_search() {
    run_partial_writeback_search(2048, 64);
}

/// シナリオ 3 の本体。シード `1..=seed_count` それぞれについて、直近コミット完了時点の
/// バイト列と、その後の（commit まで到達しなかった）追加書き込みログから部分集合を
/// 固定シードで選び、その部分 write-back 像を電源断像として再オープンを試みる。
fn run_partial_writeback_search(seed_count: u64, extra_writes: u64) {
    let mut opened_consistent: u64 = 0;
    let mut rejected: u64 = 0;

    for seed in 1..=seed_count {
        let (backend, db) = open_fresh();
        insert_row(&db, 1, b"committed-before-crash");
        let committed_snapshot_len = backend.pending_write_count();
        assert_eq!(
            committed_snapshot_len, 0,
            "commit 直後は log が空（sync 済み）のはず"
        );

        // commit まで到達しない追加の書き込みトランザクションを 1 本開始する
        // （page cache 上には乗るが、電源断時点でどこまで反映されているかは不定）。
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            for i in 0..extra_writes {
                table
                    .insert((POWER_LOSS_TENANT_ID, 100 + i), &[0u8; 512][..])
                    .expect("insert uncommitted row");
            }
        }

        // 電源断像（ログ長・部分集合・crash_image）は、トランザクションが commit も
        // abort もしていない時点で採取する。scenario2 と同じ理由（abort 完了後に採取すると
        // 正常なロールバック処理による後始末書き込みが電源断像に混入しかねない）による。
        let total_log_len = backend.pending_write_count();
        // 部分集合の探索が意味を持つには、まず「commit に到達しなかった書き込みが
        // 実際にバックエンドへ記録されている」ことが前提になる。これが 0 件のままだと
        // `random_subset` は常に空集合しか返せず、以降の全反復が
        // `crash_image(&[])` == 直近 sync 像（＝行 1 のみのシナリオ 1 相当）を
        // 再オープンするだけの空検証になってしまう。
        assert!(
            total_log_len > 0,
            "seed={seed}: 未コミットのページ書き込みが log に記録されていない。\
             open_fresh の cache_size もしくは extra_writes（={extra_writes}）を見直すこと \
             （0 件のままだと本シナリオは部分 write-back を一切検証しない）"
        );

        let mut rng = Xorshift64::new(seed);
        let subset = random_subset(&mut rng, total_log_len);
        let durable_before = backend.durable_snapshot();
        let crash_image = backend.crash_image(&subset);
        // subset が実際にバイト列を変化させていることを確認する。`random_subset` は
        // 空集合を返しうるため、これがないと当該反復が `crash_image == durable_before`
        // （＝シナリオ 1 相当の空検証）に縮退していても気づけない。
        assert_ne!(
            crash_image, durable_before,
            "seed={seed}: 選ばれた部分集合が電源断像へ何も反映していない \
             （空検証に縮退している）"
        );

        // 電源断像の採取が完了したので、ここでテストプロセスのリソース解放として
        // abort する（採取済みの `crash_image` は abort より前のスナップショットなので、
        // この呼び出しが以降の assertion に影響することはない）。
        write_txn.abort().expect("abort uncommitted txn");
        drop(db);

        match reopen_from_image(crash_image) {
            Err(_) => {
                // fail-closed: 破損像を明示的に拒否するのは合格。
                rejected += 1;
            }
            Ok(recovered) => {
                // 開けた場合の合否基準は PERSIST-1・PERSIST-3（ポインタ上記）を参照。
                let row1 = get_row(&recovered, 1);
                assert_eq!(
                    row1,
                    Some(b"committed-before-crash".to_vec()),
                    "seed={seed}: 部分 write-back 像の再オープン後、行 1 の内容が\
                     電源断前のコミット内容と不一致"
                );
                for i in 0..extra_writes {
                    assert_eq!(
                        get_row(&recovered, 100 + i),
                        None,
                        "seed={seed}, id={}: commit に到達しなかった未コミット行が部分 \
                         write-back によって可視化されてはならない（部分適用像を \
                         「正常」と誤判定してはならない）",
                        100 + i
                    );
                }
                opened_consistent += 1;
            }
        }
    }

    // 実測した Ok/Err の内訳を記録する（`cargo test -- --nocapture` で観測し、
    // `docs/design/crash-tolerance-reverification.md` の結果表へ転記する。未計測のまま
    // 「多くは拒否される」等と根拠なく報告書に書かないための実測値）。
    //
    // 注意: `Err`（拒否）は fail-closed の観点では合格の結果であり、本テストは
    // `rejected` の値そのものをアサートしない（拒否が発生しても失敗にしてはならない）。
    // モジュール doc「モデルの限界」に記載の通り、本探索が扱う部分集合は abort された
    // トランザクションの書き込み（新規割当ページのみ）に限られ、コミット済みツリーには
    // 構造的に干渉できないため、この探索空間では実測上 `rejected == 0` が続いている
    // （`redb` の頑健性の実証ではなく、探索空間の限界に起因する）。拒否経路が実際に
    // 機能することは、コミット済み像への直接バイト破損を用いた
    // `power_loss_scenario3_corrupted_durable_image_is_rejected`（否定コントロール）で
    // 別途検証している。
    println!(
        "power_loss scenario3: seed_count={seed_count} extra_writes={extra_writes} \
         opened_and_consistent={opened_consistent} rejected_fail_closed={rejected}"
    );
}

// シナリオ 3 の否定コントロール: コミット済みの電源断像そのもの（`durable_snapshot()`）を
// 直接バイト破損させた場合、`reopen_from_image` が実際に `Err` を返すことを確認する。
// `run_partial_writeback_search` は abort された txn の部分集合適用ではコミット済みツリーへ
// 構造的に干渉できず拒否経路（`Err` 分岐）へ到達し得ない（モジュール doc「モデルの限界」
// 参照）。本テストはそれとは別に、ハーネス自体が破損像を実際に拒否できることを直接示す
// （テストの拒否分岐が到達不能なコードではないことの証明）。
#[test]
fn power_loss_scenario3_corrupted_durable_image_is_rejected() {
    let (backend, db) = open_fresh();
    insert_row(&db, 1, b"committed-before-corruption");

    let mut corrupted_image = backend.durable_snapshot();
    drop(db);

    assert!(
        corrupted_image.len() >= 64,
        "破損させる先頭 64 バイトを確保できる程度にはコミット済み像が存在するはず"
    );
    for byte in corrupted_image.iter_mut().take(64) {
        *byte = !*byte;
    }

    let result = reopen_from_image(corrupted_image);
    assert!(
        result.is_err(),
        "コミット済み像の先頭 64 バイトを反転させた場合、reopen_from_image は \
         明示的な Err を返さなければならない（fail-closed。実測でこの反転が \
         Err を引き起こすことを確認済み。内部レイアウトの主張はしない）。\
         Ok になった場合はハーネスが破損を検出できていないことを意味し、\
         run_partial_writeback_search の rejected==0 が「拒否経路が機能しない」ことに \
         起因していないかを疑う必要がある"
    );
}

// 旧シナリオ 4（commit 完了後の電源断像から再オープンしても、行に含まれる全フィールドが
// 保持されることの検証）は `crates/engine/src/storage.rs` 内の `#[cfg(test)]` ユニットテスト
// （`storage::tests::power_loss::power_loss_scenario4_rls_fields_survive_crash_after_commit`）
// へ移設した（モジュール doc 参照）。
