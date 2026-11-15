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
  重ねて検査する（多層防御）。`bind_binary`/`bind_call` の型不一致は
  Issue #1186 で同じ `42804` へ統一した。

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

## 単純 CASE（Issue #3279。SQL-26・SQL-42 のポインタ）

`CASE x WHEN v THEN … [ELSE …] END` を、CASE が書けるすべての文脈（投影・WHERE・ORDER BY・
集計・GROUP BY・CHECK・ビュー本文・INSERT VALUES の定数式・FROM の無い SELECT・UPDATE SET）で
受理する。意味は各分岐 `x = v_i`（NULL は一致しない）で、被演算子は 1 回だけ評価する。

- **AST**: `Expr::Case` に `operand: Option<Box<Expr>>` を足した。`None` が従来の検索形で、`Some(x)` では
  `whens[i].0` を条件ではなく比較値として読む。構文段で `WHEN x = v` へ脱糖して `x` を複製する案は採らない。
  `routine_fold`（`LANGUAGE sql|plpgsql` 関数の呼び出しを束縛前に畳み込む）は 2 番目以降の WHEN 条件を
  条件付き位置として副作用のある呼び出しを `0A000` にするため、複製すると被演算子の副作用が 1 回にならない。
  構造的な走査（列参照収集・列名書き換え・UDF 参照収集・ノード数・正規化・privilege 等）は `operand` も辿り、
  条件を解釈する箇所（束縛・定数畳み込み・パラメータ型推論）は `udf_call::simple_case_conditions`／
  `case_condition_exprs` で検索形の条件列へ下げてから既存の処理に流す。
- **開始判定**: `Parser::case_expr_starts_at` に一本化した（投影・集計項目・一次式・ORDER BY キーが共有）。
  引用なしの `CASE` の直後が `WHEN` なら検索形、そうでなければ括弧・入れ子の `CASE … END` の外で `WHEN` に
  当たれば単純形、`,` `)` `]` `;`・キーワード・`THEN`/`ELSE`/`END`/`AS`/`AND`/`OR` に当たれば列名 `case`
  （後方互換）。走査は `CASE_START_SCAN_LIMIT` トークンで打ち切り、超えたら偽（後続で `42601`）。
  互換性の注意: `CASE WHEN … THEN case + 1 WHEN …` のように引用なしの列 `case` に演算子が続きその後に
  別の `WHEN` が来る形だけは単純 CASE として読まれる（PostgreSQL では `CASE` は予約語でこの形は書けない）。
- **束縛**: `bind_case` が各分岐を `Binary Eq` へ下げて既存の比較と同じ型規則・unknown リテラル強制で束縛する。
  被演算子か比較値が裸の `NULL` の分岐は一致しない条件（`BoundExpr::Null`）にし、もう一方の辺は通常どおり
  束縛して列の存在・型を検査する（存在オラクルにしない）。被演算子の複製は束縛の `node_budget`（`54000`）が
  有界にする。`clock_timestamp()` を被演算子に含む WHEN 2 個以上の形は分岐ごとに壁時計を読み直して
  「1 回評価」と食い違うため `0A000` で拒否する（`now()` 等の取引固定の時刻は受理）。
- **`routine_fold`**: 被演算子を最初に 1 回だけ畳み込む（呼び出しは 1 回だけ起動して値に置き換わる）。
  第 1 分岐の比較値以外・すべての結果・ELSE は従来どおり条件付き位置。
- **描画・ハッシュ**: CHECK・ビュー本文の描画は `(CASE <operand> WHEN <v> THEN <r> … END)` で、
  `parse(render(x)) == x` を単体テストで固定した。内容照合ハッシュは検索形（タグ 6）のバイト列を変えず、
  単純形を新タグ 23（被演算子・分岐数・各 (値, 結果)・ELSE 有無）にした
  （`multi-row-dml-operation-id.md` §4.4）。同値の検索形とは別内容として扱う。
- **パラメータ**: 拡張クエリの `$n` を単純 CASE の被演算子・比較値に置く形は、パラメータ位置の許可リスト
  （`sql::params`）の外のため Parse で `42601`（fail-closed）。型推論（`param_infer`）は下げた条件を辿る。

## 対象外（out-of-scope-tracking）

- 単純 CASE の被演算子に比較・論理式を書く形（`CASE a > 1 WHEN true …`）: 値式の文法外で `42601`。
- 単純 CASE の WHEN 値の `IN` 列挙（`WHEN 1, 2`）: PostgreSQL の SQL 式でも書けないため対応しない。
- 単純 CASE の被演算子・比較値への `$n`（拡張クエリ）: パラメータ位置の許可リストの拡張が必要で、
  検索形の式内 `$n` と同じ別課題。
- `WHEN` 条件内の `AND`/`OR`/`NOT`/`IS NULL`/`<>`/`IN`/`BETWEEN`: `BoundExpr`
  に論理演算ノードが無く、別 Issue（#913 等）に依存する。
- スカラー列（INTEGER/REAL/NUMERIC/TEXT 等）の式参照（レーン A）: 現状
  `COALESCE` の実用性は `NULLIF`/`CASE` の出力に限られる。
- SQL-26 が求める「組み込み関数名と同名の UDF 登録は `42723`」: Issue #1186
  で解消した。
- 既存の式層の型不一致（`bind_binary`/`bind_call`）の `42804` 統一: Issue
  #1186 で解消した。
- `ORDER BY` 位置での `CASE` 等の式。
- `VECTOR` 列が NULL の行を `VectorRef` で `ExprValue::Null` に写す意味論
  （既存の `dim == 0` ガードを維持）。
- 裸の `NULL` を任意位置で受理するための型推論（`id + NULL` 等。現状
  `0A000`）。
