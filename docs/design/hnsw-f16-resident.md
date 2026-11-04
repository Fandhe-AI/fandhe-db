# HNSW 索引ノードの f16 常駐表現と F16C／NEON fp16 デコード付き dot カーネル

- ステータス: **Implemented（既定 F32・F16 は opt-in）**
- 対応: Issue #514（親 #513・Phase 4 親 #459・ルート #455）
- 準拠 ADR: `docs/design/simd-intrinsics-adoption.md`（決定 1〜5。ステータスは
  ファイル上 Proposed のままだが、同 ADR 配下の先行実装〔Issue #528・PR #582〕が
  既にマージ済みのため、本 Issue は ADR の決定を拘束条件として踏襲する。ADR
  ステータス自体の更新はオーナー作業のため本 Issue では変更しない）
- ポインタ: TASK-132・TASK-156・CORE-16（`docs/spec` 側のビヘイビア定義。
  本文は転記しない）
- 関連コード: `crates/engine/src/f16.rs`（新設）・`crates/engine/src/isa.rs`
  （`F16cToken`／`NeonFp16Token`／`F16Kernel`）・`crates/engine/src/hnsw.rs`
  （`ResidentPrecision`／`NodeVectors`／`NodeSource`）・
  `crates/engine/src/hnsw/prefetch.rs`・`crates/engine/src/hnsw/parallel_build.rs`・
  `crates/engine/src/hnsw/provider.rs`・`crates/engine/src/search_engine.rs`・
  `crates/engine/src/sql/hnsw_cache.rs`・`crates/engine/src/sql/explain.rs`・
  `scripts/check_simd_codegen.sh`
- 関連 doc: `docs/design/hnsw-generation-cache.md`・
  `docs/design/explain-search-engine-exposure.md`・
  `docs/design/simd-codegen-guard.md`・`docs/design/chip-kernel-guidelines.md`

## 背景・目的

`sql::hnsw_cache`（#408〜#411）が保持する HNSW 索引ノードのベクトルは
`HnswIndex.vectors: Arc<[f32]>` として f32 常駐しており、探索中の候補生成
（`search_layer_in`）は毎回 f32 行を読む。本 Issue は ADR #508 決定 5 が定める
唯一の適用 seam（`HnswIndex.vectors`・`sql::hnsw_cache`）に対し、索引ノードを
IEEE binary16（`batch_search.rs::pack_f16x2` と同じ RNE 丸めの `u16` ビット表現）
で常駐させ、探索中の候補生成スコアを「f16→f32 昇格 dot」（x86_64: F16C
`_mm256_cvtph_ps`、aarch64: NEON+FP16 `vcvt_f32_f16`）で計算する。索引ヒットの
最終スコアは既存どおり `sql/hnsw_cache.rs::search_with_overlay` が f32 アリーナ
から `kernel::dot` で再計算する（#408 の契約を維持）。

目的は探索中に移動するバイト数を半減することと、intrinsics 初導入を ADR の
`unsafe` 境界（ポインタ load 不使用・`as_chunks`＋`set` 構築・新規 `unsafe` は
トークン dispatch 箇所のみ）に沿って行うことにある。

## 設計判断

| 論点 | 決定 | 理由 |
| ---- | ---- | ---- |
| D1 常駐精度の既定 | `ResidentPrecision::F32` を既定、`F16` は `ValidatedHnswParams::with_resident_precision(F16)` の opt-in | 既存 HNSW テスト・Recall ゲートをビット同一に保つ |
| D2 パラメータの置き場 | `HnswParams` へフィールド追加せず、`ValidatedHnswParams` の private フィールド（`full_scan_ratio` と同型） | `HnswParams` への追加は破壊的変更（既存コメント参照） |
| D3 索引内の f32 保持 | F16 常駐時は f32 を保持しない（`NodeVectors` enum で排他） | 常駐メモリ半減が目的。最終スコアは既にアリーナ f32 から再計算 |
| D4 差分検出（`Overlay::compute`） | `HnswIndex::node_matches(node, &[f32]) -> Option<bool>` を新設し、F32 は `to_bits` 厳密一致、F16 は現在行を `f32_to_f16_bits` で符号化してから格納ビット列と比較 | `vector()` が F16 常駐時に常に `None` を返す設計（D5）のため、旧 `vectors_bit_equal`（`base.index.vector(node)` 経由）をそのまま使うと F16 索引の差分検出が全行「未変更」と誤判定し、テーブル更新後もキャッシュが古いまま応答し続ける silent fallback になる |
| D5 `vector(node)` の挙動 | F32 常駐は従来どおり `Option<&[f32]>`。F16 常駐は `None`。新設 `vector_f16(node) -> Option<&[u16]>` と `resident_precision()` を追加 | 既存 unit test（F32 既定）は無変更 |
| D6 f16 範囲外（`\|x\| > 65504`）の扱い | 凍結時にスキャンし、1 成分でも範囲外ならその索引は F32 常駐へ自動縮退。`HnswIndexCacheStats` に `f16_residency_fallbacks` を追加 | ±Inf 化による非有限スコアの毎クエリ brute-force 縮退という性能崖を避ける |
| D7 グラフ構築 | 構築（`GraphBuilder`／`parallel_build`）は常に f32 で行い、`freeze_from` で f16 化 | 精度によらずグラフを同一に保つ（`f16_precision_produces_identical_graph_shape_to_f32` で機械検証） |
| D8 tail 処理 | 零埋め固定長バッファ（`[u16; LANES]`／`[f32; LANES]`）を同じ `set` 構築経路へ流す | ADR 決定 2 |
| D9 水平和（x86_64） | `_mm256_extractf128_ps`／`_mm_add_ps`／`_mm_shuffle_ps`／`_mm_add_ss`／`_mm_cvtss_f32` のみ。store intrinsics・`transmute` は使わない | ADR 決定 1 |
| D10 AVX-512 f16 変種 | 本 Issue では作らない（`F16cToken` は `avx2,fma,f16c`） | ADR 決定 3 のトークン表に無い |
| D11 NEON | `NeonFp16Token`（`neon,fp16`）・`dot_f16_neon_fp16` に `#[inline(never)]` | ADR 決定 2（生成コード検査からシンボルを検出可能にする） |
| D12 wire-server | 変更なし | HNSW opt-in の CLI は存在しない |

## データ構造

- `f16.rs`: `f32_to_f16_bits`／`f16_bits_to_f32`（`batch_search.rs` から移設。
  `pack_f16x2`／`unpack_f16x2` は移設前とビット同一）・`fits_f16`・
  `encode_rows`（凍結時の 1 回エンコード。範囲外は `F16EncodeError::OutOfRange`）。
- `isa.rs`: `F16cToken`／`NeonFp16Token`（sealed。`Avx2FmaToken` 等と同じ構築
  契約）・`F16Kernel`（`Scalar`／`F16c`／`NeonFp16`）・`detect_f16`／
  `current_f16`（`SimdKernel`／`current` とは独立の `OnceLock`）・
  `dot_f16_f16c`／`dot_f16_neon_fp16`（intrinsics 本体）・`dot_f16_scalar`
  （参照実装）。新規 `unsafe {` は `F16Kernel::dot_f16` のディスパッチ 2 箇所
  のみ（isa.rs 全体で 3→5）。
- `hnsw.rs`: `ResidentPrecision`（`F32`／`F16`。`Display` は `EXPLAIN` の
  閉じた語彙）・`NodeVectors`（`F32(Arc<[f32]>)`／`F16(Arc<[u16]>)`）・
  `NodeSource` trait（`score`／`touch_prefetch`。`impl NodeSource for [f32]`
  は構築経路〔`GraphBuilder`〕、`impl NodeSource for NodeVectors` は探索経路
  〔`HnswIndex`〕）。`search_layer_in`／`prefetch::PrefetchPolicy::
  prefetch_neighbor` を `NodeSource` でジェネリック化し、構築経路・探索経路が
  同じ関数を常駐精度に依存せず共有する。

## `unsafe` の立証

新規 `unsafe {` はいずれも sealed トークン（`F16cToken`／`NeonFp16Token`）の
所持を根拠とするディスパッチ呼び出し 1 箇所ずつ（`isa.rs::F16Kernel::
dot_f16`）。カーネル本体（`dot_f16_f16c`／`dot_f16_neon_fp16`）自体は
`#[target_feature]` を付けた safe fn で、intrinsics 呼び出しは
target_feature 1.1 の規則により `unsafe` ブロックを要さない（`rustc 1.96.0`
で実機確認済み）。ポインタ load/store intrinsics・`transmute`・raw pointer
キャストは使わない（ADR 決定 1）。`tests/isa.rs::
unsafe_is_confined_to_isa_module_with_safety_comments` が個数（5）・
局在（`isa.rs` 以外に `unsafe` 無し）・`SAFETY:` コメント付与を機械検証する。

## 生成コード検査（`scripts/check_simd_codegen.sh`。受入条件 A1）

`dot_f16_f16c`／`dot_f16_neon_fp16` を必須シンボルへ追加したうえで、
「期待命令が 1 件以上出現する」非 vacuous 検査（`expected_rules_for`／
`scan_expected_missing`）を新設した（既存の禁止命令検査は「あってはならない
命令」のみを検査しており、intrinsics 呼び出しがソフトウェア復号へ静かに
縮退していても検出できないため）。

- x86_64: `dot_f16_f16c` に `vcvtph2ps` がメモリオペランド付き（`vcvtph2ps
  -16(%rdi,%r8), %ymm1` の形。レジスタ→レジスタの `vcvtph2ps %xmm, %ymm` の
  みでは不合格）で 1 件以上出現することを要求する。
- aarch64: `dot_f16_neon_fp16` に `fcvtl`／`fcvtl2`（`v_.4s` 幅拡張）が 1 件
  以上出現することを要求する。

`--self-test` に pass fixture（実際に `vcvtph2ps`／`fcvtl` を含む）・fail
fixture（関数名は一致するがソフトウェア復号のみ）を追加し、双方の判定が
機械検証済み（`make simd-codegen-check`／`make simd-codegen-check-cross` で
x86_64・aarch64 の双方を確認）。基線命令カウントの抜粋:

```text
ok: ...dot_f16_f16c...: ... vcvtph2ps=3 vfmadd132ps=3 ...
ok: ...dot_f16_neon_fp16...: ... fcvtl=1 fmla=1 ...
```

（x86_64 側は `vcvtph2ps` がループ展開により 3 回出現。aarch64 側は
`fcvtl=1` が要求パターンを満たす。いずれも禁止命令〔`vpinsrw`／`vunpck*`・
`ins`／`mov v.[bhsd][n]`／レーン指定 `ld1`〕は 0 件）。

## EXPLAIN 露出（R5）

既存 `hnsw_params: m=...,ef_construction=...,ef_search=...` の末尾へ
`,resident=f32|f16`（構築時の静的設定値。`ValidatedHnswParams::
resident_precision()`）を追記する。実行時の自動縮退結果（D6）・実行時の
可視カーディナリティ・行数等は #411 の契約どおり非露出のまま維持する。

## 検証

- `hnsw.rs::tests`: T-H1〜T-H5 相当（グラフ形状の f32/f16 完全一致・
  `approx_heap_bytes` 削減・`node_matches`/`vector`/`vector_f16` の D4/D5
  契約・f16 範囲外での D6 自動縮退・brute-force 対照 Recall@10）。
- `sql/hnsw_cache.rs::tests::
  f16_resident_precision_hits_the_ann_path_and_matches_default_engine_scores_exactly`:
  F16 opt-in が実際に索引探索へ到達すること（非 vacuous・`hits >= 1`・
  `f16_residency_fallbacks == 0`）と、返るヒットのスコアが既定エンジン
  （`CpuScalarProvider`。f32 brute-force）と `to_bits()` 完全一致すること
  （R3・#408 契約）を固定する。
- `tests/isa.rs`：`F16cToken`／`NeonFp16Token` の sealed 方針・`unsafe`
  個数（5）・f16 昇格 dot のスカラー参照実装との許容差内一致・決定性を検証。
- `tests/sql_explain.rs`・`crates/wire-server/tests/wire_explain.rs`：
  `resident=f32` の既存アサートを更新。

## 既知の限界・スコープ外（後続 Issue へ申し送り）

- AVX-512 `_mm512_cvtph_ps` 変種は未実装（ADR トークン表に無い）。
- `RecallEngine` fixture（`crates/engine/tests/fixtures/recall_engine.rs`）
  への `hnsw_f16` 追加・`recall.yml` matrix 拡張・Recall 3 ゲート同一閾値
  検証は Issue #515 で実施済み（`docs/design/ann-recall-gate-verification.md`
  「Issue #515 追記」節参照）。
- 25k／100k／500k × dim 128／768 規模での常駐メモリ・レイテンシの前後比較
  実測は Issue #516 で実施済み（下記「Issue #516 追記」節参照）。
- 既定常駐精度を F16 へ反転するかどうかは #515／#516 の実測後のオーナー判断。
- wire-server への HNSW／精度 opt-in CLI 追加は対象外。
- NEON カーネルの実機（Apple Silicon 等）での生成コード・性能確認はクロス
  コンパイル確認までが本 Issue の範囲（`make check-cross`／
  `make simd-codegen-check-cross` で確認済み）。Apple 実機での性能前後
  比較の手順・記録テンプレートは Issue #526（`docs/design/
  chip-kernel-guidelines.md` §7.7）が整備し、実 Apple 計測そのものは
  オーナー申し送り（#530 も参照）。

## Issue #516 追記: f16 常駐（`hnsw_f16`）と f32 常駐（`hnsw`）の前後比較・常駐メモリ実測

### 目的・測定方法

`docs/design/benchmark-judgement-policy.md` §3〜§4 の計測プロトコル（交互
N≥5 ペア・per-run 生データ必須・固定 ±5% 帯と参照区間実測帯の両方を満たした
場合のみ有効差とする）に従い、`crates/engine/benches/knn_profile_bench.rs`
へ 2 モードを追加した（`scripts/bench_knn_f16_resident_ab.sh`
＝ `make bench-knn-f16-resident` から一括実行）。

- **hot-only モード**（`BENCH_KNN_PROFILE_HOT_ONLY=1`）: 索引 1 回構築＋
  SQL 表層 e2e ホットパス（`ORDER BY embedding <=> '<vec>' LIMIT k`。S0-hot
  相当）と参照区間（`COUNT(*)`）を計測する。毎サンプル新規 `EngineCore` を
  構築する S0-cold は、500k 行規模では非現実的な所要時間になるため対象外
  とした（本節冒頭の目的が hnsw/hnsw_f16 間の相対比較であり、索引構築コスト
  自体は #495・#413 の既存実測が担う）。
- **索引単体メモリモード**（`BENCH_KNN_PROFILE_INDEX_MEMORY=1`）: redb・SQL
  表層 `VectorArena`（`MAX_ARENA_TOTAL_BYTES` 1 GiB 上限）を経由せず、
  メモリ上のコーパスから `HnswIndex::build_parallel_with_precision` を子
  プロセス隔離で 1 回呼び出し、`approx_heap_bytes()`・VmRSS 前後差・VmHWM
  を記録する。500k×768（1 点あたり embedding だけで約 1.5 GiB）は arena
  1 GiB 上限で SQL 表層からは構造的に到達不能なため、この規模点は索引単体
  メモリでのみ計測している。

規模点は 1:128（25,000 行・dim 128）・4:128（100,000 行）・20:128
（500,000 行）・1:768・4:768（100,000 行・dim 768）の 5 点を hot-only の
主系列とし、索引単体メモリはこれに 20:768（500,000 行・dim 768）を加えた
6 点で計測した。

計測環境: 本開発環境（共有 QEMU 環境。CPU `QEMU Virtual CPU version 2.5+`・
12 vCPU・命令セットフラグ `avx2`／`fma`／`f16c`。`docs/design/
benchmark-judgement-policy.md` §5 の証拠力区分では「参考値」）。計測時点の
コミットは `2dcade0`（本 Issue のブランチ差分は計測対象コードに含まない
——`knn_profile_bench.rs` 自体が計測ハーネスであり、`crates/engine/src/`
は無変更）。各 run の `loadavg` は per-run TSV
（`docs/design/bench-data/hnsw-f16-resident-ab/20260907T095824Z-*.tsv`）に
記録済み。

### hot-only レイテンシの実測結果（min-of-N・N=5 ペア）

| point (scale:dim) | rows | hnsw min (ms) | hnsw median (ms) | hnsw_f16 min (ms) | hnsw_f16 median (ms) | ratio (min-of-N) | ratio (median) | 固定 ±5% 帯判定 | 参照区間帯（`COUNT(*)`） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1:128 | 25,000 | 0.4280 | 0.4380 | 0.4170 | 0.4240 | 0.9743 | 0.9680 | Neutral | 0.00%（n=15・0.054ms で不変） |
| 4:128 | 100,000 | 1.6460 | 1.6530 | 1.6490 | 1.6510 | 1.0018 | 0.9988 | Neutral | 0.95%（n=15） |
| 20:128 | 500,000 | 11.6230 | 11.8520 | 11.8730 | 11.9780 | 1.0215 | 1.0106 | Neutral | 1.99%（n=17） |
| 1:768 | 25,000 | 0.5480 | 0.5810 | 0.5680 | 0.5760 | 1.0365 | 0.9914 | Neutral | 0.00%（n=15・0.054ms で不変） |
| 4:768 | 100,000 | 1.7640 | 1.8000 | 1.7730 | 1.8030 | 1.0051 | 1.0017 | Neutral | 2.84%（n=15） |

6 測定点すべてが固定 ±5% 帯以内（`Neutral`）であり、f16 常駐化による
SQL 表層 e2e ホットパスの有意なレイテンシ変化（改善・悪化のいずれも）は
確認できなかった。参照区間（`COUNT(*)`）は `hnsw_params:` を経由しない
経路のため hnsw/hnsw_f16 で共通の値を使い、いずれの点でも実測帯は数%
以内に収まっている。per-run 生データは
`docs/design/bench-data/hnsw-f16-resident-ab/20260907T095824Z-hot-only.tsv`
に記録済み。

`brute_force` 系列（baseline。hnsw/hnsw_f16 各々の直前に 1 回ずつ計測する
輪番）は、初回計測実施時（2026-09-07）のスクリプトが 2 回の `brute_force`
呼び出しを同一ログファイル名へ書き込んでいたため（`log_noise` の `>` 上書き
で 1 回目の値が失われる。本 PR で `_beforehnsw`／`_beforehnswf16` の
suffix によりファイル名を分離済み）、既存ログの `brute_force` 値は「pair
あたり 1 回（hnsw_f16 直前に測定した値）」のみが残っている。本節の判定は
hnsw/hnsw_f16 間の比較のみを主対象とするため（`brute_force` は補助系列）、
この欠落は判定結論に影響しない。

### 索引単体メモリの実測結果

`approx_heap_bytes()` は決定的な集計値（同一構成での rep1/rep2 が全 6 点
でビット同一）であり、時間計測特有の run-to-run ノイズを持たないため
ノイズ帯評価の対象外とする。VmRSS・VmHWM は子プロセスの measurement 経路が
負う固定オーバーヘッド（コーパス生成の `Vec<f32>` 確保等）を含む参考値
として per-run TSV へ記録するに留め、判定には `approx_heap_bytes()` を
用いる。

| point (scale:dim) | rows | dim | hnsw `approx_heap_bytes` | hnsw_f16 `approx_heap_bytes` | ratio (f16/f32) | 削減率 |
| --- | --- | --- | --- | --- | --- | --- |
| 1:128 | 25,000 | 128 | 17,226,056 | 10,826,056 | 0.6285 | 37.15% |
| 4:128 | 100,000 | 128 | 68,903,824 | 43,303,824 | 0.6285 | 37.15% |
| 20:128 | 500,000 | 128 | 327,741,692 | 199,741,692 | 0.6094 | 39.06% |
| 1:768 | 25,000 | 768 | 81,226,056 | 42,826,056 | 0.5272 | 47.28% |
| 4:768 | 100,000 | 768 | 324,903,824 | 171,303,824 | 0.5272 | 47.28% |
| 20:768 | 500,000 | 768 | 1,607,741,692 | 839,741,692 | 0.5223 | 47.77% |

6 点すべてで一貫して常駐メモリが削減されている（dim 128 で約 37〜39%・
dim 768 で約 47〜48%）。削減率が dim に依存して変わるのは、削減対象が
ベクトル本体（`dim` に比例。4 byte→2 byte で半減）のみで、グラフ隣接
リスト（`m`・層数に依存し `dim` に依存しない f32/u32 固定サイズ）が f32/
f16 いずれの常駐でも変わらないため——dim が大きいほどベクトル本体の
相対的な割合が増え、削減率が 50% に近づく（実測方向は
`docs/design/hnsw-f16-resident.md`「データ構造」節の設計どおり）。
per-run 生データは
`docs/design/bench-data/hnsw-f16-resident-ab/20260907T095824Z-index-memory.tsv`
に記録済み。すべて `requested=effective`（f16 範囲外自動縮退〔D6〕は
未発火）であることも同 TSV から確認できる。

### 結論・申し送り

- レイテンシ: 5 規模点すべてでノイズ帯内（`Neutral`）。f16 常駐化は
  SQL 表層 e2e ホットパスに有意な影響を与えない（本開発環境の参考値）。
- 常駐メモリ: 6 規模点すべてで一貫した削減（37〜48%）。決定的な集計値の
  ため専有環境再実測は必須ではないが、`docs/design/benchmark-judgement-
  policy.md` §5 のとおり本節のレイテンシ数値そのものは「参考値」の
  位置づけを維持する。
- 既定常駐精度を F16 へ反転するかどうかの最終判断はオーナー判断
  （#515・本節の実測を踏まえた申し送り。メモリ削減の恩恵に対しレイテンシ
  面での明確な劣化は観測されていない）。

## Issue #2016 追記: HALFVEC 列の HNSW は F16 常駐に固定

`HALFVEC` 列の HNSW（`sql::halfvec_knn` の ANN 経路）は、エンジン設定の常駐精度（`hnsw`／`hnsw_f16`／`hnsw_i8`）と独立に F16 常駐を要求する。列が f16 で保存されているため f32 へ広げた値は f16 で厳密に表せ、F16 常駐は無損失。要求は `HnswSearchProvider::with_resident_precision` で載せ、EXPLAIN の `resident=` も同じ値を示す。詳細は [column-type-extension.md](column-type-extension.md) の Issue #2016 追記を参照。
