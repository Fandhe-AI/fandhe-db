# 簡易クエリのセミコロン区切り複数文実行

- ステータス: **Accepted（実装既定値）**
- 対応: Issue #938
- ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-16・`docs/spec/05-tasks.md` TASK-219
- 関連ポインタ: WIRE-4（1 メッセージ長上限）・SQL-8（許可リスト構造検証・単一文契約）・
  SQL-31（`BEGIN`/`COMMIT`/`ROLLBACK`。Issue #942 で実装済み）・RECOVER-12（複数文単位トランザクション・
  Issue #942・#1175 で実装済み）・ERR-1／ERR-2（`wire_code` 契約）

## 背景・目的

`'Q'`（簡易クエリ）本文全体を 1 文として `EngineCore::execute_sql_in_session`
へ渡す既存実装（TASK-73・WIRE-1）は、末尾の 1 個の `;` は許容するが、その後ろに
余剰トークンがあると `sql::allowlist::Parser::expect_end_of_statement` が
「複数文は未対応」として `42601` で拒否していた（SQL-8）。本 Issue は、1 つの
簡易クエリメッセージに含まれるセミコロン区切りの複数文を順に実行し、各文の
応答を順に返した後、`ReadyForQuery` を 1 回だけ返す（WIRE-16）。

## 分割の実装

分割・文種別分類は `crates/engine/src/sql/statement_splitter.rs`
（`split_statements`・`classify_statement`・`check_write_placement`）が担い、
wire 層（`crates/wire-server/src/simple_query.rs`）は SQL の字句知識を持たない
まま呼び出すだけの構成にした（既存の責務境界を維持）。

### 分割規則

- 字句走査は 5 状態（通常／`'...'`〔`''` エスケープ〕／`"..."`〔`""` エスケープ〕／
  `--` から `\n` または `\r` まで／`/* ... */`〔PostgreSQL と同じく入れ子を数える〕）。
  通常状態の `;` だけを区切りとし、コメント・引用の内部の `;` は区切りにしない
  （Issue #1175。従来はコメント・二重引用符を見つけると分割せず全文を `Single` に
  していた）。入れ子を数えないと `/* a /* b */ ; INSERT ... */` の `INSERT` が
  分割されて実行される（区切りの密輸）ため、深さを飽和演算で数える。
- 各断片は加工せず（コメントを除去せず）engine へ渡す。コメント・二重引用符識別子を
  含む断片は単一文のときと同じく `lexer::tokenize` が `42601` で拒否する
  （lexer の受理範囲は広げない）。コメントだけの断片（`SELECT 1; -- done`）も
  空文とはみなさず `42601` になる（PostgreSQL との既知の相違）。
- **PostgreSQL と分割結果が食い違いうる構文では分割しない**（fail-closed）。
  未終端の `'...'`・`"..."`・`/* */`、通常状態の `$`（ドル引用）、E 文字列の接頭辞
  （`E'`／`e'`）を検出した場合は `SplitOutcome::Single` を返し、全文を engine へ
  そのまま渡す（lexer が拒否して `42601`、1 文も実行されない）。
  単体テスト（`statement_splitter.rs::tests` の
  `unterminated_constructs_and_ambiguous_syntax_fall_back_to_single`・
  `nested_block_comments_do_not_smuggle_a_statement` 等）と wire 結合テスト
  （`wire16_implicit_transaction.rs`）で固定した。
- 非空文（前後空白 trim 後）の上限は `MAX_STATEMENTS_PER_QUERY = 16`
  （実装既定値）。超過は `54000`（`PayloadTooLarge`）で 1 文も実行しない。
- 空文（`;;`・先頭の `;`・末尾の余剰 `;`・空白のみの区間）は無視する。
  非空文が 0 個なら `EmptyQueryResponse` を返す。
- 既存の単一文契約（`;` なし、または末尾に 1 個だけの `;`）は
  `SplitOutcome::Single` として元テキストを無加工のまま渡す。これにより
  単一文の応答バイト列・エラーコード・メッセージは構造的に不変のまま保たれる
  （`crates/wire-server/tests/wire16_multi_statement.rs::
  single_statement_behavior_is_unchanged` で固定）。

### 文種別分類と「書き込みは最後の 1 文のみ」の制約

明示トランザクション（`BEGIN`/`COMMIT`/`ROLLBACK`。SQL-31・TASK-221。Issue #942）
実装後は、複数文メッセージ内で `BEGIN` から `COMMIT`/`ROLLBACK` までの区間
（`Active` 相当）にある書き込み系文は位置に関わらず許可する。`BEGIN` を含まない
複数文メッセージでは、従来どおり「書き込みが最後の 1 文に限られる」形のみを
受理する（fail-closed のまま）。詳細な状態機械は
[`explicit-transaction.md`](./explicit-transaction.md) 参照。

- `StatementEffect::ReadOnly`: `SELECT`（検索・集計・広域取得）・`EXPLAIN`。
- `StatementEffect::SessionLocal`: `SET ...`・`CREATE FUNCTION ...`。
- `StatementEffect::TransactionControl(TxnControl)`: `BEGIN`／`COMMIT`／
  `ROLLBACK`（SQL-31・TASK-221）。`check_write_placement` がトランザクション
  状態を模擬する際の遷移点になる。
- `StatementEffect::Write`: `INSERT`（UPSERT 含む）・`UPDATE`・`DELETE`・
  `TRUNCATE`、および読み取り専用・セッション局所・トランザクション制御の
  いずれとも判定できない未知の先頭語（fail-closed の既定。将来 engine に
  書き込み系構文が追加された場合に誤って許可しないための安全側の既定）。
- `StatementEffect::Rejected`: 字句解析に失敗する文、および字句解析には
  成功しても構造上どの `core.rs::execute_sql_in_session` の分岐にも到達し
  得ない先頭トークン形（`Token::Number`・`Token::Punct`・`Token::StringLiteral`・
  `Select` 以外の `Keyword`・`QualifiedIdent` 等）。実行すれば必ず
  `validate_sql` の許可リスト外（`42601`）で拒否され副作用が起きないため、
  位置に関わらず許可する（`check_write_placement` の対象外）。

`check_write_placement(stmts, initially_in_txn)` は、本メッセージの先頭文
実行前に接続が既に明示トランザクション中（`Active`）かどうかを
`initially_in_txn` で受け取り、`BEGIN` で「トランザクション内」、`COMMIT`／
`ROLLBACK` で「トランザクション外」という遷移をメッセージ内で先頭から模擬
する。トランザクション外で `Write` が最後の文以外の位置にある場合は
`0A000`（`FeatureNotSupported`）で 1 文も実行せずに拒否する。`COMMIT` は
トランザクション状態によらず必ず最後の文でのみ許可する（1 メッセージにつき
commit は高々 1 回という既存の不変条件を維持するため）。

## 原子性（暗黙トランザクション。Issue #1175）

`statement_splitter::plan_multi_statement(stmts, session_in_txn)` が、複数文
メッセージの実行方式を選ぶ。

| 条件 | 方式 | 挙動 |
| ---- | ---- | ---- |
| セッションが `Idle` でない（明示トランザクション中・`Failed`）、またはメッセージに `BEGIN`／`COMMIT`／`ROLLBACK` を含む | `Sequential` | 上記の `check_write_placement` をそのまま適用（従来と同一） |
| 書き込みが最後の 1 文だけ、または書き込みなし | `Sequential` | 文ごとの autocommit（従来と同一） |
| 上記以外（`Write` が最後以外にある） | `ImplicitTransaction` | メッセージ全体を 1 つの暗黙トランザクションで実行 |

- **既存経路を包み直さない**: `Active` の間は許可リスト内の文しか実行できないため、
  今動いているメッセージ（`SELECT ...; UPDATE ...` 等）を暗黙トランザクションで
  包むと `0A000` に後退する。`Sequential` が受理する形は必ず `Sequential` のまま残す。
- **暗黙トランザクションの開始点はメッセージの先頭**（最初の書き込みの直前ではない）。
  メッセージ全体が 1 つのトランザクションになり、`SET`・カーソル・読み取りの意味が
  一様になる代わりに、先頭の読み取りの間も単一ライタのゲートを保持する。保持時間は
  `max_duration`（20 秒）と文数上限 16 で有界。
- **状態機械**（`sql::transaction`）: `SessionTransaction::begin_implicit`（`Idle` から
  のみ）→ 各文を通常の `execute_sql_in_txn` で実行 → 最後の文の成功後に
  `commit_implicit`（遅延 `FOREIGN KEY` 検査・commit を 1 回）。エラー・上限超過・
  `operation_id` 再利用（`25000`）は `Failed` ではなく `Idle`（write txn を abort し
  ライタを解放）へ戻る。クライアントが `ROLLBACK` を発行できないため、`'E'` で固まらない。
  行・`operation_id` 台帳とも同一の write txn なので、ロールバックで一括して消える。
- **commit は最後の文の応答を書く前に、緊急応答登録の内側で 1 回だけ**行う
  （`run_statement_as` の `StatementRole::ImplicitFinal`）。これで RECOVER-6（commit
  後の panic への緊急応答）が commit 点を覆い、`_response_boundary` は変えずに
  「1 メッセージにつき commit は高々 1 回」が保たれる。commit に失敗した場合は
  `CommandComplete` を送らず ErrorResponse＋`ReadyForQuery('I')`。commit 後の応答
  エンコード失敗は「書き込みは確定したが応答はエラー」（既存の autocommit 単一文と
  同じ契約）。
- **今の時点で原子的に実行できる複数書き込みは、単一行 `INSERT`（`RETURNING` なし）と
  `TRUNCATE` の組み合わせに限られる**（明示トランザクションと同じ許可リスト）。
  `UPDATE`・`DELETE`・UPSERT・複数行 `INSERT`・DDL・UPSERT の `RETURNING` は `0A000` で暗黙
  トランザクション全体をロールバックする（fail-closed。#1179 で許可リストが広がれば
  自動的に広がる）。
- **既知の逸脱**: 書き込み済みテーブルの読み取りは `0A000`（明示トランザクションと
  同じ。例: `INSERT INTO t ...; SELECT ... FROM t` は INSERT の応答の後に `0A000`
  となり全体をロールバック）。
- **fail-closed の不変条件**: `classify_statement` は未知の先頭語を `Write` とみなす。
  そのため `ImplicitTransaction` へ振り分けられ、`Active` の許可リストに一致しなければ
  `0A000` になる。未知の書き込み構文が autocommit で素通りする経路は生まれない。
  各文の後に暗黙トランザクションが `Active` のままかも確認する（後続の書き込みが
  autocommit で実行されないよう、失われていたら `XX000` で打ち切る）。
- **セッション状態**（`SET`／`CREATE FUNCTION`）は、失敗時にメッセージ受信前の
  スナップショットへ無条件に復元する（`MessageSnapshot` は `Active` → `Idle` の遷移で
  復元を省くため暗黙モードでは使わず、専用の `run_implicit_transaction` が担う）。
- 途中の文の緊急応答登録・障害注入点は、commit が起きないため設けない
  （`StatementRole::ImplicitIntermediate`）。

`Sequential`（`BEGIN` を含むメッセージ・書き込みが最後だけのメッセージ）では従来どおり、
先行文がエラーになれば書き込み文はまだ実行されておらず、最後の書き込み文自身が
エラーになればその文の redb トランザクションが単独で原子的に失敗する。

### セッション状態の巻き戻し

複数文メッセージの途中でエラーが発生した場合、`SET search_mode`・
`CREATE FUNCTION` によるセッション局所の変更もメッセージ受信前の値へ巻き戻す
（PostgreSQL の暗黙トランザクション内で `SET` が巻き戻るのと同じ意味論）。
`execute_and_respond` が複数文モードへ入る際に `SessionState`（`Clone`
導出済み）のスナップショットを取得しておき、いずれかの文が失敗した時点で
復元する。単一文経路（`SplitOutcome::Single`）はこの clone を行わないため、
既存の単一文レイテンシ・アロケーションコストは不変。

### 制約を緩める条件（Issue #942・#1175 で緩和済み）

SQL-31（`BEGIN`/`COMMIT`/`ROLLBACK`）・RECOVER-12（複数文単位の
トランザクション機構の一部）は Issue #942 で実装済みとなり、`BEGIN` を含む
メッセージ内では「書き込みは最後の 1 文のみ」の制約を外した（上記
「文種別分類」節参照）。Issue #1175 で `BEGIN` を含まないメッセージも、書き込みが
最後以外にある形は暗黙トランザクションで原子的に実行する（上記「原子性」節）。
制御文を含むメッセージでの `check_write_placement`（`WriteNotLast`。`0A000`）は
従来どおり。

## 応答順序

各文の応答（`RowDescription`/`DataRow`*/`CommandComplete`、または
`ErrorResponse`）を順に送出し、`ReadyForQuery` は最後の文の応答にのみ付ける
（`crates/wire-server/src/simple_query.rs::Finish` 引数。`ReadyForQuery`／
`Continue` の 2 値）。途中の文がエラーになった場合は、その時点で
`ErrorResponse`＋`ReadyForQuery` を送って打ち切り、残りの文は実行しない
（`respond_error_and_ready` は `finish` に関係なく常に `ReadyForQuery` を
送る——途中エラーで打ち切るため、その時点で応答が確定する）。

`respond_command_complete`／`respond_query_result`／`respond_rows_with_tag`
はいずれも `StatementStatus`（`Completed`／`Failed`）を返し、複数文
オーケストレーション（`execute_and_respond`）がこれを見て次の文へ進むか
セッション状態を巻き戻して打ち切るかを判定する。単一文の応答バイト列
（`Finish::ReadyForQuery` 固定）は分割前と完全に同一。

## RECOVER-5/6 との関係

- `_response_boundary`（RECOVER-5 (3)。commit 成功境界と応答一意性）は
  `'Q'` 本文 1 通全体を覆ったまま変更しない。上記のとおり 1 メッセージに
  つき commit は高々 1 回に限られるため、既存の保証範囲をそのまま維持できる。
- 緊急応答の登録（RECOVER-6・`EmergencyResponseRegistration::register`）は
  文ごとに独立して張り直す（`run_statement` 内。旧 `execute_and_respond`
  本体から抽出）。書き込み文は最後の 1 文に限られるので、実質的に意味を
  持つのは最後の文の登録だけになる。
- 先行文の応答は `ResponseBuffer::push_frame`／`flush`（Issue #481）によって
  完全なフレーム単位でしかソケットに出ない。そのため、最後の文の commit 後に
  panic しても、緊急応答が書きかけのフレームに混入することはない（常に
  フレーム境界の後に続く）。

## スコープ外

- 拡張クエリプロトコル（WIRE-11）。
- `EngineCore::execute_sql`（非セッション API）・`execute_sql_in_session` の
  engine 側単一文契約。複数文対応は wire の `'Q'` 経路のみに実装し、
  `crates/engine/tests/rls_implicit.rs` の「`...; SELECT 1` → `42601`」という
  engine API 契約は不変のまま残す。
- HTTP／NoSQL 表層。SQL テキストを受けず束縛済み計画で動くため WIRE-16 の
  対象外であり、HTTP への射影変更もない。
- 3 クライアント e2e（`three_client_e2e.rs` 等）への追加は opt-in の任意
  追加に留め、本 Issue では必須にしない。
- 暗黙トランザクション内での `UPDATE`・`DELETE`・UPSERT・複数行／ファイル形 `INSERT`・
  UPSERT の `RETURNING`（`UPDATE`・`DELETE` の `RETURNING` は #1272 で対応済み）・DDL・COPY、および書き込み済みテーブルの読み取り（#1179 の成果を
  自動的に引き継ぐ）。`BEGIN` より前の書き込みを明示ブロックへ昇格させる PostgreSQL の
  意味論（`INSERT; BEGIN; ...`）、lexer でのコメント・二重引用符識別子・ドル引用・
  E 文字列の受理、拡張クエリプロトコルでの暗黙トランザクション（Sync 単位）も対象外。

## COPY の文単位化（Issue #1175）

`COPY`（WIRE-17・TASK-220・Issue #939）は、従来は `handshake::post_auth_loop` が
メッセージ全文へ覗き見判定を適用して `crate::copy::run` へ分岐していたため、複数文
メッセージの 2 文目以降の `COPY` は `42601` だった。Issue #1175 で判定を文単位へ移し、
`simple_query::run_copy_statement`（状態検査＋`crate::copy::run`）が担う。

- `SELECT ...; COPY t FROM STDIN ...` → SELECT の応答、CopyIn、`COPY n`、`ReadyForQuery`
  （PostgreSQL と一致）。`COPY (SELECT ...) TO STDOUT; SELECT 1` → CopyOut、`COPY n`、
  SELECT の応答、`ReadyForQuery`（PostgreSQL と一致）。`copy::run` は `Finish` を受け取り、
  途中の COPY では `CommandComplete` の後に `ReadyForQuery` を送らない。
- `classify_statement` は括弧の深さ 0 で最初に現れる `TO` なら `ReadOnly`、`FROM`・
  どちらも無い場合は `Write`（fail-closed）に分類する。
- **既知の相違**: `COPY t FROM STDIN ...; SELECT 1`・`INSERT ...; COPY t FROM STDIN` は
  暗黙トランザクションになり、トランザクション内の COPY は未対応のため `0A000`
  （CopyIn に入らず、`ReadyForQuery('I')`。全体をロールバック）。後続文のエラー時に
  COPY を巻き戻せないため原子性を優先して拒否する（従来は `42601`）。

## 挙動変化の明記（受理範囲の拡大）

以前は `42601` だった一部の入力が、本変更で受理側へ変わる（空文を無視する
帰結・単一文契約の外側を複数文経路が拾う帰結）。

- `SELECT 1;;`（末尾の余剰 `;`）・`;SELECT 1`（先頭の `;`）・`;`・`; ;`
  （非空文 0 個。`EmptyQueryResponse` を返す）。
- セミコロン区切りの複数文そのもの（本 Issue の主目的）。

これらはいずれも受理範囲の拡大であり、RLS・fail-closed・テナント境界の
契約を緩めるものではない（各文は独立に既存の許可リスト検証・RLS 暗黙適用を
通る）。

Issue #1175 による変化:

- 受理側へ変わるもの: 書き込みが最後以外にある複数文のうち、単一行 `INSERT`・
  `TRUNCATE` の組み合わせ（従来 `0A000`）。コメント・二重引用符識別子を含むメッセージの
  分割（断片ごとの `42601`。従来はメッセージ全体で `42601`）。2 文目以降の `COPY`。
- エラーコードの位置・種別が変わるもの: 書き込みが最後以外にある複数文のうち暗黙
  トランザクションで対応できない形は、従来「1 文も実行せず `0A000`」だったが、
  対応できない文の位置で `0A000`（それ以前の文の応答は届き、全体をロールバックする）。
  通常状態に `$` を含むメッセージは分割されず全文が `42601`（従来は断片ごとに実行を
  試みた）。`COPY ...; 他の文`（COPY が書き込みの場合）は `42601` から `0A000`。

## 検証

- `crates/engine/src/sql/statement_splitter.rs`（単体テスト）: 分割規則・
  文種別分類・書き込み配置検査・上限。
- `crates/wire-server/tests/wire16_multi_statement.rs`（層 A 結合テスト）:
  応答順序・エラー時の打ち切り・セッション状態の巻き戻し・RLS 不変
  （RLS-9/10 の応答同一性を含む）・単一文の既存挙動の不変性。
- `crates/wire-server/tests/wire16_implicit_transaction.rs`・
  `crates/engine/tests/wire16_implicit_txn.rs`: 暗黙トランザクションの原子的な commit・
  途中エラーでの全体ロールバック（行・`operation_id` 台帳・セッション状態）・
  `ReadyForQuery('I')`・RLS・区切りの密輸防止。`wire_fault_injection_cli.rs` は
  暗黙トランザクションの commit 後 panic の緊急応答が 1 回だけ送られること。
- 回帰: `crates/engine/tests/rls_implicit.rs`（engine API の単一文 `42601`
  契約）・既存 wire 結合テスト一式（`wire1_simple_query.rs` 等）・
  `wire_fault_injection_cli.rs`（commit 後 panic の緊急応答経路が文単位の
  登録でも成立すること）。
