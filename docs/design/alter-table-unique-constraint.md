# ALTER TABLE ADD／DROP CONSTRAINT UNIQUE と制約名（TABLE-16・TASK-204・TASK-205）

- **Issue**: #1067（`feat(engine)!: ALTER TABLE ADD／DROP CONSTRAINT UNIQUE と制約名`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/05-tasks.md`
  TASK-204・TASK-205・`docs/spec/04-behavior/data-model.md` TABLE-16・
  TABLE-17・`docs/spec/04-behavior/sql-surface.md` SQL-23・SQL-31・
  `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
  `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4・ERR-6
- **関連**: `docs/design/unique-constraint.md`（Issue #905。一意性検査の単一
  検査点・既存 Rust API）・`docs/design/check-constraint.md`（既定名の接尾辞
  衝突解決規則を共有）・`docs/design/foreign-key.md`（Issue #907。DROP 時の
  依存検査 D7）
- **ステータス**: 実装済み（本ドキュメントが範囲を確定する）

## 背景・目的

UNIQUE 制約（Issue #905）は `CREATE TABLE` と Rust API
`Storage::alter_table_add_unique_constraint` からしか宣言できず、名前も持たない
（カタログの `U:` 行は列リストのみ）。本 Issue は SQL `ALTER TABLE ... ADD
[CONSTRAINT <name>] UNIQUE (...)` / `DROP CONSTRAINT <name>` と、それを支える
制約名の永続化を追加する。

## 設計判断

### D1. 制約名の名前空間

テーブル単位の名前空間とし、そのテーブルの UNIQUE 制約名と CHECK 制約名で共有
する（`validate_schema` が UNIQUE∪CHECK の重複を検査する）。テーブル・ビュー・
索引の relation 名前空間とは共有しない（UNIQUE は永続索引を作らない）。
PRIMARY KEY・FOREIGN KEY は引き続き名前を持たない——`DROP CONSTRAINT
<t>_pkey` のような指定は「存在しない」＝`42704` になる。名前の比較は既存の
識別子と同じく厳密一致・`catalog::validate_identifier` で検証する。

### D2. 既定名の導出（純関数）

名前を付けずに宣言された UNIQUE 制約の名前は、テーブル名・宣言順の UNIQUE 列
リスト・CHECK 名の集合だけから決まる純関数で導出する
（`catalog::derive_unique_constraint_names`）。候補名は
`<table>_<col1>_<col2>..._key`（PostgreSQL 風）。候補が識別子として不正、また
は既に使われていれば `_2`・`_3`… の接尾辞を試し、それでも決まらなければ
`key<N>` へフォールバックする——共通ヘルパ `catalog::resolve_constraint_name`
（接尾辞と `<fallback_prefix><N>` フォールバック）に集約し、
`sql::check_constraint::resolve_unique_name`（フォールバック接頭辞
`"check"`）と同じ実装を共有する（既存の CHECK 名の既定名は 1 文字も変わらな
い。回帰テストで固定）。

名前の確定はカタログ層で行う: `Storage::create_table` と ALTER 用メソッドの
write トランザクション内で、`encode_schema` の内部（＝ `validate_schema` の
直前）に集約する（`catalog::assign_unique_constraint_names`。空名にのみ作用
する冪等関数）。`sql::allowlist` は名前未確定（空名。`UniqueConstraint::new`）
の制約を組み立てるだけで、`validate_schema` は空名を拒否する（確定処理が
漏れた経路は encode 時点で fail-closed に落ちる）。

### D3. カタログ v9（名前付き UNIQUE の永続化）

v9 は v8 の上位集合（`cols:` → `pk:` → 6 フィールド列行 → `uniq:<n>`〔v9 は
`n >= 1` 必須〕→ `n` 個の `U:<name>:<col>[,<col>]*` → `checks:<m>`〔0 件可〕→
`fks:<k>`〔v9 に限り 0 件可〕）。

**v9 で書くのは、少なくとも 1 つの UNIQUE 制約の実名が、D2 の導出（全 UNIQUE を
名前未指定とみなして導出した名前）と一致しない場合だけ**（`encode_schema`）。
それ以外は従来どおり v2〜v8 のバイト列のまま——名前指定なしの `CREATE
TABLE`／ALTER ADD は導出と一致するため、既存のゴールデンテスト（v2〜v8 の
固定バイト列アサーション）はすべて無変更で通る。名前の無い旧 v6〜v8 値は
decode 時に D2 で名前を導出する。明示名を付けた場合や、DROP で後続の制約の
導出名がずれた場合のみ v9 で実名を保存し、名前が暗黙に変わることはない。

v9 を知らない旧バイナリは「未知のフォーマットバージョン」として fail-closed
に拒否する。

**v9 追加で更新した箇所**（全部必須。1 箇所でも漏れると fail-open になり得る）:

- `CATALOG_FORMAT_VERSION_V9` 定数
- `encode_schema`（実名 vs 導出名の一致判定 → v9 分岐）
- `FormatVersion` と `decode_schema_body`（`pk:`・6 フィールド・`uniq:`〔名前
  付き〕・`checks:`〔0 可〕・`fks:`〔0 可〕）
- `parse_unique_section`（`named: bool` 引数を追加し `Vec<(Option<String>,
  Vec<String>)>` を返す。名前の識別子検証・セクション内の名前重複拒否）
- `parse_foreign_key_section`（`allow_empty: bool` 引数を追加。v9 のみ `k == 0`
  を許容）
- `catalog_value_references_enum_type`（軽量パーサー。バージョン表に v9 を
  追加し `is_v9` を各分岐へ反映）
- **`referencing_foreign_keys_in_txn`**（`v8_prefix` に加え **`v9_prefix` も
  候補にする**。v9 を見落とすと、FK を持つテーブルが `DROP TABLE` の `2BP01`
  判定と、参照先側の書き込み検査から消える fail-open になる）

### D4. エラー契約（ERR-6 の既存行だけを使い、新しい wire_code は作らない）

| 条件 | `CatalogError` | `wire_code` |
| --- | --- | --- |
| DDL 権限なし | ― | `42501`（カタログ照会より前） |
| 明示トランザクション内 | ― | `0A000`（既存 catch-all） |
| 構文が許可リスト外 | ― | `42601` |
| UNIQUE 列リストが空／未宣言列参照 | ― | `42601`（構文段・カタログ非参照） |
| 同一制約内の列重複 | ― | `42701` |
| 1 制約あたりの列数上限を超過 | ― | `54000` |
| テーブルが無い／ビュー・索引名を指定 | `TableNotFound` | `42P01`／`42809` |
| 列が無い／型が一意キーに使えない／同一列リストの制約が既にある | `Invalid`（`validate_schema` 経由。既存 Issue #905 由来のロジックを継続） | `42601` |
| **制約名の衝突**（UNIQUE または CHECK と同名） | `ConstraintAlreadyExists` | `42P07`（索引名衝突と同じ既存行を流用） |
| テーブルあたり制約数の上限（`MAX_UNIQUE_CONSTRAINTS` = 32）を超過 | `ConstraintLimitExceeded` | `54000` |
| 既存行に重複あり | `UniqueConstraintViolation`（既存） | `23505` |
| DROP する名前が存在しない | `ConstraintNotFound` | `42704` |
| DROP する名前が CHECK 制約 | `ConstraintDropNotSupported` | `0A000`（CHECK の削除はスコープ外） |
| DROP する UNIQUE を FK が参照している | `DependentObjectsStillExist`（既存） | `2BP01` |
| 書き込みゲートの待機上限超過 | `WriteLockTimeout` | `55P03` |
| その他 | ― | `XX000` |

補足: PostgreSQL が制約名衝突に使う `42710` の行が ERR-6 の表に無いため、本
実装は `42P07` を流用した。`42710` の追加要否は spec リポ側の課題として申し
送る（本実装では対処しない）。

HTTP 射影は新しい `ErrorClass` を追加していないため、`error_format.rs` と
`crates/wire-server/tests/err4_http_projection.rs` は無変更で受入基準
「HTTP 射影も整合」を満たす。

### D5. 公開 API の変更（BREAKING CHANGE）

1. `catalog::UniqueConstraint` に非公開 `name: String` を追加し、`name()` を
   公開。構築は `new(columns)`（名前未確定）と `with_name(name, columns)` の
   2 経路
2. `catalog::CatalogError` に `ConstraintAlreadyExists`・`ConstraintNotFound`・
   `ConstraintDropNotSupported`・`ConstraintLimitExceeded` を追加（`#[non_exhaustive]`
   は付与しない。`docs/design/error-enum-non-exhaustive-policy.md` の方針を継続）
3. `sql::allowlist::validate_alter_table`／`validate_alter_table_tokens` の
   戻り値を `ValidatedAlterTableAddColumn` から `ValidatedAlterTable`
   （`AddColumn`／`AddUnique`／`DropConstraint` の 3 variant の enum）へ変更
4. `core::ParsedSql::AlterTable` の中身を `ValidatedAlterTable` に変更
5. `sql::ddl::AlterTableOutcome` を `{ table_name, action: AlterTableAction }`
   に変更（`AlterTableAction::{AddColumn, AddConstraint, DropConstraint}`）。
   `AddConstraint` は確定後の名前（自動生成名を含む）を返す——カタログに
   `pg_constraint` 相当の照会手段が無いため、呼び出し元が省略時の既定名を知る
   唯一の手段。`SqlOutcome::AlterTable(AlterTableOutcome)` 自体は据え置くため
   `wire-server/src/simple_query.rs` の `CommandComplete` タグ写像は無変更
6. Rust API: 既存 `Storage::alter_table_add_unique_constraint` はシグネチャを
   据え置き、新設の `alter_table_add_named_unique_constraint(table, name:
   Option<&str>, columns) -> Result<String>`（確定名を返す）へ委譲。新設
   `alter_table_drop_constraint(table, name) -> Result<()>` を追加

### D6. スコープの境界（fail-closed 側に倒す）

対象外（`42601` のまま）: `CREATE TABLE` での `CONSTRAINT <name> UNIQUE`
（`CREATE TABLE` に明示制約名を持つ UNIQUE を書く構文は未対応）・
`ADD CONSTRAINT ... CHECK/PRIMARY KEY/FOREIGN KEY`・
`DROP CONSTRAINT IF EXISTS`／`CASCADE`／`RESTRICT`・1 文に複数の ADD／DROP・
`DROP COLUMN` の SQL 公開。CHECK 制約の DROP は `0A000`。同一列リストの
UNIQUE 重複は従来どおり拒否（`42601`）。名前を調べる SQL の手段
（`pg_constraint`／`information_schema`）は存在しない。自動生成名は (a) D2 の
規則（本ドキュメント）と (b) ALTER ADD の成功応答（`AlterTableAction::
AddConstraint`）で知る設計とする。

### D7. 判定順（決定的・fail-closed。データに依存するのは最後の `23505` だけ）

**ADD**: 構文検証 → `require_ddl_permission`（`42501`）→ テーブル存在確認
（`42P01`／`42809`）→ write txn 内で: スキーマ再取得 → 制約名の衝突
（`42P07`）→ 件数上限（`54000`）→ 名前確定後のスキーマで `validate_schema`
（列が無い／型不適格／重複を含めすべて `42601`）→
`constraint::table_has_duplicate_unique_key` で全行走査（`23505`。重複時は
commit せず破棄・副作用ゼロ）→ `encode_schema` → カタログへ挿入 → 世代 bump
→ commit。

**DROP**: 構文検証 → `42501` → 存在確認 → write txn 内で: 名前の検索
（UNIQUE にあれば削除対象。CHECK にあれば `0A000`。どちらにも無ければ
`42704`）→ FK 依存の検査（`referencing_foreign_keys_in_txn`。自己参照を
含む。`parent_columns` の**集合**が削除対象の列集合と一致するものが 1 件で
もあれば `2BP01`。主キーや他の UNIQUE が同じ集合を覆っていても救済せず拒否
する——fail-closed）→ `encode_schema` → 世代 bump → commit。

テナント境界について: DDL はテナント軸とは別の運用者レベルの権限で行う
共有カタログ操作であり、既存の Rust API と同じく全テナントを走査する。
`23505` の応答は固定文言でテナント名・値・行 id・件数を含まない（ただし
「どこかのテナントに重複がある」という 1 ビットは原理的に伝わる。これは
権限モデル〔`42501` ゲートで運用者に限定〕で扱う）。

## 検証

- `crates/engine/tests/sql_alter_table_unique_constraint.rs`: SQL 経由の
  ADD（既定名・明示名・複合列）・既存行の重複拒否（副作用ゼロ）・名前衝突
  （UNIQUE・CHECK 双方との）・DROP（成功・未検出・CHECK 名指定・PRIMARY KEY
  慣習名）・FK 依存（`2BP01`。参照されていない UNIQUE は削除できることも
  確認）・DDL 権限（存在オラクルにならないこと）・スコープ外構文の拒否・
  明示トランザクション内の `0A000`・世代 bump による即時反映
- `crates/engine/src/catalog.rs` 単体テスト: v9 の往復（既存の v2〜v8
  ゴールデンテストは無変更のまま全通過。`assign_unique_constraint_names`
  適用後の比較で検証）
- `crates/engine/tests/table17_foreign_key.rs`・`unique_constraint.rs`: 既存
  回帰がすべて無変更で通ることを確認済み

## 申し送り（Issue は起票しない）

- spec 側の課題: 制約名衝突に使う SQLSTATE（PostgreSQL は `42710`）が ERR-6
  の表に無いため、本実装は `42P07` を流用した。spec リポでの追加要否の判断を
  依頼する
- スコープ外として残すもの: `CREATE TABLE` での `CONSTRAINT <name> UNIQUE`・
  CHECK／PK／FK の ADD／DROP CONSTRAINT・`DROP CONSTRAINT IF EXISTS`／
  `CASCADE`・制約の一覧を取得する手段（`pg_constraint` 相当）・永続一意索引
