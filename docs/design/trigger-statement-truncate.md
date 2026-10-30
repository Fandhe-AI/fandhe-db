# 文レベルトリガと TRUNCATE トリガ（TABLE-45・Issue #1726）

Issue #1726（親 #1720）の実装記録。上位の決定は [trigger-execution-model.md](trigger-execution-model.md)
（D1・D3・D4・D5・D8・D10）、行トリガの経路は [trigger-before-insert.md](trigger-before-insert.md)・
[trigger-after-insert.md](trigger-after-insert.md)・[trigger-update-delete.md](trigger-update-delete.md)、
定義の保存は [trigger-catalog.md](trigger-catalog.md)。仕様は spec 側の TABLE-45・SQL-32 をポインタとして
参照する（本文は転記しない）。

## 範囲

- FOR EACH STATEMENT のトリガ（BEFORE／AFTER）が、INSERT・UPDATE・DELETE・TRUNCATE の各文について
  **1 文につき 1 回**発火する。対象が 0 行でも発火する（PostgreSQL と同じ）
- TRUNCATE イベントのトリガ（文単位だけ）は、複数表・`CASCADE` では対象の表ごとに発火する。DELETE トリガは
  発火しない（PostgreSQL と同じ）
- NEW／OLD は持たない（`old_row: None`・`new_row` は空。D1）。戻り値は無視する（D3）。`TG_LEVEL` は
  `TriggerCall::tg_level()`、`TG_OP` に `TRUNCATE` を追加した
- 遷移テーブル（REFERENCING）・INSTEAD OF・CONSTRAINT TRIGGER・DEFERRABLE・TRUNCATE の行トリガは、定義の段で
  `0A000`（#1722 で実装済み。`sql::allowlist::trigger` の `unsupported_forms_are_0a000` が回帰を担保する）

## 1 文のライフサイクル（D4 の段との対応）

| 段 | 内容 | 実装位置 |
| -- | ---- | -------- |
| 1 | 事前照合・台帳の記録 | 各フォーム関数（`tenant.rs`）。トリガの分類は台帳より前 |
| 2 | BEFORE STATEMENT | 台帳の記録の後・行ストアを開く前・対象の列挙の前（`trigger_dml::fire_before_statement`） |
| 3〜5 | 対象の確定・BEFORE ROW・書き込み | 従来どおり（`trigger_insert`・`trigger_dml`） |
| 6 | AFTER ROW | 同上 |
| 7 | AFTER STATEMENT | `write_rows_with_triggers`・`{update,delete}_rows_with_triggers` の末尾（`settle_trigger_written_tables` の前） |

`TriggerWriteHandle` は文ごとに 1 個で、フォーム関数が作って BEFORE STATEMENT・行トリガ・AFTER STATEMENT に
共有する。文トリガが書いた表も、遅延 FK の確定（Autocommit は即時、明示 Tx は COMMIT 時）の対象になる。
文トリガしか無い表も `plan_row_triggers` は `Some` を返し、従来経路へ落とさない（黙って発火しないのを防ぐ）。

## 経路ごとの扱い

| 経路 | 扱い |
| ---- | ---- |
| INSERT（単一・複数行 VALUES・INSERT ... SELECT） | 発火。INSERT ... SELECT が 0 行でも台帳の記録の後に 1 回ずつ発火 |
| UPDATE／DELETE の id 指定形 | 発火。不在・他テナント・不可視の id でも空の対象で同じ経路を通り、呼び出し列と応答が同じ |
| UPDATE／DELETE の述語形 | 発火。BEFORE STATEMENT が書いた行は対象に入り、AFTER STATEMENT が書いた行は入らない（D5） |
| 結合形 UPDATE の内部実行器 | UPDATE 文トリガがあれば `0A000`（S10: SET 列が列挙の後でしか確定せず、BEFORE STATEMENT を前に置けない） |
| TRUNCATE（Autocommit・明示 Tx） | 発火（下記） |
| 公開の `sql::exec::execute_truncate`・RETURNING 付き・UPSERT・分割実行・NoSQL・生の行 API・参照アクションが及ぶ子表 | 文トリガ（TRUNCATE を含む）があれば `0A000`（通過証を渡さない） |
| 一時表 | トリガを定義できない（定義の段で `0A000`） |

## TRUNCATE の順序

PostgreSQL と同じく、全表の BEFORE → 削除 → 全表の AFTER の順にする。

1. 対象の閉包（明示した表の順 → `CASCADE` で連鎖した表）→ RESTART IDENTITY の InTxn 拒否 → 全表のスキーマ
2. 全表のトリガの分類（`plan_truncate`。台帳より前）。通過証が無い経路で該当があれば `0A000`
3. 全表の台帳記録（再送・内容不一致はトリガを 1 つも呼ばずに `23505`／`22023`。D10）
4. 全表の BEFORE TRUNCATE（閉包の並び × トリガ名のバイト順。表ハンドルは開いていない）
5. パス 1（行の削除・索引の掃除）→ パス 2（FK 検査・世代 bump）
6. 全表の AFTER TRUNCATE → 書き込んだ表の遅延 FK の確定 → SEQUENCE のリセット

台帳の記録をパス 1 のループから前へ出した（トリガの有無にかかわらず常に）。先頭の表で `23505`／`22023` に
なれば全体が abort するので、結果は従来と等価。明示 Tx の TRUNCATE は `TriggerWrittenTablesScope` で囲み、
トリガが書いた表を `mark_written` する（漏れると遅延 FK の COMMIT 時検査が取りこぼす）。

## PostgreSQL との差分

| 差分 | 理由 |
| ---- | ---- |
| `RESTART IDENTITY` と TRUNCATE トリガの組み合わせは `0A000` | AFTER TRUNCATE 内の INSERT による採番が、同じ txn でのリセットとメモリ上の現在値の破棄（commit 後）と食い違いうる。後続の候補 |
| 結合形 UPDATE の文トリガは `0A000` | 上記 S10。SQL の dispatch はもともと未結線 |
| `INSERT ... SELECT`・サブクエリ形の内側の評価は write txn より前に行われうる | BEFORE STATEMENT の書き込みが内側の評価から見えない場合がある。発火の回数は守るので fail-open ではない |
| ビューへの文トリガは `42809` のまま（D8 は `0A000` を想定） | どちらも拒否。SQLSTATE の統一は範囲外 |
| 文トリガの `WHEN` は発火時にも `0A000` | 定義時の拒否の二重化 |

## セキュリティ上の性質

- 文トリガも書き込みテナントの `PolicyContext` と同じ write txn で実行し、分類は書き込みテナントの scope の
  カタログだけで行う。他テナントのトリガ・行の有無で呼び出し列や応答が変わらない
- NOT NULL の延期は BEFORE の**行**トリガがある場合に限る。文トリガは NEW を書き換えられないので、延期すると
  `23502` と `UPDATE 0` の差で行の存在を漏らす
- 文トリガの呼び出しごとに割り込み（`57014`）を確認する。文トリガは 1 文 1 回なので行数に比例しない。
  入れ子は `TriggerWriteHandle` の既存ガードで発生しない（深さの上限は #1727）
- エラー文言は固定の英語で、トリガ名・表名・値・件数を含めない

## 対象外・引き継ぎ

- UPSERT・RETURNING 付き・NoSQL・COPY・分割実行・参照アクションの子表での文トリガの発火（`0A000` のまま）
- 結合形 UPDATE の文トリガ（SQL の dispatch の結線と一緒に行う）
- `RESTART IDENTITY` と TRUNCATE トリガの組み合わせ
- 入れ子の深さの上限（#1727）、台帳と crash-test の整合（#1728）、自己変更の衝突 `27000` と窓口の拡張（#2226）、
  トリガ関数の実体と SQL E2E（#2228）
