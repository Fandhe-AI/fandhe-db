# 新スカラー型の WHERE 述語対応（Issue #891）

対象ビヘイビア: TABLE-13（検討中）・TASK-199。関連: SQL-2・SQL-9（TASK-79・
Issue #353）。spec 本文は転記せず、ID のポインタ表記のみを用いる。

## 背景

Issue #880〜#890 で `INTEGER`／`BIGINT`／`REAL`／`DOUBLE`／`BOOLEAN`／`DATE`／
`TIMESTAMP`／`ARRAY`／`BYTEA`／`JSON`／`JSONB`／`ENUM`／`NUMERIC`／`UUID` の
各列型を追加したが、宣言・読み書きはできても `WHERE` ではほとんど使えない
状態だった。本 Issue は、算術を持たない非数値型（`DATE`／`TIMESTAMP`／
`NUMERIC`／`UUID`／`BYTEA`）を対象に `WHERE` の比較述語（`= < <= > >=`）を
受理できるようにする。

## スコープ（2 レーン方式のうちレーン B のみを実装）

計画段階では「レーン A（算術を持つ INTEGER/BIGINT/REAL/DOUBLE を式評価系
〔`sql::udf_call`／`sql::expr_program`〕へ結線する）」と「レーン B（算術を
持たない DATE/TIMESTAMP/NUMERIC/UUID/BYTEA を宣言的フィルタ
〔`declarative_filter.rs`〕へ結線する）」の 2 レーンを想定していたが、
**本 Issue ではレーン B のみを実装した**。レーン A（`ExprProgram::eval` への
行スカラー列引数の追加・`BoundExpr::ColumnRef`／`IntLiteral` の新設・NULL
の strict 伝播・defer-on-error を保った定数畳み込みの拡張など、行ループの
実行契約そのものに関わる大規模な変更）は影響範囲が広く、算術を伴わない
非数値型だけでも独立した価値があると判断し、別 Issue（未起票。起票は
オーナー判断）へ切り出した。

| 列型 | レーン | 本 Issue での対応 |
| --- | --- | --- |
| DATE | B | 対応済み（`=` `<` `<=` `>` `>=`） |
| TIMESTAMP | B | 対応済み |
| NUMERIC(p,s) | B | 対応済み（列の scale に丸めない正確な比較） |
| UUID | B | 対応済み（バイト列の `Ord`） |
| BYTEA | B | 対応済み（バイト列の辞書順） |
| INTEGER / BIGINT | A | **対象外**（算術・式評価との結線は別 Issue。未起票） |
| REAL / DOUBLE | A | **対象外**（同上） |
| BOOLEAN | 既存（#883） | 変更なし |
| TEXT / ENUM | 既存 | 変更なし（範囲比較・式参照は SQL-24 系別 Issue） |
| ARRAY / JSON / JSONB | 対象外 | 変更なし（TABLE-14 側の範囲） |

## 設計

### 構文（`sql/allowlist.rs`）

`parse_where` のレガシー腕に `Ident (< | > | <= | >=) StringLiteral` を
追加し、`WherePredicate::Compare { column, op: CompareOp, value: String }`
（構文層の `CompareOp`。`Lt`／`Le`／`Gt`／`Ge` の 4 値）として表現する。
`=` は既存の `WherePredicate::Equality` のまま。逆向き（`'x' < col`）・
`$n` パラメータ形式（パターン 4 は `Ident '=' $n` のみ）は受理しない
（式フォールバックへ回り `42601`）。

### 束縛（`sql/parser.rs::bind_where_predicates`）

- `WherePredicate::Equality { column, value }` は、列型が
  `is_typed_compare_column_type`（DATE/TIMESTAMP/NUMERIC/UUID/BYTEA）を
  満たす場合に限り `DeclarativeFilter::compare(column, CompareOp::Eq, value)`
  へ振り分ける。それ以外（TEXT/ENUM 等）は従来どおり `DeclarativeFilter::
  equals`。
- `WherePredicate::Compare { column, op, value }` は常に
  `DeclarativeFilter::compare(column, op.into(), value)` へ振り分ける
  （列型に関わらず。型不一致は `declarative_filter::bind_impl` が判定する）。

### フィルタ本体（`declarative_filter.rs`）

- `CompareOp`（意味層。`Eq`／`Lt`／`Le`／`Gt`／`Ge`）・`TypedLiteral`
  （`Date(i32)`／`Timestamp(i64)`／`Numeric(Decimal)`／`Uuid`／
  `Bytes(Vec<u8>)`）・`FilterOp::Compare`（未束縛。列型未確定のリテラル）・
  `FilterOp::TypedCompare`（束縛済み）を追加した。
- リテラルの解析は INSERT／UPDATE／UPSERT と同じ `sql::parser::
  bind_datetime_literal`／`bind_uuid_literal`／`bind_bytea_literal` を
  共有し、第 2 のパーサーを作らない。`NUMERIC` のみ専用の
  `numeric::parse_literal_exact`（後述）を新設した。
- Prepared Describe 専用の縮退経路（PR #1012 の ENUM 語彙照合スキップ・
  `skip_enum_label_validation`）を「型付きリテラル解析のスキップ」へ
  一般化した。`sql::params::substitute_dummy` が生成する固定ダミー文字列
  `"0"` は DATE／TIMESTAMP／UUID／BYTEA の文法として不正なため、
  Describe 時点でこれを実際に解析すると `WHERE date_col = $1` 等の
  Describe が常に失敗してしまう（NUMERIC は `"0"` 自体が正当な数値の
  ため実質的に無関係）。スキップ時はプレースホルダ値（`Date(0)` 等）へ
  縮退し、実際の値検証は Bind／Execute に委ねる。

### NUMERIC の正確な比較（列の scale に丸めない）

`NUMERIC(p,s)` 列は行に列の `scale` を持たない代わりに、比較リテラル自身の
小数桁数をそのまま `scale` として `numeric::parse_literal_exact` で解析する
（列の `scale` へ丸めると `x > 1.005` が `x > 1.01` に化けてしまい範囲比較の
意味が変わるため）。異なる `scale` を持つ `Decimal` 同士の正確な比較は
`numeric::cmp_exact` が担う。

`cmp_exact` の手順:

1. 整数部を `div_euclid`（負数でも余りが非負になる床除算）で比較する。
2. 整数部が一致する場合のみ、小数部（`rem_euclid`。常に `[0, 10^scale)`）を
   大きい方の `scale` へ底上げして比較する。クロス乗算（両者の全桁を
   掛け合わせる）ではなく差分桁数だけ底上げする方式のため、底上げ後の値は
   `10^MAX_PRECISION`（38 桁）未満に収まり `i128` の乗算がオーバーフロー
   しない。

### 索引・実行計画（`sql/scalar_plan.rs`・`sql/scalar_index.rs`）

`FilterOp::TypedCompare` は二次索引（`ScalarIndex`）が対応しない
（`candidates_for` が `None` を返す）ため、`classify_scalar_plan` は
`BoolEquals` と同じ理由で単独・複合述語のいずれでも常に `PlainScan` へ
倒す（`mask_trusted_defer`／`count_star_only`／`observe_group_by_count_only`
が誤って「索引で完全被覆済み」と信頼しないための単一情報源）。新型への
索引対応は別 Issue（#893）へ申し送り。

### 行ループでの評価

`WHERE` 述語の評価は既存の `declarative_filter::matches_all` をそのまま
使う（式ステップ列 `sql::expr_program::ExprProgram` には手を入れていない。
レーン A を実装する際に必要になる `eval`／`eval_predicate` への行スカラー
列引数の追加はスコープ外）。検索 `SELECT`（SCALAR 先行）・広域取得
`scan`・集計 `COUNT(*) WHERE`・`GROUP BY ... WHERE`・述語つき
`UPDATE`/`DELETE` のいずれも既存の `MetadataFilter` 経由の適用点をそのまま
通るため、新しい評価経路を追加していない。

## 後続対応（Issue #1183）

レーン A のうち、`WHERE`・投影・`GROUP BY` での数値列（INTEGER/BIGINT/REAL/
DOUBLE）の式参照・比較を Issue #1183（SQL-24・SQL-26・TABLE-13 ポインタ）で
受理した。`BoundExpr::ColumnRef` の束縛を `CHECK` 専用ポリシーから全式束縛へ
一般化し（`ColumnRefPolicy` を撤廃）、評価規則は #1075 の設計（算術は `f64`・
NULL は strict 伝播・BIGINT の |値| > 2^53 は `22000`）をそのまま引き継ぐ。
`TEXT` 列の範囲比較（`< <= > >=`）は束縛段で式レーンの TEXT×TEXT 比較（バイト順）へ
振り替える（`WherePredicate` の AST は不変）。`GROUP BY` は数値キーを型付きキー
として全走査経路で集計する（TEXT 辞書索引経路は単一 TEXT キー限定）。NoSQL の
`filter` は同じ式レーンへ写像する。二次索引による数値列の候補削減・数値列と
文字列リテラルの比較・TEXT の `BETWEEN`・照合順序は対象外。以下の「既知の制約」
のうちレーン A に関する記述は Issue #1183 で解消済み。

## 既知の制約（本 Issue の範囲）

- `NUMERIC` 列の裸の数値リテラル形（`price > 1.5`。引用符なし）は対象外。
  文字列リテラル形（`price > '1.5'`）のみを受理する。
- 逆向きの比較（`'2024-01-01' < day`）は対象外（構文段で式フォールバックへ
  回り `42601`）。
- 比較述語での文字列リテラルの型解釈（Issue #1408・SQL-24・ERR-2・ERR-6）:
  数値列（INTEGER/BIGINT/REAL/DOUBLE）と文字列リテラルの比較（`=`・範囲・`IN`・
  `BETWEEN`・`NOT`・逆向き）は列の型として解釈し、数値リテラル形と同じ結果にする
  （解釈不能は `22P02`、範囲外は `22003`）。BOOLEAN 列との文字列等価は `boolin`
  互換文法で解釈する。比較演算子が存在しない組み合わせ（TEXT／DATE／VECTOR 列 ×
  数値リテラル、BOOLEAN／UUID／BYTEA／ARRAY／JSON／ENUM 列 × 数値リテラル）は
  `42883`。算術演算の型不一致（`42804`）・NUMERIC 列 × 裸の数値リテラル・
  CHECK 本体の構文葉（`CHECK (qty > '0')`）・DATE／TIMESTAMP の逆向き文字列は対象外のまま。
  この型解釈は式束縛の共通経路にあるため、式形の `CHECK ('0' < qty)` も受理される。
- INTEGER/BIGINT/REAL/DOUBLE（レーン A）・投影/式中での新型列参照・
  非数値型の列同士の比較は対象外。
- 二次索引（`sql::scalar_index`）による候補削減は対象外（常に `PlainScan`）。
- `<>`／`!=` は対象外のまま。`OR`・括弧グループ（SQL-24 系）は Issue #912
  （TASK-208）で、`IN`・`BETWEEN`・`IS [NOT] NULL`・`NOT` は Issue #913
  （TASK-208・SQL-24）で対応済み（本 Issue #891 の範囲外だった当時の記述を
  更新。codex-review 指摘対応）。詳細は
  [sql24-in-between-null-not.md](./sql24-in-between-null-not.md) 参照。

## テスト

- `crates/engine/src/numeric.rs::tests`: `cmp_exact`（同一 scale・異なる
  scale・符号・0 境界・38 桁境界でのオーバーフロー非発生）。
- `crates/engine/src/declarative_filter.rs::tests`: 5 型それぞれの等価・
  範囲比較・型不一致拒否・リテラル形式違反・Describe ダミー値縮退。
- `crates/engine/src/sql/scalar_plan.rs::tests`: `TypedCompare` が単独・
  複合述語のいずれでも `PlainScan` に分類されること。
- `crates/engine/tests/scalar_types_predicates.rs`（新規）: SQL 表層の
  検索 `SELECT`・`scan`・集計 `COUNT(*) WHERE`・`GROUP BY ... WHERE`・
  述語つき `UPDATE`/`DELETE` の横断検証、NULL 非一致、型不一致の拒否、
  INTEGER 列（レーン A）が引き続き拒否されること、RLS 境界。
- `crates/engine/tests/{datetime_column,uuid_column,bytea_column}.rs`:
  既存の「WHERE 述語は拒否」テストを「WHERE 等価・範囲述語は受理」へ
  更新した（`numeric_column.rs` の裸数値リテラル形の拒否テストは無変更。
  レーン B は文字列リテラル形のみを対象とするため）。

## production コード変更ファイル

`crates/engine/src/numeric.rs`・`declarative_filter.rs`・`row_codec.rs`
（`ScalarRef::as_bytes` 新設）・`sql/allowlist.rs`・`sql/parser.rs`・
`sql/scalar_plan.rs`・`sql/scalar_index.rs`・`recovery/content_hash.rs`
（述語つき DML の `operation_id` 内容照合ハッシュへ `WherePredicate::
Compare` のタグを追加）。`sql/expr_program.rs`・`sql/udf_call.rs`
（レーン A 用に計画されていた変更）は無変更。wire-server は無変更。

## 追記: NUMERIC 列 × 裸の数値リテラルと負の数値リテラル（Issue #1430）

- NUMERIC 列と裸の数値リテラル（整数・小数・指数表記）の `=`・範囲比較（左右反転を含む）・`IN`・`BETWEEN` は、WHERE 文脈で `sql::parser` の束縛段が `compare_numeric_literal`（`TypedCompare`）へ振り替える。値の解釈は文字列リテラル形と同じ `numeric::parse_literal_exact` のため、結果・`wire_code` が一致し、NUMERIC 用の二次索引もそのまま使える。CHECK 本体は従来どおり `22000`。
- 負の数値リテラルは構文段で `-` + 数値を 1 つの数値へ畳む（`- 1` も可）。`- -1`・`-col`・`-(expr)` は `42601` のまま。
- 述語形 DML の content hash は、f64 で表せない整数リテラルだけ新タグ 11＋生テキストで直列化する（既存ハッシュは不変）。f64 へ丸めると別の十進値と同一視される小数リテラルも、正規化した十進文字列をタグ 11 で直列化して区別する（f64 で往復できる入力は従来ハッシュのまま）。
