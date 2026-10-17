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
  float → numeric の扱いは #1591 で決めた（「Issue #1591 で確定した差分」参照）

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
  Timestamp・Time・TimestampTz・Interval・Json・JsonText・Numeric・Uuid・Bytea・Array（数値は
  `NumericKind` で区別。`Numeric` は Issue #1591、`Uuid`・`Bytea`・`Array` は Issue #1592 で
  追加した。`Array` は要素型を持たず、要素型は `CAST` の変換先・列のスキーマから導く）
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
| #1593 | `::` の脱糖・優先順位（実装済み。構文段で `CAST` と同一の `Expr::Cast` へ脱糖するため、規則表・SQLSTATE・既定列名・内容ハッシュは CAST と共有） |

## 対象外・既知の差分（fail-closed）

- ENUM ⇄ 文字列、VECTOR／HALFVEC、TIMETZ は本表の 20 型の外。本書では扱わず、当面は従来どおり
  fail-closed（拒否側）に倒す
- 暗黙・代入の文脈では変換を広げない（`42883`／`42804` を維持）。拡張するのは明示の CAST／`::` のみ
- NaN／Infinity を表せない NUMERIC での float → numeric、numeric NaN／Infinity → 整数は
  PostgreSQL との差分候補だった。#1591 で「本エンジンでは到達しない」と確定した（下記）

### Issue #1590 で確定した差分

- ENUM 型名は、ENUM が実在してもカタログを引かずに `42704` になる（型の存在が他テナントへの
  存在オラクルにならないための静的な判定）
- 修飾子なしの `numeric`・`numeric(p)` は、DDL と共通の型名解析のため `42601` になる
  （PostgreSQL では受理。CAST の型名に限り #1591 で受理するようにした）
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

### Issue #1591 で確定した差分

数値 6 型・BOOLEAN・文字列 3 型どうし（10 型 × 10 型の 100 セル）の値の変換を実装した。評価は
`sql::cast_value`、束縛は `sql::cast::bind_cast`。テストは
`crates/engine/tests/sql_cast_numeric_text_bool.rs`（セルキー `<変換元>-><変換先>` で 100 セル）。
PostgreSQL 17 との照合は実機を使えなかったため、本書の「系統ごとの値レベルの規則」を拠り所にした
（実機との照合は未実施）。

#### 実装した範囲

- 整数への変換: float は偶数丸め、numeric は 0 から遠い側への四捨五入の後に範囲検査（`22003`）
- 文字列 → 整数: 前後の空白、符号、`0x`／`0o`／`0b` の接頭辞、数字の間の単独の `_` を受け付ける。
  `1.5`・空文字・空白のみは `22P02`
- 文字列 → float: `+1`・`.5`・`1.`・指数を受け付ける。範囲外とアンダーフローは `22003`
- float → numeric: 有効桁 15（float8）／6（float4）の `%g` 相当の表現を経由する（最短往復表記ではない）
- `varchar(n)`／`char(n)` への明示の変換は文字数 `n` で黙って切り詰め、`char(n)` は空白で埋める。
  CHAR(n) の値を別の型へ変換するときは末尾の空白を除く
- 型なし文字列リテラルと列の値は同じ入力関数を通る（束縛時に 1 度検証し、評価は変換ノードが行う）
- 結果列の型は変換先で決める。修飾子なしの numeric は scale が値ごとに変わるため、結果列型は
  固定の sentinel（`AVG` の整数と同じ前例。wire の typmod は常に -1）

#### PostgreSQL との差分（fail-closed 側）

| 項目 | 本エンジン | 理由 |
| ---- | ---------- | ---- |
| 2^53 を超える int8 の結果 | `22003`（丸めない） | 式評価が整数を f64 で運ぶ |
| 文字列の `NaN`／`Infinity`（float・numeric への入力） | `0A000` | 非有限値を値として持たない |
| float → numeric の NaN／Infinity、numeric の NaN／Infinity → 整数 | 到達しない | float の値は常に有限、`Decimal` は非有限値を持たない |
| 16 進表記の float、`_` 区切り・非 10 進接頭辞つきの numeric | `22P02` | 文法を閉じる |
| 38 桁を超える numeric、scale 38 を超える値（`1e300`・`1e-40`） | `22003` | `Decimal` は最大 38 桁 |
| `numeric(p,s)` で p が 39 以上、`s > p`、負の位取り | `0A000` | PostgreSQL では有効だが `Decimal` で表せない |
| `numeric(0,…)`・`numeric(1001,…)` など範囲外の精度 | `22023` | PostgreSQL と同じ |
| 種別を静的に決められない数値（関数・`id`・算術・CASE）の数値・bool・文字列型への変換 | `0A000` | numeric と float で丸め・表記が異なる |
| numeric 値の消費位置 | 投影と CAST の被演算子だけ（比較・算術・関数引数・CASE・集計・ORDER BY は `0A000`） | 式評価に numeric の演算が無い |
| NUMERIC／BOOLEAN 列 | CAST の被演算子の位置でだけ式の中で参照できる | 一般の位置での解禁は別 Issue |
| CHAR(n) の比較・連結 | `CAST(x AS char(n))` の結果を TEXT として消費すると、空白埋めのまま比較される | TEXT の式型は bpchar を区別できない |
| CHECK 本体・列 DEFAULT・INSERT VALUES の値の変換 | `0A000` | 全テナントを走査する検証経路へ持ち込まない |

#### 型名

修飾子なしの `numeric`／`decimal` と `numeric(p)`（`= numeric(p,0)`）は、CAST と
`<型名> '<文字列リテラル>'` の型名に限って受け付ける。DDL の型名解析は変えていない
（修飾子なしの `numeric` 列は従来どおり `42601`）。

### Issue #1592 で確定した差分

日付時刻・JSON・UUID・BYTEA・配列が絡む残りの 300 セル（新しい 10 行 × 20 列 ＋ 数値 6 型・BOOLEAN・
文字列 3 型の 10 行 × 新しい 10 列）の値の変換を実装した。評価は `sql::cast_value`
（`apply_cast_ext`）、束縛は `sql::cast::bind_cast`。テストは
`crates/engine/tests/sql_cast_datetime_json_misc.rs`（300 セルをセルキー `<変換元>-><変換先>` で網羅。
`×` は `42846`、それ以外は値と結果列の型、NULL 伝播）。PostgreSQL 17 実機との照合は未実施で、本書の
「系統ごとの値レベルの規則」を拠り所にした。

#### 実装した範囲

- 日付時刻間: date → timestamp／timestamptz、timestamp → date／time／timestamptz、
  timestamptz → date／time／timestamp、time → interval、interval → time。timestamptz が絡む変換と
  文字列化・解析は**評価時**のセッション `TimeZone` に従う（束縛時に固定しない）。interval → time は
  月・日を捨て、時刻部分を 1 日の剰余（`rem_euclid`）にする（`-1 hour` は `23:00:00`、`24:00:00` は
  `00:00:00`）。範囲外は `22008`
- 日付時刻 ⇄ 文字列型: 出力は `format_*`（timestamptz は評価時のセッション TZ）、入力は `parse_*`
  （ゾーンの無い timestamptz は評価時のセッション TZ の壁時計）。エラーの SQLSTATE は型なし
  リテラル（束縛時）と文字列式（評価時）で共有する（`22007`／`22008`／`22015`／`22023`。interval の
  無限大は `0A000`）
- json ⇄ jsonb: json → jsonb は重複キーを最後の値で畳んで正規化、jsonb → json は正規化テキストのまま。
  文字列型 → json は構文検証のみで原文を保持し、→ jsonb は正規化する（不正は `22P02`）
- jsonb → 数値 6 型・boolean（`e`）: 整数・numeric 系への数値は numeric を経由して #1591 の変換
  （四捨五入・範囲検査 `22003`・`numeric(p,s)` の丸め）を共有する。float4／float8 への小数・指数表記の
  数値は numeric を経由せず元テキストから対象の型へ直接解析する（`1e100`／`1e-100` は double precision で
  成功。型の範囲外〔非有限・float4 のアンダーフロー含む〕のみ `22003`）。型の合わない JSON は `22023`（`cannot cast jsonb <string|numeric|
  boolean|null|array|object> to type <型名>`。値を含まない固定文言）
- uuid ⇄ 文字列型: 出力は小文字の正規形、入力は厳密な 36 文字形（不正は `22P02`）
- bytea ⇄ 文字列型: 出力は `\x` ＋ 小文字 16 進、入力は PostgreSQL の `byteain` に従う（`\x` 始まりは
  16 進、それ以外は escape 形式。`'abc'` は `\x616263`）。16 進の奇数桁・不正桁は `22023`、
  escape 形式の不正は `22P02`
- 配列 → 文字列型: engine の `ArrayValue::to_pg_text`（wire-server から移設。出力は不変）。結果が
  TEXT の上限を超えるときは描画の途中で打ち切って `54000`
- 文字列型 → 配列: `parse_array_literal` を共有する（形式不正は `22P02`、要素数の上限超過は `54000`、
  要素の不正は要素型の SQLSTATE）
- 配列 → 配列: 要素型の組で規則を引き直し（`×` なら `42846`）、要素ごとに変換する。NULL 要素は NULL の
  まま、要素のエラーはその SQLSTATE のまま。同じ要素型どうしは値を変えない
- 結果列の型: uuid／bytea は `Uuid`／`Bytea`、配列は `ArrayType`（要素数上限は `MAX_ARRAY_ELEMENTS`）
- 型なし文字列リテラルと列の値が同じ入力関数を通る（json・uuid・bytea・配列は束縛時に 1 度検証する）
- `UUID`／`BYTEA`／配列列は、NUMERIC／BOOLEAN 列と同じく CAST の被演算子の位置でだけ式の中で参照できる

#### PostgreSQL との差分（fail-closed 側）

| 項目 | 本エンジン | 理由 |
| ---- | ---------- | ---- |
| uuid の入力形式 | 厳密な 36 文字形のみ。波括弧・ハイフンなし・`urn:uuid:` 等は `22P02` | 文法を閉じる（#887 の列リテラルと共有） |
| bytea の 16 進本体の空白 | `22023`（PostgreSQL は許容） | 列リテラルの入力関数と文法を揃える |
| bytea の escape 形式の入力 | `CAST` の入力関数でのみ受理。INSERT のリテラル束縛は `\x` 形式のみのまま | INSERT の束縛は本 Issue の対象外 |
| escape 形式の入力が上限（4 MiB）を超える | `54000`（`\\`・`\ooo` で縮む場合も入力長で判定） | 確保前の上限判定を単純に保つ |
| 日付時刻の文字列入力の前後空白 | `22007`（PostgreSQL は許容） | 既存の型付きリテラルと文法を共有する |
| 配列の変換先にできる要素型 | boolean・text・integer・bigint・real・double precision・date・timestamp・uuid・`numeric(p,s)`・bytea・json・jsonb。それ以外（smallint・time・timestamptz・interval・`varchar(n)`・`char(n)`・修飾子なしの numeric・ENUM）は規則表で `×` でなければ `0A000` | 配列列の行内表現（`ArrayElemType`）に対応が無い |
| 多次元配列・添字範囲装飾（`[1:2]={..}`） | `22P02` | 1 次元のみ |
| bigint 配列の 2^53 を超える要素の変換 | `22003` | 式評価が整数を f64 で運ぶ（スカラーの int8 と同じ） |
| 配列・uuid・bytea 値の消費位置 | 投影と CAST の被演算子だけ（比較・算術・関数引数・CASE・集計・ORDER BY・代入は `0A000`） | 式評価にこれらの演算が無い |
| enum 要素の配列 | 文字列型への変換のみ可（配列への変換は `0A000`） | 要素型を CAST の型で表せない |

#### 対象外

- uuid・bytea・配列・numeric・bool 列の式の一般位置での利用（比較・関数引数等）
- 多次元配列、`ArrayElemType` に無い要素型の配列への変換
- INSERT のリテラル束縛での bytea escape 形式
- PostgreSQL の配列エラー文言（`malformed array literal` 等）との完全一致
