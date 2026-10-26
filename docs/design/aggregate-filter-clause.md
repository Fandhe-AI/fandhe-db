# 集計の `FILTER (WHERE ...)` 句（Issue #1806）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-57・SQL-13・SQL-14・SQL-25 (c)・
  SQL-29・SQL-61・RLS-7・RLS-8
- 前提: [string-agg.md](string-agg.md)・[array-agg.md](array-agg.md)（集計内 `ORDER BY`・`DISTINCT`）
- 検証コード: `crates/engine/src/sql/allowlist.rs`・`parser.rs`・`aggregate.rs`・`group_by.rs`・`subquery.rs`・
  `view.rs`・`view_alias.rs`・`params.rs`・`const_select.rs`、`crates/engine/tests/sql57_aggregate_filter.rs`

## 背景・目的

`agg(...) FILTER (WHERE <条件>)` は構文段で受理されず `42601` になっていた。集計項目ごとに行条件を付け、
条件が真の行だけをその集計のアキュムレータへ入れる（PostgreSQL 17 と同じ意味論）。

## 決定

### D1: 構文と AST

- `AggregateItem.filter: Vec<WherePredicate>`（空なら FILTER なし。AND 結合順）。集計呼び出しの `)` の直後に
  文脈語 `FILTER` と `(` が続くときだけ句として読む（`AS filter` のような別名は壊さない）
- 条件の文法は `WHERE` と同一（`parse_where_or`。`)` を境界とする）。`FILTER (` の後が `WHERE` でなければ `42601`
- 関数ごとの分岐を持たない。今後追加される集計関数にも自動で効く
- 先読み検査（`precheck_aggregate_filter_tokens`）: 条件中のウィンドウ関数（`... OVER`）は `42P20`、集計関数は
  `42803`（OVER を優先）、`$n` は `42601`。部分クエリの内側は走査しない（内側の集計は合法）。文全体の
  `) OVER (` 走査によるウィンドウ形への誤振り分けより前に、`validate_select_statement` の冒頭でも同じ検査を行う

### D2: 束縛と実行

- 束縛は文の `WHERE` と同じ経路（`bind_where_predicates`）。結果は `BoundAggregateItem.filter`
  （`BoundAggregateFilter`）に保持し、`BoundAggregate.rls_predicate_present` には反映しない
- 式ノード予算は文全体で共有する（DoS 有界）
- 評価は `sql::aggregate::item_filter_matches` の 1 箇所。全走査・索引候補・`GROUP BY`（`accumulate_row`）が
  共有する。可視性判定（RLS）と文の `WHERE` を通過した後の AGG 段でのみ評価する
- 全行除外時は既存の観測 0 件の規則で `count` は 0、それ以外は NULL。`GROUP BY` ではグループ自体は文の
  `WHERE` で決まる

### D3: 行を観測しない近道の無効化

- `count_star_only`（索引候補数の加算）: FILTER つきの項目があれば使わない
- `ReferencedColumns::derive`: FILTER の参照列・embedding を `scalar_mask`・`needs_embedding` へ反映し、
  `has_scalar_reference` を立てて `DecodeTier::Fast`（visible cache 経路）を選ばせない。
  反映漏れは縮小デコードした行で条件が NULL と取り違えられる黙った誤集計になるため

### D4: 部分クエリ

- トップレベルの `Statement::Aggregate` 実行経路で、文の `WHERE` の解決直後に FILTER 中の部分クエリを
  同じ `PolicyContext`・同じスナップショット・同じ予算で解決する（`subquery::resolve_aggregate_item_filters`）。
  テナント境界は文の `WHERE` の部分クエリと同一
- 部分クエリを許さない文脈（カーソル・ビュー本体等）は構文段が `42601` で拒否（fail-closed）

### D5: ビュー・Prepared・FROM なし

- ビュー: `collect_referenced_idents` に FILTER の識別子を含め、公開列の検査（非公開列は `42703`。filter
  oracle の防止）と別名ビューの物理名写像（`rewrite_aggregate`）を行う
- Prepared: `params::where_region` は直前が `FILTER (` の `WHERE` を領域開始に選ばない（等値リテラルの序数
  フラグのずれを防ぐ）。FILTER 内の `$n` は `42601`
- FROM なしの集計の FILTER は `0A000`、JOIN の集計は従来どおり `42601`

## 対象外（追跡）

- ウィンドウ集計の `FILTER`（`agg(...) FILTER (...) OVER (...)` は `42601`。Phase 5 後半）
- FILTER 内の `$n`（Prepared）
- JOIN の集計・FROM なしの集計・NoSQL 表層の FILTER
- 集計でない関数に `FILTER` を付けた形の PostgreSQL 固有コード（`42809`）への是正（現状 `42601`）
