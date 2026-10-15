# JSON 演算子・関数・jsonpath の ExprType への写像（Issue #1595）

- ステータス: Accepted（設計確定。実装は下記の担当 Issue が行う）
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-25・TABLE-14
  （関連: SQL-26・SQL-42・SQL-47・NOSQL-8・INDEX-8・WIRE-13・WIRE-14・ERR-4、
  TASK-243・TASK-273）

## 背景・目的

`JSON`／`JSONB` は列型として保存・往復できるが、式評価器（`sql::udf_call` の
`ExprType`／`ExprValue`／`BoundExpr`）には JSON の型がない。式中の JSON 列参照は
`22000`、パス演算子は `42601` で拒否される（[column-type-extension.md](column-type-extension.md)
「#889 追記」決定 D5）。親 #1594 配下の兄弟 Issue（#1596〜#1599・#2180・#2181）と
jsonpath 系（#2182 配下の #2183〜#2187）は、いずれも「JSON 値が式評価器の中で
どの型・値表現になるか」を前提にする。本書はその写像を 1 か所で先に決め、
兄弟 Issue が同じ判断を個別に下し直すことを防ぐ。本 Issue は docs のみで
コードは変更しない。

## 現状（`origin/main` 時点）

- `ExprType` は `Scalar`／`Vector`／`Bool`／`Text`／`Date`／`Timestamp` の 6 値。
  全数 match は `udf_call.rs`・`parser.rs`・`const_expr.rs`・`set_expr.rs`・
  `scan.rs`・`aggregate.rs`・`exec.rs`・`const_select.rs`・`expr_program.rs`・
  `declarative_predicate.rs` に散在する。
- 字句化済み（構文段が `42601` で拒否）: `->` `->>` `#>` `#>>` `::` `||`。
  未字句化: `@>` `<@` `?` `?|` `?&` `#-` `@?` `@@`。
- `json.rs`: `parse_json`（`JsonValue::Object` は `BTreeMap`、重複キー拒否）、
  `write_canonical`、`canonical_equality_text`（値等価の唯一の正準形）、
  `JsonPath`／`extract_path`／`extract_path_text`（Rust API 限定）、上限定数
  `MAX_JSON_DEPTH`=16・`MAX_JSON_CONTAINER_ITEMS`=65,536 ほか。
- JSON 列の `=`／`IN`／`IS NULL` は宣言的述語経路で `canonical_equality_text` を使い、
  式評価器を経由しない。`GROUP BY`／`DISTINCT` は `Cell::Json` を拒否し、
  `OrderValue` に JSON variant はない。
- wire 側は `ColumnMeta::Scalar{ty: Json|Jsonb}` から OID 114／3802 への写像が実装済み。
- 集合返却関数（SRF）の基盤（#2220）と CAST／`::`（#1588〜#1593）は未実装。

## 決定

### D1: ExprType の形

`ExprType::Json` と `ExprType::Jsonb` の 2 variant を追加する（`Json(JsonKind)` より、
全数 match で地点を列挙させる既存方針と相性がよい）。`column_type_for_expr_type` は
`ColumnType::Json`／`Jsonb` を返し、wire の OID 写像はそのまま効く。`jsonpath` 用の
variant と配列用の variant は追加しない（後者は配列列型の領域。
[array-column-type.md](array-column-type.md)）。担当: #1597。

### D2: 値表現

`ExprValue::Json(Cow<str>)`／`ExprValue::Jsonb(Cow<str>)`。列参照は借用、演算結果は
所有（`Text` と同じ使い分け）。正は json が原文、jsonb が `write_canonical` の出力。
`JsonValue`（`BTreeMap`）は json のキー順・重複キーを表せないため、json の演算は
既存パーサーに「順序とスパンを保持するモード」（重複キー許容・原文バイト範囲を返す）を
足して行い、第 2 のパーサーは作らない。jsonb の演算は `parse_json` → 演算 →
`write_canonical`。演算子連鎖（`c -> 'a' -> 'b'`）の行ごとの再パースは初期実装では
許容し、パース済み木の保持は将来課題とする。担当: #1597（jsonb）、#2180（json）。

### D3: json の重複キーと格納の変更

`JSON` 列は重複キー入力を原文のまま受理し、`JSONB` 列は後勝ちで正規化する
（現状は両者とも拒否）。NoSQL 表層（NOSQL-8）の重複キー拒否は維持するため、
パーサーに呼び出し元が選ぶ重複キー方針（Reject／KeepLast／Preserve）を持たせる。
受理範囲の拡大のため BREAKING として扱う。改訂前に格納済みの値は原文のまま扱う。
担当: #2180。

### D4: 字句の追加

未字句化の `@>` `<@` `?` `?|` `?&` `#-` `@?` `@@` を SQL-42 の多文字演算子表へ
最長一致で追加する。衝突の解き方: `<@` は `<=>`／`<` と、`#-` は `#`＋`-` と、
`@?`／`@@` は `@>` と最長一致で区別する。`?` は wire の `$n` と衝突しない。
`-` と `||` は既存トークンを再利用しオペランド型で振り分ける（text の `||` と
jsonb の `||` は左右の型で解決し、どちらも JSON 型でなければ従来の text 連結）。
担当: #1598（`@>`〜`?&`）、#2181（`#-`）、#2186（`@?`・`@@`）。

### D5: 優先順位と結合性

PostgreSQL の「その他の演算子」の階層に置き左結合とする。二項 `+`／`-`、`*`／`/` より
弱く、比較演算子・`IS`・`NOT`・`AND`・`OR` より強い。例: `c -> 'a' = '1'` は
`(c -> 'a') = '1'`。担当: #1596。

### D6: 型解決（演算子）

左右オペランドの型から結果型／SQLSTATE を引く表を、担当 Issue が実装時に網羅する。
方針は次のとおり。

| 演算子 | 結果 | 備考 |
| ------ | ---- | ---- |
| `->` | json／jsonb | 右辺は text キーまたは整数添字（負の添字可）。Scalar は整数性を検査 |
| `->>`・`#>>` | Text | |
| `#>` | json／jsonb | |
| `@>`・`<@`・`?`・`?\|`・`?&`・`@?`・`@@` | Bool | jsonb 専用 |
| `\|\|`・`-`・`#-` | jsonb | jsonb 専用 |

- `#>`・`#-`・`?|`・`?&` の `text[]` 引数は、当面は定数（配列リテラル表記・`$n`）を
  束縛時に解析する。定数でなければ `0A000`。
- jsonb 専用演算子を json に適用したら `42883`。NULL は strict 伝播。
- 不正引数（スカラーへの `-` 整数など）は PostgreSQL 17 と同じ SQLSTATE
  （`22023` 等）とし、PostgreSQL 17 との突き合わせテストで固定する。

担当: #1596・#1598・#2180・#2181。

### D7: 型解決（関数）

`json_*`／`jsonb_*` の主要関数（`typeof`→Text、`array_length`→Scalar 整数、
`extract_path(_text)`、`build_object`／`build_array`、`to_json(b)`、
`set`／`insert`／`strip_nulls`）の引数型・結果型を担当 Issue が表で固定する。
オーバーロードは `bind_call` の既存方式、可変長引数の上限は既存の関数引数上限に従う。
担当: #1599・#2180。

### D8: 集合返却関数と集計

`*_array_elements`・`*_each`・`*_object_keys`・`jsonb_path_query` は SRF 基盤（#2220 系）
ができるまで `0A000` で拒否し、先行実装は `_array`／`_first`／`_exists`／`_match` 版とする。
`jsonb_agg`／`jsonb_object_agg` は `AggregateInput` の拡張だが担当 Issue は未割当
（申し送り）。担当: #1599・#2185。

### D9: jsonpath

出力型としての ExprType は追加しない。パス文字列は束縛時に定数（リテラル・`$n`）として
構文解析し、コンパイル済みパスを `BoundExpr` のノードに保持する。定数でなければ
`0A000`。評価ステップ数の上限は既存の `*_limits.rs`／`*_limits_opt.rs` の流儀で
既定値と CLI フラグを設け、超過は `54000`。`lax`／`strict` を扱う。`like_regex` は
正規表現の依存が承認されるまで `0A000`。担当: #2183・#2184・#2186（問い合わせ関数は #2187）。

### D10: 比較・ORDER BY・GROUP BY

- jsonb の `=` は `canonical_equality_text` を唯一の正準形として再利用する。
- `OrderValue` に jsonb の variant を追加し、PostgreSQL の jsonb btree 順序
  （型の順位 → 要素数・ペア数 → 格納順の要素比較）を実装する。例外的な順序は
  PostgreSQL 17 との突き合わせで固定する。
- json は btree 演算子クラスがないため、式としての ORDER BY／GROUP BY／DISTINCT を
  `42883` とする。既存の JSON 列の `=`／`IN`／`IS NULL`（TABLE-14）は変えない。
  PostgreSQL との差は spec 側の課題として報告する（申し送り）。

担当: #1597。

### D11: 上限と未知リテラルの型付け

- 演算結果（`||`・`jsonb_set`・`build_*` など）にも `MAX_JSON_DEPTH`／
  `MAX_JSON_CONTAINER_ITEMS`／`MAX_JSON_FIELD_LEN` を適用し、超過は `54000`。
  入力リテラルの構文不正は従来どおり `22P02`。両者を区別する。
- CAST／`::` が未実装の間は、`Expr::DateLiteral` と同形の型付きリテラル
  `JSON '…'`／`JSONB '…'`、および JSON 型の式と並んだ未知リテラル（文字列リテラル・
  `$n`）の束縛時型付けで代替する。text 型の式からの暗黙変換はしない（`42883`）。
  `param_infer`（`42P18`）にも jsonb 推論を加える。#1593 と整合させる。

担当: #1596・#1597。

### D12: セキュリティ上の不変条件

- 評価は RLS 適用後の可視行に対してのみ行い、新しい読み取り経路を作らない。
- GIN 相当の索引（#2204）の有無で結果と可視性は変わらない。
- 未信頼入力の経路で `unwrap`・添字アクセスを使わず、`checked_*` と上限検証後の確保
  （`try_reserve`）を使う。jsonpath・`text[]` 引数は構造解析し、SQL・プラン文字列へ連結しない。
- エラーメッセージに格納値を含めない。未対応の形は `42601`／`0A000`／`42883` で
  fail-closed に拒否する。

## 担当 Issue 対応表

| 決定 | 担当 Issue |
| ---- | ---------- |
| D1・D2（jsonb）・D10・D11 | #1597 |
| D2（json）・D3・D6・D7 | #2180 |
| D4・D5・D6・D11（リテラル） | #1596 |
| D4・D6（包含・存在） | #1598 |
| D4・D6（`#-`・削除） | #2181 |
| D7・D8 | #1599 |
| D4・D9 | #2183〜#2187 |
| D12（索引） | #2204 |

## 対象外・申し送り

- コード実装はすべて上記の担当 Issue が行う。
- jsonb の出力テキスト形式（キー順・空白・数値正規化）と PostgreSQL 17 との差の是正は、
  格納形式・`content_hash` の互換に影響するため本 Issue では変えない。
- `jsonb_agg`／`jsonb_object_agg` の担当 Issue が未割当。
- json 列の `=` と PostgreSQL との差は spec 側の課題。
- SRF 基盤（#2220 系）。
