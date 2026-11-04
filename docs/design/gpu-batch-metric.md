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

## 残課題

- L1、packed i8 の metric 対応（#2004）、metric 別の回帰ベンチ・Recall（#2005）
- f16 算術版の L2 対応（オーバーフローガードの再設計が要る）
- 極小成分の行が f16 でゼロノルムになる境界は未検証（fixture で避けている）
- バッチのゼロノルムを SQL と同じ「NaN を末尾に並べる」挙動にするには、バッチの有限スコア
  契約の変更が要る（親で判断）
- WGSL では非 const 式の Inf/NaN の結果が不定になりうる。NaN 行や巨大クエリの L2 は
  既存の内積経路と同じ前提（部分 Top-k シェーダの指数ビット判定とホスト側で除外）に依存する
