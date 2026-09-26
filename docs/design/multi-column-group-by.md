# 複数列 `GROUP BY`（Issue #918）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-14
  （`GROUP BY`/`HAVING`）・SQL-25 (d)（複数列グループキー）・NOSQL-5・
  NOSQL-16 (b)・TASK-167・TASK-177
- 前提: TASK-167・SQL-14（単一列 `GROUP BY` の実装。`crates/engine/src/sql/group_by.rs`）

## 背景・目的

`GROUP BY` は `TEXT` 列 1 つのグループキーに固定されていた（構文層
`Parser::parse_group_by_clause` が識別子 1 つのみ受理、束縛層
`BoundGroupBy::column_index: usize`、実行層 `GroupKey(Option<String>)`）。
本 Issue で SQL-25 (d) に沿い、グループキーを複数列の組（タプル）へ拡張する。

## 決定

### D1: グループキーの表現

構文層 `GroupByClause.columns: Vec<String>`（宣言順保持）、束縛層
`BoundGroupBy.column_indices: Vec<usize>`、実行層 `GroupKey(Vec<Option<String>>)`
へ一般化する。`Ord` は成分ごとの辞書式比較（各成分は `Some` 同士ならバイト順、
`Some` は常に `None` より小さい＝NULL は末尾）。単一成分では旧
`GroupKey(Option<String>)` と完全に同じ順序になるため、単一列クエリの既定順序・
`ORDER BY` 未指定時の挙動は変わらない。

### D2: 列数上限

`MAX_GROUP_BY_COLUMNS = 8`（`sql::allowlist::check_group_by_column_count`。
`54000`）。重複する列名は構文層で `42601` として拒否する（spec に規定が無いため
fail-closed に倒す）。既存の `MAX_GROUPS`（10,000 グループ）・
`MAX_GROUP_KEY_TOTAL_BYTES`（16 MiB）は列数によらず据え置く。

### D3: 実行経路 — 複数列は全走査のみ

`ScalarIndex::column_groups`／`resolve_candidates` 経由の索引経路（列挙形・
候補走査形）は単一キー専用の契約のまま変更しない。`column_indices.len() >= 2`
の場合は索引スナップショットの構築自体を試みず（cold cache で使わない索引を
構築しない。既存の `text_min_max_blocks_enumeration` 分岐と同じ判断）、
`user_rows/{table}` の全走査へ一本化する。単一列クエリの実行経路（列挙形・
候補走査形・全走査のいずれも）は変更しない。複数列の索引候補走査への拡張は
性能最適化の後続課題とする。

### D4: SELECT リスト・`HAVING`／`ORDER BY` の対象解決

`ProjectionColumn::GroupKey { key_index, name }`・`OrderTarget::GroupKey(usize)`
で「どのキー列か」を保持する。`HAVING`／`ORDER BY` の対象名は、いずれかの
`GROUP BY` 列名そのもの、その列に SELECT リストで付けたエイリアス、または
集計項目の実効名のいずれか 1 つに一意に解決する（複数キーへ一致する識別子・
キーと集計項目の双方へ一致する識別子はいずれも曖昧として `22000`）。

### D5: 公開 API

`BoundAggregate::new_grouped_by_columns`（NoSQL 表層・その他クレート外からの
直接構築用）を新設し、既存の `new_grouped(&str)` は
`new_grouped_by_columns(&[col], ...)` への委譲にする（挙動・エラー分類は不変。
破壊的変更にしない）。`allowlist::MAX_GROUP_BY_COLUMNS`・
`check_group_by_column_count` も同様に公開する。

## 対象外（後続課題）

- NoSQL 表層で `group_by` の配列形（複数列）を受理すること（NOSQL-16 (b)。
  別 Issue。`new_grouped_by_columns`／`check_group_by_column_count` を使えば
  写像できる）
- 複数列 `GROUP BY` での索引候補走査形（`resolve_candidates`）の利用（D3）
- `TEXT` 以外の列（整数・日時等）をグループキーにすること
