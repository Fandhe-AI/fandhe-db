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
- 式中の識別子は従来の名前解決（キー名・キーの別名・集計項目名。曖昧・未知は `22000`）で解決してから
  合成列名へ置換する。疑似列 `id` やベースの非キー列を黙って参照させない。
- 実行（`group_by.rs` FINISH 段）は、可視行のみから作られた確定グループに対して評価する。`HAVING` は
  `Bool(true)` のグループだけを残す（`false`／`NULL` は除外）。`ORDER BY` の式は `sort_by` の前に
  グループごとに 1 回評価して保持し（評価エラーはここで返す）、広域取得と同じ比較器で並べる。
  グループが 0 件なら一切評価しない（Issue #353 の契約）。

## 対象外（Issue 起票なし・後続の管轄を除く）

- `HAVING`／`ORDER BY` 内の集計関数呼び出し（`HAVING count(*) > 1`・`ORDER BY sum(x)`）は `42601` のまま。
- 式位置での単項マイナス、`ORDER BY (expr)`・算術で始まるキー、位置指定 `ORDER BY 1`、`NULLS FIRST/LAST`。
- `SELECT DISTINCT ... ORDER BY <式>`・`GROUP BY` なしの `HAVING`・GROUP BY キー自体の式化。
- NoSQL（HTTP）表層、JOIN・集合演算・ウィンドウ・`EXPLAIN` と式 `ORDER BY` の併用。
- 評価後射影形ビュー（集計・`LIMIT` 本文のビュー。Issue #1192）への外側 `ORDER BY` の式キー（列名キーと
  同じく `42601`）。ウィンドウ関数・集合演算の枝内 `ORDER BY` との併用も、列名キーに限り併用可能にした
  後続（Issue #1189・#1191）とは別に、式キーを含む場合は `42601` とする（並べ替えを黙って落とさない）。
- SQLSTATE の `42883`／`42804` への移行（既存の束縛エラー経路を使うため移行はそのまま効く）。

## PostgreSQL との既知の差分

- `EXTRACT` の戻り型は `numeric` ではなく `f64`。`EXTRACT(hour FROM date)` はエラーにせず 0（`date_part` と同じ）。
- 式形の `HAVING`／`ORDER BY` は `f64` 意味論（2^53 超の整数は丸める）。従来形の `HAVING` は厳密比較のまま。

## テスト

- 単体: `sql::allowlist` の `extract_*`・`scan_order_by_*`・`having_keeps_legacy_form_*`。
- 結合: `tests/sql26_extract.rs`・`tests/sql26_order_by_having_expressions.rs`（独立オラクルとの照合・
  NULL 位置・`LIMIT`／`OFFSET`・`wire_code` 契約・RLS 非漏えい・ビュー列スコープ・可視行 0 件）。
- 契約更新: `tests/sql_udf_call.rs` は、`ORDER BY vec_norm(embedding)` を従来 `42601` としていた
  テストを、受理と VECTOR 型式キーの `22000` を確認するテストへ置き換えた（受理範囲の意図的な拡大）。
