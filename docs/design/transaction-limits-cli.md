# トランザクション最大持続時間・最大文数の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1553・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-transaction-duration-ms`・`--max-transaction-statements`
- 先行: [join-limits-cli.md](join-limits-cli.md)（Issue #1551）・[group-by-limits-cli.md](group-by-limits-cli.md)（Issue #1550）と同型。トランザクション機構の本体は [explicit-transaction.md](explicit-transaction.md)

## 背景

明示トランザクションの最大持続時間（既定 20 秒）と最大文数（既定 1,000）は `TransactionLimits::default()` に固定され、運用者が起動時に変えられなかった（注入点 `EngineCore::with_transaction_limits` はテストからしか呼ばれていなかった）。持続時間は単一ライタを握る時間に直結するため、設定可能にするのと同時にハード上限を定数で設ける。

## 決定

| # | 内容 |
| - | ---- |
| D1 | 兄弟フラグと違い、**既定の上下どちらへも設定できる**。兄弟の既定値は構造上のハード上限だったが、本件の既定値は実装既定値で構造上の上限ではない。既定値は変えず、その上に保守的なハード上限を置く |
| D2 | 定数（`sql::transaction`）: 持続時間 `MIN 100 ms` / `DEFAULT 20,000 ms` / `MAX 60,000 ms`、文数 `MIN 1` / `DEFAULT 1,000` / `MAX 10,000`。ハード上限は書き込みロック待ち上限の既定（30 秒）の 2 倍・既定文数の 10 倍を暫定採用（**オーナー確認待ち**） |
| D3 | `TransactionLimits` の形（公開フィールド＋`Default`）は変えない。既存テストが構造体リテラルで組み立てているため。範囲を強制するのは CLI 経路（`validate_*`）だけ |
| D4 | `validate_*` と定数は既存の公開モジュール `engine::sql::transaction` に置く（`lib.rs` の公開面は増えない） |
| D5 | フラグ名と単位: `--max-transaction-duration-ms <100-60000>`（ミリ秒）・`--max-transaction-statements <1-10000>`。2 回目以降の指定は fail-closed（last-wins にしない）。環境変数は設けない |
| D6 | `max_statements` は `u32`。厳密パースした値は `u64` で `validate_*` に渡し、`u32` に収まらない値も範囲外として拒否する |
| D7 | **暗黙トランザクション（WIRE-16。`BEGIN` を含まず書き込みが最後以外にある複数文メッセージ）にも同じ値が効く**。`--max-transaction-statements N` では N+1 文の暗黙トランザクションが `54000` で失敗する |
| D8 | 文数は加算前に判定し `BEGIN` は数えない。N 文目まで成功し、N+1 文目で `54000`・`Failed` へ遷移する（既存契約） |
| D9 | 持続時間を 30 秒超へ上げたときの相互作用: (a) wire 層の受信待ちは `min(残り時間, 読み取りタイムアウト)`（既定 30 秒。`--read-timeout-secs` で変更可） のため無通信の許容時間（WIRE-5）は延びない。(b) 書き込みロック待ち上限（既定 30 秒。`--write-lock-wait-ms` で変更可）を超えると、ほかのセッションの書き込みが `55P03` を受け得る。(b) への対処として、明示指定した持続時間が解決済みのロック待ち上限を超える場合だけ起動ログに英語の `WARNING` 行を出す（エラーにはしない） |
| D10 | 範囲外は `TransactionLimitOutOfRange { item, value, min, max }`。文言は英語で値と範囲だけを含む |

## 効く経路

`SessionTransaction` を共有する全経路（SQL simple／extended・明示トランザクション・暗黙トランザクション・`DECLARE CURSOR`・COPY）。NoSQL（HTTP）表層はトランザクション非対応（`0A000`）のため影響しない。

## SQLSTATE（従来と同じ）

持続時間・文数の超過とも `54000`。エラー文は従来のまま。

## 対象外

- ハード上限・既定値の確定（オーナー判断）
- セッション単位の `SET`・テナント単位の設定・環境変数での指定
- 書き込みロック待ち上限の CLI 化は Issue #1554 で実装済み（D9 の警告は `--write-lock-wait-ms` で解決済みの値と比較する）
