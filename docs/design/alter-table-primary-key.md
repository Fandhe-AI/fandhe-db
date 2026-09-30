# ALTER TABLE による PRIMARY KEY の追加・削除（Issue #1196）

ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-22 (a)(d)・TABLE-12・TABLE-16・
TABLE-17、`rls.md` RLS-9・RLS-10 (c)、`sql-surface.md` SQL-23・SQL-31、
`error-format.md` ERR-6、`05-tasks.md` TASK-233。spec 本文は転記しない。

## 受理する形

- `ALTER TABLE <t> ADD PRIMARY KEY (<col>[, <col>]*)`
- `ALTER TABLE <t> DROP CONSTRAINT <t>_pkey`（主キー宣言済みのテーブルのみ）

構造段で `42601` とする形: `ADD CONSTRAINT <name> PRIMARY KEY`（PK は無名）、`id` を含む
列リスト（`id` は暗黙の主キーで、見せかけの成功を作らない）、空リスト。列の重複は
`42701`、`MAX_PRIMARY_KEY_COLUMNS` 超過は `54000`。いずれもカタログを参照しない。

## 導出擬似名

PRIMARY KEY はカタログに名前を永続化しない。`catalog::primary_key_constraint_name` が
`<table>_pkey`（識別子長上限を超える場合はテーブル名を切り詰め）を返す純関数で、
`DROP CONSTRAINT` の名前解決と `ADD PRIMARY KEY` の応答にのみ使う。

- 名前解決の順序: UNIQUE → FOREIGN KEY → CHECK → 主キー擬似名 → `42704`。
- 擬似名と同名の UNIQUE・CHECK・FOREIGN KEY がある表への ADD PRIMARY KEY、および主キー
  宣言済みの表への同名の明示名 ADD は `42P07`。`validate_schema` には入れない
  （既存カタログの decode を壊さないため）。
- 既知のギャップ: `CREATE TABLE` で既に擬似名と同名の制約と主キーが併存する表は救済しない
  （名前解決が実名を先に探すため挙動は決定的）。

## ADD PRIMARY KEY の判定順序（単一 write txn・fail-closed）

権限（`42501`。カタログ参照より前）→ テーブル存在（`42P01`／`42809`）→ PK 宣言済み
（`42601`）→ 擬似名衝突（`42P07`）→ 追加後スキーマの検証（未知の列・PK 不可型は
`42601`）→ 既存行の NULL 検査（`23502`）→ 既存行の重複検査（`23505`）→ カタログ更新・
永続一意索引の無効化・世代 bump・commit。拒否はすべて commit せず破棄する（副作用ゼロ）。

- 既存行は**全テナント・Public／Private を問わず**走査する（DDL は `PolicyContext` を
  取らない共有資源操作。可視性で母集合を縮めると不可視行の違反を見逃す）。
- 一意性はテナントごとに判定する（テナントを跨いだ同値は違反ではない。TABLE-12・RLS-10 (c)）。
- NULL 検査は**変更前スキーマ**で走査する。`ADD COLUMN`（DEFAULT なし）より前の行は列の
  バイトが欠落しており、nullable の場合に限り NULL として読める。先に NOT NULL へ書き換えると
  decode エラー（`XX000`）になり `23502` を検出できない。DEFAULT 付き列の欠落は既定値。
- NULL と重複が併存する場合は行順序に関係なく常に `23502` が優先される。
- エラー文言に含めるのは列名（スキーマ情報）のみ。テナント・行・値・件数は含めない。
  `23505` は既存の固定文言で、ADD UNIQUE と同じく権限ゲート（`42501`）が存在オラクルを防ぐ。
- 既存行の走査に件数上限は設けない（ADD UNIQUE／CHECK と同じ。単一ライタを保持する DDL）。

## DROP CONSTRAINT <主キー> の判定

- 主キー未宣言のテーブル（暗黙の `id` 主キー）は削除できず `42704`。
- FOREIGN KEY（自己参照を含む）の `parent_columns` の集合が主キーの列集合と一致すれば
  `2BP01`。同じ集合を UNIQUE が覆っていても救済しない（UNIQUE 削除時と対称の fail-closed）。
  `REFERENCES parent`（列リスト省略）で主キーへ解決済みの FK も検出する。`id` 参照の FK は
  主キーと無関係なので対象外。
- 主キー構成列の NOT NULL は戻さない（緩める方向は fail-open になりやすい。PostgreSQL も
  NOT NULL を残す）。
- 永続一意索引を無効化し、キー索引の衛生処理と世代 bump を同じ txn で行う。

## エラー契約（ERR-6 の既存行のみ・新しい `wire_code` なし）

| 条件 | wire_code |
| --- | --- |
| DDL 権限なし | 42501 |
| 明示トランザクション内 | 0A000 |
| 構造段の拒否 | 42601／42701／54000 |
| テーブル無し／ビュー | 42P01／42809 |
| PK 宣言済み・未知の列・PK 不可型 | 42601 |
| 擬似名の衝突 | 42P07 |
| 既存行に NULL | 23502 |
| 既存行に重複 | 23505 |
| DROP 対象なし | 42704 |
| DROP 対象の PK を FK が参照 | 2BP01 |

## 公開 API の変更（BREAKING CHANGE）

`ValidatedAlterTable::AddPrimaryKey`・`CatalogError::NotNullConstraintViolation` を追加した。
`Storage::alter_table_add_primary_key`・`catalog::primary_key_constraint_name` を新設。
`AlterTableAction` の variant は増やさない（ADD は `AddConstraint`、DROP は `DropConstraint`）。

## 申し送り（Issue は起票しない）

- 主キーの重複宣言は PostgreSQL では `42P16` だが ERR-6 に無いため `42601` とした。
- 主キー削除後に NOT NULL を外す手段（`ALTER COLUMN ... DROP NOT NULL`）は未対応。
- `ADD CONSTRAINT <name> PRIMARY KEY`・NoSQL（HTTP）表層からの追加・削除は未対応。
