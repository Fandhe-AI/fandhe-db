# HNSW 索引のテーブル世代整合キャッシュと未索引分 brute-force 併用

- **Issue**: #408（親 Issue #402・前提 #404〜#407）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: CORE-9・CORE-10・TASK-132
- **ステータス**: 実装済み

## 背景・目的

`SearchEngineKind::Hnsw`（#407）は opt-in 構築時のエンジン選択のみを実装し、
`HnswSearchProvider::search` は常に `ParallelSearchProvider`（brute-force）へ
委譲していた（`hnsw/provider.rs` モジュールドキュメント参照）。理由は
`kernel.rs::SearchInput` が「クエリごとに変わりうる可視行の縮約ビュー」であり、
構築時点のスナップショットしか探索できない `HnswIndex` との差分を安全に判定する
機構が無かったため。

本 Issue は `sql::arena_cache::SqlArenaCache`（#363）・`sql::sparse_cache::
SparseIndexCache`（#357）と同型の「`(table, ctx)` × テーブル単位世代」キャッシュ
（`sql::hnsw_cache::HnswIndexCache`）へ構築済み HNSW を保持し、同一世代内は
索引探索を再利用、世代が進んだ直後は「索引済み集合との差分（未索引行・失効
ノード）」を brute-force で補い、差分比率が閾値を超えたら再構築する
（Lance／qdrant の `indexing_threshold` 方式。手法名のみ参照でコード転記は
しない）ことで、SQL 表層のフィルタなし `Ranking::Distance` クエリを ANN 経路へ
載せ替える。

## 設計上の前提（#405／#407 からの継続）

- 既定エンジンは不変。ANN は `EngineCore::open_with_engine`／
  `from_storage_with_engine` による opt-in 限定
- wire／SQL 表層への新規構文露出はなし（新規 `wire_code` もなし）
- 閾値（`MIN_INDEXED_ROWS`・`REBUILD_DELTA_RATIO`）はすべて本リポジトリの
  実装既定値であり、規範的契約は spec 側 SSOT（ADR `docs/design/
  ann-index-adoption.md` の「実装ガイド（B 案）」節と同じ立て付け）

## 適用条件

`Ranking::Distance` **かつ** `bound.metadata_filters.is_empty() &&
bound.expr_filters.is_empty()`（`sql::sparse_cache::SparseIndexCache` の hybrid
版適用条件と対称）。この条件下ではアリーナが「RLS 可視行の全集合」となり、同一
`(table, ctx, 世代)` でスロット割当まで決定的に再現される（redb 走査順・RLS
判定の純粋性に依存する既存不変条件。`sql::sparse_cache` の適用条件と同根）。
フィルタ付きクエリ・hybrid の密側・`precision_completeness_unbounded == true`
の経路（`sql::exec.rs` が DISTANCE 段自体を実行しない）は従来どおり全件
brute-force——**本 Issue（#408）時点の適用条件**。フィルタ付きクエリ（`Subset`
形状）は Issue #409、hybrid の密側（`sql::hnsw_hybrid::HnswDenseProvider`）は
Issue #410 でそれぞれ別経路として結線済み（`precision` は対象外のまま）。
詳細は `docs/design/hnsw-rls-cardinality-switch.md`・
`docs/design/hnsw-hybrid-iterative-scan.md` 参照。

Rust API（`VectorCore::search`）は本キャッシュを経由しない（`sql::sparse_cache`・
`sql::arena_cache` と同じ段階化。`PrefilterSnapshot`・ストレージ全体世代への
結線は別タスク）。

## データ構造（`crates/engine/src/sql/hnsw_cache.rs`）

- `IndexedBase`: 世代 `built_table_generation` の可視行全体から
  `HnswIndex::build_parallel` で構築した索引。`node_keys: Vec<(tenant_id, id)>`・
  `key_to_node: HashMap<...>` で索引ノード番号と行の恒久的な同定キーを往復できる
  （TABLE-12: 行 `id` の一意性スコープはテナント内のため `(tenant_id, id)` が
  必要。アリーナのスロット番号は行挿入で前方の行が増えるとずれるため索引ノードの
  識別子には使えない）
- `Overlay`: 世代 `generation` の現行アリーナに対する `IndexedBase` の差分。
  `slot_of_node`（ノード → 現スロット。失効は番兵値 `STALE_SLOT`）・
  `stale_nodes`・`delta_slots`／`delta_vectors`（未索引分の複製）を持つ
- `HnswCacheEntry`: `(table, ctx)` 単位のエントリ。`base: Option<Arc<IndexedBase>>`
  （`None` は「一度も構築成功していない」負のキャッシュ専用状態）・
  `overlay: Option<Arc<Overlay>>`・`build_failed_generation: Option<u64>`
- `HnswIndexCache`: `Vec<HnswCacheEntry>` を `RwLock` で保護する本体。統計
  （`hits`／`misses`／`builds`／`build_failures`／`rebuilds`／`delta_searches`／
  `fallbacks`／`entries`）は `EngineCore::hnsw_index_cache_stats()` から公開
  （`VectorCore` trait には載せない固有 API。テナント ID・行 ID 等の機微情報は
  含まない）

### 定数（本リポジトリの実装既定値。#409 の可視カーディナリティ推定への置換まで固定値運用）

| 定数 | 値 | 意図 |
| ---- | -- | ---- |
| `MIN_INDEXED_ROWS` | 1,024 | これ未満は索引を作らず常に brute-force（構築コストが探索削減分を上回るため） |
| `REBUILD_DELTA_RATIO` | 1/10 | `(delta + stale) / n > 1/10` で再構築（Lance 方式の行数比を単純化） |
| `MAX_HNSW_CACHE_ENTRIES` | 8 | エントリ数上限（`SqlArenaCache` と同じ DoS 対策方針） |
| `MAX_HNSW_CACHE_TOTAL_BYTES` | `arena::MAX_ARENA_TOTAL_BYTES` | 保持索引群の概算バイト量合計上限 |
| `HNSW_BUILD_SEED` | 固定定数 | 構築入力のみに依存させ、索引済み集合がクエリ間で無用に変化しないようにする |

## 探索フロー（`search_or_fallback`）

1. `n = arena.len()`。`MIN_INDEXED_ROWS` 未満なら該当 `(table, ctx)` エントリを
   `evict_entry` し、全件 brute-force（PR #434 Cursor Bugbot 指摘対応: 同一
   テーブルの他 `ctx`〔他テナント〕のエントリは巻き添え破棄しない）
2. `lookup` → `Ready(base, overlay)` / `NeedOverlay(base)` /
   `BuildFailedThisGeneration` / `Miss`
3. `Miss` → `IndexedBase::build`（`HnswIndex::build_parallel`）→
   `record_base`（挿入直前に新規 read txn で世代を再照合し、不一致ならキャッシュ
   非反映。戻り値の `Arc` はこのクエリでそのまま使う。`sql::arena_cache::
   SqlArenaCache::insert` と同じ fail-closed 契約）。構築失敗（`HnswError`）→
   `record_build_failed`（世代照合付き。既存エントリが無ければ `base: None` の
   負のキャッシュ専用エントリを新設——後述「構築失敗時の負のキャッシュ」節）で
   記録し全件 brute-force へ縮退。`BuildFailedThisGeneration` → 縮退のみ（同世代
   では再試行しない）
4. `NeedOverlay(base)` → `Overlay::compute(base, arena, current_generation)`
   （世代あたり 1 回・O(n·dim) の突き合わせ。各スロットのキーを `key_to_node` で
   引き、ベクトルが `f32::to_bits` でビット等価なら索引済み、キー不一致・ビット
   不一致は未索引分＝delta）。`(delta + stale) / n > 1/10` なら再構築（3. と同じ
   構築フロー）。それ以外は `Overlay` を `record_overlay_for`（世代照合付き）で
   登録
5. `Ready` → `ef = provider.effective_ef(k + stale_nodes)`、
   `HnswIndex::search`（`thread_local!` の `HnswSearchScratch` を再利用）。
   ヒットの node → `slot_of_node` で現スロットへ写像。**スコアは
   `kernel::dot(arena.vector(slot), query)` で再計算**する（`HnswIndex::search`
   自身のスコアと数式上は同一値になるが、オーバーレイ写像の正しさ自体を per-query
   で二重検証する意味も兼ねる。写像先が範囲外・`node_keys` とアリーナのキーが
   不一致の場合は当該クエリのみ全件 brute-force へ縮退し、エントリは破棄しない）
6. `delta_slots` が空でなければ `provider.search`（未索引分のみの brute-force）
7. 索引ヒットと差分ヒットを `sort_by`（スコア `total_cmp` 降順・同点スロット
   昇順の安定ソート。`sort_unstable_*` は `scripts/check_sort_determinism.sh` が
   禁止）でマージ→ `truncate(k)`。呼び出し元（`sql::exec.rs`）は従来どおり
   `core::provider_result_is_valid` で検証する（二重防御）

## 構築失敗時の負のキャッシュ

初回構築（過去に一度も成功していないテーブル・ctx）が失敗した場合でも、次の
クエリが毎回 `IndexedBase::build`（高価な索引構築本体）を再試行しないよう、
`base: None` の負のキャッシュ専用エントリを新設して `build_failed_generation`
を記録する（結合テスト `tests/hnsw_index_failure_injection.rs` で固定。実装の
初版はこの新設を「既存エントリが無ければ諦める」としており、初回失敗が
毎クエリの再試行連打になるバグを R3 テストで検出し修正した）。

「stale が 1 件でもあれば毎回再構築する」設計は採らなかった: TASK-120 の
`replace_typed_rows_by_text_key`（同一パス置換）はファイル再投入のたびに旧
チャンクを削除するため、この設計だと毎回全再構築になり遅延再構築（本 Issue の
目的そのもの）を失う。

## 失効ノードの扱い

`slot_of_node` が `STALE_SLOT` のノード（削除・不可視化・内容変更された行）は、
再構築されるまでグラフに残り `ef` 枠を消費しうるが、それらは索引構築時点で
同じ ctx に可視だった行であり、`Overlay` の写像を経ない限り結果へは一切返らない
（現在のスロットへ写像できないノードは `search_with_overlay` の走査で
`continue` される）。この扱いは ADR `docs/design/ann-index-adoption.md` が
不採用とした oversampling 事後フィルタ方式とは異なる：oversampling は「多めに
取得してから事後フィルタする」設計だが、本キャッシュは索引構築時点の可視集合を
基準にした事後除外（削除ベクトル的な扱い）であり、他テナントの行を索引側の
候補集合へ一切混入させない。

残余の申し送り: 他テナントの `Public`→`Private` 切替（自テナントには不可視化）
は、再構築が起きるまで `ef` 消費としてのみ観測されうる（結果へは返らないため
テナント境界・RLS 可視性契約自体は破れない）。

## 並行クエリの重複計算

同一世代への並行クエリが `Overlay::compute` を重複して計算した場合は
後勝ち（`record_overlay_for` の世代照合付き登録。`sql::arena_cache::
SqlArenaCache` と同じ性質）——いずれの計算結果も同じ世代に対して有効な値であり、
安全性は損なわれない。

## セキュリティ考慮（OWASP Top 10・security.md P0）

- **アクセス制御／テナント境界**: キーは `(table, PolicyContext)` 完全一致。
  索引は ctx 自身の可視アリーナのみから構築し、他 ctx の索引・オーバーレイを
  参照する経路を構造的に作らない。索引ヒットは必ず現世代アリーナのスロットへ
  写像・キー照合・スコア再計算を経て返し、呼び出し元の
  `provider_result_is_valid`（可視 id 集合所属・件数・順序）が最終防御として
  残る
- **不安全な設計／DoS**: エントリ数・総バイト上限、差分は比率で再構築を強制、
  構築失敗の負のキャッシュで再構築連打を防止、`k + stale > MAX_EF` は厳密探索
  （全件 brute-force）へ縮退
- **依存**: 追加なし（自作 HNSW・`std` のみ）
- **呼び出し元スナップショットとの世代整合**: `search_or_fallback` は
  `read_txn` から読んだテーブル世代を信頼して索引を構築・登録するため、
  呼び出し元が渡す `arena` がその世代のスナップショットであることを保証する
  責務は呼び出し元にある。`rls.rs::PrefilterSnapshot::search_with_hnsw` は
  `read_txn` のテーブル世代と `built_table_generation`（スナップショット構築
  時に読んだテーブル世代）を照合し、不一致なら本モジュールを呼ばず
  brute-force へ縮退する（Issue #409 codex-review P1 指摘・PR #435）

## 既知の限界（申し送り。Issue #409 で解消済みの項目は取り消し線）

~~`k_idx = k + stale_nodes` が `MAX_EF`（10,000）を超えると全件 brute-force へ
縮退する（fail-closed に正確な結果を返すため正しい挙動だが、`stale_nodes` が
1 万を超える状況は `REBUILD_DELTA_RATIO`（1/10）の下では行数 10 万件規模の
テーブルへの中程度の churn で到達しうる）。つまり大規模かつ churn の多いテーブル
では、再構築が発火するまで ANN 経路が実質的に効かない期間が生じる。~~
Issue #409 で `k + stale_nodes` オーバーフェッチ方式を撤去し、`Overlay::visible_mask`
（候補マスク。`crate::hnsw::NodeMask`）を `HnswIndex::search_masked` へ渡す方式へ
置き換えた。詳細は `docs/design/hnsw-rls-cardinality-switch.md` 参照。

## 既知の限界（追記: Issue #514）

`Overlay::compute` の差分検出（世代進行直後に索引済みノードと現行アリーナの
差分を特定する処理）は `HnswIndex::node_matches`（Issue #514・D4）が返す
「一致とみなせるか」の判定に依存する。索引ノードの常駐精度が f16
（`ResidentPrecision::F16`。opt-in・既定は f32）の場合、f16 分解能未満の
embedding 変更は「未変更」と判定されうる。最終スコアは常に f32 アリーナ
から再計算する契約（本 doc・#408）が不変のため探索結果の正しさには影響
しないが、影響範囲はグラフ近傍構造の再利用判定（＝再インデックス要否の
粒度）に限られる。詳細は `docs/design/hnsw-f16-resident.md` 参照。

## 除外ノードと Overlay（Issue #2241）

コサイン用の索引は正規化できない行を持たない（`HnswIndex::is_member` が偽のノード。番号はあるが索引に入っていない）。索引が除外規則を持つとき（`excludes_non_normalizable`）に限り、Overlay は次の規則を追加で適用する。内積グラフが本番の間はこの条件が偽で、挙動は変わらない。

- 現在の行が正規化できない新規行は delta に入れない
- 索引済み member の行が正規化できなくなったら失効にし、delta には入れない
- 除外ノードの行が変わらず正規化できないままなら、失効にも churn にも数えない。正規化できるようになったら delta に入れる（`node_matches` が `Some(false)`）
- 可視マスクと可視カーディナリティ比の分母は member だけで数える（`member_count`）
- 申し送り（#2000）: キャッシュ内部の全走査縮退（`full_scan_with_arena`）でゼロノルム行を NaN として返すかどうかは、metric の結線時に決める

## スコープ外・申し送り

- ~~Rust API `VectorCore::search`（`PrefilterSnapshot`・ストレージ全体世代）への
  索引結線~~ Issue #409 で `rls.rs::PrefilterSnapshot::search_with_hnsw` として
  結線済み（`FullVisible` 形状のみ。`SearchTimeFilter` は対象外のまま）
- ~~hybrid 密側・フィルタ付きクエリの ANN 化（#409／#410）~~ SCALAR 事前フィルタ
  付き DISTANCE（`Subset` 形状）は Issue #409 で、hybrid 密側再取得ループへの
  結線は Issue #410（`sql::hnsw_hybrid::HnswDenseProvider`）で結線済み。詳細は
  `docs/design/hnsw-hybrid-iterative-scan.md` 参照
- ~~`EXPLAIN` 露出（#411）~~ 実装済み。`docs/design/explain-search-engine-exposure.md` 参照
- ~~Recall ゲート同一閾値検証（#412）~~ 実装済み。
  `docs/design/ann-recall-gate-verification.md` 参照。前後比較実測（#413）は継続
- `VectorArena` の `Arc<[f32]>` 化による `HnswIndex::build` 時コピー縮退
  （#404〜#406 からの申し送りを継続）
- 索引の非同期（バックグラウンド）構築・永続化
- ~~`MIN_INDEXED_ROWS`／`REBUILD_DELTA_RATIO` の可視カーディナリティ推定への置換
  （#409）~~ Issue #409 は「探索方式判定」（可視カーディナリティ比による
  plain scan／マスク付き ANN 切替）のみを可視カーディナリティ推定へ置き換えた。
  「再構築判定」（`MIN_INDEXED_ROWS`／`REBUILD_DELTA_RATIO`）は本 Issue のスコープ
  外のまま固定値運用を継続する（`docs/design/hnsw-rls-cardinality-switch.md`
  参照）

## 検証

- 単体（`crates/engine/src/sql/hnsw_cache.rs` in-module）: 世代競合時の
  fail-closed（`record_base` の stale 拒否・cross-table 非破壊）・テナント境界・
  `evict_entry`・`Overlay::compute` の分類（新規・削除・内容変更）
- 結合（`crates/engine/tests/hnsw_cache.rs`）: 構築 → 差分 brute-force →
  再構築 → update/delete の段階遷移で既定エンジン対照の Recall@10 ≥ 0.9（本
  リポジトリの回帰基準。層 A・縮小規模フィクスチャ 1,200〜1,440 行・dim 16
  で実測安定を確認）を維持しつつ、統計カウンタで各段の経路を非 vacuous に固定。
  テナント境界・フィルタ付き/hybrid の適用条件遵守・Rust API 非経由も固定
- 結合（`crates/engine/tests/hnsw_index_failure_injection.rs`）: 全次元
  `f32::MAX` の毒行注入による索引構築失敗 → brute-force 縮退（既定エンジン完全
  一致）→ 同一世代では再試行しない → 再オープン後も同様 → 毒行削除後の次世代で
  索引経路へ復帰
- `make core-api-check`（`SearchProvider` trait 差分ゼロ）・
  `make sort-determinism-check`（`sort_unstable_*` 不使用）green を確認済み
