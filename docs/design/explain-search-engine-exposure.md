# `EXPLAIN` への検索エンジン種別・ANN 適用判定の露出（Issue #411）

## ステータス

Implemented（2026-09）

## 背景

ADR [`ann-index-adoption.md`](./ann-index-adoption.md)（Issue #367／#403・B 案）の
受け入れ基準は「`EXPLAIN` へ使用エンジン種別を露出し、どの経路で結果が得られたか
追跡可能にする」ことを要求する。`sql/explain.rs`（TASK-78・SQL-6）は従来
`search_terms[i]` → `path_hint` → `kind_hint` → `mode` → `mode_source` の 6 行のみを
返し、`sql/exec.rs` が内部で判定している 4 つの ANN 適用条件（`hnsw_full_visible_
eligible`／`hnsw_subset_eligible`／`hnsw_hybrid_full_visible_eligible`／
`hnsw_hybrid_subset_eligible`。#408・#409・#410）をクライアントから確認できなかった。

## 決定

### 1. `EXPLAIN` は実行なしを維持し「静的な計画」のみを報告する

実行時の plain scan への縮退（可視カーディナリティ比・マスク分断・結果不足による
fail-closed 縮退）は RLS 可視アリーナの走査後に決まり、これを `EXPLAIN` で求めるには
アリーナ構築・索引の `prepare_*`（構築・統計加算という副作用を持つ）が必要になる。
これは「`EXPLAIN` は検索本体を実行しない」という既存契約
（`sql/explain.rs` モジュールドキュメント・`tests/sql_explain.rs::
explain_does_not_write_or_execute_search`）に反する。

そのため `EXPLAIN` は**クエリ形状とエンジン設定から決まる静的な適用判定**
（索引経路に載る形状か・plain scan か、その理由）のみを報告する。実行時の
fail-closed 縮退・hybrid 密側再取得ラウンド数（`docs/design/
hnsw-hybrid-iterative-scan.md`）は対象外とし、集計観測は
`EngineCore::hnsw_index_cache_stats()`（Rust API）に委ねる。この判定はテーブル
内容に一切依存しないため、存在情報の副次漏えいが構造的に起きない。

### 2. 適用判定の単一情報源化

`sql/hnsw_cache.rs` に `AnnPlan`（`PlainScanEngine`／`PlainScanPrecision`／
`HnswFullVisible`／`HnswSubset`）・`AnnShapeInput`・`classify_ann_plan` を追加した。
`sql/exec.rs` の 4 boolean（`is_hybrid` の真偽による組み合わせ）はこの関数の戻り値
から導出し、`sql/explain.rs` も同じ関数を呼ぶ。「並行する推測」ではなく単一実装の
共有により `EXPLAIN` の回答が executor の実際の判定と乖離しない。同値性は 32 通り
（2^5: `hnsw_enabled`・`is_hybrid`・`is_precision`・`filters_empty`・
`scalar_prefilter`）全数の判別テストで固定した。

### 3. `EXPLAIN` の `WHERE` 形状の取り方

`sql::using_plan::pre_check_bindable` の戻り値を `PreCheckShape { filters_empty }`
へ拡張した。`filters_empty` は束縛後の `metadata_filters.is_empty() &&
expr_filters.is_empty()` であり、`stmt.where_predicates().is_empty()` とは**異なる**
（`WHERE visible()` は `WherePredicate::PredicateCall` として `rls_predicate_present`
フラグのみを立て `metadata_filters`／`expr_filters` を増やさないため）。この差分を
`explain_reports_hnsw_full_visible_ann_plan_with_only_visible_predicate` で固定した。

### 4. `USING PLAN` は `HINT ORDER` を受理しない（既存の許可リスト制約）

`sql::allowlist::parse_select_shape` は `USING PLAN` 経路で `HINT ORDER(...)` を
構造上受理しない（ランキング自体を展開結果が決めるため、既存コメント参照）。
そのため `EXPLAIN`（`USING PLAN` 専用構文）の `evaluation_order` は常に
`EvaluationOrder::DEFAULT`（SCALAR 先行）であり、`ExecutionPlan::scalar_prefilter`
は `EXPLAIN` 経由では常に `true`。`classify_ann_plan` の `!scalar_prefilter` 分岐
（DISTANCE 先行時の `FullVisible` 判定）は `EXPLAIN` からは到達しないが、`sql/
exec.rs`（`ORDER BY` 経路の `HINT ORDER` を受理する）からは到達するため、関数自体
は削除しない。この制約は判別テスト
`explain_rejects_hint_order_combined_with_using_plan`（`42601`）で固定した。

### 5. 出力行の仕様（安定契約）

既存 6 行の文言・順序は不変。`mode_source` の直後へ以下を追記する（新規行は
追記のみで既定エンジン時の出力は変更前と後方互換。TASK-164 で `mode_source` を
追加した前例と同じ方針）。

| 行 | 常時／条件 | 値の語彙（snake_case・英語・閉じた集合） |
| -- | -- | -- |
| `engine: <token>` | 常時 | `parallel_brute_force` / `cpu_scalar_brute_force` / `hnsw` / `(custom_provider)`（`search_engine_kind() == None`。`with_provider`／`from_storage` 経由） |
| `hnsw_params: m=<m>,ef_construction=<ef_c>,ef_search=<ef_s>,resident=<f32\|f16\|i8>,sparse_visited_max=<n>,metric=<cosine\|l2\|ip\|l1>,iterative_scan=<off\|relaxed_order\|strict_order>,max_scan_tuples=<n>` | `engine: hnsw` のときのみ | 構築時の静的設定値のみ（`full_scan_ratio` は含まない）。`m=`・`ef_construction=` は、HNSW を使える宣言が `WITH (m, ef_construction)` を持つとき宣言の値（Issue #2029・SQL-33 (5)。実行時の構築と同じ値）、それ以外は provider の値。`ef_search=` は、非ハイブリッドの kNN ではセッションの `hnsw.ef_search` の実効値になる（Issue #2030・SQL-73 ポインタ。上書きなしは構築時の値、ハイブリッドは常に構築時の値）。接続自身が設定した値かエンジンの静的な値であり、テナントのデータ・存在には依存しないため、この露出の契約に反しない。末尾の `iterative_scan=`・`max_scan_tuples=` は、非ハイブリッドの kNN ではセッションの `hnsw.iterative_scan`・`hnsw.max_scan_tuples`（`max_scan_tuples` は SQL-46 の走査行数上限との min の実効値）、ハイブリッドは常に既定値（`strict_order`・20000 を走査上限で頭打ちにした値。Issue #2033・SQL-73 ポインタ）。`ef_search=` と同じく接続自身の設定値で、テナントのデータ・存在には依存しない。`metric=`（Issue #1978・INDEX-7 追記）は対象表の HNSW 宣言の距離種別（opclass）で、宣言なしは `cosine`（宣言が複数種類あるときは `cosine` を含めば `cosine`、含まなければ種別の順序で最小のもの）。Issue #1979 で演算子単位になり、HNSW を使えるときは問い合わせの距離種別、距離種別の不一致で厳密走査へ縮退したときは不一致の理由が分かる宣言側の値になる（`ann_plan:` 行と同じゲート結果から決める）。`resident=`（Issue #514 追記）は要求精度の静的値で、実行時の自動縮退結果は含まない。`sparse_visited_max=`（Issue #497 追記）は visited 集合切替閾値の静的設定値（`ValidatedHnswParams::sparse_visited_max`。既定 0）で、実行時にどちらの visited 実装が選ばれたか・可視候補数・索引ノード数は含まない |
| `ann_plan: <token>` | 常時 | `plain_scan_engine` / `plain_scan_precision` / `hnsw_full_visible` / `hnsw_subset` / `unknown_custom_provider`（`engine: (custom_provider)` のときのみ。PR #437 追記）/ `plain_scan_limit_omitted`（距離順 `ORDER BY` の `LIMIT` 省略・`LIMIT ALL`・`OFFSET` 単独。Issue #2242。優先順位は `DESC`・距離種別の不一致の後ろ）/ `plain_scan_descending`（距離順 `ORDER BY` に `DESC` が付いたとき。Issue #2009・SQL-33 (2)・SQL-75 (4) 追記）/ `plain_scan_opclass_mismatch`（索引の距離種別と距離演算子が合わないとき。Issue #1979・INDEX-7・SQL-33 追記） |
| `scalar_plan: <token>` | 常時（Issue #474 追記） | `plain_scan` / `index_equality` / `index_prefix` / `index_id_range` / `index_conjunction`（`sql::scalar_plan::classify_scalar_plan` の静的判定。部分索引を選択した場合は `index_partial index=<名前>`〔Issue #1656。`docs/design/partial-index.md`〕。件数・閾値・実行時縮退結果は非露出。詳細は `docs/design/scalar-index-prune.md` 参照） |

`engine` の文字列化は `SearchEngineKind` の既存 `Display`（`full_scan_ratio` を
含む診断・ログ向け表現）をそのまま使わず、`sql/explain.rs` に専用の網羅 `match`
を置く。`(custom_provider)` はヒント未指定の `(none)` と意味が異なるため区別する。

**露出しない値**: `full_scan_ratio`（切替閾値）・`MIN_INDEXED_ROWS`・可視カーディ
ナリティ・行数・索引ノード数・キャッシュ状態・実行時縮退結果・hybrid 密側再取得
ラウンド数・実行時に選ばれた visited 実装（`VisitedBitmap`／`VisitedSparse`。
Issue #497）・`acorn_max_visible_ratio`（ACORN-1 の 2-hop 展開切替閾値。Issue #501。
`full_scan_ratio` と同じ「切替閾値」区分のため据え置き。実行時のレジーム選択
（`PlainScan`／`OneHop`／`TwoHop`）・`acorn_searches`／`acorn_expansions` も同様に
非露出）・`acorn_max_expansion_ratio`（TwoHop 展開過多時の plain scan 縮退ガード
の上限比。Issue #681。`full_scan_ratio`／`acorn_max_visible_ratio` と同じ
「切替閾値」区分のため据え置き）・`acorn_guard_fallbacks`（同ガードの実行時縮退
結果。Issue #681。他の実行時縮退結果と同じ理由で非露出）。これらはいずれも
テナントの存在情報に繋がりうるため対象外とし、必要に
なれば別 Issue でオーナー判断とする。`sparse_visited_max` 自体は `resident=`
（Issue #514）と同じ「構築時の静的 opt-in 設定値」区分のため例外的に露出する
（`full_scan_ratio` の露出可否は本 Issue で再開しない。据え置き）。

## 実装

- `crates/engine/src/sql/hnsw_cache.rs`: `AnnPlan`／`AnnShapeInput`／
  `classify_ann_plan`（+ 32 通り真理表テスト）
- `crates/engine/src/sql/exec.rs`: 4 boolean を `classify_ann_plan` からの導出へ
  置換（ディスパッチ構造・適用条件そのものは無変更）
- `crates/engine/src/sql/using_plan.rs`: `pre_check_bindable` の戻り値を
  `PreCheckShape { filters_empty }` へ拡張
- `crates/engine/src/sql/explain.rs`: `ExplainEngine { kind, ann_plan,
  scalar_plan }`（Issue #474 で `scalar_plan` フィールドを追加済み）・
  `build_explain_result(planned, engine)`（`engine_token`／`ann_plan_token` の
  網羅 `match`。新規行の閉じた語彙・非データ依存を機械的に固定するテストを含む）
- `crates/engine/src/core.rs`: `Statement::Explain` アームで `ExplainEngine` を
  組み立てる。`hnsw_enabled` は `self.hnsw_state.is_some()`（executor と同じ源泉）、
  `is_hybrid` は常に `true`（`USING PLAN` は常に `Ranking::Hybrid`）、`is_precision`
  は `planned.mode().mode()`、`filters_empty` は `pre_check_bindable` の戻り値、
  `scalar_prefilter` は `ExecutionPlan::from_evaluation_order(validated.
  evaluation_order())`。`hnsw_state` の `lookup`／`prepare_*` は一切呼ばない
- `crates/engine/tests/sql_explain.rs`・`crates/wire-server/tests/wire_explain.rs`:
  既定エンジン・HNSW opt-in（フィルタなし／等価フィルタ／`visible()` のみ／
  `precision` モード／`HINT ORDER` 拒否）・閉じた語彙固定・
  `hnsw_index_cache_stats()` 全 0 固定（副作用なし契約の維持）を検証

## スコープ外

- `EXPLAIN` の受理形状拡張（`USING PLAN` を伴わない `ORDER BY ... <->` DISTANCE
  クエリへの拡張）は SQL-6 の定義範囲に関わるため spec 側の課題として申し送り
- 実行時縮退結果・hybrid 密側再取得ラウンド数・キャッシュ状態の可視化
  （`EXPLAIN ANALYZE` 相当）は実行を伴うため別設計
- `full_scan_ratio` 等の閾値の **`EXPLAIN` への露出可否**はオーナー判断待ちの
  まま（据え置き）。一方 **CLI での設定露出**（`--hnsw-full-scan-ratio`／
  `--hnsw-acorn-max-visible-ratio`）は Issue #657 で実装済み——`EXPLAIN` の
  `hnsw_params:` 行への非露出方針自体は変わらない（`docs/design/
  hnsw-search-engine-wiring.md`「CLI 探索パラメータ opt-in（Issue #657）」節
  参照）
- wire-server CLI でのエンジン選択露出は Issue #656 で実装済み
  （`--search-engine`。`docs/design/hnsw-search-engine-wiring.md` 参照）。
  テーブル単位カタログ属性でのエンジン選択露出は引き続き対象外（ADR
  「判断確定後のスコープ外」節）
- `SearchEngineError` の `ErrorClass`／`wire_code` 正式登録（spec 側ビヘイビア ID
  確定後）

## 追記（PR #437・codex-review P1 指摘対応）

`EngineCore::with_provider`／`from_storage` 経由でカスタム `SearchProvider` を
注入した場合（`search_engine_kind() == None`）、`EngineCore` は実際に ANN か
brute-force かを判別できない。従来はこの場合も `hnsw_enabled == false` を
`classify_ann_plan` へそのまま渡していたため `ann_plan: plain_scan_engine`
（厳密 brute-force と確定）を返してしまい、`engine: (custom_provider)`
（実行方式不明）と矛盾する誤表示だった。`AnnShapeInput` へ `engine_kind_unknown`
フィールドを追加し、`search_engine_kind().is_none()` の場合は新設した
`AnnPlan::UnknownCustomProvider`（`ann_plan: unknown_custom_provider`）を返す
よう分岐した。`sql::exec` 側の 4 boolean 導出（`hnsw_full_visible_eligible` 等）
は `engine_kind_unknown` を参照しない（常に `false` を渡す）ため実行時の適用
条件は無変更。

## 追記（Issue #730・NOSQL-10 の前提）

`sql::exec::execute_insert` と並び、本 doc が扱う `sql::explain::
build_explain_result` を `BoundScan`（Issue #726）・`BoundAggregate`
（Issue #727）と同じ「同作法」で公開 API へ昇格した（`bind_scan`／
`bind_aggregate` と異なり `build_explain_result` はもともと副作用のない純粋
整形関数だったため、フィールドの非公開維持・アクセサー追加という形はそのまま
踏襲しつつ、入力である `ExplainEngine`・`AnnPlan`・`ScalarPlan`・
`classify_ann_plan`・`classify_scalar_plan` も併せて公開しないと呼べない点が
`bind_scan`／`bind_aggregate` との違い）:

- `sql.rs`: `mod explain` を `pub(crate)` → `pub` へ。`hnsw_cache::{AnnPlan,
  AnnShapeInput, classify_ann_plan}`・`scalar_plan::{ScalarPlan,
  ScalarShapeInput, classify_scalar_plan}` を `pub use` で再エクスポート
  （`hnsw_cache`／`scalar_plan` モジュール自体は内部実装として `pub(crate)`
  のまま維持）
- `sql/explain.rs`: `ExplainEngine` を `#[non_exhaustive] pub struct` 化し
  `Self::new`（構築）・`kind()`／`ann_plan()`／`scalar_plan()`（読み取り）を
  追加。フィールド自体は `pub(crate)` のまま。`build_explain_result` を
  `pub fn` 化
- `sql/hnsw_cache.rs`: `AnnPlan` を `#[non_exhaustive] pub enum` 化。
  `AnnShapeInput` は全フィールド `bool`（テナント存在情報を持たない）のため
  `#[non_exhaustive]` は付けず `pub struct`（フィールドも `pub`）にして
  クレート外から構造体リテラルで組み立てられるようにした。
  `classify_ann_plan` を `pub fn` 化
- `sql/scalar_plan.rs`: `ScalarPlan` を `#[non_exhaustive] pub enum` 化。
  `ScalarShapeInput<'a>` は既に `pub` な型（`MetadataFilter`・`BoundExpr`）の
  スライスのみを持つため `pub struct`（フィールドも `pub`）。
  `classify_scalar_plan` を `pub fn` 化
- `crates/engine/tests/sql_insert_explain_public_api.rs`（新設）:
  `EngineCore::plan_query_with_mode` → `classify_ann_plan`／
  `classify_scalar_plan` → `ExplainEngine::new` → `build_explain_result` を
  engine クレート外から組み立てた結果が、SQL `EXPLAIN`（`execute_sql_in_
  session` の `Statement::Explain` アーム）が実際に返す行と**行単位で完全
  一致**することを固定（#765 の受け入れ条件の先取り）

`PlannedQuery::new`（`pub(crate)` のまま）は対象外——外部は
`EngineCore::plan_query_with_mode`（既存 `pub`）で `PlannedQuery` を取得する。
`using_plan::pre_check_bindable`／`PreCheckShape` も対象外のまま（`sql::exec`
の `EXPLAIN` アームは引き続き内部でこれを使う。外部は `AnnShapeInput`／
`ScalarShapeInput` を自前で組み立てればよく、単一情報源（分類関数）は共有
される）。

## 追記（Issue #922・SQL-27: `EXPLAIN` の対象拡大）

上記「スコープ外」節の「`EXPLAIN` の受理形状拡張」を実装した。`EXPLAIN` の
対象を「`USING PLAN` を伴う検索 SELECT」限定から、通常検索 SELECT（`USING
PLAN` の有無いずれも）・集計 SELECT（`GROUP BY`・`SELECT DISTINCT` の脱糖形
いずれも）・広域取得（`OFFSET` を含む）へ拡大した。`sql::allowlist::
Statement::Explain` のペイロードを `ValidatedStatement` から
`ExplainTarget`（`Search`／`Aggregate`／`Scan` の 3 variant）へ変更した
（**BREAKING CHANGE**）。

出力形式は対象文の種別ごとに 3 系統になった（いずれも実行本体〔行走査・
キャッシュ消費・LLM/Embedder 呼び出し・テーブル世代の更新〕を一切行わない
契約は共通）。

- **`USING PLAN` 付き検索**: 既存の 9 行（`search_terms[i]` 〜
  `scalar_plan`）を無変更で維持
- **`USING PLAN` なし検索**: `mode`／`mode_source`／`engine`／
  （`hnsw` のときのみ）`hnsw_params`／`ann_plan`／`scalar_plan` の末尾行
  のみ（LLM I/O を行わないため `search_terms[i]`／`path_hint`／`kind_hint`
  を持たない）。末尾行の書式は `push_engine_tail_rows`（`sql/explain.rs`）
  で両系統が共有し発散しない
- **集計・広域取得**: `scalar_plan`／`access_path` の 2 行のみ。`mode`・
  `engine`・`ann_plan` は出さない（広域取得は取得モードを参照せず、
  どちらの文にもランキング段が無いため）

`access_path` は新設の閉じた語彙（`visible_bitmap_cache`／
`scalar_index_candidates`／`scalar_index_group_enumeration`／`full_scan`）で、
executor の実行時ゲートと 1 対 1 対応する静的判定のみを表す（可視
カーディナリティ・索引の構築可否・キャッシュのヒット/ミス等の実行時縮退の
結果は非露出）。集計向けの分類は `sql::aggregate::classify_aggregate_access`
が `select_decode_tier`／`classify_scalar_plan`／`has_text_min_max_aggregate`
という executor と同じ判定式から導出する単一情報源（矛盾出力の防止）。
語彙数・出さない行の選定は SQL-27 確定時に spec 側と擦り合わせる事項。

## 追記（Issue #948・NOSQL-16）: NoSQL 表層からの露出は SQL と同一

`wire-server` の NoSQL 表層（`POST /v1/query`）の `vector` 指定
`search`・`scan`・`aggregate` への `explain: true`（Issue #948）は、
`engine::core::EngineCore::search_explain_from_bound`・
`scan_explain_result`・`aggregate_explain_from_bound`（`docs/design/
bound-plan-session-entry.md` 追記節参照）を SQL `EXPLAIN` アームと共有する
ため、本ドキュメントが定めた出力形式（`USING PLAN` なし検索の末尾行・
集計／広域取得の `scalar_plan`／`access_path` の 2 行）・非露出方針（実行時
縮退・カーディナリティを出さない）は NoSQL 表層から見ても不変である。
NoSQL 表層が独自の行・語彙を追加することはない。

## 追記（Issue #1066: 使用索引名の露出）

宣言済み索引（Issue #908・#1065・TASK-206・INDEX-7）が経路に載る場合、
`ann_plan:`／`scalar_plan:` 行の末尾へ条件付きサフィックス
`index=<name>[,<name>...]`（昇順ソート・重複排除・`,` 区切り。索引名は
`catalog::validate_identifier` を満たすため区切り文字と衝突しない）を追記
する。既存行の追加・順序変更はしない安定契約で、条件を満たさない場合は
既存出力と完全に同一（既定エンジン・宣言なし時はビット同一のまま）。

- **ann**: `ann_plan` が `hnsw_full_visible`／`hnsw_subset` かつ
  `--hnsw-scope declared`（`HnswScope::Declared`）のときだけ、対象テーブルの
  `USING hnsw` 宣言すべて。`HnswScope::All` では宣言の有無が
  `catalog::hnsw_targeted_in_txn` の適格性判定に一切影響しない（#1065 の
  不変条件）ため、宣言があっても索引名は付けない（codex-review P1 指摘対応・
  Issue #1066 PR #1155。「宣言の追加・削除だけで実行経路が変わらないのに
  使用索引名表示が変わる」という「使用索引を示す」契約との矛盾を避けるため、
  適格性そのものが宣言に依存する `Declared` scope に限定した）
- **scalar**: 宣言の有効化スイッチ（起動時 HNSW opt-in）が有効・
  `scalar_plan` が `plain_scan` 以外・`WHERE` の索引対応述語
  （`metadata_filters` の列と、式述語の数値列〔Issue #1413。`id` 述語は除く〕）が参照する列**すべて**がいずれかのスカラー宣言で
  被覆されている場合のみ、被覆に用いる宣言集合（複数可）。1 列でも
  非被覆なら索引名は付けない。`id` 述語（常設の暗黙索引で宣言索引ではない）
  は対象外。実行側 `catalog::declared_index_targets_in_txn` は宣言名を捨て
  対象テーブルの全スカラー宣言列を和集合した単一の索引を構築するため（宣言別
  の個別経路は無い）、述語列を複数の宣言が重複して覆う場合は貪欲法（残り
  未被覆列を最も多く覆う宣言から、同数なら名前の昇順）で選んだ被覆集合
  だけを表示し、個別使用経路の無い宣言まで表示しない（codex-review P1
  指摘対応・Issue #1066 PR #1155）。**この被覆集合は貪欲法による選択であり、
  被覆に必要な宣言数がグローバルに最小であることは保証しない**（2 巡目
  codex-review P1 指摘対応・Issue #1066 PR #1155。反例: 列 a..h・宣言
  idx_a=(a,b,c,d)／idx_b=(a,b,e,f)／idx_c=(c,d,g,h) に対し述語列が a..h
  全列のとき、貪欲法は idx_a・idx_b・idx_c の 3 件を選ぶが idx_b＋idx_c の
  2 件でも被覆できる）。真の最小集合被覆は NP-hard であり、被覆元の
  `WHERE` 述語列はクライアントが調整可能な非信頼入力のため、厳密解を
  指数時間で求める実装は計算量 DoS 経路になり得る（`coding-rust.md`
  「untrusted 入力の扱い」）。そのため契約は「述語列を被覆するために必要な
  最小の宣言集合」ではなく「述語列を被覆する宣言集合（貪欲法選択）」と
  定義する
- **付与条件は静的判定のみで、実行時の索引使用を保証しない**（2 巡目
  codex-review P1 指摘対応・Issue #1066 PR #1155）: 上記 ann／scalar いずれの
  `index=` も「宣言上の候補索引名」であり、実行側 `sql::scalar_index::
  ScalarIndex::resolve_candidates` が選択度超過（`CandidateResolution::
  FallbackSelectivity`）・候補取得不能（`FallbackNoIndex`）で全走査へ
  縮退した場合も `index=` はそのまま付く。この縮退は可視行数・カーディナ
  リティという行データ依存の実行時値によって決まるため、`EXPLAIN` が
  これを観測するには索引の構築・照会という「決定 1」節で明示的に禁止した
  実行副作用が必要になり、かつ縮退の有無自体がテナントのカーディナリティを
  示す副チャネルになる（上記「露出しない値」節の「実行時縮退結果」と同区分）。
  そのため `EXPLAIN` は意図的にこれを反映しない
- 集計 EXPLAIN では `access_path: scalar_index_candidates` のときだけ
  `scalar_plan:` に名前が付く（`scalar_index_group_enumeration`・
  `full_scan` には付けない）
- テナント存在情報（行数・可視カーディナリティ・実行時縮退結果）は
  従来どおり一切含めない。付与条件はクエリ形状・エンジン設定・カタログ
  宣言（DDL 権限者のみが変更可能な全テナント共有メタデータ）だけで決まり、
  行データ・`PolicyContext` に依存しないため、テナント間で出力は同一
  （NoSQL 表層〔`explain_bound_search_in_session`／
  `explain_bound_aggregate_in_session`〕も同じ判定を共有し行単位で一致）
- `scalar_plan:` トークン自体を宣言除外・平均値長等の実行時ゲートへ
  合わせる変更は対象外のまま（別 Issue の管轄。本 Issue はトークンを
  変えずに索引名の注記を付けるだけ）
- **索引名注記は束縛に使ったスキーマと同一スナップショットの索引宣言だけを
  見る**（3 巡目 codex-review P1 指摘対応・Issue #1066 PR #1155）:
  `run_search_explain`／`run_explain_plan`（`USING PLAN`）／
  `run_relational_explain_aggregate` はいずれも `read_txn_with_schema` で
  開いた読み取りトランザクションを保持し続け、`explain_engine_for`・
  `aggregate_explain_from_bound` の索引宣言読み取り
  （`catalog::explain_index_names_in_txn`）へその同じ参照を渡す（新規に
  `begin_read()` し直さない）。これにより束縛時スキーマの取得と索引宣言の
  読み取りが単一スナップショットに統一され、両者の間に `DROP INDEX`／
  `CREATE INDEX`／テーブル再作成がコミットされても、旧スキーマで束縛した
  述語に新しい宣言の `index=` が付いて実際の計画と食い違うことがない

## ポインタ

SQL-6・SQL-27・TASK-78・CORE-9・CORE-10・CORE-12・TASK-132・SEARCH-9・
PLAN-11・TASK-186（NOSQL-6・NOSQL-10・NOSQL-16）

## 追記（Issue #2191・TABLE-28）: 縮退理由の露出

`scalar_plan:` 行の末尾に、照合の不一致で全走査へ縮退したときだけ `reason=collation_mismatch` を追記する。`index=` 注記と同じ「既存トークンの末尾への追記のみ」の規則で、行の追加・順序変更はしない。語彙は固定で、値・件数・他テナントの存在情報を含まない。実行時の `ScalarIndex::candidates_for` と静的判定が同じ関数（`text_index_supports`）を通るため、D5 の矛盾出力は生じない。新しい接尾辞を spec 側の語彙（SQL-27 等）へ加えるかは、spec リポ側でオーナーが判断する事項。

## 追記（Issue #2198・TABLE-23）: PK 点検索の語彙

広域取得（`sql::scan`）で宣言 PRIMARY KEY の全構成列がリテラル等価のとき、`scalar_plan: index_primary_key`
と `access_path: primary_key_lookup` を出す（新しい語彙。`ScalarPlan::IndexPrimaryKey`・
`AccessPath::PrimaryKeyLookup`）。実行側と同じ静的分類（`sql::pk_point::classify_pk_point`）を呼ぶので 2 行は
矛盾しない。テナント単位のマーカー欠落による範囲走査への縮退は露出せず、索引名の注記も付けない。
語彙数の変更は SQL-27 の確定時に spec 側とすり合わせる事項。

## 追記（Issue #2208・INDEX-8）: GIN の語彙

広域取得（`sql::scan`）で jsonb 列への包含・キー存在・jsonpath 演算子が GIN 相当索引の候補探査になるとき、
`scalar_plan: index_gin index=<索引名>` と `access_path: gin_index_candidates` を出す（新しい語彙。`ScalarPlan::IndexGin`・
`AccessPath::GinIndexCandidates`）。索引名の注記は INDEX-7 と同じ `index=<name>` 接尾辞の規則。実行側と同じ静的分類
（`sql::gin_scan::classify_gin_scan`）を呼ぶので 2 行は矛盾しない。優先順位は PK 点検索 → GIN → 全走査。
EXPLAIN は索引の物理テーブルを開かず、テナント単位のマーカー欠落による行範囲走査への縮退・候補数・テナントの
有無は露出しない。語彙数の変更は SQL-27 の確定時に spec 側とすり合わせる事項。

## 追記（Issue #1896・SQL-64 (6)）: 窓の評価段の行

窓付き広域取得では `access_path:` 行の後ろへ `window[i]: <func> partition_keys=<n> order_keys=<m>` を窓項目ごとに
出現順で追記する（新しい語彙。関数名は `WindowFunc::default_alias` の閉じた語彙）。静的な構文情報だけを出し、
列名・フレーム・行数・パーティション数は露出しない。窓なしの文の出力は不変。語彙の追加は SQL-27 の確定時に
spec 側とすり合わせる事項（spec リポ側でオーナーが判断する）。

## `plain_scan_descending`（Issue #2009）

距離順 `ORDER BY` に `DESC`（遠い順）が付いた検索は、エンジン種別・HNSW の有効無効・モードに関係なく
ANN 索引を使わず、可視行を全走査して返す。`ann_plan:` 行は常に `plain_scan_descending` になり、
`index=` の索引名注記は付かない。実行側（`sql::exec`）と `EXPLAIN`（`core.rs::explain_engine_for`）は
同じ `sql::hnsw_cache::ann_plan_for_direction` を通して判断元を 1 つにしている。

厳密走査に倒す理由が増える場合（演算子クラスの不一致・`LIMIT` 省略）も、この関数へ理由を足す。
優先順位は「`DESC` → 種別の不一致 → `LIMIT` の省略 → 既存の分類（`classify_ann_plan`）」とする。
既存の昇順の文の `EXPLAIN` 出力は変えない。

## `plain_scan_opclass_mismatch`（Issue #1979）

索引の距離種別（opclass）と `ORDER BY` の距離演算子が合わない検索は、HNSW を使わず可視行を厳密に全走査する。
`ann_plan:` 行は `plain_scan_opclass_mismatch` になり、`index=` の索引名注記は付かない。語彙はデータに依存せず、
行数・テナントの情報を含まない。優先順位は上記のとおり `DESC` が不一致より先で、実装済み
（`sql::hnsw_cache::ann_plan_for_direction`。`LIMIT` 省略は不一致の後ろで `plain_scan_limit_omitted`〔Issue #2242〕）。縮退の規則は
[index-declaration-effects.md](index-declaration-effects.md) の追記（Issue #1979）を参照。新しい語彙は
SQL-27 の確定時に spec 側とすり合わせる事項（spec リポ側でオーナーが判断する）。

## 追記（Issue #2045・SQL-75 (2)）: 段の木の表現

結合・集合演算・CTE・部分クエリの `EXPLAIN`（親 #2044 配下の #2046〜#2048）が段を入れ子にできるよう、
`sql::explain_tree::ExplainNode`（engine 内部の `pub(crate)`）を追加した。公開 API（`lib.rs`）は変えない。

- **節のモデル**: 節は「見出し（任意）・属性行・子（出現順）」を持つ。単一表の 3 経路
  （`USING PLAN` 付き検索・なし検索・集計/広域取得〔窓の段を含む〕）は「見出しなし・子なしのルート 1 節」
  として組み立てる。この形の描画は属性行そのものなので、出力は従来とバイト単位で同一（後方互換）。
  分割実行 DML の `partitioned::explain_result` は深さ 0 の木と出力が同じため載せ替えていない。
- **字下げの規則（安定契約）**: 深さ 0 は前置なし。深さ d >= 1 の見出し行は `"  " x (d-1) + "-> " + 見出し`、
  属性行は `"  " x (d-1) + "   " + 行`。見出しなしの子は見出し行を出さず属性行のみ字下げする。
  兄弟 Issue は見出し付きで使う。
- **上限**: 深さ 64・節 1024・行 8192。深さは既存の構文段の上限（表参照・部分クエリ・CTE・集合演算の括弧・
  ビューの入れ子）の合計に付帯段の余裕を見込んだ値で、コンパイル時 assert で下回らないことを保証する。
  超過は出力を切らずに `ExplainTreeError`（`TooDeep`／`TooManyNodes`／`TooManyLines`）で fail-closed にする。
  見出し・属性行の改行は `InvalidLine` で拒否する。メッセージは固定の英語文で入力値を含めない。
  `wire_code` への写像は呼び出し元（#2046〜#2048）が決める（候補: 文が複雑すぎる旨の SQLSTATE 54001 相当、
  もしくは Internal）。描画は再帰を使わず明示スタックで行う。
- **露出しない値**: 節に載せる値は SQL-27 の契約のまま（閉じた語彙のトークンと検証済みの索引名のみ。行数・
  可視件数・カーディナリティ等は載せない。SQL-75 (5)）。`EXPLAIN` は本体を実行しない。
- **対象外**: 許可リスト（42601）の拡張と各段の語彙は #2046〜#2048、`EXPLAIN ANALYZE`／`FORMAT JSON` は SQL-76。
  NoSQL 表層は SQL-75 (6) のとおり変えない。

## 追記（Issue #2046・SQL-75 (2)）: JOIN の段

JOIN を含む `SELECT` の `EXPLAIN` を受理する（従来は単一表の文法で解析して `42601`）。文は実行せず、
上記の段の木で結合の方式・順・各入力を返す。構文段は非 EXPLAIN と同じ判定（`looks_like_join` →
`parse_join_statement`）で `ExplainTarget::Join` にし、束縛は `sql::join::prepare_join`（行を読まない）を共有する。
束縛エラー・`LIMIT`／`OFFSET` の範囲・relation 数と `GROUP BY` 列数の上限の `wire_code` と判定順は
非 EXPLAIN と一致する。

- **段の重ね方（外側から。該当しない段は出さない）**: 非集計形は `limit` → `sort` → `filter` → `join` の
  連鎖 → `scan`、集計形は `limit` → `sort` → `aggregate` → `filter` → `join` の連鎖 → `scan`。実行順
  （結合 → WHERE 残余 → 集計／並べ替え → OFFSET／LIMIT）の逆。`join` は left-deep で、段 k の子は
  [段 k-1 の `join`（k = 0 なら relation 1 の `scan`）, relation k+2 の `scan`] の順。
- **語彙（閉じている・安定契約）**:
  - `join`: `join_method: hash|nested_loop|cross`（実行器と同じ判定 `StepPlan::method` を共有）、
    `join_type: inner|left|right|full`（外部結合の簡約後の実効値）、`join_keys: <n>`、
    `join_filter: present|none`（ON 残余の有無）。
  - `scan`: `relation: <n>`（FROM での 1 始まりの序数）に続けて、単一表の広域取得と同一の
    `scalar_plan:`／`access_path:`（PK 点検索 → GIN → 全走査。`scan_explain_lines` を共有）。
  - `filter`: `residual_predicates: <n>`。`aggregate`: `group_keys`・`aggregates`・`having: present|none`。
    `sort`: `sort_keys: <n>`。`limit`: `limit: present|none`・`offset: present|none`。
- **出さない値と理由**: 表名・別名（スキーマ解決後の内部名や一時表の名前など、利用者が書いた綴りと異なる名前の
  露出と、区切り文字の混入経路を避けるため。出すかどうかは将来の判断として未決）、`LIMIT`／`OFFSET` の値
  （利用者リテラル）、行数・可視件数・カーディナリティ・予算・推定値（SQL-75 (5)）。
- **エラー写像**: 木の上限超過は `54001`（固定文）、契約違反は `Internal`。セッションの無い入口は
  従来どおり `42601`（SQL-75 (6)）。`$n` を含む `EXPLAIN` 前置の JOIN は非 EXPLAIN と同じく `42601`。
- **対象外**: 派生表・CTE（`EXPLAIN WITH`）・部分クエリを含む JOIN は従来どおり `42601`（#2048）、集合演算は #2047、
  DML は #2049〜#2051、`EXPLAIN ANALYZE`／オプションは SQL-76。NoSQL 表層は変えない。

## 追記（Issue #2047・SQL-75 (2)）: 集合演算の段

集合演算（`UNION [ALL]`／`INTERSECT`／`EXCEPT`）を含む `SELECT` の `EXPLAIN` を受理する（従来は単一表の文法で
解析して `UNION` の位置で `42601`）。文は実行せず、演算子・入れ子・各枝の走査分類を段の木で返す。構文段は
非 EXPLAIN と同じ判定（JOIN の後に `looks_like_set_operation`）と同じパーサー（`parse_set_operation`）で
`ExplainTarget::SetOperation` にする。先頭が括弧の形（`EXPLAIN (SELECT ...) UNION ...`）も受理する。
束縛は `sql::set_op::prepare`（全体 `LIMIT`／`OFFSET` の範囲 → 全枝の束縛と型整合。行を読まない）を実行と
共有するため、束縛エラーの `wire_code` と判定順は非 EXPLAIN と一致する。

- **段の重ね方（外側から。該当しない段は出さない）**: 全体 `limit`（`LIMIT`／`OFFSET` があるときだけ）→
  `set_operation`（入れ子は演算子の優先順位と括弧どおり。子は左、右の順）→ 葉。葉は、通常の枝が `scan`、
  括弧付きの枝が `limit` → `sort` → `scan`、集計の枝が `limit` → `sort` → `aggregate` → `scan`。
- **語彙（JOIN と同じ閉じた集合・安定契約）**: `set_operation` の属性は `set_operator: union|union_all|intersect|except`。
  `scan` は `relation: <n>`（葉の左からの 1 始まりの序数）に続けて `scalar_plan:`／`access_path:`
  （`scan_explain_lines` を共有。枝ごとに PK 点検索 → GIN → 全走査）。ほかは JOIN の段と同じ意味。
- **集計の枝の走査分類**: 実行器は可視ビットマップのキャッシュ・arena・スカラー索引を渡さない（常に全走査）ため、
  `plain_scan`／`full_scan` に固定する。実行器の隣（`set_op::aggregate_branch_explain_lines`）に置き、
  実行器へキャッシュや索引を配線するときは一緒に変える（矛盾した出力を出さない）。
- **出さない値と理由**: 表名・別名、`LIMIT`／`OFFSET` の値、行数・可視件数・カーディナリティ・予算（SQL-75 (5)）。
- **エラー写像**: 木の上限超過は `54001`（固定文）、契約違反は `Internal`。セッションの無い入口は従来どおり
  `42601`（SQL-75 (6)）。`$n` を含む `EXPLAIN` 前置の集合演算は `42601`（後続での受理に譲る）。
- **対象外**: 派生表・CTE・部分クエリは従来どおり `42601`（#2048）、DML は #2049〜#2051、`EXPLAIN ANALYZE`／
  オプションは SQL-76。NoSQL 表層は変えない。

## 追記（Issue #2048・SQL-75 (2)）: CTE・部分クエリの段

非再帰 CTE（`WITH`）・FROM 直後の派生表・WHERE の部分クエリ（`IN`／`NOT IN`／`EXISTS`／`NOT EXISTS`／スカラー比較／
`ANY`・`ALL`）を含む `SELECT` の `EXPLAIN` を受理する（従来は `EXPLAIN` 直後が `SELECT` でない文、または
部分クエリの構文で `42601`）。文は実行せず、内側も実行しない。

- **CTE・派生表（`ExplainTarget::Cte`）**: 構文段は非 EXPLAIN と同じ検証本体（`validate_with_statement`、派生表は
  `derived_table::gate`）を通し、インライン展開で単一表の広域取得・集計に畳み込まれた形だけを受理する
  （第 2 の WITH パーサーを持たない）。派生表は合成 CTE として同じ写像にする。判定は検証結果の形で行い、
  `Scan`／`Aggregate` なら inline。`cte`（`cte_source: with|derived_table`・`cte_mode: inline`）の下に、外側から
  `limit`（`LIMIT`／`OFFSET` があるときだけ）→ `sort`（並べ替えキーがあるときだけ）→ `aggregate`（集計のときだけ）→
  `scan` を並べる。CTE の定義ごとの段は作らない（連鎖・参照されない定義も展開後は 1 段）。`scan` の行は単一表の
  `EXPLAIN` と共有の分類関数が作る。
- **WHERE の部分クエリ**: 外側（単一表の広域取得・集計）の出力は見出しなしの葉のままで、その子に `subquery` 段
  （`subquery_kind: in|not_in|exists|not_exists|scalar|any|all`・`correlated: false`）を出現順に並べる。
  `subquery` の下に内側の `scan`（集計形のスカラーは `aggregate` → `scan`）を置き、内側 WHERE の入れ子の部分クエリは
  内側 `scan` の子に再帰的に並べる。部分クエリが無い文の出力はバイト単位で不変。
- **内側の静的検証**: 失敗時の静的検証（`static_validate_subquery_predicates`）と同じ経路を収集モードで通し、
  検証ロジックを二重化しない。内側が先・外側の束縛が後の判定順は実行経路と同じなので、内側・外側・CTE 本文の
  束縛エラーの `wire_code` は非 EXPLAIN と一致する。行数に依存する検査（`21000`・結果行数上限・`IN` 値予算）は
  対象外（実行しないため）。
- **外側の保守側の分類**: 実行時、部分クエリ述語は内側の結果から書き換わり、どの形になるかはデータ依存で静的には
  決まらない。そこで部分クエリを含む最上位の述語（`OR`・`NOT` の内側を含む場合は述語全体）を、定数偽の述語
  （`Or([])`。WHERE フィルタとして数えられ、PK 点検索・GIN・スカラー索引・可視ビットマップのいずれにも使えない）へ
  置き換えてから外側を束縛・分類する。外側の分類は「部分クエリ述語を索引に使えないフィルタとみなす保守側の静的分類」で、
  `id = 1 AND x IN (...)` は PK 点検索のまま、`id = 1 OR x IN (...)` は全走査になる（実際の経路を超えて索引を主張しない）。
- **内側の分類**: 内側の広域取得は外側と同じ `sql::scan::execute_scan` を通るため単一表の分類を共有する。内側の集計形は
  実行器 `execute_inner_aggregate_statement` がキャッシュ・索引を渡さないため `plain_scan`／`full_scan` に固定する
  （**実行器を変えるときは分類も一緒に変えること**）。
- **エラー写像**: 次はすべて `42601`（`Internal` に落とさない）。`WITH RECURSIVE`・具体化経路（`MATERIALIZED`・本文に
  `ORDER BY`／`LIMIT` を持つ CTE）・JOIN／集合演算の入力になる CTE・派生表・相関部分クエリ（収集中に到達したら拒否し、
  黙って受理しない）・投影位置／式に埋め込まれた値の部分クエリ・集計の `FILTER`／`HAVING` の部分クエリ・ランキング検索の
  部分クエリ。データ変更 CTE は既存の `0A000` で書き込みは発生しない。木の上限超過は `54001`、セッションの無い入口は
  従来どおり `42601`（SQL-75 (6)）。`$n` を含む `EXPLAIN` 前置の `WITH`・派生表・部分クエリは `42601`（後続での受理に譲る）。
- **出さない値と理由**: CTE 名・別名・表名、`LIMIT`／`OFFSET` の値、行数・可視件数・カーディナリティ・予算（SQL-75 (5)）。
- **対象外（後続候補）**: 具体化 CTE（`cte_mode: materialized`）、相関部分クエリ（`correlated: true`）、再帰 CTE、
  JOIN／集合演算の入力になる CTE・派生表、DML（#2049〜#2051）、`EXPLAIN ANALYZE`／オプション（SQL-76）。NoSQL 表層は変えない。

## 追記（Issue #2976・SQL-75 (2)）: 具体化 CTE・再帰 CTE・CTE／派生表を入力にする JOIN の段

Issue #2048 で `42601` にしていた形のうち、次の 3 つを段の木で返す。いずれも非 EXPLAIN と同じ検証本体（`validate_with_statement`・`derived_table::gate`）が返す文の形を写すだけで、第 2 のパーサー・束縛器は持たない。行・CTE 本文・再帰の反復・キャッシュ・台帳・世代・書き込みロックには触れない。

- **具体化 CTE**（明示 `MATERIALIZED`・本文に `ORDER BY`／`LIMIT`／`OFFSET`／集計を持つ CTE、同じ本文の FROM 直後の派生表）: 外側の層（`limit`／`sort`／`aggregate`／`filter`）を上、`cte`（`cte_mode: materialized`）を境界、本文の段（`limit`／`sort`／`aggregate`／`scan`）を下に並べる。外側の段が無ければ `cte` が根。検証順は Describe の評価後射影形アームと同じ（外側 `LIMIT` の範囲 → 照合 → 本文の結果列の導出 → 外側の束縛）。本文に `ORDER BY` を持つ CTE は序数指定でも `cte_mode: materialized` になる。検証本体がインラインへ戻す形（本文が具体化条件を満たさない CTE。例: 本文が `ORDER BY`／`LIMIT`／集計を持たず `MATERIALIZED` 指定もない）は `cte_mode: inline` のまま。外側／本文に窓関数がある形は固定文の `42601`。
- **再帰 CTE**: `cte`（`cte_mode: recursive`）→ `recursive_union`（`union: all|distinct`・`search: none|depth_first|breadth_first`・`cycle: present|none`）の下にアンカー項と再帰項を並べる。アンカーは既存の各 builder（`scan`／`aggregate`／`join`／`set_operation`）を再利用し、`FROM` なし定数式は新しい葉 `result`（`source: constant`）で表す。再帰項は単一関係なら `work_table_scan`（`filter: present|none`）、JOIN なら作業表の側を `work_table_scan`（`relation: k`）に置き換えた `join` 木。反復回数・累積行数はデータ依存なので出さない。計画は `recursive_eval::with_prepared` を Describe と共有する。アンカーが未対応の形（ビュー経由・順位付き検索等）は固定文の `42601`。
- **CTE／派生表を入力にする JOIN**: `join` 木の CTE を指す側だけを `cte`（`relation`・`cte_source: with|derived_table`・`cte_mode: materialized`・`cte_shared: true|false`）に置き換える。`derived_table` かどうかは合成名の判定で決め、名前自体は出さない。slot の本文は `scan` 系の段、`Derived` slot は層（`filter`／`sort`／`limit`）の下に親 slot の `cte` を入れ子にする。実表の側の出力は従来の JOIN `EXPLAIN` とバイト単位で同じ。CTE の側ではカタログ・索引宣言を読まない。
- **集合演算の入力になる CTE・派生表**: 非 EXPLAIN でも未対応（`42601`）のため、EXPLAIN も同じコードで拒否する（パリティ）。集合演算の実行器に CTE の枝を足す変更は別の機能追加として扱う。
- **出さない値**: CTE 名・別名・表名・合成名・リテラル・`LIMIT` 値・行数・反復回数（SQL-75 (5)）。出力はテナントの可視行に依存しない。木の上限超過は `54001`。
- **公開 API の注記**: `CteExplainMain` に variant（`Buffered`）を足した。`ValidatedCteExplain` のフィールドは crate 内専用でクレート外から値を取り出せないため、実質的な互換性影響は無い。
- **対象外（後続候補）**: 相関／`$n` 付きの WITH・派生表（#2977）、DML（#2049〜#2051）、`EXPLAIN ANALYZE`／オプション（SQL-76）、評価後射影形ビュー由来の形（#1192）。新しい段の語彙の SQL-75 への反映はオーナー判断（spec リポ側）。

## 追記（Issue #2977: WHERE の相関部分クエリと HALFVEC の順位付き検索）

親 #2044（SQL-75 (1)(2)(3)(5)(6) ポインタ）の継続。#2048 で拒否していた形のうち、実行が受理する範囲を
対応した。**エラー写像は本節が #2048 の記述を上書きする**。

- **相関部分クエリ**: WHERE の相関 `EXISTS`／`NOT EXISTS`／`IN`／`NOT IN`／スカラー（集計形・非集計形）は、
  実行経路と同じ準備（`classify_correlated_*`・`prepare_*`）と内側だけの連言の静的検証・束縛までを収集モードで通す。
  行・キャッシュ・台帳・世代・実行回数予算には触れない。相関の ANY/ALL、`ORDER BY` と `LIMIT`／`OFFSET` を伴う相関
  スカラー、直近より外のスコープへの相関などは、実行と同じ共有の準備処理が同じ `wire_code` で拒否する。
- **出力語彙（閉じた集合への追加）**: 相関のとき `subquery` 段は `correlated: true` に続けて
  `correlation_keys: <n>`（等価の相関キーの組の数）と `correlated_residual: present|none`（等価で表せない残余述語の
  有無。ハッシュ半結合か行ごとの再評価かの静的判定）を出す。非相関の出力は #2048 とバイト単位で不変。
  列名・値・件数以外は出さない（SQL-75 (5)）。
- **入れ子の親子**: 相関の静的検証は自分でフレームを開閉するので、相関の内側の入れ子の部分クエリは正しい親に付く。
  相関の集計形スカラーは実行の書き換えどおり `aggregate`（`group_keys` は相関キーの数）→ `scan`（固定の全走査）で示す。
- **順位付き検索**: `USING PLAN(...)` つきの WHERE の部分クエリは LLM I/O 前に `42601`（実行と同じ）。HALFVEC の kNN は
  実行が Scan へ写して部分クエリを解決するため、EXPLAIN も同じ経路で `subquery` 段を返す。VECTOR の kNN・HYBRID は
  実行の束縛段が `42601` で拒否するため EXPLAIN も同じ。
- **対象外（後続）**: 投影位置・式に埋め込んだ部分クエリ、集計の `FILTER`／`HAVING` の部分クエリ、`$n` を含む
  EXPLAIN 前置の WITH・派生表・部分クエリは引き続き `42601`（fail-closed）。CTE・派生表の主クエリの部分クエリは
  実行側が拒否している間は EXPLAIN も同じコードで拒否する。

## 追記（Issue #2051・SQL-75 (2)(3)(5)(6)）: INSERT の段

- **経路**: `EXPLAIN INSERT ...` は `ParsedSql::ExplainInsert`（`#[non_exhaustive]` な `ParsedSql` への variant
  追加で、クレート外に対して非破壊）。`Statement::Explain` には載せない。書き込みゲートが通常の `INSERT` と
  同じビット（`INSERT`、`ON CONFLICT DO UPDATE` の `UPDATE`、`RETURNING` の `SELECT`、`INSERT ... SELECT` の元表の
  `SELECT`）を課す必要があるため。解析は先頭の `EXPLAIN` を外した文を通常の INSERT と同じ解析に再帰で通す
  （第 2 のパーサーを持たない）ので、構文・名前解決・台帳必須構成（`23502`）の `wire_code` と判定順が一致する。
- **実行しない**: 実行腕は `execute_insert_form` 等と同じ束縛（行数上限・スキーマ・`RETURNING`・索引由来の衝突制約・
  `bind_insert_form`・バッチ上限、`INSERT ... SELECT` は `bind_insert_select`）までで止まる。行の書き込み・
  writer・台帳（`operation_id` を登録しない）・採番（`SequenceScope` を張らない）・トリガー・埋め込み I/O は起こさない。
  明示トランザクション内でも writer を取らず、`BEGIN READ ONLY` でも実行できる。
- **語彙**: 根は `insert`。属性は `insert_source`（`values`／`default_values`／`select`）・`insert_method`（`row`／
  `row_batch`／`file`／`upsert`。VALUES 系のみ）・`on_conflict`（`none`／`do_nothing`／`do_update`）・
  `conflict_target`（`row_id`／`unique`／`omitted`）・`conflict_filter`（`do_update` のみ）・`returning`・
  `operation_id`（有無は `present`／`none`）。子は VALUES 系が `result`（`source: values`）、`INSERT ... SELECT` が
  属性なしの `select` 段で、その下に内側の計画（`EXPLAIN SELECT` と同じ実行本体の出力）を置く。FROM なし SELECT は
  `result`（`source: constant`）。行数・件数・表名・列名・制約名・シーケンス名・リテラルは出さない。
- **暗黙 LIMIT**: `INSERT ... SELECT` で内側の `LIMIT` を省略したときの暗黙 `LIMIT`（Issue #2615）は、内側の
  再検証のために付けるので、内側の計画では `limit: present` と表示される（値は出ない）。
- **対象外（後続）**: 自動更新可能なビューへの INSERT（`0A000`）、一時表（既存の振り分けで `0A000`）、`$n` を含む
  `EXPLAIN INSERT`（`42601`）、内側が評価後射影形ビューの `INSERT ... SELECT`（`42601`）、データ変更 CTE、
  `EXPLAIN ANALYZE`／オプション。セッションの無い入口（`execute_sql`）は従来どおり `42601`。

## 追記（Issue #2050・SQL-75 (1)(2)(3)(5)(6)）: 述語形 UPDATE／DELETE の対象走査

親 #2049（SQL-75 のポインタ）の DML の EXPLAIN のうち、述語形の `UPDATE`／`DELETE`（`PARTITIONED` なし）を受理する。
INSERT は #2051 が担当する。`EXPLAIN ... PARTITIONED` は従来の出力のまま不変。

- **出力語彙**: 根は `delete`／`update`。`assignments: <SET の列数>`（UPDATE のみ）、`filter: present|none`
  （束縛後のフィルタの有無）、`returning: present|none`、`operation_id: present|none`（値は出さない）に続き、
  子として `scan` 段（`scalar_plan`・`access_path`。SELECT と同じ組み立て関数）を 1 つ置く。値・表名・件数は出さない。
- **走査分類を全走査に固定する根拠**: DML 実行器の候補列挙（`core/tenant.rs` の `enumerate_dml_candidates_with`）は
  PK 点検索・GIN・スカラー索引を使わず、自テナントの物理キー範囲を全走査して行ごとに述語を評価する。
  SELECT 用の分類を流用すると実経路と矛盾する出力になるため、`plain_scan`／`full_scan` に固定した。
  **実行器が索引を使うよう変わるときは分類も一緒に変えること**。
- **副作用ゼロ・エラー一致**: 構文段は非 EXPLAIN の DML と同じ入口で解析し、実行段は実行と同じ `prepare_predicate_*` で
  束縛する。スキーマの読み取りのみで、write txn・writer・台帳・行・シーケンス・トリガに触れない。READ ONLY
  トランザクションでも実行できる。未知の列・型不一致・不存在表は実行と同じ `wire_code`。
- **権限**: 実行した場合と同じ DELETE／UPDATE 権限を write gate で要求する（`42501`。SQL-40）。
- **対象外（`42601`、一時表は `0A000`）**: 採番でない表の単一行形（物理キー直接指定の語彙が未定義）、
  `UPDATE ... FROM`／`DELETE ... USING`、自動更新可能なビュー、WHERE の部分クエリ、一時表、セッション無しの入口。
  `$n` 付きの拡張クエリの EXPLAIN は既存の EXPLAIN の規則どおり。

## 追記（Issue #2058・SQL-76 (1)）: 括弧付きオプションの解析

`EXPLAIN ( <オプション> [, ...] ) <文>` の構文を解析する（実装は `crates/engine/src/sql/explain_options.rs`）。
オプションの実装そのもの（`ANALYZE` #2052・`FORMAT JSON` #2059・`VERBOSE` #2060）は各 Issue の担当。

- **位置**: 行ロック句（`row_lock`）と同じ型の抽出器を `core::EngineCore::parse_tokens_in` で 1 回だけ呼ぶ。
  構文段（`extract`）が括弧のリストを剥いで `EXPLAIN <文>` に組み直すので、下流の EXPLAIN 判定
  （SELECT・集合演算・CTE・UPDATE／DELETE）は従来どおりの本文を見る。`allowlist` の EXPLAIN アームは変えない。
- **判別**: `EXPLAIN` の直後が `(` で、その次が `SELECT`・`(`・`WITH` のいずれでもないときだけオプションのリスト。
  `EXPLAIN (SELECT ...) UNION ...` は従来の経路。リストの直後にもう一度リストが来る形は `42601`。
- **2 段の検証**: 構文段の誤り（空・末尾の `,`・閉じない括弧・名前や値の位置の不正・本文なし）は `42601`。
  内側の文の検証が通った後に意味段（`resolve`）が出現順に評価する（PostgreSQL と同じく内側の文のエラーが先）。
  未知の名前は `42601`、PG にあって本 DB が対応しない名前と `FORMAT XML／YAML` は `0A000`、値の不正と
  `ANALYZE` を伴わない `TIMING` は `22023`。同じオプションは後の指定が勝つ。`FORMAT` の値の省略は `42601`。
- **未実装のゲート（`0A000`・文は実行しない）**: `ANALYZE`（SELECT 以外。#2054 で SELECT は受理）・`ANALYZE` なしの `SUMMARY` の真、`VERBOSE` の真（#2060）、
  `FORMAT JSON`（#2059）。各 Issue がゲートを外すときに運搬用の field を `ParsedSql`／`SqlOutcome` へ足す。
  `COSTS`・`FORMAT TEXT`・明示的な偽は受理し、出力はオプションなしの `EXPLAIN` と同一（推定値を出さないため）。
- **`$n` との併用**: `sql::params` の EXPLAIN 系ガードは先頭の次が `(` だと素通りするため、オプション付きで
  `$n` を含む文は `42601` で拒否する（受理は後続に譲る）。
- **セッション無しの入口**: 抽出器を通らないため従来どおり `42601`（#2044 の契約を維持）。
- **メッセージ**: 固定の英語文字列で、入力のオプション名・値をエコーしない。
- **既知の差分**: 引用識別子のオプション名（`"analyze"`）は字句解析で引用の有無が残らず、引用なしと同じ扱いになる。
  前置形（`EXPLAIN ANALYZE <文>`）は #2054 で構文段に取り込んだ（次節）。

## 追記（Issue #2054・SQL-76 (2)(3)）: SELECT の EXPLAIN ANALYZE

- **決定 1（実行しない静的な計画）との関係**: オプションなしの `EXPLAIN` は従来どおり実行しない。`ANALYZE` を
  利用者が明示したときに限り、同じ SELECT を通常の SELECT と同じ実行器で実際に実行し、実測値を計画に併記する。
  結果の行はクライアントへ返さず捨てる（`QUERY PLAN` 単一列・`CommandComplete` は `EXPLAIN` のまま）。
- **構文**: 括弧形 `EXPLAIN (ANALYZE[, TIMING …, SUMMARY …, COSTS …, FORMAT TEXT]) <文>` と前置形
  `EXPLAIN ANALYZE|ANALYSE [VERBOSE] <文>`・`EXPLAIN VERBOSE <文>`。前置形は構文段（`sql::explain_options::extract`）で
  括弧形と同じ生のオプション列へ正規化する。前置の直後に括弧のリストが続く形は `42601`。`TIMING`・`SUMMARY` の
  省略は真（`ANALYZE` 時）。`TIMING` の真は `ANALYZE` を要する（`22023`）。`ANALYZE` なしの `SUMMARY` の真は `0A000`。
- **対象**: 本 DB が `EXPLAIN` できる SELECT（単一表の検索・集計・広域取得、JOIN、集合演算、CTE〔再帰・具体化を含む〕、
  派生表、WHERE の部分クエリ）。DML（`UPDATE`／`DELETE`／`INSERT`・分割実行）は `0A000`（#2055）。`USING PLAN` を伴う
  検索（LLM・embedder の I/O を伴う）、一時表を参照する文、`VERBOSE`・`FORMAT JSON` との併用、`$n` を含む文
  （拡張クエリの Parse は `42601`）は fail-closed で拒否する。セッション無しの入口は従来どおり `42601`。
- **出力の語彙（安定契約）**: 根の段の属性行の末尾（最初の子の段の直前）に `actual_rows: <n>`（RLS 適用後の可視行・
  LIMIT／OFFSET 適用後）、`actual_loops: 1`、`TIMING` が真なら `actual_total_time_ms: <x.xxx>`。計画の最後に `SUMMARY`
  が真なら `planning_time_ms: <x.xxx>`（静的な計画の組み立て）、`execution_time_ms: <x.xxx>`（内側の文の実行）。
  `TIMING OFF, SUMMARY OFF` の出力は、オプションなしの `EXPLAIN` の行へ 2 行を挿入した形と一致する。
  挿入位置は `explain_tree` の字下げ規則（深さ 0 は前置なし、深さ 1 以上は `"-> "` か空白で始まる）に依存し、
  描画後の行への後処理で行う。行数が既存の上限に定数行を足した範囲を超えたら `54001`。
- **出さないもの（SQL-76 (3)）**: フィルタで除いた行数・走査した行数・推定値・他テナントを含む件数。他テナントの行量で
  出力が変わらないことを結合テストで固定する。時間は壁時計の値で件数ではなく、通常の SELECT の応答時間を測るのと
  同程度の情報しか持たない（`TIMING OFF` で抑止できる）。
- **実行の読み取り源**: autocommit は計画と実行がそれぞれ読み取りトランザクションを開く（計画はカタログだけを読むため
  行数の一致には影響しない。既知事項）。明示トランザクションは通常の SELECT と同じ読み取り源で、未 commit の自分の
  変更が見え、`READ ONLY` でも可。SELECT 権限は実行前に read gate が要求する（`42501`）。実行時のエラーは通常の
  SELECT と同じ `wire_code` で返し、計画は返さない。
- **対象外**: 段ごとの実測（内側の段の行数・ループ・時間）と実行器の計測フック（#2053）、DML の ANALYZE（#2055）、
  HNSW の反復走査の回数・上限到達・再取得回数（#2056）、`FORMAT JSON`（#2059）、`VERBOSE`（#2060）。NoSQL 表層は変えない。
