# EXTRACT と HAVING／ORDER BY の式位置（Issue #1188）

- 対象ビヘイビア: SQL-26（ポインタ: `docs/spec/04-behavior/sql-surface.md`・`docs/spec/05-tasks.md` TASK-210）
- 親 Issue: #1206（Phase 11）。関連 ADR: `datetime-scalar-functions.md`・`scalar-order-by-scan.md`・`case-coalesce-nullif.md`

## 背景

日時スカラー関数 ADR は次の 2 点を後続課題として見送っていた。

1. `EXTRACT(field FROM src)` 構文（`date_part('field', src)` での代替のみ）。
2. `HAVING`・`ORDER BY` の位置でのスカラー関数・`CASE`。従来は広域取得の `ORDER BY` が列名キーのみ、
   `GROUP BY` 集計の `HAVING` が `<ident> <cmp> [-]<number>` 限定、集計 `ORDER BY` が識別子限定だった。

## 決定

### EXTRACT

- `Parser::parse_call_expr` で `extract` の `(` 直後が「識別子または文字列リテラル」かつ次が
  `FROM` のときだけ専用形として解析し、AST 上で `date_part(<field 文字列>, <src>)` へ脱糖する。
  束縛・評価・field 検証（未知 field は `22000`）は `date_part` と共有し、CHECK・ビューの
  render→再パース往復も `date_part(...)` として再解析できる。
- `extract` は予約名にしない（先読みに一致しない `extract(...)` は通常の関数呼び出し）。
- 既定の結果列名は PostgreSQL 互換の `extract`（`AS` 無しの EXTRACT 構文由来の項目に限る。
  明示の `date_part(...)` は `date_part`）。
- prepared statement のパラメータ型推論がテーブル名を引く `FROM` は、括弧深さ 0 の最初のもの
  に限る（`core.rs::first_top_level_from_index`）。

### 広域取得の ORDER BY 式キー

- 受理形: キー先頭が `CASE WHEN`、または `<ident> '('`（順位付け関数 `HYBRID`／`HYBRID_RRF` を除く）。
  `COALESCE`／`NULLIF`／`EXTRACT` も後者に含まれる。ベクトル順位付けとの混在
  （`<=>`・`hybrid(...)`）・集計関数・位置指定・算術／括弧始まりのキーは従来どおり `42601`。
- 公開型 `ScalarOrderKey`・`ValidatedScan::order_by()` は変えない。式キーを含む文に限り crate 内の
  `ValidatedScan::order_keys`（`ScanOrderKey`）が全キーを出現順に保持し、そのとき `order_by()` は
  空になる（`has_expression_order_by()` で判別）。列キーのみの文は従来の表現のまま。
- 束縛は `udf_call::bind_expr`（WHERE・投影と同じノード予算を共有）。静的型から比較規約を決める
  （数値→`Float`・TEXT→`Bytes`・BOOLEAN→`Bool`・DATE／TIMESTAMP→`SignedInt`。VECTOR は `22000`）。
  未知関数・型不整合・未知列は既存の束縛エラー経路をそのまま使う。
- 実行は経路 (B)（上位 N 件 2 パス）。式は RLS・TABLE-12 整合検査・WHERE を通過した可視行に対してだけ
  行ごとに評価し、所有値（`OrderValue`）としてヒープ候補へ渡す。NULL 位置は既存規約（ASC 末尾・DESC 先頭）。
  デコード段階（`decode_tier_for`）は式が参照する列・embedding を含める。
- ビュー経由では式内の列参照もビューの公開列に限定する（`view::check_order_exprs_within_view`）。

### GROUP BY 集計の HAVING／ORDER BY

- `HAVING`: 従来形（`<ident> <cmp> [-]<number>`。整数を精度を落とさず厳密比較する既存契約）を先に試し、
  述語直後が境界（`AND`／`ORDER`／`LIMIT`／`OFFSET`／文末）でなければ式形 `<expr> <cmp> <expr>` として
  解析し直す。式形は少なくとも一方の辺が関数呼び出し・`CASE`・`COALESCE`・`NULLIF` を含むことを要求する
  （含まない `HAVING lang = 'ja'`・`HAVING c + 1 > 3` は従来どおり `42601`）。集計関数の直接記述は `42601`。
  件数上限は従来形と式形の合計に `MAX_AGGREGATE_ITEMS` を適用する。
- `ORDER BY`: `GROUP BY` 集計（`parse_aggregate_shape`）に限り式キーを受理する。`SELECT DISTINCT ... ORDER BY <式>`
  は従来どおり `42601`。
- 式は「グループ出力行ビュー」に対して束縛・評価する。合成スキーマの列は GROUP BY キー
  （`__gk<i>`）→集計項目（`__ga<j>`）の順で、キー列は実列型を正規化して使う（`INTEGER`→`BIGINT`・
  `REAL`→`DOUBLE`・`ENUM`→`TEXT`・`id`→`BIGINT`）ため非 `TEXT` キー（Issue #1185）にも追従する。集計項目の
  型は関数と入力から決める（`COUNT`・整数系→`BIGINT`、`AVG`・浮動小数・式入力→`DOUBLE`、`TEXT`／`DATE`／
  `TIMESTAMP` の `MIN`/`MAX`→同名型。`NUMERIC` 等は式内参照を `22000` で拒否）。
- 式中の識別子は従来の名前解決（キー名・キーの別名・集計項目名。曖昧は `42702`、未知は `22000`。Issue #1270）で解決してから
  合成列名へ置換する。疑似列 `id` やベースの非キー列を黙って参照させない。
- 実行（`group_by.rs` FINISH 段）は、可視行のみから作られた確定グループに対して評価する。`HAVING` は
  `Bool(true)` のグループだけを残す（`false`／`NULL` は除外）。`ORDER BY` の式は `sort_by` の前に
  グループごとに 1 回評価して保持し（評価エラーはここで返す）、広域取得と同じ比較器で並べる。
  グループが 0 件なら一切評価しない（Issue #353 の契約）。

### EXPLAIN との併用（Issue #1350）

- セッション経由の入口（pg wire の `execute_sql_in_txn`／`execute_sql_in_session`）では、式 `ORDER BY`／`HAVING`
  付きの広域取得・集計への `EXPLAIN` を受理し、静的判定の行だけを返す（SQL-27 の対象拡張と SQL-26 の組み合わせ。
  列名キーの前例は Issue #1189）。本体の式評価・行走査は行わない。
- 束縛は非 EXPLAIN と同じ経路を通るため、未知列・VECTOR 型・集計関数の直接記述などのエラー分類（SQLSTATE）は
  `EXPLAIN` の有無で一致する。`EXPLAIN` を前置しても束縛検証は迂回できない。
- セッションなしの入口 `EngineCore::execute_sql` は、式の有無にかかわらず `EXPLAIN` 全般を `42601` とする入口契約のまま。
  engine 側テストが以前この入口を呼んでいたため式併用の拒否を検証できておらず、pg wire との差に見えていた。

## 対象外（Issue 起票なし・後続の管轄を除く）

- `ORDER BY` 内・`HAVING` 内の集計関数呼び出しは Issue #1815・#1814 で対応した（下の各「追記」）。
- 式位置での単項マイナス、`ORDER BY (expr)`・算術で始まるキー。位置指定 `ORDER BY 1` は
  Issue #1792、`NULLS FIRST/LAST` は Issue #1791 で対応した（下の各「追記」）。
- `SELECT DISTINCT ... ORDER BY <式>`・GROUP BY キー自体の式化（キーの束縛と評価は Issue #1817、SELECT／HAVING／ORDER BY の式との照合は #1818。下の「Issue #1817 追記」）。`GROUP BY` なしの `HAVING` は Issue #1819 で対応した（下の「追記」）。
- NoSQL（HTTP）表層、JOIN・集合演算・ウィンドウと式 `ORDER BY` の併用。
- 評価後射影形ビュー（集計・`LIMIT` 本文のビュー。Issue #1192）への外側 `ORDER BY` の式キー（列名キーと
  同じく `42601`）。ウィンドウ関数・集合演算の枝内 `ORDER BY` との併用も、列名キーに限り併用可能にした
  後続（Issue #1189・#1191）とは別に、式キーを含む場合は `42601` とする（並べ替えを黙って落とさない）。
- SQLSTATE の `42883`／`42804` への移行（既存の束縛エラー経路を使うため移行はそのまま効く）。

## Issue #1792 追記（ORDER BY の序数指定。SQL-56 (2)・TASK-298 のポインタ）

- 判定（構文段・先読みのみ）: キーが「ASCII 数字だけで `i32` に収まる整数リテラル（先頭に単項 `-` 1 つ可）」で、
  直後がキーの終端（文末・`,`・`;`・`)`・`LIMIT`・`ASC`・`DESC`・`OFFSET`）のときだけ序数とする。
  `1 + 0`・`1.5`・`99999999999`・`-1 + 0`・`$1` は序数にせず従来どおり `42601`（`1::int` は従来の式キー）。
- 解決（束縛段）: 序数は位置のまま運び（`ScanOrderKey::Ordinal`・`AggregateOrderBy::ordinal`・
  `JoinOrderKey::ordinal`）、`*` 展開後の SELECT リストの出力列へ解決する。範囲外（`0`・負・列数超過）は
  `42P10`（`SqlSurfaceError::OrderByPositionNotInSelectList`。文言は書かれた位置だけで、判定は文とスキーマだけで
  決まり可視行・テナントに依存しない）。
  - 広域取得: 実列は列の添字、`id`（別名付き含む）は疑似列、式の項目は束縛済み投影の式を再利用する。
    ウィンドウ項目を挟んだ出力順は項目の位置から復元する。ウィンドウ項目の位置を指す序数は `42601`。
    ウィンドウ併用の `ORDER BY` は式キーだけを拒否し、序数は受理する。
  - 集計・`SELECT DISTINCT`: `GROUP BY` キー・集計項目・集計式の項目（式は行ビューへ束縛し直す）。
  - JOIN: 非集計形は `*` 展開後の列、集計形は SELECT リストの項目。
  - ビュー: 公開列の並びに従う（列別名つきビューも書き換え後の投影順）。
- 拒否のまま（fail-closed）: 投影位置のスカラーサブクエリとの併用（束縛前に項目が取り除かれ位置を復元できない）、
  評価後射影形ビューへの外側の序数、集合演算の枝内の序数は `42601`。
- 意図的に変えない: ウィンドウの `OVER (ORDER BY 1)`（PostgreSQL でも序数ではない）、`GROUP BY 1`
  （SQL-58 側の管轄）、NoSQL（HTTP）表層。
- テスト: `tests/sql56_order_by_ordinal.rs`、`crates/wire-server/tests/wire_sql56_order_by_ordinal.rs`。

## Issue #1791 追記（NULLS FIRST / LAST。SQL-56・TASK-298 のポインタ）

- 構文: 各キーの `ASC`／`DESC` の直後に `NULLS FIRST|LAST` を受理する（`NULLS`／`FIRST`／`LAST` は予約語化せず
  文脈的に照合するため、同名の列名キーは従来どおり使える）。対象は広域取得（列・式・序数・別名・`COLLATE` キー）、
  ウィンドウの `OVER (ORDER BY ...)`、`GROUP BY` 集計、`SELECT DISTINCT`、JOIN（非集計・集計）、ビューの外側。
  `NULLS` の後ろが `FIRST`/`LAST` でない形・順序違い（`NULLS FIRST DESC`）・重複・kNN 距離順（`<=>`・`HYBRID`）
  への併用は `42601`（fail-closed）。
- 設計: 公開型 `ScalarOrderKey` は変えない。明示キーを含む広域取得は、序数・式キーと同じく crate 内の完全形
  （`order_keys`）へ振り分け、束縛段が `NullPlacement` を確定して比較器（#1790）へ渡す。指定なしは従来どおり
  `pg_default(descending)`（ASC は末尾・DESC は先頭）で、既存クエリの経路・結果は変わらない。
  `qualified_column` の再解釈防止に `x.nulls FIRST`／`NULLS x.first` を追加した。
- 経路ごとの判断: 疑似列 `id` は NULL にならず一意なので `NULLS` は順序に影響しない（範囲走査の早期打ち切りを
  そのまま使う）。式索引の順序付き走査 API（ASC NULLS LAST 前提）は本番の ORDER BY 経路から呼ばれていない。
- 該当なし／対象外: 集計関数内の `ORDER BY`（`string_agg(x ORDER BY y)` 等）と集合演算の `ORDER BY` は構文自体が
  未実装（`42601` のまま）。NoSQL（HTTP）`sort` での NULL 位置指定は対象外。
- テスト: `tests/sql56_order_by_nulls.rs`（経路 × ASC/DESC × NULLS FIRST/LAST の独立オラクル照合・RLS・拒否形）。
  従来 `NULLS LAST` を `42601` としていた `sql25_scalar_order_by.rs`・`sql25_aggregate_order_by_scalar_keys.rs`・
  `sql27_explain_targets.rs`・`sql30_window.rs` の拒否ケースは、受理後も拒否される `NULLS MIDDLE` へ置き換えた。

## Issue #1793 追記（FETCH FIRST／OFFSET ROWS／WITH TIES。SQL-56 (3)・TASK-298 のポインタ）

標準 SQL の `FETCH {FIRST|NEXT} [n] {ROW|ROWS} {ONLY|WITH TIES}` と `OFFSET m {ROW|ROWS}` を受理する。

- **正規化方針**: 実体は `sql::fetch_clause`（トークン段）。`ONLY` は `LIMIT n`、`OFFSET m ROWS` は
  `OFFSET m` へ書き換え、構文検証の各経路は従来どおり `LIMIT`／`OFFSET` だけを解析する。範囲・上限・
  エラー契約は `LIMIT` と完全に同一（独自検証なし。`$n` の受理位置も同じ）。件数省略は 1 件。`FETCH` と
  `OFFSET` はどちらの順でも受け付け、エンジンの文法（`LIMIT n OFFSET m`）へ並べ直す。
- **階層**: 括弧グループごとに判定する。同じ階層での `LIMIT` と `FETCH` の併記、`FETCH` の重複、
  `PERCENT` など PostgreSQL 17 に無い形、`ONLY`／`WITH TIES` の欠落は `42601`。文頭の `FETCH`／`MOVE`
  （カーソル文）は書き換えない。適用位置は各パース入口と `relation_name::tokenize_resolved_with`
  （構文検証・名前解決より前。冪等）。
- **WITH TIES**: 正規形 `FETCH FIRST <n> ROWS WITH TIES` を、単一表のスカラー `ORDER BY` 広域取得
  （ウィンドウ・投影内スカラーサブクエリなし）と `GROUP BY` 集計の `ORDER BY` だけが解釈する。
  `ORDER BY` が同じ階層に無ければ `42601`。それ以外は黙って `ONLY` 扱いにせず拒否する:
  `SELECT` で始まらない文（`WITH`・`EXPLAIN`・`CREATE VIEW`・`DECLARE`・`INSERT`・`COPY` など）と
  括弧の内側は `0A000`、ウィンドウ・サブクエリ・CTE・評価後射影形ビュー・集合演算の枝も `0A000`、
  kNN・`USING PLAN`・DISTINCT・JOIN・集合演算・FROM 無し SELECT は構文が正規形を知らず `42601`。
- **実行**: 経路 (A)（先頭キーが `id`）は `id` が自テナント内で一意のため `ONLY` と同じ。経路 (B) は
  上位 `limit + offset` 件の選抜後に、最悪要素と `ORDER BY` キーが等しく `(id, tenant_id)` が後ろの
  可視行だけを 2 回目の走査で集める（`with_visible_row` を共有するため RLS・TABLE-12・WHERE・可視性の
  順序は同一）。同値行も結果行数の上限（`ResultLimits`）に数え、超過したら打ち切らず `54000`。
  `GROUP BY` 集計はソート・OFFSET 後の `limit` 件目と `ORDER BY` キーが等しい後続グループを残す
  （上限は `MAX_GROUPS`）。同値行の順序は経路の既存のタイブレーク（広域取得は `id` 昇順）。
  他テナントの同値行は同値判定にも件数にも現れない。
- **PostgreSQL との差分**: 上記の対応範囲外の `WITH TIES`（kNN・DISTINCT・JOIN・集合演算・ウィンドウ・
  サブクエリ・CTE など）は受理しない。`OFFSET m ROWS` と `LIMIT` の順（`OFFSET` 先）は `FETCH` を伴う
  場合のみ並べ替える（`OFFSET m LIMIT n` は従来どおり `42601`）。
- テスト: `tests/sql56_fetch_first.rs`、単体 `sql::fetch_clause`。

## Issue #1814 追記（HAVING 内の集計呼び出し。SQL-58 のポインタ）

- 構文段: トップレベルの集計 `SELECT`（`allow_aggregate_expr_items` が真の文）に限り、`HAVING` 内の集計呼び出しを
  読む。同一の呼び出し（別名を除いて同じ関数・引数・`DISTINCT`・内側 `ORDER BY`）は 1 つにまとめ、述語側は
  内部名（制御文字で始まる識別子。利用者の識別子と衝突しない）に置換して `GroupByClause::having_aggregates` へ
  出現順に溜める。呼び出し数は積む前に上限 32 で頭打ちにする（`54000`）。
- 従来形への脱糖: `<集計呼び出し> <cmp> [-]<数値>` は従来形の述語（`HavingPredicate`）にする。`sum(bigint)`・
  整数の `avg` のような `NUMERIC` 結果も、従来形の厳密比較で評価される。それ以外（`coalesce(max(x), 0) > 1`・
  `sum(a) > max(b)`・`count(*) * 2 > 3`）は式形で、既存の式 `HAVING` の経路で評価する。
- 束縛段: `HAVING` の集計呼び出しを、SELECT リストの同じ集計があればそれと共有し、無ければ隠し集計項目として
  `BoundAggregate::items` へ積む（可視項目と同じ入力解決・同じ WHERE／RLS 適用の走査・アキュムレータ）。
  隠し項目は `projection` に積まないため `RowDescription` に現れない。隠し項目込みの総数が 32 を超える
  場合は `54000`。`array_agg` は `0A000`、TEXT・日時の `MIN`／`MAX` は `22000`（従来形）、式内の `NUMERIC` 結果の
  算術は `22000`。エラー文言に内部名は出さない。
- SELECT リストに集計項目が無い形（`SELECT lang FROM t GROUP BY lang HAVING count(*) > 1`）は、`HAVING` が
  集計呼び出しを持つ場合に限り受理する。`GROUP BY` なしの `HAVING` は Issue #1819 で対応した。
- fail-closed の境界: CTE 主クエリ・集合演算の枝・サブクエリ内部・ビュー本体・評価後射影形ビューの外側は `42601`、
  列別名ビューは `0A000`。単純形ビューでは `HAVING` 内の集計の引数をビューの公開列に限定する（非公開列は `42703`）。
  参照列の収集（サブクエリ・評価後射影形ビュー）にも `HAVING` の集計引数を含める。
- 対象外: JOIN 集計の `HAVING`、NoSQL（HTTP）表層、`$n` を含む `HAVING`、
  式形の中の `NUMERIC` 結果の算術（`sum(bigint) + 1 > 10` は `22000`）、左右を入れ替えた `10 < sum(bigint)`。
- テスト: `tests/sql58_having_aggregate_calls.rs`、wire 層は `wire_sql58_having_aggregate_calls.rs`。

## PostgreSQL との既知の差分

- 定数式（`ORDER BY 1 + 0`）・`$n` を定数として並べ替える受理は未実装（`42601`。序数としては扱わない）。
- `EXTRACT` の戻り型は `numeric` ではなく `f64`。`EXTRACT(hour FROM date)` はエラーにせず 0（`date_part` と同じ）。
- `GROUP BY` なしの `HAVING` から SELECT リストの別名を参照する形（`SELECT count(*) AS n FROM t HAVING n > 1`）は、
  `GROUP BY` ありと同じ解決器を共有して受理する（PostgreSQL は `42703`）。`SELECT * ... HAVING` は `42601`
  （PostgreSQL は `42803`）。
- 式形の `HAVING`／`ORDER BY` は `f64` 意味論。`2^53` を超える整数（`id`・`COUNT`・`SUM` 等の結果を含む）は
  `WHERE`・投影の式と同じく丸めずに `22000` で拒否する。従来形の `HAVING` は厳密比較のまま。

## テスト

- 単体: `sql::allowlist` の `extract_*`・`scan_order_by_*`・`having_keeps_legacy_form_*`。
- 結合: `tests/sql26_extract.rs`・`tests/sql26_order_by_having_expressions.rs`（独立オラクルとの照合・
  NULL 位置・`LIMIT`／`OFFSET`・`wire_code` 契約・RLS 非漏えい・ビュー列スコープ・可視行 0 件）。
- 契約更新: `tests/sql_udf_call.rs` は、`ORDER BY vec_norm(embedding)` を従来 `42601` としていた
  テストを、受理と VECTOR 型式キーの `22000` を確認するテストへ置き換えた（受理範囲の意図的な拡大）。
- wire 層: `crates/wire-server/tests/wire_sql26_extract_order_having.rs`（簡易・拡張クエリ両経路で
  `RowDescription` の列名と型 OID・`DataRow`・`ErrorResponse` の SQLSTATE 完全一致・
  `ParameterDescription` の推論〔SELECT リストの `EXTRACT(... FROM ...)` を含む回帰ガード〕・
  束縛値とリテラル形のバイト一致・RLS 非漏えい。Issue #1276）。
- EXPLAIN 併用（Issue #1350）: `tests/sql26_order_by_having_expressions.rs` の
  `explain_accepts_expression_order_by_and_having_in_session`・`explain_expression_error_codes_match_non_explain`、
  wire 層 `crates/wire-server/tests/wire_sql26_explain_expression_order_having.rs`（簡易・拡張両経路の受理・
  SQLSTATE パリティ・本体非評価・テナント文脈間での応答同一性）。

## Issue #1815 追記（ORDER BY 内の集計呼び出し。SQL-58 のポインタ）

- 対象: `GROUP BY` ありの集計 `SELECT` の `ORDER BY` に集計関数の呼び出しを直接書ける
  （`ORDER BY sum(n) DESC NULLS LAST`・`ORDER BY coalesce(max(n), 0)`・序数キーとの併用）。
- 構文段: `HAVING` が積んだ隠し集計配列（`GroupByClause::having_aggregates`）を `ORDER BY` の解析へ引き継ぎ、
  同じ配列・同じ内部名の添字空間へ重複排除して積む（`HAVING sum(n) > 1 ORDER BY sum(n)` は 1 項目）。
  キー全体が集計呼び出し 1 つなら識別子キー（内部名）へ正規化し、それ以外は式キーのまま。
- 束縛段: `HAVING` と同じ解決関数で内部名を隠し集計項目へ解決する。識別子キーは `OrderTarget::Aggregate`
  となり既存の比較器（NUMERIC の厳密比較・NULL 位置）と `array_agg`／照合の拒否がそのまま効く。
  SELECT リストに集計が無い形（`SELECT lang ... GROUP BY lang ORDER BY count(*)`）は `ORDER BY` を読んだ後に判定する。
- fail-closed: 隠し集計は `HAVING` と配列を共有するため、ビュー本体・評価後射影形ビューの外側・列別名ビュー・
  参照列収集・単純形ビューの引数の公開列限定（非公開列は `42703`）は `HAVING` と同じ契約で効く。
  呼び出し数の上限 32（`54000`）は SELECT・`HAVING`・`ORDER BY` の合算。CTE 主クエリ・集合演算の枝・
  サブクエリ・`SELECT DISTINCT`・`GROUP BY` なしは従来どおり `42601`。入れ子の集計は `42601`。
- 対象外: 式キー内での NUMERIC 結果の算術（`ORDER BY sum(bigint) + 1` は `22000`。`HAVING` と同じ制限）、
  JOIN 集計・NoSQL（HTTP）表層・`$n` を含む形。
- テスト: engine `tests/sql58_order_by_aggregate_calls.rs`、wire `tests/wire_sql58_order_by_aggregate_calls.rs`、
  構文段の単体テスト `order_by_aggregate_calls_are_desugared_and_shared_with_having`。

## Issue #1819 追記（GROUP BY なしの HAVING。SQL-58 (4) のポインタ）

- 意味: 表全体（`WHERE`・RLS を通った可視行）を 1 つのグループとして集計し、その 1 行に `HAVING` を当てる。
  真なら 1 行、偽か `NULL` なら 0 行。可視行が 0 件でも集計の 1 行（`count` は 0、他は `NULL`）は作られ、
  それに `HAVING` を当てる（PostgreSQL 17 と同じ）。集計を含まない `HAVING 1 = 1` も受理する。
- 構文段: 振り分けは括弧の深さ 0 にある句位置の `HAVING`（`has_top_level_having_clause`）も集計形の目印にする。
  引用識別子 `"having"` を列名・別名・表名に使う既存の文は、直前が式の終端でなく直後が式の開始でないため
  句とみなさない。`GROUP BY` なしの `HAVING` は、キー 0 個の `GroupByClause`（`columns` が空）で表す。
  単独の `TRUE`／`FALSE`／`NULL` は比較述語へ畳み込む（式木に variant を足さない）。
- 束縛段: `BoundAggregate::group_by` は `None` のまま、別フィールド `ungrouped_having` にキー 0 個で束縛した
  句を入れる（`group_by` を `Some` にすると、グループ化の実行器が入力 0 行で 0 グループを返し、空テーブルでの
  PostgreSQL との一致が崩れるため）。SELECT リストの裸の列と `HAVING` 内の集計の外の列参照は、スキーマに
  列（疑似列 `id` を含む）があれば `42803`（固定文言。列名・値を載せない）、無ければ `42703`、曖昧なら `42702`。
- 実行段: 単一行の終端処理 `finish_aggregate_result`（キャッシュヒット・索引経路・全走査の 3 経路が共有）と
  評価後射影形の外側集計の終端 `GroupedRowAccumulator::finish` が `group_by::ungrouped_having_passes` を呼ぶ。
  判定はグループごとの `HAVING` と同じ規則（従来形の厳密比較・式形の 3 値論理）を共有する。`HAVING` だけが使う
  隠し集計項目があるときは必ず投影を通し、`RowDescription` との列数のずれを防ぐ。上限 32（`54000`）は SELECT・
  `HAVING` の合算。
- fail-closed の境界: CTE 主クエリ・集合演算の枝・サブクエリ・ビュー本体は `42601`。評価後射影形ビューの外側は
  `42601`（`HAVING` を黙って落とさない）。後続の `ORDER BY`／`LIMIT`／`OFFSET`、`OR`、`$n` は `42601`。
  単純形ビューでは集計の引数を公開列に限定する（非公開列は `42703`）。テナント境界は単一行集計の走査経路を
  共有するため、他テナントの行は集計にも判定にも現れない。
- 対象外: `GROUP BY` も `HAVING` も無い文での集計の外の列参照（#1818 で `42803` へ移行済み。下の「Issue #1818 追記」）、
  `HAVING <bool 列>`・`NOT`・`OR` など比較の形をとらない真偽式（`TRUE`／`FALSE`／`NULL` の単独リテラルを除く）、
  JOIN 集計・NoSQL（HTTP）表層・`$n` を含む形。
- テスト: engine `tests/sql58_having_without_group_by.rs`、wire `tests/wire_sql58_having_without_group_by.rs`、
  構文段・振り分けの単体テスト `accepts_having_without_group_by_as_a_zero_key_clause`・
  `having_without_group_by_rejects_unsupported_contexts`・`has_top_level_having_clause_distinguishes_quoted_identifiers`。

## Issue #1817 追記（GROUP BY キー式の束縛と評価。SQL-58 のポインタ）

`GROUP BY upper(lang)`・`GROUP BY n / 2` のように、グループキーに式を書けるようにした。親は #1816、
SELECT／HAVING／ORDER BY の式とグループキー式の照合（式キーの値の出力）は兄弟の #1818（下の「Issue #1818 追記」）。

- 受理形: トップレベルの集計 `SELECT` に限り、キーに式（関数呼び出し・算術・`CASE`・`COALESCE`・`NULLIF`・
  `date_trunc` 等）を書ける。列キーとの混在も可。括弧で囲んだだけの列（`(lang)`）は列キーに正規化する。
  CTE 主クエリ・集合演算の枝・サブクエリ・ビュー本体は従来どおり `42601`。キー数（上限 8）は列キーと式キーの
  合算で数え、超過は `54000`。起動時設定のキー数・グループ数の上限も式キーに効く。
- 構文段: `GroupByClause::key_exprs` に式木を積み、`columns` の当該位置には利用者が書けない予約内部名
  （制御文字の接頭辞）を入れる。このため既存の `columns` 前提の判定（キー 0 個の判定・SELECT の裸の列の
  照合）は #1817 の時点では変わらず、SELECT にキー式を書く形は `42601` だった（#1818 で受理へ更新）。式の範囲は括弧の深さ 0 の
  区切り（`,`・`HAVING`・`ORDER BY`・`LIMIT`・`FETCH`・`OFFSET`・`;`）までを先読みで決め、解析後に区間の末尾へ
  届かなければ `42601`。先読み検査: 集計呼び出しは `42803`、ウィンドウ関数は `42P20`、`$n` と部分クエリは `42601`。
- 束縛段: 基表スキーマに対して投影モード（PostgreSQL の結果型の算術。`n / 2` は整数除算）で束縛し、
  `BoundGroupBy::key_exprs` に `(束縛式, コンパイル済みプログラム, 型, 結果列型)` を持つ。キーは
  `BoundOrderKey { target: Expr(j), .. }`。比較規約は型で決める: 数値は Float、TEXT は Bytes、真偽は Bool、
  日付・時刻系は SignedInt。VECTOR は `22000`、INTERVAL・jsonb・numeric・uuid・bytea・配列は `0A000`、
  `json` は `42883`（等値演算子が無い）。行ビュー上の型は番兵（`BYTEA`）のままなので、式キーを SELECT／HAVING／
  ORDER BY から参照する形は `22000` で拒否された（#1818 で結果列型へ差し替え）。
- 実行段: 式キーを含む文は必ず全走査の `multi_groups` 経路を通る（単一 TEXT キー専用の索引経路は使わない）。
  行ごとのキー式の評価は、可視性判定・本体デコード・`WHERE`・可視性の再検査を通った行に限る（不可視行・
  `WHERE` 非該当行の値で評価エラーが起きると他テナントの行の存在が漏れるため）。評価エラーは文全体を失敗させ、
  部分結果を返さない。NULL のキーは 1 グループにまとまり、従来どおり末尾に並ぶ。`-0.0` と `0.0` は同じグループ。
  キー式が参照する列・embedding はデコードマスクへ反映する（漏れると縮小デコードで列が NULL と取り違えられる）。
  TEXT の評価結果もキー累計バイト数の予算に計上する。
- 単純形ビューの外側（`0A000`）・評価後射影形ビューの外側（`42601`）・ビュー本体（`42601`）では式キーを
  受理しない（非公開列を式経由で参照させない）。
- 既知の差分・対象外: 位置指定 `GROUP BY 1` は PostgreSQL では SELECT リストの位置参照だが、本実装では
  `42601`（黙って定数キーにしない）。重複した式キー（`GROUP BY n / 2, n / 2`）は PostgreSQL は受理するが、
  列キーの重複と同じく `42601`。整数式は f64 で評価されるため、2^53 を超える bigint の式値では分かれ方が
  PostgreSQL と異なりうる。式キーの値の出力・構造一致・非集計式の `42803` は #1818。JOIN 集計・NoSQL
  （HTTP）表層は対象外。
- テスト: engine `tests/sql58_group_by_key_expressions.rs`、wire `tests/wire_sql58_group_by_key_expressions.rs`、
  構文段の単体テスト `group_by_expression_keys_are_collected_with_placeholders`・
  `group_by_key_precheck_classifies_aggregates_windows_and_params`・`group_by_expression_keys_reject_unsupported_forms`。

## Issue #1818 追記（SELECT／HAVING／ORDER BY の式とグループキー式の構造一致。SQL-58 のポインタ）

`GROUP BY upper(lang)` のとき `SELECT upper(lang), count(*)`・`HAVING upper(lang) > 'A'`・
`ORDER BY upper(lang)` のように、同じ式をグループキーとして投影・参照できるようにした（親 #1816、
前提の #1817）。集計の外で `GROUP BY` に無い列を参照する形は `42803`（grouping_error）で拒否する
（ERR-6・SQL-14 の改訂注記のポインタ。集計の外の列参照の旧 `42601` からの契約変更）。

- 構造一致の単一実装: `sql::parser::substitute_group_key_exprs`。式を上から順に調べ、`GROUP BY` の式キーと
  構造が一致する最大の部分木を、そのキーの予約内部名（#1817 の `columns` 上の名前）の識別子へ置き換える。
  比較前に関数名を ASCII 小文字へ揃える（括弧は構文木に残らない）。識別子・リテラルの表記は揃えない
  （`2` と `2.0` は別の式）。集計呼び出しは構文段で内部名へ脱糖済みのため、集計の中は照合の対象外
  （集計の中では非グループ列を参照できる）。SELECT の式項目・HAVING の式述語・ORDER BY の式キー・序数
  `ORDER BY n` の再束縛が同じ関数を使う。
- SELECT 項目: トップレベルの集計 `SELECT` に限り、集計呼び出しを持たない式（`upper(lang)`・`n / 2`・
  `CASE`・`date_trunc`）も `AggregateSelectItem::Expr`（`aggregates` が空）として構文段が受理する。置換後に
  式全体が 1 つの式キーなら `ProjectionColumn::GroupKey`（列名は `AS` 別名、無ければ PostgreSQL の既定名
  〔`upper`・`?column?`〕。予約内部名は列名へ出ない）、部分一致（`n / 2 + 1`・`upper(lang) || ...`）は
  `ProjectionColumn::Expr` で投影する。集計呼び出しを 1 つも持たない文は従来どおり `42601`
  （`SELECT upper(lang) FROM t GROUP BY upper(lang)` を含む）。
- 照合の移動: トップレベルでは、SELECT の裸の列と `GROUP BY` 列の照合・`GROUP BY` なしの裸の列の検査を
  構文段（`42601`）から束縛段へ移した（スキーマを要する分類のため）。置換後に残った集計の外の列が
  `GROUP BY` 列でなければ、スキーマに列（疑似列 `id` を含む）があれば `42803`、無ければ `42703`
  （文言は固定で列値を載せない）。HAVING／ORDER BY で集計項目・キー・別名のいずれにも解決できない識別子も
  同じ分類（従来は `GROUP BY` なしの `HAVING` を除き `42703`）。CTE 主クエリ・集合演算の枝・サブクエリ・
  ビュー本体は従来どおり構文段が `42601`。
- HAVING／ORDER BY の構文: `GROUP BY` に式キーがある文に限り、関数・`CASE` を含まない式述語
  （`HAVING n / 2 > 1`）と、識別子・数値の直後が算術演算子の ORDER BY キー（`ORDER BY n / 2`）を受理する。
  キーを含まない列参照は束縛段が `42803`。式キーの無い文の挙動（`HAVING lang = 'ja'` の `42601` 等）は不変。
- 行ビュー・実行段: 行ビューの式キー列の型は `BoundGroupKeyExpr` の結果列型（無ければ静的型の既定写像。
  整数は `precise` でない行ビューでは BIGINT、REAL は DOUBLE へ正規化。列キーと同じ）。キー値は
  `group_by::group_key_expr_cell` が投影式と同じ写像（`expr_value_to_cell`）で `Cell` へ復元する。値の種別が
  静的型と食い違う場合は `XX000`（fail-closed）。RowDescription の型は結果列型。評価位置（可視性判定・
  `WHERE` を通った行のみ）は #1817 のまま不変で、出力は確定したグループのキー値から作る。
- 既知の差分・対象外（追跡）: SELECT の出力別名を `GROUP BY` で参照する形（`GROUP BY u`）は未対応
  （`42703` のまま。PostgreSQL は入力列を優先して出力別名も解決する）。位置指定 `GROUP BY 1`・重複キー・
  2^53 超の bigint 式キーの精度は #1817 の差分を引き継ぐ。単純形ビュー・評価後射影形ビュー経由の式キー、
  CTE・集合演算の枝・サブクエリ、JOIN 集計、NoSQL（HTTP）表層は対象外（fail-closed）。
- テスト: engine `tests/sql58_group_key_expr_matching.rs`、wire `tests/wire_sql58_group_key_expr_matching.rs`、
  `sql::parser` の単体テスト `substitute_group_key_exprs_*`。契約変更に伴い、`GROUP BY` に無い列の参照を
  `42601`／`42703` と期待していた既存テストの期待値を `42803`／`42703` へ更新した
  （`sql42_aggregate_*`・`sql26_order_by_having_expressions`・`sql58_having_without_group_by`・
  `wire_aggregate`・`wire_sql26_extract_order_having` 等）。
