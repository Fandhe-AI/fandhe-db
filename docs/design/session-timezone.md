# ADR: セッション TimeZone（SET TIME ZONE／SHOW／StartupMessage／ParameterStatus）

- ステータス: Accepted（Issue #1575 の実装に伴う設計記録）
- 対応: Issue #1575（親 #1572。前提は #1574、後続は #1576・wire の OID 1184 は #1577）
- 関連ビヘイビア: TABLE-33（ポインタ: `docs/spec/04-behavior/`。本文は転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 doc: [timestamptz-tz-data-adoption.md](timestamptz-tz-data-adoption.md)（TZ データ源）、[datetime-column.md](datetime-column.md)（TIMESTAMPTZ の codec・リテラル）
- 依存の追加・feature の変更はない（承認済みの `jiff =0.2.38`・`jiff-tzdb =0.1.9` の範囲）

## 1. 目的

接続ごとに TimeZone を持ち、TZ を持たない `TIMESTAMPTZ` 入力の解釈と出力の整形に使う。他のセッション・他のテナントへ影響しないこと、トランザクションの取り消しで PG と同じく値が戻ることを満たす。

## 2. 設計判断

| ID | 判断 |
| -- | ---- |
| D1 | 値は `engine::sql::mode::SessionState` に `time_zone`（現在値）と `time_zone_reset`（`DEFAULT`／`LOCAL` の戻り先。StartupMessage の値、無ければ UTC）として置く。`sql/transaction.rs` の ROLLBACK／abort は `SessionState` ごと巻き戻すため、取り消しで値が戻る |
| D2 | `datetime::SessionTimeZone` は（jiff の `TimeZone` または秒数の固定 offset）と表示用文字列の組。固定 offset は PG の `\|h\| < 168` を表現するため jiff の `Offset`（±25:59:59 上限）を使わず自前の秒数で持つ |
| D3 | 設定値の解決順は PG の `check_timezone` に倣い、10 進数（時間 offset）→ IANA 名（bundled tzdb。表示は tzdb 上の正規の大文字小文字）→ 裸の `±H[H]:MM`（POSIX 解釈で符号反転）→ POSIX 文字列。長さ（255 バイト）→ ASCII → 許可文字の順に検査し、失敗は固定文言の `22023`（入力値を反射しない） |
| D4 | 文法は allowlist。`SET [SESSION] TIME ZONE {'文字列'｜識別子｜[-]数値｜LOCAL｜DEFAULT}` と `SET [SESSION] {timezone｜"TimeZone"} {TO｜=} {…｜DEFAULT}`、`SHOW timezone`／`SHOW TIME ZONE`。`SET LOCAL ...`・`SET TIME ZONE INTERVAL ...` は `0A000`。既存の `SET search_mode`（`=` と文字列リテラルのみ）は緩めない。`SHOW` の他の形は従来どおり分割実行 DML の制御として解析する |
| D5 | engine 内の運び方は `JsonPathLimitsScope` と同じスレッドローカルの RAII スコープ（`SessionTimeZoneScope`）。`core.rs` の各セッション実行入口（parse の束縛を伴う execute・txn・describe・検証済み文の実行）が文ごとに張る。スコープの無い経路（NoSQL）は UTC。COPY FROM は行の束縛が複数メッセージにまたがるため、開始時の値を `CopyInSession` が保持して `feed`／`finish` で張る |
| D6 | 列 `DEFAULT` の TZ 無しリテラルは PG と同じく **DDL 実行時点のセッション TZ** で解釈して固定する。`sql::ddl::normalize_timestamptz_default` が CREATE TABLE・ADD COLUMN の永続化前に UTC の明示 offset 付き表記（`...+00`）へ正規化して保存する。`row_codec` の読み出し時補完は決定性のため UTC 固定のまま（正規化後は明示 offset を持つので結果は同じ。#1574 の時代の既定値は当時の DDL セッション＝常に UTC と同じ解釈が保たれ、移行は不要） |
| D7 | wire の出力整形は接続の `session.time_zone()` を渡す `_in_tz` 版（簡易クエリの DataRow・拡張クエリの DataRow・COPY TO）を使う。既存の公開関数は UTC で委譲する薄いラッパーとして残す。HTTP（NoSQL）は仕様どおり UTC 固定 |
| D8 | ParameterStatus は ReadyForQuery を書く場所で「現在値 ≠ 通知済みの値」のときだけ、CommandComplete の後・ReadyForQuery の直前に送る（`handshake::ready_for_query_frames`）。通知済みの値は ROLLBACK で巻き戻してはならないため `SessionState` ではなく接続単位の `SessionTransaction` に記録する。エラー応答経路は通知しない（復元は後続の RFQ で通知される） |
| D9 | StartupMessage の `TimeZone`（キーは大文字小文字を区別しない）は認証成功後に解決する（認証前の入力に tzdb の参照結果を観測させない）。不正値は AuthenticationOk の後に `FATAL 22023`（固定文言）を返して切断し、BackendKeyData は送らない。成功時は現在値と reset 値の両方に設定し、起動時の ParameterStatus に `TimeZone` を加える |
| D10 | 公開 API の破壊的変更: `allowlist::Statement` に `SetTimeZone`／`ShowTimeZone`、`SqlOutcome` に `SetTimeZone`／`Show` を追加した（いずれも `#[non_exhaustive]` ではない） |

## 3. PG 17 の実測との対応（postgres:17 で確認した公開挙動）

| 入力 | SHOW の値 | 実効 offset |
| ---- | --------- | ----------- |
| `'Asia/Tokyo'`／`'asia/tokyo'` | `Asia/Tokyo` | +09 |
| `UTC`／`utc` | `UTC` | 0 |
| `'Japan'`／`'US/Eastern'` | tzdb の大文字小文字に揃えた入力名 | — |
| `9`／`'+9'` | `<+09>-09` | +09 |
| `-8` | `<-08>+08` | −08 |
| `5.5` | `<+05:30>-05:30` | +05:30 |
| `'+09:00'` | `+09:00` | −09（POSIX 解釈で符号が逆） |
| `'est5edt'` | `EST5EDT` | — |
| `'UTC+9'`／`'JST-9'` | 入力（大文字化） | −09／+09 |

拒否（`22023`）: `'JST'`・`'Etc/Unknown'`・`'Foo/Bar'`・`'Z'`・`''`・`'local'`・`'default'`・`'../etc'`・末尾空白・`168` 以上・`'nan'`・`'inf'`。`SET timezone = LOCAL` は文字列 `'local'` として 22023、`SET TIME ZONE LOCAL|DEFAULT` は reset 値へ戻す。`SHOW` は列名 `TimeZone`（text）の 1 行、CommandComplete タグは `SHOW`、SET は `SET`。

## 4. PG との意図的な差（fail-closed）

- strtod 由来の受理形（`'1e1'`・`' 9'`・`'0x10'`・`'9 '`）、jiff の POSIX parser が受け付けない形、±25:59:59 を超える POSIX offset、裸 offset の `:SS` 成分は `22023`。
- StartupMessage の `TimeZone` 重複は `08P01` で拒否する（PG は後勝ち）。
- エラー文言は入力値を含まない固定文言（PG は値を付ける）。
- 分割実行 DML（`... PARTITIONED`）の内容照合ハッシュは SET 値の**リテラル文字列**を入力にするため、TZ を変えて同じ operation_id・同じ SQL を再送しても重複（23505）で拒否され、書き込み直されない（最初の解釈が残る）。

## 5. 対象外

- サーバー既定の TimeZone を変える CLI フラグ（reset 値は注入可能な形にしてある）。
- `SET TIME ZONE INTERVAL ...`（INTERVAL 型の担当 Issue まで `0A000`）、`SET LOCAL`（`0A000`）、`RESET timezone`／`RESET ALL`（従来どおり `42601`）。
- StartupMessage の `options`（`-c TimeZone=...`）、`SHOW ALL`、DateStyle など他の GUC。
- `AT TIME ZONE`・`timezone()`・DST の gap／fold の最終確定（#1576）。
- `now()`／`CURRENT_TIMESTAMP` の戻り値型の切り替え、`CURRENT_DATE`／`LOCALTIME`／`LOCALTIMESTAMP` のセッション TZ 評価（型切り替えの担当 Issue）。
- wire OID 1184 とバイナリ入出力（#1577）。

## 6. セキュリティ

- TZ はプロセス全体の状態を持たず接続の `SessionState` のみ。スレッドローカルのスコープは RAII で、panic しても他の文脈へ漏れない。
- 値は SQL・プラン文字列へ連結せず、ファイルシステム上の tzdata・`TZ`・`TZDIR` を読まない（bundled tzdb のみ）。
- 失敗した `SET` はセッションを変えない（検証してから代入）。受信データ経路で `unwrap`／`expect`／添字アクセスを使わない。
