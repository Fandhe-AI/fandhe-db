# 集計文のスカラー `ORDER BY`・複数列 `DISTINCT`・非 `TEXT` の `GROUP BY` キー（Issue #1185）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-25 (a)(c)(d)・
  SQL-13・SQL-14・NOSQL-16 (b)・TASK-209・TASK-167
- 前提: `docs/design/multi-column-group-by.md`（複数列 `GROUP BY`）・
  `docs/design/scalar-order-by-scan.md`（広域取得のスカラー `ORDER BY`）
- 検証コード: `crates/engine/src/sql/order_value.rs`・`group_by.rs`・`allowlist.rs`・
  `parser.rs`、`crates/engine/tests/sql25_aggregate_order_by_scalar_keys.rs`、
  `crates/wire-server/tests/wire_sql25_distinct.rs`

## 背景・目的

集計文の受理範囲が PostgreSQL より狭い箇所が 3 つあった。

1. 集計文の `ORDER BY` は識別子 1 つだけ（複数キー・グループキーと集計値の混在不可）。
   `ORDER BY ... DESC` でも NULL を末尾に固定していた
2. `SELECT DISTINCT` は単一の `TEXT` 列だけ
3. `GROUP BY` キーは `TEXT` 列だけ（疑似列 `id` と `INTEGER` 等は `22000`）

本 Issue でこれらを是正する。RLS 境界・`wire_code` のエラー契約・fail-closed は変えない。

## 決定

### D1: 受け付けるキー型

`resolve_order_kind` が `Some` を返す型（`TEXT`・`INTEGER`・`BIGINT`・`REAL`・`DOUBLE`・
`BOOLEAN`・`NUMERIC`・`UUID`・`ENUM`・`DATE`・`TIMESTAMP`）と疑似列 `id`。スカラー
`ORDER BY` と同じ型集合にそろえる。`VECTOR`・`ARRAY`・`BYTEA`・`JSON`／`JSONB` と未知列は
従来どおり `22000`。`COUNT(DISTINCT)` は `BYTEA` を受け付けるが、キーとしては受け付けない
（`ORDER BY` と同じ型集合にそろえた結果の差）。

### D2: 集計 `ORDER BY` の形

`ORDER BY <識別子> [ASC|DESC] (, ...)*`。キー数の上限は `MAX_SCALAR_ORDER_KEYS`（8）で、
超過は `54000`。各識別子は既存の解決規則（`GROUP BY` 列名・そのエイリアス・集計項目の
実効名のうち一意に一致するもの。曖昧・未知は `22000`）を使う。式・位置番号・
`NULLS FIRST/LAST` は `42601` のまま。

### D3: NULL 位置（挙動変更）

明示した `ORDER BY` は広域取得と同じ PostgreSQL 既定（ASC は NULL 末尾、DESC は NULL 先頭）
にそろえる。**`ORDER BY` を書かないときの既定順（キー昇順・NULL 末尾）と ASC の順序は
変えない**。従来は `DESC` でも NULL を末尾に固定していたため、`DESC` と `LIMIT` を併用した
既存クエリは NULL グループが先頭に来る。

### D4: 同順位の扱い

指定したキーがすべて同値なら、グループキー全体の昇順で決める（集計行には `id` がないため、
広域取得の「次いで `id` 昇順」の代わり）。ソートは安定ソート（`sort_by`）で、決定的。

### D5: 比較器の共有

`sql/order_value.rs`（新設）へ、広域取得のスカラー `ORDER BY` が持っていた比較値
（`OrderValue`・借用版 `ScalarKeyRef`）・比較器・行値抽出を移し、`sql::scan` と
`sql::group_by` が共有する。グループキーの成分は `Option<OrderValue>`。NaN 同士と
±0 は等価に扱い、`COUNT(DISTINCT)` の正準化と同じ同値契約を満たす。

### D6: 実行経路

単一の `TEXT` キーは既存経路（`string_groups`／`null_group`・索引の列挙形・候補走査形）を
そのまま使う。それ以外（複数キー・非 `TEXT` キー・疑似列 `id`）は型付きグループキーの全走査
（`multi_groups`）へ回し、索引スナップショットの構築も試みない。判定は
`group_by::single_text_key_column` に一本化し、EXPLAIN の静的判定
（`classify_aggregate_access`）も同じ関数を使う（実行経路との食い違いを防ぐ）。

### D7: 出力セル

グループキー列のメタデータは `ColumnMeta::Computed` のまま（`describe::aggregate_columns` との
一致を保つ。型 OID の是正は別 Issue の管轄）。セルは列型から復元し、広域取得の投影と同じ
variant を使う（`INTEGER`／`BIGINT` は `SignedInteger`、`REAL`／`DOUBLE` は `Float`、
`ENUM` はラベルの `Text`、`id` は `Integer` 等）。値と列型の不一致・`DATE` の範囲外は
panic ではなく `XX000` で fail-closed にする。

### D8: `SELECT DISTINCT` の複数列

`SELECT DISTINCT a [AS x], b ...`（1〜`MAX_GROUP_BY_COLUMNS`＝8 列、超過は `54000`）。
同じ列の重複記述（`DISTINCT a, a`）は PostgreSQL が受理する形のため、`GROUP BY` 側の列は
重複を除き、SELECT リスト側の項目だけを列挙する。`*`・式・集計項目との混在は `42601`。

### D9: NoSQL 表層への波及

`resolve_group_by_key` は公開 API `BoundAggregate::new_grouped_by_columns` と共有するため、
NoSQL `aggregate` の `group_by` も同じ型集合を受け付ける（SQL-25 (d) との写像を維持）。
公開シグネチャは変えない。

## セキュリティ上の考慮

- キーの抽出は、既存の「ヘッダで可視性判定 → 本体デコード → `WHERE` → 可視性の再検査」の
  後にだけ行う。走査順と RLS 適用位置は変えない。不可視行の非 `TEXT` キー値は、結果・件数・
  並び順に現れない（結合テストで固定）
- グループ数・キー累計バイト・結果予算・列数・`ORDER BY` キー数の上限は、確保より前に検査する。
  非 `TEXT` 成分もキー予算へ固定見積りで計上する
- 新しい SQLSTATE は追加しない（`42601`／`22000`／`54000`／`XX000` の既存の割り当て）

## 対象外（後続課題）

- `HAVING` でグループキーを比較すること、`ORDER BY`／`HAVING` の式位置
- 集計結果・グループキーの型 OID の是正（列メタデータは `Computed` のまま）
- `BYTEA`／`JSON`／`JSONB`／`ARRAY` のグループキー化、`SELECT DISTINCT ... OFFSET`、
  `GROUP BY` なしの単一行集計への `ORDER BY`
- 非 `TEXT` キー・複数キーの索引経路（候補走査形）による高速化
