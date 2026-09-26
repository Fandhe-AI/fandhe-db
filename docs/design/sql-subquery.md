# サブクエリ（スカラー・IN・EXISTS）

- ステータス: **Accepted（実装既定値。当初計画からスコープを大きく縮小して実装）**
- 対応: Issue #927
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
| スカラー比較サブクエリ（`<col> <cmp> (SELECT ...)`）・投影位置のスカラーサブクエリ | **非対応**（構文自体を提供しない） |
| 内側が集計（`GROUP BY`）・ランキング付き検索 SELECT | **非対応**（`42601`） |
| 内側の `LIMIT` 省略 | **非対応**（`42601`。内側は常に明示 `LIMIT` が必要） |
| 内側の `OFFSET` | 未検証（構文上は Scan 形状を再利用するため通るが、意味論は未確認。将来の Issue 課題） |
| 相関サブクエリ | **非対応**。専用の構造検出は持たず、内側の束縛（`bind_scan`）が内側テーブルのスキーマにない列参照を既存の `22000`（unknown column）へ落とすことに委ねる（内側は常に自分の FROM テーブルのスキーマのみで束縛される） |
| `NOT IN`・`NOT EXISTS` | **非対応**（文法自体が `NOT` を持たない） |
| `IN` 対象列が疑似列 `id` | 実測範囲外（このリポの既存 `WherePredicate::Equality` 束縛自体が疑似列 `id` を対象にしていないため。`sql::subquery::cell_to_equality_predicate` の `Cell::Integer` 分岐は将来の拡張に備えて到達可能コードとして残す） |
| `IN` 対象値の型 | `TEXT`／`BOOLEAN` のみ。`INTEGER`／`BIGINT`（`WherePredicate::Equality` が `TEXT`／`ENUM` 列専用のため対象外。等価比較自体がこのリポ未実装〔レーン A〕）・`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA`／`VECTOR`／配列／JSON は `22000`（push 前 Review 指摘対応。Issue #927） |
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

- `IN`: 内側の各行のセルを `<column> = <値>` 相当の葉（`Equality`／
  `BoolEquality`）へ変換し、`WherePredicate::Or(branches)` として束ねる
  （0 行なら `Or(vec![])` ＝ `where_tree::BoundOrGroup::matches` が必ず
  `false` を返す。TASK-208 で確立済みの意味論をそのまま利用）。
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
- `IN (SELECT ...)` の展開葉数: `sql::subquery::MAX_SUBQUERY_IN_LEAVES`
  （`declarative_filter::MAX_METADATA_FILTERS` と同じ 256。文全体で共有する
  `&mut usize` 予算で検査し、超過は `54000`。内側可視行数×実行回数上限の
  組合せだけでは通常の `WHERE` 述語数上限より大きい評価コストを 1 文から
  発生させられたため、PR #1103 codex-review 指摘対応で追加した）。この予算は
  内側の**行**単位ではなく正規化した**distinct 値**単位で消費する
  （`IN` は集合所属の判定であり、同じ値の重複行は展開後 1 個の述語で足りる。
  Cursor Bugbot 指摘対応: 以前は行ごとに無条件で葉を積み予算を消費していた
  ため、同一ラベルを大量に持つ参照テーブルを内側に指定するだけで、
  distinct 値が 1 個でも `LIMIT` 256 超で誤って `54000` になり得た。NULL・
  語彙外 ENUM ラベルの除外は重複排除より前に行い、いずれも予算を消費しない
  既存の扱いを維持する）。
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

## 実装ファイル

| パス | 内容 |
| ---- | ---- |
| `crates/engine/src/sql/allowlist.rs` | `WherePredicate::InSubquery`/`Exists`（BREAKING CHANGE）・`Parser::subquery_ctx`・`MAX_SUBQUERY_DEPTH`・`parse_select_shape`/`parse_aggregate_shape` の ctx 引数・`validate_sql_tokens_with_subquery_ctx` |
| `crates/engine/src/sql/subquery.rs`（新設） | 解決本体（`resolve_where_predicates`・`MAX_SUBQUERY_EXECUTIONS`） |
| `crates/engine/src/sql/parser.rs` | `bind_where_predicates_recursive` の防御的拒否腕 |
| `crates/engine/src/core.rs` | `Statement::Scan`/`Statement::Aggregate` アームでの解決呼び出し・`parse_sql`/`execute_sql` のサブクエリ許可化・`parse_sql_prepared` の拒否ガード |
| `crates/engine/src/sql/view.rs`・`check_constraint.rs`・`recovery/content_hash.rs` | 網羅 `match` の防御的拒否腕（いずれも到達不能。構文段のゲートで先に拒否される） |
| `crates/engine/tests/sql29_subquery.rs`（新設） | 結合テスト（受理・RLS 境界・深さ上限・文脈拒否） |

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

- スカラー比較サブクエリ・投影位置のスカラーサブクエリ
- 内側の集計・ランキング付き検索 SELECT
- 内側のウィンドウ関数（SQL-30・TASK-214、Issue #930）: `IN`／`EXISTS`
  いずれも内側に `window_items` が含まれる場合は `42601` で一律拒否する
  （PR #1103 Cursor Bugbot 指摘対応。理由は「上限（DoS 対策）」節参照。
  サブクエリとウィンドウ関数の組合せが正しく動く経路は設計上未検証のため、
  作り込むのではなく fail-closed に倒した）。
- 相関サブクエリ
- 拡張クエリプロトコル経由のサブクエリ・Describe 対応
- `NOT IN`・`NOT EXISTS`（#913 の `NOT` 対応に依存）
- ランキング付き検索 SELECT（外側）でのサブクエリ対応
- サブクエリ述語に対するスカラー二次索引の最適化（現状は `PlainScan` 相当のまま）
- 性能の実測（受け入れ基準未設定）
