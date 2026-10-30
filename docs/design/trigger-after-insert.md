# AFTER ROW INSERT トリガの発火（TABLE-45・Issue #1724）

Issue #1724（親 #1720）の実装記録。上位の決定は [trigger-execution-model.md](trigger-execution-model.md)
（D3・D4・D5・D9・D10）、BEFORE ROW の経路は [trigger-before-insert.md](trigger-before-insert.md)、
定義の保存は [trigger-catalog.md](trigger-catalog.md)。仕様は spec 側の TABLE-45 をポインタとして参照する
（本文は転記しない）。

## 範囲

- SQL `INSERT ... VALUES`（単一行・複数行。autocommit・明示 Tx）と `INSERT ... SELECT` が、書き込み
  テナント自身の AFTER ROW トリガを文の終わりに発火する
- 発火しない経路（UPSERT・RETURNING・NoSQL・ファイル形・生の行挿入・ビュー経由・データ変更 CTE の
  副文）は、INSERT トリガのある表で従来どおり `0A000`（台帳に触れない）。文トリガは `0A000` のまま（#1726）
- 受け入れ条件: 監査表への自動記録が動く。トリガ内でエラーが起きたら文全体がロールバックされる
  （行・監査行・台帳・索引とも残らない）

## 順序（D4 段 6）

1. 台帳照合（書き換え前の値のハッシュ。#1723 のまま）→ SEQUENCE・rowid の確定
2. 行ごとに BEFORE ROW → 書き込み → 1 行の制約検査（#1723 のまま）。`NULL` 返却でスキップした行は
   AFTER のキューに積まない
3. 全行の後の FK 検査（#1723 のまま）
4. **AFTER ROW**: 書いた行の最終 NEW を、書いた順 × トリガ名のバイト順に発火する。戻り値（`Proceed`・
   `Skip`）は無視し、返却行の構造も検査しない（D3）。呼び出しごとに割り込み（`57014`）を確認する
5. トリガが書いた表の遅延 FK の扱いの確定（下記）→ 表の世代の更新

キューは関数ローカルの `Vec`（確保は `try_reserve_exact`・行数は呼び出し元がバッチ上限で検証済み）。
消化時点で redb の表ハンドルは 1 つも開いていない（D5 の再入の不変条件）。`rows_affected` は AFTER の
影響を受けない。

台帳は #1723 の位置（BEFORE より前・同じ txn）のまま変えない。同じ write txn の単一 commit なので、
AFTER が失敗すれば台帳エントリも破棄され、D10 の「全トリガの副作用の後に記録」と原子性の上で等価。

## トリガ内書き込みの窓口 `TriggerWriteHandle`

`TriggerCall::writes`（BEFORE・AFTER の両方に渡す）。トリガ本体へ raw な `redb::WriteTransaction` は
渡さない。

| 契約 | 内容 |
| ---- | ---- |
| 経路 | SQL `INSERT` と同じ `tenant::insert_typed_row_unchecked`（同じ可視性・制約検査・索引同期・世代更新）。第 2 の書き込み経路を作らない |
| テナント | 呼び出し元の `PolicyContext` を借用し差し替えられない（行キーは常に呼び出しテナントの `(tenant_id, rowid)`） |
| 台帳 | `LedgerWrite::Disabled`（外側の文の 1 エントリだけ。D10。`SubstatementLedgerScope` の共通化は #2226） |
| 入れ子 | 書き込み先に INSERT トリガがあれば `0A000`（深さ上限〔#1727〕が入るまで発火しない。自己再帰を起こさない） |
| 表名 | 裸の識別子だけ。SCHEMA 配下の内部キー（`#<id>.<name>`）は不在と同じ `42P01` で拒否する（SCHEMA 修飾の解決は #2226） |
| エラー | `SqlSurfaceError` へ既存の写像で変換し、トリガ本体 → 発火側へ透過（最終的に `TriggerFailed`） |

本番の結線（PL/pgSQL の SQL 文をこの窓口経由で実行するホスト）は #2226・#2228 の担当。本 Issue では
テスト注入の呼び出し口がこの窓口で監査表へ書く。

## 遅延 FK（fail-open の防止）

トリガ内の書き込みは `WriteTarget::InTxn` を通るため、`INITIALLY DEFERRED` の FK が文単位検査から外れる。

- 外側が autocommit: AFTER の後に、トリガが書いた各表で `enforce_deferred_foreign_keys_in_txn` を呼ぶ
  （autocommit は COMMIT 時検査を持たない）
- 外側が明示 Tx: COMMIT 時の遅延 FK 検査は `mark_written` した表だけが対象。`core` の `INSERT`・
  `INSERT ... SELECT` の腕が `tenant::trigger_writes::TriggerWrittenTablesScope`（スレッドローカルの
  RAII 区間）でフォーム関数を囲み、トリガが書いた表を `mark_written` する
- 区間が張られていない `InTxn` 経路（ビュー経由・一時表・データ変更 CTE 等）では、文の終わりで即時検査に
  倒す（PostgreSQL より厳しいが fail-open にならない）

## エラー

新しい SQLSTATE・公開型は追加しない（`TenantWriteError::TriggerFailed` は #1723 で追加済み。非 BREAKING）。

| 状況 | SQLSTATE |
| ---- | -------- |
| 文トリガ・`WHEN` 付き・未結線の呼び出し口・未対応経路・入れ子の発火 | `0A000`（固定文言） |
| AFTER 内の書き込みの制約違反 | 従来の SQLSTATE（`23505`・`23503` 等） |
| トリガが返したエラー | 透過（autocommit は文全体を取り消し、明示 Tx は Failed → `25P02`） |

## PostgreSQL との差分

- FK 検査を全ユーザー AFTER ROW トリガより前にまとめて行う（実行モデルに既記載）
- トリガ内の書き込み先に INSERT トリガがある場合は `0A000`（PostgreSQL は入れ子で発火）
- AFTER の `WHEN` は `0A000`（実行モデルの引き継ぎ表では D7 の AFTER 側が #1724 に割り当てられているが、
  カタログ DDL が `WHEN` を `0A000` で拒否しており、評価は #1725 の作業項目に含める）
- 区間外の明示 Tx 経路では、トリガが書いた表の遅延 FK を文の終わりに検査する

## 後続への引き継ぎ

- トリガ関数の実体・PL/pgSQL の SQL 文の実行ホスト・`SubstatementLedgerScope` の共通化・SQL E2E は
  #2226・#2228。実体を結線する前に入れ子の深さ上限（D11・#1727）が要る
- `WHEN` の評価・UPDATE／DELETE・UPSERT は #1725、文トリガ・TRUNCATE は #1726
- crash-test・越境マトリクス・台帳との整合の強化は #1728
- `RETURNING` の最終値投影・共有 `public` 表の全テナント発火（D9）は要判断
