# ADR: 分割実行 DML（PARTITIONED）の対象表の行トリガ（Issue #3283）

- ステータス: **Accepted**
- 対応: Issue #3283（親 #3160）。前提は #3186（トリガ本体の UPDATE）・#3187（DELETE・SELECT）
- 関連: [trigger-execution-model.md](trigger-execution-model.md)（D8）・[trigger-update-delete.md](trigger-update-delete.md)・[partitioned-dml.md](partitioned-dml.md)
- 本書は private spec の本文を転記しない。参照は Issue 番号・設計記録のパス・ID ポインタ（TABLE-45・SQL-19・SQL-32・RECOVER-11）に限る

## 1. 背景

分割実行 DML（`UPDATE／DELETE ... USING OPERATION_ID '<id>' PARTITIONED [CHUNK n]`）は、対象表に該当イベントの
トリガが 1 つでもあれば `0A000` だった。通常経路の行トリガ発火（`tenant/trigger_dml.rs`）が整ったため、
発火できる形は発火し、できない形は固定文言の `0A000` を維持する。黙ってトリガを飛ばす経路は作らない（D8）。

## 2. 決定

### D1 形ごとの扱い

| 形 | 扱い | 理由 |
| -- | ---- | ---- |
| BEFORE／AFTER ROW（UPDATE・DELETE。SQL 表層） | **発火** | 各チャンクは 1 write txn で、チャンク内は通常経路と同じ「BEFORE → 書込 → 制約・FK → AFTER」を保てる |
| BEFORE／AFTER STATEMENT（該当イベント。`UPDATE OF` で外れたものを除く） | `0A000`（固定文言 `statement triggers are not supported for partitioned DML`） | 「文につき 1 回・全行の前後」を、チャンクごとに commit し再開もある非原子実行では保証できない（毎チャンクか初回・最終のみかが再開・取消・上限停止で曖昧になる） |
| 参照アクションが及ぶ子表に該当トリガがある形 | `0A000` | 通常経路と同じ静的判定（`plan_row_triggers` 内）を流用 |
| NoSQL 表層の分割実行 | `0A000` | NoSQL は未結線（D8）。共有本体は通過証を `Option` で受け、SQL 表層だけ `Some` を渡す |
| トリガ本体内の `PARTITIONED`・明示 Tx 内の分割実行 | 従来どおり拒否 | 変更しない |

判定は各チャンクの write txn 内・ジョブ記録と台帳照合より前で行う。第 1 チャンクは commit 前に止まり副作用はゼロ。
途中のチャンクで DDL によりトリガが追加された場合は次のチャンクで同じ判定が働き、既存の部分完了の写像（`VD001`）に従う。

### D2 発火回数・順序の保証範囲

AFTER ROW は各チャンクの終わり（制約・FK 検査の後）に発火するため、チャンク幅が行数より小さいと BEFORE と
AFTER の交互順は非分割実行と一致しない（非分割は全 BEFORE の後に全 AFTER）。保証は次のとおり。

- (a) 各トリガの発火回数は非分割実行と同一
- (b) 各 timing 内の行順（rowid 昇順）・トリガ名のバイト順は同一
- (c) チャンクが 1 つに収まる場合は全体の並びが完全一致

非分割実行との差分: BEFORE と AFTER の交互順はチャンク境界で変わる。後続チャンクは先行チャンクのトリガ書き込み
（別表）を見る。NEW の評価・トリガの遅延 FK はチャンク内で確定する。

### D3 実装上の規則

- 適用段は通常経路と共有する `update_rows_with_triggers`／`delete_rows_with_triggers` へ差し替える。トリガの無い表は従来の適用段のままでバイト不変
- `WriteTarget::Autocommit` を渡す。`InTxn` を渡すと遅延 FK が COMMIT まで先送りされ fail-open になるため使わない
- カーソルと `exhausted` は**消費した候補数**（`TriggeredDml::consumed`。BEFORE の `NULL` 返却でスキップした行を含む）で決め、ジョブ記録の累計・影響行数は書いた行数で数える。書いた行数で進めると再走査・トリガ再発火・全行スキップ時の `StateConflict` を起こす
- 変更 0 件でもトリガが別表へ書いた（BEFORE が書いて行をスキップした）チャンクは、停止点用の記録（件数 0・カーソル付き）を作って commit する。「記録なし・変更 0 件は txn を破棄」の既存規則に落とすと別表への書き込みが消える。世代 bump は通常経路と同じ規則（行を書いた、またはトリガが別表へ書いた）
- writer 保持時間と文の期限は、適用段が「1 件以上消費した後・候補の直前」に判定する（`should_stop`）。打ち切ったときは消費済みの行だけ制約・FK・AFTER を実行する。期限超過は従来どおりチャンク全体を破棄する。トリガ本体の実行時間は 1 行分が非中断段になる
- 分割実行は `interrupt::suppressed` 区間のため共有の確認点は no-op。期限は注入時計で評価する

## 3. セキュリティ観点

- テナント境界: トリガ本体は書込セッションの `PolicyContext` で同じ write txn 内で実行する（D9）。分類はカタログのみで、他テナントのトリガ・行の有無に依存しない。RLS ポリシーの解決は各チャンクで従来どおり行う
- fail-closed: 文トリガ・子表トリガ・NoSQL・通過証なしは固定文言の `0A000`（値・表名・トリガ名を含まない）
- DoS: トリガ深さ（`54001`）・文数／ループ予算（`54000`）の既存上限を維持する

## 4. テスト

- 単体: `core/trigger_update_delete_tests.rs`（発火回数・順序・スキップ・別表書き込み・失敗と再開・文トリガ拒否）
- 結合: `tests/table_rls/table45_trigger_partitioned.rs`（本番 invoker・テナント境界・RLS ポリシー併用）、`tests/wire_sql/wire_trigger_partitioned.rs`（簡易クエリの末尾 `;` 付き単独送信・拡張クエリ）

## 5. 引き継ぎ

- 文トリガの結線（毎チャンク発火か初回・最終のみかの選択）は、再開・取消・上限停止の意味づけを決めたうえでの後続課題
- NoSQL 表層の分割実行の発火は NoSQL 全体の結線に合わせる
- トリガ付き分割実行の crash 耐性試験は範囲外（トリガ無し構成は既存の crash-test-partitioned-dml が担う）
