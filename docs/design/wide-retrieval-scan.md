# ADR: 広域取得モード（ソートなしのフィルタ取得）を SQL 表層へ追加する（Issue #454）

- ステータス: Implemented（spec 側ビヘイビア ID は SQL-15・TASK-170 として付与
  済み〔vector-db-spec#12・spec main `84eb7ef4d2e2e1d3cb35bd95e13e60bec10e88c6`・
  2026-09-07〕。オーナー判断〔2026-09-07〕による本 ADR の先行公開経緯・ID 差し
  替えの経緯は下記「spec 側への申し送り」参照。SQL-15 自体の確定化〔本リポ側
  の受け入れ確認・wire 経由 3 クライアント実測〕は TASK-170 が担う）
- 対応: Issue #454・spec 側 SQL-15・TASK-170（vector-db-spec#12）
- 関連ポインタ: SQL-15・TASK-170・TASK-161・TASK-162・SQL-12・SEARCH-9（取得
  モード `recall`／`precision` との関係）・RLS-8（TASK-138。全読み取り経路への
  RLS 一般化）・TABLE-12（キー/ヘッダ tenant 整合検査）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 検証コード: `crates/engine/src/sql/allowlist.rs`（`Statement::Scan`・
  `ValidatedScan` 単体テスト）・`crates/engine/src/sql/scan.rs`（`execute_scan`
  単体テスト。nullable VECTOR・早期終了）・`crates/engine/tests/sql_scan.rs`
  （実行契約の結合テスト）・`crates/engine/tests/rls_generalized.rs`
  （`scan_limit_at_or_above_visible_total_matches_oracle_set_exactly`・
  `scan_small_limit_returns_subset_with_exact_count_and_no_leak`）・
  `crates/wire-server/tests/wire_scan.rs`（層 A）・
  `crates/wire-server/tests/extended_syntax_e2e.rs::three_clients_run_scan_where_nosort`
  （層 B）・`crates/engine/tests/sql_scan_public_api.rs`（TASK-186・NOSQL-3。
  `bind_scan`／`execute_scan`／`BoundScan` の公開 API 昇格の結合テスト。Issue #726）

## 背景

本 DB の設計思想「正解を含むデータ群を広く返し、丸ごと LLM へ渡す」を SQL 表層で
直接表現できる経路が無かった。既存の許可リスト（`sql/allowlist.rs::parse_select_shape`）
は行を返す `SELECT` に `ORDER BY <距離>` か `USING PLAN(...)` を必須とし、
`ORDER BY` なしのスカラーフィルタのみの行取得は `42601` で拒否していた。

他 DB 横断ベンチ（Issue #406 関連 PR。`docs/design/crossdb-bench.md`「広域取得」節）
では `scan_where_nosort_k500`（`WHERE lang = 'ja' LIMIT 500`・`id, body` 投影）が
self のみ unsupported だった。他 DB は同等のクエリを直接受理する。

既存の `USING MODE 'recall'`／`'precision'`（TASK-161・TASK-162・SQL-12・
SEARCH-9）は Top-k 固定件数／確信度ゲートであり、「ソートせず広く返す」取得の
構文ではない。

## 設計方針

### 構文・返却契約（本リポの実装既定値。spec ビヘイビア ID は SQL-15・TASK-170）

```text
SELECT <投影（既存許可形: *, 列名列, 式項目〔UDF 含む〕）> FROM <table>
  [WHERE <既存許可述語（等価・LIKE 前方一致・visible()・式述語）>]
  LIMIT <n>
```

| 項目 | 契約 |
| --- | --- |
| 受理条件 | `WHERE`（省略可）の直後に `LIMIT` が現れる形のみ。`ORDER BY`・`USING PLAN` は従来どおり検索 SELECT として別経路 |
| `LIMIT` | 必須。`sql::parser::validate_search_limit` を再利用し `1..=core::MAX_SEARCH_K`（10,000）。範囲外は `22000`（既存契約と同一） |
| `USING MODE`／`HINT ORDER` | 構造上受理しない（`LIMIT n [OFFSET m]` の後は文末のみ）。集計文が `USING MODE` を受理しない既存判断（「取得モードの余地を持たない」）と同じ理由 |
| `OFFSET`（Issue #916・SQL-25 (b)・TASK-209） | `LIMIT n` の直後に任意で受理する。詳細（受理範囲・順序保証との関係・カーソル方式との選択・深いページングのコスト特性）は [sql-offset-paging.md](sql-offset-paging.md) 参照。本節「順序」の性質（順序保証なし・同一スナップショット内で決定的）はそのまま `OFFSET` の前提になる |
| セッション変数 `SET search_mode` | 広域取得は参照しない（ランキング段・確信度ゲートが存在しないため） |
| `EXPLAIN` 前置 | Issue #922・SQL-27 で受理へ拡大済み（`scalar_plan: plain_scan`／`access_path: full_scan` の 2 行を返す。詳細は [explain-search-engine-exposure.md](explain-search-engine-exposure.md)「追記（Issue #922・SQL-27）」節参照） |
| 順序 | **順序保証なし**。実装は同一スナップショット内で決定的（redb 行テーブルのキー順＝`(tenant_id, id)` 昇順。他テナントの `Public` 行が可視な場合はテナント文字列順で交錯し、`id` 昇順が成り立つのは同一テナント内のみ）。グローバル `id` 順へ「修正」しない・決定性テストの前提として明記する |
| 件数 | 可視かつ `WHERE` を満たす行を先頭から最大 `n` 件。`n` 件集まった時点で走査を打ち切る（早期終了） |
| RLS | `PolicyContext::is_visible` をデコード前に無条件適用（`WHERE visible()` の有無に依存しない。RLS-8）。TABLE-12 のキー/ヘッダ tenant 整合検査を維持 |
| `score` | `ResultRow.score` は `0.0` 固定（wire では非送出） |
| エラー契約 | 既存 `SqlSurfaceError` 写像をそのまま使用（未定義テーブル `42P01`、容量超過 `54000`、破損・内部バグ `XX000`、式評価エラー `22000`/`54000`） |

### しきい値構文・大きな k の広域取得（スコープ外の判断）

「しきい値・大きな k で広く返す Top-k」のうち **しきい値構文**は SQL-15 の
スコープ外（規範化は別途）のため本 Issue では実装しない。大きな k は既存
`LIMIT`（上限 `MAX_SEARCH_K`＝10,000）で既に表現できる。

### 構文の位置付け: `USING MODE` の新値ではなく独立文種

広域取得の構文は `USING MODE` の新値ではなく「`ORDER BY`／`USING PLAN` を伴わない
`SELECT ... LIMIT n`」とした。理由:

1. ベンチ `scan_where_nosort_k500` と他 DB の規範形が bare 形。
2. `SearchMode` enum（SQL-12）へ第 3 値を足すと `precision` ゲート・プランナー
   推定（PLAN-11）・`EXPLAIN` の `mode` 行と絡み spec 改訂範囲が広がる。
3. 集計（`Statement::Aggregate`）と同じ「ランキング段を持たない文種」として
   独立させれば、既存の DISTANCE 中心経路に一切触れずに済む。

### 実行本体: `VectorArena` を経由しない redb 直接走査

`sql::aggregate::execute_aggregate`（TASK-166・SQL-13）と同じ理由で
`VectorArena`（既存の検索 SELECT 実行経路）を使わない: アリーナはスキーマに
`VECTOR` 列が必須で、可視行の embedding を全件バッファへ確保するため、
`VECTOR` 列を持たないテーブルの広域取得や大規模テーブルの `id`/`TEXT` 列のみの
取得には過剰（メモリ）かつ非対応。

`sql/scan.rs::execute_scan` は `sql/aggregate.rs::execute_aggregate` の走査ループと
同一の規約（RLS 適用順序・TABLE-12 整合検査・`DecodeTier` による必要最小限
デコード）を踏襲しつつ、集計アキュムレータの代わりに投影結果セルを蓄積し、
`bound.limit` 件集まった時点で走査を打ち切る（早期終了）。デコード段階の
参照列導出（`decode_tier_for`）は投影列（`ProjectedColumn`）・`WHERE` から
独自に導出する（`sql::aggregate::ReferencedColumns` は集計項目
`BoundAggregateItem` 形状に特化しているため型を共有しない）。

### RLS・早期終了の相互作用

早期終了はテナント境界の縮約を壊さない: 不可視行は `LIMIT` のカウント対象に
一切現れない（デコード前の `ctx.is_visible` 判定でスキップされる）ため、
どの物理走査位置で打ち切っても他テナントの存在・件数の情報を漏らさない。
`tests/rls_generalized.rs` の 2 パターン検証（(a) `LIMIT` が可視総数以上ならオラクル
集合と完全一致、(b) 小さい `LIMIT` でも返却集合がオラクル集合の部分集合かつ
件数が `min(limit, |oracle|)` に一致）で固定した——(a) を欠くとオラクル比較が
早期終了により vacuous になる。

## 影響

- `crates/engine/src/sql/allowlist.rs`: `Statement::Scan(ValidatedScan)` を追加
  （**BREAKING CHANGE**: 既存の網羅的 `match` はワイルドカードアームの追加が
  必要。`Aggregate`・`Explain` 追加時と同じ運用）。`parse_select_shape` の戻り値を
  `ParsedSelect::{Search, Scan}` へ変更（モジュール内 private）。
- `crates/engine/src/sql/parser.rs`: `BoundScan`・`bind_scan` を追加。
- `crates/engine/src/sql/scan.rs`（新設）: `execute_scan`。
- `crates/engine/src/core.rs`: `execute_sql`（非セッション）・
  `execute_validated_in_session` の両方へ `Statement::Scan` アームを追加。
- `crates/wire-server/`: 追加の変更なし（`EngineCore::execute_sql_in_session` を
  経由する既存の簡易クエリ経路がそのまま `Statement::Scan` を受理する）。

`bind_scan`／`execute_scan`／`BoundScan` を engine クレート外へ `pub` 昇格し
（TASK-186・NOSQL-3。Issue #726）、単一 `Storage` 構成で SQL テキスト非経由に
束縛済み scan 計画を実行するセッション対応エントリ
`EngineCore::execute_bound_scan_in_session` を追加した（Issue #728。詳細は
`docs/design/bound-plan-session-entry.md` 参照）。

## スコープ外

- SQL-15 自体の確定化（本リポ側の受け入れ確認・wire 経由 3 クライアント実測。
  TASK-170 が担う。下記「spec 側への申し送り」参照）。
- しきい値による可変件数の広域取得構文（SQL-15 のスコープ外。別タスク）。
- 検索 SELECT（`ORDER BY`）経路の投影固定コスト削減（Issue #453 の管轄。本
  Issue では `bulk_knn_*` の self 実測値は変わらない）。
- 広域取得への二次索引（Issue #359 ADR）適用（`EXPLAIN` 露出自体は Issue #922・
  SQL-27 で対応済み）。
- `make bench-crossdb`／`make e2e-three-client` の実行環境が無い場合の再実測・
  実行はオーナー作業。

## spec 側への申し送り（反映済み）

spec 側でモード定義・構文・返却契約（件数上限・順序保証の有無・RLS 適用）を
ビヘイビア ID として新設する作業は private spec リポ
[vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec) 側で実施され、
SQL-15（`04-behavior/sql-surface.md`）・TASK-170（`05-tasks.md`）として main へ
マージ済み（vector-db-spec#12・spec main
`84eb7ef4d2e2e1d3cb35bd95e13e60bec10e88c6`・2026-09-07）。本 ADR・関連コード内
ポインタは確定済みの ID（SQL-15・TASK-170）へ差し替え済み。SQL-15 自体の確定化
（本リポ側の受け入れ確認・wire 経由 3 クライアント実測）は TASK-170 が担う。

### オーナー判断の経緯（2026-09-07）

spec 側ビヘイビア ID の確定を待つと本 PR（#562）の統合が長期に滞留するため、
オーナー判断（2026-09-07）により本 ADR の契約を spec 側 ID 新設を待たず本リポの
実装既定値として先行公開し、本 PR をマージした。その後 spec 側で SQL-15・
TASK-170 が新設・main へマージされたため（vector-db-spec#12）、本 ADR・関連
コード内ポインタ・コミット/PR 参照を確定済みの ID へ差し替えた。
