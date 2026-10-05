# 拡張クエリプロトコル `$n` パラメータ束縛（Issue #935・WIRE-12・TASK-217）

## ステータス

Implemented（engine 側の値束縛機構は #935・#1012、wire 側の結線は Issue #1171）。
wire 側（`crates/wire-server/src/extended_query.rs`）の Parse／Describe(S)／Bind
が `$n` を受理し、値は必ず engine の `bind_prepared` を通す（第 2 の実行器を
作らない）。

### wire 結線（Issue #1171）の決定事項

- Parse: 宣言型件数は `MAX_PARAMS`（64）超過を OID 復号より前に `54000` で拒否する。
  `$n` を含まない文は従来どおり `Parsed`（`PreparedSql::parsed_if_unparameterized`）
  として保持し挙動不変。`$n` を含む文は `PreparedStatement::Parameterized` を保持する。
- 宣言 OID は値の検証には一切使わない（値は常にリテラル置換経路を通る）。0 以外は
  `ParameterDescription` へ echo し、0・未宣言は engine の型推論
  （`PreparedSql::param_types`。`ORDER BY <vec> <=> $n` は text 公告だがバイナリ不可の
  `VectorText`、`USING PLAN($n)`／`USING OPERATION_ID $n` は `Text`、`WHERE <列> = $n`
  ・`INSERT` の行内 ordinal は当該列の型、未参照番号は `Text`）で補う。
  pgjdbc が文字列に varchar（1043）を宣言する点を踏まえ、宣言 OID の不一致は拒否しない。
- 宣言件数がプレースホルダ数を超える Parse は `08P01`（PostgreSQL は受理するが、
  fail-closed な逸脱）。
- Bind: 値数が要求数と異なれば `08P01`。パラメータ format code は 0・1 以外を `08P01`。
  binary は、text 系スロット（宣言 OID が 0・25・1043）は UTF-8 バイト恒等で、`int4`／`int8`／
  `float4`／`float8`／`bool`／`bytea`／`uuid` は PostgreSQL の受信形式で正規テキストへ復号して
  から `bind_prepared` へ渡す（Issue #1345・WIRE-14）。受理は推論スロット型と実効 OID（宣言が
  0 以外ならそれ、0 なら推論）の互換表（整数列は 23／20、`REAL` は 700、`DOUBLE` は 701、
  `BOOLEAN` は 16、`BYTEA` は 17、`UUID` は 2950、`id` は宣言 23／20 のみ）に載る組み合わせに限り、
  PostgreSQL の暗黙キャストより意図的に厳しい fail-closed とする。非対応スロット
  （`NUMERIC`・日時・`JSON`・配列・`ENUM`・`VECTOR`・宣言 0 の `id` 等）への binary 指定は
  `0A000`、値の長さが固定長型の受信形式と合わなければ `08P01`（値は応答に含めない）。判定順は
  「全スロットの非対応判定（`0A000`）→ 復号（`08P01`）」で、復号後の値で保持量上限を判定する。
- 値の形式不正（非 UTF-8・NUL）は `22P02` へ移行した（従来 `22000`）。NULL は据え置き
  `22000`。
- 束縛済み `ParsedSql` は値を保持するため、portal が保持する束縛値バイトの接続単位合計を
  `limits::MAX_BOUND_PARAM_BYTES_PER_SESSION`（4 MiB）で制限する（超過は `54000`。
  portal ごとの値から都度合算するため除去経路での減算漏れが無い）。

## 背景

`crates/wire-server/src/extended_query.rs`（#933・WIRE-11）の Parse は
`num_param_types > 0` を `0A000` で拒否し、`sql::lexer::tokenize` は `$` を
一律 `42601` で拒否していた。そのため psycopg 3（既定の拡張クエリプロトコル）・
node pg（`values` 付き `query`）・psql `\bind`・JDBC 等、パラメータ付きクエリを
送るクライアントは実行できなかった。

## 設計: トークン単位の値スロット束縛

方式は「文を解析（Parse）するタイミング」と「値を束縛（Bind）するタイミング」
を分離し、値未確定のまま構造検証を一度通し、Bind 時に実値へ 1 トークン置換して
**もう一度同じ構造検証を通す**という 2 段構成にした。得られる `ParsedSql` は
「同じ値を正しくエスケープしたリテラルで書いた SQL テキスト」を `parse_sql`
した結果と完全に同一になるため、第 2 の実行器を作らずに済む。

### 字句解析（`sql::lexer`）

- `Token::Param(u16)`（1 始まり）を追加した。既定の `tokenize` は挙動不変
  （`$` を引き続き「未対応文字」として拒否する）。新設
  `tokenize_with_params` だけが `$n` を受理する。両者は `tokenize_impl` を
  共有する。
- `$0`・先頭ゼロ（`$01`）・`$`単独・`$$`・`$1a`（数字直後の識別子継続文字）は
  いずれも `LexError`（呼び出し元で `42601`）。桁数字が `u16::MAX` を超える
  場合は飽和させる（未定義動作にしない。上限超過自体は `sql::params` が
  `54000` で拒否する）。

### 許可リスト検証の分割（`sql::allowlist`）

- `pub fn validate_sql(sql, lookup)` は字句解析後 `validate_sql_tokens(&tokens,
  lookup)` へ委譲するだけの薄いラッパーである（挙動不変）。トークン列版
  `validate_sql_tokens(tokens: &[Token], lookup)` は COPY プロトコル（#939・
  WIRE-17）の内側 SELECT 検証と共有しており、`$n` 束縛経路（`core.rs` の
  `parse_tokens`）もこの入口を使う。

### `sql::params`（新設）

- `MAX_PARAMS = 64`。
- `validate_param_positions(tokens) -> Result<u16, SqlSurfaceError>`: 許可位置
  （下記）のみを構造的に受理し、最大の `$n` 番号（= 要求パラメータ数）を返す。
- `substitute_dummy(tokens) -> Vec<Token>`: 全 `$n` を固定ダミー `"0"` の
  `Token::StringLiteral` へ置換する（Parse 時点の構造検証専用。実行・Describe
  には使わない）。
- `substitute_values(tokens, values) -> Result<Vec<Token>, _>`: 実値で置換する
  （Bind 用）。
- `decode_bind_values(tokens, values) -> Result<Vec<String>, _>`: UTF-8 検証・
  NUL 拒否・**置換後総バイト数（`$n` の出現回数 × 対応する値の長さ、の総和）を
  値の複製より前に checked 演算で確定させ `lexer::MAX_INPUT_LEN`（1 MiB）と
  比較する**（同一 `$1` を多数回参照させて 1 回の値から巨大な置換結果を作らせる
  メモリ増幅対策）。

### `core.rs::EngineCore`

- `parse_sql` の本体を `parse_tokens(tokens: Vec<Token>)` へ抽出した（挙動
  不変。`parse_sql` は `tokenize(sql)?` してから委譲するだけ）。
- `pub fn parse_sql_prepared(&self, sql) -> Result<PreparedSql, _>`（Parse）:
  `tokenize_with_params` → `validate_param_positions` → `substitute_dummy` →
  `parse_tokens`（ダミー値束縛済みの `ParsedSql` を `PreparedSql::dummy_parsed`
  として Describe 専用に保持）。
- `pub fn bind_prepared(&self, prepared, values: &[Option<Vec<u8>>]) ->
  Result<ParsedSql, _>`（Bind）: 値件数検証 → `decode_bind_values` →
  `substitute_values` → `parse_tokens`。返る `ParsedSql` は既存の
  `execute_parsed_in_session`／`describe_parsed_in_session` へそのまま渡せる。
- `pub fn describe_prepared_in_session(&self, session, prepared) ->
  Result<Option<Vec<ColumnMeta>>, _>`（Describe(statement)）:
  通常の `describe_parsed_in_session` ではなく、その本体
  `describe_parsed_in_session_impl` へ `prepared.dummy_parsed` と、どの
  リテラル位置が `$n` 由来のダミー値かを示す 2 種のフラグを渡す。
  - `order_by_distance_literal_is_param`（`sql::params::
    order_by_distance_literal_is_param` が Parse 時点の元トークン列から判定）:
    `ORDER BY <vec列> <=> $n` のベクトル位置が `$n` 由来の場合に限り、
    ダミー値 `"0"` のベクトルリテラルとしての実パースを省略する。
  - `where_equality_dummy_flags`（`sql::params::
    where_equality_literal_is_param` が `WHERE` 等価述語ごとに判定）:
    `$n` 由来の等価述語に限り、ENUM 列ラベルの語彙照合（`22P02`）を省略する
    （crate 内限定の `bind_aggregate_with_dummy_flags`／
    `bind_projection_for_describe`／`bind_scan_with_dummy_flags` の
    いずれの経路でも同じ配列を共有する）。
  - これらのフラグは `PreparedSql`（フィールド非公開）が Parse 時点の元
    トークン列から計算した値に限られ、クレート外から指定する経路はない。
    公開 API の `sql::parser::bind_aggregate`／`bind_scan`・
    `EngineCore::describe_parsed_in_session` は従来どおりのシグネチャで
    フラグを受け取らず、常に全値検証を行う（PR #1012 codex-review P1）。

  省略するのは「ダミー値 `"0"` そのものの値に依存する検証」だけである。
  固定ダミーを通常どおり検証すると、正当なベクトル／ENUM 位置を持つ
  prepared statement まで Describe できなくなるためである。`$n` を含まない
  文や、`$n` と無関係な位置に書かれた実リテラルはフラグが `false` のまま
  となり、通常の Describe と同じく Describe 時点で検証される。省略された
  位置の値の妥当性（ベクトルリテラル形式・次元、ENUM ラベルの語彙）は、
  Bind まで遅延される。すなわち `bind_prepared` が実値で置換した
  `ParsedSql` を `describe_parsed_in_session`（フラグなしの通常 Describe）
  または `execute_parsed_in_session` へ渡した時点で、同じ値をリテラルで
  書いた SQL と同一のエラー契約（例: 語彙外 ENUM ラベルは `22P02`）で
  判定される（`crates/engine/tests/prepared_params.rs` の
  `bind_prepared_enum_where_equality_rejects_invalid_label_after_bind` で固定）。
  投影列の形はどの `$n` 値を束縛しても変わらない（列名・型は文の構造にのみ
  依存し、リテラル値の中身には依存しない）ため、ダミー値での導出結果は
  実値束縛後と常に一致する。LLM・埋め込み I/O は呼ばない（`USING PLAN` の
  既存 Describe 契約を維持）。

## 受理するプレースホルダ位置（規範形。これ以外は `42601`）

1. `ORDER BY <vec列> <=> $n`（ベクトル位置。直前トークンが `DistanceOp`）
2. `USING PLAN($n)`
3. `USING OPERATION_ID $n`（INSERT／TRUNCATE／DELETE／UPDATE 共通の文末句）
4. `WHERE <列> = $n`（トップレベル `WHERE` 節内側の等価条件のみ。`LIKE`・式
   比較・BOOLEAN 列条件は対象外）
5. `INSERT ... VALUES ($1, $2, ...)`（複数行含む。`VALUES` 節内の行リテラル
   位置のみ）

`LIMIT $n`・`USING MODE $n`・`HINT ORDER($n, ...)`・`SET search_mode = $n`・
`UPDATE ... SET a = $n`・`ON CONFLICT ... DO UPDATE SET a = $n`・hybrid 関数
引数（`HYBRID_RRF(col, $n, ...)`）・非等価比較（`id > $n` 等）はいずれも
`sql::params::validate_param_positions` の位置判定に含めておらず、構造的に
`42601` へ落ちる（`sql::params` の単体テスト・`crates/engine/tests/
prepared_params.rs` で固定）。

`WHERE` 節内の等価条件（パターン 4）と `SET`／`ON CONFLICT ... DO UPDATE SET`
（拒否対象）は同じ `Ident '=' $n` の字面を持つため、`WHERE` キーワードの
出現位置を境界とする範囲判定でのみ区別する（`UPDATE ... SET a = $n` の `SET`
節は `WHERE` キーワードより前に現れるため対象外、`UPDATE ... SET a = 'x'
WHERE b = $n` の `WHERE` 節内の等価条件は受理——後者は述語形 UPDATE／DELETE の
`WHERE` にも同じ判定がそのまま適用される）。

## 値の形式（型付き置換。Issue #1342）

置換トークンは `$n` の推論型から導く置換リテラル種別で決まり、字句解析器が同じ値を
リテラルで書いた SQL から生成するトークン列と同一になるよう組み立てる（リテラル
同値性を構造上保証する。第 2 の実行器・パーサーは作らない）。

| 種別 | 対象列 | 置換トークン | 形式不正 |
| ---- | ------ | ------------ | -------- |
| String | 上記以外（`TEXT`・`ENUM`・`VECTOR`・`NUMERIC`・`DATE`・`TIMESTAMP`・`UUID`・`BYTEA` 等・未参照番号） | `Token::StringLiteral` | なし（値の意味検証は束縛後の既存経路） |
| Integer | `id`・`INTEGER`・`BIGINT` | `Number`（負値は `Punct('-')` + `Number`） | `22P02` |
| Float | `REAL`・`DOUBLE` | `Number`（負値は `Punct('-')` + `Number`） | `22P02` |
| Boolean | `BOOLEAN` | `Ident("true"/"false")` | `22P02` |

- 整数の文法は `-?[0-9]+` のみ（`+` 符号・空白・小数・指数は fail-closed で拒否。
  PostgreSQL の `int4in` より狭い）。値域外は束縛後の既存経路がリテラル形と同じ
  コードで拒否する（`INTEGER` は `22003`、`id` の負値・u64 超過は `22000`）。
- 浮動小数（Issue #1406）の文法は字句解析器の数値トークン 1 個（符号なし）に完全一致
  する値（`1.5`・`.5`・`1.`・`1.5e3`）に、先頭の `-` を 1 個だけ許したもの。`+` 符号・
  前後空白・`NaN`・`Infinity`・`1e`・`1.5 OR 1=1` は `22P02`。判定は
  `sql::lexer::is_single_number_literal`（`lex_number` の規則の再利用）が担い第 2 の
  数値文法は持たない。REAL への値域外（`1e40` 等）は束縛後の既存経路がリテラル形と
  同じコード（INSERT は `22003`）で拒否する。
- `NUMERIC`・`DATE`・`TIMESTAMP`・`UUID`・`BYTEA` のリテラル形は文字列リテラルのため
  `String` 種別のまま列型別の既存束縛（Bind 後）で束縛され、束縛形とリテラル形は
  同一になる。形式不正のコードもリテラル形と同一（`NUMERIC`・`UUID`・`BYTEA` は
  `22P02`、`DATE`・`TIMESTAMP` は書式不正 `22007`・範囲外 `22008`。WIRE-12 の
  「リテラル形と同一判定」に従い、束縛経路専用の第 2 の日時検証器は作らない）。
  `NUMERIC` を `Float` 種別へ寄せてはならない（`WHERE <NUMERIC 列> = <数値リテラル>` は
  既存仕様で拒否されるため）。
- 真偽値は PostgreSQL の `boolin` 互換（前後空白除去・大文字小文字無視。
  `t`/`true`/`yes`/`on`/`1` 系と `f`/`false`/`no`/`off`/`0` 系。`o` 単独は曖昧なので拒否）。
- エラーメッセージは値本文を含めない（`$n` の番号と期待型のみ）。
- Parse は 2 段: 文字列ダミーで構造検証・型推論を行い、数値・真偽値スロットがあれば
  型付きダミー（`Number("0")`／`Ident("false")`）で再 parse して Describe 用の
  `dummy_parsed` とする。`WHERE <列> = $n` のダミーフラグ（ENUM 語彙照合の省略判定）は
  種別確定後に求め、非 String 種別の `$n` は序数に数えない（置換後は
  `WherePredicate::Equality` にならないため。数えると別述語へ省略が誤適用される）。
- 数値・真偽値スロットのバイナリ形式は Issue #1345 で受信形式の復号に置き換わった
  （バイナリを UTF-8 として誤解釈して受理しない）。
- 既知の制約: `WHERE <整数列・浮動小数列> = -N` はリテラル形でも式項の単項マイナス未対応で `42601`
  になり、`$n` に負値を束縛してもリテラル同値で同じ結果になる（`INSERT` の値位置は受理）。
- Parse 後に列型が変わった場合は種別が Parse 時点のままのため、既存の型不一致エラー
  （fail-closed）になる。
- `VECTOR`・`BYTEA` 列は `StringLiteral` 形のまま構造検証を通り、リテラル形式の妥当性は
  既存の `bind_insert`（Bind 後）が判定する。

## NULL パラメータ値

`decode_bind_values` は `None`（SQL NULL）を一律 `22000` で拒否する（本
バージョンのスコープ外）。したがって `USING OPERATION_ID $n` の `$n` に
「句の省略」に相当する値を束縛する手段は現状ない。

- 空文字列の束縛は省略と同義ではない。`OperationId::parse("")` は台帳の
  構成（`LedgerMode`）に関係なく常に `23502` を返す。一方、句の省略が
  `23502` になるのは台帳あり構成（`LedgerMode::Ledgered`）の場合だけで、
  台帳なし構成（`CompareOnlyWithoutLedger`）では省略が受理される。
- 省略相当を指定したい場合は、`$n` を使わない別の文として Parse する。
  具体的には `USING OPERATION_ID` 句そのものを書かないか、SQL テキストへ
  `USING OPERATION_ID NULL` を直接書く。後者は既存の許可リスト
  （`sql::allowlist::Parser::parse_operation_id_clause`。大小無視の `NULL`
  を句の省略と同じ `None` として扱う）が構文として受理するため `42601` には
  ならず、どちらの書き方も構成ごとに省略と同じ結果になる（台帳あり構成は
  `23502`、台帳なし構成は受理。RECOVER-1・TASK-92 の既存契約。
  `crates/engine/tests/prepared_params.rs` の
  `parse_sql_prepared_treats_literal_null_operation_id_as_omitted_clause`
  で固定）。`USING OPERATION_ID` の値そのものは、NULL 以外は文字列リテラル
  のみを受理する（数値・他の識別子は `42601`）。

## 検証

- `crates/engine/src/sql/lexer.rs`: `tokenize_with_params` の受理・拒否形状、
  `tokenize`（`allow_params=false`）が分割前後で不変であることを固定するテスト。
- `crates/engine/src/sql/params.rs`: 位置判定（受理 5 形・拒否 8 形以上）・
  パラメータ番号上限・ダミー/実値置換・NULL／非UTF-8／NUL バイト拒否・
  メモリ増幅対策の単体テスト。
- `crates/engine/tests/prepared_params.rs`:
  - **リテラル同値性**: 5 形それぞれで `bind_prepared` の結果が、同じ値を
    エスケープしたリテラル SQL の `parse_sql` 結果と `ParsedSql: PartialEq`
    で完全一致することを固定（束縛が「同じ値をリテラルで書いた SQL」と
    同一の判定になることの機械的な証明）。
  - **インジェクション耐性**: `' OR '1'='1`・`'); DROP TABLE ...; --`・
    改行・別の `$n` らしき文字列を含む値を束縛しても、単一の不透明な
    リテラルとして扱われ文の形が変わらないこと（実行結果・テーブルの
    存続を含む）。
  - 位置拒否（`42601`）・字句拒否（`$0`／`$01`／`$1a`）・番号上限
    （`54000`）・値検証（`22000`）・値件数不一致・RLS 暗黙適用（他テナントの
    行が混入しない）・Describe の同値性・Parse／Describe の副作用ゼロ・
    簡易クエリでの `$n` 拒否（既存契約）を固定。

## スコープ外・申し送り

- wire 側（Parse の宣言型受理・`ParameterDescription`・Bind の値保持・
  `bind_prepared` 結線）は #934 マージ後の別 PR。
- パラメータのバイナリ形式復号（WIRE-14）→ Issue #1345 で 7 型を実装済み。
- `REAL`／`DOUBLE`／`NUMERIC`／`DATE`／`TIMESTAMP`／`UUID`／`BYTEA` 列への型付き `$n` 束縛
  → Issue #1406 で実装済み（上記「値の形式」参照）。バイナリは `REAL`／`DOUBLE`／`UUID`／
  `BYTEA` のみ受理し、`NUMERIC`／`DATE`／`TIMESTAMP` は WIRE-14 どおり `0A000`、
  バイナリの `NaN`・`±Infinity` はリテラル形でも表現不能のため `22P02`。絶対値が非常に
  大きい浮動小数の `WHERE` 比較がリテラル形でも `22003` になる点は式評価側の既存挙動。
- 非 text スロットで宣言 OID 0 のバイナリ値を UTF-8 恒等で受理していた点は Issue #1345 で
  解消済み。
- NULL パラメータ値の意味論的な位置別処理（`USING OPERATION_ID $n` へ NULL を
  「省略と同義」として通す等）。
- 追加のプレースホルダ位置（`LIMIT $n`・非等価 WHERE 比較・`UPDATE ... SET
  col = $n`・`ON CONFLICT ... SET`・hybrid 関数引数・`LIKE $n`）。
- 層 B（psycopg 3／node pg／psql 実クライアントでの `make e2e-three-client`
  相当）→ **Issue #1176 で実施済み**（`crates/wire-server/tests/
  three_client_extended_e2e.rs`。`docs/design/three-client-e2e-harness.md`
  「拡張クエリ・型復元・バイナリ受信」参照）。
