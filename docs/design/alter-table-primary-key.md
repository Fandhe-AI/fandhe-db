# ALTER TABLE による PRIMARY KEY の追加・削除（Issue #1196・#1364）

ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-22 (a)(d)・TABLE-12・TABLE-16・
TABLE-17、`rls.md` RLS-9・RLS-10 (c)、`sql-surface.md` SQL-23・SQL-31、
`error-format.md` ERR-6、`05-tasks.md` TASK-233。spec 本文は転記しない。

## 受理する形

- `ALTER TABLE <t> ADD [CONSTRAINT <name>] PRIMARY KEY (<col>[, <col>]*)`（名前付きは Issue #1364）
- `ALTER TABLE <t> DROP CONSTRAINT <実効名>`（主キー宣言済みのテーブルのみ。実効名は明示名、無ければ `<t>_pkey`）

構造段で `42601` とする形: `id` を含む
列リスト（`id` は暗黙の主キーで、見せかけの成功を作らない）、空リスト。列の重複は
`42701`、`MAX_PRIMARY_KEY_COLUMNS` 超過は `54000`。いずれもカタログを参照しない。

## 導出擬似名と明示名（Issue #1364）

名前を明示しない PRIMARY KEY はカタログに名前を永続化しない。`catalog::primary_key_constraint_name` が
`<table>_pkey`（識別子長上限を超える場合はテーブル名を切り詰め）を返す純関数で、
`DROP CONSTRAINT` の名前解決と `ADD PRIMARY KEY` の応答に使う。

明示名は `TableSchema::primary_key_name` に持ち、導出名と同じ名前は `None` へ正規化する
（無名と同じバイト列・同じ意味。正規形が一意）。実効名は
`TableSchema::primary_key_constraint_name_effective`。主キーの名前は UNIQUE・CHECK・FOREIGN KEY と
同じテーブル単位の名前空間を共有する。明示名がある間は導出名 `<t>_pkey` は解放され、
`DROP CONSTRAINT <t>_pkey` は `42704`、他制約の明示名として使える。主キーを削除すると明示名も消える。
既定名の導出（UNIQUE・CHECK・FOREIGN KEY）は、主キーの明示名だけを衝突回避の対象に加える
（導出名は加えない。v12 以下のバイト列を変えないため）。

### カタログ v13

導出名以外の明示名を持つ主キーだけが v13 で永続化される。v12 の本体に、`pk:` 行の直後の
`pkname:<name>` 行を足した上位集合で、`uniq:`／`checks:`／`fks:` は 0 件を許す。
`pkname` は識別子として妥当・主キー宣言を伴う・導出名と異なる・他制約名と衝突しない、を
`decode_schema_body` と軽量パーサー（ENUM 依存判定）の両方が同じ共有パーサーで検査する。
v13 は FK 保持版として `FK_BEARING_FORMAT_VERSIONS` に登録済み（漏れると `DROP TABLE` の `2BP01` が
fail-open になる）。旧バイナリは v13 を「未知のバージョン」として fail-closed に拒否する
（前方互換なし）。

- 名前解決の順序: UNIQUE → FOREIGN KEY → CHECK → 主キー擬似名 → `42704`。
- 実効名と同名の UNIQUE・CHECK・FOREIGN KEY がある表への ADD PRIMARY KEY（無名は導出名、
  名前付きは明示名で判定）、および主キー宣言済みの表への実効名と同名の UNIQUE の明示名 ADD は
  `42P07`（CHECK・FOREIGN KEY の明示名は `42710`）。`validate_schema` には入れない
  （既存カタログの decode を壊さないため）。
- 既知のギャップ: `CREATE TABLE` で既に擬似名と同名の制約と主キーが併存する表は救済しない
  （名前解決が実名を先に探すため挙動は決定的）。

## ADD PRIMARY KEY の判定順序（単一 write txn・fail-closed）

権限（`42501`。カタログ参照より前）→ テーブル存在（`42P01`／`42809`）→ PK 宣言済み
（`42P16`。名前衝突より先）→ 確定名の衝突（`42P07`）→ 追加後スキーマの検証（未知の列・PK 不可型は
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

## エラー契約（ERR-4・ERR-6 ポインタ。`42P16` は Issue #1364 で追加）

| 条件 | wire_code |
| --- | --- |
| DDL 権限なし | 42501 |
| 明示トランザクション内 | 0A000 |
| 構造段の拒否 | 42601／42701／54000 |
| テーブル無し／ビュー | 42P01／42809 |
| PK 宣言済み（無名・名前付きとも。`ErrorClass::InvalidTableDefinition`・HTTP 400） | 42P16 |
| 未知の列・PK 不可型 | 42601 |
| 確定名（明示名・導出名）の衝突 | 42P07 |
| 既存行に NULL | 23502 |
| 既存行に重複 | 23505 |
| DROP 対象なし | 42704 |
| DROP 対象の PK を FK が参照 | 2BP01 |

## 公開 API の変更（BREAKING CHANGE）

`ValidatedAlterTable::AddPrimaryKey`・`CatalogError::NotNullConstraintViolation` を追加した。
`Storage::alter_table_add_primary_key`・`catalog::primary_key_constraint_name` を新設。
`AlterTableAction` の variant は増やさない（ADD は `AddConstraint`、DROP は `DropConstraint`）。

Issue #1364 で `ErrorClass::InvalidTableDefinition`・`SqlSurfaceError::InvalidTableDefinition`・
`CatalogError::MultiplePrimaryKeys` を追加し、`ValidatedAlterTableAddPrimaryKey` に
`constraint_name` を足し、`Storage::alter_table_add_named_primary_key` を新設した
（`alter_table_add_primary_key` は名前なしの薄いラッパ）。

## 申し送り（Issue は起票しない）

- 解消済み（Issue #1412）: `CREATE TABLE` 内の `CONSTRAINT <name> PRIMARY KEY` と、
  主キーの二重宣言（`42P16`）。
- 主キー削除後に NOT NULL を外す手段（`ALTER COLUMN ... DROP NOT NULL`）は未対応。
- NoSQL（HTTP）表層からの追加・削除は未対応。
