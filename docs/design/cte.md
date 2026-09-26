# 非再帰 CTE（`WITH` 句）の設計判断

Issue #928・対象ビヘイビア: SQL-29 (b)（TASK-213）。関連ポインタ:
RLS-10 (b)・RLS-7・RLS-8（暗黙適用）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 概要

CTE を「クエリの中だけで有効な名前なしビュー」として扱い、参照時にインライン
展開する。`docs/design/create-view.md`（TABLE-18・TASK-205、Issue #909）と
同型の設計で、第 2 の SQL パーサー・実行器は作らない。CTE 本文は `CREATE
VIEW` 本文と同一の許可パーサー（`sql::allowlist::parse_view_body`）で検証し、
名前解決は `sql::view::resolve_from` と同じ「内側から外側へ畳み込む」処理を
`sql::cte::resolve_relation` へ切り出して共有する。

## 受理する構文

```
WITH <name> AS (SELECT <* | 列名リスト> FROM <table | view | cte> [WHERE <単純述語> [AND ...]])
    [, <name> AS (...)]*
<主クエリ: SELECT <* | 列名リスト> FROM <name> [WHERE ...] LIMIT n>
```

- CTE 本文は `parse_view_body` の受理範囲（`LIMIT`・`ORDER BY`・`USING PLAN`・
  集計・式項目・UDF 呼び出し述語はいずれも `42601`）に加え、本文内の `;`
  （`Token::Punct(';')`）も明示的に拒否する。`parse_view_body` が使う
  `expect_end_of_statement` は末尾の単一 `;` を許容してしまうため、CTE の
  括弧内トークン列に対しては呼び出し前に別途検査する。
- 主クエリは広域取得（SQL-15。`ParsedSelect::Scan`）のみを受理する。順位付き
  （`ORDER BY`／`USING PLAN`）・集計は明示的なメッセージで `42601` に拒否し、
  `WITH` 句を剥がして後段へ流すことは絶対にしない（同名の実テーブルを黙って
  読む危険があるため）。
- `WITH RECURSIVE`・列名リスト `<name>(a, b)`・`AS MATERIALIZED`／
  `AS NOT MATERIALIZED`・CTE 名の重複・データ変更 CTE（`WITH x AS (DELETE
  ...)` 等。body が `SELECT` 以外は受理しない）はいずれも `42601`。
- 定義数上限（`MAX_CTE_DEFINITIONS = 16`）・連鎖の深さ上限
  （`MAX_CTE_NESTING_DEPTH = 4`）の超過はいずれも `54000`。加えて、CTE 名の
  解決回数を「1 回のトップレベル呼び出し（1 つの CTE 定義の事前検証、または
  主クエリの解決）」単位でリセットするカウンタ（`MAX_CTE_REFERENCES = 32`。
  将来 JOIN が入ることに備えた明示的な上限）も超過時は同じく `54000`。この
  3 つは独立の上限ではなく、深さ上限が 1 回の呼び出し系列の長さを構造的に
  `MAX_CTE_NESTING_DEPTH + 1` 段に抑えるため、参照回数上限は現状の単一
  リレーション解決では実質到達しない（Issue #928 レビュー指摘: 文全体で
  1 つのカウンタを共有する実装は、事前検証ループが各定義のチェーンを
  再帰的に辿るたびに参照回数が累積し、定義数・深さのどちらも上限内の
  有効なクエリを誤って拒否する不具合があったため、呼び出し単位のリセットに
  修正した）。
- 拡張クエリプロトコルの `$n` を含む `WITH` 文は一律 `42601`（カーソルと同じ
  fail-closed 方針。理由は下記「対象外」節）。

## 名前解決（PostgreSQL と同じ意味論）

CTE 名はカタログのテーブル・ビューを隠す。i 番目の CTE の本文から見えるのは
0..i-1 番目の CTE だけで、見つからなければ `sql::view::resolve_from`
（ビュー→テーブル）へ進む。この可視範囲の境界により、自己参照・循環は構造的
に作れない（自己参照に見える名前は同名の実リレーションへ解決される。
`sql29_cte.rs::cte_name_hides_real_table_of_the_same_name` で固定）。

## 展開方式（検証段階でのインライン展開）

`sql::allowlist::validate_sql_tokens` に `WITH` 分岐を追加した。

- `parse_with_clause`（`Parser` のメソッド）が CTE 定義列を切り出し、各本文を
  `parse_view_body` で検証する。括弧の対応取りは `COPY (<SELECT>) TO STDOUT`
  と共有する `split_parenthesized` を再利用する。
- `sql::cte::resolve_relation` が CTE 名の解決を行い、一致すれば本文の FROM を
  再帰的に解決してから `sql::view::check_columns_within_view` で列スコープを
  検証し、内側の述語を先頭に置いて合成する。一致しなければ `sql::view::
  resolve_from` へ委譲する。
- ビュー・CTE いずれの解決結果（`sql::view::Resolved`）も、新設した
  `build_scan_from_resolved`（`sql::allowlist` 内、`validate_sql_tokens` の
  `Statement::Scan` 分岐と `WITH` 分岐の双方から呼ぶ）へ合流させ、
  `ValidatedScan` を組み立てる。これにより VIEW の既存の挙動をビット単位で
  変えずに CTE 経由の畳み込みと処理を共有する。
- 参照されない CTE も含め、すべての定義本文の FROM を検証する（決定性・
  fail-closed のため。構造検証・存在確認を省略しない）。

## RLS-10 (b)・RLS-7・RLS-8 の不変条件

`sql::cte::CteDef` は `tenant_id`／`PolicyContext`／作成者の情報を一切保持
しない（CTE はクエリ実行のたびに使い捨てで、カタログに永続化すらされない）。
畳み込み後の文は、参照した**セッション自身**の `PolicyContext` を使う既存の
実行経路（VIEW と共有する `build_scan_from_resolved` → `Statement::Scan`）
でしか評価されないため、作成者・他テナントの可視性が引き継がれることは構造
的に起こらない（`sql29_cte.rs` で 3 テナント対照検証済み）。

## 行数上限が構造的に満たされる根拠

CTE は実体化しない（インライン展開のみ）。主クエリ（`ParsedSelect::Scan`）の
`LIMIT` は既存の許可リストが `1..=MAX_SEARCH_K` に制限しているため、CTE 経由
でも直接クエリと同じ行数上限が構造的に適用される。CTE 自身が返す「中間結果」
という概念は実行時に存在しない（畳み込み後は基底テーブルへの `WHERE` 述語の
連言になる）。

## 対象外・申し送り

- CTE と JOIN の併用、サブクエリ、集合演算（別 Issue の対象）。JOIN 構文
  そのものが許可リスト外のため、現状は `42601` で構造的に拒否される。
- 順位付き（`ORDER BY <=>`／`USING PLAN`）・集計・`EXPLAIN` の主クエリで
  CTE を参照すること（VIEW と同じスコープ縮小）。
- 拡張クエリプロトコルで `WITH` 文に `$n` を束縛すること。`sql::params::
  validate_param_positions` の等価述語判定（パターン 4）はトークン列上の
  位置だけで許可位置を判定するが、CTE を含む文では CTE の `WHERE` 述語が
  主クエリの述語の前へ挿入される（`build_scan_from_resolved`）ため、束縛時に
  `$n` の対応先が組み立て後の述語列とトークン列上の位置とでずれる恐れがある。
  この対応ずれを解消してから別 Issue で扱う。
- `WITH RECURSIVE`（SQL-29 で対象外）、列名リスト、`MATERIALIZED` 指定。
- NoSQL（HTTP）表層での CTE（SQL-28〜30 は非対応のまま）。
