# NoSQL 表層 DDL（`create_table`／`alter_table`／`drop_table`）の設計判断

Issue #910・対象ビヘイビア: NOSQL-13（TASK-207）。関連ポインタ: SQL-23（DDL 本体）・
TABLE-6, TABLE-13, TABLE-14（型集合）・ERR-4（エラー射影）・NOSQL-1・NOSQL-9
（語彙の改訂注記）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 背景

NoSQL 表層（`POST /v1/query`）の `op` 語彙は当初 6 値（search／scan／aggregate／
insert／update／delete）に閉じており、DDL 相当（`create_table`／`alter_table`／
`drop_table`）は語彙外として `0A000` に落ちていた。本 Issue はこの 3 op を語彙へ
加え、SQL 表層の DDL（SQL-23）と機能パリティを取る。

## 設計方針: 第 2 の DDL 実行器・第 2 の権限判定を作らない

JSON の各フィールドを `engine::sql::lexer::Token` へ直接写像し、
`engine::sql::allowlist::{validate_create_table_tokens, validate_alter_table_tokens,
validate_drop_table_tokens}`（`pub(crate)` → `pub` へ Issue #910 で公開）へ
そのまま渡す。これは `EngineCore::parse_sql_prepared`／`bind_prepared`
（拡張クエリプロトコルの `$n` を実値の `Token::StringLiteral` へ置換したトークン列
を「SQL テキストへ戻さず」パーサーへ渡す設計）と同じ前例に倣う。

利点:

- SQL 表層が適用する構造検証（予約列名・列名重複・列数／制約数上限・
  `PRIMARY KEY`／`UNIQUE`／`FOREIGN KEY` の finalize 処理・`VECTOR` 列への
  `DEFAULT`／`UNIQUE` 禁止・`DEFAULT` リテラルの型整合）をすべて再利用し、
  複製しない。
- DDL 実行権限ゲート（`engine::sql::ddl::require_ddl_permission`）・カタログ照会を
  含む実行本体は `EngineCore::execute_parsed_in_session`（`ParsedSql::CreateTable`／
  `AlterTable`／`DropTable` 分岐）単独が担う。wire-server 側（`http/query/ddl.rs`）
  には権限判定を一切置かない。

却下した代替案: `ValidatedCreateTable` を wire-server から直接組み立てる方式は
採らない。`catalog::validate_schema` は予約列名を検査しないため、直接構築すると
`tenant_id` のような RLS 内部列を隠す列を作れてしまう（P0）。

## 字句アトムの生成規則（パリティと注入防止）

- **識別子**（table・列名・制約の参照列・参照先テーブル・ENUM 型名）:
  `http/query/ident::check_identifier`（長さ・文字種の事前フィルタ）を通したうえで、
  `engine::sql::lexer::tokenize(raw)` の結果が**ちょうど `[Token::Ident(s)]` かつ
  `s == raw`** であることを要求する（`ddl.rs::ident_token`）。`Token::Ident` を
  直接組み立てない——lexer がキーワード化する語（`select`／`from` 等）を NoSQL
  だけで作れてしまい、SQL から参照できない列が生じるため。
- **数値**（`dim`／`precision`／`scale`）: `Validated::required_u32` で取得した
  非負整数を 10 進テキスト化して `Token::Number` にする。
- **`DEFAULT` の数値**: `http/query/typed_json::number_literal_text` を再利用して
  正準テキストにし、負数は `Token::Punct('-')` を独立して積む（SQL の字句解析が
  符号を数値トークンへ含めないため。`sql::allowlist::Parser::expect_literal` と
  同じ設計）。指数表記（`e`／`E`）は `lex_number` が受理しない形状のため
  `42601` で拒否する。
- **`DEFAULT` の文字列**: `Token::StringLiteral(s)` を直接使う（prepared の前例と
  同じ）。制御文字（NUL 等）を含む値は往復不能なため `42601`。
- **`DEFAULT` の null／配列／オブジェクト**: SQL の `CREATE TABLE` で表現
  できないため `42601`（真偽値は `add_column` と同じ `true`／`false` 識別子へ写像。
  配列の既定値は文字列 `"{1,2}"` 形）。
- **型名**: wire 側に閉じた固定語彙の対応表を置く（`ddl.rs::column_type_tokens`／
  `base_type_tokens`。`create_table`・`add_column` で共用。Issue #1409）。全スカラー型・
  `numeric`・`vector`・`enum`・配列を SQL と同じ型集合で受け付け、表にない値・型と
  無関係なパラメータの混入は engine を呼ぶ前に `42601`。配列の表現は
  「配列の JSON 表現」節を参照。
- **参照アクション**（`references.on_delete`／`on_update`。Issue #1148・
  NOSQL-13）: 固定語彙（`no_action`／`restrict`／`cascade`／`set_null`／
  `set_default`。小文字 snake_case・完全一致）から `ON DELETE`／`ON UPDATE`
  の固定トークン列への `match` だけで写像する（`ddl.rs::
  referential_action_tokens`）。JSON 文字列値を `Token::Ident` へ直接転用
  しない点は識別子と同じ注入防止方針。省略時はトークンを生成せず engine
  既定の `NO ACTION` に委ねる（現行挙動と完全互換）。語彙外・非文字列値は
  `42601`。

## 配列の JSON 表現（Issue #1409・NOSQL-13。spec に規定がないため本リポで決定）

採用: 構造化形式 `{"type":"array","element_type":"text","max_len":8}`。
`max_len` 省略は SQL の `[]`。要素型のパラメータは既存の `precision`／`scale`／
`enum_type`／`dim` をそのまま使い、固定語彙の `match` だけでトークン列へ写像する。
要素数上限（0・1024 超）・VECTOR 要素の拒否は SQL と同じく engine（`ArrayType::new`）に
一本化し、wire 側に上限定数を複製しない。入れ子配列は wire で `42601`。

却下: `"type":"integer[]"` の接尾辞形式。untrusted な文字列を部分文字列として
解析する必要があり、応答側の型名（`double precision[]`・`enum[]` 等、精度や型名を
含まない）とも往復しないため、見た目の一致という利点が実質的に無い。

SQL と NoSQL の同一宣言が同一のカタログ表現になることは、`ddl.rs` の unit tests
（`validate_*_tokens` の結果比較）と `tests/nosql13_ddl.rs`（SELECT の列メタ
`ColumnType` の比較）で層 A として固定する。

## セッションへの DDL 実行権限の搬送

- `http/session/store.rs::Entry` に `ddl_allowed: bool` を追加。値は `issue.rs` で
  `auth::verify` 成功**直後**に 1 回だけ `UserStore::is_ddl_allowed(user)` から
  確定させる（pg wire 側 `handshake.rs` の `session.allow_ddl()` と同じ
  「認証成功後」の順序）。
- 既存 API 互換のため `SessionStore::issue` は `issue_with_ddl(ctx, false, now)` の
  薄いラッパーとして残す。`lookup` も同様に維持し、新設 `lookup_grant` が
  `SessionGrant { ctx, ddl_allowed }` を返す。
- `SessionPrincipal::ddl_allowed()` を `http/query/ddl.rs` のみが読み、
  `SessionState::allow_ddl()` を呼ぶかどうかを決める。他の op ハンドラは従来どおり
  `SessionState::default()`。

## 未実装形（fail-closed。成功を偽装しない）

- `create_index`／`drop_index`／`create_view`／`drop_view`: NOSQL-13 の対象外の
  まま語彙外（`0A000`）に据え置く。

## CHECK 制約（Issue #1199・NOSQL-13・TABLE-16・SQL-23）

`create_table.constraints[]` に `{"kind":"check","name":"<任意>","predicate":[...]}`
を指定できる。`predicate` は `FILTER_ITEM_SCHEMA` の葉形（`column`／`op`／`value`）の
配列で、要素同士は AND 結合。表制約 `[CONSTRAINT <name>] CHECK (<葉> AND ...)` の
トークン列へ写像し、SQL 表層と同じ `validate_create_table_tokens` → `run_ddl` へ
合流する（第 2 の DDL 実行器・評価器・権限判定は作らない）。

| JSON | トークン |
| ---- | -------- |
| `name` 指定 | `CONSTRAINT <ident>`（省略時の名前は engine が確定） |
| `eq`／`lt`／`gt` | `=`／`<`／`>` |
| `le`・`lte`／`ge`・`gte` | `<=`／`>=` |
| `prefix`（文字列のみ） | `LIKE '<メタ文字エスケープ済み>%'`（update/delete の prefix と同じ `like_escape`） |
| 値: 文字列・数値 | `default_literal_tokens`（負数は `-` と数値に分かれ、engine が SQL 表層と同じ `42601` で拒否） |
| 値: 真偽値（`eq` のみ） | `true`／`false` |

- CREATE TABLE 時点ではスキーマが無いため、値トークンの種類は JSON 値の型で決め、
  列型との整合は engine の束縛が判定する（wire に型検査を持たない）。
- 表制約としてのみ生成するため既定名は `<table>_check` 系（SQL の列制約形の
  `<table>_<col>_check` とは既定名だけが異なる。意図した差分）。
- fail-closed の拒否（すべて `42601`）: check への `columns`／`references`、
  pk/unique/foreign_key への `predicate`、unique/foreign_key への `name`、`predicate` の欠落・空配列、
  `in` と `or` グループ、語彙外の `op`、RLS 述語名の列、`prefix` と非文字列、
  真偽値と `eq` 以外。葉が 256 件を超える場合は確保前に `54000`。
- `primary_key` の `name`（Issue #1437・NOSQL-13・TABLE-22）は
  `CONSTRAINT <ident> PRIMARY KEY (...)` へ写し、SQL 表層と同じカタログ表現にする。
  名前は `ident_token` と engine の制約名検証の二段で検証し、重複宣言（`42P16`）・
  名前衝突・名前付き `(id)` 単独（`42601`）は engine に一本化する（wire は先回り判定しない）。
- 違反した書き込み（insert／update）は engine の単一検査点が `23514`（HTTP 409）で
  拒否する。応答には制約名のみを含む。
- `DdlError::FeatureNotSupported` と `CHECK_CONSTRAINT_UNAVAILABLE_MESSAGE` は
  公開 API 互換のため残置（後者は deprecated。現状、構築箇所なし）。

## エラー射影（ERR-4 との整合）

`require_ddl_permission`（`42501`）はカタログ照会より必ず先に判定する
（`ParsedSql::CreateTable`／`AlterTable`／`DropTable` の各ドキュメント参照）ため、
権限の無いセッションは対象テーブルの有無にかかわらず常に同一の応答（`403`）を
返す（存在オラクル非公開。`tests/nosql13_ddl.rs::
permission_denial_is_byte_identical_regardless_of_table_existence` で固定）。

その他の分類は SQL 表層と共有: `42P07`（重複テーブル）・`42P01`（未定義テーブル）・
`42701`（列名重複）・`42601`（構文・意味検証）・`42830`（参照アクションの
宣言時検査失敗。Issue #1148）・`54000`（参照アクション連鎖の深さ・行数上限
超過。副作用ゼロ。Issue #1148）・`23503`（連鎖適用後も含む参照整合性違反）・`23514`（CHECK 違反。Issue #1199）。

## 検証

- `crates/engine/tests/sql_ddl_tokens_public_api.rs`: トークン入口が SQL テキスト
  経由の `parse_sql` と同一の `ParsedSql` になることを固定。
- `crates/wire-server/tests/nosql13_ddl.rs`: HTTP フレーミング越しの成功系・
  権限拒否・エラー分類・SQL/NoSQL パリティを固定（うち 7 件は
  Issue #1148 の参照アクション宣言・連鎖適用・宣言時検査・上限超過・
  テナント境界、7 件は Issue #1199 の CHECK 違反 `23514`・既定名・拒否・
  件数上限・権限）。
- `crates/wire-server/src/http/query/ddl.rs`・`op.rs`・`schema.rs`・`gate.rs`・
  `http/session/{store,issue,middleware}.rs` の単体テスト。

## スコープ外（後続 Issue の担当）

- `ALTER COLUMN TYPE` 相当の op 語彙（`alter_table.drop_column` は Issue #1167 で
  SQL 表層と同じ入口へ結線済み）。
- `alter_table` での CHECK 追加・削除、算術式を含む CHECK 述語、列制約形の
  既定名、pk/unique/fk の制約名指定の NoSQL 表現。
- `create_index`／`drop_index`／`create_view`／`drop_view` の NoSQL 対応。
