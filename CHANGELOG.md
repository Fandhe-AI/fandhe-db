# Changelog

`crates/engine`（`fandhe-db-engine`）・`crates/wire-server`
（`fandhe-db-wire-server`）の公開 API に影響する変更を記録する。
両クレートは同一バージョンで同期して公開する（`crates/wire-server/Cargo.toml`
の `engine` path 依存が完全固定バージョンで参照するため）。

## 0.2.0

### Added

- `SELECT` の行ロック句（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`、`OF <名前>[, ...]`、`NOWAIT`／`SKIP LOCKED`）を構文解釈する（Issue #1957・SQL-38）。解析結果は新しい `sql::row_lock`（`LockingSelect`・`RowLocking`・`RowLockClause`・`LockStrength`・`LockWaitPolicy`）の型で `ParsedSql::LockingSelect` として実行計画へ渡す。`DISTINCT`・`GROUP BY`・`HAVING`・集計・ウィンドウ関数・集合演算・外部結合で NULL を補う側との併用は `0A000`、`OF` が FROM の参照名に無ければ `42P01`。ロックの取得は #2230、`SKIP LOCKED` の実行は #2231 の担当で、それまで実行入口は `0A000` の暫定ゲートで拒否する。詳細は [explicit-transaction.md](docs/design/explicit-transaction.md)。
  挙動変更: 受理する形と併用違反は `42601` から `0A000`（`OF` の解決失敗は `42P01`）になった（構文誤りは従来どおり `42601`）。公開 enum `ParsedSql` に `LockingSelect` を追加した（BREAKING CHANGE）。
- `BEGIN ISOLATION LEVEL {SERIALIZABLE|REPEATABLE READ|READ COMMITTED|READ UNCOMMITTED}` を受理する（Issue #1938・SQL-38 (1)。要求レベルは記録のみで実行は不変）。`sql::transaction::RequestedIsolationLevel`・`SessionTransaction::begin_with_modes`（`isolation_level` 引数）・`SessionTransaction::isolation_level` を追加し、公開構造体 `ValidatedTxnControl` に `isolation_level` フィールドを追加した（BREAKING CHANGE）。
- `40001`（serialization_failure）の `wire_code` と失敗状態への遷移の入口を追加した（Issue #2398・TASK-333・ERR-7・SQL-38）。`ErrorClass::SerializationFailure`・`SqlSurfaceError::SerializationFailure`（固定文言・表名やテナントを含まない）を追加し、HTTP 射影は 409。`SessionTransaction` に crate 内部の入口を置き、`40001` の後は `ROLLBACK` 以外が `25P02`、`ROLLBACK` で復帰する。判定と送出の結線は #2405（現状は到達しない）。
  公開 enum `ErrorClass`・`SqlSurfaceError` に variant を追加した（BREAKING CHANGE）。
- `CREATE [OR REPLACE] FUNCTION` が SQL 標準形の関数本体 `RETURN <式>`・`BEGIN ATOMIC <文>; ... END` を受理する（Issue #2219・SQL-67 (2)。`LANGUAGE` 省略は sql）。標準形は定義時に名前を解決して参照先への依存辺を記録し、参照先の DROP・改名・列変更を `2BP01` で止める（CASCADE でも。連鎖削除・改名への追随は未対応）。簡易クエリの複数文メッセージは本体内の `;` で分割しない。詳細は [plpgsql-subset.md](docs/design/plpgsql-subset.md)。
  公開 enum `CreateFunctionBody` に `SqlStandard` を追加した（BREAKING CHANGE）。
- WASM UDF サンドボックスの資源上限を追加した（Issue #1927・EXT-6／EXT-9）。`wasm_udf::SandboxLimits` に `fuel` を追加（BREAKING: 公開フィールドの追加）、`SandboxLimits::clamped`・`HARD_MAX_*`・`validate_wasm_*`・`WasmLimitOutOfRange`・`TrapKind::FuelExhausted`・`EngineCore::with_wasm_limits`／`wasm_limits` を追加した。`wasm-udf` ビルドでは epoch ティッカーで実行時間上限が効き、実行時の上限超過は `54000`（従来は `22000`）。起動フラグ `--wasm-max-memory-bytes`・`--wasm-max-call-duration-ms`・`--wasm-max-fuel`（下げる方向だけ）を追加。
- WASM UDF をモジュールのバイト列から登録して SQL の結果列・`WHERE` から呼べるようにした（Issue #1926・EXT-9）。`SessionState::register_wasm_udf_from_module`・`sql::udf_call::define_wasm_function_from_module`・`wasm_udf::WasmUdfSignature`／`WasmSqlType`／`check_signature_against_abi` を追加した（追加のみ）。実体のコンパイルは feature `wasm-udf` 有効時に限り、無効ビルドでは `0A000`。
  挙動変更: `CREATE FUNCTION ... LANGUAGE wasm` の拒否が `42704` から `0A000` になった（SQL による WASM 関数の定義は未対応）。
- `CREATE FUNCTION` の `LANGUAGE sql|plpgsql` 句と関数属性（揮発性・`STRICT`・`PARALLEL`・`COST`）を受理する（Issue #2217・SQL-67 (3)）。本体は文字列リテラル（`$$...$$` 可）で原文のまま保存し、plpgsql は定義時に検証する。`RETURNS trigger` は plpgsql・引数なしだけ受理する。非 IMMUTABLE の `sql`／`plpgsql` 関数を使う式索引・部分索引は `42P17`。`sql`／`plpgsql` 関数の呼び出しは #2218 まで `42883`。
  公開 `Statement::CreateFunction` に `language`・`attributes`・`returns_trigger` を追加し、`body` を `CreateFunctionBody` に置き換えた（BREAKING CHANGE）。
- トリガ関数の内部 API と `DO` ブロックを追加した（Issue #2228・SQL-68）。`DO [LANGUAGE plpgsql] '<本体>'` を受理し、暗黙・明示トランザクションの上で本体を実行する（CommandComplete タグ `DO`。本体のエラーで副作用なし。`USING OPERATION_ID` は `0A000`）。トリガ関数は `NEW`／`OLD`／`TG_*` を使え、`RETURN NEW`・`RETURN NULL` の意味は PostgreSQL に合わせる（発火への結線は #1720）。
  公開 enum `SqlOutcome` に `Do`、`ParsedSql` に `Do` を追加した（BREAKING CHANGE）。
- DDL 権限のあるセッションで、型付き引数と `RETURNS` つきの `CREATE [OR REPLACE] FUNCTION` を UDF カタログへ永続化する（Issue #2798・SQL-66）。従来の `0A000` 拒否は無くなり、再起動後のロードで型が復元される（blob v1 は不変）。公開 API の変更はない。詳細は [udf-catalog.md](docs/design/udf-catalog.md)。
- トリガの入れ子（再帰）の発火と深さ上限を追加した（Issue #1727・TABLE-45）。トリガ本体の書き込み先のトリガも発火し、入れ子の深さが上限（既定 16）を超えると `54001` で文全体を拒否する。起動フラグ `--max-trigger-depth` と `EngineCore::with_trigger_limits`／`engine::trigger_limits::TriggerLimits` を追加（下げる方向だけ）。挙動変更: 書き込み先に INSERT トリガがある場合の `0A000` は入れ子の発火に変わる（未結線の経路は従来どおり `0A000`）。詳細は [trigger-depth-limit-cli.md](docs/design/trigger-depth-limit-cli.md)。
- PL/pgSQL インタプリタと実行上限を追加した（Issue #2225・SQL-68）。AST を辿る木構造インタプリタ（変数環境・制御構文・`INTO`／`STRICT`・`FOUND`／`ROW_COUNT`・集合返却）と、式・SQL を委ねる境界トレイトを crate 内部に置いた（`LANGUAGE plpgsql` の受理と core 実装は #2217／#2226 のため、SQL からは従来どおり利用できない）。起動フラグ `--max-plpgsql-statements`・`--max-plpgsql-loop-iterations`・`--max-plpgsql-nesting-depth`・`--max-plpgsql-body-bytes`・`--max-function-call-depth` と `EngineCore::with_plpgsql_limits`／`engine::plpgsql_limits::PlpgsqlLimits` を追加（既定はハード上限と同値で下げる方向だけ）。公開 enum `ErrorClass` に `NoDataFound`（`P0002`。`RowNotFound` と `wire_code` を共有）・`TooManyRows`（`P0003`）・`CaseNotFound`（`20000`）・`FunctionExecutedNoReturnStatement`（`2F005`）、`SqlSurfaceError` に同名の variant と `RecordNotAssigned`（`55000`）を追加した（BREAKING）。詳細は [plpgsql-subset.md](docs/design/plpgsql-subset.md)。
- UPDATE／DELETE の BEFORE・AFTER ROW トリガを発火させる（Issue #1725・TABLE-45）。SQL の `UPDATE`・`DELETE`（id 指定形・述語形・サブクエリ形）が OLD・NEW を渡して発火し（`UPDATE ... FROM`／`DELETE ... USING` は内部実行器のみ対応で、SQL からは従来どおり `0A000`）、`UPDATE OF <列>` は `SET` に書かれた列だけで発火する。分割実行 DML・RETURNING・UPSERT・NoSQL・参照アクションが及ぶ子表のトリガは `0A000`。トリガの無い表の挙動は不変。公開 API の変更はない。詳細は [trigger-update-delete.md](docs/design/trigger-update-delete.md)。
- AFTER ROW INSERT トリガを発火させる（Issue #1724・TABLE-45）。SQL の `INSERT ... VALUES`・`INSERT ... SELECT` が、全行の書き込み・FK 検査の後に、書いた行ごと・トリガ名のバイト順で AFTER ROW トリガを発火する。戻り値は無視し、トリガ内の書き込みは外側の文と同じ txn・呼び出しテナントの文脈で行う。AFTER の失敗は行・監査行・台帳を含めて文全体を取り消す。トリガが書いた表の遅延 FK も検査する。文トリガ・`WHEN` 付き・UPSERT・RETURNING 等の未結線経路は `0A000` のまま。公開 API の変更はない。
- BEFORE ROW INSERT トリガを発火させる（Issue #1723・TABLE-45）。SQL の `INSERT ... VALUES`・`INSERT ... SELECT` が、書き込みテナント自身の BEFORE ROW トリガを名前のバイト順に連鎖実行し、NEW の書き換えと `NULL` 返却による行のスキップに対応する。台帳の照合は書き換え前の値で行い、制約違反は従来の SQLSTATE のまま。トリガ関数の実体（#2228）が未整備のため、呼び出し口の既定は固定文言の `0A000`。UPSERT・RETURNING・NoSQL・ファイル形・生の行挿入・ビュー経由・データ変更 CTE と、AFTER ROW・文トリガは INSERT トリガのある表で `0A000`。公開 enum `TenantWriteError` に `TriggerFailed` を追加した（BREAKING）。
- `DROP FUNCTION [IF EXISTS] ... [CASCADE | RESTRICT]` と `CREATE OR REPLACE FUNCTION` を追加した（Issue #1917・SQL-66）。置換は同じ内部 ID のまま本体を差し替え、引数名・戻り値型の変更は `42P13`、引数の型の並びが違う同名は `42723`、呼び出しの循環を作る置換は `0A000`。`CASCADE` は依存トリガを同じ write txn で消し、`RESTRICT`（既定）は `2BP01`。公開 enum `Statement::CreateFunction` にフィールド `or_replace`、`SqlSurfaceError` に variant `AmbiguousFunction`（`42725`）、`ErrorClass` に `AmbiguousFunction` を追加し、`DropFunctionOutcome` はフィールドを持つ（`skipped()`）ため外部から `{}` で構築できなくなった（BREAKING）。
- `CREATE [OR REPLACE] TRIGGER`／`DROP TRIGGER [IF EXISTS]` と TRIGGER カタログを追加した（Issue #1722・TABLE-45）。定義は表・テナントごとに永続化され、表の DROP・改名、`DROP FUNCTION`（`2BP01`）、`DROP SCHEMA ... CASCADE` と同じ書き込みで連動する。`INSTEAD OF`・`REFERENCING`・`WHEN`・`CONSTRAINT TRIGGER` は `0A000`。`RETURNS trigger` 関数（#2228）が未整備のため、`EXECUTE FUNCTION` は関数が無ければ `42883`、あれば `42P17` で、現状は作成に成功しない。詳細は [trigger-catalog.md](docs/design/trigger-catalog.md)。
  挙動変更: `UPDATE OF` が参照する列の `DROP COLUMN` は `2BP01`、`RENAME COLUMN`・`ALTER COLUMN TYPE` は `0A000`（トリガが無ければ従来どおり）。公開 enum `SqlOutcome` に `CreateTrigger`／`DropTrigger`、`ParsedSql` に同名の variant を追加した（BREAKING）。
- 文字列関数 `left`／`right`／`reverse`／`initcap` を追加した（Issue #1902・SQL-65）。文字単位は Unicode スカラー値で、4 関数とも strict（NULL 引数で NULL）。`left`／`right` の長さは負で「除く文字数」を表し、`initcap` は英数字の連続を 1 語として扱う（ロケール非依存。PostgreSQL との既知の差は `string_fn.rs` のモジュール doc に記載）。
  挙動変更: 4 名は組み込みの予約名になり、同名の `CREATE FUNCTION` は `42723` になる。
- `ln`／`log`／`exp`／`sign` を追加する（Issue #1907・SQL-65）。`log(x)` は底 10、`log(b, x)` は底 b の対数。`ln`・`log` の 0 と負は `2201E`、`log(1, x)` は `22012`、`exp` のあふれ・アンダーフローは `22003`。NUMERIC 型の引数は従来どおり `42804`。詳細は [numeric-scalar-functions.md](docs/design/numeric-scalar-functions.md) の「Issue #1907 追記」節。
- 数値関数 `trunc(x)`／`trunc(x, n)` を追加した（Issue #1908・SQL-65）。0 方向への切り捨てで、2 引数形は小数点以下 `n` 桁（負の `n` は整数部の桁）で打ち切る。10 進数字列で処理するため桁上げ・二進誤差はない。`n` が非整数は `22000`、`i32` の範囲外は `22003`。
  挙動変更: `trunc` が組み込み関数の予約名になり、同名の `CREATE FUNCTION` は `42723`。公開 enum `BuiltinFn` に variant `Trunc1`／`Trunc2` を追加した（BREAKING）。
- 文字列関数 `lpad`／`rpad`／`split_part` を追加した（Issue #1904・SQL-65）。いずれも strict で、長さ・位置は文字単位。`lpad`／`rpad` は第 3 引数の省略時に半角空白で埋め、`split_part` は負の位置を末尾から数え、位置 0 は `22023`。結果長が上限（4 MiB）を超えると `54000`。
  挙動変更: 3 名は組み込みの予約名になり、同名の `CREATE FUNCTION` は `42723` で拒否される。
- 文字列関数 `md5(text)` を追加した（Issue #1905・SQL-65）。UTF-8 バイト列の MD5 を 32 文字の小文字 16 進で返す strict 関数（NULL は NULL）で、依存追加なしの自作実装。`md5(bytea)` は bytea が関数引数に使えない既存規則どおり `0A000`（PostgreSQL との既知の差）。
  挙動変更: `md5` が組み込み関数の予約名になり、同名の `CREATE FUNCTION` は `42723`。公開 enum `BuiltinFn` に variant `Md5` を追加した（BREAKING）。
- `substring(s FROM n [FOR m])`／`substring(s FOR m [FROM n])` とカンマ形 `substring(s, n[, m])` を受理する（Issue #1903・SQL-65）。意味は `substr` と同じ文字単位の窓計算で、`substring` は組み込み関数名として予約される。正規表現形（第 2 引数が文字列・`SIMILAR ... ESCAPE`）は SQL-55 の管轄のため `0A000`。`position(needle IN haystack)` は Issue #919 の実装を SQL-65 の回帰テストで固定した。公開 API の変更はない（非 BREAKING）。
- 派生表を JOIN／カンマ区切りの入力にでき、本文に派生表を入れ子にできるようにした（Issue #1867・SQL-62）。派生表と実表・派生表どうしの JOIN は合成 CTE を `WITH`＋JOIN 経路の slot として評価し、内側は参照者の可視行だけを読む。入れ子の深さはサブクエリ深さ上限（既定 4）を超えると `54000`。
  挙動変更: 従来 `0A000`／`42601` だった JOIN 入力・複数件・入れ子の派生表が実行される。別名なしの派生表の JOIN 入力・集計本文の JOIN 入力は `0A000`、`EXPLAIN` は `42601` のまま。公開 API の変更はない（非 BREAKING）。詳細は [cte.md](docs/design/cte.md) の「Issue #1867 追記」節。
- FROM 直後の派生表 `(SELECT ...) [[AS] 別名 [(列名, ...)]]` を実行できるようにした（Issue #1866・SQL-62）。合成 CTE として既存の単一表 `WITH` 経路でインライン展開または具体化して評価し、外側の `WHERE`・投影・`ORDER BY` が派生列を参照でき、内側は参照者の可視行だけを読む。
  挙動変更: 従来 `0A000` だった FROM 直後の単一の派生表が実行される。JOIN／カンマ区切りの入力・複数件は引き続き `0A000`、派生表を含む文の `$n` は `42601`。公開 API の変更はない（非 BREAKING）。詳細は [cte.md](docs/design/cte.md) の「Issue #1866 追記」節。
- 組み込み関数の引数個数を関数ごとの表で検査し、1 回の呼び出しを 100 個まで認める（Issue #1910・SQL-65）。可変長の `concat` は 100 個まで動く。100 個を超えると `54023`、未登録の関数名と個数不一致は `42883`。
  挙動変更: 33〜100 個の呼び出しは、従来の `54000` に代えて実行されるか、個数が合わなければ `42883` になる。`COALESCE`・`PASSING` は 32 個・`54000` のまま。公開 API は定数 `MAX_FUNCTION_CALL_ARGS` の追加のみ（非 BREAKING）。
- 再帰 CTE の `UNION`（重複除去）版を評価できるようにした（Issue #1872・SQL-35）。既出行を捨てるため循環グラフでも止まり、捨てた重複は累積行数の上限に数えない。`JSON` 列の `UNION` と `SEARCH` 句との併用は固定 `0A000`。
  挙動変更: 従来 `0A000` だった `UNION` の再帰が評価される。公開 API の変更はない（非 BREAKING）。詳細は [recursive-cte.md](docs/design/recursive-cte.md) の「Issue #1872 追記」節。
- CTE 本文の集計・`GROUP BY`・`SELECT DISTINCT`・ウィンドウ関数を受け付ける（Issue #1863・SQL-62）。インライン展開では外側の `WHERE` が集計・窓の前に掛かって誤結果になるため、常に具体化して評価し、集計値・窓の値は参照者の可視行だけから計算される。`LIMIT` なしの本文は暗黙上限で、可視行（グループ）が `--max-result-rows` を超えると `54000`。
  挙動変更: 従来 `42601` で拒否していた形が受理される。他の CTE が読む連鎖・集計 CTE の JOIN 入力・ビュー本文や cursor などインライン専用の文脈での参照は固定 `0A000`。公開 enum は変えない（非 BREAKING）。詳細は [cte.md](docs/design/cte.md) の「Issue #1863 追記」節。
- CTE 本文の `ORDER BY`・`LIMIT`・`OFFSET` を受け付ける（Issue #1862・SQL-62）。本文に `LIMIT` を持つ CTE はインライン展開では `LIMIT` が落ちるため、常に具体化して評価し（単一表の主クエリは評価後射影形、JOIN は入力 slot）、外側の `WHERE` は本文の `LIMIT` の後に適用される。`LIMIT` なしの本文は暗黙上限で、可視行が `--max-result-rows` を超えると `54000`。
  挙動変更: 従来 `42601` で拒否していた形が受理される。他の CTE が読む連鎖・ビュー本文や cursor などインライン専用の文脈での参照は固定 `0A000`。公開 enum は変えない（非 BREAKING）。詳細は [cte.md](docs/design/cte.md) の「Issue #1862 追記」節。
- データ変更 CTE の 1 文内の可視性と失敗時の動作を PostgreSQL に揃える（Issue #1877・SQL-63）。副文を 2 つ以上・
  主クエリから参照されない副文・`RETURNING` のない副文で実行でき、主クエリが実表・ビュー・SELECT 本文 CTE を読む形も実行できる。
  全副文と主クエリは文の開始時点の状態を読み、互いの変更は `RETURNING` を通したときだけ見える。同じ行を複数の副文が
  変更する場合は `WITH` の順に実行し、後の副文は先の副文が書いた行に触れない。副文または主クエリのどこかでエラーなら
  文全体が取り消され、行数上限は副文ごとに判定する。挙動変更: 従来 `0A000` だった複数副文・主クエリが実表を読む形・
  副文を読まない形が実行される。`ON CONFLICT` 付き INSERT 副文が他の副文と同じ表を対象にする形、副文が 2 つ以上で対象表が
  `FOREIGN KEY` の参照アクションの参照先になる形、JOIN の主クエリは引き続き `0A000`。あわせて、`--require-operation-id` で
  `WHERE` にサブクエリを持つ副文が `23502` になる不具合を直した。公開 API の変更はない（非 BREAKING）。
  詳細は [data-modifying-cte.md](docs/design/data-modifying-cte.md) の「Issue #1877」節。
- 再帰 CTE の反復回数・累積行数の上限を起動時設定にする（Issue #1871・SQL-35・SQL-46。段階 B）。
  wire-server に `--max-recursive-cte-iterations <1-1000>`・`--max-recursive-cte-rows <1-100000>` を追加し、
  `EngineCore::with_recursive_cte_limits` へ注入する。ハード上限は既定値（反復 1,000 回・累積 100,000 行）と
  同じで、下げる方向だけ設定できる。超過は `54000`。範囲外・非数値・値欠落・重複指定は起動エラー。
  累積行数は `--max-result-rows` との小さい方が実効上限。公開 API は追加のみ（`engine::recursive_cte_limits`）。
- `WITH RECURSIVE` の `CYCLE 列[, ...] SET 印列 [TO 値 DEFAULT 既定値] USING 経路列` 句を評価する（Issue #2214・SQL-35）。
  行ごとに経路（循環列の行値の `record[]`）を持ち、新しい行の行値が親の経路に含まれれば印を `TO` 値（省略時は `true`）にして出力し、その行からは再帰しない。
  `UNION ALL` でも循環グラフで止まる。印列・経路列は順序列（あれば）の後ろに付く。反復回数・累積行数・共有バイト予算の上限は従来どおりで、超過は部分結果なしの `54000`。
  挙動変更: 従来 `0A000` だった `CYCLE` 句が評価される。`USING` のない形・符号付きの `TO`／`DEFAULT` 定数は `42601`。公開 API の変更はない（非 BREAKING）。詳細は [recursive-cte.md](docs/design/recursive-cte.md) の「Issue #2214 追記」節。
- `WITH RECURSIVE` の `SEARCH DEPTH FIRST／BREADTH FIRST BY 列[, ...] SET 順序列` 句を評価する（Issue #2213・SQL-35。段階 C）。
  深さ優先は根から当該行までの BY 列の行値の配列（`record[]`）、幅優先は `(深さ, BY 列の値...)`（`record`）を順序列として結果列の末尾へ足し、
  主クエリの `ORDER BY 順序列 [DESC]` で PostgreSQL 17 と同じ順序になる。wire は OID 2249／2287 のテキスト形式（バイナリ結果形式の要求は `0A000`）、
  HTTP の SQL 応答は入れ子の JSON 配列。BY 列が列名に無い・SET 名の衝突・非再帰定義への指定は `42601`、BY 列の重複は `42701`、
  アンカー項が集合演算・`CYCLE` 句の併用は `0A000`。深さ優先の経路は確保前に共有バイト予算へ計上し、超過は部分結果なしの `54000`。
  挙動変更: `SEARCH` 句つきの再帰 CTE は従来 `0A000` だったが評価される。詳細は [recursive-cte.md](docs/design/recursive-cte.md)。
  BREAKING CHANGE: 公開 enum `sql::exec::Cell` に variant（`Record`／`RecordArray`）、`sql::exec::ColumnMeta` に variant（`Record`）を追加したため、
  網羅的な `match` は更新が要る。
- `WITH RECURSIVE` の反復評価器を追加する（Issue #1870・SQL-35。段階 B）。アンカー項の結果を最初の作業表とし、
  「前回の反復で増えた行だけ」を作業表として再帰項（単一関係、または基底表との JOIN）を再実行し、作業表が空に
  なったら終了する。出力順は生成順。階層走査と数列が評価できる。暫定の固定上限（反復 1,000 回・累積 100,000 行。
  超過は部分結果を返さず `54000`）を置く（設定化は #1871）。挙動変更: 構造検証を通過した `UNION ALL` の再帰形は
  従来 `0A000` だったが評価される。自己参照の無い `WITH RECURSIVE` は通常 CTE として実行される。`UNION`（重複除去）・
  複数定義・セッション最上位以外の文は `0A000` のまま。再帰項の整数型がアンカー型と異なる形は `42804`、作業表で評価できない型（`TIME`・`TIMESTAMPTZ`・`INTERVAL` 等）は `0A000`。詳細は [recursive-cte.md](docs/design/recursive-cte.md)。
  BREAKING CHANGE: 公開 enum `Statement` に variant（`RecursiveCte`）を追加したため、網羅的な `match` は更新が要る。
- CTE を JOIN の入力（リレーション）として受け付ける（Issue #1861・SQL-62）。セッションの最上位 SELECT で `WITH` の主クエリが JOIN の形のとき、
  FROM の CTE を評価済みの行の側入力にし、複数回参照しても共有する CTE の本文は文内で 1 回だけ評価する（指定なしは参照数 2 以上で共有）。
  従来 `42601` で拒否していた形が受理される挙動変更で、ICU 照合の列を読む CTE・重複列名・型なし列・導出できない層で先行 CTE を読む本文・CTE が公開した行 `id` への集計は `0A000`、
  本文の可視行が `--max-result-rows` を超えると `54000`。観測用に `EngineCore::materialized_cte_eval_stats`（`VectorCore` 外の固有 API）を追加。
  公開 enum は変えない（非 BREAKING）。詳細は [cte.md](docs/design/cte.md) の「Issue #1861 追記」節。
- 相関スカラーサブクエリを式の内側で使えるようにする（Issue #1848・SQL-61）。算術・比較・`COALESCE`・`CASE`・関数引数・`CAST` の
  被演算子として、投影の式項目・WHERE の式述語・スカラー `ORDER BY` の式キーに置ける（`(SELECT max(v) ...) + 1`・
  `COALESCE((SELECT ...), 0)`・`v > (SELECT ...) * 2`）。該当なしは NULL が式の中で伝播し（集計形の `count` は 0）、結果列の型は
  内側の結果列の型どおり。非相関の埋め込みは従来どおり `42601`、NUMERIC の結果の算術は `0A000`。
  挙動変更: `GROUP BY` のキー内のサブクエリを `42601` から `0A000` へ変えた（非相関にも効く）。
  BREAKING CHANGE: 公開 enum `sql::udf_call::BoundExpr` に variant `CorrelatedScalar` を追加したため、網羅的な `match` は更新が要る。
  詳細は [sql-subquery.md](docs/design/sql-subquery.md) の「Issue #1848 追記」節。
- `WITH RECURSIVE` の構文を受理し再帰構造を検証する（Issue #1869・SQL-35。段階 A）。アンカー項と再帰項を
  `UNION [ALL]` で結んだ形の自己参照の位置・回数を検証し、不正な再帰は `42P19`、未対応の形と構造検証を通過した
  文の実行（反復評価は #1870）は `0A000` で拒否する。従来 `42601` だった挙動変更。詳細は [recursive-cte.md](docs/design/recursive-cte.md)。
  BREAKING CHANGE: 公開 enum `ErrorClass` と `SqlSurfaceError` に variant（`InvalidRecursion`）を追加したため、
  網羅的な `match` は更新が要る。
  サブクエリ内部も構文検証してから構造判定へ進み（構文 `42601` → `42P19` → `0A000`）、引用符付きの `"join"` 列・別名を結合演算子と取り違えない。
- CTE の具体化経路を追加する（Issue #1860・SQL-62）。明示 `MATERIALIZED` の CTE を主クエリが直接参照すると、本文全体を評価した結果行集合を外側が読む。
  挙動変更: 本文の可視行数が `--max-result-rows` を超えると、外側の `LIMIT` に関わらず `54000` で拒否される。公開 API の破壊的変更なし。詳細は [cte.md](docs/design/cte.md)。
- 非集計形の相関スカラーサブクエリ（`(SELECT y FROM b WHERE b.k = a.k)`。`y` は単一の裸の列）を、WHERE 位置と投影位置で
  外側の行ごとの再評価により実行する（Issue #1847・SQL-61）。一致 0 件は NULL（WHERE では偽）、1 件はその値、
  2 件以上は `21000`。内側を 1 回だけ実体化し、テナント境界は外側と同じ `PolicyContext` に従う。
  従来 `42601` で拒否していた形が実行可能になる挙動変更。公開 enum は変えない（非 BREAKING）。
  wire-server に `--max-correlated-reevaluations <1-100000>`（再評価回数の上限を下げる opt-in。超過は `54000`）を追加し、
  `engine::correlated_limits`・`EngineCore::with_correlated_limits` を公開する。詳細は
  [sql-subquery.md](docs/design/sql-subquery.md) の「Issue #1847 追記」節。
- データ変更 CTE を書き込み経路で実行する（Issue #1876・SQL-63）。最上位の文で `WITH d AS (DELETE|UPDATE|INSERT ... RETURNING ...) SELECT ... FROM d`
  を 1 つの書き込みトランザクションで実行し、`RETURNING` 行を主クエリが読む。文末の `USING OPERATION_ID` は文全体を台帳 1 エントリで記録する。
  従来固定 `0A000` だった形が実行されるようになる挙動変更（複数のデータ変更 CTE・実表を読む主クエリ・一時表対象は引き続き `0A000`）。
  公開 enum `ParsedSql` に variant（`DataModifyingCte`）を追加（`#[non_exhaustive]`）。詳細は [data-modifying-cte.md](docs/design/data-modifying-cte.md)。
- CTE 本文のデータ変更文（`WITH x AS (DELETE ... RETURNING ...)` 等）の構文を受理し、本文の解析と `RETURNING` の公開列の確定までを行う（Issue #1875・SQL-63）。
  実行は未対応のため、検証を通過した文も `0A000` で拒否する（従来は `42601`）。書き込みは発生しない。詳細は [data-modifying-cte.md](docs/design/data-modifying-cte.md)。
- CTE の `MATERIALIZED`／`NOT MATERIALIZED` を受理する（Issue #1858・SQL-62 (3)）。評価方式の指定として読み捨て、結果は変わらない。
  従来 `42601` で拒否していた構文が受理される挙動変更。詳細は [cte.md](docs/design/cte.md)。
- CTE の列名リスト `WITH x(a, b) AS (...)` を受理する（Issue #1857・SQL-62 (2)）。出力列へ先頭から順に名前を付け、短いリストは残りを元の名前のまま受理する。
  出力列より長いリストだけ `42P10`（参照されない CTE も同様）。別名付きビューの CTE 内参照も受理する。詳細は [cte.md](docs/design/cte.md)。
  BREAKING CHANGE: 公開 enum `SqlSurfaceError` に variant（`CteColumnListTooLong`）を追加したため、網羅的な `match` は更新が要る。
- `GROUP BY` の `ROLLUP`・`CUBE`・`GROUPING SETS` を構文解析し、グルーピングセットの列へ展開する（Issue #1821・SQL-59）。
  展開前に上限（展開後 4,096 セット・`CUBE` 12 要素）を検査し、超過は `54001`・`54011` で拒否する。詳細は [grouping-sets.md](docs/design/grouping-sets.md)。
  BREAKING CHANGE: 公開 enum `ErrorClass` と `SqlSurfaceError` に variant（`StatementTooComplex`・`TooManyColumns`）を
  追加したため、網羅的な `match` は更新が要る。
- `GROUPING(<キー>, ...)` 関数を追加する（Issue #1823・SQL-59）。引数 i はそのキーが現在のセットに含まれないとき 1、
  最初の引数が最上位ビットの int4 を返す（通常の `GROUP BY` は常に 0）。引数は 31 個までで、超過は `54023`。
  BREAKING CHANGE: 公開 enum `AggregateFunc` に `Grouping`、`ErrorClass`・`SqlSurfaceError` に `TooManyArguments` を
  追加したため網羅的な `match` は更新が要る。`grouping` は集計関数名として予約され、同名の `CREATE FUNCTION` は拒否される。
- `ROLLUP`・`CUBE`・`GROUPING SETS` をセットごとに集約して結果を合成する（Issue #1822・SQL-59）。セットに含まれない
  キー列は NULL で出力し、空のセットは入力 0 行でも総計 1 行を返す。グループ数などの上限は全セット合計で検査する。
  従来 `0A000` で拒否していた構文が実行可能になる挙動変更。ビュー経由の外側集計は未対応のまま拒否する。
- 実行中の文の協調的中断を追加した（Issue #1798・SQL-34・WIRE-11）。公開 API は `engine::interrupt`
  （`InterruptScope`・`InterruptGuard`・`InterruptReason`）と `ErrorClass::QueryCanceled`（`57014`）。
  wire-server は実行中のクライアント切断を検出して走査を止める（切断済みの接続の資源占有を防ぐ）。
  CancelRequest の受け付けは含まない。HTTP への射影は 503。詳細は
  [statement-interrupt.md](docs/design/statement-interrupt.md)。
  BREAKING CHANGE: 公開 enum `ErrorClass` と `SqlSurfaceError` に variant（`QueryCanceled`）を追加したため、
  網羅的な `match` は更新が要る。
- `GROUPING SETS`・`ROLLUP`・`CUBE` と `FILTER`・集計 `DISTINCT`・`ORDER BY`・`LIMIT` 等の併用と上限境界（結果行数・グループ数・`COUNT(DISTINCT)` 予算の全セット合計）をテストで固定（Issue #1824・SQL-59。挙動変更なし・公開 API 変更なし）。
- `--max-result-rows` の受け入れ条件（既定 10,000 の境界・上げた側とハード上限 100,000 の境界・超過時に DataRow を送らない）をテストで固定し、README と設計記録を `LIMIT` 省略時の上限の現状に整合（Issue #2211・SQL-34。挙動変更なし）。
- jsonpath の `like_regex` を評価する（Issue #2677・SQL-55・TABLE-25）。`jsonb_path_*`・`@?`／`@@`・`JSON_EXISTS` 等が
  PG 17 と同じ結果を返す。フラグ `i`・`s`・`m`・`q` を受理し、`x`（`q` なし）は `0A000`、正規表現の構文誤りは `2201B`、
  上限超過は `54000`。1 つの jsonpath が持てる `like_regex` は 16 個まで（超過は `54000`）。
  `parse_jsonpath_with_regex_limits` と `regex_match::RegexSyntaxOptions` を追加。
  詳細は [regex-matcher.md](docs/design/regex-matcher.md)。
- `INSERT ... SELECT`・CTAS・MATERIALIZED VIEW の入力行数上限の算出を共有化した（Issue #2212・SQL-34）。設定が
  ハード上限（100,000）のとき、LIMIT を注入しない形は 99,999 行ではなく 100,000 行まで通る。matview の評価後射影形
  （`GROUP BY` 等）の本文は `--max-result-rows` を既定値より上げると上限も上がる。超過は従来どおり書き込み前に `54000`。
- `LIMIT` 省略・`LIMIT ALL` の結果行数上限超過（`54000`）を、`cap + 1` 件目の確定時点で走査・結合・集約を中断して返す（Issue #1797・SQL-34）。
  広域取得・ウィンドウ・HAVING なしの `GROUP BY`／`SELECT DISTINCT`・残余述語なしの JOIN が対象で、上限を超える行は保持しない。
  エラーコードと文言は変わらない。
- 行を返す SELECT で `LIMIT` の省略・`LIMIT ALL`・`OFFSET` 単独を受理する（Issue #1796・SQL-34）。広域取得・
  スカラー `ORDER BY`・ウィンドウ・JOIN・`GROUP BY`／`SELECT DISTINCT`・集合演算全体・ビュー参照・CTE の主クエリが対象。
  可視結果が結果行数上限（`--max-result-rows`・既定 10,000）を超えたら黙って切り詰めず `54000` で文全体を拒否する。
  従来これらの形を `42601` で拒否していたクライアントの挙動が変わる。距離 `ORDER BY`・`USING PLAN`・
  サブクエリ内側・ビュー／CTAS／matview 本文の省略は引き続き `42601`。
  詳細は [limit-omitted-result-cap.md](docs/design/limit-omitted-result-cap.md)。
- `WHERE <列または式> [NOT] SIMILAR TO '<pattern>' [ESCAPE '<c>']` を受理して評価する（Issue #1788・SQL-55）。
  SQL 正規表現を ARE へ変換し、`~` と同じマッチャで全体一致を判定する（PG 17 の実測表で検証）。
  `ESCAPE` は句なし（`\`）・指定文字・`''`（無効）、2 文字以上は `22025`。不正パターンは `2201B`、
  上限超過は `54000`。NULL は三値論理。索引は使わず常に残余述語。
  詳細は [regex-matcher.md](docs/design/regex-matcher.md)。
  BREAKING CHANGE: 公開 enum `Expr::RegexMatch` に必須フィールド `raw_pattern_len: Option<usize>` を追加したため、
  構築コードと網羅的な `match` は更新が要る。既存の `~` 系の構築では `raw_pattern_len: None` を指定する。
- `WHERE <列または式> ~ | ~* | !~ | !~* '<pattern>'`（正規表現マッチ演算子）を受理して評価する（Issue #1787・SQL-55）。
  パターンは文字列リテラルか `NULL`（拡張クエリでは `<列> ~ $n`。`$n` は常に text）。ASCII のみ大小無視（`~*`）、
  NULL は三値論理、不正パターンは `2201B`、非対応構文は `0A000`、上限超過は `54000`。索引は使わず常に残余述語。
  詳細は [regex-matcher.md](docs/design/regex-matcher.md)。
- `FETCH {FIRST|NEXT} [n] {ROW|ROWS} ONLY` と `OFFSET m {ROW|ROWS}` を受理する（Issue #1793・SQL-56）。トークン段で
  `LIMIT n`／`OFFSET m` へ正規化するため範囲・上限・エラー契約は `LIMIT` と同一。`FETCH ... WITH TIES` は
  単一表のスカラー `ORDER BY` 広域取得と `GROUP BY` 集計で受理し（同値行も結果行数の上限に数え、超過は `54000`）、
  それ以外の経路は `0A000`／`42601`。`LIMIT` との併記・`PERCENT` は `42601`。
  詳細は [order-by-having-expressions.md](docs/design/order-by-having-expressions.md)。
- `ORDER BY <key> [ASC|DESC] NULLS FIRST|LAST` を広域取得・ウィンドウ・`GROUP BY` 集計・`SELECT DISTINCT`・JOIN・
  ビューの各経路で受理する（Issue #1791・SQL-56）。指定なしは従来どおり（ASC は末尾・DESC は先頭）。
  不正な指定と kNN 距離順への併用は `42601`。詳細は [order-by-having-expressions.md](docs/design/order-by-having-expressions.md)。
- `WHERE <col> [NOT] LIKE|ILIKE '<pattern>' ESCAPE '<c>'` を受理する（Issue #1779・SQL-54）。構文層で既定の `\` 形へ
  正規化するため評価・索引・台帳の扱いは ESCAPE 無しと同じ。`ESCAPE ''` はエスケープ無効。
  詳細は [like-wildcard-patterns.md](docs/design/like-wildcard-patterns.md)。
- `WHERE <col> [NOT] ILIKE '<pattern>'` を受理して評価する（Issue #1778・SQL-54）。ASCII 英字のみを畳み込み、
  非 ASCII はバイト完全一致で比べる。ICU 照合の列は `0A000`。索引は使わない。
  詳細は [like-wildcard-patterns.md](docs/design/like-wildcard-patterns.md)。
- 複数文メッセージの暗黙トランザクション内の `COPY` を受理する（Issue #1773・WIRE-20）。COPY の文で CopyIn／CopyOut へ入り、
  CopyDone の後に続きの文へ進む（従来は `0A000`）。最後の文が COPY のときは応答の前に commit する。
  詳細は [wire-copy-protocol.md](docs/design/wire-copy-protocol.md)。
- 複数文メッセージ中の `COPY` の原子性とエラー伝播の検証を網羅した（Issue #1774・WIRE-20。挙動変更なし）。
  失敗要因と位置の組み合わせで先行文の変更が残らないこと、明示トランザクションの `Failed`／`25P02` の整合を固定。
- `COPY ... FROM STDIN` が `HEADER`（`MATCH` を含む）・`DELIMITER`・`NULL`・`QUOTE`・`ESCAPE` を適用する（Issue #1764・WIRE-22）。
  text 形式では `\<区切り文字>` を区切り文字そのものとして受理する（既定のタブ区切りでも `\<TAB>` がリテラルのタブになる）。
  詳細は [wire-copy-protocol.md](docs/design/wire-copy-protocol.md)。
- `COPY (...) TO STDOUT` が `HEADER`・`DELIMITER`・`NULL`・`QUOTE`・`ESCAPE` を適用する（Issue #1765・WIRE-22）。
  ヘッダ行は `COPY n` の n に含めない。非既定のオプションを `0A000` で拒否していたゲートは撤去した。
- テーブル形 `COPY <table> [(<col>, ...)] TO STDOUT`（Issue #1771・WIRE-22 (5)・SQL-34）: `validate_copy` が受理し、
  `--max-result-rows` 超過は行を送る前に `54000` で拒否する。ビューは `42809`・不在表は `42P01`・重複列は `42701`。
  詳細は [wire-copy-protocol.md](docs/design/wire-copy-protocol.md)。
- opt-in の `--require-where-for-dml`（Issue #2210・SQL-36）: `EngineCore::with_require_where_for_dml(bool)` を追加し、
  有効時は `WHERE` 省略の SQL `UPDATE`／`DELETE` を変更開始前に `42601` で拒否する（既定は無効で挙動不変）。
  wire-server に値なしスイッチ `--require-where-for-dml` を追加した。明示した述語・NoSQL 表層・`TRUNCATE` は対象外。
  詳細は [require-where-for-dml-cli.md](docs/design/require-where-for-dml-cli.md)。

- `greatest(v1, ..., vn)`／`least(v1, ..., vn)` を追加した（Issue #1909・SQL-65）。NULL 引数は無視し、全引数が NULL の行は NULL。
  引数の型は `CASE`／`COALESCE` と同じ規則で統一し（統一できなければ `42804`）、引数は 100 個まで（超過は `54023`）。
  数値・TEXT（バイト順）・DATE・TIMESTAMP・TIME・TIMESTAMPTZ を比較でき、それ以外の型は `0A000`（vector は `42883`）。
  挙動変更: 2 名が組み込みの予約名になり、同名の `CREATE FUNCTION` は `42723`。

### Changed

- 拡張クエリの Execute の `max_rows` 分割で、完了タグの件数を累計からその回の件数に変え、`max_rows` ちょうどで
  残りが尽きた回も `PortalSuspended` で終えるようにした（Issue #1798・WIRE-11。PostgreSQL 17 と同じ）。
  BREAKING CHANGE: 分割送出の最終 Execute の完了タグ（例: 5 行を `max_rows=2` で 3 回に分けた最後が `SELECT 5` から
  `SELECT 1`）と、行がちょうど尽きた回の応答（`CommandComplete` から `PortalSuspended`）が変わる。
  `FETCH` は utility 文のため対象外で、完了タグは分割しても FETCH 文全体の取得件数（`FETCH 5`）のまま。
- 単一表の具体化 CTE・評価後射影形ビューの外側で、`i64::MAX` を超える行 id を `22003` にせず評価するようにした
  （Issue #2744・SQL-62）。外側の `MIN`／`MAX`／`SUM`／`AVG(id)` と `GROUP BY id` の結果型は numeric になる。

### Breaking Changes

- `sql::udf_call::BuiltinFn`（Issue #1902・SQL-65）: variant `Left`／`Right`／`Reverse`／`Initcap` を追加した。網羅 `match` する外部コードは腕を足す。
- `sql::udf_call::BoundExpr`（Issue #1909・SQL-65）: variant `MinMax { op, args }` と公開型 `MinMaxOp` を追加した。
  網羅 `match` する外部コードは腕を足す。
- `ln`／`log` の定義域外を `2201E` で拒否する（Issue #1907・SQL-65）。公開型の変更として `SqlSurfaceError::InvalidArgumentForLogarithm`・
  `ErrorClass::InvalidArgumentForLogarithm`・`ArenaError::InvalidArgumentForLogarithm` を追加した（HTTP 射影は 400）。
- `sql::udf_call::BuiltinFn`（Issue #1904・SQL-65）: variant を追加した（`Lpad2`／`Lpad3`／`Rpad2`／`Rpad3`／`SplitPart`）。
  網羅 `match` する外部コードは腕を足す。
- `substring`／`substr` の負の長さを `22011`（substring_error）で拒否する（Issue #1903・SQL-65）。従来は `22000`。公開型の変更として
  `SqlSurfaceError::SubstringError`・`ErrorClass::SubstringError`・`ArenaError::SubstringError` を追加した（HTTP 射影は 400）。
- jsonpath の `like_regex` 配線（Issue #2677）に伴う公開型の変更。`PathNode::LikeRegex` に `matcher: LikeRegexMatcher` を追加、
  `JsonPathParseError` に `Regex(RegexError)`・`TooManyRegex`、`JsonPathEvalError` に `RegexInputTooLong` を追加し、
  `RegexNotSupported` は `x` フラグ専用になった。従来 `like_regex` を一律 `0A000` で拒否していた挙動が変わる。
- `sql::lexer::Token`・`sql::udf_call::Expr`・`sql::udf_call::BoundExpr`・`sql::allowlist::SqlSurfaceError`（Issue #1787・SQL-55）:
  variant を追加した（`Token::RegexMatch`／`RegexMatchCi`／`RegexNotMatch`／`RegexNotMatchCi`、`Expr::RegexMatch`、
  `BoundExpr::RegexMatch`、`SqlSurfaceError::Regex`）。網羅 `match` する外部コードは腕を足す
  （`wire_code()`／`classify()` 経由で扱うコードは変更不要）。
- `error_format::ErrorClass::InvalidEscapeSequence`（`22025`）・`sql::allowlist::SqlSurfaceError::InvalidEscapeSequence`
  （Issue #1779・SQL-54）: variant を追加した（網羅 `match` するコードは腕を足す。HTTP は 400）。
  あわせて `LIKE` パターン末尾の単独 `\` のエラーを `22000` から `22025` へ変えた（`ILIKE`・NoSQL `like` も同じ。HTTP は 400 のまま）。
- `sql::allowlist::SqlSurfaceError`（Issue #1792・SQL-56 (2)）: variant `OrderByPositionNotInSelectList { position: i64 }`
  （`42P10`。`ORDER BY` の序数が出力列の範囲外）を追加した。列挙型を網羅的に `match` する外部コードは
  コンパイルできなくなる。移行: 腕を追加する（`wire_code()`／`classify()` 経由で扱うコードは変更不要）。
- `sql::udf_call::BoundExpr`（Issue #1782・SQL-54）: variant `Like { lhs, pattern, negated }` を追加した
  （式左辺の `LIKE`／`ILIKE`。網羅 `match` するコードは腕を足す）。あわせて構文層が式左辺の
  `WherePredicate::Prefix` を生成するようになった（`lower(name) LIKE 'a%'` が受理される。左辺が文字列型でなければ `42883`）。
- `sql::allowlist::WherePredicate::Prefix`（Issue #1781・SQL-54）: フィールド `column: String` を `lhs: Expr`
  （左辺の式ノード）へ置き換えた。構文層が生成する左辺は引き続き裸の列だけで、挙動は変わらない。
  構築側は `lhs: Expr::Ident(col)`、分解側は `Expr::as_bare_column()` か `..` を使う。
  あわせて `Expr::as_bare_column()` を追加した。
- `sql::allowlist::WherePredicate::Prefix`（Issue #1778・SQL-54）: 必須フィールド `case_insensitive: bool` を追加した
  （`ILIKE` のとき真）。構築する外部コードと、全フィールドを列挙して分解するコードはコンパイルできなくなる。
  移行: 構築側は `case_insensitive: false` を足し、分解側は `..` を付ける。あわせて
  `declarative_filter::FilterOp` に `ILikeUnbound`、`LikePattern` に非公開フィールドを追加した
  （`FilterOp` を網羅 `match` するコードは腕を足す）。
- `sql::allowlist::ValidatedCopyTo`（Issue #1771・WIRE-22 (5)・SQL-34）: クレート内専用のフィールド `table_form`
  （`pub(crate)`）を追加した。構造体リテラルで生成する外部コードと、全フィールドを列挙して分解するコードは
  コンパイルできなくなる。移行: 構造体を自前で組み立てず、`validate_copy` 等の公開入口が返す検証済み値を使う。
  テーブル形かは `is_table_form()` で判定する。分解側は `..` を付ける。
- `core::CopyPlan::To`（Issue #1765・WIRE-22）: 第 3 要素に解決済みの `sql::copy_options::CopyOptions` を追加した
  （`To(format, result)` → `To(format, result, options)`）。2 要素でパターンマッチするコードはコンパイルできなくなる。
  移行: `To(f, r)` を `To(f, r, _)` に改め、出力にオプションを適用する側は第 3 要素を使う（`To(..)` は変更不要）。
- `COPY` の検証済みオプション（Issue #1764・WIRE-22）: `sql::allowlist::ValidatedCopyFrom`／`ValidatedCopyTo` に
  必須の公開フィールド `options: sql::copy_options::CopyOptions` を追加した。構造体リテラルで生成する外部コードと、
  全フィールドを列挙して分解（パターンマッチ）するコードはコンパイルできなくなる。移行: 構造体を自前で
  組み立てず、`validate_copy_*` 系の入口が返す検証済み値を使う（`options` は検証・既定値適用済みで、HEADER・DELIMITER・NULL・
  QUOTE・ESCAPE を `options` フィールドから参照できる）。分解側は `..` を付けるか `options` を束縛して使う。
- 述語形 `UPDATE`／`DELETE` の恒真述語（Issue #1744・SQL-36・SQL-41）: `WHERE TRUE`・`... OR TRUE`・`WHERE visible()` 単独を受理する。
  `sql::parser::bind_predicate_delete`／`bind_update_form` がこれらに `42601` を返さなくなり、`BoundOrGroup::is_trivially_true` を削除した。
  対象は自テナントの可視行で、影響行数上限は実行段が判定する。NoSQL 表層は不変。
- `WHERE` 省略の `UPDATE`／`DELETE`（Issue #1743・SQL-36）: `DELETE FROM t`・`UPDATE t SET ...` を受理する。
  `validate_delete_statement`／`validate_update_form` が `42601` を返さなくなり、`where_predicates()` が空を返しうる。
  単一行専用の入口（`validate_delete`・`validate_update`）・NoSQL 表層は不変（恒真述語は #1744 で変更）。
- 対象省略の `ON CONFLICT DO NOTHING`（Issue #1736・SQL-49）: `INSERT ... ON CONFLICT DO NOTHING` を受理し、行キー・
  PRIMARY KEY・UNIQUE 制約・UNIQUE 索引（式・部分を含む）のどれかに違反した行だけを飛ばす（`NOT NULL`・`CHECK`・`FOREIGN KEY`
  は従来どおりエラー）。公開 API の変更: `sql::allowlist::OnConflictClause::target` を `Vec<String>` から
  `OnConflictTarget`（`Columns(Vec<String>)`／`Omitted`）へ変更し、`sql::parser::BoundConflictTarget` に `AnyUnique` を追加した。
  対象省略の `DO UPDATE` は `42601`。詳細は [sql-upsert.md](docs/design/sql-upsert.md) の「Issue #1736 追記」。
- `current_schema()`・`current_schemas()`（Issue #1718・TABLE-27 (7)）: FROM なし SELECT で `current_schema()`・
  裸の `current_schema`・`current_schemas(bool)` が `search_path` に従った値（テナントで実在する SCHEMA のみ）を返す。
  互換性の変更: UDF 名 `current_schema`・`current_schemas` を予約名にした（`CREATE FUNCTION` は拒否される）。
  FROM あり SELECT・INSERT・DEFAULT・CHECK・索引・ビュー本文での使用は `0A000`。型は `text`／`text[]`。
  詳細は [schema-tenant-namespace.md](docs/design/schema-tenant-namespace.md) の §11。
- `search_path` の追加（Issue #1715・TABLE-27）: セッション変数 `search_path`（既定 `"$user", public`）の `SET`／`RESET`／`SHOW`
  を受理する。実効 path に `public` が含まれないセッションでは、一時表以外の未修飾のテーブル参照が `42P01`、作成系 DDL の
  作成先が `public` 以外になる文が `3F000`／`0A000` になる（既定の path の挙動は不変）。`"$user"` は handshake の username から
  展開し、自分の username と同名の SCHEMA を持つテナントの未修飾 `CREATE TABLE` は `0A000`（テナント SCHEMA への作成は後続 Issue）。
  公開 API は `Statement::SetSearchPath`・`ShowSearchPath`、`SqlOutcome::SetSearchPath`、`Token::QuotedDollarUser`、
  `SqlSurfaceError::NoSchemaSelected`、`ValidatedCreateTable.public_qualified`（pub フィールド追加）、`sql::search_path` の追加。
  BREAKING CHANGE: `Statement`・`SqlOutcome`・`Token`・`SqlSurfaceError` を網羅的に `match` するコードは variant の追従が必要。
- GIN 相当索引の読み取り（Issue #2208・INDEX-8）: 広域取得で jsonb 列への `@>`・`?`・`?|`・`?&`（`jsonb_ops`）と
  `@>`・`@?`・`@@`（`jsonb_path_ops`）が GIN 相当索引のポスティングで候補を絞るようになった（候補は全走査と同じ可視性判定と
  WHERE 全体の再評価を通るため、結果の行・順序・`LIMIT`／`OFFSET` の数え方は不変）。`EXPLAIN` はこの形のときだけ
  `scalar_plan: index_gin index=<索引名>`・`access_path: gin_index_candidates` を出す（従来は `plain_scan`／`full_scan`）。
  PK 点検索が成り立つ文は従来どおり PK が優先。`ORDER BY`・ウィンドウ・OR・NOT・空集合の問い合わせ・許可形でない jsonpath は不変。
  公開 API は `ScalarPlan::IndexGin` の追加（`#[non_exhaustive]`）。詳細は [gin-jsonb-index.md](docs/design/gin-jsonb-index.md) の §9。
- GIN 相当索引のキー抽出と DML 維持（Issue #2207・INDEX-8）: `CREATE INDEX ... USING gin` の宣言が物理テーブル
  `user_gin/{索引名}` のポスティングを持つようになり、INSERT・UPSERT・UPDATE・DELETE・COPY・TRUNCATE・分割実行 DML・
  FK 参照アクションと同じ write txn で維持する（`CREATE INDEX` は既存行から同期構築）。1 値あたり 65,535 エントリ・
  `jsonb_ops` の 1 エントリ 4,096 バイトを超える jsonb の書き込み・`CREATE INDEX` は `54000`（副作用なし）。
  結果集合は宣言の有無で変わらない（読み取り側の利用は #2208）。公開 API は `TenantWriteError::GinIndexEntryLimitExceeded`・
  `EngineCore::verify_gin_index_read_only`・`GinIndexAudit` の追加。
  BREAKING CHANGE: `TenantWriteError` に variant を追加したため外部の網羅的な `match` は更新が要る。GIN 索引付きの表への
  書き込みが上限超過で `54000` になりうる。
- GIN 相当索引の宣言（Issue #2206・INDEX-8）: `CREATE INDEX ... USING gin (<JSONB 列> [jsonb_ops | jsonb_path_ops])` を
  受理してカタログへ永続化する（従来は `0A000`）。JSONB 以外の列・複数列・式・`UNIQUE`・`WHERE`・`COLLATE` は `0A000`、
  未知の演算子クラスは `42704`。宣言の受理までで、索引の生成・利用は後続（宣言は結果集合に影響しない）。
  公開 API は `IndexKind::Gin(GinOpClass)` と `GinOpClass` の追加。索引カタログの形式は新版 `v8`（`v1`〜`v7` は不変）。
  BREAKING CHANGE: `IndexKind` に variant を追加したため外部の網羅的な `match` は更新が要る。`v8` を含むデータディレクトリは
  旧バイナリでは読めない（未知の版として拒否する）。
- NoSQL の単一行アドレスと insert・scan を PRIMARY KEY 基準へ移行（Issue #2201・TABLE-23・NOSQL-23）: SQL の `CREATE TABLE` で作った表（採番方式）に対して NoSQL の `scan`（`explain` を含む）・`insert`（`id` 不要）・`update`／`delete`（`where` に PRIMARY KEY の列を指定、複合可。PK なし表の単一行形は `42601`・述語形 `filter` は使える）が使える。疑似列 `id` を持つ表の `where: {"id": n}` は互換のまま。NoSQL の `aggregate`／`search` は採番方式の表に対して引き続き `0A000`。`update`／`delete` の `where` の型検査は表のスキーマ解決後（未知キーは従来どおり `42601`）。既存の要求は同じ応答を返すため、既存クライアントへの破壊的変更ではない。
- 既存表の `id` の移行（Issue #2200・TABLE-23）: 既存データディレクトリを開く時点で、従来方式の永続表のカタログを
  移行済みの印（v15 の `rowmode:migrated`。`RowIdentity::MigratedId`）へ一方向に書き換える（表ごとに原子的・再 open で収束。
  行・キー・索引・台帳は書き換えず、既存クエリの結果と wire の型 OID は不変）。壊れたカタログ項目は移行せず残す。
  運用手順（バックアップ）は README を参照。
  BREAKING CHANGE: 移行後のデータディレクトリは旧バイナリでは開けない（`rowmode:migrated` を不正値として拒否する）。
- 暗黙の `id` の廃止（Issue #2199・TABLE-23）: SQL 表層の `CREATE TABLE`・`CREATE TEMP TABLE` で新しく作る表は暗黙の
  `id` 疑似列を持たない（PG と同じ表定義）。`id` は予約名ではなくなり、`id` を通常の列として宣言でき（`tenant_id`・
  `visibility`・`check`・`constraint` は引き続き予約）、PRIMARY KEY を持たない表・`id` 列を持たない表を作れる。
  `INSERT` は `id` を要求せず、`SELECT *`・`RETURNING *` は宣言した列だけを返し、宣言の無い `id` は未定義列（`42703`）になる。
  `INSERT`（`ON CONFLICT`・`COPY` を含む）の行識別子は書き込み段が払い出す（応答・エラーに出ない）。`WHERE id = <n>` は
  新しい表では通常の述語。NoSQL 表層の `create_table`・Rust API の `create_table`・既存の全表は従来方式のまま
  （疑似列 `id` の挙動は不変）。新しい表に対する NoSQL の行 op・Rust API の行 API／検索 API は固定文言の `0A000` で拒否する
  （後続 Issue で対応）。新しい表の親を `id` 列で参照する FK は `0A000`、主キーの無い親への列省略 FK は `42830`。
  公開 API は `validate_create_table_tokens_with_implicit_id`・`ValidatedCreateTable.legacy_row_identity`（pub フィールド追加）・
  `TableLookup::table_is_allocated`（既定実装つきのメソッド追加）・`EngineCore::table_uses_allocated_row_identity` の追加。
  BREAKING CHANGE: SQL の `CREATE TABLE` で作った表の `INSERT ... (id, ...)` は `id` を宣言していなければ `42703` になる。
- 宣言 PRIMARY KEY の点検索（Issue #2198・TABLE-23）: 広域取得で PK の全構成列がリテラル等価のとき、永続一意索引の
  点検索で実行する。`EXPLAIN` はこの形のときだけ `scalar_plan: index_primary_key`・`access_path: primary_key_lookup`
  を出す（従来は `plain_scan`／`full_scan` 固定。PK 未宣言の表・部分等価・範囲比較・`ORDER BY` 付きは不変）。
  公開 API は `ScalarPlan::IndexPrimaryKey` の追加（`#[non_exhaustive]`）。結果は全走査と一致する。
- ビューの `WITH CHECK OPTION`（Issue #1673・TABLE-43 (2)）: `CREATE [OR REPLACE] VIEW ... WITH [CASCADED | LOCAL]
  CHECK OPTION` を受理して永続化し（従来は `42601`）、ビュー経由の `INSERT`／`UPDATE`（UPSERT を含む。`DELETE` は
  検査しない）が書いた行を検査対象の述語（NULL も違反）で検査して、違反を `44000`（固定文言。ビュー名・行の値を
  含めない）で文ごと拒否する。入れ子は PostgreSQL と同じ規則（LOCAL は自身の段、CASCADED は下位すべて、
  下位が自分で宣言していればその段も検査）。自動更新できないビューへの宣言は `0A000`。公開 API は
  `ViewDef.check_option`（pub フィールド追加）・`ViewCheckOption`・`Storage::create_view_with_check_option` の追加と、
  `TenantWriteError::WithCheckOptionViolation`・`SqlSurfaceError::WithCheckOptionViolation`・
  `ErrorClass::WithCheckOptionViolation`（`44000`・HTTP 400）の追加。宣言ありのビュー定義のカタログ形式は新版
  v3（宣言なしの v1／v2 のバイト列は不変。旧バイナリは v3 を読めない）。
- 部分 UNIQUE 索引（Issue #1657・INDEX-11 (4)）: 従来 `0A000` だった `CREATE UNIQUE INDEX ... WHERE <述語>` を受理し、
  述語が TRUE の行どうしの間でだけ一意性を `23505` で強制する（述語外の行どうし・述語外と述語内の重複は許可。
  テナントごと・NULLS DISTINCT）。作成時の既存行検査も述語 TRUE の行だけが対象。UNIQUE 制約へは展開しないため
  `ON CONFLICT` の推論対象にならない。述語付き一意索引のカタログ形式は新版 `v7`（`v1`〜`v6` のバイト列は不変。
  旧バイナリは `v7` を読めない）。永続一意索引（`user_uniq`）の `FORMAT_VERSION` を 3 から 4 へ上げたため、
  既存の全テナントは初回の書き込み時に一意索引を 1 回だけ再構築する（その回のみ O(n)）。`USING hnsw`・`COLLATE`・
  マテリアライズドビューとの組み合わせは引き続き `0A000`。
- 部分索引の格納と DML 維持（Issue #1654・INDEX-11）: 従来 `0A000` だった `CREATE INDEX ... WHERE <述語>` の
  決定的な述語を、書き込みトランザクション内で束縛して正規化テキストを永続化する（SQLSTATE が `0A000` から
  成功、または未知列 `42703`・セッション依存の述語 `42P17`・予約列と `visible()` `42601` 等へ変わる）。
  述語を満たす行だけが索引化され、`INSERT`／`UPDATE`／UPSERT／`COPY` は述語が TRUE の行でだけ式要素を評価する
  （NULL・FALSE の行は索引外。述語の評価エラーは文を拒否し、`CREATE INDEX` は既存行を全件検証する）。
  `UNIQUE`・`USING hnsw`・`COLLATE`・マテリアライズドビューとの組み合わせは引き続き `0A000`。参照列の
  `DROP COLUMN` は部分索引を連鎖削除し、`RENAME COLUMN` は述語を追随させ、述語・要素が参照する列の
  `ALTER COLUMN TYPE` は `0A000`。部分索引のプランナ選択と `EXPLAIN` への表示は #1656（下記）で結線した。
  公開 API は `IndexDef.predicate`（pub フィールド追加）・`IndexPredicateDef`・`IndexDef::with_predicate`・
  `IndexDef::has_predicate` の追加。述語付きの宣言のカタログ形式は新版 `v6`（`v1`〜`v5` のバイト列は不変。旧バイナリは
  `v6` を読めない）。公開の `Storage::create_index` は述語付きの `IndexDef` を `Invalid` で拒否する
  （SQL 表層のみが作成できる）。
- 部分索引のプランナ選択と EXPLAIN 表示（Issue #1656・INDEX-11・INDEX-7）: 起動時 opt-in 済みのエンジンで、
  通常の分類が全走査になる検索 SELECT のうち、`WHERE` が部分索引の述語を含意すると保守的に証明できるものは、
  部分索引の格納行で候補を絞る（残りの述語は必ず再評価する。信頼マスク経路には入れない）。証明できない・
  未構築・選択度超過・カタログ読み取り失敗はこれまでどおり全走査で、結果と RLS の契約は索引の有無で変わらない。
  `EXPLAIN` の `scalar_plan:` 行に新しい語彙 `index_partial` を追加し、含意された部分索引の名前を
  `index=<名前>[,<名前>...]`（昇順）で示す（クライアントから見える出力の語彙が増える）。公開 API は
  `ScalarPlan::IndexPartial` の追加（`#[non_exhaustive]`）。集計・`GROUP BY`・広域取得は部分索引を使わない。
- 式索引の UNIQUE と失効管理（Issue #1651・INDEX-9・INDEX-10）: 従来 `0A000` だった
  `CREATE UNIQUE INDEX ... ((<式>))`（列要素との複合を含む）を受理し、式の値の一意性を全書き込み経路で
  `23505` により強制する（NULL は重複しない・テナントごとに独立・不可視行とも衝突）。作成時に既存行へ
  重複があれば `23505` で拒否する（索引は残らない）。参照列の `DROP COLUMN` は一意の式索引も連鎖削除し、
  `RENAME COLUMN` は式テキストを追随させ、`ALTER COLUMN TYPE` は式を再束縛して既存行を再検証する
  （従来の `0A000` を撤去）。一意の式索引のカタログ形式は新版 `v5`（`v1`〜`v4` のバイト列は不変。旧バイナリは
  `v5` を読めない）。永続一意索引（`user_uniq`）の `FORMAT_VERSION` を 2 から 3 へ上げたため、既存の
  全テナントは初回の書き込み時に一意索引を 1 回だけ再構築する（その回のみ O(n)）。
- 式索引のプランナ選択（Issue #1650・INDEX-10・INDEX-7）: WHERE の `<式> op <リテラル>`（`op` は `=`／`<`／`<=`／
  `>`／`>=`）が宣言済みの式索引と構造一致したときだけ、式キー索引で候補を絞る。一致しない形・宣言なし・
  起動時 opt-in なしは従来どおりの全走査で、結果は変わらない。`EXPLAIN` の `scalar_plan:` 行（検索・集計・
  `USING PLAN`）に `index=<式索引名>` を出す（語彙は増やさない）。公開 API は `sql::explain::ExplainShape` が
  `Copy`・`Eq` でなくなる（式候補を保持するため。`from_filters` のシグネチャは不変）。
- 式索引の DML 維持（Issue #1649・INDEX-10）: 式索引を持つテーブルへの `INSERT`／`UPDATE`／UPSERT／`COPY`
  （参照アクションによる子表の更新を含む）が、式の評価エラー（0 除算 `22012` 等）・非有限値（`22003`）で
  失敗する（副作用ゼロ）。NULL を返す行は許容する。`CREATE INDEX ... ((<式>))` は保存前に既存行を全件検証し、
  失敗行があれば拒否する（応答は固定文言）。公開 API は `TenantWriteError::IndexExpressionEvaluationFailed`
  の追加（`#[non_exhaustive]` ではないため網羅的 `match` の下流は追随が必要）。
- 式索引の格納（Issue #1648・INDEX-10）: 従来 `0A000` だった `CREATE INDEX ... ((<式>))`／`(f(col))` のうち
  決定的な式を、書き込みトランザクション内で束縛して永続化する（未知列 `42703`・型不整合は束縛側の
  SQLSTATE・型で決まる非決定性 `42P17`・索引化できない結果型 `0A000`）。`UNIQUE`・`USING hnsw`・式への
  `COLLATE` との組み合わせは引き続き `0A000`。参照列の `DROP COLUMN` は式索引を連鎖削除し、
  `RENAME COLUMN`／`ALTER COLUMN TYPE` は `0A000`。公開 API は `IndexDef.expressions`（pub フィールド追加）・
  `IndexExpressionDef`・`IndexDef::with_expressions` の追加。式を含む宣言のカタログ形式は新版 `v4`
  （`v1`／`v2`／`v3` のバイト列は不変。旧バイナリは `v4` を読めない）。公開の `Storage::create_index` は
  式付きの `IndexDef` を `Invalid` で拒否する（SQL 表層のみが作成できる）。
- `REFRESH MATERIALIZED VIEW [CONCURRENTLY] <名前> [WITH [NO] DATA]`（Issue #1665・TABLE-42）:
  従来 `42601` だった構文を受理し、全文脈の実体を同じスナップショットから単一トランザクションで作り直す。
  失敗時は旧実体が残る。`WITH NO DATA` は実体を破棄して未実体化（`55000`）にする。タグは固定の
  `REFRESH MATERIALIZED VIEW`。`CONCURRENTLY` は #1666 で本実装済み。公開 API は
  `ParsedSql::RefreshMaterializedView`・`SqlOutcome::RefreshMaterializedView`・
  `ddl::RefreshMaterializedViewOutcome` を追加する。

- `REFRESH MATERIALIZED VIEW CONCURRENTLY`（Issue #1666・TABLE-42 (4)）: 従来の暫定 `0A000` をやめて受理する。
  未実体化は `0A000`、WHERE を持たない列の UNIQUE 索引が無ければ `55000`（固定文言）で、いずれも旧実体が残る。
  前提を満たせば非 CONCURRENTLY と同じ結果になる。`CONCURRENTLY` と `WITH NO DATA` の併用は `42601`。公開 API は
  `SqlSurfaceError::MaterializedViewNotRefreshableConcurrently` を追加する。

- `DROP MATERIALIZED VIEW [IF EXISTS] <名前>[, ...] [CASCADE | RESTRICT]` と matview への索引（Issue #1667・
  TABLE-42 (5)(6)・TABLE-37）: 従来 `42601` だった `DROP MATERIALIZED VIEW` を受理する（複数指定は単一 txn で原子的、
  表・ビュー・索引名の指定は `IF EXISTS` でも `42809`、依存する matview は RESTRICT で `2BP01`、CASCADE で依存する
  側から削除）。表・ビューの `DROP ... CASCADE` は依存 matview を推移的に削除する（従来は #1663 の暫定ガードで
  `2BP01`）。matview への `CREATE [UNIQUE] INDEX` は従来 `42809` だったが宣言を受理する（スカラーのみ。
  `USING hnsw` は `0A000`、暗黙の `id` 列は無く `42703`）。UNIQUE 索引は文脈（テナント）ごとに一意性を強制し、
  重複は `23505`（固定文言）。公開 API は `ParsedSql::DropMaterializedView`・`SqlOutcome::DropMaterializedView`・
  `CascadedObject::MaterializedView` の追加。
- `CREATE MATERIALIZED VIEW [IF NOT EXISTS] <名前> [(<別名>, ...)] AS <本文> [WITH [NO] DATA]`（Issue #1664・TABLE-42）:
  従来 `42601` だった構文を受理し、実体化対象の文脈ごとに本文を評価した結果を単一トランザクションで保存する。
  読み取りは読み手の文脈に一致する実体だけを返し、実体が無ければ新分類 `55000`（HTTP 射影は 409）。
  書き込みは `42809`。実体化対象の文脈は `EngineCore::with_materialization_contexts` で注入する（未設定なら
  作成セッションの文脈のみ）。公開 API は `Statement::MaterializedView`・`ParsedSql::CreateMaterializedView`・
  `SqlOutcome::CreateMaterializedView`・`SqlSurfaceError::MaterializedViewNotPopulated`・
  `ErrorClass::ObjectNotInPrerequisiteState` の追加と、`TableLookup::materialized_view_columns`（既定実装あり）の追加。
  matview が直接参照する表への `DROP COLUMN`（`2BP01`）・`ALTER COLUMN TYPE`／`RENAME COLUMN`（`0A000`）は
  列単位の追随（別 Issue で追跡）まで拒否する。
- `TRUNCATE ... [CASCADE | RESTRICT]`（Issue #1644・SQL-48 (2)）: 従来 `42601` だった `CASCADE`／`RESTRICT` を
  受理する（省略時は RESTRICT）。RESTRICT は指定していない表の FK が参照する表の `TRUNCATE` を、参照元の
  行の有無・他テナントの行に関係なくカタログ宣言だけで `0A000` で拒否する（従来は行を消した後の事後検査で
  自テナントの子の行が残れば `23503`。子が空なら成功していた）。CASCADE は FK の参照元の表を推移的に
  対象へ加え、同じトランザクションで自テナントの行を削除する（参照アクションは発火しない）。公開 API は
  `ValidatedTruncate::behavior` の追加、`TruncateOutcome` の `Copy` 削除と `cascaded()` の追加、
  `TenantWriteError::TruncateReferencedByForeignKey` の追加。
- IDENTITY 列（Issue #1695・TABLE-24）: `CREATE TABLE` の列制約 `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY
  [(<SEQUENCE オプション>)]` を受理する（従来は `42601`）。整数型・`NOT NULL`・列が所有する暗黙 SEQUENCE（型は列型で
  決まり、範囲外のオプションは `22023`）・`DEFAULT` の IDENTITY 表現へ展開し、表と同じ write txn で作る。`ALWAYS` 列への
  明示値・明示 NULL・UPDATE の SET は `428C9`（HTTP 400。型の束縛より前で、SEQUENCE を進めない）、`BY DEFAULT` は
  明示値を受け付ける。COPY は IDENTITY 列へ入力値をそのまま書く。`OVERRIDING`・ALTER の IDENTITY 操作・IDENTITY 列の
  型変更・一時表・生成列は `0A000`、`DEFAULT` との併記は `42601`。公開 API は `ColumnDefault::Identity`・`IdentityKind`・
  `ValidatedCreateTable::identity_columns`（pub フィールド追加）・`IdentityColumn`・`SequenceOptions`・
  `SqlSurfaceError::GeneratedAlways`・`ErrorClass::GeneratedAlways`（`428C9`）の追加。カタログの列 `default` に新タグ
  `a`／`b` を足す（旧バイナリは未知タグを `CorruptSchema` で拒否する）。
- `SERIAL` 系型（Issue #1694・TABLE-24）: `CREATE TABLE` の `SERIAL`／`BIGSERIAL`／`SMALLSERIAL`（別名 `SERIAL4`・
  `SERIAL8`・`SERIAL2`）を受理する（従来は ENUM 型名候補として `42704`）。基底の整数型・`NOT NULL`・暗黙 SEQUENCE
  （`<table>_<col>_seq`・列が所有・MAXVALUE は列型の上限）・`DEFAULT nextval` へ展開し、表と同じ write txn で作る。
  DROP TABLE・DROP COLUMN は所有 SEQUENCE も削除し、RENAME は所有者を追従させる。`DEFAULT` 併記は `42601`、
  `ADD COLUMN`・一時表・配列は `0A000`、列定義の外の型名は `42704`。SERIAL 系の 6 語は列型キーワードとなり、
  制約名・NoSQL の `enum_type` として使えなくなる。公開 API は `ValidatedCreateTable::serial_columns`（pub フィールド追加）・
  `SerialColumn`・`SqlColumnTypeName::Serial`・`SerialKind` の追加。
- `TRUNCATE ... RESTART IDENTITY`／`CONTINUE IDENTITY`（Issue #1645・SQL-48 (3)）: 従来 `42601` だった識別句を
  受理する（省略時は CONTINUE）。RESTART は対象表が所有する SEQUENCE の自テナントの採番を開始値へ戻す。
  所有関係は SEQUENCE 定義 blob の版 3 に格納する（所有者なしは従来の版 2 のまま。旧バイナリは版 3 を
  `CorruptSchema` で拒否する）。SQL から所有者を作る `OWNED BY`／SERIAL は後続 Issue（#1694）のため、それまで
  RESTART は採番に作用しない。明示トランザクション内は所有 SEQUENCE があれば `0A000`。公開 API は
  `ValidatedTruncate::restart_identity` の追加。
- `CREATE VIEW v (a, b) AS ...`（Issue #1659・TABLE-41 (1)）: 従来 `42601` だった列別名リストを受理する。
  別名は出力列に先頭から名前を付け、短いリストの残りは導出名のまま。出力列より長いリストは `42601`、
  リスト内または最終名の重複は `42701`。別名付きビューが読む表への `RENAME COLUMN` は `0A000`、
  `DROP COLUMN`／`ALTER TYPE` は依存ありとして拒否する（保守側）。公開 API は `ViewDef.column_aliases` の追加
  （格納形式 v2。別名なしは従来と同一のバイト列）と `Storage::create_view_with_column_aliases`・
  `ValidatedCreateView::column_aliases` の追加。
- `ALTER COLUMN TYPE` の依存オブジェクト再構築（Issue #1637・TABLE-40 (3)(4)・TASK-278）:
  一般変換が、依存する索引宣言・PK／UNIQUE・CHECK・FK・DEFAULT／補完値を新しい型で再構築・再検証する
  （従来は一律 `0A000`。重複 `23505`・CHECK 違反 `23514`・FK の型照合 `42804`／参照整合性 `23503`・
  DEFAULT の変換不能 `42804`）。再検証は全テナント・全可視性の行で、拒否時は表不変・エラー文言に値を含めない。
  ビューが参照する列（`SELECT *`・連鎖を含む）は PostgreSQL と同じ `0A000` のまま。公開 API の変更なし。
- DROP の CASCADE／RESTRICT（Issue #1641・TABLE-37）: `DROP TABLE|VIEW` が `CASCADE`／`RESTRICT` を受理し、`CASCADE` は依存する FK 制約とビューを連鎖削除する。`ValidatedDropTable` に `pub behavior: DropBehavior` を追加、`DropTablesReport` に `cascaded` を追加、`DropTableOutcome`／`DropViewOutcome` から `Copy` を外し（`cascaded()` を追加）、`Storage::drop_tables` に `DropBehavior` 引数を追加した。`Storage::drop_view_with_behavior` を追加。`DROP INDEX|TYPE ... CASCADE` は `42601` のまま。
- 表の改名でビュー・台帳を追随（Issue #1625・TABLE-38）: `ALTER TABLE ... RENAME TO` が単純形ビューの
  定義を新名へ付け替え、`operation_id` 台帳・`last_op`・分割実行ジョブ記録を新名へ移す（従来は
  ビュー依存の表の改名を `0A000` で拒否し、台帳は削除していた）。評価後射影形ビューが読む表の改名は
  引き続き `0A000`。公開 API の型変更なし。
- 式索引の parser と式の決定性検査（Issue #1647・INDEX-10）: `CREATE INDEX` の列リストで
  `((<式>))` と関数呼び出し形 `(f(col))` を構文上受理し、非決定式を `42P17`、集計を `42803`、
  ウィンドウを `42P20`、部分クエリ・SRF・組み込み以外の関数を `0A000` で拒否する（従来はいずれも
  一律 `0A000`）。決定的な式も後続 Issue（#1648〜#1651）までは `0A000`（式索引は作成できない）。
  `((col))` は通常の列索引として作成できる。公開 API は `SqlSurfaceError`（`InvalidObjectDefinition`・
  `GroupingError`・`WindowingError`）・`ErrorClass`（同名 3 分類）への variant 追加と
  `ValidatedCreateIndex::expression_keys` の追加。
- 部分索引の述語 parser と決定性検査（Issue #1653・INDEX-11）: `CREATE INDEX ... WHERE <述語>` の述語を
  解析し、非決定を `42P17`、集計を `42803`、ウィンドウを `42P20`、部分クエリ・SRF・組み込み以外の
  関数を `0A000`、`visible()` と構文誤りを `42601` で拒否する（従来は `WHERE` を一律 `0A000`）。
  決定的な述語も後続 Issue（#1654）までは `0A000`（部分索引は作成できない）。公開 API は
  `ValidatedCreateIndex::predicate` の追加。
- `DROP TABLE a, b`（Issue #1639・TABLE-37 (1)）: 従来 `42601` だった複数テーブル指定を受理する。
  文全体を 1 つの write txn で原子的に実行し（不在・種類違い・依存のどれか 1 つでもあれば何も削除しない）、
  同じ文に並べた名前どうしの依存は数えない。`CASCADE`／`RESTRICT` は従来どおり `42601`。公開 API は
  `ValidatedDropTable.table_name: String` の `table_names: Vec<String>` への置き換えと
  `Storage::drop_tables`・`DropTablesReport` の追加。
- `ALTER COLUMN TYPE` の全行書き換え（Issue #1636・TABLE-40 (1)(2)(5)・TASK-278）:
  `USING <式>` 付きの変換と、代入変換（SQL-47）のある組の `USING` 省略変換（BIGINT→INTEGER・
  NUMERIC の位取り変更・数値↔文字列等）を受理する。従来の `0A000`（USING 付き）・`42804`（縮小・異種）
  から変わる。全テナント・全可視性の行を 1 つの write txn で書き換え、1 行でも変換に失敗
  （`22P02`／`22003`／`22001`／`23502` 等）すれば表は不変で、エラー文言に値・テナント・行 id を含めない。
  PK／UNIQUE／FK／CHECK／索引宣言／ビューが参照する列と DEFAULT を持つ列への一般変換は、Issue #1637
  で再構築に置き換えた（下記。従来の受理経路は不変）。`USING <対象列>` は省略と同じ扱い。公開 API は
  `Storage::alter_table_alter_column_type_collated` の引数追加（crate 内部）で、外部公開面の変更なし。
- `ALTER TABLE ... ALTER COLUMN ... TYPE ... USING <式>` と `SET DATA TYPE`（Issue #1635・TABLE-40）:
  従来 `42601` だった構文を受理する。USING は旧スキーマで束縛・型検査し（未定義列 `42703`、
  代入変換不可 `42804`、組み込み以外の関数 `42601`）、通ったものは行の書き換え未実装のため
  `0A000` で拒否する。USING 省略で代入変換の無い組は `42804`。公開 API は
  `ValidatedAlterTableAlterColumnType.using` の追加。
- `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` と複数アクション（Issue #1633・TABLE-36・TABLE-39 (5)）:
  従来 `42601` だった構文を受理する。同名の生存列があれば何もせず成功し、`,` 区切りの列単位アクション
  （ADD／DROP COLUMN・SET／DROP DEFAULT・SET／DROP NOT NULL）は 1 つの write txn で原子的に適用する
  （途中失敗で何も適用しない）。RENAME との混在は `42601`、制約・TYPE 変更との混在は `0A000`。
  公開 API は `ValidatedAlterTableAddColumn.if_not_exists`・`ValidatedAlterTable::Multiple`・
  `AlterTableAction::{AddColumnSkipped, Multiple}` の追加（既存の網羅的 `match` はアームの追加が必要）。
- `ALTER COLUMN DROP NOT NULL`（Issue #1632・TABLE-39 (4)）: 従来 `42601` だった構文を受理する。
  主キーの構成列は `42P16`、FK 参照先側の整合違反は `42830`。公開 enum `ValidatedAlterTable` に
  `AlterColumnDropNotNull`、`AlterTableAction` に同名の variant、`CatalogError`・`SqlSurfaceError` に
  `ColumnInPrimaryKey` を追加した（既存の網羅的 `match` はアームの追加が必要）。
- `ALTER COLUMN SET／DROP DEFAULT`（Issue #1628・TABLE-39）: 従来 `42601` だった構文を受理し、
  INSERT 既定値（`default_expr`）だけを変える（既存行の補完値 `fill_value` は変えない）。公開 enum
  `ValidatedAlterTable` に `AlterColumnSetDefault`・`AlterColumnDropDefault`、`AlterTableAction` に
  同名の variant を追加した（既存の網羅的 `match` はアームの追加が必要）。以後 `ColumnDef` の
  `fill_value` と `default_expr` は ALTER で食い違いうる（食い違うスキーマはカタログ v14 で書く）。
- 列定義の分離（Issue #1627・TABLE-39 の前提）: `engine::catalog::ColumnDef` の公開フィールド
  `default` を `default_expr`（`INSERT` 既定値）へ改名し、読み出し時補完値 `fill_value` を追加する。
  `ColumnDef::with_default` は両方を同じ値にする。カタログは補完値が食い違う列を持つ場合のみ
  新形式 v14 で書く（旧バイナリは v14 を拒否する）。

- `DROP TABLE|INDEX|VIEW|TYPE IF EXISTS`（Issue #1614・TABLE-36）: 従来 `42601` だった構文を受理し、
  対象不在は副作用ゼロの no-op で成功する。公開 API は `ValidatedDropTable.if_exists` の追加と
  `Drop{Table,View,Index,Type}Outcome::skipped()` の追加（構造体リテラルでの構築は不可に）。
- `CREATE TABLE`／名前付き `CREATE INDEX` の `IF NOT EXISTS`（Issue #1613・TABLE-36）: 従来 `42601` だった
  句を受理し、同名の表・ビュー・索引があれば何もせず成功する。公開構造体 `ValidatedCreateTable` に
  公開フィールド `if_not_exists` を追加した（構造体リテラルで作る外部コードは追従が必要）。
  `CREATE VIEW`／`CREATE TYPE`／名前なし `CREATE INDEX` の `IF NOT EXISTS` は `42601` のまま。
- 配列型 OID の公告（Issue #1606・TABLE-35）: 配列列と結果型が確定する配列式の `RowDescription` の
  型 OID が `text`（25）から要素型ごとの PostgreSQL 配列型 OID（`int4[]`=1007 など）に変わる。
  `ENUM[]` は 25 のまま。配列のバイナリ形式は従来どおり `0A000`。
- `EXTRACT`／`date_part`／`date_trunc` の INTERVAL 対応と `justify_*`（Issue #1582・TABLE-34）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::BuiltinFn` に `IntervalPart`・`IntervalTrunc`・`JustifyHours`・
  `JustifyDays`・`JustifyInterval` を追加した（既存の網羅的 `match` はアームの追加が必要）。
  従来 `42804` だった `date_part(field, interval)` などが受理に変わる。`justify_hours`／
  `justify_days`／`justify_interval` は組み込み関数名として予約され、同名の UDF 定義は `42723` になる。
- 日付時刻と INTERVAL の加減算・乗除（Issue #1581・TABLE-34）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::Expr` に `IntervalLiteral(Interval)`、`BoundExpr` に
  `Interval`・`IntervalColumnRef`、`ExprType`／`ExprValue` に `Interval` を追加した
  （既存の網羅的 `match` はアームの追加が必要）。従来 `42804` か「列を式で使えない」
  エラーだった `TIMESTAMP ± INTERVAL`・`DATE ± INTERVAL`・`INTERVAL * n`・
  `TIMESTAMP - TIMESTAMP` などの INTERVAL 算術と、INTERVAL 列の式内参照・型付きリテラル
  `INTERVAL '…'` が受理に変わる。INTERVAL が絡む未対応の演算子の組は `42883` で拒否する。
- `AT TIME ZONE`・TIMESTAMPTZ の変換と問い合わせ（Issue #1576・TABLE-33）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::Expr` に `TimestampTzLiteral { micros, session_local }`、
  `BoundExpr` に `TimestampTz`・`TimestampTzColumnRef`、`ExprType`／`ExprValue` に `TimestampTz`、
  `BuiltinFn` に TZ 変換・`DatePartTz`・`DateTruncTz` 系の variant、
  `datetime_fn::DatePartField` に `Timezone`・`TimezoneHour`・`TimezoneMinute`、
  `declarative_filter::TypedLiteral` に `TimestampTz` を追加した（既存の網羅的 `match` は
  アームの追加が必要）。`timezone` を組み込み関数名として予約したため、同名の UDF は
  作れない（`42723`）。TIMESTAMPTZ の述語・集計・二次索引・集合演算は従来の `0A000`／`42883`
  拒否から受理へ変わる。
- セッション TimeZone（Issue #1575・TABLE-33）に伴う公開 API の変更: 公開 enum
  `engine::sql::allowlist::Statement` に `SetTimeZone { value: TimeZoneSetting }`・`ShowTimeZone`、
  公開 enum `engine::sql::SqlOutcome` に `SetTimeZone`・`Show(QueryResult)` を追加した
  （既存の網羅的 `match` はアームの追加が必要）。`engine::datetime::SessionTimeZone` は表示値を
  持つ構造体になり、`SessionState::time_zone`・`SessionTimeZone::parse_setting` を追加した。
  pg wire の起動時 ParameterStatus に `TimeZone`（既定 `UTC`）が増え、StartupMessage の
  `TimeZone` を受理する（不正値は認証後に FATAL `22023`）。
- crate 名を `fandhe-vector-db-engine` / `fandhe-vector-db-wire-server` から
  `fandhe-db-engine` / `fandhe-db-wire-server` へ変更した（プロジェクト名を
  vector-db から fandhe-db へ改めたため）。0.2.0 は新しい crate 名での初公開版となる。
  旧 crate 名の 0.1.0 は crates.io に残すが、以後は更新しない。移行は `Cargo.toml` の
  依存名を新しい crate 名へ置き換える（lib 名 `engine` / `wire_server` は不変のため
  `use` 文の変更は不要）。
- バッチ上限の環境変数を `VECTOR_DB_BATCH_MAX_FILES` / `VECTOR_DB_BATCH_MAX_TOTAL_BYTES` /
  `VECTOR_DB_BATCH_MAX_CHUNKS` から `FANDHE_DB_BATCH_MAX_FILES` /
  `FANDHE_DB_BATCH_MAX_TOTAL_BYTES` / `FANDHE_DB_BATCH_MAX_CHUNKS` へ改名した。旧名は
  読まれず（設定しても既定の上限が適用される）、設定されていれば起動時に stderr へ
  警告が出る。移行は環境変数名を新名へ置き換える。
- `engine::sql::allowlist::ValidatedInsert` の公開フィールド
  `pub values: Vec<InsertLiteral>` を `pub rows: Vec<Vec<InsertLiteral>>` へ
  変更した。単一行 INSERT は `rows.len() == 1` として同じ内容を保持するため、
  移行は `stmt.values` を `stmt.rows[0]`（単一行前提のコードの場合）へ
  読み替えるか、複数行を扱う場合は `rows` を反復する形へ書き換える。
- 書き込み文の現在時刻関数（Issue #2188・SQL-37）に伴う公開 API の変更: 公開 enum
  `engine::sql::allowlist::InsertLiteral` に `CurrentTime(TimeDefault)`、公開 enum
  `engine::catalog::TimeDefault` に `StatementTimestamp`・`ClockTimestamp`・`LocalTime(Option<u8>)`
  を追加した（網羅 `match` は更新が必要）。`engine::sql::parser::BoundInsert::volatile_default_columns`
  の型を `Vec<usize>` から `Vec<VolatileColumn>`（新しい公開 enum）へ変更した。移行は
  `VolatileColumn::Default(i)` を従来の位置 `i` として読み替える。
- `operation_id` 台帳に値フォーマット v3（ハッシュ＋評価に用いたトランザクション開始時刻）を追加した。
  時刻関数を評価した書き込みだけが v3 を書く。v3 を書いた DB を旧バイナリで開くと、該当キーの再送照合が
  `XX000`（fail-closed）になる（ダウングレード不可の既知の制約。移行処理は無い）。
- `engine::sql::parser::BoundInsertForm` に新しい variant `RowBatch(Vec<BoundInsert>)`
  を追加した。この enum を網羅的に `match` していた利用側コードは、新 variant
  への対応を追加しないとコンパイルできない。
- いずれも SQL 表層の複数行 `VALUES (...), (...)` 構文サポート（ビヘイビア ID:
  SQL-16、TASK-190）のための変更であり、単一行 `INSERT` の外部観測可能な挙動
  （wire プロトコル応答・`wire_code`）は不変。
