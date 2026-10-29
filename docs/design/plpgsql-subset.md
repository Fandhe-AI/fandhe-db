# PL/pgSQL サブセットの範囲と実行モデル（Issue #2223）

- ステータス: **採用（設計記録。実装は Issue #2224〜#2229）**
- 対応: Issue #2223（親 #2222）
- ポインタ: SQL-68（関連: SQL-67・SQL-70・SQL-46・SQL-47・TABLE-45・RLS-10・RECOVER-1・
  RECOVER-11・TASK-311）。`docs/spec/04-behavior/sql-surface.md`・`docs/spec/05-tasks.md`
- 前提: Issue #2215 配下（#2216 ドル引用の字句化・#2217 `LANGUAGE` 句と属性・
  #2218 `LANGUAGE sql` 本体の実行・#2220 集合返却関数の `FROM` 利用）、UDF の永続化
  （PR #2775 で追加予定の UDF カタログ設計。本体は原文テキストで保存）
- 関連 ADR: [explicit-transaction.md](explicit-transaction.md)・
  [sql-cursor.md](sql-cursor.md)・[statement-interrupt.md](statement-interrupt.md)・
  [recursive-cte.md](recursive-cte.md)・[nesting-limits-cli.md](nesting-limits-cli.md)・
  [cast-conversion-rules.md](cast-conversion-rules.md)・
  [error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)・
  [sql-parse-bind-cache.md](sql-parse-bind-cache.md)

本書は設計記録であり、コードは変更しない。出典は public な Issue・PostgreSQL 17 公式
ドキュメントの PL/pgSQL 章・既存コード／ADR に限る。spec 本文は転記しない。

## 背景

- 現状の UDF は宣言的な式 1 つで、束縛時に `BoundExpr` へインライン展開される
  （`crates/engine/src/sql/udf_call.rs`）。`eval` は純粋関数で、ストレージ・セッション・
  トランザクションへの経路を持たない。手続き言語は無い
- #2222 は `LANGUAGE plpgsql` の関数・トリガ関数・`DO` を、主要な制御構文に絞った
  サブセットとして PostgreSQL 17 と同等に実行することを目標にする
- 後続の #2224〜#2229 が共有する契約（受理する構文と拒否方針・変数のスコープ・
  埋め込み SQL の実行・上限・台帳の整合・トリガ／`DO` との接続点）を本書で確定する

## 決定

### D1. モジュール配置と「第 2 の SQL 実行器を作らない」原則

- 新モジュール `crates/engine/src/plpgsql/`（`lexer`・`ast`・`parser`〔#2224〕、
  `interp`〔#2225〕、`host`〔埋め込み SQL の実行口。#2226〕、`trigger`／`do_block`
  〔#2228〕）。上限は `crates/engine/src/plpgsql_limits.rs`、CLI は
  `crates/wire-server/src/plpgsql_limits_opt.rs`
- インタプリタは AST を辿る木構造インタプリタ。SQL の実行は `host` のトレイト
  （core が実装）経由で、既存の実行経路（`EngineCore` の `execute_*_in_session` 系・
  明示 Tx の `execute_in_active_txn`）へ委ねる。cursor・評価後射影形ビューと同じ
  「内側の文を既存経路で実行して結果を後処理する」方式
- host は明示的な引数（`&mut dyn` の文脈）で渡す。thread-local に借用を載せる方式は
  `unsafe` が要るため採らない（`coding-rust.md`）。キャンセル確認だけは既存の
  `engine::interrupt::check()` を使う

### D2. 本体の受け取りと定義時・呼び出し時の検証

- 本体はドル引用または文字列リテラル 1 個として届く。**#2216 が前提**。現行の
  `sql::statement_splitter` は通常状態の `$` で分割しない（`SplitOutcome::Single`）ため、
  複数文メッセージで `$$` を正しく扱うには #2216 側で splitter の更新も要る
- `LANGUAGE plpgsql` の受理と属性（揮発性・`STRICT`）は #2217、保存は PR #2775 に揃える。
  AST は保存せず、原文を保存して呼び出し時に再解析する
- 定義時: PL/pgSQL の構文解析（範囲外は `0A000`、構文誤りは `42601`、上限超過は D7）と
  埋め込み SQL の構文検証（D3 のダミー置換）。表・列の名前解決は**呼び出し時**
  （PostgreSQL と同じ late binding。存在しない表は呼び出し時の `42P01`）。
  参照先への依存記録（`2BP01`）は行わない
- 初版は埋め込み文の計画キャッシュを持たず、実行のたびに許可リスト解析を通す
  （最適化の入口は `sql-parse-bind-cache.md`）

### D3. 変数の参照と値の受け渡し（インジェクションを構造的に塞ぐ）

- 埋め込み SQL は定義時にトークン列として切り出し、スコープ内の変数に解決した識別子
  トークンを記録する
- 呼び出し時は変数の**型付きの値から**、字句解析器が同じ値をリテラルで書いた SQL から
  生成するのと同一のトークン列を組み立てて置換し、通常の許可リスト解析へ渡す。
  `sql::params` の型付き置換（`substitute_values_typed`・`ParamLiteralKind`）と同じ構成
  原理を共有する。値は SQL テキストにも字句解析器にも戻らず、値経由のインジェクション
  経路は存在しない
- `sql::params::validate_param_positions` の受理位置制限（WIRE-12）は**適用しない**。
  その制限は拡張プロトコルの Parse 時点で値と型が未知であることに由来し、PL/pgSQL では
  どちらも既知。適用すると一般的な本体が定義時に `42601` になり PostgreSQL との差が
  大きくなる。リテラルを置けない位置（識別子位置など）の変数は許可リストの `42601`
- 定義時の構文検証は `substitute_dummy` の先例（Describe）と同様にダミー値で行う。
  ダミー起因の型だけの失敗は定義時に拒否せず呼び出し時の検証へ回す（呼び出し時に完全に
  検証するため書き込み経路は fail-open にならない）
- 変数名と列名が衝突する参照は PostgreSQL の既定（`variable_conflict = error`）と同じく
  `42702`（`ErrorClass::AmbiguousColumn`）。検出は #2226 が文の束縛時に行う
- 1 文が参照できる変数の数に上限を置く（`sql::params` の 1 文あたり番号上限と同じ流儀。
  超過は `54000`）

### D4. 変数のスコープと値

- ブロック単位のスコープ（内側が外側を隠す）・ラベル修飾（`<<lbl>>` と `lbl.var`）・
  引数は名前と `$n` の両方で参照・`FOUND` は関数ごと・`GET DIAGNOSTICS ... ROW_COUNT`
  は直前の SQL 文の行数（PostgreSQL 17 ドキュメント「宣言」「基本的な文」節）
- 代入の型変換は既存の代入時 CAST 規則（`cast-conversion-rules.md`・SQL-47 ポインタ）。
  値の表現は既存の型付きセル（`sql::exec::Cell`＋`catalog::ColumnType`）の再利用を推奨し、
  最終判断は #2225
- `%TYPE`・`%ROWTYPE`・`RECORD` は呼び出し時に表のカタログから解決する。対応は既存
  `ColumnType` の集合に限り、それ以外（多相型など）は `0A000`

### D5. 実行モデル: 同一 Tx・同一 RLS・書き込み経路の事前決定

- 本体の SQL はすべて呼び出し元と同じ `PolicyContext`（同じテナント・RLS-7 の暗黙適用）・
  同じトランザクションで実行する。`SECURITY DEFINER` は `0A000`（SQL-67 の権限の確定まで）。
  テナントの文脈を切り替える API を host に作らない
- **書き込み経路は事前に決める**。読み取りスナップショットから書き込みトランザクションへ
  文の途中で昇格する設計は、スナップショットの一貫性が崩れるため採らない。定義時に本体から
  「書き得る」フラグ（DML・`EXECUTE`・ユーザー定義関数呼び出しを含むか）を計算し、
  `VOLATILE` で「書き得る」関数を呼ぶ文と `DO` は最初から書き込み経路（writer gate を取得し、
  `WriteTarget::InTxn` 相当の 1 つの書き込みトランザクションを本体の全文へ渡す）で実行する。
  それ以外は外側と同じ読み取りスナップショット
- `IMMUTABLE`／`STABLE` 関数の本体が書き込もうとした時点で `0A000`（PostgreSQL と同じ）。
  読み取り経路で書き込みが試みられた場合（分類の配線漏れ）も fail-closed で拒否する
- 明示トランザクション中は既存の共有書き込みトランザクションに参加し、本体の書き込みも
  `mark_written`・`written_tables` の記録対象にする（`explicit-transaction.md`）。本体の
  後続文は同じ Tx の未 commit の変更を読む。本体のエラーは外側の文を失敗させる
  （autocommit は副作用ゼロ、明示 Tx は `Failed`／`25P02`）。`EXCEPTION` 節は #2227
- 複数文メッセージ（WIRE-16）: 字句段の `statement_splitter` は揮発性を知らないため、
  関数呼び出しを含みうる文と `DO` を保守的に書き込みと分類し、必要なら暗黙 Tx に倒す
  （偽陽性は暗黙 Tx 化するだけで正しさを損なわない）。実装は #2226
- **再入の不変条件**: host が入れ子の文を実行するのは、外側の実行器が同じ書き込み
  トランザクションの redb の表ハンドルを開いていない時点に限る（同じ書き込み
  トランザクションで同じ表を二重に開くと `TableAlreadyOpen`）。外側の対象行・可視行は
  関数の評価より前に確定させ、本体の書き込みの影響を受けない
- 呼び出し位置の初版範囲（#2218 が先に基盤を入れていれば共有し、本書は不変条件と最小集合を定める）:

| 呼び出し位置 | 初版 |
| ------------ | ---- |
| `DO` | 受理（トップレベル文として core が直接インタプリタを起動） |
| FROM の無い `SELECT f(...)`、本体内の代入・条件式・`PERFORM`・`SELECT INTO` | 受理 |
| 表を読む `SELECT` の投影 | 受理。外側の走査を実体化した後に行ごとに呼ぶ |
| `INSERT ... VALUES (f(...))` の値 | 受理。書き込み実行器を呼ぶ前に評価 |
| トリガ関数の呼び出し口（#2228 の内部 API） | 受理。発火位置は #1720／#1721 が不変条件を満たす位置に置く |
| `WHERE`・`ON`・`GROUP BY`・`HAVING`・`ORDER BY`・窓・`CHECK`・`DEFAULT`・式索引・部分索引・ビュー本文・NoSQL 表層 | `0A000`（拡大は別 Issue） |

- 純粋な式評価器（`udf_call::eval`）へ PL/pgSQL 呼び出しが届いた場合（配線漏れ）は内部
  エラーで fail-closed に拒否する

### D6. 動的 SQL（`EXECUTE`）

- 受理する。文字列は通常の文と同じ字句解析・許可リスト解析（SQL-8 ポインタ）・権限・RLS を
  通して実行し、動的 SQL によって本体の外の権限・テナントを得られない
- `USING` の値は文字列中の `$n`（`Token::Param`）を D3 と同じ型付きの値のトークンで置換して
  渡す（文字列連結ではない）。`$n` の番号の欠落・過剰は `42601`
- `EXECUTE ... INTO [STRICT]` は `SELECT INTO` と同じ規則。`FOR ... IN EXECUTE`・
  `RETURN QUERY EXECUTE` の初版での受理可否は #2224 で決め、受理しない場合は `0A000`

### D7. 資源上限（既定値＋起動時 CLI。下げる方向だけ）

`recursive_cte_limits`（#1871）・`nesting_limits`（#1552）と同型。
`engine::plpgsql_limits::PlpgsqlLimits`（`Copy`・フィールド非公開・ハード上限で飽和）を
`EngineCore::with_plpgsql_limits` で起動時に 1 回注入する。範囲検証は engine の
`validate_*` に一本化し、wire-server は CLI 文字列を厳密パースするだけ。グローバル状態は
使わない。全テナント同値（SQL-46 ポインタ）。以下は**実装既定値であり spec 由来の数値ではない**
（最終確定は #2225）。

| 上限 | CLI（案） | 既定＝ハード上限（案） | 超過時 |
| ---- | --------- | ---------------------- | ------ |
| トップレベル 1 回で実行する文の数（入れ子呼び出しを合算） | `--max-plpgsql-statements` | 100,000 | `54000` |
| ループ反復の回数（同上・合算） | `--max-plpgsql-loop-iterations` | 1,000,000 | `54000` |
| ブロック・制御構文の入れ子の深さ（構文段） | `--max-plpgsql-nesting-depth` | 32 | `54001` |
| 関数呼び出しの入れ子の深さ（再帰含む。`LANGUAGE sql` と共有） | `--max-function-call-depth`（#2218 と共有。先に入った名前に揃える） | 32 | `54001` |
| 本体の長さ | `--max-plpgsql-body-bytes` | 64 KiB（PR #2775 の格納上限と同値） | `54000` |

- 本体長は字句解析の前に検査する（untrusted 入力の無制限確保の禁止）。深さの上限は
  Rust の再帰スタックの保護も兼ねる
- `FOR` の問い合わせループ・`RETURN QUERY` の行は既存の実行器で実体化し
  （INSENSITIVE。`sql-cursor.md` と同じ判断）、`--max-result-rows`（SQL-34）を上限とする
- キャンセル: 文の実行ごと・ループの反復ごとに `interrupt::check()?` を呼び `57014`。
  確認点を commit・台帳の後ろに置かない（`statement-interrupt.md` の不変条件）
- NoSQL 表層には CLI を加えない

### D8. 台帳（operation_id）の整合

- 台帳のエントリは**外側の文の単位で 1 つ**（`recovery::ledger::record_in_txn` は外側の
  文だけが呼ぶ）。本体の文は台帳を記録しない。同じ `operation_id` の再送は既存どおり
  `23505` で外側の文ごと再実行せず、本体の副作用を重複させない
- `DO` は書き込み文として扱い、既存 DML と同じ文末の `USING OPERATION_ID '<id>'` を受け付ける
  （`DO` の単位で 1 エントリ）。内容照合（content_hash）は `DO` の本文テキストを含めて
  決定的に計算する（#2216 のドル引用トークンの直列化が前提）
- 本体の中・`EXECUTE` の文字列の中の `USING OPERATION_ID`・`PARTITIONED`（分割実行は文の
  途中で commit するため原子性が崩れる）は `0A000`
- `--require-operation-id`（`LedgerMode::Ledgered`）の構成で、`operation_id` を持たない文
  （`SELECT` を含む）から本体が最初に書き込もうとした時点で `23502` とし、文全体を取り消す
  （副作用ゼロ）。`SELECT` に `USING OPERATION_ID` を足すかは #2218 の判断に揃え、本書では
  追加しない
- 同一の書き込みトランザクションに載るため、クラッシュ後に外側と本体の書き込みの一方だけが
  残ることはない（redb の原子性。検証は #2229 の crash-test）
- 既知の性質: 同じ operation_id の再送の間に関数が再定義されても、台帳が記録済みなら
  `23505` で本体は実行しない（内容照合の対象は外側の文のテキスト）

### D9. トリガ関数と `DO` の接続点（関数側の契約だけ）

- `RETURNS trigger` の関数を通常の関数として呼ぶと `0A000`（PostgreSQL と同じ）。#2228 が
  内部 API（例: `invoke_trigger_function(ctx, TriggerCall) -> Result<TriggerOutcome, _>`）を
  用意する。`TriggerCall` は `NEW`・`OLD`（`RECORD`）と `TG_OP`・`TG_NAME`・`TG_WHEN`・
  `TG_LEVEL`・`TG_TABLE_NAME`・`TG_TABLE_SCHEMA`・`TG_NARGS`・`TG_ARGV`
  （PostgreSQL 17 ドキュメントのトリガ関数の節）を持つ。`TriggerOutcome` は「続行（書き換えた
  `NEW` を含む）」「NULL（当該行の操作をしない）」。返した行の構造が対象表と合わなければ
  `42804`（`ErrorClass::DatatypeMismatch`）
- 発火の規則・`WHEN`・発火順序・再帰上限・トリガ経由の台帳の整合は **#1721 の ADR が定める**。
  本書は重複して書かず、D5 の再入の不変条件と D8 の台帳の単位を #1721 への前提として列挙する
- `DO [LANGUAGE plpgsql] <本体> [USING OPERATION_ID '<id>']`: 無名ブロックを 1 回実行し、
  結果を返さない（CommandComplete のタグは `DO`）。`LANGUAGE sql` は `0A000`、未知の言語は
  `42704`（`ErrorClass::UndefinedObject`）。`Statement` への `Do` variant 追加は公開 enum の変更

### D10. エラー分類（SQLSTATE は PostgreSQL 17 の公開挙動を出典とする）

| 事象 | SQLSTATE | 方針 |
| ---- | -------- | ---- |
| `SELECT ... INTO STRICT` が 0 行 | `P0002` | 新 variant（例 `NoDataFound`）。既存 `RowNotFound`（`P0002`・HTTP 404）と `wire_code` を共有するため `error_format::SHARED_WIRE_CODES` に `P0002` を追加し、HTTP 射影を 404 に揃える |
| `SELECT ... INTO STRICT` が 2 行以上 | `P0003` | 新 variant |
| `ELSE` の無い `CASE` 文がどの分岐にも一致しない | `20000` | 新 variant |
| `RETURN` に達せず関数の終わりに到達（`void`・集合返却・トリガ関数を除く） | `2F005` | 新 variant |
| `NOT NULL` 変数への NULL 代入 | `22004` | 既存 `NullValueNotAllowed` |
| 変数名と列名の衝突 | `42702` | 既存 `AmbiguousColumn` |
| サブセットの範囲外 | `0A000` | 既存 `FeatureNotSupported` |
| 上限超過 | `54000`／`54001` | 既存 `PayloadTooLarge`／`StatementTooComplex` |

- 文言は PostgreSQL と同じ固定の英語とし、変数の値・行の内容・テナント情報を含めない
- `ErrorClass` と `Statement` は公開 enum のため variant 追加は破壊的変更
  （`error-enum-non-exhaustive-policy.md`）。#2224（`Statement`）・#2225（`ErrorClass`）・
  #2228（`Do`／トリガ関数）は `feat(engine)!:` と `BREAKING CHANGE:` フッタで出す

## 構文表

出典は #2222 の目標欄（public）と PostgreSQL 17 ドキュメントの PL/pgSQL 章の節立て。
細目で迷う要素は #2224 で PostgreSQL 17 ドキュメントに基づき確定する（SQL-68 ポインタ）。
Issue #2222 の目標欄は `RAISE` を挙げつつ #2227 へ配置しているため、**初版は `RAISE` も `0A000`**、
Issue #2227 で受理する。

| 区分 | 受理（初版） | 範囲外（`0A000`） |
| ---- | ------------ | ----------------- |
| ブロック | ラベル付きブロック、`DECLARE`、`BEGIN ... END`、`NULL;` | `EXCEPTION` 節（#2227）、`#variable_conflict` 等のコンパイラ指示 |
| 宣言 | 型・`CONSTANT`・`NOT NULL`・`:=`／`DEFAULT` の初期値・`%TYPE`・`%ROWTYPE`・`RECORD` | `ALIAS FOR`・カーソル変数（`refcursor`）・多相型 |
| 代入 | `:=`・`=` | — |
| 条件 | `IF`／`ELSIF`／`ELSE`、`CASE`（単純形・検索形） | — |
| ループ | `LOOP`・`WHILE`・`FOR`（整数範囲〔`REVERSE`・`BY`〕・問い合わせ結果の行）、`EXIT`／`CONTINUE`（ラベル・`WHEN`） | `FOREACH`、カーソルの `FOR`（`FOR ... IN EXECUTE` は #2224 で確定） |
| 戻り値 | `RETURN`、`RETURN NEXT`／`RETURN QUERY`（集合返却は #2220 の基盤が前提。未整備なら `0A000`） | `OUT`／`INOUT` 引数（引数モードは PR #2773 で拒否済み） |
| SQL | `SELECT ... INTO [STRICT]`、`PERFORM`、`INSERT`／`UPDATE`／`DELETE`（`RETURNING ... INTO`）、`EXECUTE ... [INTO] [USING]`、`FOUND`、`GET DIAGNOSTICS ... = ROW_COUNT` | DDL・トランザクション制御（`COMMIT`／`ROLLBACK`）・`SET`／`RESET`・`CALL`・`COPY`、`ROW_COUNT` 以外の診断項目・`GET STACKED DIAGNOSTICS` |
| 通知・例外 | — | `RAISE`（全水準）・`ASSERT`（#2227。SQL-70 ポインタ） |
| カーソル | — | `OPEN`／`FETCH`／`MOVE`／`CLOSE` |

範囲外の構文は `0A000` で拒否し、別の意味へ読み替えない（fail-closed）。

## セキュリティ観点（OWASP Top 10）

| 観点 | 対応 |
| ---- | ---- |
| インジェクション | 変数・`USING` の値は型付きの値から組み立てたトークンで置換し、SQL テキストへ連結しない（D3・D6）。`EXECUTE` の文字列は許可リストを必ず通す。本体内の `USING OPERATION_ID`・`PARTITIONED` は `0A000` |
| アクセス制御の不備 | 全文を呼び出し元の `PolicyContext`・同じ Tx で実行し、テナント切替 API を作らない。`SECURITY DEFINER` は `0A000`。関数定義はテナント単位 |
| 不安全な設計（DoS） | 文数・ループ回数・入れ子深さ・呼び出し深さ・本体長・1 文あたり変数数の上限（D7）、キャンセル（`57014`）、`--max-result-rows` 配下の実体化。超過は文全体を拒否 |
| 情報漏えい | エラー文言は固定の英語で、値・行・他テナントの存在を含めない |
| 整合性（台帳） | 台帳は外側の文の単位で 1 エントリ。再送は `23505` で本体を再実行しない。`--require-operation-id` で `23502`。単一の書き込み Tx で crash に対し原子的 |
| 脆弱な依存 | 外部クレートを追加しない（`dependency-policy.md`） |

## 既知の制限

- テキスト型変数を文字列リテラルとして置換するため、PostgreSQL が型付きパラメータとして
  `42883` にする比較が暗黙変換で通りうる。#2226 で PostgreSQL と照合し、差が出る形は
  `CAST(... AS <型>)` で包む（許可リストが CAST を受理する位置）か表に記録する
- 計画キャッシュなし（実行のたびに再解析）。最適化は別 Issue
- 呼び出し位置の初版範囲（D5 の表の `0A000` 行）。拡大は別 Issue で追跡する
- 追跡対象（起票は別途承認が必要）: 呼び出し位置の拡大、`SELECT` への `USING OPERATION_ID`、
  `FOR ... IN EXECUTE`

## 後続 Issue への引き継ぎ

| Issue | 引き継ぐ前提 |
| ----- | ------------ |
| #2216 | ドル引用トークン・`statement_splitter` の `$` の扱い・content_hash への反映 |
| #2217／#2218／#2220 | `LANGUAGE plpgsql` の受理（それまで `0A000`）・揮発性属性・呼び出し位置の基盤・関数呼び出し深さカウンタの共有・集合返却 |
| #2224 | 構文表の網羅テスト・範囲外の `0A000`・本体長と入れ子深さ・D3 の定義時ダミー検証 |
| #2225 | 変数環境・D7 の上限と CLI・キャンセル・D10 の新 variant（`!`） |
| #2226 | D3 の値置換・D5 の経路決定と再入の不変条件・D6・D8・`42702` 検出・既知の制限の照合 |
| #2228 | D9 の内部 API と `DO` |
| #2229 | 越境マトリクス（RLS-10 ポインタ）・上限超過の fail-closed・再送で副作用が重複しないこと・crash-test |
| #2227 | `RAISE`・`EXCEPTION` 節（SAVEPOINT #1941 の後） |
| #1721／#1720 | 発火規則・発火位置が D5 の不変条件を満たすこと・トリガ経由の台帳 |
