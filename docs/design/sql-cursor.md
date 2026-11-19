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

- `DECLARE <name> [SCROLL | NO SCROLL | INSENSITIVE | ASENSITIVE]... CURSOR [WITHOUT HOLD] FOR <SELECT>`（修飾語は Issue #3334）
- `FETCH [ <direction> ] { FROM | IN } <name>`（向きは §4。Issue #3335）
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

`LIMIT` 省略時の結果行数上限も `DECLARE` 時点で数える（[limit-omitted-result-cap.md](limit-omitted-result-cap.md) D5）。

**権限検査（SQL-40・Issue #1967）**: 現在ロールが束縛済みのセッションでは、内側の問い合わせが参照する relation の
SELECT 権限を `DECLARE` の時点で検査し（不足は `42501` で Tx は `Failed`）、`FETCH`／`CLOSE` では再検査しない。
結果は `DECLARE` で確定済みで、`FETCH` は確定済みの行を払い出すだけで新しい読み取りを行わないため。`DECLARE` の後に
`REVOKE` やロールの変更があっても、開いているカーソルには影響しない（PostgreSQL と同じ）。詳細は
[role-privilege-model.md](role-privilege-model.md) §12。

遅延評価（`FETCH` のたびに検索本体を再実行する方式）を採らない理由: 後から
同じテーブルへ自分で書き込んだときに結果が変わってしまい（INSENSITIVE を満たせない）、
読み取りトランザクションの寿命を管理するコードを新たに引き回す必要が出る。

### 2. スナップショットの一貫性（TABLE-3）

writer（単一ライタの書き込み許可）は最初の書き込み文で取得する（#2396・#2400）。
このため、カーソルの具体化は writer の取得前後の 2 段に分かれる（WIRE-15。詳細は
`explicit-transaction.md` 1a、実装は #2403、検証は #2411）。

- writer の取得**前**に宣言したカーソルは、`BEGIN` 時点のスナップショットで行集合を確定する。
  取得後も同じ行集合を払い出し、取得後の自分の書き込みも他セッションの commit も混ざらない。
- writer の取得**後**に宣言したカーソルは、取得時点の最新の確定状態に自トランザクションの
  未 commit の変更を加えた状態で確定する。
- 読み取りだけの区間では他セッションの commit は待たされず、`DECLARE` より前に入りうる。
  カーソルに混ざらないのは、`DECLARE` の時点で行集合が確定し `FETCH` が読み取り源に触れない
  ためである。
- 取得前に読んだ表へ他セッションが commit した場合、writer の取得時に `40001` になる
  （SQL-31。カーソルの内側 SELECT も読み取り集合に入る）。

- 同じトランザクションで**既に書き込んだ**テーブルに対する `DECLARE` は、
  自トランザクションの未 commit の変更を反映した内容で確定する（Issue #1179。
  dirty テーブルがあるとき共有書き込みトランザクションを読み取り源にして内側
  SELECT を実行する。`docs/design/explicit-transaction.md`「6.」参照）。変更が
  なければ従来どおり確定済みスナップショットを読む。
- `DECLARE` の**後**に自トランザクションが同じテーブルへ書き込んでも、
  カーソルの行集合は変わらない（`DECLARE` は書き込みではなく書き込み自体は
  妨げない一方、確定済みのカーソルはその書き込みを一切参照しない）。
- 他セッションは writer を保持している間、書き込みを `55P03` で待たされる。いずれの場合も
  `FETCH` の途中で他セッションの commit がカーソルに混ざることは構造的に起きない。

### 3. RLS（RLS-7）

`DECLARE` は既存の実行器（`execute_validated_in_session`）を通すため、
RLS-7 の暗黙適用がそのまま効く。カーソルは接続ごとの `SessionTransaction`
に紐づき、`PolicyContext` は接続内で変わらない。カーソル名の名前空間も
セッション単位なので、他セッションのカーソル名は常に「不在」（`34000`）と
同じ応答になり、存在情報は漏れない（固定文言のみを保持し、カーソル名・
他セッション所有かどうかを一切含めない）。

### 4. 構文（許可リスト方式）

- **`DECLARE`**: 受理するのは `DECLARE <name> [修飾語]... CURSOR [WITH HOLD | WITHOUT HOLD] FOR <SELECT...>` のみ（修飾語は下記の表）。
  `FOR` より後のトークン列は再トークナイズせず
  `sql::allowlist::validate_sql_tokens` へそのまま渡す（第 2 のパーサを
  作らない設計）。受理する内側 `SELECT` は集計 `SELECT`（SQL-13・SQL-14）・
  広域取得 `SELECT`（SQL-15）・JOIN `SELECT`（Issue #3175。INNER／OUTER／CROSS／NATURAL／USING・
  集計形・`LIMIT` 省略形〔SQL-34〕。CTE を入力に持つ JOIN を含む。Issue #3176）・集合演算
  （UNION／UNION ALL／INTERSECT／EXCEPT。Issue #3176）のみ。検証入口は
  `sql::allowlist::validate_cursor_query_tokens`（cursor 専用の CTE 評価モード。JOIN 主クエリだけを
  開き、具体化・派生表・関数呼び出し・再帰 CTE は閉じたまま）。許可リストと
  `core.rs::execute_cursor_inner_query` の上限付き実行アームは 1 対 1 で、対応の無い形は
  上限なしの通常経路へ流さず `Internal` で止める（fail-closed）。
  JOIN を枝に持つ集合演算・`WITH` ＋ 集合演算の主クエリは通常の SELECT でも未対応のため `42601`。
  ベクトル順位付けの検索 `SELECT`・
  `EXPLAIN`・`SET`／`CREATE FUNCTION` は許可形状に一致せず `42601`。
- **`DECLARE` の修飾語（Issue #3334・WIRE-27 (2)(5)(6)）**: PG17 の文法順（`CURSOR` の前に任意順の
  修飾語、`CURSOR` と `FOR` の間に hold 指定）に合わせる。トークン列の許可リスト照合だけで、再結合・
  再トークナイズはしない。

  | 形 | 扱い |
  | -- | ---- |
  | `SCROLL` | 受理。逆方向の取得を許す |
  | `NO SCROLL`・指定なし | 受理。逆方向の取得は `55000`（`cursor can only scan forward`） |
  | `INSENSITIVE`・`ASENSITIVE`・`WITHOUT HOLD` | 受理して何もしない（もともと INSENSITIVE） |
  | 同じ修飾語の重複 | 受理（冪等。PG と同じ） |
  | `WITH HOLD` | `0A000`（受け皿 #3340。黙って無視して受理する経路は作らない） |
  | `BINARY` | `0A000`（意図した拒否。カーソルの行の出力形式は Bind の format code に従う既存の経路しか無い） |
  | `SCROLL` と `NO SCROLL`、`INSENSITIVE` と `ASENSITIVE` の併記 | `42601` |
  | 未知の語・`NO` の後に `SCROLL` が無い・`CURSOR` の後の修飾語・`CURSOR` より前の `WITH HOLD`・`HOLD` の欠落／重複／相反 | `42601` |

  判定順は 修飾語・hold の構文（`42601`）→ 内側 SELECT の構造検証 → `WITH HOLD`／`BINARY` の `0A000`
  （構文が壊れた文は常に構文エラー）。`0A000` は構文解析段で返るため、Tx 内では `Failed` になる。
  **意図した PG17 との差**: (1) 指定なしは `NO SCROLL` と同じで、PG が計画により許す逆方向を許さない。
  (2) 矛盾指定は PG17 17.11 の実測で `42P11`（invalid_cursor_definition）だが、ErrorClass を新設せず
  `42601` で拒否する（Issue #3337 で実測。一覧は後述「PG17 との意図した差」）。
- **末尾の `;`（Issue #3171）**: `FETCH`・`CLOSE` は末尾の単一 `;` を受理する（`sql::lexer::strip_one_trailing_semicolon`）。`;;` は `42601`。`DECLARE` は内側 SELECT の検証が末尾 `;` を 1 個受理する。判断は [explicit-transaction.md](explicit-transaction.md) の「末尾 `;` の扱い」節。
- **`DECLARE` のカタログ照合の遅延とビュー**: 構文解析段（`core.rs::parse_tokens`）
  は内側 `SELECT` の構造検証のみを行い、カタログを参照しない（トランザクション
  外の `DECLARE` を常に `25P01` にするため）。`Active` なトランザクション内の
  実行時に、保持しておいた内側 `SELECT` のトークン列を実カタログに対して
  `validate_sql_tokens` で再検証する（`sql::cursor::validate_declare_inner`）。
  通常の広域取得 `SELECT` と同じ経路を通るため、FROM がビュー（TABLE-18・
  SQL-23）を指す場合も同じビュー展開（基底テーブル＋ビュー由来述語・列スコープ
  検査）が適用され、RLS は参照セッション自身の `PolicyContext` で暗黙適用される
  （§3）。読み取り源（`BEGIN` 時点のスナップショット／共有 write txn／確定状態）は
  `SessionTransaction::read_source` の三分岐で選ぶ（Issue #1179 で `0A000` 拒否から
  未 commit 変更の反映へ変更）。
- **JOIN の内側（Issue #3175）**: 実行は通常の SELECT と同じ `sql::join` の実行器
  （参照セッション自身の `PolicyContext` を各辺へ独立に適用）を使い、第 2 の実行器を作らない。
  カーソルの残容量（§1）は JOIN の累計バイト予算の上限として渡し、生成中に打ち切る
  （`sql::join::execute_with_result_byte_cap`）。JOIN の予算は中間行も計上するため、
  カーソル内の JOIN は通常の SELECT より早く `54000` に達しうる（安全側の差）。
  リレーション数・入出力行数・比較回数・結果行数の上限は起動時設定のまま。
  SELECT 権限（#1967）は全 relation を `DECLARE` の時点で検査し、`FETCH` では再検査しない。
- **集合演算・CTE 入力の JOIN の内側（Issue #3176）**: 集合演算は
  `sql::set_op::execute_with_budget_and_result_limits`、CTE 入力の JOIN は通常の SELECT と共有する
  `EngineCore::execute_cte_join`（`sql::join::JoinBudget::capped_at` で文全体の予算を縮める）で
  実行し、カーソルの残容量を生成中の累計バイト予算の上限にする。いずれも枝・中間行・CTE 本文の
  評価を計上するため、通常の SELECT より早く `54000` に達しうる（安全側の差）。
  SELECT 権限は全枝・CTE 本文の基底表・JOIN の相手表を `DECLARE` の時点で検査する。
- **`FETCH`**（Issue #3173・#3174・#3335・WIRE-27 (1)(2)）: 受理するのは
  `FETCH [ <direction> ] { FROM | IN } <name>`。`<direction>` は `NEXT`・`PRIOR`・`FIRST`・`LAST`・
  `ABSOLUTE <n>`・`RELATIVE <n>`・`<n>`・`ALL`・`FORWARD [<n> | ALL]`・`BACKWARD [<n> | ALL]`
  （`<n>` は `+`／`-` 付きの整数）。向きの省略は `NEXT`、件数なしの `FORWARD` は `NEXT`、件数なしの
  `BACKWARD` は `PRIOR` と同じ。`IN` は `FROM` の別名。負の件数は向きを反転する（`FETCH -2` は
  `BACKWARD 2`、`BACKWARD -2` は `FORWARD 2`）。件数 0（`FETCH 0`・`FORWARD 0`・`BACKWARD 0`・
  `RELATIVE 0`）は位置を動かさず現在の行を返す（従来の `22000` から受理へ変更。行の上でなければ 0 行）。
  構文解析は `sql::cursor::FetchDirection`（`Forward`／`Backward`／`Current`／`Absolute`／`Relative`）を作り、
  行数として扱う件数（`<n>`・`FORWARD <n>`・`BACKWARD <n>`。絶対値）は `validate_search_limit_with`
  （`1..=max_result_rows`）を通す。`ABSOLUTE`／`RELATIVE` の数値は位置で返す行は高々 1 行のため行数上限は
  適用せず、`u32` に収まらない値だけ `42601`（O(1) の clamp で処理し、走査も確保もしない）。
  `FROM`／`IN` の省略（`FETCH c`・`FETCH NEXT c`）・余剰な件数（`PRIOR 1`）・符号の重複は前方向へ
  読み替えず `42601` で拒否する（fail-closed。`FROM`／`IN` の省略は受け皿 #3333）。
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

- **`MOVE`**（Issue #3336・WIRE-27）: `MOVE [<向き>] { FROM | IN } <name>`。向き・件数の文法、
  上限検証（`22000`）、`42601` の拒否集合は `FETCH` と完全に同一で、`sql::cursor` の共通関数
  （`parse_motion_tokens`）を `validate_fetch_tokens`／`validate_move_tokens` が呼ぶ。違いは
  エラー文言の文名だけ。`FROM`／`IN` の省略（`MOVE c`）は `FETCH` と同じく `42601`。
  PG は任意の大きさの件数を受理するが、本実装は `FETCH` と同じく `u32` と `max_result_rows` で
  制限する（意図した差。行集合は `DECLARE` 時点で cap 済みなので、上限超の件数は `ALL` で代替でき機能は失われない）。

### 5. 上限とエラー分類（実装既定値）

| 事象 | `wire_code` | 備考 |
| ---- | ---- | ---- |
| トランザクション外の `DECLARE` | `25P01` | `sql::allowlist::SqlSurfaceError::NoActiveSqlTransaction` を再利用 |
| 不在のカーソル名に対する `FETCH`／`CLOSE`（トランザクション外も含む） | `34000` | 新設 `ErrorClass::InvalidCursorName` |
| `n` の範囲外（行数として扱う件数の絶対値） | `22000` | `validate_search_limit` を再利用。件数 0 は受理（現在の行） |
| NO SCROLL（指定なしを含む）への後ろへ戻る取得（§9 の表） | `55000` | 文言 `cursor can only scan forward`。位置は変えない。Issue #3335 |
| `DECLARE` 内側 SELECT の結果行数上限超過（`LIMIT` 省略・`ALL` の受理後） | `54000` | `DECLARE` を拒否しカーソルは登録しない。設計は [limit-omitted-result-cap.md](limit-omitted-result-cap.md) |
| `FETCH ALL` の残り行数が結果行数上限を超える場合 | `54000` | 部分払い出しをせず位置も進めない多層防御（`DECLARE` で cap 済みのため SQL 経路からは通常到達しない） |
| 同時カーソル数の上限超過（`MAX_CURSORS_PER_SESSION`＝16 本目の次） | `54000` | 内側の `SELECT` を実行する**前**に判定する |
| セッション内でカーソルが保持する確定行の合計バイト数の上限超過（`MAX_CURSOR_BYTES_PER_SESSION`＝16 MiB） | `54000` | `wire-server::limits::MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION` と同じ根拠 |
| カーソル名の重複 | `22000` | PostgreSQL は `42P03` を返すが、本リポは既存の `22000`（`InvalidInput`）へ写像した（実装既定値。ERR-6 ポインタ） |
| ベクトル順位付けの `SELECT`・規範形以外の構文 | `42601` | |
| 同じトランザクションで書き込み済みのテーブルに対する `DECLARE` | 成功（未 commit 変更を反映） | Issue #1179 で `0A000` から変更 |
| `Failed` 中の `DECLARE`／`FETCH`／`CLOSE` | `25P02` | 既存の仕組み |
| 内側がデータ変更 CTE（書き込みを伴う `WITH`。単一表・JOIN の主クエリとも） | `0A000` | 意図的な拒否（PG17 と同じくカーソルに載せない・fail-closed）。検証の段で固定し書き込みへ到達しない。Issue #3176 |

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
よう結線した。`MOVE` は行を返さないため Describe（Statement／Portal）は `DECLARE`／`CLOSE` と同じく
NoData、Execute は Command 応答（`MOVE n`）で portal の中断（`Suspended`）は起きない。

`FETCH` の portal を `max_rows` で分割送出する処理は、既存の `Suspended`
の仕組みにそのまま乗る。カーソル位置は Execute 1 回につき 1 度だけ進み、
再 Execute で再実行はしない（既存の契約）。

### 7. wire 応答（`CommandComplete` タグ。pg 互換）

- `DECLARE` → `DECLARE CURSOR`（固定タグ）
- `FETCH` → `TagShape::Dynamic("FETCH")`（`FETCH <実際に送出した行数>`）。
  `RowDescription`／`DataRow` を伴う（検索 SELECT・`EXPLAIN` と同じエンコード
  を再利用する）。
- `CLOSE` → `CLOSE CURSOR`（固定タグ）
- `MOVE` → `MOVE <n>`（行なし。`RowDescription` を送らない）。n は同じ向きの `FETCH` が返す行数。

`sql::statement_splitter::classify_statement` は `DECLARE`／`FETCH`／`MOVE`／`CLOSE`
を `StatementEffect::ReadOnly` に分類する（redb の commit を伴わないため）。

### 8. wire-server 拡張クエリの portal 束縛（PR #1049 レビュー指摘対応）

`FETCH` を拡張クエリプロトコル（Bind/Execute）で実行すると、`max_rows` に
より結果が `PortalState::Suspended` として中断保持されうる（§6）。この
中断保持分は wire-server 側が既にエンコード済みのフレームとして保持して
おり、`CursorRegistry` の `Drop`（§1）とは寿命が独立している。名前付き
portal は Sync（'S'）まで破棄されないため（明示トランザクション中は Sync 後も
同世代の名前付き portal が保持され、トランザクション終了時に失効する。
WIRE-11・Issue #1174。Sync 越し保持分もカーソル・世代の照合は同じ）、同一 Sync サイクル
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

`ROLLBACK TO SAVEPOINT` も無効化要因に加わる（Issue #1946・SQL-69 (5)）。savepoint より後に
宣言したカーソルは engine が閉じるので、それに束縛された中断 portal は `cursor_id` が解決できず
`34000` になる。`ROLLBACK TO` は Tx 世代を変えないため、世代ではなくカーソル識別子で弾く。
それ以前のカーソルは位置を保ったまま残り、portal は続きから送出できる。詳細は
[explicit-transaction.md](explicit-transaction.md) の「savepoint 巻き戻しとカーソル」節。

### 9. 位置モデル（Issue #3334・WIRE-27 (5)(6)）

`sql::cursor::CursorPosition` が、確定済み行集合（N 行）の上の位置を `0..=N+1` で持つ。
`0` は先頭の前、`k`（1..=N）は k 行目の上、`N+1` は末尾の後。位置は `usize` 1 個で、行集合の複製を作らない。
不変条件 `pos <= N+1` はすべての遷移（`saturating_*`・`min`）で守り、単体テストで 1 始まりの参照モデルと
突き合わせて固定する。

- 前方向 `forward(n)`（現在位置 p）: p+1..=min(p+n, N) の行を返し、p+n <= N なら位置は p+n、それ以外は N+1。
  N 行をちょうど取り切った直後の `FETCH n` は 0 行で位置は N+1。`forward_all` は残り全行を返し位置は N+1。
- 逆方向 `backward(n)`: 位置は `p.saturating_sub(n)`。p-1 行目から max(新位置,1) 行目までを降順で返す
  （N+1 からは N 行目から）。p が 0／1 なら 0 行。
- `55000`（非 SCROLL への逆方向）・`54000`（`ALL` の行数上限超過）では位置を変えない。
- メモリ課金は SCROLL／NO SCROLL で同じ（`estimate_result_bytes`）で、16 MiB・16 本の上限に数える。
- 取得口は `CursorRegistry::fetch(name, FetchDirection, max_rows)` の 1 本（Issue #3335 で前方向・全件・逆方向を
  統合し、`#[allow(dead_code)]` を外した）。`MOVE`（#3336）は `CursorPosition::plan`／`apply` を行の切り出しなしで再利用する（後述）。

#### FETCH の向きの意味（Issue #3335・WIRE-27 (1)(2)）

N を行数、p を現在位置とし、「行 t を返す」は 1 ≦ t ≦ N のときだけ 1 行を返すこと。位置の更新は
`CursorPosition::plan`（状態を変えない純粋関数）で計算し、`apply` で確定する。

| 向き | 新位置 | 返す行 |
| ---- | ------ | ------ |
| `Forward(n)`／`Forward(ALL)`・`Backward(n)`／`Backward(ALL)` | 上記の `forward`／`forward_all`／`backward` | 既存どおり（逆方向は降順） |
| `ABSOLUTE k`（k>0。`FIRST` は 1） | min(k, N+1) | 行 t |
| `ABSOLUTE k`（k<0。`LAST` は -1） | \|k\| ≦ N なら N+1-\|k\|、それ以外は 0 | 行 t |
| `ABSOLUTE 0` | 0 | 0 行 |
| `RELATIVE k`（k>0） | min(p+k, N+1) | 行 t（途中の行は返さない） |
| `RELATIVE k`（k<0） | p.saturating_sub(\|k\|) | 行 t |
| 件数 0 | p のまま | 1 ≦ p ≦ N なら行 p、それ以外は 0 行 |

NO SCROLL のカーソルで `55000`（位置は変えない）にする条件は次のとおり。PostgreSQL 17.11 の実測
（`crates/engine/tests/data/cursor_directions_pg17.tsv` の 700 ケース）と全件一致を確認済み（Issue #3337）。

| 向き | NO SCROLL での扱い |
| ---- | ------------------ |
| `PRIOR`・`BACKWARD`（件数なし・n・0・ALL）・`FORWARD -n`・`FETCH -n`・`RELATIVE -n`・`ABSOLUTE -n`（`LAST` を含む） | 常に `55000` |
| `FETCH 0`・`FORWARD 0`・`RELATIVE 0`（`FETCH` のみ。`MOVE` は後述） | 1 ≦ p ≦ N（行の上）なら `55000`、それ以外は 0 行で受理 |
| `ABSOLUTE k`（k>0。`FIRST` を含む） | k > p なら受理、k ≦ p なら `55000`（末尾の後 p = N+1 では k ≦ N+1 が `55000`、k ≧ N+2 は 0 行で受理） |
| `ABSOLUTE 0` | p == 0 なら受理（0 行）、それ以外は `55000` |
| `NEXT`・`n`・`FORWARD n`・`ALL`・`RELATIVE k`（k>0）・`BACKWARD -n` | 常に受理 |

判定順は 構文と件数（`42601`／`22000`。構文解析段）→ カーソルの不在（`34000`）→ NO SCROLL（`55000`）→
`ALL` の行数上限（`54000`）→ 位置の更新と行の切り出し。いずれの拒否でも位置は変えない。

- **PG17 実測で是正した境界（Issue #3337）**: 当初は PG17 のソースからの導出で拒否側に倒していた次の 2 点を、
  実測に合わせて受理へ変えた。(1) 末尾の後からの `ABSOLUTE k`（k ≧ N+2）は 0 行で受理（`FETCH`・`MOVE` 共通）。
  (2) `MOVE` の件数 0 と先頭の前の `MOVE BACKWARD ALL` は NO SCROLL でも受理（後述）。`FETCH` の
  件数 0（行の上）・`FETCH BACKWARD ALL`（先頭の前を含む）は PG17 でも `55000` のまま。
- **Issue の方針の行との差**: Issue 本文の方針は NO SCROLL の 6 つの向きを一律に `55000` と読めるが、仕様
  （WIRE-27 (2)）と PG17 は「後ろへ戻る取得」だけを拒否する。前方向の `RELATIVE k`（k>0）や先の位置への
  `ABSOLUTE` は NO SCROLL でも受理する。

#### MOVE（Issue #3336・WIRE-27）

`CursorRegistry::move_cursor(name, FetchDirection) -> u64` が `fetch` と同じ前段（不在 `34000`→
NO SCROLL の後退走査 `55000`。共通の private 関数）と同じ `plan`／`apply` で位置だけを動かす。
行は複製しないので `ALL` の行数上限（`54000`）は適用しない（結果メモリが増えない）。いずれの拒否でも位置は変えない。
NO SCROLL の境界は `FETCH` と同じ判定だが、PG17 17.11 の実測に合わせて次の `MOVE` だけ例外にする
（Issue #3337）: 件数 0 の向き（`MOVE 0`・`FORWARD 0`・`RELATIVE 0`・`BACKWARD 0`）は走査せず位置を変えないため
NO SCROLL でも受理し、タグは行の上なら `MOVE 1`・それ以外は `MOVE 0`。先頭の前の `MOVE BACKWARD ALL` も
受理（`MOVE 0`）。不在 `34000` の検査は先に行う。

タグの n は「同じ向きの `FETCH` が返す行数」（PG17 17.11 の実測と一致）。N=5・位置 0 からの例:

| 文 | タグ |
| -- | ---- |
| `MOVE FORWARD 2` | `MOVE 2` |
| `MOVE FORWARD 10` | `MOVE 5` |
| `MOVE ABSOLUTE 0` | `MOVE 0` |
| `MOVE ABSOLUTE 3` | `MOVE 1` |
| `MOVE ABSOLUTE 10` | `MOVE 0` |
| 位置 2 から `MOVE BACKWARD 1` | `MOVE 1` |
| 末尾の後から `MOVE BACKWARD ALL` | `MOVE 5` |
| 行の上で `MOVE RELATIVE 0`（SCROLL） | `MOVE 1` |

- 上のタグ件数と NO SCROLL の境界は PG17 17.11 の実測と一致を確認済み（照合は
  `crates/engine/tests/sql_named/sql_cursor_pg17_parity.rs`）。

#### PG17 との意図した差（Issue #3337）

| 形 | PG17 17.11 の実測 | 本実装 | 理由・受け皿 |
| -- | ----------------- | ------ | ------------ |
| 指定なしのカーソルの逆方向 | 計画次第で許す | NO SCROLL と同じ（`55000`） | 計画に依存しない決定的な挙動にする |
| `SCROLL` と `NO SCROLL`・`INSENSITIVE` と `ASENSITIVE` の併記 | `42P11` | `42601` | ErrorClass を新設しない |
| 同名カーソルの再 `DECLARE` | `42P03` | `22000` | 既存の契約 |
| `WITH HOLD` | 受理 | `0A000` | 受け皿 #3340 |
| `BINARY` | 受理 | `0A000` | 意図した拒否（上記） |
| `FROM`／`IN` の省略（`FETCH c`・`MOVE c`） | 受理（`NEXT`） | `42601` | 受け皿 #3333 |
| カーソル自身の実行が `55000` で失敗した後の `ROLLBACK TO` | そのカーソルは使えなくなる（以降 `55000`） | 位置を保って使い続けられる | 既存の契約（SQL-69 (5)・WIRE-27 (2) のポインタ先）を維持。PG17 に合わせるかは spec 側の判断事項 |

- 他の形（SCROLL・NO SCROLL × 向き × 位置 × `FETCH`／`MOVE`）に差は無い。
- 別の文のエラー後の `ROLLBACK TO`、SCROLL カーソルの位置が `ROLLBACK TO` で戻らないことは PG17 と一致する。

## 対象外・申し送り

- `WITH HOLD`（#3339・#3340）／`FROM`・`IN` 省略の `FETCH`（#3333）／`CLOSE ALL`／
  PL/pgSQL 本体内の `MOVE`（範囲外語として `0A000` のまま）、二重引用符のカーソル名（psycopg 3 の `ServerCursor` はこの形式を
  使うため非対応になる）。
- ベクトル順位付けの `SELECT`（SQL-1〜4）のカーソル化（WIRE-15 の対象外）。
- 主クエリが tail CTE（本文に `ORDER BY`／`LIMIT`／`OFFSET`）を直接読む単一表の `DECLARE`・
  `WITH RECURSIVE`・FROM の派生表・明示 `MATERIALIZED` の具体化経路（カーソルにバイト上限付きの
  具体化経路が無いため従来どおり `0A000`／`42601`。Issue #3176 の対象外）。評価後射影形ビュー
  （JOIN 本文のビュー・派生表）の `DECLARE`（`42601` のまま）。一時表を含む `DECLARE` は
  一時表の振り分け経路の対象外で受理されない（fail-closed）。
- スカラー `ORDER BY`（SQL-25 (a)。未実装）の形のカーソル化。`OFFSET`
  （SQL-25 (b)・TASK-209、Issue #916）は広域取得・`GROUP BY` 集計の `LIMIT`
  経由で実装済みで、本カーソル経路（`bind_scan`／`bind_group_by_clause` を
  そのまま通す）でも追加実装なしに使える（`crates/engine/tests/
  sql25_offset.rs::declare_cursor_with_offset_matches_direct_execution`
  参照。詳細は [sql-offset-paging.md](sql-offset-paging.md)）。
- `DECLARE`／`FETCH`／`CLOSE` での `$n` パラメータ（今回は `42601`）。
- NoSQL 表層のカーソル（`op` の許可リストに追加しない。`34000` は NoSQL
  表層の実要求からは到達不能として射影のみを固定する）。
- 上限（16 本・16 MiB）の既定値の確定 → オーナー判断。
