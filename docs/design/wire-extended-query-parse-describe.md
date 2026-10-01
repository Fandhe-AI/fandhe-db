# 拡張クエリプロトコル Parse／Describe（Issue #933・TASK-71・WIRE-11）

## ステータス

Implemented（#933 の範囲）。Bind／Execute／Sync／Close／Flush と Sync による
同期回復は #934 で実装済み（`docs/design/wire-extended-query-bind-execute-sync.md`
参照）。`$n` パラメータ束縛は #935 の担当のまま。

## 背景

`crates/wire-server/src/protocol_dispatch.rs`（WIRE-8・TASK-71）は認証後の
Parse（'P'）・Bind（'B'）・Describe（'D'）・Execute（'E'）・Sync（'S'）・
Close（'C'）・Flush（'H'）を一律 `0A000`（`FeatureNotSupported`）＋有界
lingering close で拒否していた。libpq／psycopg 3／node pg・JDBC 等の多くの
クライアントは既定で拡張クエリプロトコルを使うため、この一律拒否は実クライ
アントの初手接続を落としていた。

vector-db-spec 側の改訂（2026-09-22）で WIRE-11（拡張クエリプロトコル実装
契約）が新設され、#933／#934 を直接の実装 Issue として参照した。

## 受理範囲（本 Issue）

本 Issue（#934）以降は portal も構築されるようになり、Describe（'D' 種別
P）・Bind／Execute／Sync／Close／Flush はすべて受理される。詳細は
`docs/design/wire-extended-query-bind-execute-sync.md` 参照。

| メッセージ | 挙動（#933 時点） |
| --- | --- |
| Parse（'P'） | SQL を簡易クエリと同一の許可リストで検証し、接続単位で保持する |
| Describe（'D' 種別 S） | `ParameterDescription`（`$n` の公告 OID。#1171 以降。`$n` なしは 0 件）＋ `RowDescription` または `NoData` |
| Describe（'D' 種別 P） | portal は構築され得ないため `0A000` + 切断（WIRE-8 のまま。#934 で受理範囲に入った） |
| Bind／Execute／Sync／Close／Flush | WIRE-8 のまま `0A000` + 切断（#934 で受理範囲に入った） |

## エンジン側の分割（`crates/engine/src/core.rs`）

`EngineCore::execute_sql_in_session` が担っていた「許可リスト検証（先頭
トークン覗き見による `INSERT`／`TRUNCATE`／`DELETE`／`UPDATE` の専用検証経路
分岐を含む）→ 実行」を、次の 3 メソッドへ分割した。

- `pub fn parse_sql(&self, sql: &str) -> Result<ParsedSql, SqlSurfaceError>`:
  検証のみ。行・台帳・世代のいずれにも触れない。
- `pub fn execute_parsed_in_session(&self, ctx, session, parsed: &ParsedSql)`:
  `parse_sql` の結果を実行する（各分岐は分割前の実行本体をそのまま移設）。
- `pub fn execute_sql_in_session(&self, ctx, session, sql: &str)`: 上記 2 つの
  合成（`parse_sql` → `execute_parsed_in_session`）。判定順序・エラー分類は
  分割の前後で完全に不変（`crates/engine/tests/describe_parity.rs` の副作用
  ゼロテストで固定）。

`ParsedSql` は `#[non_exhaustive] pub enum`（`Statement`／`Insert`／
`Truncate`／`Delete`／`Update`）で、`sql::allowlist` の各検証済み型をそのまま
保持する。#934 の Execute はこの `execute_parsed_in_session` をそのまま使う
想定（第 2 の実行器を作らない）。

`pub fn describe_parsed_in_session(&self, session, parsed: &ParsedSql) ->
Result<Option<Vec<ColumnMeta>>, SqlSurfaceError>` は、`parsed` の種類ごとに
結果列を実行を経ずに導出する:

- `SELECT`（`USING PLAN` 含む）: 投影列を束縛するだけ（`USING PLAN` は
  `plan_query`・`Embedder::embed_batch` 等の LLM／埋め込み I/O を一切呼ばない
  ——Describe を LLM コスト増幅の DoS 経路にしない）。
- 集計（`GROUP BY` 有無問わず）: `BoundAggregate::projection` から導出。
- 広域取得（scan）: `BoundScan::projection` から導出。
- `EXPLAIN`: 常に固定の単一列（`ColumnMeta::Computed{name: "QUERY PLAN"}`）。
- `INSERT`／`DELETE`（単一行）: `RETURNING` があれば投影列、無ければ `None`。
- `TRUNCATE`／`SET search_mode`／`CREATE FUNCTION`／述語形 `DELETE`／
  `UPDATE`: 常に `None`（`UPDATE` は `RETURNING` の実行結線が未着手のため
  構造検証段が常に拒否する。`ValidatedUpdate::returning` ドキュメント参照）。

投影列の導出は `crates/engine/src/sql/describe.rs`（`projected_columns`／
`aggregate_columns`）に集約し、`sql::exec`／`sql::scan`／`sql::aggregate`／
`sql::group_by` の実行時列構築と同一の写像を使う（`describe_parity.rs` が
実行結果との完全一致を機械検証）。

## wire-server 側の実装

- `crates/wire-server/src/extended_query.rs`（新規）: body 復号
  （`parse_parse_body`／`parse_describe_body`。`unwrap`/`expect`/添字禁止）・
  接続単位の `PreparedStatementStore`（`HashMap<String, PreparedStatement>`。
  無名 `""` は黙って置換し件数を増やさない）・`handle_parse`／
  `handle_describe`。
- `crates/wire-server/src/result_encoder.rs`: `encode_parse_complete`
  （'1'）・`encode_parameter_description`（'t'）・`encode_no_data`（'n'）を
  追加。`RowDescription` は既存 `encode_row_description` をそのまま再利用
  （型 OID 拡張〔#895・WIRE-13〕は Describe にも自動反映される）。
- `crates/wire-server/src/protocol_dispatch.rs`: `FrontendMessageKind` へ
  `Parse`／`Describe` を追加し、`ExtendedQuery` は Bind/Execute/Sync/Close/
  Flush（'B'/'E'/'S'/'C'/'H'）のみに縮小。
- `crates/wire-server/src/handshake.rs`: `post_auth_loop` に `b'P'`／`b'D'`
  腕を追加。`engine: None`（`handle_connection_bounded` 経由の後方互換
  パス）の場合は従来どおり長さ検証後に `protocol_dispatch::reject_and_close`
  （`0A000` + 切断）へ倒す。接続単位の `PreparedStatementStore` は
  `SessionState` と並べて接続ループが所有し、接続終了で破棄する（接続間・
  テナント間で共有しない）。
- `crates/wire-server/src/limits.rs`: `MAX_PREPARED_STATEMENTS_PER_SESSION`
  （64）・`MAX_STATEMENT_NAME_LEN`（63）・
  `MAX_PREPARED_SQL_BYTES_PER_SESSION`（4 MiB）。いずれも本リポの実装既定値
  （spec は数値までは定めない）。

## SQLSTATE 写像

engine の `ErrorClass`（閉じた 16 分類）にも spec にも、未定義ステートメント
名・名前付きステートメント重複に相当する専用分類が無いため、コードを発明
せず既存分類へ倒す:

| 事象 | wire_code |
| --- | --- |
| 未定義ステートメント名への Describe | `08P01`（`ProtocolViolation`） |
| 名前付きステートメントの重複 Parse | `08P01` |
| Describe の対象が portal | `0A000`（`FeatureNotSupported`） |
| 件数・名前長・保持バイト上限超過 | `54000`（`PayloadTooLarge`） |
| body の構造不正（NUL 終端欠落・余剰バイト・負の件数・非 UTF-8・種別バイト不正） | `08P01` |
| パラメータ型宣言（`num_param_types > 0`） | #1171 以降は受理（上限 64 超過は `54000`、プレースホルダ数超過は `08P01`。WIRE-12） |
| SQL 検証失敗 | `SqlSurfaceError::error_class()`（簡易クエリと同一） |

PostgreSQL の `26000`（undefined prepared statement）・`42P05`（duplicate
prepared statement）に相当する分類は `ErrorClass` に無く、本実装は上表の写像を
使う（WIRE-11・ERR-2・ERR-6 ポインタ）。

## 暫定契約（#934 で置換済み）

本節は #933 時点の暫定契約の記録として残す。Sync による同期回復を持たな
かった #933 の時点では、Parse／Describe の失敗は ErrorResponse 送出後に
`protocol_dispatch::drain_and_close` による有界 lingering close で接続を
終了していた（WIRE-8 が採用していた設計をそのまま踏襲）。#934 で Sync 対応
と同時に、body 復号後に判明するエラーは接続を維持したまま Sync まで同期
回復する契約へ置き換え済み（フレーム自体が壊れている場合は引き続き切断。
詳細は `docs/design/wire-extended-query-bind-execute-sync.md`
「エラー後の同期回復」節参照）。

## 検証

- `crates/engine/tests/describe_parity.rs`: `describe_parsed_in_session` の
  列が `execute_sql_in_session` の実行結果列と完全一致すること（SELECT・
  WHERE 付き SELECT・scan・集計〔GROUP BY 有無〕・EXPLAIN・INSERT/DELETE
  RETURNING）、副作用ゼロ（行・台帳・世代が Describe 前後で不変）を固定。
- `crates/wire-server/src/extended_query.rs`（`#[cfg(test)]`）: body 復号・
  `PreparedStatementStore` の単体テスト。
- `crates/wire-server/tests/wire_extended_query.rs`: WIRE-8（Bind/Execute/
  Sync/Close/Flush の一律拒否）は不変のまま green。`engine: None` での
  Parse／Describe も従来どおり拒否されることを固定。
- `crates/wire-server/tests/wire11_parse_describe.rs`（新規）: engine 接続
  済み経路での Parse／Describe の受理・エラー系（許可リスト外 SQL・未存在
  テーブル・重複名・未定義名・portal・上限超過・body
  構造不正）・テナント間の応答バイト一致・簡易クエリ挙動の不変性を固定。

## スコープ外・申し送り

- #934（実装済み）: Sync での同期回復・接続維持への置換、Bind／Execute
  （`execute_parsed_in_session` の再利用）、portal の Describe、Close／
  Flush、応答のバッファリング方針。
- #935・#1171（実装済み）: `$n` と `ParameterDescription` の型 OID 推論（WIRE-12）、
  Parse のパラメータ型宣言受理（`docs/design/wire-extended-query-param-binding.md`）。
- 実クライアント（psql／psycopg 3／node pg）でのパラメータ付き拡張プロト
  コル実行検証は #935 以降（パラメータなしの層 B シナリオは #934 の任意
  範囲として `docs/design/wire-extended-query-bind-execute-sync.md` 参照）。
