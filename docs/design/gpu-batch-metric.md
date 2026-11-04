# GPU バッチ検索の metric 対応（L2・コサイン）

- 対象 Issue: #2003（親 #2002）。ポインタ: CORE-6・CORE-16・SQL-33
- 状態: 実装済み（`crates/engine/src/batch_search.rs`・`batch_fallback.rs`・`gpu_batch.rs`）
- 関連: [cosine-distance-method.md](cosine-distance-method.md)（コサインの方式 C2′）、
  [gpu-batch-wgpu-enablement.md](gpu-batch-wgpu-enablement.md)（GPU バッチの基盤）

## 背景

バッチ経路は内積だけを前提にしていた。SQL 表層と HNSW には L2・コサインが入っており、
バッチ経路にも同じスコア規約を持たせる。親 #2002 から引き継いだ条件は次の 3 つ。

- GPU とフォールバック（CPU）で Top-k が一致する
- 未対応の形は fail-closed のまま
- テナント境界（`PolicyContext::is_visible` の単一照合パス）を緩めない

## 決定

### D1. metric は常駐行列単位（`BatchMetric`）

`BatchMetric { InnerProduct（既定）, L2, Cosine }` を `batch_search` に追加し、
`ResidentMatrix::build_with_metric` で行列に持たせる。`BatchQuery` は公開 struct で広く
構築されているため、metric を足さない。`GpuBatchBackend::try_new` のシグネチャは変えず、
`matrix.metric()` を読む。`SQL` 側の `DistanceKind` や HNSW の `HnswMetric` は流用しない
（依存方向と可視性のため）。L1 は対象外。

### D2. コサインは正規化して内積に帰着（C2′）

行は構築時に 1 回だけ単位長へ正規化してから f16 パックする。CPU 縮退用の f32 コピーも
元の入力から同じ関数で正規化する。クエリはバッチの入口で 1 回だけ正規化する。正規化後は
既存の内積シェーダ・カーネルをそのまま使え、bind group layout も変わらない。
正規化関数 `normalize_into` は HNSW と共有するため `kernel` へ移した（挙動は不変）。

### D3. L2 は uniform の分岐

スコアは `-Σ(r - q)²`（大きいほど近い）。WGSL は `METRIC_DOT`／`METRIC_L2` 定数を持ち、
j／q ループの外側で 1 回だけ分岐する。内積側のループ本文は従来の文字列のまま。符号反転は
全量 readback シェーダと部分 Top-k シェーダで同じ文字列にし、既存のビット同一契約を保つ。
奇数次元の端数は行・クエリとも 0.0 パディングなので差が 0 になり、結果に影響しない。

### D4. uniform のレイアウト

`Params` は 32 バイト（`metric` はオフセット 16）、`TopKParams` は旧 `pad0` を `metric` に
転用（オフセット 20）。f16 算術版と packed i8 版の WGSL `Params` は 16 バイトのままで、
uniform の `min_binding_size` が `None` のため、32 バイトのバッファの先頭 16 バイトを読む
形で互換になる。バッファ確保サイズは 32 バイトへ揃えた。

### D5. 内積専用の経路は fail-closed

- f16 算術版: L2 では常に unpack 版を選ぶ（オーバーフローガードは積の上界から導いており
  `(r-q)²` には当てはまらず、f16 算術版の WGSL にも L2 分岐が無い）。強制指定との組み合わせは
  `KernelLaunchFailed`。コサインは正規化後の内積なので既存ガードをそのまま適用する
- packed i8 版: 内積以外の行列は GPU コンテキスト取得より前に `InitFailed`（#2004 で対応）

### D6. ゼロノルムの扱い（バッチ契約に合わせる）

バッチ経路のスコア契約は「有限値のみ」。そのため SQL の「NaN を昇順末尾に並べる」は採らない。

- ゼロノルム（アンダーフロー・オーバーフロー含む）の行: 全成分を NaN にして格納する。
  スロットは保たれ、走査時に非有限スコアとして候補から外れる
- ゼロノルムのクエリ: `BatchSearchError::ZeroNormQuery { query_index }` で拒否する。
  primary へ渡す前に判定するので、`Backend` エラーによる誤ラッチは起きない
  （公開 enum への variant 追加なので破壊的変更）

### D7. backend の metric 不一致

`BatchBackend::metric()`（既定実装は内積）を追加した。`FallbackBatchEngine::build_with_metric`
は factory が返した backend の metric が engine と違う場合に初期化失敗（`Init`）として
CPU 専用モードにする。metric を無視する外部実装が誤った順位を返す経路を作らない。

## テナント境界

metric の分岐はスコア計算だけに入れ、可視性の判定（`gather_reachable_rows`・
`PolicyContext::is_visible`・選出後の `resolve_batch_slot` と `revalidate_primary_hits`）には
触れない。NaN 行もスロットを保つため、スロットからテナントへの解決はずれない。

## 検証

- 単体: `batch_search`・`batch_fallback`・`gpu_batch`（シェーダ定数・L2 分岐の文字列一致・
  uniform レイアウト・シェーダ選択）・`packed_i8`（内積以外の拒否）
- 結合 `tests/gpu_batch.rs`: GPU 初期化の成否で分岐しつつ、どちらの分岐でもオラクル一致を
  assert する。GPU 分岐は奇数次元・複数タイル・部分 Top-k と全量 readback のビット同一・
  行チャンク分割・複数テナント混在・f32 対照経路を含む
- 結合 `tests/core_misc/batch_fallback.rs`: 必ず失敗する factory で、GPU の有無に依らず
  縮退経路の L2・コサインを検証する（アダプタ無し CI でも skip にならない）

## metric 別実測（Issue #2005・親 #2002。CORE-6・CORE-16・SQL-33 ポインタ）

規模スイープ `gpu_scaling_bench` と受け入れゲート `batch_bench` を metric 別に計測できるようにし、
GPU 実機で性能と Recall を再計測した。生データは
`docs/design/bench-data/gpu-scaling-ab/20261104-2005-*`（`*-summary.tsv`・metric 別 `*-raw.log`・
`*-batch-bench-raw.log`）に置く。

### 計測の仕組み

- `BENCH_GPU_SCALING_METRIC`／`BENCH_BATCH_METRIC`: `inner_product`（既定）・`l2`・`cosine`。
  `l1` を含む allowlist 外は起動時に fail-closed で拒否する。1 プロセス = 1 metric（連続計測のノイズを避ける）
- 既定（内積）の既存出力行は不変。追加行は `gpu_scaling_metric:`（起動時）・`gpu_scaling_recall:`
  （規模点ごと）・`batch_metric:`（`batch_bench` 起動時）。`batch_bench` のゲート行は内積以外のときだけ
  末尾に `metric=<name>` が付く
- Recall の基準は「同じ metric の CPU-SIMD（f16 常駐）経路」の Top-k の id 集合であり、f32 厳密値に対する
  Recall ではない。したがって GPU f16 経路（CPU と同じ f16 量子化）は 1.0 になりやすく、f32 常駐対照の値が
  f16 量子化による境界入れ替わりを表す。i8 は同じ基準に対する id ベースの Recall@k
- i8 のコサインは最終スコアが `-(1 - cos)` で CPU 基準と 1.0 ずれるため、不一致件数の算出時だけ +1.0 して
  尺度を揃える（参考値）。主指標は Recall@k
- `bench.yml` の `bench-batch` へ repo variable `BENCH_BATCH_METRIC` を注入する（未設定は内積）。
  閾値は metric によらず既存の `bench-gate` secrets

### 環境と条件

- GPU: NVIDIA GeForce RTX 3060（Vulkan、ドライバ 595.91.07）。CPU 10 論理コア（Avx2Fma）、Linux 7.0。
  rustc 1.98.1。**共有開発機**（専有環境ではない。計測開始時 loadavg 約 1.1）
- 条件: rows = 20,000／100,000、dim = 128／256、batch = 8／64、k = 10、計測反復 20（warmup 20）、
  合成乱数 seed 固定、i8 oversample = 4（既定）
- ラウンド数: 3（metric の実行順はラウンドごとに入れ替え）。表のレイテンシは p95 の **3 ラウンド最小値**
  （min-of-N。中央値は summary.tsv に併記）。`docs/design/benchmark-judgement-policy.md` が求める 5 ラウンド
  には届いていないため、レイテンシは参考値として扱う。Recall・不一致件数は確定的指標
- `BENCH_GPU_SCALING_QUERY_F16_EXACT` は無効のため、全 metric で `f16_arith_dispatches=0`
  （ガードにより unpack 版。L2 の f16 算術版非対応〔D5〕とは別に、この条件では内積でも 0 になる）

### 結果（p95 の 3 ラウンド最小値）

| metric | rows | dim | batch | CPU-SIMD p95 | GPU f16 p95 | GPU f32 p95 | GPU i8 p95 | speedup_f16_p95 | mismatch | f32 Recall@10 | i8 Recall@10 |
| ------ | ---: | --: | ----: | -----------: | ----------: | ----------: | ---------: | --------------: | -------: | ------------: | -----------: |
| inner_product | 20000 | 128 | 8 | 5.57 ms | 0.59 ms | 0.70 ms | 7.48 ms | 9.49x | 1 | 0.9875 | 1.0000 |
| inner_product | 20000 | 128 | 64 | 17.45 ms | 3.88 ms | 5.08 ms | 68.23 ms | 4.50x | 0 | 1.0000 | 1.0000 |
| inner_product | 20000 | 256 | 8 | 9.53 ms | 2.81 ms | 3.23 ms | 7.97 ms | 3.39x | 0 | 1.0000 | 1.0000 |
| inner_product | 20000 | 256 | 64 | 28.17 ms | 5.56 ms | 7.81 ms | 75.12 ms | 5.07x | 1 | 0.9984 | 1.0000 |
| inner_product | 100000 | 128 | 8 | 27.74 ms | 3.60 ms | 3.10 ms | 41.18 ms | 7.70x | 0 | 1.0000 | 1.0000 |
| inner_product | 100000 | 128 | 64 | 84.04 ms | 16.10 ms | 21.13 ms | 400.56 ms | 5.22x | 0 | 1.0000 | 1.0000 |
| inner_product | 100000 | 256 | 8 | 46.07 ms | 3.40 ms | 6.13 ms | 48.42 ms | 13.53x | 0 | 0.9875 | 1.0000 |
| inner_product | 100000 | 256 | 64 | 135.99 ms | 22.27 ms | 31.96 ms | 465.92 ms | 6.11x | 0 | 1.0000 | 1.0000 |
| l2 | 20000 | 128 | 8 | 10.84 ms | 0.61 ms | 0.70 ms | 10.77 ms | 17.77x | 0 | 1.0000 | 1.0000 |
| l2 | 20000 | 128 | 64 | 54.50 ms | 4.97 ms | 4.74 ms | 105.66 ms | 10.97x | 0 | 1.0000 | 1.0000 |
| l2 | 20000 | 256 | 8 | 20.18 ms | 4.96 ms | 1.32 ms | 10.98 ms | 4.07x | 0 | 1.0000 | 1.0000 |
| l2 | 20000 | 256 | 64 | 118.43 ms | 5.92 ms | 7.81 ms | 103.27 ms | 20.00x | 0 | 0.9984 | 1.0000 |
| l2 | 100000 | 128 | 8 | 51.42 ms | 2.74 ms | 3.03 ms | 59.88 ms | 18.74x | 0 | 1.0000 | 1.0000 |
| l2 | 100000 | 128 | 64 | 268.05 ms | 16.21 ms | 21.34 ms | 711.72 ms | 16.54x | 0 | 1.0000 | 1.0000 |
| l2 | 100000 | 256 | 8 | 101.21 ms | 3.45 ms | 5.83 ms | 65.71 ms | 29.36x | 0 | 1.0000 | 1.0000 |
| l2 | 100000 | 256 | 64 | 574.53 ms | 22.45 ms | 32.33 ms | 601.30 ms | 25.60x | 1 | 0.9984 | 1.0000 |
| cosine | 20000 | 128 | 8 | 5.56 ms | 0.59 ms | 0.71 ms | 16.69 ms | 9.36x | 0 | 1.0000 | 1.0000 |
| cosine | 20000 | 128 | 64 | 16.78 ms | 3.95 ms | 5.12 ms | 159.78 ms | 4.25x | 0 | 1.0000 | 1.0000 |
| cosine | 20000 | 256 | 8 | 9.53 ms | 4.43 ms | 1.25 ms | 17.80 ms | 2.15x | 0 | 1.0000 | 1.0000 |
| cosine | 20000 | 256 | 64 | 27.06 ms | 5.85 ms | 7.92 ms | 179.53 ms | 4.62x | 0 | 1.0000 | 1.0000 |
| cosine | 100000 | 128 | 8 | 27.90 ms | 2.67 ms | 3.15 ms | 105.76 ms | 10.44x | 0 | 1.0000 | 1.0000 |
| cosine | 100000 | 128 | 64 | 82.84 ms | 16.85 ms | 20.79 ms | 1208.78 ms | 4.92x | 0 | 1.0000 | 1.0000 |
| cosine | 100000 | 256 | 8 | 46.13 ms | 3.27 ms | 5.94 ms | 112.10 ms | 14.13x | 0 | 1.0000 | 1.0000 |
| cosine | 100000 | 256 | 64 | 135.09 ms | 22.04 ms | 32.16 ms | 1199.35 ms | 6.13x | 0 | 1.0000 | 1.0000 |

読み取り:

- **Recall**: GPU f16 経路は全 24 点で CPU-SIMD 基準と Recall@10 = 1.0。f32 常駐対照は内積の 3 点・L2 の 2 点で
  0.9875〜0.9984（f16 量子化の境界入れ替わり。コサインは全点 1.0）。i8（oversample=4）は全 metric・全点で
  Recall@10 = 1.0000、`i8_mismatch` = 0
- **コサインは内積とほぼ同コスト**: 正規化して内積に帰着する設計（D2）どおり、GPU f16 の p95 は
  同条件の内積と同程度（例: 100k×256×64 で 22.0 ms 対 22.3 ms）。CPU-SIMD も同程度
- **L2 の短縮率が大きいのは CPU 側が遅いため**: L2 の GPU f16 p95 は内積と同程度だが、CPU-SIMD の L2 は
  内積の約 2〜4 倍遅く（例: 100k×256×64 で 575 ms 対 136 ms）、短縮率が 4.1〜29.4 倍に開く。GPU 優位の
  根拠として使う場合はこの基準側の差に注意する
- **i8 は GPU f16 より全点で遅い**。CPU-SIMD に対しても、内積は 20k×256×8 の 1 点だけ、コサインは全点で
  遅い。L2 は CPU 基準が遅いため 8 点中 4 点で CPU より速い。コサインの i8 は内積の約 2〜3 倍
  （100k×128×64 で 1.2 s）。Recall は全 metric で維持されるが、現状の i8 経路は性能目的では優位でない
  （#543 の結論と整合）。oversample 1・2 の計測は時間の都合で未実施
- 20k×256×8 の点では GPU f16 の p95 が 2.8〜4.9 ms と、同条件の `batch_bench`（1 ms 未満）や隣接点より
  大きい。L2・コサインでは f32 常駐対照の約 4 倍、内積は同程度。規模点の切替直後の影響の可能性があり、
  metric 起因とは断定できない（スコープ外として追跡を提案）

### CORE-6／CORE-16 ゲートの実測（`batch_bench`・20,000×256・batch 8・各 1 回）

閾値は検証用のプレースホルダ（1%）を使ったため、pass はあくまで参考であり spec 閾値での判定ではない
（正式判定は Environment `bench-gate` の管理者作業）。

| metric | CPU-SIMD p95 | GPU p95（CORE-6） | f32 常駐 p95 | f16 常駐 p95（CORE-16） |
| ------ | -----------: | ----------------: | -----------: | ----------------------: |
| inner_product | 8.78 ms | 1.92 ms | 1.10 ms | 0.86 ms |
| l2 | 28.50 ms | 4.25 ms | 1.25 ms | 0.96 ms |
| cosine | 9.02 ms | 1.95 ms | 1.09 ms | 0.88 ms |

GPU 結合テスト（`tests/gpu_batch.rs` 15 件・`tests/gpu_batch_i8.rs` 9 件）は実機で全件 pass。これらの
テストは GPU 無しでも skip して通る作りのため、GPU 経路が実際に動いたことは上記のベンチ出力
（`gpu_f16_p95` 等）で確認した。#2004 で「未検証」とした i8 の GPU 実機確認は、ベンチ上の
Recall@10 = 1.0000（全 metric）で補う。

### 未実施・申し送り

- 5 ラウンド以上の追試、500k 行・batch 1／256 の点、i8 oversample 1・2 の追加計測
- repo variable `BENCH_BATCH_METRIC` の設定と GPU 搭載ホストでの `bench.yml` 実行は運用者作業
  （本変更では設定していない）
- spec 閾値での CORE-6／CORE-16 の metric 別正式判定は Environment `bench-gate` の管理者作業

## 残課題

- L1（バッチ経路の対象外）。packed i8 の metric 対応は #2004、metric 別の回帰ベンチ・Recall は #2005 で完了
- f16 算術版の L2 対応（オーバーフローガードの再設計が要る）
- 極小成分の行が f16 でゼロノルムになる境界は未検証（fixture で避けている）
- バッチのゼロノルムを SQL と同じ「NaN を末尾に並べる」挙動にするには、バッチの有限スコア
  契約の変更が要る（親で判断）
- WGSL では非 const 式の Inf/NaN の結果が不定になりうる。NaN 行や巨大クエリの L2 は
  既存の内積経路と同じ前提（部分 Top-k シェーダの指数ビット判定とホスト側で除外）に依存する
