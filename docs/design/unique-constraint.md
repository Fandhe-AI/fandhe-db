# UNIQUE 制約（TABLE-16・TASK-204）

- **Issue**: #905（`feat(engine): UNIQUE 制約`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/05-tasks.md`
  TASK-204・`docs/spec/04-behavior/data-model.md` TABLE-16・TABLE-12・
  `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
  `docs/spec/04-behavior/error-format.md` ERR-2・ERR-4・ERR-6・
  `docs/spec/04-behavior/recovery.md` RECOVER-7・RECOVER-12・
  `docs/spec/04-behavior/sql-surface.md` SQL-31
- **関連**: `docs/design/sql-primary-key.md`（Issue #903。一意性の検査点を共有）・
  `docs/design/not-null-default.md`（Issue #904。カタログ v5）・
  `docs/design/explicit-transaction.md`（Issue #942。明示トランザクション）
- **ステータス**: 実装済み（本ドキュメントが範囲を確定する。カタログ永続化・
  `PRIMARY KEY` と共有する単一検査点・`CREATE TABLE` 構文・
  `Storage::alter_table_add_unique_constraint`・層 A テスト）

## 背景・目的

行 `id` の暗黙一意性（TABLE-12）・`PRIMARY KEY` 宣言（Issue #903）に加え、任意の
列（単一列・複数列）の値の組に一意性を課す UNIQUE 制約を導入する。受入基準:

1. 宣言済み列に対する重複する INSERT・UPDATE・UPSERT を `23505` で拒否する
2. 一意性のスコープはテナント内に閉じる（他テナントの同値は違反にならない）
3. 既存行に重複がある状態での制約追加を fail-closed に拒否する（副作用ゼロ）
4. 応答（成否・`wire_code`・文言）から他テナントの値の存在を推測できない
5. RLS 暗黙適用・fail-closed 維持、`wire_code` 契約との整合、untrusted 入力経路
   での `unwrap`/`expect`/添字アクセス禁止、依存追加なし

## 設計判断

### D1. 検査点は `PRIMARY KEY` と共有する（第 2 の仕組みを作らない）

一意性の検査は `constraint::enforce_unique_keys_in_txn`（`crates/engine/src/
constraint.rs`。Issue #903 で `PRIMARY KEY` 用に新設された検査点を一般化したもの）
に一本化する。当初（main 取り込み前）の実装は `tenant/unique_check.rs` に独自の
「書き込み直前」検査点を持っていたが、main 側で同型の `PRIMARY KEY` 検査点が
先にマージされたため、base 取り込みで独自実装を撤去し、main 側の検査点へ
UNIQUE 制約を載せ替えた。

- 検査対象のキーは「主キー（宣言時）＋各 UNIQUE 制約（宣言順）」。構成列の
  和集合だけを `row_codec::scan_scalar_columns_masked` でデコードし、テナント
  範囲の走査は**1 文あたり 1 回**（全キーをまとめて照合する）。
- 呼び出し位置は main の `PRIMARY KEY` と同じく、`tenant.rs` の各書き込み関数
  （`insert_row_unchecked`・`insert_rows_unchecked`・`insert_typed_row_unchecked`・
  `insert_typed_rows_unchecked`〔COPY・NoSQL `insert` op・SQL 複数行 INSERT の
  共有入口〕・`upsert_typed_rows_unchecked`・`update_row_unchecked`・
  `update_row_columns_unchecked`・`update_rows_where_unchecked`・
  `replace_typed_rows_by_text_key`）で、**台帳記録・行書き込みの後**・**テーブル
  世代 bump・commit の前**。判定は今回書き込んだ行の `id` 集合（`written_ids`。
  UPSERT の `DO NOTHING` は含まない）について (1) `written_ids` 同士、(2) 対象
  テナントの残り全行との 2 段で行う。自己更新の除外は `written_ids` からの除外
  そのもので行う。
- 違反時は commit 前に `Err` を返し write txn を破棄する（台帳エントリを含め
  副作用ゼロ）。同一 `operation_id`・同一内容の再送は台帳照合が先に走るため、
  値そのものが UNIQUE 制約と衝突していても台帳由来の判定が優先される
  （RECOVER-12）。
- 母集合はテナント所有の**全行**（`Public`／`Private` を問わない。可視集合では
  ない）。物理キー範囲 `(tenant_id, 0)..=(tenant_id, u64::MAX)`（TABLE-12）だけを
  走査し、他テナントの範囲は構造的に一切読まない。二次索引・世代整合キャッシュは
  流用しない。

**既知の制約**: 永続一意索引は導入しないため、一意キーを宣言したテーブルへの
書き込みは 1 文あたり O(自テナント行数) の走査を伴う（`MAX_SCANNED_ROWS` のような
上限は課さない——課すと大規模テナントの制約付きテーブルが書き込み不能になるため）。
redb は単一ライターのため、この走査中は他テナントの書き込みも待たされる。走査を
文あたり 1 回に保つことが直接的な緩和策であり、永続一意索引（redb 二次テーブルに
よる O(log n) 判定）は将来検討事項として残す。

### D2. 明示トランザクション内の書き込み（SQL-31・TASK-221）

明示トランザクション中の書き込み（`tenant::WriteTarget::InTxn`）は、`BEGIN` で
取得した共有 write トランザクションへ未 commit のまま積まれる。検査点は同じ
write トランザクション内で走査し、redb の write トランザクションは自身が書いた
（削除した）未 commit の行を読めるため、同一トランザクション内の先行文が書いた
行との重複も見落とさない（`BEGIN; INSERT (1,'x'); INSERT (2,'x')` の 2 文目が
`23505`。トランザクションは `Failed` へ遷移する）。同様に、同一トランザクション内で
先に `TRUNCATE` した行の値は後続 INSERT の衝突相手にならない。追加の仕組みは
不要で、`crates/engine/tests/unique_constraint.rs` の明示トランザクション節で
固定している。

### D3. 等価性・NULL・対象型

- NULLS DISTINCT: 構成列のいずれかが NULL の行は、その制約の検査対象外（NULL
  同士は衝突しない）。主キー（構成列は非 nullable）は NULL を内部矛盾として
  fail-closed に拒否する点だけが異なる。
- キーは主キーと同じ正準バイト列（型タグ＋`u32` BE 長さ前置＋本体。可変長
  コンポーネントの境界曖昧性を構造的に排除）で比較する。
- 対象型は主キーと共有する単一の許可リスト `ColumnType::is_primary_key_allowed`
  （`TEXT`・`INTEGER`・`BIGINT`・`BOOLEAN`・`DATE`・`TIMESTAMP`・`UUID`・`BYTEA`・
  `ENUM`）。`VECTOR`・`REAL`／`DOUBLE PRECISION`・`NUMERIC`・`JSON`／`JSONB`・
  配列型は対象外。取り込み前の実装は `NUMERIC` も対象にしていたが、第 2 の
  許可リストを持たない方針に合わせて主キーと同じ範囲へ揃えた（SQL 表層の
  `CREATE TABLE` は現状 `TEXT`／`VECTOR` のみを受理するため、SQL から到達する
  範囲は変わらない）。

### D4. カタログ永続化（v6）

`TableSchema` に非公開フィールド `unique_constraints: Vec<UniqueConstraint>`
（`pub fn unique_constraints()`・`pub(crate) fn with_unique_constraints`）を追加。
`UniqueConstraint::columns()` は宣言順の列名リスト。

カタログ形式: main 側で `v4`（`PRIMARY KEY`・Issue #903）・`v5`（`DEFAULT`・
Issue #904）が先に採番されたため、UNIQUE 制約は main の最新版の次の **`v6`**
とした（取り込み前の実装が使っていた `v4` は未リリースのため互換読み込みは不要）。
`v6` は `v5` の上位集合で、UNIQUE 制約を 1 つ以上持つスキーマは主キー・
`DEFAULT`・墓標の有無に関わらず必ず `v6` で書く。

```text
v6
cols:<物理スロット数>
pk:<col>,<col>          ← 主キー未宣言なら空（v5 と同じ）
<name>:<tag>:<param>:<nullable>:<state>:<default>
...
uniq:<n>                ← n >= 1
U:<col>[,<col>]*        ← n 行
```

- UNIQUE 制約を持たないスキーマは従来どおり `v2`〜`v5` のままバイト列不変
  （既存ゴールデンテストへの影響なし。`v2`〜`v6` は互いに排他な正規形）。
- `uniq:` セクションは共有パーサー `parse_unique_section` が構造検証する（件数の
  数値形式・`1..=MAX_UNIQUE_CONSTRAINTS`・`U:` 接頭辞・空要素なし・要素数上限を
  `Vec` へ積む前に判定・識別子形状・制約内の列名重複なし・同一列リストの制約
  重複なし）。`decode_schema_body` と、`DROP TYPE` の依存判定に使う軽量パーサー
  `catalog_value_references_enum_type` の両方がこのパーサーを使い、後者も参照列が
  生存列に実在することを列行の読み取り後に検証する（片方だけが緩いと、壊れた
  カタログ値が `DROP TYPE` の依存判定だけ「依存なし」に丸められるため）。
- `validate_schema`（`validate_unique_constraints`）が参照列の実在（生存列）・
  対象型・制約内列重複・同一列リストの制約重複（宣言順のまま比較する実装
  既定値）・上限（`MAX_UNIQUE_CONSTRAINTS` = 32 制約／テーブル・
  `MAX_UNIQUE_CONSTRAINT_COLUMNS` = 32 列／制約）を検証する。違反は主キーと
  同じく `CatalogError::Invalid`。

### D5. DDL 経路

**SQL `CREATE TABLE`**（`sql/allowlist.rs::Parser::parse_create_table`）: 列制約
`<col> TEXT UNIQUE`（`NOT NULL`／`DEFAULT` と順序自由・最大 1 回。`VECTOR` 列への
付与は `42601`）と、表制約 `UNIQUE (<col>[, <col>]*)` を受理する。表制約は
「識別子 `UNIQUE` の直後が `(`」で判定し、列名 `unique` と区別する。参照列の解決
（未宣言列・`id`・対象外型の参照は `42601`）は全列が出揃った後にまとめて行う
（`finalize_unique_constraints`）。同一制約内の列名重複は `42701`、空リストは
`42601`、制約数・制約あたり列数の上限超過は `Vec` へ積む前に `54000`。

**列数上限の判定（レビュー指摘の是正）**: 列数上限（`MAX_CREATE_TABLE_COLUMNS` =
256）は、列定義 1 個をパースする直前に「確定済みの列数」だけで判定する。表制約は
列を追加しないため判定の対象外で、制約が列リストの先頭・中間・末尾のどこに
あっても結果が変わらない。取り込み前の実装はカンマ直後の先読みで表制約を
判定対象から除外していたため、表制約の**後ろ**に続く最後の超過列
（例: `c0 .. c255, UNIQUE (c0), c_extra`）が構文段階の `54000` をすり抜けて
257 列を受理していた。位置非依存の判定へ置き換え、`PRIMARY KEY` 表制約も同じ
判定を共有する。

**制約追加（受入基準 3）**: `Storage::alter_table_add_unique_constraint(table,
&[&str])`（Rust API 専用。SQL `ALTER TABLE ... ADD UNIQUE` は対象外）。追加後の
スキーマとして `validate_schema` を先に通し、単一 write txn 内でテーブル全行を
テナントごとに独立して走査して重複を検出する
（`constraint::table_has_duplicate_unique_key`。書き込み時と同じ正準キー・NULLS
DISTINCT・行ヘッダと物理キーのテナント整合検査つき）。1 件でもあれば
`CatalogError::UniqueConstraintViolation` で拒否する（副作用ゼロ・カタログ不変・
世代不変）。

`alter_table_drop_column`: 制約に含まれる列の削除は主キー構成列と同じく
`CatalogError::DependentObjectsStillExist` で fail-closed に拒否する（暗黙
cascade しない）。

**ファイル形 INSERT**（`replace_typed_rows_by_text_key`。増分インデックス反映・
TASK-120。UNIQUE 制約テーブルへの対応は Issue #1072）: 旧チャンク行の削除・
新規行の挿入・D6 の一意性検査（`constraint::enforce_row_constraints_in_txn`）を
すべて同一 write トランザクション内で行い、削除を検査より先に完了させる。redb の
write トランザクションは自分が消した行をそのまま読めるため、旧チャンクは検査時の
母集合から自然に除外される。これにより、同じ `path` への再送は旧チャンクと
衝突せず成功し、宣言された UNIQUE 制約はそのまま（特別扱いなく）検査される
（チャンク間で値が変わらない列だけで構成される UNIQUE は、複数チャンクに
分割されるファイルを新規チャンクどうしの衝突として `23505` で拒否する）。
違反時は commit 前の `?` でトランザクションごと abort し、台帳記録・削除・
挿入のいずれも副作用として残らない。

### D6. エラー契約

main 側で `PRIMARY KEY` 用に追加された `TenantWriteError::UniqueViolation`・
`SqlSurfaceError::UniqueViolation`（いずれも固定文言 `unique constraint
violation`。値・列名・行 id・テナントを含めない）をそのまま再利用し、既存の
`ErrorClass::UniqueViolation`（`23505`／`UNIQUE_VIOLATION`）へ写像する。HTTP
（NoSQL 表層）も既存の `ErrorClass::UniqueViolation → 409` 写像を再利用する。

台帳由来の重複（`DuplicateOperationId`）・行キー衝突（`IdConflict`）とは Rust の
型レベルでは区別できるが、`wire_code`／HTTP `code` レベルではいずれも
`23505`／`UNIQUE_VIOLATION` のまま（PostgreSQL 自身も行制約・値制約いずれの一意性
違反も同一 SQLSTATE `23505` で返す。`ErrorClass` の 1 wire_code=1 label の
不変条件〔`wire_codes_are_pairwise_distinct`〕とも整合する）。

`CatalogError::UniqueConstraintViolation`（公開 enum への variant 追加。BREAKING
CHANGE）は `alter_table_add_unique_constraint` 専用で、SQL 表層からは到達しない。
制約宣言の不正は専用 variant を作らず `CatalogError::Invalid` へ揃えた。

## スコープ外・申し送り

- SQL `ALTER TABLE ... ADD [CONSTRAINT] UNIQUE` / `DROP CONSTRAINT` と制約名
- 永続一意索引（redb 二次テーブル）による O(log n) 判定
- `UPSERT` の `ON CONFLICT` 対象列（`id` 固定）への UNIQUE 列拡張
- `NUMERIC`／`REAL`／`DOUBLE PRECISION`／`JSON`／`JSONB`／配列型への拡張
- NoSQL 表層の DDL op（`create table` 相当）・wire-server 経由の専用結合テスト
  （NoSQL 表層は `execute_bound_insert_in_session`／`execute_bound_update_in_session`
  が同一の書き込みプリミティブを共有するため、同じ検査点の契約を継承する）

## 検証

- `crates/engine/tests/unique_constraint.rs`: `CREATE TABLE` の列制約・表制約
  構文、単一列・複合列・NULL 許容、テナントスコープ（可視・不可視を問わない
  母集合）、バッチ内重複・副作用ゼロ、台帳優先、UPDATE の自己比較除外、UPSERT の
  `DO UPDATE`／新規挿入分岐、`PRIMARY KEY` との併用、明示トランザクション内の
  未 commit 行との重複検出・`TRUNCATE` 後の再挿入、他テナントの値に依存しない
  応答、`Storage::alter_table_add_unique_constraint` の拒否・成功・事後強制、
  `alter_table_drop_column` の依存検査
- `crates/engine/tests/incremental_index.rs`（Issue #1072）: UNIQUE 制約付き
  テーブルへのファイル形 INSERT の同一パス再送成功・違反時のロールバック
  （旧チャンク復元・台帳未記録）・複数チャンクファイルの宣言どおりの `23505`・
  `body` を含む UNIQUE での複数チャンク成功・NULLS DISTINCT・テナント境界
- `crates/engine/src/constraint.rs` 単体テスト: NULLS DISTINCT・複合キーの完全
  一致判定とテナント境界・制約追加前の既存行重複判定
- `crates/engine/src/catalog.rs` 単体テスト: v6 の往復（主キー・`DEFAULT`・墓標と
  の併存を含む）・v2〜v5 のバイト列不変・`validate_unique_constraints` の拒否・
  v6 破損値の `CorruptSchema` 拒否・`catalog_value_references_enum_type` の v6
  検証
- `crates/engine/src/sql/allowlist.rs` 単体テスト: `UNIQUE` 構文の受理・拒否形、
  上限ちょうどの列＋表制約（先頭・中間・末尾）の受理、表制約の前・後ろ・間に
  超過列がある場合の `54000`、制約数・制約あたり列数の上限
