# ADR: WHERE の IN / BETWEEN / IS [NOT] NULL / NOT 述語（Issue #913）

- ステータス: Accepted（実装済み）
- 対応: Issue #913（TASK-208・SQL-24。ポインタ: `docs/spec/05-tasks.md`
  TASK-208、`docs/spec/04-behavior/sql-surface.md` SQL-24）。spec 本文は転記
  しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/sql/allowlist.rs`（`WherePredicate`・
  `Parser::parse_where_leaf`／`try_parse_structural_leaf`／
  `parse_in_list_body`／`parse_between_bounds`）・
  `crates/engine/src/declarative_filter.rs`（`FilterOp`・`bind_filter_op`・
  `MetadataFilter::eval`）・`crates/engine/src/sql/parser.rs`
  （`declarative_leaf_to_filter`）・`crates/engine/src/sql/scalar_plan.rs`
  （`ScalarPlan::IndexInList`）・`crates/engine/src/sql/scalar_index.rs`
  （`ScalarIndex::candidates_for`）・`crates/engine/src/recovery/
  content_hash.rs`（`push_dml_where_predicate`）
- 関連 doc: `docs/design/scalar-types-predicates.md`（TABLE-13・TASK-199、
  Issue #891。本 Issue が対象とする 5 型〔DATE/TIMESTAMP/NUMERIC/UUID/
  BYTEA〕の宣言的経路を新設した先行実装）・`docs/design/scalar-secondary-index.md`
  （二次索引の設計検討）
- 関連 Issue: #912（`OR`・括弧グループ。並行 Issue）・#914（`LIKE` の中間一致
  等の拡張。並行 Issue。`NOT LIKE` は本 Issue で `Not(Prefix)` として受理する
  ため #914 側での合流が必要）

## 背景・目的

`WHERE` の許可形は等価・前方一致・型付き範囲比較・BOOLEAN 系・`visible()`・
`id`/VECTOR 系の式比較のみだった。SQL-24 の一部として `IN (<lit>, ...)`・
`BETWEEN`・`IS [NOT] NULL`・`NOT` を受理し、PostgreSQL 互換の三値論理で評価
する。

## 構文段（`sql::allowlist`）

`Parser::parse_where_leaf` を新設し、まず構造的に確定できる葉
（`try_parse_structural_leaf`。既存の等価・前方一致・述語呼び出し・BOOLEAN
系・範囲比較に加え、`IS [NOT] NULL`・`[NOT] IN (...)`・`[NOT] BETWEEN ... AND
...`・`NOT LIKE`・BOOLEAN 裸参照を追加）を試し、一致しなければ前置 `NOT` を
試し、最後に式述語へフォールバックする。

- 前置 `NOT` は連続する個数をループで数え、偶奇で畳む（再帰させない。三値
  論理では `NOT NOT x ≡ x` が厳密に成り立つ）。畳み込み後の内側が
  `PredicateCall`（`visible()`）になる場合、直後が `(`（括弧グループ。#912
  の対象）の場合、または構造的な葉に一致しない場合（式述語）はいずれも
  `42601`——`WherePredicate::Not` の内側が `Not`・`PredicateCall`・
  `Expression` になることはないという構文段の不変条件を保つ。
- `IN` の要素数上限は `MAX_IN_LIST_ITEMS = 256`（`declarative_filter::
  MAX_METADATA_FILTERS` と同値）。要素を `Vec` へ push する**前**に上限を
  判定し、超過は `54000`。空リスト・非文字列要素（数値・`NULL`・`$n` を
  含む）はいずれも `42601`。
- `BETWEEN` の内側の `AND` は葉の内部で消費し、外側の `AND` 連結ループへは
  渡さない。
- `IS` に一致した以上、`NULL`／`NOT NULL` 以外の形（`IS TRUE`・`IS DISTINCT
  FROM` 等）は式フォールバックへ回さず `42601`。

## 意味層（`declarative_filter`）

`FilterOp` に `InListLiteral`／`InText`／`InTyped`／`BetweenLiteral`／
`Between`／`IsNull`／`IsNotNull`／`Not` を追加した（**破壊的変更**。網羅的
`match` を持つ外部コードは要対応）。束縛（`bind_filter_op`）は列型で
`InListLiteral` を `InText`（TEXT/ENUM・辞書等価。ソート＋重複除去）または
`InTyped`（DATE/TIMESTAMP/NUMERIC/UUID/BYTEA・型付き等価）へ、
`BetweenLiteral` を `Between`（同 5 型のみ）へ確定させる。`IsNull`／
`IsNotNull` は `VECTOR` 列を拒否する（`row_codec::scan_scalar_columns_masked`
がマスク外・`VECTOR` 列に常に `None` を積むため、評価させると fail-open
になり得る）。`Not` は内側を再帰で束縛する（深さは構文段の不変条件により
常に 1）。

### 三値論理（`MetadataFilter::eval`）

`matches(value) -> bool` の内部を `eval(value) -> Option<bool>`
（`None` = UNKNOWN）へ一般化した。

- 既存の op（`Equals`/`StartsWith`/`BoolEquals`/`TypedCompare`）: `value` が
  `None`（NULL）のときも、型が合わない（`as_text()` 等が `None`）ときも
  **UNKNOWN**（型不一致を `Some(false)` にすると `NOT` で真に反転する
  fail-open になるため）。
- `InText`／`InTyped`: NULL は UNKNOWN、非 NULL は要素と一致するかで
  `Some(bool)`。
- `Between`: `Ge`∧`Le` の判定。`low > high` は評価が常に偽になる（エラーに
  しない。PG の `BETWEEN` と同じ扱い）。
- `IsNull`: `Some(value.is_none())`。`IsNotNull`: `Some(value.is_some())`
  （常に確定し UNKNOWN にならない）。
- `Not(inner)`: `inner.eval(value).map(|b| !b)`（UNKNOWN は UNKNOWN のまま）。

`matches(value) = eval(value) == Some(true)`。

### `matches_all` の範囲外インデックス

範囲外インデックス（列値を読めていない）を `Option<Option<ScalarRef>>::
flatten()` で NULL と同一視すると、`IsNull` が誤って真になる（fail-open）。
`matches_all` は範囲外を明示的に「常に不一致」（`IsNull`／`IsNotNull` を
含むすべての op で不一致）へ倒すよう修正した。

## 索引経路（`scalar_plan`・`scalar_index`）

`classify_scalar_plan` は `Not`／`IsNull`／`IsNotNull`／`InTyped`／
`Between{Bytes}` を（単独でも複合述語の一部でも）先頭で `PlainScan` へ倒す
事前ゲート（`BoolEquals`・`TypedCompare{Bytes}` と同じ設計）。単独の
`InText` は `ScalarPlan::IndexInList`、単独の `Between`（`Bytes` 以外）は
`IndexTypedRange` に合流する。

`ScalarIndex::candidates_for` は `InText` を値ごとの等価スロットの和集合
（ソート＋重複除去）、`Between` を `typed_compare_candidates(Ge, low)` と
`(Le, high)` の交差（`intersect_sorted`）として導出する。`mask_trusted_defer`
等が候補集合を厳密一致として信頼する契約上、索引対応に分類する op は
`candidates_for` が厳密なスロット集合を返さなければならない。

## CHECK 制約は非対応のまま

`sql::check_constraint::reject_forbidden_elements` は `InList`／`Between`／
`IsNull`／`Not` を含む CHECK を構築時に `42601` で拒否する。`enforce` の
「NULL なら常に合格」という短絡（三値論理で UNKNOWN を合格扱いする既存
実装）が `IS NOT NULL` の意味（NULL 行は不合格になってはならない）と
相容れないため、対応は別 Issue へ申し送る（TABLE-16 の既存挙動は不変）。

## ビュー

`sql::view::predicate_column` は `Not(inner)` で再帰し、内側の列を返す
（欠くと `NOT hidden_col = 'x'` のような否定越しに非公開列の存在情報が
漏れる。A01 アクセス制御の不備）。`CREATE VIEW` 本体の `WHERE` 句
（`sql::allowlist::parse_view_body`）は新しい 4 形をスコープ外のまま据え
置く（fail-closed。対応は別 Issue）。

## 再送判定のハッシュ（RECOVER-11）

`recovery::content_hash::push_dml_where_predicate` に新しい種別タグ
（9=`InList`、10=`Between`、11=`IsNull`〔`negated` を付加〕、12=`Not`〔内側を
再帰で直列化〕）を追加した（origin/main の Issue #912・`OR` が先にタグ 8 を
採番済みだったため、本 PR を origin/main へマージした際に 9〜12 へ採番し
直した。実装記録は `docs/design/implementation-status.md` のマージ記録参照）。
構文段が連続する `NOT` を偶奇で正規化するため、`NOT NOT x` と `x` は
同じハッシュになる（意図した正規化）。

## レビュー是正（PR #913・codex-review）

- `sql::exec` の DISTANCE 先行（`HINT ORDER(DISTANCE, ...)`）SCALAR 事後
  フィルタが、`candidate_columns`（`Value`）を判定直前に `row_codec::
  ScalarRef` へ逆変換していたため、`Value::Integer`／`BigInt`／`Array`
  （他の演算子では TEXT 前提のため従来 `None` へ丸めていた）が実 NULL
  （`Value::Null` の逆変換結果も `None`）と区別できなくなっていた。本 Issue
  で列型を問わず許容する `IsNull`/`IsNotNull` がこの `None` を「NULL」と
  解釈するため、非 NULL の INTEGER/BIGINT/ARRAY 列が `IS NULL` に fail-open
  で一致していた。`on_visible_row` が生の `ScalarRef`（実 NULL と型不一致を
  区別できる）を見ている時点で判定結果を `postfilter_verdicts` として記録し、
  DISTANCE 段の後は逆変換を経ずその真偽値を引くだけに変更して解消した
  （`crates/engine/tests/sql24_in_between_null_not.rs` に回帰テストを追加）。
- `sql::allowlist::Parser::parse_where_leaf` で、前置 `NOT` の内側が後置
  `NOT`（`NOT LIKE`／`NOT IN`／`NOT BETWEEN`）由来の `WherePredicate::Not`
  だった場合、それをそのまま包むと `Not(Not(x))` になり、上記「`Not` の内側が
  `Not` になることはない」という構文段の不変条件に反していた（三値論理では
  `x` と評価結果は等価）。畳み込み処理を追加し、単一の `Not`（または前置
  `NOT` が偶数個なら畳んで消える）へ正規化した。

## Issue #1184: `NOT ( ... )` と数値リテラルの `IN`／`BETWEEN`

ポインタ: SQL-24・TASK-208（本文は転記しない）。

- **AST は増やさない**: `WherePredicate` に variant を足さず、既存の `Not`・`Or`・
  `Expression` だけで表す（公開 enum の破壊的変更を避ける）。束縛段・実行経路・
  content hash（RECOVER-11）は既存処理にそのまま乗る。
- **否定は構文段で葉まで押し下げる**（`sql::where_negation::negate_conjunction`）。
  `where_tree` の二値評価器は UNKNOWN を false として扱うため、`Or`／AND 群の上に
  `Not` を置くと NULL 行が真に反転して fail-open になる。De Morgan は Kleene 三値論理
  でも厳密に成立するので、否定は葉（既存の `Not(leaf)` または演算子反転）にだけ残る。
  `Expression(a = b)` は `BinOp` に `<>` が無いため `a < b OR a > b` に展開する。
- **`visible()` は否定できない**: `NOT (a AND visible())` は `42601`（RLS-7 の保証を
  否定側でも維持）。`NOT EXISTS`／`NOT IN (SELECT)` は従来どおり `0A000`。
- **数値リテラルの `IN`／`BETWEEN` は脱糖**: `col IN (n..)` は `col = n` の `OR`、
  `col BETWEEN a AND b` は `col >= a AND col <= b`。列型の判定は束縛段に任せ、
  `col = n` が受理される列だけが受理される（構文段は schema を知らない）。
  混在リスト・`NULL`・`$n`・単項マイナス・`BETWEEN SYMMETRIC`・空リストは `42601`。
- **上限**: 要素数は push 前に `MAX_IN_LIST_ITEMS` で検査（`54000`）。脱糖・否定で
  増えるノードは式ノード予算（`MAX_EXPR_NODES`）へ課金する。`NOT IN` は比較が 2 倍に
  なるため、要素数が約 170 を超えると `54000` になる。
- **CHECK 本体（TABLE-16）・ビュー本体・JOIN の WHERE は対象外のまま**: 脱糖後の形が
  CHECK の許可形と区別できないため、`Parser::in_check_body` で従来どおり `42601` にする。
  ビュー本体は `Expression`／`Or`／`Not` を拒否する既存契約のまま（`NOT (NOT x = 'v')`
  は `Equality` に畳まれて受理されるが意味は同値で無害）。
- **content hash の正規化**: `NOT (a AND b)` と `NOT a OR NOT b`、`id IN (1,2)` と
  `id = 1 OR id = 2` は同じ AST（同じハッシュ）になる。`NOT NOT x ≡ x` と同じ意図した正規化。
- **索引**: `classify_scalar_plan` は変更しない。`NOT (tag NOT IN ..)`・`id BETWEEN ..`・
  `NOT (id > n)` などは既存の分類に入り、`Or` を含む形（`id IN (..)`・`NOT (id BETWEEN ..)`）は
  `PlainScan`（結果は索引経路と一致することをテストで固定）。
- 対象外: `Or`／`Not` を含む述語の索引和集合、`BinOp::Ne` の追加（公開 enum の変更を伴う）、
  `$n` を要素にした `IN`、NoSQL の `filter`（#1197）。

## 対象外・申し送り

- CHECK 制約での新しい形の対応（`enforce` の三値化）
- `InTyped`と `IsNull`/`IsNotNull` の索引対応
- SQL-24 の性能基準（`IN` 8 要素・`OR` 2 項の p95）の実測（bench 側の受け入れ
  項目）
- NoSQL の `filter` への写像（TASK-223）
- `LIKE` の中間一致等の拡張（#914）
