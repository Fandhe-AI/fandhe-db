# UPDATE／DELETE の行トリガ（TABLE-45・Issue #1725）

Issue #1725（親 #1720）の実装記録。上位の決定は [trigger-execution-model.md](trigger-execution-model.md)
（D3・D4・D5・D7・D8・D10）、INSERT の経路は [trigger-before-insert.md](trigger-before-insert.md)・
[trigger-after-insert.md](trigger-after-insert.md)、定義の保存は [trigger-catalog.md](trigger-catalog.md)。
仕様は spec 側の TABLE-45・SQL-32 をポインタとして参照する（本文は転記しない）。

## 範囲

- SQL の `UPDATE`（id 指定形・述語形・サブクエリ形）と `DELETE`（id 指定形・述語形・サブクエリ形）が、
  書き込みテナント自身の BEFORE ROW・AFTER ROW トリガを発火する（autocommit・明示 Tx とも）。
  OLD・NEW・`TG_OP`・`TG_WHEN` は `TriggerCall` に載る
- 結合形（`UPDATE ... FROM`・`DELETE ... USING`）は内部の実行器（`execute_joined_dml_in`）だけがトリガに
  対応している。SQL の dispatch は未結線で、SQL から送ると従来どおり `0A000`（束縛後に拒否）になる。
  結線は #1757 以降の後続で行う
- `SET n = NULL`・既定値の無い NOT NULL 列への `SET n = DEFAULT` も、対象表に UPDATE のトリガがあるときは
  束縛時に `23502` にせず、BEFORE が補正した後の最終 NEW だけを検査する（束縛区間の
  `DeferNotNullBindScope`。トリガの無い表は束縛時の `23502` のまま）
- `UPDATE OF <列>` は、その列が `SET` の対象として書かれた文でだけ発火する。BEFORE による書き換えは
  判定に含めない（D7）
- 分割実行 DML（`... PARTITIONED`）は、対象表に該当イベントのトリガ（行・文を問わない）が 1 つでもあれば
  `0A000`。ジョブ記録・台帳照合より前に判定するので、第 1 チャンクでは commit 前に止まり副作用はゼロ（D8）

## 経路ごとの扱い

| 経路 | 扱い |
| ---- | ---- |
| SQL の UPDATE／DELETE（上記 3 形。ビュー経由・データ変更 CTE の副文も同じフォーム関数を通るため INSERT と同様に発火する） | 発火 |
| 分割実行 DML | 該当イベントのトリガがあれば `0A000` |
| `RETURNING` 付きの UPDATE／DELETE | 該当イベントのトリガがあれば `0A000`（最終値の投影は後続） |
| UPSERT（`ON CONFLICT`） | INSERT・UPDATE のどちらかのトリガがあれば `0A000`（`DO UPDATE` を発火させるのは後続） |
| NoSQL の更新・削除、生の行 API、`replace_typed_rows_by_text_key` | 該当イベントのトリガがあれば `0A000` |
| 参照アクション（CASCADE・SET NULL・SET DEFAULT）が及ぶ子表に該当イベントのトリガ | `0A000`（カタログの FK 宣言だけから静的に判定。親にトリガが無くても拒否する）。対象表自身（自己参照 FK）は子表に数えず、参照アクションで書き換わる同表の別行ではトリガが発火しない（既知の制限） |
| 文トリガ（UPDATE・DELETE の STATEMENT） | `0A000`（#1726） |
| `WHEN` 付き | `0A000` のまま（発火時にも二重に拒否。評価は後続） |

判定はすべて、同じ write txn の中でカタログだけを見て行う。書き込みテナントの scope のトリガだけを見るので、
他テナントの定義・行の有無は応答の差に出ない。文言は固定の英語で、トリガ名・表名・値を含めない。
トリガが 1 件も無い環境では、トリガ表の空判定だけで従来経路へ進む（子表の走査も行わない）。

## 実行の順序

トリガのある表に限り、行ストアの可変ハンドルを開いたまま候補を回す従来の適用段を、
`tenant/trigger_dml.rs` の行ごとループへ差し替える（BEFORE を表ハンドル無しで呼ぶ D5 の不変条件のため）。
トリガの無い表は従来の適用段のままでバイト不変。

1. 従来どおり: スキーマ照合 → `SET` 検証 → トリガの分類（台帳より前）→ 台帳の記録 → 候補の列挙 → 上限判定。
   再送は `23505`／`22023` で返り、トリガは 1 つも呼ばない。台帳のハッシュは外側の要求だけが材料（D10）
2. 行ごとに、割り込み（`57014`）→ OLD を読む → UPDATE は `SET` を適用した NEW の候補を作る
3. BEFORE を名前のバイト順に連鎖する。`NULL` 返却の行は書かず、影響行数にも AFTER にも数えない。
   DELETE の BEFORE は返却行を使わない（D3）
4. UPDATE は最終 NEW を検査（42804・23502・VECTOR の次元）して書く（可視性・行キーは既存行のまま）。
   DELETE は行を消す
5. 全行の後に制約検査・FK 検査（参照元側・参照先側）を 1 回ずつ行う。BEFORE UPDATE トリガがある表では、
   BEFORE が `SET` に無い主キー・UNIQUE 列を書き換えうるため、参照先側は全列を更新済みとして検査する
   （fail-closed）
6. AFTER を、変更の順 × トリガ名のバイト順で発火する。UPDATE は `(OLD, 最終 NEW)`、DELETE は OLD のみ。
   戻り値は無視する（D3）
7. トリガが書いた表の遅延 FK を、INSERT と同じ規則（autocommit は文末、明示 Tx は COMMIT 時）で確定する

AFTER の失敗は文全体の失敗で、同じ write txn の単一 commit なので行・トリガが書いた行・台帳・索引は残らない。
世代は、1 行以上変更したときか、トリガが別表へ書いたときに進める（id 指定形の UPDATE は従来どおり常に進める）。

## PostgreSQL との差

- 同じ文で複数トリガが同じ行を触る場合の自己変更の衝突（27000。D6）は今回入れない。
  `TriggerWriteHandle` は `insert_row` しか持たず、外側の文の対象行をトリガが変更・削除する状態を作れない
  ためで、窓口に UPDATE／DELETE が入る #2226 に引き継ぐ
- 制約検査・FK 検査は全行の書き込み後にまとめて行う（BEFORE と AFTER の間）

## 引き継ぎ

- UPSERT の発火、`RETURNING` の最終値の投影、`WHEN` の評価（NEW／OLD の列修飾の束縛を要する）
- 結合形 DML の SQL dispatch の結線（現状は `0A000`）
- 文トリガ（#1726）、入れ子の深さ上限（#1727）、自己変更の衝突 27000 と窓口の UPDATE／DELETE（#2226）
- 分割実行の途中のチャンクで DDL によりトリガが追加された場合は、既存の部分完了の写像に従う
