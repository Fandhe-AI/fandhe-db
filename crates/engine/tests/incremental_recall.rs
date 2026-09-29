//! 増分インデックス（ファイル形 `INSERT` → チャンク化 → `Embedder` によるベクトル化 →
//! 置換書き込み）の回帰テスト（TASK-121、対象ビヘイビア: INDEX-1, INDEX-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-121・`docs/spec/04-behavior/indexing.md` INDEX-1,
//! INDEX-2）。
//!
//! **位置づけ**: 本テストは TASK-121（対象ビヘイビア: INDEX-1, INDEX-2。上記は
//! ポインタ参照のみ）に関連する、本リポ独自の回帰テストである。README で公開済みの
//! 要点（ファイル形 INSERT → チャンク化 → ベクトル化 → 同一パス置換書き込み、および
//! 追加ファイルが検索で取得可能になること）に基づいて、本テストが独自に設定した
//! 基準（測定方法・閾値・アサーション）を検証するものであり、private spec の受け入れ
//! 基準の転記・具体化ではない。
//!
//! `tests/incremental_index.rs`（TASK-120）は検索反映・置換セマンティクス・
//! fail-closed 経路を単発挿入で検証し、`tests/incremental_write_perf.rs`（TASK-143・
//! PERSIST-2）は `Storage::put_batch` **単体**（ストレージ層のみ）の増分/全体再構築比を
//! 検証する。本ファイルはその間を埋め、`execute_insert_sql` のファイル形挿入
//! **パイプライン全体**（チャンク化＋埋め込み＋行バッファ構築＋書き込み。
//! `InsertOutcome.incremental` が返す `IndexTiming`）を対象に、既存コーパスが積まれた
//! 状態での「1 ファイル追加の性能」の代替回帰指標（`index1_*`）と「検索への反映」
//! （INDEX-2）を固定する。
//!
//! **Issue #281（PR #241 の codex-review P2 指摘を受けた見直し）**: 従来の
//! `index1_incremental_indexing_completes_within_ratio_threshold_of_full_rebuild` 単体
//! では、単一ファイル挿入が既存コーパス全体を再処理する（再チャンク化・再埋め込み・
//! 全行再書き込みを既存コーパス規模分繰り返す）O(N) 実装へ退行した場合に、`t_inc` も
//! `t_full` も同じ増分経路を N 回呼ぶ側に乗るため、比較が両方 O(N) 側へシフトして
//! 判定式を素通りしてしまう経路があった（`t_inc` は O(N)、`t_full` は増分経路を
//! 65 回呼ぶため O(N²) となり `t_inc * RATIO_THRESHOLD_DENOM <= t_full` は依然成立し
//! 得る）。本ファイルはこれを複数本柱の判定へ再構成する。
//!
//! **検出対象（本テストが固定する性質）**: 単一ファイル挿入が既存コーパス規模 N に
//! 比例して重い処理（再チャンク化・再埋め込みを既存コーパス規模分繰り返す）を行う
//! 「再処理型」の退行。柱 1・4 の直接観測は `Embedder::embed_batch` の呼び出し回数・
//! 入力テキスト総数という**再埋め込み量**を計装しており、これが検出できるのは
//! 「余分な再チャンク化・再埋め込みを伴う」退行に限られる（下記検出対象外参照）。
//!
//! **検出対象外（PR #296 codex-review P2 指摘。別 Issue の対象）**: 埋め込み呼び出し
//! 回数・入力テキスト総数が正常値のまま（＝再埋め込みは発生しないまま）、既存の
//! 埋め込みベクトルを使い回して行を丸ごと再書き込みする、または
//! `tenant::replace_typed_rows_by_text_key` 呼び出し自体を余分に反復する、といった
//! 「ストレージ書き込みのみの再処理型」退行は、本テストの計装（`CountingEmbedder`
//! による `Embedder::embed_batch` 観測）では検出できない。この検出には
//! `Storage`（`src/storage.rs`。`redb` を直接ラップする具象型で、`EngineCore` から
//! trait 経由の差し替えができない）自体への書き込み回数計装という engine 側の
//! production コード変更が要り、本テストファイル（テスト専用計装の追加のみが
//! スコープ）の変更では解消できない別 Issue の対象とする（本パラグラフが柱 4 の
//! 契約を「再埋め込み型退行の検出」に限定して明確化する）。
//!
//! **検出対象外（別 Issue の対象。従来からの既知の設計事項）**: 実装上、
//! `tenant::replace_typed_rows_by_text_key`（同一パスの既存行を特定するための置換
//! キー探索）はテナント名前空間内の既存行を線形走査するため、増分 1 ファイルの所要
//! 時間は既存コーパス規模に対して緩やかに線形増加する。この軽量な線形走査コストの
//! 解消（キー探索の索引化）は redb スキーマ・障害回復台帳の不変条件に関わる engine
//! 側の設計変更であり、本テストの回帰検知パラメータ調整では解消できない別 Issue の
//! 対象とする。本テストの判定係数（[`SCALING_SLACK`] 等）はこの軽量な線形走査の傾きを
//! 許容しつつ「再処理型」の桁違いな傾きを検出できるよう校正している。
//!
//! **4 本柱の判定**（すべて本テスト固有の回帰検知パラメータであり、private spec の
//! 数値基準・実測値の転記ではない）:
//! 1. **カウント判定**（[`index1_single_file_insert_work_is_independent_of_corpus_size`]。
//!    決定的・時間非依存で最優先）: テスト専用 [`CountingEmbedder`] で
//!    `Embedder::embed_batch` の呼び出し回数・入力テキスト総数を数え、単一ファイル
//!    挿入がコーパス規模に関わらず「1 回・チャンク数分のテキスト」に収まることを
//!    固定する。ただしこの経路（`execute_insert_sql`／`index_file`）は一括投入経路
//!    （`execute_insert_sql_batch`／`index_file_batch`）とは別コード経路であり、
//!    柱 4 が担うカバレッジをここでは代替しない。
//! 2. **スケーリング判定**（[`index1_single_file_insert_time_does_not_scale_with_corpus_size`]。
//!    比率ベース）: 小規模／大規模コーパスでの単一ファイル挿入時間が [`SCALING_SLACK`]
//!    倍を超えて増加しないことを固定する。
//! 3. **全体再構築比判定**（[`index1_incremental_indexing_completes_within_ratio_threshold_of_full_rebuild`]。
//!    既存テストの再定義）: `t_full` を増分経路の逐次累積ではなく、一括投入 API
//!    （`EngineCore::execute_insert_sql_batch`。TASK-122・INDEX-4）による 1 バッチ
//!    全体再構築で計測する。**限界（PR #296 codex-review 指摘）**: `index_file_batch`
//!    （`src/incremental.rs`）はバッチ内の各ファイルについて `index_file`（柱 1・2 が
//!    対象とする単一ファイル経路）と同じ [`embed_and_write_phase`
//!    (`engine::incremental`)] を順に呼ぶ実装であり、独立した別実装の全体再構築経路
//!    ではない。そのため「単一ファイル挿入 1 回が既存コーパス規模に比例して重くなる」
//!    再処理型退行が起きた場合、`t_full` を構成する `BASELINE_FILES + 1` 回の呼び出し
//!    自体も同じ退行の影響を受けて逐次重くなり（0 件→ `BASELINE_FILES` 件まで増える
//!    コーパスに対する `embed_and_write_phase` 呼び出しの総和）、退行の有無に関わらず
//!    `t_full` は `t_inc`（`BASELINE_FILES` 件時点の 1 回分）のおよそ `BASELINE_FILES`
//!    倍程度の桁になる。結果として `within_ratio_threshold` は退行の有無を問わず
//!    ほぼ常に真になり、柱 3 単独ではこの再処理型退行を判別できない。柱 3 が実際に
//!    検出できるのは、`index_file` にのみ存在し `index_file_batch` には存在しない、
//!    単一ファイル経路固有の退行（例: `execute_insert_sql`／`index_file` 呼び出し
//!    経路だけに挿入された余分な処理）に限られる。
//! 4. **一括投入経路の再埋め込み量判定**
//!    （[`index1_full_rebuild_batch_embedding_quantity_is_independent_of_reprocessing`]。
//!    決定的・時間非依存。柱 3 の上記「限界」への対応。PR #296 codex-review 指摘:
//!    「一括投入側も同じファイル単位処理を反復するため比較の独立性がない」。
//!    再指摘（PR #296 Cursor Bugbot）: 応答フィールドの合算だけでは、outcome には
//!    正しくファイル単位分だけを計上しつつ内部で余分に再処理する退行を検出できず
//!    vacuous pass しうる）:
//!    柱 1 の [`CountingEmbedder`] によるカウント判定は単一ファイル経路
//!    （`index_file`）だけを対象にしており、`t_full` の計測に使う一括投入経路
//!    （`index_file_batch`）自身は独立に検証していなかった。柱 4 は柱 1 と同じ
//!    [`CountingEmbedder`] 計装を一括投入経路へ挿し、`execute_insert_sql_batch`
//!    呼び出し全体を通した `Embedder::embed_batch` の実際の呼び出し回数・入力
//!    テキスト総数を直接観測する（応答が返す `chunks_written`／`rows_replaced` の
//!    自己申告値を経由しない）。`BASELINE_FILES + 1` 件のバッチで、呼び出し回数が
//!    `BASELINE_FILES + 1` 回、入力テキスト総数が `(BASELINE_FILES + 1) *
//!    expected_chunks_for_new_file()`（＝各ファイル 1 件あたりちょうど期待チャンク数）
//!    に一致することを固定する。`index_file_batch` が既存コーパス規模に比例して
//!    重くなる**再埋め込みを伴う**再処理型退行を起こせば、バッチ後半の呼び出しほど
//!    余分な `embed_batch` 呼び出し・入力テキストが発生し、これらの直接観測値は
//!    期待値を確実に上回るため、応答フィールドの自己申告を経由せず一括投入経路
//!    自身の再処理型退行を検出できる（経路共通の退行の検出は柱 1・2・4 が担い、
//!    いずれも `t_full` に依存しない。モジュール冒頭の「検出対象」参照）。応答
//!    フィールド（`chunks_written`／`rows_replaced`）はこの直接観測値との整合性
//!    チェックとして従属的に突き合わせる（**限界（PR #296 codex-review P2
//!    再指摘）**: 柱 4 が計装しているのは `Embedder::embed_batch` の呼び出し回数・
//!    入力テキスト総数、つまり**再埋め込み量**である。埋め込み呼び出しは正常値の
//!    まま、既存の埋め込みベクトルを使い回してコーパス全体を再書き込みする、
//!    または置換書き込み（`tenant::replace_typed_rows_by_text_key`）自体を余分に
//!    反復する「ストレージ書き込みのみの再処理型」退行は、この直接観測にも
//!    従属的な応答フィールド突き合わせにも現れないため柱 4 では検出できない
//!    （`chunks_written`／`rows_replaced` も同じ実装が返す自己申告値であり、
//!    独立したストレージ計装ではない。モジュール冒頭「検出対象外」参照）。この
//!    クラスの退行を検出するには `Storage` 自体への書き込み回数計装という engine
//!    側の production コード変更が要り、本テストファイルのスコープ外の別 Issue と
//!    する）。
//!
//! vacuous pass 防止（Issue #281 AC2。柱 4 分は PR #296 Cursor Bugbot「no negative
//! control either」指摘への対応も兼ねる）として、柱 1〜3 と同一の判定述語
//! （[`within_ratio_threshold`]・[`within_scaling_slack`]・[`work_is_single_file`]）を、
//! 「既存コーパス全件の同一パス再投入 ＋ 新規 1 件」という再処理型退行の実測モデル
//! （[`measure_simulated_full_reprocess`]）に通し、判定が確実に `false`（拒否）を返す
//! ことに加え、柱 4 の直接観測値（呼び出し回数・入力テキスト総数）についても
//! 「バッチ呼び出しごとに累積ファイル分を毎回再埋め込みする」再処理型退行の実測
//! モデル（[`measure_simulated_batch_full_reprocess_counts`]）を通し、正常時の期待値を
//! 確実に上回ることを、いずれも
//! [`index1_judgements_reject_simulated_full_reprocess_regression`] で固定する。
//! **この負例が検証しているのは「[`measure_simulated_full_reprocess`] というモデルに
//! 対して各判定述語が `false` を返すこと」であり、柱 3
//! （[`within_ratio_threshold`]）が実際の実装退行を検出できることの証明ではない**
//! （上記「限界」参照。柱 3 単独の検出力の不足は柱 4 が補う）。
//! [`measure_simulated_full_reprocess`] は既存 `n_files` 件全件を**同一パスへ**
//! 再投入する（コーパス総行数は増えず一定のまま `n_files + 1` 回の呼び出しを行う）
//! ため、`measure_full_rebuild_batch` の「0 件から `BASELINE_FILES` 件まで増えていく
//! コーパスに対する `BASELINE_FILES + 1` 回の呼び出し（各呼び出しの重さが右肩上がり）」
//! とは負荷の形が異なり、単純な総和比較にはならない（柱 3 の負例は判定式ライブラリの
//! 動作確認であり、柱 3 単体の検出力を主張する根拠ではない。柱 3 の実際の検出対象は
//! 上記のとおり「経路固有」の退行に限られ、一括投入経路自身の再処理型退行の検出は
//! 柱 4 が担う）。

//!
//! **計測設計の頑健化（Issue #1164）**: 時間判定（柱 2・3 と各負例）は、比較する 2 系統を
//! ラウンドごとに交互（ABAB）に計測し、両系統へ同じ推定量（[`robust_estimate`]＝最小値）
//! を適用する。負例にも正例と同じウォームアップ・ラウンド数・推定量を適用する（従来は
//! 単発・逐次計測で、一過性の負荷が片側の小さな合計値に直撃して間欠失敗した）。判定閾値
//! （[`SCALING_SLACK`]・[`RATIO_THRESHOLD_DENOM`]）と判定述語は据え置きで、アサーションの
//! 弱体化ではない（O(N) 退行は最小値も押し上げる）。実測値はソースへ記載しない。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder, HashingEmbedder};
use engine::incremental::{IncrementalConfig, IndexTiming};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// 計測系テスト（時間・カウントいずれも既存コーパス規模に応じて重くなる処理を含む）
// が並列に走って互いのタイミングを乱さないようにする（フレーク対策。coding-rust.md
// のフレーク対策方針に沿い、判定自体は比率・回数ベースだが、計測対象は実プロセスの
// CPU 時間である以上、並走する他テストの負荷から隔離する）。
//
// Issue #281 codex-review P2 指摘: 従来この直列化機構（プロセス内 Mutex ＋クロス
// プロセスファイルロック）は本ファイル固有のコピーで、`tests/incremental_write_perf.rs`
// 等の他 integration-test バイナリはこの Mutex を取得しないため並走し得た。
// `crates/engine/src/test_util/timing_lock.rs` へ一本化し、計測系テストファイルが
// 同じロックファイルパスを共有することで、バイナリをまたいだ直列化を成立させる
// （同モジュールのドキュメンテーションコメント参照。本ファイルも他の計測系テスト
// ファイルと同じく `#[path]` で取り込む）。
#[path = "../src/test_util/timing_lock.rs"]
mod timing_lock;
use timing_lock::acquire_timing_lock;

const DIM: u32 = 128;

/// 生成チャンク数を小さく固定するための `IncrementalConfig`（1 チャンク = 2 行。
/// `tests/incremental_index.rs` の `small_chunk_config` と同じ流儀）。
fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 2,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        "documents",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// 埋め込み呼び出し回数・入力テキスト総数を数える `Embedder` ラッパー（柱 1: カウント
/// 判定用。TASK-281）。`HashingEmbedder` へ委譲しつつ、呼び出し側から見える副作用
/// （原子カウンタの増加）だけを追加する。`incremental.rs::embed_and_write_phase` は
/// ファイル 1 件につき `embed_batch` を 1 回、そのファイルの全チャンク本文をまとめて
/// 呼ぶ契約のため、単一ファイル挿入で `calls == 1` かつ `texts` が
/// [`expected_chunks_for_new_file`] の期待値と一致することを、実行時間に依存せず
/// 固定できる（再埋め込み型の O(N) 退行——本番では外部
/// 埋め込みサービス呼び出し回数の増加として現れる——を、決定的な `HashingEmbedder`
/// では時間に現れない場合でも検出できる唯一の手段）。
struct CountingEmbedder {
    inner: HashingEmbedder,
    calls: Arc<AtomicUsize>,
    texts: Arc<AtomicUsize>,
}

impl CountingEmbedder {
    /// `dim` 次元の `HashingEmbedder` をラップした `CountingEmbedder` と、呼び出し元が
    /// 観測に使う共有カウンタ（呼び出し回数・入力テキスト総数）を返す。
    fn new(dim: u32) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let texts = Arc::new(AtomicUsize::new(0));
        let embedder = Self {
            inner: HashingEmbedder::new(dim).expect("valid dim"),
            calls: Arc::clone(&calls),
            texts: Arc::clone(&texts),
        };
        (embedder, calls, texts)
    }
}

impl Embedder for CountingEmbedder {
    fn dim(&self) -> u32 {
        self.inner.dim()
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.fetch_add(texts.len(), Ordering::SeqCst);
        self.inner.embed_batch(texts)
    }
}

/// 新規 DB ファイルへ `documents` テーブルを作成し、`EngineCore` を構築する
/// （ベースライン構築・全体再構築側の各ラウンドで使う。テーブルが未作成の状態から
/// 開始する）。
fn new_core_with_documents_table(path: &Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

/// 既存 DB ファイル（`documents` テーブル作成済み・複製直後）を開いて `EngineCore` を
/// 構築する（増分側の各ラウンドで、ベースライン DB を複製したファイルに対して使う。
/// `Storage::open` の時間・テーブル作成の時間はいずれの側の計測対象にも含めない）。
fn open_core_on_existing_table(path: &Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage on copied db");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

/// [`open_core_on_existing_table`] の計数版（柱 1: カウント判定用）。埋め込みを
/// [`CountingEmbedder`] 経由にし、呼び出し元が読み取る共有カウンタを返す。
fn open_core_on_existing_table_with_counting(
    path: &Path,
) -> (EngineCore, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let storage = Storage::open(path).expect("open storage on copied db");
    let (embedder, calls, texts) = CountingEmbedder::new(DIM);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(embedder))
        .with_incremental_config(small_chunk_config());
    (core, calls, texts)
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

fn body_text_cells(result: &engine::sql::exec::QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected Cell::Text, got {other:?}"),
        })
        .collect()
}

/// 計測値の代表値として最小値を返す（`values` は非空であること、呼び出し側で保証する）。
/// 負荷ノイズ（CPU 競合・fsync 遅延）は加算的で、試行の過半が汚染されると中央値も崩れる。
/// 最小値は「本来のコスト」の推定量であり、O(N) の退行は最小値も押し上げるため検出力は
/// 落ちない（判定閾値は不変。正例・負例の双方に同じ推定量を適用する。Issue #1164）。
fn robust_estimate(values: Vec<Duration>) -> Duration {
    values.into_iter().min().unwrap_or(Duration::ZERO)
}

/// `IndexTiming` の各段階（チャンク化・埋め込み・書き込み）の合計を返す
/// （オーバーフローを未定義動作にしないよう checked 加算する。coding-rust.md 参照）。
fn timing_total(timing: IndexTiming) -> Duration {
    timing
        .chunking
        .checked_add(timing.embedding)
        .and_then(|d| d.checked_add(timing.write))
        .expect("timing sum must not overflow")
}

/// ベースラインコーパスの規模（既存ファイル数。全体再構築比判定〔柱 3〕で使う）。
/// ノイズに対するマージンと CI での実行時間のバランスを取ったテスト固有のパラメータ
/// （spec 所定の値ではない）。
const BASELINE_FILES: usize = 64;

/// スケーリング判定（柱 2）・カウント判定（柱 1）で使う小規模コーパスの既存ファイル数。
const CORPUS_FILES_SMALL: usize = 8;

/// スケーリング判定（柱 2）・カウント判定（柱 1）で使う大規模コーパスの既存ファイル数
/// （`CORPUS_FILES_SMALL` の 32 倍。軽量な置換キー線形走査〔モジュールドキュメント
/// 参照〕の傾きに対して十分な倍率で「再処理型」退行との差を判別する）。
const CORPUS_FILES_LARGE: usize = 256;

/// スケーリング判定（柱 2）の許容倍率。既存コーパス規模が `CORPUS_FILES_SMALL` から
/// `CORPUS_FILES_LARGE`（32 倍）へ増えても、単一ファイル挿入の所要時間はこの倍率を
/// 超えて増加しないことを固定する。軽量な置換キー線形走査（別 Issue の対象。
/// モジュールドキュメント参照）の傾き込みで debug/release 双方に余裕を持って通り、
/// 再処理型退行（規模比 ≒ 32 倍に比例した傾き）は確実に超過する中間値として、
/// 本テスト固有のパラメータで校正した（校正結果の実測値は private spec の数値基準と
/// 誤認されないようソースへは記さない。spec-confidentiality.md 準拠）。
const SCALING_SLACK: u32 = 8;

/// 1 ファイルあたりの行数（`lines_per_chunk=2` と合わせて 1 ファイル = 2 チャンク）。
const LINES_PER_FILE: usize = 4;

/// ノイズ対策として、増分・全体再構築それぞれを複数回計測し中央値を取る回数。
/// 3 → 5 回へ見直し（Issue #281。中央値の安定性を上げる）。
const MEASUREMENT_ROUNDS: usize = 5;

/// 判定閾値の分母（増分側は全体再構築側の `1 / RATIO_THRESHOLD_DENOM` 以下の時間で
/// 完了すること）。本テスト固有の回帰検知パラメータであり、`tests/incremental_write_perf.rs`
/// （TASK-143・PERSIST-2）と数値・判定式の形が一致するのは実装が同じ書き込み経路を
/// 共有するためである。
const RATIO_THRESHOLD_DENOM: u32 = 10;

fn generic_body(index: usize) -> String {
    (0..LINES_PER_FILE)
        .map(|line| format!("line {line} of file {index}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 「増分 1 ファイルの処理時間」対「全体再構築の総処理時間」の比較判定（柱 3）。
/// `tests/incremental_write_perf.rs`（PERSIST-2）と同じ形（`Duration` 同士の乗算で
/// 比較し、浮動小数の除算誤差を判定から排除する）。
fn within_ratio_threshold(t_inc: Duration, t_full: Duration) -> bool {
    t_inc.saturating_mul(RATIO_THRESHOLD_DENOM) <= t_full
}

/// 「小規模コーパス／大規模コーパスでの単一ファイル挿入時間」の比較判定（柱 2）。
fn within_scaling_slack(t_small: Duration, t_large: Duration) -> bool {
    t_large <= t_small.saturating_mul(SCALING_SLACK)
}

/// 新規ファイル 1 件分の期待チャンク数を、挿入結果（`chunks_written`）を経由せず
/// テスト側の入力パラメータ（[`LINES_PER_FILE`] と `small_chunk_config()` の
/// `lines_per_chunk`）から独立に算出する（柱 1: [`work_is_single_file`] の比較対象。
/// Issue #281 codex-review P2 指摘）。既存コーパス全体を 1 回の `embed_batch` 呼び出しに
/// 束ねて全チャンクを書き戻す退行では、`calls == 1` は成立しつつ `chunks_written` も
/// その 1 回の呼び出しが書いた全行数（＝送った全テキスト数）を報告してしまうため、
/// 挿入結果自身の `chunks_written` と `texts` を比較する判定では `calls == 1 &&
/// texts == chunks_written` が両方成立し得て検出をすり抜ける。ここでの期待値は
/// 挿入結果を一切参照しないテスト定数由来の値なので、この退行では
/// `texts`（全コーパス分）と一致しなくなる。
fn expected_chunks_for_new_file() -> usize {
    let lines_per_chunk = small_chunk_config().chunking.lines_per_chunk;
    LINES_PER_FILE.div_ceil(lines_per_chunk)
}

/// 「単一ファイル挿入 1 回分の埋め込み呼び出し」の判定（柱 1。時間非依存）。
/// `calls == 1`（`embed_batch` の呼び出し回数）かつ `texts == expected_chunks`
/// （入力テキスト総数が、[`expected_chunks_for_new_file`] が返す、挿入結果を経由しない
/// 独立算出の期待チャンク数と一致する）ことを要求する。
fn work_is_single_file(calls: usize, texts: usize, expected_chunks: usize) -> bool {
    calls == 1 && texts == expected_chunks
}

/// `label` で名前空間化した既存ファイル数 `n_files` のベースラインコーパスを構築する
/// （柱 1〜3 共通のベースライン構築ヘルパー。Issue #281 で各判定が独立した規模の
/// コーパスを必要とするため一般化した）。`label` は同一テスト内の複数ベースライン
/// （小規模／大規模等）が同じ論理パス（`corpus/{label}-NNNN.txt`）を使っても DB ファイル
/// 自体が別なので衝突しない。返り値のパスは `CleanupGuard` が生存する間だけ有効。
fn build_baseline(label: &str, n_files: usize) -> (PathBuf, CleanupGuard) {
    let baseline_path = unique_db_path(&format!("recall-baseline-{label}"));
    let guard = CleanupGuard(baseline_path.clone());
    let core = new_core_with_documents_table(&baseline_path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for i in 0..n_files {
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/{label}-{i:04}.txt"),
            &generic_body(i),
            &format!("op-{label}-{i:04}"),
        );
        core.execute_insert_sql(&write_ctx, &sql)
            .expect("baseline file insert should succeed");
    }
    (baseline_path, guard)
}

/// 既存ファイル数 `n_files` のコーパス（`baseline_path`）に対する増分挿入計測の
/// ウォームアップ（ファイルシステムキャッシュ等の初回コストの除外。柱 2・柱 3 共通）。
/// 複製先で行い、`baseline_path` 自体は変更しない。`round_label` は一時ファイル名・
/// `operation_id` の名前空間化にのみ使う。
fn warm_up_single_file_insert(baseline_path: &Path, n_files: usize, round_label: &str) {
    let warmup_path = unique_db_path(&format!("recall-{round_label}-warmup"));
    let _warmup_cleanup = CleanupGuard(warmup_path.clone());
    std::fs::copy(baseline_path, &warmup_path).expect("copy baseline for warmup");
    let core = open_core_on_existing_table(&warmup_path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = insert_file_sql(
        "documents",
        &format!("corpus/{round_label}-warmup-new.txt"),
        &generic_body(n_files),
        &format!("op-{round_label}-warmup-new"),
    );
    core.execute_insert_sql(&write_ctx, &sql)
        .expect("warmup incremental insert should succeed");
}

/// 既存ファイル数 `n_files` のコーパス（`baseline_path`）に対し、1 ファイルを増分
/// 挿入する所要時間を 1 ラウンド分計測する（柱 2・柱 3 共通）。
///
/// 素のベースライン DB（既存ファイル数固定）を新規ファイルへ複製してから計測することで、
/// 計測時点の既存コーパス規模を毎ラウンド揃える（前のラウンドの増分挿入で規模が変わらない
/// ようにする）。呼び出し側は比較対象側とラウンドごとに交互（ABAB）に呼び、負荷変動が
/// 片側だけに乗らないようにする（Issue #1164）。
fn measure_single_file_insert_round(
    baseline_path: &Path,
    n_files: usize,
    round: usize,
    round_label: &str,
) -> Duration {
    let round_path = unique_db_path(&format!("recall-{round_label}-round-{round}"));
    let _round_cleanup = CleanupGuard(round_path.clone());
    std::fs::copy(baseline_path, &round_path).expect("copy pristine baseline for round");
    let core = open_core_on_existing_table(&round_path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = insert_file_sql(
        "documents",
        &format!("corpus/{round_label}-incremental-round-{round}.txt"),
        &generic_body(n_files + round),
        &format!("op-{round_label}-incremental-round-{round}"),
    );
    let outcome = core
        .execute_insert_sql(&write_ctx, &sql)
        .expect("incremental file insert (measured) should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    timing_total(incremental.timing)
}

/// `BASELINE_FILES + 1` 件を空 DB から 1 バッチとして
/// `EngineCore::execute_insert_sql_batch`（TASK-122・INDEX-4 の一括投入 API）で構築する
/// 「全体再構築」の総所要時間を 1 ラウンド分計測する（柱 3。呼び出し側が増分挿入側と
/// 交互に呼ぶ。Issue #1164）。
///
/// Issue #281 見直し前は増分経路（`execute_insert_sql`）そのものを `BASELINE_FILES + 1`
/// 回呼ぶ逐次投入だったため、増分経路自体の退行が `t_inc`・`t_full` 双方に同時に乗り
/// 判定をすり抜け得た（モジュールドキュメント参照）。一括投入 API へ切り替えたが、
/// **`index_file_batch` はバッチ内の各ファイルに対し `index_file` と同じ
/// `embed_and_write_phase` を順に呼ぶ実装（`src/incremental.rs` 参照）であり、
/// 経路共通の再処理型退行（既存コーパス規模に比例して単一ファイル挿入が重くなる
/// 種類の退行）に対しては `t_full` 自身も同じ影響を受けるため判別できない
/// （PR #296 codex-review 指摘。モジュールドキュメントの柱 3「限界」節に詳細）**。
/// 本関数が実際に検出できるのは `index_file` にのみ存在し `index_file_batch` には
/// 存在しない、単一ファイル経路固有の退行に限られる（経路共通の退行の検出は
/// `t_full` に依存しない柱 1・2 が担う）。
fn measure_full_rebuild_batch_round(round: usize) -> Duration {
    {
        let full_path = unique_db_path(&format!("recall-full-rebuild-batch-{round}"));
        let _full_cleanup = CleanupGuard(full_path.clone());
        let storage = Storage::open(&full_path).expect("open storage");
        storage
            .create_table(&documents_schema())
            .expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
            .with_incremental_config(small_chunk_config())
            .with_batch_limits(BatchLimits {
                max_files_per_batch: BASELINE_FILES + 1,
                ..BatchLimits::default()
            });
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let sqls: Vec<String> = (0..=BASELINE_FILES)
            .map(|i| {
                insert_file_sql(
                    "documents",
                    &format!("corpus/full-batch-{round}-{i:04}.txt"),
                    &generic_body(i),
                    &format!("op-full-batch-{round}-{i:04}"),
                )
            })
            .collect();
        let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();
        let outcomes = core
            .execute_insert_sql_batch(&write_ctx, &sql_refs)
            .expect("full rebuild batch insert should succeed");

        let mut total = Duration::ZERO;
        for outcome in outcomes {
            let incremental = outcome
                .incremental
                .expect("file-form insert sets incremental");
            total = total
                .checked_add(timing_total(incremental.timing))
                .expect("full rebuild duration sum must not overflow");
        }
        total
    }
}

/// `index_file_batch`（`execute_insert_sql_batch` が呼ぶ一括投入経路本体）自身に対する、
/// 時間非依存・直接観測の処理量判定（柱 4: 直接観測判定用。PR #296 codex-review・
/// Cursor Bugbot の再指摘への対応）。
///
/// 当初案は `execute_insert_sql_batch` が返す
/// `IndexOutcome.chunks_written`／`rows_replaced`（wire 応答として公開される、
/// `index_file_batch` が「自己申告」する論理的な処理結果）だけを合算する判定であり、
/// `index_file_batch` が実際に `Embedder::embed_batch` を何回・何件のテキストで
/// 呼んだか（＝内部の再チャンク化・再埋め込み量そのもの）は独立に観測していなかった。
/// もし `index_file_batch` が「各ファイルの outcome には正しくそのファイル分だけを
/// 計上しつつ、内部では既存コーパス分も余分に re-chunk・re-embed・re-write する」
/// 退行を起こした場合、outcome の合算はこの退行の影響を受けず期待値のままとなり
/// 判定が vacuous pass しうる（PR #296 codex-review・Cursor Bugbot 指摘）。
///
/// 本関数は柱 1（[`CountingEmbedder`]）と同じ計装をバッチ経路（一括投入経路）に
/// 挿し、`execute_insert_sql_batch` 呼び出し全体を通した `embed_batch` 呼び出し
/// 回数・入力テキスト総数を、`IndexOutcome` の自己申告を経由せず直接数える。
/// `embed_and_write_phase` の契約（ファイル 1 件につき `embed_batch` を 1 回、
/// そのファイル自身のチャンクだけを渡す）が守られていれば、`n_files + 1` 件の
/// バッチで `calls == n_files + 1` かつ `texts == (n_files + 1) *
/// expected_chunks_for_new_file()` となるはずである。柱 1 と異なり、この直接観測は
/// バッチ内の各ファイルが既存コーパス規模に比例して余分な入力テキストで
/// 再埋め込みされる退行を、応答フィールドを介さずに検出する。
///
/// **限界（PR #296 codex-review P2 再指摘）**: 本関数が計装するのは
/// `Embedder::embed_batch` の呼び出し回数・入力テキスト総数、すなわち
/// **再埋め込み量**である。返り値の `total_chunks_written`／`total_rows_replaced`
/// も `IndexOutcome` が返す自己申告値の合算であり、`embed_batch` 呼び出しとは
/// 独立な `Storage` 書き込み回数の計装ではない。したがって、埋め込み呼び出しは
/// 正常値のまま既存の埋め込みベクトルを使い回して行を再書き込みする、または
/// 置換書き込み（`tenant::replace_typed_rows_by_text_key`）自体を余分に反復する
/// 「ストレージ書き込みのみの再処理型」退行は、`calls`／`texts`／
/// `total_chunks_written`／`total_rows_replaced` のいずれにも現れず検出できない
/// （モジュールドキュメント「検出対象外」参照。`Storage` への独立した書き込み
/// 計装は別 Issue の対象）。
fn full_rebuild_batch_embedding_quantity_with_counting(
    n_files: usize,
) -> (usize, usize, usize, usize) {
    let full_path = unique_db_path("recall-full-rebuild-batch-quantity-counting");
    let _full_cleanup = CleanupGuard(full_path.clone());
    let storage = Storage::open(&full_path).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    let (embedder, calls, texts) = CountingEmbedder::new(DIM);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(embedder))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_files_per_batch: n_files + 1,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sqls: Vec<String> = (0..=n_files)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("corpus/full-batch-quantity-counting-{i:04}.txt"),
                &generic_body(i),
                &format!("op-full-batch-quantity-counting-{i:04}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();
    let outcomes = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect("full rebuild batch insert should succeed");

    let mut total_chunks_written: usize = 0;
    let mut total_rows_replaced: usize = 0;
    for outcome in outcomes {
        let incremental = outcome
            .incremental
            .expect("file-form insert sets incremental");
        total_chunks_written = total_chunks_written
            .checked_add(incremental.chunks_written)
            .expect("total chunks_written must not overflow");
        total_rows_replaced = total_rows_replaced
            .checked_add(incremental.rows_replaced)
            .expect("total rows_replaced must not overflow");
    }

    (
        calls.load(Ordering::SeqCst),
        texts.load(Ordering::SeqCst),
        total_chunks_written,
        total_rows_replaced,
    )
}

/// [`full_rebuild_batch_embedding_quantity_with_counting`] の負例用実測モデル（柱 4 の
/// vacuous pass 防止・PR #296 Cursor Bugbot 指摘「no negative control either」への
/// 対応）。「バッチ呼び出し `i` 回目が、それまでに積んだ `0..=i` 件分のファイルを
/// 毎回まとめて再埋め込みする」再処理型退行を、`execute_insert_sql_batch` を
/// `0..=n_files` 回、呼び出しごとに引数のバッチへ「これまでの全ファイル」を含める
/// ことで実測モデル化する（モジュール冒頭「検出対象」参照。柱 1 の負例
/// [`measure_simulated_full_reprocess_counts`] の単一ファイル経路版に対する、
/// 一括投入経路版）。この呼び出し列全体を通した `embed_batch` 呼び出し回数・
/// 入力テキスト総数を返す。同一パスへの再投入は置換（`INSERT` の同一パス置換
/// 書き込みセマンティクス）になるため、コーパスの総行数は各呼び出し後も
/// `i + 1` 件のままである。
fn measure_simulated_batch_full_reprocess_counts(n_files: usize, label: &str) -> (usize, usize) {
    let full_path = unique_db_path(&format!("recall-{label}-batch-reprocess-counts"));
    let _full_cleanup = CleanupGuard(full_path.clone());
    let storage = Storage::open(&full_path).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    let (embedder, calls, texts) = CountingEmbedder::new(DIM);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(embedder))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_files_per_batch: n_files + 1,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for i in 0..=n_files {
        let sqls: Vec<String> = (0..=i)
            .map(|j| {
                insert_file_sql(
                    "documents",
                    &format!("corpus/{label}-{j:04}.txt"),
                    &generic_body(j),
                    &format!("op-{label}-batch-reprocess-{i:04}-{j:04}"),
                )
            })
            .collect();
        let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();
        core.execute_insert_sql_batch(&write_ctx, &sql_refs)
            .expect("simulated batch reprocess insert should succeed");
    }

    (calls.load(Ordering::SeqCst), texts.load(Ordering::SeqCst))
}

/// 負例（vacuous pass 防止・Issue #281 AC2）: 「既存 `n_files` 件全件の同一パス
/// 再投入 ＋ 新規 1 件」を、再処理型 O(N) 退行が実際に行う仕事量の実測モデルとして
/// 扱い、全区間（チャンク化・埋め込み・書き込み）の `IndexTiming` 合計を返す。
///
/// 単一ファイル経路（`execute_insert_sql`）を `n_files + 1` 回呼ぶ点は、Issue #281 前の
/// `measure_full_rebuild`（逐次投入）と同型だが、ここでは「既存コーパス規模に比例して
/// 増える 1 回の挿入」の代替モデルとして意図的に使う（[`within_ratio_threshold`]・
/// [`within_scaling_slack`] という柱 2・3 の判定述語へ正例と同じ形で通し、`false` が
/// 返ることを確認する）。`label` は `baseline_path` を構築した [`build_baseline`] と
/// 同じものを渡し、既存ファイルのパスが一致する（＝再投入が新規挿入ではなく置換に
/// なる）ようにする。
fn measure_simulated_full_reprocess(
    baseline_path: &Path,
    n_files: usize,
    label: &str,
    round: usize,
) -> Duration {
    let round_path = unique_db_path(&format!("recall-{label}-reprocess-{round}"));
    let _round_cleanup = CleanupGuard(round_path.clone());
    std::fs::copy(baseline_path, &round_path).expect("copy baseline for simulated reprocess");
    let core = open_core_on_existing_table(&round_path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let mut total = Duration::ZERO;
    for i in 0..n_files {
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/{label}-{i:04}.txt"),
            &generic_body(i),
            &format!("op-{label}-reprocess-{round}-{i:04}"),
        );
        let outcome = core
            .execute_insert_sql(&write_ctx, &sql)
            .expect("simulated reprocess re-insert should succeed");
        let incremental = outcome
            .incremental
            .expect("file-form insert sets incremental");
        total = total
            .checked_add(timing_total(incremental.timing))
            .expect("simulated reprocess duration sum must not overflow");
    }

    let new_file_sql = insert_file_sql(
        "documents",
        &format!("corpus/{label}-reprocess-new-file.txt"),
        &generic_body(n_files),
        &format!("op-{label}-reprocess-new-{round}"),
    );
    let outcome = core
        .execute_insert_sql(&write_ctx, &new_file_sql)
        .expect("simulated reprocess new-file insert should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    total = total
        .checked_add(timing_total(incremental.timing))
        .expect("simulated reprocess duration sum must not overflow");

    total
}

/// [`measure_simulated_full_reprocess`] の計数版（柱 1 の負例用）。「既存 `n_files` 件
/// 全件の同一パス再投入 ＋ 新規 1 件」全体を通した `embed_batch` 呼び出し回数・入力
/// テキスト総数と、最後に挿入した新規ファイルの `chunks_written` を返す。
/// [`work_is_single_file`] に通すと `calls != 1` により確実に `false` を返す
/// （柱 1 の検出力を固定する）。
fn measure_simulated_full_reprocess_counts(
    baseline_path: &Path,
    n_files: usize,
    label: &str,
) -> (usize, usize, usize) {
    let round_path = unique_db_path(&format!("recall-{label}-reprocess-counts"));
    let _round_cleanup = CleanupGuard(round_path.clone());
    std::fs::copy(baseline_path, &round_path)
        .expect("copy baseline for simulated reprocess counts");
    let (core, calls, texts) = open_core_on_existing_table_with_counting(&round_path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for i in 0..n_files {
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/{label}-{i:04}.txt"),
            &generic_body(i),
            &format!("op-{label}-reprocess-counts-{i:04}"),
        );
        core.execute_insert_sql(&write_ctx, &sql)
            .expect("simulated reprocess re-insert should succeed");
    }

    let new_file_sql = insert_file_sql(
        "documents",
        &format!("corpus/{label}-reprocess-counts-new-file.txt"),
        &generic_body(n_files),
        &format!("op-{label}-reprocess-counts-new"),
    );
    let outcome = core
        .execute_insert_sql(&write_ctx, &new_file_sql)
        .expect("simulated reprocess new-file insert should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");

    (
        calls.load(Ordering::SeqCst),
        texts.load(Ordering::SeqCst),
        incremental.chunks_written,
    )
}

// --- INDEX-1 関連: 増分性能の本テスト固有の回帰基準（既存コーパスへの 1 ファイル
// 追加 vs 全体再構築） ---------------------------------------------------------
//
// 対象時間はサーバー内部処理（チャンク化・埋め込み・行バッファ構築・redb 書き込み。
// `IndexTiming.write` のドキュメンテーションコメント参照）のみで、wire プロトコル
// 転送時間は含めない（`InsertOutcome.incremental.timing` を直接使うため SQL 解析・
// 束縛の時間もほぼ含まれない）。
//
// 判定は「増分 1 ファイルの処理時間」対「全体再構築（`BASELINE_FILES + 1` 件の
// 一括投入バッチ）の総処理時間」の比を、`tests/incremental_write_perf.rs`
// （PERSIST-2）と同じ形（`within_ratio_threshold`）で判定する（モジュールドキュメント・
// `RATIO_THRESHOLD_DENOM` のドキュメンテーションコメント参照）。柱 3 単独で経路共通
// 退行を検出する設計ではない（柱 1・2 が担う。モジュールドキュメント参照）。
#[test]
fn index1_incremental_indexing_completes_within_ratio_threshold_of_full_rebuild() {
    let _timing_guard = acquire_timing_lock();

    let (baseline_path, _baseline_cleanup) = build_baseline("incremental", BASELINE_FILES);
    warm_up_single_file_insert(&baseline_path, BASELINE_FILES, "incremental");
    let mut inc_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    let mut full_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        inc_durations.push(measure_single_file_insert_round(
            &baseline_path,
            BASELINE_FILES,
            round,
            "incremental",
        ));
        full_durations.push(measure_full_rebuild_batch_round(round));
    }
    let t_inc = robust_estimate(inc_durations);
    let t_full = robust_estimate(full_durations);

    let ratio = t_inc.as_secs_f64() / t_full.as_secs_f64().max(f64::EPSILON);

    // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
    println!(
        "index1 incremental indexing perf: t_inc={t_inc:?} t_full={t_full:?} \
         (baseline_files={BASELINE_FILES}) ratio={ratio:.4}"
    );

    assert!(
        within_ratio_threshold(t_inc, t_full),
        "incremental single-file insert ({t_inc:?}) must complete within \
         1/{RATIO_THRESHOLD_DENOM} of full rebuild ({t_full:?}) of baseline_files={BASELINE_FILES}; \
         ratio={ratio:.4}"
    );
}

// --- INDEX-1 柱 4: 一括投入経路（`index_file_batch`）自身の再埋め込み量判定
// （決定的・時間非依存。PR #296 codex-review 指摘: 柱 3 の `t_full` 計測に使う
// 一括投入経路自体が独立検証されていなかった。限界（PR #296 codex-review P2
// 再指摘）: 検出できるのは再埋め込みを伴う退行のみで、既存ベクトルの使い回しに
// よるストレージ書き込みのみの再処理型退行は検出対象外。モジュールドキュメントの
// 柱 4 節・「検出対象外」参照） ---------------------------------------------------

#[test]
fn index1_full_rebuild_batch_embedding_quantity_is_independent_of_reprocessing() {
    let _timing_guard = acquire_timing_lock();

    let (calls, texts, total_chunks_written, total_rows_replaced) =
        full_rebuild_batch_embedding_quantity_with_counting(BASELINE_FILES);

    let expected_chunks_per_file = expected_chunks_for_new_file();
    let expected_calls = BASELINE_FILES + 1;
    let expected_total = expected_calls * expected_chunks_per_file;

    println!(
        "index1 full-rebuild-batch write-quantity judgement: \
         calls={calls} texts={texts} total_chunks_written={total_chunks_written} \
         total_rows_replaced={total_rows_replaced} expected_calls={expected_calls} \
         expected_total={expected_total} (baseline_files={BASELINE_FILES})"
    );

    // 主判定（直接観測。PR #296 codex-review・Cursor Bugbot 再指摘への対応）:
    // `execute_insert_sql_batch` の応答フィールドを経由せず、`Embedder::embed_batch`
    // の実際の呼び出し回数・入力テキスト総数そのものを検査する。
    // `index_file_batch` が既存コーパス規模に比例して重くなる再処理型退行を起こせば、
    // 呼び出し回数がファイル数を上回るか、入力テキスト総数が期待値を上回るため、
    // 自己申告の `IndexOutcome` フィールドが（誤って）退行の影響を受けなかった
    // 場合でも本判定は確実に失敗する。
    assert_eq!(
        calls,
        expected_calls,
        "full-rebuild batch (execute_insert_sql_batch, {} files) must call \
         Embedder::embed_batch exactly once per file regardless of accumulated corpus size \
         within the batch (embed_and_write_phase contract); a reprocessing-type O(N) \
         regression in index_file_batch that re-embeds already-processed files would \
         inflate this call count",
        BASELINE_FILES + 1
    );
    assert_eq!(
        texts,
        expected_total,
        "full-rebuild batch (execute_insert_sql_batch, {} files) must feed \
         Embedder::embed_batch exactly {expected_chunks_per_file} chunks of text per file \
         regardless of accumulated corpus size within the batch; a reprocessing-type O(N) \
         regression in index_file_batch would inflate this total independently of what \
         IndexOutcome self-reports",
        BASELINE_FILES + 1
    );

    // 従属判定（冗長な整合性チェック。PR #296 codex-review P2 再指摘への対応で
    // 限界を明記）: 応答フィールド（`chunks_written`／`rows_replaced`）が
    // 直接観測値（`texts`）と食い違わないこと自体を固定する。検出力はあくまで
    // 上記 `calls`／`texts` の直接観測判定が担う。`total_chunks_written`／
    // `total_rows_replaced` はいずれも `execute_insert_sql_batch` 自身が返す
    // 自己申告値の合算であり、`Storage` への書き込み回数を独立に計装した値では
    // ない。したがって、埋め込み呼び出しは正常値のまま既存ベクトルを使い回して
    // 行を再書き込みする、または置換書き込みを余分に反復する「ストレージ書き込み
    // のみの再処理型」退行が起きても、この自己申告値は退行の影響を受けずに
    // 期待値のまま一致し得るため、以下 2 つの assert はそのクラスの退行に対して
    // 検出力を持たない（full_rebuild_batch_embedding_quantity_with_counting doc・
    // モジュールドキュメント「検出対象外」参照）。
    assert_eq!(
        total_chunks_written, texts,
        "IndexOutcome.chunks_written summed across the batch must match the directly \
         observed embed_batch input text count (texts={texts}); a mismatch would mean \
         chunks_written under- or over-reports the actual embedding work"
    );
    assert_eq!(
        total_rows_replaced, total_chunks_written,
        "rows actually written via tenant::replace_typed_rows_by_text_key must match the \
         chunk count produced for each file (redundant consistency check; see \
         full_rebuild_batch_embedding_quantity_with_counting doc for why this alone does not \
         add detection power beyond the calls/texts direct-observation judgement)"
    );
}

// --- INDEX-1 柱 1: カウント判定（決定的・時間非依存・最優先） -------------------

#[test]
fn index1_single_file_insert_work_is_independent_of_corpus_size() {
    let _timing_guard = acquire_timing_lock();

    for &n_files in &[CORPUS_FILES_SMALL, CORPUS_FILES_LARGE] {
        let label = format!("count-{n_files}");
        let (baseline_path, _baseline_cleanup) = build_baseline(&label, n_files);

        let round_path = unique_db_path(&format!("recall-{label}-work"));
        let _round_cleanup = CleanupGuard(round_path.clone());
        std::fs::copy(&baseline_path, &round_path).expect("copy baseline for count judgement");
        let (core, calls, texts) = open_core_on_existing_table_with_counting(&round_path);
        let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");

        let sql = insert_file_sql(
            "documents",
            &format!("corpus/{label}-work-new.txt"),
            &generic_body(n_files),
            &format!("op-{label}-work-new"),
        );
        let outcome = core
            .execute_insert_sql(&write_ctx, &sql)
            .expect("single-file insert should succeed");
        let incremental = outcome
            .incremental
            .expect("file-form insert sets incremental");
        let observed_calls = calls.load(Ordering::SeqCst);
        let observed_texts = texts.load(Ordering::SeqCst);

        let expected_chunks = expected_chunks_for_new_file();
        println!(
            "index1 count judgement: n_files={n_files} calls={observed_calls} \
             texts={observed_texts} expected_chunks={expected_chunks} chunks_written={}",
            incremental.chunks_written
        );

        assert!(
            work_is_single_file(observed_calls, observed_texts, expected_chunks),
            "single-file insert work must not scale with corpus size (n_files={n_files}): \
             calls={observed_calls} texts={observed_texts} expected_chunks={expected_chunks} \
             chunks_written={}",
            incremental.chunks_written
        );

        // 埋め込み呼び出し回数・テキスト数だけでなく、実際に書き込まれた行数
        // （既存コーパス 2 * n_files 行 + 新規ファイル 2 行）でも独立性を裏付ける。
        let count_result = core
            .execute_sql(&read_ctx, "SELECT COUNT(*) FROM documents")
            .expect("count query should succeed");
        let total_rows = match count_result.rows.first().and_then(|r| r.cells.first()) {
            Some(Cell::Integer(v)) => *v,
            other => panic!("expected Cell::Integer, got {other:?}"),
        };
        let expected_rows = 2 * (n_files as u64 + 1);
        assert_eq!(
            total_rows, expected_rows,
            "documents row count after single-file insert must equal 2 * (n_files + 1) \
             for n_files={n_files}"
        );
    }
}

// --- INDEX-1 柱 2: スケーリング判定（比率ベース） -------------------------------

#[test]
fn index1_single_file_insert_time_does_not_scale_with_corpus_size() {
    let _timing_guard = acquire_timing_lock();

    let (small_baseline, _small_cleanup) = build_baseline("scale-small", CORPUS_FILES_SMALL);
    let (large_baseline, _large_cleanup) = build_baseline("scale-large", CORPUS_FILES_LARGE);

    warm_up_single_file_insert(&small_baseline, CORPUS_FILES_SMALL, "scale-small");
    warm_up_single_file_insert(&large_baseline, CORPUS_FILES_LARGE, "scale-large");
    let mut small_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    let mut large_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        small_durations.push(measure_single_file_insert_round(
            &small_baseline,
            CORPUS_FILES_SMALL,
            round,
            "scale-small",
        ));
        large_durations.push(measure_single_file_insert_round(
            &large_baseline,
            CORPUS_FILES_LARGE,
            round,
            "scale-large",
        ));
    }
    let t_small = robust_estimate(small_durations);
    let t_large = robust_estimate(large_durations);

    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(f64::EPSILON);
    println!(
        "index1 scaling judgement: t_small={t_small:?} (n_files={CORPUS_FILES_SMALL}) \
         t_large={t_large:?} (n_files={CORPUS_FILES_LARGE}) ratio={ratio:.4} slack={SCALING_SLACK}"
    );

    assert!(
        within_scaling_slack(t_small, t_large),
        "single-file insert time must not scale with corpus size beyond the {SCALING_SLACK}x \
         slack: t_small={t_small:?} (n_files={CORPUS_FILES_SMALL}) t_large={t_large:?} \
         (n_files={CORPUS_FILES_LARGE}) ratio={ratio:.4}"
    );
}

// --- vacuous pass 防止（Issue #281 AC2）: 正例と同じ判定述語を負例（再処理型退行の
// 実測モデル）に通し、確実に拒否（`false`）されることを固定する -------------------

#[test]
fn index1_judgements_reject_simulated_full_reprocess_regression() {
    let _timing_guard = acquire_timing_lock();

    // 柱 2（スケーリング判定）の負例: 小規模／大規模コーパスそれぞれで「再処理型」の
    // 実測モデルを計測すると、規模比（32 倍）に比例した傾きとなり
    // `within_scaling_slack` が `false` を返すこと。
    let (small_baseline, _small_cleanup) = build_baseline("reject-scale-small", CORPUS_FILES_SMALL);
    let (large_baseline, _large_cleanup) = build_baseline("reject-scale-large", CORPUS_FILES_LARGE);
    // 正例（柱 2）と同じ計測設計（ウォームアップ・ラウンドごとの交互計測・最小値推定）を
    // 適用する。単発・逐次計測では、分母（小規模側）の小さな合計値に一過性の負荷が
    // 直撃して比が閾値を割る間欠失敗があった（Issue #1164）。ウォームアップ用に
    // 使い捨て 1 ラウンド（round = MEASUREMENT_ROUNDS）を計測前に回す。
    let _ = measure_simulated_full_reprocess(
        &small_baseline,
        CORPUS_FILES_SMALL,
        "reject-scale-small",
        MEASUREMENT_ROUNDS,
    );
    let _ = measure_simulated_full_reprocess(
        &large_baseline,
        CORPUS_FILES_LARGE,
        "reject-scale-large",
        MEASUREMENT_ROUNDS,
    );
    let mut regressed_small = Vec::with_capacity(MEASUREMENT_ROUNDS);
    let mut regressed_large = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        regressed_small.push(measure_simulated_full_reprocess(
            &small_baseline,
            CORPUS_FILES_SMALL,
            "reject-scale-small",
            round,
        ));
        regressed_large.push(measure_simulated_full_reprocess(
            &large_baseline,
            CORPUS_FILES_LARGE,
            "reject-scale-large",
            round,
        ));
    }
    let t_regressed_small = robust_estimate(regressed_small);
    let t_regressed_large = robust_estimate(regressed_large);
    let scale_ratio =
        t_regressed_large.as_secs_f64() / t_regressed_small.as_secs_f64().max(f64::EPSILON);
    println!(
        "index1 negative control (scaling): t_regressed_small={t_regressed_small:?} \
         (n_files={CORPUS_FILES_SMALL}) t_regressed_large={t_regressed_large:?} \
         (n_files={CORPUS_FILES_LARGE}) ratio={scale_ratio:.4} slack={SCALING_SLACK}"
    );
    assert!(
        !within_scaling_slack(t_regressed_small, t_regressed_large),
        "simulated full-reprocess regression must be rejected by the scaling judgement \
         (i.e. within_scaling_slack must return false): t_regressed_small={t_regressed_small:?} \
         t_regressed_large={t_regressed_large:?} ratio={scale_ratio:.4}"
    );

    // 柱 3（全体再構築比判定）の負例: `BASELINE_FILES` 規模での「再処理型」の実測
    // モデル 1 回分は、`t_full`（`BASELINE_FILES + 1` 件の一括再構築）と同程度の
    // 重さであり、`within_ratio_threshold` が `false` を返すこと。
    let (ratio_baseline, _ratio_cleanup) = build_baseline("reject-ratio", BASELINE_FILES);
    let _ = measure_simulated_full_reprocess(
        &ratio_baseline,
        BASELINE_FILES,
        "reject-ratio",
        MEASUREMENT_ROUNDS,
    );
    let mut regressed_ratio = Vec::with_capacity(MEASUREMENT_ROUNDS);
    let mut full_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
    for round in 0..MEASUREMENT_ROUNDS {
        regressed_ratio.push(measure_simulated_full_reprocess(
            &ratio_baseline,
            BASELINE_FILES,
            "reject-ratio",
            round,
        ));
        full_durations.push(measure_full_rebuild_batch_round(round));
    }
    let t_regressed_ratio = robust_estimate(regressed_ratio);
    let t_full = robust_estimate(full_durations);
    let full_ratio = t_regressed_ratio.as_secs_f64() / t_full.as_secs_f64().max(f64::EPSILON);
    println!(
        "index1 negative control (ratio): t_regressed_ratio={t_regressed_ratio:?} \
         (baseline_files={BASELINE_FILES}) t_full={t_full:?} ratio={full_ratio:.4}"
    );
    assert!(
        !within_ratio_threshold(t_regressed_ratio, t_full),
        "simulated full-reprocess regression must be rejected by the full-rebuild-ratio \
         judgement (i.e. within_ratio_threshold must return false): \
         t_regressed_ratio={t_regressed_ratio:?} t_full={t_full:?} ratio={full_ratio:.4}"
    );

    // 柱 1（カウント判定）の負例: 「再処理型」の実測モデルは既存ファイル数分だけ
    // `embed_batch` を呼ぶため `calls != 1` となり、`work_is_single_file` が `false`
    // を返すこと（時間に依存しない確実な検出）。
    let (count_baseline, _count_cleanup) = build_baseline("reject-count", CORPUS_FILES_SMALL);
    let (calls, texts, chunks_written) = measure_simulated_full_reprocess_counts(
        &count_baseline,
        CORPUS_FILES_SMALL,
        "reject-count",
    );
    let expected_chunks = expected_chunks_for_new_file();
    println!(
        "index1 negative control (count): calls={calls} texts={texts} \
         expected_chunks={expected_chunks} chunks_written={chunks_written} \
         (n_files={CORPUS_FILES_SMALL})"
    );
    assert!(
        !work_is_single_file(calls, texts, expected_chunks),
        "simulated full-reprocess regression must be rejected by the count judgement \
         (i.e. work_is_single_file must return false): calls={calls} texts={texts} \
         expected_chunks={expected_chunks} chunks_written={chunks_written}"
    );

    // 柱 4（一括投入経路の直接観測判定）の負例（PR #296 Cursor Bugbot 指摘
    // 「no negative control either」への対応）: バッチ呼び出しごとに、それまでに
    // 積んだ全ファイルを毎回まとめて再埋め込みする再処理型退行の実測モデルでは、
    // `embed_batch` 呼び出し回数・入力テキスト総数が、退行なしの一括投入
    // （ファイル数 + 1 回・ファイルあたり expected_chunks 件）の期待値を確実に
    // 上回ること。
    let (batch_calls, batch_texts) =
        measure_simulated_batch_full_reprocess_counts(CORPUS_FILES_SMALL, "reject-batch-count");
    let expected_batch_calls = CORPUS_FILES_SMALL + 1;
    let expected_batch_texts = expected_batch_calls * expected_chunks;
    println!(
        "index1 negative control (batch count): batch_calls={batch_calls}          expected_batch_calls={expected_batch_calls} batch_texts={batch_texts}          expected_batch_texts={expected_batch_texts} (n_files={CORPUS_FILES_SMALL})"
    );
    assert!(
        batch_calls > expected_batch_calls && batch_texts > expected_batch_texts,
        "simulated batch full-reprocess regression model must produce more embed_batch          calls/input texts than a non-regressed batch would (batch_calls={batch_calls}          > expected_batch_calls={expected_batch_calls}, batch_texts={batch_texts}          > expected_batch_texts={expected_batch_texts}); this confirms the direct-observation          judgement used by index1_full_rebuild_batch_embedding_quantity_is_independent_of_reprocessing          has detection power against a reprocessing-type regression in the batch path"
    );
}

// --- INDEX-2: 結果整合性（既存コーパスが積まれた状態での増分追加の検索反映） -------

#[test]
fn index2_added_file_is_reflected_in_next_search_over_existing_corpus() {
    let path = unique_db_path("recall-index2-corpus");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    const CORPUS_FILES: usize = 10;
    let marker_for = |i: usize| format!("existingmarker{i:02}");
    let body_for = |i: usize| {
        format!(
            "corpus filler line one\n\
             corpus filler line two\n\
             unique {} token line\n\
             unique {} token line continued",
            marker_for(i),
            marker_for(i)
        )
    };

    // 既存コーパスを事前投入する。
    for i in 0..CORPUS_FILES {
        let sql = insert_file_sql(
            "documents",
            &format!("corpus/existing-{i:02}.txt"),
            &body_for(i),
            &format!("op-existing-{i:02}"),
        );
        core.execute_insert_sql(&write_ctx, &sql)
            .expect("existing corpus file insert should succeed");
    }

    // 新規ファイルを増分挿入する（固有トークン zzzqq を含む）。
    let new_body = "alpha alpha filler line one\n\
                     alpha alpha filler line two\n\
                     charlie charlie unique marker zzzqq\n\
                     charlie charlie unique marker zzzqq continued";
    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "corpus/new-file.txt", new_body, "op-new-file"),
        )
        .expect("incremental file insert should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    assert_eq!(incremental.chunks_written, 2);
    assert_eq!(incremental.rows_replaced, 2);

    let embedder = HashingEmbedder::new(DIM).expect("valid dim");

    // 1. 新規ファイルの内容が、既存コーパスが積まれた状態でも密検索・ハイブリッド検索の
    //    上位（LIMIT を絞った結果集合）に反映されること。`HashingEmbedder` は決定的だが
    //    意味的ではない（`embedding.rs` モジュールドキュメント参照）ため、
    //    厳密な Top-1 一致ではなく上位集合中に含まれることを確認する
    //    （`tests/incremental_index.rs` と同じ流儀）。
    let query_text = "unique marker zzzqq";
    let query_vec = embedder
        .embed_batch(&[query_text])
        .expect("query embedding should succeed")
        .remove(0);
    let query_literal = vector_literal(&query_vec);

    let distance_sql =
        format!("SELECT body FROM documents ORDER BY embedding <=> {query_literal} LIMIT 3");
    let distance_result = core
        .execute_sql(&read_ctx, &distance_sql)
        .expect("distance search should succeed");
    let distance_bodies = body_text_cells(&distance_result);
    assert!(
        distance_bodies.iter().any(|b| b.contains("zzzqq")),
        "distance search top-3 should contain the new file's marker, got: {distance_bodies:?}"
    );

    let hybrid_sql = format!(
        "SELECT body FROM documents ORDER BY HYBRID(embedding, {query_literal}, body, '{query_text}') LIMIT 3"
    );
    let hybrid_result = core
        .execute_sql(&read_ctx, &hybrid_sql)
        .expect("hybrid search should succeed");
    let hybrid_bodies = body_text_cells(&hybrid_result);
    assert!(
        hybrid_bodies.iter().any(|b| b.contains("zzzqq")),
        "hybrid search top-3 should contain the new file's marker, got: {hybrid_bodies:?}"
    );

    // 2. 存在性チェック（ランキングに依存しない、より強い保証）: 新規ファイルのパスで
    //    直接引けること。
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let new_file_rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'corpus/new-file.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select by new path should succeed");
    assert_eq!(new_file_rows.rows.len(), 2);
    assert!(
        body_text_cells(&new_file_rows)
            .iter()
            .any(|b| b.contains("zzzqq")),
        "new file's chunks must be retrievable by path and contain the marker"
    );

    // 3. 増分追加後も既存ファイルが引き続き検索可能であること（増分反映が既存索引を
    //    破壊しないことを固定する）。既存コーパスの先頭・末尾ファイルを代表として確認する。
    for &i in &[0usize, CORPUS_FILES - 1] {
        let existing_path = format!("corpus/existing-{i:02}.txt");
        let existing_rows = core
            .execute_sql(
                &read_ctx,
                &format!(
                    "SELECT body FROM documents WHERE path = '{existing_path}' ORDER BY embedding <=> {zero_vec} LIMIT 100"
                ),
            )
            .expect("select by existing path should succeed");
        assert_eq!(existing_rows.rows.len(), 2);
        let marker = marker_for(i);
        assert!(
            body_text_cells(&existing_rows)
                .iter()
                .any(|b| b.contains(&marker)),
            "existing file {existing_path} content must survive the incremental add"
        );
    }
}
