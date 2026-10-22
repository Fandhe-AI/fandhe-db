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
- 明示トランザクション中の COPY（Issue #1179）: `Idle`・`Active` のとき `copy::run` へ
  委譲する。`COPY FROM STDIN` は `EngineCore::commit_copy_in_txn` で共有書き込み
  トランザクションへ書き込み（commit は `COMMIT` 文のみ）、`COPY (...) TO STDOUT` は
  未 commit の変更を反映する。COPY 中のエラー（CopyFail・デコード失敗・上限超過）は
  トランザクションを `Failed`（ReadyForQuery `'E'`）へ遷移させ、成功時の状態バイトは
  `txn.status()` から導出する。`Failed` 中の COPY は `25P02`。受信期限は
  ハンドシェイク層のフレーム受信期限が及び、期限超過で接続を閉じてライタを解放する
  （詳細は `docs/design/explicit-transaction.md`「wire-server への結線」）。
- 対象外（起票済み・別 Issue／別タスクへ申し送り）:
  - 拡張クエリプロトコル（TASK-216）経由の COPY の、CopyFail 等の後の Sync までの
    同期回復の詳細（Issue #1761）。Parse／Bind／Describe は Issue #1759、Execute は
    Issue #1760 で対応済み（下の 2 節）。
  - COPY オプションの TO 側の適用（Issue #1765。構文の解析は Issue #1763、FROM 側の
    `HEADER`／`DELIMITER`／`NULL`／`QUOTE`／`ESCAPE` の適用は Issue #1764 で実装済み。
    下の「許可リスト」「FROM STDIN の逐次取り込み」節）、binary 形式（WIRE-14 関連）、
    テーブル形 `COPY t TO STDOUT`、8 進・16 進エスケープ。

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
- COPY オプションの構文（Issue #1763・WIRE-22 ポインタ。`sql/copy_options.rs`）: 新構文
  `[WITH] (opt [value], ...)` と旧構文（`CSV HEADER`・`DELIMITER [AS] 's'` 等。順序任意）を
  受理し、簡易クエリ・拡張クエリの Parse で共通の検証を行う。エラー分類は、構文エラー・
  オプションの重複・未知のオプション・値の欠落が `42601`、未知の形式名・不正な値・区切り文字
  等の制約違反が `22023`、モード不一致（text での `QUOTE`／`ESCAPE`）・1 バイトでない
  区切り文字・`HEADER MATCH` の `COPY TO` での使用・`FORCE_*` 等の未対応オプションが
  `0A000`。エラー文言には入力値を含めない。
- ゲート: データ経路が未適用の方向では、解決結果が format の既定値と異なる指定（`HEADER` 等）を
  `0A000` で拒否する（無言の fail-open の防止）。`COPY FROM` は Issue #1764 で適用済みのため
  ゲートを外した。`COPY TO` は Issue #1765 が `reject_unapplied_for_to` の呼び出しを外すまで
  残る（既定値と等しい明示指定と旧構文の `CSV` は両方向で受理する）。
- 既知の相違: `FORMAT binary` は WIRE-23 まで従来どおり `42601`、`WITH` だけでオプションの
  無い指定も `42601`（PostgreSQL は受理）、エスケープ文字列 `E'...'` は字句解析が未対応のため
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
  （`\\`／`\t`／`\n`／`\r`／`\b`／`\f`／`\v` と `\<区切り文字>`。生バイト列が NULL 文字列
  〔既定 `\N`〕と全体一致するフィールドのみ NULL）、CSV は RFC4180 風（引用符なしで NULL 文字列
  〔既定は空欄〕と一致するフィールドが NULL、引用符ありは決して NULL にせず `""` は空文字列）。
  いずれも未対応のエスケープ・不正な UTF-8 は `22000` で fail-closed に拒否する。
- オプションの適用（Issue #1764・WIRE-22 (1)(3)(6) ポインタ。`CopyInSession::new` が解決済みの
  `CopyOptions` を受け取る）:
  - `DELIMITER`／`NULL`: text は 1 パスのバックスラッシュ認識分割（エスケープされた区切りでは
    分割しない。生バイト列で NULL 判定してからエスケープを解決する）。既定のタブ区切りでも
    `\<TAB>` がリテラルのタブとして受理される（PostgreSQL と同じ）。
  - `QUOTE`／`ESCAPE`（csv）: `CsvRecordScanner` が `quote`／`escape`／`delimiter` を保持する。
    `ESCAPE != QUOTE` では引用内で `QuotedEscape` 状態を持ち、`ESCAPE` の次が `QUOTE`／`ESCAPE`
    ならそのバイトをデータ、それ以外なら `ESCAPE` 自体をデータとして扱う（チャンク境界を
    またいでも状態で保持。`ESCAPE` で終わるストリームは未終端引用として `22000`）。引用外の
    `ESCAPE` はリテラル。
  - `HEADER`: 最初に確定した 1 レコードをヘッダ行として扱う（引用付き・チャンク境界をまたぐ
    ヘッダも同じ splitter で 1 レコードになる）。`true` は破棄、`MATCH` は列数と各列名
    （`id` 疑似列を含む COPY 列リスト。大小区別・NULL 不可）の一致を検査し、不一致は固定文言の
    `22000`（ヘッダの内容は含めない）。ヘッダ行は行数①・チャンク数④・束縛の対象外で、生バイトは
    ③に従来どおり計上される（ヘッダ用の保持は 1 レコード分で③により有界）。
  - csv で `QUOTE`／`ESCAPE` が CR／LF の場合は、レコード終端と曖昧になるため CopyInResponse
    送出前に `0A000` で拒否する。
  - 行形式エラー（列数過不足・未終端引用・ヘッダ不一致）は従来どおり `22000` を維持する
    （WIRE-17 の改訂注記にある別コードへの切り替えは WIRE-22 確定時の扱い）。
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
   （query_canceled）を返すが、本リポは既存の分類（`InvalidInput`／`22000`）へ
   写像した（ERR-6 ポインタ）。
4. **空の COPY（0 行）**: 既存の複数行 `INSERT` バッチ契約と同じく `22000`
   （空バッチ拒否）とする。PostgreSQL の `COPY 0`（成功）とは異なる。ヘッダ行のみで
   データ行が 0 のストリームも同じ扱い（Issue #1764）。

   Issue #1764 で追加した既知の相違: (a) `HEADER true` でもヘッダ行を他の行と同じ
   デコードに通すため、不正なエスケープ・不正 UTF-8 を含むヘッダは `22000`（PostgreSQL は
   読み飛ばす）。(b) csv の `ESCAPE != QUOTE` で閉じ引用符直後の引用符は引用の再開として
   受理せず `22000`（PostgreSQL は受理。COPY TO はこの形を出力しない）。(c) text の
   `\<LF>`（エスケープされた改行による行継続）は未対応で、生 LF は常に行終端。
5. **既定の①（`max_files_per_batch`）が 64 行**: 既定設定では 65 行以上の
   COPY は `54000` になる。契約どおりの挙動だが、既知の運用上の制約
   （起動時 CLI `--batch-max-files`〔優先〕または環境変数 `FANDHE_DB_BATCH_MAX_FILES` で上書き可能）として記録する。
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

## 移行済み表での `id` 列の省略（Issue #1700）

移行済み表（[migrated-id-identity.md](migrated-id-identity.md)）への `COPY FROM STDIN` は列リストから
`id` を省略でき、行順に所有 SEQUENCE から採番する（`\N` を渡した `id` 列は `22000`。従来方式の表は
`id` 必須のまま）。採番は session 付きの完了処理で INSERT と同じ文脈を張って行い、autocommit・
明示トランザクションの両方で動く。

## 拡張クエリの Parse／Bind／Describe（Issue #1759。親 #1758・WIRE-21 (1)・ERR-6 ポインタ）

- 受理: Parse で `COPY ... FROM STDIN`／`COPY (SELECT ...) TO STDOUT` を ParseComplete で受理する。
  Describe（statement）は ParameterDescription（0 個）＋ NoData、Describe（portal）は NoData、
  Bind はパラメータ 0 個で BindComplete。
- Parse 時検証: 他の文種と同じく Parse で許可リスト検証する（`EngineCore::parse_sql_prepared` が
  `validate_copy_tokens` を簡易クエリと共有。未定義表 `42P01`・許可外の形 `42601`・
  `operation_id` 必須化 `23502` の分類は簡易クエリの `begin_copy` と一致）。検証済みの文は
  `ParsedSql::Copy(ValidatedCopy)` で保持し、元の SQL テキストも持つ。`EngineCore::parse_sql`
  （簡易クエリ側の解析）は `COPY` を受理しない。
- `$n`: COPY に `$n` を置くと Parse で `42P02`（`ErrorClass::UndefinedParameter`・HTTP は 400）。
  位置検査（`42601`）より前、型宣言の過剰宣言（`08P01`）より前に判定する。文字列リテラル・
  コメント内の `$n` は字句解析で除外される。エラー後は既存の同期回復（Sync まで読み捨て）で
  接続を維持する。
- 一時表: 一時表を参照する COPY の Parse は `0A000`（fail-closed。一時表の経路〔`is_routable`〕が
  COPY を含まないため）。対応は Issue #1760 以降で判断する。
- Execute（#1760 で対応済み。次節参照。以下は #1759 時点の引き継ぎ契約の記録）: `execute_parsed_in_session`／`execute_in_active_txn` の
  `ParsedSql::Copy` の腕は `0A000` の fail-closed スタブ（行・台帳・世代に触れない。明示
  トランザクション中は Failed へ遷移）。#1760 は `ValidatedCopy::sql()` を `begin_copy`／
  `begin_copy_in_txn` へ渡す形でこの腕を置き換える。wire-server の `is_write_statement`
  判定（`extended_query.rs`）は `_` の腕で Copy を書き込み扱いにする点も #1760 で見直す。

## 拡張クエリの Execute（Issue #1760。親 #1758・WIRE-21 (2) ポインタ）

- 分岐位置: CopyIn／CopyOut は stream を要するため engine では実行できない。wire-server の
  `extended_query::execute_portal` が `ParsedSql::Copy` の portal を `execute_parsed_in_txn` へ渡さず
  横取りし、`handle_execute` が `simple_query::run_copy_statement`（`Finish::Continue`）経由で
  `copy.rs` へ委譲する。簡易クエリと同じ gating（暗黙 Tx の `0A000`・`Failed` の `25P02`）と
  完了タグ・Tx 遷移を共有し、第 2 の COPY 実行器を作らない。engine の `ParsedSql::Copy` の
  `0A000` 腕は他の呼び出し元向けの防御的な fail-closed として残す。
- ReadyForQuery: Sync が送るため `copy.rs` は `CommandComplete` か ErrorResponse だけを書く。
  copy-in 中の Flush／Sync は既存どおり無視される（libpq 型の一括送信でも RFQ は完了後に 1 回）。
- portal 状態: 実行を試みる時点で `Failed` へ倒し、成功時のみ `Done { tag }` へ上書きする。
  再 Execute は既存の `Done` 契約どおりタグだけを再送する。失敗時は `copy.rs` が ErrorResponse を
  書き済みのため二重送出せず、`ignore_till_sync` だけを立てて Sync で回復する（`CopyStep::Failed`）。
  Terminate／早期 EOF は接続終了。
- TOCTOU: Parse 時の検証結果は信用せず、元 SQL を `begin_copy*` へ渡して実行時点で再検証する
  （Parse 後に表が DROP されていれば CopyInResponse の前に `42P01`）。
- `COPY (...) TO STDOUT` も同じ経路で処理する。`max_rows` は COPY には無関係で無視する。
- `is_write_statement`: COPY は計算より前に横取りされるため到達しない（明示の腕で明記）。
- 層 A: `crates/wire-server/tests/wire21_copy_execute.rs`（簡易クエリとのバイト同一性・
  テナント境界・libpq 型順序を含む）。実クライアント（psycopg の pipeline 等）での層 B 確認は
  未実施（親 #1758 で扱う）。#1761 への申し送り: エラー後の Flush／Sync 規則の詳細、
  ignore_till_sync 中に届いた CopyData／CopyDone／CopyFail の扱い。

## 拡張クエリの Sync 同期回復（Issue #1761。親 #1758・WIRE-21 (3) ポインタ）

COPY の Execute が失敗した後は、ErrorResponse を返して `ignore_till_sync` に入り、Sync で
ReadyForQuery を返して接続を継続する。層 A: `crates/wire-server/tests/wire21_copy_sync_recovery.rs`。

### 接続継続可否

| 事象 | 応答 | 接続 | RFQ 状態 |
| ---- | ---- | ---- | -------- |
| CopyFail・行デコード失敗・INDEX-4 上限超過・commit 時の制約違反 | ErrorResponse | 継続（Sync まで読み捨て） | 暗黙 `I`／明示 `E` |
| Execute 時の再検証失敗・gating（`25P02` 等） | ErrorResponse（CopyInResponse 前） | 継続 | 同上 |
| フレーム TooLarge／Malformed | ErrorResponse | 切断 | - |
| 不正形 CopyFail・同期回復中の本文付き CopyDone | `08P01` | 切断 | - |
| 読み捨て予算超過 | `08P01` | 切断 | - |
| copy-in 中の非 COPY メッセージ（WIRE-21 (2)） | `08P01` | 切断 | - |
| Terminate／EOF | なし | 切断 | - |

### 規則

- Flush／Sync: copy-in 中（CopyDone／CopyFail 受信前）は既存どおり無視する。失敗後の Flush は
  何も返さず、Sync が ReadyForQuery を返す。ErrorResponse は Sync を待たず flush 済みで届く。
- 同期回復中の CopyData／CopyDone／CopyFail は、失敗の発生源を問わず応答なしで読み捨てる。
  本文はアロケートせず、`limits::COPY_DISCARD_MAX_BYTES`／`COPY_DISCARD_MAX_MESSAGES` の
  予算（`ExtendedQueryState` が保持し Sync で 0 へ戻す）で有界化する。
- 同期回復の外での CopyData／CopyDone／CopyFail は WIRE-8 の `0A000` + 切断のまま。
- 明示トランザクション中の失敗は `Failed`（後続は `25P02`）。暗黙トランザクションでは commit しない。

### 判断と対象外

- ErrorResponse は CopyDone／CopyFail 受信後にまとめて送る方式（簡易クエリ COPY と共有経路・
  WIRE-21 (5) のバイト同一性）を維持する。PostgreSQL 本家同等の即時送出は別課題。
- 実クライアント（psycopg 等）での層 B 確認は親 #1758 で扱う。

## Issue #1767: binary 形式のフレーム層（親 #1766・WIRE-23 (1)(2) ポインタ）

`crates/wire-server/src/copy_binary.rs` に、列の型を知らない binary COPY のフレームコーデックを新設した
（形式は PostgreSQL 公式ドキュメント「COPY — Binary Format」）。本番の呼び出し元はまだ無く、結線は
兄弟 Issue が担う（モジュールは `dead_code` を許可）。

- 責務分割: parser（`BinaryCopyReader`）はヘッダ・タプル・トレーラをフィールド単位のバイト列へ分解するだけで、
  型変換は #1768、`CopyInSession` への結線と INDEX-4 上限との接続は #1769、`COPY TO STDOUT` は #1770。
  writer はヘッダ・タプル・トレーラを別関数にし、PostgreSQL と同じく別 CopyData で送れるようにした。
- 状態機械: バイト単位で署名・flags・拡張長・タプルのフィールド数・フィールド長・本文を進め、
  CopyData の分割位置に依存しない。一度エラーになると以降の入力はすべて拒否する。
- 検証: 署名の完全一致、OID flag と critical flag の拒否、負のヘッダ拡張長の拒否（`22P04`）。
  フィールド数不一致・`-1` 未満の長さ・トレーラ後のデータ・途中終端も `22P04`。フィールド長と
  タプル合計の上限超過は確保前に `54000`。宣言長を確保量に使わず、ヘッダ拡張はカウンタで読み飛ばす。
- 終端: トレーラ済み、またはタプル境界での終端は成功。フィールド数の 1 バイト端数だけで終わる入力は
  PostgreSQL では EOF 扱いになりうるが、本実装は fail-closed で `22P04` とする（既知の相違）。
  0 タプルは parser では成功とし、空バッチの扱いは #1769 で engine の契約に接続する。
- 対象外・申し送り: `22P04` の `ErrorClass` variant と HTTP／nosql-api.md／err4 テストへの射影、
  `CopyFormat::Binary` の許可、`CopyResponse` の format 化は #1769／#1770。binary は引き続き `42601` で拒否する。

## Issue #1768: binary 形式の型コーデック（親 #1766・WIRE-23 (3) ポインタ）

`crates/wire-server/src/copy_binary_types.rs` に、列の型ごとの binary 値変換層を新設した。フレーム層
（`copy_binary.rs`）が切り出したフィールドを正規テキストへ復号し（COPY FROM）、結果セルを binary 値へ
符号化する（COPY TO）。本番の呼び出し元はまだ無く、結線は #1769（FROM）・#1770（TO）が担う。

- 対応型の単一情報源: `result_encoder::column_binary_support`（Bind・DataRow と同一集合）。新たに足したのは
  `WireType → BinaryParam` の網羅 `match`（`binary_param_for_wire_type`）のみで、`supports_binary()` との
  一致を単体テストで固定している。COPY binary はフィールドに OID を持たないため Bind の OID 照合は使わない。
- 対応型: 整数（int2/4/8）・float4/8・bool・text/varchar/bpchar・bytea・uuid・time・timestamptz・interval・
  numeric（`id` を含む）。DATE／TIMESTAMP／JSON／JSONB／配列／ENUM／VECTOR／型を持たない式列は
  `resolve_columns` が列集合ごと `0A000` で拒否する（CopyIn／CopyOutResponse の前に呼ぶ契約）。
- 方式: binary → 正規テキスト → engine の型付き束縛（Bind と同じ。binary ≡ text を構造的に保証）。
  符号化は DataRow と同じ `encode_binary_cell` を再利用し、長さ無しの `encode_binary_payload_into` で包む。
- エラー写像: 長さ不正・numeric 表現不正・不正 UTF-8・NUL は `22P03`、範囲外は `22008`、numeric 上限超過は
  `22003`、内部不整合は `XX000`。既存 `ErrorClass` で表せるため新 variant は無い。
- 既知の相違: 固定長型の長さ不足は PostgreSQL では `08P01`、本実装は `22P03`（WIRE-23 (3) ポインタに従う）。
  text の不正 UTF-8・NUL は PostgreSQL では `22021`、本実装は新 SQLSTATE を入れず `22P03`。
  非有限 float や CHAR(n) の詰め／切り詰め、`id` の扱いは engine の text 束縛と同じ結果になる。
- 対象外・申し送り: `copy.rs` への結線・`CopyFormat::Binary` の許可・`22P04` の `ErrorClass` は #1769／#1770。
  date／timestamp／json／jsonb／配列／enum／vector の binary 対応は WIRE-14 側の変更を伴う別課題。
