# 単一テーブルの FROM 別名と修飾名の解決

対象: Issue #1534（親 #1533）。ビヘイビア: SQL-44（`docs/spec/04-behavior/` のポインタのみ）。

## 受理形

`FROM <table>`、`FROM <table> AS <alias>`、`FROM <table> <alias>`（暗黙別名）。対象は単一テーブルの通常 SELECT・集計・`SELECT DISTINCT`。別名は `catalog::validate_identifier` を通し、予約語（JOIN 用の語に加え `UNION`／`INTERSECT`／`EXCEPT`）は別名にできない（`AS` 直後なら `42601`、暗黙別名位置なら別名として消費しない）。

## 設計

- 別名は parse 時に確定する情報で、`Parsed*Shape`／`Validated*` には持ち込まない。束縛・実行は実テーブル名のみで動くため、テナント境界（RLS・可視性）は従来どおり。
- `Parser::from_ref`（`Option<TableRef>`）が FROM 読了後に設定される。`None` の文脈（DML 述語・CHECK・VIEW 本体・CTE 本体）は挙動不変。集合演算の枝は既存の SELECT／集計パーサーを再利用するが、`Parser::reject_from_alias` により FROM 別名を `42601` で拒否する（`UNION` 枝の `FROM docs AS x` は従来どおり非対応のまま）。
- 解決規則は `sql::relation::TableRef::matches_qualifier`／`ensure_qualifier` に一本化し、JOIN の `BindingScope::resolve` と共有する。公開名は別名があれば別名、無ければテーブル名。
- 修飾子が公開名に一致しない場合は `42P01`。カタログを照会せず入力された修飾子のみを返すため、別テーブルの存在オラクルにならない（fail-closed）。

## 修飾列参照の受理（Issue #1535）

`sql::qualified_column::normalize_single_table_qualifiers` が、`validate_select_statement`（トップレベル SELECT・`EXPLAIN`・カーソル・`COPY (SELECT) TO`・サブクエリ内側の再検証）と CTE 主クエリの入口で、構文解析の前に 1 回だけ走る。FROM の公開名に一致する `Token::QualifiedIdent` を `Token::Ident(name)` へ 1 対 1 で置き換えるため、修飾あり・なしで AST・行・列名・エラーコードが一致する（トークン数不変）。

- スコープ: 先頭 `SELECT` と各 `( SELECT ...)`。直近のスコープの FROM 参照から照合する（内側優先）。FROM 参照の読み取りは `Parser::parse_from_table_ref` を試し解析に使い、別名規則を二重化しない。
- 書き換え区間: 投影と、FROM 参照以降〜`LIMIT`／`;`／`USING PLAN|MODE`／`HINT ORDER` の手前。関数名形（直後が `(`）・`AS` 直後・`::` 直後・`EXTRACT(` 直後は列参照ではないため残す。
- 不一致は `42P01`（カタログ非照会）。外側スコープの公開名に一致する修飾（相関）は `42601`。
- 不透明スコープ（FROM なし・スキーマ修飾 FROM・JOIN・カンマ結合・集合演算・深さ超過・解析不能）は書き換えず、従来のパーサー判定に委ねる。
- 適用しない経路: 集合演算の枝、JOIN（`BindingScope`）、VIEW 本体、CTE 定義本体、DML、INSERT、CHECK。
- content_hash は DML 台帳専用で SELECT 経路では計算しない。正規化は DML に適用しないため入力は不変。
- `x.id` は非修飾 `id` と同じ扱いになる。

## 判定順

正規化はパースより前に走るため、修飾子の不一致による `42P01` は後続の文法誤りによる `42601` より先に返りうる。非修飾の適用外経路（集合演算の枝等）では、WHERE 葉の先頭位置のみ `Parser::parse_where_leaf` のフックが同じ規則を担う。

## 申し送り

- AS 別名（`t.id AS i` 等）は #1537・#1540 の担当（現状は非修飾と同じ `42601`）。

## `t.*`（Issue #1536）

- lexer は空白を挟まない `t.*` を `Token::QualifiedStar` にする（予約語の修飾子・`t .*`・`a.b.*` は従来どおり字句エラー）。
- 正規化器は FROM の公開名に一致し、かつ選択リスト項目の位置（直前 `SELECT`／`,`／`DISTINCT`／`ALL`、直後 `FROM`／`,`）にある場合に限り `Punct('*')` へ 1 対 1 で置き換える。以降は `SELECT *` と同一の AST（`id` 疑似列が先頭）。
- 不一致の修飾子は `42P01`（外側スコープの公開名なら `42601`）。一致しても上記以外の位置（WHERE・ORDER BY・関数引数・演算子の隣）は乗算等への再解釈を避けるため `42601`。
- `t.*, col` の混在は非修飾の `*, col` と同じく未対応。JOIN・集合演算・DML・VIEW 本体・RETURNING は対象外（正規化されない `QualifiedStar` はパーサーが `42601` で拒否）。
- テスト: `crates/engine/tests/sql44_qualified_star.rs`。
