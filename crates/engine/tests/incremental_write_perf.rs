//! `engine::storage::Storage` の性能回帰テスト（TASK-143、対象ビヘイビア: PERSIST-2。
//! ポインタ: `docs/spec/04-behavior/persistence.md`）。
//!
//! `crates/engine/tests/persistence.rs`（TASK-140/141）は PERSIST-2 の正しさ（既存行が
//! 増分書き込みで破壊されないこと）のみを検証し、性能基準の回帰測定は TASK-143 に
//! 切り出されていた。本ファイルはその回帰テストを追加し、
//! 「既存データを持つ DB への増分書き込みが、全体再構築より十分速いこと」を
//! CI で検出可能にする。
//!
//! 計測方針:
//! - 計測対象は `put_batch` 1 回（encode＋トランザクション＋commit）の所要時間のみとし、
//!   `Storage::open` の時間は両側とも計測から除外する（DB ファイルオープンのコストは
//!   増分・全体再構築の両方に共通で、比較の本質と無関係なため）。
//! - 共有 CI ランナーのノイズを吸収するため、ウォームアップ 1 回の後、増分・全体再構築を
//!   ラウンドごとに交互（ABAB）に複数回計測し、加算的ノイズに強い最小値同士で比較する
//!   （Issue #1164。判定閾値は据え置き、負荷が片側だけに乗る計測順と推定量のみ改めた）。
//! - 増分側の各ラウンドは、素の（ウォームアップ未適用の）ベースライン DB ファイルを
//!   都度複製した上で計測する。1 つの DB に増分をラウンドをまたいで積み上げていくと、
//!   計測時点の既存行数がラウンドごとに変化してしまい、全ラウンド固定の総行数で
//!   計測する全体再構築側と比較条件がずれるため（増分後の総行数と全体再構築の総行数を
//!   毎ラウンド一致させることが本テストの比較契約）。
//! - 判定閾値・ワークロード規模はいずれも本テスト固有の計測パラメータであり、
//!   debug/release いずれのビルド設定でも安定して通るようマージンを持たせている
//!   （spec 所定の完了条件の値そのものではなく、実測値・spec 本文も転記しない。
//!   対象ビヘイビアは TASK-143／PERSIST-2／`docs/spec/04-behavior/persistence.md` を参照）。
//!
//! `tests/persistence.rs` とのヘルパ（`unique_db_path` / `CleanupGuard`）は Issue #173 で
//! `crates/engine/src/test_util/temp_db.rs` へ一本化した。行生成ヘルパはこのファイル固有の
//! ままとし、小さく複製するに留める（既存ファイルへの変更を避けスコープを最小化する意図）。

use std::time::{Duration, Instant};

use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// 本テストの計測（`put_batch` の実処理時間）は実プロセスの CPU 時間に依存するため、
// 並走する他の計測系テストの負荷から隔離する（Issue #281 codex-review P2 指摘。
// `tests/incremental_recall.rs` が持つタイミングロックを本ファイルが取得しないため、
// `cargo test` が別プロセスとして並列起動するそちらのバイナリと計測区間が並走し
// 得た）。同じロックファイルパスを共有する
// `crates/engine/src/test_util/timing_lock.rs` へ `#[path]` で相乗りし、バイナリを
// またいだ直列化を成立させる（同モジュールのドキュメンテーションコメント参照）。
#[path = "../src/test_util/timing_lock.rs"]
mod timing_lock;
use timing_lock::acquire_timing_lock;

/// 全行に付与するダミーのテナント識別子（本テストは性能測定のみが目的で、
/// テナント境界の判定経路には踏み込まない。空文字列は `RowInput` 側で拒否されるため
/// 非空の固定値を使う）。
const TENANT_ID: &str = "tenant-a";

/// 埋め込みの次元数（外部クレートへの新規依存を避けるため、決定的な擬似乱数で生成する）。
const EMBEDDING_DIM: usize = 128;

/// ベースライン規模: 増分書き込みの前提として事前に構築しておく行数。
/// release ビルドでは `put_batch` 1 回あたりの commit（fsync）固定費が支配的になり、
/// ベースラインが小さいと `t_inc` が行数をほぼ反映しない領域（固定費支配）に入って
/// しまい、比率が本来測りたい「行数に対する増分コスト」を表さなくなる。ベースラインを
/// 増やして全体再構築側の行コストを相対的に大きくすることで行コスト支配の領域に
/// 移し、debug/release 双方で判定閾値に対し十分なマージンを持たせている
/// （これは閾値の緩和ではなく、閾値が意味を持つ計測条件への調整）。
const BASELINE_ROWS: u64 = 100_000;

/// 1 回の増分書き込みで追加する行数（ベースラインに対して十分小さく保ち、
/// 判定閾値に対して現実的なマージンを持たせる）。
const INCREMENTAL_ROWS: u64 = 400;

/// ノイズ対策として、増分・全体再構築それぞれを複数回計測し最小値を取る回数。
const MEASUREMENT_ROUNDS: usize = 5;

/// 判定閾値の分母（増分書き込みは全体再構築の `1 / RATIO_THRESHOLD_DENOM` 以下の
/// 時間で完了すること）。本テストの計測パラメータであり、アサーション弱体化は行わない
/// （`.claude/rules/coding-rust.md` 参照）。対象ビヘイビアは TASK-143／PERSIST-2 を参照。
const RATIO_THRESHOLD_DENOM: u32 = 10;

/// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。
/// テストデータの生成にのみ使い、暗号用途ではない。
struct Xorshift32(u32);

impl Xorshift32 {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn next_f32(&mut self) -> f32 {
        // [0, 1) に正規化する。分布の質は問題にせず、決定的で毎回異なる値であれば十分。
        (self.next_u32() as f64 / u32::MAX as f64) as f32
    }
}

fn make_embedding(rng: &mut Xorshift32) -> Vec<f32> {
    (0..EMBEDDING_DIM).map(|_| rng.next_f32()).collect()
}

fn make_metadata(id: u64) -> Vec<u8> {
    format!("row-{id}").into_bytes()
}

/// 行データの所有型表現。`RowInput` はライフタイム付き借用のみを持つため、
/// `put_batch` に渡す直前まではこの所有型で保持しておく（参照実装の `Box::leak` による
/// `'static` 化はプロセス終了までメモリを解放しないため、リークを伴わない形に改めた）。
struct OwnedRow {
    id: u64,
    embedding: Vec<f32>,
    metadata: Vec<u8>,
}

/// 指定した ID 範囲（`start..start+count`）分の行を決定的に生成する。
fn make_rows(start: u64, count: u64, seed: u32) -> Vec<OwnedRow> {
    let mut rng = Xorshift32(seed | 1); // seed 0 は xorshift の不動点になるため奇数化する。
    (0..count)
        .map(|i| {
            let id = start + i;
            OwnedRow {
                id,
                embedding: make_embedding(&mut rng),
                metadata: make_metadata(id),
            }
        })
        .collect()
}

/// 所有型の行データを、`put_batch` が要求する `(id, RowInput)` の借用スライス形へ変換する。
fn borrow_rows(rows: &[OwnedRow]) -> Vec<(u64, RowInput<'_>)> {
    rows.iter()
        .map(|row| {
            (
                row.id,
                RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &row.embedding,
                    metadata: &row.metadata,
                },
            )
        })
        .collect()
}

/// 計測値の代表値として最小値を返す（`values` は非空であること、呼び出し側で保証する）。
/// 負荷ノイズ（CPU 競合・fsync 遅延）は加算的で、試行の過半が汚染されると中央値も
/// 崩れる。最小値は「本来のコスト」の推定量であり、O(N) の退行は最小値も押し上げるため
/// 検出力は落ちない（判定閾値は不変。Issue #1164）。
fn robust_estimate(values: Vec<Duration>) -> Duration {
    values.into_iter().min().unwrap_or(Duration::ZERO)
}

// 対象ビヘイビア: PERSIST-2（ポインタ: docs/spec/04-behavior/persistence.md、TASK-143）。
// 増分書き込みが全体再構築より十分速いことを判定閾値（RATIO_THRESHOLD_DENOM）で検証する。
#[test]
fn persist2_incremental_write_completes_within_ratio_threshold_of_full_rebuild() {
    let _timing_guard = acquire_timing_lock();

    // 1. ベースライン DB を事前構築する（この書き込み自体は計測対象外）。
    let baseline_path = unique_db_path("baseline");
    let _baseline_cleanup = CleanupGuard(baseline_path.clone());
    let baseline_storage = Storage::open(&baseline_path).expect("open baseline storage");
    let baseline_rows = make_rows(0, BASELINE_ROWS, 0x1234_5678);
    baseline_storage
        .put_batch(&borrow_rows(&baseline_rows))
        .expect("build baseline dataset");
    drop(baseline_storage);

    // 2. ウォームアップ（ファイルシステムキャッシュ等の初回コストを計測から除外する）。
    //    素のベースライン DB を複製した使い捨てファイル上で行い、以降の各ラウンドが
    //    複製元とする `baseline_path` 自体は変更しない（3. 参照）。
    {
        let warmup_path = unique_db_path("warmup");
        let _warmup_cleanup = CleanupGuard(warmup_path.clone());
        std::fs::copy(&baseline_path, &warmup_path).expect("copy baseline for warmup");
        let storage = Storage::open(&warmup_path).expect("open warmup storage");
        let warmup_rows = make_rows(BASELINE_ROWS, INCREMENTAL_ROWS, 0xdead_beef);
        storage
            .put_batch(&borrow_rows(&warmup_rows))
            .expect("warmup incremental batch write");
    }

    // 3. 増分書き込みと全体再構築を、ラウンドごとに交互（ABAB）に計測する。
    //    増分側の各ラウンドは素のベースライン DB（行数 = BASELINE_ROWS で固定）を新規
    //    ファイルへ複製してから書き込み、計測時点の既存行数を毎ラウンド揃える。これにより
    //    増分後の総行数（BASELINE_ROWS + INCREMENTAL_ROWS）が全体再構築側の総行数と一致し、
    //    比較条件がラウンドをまたいでずれない。全体再構築側は毎ラウンド同じ総行数を新規 DB へ
    //    一括で書き込む。片側の全ラウンドを先に回すと一時的な負荷が片側だけに乗って
    //    比が歪むため、交互にして負荷変動を両側へ均等に乗せる（Issue #1164）。
    let mut incremental_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    let mut full_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS as u64 {
        {
            let round_path = unique_db_path(&format!("incremental-round-{round}"));
            let _round_cleanup = CleanupGuard(round_path.clone());
            std::fs::copy(&baseline_path, &round_path).expect("copy pristine baseline for round");
            let storage = Storage::open(&round_path).expect("open round storage");
            let rows = make_rows(
                BASELINE_ROWS,
                INCREMENTAL_ROWS,
                0x9e37_79b9u32.wrapping_add(round as u32),
            );
            let batch = borrow_rows(&rows);

            let started = Instant::now();
            storage
                .put_batch(&batch)
                .expect("incremental batch write (measured)");
            incremental_durations.push(started.elapsed());
        }
        {
            let full_path = unique_db_path(&format!("full-rebuild-{round}"));
            let _full_cleanup = CleanupGuard(full_path.clone());
            let storage = Storage::open(&full_path).expect("open full-rebuild storage");
            let rows = make_rows(
                0,
                BASELINE_ROWS + INCREMENTAL_ROWS,
                0x1357_9bdfu32.wrapping_add(round as u32),
            );
            let batch = borrow_rows(&rows);

            let started = Instant::now();
            storage
                .put_batch(&batch)
                .expect("full rebuild batch write (measured)");
            full_durations.push(started.elapsed());
        }
    }

    let t_inc = robust_estimate(incremental_durations);
    let t_full = robust_estimate(full_durations);
    let ratio = t_inc.as_secs_f64() / t_full.as_secs_f64().max(f64::EPSILON);

    // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
    println!("persist2 incremental write perf: t_inc={t_inc:?} t_full={t_full:?} ratio={ratio:.4}");

    // `Duration` 同士の乗算で比較し、浮動小数の除算誤差を判定から排除する
    // （`ratio` はログ出力にのみ使い、判定には使わない）。
    assert!(
        t_inc.saturating_mul(RATIO_THRESHOLD_DENOM) <= t_full,
        "incremental write ({t_inc:?}) must complete within 1/{RATIO_THRESHOLD_DENOM} of full \
         rebuild ({t_full:?}), ratio={ratio:.4}"
    );
}
