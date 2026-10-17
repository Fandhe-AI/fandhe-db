# ALTER TABLE の ADD COLUMN IF NOT EXISTS と複数アクション

対象ビヘイビア（ポインタのみ）: TABLE-36（`ADD COLUMN IF NOT EXISTS`）、TABLE-39 (5)（複数アクションの原子性）。
関連: TABLE-38 (6)（RENAME との混在）。いずれも `docs/spec/04-behavior/data-model.md`。実装 Issue は #1633。

## 決定事項

| 項目 | 決定 |
| ---- | ---- |
| `IF NOT EXISTS` の位置 | `ADD COLUMN` の直後（`ADD COLUMN IF NOT EXISTS <列> <型>`）。`COLUMN` 省略形は未対応のまま |
| skip 判定 | 実行段で、書き込み前のスキーマに同名の生存列があれば成功扱い。型解決（ENUM 未登録 `42601`・VECTOR `0A000`）と DEFAULT の束縛より前に判定する。墓標（DROP 済み列）は名前空間に入らないので追加へ進む |
| skip 時の副作用 | なし（commit しない。カタログ・行・台帳・表の世代とも不変）。NoticeResponse は送らない（wire-server に Notice の送出基盤が無い。#1613・#1614 と同じ） |
| 不変にする拒否 | 予約列名（`42601`）・表の不在（`42P01`／`42809`）・DDL 権限（`42501`）は `IF NOT EXISTS` でも緩めない。構造検証はカタログを参照しないため存在オラクルにならない |
| 複数アクションの構文 | `ALTER TABLE <表> <アクション>, <アクション>, ...`。1 個なら従来の単一 variant、2 個以上なら `ValidatedAlterTable::Multiple` |
| 複数に含められる種別 | 列単位のカタログ操作のみ: ADD COLUMN（`IF NOT EXISTS` 可）・DROP COLUMN・SET／DROP DEFAULT・SET／DROP NOT NULL |
| 含められない種別 | RENAME は `42601`。ADD UNIQUE／CHECK／FOREIGN KEY／PRIMARY KEY・DROP CONSTRAINT・ALTER COLUMN TYPE は `0A000`（固定文言）。いずれも構文段で判定 |
| アクション数上限 | `MAX_ALTER_TABLE_ACTIONS`（= `MAX_COLUMN_COUNT`）。超過は確保前に `54000` |
| 原子性 | `Storage::alter_table_in_single_txn` の単一 write txn。途中の失敗は txn を破棄し何も適用しない。世代 bump は commit 直前に 1 回だけ。全アクションが no-op なら commit しない |
| 適用順 | PostgreSQL の `AlterTablePass` に倣う pass 順（DROP COLUMN → ADD COLUMN → 列属性変更）。列属性変更（SET/DROP NOT NULL・SET/DROP DEFAULT）は同一 pass で記述順に適用する。応答の `actions` は記述順 |
| エラー写像 | 各アクションは単一アクション経路と同じ `map_*` で SQLSTATE を写す |

## 実装上の要点

- `Storage::alter_table_*` は各自でゲート付き write txn を開く。同一スレッドがゲートを再取得すると `55P03` になるため、
  対象 5 操作の本体を `*_in_txn`（begin・世代 bump・commit を含まない）へ切り出し、単独 wrapper と複数アクション経路が共有する。
  wrapper の挙動（検査順序・エラー・冪等 no-op は commit しない）は変えない。
- `*_in_txn` が返す `AlterApply::{Applied, Unchanged}` で「カタログを変えたか」を伝える。
- 前のアクションの結果を見る必要があるため、スキーマ参照は `get_table_schema_in_txn`（write txn 内）で行う。
- 同じ列への複数アクション: 重複 ADD は 2 個目で `42701`、重複 DROP は `42703`、DROP と ADD の同名は DROP が先に適用され新しい列で置き換わる（旧値は復活しない）。
  `ADD COLUMN x, SET NOT NULL x`（DEFAULT なし）で既存行がある場合の `23502` は 2 文に分けた場合と同じ挙動で、新たな存在オラクルではない。

## スコープ外

- 複数アクションへの ALTER COLUMN TYPE・ADD／DROP CONSTRAINT・ADD PRIMARY KEY の参加（`0A000` のまま）
- `ADD` の `COLUMN` 省略形、`ALTER TABLE IF EXISTS`、`DROP COLUMN IF EXISTS`、`DROP CONSTRAINT IF EXISTS`
- NoSQL 表層 `alter_table` の if_not_exists・複数アクション
- TABLE-36 (2) の NoticeResponse
