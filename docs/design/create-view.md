# `CREATE VIEW` / `DROP VIEW`（非マテリアライズド）の設計判断

Issue #909・対象ビヘイビア: TABLE-18・SQL-23（TASK-205）。関連ポインタ:
RLS-10 (b)（複数のテーブル参照を持つ読み取り経路での暗黙適用）・ERR-6
（`42P07`／`2BP01`／`42809` の新設）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

`MATERIALIZED VIEW`（TABLE-42）は本ファイル末尾の「Issue #1668 追記」にまとめる。以降の節は
非マテリアライズドビューの記録である。更新可能ビュー（TABLE-43。関連ポインタ SQL-49・
RLS-10 (b)・ERR-6 の `55000`）は末尾の「Issue #1675 追記」にまとめる。

## 概要

保存したクエリをビューとして登録し、`FROM` に書いて読めるようにする。
**非マテリアライズド**とし、参照のたびに定義を展開する。ビュー定義は既存の
許可リストパーサーをそのまま通したもの（許可リストの部分集合）に限って保存し、
参照時は保存済みの定義を**同じパーサーで再検証**する。第 2 の SQL パーサー・
実行器は作らない。

## 受理する構文（Phase 1）

```
CREATE VIEW <name> [(<列別名> [, ...])] AS SELECT <* | 列名リスト> FROM <table | view> [WHERE <単純述語> [AND ...]]
DROP VIEW <name>
```

列別名リスト（Issue #1659）は末尾の「Issue #1659 追記」節を参照。

- body の `LIMIT`・`ORDER BY ... <=>`・`HYBRID`・`USING PLAN`・集計形・式項目
  （`Projection::Items`）・UDF 呼び出し述語（`WherePredicate::PredicateCall`／
  `Expression`）・`EXPLAIN`・`CREATE OR REPLACE` の `VIEW` 以外への適用・`IF NOT EXISTS`・`TEMP`／
  `MATERIALIZED`（`CREATE VIEW` の中では受理しない。`CREATE MATERIALIZED VIEW` は別の文として
  Issue #1664 で受理済み。「Issue #1668 追記」参照）・`WITH CHECK OPTION`（#1673 で対応予定）・末尾の余剰トークンは
  いずれも構造的に受理しない（`42601`）。使える単純述語は
  `Equality`／`Prefix`／`BoolEquality`／`BoolColumn`／`Compare`（列 対
  リテラル）のみ。
- `DROP VIEW` は単一ビュー名（`IF EXISTS` 付きも可。Issue #1614、[drop-table.md](drop-table.md) の「Issue #1614 追記」）の指定のみを受理し、
  `CASCADE`／`RESTRICT` は Issue #1641 で受理に変わった（[drop-table.md](drop-table.md) の「Issue #1641 追記」）。複数名指定は `42601`。
- 拡張クエリプロトコルの `$n` を含む `CREATE VIEW`／`DROP VIEW` は Parse 時点で
  拒否する（DDL はバインドパラメータを受け付けない PostgreSQL の慣習に揃える）。

## ビューを参照するクエリ（Phase 1 のスコープ）

広域取得（SQL-15。`Statement::Scan`）のみをビュー展開の対象とする。
**集計（`Statement::Aggregate`）・ベクトル検索（`Statement::Select`）・
`EXPLAIN` はビュー展開の対象外**とし、意図的にスコープを縮小した
（実装時間の制約による判断。集計はいずれ同じ `resolve_from` を再利用できる
設計だが、追加の列スコープ検査コードが必要なため Phase 1 では見送った）。
ベクトル検索・`EXPLAIN` の対象はそもそも spec 上も対象外（ビューは
ランキング段を持てない）。これらの経路は `lookup.table_exists(view_name)` が
そのまま `false` を返すため `42601` ではなく `42P01`（未定義テーブル）に
fail-closed で落ちる——ビューの存在の有無に関わらず同一の応答になるため
情報漏えいにはならないが、PostgreSQL 的な「操作対象が違う」区別
（`42809` 相当）ではない点は既知の簡略化として記録する。

## Phase 2（Issue #1192）: 本文の受理形の拡大と集計からの参照

対象ビヘイビア: TABLE-18（主対象）・SQL-13／SQL-14（集計・`GROUP BY`）・
SQL-15（広域取得）・SQL-25（`ORDER BY`／`OFFSET`／`DISTINCT`）・SQL-28（JOIN）・
RLS-10 (b)・ERR-6。spec 本文は転記しない。

### 本文の 2 系統

| 系統 | 本文 | 参照時の処理 |
| ---- | ---- | ------------ |
| 単純形（Phase 1。変更なし） | 単一 relation への射影＋単純述語（`LIMIT` なし） | 既存のインライン展開（`Resolved::View`） |
| 評価後射影形（Buffered。新設） | 広域取得に `LIMIT`／`OFFSET`／スカラー `ORDER BY` が付いたもの、集計（`GROUP BY`／`HAVING`／`ORDER BY`／`LIMIT`／`SELECT DISTINCT`）、2 テーブル JOIN（`LIMIT` 必須） | 本文を 1 文として、**参照したセッション自身**の `PolicyContext`・同一スナップショットで既存の実行経路に通し、結果へ外側の列射影と `LIMIT`／`OFFSET` の切り出しだけを行う |

- 判定は `sql::allowlist::classify_view_body`: まず単純形（`parse_view_body`）を
  試し、失敗した場合のみ評価後射影形として、通常の SELECT と同じ
  `validate_sql_tokens` を通して文の種別を確定させ、`check_buffered_body_shape`
  で許可形状（`Scan`／`Aggregate`／`Join`）に絞る。第 2 のパーサー・実行器は
  作らない。`CREATE VIEW` 時はカタログを照会しない構文専用 lookup
  （`StructuralOnlyLookup`）を渡し、判定順序（構文 `42601` → DDL 権限 `42501` →
  カタログ判定）を保つ。参照時は実カタログのラッパー（再帰ガード付き）を渡す。
- 永続化する本文は、検証済みトークン列の正規化描画（`sql::lexer::render_tokens`）。
  旧形式（Phase 1）で保存された単純形本文は無変更で再パースでき、カタログの
  フォーマット変更・移行は不要。`ParsedViewBody`／`parse_view_body`／
  `render_view_body` のシグネチャは変更していない（CTE と共有）。
- 本文に書けないもの（`42601`）: ベクトル順位付け・`HYBRID`・`USING PLAN`・
  `EXPLAIN`・ウィンドウ項目・式項目・式述語・UDF 述語・集計引数の式（本文が
  参照セッションの UDF レジストリに依存しないようにする。連鎖本文の組み込み関数だけの
  式述語は Issue #1436 で例外的に受理する。下記「連鎖」節）。CTE・集合演算・
  サブクエリは Phase 3（Issue #1360）で受理側へ移った。

### 評価後射影形ビューを参照するクエリ

外側クエリは `SELECT <* | 列名リスト> FROM <view> [WHERE ...] [ORDER BY <列>, ...]
LIMIT n [OFFSET m]`（`Statement::BufferedView`。破壊的変更: 公開 enum への variant
追加）。外側の `WHERE`／`ORDER BY` は Phase 3（Issue #1360）で、集計・`DISTINCT`・
ウィンドウ関数・式 `ORDER BY` は Phase 4（Issue #1411）で受理側へ移った（下記）。
式項目・投影位置のスカラーサブクエリは `42601`。列指定が本文の結果列に
無ければ `22000`、本文に同名の結果列が複数あって一意に決まらなければ `42702`
（PostgreSQL は作成時に拒否するが、本実装は参照時に拒否する差異）。`EXPLAIN`・
cursor の `DECLARE`・サブクエリの内側・CTE・集合演算の枝・JOIN の辺・
`validate_statement` からの参照は `42601`。明示トランザクション内では他の読み取り文と
同じく、未 commit の変更を読む経路で本文を評価する。拡張クエリの Describe は
本文の列メタデータを導出して外側の後処理を束縛する（本文は実行しない）。

### 外側の `WHERE`・`ORDER BY`（評価済みセルに対する後処理）

本文を参照者の `ctx` で実行して得た結果行（評価済みセル）に対し、
`sql::view_buffered::plan_outer`（Execute・Describe 共通）が束縛し、`apply_outer` が
`WHERE` → `ORDER BY` → `OFFSET`／`LIMIT` → 射影の順に適用する（PostgreSQL と同じ。
**本文の `LIMIT` の後で絞り込む**）。第 2 の評価器は作らず、既存部品を再利用する。

- `WHERE`: 本文の結果列から合成した `TableSchema` に既存の `bind_scan` で束縛する。
  受理するのは宣言的な葉（`=`・`LIKE`・BOOLEAN・比較・`IN`・`BETWEEN`・`IS [NOT] NULL`）・
  式述語（整数・浮動小数の比較は式として解析される）・`NOT`／`OR`。UDF 述語・サブクエリ
  は `42601`。評価は `sql::join::values` の `cell_scalar`（評価済みセル→`ScalarRef`）と
  既存の宣言的フィルタ・式プログラムを使う。集計本文の `COUNT` 等が持つ
  `Cell::Integer(u64)` は、列の宣言型に合わせて符号付き整数・十進数・浮動小数へ正規化する
  （範囲外は `22003`。黙って NULL にしない）。
- `ORDER BY`: 本文の結果列名で解決する列キーのみ（式キーは `42601`）。比較は
  `sql::join::values` の比較部品（NULL は昇順で末尾・降順で先頭）。`sort_by`（安定）で
  並べ、同値の行は本文の順序を保つ（`sort_unstable*` は使わない）。
- 列の解決: 参照できるのは本文の**結果列**だけ。未知の列は `22000`、同名の結果列が複数あれば
  `42702`、並べ替え・比較できない型（`VECTOR`・`ARRAY` 等）は `22000`。疑似列 `id` は本文が
  `id` を結果列として公開しているときだけ参照でき（式述語の `id` はその結果列の値）、
  公開していない本文では `22000`（ビューが公開しない物理キーでの絞り込みを許さない）。
- 資源: 後処理の対象は本文の結果行（既存の予算で上限済み）で、並べ替えキーは
  「行数 × キー数」の比較値だけ。

### 連鎖・依存関係

- ~~評価後射影形は連鎖の最外段の 1 段に限る~~ は Phase 4（Issue #1411）で緩和した
  （下記「連鎖」節）。
- 本文が読む**すべての** relation の存在を作成時に確認する（`42P01`。JOIN の右辺を
  含む）。JOIN 本文の両辺はテーブルに限る（`42601`）。
- `DROP TABLE`／`DROP VIEW` の依存検査は `base_relation` だけでなく本文が読む全
  relation で判定する（JOIN 右辺の `DROP TABLE` は `2BP01`）。本文を再検証できない
  ビューは fail-closed で「依存あり」とする。`ALTER TABLE DROP COLUMN` は、評価後射影形
  ビューが対象テーブルを読んでいれば保守的に拒否し、無関係なテーブルのみを読む
  ビューは妨げない。

### 集計・`SELECT DISTINCT` からの単純形ビュー参照

`validate_select_statement` の集計・DISTINCT 分岐は `lookup.table_exists` の代わりに
`resolve_from` を使い、単純形ビューは基底テーブルへ書き換えてビュー由来の述語を
先頭にマージする（`build_aggregate_from_resolved`。非破壊）。列スコープは
`sql::view::check_aggregate_columns_within_view` で検査する（グループキー・集計
引数の式木・`GROUP BY` 列・`ORDER BY` 対象・`WHERE` の `OR`／`NOT` を再帰。ビューが
公開しない列を集計に使う filter oracle を塞ぐ）。評価後射影形ビューへの集計は
`42601`。`EXPLAIN` の集計も同じ経路を通るため、単純形ビューへの
`EXPLAIN SELECT COUNT(*) ...` を受け付けるようになった（広域取得の `EXPLAIN` が
既にビュー展開を受け付けているのと整合）。

### RLS-10 (b) の維持

`ViewDef` は Phase 2 でも `tenant_id`／`PolicyContext`／作成者の情報を持たない。
評価後射影形の本文は参照セッションの `ctx` で既存の実行経路が評価するため
（JOIN は両辺に独立して適用）、作成者の可視性は構造的に引き継がれない。外側の
後処理（`sql::view_buffered`）は `PolicyContext` を受け取らず、可視性判定に関与
しない。3 テナント対照の結合テスト（`tests/table18_view.rs`）で固定した。

## Phase 3（Issue #1360）: 本文の受理形のさらなる拡大

対象ビヘイビア: TABLE-18（主対象）・SQL-28・SQL-29・RLS-10 (b)・ERR-6。spec 本文は
転記しない。

### 受理する本文（評価後射影形）

先頭トークンは `SELECT`・`WITH`（非再帰 CTE）・`(`（括弧で始まる集合演算）。
`classify_view_body` は通常の読み取り SELECT と同じ
`validate_sql_tokens_with_subquery_ctx`（深さ 0）で構造を確定し、
`check_buffered_body_shape` で形状を絞る。`EXPLAIN`／`SET`／`CREATE` で始まる本文は
`42601`。

| 本文の形 | 検査 |
| -------- | ---- |
| CTE（`WITH`） | 畳み込み後の `Scan`／`Aggregate` に従来の検査。主クエリは従来どおり `subquery_ctx: None`（CTE の主クエリにサブクエリは書けない） |
| 集合演算 | `SetTree` の全枝（`Branch`／`LimitedBranch`／`AggregateBranch`）へ式項目・式述語・UDF 述語の拒否を適用。全体 `LIMIT` は任意（枝は各 `MAX_SEARCH_K` で頭打ち） |
| サブクエリ（`IN`／`EXISTS`／スカラー比較。`NOT`・`OR` の中も） | 内側のトークン列を作成時・参照時の双方で `validate_sql_tokens_with_subquery_ctx` により構造検証し、同じ形状検査を再帰的に適用。形の規則は `sql::subquery::execute_inner_query` と同じ（IN／EXISTS は `Scan`、スカラーは `Scan` か `Aggregate`）。実行は従来どおり参照者の `ctx` で `sql::subquery` が行う |
| 3 テーブル以上の JOIN | 既存の JOIN 経路でそのまま受理される（テストで固定。依存検査は全辺に効く） |

`LIMIT` の無い広域取得本文は、サブクエリ付きであっても従来どおり受理しない
（インライン展開すると他の経路へサブクエリ述語が漏れるため）。

### relation 一覧（SSOT）

`ViewBodyKind::Buffered { stmt, relations }` の `relations` を `classify_view_body` が
1 回だけ計算し、`validate_create_view_tokens`（`base_relation` ＝先頭）・
`catalog::view_body_shape`（存在確認・連鎖拒否・`DROP TABLE` の `2BP01`・
`DROP COLUMN` の保守的拒否の唯一の根拠）・`sql::view::reparse_buffered_body` が共有する。
先頭は主クエリの最初の FROM。CTE は各定義の FROM のうち「その位置から見える CTE 名
でないもの」（参照されない CTE の分も含む。`sql::cte::resolve_relation` の可視範囲と
同一）、サブクエリは内側の relation、集合演算は全枝、JOIN は全辺を重複なく含む。

### 変えないもの

本文の CTE・サブクエリ・集合演算の枝・JOIN の辺から評価後射影形ビューを参照することは
`42601`（Phase 4 でも不変。主 FROM に評価後射影形ビューを取る連鎖だけが受理側へ移った）。
JOIN 本文の辺はテーブルに限る。本文の大きさの上限（64 KiB・`54000`）、`$n` を含む DDL の Parse 拒否、
判定順序（構文 `42601` → `42501` → カタログ）は不変。

## Phase 4（Issue #1411）: 外側の集計・DISTINCT・ウィンドウ・式 ORDER BY と連鎖

対象ビヘイビア: TABLE-18（主対象）・RLS-10 (b)・ERR-1／ERR-2／ERR-4／ERR-6。spec 本文は
転記しない。

### 外側の形（`Statement::BufferedView` の中身の拡張）

`Statement` の variant は増やさず、`ValidatedBufferedView` を `view_name`・`body`・
`outer`（`BufferedOuter`）へ再構成した（フィールドは `pub(crate)`）。`outer` は 2 系統。

| 外側 | 受理する形 | 実行 |
| ---- | ---------- | ---- |
| 行形（`Rows`） | 列射影・宣言的／式 `WHERE`・`ORDER BY`（列キーまたは式キー）・ウィンドウ項目・`LIMIT`／`OFFSET` | `WHERE` → ウィンドウ計算（`WHERE` 通過後の行が母集合）→ `ORDER BY`（安定ソート）→ `OFFSET`／`LIMIT` → 射影 |
| 集計形（`Aggregate`） | `COUNT`／`SUM`／`AVG`／`MIN`／`MAX`（引数は裸の列参照か `*`）・`GROUP BY`・`HAVING`・`ORDER BY`・`SELECT DISTINCT`（脱糖後の集計形）・`LIMIT`／`OFFSET` | `WHERE` → `sql::group_by::GroupedRowAccumulator`（グループ表・予算・`HAVING`・`ORDER BY`・結果列の終端処理はストレージ走査経路と共有） |

第 2 の実行器は作らない。本文の結果列から合成した `TableSchema`（列位置は本文の結果列と
一致。評価できない列は到達不能なプレースホルダ名）に既存の `bind_scan`／`bind_aggregate`
で束縛し、評価済みセルは `cell_scalar` で `ScalarRef` へ写して既存の述語・式・
アキュムレータ・ウィンドウ評価へ流す。`group_by.rs` は終端処理を `finish_groups` へ切り出し
（挙動不変）、`window.rs` はキー抽出を `collect_window_row_values` へ切り出して
`CellWindowEvaluator` と共有する。

- 式 `ORDER BY`: 広域取得と同じ比較器（`compare_order_key`。NULL は昇順で末尾・降順で先頭）。
  キー値は行ごとに評価して所有値へ展開し、`sort_by`（安定）で並べる。式キーとウィンドウ項目の
  併用は構文段で `42601`（広域取得と同じ）。
- 集計の `GROUP BY` なしは 0 行でも 1 行（`COUNT` は 0、他は NULL）、`GROUP BY` ありで 0 行なら
  0 行。グループ数・キー・TEXT 集計状態・`COUNT(DISTINCT)` の予算は広域の集計と同じ
  （超過は `54000`）。ウィンドウの行数・パーティション数・状態予算も既存の定数を共有する。
- 列の公開範囲（P0）: 外側が参照する識別子（`WHERE`・集計引数・`GROUP BY`・`HAVING`／`ORDER BY` の
  式・ウィンドウの `PARTITION BY`／`ORDER BY`／引数・式キー）はすべて `sql::view` の列スコープ検査
  （`check_*_within_view`）を本文の結果列名に対して通す。ビューが公開していない物理キー `id`
  は集計・グループ化・並べ替え・分割のいずれにも使えず `22000`。同名の結果列が複数ある識別子は
  `42702`。`VECTOR`／`ARRAY`／型なし列を集計・並べ替え・分割に使うと `22000`。参照列だけを
  `normalize_cell`（`COUNT` の `u64` を宣言型へ。範囲外は `22003`）の対象にする。ウィンドウ別名を
  `WHERE`／`ORDER BY` で参照する形は広域取得と同じく `42601`。
- RLS-10 (b): 外側の後処理は `PolicyContext` を受け取らず、母集合は参照セッションの `ctx` で
  評価した本文の結果だけ。他テナントの行はグループ・件数・順位・パーティションに現れない
  （他テナントの `Private` 行を増減させても閲覧テナントの応答が変わらないことを結合テストで固定）。

### 連鎖

- 評価後射影形 → 評価後射影形: 本文の FROM が評価後射影形ビューなら、参照時の再検証で本文が
  `Statement::BufferedView` になる（`check_buffered_body_shape` に受理アームを追加。本文として
  持てるのは列射影・宣言的 `WHERE`・`LIMIT`／`OFFSET`、または裸の列引数の集計だけで、式キー・
  ウィンドウ・UDF 述語・サブクエリは `42601`。式述語は Issue #1436 で、組み込み関数だけを
  呼ぶものに限り受理側へ移った）。
- 連鎖本文の式述語（Issue #1436）: 組み込み関数はレジストリより先に解決され同名 UDF は登録できない
  （`42723`）ため、組み込み関数だけの式は参照セッションの UDF レジストリに依存せず意味が一定になる。
  非組み込みの呼び出し（セッション UDF・WASM UDF・未知の名前）は `42601`。判定は
  `udf_call::first_non_builtin_call`（`CHECK` 制約と共有）。構造検証（`StructuralOnlyLookup`）は
  FROM のビューをテーブル扱いにするため連鎖本文を判別できず、トップレベルの `Scan`／`Aggregate` の
  組み込み関数だけの式述語を**保留**して通し（`ViewBodyCheck::Structural`）、
  `catalog::Storage::create_view` が実カタログ（`ViewBodyCheck::Resolved`）で必ず再検証する。
  テーブル（または単純形ビュー経由でテーブル）直下の本文の式述語はこの再検証で `42601`
  （何も永続化されない。拒否の段が構造検証からカタログ段へ移るため、DDL 権限なしは `42501`、
  不在テーブルは `42P01` が先に返る）。サブクエリ内側・集合演算の枝は従来どおり厳格。
  依存検査（`2BP01`）・RLS（本文は参照セッションの `PolicyContext` で評価）は不変。
  単純形の段（`LIMIT` なしの列射影）は式述語を持てず、式述語つきの連鎖本文は `LIMIT` 付きの行形か
  集計形で書く。
- 単純形 → 評価後射影形: `resolve_from` が連鎖の各段（単純形）を、下位の本文の上に重ねた行形の
  `BufferedView`（`LIMIT` なし）として内側から畳み込む。各段の列スコープは下位の結果列に対する
  外側の検査（`22000`／`42702`）で担保する。式項目・UDF 述語を持つ段は作成時に `42601`。
- 深さ: `catalog::resolve_reference_depth_in_txn` を DAG の最大深さ（反復・メモ付き。再帰しない）へ
  一般化した。評価後射影形ビューは本文が読む全 relation の最大深さ + 1、単純形は `base_relation`。
  新規ビューの深さが `MAX_VIEW_NESTING_DEPTH`（4）を超える作成は `54000`。参照時の再検証は
  `TableLookup::buffered_depth`（`BufferedBodyLookup` が 1 段ずつ加算）で打ち切り、カタログ破損で
  循環していても無限再帰にならず `54000`。
- 作成時の検証: 本文がいずれかの relation 経由で評価後射影形ビューへ到達する場合、参照時と同じ
  実カタログ照会（`TxnViewLookup`。write txn 内）で `classify_view_body` を通す。CTE・サブクエリ・
  集合演算の枝・JOIN の辺から評価後射影形ビューを参照する本文は従来どおり `42601`
  （作成できても参照時に必ず失敗する定義を作らない）。
- 依存検査は不変: `DROP TABLE`／`DROP VIEW` は `base_relation`・本文の全 relation で判定し
  `2BP01`、`DROP COLUMN` は最下段の評価後射影形ビューが対象テーブルを読む限り保守的に拒否する
  （無関係なテーブルは妨げない）。

### 対象外（Phase 4 時点）

外側の式項目（`SELECT lower(lang) FROM v`）・投影位置のスカラーサブクエリ・外側の UDF 述語／
サブクエリ述語・集計の式引数、CTE・サブクエリ・集合演算の枝・JOIN の辺からの評価後射影形ビュー
参照、JOIN の辺をビューにすること、本文でのウィンドウ関数・式項目・UDF 述語、評価後射影形ビューへの
`EXPLAIN`・cursor `DECLARE`、NoSQL 表層でのビュー指定（`42P01` のまま）。

## 展開方式（検証段階での書き換え）

`sql::allowlist::validate_sql_tokens` の `Statement::Scan` 分岐にある
`lookup.table_exists(&shape.table_name)` を `sql::view::resolve_from` へ
置き換えた。

- `TableLookup` トレイトへ既定実装付きの `view_definition` メソッドを追加した
  （既定 `Ok(None)`）。既存の `TableLookup` 実装（テスト用モック等）は無変更の
  ままコンパイルできる。`catalog::Storage` のみが実データを返す。
- `resolve_from` は連鎖（ビューがビューを参照する）を実テーブルへ到達する
  まで辿ったのち、最も内側から外側へ向けて各段の投影・`WHERE` がその段の
  FROM が指す関係の公開列集合に収まっているかを検証しながら畳み込む。
  `WHERE` 述語は `内側のビュー ++ 外側のビュー ++ クエリ自身` の順で合成し、
  クエリへ最終的に公開される列集合は連鎖全体を積み上げた結果（`SELECT *`
  を挟んでも内側の列制限は失われない。外側ビューが内側ビューの非公開列を
  投影・`WHERE` のいずれで参照しても連鎖のどの段でも一様に拒否する）。
- 畳み込み後の文は通常のテーブル参照に対する `ValidatedScan` と完全に同じ形に
  なり、束縛（`sql::parser`）・実行（`sql::scan`）・RLS 適用（既存の暗黙適用
  経路）はすべて無変更のまま通る。

## RLS-10 (b) の不変条件

保存するビュー定義（`catalog::ViewDef`）は `tenant_id`／`PolicyContext`／
作成者の情報を一切含めない。展開後の文は、参照したセッションの `ctx` を使う
既存の実行経路でしか評価されないため、作成者の可視性が参照者へ引き継がれる
ことは構造的に起こらない（`table18_view.rs::
view_read_applies_referencing_session_rls_not_creator_visibility` で
3 テナント対照検証済み）。

## カタログ（redb）

- 新規テーブル `VIEWS_TABLE`（キー: ビュー名、値: `直接の参照先名 + 正規化
  body SQL` の 2 フィールド blob）。
- **body は検証済み AST を正規化して描画した SQL で保存する**
  （`sql::allowlist::render_view_body`）。「描画 → 再トークン化 →
  `parse_view_body`」が元と等価な AST を復元することを round-trip テスト
  （`sql/view.rs` 内）で固定した。
- 名前空間はテーブルと共有する: `Storage::create_view` は同一 write
  トランザクションで `CATALOG_TABLE`／`VIEWS_TABLE` の両方を確認し、衝突は
  `42P07`。`Storage::create_table` にも `VIEWS_TABLE` の確認を追加した。
- 上限（実装既定値）: ビュー総数 10,000（`MAX_LIST_TABLES` と同じ）、body
  64 KiB、ネスト深さ 4（テーブル自身を深さ 0 とする）。超過は `54000`。
- すべての検査（名前衝突・参照先の存在・ネスト深さ・依存オブジェクト）は
  それぞれの操作の write トランザクション内で行う（TOCTOU 回避）。

## ネスト深さと循環

- 循環は正常な経路からは構造的に作れない: `CREATE VIEW` は参照先の存在を
  作成前に要求するため自己参照は `42P01` になり、何も永続化されない。
  他から参照されているビュー・テーブルの `DROP` は `2BP01` になるため、
  「削除して作り直す」ことで循環を組めない。`ALTER VIEW` は `42601`。
  `CREATE OR REPLACE VIEW` は Issue #1660 で受理し、自己参照・循環は置換時に `42P17` で拒否する
  （末尾の「Issue #1660 追記」参照）。
- それでもカタログ破損に備え、`resolve_from`・深さ計算のいずれも visited
  集合と上限付き反復（ループ。再帰なし）を使う。循環を検出した場合は
  `XX000`（固定文言）、上限超過は `54000`。

## 判定順序（fail-closed）

`DROP TABLE`（Issue #902）と同じ判定順序に揃えた: 構文検証（カタログ非照会）
→ `require_ddl_permission`（`42501`。テーブル・ビューの存在有無を問わず
一律拒否）→ write トランザクション内でのカタログ判定（名前衝突・参照先存在・
深さ・件数上限）→ 保存。

- `CREATE VIEW`: 名前衝突 `42P07` → 参照先不存在 `42P01` → ネスト深さ・
  件数上限 `54000` → 保存。
- `DROP VIEW`: 名前がテーブルなら `42809`、存在しなければ `42P01` →
  依存するビューがあれば `2BP01` → 削除。
- `DROP TABLE <ビュー名>`: `42809`。参照しているビューがあれば `2BP01`。
- **ビューへの書き込み**（`INSERT`／UPSERT／`UPDATE`／`DELETE`／`TRUNCATE`）は、
  Issue #1670〜#1672 で次の 3 分岐に改訂した（TABLE-43。詳細は末尾の
  「Issue #1675 追記」）。
  - 自動更新可能なビューへの `INSERT`（UPSERT・`RETURNING` を含む）／`UPDATE`／`DELETE`
    は基表への同じ文に書き換えて実行する（`INSERT` は #1671、`UPDATE`／`DELETE` は #1672）。
  - 自動更新不可のビューへの `INSERT`／`UPDATE`／`DELETE` は `55000`（#1670。
    `SqlSurfaceError::ViewNotAutoUpdatable`、文種別ごとの固定文言）。
  - `TRUNCATE`・`EXPLAIN` 系・matview・索引名、および Parse 後の並行 DDL でビューが
    現れた競合は、従来どおり `42809`。`EngineCore::parse_tokens` の書き込み分岐が
    `UndefinedTable` を返し、かつその名前がビューとして存在する場合に限り
    `WrongObjectType` へ読み替える（`EngineCore::reclassify_write_to_view_error_for`）。
  - `55000` と `42809` の拒否経路では、副作用（台帳・世代・行）が一切発生しない
    ことをテストで固定した。

## 情報漏えいの抑止

保存済み body の再検証・展開が失敗した場合（カタログ破損・非互換の将来
フォーマット変更）は `XX000` の固定文言（`invalid view definition`）にまとめ、
body のリテラル値・破損理由の詳細をクライアントへ運ばない
（`sql::view::corrupt_view_error`）。列名（`22000 unknown column`）は既存の
束縛エラーと同じ扱い（カタログ情報であり秘匿情報ではない）。

## 対象外・申し送り

- ~~集計形の body、LIMIT／`ORDER BY` 付きの body~~・~~ビューを対象にした集計~~
  は Phase 2（Issue #1192）で対応済み（上記節参照）。ビューを対象にした
  ベクトル検索（`Statement::Select`）は仕様上も対象外。
- Phase 2 の申し送りのうち、外側の `WHERE`／`ORDER BY`、本文での CTE・集合演算・
  サブクエリ、3 テーブル以上の JOIN 本文は Phase 3（Issue #1360）で対応済み。残り:
  外側の集計・`DISTINCT`・ウィンドウ関数・式による `ORDER BY`・評価後射影形の連鎖は
  Phase 4（Issue #1411）で対応済み。残り: 外側の UDF 述語・サブクエリ・式項目・集計の式引数、
  本文でのウィンドウ関数・式項目・UDF 述語、JOIN の辺のビュー、評価後射影形ビューへの
  cursor `DECLARE`／`EXPLAIN`、サブクエリ付き本文を参照するクエリの Describe（サブクエリ付き
  SELECT 自体が Describe 未対応のため `42601`）。
- 許可名の述語呼び出し（`WherePredicate::PredicateCall`。空引数の呼び出し形
  で列参照を持たない）のビュー越し列スコープ検査。なお式項目
  （`SelectItem::Expr`）・式述語（`WherePredicate::Expression`）は
  `sql::view::check_columns_within_view` が式木を再帰的に走査して全列参照を
  検査する（PR #1048 レビュー対応で実装済み）ため対象外ではない。いずれも
  RLS 境界には影響しない——書き換え後の述語は基底テーブルへ委譲される既存の
  束縛・実行経路でそのまま評価されるため、この検査はビューが宣言した列
  スコープ契約のためのものであり安全性の境界ではない。
- ~~更新可能ビュー。~~ Issue #1670〜#1672 で対応済み（末尾「Issue #1675 追記」）。残りの
  `WITH CHECK OPTION` は #1673、RLS・台帳・crash の総合テストは #1674 で扱う。
- `MATERIALIZED VIEW`（TABLE-42）は Issue #1663〜#1667 で実装済み。設計は「Issue #1668 追記」、
  未対応事項も同節の「対象外」に集約した。
- NoSQL 表層の `scan`／`aggregate` op でビューを対象にすると従来どおり
  `42P01`（fail-closed のまま）。NOSQL-13 の DDL op はスコープ外。
- FOREIGN KEY 由来の `2BP01`（#907）。
- `SELECT *` の展開は参照時点で動的に行う（作成時点の列集合で固定する
  PostgreSQL 互換の方式は採らない）。
- 参照時の展開は Parse 時点で確定する（拡張クエリで Parse から Execute までの
  間にビューが削除されても、Parse 時点の定義で実行される。ビュー自体は権限を
  運ばないため権限昇格にはならない）。
- PR #1041（明示トランザクション。SQL-31）とのトランザクション内 DDL 拒否
  ガードの統合はマージ順に依存する申し送り事項。

## Issue #1659 追記: 列別名リスト `CREATE VIEW v (a, b) AS ...`

TABLE-41 (1) のポインタ。TABLE-18 の RLS 契約（参照したセッションの文脈で評価する）は変えない。
兄弟の `OR REPLACE`（互換性検査）は #1660、依存ビューの再検証は #1661 が扱う。

### 構文と構造検証（`sql::allowlist`、カタログは照会しない）

- `CREATE VIEW <name> [(<alias> [, <alias>]*)] AS <body>`。`IF NOT EXISTS` の `42601` が先に判定される
- 空のリスト・末尾カンマ・識別子でない要素は `42601`、件数が `catalog::MAX_COLUMN_COUNT`（256）を
  超えれば `54000`、リスト内の同名は `42701`
- 本文が列名を明示する単純形のとき、別名が列数より多ければ構造段で `42601`
- 判定順序は「構文 → 本文サイズ → DDL 権限（`42501`）→ カタログ判定」を保つ。`*` の本文・評価後射影形の
  本文との長さ突合と最終名の重複検査（`42701`）は権限ゲートの後（`EngineCore::check_create_view_column_aliases`）。
  権限のない主体へ列構成を返さない

### 出力列の規則

- 別名は出力列に先頭から順に名前を付け、短いリストの残りは本文から導いた名前を保つ。長いリストは `42601`
- 単純形の `SELECT *` の出力列は `id` が先頭で、利用者列がスキーマ順に続く（`tenant_id`・`visibility`
  は出さない）。したがって `v (x) AS SELECT * FROM t` の `x` は `id` に当たる。このリポの `*` の意味に
  揃えた規則で、作成時の導出（`sql::view::body_output`）と参照時の適用が同じ関数（`star_columns_of_table`）を
  共有する
- 別名を付けた後の最終的な名前が重複すれば `42701`（別名のないビューの既存挙動は変えない）

### 格納形式（`catalog::ViewDef`）

- `ViewDef` に `column_aliases: Vec<String>` を追加（公開型へのフィールド追加。破壊的変更）。
  別名が空なら従来の v1 と同一のバイト列、非空なら v2（v1 の 2 部分の後に `u32` LE の件数と、
  各別名の `u32` LE 長 + UTF-8）。件数（256）・長さ（63）は構文段・`Storage::create_view_with_column_aliases`・
  decode の 3 か所で bound し、上限検査の前に確保しない。破損は `CorruptSchema`（参照時は固定文言の `XX000`）
- `Storage::create_view` は空の別名で `create_view_with_column_aliases` に委譲する

### 参照時の 2 経路（本文の形で分ける）

- 単純形本文: インライン展開を保つ。`sql::view::resolve_from` の第 2 パスが連鎖の各段の公開名と
  基底表の物理名の対応（`Resolved::View::column_map`）を積み、`sql::view_alias` が列スコープ検査
  **の後に**外側の投影・`WHERE`・`ORDER BY`・集計を物理名へ書き換える。出力名は SQL-42 の
  「別名付き列参照」で公開名に保つ。段の `WHERE` も親の対応で物理名へ書き換えてから合成する。
  書き換えは `check_constraint` の列改名と同じ走査器（`map_predicate_columns`・`map_expr_idents`）を共有し、
  一括適用なので別名の入れ替え（`(lang, body) AS SELECT body, lang`）でも連鎖置換しない
- 評価後射影形本文（集計・`LIMIT` 等）と、評価後射影形ビューの上に重なる別名付きの段: 素通しの
  `Statement::BufferedView` 層（`column_aliases` 付き）を重ね、`sql::view_buffered::plan_outer` が
  外側の束縛より前に本文の結果列を先頭から改名する。Describe と Execute は同じ入口を通るため一致する
- 単純形本文を評価後射影形へ回さない理由: 単純形本文には `LIMIT` がなく、評価後射影形に回すと本文の
  行数上限（`ResultLimits` の既定 10,000 行）で選択的な外側 `WHERE` の対象行が黙って欠ける。
  「別名を付けても返る行は変わらない」ことを回帰テストで固定している

### 公開範囲と fail-closed

- 外側の列は別名（公開名）だけが見え、物理名・`tenant_id`・`visibility` での参照は `42703`。
  別名を `tenant_id` 等にしても、外側は別名の利用者列として束縛される
- 公開名の重複は `42702`、別名が出力列より長くなった（`*` の基底表の列が変わった）場合は `42601`。
  作成時の検査と書き込みの間の TOCTOU は、この参照時の再検査が塞ぐ
- 未対応の位置は `0A000` で拒否する（状態は変えない）: 集合演算の枝（括弧なし）、CTE の本文、ウィンドウ項目、
  投影位置のスカラーサブクエリ、サブクエリ述語、集計式項目・式の `HAVING`・項目名でない列を指す
  集計の `ORDER BY`、疑似列 `id` へ別名で写る列の投影以外での参照（`IdCompare` の意味の取り違え防止）。
  JOIN の辺のビューは従来どおり拒否
- 別名はテナントの可視性に関与しない名前の写像であり、RLS-10 (b) は展開後または本文の実行経路で
  参照したセッションの `PolicyContext` により判定される

### DDL 依存ゲート（保守側）

別名付きビューは `catalog::view_def_shape` で評価後射影形と同じ保守側（`buffered = true`）に分類する。
結果、別名付きビューが読む表への `RENAME COLUMN` は `0A000`、`DROP COLUMN`／`ALTER TYPE` は依存ありとして
拒否される（PostgreSQL は受理するため、精密な追随は後続課題）。連鎖の深さ・到達判定は本文の形のまま。
`RENAME COLUMN` で別のビューの定義を書き換える際は `column_aliases` を保つ。

## Issue #1660 追記: `CREATE OR REPLACE VIEW` の互換性検査

TABLE-41 (2) のポインタ（TABLE-18 の RLS 契約は変えない）。兄弟: 列別名リストは #1659、依存ビューの
再検証と原子的置換・途中失敗時の復旧は #1661。

### 構文と判定順序

- `CREATE OR REPLACE VIEW <name> [(<alias>, ...)] AS <body>`。`OR REPLACE` は VIEW 専用で、`OR` 単独・
  `OR REPLACE TABLE`／`INDEX`／`TYPE` 等と `IF NOT EXISTS` 併用は `42601`（`Parser::parse_create_view`）
- 順序: 構文 → 本文サイズ → DDL 権限（`42501`。互換性の情報を権限のない主体へ返さない）→ 別名の突合
  （`42601`／`42701`）→ 同名が表・索引なら `42809` → 新本文の検証（参照先不存在 `42P01` 等）→
  自己参照・循環 `42P17` → 互換性 `42P16` → 依存ビューあり `0A000` → ネスト深さ `54000` → 置換。
  不在なら通常の `CREATE VIEW` と同じ。明示トランザクション内の DDL は従来どおり `0A000`

### 互換性規則（`sql::view_replace::check_replace_compatible`）

- 新定義の出力列が、既存の出力列を同じ名前・同じ型・同じ順で保ち、末尾に列を足すだけのときに限り置換できる
- 列数の減少・位置ごとの名前不一致（改名・順序変更）・型不一致（`VARCHAR(n)` の n 等の typmod も完全一致）は
  `42P16`。静的な型を持たない列（ベクトル式等）は同一性を検証できないため `0A000`（fail-closed）
- 旧定義・新定義の出力列は同一関数 `EngineCore::view_output_columns`（`body_output` ＋ 本文を実行しない
  Describe。別名適用込み）で導出する。`SELECT *` は参照時に動的展開する方針のため、旧ビューの列は
  置換時点の展開結果で比較する（作成時に列を固定する PostgreSQL とは異なる）
- 照合順序（collation）は現行の列メタデータが保持しないため比較対象外

### 原子性と競合

- 置換は単一 write txn の 1 回の insert（`Storage::create_or_replace_view`）。互換性検査に使った旧定義を
  txn 内で現在の定義と突き合わせる compare-and-swap で、不一致は `ViewDefinitionChanged`。SQL 表層は
  検査からやり直し（最大 3 回。超過は `55P03`）、旧定義は変えない
- 新本文の検証は `validate_new_view_body_in_txn` を `create_view` と共有する（第 2 の検証実装を作らない）。
  自己参照・循環の探索は訪問済み集合と `MAX_VIEW_CHAIN_WALK` で打ち切る再帰なしの反復で、読めない場合は
  到達ありとみなす（fail-closed）
- ビューは行ストア・世代を持たないため `bump_table_generation_in_txn` は呼ばない

### 暫定制限（Issue #1661 で解除済み。以下は #1660 時点の記録）

- 置換対象に依存するビューがある場合は `0A000` で拒否し、状態を変えない（依存ビューの再検証と
  途中失敗時の復旧は #1661）
- 検査と commit の間の基底表 DDL による残余 TOCTOU は、依存ビュー不在の制限下では置換対象自身の出力に
  限られる（#1661 で txn 内検査化を検討）
- 公開型の変更: `CatalogError::{ViewDefinitionChanged, ViewReferenceCycle}`・
  `SqlSurfaceError::InvalidViewReplacement`・`ValidatedCreateView::or_replace` を追加（破壊的変更）

## Issue #1661 追記: `OR REPLACE` 時の依存ビュー再検証と原子的置換

TABLE-41 (2)(3) のポインタ（TABLE-18 の RLS 契約は変えない）。#1660 の「暫定制限」（依存ビューがあれば
`0A000`）を解除した。

### 原子性

- 置換は単一 write txn。新定義を未 commit のまま insert し、同じ txn で依存ビュー（推移閉包）を
  再検証してから commit する（redb は同一 txn 内の自身の書き込みを読める）。再検証が失敗すれば txn は
  commit されず破棄され、旧定義がバイト列ごと残る。クラッシュ時は redb の commit 原子性に依存し、追加の
  復旧機構は持たない（`table41_view_or_replace_crash_atomicity.rs` が SIGKILL で確認）
- 閉包は `dependent_views_closure_in_txn`（全定義を `MAX_VIEWS` 上限付きで 1 回走査し、
  `view_def_dependence_on` の逆辺を再帰なしの BFS で辿る。本文を読めないビューは依存ありとみなして拒否）

### 再検証の 2 段（`revalidate_dependent_views_in_txn`）

1. 作成時と同じ `validate_new_view_body_in_txn`（参照先の存在・評価後射影形／別名付きビューへの到達時の
   再検証・ネスト深さ `54000`）
2. 参照時と同じ `sql::view::resolve_from`（単純形の連鎖の列スコープ・別名の公開名重複）

形・列の破綻は `0A000`（`CatalogError::DependentViewsExist`。公開 enum は変えず、文言は固定英文で
置換対象名以外を含めない）。深さ超過は `54000`。カタログ破損・ストレージエラーはそのまま伝播する。

### `SELECT *` と列追加ゲート

- ビューの `*` は参照時に動的展開されるため、`v2 AS SELECT * FROM v1` は v1 に末尾列を足すと v2 の
  出力も末尾が増える（既存列の名前・型・順序は不変のため受理）
- ただし集合演算の枝・部分クエリ・CTE の中で `*` が置換対象へ届くと、列数が変わって参照時に壊れる。
  この壊れ方は上の 2 段では検出できないため、新定義が列を追加する置換（`appends_columns`。
  `EngineCore` が互換検査通過後に算出して渡す）では、閉包内に本文の `SELECT` が 2 個以上のビュー、または
  列別名付きの評価後射影形ビューがあれば保守的に `0A000` で拒否する
- 公開 API `Storage::create_or_replace_view` は互換性検査を行わないため、fail-closed として常に
  `appends_columns = true` で扱う
- 既知の過剰拒否: 明示列だけの集合演算でも、列追加の置換は拒否される

### 判定順序の追加分

… 互換性 `42P16` → txn 内の CAS・指紋照合 → 新本文の検証 → 未 commit の insert → 依存ビューの再検証
（`54000`／`0A000`）→ 列追加ゲート（`0A000`）→ commit。

### TOCTOU

互換性検査（Describe）は core に残す。検査が読んだ relation は #1660 の CAS と指紋が txn 内で照合済みで、
依存ビュー側は本 Issue で txn 内検査になる。検査後に作られた依存ビューも commit 時の閉包に入るため、
新たな窓は生じない。

### 対象外

- 新状態に対して依存ビューを Describe する完全な再検証（列追加ゲートの過剰拒否の解消）
- 照合順序の比較、静的な型を持たない列、ALTER VIEW、TABLE-42（#1663〜#1667 で実装済み。「Issue #1668 追記」参照）／43（#1670〜#1672 で一部実装済み。「Issue #1675 追記」参照。`WITH CHECK OPTION` は #1673）
- prepared statement の列メタデータ（`*` 依存ビューの列増加）

## Issue #1668 追記: MATERIALIZED VIEW（TABLE-42）

親 #1662。実装は #1663（カタログと実体ストア）・#1664（`CREATE`・読み取り）・
Issue #1665（`REFRESH`）・#1666（`REFRESH ... CONCURRENTLY`）・#1667（索引・`DROP`・CASCADE）で
完了している。
本節はそれらの設計判断をまとめた記録で、コードは変更しない。Issue 単位の経緯は
[implementation-status.md](implementation-status.md) の各エントリを参照する。

### 概要と非マテリアライズドビューとの境界（#1663）

- 定義は `VIEWS_TABLE` と分けた専用カタログ表 `materialized_views`
  （`catalog::materialized::MATERIALIZED_VIEWS_TABLE`）に置く。既存のビュー展開が matview を
  「展開できるビュー」と誤認しないため。名前空間は表・ビュー・索引と共有する
  （重複は `42P07`、種別不一致は `42809`）。
- 参照のたびに定義を展開する通常ビューと違い、matview は保存済みの実体を読む。
  `FROM <matview>` は `Resolved::Buffered` の葉（`Statement::MaterializedView`）として、
  評価後射影形ビューの外側処理（WHERE・ORDER BY・射影・集計・LIMIT）に乗る。
- 本文は `CREATE VIEW` と同じ許可形。`OR REPLACE`・`TEMP`・保存パラメータ・ベクトル順位付けは
  `42601`。

### 文脈単位の実体とテナント境界の不変条件（#1663・#1664）

- 実体は `PolicyContext` ごとに redb へ確保する。キー（`ContextKey`）はテナント ID と許可可視性の
  集合の完全一致で、`Public` のみの文脈が `Private` 込みの実体を読む経路を作らない。
- 「全文脈を読む」API は作らない。読み取りは読み手の文脈に完全一致する実体だけを返す。
- 実体が無い文脈（`WITH NO DATA`・実体化対象外）は `55000`。他文脈の有無・件数は観測できない。
- 件数不一致や行ヘッダのテナント不一致は `CorruptSchema` で拒否し、行を 1 件も返さない。
- RLS-10 (b) の暗黙適用は評価時（CREATE／REFRESH の文脈別評価）に既存経路を通る。
  読み取りで再評価はしない。実体は評価済みの行である。

### 実体化対象の文脈（#1664）

- wire-server が起動時に認証構成のテナント集合から `session_policy_context` で導出し、
  `EngineCore::with_materialization_contexts` で 1 回だけ注入する。導出に失敗したら起動エラー。
- Rust API で未設定なら、作成セッションの文脈だけが対象になる。

### 構文と判定順序（fail-closed）

- `CREATE MATERIALIZED VIEW [IF NOT EXISTS] <名前> [(<別名>, ...)] AS <本文> [WITH [NO] DATA]`
  （#1664）: 構文 `42601` → DDL 権限 `42501`（名前解決より先）→ 明示トランザクション内 `0A000`
  → 列型の確定（型なし・ENUM・ICU 照合の列を読む本文は `0A000`、不正な列名 `42601`、重複 `42701`）
  → 文脈ごとの評価（行数が `--max-result-rows` 超過で `54000`）。
- `REFRESH MATERIALIZED VIEW [CONCURRENTLY] <名前> [WITH [NO] DATA]`（#1665）: 構文 → 明示
  トランザクション内 `0A000` → `42501` → 書き込みゲート（待機超過 `55P03`）→ 定義のロード
  （無ければ `42P01`、表・ビュー・索引の名前なら `42809`）→ 列形状の再検証（`0A000`）→
  照合順序 → 文脈ごとの評価と逐次書き込み → commit（1 回）。
- `DROP MATERIALIZED VIEW [IF EXISTS] n1[, n2 ...] [CASCADE|RESTRICT]`（#1667）: `42501` →
  種別不一致 `42809`（`IF EXISTS` でも）／不在 `42P01` → 依存があれば `2BP01`。
- matview への INSERT／UPDATE／DELETE／TRUNCATE／COPY FROM／ALTER は `42809`。
- 成功タグ: `CREATE` は `SELECT <n>` で、`n` は作成者自身の文脈の行数だけ（他文脈の件数は
  合算しない）。`REFRESH`・`DROP` は件数を持たない固定タグ
  （`REFRESH MATERIALIZED VIEW`・`DROP MATERIALIZED VIEW`）。

### 原子性と失敗方針（#1664・#1665）

- CREATE と REFRESH は、書き込みゲートを取った 1 つの write txn を読み取り源と書き込みに兼用する。
  旧実体の消去・全文脈の再評価・書き込みは同一スナップショットで行い、commit は 1 回。
- 実行者自身の文脈の評価失敗は返して txn を破棄し、全文脈の旧実体と定義を残す。
- 他の文脈の失敗は実行者へ返さず、その文脈の実体を書かない（読み取りは `55000`）。成否の差から
  他テナントの件数を探れないようにするため。
- 「失敗した文脈だけ旧実体を残す」案は新旧スナップショットの実体が混在するため採らない。
- 読み手は MVCC スナップショットで旧実体を読み続け、REFRESH にブロックされない。
- `WITH NO DATA` は全文脈の実体を破棄して `populated=false` にする。

### 索引（#1667）

- 表と同じ `INDEX_CATALOG_TABLE` に `IndexDef { table: <matview 名> }` として宣言だけを置く。
  `MaterializedViewDef` は不変で `DEF_FORMAT_VERSION` は上げない。物理構造は持たない
  （matview の読み取りは実体全体を外側処理に渡すので、索引の有無で結果は変わらない）。
- 対象はスカラー列のみ。`USING hnsw` は `0A000`、暗黙の `id` 列は無く `42703`、COLLATE 不可型は
  `42804`。索引カタログの読み手は `def.table` で絞り込むため、他表の索引解決には影響しない。
- UNIQUE は文脈ごとに判定する（NULL は distinct、文脈をまたぐ同値は重複にしない）。強制箇所は
  `CREATE UNIQUE INDEX` 時の既存実体の走査と、実体の書き込みプリミティブ
  （`write_entities_in_txn`・`write_context_entity_in_txn`）の 2 つ。
  違反は `23505`（固定文言でテナント・値・件数を含めない）で、書き込みでは txn ごと破棄して
  旧実体を残す。REFRESH は共通プリミティブを通るため索引との整合が自動で満たされる。

### REFRESH CONCURRENTLY の方式（#1666）

- 前提検査は定義のロード後に行う。未実体化 `0A000` → WHERE を持たない列だけの UNIQUE 索引が
  無ければ `55000`（固定文言） → 以降は非 CONCURRENTLY と同一経路。
- 検査はカタログ構造だけで決まり、テナントのデータに依存しない（`populated` は定義単位の
  フラグ）ので、成否から他テナントの情報は漏れない。失敗は txn ごと破棄する。
- 差分適用は採らず全置換のままにした。理由: (a) MVCC により読み手は CONCURRENTLY の有無に
  よらず元からブロックされない、(b) 行キーが文脈キーと連番の組で、差分適用でも振り直しが要り
  利点が無い、(c) 書き込みプリミティブを二重化するとテナント境界のリスクだけが増える。

### CASCADE と依存（#1667・#1663・#1664）

- `dependency.rs` の `DropTarget::MaterializedView`／`DependentObject::MaterializedView` により、
  表・ビュー・matview の `DROP ... CASCADE` が依存 matview を依存する側から推移的に削除する。
  RESTRICT は `2BP01`。`CascadedObject::MaterializedView` は共有カタログの名前だけを運ぶ。
- DROP は定義・全文脈のメタ・行表・索引宣言の削除と世代 bump を同一 txn で行い、同名の
  再作成で旧実体・旧索引宣言が復活しない。
- #1663 の暫定ガードのうち、参照元の表・ビューの DROP に関する 3 箇所（直接ガード 2 箇所と
  閉包ガード）は #1667 で撤去した（RESTRICT 側の直接検査は残置）。
- 残した暫定ガード（表単位の保守的な判定）: 参照元の表の改名は `2BP01`、`DROP COLUMN` は
  `2BP01`、`ALTER COLUMN TYPE`・`RENAME COLUMN` は `0A000`。

### 上限値

- 定義数・文脈数の列挙は `MAX_LIST_TABLES`（10,000）。
- `MAX_MATVIEW_BASE_RELATIONS` = 64、本文は `MAX_VIEW_BODY_BYTES`（64 KiB）、
  スキーマ blob は 2 MiB、`DEF_FORMAT_VERSION` = 1。
- 文脈ごとの行数上限は `--max-result-rows`。設定がハード上限のときは超過検出のため
  1 つ手前（99,999 行）に丸める。

### PostgreSQL との差異

- 単純形本文の `*` は作成時に展開せず保存する。基表への `ADD COLUMN` で出力列がずれたら
  REFRESH は `0A000` で拒否し旧実体を残す（PostgreSQL は成功する）。
- 明示トランザクション内の CREATE／REFRESH は `0A000`（PostgreSQL は REFRESH を許す）。
- `CONCURRENTLY` と `WITH NO DATA` の併用は構文段で `42601`。
- CONCURRENTLY の重複行は `21000` ではなく `23505`（非 CONCURRENTLY と同結果にする方針）。
- 単一ライタのため、REFRESH 中は基表への書き込みが待たされる。
- NOTICE は送出しない（`IF NOT EXISTS`・`IF EXISTS`）。

### テスト

- `crates/engine/tests/table42_create_materialized_view.rs`・
  `table42_refresh_materialized_view.rs`・`table42_refresh_concurrently_reads.rs`
  （`test-sync-points`）・`table42_matview_index_drop.rs`
- `crates/wire-server/tests/wire_create_materialized_view.rs`
- `catalog::materialized::tests`・`sql::matview`・`sql::allowlist` の単体テスト

### 対象外

- ENUM／ENUM 配列列の matview 化。
- JOIN・CTE・部分クエリ・集合演算の枝・EXPLAIN・カーソルからの matview 参照（`42601` のまま）。
- ビューから matview を参照すること。
- NoSQL 表層からの matview 参照・REFRESH（fail-closed のまま）。
- `IF NOT EXISTS`・`DROP` の NoticeResponse。
- 表改名・列 DDL への列単位の追随と、matview→ビュー→表の推移依存。
- matview 索引による読み取りの高速化、`DROP INDEX ... CASCADE`。
- DROP CASCADE のクラッシュ原子性テスト。
- 式索引の取り込み時に、適格判定 `has_concurrent_refresh_unique_index_in_txn` から式を含む宣言を
  除く追随。

## Issue #1675 追記: 更新可能ビュー（TABLE-43）

親 #1669。実装は #1670（判定規則）・#1671（`INSERT`）・#1672（`UPDATE`／`DELETE`）。関連ポインタは
SQL-49・RLS-10 (b)・ERR-6 の `55000`。本節は設計判断の記録でコードは変更しない。Issue ごとの
経緯は [implementation-status.md](implementation-status.md) を参照。`WITH CHECK OPTION`（#1673）と
RLS・台帳・crash の総合テスト（#1674）は未実装で、本節では後続としてのみ扱う。

### 自動更新可能性の判定（#1670）

- `sql::view_updatable::view_updatability` は第 2 の分類器を作らず、読み取り経路と同じ
  `sql::view::resolve_from` の結果に乗る。`Resolved::View`（連鎖の全段が単純形）なら更新可能、
  `Resolved::Buffered`（評価後射影形）なら不可。
- 不可の理由（`ViewNotUpdatableReason`）は DISTINCT・GROUP BY・HAVING・集合演算・WITH・
  LIMIT／OFFSET・集計・ウィンドウ・単一の表ではない・下位が不可、を固定の優先順位で決定的に
  分類する。wire 文言には出さない（ビュー名・理由を含めない固定文言）。
- fail-closed: 判定の失敗（破損 `XX000`・深さ超過 `54000`）を「ビューではない」「更新可能」の
  どちらにも倒さず伝播する。

### `INSERT` の書き換え（#1671）

- Parse は構造検証と判定だけを行い（`ParsedSql::ViewInsert`）、書き換え（`sql::view_insert`）は
  Execute／Describe 時に現在のカタログで行う。Parse と Execute の間の `CREATE OR REPLACE VIEW`／
  `DROP VIEW` を取り込むため。
- 公開名に対して列スコープ検査を先に行い（非公開列は `42703`）、その後で物理名へ写す。写す対象は
  列リスト・`ON CONFLICT` の対象列・`DO UPDATE SET` の左辺・`EXCLUDED.<col>`・`RETURNING`。
- 別名で疑似列 `id` へ写る公開名は `0A000`。ビューに現れない基表の列は基表の `DEFAULT`
  （無ければ NULL）。`RETURNING` の列名と Describe はビューの公開名で返す。
- ビューの `WHERE` 述語は `INSERT` に適用しない（検査は #1673）。
- 書き換え後は既存の基表 `INSERT` 経路を通るため、制約・上限・テナント所有・台帳は基表の単位で働く。

### `UPDATE`／`DELETE` の書き換え（#1672）

- `sql::view_dml`（写像器と `RETURNING` の書き換えは `sql::view_insert` と共用）。
- 述語はビュー連鎖の述語（内側の段が先）、利用者の述語の順に AND で合成する。読み取り経路と
  同じ順序で、ビューの外の行は更新も削除もされない。
- 単一行形 `WHERE id = <n>`: ビューが `id` を公開しなければ `42703`、利用者列を公開名 `id` で
  公開していれば `0A000`、ビュー述語が空なら単一行形のまま基表へ、空でなければ述語形へ変換する。
- `PARTITIONED` 修飾は `0A000`。分割実行はチャンクごとに commit するため、指紋照合と再試行の
  意味が定まらないことが理由。
- 影響行数の上限（`--max-dml-affected-rows`）・RLS・`RETURNING` は基表への書き込みと同じに働く。
  Parse 時のパラメータ型推論は、公開列から基表の列型を引く。

### 並行 DDL と TOCTOU（#1671・#1672 共通）

- 書き換えの前に `Storage::view_write_fingerprint` で「ビュー連鎖の定義と終端の基表スキーマ」の
  指紋を取る（`CREATE OR REPLACE VIEW` と同じ `replace_fingerprint`）。`sql::view_write_scope` 経由で
  書き込み txn の冒頭にその指紋を照合する。
- 不一致なら書き込み前に中断し、`EngineCore::run_view_write` が上限つき（8 回）で再試行する。
  上限到達は `55P03`。

### 台帳と冪等再送

- 台帳のキーは基表名のまま。内容ハッシュには、ビュー経由のときだけ `content_hash::for_view_write` で
  ビュー名のドメインを被せる。
- 同じ `operation_id` をビュー経由で再送すると `23505`、同じ値を基表へ直接送ると `22023`。
  ビュー経由でない既存のハッシュは変わらない。

### RLS-10 (b) の不変条件と判定順序

- 書き換えは名前の写像と述語の AND 追加だけで、可視性を緩めない。書き込み対象行の所有テナントは
  書き込みセッションの `PolicyContext` が決め、ビュー作成者の可視性は引き継がない。
- 順序: 構文 → 更新可能性（不可なら `55000`）→ 解決 → `PARTITIONED` の拒否 → 公開名での列スコープ検査
  （`42703`）→ 物理名への写像（`id` 写像は `0A000`）→ 指紋照合と基表 DML 経路。

### PostgreSQL との差異

- ビュー経由の `PARTITIONED` は `0A000`。
- 並行 DDL との競合が上限を超えると `55P03` になりうる。
- `WITH CHECK OPTION` は未対応（#1673）で、宣言は `42601`。
- `TRUNCATE <view>` は `42809`。
- 非セッション API（`execute_insert_sql`／`execute_update_sql`／`execute_delete_sql`）と
  NoSQL 表層からのビューへの書き込みは `42P01`。

### テスト

- engine: `tests/table43_view_updatability.rs`・`table43_view_insert.rs`・`table43_view_update_delete.rs`
- wire-server: `tests/wire_view_insert.rs`・`wire_view_update_delete.rs`
- 単体: `sql::view_updatable`・`sql::view_insert`・`sql::view_dml`・`sql::view_write_scope`、
  `catalog` の `view_write_fingerprint_tracks_chain_definitions`

### 対象外（後続）

- `WITH CHECK OPTION`（#1673）。
- RLS・台帳・crash 耐性の総合テスト（#1674）。
- ビュー経由の `PARTITIONED` 分割実行 DML。
- 非セッション API と NoSQL 表層からのビューへの書き込み。
- `TRUNCATE` のビュー対応（`42809` を維持する）。
