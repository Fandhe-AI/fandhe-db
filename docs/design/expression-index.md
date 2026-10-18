# 式索引の宣言側（parser と式の決定性検査）

対象: Issue #1647（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7
（spec 本文は転記しない）。後続: #1648（キー計算・エンコード・カタログ永続化）→ #1649（DML 維持）→
（プランナ選択は #1650）→ （UNIQUE・失効は #1651）。

## 範囲

`CREATE INDEX ... ((<式>))` と関数呼び出し形 `(f(col))` を構文上受理し、式の決定性
（PostgreSQL の IMMUTABLE 相当）を判定する。**カタログ非参照の構文段だけ**で完結し、決定的と判定した
式も最後に `0A000`（`expression indexes are not supported yet`）で拒否する。永続化しない理由:

- 参照列の `DROP COLUMN`／`RENAME COLUMN`／`ALTER TYPE` への追随（#1651）が無いまま宣言が残る。
- キー計算・維持（#1648・#1649）が無い宣言は効かない索引になり、`UNIQUE` なら一意性の強制漏れ
  （fail-open）になる。
- 式の束縛（`42703`・`42804`・`42883`）と型依存の決定性判定は、TOCTOU を避けるため永続化と同じ
  write txn（`Storage::create_index`）で行うべき（#1648 の担当）。

## 列リスト要素の構文

| 形 | 例 | 扱い |
| -- | -- | ---- |
| 裸の識別子 | `lang`・`lang COLLATE "C"` | 従来どおり列要素 |
| 関数呼び出し形 | `lower(lang)` | 一次式のみ。直後が `,`／`)`／`COLLATE` 以外（`lower(a) + 1`・`lower(a)::text`）は `42601` |
| 括弧式 | `(lang \|\| body)` | 式をノード予算内で解析し、対応する `)` でちょうど終わる必要がある（`(a) + 1` は `42601`） |
| 式が単なる列参照 | `((lang))`・`((lang COLLATE "C"))` | 通常の列要素へ正規化（重複列 `42601` の対象） |
| リテラル・空括弧 | `('a')`・`()` | 従来どおり `0A000` |

## 判定順（全て構文段・カタログ非参照）

1. 字句
2. 要素ごとの先読み検査（`sql::index_expr::precheck_element_tokens`。トークン区間を左端優先で線形走査）
3. 要素の構文解析（`42601`／`54000`）
4. 文全体の構造検査（重複列・`USING hnsw` の列数・`UNIQUE` との併用・要素数上限）
5. 式要素の決定性（`sql::index_expr::check_index_expression_determinism`）
6. 部分索引 `WHERE`（`0A000`）・文末
7. 最終ゲート（`reject_unimplemented_expression_keys`。`USING hnsw` ＋式も `0A000`）

## SQLSTATE

| 検出 | SQLSTATE | 固定文言（PostgreSQL 17 の公開文言） |
| ---- | -------- | ------------------------------------ |
| `(` 直後が `SELECT`（`EXISTS`・`IN`・`ARRAY(` を含む） | `0A000` | `cannot use subquery in index expression` |
| 集計・ウィンドウ関数名の呼び出し直後が `OVER` | `42P20` | `window functions are not allowed in index expressions` |
| 集計関数（`OVER` なし） | `42803` | `aggregate functions are not allowed in index expressions` |
| 集合返却関数 | `0A000` | `set-returning functions are not allowed in index expressions` |
| 確定の非決定式 | `42P17` | `functions in index expression must be marked IMMUTABLE` |
| 組み込み以外の関数 | `0A000` | `user-defined functions in index expressions are not supported` |
| 決定的な式（最終ゲート） | `0A000` | `expression indexes are not supported yet` |

文言は `&'static str` の固定文言（新 variant `InvalidObjectDefinition`・`GroupingError`・
`WindowingError`）で、列名・値・テナント名を載せられない型にしている。

## 決定性判定表（構文段）

`Expr` の全 variant を `_ =>` 無しで網羅する再帰関数。引数の型に依存せず確実に非決定的なものだけを
`42P17` にし、型に依存するものは通して #1648 の束縛後に再判定する。

| 区分 | 対象 | 構文段 |
| ---- | ---- | ------ |
| 非決定的（確定） | 括弧なし SQL 値関数（`CURRENT_TIMESTAMP` 等）、`now`・`transaction_timestamp`・`statement_timestamp`・`clock_timestamp`、1 引数の `timezone`、`concat`・`concat_ws`、`to_json`・`to_jsonb`・`(j)son(b)_build_object/array`、`array_to_string`、本エンジンに無い既知の非決定名（`random`・`nextval` 等の小さな明示リスト） | `42P17` |
| 型依存（#1648 が束縛後に再判定） | `date_part`・`date_trunc`・2 引数の `timezone`、時刻型が絡む `CAST`・算術、`TIMESTAMPTZ` リテラル（セッション TZ 解決のもの）、`COLLATE`、SQL/JSON 問い合わせ | 通す（子は再帰検査） |
| 決定的 | リテラル・列参照・`lower`・`upper`・`length`・算術・`CASE`・`COALESCE`・`NULLIF` など | 通す |
| 組み込み以外 | セッション UDF・未知名 | `0A000` |

非決定名の区分は PostgreSQL 17 の `pg_proc.provolatile` が全オーバーロードで非 IMMUTABLE のものに限る
（偽の `42P17` を避ける）。

## PostgreSQL・spec との差

- 順位関数の `OVER` なし、非窓関数への `OVER` は PostgreSQL では `42809` だが、本実装は fail-closed の
  `42601`（ERR-6 の既存行での扱いは spec 側の確認事項）。
- UDF は `0A000`。`UdfRegistry` はセッション単位で永続化されず、永続する索引がセッション外で消える関数に
  依存すると維持できず fail-open になるため。INDEX-10 の宣言的 UDF の期待は、永続する関数基盤が入るまで
  満たせない（spec 側の確認事項）。
- `42P17` の ERR-6 への収載状況の確認が必要（spec 側の確認事項）。

## 後続 Issue への申し送り

- **#1648**: `reject_unimplemented_expression_keys` の呼び出しを外し、`IndexDef` に式要素を追加
  （カタログ形式の新版。式は正規化 SQL テキストで保存し再パースの往復一致を検査）。
  `Storage::create_index` の write txn 内で式を束縛して型依存区分を再判定し、`TimestampTzLiteral`
  （`session_local`）を確定値へ畳み、式から参照列集合を抽出する。
- **#1651**: `UNIQUE` 式索引の強制、参照列の `DROP COLUMN`（索引削除）・`ALTER TYPE`（再構築）・
  `RENAME COLUMN` 追随。
- **#1650**: 正規化構文での式一致判定（#1648 の正規化テキストを共有）。
