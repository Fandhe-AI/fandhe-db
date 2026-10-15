# JSON 演算子・関数・jsonpath の ExprType への写像（Issue #1595）

- ステータス: 公開境界の縮小版（詳細な設計判断は private spec 側で管理し、本書はポインタと現状の記録に限る）
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-25・TABLE-14
  （関連: SQL-26・SQL-42・SQL-47・NOSQL-8・INDEX-8・WIRE-13・WIRE-14・ERR-4、
  TASK-243・TASK-273）

## 背景・目的

`JSON`／`JSONB` は列型として保存・往復できるが、式評価器（`sql::udf_call` の
`ExprType`／`ExprValue`／`BoundExpr`）には JSON の型がない。式中の JSON 列参照は
`22000`、パス演算子は `42601` で拒否される（[column-type-extension.md](column-type-extension.md)
「#889 追記」決定 D5）。親 #1594 配下の兄弟 Issue（#1596〜#1599・#2180・#2181）と
jsonpath 系（#2182 配下の #2183〜#2187）は、JSON 値が式評価器の中でどの型・値表現に
なるかを前提にする。本書は、その写像の設計判断が private spec 側にあること、および
公開コードの現状と担当 Issue の対応を 1 か所に示す。本 Issue は docs のみでコードは
変更しない。

## 公開コードの現状（`origin/main` 時点）

- `ExprType` は `Scalar`／`Vector`／`Bool`／`Text`／`Date`／`Timestamp` の 6 値で、
  JSON 用の variant はない。
- 字句化済み（構文段が `42601` で拒否）: `->` `->>` `#>` `#>>` `::` `||`。
  JSON 関連の他の演算子は未字句化。
- `json.rs` に `parse_json`・`write_canonical`・`canonical_equality_text`・`JsonPath`／
  `extract_path` 等（Rust API 限定）と上限定数がある。
- JSON 列の `=`／`IN`／`IS NULL` は宣言的述語経路で処理され、式評価器を経由しない。
  `GROUP BY`／`DISTINCT` は `Cell::Json` を拒否する。
- wire 側は `ColumnMeta::Scalar{ty: Json|Jsonb}` から OID 114／3802 への写像が実装済み。
- 集合返却関数の基盤（#2220）と CAST／`::`（#1588〜#1593）は未実装。

## Issue #1596 での変更（パス演算子の構文・束縛・評価）

- `ExprType::Json`・`ExprValue::Json`（正規化テキスト）・`BoundExpr::JsonbColumnRef`／
  `JsonAccess` を最小限だけ導入した。jsonb 値を消費できるのは投影とパス演算子の左辺だけで、
  比較・順序・集計・関数引数・`CASE`／`COALESCE`／`NULLIF`・`UPDATE SET` の代入は
  `0A000`（#1597 が開放する）。
- 構文は `Expr::JsonAccess`（`BinOp` とは別 variant）。優先順位は加減算より弱く比較より強い
  左結合。CHECK 本体では受理しない。
- `->`・`->>` の右辺は text（キー）または int4（添字。負値は末尾から）、`#>`・`#>>` の右辺は
  text[] リテラルのみ。左辺が jsonb でなければ `42883`、`json` 列は `0A000`、jsonb 列の
  裸の参照は従来どおり `22000`。
- 結果: `->`・`#>` は jsonb（OID 3802）、`->>`・`#>>` は text（OID 25）。存在しないキー・
  範囲外の添字・容器の型不一致は SQL の NULL。
- DML の content hash にタグ 12 を追加した（[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)）。

## Issue #1597 での変更（比較・述語・ORDER BY）

- jsonb 同士の比較（`=` `<` `>` `<=` `>=`）は PostgreSQL 17 の jsonb btree 順序で行う。
  順序の定義は `json::jsonb_sort_key`（バイト列の辞書順が全順序になるソートキー）の 1 か所に
  置き、WHERE の比較と ORDER BY が共有する。キーの等しさは `canonical_equality_text` の
  等しさと一致する（数値は 10 進の厳密比較。`1`・`1.0`・`10e-1` は等しい）。
- jsonb 式と比較される型なしの文字列リテラルは jsonb として解析する（`bind_jsonb_literal`。
  不正な JSON は `22P02`、長さ超過は `54000`）。キャスト（#1588〜#1593）は未実装のため、
  この暗黙解釈で `WHERE docb -> 'k' = '1'` を実現する。jsonb と他の型の比較は `42883`。
- ORDER BY は jsonb の式キー（パス演算子・jsonb 列・jsonb の式項目の別名）を受理する。
  NULL は昇順で末尾・降順で先頭。ウィンドウ関数付きの文では `0A000`。
- jsonb 列の裸の参照を式の中で受理する。jsonb を受け付けない位置（算術・関数引数・集計・
  `CASE` 等・代入）は `0A000`。そのため `vec_norm(docb)` のような従来エラーだった文の
  SQLSTATE が `22000` から `0A000` に変わる。
- 出力形式（コンパクトな正規化テキスト）は #1596 から変えない。

## Issue #1598 での変更（包含・存在演算子）

- `@>` `<@` `?` `?|` `?&` を実装した。構文は #1596 のパス演算子と同じ優先順位（その他の
  演算子の段・左結合）で、`Expr::JsonAccess` の演算子種別を増やして表す。束縛後は
  `BoundExpr::JsonPredicate`（結果は bool）。
- 包含は PostgreSQL 17 の規則（配列の順序・重複を無視、入れ子、最上位の配列 ⊇ スカラーの例外、
  数値は値の等価）。存在は最上位のキー・配列の文字列要素・文字列スカラーのみが対象で、
  `?|`・`?&` のキー集合の NULL 要素は読み飛ばす。左右いずれかが NULL なら結果も NULL。
- 型規則: `@>`・`<@` は jsonb 同士（型なし文字列リテラルは jsonb として解釈。不正な JSON は
  `22P02`）、`?` の右辺は text、`?|`・`?&` の右辺は text[] リテラルのみ（それ以外は `0A000`。
  配列式は TABLE-35）。`json` 列や非 jsonb の左辺は `42883`。
- 包含判定の評価ステップに上限を置き、超過は `54000`。スカラー要素は `HashSet` 引きで線形に
  保ち、容器 × 容器の総当たりだけが上限の対象になる。
- WHERE では比較演算子を伴わない裸の述語として受理する。`NOT` 付き（三値論理を保った否定の
  表現が要る）と CHECK 本体は受理しない（`42601`）。
- DML の content hash はタグ 12 の下位種別 5〜9 を追加した
  （[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)）。
- 対象外: GIN 相当の索引（#2204）、NoSQL の filter、`@?`・`@@`（#2182 配下）、`||`・`-`・`#-`
  （#2181）、`$n` の型推論、ORDER BY・GROUP BY・CASE の条件に置く裸の述語、JOIN 経路の
  修飾付き識別子。

## Issue #2181 での変更（連結・削除演算子）

- `||`・`-`・`#-` を実装した（TABLE-25 ポインタ）。`||` と `#-` は #1596・#1598 と同じ段
  （その他の演算子・左結合）で `Expr::JsonAccess` の種別を増やして表す。`-` は加減算の段の
  `Binary Sub` のままにし、束縛段で左辺が jsonb を返しうる式のときだけ削除演算子へ振り分ける
  （`d - 'a' -> 'b'` は `(d - 'a') -> 'b'`）。束縛後は `BoundExpr::JsonModify`（結果は jsonb）。
- 規則は PostgreSQL 17 に合わせる。`||` はオブジェクト同士が浅いマージ（重複キーは右辺）、それ以外は
  配列でない側を 1 要素配列とみなして連結する。`- text` はキー削除（配列では同じ文字列要素を
  すべて削除）、`- integer` は添字削除（負は末尾から、範囲外は変化なし）、`#-` はパス削除
  （存在しないパスは変化なし）。スカラーからの削除・オブジェクトへの添字削除・スカラー根への `#-`・
  配列の段の非整数パス要素は `22023`。
- 型規則: `||` は両辺 jsonb（型なし文字列リテラルは反対側が jsonb のとき jsonb として解釈）。
  文字列連結に当たる組（text 同士・`json` 列・jsonb と text）は `0A000`、jsonb と数値などは
  `42883`。`-`・`#-` の左辺は jsonb のみで、`json` 列は `42883`。`- integer` は int4 式のみ。
  `#-` の右辺は text[] リテラルのみ（それ以外は `0A000`）。
- 結果は正規化して上限（長さ・深さ・要素数）を再検証し、超過は `54000`。
- DML の content hash はタグ 12 の下位種別 10（`||`）・11（`#-`）を追加した
  （[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)。`jsonb - x` は `Binary Sub` で区別）。
- 対象外: `jsonb - text[]` の SQL 表層（text[] を書く手段のキャスト・`ARRAY[...]` 待ち。評価関数
  `json::jsonb_delete_keys` は用意済み）、`#-` のパスの NULL 要素（`22004` の分類が未実装のため
  `0A000`）、`UPDATE ... SET` への jsonb 式の代入、text／json の `||`、ORDER BY の式キー、
  `$n` の型推論、NoSQL の filter。

## Issue #1599 での変更（スカラー関数）

- `jsonb_typeof`・`jsonb_array_length`・`jsonb_extract_path`・`jsonb_extract_path_text`・
  `jsonb_build_object`・`jsonb_build_array`・`to_jsonb` を実装した。引数が多相・可変長・
  jsonb 値のため `BuiltinFn` は拡張せず、専用の `BoundExpr::JsonFn`（`JsonFnKind`）で表す。
  式プログラムでは `JsonSubtree` として再帰評価に委ねる（評価の実装は 1 つ）。
- 結果型: `jsonb_typeof`・`jsonb_extract_path_text` は text（OID 25）、`jsonb_array_length` は
  int4（OID 23）、それ以外は jsonb（OID 3802）。
- NULL の扱い: `typeof`・`array_length`・`extract_path(_text)`・`to_jsonb` は strict
  （`extract_path` はパス要素の NULL も NULL）。`build_*` は値の NULL を JSON の `null` にし、
  `jsonb_build_object` のキーが NULL なら `22004`（新設の `ErrorClass::NullValueNotAllowed`。
  HTTP は 400）。
- エラー: `jsonb_array_length` をスカラー・オブジェクトに適用すると `22023`。
  `jsonb_build_object` の奇数個の引数と jsonb のキーも `22023`（個数の検査は実行時なので、
  行のない表では成功する）。型の合わない引数・`json` 列・パス要素なしは `42883`、
  `to_jsonb` の型なしリテラル・裸の `NULL` は `42804`、不正な jsonb リテラルは `22P02`。
  結果の入れ子が 16 段、総長が `MAX_JSON_FIELD_LEN`、文字列 1 個が上限を超えると `54000`
  （出力は 1 要素ずつ検査する）。
- 型なし文字列リテラルは jsonb 位置では jsonb として、「any」位置では text として解釈する。
  数値・bool のキーは文字列化し、重複キーは最後の値が勝つ。DATE は `YYYY-MM-DD`、
  TIMESTAMP は `T` 区切りの ISO 8601（小数秒の末尾の 0 は除く）に変換する。
- 集合返却関数（`jsonb_object_keys`・`jsonb_array_elements(_text)`・`jsonb_each(_text)`）は
  SRF の基盤（#2220）ができるまで実装しない。名前だけを予約し、束縛時に `0A000` で拒否する
  （従来の `42883` から変わる。同名 UDF の定義は `42723`）。
- 未対応のまま（fail-closed）: `UPDATE ... SET jsonb列 = <jsonb 関数>`・`INSERT ... VALUES` の
  jsonb 関数・CHECK 本体（`0A000`）、GROUP BY・DISTINCT の jsonb 結果、`$n` の jsonb 型推論、
  JOIN 経路、`VARIADIC ARRAY[...]` 形式の呼び出し。
- 既知の差分: 引数の個数は `MAX_CALL_ARGS`（32。超過は `54000`）なので `jsonb_build_object` は
  16 組まで（PostgreSQL は 100 個）。数値は内部が f64 のため `to_jsonb(1.0)` は `1`（末尾の 0 が
  残らない）。指数表記なしで 64 バイトを超える極端な値は指数表記で出力する。`to_jsonb(vector)` は
  `0A000`（PostgreSQL＋pgvector は文字列化する）。

## 設計判断の所在

型の形・値表現・字句と優先順位・演算子と関数の型解決・集合返却関数・jsonpath・
比較と並び順・上限・セキュリティ上の不変条件の各判断は、private spec 側の
上記ビヘイビア ID 群と各担当 Issue の実装時に確定する（`docs/spec` 配下。
本書へは転記しない）。実装者は spec を参照し、各 Issue の PR で判断を public に
出してよい範囲だけを記録する。

## 担当 Issue 対応表

| 領域 | 担当 Issue |
| ---- | ---------- |
| 式評価器への JSON 型の導入（jsonb。比較・述語・ORDER BY は #1597 で実装済み） | #1597 |
| json 型の演算・関数 | #2180 |
| パス演算子の構文 | #1596 |
| 包含・存在演算子（実装済み） | #1598 |
| 連結・削除演算子（実装済み） | #2181 |
| JSON スカラー関数（実装済み） | #1599 |
| JSON 集合返却関数（SRF 基盤待ち） | #2220 |
| jsonpath | #2183〜#2187 |
| 索引 | #2204 |

## 対象外・申し送り

- コード実装はすべて上記の担当 Issue が行う。
- 集計関数（`jsonb_agg` 系）の担当 Issue は未割当。
- SRF 基盤（#2220 系）。
- spec 側の課題があれば spec リポ側で扱う。
