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
  走査の要約（HNSW 宣言テーブル集合）を `IndexCatalogGateCache` がストレージ
  世代単位に再利用する。`all` では集合の中身を使わず、走査の成否（カタログを
  読み取れるか）だけを使う。読み取れない場合はいずれの scope でも fail-closed に
  brute-force へ倒す（次節）。
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

- **`EXPLAIN` の `scalar_plan:` は「索引が実際に使われる」ことを意味しない**:
  `scalar_plan:`（`sql::scalar_plan::classify_scalar_plan`）は `WHERE`
  述語の**形**（列型・演算子の組み合わせ）だけを見る束縛時の静的判定であり、
  カタログ・スキーマ・行データを一切参照しない（`ScalarShapeInput` に
  storage/schema を渡さない設計。§2 系の `ann_plan:` が
  `catalog::hnsw_targeted_in_txn` で実行時ゲートと揃えているのとは対照的）。
  このため `scalar_plan:` が索引適格と表示されても、実行時に対象列が
  索引化されていなければ `ScalarIndex::resolve_candidates`
  （`sql/scalar_index.rs`）は該当述語で `CandidateResolution::FallbackNoIndex`
  を返し、**クエリ全体が全走査（plain scan）へフォールバックする**
  （索引化した候補だけ通して残りを事後フィルタする、ではない）。これは
  宣言で除外された列に限らず、平均値長ゲート（Issue #632）・`2^53` ゲート
  （Issue #893）でも既に起きている既知の制約であり、宣言（本 Issue）は
  `scalar_plan:` が反映しない実行時ビルドゲートの 3 件目にすぎない
  （RLS・可視性・結果の正しさには影響しない。fail-closed に全走査へ倒れる
  だけで誤った結果を返さない）。
  `scalar_plan:`／`access_path:` を実行時ビルドゲート（宣言・平均値長・
  `2^53`）まで反映させる修正は、`ann_plan:` と同じ「実行時判定・`EXPLAIN`
  表示の単一情報源化」パターンを `search_explain_from_bound`・
  `aggregate_explain_from_bound`・`run_explain_plan` の 3 経路すべてに
  広げる設計変更（SQL／NoSQL 表層の bit 同一性テストの更新を伴う）になる
  ため、宣言のみを対象にした部分修正は行わず別 Issue（#1153）の対象と
  する。Issue #1066 で索引経路使用時に `ann_plan:`／`scalar_plan:` 行へ
  使用**索引名**の注記（`index=<name>`）を追加したが、これは上記のトークン
  不一致そのものを解消するものではない——`scalar_plan:` のトークン判定式
  （`classify_scalar_plan`）自体は変えておらず、名前が付いた場合でも
  「索引が実際に使われる」ことを保証しない（本節の制約は変わらず有効。
  `aggregate_explain_from_bound` は #1066 でカタログ読み取りを追加した
  ため `&self` メソッドへ変更済み）。トークン自体の実行時整合は引き続き
  #1153 の管轄。
- 疎索引（BM25）の宣言、NoSQL 表層の索引 DDL（[index-ddl-declaration.md]
  (index-ddl-declaration.md) の申し送りのまま）
- 宣言による強制索引化（既存のゲート・`MIN_INDEXED_ROWS` を無視する経路は作らない）
- スカラー専用 opt-in の新設（上位スイッチは起動時 HNSW opt-in のみ）
- **`IndexCatalogGateCache`（`catalog.rs`）はストレージ全体世代キーのため、
  索引宣言と無関係な行 DML の commit でも次回参照時に再走査が起きる**
  （codex-review P2 指摘・PR #1124）: `hnsw_targeted_in_txn` のカタログ全件
  走査の結果キャッシュは `crate::storage::current_generation_in_txn`（ストレージ
  全体の単一世代カウンタ）をキーにしている。これは索引カタログを変更する 4 経路
  （`create_index`／`drop_index`／`drop_table`／`alter_table_drop_column`）
  がいずれも commit 前に必ずこのカウンタを進める（取りこぼしなし）ことを
  根拠に選んだキーだが、宣言と無関係な通常の行 DML でも同じカウンタが進む
  ため、書き込みと検索が交互に発生する構成ではキャッシュがほぼ効かず、
  キャッシュ導入前と同じフルスキャン 1 回分のコストが検索のたびに残る
  （悪化はしない。`catalog.rs` の `IndexCatalogGateCache` ドキュメンテーション
  コメント参照）。
  是正する場合の設計方針: 上記 4 経路の commit 時にのみ進む専用の「索引
  カタログ世代」カウンタを新設し、`hnsw_targeted_in_txn`・スカラー宣言解決の
  両方をそのカウンタでキー付けする（通常の行 DML による過剰無効化を避ける）。
  ただし新カウンタは commit_boundary 経由の全 4 経路で確実に進める必要があり
  （1 経路でも取りこぼすと、`declared` で stale な HNSW 宣言テーブル集合を
  再利用し、宣言済み・削除済みの経路切り替えを取りこぼす）、永続フォーマット
  （新カウンタ未保持の既存 DB）との互換性も設計する必要があるため、本 Issue
  では部分修正を行わず別 Issue（#1154。`perf` 分類）の対象とする。

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
