# UDF カタログのスキーマと保存形式（Issue #1919）

親 Issue #1918（UDF の永続化）の土台。ビヘイビア ID は SQL-66（関連: SQL-9・SQL-67 (3)・
TABLE-27・RLS-12・TASK-309）のポインタ表記のみで参照し、spec 本文は転記しない。

## 範囲

- #1919: redb のテーブル定義、定義 blob の版付き保存形式、crate 内部の保存・読み出し API
  （`crates/engine/src/catalog/function.rs`）。
- #1920: `CREATE FUNCTION`／`DROP FUNCTION` からの書き込み（下記「書き込み経路」）。
- #1921: セッション開始時のロードと他セッションの変更の反映（下記「ロード経路」）。
- 範囲外: 件数上限の設定・DDL 権限（#1922）、依存追跡、`OR REPLACE` の意味論。

## テーブル

| 表 | キー | 値 |
| -- | ---- | -- |
| `functions` | `(scope, function_key, arg_signature)` | 定義 blob |
| `function_id_counter` | 固定キー `next_id` | `u64`（1 始まり・再利用しない） |

どちらも最初の書き込みまで作らない（遅延作成）。読み取りは表の不在を 0 件と扱うため、UDF を
使わない DB や本変更前の DB のファイル構成・既存カタログの値は変わらない。

## キー設計

- `scope`: 可視範囲の所有者。本 Issue では検証済みの `tenant_id` のみ受理する。キーの先頭に置くため、
  他テナントの定義は構造上引けない（RLS-12 の境界）。
- `function_key`: relation キー形（`public` は裸の名前、SCHEMA 配下は `#<schema_id>.<name>`）を
  ASCII 小文字へ正規化したもの。
- `arg_signature`: 多重定義を別キーにするための引数型の並び。各引数を typmod・配列の最大長と次元を
  落とした型トークンへ写して `;` で連結する（型注釈なしは `?`、引数 0 個は空文字列）。decode 時に値から
  再計算して一致を検査する。

## 定義 blob v1

リトルエンディアン。文字列は `u32` 長 + UTF-8。

```text
u8 version(=1) / u64 id / u8 owner(予約=0) / u8 language / u8 volatility / u8 flags
/ u8 parallel / cost(u8 有無 [+ f64]) / rows(u8 有無 [+ f64])
/ u32 引数数 { name(u8 有無 [+ str]) / type(u8 有無 [+ str tag + str param]) }
/ return type(同上) / str 本体の原文
```

- 版の運用: 未知の版は拒否する（`CorruptSchema`）。形式を変える場合は新しい版を足し、v1 の decode を
  残す。ゴールデンバイト列のテストで v1 を固定している。
- 型は `ColumnType::catalog_fields` の `(tag, param)` 1 対で持つ。ENUM は本 Issue では encode・decode
  とも拒否する。受理に広げても `(tag, param)` のバイト表現は変わらないため、版を上げずに解禁できる。
- 上限: 引数 100（格納形式上の上限）・本体 64 KiB・スコープあたり 64 件（`ViewLimitExceeded`）。長さ・件数は
  確保の前に検査する。

## fail-closed

未知の言語／揮発性／parallel タグ、未定義の flag ビット、owner 予約領域の非零、id=0、切断、末尾の余り、
上限超過、不正 UTF-8・識別子・型、キーと値のシグネチャ不一致はすべて固定文言の `CorruptSchema`。
blob の断片・tenant_id・本体の原文はエラー文言へ出さない。

## 書き込み経路（Issue #1920）

`CREATE FUNCTION`／`DROP FUNCTION` をカタログへ結線した（実行本体は `sql/udf_ddl.rs`、呼び出し元は
`core.rs`）。

- **判定順（CREATE）**: 構文（`AS` 以降のトークン列を `lexer::render_tokens` で描画し `body_source` にする。
  CREATE VIEW の `body_sql` と同じ規約。名前解決後のトークンのため SCHEMA 配下は `#<id>.f` 形のまま。
  描画不能・上限超過は拒否）→ `prepare_function_definition`（予約名・識別子・引数・閉じた本体。レジストリ
  不変）→ セッションに同名があれば `42723` → `Storage::create_function`（単一 write txn・1 回の commit。
  同名の定義があれば `42723`・件数上限 `54000`）→ commit 成功後にレジストリへ登録。カタログで失敗した
  場合セッションは変わらない。
- **DROP（最小形）**: `DROP FUNCTION <名前> [(<型>, ...)]` のみ。`IF EXISTS`・`CASCADE`／`RESTRICT`・複数名は
  `0A000`（#1917 が受理）。カタログ（DDL 権限ありのときだけ）とセッションのレジストリの両方から消し、
  どちらでも消えなかった場合だけ `42883`（他テナントの定義も同じ。存在情報を出さない）。型の並びは引数の
  数と、型注釈のある引数の型で照合する（型注釈なしの引数は任意の型に一致）。
- **権限**: `session.ddl_allowed()` のときだけカタログへ書く。権限が無いセッションは従来どおりセッション内
  だけ（カタログは読みも書きもしない）。`42501` ゲートへの切り替えは #1922。
- **多重定義**: 同名の定義がカタログに 1 件でもあれば、シグネチャが違っても `42723`。`UdfRegistry` は名前
  単位でしか持てず、書けても読めない状態を作らないため。名前だけの DROP が常に一意に解決できる。
- **明示トランザクション**: DDL 権限ありのセッションの `CREATE`／`DROP FUNCTION` は `0A000`（ROLLBACK で
  取り消せない書き込みを txn 内で行わない）。権限なしのセッションはセッション内登録のみで従来どおり。
- **DROP SCHEMA との連動**: `RESTRICT` は配下に関数があれば `2BP01`。`CASCADE` は同じ write txn で配下の
  関数行（自テナント・自 SCHEMA の接頭辞）も消す。残すと内部キーを書けない利用者が DROP できない孤児になる。
- **crash 耐性**: 新しい crash-test ジョブは足さない。CREATE／DROP はいずれも単一 write txn の 1 回の
  `commit_boundary::commit` で、CREATE VIEW・SEQUENCE と同じ保証になるため、再オープンを検証する
  in-process テスト（`tests/sql_t40_up/sql66_udf_catalog_write.rs`）で足りる。
- **public の共有**: 見送り（テナント単位を維持）。全テナント共有は予約 scope（空文字列）で後から足せ、
  blob 形式は変わらない。spec 側の確認事項。
- **スコープ外の扱い**: ENUM 型は解禁せず、カタログが拒否した場合は `0A000`。

## ロード経路（Issue #1921）

実装: `crates/engine/src/sql/udf_load.rs`。`EngineCore::load_session_udfs`（開始時）と
`refresh_session_udfs`（Query／Parse の前）が薄いラッパー。

- 対象: 認証済みテナントの scope だけ。DDL 権限の有無を問わずロードする（呼び出しに DDL 権限は要らない）。
- 再パース: `body_source` を `tokenize_rendered` で字句解析し、`CREATE FUNCTION` と同じ式文法で再パースして、
  作成時と同じ検証（`prepare_function_definition`。予約名・閉じた式・上限）を通す。
- 登録順: `id` 昇順で試し、失敗した定義は次のパスへ回す（呼び出し先を DROP して作り直すと id が逆転するため）。
  進捗が無くなれば終了し、パス数は `MAX_FUNCTIONS` が上限。
- 出所: レジストリはカタログ由来の名前を覚える。権限の無いセッションがカタログ由来の関数を `DROP FUNCTION`
  すると `42501`（呼べる関数に `42883` は事実と食い違うため）。セッション内だけの登録は従来どおり。
- 失敗の扱い（2 層）:
  - カタログ全体が読めない（blob 破損・I/O）: ハンドシェイクは `AuthenticationOk` の後に FATAL `XX000`
    （固定文言）を返して切断する。refresh ではカタログ由来を全部消し、次回再試行する。
  - 1 件の定義が登録できない（再パース失敗・予約名との衝突・呼び出し先の消失・レジストリで表せない形
    〔`language` が Declarative 以外・型付き引数・戻り値型〕）: その定義だけ登録しない（呼ぶと `42883`）。行は
    カタログに残り、DDL セッションが `DROP FUNCTION` で消せる。skip したことを知る手段は限られる。
- テナント束縛（P0）: レジストリがロード時の scope を記録し、別テナントの ctx での呼び出しは何もロードせず
  カタログ由来を全部消す。
- 世代: `Storage` がテナント単位の世代をメモリ上に持つ（永続化しない。redb は単一プロセスが占有し、再起動後は
  全セッションが開始時にロードするため）。`CREATE`／`DROP FUNCTION`・`DROP SCHEMA ... CASCADE` の commit 成功後に
  進める。読み取り側は世代を先に読んでから一覧するため、競合しても次の確認で必ずリロードされる。
  `BEGIN` 時のセッション複製が `ROLLBACK` で古い世代へ戻っても、次のメッセージで再びリロードされるだけで害はない。
- 適用範囲: ロードを結線するのは pg wire のハンドシェイクだけ。HTTP（NoSQL）表層はリクエストごとに
  `SessionState::default()` を作るため変えない。

## 後続への申し送り

- 共有 `public` の扱い: 上記のとおりテナント単位を維持。共有する場合は spec 側の確認が要る。
- ENUM 型の引数・戻り値の解禁（形式は不変）。
- `sql::udf_call::MAX_UDF_PARAMS`（32）は構文段で先に効き、格納上限 100 の内側に収まる。100 と `54023` への
  引き上げは #1917／#1922。
- #1921（ロード）: 実装済み（「ロード経路」）。準備済み文（Parse 済み）が他セッションの DROP の後に
  陳腐化する件は未対応（refresh は Query／Parse の時点だけ）。
- `MAX_SESSION_UDFS`（64）と `MAX_FUNCTIONS`（64）は等しく、カタログが満杯だとセッション内の登録・WASM を
  入れる余地が無い。#1922（上限の CLI 設定）で値をずらすときは両者の関係を見直す。
- #1917: `IF EXISTS`・`CASCADE`／`RESTRICT`・複数名・`OR REPLACE`・依存追跡（`2BP01`）・多重定義の解決規則と
  `42725`（現状は名前の多重定義を作らないため、防御的な `Internal` に写している）。
- #1922: `42501` ゲート（権限なしのセッション内経路の撤去を含む）、件数上限の CLI 設定、引数上限。
