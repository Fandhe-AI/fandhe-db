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

- `ORDER BY` 内の集計関数呼び出し（`ORDER BY sum(x)`）は `42601` のまま（Issue #1815 の管轄）。`HAVING` 内の
  集計関数呼び出しは Issue #1814 で対応した（下の「Issue #1814 追記」）。
- 式位置での単項マイナス、`ORDER BY (expr)`・算術で始まるキー。位置指定 `ORDER BY 1` は
  Issue #1792、`NULLS FIRST/LAST` は Issue #1791 で対応した（下の各「追記」）。
- `SELECT DISTINCT ... ORDER BY <式>`・`GROUP BY` なしの `HAVING`・GROUP BY キー自体の式化。
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
  集計呼び出しを持つ場合に限り受理する。`GROUP BY` なしの `HAVING` は従来どおり `42601`。
- fail-closed の境界: CTE 主クエリ・集合演算の枝・サブクエリ内部・ビュー本体・評価後射影形ビューの外側は `42601`、
  列別名ビューは `0A000`。単純形ビューでは `HAVING` 内の集計の引数をビューの公開列に限定する（非公開列は `42703`）。
  参照列の収集（サブクエリ・評価後射影形ビュー）にも `HAVING` の集計引数を含める。
- 対象外: `GROUP BY` なしの `HAVING`、JOIN 集計の `HAVING`、NoSQL（HTTP）表層、`$n` を含む `HAVING`、
  式形の中の `NUMERIC` 結果の算術（`sum(bigint) + 1 > 10` は `22000`）、左右を入れ替えた `10 < sum(bigint)`。
- テスト: `tests/sql58_having_aggregate_calls.rs`、wire 層は `wire_sql58_having_aggregate_calls.rs`。

## PostgreSQL との既知の差分

- 定数式（`ORDER BY 1 + 0`）・`$n` を定数として並べ替える受理は未実装（`42601`。序数としては扱わない）。
- `EXTRACT` の戻り型は `numeric` ではなく `f64`。`EXTRACT(hour FROM date)` はエラーにせず 0（`date_part` と同じ）。
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
