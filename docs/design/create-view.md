# `CREATE VIEW` / `DROP VIEW`（非マテリアライズド）の設計判断

Issue #909・対象ビヘイビア: TABLE-18・SQL-23（TASK-205）。関連ポインタ:
RLS-10 (b)（複数のテーブル参照を持つ読み取り経路での暗黙適用）・ERR-6
（`42P07`／`2BP01`／`42809` の新設）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 概要

保存したクエリをビューとして登録し、`FROM` に書いて読めるようにする。
**非マテリアライズド**とし、参照のたびに定義を展開する。ビュー定義は既存の
許可リストパーサーをそのまま通したもの（許可リストの部分集合）に限って保存し、
参照時は保存済みの定義を**同じパーサーで再検証**する。第 2 の SQL パーサー・
実行器は作らない。

## 受理する構文（Phase 1）

```
CREATE VIEW <name> AS SELECT <* | 列名リスト> FROM <table | view> [WHERE <単純述語> [AND ...]]
DROP VIEW <name>
```

- body の `LIMIT`・`ORDER BY ... <=>`・`HYBRID`・`USING PLAN`・集計形・式項目
  （`Projection::Items`）・UDF 呼び出し述語（`WherePredicate::PredicateCall`／
  `Expression`）・`EXPLAIN`・`CREATE OR REPLACE`・`IF NOT EXISTS`・`TEMP`／
  `MATERIALIZED`・列別名リスト・`WITH CHECK OPTION`・末尾の余剰トークンは
  いずれも構造的に受理しない（`42601`）。使える単純述語は
  `Equality`／`Prefix`／`BoolEquality`／`BoolColumn`／`Compare`（列 対
  リテラル）のみ。
- `DROP VIEW` は単一ビュー名の指定のみを受理し、`IF EXISTS`・
  `CASCADE`／`RESTRICT`・複数名指定は `42601`。
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
  参照セッションの UDF レジストリに依存しないようにする）。CTE・集合演算・
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
  ウィンドウ・式述語・UDF 述語・サブクエリは `42601`）。
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
  「削除して作り直す」ことで循環を組めない。`CREATE OR REPLACE VIEW`・
  `ALTER VIEW` は `42601`。
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
- **ビューへの書き込み**（`INSERT`／UPSERT／`UPDATE`／`DELETE`／`TRUNCATE`）は
  `42809`。`EngineCore::parse_tokens` の書き込み分岐（`validate_insert_tokens`
  等）が `UndefinedTable` を返し、かつその名前がビューとして存在する場合に
  限り `WrongObjectType` へ読み替える（`EngineCore::
  reclassify_write_to_view_error`。1 か所のみ）。副作用（台帳・世代・行）が
  一切発生しないことをテストで固定した。

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
- 更新可能ビュー。
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
