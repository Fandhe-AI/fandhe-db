# 非再帰 CTE（`WITH` 句）の設計判断

Issue #928・対象ビヘイビア: SQL-29 (b)（TASK-213）。関連ポインタ:
RLS-10 (b)・RLS-7・RLS-8（暗黙適用）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 概要

CTE を「クエリの中だけで有効な名前なしビュー」として扱い、参照時にインライン
展開する。`docs/design/create-view.md`（TABLE-18・TASK-205、Issue #909）と
同型の設計で、第 2 の SQL パーサー・実行器は作らない。CTE 本文は `CREATE
VIEW` 本文と同一の許可パーサー（`sql::allowlist::parse_view_body`）で検証し、
名前解決は `sql::view::resolve_from` と同じ「内側から外側へ畳み込む」処理を
`sql::cte::resolve_relation` へ切り出して共有する。

## 受理する構文

```
WITH <name> [(<列名>, ...)] AS (SELECT <* | 列名リスト> FROM <table | view | cte> [WHERE <単純述語> [AND ...]])
    [, <name> [(<列名>, ...)] AS (...)]*
<主クエリ: SELECT <* | 列名リスト> FROM <name> [WHERE ...] LIMIT n>
```

- CTE 本文は `parse_view_body` の受理範囲（`LIMIT`・`ORDER BY`・`USING PLAN`・
  集計・式項目・UDF 呼び出し述語はいずれも `42601`）に加え、本文内の `;`
  （`Token::Punct(';')`）も明示的に拒否する。`parse_view_body` が使う
  `expect_end_of_statement` は末尾の単一 `;` を許容してしまうため、CTE の
  括弧内トークン列に対しては呼び出し前に別途検査する。
- 主クエリは広域取得（SQL-15。`ParsedSelect::Scan`。スカラー `ORDER BY`・`OFFSET`
  を含む）と、Issue #1191 で追加した集計 SELECT（`GROUP BY`／`HAVING`／`ORDER BY`／
  `SELECT DISTINCT` の脱糖形を含む）を受理する。集計主クエリは `build_scan_from_resolved`
  の兄弟 `build_aggregate_from_resolved` で `ValidatedAggregate` へ畳み込み（CTE の
  `WHERE` は主クエリの `WHERE` の前へ合成）、集計引数・グループキー・`GROUP BY` 列・
  `WHERE` は CTE の公開列集合に収まることを `sql::view::check_aggregate_columns_within_view`
  で検査する（範囲外は `22000`）。`HAVING`・集計 `ORDER BY` は SELECT リストの出力名を
  指すため列検査の対象外。順位付き（`ORDER BY <ベクトル>`／`USING PLAN`）は明示的な
  メッセージで `42601` に拒否し、`WITH` 句を剥がして後段へ流すことは絶対にしない
  （同名の実テーブルを黙って読む危険があるため）。
- CTE 名の重複は `42601`。データ変更 CTE（`WITH x AS (DELETE
  ...)` 等）は本文の構文・名前・`RETURNING` の公開列までを受理し、実行は未対応のため
  固定 `0A000` で止める（SQL-63・Issue #1875。[data-modifying-cte.md](data-modifying-cte.md)）。
  `WITH RECURSIVE` は本経路に流さず `sql::recursive_cte` へ分岐する
  （構造検証の `42P19`・評価未実装の `0A000`。[recursive-cte.md](recursive-cte.md)）。
- 定義数上限（`MAX_CTE_DEFINITIONS = 16`）・連鎖の深さ上限
  （`MAX_CTE_NESTING_DEPTH = 4`）はハード上限（＝既定値）で、起動フラグ
  `--max-cte-definitions`・`--max-cte-nesting-depth` により下げられる
  （Issue #1552。[nesting-limits-cli.md](nesting-limits-cli.md)）。超過はいずれも `54000`。加えて、CTE 名の
  解決回数を「1 回のトップレベル呼び出し（1 つの CTE 定義の事前検証、または
  主クエリの解決）」単位でリセットするカウンタ（`MAX_CTE_REFERENCES = 32`。
  将来 JOIN が入ることに備えた明示的な上限）も超過時は同じく `54000`。この
  3 つは独立の上限ではなく、深さ上限が 1 回の呼び出し系列の長さを構造的に
  `MAX_CTE_NESTING_DEPTH + 1` 段に抑えるため、参照回数上限は現状の単一
  リレーション解決では実質到達しない（Issue #928 レビュー指摘: 文全体で
  1 つのカウンタを共有する実装は、事前検証ループが各定義のチェーンを
  再帰的に辿るたびに参照回数が累積し、定義数・深さのどちらも上限内の
  有効なクエリを誤って拒否する不具合があったため、呼び出し単位のリセットに
  修正した）。
- 拡張クエリプロトコルの `$n` を含む `WITH` 文は一律 `42601`（カーソルと同じ
  fail-closed 方針。理由は下記「対象外」節）。

## 名前解決（PostgreSQL と同じ意味論）

CTE 名はカタログのテーブル・ビューを隠す。i 番目の CTE の本文から見えるのは
0..i-1 番目の CTE だけで、見つからなければ `sql::view::resolve_from`
（ビュー→テーブル）へ進む。この可視範囲の境界により、自己参照・循環は構造的
に作れない（自己参照に見える名前は同名の実リレーションへ解決される。
`sql29_cte.rs::cte_name_hides_real_table_of_the_same_name` で固定）。

## 展開方式（検証段階でのインライン展開）

`sql::allowlist::validate_sql_tokens` に `WITH` 分岐を追加した。

- `parse_with_clause`（`Parser` のメソッド）が CTE 定義列を切り出し、各本文を
  `parse_view_body` で検証する。括弧の対応取りは `COPY (<SELECT>) TO STDOUT`
  と共有する `split_parenthesized` を再利用する。
- `sql::cte::resolve_relation` が CTE 名の解決を行い、一致すれば本文の FROM を
  再帰的に解決してから `sql::view::check_columns_within_view` で列スコープを
  検証し、内側の述語を先頭に置いて合成する。一致しなければ `sql::view::
  resolve_from` へ委譲する。
- ビュー・CTE いずれの解決結果（`sql::view::Resolved`）も、新設した
  `build_scan_from_resolved`（`sql::allowlist` 内、`validate_sql_tokens` の
  `Statement::Scan` 分岐と `WITH` 分岐の双方から呼ぶ）へ合流させ、
  `ValidatedScan` を組み立てる。これにより VIEW の既存の挙動をビット単位で
  変えずに CTE 経由の畳み込みと処理を共有する。
- 参照されない CTE も含め、すべての定義本文の FROM を検証する（決定性・
  fail-closed のため。構造検証・存在確認を省略しない）。

## RLS-10 (b)・RLS-7・RLS-8 の不変条件

`sql::cte::CteDef` は `tenant_id`／`PolicyContext`／作成者の情報を一切保持
しない（CTE はクエリ実行のたびに使い捨てで、カタログに永続化すらされない）。
畳み込み後の文は、参照した**セッション自身**の `PolicyContext` を使う既存の
実行経路（VIEW と共有する `build_scan_from_resolved` → `Statement::Scan`）
でしか評価されないため、作成者・他テナントの可視性が引き継がれることは構造
的に起こらない（`sql29_cte.rs` で 3 テナント対照検証済み）。

## 行数上限が構造的に満たされる根拠

CTE は原則として実体化しない（インライン展開。明示 `MATERIALIZED` の具体化経路だけは
末尾の「Issue #1860 追記」節の上限に従う）。主クエリ（`ParsedSelect::Scan`）の
`LIMIT` は既存の許可リストが `1..=MAX_SEARCH_K` に制限しているため、CTE 経由
でも直接クエリと同じ行数上限が構造的に適用される。CTE 自身が返す「中間結果」
という概念は実行時に存在しない（畳み込み後は基底テーブルへの `WHERE` 述語の
連言になる）。

## 対象外・申し送り

- サブクエリ、集合演算（別 Issue の対象）。CTE を JOIN の入力にする併用は Issue #1861 で対応済み
  （セッションの最上位 SELECT の入口に限る。末尾の「Issue #1861 追記」節）。
- 順位付き（`ORDER BY <=>`／`USING PLAN`）・`EXPLAIN` の主クエリで
  CTE を参照すること（VIEW と同じスコープ縮小。集計は Issue #1191 で対応済み。
  ベクトル順位付き主クエリはキャッシュ・precision 契約への影響が未検証のため
  fail-closed で `42601` を維持した）。
- 拡張クエリプロトコルで `WITH` 文に `$n` を束縛すること。`sql::params::
  validate_param_positions` の等価述語判定（パターン 4）はトークン列上の
  位置だけで許可位置を判定するが、CTE を含む文では CTE の `WHERE` 述語が
  主クエリの述語の前へ挿入される（`build_scan_from_resolved`）ため、束縛時に
  `$n` の対応先が組み立て後の述語列とトークン列上の位置とでずれる恐れがある。
  この対応ずれを解消してから別 Issue で扱う。
- `WITH RECURSIVE` の評価（構文・構造検証は受理済み。[recursive-cte.md](recursive-cte.md)）。
- NoSQL（HTTP）表層での CTE（SQL-28〜30 は非対応のまま）。

## Issue #1857 追記: 列名リスト（SQL-62 (2) ポインタ）

`WITH <name>(a, b) AS (...)` を受理し、CTE 本文の出力列へ先頭から順に名前を付け直す。

- 実現方式はビューの列別名（TABLE-41）と同じ「公開名 → 物理名」の対応表（`column_map`）。
  読み取りは `parse_optional_view_column_aliases`、適用は `sql::view::apply_column_aliases` を
  ビューと共有し、第 2 の実装は持たない。外側の `WHERE`・`ORDER BY`・集計は別名で書け、
  元の名前は公開列の外として既存の列スコープ検査で拒否される。
- リストが出力列より短いときは残りを元の名前のまま受理し、**長いときだけ `42P10`**
  （PostgreSQL 17 と同じ。エラーコードの対応は `docs/spec/04-behavior/error-format.md`）。
  文言には利用者が書いた CTE 名と件数だけを含める。
- 参照されない CTE も事前検証ループで同じ検査を受ける（長すぎるリストは参照の有無に関係なく `42P10`）。
- リスト内の重複は構文段で `42701`、短いリストの別名が残りの元の列名と重なる場合は `42702`、
  空リスト `()` は `42601`、件数超過は `54000`。PostgreSQL は重複を参照時の曖昧エラーにするが、
  本実装は fail-closed 側に倒して早期に拒否する（差分）。
- CTE の連鎖（`WITH a(x) AS (...), b AS (SELECT x FROM a) ...`）では内側の対応表を伝播する。
  従来の「列別名付きビューを CTE から参照すると `0A000`」は外し、同じ伝播で受理する。
  段の `WHERE` は内側の公開名で検査してから物理名へ書き換える。
- 疑似列 `id` に別名が付く形（`WITH c(x) AS (SELECT * FROM t)`）を外側の述語で参照すると、
  ビューの列別名と同じく `0A000`（fail-closed）。投影だけなら受理される。
- 元の名前での参照は本実装では既存の列スコープ検査の結果（`42703` 相当）に従う。
- RLS-10 (b) の不変条件は不変。`CteDef` は列名の写像だけを追加で持ち、テナント情報を持たない。

## Issue #1858 追記: `MATERIALIZED`／`NOT MATERIALIZED`（SQL-62 (3) ポインタ）

- `WITH x AS [[NOT] MATERIALIZED] (<body>)` を構文として受理し、`parse_with_clause` が読み捨てる。
  `CteDef` は変更せず、実行経路にも渡さない。現行の CTE は常にインライン展開されるため、
  指定の有無で結果は変わらない（ビュー本文内の CTE にも同じく適用される）。
- `NOT` の後に `MATERIALIZED` が無い形・重複指定・括弧なし・`AS` より前や本文の後ろへの指定は `42601`。
- 既知の差分: 引用識別子 `"MATERIALIZED"` も識別子トークンとして受理される（PostgreSQL は構文エラー）。
  既存の文脈キーワードと同じ扱いで、受理側に緩いだけで安全性に影響しない。
- 実際に一度だけ評価する実体化は対象外（別項目）。

## Issue #1860 追記: 具体化経路（SQL-62 (1)(3)(5)(6) ポインタ）

- 発火条件: セッションの最上位 SELECT の入口（`validate_session_sql_tokens`）で、明示 `MATERIALIZED`
  の CTE を主クエリの FROM が**直接**参照するときだけ。指定なし・`NOT MATERIALIZED`・
  ビュー本文・cursor・COPY・`INSERT ... SELECT`・CTAS・サブクエリ内側の再検証は従来のインライン展開
  （結果は同じ）。CTE 連鎖の内側は `MATERIALIZED` でもインライン合成する（文単位の保持は #1861）。
- 方式: 第 2 の実行器は作らず、既存の評価後射影形ビュー経路（`Statement::BufferedView`）を再利用する。
  本文を先行 CTE をインライン合成した 1 つの `Statement::Scan`（`LIMIT` なし＝暗黙上限つき）にし、
  主クエリの射影・`WHERE`・`ORDER BY`・`LIMIT`・外側集計を評価済みの行に適用する。`CteDef` は
  評価方式のヒントだけを追加で持ち、テナント情報を持たない。
- エラー契約: 先に従来のインライン解決・畳み込みを通して列スコープ等の検証を行い、失敗はそのまま返す。
  外側が評価後射影形として受理できない形（式投影・UDF 述語・サブクエリ・`WITH TIES` 等）は
  インライン展開へフォールバックする（現行の本文形ではインラインと結果が等しい）。
- スナップショット・RLS: 本文は参照したセッション自身の `PolicyContext`・外側と同一の `read_txn`
  で評価される。具体化結果は文の実行中のスタック上にだけ存在し、文の終了で破棄される
  （他の文・他セッションとは共有しない）。
- 上限: 本文は呼び出し段の `--max-result-rows` で実行し（ビュー本文は従来どおり既定値）、可視行が
  上限を超えたら黙って切り詰めず `54000`。判定は RLS 適用後の可視行だけで行い、他テナント行の
  存在は漏れない。Describe と Execute は同じ上限を使う。
- 対象外: 同一 CTE の複数回参照時の 1 回評価・JOIN 入力・指定なし CTE の既定の具体化は #1861、
  本文の `LIMIT`／`ORDER BY` は #1862、本文の集計・窓は #1863。`EXPLAIN` での表示は対象外。

## Issue #1861 追記: CTE を JOIN の入力にする（SQL-62 (1)(3)(5)(6) ポインタ）

- 発火条件: セッションの最上位 SELECT の入口（`validate_session_sql_tokens`）で、`WITH`（非再帰）の
  主クエリが JOIN の形のとき。FROM の各リレーションを**カタログより先に**大文字小文字を区別せず CTE 名と
  照合する（CTE は同名の表・ビューを隠す）。CTE を 1 つも参照しない JOIN は通常の JOIN として返す。
  ビュー本文・cursor・COPY・CTAS・`INSERT ... SELECT`・サブクエリ内側・`EXPLAIN` は従来どおり `42601`。
  データ変更 CTE を含む文は、検証を通した後に固定 `0A000`（SQL-63 の契約）で止める。
- 評価計画: `ValidatedJoin` に `cte_inputs`（`pub(crate)`。`None` なら従来の JOIN）を足し、到達できる CTE を
  slot（`Scan` 起点＝実表を読む本文、`Derived` 起点＝別 CTE の評価済みの行へ層を重ねる本文）にする。
  新しい `Statement` の variant は足さない（公開 enum を壊さない）。`collect_join_tables` は CTE の
  リレーションを含めず、同名の実表を黙って読む経路を作らない（schema 欠落は `Internal`＝fail-closed）。
- 評価方式（結果は方式で変わらない）: 参照数は主クエリの FROM からの参照と、到達できる CTE 本文の FROM
  からの参照の合計。`MATERIALIZED` は共有、指定なしは参照数 2 以上で共有（PostgreSQL と同じ既定）、
  `NOT MATERIALIZED` は共有せず参照ごとに評価する。共有する slot の本文は文内で 1 回だけ評価し、
  CTE→CTE の連鎖の内側も文単位で保持する。単一表の主クエリ（Issue #1860 の経路）での連鎖は、構造上
  1 回しか参照されずインライン合成でも評価は 1 回で結果も等しいため据え置く。
- 側入力: JOIN の側スキャン（プッシュダウン済みの述語・側投影）を、評価済みの行へ
  `view_buffered::plan_rows_layer`／`apply_rows_layer`（評価後射影形ビューの外側と同じ束縛・評価の規則）で
  適用する。JOIN はビュー入力（インライン合成の側スキャン）に未対応のため、方式指定にかかわらず
  すべての CTE リレーションを評価済みの行の側入力にする。側入力は `JoinBudget` と `max_input_rows` に
  課金する（自己結合では行を複製し 2 回分）。
- メモリ予算（PR #2729 指摘対応）: CTE 本文の評価結果・共有キャッシュへの保持・参照ごとの複製・
  `Derived` の層適用は、後続の JOIN と共有する文全体の 1 つの `JoinBudget` へ**確保の前に**計上する
  （`Scan` 起点の本文は走査へ残予算を上限として渡し、生成中に打ち切る）。側入力は所有権ごと渡して
  消費し、複製を作らない。超過は `54000`。独立した既定予算を複数持たない。
- 導出できない層: 本文が先行 CTE を読むのに層（`WHERE` の UDF・`ILIKE` 等）を評価後の行へ導出できない形は、
  親をインライン合成すると親が slot とは別に基底表から再評価され `MATERIALIZED` の文内 1 回評価・可視行上限を
  迂回するため、評価前に固定文言の `0A000` で拒否する。
- 行 `id`（符号なし）: CTE が公開した `id`（別名含む）の列は、側述語・導出層では `plan_rows_layer` が
  `NUMERIC` 相当（`u64` を損失なく評価）で束縛し、JOIN の束縛器は疑似列 `id` と同じ扱い（型なし）で
  `ColumnMeta::Id`／`AliasedId` として公告する（`int8` へ丸めない）。この列への集計は `0A000`。
- 合成スキーマ: slot の結果列（列名リスト適用後）から `RowIdentity::Allocated` のスキーマを作る。暗黙の
  疑似列 `id` を持たないため、`*` は CTE の公開列だけを展開し、`c.id` は本文が公開した `id` 列にだけ
  解決される。疑似列 `id` の結果列は `BIGINT` の実列として載せるが、セルは `u64` のまま（結合キーでは整数クラスで比較され、
  `i64::MAX` 超でも誤比較しない。公告・束縛は下の「行 `id`」項）。Execute と Describe は同じ `join::cte_schema` で作る。
- fail-closed（`0A000`、固定文言）: 非バイト列順の照合（ICU）の列を持つ表を読む CTE（照合は評価済みの行へ
  運ばれず、インラインへ退避する経路が無いため）、重複した列名を返す本文、型を持たない列。
  VECTOR／ARRAY 列は素通しでき、キー・述語での参照は既存の JOIN 束縛の規則で拒否される。
- 上限: slot の本文は呼び出し段の `--max-result-rows` で実行し、可視行が超えたら黙って切り詰めず `54000`
  （SQL-62 (5)）。外側の述語がプッシュダウンされないため、PostgreSQL のインライン展開より早く `54000`
  になりうる。判定は RLS 適用後の可視行だけで行う。
- スナップショット・RLS: slot は参照したセッション自身の `PolicyContext`・同一の `read_txn` で評価する。
  保持した行は文の実行関数のローカル変数だけに置き、文の終了で破棄する（他の文・他セッションとは共有しない）。
  明示トランザクション内では同じトランザクションの未 commit の書き込みを読む（dirty 判定は文全体）。
- Describe: 本文は実行せず、slot の結果列だけを導出する（`Scan` は広域取得の Describe、`Derived` は層の計画）。
- 一時表: 本文が一時表だけを読む文は一時コアで実行し、永続表と混在する文は従来どおり `0A000` で拒否する。
- 観測: 本文の評価回数は結果から観測できないため、`EngineCore::materialized_cte_eval_stats`
  （`VectorCore` trait 外の固有 API。テナント ID・行数を含まない）で「記憶域に対する本文の実行回数」を
  公開する。`Derived` の導出は数えない。
- 対象外: 本文の `LIMIT`／`ORDER BY`（#1862）、集計・`GROUP BY`・窓（#1863）、派生表（FROM の部分クエリ）、
  主クエリのサブクエリからの CTE 参照、集合演算の主クエリ、`EXPLAIN` での具体化の表示、NoSQL 表層、
  JOIN 入力の非共有 CTE への述語プッシュダウン。
