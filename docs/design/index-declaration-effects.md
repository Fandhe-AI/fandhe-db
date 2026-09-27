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

## 優先順位（上位スイッチ）

| 起動構成 | 宣言あり | 宣言なし |
| --- | --- | --- |
| opt-in なし（既定エンジン・`with_provider`／`from_storage`） | 宣言は無視（現行挙動のまま） | 現行挙動 |
| opt-in あり（`SearchEngineKind::Hnsw`） | 宣言が構築対象を**絞り込む** | 現行の自動挙動を維持 |

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

## HNSW（カタログ全体単位）

- テーブルは VECTOR 列を高々 1 本しか持てない（`catalog.rs` のスキーマ検証）ため、
  「テーブル単位で宣言なし → 自動」は HNSW では常に真になり無効果（vacuous）に
  なる。よって HNSW は「カタログ全体に `IndexKind::Hnsw` 宣言が 1 件でもあるか」
  （`DeclaredIndexTargets::hnsw_anywhere`）を切替点とする
  （`catalog::hnsw_targeted_in_txn`）:
  - opt-in 有効かつ HNSW 宣言が 0 件: 全テーブルで現行どおり HNSW
  - opt-in 有効かつ HNSW 宣言が 1 件以上: HNSW 経路を使うのは宣言のあるテーブル
    のみ（他テーブルは厳密 brute-force）
  - opt-in なし: 常に brute-force
- 他テーブルへ効く宣言でも当該他テーブルの世代は進まないが、ゲートを
  **適格性判定（`HnswIndexCache` 照会より前）**に置くため、ゲート対象外の
  テーブルの `HnswIndexCache` エントリは照会・構築されない（stale 使用の経路が
  無い）。宣言削除で再び HNSW 経路に戻った際も、キャッシュは既存の世代照合＋
  overlay の契約で正しさを保つ。
- `sql::exec`（`AnnShapeInput.hnsw_enabled`）・`core.rs`（Rust API
  `search_with_snapshot`・`EXPLAIN` の `ann_plan:` 行）はいずれも
  `catalog::hnsw_targeted_in_txn` を呼ぶことで、実行時判定と `EXPLAIN` 表示の
  乖離を作らない。

## 宣言の読み取りと fail-closed

- 宣言はクエリの `read_txn`（アリーナ・世代と同一スナップショット）から読む
  （`catalog::declared_index_targets_in_txn`）。`Storage::list_indexes`
  （独自に read txn を開く）はクエリ経路から呼ばない。
- カタログ値のデコード失敗・走査上限（`MAX_INDEX_COUNT`）超過は「宣言なし→
  自動」へは倒さず、**索引を使わない側**（スカラー: 索引構築失敗扱い →
  plain scan／HNSW: brute-force）へ倒す。いずれも厳密結果になるため結果は
  変わらず、fail-soft かつ安全側。
- `EXPLAIN`（`core.rs::explain_engine_for`）はこの判定専用に新規の read txn を
  開く（検索本体を実行しない契約を保つため、計画開始時の txn を引き回さない）。
  読み取り失敗は `false`（brute-force 表示）へ倒す。

## テナント境界・RLS への影響

- 索引の物理表現は引き続き `(table, PolicyContext)` 可視スナップショットからのみ
  構築する。宣言は構築**対象列の絞り込み**と ANN 経路の適格性にのみ作用し、
  RLS 暗黙適用・可視性判定には一切触れない。統計・`EXPLAIN` にテナント ID・
  行 ID・他テナントの存在情報を追加しない。
- 結果集合・RLS 境界は宣言の有無で変わらない（影響は性能のみ）。
  `crates/engine/tests/index_declaration_targets.rs`・
  `crates/engine/tests/sql_index_ddl.rs::index_declarations_do_not_change_query_results_or_rls`
  で固定している。

## 対象外（申し送り）

- `EXPLAIN` への索引名露出（`scalar_plan:` 行は束縛時の静的判定のまま。平均値長
  ゲートと同じく、宣言で除外された列も静的判定上は索引適格と表示されうる既知の
  制約）
- 疎索引（BM25）の宣言、NoSQL 表層の索引 DDL（[index-ddl-declaration.md]
  (index-ddl-declaration.md) の申し送りのまま）
- 宣言による強制索引化（既存のゲート・`MIN_INDEXED_ROWS` を無視する経路は作らない）
- スカラー専用 opt-in の新設（上位スイッチは起動時 HNSW opt-in のみ）
- ホットパスの宣言読み取りコストを削る世代キー付き小キャッシュ（性能問題が
  実測されれば別 Issue で判断する）

## 影響を受ける既存 fixture

宣言を一切使わない既存の opt-in 利用者（`tests/hnsw_cache.rs`・
`tests/hnsw_provider.rs`・`tests/hnsw_acorn_recall.rs`・
`tests/hnsw_hybrid_refetch.rs`・`tests/fixtures/recall_engine.rs`・
`crates/wire-server/tests/wire_tls_cli.rs` 等）は、宣言が 0 件のため
`ScalarIndexTarget::Auto`／HNSW 全テーブル対象のまま無変更で通ることを
既存テストの回帰実行で確認済み（宣言なし opt-in の挙動不変）。

## テスト

- `crates/engine/tests/index_declaration_targets.rs`（新規）: opt-in なしでの
  無効果・opt-in ありでのスカラー列絞り込み・カタログ全体単位の HNSW ゲート・
  `DROP INDEX` によるゲート復帰・テナント境界の非漏えいを結合テストで固定。
- `crates/engine/tests/sql_index_ddl.rs`: 既存の結果不変テストは無変更のまま
  green（opt-in なしでの回帰）。
