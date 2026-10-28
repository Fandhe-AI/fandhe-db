# 候補集合を id マスクで直接探索し arena 複製を回避する

- **Issue**: #654（親 Issue #650。ルート #649。前提 Issue #474）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/04-behavior/data-model.md`
  TABLE-12・`docs/spec/04-behavior/rls.md`・CORE-3, CORE-4, CORE-13
- **ステータス**: 実装済み・前後比較実測済み（Issue #655）

## 背景・目的

Issue #474 は `sql::scalar_index::ScalarIndex::resolve_candidates` が絞った候補
スロットを `arena.rs::build_from_cached_rls_rows_subset` へ渡し、候補行の
embedding・id・tenant_id（`String` 複製）を**新規 `VectorArena` へ複製**してから
`SearchProvider::search` に渡していた。選択率が低い述語（crossdb fixture
`lang='ja'` で約 33%）では、候補行数 × dim ぶんの複製が毎クエリ発生する。
`SqlArenaCache`（Issue #363）の借用高速経路は SCALAR 段が恒等写像の場合
（`WHERE`・式述語・hybrid のいずれも無い）にしか効かない。

本 Issue は、キャッシュ済み `SqlArenaSnapshot` の `VectorArena` を借用したまま、
候補スロット列（狭義昇順の `Vec<u32>`）を brute-force provider へ直接渡して
探索する経路を追加し、DISTANCE 経路の arena 複製をなくす。

## 設計

### `SearchProvider::search_subset`（既定実装付き・object-safe 維持）

`kernel.rs` に部分集合探索の入力型 [`SubsetSearchInput`] と trait メソッド
`search_subset` を追加した。

```rust
pub struct SubsetSearchInput<'a> {
    pub slots: &'a [u32],   // vectors への行番号。狭義昇順・重複なし
    pub vectors: &'a [f32], // rows * dim（可視行のみ）
    pub dim: u32,
    pub query: &'a [f32],
    pub k: usize,
}

pub trait SearchProvider: Send + Sync {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError>;
    fn search_subset(&self, input: SubsetSearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        /* 既定実装: slots の行を一時バッファへ gather してから search へ委譲 */
    }
}
```

`SearchInput` を再利用せず専用型にした理由: `ids.len() * dim == vectors.len()`
という既存契約を崩さないため。本型は `ids` を持たず、返る `CandidateHit::id`
は常に `slots[i] as u64` に固定される（`sql::exec` の「provider へ渡す id は
アリーナのスロット番号」という既存契約と同型）。

既定実装（gather）があるため、trait へのメソッド追加という公開 API 変更は
既存のカスタム `SearchProvider` 実装を無変更のままコンパイル・同一結果に保つ
（`tests/kernel.rs`（本ファイル内単体テスト）
`default_search_subset_matches_overridden_implementation` で固定）。

`CpuScalarProvider::search_subset` は gather せず `vectors.get(slot*dim..)` を
直接参照する。`ParallelSearchProvider::search_subset` は既存 `search` と同型の
並列度決定・ワーカー予算調停（`acquire_execution_threads`。両メソッドが共有する
形へリファクタ）を経て、`slots` をワーカー数分に均等分割し
`search_range_by_slots`（`search_range` の「絶対行インデックス範囲」の代わりに
`slots` 配列を分割対象にした版。4 スロットブロック化＋`dot_block4` は共有）で
並列探索する。

ビット一致の根拠: スコアは行ごとに `dot`／`dot_block4`（要素ごとに `dot` と
ビット同一の契約）で計算され、`TopKSelector` はスコア降順・同点 id 昇順の全
順序選出で push 順・分割数に依存しない。候補列が狭義昇順であるため、複製経路
（連番 id）とマスク経路（元スロット id）は「同じ順序を保つ単調写像」の違いで
しかなく、同点タイブレークの相対順は不変。

### `arena.rs::filter_cached_rls_rows_subset`（複製しない候補フィルタ）

`build_from_cached_rls_rows_subset` の隣に、新規アリーナを構築せず通過した
**元スロット番号**だけを返す版を追加した。

```rust
pub(crate) fn filter_cached_rls_rows_subset<G>(
    expected_dim: u32,
    source_arena: &VectorArena,
    metadata: &[Vec<u8>],
    source_slots: &[u32],
    on_visible_row: G,
    max_rows: usize,
    max_bytes: usize,
) -> Result<Vec<u32>>
```

`on_visible_row` の第 1 引数（出力側の連番）は複製版と完全に同じ意味・同じ値に
なる（両関数とも `push_visible_row` と同じカウンタ規約を独立に踏襲しており、
`sql::exec` の `candidate_columns`（出力側連番で添字づけられる）契約はそのまま
満たされる）。索引は候補行を「絞る」ことしかできず「通す」ことはできないため、
`on_visible_row` を省略せず候補ごとに引き続き呼ぶ。

検証（行数/metadata 数一致・狭義昇順・重複なし・範囲内）・容量チェック
（`check_capacity`）は複製版と同じ契約を維持する。実際のバッファ確保
（`GrowableArenaBuffers`）は行わないため、返す `Vec<u32>` 自体の確保のみが
新たに発生する（候補件数に比例。索引が既に上限内へ絞っている）。

### `sql/exec.rs`: マスク経路の結線

「ヒットだが SCALAR 段に実質的な処理がある」分岐で、以下をすべて満たす場合に
限りマスク経路を採る:

- `index_candidate_slots` が `Some`（`CandidateResolution::Use`。
  `FallbackNoIndex`／`FallbackSelectivity`／同一性ガード不一致は従来どおり
  `build_from_cached_rls_rows_subset` の複製経路へ縮退する）
- `!is_hybrid`（`Ranking::Distance` のみ。hybrid の疎コーパス `DocId` は
  スロット番号に依存するため対象外）
- `hnsw_subset_eligible` の場合も含む（Issue #676。ただし縮退時のみマスク経路
  を使い、ANN 探索へ進む場合はその時点で複製する。下記「Issue #676」節参照）

マスク経路が発火すると:

1. `filter_cached_rls_rows_subset` で `kept_slots: Vec<u32>` を得る
2. `owned_arena` を作らず、高速経路（`cache_fast_path_eligible`）と同じく
   `cache_hit_snapshot = Some(snapshot)` として `snapshot.arena()` を借用する
3. `slot_ids`（DISTANCE 段・RLS 安全網へ渡す候補識別子列）は
   `mask_kept_slots.as_ref()` が `Some` なら `kept_slots` を `u64` へ写像した
   もの、`None`（従来どおり）なら `0..arena.ids().len()`
4. DISTANCE 段（非 HNSW 分岐）で `mask_kept_slots` が `Some` のとき
   `provider.search_subset(SubsetSearchInput{ slots: kept, vectors: arena.vectors(), .. })`
   を呼ぶ（`provider.search` ではない——`arena` が複製されない全行ぶんの行列で
   あるため、位置ベースの `SearchInput`（`ids[idx]` ではなく配列中の位置 `idx`
   で `vectors` の行を決める契約）へそのまま渡すと誤った行を候補にしてしまう）
5. 投影: `ScalarSource` に `EagerSubset { columns, kept_slots }` を追加した。
   `arena.vector(slot)`／`arena.ids().get(slot)` は `slot`＝元スロット番号で
   正しく引けるが（`arena` が snapshot そのものであるため）、
   `candidate_columns`（`on_visible_row` が出力側連番で積んだもの）は
   `kept_slots.binary_search(&slot)` で元スロット→出力側連番へ写像してから
   引く必要がある。`kept_slots` に無いスロットは fail-closed に拒否する
6. 統計: `scalar_access.cache.record_index_scan()`（従来どおり）に加え、新設
   `record_index_mask_scan()`（`ScalarIndexCacheStats::index_mask_scans`。
   `index_scans` の部分集合。テナント ID・値・件数を含まないカウンタ）を呼ぶ

`index_candidate_slots` は「metadata_filters か expr_filters が索引対応形状」
の場合にのみ `Some` になり得る（`classify_scalar_plan` が `scalar_prefilter`
込みで判定するため、DISTANCE 先行・SCALAR 事後フィルタ（`!plan.scalar_prefilter`）
のクエリでは常に `PlainScan` を返し `index_candidate_slots` は `None` のまま。
Issue #474 の既存契約）。したがって `!is_hybrid` を満たす限り、マスク経路は
`hnsw_full_visible_eligible`（`filters_empty` を要求するため構造的に両立しない）
とは競合しない。

## 対象外

- **hybrid（`Ranking::Hybrid`）の Subset 形状**: 疎コーパスの `DocId` がスロット
  番号に依存し `hybrid::hybrid_search` の `SearchInput` 契約を変えないと載せ
  られないため、従来の複製経路（`build_from_cached_rls_rows_subset`）を維持する
- **集計・`GROUP BY` 経路**（Issue #475 で別途結線済み）

## Issue #676: HNSW `Subset` 形状の plain scan 縮退時にも本経路を使う

Phase 2 として申し送っていた HNSW opt-in 時の `Subset` 形状（SCALAR 事前
フィルタ付き DISTANCE。`hnsw_subset_eligible`）についても、**候補削減後
（`filter_cached_rls_rows_subset` が返す `kept: Vec<u32>`）に plain scan へ
縮退すると判明した場合に限り**、本経路（`VectorArena` を複製せず候補 id
マスクで直接探索）へ委譲するよう拡張した。ANN 探索へ進む場合は従来どおり
`build_from_cached_rls_rows_subset` で複製する——「複製してから plain scan
するか判定する」のではなく「plain scan と判明してから複製の要否を決める」
順序へ入れ替えたのが本 Issue の核心。

判定は `sql::hnsw_cache::prepare_subset_from_slots`（新設）が `kept.len()`
（複製前に判明する正確な可視カーディナリティ）と
[`Overlay::compute_over_slots`](../../crates/engine/src/sql/hnsw_cache.rs)
（`Overlay::compute` の一般化版。`0..arena.len()` の代わりに任意の順序付き
スロット列を受け取り、`kept` を実際に複製した subset アリーナに対して
`compute` を呼んだ場合と全フィールド同一の `Overlay` を、複製せず ordinal
座標系で返す）で行う。`sql/exec.rs` の DISTANCE 段は判定結果
（`sql::hnsw_cache::SubsetSlotPlan::MaskScan`／`Ann(PreparedHnswSearch)`）に
応じて `provider.search_subset`（複製なし）または `build_from_cached_rls_rows_subset`
＋`search_prepared`（複製あり。ANN 探索）のいずれかへ分岐する。ANN 経路が
返す `id` は subset アリーナの連番（ordinal）であるため、`kept.get(ordinal)`
で元スロット番号へ写像し戻す（範囲外は `SqlSurfaceError::Internal` で
fail-closed に拒否する）。

新設カウンタ `HnswIndexCacheStats::subset_mask_scans`（複製なしマスク経路を
選んだ回数）・`subset_arena_copies`（ANN 進行時に複製した回数。両者は
互いに排他）で採否を観測できる。`index_candidate_slots == None`（索引未消費・
`FallbackSelectivity`・キャッシュミス）の場合は従来どおり
`search_subset_or_fallback`（複製経路）を使う。`EXPLAIN`（Issue #411）の
`ann_plan: hnsw_subset` 露出契約・実行時縮退非露出の方針は不変（検索本体を
実行しない）。

詳細・実装記録は `docs/design/hnsw-rls-cardinality-switch.md`「Issue #676」節
参照。テストは `crates/engine/tests/hnsw_subset_mask_scan.rs`（新規）。
前後比較実測は Issue #677（`docs/design/hnsw-rls-cardinality-switch.md`
「Issue #677」節）が担当済み。

## テスト設計

`crates/engine/tests/scalar_index_mask_search.rs`（新規。`tests/scalar_index_prune.rs`
と同じ流儀: `unique_db_path`／`CleanupGuard`、実 `Storage`、
`engine::tenant::insert_typed_row`）。

- **`mask_path_avoids_arena_duplication_and_never_calls_full_copy_search`**:
  `search`／`search_subset` の呼び出しを記録する provider を注入し、hot 実行で
  `search_subset` が呼ばれ、渡された `vectors.len()` がスナップショット全行ぶん
  （`slots.len()` より真に大きい）であることを直接アサートする——「候補行だけを
  新規複製した」のでは説明できない、行列全体を借用したままマスクで絞った
  ことの機械的証拠
- **cold/hot ビット一致**（等価・前方一致・`id` 範囲・結合の 4 形状）: 複数列
  投影（`id, kind, path`）を含め `(id, cells)` の完全一致を固定する
  （`ScalarSource::EagerSubset` の写像が正しいことの直接検証。`SELECT id` のみ
  だと `EagerSubset` の値読み出しが実質検証されないため、既存の
  `tests/scalar_index_prune.rs` とは独立にこの観点を追加した）
- **`LIMIT` を候補件数未満**にした Top-k 境界検証
- **同点誘発コーパス**での cold/hot 一致
- **統計**: `index_scans`／`index_mask_scans` の非 vacuous な増分、
  `HINT ORDER(RLS, DISTANCE, SCALAR)`（DISTANCE 先行）ではマスク経路が
  一切消費されないこと
- **RLS**: 他テナント private 行が cold/hot いずれのマスク経路でも非混入
- **世代進行**: `INSERT` 後に再構築され、cold/hot 一致が保たれる

`crates/engine/src/kernel.rs`・`parallel_search.rs`・`arena.rs` の単体テストで、
既定実装（gather）とオーバーライド（直接参照）のビット一致、並列経路の
gather 済み `CpuScalarProvider::search` 対照一致（同点誘発コーパス含む）、
`filter_cached_rls_rows_subset` の戻り値が複製版 `build_from_cached_rls_rows_subset`
の出力アリーナと完全に対応することを固定した。

## 検証コマンド

```bash
cargo test -p engine --lib kernel:: parallel_search:: arena::
cargo test -p engine --test core_misc scalar_index_
cargo test -p engine --test core_misc hnsw_cache::
cargo test -p engine --test hnsw_hybrid_refetch
cargo test -p engine --test sql_named sql_explain::
cargo test -p engine --test sql_named sql_precision_mode::
scripts/check_core_api.sh --update && bash scripts/check_core_api.sh
```

既存テスト（`tests/scalar_index_prune.rs` 等）は無変更のまま green（マスク経路
は既存クエリ形状でも自動的に発火するが、投影が `id` のみのクエリでは
`EagerSubset` の値読み出しパスまでは検証しないため、`tests/scalar_index_mask_search.rs`
を独立に追加した）。

## 前後比較実測（Issue #655）

Issue #654（PR #664・merge `2488128`）が導入した候補 id マスク経路（複製
排除）について、`docs/design/benchmark-judgement-policy.md` の計測規約
（交互 N≥5 ペア・per-run 生データ必須・min-of-N＋median 併記・固定 ±5% 帯と
参照区間実測帯の 2 種ノイズ帯）に従い、段別プロファイル（Track A）・crossdb
横断ベンチ（Track B）の 2 系統で前後比較を行った。

### 計測条件

| arm | commit | 位置づけ |
| --- | --- | --- |
| before | `8225baa` | #654 適用直前（PR #665 merge） |
| after | `2488128` | #654 適用直後（PR #664 merge） |

ビルド入力同一性: `git diff --stat 8225baa 2488128 -- Cargo.lock Cargo.toml
crates/wire-server scripts/crossdb_bench` は空（Track B の wire-server バイナリは
engine 以外の入力が同一）。段別プロファイル（Track A）は #663（選択率 opt-in・
merge `db9bd94`）の追加が必要なため作業ブランチ HEAD（`db9bd94`）を after 側の
ビルド元に使ったが、`git diff --stat 2488128 db9bd94 -- crates/engine/src
crates/wire-server/src Cargo.lock Cargo.toml` は空（production コードは #654 適用後
のまま無変更）であることを確認したうえで実施した。

環境: 12 vCPU（`QEMU Virtual CPU version 2.5+`）の共有 QEMU 開発環境。他の
Issue エージェントが並行実行中のため、`BENCH_DEDICATED_ENV=1` は自己申告した
ものの実測 loadavg（Track A: 2.52〜3.97・Track B: 1.66〜2.77）は真の専有環境
（loadavg ≈ 0）ではない。**専有条件は満たされていない・参考値**として扱い、
`docs/design/benchmark-judgement-policy.md` §5 の共有環境区分に従いオーナーの
専有環境再実測を申し送る。`bench-qdrant` コンテナは停止せず稼働のまま
（Issue #636 と同様の逸脱として記録）。

### Track A: 段別プロファイル（`scan_stage_profile_bench`・選択率 1/5＝既定 20%）

`scripts/bench_filtered_distance_ab.sh`（新規）で before/after 各 5 ペア・
5 ラウンド輪番実行。生データ:
`docs/design/bench-data/filtered-distance-mask-ab/20260908T164350Z-scan-profile-*`
（before バイナリ sha256 `09af9b2f…`・after バイナリ sha256 `154d9bec…`）。

| 区間 | before min / median | after min / median | ratio(min) | 参照帯 | 判定 |
| --- | --- | --- | --- | --- | --- |
| `e2e(vector_knn_where/W0-hot)` | 0.6490 / 0.6840 ms | 0.3360 / 0.3410 ms | 0.5177 | 40.07% | **improved** |
| `e2e(vector_knn_where/W0-cold)` | 14.5570 / 14.6340 ms | 14.3960 / 14.5530 ms | 0.9889 | 40.07% | within_band |
| `e2e(vector_knn/W0-nowhere)`（参照） | 0.5950 / 0.6060 ms | 0.5940 / 0.6070 ms | 0.9983 | 40.07% | within_band |
| `R_dot_kernel_distance_only`（参照） | 0.1850 / 0.1890 ms | 0.1730 / 0.1790 ms | 0.9351 | 40.07% | within_band |
| `e2e(agg_count/A0a)`（参照） | 0.0500 / 0.0500 ms | 0.0500 / 0.0500 ms | 1.0000 | 40.07% | within_band |
| `e2e(rls_isolation/A0b)`（参照） | 0.0500 / 0.0500 ms | 0.0500 / 0.0500 ms | 1.0000 | 40.07% | within_band |

`W0-hot`（ホットパス。#654 が変更する経路そのもの）は min-of-N 比 0.5177
（約 48% 高速化）と、参照帯（40.07%。`vector_knn/W0-nowhere`・`R_dot` の広い方）
を超えて `improved` と判定できた。参照 4 区間はいずれも `within_band` で
非退行を確認した（`W0-cold`＝毎回 `Storage::open` を含む経路は #654 の対象外
のため不変）。

after-only（`BENCH_SCAN_PROFILE_SELECTIVITY=1/3`＝crossdb fixture 相当の選択率
33%。before バイナリは選択率 opt-in を持たないため before 対照なし）:

| 区間 | after min / median |
| --- | --- |
| `e2e(index,k=10)` | 0.5010 / 0.5100 ms |
| `e2e(plain,k=10)` | 1.9880 / 1.9950 ms |
| `I1_index_candidate_resolve` | 1.9 / 1.9 µs |
| `I2a_candidate_predicate` | 137.1 / 138.1 µs |
| `I2b_candidate_mask_build` | 140.0 / 141.4 µs |
| `I3_provider_search` | 145.3 / 152.8 µs |

`I2b_candidate_mask_build`（140.0µs）は `docs/design/filtered-distance-stage-profile.md`
が記録した in-binary 対照値（`I2b_candidate_arena_copy` 605.1µs vs
`I2b_candidate_mask_build` 137.5µs）と同水準であり、独立した計測セッションでも
一貫した値であることを確認した。`index_mask_scans_delta>0`・
`consistency checks passed` は全 run で非 vacuous に確認済み。

### Track B: crossdb 横断ベンチ（self・`vector_knn_where`／`bulk_knn_where_k200`）

`scripts/bench_scalar_index_crossdb_ab.sh`（`REF_COMMIT=""` で ref arm を無効化
し所要時間を短縮）で before/after 各 5 ペア輪番実行。生データ:
`docs/design/bench-data/filtered-distance-mask-ab/20260908T164059Z-crossdb-*`・
`*-hybrid-*`（before バイナリ sha256 `9c9ebad5…`・after バイナリ sha256
`45c28ec9…`）。

| フェーズ（p50） | before min | after min | ratio(min) | 参照帯 | 判定 |
| --- | --- | --- | --- | --- | --- |
| `vector_knn_where` | 1895.85 µs | 1282.85 µs | 0.6767 | 45.31% | within_band |
| `bulk_knn_where_k200` | 3018.54 µs | 2178.09 µs | 0.7216 | 45.31% | within_band |
| `hybrid_rrf`（対象外） | 6415.32 µs | 6472.49 µs | 1.0089 | 45.31% | within_band |
| `bulk_hybrid_k200`（対象外） | 9103.81 µs | 9107.97 µs | 1.0005 | 45.31% | within_band |
| `where_compound_count`（対象外） | 1012.41 µs | 1037.80 µs | 1.0251 | 45.31% | within_band |
| `vector_knn`（参照） | 660.03 µs | 666.70 µs | 1.0101 | 45.31% | within_band |
| `agg_count`（参照） | 82.08 µs | 82.14 µs | 1.0007 | 45.31% | within_band |
| `mode_recall`（参照） | 670.80 µs | 676.09 µs | 1.0079 | 45.31% | within_band |

min-of-N は `vector_knn_where` で約 32%・`bulk_knn_where_k200` で約 28% の
高速化を示し、Track A（段別プロファイル。W0-hot 約 48% 改善）と方向が一致する。
ただし本計測セッションの参照区間実測ノイズ帯（`vector_knn.p50` run-to-run 幅
45.31%）が Track A（40.07%）よりさらに広く、共有環境の負荷変動（loadavg
1.66〜2.77・並行 Issue 実行由来）により両ノイズ帯判定では `within_band`
（regressed/improved を断定しない）に留まった。**min-of-N の改善方向自体は
Track A の段別内訳（`docs/design/filtered-distance-stage-profile.md`
〔Issue #653〕の in-binary 対照値 `I2b_candidate_arena_copy` 605.1µs vs
`I2b_candidate_mask_build` 137.5µs——本 Issue の Track A after-only 実測
140.0µs も同水準）と整合しており、e2e レベルでの改善の存在を否定するもので
はない**——参照帯が広いのは計測環境のノイズによるものであり、専有環境での
再実測を待って確定判定とすべきである。非対象フェーズ（`hybrid_rrf`・
`bulk_hybrid_k200`・`where_compound_count`）・参照区間（`vector_knn`・
`agg_count`・`mode_recall`）に加え、hybrid ループ 3 モード（`hybrid`・
`warm_where_then_hybrid`・`body_predicate`。#654 の対象外区間）もいずれも
`within_band` で非退行を確認した。

### Qdrant との差

`docs/design/crossdb-bench.md` の既存実測表と対比する（本 Issue では Qdrant
自体の再計測は行っていない）。同 doc の自己申告どおり、いずれの表も
**共有 VM（loadavg 約 2 前後）での単発実測であり専有環境での再測定は未実施**
——本 Issue も含め、現時点で self・Qdrant を専有環境で比較した実測値は存在
しない。

- 初回実測表（同 doc「横断ベンチ実測（25,000 行・dim 128・k=10）」節。共有
  VM・単発実測）: self `vector_knn_where` 2819µs（p50）に対し Qdrant exact
  732µs・Qdrant HNSW 615µs。
- 2026-09-08 共有環境再計測（同 doc「2026-09-08 再計測」節）: self
  `vector_knn_where` 1953µs（p50）に対し Qdrant exact 657µs・Qdrant HNSW
  675µs。self `bulk_knn_where_k200` 3287µs に対し pgvector HNSW 2500µs。

本 Issue の Track B（本セッション・共有環境）実測では self `vector_knn_where`
の min-of-N が 1282.85µs まで下がった（before 1895.85µs 比 0.68 倍）。同一
セッション内で Qdrant を計測していないため直接比較はできないが、上記
2026-09-08 の Qdrant exact 657µs と比べると差は縮小方向（before 相当の
1895〜1953µs 比では約 2.9 倍だった差が、今回の after min-of-N 1282.85µs では
約 2.0 倍まで縮小）にあると見られる。ただし本計測は参照帯が広く（上記）
確定的な判定ではないため、Qdrant を含めた同一セッションでの専有環境再計測
（README「他 DB との機能別横断ベンチ」節の手順）をオーナーへ申し送る。

### 限界・申し送り

- **1/3 ペア比較は構造的に不能**: before（#654 適用前）バイナリは選択率
  opt-in（Issue #653・#663 で追加）・`I2b`/`I3`/`index_mask_scans_delta` の
  いずれも持たないため、crossdb fixture 相当の選択率 33% での before/after
  ペア比較はできない。HEAD の `scan_stage_profile_bench` を before の
  engine（`ScalarIndex::resolve_candidates` を複製経路のまま持つ #654 適用前
  コード）へ差し替えて再コンパイルする方法も、HEAD 側ハーネスが
  `search_subset`（#654 で新設された API）を参照するためコンパイル不能であり
  採らなかった。
- **100,000 行規模点は未実施**（時間予算の都合。任意項目として plan に記載）。
- **Qdrant の同一セッション再計測は未実施**（README 手順に沿った別途 Docker
  起動が必要なため本 Issue の対象外とした）。
- **専有環境再実測**: 本計測はいずれも共有 QEMU 環境（他 Issue エージェント
  並行実行中）での実測であり、参照帯（40〜45%）が示す通りノイズが大きい。
  min-of-N は一貫して改善方向を示すが、両ノイズ帯判定での確定的な
  `improved` 判定にはオーナーの専有環境再実測が必要。
- **hybrid・HNSW `Subset` 形状**（#654 の対象外区間。「対象外」節参照）は
  本 Issue でも計測していない。

> Issue #2194 注記: 本書の索引の行参照は `SqlArenaSnapshot` のスロットで、Issue #2194 では変更していない（`id_index` を含め #2195 が扱う）。詳細は [rowid-primary-key-separation.md](rowid-primary-key-separation.md) の §8。
