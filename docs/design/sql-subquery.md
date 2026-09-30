# サブクエリ（スカラー・IN・NOT IN・EXISTS・NOT EXISTS）

- ステータス: **Accepted（実装既定値。当初計画からスコープを大きく縮小して実装）**
- 対応: Issue #927（`IN`／`EXISTS`）・Issue #1191（スカラー比較・`NOT IN`／`NOT EXISTS`・`IN` 対象型の拡大・相関サブクエリの `42601`）
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-29 (a)・`docs/spec/04-behavior/rls.md`
  RLS-10 (b)・`docs/spec/05-tasks.md` TASK-213
- 関連ポインタ: SQL-24・TASK-208（`WHERE` の `OR` 結合。本 Issue が再利用する
  `sql::where_tree::BoundOrGroup` の基盤）・TASK-212（複数テーブル実行計画。
  未実装・本 Issue は依存しない）

## 背景・目的

読み取り SELECT の `WHERE` で `IN (SELECT ...)`・`EXISTS (SELECT ...)` を
受理し、外側と同じ `PolicyContext`（RLS）で内側を実行した結果に基づいて
外側の行をフィルタできるようにする。

## スコープ（当初計画との差分）

実装時間の制約により、Issue 起票時に想定していた範囲から大きく絞り込んだ。
対応・非対応は次のとおり（非対応はすべて `42601`／`22000` で fail-closed に
拒否する。黙って無視・誤った結果を返す経路は無い）。

| 項目 | 対応状況 |
| ---- | -------- |
| `<col> IN (SELECT <単一列> FROM <table> [WHERE ...] LIMIT <n>)` | 対応 |
| `EXISTS (SELECT ... FROM <table> [WHERE ...] LIMIT <n>)` | 対応 |
| ネスト（`MAX_SUBQUERY_DEPTH` = 4 まで） | 対応 |
| 外側が広域取得 SELECT（`Statement::Scan`）・集計 SELECT（`Statement::Aggregate`） | 対応 |
| 外側がランキング付き検索 SELECT（`ORDER BY <=>`・`HYBRID`・`USING PLAN`） | **非対応**（構文は受理するが束縛時に `42601`） |
| WHERE 値位置のスカラー比較サブクエリ（`<col> <op> (SELECT ...)`。`<op>` は `= <> < <= > >=`） | 対応（Issue #1191）。逆向き（`(SELECT ...) <op> <col>`）・式への埋め込み・投影位置のスカラーサブクエリは **非対応**（`42601`） |
| 内側がランキング付き検索 SELECT・集合演算・JOIN | **非対応**（`42601`）。集計形（単一集計項目。`GROUP BY` の有無を問わず `LIMIT` 不要）はスカラー比較の内側に限り対応（Issue #1191）。`IN`／`EXISTS` の内側の集計形は非対応 |
| 内側の `LIMIT` 省略 | **非対応**（`42601`。内側は常に明示 `LIMIT` が必要） |
| 内側の `OFFSET` | 未検証（構文上は Scan 形状を再利用するため通るが、意味論は未確認。将来の Issue 課題） |
| 相関サブクエリ | **非対応**。束縛前の静的走査で `42601`（Issue #1191。内側スキーマに無く外側スコープのいずれかに有る非修飾列名の参照を検出する。どこにも無い名前は従来どおり `22000`） |
| `NOT IN`・`NOT EXISTS` | 対応（Issue #1191。`NOT IN` は NULL 規則込み。後述） |
| `IN` 対象列が疑似列 `id` | 実測範囲外（このリポの既存 `WherePredicate::Equality` 束縛自体が疑似列 `id` を対象にしていないため。`sql::subquery::cell_to_equality_predicate` の `Cell::Integer` 分岐は将来の拡張に備えて到達可能コードとして残す） |
| `IN` 対象値の型 | `TEXT`／`ENUM`／`BOOLEAN`／`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA`／`INTEGER`／`BIGINT`（Issue #1191 で拡大。内側の投影列は同じ値族であること）。`REAL`／`DOUBLE`／`VECTOR`／配列／JSON・疑似列 `id` は `22000` |
| 拡張クエリプロトコル（Parse/Bind、`$n`） | **非対応**（`42601`。理由は後述） |
| Describe（拡張クエリプロトコルの投影列導出） | 対象外（拡張クエリプロトコル自体が非対応のため） |
| `EXPLAIN`・カーソル `DECLARE`・`COPY (SELECT ...) TO`・CHECK 制約本体・`CREATE VIEW` 本体・述語形 `UPDATE`/`DELETE`・明示トランザクション内 | **非対応**（`42601`。すべて構文解析段でゲート） |

## 設計

### 文脈ゲート（構文解析段）

`sql::allowlist::Parser` に `subquery_ctx: Option<usize>`（`None` は不許可・
既定値）を追加した。`IN (SELECT`／`EXISTS (SELECT` を検出した位置
（`Parser::parse_where_leaf`）で `subquery_ctx` を確認し、`None` なら
`42601`、`Some(depth)` かつ `depth + 1 > MAX_SUBQUERY_DEPTH`（4）なら `54000`
にする（`Parser::require_subquery_depth`）。

`subquery_ctx` を `Some(0)` に設定するのは
`sql::allowlist::validate_sql_tokens_with_subquery_ctx`（`sql::allowlist::
validate_sql_with_subquery_ctx` の本体）を経由するトップレベルの読み取り
SELECT／集計 SELECT 解析だけで、`EXPLAIN`・カーソル・`COPY`・CHECK・
`CREATE VIEW` 本体・述語形 `UPDATE`/`DELETE` は既存の `Parser::new`（既定
`None`）のまま呼ぶため、個別の拒否腕を書かずに fail-closed になる
（`docs/design` レビュー観点: 新しい呼び出し経路を増やすたびに `Some` を
明示しない限り安全側に倒れる）。

内側（`WherePredicate::InSubquery`/`Exists` が保持する生トークン列）は構文
解析段では一切パースしない（開き括弧に対応する閉じ括弧をトークン走査で
見つけるだけ）。実行前解決（後述）が初めて内側をパースする。

### 解決（束縛前の AST 書き換え）

新設 `crates/engine/src/sql/subquery.rs::resolve_where_predicates` が、
`core.rs` の `Statement::Scan`／`Statement::Aggregate` 実行アームから、
束縛（`bind_scan`／`bind_aggregate`）の**前**・外側と同じ `read_txn`
（同一スナップショット）・同じ `PolicyContext` で呼ばれる。内側は
`sql::allowlist::validate_sql_tokens_with_subquery_ctx` → `sql::parser::
bind_scan` → `sql::scan::execute_scan` という、通常の広域取得 SELECT と
完全に同じ経路で実行する（第 2 の評価器を作らない。CLAUDE.md「委譲方針」）。
RLS は既存の実行器がそのまま適用するため、新しい可視性判定は無い。

解決結果は既存の `WherePredicate` へ書き換える（新しい評価器を
`sql::where_tree` に追加しない）:

- `IN`（TEXT／ENUM 対象列）: 内側の非 NULL・語彙内の値をソート・重複除去し、
  `declarative_filter::MAX_IN_LIST_ITEMS`（256）件以下のチャンクごとの
  `WherePredicate::InList` を分岐とする `Or` へ束ねる（Issue #1165）。各チャンクは
  既存経路（`DeclarativeFilter::in_list` → `FilterOp::InText`）でソート済み集合の
  二分探索に束縛される＝集合照合（第 2 の評価器なし）。1 チャンクでも必ず `Or` で
  包み、0 件は `Or(vec![])`（`where_tree::BoundOrGroup::matches` が必ず `false`
  を返す。TASK-208 で確立済みの意味論）。`Or` を含む文は
  `scalar_plan::classify_scalar_plan` が一律 `PlainScan` にするため、計画形状は
  内側データ量に依存せず `scalar_index_prune` にも影響しない。
- `IN`（BOOLEAN 対象列）: distinct は高々 2 のため従来どおり `BoolEquality` 葉の
  `Or`。
- `EXISTS`: 真なら述語自体を追加しない（制約を課さない）、偽なら
  `Or(vec![])`（常に偽）。

`sql::parser::bind_where_predicates_recursive` に `InSubquery`/`Exists` の
網羅腕を追加し、解決を経由せずここへ到達した場合は一律 `42601` にする
（ランキング付き検索 SELECT・`EXPLAIN` 等、解決を呼ばない経路の防御）。

### 上限（DoS 対策）

- ネスト深さ: `sql::allowlist::MAX_SUBQUERY_DEPTH` = 4（構文解析段で検査。
  実装既定値）。
- 1 文あたりの内側クエリ実行回数: `sql::subquery::MAX_SUBQUERY_EXECUTIONS`
  = 16（実装既定値。解決の再帰呼び出し階層全体で共有する `&mut usize`
  カウンタで検査。ネスト深さと組み合わせて評価コストを有界にする）。
- 内側の可視結果行数: `core::MAX_SEARCH_K`（10,000）超過は `54000`
  （内側 `LIMIT` の範囲検証とは独立の追加防御）。
- `IN (SELECT ...)` の distinct 値数: `sql::subquery::MAX_SUBQUERY_IN_VALUES`
  （`core::MAX_SEARCH_K` と同じ 10,000。Issue #1165 で旧 256 葉上限〔PR #1103
  codex-review 指摘対応で導入〕から集合照合方式へ見直して引き上げ。文全体で共有する
  `&mut usize` 予算で検査し、超過は `54000`。distinct 化の後・チャンク構築の**前**に
  distinct 値数を `checked_sub` で一括消費する）。予算は内側の**行**単位ではなく
  **distinct 値**単位で消費する（重複行は 1 件分。NULL・語彙外 ENUM ラベルは
  重複除去より前に除外し予算を消費しない。Cursor Bugbot 指摘対応の維持）。
  内側 1 回の可視行数は既に 10,000 で頭打ちのため、単一の `IN` サブクエリは実質
  上限なしで、複数の `IN` の distinct 合計のみが上限に効く。
  - 1 行あたり評価コスト: 分岐数（≤ ⌈10,000/256⌉ = 40 ＋ `IN` サイト数 ≤ 実行回数
    上限 16）× 二分探索（≤ 8 比較）≒ 最悪 450 比較。旧方式は最悪 256 比較で、
    同じ 10,000 値を旧方式で許すと 10,000 比較/行になる。集合サイズに対し
    線形以下（分岐数は N/256 に比例、各分岐は対数）。
  - 束縛時コスト: チャンクごとのソート O(k log k)・合計 O(N log N)。メモリは
    distinct 値 1 コピー（内側 `execute_scan` の結果〔各 ≤ `MAX_SCAN_RESULT_BYTES`、
    実行 ≤ 16 回〕以下）。
  - 実行回数上限（16）・内側可視行数上限（10,000）との組合せ: 最悪でも文全体の
    distinct 合計は 10,000 で有界（実行 16 回 × 10,000 行でも予算が先に尽きる）。
  - 不採用案: (1) 単一集合述語の新 `WherePredicate` variant（O(log N) 化できるが
    `pub enum` の網羅 match が多数あり BREAKING CHANGE になるため見送り）。
    (2) `bind_impl` の `MAX_IN_LIST_ITEMS` 検査の緩和・迂回（公開 API・NoSQL 表層と
    共有する事前防御点を弱めるため不可。チャンク幅で満たす）。
- `IN (SELECT ...)` の対象列（外側スキーマ）の存在・型検証は、内側の結果
  行数（0 行・NULL のみを含む）に関わらず必ず行う（PR #1103 codex-review
  指摘対応: 以前は変換後の葉が実際に束縛される時点でしか検証されず、内側が
  0 行／NULL のみだと存在しない列・非対応列の型を指定しても検証を回避できた）。
- `IN (SELECT ...)` は対象列（外側）・内側投影列の「値族」（`TEXT`／`ENUM`
  は同族、`BOOLEAN` は別族）が一致する組合せ（`TEXT`↔`TEXT`・`ENUM`↔`TEXT`・
  `BOOLEAN`↔`BOOLEAN` 等）だけを展開対象にし、それ以外は `22000` で拒否する
  （PR #1103 codex-review 指摘対応: 内側投影が疑似列 `id`〔`Cell::Integer`〕の
  場合、以前はそれを無条件に文字列化して外側 `TEXT` 列の `Equality` へ変換
  していたため、`id` の文字列表現と偶然一致する `TEXT` 値が誤って一致して
  しまっていた＝型の異なる値の暗黙同一視。この検証も内側の結果行数・値には
  一切依存しない静的な検証）。

自己点検（PR #1103 codex-review 指摘対応）: 同種の暗黙型変換が `IN` 以外の
サブクエリ経路にも無いか確認した。`EXISTS (SELECT ...)` は内側の可視行数の
有無のみを見る（`resolve_exists_subquery`）ため、セル値を述語へ変換する
経路自体を持たず対象外。スカラー比較サブクエリ（`<col> <cmp> (SELECT ...)`）
は本 Issue のスコープ外で構文自体を提供しない（`sql::allowlist::Parser` が
受理しない）ため同様に対象外。

- `EXISTS (SELECT ...)` は可視行が 1 件以上存在するかどうかしか使わないため、
  内側を `sql::subquery::InnerScanIntent::ExistenceOnly` で評価する（投影を
  空へ、`LIMIT` を実質 1 へ差し替える。`WHERE`・RLS の適用は通常の内側評価と
  完全に同一のまま＝可視性判定を迂回しない）。PR #1103 codex-review 指摘
  対応: 以前はユーザー指定の投影・`LIMIT` をそのまま使っていたため、
  可視行があっても `SELECT *` 等の広い投影×大きい `LIMIT` の組合せで
  `sql::scan::execute_scan` の結果バイト上限に達し `EXISTS` 文全体が
  失敗しえた。

自己点検（同修正時。EXISTS 側の資源上限修正と同種の問題が `IN` 側にも無いか
の確認）: `IN (SELECT ...)` の投影列数（ちょうど 1 列である契約）も、以前は
実行結果（`result.columns.len()`）からしか検査しておらず、`SELECT *` 等の
不正な内側クエリでも束縛・全件走査を最後まで終えてから拒否していた。
`validated.projection`／内側スキーマから投影列数を実行前に静的に確定できる
ため、`sql::subquery::execute_inner_scan` が束縛（`bind_scan`）・走査
（`execute_scan`）より前に検査するよう修正した（`resolve_in_subquery` 側の
実行後チェックは多層防御として残す）。

- `EXISTS (SELECT ...)` の投影を空へ差し替えるタイミングは、元の投影を
  `sql::parser::bind_projection` で束縛・検証した**後**に限る（PR #1103
  再々レビュー codex-review P1 指摘対応: 差し替えを先に行うと、
  `EXISTS (SELECT <存在しない列> FROM ... LIMIT 1)` のような不正な内側
  クエリが、実際には使わないという理由だけで列検証をすり抜け、可視行の
  有無だけで成否が決まってしまい、列検証・エラー契約〔通常の `SELECT` の
  未知列と同じ `22000`〕を破ってしまう）。`LIMIT` の差し替えは元々、
  差し替え前の値を `sql::parser::validate_search_limit` で検証済みだった
  （本自己点検で他に同種の「検証前に入力を差し替える」箇所が無いことを
  確認した。`OFFSET` は変更しない）。

- 内側にウィンドウ関数（`ValidatedScan::window_items`。SQL-30・TASK-214、
  Issue #930）が含まれる場合は `IN`／`EXISTS` いずれも `42601` で一律拒否
  する（PR #1103 Cursor Bugbot 指摘対応）。main へ後から合流した機能が
  `execute_scan` の内部分岐（`window_items` が非空なら `sql::window::
  execute_window_scan` へ委譲。`LIMIT` による早期終了なしに可視行を全件
  materialize する契約）を経由するため、`EXISTS` の `InnerScanIntent::
  ExistenceOnly`（投影・`LIMIT` の差し替えのみ）だけでは塞げず、ウィンドウ
  関数経由で同じ資源上限問題が再発しえた。サブクエリとウィンドウ関数の
  組合せは設計上未検証のため、正しく動く経路を作り込むのではなく
  fail-closed に倒した。

  自己点検（同種の問題を持つ他の新機能が無いか）: 内側は
  `sql::allowlist::validate_sql_tokens_with_subquery_ctx` が
  `Statement::Scan` を返す場合のみ受理し、それ以外（`ORDER BY`・
  `USING PLAN`・`GROUP BY`／集計・`SELECT DISTINCT`〔集計へ脱糖〕はいずれも
  `Statement::Select`／`Statement::Aggregate` になる）は構造上すでに一律
  `42601` で拒否される（`execute_inner_scan` の `_ => Err(unsupported(...))`
  腕）。集合演算（`UNION`／`INTERSECT`／`EXCEPT`）は `sql::allowlist::
  Statement` に該当する variant 自体が存在せず非対応。CTE（`WITH` 句）は
  字句解析段で `Token::Ident("WITH")` となり、`IN (SELECT`／`EXISTS (SELECT`
  の検出条件（次トークンが `Keyword::Select` であること）を満たさないため
  構造的にサブクエリ経路へ到達しない。以上より、早期終了を妨げる形で
  `LIMIT 1`・投影なしの経路をすり抜けられるのはウィンドウ関数のみだった。

### 拡張クエリプロトコルでの非対応

`sql::params::where_equality_literal_is_param` は Parse 時点の元トークン列
全体を位置で走査して `Ident '=' $n` を数え、束縛時の `equality_ordinal`
（出現順カウンタ）と対応させるダミーフラグ配列を組み立てる。本 Issue の
解決は束縛前に新たな `Equality` 葉を追加しうるため、これを拡張クエリ
プロトコル経由で許すとフラグ配列の添字と実際の束縛順序がずれ得る
（`$n` を含まない通常の等価条件に対する enum ダミー値検証スキップ判定を
誤らせる恐れがある）。安全側に倒し、`EngineCore::parse_sql_prepared` が
ダミー置換より前に元トークン列を走査して `IN (SELECT`/`EXISTS (SELECT`
を検出し、一律 `42601` で拒否する（簡易クエリプロトコル
`EngineCore::parse_sql`／`execute_sql` はこの経路を通らないため影響しない）。

## Issue #1191 追記: スカラー比較・NOT IN／NOT EXISTS・相関の `42601`

ポインタ: SQL-29 (a)・RLS-10 (b)・RLS-7・RLS-8・TASK-213・ERR-6（`42804`）。

### `NOT IN`／`NOT EXISTS`（非破壊）

新 variant は足さず `WherePredicate::Not(Box<InSubquery>)`／`Not(Box<Exists>)` で表す。
構文段は `require_subquery_depth` を `Not` で包む前に呼ぶため、サブクエリ不許可の
文脈（`EXPLAIN`・ビュー本体・CHECK・カーソル・`COPY`・述語形 DML・CTE 主クエリ・
集合演算の枝）は従来どおり `42601`。`sql::where_negation` は `NOT ( ... )` の De Morgan
押し下げで `Not(InSubquery)`／`Not(Exists)` を葉として残し、二重否定は畳む。解決段
（`sql::subquery`）が否定形を評価し、`Not` は束縛前に消える（解決を経由せず束縛へ届いた
場合は `42601`）。

- `NOT EXISTS`: 可視行が 1 件でもあれば `Or(vec![])`（常に偽）、無ければ述語なし（常に真）。
- `NOT IN`（対象列の存在・型・値族の静的検証は内側の行数に依存せず先に行う）:
  1. 内側 0 行: 常に真（外側値が NULL でも真。述語を追加しない）。
  2. 内側の結果に NULL を 1 つでも含む: 真にならない（`Or(vec![])`）。NULL の検出は
     語彙外 ENUM ラベルの除外より前に行う。
  3. それ以外: distinct 値のチャンクごとに `Not(InList)` を連言で並べる。外側値が NULL の
     行は宣言的フィルタの三値評価（UNKNOWN）で除外される。照合集合が空（語彙外ラベルのみ等）
     でも内側は非空のため、外側値が非 NULL の行だけを残す（`IS NOT NULL`）。
  整数列は数値リテラル `IN` と同じ式脱糖形（`Expression(col = n)` の `Or`。`NOT IN` は
  `col < n OR col > n` の連言）。1 サイトの distinct 値数は式ノード予算（1024）に収まる
  上限（`IN` は 256、`NOT IN` は 128）で、超過は `54000`。
- 否定を葉まで押し下げた後の葉で UNKNOWN を偽に潰すのは、上位が単調な AND／OR のみで
  あるため WHERE 最終判定と同値（`where_negation` のモジュールドキュメントと同じ根拠）。

### スカラー比較サブクエリ（BREAKING CHANGE）

`WherePredicate::ScalarSubqueryCompare { column, op, inner_tokens, depth }`（と
`ScalarSubqueryOp`）を追加した。`WherePredicate` は公開・非 `non_exhaustive` のため、
網羅 `match` を持つ外部コードは要対応。否定は演算子反転（`=`↔`<>`・`<`↔`>=`・`>`↔`<=`）。

解決結果は「同じ列型で `col <op> <リテラル>` と書いたときにパーサーが生成する AST と
同一形」にする（第 2 の評価器を作らない）:

| 対象列の値族 | 生成する述語 |
| ------------ | ------------ |
| `TEXT`／`ENUM` | `=`: `Equality`、`<>`: `Not(Equality)`、範囲: `Compare`（`TEXT` のみ。`ENUM` の範囲比較は `22000`）。`ENUM` の語彙外の値は `=` が常に偽・`<>` が非 NULL の全行で真 |
| `BOOLEAN` | `=`／`<>` のみ（`BoolEquality`）。範囲は `22000` |
| `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` | `=`: `Equality`、`<>`: `Not(Equality)`、範囲: `Compare`（値は wire と同じ既存フォーマッタによる正準テキスト） |
| `INTEGER`／`BIGINT`／`REAL`／`DOUBLE`／疑似列 `id`（数値族。集計の DOUBLE 結果とも比較可） | `Expression`（`<>` は `<` と `>` の `Or`） |

結果の扱い: 0 行または NULL は UNKNOWN（常に偽）。**2 行以上はエラー**（先頭行を採用
しない）。`21000`（cardinality_violation）相当の分類は本リポの `wire_code` 表に無いため、
既存分類の `22000` で PostgreSQL と同じ文言を返す。判定は内側の可視行のみに依存する
（他テナント行では発火しない）。投影列数 ≠ 1 は `42601`、値族の不一致は `22000`
（いずれも内側の行数・値に依存しない静的検証）。

拡張クエリプロトコルは `core.rs::contains_subquery_syntax` が比較演算子直後の
`( SELECT` を検出し、`$n` 併用は従来どおり `42601`（`$n` なしは簡易クエリと同じ）。

### 相関サブクエリの `42601`

内側が参照する非修飾の列名（投影・WHERE の全葉・式内の識別子・スカラー `ORDER BY`、
集計内側は集計引数・グループキー・`GROUP BY` 列）のうち、内側スキーマに無く外側スコープ
（ネストの深さ分の連鎖）のいずれかに有るものがあれば、束縛・走査より前に `42601`
（`correlated subqueries are not supported`）。PostgreSQL の名前解決順に合わせ、内外に同名の
列があれば内側を優先する。疑似列 `id` は全テーブルにあるため相関と判定しない。修飾参照は
内側の構文解析が `42601` にする。エラー文言は静的文字列のみ。

## 実装ファイル

| パス | 内容 |
| ---- | ---- |
| `crates/engine/src/sql/allowlist.rs` | `WherePredicate::InSubquery`/`Exists`/`ScalarSubqueryCompare`（BREAKING CHANGE）・`Parser::subquery_ctx`・`MAX_SUBQUERY_DEPTH`・`parse_select_shape`/`parse_aggregate_shape` の ctx 引数・`validate_sql_tokens_with_subquery_ctx` |
| `crates/engine/src/sql/subquery.rs`（新設） | 解決本体（`resolve_where_predicates`・`MAX_SUBQUERY_EXECUTIONS`） |
| `crates/engine/src/sql/parser.rs` | `bind_where_predicates_recursive` の防御的拒否腕 |
| `crates/engine/src/core.rs` | `Statement::Scan`/`Statement::Aggregate` アームでの解決呼び出し・`parse_sql`/`execute_sql` のサブクエリ許可化・`parse_sql_prepared` の拒否ガード |
| `crates/engine/src/sql/view.rs`・`check_constraint.rs`・`recovery/content_hash.rs` | 網羅 `match` の防御的拒否腕（いずれも到達不能。構文段のゲートで先に拒否される） |
| `crates/engine/tests/sql29_subquery.rs`（新設） | 結合テスト（受理・RLS 境界・深さ上限・文脈拒否・`NOT IN`／`NOT EXISTS`） |
| `crates/engine/tests/sql29_subquery_scalar.rs`（Issue #1191） | スカラー比較・相関 `42601`・`IN` 対象型拡大・RLS の結合テスト |

## OWASP Top 10 観点

- **A01 アクセス制御の不備**: 内側クエリは外側と同じ `PolicyContext` で
  既存の実行器（`execute_scan`）を通るため、RLS の暗黙適用を外す経路が無い。
  `tests/sql29_subquery.rs` の `in_subquery_only_sees_own_tenant_rows`・
  `exists_subquery_only_sees_own_tenant_rows` で 2 テナント間の越境が
  無いことを固定した。
- **A03 インジェクション**: 内側は括弧で区切ったトークン列を同じ許可
  リストのパーサーで再検証する。SQL 文字列の再構築・連結は行わない。
- **A04 安全でない設計（DoS）**: ネスト深さ・実行回数・結果行数の上限は
  いずれも実行・確保の前に検査する。
- **A04 fail-closed**: サブクエリを受理しない文脈は構文段で拒否し、
  解決を経由せず束縛に届いた場合も `bind_where_predicates_recursive` が
  二重に拒否する。
- **spec 機密**: 本ドキュメント・コード・コミット・テストは ID ポインタ
  （SQL-29・RLS-10・TASK-213）と、既に公開可能な数値基準（深さ 4・
  `MAX_SEARCH_K`）のみを含む。

## スコープ外（Issue 化はユーザー承認後に判断）

- 投影位置のスカラーサブクエリ（Describe が実行なしで結果列の静的型・列名を確定する必要があり、`Expr`／`SelectItem` の公開 enum 変更と型 OID 導出の設計が別途必要。`42601` 維持）
- スカラー比較の 2 行以上を `21000` で返すこと（`wire_code` 表への分類追加が前提。現状は `22000`）
- `IN`／`EXISTS` の内側の集計形・ランキング付き検索 SELECT・`REAL`／`DOUBLE` 等の `IN` 対象型
- 内側のウィンドウ関数（SQL-30・TASK-214、Issue #930）: `IN`／`EXISTS`
  いずれも内側に `window_items` が含まれる場合は `42601` で一律拒否する
  （PR #1103 Cursor Bugbot 指摘対応。理由は「上限（DoS 対策）」節参照。
  サブクエリとウィンドウ関数の組合せが正しく動く経路は設計上未検証のため、
  作り込むのではなく fail-closed に倒した）。
- 相関サブクエリ（`42601` で拒否。実行はしない）
- 拡張クエリプロトコル経由のサブクエリ・Describe 対応
- ランキング付き検索 SELECT（外側）でのサブクエリ対応
- サブクエリ述語に対するスカラー二次索引の最適化（現状は `PlainScan` 相当のまま。
  チャンク化 `InList` への `IndexInList` 適用による候補削減も同様に対象外）
- 性能の実測（受け入れ基準未設定）
