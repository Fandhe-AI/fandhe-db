# 明示トランザクション BEGIN / COMMIT / ROLLBACK（Issue #942）

- ステータス: **Accepted（実装既定値）**
- 対応: Issue #942・TASK-221
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-31・
  `docs/spec/04-behavior/persistence.md` RECOVER-12・
  `docs/spec/04-behavior/error-format.md` ERR-6
- 関連ポインタ: RECOVER-5・RECOVER-6・RECOVER-8・TASK-96・TASK-97・TASK-99・
  TABLE-3・SQL-18（0 行 DELETE は台帳非記録）・WIRE-16（複数文実行。
  `wire-multi-statement.md`）・WIRE-19（`ReadyForQuery` 状態バイト。PR #1041
  レビュー指摘対応で #943 の担当分を本 PR へ吸収し実装済み）

## 背景・目的

各書き込み文（`INSERT`／`UPDATE`／`DELETE`／`TRUNCATE`／UPSERT）は、`tenant.rs`
の `*_unchecked` 関数の中で `Storage::begin_write_txn()` → 書き込み →
`commit_boundary::commit` まで自己完結していた（autocommit）。`BEGIN` などは
許可リストに無いため `42601` で拒否され、複数文メッセージ（Issue #938・
`wire-multi-statement.md`）では書き込み文は最後の 1 文に限られていた。

本 Issue は、簡易クエリ・拡張クエリの両経路で `BEGIN`〜`COMMIT`／`ROLLBACK`
を受理し、複数の書き込み文を 1 つの redb 書き込みトランザクションにまとめて
原子的に commit／破棄できるようにする。

## 設計の要点（判断記録）

### 1. `BEGIN` の時点で redb の write txn を取得して保持する

`redb::WriteTransaction`（`=4.2.0`）はライフタイム引数を持たず、全フィールドが
`Arc`／`Mutex`／`Atomic` なので `Send`。`Drop` は abort 相当（panicking 中を
除く）。単一ライタ（TABLE-3）なので、`BEGIN` で write txn を取得すれば
`COMMIT`／`ROLLBACK` までの間に他者は commit できない。台帳
（`recovery::ledger::record_in_txn`）も同じ write txn 内で書かれるため、
「台帳エントリが `COMMIT` と同じトランザクションで永続化され、`ROLLBACK`・
接続断で消える」（RECOVER-12）は追加コードなしで成立する。

代償: 読み取り専用のトランザクションでも単一ライタを占有する。既知のトレード
オフとして記録する。

### 2. writer gate（待機上限付き。ロック待ちで `55P03`）

`crates/engine/src/storage/writer_gate.rs`。`Mutex<GateState>` と `Condvar` で
構成し、`GateState` は `{ held_by: Option<ThreadId> }`。

- **autocommit 経路**（`Storage::begin_write_txn`）と**明示トランザクション
  経路**（`Storage::begin_explicit_write_txn`）は同じ保持方針をとる。どちらも
  ゲートを待機上限つきで取得し、`redb` の書き込みトランザクションが commit／abort
  されるまで `WriterPermit`（RAII）を保持する。`storage::GatedWriteTxn` が
  `redb::WriteTransaction` と permit を同じ寿命で束ねる。超過時は
  `StorageError::WriteLockTimeout`（`55P03`）。
- 当初の実装では、autocommit が `redb` のライタを得た直後にゲートを手放して
  いた。このため autocommit の書き込みがライタを持っている間に別セッションが
  ゲートを取得すると、`redb::Database::begin_write` の中で待機上限なしに
  止まっていた（PR #1041 レビュー指摘）。現在は「ゲートを保持していること」と
  「`redb` のライタを保持していること」が常に一致するため、ライタ待ちはすべて
  ゲートの待機上限で打ち切られる。
- **デッドロックが起きない理由**: ゲートは 1 段しかなく、`redb` のライタへの
  経路はゲート経由だけ。待機はすべてゲートの待機上限で打ち切られる。
- **自己デッドロックの防御**: 書き込みトランザクションを保持しているスレッド自身が
  再度ゲートへ到達した場合は待たずに即座に `WriteTxnHeldByCurrentSession`
  エラーを返す（`55P03` へ写像。書き込み経路の配線漏れに対する多層防御）。

### 3. `tenant.rs` の書き込み本体を `WriteTarget` で共有する

`pub(crate) enum WriteTarget<'a> { Autocommit(&'a Storage), InTxn(&'a redb::WriteTransaction) }`
と `WriteTarget::with_txn`（`f: impl FnOnce(&WriteTransaction) -> Result<(T, TxnEffect), E>`
を受け取り、`Autocommit` は `begin_write_txn` → `f` → `commit`（`f` が
`TxnEffect::NoOp` を返したときは commit せず abort）、`InTxn` は `f` の結果を
そのまま返す。`E` は `TenantWriteError` から変換できる型なら何でもよく、述語形
DML は `PredicateDmlError<E>` を使う）を導入した。Issue #942 では次の 4 関数のみを
対象にし、Issue #1179 で行を書き換える全書き込み関数へ拡大した:

- `insert_row_unchecked`（Rust API の行形 INSERT）
- `insert_rows_unchecked`（同・バッチ）
- `insert_typed_row_unchecked`（SQL 表層の単一行 typed INSERT）
- `truncate_table_unchecked`（`TRUNCATE TABLE`）
- （Issue #1179）`insert_typed_rows_unchecked`（複数行 `VALUES`・COPY）・
  `upsert_typed_rows_unchecked`（`ON CONFLICT`）・`update_row_unchecked`・
  `update_row_columns_unchecked`（単一行 `UPDATE`）・`delete_row_impl`
  （単一行 `DELETE`。`RETURNING` を含む）・`delete_rows_where_unchecked`・
  `update_rows_where_unchecked`（述語形 `DELETE`／`UPDATE`）

`sql::exec` は各実行関数の `_in` 版（`WriteTarget` を受け取る本体）を持ち、既存の
関数は `Autocommit` を渡す薄いラッパーとして挙動を変えない。`core.rs` の
`execute_in_active_txn` が `Insert`（単一行・複数行・UPSERT）・`Delete`・`Update` を
受理し、成功後に `mark_written` する。`RETURNING` は UPSERT を除く全 DML
（`INSERT`・`UPDATE`・`DELETE`。Issue #1272 で `UPDATE`・述語形 `DELETE` を追加）で
受け付け、UPSERT だけ下記の `0A000` で拒否する。
`parsed_operation_id` は `Delete`・`Update` の `operation_id` も返し、同一
トランザクション内での再利用（`25000`）は全 DML に効く。

**明示トランザクション内で拒否（`0A000`）を維持するもの**: ファイル形 `INSERT`
（`replace_typed_rows_by_text_key`。埋め込み I/O を単一ライタの保持中に行うことに
なるため autocommit 専用のまま）、DDL、`USING PLAN` を伴う検索 SELECT・`EXPLAIN`
のうち対象テーブルが dirty のもの（「6. トランザクション内の読み取り」参照）。

**明示トランザクション内の `RETURNING` で拒否（`0A000`）するもの（Issue #1182・#1272 の
経緯・SQL-31・SQL-21）**: UPSERT（`INSERT ... ON CONFLICT ... RETURNING`）のみ。
`execute_insert_returning_form` の `Upsert` 腕で束縛の後・書き込みの前に拒否する。
単一行・述語形の `UPDATE ... RETURNING` と述語形の `DELETE ... RETURNING` は Issue #1272
で受理に変わり、`execute_in_active_txn` が autocommit と同じ
`execute_update_returning_form`・`execute_predicate_delete_returning_form` へ
`WriteTarget::InTxn` を渡す（返却行数＝影響行数・可視集合での対象選定・RLS は共通）。
`RETURNING` を黙って落とす fail-open にはしない。fail-closed の契約: autocommit では
投影の失敗（`54000` 等）がその文自身の write トランザクションを abort するが、`InTxn` では
文ごとの abort は無く、`execute_parsed_in_txn` がエラーを受けて `txn.fail()` を呼び共有の
`write_txn` を drop（abort）する。明示トランザクションは `Failed` へ遷移して部分書き込みは
永続化されず（COMMIT は `25P02`、受け付けるのは ROLLBACK だけ）、暗黙トランザクション
（WIRE-16・Issue #1175。文の実行ディスパッチは起源を区別しない）は abort して `Idle` に
戻る。受け付けるようにする後続は Issue #1273（UPSERT）。

**commit 後の副作用の監査（Issue #1179）**: 上記の書き込み関数と呼び出し元
（`sql::exec`・`core.rs`）に、`commit_boundary::commit` の後で共有インメモリ状態
（キャッシュの差分更新・索引・統計）を変更するコードは無い（キャッシュはテーブル世代
キーで失効する方式）。したがって `InTxn` でも本当の COMMIT より前に走ってしまう
副作用は無い。

**空バッチ**: `insert_rows_unchecked` の空バッチ（`rows.is_empty()`）は、main と
同じく commit せずに `write_txn` を abort する（`TxnEffect::NoOp`）。当初の実装は
空の `write_txn` を `commit_boundary::commit` で commit していたため、グローバル
世代が進み、世代で失効するキャッシュを無駄に捨てていた（PR #1041 レビュー指摘）。
他の関数は基本的に常に書き込む（`truncate_table_unchecked` は 0 行でも台帳記録と
テーブル世代の進行を行う既存契約）ため `TxnEffect::Wrote` を返す（述語形 DML の
件数上限超過は `TxnEffect::NoOp` で、呼び出し元が `54000` を返し `Failed` へ遷移する）。

### 4. 状態機械（`crates/engine/src/sql/transaction.rs`）

`SessionTransaction<'e>`（`enum TxnState<'e> { Idle, Active(Box<ActiveTxn<'e>>),
Failed { session_at_begin: SessionState, expired: bool } }`）。`ActiveTxn` は
`write_txn: GatedWriteTxn`（permit を内包）・`started_at`・
`statements`・`seen_operation_ids`・`written_tables`・`written_by_tenant`・`has_writes`・
`session_at_begin` と、dirty テーブル判定（確定済みの世代の読み取り）に使う
`&'e Storage` を保持する。

- **`BEGIN`**: `Idle` → `Active`（`Storage::begin_explicit_write_txn` 経由）。
  `Active` 中の再 `BEGIN` は `25001`（`ActiveSqlTransaction`）で `Failed` へ
  遷移。`Failed` 中は `25P02`。
- **`COMMIT`**: `Idle` は `25P01`（`NoActiveSqlTransaction`）。`Active` は
  `has_writes` なら `commit_boundary::commit`、無ければ `drop`（abort）して
  `Idle` へ。`Failed` は `25P02` のまま（`ROLLBACK` のみ受理し続ける）。
  持続時間の上限を過ぎた `Active` の `COMMIT` は確定させず、abort して
  `Failed` へ遷移し `54000` を返す（文実行時の上限超過と同じ契約）。commit
  自体が失敗した場合は、PostgreSQL と同じくロールバック扱いとし、`BEGIN`
  時点の `SessionState` を復元してから `Idle` へ戻る（PR #1041 レビュー指摘）。
- **`ROLLBACK`**: `Active`／`Failed` いずれからも `Idle` へ戻り、`BEGIN` 時点の
  `SessionState`（`SET search_mode`・`CREATE FUNCTION` 等）を復元する
  （PostgreSQL の挙動に準拠する実装判断。spec は沈黙）。`Idle` からの
  `ROLLBACK` は `25P01`。
- **通常の文**: `Idle` は既存 autocommit（`execute_parsed_in_session`）を
  そのまま通す（挙動は不変）。`Failed` は実行せず `25P02`。`Active` は
  上限検査 → `operation_id` 再利用検査 → 種別判定 → 実行、の順。
- **`Active` 中のどの文でエラーが起きても**、その場で `write_txn` を abort
  し permit を解放して `Failed`（ロックは保持しない）へ遷移し、元のエラーを
  そのまま返す。Failed 状態から `COMMIT` はできないため、失敗した文が残した
  部分書き込みが永続化されることはない。**redb の savepoint は不要**。
  構文・許可リスト検証のエラー（`execute_sql_in_txn` の `parse_sql` 失敗）、
  簡易クエリの複数文分割・位置検証のエラー、拡張クエリプロトコルの
  Parse／Bind／Describe／Execute のエラー応答も同じく `Failed` へ遷移させる
  （PostgreSQL と同じく、エラーの種類を問わない。PR #1041 レビュー指摘）。
  `SessionTransaction::fail` は `Active` 以外では状態を変えない（冪等）ため、
  wire 層はエラー応答のたびに状態を問わず呼べる。
- **同一トランザクション内での `operation_id` の再利用**: 台帳照合より前に
  `seen_operation_ids` と照合し、一致すれば `25000`
  （`InvalidTransactionState`）で `Failed` へ遷移する。台帳は自トランザクションの
  未 commit エントリを見て `23505` を返してしまうため、それより先に検査する。
  テーブルを問わず同じ ID を拒否する（保守的な側に倒す）。

### 5. 上限（実装既定値。spec 由来の数値ではない）

- `max_duration`（`BEGIN` からの経過時間の上限）: **20 秒**
- `max_statements`（トランザクション内の文数の上限）: **1,000**
- 超過時は `54000`（`PayloadTooLarge`）で `Failed` へ遷移する。
- `max_duration` は文の実行時・`COMMIT` 時に検査するほか、wire 層
  （`handshake::post_auth_loop`）が要求を受け取るたびに
  `SessionTransaction::release_if_expired` で検査する。期限を過ぎていれば、
  要求の種類（Sync・Flush 等の SQL を伴わない要求を含む）を問わず
  共有書き込みトランザクションを abort してライタを解放し、`Failed` へ遷移する。
  その後の最初の文／`COMMIT` には `54000` を 1 回だけ返し、以降は `25P02`。
- 要求が 1 件も届かない無通信の間も `max_duration` で解放する（PR #1041
  レビュー指摘）。`Active` の間だけ、wire 層（`handshake::read_next_frame_header`）
  が次の要求の型バイトを待つ読み取りタイムアウトを「上限までの残り時間」と
  接続の読み取りタイムアウト（`limits::READ_TIMEOUT`＝30 秒。WIRE-5）の小さい方へ
  切り詰める。期限到達でタイムアウトしたら `release_if_expired` で abort して
  ライタを解放し、応答は送らずに接続を維持したまま受信待ちを続ける（最初の文／
  `COMMIT` に `54000` が返る点は受信時の検査と同じ）。
  - 型バイトの受信待ちで期限に達した場合は接続を維持する（フレームの境界のため）。
    型バイトの受信後、長さ・本文の受信全体にも同じ期限を課し（`framing::
    FrameDeadlineGuard`。読み取り 1 回あたりの待機は 100 ms で区切る）、途中で
    期限に達したら応答を送らずに接続を閉じてライタを解放する（本文を少しずつ
    送り続けて上限後もライタを保持させる経路を塞ぐ。PR #1041 レビュー指摘）。
  - 無通信の上限（WIRE-5）は不変: 期限到達後の再待機は、受信待ちを始めた時点
    からの経過を差し引いた読み取りタイムアウトの残りで待ち、尽きたら従来どおり
    応答なしで切断する。
  - `Failed`・`Idle` はライタを保持しないため切り詰めない。
- `lock_wait`（他セッションが writer gate を待つ上限）: **30 秒**
  （`Storage::DEFAULT_WRITE_LOCK_WAIT`。既存 `READ_TIMEOUT` と同じ値）。
  超過時は `55P03`。

### 6. トランザクション内の読み取り（未 commit 変更の反映。Issue #1179）

読み取りの入力源を `storage::read_source::ReadSource`（確定済みスナップショットの
`redb::ReadTransaction` と、明示トランザクションが保持する共有
`redb::WriteTransaction` の双方を実装）へ抽象化し、`sql::scan`・`aggregate`・
`group_by`・`window`・`subquery`・`set_op`・`join`・`relation_snapshot`・`exec`・
`arena`・`catalog` の読み取り本体を同じコードで動かす。`EngineCore::read_only_in_active_txn`
が次のとおり経路を選ぶ。

- 自トランザクションが未 commit の変更を持たない（`SessionTransaction::dirty_tables`
  が空）間: 従来どおり BEGIN 時点の確定済みスナップショットで読む（キャッシュ利用可。
  単一ライタにより BEGIN 時点と厳密に一致する）。
- 変更を持つ間: 共有書き込みトランザクションを読み取り源にして、autocommit と同じ
  実行本体（`EngineCore::execute_read_statement`。スキーマ取得・サブクエリ解決・
  RLS 適用・実行）を呼ぶ。対象は `Select`（`USING PLAN` なし）・`Aggregate`・`Scan`
  （ウィンドウ関数を含む）・`SetOperation`・`Join`・`DECLARE` の内側 SELECT・
  `COPY (...) TO STDOUT`。サブクエリ・JOIN・集合演算が別テーブルの未 commit 変更を
  読む場合も、文全体が同じ読み取り源を使うため反映される。RLS の判定ロジックには
  手を入れない（読み取り源だけが変わる）。
- **dirty テーブル**は、文が直接書き込んだテーブル（`mark_written`）と、書き込み
  トランザクション内のテーブル世代が確定済みの世代から変化したテーブルの和。
  行を書き換える全経路は commit 前に `bump_table_generation_in_txn` を呼ぶ契約
  （`tests/table_generation_bump_coverage.rs` が構造的に固定）で、参照アクションの
  連鎖で書き込まれた子テーブルも bump するため、`mark_written` の記録漏れに依存しない。
  取得に失敗した場合は fail-closed に dirty として扱う。

**キャッシュの構造的ゲート（P0）**: テーブル世代キーのキャッシュ（`arena_cache`・
`hnsw_cache`・`sparse_cache`・`visible_cache`・`scalar_index`・`relation_snapshot`）は
具体型 `&redb::ReadTransaction` のまま残し、`ReadSource::snapshot()` が `Some`
（確定済みスナップショット）のときだけ使える構造にした。書き込みトランザクションでは
`None` を返し、読み取り本体はキャッシュなしの brute-force 経路へ落ちる（ベクトル検索の
投影遅延も使わない）。理由: ROLLBACK したトランザクションのテーブル世代は次に commit
される別の書き込みで再利用されるため、未 commit の行から作ったキャッシュエントリが
確定済みデータとして別セッション（別テナントを含む）へ返る恐れがある。
`WriteTransaction` を読み取り源にするときは、存在しないテーブルを `open_table` が作成
してしまう副作用を避けるため、`list_tables` で存在を確認してから開く。

**残る既知の逸脱**: LLM I/O とテーブル世代の再照合を伴う `USING PLAN` の検索 SELECT と
`EXPLAIN` は、対象テーブルが dirty のとき `0A000` を返し `Failed` へ遷移する（黙って
古い結果を返さない）。書き込み後の読み取りはキャッシュ・HNSW を使わない brute-force に
なる（性能上のトレードオフ。テーブル単位のキャッシュ再利用は後続課題）。

## `#943` との分担

`ReadyForQuery` の状態バイト（`'I'`／`'T'`／`'E'`。WIRE-19）は当初 `#943` が
担当する計画だったが、PR #1041 レビュー指摘（codex P1: 簡易・拡張クエリ両
プロトコルとも `SessionTransaction` 導入後も常に `'I'` を送出しており、
`BEGIN` 後もクライアントからトランザクションが終了したように見える不整合）
への対応として本 PR へ吸収し実装済み（`wire-server::result_encoder::
encode_ready_for_query`／`simple_query.rs`／`extended_query::handle_sync`）。

`#943`（WIRE-19）は上記の中核実装を前提に、残差の検証（単一文の異常遷移・
複数文（WIRE-16）・空クエリ・COPY・拡張クエリの Execute 段エラーの状態
バイト、および無改造の実クライアント 3 種による観測）を層 A
（`crates/wire-server/tests/wire19_ready_for_query_status.rs`・
`wire942_extended_transaction.rs` の失効テストへの状態アサーション追加）・
層 B（`crates/wire-server/tests/three_client_e2e.rs::
three_clients_observe_transaction_status_transitions`）として実施した。
production コード（`crates/wire-server/src/`）は無変更・テスト専任。

## wire-server への結線

- `handshake.rs::post_auth_loop` が接続単位の `Option<SessionTransaction<'e>>`
  を `session` と並べて保持する（`engine.map(|e| e.new_session_transaction())`）。
  未 commit のまま接続が切れた場合、共有 `write_txn` は commit されずに abort
  され、`WriterPermit` も解放される。
- `simple_query.rs::execute_and_respond`／`run_statement` は
  `engine.execute_sql_in_session` の代わりに `engine.execute_sql_in_txn` を
  呼ぶ（`txn` が `Idle` の間はビット同一の挙動）。
- `statement_splitter::check_write_placement(stmts, initially_in_txn)` が
  `TransactionControl` 遷移を模擬し、`BEGIN` を含む複数文メッセージでは書き込み
  文の位置制約を緩和する（詳細は `wire-multi-statement.md` 参照）。
- 明示トランザクション中の `COPY`（Issue #1179）は `crate::copy::run` へ委譲する。
  `COPY FROM STDIN` は `EngineCore::commit_copy_in_txn`（`check_and_register_statement`
  → 共有 `write_txn` への複数行 INSERT → `mark_written`。既存の `commit_copy_in` と
  実行本体を共有）、`COPY (...) TO STDOUT` は `EngineCore::begin_copy_in_txn`（未 commit
  変更を反映）を呼ぶ。COPY 中のあらゆるエラー（CopyFail・デコード失敗・上限超過・
  実行エラー）はトランザクションを `Failed`（ReadyForQuery `'E'`）へ遷移させ、成功時の
  ReadyForQuery は `txn.status()` から導出する。`Failed` 中の `COPY` は autocommit として
  実行せず `25P02` で拒否する（PR #1041 レビュー指摘）。受信期限（DoS 対策）は
  ハンドシェイク層のフレーム受信期限（`framing::FrameDeadlineGuard`。`Active` の間は
  `min(残り持続時間, 読み取りタイムアウト)`）が COPY サブプロトコルの受信にも及び、
  期限超過で接続を閉じて `SessionTransaction` の drop でライタを解放する（CopyData を
  少しずつ送り続けても単一ライタを無期限に保持できない）。

## エラー時の遷移と `Failed` 中の拒否（入口別の網羅表）

`Active` 中のエラーは種類を問わず `Failed` へ遷移させ、`Failed` 中は `ROLLBACK`
以外を `25P02` で拒否する（持続時間の上限で解放した直後の最初の 1 回だけは
`54000`。`SessionTransaction::take_failed_error`）。PR #1041 のレビューを受けて、
入口ごとに次のとおり確認した。

| 入口 | `Active` 中のエラー → `Failed` | `Failed` 中の拒否 |
| ---- | ---- | ---- |
| 簡易クエリ: 複数文の分割・位置検証（`simple_query::respond_splitter_error`） | `txn.fail()` | `25P02` |
| 簡易クエリ: 各文（`EngineCore::execute_sql_in_txn`） | 字句・構文・許可リスト検証のエラーは `fail()`、上限超過・`operation_id` 再利用・実行エラーは `execute_parsed_in_txn` が `fail()` | parse より前に先頭トークンで判定し、`ROLLBACK` 以外は `25P02` |
| 簡易クエリ: `BEGIN` | 入れ子は `25001` で `fail()` | `25P02` |
| 簡易クエリ: `COMMIT` | 期限切れは abort して `Failed`・`54000`。commit 失敗はロールバック扱いで `Idle` | `25P02` |
| 簡易クエリ: `COPY`（`handshake::post_auth_loop`） | 実行エラー・CopyFail は `copy::run` が `fail()`（Issue #1179。正常終了は `'T'` のまま） | `25P02`（`crate::copy::run` へ委譲しない） |
| 簡易クエリ: 空文字列 | 対象外（エラーにならない） | EmptyQueryResponse（副作用なし） |
| 簡易クエリ: 応答のエンコード失敗（`RowDescription`・`DataRow`・`CommandComplete`） | 文の実行後でも `txn.fail()` | — |
| 拡張: Parse | エラー応答は `respond_error_and_await_sync` を通り、`post_auth_loop` が `ignore_till_sync` を見て `fail()` | `ROLLBACK`・空文字列以外は、本体の構造検証（`08P01`）の後、パラメータ型 OID 指定の検査（`0A000`）・parse より前に `25P02` |
| 拡張: Bind | 同上 | `ROLLBACK`・空文字列以外のステートメントは、本体の構造検証の後、format code の検証・ステートメントの存在確認より前に `25P02`（未登録・`Failed` 前に Parse 済みのものを含む） |
| 拡張: Describe | 同上 | 受理（副作用なし。実行は Execute で拒否される） |
| 拡張: Execute | 実行エラーは `execute_parsed_in_txn` が `fail()`。後処理のエラー応答は `ignore_till_sync` 経由で `fail()` | 未実行の `ROLLBACK`・空文字列以外の portal は、本体の構造検証の後、portal の存在確認・実行状態より前に `25P02`（期限切れの未報告分は `54000`）で拒否し、portal を終端状態 `Failed` にする（実行を開始済みの `Suspended`・`Done` も残り行の送出・タグの再送をしない） |
| 拡張: Close・Sync・Flush | 対象外（エラー応答は `ignore_till_sync` 経由で `fail()`） | 受理（副作用なし） |
| フレーミング・プロトコル違反 | 接続を切断し、`SessionTransaction` の drop で abort | 同左 |
| 全メッセージ共通（受信直後） | 期限切れなら `release_if_expired` で `Failed` にしてライタを解放 | — |
| 無通信（受信待ち） | 期限到達で受信待ちを打ち切り、`release_if_expired` で `Failed` にしてライタを解放 | — |

## 検証

- `crates/engine/tests/sql31_transaction.rs`: BEGIN/INSERT/INSERT/COMMIT の
  可視性・ROLLBACK の完全巻き戻し・入れ子 BEGIN の `25001`・トランザクション外
  COMMIT/ROLLBACK の `25P01`・同一トランザクション内 `operation_id` 再利用の
  `25000`・対応外文（DDL）の `0A000`・文数上限の `54000`・TRUNCATE と INSERT の原子的
  commit・接続断相当（drop）での完全ロールバックを固定。
- `crates/engine/tests/sql31_txn_dml.rs`（Issue #1179）: 複数行 INSERT・UPSERT・
  UPDATE・DELETE の COMMIT／ROLLBACK／drop、`UPDATE`・述語形 `DELETE` の `RETURNING`
  （Issue #1272。read-your-writes・ROLLBACK で行・索引・台帳に痕跡なし・テナント境界・
  未知列での fail-closed）、全 DML への
  `operation_id` 再利用検査、遅延 FK の COMMIT 時検査（連鎖で書き換わったテーブルの
  下位の遅延 FK を含む）、未 commit 変更の読み取り（Scan・Aggregate・GROUP BY・JOIN・
  自己 JOIN・サブクエリ・集合演算・カーソル）、キャッシュ汚染の回帰（ROLLBACK 後に
  世代が再利用されても未 commit 行が現れない）、RLS を固定。
- `crates/engine/tests/sql_returning.rs`
  （`upsert_returning_inside_explicit_transaction_is_still_rejected`）: 明示トランザクション内の
  UPSERT の `RETURNING` が `0A000` になり行が変わらないことを固定（#1273 まで）。
  `sql21_returning_row_count_parity.rs` は `UPDATE`・述語形 `DELETE` の `txn: true` セルで
  返却行数・影響行数・物理差分の一致を両可視性モードで固定。暗黙トランザクション側の
  対応外文の例は `DROP TABLE`（`crates/engine/tests/wire16_implicit_txn.rs`・
  `crates/wire-server/tests/wire16_multi_statement.rs`・`wire16_implicit_transaction.rs`）で
  固定し、`UPDATE ... RETURNING` の受理は `wire16_implicit_transaction.rs` が固定。
- `crates/wire-server/tests/wire19_ready_for_query_status.rs`・
  `wire942_extended_transaction.rs`（Issue #1179）: トランザクション内 COPY の
  ReadyForQuery 状態・拡張クエリ経路の UPDATE／DELETE／UPSERT・停滞した COPY の
  受信期限を固定。
- `crates/wire-server/tests/`: 既存の複数文・COPY・エラー射影テストが
  `check_write_placement` のシグネチャ変更・`SqlOutcome` の新 variant 追加後も
  無変更のまま green（回帰なし）。
- `crates/wire-server/tests/err4_http_projection.rs`・
  `crates/wire-server/docs/nosql-api.md`（`nosql_api_doc.rs`）: 新規 5 分類
  （`55P03`／`25000`／`25001`／`25P01`／`25P02`）を NoSQL 表層からの到達不能
  分類として追加し、production の応答エンコーダ経由で射影のみを固定する
  （NoSQL 表層の `op` 許可リストにトランザクション制御が無いため）。

## 起源（Explicit／Implicit）ごとの遷移差分（Issue #1175）

`ActiveTxn` は起源 `TxnOrigin`（`Explicit`／`Implicit`）を持つ。起源の違いは状態遷移
だけで、文の実行ディスパッチ（`core.rs::execute_parsed_in_txn`）は区別しない。

| 事象 | Explicit | Implicit |
| ---- | -------- | -------- |
| 開始 | `BEGIN`（`begin`） | メッセージ先頭で wire 層が `begin_implicit`（`Idle` からのみ。それ以外は `XX000`） |
| 文のエラー・上限超過・`operation_id` 再利用（`fail`） | `Failed`（`ROLLBACK` 待ち） | `Idle`（write txn を abort・ライタ解放） |
| 確定 | `COMMIT`（上限超過は `Failed` へ遷移し `54000`） | 最後の文の成功後に `commit_implicit`（上限超過・commit 失敗のどちらも `Idle`） |
| 期限切れ（`release_if_expired`） | `Failed`（次の文で `54000`） | `Idle`（メッセージをまたがないため通常は到達しない） |
| `BEGIN`／`COMMIT`／`ROLLBACK` が届いた場合 | 通常の遷移 | 不変条件違反として abort して `Idle`、`XX000` |
| `ReadyForQuery` の状態バイト | `'T'`／`'E'` | メッセージ内の途中は送らず、終了時は常に `'I'` |

## 対象外・申し送り

- 明示トランザクション内の UPSERT の `RETURNING` 受理は Issue #1273。現状は
  上記「`RETURNING` で拒否（`0A000`）するもの」のとおり（SQL-31・SQL-21）。
- 明示トランザクション内のファイル形 INSERT（埋め込み I/O。`0A000` のまま）と、
  dirty テーブルに対する `USING PLAN` の検索 SELECT・`EXPLAIN`（`0A000` のまま。
  上記「6. トランザクション内の読み取り」の残る既知の逸脱）。
- 書き込み後の読み取りでのキャッシュ・HNSW の再利用（テーブル単位の最適化。現状は
  brute-force）。
- `DUPLICATE_OPERATION_ID` と `UNIQUE_VIOLATION` の `code` ラベル区別は Issue #1180 で
  engine の `ErrorClass` と HTTP `code` に実装済み（ERR-6）。pg wire の `ErrorResponse` は
  ERR-1 の既存形式（`S`/`C`/`M`）のままで、固定文言の違いで区別する。
- 暗黙トランザクション（WIRE-16）による複数文の書き込み位置制約の撤廃は
  **Issue #1175 で実装済み（制約付き）**。`BEGIN` を含まないメッセージのうち書き込みが
  最後以外にある形は、メッセージ全体を 1 つの暗黙トランザクションで原子的に実行する
  （詳細は [`wire-multi-statement.md`](./wire-multi-statement.md)「原子性」節）。
  使える文は明示トランザクションと同じ許可リスト（Issue #1179 で拡大済み。`RETURNING`
  は UPSERT のみ未対応）。`BEGIN` より前の書き込みを
  明示ブロックへ昇格させる PostgreSQL の意味論（`INSERT; BEGIN; ...`）は対象外。
- savepoint、分離レベルの指定、`START TRANSACTION`／`END`／`ABORT` などの別名。
- NoSQL 表層のトランザクション（設計上 `0A000`。`op` 許可リストに追加しない）。
- 先頭文が 0 行 DELETE の場合に RECOVER-12 の再送判定が成立しない制約
  （SQL-18 の既存契約〔0 行 DELETE は台帳非記録〕との相互作用）。
- 上限の既定値（20 秒・1,000 件・30 秒）の確定 → オーナー判断。

## カーソルとの関係

`DECLARE`／`FETCH`／`CLOSE`（Issue #937・WIRE-15・TASK-218）は本トランザクション
機構の上に実装されており、カーソルは `sql::transaction::ActiveTxn` に埋め込
まれるため、`COMMIT`／`ROLLBACK`／`fail`／期限切れ／接続断のいずれでも自動的に
クローズされる。詳細は `docs/design/sql-cursor.md` 参照。
