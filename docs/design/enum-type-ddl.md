# ENUM 型 DDL（CREATE TYPE ... AS ENUM／DROP TYPE）の SQL 表層公開

Issue #1194。ポインタ: TABLE-14・SQL-23・ERR-6・TASK-198（spec 本文は転記しない）。

## 背景

名前付き ENUM 型（Issue #890）は `Storage::create_enum_type`／`drop_enum_type` の
Rust API のみで、SQL から型を定義できず、`DROP TYPE` の依存列拒否（`2BP01`）も
SQL から発生しなかった。本 Issue で `sql::ddl` へ結線した。

## 受理する構文

- `CREATE TYPE <name> AS ENUM ('<label>'[, '<label>']*)`
- `DROP TYPE <name>`
- 上記以外（`IF [NOT] EXISTS`・`CASCADE`／`RESTRICT`・スキーマ修飾名・複合型・空リスト・
  非文字列リテラルのラベル・`$n`・`ALTER TYPE`）は構造検証で `42601`。
- 型名は識別子形状（`validate_identifier`）のみ構文検証段で確認し、大文字小文字は区別して保持する。
- ラベル数は push 前に `MAX_ENUM_LABELS`（256）で打ち切り `54000`。ラベルの重複・空・
  長さ・組み込み型名との衝突はカタログ側 `create_enum_type` の検証に一本化し `42601`。

## 判定順序（fail-closed）

1. 構文検証（カタログ非参照）→ `42601`／`54000`
2. DDL 実行権限（`require_ddl_permission`、`--ddl-allowed-users`）→ 型の有無を問わず `42501`
3. 実行本体（単一 write txn 内で存在・依存を判定。TOCTOU なし）

ENUM 型は全テナント共有カタログで `PolicyContext` を取らない。RLS 判定は変更しない。

## エラー写像

| CatalogError | CREATE TYPE | DROP TYPE |
| ------------ | ----------- | --------- |
| `TypeAlreadyExists` | `42P07` | - |
| `TypeNotFound` | - | `42704` |
| `DependentObjectsStillExist` | - | `2BP01` |
| `Invalid` | `42601` | `42601` |
| `WriteLockTimeout` | `55P03` | `55P03` |
| その他 | `XX000`（固定文言） | `XX000`（固定文言） |

エラー文言は型名のみで、依存テーブル名・ラベル・テナント情報を含めない。

## wire・トランザクション

- `CommandComplete` タグは `CREATE TYPE`／`DROP TYPE`（件数なし）。
- Describe は結果列なし。拡張クエリの `$n` は `42601`。
- 複数文メッセージで最後以外は `0A000`、明示トランザクション内は `0A000`（他の DDL と同じ）。

## 既知の制限・対象外

- 型名の重複は既存の `42P07` へ写像した（PostgreSQL の `42710` ではない。ERR-6 ポインタ）。
- 型数上限（`MAX_ENUM_TYPES`）超過はカタログが `Invalid` を返すため `42601`（`54000` ではない）。
- `ALTER TYPE ... ADD VALUE` の SQL 公開、`CREATE TABLE` 列定義での ENUM 型名、NoSQL 表層での型 DDL は対象外
  （ENUM 列は `ALTER TABLE ... ADD COLUMN` で宣言する）。

## 破壊的変更

`SqlOutcome` に `CreateType`／`DropType` variant を追加（網羅 match の追随が必要）。
