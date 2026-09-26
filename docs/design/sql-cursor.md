# カーソル DECLARE / FETCH / CLOSE（Issue #937）

- ステータス: **Accepted（実装既定値）**
- 対応: Issue #937・TASK-218
- ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-15・
  `docs/spec/04-behavior/error-format.md` ERR-6
- 前提: 明示トランザクション（Issue #942・SQL-31・TASK-221。
  `docs/design/explicit-transaction.md`）
- 関連ポインタ: TABLE-3（スナップショット一貫性）・RLS-7（暗黙適用）・
  SQL-13／SQL-14（集計 `SELECT`）・SQL-15（広域取得 `SELECT`）

## 背景・目的

大量の結果を分割して取得する手段がこれまで `LIMIT` しかなかったため、
明示トランザクション（Issue #942）の中で次の 3 文を受理できるようにする。

- `DECLARE <name> CURSOR FOR <SELECT>`
- `FETCH [FORWARD] <n> FROM <name>`
- `CLOSE <name>`

## 設計の要点（判断記録）

### 1. 結果は `DECLARE` 時点で確定し、カーソルは `ActiveTxn` に保持する

`DECLARE` の時点で内側の `SELECT` を既存の読み取り経路
（`EngineCore::execute_validated_in_session`）で 1 回だけ実行し、
`QueryResult`（列メタ・行）をメモリへ確定させる。`FETCH` は確定済みの行を
先頭から最大 `n` 件ずつ払い出すだけで、検索本体を再実行しない
（INSENSITIVE。PostgreSQL の既定カーソルと同じ）。

カーソルの状態は `sql::transaction::ActiveTxn` の新フィールド
（`cursors: sql::cursor::CursorRegistry`）として持つ。これにより次の性質が
追加コードなしで成り立つ:

- `COMMIT`／`ROLLBACK`／`fail()`／`release_if_expired`／接続断（`SessionTransaction`
  の drop）のいずれでも、`sql::cursor::CursorRegistry`（engine 側の実体）が
  自動的にクローズされる。ただし wire-server 拡張クエリプロトコルの portal
  （`PortalState::Suspended`）は `FETCH` 実行時点で既に送出用フレームへ
  エンコード済みの行を独立に保持するため、この `Drop` だけでは portal 側の
  残存を防げない——`CLOSE`／`COMMIT`／`ROLLBACK`／再 `DECLARE` を挟んでからの
  portal 再開を拒否する追加の検証が必要（§8 参照。PR #1049 レビュー指摘）。
- `Failed` 中の `FETCH`／`CLOSE`／`DECLARE` は、既存の parse 前判定
  （`is_rollback_statement`）によって `25P02` になる。

**保持期間の上限**: カーソルの寿命はトランザクションの寿命以下になる。つまり
`TransactionLimits::max_duration`（実装既定 20 秒）が上限になる。期限切れの
検出は既存の `SessionTransaction::release_if_expired`（wire 層の受信待ち
タイムアウトの切り詰めを含む）がそのまま担う。

遅延評価（`FETCH` のたびに検索本体を再実行する方式）を採らない理由: 後から
同じテーブルへ自分で書き込んだときに `written_tables` 判定と矛盾するうえ、
読み取りトランザクションの寿命を管理するコードを新たに引き回す必要が出る。

### 2. スナップショットの一貫性（TABLE-3）

`BEGIN` が単一ライタのゲートを保持しているため、`DECLARE` の時点で見える
コミット済み状態は `BEGIN` 時点のスナップショットと一致する。

- 同じトランザクションで**既に書き込んだ**テーブルに対する `DECLARE` は、
  既存の `written_tables` 判定で `0A000` にする（`sql::transaction`
  モジュールドキュメント「読み取りの既知の逸脱」の踏襲）。
- `DECLARE` の**後**に自トランザクションが同じテーブルへ書き込んでも、
  カーソルの行集合は変わらない（`DECLARE` は `written_tables` へ追加
  しないため書き込み自体は妨げない一方、確定済みのカーソルはその書き込みを
  一切参照しない）。
- 他セッションは `COMMIT`／`ROLLBACK` までライタを取得できない
  （`55P03` で待たされる）。したがって `FETCH` の途中で他セッションの
  commit がカーソルに混ざることは構造的に起きない。

### 3. RLS（RLS-7）

`DECLARE` は既存の実行器（`execute_validated_in_session`）を通すため、
RLS-7 の暗黙適用がそのまま効く。カーソルは接続ごとの `SessionTransaction`
に紐づき、`PolicyContext` は接続内で変わらない。カーソル名の名前空間も
セッション単位なので、他セッションのカーソル名は常に「不在」（`34000`）と
同じ応答になり、存在情報は漏れない（固定文言のみを保持し、カーソル名・
他セッション所有かどうかを一切含めない）。

### 4. 構文（許可リスト方式・規範形のみ）

- **`DECLARE`**: 受理するのは `DECLARE <name> CURSOR FOR <SELECT...>` のみ。
  `FOR` より後のトークン列は再トークナイズせず
  `sql::allowlist::validate_sql_tokens` へそのまま渡す（第 2 のパーサを
  作らない設計）。受理する内側 `SELECT` は集計 `SELECT`（SQL-13・SQL-14）・
  広域取得 `SELECT`（SQL-15）のみ。ベクトル順位付けの検索 `SELECT`・
  `EXPLAIN`・`SET`／`CREATE FUNCTION` は許可形状に一致せず `42601`。
  `SCROLL`／`NO SCROLL`／`BINARY`／`INSENSITIVE`／`ASENSITIVE`／`WITH HOLD`／
  `WITHOUT HOLD` はいずれも `42601`（規範形のみ受理。`name` の直後が必ず
  `CURSOR` であることを要求する構造上、これらの修飾語が割り込むと自然に
  拒否される）。
- **`DECLARE` のカタログ照合の遅延とビュー**: 構文解析段（`core.rs::parse_tokens`）
  は内側 `SELECT` の構造検証のみを行い、カタログを参照しない（トランザクション
  外の `DECLARE` を常に `25P01` にするため）。`Active` なトランザクション内の
  実行時に、保持しておいた内側 `SELECT` のトークン列を実カタログに対して
  `validate_sql_tokens` で再検証する（`sql::cursor::validate_declare_inner`）。
  通常の広域取得 `SELECT` と同じ経路を通るため、FROM がビュー（TABLE-18・
  SQL-23）を指す場合も同じビュー展開（基底テーブル＋ビュー由来述語・列スコープ
  検査）が適用され、RLS は参照セッション自身の `PolicyContext` で暗黙適用される
  （§3）。同一トランザクション内の書き込み済み判定もビュー展開後の基底テーブル
  名で行う（PR #1049 レビュー指摘対応）。
- **`FETCH`**: 受理するのは `FETCH [FORWARD] <n> FROM <name>` のみ。`n` は
  `sql::parser::validate_search_limit`（`1..=MAX_SEARCH_K`）を再利用する。
  `IN`／`ALL`／`NEXT`／`BACKWARD`／`ABSOLUTE` はいずれも `n` の位置に
  `Token::Number` を要求する構造上、自然に `42601` へ落ちる。
- **`CLOSE`**: 受理するのは `CLOSE <name>` のみ。`CLOSE ALL` は明示的に
  拒否する（`ALL` 自体は識別子として妥当な形のため、拒否しないと「たまたま
  `ALL` という名前のカーソルが存在する場合にだけ動く曖昧な構文」になって
  しまう）。
- **カーソル名**: `catalog::validate_identifier`（テーブル名・列名と同じ
  `[A-Za-z_][A-Za-z0-9_]*`・63 バイト以下）で検証する。テーブル名・列名と
  同じくケースを正規化せず検証する（本 DB の識別子は既に大文字小文字を
  区別する運用であり、カーソル名だけ PostgreSQL 互換の小文字畳み込みを
  行うと運用が二重になるため、既存の識別子契約に揃えた）。
- **`$n`（WIRE-12）**: `DECLARE`／`FETCH`／`CLOSE` で `$n` を使うことは今回の
  範囲では受理せず `42601` にする（fail-closed。範囲を縮小）。
  `sql::params::validate_param_positions` が先頭語を見て一律拒否する
  （既存の `WHERE` 等価述語パターンが文の先頭語を見ずに `WHERE` キーワードの
  位置だけで判定するため、先頭語を見て弾かないと `DECLARE ... FOR SELECT
  ... WHERE col = $1` のような形が誤って受理されてしまう）。

### 5. 上限とエラー分類（実装既定値）

| 事象 | `wire_code` | 備考 |
| ---- | ---- | ---- |
| トランザクション外の `DECLARE` | `25P01` | `sql::allowlist::SqlSurfaceError::NoActiveSqlTransaction` を再利用 |
| 不在のカーソル名に対する `FETCH`／`CLOSE`（トランザクション外も含む） | `34000` | 新設 `ErrorClass::InvalidCursorName` |
| `n` の範囲外 | `22000` | `validate_search_limit` を再利用 |
| 同時カーソル数の上限超過（`MAX_CURSORS_PER_SESSION`＝16 本目の次） | `54000` | 内側の `SELECT` を実行する**前**に判定する |
| セッション内でカーソルが保持する確定行の合計バイト数の上限超過（`MAX_CURSOR_BYTES_PER_SESSION`＝16 MiB） | `54000` | `wire-server::limits::MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION` と同じ根拠 |
| カーソル名の重複 | `22000` | spec 未定義。PostgreSQL の `42P03` は ERR-6 の管轄表に無く独自コードの新設は禁止のため、「構文上は受理された値が不正」という `22000` の判定境界に寄せた（実装既定値。確定はオーナー判断待ち） |
| ベクトル順位付けの `SELECT`・規範形以外の構文 | `42601` | |
| 同じトランザクションで書き込み済みのテーブルに対する `DECLARE` | `0A000` | 既存の既知の逸脱（`sql::transaction`） |
| `Failed` 中の `DECLARE`／`FETCH`／`CLOSE` | `25P02` | 既存の仕組み |

- `Active` 中にエラーが起きた場合は、既存どおり種類を問わず `Failed` へ
  遷移させる（PostgreSQL と同じ）。
- `DECLARE`／`FETCH`／`CLOSE` は `SessionTransaction::check_and_register_statement`
  の文数上限（1,000）の計数対象。`operation_id` は常に `None`
  （`core::parsed_operation_id`）。
- エラーメッセージは汎用の英語文言（例: `cursor does not exist`）にし、
  カーソル名や他の状態を含めない。

### 6. `EXPLAIN`・拡張クエリプロトコル（WIRE-11）

第 2 の実行器を作らないため、`FETCH` にも Describe を対応させる。
`EngineCore::describe_parsed_in_txn(session, txn, parsed)` を新設し、
`ParsedSql::Cursor(CursorStatement::Fetch { name, .. })` に限り `txn` が
`Active` で `name` のカーソルが実在する場合はその結果列メタデータ
（`DECLARE` 時点で確定済みの `QueryResult::columns`。検索本体を再実行しない）
を返し、実在しない場合は `34000` を返す。`DECLARE`／`CLOSE` は結果列を持たない
（`None`）。それ以外の `ParsedSql` は `describe_parsed_in_session` と完全に
同一の判定へ委譲する。`wire-server::extended_query::handle_describe`／
`handle_bind` が接続単位の `SessionTransaction` を渡してこの API を呼ぶ
よう結線した。

`FETCH` の portal を `max_rows` で分割送出する処理は、既存の `Suspended`
の仕組みにそのまま乗る。カーソル位置は Execute 1 回につき 1 度だけ進み、
再 Execute で再実行はしない（既存の契約）。

### 7. wire 応答（`CommandComplete` タグ。pg 互換）

- `DECLARE` → `DECLARE CURSOR`（固定タグ）
- `FETCH` → `TagShape::Dynamic("FETCH")`（`FETCH <実際に送出した行数>`）。
  `RowDescription`／`DataRow` を伴う（検索 SELECT・`EXPLAIN` と同じエンコード
  を再利用する）。
- `CLOSE` → `CLOSE CURSOR`（固定タグ）

`sql::statement_splitter::classify_statement` は `DECLARE`／`FETCH`／`CLOSE`
を `StatementEffect::ReadOnly` に分類する（redb の commit を伴わないため）。

### 8. wire-server 拡張クエリの portal 束縛（PR #1049 レビュー指摘対応）

`FETCH` を拡張クエリプロトコル（Bind/Execute）で実行すると、`max_rows` に
より結果が `PortalState::Suspended` として中断保持されうる（§6）。この
中断保持分は wire-server 側が既にエンコード済みのフレームとして保持して
おり、`CursorRegistry` の `Drop`（§1）とは寿命が独立している。名前付き
portal は Sync（'S'）のたびにしか破棄されないため、同一 Sync サイクル
（`BEGIN` → `DECLARE` → `FETCH`（一部だけ Execute で中断保持）→ `CLOSE`／
`COMMIT`／`ROLLBACK` → 再 Execute）の中では、修正前は中断保持分がカーソル・
トランザクションの終了を一切確認せずにそのまま送出できてしまっていた
（codex-review P1「終了したカーソルの FETCH portal から行を送出できる」）。

是正として、`FETCH` の実行が成功した時点（`wire-server::extended_query::
execute_portal`）で以下の 2 つを portal へ束縛し、再開前に突き合わせる:

- **トランザクション世代**（`engine::sql::transaction::SessionTransaction::
  active_generation`）: `BEGIN` が成功するたびに 1 つ進む単調増加カウンタ。
  `COMMIT`／`ROLLBACK` は `Idle` へ戻るため世代が失われ、次の `BEGIN` は
  新しい世代になる——同一名で再 `BEGIN` しても不一致になる。
- **カーソル個体識別子**（`engine::sql::cursor::CursorRegistry::cursor_id`／
  `SessionTransaction::cursor_id`）: `DECLARE` のたびに払い出す単調増加値。
  `CLOSE` 後に同じ名前で再 `DECLARE` した別インスタンスと区別する。

再開時にどちらか一方でも不一致なら、蓄積済みフレームを送出せず portal を
`Failed` へ倒し `34000`（invalid cursor name）を返す。`FETCH` 以外の文
（通常の検索 `SELECT`・集計・広域取得等）から作った portal は束縛が常に
`None` のためこの検証の対象外で、既存の挙動は変えない。

検証対象は保持行を送出し得る `Suspended` のみとする。全行を送出済みの
`Done` portal の再 Execute は、`FETCH` 以外の portal と同じく保持済みの
完了タグ（`CommandComplete`）を再送するだけで行を一切送出しないため、
`CLOSE`／`COMMIT`／`ROLLBACK` を挟んでいても `34000` にはしない
（PR #1049 レビュー指摘対応）。

## 対象外・申し送り

- 重複カーソル名の `wire_code`（PostgreSQL の `42P03` は ERR-6 の表に無い）を
  spec 側でどう扱うか → オーナー判断・spec リポの課題として申し送り。
- `SCROLL`／`WITH HOLD`／`BINARY`／`FETCH ALL`／`BACKWARD`／`CLOSE ALL`／
  `MOVE`、二重引用符のカーソル名（psycopg 3 の `ServerCursor` はこの形式を
  使うため非対応になる）。
- ベクトル順位付けの `SELECT`（SQL-1〜4）のカーソル化（WIRE-15 の対象外）。
- スカラー `ORDER BY`（SQL-25 (a)。未実装）の形のカーソル化。`OFFSET`
  （SQL-25 (b)・TASK-209、Issue #916）は広域取得・`GROUP BY` 集計の `LIMIT`
  経由で実装済みで、本カーソル経路（`bind_scan`／`bind_group_by_clause` を
  そのまま通す）でも追加実装なしに使える（`crates/engine/tests/
  sql25_offset.rs::declare_cursor_with_offset_matches_direct_execution`
  参照。詳細は [sql-offset-paging.md](sql-offset-paging.md)）。
- 自トランザクションの未 commit 変更の可視化（Issue #942 の既知の逸脱を
  引き継ぐ）。
- `DECLARE`／`FETCH`／`CLOSE` での `$n` パラメータ（今回は `42601`）。
- NoSQL 表層のカーソル（`op` の許可リストに追加しない。`34000` は NoSQL
  表層の実要求からは到達不能として射影のみを固定する）。
- 上限（16 本・16 MiB）の既定値の確定 → オーナー判断。
