# ALTER TABLE ADD／DROP CONSTRAINT CHECK（TABLE-16・TASK-204）

- **Issue**: #1068（`feat(engine)!: ALTER TABLE ADD／DROP CONSTRAINT CHECK`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/05-tasks.md`
  TASK-204・`docs/spec/04-behavior/data-model.md` TABLE-16・
  `docs/spec/04-behavior/sql-surface.md` SQL-23・SQL-31・
  `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
  `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4・ERR-6
- **関連**: `docs/design/sql-check-constraint.md`（#906。CHECK の単一検査点・
  述語文法・永続化 v7・評価エラー契約〔#1075〕）・
  `docs/design/alter-table-unique-constraint.md`（#1067。制約名の名前空間
  D1・既定名 D2・エラー契約 D4・判定順 D7。本 Issue はこれと同じ設計方針を
  CHECK へ拡張する）
- **ステータス**: 実装済み（本ドキュメントが範囲を確定する）

## 背景・目的

CHECK 制約（#906）は `CREATE TABLE` でしか宣言できず、`ALTER TABLE ... DROP
CONSTRAINT <check 名>` は #1067 で `0A000` として意図的に拒否されていた。
本 Issue は既存テーブルへの CHECK の追加・削除を SQL 表層に追加する。追加時
は既存行（全テナント）が新しい述語を満たすことを検証しなければ制約が嘘に
なるため、全行の再検証が必要になる。

## 設計判断

### D1. 受理する構文

```text
ALTER TABLE <table> ADD [CONSTRAINT <name>] CHECK ( <述語> ) [;]
ALTER TABLE <table> DROP CONSTRAINT <name> [;]      -- CHECK 名も対象になる
```

述語文法は `CREATE TABLE` の CHECK と同一（`Parser::parse_check_parenthesized_body`。
`)` 境界・`OR` は `42601`）。制約名の検証は `catalog::validate_identifier` のみ
（`ADD UNIQUE` と揃える）。`CREATE TABLE` の列リストにある「列型キーワード
（`TEXT`／`VECTOR`）と一致する名前の拒否」は列定義との曖昧さ対策であり、
`ADD` の直後には曖昧さが無いので適用しない。

スコープ外（`42601` のまま）: `NOT VALID`（既存行の検証を飛ばす形。黙って
受理しない）・`VALIDATE CONSTRAINT`・`NO INHERIT`・`DROP CONSTRAINT IF
EXISTS`／`CASCADE`／`RESTRICT`・1 文に複数の `ADD`／`DROP`・`CHECK` 本体の
`OR`。NoSQL 表層（`alter_table` op）への CHECK 追加も対象外
（`create_table` の `check` も `0A000` のまま）。

### D2. 制約名（UNIQUE と揃える）

名前空間はテーブル単位で UNIQUE と CHECK の実名を共有する（#1067 D1 と同じ）。
PK は名前を持たない。FOREIGN KEY は当初（本 Issue 実装時点）は名前を持たな
かったが、Issue #1069（`ALTER TABLE ADD／DROP CONSTRAINT FOREIGN KEY`）で
同じテーブル単位の名前空間へ合流した（設計 F1。#1069 との base（main）取り込み
マージで `alter_table_add_check_constraint` の明示名衝突判定・既定名の衝突
回避の両方を FOREIGN KEY 実名にも対称に広げた。
`docs/design/alter-table-foreign-key-constraint.md` D2 参照）。

- 明示名の衝突（既存 UNIQUE 名・CHECK 名・FOREIGN KEY 名）→
  `CatalogError::ConstraintAlreadyExists` → `42710`（Issue #1195。`ADD UNIQUE` は `42P07`）（
  `ADD FOREIGN KEY` は CHECK と同じ）。`validate_schema` より前に判定する。
- 名前省略時の既定名: `CREATE TABLE` の表制約と同じ `<table>_check`
  （`sql::check_constraint::default_check_name(table, None)`）。衝突解決は
  `resolve_unique_name`（＝ `catalog::resolve_constraint_name`、接頭辞
  `"check"`）を使い、使用済み集合は **UNIQUE ∪ CHECK ∪ FOREIGN KEY の実名**
  （`CREATE TABLE` の `validate_and_build` は CHECK 名のみを見るが、ALTER
  では既存 UNIQUE 名・FOREIGN KEY 名も避ける必要がある）。
- 件数上限: 既存 `MAX_CHECK_CONSTRAINTS_PER_TABLE`（32）を超える追加は
  `CatalogError::ConstraintLimitExceeded` → `54000`（UNIQUE と同じ）。
- カタログフォーマットの変更は不要。CHECK 名は v7 以降の `check:<name>:...`
  行に常に実名で保存される。UNIQUE の既定名導出
  （`derive_unique_constraint_names`）は CHECK 名を使用済み集合に含むため、
  CHECK の追加・削除で導出名がずれることがあるが、`encode_schema` が
  「実名 ≠ 導出名」を検出して v10/v11 で実名を保存するため、UNIQUE の名前が
  暗黙に変わることはない（回帰テスト
  `sql_alter_table_check_constraint.rs::drop_check_preserves_unique_constraint_real_name_across_reopen`
  で固定）。

### D3. エラーの運び方（crate 内部の合成エラー型）

ADD CHECK の実行本体は、既存行の検証中に `TenantWriteError::
CheckEvaluationFailed(SqlSurfaceError)`（0 除算・`BIGINT` 精度超過等）を
返し得る。#1075 のオーナー判断（`docs/design/sql-check-constraint.md` の
エラー契約節）により、この `SqlSurfaceError` は `XX000` に丸めずそのまま
透過する必要がある。また in-txn の意味論検証（束縛・禁止要素・往復一致）も
`SqlSurfaceError` を返す。

`CatalogError` にこれを運ばせると catalog → sql の型依存と公開 API
（`core_api.snapshot`）の拡大を招くため、crate 内部の合成エラー型
`catalog::AlterCheckError { Catalog(CatalogError), Sql(SqlSurfaceError) }`
を新設した（`impl From<CatalogError> for AlterCheckError` で `?` の連鎖を
保つ）。`Storage::alter_table_add_check_constraint` は `pub(crate)`（CHECK は
`CheckConstraint`・`with_checks`・`validate_and_build` がすべて `pub(crate)`
で、`CREATE TABLE` にも公開 Rust API が無い既存方針に合わせる。任意の SQL
テキストを公開 API から差し込ませない）。

新しい `CatalogError` variant・`ErrorClass`・`wire_code` は追加しない
（HTTP 射影・`err4_http_projection.rs` は無変更で整合）。

### D4. エラー契約（ERR-6 の既存行のみ）

| 条件 | 経路 | `wire_code` |
| --- | --- | --- |
| 構文が許可リスト外（`NOT VALID`・`OR`・名前不正等） | `sql::allowlist` | `42601` |
| DDL 権限なし | `require_ddl_permission` | `42501`（カタログ照会より前） |
| 明示トランザクション内 | 既存 catch-all | `0A000` |
| テーブルが無い／ビュー・索引名 | `TableNotFound` 等 | `42P01`／`42809` |
| 明示名の衝突（UNIQUE・CHECK・FOREIGN KEY と同名） | `ConstraintAlreadyExists` | `42710`（Issue #1195。UNIQUE 追加側は `42P07`） |
| CHECK 件数上限超過 | `ConstraintLimitExceeded` | `54000` |
| 述語の意味論エラー（未知列・禁止要素〔`visible()`・UDF〕・参照列数／述語長上限・往復不一致） | `build_check_constraint` と同じ `SqlSurfaceError` | CREATE TABLE と同一（`42601`／`54000` 等） |
| 既存行が述語を満たさない（FALSE） | `SqlSurfaceError::check_violation(<新制約名>)` | `23514`（HTTP 409） |
| 既存行の評価自体が失敗（0 除算等） | `CheckEvaluationFailed(e)` の `e` を透過 | 通常の式評価と同じ（例: 0 除算は `22012`） |
| 既存行のデコード失敗・再束縛失敗・ヘッダのテナント不整合 | `CatalogError::CorruptSchema` | `XX000`（詳細はクライアントへ出さない） |
| DROP 対象名が存在しない | `ConstraintNotFound` | `42704` |
| DROP 対象が UNIQUE で FK が参照 | 既存 `DependentObjectsStillExist` | `2BP01` |
| 書き込みゲート待機上限超過 | `WriteLockTimeout` | `55P03` |

既存行のデコード失敗（例: 制約の無い時期に Rust API の生 `RowInput` で正規
レイアウト外の metadata を書いた行）は `CompiledChecks::enforce` が
`TenantWriteError::Catalog(Invalid(..))` を返し得る。これをそのまま呼び出し
元へ流すと `Invalid → 42601` になり誤分類になるため、
`constraint::validate_existing_rows_for_check` の境界で「`CheckViolation`／
`CheckEvaluationFailed` 以外はすべて `CorruptSchema`」へ写像する
（fail-closed。ADD は拒否され commit されない）。

### D5. 判定順（決定的・fail-closed。データに依存するのは最後の `23514`／評価エラーのみ）

**ADD CHECK**: 構文検証（`42601`。カタログ非参照）→ 明示トランザクション内
なら `0A000` → `require_ddl_permission`（`42501`）→ テーブル存在確認
（`42P01`／`42809`）→ **write txn 内で**: スキーマ再取得
（`require_table_schema_write`）→ 明示名の衝突（`42710`）→ CHECK 件数上限
（`54000`）→ 取得したスキーマに対する意味論検証（束縛・禁止要素・参照列
抽出・正規化レンダリング・往復一致）→ 名前確定（明示名 or 既定名）→
追加後スキーマの `validate_schema` → **新しい CHECK 1 件だけ**をコンパイル
して全行走査（違反 `23514`／評価エラー透過／破損 `XX000`。いずれも commit
せず破棄）→ `encode_schema` → カタログ挿入 → `bump_table_generation_in_txn`
→ commit。

0A000 と 42501 の順序は既存 DDL の実装（`core.rs`）に従う。意味論検証を
write txn の**外**（`get_table_schema` のスナップショット）で行わない。間に
`DROP COLUMN` 等が挟まった場合、`CompiledChecks::compile` の漂流検出で
`XX000` になってしまい誤分類になるため、in-txn のスキーマで検証する
（TOCTOU 回避）。

**DROP CONSTRAINT**: 構文 → `0A000`（txn 内）→ `42501` → 存在確認 →
write txn 内で: 名前の検索は **UNIQUE → FOREIGN KEY → CHECK** の順
（Issue #1069 で FOREIGN KEY が名前空間に合流したため拡張。
`docs/design/alter-table-foreign-key-constraint.md` F8 参照）。UNIQUE 実名に
一致 → 既存ロジック（FK 依存 `2BP01`）／FOREIGN KEY 実名に一致 → FOREIGN
KEY を除去（既存行を変更しないため依存検査は不要）／CHECK 実名に一致 →
CHECK を除去（CHECK には FK 依存が無い）／いずれにも無ければ `42704` →
`encode_schema`（CHECK が 0 件になれば v7 未満の正規形へ戻る。UNIQUE の実名は
D2 のとおり保持）→ 索引衛生（削除後のカタログから求めた「本当に必要な索引名」
で対象テーブル自身の stale 索引を刈り込み、FOREIGN KEY 削除時はその参照先
〔親〕テーブルも対象にする。CHECK は索引を持たないため、CHECK 削除時のこの
刈り込みは対象索引がなく実質的に no-op）→ 世代 bump → commit。

### D6. 既存行の走査（全テナント・全可視性）

`constraint::validate_existing_rows_for_check` を新設し、
`table_has_duplicate_unique_key` と同じ形で実装した:

- 行ストア全体を物理キー順に走査。`decode_row_header` + `verify_row_key_tenant`
  でヘッダと物理キーのテナント整合を検査してから値を読む。
- `decode_row_embedding_and_metadata_into`（スクラッチ `Vec<f32>` を再利用）
  → `compiled.enforce(schema, id, &embedding, metadata)`。
- 三値論理（NULL は通過）は `enforce` の既存実装をそのまま使う（第 2 の
  評価器を作らない）。

コンパイル対象は `schema.clone().with_checks(vec![new_check])` とし、
**新しい制約だけ**を評価する（違反時のエラーが新制約名を指す。既存 CHECK
は再評価しない）。行ストア未作成（`redb::TableError::TableDoesNotExist`）は
0 行として成功（UNIQUE と同じ）。

`Public`／`Private` を問わず全行を対象にする（`PolicyContext` を取らない。
`(table, PolicyContext)` 可視スナップショット由来の索引・キャッシュは流用
しない。可視集合へ縮めると不可視行の違反を見逃す fail-open になる）。走査
は書き込みゲートを保持した write txn 内で行うため、並行書き込みとの
TOCTOU は無い（行数に比例するコストは UNIQUE の ADD と同じ扱い）。

### D7. 公開 API の変更（BREAKING CHANGE）

1. `sql::allowlist::ValidatedAlterTable` に `AddCheck(ValidatedAlterTableAddCheck)`
   variant を追加（`ValidatedAlterTableAddCheck { pub table_name: String, pub
   check: ParsedCheck }`。`ParsedCheck.column` は常に `None`）。クレート外の
   網羅 `match` は追随が必要。
2. `catalog::CatalogError::ConstraintDropNotSupported` を**削除**（本 Issue
   以降どの経路からも生成されない。PK/FK は名前を持たず `42704`、CHECK の
   DROP は成功する）。`core_api.snapshot` を更新。
3. `Storage::alter_table_drop_constraint`（公開 Rust API）の挙動変更: CHECK
   名を指定すると削除する（従来は `ConstraintDropNotSupported`）。
4. `AlterTableAction` は据え置き（`AddConstraint` を CHECK でも使う）。
   `SqlOutcome::AlterTable` も据え置きで wire-server は無変更。

## セキュリティ考慮事項（OWASP Top 10・AGENTS.md P0）

- DDL は `require_ddl_permission`（`42501`）を必ず先に通し、カタログ照会
  （存在確認・名前衝突判定・述語の束縛）はすべて権限ゲートの後に置く。
  構文段（カタログ非参照）だけが権限ゲートの前に動く。
- 既存行の検証は `PolicyContext` を取らず全テナント・全可視性を走査する
  （可視集合へ縮めると不可視行の違反を見逃して「制約付きなのに違反行が
  存在する」fail-open になる）。RLS の判定そのものは一切変更しない。
- 応答は `23514`＋制約名のみ（テナント名・値・行 id・件数を含まない）。
  「どこかのテナントに違反行がある」という 1 ビットは原理的に伝わるが、
  UNIQUE の ADD（#1067 D7）と同じく運用者権限（`42501` ゲート）で扱う。
- 述語は `WHERE` と同一の許可リスト文法でパースし、正規化レンダリング →
  再パースの往復一致を検証したテキストだけを永続化する（生 SQL は保存
  しない）。`visible()`・UDF は禁止要素として `42601`。
- 既存行違反・評価エラー・デコード失敗のいずれでも write txn を commit
  せず破棄する（カタログ・世代・行とも不変）。`NOT VALID` を黙って受理
  しない。デコード不能行は `42601` に誤分類せず `XX000` で拒否する。
- 件数上限（32）・述語長上限・参照列数上限・式ノード数／深さ上限は既存
  定数を再利用。走査は行ごとに埋め込みスクラッチを再利用し、
  `scan_scalar_columns_masked` のマスクで参照列だけをデコードする。
- 新しい commit 経路には必ず `bump_table_generation_in_txn` を伴わせ、世代
  照合の失効検出漏れを作らない
  （`crates/engine/tests/table_generation_bump_coverage.rs` の構造テストで
  強制）。
- 依存の追加・更新なし。

## 検証

- `crates/engine/tests/sql_alter_table_check_constraint.rs`: SQL 経由の
  ADD（既定名・明示名・既定名の UNIQUE／CHECK 名衝突回避）・既存行の違反
  拒否（副作用ゼロ・クロステナント・`Private` 可視性を含む）・既存行の
  評価エラー透過（0 除算は `22012`）・修正後の再 ADD 成功・件数上限・DROP（成功・
  往復・UNIQUE 実名保持回帰・DROP 後の DROP COLUMN 許可）・DDL 権限・
  明示トランザクション内の `0A000`・スコープ外構文の拒否・永続化
- `crates/engine/tests/sql_alter_table_unique_constraint.rs`: 既存の
  `drop_constraint_naming_a_check_constraint_is_0a000` を成功系へ置き換え、
  `out_of_scope_alter_table_forms_are_rejected_with_42601` の CHECK
  アサーションを `NOT VALID` 付きの形へ更新
- `crates/engine/src/sql/check_constraint.rs`・`crates/engine/tests/
  table16_check_constraint.rs`: `validate_and_build` の
  `build_check_constraint` への抽出が挙動を変えないことを既存テストで確認

## 対象外（申し送り。Issue は起票しない）

- NoSQL 表層の `alter_table` op での CHECK 追加・削除（`create_table` の
  `check` も `0A000` のまま）
- `NOT VALID`／`VALIDATE CONSTRAINT`・`DROP CONSTRAINT IF EXISTS`／
  `CASCADE`・1 文複数 ADD/DROP
- PostgreSQL の既定名（単一列参照時の `<table>_<col>_check`）との差異
  （本リポの `CREATE TABLE` 表制約の既定名に揃えた）
- 制約名衝突の専用 SQLSTATE（`42710`）の要否は #1067 から引き続き spec 側の
  課題（Issue #1195 で `42710` へ是正済み）
- 制約一覧の照会手段（`pg_constraint` 相当）が無い点は #1067 と同じ
