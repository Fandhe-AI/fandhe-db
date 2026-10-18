# OPERATION_ID の任意化の意味付けと `--require-operation-id`

## ステータス・対象

- ステータス: 採用（設計段階。実装は #1703・#1704・#1705・#2203。RECOVER-1・HTTP-14 のポインタのみ。spec 本文は転記しない）
- 親: #1701。spec 側の改訂は #1503 で submodule に反映済み
- ビヘイビア ID: RECOVER-1（`docs/spec/04-behavior/recovery.md`）・HTTP-14（NoSQL 表層）。補助として RECOVER-2・RECOVER-7・RECOVER-10・RECOVER-11・RECOVER-12・SQL-10・ERR-2
- 関連: [operation-id-required-guard.md](operation-id-required-guard.md)・[operation-id-ledger.md](operation-id-ledger.md)・[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)・[partitioned-dml.md](partitioned-dml.md)・[now-evaluated-value-ledger.md](now-evaluated-value-ledger.md)・[resend-semantics.md](resend-semantics.md)・[explicit-transaction.md](explicit-transaction.md)・[error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)
- フラグ実装の流儀の参照: `crates/wire-server/src/dml_limits_opt.rs`・`crates/wire-server/src/ddl_permission_opt.rs`

## 背景

現状、書き込み文（INSERT／UPDATE／DELETE／TRUNCATE／COPY／一括投入）と NoSQL の書き込み op は operation_id が必須で、省略すると `23502` で拒否される（`crates/engine/src/recovery/required_op_id.rs` の `LedgerMode::require`／`resolve`）。`LedgerMode::CompareOnlyWithoutLedger` は存在するが、engine のテストからしか切り替えられず、しかも「台帳そのものを持たない」構成で、冪等保証も消える。

親 #1701 で、PostgreSQL と同様に operation_id を既定で省略可能にし（省略した書き込みは台帳に記録しない通常の書き込み）、従来の必須契約は opt-in の CLI フラグ `--require-operation-id` で選べる方針が決まった。本メモはその意味（保証の範囲・例外・各経路への影響）を確定し、後続 Issue の前提にする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | モードを 3 つに区別する。(a) 必須モード: 現行の `Ledgered`。`--require-operation-id` 指定時。(b) 任意モード（新設）: フラグ未指定時の既定。(c) `CompareOnlyWithoutLedger`: 台帳を持たない限定用途で現状維持（CLI には出さない）。任意モードは (c) の流用では実現できない。(c) は operation_id を指定しても `LedgerWrite::Disabled`・`LedgerLookup::NoLedger` になり冪等保証が消えるため |
| D2 | 判定表。`require`・`resolve`・`operation_recorded`／`last_operation_id`・分割実行 DML・NoSQL 書き込み op のいずれも、(a) は現行どおり。(b) は、指定ありなら `Record`（冪等性・`23505`／`22023` の競合検出は (a) と同じ）、省略なら台帳に触れずに書き込む（台帳なし書き込み。`Disabled` とは別の意図であることを型で表す。名前は #1704 で決める）。(b) の照会 API は台帳ありとして振る舞い、`NoLedger` を返さない。(c) は現状維持 |
| D3 | 台帳なし書き込みの保証範囲: 1 文 = 1 つの redb write トランザクションで原子的にコミットされ、crash 後は「全部適用」か「全く適用されない」かのどちらか（既存のコミット境界と同じ）。冪等性は無く、再送すると二重に適用されうる（PostgreSQL の通常の書き込みと同じ）。台帳エントリ・内容照合ハッシュ・評価済み時刻値は記録せず、`23505`／`22023` の再送判定は起こらない |
| D4 | 同じ表で台帳あり・なしの書き込みが混在してよい。結果として最終 operation_id の照会（RECOVER-7）は「最後の台帳あり書き込み」を指し、「最後の書き込み」ではない。利用者向けの注意として残す |
| D5 | 分割実行 DML は任意モードでも例外として必須のまま。SQL の `PARTITIONED`／NoSQL の `"mode": "partitioned"` で operation_id を省略すると `23502`（再開用カーソルが台帳を前提にするため）。SQL で `USING OPERATION_ID` なしに `PARTITIONED` が来た場合は `42601` ではなく `23502` で拒否する（構文判定順の実装は #1704）。(c) で `0A000` になる既存の挙動は変えない |
| D6 | モードを切り替えられるのはサーバー構成（CLI）だけ。クエリ文字列・セッション変数・SQL の `SET`・HTTP 要求・テナント単位の設定からは変えられず、プロセス全体・全テナントに同じモードが効く。差し替え点はこれまでどおり `EngineCore::with_ledger_mode` のみ（fail-closed。他の経路を作らない） |
| D7 | CLI: `--require-operation-id` は値を取らない真偽フラグ（既定 off）。既存の `*_opt.rs` はすべて値つきフラグで、値なしフラグの前例は無い。2 回目の指定は起動エラー（last-wins にしない）。`--require-operation-id=...` のように値を付けた形や未知の綴りは未知の引数として起動を拒否する。モジュールは新規 `crates/wire-server/src/require_operation_id_opt.rs`（#1703 で作成予定。`dml_limits_opt.rs` と同じ流儀でフラグ名の定数と解決関数を置く）。起動ログに解決したモードを出す。最終的な名前と配置は #1703 で決め、差が出たら本メモを追随させる |
| D8 | engine 側の既定値: `LedgerMode::default()` は必須モードのまま（ライブラリ単体では fail-closed を保ち、既存テストへの影響を抑える）。wire-server バイナリが、フラグ未指定時に任意モードを明示的に注入する。「既定で任意」はサーバーの既定を指す。#1704 で別の判断をする場合は本メモを改訂する |
| D9 | 公開 API: `pub enum LedgerMode` への variant 追加は下流の網羅的な `match` に対してソース非互換。`error-enum-non-exhaustive-policy.md` の方針に従い `#[non_exhaustive]` は後付けせず、#1704 のコミットを `feat(engine)!`（`BREAKING CHANGE:` 付き）にする |
| D10 | エラー契約は変えない。必須モードで省略したときは従来どおり `23502`（`MISSING_OPERATION_ID`）で、新しい `wire_code` は追加しない。エラー文にテーブル名・テナント・行内容を含めない現行方針も保つ |

## 影響一覧

| 経路 | 任意モードでの扱い | 担当 |
| ---- | ------------------ | ---- |
| SQL INSERT（単一行・複数行 VALUES・ファイル形）・UPDATE・DELETE（述語形を含む）・UPSERT | 省略を受け付け、台帳には記録しない（`sql/allowlist.rs` の `validate_insert` 等・`sql/exec.rs`） | #1704 |
| TRUNCATE | 省略を受け付ける。`TRUNCATE TABLE only` が表名として解釈される形（`allowlist.rs` の該当分岐）が既定の構文解析になる点に注意 | #1704 |
| COPY | `sql/copy.rs` はすでに `Option<OperationId>` を運んでいる。省略時は台帳に記録しない | #1704 |
| 一括投入 API（`core.rs` の判定 1） | 判定 1 を `ledger_mode` に従わせる | #1704 |
| `EngineCore::insert_row`／`update_row`／`delete_row` | 同上 | #1704 |
| 明示トランザクション | 文ごとに台帳への記録を判定する。トランザクション内で台帳あり・なしの文が混在してよい | #1704 |
| 分割実行 DML（SQL・NoSQL） | D5 のとおり必須のまま | #1704・#2203 |
| NoSQL HTTP の insert／update／delete | `http/query/schema.rs` のモジュール文書と `insert.rs`／`update.rs`／`delete.rs` の「欠落／`null`／空文字 → `23502`」をモードで分岐させる。指定時の `23505`／`22023` は変えない（HTTP-14） | #2203 |
| `Idempotency-Key` ヘッダ | 本文の `operation_id` と同じ台帳・同じ意味で扱う。両方省略した書き込みは台帳なし書き込みになる | #2062（Phase 8） |
| SEQUENCE／IDENTITY | 台帳なし書き込みでは採番値を台帳に残さない（再送すると別の値になる） | #1686・#1696 |
| データ変更 CTE | 同じ規則に従う前提を書き残す | #1874 |
| crash-test 群（`scripts/crash_test*.sh`・`crates/engine/examples/crash_tool*.rs`） | いずれも operation_id を指定して書き込むため、既定の切り替えの影響を受けない。台帳なし書き込みの crash 耐性確認は #1704 | #1704 |
| 回帰テスト | 省略／指定 × 任意／必須の組み合わせ、分割実行 DML、セッション・クエリからの切り替え拒否 | #1705 |

## 安全性の前提

- RLS 相当のテナント境界は変えない。台帳は従来どおりテナント・テーブル単位で、台帳なし書き込みも RLS を通る
- 省略を許すのは、PostgreSQL と同等の挙動（二重適用されうること）を明示したうえでの意図的な緩和である。切り替え点は CLI と `with_ledger_mode` に限り（D6）、不正なフラグの形と重複指定は起動エラー（D7）、ライブラリの既定は必須モード（D8）とする
- フラグは値を受け取らないため、untrusted 文字列の解析面は増えない。起動ログに解決したモードを出し、運用者の取り違えを防ぐ

## スコープ外

- コード変更（#1703・#1704・#1705・#2203 で実施）
- `README.md` の `USING OPERATION_ID` の記述、`crates/wire-server/src/main.rs` の起動ヘルプ、NoSQL API 文書の更新（それぞれ #1703・#1704・#2203）
- 既存 ADR 本文の書き換え、`docs/spec` の変更
