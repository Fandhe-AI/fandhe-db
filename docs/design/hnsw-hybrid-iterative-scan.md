# フィルタ付き ANN の境界再取得（iterative scan 型）と fail-closed 検証（Issue #410）

親 Issue #402（Phase 3・ANN opt-in 採用）。前提 #408（`sql::hnsw_cache::
HnswIndexCache`）・#409（`docs/design/hnsw-rls-cardinality-switch.md`）。対象
ビヘイビア（ポインタのみ）: CORE-9・CORE-10・TASK-132・SEARCH-1・SEARCH-3・
RLS-1〜4・TASK-138・TASK-139。ADR: `docs/design/ann-index-adoption.md`
（Accepted・B 案）。本ドキュメントが定める閾値・ラウンド上限・統計名はいずれも
本リポの実装既定値（非規範）であり、`wire_code` の新設・`EXPLAIN` への露出
（#411）は行わない（#411 は実装済みだが、ラウンド数・実行時縮退結果は
引き続き非露出。`docs/design/explain-search-engine-exposure.md` 参照）。

## 背景・目的

Issue #410 は「フィルタ付き ANN が結果不足・境界同点未確定のとき `ef` を段階的
に拡張して再探索する（iterative scan）」ことを目的として起票された。調査の
結果、この目的を満たす経路は当初想定と異なることが判明した:

- **DISTANCE 経路（`sql::hnsw_cache::search_with_overlay`）の「結果不足」
  （`masked_short`）は #409（PR #435）以降、構造的に到達不能**（下記「証明」
  節）。この分岐へ `ef` 倍増ループを追加すると、本リポが避ける方針の
  到達不能分岐（`hnsw.rs::select_neighbors_heuristic` の `extend_candidates`
  注記と同じ方針）になるため、**追加しなかった**。
- **iterative scan が非 vacuous に効くのは hybrid 密側の再取得ラウンド**
  （`hybrid.rs::hybrid_search_boosted` の `dense_fetch_k` 倍増ループ）。
  このループは元々 `sql::hnsw_cache` を経由せず生の `&dyn SearchProvider`
  （全件 brute-force）を使っていたため、ここへ HNSW を結線することが
  #410 の実質的なスコープになった。

## DISTANCE 経路の `masked_short` 到達不能性（証明）

`search_masked`（`hnsw.rs`）は次の 2 点を保証する:

1. 層 0 の初期候補集合に、検査済み起点（`search_entry_for_mask` の戻り値。
   `is_mask_fully_reachable` と同じ起点選択）を必ず含める（降下後ノードと
   異なる場合。PR #435）。
2. `overlay.mask_splits_graph == false`（`Overlay::compute` が世代毎に 1 回
   判定）のとき、`is_mask_fully_reachable` は検査済み起点から**受理ノード
   全体**への到達可能性を保証済み。

`search_layer` の受理判定は「`results.len() >= ef && strictly_farther` の
場合のみ打ち切る」規約であり、それ以外は候補ヒープが空になるまで受理ノードを
`results` へ積み続ける（`worst_ok = results.len() < ef` の間は常に真）。1・2
と合わせると、`mask_splits_graph == false` のとき、層 0 探索の**切り詰め前**
の結果 `results`（`search_layer` の戻り値。`hnsw.rs::search_masked`）について

```text
results.len() >= min(ef_eff, visible_in_index)
```

が常に成立する（`ef_eff = effective_ef(k) = ef_search.max(k).min(MAX_EF)`
が `k` 以上であり、`min(x, visible_in_index)` は `x` に関して単調非減少の
ため `min(ef_eff, visible_in_index) >= min(k, visible_in_index) = expected`
も成立する）。`search_masked` は最後にこれを `take(k)` で切り詰めて返す
（`hnsw.rs::search_masked` 末尾）ため、呼び出し元
（`sql::hnsw_cache::search_with_overlay`）が受け取るのは
`index_hits.len() = min(results.len(), k)` である。`results.len() >=
expected` かつ `expected <= k` （`expected = min(k, visible_in_index)`）
なので

```text
index_hits.len() = min(results.len(), k) >= min(expected, k) = expected
```

が成立する。`search_with_overlay` の `masked_short` 分岐
（`index_hits.len() < expected`）はこの不等式と矛盾するため、
`mask_splits_graph == false` の間は到達しない。`mask_splits_graph ==
true` のときは `search_masked` 自体を呼ばず別分岐（統計
`mask_splits_graph`）で plain scan するため、こちらも `masked_short` へは
到達しない。

結論: 現行実装では `masked_short` は防御的分岐（想定外の不整合に対する
fail-closed の最終防御）に留まり、「`ef` 拡張で結果不足を解消する」という
狙いの対象にはならない。`docs/design/hnsw-rls-cardinality-switch.md`
「切替規則」節・`sql/hnsw_cache.rs` の `masked_short` コメントへこの結論を
反映済み。

### `TwoHop` レジーム下の再検証（Issue #501）

上記の証明は前提 1・2 を `search_masked`（`HopMode::OneHop` 固定）に対して
述べたものだが、`sql::hnsw_cache::TraversalRegime::TwoHop`（ACORN-1・
Issue #501）が選ばれる場合は `HnswIndex::search_masked_with_hop(.., hop:
TwoHop, ..)` を呼ぶ。前提 1・2 が `TwoHop` でも成立することを確認する:

- 前提 1（検査済み起点を層 0 の初期候補集合に含める）: `search_masked_with_hop`
  の起点選択（`search_entry_for_mask` 由来の `checked_entry`）は `hop` に
  依存しないコード経路のため無変更で成立する。
- 前提 2（`mask_splits_graph == false` のとき検査済み起点から受理ノード
  全体へ到達可能）: `Overlay::compute` は `regime.hop()`（`TwoHop` なら
  `HopMode::TwoHop`）を渡して `is_mask_fully_reachable_with` を呼ぶ
  （`sql::hnsw_cache::traversal_regime_for` が単一情報源。Issue #501）。
  `bridge_expand`（`hnsw.rs`）は探索（`search_layer_in`）・BFS
  （`HnswIndex::accepted_reachable_count`）の双方から呼ばれる共有実装
  であり、「非受理ノードに出会ったときの規則」（1-hop 非受理として初めて
  訪問したときのみ中継点として使い、受理済み・未訪問の 2-hop ノードだけを
  候補化する）がビット同一であることを構造上（同一関数の共有）保証する。
  したがって `TwoHop` の BFS が「到達可能」と判定した受理ノード集合は、
  `TwoHop` の探索が実際に辿り着ける集合と一致し、前提 2 は成立する。

前提 1・2 が成立する以上、上記の不等式の導出（`results.len() >=
min(ef_eff, visible_in_index)` から `masked_short` 非到達へ至る論理）は
`hop` に依存しないため、`TwoHop` でも同じ結論（`masked_short` は
`mask_splits_graph == false` の間は到達しない）が成立する。BFS と探索が
異なる規則を実装していた場合（`bridge_expand` を共有せず個別実装した
場合）はこの前提 2 が崩れ、`masked_short` が `TwoHop` 限定で到達可能に
なりうる——`bridge_expand` の共有はこの証明を維持するための構造的な
必須条件である（`docs/design/hnsw-rls-cardinality-switch.md`「Issue #501」
節「同期」参照）。

## hybrid 密側再取得ループへの結線

### 現状整理

`hybrid.rs::hybrid_search_boosted` の密側再取得ループは、境界同点グループが
未確定（`TieBoundary::Undetermined`）の間 `dense_fetch_k` を
`pool_depth * 2` から倍増しつつ `provider.search` を呼び直す（Issue #310・#320。
`docs/design/hybrid-recall-regression.md`「Issue #310」節・
`docs/design/rrf-tie-break-determinism.md` 参照）。この `dense_fetch_k` の
単調倍増そのものが iterative scan の実体であり、Issue #410 の役割は
「各ラウンドの `provider.search` を HNSW 索引経由にする」ことに絞られる。

### 設計: 準備／探索の分離（`sql/hnsw_cache.rs`）

`search_or_fallback`／`search_subset_or_fallback`（#408・#409）は「索引・
オーバーレイの解決」（`IndexedBase::build`・`Overlay::compute` を伴う O(N)
相当の重い処理。`k` に依存しない）と「Top-k 探索」（`k` に依存し軽い）が
1 関数に混在していた。hybrid の複数ラウンドで毎回呼ぶと重い解決を毎ラウンド
やり直すことになるため、次の中間表現へ分離した:

- `PreparedHnswSearch`（`pub(crate) enum`）: `Indexed { base, overlay,
  success_stat }` または `FullScan`。
- `prepare_full_visible`／`prepare_subset`: 解決のみを行い
  `PreparedHnswSearch` を返す（`search_or_fallback`／`search_subset_or_fallback`
  から抽出。統計計上位置・縮退分岐・fail-closed 契約は移動していない）。
- `search_prepared`: `PreparedHnswSearch` を使って 1 ラウンド分の Top-k
  探索を行う（`Indexed` なら既存 `search_with_overlay` をそのまま呼ぶ——
  この関数自体は無変更。`FullScan` なら `slot_ids` を使う全件 brute-force）。

既存の `search_or_fallback`／`search_subset_or_fallback`（DISTANCE 単発
クエリ向け）は「`prepare_*` を 1 回呼び `search_prepared` を 1 回呼ぶだけの
薄いラッパー」へ書き換えた。単発クエリでは解決・探索をそれぞれ 1 回ずつ
呼ぶだけであり、統計・戻り値・エラー契約は分離前とビット単位で同一
（`crates/engine/src/sql/hnsw_cache.rs` の既存 in-module テスト・
`crates/engine/tests/hnsw_cache.rs` が無変更のまま green であることで確認
済み）。

### アダプタ（`sql/hnsw_hybrid.rs`。新設）

`HnswDenseProvider<'a>` は `sql::exec::execute_statement_with_cache` の
`Ranking::Hybrid` 分岐がクエリ開始時に一度だけ構築する `SearchProvider`
アダプタ。`access`（`HnswCacheAccess`）・`arena`・`slot_ids`・`inner`
（索引を使わない場合の委譲先。既存の hybrid 経路がそのまま使っていた
provider）・`prepared`（`prepare_full_visible`／`prepare_subset` の結果）を
保持する。

**fail-closed な受理条件**: `search(input)` は `input.vectors`／`input.ids`
が構築時に捕捉した `arena.vectors()`／`slot_ids` と**同一バッファ**（ポインタ
・長さが一致）を指している場合に限り `search_prepared` を呼ぶ。1 つでも
外れれば `inner` へそのまま委譲する。`hybrid.rs::hybrid_search_boosted` の
密側再取得ループは `dense_input.ids = input.ids`／`dense_input.vectors =
input.vectors`（`k` のみ変更）で `provider.search` を複数ラウンド呼ぶ契約
（コード事実。`hybrid_search_boosted` 本体で確認済み）であり、`sql/exec.rs`
が構築する `SearchInput` も常に `&slot_ids`／`arena.vectors()` を渡すため、
この受理条件は同一クエリの全ラウンドで一貫して成立する（アダプタの
in-module テスト `search_reuses_prepared_base_across_rounds_for_the_same_buffer`
で固定）。別クエリ・別バッファに対して索引経由で答えてしまう構造的リスクを
この ptr-eq 判定で塞ぐ。

`sql/exec.rs` の `Ranking::Hybrid` 分岐は、`hnsw_hybrid_full_visible_eligible`
／`hnsw_hybrid_subset_eligible`（DISTANCE 版の `hnsw_full_visible_eligible`
／`hnsw_subset_eligible` と対称。`is_hybrid` を要求する点のみが異なる）の
いずれかが真かつ `hnsw_cache` が `Some` の場合のみアダプタを構築し、
密のみ縮退経路（疎コーパス 0 件）・`hybrid::hybrid_search`（疎索引あり）の
両方をこのアダプタ経由にする。`hybrid.rs`・`SearchProvider` trait は無変更。
`precision` モードは DISTANCE と同じ理由（`precision_policy.hybrid()` の
確信度ゲートは厳密順位を前提とするため。TASK-162・SEARCH-9）で対象外。

### 統計（`HnswIndexCacheStats`）

- `ef_cap_fallbacks`: `search_with_overlay` が `k > MAX_EF` を `search_masked`
  呼び出し前に検出し（`HnswError::InvalidParams` の文字列比較に依存しない
  よう検証順序を先取り）、直ちに plain scan へ縮退した回数（`fallbacks` の
  内数）。`hybrid.rs::MAX_FETCH_K`（`MAX_POOL_DEPTH * 4` = 40,000）は
  `crate::hnsw::MAX_EF`（10,000）の 4 倍のため、密側再取得ループが
  `fetch_k` を伸ばし切ると理論上到達しうる。
- `hybrid_dense_searches`: アダプタが受理条件を満たし `search_prepared` を
  呼んだ回数（1 クエリで複数ラウンドぶん加算されうる）。
- `hybrid_queries`／`hybrid_rounds_max`: `HnswDenseProvider::finish`
  （クエリ終了時に `sql/exec.rs` が明示的に呼ぶ。ロックを取る統計反映を
  `Drop` に持ち込まない設計）が、そのクエリで観測されたラウンド数を
  `hybrid_queries`（+1）・`hybrid_rounds_max`（CAS で最大値を更新）へ反映
  する。索引経路を一度も使わなかったクエリ（`rounds == 0`）は加算しない。

いずれもテナント ID・行 ID・スコアを含まない（`HnswIndexCacheStats` の既存
方針を踏襲）。

## 停止性・決定性

- 停止性は `hybrid.rs` 側の `dense_cap = MAX_FETCH_K.min(input.ids.len())`
  と `dense_fetch_k` の単調倍増（Issue #310・#320。本 Issue で無変更）が
  provider の実装に依らず保証する。初期 `dense_fetch_k` は
  `min(2·pool_depth, dense_cap)` であり（実装は `hybrid.rs::
  hybrid_search_boosted` の `dense_fetch_k` 初期化を参照）、これが既に
  `dense_cap` に達している場合（`dense_cap < 2·pool_depth`。例:
  `dense_cap = 100`・`pool_depth = 200`）は倍増の余地がなく 1 ラウンドで
  確定する。ラウンド数は高々
  `⌈log2(dense_cap / min(2 · pool_depth, dense_cap))⌉ + 1`（分母を
  `min(2·pool_depth, dense_cap)` に補正した式。既定 `pool_depth = 200` の
  小〜中規模コーパス〔`dense_cap >= 2·pool_depth`〕では 8 以下）。
  `dense_cap = 0`（可視候補 0 件）の場合は `dense_fetch_k = 0` のまま
  `provider.search` を 1 回呼ぶだけ（式の分母が 0 になり定義できないため、
  この境界は式の対象外として明示する）で、空の結果を確定させて終了する。
- `k > MAX_EF` のラウンド（`fetch_k > 10,000`）は `ef_cap_fallbacks` 経由で
  brute-force 縮退し厳密結果になる。ラウンドごとに近似（ANN）／厳密
  （brute-force）が混在しうるが、`hybrid.rs` は各ラウンドの `hits` を
  置き換える設計のため整合性は崩れない。
- 決定性（同一索引・同一クエリ・同一世代で同一結果）は、索引ヒットを常に
  `kernel::dot` で再計算する既存契約（#408）と `search_with_overlay` 自体が
  無変更であることから維持される。`crates/engine/tests/hnsw_hybrid_refetch.rs
  ::tie_inducing_corpus_hybrid_search_terminates_and_is_deterministic` が
  同点誘発コーパス（`quantize_levels`。`benches/harness/hybrid_latency.rs`）
  で同一クエリを 3 回実行し結果が完全一致すること・`hybrid_rounds_max <= 8`
  であることを固定する。

## `complete_boundary_tie_group` との相互作用

各ラウンドは独立した `provider.search` 呼び出しであり、
`resolve_boundary_tie_group`（Issue #310・#320）は**そのラウンドの返却列**
に対して境界同点グループの終端確定を行う。ANN 経由では返却列が近似候補列
であるため、同点グループの完全性は「ANN 候補順序に対する完全性」であり
真の総当たり順序に対する完全性ではない（`docs/design/hnsw-search.md`
「決定性の保証範囲」と同じ位置づけ）。決定性（同一索引・同一クエリ・同一
世代で同一結果）は維持されるが、Recall はブルートフォース対照からの近似
乖離を許容する（`hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`
が Recall@10 ≥ 0.9 の回帰基準で検証）。ラウンド間の前方一致（疎側
`SparseScored::top` が持つ性質。Issue #392）は密 ANN 側では保証しない
（`ef` 拡大でより良い候補が見つかれば prefix が入れ替わりうる）。`hybrid.rs`
はラウンドごとに `hits` を丸ごと置き換える設計のため契約上の問題はない。

## 検証

- 単体（`sql/hnsw_hybrid.rs` in-module）: 別バッファは `inner` へ委譲（統計
  非汚染）・同一バッファの複数ラウンドで `prepare_*` が 1 回のみ実行される
  こと（非 vacuous）・`k > MAX_EF` の brute-force 縮退（空集合の誤返却なし）
- 単体（`sql/hnsw_cache.rs` in-module）: 既存テストが分離後も無変更のまま
  green（`search_or_fallback`／`search_subset_or_fallback` の振る舞い・
  統計計上位置が不変であることの確認）
- 結合（`crates/engine/tests/hnsw_cache.rs`）:
  `filtered_distance_bypasses_full_visible_entries_and_matches_default_engine`
  （フィルタ付き DISTANCE は `FullVisible` エントリを占有しない契約を維持）・
  `hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`
  （新設。既定エンジン対照 Recall@10 ≥ 0.9・可視外テナント非混入・
  `hybrid_dense_searches > 0` の非 vacuous 検証。旧来「hybrid は常に
  `HnswIndexCache` を迂回する」としていた契約を反転した——旧テストは実際には
  hybrid SQL を一切実行しておらずその主張を検証していなかった点も含め、
  本 Issue で是正した）・
  `hybrid_queries_use_subset_shape_and_match_default_engine_recall`
  （新設。`hnsw_hybrid_subset_eligible`（`WHERE` 付き hybrid）は本テストでしか
  経由しない分岐のため、`subset_searches > 0` **かつ** `hybrid_dense_searches
  > 0` の両方で非 vacuous を固定する。可視カーディナリティ比・グラフ連結性は
  コーパスの乱数シードに依存し、シードによっては `mask_splits_graph` 経由の
  plain scan 縮退（fail-closed。誤りではない）に落ちて `subset_searches` が
  0 のままになりうることを実装中に確認したため、実測で `subset_searches > 0`
  になることを確認済みのシードを固定して使う）
- 結合（新設 `crates/engine/tests/hnsw_hybrid_refetch.rs`）: 同点誘発
  コーパスでの停止性（`hybrid_rounds_max <= 8`）・複数ラウンドの実発生
  （`hybrid_rounds_max >= 2`。`prepared` の再利用が SQL 表層経由でも実際に
  複数ラウンドにわたって働くことの確認）・決定性（3 回実行の完全一致）。
  `k > MAX_EF` の SQL 表層直接誘発は `LIMIT` の許容上限（`crate::hnsw::
  MAX_EF` と同値）により不可能なため、この経路は単体テストでのみ検証する
- `make core-api-check`（`SearchProvider`/`VectorCore` trait 差分ゼロ。
  シグネチャ無変更）・`make sort-determinism-check`（`sort_by` のみ使用）
- 既存の hybrid・RLS 統合テスト（`tests/hybrid.rs`・`tests/hybrid_recall.rs`
  層 A・`tests/sparse_cache_recall.rs`・`tests/sparse_determinism.rs`・
  `tests/plan_rls_boost.rs`・`tests/default_preset.rs`）は無変更のまま green

## Phase B（訪問済みビットマップを引き継ぐ再開型スキャン）の採否

Issue 起票時の作業内容に「訪問済みビットマップの引き継ぎ」があったが、上記
「停止性・決定性」節のとおり停止性・k 件充足は既存の `dense_cap`・
`ef_cap_fallbacks` で既に保証されており、受け入れ基準はいずれも満たされて
いる。`hnsw.rs::search_layer` を「seed → run(ef) → 再開」型へ再構成する
実装は、`hnsw.rs` の構築経路・探索経路が共有する `search_layer` の複製を
避けられるかの検証（bit 同一ゲート: `tests/hnsw.rs`・`tests/hnsw_search.rs`・
`tests/hnsw_cache.rs` 全件 green）にコストが見合うだけの実測上の必要性
（`ef` 拡張のたびに新規探索をやり直すコスト超過の実測）がこの時点では
確認できていないため、本 Issue のスコープでは**実装を見送った（Rejected）**。
将来、hybrid 密側の再取得ラウンド数・レイテンシが実運用上問題になった場合に
`make bench-hybrid-profile`（Issue #356・#387〜#392）で再開コストを実測した
うえで再検討する。

## セキュリティ考慮（OWASP Top 10 観点）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御の不備／テナント境界（P0） | 索引は `(table, ctx)` 可視アリーナのみから構築（不変）。アダプタは同一バッファ・同一 `slot_ids` の場合のみ索引経路を使い、それ以外は `inner` へ委譲（fail-closed）。索引ヒットはスロット写像・`(tenant_id, id)` キー照合・`kernel::dot` 再計算を経由し、`hybrid.rs` の可視 id 検証（`core::provider_result_is_valid`・`HybridError::ProviderResultRejected`）・`RlsSafetyNet` の多層防御は無変更。`PolicyContext::is_visible` に新規比較ロジックを追加していない |
| 存在情報の副次漏えい | 統計（`HnswIndexCacheStats`）にテナント ID・行 ID・スコアを含めない |
| 不安全な設計（DoS） | ラウンド数は `dense_cap`・`MAX_FETCH_K` で有界。`k > MAX_EF` は fail-closed に brute-force。`prepare_*` は準備 1 回に限定しラウンド数倍の O(N) を回避 |
| インジェクション | SQL 文字列の組み立てなし |
| untrusted 入力 | `k`／`fetch_k` は `HnswIndex::search_masked` の `MAX_EF` 検証と `hybrid.rs` の長さ検証を通る。`unwrap`/`expect`/添字アクセスは production コードに置いていない（`get()`／`checked_*` を使用） |
| 脆弱な依存 | 依存追加なし |
| private spec 漏えい（P0） | コメント・doc・コミット・PR は TASK／ビヘイビア ID のポインタ表記のみ |

## 将来の拡張・申し送り（本 Issue のスコープ外）

- ~~`EXPLAIN` へのエンジン種別・ラウンド数・縮退有無の露出（#411）~~ エンジン種別・
  静的な ANN 適用判定のみ実装済み（ラウンド数・実行時縮退結果は引き続き非露出。
  `docs/design/explain-search-engine-exposure.md` 参照）
- ~~Recall 3 ゲートの ANN 同一閾値検証（#412）~~ 実装済み。
  `docs/design/ann-recall-gate-verification.md` 参照。前後比較と
  `full_scan_ratio`／`MAX_EF` 既定値の再調整（#413）は継続
- `precision` モード hybrid の ANN 化（確信度ゲート契約の再設計が前提）
- `SearchTimeFilter` 経路・Rust API `hybrid` 相当 API の結線
- ~~Phase B（再開型スキャン）の再検討（実測に基づく必要性の確認後）~~ 状態保持・
  決定性・停止性の契約設計は完了（Issue #504。上記「Phase B 再検討
  （Issue #504）」節参照）。実装（#505）・前後比較実測（#506）は継続

## Phase B 再検討（Issue #504）: 再開型探索の状態保持と決定性・停止性契約

親 Issue #503（`docs/design/hotpath-implementation-survey.md` §5・§9-#3c で
「条件付き採用」と評価された pgvector `hnswscan.c` 型の破棄候補ヒープ保持）。
Phase 3 親 #458／ルート #455。依存 #465（CLOSED。最新基線は
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線（2026-09-06・
Issue #465）」節）。実装は #505、前後比較実測は #506（本 Issue のスコープ外）。
ステータス: **Proposed**（採否はオーナー判断。#505 のマージ根拠は本節が定める
正しさ契約——bit 同一ゲート・Recall 非劣化——であり、性能面の採否は専有環境
実測に委ねる。#506 が共有 QEMU 環境での N=5 ペア前後比較実測を記録済み
（下記「前後比較実測（Issue #506）」節。`masked_short` 悪化は fix `923efcf`
で解消済みだが、レイテンシは条件依存で改善〔複数ラウンド発火時〕・悪化
〔単発ラウンドでも生じる固定コスト〕の両方が観測されており、共有 QEMU 環境の
数値は採否根拠にしない。`docs/design/benchmark-judgement-policy.md` §5〜6。
専有環境実測は引き続きオーナー作業）。

上記「Phase B（訪問済みビットマップを引き継ぐ再開型スキャン）の採否」節の
**Rejected 判定を撤回するものではない**。当時は実測上の必要性が確認できず
見送ったのであり、本節はそれとは独立に「再開型にした場合、Issue #504 の
見出し主張（融合結果・境界同点グループ・`fetch_k` スケジュールが不変）が
どの範囲で成立するか」を #505 が機械検証できる粒度まで先に契約化する
（実装 GO を意味しない）。

### 現状の再実行型（コード事実の整理）

呼び出し系列は次のとおり（origin/main で確認済み。関数名・定数名は本節末尾の
すべてが対応するコードと一致することを実装前に再確認すること）:

1. `sql/exec.rs` の `Ranking::Hybrid` 分岐が `prepare_full_visible`／
   `prepare_subset`（`sql/hnsw_cache.rs`）をクエリ 1 回だけ呼び、
   `sql::hnsw_hybrid::HnswDenseProvider::new` を構築、終了時に `finish()`
2. `hybrid.rs::hybrid_search_boosted` の密側再取得ループが `dense_cap =
   MAX_FETCH_K.min(input.ids.len())` を上限に、初期 `min(2·pool_depth,
   dense_cap)` から `TieBoundary::Undetermined` の間 `dense_fetch_k` を
   倍増しながら `provider.search` を**毎ラウンド最初から呼び直す**
3. `HnswDenseProvider::search` は ptr-eq 受理条件（`input.vectors`／
   `input.ids` が捕捉済みバッファと同一の場合のみ）で `search_prepared` へ
   委譲し、`search_with_overlay`（`full_scan_ratio` 切替・
   `mask_splits_graph` 検査・`k > MAX_EF` の `ef_cap_fallbacks`・
   `search_masked`・`masked_short` ガード・スロット写像＋`kernel::dot`
   再計算・delta brute-force マージ・`sort_by(score desc, id asc)`・
   `dedup`・`truncate(k)`）を実行する

`prepare_*` は #410 で既にクエリ 1 回に償却済み。ラウンド毎に再評価される
のは `search_masked`（上位層貪欲降下・層 0 ビーム探索の全 visited ノードの
距離計算）・写像検証・delta brute-force の部分であり、再開型の狙いは
このラウンド間の重複探索を削減することにある。

`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線（2026-09-06・
Issue #465）」節のとおり、既定エンジン `hybrid_rrf` の `dense(B0)` は
25,000 行・dim 128 で全体の約 4.7〜4.8% にとどまる。再開型の効果は
**hnsw opt-in かつ再取得ラウンドが実際に複数回発火するクエリ**（同点誘発
コーパス等。`tests/hnsw_hybrid_refetch.rs` の条件）に限定され、全体レイテンシ
への寄与は構造的に有界であることをここに明記する。

### 状態保持契約

- **所有者・寿命**: クエリ単位。`HnswDenseProvider` インスタンスが保持し
  `finish()`（クエリ終了）で破棄する。**`HnswIndexCache` には置かない**——
  同キャッシュは世代キー・複数クエリ間で共有される構造であり、再開状態は
  特定の `prepared`（base ＋ overlay）・クエリベクトル・マスクの組に一意に
  束縛されるため、キャッシュに混ぜるとテナント・クエリを跨いだ誤共有の
  リスクを生む
- **内容**（`hnsw.rs` の型で記述。#505 が新設する）: 独自の visited
  ビットマップ（既存の thread-local `SEARCH_SCRATCH` はラウンド間で状態が
  リセットされる前提のため使えず、専用スクラッチを新設する）・`expanded`
  ビットマップ（`candidates.pop()` で実際に展開した——隣接ノードを走査
  した——ノードを記録する。`visited` とは別物: `visited` は隣接ノードとして
  **発見**された時点で立つのに対し、`expanded` はそのノード自身が候補
  ヒープから**pop されて隣接走査された**時点で立つ。**`discarded`
  ／`candidates` のどちらの集合に属するかとは独立**——`results.pop()` に
  よる `discarded` への追い出しは「そのノードが現在の `results` の
  top-ef から外れた」ことのみを意味し、`candidates.pop()` による展開の
  有無とは無関係に起こり得る〔push 時点で `candidates`・`results` の
  両方へ同時に積まれるため、`candidates` に残ったまま＝未展開のまま
  `results` からだけ追い出されるケースがある〕）・`in_candidates`
  ビットマップ（そのノードが現在 `candidates` ヒープへ push 済みで未 pop
  ——エントリが在庫として残っている——ことを示す。push 時に立て、pop 時に
  下ろす。上記の「`candidates` に残ったまま `results` からだけ追い出され
  る」ノードを resume 時に再判定する際、`discarded` 側だけを見て
  無条件に再 push すると同一ノードが `BinaryHeap` に重複して積まれるため、
  この重複防止に用いる——下記「再開手順」参照）・`candidates`
  （未展開の受理済みノード。`BinaryHeap<ScoredNode>`）・`discarded`
  （**「まだ自己昇格していない」未受理ノード全体の集合**——`worst_ok`
  不受理で候補ヒープへ一度も積まれなかったノード、および `results.pop()`
  で `results` から追い出されたノード——後者は `expanded` の有無を問わない。
  **除去は自己昇格の成功時のみ**——`candidates` へ push された（＝
  `in_candidates` が立った）ことは `discarded` からの除去条件にしない。
  この 2 つの集合（`discarded`＝自己昇格の再評価対象／`candidates`＋
  `in_candidates`＝展開キューへの在庫）は独立な軸であり、1 ノードが
  同時に両方へ属し得る（自己昇格に失敗しつつ展開キューには積まれている、
  という状態が正常に存在する。下記「再開手順」参照）。`candidates` と
  同じ `ScoredNode::Ord`——スコア降順・id 昇順の全順序）・`admitted`
  （これまでに受理されたノードとスコア。ラウンド r の返却は `admitted`
  の上位 `k_r` 件）・直前ラウンドの `ef`
- **再開手順**: ラウンド r（`k_r > k_{r-1}`）では、まず `discarded` の
  **全ノード**（`expanded` の有無を問わない）について `ef_r =
  effective_ef(k_r)` の下で `worst_ok` を再評価し、満たすものは
  `results`（および `admitted`）へ**直接**挿入し、**このときに限り**
  `discarded` から除去する（自己昇格）。これは discovery 時と同じ
  自己昇格処理を明示的に行うものであり、`candidates` への合流だけでは
  代替できない——`search_layer_in` の while ループは `candidates.pop()`
  した候補**自身**を `results` へ追加しない（隣接ノードのうち
  `worst_ok` を満たすものだけを追加する）ため、discarded ノードを
  `candidates` へ戻して pop させるだけでは、それ自身が新たな `ef_r` の
  下で top-ef 相当になっていても `results` へは決して戻らない（例: 起点
  E（score=3）から A（2）、B（1）へ接続し、A/B の隣接が E のみの場合、
  `ef₁=k₁=2` で `worst_ok` 不受理となり `discarded` へ入った B を、
  自己昇格なしに `candidates` へ戻すだけで `ef₂=k₂=3` を再開しても、B の
  隣接はすべて visited 済みで新規発見が無いため `results` は `{E, A}` の
  2 件のまま——`k₂=3` の期待件数を満たせず `masked_short` へ縮退しうる。
  自己昇格を先に行えば `worst_ok` 再評価で B が `results` へ直接復帰し
  3 件になる）。

  **`discarded` からの除去条件は自己昇格の成功のみであり、`candidates`
  への push とは独立**（codex-review 指摘・PR #589）。自己昇格の成否に
  関わらず、discarded から見つかった各ノードのうち `expanded` が未設定
  であり、かつ `in_candidates` が未設定（＝`candidates` ヒープに在庫が
  無い）のものは `candidates` へ push し `in_candidates` を立てるが、
  **この push は `discarded` からの除去を伴わない**——自己昇格に**失敗**
  したノードは `expanded`／`in_candidates` の状態と無関係に `discarded`
  に残り続け、次ラウンド以降も `worst_ok` の再評価対象になり続ける（1
  ノードが `discarded`（自己昇格の再評価対象）と `candidates`＋
  `in_candidates`（展開キューの在庫）の両方に同時に属する状態が正常に
  存在する）。旧版はこの push を「discarded からの除去」と誤って結び
  付けており（自己昇格の成否によらず除去）、次の反例で結果件数の下限を
  満たせないことが判明した（codex-review・PR #589 指摘）: 起点 E
  （score=10）から葉 7 個 L1〜L7（score=9〜3。すべて E のみに接続する
  星型グラフ）へ接続し、`ef` を `k` として 2→4→8 と拡張する。ラウンド 1
  （`ef=2`）で E・L1 が受理され L2〜L7 は `discarded` へ入る。ラウンド 2
  （`ef=4`）で自己昇格を再評価すると L2・L3 が新たに受理されるが、
  L4〜L7 は依然 `worst_ok` 不受理のまま——旧版はここで L4〜L7 を
  （push 対象であることを理由に）`discarded` から除去してしまうため、
  ラウンド 3（`ef=8`）の開始時点で `discarded` が空になり、L4〜L7 は
  二度と自己昇格の再評価を受けられない。`candidates` に積まれた L4〜L7
  は葉ノードで隣接が E のみ（visited 済み）のため pop されても新規発見
  が無く、`results` は 4 件のまま `k₃=8` を満たせず `masked_short` へ
  不要に縮退する。修正版（除去を自己昇格成功時のみに限定）では L4〜L7
  は `discarded` に残ったままラウンド 3 を迎え、`ef₃=8` の下での
  `worst_ok` 再評価で全員が受理され 8 件そろう。

  **自己昇格したノードもこの push 対象に含める**——自己昇格は「そのノード
  を `results`／`admitted` へ復帰させる」操作であり、`search_layer_in` の
  while ループの構造上、「そのノード自身の隣接を展開する」操作を代替
  しない（`candidates.pop()` した候補自身は `results` へ追加されないのと
  対称に、`results`／`admitted` へ直接挿入された候補も自動では展開され
  ない）。自己昇格の成否に関わらず未展開ノードを候補へ戻さなければ、
  その隣接ノードは exhaustive 終了まで発見されないままになりうる（上記の
  例を延長: B の隣接に C（score=4）が新たに接続されているとする。
  自己昇格のみで `candidates` へ戻さない場合、B は `results` へ復帰し
  `{E, A, B}` の 3 件で `k₂=3` の期待件数自体は満たすが、C は永久に
  未発見のまま——候補・破棄ヒープが尽きて `exhaustive` 終了と誤判定
  されうる。B の隣接を展開して初めて C が発見され、C のスコア次第では
  真の Top-3 が入れ替わる。exhaustive 終了時に「マスク受理ノードの到達
  可能成分を全訪問した」という下記「決定性契約」の前提を満たすには、
  自己昇格の成否と無関係に未展開ノードを候補へ戻す必要がある）。
  `expanded` 済みのノードは自己昇格のみを行い `candidates` へは戻さない
  ——隣接ノードは既に走査済み（`visited` 済み）のため再度 pop して隣接
  走査しても新規の候補は一切生まれず、二重の展開コストにしかならない
  （自己昇格に失敗した場合は上記のとおり `discarded` に残り続ける）。
  続いて `search_layer_in` の while ループを**同じ停止条件・同じ受理
  判定（`worst_ok`）**のまま続行する（上位層の貪欲降下〔`ef=1`〕は
  再実行しない——層 0 の状態のみを再開する）。`candidates.pop()` 時にも
  防御的に `expanded` を再検査し、既に設定済みであれば隣接走査せず
  読み捨てる——`in_candidates` の更新漏れ等で万一重複エントリが紛れ
  込んでも二重展開にならないようにする多重の安全策であり、これにより
  **ノード 1 個あたりの実質的な隣接走査は同一クエリの全ラウンドを通じて
  高々 1 回**になり、下記「停止性契約」の「全ラウンド合計の展開数は
  高々 N」が成立する（`discarded` からの除去を自己昇格成功時のみに
  限定しても、`discarded` の要素数は依然として高々 N であり——除去
  されないノードも `expanded` が立てば再度 `candidates` へ push
  されない——下記「メモリ上限」の見積りは変わらない）
- **停止条件は `peek` で判定し、打ち切り候補を消失させない**: 現行の
  `search_layer_in` は `candidates.pop()` で候補を取り出してから
  `results.peek()` に対する停止条件（`worst_ok` の否定）を判定するため、
  停止が発火した時点で pop 済みの候補（`top_candidate`）は
  `candidates`・`discarded` のいずれにも属さないまま関数を抜ける。
  非再開の 1 回実行ではこの候補は元々 `results` へ追加されない候補
  なので影響しないが、再開型ではラウンド r+1 で `ef` が増え
  `top_candidate` 自身が受理対象になり得るにもかかわらず両ヒープから
  消失しているため二度と発見できない（例: 起点 E〔score=10〕から
  A〔9〕・B〔8〕・C〔7〕へ接続し、C の隣接に F〔20〕があるとする。
  `ef=2` で A・B を自己昇格・展開した後 C を `pop` して停止条件が成立し
  `break` すると、C は `candidates`・`discarded` のどちらにも残らない。
  `ef` を 3 へ増やす次ラウンドは `discarded` の再評価から始まるため C を
  再訪できず、F は永久に未発見のまま——上記「決定性契約」が要求する
  exhaustive 時の到達可能成分全訪問と矛盾し、`results` が真の Top-3 を
  欠いたまま返る）。この消失を防ぐため、再開型 `run(ef)` の停止条件判定
  は `candidates.pop()` ではなく **`candidates.peek()`** で行う——停止が
  成立する場合は候補を pop せず、そのまま `candidates`（および
  `in_candidates`）に残し次ラウンドの再開に委ねる。停止が成立しない
  場合のみ pop して展開する。この変更は非再開の 1 回実行の返却値
  （`results`）を変えない——`peek` で停止が成立するケースは元実装でも
  その候補を `results` へ追加しないまま `break` していたため、pop の
  タイミングを条件判定の前後どちらに置いても最終的な `results` の内容は
  同一であり、下記「実装方針」節の「ラウンド 1 の bit 同一性」契約は
  保たれる。相違が生じるのは関数終了後の `candidates` ヒープの残存状態
  のみであり、これは再開型のみが参照する。**peek で停止し `candidates`
  に残ったノードの自己昇格経路**（Cursor Bugbot 指摘・PR #589）: この
  ノードは「上記『再開手順』の訂正——`discarded` からの除去は自己昇格
  成功時のみ」により、`candidates`（展開キューの在庫）に残ることと
  `discarded`（自己昇格の再評価対象）に残ることが独立に両立する。
  したがって次ラウンド以降、このノードが `candidates` から pop されて
  展開されるかどうかに関わらず、ラウンド先頭の自己昇格ステップが
  `discarded` を経由して毎ラウンド `worst_ok` を再評価し続けるため、
  `results`／`admitted` への復帰経路が `candidates` の pop タイミングに
  依存して失われることはない
- **メモリ上限**: visited・`expanded`・`in_candidates` はいずれも
  `⌈N/64⌉` 語（N は索引ノード数）のビットマップ、`candidates`／
  `discarded` の要素数はいずれも高々 N、`admitted` も高々 N。
  `k`・`ef` は構築済み `HnswIndex::search_masked` の検証（`MAX_EF` =
  10,000 以下）を経由済みの値のみを受け取る。無制限確保はしない
  （`coding-rust.md`「untrusted 入力の扱い」）
- **並行性**: `SearchProvider` trait は `search(&self, ...)` を要求し
  `Send + Sync` が前提のため、状態は `Mutex<Option<ResumeState>>`
  （`RefCell` は `Sync` でないため不可）で保持する。lock poisoning は
  fail-closed に「状態を捨てて `inner`（brute-force）へ委譲」する側へ倒す
  ——poison から回復して不整合な状態を使い続けない
- **無効化条件**（該当ラウンドは状態を破棄し既存の縮退経路へ倒す）:
  - ptr-eq 受理条件が外れた（`input.vectors`／`input.ids` が別バッファ。
    `search_delegates_to_inner_for_a_different_buffer` の既存契約と同型）
  - `k_r > MAX_EF`（既存の `ef_cap_fallbacks` 経路）
  - 解決形状が `PreparedHnswSearch::FullScan`（plain scan。索引探索を
    経由しない）
  - 索引探索エラー（`HnswError` 系）
- **`FullScan`／`ef_cap_fallbacks` ラウンドの状態**: 不要（厳密
  brute-force のため再開する探索状態自体が存在しない）。任意拡張として
  「1 回の全件スコアリング結果を保持し以後は prefix を伸ばして提供する」
  （疎側 #392 の `SparseScored::top` と同型で厳密かつ決定的）を候補として
  記すが、**本 Issue では採否を決めない**（#505 のスコープ外）

### 決定性契約: 「不変」の正確な等価クラス

Issue の見出し「再開型にしても融合結果・境界同点グループ・`fetch_k`
スケジュールが不変」を「現行の再実行型との bit 一致」と読むと、pgvector 型の
破棄候補ヒープ再開では**一般には成立しない**。根拠は `search_layer_in` の
受理判定 `worst_ok = results.len() < ef || score >= worst` が `ef` に
依存することにある。`ef₁` で不受理になったノードは現行実装では visited
マークのみ付けて捨てられる。`ef₂ > ef₁` の新規探索であれば同じノードが
**受理され、さらにその隣接ノードが新たに展開される**。再開型は破棄ヒープ
からそのノードを後で拾い直せるが、拾い直した時点での展開順序・visited
集合の状態・`results.pop()` の追い出し対象が、`ef₂` からの新規探索と
一致する保証はない。

このため本節は次の等価クラスを契約として定める（弱体化ではなく、成立範囲を
正確に記述するもの）:

1. **provider 非依存で不変**（無条件）: `hybrid.rs` の `fetch_k` 生成規則・
   `validate_extended_pool`・可視 id 検証（`core::provider_result_is_valid`）・
   `resolve_boundary_tie_group`／`complete_boundary_tie_group_by`・
   `rrf_fuse_with_limits`（`TieRank::GroupEnd`）・出力順（score desc・id
   asc の安定ソート。`docs/design/rrf-tie-break-determinism.md`）。
   `SearchProvider` trait のシグネチャは無変更のまま
2. **再実行型と bit 一致するラウンド**: (a) ラウンド 1（同一クエリの
   `search_layer_in` 1 回実行。破棄候補を捨てずに保持するだけでは
   `results` の中身は変わらない）。(b) exhaustive 終了時（候補ヒープ ∪
   破棄ヒープが空——マスク受理ノードの到達可能成分を全訪問した状態。
   `mask_splits_graph == false` により到達可能集合 ＝ 受理ノード全体
   （上記「DISTANCE 経路の `masked_short` 到達不能性」節の証明と同じ前提）
   のため、この状態は厳密 Top-k と一致する）
3. **一致を保証しないラウンド**: 非 exhaustive な ラウンド ≥ 2。上記の
   `worst_ok` の `ef` 依存が根拠。結果は「ANN 候補順序に対する近似」であり、
   これは上記「密 ANN 側の前方一致非保証」節（`ef` 拡大で prefix が
   入れ替わりうる）と同じ位置づけの近似であって、新たに緩める契約ではない
4. **帰結**: 同一クエリでも再開型と再実行型で**実現するラウンド数**が
   異なりうる（各ラウンドの返却列が異なれば、そのラウンドでの
   `TieBoundary` 判定——`Resolved`／`Undetermined`——も異なりうるため）。
   「`fetch_k` スケジュールが不変」とは `fetch_k` の**生成規則**
   （`dense_cap`・倍増式）が不変であることを意味し、実現ラウンド数の一致を
   主張するものではない
5. **同一索引・同一クエリ・同一世代での再現性**（`docs/design/
   hnsw-search.md`「決定性の保証範囲」）は再開型でも維持する: 全ヒープが
   `ScoredNode::Ord`（score desc・id asc）の全順序を持つこと、visited・
   ヒープの初期化がクエリ開始時点で決定的であること、クエリ内は単一
   スレッドで実行されること（スレッド非依存）
6. **返却列の契約**（再開型でも不変）: 毎ラウンド、全ヒットに対して
   スロット写像・`(tenant_id, id)` 照合・`kernel::dot` 再計算を行う
   （差分更新はしない。O(k·dim) のコストより fail-closed な単純さを
   優先する既存方針を維持）→ delta brute-force マージ → `sort_by`・
   `dedup`・`truncate(k)`。`validate_extended_pool` を通過する
7. **代替案（bit 一致を構成的に保証する案）**: 距離メモ再実行型——ノード→
   スコアのメモをラウンド間で保持し、走査自体（展開・受理判定）は毎回
   やり直す。展開を再開しないため性能面の主張（合計展開数 ≤ N）は成立
   しないが、dot 積計算の重複だけは削減でき、結果は現行実装と bit 同一に
   なる。#506 の実測で Recall 劣化や決定性上の懸念が出た場合の
   フォールバックとして本節に記録するが、**採否はオーナー判断**とする

### 停止性契約

- ラウンド数上限は既存の `dense_cap`・`MAX_FETCH_K`（= 40,000）による
  provider 非依存の有界性（「停止性・決定性」節のとおり
  `⌈log2(dense_cap / min(2·pool_depth, dense_cap))⌉ + 1`。
  `dense_cap < 2·pool_depth`（初期取得が既に `dense_cap` にクランプされる
  小規模・強選択性フィルタの場合）は 1 ラウンドに縮退し、
  `pool_depth = 200`・`dense_cap >= 2·pool_depth` の小〜中規模では 8 以下）
  を再開型でも変更しない
- ラウンド内の停止性: 各ヒープ pop は「停止条件成立」か「未訪問ノードの
  展開」のいずれかであり、visited は単調増加かつ N で有界なため各
  `run(ef)` は高々 N 回の展開で必ず停止する。**全ラウンド合計の実質的な
  隣接走査回数は高々 N**（再実行型は各ラウンドが visited をリセットする
  ため Σ_r visited_r になり得る）——これが再開型の性能面の狙いであり、
  #506 で実測する対象。この上限は上記「状態保持契約」の `expanded`・
  `in_candidates` ビットマップによる重複排除が前提であり、それなしでは
  成立しない: `discarded`（`results.pop()` で追い出されたノード）は
  `expanded` の有無を問わず存在しうるため、これを区別せず無条件に
  `candidates` へ再合流させると、既に `candidates.pop()` を経て展開済み
  のノードが複数ラウンドで繰り返し pop・隣接走査され得るため上限が崩れる
  （`expanded` フラグにより、そのようなノードは `candidates` へ再合流
  させない。一方 `expanded` 未設定のノードは、自己昇格〔`worst_ok`
  再評価 → `results`／`admitted` への O(1) 直接挿入〕の成否に**関わらず**
  `candidates` へ合流させる——自己昇格は「復帰」であって「展開」の代替
  ではないため、これを怠ると自己昇格したノードの隣接が永久に未発見のまま
  残り、上記「決定性契約」が前提とする exhaustive 終了時の到達可能成分
  全訪問が成立しなくなる。展開一覧の重複排除〔`expanded`〕と、未展開
  ノードの `results` 復帰〔自己昇格〕は独立した 2 つの操作であり、
  どちらか一方だけでは正しくない）。同一ノードが `candidates` ヒープへ
  複数回 push されて重複エントリになることは `in_candidates` ビットマップ
  （push 済みかどうかの在庫判定）で防ぎ、`candidates.pop()` 時の
  `expanded` 再検査（既に展開済みなら読み捨てる）を多重の安全策として
  併用する
- `exhaustive` 推論の健全性: `hybrid.rs` は `hits.len() < dense_fetch_k`
  から `exhaustive` を推論する。再開型でも「候補 ∪ 破棄ヒープが空のとき
  にのみ `k` 未満を返す」（fail-closed。空でないのに `k` 未満を返しては
  ならない）契約を維持する。既存の `masked_short` ガード
  （`index_hits.len() < min(k, visible_in_index)` で plain scan へ縮退）も
  残置し、切り詰め前の層 0 探索結果 `results` に対する不等式
  `results.len() >= min(ef_eff, visible_in_index)`（上記「DISTANCE 経路の
  `masked_short` 到達不能性」節の証明）と、それを `take(k)` で切り詰めた
  `index_hits.len() = min(results.len(), k) >= min(k, visible_in_index)`
  が再開型でも成立することを #505 で示す（`mask_splits_graph == false` に
  より受理ノード全体が到達可能であるため、この不等式は既存証明の系として
  成立する。`index_hits` 自体への下限は `ef_eff` ではなく `k` に対する
  ものであることに注意——`ef_eff` に対する下限は切り詰め前の `results`
  についてのみ成立する）
- `k_r > MAX_EF`: 既存契約どおり状態を破棄し `ef_cap_fallbacks` を計上して
  厳密 brute-force へ縮退する（変更しない）

### 実装方針（#505 向け・列挙のみ。本 Issue では実装しない）

- `hnsw.rs::search_layer_in` を「状態構造体 `LayerScanState { candidates,
  discarded, admitted, visited, expanded, in_candidates }` に対する
  `run(ef)`」へ再構成し、ラウンド開始時に `discarded` 全ノードへ
  `worst_ok` 再評価 → 満たすものは `results`／`admitted` への自己昇格を
  行い、**このときに限り** `discarded` から除去する。**自己昇格の成否と
  `discarded` からの除去を混同しない**（codex-review 指摘・PR #589。旧版は
  push 対象になったことを理由に自己昇格の成否によらず除去しており、失敗
  ノードが以後の自己昇格再評価から永久に外れ、結果件数の下限を満たせなく
  なる反例〔星型グラフ・上記「再開手順」節〕があった）。自己昇格の成否
  によらず、見つかった各ノードのうち `expanded` 未設定かつ `in_candidates`
  未設定のものを `candidates` へ push し `in_candidates` を立てる——この
  push は `discarded` の状態を変えない（自己昇格に失敗したノードは
  `discarded` に残ったまま `candidates` にも同時に存在し得る）
  （**自己昇格したノードも push 対象に含める**——上記「状態保持契約」
  参照。自己昇格を欠くと `candidates` の pop 処理がノード自身を
  `results` へ追加しないため discarded ノードが恒久的に失われ、逆に
  自己昇格したノードを `candidates` へ戻さないとそのノード自身の隣接が
  展開されないまま残る——どちらか一方だけでは不十分）。while ループの
  停止条件判定は `candidates.peek()` で行い
  （上記「状態保持契約」の「停止条件は `peek` で判定し」参照）、停止が
  成立する場合は候補を pop せず `candidates`／`in_candidates` に残したまま
  ラウンドを終える。停止が成立しない場合のみ `candidates.pop()` して
  展開し、pop 時には `in_candidates` を下ろし `expanded` を再検査して
  既に設定済みなら読み捨てる（重複 push が万一紛れ込んでも二重展開に
  ならない安全策）。既存の
  `search_layer_in`（現行の公開シグネチャ・呼び出し元）はその 1 回実行の
  薄いラッパとして維持する。アルゴリズム本体を複製しない（#494 の
  `Adjacency` ジェネリック・#490 の `PrefetchPolicy` をそのまま利用する）。
  **ラウンド 1 の bit 同一性**（再開型 `run(ef₁)` と現行
  `search_layer_in(ef₁)` の完全一致。`search_masked_none_matches_search`・
  `tests/hnsw_search.rs`・`tests/hnsw_cache.rs` の全件無変更 green）が
  受け入れゲートとなる
- `discarded` の保持は既存の `search_masked`（通常呼び出し）では行わない
  ——コスト・挙動を変えないため。再開型 API（例:
  `HnswIndex::search_masked_resumable`）を別途 `pub(crate)` として追加し、
  既存の `search`／`search_masked` の公開 API・エラー契約は変更しない
- `sql/hnsw_cache.rs::search_with_overlay` の解決済み経路に「再開ハンドル
  付き探索」を追加する（`prepare_*`／`search_prepared` が既に分離済みで
  ある構成を活かす）。統計に `hybrid_resumed_rounds`（再開により完走した
  ラウンド数）等の追加候補を挙げるが、テナント ID・行 ID・スコアは
  含めない（既存の統計方針を維持）
- `sql/hnsw_hybrid.rs::HnswDenseProvider` が `Mutex<Option<ResumeState>>`
  を保持し、上記「状態保持契約」の無効化条件を判定する
- 新規 `unsafe` は追加しない。新規依存も追加しない（`Mutex` は std）

### #505／#506 向け検証計画（列挙のみ）

- 単体（`hnsw.rs`）: ラウンド 1 の bit 同一（再開型 `run(ef₁)` vs 現行
  `search_layer_in(ef₁)`）、exhaustive 終了時の厳密性（brute-force 対照と
  完全一致）、同一入力に対する再現性、`discarded` を合流しない実装との
  差分が非 vacuous であること（合流しないと結果が変わる入力が存在する
  ことの確認）、**`discarded` からの除去を自己昇格成功時のみに限定する
  契約の回帰テスト**（上記「再開手順」節の星型グラフ反例に対応する
  固定フィクスチャで `ef` を 2→4→8 と複数ラウンド拡張し、各ラウンド後の
  `results.len()` が `min(k_r, visible)` を満たすこと・最終ラウンドで
  `masked_short` へ誤縮退しないことを固定。codex-review 指摘・PR #589）
- 結合（`tests/hnsw_hybrid_refetch.rs`・`tests/hnsw_cache.rs`）: 既存の
  停止性（`hybrid_rounds_max <= 8`）・複数ラウンドの実発生
  （`hybrid_rounds_max >= 2`）・3 回実行の bit 一致・既定エンジン対照
  Recall@10 ≥ 0.9・可視外テナント非混入・`hybrid_dense_searches > 0` を
  **無変更のまま green** で維持する
- 「再実行型との一致」の定義: 単一ラウンドで完了するクエリ
  （`hybrid_rounds_max == 1` を assert）と exhaustive 完了クエリについては
  融合結果の bit 一致を検証する。複数ラウンドかつ非 exhaustive なクエリは
  bit 一致ではなく契約プロパティ（ソート順・一意性・`len <= k`・可視集合
  内であること）と既存の Recall 基準で検証する
- #506（本 Issue のスコープ外）: `make bench-hybrid`（同点誘発コーパスの
  A/B。Issue #324 の方式）を交互 min-of-N（N ≥ 5）・ノイズ帯併記で前後
  比較し、`RECALL_ENGINE=hnsw` の 3 Recall ゲート（hybrid・rerank・
  query-planning）が同一閾値で通ることを確認する。共有 QEMU 環境の数値は
  採否根拠にしない（`docs/design/benchmark-judgement-policy.md` §5〜6）
- ガード: `make core-api-check`（`SearchProvider`／`VectorCore` の trait
  差分ゼロ）・`make sort-determinism-check`（`sort_by` のみの使用）

### 外部実装の参照（手法名・ライセンスのみ）

pgvector（PostgreSQL License）の `hnsw.iterative_scan` は上流 README で
確認できる設定として `strict_order`（結果を厳密に距離順に保つ）・
`relaxed_order`（順序をわずかに緩めて Recall を優先する）の 2 モードを持ち、
`hnsw.max_scan_tuples`（既定 20,000。訪問タプル数の近似上限。初回スキャンには
影響しない）・`hnsw.scan_mem_multiplier`（既定 1。`work_mem` に対する倍数
としてのメモリ上限）で打ち切り条件を持つ。本リポの hybrid 密側再取得
ループは各ラウンドで `hits` を丸ごと置き換える設計であり、`strict_order`
型（後続で見つかった近い候補のために既に確定した順序を保つ機構）に相当する
仕組みは不要である——`hybrid.rs` はラウンドの結果をマージ元として保持する
だけで、途中経過の順序保証を提供する契約を持たないため。

### セキュリティ考慮（OWASP Top 10 観点）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御の不備／テナント境界（P0） | 索引は `(table, ctx)` 可視アリーナのみから構築する契約は不変。再開状態はクエリ単位で同一 `prepared`（同一 base・overlay・マスク）に束縛され、ptr-eq 受理条件が外れたラウンドは状態を破棄し `inner` へ委譲する（fail-closed）。`NodeMask` による非受理ノード非通過（#409 の P0 条件）は再開時も同一の受理判定を経由する。`hybrid.rs` の可視 id 検証・`RlsSafetyNet` の多層防御は無変更 |
| 存在情報の副次漏えい | 追加統計案（`hybrid_resumed_rounds` 等）にテナント ID・行 ID・スコアを含めない。`EXPLAIN` へラウンド数・再開有無を露出しない（#411 の既存方針を維持） |
| 不安全な設計（DoS） | ラウンド数は `dense_cap`・`MAX_FETCH_K` で有界のまま。再開状態のサイズは索引ノード数 N で有界。合計展開数は再実行型以下（≤ N）になる設計であり増加方向の変更ではない |
| インジェクション | SQL 文字列の組み立てを伴わない（docs 専任） |
| untrusted 入力 | `k`／`fetch_k` の検証順序（`MAX_EF` 検証 → `effective_ef`）を変更しない。`unwrap`／`expect`／添字アクセスを production コードに持ち込まない方針を #505 の実装制約として明記する |
| 脆弱な依存 | 依存追加なし（`Mutex` は標準ライブラリ） |
| private spec 漏えい（P0） | 本節・関連コミット・PR は TASK-nn／ビヘイビア ID のポインタ表記のみ。Issue 本文の逐語引用を行わない |

### 実装記録（Issue #505）

上記契約に従い、`hnsw.rs::search_layer_in`（既存ホットパス）は複製・変更せず、
`HopMode::OneHop` 限定の独立実装 `ResumableMaskedSearch`
（`search_masked_resumable_start`／`search_masked_resume`）として実装した。
`search_layer_in` へ記録方針をジェネリック注入する当初案は、`bridge_expand`
（TwoHop・ACORN-1）との相互作用を精査する追加コストに見合わないと判断し
見送った——再開型は TwoHop レジームのラウンドを状態化せず既存の単発経路
（`search_masked_with_hop`）へ倒す（`sql/hnsw_cache.rs::search_prepared_resumable`
がレジームを見て分岐する）ため、`search_layer_in` 本体には一切触れていない。

- **停止条件の pop→peek 化**: 元実装は `candidates.pop()` してから停止条件を
  判定し、停止時は pop 済みの候補をそのまま捨てる。再開型は `candidates.peek()`
  で判定してから pop する（停止時は候補を `candidates` に残し次ラウンドで
  続行できるようにする）。単発実行の出力（`results`）はどちらの実装でも
  同一であることを `resumable_start_matches_search_masked_bit_identical`
  （ラウンド 1・複数 `ef`・マスク有無）で機械検証済み。
- **自己昇格の同値な縮約**: `results ∪ discarded` を `ScoredNode::Ord`
  （スコア降順・同点 id 昇順の全順序）で安定ソートし上位 `ef_eff` 件を
  `results` へ戻す操作は、「discarded 全ノードを best-first に再評価し
  `worst_ok` を満たすものだけ `results` へ挿入する」操作と同値である
  （`results` は定義上「常に全順序の top-`ef_eff`」に等しいため、独立した
  `admitted` 集合を持つ必要がない）。`ef_eff` は呼び出し元の整合検査で
  単調非減少が保証されるため、この再ソートは既存 `results` を一切降格
  しない。
- **候補復帰の二重 push 防止**: `expanded`／`in_candidates` はいずれも
  「一度立てたら降ろさない」ビットマップ（`VisitedBitmap::is_set` を新設し
  読み取り専用の照会に使う）。`expanded` が立ったノードは、`candidates` に
  過去積まれていたかどうかによらず恒久的に候補復帰の対象から外れる——
  「ノードは高々 1 回しか隣接走査されない」という元実装の不変条件（`visited`
  の discovery gate 由来）を再開型でも維持する。
- **星型グラフ回帰テスト**（`resume_star_graph_promotes_discarded_every_round`）:
  設計節「再開手順」の反例（起点 1 点にのみ全葉が接続）を固定フィクスチャ化し、
  `ef` を 2→4→8 と拡張しながら全 8 ノードが漏れなく最終結果へ現れることを
  確認した（候補復帰のみで自己昇格を怠る実装ではこのテストが red になる）。
- **`hnsw_cache.rs` 側の後段共有**: `search_with_overlay`（単発経路）の
  「結果件数充足検査・スロット写像＋`(tenant_id, id)` 照合・`kernel::dot`
  再計算・delta マージ・ソート/重複排除/truncate・成功統計」を
  `finish_indexed_search` として切り出し、単発・再開型の両経路が共有する。
  `finish_indexed_search` 自体は `ResumableMaskedSearch` に一切触れない——
  内部で plain scan／brute-force へ縮退しても、既にグラフ探索を終えて
  `resume` へ格納済みの状態は引き続き有効（次ラウンドも再開できる）ため。
  再開状態の破棄が必要なのは、そもそも `search_masked_resumable_start`／
  `search_masked_resume` を呼ばずに済ませた前段ガード
  （`search_prepared_resumable` 側。アリーナ不一致・`PlainScan`・
  `mask_splits_graph`・`k > MAX_EF`・`TwoHop`）だけである。
- **`HnswDenseProvider` の状態保持**: `Mutex<Option<HnswResumeState>>`
  （`SearchProvider::search` が `&self` シグネチャのため。`RefCell` は
  `Send + Sync` を満たせない）。poison 時は中身を捨てて `None` から始める
  fail-closed 設計（他ラウンドの panic による不整合な状態を使い続けない）。
  ptr-eq 受理条件が外れたラウンド（別バッファへの委譲）は本状態に一切
  触れない（`search_for_a_different_buffer_does_not_touch_resume_state` で
  固定）。
- **`hybrid_resumed_rounds` 統計**: `search_prepared_resumable` が
  「`resume` が `Some` だった側（再開経路）を通り、かつ
  `finish_indexed_search` が `fallbacks` を加算せず完走した」ラウンドのみ
  計上する診断用カウンタ。テナント ID・行 ID・スコアを含まない。
  `search_masked_resumable_start`／`hybrid_cache.rs` の単体テスト
  （一様分布コーパス・`k` を 10→20→40 と倍増）で非 vacuous であることを
  固定した。
- **SQL 表層経由の非 vacuous 固定を見送った理由（実測で確認した既知の制約。
  ※下記「原因の訂正（fix 923efcf）」で述べるとおり、この実測は候補復帰の
  走査対象漏れバグを含んだ実装によるものであり、現在の実装ではこの数値は
  そのまま成立しない。再測定は Issue #506 の担当のまま）**:
  Issue #410 の同点誘発コーパス（`quantize_levels: Some(2)`。
  `tests/hnsw_hybrid_refetch.rs`）に対して、変更前（`search_with_overlay`を
  毎ラウンド再実行する旧経路）と本 Issue の再開型経路を同一フィクスチャで
  比較実測したところ、**旧経路は 12 ラウンド中 `masked_short` 0 回（全ラウンド
  索引経由で要求件数を充足）だったのに対し、再開型経路は 12 ラウンド中 9 回
  `masked_short` へ縮退した**（`ef_search` を既定 64→400 に引き上げても
  変わらず）。個々のラウンドを追跡すると、再開型は 1 ラウンド目（`ef_eff=400`）
  こそ旧経路と同じ 400 件を返すが、2 ラウンド目（`ef_eff=800`）で 790 件に
  留まり、3・4 ラウンド目（`ef_eff=1600`／`3200`）でも **790 件から一切
  増えない**（`candidates` が完全に涸れ、`discarded` も自己昇格で
  空になり候補復帰する対象が無くなるため）。同条件で毎ラウンド新規に
  `search_masked_with_hop` を呼ぶ旧経路は 4 ラウンド目でも要求どおり
  3,200 件を返す——索引・エントリポイントは同一なので、これは索引の
  連結性の限界ではない。

  当時この実測に付けていた原因説明は誤りだった（後述「原因の訂正」参照）。
  実測時点の実装は、候補復帰ループの走査対象を `discarded` のみに限定
  しており、直前のラウンドで `discarded` から `results` へ自己昇格した
  ノードが候補復帰の対象から漏れ、そのノードの隣接が一切展開されない
  まま `candidates` が涸れて探索が停止するという**実装バグ**を含んでいた
  （`hnsw.rs` の該当箇所は fix 923efcf で修正済み。下記参照）。

  ### 原因の訂正（fix 923efcf）

  上記の実測後、codex-review・Cursor Bugbot が独立にこの箇所（自己昇格
  ノードのビーム展開漏れ）を指摘し、`hnsw.rs::search_masked_resume` の
  候補復帰ループを `discarded` 限定から `merged`（`results ∪ discarded`
  を全順序で再ソートした対象全体）へ拡張する形で修正した（コミット
  923efcf）。この修正により、自己昇格で `results` へ移ったノードも
  `expanded`／`in_candidates` が未設定であれば次ラウンドの候補復帰対象に
  含まれ、隣接探索の起点として再考されるようになった。

  現在の実装（`search_masked_resume`）が停止するのは、`visited`
  （`expanded` ビットマップ）が「一度立てたら降ろさない」という正しさ上
  必須の不変条件そのものではなく、`candidates` が空になった時点——
  `resumable_run` は `results.len() < ef_eff`（`peek` による停止条件判定）
  の間はスコアに基づいて展開を続け、`results ∪ discarded` の全ノードが
  `expanded` 済みになって初めて候補復帰の材料が尽きる。上記実測時点の
  「9/12 が `masked_short` へ縮退する」という具体的な数値は、この
  バグを含んだ実装によるものであり、fix 923efcf 後の実装でそのまま
  成立するとは限らない（探索フロンティアが自己昇格ノード経由でも
  拡張されるようになったため、涸れるまでの到達範囲は広がったはずだが、
  それでも旧経路——毎ラウンド `ef` を最初から大きく取って新規に探索する
  ——と比べて `visited` の単調性そのものに由来する到達範囲の差は理論上
  残り得る）。fix 923efcf を踏まえた再測定は Issue #506 の担当のまま
  未実施であり、下記の数値・結論はいずれも**修正前の実装によるもの**と
  読み替える必要がある。

  **SQL クエリの最終的な正しさへの影響は無い**（`finish_indexed_search`
  の `masked_short` 縮退は既存の fail-closed 契約どおり plain scan で
  埋め合わせるため、`tests/hnsw_hybrid_refetch.rs` の bit 決定性・
  `hybrid_rounds_max <= 8` はいずれも無変更のまま green）。一方で、
  この特性は「再開型探索が同点誘発コーパスで期待した高速化を実現できず、
  むしろ索引探索の作業と brute-force 縮退の両方を行う分だけ悪化しうる」
  ことを意味し、Issue #505 の目的（全ラウンド合計の隣接走査削減）を
  精度良く重複の多いデータで達成できていないことを示す実測結果である。
  `hybrid_resumed_rounds` の SQL 表層経由・同点誘発コーパスでの非 vacuous
  固定はこの制約下では成立しないため見送り、`crates/engine/src/sql/
  hnsw_cache.rs`・`sql/hnsw_hybrid.rs` の単体テストで一様分布コーパス
  （重複・同点が無い）における非 vacuous 性のみを固定した（上記参照）。
- **申し送り（Issue #506 以降。性能面の採否判断に必須の入力）**: 上記
  「原因の訂正（fix 923efcf）」のとおり、当初「exploration starvation」
  と説明していた現象の実体は候補復帰ループの走査対象漏れという実装
  バグであり、fix 923efcf で修正済み。したがって Issue #410 の
  同点誘発コーパスでの `masked_short` 悪化（9/12）を fix 後の実装で
  再測定し、修正がどの程度改善したか（改善が不十分な場合は残存する
  `visited` 単調性由来の到達範囲差への追加対応が必要か）を確認する
  ことが #506 の最初のタスクになる。前後比較実測（`make bench-hybrid`）
  は、Issue #410 の同点誘発コーパスに加えて一様分布コーパスでも必ず
  行い、両者の結果を区別して報告すること。
- **他ラウンドが失う既存の最適化（re-executing 経路のみが持っていた
  もの。実装記録・申し送り）**: 再開型経路（`resumable_run`）は
  `HopMode::OneHop` のビーム探索本体を独立実装したため、既存の
  ソフトウェアパイプライン先読み（Issue #490・`prefetch::PipelinePrefetch`）
  を一切行わない。また `search_masked_resumable_start`／`search_masked_resume`
  は常に dense な `VisitedBitmap` を使い、`ValidatedHnswParams::
  sparse_visited_max`（Issue #497 の opt-in）を一切参照しない——この
  opt-in を有効にしているユーザーでも、hybrid 密側の 2 ラウンド目以降は
  常に dense 実装が使われる（`HnswIndexCacheStats::sparse_visited_searches`
  は再開型ラウンドを一切計上しない）。結果は不変（ビット同一）だが、
  性能特性が変わりうる。両者とも #506 以降で採否を判断する。
- 性能面の採否・前後比較実測は引き続き Issue #506 の担当（本 Issue のマージ
  根拠は正しさ契約のみ——上記のとおり、性能面はむしろ既知の悪化シナリオが
  実測で確認されており、#506 でこの特性を踏まえた採否判断が必要）。

## 前後比較実測（Issue #506）

### 計測設計

既定の `make bench-hybrid`（`crates/engine/benches/hybrid_latency_bench.rs`。
Issue #324）は `hybrid::hybrid_search` を `ParallelSearchProvider` で直接呼ぶ
in-build 比較であり、#505 の実 seam（`sql::hnsw_hybrid::HnswDenseProvider`）を
一切通らない（CORE-7〔Issue #324〕・ANN opt-in Recall ゲート〔Issue #412〕と
同型の構造的制約）。#505 の変更経路へ到達できる唯一の production API は
SQL 表層（`EngineCore::from_storage_with_engine` ＋
`ORDER BY HYBRID(...)`）であるため、`BENCH_HYBRID_LATENCY_ENGINE` 環境変数
opt-in で起動する SQL 表層計測モード（`harness::hybrid_latency::
SqlHybridBenchFixture`〔統計・パース関数〕＋ `hybrid_latency_bench.rs` 側の
同型 fixture〔`mod temp_db;` の二重宣言〔`clippy::duplicate_mod`〕を避ける
ため実体は bench 側に置く〕）を追加した。既定モード（env 未設定）の出力は
本追加の前後でバイト単位不変（`make bench-hybrid` 後方互換）。

before／after は 2 コミット間の別バイナリで比較する（`sql::hnsw_hybrid::
HnswDenseProvider` に in-build 無効化トグルが無いため）。

- before: `838c53e`（#505 直前）
- after: `4ceb6b5`（#505 マージ済み。fix `923efcf` を含む）
- `git diff --stat 838c53e 4ceb6b5 -- Cargo.lock` は空（ビルド条件統一。
  `docs/design/benchmark-judgement-policy.md` §3）
- overlay: `crates/engine/benches/hybrid_latency_bench.rs`・`benches/harness/
  hybrid_latency.rs`（本 Issue のベンチ差分のみ）を before ツリーへコピーし
  `cargo build --release -p engine --bench hybrid_latency_bench` でビルド
  （production・`Cargo.lock` は各コミットのまま）。before ツリーには
  `HnswIndexCacheStats::hybrid_resumed_rounds` フィールド自体が存在しないため、
  `extract_counter`（Debug 文字列越しの薄いパーサ）でこの関数自体を両ツリーで
  コンパイル可能にしている——before バイナリの出力は `hybrid_resumed_rounds=n/a`
  になる

### 環境記録

```
nproc=12
cpu_model=QEMU Virtual CPU version 2.5+
cpu_flags_subset=fma f16c avx2
bench_dedicated_env=unset（共有 QEMU 環境。docs/design/benchmark-judgement-policy.md §5〜6 により
                             本実測は参考値扱い・専有環境再実測はオーナー作業）
before_commit=838c53e
after_commit=4ceb6b5
```

`AB_PAIRS=5`（`docs/design/benchmark-judgement-policy.md` §3 の下限）。各条件
1 プロセス = 1 条件（Issue #313 と同方針）、before → after の順で 5 ペア交互
実行。`scripts/bench_hybrid_latency_ab.sh` が各 run 直前の `loadavg`・同時実行
プロセス（`running_processes_excluding_self`・`top_cpu_processes`）を記録済み
（生ログは `target/bench-hybrid-latency-ab/<ts>/`。CI では退避されないため
本 doc の表が一次記録）。

### 実測結果（median_us・min-of-5／median-of-5、ratio = after/before）

| 条件 | before min／median | after min／median | ratio（min） | ratio（median） | 判定 |
| ---- | ---- | ---- | ---- | ---- | ---- |
| `ref_bf_large_tie5`（brute_force 参照区間） | 6612 / 6672 | 6593 / 6618 | 0.997 | 0.992 | ノイズ帯内（参照区間そのもの） |
| `hnsw_large_uniform`（一様分布・no_refetch） | 4075 / 4104 | 4425 / 4437 | 1.086 | 1.081 | 5/5 ペア一貫して悪化・両ノイズ帯超過 |
| `hnsw_large_tie5`（20,000 件・QUANTIZE_LEVELS=5） | 3903 / 3939 | 3866 / 3896 | 0.991 | 0.989 | ノイズ帯内 |
| `hnsw_410shape_tie2`（Issue #410 形状。4,000 件・dim=16・vocab=64・QUANTIZE_LEVELS=2） | 1669 / 1674 | 1356 / 1357 | 0.812 | 0.811 | 5/5 ペア一貫して改善・両ノイズ帯超過 |

参照区間（`ref_bf_large_tie5`）の run-to-run 幅（`(max−min)/min`。`docs/design/
benchmark-judgement-policy.md` §4 の参照区間実測ノイズ帯）: before 1.35%
（6701 vs 6612）・after 1.99%（6724 vs 6593）。固定 ±5% 帯・実測ノイズ帯
（約 1.3〜2%）のいずれも大きく超える変化のみを「一貫した差」として扱う。

per-run 生データ（`median_us`。実行順。5 ペア）:

```
ref_bf_large_tie5      before: 6701 6672 6697 6612 6615
                        after:  6642 6593 6724 6618 6611
hnsw_large_uniform      before: 4082 4075 4109 4104 4196
                        after:  4435 4467 4540 4437 4425
hnsw_large_tie5         before: 3958 3903 3916 3941 3939
                        after:  3896 3896 3866 3870 3898
hnsw_410shape_tie2      before: 1674 1679 1669 1675 1673
                        after:  1357 1360 1356 1363 1356
```

### `masked_short` 再測定（fix `923efcf` 後。#506 の最初のタスク）

全条件・全 run で `masked_short=0`（`hybrid_dense_searches` に対する比率
0/60〜0/240）——**before（`838c53e`。既に #410〔iterative scan 型〕・fix
`923efcf` を含む）・after（`4ceb6b5`）の双方で 0** だった。実装記録節が
記録した「再開型経路が 12 ラウンド中 9 回 `masked_short` へ縮退する」実測
（旧 `923efcf` 未適用の実装時点）は、fix 適用後の `838c53e`／`4ceb6b5`
いずれでも再現しない。fix `923efcf`（候補復帰ループの走査対象を
`discarded` 限定→`merged` 全体へ拡張）が実装記録の「候補復帰ループが自己
昇格ノードを走査対象から取りこぼす実装バグ」を解消し、再開型経路でも旧経路
（毎ラウンド新規探索）と同水準まで到達範囲を回復したことを実測で確認した
（`hnsw_410shape_tie2`: before/after とも `hybrid_dense_searches=240`・
`hybrid_rounds_max=4`・`masked_short=0`）。

`hybrid_resumed_rounds`（再開型経路が実際に前ラウンドの状態を再開した回数。
after 側のみ・`hybrid_dense_searches` 中の内数）:

| 条件 | after `hybrid_resumed_rounds` | 解釈 |
| ---- | ---- | ---- |
| `hnsw_large_uniform` | 0 | 実測（`hybrid_rounds_max=1`）は確認済み。`hybrid.rs` は境界（`pool_depth` 番目と `pool_depth+1` 番目）の同点グループが `TieBoundary::Resolved` になった時点で探索を終了する（`exhaustive` フラグ自体は `dense_fetch_k`（400）が可視集合全体（20,000）を下回るため false）ため、`hybrid_rounds_max=1` は初回 `fetch_k`（400）で可視集合全体を取り切ったことの証明にはならない。`no_refetch`（連続値・量子化なし）は境界の 2 件が厳密に同値になる確率が実質ゼロのため 1 ラウンドで `Resolved` すると推測されるが、ラウンド内訳ログは取得しておらず未確認（構造的に 0 が正しい挙動であること自体は変わらない） |
| `hnsw_large_tie5` | 0 | 実測（`hybrid_rounds_max=1`）は確認済み。20,000 件・QUANTIZE_LEVELS=5 でも `dense_fetch_k`（400）は可視集合全体（20,000）を大きく下回るため、上記と同じ理由で「可視集合全体を取り切った」ことにはならない。同点誘発コーパスであるにもかかわらず 1 ラウンドで境界が確定した理由（同点グループが `pool_depth` 境界をまたがなかったのか、他の要因か）は未確認のまま。同点誘発コーパスであっても本フィクスチャの規模・パラメータでは複数ラウンドへ到達しない（構造的に 0）という結論自体は不変 |
| `hnsw_410shape_tie2` | 180（5 run とも同一値。決定的） | 同点誘発コーパス・複数ラウンド（`hybrid_rounds_max=4`）に対し再開型経路が実際に発火することを固定 |

`hnsw_large_tie5` が resumed=0 になる事実は `hybrid_latency_bench.rs::
QUANTIZE_LEVELS`（Issue #324 が定めた既定の同点誘発強度。20,000 件規模）が
再開型経路の発火条件（複数ラウンドへの到達）を満たさないことを意味し、
Issue #410 の同点誘発コーパス（4,000 件・vocab=64・QUANTIZE_LEVELS=2。より
語彙が小さく密ベクトルの重複率が高い）のほうが再開型経路の性能特性を捉え
やすいことを示す。

### 判断

- **`masked_short` 悪化は fix `923efcf` により解消済み**（0/12→0/12。実装
  記録が懸念した残存する `visited` 単調性由来の到達範囲差は、少なくとも
  本フィクスチャでは観測されなかった）
- レイテンシは条件によって方向が割れる: 再開型経路が実際に複数ラウンド
  発火する条件（`hnsw_410shape_tie2`）では **min-of-5 で約 19% 改善**
  （両ノイズ帯を明確に超え 5/5 ペアで一貫）した一方、再開型経路が実質的に
  発火しない一様分布・単発ラウンド条件（`hnsw_large_uniform`）では
  **min-of-5 で約 8.6% 悪化**（同じく両ノイズ帯を超え 5/5 ペアで一貫）した。
  後者は `HnswDenseProvider` が `Mutex<Option<HnswResumeState>>` の
  ロック・状態確認を毎ラウンド（1 ラウンドのみでも）行うようになった
  固定コストの可能性が高いが、静的解析・アセンブリでの裏付けは行っていない
  （申し送り）
- 20,000 件・QUANTIZE_LEVELS=5 という Issue #324 の既存パラメータでは
  再開型経路が発火条件（複数ラウンド）に到達しないため、大規模コーパスで
  実際に再開が効く条件（大規模かつ複数ラウンドを要する）での実測は未実施
  のまま残る
- 実装記録が申し送った既存最適化の欠落（`PipelinePrefetch`〔#490〕未適用・
  `sparse_visited_max`〔#497〕不参照）は、`hnsw_410shape_tie2` の改善が
  それでも純便益として観測されたことから、少なくとも本フィクスチャでは
  改善効果を相殺するほどの影響は無いと推測されるが、直接計測はしていない
  （申し送り）
- 共有 QEMU 環境の 1 回の N=5 ペア実測であり、`docs/design/
  benchmark-judgement-policy.md` §5〜6 により **参考値**（採否根拠にしない）。
  専有環境（`BENCH_DEDICATED_ENV=1`）での再実測をオーナーへ申し送る

### Recall 3 ゲートの同一閾値検証（Step 4）

`docs/spec`（private submodule）が本環境にチェックアウトされておらず、
`HYBRID_RECALL_MIN_*` 等の spec 由来閾値を注入できない（環境変数未設定）。
`RECALL_ENGINE=hnsw RECALL_VERBOSE=1 cargo test --release -p engine --test
hybrid_recall -- --ignored --nocapture` を実行したところ、閾値ゲートは
契約どおり **明示的な no-op**（`HYBRID_RECALL_MIN_R20_SMALL not configured;
gate not enabled` 等。fail ではない）として完了し、`RECALL_VERBOSE=1` でも
閾値未設定時は実測値そのものを計算・出力しない（測定本体が閾値ガードの
内側にあるため）。したがって本環境では新規の Recall 実測値は得られていない。

一方 `tests/hnsw_hybrid_refetch.rs`（本 Issue で `hybrid_resumed_rounds >= 1`
の非 vacuous アサーションを追加。下記参照）・`tests/hnsw_cache.rs` の既存
テストは brute-force 対照 Recall@10 ≥ 0.9・可視外テナント非混入を無変更の
まま固定しており、Issue #412／#515 が記録した「brute_force／hnsw／hnsw_f16
の全 8 測定点で Recall 実測値が完全一致」という既存結果（`docs/design/
ann-recall-gate-verification.md`）は、#505 が SQL 表層の Recall セマンティクス
（`finish_indexed_search` の `matches_all`・fail-closed 縮退契約）を一切
変更していないことから、そのまま成立すると判断する。`RECALL_ENGINE=hnsw`／
`hnsw_f16` の実閾値評価（マージ後の `recall.yml` schedule/dispatch 実行）は
管理者作業として申し送る。

### `hybrid_resumed_rounds` の非 vacuous 固定（Step 5）

上記実測で `hnsw_410shape_tie2`（SQL 表層・同点誘発コーパス）の
`hybrid_resumed_rounds` が 5 run とも 180（安定して `>= 1`）だったため、
`tests/hnsw_hybrid_refetch.rs::run_tie_inducing_corpus_hybrid_search_
terminates_and_is_deterministic`（F32／F16 共有本体）へ
`stats.hybrid_resumed_rounds >= 1` のアサーションを追加した（既存の停止性・
決定性アサーション・fixture パラメータは無変更）。

### 申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測（採否確定に必須）
- `hnsw_large_uniform` で観測された単発ラウンドでも生じる固定コスト
  （推定 `Mutex<Option<HnswResumeState>>` のロック・状態確認）の静的解析・
  実アセンブリでの裏付けと、必要なら軽量化（例: ラウンド数が既知で 1 の
  場合は状態管理をバイパスする等）
- 大規模コーパスで再開型経路が実際に複数ラウンド発火する条件（規模・
  QUANTIZE_LEVELS の組み合わせ）を探索したうえでの追加実測
- `PipelinePrefetch`（#490）適用・`sparse_visited_max`（#497）参照・
  `HopMode::TwoHop`（#501）統合による再開型経路の追加最適化（承認なしに
  起票しない別 Issue 候補）
- Recall ゲート閾値による hnsw／hnsw_f16 の実 run（マージ後の管理者作業）
- 親 Issue #503 への採否判断（`masked_short` は解消済み・レイテンシは
  条件依存で改善/悪化が両方観測されているため、単純な Accept/Reject では
  なく条件別の適用方針の検討を推奨する、というのが本実測からの示唆）

## Issue #2033: 反復スキャン設定（`hnsw.iterative_scan`・`hnsw.max_scan_tuples`）の SQL 公開

親 #2031・SQL-73 のポインタ。設定の構文・範囲・既定値は README「HNSW の探索幅」節の公開済みの範囲に従う。

### 判断

| 論点 | 判断 |
| ---- | ---- |
| 取りこぼしの発生範囲 | 既定の評価順（RLS → SCALAR → DISTANCE）はもともと `min(k, 一致行数)` 件を返す。取りこぼすのは `HINT ORDER(DISTANCE, ...)`（SCALAR 事後フィルタ）の場合だけ。反復の対象はこの経路（HNSW を使える `Ranking::Distance` かつ WHERE あり）に限る |
| ハイブリッド | Issue 本文は `hnsw_hybrid.rs` への伝播を挙げるが、SQL-73 (4) は「ハイブリッドの結果を変えない」と定める。SSOT に従い、ハイブリッドにはセッション値を渡さず（`core.rs` が既定値へ置き換える。`ef_search` と同じ扱い）、`hnsw_hybrid.rs` の挙動は変えない |
| 厳密走査 | 厳密走査（HNSW 不適格・opt-in なし・opclass 不一致・`DESC`・`LIMIT` 省略・precision）の DISTANCE 先行は設定の対象外で、件数不足のまま |
| `off` | 反復せず、HNSW を使える経路の DISTANCE 段の取得件数を実効 `ef_search` で打ち切る（経路内部の縮退によらず件数を揃える。#2030 で #2031 側に残した「k > ef の打ち切り」の担当） |
| `relaxed_order` | 受理するが `strict_order` と同じ動作（出力は常に距離順。「順序の小さな入れ替わりを許す」契約の範囲内） |
| 走査数の定義 | 反復の各回で DISTANCE 段へ要求した候補数。上限は `min(max_scan_tuples, SQL-46 の走査行数上限, MAX_SEARCH_K, 可視行数)`。provider の `k` の契約上限が 10,000 のため、既定 20,000 の実効上限は 10,000 になる |
| 反復 | 取得件数 `k_round` を倍々（`saturating_mul(2)`）に増やし、毎回 SCALAR 事後フィルタをかける。終了条件は「通過件数 ≥ `OFFSET + LIMIT`」「可視集合を使い切った」「上限到達」。回数は高々 ⌈log2(上限 / 目標件数)⌉ + 1。各回の前に文の期限を確認する。上限到達はエラーにしない |
| 実装 | `sql::exec` の DISTANCE 段と事後フィルタをクロージャへ切り出し、反復で再利用した（各回は既存の `search_or_fallback` 等＝キャッシュ済み索引を引く） |
| `SET LOCAL` | `hnsw.ef_search` と同じく `0A000`。spec は受理側のため、3 設定をまとめて対応する別 Issue が必要 |
| 事後フィルタの評価エラー | 反復で評価する行が増えるため、以前は評価されなかった行の式エラー（ゼロ除算等）が表に出うる。PG と同じ動作 |
| テナント境界 | 反復は RLS 適用後のアリーナ・`slot_ids` の中だけで行い、各回の結果を既存の可視 ID 検証と安全網に通す。応答に走査数・回数は出さない（統計は集計値でテナント・行 ID を含まない）。tenant-b の行数（0 / 1,000）を変えても tenant-a の結果が変わらないことをテストで固定 |

### 計測（`tests/sql_t40_up/sql2033_hnsw_iterative_scan_behavior.rs`）

tenant-a 4,000 行・tenant-b 1,000 行・次元 16・HNSW opt-in・`HINT ORDER(DISTANCE, SCALAR, RLS)`。

| 選択率 | k | `strict_order`／`relaxed_order` の返却件数 | `off`（k=10・1%） |
| ------ | - | ------------------------------------------ | ------------------ |
| 1%・5%・10%・50% | 10・100 | 全クエリで `min(k, 一致行数)`（充足率 1.000） | 件数不足（k=10 に届かないクエリがある） |

`max_scan_tuples = 20` では 0.3% の述語で部分結果を返し（エラーなし）、`iterative_scan_cap_hits` が増える。

### 対象外

`SET LOCAL hnsw.*`、`EXPLAIN ANALYZE` での反復回数・上限到達の表示（SQL-76）、`relaxed_order` 固有の最適化、`RESET ALL`／`DISCARD ALL`、NoSQL（HTTP）表層からの設定（既定値で動く）、選択率しきい値による厳密走査への自動切替の EXPLAIN での動的表示。
