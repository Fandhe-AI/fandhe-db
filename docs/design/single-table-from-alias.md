# 単一テーブルの FROM 別名と修飾名の解決

対象: Issue #1534（親 #1533）。ビヘイビア: SQL-44（`docs/spec/04-behavior/` のポインタのみ）。

## 受理形

`FROM <table>`、`FROM <table> AS <alias>`、`FROM <table> <alias>`（暗黙別名）。対象は単一テーブルの通常 SELECT・集計・`SELECT DISTINCT`。別名は `catalog::validate_identifier` を通し、予約語（JOIN 用の語に加え `UNION`／`INTERSECT`／`EXCEPT`）は別名にできない（`AS` 直後なら `42601`、暗黙別名位置なら別名として消費しない）。

## 設計

- 別名は parse 時に確定する情報で、`Parsed*Shape`／`Validated*` には持ち込まない。束縛・実行は実テーブル名のみで動くため、テナント境界（RLS・可視性）は従来どおり。
- `Parser::from_ref`（`Option<TableRef>`）が FROM 読了後に設定される。`None` の文脈（DML 述語・CHECK・VIEW 本体・CTE 本体）は挙動不変。集合演算の枝は既存の SELECT／集計パーサーを再利用するが、`Parser::reject_from_alias` により FROM 別名を `42601` で拒否する（`UNION` 枝の `FROM docs AS x` は従来どおり非対応のまま）。
- 解決規則は `sql::relation::TableRef::matches_qualifier`／`ensure_qualifier` に一本化し、JOIN の `BindingScope::resolve` と共有する。公開名は別名があれば別名、無ければテーブル名。
- 修飾子が公開名に一致しない場合は `42P01`。カタログを照会せず入力された修飾子のみを返すため、別テーブルの存在オラクルにならない（fail-closed）。

## 判定順

SELECT リストは FROM より前に読まれるため、`SELECT t.id FROM t AS x` は従来どおり `42601`。WHERE 葉の先頭位置の修飾子不一致は、後続の文法誤りより先に `42P01` になりうる。

## 申し送り

- #1535: `Parser::parse_where_leaf` 先頭のフックで「一致時 `42601`」を「修飾子を剥がして非修飾列として処理」へ差し替え、投影・ORDER BY・GROUP BY の修飾も受理する。
- #1536: `t.*` と id 修飾。
