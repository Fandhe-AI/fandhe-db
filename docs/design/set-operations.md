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
top       := set_expr [ORDER BY <set_key> {, <set_key>}] [LIMIT <n>] [OFFSET <m>] [;]
set_key   := (<出力列名> | <整数序数>) [ASC|DESC] [NULLS FIRST|LAST]   -- Issue #3266（構文・束縛）・#3267（実行）
set_expr  := set_term { (UNION [ALL | DISTINCT] | EXCEPT [ALL | DISTINCT]) set_term }  -- 左結合
set_term  := primary { INTERSECT [ALL | DISTINCT] primary }                   -- 左結合・UNION/EXCEPT より高優先
primary   := branch | '(' set_expr ')'                         -- 括弧の入れ子深さは 4 まで（実装既定値）
branch    := SELECT <select_list> FROM <table_or_view> [WHERE <既存述語>]
           | <集計形の枝>                                        -- Issue #1191
           | SELECT <式>[ AS <別名>] [, ...]                       -- Issue #3270（FROM なしの定数枝）
           | '(' <単一 SELECT> ORDER BY ... LIMIT n [OFFSET m] ')'  -- Issue #1191（括弧付きのみ）
```

優先順位は PostgreSQL と同じ: `INTERSECT` は `UNION`／`EXCEPT` より強く結合
し、同じ優先度の演算子は左結合。`UNION`・`UNION ALL`・`INTERSECT`・
`INTERSECT ALL`・`EXCEPT`・`EXCEPT ALL` を受理する。明示の `UNION DISTINCT`／`INTERSECT DISTINCT`／
`EXCEPT DISTINCT` は既定の `UNION`／`INTERSECT`／`EXCEPT` と同じ意味（Issue #3268・#3269）。
`INTERSECT ALL` は多重集合の交差で、同じ行を min(左の出現数, 右の出現数) 件残す。`EXCEPT ALL` は
多重集合の差で、同じ行を max(左の出現数 − 右の出現数, 0) 件残す。量指定語の連続
（`ALL DISTINCT` 等）は `42601`。

Issue #1191 で次を受理する:

- 集計形の枝（先頭が集計関数名＋`(`、または `GROUP BY` を含む）。括弧なしの枝は
  `HAVING` まで。結果列の静的型（Issue #1173）で型整合を検証する（`COUNT` は BIGINT。
  静的型を持たない列は `42804`）。集計結果の行キーは `Integer`／`SignedInteger` を
  正準化して同値判定する。
- 括弧付き単一 `SELECT` 枝の枝内 `ORDER BY`（スカラー列）・`LIMIT`・`OFFSET`
  （`LimitedBranch`。明示 `LIMIT` により切り詰め、`54000` にしない。集計の括弧付き枝は
  `GROUP BY` 句の `ORDER BY`／`LIMIT`）。
- 落とし穴: 括弧なしの末尾 `ORDER BY`／`LIMIT` は PostgreSQL では**集合演算全体**に
  掛かる。最後の枝の句と解釈すると結果が異なるため、枝の句としては扱わない。末尾 `LIMIT n`
  は従来どおり全体 `LIMIT`。

Issue #3266（SQL-29・SQL-56 のポインタ）で、括弧なし末尾の `ORDER BY` を**集合全体のキー**
として構文解析し `ValidatedSetOperation::order_by` に保持する（最上位・最後の枝の直後だけ）。

- キーは出力列名（左端の枝の列名）か整数序数。解決は `sql::set_op::resolve_order_keys`
  （序数が範囲外は `42P10`、不一致は `42703`、複数一致は `42702`。名前の大文字小文字は広域取得の
  `ORDER BY` と同じく区別する。枝の表にあっても出力に無い列は `42703`）。
- 式キーは `0A000`（PG は結果列名しか許さない）。キー数の上限は広域取得と同じ 8（超過は `54000`）。
- 判定順は、構文（`42601`）→ 全体 `LIMIT`／`OFFSET` の範囲（`22000`）→ 型整合（`42804`）→
  キーの解決 → 関門。
- Issue #3267 で実行を実装した。合成結果（重複除去・上限判定の後）を `sql::cell_sort::sort_rows`
  （安定ソート。`sort_by` のみ）で並べ、その後に `OFFSET` → `LIMIT`／結果行数上限を適用する
  （`FETCH FIRST n ROWS ONLY`・`OFFSET m ROWS` は正規化で `LIMIT`／`OFFSET` になる）。同値の行は
  下記の順序規約（左の子が先・枝内は走査順）を保つ。ソート用の領域は確保前に文全体の累計バイト
  予算へ計上し（`checked_*`）、超過は `54000`。
- キー型と照合は `sql::set_op::plan_set_order_by` が行を読まずに検査し、`prepare`／
  `describe_columns` が共有する（実行・Describe・`EXPLAIN`〔`ANALYZE` 含む〕・`DECLARE CURSOR` の
  受理・拒否が一致する）。比較できない型（`TIMESTAMPTZ`・`TIME`・`INTERVAL`・`CHAR(n)`・`JSON`・
  配列・`VECTOR` 等）と、非バイト列順（ICU）照合の列を持つ表がある文の `TEXT` キーは `0A000`
  （バイト列順で黙って比べない fail-closed）。ビュー本文は参照時に同じ検査を通る。
- `EXPLAIN` は全体 `sort` 段（`sort_keys` はキー数のみ）を、全体 `limit` 段と `set_operation` 段の
  間に出す。
- 式キーの `0A000` は PG と同じ（結果列名・序数しか許さない）意図的な拒否。再帰 CTE のアンカー項の
  全体 `ORDER BY` は PG でも受理されない位置のため定義時に `0A000` のままとする（意図的な拒否）。
- 次は従来どおり `42601`: 最後以外の枝の直後の `ORDER BY`、括弧内の入れ子の集合式の `ORDER BY`、
  `LIMIT` の後の `ORDER BY`、キー無し・末尾カンマ、修飾名、距離演算子キー。

以下はいずれも枝の中では `42601`:

- 枝内の `<=>`（ベクトル順位付け）・`USING PLAN`・`USING MODE`・`HINT ORDER`・
  `DISTINCT`、括弧なしの枝内 `OFFSET`（括弧なしの最後の枝の直後の `ORDER BY` は集合全体のキー。上記）
- 枝内の窓関数・値の部分クエリ項目・距離列、式の中の集計呼び出し（`coalesce(sum(n), 0)` 等）、
  枝内の式 `ORDER BY` キー

Issue #3270（SQL-42 のポインタ）で、`FROM` あり枝の式項目（リテラル・算術・関数呼び出し・
`列 AS 別名`）を受理した。通常の SELECT と同じ `bind_scan`（`bind_projection`）で束縛し、型の
統一は下の既存規則に任せる（規則は新設しない）。あわせて `FROM` なしの定数枝
（`SELECT 7 UNION SELECT y FROM b`）を `SetTree::ConstBranch` として受理する。定数枝は
`sql::const_select` の束縛・実行をそのまま使い、表を読まないので RLS の対象行を持たない。
定数枝の集計項目・`WHERE`／`HAVING`、括弧付きの `LIMIT`／`OFFSET` は `42601`、ルーチン呼び出し・
スキーマ関数は束縛段が `0A000`。全枝が定数の集合演算（`SELECT 1 UNION SELECT 2`）とその
`EXPLAIN` は `0A000`。ビュー本文の定数枝・式枝は `42601`。型の決まらない `NULL`・文字列リテラルを他の枝の型へ解決する規則は
持たず、型が一致しなければ `42804`。

Issue #3271 で、式射影の枝・定数枝を含む集合演算の結果（`INTERSECT`／`EXCEPT`・`ALL` を含む）と
`EXPLAIN`（`ANALYZE`・`VERBOSE`・`FORMAT JSON` を含む）を対応済みにした。EXPLAIN は、式射影の枝を
通常の `scan` 葉（単一表の `EXPLAIN` と同じく射影の中身は描画しない）、定数枝を葉 `result`
（`source: constant`。`cte_explain` と共通の語彙）として描画する。`relation:` の序数は位置ベースで、
定数枝も葉の位置を 1 つ消費する（`SELECT 7 UNION SELECT k FROM a` の `scan` は `relation: 2`）。

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
に加え、整数型〔`SMALLINT`／`INTEGER`／`BIGINT`〕どうしを互換とし結果列は広い方の型へ
拡大する〔Issue #1560。名前は左端の枝〕）が一致しない場合は新設エラー分類 `SqlSurfaceError::DatatypeMismatch`
（`ErrorClass::DatatypeMismatch`。`42804`）で拒否する。式射影・集計・定数枝が返す `Computed`
列は、束縛時に確定した静的型を `Scalar` と同じ型として比較し、静的型を持たない列
（`Computed { ty: None }`）は不一致（`42804`）とする。結果列名・メタデータは常に左端の枝に
揃える。

`VECTOR` 列を投影に含む枝は、重複除去を伴う演算（`UNION`／`INTERSECT`／`INTERSECT ALL`／
`EXCEPT`／`EXCEPT ALL`）の対象なら `22000`（`UNION ALL` のみなら許可）。判定は演算子木を
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
- `INTERSECT`／`INTERSECT ALL`／`EXCEPT`／`EXCEPT ALL` は左の子の順序を保つ。`INTERSECT ALL` は同じ行を左で先に現れたものから min 件残す。`EXCEPT ALL` は同じ行の左で先に現れた min(左, 右) 件を相殺し、後ろの行を左の順序のまま残す。
- `INTERSECT ALL`／`EXCEPT ALL` の右辺の出現数表も文全体の累計バイト予算に計上する（超過は `54000`）。
- 全体の `LIMIT n` は、この順序の先頭から n 件。

## スコープ外（Issue #929・#3270・#3271 の対象外事項）

- `DISTINCT`・ベクトル順位付けを含む枝（集合演算の結果全体に対する `ORDER BY` は構文・束縛を
  Issue #3266、実行を Issue #3267 で対応済み。集計の枝・括弧付き枝の `ORDER BY`／`LIMIT`／`OFFSET` は Issue #1191 で対応済み）。
  全体 `ORDER BY` の未対応は、比較できない型・ICU 照合のキー、`FETCH ... WITH TIES`（いずれも
  `0A000`／`42601`。親 Issue #3265 の配下で追跡）
- 式射影の枝・定数枝（Issue #3270・#3271 で結果と `EXPLAIN` を対応済み）の残る未対応（受け皿の Issue は未定）:
  全枝が定数の集合演算とその `EXPLAIN`（`0A000`）、定数枝の集計・`WHERE`／`HAVING`・括弧付き定数枝の
  `LIMIT`／`OFFSET`（`42601`）、型の決まらない `NULL`・文字列リテラルの型解決（`42703`／`42804`）、
  ビュー本文の定数枝・式枝（`42601`）、枝内の窓関数・式 `ORDER BY` キー（`42601`）
- `COPY TO`、明示トランザクション内での実行（既存の既定経路で `42601`／`0A000` に自然に落ちる）。
  `EXPLAIN` は Issue #2047、`DECLARE CURSOR` は Issue #3176 で対応済み（cursor の詳細は
  [sql-cursor.md](sql-cursor.md)、`EXPLAIN` は
  [explain-search-engine-exposure.md](explain-search-engine-exposure.md)）
- NoSQL（HTTP）表層での集合演算（spec 上も非対応）
- サブクエリと CTE（#927・#928）、`JOIN` を含む複数テーブル計画基盤
  （#924〜#926）、RLS-10 の横断検証（#931）
