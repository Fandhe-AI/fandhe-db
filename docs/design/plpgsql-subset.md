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
  `unsafe` が要るため採らない（`coding-rust.md`）。所有データだけを持つ thread-local の
  RAII ガード（`sql::const_expr` の遅延モード・`sql::colref_sink` の記録区間が先例。借用・ポインタを
  載せず、Drop で必ず元へ戻る）は可。キャンセル確認だけは既存の `engine::interrupt::check()` を使う

### D2. 本体の受け取りと定義時・呼び出し時の検証

- 本体はドル引用または文字列リテラル 1 個として届く。**#2216 が前提**。現行の
  `sql::statement_splitter` は通常状態の `$` で分割しない（`SplitOutcome::Single`）ため、
  複数文メッセージで `$$` を正しく扱うには #2216 側で splitter の更新も要る
- `LANGUAGE plpgsql` の受理と属性（揮発性・`STRICT`）は #2217、保存は PR #2775 に揃える。
  AST は保存せず、原文を保存して呼び出し時に再解析する
- 定義時: PL/pgSQL の構文解析（範囲外は `0A000`、構文誤りは `42601`、上限超過は D7）と
  埋め込み SQL の定義時分類（D3。SQL 構文解析器の記録による変数参照の分類と構文検証）。表・列の名前解決は**呼び出し時**
  （PostgreSQL と同じ late binding。存在しない表は呼び出し時の `42P01`）。
  参照先への依存記録（`2BP01`）は行わない
- 初版は埋め込み文の計画キャッシュを持たず、実行のたびに許可リスト解析を通す
  （最適化の入口は `sql-parse-bind-cache.md`）

### D3. 変数の参照と値の受け渡し（インジェクションを構造的に塞ぐ）

- 埋め込み SQL の変数参照の分類は **SQL 構文解析器が行う**。PL/pgSQL 層は SQL 文法を再推定しない
  （識別子位置のマスクもダミー置換も持たない）。解析器（`sql::allowlist`）が列参照（裸／修飾）または
  リテラル専用位置として読んだ識別子のうち、PL/pgSQL のスコープで解決できるものだけが変数参照になる。
  列リスト・別名・型名・CTE 名・`SET` の代入先・関数名は解析器が列参照として読まないため、同名の変数が
  あっても変数にならない。記録の受け口は `sql::colref_sink`（所有データだけを持つ thread-local の
  RAII ガード。ガードが無いとき既存の挙動は変わらない）
- 分類は定義時に、正規化（行数制限句・名前付き `WINDOW`。冪等）後のトークン列に対して、変数を置換しない
  まま構造解析（カタログを引かない。SQL-68 ポインタ）へ通して行う。サブクエリ・CTE 本体は解析器が取り込んだ
  範囲ごとにルート上のスライスで再帰的に解析する。本体中の `$n` はスコープ内に見える合成識別子として扱う。
  結果（位置・役割・変数）を AST（`SqlExpr::var_refs`・`EmbeddedSql::var_refs`）へ保存する。役割は解析器が
  確定した `Value`／`WhereHead`／`SelectItemBare`／`OrderKey`／`GroupKey`／`LiteralSlot`
- 解析器が分類できない形は `0A000`（fail-closed）: ルートへ解決できない列参照（範囲外）・変数に解決できない
  リテラル専用位置・変数を受理しないリテラル専用位置（`ESCAPE`・型付きリテラル・`SIMILAR TO` 等）・記録されない
  `$n`・先送りのエラーで解析が途中で終わり未記録の変数名が残る文。**列しか受理しない位置**（窓の
  `PARTITION BY`／`ORDER BY`・`DISTINCT`・`RETURNING`・`IN (SELECT)` の左辺・集合演算の枝等）に変数が来た場合は、
  値を置いても構文として成立しないため、定義時に `0A000`（記録した位置へ型付きのリテラルを置いた再解析で判定）
- 呼び出し時（#2226）は、定義時に記録した位置と役割を使って、変数の**型付きの値から**、字句解析器が同じ値を
  リテラルで書いた SQL から生成するのと同一のトークン列を組み立てて置換し、通常の許可リスト解析へ渡す。
  `sql::params` の型付き置換（`substitute_values_typed`・`ParamLiteralKind`）と同じ構成原理を共有する。
  値は SQL テキストにも字句解析器にも戻らず、値経由のインジェクション経路は存在しない。役割別の置換
  （`OrderKey`／`GroupKey` は整数リテラルが序数参照へ化けないよう包むか拒否、`SelectItemBare` は出力列名を
  保つ別名付与）は #2226 の範囲
- `sql::params::validate_param_positions` の受理位置制限（WIRE-12）は**適用しない**。
  その制限は拡張プロトコルの Parse 時点で値と型が未知であることに由来し、PL/pgSQL では
  どちらも既知。適用すると一般的な本体が定義時に `42601` になり PostgreSQL との差が
  大きくなる。リテラルを置けない位置（識別子位置など）の変数は許可リストの `42601`
- 構造解析で起きる表・列・型だけの失敗（`42P01`・`42703` 等）は定義時に拒否せず呼び出し時の検証へ回す
  （呼び出し時に完全に検証するため書き込み経路は fail-open にならない）。定義時に拒否するのは構文誤り
  （`42601`）・範囲外（`0A000`）・上限超過（`54000`／`54001`）
- 左辺が式の `IS [NOT] NULL`・`[NOT] BETWEEN`・`[NOT] IN (...)` を SQL 側が受理する（SQL-24 のポインタ。
  `sql::allowlist`。括弧付きの左辺・演算を含む左辺の変数がこれで分類できる）
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
- 集合返却（`RETURN NEXT`／繰り返し実行される `RETURN QUERY`）の蓄積結果にも上限を置く。
  個々の問い合わせが `--max-result-rows` 以下でも、ループでの追加を重ねて全体が上限を
  超えないよう、関数呼び出し単位（入れ子呼び出しは合算）で累積行数と保持バイト数を持ち、
  **行を追加する前に**検査する（追加後検査にしない。無制限確保の禁止）。行数の上限は
  `--max-result-rows`、バイト数の上限は既存の結果実体化のバイト上限と同値とする
  （新規 CLI は加えない。最終確定は #2225）。超過は `54000` で文全体を取り消す
  （部分結果を返さない・fail-closed）
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
| #2224 | 構文表の網羅テスト・範囲外の `0A000`・本体長と入れ子深さ・D3 の定義時分類（記録した位置と役割を AST に保存） |
| #2225 | 変数環境・D7 の上限と CLI・キャンセル・D10 の新 variant（`!`） |
| #2226 | D3 の値置換（役割別の置換、`42702` の判定規則）・D5 の経路決定と再入の不変条件・D6・D8・`42702` 検出・既知の制限の照合 |
| #2228 | D9 の内部 API と `DO` |
| #2229 | 越境マトリクス（RLS-10 ポインタ）・上限超過の fail-closed・再送で副作用が重複しないこと・crash-test |
| #2227 | `RAISE`・`EXCEPTION` 節（SAVEPOINT #1941 の後） |
| #1721／#1720 | 発火規則・発火位置が D5 の不変条件を満たすこと・トリガ経由の台帳 |

## Issue #2224 追記: 構文解析器の実装方針

ポインタ: SQL-68。実装は `crates/engine/src/plpgsql/`（`lexer`・`ast`・`parser`・`embedded`）と
`crates/engine/src/plpgsql_limits.rs`（ハード上限）、`core/plpgsql_validate.rs`（定義時検証）。

- 方針: PL/pgSQL 独自の式パーサーは持たない。制御構造だけを再帰下降で解析し、式・埋め込み SQL は
  括弧の深さ 0 の終端（`;`・`THEN`・`LOOP`・`..`・`BY`・`INTO`・`USING`・`WHEN` 等）で原文の範囲を
  切り出して `sql::lexer::tokenize_with_params` に通す。本体をそのまま SQL 字句解析器へ通せない
  （`:=`・`1..10`・`<<l>>`・`%TYPE`）ため、span 付きのスキャナを別に置く。文字列・引用識別子・
  コメントの区切りは SQL 字句解析器の関数を共有する
- 結線しない単体モジュール（`pub(crate)`）。`LANGUAGE plpgsql` の受理（#2217）と実行（#2225）が
  結線するまで呼び出し元はテストだけ。公開 enum（`Statement`・`ErrorClass`・`SqlSurfaceError`）は
  変えないため非 BREAKING。**D10 の「#2224（`Statement`）」は撤回する**（`Do` variant は #2228 で入れる）
- 定義時の分類（D3）: `parse_body` は純粋な構文解析で、各文・式にその時点のスコープ（`ScopeView`）を
  保存する。`plpgsql::embedded::validate_embedded` が、`EmbeddedSqlChecker`（core が
  `sql::embedded_unit::parse_unit` へ委譲して実装）経由で、記録区間（`sql::colref_sink`）の下の構造解析に
  変数を置換しないトークン列を通し、記録された位置のうちスコープで解決できる名前を `VarSite`（位置・役割・
  変数）として AST へ書き込む。あわせて、正規化後のトークン列・文種・書き込みの有無（データ変更 CTE を含む
  `WITH` は解析木の `SessionWith::DataModifying` から）も確定する。構造解析はカタログを引かず、定義時に
  伝播する wire_code は `42601`／`0A000`／`54000`／`54001` だけ。許可リストは比較・論理演算を `WHERE` の中でしか
  受理しないため、条件式（`IF`・`WHILE`・検索形 `CASE`・`EXIT/CONTINUE WHEN`）は
  `SELECT 1 FROM <仮の表> WHERE <cond>`、値の式は `SELECT <expr>` の形で解析する
- 例外: データ変更 CTE を含む最上位の `WITH` 文は、書き込みの実行形（副文の `RETURNING` 公開列・主クエリの
  形）が構造解析だけでは判定できないため、core の実装がカタログ付きの解析（`ParsedSql::DataModifyingCte`）へ
  回す。表が無く判定できなければ書き込みの有無を決められないので `0A000`（fail-closed）
- 変数参照の分類: PL/pgSQL 側に識別子位置の判定は無い。解析器が列参照と確定しなかった位置（関数名・
  `FROM`／`JOIN` の表名・別名・`INSERT` の列リスト・`UPDATE ... SET` の代入先・型名・CTE 名）は変数にならない。
  分類結果（位置・役割）は #2226 の呼び出し時の値置換が使う
- 範囲外の追加確定: `FOR ... IN EXECUTE`・`RETURN QUERY EXECUTE`・カーソル `FOR` は `0A000`。
  本体内の `USING OPERATION_ID`・深さ 0 の `PARTITIONED` は `0A000`（D8）。`RAISE`・`ASSERT` は
  `0A000` のまま（#2227 で受理）
- 本体内のドル引用（例: `EXECUTE $q$...$q$`）は、#2216 のマージまでは SQL 字句解析器が `$` を拒否するため
  `42601`。スキャナはドル引用 1 個を 1 トークンとして読み飛ばす（中の `;` で文を分けない）
- 変数名・ラベルが PL/pgSQL の予約語（`BEGIN`・`END`・`IF`・`LOOP` 等）と同じ場合は `42601`
  （PostgreSQL より厳しい。fail-closed）。識別子の比較は ASCII の大小を無視し、引用付きと引用無しの
  同名を区別しない（既知の差）
- `FOR rec IN <query> LOOP` の単独かつ未宣言の対象は、ループ範囲に暗黙のレコード変数として宣言する
  （PostgreSQL と同じ）。整数 `FOR` の変数は暗黙の整数型で、ループ範囲のスコープを持つ
- `$n` の番号が引数の個数を超える場合は `42P02`。`RETURN` の値の有無の検査は `ReturnShape` で行う
  （`void`／集合返却で値付き `RETURN` は `42804`、集合返却以外の `RETURN NEXT`／`QUERY` は `42601`。
  SQLSTATE は PostgreSQL 17 の挙動の記憶に基づくため、#2225 で実測照合する）
- 上限: `plpgsql_limits.rs` のハード定数（本体長は格納上限と同値・入れ子 32・トークン数・1 文あたり
  変数数は `sql::params::MAX_PARAMS`）。`ParseOptions::with_limits` はハード上限へ飽和し、`PlpgsqlLimits`・
  CLI は #2225 で入れる。入れ子の深さ超過は `54001`、それ以外は `54000`
- エラー文言は固定の英語で、本体の断片・変数名を含めない

### 変数名の制限（Issue #2224 レビュー対応）

- **規則**: 変数・関数パラメータ・ループ変数の名前に使えないのは、PL/pgSQL の予約語（`BEGIN`・`END`・`IF`・
  `LOOP` 等。`42601`）、SQL 字句解析が `Token::Keyword` にする語（`select`・`from`・`where`・`and`・`order`・
  `by`・`limit`。記録できないため。`42601`）、SQL の予約語（PostgreSQL 公開ドキュメント付録「SQL Key Words」の
  「reserved」列の 78 語。`true`・`false`・`null` を含む。`0A000`）だけ（比較は大小無視。エラー文言に名前は
  含めない）。**引用なしの宣言だけ**が対象で、引用付き `"..."` は PostgreSQL と同じく予約語でも宣言でき、
  引用付きで参照できる（`pl_scanner.c` は引用付きの語を未予約キーワード表と照合せず `T_WORD` にする）。
  例外（fail-closed。引用付きでも `0A000`）: SQL 字句解析が引用付きでも拒否する Keyword 語
  （`select`・`from`・`where`・`and`・`order`・`by`・`limit`）と、引用付きを専用トークンにして変数参照へ
  解決できない語（`true`・`false`・`null`・`array`・`default`・SQL 値関数）。引数名は引用の有無が不明な
  ため引用なし相当で検査する。既知の差: 埋め込み SQL 内の引用なしの語（例 `loop`）は引用の有無を保持
  しないため、引用付きで宣言した同名の変数に束縛される（PostgreSQL は構文エラー）
- 理由: 変数参照は SQL 構文解析器の記録（列参照と確定した位置）で解決するため、SQL 側の文脈キーワードや
  文の語と同名の変数（`set`・`values`・`is`・`filter`・`over` 等）も、構文上の語を横取りしない。位置の推定を
  持たないので、語ごとの拒否一覧とその漏れ検出テストは持たない。PostgreSQL でも予約語は宣言できない
  （`src/include/parser/kwlist.h` の `RESERVED_KEYWORD`）ため、この制限は PostgreSQL の挙動の範囲内
- 定数は `crates/engine/src/plpgsql/parser.rs` の `RESERVED`・`SQL_KEYWORD_NAMES`・`SQL_RESERVED`
- ラベル名は対象外（識別子位置に現れず `lbl.name` の修飾子としてだけ使われるため曖昧さが無い）
- `FOUND` は引数より後に暗黙宣言される（PostgreSQL の `do_compile` が引数を名前空間へ入れた後に `found` を
  作り、`plpgsql_ns_lookup` が新しい項目から検索する順序と一致）。`found` という名前の引数は `FOUND` に隠れる
- `WITH` 文の種別とデータ変更 CTE の有無は解析木から取る。CTE 名が `"update"` 等でも誤分類しない。
  形を読み解けない `WITH`（許可リストが受理しない主文を含む）は `42601` で拒否する

- 宣言の既定値は、その変数を宣言する前のスコープで解決する（PostgreSQL と同じ。`DECLARE x int := x`
  の右辺の `x` は外側の `x`）。宣言後の本体では内側の変数が外側を隠す（D4）
- 引用付き識別子: SQL 字句解析器は `"into"` も `into` も `Ident("into")` にするため、`INTO` 句・禁止句・
  先頭の語の判定（`split_into`・`reject_forbidden_clauses`・`head_kind`）は並走する引用ビット
  （`sql::lexer::tokenize_with_params_quoted`）で引用付きを常に識別子として扱う。変数参照の分類は
  解析器の記録を使うので、引用ビットには依存しない
- 式の定義時の分類は、値の式なら `SELECT <expr>`、条件式なら `SELECT 1 FROM <仮の表> WHERE <expr>` を順に試し、
  最初に受理された形の記録を採る。すべて拒否なら最初の `42601` を返す（変数がリテラル専用位置にあって
  拒否された場合だけ `0A000`）。許可リストが比較・論理演算を `WHERE` 内でだけ受理し、`FROM` の無い `WHERE` は
  単一の述語しか受理しないため仮の表を置く（構造解析では表は常に存在する扱い）

## Issue #2225 追記: インタプリタと実行上限（SQL-68 ポインタ）

D4（値の表現）・D7（上限と CLI）・D10（`ErrorClass` の新分類）の最終判断。実装は
`crates/engine/src/plpgsql/{host,interp,value}.rs`・`plpgsql_limits.rs`。

### 境界: `PlHost` トレイト

- インタプリタ（`interp.rs`）は制御フロー・変数環境・代入時の型変換・`INTO`／`STRICT` の行数規則・
  `FOUND`／`ROW_COUNT`・集合返却の蓄積・実行予算だけを持つ。式の評価・埋め込み SQL の実行・
  `%TYPE` 等の型解決は `&mut dyn PlHost`（`host.rs`）へ委ね、SQL 文字列を組み立てない
  （D1・D3。単純形 `CASE` の比較も `values_equal` をホストに頼む）。thread-local へ借用を載せる方式は採らない
- core の `PlHost` 実装（変数値のトークン置換・同じ Tx／RLS での実行・FOUND／ROW_COUNT の実測値・
  D5 の事前決定・D6 の動的 SQL・D8 の台帳）は #2226。#2225 の時点ではテスト専用ホストだけが実装する。
  メソッドの形は #2226 の要件で変えうる（予算を引数で渡すのは、式の中の関数呼び出しが同じ予算へ合算するため）
- `ReturnShape::Trigger` を通常の関数として呼ぶと `0A000`（入口は #2228）

### 値の表現（D4 の最終判断）

- スカラーは `PlScalar { cell: Cell, ty: ColumnType }`（NULL も型付き）。レコードは
  `PlRecord { fields: Option<Vec<(String, PlScalar)>> }` で、`None`（未代入）のフィールド参照は `55000`
  （`SqlSurfaceError::RecordNotAssigned`）
- 宣言型 `Named` は `CastType::from_sql_type_name` → `result_column_type` で解く。`%TYPE` 等は
  `resolve_type`（呼び出し時）
- 代入時 CAST は `plpgsql/value.rs`。変換の実体は既存の `sql::cast_value::apply_cast` で、ここは `Cell` と
  式評価値の往復だけ。代入規則（`cast_type_assignable`）に無い組は、PostgreSQL の PL/pgSQL と同じく
  「文字列への出力変換 → 入力変換」（I/O 変換）で通し、不正な文字列は `22P02`／`22003` になる

### 実行予算と上限（D7 の最終判断）

- `ExecBudget` はトップレベルの 1 回の起動ごとに 1 個作り、入れ子の呼び出しへ `&mut` で渡して合算する
  （文数・ループ反復・呼び出し深さ・集合返却の累積行数とバイト数）。検査はすべて**実行前・追加前**
- 文数・反復は `54000`、呼び出し深さは `54001`。上限ちょうどの文数・反復までは実行でき、次で超過
- 集合返却の行数上限は `--max-result-rows`、バイト数上限は `arena::MAX_ARENA_TOTAL_BYTES`
  （`MAX_RETURNING_RESULT_BYTES` と同値。新しい CLI は作らない）
- 毎文・毎反復で `interrupt::check()`（`57014`）。インタプリタは commit しないため確認点が commit の後ろに
  回らない。超過・エラー時は `Err` だけを返し部分結果を返さない
- CLI は 5 本（`--max-plpgsql-statements` 1〜100,000・`--max-plpgsql-loop-iterations` 1〜1,000,000・
  `--max-plpgsql-nesting-depth` 1〜32・`--max-plpgsql-body-bytes` 1〜65,536・`--max-function-call-depth` 1〜32）。
  既定値はハード上限と同値で、下げる方向だけ。実装既定値であり spec 由来の数値ではない。
  `--max-function-call-depth` の名前はここで確定する（#2218 は `EngineCore::plpgsql_limits().max_call_depth()` を読む）。
  入れ子・本体長は構文段（`ParseOptions::with_limits`）へ、文数・反復・呼び出し深さは `ExecBudget` へ渡す

### エラー分類（D10 の最終判断。破壊的変更）

| 事象 | SQLSTATE | `ErrorClass` |
| ---- | -------- | ------------ |
| `INTO STRICT` が 0 行 | `P0002` | `NoDataFound`（`RowNotFound` と `wire_code` を共有。`from_wire_code` は `RowNotFound`） |
| `INTO STRICT` が 2 行以上。DML の `RETURNING INTO` は STRICT 無しでも 2 行以上 | `P0003` | `TooManyRows` |
| `ELSE` の無い `CASE` 文が不一致 | `20000` | `CaseNotFound` |
| スカラー関数が `RETURN` に達せず終わりに到達 | `2F005` | `FunctionExecutedNoReturnStatement` |

- HTTP 射影は `NoDataFound` が 404、他は 400。NoSQL 表層から PL/pgSQL 関数を呼ぶ op は無く到達不能
- 既存の分類を再利用するもの: NOT NULL 変数への NULL・整数 `FOR` の境界が NULL・`EXECUTE` の文字列が NULL は
  `22004`、`BY` が 0 以下は `22023`、未代入レコードの参照は `55000`
- `ErrorClass` と `SqlSurfaceError`（公開 enum）への variant 追加のため BREAKING

### 制御構文の意味（PostgreSQL 17 の公開された挙動に合わせる）

- ブロックは入るたびに `DECLARE` の既定値を宣言順に評価する（ループ内のブロックは反復ごとに再初期化）
- `IF`／`WHILE`／`EXIT WHEN` の条件が NULL は偽。単純形 `CASE` はセレクタを 1 回だけ評価する
- 整数 `FOR` は境界と `BY` を 1 回だけ int4 として評価し、`REVERSE` は書いた順（大..小）で、
  小..大の順に書くと 0 回。次の値が int4 を溢れる時点で終了する（オーバーフローしない）
- `FOR` は終了時に FOUND を「1 回以上反復したか」にする。`EXECUTE` は FOUND を変えず ROW_COUNT だけ変える
- `INTO`（STRICT 無し）は 0 行で NULL、2 行以上で先頭行。対象が複数のスカラーなら列順で、列が足りなければ NULL
- `RETURN NEXT`／`RETURN QUERY` は蓄積し、`RETURN` で打ち切る。`SETOF` は終わりに到達しても `2F005` にならない

### 既知の差（security.md P0 の優先。後続で縮められる）

- エラー文言は固定の英語で、PostgreSQL が含める変数名・値・行の内容は含めない
- 代入時 CAST の対象は数値 6 型・`boolean`・文字列 3 型。同じ型の組は素通しだが、それ以外の型の組
  （日時・配列・`json` 等の相互変換）は `0A000`。`bigint` の 2^53 超は式評価の制約で型をまたぐ変換が拒否される
- `RETURN QUERY` の行型と戻り値の型の厳密な照合は行わない（1 列の場合だけ戻り値の型へ代入変換する）

### 後続への引き継ぎ

- #2226: `PlHost` の core 実装。変数値のトークン置換、同じ Tx・同じ RLS での実行、FOUND／ROW_COUNT の
  実測値の検証、`resolve_type`（`%TYPE`／`%ROWTYPE` のカタログ解決）
- #2217: `LANGUAGE plpgsql` の受理と定義時の `parse_body` 呼び出し（`PlpgsqlLimits` の入れ子・本体長を
  `ParseOptions::with_limits` へ渡す）
- #2218: `--max-function-call-depth` と `EngineCore::plpgsql_limits()` を共有
- #2227: `RAISE`・`EXCEPTION`、#2228: `DO`・トリガ関数、#2229: 越境・crash 検証
