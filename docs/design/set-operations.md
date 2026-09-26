# ADR: 集合演算 `UNION`／`UNION ALL`／`INTERSECT`／`EXCEPT` を SQL 表層へ追加する（Issue #929）

- ステータス: Implemented（spec 側ビヘイビア ID は SQL-29 (c)・RLS-10 (b)・
  TASK-213 として付与済み。数値基準・実測値の公開はオーナー判断に基づく
  ［spec-confidentiality］の許可範囲内）
- 対応: Issue #929・spec 側 SQL-29 (c)・RLS-10 (b)・TASK-213
- 関連ポインタ: SQL-29 (c)・RLS-10 (b)・TASK-213・SQL-15（TASK-170。各枝の
  実行基盤）・RLS-8（TASK-138。全読み取り経路への RLS 一般化）・SQL-25 (c)
  （`UNION` の同値判定と同一の重複除去規約）・TASK-212（`JOIN`・複数テーブル
  実行計画基盤。本 ADR は前提タスクとせず切り離す設計判断。下記「設計判断」
  節参照）
- 検証コード: `crates/engine/src/sql/allowlist.rs`（構文検出・再帰下降パーサー・
  `ValidatedSetOperation`／`SetTree`／`SetOperator`）・
  `crates/engine/src/sql/set_op.rs`（束縛・型整合検証・実行・行キー・
  Describe 用の型検証のみ経路）・`crates/engine/src/core.rs`
  （`read_txn_with_schemas`・`Statement::SetOperation` の各実行経路への結線）・
  `crates/engine/tests/sql29_set_operations.rs`（意味論・優先順位・型整合・
  `VECTOR`／重複除去の組・構文拒否・上限・RLS・決定性）・
  `crates/wire-server/src/http/status.rs`・
  `crates/wire-server/tests/err4_http_projection.rs`・
  `crates/wire-server/docs/nosql-api.md`（`42804` の HTTP 射影・到達不能分類
  としての記載）

## 背景

SQL 表層は集合演算の構文を持たず、`UNION`／`UNION ALL`／`INTERSECT`／
`EXCEPT` はいずれも許可リスト外として `42601` で拒否されていた。複数の
`SELECT` 結果を集合演算で合成する経路を、RLS 暗黙適用（fail-closed）を
保ったまま提供することが目的。

## 設計判断: 各枝を単一テーブル広域取得（SQL-15）に限定する

spec 上、集合演算（SQL-29 (c)）の前提タスク TASK-213 は `JOIN`・複数テーブル
実行計画基盤（TASK-212）に後続する。TASK-212（および SQL-25 の `DISTINCT`・
`ORDER BY`・複数列 `GROUP BY`）は本 Issue 着手時点でまだ未完了・open PR
だった。そこで本実装は、各枝を「単一テーブルの広域取得」（SQL-15 の
`ValidatedScan` → `bind_scan` → `execute_scan`）に限定することで、複数
テーブル計画の基盤を待たずに導入する設計判断を取った。

この判断の帰結:

- 集合演算の対象は「`SELECT <list> FROM <table> [WHERE ...]` 形の枝のみ」で
  あり、`JOIN`・サブクエリを含む枝は構文上受理しない（`42601`）。
- 各枝は既存の広域取得の実行経路（`execute_scan`）をそのまま通るため、RLS
  暗黙適用・fail-closed のエラー契約・`wire_code` 契約を第 2 の実行器なしに
  継承する。
- TASK-212 完了後、`JOIN` を枝に含める拡張は別途検討する（本 ADR のスコープ
  外。Issue の「対象外」節参照）。

## 受理文法

```text
top       := set_expr [LIMIT <n>] [;]
set_expr  := set_term { (UNION [ALL] | EXCEPT) set_term }      -- 左結合
set_term  := primary { INTERSECT primary }                     -- 左結合・UNION/EXCEPT より高優先
primary   := branch | '(' set_expr ')'                         -- 括弧の入れ子深さは 4 まで（実装既定値）
branch    := SELECT <select_list> FROM <table_or_view> [WHERE <既存述語>]
```

優先順位は PostgreSQL と同じ: `INTERSECT` は `UNION`／`EXCEPT` より強く結合
し、同じ優先度の演算子は左結合。演算子は `UNION`・`UNION ALL`・`INTERSECT`・
`EXCEPT` の 4 種のみを受理し、`INTERSECT ALL`／`EXCEPT ALL`／`UNION DISTINCT`／
`EXCEPT DISTINCT` 等は `42601`。

以下はいずれも枝の中では `42601`:

- 集計形の枝（先頭が集計関数名＋`(`、または `GROUP BY` を含む）
- 枝内の `ORDER BY`・`<=>`（ベクトル順位付け）・`USING PLAN`・`USING MODE`・
  `HINT ORDER`・`DISTINCT`・`OFFSET`
- 式項目（`Computed` 投影。宣言的 UDF・組み込み関数呼び出し）: 束縛時に型を
  確定できないため fail-closed で拒否する

全体に対する `LIMIT <n>` は任意（`1..=core::MAX_SEARCH_K`。範囲外は
`22000`）。範囲検証は `sql::set_op::execute` が全枝の走査より前に行い、
`sql::set_op::describe_columns`（Describe 経路）も同じ検証を行う（PR #1105
レビュー指摘対応。従来は Execute のみ全枝の走査・合成の後に検証しており、
Describe は検証していなかったため範囲外の値が Describe だけ受理されていた）。
括弧の入れ子上限は 4（実装既定値）、超過は `54000`。枝数の上限は 16
（実装既定値）で、超過は `Vec` に積む前に判定し `54000`。

`UNION`／`INTERSECT`/`EXCEPT` は [`crate::sql::lexer::Keyword`] 化しない
（既存の列名・テーブル名としての用法を壊さないため）。検出は「演算子ident
の直後（`ALL`／`DISTINCT` を 1 個挟んでもよい）に `SELECT`、または括弧を
読み飛ばした先が `SELECT` になる `(` の連なりが続く」という並びのみを対象
にし、`validate_sql_tokens` の集計形状判定（`is_aggregate_select`・
`contains_group_by`）より前に行う（これらはトークン列全体を走査するため、
`SELECT ... UNION SELECT ...` の 2 つ目以降に集計形が現れる場合の誤判定を
避ける）。「次が `(`」だけを条件にすると `union(score)`（宣言的 UDF・組み込み
関数の呼び出し）の呼び出し括弧まで枝開始と誤検出するため、`(` の先読みは
必ず `SELECT` への到達まで確認する（Issue #929 最終レビュー指摘の回帰修正）。

## 束縛時の型整合（受入基準 1・ERR-6 `42804`）

各枝を `bind_scan`（Describe では `bind_scan_with_dummy_flags`）で束縛し、
投影の `ColumnMeta` 列を得る。列数が不一致、または列ごとの型
（`ColumnMeta::Id` と `Id` は一致、`Scalar { ty }` は `ColumnType` の完全一致
のみ許容）が一致しない場合は新設エラー分類 `SqlSurfaceError::DatatypeMismatch`
（`ErrorClass::DatatypeMismatch`。`42804`）で拒否する。`Computed` 列は構文
検証段で既に排除済みのため到達しない。結果列名・メタデータは常に左端の枝に
揃える。

`VECTOR` 列を投影に含む枝は、重複除去を伴う演算（`UNION`／`INTERSECT`／
`EXCEPT`）の対象なら `22000`（`UNION ALL` のみなら許可）。判定は演算子木を
辿り、重複除去を伴うノードの配下に `VECTOR` 列があるかで行う。

## 実行・上限（受入基準 2・4）

- 単一スナップショット: `EngineCore::read_txn_with_schemas`（`Self::
  read_txn_with_schema` の複数テーブル版）が `storage.db().begin_read()` を
  1 回だけ開き、枝が参照する全テーブルのスキーマをまとめて解決する。
- 各枝の評価は `execute_scan_with_budget` を呼ぶ。可視行数が
  `core::MAX_SEARCH_K` を超える場合は `54000`（超過検出のため
  `MAX_SEARCH_K + 1` を上限として走査する）。バイト予算は文全体（全枝の走査・
  合成・行キー生成）で 1 つの累計予算（`sql::set_op::SetOpBudget`。単一クエリ
  の結果予算と同じ上限）を共有し、枝ごとに独立の予算を持たせない（PR #1105
  レビュー指摘対応。従来は各枝が独立に同予算を使い、最大 16 枝分の結果を
  同時に保持し得たため、単一クエリの結果予算をはるかに超える合計メモリを
  SQL 入力だけで確保できていた）。
- RLS: 全枝に、呼び出しセッション自身の `PolicyContext` を独立して渡す
  （scan 経路の既存 RLS には一切手を入れない）。
- 重複除去の基数上限・合成結果の行数上限: いずれも `core::MAX_SEARCH_K`
  （実装既定値）。上限判定は全体 `LIMIT` による切り詰めの前に行う（`LIMIT`
  の有無で成否が変わらない単純な規則）。
- 同値判定（行キー）: セルごとに「型タグ＋長さ接頭辞（`u32` BE）＋
  ペイロード」を連結した正準バイト列（SQL-25 (c) の同値判定と同一の正規化。
  `NULL` は専用タグ、`Float` は `-0.0`/`+0.0`・NaN の正規化）。

## 順序規約（受入基準 3）

結果の順序は次の規則で同一スナップショット内で決定的:

- 演算子木の左の子の結果を先に、右の子の結果を後に並べる。
- 各枝の中は SQL-15 の走査順（`(tenant_id, id)` 昇順の物理走査順）に従う。
- 重複除去では最初に出現した行だけを残す。
- `INTERSECT`／`EXCEPT` は左の子の順序を保つ。
- 全体の `LIMIT n` は、この順序の先頭から n 件。

## スコープ外（Issue #929 の対象外事項）

- 集計・`DISTINCT`・`ORDER BY`・`OFFSET`・ベクトル順位付けを含む枝、集合演算
  の結果に対する `ORDER BY`
- `INTERSECT ALL`／`EXCEPT ALL`、括弧内の `LIMIT`
- `DECLARE CURSOR`・`COPY TO`・`EXPLAIN` での集合演算、明示トランザクション
  内での実行（いずれも既存の既定経路で `42601`／`0A000` に自然に落ちる）
- NoSQL（HTTP）表層での集合演算（spec 上も非対応）
- サブクエリと CTE（#927・#928）、`JOIN` を含む複数テーブル計画基盤
  （#924〜#926）、RLS-10 の横断検証（#931）
