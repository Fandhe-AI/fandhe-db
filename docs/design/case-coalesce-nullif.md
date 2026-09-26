# ADR: CASE 式と COALESCE / NULLIF（Issue #921）

## 対象

対象ビヘイビア: `docs/spec/04-behavior/sql-surface.md` SQL-26。
タスク: `docs/spec/05-tasks.md` TASK-210。
関連: SQL-9（宣言的 UDF、TASK-79）・Issue #353（式評価のステップ列コンパイル化）。

式層（`sql::udf_call` の `Expr`/`BoundExpr`/`ExprValue`、`sql::expr_program` の
ステップ列）に検索形 `CASE WHEN … THEN … [ELSE …] END`・`COALESCE(...)`・
`NULLIF(a, b)` と、これらに伴う NULL の概念を追加した。

## 設計判断

### NULL の表現

- 実行時値 `ExprValue::Null`／ステップ列スタック値 `StackValue::Null` を追加。
  `ExprType`（Scalar/Vector/Bool）は変えず、NULL の静的型は文脈（兄弟枝）から
  単一化する。
- 構文 `NULL` は `Expr::Null` として、`CASE`/`NULL` は既存の `AS`/`WHEN` と
  同じ文脈的キーワード（`Token::Ident` の大文字小文字非区別照合）で扱う。
  `Keyword` enum は変えない。
- `Expr::Null` を受理するのは `CASE` の THEN/ELSE・`COALESCE`/`NULLIF` の引数
  の 3 箇所のみ（`sql::udf_call::bind_null_aware`）。それ以外の位置の裸の
  `NULL`、および結果がすべて NULL で型が決まらない形
  （`COALESCE(NULL)`・`NULLIF(NULL, NULL)`・`CASE WHEN … THEN NULL END`）は
  `FeatureNotSupported`（`0A000`）で拒否する（式層に text 型が無いため）。
- NULL 伝播は共有関数で一元化する: `eval_binary`・`apply_builtin`
  （strict 関数扱い）はいずれかのオペランドが NULL なら NULL を返す。
  WASM UDF は RETURNS NULL ON NULL INPUT（NULL 引数ならバックエンドを
  呼ばない。ABI に NULL を表現する値が無いため）。

### AST・型規則

- `Expr`/`BoundExpr` に `Null`/`Case`/`Coalesce`/`NullIf` を追加（通常の
  `Call` と兼用しない: 遅延評価の意味論が異なる・`content_hash` のタグを
  明示できる・UDF 名前空間との衝突を避けられる、の 3 点が理由）。
  `is_reserved_function_name` に `coalesce`/`nullif` を追加し同名 UDF 登録を
  拒否する。
- 束縛済み `CASE` は `ELSE` 省略時に `BoundExpr::Null` を補って正規化する。
- 型検査: `CASE`/`COALESCE` は兄弟枝の型を単一化（不一致は `DatatypeMismatch`
  ＝`42804`）。`NULLIF` は両辺 Scalar 限定（既存の `=` が Scalar 同士しか
  比較できないため）。`CASE WHEN` の条件は許可リストが常に比較の
  `Expr::Binary` に限定するため実質的に到達しないが、束縛段でも Bool 型を
  重ねて検査する（多層防御）。既存の `bind_binary`/`bind_call` の型不一致
  （`22000`。SQL-9 の既存契約）は変えない。

### 上限

- `MAX_CASE_BRANCHES`（64）: 1 つの `CASE` あたりの `WHEN` 数。
- `MAX_CASE_NESTING`（8）: `CASE`/`COALESCE`/`NULLIF` の入れ子段数。構文段
  （`sql::allowlist::Parser::case_nesting`）と束縛段
  （`sql::udf_call::BindEnv::case_nesting`）の双方で独立に検査する。束縛段の
  検査は UDF 本体のインライン展開により実際のネストが構文段の計測値
  （文ごとの字面上のネスト）をすり抜けうるために必要（呼び出し元の現在の
  ネスト段数を `bind_call` のインライン展開時に引き継ぐ）。
- 既存の `MAX_EXPR_DEPTH`（32）・`MAX_EXPR_NODES`（1024）は変えず併用する。

### ステップ列コンパイル（Issue #353 拡張）

詳細は `docs/design/expr-step-compilation.md`「分岐命令の追加」節を参照
（ジャンプ命令・pc ループ・停止性・定数畳み込みの扱い）。

### 消費側の NULL 意味論

| 経路 | NULL の扱い |
| ---- | ---- |
| `WHERE` の式述語 | UNKNOWN として行を除外（`Bool(false)` と同じ） |
| 投影の式項目 | `Cell::Null` |
| 集計の `ScalarExpr` | NULL をスキップ（`SUM`/`AVG`/`MIN`/`MAX` は無視、`COUNT(expr)` は非 NULL のみ数える） |
| `CHECK` の式 | UNKNOWN は充足扱い（PostgreSQL 互換。既存の `Declarative` 腕「参照列 NULL は違反にしない」と同じ判断。NULL を返す式を書けるのは DDL 権限を持つ主体のみで制約の迂回にはならない） |

`VECTOR` 列が NULL の行（`dim == 0`）の既存ガード（WHERE/CHECK で UNKNOWN
扱い）は変えない。

### 永続化・ハッシュ

- `recovery::content_hash::push_dml_expr` に新タグ（5=Null、6=Case、
  7=Coalesce、8=NullIf）を割り当てた。詳細は
  `docs/design/multi-row-dml-operation-id.md` §4.4 参照。
- `sql::check_constraint::render_expr` は `CASE`/`COALESCE`/`NULLIF`/`NULL`
  のテキスト表現（`(CASE WHEN … END)`・`COALESCE(a, b)`・`NULLIF(a, b)`・
  `NULL`）を生成する。このテキストは永続化されて再パースされるため
  `parse(render(x)) == x` を単体テストで固定する（`CASE WHEN` の条件は
  周囲を括弧で囲まず `render_condition` でレンダリングする——`parse_case_expr`
  の `cond` 文法が括弧で囲まれた比較全体を受理しないため）。

## 対象外（out-of-scope-tracking）

- 単純 CASE（`CASE x WHEN v …`）: `42601` を維持。
- `WHEN` 条件内の `AND`/`OR`/`NOT`/`IS NULL`/`<>`/`IN`/`BETWEEN`: `BoundExpr`
  に論理演算ノードが無く、別 Issue（#913 等）に依存する。
- スカラー列（INTEGER/REAL/NUMERIC/TEXT 等）の式参照（レーン A）: 現状
  `COALESCE` の実用性は `NULLIF`/`CASE` の出力に限られる。
- SQL-26 が求める「組み込み関数名と同名の UDF 登録は `42723`」: 既存は
  `22000` のまま（SQL-9 の既存契約を変えない）。
- 既存の式層の型不一致（`bind_binary`/`bind_call` の `22000`）を `42804` へ
  統一すること。
- `ORDER BY` 位置での `CASE` 等の式。
- `VECTOR` 列が NULL の行を `VectorRef` で `ExprValue::Null` に写す意味論
  （既存の `dim == 0` ガードを維持）。
- 裸の `NULL` を任意位置で受理するための型推論（`id + NULL` 等。現状
  `0A000`）。
