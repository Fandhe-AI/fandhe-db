# COPY プロトコル（`COPY ... FROM STDIN`／`COPY (...) TO STDOUT`）

- Issue: #939
- ポインタ: TASK-220・WIRE-17・INDEX-4（`docs/spec/04-behavior/wire-protocol.md`・
  `docs/spec/05-tasks.md`。数値・受理形の詳細は spec 側 SSOT。本 doc は
  spec-confidentiality.md に従いポインタ表記・本リポ独自の実装既定値のみを記す）
- ステータス: 実装済み（簡易クエリプロトコル経由に限定。拡張クエリプロトコル
  経由の COPY は対象外）

## 背景・目的

一括取り込み・一括取り出しの標準的な手段として、psql の `\copy` が使う
`COPY` サブプロトコルを簡易クエリプロトコル（'Q'）経由で受理する。書き込み
本体は既存の複数行 `INSERT`（SQL-16・TASK-190）と同一の実行器
（`EngineCore::execute_bound_insert_in_session`）を共有し、第 2 の書き込み
経路を作らない。

## スコープ

- 対象: `COPY <table> (<cols>) FROM STDIN [WITH] [(FORMAT text|csv)]
  USING OPERATION_ID '<id>'`、`COPY (<SELECT>) TO STDOUT [[WITH]
  (FORMAT text|csv)]`。いずれも簡易クエリプロトコル（'Q'）経由のみ。
- 対象外（起票済み・別 Issue／別タスクへ申し送り）:
  - 拡張クエリプロトコル（TASK-216）経由の COPY・Sync までの同期回復。
  - `HEADER`／`DELIMITER`／`NULL`／`QUOTE` などの COPY オプション、binary
    形式（WIRE-14 関連）、テーブル形 `COPY t TO STDOUT`、8 進・16 進エスケープ。
  - `CopyFail` に割り当てる `wire_code`（後述）の ERR-6 表への追加要否。

## 設計（決定事項）

### 責務分割

- **engine**（`crates/engine/src/sql/copy.rs`・`core.rs::begin_copy`／
  `commit_copy_in`）: COPY 構文の許可リスト（`sql/allowlist.rs::validate_copy`）、
  レコード分割・フィールドデコード・束縛、INDEX-4 の逐次判定、commit。
- **wire-server**（`crates/wire-server/src/copy.rs`）: CopyIn／CopyOut
  サブプロトコルのメッセージ層（フレーミング・状態遷移・行のエンコード）の
  みを担う。第 2 の実行器・第 2 の書き込み経路は作らない。

### 許可リスト（`sql::allowlist::validate_copy`）

- `FROM STDIN` 形は列リスト必須（`id` を含むこと。テーブル形の全列省略は
  `42601`）。`operation_id` 必須化ガード（`LedgerMode::require`）はカタログ
  照会より前（`validate_insert` と同じ順序）。
- `TO STDOUT` 形の内側 `SELECT` は、`validate_sql_tokens`（本 Issue で
  `validate_sql` から切り出した公開トークン列版。純粋なリファクタリング）が
  返す `Statement::Scan`（広域取得。SQL-15・Issue #454）のみを受理する
  （第 2 の SELECT パーサーを持たない）。順位付き `SELECT`（`ORDER BY`／
  `USING PLAN`）・集計・テーブル形は `42601`。
- `FORMAT` 以外のオプション（`HEADER`・`DELIMITER`・`NULL` 等）はいずれも
  `42601`。

### FROM STDIN の逐次取り込み（`sql::copy::CopyInSession`）

- レコード分割（`RecordSplitter`）: text 形式は生の LF を行終端とし、
  CRLF（`\r\n`）も行終端として受理する。ただし LF の続かない裸の `\r`
  （text 形式のエスケープされていないデータ中の CR）は `22000` で
  fail-closed に拒否する（PostgreSQL 本家が text 形式で出す `literal
  carriage return found in data` に相当。`pending_cr` フラグで直前バイトが
  裸の `\r` だった状態をチャンク境界をまたいで保持し、次バイトが `\n` なら
  CRLF として確定、それ以外は拒否する）。CSV 形式はレコード分割と
  フィールドデコードを `CsvRecordScanner` の 1 バイト単位の状態機械
  （`Start`／`Unquoted`／`Quoted`／`AfterQuote`／`AfterQuoteCr`／
  `UnquotedCr` の 6 状態）で同時に行う。`"` はフィールド先頭（`Start`）
  でのみ引用符開始として受理し、`Unquoted` 中の生 `"`・閉じ引用符直後
  （`AfterQuote`）の区切り／エスケープ／`\r`（CRLF 終端の一部）以外の
  バイトはいずれも不正な引用符配置として `22000` で fail-closed に拒否
  する。引用符なしフィールド中の裸の `\r`（`Start`／`Unquoted` から
  `UnquotedCr` へ遷移）・閉じ引用符直後の裸の `\r`（`AfterQuote` から
  `AfterQuoteCr` へ遷移）はいずれも、次バイトが `\n` のときのみ CRLF
  レコード終端として確定し、それ以外（LF が続かない場合。ストリームが
  そこで終わる場合を含む）は PostgreSQL 本家が CSV 形式で出す `unquoted
  carriage return found in data` に相当するエラーとして `22000` で
  fail-closed に拒否する。除去も保持もしない——「LF の無い CR は行終端の
  一部と確定できない」ため、無条件の除去は入力の無言改変になる
  （codex-review 指摘 PRRT_kwDOUAKASM6l0EjU。除去する旧実装は
  Cursor Bugbot PRRT_kwDOUAKASM6lzwN7 が指摘した `value\r\n` と `value\r`
  の格納値不一致の原因でもあった）。引用符外の生 `\r\n`／`\n` は通常の
  行終端としてそのまま受理する。引用符付きフィールド内の生改行は区切り
  ではなくデータとして 1 レコードへ含める（この経路は変更していない）。
  CopyData のチャンク境界をまたぐ場合も、text 形式は `pending`（未確定の
  行バイト列）と `pending_cr`（直前バイトが裸の `\r` だったか）、CSV 形式
  はスキャナ自身の状態（現在の `CsvFieldState`・確定済み `fields`・構築中の
  `field` バイト列）をそのまま保持して次チャンクの継続として処理する。
- フィールドデコード: text は PostgreSQL 互換のバックスラッシュエスケープ
  （`\\`／`\t`／`\n`／`\r`／`\b`／`\f`／`\v`。フィールド全体一致の `\N` のみ
  NULL）、CSV は RFC4180 風（`""` エスケープ、引用符なし空欄が NULL・引用符
  ありの空欄が空文字列）。いずれも未対応のエスケープ・不正な UTF-8 は
  `22000` で fail-closed に拒否する。
- 束縛（`bind_copy_record`）: `sql::parser::bind_insert_row`（`INSERT ...
  VALUES` 専用）とは意図的に別実装。`InsertLiteral` は文字列／数値リテラル
  のみを表現でき明示的な NULL 値を持てない（`INSERT` の許可形状自体が
  `VALUES` 内の `NULL` キーワードを受理しない）のに対し、COPY のフィールドは
  `\N`／引用符なし空欄という独自の NULL 表現を持つため、共有すると
  `InsertLiteral` へ破壊的変更（`Null` variant 追加）を要する。列名解決・
  非 nullable 列欠落・型照合の検査内容は `bind_insert_row` と同じ規則に
  揃えている。
- INDEX-4 の 4 上限（`batch_limits.rs`）: 複数行 `INSERT` は行を全て集めて
  から一括判定するのに対し、COPY はストリーミングであるため CopyDone を
  待たずに超過を検出する。`check_running_total`（③生 CopyData バイト量）は
  `feed` の先頭で `pending` へ追加する前に判定し、①（行数）・④（生成チャンク
  数。1 行=1 チャンクと読み替える既存の単文 INSERT 経路と同じ読み替え）・
  ②（当該行のデコード後バイト量）は完了したレコードごとに判定する。いずれの
  超過も副作用ゼロ（`Vec<BoundInsert>` へ積む前に拒否）。
- commit（`EngineCore::commit_copy_in`）: `CopyInSession::finish` が確定させた
  `CopyInBatch` を `execute_bound_insert_in_session`（SQL-16 と同一の実行器）
  へ委譲する。`bind` closure は束縛時点（`begin_copy`）に取得したスキーマが
  commit 時点のスキーマと一致することを検証し、不一致は `22000` で
  fail-closed に拒否する（CopyData 受信中の DDL によるスキーマ食い違いを
  防ぐ）。

### TO STDOUT（`EngineCore::begin_copy` の `CopyPlan::To` 分岐）

- 広域取得（`sql::scan::execute_scan`。RLS 暗黙適用・`WHERE`／`LIMIT` 有界の
  早期終了走査）をそのまま呼ぶ（第 2 の SELECT 実行器を持たない）。
- 行の値表現は `result_encoder::cell_to_text`（通常の SELECT 応答と同じ）を
  土台に、text はバックスラッシュエスケープ、CSV は二重引用符エスケープへ
  再エンコードする。`COPY (...) TO STDOUT` の出力を同じテーブルへ
  `COPY ... FROM STDIN` で再投入すると元の値へ戻る対称性を持つ。

### エラー処理・PostgreSQL 本家との相違点

1. **`\.` 終端行**: protocol v3 の COPY は CopyDone（'c'）メッセージで終端を
   表現するため、protocol v2 由来の `\.` 終端行は本実装では受理・要求しない。
2. **COPY FROM STDIN 中のエラー応答タイミング**: PostgreSQL 本家は
   ErrorResponse を即座に送るが、クライアントはその後も CopyDone／CopyFail
   を送ってくることを許容し続ける必要があり、次の 'Q' が先に届く可能性の
   ある接続レベルの「読み捨て状態」を要求する（`docs` 化する前の設計検討では
   この接続レベル状態を `handshake::post_auth_loop` へ持たせる案を検討した）。
   本実装は COPY サブプロトコルの開始から終了までを **1 回の関数呼び出し
   （`wire-server::copy::run`）で完結させる単純化**を採用し、`feed` が失敗
   した時点では応答を送らず「以降の CopyData を読み捨てる」状態へ移り、
   CopyDone／CopyFail を受信してからまとめて ErrorResponse＋ReadyForQuery を
   送る。psql `\copy`・libpq は COPY サブプロトコルを開始したら必ず
   CopyDone／CopyFail のいずれかで終端させる（次のクエリへ先に進まない）ため、
   実用上の互換性は保たれる。読み捨てる総バイト数は
   `limits::COPY_DISCARD_MAX_BYTES`（16 MiB。フレームヘッダー分の固定
   オーバーヘッド `COPY_DISCARD_FRAME_OVERHEAD_BYTES` も含む）と、読み捨て
   フレーム件数の上限 `limits::COPY_DISCARD_MAX_MESSAGES` の両方で有界化し、
   いずれかを超過時は `08P01` で切断する（DoS 対策。body が空のフレームを
   連送してもバイト予算だけでは非現実的な件数が必要になるため、件数上限を
   独立に設けている）。`d`（CopyData）に加え、`H`（Flush）／`S`（Sync）も
   この読み捨て予算を共有する（後述）。
3. **`CopyFail` の `wire_code`**: PostgreSQL は `57014`
   （query_canceled）を返すが、ERR-6 の管轄表に `57014` が無く表外の新設は
   禁じられているため、既存の分類（`InvalidInput`／`22000`）へ写像した。
   spec 側で確認してもらう事項として申し送る。
4. **空の COPY（0 行）**: 既存の複数行 `INSERT` バッチ契約と同じく `22000`
   （空バッチ拒否）とする。PostgreSQL の `COPY 0`（成功）とは異なる。
5. **既定の①（`max_files_per_batch`）が 64 行**: 既定設定では 65 行以上の
   COPY は `54000` になる。契約どおりの挙動だが、既知の運用上の制約
   （起動時 CLI `--batch-max-files`〔優先〕または環境変数 `VECTOR_DB_BATCH_MAX_FILES` で上書き可能）として記録する。
6. **Flush（'H'）／Sync（'S'）の受理形状**: PostgreSQL wire v3 上この 2 種類は
   length=4（body 厳密に空）以外の形状を持たない。COPY サブプロトコル中に
   届いた場合は本文なしの no-op として無視するが、本文付き（宣言長が
   4 バイトを超える）ものは `c`（CopyDone）／`X`（Terminate）と同じ
   `read_length_prefixed_body(stream, 4, 4)` で検証し `08P01` として
   fail-closed に拒否する（CopyDone・Terminate に既に課していた形状検査を
   本 2 種類にも揃えた）。
7. **複数文メッセージ（WIRE-16・TASK-219）との関係（Issue #1175 で文単位へ変更）**:
   `COPY` の判定は文単位（`simple_query::run_copy_statement`。`handshake::
   post_auth_loop` は `'Q'` をすべて `simple_query::execute_and_respond` へ委譲する）。
   - `<読み取りの文>; COPY t FROM STDIN ...`（書き込みが最後の 1 文）→ 逐次実行され、
     前の文の応答、CopyIn、`COPY n`、`ReadyForQuery`（PostgreSQL と一致）。
   - `COPY (SELECT ...) TO STDOUT; <他の文>`（`TO` は読み取りに分類）→ CopyOut、
     `COPY n`、続きの文の応答、`ReadyForQuery`（PostgreSQL と一致。`copy::run` は
     `Finish` を受け取り、途中の COPY では `ReadyForQuery` を送らない）。
   - `COPY t FROM STDIN ...; <他の文>`・`<書き込み>; COPY t FROM STDIN ...` →
     書き込みが最後以外にあるためメッセージ全体が暗黙トランザクションになり、
     トランザクション内の COPY は未対応（`0A000`）。CopyIn へは入らず、暗黙
     トランザクションは全体をロールバックして `ReadyForQuery('I')`。**既知の相違**:
     PostgreSQL は COPY を受理するが、後続文のエラー時に COPY を巻き戻せないため
     原子性を優先して拒否する（従来は `42601`）。
   いずれの形でも接続はデシンクしない
   （`crates/wire-server/tests/wire17_copy.rs` の複数文相互作用テスト参照）。

## 対象ファイル

| パス | 変更概要 |
| ---- | -------- |
| `crates/engine/src/sql/allowlist.rs` | `validate_sql` → `validate_sql_tokens` への純粋な切り出し、`validate_copy`／`validate_copy_tokens`・`CopyStatement`・`ValidatedCopyFrom`／`ValidatedCopyTo`・`CopyFormat` |
| `crates/engine/src/sql/copy.rs`（新設） | `is_copy_statement`・`RecordSplitter`・text／CSV デコーダ・`bind_copy_record`・`CopyInSession`／`CopyInBatch` |
| `crates/engine/src/core.rs` | `CopyPlan`・`EngineCore::begin_copy`／`commit_copy_in` |
| `crates/engine/src/batch_limits.rs` | 逐次判定ヘルパー（`check_row_count`／`check_row_body_len`／`check_running_total`） |
| `crates/engine/tests/copy_from.rs`（新設） | engine API の層 A（原子性・INDEX-4・台帳共有・RLS・fail-closed） |
| `crates/wire-server/src/copy.rs`（新設） | CopyIn／CopyOut サブプロトコル・行エンコーダ |
| `crates/wire-server/src/handshake.rs` | 'Q' 分岐での COPY 委譲（`is_copy_statement` の覗き見） |
| `crates/wire-server/src/result_encoder.rs` | `cell_to_text` の `pub(crate)` 化（COPY TO の行エンコーダが共有） |
| `crates/wire-server/src/limits.rs` | `COPY_DISCARD_MAX_BYTES`・`COPY_DISCARD_FRAME_OVERHEAD_BYTES`・`COPY_DISCARD_MAX_MESSAGES` |
| `crates/wire-server/src/lib.rs` | `pub(crate) mod copy;` |

## セキュリティ考慮（OWASP Top 10・AGENTS.md P0）

- **インジェクション**: `TO STDOUT` の内側 `SELECT` はトークン列のまま
  既存の許可リスト（`validate_sql_tokens`）へ渡し、SQL 文字列を組み立て
  直したり連結したりしない。行データは値として束縛するのみ。
- **テナント境界**: 書き込みのテナントは `ctx.tenant_id()` と
  `Visibility::Private` に固定（既存の書き込み経路をそのまま使う）。
  `COPY TO STDOUT` は広域取得の RLS 暗黙適用をそのまま経由する。
- **DoS**: 生 CopyData バイト量（③）はバッファへ追加する前に判定する。
  読み捨て状態のバイト総量は `COPY_DISCARD_MAX_BYTES`、フレーム件数は
  `COPY_DISCARD_MAX_MESSAGES` でそれぞれ独立に有界化する（`d`／`H`／`S` の
  いずれも本文が空でもフレームヘッダー分のオーバーヘッドとして予算を
  消費するため、極小フレームの連送で予算を無限に回避できない）。
  Flush／Sync は本文付きを受理しない（本文付きは `08P01`）。
- **fail-closed**: 曖昧なものはすべて拒否する（未対応のエスケープ・
  オプション・想定外のメッセージ、スキーマの競合）。commit は CopyDone の
  後に 1 回だけ行い、それより前のどの失敗でも副作用はゼロ。
- **untrusted 入力の経路**: `unwrap`／`expect`／`[]` を使わず `get()`／
  `split_first`／イテレータで処理する。
- **spec の機密保持**: 本 doc・コード・コミットは TASK-220・WIRE-17・
  INDEX-4 のポインタのみを使い、spec 本文を転記しない。
