# JSON 演算子・関数・jsonpath の ExprType への写像（Issue #1595）

- ステータス: 公開境界の縮小版（詳細な設計判断は private spec 側で管理し、本書はポインタと現状の記録に限る）
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-25・TABLE-14
  （関連: SQL-26・SQL-42・SQL-47・NOSQL-8・INDEX-8・WIRE-13・WIRE-14・ERR-4、
  TASK-243・TASK-273）

## 背景・目的

`JSON`／`JSONB` は列型として保存・往復できるが、式評価器（`sql::udf_call` の
`ExprType`／`ExprValue`／`BoundExpr`）には JSON の型がない。式中の JSON 列参照は
`22000`、パス演算子は `42601` で拒否される（[column-type-extension.md](column-type-extension.md)
「#889 追記」決定 D5）。親 #1594 配下の兄弟 Issue（#1596〜#1599・#2180・#2181）と
jsonpath 系（#2182 配下の #2183〜#2187）は、JSON 値が式評価器の中でどの型・値表現に
なるかを前提にする。本書は、その写像の設計判断が private spec 側にあること、および
公開コードの現状と担当 Issue の対応を 1 か所に示す。本 Issue は docs のみでコードは
変更しない。

## 公開コードの現状（`origin/main` 時点）

- `ExprType` は `Scalar`／`Vector`／`Bool`／`Text`／`Date`／`Timestamp` の 6 値で、
  JSON 用の variant はない。
- 字句化済み（構文段が `42601` で拒否）: `->` `->>` `#>` `#>>` `::` `||`。
  JSON 関連の他の演算子は未字句化。
- `json.rs` に `parse_json`・`write_canonical`・`canonical_equality_text`・`JsonPath`／
  `extract_path` 等（Rust API 限定）と上限定数がある。
- JSON 列の `=`／`IN`／`IS NULL` は宣言的述語経路で処理され、式評価器を経由しない。
  `GROUP BY`／`DISTINCT` は `Cell::Json` を拒否する。
- wire 側は `ColumnMeta::Scalar{ty: Json|Jsonb}` から OID 114／3802 への写像が実装済み。
- 集合返却関数の基盤（#2220）と CAST／`::`（#1588〜#1593）は未実装。

## Issue #1596 での変更（パス演算子の構文・束縛・評価）

- `ExprType::Json`・`ExprValue::Json`（正規化テキスト）・`BoundExpr::JsonbColumnRef`／
  `JsonAccess` を最小限だけ導入した。jsonb 値を消費できるのは投影とパス演算子の左辺だけで、
  比較・順序・集計・関数引数・`CASE`／`COALESCE`／`NULLIF`・`UPDATE SET` の代入は
  `0A000`（#1597 が開放する）。
- 構文は `Expr::JsonAccess`（`BinOp` とは別 variant）。優先順位は加減算より弱く比較より強い
  左結合。CHECK 本体では受理しない。
- `->`・`->>` の右辺は text（キー）または int4（添字。負値は末尾から）、`#>`・`#>>` の右辺は
  text[] リテラルのみ。左辺が jsonb でなければ `42883`、`json` 列は `0A000`、jsonb 列の
  裸の参照は従来どおり `22000`。
- 結果: `->`・`#>` は jsonb（OID 3802）、`->>`・`#>>` は text（OID 25）。存在しないキー・
  範囲外の添字・容器の型不一致は SQL の NULL。
- DML の content hash にタグ 12 を追加した（[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)）。

## Issue #1597 での変更（比較・述語・ORDER BY）

- jsonb 同士の比較（`=` `<` `>` `<=` `>=`）は PostgreSQL 17 の jsonb btree 順序で行う。
  順序の定義は `json::jsonb_sort_key`（バイト列の辞書順が全順序になるソートキー）の 1 か所に
  置き、WHERE の比較と ORDER BY が共有する。キーの等しさは `canonical_equality_text` の
  等しさと一致する（数値は 10 進の厳密比較。`1`・`1.0`・`10e-1` は等しい）。
- jsonb 式と比較される型なしの文字列リテラルは jsonb として解析する（`bind_jsonb_literal`。
  不正な JSON は `22P02`、長さ超過は `54000`）。キャスト（#1588〜#1593）は未実装のため、
  この暗黙解釈で `WHERE docb -> 'k' = '1'` を実現する。jsonb と他の型の比較は `42883`。
- ORDER BY は jsonb の式キー（パス演算子・jsonb 列・jsonb の式項目の別名）を受理する。
  NULL は昇順で末尾・降順で先頭。ウィンドウ関数付きの文では `0A000`。
- jsonb 列の裸の参照を式の中で受理する。jsonb を受け付けない位置（算術・関数引数・集計・
  `CASE` 等・代入）は `0A000`。そのため `vec_norm(docb)` のような従来エラーだった文の
  SQLSTATE が `22000` から `0A000` に変わる。
- 出力形式（コンパクトな正規化テキスト）は #1596 から変えない。

## 設計判断の所在

型の形・値表現・字句と優先順位・演算子と関数の型解決・集合返却関数・jsonpath・
比較と並び順・上限・セキュリティ上の不変条件の各判断は、private spec 側の
上記ビヘイビア ID 群と各担当 Issue の実装時に確定する（`docs/spec` 配下。
本書へは転記しない）。実装者は spec を参照し、各 Issue の PR で判断を public に
出してよい範囲だけを記録する。

## 担当 Issue 対応表

| 領域 | 担当 Issue |
| ---- | ---------- |
| 式評価器への JSON 型の導入（jsonb。比較・述語・ORDER BY は #1597 で実装済み） | #1597 |
| json 型の演算・関数 | #2180 |
| パス演算子の構文 | #1596 |
| 包含・存在演算子 | #1598 |
| 削除・更新系演算子 | #2181 |
| JSON 関数・集合返却関数 | #1599 |
| jsonpath | #2183〜#2187 |
| 索引 | #2204 |

## 対象外・申し送り

- コード実装はすべて上記の担当 Issue が行う。
- 集計関数（`jsonb_agg` 系）の担当 Issue は未割当。
- SRF 基盤（#2220 系）。
- spec 側の課題があれば spec リポ側で扱う。
