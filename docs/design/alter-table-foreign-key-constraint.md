# ALTER TABLE ADD／DROP CONSTRAINT FOREIGN KEY と制約名（TABLE-22・TASK-233）

- **Issue**: #1069（`feat(engine)!: ALTER TABLE ADD／DROP CONSTRAINT FOREIGN KEY と制約名`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/05-tasks.md`
  TASK-233・TASK-205・`docs/spec/04-behavior/data-model.md` TABLE-22（関連:
  TABLE-17・TABLE-20・TABLE-21・TABLE-16）・`docs/spec/04-behavior/sql-surface.md`
  SQL-23・SQL-31・`docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
  `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4・ERR-6
- **関連**: `docs/design/alter-table-unique-constraint.md`（Issue #1067。同じ
  制約名の名前空間・既定名導出パターンを FK へ拡張）・`docs/design/foreign-key.md`
  （Issue #907・#1076・#1077。FK の宣言・参照アクション・カタログ v8〜v11）
- **ステータス**: 実装済み（本ドキュメントが範囲を確定する）

## 背景・目的

`FOREIGN KEY` は `CREATE TABLE` でしか宣言できず、制約名も持たない
（`CONSTRAINT <name> FOREIGN KEY` は従来 `42601`）。本 Issue は SQL
`ALTER TABLE ... ADD [CONSTRAINT <name>] FOREIGN KEY (...) REFERENCES ...` /
名前による `DROP CONSTRAINT`（FK 名の解決を追加）と、`CREATE TABLE` での
`CONSTRAINT <name> FOREIGN KEY` の受理を追加する。

## 設計判断

### F1. 制約名の名前空間

Issue #1067（UNIQUE の名前空間）に FOREIGN KEY を合流させ、テーブル単位で
UNIQUE・CHECK・FOREIGN KEY の 3 種が名前空間を共有する（`validate_schema` が
3 種すべての重複を検査する）。relation 名前空間（テーブル・ビュー・索引）とは
共有しない。PRIMARY KEY は引き続き無名。

### F2. 既定名の導出（純関数）

`catalog::derive_foreign_key_constraint_names`（`derive_unique_constraint_names`
と同型）。候補名は `<table>_<col1>_<col2>..._fkey`（PostgreSQL 風）。衝突解決は
共通ヘルパ `catalog::resolve_constraint_name`（フォールバック接頭辞
`"fkey"`）に委譲する——UNIQUE・CHECK と同じ衝突解決規則を共有する。

名前の確定はカタログ層に集約する: `encode_schema` の内部で
`assign_unique_constraint_names` の**直後**に `assign_foreign_key_constraint_names`
を呼ぶ（UNIQUE の確定名を FK の既定名衝突判定の `used` 集合に含めるため）。
`sql::allowlist` は名前未確定（空名）の `ForeignKeyDef` を組み立てるだけで、
`validate_schema` の最終検証（`allow_unresolved == false`）は空名を拒否する
（TOCTOU 回避。UNIQUE と同じ設計）。

明示 FK 名は UNIQUE の既定名導出にも影響する（`assign_unique_constraint_names`
の `used` に非空の FK 名を加える）ため、`CREATE TABLE` に `CONSTRAINT fk_a
FOREIGN KEY (a) REFERENCES p` と無名 `UNIQUE(a)` を両方宣言しても衝突しない。

### F3. 既存（v8〜v11）カタログの FK の名前

v8〜v11 の無名 FK は decode 時に F2 で名前を導出する（`assign_foreign_key_constraint_names`
を decode 後に適用。v12 のみ素通しする）。既存 DB の FK も
`DROP CONSTRAINT <導出名>` で削除できるようになる——名前が暗黙に変わることは
ない（DROP 後に後続 FK の導出名がずれても再導出しない。encode 時の
`has_named_fk` 判定が「実名が現在の導出と食い違う」場合のみ v12 を選ぶため）。

### F4. カタログ v12

v11 の上位集合。`uniq:`／`checks:` セクションは 0 件を許し（この形式を選ぶ
判断材料は FK の実名のみで UNIQUE・CHECK の有無とは独立なため）、`fks:`
セクションのみ名前付き 8 フィールドの `fk:` 行
（`fk:<name>:<cols>:<parent>:<pcols>:<on_delete>:<on_update>:<match>:<deferral>`）
で書く。**v12 を書くのは、FK の実名が 1 つでも F2 の導出と食い違う場合だけ**
（`encode_schema` の `has_named_fk` 判定）。それ以外は現行の v2〜v11 の選択
ロジックとバイト列を変えない（既存ゴールデンテストは無変更で通る）。v12 の
`fk:` 行は v8〜v11 の 5/7 フィールド後方互換判別を持たない専用の
書き込み専用実装（`encode_foreign_key_section_v12`・`parse_foreign_key_section_v12`）
とし、既存の共有パーサー（`encode_foreign_key_section`・
`parse_foreign_key_section`）には手を入れない（v8 の固定バイト列の回帰を
崩さないため）。旧バイナリは v12 を未知の版として fail-closed に拒否する。

**v12 追加で更新した箇所**（全部必須。1 箇所でも漏れると fail-open になり
得る）:

- `CATALOG_FORMAT_VERSION_V12` 定数・`FK_BEARING_FORMAT_VERSIONS`（FK を
  持ちうる全版の単一 const 配列。v8〜v12）
- `encode_schema`（`has_named_fk` 判定 → v12 分岐。`assign_foreign_key_constraint_names`
  呼び出しの追加）
- `FormatVersion` と `decode_schema_body`（`pk:`・`uniq:`〔0 可・named〕・
  `checks:`〔0 可〕・`fks:`〔named・8 フィールド・`k >= 1` 必須〕）
- `catalog_value_references_enum_type`（軽量パーサー。v12 の分岐を追加し、
  FK 名と UNIQUE・CHECK 名の衝突検査も追加）
- **`referencing_foreign_keys_in_txn`**（`FK_BEARING_FORMAT_VERSIONS` から
  候補接頭辞を生成する。v12 を見落とすと、`DROP TABLE` の `2BP01` 判定・
  参照先側の書き込み検査・`required_key_index_names_in_txn`（§ 索引衛生）の
  いずれもが fail-open になる）

### F5. エラー契約（新しい `wire_code` は追加しない。ERR-6 ポインタ）

| 条件 | `CatalogError` | `wire_code` |
| --- | --- | --- |
| DDL 権限なし | ― | `42501`（カタログ照会より前） |
| 明示トランザクション内 | ― | `0A000`（既存 catch-all） |
| 構文が許可リスト外（`NOT VALID`・列制約 `CONSTRAINT n REFERENCES` 等） | ― | `42601` |
| 子テーブルが無い／ビュー・索引名 | `TableNotFound`／`WrongObjectKind` | `42P01`／`42809`（子） |
| 親テーブルが無い／ビュー・索引名 | `TableNotFound`／`WrongObjectKind` | `42P01`／`42809`（**親**の名前で報告） |
| 参照先が一意キーと一致しない・型不一致 | `InvalidForeignKey` | `42830` |
| **制約名の衝突**（UNIQUE・CHECK・FOREIGN KEY のいずれかと同名） | `ConstraintAlreadyExists` | `42710`（Issue #1195。UNIQUE 追加側は `42P07`） |
| テーブルあたり FK 数の上限（`MAX_FOREIGN_KEYS_PER_TABLE`）を超過 | `ConstraintLimitExceeded` | `54000` |
| **既存行が新しい FK を満たさない**（全テナント検証） | `ForeignKeyViolation`（新設） | `23503` |
| DROP する名前が存在しない | `ConstraintNotFound` | `42704` |
| DROP する名前が CHECK 制約 | ― | 成功（Issue #1068。従来の `ConstraintDropNotSupported`／`0A000` 拒否は撤廃） |
| 書き込みゲートの待機上限超過 | `WriteLockTimeout` | `55P03` |
| その他 | ― | `XX000` |

`ForeignKeyViolation` の文言は固定（`insert or update on table violates
foreign key constraint` 相当）でテナント・値・行・表名を含まない
（`TenantWriteError::ForeignKeyViolation` と同じ秘匿方針）。HTTP 射影は
新しい `ErrorClass` を追加していないため、`crates/wire-server/tests/err4_http_projection.rs`
は無変更で通過する。

### F6. `CREATE TABLE` での受理

表制約 `CONSTRAINT <name> FOREIGN KEY (...) REFERENCES ...` を受理する
（`allowlist::Parser::parse_create_table` に専用の先読み分岐を追加し、素の
`CONSTRAINT` を CHECK 句の開始として扱う既存の `peek_check_clause_start` より
前で判定する）。列制約 `<col> ... CONSTRAINT <n> REFERENCES` も Issue #1428 で
受理する（`parse_create_table_column` が PK 判定の後・CHECK ループの前で名前を読む）。
`CREATE TABLE` 内の明示名重複は Issue #1428 で
`allowlist::check_create_table_constraint_namespace` に一本化し、宣言順に依存しない
1 つの規則で判定する（FOREIGN KEY を含めば `42710`、含まず UNIQUE を含めば `42P07`、
主キーと CHECK は `42601`、CHECK 同士は従来どおり `validate_and_build` の `42601`）。
主キーの実効名（明示名、無ければ主キーが実在するときの導出名 `<table>_pkey`）も同じ
名前空間に含め、導出名と同名の制約は DROP で主キーを隠すため拒否する。

### F7. 判定順（ADD。決定的・fail-closed）

構文検証 → `require_ddl_permission`（`42501`）→ 子テーブルの存在確認
（`TableNotFound`）→ write txn 内で: スキーマ再取得 → 明示名の衝突
（UNIQUE∪CHECK∪FK。`42710`）→ FK 件数上限（`54000`）→ 参照先の解決
（ビュー・索引なら `WrongObjectKind`、不在なら `TableNotFound`。自己参照は
**変更前**の子スキーマ自身を親として解決する——親の列・主キー・UNIQUE 制約は
今回の FK 追加で変わらないため）→ `resolve_foreign_key_target` の照合
（`InvalidForeignKey`）→ 名前確定後の更新後スキーマで `validate_schema` →
索引衛生（子表・親表の stale 索引を検証**前**に刈り込む。§ 索引衛生参照）→
全テナントの既存行検証（`constraint::verify_new_foreign_key_all_tenants_in_txn`。
1 行でも違反があれば `23503` で副作用ゼロに拒否）→ `encode_schema` → カタログ
挿入 → 子テーブルの世代 bump → commit。確定した制約名を返す。

### F8. 判定順（DROP。決定的・fail-closed）

名前の検索は **UNIQUE → FOREIGN KEY → CHECK** の順（どれにも無ければ
`ConstraintNotFound`〔`42704`〕。CHECK 名を指定した DROP も Issue #1068 で
削除対象として成功するようになった——従来の `ConstraintDropNotSupported`
〔`0A000`〕拒否は撤廃済み。`docs/design/alter-table-check-constraint.md`
参照）。UNIQUE の削除は Issue #1067 と同じ FK 依存検査
（`referencing_foreign_keys_in_txn`。`parent_columns` の集合が削除対象の列
集合と一致する宣言があれば `2BP01`）を維持する。FOREIGN KEY・CHECK の削除は
既存行を変更しない（依存検査は不要）。いずれの削除後も索引衛生（§ 索引衛生）
を行い、世代を bump して commit する。

### 索引衛生（P0。`key_index.rs` の stale 索引による fail-open の防止）

`key_index.rs` の永続キー索引は「登録済みのテナントの間だけ」`sync_rows_in_txn`
で同期される。子の唯一の FK を DROP する、または親の UNIQUE を DROP すると、
`table_may_need_index(schema)` が偽になり、登録簿に残った索引はその表への
以後の書き込みで一切同期されなくなる。その空白期間中に行を追加・削除した
あと、同じ列集合の FK・UNIQUE を再度 ADD すると、`ensure_index_in_txn` は
「登録済み」と誤認して backfill をスキップし、索引は空白期間の変更を反映
しないまま参照整合性検査に使われる（fail-open）。

これを防ぐため、`key_index::prune_unneeded_indexes_in_txn(write_txn, table,
required_names)` を新設した。`table` の登録簿にある索引のうち
`required_names` に含まれないものを、fwd／rev の実テーブルごと・全テナント分
削除する。「本当に必要な索引名」は `catalog::required_key_index_names_in_txn`
が、`table` の登録簿が持ちうる 2 種類の索引を合わせて求める（Cursor Bugbot
Medium・codex P2 指摘・PR #1156 スレッド `PRRT_kwDOUAKASM6muWhg`・
`PRRT_kwDOUAKASM6mu1xV`・`PRRT_kwDOUAKASM6mu2fh`）:

- **親側索引**（`id` 参照は除外）: `referencing_foreign_keys_in_txn(write_txn,
  table)` が返す「`table` を参照する FK」の `parent_columns()` から求める。
  `id` 参照 FK は `table` 自身の索引を使わず物理キーの点照会で検査するため
  対象外。
- **子側索引**（`id` 参照も含む）: `table` 自身が現在宣言する `FOREIGN KEY`
  （`require_table_schema_write` で読む現在のカタログの `foreign_keys`）の
  参照元列 `columns()` から求める。`constraint::enforce_referencing_rows_in_txn`
  が `table = child_schema.name`（＝ FK 宣言側自身）で構築する索引で、`id`
  参照 FK でも `key_index::none_referenced_in_txn` が `parent_id_key_bytes`
  でエンコードした親 `id` 値をこの索引と突き合わせるため、`id` 参照だからと
  いって対象から除外してはならない。

呼び出し点:

- **DROP CONSTRAINT FOREIGN KEY**: カタログ書き換え後（削除後の状態を見る
  必要があるため）に、**この表自身**（他の FK の子側索引を巻き添えで
  削除しないため）と、削除した FK の**参照先（親）表**の両方を対象に刈り込む
- **DROP CONSTRAINT UNIQUE**: カタログ書き換え後に、**この表自身**
  （他表の FK がこの表の UNIQUE 列を参照しうるため）を対象に刈り込む
- **ADD FOREIGN KEY**: 検証**前**に、まだ新 FK を含まない現在のカタログから
  求めた必要集合で、**子表・親表の両方**を刈り込む（検証が信用する索引を
  検証前にクリーンな状態へ揃える）

`crates/engine/tests/sql_alter_table_foreign_key.rs` の
`readding_foreign_key_after_parent_unique_gap_detects_current_parent_rows`
（`prune_unneeded_indexes_in_txn` を一時的に無効化するとこのテストが失敗する
ことをミューテーションテストで確認済み）がこの P0 回帰を固定する。
`readding_foreign_key_after_drop_detects_rows_added_during_the_gap` は `id`
参照（`key_index.rs` を経由しない全行スキャン経路）を使うため索引の stale 化
そのものは再現しないが、`ADD FOREIGN KEY` の既存行検証が常に現在の子テーブル
状態を見ることを別途固定する。`crates/engine/src/key_index.rs` の
`tests::add_and_drop_sibling_foreign_key_preserve_other_fk_indexes`・
`tests::add_and_drop_sibling_foreign_key_preserve_id_referencing_fk_child_index`
（いずれもミューテーションテストで確認済み）は、2 本目の FK を ADD／DROP
しても同じ子テーブルが持つ**別の** FK（列参照・`id` 参照それぞれ）の子側・
親側索引が巻き添えで削除されないことを固定する。

### F9. 循環

`ALTER TABLE ADD` により、自己参照以外の循環（A↔B）を後から作れるようになる。
CASCADE 連鎖は既存の `MAX_REFERENTIAL_ACTION_DEPTH`／`MAX_REFERENTIAL_ACTION_ROWS`
で有界のまま。循環した両テーブルは、どちらの `DROP TABLE` も
`referencing_foreign_keys_in_txn` の依存検査により `2BP01` になり、先に
`DROP CONSTRAINT` が必要（PostgreSQL の CASCADE 無しと同じ）。
`docs/design/foreign-key.md` D8 の「`ALTER TABLE ADD FOREIGN KEY` を持たない
ため循環は作れない」という前提は本 Issue で成立しなくなるため、当該ドキュメント
を更新した（本ファイル末尾の関連ドキュメント更新箇所参照）。

### F10. 再検証コストのトレードオフ

Issue #1067（UNIQUE の ADD 時全テナント検証）と同じ判断で、上限は設けない
（`54000` は導入しない）。全テナント・全行走査は DDL 級のコストとして許容
する。テナント集合は子の行ストアを 1 回走査して求め、以後テナントごとに
1 回ずつ `enforce_referencing_rows_by_scan_for_fk` を呼ぶ。

### F11. 公開 API の変更（BREAKING CHANGE）

1. `catalog::ForeignKeyDef` に非公開 `name: String` を追加し、`name()` を
   公開。構築は `new(...)`（名前未確定）と `with_name(name)` ビルダーの
   2 経路。`ForeignKeyDef` の `PartialEq`／`Eq` 導出に名前が含まれるように
   なった
2. `catalog::CatalogError` に `ForeignKeyViolation` を追加
   （`#[non_exhaustive]` は付与しない方針を継続）
3. `sql::allowlist::ValidatedAlterTable` に `AddForeignKey(ValidatedAlterTableAddForeignKey)`
   variant を追加
4. Rust API: 新設 `Storage::alter_table_add_foreign_key(table, name:
   Option<&str>, foreign_key: ForeignKeyDef) -> Result<String>` は
   `pub(crate)`（宣言面は SQL 表層のみ。`ForeignKeyDef::new`・
   `with_foreign_keys` が crate 内限定のため。Issue #907 の `create_table`
   専用設計と同じ方針）

### スコープの境界（fail-closed 側に倒す。申し送りのみ・Issue は起票しない）

対象外（`42601` のまま）: `NOT VALID`／`VALIDATE CONSTRAINT`・
`DROP CONSTRAINT IF EXISTS`／`CASCADE`／`RESTRICT`・PRIMARY KEY の追加と削除・
列制約形式の名前付き `REFERENCES`・NoSQL 表層の DDL 操作。legacy DB
（`CREATE TABLE` 経路）で親の索引が stale なまま `CREATE TABLE` 側の FK
解決に使われる経路は、DROP UNIQUE 時の刈り込み（本ドキュメント「索引衛生」）
により今後は発生しないが、既存 DB に残存する分の掃除は本 Issue のスコープ外
とする。`docs/spec` 側 TABLE-22 の未決事項の確定内容はオーナーへ別途報告する。

## 検証

- `crates/engine/tests/sql_alter_table_foreign_key.rs`: ADD（既定名・明示名・
  自己参照）・既存行の重複拒否（副作用ゼロ）・テナント境界（cross-tenant の
  孤児行が救済されないこと・エラー文言にテナント名を含まないこと）・名前衝突
  （UNIQUE との）・参照先エラー分類（`42P01`／`42830`）・DDL 権限
  （`42501`。存在オラクルにならないこと）・索引衛生の P0 回帰（子側・親側）・
  `CREATE TABLE` の FK・CHECK 名重複・スコープ外構文の拒否
- `crates/engine/src/key_index.rs` 単体テスト:
  `add_and_drop_sibling_foreign_key_preserve_other_fk_indexes`・
  `add_and_drop_sibling_foreign_key_preserve_id_referencing_fk_child_index`
  （無関係な FK の ADD／DROP が、同じ子テーブルが持つ別の FK の子側・親側
  索引を巻き添え削除しないこと。`id` 参照・列参照の両方を固定）
- `crates/engine/src/catalog.rs` 単体テスト: v12 の往復・v8〜v11 の decode 後
  既定名導出（`assign_foreign_key_constraint_names` 適用後の比較で検証。既存
  v8〜v11 のゴールデンバイト列アサーションは無変更のまま全通過）
- `crates/engine/tests/table17_foreign_key.rs`・`fk_referential_actions.rs`・
  `sql_alter_table_unique_constraint.rs`・`unique_constraint.rs`・
  `table_generation_bump_coverage.rs`・`sql31_transaction.rs`: 既存回帰が
  すべて無変更（`table_generation_bump_coverage.rs` のアローリスト行番号追随を
  除く）で通ることを確認済み
- `crates/wire-server/tests/err4_http_projection.rs`: 新しい `ErrorClass` が
  無いため無変更で通過（HTTP 射影の整合確認を兼ねる）
