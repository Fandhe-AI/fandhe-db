# UNIQUE 制約（TABLE-16・TASK-204）

- **Issue**: #905（`feat(engine): UNIQUE 制約`）・#1073（対象型の拡張:
  `feat(engine): UNIQUE 制約の対象型を NUMERIC・REAL・DOUBLE PRECISION・
  JSON 等へ拡張`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/05-tasks.md`
  TASK-204・`docs/spec/04-behavior/data-model.md` TABLE-16・TABLE-12・
  TABLE-13・TABLE-14・`docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
  `docs/spec/04-behavior/error-format.md` ERR-2・ERR-4・ERR-6・
  `docs/spec/04-behavior/recovery.md` RECOVER-7・RECOVER-12・
  `docs/spec/04-behavior/sql-surface.md` SQL-31
- **関連**: `docs/design/sql-primary-key.md`（Issue #903。一意性の検査点を共有）・
  `docs/design/not-null-default.md`（Issue #904。カタログ v5）・
  `docs/design/explicit-transaction.md`（Issue #942。明示トランザクション）・
  `docs/design/foreign-key.md`（Issue #907。D3 が前提にする「参照元は PK 許可型」を
  本 Issue の UNIQUE 対象型拡張と独立に維持する）
- **ステータス**: 実装済み（本ドキュメントが範囲を確定する。カタログ永続化・
  `PRIMARY KEY` と共有する単一検査点・`CREATE TABLE` 構文・
  `Storage::alter_table_add_unique_constraint`・層 A テスト。Issue #1073 で
  対象型を REAL・DOUBLE PRECISION・NUMERIC・JSON／JSONB・配列型へ拡張）

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
- 対象型は `ColumnType::is_unique_constraint_allowed`（Issue #1073）。
  `PRIMARY KEY`・`FOREIGN KEY` の参照元列が共有する許可リスト
  `ColumnType::is_primary_key_allowed`（`TEXT`・`INTEGER`・`BIGINT`・
  `BOOLEAN`・`DATE`・`TIMESTAMP`・`UUID`・`BYTEA`・`ENUM`）の**上位集合**で、
  `REAL`・`DOUBLE PRECISION`・`NUMERIC`・`JSON`／`JSONB`・配列型を追加で許可する。
  `VECTOR` のみ引き続き対象外。
  - **2 つの許可リストを持つ理由**（取り込み時点の「第 2 の許可リストを持たない」
    という当初方針からの改訂）: `FOREIGN KEY`（Issue #907・`docs/design/
    foreign-key.md` D3）は「参照元列は PK 許可型であること」を前提に設計されて
    いる。UNIQUE の許可リストをそのまま拡張すると、`REAL`・`JSON` 等が `PRIMARY
    KEY`・`FOREIGN KEY` の参照元列としても暗黙に宣言できるようになり、それらの
    型に対する等価性・参照整合性の設計判断（D7 参照）を経ないままスコープ外の
    挙動変更が入ってしまう。`is_primary_key_allowed` は据え置き、UNIQUE 専用の
    上位集合を新設することで、PK・FK の対象型を意図せず広げない
    （`crates/engine/src/catalog.rs` の `validate_foreign_keys_still_rejects_
    real_referencing_column_after_unique_extension` が固定回帰）。

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

### D7. 型ごとの正準キー（Issue #1073）

一意性判定は D3 と同じ「正準バイト列（型タグ＋長さ前置）の完全一致」のまま、
`constraint::push_canonical_component` が型ごとに**値として等価な表現を同一
バイト列へ正規化**してから比較する。カタログの互換性（v6 のバイト列形式）は
変更しない——正規化はキー生成時のスクラッチ処理であり、行バイト表現・カタログ
形式のいずれにも影響しない。

| 型 | 正規化 |
| --- | --- |
| `REAL`／`DOUBLE PRECISION` | `scalar_float::canonicalize_*` 適用後のビットパターン（`-0.0` を `+0.0` へ）。非有限値は `Err`（`row_codec` の encode が既に拒否するため通常到達しない防御層） |
| `NUMERIC` | 末尾ゼロを除去した `(unscaled, scale)` の最簡表現（`1.50` と `1.5` が同一キー。列内では `scale` が固定のため元々単射だが表現の揺れを構造的に吸収する） |
| `JSON`／`JSONB` | `json::canonical_equality_text`（値としての等価正規化テキスト。キー順・空白だけでなく数値も値として正規化する。`1`・`1.0`・`1e0` を同一に、`-0`・`0`・`0e5` を同一のゼロに。`JSON`／`JSONB` いずれも UNIQUE キー生成時にこの関数を共通して通す） |
| 配列 | `[要素タグ][要素数: u32 BE][要素列の生ペイロード]`（`row_codec::ArrayRef` のエンコーダ決定性〔要素順保持・flags 固定・代替表現なし〕により単射。要素順は区別し〔`{a,b}` ≠ `{b,a}`〕、`{}` と NULL も区別する） |

`JSON`／`JSONB` 列の等価正規化テキストは、将来 TABLE-14 の複合型等価述語
（`=`）を実装する際にも再利用すべき唯一の正準形とする（判断を二重に持たない）。

**コスト**: `JSON`／`JSONB` 列に UNIQUE を宣言した場合、テナント走査の 1 行
ごとに JSON の再パースが入るため、計算量は D1 の O(テナント行数) に加えて
O(JSON サイズ) の係数が乗る（既知の制約として記録するのみで、本 Issue では
対処しない）。

**前方互換**: 新しい型を含む UNIQUE 制約を持つカタログ値（v6）を旧バイナリが
読むと、`validate_schema` の型判定（`is_unique_constraint_allowed` 拡張前の
`is_primary_key_allowed` 相当）で `CatalogError::Invalid` となり fail-closed に
拒否される。カタログのバイト列形式自体は変えていないため、前方互換が無いのは
新しい型の**受理判定**の差分のみ。

## スコープ外・申し送り

- SQL `ALTER TABLE ... ADD [CONSTRAINT] UNIQUE` / `DROP CONSTRAINT` と制約名は
  Issue #1067 で実装済み。詳細は
  [alter-table-unique-constraint.md](./alter-table-unique-constraint.md) 参照
- 永続一意索引（redb 二次テーブル）による O(log n) 判定
- `UPSERT` の `ON CONFLICT` 対象列への UNIQUE 列拡張は実装済み（TABLE-16、
  Issue #1074。設計判断は `docs/design/sql-upsert.md`「ON CONFLICT 対象の
  UNIQUE 制約列への拡張」節参照）。`PRIMARY KEY` 宣言列を対象にすることは
  引き続きスコープ外（`42601`）。
- `PRIMARY KEY`／`FOREIGN KEY` 参照元列の対象型拡張（Issue #1073 は UNIQUE
  のみを拡張し、PK・FK は据え置き。D3・D7 参照）
- SQL `CREATE TABLE` での `REAL`・`NUMERIC`・`JSON`・配列型の列型受理・
  SQL `ALTER TABLE ADD UNIQUE`（別課題の SQL 表層拡張。TABLE-13／TABLE-14）
- TABLE-14 の複合型等価述語（`=`）の実装（`json::canonical_equality_text` を
  再利用する前提。D7 参照）
- JSON 文字列値の Unicode 正規化（NFC 等）
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
  `alter_table_drop_column` の依存検査。Issue #1073: 拡張型（`Storage::
  create_table` ＋ `alter_table_add_unique_constraint` で宣言し、書き込みは
  SQL `INSERT`／`UPDATE`／UPSERT 経由）の `-0.0`／`0.0`（`REAL`／`DOUBLE
  PRECISION`）・末尾ゼロ表現の揺れ（`NUMERIC`）・キー順／空白／数値表現の揺れ
  （`JSON`／`JSONB`）を同一値として検出、配列の要素順区別、NULL 許容・
  テナント境界・UPDATE 自己代入・バッチ内重複の拡張型版、および
  `alter_table_add_unique_constraint` が値として等価だがテキストが異なる
  既存 JSON 行の重複を検出して制約を永続化しないこと（副作用ゼロ）
- `crates/engine/tests/incremental_index.rs`（Issue #1072）: UNIQUE 制約付き
  テーブルへのファイル形 INSERT の同一パス再送成功・違反時のロールバック
  （旧チャンク復元・台帳未記録）・複数チャンクファイルの宣言どおりの `23505`・
  `body` を含む UNIQUE での複数チャンク成功・NULLS DISTINCT・テナント境界
- `crates/engine/src/constraint.rs` 単体テスト: NULLS DISTINCT・複合キーの完全
  一致判定とテナント境界・制約追加前の既存行重複判定。Issue #1073:
  `push_canonical_component` の `REAL`／`DOUBLE PRECISION` の `-0.0`
  正規化・非有限値拒否、`NUMERIC` の末尾ゼロ正規化、JSON・配列列の UNIQUE
  制約結合テスト（`enforce_unique_keys_in_txn` 経由）
- `crates/engine/src/json.rs` 単体テスト: Issue #1073
  `canonical_equality_text` のキー順・空白・エスケープ・数値表現（整数・
  小数点・指数・`i128` の桁数上限を超える巨大な指数を含む）の揺れの同一視、
  異なる値の区別、無効な JSON・上限超過の拒否
- `crates/engine/src/catalog.rs` 単体テスト: v6 の往復（主キー・`DEFAULT`・墓標と
  の併存を含む）・v2〜v5 のバイト列不変・`validate_unique_constraints` の拒否・
  v6 破損値の `CorruptSchema` 拒否・`catalog_value_references_enum_type` の v6
  検証。Issue #1073: `validate_unique_constraints_accepts_extended_types`
  （拡張型の受理）・`validate_primary_key_still_rejects_extended_unique_only_
  types`／`validate_foreign_keys_still_rejects_real_referencing_column_
  after_unique_extension`（PK・FK は据え置きの固定回帰）・拡張型を含む v6 の
  往復
- `crates/engine/src/sql/allowlist.rs` 単体テスト: `UNIQUE` 構文の受理・拒否形、
  上限ちょうどの列＋表制約（先頭・中間・末尾）の受理、表制約の前・後ろ・間に
  超過列がある場合の `54000`、制約数・制約あたり列数の上限
