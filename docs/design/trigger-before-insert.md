# BEFORE ROW INSERT トリガの発火（TABLE-45・Issue #1723）

- ステータス: 実装記録（発火側の意味論は engine 水準で固定。SQL だけで通す E2E は #2228 の後）
- 対応 Issue: #1723（親 #1720。前提 #1722・実行モデルの ADR は [trigger-execution-model.md](trigger-execution-model.md)）
- 仕様ポインタ: `docs/spec/04-behavior/` の TABLE-45（本文は転記しない）

## 範囲

行を挿入する前に呼ばれるトリガ（BEFORE ROW INSERT）が、挿入する行（NEW）を書き換える、または `NULL`
を返してその行の挿入をやめる、という PostgreSQL 17 と同じ意味を engine に入れる。実行モデルの D2（DEFAULT
の順序）・D3・D4 の段 1〜4・D5・D8（ガードの土台）のうち INSERT に関する部分を担当する。

トリガ関数の実体（`RETURNS trigger` の PL/pgSQL 関数）は #2228 の担当で、まだ無い。したがって
`catalog::trigger::returns_trigger` は常に `false` のままで、SQL の `CREATE TRIGGER` は `42P17` で止まる。
本 Issue は「呼び出し口」のトレイトだけを定義し（`trigger::call`）、既定の実装は固定文言の `0A000` を返す
（黙って発火しない）。意味論はテストが注入した呼び出し口と、`create_trigger_for_test`（テスト専用の
定義注入）で固定する。

## モジュール構成

| ファイル | 役割 |
| -------- | ---- |
| `trigger/call.rs` | `TriggerCall`（NEW・`TG_ARGV` 等）・`TriggerOutcome`（`Proceed`／`Skip`）・`TriggerFunctionInvoker`・未結線の既定実装 |
| `trigger/runtime.rs` | `TriggerRuntime`（呼び出し口の通過証。`EngineCore` が 1 つ持つ） |
| `trigger/plan.rs` | 書き込み txn 内でカタログを 1 回読み、INSERT トリガを分類する |
| `trigger/fire.rs` | BEFORE ROW の連鎖実行と返却行の構造検査 |
| `tenant/trigger_insert.rs` | トリガのある表の行書き込みループ（行ごとの制約検査・全行後の FK 検査） |

## 経路の割り当て

| 経路 | 扱い |
| ---- | ---- |
| SQL `INSERT ... VALUES`（単一行・複数行。autocommit・明示 Tx） | 発火 |
| `INSERT ... SELECT` | 発火（BEFORE ROW。AFTER ROW は [trigger-after-insert.md](trigger-after-insert.md)） |
| `INSERT ... RETURNING` | INSERT トリガのある表は `0A000`（最終値での投影は後続。下記） |
| UPSERT（`ON CONFLICT`） | INSERT トリガのある表は `0A000`（#1725 で結線） |
| NoSQL 表層・ファイル形 INSERT・生の `RowInput` 挿入・ビュー経由・データ変更 CTE の副文 | INSERT トリガのある表は `0A000` |
| 一時表・CTAS | 通過証なし（トリガの無い表のみ。ガードは常に通る） |

`0A000` は書き込み txn の中でスキーマ取得の直後・台帳より前に判定する（結線していない経路は台帳に触れず、
同じ `operation_id` を発火できる経路でそのまま使える）。文トリガ（INSERT）を含む表への
SQL INSERT も、#1726 で結線するまで `0A000`（AFTER ROW は Issue #1724 で結線済み）。トリガの無い表は従来経路のままで、行バイト・
ハッシュ・判定順は変わらない。

## トリガ経路の順序

1. スキーマ照合・バッチ内 `id` 重複検査・**書き換え前の値の content_hash**・台帳照合（再送は `23505`／
   `22023` で、トリガは 1 つも呼ばれない。D4 段 1・D10）
2. 省略した非 nullable 列の事前検査（`MissingNotNull`）は遅らせる（BEFORE が補えるように）
3. SEQUENCE・`id` 省略の払い出し・採番方式の rowid は BEFORE より前に確定し NEW に入れる（D2。NULL で
   スキップした行の rowid・採番値は欠番になる）
4. 行ごとに、redb の表ハンドルを開かない状態でトリガを連鎖実行（名前のバイト順。前のトリガの返却行が次の
   NEW。`NULL` 返却で残りを呼ばずスキップ）→ 最終 NEW の NOT NULL 検査（`23502`）→ VECTOR 次元検査 →
   encode → 行書き込み → その行の制約検査（索引同期・CHECK・式索引・部分索引・UNIQUE・GIN・ビュー。FK 除く）
5. 全行の後、書いた行すべての FK を 1 回検査（同じ文の後の行を参照する行を誤って `23503` にしない）
6. 1 行以上書いたら表の世代を進める。全行スキップで台帳だけ記録したなら commit（世代は進めない）

## エラー

| 状況 | SQLSTATE |
| ---- | -------- |
| 未結線の呼び出し口・未対応経路・文トリガ・`WHEN` 保存済み | `0A000`（固定文言） |
| 返却行の列数・VECTOR 列の形の不一致、行 codec が受理しない値 | `42804`（固定文言） |
| 割り込み | `57014` |
| 制約違反 | 従来どおり（`23502`・`23505`・`23514`・`23503` 等） |
| トリガ関数が返したエラー | 内側の `SqlSurfaceError` のまま透過 |

公開 enum `TenantWriteError` に `TriggerFailed(SqlSurfaceError)` を追加した（BREAKING。
[error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md) に従う）。`Display`／`Debug` は
値・行・トリガ名・テナントを出さない。

## テナント境界

発火するのは書き込みテナント自身の scope のトリガだけ（カタログが作成テナント単位。他テナントの定義は
構造上引けない）。NEW に入るのは利用者の列だけで、テナントのキー・rowid・可視性・`id` 疑似列は入れず、
書き換えもできない。実行モデルの D9 のうち「共有 `public` 表のトリガを全テナントの書き込みで発火する」
部分は、他テナントの関数を書き込みテナントの文脈で実行することになりテナント境界に触れるため、本 Issue
では実装しない（オーナー・spec 側の判断事項）。

## PostgreSQL との差分（fail-closed 側）

- 明示的な `NULL` を非 nullable 列へ入れる文は束縛時に `23502`（BEFORE で補えない）。補えるのは省略した列のみ
- `INSERT ... SELECT` の省略した非 nullable 列も束縛時に拒否される（BEFORE で補えない）
- VECTOR 列の省略は束縛時に拒否（既存）
- NEW に `id` 疑似列が無い
- SEQUENCE は全行分を BEFORE より前に払い出す（PostgreSQL は行ごと）
- トリガが返した行の型・長さの違反は `42804`（PostgreSQL の `22001` 等とは異なる）

## 後続への引き継ぎ

- トリガ関数の実体・`returns_trigger` の結線・SQL E2E は #2228。**実体を結線する前に、入れ子の深さ上限
  （D11・#1727）が要る**（同じ表に INSERT するトリガは無限再帰になりうる）
- `RETURNING` の最終値での投影（txn 内の投影コールバック）、文トリガ（#1726）、
  UPSERT・UPDATE／DELETE（#1725）、crash-test と越境マトリクス（#1728）
- D9 の共有 `public` 表の全テナント発火（オーナー・spec 側の判断）
