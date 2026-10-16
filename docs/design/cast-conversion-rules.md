# CAST の変換規則表と SQLSTATE 写像（Issue #1589）

- ステータス: Accepted（兄弟 Issue の実装で差分が出たら本書を更新する）
- 対象: SQL-47（`docs/spec/04-behavior/sql-surface.md`）。関連: TABLE-13・TABLE-14・
  TABLE-19・TABLE-22・TABLE-25・TABLE-29〜35・TABLE-39・TABLE-40・INDEX-9・TASK-271
- 親 Issue: #1588（CAST と `::` 演算子の追加）。兄弟: #1590（構文解析・束縛）・
  #1591（数値・文字列・BOOLEAN の変換）・#1592（日付時刻・JSON・UUID・BYTEA・配列の変換）・
  #1593（`::` の脱糖・優先順位）

本書は実装上の規則表と設計判断のみを記す。private spec 本文は転記せず、ビヘイビア ID・
タスク ID のポインタ表記に限る（`.claude/rules/spec-confidentiality.md`）。規則の根拠は
**PostgreSQL 17 の `pg_cast` カタログと各型の入出力関数の挙動**（公開情報）であり、
表と例は PostgreSQL 17.11 の実機（`postgres:17` コンテナ）で採取した。

## 背景・目的

現状、`CAST(x AS t)` は `AS` の位置で、`x::t` は字句段で、いずれも `42601` として
拒否される（CAST 未実装）。兄弟 Issue（#1590〜#1593）が実装する変換は、「型 × 型ごとに
変換できるか・どの区分か・失敗時にどの SQLSTATE と文言を返すか」を共通の前提にする。
この前提が Issue ごとにばらけると SQLSTATE や文言がずれるため、単一の拠り所として
本書に規則表と写像を置く。

## 凡例

| 記号 | 意味 |
| ---- | ---- |
| `i` | 暗黙（implicit）の変換。式・代入・明示のいずれの文脈でも使える |
| `a` | 代入（assignment）の変換。代入文脈と明示の CAST で使える |
| `e` | 明示（explicit）の変換。明示の `CAST`／`::` でのみ使える |
| `io/a` | 文字列表現を介した入出力変換（各型 → 文字列型）。代入と明示で使える |
| `io/e` | 文字列表現を介した入出力変換（文字列型 → 各型）。明示でのみ使える |
| `=` | 同一型。型修飾子（長さ・精度）の適用のみ。値の変換はない |
| `elem` | 配列 → 配列。要素型の変換規則に従い要素ごとに適用する |
| `×` | 変換の対応なし |

- `×` は明示の `CAST`／`::` で要求された場合に **`42846`（cannot_coerce）** を返す。
  暗黙・代入の文脈で変換が存在しない場合は PostgreSQL と同じく従来どおり、演算子・関数の
  解決失敗は `42883`、代入の型不一致は `42804` であり、`42846` にはしない
- `42846` は型の組だけで決まる**束縛時**のエラーで、値が NULL でも（`NULL::uuid::integer`）、
  値が不正でも（`'abc'::text::uuid::int4` は入力値の検査より前に `42846`）同じく出る。
  値に依存するエラー（`22P02`・`22003` 等）は**評価時**に出る
- 型なしリテラルと text 型の式は区別する。`'abc'::integer` や `DATE '2026-10-10'` の
  型なしリテラルは、変換先型の入力関数で直接解釈される。text 型の式（列・関数結果）からの
  変換は表の `io/e` 経由である。どちらも明示の CAST では許可され、不正形式は同じ SQLSTATE
  になるが、束縛の経路が異なる（#1590 の束縛設計の前提）
- 対応のある組で入力が NULL のときの結果は NULL

## 規則表（20 型 × 20 型）

行が変換元、列が変換先。型名は内部表記で、`int2`＝SMALLINT・`int4`＝INTEGER・
`int8`＝BIGINT・`float4`＝REAL・`float8`＝DOUBLE PRECISION・`bool`＝BOOLEAN・
`bpchar`＝CHAR・`array`＝配列（要素型は任意）。`varchar`・`bpchar` と `numeric` の
`=` は型修飾子の適用で、規則は後述する。

| 変換元＼変換先 | int4 | int8 | int2 | float4 | float8 | numeric | bool | text | varchar | bpchar | date | time | timestamp | timestamptz | interval | json | jsonb | uuid | bytea | array |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| int4 | = | i | a | i | i | i | e | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | × |
| int8 | a | = | a | i | i | i | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | × |
| int2 | i | i | = | i | i | i | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | × |
| float4 | a | a | a | = | i | a | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | × |
| float8 | a | a | a | a | = | a | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | × |
| numeric | a | a | a | i | i | = | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | × |
| bool | e | × | × | × | × | × | = | a | a | a | × | × | × | × | × | × | × | × | × | × |
| text | io/e | io/e | io/e | io/e | io/e | io/e | io/e | = | i | i | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e |
| varchar | io/e | io/e | io/e | io/e | io/e | io/e | io/e | i | = | i | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e |
| bpchar | io/e | io/e | io/e | io/e | io/e | io/e | io/e | i | i | = | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e | io/e |
| date | × | × | × | × | × | × | × | io/a | io/a | io/a | = | × | i | i | × | × | × | × | × | × |
| time | × | × | × | × | × | × | × | io/a | io/a | io/a | × | = | × | × | i | × | × | × | × | × |
| timestamp | × | × | × | × | × | × | × | io/a | io/a | io/a | a | a | = | i | × | × | × | × | × | × |
| timestamptz | × | × | × | × | × | × | × | io/a | io/a | io/a | a | a | a | = | × | × | × | × | × | × |
| interval | × | × | × | × | × | × | × | io/a | io/a | io/a | × | a | × | × | = | × | × | × | × | × |
| json | × | × | × | × | × | × | × | io/a | io/a | io/a | × | × | × | × | × | = | a | × | × | × |
| jsonb | e | e | e | e | e | e | e | io/a | io/a | io/a | × | × | × | × | × | a | = | × | × | × |
| uuid | × | × | × | × | × | × | × | io/a | io/a | io/a | × | × | × | × | × | × | × | = | × | × |
| bytea | × | × | × | × | × | × | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | = | × |
| array | × | × | × | × | × | × | × | io/a | io/a | io/a | × | × | × | × | × | × | × | × | × | elem |

### 配列行・列の読み方

| 組 | 規則 |
| -- | ---- |
| 配列 → 配列 | 要素型の組が `×` でなければ許可（`elem`）。要素のエラーはその SQLSTATE のまま返す。要素型の組が `×` なら `42846` |
| 配列 → text／varchar／bpchar | `io/a`（配列リテラル文字列） |
| text／varchar／bpchar → 配列 | `io/e`（配列リテラルの解析。不正形式は `22P02`） |
| スカラー ⇄ 配列 | `×`（`42846`。例: `1::int4[]`、`'{1}'::int4[]::int4`） |

### セルキーの命名規則

テストは**セルキー `<変換元>-><変換先>`** で命名し、全 400 セルを網羅する（親 #1588 の
受入条件）。型名は表の見出しの語を使う（例: `int4->bool`・`text->uuid`・`jsonb->int8`・
`array->array`）。`×` のセルは `42846` の確認、`=` のセルは型修飾子の適用のみの確認とする。

## 系統ごとの値レベルの規則

以下はすべて PostgreSQL 17.11 実機での確認結果。

### 数値間（int2／int4／int8／float4／float8／numeric）

- 拡大は `i`、縮小は `a`。float4 → numeric、float8 → numeric は `a`
- 整数への丸め: float → 整数は**偶数丸め**（`2.5::float8::int4` は 2、`3.5` は 4）、
  numeric → 整数は**四捨五入**（`2.5` は 3、`-2.5` は -3）
- 範囲外は `22003`: `smallint out of range`・`integer out of range`・
  `bigint out of range`、float8 → float4 は `value out of range: overflow`／`underflow`、
  `numeric(p,s)` の精度あふれは `numeric field overflow`
- float8 の NaN → 整数は `22003`（`integer out of range`）
- numeric の NaN／Infinity → 整数は **`0A000`**（`cannot convert NaN to integer`・
  `cannot convert infinity to integer`）。NaN／Infinity を表せない NUMERIC での
  float → numeric の扱いは #1591 で決める（本書では決めない）

#### numeric の型修飾子 `numeric(p,s)`（`numeric → numeric` の `=`）

PostgreSQL 17 の挙動（公開情報）に従う。

- 適用順は、(1) スケール `s` へ**四捨五入**（0 から遠い側へ丸める。`1.25` は `numeric(3,1)` で
  `1.3`、`-1.25` は `-1.3`。float の偶数丸めとは異なる）、(2) **丸め後**の値で精度を判定、の順
- 丸め後の整数部の桁数が `p - s` を超えると `22003`（`numeric field overflow`）。
  例: `99.95::numeric(3,1)` は丸めで `100.0` となり整数部 3 桁が `p - s = 2` を超えるため
  `22003`。丸め前の桁数では判定しない
- NaN は修飾子を問わず通る。Infinity は有限の `p`・`s` で表せないため `22003`
- スケールが精度を超える指定（`s > p`。例: `numeric(3,5)`）も有効で、`p - s` が負になる。
  このとき小数点以下の先頭 `s - p` 桁が 0 であることを要し、丸め後の絶対値が
  `10^(p-s)` 未満であれば保持できる。例: `0.00123::numeric(3,5)` は `0.00123` のまま通り、
  `0.0123::numeric(3,5)` は丸め後も範囲を超えるため `22003`。負のスケール（`s < 0`）は
  小数点より左の位へ丸める
- 修飾子の範囲は `1 <= p <= 1000`、`-1000 <= s <= 1000`（`s <= p` は要求しない）。
  範囲外は `22023`（`invalid_parameter_value`）。修飾子なしの `numeric` は精度・スケールとも無制限で、
  丸めも精度判定も行わない
- 数値間の他の変換（整数・float → `numeric(p,s)`）も、変換後に同じ丸めと精度判定を適用する

### BOOLEAN

- `int4 ⇄ bool` のみ `e`。int2・int8・numeric・float との組は `×`。
  代入文脈の `int4 → bool` は `42804`（`e` は明示専用のため）
- bool → text／varchar／bpchar は表のとおり `a`（代入。PostgreSQL では入出力変換でなく専用の変換として登録）
- 文字列 → bool の不正形式は `22P02`（`invalid input syntax for type boolean`）

### 文字列型（text／varchar／bpchar）

- 相互は `i`
- 長さ修飾付きの型への**明示**の変換は黙って切り詰める（`'abcdef'::varchar(3)` は `abc`）。
  **代入**の変換で長さを超えると `22001`（`value too long for type character varying(3)`）。
  超過部分が空白のみなら切り詰めて受理する
- bpchar → text／varchar は末尾の空白を除く
- 各型 → 文字列型は `io/a`、文字列型 → 各型は `io/e`

### 入出力変換のエラー

| 条件 | SQLSTATE |
| ---- | -------- |
| 文字列の形式不正（数値・BOOLEAN・UUID・JSON・配列リテラル） | `22P02` |
| 日付時刻の形式不正 | `22007` |
| 数値の範囲外 | `22003` |
| 日付時刻のフィールド値の範囲外（`2026-13-45`・`25:00`） | `22008` |
| INTERVAL のフィールド値の範囲外 | `22015` |
| BYTEA の 16 進表記の不正桁 | `22023`（`invalid hexadecimal digit`） |

配列リテラルの不正は `22P02`（`malformed array literal`）、要素の不正は要素型のコードのまま。

### 日付時刻間

- date → timestamp／timestamptz は `i`、timestamp → timestamptz は `i`
- timestamp → date／time は `a`、timestamptz → date／time／timestamp は `a`
- time → interval は `i`、interval → time は `a`（日の部分は捨てる）
- timestamp ⇄ timestamptz、date → timestamptz、timestamptz → date／time はセッションの
  `TimeZone` に依存する（ポインタ: TABLE-33）
- date ⇄ time、date／timestamp／timestamptz ⇄ interval、time → date／timestamp 等は `×`
  （例: `now()::date::time` は `42846`）

### JSON

- json ⇄ jsonb は `a`。json → スカラー型はすべて `×`
- jsonb → bool／int2／int4／int8／float4／float8／numeric は `e`。JSON の型が合わない場合
  （文字列・配列・オブジェクト・`null`）は **`22023`**（`cannot cast jsonb string to type integer`
  の形式。`null` も同コードで `cannot cast jsonb null to type integer`）。
  数値の範囲外は `22003`。`'1.5'::jsonb::int4` は numeric 経由で 2 に丸める
- jsonb → text／varchar／bpchar は `io/a`（`'null'::jsonb::text` は文字列 `null`）

### UUID・BYTEA

- 文字列型との `io` のみ。uuid ⇄ bytea、整数 ⇄ uuid、整数 ⇄ bytea は `×`
  （整数 ⇄ bytea は PostgreSQL 17 には無く、より新しい版で追加されたため、17 に合わせ `×`）
- text → bytea は文字列の入力関数を介する（`'abc'::text::bytea` は `\x616263`）

## SQLSTATE 写像

| 時点 | 条件 | SQLSTATE |
| ---- | ---- | -------- |
| 束縛時 | 型の組に変換の対応なし（明示の CAST） | `42846` |
| 評価時 | 文字列の形式不正（日付時刻以外） | `22P02` |
| 評価時 | 日付時刻の形式不正 | `22007` |
| 評価時 | 数値の範囲外・numeric 精度あふれ・float8 NaN → 整数 | `22003` |
| 評価時 | 日付時刻の範囲外 | `22008` |
| 評価時 | INTERVAL のフィールド値の範囲外 | `22015` |
| 評価時 | 代入の変換での長さ超過 | `22001` |
| 評価時 | jsonb → スカラーの型不一致（`null` を含む）・BYTEA の 16 進桁不正 | `22023` |
| 評価時 | numeric NaN／Infinity → 整数 | `0A000` |

使い分けの要点は、**`22P02` は形式不正、`22003` は範囲外、`42846` は型の組に対応がない**ことである。

## エラー文言テンプレート

- PostgreSQL 17 の文言をそのまま採用し、PostgreSQL が値を含める文言は同じく値を含める。
  値の書式は PostgreSQL と同じ `"..."` の引用とし、Rust の `{:?}` によるエスケープや
  `, column ...` の接尾辞は付けない（書式を 1 つに固定する。現行の `sql/parser.rs` は
  `{raw:?}`・`, column {name:?}` が混在しているため、#1591／#1592 はこの基準に揃える。
  既存箇所の書き換えは本 Issue の範囲外）
- 文言へ埋め込む値は表示専用で、SQL やプラン文字列の組み立てには使わない。埋め込む値は
  既存の文の長さ上限を経たものに限る
- `42846` の文言は `cannot cast type <変換元> to <変換先>`（型名のみ。値を含まない）

| SQLSTATE | 値あり（既定） | 値なし（全テナント DDL 経路） |
| -------- | -------------- | ------------------------------ |
| `22P02` | `invalid input syntax for type integer: "abc"` | `invalid input syntax for type integer` |
| `22003`（入力） | `value "99999999999" is out of range for type integer` | `value is out of range for type integer` |
| `22003`（変換） | `integer out of range` | 同左（値を含まない） |
| `22007` | `invalid input syntax for type date: "abc"` | `invalid input syntax for type date` |
| `22008` | `date/time field value out of range: "2026-13-45"` | `date/time field value out of range` |
| `22015` | `interval field value out of range: "99999999999 years"` | `interval field value out of range` |
| `22023`（jsonb） | `cannot cast jsonb string to type integer` | 同左（値を含まない） |
| `42846` | `cannot cast type uuid to integer` | 同左（値を含まない） |

**値を含めてよいのは、呼び出し元の文・パラメータ・`COPY` の入力と、呼び出し元から見える
行の値に限る**（ポインタ: SQL-47 (5)・RLS-13）。表の全テナントの行を検証・変換する DDL の
経路（`ALTER COLUMN ... TYPE`・`SET NOT NULL`・制約追加の検証。ポインタ: TABLE-22・
TABLE-39・TABLE-40・INDEX-9）では、他テナントの値が呼び出し元へ漏れないよう、SQLSTATE は
同じまま**値を含まない文言**を返す。

## 公開コードの現状（origin/main 時点）

- `crates/engine/src/catalog.rs` の `ColumnType`: Text・Varchar・Bpchar・Vector・SmallInt・
  Integer・BigInt・Real・Double・Boolean・Date・Timestamp・Time・TimestampTz・Interval・
  Array・Bytea・Json・Jsonb・Enum・Numeric・Uuid
- `crates/engine/src/sql/udf_call.rs` の `ExprType`: Scalar・Vector・Bool・Text・Date・
  Timestamp・Time・TimestampTz・Interval・Json・JsonText（数値は `NumericKind` で区別）。
  Uuid・Bytea・配列・Numeric の厳密値を表す式の型は未整備で、#1591／#1592 の前提となる
- `crates/engine/src/error_format.rs` の `ErrorClass` に `42846`（`CannotCoerce`）を登録済み
  （Issue #1590。wire-server の HTTP 写像〔400〕・`nosql-api.md` も追従）。構文は `Expr::Cast`
  （`sql::udf_call`）、規則表と束縛は新モジュール `sql::cast`（表を 400 セル照合する単体テスト付き）
- 既存の値リテラル解析（`datetime::parse_interval` や `sql::parser` の型別リテラル束縛）は
  変換の入力関数として再利用できる見込み（判断は兄弟 Issue）

## 兄弟 Issue への引き渡し

| Issue | 担当範囲 |
| ----- | -------- |
| #1590 | 構文・束縛・`42846` の判定（型の組だけで決める）・`ErrorClass` への追加 |
| #1591 | 数値・文字列・BOOLEAN の行／列のセル |
| #1592 | 日付時刻・JSON・UUID・BYTEA・配列のセル |
| #1593 | `::` の脱糖・優先順位（規則表は CAST と共有） |

## 対象外・既知の差分（fail-closed）

- ENUM ⇄ 文字列、VECTOR／HALFVEC、TIMETZ は本表の 20 型の外。本書では扱わず、当面は従来どおり
  fail-closed（拒否側）に倒す
- 暗黙・代入の文脈では変換を広げない（`42883`／`42804` を維持）。拡張するのは明示の CAST／`::` のみ
- NaN／Infinity を表せない NUMERIC での float → numeric、numeric NaN／Infinity → 整数は
  PostgreSQL との差分候補で、扱いは #1591 で決める

### Issue #1590 で確定した差分

- ENUM 型名は、ENUM が実在してもカタログを引かずに `42704` になる（型の存在が他テナントへの
  存在オラクルにならないための静的な判定）
- 修飾子なしの `numeric`・`numeric(p)` は、DDL と共通の型名解析のため `42601` になる
  （PostgreSQL では受理。扱いは #1591）
- CHECK 本体での CAST と `<型名> '<文字列リテラル>'` は `0A000`
- INSERT VALUES・列 DEFAULT の定数式での CAST は `0A000`
- FROM ありの投影項目の先頭に置いた `<型名> '<文字列リテラル>'`（`SELECT INTERVAL '1 day' FROM t` 等）は
  従来どおり拒否（FROM なし SELECT・式の途中・`WHERE` では受理）
- `CAST($n AS t)` の `$n` は変換先の型での推論をしない（Describe の仮値置換との整合が要るため。
  拡張クエリでは `$n` を CAST の被演算子・比較の相手に置いた形が `42601` で拒否される。実測済み）
- 本 Issue の束縛が値を扱うのは、恒等（同一型で型修飾子なし）・型なし文字列リテラル（date／time／
  timestamp／timestamptz／interval／jsonb／修飾子なしの text・varchar）・型付き NULL に限る。規則表が
  変換を許すそれ以外の組は `0A000`（#1591・#1592 が `BuiltinFn` の variant として足す）
- 数値だが種別を静的に決められない式（`id`・関数・算術・CASE）は、数値 6 型・bool への変換を `0A000`、
  それ以外の変換先は全数値行で規則が同じなので通常どおり判定する
- 式の深さ上限（32）のもとで CAST を入れ子にできるのは 10 段まで（CAST 1 段は丸括弧で包んだ部分式 1 段と同じ再帰を消費し、括弧の入れ子と最大段数が一致する）
- UDF 本体の展開中はスキーマを持たないため、本体の CAST は実引数の変換元を精密に導けない（整数列は種別不明の数値、TEXT 系の列は text として扱う）。このため `CAST(x AS integer)` を本体に持つ UDF に整数列を渡すと、直接の `CAST(i4 AS integer)` と違って `0A000` になる。fail-closed 側の差で、精密化は #1591 以降で扱う
- 既定の列名は PostgreSQL と同じく、被演算子が名前を持つ形（列・関数呼び出し）ならその名前、
  持たなければ変換先のカタログ名（`int4` 等）
