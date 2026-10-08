//! バッチ検索の実 GPU バックエンド（TASK-128〜130・対象ビヘイビア: CORE-6, 8,
//! 16。ポインタ: Issue #178）。
//!
//! `batch_fallback.rs::BatchBackend` の公開差し替え点へ差し込む実装で、
//! `wgpu`（=30.0.1・依存追加はオーナー承認済み〔2026-08-26〕。`crates/engine/Cargo.toml`
//! コメント参照）を通じて Vulkan/Metal/DX12 の compute パイプラインを扱う。
//! `batch_search.rs::ResidentMatrix`（f16 2 要素/u32 パック常駐行列）が保持する
//! `packed()` バッファを GPU の STORAGE バッファへアップロードし、行 × クエリの
//! 内積計算だけを GPU 側で行う。
//!
//! # 責務境界（`batch_fallback.rs`・`batch_search.rs` との分担）
//!
//! - テナント境界・可視性判定は本モジュールも
//!   `policy.rs::PolicyContext::is_visible` の単一照合パスを使う（CORE-2 と
//!   同じ判定関数。独自のテナント文字列比較は行わない）
//! - 本バックエンドが返す結果は [`crate::batch_fallback::BatchBackend`] の
//!   契約（doc 参照）を満たすことを目指すが、`FallbackBatchEngine::
//!   revalidate_primary_hits` が独立に再検証するため、本モジュールが唯一の
//!   防御線ではない（codex-review P0 指摘対応の設計を踏襲）
//! - 計算量 DoS 対策は `batch_search.rs::compute_tenant_work`（`rows × queries
//!   × dim` の checked 演算）を CPU 経路と共有し、`batch_search` 冒頭で
//!   dispatch 前に `queries.len()` を乗じた総量を [`MAX_BATCH_WORK`] と照合
//!   する。GPU 経路はテナント別に走査を分けないため「常駐行列の全行数」を
//!   単一テナント分の行数として扱い、`run_batch_search` のテナント別合算と
//!   同じかより厳しい側に倒れる保守的な上界にする（クエリごとの
//!   `gather_reachable_rows` 内の単発チェックはこの総量ガードの後段に
//!   残す防御的な二重チェックであり、唯一の防御線ではない）
//!
//! # panic を作らない設計
//!
//! `unwrap`/`expect`/添字アクセスは使わない。バッファサイズは `checked_*` で
//! 導出し、wgpu のエラー・デバイスロスト・ポーリング失敗はすべて
//! [`crate::batch_fallback::BatchBackendError`] へ写像して `Result` で返す
//! （coding-rust.md「ライブラリコードでは panic させない」）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::batch_fallback::{BatchBackend, BatchBackendError, BatchExecError};
use crate::batch_search::{
    compute_tenant_work, resolve_batch_slot, try_owned_str, try_reserve_exact,
    validate_batch_queries, BatchHit, BatchQuery, BatchRowSource, BatchSearchError, MAX_BATCH_WORK,
};
use crate::kernel::SearchHit;
use crate::kernel::{CandidateHit, TopKSelector};
use crate::policy::PolicyContext;

/// i8 パック常駐・`dot4I8Packed` シェーダによる opt-in 候補生成バックエンド
/// （Issue #542・親 #541・Phase 5 親 #460）。本番 primary
/// （[`GpuBatchBackend`]・`FallbackBatchEngine::build_with_gpu`）へは接続せず、
/// `GpuI8BatchBackend::try_new` からのみ明示構築する（[`GpuF32ContrastBackend`]
/// と同型の opt-in 専用経路。CORE-12「経路を外部から上書きする機構を作らない」
/// と整合）。詳細設計は `docs/design/gpu-batch-i8-packed.md` 参照。
pub mod packed_i8;

/// 1 回の GPU dispatch で読み戻すスコアバッファの予算（バイト）。adapter の
/// `max_storage_buffer_binding_size` に依らず、compute の 1 次元 dispatch が
/// `max_compute_workgroups_per_dimension`（実測値: 65535。§0 実測記録
/// ポインタ）に収まるよう、ワークグループサイズ 256 との積が確実に収まる
/// 小さめの固定値を選ぶ（32 MiB ÷ 4 byte ÷ 256 = 32768 workgroups
/// ＜ 65535）。この値を超える行数は [`GpuBatchBackend::batch_search`] が
/// 複数回の dispatch に分割して処理する（クエリを跨いだ結果の合算は行わず、
/// クエリごとに独立して縮小するため、分割自体はスコア計算の正しさに影響しない）。
const GPU_SCORE_BUFFER_BUDGET_BYTES: usize = 32 * 1024 * 1024;
const GPU_WORKGROUP_SIZE: u32 = 256;

/// 1 dispatch にタイル化できる最大クエリ本数（Issue #532: 1 dispatch = 1
/// クエリだった旧構造を、1 dispatch = 複数クエリへ変更する核となる定数）。
/// [`DOT_SHADER_WGSL`]/[`DOT_SHADER_F32_WGSL`] のレジスタ配列
/// `acc: array<f32, QUERY_TILE_MAX>` のサイズと一致していなければならない
/// （`tests::dot_shader_wgsl_query_tile_max_matches_host_constant` で機械検証）。
/// 値はレジスタ圧・タイル幅のトレードオフに基づく実装既定値であり、spec 由来の
/// 数値ではない。
const GPU_QUERY_TILE_MAX: usize = 16;

/// [`select_dot_shader`] の保守的なオーバーフロー margin 計算にのみ残る
/// 旧定数（PR #591 レビュー P1 指摘対応・2 巡目で用途が変わった）。
///
/// 元は f16 算術版シェーダが `vec2<f16>` レジスタ `acc2` を f32
/// アキュムレータへフラッシュするまでの積算回数で、WGSL 側にも同名の
/// 定数が宣言されていた。しかし `1` 固定にしても「単一の積そのものが
/// f16 の分解能へ丸められる」桁落ちは防げないと判明したため
/// （[`DOT_SHADER_F16_ARITH_WGSL`] doc 参照）、本 PR の 2 巡目修正で
/// シェーダ本体は積和を f32 へ拡張後に行う設計へ変更し、`acc2`／
/// フラッシュ機構・WGSL 側の同名定数は撤去した。この Rust 側定数だけは
/// [`F16_ARITH_PARTIAL_SUM_LIMIT`] とのオーバーフロー margin 計算式
/// （既存テストが数値ごと固定）を変えないため値 `1` のまま残しており、
/// 実質的には乗算しても値を変えない定数（`* 1`）に縮退している。
const GPU_F16_ACC_BLOCK: u32 = 1;

/// f16 の有限最大値（IEEE 754 half-precision）。[`select_dot_shader`] が
/// クエリ成分の f16 パック時飽和（±Inf 化）を防ぐ独立ガードとして使う。
const F16_MAX_FINITE: f32 = 65504.0;

/// [`select_dot_shader`] が f16 算術版を選ぶための保守的なオーバーフロー
/// 上界判定に使う閾値。シェーダ本体の積和は f32 で行う（doc 参照）ため
/// この上界を超えても実際にオーバーフローするわけではないが、既存の
/// 安全マージンとして維持する。`row_max_abs * query_max_abs *
/// GPU_F16_ACC_BLOCK` がこの値を超える場合は f16 算術版を選ばない
/// （[`DOT_SHADER_WGSL`] の unpack 版へ縮退）。65504 の約半分を選び、
/// 余裕を持たせる。spec 由来の数値ではなく実装既定値。
const F16_ARITH_PARTIAL_SUM_LIMIT: f32 = 32768.0;

/// f16（IEEE 754 half-precision）の最小正 subnormal（2^-24）。この値未満の
/// 絶対値を持つ非ゼロ有限成分は [`crate::batch_search::pack_f16x2`] で
/// 厳密にゼロへ丸められ情報が失われる（PR #591 レビュー P1 指摘対応）。
/// 既存のオーバーフローガード（上限のみ）はこの種のアンダーフローを
/// 検知できず、有効な小さい値が f16 変換で消えて正解行がスコア差から
/// 脱落しうるため、[`select_dot_shader`] は該当時に unpack 版へ縮退する。
const F16_MIN_POSITIVE_SUBNORMAL: f32 = 5.960_464_5e-8;

/// workgroup 内部分 Top-k シェーダ（[`DOT_SHADER_TOPK_WGSL`]/
/// [`DOT_SHADER_TOPK_F32_WGSL`]）が 1 ワークグループから出力する候補数の
/// 上限（Issue #536・ポインタ: `docs/design/gpu-batch-topk.md` 決定 1・3）。
/// ワークグループサイズ [`GPU_WORKGROUP_SIZE`]（256）と同値で、共有メモリ
/// 上の bitonic ソート網が扱える要素数（`sort_key`/`sort_slot` の配列長）と
/// 一致する。`k_out` はこの値でクランプされ（ホスト・シェーダ二重防御）、
/// これを超える `k` を持つクエリを含むタイルは
/// [`select_readback_mode`] が全量 readback へ縮退させる。
const GPU_TOPK_OUT_MAX: u32 = 256;

/// GPU の submit 完了・readback・error scope 完了を待つ上限時間
/// （codex/Bugbot 指摘対応: `PollType::wait_indefinitely()` と終了条件のない
/// ループは、Metal 等でコマンド完了通知が停止した場合に永久に戻らず、
/// 「GPU 実行時エラーは `BatchBackendError` を返して CPU 縮退（CORE-8）へ倒す」
/// という設計契約を破る）。期限超過は `DeviceLost` として返し、
/// `FallbackBatchEngine` が CPU-SIMD 経路へ縮退できるようにする。値は
/// spec 由来の閾値ではなく、正常な dispatch が十分収まる範囲で「実質ハング」を
/// 打ち切るための防御的上限。
const GPU_POLL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// 1 回の `device.poll` でブロックしてよい最大時間。`GPU_POLL_DEADLINE` に
/// 達するまで複数回に分けて待つことで、deadline 判定の粒度を確保する。
const GPU_POLL_SLICE: std::time::Duration = std::time::Duration::from_millis(100);
/// adapter の `max_compute_workgroups_per_dimension` に対する安全側の固定上限
/// （実機実測値は 65535）。取得できた実際の limits がこれを下回る場合はその値を使う。
const MAX_WORKGROUPS_PER_DIMENSION_FALLBACK: u32 = 65535;

/// WGSL: 常駐行列の 1 行（f16 2 要素/u32 パック。`batch_search.rs::pack_f16x2`
/// と同一表現）と、1 dispatch にタイル化した最大 [`GPU_QUERY_TILE_MAX`] 本の
/// クエリベクトルの内積を計算する（Issue #532: 1 dispatch = 1 クエリだった
/// 旧構造を、1 dispatch = 複数クエリへ変更。CORE-6・8・16 ポインタ）。
/// `unpack2x16float` は WGSL コア機能（`shader-f16` 拡張は不要）で、
/// `batch_search.rs::unpack_f16x2` と同じビット解釈をとる。
///
/// 各スレッド（行 1 つを担当）は `packed_rows` の当該行を 1 回だけ読み、
/// レジスタ配列 `acc`（要素数 `QUERY_TILE_MAX`。共有メモリは使わない設計上の
/// 簡略化。§9 申し送り）へタイル内の全クエリ分を同時に積算する。これにより
/// 常駐行列の HBM トラフィックはクエリ本数に比例せず、行 1 回読みをタイル幅
/// 分のクエリで償却する（親 Issue #531 の目的である「行列トラフィックの
/// Q 倍削減」の核）。
///
/// `params.row_stride` は「1 行あたりの `packed_rows` 要素数」（= `dim.div_ceil(2)`）、
/// `params.query_stride` は「1 クエリあたりの `query` 配列要素数」
/// （= `row_stride * 2`。f32 換算でパディング込み）を表す。`params.query_count`
/// は本 dispatch が実際に処理するクエリ本数（`<= QUERY_TILE_MAX`）で、
/// ホスト側（[`dispatch_dot_products`]）が保証し、シェーダ側でも `min` で
/// クランプする（fail-closed。ホスト・シェーダ二重の範囲外アクセス防止）。
/// [`DOT_SHADER_F32_WGSL`]（Issue #234・CORE-16 対照経路）と bind group
/// layout（バインディング構成・各エントリの型）を共用するため `Params` の形は
/// 揃えてあるが、`row_stride`/`query_stride` の意味はシェーダごとに異なる
/// （本シェーダでは「u32 パック要素数」、f32 版では「f32 要素数 = dim」）。
const DOT_SHADER_WGSL: &str = r#"
const QUERY_TILE_MAX: u32 = 16u;

struct Params {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> packed_rows: array<u32>;
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: array<f32>;
@group(0) @binding(4) var<storage, read_write> scores: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.row_count) {
        return;
    }
    let query_count = min(params.query_count, QUERY_TILE_MAX);
    let row = row_ids[i];
    let row_base = row * params.row_stride;

    var acc: array<f32, QUERY_TILE_MAX>;
    var qi: u32 = 0u;
    loop {
        if (qi >= QUERY_TILE_MAX) {
            break;
        }
        acc[qi] = 0.0;
        qi = qi + 1u;
    }

    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        let packed = packed_rows[row_base + j];
        let unpacked = unpack2x16float(packed);
        var q: u32 = 0u;
        loop {
            if (q >= query_count) {
                break;
            }
            let qbase = q * params.query_stride + j * 2u;
            acc[q] = acc[q] + unpacked.x * query[qbase] + unpacked.y * query[qbase + 1u];
            q = q + 1u;
        }
        j = j + 1u;
    }

    var qo: u32 = 0u;
    loop {
        if (qo >= query_count) {
            break;
        }
        scores[qo * params.row_count + i] = acc[qo];
        qo = qo + 1u;
    }
}
"#;

/// WGSL: CORE-16（GPU 常駐コピーの f16 パック vs f32 常駐の A/B 対照経路。
/// Issue #234・ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-16）用の
/// **対照（bench/テスト専用）** シェーダ。[`DOT_SHADER_WGSL`] と異なり行データを
/// `array<f32>` としてそのまま読み、`unpack2x16float` を経由しない f32 精度の
/// 内積を計算する。バインディング構成（型・数）は [`DOT_SHADER_WGSL`] と同一の
/// ため bind group layout を共用できる（WGSL の要素型 `array<u32>` vs
/// `array<f32>` は wgpu のバインドグループレイアウト検証に現れない）。
/// `params.row_stride`/`params.query_stride` はここでは「1 行・1 クエリあたりの
/// f32 要素数」= `dim`（パディング無し）を表す。ディスパッチ構造・クエリタイル化
/// （Issue #532）は [`DOT_SHADER_WGSL`] と同一の設計で、CORE-16 の A/B が
/// 「f16 vs f32 常駐」の差のみを見るよう、両シェーダのタイル構造を意図的に
/// 揃えている（行データの読み方だけが異なる）。
const DOT_SHADER_F32_WGSL: &str = r#"
const QUERY_TILE_MAX: u32 = 16u;

struct Params {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> rows: array<f32>;
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: array<f32>;
@group(0) @binding(4) var<storage, read_write> scores: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.row_count) {
        return;
    }
    let query_count = min(params.query_count, QUERY_TILE_MAX);
    let row = row_ids[i];
    let row_base = row * params.row_stride;

    var acc: array<f32, QUERY_TILE_MAX>;
    var qi: u32 = 0u;
    loop {
        if (qi >= QUERY_TILE_MAX) {
            break;
        }
        acc[qi] = 0.0;
        qi = qi + 1u;
    }

    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        let v = rows[row_base + j];
        var q: u32 = 0u;
        loop {
            if (q >= query_count) {
                break;
            }
            acc[q] = acc[q] + v * query[q * params.query_stride + j];
            q = q + 1u;
        }
        j = j + 1u;
    }

    var qo: u32 = 0u;
    loop {
        if (qo >= query_count) {
            break;
        }
        scores[qo * params.row_count + i] = acc[qo];
        qo = qo + 1u;
    }
}
"#;

/// WGSL: `SHADER_F16` 対応アダプタ向けの f16 算術版（Issue #539・親 #538。
/// 対象ビヘイビア: CORE-6, 8, 16 ポインタ）。[`DOT_SHADER_WGSL`] と行データ
/// （常駐 f16 2 要素/u32 パック）は完全に同一で、`enable f16;`（WGSL 拡張。
/// `Features::SHADER_F16` 要求時のみ有効）を使い `vec2<f16>` のまま
/// ストレージバッファから読む（帯域幅の削減がこのシェーダの目的）。
/// クエリ側もホスト（[`encode_query_bytes`]）が `batch_search.rs::pack_f16x2`
/// と同じ表現で `vec2<f16>` パックしてアップロードする
/// （[`QueryEncoding::F16Packed`]）。
///
/// **積和そのものは f32 で行う**（PR #591 レビュー P1 指摘対応・2 巡目）。
/// 旧実装は読み出した `vec2<f16>` のまま `fma` を実行しネイティブ半精度の
/// 積和を使っていたが、[`GPU_F16_ACC_BLOCK`] を `1` にして複数項の f16
/// 加算を無くしても、**単一の積そのものが f16 の分解能（仮数部 10 bit）へ
/// 丸められる**問題は残っていた（反例: `query=[128.125,128]`・
/// `row_a=[128.125,-128.25]`・`row_b=[0,2^-14]` は全成分が f16 で厳密表現
/// でき既存のオーバーフロー・アンダーフローガードもすべて通過するが、
/// `128.125 * 128.125 = 16416.015625` がその区間の分解能〔16〕で
/// `16416` へ丸められ、`row_a` の真のスコア差〔`0.015625`〕がまるごと
/// 消えて `row_b` に順位が入れ替わる）。この丸めは「f16 領域での複数項の
/// 加算」ではなく「1 回の乗算の結果を f16 として保持すること」自体が原因
/// のため、フラッシュ頻度（`GPU_F16_ACC_BLOCK`）をどう調整しても解消しない。
/// 本シェーダは `vec2<f32>(row_pair)`/`vec2<f32>(qv)` で読み出し直後に
/// f32 へ拡張してから乗算・加算する（[`DOT_SHADER_WGSL`] の
/// `unpack2x16float` 版と同じ演算順）ことでこの丸めを構造的に無くす。
/// f16→f32 の拡張は常に厳密（情報の追加消失は無い）なため、クエリの
/// 各成分が f16 へ厳密往復可能な場合（[`select_dot_shader`] の
/// 精度ガード）は [`DOT_SHADER_WGSL`] とビット同一になる。
///
/// クエリ成分自体が f16 パック時に丸められる別リスク（PR #591 レビュー
/// P1 指摘・2 巡目のもう一方）は本シェーダの外側、[`select_dot_shader`]
/// の精度ガードで防ぐ（クエリが f16 へ厳密往復できない場合は
/// [`DOT_SHADER_WGSL`] へ縮退しクエリを f32 のまま送る）。
///
/// [`DOT_SHADER_TOPK_F16_ARITH_WGSL`]（`topk_dot_shader!` 経由）の S0 と
/// 演算順を完全に一致させてあり、全量 readback／部分 Top-k いずれの経路でも
/// 同一のスコアになる（`tests/gpu_batch.rs` のビット同一検証対象）。
const DOT_SHADER_F16_ARITH_WGSL: &str = r#"
enable f16;

const QUERY_TILE_MAX: u32 = 16u;

struct Params {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> packed_rows: array<vec2<f16>>;
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: array<vec2<f16>>;
@group(0) @binding(4) var<storage, read_write> scores: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.row_count) {
        return;
    }
    let query_count = min(params.query_count, QUERY_TILE_MAX);
    let row = row_ids[i];
    let row_base = row * params.row_stride;
    let query_pairs = params.query_stride >> 1u;

    var acc: array<f32, QUERY_TILE_MAX>;
    var qi: u32 = 0u;
    loop {
        if (qi >= QUERY_TILE_MAX) {
            break;
        }
        acc[qi] = 0.0;
        qi = qi + 1u;
    }

    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        // 読み出し直後に f32 へ拡張してから乗算する（f16 のまま積を
        // 計算し保持する丸めを構造的に排除する。doc 参照）。
        let row_pair = vec2<f32>(packed_rows[row_base + j]);
        var q: u32 = 0u;
        loop {
            if (q >= query_count) {
                break;
            }
            let qv = vec2<f32>(query[q * query_pairs + j]);
            acc[q] = acc[q] + row_pair.x * qv.x + row_pair.y * qv.y;
            q = q + 1u;
        }
        j = j + 1u;
    }

    var qo: u32 = 0u;
    loop {
        if (qo >= query_count) {
            break;
        }
        scores[qo * params.row_count + i] = acc[qo];
        qo = qo + 1u;
    }
}
"#;

/// workgroup 内部分 Top-k シェーダ（Issue #536）を `macro_rules!` で組み立てる。
/// 行データの読み方（S0 内積）だけが常駐形式（f16 パック常駐 /
/// f32 常駐対照）ごとに異なり、パラメータ構造・共通バインディング・
/// Top-k 選出（S1・出力）はマクロ本体に 1 度だけ書かれた同一リテラルを
/// 両方の呼び出しが共有する（`docs/design/gpu-batch-topk.md` 決定 1・3）。
/// `concat!` はリテラルトークンしか受け付けないため（`const` 経由の断片は
/// 渡せない）、可変部分だけを `:literal` マクロ引数として渡す構成にしている。
///
/// # 実装スコープの申し送り（決定 1 からの意図的な縮小）
///
/// ADR 決定 1 は「候補 B」（`SUBGROUP` 有効時に subgroup shuffle 段を使い、
/// 無効時は共有メモリ＋バリアのみで同じ比較網を実行する）を採用としたが、
/// 本実装は**共有メモリ＋バリアのみ（候補 A 相当）に統一**している。理由:
/// naga 30.0.1 はバリアの一様性も subgroup builtin の一様性も検証しない
/// （ADR §1.3 実測）ため、`use_subgroup` の分岐先で `li` の取り違え等が
/// 起きてもコンパイル時・CI では検知できず、実機デバッグでしか発覚しない
/// リスクがある。本 Issue の主目的（readback 量をクエリ本数×行数比例から
/// 「ワークグループ数 × k_out」比例へ削減する）は共有メモリのみの構成でも
/// 達成できるため、正しさの検証可能性を優先しこちらを採用した。
/// subgroup shuffle 段の追加最適化は別途検討する（README/ADR・PR 本文へ
/// 申し送り）。
macro_rules! topk_dot_shader {
    ($prelude:literal, $row_binding:literal, $query_binding:literal, $row_read_loop:literal) => {
        concat!(
            $prelude,
            r#"
const WORKGROUP_SIZE: u32 = 256u;
const TOPK_OUT_MAX: u32 = 256u;
const QUERY_TILE_MAX: u32 = 16u;

struct TopKParams {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
    k_out: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
};

@group(0) @binding(0) var<uniform> params: TopKParams;
"#,
            $row_binding,
            r#"
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: "#,
            $query_binding,
            r#";
@group(0) @binding(4) var<storage, read_write> out_topk: array<u32>;

var<workgroup> sort_key: array<u32, 256>;
var<workgroup> sort_slot: array<u32, 256>;

// `f32::total_cmp`（`kernel.rs::MinHeapItem::cmp` の降順基準）と同順に
// 単調な u32 キーへ写像する（ADR 決定 1）。符号ビットが立っていれば
// 全ビット反転、立っていなければ符号ビットのみ立てる変換で、比較は常に
// 符号なし整数比較で行う（`score_from_key`（ホスト側）が逆写像）。
fn topk_score_key(score: f32) -> u32 {
    let bits = bitcast<u32>(score);
    if ((bits & 0x80000000u) != 0u) {
        return ~bits;
    }
    return bits | 0x80000000u;
}

@compute @workgroup_size(256)
fn main(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
    @builtin(workgroup_id) wg_id: vec3<u32>,
    @builtin(num_workgroups) num_wg: vec3<u32>,
) {
    let i = gid.x;
    let valid = i < params.row_count;
    let query_count = min(params.query_count, QUERY_TILE_MAX);
    let k_out = min(params.k_out, WORKGROUP_SIZE);
    // `i >= params.row_count` でも `row_ids[i]`/`packed_rows[...]` の読み出しは
    // WebGPU の robust buffer access 契約により未定義動作にはならない
    // （範囲外は実装依存の値を返すのみ）。この経路の結果は `valid` が false の
    // ため S1 で番兵へ強制されるので、読み出し値自体は使われない。
    let row = row_ids[i];
    let row_base = row * params.row_stride;

    var acc: array<f32, QUERY_TILE_MAX>;
    var qi: u32 = 0u;
    loop {
        if (qi >= QUERY_TILE_MAX) {
            break;
        }
        acc[qi] = 0.0;
        qi = qi + 1u;
    }
"#,
            $row_read_loop,
            r#"
    var qidx: u32 = 0u;
    loop {
        if (qidx >= query_count) {
            break;
        }

        let score = acc[qidx];
        let score_bits = bitcast<u32>(score);
        // `isNan`/`isInf` は fast-math で畳まれうるため使わず、指数ビットの
        // パターンで非有限（Inf/NaN）を判定する（ADR §2.1）。
        let is_finite = (score_bits & 0x7F800000u) != 0x7F800000u;

        var key: u32 = 0u;
        var slot: u32 = 0xFFFFFFFFu;
        if (valid && is_finite) {
            key = topk_score_key(score);
            slot = row;
        }

        var size: u32 = 2u;
        loop {
            if (size > WORKGROUP_SIZE) {
                break;
            }
            var stride: u32 = size >> 1u;
            loop {
                if (stride == 0u) {
                    break;
                }

                sort_key[li] = key;
                sort_slot[li] = slot;
                workgroupBarrier();

                let partner_li = li ^ stride;
                let partner_key = sort_key[partner_li];
                let partner_slot = sort_slot[partner_li];
                workgroupBarrier();

                let dir_up = (li & size) == 0u;
                let is_low = (li & stride) == 0u;
                let want_better = is_low == dir_up;
                let self_better =
                    (key > partner_key) || (key == partner_key && slot < partner_slot);

                if (want_better) {
                    if (!self_better) {
                        key = partner_key;
                        slot = partner_slot;
                    }
                } else {
                    if (self_better) {
                        key = partner_key;
                        slot = partner_slot;
                    }
                }

                stride = stride >> 1u;
            }
            size = size << 1u;
        }

        if (li < k_out) {
            let out_base = ((qidx * num_wg.x + wg_id.x) * k_out + li) * 2u;
            out_topk[out_base] = key;
            out_topk[out_base + 1u] = slot;
        }

        qidx = qidx + 1u;
    }
}
"#,
        )
    };
}

/// workgroup 内部分 Top-k シェーダ（f16 パック常駐・本番経路）。
/// [`GpuBatchBackend`] が [`select_readback_mode`] で `PartialTopK` を
/// 選んだ場合に使う（Issue #536）。S0 は [`DOT_SHADER_WGSL`] の演算順と
/// 完全に同一（スコアのビット同一契約の根拠）。
const DOT_SHADER_TOPK_WGSL: &str = topk_dot_shader!(
    "",
    "\n@group(0) @binding(1) var<storage, read> packed_rows: array<u32>;\n",
    "array<f32>",
    r#"
    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        let packed = packed_rows[row_base + j];
        let unpacked = unpack2x16float(packed);
        var q: u32 = 0u;
        loop {
            if (q >= query_count) {
                break;
            }
            let qbase = q * params.query_stride + j * 2u;
            acc[q] = acc[q] + unpacked.x * query[qbase] + unpacked.y * query[qbase + 1u];
            q = q + 1u;
        }
        j = j + 1u;
    }
"#
);

/// workgroup 内部分 Top-k シェーダ（f32 常駐対照・CORE-16 公平性のため
/// [`GpuF32ContrastBackend`] にも用意する。ADR §2.1「決定事項」）。S0 は
/// [`DOT_SHADER_F32_WGSL`] の演算順と完全に同一。
const DOT_SHADER_TOPK_F32_WGSL: &str = topk_dot_shader!(
    "",
    "\n@group(0) @binding(1) var<storage, read> rows: array<f32>;\n",
    "array<f32>",
    r#"
    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        let v = rows[row_base + j];
        var q: u32 = 0u;
        loop {
            if (q >= query_count) {
                break;
            }
            acc[q] = acc[q] + v * query[q * params.query_stride + j];
            q = q + 1u;
        }
        j = j + 1u;
    }
"#
);

/// workgroup 内部分 Top-k シェーダ（f16 算術版・Issue #539。
/// [`GpuContext::f16_arith_pipelines`] が保持し、`SHADER_F16` 対応アダプタで
/// [`select_dot_shader`] が [`GpuDotShaderKind::F16Arith`] を選んだ場合に使う）。
/// S0 は [`DOT_SHADER_F16_ARITH_WGSL`] の演算順（f16 で保持した行・クエリを
/// 読み出し直後に f32 へ拡張してから乗算・加算する。PR #591 レビュー P1
/// 指摘対応・2 巡目で f16 のまま積を計算する設計から変更。[`DOT_SHADER_F16_ARITH_WGSL`]
/// doc 参照）と完全に同一（全量 readback／部分 Top-k のビット同一契約の根拠）。
const DOT_SHADER_TOPK_F16_ARITH_WGSL: &str = topk_dot_shader!(
    "enable f16;\n",
    "\n@group(0) @binding(1) var<storage, read> packed_rows: array<vec2<f16>>;\n",
    "array<vec2<f16>>",
    r#"
    let query_pairs = params.query_stride >> 1u;
    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        // 読み出し直後に f32 へ拡張してから乗算する
        // （[`DOT_SHADER_F16_ARITH_WGSL`] doc 参照。f16 のまま積を保持する
        // 丸めを構造的に排除する）。
        let row_pair = vec2<f32>(packed_rows[row_base + j]);
        var q: u32 = 0u;
        loop {
            if (q >= query_count) {
                break;
            }
            let qv = vec2<f32>(query[q * query_pairs + j]);
            acc[q] = acc[q] + row_pair.x * qv.x + row_pair.y * qv.y;
            q = q + 1u;
        }
        j = j + 1u;
    }
"#
);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GpuParams {
    row_stride: u32,
    row_count: u32,
    /// 本 dispatch が処理するクエリ本数（`<= GPU_QUERY_TILE_MAX`。Issue #532）。
    /// 旧 `_pad0` を転用（bind group layout・`GpuParams` のバイト長は不変）。
    query_count: u32,
    /// 1 クエリあたりの `query` 配列要素数（f16 経路: `row_stride * 2`、
    /// f32 対照経路: `row_stride` と同値の `dim`）。旧 `_pad1` を転用。
    query_stride: u32,
}

impl GpuParams {
    /// `bytemuck` は使わず（依存最小方針・.claude/rules/dependency-policy.md）、
    /// `to_ne_bytes` の連結だけでバイト列化する。フィールド順は WGSL の
    /// `Params` と一致させ、host/device 双方をネイティブエンディアンに揃える
    /// （native GPU バックエンドはホストと同一エンディアンで動作する前提）。
    fn to_ne_bytes_vec(self) -> Result<Vec<u8>, BatchBackendError> {
        let mut out = Vec::new();
        try_reserve_bytes(&mut out, 16)?;
        out.extend_from_slice(&self.row_stride.to_ne_bytes());
        out.extend_from_slice(&self.row_count.to_ne_bytes());
        out.extend_from_slice(&self.query_count.to_ne_bytes());
        out.extend_from_slice(&self.query_stride.to_ne_bytes());
        Ok(out)
    }
}

/// [`DOT_SHADER_TOPK_WGSL`]/[`DOT_SHADER_TOPK_F32_WGSL`] の `TopKParams`
/// （32 バイト）と一致するホスト側パラメータ（Issue #536）。`GpuParams` の
/// 4 フィールドに `k_out` を加え、32 バイト境界へパディングする。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GpuTopKParams {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
    /// 1 ワークグループが出力する候補数（`<= GPU_TOPK_OUT_MAX`。シェーダ側も
    /// `min` でクランプする二重防御）。
    k_out: u32,
}

impl GpuTopKParams {
    /// WGSL `TopKParams` とフィールド順を一致させ、32 バイトへパディングする
    /// （`dot_topk_shader_params_size_matches_host_constant` で機械検証）。
    fn to_ne_bytes_vec(self) -> Result<Vec<u8>, BatchBackendError> {
        let mut out = Vec::new();
        try_reserve_bytes(&mut out, 32)?;
        out.extend_from_slice(&self.row_stride.to_ne_bytes());
        out.extend_from_slice(&self.row_count.to_ne_bytes());
        out.extend_from_slice(&self.query_count.to_ne_bytes());
        out.extend_from_slice(&self.query_stride.to_ne_bytes());
        out.extend_from_slice(&self.k_out.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        Ok(out)
    }
}

/// `topk_dot_shader!` マクロが生成する WGSL 内 `topk_score_key` のホスト側
/// 等価物。`f32::total_cmp`（`kernel.rs::MinHeapItem::cmp` の降順基準）と
/// 同順に単調な u32 キーへ写像する（ADR 決定 1）。[`score_from_key`] の逆
/// 写像との往復・順序保存の単体テストでのみ使う（GPU からの readback
/// デコードは常に逆方向の `score_from_key` のみを要する）ため、production
/// 経路からは呼ばれない。
#[cfg_attr(not(test), allow(dead_code))]
fn score_key(score: f32) -> u32 {
    let bits = score.to_bits();
    if (bits & 0x8000_0000) != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    }
}

/// [`score_key`] の逆写像（往復でビット同一）。`key` の最上位ビットが
/// 立っていれば元の符号ビットは 0（`score_key` が OR で強制した側）だった
/// と分かるため下位 31 ビットをそのまま復元し、立っていなければ元は符号1
/// 側だったとして全ビット反転で復元する。
fn score_from_key(key: u32) -> f32 {
    let bits = if (key & 0x8000_0000) != 0 {
        key & 0x7FFF_FFFF
    } else {
        !key
    };
    f32::from_bits(bits)
}

/// [`GpuBatchBackend::batch_search`]/[`GpuF32ContrastBackend::batch_search`]
/// が 1 dispatch の readback 方式を決める fail-closed な純関数（GPU デバイス
/// 非依存。ADR 決定 2）。Top-k パイプラインが利用不能、またはタイル内の
/// クエリが要求する `k` の最大値が [`GPU_TOPK_OUT_MAX`] を超える場合は
/// 常に既存の全量 readback へ縮退し、部分結果を返さない。
///
/// 実装スコープの申し送り（[`DOT_SHADER_TOPK_WGSL`] doc 参照）: 本実装は
/// 共有メモリのみの Top-k シェーダに統一しているため、ADR 決定 2 が挙げる
/// `SUBGROUP` 可用性・subgroup サイズ範囲の判定は行わない（Top-k パイプライン
/// 自体の生成可否のみで判定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuReadbackMode {
    FullScores,
    PartialTopK { k_out: usize },
}

fn select_readback_mode(topk_pipeline_available: bool, tile_max_k: usize) -> GpuReadbackMode {
    if !topk_pipeline_available || tile_max_k == 0 || tile_max_k > GPU_TOPK_OUT_MAX as usize {
        return GpuReadbackMode::FullScores;
    }
    GpuReadbackMode::PartialTopK { k_out: tile_max_k }
}

/// dispatch する S0（内積）シェーダの種別（Issue #539）。`Unpack` は既存の
/// `unpack2x16float` 経由 f32 積和（[`DOT_SHADER_WGSL`]/[`DOT_SHADER_TOPK_WGSL`]）、
/// `F16Arith` は `SHADER_F16` 対応アダプタでのみ選ばれる f16 パック常駐版
/// （[`DOT_SHADER_F16_ARITH_WGSL`]/[`DOT_SHADER_TOPK_F16_ARITH_WGSL`]。
/// 行・クエリとも読み出した `vec2<f16>` を直後に `vec2<f32>` へ拡張して
/// から積和するため算術自体は `Unpack` と同じく f32 で行う。常駐・転送
/// 表現のみが f16 パックの点が異なる。クエリが f16 へ厳密往復できない
/// 場合は [`select_dot_shader`] が `Unpack` へ fail-closed に縮退する）。
/// `bench-internals` feature 限定の [`GpuSearchTestOptions::dot_shader`]
/// フィールドで公開する必要があるため `pub` にしているが、既定ビルド・
/// `wire-server` からは（`GpuSearchTestOptions` 自体が feature gate 済みの
/// ため）到達不能で、テナント境界・RLS 迂回の経路は増やさない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuDotShaderKind {
    Unpack,
    F16Arith,
}

/// クエリバッファのホスト側エンコーディング（Issue #539）。`F16Arith` シェーダ
/// を選んだときのみ `F16Packed`（[`batch_search::pack_f16x2`] と同一表現）を
/// 使い、それ以外は既存の `F32`（`bytes_of_f32_slice`）のまま。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryEncoding {
    F32,
    F16Packed,
}

/// 常駐行列（f16 2 要素/u32 パック。[`crate::batch_search::pack_f16x2`]）の
/// 有限成分のみの絶対値最大を走査する（Issue #539・[`select_dot_shader`] の
/// `row_max_abs` 引数を作る）。非有限成分（f16 パック時の飽和で ±Inf 化した
/// 値）は unpack 版でも必ず非有限スコアとして除外される値のため、
/// オーバーフローガードの母数から除外してよい（`select_dot_shader` doc
/// 参照）。行列が空、または全成分が非有限の場合は `0.0`（安全側 = ガードが
/// 通りやすい方向ではなく、`query_max_abs` 側の独立チェックで overflow は
/// 別途防がれる）を返す。
///
/// 本関数は常駐行列（既に `pack_f16x2` で f16 量子化済みのバイト列）を
/// 走査するため、「非ゼロの元の値が f16 変換でゼロへ丸められたか」を
/// ここで検知することはできない（丸め後の値しか観測できず、丸め前が
/// 真にゼロだった行との区別がつかない）。この量子化は `Unpack`/`F16Arith`
/// いずれのシェーダを選んでも常駐行列の読み出し元（`self.row_buffer`）が
/// 共通のため両経路で等しく発生する既存の制約であり、[`select_dot_shader`]
/// の shader 選択が新たに追加するリスクではない（PR #591 レビュー P1
/// 指摘対応の検討過程で確認。アンダーフロー検知が意味を持つのは、
/// `F16Arith` 選択時にのみ追加で f16 量子化されるクエリ側
/// [`max_abs_finite_from_queries_subset`] のみ）。
fn max_abs_finite_from_packed(packed: &[u32]) -> f32 {
    let mut max_abs: f32 = 0.0;
    for &word in packed {
        let (a, b) = crate::batch_search::unpack_f16x2(word);
        if a.is_finite() {
            max_abs = max_abs.max(a.abs());
        }
        if b.is_finite() {
            max_abs = max_abs.max(b.abs());
        }
    }
    max_abs
}

/// [`max_abs_finite_from_queries_subset`] の走査結果（Issue #539・#591 P1 レビュー
/// 指摘対応）。`max_abs` は既存のオーバーフローガードの母数、
/// `has_subnormal_underflow` は「非ゼロ有限成分のうち
/// [`F16_MIN_POSITIVE_SUBNORMAL`] 未満で `pack_f16x2` により厳密にゼロへ
/// 丸められる値が存在するか」を表す。後者が真の場合、オーバーフローが
/// 起きなくても有効な小さいクエリ成分が f16 変換で消え去り、正解行が
/// スコア差から脱落しうるため、[`select_dot_shader`] は f16 算術版を
/// 選ばない（クエリは `Unpack` 選択時は f32 のまま送るため、この量子化は
/// `F16Arith` を選んだ場合にのみ新たに生じる。[`max_abs_finite_from_packed`]
/// doc 参照）。
///
/// `has_precision_loss`（PR #591 レビュー P1 指摘対応・2 巡目）は
/// [`f16_round_trip_exact`] doc 参照。`has_subnormal_underflow` が示す
/// 「ゼロへ丸められる」ケースはこの一般化された丸め検知の特殊例だが、
/// 既存テスト・呼び出し元との互換のため独立フィールドとして残す
/// （両方が真になりうる。`select_dot_shader` はどちらか一方でも真なら
/// 拒否する）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct QueryAmplitudeStats {
    max_abs: f32,
    has_subnormal_underflow: bool,
    has_precision_loss: bool,
}

/// クエリ成分 1 個が f16 へパック・アンパックしても完全に元の値へ戻るか
/// を判定する（PR #591 レビュー P1 指摘対応・2 巡目）。
///
/// [`select_dot_shader`] の既存ガード（振幅上限・非正規化アンダーフロー）
/// はいずれも「値の大きさ」だけを見ており、その範囲内でも f16 の仮数部
/// 10 bit では表現できない値が丸められる一般のケースを検知できない
/// （反例: `query=[2048.5,2048]`・`row_a=[1,-1]`・`row_b=[0,2^-13]` は
/// いずれのガードも通過するが、`2048.5` が最近接偶数丸めで `2048` へ
/// 変わり `row_a` の真のスコア〔`0.5`〕が `0` になって `row_b`
/// 〔`0.25`〕に順位が入れ替わる。この丸めはクエリを f16 へエンコードする
/// 時点で発生するため、シェーダ側の算術精度をどう変えても救えない）。
/// 本関数は実際のエンコード（[`encode_query_bytes`]）と同じ
/// [`crate::batch_search::pack_f16x2`]/[`crate::batch_search::unpack_f16x2`]
/// で判定するため、ガードとエンコードの丸め規則が乖離しない。非有限値は
/// 呼び出し元（[`max_abs_finite_from_query_iter`]）が別途フィルタ済みの
/// 前提で、常に「往復可能」として扱う。
fn f16_round_trip_exact(v: f32) -> bool {
    if !v.is_finite() {
        return true;
    }
    let (roundtrip, _) = crate::batch_search::unpack_f16x2(crate::batch_search::pack_f16x2(v, 0.0));
    roundtrip == v
}

/// クエリバッチ（f32・パック前）の有限成分のみの絶対値最大・アンダーフロー・
/// 丸め有無を走査する（Issue #539・[`select_dot_shader`] の
/// `query_max_abs`/`query_has_subnormal_underflow`/`query_has_precision_loss`
/// 引数を作る）。`f16` へパックした時点での飽和（±Inf 化）を判定する独立
/// ガード（`query_max_abs > F16_MAX_FINITE`）の母数となるため、f32 の値
/// そのまま（f16 丸め前）で走査する（`select_dot_shader` doc 参照）。
/// 呼び出し元はバッチ全体ではなく [`group_queries_by_ctx`] が返す 1
/// グループ分の index 列（[`max_abs_finite_from_queries_subset`]）を渡し、
/// シェーダ選択の母数を `PolicyContext` グループ単位に分離する
/// （PR #591 レビュー P0 指摘対応）。
fn max_abs_finite_from_queries_subset(
    queries: &[BatchQuery<'_>],
    indices: &[usize],
) -> QueryAmplitudeStats {
    max_abs_finite_from_query_iter(indices.iter().filter_map(|&i| queries.get(i)))
}

fn max_abs_finite_from_query_iter<'a>(
    queries: impl Iterator<Item = &'a BatchQuery<'a>>,
) -> QueryAmplitudeStats {
    let mut max_abs: f32 = 0.0;
    let mut has_subnormal_underflow = false;
    let mut has_precision_loss = false;
    for q in queries {
        for &v in q.vector {
            if !v.is_finite() {
                continue;
            }
            let abs = v.abs();
            max_abs = max_abs.max(abs);
            if abs > 0.0 && abs < F16_MIN_POSITIVE_SUBNORMAL {
                has_subnormal_underflow = true;
            }
            if !f16_round_trip_exact(v) {
                has_precision_loss = true;
            }
        }
    }
    QueryAmplitudeStats {
        max_abs,
        has_subnormal_underflow,
        has_precision_loss,
    }
}

/// [`GpuDotShaderKind`] を GPU デバイス非依存の純関数として決める
/// （`select_readback_mode` と同型。単体テストの対象）。
///
/// f16 算術版は行・クエリを `vec2<f16>` のまま常駐・転送する（積和自体は
/// f32 へ拡張してから行う。[`DOT_SHADER_F16_ARITH_WGSL`] doc 参照）ため、
/// 次のいずれかが崩れると unpack 版との等価性（受け入れ条件の核心）が
/// 壊れる:
///
/// - `f16_arith_available`: アダプタが `SHADER_F16` に対応し、かつ
///   f16 算術版パイプラインの生成に成功していること
///   （[`GpuContext::f16_arith_pipelines`] が `Some`）
/// - `row_max_abs`/`query_max_abs` がいずれも有限であること（非有限は
///   別途 unpack 版でも非有限スコアとして除外される値のため、ここでは
///   「ガード計算自体が意味を持つか」だけを見る）
/// - `query_max_abs <= F16_MAX_FINITE`（65504）: クエリ成分が f16 へ
///   パックされた時点で ±Inf へ飽和すると、unpack 版（f32 のまま計算）
///   では有限のスコアになる行が f16 算術版だけ除外されてしまう
///   （`row_max_abs` の値に関わらず崩れる独立した条件のため、積の判定
///   より先に単独でチェックする）
/// - `row_max_abs * query_max_abs * GPU_F16_ACC_BLOCK <=
///   F16_ARITH_PARTIAL_SUM_LIMIT`: 保守的なオーバーフロー上界判定
///   （[`GPU_F16_ACC_BLOCK`] doc 参照。積和自体は f32 で行うため必須では
///   ないが、既存の安全マージンとして維持する）
/// - `query_has_subnormal_underflow` が偽であること（PR #591 レビュー P1
///   指摘対応）: 上限のみを見る上記オーバーフローガードは、有効な非ゼロ
///   小成分が [`F16_MIN_POSITIVE_SUBNORMAL`] 未満で f16 へ厳密にゼロ丸め
///   されるアンダーフローを検知できない
/// - `query_has_precision_loss` が偽であること（PR #591 レビュー P1
///   指摘対応・2 巡目。[`f16_round_trip_exact`] doc 参照）:
///   振幅上限・アンダーフローの範囲内でも、f16 の仮数部 10 bit では
///   表現できない値は丸められる。この丸めはクエリを f16 へエンコードする
///   時点で発生し、積和を f32 で行っても取り戻せないため独立に検知する。
///   クエリは `F16Arith` 選択時のみ f16 パックされる（`Unpack` 選択時は
///   f32 のまま送る）ため、上記 2 つのガードと同様にクエリ側にのみ新たに
///   生じるリスクである（常駐行列側は両シェーダ共通で既に f16 量子化済み
///   のため対象外。[`max_abs_finite_from_packed`] doc 参照）
fn select_dot_shader(
    f16_arith_available: bool,
    row_max_abs: f32,
    query_max_abs: f32,
    query_has_subnormal_underflow: bool,
    query_has_precision_loss: bool,
) -> GpuDotShaderKind {
    if !f16_arith_available {
        return GpuDotShaderKind::Unpack;
    }
    if !row_max_abs.is_finite() || !query_max_abs.is_finite() {
        return GpuDotShaderKind::Unpack;
    }
    if query_max_abs > F16_MAX_FINITE {
        return GpuDotShaderKind::Unpack;
    }
    if query_has_subnormal_underflow {
        return GpuDotShaderKind::Unpack;
    }
    if query_has_precision_loss {
        return GpuDotShaderKind::Unpack;
    }
    let bound = row_max_abs * query_max_abs * (GPU_F16_ACC_BLOCK as f32);
    if !bound.is_finite() || bound > F16_ARITH_PARTIAL_SUM_LIMIT {
        return GpuDotShaderKind::Unpack;
    }
    GpuDotShaderKind::F16Arith
}

/// クエリバッファをホスト側で `encoding` に従いバイト列化する
/// （[`bytes_of_f32_slice`] を `QueryEncoding::F32` の場合に委譲し、
/// `QueryEncoding::F16Packed` は 2 要素ずつ [`crate::batch_search::pack_f16x2`]
/// で f16 パックする。パック後のバイト数は f32 表現の半分になる）。
///
/// `values.len()` が奇数の場合は呼び出し元（`run_tiled_batch_search`）の
/// `query_stride` 契約違反（f16 経路は常に偶数ストライド）を示すため、
/// GPU に触れる前に拒否する（fail-closed。coding-rust.md「untrusted 入力の
/// 扱い」と同じ「シェーダに触れる前に検証する」方針をホスト内部の不変条件
/// 違反にも適用）。
fn encode_query_bytes(
    values: &[f32],
    encoding: QueryEncoding,
) -> Result<Vec<u8>, BatchBackendError> {
    match encoding {
        QueryEncoding::F32 => bytes_of_f32_slice(values),
        QueryEncoding::F16Packed => {
            if !values.len().is_multiple_of(2) {
                return Err(BatchBackendError::TransferFailed(
                    "f16 packed query buffer length must be even".to_string(),
                ));
            }
            let mut out = Vec::new();
            try_reserve_bytes(&mut out, (values.len() / 2).saturating_mul(4))?;
            // `chunks_exact(2)` は常に長さ 2 のスライスを返す契約だが、
            // clippy `chunks_exact_to_as_chunks` 指摘対応で `as_chunks::<2>()`
            // （配列の固定長ぶんだけ添字アクセス無しで分配可能）へ置き換える。
            let (chunks, _remainder) = values.as_chunks::<2>();
            for &[a, b] in chunks {
                let packed = crate::batch_search::pack_f16x2(a, b);
                out.extend_from_slice(&packed.to_ne_bytes());
            }
            Ok(out)
        }
    }
}

/// [`GpuReadbackMode::PartialTopK`] 経路の 1 dispatch あたり行チャンク数を
/// fail-closed に決める純関数（ADR 決定 3）。1 チャンクの readback バイト数
/// `width × ceil(chunk_rows / GPU_WORKGROUP_SIZE) × k_out × 8`（`(key,slot)`
/// 各 4 byte の u32 ペア）`+ chunk_rows × 4`（行 index バッファ）が
/// `budget_bytes` を超えない最大の `chunk_rows` を、ワークグループ数を
/// 1 から `max_workgroups_per_dimension` まで増やしながら求める
/// （ワークグループ数を固定すれば出力バイト数も固定され、その中で
/// `chunk_rows` を大きくするコストは行 index バッファの線形増分のみのため、
/// 各ワークグループ数の上限 `chunk_rows` で予算を再評価すれば最大値に届く）。
fn plan_partial_topk_chunk_rows(
    width: usize,
    k_out: usize,
    budget_bytes: usize,
    max_workgroups_per_dimension: u32,
) -> usize {
    let width_u64 = width.max(1) as u64;
    let k_out_u64 = (k_out.clamp(1, GPU_TOPK_OUT_MAX as usize)) as u64;
    let per_workgroup_output_bytes = width_u64.saturating_mul(k_out_u64).saturating_mul(8);
    let budget = budget_bytes as u64;
    let max_wg = (max_workgroups_per_dimension.max(1)) as u64;

    let mut best_chunk_rows: u64 = 0;
    let mut wg: u64 = 1;
    while wg <= max_wg {
        let bracket_top = wg.saturating_mul(GPU_WORKGROUP_SIZE as u64);
        let bracket_bottom = (wg - 1).saturating_mul(GPU_WORKGROUP_SIZE as u64) + 1;
        let fixed_cost = per_workgroup_output_bytes.saturating_mul(wg);
        if fixed_cost >= budget {
            break;
        }
        let remaining = budget.saturating_sub(fixed_cost);
        let max_rows_by_bytes = remaining / 4;
        let candidate = max_rows_by_bytes.min(bracket_top);
        if candidate < bracket_bottom {
            break;
        }
        best_chunk_rows = candidate;
        wg = wg.saturating_add(1);
    }

    usize::try_from(best_chunk_rows)
        .unwrap_or(usize::MAX)
        .max(1)
}

/// GPU から readback した部分 Top-k 候補列（`(key: u32, slot: u32)` を
/// `num_workgroups × k_out` 件（クエリごと）並べたもの）を、対応する
/// `chunk`（[`gather_reachable_rows`] が返す昇順スロット列）と照合しつつ
/// クエリごとの [`TopKSelector`] へ push する（ADR 決定 3。GPU デバイス
/// 非依存の純関数で単体テスト対象）。
///
/// - 件数不一致（readback 破損の疑い）は [`BatchBackendError::TransferFailed`]。
/// - 番兵（`slot == 0xFFFFFFFF`）は skip。
/// - 番兵以外の `slot` が `chunk` に存在しない場合は readback 破損とみなし
///   `TransferFailed`（部分結果を返さない。fail-closed）。
/// - 非有限スコア（キー変換の往復で `!is_finite()` になったもの）は
///   `TopKSelector::push` 側の無視と二重に skip する。
fn merge_partial_topk_readback(
    readback: &[u32],
    chunk: &[u32],
    width: usize,
    num_workgroups: usize,
    k_out: usize,
    selectors: &mut [Option<TopKSelector>],
) -> Result<(), BatchBackendError> {
    let expected_len = width
        .checked_mul(num_workgroups)
        .and_then(|v| v.checked_mul(k_out))
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| {
            BatchBackendError::TransferFailed("partial topk readback size overflow".to_string())
        })?;
    if readback.len() != expected_len {
        return Err(BatchBackendError::TransferFailed(
            "partial topk readback length mismatch".to_string(),
        ));
    }

    for q in 0..width {
        let Some(selector_slot) = selectors.get_mut(q) else {
            continue;
        };
        let Some(selector) = selector_slot.as_mut() else {
            continue;
        };
        for wg in 0..num_workgroups {
            for slot_idx in 0..k_out {
                let base = ((q * num_workgroups + wg) * k_out + slot_idx) * 2;
                let (Some(&key), Some(&slot)) = (readback.get(base), readback.get(base + 1)) else {
                    return Err(BatchBackendError::TransferFailed(
                        "partial topk readback index out of range".to_string(),
                    ));
                };
                if slot == u32::MAX {
                    continue;
                }
                if chunk.binary_search(&slot).is_err() {
                    return Err(BatchBackendError::TransferFailed(
                        "partial topk readback slot outside dispatched chunk".to_string(),
                    ));
                }
                let score = score_from_key(key);
                if !score.is_finite() {
                    continue;
                }
                selector.push(CandidateHit {
                    id: u64::from(slot),
                    score,
                });
            }
        }
    }
    Ok(())
}

/// [`GpuBatchBackend`]/[`GpuF32ContrastBackend`] の readback 方式別 dispatch
/// 回数・バイト数を数える統計（Issue #536・#537 が前後比較・非 vacuous 判定に
/// 使う `stats()` の実体）。可視性判定・スコア計算には一切関与しない
/// 性能観測専用のカウンタで、テナント・行数・可視カーディナリティは
/// 保持しない。
#[derive(Debug, Default)]
struct GpuBatchStats {
    partial_topk_dispatches: std::sync::atomic::AtomicU64,
    full_readback_dispatches: std::sync::atomic::AtomicU64,
    /// Top-k パイプラインは利用可能だが、当該タイルの `k` が
    /// [`GPU_TOPK_OUT_MAX`] を超える等の理由で全量 readback へ縮退した回数
    /// （ADR 決定 2 の縮退が実際に発生した観測点）。
    full_readback_fallbacks: std::sync::atomic::AtomicU64,
    readback_bytes: std::sync::atomic::AtomicU64,
    /// [`select_dot_shader`] が `GpuDotShaderKind::F16Arith` を選んだ
    /// `batch_search` 呼び出し回数（Issue #539）。`partial_topk_dispatches`/
    /// `full_readback_dispatches`（`run_tiled_batch_search` 内でチャンク単位
    /// に実 GPU dispatch が成功するたび加算）とは加算タイミングが異なり、
    /// 本カウンタは選択直後（`run_tiled_batch_search` 呼び出し前）に 1 回
    /// だけ加算する。そのためこの後の dispatch が失敗しても本カウンタは
    /// 減らない（「選択された回数」であって「成功した dispatch 回数」では
    /// ない）。非 vacuous 判定（f16 経路が実際に選ばれたか）の用途では
    /// この違いは問題にならない。
    f16_arith_dispatches: std::sync::atomic::AtomicU64,
    /// f16 算術版パイプラインは利用可能（`f16_arith_available()` が true）
    /// だが、[`select_dot_shader`] のオーバーフローガード不成立により
    /// unpack 版へ縮退した回数（Issue #539。ガードが実際に働いた観測点）。
    f16_arith_guard_fallbacks: std::sync::atomic::AtomicU64,
}

impl GpuBatchStats {
    fn snapshot(&self) -> GpuBatchStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        GpuBatchStatsSnapshot {
            partial_topk_dispatches: self.partial_topk_dispatches.load(Relaxed),
            full_readback_dispatches: self.full_readback_dispatches.load(Relaxed),
            full_readback_fallbacks: self.full_readback_fallbacks.load(Relaxed),
            readback_bytes: self.readback_bytes.load(Relaxed),
            f16_arith_dispatches: self.f16_arith_dispatches.load(Relaxed),
            f16_arith_guard_fallbacks: self.f16_arith_guard_fallbacks.load(Relaxed),
        }
    }
}

/// [`GpuBatchStats`] の外部公開スナップショット（`stats()` の戻り値）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuBatchStatsSnapshot {
    pub partial_topk_dispatches: u64,
    pub full_readback_dispatches: u64,
    pub full_readback_fallbacks: u64,
    pub readback_bytes: u64,
    pub f16_arith_dispatches: u64,
    pub f16_arith_guard_fallbacks: u64,
}

/// プロセス共有の GPU デバイス文脈（adapter/device/queue/pipeline）。
/// [`OnceLock`] で 1 回だけ初期化し、初期化失敗も含めて結果をキャッシュする
/// （毎回の `GpuBatchBackend::try_new` が重い初期化をやり直さないため）。
struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    /// workgroup 内部分 Top-k パイプライン（f16 パック常駐・本番経路。
    /// Issue #536）。シェーダ生成・検証に失敗した場合も `init_gpu_context`
    /// 自体は失敗させず `None` にする（ADR 決定 2: 段階的 fail-closed 縮退。
    /// 呼び出し元は [`select_readback_mode`] で常に全量 readback 側へ倒れる）。
    topk_pipeline: Option<wgpu::ComputePipeline>,
    /// [`topk_pipeline`] が `None` になった理由（英語・adapter 名やテナント
    /// 情報を含まない）。診断・`EXPLAIN` 等の将来的な露出のために保持するが、
    /// 本 Issue では未参照（`#[allow(dead_code)]`）。
    #[allow(dead_code)]
    topk_unavailable_reason: Option<String>,
    /// f16 算術版の内積／Top-k パイプライン一式（Issue #539）。`SHADER_F16`
    /// をデバイスへ要求できた場合のみ `Some`。パイプライン生成自体の失敗も
    /// （feature 要求の成否とは独立に）`None` へ吸収する（`topk_pipeline` と
    /// 同じ段階的 fail-closed 縮退。呼び出し元は [`select_dot_shader`] で
    /// 常に [`GpuDotShaderKind::Unpack`] 側へ倒れる）。
    f16_arith_pipelines: Option<F16ArithPipelines>,
    /// f16 算術版が利用できない理由（英語・adapter 名やテナント情報を含まない）。
    /// 診断・将来的な `EXPLAIN` 露出のために保持するが、本 Issue では未参照。
    #[allow(dead_code)]
    f16_arith_unavailable_reason: Option<String>,
    /// `request_device` に `Features::SHADER_F16` を要求したかどうか
    /// （adapter が対応を報告した場合のみ true）。要求そのものが失敗した
    /// 場合は `Features::empty()` へ 1 回だけ再試行するため、実際に feature
    /// 付きデバイスが得られたかは `f16_arith_pipelines.is_some()` 側で見る。
    #[allow(dead_code)]
    shader_f16_requested: bool,
    bind_group_layout: wgpu::BindGroupLayout,
    max_storage_buffer_binding_size: u64,
    max_workgroups_per_dimension: u32,
    /// adapter が実装する wgpu backend（Vulkan/Metal/DX12 等。Issue #542）。
    /// `packed_i8::GpuI8Meta::backend` が meta として公開する値の出所で、
    /// 専用整数内積命令（`OpSDot`/`dot4add_i8packed`/`packed_char4`）と
    /// polyfill のどちらが選ばれたかを判別する材料に使う想定
    /// （`docs/design/gpu-batch-i8-packed.md` の backend 別規則表参照。
    /// wgpu 30.0.1 の公開 API では判別結果そのものは取得できないため、
    /// 本フィールド自体は判別を行わない）。
    backend: wgpu::Backend,
    /// デバイスロスト検知用ラッチ。`Device::set_device_lost_callback` から
    /// 更新される（コールバックは別スレッドから呼ばれうるため `AtomicBool`）。
    device_lost: std::sync::Arc<AtomicBool>,
    /// error scope 外で発生した wgpu エラーのラッチ（`Device::on_uncaptured_error`
    /// から更新）。既定ハンドラの panic を避けつつ、異常を握り潰さないための記録で、
    /// `GpuBatchBackend::batch_search` が検知すると backend エラーを返して CPU 縮退へ倒す。
    uncaptured_error: std::sync::Arc<AtomicBool>,
}

fn global_context() -> &'static Result<GpuContext, String> {
    static CONTEXT: OnceLock<Result<GpuContext, String>> = OnceLock::new();
    CONTEXT.get_or_init(init_gpu_context)
}

/// プロセス共有 `wgpu::Device` への dispatch・バッファアップロードを直列化する
/// グローバルロック（[`global_context`] と同じ生存範囲）。
///
/// [`GpuContext`] がプロセス単位で 1 つなのに対し [`GpuBatchBackend`] は
/// 複数インスタンス存在しうるため、直列化はインスタンス単位ではなくプロセス単位で
/// 行う（codex P1 指摘対応）。poisoning は無視する（保護対象は `()` のみで、
/// panic により壊れる不変条件を持たないため。`batch_search.rs::row_buffer_pool`
/// と同じ方針）。
fn gpu_dispatch_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// GPU デバイスの初期化本体。失敗はすべて `Err(String)`（英語・adapter 名や
/// テナント情報を含まない）として返し、panic 経路（`Instance::new` の一部条件
/// 等）を事前ガードで避ける（TASK-128 設計ドキュメント §3.2 ポインタ）。
fn init_gpu_context() -> Result<GpuContext, String> {
    if wgpu::Instance::enabled_backend_features().is_empty() {
        return Err("no wgpu backend compiled into this binary".to_string());
    }

    // `InstanceDescriptor::new_without_display_handle` を使い、`WGPU_*` 環境
    // 変数を読む `from_env` 系は使わない（CORE-12: 経路を外部から上書きする
    // 機構を設けない方針。`dispatch.rs` モジュールドキュメント参照）。
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

    let adapter = pollster_free_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        // fingerprinting 対策の limit bucketing は untrusted content へ wgpu
        // を露出する用途向けの機能で、engine は自プロセス内でのみ GPU を
        // 使うため無効のままでよい（実測 limits をそのまま使う）。
        apply_limit_buckets: false,
    }))
    .map_err(|()| "adapter request timed out".to_string())?
    .map_err(|e| format!("adapter request failed: {e}"))?;

    let info = adapter.get_info();
    if info.device_type == wgpu::DeviceType::Cpu {
        // lavapipe 等のソフトウェア実装は「GPU 経路」としての capability を
        // 偽陽性にしないため拒否する（CORE-8/16 の性能ゲート意図に反するため）。
        return Err("adapter is a software (CPU) implementation".to_string());
    }

    let adapter_limits = adapter.limits();
    let mut required_limits = wgpu::Limits::downlevel_defaults();
    required_limits.max_storage_buffer_binding_size =
        adapter_limits.max_storage_buffer_binding_size;
    required_limits.max_buffer_size = adapter_limits.max_buffer_size;
    required_limits.max_compute_workgroups_per_dimension =
        adapter_limits.max_compute_workgroups_per_dimension;
    required_limits.max_compute_invocations_per_workgroup = adapter_limits
        .max_compute_invocations_per_workgroup
        .max(256);
    required_limits.max_compute_workgroup_size_x =
        adapter_limits.max_compute_workgroup_size_x.max(256);

    // `SHADER_F16`（Issue #539）はアダプタが対応を報告した場合のみ要求する。
    // プロセス共有 `wgpu::Device` は `OnceLock` で 1 回しか作られないため、
    // feature 要求そのものが原因で device 生成に失敗すると GPU 経路全体が
    // 死んでしまう。そのため feature 付き要求が失敗した場合は
    // `Features::empty()` で 1 回だけ再試行し、GPU 経路自体は必ず既存の
    // unpack 版シェーダで動作を続けられるようにする（ADR 決定 2 と同じ
    // 段階的 fail-closed 縮退）。
    let shader_f16_requested = adapter.features().contains(wgpu::Features::SHADER_F16);
    let device_descriptor =
        |features: wgpu::Features, limits: wgpu::Limits| wgpu::DeviceDescriptor {
            label: Some("fandhe-db batch backend"),
            required_features: features,
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        };
    let (device, queue, shader_f16_enabled) = if shader_f16_requested {
        match pollster_free_block_on(adapter.request_device(&device_descriptor(
            wgpu::Features::SHADER_F16,
            required_limits.clone(),
        ))) {
            Ok(Ok((device, queue))) => (device, queue, true),
            _ => {
                let (device, queue) = pollster_free_block_on(adapter.request_device(
                    &device_descriptor(wgpu::Features::empty(), required_limits.clone()),
                ))
                .map_err(|()| "device request timed out".to_string())?
                .map_err(|e| format!("device request failed: {e}"))?;
                (device, queue, false)
            }
        }
    } else {
        let (device, queue) = pollster_free_block_on(adapter.request_device(&device_descriptor(
            wgpu::Features::empty(),
            required_limits.clone(),
        )))
        .map_err(|()| "device request timed out".to_string())?
        .map_err(|e| format!("device request failed: {e}"))?;
        (device, queue, false)
    };

    let device_lost = std::sync::Arc::new(AtomicBool::new(false));
    let device_lost_flag = device_lost.clone();
    device.set_device_lost_callback(move |_reason, _msg| {
        device_lost_flag.store(true, Ordering::SeqCst);
    });

    // error scope で捕捉しきれなかったエラーの既定ハンドラは panic しうる
    // （wgpu の既定動作）。engine はライブラリクレートであり panic させない
    // 契約（coding-rust.md）のため、独自ハンドラで `uncaptured_error` ラッチへ
    // 記録するだけに置き換える。ラッチは次回以降の `batch_search` 冒頭で
    // 参照され、GPU 経路を使わず CPU 縮退（CORE-8）へ倒すための入力になる
    // （codex/Bugbot P1 指摘対応: scope 外の wgpu 操作が panic しうる問題）。
    let uncaptured_error = std::sync::Arc::new(AtomicBool::new(false));
    let uncaptured_error_flag = uncaptured_error.clone();
    device.on_uncaptured_error(std::sync::Arc::new(move |_e: wgpu::Error| {
        uncaptured_error_flag.store(true, Ordering::SeqCst);
    }));

    // シェーダ・レイアウト・パイプライン生成も error scope の内側で行う
    // （codex P1 指摘対応: scope 外の生成失敗は uncaptured error 扱いになり、
    // 上記ハンドラ導入前は panic しえた。ここで捕捉して `Err` へ写像する）。
    let init_validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let init_oom_scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("batch dot product"),
        source: wgpu::ShaderSource::Wgsl(DOT_SHADER_WGSL.into()),
    });

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("batch dot product bind group layout"),
        entries: &[
            storage_layout_entry(0, wgpu::BufferBindingType::Uniform),
            storage_layout_entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
            storage_layout_entry(2, wgpu::BufferBindingType::Storage { read_only: true }),
            storage_layout_entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
            storage_layout_entry(4, wgpu::BufferBindingType::Storage { read_only: false }),
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("batch dot product pipeline layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });

    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("batch dot product pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    // LIFO で pop する（後に push した OutOfMemory スコープを先に pop する）。
    // 待機中も `device.poll` を駆動する（`block_on_with_device_poll`）ため、
    // デバイスのポーリング待ちで無限スピンしない。
    if block_on_with_device_poll(&device, init_oom_scope.pop())
        .map_err(|_| "device poll failed or timed out during pipeline creation".to_string())?
        .is_some()
    {
        return Err("gpu out of memory during pipeline creation".to_string());
    }
    if block_on_with_device_poll(&device, init_validation_scope.pop())
        .map_err(|_| "device poll failed or timed out during pipeline creation".to_string())?
        .is_some()
    {
        return Err("gpu validation error during pipeline creation".to_string());
    }

    let max_workgroups_per_dimension = if adapter_limits.max_compute_workgroups_per_dimension > 0 {
        adapter_limits
            .max_compute_workgroups_per_dimension
            .min(MAX_WORKGROUPS_PER_DIMENSION_FALLBACK)
    } else {
        MAX_WORKGROUPS_PER_DIMENSION_FALLBACK
    };

    // Top-k パイプラインの生成は独立した error scope で試み、失敗しても
    // `init_gpu_context` 自体は失敗させない（ADR 決定 2: 段階的 fail-closed
    // 縮退。本番の内積 dispatch 経路は Top-k 抜きでも従来どおり動く）。
    let (topk_pipeline, topk_unavailable_reason) =
        match create_topk_pipeline(&device, &bind_group_layout, DOT_SHADER_TOPK_WGSL, "f16") {
            Ok(p) => (Some(p), None),
            Err(msg) => (None, Some(msg)),
        };

    // f16 算術版パイプライン（Issue #539）は `shader_f16_enabled`（feature
    // 要求が実際に通った場合のみ）でのみ生成を試みる。feature 非対応の
    // デバイスで `enable f16;` を含むシェーダをコンパイルすると naga が
    // capability 不足として validation エラーにするため（`enable
    // subgroups;` と同じ扱い。ADR §1.3 実測ポインタ）、そもそも試みない。
    // 生成失敗（内積・Top-k いずれも）は `init_gpu_context` 自体を失敗させず
    // `None` へ吸収する（`topk_pipeline` と同じ段階的 fail-closed 縮退）。
    let (f16_arith_pipelines, f16_arith_unavailable_reason) = if shader_f16_enabled {
        match create_topk_pipeline(
            &device,
            &bind_group_layout,
            DOT_SHADER_F16_ARITH_WGSL,
            "f16-arith-dot",
        ) {
            Ok(dot) => {
                // Top-k 側の生成失敗は内積側の可用性へ影響させない（`topk_pipeline`
                // と同方針。`select_readback_mode` が Top-k 非対応時に常に全量
                // readback へ倒すのと同じ構造で `select_dot_shader` の判定とは独立）。
                let topk = create_topk_pipeline(
                    &device,
                    &bind_group_layout,
                    DOT_SHADER_TOPK_F16_ARITH_WGSL,
                    "f16-arith-topk",
                )
                .ok();
                (Some(F16ArithPipelines { dot, topk }), None)
            }
            Err(msg) => (None, Some(msg)),
        }
    } else {
        (
            None,
            Some("adapter or device does not support SHADER_F16".to_string()),
        )
    };

    Ok(GpuContext {
        device,
        queue,
        pipeline,
        topk_pipeline,
        topk_unavailable_reason,
        f16_arith_pipelines,
        f16_arith_unavailable_reason,
        shader_f16_requested,
        bind_group_layout,
        max_storage_buffer_binding_size: adapter_limits.max_storage_buffer_binding_size,
        max_workgroups_per_dimension,
        device_lost,
        uncaptured_error,
        backend: info.backend,
    })
}

/// [`GpuContext::f16_arith_pipelines`] の内訳（Issue #539）。内積本体
/// （`dot`）は `SHADER_F16` 対応・パイプライン生成成功の必須条件だが、
/// workgroup 内部分 Top-k（`topk`）は既存の [`GpuContext::topk_pipeline`]
/// と同じく生成失敗を `None` へ吸収する（Top-k 非対応時は
/// [`select_readback_mode`] が全量 readback 側へ倒すため、f16 算術版でも
/// `dot` さえ使えれば全量 readback 経路は動く）。
struct F16ArithPipelines {
    dot: wgpu::ComputePipeline,
    topk: Option<wgpu::ComputePipeline>,
}

/// [`GpuContext::topk_pipeline`]／CORE-16 対照経路の Top-k パイプライン生成
/// 共通処理（Issue #536）。既存パイプライン生成（`init_gpu_context`・
/// `init_f32_contrast_pipeline`）と同じ手順（独立 error scope → シェーダ・
/// パイプライン生成 → LIFO pop → 失敗を `Err(String)` へ写像）を踏むが、
/// 失敗を上位へ伝播させるだけで `init_gpu_context` 全体を失敗させない
/// （呼び出し元が `Option` へ吸収する）。
fn create_topk_pipeline(
    device: &wgpu::Device,
    bind_group_layout: &wgpu::BindGroupLayout,
    shader_src: &str,
    label: &str,
) -> Result<wgpu::ComputePipeline, String> {
    let validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("batch dot product topk"),
        source: wgpu::ShaderSource::Wgsl(shader_src.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("batch dot product topk pipeline layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("batch dot product topk pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    if block_on_with_device_poll(device, oom_scope.pop())
        .map_err(|_| {
            format!("device poll failed or timed out during {label} topk pipeline creation")
        })?
        .is_some()
    {
        return Err(format!(
            "gpu out of memory during {label} topk pipeline creation"
        ));
    }
    if block_on_with_device_poll(device, validation_scope.pop())
        .map_err(|_| {
            format!("device poll failed or timed out during {label} topk pipeline creation")
        })?
        .is_some()
    {
        return Err(format!(
            "gpu validation error during {label} topk pipeline creation"
        ));
    }

    Ok(pipeline)
}

/// CORE-16 対照経路（[`GpuF32ContrastBackend`]）専用の compute pipeline を
/// プロセス内で 1 回だけ遅延初期化する（Issue #234）。本番 dispatch 経路
/// （[`GpuBatchBackend`]）の初期化コストへ影響させないため [`GpuContext`] には
/// 足さず、独立の [`OnceLock`] として持つ。生成に失敗した場合もその結果を
/// キャッシュし、以降の呼び出しは再試行しない（[`global_context`] と同方針）。
///
/// bind group layout は本番経路と共用する（[`DOT_SHADER_F32_WGSL`] のドキュメント
/// コメント参照。バインディング構成が同一のため wgpu のレイアウト検証上は
/// 区別されない）。
fn f32_contrast_pipeline() -> Result<&'static ContrastPipelines, String> {
    static PIPELINE: OnceLock<Result<ContrastPipelines, String>> = OnceLock::new();
    let ctx = global_context().as_ref().map_err(String::clone)?;
    let result = PIPELINE.get_or_init(|| {
        // 生成は本番 dispatch と同じプロセス単位ロックの下で行う（codex P1
        // 指摘対応と同方針: 共有 `wgpu::Device` への操作を並行させない）。
        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        init_f32_contrast_pipeline(ctx)
    });
    result.as_ref().map_err(String::clone)
}

/// [`f32_contrast_pipeline`] が保持する対照経路の compute pipeline 一式
/// （Issue #536: 内積本体 `dot` に加え、CORE-16 の公平性のため workgroup 内
/// 部分 Top-k パイプライン `topk` も同じ生成タイミングで確保する）。
struct ContrastPipelines {
    dot: wgpu::ComputePipeline,
    topk: Option<wgpu::ComputePipeline>,
}

/// [`f32_contrast_pipeline`] の初期化本体。`init_gpu_context` のパイプライン
/// 生成部と同じ手順（error scope で生成失敗を捕捉 → `Err` へ写像）を踏むが、
/// device/queue/bind_group_layout は共有の [`GpuContext`] から借用するだけで
/// 新規作成しない。Top-k パイプラインの生成失敗は本体（`dot`）の初期化を
/// 失敗させず `None` に吸収する（[`GpuContext::topk_pipeline`] と同方針）。
fn init_f32_contrast_pipeline(ctx: &GpuContext) -> Result<ContrastPipelines, String> {
    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("batch dot product f32 contrast"),
            source: wgpu::ShaderSource::Wgsl(DOT_SHADER_F32_WGSL.into()),
        });
    let pipeline_layout = ctx
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("batch dot product f32 contrast pipeline layout"),
            bind_group_layouts: &[Some(&ctx.bind_group_layout)],
            immediate_size: 0,
        });
    let pipeline = ctx
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("batch dot product f32 contrast pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

    if block_on_with_device_poll(&ctx.device, oom_scope.pop())
        .map_err(|_| {
            "device poll failed or timed out during f32 contrast pipeline creation".to_string()
        })?
        .is_some()
    {
        return Err("gpu out of memory during f32 contrast pipeline creation".to_string());
    }
    if block_on_with_device_poll(&ctx.device, validation_scope.pop())
        .map_err(|_| {
            "device poll failed or timed out during f32 contrast pipeline creation".to_string()
        })?
        .is_some()
    {
        return Err("gpu validation error during f32 contrast pipeline creation".to_string());
    }

    let topk = create_topk_pipeline(
        &ctx.device,
        &ctx.bind_group_layout,
        DOT_SHADER_TOPK_F32_WGSL,
        "f32 contrast",
    )
    .ok();

    Ok(ContrastPipelines {
        dot: pipeline,
        topk,
    })
}

fn storage_layout_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// std-only の同期化ヘルパー（`pollster` 依存を追加しない。
/// .claude/rules/dependency-policy.md「依存最小方針」）。`Waker::noop()` は
/// std 安定 API（Rust 1.85+。本リポ toolchain は stable 1.96 想定）で、
/// wgpu の `request_adapter`/`request_device` は native バックエンドでは
/// 即座に完結するため通常 1 回の `poll` で `Ready` になるが、ドライバ無応答時に
/// 永久停止しないよう [`GPU_POLL_DEADLINE`] で打ち切る（超過は `Err(())` を返し、
/// 呼び出し元が `InitFailed` へ写像して CPU 縮退させる）。
fn pollster_free_block_on<F: std::future::Future>(fut: F) -> Result<F::Output, ()> {
    use std::task::{Context, Poll, Waker};

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut boxed = Box::pin(fut);
    let deadline = std::time::Instant::now() + GPU_POLL_DEADLINE;
    loop {
        match boxed.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return Ok(v),
            Poll::Pending => {
                // deadline 超過は「adapter 列挙・デバイス要求が応答しない」異常と
                // みなして打ち切る（codex P1 指摘対応: 終了条件のない自己ポーリングは
                // ドライバ無応答環境で `try_new` を永久停止させ、`InitFailed` を返して
                // CPU-SIMD へ縮退する CORE-8 の契約を満たせない）。
                if std::time::Instant::now() >= deadline {
                    return Err(());
                }
                std::thread::yield_now();
            }
        }
    }
}

/// `device.poll` を駆動しながら future を完了させる同期化ヘルパー
/// （codex/Bugbot P1 指摘対応: `push_error_scope`/`pop` の future は
/// デバイスをポーリングするまで `Pending` のままになりうるため、
/// [`pollster_free_block_on`] の自己ポーリングだけでは進行せずハングする）。
/// ポーリング自体が失敗した場合はデバイス異常として `Err(())` を返し、
/// 呼び出し元がデバイスロスト・初期化失敗として写像する。
fn block_on_with_device_poll<F: std::future::Future>(
    device: &wgpu::Device,
    fut: F,
) -> Result<F::Output, ()> {
    use std::task::{Context, Poll, Waker};

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut boxed = Box::pin(fut);
    let deadline = std::time::Instant::now() + GPU_POLL_DEADLINE;
    loop {
        if let Poll::Ready(v) = boxed.as_mut().poll(&mut cx) {
            return Ok(v);
        }
        // 送信済みコマンド・コールバックを進める。`PollType::Poll`（非ブロッキング）
        // を使い、完了していなければ次のループで future を再ポーリングする。
        if device.poll(wgpu::PollType::Poll).is_err() {
            return Err(());
        }
        // deadline 超過は「完了通知が来ない」異常として打ち切る
        // （codex/Bugbot 指摘対応: 終了条件のないループはハングになる）。
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        std::thread::yield_now();
    }
}

/// GPU バックエンド構築・実行時の入力エラー（[`BatchExecError::Input`] へ写像）。
/// テナント情報・adapter 製品名は含めない。
///
/// 計算量超過（`WorkBudgetExceeded`）の判定は主防御線
/// [`check_reachable_batch_work`] が `BatchSearchError` を直接返すため、本 enum は
/// 確保失敗のみを表す（旧 `WorkBudgetExceeded` バリアントは、到達不能だった
/// `gather_reachable_rows` 内の全行課金チェックとともに削除した）。
#[derive(Debug, Clone, PartialEq)]
enum GpuInputError {
    CapacityExceeded,
}

impl GpuInputError {
    fn into_batch_search_error(self) -> crate::batch_search::BatchSearchError {
        use crate::batch_search::BatchSearchError;
        match self {
            GpuInputError::CapacityExceeded => BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: GPU_SCORE_BUFFER_BUDGET_BYTES,
            },
        }
    }
}

/// 実 GPU バックエンド（TASK-128〜130。CORE-6, 8, 16 ポインタ）。
/// [`crate::batch_fallback::BatchBackend`] の実装として
/// [`crate::batch_fallback::FallbackBatchEngine::build_with_gpu`] から
/// primary として差し込まれる。
pub struct GpuBatchBackend {
    matrix: crate::batch_search::ResidentMatrix,
    row_buffer: wgpu::Buffer,
    /// 実行時エラー（デバイスロスト等）の 1 回限りのラッチではなく、呼び出し
    /// ごとにデバイスロストの有無を確認するための共有フラグへの参照
    /// （`GpuContext::device_lost` と同一。`FallbackBatchEngine` 側の
    /// `runtime_latched` とは独立: 本フィールドは「このプロセスの GPU
    /// デバイスが失われたか」を見るだけで、縮退の可否判断自体は
    /// `batch_fallback.rs` が担う）。
    device_lost: std::sync::Arc<AtomicBool>,
    /// `GpuContext::uncaptured_error` と同一のラッチ（error scope 外で発生した
    /// wgpu エラーの記録）。`batch_search` 冒頭で参照し、記録があれば GPU 経路を
    /// 使わず backend エラーを返して CPU 縮退（CORE-8）へ倒す。
    uncaptured_error: std::sync::Arc<AtomicBool>,
    /// readback 方式別の dispatch 回数・バイト数（Issue #536・#537 が
    /// `stats()` 経由で読む）。インスタンス単位（`GpuContext` のようなプロセス
    /// 共有ではない）で、このバックエンドが処理した dispatch のみを数える。
    stats: std::sync::Arc<GpuBatchStats>,
    /// 常駐行ごとの有限成分のみの絶対値最大（Issue #539・PR #591 レビュー
    /// P2 指摘対応）。`try_new` で 1 回だけ全行を走査して確定させ、以降の
    /// `batch_search` 呼び出しでは `PolicyContext` ごとの可視行 index
    /// （[`gather_reachable_rows`]）に対する単純な配列参照 + 最大値集約
    /// （[`max_abs_finite_from_precomputed_rows`]）だけで S0 シェーダ選択の
    /// 母数を求める。各要素は対応する行自身の f16 常駐データのみに由来し
    /// 他行・他テナントの値を含まないため、行内容と同じ扱いでキャッシュして
    /// よい（可視性の判定は従来どおり `gather_reachable_rows`/
    /// `PolicyContext::is_visible` が担う。本フィールドはテナント境界を
    /// 判定しない）。
    row_max_abs: Vec<f32>,
}

impl GpuBatchBackend {
    /// 常駐行列から GPU バックエンドを構築する。GPU デバイスの初期化
    /// （[`global_context`]）はプロセス内で 1 回だけ行われ、以降の呼び出しは
    /// キャッシュされた結果（成功・失敗いずれも）を使う。シグネチャは
    /// `batch_fallback.rs::FallbackBatchEngine::build` の `backend_factory`
    /// 引数（`FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>,
    /// BatchBackendError>`）にそのまま渡せる形にする（`build_with_gpu` 参照）。
    pub fn try_new(matrix: crate::batch_search::ResidentMatrix) -> Result<Self, BatchBackendError> {
        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => return Err(BatchBackendError::InitFailed(msg.clone())),
        };

        let packed_bytes = matrix
            .packed()
            .len()
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| {
                BatchBackendError::InitFailed("packed matrix byte size overflow".to_string())
            })?;
        if packed_bytes as u64 > ctx.max_storage_buffer_binding_size {
            return Err(BatchBackendError::InitFailed(
                "resident matrix exceeds adapter storage buffer limit".to_string(),
            ));
        }
        // wgpu は 0 バイトのバッファ作成を許さない実装があるため、空行列は
        // GPU 経路を使わず CPU 縮退へ委ねる（`FallbackBatchEngine::build` が
        // 空行列を許容する契約と衝突しないよう、ここでは軽い理由で `InitFailed`
        // にする）。
        if packed_bytes == 0 {
            return Err(BatchBackendError::InitFailed(
                "resident matrix is empty".to_string(),
            ));
        }

        // 常駐行列バッファの確保・アップロードも error scope の内側で行い、
        // 失敗を `InitFailed`（＝CPU 縮退）へ写像する（codex P1 指摘対応）。
        // `batch_search` の dispatch と同じグローバルロックで保護し、共有 Device
        // への操作が並行しないようにする（codex P1 指摘対応）。
        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // ステージング確保は scope の外で先に済ませる（`dispatch_dot_products`
        // と同じ理由。codex P1 指摘対応）。
        let packed_staging = bytes_of_u32_slice(matrix.packed())?;
        let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let row_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resident matrix packed rows"),
            size: packed_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&row_buffer, 0, &packed_staging);
        let poll_failed = || {
            BatchBackendError::InitFailed(
                "device poll failed or timed out during buffer upload".to_string(),
            )
        };
        if block_on_with_device_poll(&ctx.device, oom_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu out of memory while uploading the resident matrix".to_string(),
            ));
        }
        if block_on_with_device_poll(&ctx.device, validation_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu validation error while uploading the resident matrix".to_string(),
            ));
        }

        // f16 算術版の選択可否ガード（Issue #539・[`select_dot_shader`]）に
        // 使う「行の有限成分のみの絶対値最大」は行ごとに独立な値（他行・
        // 他テナントの値に依存しない）なので、ここで 1 回だけ全行を走査して
        // 確定させる（PR #591 レビュー P2 指摘対応: 以前は `batch_search`
        // 呼び出しのたびに可視行を unpack し直していたため、GPU dispatch 前に
        // プロセス共有ロック保持中で O(可視行数 × 次元数) の CPU 処理が
        // 発生していた）。可視行の集計（＝どの `PolicyContext` から見えるか）
        // は依然として `batch_search` 呼び出しごとに [`gather_reachable_rows`]
        // で求め、他テナントの不可視行の値が集約へ混ざらないようにする
        // （PR #591 レビュー P0 指摘対応。[`max_abs_finite_from_precomputed_rows`]
        // 参照）。
        let dim_half = matrix.dim().div_ceil(2);
        let row_count = matrix.row_count();
        let mut row_max_abs: Vec<f32> = Vec::new();
        row_max_abs.try_reserve_exact(row_count).map_err(|_| {
            BatchBackendError::InitFailed("row amplitude cache alloc failed".to_string())
        })?;
        for row_idx in 0..row_count {
            let start = row_idx.saturating_mul(dim_half);
            let end = start.saturating_add(dim_half);
            let row = matrix.packed().get(start..end).unwrap_or(&[]);
            row_max_abs.push(max_abs_finite_from_packed(row));
        }

        Ok(Self {
            matrix,
            row_buffer,
            device_lost: ctx.device_lost.clone(),
            uncaptured_error: ctx.uncaptured_error.clone(),
            stats: std::sync::Arc::new(GpuBatchStats::default()),
            row_max_abs,
        })
    }

    /// `SHADER_F16` 対応アダプタでこのプロセスの GPU 経路が f16 算術版
    /// シェーダを使えるかどうか（Issue #539）。テナント・行数などの情報は
    /// 含まない、GPU 初期化結果のみに依存する情報提供専用の問い合わせ。
    pub fn f16_arith_available(&self) -> bool {
        matches!(global_context(), Ok(ctx) if ctx.f16_arith_pipelines.is_some())
    }

    /// readback 方式別の dispatch 回数・バイト数のスナップショット
    /// （Issue #536。#537 の前後比較・非 vacuous 判定が読む）。
    pub fn stats(&self) -> GpuBatchStatsSnapshot {
        self.stats.snapshot()
    }
}

/// `&[u32]` を `to_ne_bytes` で `&[u8]` 相当のバイト列へ変換する
/// （`bytemuck` 不採用。依存最小方針）。返す `Vec<u8>` は呼び出し元が
/// `Queue::write_buffer` へそのまま渡す想定の一時バッファ。
fn bytes_of_u32_slice(values: &[u32]) -> Result<Vec<u8>, BatchBackendError> {
    let mut out = Vec::new();
    try_reserve_bytes(&mut out, values.len().saturating_mul(4))?;
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    Ok(out)
}

fn bytes_of_f32_slice(values: &[f32]) -> Result<Vec<u8>, BatchBackendError> {
    let mut out = Vec::new();
    try_reserve_bytes(&mut out, values.len().saturating_mul(4))?;
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    Ok(out)
}

/// ステージング用バイト列・`f32` 列のフォールブル確保ヘルパー
/// （Cursor Bugbot / codex P1 指摘対応: `Vec::with_capacity` は確保失敗時に
/// プロセスを abort するため、常駐行列・チャンク単位の数 MiB 級コピーでは使わない。
/// 失敗は [`BatchBackendError::KernelLaunchFailed`] として返し、`FallbackBatchEngine`
/// の CPU 縮退（CORE-8）が働く経路に載せる）。
fn try_reserve_bytes(buf: &mut Vec<u8>, additional: usize) -> Result<(), BatchBackendError> {
    buf.try_reserve_exact(additional).map_err(|_| {
        BatchBackendError::KernelLaunchFailed("staging buffer allocation failed".to_string())
    })
}

fn try_reserve_f32(buf: &mut Vec<f32>, additional: usize) -> Result<(), BatchBackendError> {
    buf.try_reserve_exact(additional).map_err(|_| {
        BatchBackendError::TransferFailed("readback buffer allocation failed".to_string())
    })
}

/// [`try_reserve_f32`] の `u32` 版（[`u32_vec_from_ne_bytes`] が使う）。
fn try_reserve_u32(buf: &mut Vec<u32>, additional: usize) -> Result<(), BatchBackendError> {
    buf.try_reserve_exact(additional).map_err(|_| {
        BatchBackendError::TransferFailed("readback buffer allocation failed".to_string())
    })
}

/// GPU の readback バッファから `f32` 列を復元する（`from_ne_bytes`。
/// `bytemuck` 不採用）。長さが 4 の倍数でない場合は空を返す（呼び出し元が
/// バッファサイズを 4 の倍数で確保しているため通常到達しないが、fail-closed
/// に空扱いで打ち切る）。
fn f32_vec_from_ne_bytes(bytes: &[u8]) -> Result<Vec<f32>, BatchBackendError> {
    // `as_chunks::<4>()` は「ちょうど 4 バイトの配列列」と「端数」に分ける
    // （`chunks_exact` + `try_into` と違い添字・変換失敗の分岐が生じない）。
    let (quads, remainder) = bytes.as_chunks::<4>();
    if !remainder.is_empty() {
        // 呼び出し元はバッファサイズを 4 の倍数で確保しているため通常到達しない。
        // 端数がある＝readback が破損しているとみなし、部分結果を返さず空で打ち切る
        // （呼び出し元の件数一致チェックが `TransferFailed` として拒否する）。
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    try_reserve_f32(&mut out, quads.len())?;
    for quad in quads {
        out.push(f32::from_ne_bytes(*quad));
    }
    Ok(out)
}

impl GpuBatchBackend {
    /// [`BatchBackend::batch_search`] の実体。スコアバッファ予算
    /// （`budget_bytes`）を呼び出し元から受け取る内部共通経路にし、
    /// 既定の公開経路（trait 実装）は常に [`GPU_SCORE_BUFFER_BUDGET_BYTES`]
    /// を使う。テスト・ベンチ専用に小さい予算を注入して行チャンク分割を
    /// 強制する経路（[`Self::batch_search_with_row_budget_for_tests`]）と
    /// 実装を共有するための分離（Issue #532 codex-review P2 指摘対応:
    /// 端数を含む複数行チャンクを実 GPU dispatch 経由で検証できるようにする）。
    fn batch_search_with_budget(
        &self,
        queries: &[BatchQuery<'_>],
        budget_bytes: usize,
    ) -> Result<Vec<BatchHit>, BatchExecError> {
        self.batch_search_with_budget_and_mode(queries, budget_bytes, false, None)
    }

    /// [`Self::batch_search_with_budget`] へ「常に全量 readback 経路を使う」
    /// 強制フラグを加えた内部共通経路（Issue #536）。`force_full_readback`
    /// は Top-k パイプラインの可用性に関わらず [`GpuReadbackMode::FullScores`]
    /// を選ばせるテスト・ベンチ専用のオーバーライドで、実 GPU dispatch 経由の
    /// ビット同一検証（`batch_search_with_options_for_tests`）にのみ使う。
    /// `forced_dot_shader`（Issue #539）は S0 シェーダ選択（[`select_dot_shader`]）
    /// を上書きするテスト専用オーバーライド。`Some(GpuDotShaderKind::F16Arith)`
    /// が f16 算術版を使えない環境（未対応アダプタ・オーバーフローガード不成立）
    /// で指定された場合は黙って unpack 版へ縮退せず `Err` を返す（fail-closed。
    /// [`GpuSearchTestOptions::dot_shader`] 経由のみ到達する）。
    fn batch_search_with_budget_and_mode(
        &self,
        queries: &[BatchQuery<'_>],
        budget_bytes: usize,
        force_full_readback: bool,
        forced_dot_shader: Option<GpuDotShaderKind>,
    ) -> Result<Vec<BatchHit>, BatchExecError> {
        if self.device_lost.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(BatchBackendError::DeviceLost(
                "gpu device lost".to_string(),
            )));
        }
        // error scope 外で発生した wgpu エラーが記録されていれば、GPU 経路を
        // 信頼せず backend エラーとして返す（`FallbackBatchEngine` が CPU 縮退へ
        // 倒す。codex/Bugbot P1 指摘対応の一部）。
        if self.uncaptured_error.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(
                BatchBackendError::KernelLaunchFailed(
                    "gpu reported an uncaptured error".to_string(),
                ),
            ));
        }

        // `FallbackBatchEngine::batch_search` が本メソッド呼び出し前に
        // `validate_batch_queries` を適用する契約だが（`batch_fallback.rs`
        // の `BatchBackend` trait doc 参照）、本バックエンドは独自の走査
        // パイプラインを持つため（`run_batch_search` を経由しない）防御的に
        // 再検証する（TASK-128 設計方針 §3.2 ポインタ）。
        validate_batch_queries(self.matrix.dim(), queries).map_err(BatchExecError::Input)?;

        // dispatch 前の総量ガード（Issue #178 レビュー指摘対応: GPU 経路が
        // `queries.len()` を乗じていなかった DoS 増幅の修正と、その後の
        // codex/Bugbot P1 指摘対応: 全行 × 全クエリの直積で課金すると CPU 経路の
        // テナント別合算では予算内の要求まで `Input` エラー〔＝CPU 縮退しない〕で
        // 恒久的に拒否してしまうため、クエリごとの実到達行数で課金する）。
        check_reachable_batch_work(&self.matrix, queries).map_err(BatchExecError::Input)?;

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => {
                return Err(BatchExecError::Backend(BatchBackendError::InitFailed(
                    msg.clone(),
                )))
            }
        };

        // `GpuContext`（`wgpu::Device`）はプロセス共有の `OnceLock` であり、
        // `GpuBatchBackend` インスタンスは複数存在しうる。したがって直列化は
        // インスタンス単位ではなくプロセス単位で行う必要がある
        // （codex P1 指摘対応: インスタンス単位の Mutex では、別インスタンスが
        // 同じ Device へ並行に `push_error_scope`/`pop` を発行しうる。wgpu 30 の
        // error scope スタックは `std` feature 有効時 thread-local であるため
        // 本実装でスタックが交錯することは無いが、`device.poll` の駆動と
        // `map_async` コールバックの完了待ちが相互に影響しうるため、
        // 「共有 Device への dispatch は 1 つずつ」という不変条件を
        // プロセス単位の lock で明示する）。
        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dim = self.matrix.dim();
        let dim_half = dim.div_ceil(2);
        let query_stride = dim_half.saturating_mul(2);

        // S0 シェーダ選択（Issue #539）は `run_tiled_batch_search` が
        // `group_queries_by_ctx` で分けた `PolicyContext` グループごとに
        // 独立して行う（PR #591 レビュー P0 指摘対応）。ここでは選択に
        // 必要な材料（GPU コンテキスト・行バッファ・事前計算済み行振幅
        // [`Self::row_max_abs`]）を束ねるだけで、`row_max_abs`/`query_stats`
        // の実際の集約・`select_dot_shader` 呼び出しは
        // [`AdaptiveShaderSelector::resolve`] がグループ単位に行う。
        let strategy = TargetStrategy::Adaptive(AdaptiveShaderSelector {
            gpu_ctx: ctx,
            row_buffer: &self.row_buffer,
            bind_group_layout: self.bind_group_layout_ref(ctx),
            row_stride: dim_half as u32,
            row_max_abs: &self.row_max_abs,
            forced_dot_shader,
            stats: &self.stats,
        });

        run_tiled_batch_search(
            ctx,
            &self.matrix,
            queries,
            &strategy,
            query_stride,
            RunTiledBatchSearchOptions {
                budget_bytes,
                force_full_readback,
                stats: &self.stats,
            },
        )
    }

    /// **テスト・ベンチ専用**。[`Self::batch_search_with_budget`] へ任意の
    /// スコアバッファ予算を注入し、`GPU_SCORE_BUFFER_BUDGET_BYTES`（既定
    /// 32MiB）では通常のデバイス上で発生しない行チャンク分割（端数を含む
    /// 複数行チャンク）を実 GPU dispatch 経由で強制的に発生させる
    /// （Issue #532 codex-review P2 指摘対応。`plan_query_tile` の単体テストは
    /// 分割境界の算出だけを検証しており、実際の dispatch・readback・
    /// `TopKSelector` への累積までは通していなかった）。非既定 feature
    /// `bench-internals` でのみ公開する（`hybrid.rs::sparse_refetch_observed`
    /// と同パターン。既定ビルド・`wire-server` からは到達不能で、テナント
    /// 境界・RLS 迂回 API は一切露出しない——`budget_bytes` は dispatch を
    /// 何回に分けるかだけを左右する純粋な性能パラメータであり、可視性判定・
    /// スコア計算そのものには関与しない）。
    #[cfg(feature = "bench-internals")]
    pub fn batch_search_with_row_budget_for_tests(
        &self,
        queries: &[BatchQuery<'_>],
        budget_bytes: usize,
    ) -> Result<Vec<BatchHit>, BatchExecError> {
        self.batch_search_with_budget(queries, budget_bytes)
    }

    /// **テスト・ベンチ専用**（Issue #536）。[`GpuSearchTestOptions`] 経由で
    /// 「常に全量 readback 経路を使う」強制フラグを注入し、既定経路
    /// （workgroup 内部分 Top-k）と全量 readback 経路の結果が実 GPU dispatch
    /// 経由でビット同一であることを結合テストから検証できるようにする
    /// （`tests/gpu_batch.rs`）。`budget_bytes`・`force_full_readback` は
    /// いずれも dispatch の分割方式・readback 方式だけを左右する性能
    /// パラメータであり、可視性判定・スコア計算そのものには関与しない
    /// （[`batch_search_with_row_budget_for_tests`] と同じ露出方針）。
    #[cfg(feature = "bench-internals")]
    pub fn batch_search_with_options_for_tests(
        &self,
        queries: &[BatchQuery<'_>],
        options: GpuSearchTestOptions,
    ) -> Result<Vec<BatchHit>, BatchExecError> {
        self.batch_search_with_budget_and_mode(
            queries,
            options.budget_bytes,
            options.force_full_readback,
            options.dot_shader,
        )
    }
}

/// [`GpuBatchBackend::batch_search_with_options_for_tests`] へ渡すオプション
/// （`bench-internals` feature 限定。Issue #536・#539）。
#[cfg(feature = "bench-internals")]
#[derive(Debug, Clone, Copy)]
pub struct GpuSearchTestOptions {
    pub budget_bytes: usize,
    pub force_full_readback: bool,
    /// S0 シェーダ選択（[`select_dot_shader`]）の強制オーバーライド
    /// （Issue #539）。`None` は既定の自動選択。`Some(Unpack)` は常に
    /// unpack 版を強制する。`Some(F16Arith)` は f16 算術版が利用不能
    /// （未対応アダプタ・パイプライン生成失敗）またはオーバーフロー
    /// ガード不成立の場合 `Err(KernelLaunchFailed)` を返し、黙って
    /// unpack 版へ縮退しない（fail-closed 分岐の検証用）。
    pub dot_shader: Option<GpuDotShaderKind>,
}

impl BatchBackend for GpuBatchBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        self.batch_search_with_budget(queries, GPU_SCORE_BUFFER_BUDGET_BYTES)
    }
}

/// [`GpuBatchBackend::batch_search`]/[`GpuF32ContrastBackend::batch_search`]
/// が共有する dispatch 本体（Issue #532・R1）。クエリを [`group_queries_by_ctx`]
/// で `PolicyContext` 単位にグループ化し、グループごとに [`gather_reachable_rows`]
/// を 1 回だけ実行したうえで、[`plan_query_tile`] が決めた幅 Q でクエリを
/// タイル化し 1 dispatch へ束ねる。f16/f32 の差異は `query_stride` と
/// [`TargetStrategy`]（呼び出し元の常駐形式ごとに固定の `Fixed`、または
/// [`GpuBatchBackend`] のようにグループごとに S0 シェーダ選択をやり直す
/// `Adaptive`）のみに閉じ込め、タイル化・グループ化のロジック自体は
/// 両バックエンドで完全に共有する。
///
/// テナント境界（P0）: タイル内の全クエリが同一 `PolicyContext` であることは
/// `group_queries_by_ctx` の構成上保証されるため、1 回の `gather_reachable_rows`
/// 呼び出し結果をタイル内の全クエリで安全に共有できる（異なる可視性のクエリが
/// 同じ行集合を参照する経路は作らない）。選出後の解決は既存どおり
/// [`finalize_gpu_hits`]（`PolicyContext::is_visible` 単一照合パス）が
/// クエリごとに独立して再検証する。
/// [`run_tiled_batch_search`] へ渡す readback 方式関連の付随パラメータを
/// 束ねる（clippy `too_many_arguments` を避けるため。Issue #536）。
struct RunTiledBatchSearchOptions<'a> {
    budget_bytes: usize,
    /// テスト・ベンチ専用のオーバーライド（`GpuSearchTestOptions`）。常に
    /// `GpuReadbackMode::FullScores` を選ばせる。
    force_full_readback: bool,
    stats: &'a GpuBatchStats,
}

/// [`run_tiled_batch_search`] が `PolicyContext` グループごとに使う dispatch
/// target の決め方（Issue #539・PR #591 レビュー P0 指摘対応）。
///
/// - `Fixed`: [`GpuF32ContrastBackend`]（CORE-16 対照経路。f16 算術版の対象外
///   で常に f32 常駐固定）が使う。全グループで同一の target をそのまま使う
/// - `Adaptive`: [`GpuBatchBackend`] が使う。グループの可視行・クエリのみを
///   母数に [`select_dot_shader`] を呼び直し、シェーダ選択をグループ単位に
///   分離する（他 `PolicyContext` の不可視行・クエリが選択へ混ざらない）
enum TargetStrategy<'a> {
    Fixed(DotDispatchTarget<'a>),
    Adaptive(AdaptiveShaderSelector<'a>),
}

/// [`TargetStrategy::Adaptive`] が保持する、S0 シェーダ選択に必要な材料
/// （Issue #539・PR #591 レビュー P0・P2 指摘対応）。`row_buffer`・
/// `bind_group_layout`・`row_stride` は `GpuBatchBackend` の常駐形式
/// （f16 パック常駐）に固定の値で、シェーダ選択の結果（`pipeline`・
/// `topk_pipeline`・`query_encoding`）だけがグループごとに変わる。
struct AdaptiveShaderSelector<'a> {
    gpu_ctx: &'a GpuContext,
    row_buffer: &'a wgpu::Buffer,
    bind_group_layout: &'a wgpu::BindGroupLayout,
    row_stride: u32,
    /// [`GpuBatchBackend::row_max_abs`]（行ごとに事前計算済みの有限成分
    /// 絶対値最大）への参照。グループの可視行 index（`reachable`）に対する
    /// 単純な配列参照 + 最大値集約だけで済み、行データを再度 unpack しない
    /// （P2 指摘対応）。
    row_max_abs: &'a [f32],
    forced_dot_shader: Option<GpuDotShaderKind>,
    stats: &'a GpuBatchStats,
}

impl<'a> AdaptiveShaderSelector<'a> {
    /// 1 グループ（同一 `PolicyContext` のクエリ集合）向けに
    /// [`select_dot_shader`] を呼び、対応する [`DotDispatchTarget`] を返す
    /// （PR #591 レビュー P0 指摘対応: `row_max_abs`/`query_stats` の母数を
    /// このグループの可視行・クエリだけに限定する）。
    fn resolve(
        &self,
        queries: &[BatchQuery<'_>],
        group: &[usize],
        reachable: &[u32],
    ) -> Result<DotDispatchTarget<'a>, BatchExecError> {
        let f16_available = self.gpu_ctx.f16_arith_pipelines.is_some();
        let row_max_abs = max_abs_finite_from_precomputed_rows(self.row_max_abs, reachable);
        let query_stats = max_abs_finite_from_queries_subset(queries, group);
        let natural_shader_kind = select_dot_shader(
            f16_available,
            row_max_abs,
            query_stats.max_abs,
            query_stats.has_subnormal_underflow,
            query_stats.has_precision_loss,
        );
        let shader_kind = match self.forced_dot_shader {
            None => natural_shader_kind,
            Some(GpuDotShaderKind::Unpack) => GpuDotShaderKind::Unpack,
            Some(GpuDotShaderKind::F16Arith) => {
                // 強制指定は「f16 算術版が実際に選ばれる状況」でのみ受理する。
                // 未対応アダプタ・オーバーフローガード不成立のいずれでも
                // 黙って unpack 版へ縮退せず拒否する（fail-closed。テスト・
                // ベンチ専用オーバーライドの契約。§2.5）。
                if natural_shader_kind != GpuDotShaderKind::F16Arith {
                    return Err(BatchExecError::Backend(
                        BatchBackendError::KernelLaunchFailed(
                            "f16 arith dot shader forced but unavailable or overflow guard rejected it"
                                .to_string(),
                        ),
                    ));
                }
                GpuDotShaderKind::F16Arith
            }
        };
        let (pipeline, topk_pipeline, query_encoding) = match shader_kind {
            GpuDotShaderKind::F16Arith => match self.gpu_ctx.f16_arith_pipelines.as_ref() {
                Some(p) => (&p.dot, p.topk.as_ref(), QueryEncoding::F16Packed),
                None => (
                    &self.gpu_ctx.pipeline,
                    self.gpu_ctx.topk_pipeline.as_ref(),
                    QueryEncoding::F32,
                ),
            },
            GpuDotShaderKind::Unpack => (
                &self.gpu_ctx.pipeline,
                self.gpu_ctx.topk_pipeline.as_ref(),
                QueryEncoding::F32,
            ),
        };
        match (shader_kind, query_encoding) {
            (GpuDotShaderKind::F16Arith, QueryEncoding::F16Packed) => {
                self.stats
                    .f16_arith_dispatches
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ if f16_available && self.forced_dot_shader.is_none() => {
                // f16 算術版パイプラインは使えたが（`f16_available`）、
                // 自動選択（`forced_dot_shader` 非指定）でオーバーフロー
                // ガードが unpack 版を選んだ（Issue #539 のガード観測点。
                // テスト専用の強制 `Unpack` は数えない）。
                self.stats
                    .f16_arith_guard_fallbacks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
        Ok(DotDispatchTarget {
            pipeline,
            topk_pipeline,
            row_buffer: self.row_buffer,
            bind_group_layout: self.bind_group_layout,
            row_stride: self.row_stride,
            query_encoding,
        })
    }
}

fn run_tiled_batch_search(
    ctx: &GpuContext,
    matrix: &crate::batch_search::ResidentMatrix,
    queries: &[BatchQuery<'_>],
    strategy: &TargetStrategy<'_>,
    query_stride: usize,
    opts: RunTiledBatchSearchOptions<'_>,
) -> Result<Vec<BatchHit>, BatchExecError> {
    let RunTiledBatchSearchOptions {
        budget_bytes,
        force_full_readback,
        stats,
    } = opts;
    let groups = group_queries_by_ctx(queries);

    // 結果は入力順で復元する（`Vec<Option<_>>` → 全件 `Some` 検証で
    // fail-closed に欠落を検知する。§4.2 ポインタ）。
    let mut results: Vec<Option<BatchHit>> = Vec::new();
    try_reserve_exact(&mut results, queries.len(), "gpu tiled batch results")
        .map_err(BatchExecError::Input)?;
    results.resize_with(queries.len(), || None);

    for group in &groups {
        let Some(&first_idx) = group.first() else {
            continue;
        };
        let group_ctx = queries.get(first_idx).map(|q| q.ctx).ok_or_else(|| {
            BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                "query group index out of range".to_string(),
            ))
        })?;
        let reachable = gather_reachable_rows(matrix, group_ctx)
            .map_err(|e| BatchExecError::Input(e.into_batch_search_error()))?;

        // S0 シェーダ選択（Issue #539）。グループ（同一 `PolicyContext`）
        // 単位で解決する（PR #591 レビュー P0 指摘対応。`TargetStrategy` doc
        // 参照）。
        let target = match strategy {
            TargetStrategy::Fixed(t) => *t,
            TargetStrategy::Adaptive(selector) => selector.resolve(queries, group, &reachable)?,
        };
        let target = &target;

        let plan = plan_query_tile(group.len(), budget_bytes, ctx.max_workgroups_per_dimension);

        for tile in group.chunks(plan.width.max(1)) {
            let width = tile.len();

            // タイル内の各クエリを `query_stride` へパディングして連結する
            // （§4.2「行データを 1 回読みで償却」の前提: シェーダは同じ行
            // データをタイル幅ぶんのクエリで再利用するため、クエリ側は
            // 固定ストライドで並んでいる必要がある）。
            let mut queries_concat: Vec<f32> = Vec::new();
            try_reserve_f32(&mut queries_concat, width.saturating_mul(query_stride))
                .map_err(BatchExecError::Backend)?;
            for &qi in tile {
                let vector = queries.get(qi).map(|q| q.vector).ok_or_else(|| {
                    BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                        "query tile index out of range".to_string(),
                    ))
                })?;
                let before = queries_concat.len();
                queries_concat.extend_from_slice(vector);
                let padded_len = before.saturating_add(query_stride);
                queries_concat.resize(padded_len.max(queries_concat.len()), 0.0);
            }

            // クエリごとに独立した選出器（`Option` で保持し、確定後に
            // `take` で 1 度だけ取り出す。`TopKSelector::into_sorted_vec`
            // が `self` を消費するため）。
            let mut selectors: Vec<Option<TopKSelector>> = Vec::new();
            try_reserve_exact(&mut selectors, width, "gpu tile selectors")
                .map_err(BatchExecError::Input)?;
            for &qi in tile {
                let k = queries.get(qi).map(|q| q.k).unwrap_or(0);
                selectors.push(Some(TopKSelector::new(k)));
            }

            // タイル内クエリの `k` の最大値で readback 方式を決める
            // （ADR 決定 2。`select_readback_mode` は GPU デバイス非依存の
            // 純関数で単体テスト対象）。`force_full_readback` はテスト・
            // ベンチ専用のオーバーライドで、Top-k パイプラインが利用可能でも
            // 常に全量 readback を選ばせる（実 GPU dispatch 経由のビット
            // 同一検証に使う）。
            let tile_max_k = tile
                .iter()
                .filter_map(|&qi| queries.get(qi).map(|q| q.k))
                .max()
                .unwrap_or(0);
            let mode = if force_full_readback {
                GpuReadbackMode::FullScores
            } else {
                select_readback_mode(target.topk_pipeline.is_some(), tile_max_k)
            };
            if target.topk_pipeline.is_some() && mode == GpuReadbackMode::FullScores {
                stats
                    .full_readback_fallbacks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }

            match mode {
                GpuReadbackMode::FullScores => {
                    for chunk in reachable.chunks(plan.chunk_rows.max(1)) {
                        let scores = dispatch_dot_products(
                            ctx,
                            target,
                            chunk,
                            &queries_concat,
                            width,
                            query_stride,
                        )
                        .map_err(BatchExecError::Backend)?;

                        let expected_len = chunk.len().saturating_mul(width);
                        if scores.len() != expected_len {
                            return Err(BatchExecError::Backend(
                                BatchBackendError::TransferFailed(
                                    "readback length mismatch".to_string(),
                                ),
                            ));
                        }
                        stats
                            .full_readback_dispatches
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        stats.readback_bytes.fetch_add(
                            (scores.len() as u64).saturating_mul(4),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        for (qpos, selector_slot) in selectors.iter_mut().enumerate() {
                            let Some(selector) = selector_slot.as_mut() else {
                                continue;
                            };
                            let base = qpos.saturating_mul(chunk.len());
                            for (offset, &row_idx) in chunk.iter().enumerate() {
                                let Some(&score) = scores.get(base + offset) else {
                                    continue;
                                };
                                if !score.is_finite() {
                                    continue;
                                }
                                // 候補識別子は「行 id」ではなく常駐行列のスロット
                                // 番号（`gather_reachable_rows` が返す行 index）
                                // を使う。`TopKSelector` の同点タイブレークは
                                // 候補識別子の昇順であり、CPU 経路
                                // （`batch_search.rs::run_batch_search`）はスロット
                                // 昇順を契約としているため（`batch_fallback.rs::
                                // revalidate_primary_hits` の順序検証 (4) が同じ
                                // 基準で判定する）、ここで行 id を使うと同点時に
                                // 順序契約違反となり正当な結果まで
                                // `PrimaryResultRejected` で拒否される（PR #205/
                                // #228 の `(tenant_id, id)` 統一に追随。Issue #178）。
                                selector.push(CandidateHit {
                                    id: u64::from(row_idx),
                                    score,
                                });
                            }
                        }
                    }
                }
                GpuReadbackMode::PartialTopK { k_out } => {
                    let chunk_rows = plan_partial_topk_chunk_rows(
                        width,
                        k_out,
                        budget_bytes,
                        ctx.max_workgroups_per_dimension,
                    )
                    .max(1);
                    for chunk in reachable.chunks(chunk_rows) {
                        let num_workgroups =
                            chunk.len().div_ceil(GPU_WORKGROUP_SIZE as usize).max(1);
                        let readback = dispatch_partial_topk(
                            ctx,
                            target,
                            chunk,
                            &queries_concat,
                            width,
                            query_stride,
                            k_out,
                        )
                        .map_err(BatchExecError::Backend)?;
                        stats
                            .partial_topk_dispatches
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        stats.readback_bytes.fetch_add(
                            (readback.len() as u64).saturating_mul(4),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        merge_partial_topk_readback(
                            &readback,
                            chunk,
                            width,
                            num_workgroups,
                            k_out,
                            &mut selectors,
                        )
                        .map_err(BatchExecError::Backend)?;
                    }
                }
            }

            for (qpos, &qi) in tile.iter().enumerate() {
                let Some(selector) = selectors.get_mut(qpos).and_then(Option::take) else {
                    return Err(BatchExecError::Backend(
                        BatchBackendError::KernelLaunchFailed(
                            "query tile selector missing".to_string(),
                        ),
                    ));
                };
                let q_ctx = queries.get(qi).map(|q| q.ctx).ok_or_else(|| {
                    BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                        "query tile index out of range".to_string(),
                    ))
                })?;
                let hit = BatchHit {
                    hits: finalize_gpu_hits(matrix, q_ctx, &selector.into_sorted_vec())
                        .map_err(BatchExecError::Input)?,
                };
                if let Some(slot) = results.get_mut(qi) {
                    *slot = Some(hit);
                }
            }
        }
    }

    // 入力順で全クエリ分の結果が揃っていることを検証する（fail-closed:
    // グループ化・タイル化の実装バグで一部クエリが取りこぼされた場合、
    // 部分結果を返さず backend エラーとして CPU 縮退〔CORE-8〕へ倒す）。
    let mut hits: Vec<BatchHit> = Vec::new();
    try_reserve_exact(&mut hits, results.len(), "gpu tiled batch results (final)")
        .map_err(BatchExecError::Input)?;
    for slot in results {
        match slot {
            Some(hit) => hits.push(hit),
            None => {
                return Err(BatchExecError::Backend(
                    BatchBackendError::KernelLaunchFailed(
                        "query result missing after tiled dispatch".to_string(),
                    ),
                ))
            }
        }
    }

    Ok(hits)
}

/// GPU 側で選出した候補（常駐行列のスロット番号 + スコア）を、テナント修飾済みの
/// [`SearchHit`]（`(tenant_id, id)` で行を一意に解決できる契約。対象ビヘイビア:
/// TABLE-12・RLS-9。PR #205/#228）へ解決する。
///
/// [`GpuBatchBackend::batch_search`] の最終段だが、GPU デバイスに触れないため
/// GPU 非搭載環境（CI）でも単体テストできるよう独立関数として切り出している
/// （`tests` モジュール参照。Issue #178 レビュー指摘対応）。
///
/// 解決は CPU 経路（`batch_search.rs::run_batch_search` の「選出後の独立再検証」）
/// と同一の `resolve_batch_slot` + `PolicyContext::is_visible`（CORE-2 の単一照合
/// パス）を通す。スロットが解決不能（GPU 側の readback 破損・実装バグ）、または
/// 解決した行が当該クエリから不可視の場合は、部分結果を返さず
/// [`BatchSearchError::TenantMaskViolation`] で全体を拒否する（fail-closed）。
fn finalize_gpu_hits(
    matrix: &crate::batch_search::ResidentMatrix,
    ctx: &PolicyContext,
    candidates: &[CandidateHit],
) -> Result<Vec<SearchHit>, BatchSearchError> {
    let mut out: Vec<SearchHit> = Vec::new();
    try_reserve_exact(&mut out, candidates.len(), "gpu resolved hits")?;
    for hit in candidates {
        let Some((tenant, id, visibility)) = resolve_batch_slot(matrix, hit.id) else {
            return Err(BatchSearchError::TenantMaskViolation);
        };
        if !ctx.is_visible(tenant, visibility) {
            return Err(BatchSearchError::TenantMaskViolation);
        }
        out.push(SearchHit {
            tenant_id: try_owned_str(tenant)?,
            id,
            score: hit.score,
        });
    }
    Ok(out)
}

impl GpuBatchBackend {
    /// `GpuContext` から bind group layout の参照を取り出す（`&self` から
    /// `ctx` を経由するだけの薄いヘルパー。`dispatch_dot_products` の引数を
    /// 揃えるために存在する）。
    fn bind_group_layout_ref<'a>(&self, ctx: &'a GpuContext) -> &'a wgpu::BindGroupLayout {
        &ctx.bind_group_layout
    }
}

/// CORE-16（GPU 常駐コピーの f16 パック vs f32 常駐の A/B 対照経路と受け入れ判定。
/// Issue #234・ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-16）の
/// **対照（bench/テスト専用）** バックエンド。[`GpuBatchBackend`] が保持する
/// f16 2 要素/u32 パック常駐に対し、本バックエンドは元の f32 ベクトル列を
/// そのまま GPU の STORAGE バッファへ常駐させ、`unpack2x16float` を経由しない
/// f32 精度の内積を計算する。
///
/// `crate::batch_fallback::FallbackBatchEngine::build_with_gpu` の primary
/// backend 選択には接続しない（CORE-12「経路を外部から上書きする機構を
/// 設けない」と整合。本番の dispatch 経路選択は変えず、`benches/batch_bench.rs`
/// の CORE-16 ゲートおよび `tests/gpu_batch.rs` の結合テストからのみ構築される。
/// `crate::batch_fallback::GpuReferenceBackend` を pub で残している既存前例に倣う）。
pub struct GpuF32ContrastBackend {
    matrix: crate::batch_search::ResidentMatrix,
    row_buffer: wgpu::Buffer,
    /// [`GpuBatchBackend::device_lost`] と同じ役割（`GpuContext::device_lost`
    /// への参照）。
    device_lost: std::sync::Arc<AtomicBool>,
    /// [`GpuBatchBackend::uncaptured_error`] と同じ役割。
    uncaptured_error: std::sync::Arc<AtomicBool>,
    /// [`GpuBatchBackend::stats`] と同じ役割。
    stats: std::sync::Arc<GpuBatchStats>,
}

impl GpuF32ContrastBackend {
    /// 元データ（`ResidentMatrix::build` と同じ引数形）から f32 常駐対照
    /// バックエンドを構築する。`ResidentMatrix` はテナント境界判定・スロット
    /// 解決（`gather_reachable_rows`/`finalize_gpu_hits`/
    /// `check_reachable_batch_work` の入力）のために内部でも構築するが、GPU
    /// バッファへは常駐行列の f16 パック（`packed()`）ではなく、引数で渡された
    /// **元の f32 ベクトル列**をそのままアップロードする。
    pub fn try_new(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[crate::storage::Visibility],
        dim: usize,
        vectors: &[f32],
    ) -> Result<Self, BatchBackendError> {
        let matrix =
            crate::batch_search::ResidentMatrix::build(ids, tenant_ids, visibilities, dim, vectors)
                .map_err(|e| {
                    BatchBackendError::InitFailed(format!("resident matrix build failed: {e}"))
                })?;

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => return Err(BatchBackendError::InitFailed(msg.clone())),
        };
        // f32 対照経路専用パイプラインもここで先に確保できることを確認する
        // （`try_new` 時点で `InitFailed` にできる失敗は早期に返す。
        // `GpuBatchBackend::try_new` が本番パイプラインを `init_gpu_context` で
        // 前もって確認しているのと同じ方針）。
        f32_contrast_pipeline().map_err(BatchBackendError::InitFailed)?;

        let vectors_bytes = vectors
            .len()
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| {
                BatchBackendError::InitFailed("f32 vector byte size overflow".to_string())
            })?;
        if vectors_bytes as u64 > ctx.max_storage_buffer_binding_size {
            return Err(BatchBackendError::InitFailed(
                "f32-resident vectors exceed adapter storage buffer limit".to_string(),
            ));
        }
        if vectors_bytes == 0 {
            return Err(BatchBackendError::InitFailed(
                "f32-resident vectors are empty".to_string(),
            ));
        }

        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let vectors_staging = bytes_of_f32_slice(vectors)?;
        let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let row_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("f32 contrast resident vectors"),
            size: vectors_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&row_buffer, 0, &vectors_staging);
        let poll_failed = || {
            BatchBackendError::InitFailed(
                "device poll failed or timed out during f32-resident buffer upload".to_string(),
            )
        };
        if block_on_with_device_poll(&ctx.device, oom_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu out of memory while uploading the f32-resident vectors".to_string(),
            ));
        }
        if block_on_with_device_poll(&ctx.device, validation_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu validation error while uploading the f32-resident vectors".to_string(),
            ));
        }

        Ok(Self {
            matrix,
            row_buffer,
            device_lost: ctx.device_lost.clone(),
            uncaptured_error: ctx.uncaptured_error.clone(),
            stats: std::sync::Arc::new(GpuBatchStats::default()),
        })
    }

    /// [`GpuBatchBackend::stats`] と同じ役割（Issue #536）。
    pub fn stats(&self) -> GpuBatchStatsSnapshot {
        self.stats.snapshot()
    }
}

impl GpuF32ContrastBackend {
    /// [`GpuBatchBackend::batch_search_with_budget_and_mode`] の f32 対照
    /// 経路版（Issue #536・PR #578 codex 指摘対応）。`batch_search`（trait
    /// 実装。既定 budget・既定の readback 方式選択）と
    /// [`Self::batch_search_with_options_for_tests`]（テスト・ベンチ専用に
    /// budget・`force_full_readback` を注入する経路）の両方から呼ばれる
    /// 内部共通経路にすることで、既定経路と強制全量 readback 経路が実 GPU
    /// dispatch を通じて完全に同一の可視性判定・スコア計算パスを通ることを
    /// 保証する（[`GpuBatchBackend`] と同じ方針）。
    fn batch_search_with_budget_and_mode(
        &self,
        queries: &[BatchQuery<'_>],
        budget_bytes: usize,
        force_full_readback: bool,
    ) -> Result<Vec<BatchHit>, BatchExecError> {
        if self.device_lost.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(BatchBackendError::DeviceLost(
                "gpu device lost".to_string(),
            )));
        }
        if self.uncaptured_error.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(
                BatchBackendError::KernelLaunchFailed(
                    "gpu reported an uncaptured error".to_string(),
                ),
            ));
        }

        validate_batch_queries(self.matrix.dim(), queries).map_err(BatchExecError::Input)?;
        check_reachable_batch_work(&self.matrix, queries).map_err(BatchExecError::Input)?;

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => {
                return Err(BatchExecError::Backend(BatchBackendError::InitFailed(
                    msg.clone(),
                )))
            }
        };
        let pipelines = f32_contrast_pipeline()
            .map_err(|msg| BatchExecError::Backend(BatchBackendError::InitFailed(msg)))?;

        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dim = self.matrix.dim();
        // f32 常駐は行データを `array<f32>` としてそのまま持つため、パディング
        // 無しで `row_stride == dim`・`query_stride == dim`（f16 経路のような
        // 偶数丸めは不要）。
        let row_stride = dim as u32;
        let query_stride = dim;

        let target = DotDispatchTarget {
            pipeline: &pipelines.dot,
            topk_pipeline: pipelines.topk.as_ref(),
            row_buffer: &self.row_buffer,
            bind_group_layout: &ctx.bind_group_layout,
            row_stride,
            // CORE-16 対照経路は Issue #539（f16 算術版）の対象外。常に f32
            // 常駐のまま比較する契約を保つため `QueryEncoding::F32` 固定。
            query_encoding: QueryEncoding::F32,
        };
        let strategy = TargetStrategy::Fixed(target);

        run_tiled_batch_search(
            ctx,
            &self.matrix,
            queries,
            &strategy,
            query_stride,
            RunTiledBatchSearchOptions {
                budget_bytes,
                force_full_readback,
                stats: &self.stats,
            },
        )
    }

    /// **テスト・ベンチ専用**（Issue #536・PR #578 codex 指摘対応）。
    /// [`GpuBatchBackend::batch_search_with_options_for_tests`] の f32
    /// 対照経路版で、「常に全量 readback 経路を使う」強制フラグを注入し、
    /// f32 対照経路でも既定経路と強制全量 readback 経路の結果が実 GPU
    /// dispatch 経由でビット同一であることを結合テストから検証できる
    /// ようにする（`tests/gpu_batch.rs`）。
    #[cfg(feature = "bench-internals")]
    pub fn batch_search_with_options_for_tests(
        &self,
        queries: &[BatchQuery<'_>],
        options: GpuSearchTestOptions,
    ) -> Result<Vec<BatchHit>, BatchExecError> {
        self.batch_search_with_budget_and_mode(
            queries,
            options.budget_bytes,
            options.force_full_readback,
        )
    }
}

impl BatchBackend for GpuF32ContrastBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        self.batch_search_with_budget_and_mode(queries, GPU_SCORE_BUFFER_BUDGET_BYTES, false)
    }
}

/// [`GpuBatchBackend::batch_search`]/[`GpuF32ContrastBackend::batch_search`]
/// が 1 dispatch へタイル化するクエリ本数（`width`）と、そのタイルで 1 回の
/// dispatch に含める行チャンク行数（`chunk_rows`）の組（Issue #532・R3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueryTilePlan {
    width: usize,
    chunk_rows: usize,
}

/// [`QueryTilePlan`] を fail-closed に決める純関数（GPU デバイス非依存。
/// `checked_*`/`saturating_*` のみで導出し、`group_len == 0` 以外は必ず
/// `width >= 1`・`chunk_rows >= 1` を返す）。
///
/// `width` は「このグループのクエリ本数」と [`GPU_QUERY_TILE_MAX`] の小さい方。
/// `chunk_rows` は 1 回の dispatch のスコア + 行 index バッファ合計が
/// `budget_bytes`（[`GPU_SCORE_BUFFER_BUDGET_BYTES`] ポインタ）に収まり、かつ
/// dispatch のワークグループ数が adapter の
/// `max_workgroups_per_dimension` 内に収まるように決める（`width == 1` の
/// 場合、旧 `gpu_chunk_row_capacity` と同一の値になることを単体テストで固定）。
fn plan_query_tile(
    group_len: usize,
    budget_bytes: usize,
    max_workgroups_per_dimension: u32,
) -> QueryTilePlan {
    let width = group_len.clamp(1, GPU_QUERY_TILE_MAX);
    // 1 行あたりのバイト数: scores（`width` クエリ分の f32）+ row_ids（u32 1 個）。
    let per_row_bytes = (width as u64).saturating_mul(4).saturating_add(4).max(1);
    let by_budget = usize::try_from(budget_bytes as u64 / per_row_bytes).unwrap_or(usize::MAX);
    let by_workgroups =
        (max_workgroups_per_dimension as usize).saturating_mul(GPU_WORKGROUP_SIZE as usize);
    let chunk_rows = by_budget.min(by_workgroups).max(1);
    QueryTilePlan { width, chunk_rows }
}

/// クエリ列を `PolicyContext` の等価性（CORE-2 の単一照合パスが参照する
/// `tenant_id`／`visibilities` の組。`policy.rs::PolicyContext` は
/// `PartialEq`/`Eq` を derive 済み）でグループ化し、入力順を保った index 列を
/// 返す（Issue #532・R1: タイル内の全クエリが同一可視性集合を共有することを
/// 構造的に保証し、`gather_reachable_rows` をグループ単位で 1 回だけ実行する
/// ための下ごしらえ）。件数は [`crate::batch_search::MAX_BATCH_QUERIES`]
/// （4,096）以下であることが呼び出し元（`validate_batch_queries`）で
/// 保証されるため、O(グループ数 × クエリ数) の線形走査で十分。
fn group_queries_by_ctx(queries: &[BatchQuery<'_>]) -> Vec<Vec<usize>> {
    let mut groups: Vec<(&PolicyContext, Vec<usize>)> = Vec::new();
    for (idx, q) in queries.iter().enumerate() {
        match groups.iter_mut().find(|(ctx, _)| *ctx == q.ctx) {
            Some((_, members)) => members.push(idx),
            None => groups.push((q.ctx, vec![idx])),
        }
    }
    groups.into_iter().map(|(_, members)| members).collect()
}

/// [`GpuBatchBackend::batch_search`] の dispatch 前総量ガード本体。
///
/// CPU 経路（`batch_search.rs::run_batch_search`）は「テナントごとの行数 ×
/// そのテナントのクエリ数 × dim」を合算して [`MAX_BATCH_WORK`] と照合する。
/// GPU 経路も同じ基準に揃えるため、クエリごとに実際に走査する行
/// （`PolicyContext::is_visible` を満たす行）の数だけを課金する
/// （codex/Bugbot P1 指摘対応: 以前は「常駐行列の全行数 × 全クエリ数 × dim」の
/// 直積で課金していたため、複数テナントが混在すると CPU 経路では予算内の要求まで
/// 超過扱いになり、しかも超過は `BatchExecError::Input` として
/// `FallbackBatchEngine` の CPU 縮退対象外＝恒久的な失敗になっていた）。
///
/// 事前走査のコストは `rows × queries` 回の可視性判定のみ（dim を乗じない）で、
/// CPU 経路が本走査で行う判定回数と同じオーダーに収まる。
fn check_reachable_batch_work(
    matrix: &crate::batch_search::ResidentMatrix,
    queries: &[BatchQuery<'_>],
) -> Result<(), crate::batch_search::BatchSearchError> {
    let mut counts: Vec<usize> = Vec::new();
    try_reserve_exact(&mut counts, queries.len(), "gpu reachable row counts")?;
    for q in queries {
        let visible = matrix
            .tenant_ids()
            .iter()
            .zip(matrix.visibilities().iter())
            .filter(|(tenant, visibility)| q.ctx.is_visible(tenant, **visibility))
            .count();
        counts.push(visible);
    }
    check_batch_work_from_visible_counts(&counts, matrix.dim())
}

/// [`check_reachable_batch_work`] の判定本体（GPU デバイス非依存。
/// 境界値をハードウェアなしでテストできるよう独立関数として切り出す）。
/// 各要素は 1 クエリが実際に走査する行数で、`Σ(rows_q × dim)` を
/// [`MAX_BATCH_WORK`] と照合する。オーバーフローは超過として扱う。
fn check_batch_work_from_visible_counts(
    visible_rows_per_query: &[usize],
    dim: usize,
) -> Result<(), crate::batch_search::BatchSearchError> {
    let mut total: usize = 0;
    for rows in visible_rows_per_query {
        let work = compute_tenant_work(*rows, 1, dim)?;
        total = total.checked_add(work).ok_or(
            crate::batch_search::BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            },
        )?;
    }
    if total > MAX_BATCH_WORK {
        return Err(crate::batch_search::BatchSearchError::WorkBudgetExceeded {
            work: total,
            max: MAX_BATCH_WORK,
        });
    }
    Ok(())
}

/// クエリ `ctx` から見て到達可能な行の index 列を求める（CORE-2 の単一照合
/// パス `PolicyContext::is_visible` を使う。テナント文字列の独自比較はしない）。
///
/// 計算量ガードの主防御線は呼び出し元 [`GpuBatchBackend::batch_search`] 冒頭の
/// [`check_reachable_batch_work`]（クエリごとの実到達行数を合算した総量）である。
/// 本関数は計算量ガードを持たない。以前は常駐行列の全 `row_count × dim` を
/// 課金する後段チェックを置いていたが、(a) 他テナント行を多く含む行列では当該
/// クエリの可視行が少なくても `WorkBudgetExceeded` になり、`BatchExecError::Input`
/// のため CPU 縮退もせず有効な要求を恒久的に拒否する（codex P1 指摘）、
/// (b) そもそも `MAX_BATCH_ROWS × MAX_BATCH_DIM`（8,192,000,000）は
/// [`MAX_BATCH_WORK`]（10,000,000,000）未満なので、1 クエリ分の課金がこの上限を
/// 超えることは構造的にありえず到達不能なコードだった（Cursor Bugbot 指摘:
/// この形のチェックは回帰を検知できない）、の 2 点により削除した。
/// 総量ガードは主防御線 [`check_reachable_batch_work`]（クエリ件数をまたいで
/// 合算するため実際に到達しうる）が単独で担う。
fn gather_reachable_rows(
    matrix: &crate::batch_search::ResidentMatrix,
    ctx: &PolicyContext,
) -> Result<Vec<u32>, GpuInputError> {
    let mut out: Vec<u32> = Vec::new();
    // 確保量は全行数を上限とする（行数自体は `ResidentMatrix::build` が
    // `MAX_BATCH_ROWS` 以下に制限済み。ここでの `try_reserve` は
    // abort-on-OOM を避けるためのフォールブル確保）。
    out.try_reserve(matrix.row_count())
        .map_err(|_| GpuInputError::CapacityExceeded)?;
    for (idx, (tenant, visibility)) in matrix
        .tenant_ids()
        .iter()
        .zip(matrix.visibilities().iter())
        .enumerate()
    {
        if ctx.is_visible(tenant, *visibility) {
            // `idx` は `ResidentMatrix::build` が `MAX_BATCH_ROWS`
            // （1,000,000）以下に制限済みのため `u32` で表現できる。
            if let Ok(idx_u32) = u32::try_from(idx) {
                out.push(idx_u32);
            }
        }
    }

    Ok(out)
}

/// 1 つの `PolicyContext` グループ（[`group_queries_by_ctx`] が返す 1 要素）
/// が可視な常駐行だけを母数に、行ごとに事前計算済みの有限成分絶対値最大
/// （[`GpuBatchBackend::row_max_abs`]）を集約する（PR #591 レビュー P0・P2
/// 指摘対応）。
///
/// P0: `reachable`（呼び出し元がグループごとに 1 回だけ求めた
/// [`gather_reachable_rows`] の結果）はそのグループの `PolicyContext` から
/// 可視な行のみを含む。以前は全グループの可視行集合の和を単一の
/// `max_abs` へ縮約していたため、複数 `PolicyContext` が混在するバッチでは
/// あるテナントから不可視な行の振幅が、その行を含まないグループの
/// シェーダ選択・返却スコアの数値精度にまで波及していた（グループ間の情報
/// 干渉）。本関数はグループ単体の可視行だけを走査するため、シェーダ選択は
/// 呼び出し元がグループごとに独立して行える。
///
/// P2: 各行の絶対値最大は行自身の内容にのみ依存する定数のため、
/// `try_new` で 1 回だけ計算済みの `row_max_abs` から単純な配列参照 +
/// 最大値集約を行うだけで済み、`batch_search` 呼び出しのたびに行データを
/// unpack し直す必要が無い。
fn max_abs_finite_from_precomputed_rows(row_max_abs: &[f32], reachable: &[u32]) -> f32 {
    let mut max_abs: f32 = 0.0;
    for &row_idx in reachable {
        if let Some(&v) = row_max_abs.get(row_idx as usize) {
            max_abs = max_abs.max(v);
        }
    }
    max_abs
}

/// [`dispatch_dot_products`] へ渡す「呼び出し元の常駐形式ごとに固定の値」を
/// 束ねる（clippy `too_many_arguments` を避けつつ、f16 パック常駐 /
/// f32 常駐（Issue #234・[`GpuF32ContrastBackend`]）で異なるパイプライン・
/// 行バッファ・行ストライドを 1 つの呼び出しで渡せるようにする）。
/// `bind_group_layout` は両常駐形式で共用（[`DOT_SHADER_F32_WGSL`] のドキュメン
/// テーションコメント参照）だが、`pipeline`・`row_buffer`・`row_stride` は
/// 異なる。
#[derive(Clone, Copy)]
struct DotDispatchTarget<'a> {
    pipeline: &'a wgpu::ComputePipeline,
    /// workgroup 内部分 Top-k パイプライン（Issue #536）。`None` は
    /// [`select_readback_mode`] が常に `FullScores` を選ぶことで表現される
    /// （ADR 決定 2 の段階的 fail-closed 縮退）。
    topk_pipeline: Option<&'a wgpu::ComputePipeline>,
    row_buffer: &'a wgpu::Buffer,
    bind_group_layout: &'a wgpu::BindGroupLayout,
    /// `row_buffer` の 1 行あたりの要素数（f16: `dim.div_ceil(2)` 個の u32
    /// パック要素、f32: `dim` 個の f32 要素）。
    row_stride: u32,
    /// クエリバッファのホスト側エンコーディング（Issue #539）。`F16Packed`
    /// は `pipeline`/`topk_pipeline` が f16 算術版であることを前提にした
    /// 呼び出し元契約で、`dispatch_dot_products`/`dispatch_partial_topk` は
    /// この値に従って `queries_concat`（常に f32 論理値）を
    /// [`encode_query_bytes`] でバイト列化する。
    query_encoding: QueryEncoding,
}

/// 1 クエリ × `row_indices` 分の内積を GPU で計算し、readback した `f32` 列を返す。
///
/// `target` は呼び出し元の常駐形式（f16 パック / f32 常駐）ごとに異なる値を
/// 渡す共通実装（Issue #234。CORE-16 対照経路 [`GpuF32ContrastBackend`] を
/// 追加するにあたり、本番経路 [`GpuBatchBackend::batch_search`] と dispatch
/// 本体をここへ共通化した。error scope・deadline 付きポーリング・readback
/// 検証の防御を単一実装に保つ）。`query_stride` は `query` バッファのパディング後
/// の要素数（f16 経路は偶数丸め、f32 経路はパディング不要のため `dim` そのもの）。
fn dispatch_dot_products(
    ctx: &GpuContext,
    target: &DotDispatchTarget<'_>,
    row_indices: &[u32],
    queries_concat: &[f32],
    query_count: usize,
    query_stride: usize,
) -> Result<Vec<f32>, BatchBackendError> {
    if row_indices.is_empty() || query_count == 0 {
        return Ok(Vec::new());
    }
    // ホスト側の呼び出し規約違反（シェーダの `array<f32, QUERY_TILE_MAX>` を
    // 超えるクエリ本数）は fail-closed に拒否する。シェーダ側も `min` で
    // クランプするが、ここで弾くことでレジスタ配列の範囲外アクセスに
    // 依存しない二重の防御にする（Issue #532・R3）。
    if query_count > GPU_QUERY_TILE_MAX {
        return Err(BatchBackendError::KernelLaunchFailed(
            "query tile width exceeds GPU_QUERY_TILE_MAX".to_string(),
        ));
    }
    // `queries_concat` は呼び出し元（`run_tiled_batch_search`）が各クエリを
    // `query_stride` へパディング済みで連結したバッファである契約
    // （長さ不整合は呼び出し元の実装バグを示すため、GPU に触れる前に拒否する）。
    let expected_query_len = query_count.checked_mul(query_stride).ok_or_else(|| {
        BatchBackendError::KernelLaunchFailed("query buffer size overflow".to_string())
    })?;
    if queries_concat.len() != expected_query_len {
        return Err(BatchBackendError::TransferFailed(
            "query buffer length does not match query_count * query_stride".to_string(),
        ));
    }
    let row_count = row_indices.len() as u32;
    let query_count_u32 = query_count as u32;

    let params = GpuParams {
        row_stride: target.row_stride,
        row_count,
        query_count: query_count_u32,
        query_stride: query_stride as u32,
    };

    // ステージング用バイト列（ホスト側の確保）は error scope を push する**前**に
    // すべて用意する（codex P1 指摘対応: scope の内側で `?` により早期 return すると
    // pop が明示的に実行されない。`ErrorScopeGuard::drop` が自動 pop するため
    // スタックが壊れることはないが、「push した scope はこの関数内で必ず明示的に
    // pop する」という読み手に分かりやすい不変条件を保つ。GPU に触れない確保処理を
    // scope の内側へ入れる必要はそもそもない）。
    let params_bytes = params.to_ne_bytes_vec()?;
    let row_ids_bytes = bytes_of_u32_slice(row_indices)?;
    let query_bytes = encode_query_bytes(queries_concat, target.query_encoding)?;

    // バッファ・bind group の生成もすべて error scope の内側で行う
    // （codex/Bugbot P1 指摘対応: 以前は encoder 直前で push していたため、
    // 生成失敗が scope 外の uncaptured error になり panic しえた）。
    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let params_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&params_buffer, 0, &params_bytes);

    let row_ids_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product row ids"),
        size: row_ids_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&row_ids_buffer, 0, &row_ids_bytes);

    let query_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product query"),
        size: query_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&query_buffer, 0, &query_bytes);

    // スコアバッファは「行 × クエリタイル幅」（`scores[q * row_count + i]`
    // レイアウト。`DOT_SHADER_WGSL`/`DOT_SHADER_F32_WGSL` doc 参照）。
    let scores_bytes = (row_indices.len() as u64)
        .saturating_mul(query_count as u64)
        .saturating_mul(4);
    let scores_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product scores"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product readback"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("batch dot product bind group"),
        layout: target.bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: target.row_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: row_ids_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: query_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: scores_buffer.as_entire_binding(),
            },
        ],
    });

    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("batch dot product encoder"),
        });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("batch dot product pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(target.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        let workgroups = (row_count as u64).div_ceil(GPU_WORKGROUP_SIZE as u64);
        let workgroups_x = u32::try_from(workgroups).unwrap_or(u32::MAX);
        pass.dispatch_workgroups(workgroups_x, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&scores_buffer, 0, &readback_buffer, 0, scores_bytes);
    ctx.queue.submit(std::iter::once(encoder.finish()));

    // LIFO で pop する（後に push した OutOfMemory スコープを先に pop する）。
    // `pop()` の future はデバイスがポーリングされるまで `Pending` のままに
    // なりうるため、待機中も `device.poll` を駆動する（codex/Bugbot P1 指摘対応:
    // 自己ポーリングのみだと後続の readback ポーリングへ到達できずハングする）。
    let poll_failed =
        || BatchBackendError::DeviceLost("device poll failed or timed out".to_string());
    let oom_err =
        block_on_with_device_poll(&ctx.device, oom_scope.pop()).map_err(|_| poll_failed())?;
    let validation_err = block_on_with_device_poll(&ctx.device, validation_scope.pop())
        .map_err(|_| poll_failed())?;
    if let Some(e) = oom_err {
        return Err(BatchBackendError::KernelLaunchFailed(format!(
            "gpu out of memory: {e}"
        )));
    }
    if let Some(e) = validation_err {
        return Err(BatchBackendError::KernelLaunchFailed(format!(
            "gpu validation error: {e}"
        )));
    }

    let bytes = wait_and_read_buffer(ctx, &readback_buffer)?;
    let scores = f32_vec_from_ne_bytes(&bytes)?;
    Ok(scores)
}

/// [`dispatch_dot_products`]/[`dispatch_partial_topk`] が共有する readback
/// 完了待ちの本体（Issue #536・R: 両関数から重複していた map_async・deadline
/// 付きポーリング・エラー写像を 1 箇所へ集約する）。呼び出し元は
/// `copy_buffer_to_buffer` 済みの `readback_buffer`（`MAP_READ` 用途）を渡し、
/// マップ完了後の生バイト列を受け取る（呼び出し元が `f32`/`u32` へ解釈する）。
fn wait_and_read_buffer(
    ctx: &GpuContext,
    readback_buffer: &wgpu::Buffer,
) -> Result<Vec<u8>, BatchBackendError> {
    let slice = readback_buffer.slice(..);
    let map_result: std::sync::Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> =
        std::sync::Arc::new(Mutex::new(None));
    let map_result_cb = map_result.clone();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        if let Ok(mut guard) = map_result_cb.lock() {
            *guard = Some(res);
        }
    });

    // 無期限待機（`PollType::wait_indefinitely()`）は使わず、有限タイムアウトの
    // `Wait` を deadline まで繰り返す（codex/Bugbot 指摘対応: Metal 等で完了通知が
    // 止まると無期限待機はそのまま戻らず、CPU 縮退〔CORE-8〕へ移れない）。
    // `PollError::Timeout` は「まだ完了していない」だけなのでループを継続し、
    // deadline 超過時に `DeviceLost` として返す。
    let deadline = std::time::Instant::now() + GPU_POLL_DEADLINE;
    loop {
        let poll_result = ctx.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(GPU_POLL_SLICE),
        });
        if let Err(e) = poll_result {
            // タイムアウト以外のポーリング失敗はデバイス異常として即座に返す。
            if !matches!(e, wgpu::PollError::Timeout) {
                return Err(BatchBackendError::DeviceLost(
                    "device poll failed".to_string(),
                ));
            }
        }
        let ready = match map_result.lock() {
            Ok(guard) => guard.is_some(),
            Err(_) => {
                return Err(BatchBackendError::TransferFailed(
                    "map result mutex poisoned".to_string(),
                ))
            }
        };
        if ready {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(BatchBackendError::DeviceLost(
                "device poll timed out while waiting for readback".to_string(),
            ));
        }
        std::thread::yield_now();
    }

    let mapped = match map_result.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => {
            return Err(BatchBackendError::TransferFailed(
                "map result mutex poisoned".to_string(),
            ))
        }
    };
    match mapped {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            return Err(BatchBackendError::TransferFailed(format!(
                "buffer map failed: {e}"
            )))
        }
        None => {
            return Err(BatchBackendError::TransferFailed(
                "buffer map did not complete".to_string(),
            ))
        }
    }

    let bytes = {
        let view = slice.get_mapped_range().map_err(|e| {
            BatchBackendError::TransferFailed(format!("get_mapped_range failed: {e}"))
        })?;
        // codex 指摘対応（PR #578）: `view.to_vec()` はマップ領域と同サイズの
        // ヒープ確保に失敗すると abort し、CORE-8 の CPU 縮退（呼び出し元が
        // `Err` を受け取って `FallbackBatchEngine` へ移る経路）へ戻れない。
        // `try_reserve_bytes` でフォールブルに確保してから `copy_from_slice`
        // する（[`f32_vec_from_ne_bytes`] 等と同じ fail-closed 契約）。
        let mut buf = Vec::new();
        buf.try_reserve_exact(view.len()).map_err(|_| {
            BatchBackendError::TransferFailed("readback buffer allocation failed".to_string())
        })?;
        buf.extend_from_slice(&view);
        buf
    };
    readback_buffer.unmap();
    Ok(bytes)
}

/// `&[u8]`（ネイティブエンディアン・4 の倍数長）を `Vec<u32>` へ変換する
/// （[`f32_vec_from_ne_bytes`] の u32 版。[`dispatch_partial_topk`] の
/// readback デコードに使う）。
fn u32_vec_from_ne_bytes(bytes: &[u8]) -> Result<Vec<u32>, BatchBackendError> {
    let (quads, remainder) = bytes.as_chunks::<4>();
    if !remainder.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    try_reserve_u32(&mut out, quads.len())?;
    for quad in quads {
        out.push(u32::from_ne_bytes(*quad));
    }
    Ok(out)
}

/// 1 クエリタイル × `row_indices` 分の workgroup 内部分 Top-k を GPU で計算し、
/// `(key, slot)` を `num_workgroups × k_out` 件（クエリごと）並べた `u32` 列を
/// readback する（Issue #536）。手順・防御は [`dispatch_dot_products`] と
/// 共通化しており（`wait_and_read_buffer` を共有）、差分は出力バッファの
/// サイズ（スコア行列ではなく Top-k 候補列）と uniform パラメータの型
/// （[`GpuTopKParams`]。32 バイト）のみ。
fn dispatch_partial_topk(
    ctx: &GpuContext,
    target: &DotDispatchTarget<'_>,
    row_indices: &[u32],
    queries_concat: &[f32],
    query_count: usize,
    query_stride: usize,
    k_out: usize,
) -> Result<Vec<u32>, BatchBackendError> {
    if row_indices.is_empty() || query_count == 0 {
        return Ok(Vec::new());
    }
    if query_count > GPU_QUERY_TILE_MAX {
        return Err(BatchBackendError::KernelLaunchFailed(
            "query tile width exceeds GPU_QUERY_TILE_MAX".to_string(),
        ));
    }
    let Some(topk_pipeline) = target.topk_pipeline else {
        return Err(BatchBackendError::KernelLaunchFailed(
            "partial topk dispatch requested without a topk pipeline".to_string(),
        ));
    };
    let expected_query_len = query_count.checked_mul(query_stride).ok_or_else(|| {
        BatchBackendError::KernelLaunchFailed("query buffer size overflow".to_string())
    })?;
    if queries_concat.len() != expected_query_len {
        return Err(BatchBackendError::TransferFailed(
            "query buffer length does not match query_count * query_stride".to_string(),
        ));
    }
    let k_out_u32 = u32::try_from(k_out.clamp(1, GPU_TOPK_OUT_MAX as usize)).unwrap_or(1);
    let row_count = row_indices.len() as u32;
    let query_count_u32 = query_count as u32;
    let num_workgroups = (row_count as u64).div_ceil(GPU_WORKGROUP_SIZE as u64);
    let workgroups_x = u32::try_from(num_workgroups).unwrap_or(u32::MAX);

    let params = GpuTopKParams {
        row_stride: target.row_stride,
        row_count,
        query_count: query_count_u32,
        query_stride: query_stride as u32,
        k_out: k_out_u32,
    };

    let params_bytes = params.to_ne_bytes_vec()?;
    let row_ids_bytes = bytes_of_u32_slice(row_indices)?;
    let query_bytes = encode_query_bytes(queries_concat, target.query_encoding)?;

    let out_count = (num_workgroups)
        .saturating_mul(query_count as u64)
        .saturating_mul(k_out_u32 as u64)
        .saturating_mul(2);
    let out_bytes_len = out_count.saturating_mul(4);
    if out_bytes_len == 0 {
        return Err(BatchBackendError::KernelLaunchFailed(
            "partial topk output buffer size is zero".to_string(),
        ));
    }

    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let params_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product topk params"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&params_buffer, 0, &params_bytes);

    let row_ids_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product topk row ids"),
        size: row_ids_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&row_ids_buffer, 0, &row_ids_bytes);

    let query_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product topk query"),
        size: query_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&query_buffer, 0, &query_bytes);

    let out_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product topk out"),
        size: out_bytes_len,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product topk readback"),
        size: out_bytes_len,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("batch dot product topk bind group"),
        layout: target.bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: target.row_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: row_ids_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: query_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: out_buffer.as_entire_binding(),
            },
        ],
    });

    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("batch dot product topk encoder"),
        });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("batch dot product topk pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(topk_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(workgroups_x, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&out_buffer, 0, &readback_buffer, 0, out_bytes_len);
    ctx.queue.submit(std::iter::once(encoder.finish()));

    let poll_failed =
        || BatchBackendError::DeviceLost("device poll failed or timed out".to_string());
    let oom_err =
        block_on_with_device_poll(&ctx.device, oom_scope.pop()).map_err(|_| poll_failed())?;
    let validation_err = block_on_with_device_poll(&ctx.device, validation_scope.pop())
        .map_err(|_| poll_failed())?;
    if let Some(e) = oom_err {
        return Err(BatchBackendError::KernelLaunchFailed(format!(
            "gpu out of memory: {e}"
        )));
    }
    if let Some(e) = validation_err {
        return Err(BatchBackendError::KernelLaunchFailed(format!(
            "gpu validation error: {e}"
        )));
    }

    let bytes = wait_and_read_buffer(ctx, &readback_buffer)?;
    u32_vec_from_ne_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    // 本体側はマスク判定を `PolicyContext::is_visible` の単一照合パスへ委ねており
    // `Visibility` を直接参照しない。フィクスチャ構築のためテスト側でのみ取り込む。
    use crate::storage::Visibility;

    // GPU デバイスに依存しない純粋関数のみをここで検証する。デバイス初期化を
    // 要するテスト（初期化失敗→縮退・実 GPU 分岐）は `tests/gpu_batch.rs`
    // （結合テスト。環境条件で両分岐を検証する。TASK-128 設計方針 §3.5）に置く。

    // --- Issue #532: クエリタイル化（1 dispatch で複数クエリを処理）の純関数 ---

    #[test]
    fn dot_shader_wgsl_query_tile_max_matches_host_constant() {
        // WGSL のレジスタ配列サイズ（`array<f32, QUERY_TILE_MAX>`）が
        // ホスト側の `GPU_QUERY_TILE_MAX`（ホストが `dispatch_dot_products`
        // で拒否する上限）とビットで一致することを固定する。値がずれると
        // シェーダ側が `min` でクランプした本数しか計算しないのに対し
        // ホストは超過分を範囲外アクセスとして readback してしまう。
        let expected = format!("const QUERY_TILE_MAX: u32 = {GPU_QUERY_TILE_MAX}u;");
        assert!(
            DOT_SHADER_WGSL.contains(&expected),
            "f16 shader must declare {expected}"
        );
        assert!(
            DOT_SHADER_F32_WGSL.contains(&expected),
            "f32 contrast shader must declare {expected}"
        );
    }

    // --- Issue #539: SHADER_F16 対応アダプタでの f16 算術版シェーダ選択 ---

    #[test]
    fn dot_shader_f16_arith_wgsl_declares_enable_first_and_matches_host_constants() {
        // `enable f16;` は WGSL の仕様上、他のすべての module-scope 宣言に
        // 先行しなければならない（naga 30.0.1 は違反を validation エラーに
        // する）。先頭付近にあることを固定する。
        let enable_pos = DOT_SHADER_F16_ARITH_WGSL
            .find("enable f16;")
            .expect("f16 arith shader must declare enable f16;");
        let struct_pos = DOT_SHADER_F16_ARITH_WGSL
            .find("struct Params")
            .expect("f16 arith shader must declare struct Params");
        assert!(
            enable_pos < struct_pos,
            "enable f16; must precede other module-scope declarations"
        );

        let tile_max = format!("const QUERY_TILE_MAX: u32 = {GPU_QUERY_TILE_MAX}u;");
        assert!(
            DOT_SHADER_F16_ARITH_WGSL.contains(&tile_max),
            "f16 arith shader must declare {tile_max}"
        );
        // `F16_ACC_BLOCK`（f16 レジスタのフラッシュ機構）は PR #591 レビュー
        // P1 指摘対応・2 巡目で撤去済み（積和を f32 へ拡張後に行う設計へ
        // 変更したため不要。`DOT_SHADER_F16_ARITH_WGSL` doc 参照）。両
        // シェーダとも f16 のまま乗算しないことを固定する。
        assert!(
            !DOT_SHADER_F16_ARITH_WGSL.contains("F16_ACC_BLOCK"),
            "f16 arith shader must not retain the removed F16_ACC_BLOCK flush mechanism"
        );
        assert!(
            !DOT_SHADER_F16_ARITH_WGSL.contains("fma(row_pair, qv"),
            "f16 arith shader must widen to f32 before multiplying (no half-precision fma)"
        );

        // topk 版（`topk_dot_shader!` 経由）も同一の prelude 定数を共有する
        // ことを固定する（S0 の演算順一致契約の一部）。
        assert!(
            DOT_SHADER_TOPK_F16_ARITH_WGSL.contains("enable f16;"),
            "f16 arith topk shader must declare enable f16;"
        );
        assert!(
            !DOT_SHADER_TOPK_F16_ARITH_WGSL.contains("F16_ACC_BLOCK"),
            "f16 arith topk shader must not retain the removed F16_ACC_BLOCK flush mechanism"
        );
        assert!(
            !DOT_SHADER_TOPK_F16_ARITH_WGSL.contains("fma(row_pair, qv"),
            "f16 arith topk shader must widen to f32 before multiplying (no half-precision fma)"
        );
        let enable_pos_topk = DOT_SHADER_TOPK_F16_ARITH_WGSL
            .find("enable f16;")
            .expect("f16 arith topk shader must declare enable f16;");
        let struct_pos_topk = DOT_SHADER_TOPK_F16_ARITH_WGSL
            .find("struct TopKParams")
            .expect("f16 arith topk shader must declare struct TopKParams");
        assert!(
            enable_pos_topk < struct_pos_topk,
            "enable f16; must precede other module-scope declarations in the topk shader too"
        );
    }

    #[test]
    fn dot_shader_topk_unpack_and_f32_variants_do_not_declare_enable_f16() {
        // 既存 2 呼び出し（unpack・f32 対照）はマクロの `$prelude` 引数化後も
        // 挙動不変であることの回帰: `enable f16;` を宣言しない
        // （feature 非対応デバイスでもコンパイル可能なままであることの固定）。
        assert!(!DOT_SHADER_TOPK_WGSL.contains("enable f16;"));
        assert!(!DOT_SHADER_TOPK_F32_WGSL.contains("enable f16;"));
    }

    #[test]
    fn select_dot_shader_requires_f16_available() {
        assert_eq!(
            select_dot_shader(false, 1.0, 1.0, false, false),
            GpuDotShaderKind::Unpack
        );
    }

    #[test]
    fn select_dot_shader_rejects_non_finite_inputs() {
        assert_eq!(
            select_dot_shader(true, f32::INFINITY, 1.0, false, false),
            GpuDotShaderKind::Unpack
        );
        assert_eq!(
            select_dot_shader(true, 1.0, f32::NAN, false, false),
            GpuDotShaderKind::Unpack
        );
    }

    #[test]
    fn select_dot_shader_rejects_query_amplitude_above_f16_max_independently_of_row_max_abs() {
        // advisor 指摘（点 4）: `query_max_abs` が f16 の有限最大値を超える
        // 場合、`row_max_abs` が 0（全成分 0 の行）であっても採用してはならない
        // （0 * ±Inf は f16 でも f32 でも NaN になり一見一致するように見えるが、
        // クエリ成分が f16 パック時に飽和する分岐そのものを閉じるための独立
        // ガード）。
        assert_eq!(
            select_dot_shader(true, 0.0, F16_MAX_FINITE + 1.0, false, false),
            GpuDotShaderKind::Unpack
        );
    }

    #[test]
    fn select_dot_shader_rejects_partial_sum_overflow_bound() {
        // row_max_abs * query_max_abs * GPU_F16_ACC_BLOCK が上限を超える。
        let over = (F16_ARITH_PARTIAL_SUM_LIMIT / (GPU_F16_ACC_BLOCK as f32)) + 1.0;
        assert_eq!(
            select_dot_shader(true, over, 1.0, false, false),
            GpuDotShaderKind::Unpack
        );
    }

    #[test]
    fn select_dot_shader_accepts_when_all_guards_pass() {
        let per_side = ((F16_ARITH_PARTIAL_SUM_LIMIT / (GPU_F16_ACC_BLOCK as f32)) - 1.0).sqrt();
        assert_eq!(
            select_dot_shader(true, per_side, per_side, false, false),
            GpuDotShaderKind::F16Arith
        );
    }

    #[test]
    fn select_dot_shader_accepts_boundary_value_exactly_at_limit() {
        // `<=` 判定であることを固定する（境界値ちょうどは受理）。
        let exact = F16_ARITH_PARTIAL_SUM_LIMIT / (GPU_F16_ACC_BLOCK as f32);
        assert_eq!(
            select_dot_shader(true, exact, 1.0, false, false),
            GpuDotShaderKind::F16Arith
        );
    }

    #[test]
    fn select_dot_shader_rejects_query_subnormal_underflow_even_when_overflow_guard_passes() {
        // PR #591 レビュー P1 指摘対応: オーバーフローガードだけを見た既存挙動
        // では、クエリ側に f16 変換でゼロへ丸められる有効な小成分があっても
        // f16 算術版を採用してしまい、正解行がスコア差から脱落しうる
        // （行側は両シェーダ共通で既に f16 量子化済みのためチェック対象外。
        // `max_abs_finite_from_packed` doc 参照）。
        assert_eq!(
            select_dot_shader(true, 1.0, 1.0, true, false),
            GpuDotShaderKind::Unpack
        );
    }

    #[test]
    fn select_dot_shader_rejects_query_precision_loss_even_when_overflow_and_underflow_guards_pass()
    {
        // PR #591 レビュー P1 指摘対応・2 巡目: 振幅上限・非正規化アンダー
        // フローのいずれのガードも通過する値でも、f16 の仮数部 10 bit で
        // 表現できない値は丸められうる（`f16_round_trip_exact` doc 参照）。
        assert_eq!(
            select_dot_shader(true, 1.0, 1.0, false, true),
            GpuDotShaderKind::Unpack
        );
    }

    #[test]
    fn f16_round_trip_exact_accepts_representable_values_and_rejects_the_review_counterexample() {
        // `query=[128.125,128]`（PR #591 レビュー・積の丸め反例）はいずれも
        // f16 で厳密表現できる（往復可能）。この反例は積そのものの丸め
        // （シェーダ側の f32 拡張で解消）が原因であり、クエリ成分自体は
        // 往復可能なことを確認しておく（`f16_round_trip_exact` が過検知
        // しないことの固定）。
        assert!(f16_round_trip_exact(128.125));
        assert!(f16_round_trip_exact(128.0));
        // `query=[2048.5,2048]`（PR #591 レビュー・クエリ量子化反例）は
        // `2048.5` が f16 で厳密表現できず往復不可能。
        assert!(!f16_round_trip_exact(2048.5));
        assert!(f16_round_trip_exact(2048.0));
        // 境界・特殊値。
        assert!(f16_round_trip_exact(0.0));
        assert!(f16_round_trip_exact(-0.0));
        assert!(
            f16_round_trip_exact(f32::NAN),
            "非有限値は対象外として往復可能扱い"
        );
        assert!(f16_round_trip_exact(f32::INFINITY));
    }

    #[test]
    fn max_abs_finite_from_queries_detects_precision_loss_on_review_counterexample() {
        // PR #591 レビュー P1 指摘対応・2 巡目の反例そのもの
        // （`query=[2048.5,2048]`）を固定する。
        let query = [2048.5f32, 2048.0];
        let c = PolicyContext::new("tenant-a").expect("valid tenant id");
        let queries = [BatchQuery {
            vector: &query,
            k: 1,
            ctx: &c,
        }];
        let stats = max_abs_finite_from_query_iter(queries.iter());
        assert!(stats.has_precision_loss);
        assert!(
            !stats.has_subnormal_underflow,
            "この反例は振幅・丸めの問題であり非正規化アンダーフローではない"
        );
    }

    /// PR #591 レビュー P1 指摘（2 巡目）の反例を GPU デバイスなしで
    /// 再現する: `select_dot_shader` の全ガードを通過する query/row の
    /// 組でも、積そのものを f16 として保持すると（旧実装）丸めが起き
    /// 真値と異なるスコアになる（反例: `query=[128.125,128]`・
    /// `row_a=[128.125,-128.25]`・`row_b=[0,2^-14]`）。修正後
    /// （積和を f32 で行う設計）は真値と一致する。
    #[test]
    fn f16_arith_single_product_rounding_bug_pr591_p1_round2_reproduction_and_fix() {
        let query = [128.125f32, 128.0];
        let row_a = [128.125f32, -128.25];
        let row_b = [0.0f32, 2f32.powi(-14)];

        let true_dot_a = dot_f32(&query, &row_a);
        let true_dot_b = dot_f32(&query, &row_b);
        assert!(
            true_dot_a > true_dot_b,
            "真値では row_a が k=1 の正解であるべき"
        );

        // 旧実装（`GPU_F16_ACC_BLOCK == 1` の f16 積算シミュレーション）は
        // この反例で row_a のスコアを 0 へ潰し、row_b に順位が入れ替わる
        // ことを固定する（回帰の記録。撤去済みのシェーダ設計を指すが、
        // 積を f16 のまま保持すると必ずこうなることの証拠として残す）。
        let old_score_a = simulate_f16_arith_dot(&row_a, &query, GPU_F16_ACC_BLOCK);
        let old_score_b = simulate_f16_arith_dot(&row_b, &query, GPU_F16_ACC_BLOCK);
        assert_eq!(
            old_score_a, 0.0,
            "128.125^2 が f16 の分解能で丸められ row_a のスコアが 0 になるはず"
        );
        assert!(
            old_score_b > old_score_a,
            "旧実装では row_b に順位が入れ替わるはず（P1 指摘・2 巡目の再現）"
        );

        // 修正後（積和を f32 で行う。DOT_SHADER_F16_ARITH_WGSL doc 参照）は
        // 真値と一致し、row_a が正しく上位に来る。
        let fixed_score_a = simulate_f16_arith_dot_fixed(&row_a, &query);
        let fixed_score_b = simulate_f16_arith_dot_fixed(&row_b, &query);
        assert_eq!(fixed_score_a, true_dot_a);
        assert_eq!(fixed_score_b, true_dot_b);
        assert!(
            fixed_score_a > fixed_score_b,
            "修正後は row_a が正しく k=1 の正解であるべき"
        );
    }

    #[test]
    fn encode_query_bytes_f32_matches_bytes_of_f32_slice() {
        let values = [1.0f32, -2.5, 0.0, 3.25];
        assert_eq!(
            encode_query_bytes(&values, QueryEncoding::F32).expect("f32 encode must not fail"),
            bytes_of_f32_slice(&values).expect("bytes_of_f32_slice must not fail")
        );
    }

    #[test]
    fn encode_query_bytes_f16_packed_round_trips_through_pack_f16x2() {
        let values = [1.0f32, -2.0, 0.5, 4.0];
        let encoded = encode_query_bytes(&values, QueryEncoding::F16Packed)
            .expect("f16 packed encode must not fail");
        // 4 要素 → 2 個の u32（各 4 byte）= 8 byte。
        assert_eq!(encoded.len(), 8);
        let mut roundtrip = Vec::new();
        // clippy `chunks_exact_to_as_chunks` 指摘対応（gpu_batch.rs 本体側と
        // 同方針）。
        let (chunks, _remainder) = encoded.as_chunks::<4>();
        for &bytes in chunks {
            let word = u32::from_ne_bytes(bytes);
            let (a, b) = crate::batch_search::unpack_f16x2(word);
            roundtrip.push(a);
            roundtrip.push(b);
        }
        assert_eq!(roundtrip, values);
    }

    #[test]
    fn encode_query_bytes_f16_packed_rejects_odd_length() {
        let values = [1.0f32, 2.0, 3.0];
        assert!(encode_query_bytes(&values, QueryEncoding::F16Packed).is_err());
    }

    #[test]
    fn max_abs_finite_from_packed_ignores_non_finite_components() {
        let packed = vec![
            crate::batch_search::pack_f16x2(3.0, -5.0),
            crate::batch_search::pack_f16x2(f32::INFINITY, 1.0),
        ];
        // 3.0 と 5.0（絶対値）が有限成分の最大。Inf は除外される。
        assert_eq!(max_abs_finite_from_packed(&packed), 5.0);
    }

    #[test]
    fn max_abs_finite_from_queries_ignores_non_finite_components() {
        let query_a = [1.0f32, -7.0];
        let query_b = [f32::NAN, 2.0];
        let c = PolicyContext::new("tenant-a").expect("valid tenant id");
        let queries = [
            BatchQuery {
                vector: &query_a,
                k: 1,
                ctx: &c,
            },
            BatchQuery {
                vector: &query_b,
                k: 1,
                ctx: &c,
            },
        ];
        let stats = max_abs_finite_from_query_iter(queries.iter());
        assert_eq!(stats.max_abs, 7.0);
        assert!(!stats.has_subnormal_underflow);
    }

    #[test]
    fn max_abs_finite_from_queries_detects_subnormal_underflow() {
        let query = [1e-8f32, 2.0];
        let c = PolicyContext::new("tenant-a").expect("valid tenant id");
        let queries = [BatchQuery {
            vector: &query,
            k: 1,
            ctx: &c,
        }];
        assert!(max_abs_finite_from_query_iter(queries.iter()).has_subnormal_underflow);
    }

    #[test]
    fn plan_query_tile_single_query_matches_legacy_chunk_row_capacity() {
        // width == 1（旧「1 dispatch = 1 クエリ」相当）のとき、旧
        // `gpu_chunk_row_capacity` と同じ `chunk_rows`（32 MiB / 8 bytes/行）
        // になることを固定し、単一クエリ経路の挙動が退行していないことを示す。
        let plan = plan_query_tile(1, GPU_SCORE_BUFFER_BUDGET_BYTES, 65_535);
        assert_eq!(plan.width, 1);
        assert_eq!(plan.chunk_rows, GPU_SCORE_BUFFER_BUDGET_BYTES / 8);
    }

    #[test]
    fn plan_query_tile_clamps_width_to_gpu_query_tile_max() {
        let plan = plan_query_tile(100, GPU_SCORE_BUFFER_BUDGET_BYTES, 65_535);
        assert_eq!(plan.width, GPU_QUERY_TILE_MAX);
        assert!(plan.chunk_rows >= 1);
    }

    #[test]
    fn plan_query_tile_shrinks_chunk_rows_as_width_grows() {
        // タイル幅が広いほど 1 行あたりのスコアバイト数が増えるため、
        // 同じバイト予算では収容できる行チャンクが小さくなる。
        let narrow = plan_query_tile(1, GPU_SCORE_BUFFER_BUDGET_BYTES, 65_535);
        let wide = plan_query_tile(GPU_QUERY_TILE_MAX, GPU_SCORE_BUFFER_BUDGET_BYTES, 65_535);
        assert!(wide.chunk_rows < narrow.chunk_rows);
    }

    #[test]
    fn plan_query_tile_never_returns_zero_even_under_a_tiny_workgroup_limit() {
        // adapter の `max_workgroups_per_dimension` が極端に小さくても
        // `chunk_rows >= 1` を維持し、0 行チャンクで無限ループにならない
        // ことを固定する（fail-closed だが panic はしない）。
        let plan = plan_query_tile(GPU_QUERY_TILE_MAX, GPU_SCORE_BUFFER_BUDGET_BYTES, 1);
        assert_eq!(plan.width, GPU_QUERY_TILE_MAX);
        assert!(plan.chunk_rows >= 1);
    }

    fn ctx_for(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id")
    }

    #[test]
    fn group_queries_by_ctx_preserves_input_order_within_each_group() {
        let ctx_a = ctx_for("tenant-a");
        let ctx_b = ctx_for("tenant-b");
        let v = vec![0.0f32; 1];
        let queries = vec![
            BatchQuery {
                vector: &v,
                k: 1,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &v,
                k: 1,
                ctx: &ctx_b,
            },
            BatchQuery {
                vector: &v,
                k: 1,
                ctx: &ctx_a,
            },
        ];
        let groups = group_queries_by_ctx(&queries);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec![0, 2]);
        assert_eq!(groups[1], vec![1]);
    }

    #[test]
    fn group_queries_by_ctx_splits_differing_visibility_even_for_the_same_tenant() {
        // `PolicyContext` の等価性は tenant_id だけでなく可視性集合も見るため、
        // 同一テナントでも可視性が異なれば別グループになる（タイル内で
        // `gather_reachable_rows` の行集合を共有してよいのは完全に同じ ctx の
        // クエリだけ、という R1 の前提を固定する）。
        let ctx_private =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Private, Visibility::Public])
                .expect("valid ctx");
        let ctx_public = ctx_for("tenant-a");
        let v = vec![0.0f32; 1];
        let queries = vec![
            BatchQuery {
                vector: &v,
                k: 1,
                ctx: &ctx_private,
            },
            BatchQuery {
                vector: &v,
                k: 1,
                ctx: &ctx_public,
            },
        ];
        let groups = group_queries_by_ctx(&queries);
        assert_eq!(groups, vec![vec![0], vec![1]]);
    }

    #[test]
    fn bytes_of_u32_slice_round_trips_via_f32_vec_from_ne_bytes_is_not_applicable() {
        // u32 バイト列と f32 バイト列は別関数だが、ラウンドトリップ可能な
        // エンコード（native endian の 4 byte 固定）であることだけを確認する。
        let values = [0u32, 1, u32::MAX, 42];
        let bytes = bytes_of_u32_slice(&values).expect("small staging buffer must allocate");
        assert_eq!(bytes.len(), 16);
        let mut restored = Vec::new();
        let (quads, remainder) = bytes.as_chunks::<4>();
        assert!(remainder.is_empty(), "u32 encoding must be a multiple of 4");
        for quad in quads {
            restored.push(u32::from_ne_bytes(*quad));
        }
        assert_eq!(restored, values);
    }

    #[test]
    fn f32_vec_from_ne_bytes_round_trips() {
        let values = [0.0f32, 1.5, -3.25, f32::MIN, f32::MAX];
        let bytes = bytes_of_f32_slice(&values).expect("small staging buffer must allocate");
        let restored = f32_vec_from_ne_bytes(&bytes).expect("small readback buffer must allocate");
        assert_eq!(restored, values);
    }

    #[test]
    fn f32_vec_from_ne_bytes_rejects_truncated_input() {
        let bytes = vec![0u8, 1, 2];
        assert!(f32_vec_from_ne_bytes(&bytes)
            .expect("small readback buffer must allocate")
            .is_empty());
    }

    // 回帰テスト（Issue #178 レビュー指摘対応）: GPU 経路は以前
    // `gather_reachable_rows` 内で `rows * dim`（1 クエリ分）しか見積もらず
    // クエリ件数を乗じていなかったため、単発クエリでは予算内でもクエリ件数が
    // 多いバッチで総計算量が `MAX_BATCH_WORK` を大幅に超過しうる DoS 増幅の
    // 穴があった。判定本体（GPU デバイス非依存）を直接呼んで検証する。
    #[test]
    fn batch_work_budget_accumulates_across_queries() {
        let rows = 2_000_000usize;
        let dim = 2_000usize;
        let single_query_work = rows.checked_mul(dim).expect("fixture should not overflow");
        assert!(
            single_query_work <= MAX_BATCH_WORK,
            "fixture must stay within budget for a single query: {single_query_work}"
        );
        assert!(
            check_batch_work_from_visible_counts(&[rows], dim).is_ok(),
            "a single query within budget must be accepted"
        );

        let counts = vec![rows; 4_096]; // MAX_BATCH_QUERIES
        match check_batch_work_from_visible_counts(&counts, dim) {
            Err(crate::batch_search::BatchSearchError::WorkBudgetExceeded { .. }) => {}
            other => {
                panic!("expected WorkBudgetExceeded once query count is accumulated, got {other:?}")
            }
        }
    }

    // 回帰テスト（codex/Bugbot P1 指摘対応）: 課金対象はクエリごとの実到達行数で
    // あり「常駐行列の全行数 × 全クエリ数」の直積ではない。テナント分離により
    // 各クエリが常駐行列の一部しか走査しない構成では、直積課金だと超過扱いに
    // なる要求が受理されることを確認する（超過は `Input` エラーとして CPU 縮退の
    // 対象外になるため、過大課金は成功可能な検索の恒久的失敗を招く）。
    #[test]
    fn batch_work_budget_bills_reachable_rows_not_the_cartesian_product() {
        // 100 テナント × 各 10,000 行 = 全 1,000,000 行の常駐行列に、
        // テナントごとに 1 本ずつ（計 100 本）のクエリが来る構成を模す。
        let total_rows = 1_000_000usize;
        let queries = 100usize;
        let reachable_per_query = total_rows / queries;
        let dim = 768usize;

        // 直積課金（旧実装）: 全行 × 全クエリ × dim は予算を大きく超える。
        let cartesian = total_rows.saturating_mul(queries).saturating_mul(dim);
        assert!(
            cartesian > MAX_BATCH_WORK,
            "fixture must exceed the budget under the old cartesian billing"
        );

        // 実到達行数課金（本実装）: Σ(到達行数 × dim) は予算内であり受理される。
        let counts = vec![reachable_per_query; queries];
        assert!(
            check_batch_work_from_visible_counts(&counts, dim).is_ok(),
            "a batch that the cpu path would accept must not be rejected by the gpu guard"
        );

        // 予算を実際に超える構成は引き続き拒否される（ガードの弱体化防止）。
        let over_budget = vec![total_rows; queries];
        match check_batch_work_from_visible_counts(&over_budget, dim) {
            Err(crate::batch_search::BatchSearchError::WorkBudgetExceeded { .. }) => {}
            other => {
                panic!("expected WorkBudgetExceeded for a genuinely oversized batch, got {other:?}")
            }
        }
    }

    // `check_reachable_batch_work` が常駐行列の可視性判定（`PolicyContext::is_visible`）
    // を通して行数を数えること（他テナントの Private 行を課金対象にしないこと）を、
    // GPU デバイスなしで確認する。
    #[test]
    fn reachable_batch_work_counts_only_visible_rows() {
        let ids = vec![1u64, 2, 3];
        let tenant_ids = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let visibilities = vec![
            Visibility::Private,
            Visibility::Private,
            Visibility::Private,
        ];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 0.0, 1.0, 1.0, 1.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let ctx = PolicyContext::with_visibilities("a", [Visibility::Private])
            .expect("policy context with explicit visibilities should build");
        let query = [1.0f32, 0.0];
        let queries = [BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx,
        }];
        // テナント a の 2 行のみが課金対象（テナント b の Private 行は不可視）。
        assert!(check_reachable_batch_work(&matrix, &queries).is_ok());
    }

    #[test]
    fn gpu_input_error_maps_to_expected_batch_search_error_variant() {
        use crate::batch_search::BatchSearchError;
        match GpuInputError::CapacityExceeded.into_batch_search_error() {
            BatchSearchError::CapacityExceeded { .. } => {}
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn probe_gpu_availability_debug_only() {
        match global_context() {
            Ok(ctx) => {
                eprintln!("GPU_PROBE: available");
                match &ctx.topk_unavailable_reason {
                    None => eprintln!("GPU_PROBE: topk pipeline available"),
                    Some(reason) => eprintln!("GPU_PROBE: topk pipeline unavailable: {reason}"),
                }
            }
            Err(e) => eprintln!("GPU_PROBE: unavailable: {e}"),
        }
    }

    #[test]
    fn gather_reachable_rows_respects_policy_context_is_visible() {
        let ids = vec![1u64, 2, 3];
        let tenant_ids = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let visibilities = vec![Visibility::Private, Visibility::Public, Visibility::Private];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 0.0, 1.0, 1.0, 1.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let ctx = PolicyContext::new("a").expect("policy context should build for valid tenant");
        let reachable = gather_reachable_rows(&matrix, &ctx)
            .expect("reachable row gather should succeed within work budget");
        // テナント a の Private 行（idx 0, 2）と、Public 許可がある場合の
        // テナント b の Public 行（idx 1）が到達可能（`PolicyContext::new` は
        // 既定で Public のみ許可するため、Private 行 idx 0/2 は不可視）。
        assert_eq!(reachable, vec![1]);

        let ctx_priv =
            PolicyContext::with_visibilities("a", [Visibility::Private, Visibility::Public])
                .expect("policy context with explicit visibilities should build");
        let mut reachable_priv = gather_reachable_rows(&matrix, &ctx_priv)
            .expect("reachable row gather should succeed within work budget");
        reachable_priv.sort_unstable();
        assert_eq!(reachable_priv, vec![0, 1, 2]);
    }

    /// テスト専用ヘルパ: 1 個の f32 値を [`crate::batch_search::pack_f16x2`]/
    /// [`crate::batch_search::unpack_f16x2`] で往復させ「f16 へ最近接偶数丸め
    /// した値を f32 として持つ」状態を作る（GPU の f16 レジスタ演算を CPU 上で
    /// エミュレートするための丸め関数）。
    fn round_f16(x: f32) -> f32 {
        crate::batch_search::unpack_f16x2(crate::batch_search::pack_f16x2(x, 0.0)).0
    }

    /// テスト専用ヘルパ: [`DOT_SHADER_F16_ARITH_WGSL`] の演算順（vec2<f16>
    /// レーンごとの `fma` 累積 → `acc_block` 件ごとに f32 アキュムレータへ
    /// フラッシュ）を CPU 上で再現する（PR #591 レビュー P1 指摘対応の
    /// 回帰テスト用。実 GPU dispatch を経由せず、ホストの `unsafe` なし
    /// 純関数のみで検証する）。行・クエリは奇数長でも 0 埋めして扱う。
    fn simulate_f16_arith_dot(row: &[f32], query: &[f32], acc_block: u32) -> f32 {
        let padded_len = row.len().max(query.len()).next_multiple_of(2);
        let mut row_padded = row.to_vec();
        row_padded.resize(padded_len, 0.0);
        let mut query_padded = query.to_vec();
        query_padded.resize(padded_len, 0.0);
        let pairs = padded_len / 2;

        let mut acc: f32 = 0.0;
        let mut acc2: (f32, f32) = (0.0, 0.0);
        let mut block: u32 = 0;
        for j in 0..pairs {
            let (rx, ry) = (row_padded[2 * j], row_padded[2 * j + 1]);
            let (qx, qy) = (query_padded[2 * j], query_padded[2 * j + 1]);
            // シェーダの `fma(row_pair, qv, acc2[q])` は vec2<f16> の
            // レーンごと独立な積和（x レーン・y レーンは互いに加算されない。
            // 合算はフラッシュ時に f32 領域で行われる）。
            acc2.0 = round_f16(rx * qx + acc2.0);
            acc2.1 = round_f16(ry * qy + acc2.1);
            block += 1;
            let flush = block >= acc_block || j + 1 == pairs;
            if flush {
                acc += acc2.0 + acc2.1;
                acc2 = (0.0, 0.0);
                block = 0;
            }
        }
        acc
    }

    /// テスト専用ヘルパ: 素朴な f32 内積（真値の対照。零埋めして扱う）。
    fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().max(b.len());
        let mut acc = 0.0f32;
        for j in 0..len {
            let av = a.get(j).copied().unwrap_or(0.0);
            let bv = b.get(j).copied().unwrap_or(0.0);
            acc += av * bv;
        }
        acc
    }

    /// テスト専用ヘルパ: 修正後の [`DOT_SHADER_F16_ARITH_WGSL`] の演算順
    /// （読み出し直後に f32 へ拡張してから乗算・加算する。積を f16 として
    /// 保持しない）を CPU 上で再現する（PR #591 レビュー P1 指摘対応・
    /// 2 巡目の回帰テスト用）。`row`/`query` は「既に f16 表現された」
    /// 前提の値（呼び出し元がテストで f16 表現可能な値を選ぶ）を渡す想定
    /// で、本関数自体は追加の丸めを行わない（[`dot_f32`] と数式上は同一だが、
    /// シェーダの読み出し順・vec2 単位の加算順を明示するため独立関数として
    /// 残す）。
    fn simulate_f16_arith_dot_fixed(row: &[f32], query: &[f32]) -> f32 {
        let padded_len = row.len().max(query.len()).next_multiple_of(2);
        let mut row_padded = row.to_vec();
        row_padded.resize(padded_len, 0.0);
        let mut query_padded = query.to_vec();
        query_padded.resize(padded_len, 0.0);
        let pairs = padded_len / 2;

        let mut acc: f32 = 0.0;
        for j in 0..pairs {
            let (rx, ry) = (row_padded[2 * j], row_padded[2 * j + 1]);
            let (qx, qy) = (query_padded[2 * j], query_padded[2 * j + 1]);
            acc += rx * qx + ry * qy;
        }
        acc
    }

    #[test]
    fn f16_arith_precision_bug_pr591_p1_is_fixed_by_acc_block_1() {
        // PR #591 レビュー P1 指摘（1 巡目）の反例をそのまま再現する:
        // query の f16 で厳密表現できる成分が row と積算される過程で、
        // 複数項を f16 のまま加算するとオーバーフローガードを通過して
        // いても桁落ちが起き、
        //
        // 注記: 本テストが固定する「ブロック幅 1」は複数項の f16 加算を
        // 無くすが、単一の積そのものを f16 として保持する丸め（PR #591
        // レビュー P1 指摘・2 巡目。
        // `f16_arith_single_product_rounding_bug_pr591_p1_round2_reproduction_and_fix`
        // 参照）までは解消しない。実際の production シェーダは積和を
        // f32 で行う設計（`DOT_SHADER_F16_ARITH_WGSL` doc 参照）へ変更
        // 済みのため、本テストが再現する `simulate_f16_arith_dot` は
        // 撤去済みの旧シェーダ設計の記録用シミュレーションである。
        // k=1 の正解行が入れ替わりうる（`GPU_F16_ACC_BLOCK` doc 参照）。
        let query = [2048.0f32, 0.0, 1.0, 0.0, -2048.0];
        let row = [1.0f32, 0.0, 1.0, 0.0, 1.0];
        let true_dot = 1.0f32; // 2048*1 + 1*1 + (-2048)*1

        // 旧実装（`GPU_F16_ACC_BLOCK == 8`）が指摘どおり誤ったスコアを返す
        // ことを固定する（回帰の記録。本 Issue 以降は到達しない設定値）。
        let old_block_score = simulate_f16_arith_dot(&row, &query, 8);
        assert_eq!(
            old_block_score, 0.0,
            "ブロック幅 8 は PR #591 指摘の桁落ち（2048+1 が f16 で 2048 へ丸められる）を再現するはず"
        );
        assert_ne!(
            old_block_score, true_dot,
            "旧ブロック幅は真値と異なる（k=1 の正解が入れ替わる根本原因）"
        );

        // 現在の production 定数（`GPU_F16_ACC_BLOCK == 1`）はこの反例で
        // 真値と一致することを固定する。
        assert_eq!(
            GPU_F16_ACC_BLOCK, 1,
            "P1 修正はブロック幅 1 を前提にしている"
        );
        let fixed_score = simulate_f16_arith_dot(&row, &query, GPU_F16_ACC_BLOCK);
        assert_eq!(
            fixed_score, true_dot,
            "ブロック幅 1（複数項を f16 のまま加算しない）は真値と一致するべき"
        );
    }

    /// テスト専用ヘルパ: [`GpuBatchBackend::try_new`] の `row_max_abs`
    /// 事前計算（P2 指摘対応）を GPU デバイスなしで再現する。
    fn precompute_row_max_abs(matrix: &crate::batch_search::ResidentMatrix) -> Vec<f32> {
        let dim_half = matrix.dim().div_ceil(2);
        (0..matrix.row_count())
            .map(|row_idx| {
                let start = row_idx.saturating_mul(dim_half);
                let end = start.saturating_add(dim_half);
                let row = matrix.packed().get(start..end).unwrap_or(&[]);
                max_abs_finite_from_packed(row)
            })
            .collect()
    }

    #[test]
    fn max_abs_finite_from_precomputed_rows_excludes_other_tenant_invisible_rows() {
        // PR #591 レビュー P0 指摘対応: テナント "a" の可視行は 1.0 のみだが、
        // テナント "b" の（"a" からは不可視な）Private 行に極端な大きさの
        // 成分（65504）を仕込む。母数が常駐行列全体のままなら
        // `max_abs` が 65504 になってしまい、テナント "a" 視点のシェーダ
        // 選択・返却スコアの数値精度が他テナントのデータ有無に左右される。
        let ids = vec![1u64, 2];
        let tenant_ids = vec!["a".to_string(), "b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Private];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, F16_MAX_FINITE, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let row_max_abs = precompute_row_max_abs(&matrix);
        let ctx_a = PolicyContext::new("a").expect("policy context should build for valid tenant");
        let reachable = gather_reachable_rows(&matrix, &ctx_a)
            .expect("reachable row gather should succeed within work budget");
        let visible_max_abs = max_abs_finite_from_precomputed_rows(&row_max_abs, &reachable);
        assert_eq!(
            visible_max_abs, 1.0,
            "tenant b の不可視行（65504）が母数へ混入してはならない"
        );
    }

    #[test]
    fn max_abs_finite_from_precomputed_rows_is_scoped_to_a_single_group_even_when_batch_has_multiple_contexts(
    ) {
        // PR #591 レビュー P0 指摘対応（再発防止）: 同一バッチに複数
        // `PolicyContext` が混在する場合でも、あるグループの母数計算に
        // 「そのグループの reachable」だけを渡せば他グループの可視行は
        // 一切混入しない。テナント "b" の行を Private にし "a" からは
        // 不可視にしたうえで、"a" 視点の `reachable` を渡した場合の結果が
        // "b" の値（65504）に左右されないことを固定する。
        let ids = vec![1u64, 2];
        let tenant_ids = vec!["a".to_string(), "b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Private];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, F16_MAX_FINITE, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let row_max_abs = precompute_row_max_abs(&matrix);

        let ctx_a = PolicyContext::new("a").expect("policy context should build for valid tenant");
        let reachable_a = gather_reachable_rows(&matrix, &ctx_a)
            .expect("reachable row gather should succeed within work budget");
        assert_eq!(
            max_abs_finite_from_precomputed_rows(&row_max_abs, &reachable_a),
            1.0,
            "tenant a の reachable にはテナント b の行が含まれないため、\
             同一バッチ内に b の巨大成分があっても a 視点の母数へ混入しない"
        );
    }

    #[test]
    fn select_dot_shader_decision_is_independent_per_policy_context_group_in_a_mixed_batch() {
        // PR #591 レビュー P0 指摘対応（再発防止・エンドツーエンド）: 1 バッチに
        // 2 つの `PolicyContext`（テナント "a"・"b"）を含み、"a" は
        // オーバーフローガードを満たす小振幅のみ、"b" は単独ではガードを
        // 超える巨大振幅を持つ。テナント "b" の行は Private にし "a" からは
        // 不可視にする（RLS 相当のテナント境界）。母数を `PolicyContext`
        // グループ単位に分離できていれば、"a" のシェーダ選択は "b" の
        // 存在・値に一切左右されず `F16Arith` のまま、"b" は自身の振幅に
        // より `Unpack` へ縮退する。`AdaptiveShaderSelector::resolve` が
        // 実際に呼ぶ `max_abs_finite_from_precomputed_rows`／
        // `max_abs_finite_from_queries_subset`／`select_dot_shader` の
        // 組み合わせをそのまま再現する（GPU デバイス非依存の純関数のみ）。
        let ids = vec![1u64, 2];
        let tenant_ids = vec!["a".to_string(), "b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Private];
        // テナント a の行は 1.0 のみ（小振幅）、テナント b の行は
        // F16_MAX_FINITE 近傍（大振幅）。
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, F16_MAX_FINITE, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let row_max_abs = precompute_row_max_abs(&matrix);

        let ctx_a = PolicyContext::new("a").expect("policy context should build for valid tenant");
        // "b" は自分自身の Private 行を見る必要があるため、`Private` を
        // 明示許可する `PolicyContext` を使う（`PolicyContext::new` は
        // `Public` のみ許可の既定・最小権限）。
        let ctx_b =
            PolicyContext::with_visibilities("b", [Visibility::Private, Visibility::Public])
                .expect("policy context with explicit visibilities should build");
        let query_a = [1.0f32, 0.0];
        let query_b = [1.0f32, 0.0];
        let queries = [
            BatchQuery {
                vector: &query_a,
                k: 1,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 1,
                ctx: &ctx_b,
            },
        ];
        let groups = group_queries_by_ctx(&queries);
        assert_eq!(groups.len(), 2, "2 テナントは別グループに分かれる");

        for group in &groups {
            let &first_idx = group.first().expect("group should be non-empty");
            let group_ctx = queries[first_idx].ctx;
            let reachable = gather_reachable_rows(&matrix, group_ctx)
                .expect("reachable row gather should succeed within work budget");
            let group_row_max_abs = max_abs_finite_from_precomputed_rows(&row_max_abs, &reachable);
            let group_query_stats = max_abs_finite_from_queries_subset(&queries, group);
            let shader_kind = select_dot_shader(
                true,
                group_row_max_abs,
                group_query_stats.max_abs,
                group_query_stats.has_subnormal_underflow,
                group_query_stats.has_precision_loss,
            );
            if group_ctx == &ctx_a {
                assert_eq!(
                    shader_kind,
                    GpuDotShaderKind::F16Arith,
                    "tenant b の巨大行がバッチに同居していても、\
                     tenant a 視点の母数はテナント a の可視行のみで決まるため \
                     F16Arith が選ばれ続けるべき"
                );
            } else {
                assert_eq!(
                    shader_kind,
                    GpuDotShaderKind::Unpack,
                    "tenant b 自身の巨大行はオーバーフローガードを超えるため \
                     Unpack へ縮退するべき"
                );
            }
        }
    }

    /// 同一 `(tenant_id, id)` 契約の検証用フィクスチャ（Issue #178 レビュー指摘対応）。
    /// スロット 0 = `("tenant-a", 99, Public)`・スロット 1 = `("tenant-b", 1, Public)` と、
    /// 「スロット昇順」と「行 id 昇順」が逆順になる配置にする。行は本番経路では
    /// `(tenant_id, id)` 順で常駐行列へ渡される（`batch_search.rs::BatchHit` の
    /// 順序契約）ため、この配置は特殊ケースではなく通常のマルチテナント配置である。
    fn tie_fixture() -> (Vec<u64>, Vec<String>, Vec<Visibility>, usize, Vec<f32>) {
        (
            vec![99u64, 1],
            vec!["tenant-a".to_string(), "tenant-b".to_string()],
            vec![Visibility::Public, Visibility::Public],
            2,
            // 2 行とも同一ベクトル = 同点スコアになりタイブレークが顕在化する。
            vec![1.0, 0.0, 1.0, 0.0],
        )
    }

    fn tie_matrix() -> crate::batch_search::ResidentMatrix {
        let (ids, tenant_ids, visibilities, dim, vectors) = tie_fixture();
        crate::batch_search::ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, &vectors)
            .expect("resident matrix build should succeed for well-formed fixture")
    }

    /// `finalize_gpu_hits` は候補識別子をスロット番号として解決し、
    /// `(tenant_id, id)` 付きの `SearchHit` を選出順のまま返す。
    #[test]
    fn finalize_gpu_hits_resolves_slots_to_tenant_qualified_hits() {
        let matrix = tie_matrix();
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let mut selector = TopKSelector::new(2);
        selector.push(CandidateHit { id: 0, score: 1.0 });
        selector.push(CandidateHit { id: 1, score: 1.0 });
        let hits = finalize_gpu_hits(&matrix, &ctx, &selector.into_sorted_vec())
            .expect("visible slots should resolve");
        // 同点のタイブレークはスロット昇順（行 id 昇順ではない）。
        let resolved: Vec<(&str, u64)> =
            hits.iter().map(|h| (h.tenant_id.as_str(), h.id)).collect();
        assert_eq!(resolved, vec![("tenant-a", 99), ("tenant-b", 1)]);
    }

    /// 解決不能なスロット（GPU 側 readback 破損・実装バグを模す）は部分結果を
    /// 返さず全体を拒否する（fail-closed）。
    #[test]
    fn finalize_gpu_hits_rejects_out_of_range_slot() {
        let matrix = tie_matrix();
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let err = finalize_gpu_hits(
            &matrix,
            &ctx,
            &[
                CandidateHit { id: 0, score: 1.0 },
                CandidateHit {
                    id: 9_999,
                    score: 0.5,
                },
            ],
        )
        .expect_err("out-of-range slot must be rejected");
        assert_eq!(
            err,
            crate::batch_search::BatchSearchError::TenantMaskViolation
        );
    }

    /// 当該クエリから不可視の行（他テナントの `Private` 行）を指すスロットも
    /// `PolicyContext::is_visible` の単一照合パスで拒否する。
    #[test]
    fn finalize_gpu_hits_rejects_slot_invisible_to_query_context() {
        let ids = vec![1u64, 2];
        let tenant_ids = vec!["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Private];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 1.0, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let err = finalize_gpu_hits(&matrix, &ctx, &[CandidateHit { id: 1, score: 1.0 }])
            .expect_err("invisible row must be rejected");
        assert_eq!(
            err,
            crate::batch_search::BatchSearchError::TenantMaskViolation
        );
    }

    /// テナント跨ぎで同じ `id` を持つ行（`id` 単独では一意でない）も、
    /// `(tenant_id, id)` として区別して返る（対象ビヘイビア: TABLE-12・RLS-9）。
    #[test]
    fn finalize_gpu_hits_distinguishes_same_id_across_tenants() {
        let ids = vec![7u64, 7];
        let tenant_ids = vec!["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Public];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 1.0, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let hits = finalize_gpu_hits(
            &matrix,
            &ctx,
            &[
                CandidateHit { id: 0, score: 1.0 },
                CandidateHit { id: 1, score: 1.0 },
            ],
        )
        .expect("both public rows are visible to tenant-a");
        let resolved: Vec<(&str, u64)> =
            hits.iter().map(|h| (h.tenant_id.as_str(), h.id)).collect();
        assert_eq!(resolved, vec![("tenant-a", 7), ("tenant-b", 7)]);
    }

    /// GPU 経路の最終出力が `FallbackBatchEngine::revalidate_primary_hits`
    /// （PR #205/#228 で `(tenant_id, id)` 基準へ統一された順序・存在・可視性の
    /// 独立再検証）を通ることを、GPU デバイスなしで確認する回帰テスト。
    /// 選出時の候補識別子に行 id を使う旧実装では、同点時に「行 id 昇順」で
    /// 並ぶため再検証の順序契約（スロット昇順）に違反し、正当な結果まで
    /// `PrimaryResultRejected` として拒否されていた（Issue #178 レビュー指摘）。
    #[test]
    fn gpu_finalized_hits_pass_primary_revalidation_while_row_id_order_is_rejected() {
        use crate::batch_fallback::{
            BatchBackend, BatchExecError, FallbackBatchEngine, FallbackObserver,
        };

        /// 与えられたクロージャの結果をそのまま返す差し替え用バックエンド
        /// （`batch_fallback.rs` のテストにある `MaliciousBackend` と同じ形。
        /// 本体へテスト専用の公開 API は追加しない）。
        struct StubBackend<F: Fn() -> Vec<BatchHit> + Send + Sync> {
            make_hits: F,
        }
        impl<F: Fn() -> Vec<BatchHit> + Send + Sync> BatchBackend for StubBackend<F> {
            fn batch_search(
                &self,
                _queries: &[BatchQuery<'_>],
            ) -> Result<Vec<BatchHit>, BatchExecError> {
                Ok((self.make_hits)())
            }
        }

        struct SilentObserver;
        impl FallbackObserver for SilentObserver {
            fn on_fallback(&self, _event: crate::batch_fallback::FallbackEvent) {}
        }

        let (ids, tenant_ids, visibilities, dim, vectors) = tie_fixture();
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let query_vec = vec![1.0f32, 0.0];

        // 現行実装（スロット順）の出力を返すバックエンド: 再検証を通る。
        let engine = FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            |matrix| {
                Ok(Box::new(StubBackend {
                    make_hits: move || {
                        let ctx = PolicyContext::new("tenant-a")
                            .expect("policy context should build in stub");
                        let mut selector = TopKSelector::new(2);
                        selector.push(CandidateHit { id: 0, score: 1.0 });
                        selector.push(CandidateHit { id: 1, score: 1.0 });
                        let hits = finalize_gpu_hits(&matrix, &ctx, &selector.into_sorted_vec())
                            .expect("slots are visible to tenant-a");
                        vec![BatchHit { hits }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(SilentObserver),
        )
        .expect("fallback engine build should succeed for well-formed fixture");
        let queries = [BatchQuery {
            vector: &query_vec,
            k: 2,
            ctx: &ctx,
        }];
        let hits = engine
            .batch_search(&queries)
            .expect("slot-ordered gpu output must pass primary revalidation");
        let resolved: Vec<(&str, u64)> = hits
            .first()
            .map(|b| {
                b.hits
                    .iter()
                    .map(|h| (h.tenant_id.as_str(), h.id))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(resolved, vec![("tenant-a", 99), ("tenant-b", 1)]);

        // 旧実装（行 id 順）の出力を模したバックエンド: 再検証で拒否される
        // ことを確認し、上のテストが順序契約を実際に守っていることを裏付ける。
        let engine_row_id_order = FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            |_matrix| {
                Ok(Box::new(StubBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                SearchHit::new("tenant-b", 1, 1.0),
                                SearchHit::new("tenant-a", 99, 1.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(SilentObserver),
        )
        .expect("fallback engine build should succeed for well-formed fixture");
        let err = engine_row_id_order
            .batch_search(&queries)
            .expect_err("row-id-ordered output violates the slot-ascending contract");
        assert_eq!(
            err,
            crate::batch_search::BatchSearchError::PrimaryResultRejected
        );
    }

    // `gather_reachable_rows` が「当該クエリから可視な行だけ」を収集すること
    // （他テナントの `Private` 行を走査対象に含めないこと）を、他テナント行を
    // 多数含む行列で確認する。計算量ガードは本関数ではなく主防御線
    // `check_reachable_batch_work` が担うため、ここでは可視性フィルタのみを検証する
    // （Cursor Bugbot 指摘対応: 旧テストは「全行課金なら予算超過」と述べていたが、
    // `MAX_BATCH_ROWS × MAX_BATCH_DIM` < `MAX_BATCH_WORK` のため全行課金でも
    // 超過しえず、主張していた回帰を検知できなかった）。
    #[test]
    fn gather_reachable_rows_collects_only_visible_rows_in_a_crowded_matrix() {
        let dim = 2usize;
        let rows = 40usize; // 全 40 行のうち可視は 1 行だけ
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenant_ids: Vec<String> = (0..rows)
            .map(|i| {
                if i == 0 {
                    "a".to_string()
                } else {
                    format!("other-{i}")
                }
            })
            .collect();
        let visibilities = vec![Visibility::Private; rows];
        let vectors = vec![0.5f32; rows * dim];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let ctx = PolicyContext::with_visibilities("a", [Visibility::Private])
            .expect("policy context with explicit visibilities should build");
        let reachable = gather_reachable_rows(&matrix, &ctx).expect("visible rows should gather");
        assert_eq!(
            reachable,
            vec![0],
            "only tenant a's private row is visible; other tenants' private rows must not be scanned"
        );
    }

    // GPU デバイスが利用可能な実行環境でのみ実走する end-to-end 正しさの検証。
    // 利用不能な環境（CI の GitHub ホステッド runner 等）では `try_new` が
    // `InitFailed` を返すため、テスト自体は「初期化失敗を確認して終了」に
    // フォールバックする（skip・ignore にはしない。TASK-128 設計方針 §3.5）。
    #[test]
    fn gpu_batch_search_matches_hand_computed_dot_products_when_available() {
        let ids = vec![10u64, 20, 30];
        let tenant_ids = vec!["t".to_string(); 3];
        let visibilities = vec![Visibility::Public; 3];
        let dim = 4;
        // 行 0: [1,0,0,0] 行 1: [0,1,0,0] 行 2: [1,1,1,1]
        let vectors = vec![
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            1.0, 1.0, 1.0, 1.0,
        ];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "gpu unavailable in this environment, skipping end-to-end assertions: {e}"
                );
                return;
            }
        };

        let ctx = PolicyContext::new("t").expect("policy context should build for valid tenant");
        let query_vec = vec![2.0f32, 3.0, 0.0, 0.0];
        let query = BatchQuery {
            vector: &query_vec,
            k: 3,
            ctx: &ctx,
        };
        let hits = backend
            .batch_search(std::slice::from_ref(&query))
            .expect("gpu batch_search should succeed once the device initialized");
        assert_eq!(hits.len(), 1);
        let mut scored: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
        scored.sort_by_key(|(id, _)| *id);
        // 期待値: id=10 → 2.0（[1,0,0,0]・[2,3,0,0]）・id=20 → 3.0
        // （[0,1,0,0]・[2,3,0,0]）・id=30 → 5.0（[1,1,1,1]・[2,3,0,0]）。
        assert_eq!(scored.len(), 3);
        for (id, score) in scored {
            let expected = match id {
                10 => 2.0f32,
                20 => 3.0f32,
                30 => 5.0f32,
                other => panic!("unexpected id in gpu batch result: {other}"),
            };
            assert!(
                (score - expected).abs() < 1e-3,
                "id={id} expected={expected} actual={score}"
            );
        }
    }

    // --- Issue #536: workgroup 内部分 Top-k の GPU 非依存純関数テスト ---

    #[test]
    fn dot_shader_topk_wgsl_constants_match_host_constants() {
        // WGSL 側の定数（`WORKGROUP_SIZE`/`TOPK_OUT_MAX`/`QUERY_TILE_MAX`）が
        // ホスト側の `GPU_WORKGROUP_SIZE`/`GPU_TOPK_OUT_MAX`/`GPU_QUERY_TILE_MAX`
        // と一致することを固定する（両シェーダ共通）。
        let expect_wg = format!("const WORKGROUP_SIZE: u32 = {GPU_WORKGROUP_SIZE}u;");
        let expect_topk = format!("const TOPK_OUT_MAX: u32 = {GPU_TOPK_OUT_MAX}u;");
        let expect_tile = format!("const QUERY_TILE_MAX: u32 = {GPU_QUERY_TILE_MAX}u;");
        for shader in [DOT_SHADER_TOPK_WGSL, DOT_SHADER_TOPK_F32_WGSL] {
            assert!(shader.contains(&expect_wg), "missing {expect_wg}");
            assert!(shader.contains(&expect_topk), "missing {expect_topk}");
            assert!(shader.contains(&expect_tile), "missing {expect_tile}");
        }
    }

    #[test]
    fn score_key_round_trips_and_preserves_total_cmp_order() {
        let values: [f32; 12] = [
            0.0,
            -0.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN,
            f32::MAX,
            1.0,
            -1.0,
            1e-30,
            -1e-30,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
        ];
        for &v in &values {
            assert_eq!(
                score_from_key(score_key(v)).to_bits(),
                v.to_bits(),
                "round trip must be bit-identical for {v}"
            );
        }
        for &a in &values {
            for &b in &values {
                let want = a.total_cmp(&b);
                let got = score_key(a).cmp(&score_key(b));
                assert_eq!(got, want, "order mismatch for a={a} b={b}");
            }
        }
    }

    #[test]
    fn score_key_orders_negative_floats_correctly_unlike_signed_reinterpretation() {
        // 単純な i32 再解釈（ビットパターンをそのまま符号付き整数とみなす）では
        // 負の浮動小数点数同士の大小関係が逆転する（絶対値が大きいほど
        // マグニチュードのビットパターンは大きくなるため）。`score_key` は
        // この誤りを避けるための変換であることを固定する。
        assert!(score_key(-1.0) > score_key(-2.0));
        assert!(((-1.0f32).to_bits() as i32) < ((-2.0f32).to_bits() as i32));
    }

    #[test]
    fn select_readback_mode_falls_back_without_pipeline() {
        assert_eq!(select_readback_mode(false, 10), GpuReadbackMode::FullScores);
    }

    #[test]
    fn select_readback_mode_falls_back_when_k_exceeds_topk_out_max() {
        assert_eq!(
            select_readback_mode(true, GPU_TOPK_OUT_MAX as usize + 1),
            GpuReadbackMode::FullScores
        );
        assert_eq!(
            select_readback_mode(true, GPU_TOPK_OUT_MAX as usize),
            GpuReadbackMode::PartialTopK {
                k_out: GPU_TOPK_OUT_MAX as usize
            }
        );
    }

    #[test]
    fn select_readback_mode_falls_back_on_zero_k() {
        assert_eq!(select_readback_mode(true, 0), GpuReadbackMode::FullScores);
    }

    #[test]
    fn select_readback_mode_selects_partial_topk_when_available() {
        assert_eq!(
            select_readback_mode(true, 10),
            GpuReadbackMode::PartialTopK { k_out: 10 }
        );
    }

    #[test]
    fn gpu_topk_params_to_ne_bytes_vec_is_32_bytes_in_field_order() {
        let params = GpuTopKParams {
            row_stride: 1,
            row_count: 2,
            query_count: 3,
            query_stride: 4,
            k_out: 5,
        };
        let bytes = params
            .to_ne_bytes_vec()
            .expect("32 byte allocation must succeed");
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[0..4], &1u32.to_ne_bytes());
        assert_eq!(&bytes[4..8], &2u32.to_ne_bytes());
        assert_eq!(&bytes[8..12], &3u32.to_ne_bytes());
        assert_eq!(&bytes[12..16], &4u32.to_ne_bytes());
        assert_eq!(&bytes[16..20], &5u32.to_ne_bytes());
        assert_eq!(&bytes[20..32], &[0u8; 12]);
    }

    #[test]
    fn plan_partial_topk_chunk_rows_never_returns_zero() {
        assert!(plan_partial_topk_chunk_rows(16, 256, GPU_SCORE_BUFFER_BUDGET_BYTES, 1) >= 1);
        assert!(plan_partial_topk_chunk_rows(1, 1, 64, 1) >= 1);
    }

    #[test]
    fn plan_partial_topk_chunk_rows_shrinks_as_k_out_or_width_grows() {
        let base = plan_partial_topk_chunk_rows(1, 1, GPU_SCORE_BUFFER_BUDGET_BYTES, 65_535);
        let wider = plan_partial_topk_chunk_rows(
            GPU_QUERY_TILE_MAX,
            1,
            GPU_SCORE_BUFFER_BUDGET_BYTES,
            65_535,
        );
        let deeper_k = plan_partial_topk_chunk_rows(
            1,
            GPU_TOPK_OUT_MAX as usize,
            GPU_SCORE_BUFFER_BUDGET_BYTES,
            65_535,
        );
        assert!(wider <= base);
        assert!(deeper_k <= base);
    }

    #[test]
    fn merge_partial_topk_readback_rejects_length_mismatch() {
        let mut selectors = vec![Some(TopKSelector::new(1))];
        let err = merge_partial_topk_readback(&[0u32; 3], &[0u32], 1, 1, 1, &mut selectors)
            .expect_err("short readback must be rejected");
        assert!(matches!(err, BatchBackendError::TransferFailed(_)));
    }

    #[test]
    fn merge_partial_topk_readback_skips_sentinels() {
        let mut selectors = vec![Some(TopKSelector::new(2))];
        // 1 ワークグループ・k_out=2・sentinel 1 件 + 有効候補 1 件。
        let readback = vec![0u32, u32::MAX, score_key(1.5), 7u32];
        merge_partial_topk_readback(&readback, &[7], 1, 1, 2, &mut selectors)
            .expect("well-formed readback must merge");
        let hits = selectors
            .remove(0)
            .expect("selector must remain populated")
            .into_sorted_vec();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 7);
        assert!((hits[0].score - 1.5).abs() < 1e-6);
    }

    #[test]
    fn merge_partial_topk_readback_rejects_slot_outside_chunk() {
        let mut selectors = vec![Some(TopKSelector::new(1))];
        let readback = vec![score_key(1.0), 99u32];
        let err = merge_partial_topk_readback(&readback, &[7], 1, 1, 1, &mut selectors)
            .expect_err("slot outside the dispatched chunk must be rejected");
        assert!(matches!(err, BatchBackendError::TransferFailed(_)));
    }

    #[test]
    fn merge_partial_topk_readback_matches_direct_push_across_multiple_chunks() {
        // 複数チャンク・複数ワークグループにまたがる合成 readback を push した
        // 結果が「全候補を直接 push した TopKSelector」とビット同一であることを
        // 固定する（ADR 決定 3 の維持契約）。
        let candidates: Vec<(u32, f32)> = vec![(10, 3.0), (11, 1.0), (12, 5.0), (13, 5.0)];

        let mut direct = TopKSelector::new(10);
        for &(slot, score) in &candidates {
            direct.push(CandidateHit {
                id: u64::from(slot),
                score,
            });
        }
        let direct_sorted = direct.into_sorted_vec();

        // 2 チャンク（先頭 2 件・後半 2 件）× 1 ワークグループ・k_out=4 として
        // 合成する（各チャンクの候補数が k_out 以下なので全件そのまま出力される
        // 想定で読み替える単純化されたテスト readback）。
        let mut merged_selector = vec![Some(TopKSelector::new(10))];
        let chunk_a = [10u32, 11];
        let readback_a = vec![
            score_key(3.0),
            10,
            score_key(1.0),
            11,
            0,
            u32::MAX,
            0,
            u32::MAX,
        ];
        merge_partial_topk_readback(&readback_a, &chunk_a, 1, 1, 4, &mut merged_selector)
            .expect("chunk a merge must succeed");

        let chunk_b = [12u32, 13];
        let readback_b = vec![
            score_key(5.0),
            12,
            score_key(5.0),
            13,
            0,
            u32::MAX,
            0,
            u32::MAX,
        ];
        merge_partial_topk_readback(&readback_b, &chunk_b, 1, 1, 4, &mut merged_selector)
            .expect("chunk b merge must succeed");

        let merged_sorted = merged_selector
            .remove(0)
            .expect("selector must remain populated")
            .into_sorted_vec();

        assert_eq!(merged_sorted.len(), direct_sorted.len());
        for (a, b) in merged_sorted.iter().zip(direct_sorted.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.score.to_bits(), b.score.to_bits());
        }
    }
}
