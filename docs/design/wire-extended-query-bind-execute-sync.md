# 拡張クエリプロトコル Bind／Execute／Sync（Issue #934・TASK-71・WIRE-11）

## ステータス

Implemented（本 Issue の範囲。`$n` パラメータ束縛・型 OID 推論は
WIRE-12・#935、暗黙トランザクションブロックは #942・RECOVER-12・SQL-31。
ReadyForQuery の状態バイト（#943・WIRE-19）は PR #1041 レビュー指摘対応で
Sync（`extended_query::handle_sync`）へ結線済み）。結果 format
code のバイナリエンコード（WIRE-14・#936・PR #998）は本 Issue でマージ後に
結線済み——「対象ファイル」「スコープ外・申し送り」節参照。

## 背景

先行の #933 で Parse（'P'）・Describe（'D' の statement 対象）を受理できるように
なった一方、Bind（'B'）・Execute（'E'）・Sync（'S'）・Close（'C'）・
Flush（'H'）と、Describe の portal 対象（種別 'P'）は `0A000`＋切断のまま
だった（`docs/design/wire-extended-query-parse-describe.md` 参照）。
psycopg 3 の既定 Cursor・node pg・JDBC・psql の `\bind` 等、拡張プロトコル
を既定で使うクライアントは最初のクエリで失敗していた。

本 Issue で Bind／Execute／Sync／Close／Flush を実装し、エラーが起きても
その Sync まで後続メッセージを破棄して `ReadyForQuery` を返し接続を維持す
る（同期回復）契約へ揃えた。

## 受理範囲

| メッセージ | 挙動 |
| --- | --- |
| Bind（'B'） | 対象 statement を `describe_parsed_in_session` で確定し portal を保持する。結果 format code を [`result_encoder::ResultFormats::resolve`]／[`validate_binary_formats`] で列ごとに解決・事前検査する（WIRE-14）。パラメータ数不一致・format code 個数不正は `08P01`、パラメータ側の binary 指定・結果側の非対応型指定は `0A000` |
| Describe（'D' 種別 P） | portal の `RowDescription`／`NoData`（`ParameterDescription` は返さない） |
| Execute（'E'） | portal を実行し、`max_rows` に応じて分割送出する（`PortalSuspended`／`CommandComplete`） |
| Sync（'S'） | エラー後の同期回復モードを解除し、名前付き・無名を問わず全 portal を破棄して `ReadyForQuery` を返す（PR #1013 レビュー指摘・codex P1。本サーバーには明示トランザクションが無く各 Sync サイクルが暗黙トランザクションに相当する） |
| Close（'C'） | statement／portal を解放（未存在名も成功）。statement の Close は派生 portal も閉じる |
| Flush（'H'） | 出力を flush するのみ（`ReadyForQuery` は送らない） |

## エラー後の同期回復（ignore-till-sync）

- **回復可能**: Parse／Bind／Describe／Execute／Close／Flush の処理中に、
  メッセージ境界が確定した**後**に判明したエラー（body の構造不正・SQL 検証
  エラー・実行エラー・保持上限超過・未定義 statement／portal 等）。
  `respond_error_and_await_sync` が ErrorResponse を送出し、
  `ExtendedQueryState::ignore_till_sync` を立てる。接続は維持する。
- **回復不能**: フレーム自体が壊れている（`framing::read_length_prefixed_body`
  自体が `FrameError` を返す）場合。`respond_error_and_close` が有界
  lingering close で切断する（WIRE-4・WIRE-10 の既存契約のまま）。
- `ignore_till_sync == true` の間、`handshake::post_auth_loop` は
  Sync（'S'）・Terminate（'X'）以外のメッセージについて、長さフィールドの
  みを検証して本文を読み捨て、一切応答しない（PostgreSQL と同じく 'Q'
  （簡易クエリ）も破棄する）。読み捨ては `framing::discard_body`（`io::copy`
  ＋`io::sink()`）で全量を確保しない。COPY・FunctionCall・未知の型バイトは
  破棄対象にせず、従来どおり `protocol_dispatch::reject_and_close`（fail-
  closed。フレームの意味を持たない可能性がある入力を安全側で拒否する）。
  `'X'`（Terminate）も通常分岐と同じく length=4・body 厳密に空であることを
  検証してから終了する（PR #1013 レビュー指摘・codex P1: 検証を素通りする
  経路があると、長さフィールド欠落・不正長・余剰 body を持つ malformed
  Terminate が「エラー後」という条件だけで正規の Terminate として受理されて
  しまう）。
- Sync 到達で `ignore_till_sync` を解除し、名前付き・無名を問わず全 portal
  を破棄してから `ReadyForQuery`（状態バイトは `SessionTransaction::status()`
  から `'I'`／`'T'`／`'E'` へ写像。WIRE-19・#943・PR #1041 レビュー指摘対応で
  結線済み）を返す。

## portal のライフサイクル

- `Portal` は `source_statement`（派生元 statement 名。Close(Statement) が
  連動して閉じるために使う）・`body`（`Empty` または Bind 時点でクローンした
  `ParsedSql`。後から statement が再 Parse されても portal の実行対象は
  変わらない——PostgreSQL の「bind snapshots the plan」と同じ）・`columns`
  （Bind 時点で `describe_parsed_in_session` から求めた結果列。portal の
  Describe と Execute の結果列整合ガードの双方に使う）・`state`
  （`Ready`／`Suspended`／`Done`／`Failed`）を持つ。
- `Failed` は「この portal への Execute 実行を試みたが失敗した」終端状態で、
  `engine::execute_parsed_in_session` を呼ぶ**前**に立て、成功した場合のみ
  `Done`／`Suspended` へ上書きする。実行呼び出し自体が失敗する経路・
  呼び出し成功後の後処理（結果列整合検査・行エンコード・中断バイト上限
  判定・応答フレーム送出）が失敗する経路のいずれも、以降の再 Execute で
  実行を再試行して副作用を重複させることを防ぐ（PR #1013 レビュー指摘・
  Cursor Bugbot Medium「Failed Execute leaves portal runnable」。以前は
  実行呼び出し成功後にのみ `Failed` へ倒していたため、呼び出し自体が
  失敗した場合は portal が `Ready` のまま残っていた）。`Failed` への
  再 Execute は `08P01`（`ProtocolViolation`）で拒否し、実行し直すには
  新しい Bind で portal を作り直す必要がある。
- 書き込み系文（`INSERT`/`UPDATE`/`DELETE`/`TRUNCATE` 等）の Execute で、
  `engine::execute_parsed_in_session` が `Ok` を返した（＝`RECOVER-5`
  「commit 成功境界」契約により commit が既に成功している）後に発生した
  失敗（結果列整合検査・`DataRow` エンコード・中断バイト上限判定・応答
  フレーム送出のいずれか）は、通常の失敗と区別するため `ErrorResponse` に
  ERR-5・`RECOVER-5` (3) の `state=may_be_committed` `D`（detail）フィールド
  を付ける（PR #1013 レビュー指摘・codex P1。既存の
  `error_response::encode_with_detail`／`MAY_BE_COMMITTED_DETAIL`——panic
  経由の緊急応答〔TASK-97・RECOVER-6〕専用だった——を通常の `Err` 経路にも
  拡張適用したもの。新規 SQLSTATE は追加しない。中断保持継続（2 回目以降の
  Execute）でも `PortalRows::committed` にこの区別を引き継ぐ）。
- 名前付き portal は `MAX_PORTALS_PER_SESSION`（64。実装既定値）まで、無名
  （`""`）は件数にカウントせず黙って置換する（`PreparedStatementStore` と
  同型の設計）。名前の重複は `08P01`、長すぎる名前・件数超過は `54000`。
- Execute は portal の状態に応じて振る舞う。
  - `Ready`: 実行し（下記「Execute と応答整形の共有」参照）、結果を
    `Suspended`（残り行あり）または `Done`（完了。副作用を持つ
    `CommandComplete` タグを保持）へ遷移させる。
  - `Suspended`: **再実行せず**保持済みフレームの続きから送出する。
  - `Done`: **副作用を再実行せず**保持済みタグで `CommandComplete` のみ返す
    （実装既定値。件数はそのタグに固定されたまま）。
  - `Empty`（空クエリの Bind）: 状態を持たず常に `EmptyQueryResponse` を返す
    （副作用がないため冪等）。
- `max_rows <= 0` は全行を返す（PostgreSQL と同じ）。`max_rows > 0` は先頭
  `max_rows` 行を送出し、残りがあれば `PortalSuspended`（'s'）を返す前に、
  保持し続ける残り行の合計バイト数を判定する（`MAX_SUSPENDED_PORTAL_BYTES_
  PER_SESSION`。1 行も送る前に判定し、超過は `54000`。この上限は portal
  単体ではなくセッション（接続）全体で合算して適用する——[`PortalStore::
  total_suspended_bytes_excluding`]。PR #1013 レビュー指摘・P0）。
  送出予定分は結果セット全体を先にメモリへエンコードして保持するのではなく
  1 行ずつエンコード→送出する（`max_rows<=0`／非常に大きい `max_rows` でも
  送出予定分のメモリ使用量が結果セット規模に応じて無制限に膨らまない。
  PR #1013 レビュー指摘・P0）。分割送出時の `CommandComplete` の件数は
  「portal 全体の累計送出行数」（PostgreSQL の `PortalRun` と同じ契約。
  PR #1013 レビュー指摘・P1: 直近 Execute の件数だけでは過小になる）。
- Sync は名前付き・無名を問わず全 portal を破棄する（PR #1013 レビュー
  指摘・codex P1。本サーバーには明示トランザクション〔`BEGIN`/`COMMIT`〕が
  無く各 Sync サイクルが暗黙トランザクションに相当するため、PostgreSQL の
  「トランザクション終了時に portal を閉じる」契約〔PostgreSQL 34.4
  「Bind」〕を Sync 境界へ適用する。名前付き prepared statement
  〔`PreparedStatementStore`〕は Sync を越えて残る——PostgreSQL と同じ
  挙動）。

## Execute と応答整形の共有（第 2 の実行器を作らない）

`crates/wire-server/src/simple_query.rs` から次の 2 つを `pub(crate)` で切り
出し、簡易クエリ（`run_statement`）と Execute（`extended_query::
execute_portal`）の双方から使う。**簡易クエリの応答バイト列は完全に不変**
（`respond_query_result_matches_*`・`wire1_simple_query`・
`wire16_multi_statement`・`wire_returning` 等の既存テストが green のまま
固定）。

1. **`execute_with_emergency_registration(stream, f)`**: 旧 `run_statement`
   の「outcome を決定する区間」（`_emergency_registration` の登録ブロックと
   `#[cfg(feature = "fault-injection")] maybe_panic_after_commit`）を、
   クロージャを受け取る形にしたもの。`run_statement` は
   `|| engine.execute_sql_in_session(..)`、Execute は
   `|| engine.execute_parsed_in_session(ctx, session, &parsed)` を渡す。
   登録位置と panic 注入点の位置関係（TASK-97・RECOVER-6）は変えない。
2. **`map_outcome(outcome) -> OutcomeResponse`**: `SqlOutcome` から応答の形
   （`OutcomeResponse::Rows { result, shape: TagShape }` /
   `OutcomeResponse::Command { tag }`）への写像を 1 か所に集約する。
   `TagShape::Dynamic`（`SELECT n`。実際に送出した行数から組み立てる）・
   `Fixed`（`EXPLAIN`。件数を持たない）・`FixedTag`（`INSERT 0 n`・
   `RETURNING` の `rows_affected`。分割送出の影響を受けない固定値）の 3 種
   で、簡易クエリ（常に全行を 1 回で送る。`sent = result.rows.len()`）と
   Execute（`max_rows` ごとに `sent` が変わりうる）のタグ組み立て規則の違い
   を吸収する。

Execute はさらに、実行結果の `result.columns` が Bind 時点で確定した
`portal.columns` と一致するかを検証する（`0A000`。PostgreSQL の「cached
plan must not change result type」に相当する fail-closed ガード。本実装で
は再 Parse が portal の実行対象を変えない設計のため構造的に到達しにくいが、
将来の拡張に対する防御として残す）。

自動コミットは Execute ごとに行う（現行の簡易クエリ 1 文と同じ）。同じ
Sync バッチ内で後続のメッセージが失敗しても、先に commit 済みの Execute は
巻き戻らない（暗黙トランザクション未実装〔#942〕に伴う既知の差分）。

## SQLSTATE 写像

| 事象 | wire_code |
| --- | --- |
| 未定義 statement／portal への参照・名前付き statement／portal の重複作成・`Failed` 状態の portal への再 Execute | `08P01`（`ProtocolViolation`） |
| Bind のパラメータ数不一致・format code 個数不正 | `08P01` |
| 結果 format code の個数不正・値不正（0/1 以外） | `08P01`（`ResultFormats::resolve`） |
| Bind のパラメータ format code が binary 指定で対象スロットが text 系以外（WIRE-12・#1171）・結果 format code が非対応型の列を binary 指定（WIRE-14。`id`／`VECTOR`／`Computed`。TABLE-13・Issue #886 の `BOOLEAN`／`BYTEA` も同様） | `0A000`（`FeatureNotSupported`） |
| Execute の結果列不整合（cached plan must not change result type 相当） | `0A000` |
| 件数・名前長・保持バイト上限超過 | `54000`（`PayloadTooLarge`） |
| body の構造不正（NUL 終端欠落・余剰バイト・負の件数・非 UTF-8・種別バイト不正） | `08P01` |
| SQL 検証失敗・実行時エラー | `SqlSurfaceError::error_class()`（簡易クエリと同一） |

**申し送り（spec リポ側の課題）**: PostgreSQL の `26000`（undefined cursor）・
`34000`（invalid cursor name）・`42P03`（duplicate cursor）相当の分類が
`ErrorClass`（閉じた 16 分類）に無いため、`08P01` へ寄せている。

## write-through の理由（RECOVER-5）

先行の #933 と同じくメッセージごとに即時書き出す write-through 方式を採る
（PostgreSQL プロトコル上バックエンドは任意時点で応答してよい）。Execute は
`ResponseBoundaryGuard`（RECOVER-5 (3)）・緊急応答登録（RECOVER-6）を、
簡易クエリと同じ「commit から応答送出完了までの区間をメッセージ内に閉じる」
位置関係で適用する（`ResponseBoundaryGuard` は `handle_execute` が Execute
（'E'）メッセージ 1 通の処理全体を覆う形で保持する。PR #1013 レビュー
指摘・cursor High: 当初の実装ではこのガードが欠落しており、commit 成功後に
panic すると緊急応答〔TASK-97・RECOVER-6〕が発火しない fail-open な穴が
あった）。Sync までまとめて応答をバッファリングする最適化は、この境界を
曖昧にしうるため見送り、将来 Issue の候補として記録するに留める。

## 対象ファイル

- `crates/wire-server/src/extended_query.rs`: `ExtendedQueryState`
  （`statements`／`portals`／`ignore_till_sync`）・`PortalStore`／`Portal`
  （`result_formats: Vec<result_encoder::FormatCode>` を含む——Bind が確定
  した結果 format code を Describe(Portal)／Execute の双方が参照する）を
  新設し、Bind／Execute／Close の body デコーダ、`handle_bind`／
  `handle_execute`／`handle_sync`／`handle_close`／`handle_flush`、
  Describe の portal 対象対応を追加。`respond_error_and_close`（フレーム
  違反用）と `respond_error_and_await_sync`（同期回復用）を分離。
- `crates/wire-server/src/handshake.rs`: `post_auth_loop` に 'B'／'E'／'S'／
  'C'／'H' のアームを追加し、ループ先頭で `ignore_till_sync` 中の破棄を
  振り分ける。引数を `PreparedStatementStore` から `ExtendedQueryState` へ
  変更。
- `crates/wire-server/src/simple_query.rs`:
  `execute_with_emergency_registration`・`map_outcome`／`OutcomeResponse`／
  `TagShape` を `pub(crate)` で切り出し（挙動・バイト列は不変）。
- `crates/wire-server/src/result_encoder.rs`: `encode_bind_complete`（'2'）・
  `encode_close_complete`（'3'）・`encode_portal_suspended`（'s'）を追加
  （いずれも固定 5 バイト）。結果 format code のエンコード基盤
  （`FormatCode`／`ResultFormats`／`validate_binary_formats`／
  `encode_row_description_with_formats`／
  `encode_data_row_into_with_formats`。WIRE-14・#936・PR #998）は本 Issue
  では変更せず、`handle_bind`／Describe(Portal)／Execute から呼ぶだけ。
- `crates/wire-server/src/framing.rs`: `discard_body`（有界な読み捨て）を
  追加。
- `crates/wire-server/src/limits.rs`: `MAX_PORTALS_PER_SESSION`（64）・
  `MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`（16 MiB）を追加（いずれも本リポ
  の実装既定値）。

## 検証

- `crates/wire-server/src/extended_query.rs`（`#[cfg(test)]`）: Bind／
  Execute／Close の body デコーダの境界値（負の件数・切り詰め・余剰バイト・
  値長 -1／-2 等）、`PreparedStatementStore`／`PortalStore` の件数・名前長・
  無名置換・派生 portal 一括削除、`validate_format_codes` の境界。
- `crates/wire-server/tests/wire11_bind_execute_sync.rs`（新規）: SELECT の
  正常系（簡易クエリ応答とバイト単位一致）・`max_rows` による分割送出（再
  実行なし）・`INSERT ... USING OPERATION_ID` の可視性と衝突検知
  （`23505`／`22023`）・Sync による多様なエラーからの回復（許可リスト外
  SQL・未存在テーブル・未定義 statement／portal・実行時エラー）・エラー後
  Sync 前の 'Q' 破棄・portal のライフサイクル（無名／名前付き／Close／
  派生 portal 一括 Close／未存在名の Close）・Flush の無応答性と非空
  body の Sync／Flush の切断・portal 件数／名前長上限・結果 format code
  の解決／事前検査（`TEXT` 列への binary 指定は成功し `RowDescription`／
  `DataRow` に反映される・非対応型〔`id` 等〕への binary 指定は `0A000`・
  個数／値不正は `08P01`）・テナント境界（RLS-7）・`engine: None` 経路の
  後方互換・簡易クエリ応答の不変性を固定。PR #1013 レビュー指摘対応で
  `malformed_terminate_during_ignore_till_sync_is_rejected_not_silently_closed`
  （`ignore_till_sync` 中の malformed Terminate が `08P01` で拒否されること）・
  `failed_execute_marks_named_portal_failed_and_rejects_reexecute`
  （実行呼び出し自体が失敗した named portal への再 Execute が `PortalFailed`
  で拒否されること）・
  `post_commit_suspended_bytes_overflow_carries_may_be_committed_detail`
  （commit 成功後の中断バイト上限超過が `state=may_be_committed` detail 付き
  で返り、行は実際にコミット済みであること）を追加。
- `crates/wire-server/tests/wire11_parse_describe.rs`: Parse／Describe の
  エラー系テストを `_and_closes` から `_and_recovers`（Sync で同期回復し
  簡易クエリが通ることを確認）へ更新。portal 対象の Describe は本 Issue で
  受理されるようになったため、未定義 portal への Describe が `08P01`＋
  同期回復になることを固定するテストへ差し替え。
- 既存 `wire1_simple_query.rs`・`wire16_multi_statement.rs`・
  `wire_returning.rs`・`wire_emergency_response.rs`・
  `wire_extended_query.rs`（`engine: None` の WIRE-8 契約）はすべて無変更の
  まま green。

## スコープ外・申し送り

- （実装済み: Issue #1171）`$n` の束縛と `ParameterDescription` の型 OID 推論
  （WIRE-12）。Bind の値数は文の要求数と一致しなければ `08P01`。パラメータ側の
  binary は text 系スロットのみ受理し（UTF-8 バイト恒等）、それ以外は `0A000`
  （詳細は `docs/design/wire-extended-query-param-binding.md`）。
- 暗黙トランザクションブロック（Sync までの複数 Execute を 1 トランザクショ
  ンにまとめる挙動。#942・RECOVER-12・SQL-31）。
- CancelRequest。
- パラメータ付きクエリを含む実クライアント（psycopg 3／node pg／psql）での
  拡張プロトコル層 B 検証は #935 以降。パラメータなしのシナリオは本 Issue
  の任意範囲だが、層 A（`wire11_bind_execute_sync.rs`）で十分に受理範囲を
  固定できたため見送った。
