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
- 宣言 OID は Issue #2436（WIRE-24 ポインタ）以降、値の検証にも用いる（後述「Parse の型 OID
  指定時の判定規則」。値は常にリテラル置換経路を通る点は不変）。0 以外は
  `ParameterDescription` へ echo し、0・未宣言は engine の型推論
  （`PreparedSql::param_types`。`ORDER BY <vec> <=> $n` は text 公告だがバイナリ不可の
  `VectorText`、`USING PLAN($n)`／`USING OPERATION_ID $n` は `Text`、`WHERE <列> = $n`
  ・`INSERT` の行内 ordinal は当該列の型、未参照番号は `Text`）で補う。
  pgjdbc が文字列に varchar（1043）を宣言する点を踏まえ、宣言 OID の不一致は拒否しない。
  （この 2 点は WIRE-24 の確定〔#2436〜#2438〕で、後述「Parse の型 OID 指定時の判定規則」（WIRE-24 ポインタ）へ置き換え済み）
- 宣言件数がプレースホルダ数を超える Parse は `08P01`（PostgreSQL は受理するが、
  fail-closed な逸脱）。
- Bind: 値数が要求数と異なれば `08P01`。パラメータ format code は 0・1 以外を `08P01`。
  binary は、text 系スロット（宣言 OID が 0・25・1043）は UTF-8 バイト恒等で、`int4`／`int8`／
  `float4`／`float8`／`bool`／`bytea`／`uuid` は PostgreSQL の受信形式で正規テキストへ復号して
  から `bind_prepared` へ渡す（Issue #1345・WIRE-14）。受理は推論スロット型と実効 OID（宣言が
  0 以外ならそれ、0 なら推論）の互換表（整数列は 23／20、`REAL` は 700、`DOUBLE` は 701、
  `BOOLEAN` は 16、`BYTEA` は 17、`UUID` は 2950、`NUMERIC` と `id` は宣言 0／1700〔Issue #2443・WIRE-26〕、`id` は宣言 23／20 も可）に載る組み合わせに限り、
  PostgreSQL の暗黙キャストより意図的に厳しい fail-closed とする。非対応スロット
  （日時・`JSON`・配列・`ENUM`・`VECTOR`・別名付き `id`・計算列等。`NUMERIC` 列・`id` は numeric として受理）への binary 指定は
  `0A000`（Parse 時の `0A000` とは別の意味。WIRE-24 参照）、値の長さが固定長型の受信形式と合わなければ `08P01`（値は応答に含めない）。判定順は
  「全スロットの非対応判定（`0A000`）→ 復号（`08P01`）」で、復号後の値で保持量上限を判定する。
- 値の形式不正（非 UTF-8・NUL）は `22P02` へ移行した（従来 `22000`）。NULL は #2439 で
  リテラル `NULL` と同値の束縛へ移行した（後述「NULL パラメータ値」節）。
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
   位置のみ。行の括弧の直下の要素単独位置に限り、`upper($1)`・`1 + $1` 等の式の内側はパターン 7。Issue #1543・#1546）
6. `UPDATE <表> SET <列> = $n [, <列> = $m ...]`（先頭語が `UPDATE` の文の `SET`
   節で、括弧深さ 0 の `(SET|,) <列> = $n (,|節末)` の形＝右辺全体が `$n` 単独の
   位置のみ。`a = a + $1`・`upper($1)` 等の式の内側はパターン 7。`$n` の型は対象列の型
   〔`WhereColumn` と同じ列型解決〕。Issue #1544・SQL-45 のポインタ。位置判定と
   型推論は `sql::params::set_param_slots` の 1 パスの表を共有する。行値の代入
   `SET (<列>, ...) = (<要素>, ...)`／`= ROW(...)` も Issue #1748 で同じ規則に対応し、
   列リストの位置で要素に列を割り当てる〔要素単独の `$n` はここ、式の内側はパターン 7。
   列と要素の数が違う・形が崩れた場合は登録せず `42601`。parser 側の受理は #2629〕）
7. 式の内側の `$n`（Issue #1546・SQL-45 期待欄 (6)・WIRE-12 のポインタ）。パターン 5 の
   VALUES 要素・パターン 6 の SET 右辺のうち `$n` 単独ではないもの（`n = n + $1`・
   `s = upper($1)`・`VALUES (1 + $1)`・`CASE WHEN m = $1 THEN ... END`）。SET 右辺は
   `(SET|,) <列> =` の境界から次の深さ 0 の `,` または節末（深さ 0 の `WHERE`／
   `RETURNING`／`USING OPERATION_ID|PLAN`）まで、VALUES 要素は深さ 1 の `,`／行の閉じ括弧
   までを 1 セグメントとし、セグメントごとに型を推論する（下記「式内 `$n` の型推論」）
8. `INSERT ... ON CONFLICT ... DO UPDATE SET <列> = <同じ列> + $n`（Issue #2427・SQL-49・
   WIRE-12 のポインタ）。括弧深さ 0 の `ON CONFLICT` 以降の `DO UPDATE SET` 節で、
   `(SET|,) <列> = <同じ列> + $n (,|節末)` の形だけを受理する（衝突対象は列リスト・
   `ON CONSTRAINT`・省略のいずれでも可。節末は深さ 0 の `WHERE`／`RETURNING`／
   `USING OPERATION_ID|PLAN`）。`$n` の型は INSERT 対象表の代入先列の型（列リスト省略形は
   表の宣言順の列）。詳細は下記「ON CONFLICT 加算式の `$n`（パターン 8）」

9. `SELECT ... WHERE <列> (< | > | <= | >= | <>) $n`・`WHERE <列> [NOT] BETWEEN $a AND $b`
   （Issue #2430・#2431・SQL-78・SQL-24・WIRE-12 のポインタ）。先頭語が `SELECT`／`UPDATE`／`DELETE`
   で集合演算を持たない文〔`INSERT ... ON CONFLICT DO UPDATE ... WHERE` は対象外〕の
   `WHERE` 節内に限る（述語形 `UPDATE`／`DELETE` は Issue #2431。Describe は `WHERE` を束縛しないため
   ダミーフラグは関係せず、台帳の内容照合〔RECOVER-11〕は `$n` に値を代入した後の文で行うため、
   同じ値のリテラル形と同じハッシュになる）。`$n` の型は比較相手の列の型（疑似列 `id` は `id` の型）で、表・列を引けなければ
   `text`（存在を応答コードで観測させない）。`$n op $m`（両辺とも `$n`）は型を決められず、同じ番号の
   別の出現で決まればその型、無ければ宣言型、それも無ければ Parse で `42P18`（構文エラー `42601` が先）。
   判定は `sql::params::noneq_leaf_at` の 1 関数に集約し、受理・型の分類・Describe 用ダミーフラグの
   3 箇所が共有する（`1 + col > $1`・`col > $1 + 1`・`col > $1::int` を葉と取り違えないため、葉の前後の
   境界も検査する）。置換後のトークン列は既存の構造葉（`Compare`・`Between`・`Not(Equality)`）または
   数値の式述語になり、第 2 の実行器・パーサーを作らない。
   - **範囲外（`42601` のまま）**: 逆向き `$n op <列>`（置換後が式述語へ回り、Describe のダミー値を
     省略する仕組みを通らない）、片側だけが `$n` の `BETWEEN`（1 葉 1 フラグのため実リテラル側の
     Describe 検証が省略される）、左辺が `$n` の `BETWEEN`、相手がリテラル・関数・式の比較、
     集合演算（枝ごとにフラグの序数が 0 から始まる既存の制約）、
     `LIKE`・`IN` リスト
   - **序数の不変条件**: トークン走査で宣言的な葉（`Ident op (文字列リテラル|String 種別の $n)` の
     `Equality`／`Not(Equality)`／`Compare`、文字列の `Between`）を 1 つ見つけるごとにフラグを 1 つ積み、
     束縛側（`sql::parser::bind_where_predicates_recursive`）はその葉をどの経路（宣言的・数値列の
     書き換え・TEXT 範囲の式化・BOOLEAN の書き換え）で処理しても序数を 1 つ進める。
     数値・真偽値種別の `$n` は式述語になるため数えない

`LIMIT $n`・`USING MODE $n`・`HINT ORDER($n, ...)`・`SET search_mode = $n`・
`ON CONFLICT ... DO UPDATE SET a = $n`（`$n` 単独の右辺。加算形はパターン 8）・hybrid 関数
引数（`HYBRID_RRF(col, $n, ...)`）・パターン 9 以外の非等価比較（`id > $n` 等）はいずれも
`sql::params::validate_param_positions` の位置判定に含めておらず、構造的に
`42601` へ落ちる（`sql::params` の単体テスト・`crates/engine/tests/
prepared_params.rs` で固定）。

`WHERE` 節内の等価条件（パターン 4）と `SET`（パターン 6）は同じ
`Ident '=' $n` の字面を持つため、`WHERE` キーワードの出現位置を境界とする範囲判定で
区別する（`UPDATE ... SET a = $n` の `SET` 節は `WHERE` キーワードより前に現れ、
パターン 6 の表で判定する。`UPDATE ... SET a = 'x' WHERE b = $n` の `WHERE` 節内の
等価条件はパターン 4 で受理——後者は述語形 UPDATE／DELETE の `WHERE` にも同じ判定が
そのまま適用される）。`ON CONFLICT ... DO UPDATE SET` は先頭語が `INSERT` のため
パターン 6 の対象外（加算形だけがパターン 8 で受理され、それ以外は拒否のまま）。

## 式内 `$n` の型推論（パターン 7。Issue #1546）

推論は構造検証 parse の**前**に、トークン列とカタログだけで行う（式内 `$n` は
文字列ダミーでは定数畳み込み〔VALUES〕や束縛の型検査〔SET〕に落ちるため）。型の確定後に、
確定した置換リテラル種別の型付きダミーで 1 回だけ構造検証 parse する。`INSERT` の表名・列リストは
トークン列（`INSERT INTO <表> ( <列>, ... ) VALUES`）から読む。

セグメントの式木は既存の `sql::allowlist::Parser` を再利用して得る
（`allowlist::parse_param_inference_expr`。第 2 のパーサーは作らない）。`$n` は parse 前に
字句解析器が生成し得ない擬似識別子（`$` 始まり）へ置き換えるため、`Expr` に専用 variant は
足さず、実行・束縛・評価の経路へ `$n` 由来の構文木が入る余地はない。

| 形 | `$n` の型 |
| -- | --------- |
| 右辺・要素の全体が括弧付きの `$n` 単独 | 代入先の列の型 |
| `<A> op $n`／`$n op <A>`（`+ - * /` と `= < > <= >=`） | 相手 `<A>` の型（列参照は列の型、整数リテラルは `INTEGER`、i32 を超えれば `BIGINT`、小数・指数は `DOUBLE PRECISION`） |
| 組み込み関数のテキスト引数（`upper`／`lower`／`trim`／`length`／`replace`／`position`／`concat`／`substr` の第 1 引数） | `TEXT` |
| 組み込み関数の数値引数（`abs($1)`・`substr(s, $1)` の第 2 引数等） | 推論不能（`42P18`） |
| 組み込みでない関数（登録 UDF 等）の引数 | 未解決（`Text`。関数の存在を応答コードで観測させない） |
| `CASE` の THEN／ELSE・`COALESCE`・`NULLIF` の `$n` | 兄弟のうち型が決まるものの型 |
| `$1 + $2`・`COALESCE($1, $2)` のようにどこからも決まらない | 推論不能（`42P18`） |

- 代入先の列の型は式の内側へ押し下げない（PostgreSQL と同じく演算の相手から決める）
- `42P18`（`IndeterminateDatatype`）にするかどうかは**構文の形だけ**で決める。相手が列参照なら
  「決定可能」として扱い、表・列がスキーマに無ければ `Text` へ倒す（表・列の存在で Parse の
  応答コードが変わる経路を作らない。未知の列は既存の構造検証・束縛段のエラーで返る）
- 同じ番号が複数位置に現れる場合は既存の統合規則を維持し、推論不能の出現は無視する。
  全出現が推論不能のときだけ `42P18`。出現をまたいだ型の伝播（`$1 + $2` の `$2` を別の位置で
  決まった型で埋める等）はしない
- 構文エラー（`42601`）は `42P18` より先に返す（PostgreSQL と同じ順序）
- 推論した型が `INTEGER`／`BIGINT`／`REAL`／`DOUBLE`／`TEXT`／`VARCHAR`（と `id`）以外
  （`BOOLEAN`・`DATE`・`TIMESTAMP`・`NUMERIC` 等）の `$n` は、式の中では置換後のリテラルがその型として
  束縛されないため `0A000` で fail-closed にする
- エラー文言には `$n` の番号だけを含める（値・表の存在情報は含めない）
- 既知の PG 差分: Parse の宣言 OID（0・705 以外で本 DB が持つ型）は #2436 で engine に渡り、推論不能な位置は宣言型で解決する（`42P18` は宣言が無い番号だけ）。宣言済みの `$k` を式内の別の `$m` の推論の相手として伝播させること（`$1::int4 + $2` で `$2` を int4 にする等）はしない（OID 0 の推論は不変）。
  小数リテラルとの演算は `DOUBLE`（PG は numeric）。`-$1` は `0 - $1` に脱糖されるため
  `INTEGER`（PG では曖昧）。`INSERT ... VALUES` の `-$1` に負の実値を Bind すると `- -n` となり
  `42601` で fail-closed（既存の二重マイナス拒否と同じ契約）。`ON CONFLICT ... DO UPDATE SET` の加算式は Issue #2427 で受理（下記パターン 8 の節）、それ以外の式と `$n` は `42601` のまま

## ON CONFLICT 加算式の `$n`（パターン 8。Issue #2427）

SQL-49・WIRE-12 のポインタ。#2425 の加算式 `<列> = <同じ列> + <数値リテラル>` の数値リテラル位置を
`$n` にできる。Bind 後の文は同じ値をリテラルで書いた SQL と同一になるため、加算・範囲検査・
台帳（`operation_id` の内容照合は置換後の文）は #2425 の経路がそのまま担う。

- **型推論**: `$n` の型は INSERT 対象表の代入先列（SMALLINT／INTEGER／BIGINT／REAL／DOUBLE は
  OID 21／23／20／700／701 を ParameterDescription で公告）。表・列が引けない場合は `Text` へ倒す
  （Parse で存在を観測させない）。
- **Parse の宣言型**: Operator 規則（text／bool 等は `42883`）に加え、整数の代入先へ
  float4／float8／numeric を宣言した場合は `0A000`（整数列への非整数リテラルの加算と同じ）。
- **数値でない代入先列**: 列が引けた場合に限り、リテラル経路が束縛段で返す `42883`（NUMERIC は
  `0A000`）を Parse で返す。
- **Bind**: 整数スロットへ `-?[0-9]+` 以外は `22P02`、和が範囲外は Execute で `22003`（副作用なし）。
- **拒否（`42601`）**: `SET a = $n`（`$n` 単独の右辺）・`n = m + $1`・`$1 + n`・`n = n + $1 + 1`・
  `-$1`・`$1::int`・`EXCLUDED.n + $1`。
- **既知の PG 差分**: NULL の Bind はリテラル `n + NULL` と同じ `42601`（PG は NULL を受理）。
  `$n` 単独の右辺と一般式中の `$n` は未対応（#1738）。

## Parse の型 OID 指定時の判定規則（Issue #2434・#2435。TASK-337・WIRE-24 ポインタ）

ステータス: 設計記録の追記（判定規則の確定は spec 側のビヘイビア WIRE-24 が SSOT）。
本節はポインタ表記のみで、判定の詳細は spec 本文を参照する（転記しない）。

- Parse で 0 以外の型 OID が宣言された場合の判定は WIRE-24 に従う。関連: WIRE-12・WIRE-13・
  WIRE-14、SQL-41・SQL-45・SQL-47、ERR-6。
- 実装の分担: #2436（OID の公告・復号）・#2437（変換可否。#1588 に依存）・#2438（持たない型の拒否）・
  #2439（NULL）・#2440（テスト）。
- 「持つ型」の単一情報源は `crates/wire-server/src/result_encoder.rs` の `WireType::ALL` とし、
  第 2 の表を作らない。
- OID 0（未宣言を含む）の推論と公告は変えない。拒否時は ErrorResponse を返して同期回復し、
  接続は維持する（WIRE-11 の既存経路）。
- 新しい `wire_code` は追加せず、既存の `ErrorClass` と HTTP 射影を使う。エラー文には値・
  表の存在情報を含めず、テナント境界を越える情報を返さない。
- 既存テストが固定している寛容な挙動の更新は #2436・#2437 の担当が行う（削除・弱体化はしない）。

### #2436 の決定事項（宣言 OID の公告と値の復号。WIRE-24・WIRE-12・WIRE-14 ポインタ）

- 運搬: wire-server が宣言 OID を `WireType::from_oid` で engine の `DeclaredParamType` へ写し
  （写像は `WireType::declared_param_type` の網羅 `match` のみ）、
  `EngineCore::parse_sql_prepared_with_declared` へ渡す。既存の `parse_sql_prepared` は宣言なしの
  薄いラッパー。`PreparedSql::declared_param_types` が宣言型を、`param_types` が推論型を、
  並べて保持する（上書きしない。#2437 が両方を見て変換可否を判定する）。
- 0 と unknown（705）は宣言なしとして推論・公告する。
- 持たない型の OID（#2438 の決定。WIRE-24 (1)・WIRE-11・ERR-6 ポインタ）:
  - 「持つ型」の判定元は `WireType::ALL`（`WireType::from_oid`）のみ。第 2 の表は作らない。
    それ以外の OID（負値・配列型・存在しない OID を含む）は Parse で `0A000`
    （`HandlerError::UnsupportedParamType`。message は `$n` 番号のみで OID の値・表名・テナントを含めない）。
    新しい `wire_code` は追加しない。
  - 判定位置と順序: `25P02`（Failed トランザクション）→ 宣言件数上限 `54000` → 空文 + 宣言の
    `08P01` → **持たない型の `0A000`（OID 復号の直後・engine の parse より前）** → engine の parse
    （`42601`・`42P18` 等）→ 過剰宣言の `08P01`。判定が宣言 OID だけで決まるためカタログに依存せず、
    他テナントの表の存在を観測できない。推論不能位置が `42P18` に化ける経路も塞ぐ。
  - 回復: 既存の ignore-till-sync に乗り、接続は維持する。拒否した Parse は statement を保存せず、
    同名の既存 statement も変更しない。
  - 既知の PostgreSQL 差分: 構文エラー文に持たない型の OID がある場合、PostgreSQL は構文エラーを
    先に返すが本実装は `0A000` を先に返す。過剰宣言位置の持たない型も `0A000`（`08P01` より先）。
    存在しない型 OID も `0A000` に統一する。
  - クライアント影響: 配列型 OID を宣言するドライバ（例: 配列パラメータ）は Parse で `0A000` になる。
    スカラーの主要 OID と、OID 0 を送るクライアントは影響しない。実クライアントでの確認は #2440 で
    完了した（psycopg の `list[int]`＝1005・node の `types=[1007]` は `0A000`、同一接続の後続文は成功）。
- スロット型: 推論型がある位置は推論型のまま。推論型が無い位置（式内の全出現が推論不能・
  未参照番号）は宣言型を `PreparedParamType::Declared` としてスロット型にし、`42P18` を返さない。
  式の中で扱えない宣言型（`BOOLEAN`・日時・`NUMERIC` 等）は推論時と同じ `0A000`。
- 値の検証: Bind はテキスト形式の正規テキストを置換の前に宣言型として検証する。整数系は
  既存の整数文法と値域（不正 `22P02`・範囲外 `22003`）、浮動小数系は既存の浮動小数文法
  （`float4` は有限性も確認）、`bool` は `parse_pg_bool`。text 系と `numeric`・日時・`bytea`・
  `uuid`・`json(b)` は、単独で呼べる検証器を第 2 の文法として持たない方針により追加検証をせず、
  束縛後の既存リテラル経路に委ねる。エラー文言は `$n` 番号と型名だけで値を含めない。
- バイナリ: `Declared` スロットは宣言型の受信形式を受理する（`NUMERIC`・日時・`JSON` は
  未対応のまま `0A000`）。推論スロット側の互換表は変更しない（#2437 の判定は Parse の段で
  その手前に入り、バイナリ形式の受理範囲は据え置く。下記「#2437 の決定事項」）。
- 受理範囲: 宣言型として値が不正な場合の拒否が増え、推論不能位置と未参照番号の宣言型が
  受理に変わる。それ以外は据え置く（fail-closed）。新しい `wire_code` は追加しない。

### #2437 の決定事項（宣言型と文脈の型の変換可否。WIRE-24 (1)(5)・SQL-41・SQL-45 ポインタ）

- 規則の拠り所は [cast-conversion-rules.md](cast-conversion-rules.md) の規則表を
  データ化した `sql::cast` の `cast_rule` で、第 2 の規則表は持たない。判定は
  `sql::param_coerce`（新設）が担い、`EngineCore::parse_sql_prepared_with_declared` が
  構造検証 parse の成功後に呼ぶ。そのため `42601`・`42P01`・`42703`・`42P18` は
  型エラーより先に返る。宣言が無い番号は判定しない。
- 位置の種別と受理条件:
  - 代入（`SET c = $n`・`INSERT VALUES`・括弧付き `$n` 単独）: 規則が恒等・暗黙・代入・
    入出力代入のとき受理。それ以外は `42804`。
  - 比較・演算子（`WHERE c = $n`・ベクトル位置・式内の `<A> op $n`）: どちらかの向きで
    恒等・暗黙のとき受理（PG は型をまたぐ演算子を持ち、暗黙の変換でどちらかに寄せて解決
    できるため対称にする）。それ以外は `42883`。式内の相手 `<A>` は列参照・数値リテラル・
    それらの算術・型を明示した組み込み関数に限り、文字列リテラルと戻り型を近似で置く関数は
    判定しない。
  - 組み込み関数の文字列引数: 宣言型から `text` が恒等・暗黙のとき受理。それ以外は `42883`。
    `concat` は PG で任意型を受けるため除外する。
- 文脈の型は ParameterDescription で公告する型と一致させる（`id` は numeric、ベクトル・enum・
  配列列と型なしは text）。公告した OID をそのまま宣言したクライアントが拒否されないことを
  不変条件とし、テストで固定する。
- 判定対象外（従来どおり）: `USING PLAN($n)`・`USING OPERATION_ID $n`（本 DB 固有の句で、
  文字列への代入とみなすと常に受理になる）、登録 UDF の引数、文脈の表・列がカタログで引けない
  位置、宣言型をスロット型にした位置、`CASE`／`COALESCE`／`NULLIF` の兄弟枝・`CAST($n AS T)`・
  `COLLATE`・jsonb 演算子・SQL/JSON 関数の被演算子。文脈が引けない位置を判定しないのは、
  宣言の有無で応答コードが変わる存在オラクルを作らないため。
- エラー文言は `$n` の番号と静的な型名だけで組み立てる。新しい `wire_code` は追加しない。
  同じ `$n` が複数の位置に現れる場合は番号の昇順（番号内は位置、式の順）で最初の不適合を返す。
- 既知の差分: Bind 段の値の変換は変えない（例: INTEGER 列に `float8` を宣言して小数を送ると
  `22P02`。PG は丸める）。`binary_param_kind` の互換表も据え置く。兄弟枝・CAST・COLLATE の
  判定は行わない（PG では兄弟枝は `42804`、CAST の対応なしは `42846`）。JSON の `=` は PG に
  無いが規則表では恒等として受理する。層 A の網羅と実クライアントの層 B は #2440 で実施済み
  （[three-client-e2e-harness.md](three-client-e2e-harness.md) の該当節）。psycopg の自然な binary
  送信のうち互換表に載らない組（`float`→REAL・`date`→DATE 等）は `0A000` のまま据え置く。

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

Bind 値 `None`（SQL NULL）は、`sql::params` が裸の `Token::Ident("NULL")` 1 個へ
置換する（#2439・TASK-337・WIRE-24 ポインタ）。`QuotedNull` は使わない。値は SQL
テキストへ連結せずトークンとして直接生成する。受理／拒否と `wire_code` は
後段の許可リストパーサーが「同じ位置に SQL テキストで `NULL` と書いた場合」と
同一の規則で決めるため、`params.rs` に位置別の NULL 分岐は置かない。従来の
一律 `22000` は廃止した（`bind_prepared` の挙動変更。公開関数
`decode_bind_values`／`substitute_values` の署名は不変で、前者は `None` を含む入力に
従来どおり `22000` を返すテキスト専用ラッパーとして残る）。

- 型なしのリテラル `NULL` として扱う。宣言 OID と組み合わせた型付き NULL は #2436／#2437
  の範囲。
- `USING OPERATION_ID $n` への NULL は、既存契約の `USING OPERATION_ID NULL`（句の省略と
  同義。台帳あり構成 `23502`、台帳なし構成は受理。RECOVER-1・TASK-92）をそのまま継承する。
- 実装時点の事実: リテラルの `<列> = NULL` はベクトル `<=>`・`USING PLAN(NULL)` と同様、
  現状では実行時に拒否される位置があり（TEXT 等の `= NULL` は `42703`）、NULL パラメータも
  同じ `wire_code` で拒否される。`INSERT ... VALUES`・`UPDATE ... SET`・式内では受理される。
- 同値性は `crates/engine/tests/prepared_params.rs` の
  `bind_prepared_null_matches_literal_null_per_position` で固定する。
- wire 層の第 1 パスは、binary 非対応スロットに binary format が指定された場合、値が NULL でも
  `0A000` を先に返す（既存の fail-closed）。PostgreSQL では NULL に format は無関係なため
  差分になりうる。#2440 で現行の fail-closed を層 A に固定した（仕様準拠としての固定ではない）。
- パターン 9（非等価・BETWEEN・述語形 DML）の NULL も同じ経路で、リテラル形と同値
  （Issue #2432・TASK-336・SQL-78・WIRE-12 ポインタ。プロダクションコードの変更なし）。
  engine 側は `bind_prepared_null_matches_literal_null_in_noneq_between`、wire 側は
  `crates/wire-server/tests/wire_sql78_noneq_between_param.rs` で固定する。同ファイルは
  `ParameterDescription` の公告（列型の OID。順序比較が型として拒否される BOOLEAN の `<` 等と
  TEXT の BETWEEN は Parse で `22000`、存在しない列・表は `42703`／`42P01`、`$n op $m` は
  宣言なしで `42P18`、宣言 OID は echo）も現行挙動として固定する。

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
    （`54000`）・値検証（`22P02`）・NULL のリテラル同値・値件数不一致・RLS 暗黙適用（他テナントの
    行が混入しない）・Describe の同値性・Parse／Describe の副作用ゼロ・
    簡易クエリでの `$n` 拒否（既存契約）を固定。

## UPDATE／DELETE の `$n`（Issue #1748。SQL-50 (4)・SQL-45 (6)・WIRE-12 のポインタ）

- `UPDATE`／`DELETE` の `WHERE <列> = $n` は SELECT と同じ規則（パターン 4）で、表は
  `UPDATE <表>`／`DELETE FROM <表>` から引く。`SET` の右辺はパターン 6・7（行値の代入を含む）。
- 修飾付きの列・`IN`・`$1 = 列`・`LIKE`・`$n::型` は SELECT の WHERE と同じく `42601`。
  非等価の比較と `[NOT] BETWEEN` は #2429（SQL-78）の担当で、本 Issue では `42601` のまま。
- wire 層に UPDATE／DELETE 専用の分岐は置かない。型不一致は既存の `22P02`／`08P01`／`0A000`
  で fail-closed に拒否し、値は応答に含めない。
- テスト: `crates/engine/tests/sql50_update_delete_params.rs`（リテラル同値性・拒否形・RLS・
  明示トランザクション）、`crates/wire-server/tests/wire_sql50_update_delete_param.rs`
  （text／binary の Bind・型不一致・テナント境界）、`sql::params` の単体テスト（行値の位置判定）。

## スコープ外・申し送り

- wire 側（Parse の宣言型受理・`ParameterDescription`・Bind の値保持・
  `bind_prepared` 結線）は #934 マージ後の別 PR。
- パラメータのバイナリ形式復号（WIRE-14）→ Issue #1345 で 7 型を実装済み。
- `REAL`／`DOUBLE`／`NUMERIC`／`DATE`／`TIMESTAMP`／`UUID`／`BYTEA` 列への型付き `$n` 束縛
  → Issue #1406 で実装済み（上記「値の形式」参照）。バイナリは `REAL`／`DOUBLE`／`UUID`／
  `BYTEA` のみ受理し（当時。`NUMERIC` は Issue #2443 で受理）、`DATE`／`TIMESTAMP` は WIRE-14 どおり `0A000`、
  バイナリの `NaN`・`±Infinity` はリテラル形でも表現不能のため `22P02`。絶対値が非常に
  大きい浮動小数の `WHERE` 比較がリテラル形でも `22003` になる点は式評価側の既存挙動。
- 非 text スロットで宣言 OID 0 のバイナリ値を UTF-8 恒等で受理していた点は Issue #1345 で
  解消済み。
- TASK-337 の分担: #2436 OID の公告・復号、#2437 変換可否（#1588 に依存）、#2438 `0A000`、
  #2439 NULL、#2440 層 A/B（実施済み）。OID 705 は #2440 の層 B で psycopg・node pg とも OID 0 と同じ
  推論になることを確認した。
- NULL パラメータ値は #2439 で解消済み（上記節）。リテラル `<列> = NULL` 自体が未対応の
  位置（`42703`）は SQL-41 系の別課題。
- 追加のプレースホルダ位置（`LIMIT $n`・非等価 WHERE 比較〔SELECT は Issue #2430、UPDATE／DELETE は #2431 で受理済み〕・`ON CONFLICT ... SET`（加算形 `n = n + $1` は Issue #2427 で受理済み）・
  hybrid 関数引数・`LIKE $n`。`UPDATE ... SET col = $n` は Issue #1544 で受理済み）。
- 層 B（psycopg 3／node pg／psql 実クライアントでの `make e2e-three-client`
  相当）→ **Issue #1176 で実施済み**（`crates/wire-server/tests/
  three_client_extended_e2e.rs`。`docs/design/three-client-e2e-harness.md`
  「拡張クエリ・型復元・バイナリ受信」参照）。
