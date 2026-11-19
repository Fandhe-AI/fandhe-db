# RRF 融合結果の同点タイブレーク: 現状維持の判断記録

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-84（Issue #61。ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-103（RRF 融合。Issue #36 / PR #144 でマージ済み）・TASK-75（SQL-4 表層統合。
  Issue #56 / PR #184 でマージ済み）
- 関連ビヘイビア: SEARCH-1・SEARCH-3・SQL-4（ポインタ: `docs/spec/04-behavior/search.md`・
  `docs/spec/04-behavior/sql-surface.md`）
- 出典: PoC-10（`docs/spec/03-poc/query-protocol-comparison`。private spec 側の PoC
  実装。本文は転記せずポインタのみ参照）

## 目的

PoC-10（詳細は非公開のため本文へ転記しない。ポインタのみ参照）で確認された
RRF 融合スコアの同点タイブレークに関する非決定性の懸念が、本リポの `engine`
クレートへ持ち込まれていないかを判定し、判定結果を記録する。修正が必要な場合は
修正し、不要な場合は根拠とともに現状維持の判断を残す（TASK-84 の成果物は
「`hybrid.rs` の修正」または「現状維持の判断記録」のいずれか）。

## 調査結果

`crates/engine/src` のスコア順ソート経路をすべて確認した。

| 経路 | ファイル:関数 | 現状 |
| ---- | ------------ | ---- |
| 密 Top-k 選出 | `kernel.rs`（`MinHeapItem::cmp`・`TopKSelector::into_sorted_vec`） | ヒープ比較・最終ソートともスコアは `total_cmp`、同点は id 昇順でタイブレーク。最終整列は安定ソート（`sort_by`） |
| 並列・バッチ選出 | `parallel_search.rs`・`batch_search.rs` | いずれも `TopKSelector` を共用するため分割数・スレッド数に依存しない |
| 疎（BM25）Top-k | `sparse.rs`（`Candidate::cmp`） | `total_cmp` + `doc_id` 昇順、`sort_by`（安定） |
| RRF 融合 | `hybrid.rs`（`rrf_fuse`） | 当時（本 ADR 作成時点）は累積が `HashMap` ではなく `BTreeMap`（走査順序が id 昇順で決定的）だった。Issue #549 で id ソート済み位置索引（`(id, pos)` タプルの比較関数なし `sort_unstable()`。後述「追記（Issue #549）」節参照）へ置換したが、最終整列は変わらず `sort_by`（安定）+ スコア降順・id 昇順タイブレーク |
| 融合結果の `LIMIT` 適用 | `hybrid.rs`（`hybrid_search`） | `rrf_fuse` が返す順序をそのまま `truncate(k)` するのみで、`truncate` 自体は順序を変えない |
| 再ランク | `rerank.rs` | 同様に `BTreeMap` 累積 + `sort_by_key`（安定） |
| SQL-4 実行 | `sql/exec.rs` | `hybrid_search`/`rrf_fuse` の順序をそのまま `LIMIT` 相当の件数へ切り詰めるのみ |
| SQL `DISTINCT ON` グループ代表 | `sql/scan.rs`（経路 (B) の `groups`） | 整列済み `Vec` + `binary_search_by`。同点は id 昇順 → tenant_id バイト順で決まり、不安定ソートは使わない（Issue #3262。`select-distinct-on.md` §6） |

`crates/engine/src` に `sort_unstable_by`／`sort_unstable_by_key`／
`select_nth_unstable_by(_key)` の使用は 0 件（`scripts/check_sort_determinism.sh` で
機械確認）。`sort_unstable`（比較関数なし）の使用は `storage.rs` の整数 id 列 1 件のみで、
全順序を持つ整数値の並び替えであり同点タイブレークの問題を生じない。

入力側の契約検証も揃っている: `hybrid.rs` は密・疎双方の入力が「スコア降順・同点は
id 昇順」で事前ソート済みであることを検証し（`is_sorted_desc_id_asc`）、非有限スコアを
拒否し、同一リスト内の id 重複を拒否する。

## 判断

**修正不要・現状維持**。`engine` 本体の RRF 融合経路は、PoC-10 が指摘した非決定性の
原因（詳細は転記しない。ポインタのみ参照）を持たない。

判断を将来にわたって検証可能にするため、以下を本 PR で追加する（`hybrid.rs` 自体の
ロジックは変更しない）:

1. **回帰テスト**（既存テストが薄かった「同点グループが `LIMIT`/`k` 境界を跨ぐ」
   ケースを補う）
   - `crates/engine/src/hybrid.rs`（`mod tests`）:
     `rrf_fuse_multiple_tie_groups_are_ordered_score_desc_id_asc`
   - `crates/engine/src/kernel.rs`（`mod tests`）:
     `tied_scores_are_ordered_by_id_ascending_when_multiple_survive`
   - `crates/engine/tests/hybrid.rs`:
     `hybrid_search_tie_group_across_limit_boundary_is_deterministic_and_matches_oracle`
     （`ParallelSearchProvider` での 20 回反復実行の bit 一致 + 独立オラクルとの一致）
   - `crates/engine/tests/sql_surface.rs`:
     `sql4_hybrid_tie_group_across_limit_boundary_is_deterministic`（SQL-4 end-to-end
     で同一の反復実行検証 + `hybrid_rrf(...)`/`HYBRID(...)` 2 構文形の一致）
2. **軽量ガードスクリプト**（`scripts/check_sort_determinism.sh`。`make
   sort-determinism-check`・CI の `sort-determinism-check` ジョブから実行。詳細は
   スクリプト冒頭コメント参照）: `sort_unstable_by`/`sort_unstable_by_key`/
   `select_nth_unstable_by(_key)` の再混入を機械的に検知する。

## 維持すべき不変条件

以後 `hybrid.rs`・`kernel.rs`・`sparse.rs`・`rerank.rs`（および将来 wire 経由で結果を
再ソートする経路を追加する場合はその経路も含む）が壊してはならない契約:

- スコア順に並べる箇所は必ず**安定ソート**（`sort_by`/`sort_by_key`。`sort_unstable_by`
  系は使わない）を使う。
- 同点（スコアが等しい）要素のタイブレークは常に **id 昇順**とする。
  複数テナントを 1 バッチで扱うバッチ検索経路（`batch_search.rs`・`gpu_batch.rs`）
  では行の一意キーが `(tenant_id, id)` であるため（PR #205/#228）、タイブレーク基準も
  `(tenant_id, id)` 昇順へ拡張される。ただし**実装上の基準は選出段の候補識別子
  （常駐行列の行スロット番号）昇順**であり、`ResidentMatrix::build` は渡された行を
  並べ替えないため、両者が一致するのは「行を `(tenant_id, id)` キー順で常駐行列へ
  渡す」という事前条件を呼び出し元が満たす場合に限る（`Storage` の行キー順で
  スキャンした結果をそのまま渡せば自然に満たされる。`ResidentMatrix::build` の
  ドキュメンテーションコメント「事前条件: 行の並び順」参照）。事前条件を満たさない
  並びでも結果はスロット昇順で決定的であり、非決定性は生じない。単一テナント内では
  従来どおり id 昇順に退化し、CPU 経路・GPU 経路の間で順序が食い違わないことは
  `FallbackBatchEngine::revalidate_primary_hits` の順序検証で機械的に担保される。
- スコアの比較には（NaN を含む非全順序を扱う場合）`total_cmp` を使い、非有限値は
  比較前に事前拒否する（`hybrid.rs`・`kernel.rs`・`sparse.rs` の既存契約と同じ）。
- RRF 等の融合スコアを id ごとに累積する構造は、`HashMap`（走査順序が非決定的）を
  使わず決定的走査順序を持つ構造を使う（`BTreeMap`、または Issue #549 の id
  ソート済み位置索引のように id 昇順へ明示的に整列した構造）。
- `LIMIT`/`k` による切り詰め（`truncate`）は、切り詰め前の順序が上記契約を満たして
  いる限り、それ自体は非決定性を生まない（`truncate` は要素の順序を変更しない）。
  ただし切り詰め前の順序が同点タイブレークを欠いていれば、どの位置で切っても
  非決定的になりうる点に注意する。
- 同点グループへ割り当てる RRF **順位**（同点タイブレーク＝最終出力の**並び順**とは
  別の契約）は `hybrid.rs::RrfConfig::tie_rank`（`TieRank`。既定 `GroupEnd`）で決め、
  `pool_depth` 境界で同点グループを分断しない（`complete_boundary_tie_group`。
  Issue #310。詳細は `docs/design/hybrid-recall-regression.md`「Issue #310: engine
  側改善」節参照）。この規約変更は上記の同点**タイブレーク**契約（安定ソート・
  id 昇順・`total_cmp`・決定的走査順序を持つ累積構造）を弱めない:
  `compute_contributions`（Issue #549 以前は `accumulate_ranked`）が
  同点グループ内の全メンバーへ同一の RRF 順位（＝同一の寄与）を割り当てた**後**の
  最終整列は、従来どおり融合スコア降順・同点 id 昇順の安定ソートで確定する。

## 追記（Issue #393）: 疎（BM25）側の転置索引化に伴う不安定ソート API 導入

`SparseIndex` の転置索引化（Issue #386 Phase 1）のうち、文書長クラス別テーブル参照 +
2k バッファ選出（Issue #391）・再取得ループの再スコアリング回避（Issue #392）で、
`sparse.rs::Candidate`（スコア `total_cmp` + `doc_id` 昇順の明示的タイブレークを持つ
全順序型）に対する `select_nth_unstable_by`／`sort_unstable_by` を `// sort-determinism:
allow` マーカー付きで導入した（上表「疎（BM25）Top-k」行の `sort_by` は当時の記述であり、
現在は `TopKSelector::shrink_to_k_eff`・`SparseScored::top` がこの 2 API を使う）。
`Candidate::Ord` が同点の相対順序も含めて一意に定まる全順序であるため、不安定な選出・
整列でも最終的な id 順は変わらず、本 ADR の同点タイブレーク契約（安定ソート＋id 昇順）は
弱まらない。この判断の固定テストは `crates/engine/tests/sparse_determinism.rs`
（Issue #393。`search_within`/`score_within().top(k)` のビット一致・同点グループの
前方一致契約・投入順非依存・`hybrid_search` の疎側境界同点グループ決定性を検証）を
参照。

## 追記（Issue #549）: RRF 融合コアの id 写像を位置索引方式へ置換

`hybrid.rs::rrf_fuse_with_limits` の融合コアが、id をキーにした `BTreeMap`
累積（ノード確保を伴う）から、検証済み長さの位置索引方式（`compute_contributions`
が密・疎それぞれの寄与を位置ごとに `contrib: Vec<f64>` へ書き込み、`index:
Vec<(u64, usize)>` を比較関数なし `sort_unstable()` で id 昇順へ整列してから
等 id 区間を合算する）へ置換された（詳細・等価性検証は `docs/design/
hybrid-rrf-latency-breakdown.md`「Issue #549」節参照）。

`index.sort_unstable()` は `(id, pos)` タプルの `Ord`（id 優先・`pos` は同一 id
内で一意）に従う比較関数なしの呼び出しであり、`(id, pos)` が要素ごとに一意な
ため不安定性（同値要素の相対順序不定）は観測されない。`scripts/
check_sort_determinism.sh` の検知対象（`sort_unstable_by`/`sort_unstable_by_key`/
`select_nth_unstable_by(_key)` への識別子参照）には比較関数なしの `sort_unstable`
は含まれないため、`storage.rs` の整数 id 列と同じ「許可が必要な正当ケース」の
枠組みに入る（下記「例外として許容する箇所」参照）。最終スコアソート
（`out.sort_by`）は本 Issue でも安定ソートのまま変更していない。

## 例外として許容する箇所

- `storage.rs` の整数 id 列に対する `sort_unstable`（比較関数を渡さない版）。id は
  全順序を持ち、同一 id の重複が意味を持つペイロードでもないため、不安定ソートでも
  結果の並びは一意に定まる。`scripts/check_sort_determinism.sh` はこのケースを検知
  対象パターン（比較関数を伴う `_by`/`_by_key` 系のみ）から除外している。
- `hybrid.rs::rrf_fuse_with_limits` の `index: Vec<(u64, usize)>` に対する
  `sort_unstable`（比較関数を渡さない版。Issue #549）。`(id, pos)` は要素ごとに
  一意なタプルであり、上記の `storage.rs` と同じ理由で不安定性は観測されない。
  上記「追記（Issue #549）」節参照。
- `sparse.rs::TopKSelector::shrink_to_k_eff`・`SparseScored::top` の
  `select_nth_unstable_by`／`sort_unstable_by`（`// sort-determinism: allow` マーカー
  付き。Issue #391・#392）。上記「追記（Issue #393）」節参照。

## スコープ外・申し送り

- PoC 側（`docs/spec/03-poc/query-protocol-comparison`）の実装は spec リポ管理のため
  本リポでは変更しない。
- wire-server 側で結果順序を再ソートする経路は現時点で存在しない。将来 wire 経由の
  結果返却を実装する際も、本 ADR の順序契約（安定ソート + id 昇順タイブレーク）を
  維持すること。
- `sort-determinism-check` の branch protection 必須チェック登録はユーザー作業
  （既存 `core-api-check` と同運用。PR 本文参照）。

## 参照

- TASK-84（ポインタ: `docs/spec/05-tasks.md`）
- SEARCH-1・SEARCH-3（ポインタ: `docs/spec/04-behavior/search.md`）
- SQL-4（ポインタ: `docs/spec/04-behavior/sql-surface.md`）
- `crates/engine/src/hybrid.rs`・`crates/engine/src/kernel.rs`・
  `crates/engine/src/sparse.rs`・`crates/engine/src/rerank.rs`
- `scripts/check_sort_determinism.sh`
