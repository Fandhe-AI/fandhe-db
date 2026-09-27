# 束縛済み scan／aggregate 計画を実行する EngineCore のセッション対応エントリ

- Issue: #728
- 対象タスク: TASK-186（内訳「単一 `Storage` 構成での直接束縛と `EngineCore` の
  共存方式の確認・結線」）
- 対象ビヘイビア: NOSQL-3／NOSQL-4／NOSQL-5（関連 SQL-13／SQL-14／SQL-15）
- ステータス: Implemented

## 背景

`EngineCore` の SQL テキスト非経由の実行入口は SQL テキストを一切介さない値と
してのみ提供されていた（`sql::scan::execute_scan`／
`sql::aggregate::execute_aggregate`。それぞれ Issue #726・#727 で `pub` 昇格）。
しかしいずれも `&redb::ReadTransaction` を要求し、これは `Storage::db()`
（`pub(crate)`）経由でしか得られない。`EngineCore::from_storage` が `Storage`
を所有するため、engine クレート外の呼び出し元（`wire-server` の NoSQL 表層。
TASK-175・TASK-177 のポインタ）は production プロセス内で束縛済み計画を実行
できなかった（既存の公開 API テスト `sql_scan_public_api.rs`／
`sql_aggregate_public_api.rs` は `Storage` を drop して生 `redb::Database` を
再オープンする回避策を使っており、redb は同一ファイルの二重オープンを許さない
ため production では成立しない構成）。

本 Issue は、単一の `Storage` を `EngineCore` が所有したまま、SQL テキストを
経由せずに束縛済み scan／aggregate 計画を `PolicyContext`（RLS 暗黙適用）付き
で実行するセッション対応エントリを `EngineCore` に追加する。

## 採用した設計: binder closure 方式

`BoundScan`／`BoundAggregate` はスキーマ（列 index）に依存する。呼び出し元
（クレート外）は `EngineCore` からスキーマを取る公開手段を持たない。仮に別
トランザクションでスキーマを取って束縛すると、`ALTER TABLE ADD COLUMN` との
競合で旧スキーマの計画を新スナップショットへ適用する障害を再導入する（単一
スナップショット契約。Issue #56 の指摘対応の踏襲）。

そのため「束縛済み計画」は **同一 `read_txn` 内で呼ばれる binder closure の
戻り値** として受け取る:

```rust
pub fn execute_bound_scan_in_session<F>(
    &self,
    ctx: &PolicyContext,
    session: &SessionState,
    table: &str,
    bind: F,
) -> Result<QueryResult, SqlSurfaceError>
where
    F: FnOnce(&TableSchema, &UdfRegistry) -> Result<BoundScan, SqlSurfaceError>;

pub fn execute_bound_aggregate_in_session<F>(
    &self,
    ctx: &PolicyContext,
    session: &SessionState,
    table: &str,
    bind: F,
) -> Result<QueryResult, SqlSurfaceError>
where
    F: FnOnce(&TableSchema, &UdfRegistry) -> Result<BoundAggregate, SqlSurfaceError>;
```

処理順（両者共通）:

1. `table` のスキーマを同一トランザクションで取得する（`bind` はまだ呼ばない）。
   テーブル不存在は SQL 経路と同一の `UndefinedTable` へ丸め込む。
2. `bind(&schema, session.udfs())` を呼び、束縛済み計画を得る。`Err` はそのまま
   伝播する（engine 側でエラー分類を変えない）。
3. 得られた計画の対象テーブルが `table` と一致するか検証し、不一致は
   fail-closed に拒否する（他テーブル向けに束縛された計画を別スキーマで
   実行する事故への多層防御）。
4. 実行する。

設計上のポイント:

- `session` は `&SessionState`（不変借用）で足りる。scan／aggregate は
  `udfs()` しか読まず、`SearchMode`／`precision` は関与しない。
- RLS は既存の `execute_scan`／`execute_aggregate_with_cache` が `ctx` から
  暗黙適用する契約をそのまま使う（計画側の `rls_predicate_present` の有無に
  依存しない。新規のバイパス経路は作らない）。
- `bind` closure は `read_txn` が開いている間に呼ばれる。したがって呼び出し元
  の closure は純粋・軽量に保つ契約とする（I/O・LLM 呼び出し等の重い処理を
  closure 内で行わない）。

## engine 内の実行器一本化（リファクタ・挙動不変）

`execute_validated_in_session` の `Statement::Aggregate`／`Statement::Scan`
アームは、従来 `begin_read`・`get_table_schema_in_txn` を inline していた。
これは既存 `read_txn_with_schema`（`USING PLAN`／`EXPLAIN` アームが既に使って
いた private helper）と同一のエラー写像のため、両アームをこの helper へ置換
したうえで、実行本体を `run_scan_plan`／`run_aggregate_plan`（`core.rs` の
private メソッド）へ抽出した。SQL 経路は
`read_txn_with_schema` → `bind_scan`／`bind_aggregate` → `run_*_plan` に、
新エントリは `read_txn_with_schema` → closure → `run_*_plan` になり、
トランザクション・スキーマ・キャッシュ配線が 1 か所に閉じる。新エントリは
公開ラッパー `sql::aggregate::execute_aggregate`（キャッシュ非経由）と異なり、
`run_aggregate_plan` が配線する `VisibleBitmapCache`（Issue #478）・
`SqlArenaCache`（Issue #363）・`ScalarIndexCache`（Issue #473）を SQL 経路と
共有する。

## 却下した設計案

| 案 | 内容 | 却下理由 |
| --- | --- | --- |
| A | wire-server が `sql::exec::execute_statement`／`execute_scan`／`execute_aggregate` を直接呼ぶ | `&redb::ReadTransaction` を得る公開手段が無い（`Storage::db()` は `pub(crate)`・`EngineCore` が `Storage` を所有・redb は同一ファイルの二重オープン不可）。得られたとしてもキャッシュ非経由で SQL 経路と性能・挙動が乖離し、txn・スキーマ・キャッシュ配線を wire-server 側へ複製する＝第 2 の実行器になる。責務境界（プロトコル処理は wire-server、コアロジックは engine）にも反する |
| B | `EngineCore` から `Storage`／`db()`／`begin_read` を露出 | A と同じ複製問題に加え、永続化ハンドルのカプセル化を破り `core-api-check` の到達性契約を広げる |
| C | 束縛済み計画を値渡しで受ける `execute_bound_*(ctx, session, &BoundScan)` ＋ スキーマ公開アクセサ | スキーマ取得と実行が別トランザクションになり単一スナップショット契約（Issue #56）を破る。安全にするには列 index／名／型の再検証や世代照合が必要で API 面積が膨らむ。将来必要になれば本エントリの上に薄く足せる |

## スコープ外

- `BoundAggregate::new`（SQL テキスト非経由の直接構築 constructor）: Issue #768・
  TASK-177（aggregate 写像・NOSQL-4）で実装済み（`BoundScan::new` と同じ作法。
  `GROUP BY` を持たない単一行集計〔TASK-166・SQL-13〕限定。`AggregateInput` の
  `ScalarExpr` variant（複合式）は対象外のまま SQL テキスト経由のみ）。
  `GROUP BY`／`HAVING` を伴う計画〔TASK-167・SQL-14〕の直接構築は
  `BoundAggregate::new_grouped` として Issue #769・TASK-186・NOSQL-5 で実装
  済み（列名解決〔`resolve_group_by_column`〕・HAVING 対象の型検査
  〔`check_having_target_is_numeric`〕を SQL テキスト経由の
  `bind_group_by_clause` と共有。`ORDER BY`／`LIMIT` 相当は対象外のまま
  ——本エントリ・NoSQL 表層のスキーマにこれらに相当するキーが存在しない
  ため。上記の TASK-186 申し送りは解消済み）。
- `execute_insert`／`build_explain_result` の公開: Issue #730。

## search（`BoundStatement`）向けエントリ（Issue #764・TASK-186・NOSQL-2）

`vector` 指定は `execute_bound_scan_in_session` と同型の
`execute_bound_search_in_session<F>`（`F: FnOnce(&TableSchema, &UdfRegistry)
-> Result<BoundStatement, SqlSurfaceError>`）で足りる。`mode`（`precision`／
`recall`）は NoSQL 表層に `SET search_mode` 相当が無いため、呼び出し元が
`bind` の戻り値である `BoundStatement::mode` にすでに解決済みの値として
含める契約（`wire-server::http::query::search::bind_search` が
`mode::resolve_mode` を通す）。

`plan` 指定（`USING PLAN` 相当）は LLM 展開・再埋め込みという高コスト I/O を
挟むため、値渡しの束縛済み計画では表せない。SQL テキスト経由の
`Statement::Select`（`USING PLAN` 分岐）が内蔵していた一連の fail-closed
手順（`LIMIT` 範囲検証 → 辞書必須列の事前スキーマ検証 → スキーマ依存の
事前束縛検証 → 計画開始時のテーブル世代記録 → I/O（`plan_using_plan_expansion`）
→ I/O 完了後の世代照合 → 辞書必須列の再検証 → 束縛 → 実行）を、`core.rs`
内の private ジェネリック `run_using_plan_select<Pre, Bind>` へ抽出し、
SQL アーム・新エントリ `execute_bound_plan_search_in_session<F>` の双方が
共有する（第 2 の実行器を作らない設計を I/O を挟む経路にも一貫させる）。

```rust
fn run_using_plan_select<Pre, Bind>(
    &self,
    ctx: &PolicyContext,
    session: &SessionState,
    table: &str,
    question: &str,
    mode_literal: Option<&str>,
    limit: u32,
    pre_check: Pre,
    bind: Bind,
) -> Result<QueryResult, SqlSurfaceError>
where
    Pre: FnOnce(&TableSchema, &UdfRegistry) -> Result<(), SqlSurfaceError>,
    Bind: FnOnce(&TableSchema, &UdfRegistry, UsingPlanExpansionResult)
        -> Result<BoundStatement, SqlSurfaceError>;

pub fn execute_bound_plan_search_in_session<F>(
    &self,
    ctx: &PolicyContext,
    session: &SessionState,
    table: &str,
    question: &str,
    mode_literal: Option<&str>,
    limit: u32,
    bind: F,
) -> Result<QueryResult, SqlSurfaceError>
where
    F: Fn(&TableSchema, &UdfRegistry) -> Result<PlanSearchBinding, SqlSurfaceError>;
```

`mode_literal`（`USING MODE` 相当の生リテラル）は `execute_bound_plan_search_in_session`
自身では解析しない。`run_using_plan_select` がテーブル解決後に初めて解析する
ことで、テーブル未存在＋ `mode` 値不正の要求でも `42P01` が優先される
（SQL テキスト経由の `USING PLAN` と同一の fail-closed 順序。PR #827
codex-review 指摘対応で `Option<SearchMode>` から変更）。

設計上のポイント:

- `bind: F` は `Fn`（`FnOnce` ではない）。`run_using_plan_select` は
  `pre_check`（I/O 前・戻り値を捨てる多層防御）と `bind`（I/O 後・実際に
  使う）の 2 回、異なる用途で同じ束縛ロジックを必要とするため、
  `execute_bound_plan_search_in_session` は `bind` を両方へ渡す（呼び出し元
  の closure は 2 回呼ばれても副作用のない純粋な束縛のみを行う契約）。
- `bind` の戻り値 [`PlanSearchBinding`]（投影・フィルタのみを持つ
  `#[non_exhaustive]` 構造体）は、SQL アームの `sql::using_plan::
  bind_expansion` が返す `BoundStatement` から意図的に絞り込んである。
  `Ranking::Hybrid`（再埋め込みベクトル・本文列インデックス・展開後クエリ
  文字列）・`mode`（`resolved_mode`）は `execute_bound_plan_search_in_session`
  自身が `planned`（`UsingPlanExpansionResult`）から組み立てる——NoSQL
  表層（呼び出し元）はクエリベクトル・ランキング方式・モードを一切
  差し替えられない（`.claude/rules/security.md`「アクセス制御の不備」対応。
  `plan_using_plan_expansion` の `query_mode` は呼び出し元が事前に解析した
  値を渡す形へ一般化し、`ValidatedStatement` への依存を除去した）。
- `run_select_plan`（`execute_statement_with_cache` の薄いラッパー）を新設し、
  SQL アームの `ORDER BY` 分岐・`run_using_plan_select`・
  `execute_bound_search_in_session` の 3 箇所が同一のキャッシュ配線
  （`SparseIndexCache`・`SqlArenaCache`・`HnswIndexCache`・`ScalarIndexCache`）
  を共有する。

### 判定順序（`wire-server::http::query::search::execute` が固定する契約）

1. `table` の識別子形状検査（`42601`）
2. `vector`／`plan` の同時指定（排他違反）の拒否（`42601`）。スキーマ解決
   （テーブルの存在確認）を一切要さない、要求本文自身が抱える構造的な
   契約違反のため、他のどの検証よりも先にここで確定する（PR #827
   codex-review 指摘対応。以前は `plan` 分岐内のローカル検証
   〔`validate_using_plan_question`・`mode` 解析〕がテーブル解決前に
   位置していたため、`vector` も同時指定された要求では排他違反より先に
   別のエラーが返ってしまっていた）
3. `explain: true` の拒否（`vector`／`plan` のどちらか一方だけが指定された
   場合に限る。`vector` 指定は `42601`——SQL-6 の `EXPLAIN SELECT ...
   ORDER BY` 拒否と同じ分類、`plan` 指定は `0A000`——NOSQL-10・Issue #765
   が正式な `explain` op 写像へ置き換えるまでの暫定の未実装扱い。両方
   欠落のときはここで先回りせず、未知テーブルが `42P01` で拒否される
   優先順位契約〔手順 4〕を壊さないよう通常のディスパッチへ進める）
4. `vector`／`plan` の有無（スキーマ非依存の JSON 上の存在確認）で
   `execute_bound_search_in_session`／`execute_bound_plan_search_in_session`
   のどちらを呼ぶかを決め、選んだエントリが `table` のスキーマを取得
   （未知テーブルは `42P01`。この判定は binder の排他判定〔`vector`／
   `plan` 両方欠落〕・`mode` リテラルの解析より**先に**確定する——
   テーブル解決が binder 呼び出し・`mode` 解析の前提条件のため。手順 2 の
   同時指定判定とは異なり、こちらはスキーマ依存の判定〔両方欠落・`mode`
   値不正〕に対する優先順位）
5. binder（`wire-server::http::query::search::bind_search`）が schema 依存の
   束縛・`mode` リテラルの解析・（両方欠落の場合の）排他判定を行う

### 却下した設計案（追加分）

| 案 | 内容 | 却下理由 |
| --- | --- | --- |
| D | wire 側で `ValidatedStatement::new(..).with_using_plan(..)` を組み立てて SQL アーム（`Statement::Select`）へ流す | `WherePredicate::Prefix { pattern }` は LIKE パターン（`declarative_filter::parse_prefix_pattern` が末尾 `%` 必須）を要求するが、NoSQL `filter.prefix` は生プレフィックス（`DeclarativeFilter::prefix`）であり意味が食い違う。SQL 文字列を経由しない設計方針にも反する |

### テスト（search エントリ追加分）

`crates/engine/tests/core_bound_plan_entry.rs`（Issue #764 で追加）:

- `vector` 指定: RLS 暗黙適用・未定義テーブルの binder 呼び出し前拒否・
  binder エラーの伝播・対象テーブル不一致の拒否・SQL 経由（`USING MODE`
  含む）と `Cell` レベル一致（`precision` の確信度ゲートを含む）
- `plan` 指定: SQL の `USING PLAN` 経由と行一致（決定的スタブ）・binder が
  I/O 前に拒否する場合は LLM 呼び出し 0 回（`bind` が 2 回とも純粋に
  呼ばれることの非 vacuous な証跡）・`query_planner`／`embedder` 未注入の
  fail-closed・PLAN-11 のプランナー推定モードとクエリ句明示指定の優先順位

既存回帰（無変更で green を確認）: `sql_using_plan.rs`・`sql_precision_mode.rs`・
`sql_search_mode.rs`・`bound_plan_public_api.rs`。wire-server 側は
`crates/wire-server/tests/nosql2_search.rs`（新設）が `wire_search_mode.rs`・
`wire_using_plan.rs` と同型の決定的フィクスチャで SQL 経由とのバイト単位
パリティ・RLS 非漏えい・precision fail-closed・`explain` の 2 分岐拒否を
固定する。

## テスト

`crates/engine/tests/core_bound_plan_entry.rs`（単一 `Storage` 構成。生 redb
再オープンを使わない）:

- RLS 暗黙適用（他テナントの `Private` 行が非漏えい）
- 未定義テーブルは binder 呼び出し前に拒否（binder が呼ばれないことを固定）
- binder のエラーがそのまま伝播する
- 束縛済み計画の対象テーブルが要求テーブルと不一致なら拒否
- 集計エントリが SQL テキスト経由（`execute_sql_in_session`）と `Cell` レベル
  で一致し、`VisibleBitmapCache` を共有する（2 回目の呼び出しでキャッシュヒット
  することを確認）
- `GROUP BY` 付き集計で他テナント専有のグループ値が非漏えい
- binder closure がセッションに登録済みの UDF レジストリを観測できる

既存回帰（無変更で green を確認）: `sql_scan.rs`・`sql_scan_public_api.rs`・
`sql_aggregate.rs`・`sql_aggregate_public_api.rs`・`sql_group_by.rs`・
`scalar_index_aggregate.rs`・`sql_surface.rs`・wire-server `wire_scan.rs`
（7 件）・`wire_aggregate.rs`（7 件）。

`crates/engine/tests/bound_plan_public_api.rs`（Issue #729。単一 `Storage` 上で
SQL 経由の実行と束縛済み計画経由の実行が混在しても結果が一致し、テナント境界
（RLS-7・RLS-8）を破らないことを固定）:

- scan: `SELECT` の複数形（`WHERE`・`SELECT *` を含む）で `id` ソート後の行が
  両経路で完全一致（`LIMIT` が可視行数を下回る場合は SQL-15 の順序保証なし
  契約に合わせ件数・可視範囲のみ比較）
- 集計（`GROUP BY` なし）: `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`・UDF（`vec_norm`）
  経由の式項目が固定オラクルと両経路で一致
- `GROUP BY`／`HAVING`／`ORDER BY ... LIMIT`: 順序込みで両経路が固定オラクルと
  一致
- 書き込み（`insert_row`）を挟んだ世代進行の前後で両経路が同時に新しい世代を
  反映し、`VisibleBitmapCache` が実際にヒットした状態（非 vacuous）であることを
  確認
- 一方のテナントを束縛経路、他方を SQL 経路で交互実行（往復）しても
  `Visibility::Private` 行が他テナントへ非漏えい（`Visibility::Public` は
  テナント非依存の全体公開契約どおり両テナントから見える）
- `Arc<EngineCore>` を複数スレッドで共有し SQL 経路・束縛済み経路を交互実行
  しても単一スレッド参照結果と一致
- 同一の拒否入力（`SUM(embedding)`・`HAVING` での `GROUP BY` キー列比較）に
  対する `wire_code` が両経路で一致

再実測した wire 回帰の pass 件数（本 Issue 時点で無変更）: `wire_scan.rs`
7 件・`wire_aggregate.rs` 7 件、いずれも pass。

## 追記（Issue #948・NOSQL-16・SQL-27・TASK-186）: EXPLAIN 用エントリ 3 本

上記の実行用エントリ（`execute_bound_search_in_session`・
`execute_bound_scan_in_session`・`execute_bound_aggregate_in_session`）と
同型・同一処理順序（(1) スキーマ取得、(2) `bind` で束縛、(3) 対象テーブル
一致検証、(4) 実行）の EXPLAIN 版として `explain_bound_search_in_session`・
`explain_bound_scan_in_session`・`explain_bound_aggregate_in_session` を
追加した。相違点は手順 (4) が実行本体（`run_select_plan`／`run_scan_plan`／
`run_aggregate_plan`）を呼ばず、SQL `EXPLAIN` アームと共有する私的ヘルパー
（`search_explain_from_bound`・`scan_explain_result`・
`aggregate_explain_from_bound`）だけを呼ぶ点のみ。`ctx`
（`PolicyContext`）は execute 系との対称性のために残すが、EXPLAIN は行を
読まないため本体では使わない（各エントリの doc コメント参照）。

行の一致（SQL 経由・NoSQL 経由でビット同一）は、SQL `Statement::Explain`
アームと新エントリの両方がこの私的ヘルパーを共有する構造によって保証される
（第 2 の実装を作らない設計。テストでの後追い確認ではなく構造上の保証）。
契約テストは `crates/engine/tests/core_explain_bound_entries.rs`
（Refs #948）。
