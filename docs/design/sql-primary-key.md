# `PRIMARY KEY` 宣言構文（単一列・複合キー）の設計判断

Issue #903・対象ビヘイビア: TABLE-16（TASK-204）。関連ポインタ: TABLE-12（行ストア
物理キー `(tenant_id, id)`）・RLS-9／RLS-10 (c)（テナント境界・制約検査のテナント
境界）・ERR-6（`23505`／`UNIQUE_VIOLATION`）・RECOVER-12（台帳照合と制約検査の
順序）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメント
は本リポ側の実装判断・設計記録のみを扱う。

## 背景

行ストアの物理キーは `(tenant_id, id)` 固定で、テーブルの主キーを宣言する構文が
なく、`id` 以外の列（複合を含む）で一意性を保証する手段がなかった。`CREATE TABLE`
へ主キー宣言を追加し、宣言済み主キーの一意性をテナント内スコープで全書き込み経路
に強制する。

## `id` 暗黙主キーとの共存規約

- 物理キーは常に `(tenant_id, id)` のまま不変。`id` は引き続き全テーブルで必須・
  テナント内一意の行識別子（TABLE-12）であり、`UPDATE`／`DELETE ... WHERE id = n`・
  `ON CONFLICT (id)` のアドレッシングも `id` のまま。
- 主キー未宣言のテーブル（大多数）は `id` 暗黙主キーのまま。カタログバイト列・
  挙動とも完全不変。
- `PRIMARY KEY (id)`（`id` 単独）は暗黙主キーの明示宣言として**受理し、何も
  永続化しない**（未宣言と同一カタログ・同一挙動）。
- `id` を他列と組み合わせた複合キー（例 `PRIMARY KEY (id, code)`）は `id` 単独で
  一意性が既に成立し冗長なため `42601` で拒否する（fail-closed・曖昧さ排除）。
- 宣言済み主キー（`id` 以外の列集合）は `id` に**追加される**テナント内一意性
  制約として働く。主キー列は暗黙に `nullable = false`。
- UPSERT の `ON CONFLICT` 対象は引き続き `(id)` のみ。宣言済み主キーと衝突する
  UPSERT は本機能により `23505` で拒否される（`ON CONFLICT (<主キー列>)` 構文自体
  は対象外のまま `42601`）。

## 構文（許可リスト。`sql/allowlist.rs`）

`PRIMARY`／`KEY` は `lexer::Keyword` へ追加せず、`CREATE TABLE` の列リスト内という
文脈でのみ文脈的キーワードとして照合する（`TEXT`／`VECTOR` と同方針）。

- 列制約: `<col> <type> PRIMARY KEY`
- 表制約: `PRIMARY KEY (<col>[, <col>]*)`（複合キーを含む。列リスト中の要素として
  任意位置に置ける）
- 判定順序: 列リストの各要素を先頭から見て、`Ident("PRIMARY")` の次のトークンが
  `Ident("KEY")` なら表制約、それ以外は通常の列定義として解釈する（`KEY` は列型
  キーワードではないため、列名 `primary` を宣言する既存 SQL との構文上の曖昧さは
  生じない）。
- 拒否（いずれも `42601`。決定的な構造検証段で判定し、権限ゲート・カタログ照会より
  前に確定する）:
  - 主キー宣言が 2 つ以上（列制約 × 2・列制約 + 表制約・表制約 × 2）
  - `CONSTRAINT <name> PRIMARY KEY` 形・`CHECK`／`REFERENCES`（`NOT NULL`／
    `DEFAULT` は Issue #904、`UNIQUE` は Issue #905 で受理済み）
  - 空リスト `PRIMARY KEY ()`
  - 列リストに存在しない列名（`42703` は `ErrorClass` 未実装のため既存の
    未知列扱いに揃える）
  - `id` と他列の混在
  - 主キー対象外の型（`VECTOR`。TABLE-13／14 の追加型は SQL 表層の `CREATE TABLE`
    が現状 `TEXT`／`VECTOR` のみ受理するため到達しない）
  - `MAX_PRIMARY_KEY_COLUMNS`（32。PostgreSQL の索引キー列数慣習に合わせた実装
    既定値）超過（`54000`。`Vec` を確保する前に判定）
  - 同一主キー内の列名重複（`42701`）

## カタログ永続化（v4。`catalog.rs`）

主キーを宣言したスキーマは（削除済み列〔墓標〕の有無に関わらず）新フォーマット
`v4` で書く。`v4` は `v3`（TABLE-19・墓標を持つスキーマ専用）と列行の形式（5
フィールド・`state` `L`／`D`）を共有し、`cols:` 行の直後に `pk:<col>[,<col>]*`
行を 1 行追加する点のみが異なる。

```
v4
cols:<物理スロット数>
pk:<col>,<col>
<name>:<tag>:<param>:<nullable>:<state>
...
```

- 主キーを宣言しないスキーマ（墓標の有無を問わず）は従来どおり `v2`／`v3` で書き、
  既存のゴールデンテスト（`encode_schema_golden_v2_layout` 等）のバイト列は
  一切変えない。
- `v4` は墓標が 0 件でも唯一のエンコードとして成立する（`v3` が「墓標 1 件以上」を
  形式の一意性条件とするのとは独立の判断基準）。
- デコード時、`pk:` 行の欠落・空要素・未知列・重複・許可外型・`id` 混入・上限超過は
  すべて `CorruptSchema` で fail-closed に拒否する（アロケーション前に要素数を
  `MAX_PRIMARY_KEY_COLUMNS` で検証）。
- `catalog_value_references_enum_type`（`DROP TYPE`／`ALTER TYPE ... ADD VALUE` が
  使う軽量パーサー）も `v4` を認識し、`pk:` 行を読み飛ばしてから列行を走査する
  （認識しないと主キー宣言テーブルに対する `DROP TYPE` 等が構造的に失敗する）。

`TableSchema` は非公開フィールド `primary_key: Option<Vec<String>>`（列**名**で
保持。`DROP COLUMN` で物理位置がずれても安全）を持ち、`primary_key()` アクセサ・
`with_primary_key()` ビルダー（Rust API 用）を提供する。`ALTER TABLE ... DROP
COLUMN`（Rust API。TABLE-19）は主キー構成列の削除を
`CatalogError::DependentObjectsStillExist` で拒否する（`DROP TYPE` が依存 ENUM 型
を拒否するのと同じ分類）。

## テナント内一意性制約の検査点（`constraint.rs`）

単一の検査点 `constraint::enforce_unique_keys_in_txn` を新設し、`tenant.rs` の
各書き込み関数（`insert_row_unchecked`・`insert_rows_unchecked`・
`insert_typed_row_unchecked`・`insert_typed_rows_unchecked`・
`upsert_typed_rows_unchecked`・`update_row_unchecked`・
`update_row_columns_unchecked`・`update_rows_where_unchecked`・
`replace_typed_rows_by_text_key`）から、**台帳への記録・行の書き込みの後**・
**テーブル世代 bump・commit の前**に同一 write トランザクション内で呼ぶ
（RECOVER-12。台帳照合が本検査より優先されるため、`operation_id` の再送判定
〔`23505`／`22023`〕は主キー違反として誤検出されない）。

- 主キー未宣言のテーブルは `schema.primary_key()` が `None` を返し、即座に成功
  する（コストゼロ）。UNIQUE 制約（Issue #905。`docs/design/unique-constraint.md`）
  も同じ検査点で判定し、テナント範囲の走査は 1 文あたり 1 回に保つ（主キー・
  UNIQUE 制約の両方を宣言したテーブルでも走査は 1 回）。
- 判定は今回の書き込みで実際に値が変わった行の `id` 集合（`written_ids`。UPSERT
  の `DO NOTHING` は含めない）についてのみ行う。
- 各行の主キー列値を `[type_tag][u32 BE len][payload]` の型タグ＋長さ前置バイト列
  へ変換し連結した正準キーで比較する（可変長コンポーネント〔TEXT・BYTEA・ENUM〕を
  素朴に連結すると `("ab","c")` と `("a","bc")` が同一バイト列になる境界曖昧性を
  構造的に排除する）。
- 判定手順は 2 段: (1) `written_ids` 同士の主キー値衝突（同一文内で 2 行が同じ
  値を持つ）、(2) 対象テナントの残り全行（`written_ids` を除く）との衝突。(2) の
  母集合はテナントが所有する**全行**（`Public`／`Private` を問わない。可視性
  フィルタで縮めない。TABLE-16・RLS-10 (c)）で、二次索引（`ScalarIndex`）・世代
  整合キャッシュのいずれも流用しない生の redb 走査で判定する。
- 自己更新の除外は `written_ids` からの除外そのもので行い、新旧値の比較はしない
  （id が一致する限り自己衝突しない）。
- 走査は常にサーバー側導出テナント（`ctx.tenant_id()` 由来の物理キー範囲
  `(tenant, 0)..=(tenant, u64::MAX)`）に閉じ、他テナントの行キー・値には一切
  触れない。エラー応答（`TenantWriteError::UniqueViolation`）はキー値・行 id・
  テナント名・テーブル名を含まない固定文言。

### 永続一意索引（Issue #1070）

判定は `user_uniq/{table}`（redb 二次テーブル）への点照会で行い、検査コストは
対象テナントの保有行数に比例しない（1 行あたり O(k・log n)。k は宣言済み一意
キー数）。設計・不変条件・後方互換は `docs/design/unique-index.md` 参照。
`tenant::enumerate_dml_candidates` が持つ総走査上限 `MAX_SCANNED_ROWS` は意図的
に継承しない——継承すると、その上限を超える行数を既に保有するテナントが主キー
宣言テーブルへ一切書き込めなくなる過剰に fail-closed な制約になってしまうため
（既存 DB からの遅延バックフィルはテナントごとに高々 1 回であり、以降の書き込み
は索引照会のみで完結する）。

## エラー契約

- `TenantWriteError::UniqueViolation`・`SqlSurfaceError::UniqueViolation` を新設し、
  いずれも既存の `ErrorClass::UniqueViolation`（`23505`・`UNIQUE_VIOLATION`）へ
  写像する（新規 `ErrorClass` は追加しない）。行キー衝突
  （`IdConflict`／`SqlSurfaceError::IdConflict`。物理キー `(tenant_id, id)` の
  衝突）とは別 variant・別固定文言だが、`wire_code` は共有する。
- HTTP（NoSQL 表層）は既存の `ErrorClass::UniqueViolation → 409` 写像をそのまま
  再利用する（新規の HTTP 写像は追加していない）。

## 既知のギャップ（スコープ外）

- NOT NULL 違反の `23502` 統一・`DEFAULT`（Issue #904 で実装済み）、`UNIQUE`
  （Issue #905 で実装済み）、`CHECK`（Issue #906 で実装済み）、`FOREIGN KEY`（Issue #907 で実装済み。
  `docs/design/foreign-key.md`）。
- `ALTER TABLE ADD/DROP CONSTRAINT`・`ADD PRIMARY KEY`、`ON CONFLICT (<主キー列>)`、
  NoSQL 表層の DDL op。
- `42703`（未知列）・`42P16`・`2BP01` の `ErrorClass` 追加と SQL 写像。
- 台帳由来 `23505` のラベル `DUPLICATE_OPERATION_ID` 分離は Issue #1180 で解消
  （engine の `ErrorClass`・HTTP `code`。pg wire は ERR-1 の既存形式のまま）。
- `catalog.rs` の `pub(crate)` 生書き込み API（`insert_row_into_table`・
  `insert_rows_into_table`・`insert_typed_row`。いずれも `#[cfg(test)]` 専用の
  テナント境界チェックなし経路）は主キー検査点を経由しない。これらはテナント
  境界チェック自体を持たない生の経路であり、production の書き込みはすべて
  `tenant.rs` のガード付き入口（本検査点を経由済み）を通る。Issue #1078 で、
  これら 3 API 自身が主キー（UNIQUE・CHECK・FOREIGN KEY も同様）を宣言した
  テーブルへの書き込みを fail-closed に拒否するガードを追加したため、検査点を
  経由しなくてもテストがこれらの制約に違反する状態を作ることはできない。
