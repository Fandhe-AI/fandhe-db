# 索引宣言の構築対象への反映（Issue #1065・TASK-206・INDEX-7・CORE-9・CORE-10）

## ステータス

Accepted・実装済み。

## ポインタ

- spec: `docs/spec/05-tasks.md` TASK-206・`docs/spec/04-behavior/indexing.md` INDEX-7・
  `docs/spec/04-behavior/search.md` CORE-9・CORE-10
- 前提: [index-ddl-declaration.md](index-ddl-declaration.md)（`CREATE INDEX`／
  `DROP INDEX` 構文・カタログ永続化。Issue #908）
- 関連: `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4

spec 本文はここへ転記しない（`.claude/rules/spec-confidentiality.md`）。以下は本リポの
実装既定値・設計判断の記録。

## 背景・目的

[index-ddl-declaration.md](index-ddl-declaration.md)（Issue #908）で `CREATE INDEX`／
`DROP INDEX` を追加したが、宣言は `redb` の `index_catalog` に永続化され対象テーブルの
世代を進めるだけで、`sql::scalar_index::ScalarIndex`（スカラー二次索引）と
`sql::hnsw_cache::HnswIndexCache`（HNSW 索引）の構築対象には効いていなかった。
本 Issue で宣言を構築対象の選択へ結線し、起動時 opt-in（`--search-engine
hnsw|hnsw_f16|hnsw_i8` → `SearchEngineKind::Hnsw`）との優先順位を確定する。

Issue コメント（オーナー判断 2026-09-27）の要旨: 起動時 opt-in が索引機構の上位
スイッチ／宣言は opt-in 有効時に構築対象の選択に使う／opt-in なしでは宣言の有無で
挙動を変えない。

Issue コメント（オーナー判断 2026-09-28）の要旨: HNSW は「CLI 既定＋宣言でテーブル
単位に有効化」とする。新 CLI フラグ `--hnsw-scope all|declared`（既定 `all`）で、
`all` は opt-in 時に全テーブル HNSW（宣言は記録のみ）、`declared` は `USING hnsw`
を宣言したテーブルだけ HNSW。判定はテーブル単位で他テーブルの宣言に影響されない。
新しい SQL 構文・カタログ形式は追加しない。

## 優先順位（上位スイッチ）

| 起動構成 | 宣言あり | 宣言なし |
| --- | --- | --- |
| opt-in なし（既定エンジン・`with_provider`／`from_storage`） | 宣言は無視（現行挙動のまま） | 現行挙動 |
| opt-in あり（`SearchEngineKind::Hnsw`） | スカラー宣言が構築対象を**絞り込む**（HNSW 宣言の効果は `--hnsw-scope` に従う。次々節） | 現行の自動挙動を維持（`--hnsw-scope declared` の HNSW のみ厳密） |

上位スイッチは `EngineCore::hnsw_state.is_some()`（＝ `SearchEngineKind::Hnsw` で
構築された場合のみ真）とする。今日の起動時索引 opt-in はこれだけのため、スカラー
宣言の有効化もこのスイッチに従う（オーナー判断の「opt-in なしでは宣言の有無で
挙動を変えない」を文字どおり満たす）。将来スカラー専用 opt-in を追加する場合は
本 ADR を改訂する。

**「宣言なし → 現行の自動挙動を維持」は本実装の安全側の既定値**（オーナー確定事項
ではなく、オーナーが将来覆せる実装判断）である。既存の opt-in 利用者はすべて
`CREATE INDEX` を使わずに opt-in している（`tests/hnsw_cache.rs`・
`tests/fixtures/recall_engine.rs`〔bench.yml／recall.yml のゲート入力〕・
`tests/hnsw_provider.rs` 等）。「宣言必須」にすると全経路が黙って brute-force へ
縮退し、recall ゲートが vacuous に通ってしまう（brute-force は recall 1.0）。
これは破壊的変更に当たり Issue #1065 のタイトル（`feat`、非破壊）とも矛盾する。
「宣言必須」への引き締めは将来の破壊的変更として別途判断する。

## スカラー索引（テーブル単位）

- opt-in 有効かつ対象テーブルに `IndexKind::Scalar` の宣言が 1 件以上ある場合、
  `ScalarIndex` は宣言列の和集合だけを索引対象にする（`sql::scalar_index::
  ScalarIndexTarget::Declared`）。宣言されていない列は列型に関わらず「列が索引
  未対応」（`per_column`／`per_column_typed` とも `None`）へ合流し、呼び出し元
  （`sql::exec`・`sql::aggregate`）は無変更のまま plain scan へ縮退する
  （`plain_scan_fallbacks` 統計に計上）。
- 宣言は「絞り込み」であり「強制」ではない: 平均値長ゲート
  （`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`）・バイト予算
  （`check_scalar_index_budget`）・`2^53` ゲートは宣言列にもそのまま適用する
  （DoS 対策・fail-soft 契約を緩めない）。対象外の列はバイト予算の計上自体を
  行わない（構築コストを一切払わない）。
- `id` の順序索引（`id_index`）は宣言の有無によらず従来どおり構築する（安価で、
  `id` を宣言対象に含めるかで挙動を分けると複雑化するだけのため）。
- 宣言なしのテーブルは現行の自動挙動（全対応列。`ScalarIndexTarget::Auto`）。

## HNSW（テーブル単位）

HNSW の適格性ゲート（`catalog::hnsw_targeted_in_txn`）は**テーブル単位**とし、
クエリ対象テーブルの経路は他テーブルの宣言に一切影響されない。HNSW を使う
テーブルの範囲は `engine::search_engine::HnswScope`（`EngineCore::with_hnsw_scope`
で設定。`wire-server` の起動時 CLI フラグ `--hnsw-scope` から到達する）で選ぶ:

| `--hnsw-scope`（`HnswScope`） | 宣言あり | 宣言なし |
| --- | --- | --- |
| `all`（既定・`All`） | HNSW | HNSW（宣言導入前と同一） |
| `declared`（`Declared`） | HNSW | 厳密（brute-force） |

- opt-in なし（既定エンジン）では scope によらず常に brute-force（scope は
  参照されない。CLI でも組合せエラーにしない）。
- `all`: テーブルは VECTOR 列を高々 1 本しか持てない（`catalog.rs` のスキーマ
  検証）ため、HNSW 宣言は経路選択を変えずカタログへ記録されるだけになる。
  既定値であり、宣言を使わない既存の opt-in 利用者（後述「影響を受ける既存
  fixture」）の挙動はビット同一のまま。
- `declared`: `CREATE INDEX ... USING hnsw` を宣言したテーブルだけ HNSW（近似）
  を使い、未宣言テーブルは厳密。`DROP INDEX` で厳密へ戻る。宣言したテーブル
  自身の探索方式だけが近似／厳密の間で切り替わり、他テーブルの経路・結果は
  変わらない。
- 新しい SQL 構文・カタログ形式は追加しない（既存の `CREATE INDEX ... USING
  hnsw`／`DROP INDEX` と `index_catalog` をそのまま使う。
  [index-ddl-declaration.md](index-ddl-declaration.md)）。
- 判定の入力は「対象テーブルに HNSW 宣言があるか」だけで、索引カタログ全件
  走査の要約（HNSW 宣言テーブル集合）を `IndexCatalogGateCache` が索引カタログ
  専用世代単位に再利用する（Issue #1154。索引カタログを変える commit だけで
  進む専用カウンタで、通常の行 DML では無効化されない。旧: ストレージ全体の
  単一世代カウンタをキーにしており、行 DML の commit でも過剰に無効化されて
  いた）。`all` では集合の中身を使わず、走査の成否（カタログを読み取れるか）
  だけを使う。読み取れない場合はいずれの scope でも fail-closed に brute-force
  へ倒す（次節）。
- 旧実装（PR #1124 のレビュー前）は「カタログ全体に `IndexKind::Hnsw` 宣言が 1 件でも
  あれば、宣言のあるテーブルだけを HNSW にする」カタログ全体単位のゲートだった。
  これは `table_a` への宣言で未宣言の `table_b` を近似から厳密へ切り替え、
  `table_b` の Top-k を変え得るため廃止した。
- `sql::exec`（`AnnShapeInput.hnsw_enabled`。scope は `sql::hnsw_cache::
  HnswCacheAccess::hnsw_scope` で受け取る）・`core.rs`（Rust API
  `search_with_snapshot`・`EXPLAIN` の `ann_plan:` 行）はいずれも同じ scope で
  `catalog::hnsw_targeted_in_txn` を呼ぶことで、表層間・実行時判定と `EXPLAIN`
  表示の乖離を作らない。
- Rust API（`search_with_snapshot`）は判定用 read txn を閉じてから検索本体を
  呼ぶため、判定時に読んだストレージ世代を検索本体の直前に再照合し、その間に
  何らかのコミットがあれば brute-force へ倒す（TOCTOU 再照合。fail-closed）。
- `declared` で `DROP INDEX` した宣言テーブルは、宣言前と同一の厳密結果へ戻る
  （`index_declaration_targets.rs` で固定）。

### Issue #1065 受け入れ条件との対応

- 条件 1「宣言の有無で索引構築対象が変わる」: スカラー宣言（opt-in 時の列の
  絞り込み）と、`declared` での HNSW 宣言（宣言テーブルのみ HNSW 索引を構築・
  使用）で満たす。`all` では HNSW 宣言は記録のみ。
- 条件 4「クエリ結果・RLS 境界が宣言の有無で変わらない」: `all`（既定）では
  HNSW・スカラーとも満たす。`declared` は宣言したテーブル自身の探索方式（近似／
  厳密）を宣言で切り替えることを明示的に選ぶモードであり、宣言テーブル自身の
  Top-k は変わり得るが、他テーブルの経路・結果は変わらない。RLS 境界はいずれの
  scope でも不変（次々節）。

## 宣言の読み取りと fail-closed

- 宣言はクエリの `read_txn`（アリーナ・世代と同一スナップショット）から読む
  （スカラー: `catalog::declared_index_targets_in_txn`／HNSW ゲート:
  `catalog::hnsw_targeted_in_txn` のカタログ全件走査）。`Storage::list_indexes`
  （独自に read txn を開く）はクエリ経路から呼ばない。
- カタログ値のデコード失敗・走査上限（`MAX_INDEX_COUNT`）超過は「宣言なし→
  自動」へは倒さず、**索引を使わない側**（スカラー: 索引構築失敗扱い →
  plain scan／HNSW: brute-force）へ倒す。いずれも厳密（brute-force）結果に
  なるため誤った結果を返すことはなく、fail-soft かつ安全側（HNSW 側は通常の
  近似経路と Top-k が異なり得るが、宣言の有無ではなくカタログ破損という異常時に
  限った縮退であり、`IndexCatalogGateCache` の `gate_read_failures` 統計で
  観測できる）。
- `EXPLAIN`（`core.rs::explain_engine_for`）はこの判定専用に新規の read txn を
  開く（検索本体を実行しない契約を保つため、計画開始時の txn を引き回さない）。
  読み取り失敗は `false`（brute-force 表示）へ倒す。

## テナント境界・RLS への影響

- 索引の物理表現は引き続き `(table, PolicyContext)` 可視スナップショットからのみ
  構築する。宣言は構築**対象列の絞り込み**と、`--hnsw-scope declared` での
  宣言テーブル自身の ANN 経路の適格性にのみ作用し、
  RLS 暗黙適用・可視性判定には一切触れない。統計・`EXPLAIN` にテナント ID・
  行 ID・他テナントの存在情報を追加しない。
- RLS 境界（テナント間の可視性・非漏えい）は宣言の有無で変わらない。
  スカラー索引の宣言（列の絞り込み）も、索引経路と plain scan のどちらを
  通っても返す結果集合は同一であり厳密検索のまま変わらない。
- HNSW はテーブル単位のゲート（前述「HNSW（テーブル単位）」）により、あるテーブル
  への宣言が他テーブルの探索方式（近似／厳密）・検索結果集合（Top-k の順序・
  メンバーシップ）を変えることはない。`all`（既定）では宣言したテーブル自身の
  結果も変わらない。`declared` では宣言したテーブル自身の探索方式だけが近似／
  厳密の間で切り替わり、その Top-k は宣言の有無で変わり得る（可視性・テナント間
  非漏えいは不変で、影響は近似精度の面に限る）。
  `crates/engine/tests/index_declaration_targets.rs` の
  `scope_all_declaration_on_one_table_does_not_change_any_table_results_or_path`・
  `scope_declared_uses_hnsw_only_on_declared_table_and_drop_returns_to_exact` は
  `table_a` への HNSW 宣言の追加・削除の前後で `table_b` の経路と Top-k（SQL 表層・
  Rust API・`EXPLAIN` の `ann_plan:`）が一致することを、
  `crates/engine/tests/sql_index_ddl.rs::index_declarations_do_not_change_query_results_or_rls`
  はテナント境界の非漏えいと結果不変をそれぞれ固定している。

## 対象外（申し送り）

- **`EXPLAIN` の `scalar_plan:` は宣言による対象外化を反映する（Issue #1153
  で対応済み）**: `scalar_plan:`（`sql::scalar_plan::classify_scalar_plan`）
  自体は引き続き `WHERE` 述語の**形**だけを見る束縛時の静的判定（カタログ非
  依存）のままだが、`search_explain_from_bound`・`aggregate_explain_from_
  bound`・`run_explain_plan`（`USING PLAN`）の 3 経路は、これに
  `sql::scalar_index::scalar_plan_under_target`（純粋関数）を通し、クエリと
  同一の `read_txn` から解決した索引宣言（`resolve_scalar_index_target_in_
  txn`）で「宣言により対象外にした列への述語」を `plain_scan`（集計の
  `access_path:` は `full_scan`／`scalar_index_group_enumeration` の判定）へ
  補正する。`ann_plan:` が `catalog::hnsw_targeted_in_txn` で実行時ゲートと
  揃えているのと同じ「実行時判定・`EXPLAIN` 表示の単一情報源化」パターン。
  `id` 述語（`id_index` は宣言の有無によらず常に構築される）は対象外にしない。
  固定は `crates/engine/tests/explain_scalar_plan_declarations.rs`・
  `crates/engine/tests/core_explain_plan_entry.rs`
  （`explain_entry_reflects_scalar_declaration_target_with_hnsw_opt_in`）参照
  （TASK-206・INDEX-7・SQL-27・NOSQL-16・NOSQL-10 ポインタ）。
  一方、以下は **データ依存**（EXPLAIN は索引構築・`lookup`・`prepare_*` の
  副作用を持たない契約のため、行データを見ないと分からない）ため、Issue
  #1153 の対象外のまま残る既知の限界:
  - 平均値長ゲート（Issue #632）・`2^53` ゲート（Issue #893）による列単位の
    実行時除外
  - 選択度による縮退（`CandidateResolution::FallbackSelectivity`）

  この 2 つはいずれも fail-closed に全走査（plain scan）へ倒れるだけで、
  RLS・可視性・結果の正しさには影響しない。

  `ScalarIndex::resolve_candidates` の早期打ち切り（交差候補が 0 件になった
  時点で、以降の述語を評価せず打ち切る）はこの 2 つとは性質が異なり、全走査
  への縮退ではない: 交差は述語を追加するほど結果が単調非増加になるため、
  空集合との交差は以降の述語によらず必ず空集合になり、
  `CandidateResolution::Use`（索引経路）のまま空の候補集合を返す。索引経路を
  使い続けるだけで結果・RLS には影響しない。ただし早期打ち切りは
  `metadata_filters` の列がすべて索引化されている（宣言による対象外化・
  平均値長ゲート・`2^53` ゲートのいずれでも除外されていない）ことを候補
  評価より前に静的検査した**後**にしか働かない
  （`ScalarIndex::filter_column_is_indexed`。codex-review P1 対応・PR
  #1158）: 旧実装はこの静的検査を欠き、先に評価した宣言列の交差が 0 件に
  なると早期打ち切りが働いて宣言外列の述語を一度も評価しないまま索引経路
  （`Use`）を返してしまい、`scalar_plan_under_target` の「宣言外列が 1 つ
  でもあれば `plain_scan`」という値に依存しない静的判定と、述語の順序・
  値によっては矛盾しうる状態だった（結果の正しさ自体には影響しない。
  空の候補集合を返す索引経路と全走査はいずれも「一致 0 件」で同じ結果に
  なるため）。静的検査を候補評価の前段に追加したことで、`EXPLAIN` と実行時
  の経路選択は述語の順序・値によらず常に一致する。固定は
  `crates/engine/tests/explain_scalar_plan_declarations.rs::
  search_explain_matches_runtime_when_declared_column_predicate_yields_
  empty_candidates`（宣言外列を含む複数述語を両順序で束縛し、`EXPLAIN` の
  `plain_scan` 表示と実行時統計〔`index_scans`／`plain_scan_fallbacks`〕が
  一致することを固定）参照。
- 疎索引（BM25）の宣言、NoSQL 表層の索引 DDL（[index-ddl-declaration.md]
  (index-ddl-declaration.md) の申し送りのまま）
- 宣言による強制索引化（既存のゲート・`MIN_INDEXED_ROWS` を無視する経路は作らない）
- スカラー専用 opt-in の新設（上位スイッチは起動時 HNSW opt-in のみ）
- **解消済み（Issue #1154）**: `IndexCatalogGateCache`（`catalog.rs`）は
  当初ストレージ全体の単一世代カウンタ（`crate::storage::current_generation_in_txn`）
  をキーにしていたため、索引宣言と無関係な行 DML の commit でも次回参照時に
  再走査が起きていた（codex-review P2 指摘・PR #1124）。Issue #1154 で、索引
  カタログを変更する経路（`create_index`／`drop_index`／`retain_index_defs_in_txn`。
  後者を `drop_table`・`alter_table_drop_column` が共有）だけで進む専用の
  「索引カタログ世代」カウンタ（`catalog::index_catalog_generation_in_txn`）を
  新設し、`hnsw_targeted_in_txn` のキャッシュキーをそちらへ切り替えた。これに
  より、索引宣言と無関係な行 DML の commit ではキャッシュが無効化されなくなった
  （行 DML を跨いでヒットし続けることを `crates/engine/tests/
  index_declaration_targets.rs::hnsw_scope_declared_gate_cache_ignores_row_dml_but_tracks_index_ddl`
  で固定）。**当初の申し送りとの相違点** 2 点: (1) スカラー宣言解決
  （`declared_index_targets_in_txn`）は申し送りの想定に反しキャッシュ化しな
  かった（`ScalarIndex` 構築時にしか呼ばれず、その構築自体が行 DML＝テーブル
  世代で必ずやり直しになるため利得がほとんど無い）。(2) Rust API
  `search_with_snapshot` の TOCTOU 再照合（判定〜検索本体呼び出し間の失効
  検出）はストレージ全体世代のまま据え置いた。これは「索引カタログの変更に
  限らず、対象テーブルへの任意の commit が判定直後に挟まった場合」を検出する
  ための安全弁であり、専用世代へ緩めると対象テーブルの行 DML による失効検出を
  落としてしまうため、性能改善の対象外とした（`core.rs::search_with_snapshot`
  のコメント参照。緩める場合は別途の性能論点）。永続フォーマットとの互換性は
  新カウンタ未保持の既存 DB でテーブル未作成＝世代 0 として扱うことで確保し、
  マイグレーションは不要である。

## 影響を受ける既存 fixture

宣言を一切使わない既存の opt-in 利用者（`tests/hnsw_cache.rs`・
`tests/hnsw_provider.rs`・`tests/hnsw_acorn_recall.rs`・
`tests/hnsw_hybrid_refetch.rs`・`tests/fixtures/recall_engine.rs`・
`crates/wire-server/tests/wire_tls_cli.rs` 等）は、宣言が 0 件のため
`ScalarIndexTarget::Auto`／HNSW 全テーブル対象のまま無変更で通ることを
既存テストの回帰実行で確認済み（宣言なし opt-in の挙動不変）。

## テスト

- `crates/engine/tests/index_declaration_targets.rs`（新規）: opt-in なしでの
  無効果（`HnswScope` 設定時を含む）・opt-in ありでのスカラー列絞り込み・
  テーブル単位の HNSW ゲート（`all`: 全テーブル HNSW で宣言の追加・削除の前後で
  経路と Top-k が不変／`declared`: 宣言テーブルのみ HNSW・未宣言テーブルは
  厳密のまま不変・`DROP INDEX` で厳密へ戻り宣言前と同一結果。SQL 表層・Rust
  API・`EXPLAIN` の `ann_plan:` の 3 経路で確認）・テナント境界の非漏えいを
  結合テストで固定。
- `crates/engine/src/catalog.rs` の単体テスト: `hnsw_targeted_in_txn` の
  scope 別判定（`all` は宣言で変わらない／`declared` は自テーブルの宣言のみに
  従う）・カタログ破損時の fail-closed 縮退と `gate_read_failures` 計上・
  `read_txn` スナップショット隔離（TOCTOU 前提）を固定。
- `crates/wire-server/src/hnsw_scope_opt.rs`・`main.rs` の単体テスト、
  `crates/wire-server/tests/wire_hnsw_scope_cli.rs`（子プロセス）: `--hnsw-scope`
  の受理（HNSW opt-in の有無によらず起動）・不正値・値欠落・重複指定・`=`
  連結形の fail-closed 拒否を固定。
- `crates/engine/tests/sql_index_ddl.rs`: 既存の結果不変テストは無変更のまま
  green（opt-in なしでの回帰）。
- Issue #1153（`EXPLAIN` の `scalar_plan:`／`access_path:` を実行時の索引
  構築対象選択へ揃える）: `crates/engine/src/sql/scalar_index.rs` の単体テスト
  （`scalar_plan_under_target_tests`）で純粋関数の分岐（`Auto`／`Declared`／
  `Err`・`id` 述語の非降格・解決不能添字の fail-closed 降格）を固定。
  `crates/engine/tests/explain_scalar_plan_declarations.rs`（新規）: 検索
  `EXPLAIN`（宣言列・宣言外列・複合述語・`id` 述語・`DROP INDEX` 後の復帰）・
  `EXPLAIN` 表示と実行時統計（`index_scans`／`plain_scan_fallbacks`）の一致・
  集計 `EXPLAIN`（`GROUP BY` 列挙形のゲートを含む）・opt-in なしでの無変更を
  結合テストで固定。`crates/engine/tests/core_explain_plan_entry.rs`
  （`explain_entry_reflects_scalar_declaration_target_with_hnsw_opt_in`）:
  `USING PLAN` 経由の `EXPLAIN`（SQL テキスト・
  `EngineCore::explain_bound_plan_in_session` の両方）でも同じ反映が効き、
  行がビット単位で一致することを固定。
