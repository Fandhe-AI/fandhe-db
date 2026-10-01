# SQL 表層での同じ列への等価 OR の IN フィルタ書き換え

Issue #1305。ポインタ: TASK-208・SQL-24・SQL-2・RLS-10（spec 本文は転記しない）。
計測の経緯は [relational-p95-bench.md](./relational-p95-bench.md)（Issue #1275）の改善案 A。

## 背景

`WHERE lang = 'a' OR lang = 'b'` は OR 群として束縛され、`classify_scalar_plan` が
一律 `PlainScan` に落としていた。可視行全件を評価して一致行の embedding を複製するため、
同じ選択率の `IN` より約 20 倍遅かった。

## 決定

WHERE のトップレベルで、全分岐が「同じ TEXT／ENUM 列への等価（または `IN`）1 件だけ」の
OR 群を、束縛時に 1 本の `InText` メタデータフィルタへ書き換える
（`sql::where_tree::fold_same_column_text_or_groups`、判定は `BoundOrGroup::as_same_column_text_in`、
構築は `MetadataFilter::from_bound_text_in`）。`EXPLAIN` の `scalar_plan` は `index_in_list` になる。
分類ロジック（`classify_scalar_plan`）は変えない。束縛結果が変わるだけで、実行・`EXPLAIN`・
集計・GROUP BY の判定が同じ入力から同時に追従する。

## 書き換える条件（すべて満たすとき）

1. WHERE のトップレベルの OR 群であること
2. 分岐が 2 個以上
3. 全分岐が、メタデータフィルタ 1 件のみ（式述語・入れ子の OR を持たない）で、
   op が `Equals` または `InText`
4. 全分岐が同じ列
5. 統合後（sort・dedup 後）の値が 1 件以上 256 件（`MAX_IN_LIST_ITEMS`）以下。
   合計は `Vec` を伸ばす前に `checked_add` で判定する
6. エラー順序のゲート: トップレベルに式述語が無く、畳まずに残る OR 群も式述語を含まない
7. メタデータフィルタ数が `MAX_METADATA_FILTERS` 未満

満たさない群は従来どおり OR のまま残り `PlainScan` に縮退する。新しいエラーは導入しない。
束縛時のエラー（`22P02`・未知列等）は分岐の束縛後に畳むため従来と同じ。

## エラー順序のゲートの理由

式述語は実行時にエラー（`22012` 等）を返し得るが、メタデータフィルタは返さない。
評価順は metadata、式、OR 群の順のため、畳むと OR 群が式述語より前に評価され、
従来は式のエラーになっていた文が空結果になり得る。これを避けるため、式述語を含む文は畳まない。

## 適用範囲

- SQL WHERE のトップレベル（`sql::parser::bind_where_predicates`）。
- NoSQL（HTTP）`filter` のトップレベルの `or`（Issue #1306。`sql::declarative_predicate::bind_declarative_predicates`）。
  同じ関数を共有し、入れ子の `or`（分岐内の `or`）・式述語の併用などの対象外条件も SQL と同じ。

## 対象外とその理由

| 経路 | 理由 |
| ---- | ---- |
| JOIN の残余 | 1 分岐の `Or` で包んで束縛され（条件 2 で除外）、索引も参照しないため効果が無い |
| `IN (SELECT ...)` | 1 チャンクでも 1 分岐の `Or` で包む。2 チャンク以上は先頭が 256 件で上限超過。計画形状は不変 |
| CHECK | 別の入口（`bind_check_predicates`）。永続化・再オープン時の再検証経路を変えない |
| 型付き等価（DATE 等）・BOOLEAN | `InTyped` は索引非対応で効果が無い |
| TEXT の範囲比較・LIKE・IS NULL・NOT | 等価でないため畳めない |
| 入れ子の OR | 索引の効果が無い |

## HINT ORDER

`HINT ORDER` で DISTANCE が先行する場合は `scalar_prefilter=false` のため、書き換えは適用されても
`scalar_plan` は `plain_scan` のまま。事後フィルタはメタデータフィルタで評価され、結果は同じ。

## NULL

`Equals` と `InText` はどちらも辞書文字列で比べ、NULL・型不一致は UNKNOWN（不一致）になる。
書き換えの前後で NULL 行の扱いは同じ。

## 検証

`tests/sql_where_or_in_rewrite.rs`（EXPLAIN・経路カウンタ・全経路の結果同値・RLS・上限・縮退・
エラー同値・HINT ORDER・ENUM・Describe）、単体テスト（`where_tree`・`declarative_filter`）、
ベンチの経路期待値の固定（`relational_p95_accept`）。
