# i8 パック常駐と `dot4I8Packed` シェーダ（Issue #542）

- 親: #541（Phase 5 親 #460・ルート #455）
- 依存: #521（対称 SQ8 量子化。実装済み・`sq8.rs` の共有層への統合は
  未実施・後続。下記「2.」節参照）
- 後続: #543（前後比較・Recall 影響の記録）

## 1. 背景・目的

既存の GPU バッチ経路（`gpu_batch.rs::GpuBatchBackend`）は f16 2 要素/u32
パック常駐（`batch_search.rs::ResidentMatrix::packed()`）を
`unpack2x16float` で f32 へ戻して積和する（2 byte/要素）。本 Issue は、
行単位対称 SQ8 量子化（各行の絶対値最大からスケールを独立に導出する。D3
改訂）で行を i8 へ落とし 4 要素/u32 でパックし、WGSL 組み込み
`dot4I8Packed` で整数内積を計算する経路（1 byte/要素）を追加する。

親 Issue #541 の制約により、量子化・低精度常駐は **opt-in 限定・候補生成の
みで最終スコアは f32 再計算**。既定経路（`FallbackBatchEngine::
build_with_gpu` の primary）へは接続しない。

## 2. #521 の実装状況（着手前の確認・追記）

本 Issue 実装時点（着手前確認）では #521（対称 SQ8 量子化・次元別 min/max）は
OPEN・実装未着手（`crates/engine/src/` に `Sq8`/`sq8`/量子化のトップレベル
共有モジュールが存在しないことを確認済み）だった。そのため本 Issue は
`gpu_batch/packed_i8.rs` モジュール内に対称 SQ8 エンコーダを自前で実装した
（`encode_rows`・`Sq8RowScales`。当初は次元別 `min`/`max` パラメータ
`Sq8DimParams` だったが §7 の PR レビュー対応で行単位スケールへ改めた）。

**追記（#521 実装後）**: #521 は `crates/engine/src/sq8.rs` に次元別 min/max
方式の共有量子化層（`Sq8DimParams { scales }`・`fit_dim_params`・
`encode_rows`）を実装し、`hnsw.rs::NodeVectors::I8` 専用に接続した
（詳細は `docs/design/hnsw-sq8-resident.md` 参照）。§7-1 で申し送った
テナント境界（次元別 min/max が ctx 可視アリーナのみから導出される）は Issue #521 の `IndexedBase::build` の既存構造（ctx 可視アリーナのみを受け取る）にそのまま追従する形で成立している。本モジュール（`packed_i8.rs`・GPU・行単位
対称スケール）の `sq8.rs` への統合は本追記時点では未実施のまま
（行単位／次元別で方式が異なり、どちらへ寄せるかは #522（VNNI／NEON
整数カーネル）実装後に判断する。§9 の申し送りを参照）。

## 3. 設計判断

| 論点 | 決定 |
| ---- | ---- |
| metric | **（§10 参照）** InnerProduct・Cosine・L2 に対応、L1 は非対応（`try_new_with_metric` が `InitFailed`） |
| 適用境界 | `FallbackBatchEngine::build_with_gpu`（本番 primary）は無変更。i8 経路は `GpuI8BatchBackend::try_new` からのみ構築する opt-in 専用バックエンド（`GpuF32ContrastBackend` の前例と同型） |
| 配置 | 新規子モジュール `crates/engine/src/gpu_batch/packed_i8.rs`。`gpu_batch.rs` への編集は `pub mod packed_i8;` と `GpuContext::backend: wgpu::Backend` フィールド追加の 2 点に限定 |
| 行の量子化 | **（D3 改訂・§7-1 参照）** 行 `i` ごとに独立なスケール `s_i = max_j(abs(x_{i,j})) / 127`。`xq_{i,j} = round(x_{i,j}/s_i)` を `[-127, 127]` へクランプ（`-128` は使わない）。`s_i == 0`（零行）は除算せず `xq_{i,j} = 0`。他行（他テナントの不可視行を含む）の値には一切依存しない |
| パック規約 | 4 レーン/u32。レーン `j`（`j = 0..4`）を bit `8j..8j+7` に格納（下位から詰める規約。`pack_f16x2` と同じ方向）。`row_stride = dim.div_ceil(4)`。末尾パディングレーンは 0 |
| クエリ側の量子化 | **（D5 改訂・§7-1 参照）** クエリ自身の成分だけから対称スケール `s_q = max_i(abs(q_i))/127` を求めて量子化する（詳細は本節末尾の式参照）。行パラメータには一切依存しない |
| オーバーフロー | `MAX_BATCH_DIM(8,192) × 127 × 127 = 132,129,792 < 2^31`。単体テスト `dot_i8_packed_ref_extreme_values_stay_within_i32_bound` で固定 |
| CPU 参照実装 | `packed_i8::dot_i8_packed_ref`（レーン展開＋i32 累積の純関数）。#522（VNNI）はこの参照実装とビット同一であることを契約とする |
| 候補生成＋再スコア | **（§7-2 参照）** GPU は i32 整数内積を返す。ホストは行スケール `s_i` を掛けた近似内積（`i32_score × s_i`）降順（同点は slot 昇順）でチャンクごとに逐次縮約し、`k' = min(reachable_rows, k × oversample)` 件だけを保持 → `ResidentMatrix::row_f32_into`（本 Issue で `pub(crate)` 化）で f32 復号し `kernel::dot` で再スコア → 上位 `k` 件へ切り詰め → 既存 `finalize_gpu_hits` で解決 |
| 量子化入力の出所 | `try_new(matrix, options)` は元の f32 ベクトル列を別引数で受け取らず、`matrix` 自身の全行を `row_f32_into` でデコードして量子化入力にする。別引数にすると呼び出し元が渡す行順とスロット順が食い違うリスクがあるため、`matrix` を単一の真値にした（D8 の f32 再スコアが参照する真値とも一致） |
| oversample | `GpuI8Options { oversample: usize }`。既定 `DEFAULT_I8_OVERSAMPLE = 4`・上限 `MAX_I8_OVERSAMPLE = 32`（範囲外は `InitFailed`）。環境変数では読まない（CORE-12） |

クエリ側の量子化の式（表の「クエリ側の量子化」行の詳細。D5 改訂後）:

1. スケール = クエリ成分の絶対値の最大 ÷ 127
2. 各成分をスケールで割って丸め、`[-127, 127]` へクランプ
   （スケールが 0 なら全成分 0）

近似内積: GPU が返す整数内積 `Σ qq_i·xq_i` に行スケール `s_i` とクエリ
スケール `s_q` を掛けると `s_i · s_q · Σ qq_i·xq_i` が近似内積になるが、
同一クエリ内の候補順位付けでは `s_q` は全候補に共通の正の定数なので、
順位付けには `s_i` だけを掛ければ足りる（`packed_i8.rs::ranking_key`）。
D3 改訂前は次元別 `center`（行に依存しない次元別パラメータ）との内積を
定数項として省略する設計だったが、行単位スケール方式にはこの「次元別
center」という概念自体が存在しないため、この定数項の議論は D3 改訂で
消滅した。
| メモリ | GPU 側は i8 パック（1 byte/要素）のみ常駐。CPU 側は再スコア・スロット解決のため f16 `ResidentMatrix` を保持する |
| readback 方式 | 全量 readback のみ（i8 用 Top-k シェーダは本 Issue では作らない。#411 系の申し送りと同型） |
| クエリタイル化 | 本実装は 1 dispatch = 1 クエリに単純化した（既存 f16/f32 経路が持つ `GPU_QUERY_TILE_MAX` タイル化は本 Issue の対象外。#543 以降で必要なら追加） |
| meta | `GpuI8Meta { backend: wgpu::Backend, dot4_impl: Dot4I8Impl }`。`Dot4I8Impl` は `Native`／`Polyfill`／`Undetermined` の閉じた列挙で、下記 §4 の理由により本実装は常に `Undetermined` を返す |
| 統計 | `GpuI8BatchStatsSnapshot { dispatches, readback_bytes, rescored_candidates }` |

## 4. `Dot4I8Impl` が常に `Undetermined` である理由

`wgpu-types 30.0.1` の `Features` に `NATIVE_PACKED_INTEGER_DOT_PRODUCT`
相当の定数は存在しない。naga 30.0.1 の SPIR-V backend は
`DotProduct`／`DotProductInput4x8BitPacked` capability の有無で `OpSDot`
と `BitFieldSExtract` polyfill を切り替えるが、その capability は
wgpu-hal Vulkan が private capability（`VK_KHR_shader_integer_dot_product`
／API 1.3 昇格）から設定する。公開経路は `Adapter::as_hal`（`unsafe`）
のみで、本リポは `unsafe` 原則禁止（`tests/isa.rs` が個数・局在を固定）の
ため使えない。専用命令・polyfill のいずれでも整数結果は同一（`dot4I8Packed`
の仕様上の契約）なので、整数一致テストの正しさは判別結果に依存しない。

backend 別の専用命令の条件（naga／wgpu-hal 30.0.1 の実装コードから確認。
本実装では判別せず記録のみ）:

| backend | 専用命令の条件 |
| ---- | ---- |
| Vulkan | `VK_KHR_shader_integer_dot_product`（API 1.3 で昇格）の `shaderIntegerDotProduct` が true のとき `OpSDot`、それ以外は `BitFieldSExtract` polyfill |
| DX12 | Shader Model 6.4 以上で `dot4add_i8packed`、未満は polyfill |
| Metal | MSL 2.1 以上で `packed_char4` の内積、未満は polyfill |

## 5. 実測（本開発環境: RTX 3060・Vulkan backend）

`crates/engine/tests/gpu_batch_i8.rs`（`bench-internals` feature）を実
GPU 上で実行し、以下を確認した:

- 生の i32 スコア（`batch_search_raw_i32_for_tests`）が CPU 参照実装
  （`dot_i8_packed_ref`）と**完全一致**（奇数次元 dim=5 を含むフィクスチャ）
- 混在テナントバッチで他テナントの id が混入しない
- `stats()` が非 vacuous（`dispatches >= 1`・`rescored_candidates >= 1`）
- 同一入力の 2 回実行が id・スコア列とも一致（決定的）
- `meta().dot4_impl == Undetermined`

既存 GPU テスト（`tests/gpu_batch.rs`・`tests/batch_fallback.rs`・
`tests/gpu_scaling_accept.rs`）はいずれも本 Issue の変更後も green
（受け入れ条件「既存 GPU テストが green」を満たす）。

性能の前後比較・Recall 影響の記録は #543 の担当（本 Issue では実施しない）。
実施結果は「前後比較実測（Issue #543）」節参照。

## 6. スコープ外・申し送り

- #521 マージ後のエンコーダ統合（本 Issue で置いた `packed_i8.rs` 内エンコー
  ダを #521 の共有層へ寄せる）
- i8 用 workgroup 内部分 Top-k シェーダ（i32 キーの bitonic 網）は未実装
  （#543 実測により readback 量が f16 比約 12.8 倍多いことが定量的に判明。
  §8「原因分析」参照。実装自体は引き続き未着手）
- クエリタイル化（1 dispatch = 複数クエリ。既存 f16/f32 経路の
  `GPU_QUERY_TILE_MAX` 相当）は未実装（同じく §8「原因分析」で速度低下の
  主因の一つと分析。実装自体は引き続き未着手）
- oversample 既定値の調整（#543 実測で推奨値〔既定 4 のまま〕を記録。
  production コード〔`DEFAULT_I8_OVERSAMPLE`〕の変更は見送り。§8「oversample
  の推奨値」参照）
- `bench-gpu-scaling` への i8 経路追加と前後比較表（#543 で実施済み。
  §8 参照）
- wgpu 更新時に判別可能な feature が追加された場合の `Dot4I8Impl` 判定の実装
- `FallbackBatchEngine` への i8 primary 接続は行わない（既定経路不変・#541
  契約）

## 7. PR #598 レビュー対応

codex-review の指摘 3 件（P0 1 件・P1 2 件）はいずれも修正済み
（`crates/engine/src/gpu_batch/packed_i8.rs`）。

### 7-1. P0: 量子化パラメータのテナント境界侵害

当初実装は次元別 `min`/`max`（`ResidentMatrix` の全行・全テナント横断）
から `center`/`alpha` を導出していたため、他テナントの不可視行の値・存在
が量子化スケール経由で候補選出（ひいては検索結果）に影響しうるテナント
境界侵害だった。行 `i` 単独から決まる対称スケール
`s_i = max_j(|x_{i,j}|)/127` へ変更し（`Sq8DimParams` → `Sq8RowScales`）、
他行（他テナントの不可視行を含む）の値に一切依存しない構成にした
（§3 の「行の量子化」「クエリ側の量子化」行を参照）。

検証は CPU オンリーの回帰テスト
`packed_i8::tests::encode_rows_row_scale_is_independent_of_other_rows_cross_tenant_leak_regression`
で固定した: 同じ行列に同居する他テナントの行を極端な外れ値へ差し替えても、
対象行のスケール・パック済みバイト列がビット同一であることを確認する
（GPU 不要）。

**#521（対称 SQ8 量子化）が次元別 `min`/`max` 方式を踏襲する場合は同種の
境界問題を引き継ぐため、#521 実装時にオーナーへ申し送りが必要**
（§2 にも記載）。

### 7-2. P1: `raw_i32_scores` の無制限メモリ保持

当初実装は `raw_i32_scores` がクエリごとに到達行**全件**の `(slot, score)`
を一括保持していた。`dim` が小さい入力（例: dim=1・到達行 100 万・4,096
クエリ・各 k=1）では既存の件数・`sum(k)`・`MAX_BATCH_WORK` 検証を通過し
つつ約 32.768GB のメモリを要求しうる構造で、`Vec::push`/`clone`（未予約）
も確保失敗時に abort するため fail-closed 契約に反していた。

チャンク受信のたびに [`reduce_top_k_prime`] で逐次縮約し、保持量を
`k' = min(reachable_rows, k × oversample)` 件（＋直近チャンクの行数）以内
に抑える方式へ変更した。ストリーミング top-k の標準的な性質
`TopK(A∪B, k) == TopK(TopK(A,k)∪B, k)` により、逐次縮約後の最終集合は
「全件を一括保持してから 1 回だけ選出した場合」と一致する（正当性の証明は
コード内コメント参照）。バッチ全体での保持量の上限は
`Σk' <= MAX_BATCH_TOTAL_K(1,000,000) × MAX_I8_OVERSAMPLE(32) = 32,000,000`
要素（既存の `sum(k)` 上限・oversample 範囲検証から導かれる）。すべての
新規確保は `try_reserve_exact` によるフォールブル確保とし、`batch_search`
側の未予約 `clone`（旧・791 行相当）は撤去した（縮約済みの結果をそのまま
再利用する）。

行単位スケール化（§7-1）に伴い、チャンク内の縮約・最終選出のキーも
「生の i32 スコア降順」から「行スケールを掛けた近似内積降順」
（`ranking_key`／`sort_by_ranking_key`）へ変更した（行をまたぐ生の i32
スコアは、行ごとにスケールが異なるためそのままでは比較できない）。

### 7-3. P1: 量子化の中間演算オーバーフロー

`encode_rows`/`quantize_query` の量子化（除算・丸め）を f32 のみで行うと、
極端な入力（非常に大きい／小さい有限値）で中間演算が非有限値
（NaN/Inf）になり、`NaN as i8 == 0` へ暗黙に丸まって本来有効なスコアが
無言で無効化されうる欠陥があった。除算・丸めを f64（`row_scale`・
`quantize_scalar`）で行い、桁数に余裕を持たせたうえで、それでも中間結果が
非有限になった場合は `I8EncodeError::NonFinite` を明示的に返すフェイル
クローズへ変更した。

### 検証

- `cargo test -p engine --features bench-internals,contrast-bench --lib gpu_batch::packed_i8`
  （CPU オンリーの単体テスト。§7-1 の回帰テストを含む）
- `cargo test -p engine --features bench-internals,contrast-bench --test gpu_batch_i8`
  （実 GPU がある本開発環境〔RTX 3060・Vulkan backend〕で実走。既存の
  CPU 参照実装一致・混在テナント非漏えいテストは無変更のまま green）
- `cargo clippy -p engine --all-targets --features bench-internals,contrast-bench -- -D warnings`

## 8. 前後比較実測（Issue #543）

### 前提

- before: `2d2c74e`（#542 適用直前）／after: 本 PR の作業ブランチ
  （`e14d53f`〔#542 マージコミット〕以降・`git diff e14d53f <作業ブランチ>
  -- crates/engine/src/` が空であることを確認済み）。`git diff 2d2c74e
  e14d53f -- crates/engine/src/gpu_batch.rs crates/engine/src/lib.rs` の
  差分は `pub mod packed_i8;`・`GpuContext::backend` フィールド追加のみで、
  既存 f16/f32 経路の dispatch・シェーダは不変。`Cargo.lock` は両コミット間で
  無変更（`git diff 2d2c74e e14d53f -- Cargo.lock` が空）。
- i8 経路（`packed_i8.rs`）は `e14d53f` より前には存在しないため、i8 の
  「before」は作れない（実測できない）。before バイナリでの `gpu_scaling_i8:`
  行は全 run で不在であることを確認済み（既存 `gpu_scaling:`／
  `gpu_scaling_stats:` の非退行〔層 1〕は本 Issue の主題ではないため個別の
  前後比較表は作らず、以下の層 2（同一 after バイナリ内の A/B/D 比較）のみ
  記録する）。
- 環境: 本開発環境（共有 QEMU VM・NVIDIA GeForce RTX 3060・Vulkan backend・
  12 vCPU・計測中の loadavg 約 3.6〜8.5）。`BENCH_DEDICATED_ENV` 未設定の
  共有環境であり、`benchmark-judgement-policy.md` §5 により**レイテンシ
  数値は参考値・採否根拠にしない**。Recall・mismatch・readback バイト数・
  再スコア候補数は実行のたびに一意に定まる確定的指標のため、本環境でも
  確定的に判定できる。
- 規模点: `20000:128:8`（GPU 転送・dispatch の固定コストが支配的な小規模
  点）・`100000:128:64`（`crossdb-bench.md` GPU 節と同一の中規模点）・
  `100000:256:64`（i8 は f16 比でバイト/要素が半減するため dim 拡大点を
  1 点含める）の 3 点。ペア数 N=5（交互 before→after。既定 oversample=4。
  `scripts/bench_gpu_scaling_ab.sh`）。`500000:128:64` 等の追加規模点・
  oversample のフル N=5 スイープ（{1,2,8}）は計測時間の都合で本実測の
  スコープ外とした（下記「oversample の推奨値」節参照。スコープ外事項として
  記録）。生データは
  `docs/design/bench-data/gpu-scaling-ab/20260907T084600Z-summary.tsv`
  （31 行＝3 点 × 5 ペア × 2 側 + ヘッダ）・
  `docs/design/bench-data/gpu-scaling-ab/20260907T084600Z-i8-stats.txt`
  （規模点ごとの `gpu_scaling_i8:`／`gpu_scaling_i8_stats:` 行の抜粋）に
  保持。

### 確定的指標: Recall・不一致件数・読み戻し量（環境ノイズの影響を受けない）

CPU-SIMD（A）厳密対照に対する i8 経路（D。既定 oversample=4）の平均
Recall@10・同点許容つき不一致件数は、3 規模点 × 5 ペア（after 側 15 run）
の**全 run で完全に同一の値**だった:

| rows | dim | batch | oversample | i8_recall_at_k（全 5 run） | i8_mismatch（全 5 run） |
| --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 8 | 4 | 1.0000 | 0 |
| 100,000 | 128 | 64 | 4 | 1.0000 | 0 |
| 100,000 | 256 | 64 | 4 | 1.0000 | 0 |

**Recall@10 の低下は 1 件も観測されなかった**（3 規模点 × 5 ペアの全 15
run で `i8_recall_at_k = 1.0000`・`i8_mismatch = 0`）。`crates/engine/
tests/gpu_batch_i8.rs` の受け入れ基準（brute-force 対照 Recall@10 ≥ 0.9・
oversample 増加で非減少）とも整合する。

読み戻し量・再スコア候補数（規模点ごと 1 run の代表値。同一規模点内で
`calls`・`readback_bytes_total`・`rescored_candidates_total` は決定的に
一致する——`oversample` が構築時固定でクエリ内容に依存しないため）:

| rows | dim | batch | readback_bytes/call | rescored_candidates/call | backend | dot4_impl | build_ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 8 | 640,000 | 320 | Vulkan | Undetermined | 20 |
| 100,000 | 128 | 64 | 25,600,000 | 2,560 | Vulkan | Undetermined | 91 |
| 100,000 | 256 | 64 | 25,600,000 | 2,560 | Vulkan | Undetermined | 175 |

readback バイト数は `dim` に依存せず（i8 dispatch が読み戻すのは i32
スコア配列のみでベクトル次元数を含まない）、`rows × batch × 4`（到達可能な
**全行**の生 i32 スコアをそのまま readback し、`k' = k × oversample` 件への
縮約は CPU 側〔`reduce_top_k_prime`〕で行う構造。`packed_i8.rs::
dispatch_i8_dot_products`／`raw_i32_scores` 参照）で決まる。既存 f16/f32
経路が Issue #536 で獲得した「GPU 側 workgroup 内部分 Top-k による readback
削減」を i8 経路は持たない——同一規模点の f16 readback（`gpu_scaling_stats:`
行。100000:128:64 で 2,001,920 bytes/call）と比べて i8 は**約 12.8 倍多い**
（25,600,000 bytes/call）。`dot4_impl` は wgpu 30.0.1 の制約により全 run で
`Undetermined`（§4「`Dot4I8Impl` が常に `Undetermined` である理由」節参照。
native/polyfill の A/B は本実測のスコープ外）。

### レイテンシ（参考値・共有 QEMU 環境のため採否根拠にしない）

`gpu_i8_p95`（after 側。min-of-5・median）と、同一 after バイナリ内の A
（CPU-SIMD）・B（GPU f16 常駐）の p95 を突き合わせたレイテンシ比
（下表の「ratio i8/A」「ratio i8/B」列 = i8_p95 / 対照 p95〔min-of-N〕。
1.0 超は i8 の方が遅いことを表す）。この比は `gpu_scaling_bench.rs` が
自ら出力する `speedup_i8_vs_cpu_p95`/`speedup_i8_vs_f16_p95`
フィールド（`harness/gpu_scaling.rs::speedup_ratio` = 対照 p95 / i8_p95。
1.0 未満が i8 の方が遅いことを表す）とは分子・分母が逆で値も逆数の
関係にある——本節の表はレイテンシ比（i8/対照）として独自に算出した
ものであり、ベンチ出力の `speedup_ratio` フィールドをそのまま転記した
ものではない。参照区間帯は pooled `cpu_p50`（before + after 両側。
`isa.rs`／`batch_search.rs` は本 Issue で無変更のため純粋な
run-to-run ノイズの目安）。

| rows:dim:batch | i8_p95（min/median） | A（cpu）p95（min） | B（f16）p95（min） | ratio i8/A（min-of-N） | ratio i8/B（min-of-N） | 参照区間帯 |
| --- | --- | --- | --- | --- | --- | --- |
| 20000:128:8 | 5,486 / 6,196 µs | 5,047 µs | 535 µs | 1.087x（**遅い**） | 10.254x（**遅い**） | 11.32% |
| 100000:128:64 | 306,634 / 310,024 µs | 82,869 µs | 15,810 µs | 3.700x（**遅い**） | 19.395x（**遅い**） | 11.62% |
| 100000:256:64 | 336,079 / 361,815 µs | 137,090 µs | 21,899 µs | 2.452x（**遅い**） | 15.347x（**遅い**） | 121.0%（1 run の外れ値〔loadavg スパイク〕で拡大。実測帯も超過） |

`docs/design/benchmark-judgement-policy.md` §4 の判定規約（固定相対帯
±5%・実測帯の両方を超える場合のみ悪化と判定し、片方のみを超える差分は
「ノイズ帯内」として記録し採否根拠にしない）に照らすと、対 B（GPU f16
常駐）の比較は 3 規模点すべてが固定相対帯・実測帯の両方を明確に超える
一貫した悪化方向であり（比 10.3〜19.4x・`|ratio-1|` は 925〜1840%。いずれも
参照区間帯 11.32〜121.0% を大きく上回る）、共有環境のノイズでは説明できない。

対 A（CPU-SIMD）の比較は規模点ごとに判定が分かれる。`20000:128:8` は
`|ratio-1| = 8.7%` で参照区間帯 11.32% を**下回り**実測帯の要件を満たさない
ため、固定相対帯（5%）のみ超過の「ノイズ帯内」として記録し、この 1 点の
対 A 比較は悪化の根拠にしない。一方 `100000:128:64`（`|ratio-1| = 270%` ＞
帯 11.62%）・`100000:256:64`（`|ratio-1| = 145.2%` ＞ 帯 121.0%。帯自体が
1 run の外れ値〔loadavg スパイク〕で拡大しているが、それでもなお上回る）は
両ノイズ帯を超えており、対 A でも規模拡大に伴い明確な悪化が観測される。

総括: 対 B は 3 規模点全域で明確な悪化、対 A は大規模 2 点（100k 行）で
明確な悪化・小規模 1 点（20k 行）はノイズ帯内。速度低下の傾向自体は
規模拡大とともに悪化する一貫した方向であり、下記「原因分析」の構造要因
（readback 量・クエリタイル化未実装）と整合する。

### 原因分析

速度低下は 2 つの既知のスコープ外事項（#542 doc §6「i8 用 workgroup 内
部分 Top-k シェーダは未実装」「クエリタイル化は未実装」）に起因すると
分析する。

1. **readback 量が f16 の約 12.8 倍**（上記「確定的指標」節）。i8 経路は
   `raw_i32_scores`／`dispatch_i8_dot_products` が到達可能な**全行**の
   生 i32 スコアを readback してから CPU 側で `k' = k × oversample` 件へ
   縮約する構造で、Issue #536 が f16/f32 経路に追加した「workgroup 内
   部分 Top-k による readback 削減」を持たない。つまり i8 経路の readback
   構造は #536 適用**前**の f16/f32 経路（`docs/design/gpu-batch-topk.md`
   の before 相当）と同型であり、あらためて readback 削減を実装すれば
   同種の改善（実測 12.66〜12.79x）が見込める。
2. **「1 dispatch = 1 クエリ」のまま**（`packed_i8.rs::DOT_SHADER_I8_WGSL`
   ドキュメンテーションコメント「D11 の申し送り」参照）。既存 f16/f32
   経路は Issue #532（クエリタイル化。最大 `GPU_QUERY_TILE_MAX` 本を
   1 dispatch へ束ねる）で GPU dispatch・常駐行列読み込みの固定コストを
   複数クエリで償却できるようになったが、i8 経路にはこの償却機構がなく、
   `batch >= 8` の全規模点で `1 dispatch/query` の固定オーバーヘッドが
   バッチサイズに比例して積み上がる。

両要因とも本実測で初めて定量的な裏付け（readback 12.8 倍・速度比
1.09〜19.4x）が得られた。行単位対称 SQ8 量子化そのもの（1 byte/要素の
常駐サイズ削減）は本実測の対象外だが、readback・dispatch 構造の 2 点を
是正しない限り、常駐サイズ削減の恩恵は速度実測には現れないと考えられる。

### oversample の推奨値

構築時固定オプションのため、本実測（既定 oversample=4）に加え、`crates/
engine/tests/gpu_batch_i8.rs::
i8_backend_recall_is_monotone_non_decreasing_in_oversample_when_gpu_
available`（2,000 行・dim 128 のクラスタ構造ありコーパス）で oversample を
1・4・8 と振った際の Recall@10 が非減少である
ことを確認済み（同テストは `make ci` 対象として本 PR に含まれる）。加えて
2,000 行・dim 128・batch 8 の単発スモーク実測で oversample 1 → 2 → 4 → 8
の平均 Recall@10 が 0.9875 → 1.0000 → 1.0000 → 1.0000 と単調に改善する
ことを確認した（N=5 の正式なペア実測はこの oversample スイープでは実施
していない——時間予算の制約によりスコープ外。下記「スコープ外・申し送り」
参照）。

以上より、**既定値 4 は Recall の観点からは十分に安全**（本実測 3 規模点
すべてで Recall@10=1.0000）である一方、**レイテンシの観点では oversample
を下げても i8 経路自体が既存 2 経路（A・B）より大幅に遅いという結論は
変わらない**と見込まれる（原因はクエリタイル化の欠如であり oversample
とは独立の要因のため）。`DEFAULT_I8_OVERSAMPLE` の変更は本 Issue（テスト・
ベンチ・docs 専任）のスコープ外とし、production コード（`packed_i8.rs`）
は無変更のまま維持する。

### 判定

- **Recall 影響**: 3 規模点 × 5 ペアの全 run で `i8_recall_at_k=1.0000`・
  `i8_mismatch=0`。既定 oversample=4 で Recall 劣化は一切観測されなかった
  （確定的指標）。
- **速度**: 共有環境の参考値としては 3 規模点すべてで i8 経路が A（CPU-
  SIMD）・B（GPU f16 常駐）より遅い（比 1.09〜19.4x）。ただし
  `benchmark-judgement-policy.md` §4 の両ノイズ帯判定では、対 A の
  `20000:128:8` の 1 点は実測帯を下回り「ノイズ帯内」（悪化の根拠にしない）。
  対 B は 3 規模点全域、対 A も大規模 2 点は両ノイズ帯を明確に超える
  悪化であり（詳細は「レイテンシ」節参照）、原因はクエリタイル化未実装と
  いう既知のスコープ外事項（§6「申し送り」）に帰着すると分析した。
- 本実測は #542 の設計判断（候補生成専用・opt-in・primary 未接続）を
  変更する根拠にはならない——i8 経路は既定経路に一切接続されておらず、
  本実測結果は速度改善が必要になった場合の後続実装（クエリタイル化）の
  優先度判断材料として記録する。

### 再現手順（前後比較）

1. before バイナリを別 worktree・別 `CARGO_TARGET_DIR` で退避:
   `git worktree add <dir> 2d2c74e && cd <dir> && CARGO_TARGET_DIR=<target>
   cargo bench --bench gpu_scaling_bench -p engine --no-run
   --message-format=json` を実行し `executable` を抽出する。
2. after バイナリは作業ブランチで同様にビルドする。
3. 交互実行: `BEFORE_BIN=<path> AFTER_BIN=<path> OUT_DIR=<dir>
   I8_OVERSAMPLE=4 scripts/bench_gpu_scaling_ab.sh 5 20000:128:8
   100000:128:64 100000:256:64`
4. 集計: `summary.tsv` の `i8_*` 列を `rows:dim:batch` × `side` でグルー
   ピングし、min-of-N・median・`ratio = i8_p95_min / {cpu,f16}_p95_min`・
   pooled `cpu_p50` の `reference_band` を算出する。

## 9. スコープ外・申し送り（Issue #543）

- `500000:128:64` 等の追加規模点（i8 は 1 run あたり最大約 2.2 秒〔p50〕・
  N=5 ペアで数分規模になり、本実測の時間予算では計測時間の都合で見送った）
- oversample {1, 2, 8} の正式な N=5 ペア実測（上記スモーク実測で単調性は
  確認済みだが、参照区間帯を伴う正式な前後比較表は未作成）
- `DEFAULT_I8_OVERSAMPLE` の変更（推奨値の記録のみ。採否は専有環境再実測
  後にオーナー判断）
- 専有環境（`BENCH_DEDICATED_ENV=1`）でのレイテンシ再実測（オーナー作業）
- i8 経路のクエリタイル化・workgroup 内部分 Top-k（#542 doc §6 の既存申し
  送りだが、本実測により定量的な優先度判断材料が追加された）

## 10. metric 対応（Issue #2004・親 #2002・CORE-8・CORE-16・SQL-33 ポインタ）

### 判断

i8 経路は候補生成専用（D8）のため、metric ごとに変えるのはホスト側の**候補選出キー**と**最終 f32 再スコア**だけで、WGSL（整数内積）は無変更。行単位対称 SQ8 は行スケールに対して不変なので、コサイン用の正規化コピーは持たない。

| metric | 候補選出キー（大きいほど近い） | 最終スコア | 判断 |
| ---- | ---- | ---- | ---- |
| InnerProduct（`<#>` 負の内積を含む） | `Σ·s_i`（従来と同一） | `dot` | 対応（挙動不変） |
| Cosine | `Σ·s_i / sqrt(‖x_i‖²)`（`s_q`・`‖q‖` は定数なので省く） | `-(1 - cos)` | 対応 |
| L2 | `2·s_i·s_q·Σ - ‖x_i‖²`（f64。`s_q` は加法項と組むため省けない） | `-l2_sq` | 対応（下記ゲート合格） |
| L1 | 内積へ分解できない | - | 非対応。`GpuI8BatchBackend::try_new_with_metric` が GPU に触れる前に `InitFailed("i8 path does not support this metric")` |

- 公開 API: `engine::gpu_batch::GpuBatchMetric`（親モジュール。#2003 が f32/f16 経路で再利用する）、`GpuI8BatchBackend::try_new_with_metric`。`try_new` は `InnerProduct` への委譲で互換、`GpuI8Options` は不変。
- 行ノルム²は再スコアと同じ真値（f16 往復デコード行）から行自身だけで求める（他行・他テナントに非依存。D3 の境界契約を維持）。Cosine／L2 のときだけ行あたり 4 byte 追加で保持する。
- ゼロノルム: 行は候補キー `-inf`・最終スコア NaN で除外、クエリがゼロノルムなら結果は空（バッチ層の「非有限スコアは除外」契約）。SQL の `<=>`（NaN を末尾に残す）との差の統一は #2002 の判断事項。

### 判定ゲートと実測（CPU・GPU 不要）

GPU の整数結果は `dot_i8_packed_ref` とビット一致する契約のため、量子化誤差と候補 Recall は CPU で決定的に測れる。ゲートは既存の i8 Recall テストと同じ実装既定値で、「全フィクスチャ × 次元で oversample=4 の Recall@10 >= 0.9、かつ oversample 1→2→4→8 で単調非減少」。単体テスト `precision_tests` が、`i8_supports_metric` が true の metric すべてについてこれを機械検証する（対応表と実測が乖離するとテストが落ちる）。

条件: 2,000 行・クエリ 32 本（半数は分布抽出、半数は既存行 + 小ノイズ）・k=10・固定シード・真値は f16 往復デコード行への f32 厳密総当たり（同点許容）。F1 一様 [-1,1]、F2 クラスタ、F3 全成分に +3.0 のオフセット（非中心化）、F4 行ノルムを 10^U(-2,2) 倍にばらつかせた分布。誤差は先頭 64 行 × クエリの標本で、InnerProduct は近似内積の絶対誤差、Cosine は近似コサイン類似度の絶対誤差、L2 は近似二乗距離の相対誤差（mean／p99／max）。

| metric | fixture | dim | Recall@10 os=1 | os=2 | os=4 | os=8 | 誤差 mean | p99 | max |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| InnerProduct | F1 | 128 | 0.9969 | 1.0000 | 1.0000 | 1.0000 | 0.016702 | 0.055083 | 0.078420 |
| Cosine | F1 | 128 | 0.9969 | 1.0000 | 1.0000 | 1.0000 | 0.000392 | 0.001296 | 0.001813 |
| L2 | F1 | 128 | 0.9969 | 1.0000 | 1.0000 | 1.0000 | 0.000412 | 0.001316 | 0.036482 |
| InnerProduct | F1 | 768 | 0.9875 | 1.0000 | 1.0000 | 1.0000 | 0.041488 | 0.128897 | 0.190631 |
| Cosine | F1 | 768 | 0.9906 | 1.0000 | 1.0000 | 1.0000 | 0.000162 | 0.000506 | 0.000763 |
| L2 | F1 | 768 | 0.9969 | 1.0000 | 1.0000 | 1.0000 | 0.000217 | 0.000518 | 0.111907 |
| InnerProduct | F2 | 128 | 0.9906 | 1.0000 | 1.0000 | 1.0000 | 0.018453 | 0.057886 | 0.089293 |
| Cosine | F2 | 128 | 0.9031 | 1.0000 | 1.0000 | 1.0000 | 0.000427 | 0.001357 | 0.001930 |
| L2 | F2 | 128 | 0.9156 | 1.0000 | 1.0000 | 1.0000 | 0.001280 | 0.018631 | 0.723575 |
| InnerProduct | F2 | 768 | 0.9906 | 1.0000 | 1.0000 | 1.0000 | 0.049930 | 0.154701 | 0.200448 |
| Cosine | F2 | 768 | 0.9156 | 1.0000 | 1.0000 | 1.0000 | 0.000189 | 0.000585 | 0.000780 |
| L2 | F2 | 768 | 0.9156 | 1.0000 | 1.0000 | 1.0000 | 0.000478 | 0.007978 | 0.075816 |
| InnerProduct | F3 | 128 | 0.9656 | 1.0000 | 1.0000 | 1.0000 | 0.315068 | 1.017818 | 1.383992 |
| Cosine | F3 | 128 | 0.8937 | 1.0000 | 1.0000 | 1.0000 | 0.000264 | 0.000848 | 0.001125 |
| L2 | F3 | 128 | 0.8812 | 1.0000 | 1.0000 | 1.0000 | 0.007513 | 0.024024 | 0.365161 |
| InnerProduct | F3 | 768 | 0.9719 | 1.0000 | 1.0000 | 1.0000 | 0.894826 | 2.823392 | 4.239602 |
| Cosine | F3 | 768 | 0.9312 | 1.0000 | 1.0000 | 1.0000 | 0.000125 | 0.000394 | 0.000590 |
| L2 | F3 | 768 | 0.9094 | 1.0000 | 1.0000 | 1.0000 | 0.003687 | 0.011101 | 0.473031 |
| InnerProduct | F4 | 128 | 0.9969 | 1.0000 | 1.0000 | 1.0000 | 4.707606 | 96.235581 | 275.371414 |
| Cosine | F4 | 128 | 0.9938 | 1.0000 | 1.0000 | 1.0000 | 0.000400 | 0.001338 | 0.001831 |
| L2 | F4 | 128 | 0.9937 | 1.0000 | 1.0000 | 1.0000 | 0.000343 | 0.000927 | 0.463205 |
| InnerProduct | F4 | 768 | 0.9937 | 1.0000 | 1.0000 | 1.0000 | 3.209642 | 66.875791 | 312.701576 |
| Cosine | F4 | 768 | 0.9844 | 1.0000 | 1.0000 | 1.0000 | 0.000162 | 0.000536 | 0.000721 |
| L2 | F4 | 768 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 0.000088 | 0.000365 | 0.082753 |

読み取り:

- 3 metric とも全条件でゲートに合格した（os=4 で Recall@10 は全点 1.0000）。oversample=1 でも最小 0.8812（L2・F3・dim 128）で、候補幅 2 倍以上で 1.0 に到達する。したがって L2 も対応とした。
- InnerProduct の絶対誤差は値のスケール（行ノルム・オフセット）に比例して大きく見える（F3・F4）が、Recall には現れない。Cosine の誤差は 1e-3 台以下、L2 の相対誤差は p99 で 2.4% 以下。L2 の max（最大 0.72）は距離が 0 に近い近傍標本で分母が小さい場合の相対誤差である。
- 非中心化データ（F3）が最も厳しい（os=1 で Cosine 0.89・L2 0.88）が、候補生成専用のため oversample=2 以上で解消する。

再現: `cargo test -p fandhe-db-engine --all-features --lib gpu_batch::packed_i8::precision_tests -- --nocapture`

### GPU 実機の確認

結合テスト `tests/gpu_batch_i8.rs` に、Cosine・L2 の GPU Recall・テナント分離・決定性の検査（GPU 搭載環境のみ動作）と、非対応 metric の拒否（GPU 不要）を追加した。#2004 時点では GPU 実機が使えず未検証だったが、Issue #2005 で RTX 3060（Vulkan）上にて実行し、`tests/gpu_batch_i8.rs` 9 件が pass、`gpu_scaling_bench` の metric 別計測（内積・L2・コサイン、oversample=4、全 24 点）で i8 の Recall@10 = 1.0000・`i8_mismatch` = 0 を確認した。metric 別のレイテンシ表は `gpu-batch-metric.md`「metric 別実測（Issue #2005）」節を参照（i8 は CPU-SIMD・GPU f16 より遅い）。oversample 1・2 の GPU 実機計測は未実施。

### CPU フォールバックの境界

非対応 metric を拒否した呼び出し元は `FallbackBatchEngine::build(backend_factory)` 経由で CORE-8 の初期化失敗縮退に乗せられる。ただし縮退先 CPU 経路（`run_batch_search`）は現状**内積専用**で、L2・コサイン・L1 を正しく計算するのは #2003／#2002 の範囲。誤った metric の結果を黙って返さないため、本経路は i8 のまま内積へ縮退させる実装を持たない。

### スコープ外

- CPU 縮退経路・`run_batch_search` の metric 対応と f32/f16 経路の WGSL の metric 化（#2003／#2002）
- ~~metric 別の性能ベンチと GPU 実機での Recall 再計測~~（#2005 で実施済み。`gpu-batch-metric.md` 参照）
- バッチ層と SQL `<=>` のゼロノルムの扱いの統一（#2002 の判断事項）
