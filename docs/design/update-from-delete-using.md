# UPDATE ... FROM／DELETE ... USING の parser と束縛

Issue #1754（親 #1753）。ビヘイビア SQL-53 (b) のポインタ。関連: RLS-10 (b)・SQL-28（JOIN）・SQL-36（WHERE 省略）・RECOVER-11。
spec 本文は転記せず、ID のポインタのみを記す。

## 範囲

本 Issue（#1754）は **構文と束縛だけ**を担当した。束縛・変換（#3201）を通った文は、#3202 で結合実行器へ dispatch される（下記「dispatch」節）。

| 担当 | 内容 |
| ---- | ---- |
| #1754（本書） | 構文の解析、表の存在確認、列参照の解決、SET 右辺の型照合 |
| #1755 | 結合の実行、多重一致時の 1 回だけの更新、影響行数上限 |
| #1756 | 全参照への RLS 適用、`RETURNING` |
| #1757 | 台帳の content_hash と operation_id の再送冪等（文ハッシュは構文段で算出済み。実行入口で事前照合） |

## 構文

```text
UPDATE <表> [[AS] <別名>] SET <列> = <右辺>[, ...] FROM <from_list> [WHERE <join_where>]
       [USING OPERATION_ID '<id>' | NULL]
DELETE FROM <表> [[AS] <別名>] USING <from_list> [WHERE <join_where>]
       [USING OPERATION_ID '<id>' | NULL]
<from_list> := <rel> { , <rel> | CROSS JOIN <rel> | [INNER | LEFT | RIGHT | FULL [OUTER]] JOIN <rel> ON <cond> }*
<rel> := <表> [[AS] <別名>]
```

`<from_list>` の JOIN 連鎖は Issue #3294 で受理した（下記「多方向結合」節）。

- `WHERE` は JOIN の `WHERE` 文法（`JoinWhereExpr`）を再利用する。列同士の比較・`AND`／`OR`・括弧を受理する。DML の WHERE だけが、数値リテラルとの比較（`= < <= > >=`）・数値 `IN`・`IS [NOT] NULL` の葉を加えて受理する（Issue #3295。SELECT の JOIN の WHERE／ON は同じ入力で `42601` のまま）。`NOT`・サブクエリ・`<>`・`IS TRUE` は構造的に `42601`
  になる。#1750 のサブクエリ形と FROM 形は当面同時に使えない
- SET 左辺は非修飾の列名だけ（`SET t.a = ...` は `42601`）。行値の代入 `(a, b) = (...)`／`= ROW(...)` は Issue #3296 で受理（下記「SET の行値代入と FROM 側の式」節）
- SET 右辺は 2 系統に分ける
  - 列参照 1 個（`DmlFromSetValue::Column`）: 修飾あり・なしとも FROM 側の列を指せる。直後が句境界（`,`・`FROM`・`WHERE`・`USING`・`RETURNING`・文末）のときだけ
  - 列参照を含む式（`DmlFromSetValue::Expr`。Issue #3296）: 式中の列参照は修飾の有無によらずプレースホルダへ置き換えて参照ごとに運び、FROM 側の列も参照できる
  - それ以外（`DmlFromSetValue::Value`）: リテラル・`DEFAULT`・列参照を含まない式（既存の `SetValue`）
- 参照数（対象表を含む）は `MAX_TABLE_REFS` で積む前に検査する（超過は `54000`）。公開名の重複は `42712`（Issue #3297。PG の `duplicate_alias`。SELECT の JOIN が使う `BindingScope::new` は `42601` のまま）。別名で公開名を分ければ自己結合は受理する
- 派生表・関数呼び出しは `42601`。`JOIN ... USING (...)`・`NATURAL JOIN`・カンマの後の `CROSS JOIN` 以外の明示 JOIN は `0A000`（意図的な拒否。下記「多方向結合」節）
- `PARTITIONED` 併用は `42601`、`RETURNING` は `*`・`<公開名>.*`・列参照を受理する（#3204。式・関数・`AS` 別名は `42601`。束縛・実行は [sql-returning.md](sql-returning.md) の「結線」節）、`$n` は Issue #3297 で受理（「`$n` と公開名重複」節）
- `--require-where-for-dml` は FROM／USING 形でも `WHERE` 省略を `42601` で拒否する（opt-in のときだけ）

## 既存解析器との振り分け

- 入口（`EngineCore::parse_tokens_in`）が `looks_like_update_from`／`looks_like_delete_using` で先読みし、専用の解析器へ回す。既存の `parse_update`／`parse_delete` は変えない
- `looks_like_update_from`: 括弧深さ 0 の `FROM`（`IS [NOT] DISTINCT FROM` を除く）がある `UPDATE` 文
- `looks_like_delete_using`: 括弧深さ 0 の `USING` の直後が `OPERATION_ID` でない `DELETE FROM` 文。`DELETE FROM t USING OPERATION_ID '...'`（WHERE 省略形）は従来どおり既存の経路を通る。`operation_id` という名前の表を USING に書く形は operation_id 句として扱われ、既存の解析器が `42601` で拒否する（fail-closed）
- 非セッション入口（`validate_update`・`validate_delete_statement` など）は FROM／USING 形を従来どおり `42601` で拒否する（`EXPLAIN` の FROM／USING 形は Issue #3293 で受理。末尾「EXPLAIN（Issue #3293）」節）
- 別名付きで FROM／USING が無い単一表 DML（`UPDATE t AS x SET ... WHERE ...`）は Issue #3291 で受理に変わった。入口のトークン正規化で別名なしの同じ文へ書き換える（下記「別名つき単一表 DML」節）

## 新しい `ParsedSql` variant

`ParsedSql::UpdateFrom`／`ParsedSql::DeleteUsing` を追加した（BREAKING CHANGE）。既存の `ValidatedPredicateUpdate`／`ValidatedPredicateDelete` に `from` を足すと、既存の実行腕が
FROM を黙って無視する（単一表として処理する）fail-open 経路ができるため、別 variant にして網羅 `match` の追随漏れをコンパイラに検出させる。

- `parsed_operation_id` は `None` を返す。拒否される文の operation_id を `SessionTransaction` に登録しないため。SQL 経路の dispatch 結線時に見直す（#1755・#1757 は実行入口と台帳ハッシュまで）
- 一時表が関与する形は `is_routable` が `false` を返し `0A000`。ビューが関与する形は `0A000`

## 束縛（`sql::dml_from`）

対象表と FROM／USING 側の表を 1 つの `BindingScope` にまとめ、純関数で次を行う。カタログ照会・行の読み取り・RLS・台帳には触れない。

1. スコープ構築（参照数 `54000`・公開名の重複 `42712`）
2. `WHERE` の全列参照を解決（修飾子の不一致 `42P01`・非修飾で複数表に該当 `42702`・未知列 `42703`）
3. SET 左辺の検査（重複・疑似列・未知列・`GENERATED ALWAYS`）
4. SET 右辺: 列参照は解決と代入可否の判定（不一致は `42804`）、式は全参照をスコープで解決する（Issue #3296。`42P01`・`42702`・`42703`）。対象表の列だけの式は従来の式束縛、FROM 側を含む式は変換器が束縛する

結合条件の抽出・プッシュダウン・実行計画は #1755 の担当。

## 変換器（Issue #3201。`sql::join::dml_plan`）

束縛結果（`BoundUpdateFrom`／`BoundDeleteUsing`）を結合 DML の実行入力（`JoinedDmlPlan`）へ変換する純関数。行・RLS・台帳・書き込みトランザクションには触れない。SQL 経路の dispatch は #3202（`core::dml_from_exec::plan_*_stmt` が入口。下記「dispatch」節）。

WHERE は最上位の `AND` で conjunct に平坦化し、解決済みの relation 添字だけで（表名では判定せず）次のとおり全域に振り分ける。

| conjunct | 行き先 |
| -------- | ------ |
| 別 relation の列同士の等値 | 結合キー（対象側・source 側へ向きを正規化。複合キー可） |
| 対象側だけを参照（`OR` 可） | 対象側フィルタ（既存の述語形 DML と同じ束縛器） |
| source 側だけを参照（`OR` 可） | 単一表 source は走査へ押し下げる述語。畳み込み source（Issue #3294）は合成 JOIN 文の `WHERE` |
| 対象と source をまたぐ上記以外（非等値・`OR`・キー化できない型の等値） | 残余述語（Issue #3295。単一表 source のみ。畳み込み source は `0A000`。下記「非等値・OR をまたぐ述語」節） |
| 上記以外（参照列が無い等） | `0A000` |

`0A000` で拒否する形（設計上の対象外・fail-closed の安全弁。取りこぼした述語は書き込み範囲を広げるため、未分類は必ず拒否する）: 畳み込み source（FROM／USING が 2 表以上・JOIN 段あり）での対象と source をまたぐ非等値・`OR`・キー化できない型の等値、embedding を参照する対象側述語・残余、SET 右辺の source 疑似列 `id`・結合ビューへ写像できない型の source 列、残余での source 疑似列 `id`・VECTOR／配列列の参照。同じ relation 内の列比較は Issue #3295 で対象側／（単一表）source 側へ押し下げて受理する。キー列の型クラスが比較できない組は `42804`。等値結合キーが無い形（WHERE 省略・source だけの述語）と FROM／USING が 2 表以上の形は Issue #3294 で受理した。

SET は、リテラルと対象表の列だけを使う式をそのまま運び、列参照（source 列を含む）と source 列を含む式（Issue #3296）を「対象列 ++ source の取り出し列」の合成スキーマ上の式として束縛する（合成名は利用者の識別子と衝突しない綴り）。Describe 経路で変換エラーを返すかどうかは #3202 の方針の範囲。

## 多方向結合（Issue #3294。親 #3290）

ポインタ: SQL-53 (b)(3)(4)(5)・RLS-10・RECOVER-10/11・SQL-28。FROM／USING 内の `JOIN ... ON` と 3 表以上の結合を実行する。

- **畳み込み**: source が 2 表以上、または JOIN の段を持つとき、source 側のリレーション列と結合段（カンマは `Cross` 段）から合成 JOIN 文（`FoldedSource`）を作り、既存の結合実行器（`sql::join::execute_with_limits`）で評価した出力 1 つを source 行源として扱う。対象表はその行源と結合キーで突き合わせる。第 2 の結合実行器は作らず、各リレーションは書き込むセッションの `PolicyContext` で独立に走査する（RLS-10 (b)）。走査は共有 write txn 上で行い、文の開始時点のスナップショットになる
- **畳み込みスキーマ**: SET・結合キー・`RETURNING` が読む source 列だけを疑似列 `id` なしの採番方式スキーマとして並べる（疑似列 `id` は整数クラスの `BIGINT` 列）。`JoinedDmlSource` の添字系列（`columns`・`EquiKey::source_column`・セル位置）はその添字を指す。probe は単一表 source と共通
- **単一表 source（JOIN の段なし）**: 従来の単一表走査のまま。結合キーが 0 本でもよい
- **WHERE の振り分け**: 畳み込み source で source だけを参照する述語は、修飾した論理木のまま合成 JOIN 文の `WHERE` へ渡す（外部結合の意味論は結合計画器の pushdown・残余が守る）。複数の source にまたがる述語・列比較も可。対象表と source をまたぐ述語は列同士の等値だけを結合キーにし、それ以外は、単一表 source でなければ `0A000`（畳み込み source での残余は未対応。#3295 の後続で、複数 source をまたぐ非等値・OR の拒否の受け皿は #3534）
- **結合キーが 0 本**: 全 source 行を空キーの 1 バケットに入れ、対象行 1 件ごとに一致ペアへ source の行数を加算する。累計が出力行数上限を超えたら `54000`
- **ON の束縛**: `ON` は FROM 側の表だけのスコープで解決する（段 k が参照できるのは先頭から k + 1 番目まで）。対象表への参照は PG と同じく拒否（修飾参照は `42P01`、対象表にしか無い非修飾列は `42703`）。判定順は「スコープ構築 → ON → WHERE → SET → RETURNING」。ON の型検証・結合キー／残余の分類は結合実行器が担い、変換の段階で合成 JOIN 文を束縛して Describe と Execute のコードをそろえる
- **上限**（超過はすべて `54000`・副作用ゼロ）: リレーション数（対象表 + source の数）、各リレーションの入力行数、結合段の出力、NLJ の比較回数、畳み込み出力の行数（単一表 source と同じ可視行数上限）、累計バイト予算（結合実行器の上限を共有予算の残りに絞り、返った行を共有予算へ計上）、一致ペア
- **RETURNING**: 畳み込み行 1 本を各 source relation 分の行参照として渡す（`from_rows[relation - 1]`）。外部結合の NULL 補完側は NULL
- **スキーマ変化**: 実行時に畳み込みの全実表のスキーマを束縛時と照合する（不一致は従来どおり）
- **台帳**: 文ハッシュはトークン列だけが材料で結合結果を含まない。事前照合は source の走査より前に行う

意図的な拒否（`0A000` の固定文言。DML の束縛スコープに統合列の解決が無く、結合器の統合列と PG の優先順位の差を持ち込まないための安全弁。受け皿の Issue は無い）:

| 形 | 理由 |
| -- | ---- |
| `JOIN ... USING (...)` | 統合列（`USING` の共通列）の解決を DML の束縛スコープが持たない |
| `NATURAL JOIN` | 共通列の導出と統合列の解決を DML の束縛スコープが持たない |
| カンマの後の `CROSS JOIN` 以外の明示 JOIN | PG ではカンマより JOIN が強く結合するため束縛がずれる（SELECT の JOIN と同じ安全弁） |

派生表・関数呼び出しの FROM／USING は構文段の `42601` のまま。

## セキュリティ

- 束縛・変換の段は行を読まず書かない。実行は #3202 の dispatch 経由で結合実行器だけが行い、`0A000`（変換できない形・op id 付き・暗黙 Tx を通らない経路）では書き込みトランザクション・台帳・世代に触れない
- RLS は結合実行器が適用する。dispatch でもバイパス経路を作らない
- 修飾子の不一致は、カタログを引かない `42P01`（存在オラクルにならない）
- `search_path` ゲート（TABLE-27）: `relation_positions` が `DELETE ... USING` の表参照リストを認識し、非修飾参照のゲート素通り（fail-open）を防ぐ

## dispatch（Issue #3202）

MERGE（Issue #3350）はこの結合実行器を共有する。方式は [sql-merge.md](sql-merge.md)。

SQL の入口を結合実行器（`execute_joined_dml_in`）へ結線した。ポインタ: SQL-53 (b)・RLS-10・RECOVER-10/11・SQL-28。

- **autocommit**: `execute_parsed_in_session_inner` が `is_joined_dml` を見て、サブクエリ付き述語 DML と同じ文単位の暗黙 Tx（`execute_subquery_dml_autocommit`）へ振り分ける。実行本体は明示 Tx と共通
- **明示 Tx**: 共有 write txn 上で実行し、対象表とトリガが書いた表へ `mark_written` する（漏れると COMMIT が write txn を破棄し、遅延 FK の検査も取りこぼす）。コマンドタグは autocommit と同じ `UPDATE n`／`DELETE n`
- **暗黙 Tx を通らない経路**（ネストした実行等。`EXPLAIN ANALYZE` の autocommit は暗黙 Tx へ振り分ける）: `execute_parsed_statement` の腕は固定文言の `0A000`（fail-closed）
- **Describe**: Execute と同じ準備段（束縛・変換）を通す。実行できる形は結果列なし、変換器が拒否する形は `0A000`（op id 付きも通常の Describe と同じ）、束縛エラーは従来のコード。これで Describe と Execute のコードがそろう
- **`USING OPERATION_ID` 付き**（Issue #3203・RECOVER-10/11）: 台帳へ結線済み。構文段で検証した `operation_id` と content_hash を実行器へ渡し、FROM／USING 側の走査より前の事前照合で再送は `23505`、別文への再利用は `22023`、確定記録は書き込み本体と同じ txn で行う。束縛・変換・実行のエラーは txn が commit されないため台帳を消費しない。`parsed_operation_id` は実値を返し、同一 Tx 内の再利用は既存 DML と同じく拒否される（Tx は `Failed`）。`READ ONLY` Tx・一時表ルーティング・PL/pgSQL 本体は op id 付きを拒否側に倒す。`--require-operation-id` 構成でも使える（省略は `23502`）
- **`PARTITIONED`**: 構文段が `42601` で拒否する。`ValidatedUpdateFrom`／`ValidatedDeleteUsing` は `partitioned` を持たず、`run_partitioned_*` は述語形だけを受け取るため、結合形が分割実行器へ入る経路は構造上存在しない（autocommit・明示 Tx の両方を結合テストで固定）
- **多重一致**: 実行器の規約どおり、対象行ごとに 1 回だけ更新・削除し、UPDATE の値は走査順で最初の source 行を採る
- **まだ拒否する形**: 変換器の対象外（上記の変換器節。`0A000`）、ビュー・一時表が関与する形のうち「ビュー・一時表（Issue #3292）」節で受理した以外、データ変更 CTE の本文。いずれも第 1 弾の設計上の対象外で、fail-closed の安全弁

## ビュー・一時表（Issue #3292）

ポインタ: SQL-53 (b)・RLS-10・RECOVER-10/11・SQL-28。

| 形 | 扱い | 理由 |
| -- | ---- | ---- |
| FROM／USING 側が単純形ビュー（列別名なし） | 受理 | 基底表 1 つと述語へ畳み込める（集合演算枝のビュー展開と同じ前例） |
| FROM／USING 側が列別名つきビュー | `0A000` | 公開名から物理名への書き換えが未対応 |
| FROM／USING 側が評価後射影形ビュー（集計・LIMIT・JOIN 本文・matview） | `0A000` | インライン展開できない |
| target がビュー | `0A000` | 更新可能ビューは別の穴 |
| 一時表だけ（target・source とも一時表） | 受理 | 一時コアが自分の一時 DB の write txn で実行する |
| 一時表だけの形に `USING OPERATION_ID` | `0A000` | 一時表の書き込みは台帳の対象外。黙って捨てず拒否する |
| 永続表・ビューと一時表の混在（向きを問わない） | `0A000` | 書き込みが一時側と永続側にまたがる。振り分け・Bind の再検証が永続／一時の二択を前提にしている |

- **ビュー source の実行**: 構文段（`resolve_dml_from_relations`）が `DmlFromSource` を決め、`dml_from_exec::apply_view_source` が計画の読み取り先を基底表へ張り替え、ビュー述語を source 側述語の先頭へ畳み込む（基底スキーマで再検証）。スキーマは射影せず、束縛スコープだけを狭める
- **列のスコープ**: `BindingScope::with_visible` がビューの公開列だけを解決する。公開しない基底列は `42703`、非修飾解決の曖昧判定（`42702`）にも参加しない。`RETURNING v.*`／`*` は公開列だけを公開順に展開する。疑似列 `id` は公開列に含まれるときだけ解決する
- **RLS（P0）**: ビュー定義はテナント情報を持たないため、作成者の可視性は引き継がない。基底表は書き込むセッションの `PolicyContext` で走査され、他テナントの行はビュー経由でも結合に入らない。ビュー述語は絞り込みで、セキュリティ境界ではない
- **自己結合**: ビューの基底表が target と同じ表でもよい（source の走査は書き込みより前に完了する既存の規約）
- **権限**: `classify_write` は従来どおり target のビットだけを課し、source のビュー名はトークン走査の読み取りゲートで判定される。準備済み文の陳腐化はトークン列に現れる名前の同一性検査が覆う
- **一時表だけの形**: `is_routable` が `UpdateFrom`／`DeleteUsing` を受理し、`temp_dispatch` が一時コアの `execute_update_from_on`／`execute_delete_using_on` を呼ぶ。総バイト数の上限（`54000`）・明示 Tx・Describe は既存の一時表経路に乗る

## 非等値・OR をまたぐ述語（Issue #3295）

SQL-53 (b)・RLS-10・RECOVER-10/11・SQL-28 のポインタ。等値キーが取れない部分を、対象行 × source 行のネストループで評価する残余述語として受理する。

- **振り分け**: 最上位の `AND` の conjunct のうち、キー化できる列等値だけが結合キー。対象と source をまたぐそれ以外（非等値・`OR`・`AND`／`OR` 混在・REAL／DOUBLE／NUMERIC 等の等値・非バイト列順の照合の等値）は残余。型クラスが比較できない等値は従来どおり `42804`。キーが 0 本でも残余があれば受理する（キーも残余も無い形は交差結合として受理する）
- **束縛・評価**: 残余は「対象表のスキーマ ++ 残余が参照する source 列（合成名）」の合成スキーマ上の `WherePredicate` へ変換し、通常の単一表 WHERE と同じ `bind_where_predicates` で束縛する（`sql::join::dml_residual`）。実行は `where_tree::BoundConjunction::matches` で、ビューは「対象行の走査結果 ++ source 行のスカラー」。第 2 の評価器を作らないため、NULL の三値論理・型強制・照合・数値リテラル・`IS NULL` の真理値が通常の WHERE と一致する。source 列は照合を引き継ぐ。SELECT JOIN の `residual::eval_all` は使わない（NULL 補完行を偽にする前提が `IS NULL` と相性が悪い）
- **実行**: 候補は等値キーのバケット（キー 0 本なら source の全行）。バケットを評価する前に `charge_comparisons` で比較回数を文全体の上限へ計上する（超過は `54000`）。走査位置の昇順に評価し、残余が真になった組だけを一致ペアに数える（`max_output_rows`。一意化の前の累計）。採る source 行は真になった中の最初（決定的）。DELETE も短絡せず全候補を評価し、UPDATE と `54000` の判定を同じにする。内側ループで協調的中断（`57014`）に応じる。残余が無い形は従来経路のまま
- **列配置**: 残余が参照する source 列は走査投影の末尾（SET・キー・`RETURNING` の列の後ろ）へ追記し、SET ビューと `RETURNING` のセル位置を動かさない
- **DML 専用の葉**: `JoinWherePredicate::NumericCompare`・`IsNull`。`IN (数値, ...)` は構文段で数値比較の `OR` へ脱糖する。葉数・`IN` の要素数は積む前に検査する（`54000`）。`IsNull` は非 strict（`is_null_rejecting` は偽）で、SELECT の JOIN の文法は生成しない。多重防御として SELECT の計画器（`build_plan`）は新 variant を含む木を `0A000` で拒否する
- **fail-closed**: 残余での source 疑似列 `id`（u64 を式評価ビューへ載せない）・VECTOR／配列列の参照は固定文言の `0A000`。サブクエリ・`NOT`・`<>`・`IS TRUE` は従来どおり `42601`。RLS は変えない（対象側は所有∧可視の候補、source 側は `ctx` 走査結果だけが入力で、他テナントの Private 行はどちらにも現れない）。エラー文言は固定文言と入力された名前だけ
- **対象外**: WHERE 省略・片側だけの述語による交差結合と 3 表以上（#3294）、SET 右辺の FROM 側修飾式・行値代入（#3296）、`EXPLAIN` への残余の表示（対応しない。`EXPLAIN` が壊れないことだけを結合テストで確認）、SELECT の JOIN の WHERE／ON への数値リテラル・`IS NULL`（対応しない。外部結合の strict 簡約の見直しが要る別の判断）
- **テスト**: `sql53_dml_from_using_residual.rs`（sql_t40_up。非等値・キー＋残余・OR・三値論理・数値リテラル・決定性・自己結合・RLS・明示 Tx・台帳・上限・fail-closed・EXPLAIN）・`dml_join_exec_tests.rs`（バケット内の偽の行の飛ばし・比較回数／ペア数上限）・`dml_plan_tests.rs`・`allowlist.rs` の単体テスト・`wire_sql53_dml_from_using.rs`（末尾 `;` のタグ）

## テスト

- Issue #3292 の追加テスト: `sql53_dml_from_using_view.rs`（sql_t40_up）・`table44_temp_table_dml_from.rs`（table_rls）
- `crates/engine/tests/sql_t40_up/sql53_dml_from_using_bind.rs`: 構文・束縛・変換器の拒否表・台帳の基本
- `crates/engine/tests/sql_t40_up/sql53_dml_from_using_exec.rs`: dispatch の最小固定と RETURNING
- `crates/engine/tests/sql_t40_up/sql53_dml_from_using_multi_join.rs`（Issue #3294）: JOIN ON・多方向結合・LEFT の NULL 補完・交差結合・多重一致・上限・RLS（3 表の越境）・RETURNING・再送冪等・fail-closed の拒否・Describe の一致・`search_path` ゲート
- `crates/engine/tests/sql_t40_up/sql53_dml_from_using_acceptance.rs`（Issue #3205）: SQL 経由の受け入れ。多重一致・RLS（他テナントの件数が漏れない）・上限（`54000`・副作用ゼロ）・再送冪等・明示 Tx（COMMIT／ROLLBACK・read-your-writes・Failed）・自己結合・末尾 `;`・未対応形の fail-closed
- `crates/engine/src/core/dml_join_exec_tests.rs`: 実行器の単体テスト（走査位置 `matches` の固定を含む）
- `crates/wire-server/tests/wire_sql/wire_sql53_dml_from_using.rs`: 末尾 `;` 付き単独送信・タグ・op id
- `allowlist.rs`・`relation_name.rs` の単体テスト

## 別名つき単一表 DML（Issue #3291）

`UPDATE t [AS] x SET ... WHERE x.a = 1 RETURNING x.*`・`DELETE FROM t [AS] x WHERE x.a = 1`（FROM／USING なし）を受理する（SQL-53・RLS-10・RECOVER-10/11 のポインタ）。

- 方式: 構文解析より前に、別名なしの同じ文のトークン列へ書き換える（実体 `sql::dml_target_alias`。呼び出しは `EngineCore::normalize_row_count_clauses`。冪等）。別名トークンを除き、`<別名>.<列>` を `<列>` へ 1 対 1 で置換する。以降の経路（構文解析・名前解決・一時表の振り分け・`$n` の型推論・ビュー DML・台帳ハッシュ・RLS・分割実行）は既存の別名なし文と同一で、`parse_update`／`parse_delete` は変えない
- 解析器を変えず入口で正規化する理由: `$n` の位置・search_path 再解決の位置・ビューの WHERE 末尾など、トークン列の索引に依存する構造が複数あり、生トークンと書き換え後トークンの索引がずれる経路を作らないため。別名つき文と別名なし文は同じ AST・同じ content_hash になり、台帳の再送冪等は別名の有無を問わず同じ操作として扱われる（別の文への再利用は従来どおり `22023`）
- 照合規則は FROM／USING 形と同じ `TableRef::matches_qualifier`。別名を付けた表を本名・他表名で修飾すると `42P01`（カタログを引かないため存在のオラクルにならない）
- 受理しない（従来どおり `42601`。副作用なし）: SET 左辺の修飾（`SET x.a = ...`・`SET (x.a, x.b) = ...`）、予約語の別名、別名の直後が想定外のトークン、`x.null`・構文語と衝突する名前・WHERE の述語葉以外の `x.true`、RETURNING の項目位置以外の `x.*`
- 正規化しない経路（`42601` のまま）: `EXPLAIN`（分割実行 DML の別名形を含む。実行と `EXPLAIN ... PARTITIONED` でこの差が出る）、データ変更 CTE の本文、PL/pgSQL・埋め込み単位（`colref_sink` の記録区間では何もしない）、非セッション入口（`validate_update` 等）、サブクエリ括弧の中の外側の別名・本名への修飾相関（別名なし文の `t.c` と同じ fail-closed）
- 別名なしの経路はバイト不変（別名なし文の本名修飾 `WHERE t.a = 1` は従来どおり `42601`）

## EXPLAIN（Issue #3293）

SQL-53 (b)・SQL-75・SQL-76 のポインタ。`EXPLAIN [ANALYZE|VERBOSE|(オプション)] UPDATE ... FROM`／`DELETE ... USING` を受理する。

- **構文段**: 先頭の `EXPLAIN` を外した文を既存の結合 DML 解析器へ再帰で通し、返った検証済みの値を `ExplainDml::UpdateFrom`／`DeleteUsing` に包む（第 2 の解析器を持たない）。`$n` は素の `EXPLAIN` の WHERE のみ受理（#3297。SET の `$n` と ANALYZE・オプション前置の `$n` は既存の EXPLAIN 規則どおり `42601`）
- **静的な EXPLAIN の出力**（閉じた語彙）: 根 `update`／`delete`（`assignments`・`filter`・`returning`・`operation_id`）の下に `join`（`join_method: hash`・`join_type: inner`・`join_keys: n`・`join_filter: none`）、その下に対象表（`relation: 1`・全走査）と FROM／USING 側（`relation: 2`）の `scan`。source 側の分類は実行器と同じ `source_bound_scan` を共有分類（PK → GIN → 全走査）へ渡して作り、実経路と矛盾しない
- **見積もりは出さない**: 行数・見積もり・表名・別名・値は載せない（SQL-75 (5)・RLS-10。既存の JOIN の `EXPLAIN` と同じ契約。テナントの存在情報を漏らさないため）
- **副作用ゼロ**: 読むのはスキーマと GIN 宣言だけ。台帳・writer・世代・行には触れず、`READ ONLY` Tx でも実行できる。束縛・変換・走査前検証の順序と `wire_code` は実行と一致する
- **ANALYZE**: 既存の DML ANALYZE（#2055）と同じ契約で実際に実行する。autocommit は暗黙 Tx で実行して commit、明示 Tx は同じ Tx 上で実行（`ROLLBACK` で戻る）、`USING OPERATION_ID` は台帳に記録される。`actual_rows` は `RETURNING` の可視行数（無ければ 0）。`READ ONLY` Tx は `25006`
- **VERBOSE**: `RETURNING` の列を `output:` に出す（Describe と同じ列）。`FORMAT JSON` は既存の包みで通り、`ANALYZE` との併用は `0A000`
- **権限**: 非 EXPLAIN と同じビット（対象表 `UPDATE|SELECT`／`DELETE|SELECT`、FROM／USING 側の SELECT）。EXPLAIN は抜け道にならない
- **まだ拒否する形**: target がビューの形・一時表が関与する形（FROM／USING 側の単純形ビューは #3292 で受理済みで、EXPLAIN も同じ計画を返す）、FROM 内の JOIN ON・多方向結合（#3294。木の組み立ては source 1 個前提で、#3294 が計画を拡張するときに追随させる）、分割実行（`PARTITIONED`）、セッション無しの入口

## `$n` と公開名重複（Issue #3297）

SQL-53 (b)・RLS-10・RECOVER-10・RECOVER-11・SQL-28 のポインタ。依存・unsafe の追加なし。

- **前提**: Bind は `$n` を型付きリテラルへ置換して再解析する（`bind_prepared`）。Bind 後は同じ値をリテラルで書いた簡易クエリと同じ経路（結合 DML 解析器 → 束縛 → 変換 → 結合実行器・RLS・台帳）を通る。このため受理・拒否・`wire_code`・RLS の適用順は変えない。Parse の一律拒否（#1754）を撤去し、位置規則と型推論で受理範囲を決める。
- **受理位置**: SET 右辺（`$n` 単独・式内。型は書き込み先の代入先列／演算の相手）、WHERE の `<列|公開名.列> = $n`（`AND` 区切りの連言で、`OR`・括弧内・`BETWEEN`・非等価比較は `42601`）、`USING OPERATION_ID $n`。IN・LIKE・RETURNING・FROM 句の `$n` は従来どおり `42601`。
- **型推論**: WHERE の修飾列は修飾子（公開名）が指す表の列で引く。非修飾列は、列を持つ表がちょうど 1 つのときだけその型を使い、0 件・複数件は `Text`（束縛段が `42703`／`42702` を返す。応答コードで存在を観測させない）。整数・浮動小数列の WHERE は、数値列の `$n` を数値リテラルへ置換し、結合 WHERE の数値比較（`NumericCompare`）として受理する（等価比較は通る）。
- **SET の別名形**: `UPDATE t [AS] x SET ...` の `$n` は FROM 形に限り受理する（単一表の別名形の受理範囲は変えない）。SET 節は深さ 0 の `FROM`（`IS [NOT] DISTINCT FROM` を除く）で終わる。
- **EXPLAIN**: 素の `EXPLAIN` の WHERE の `$n` は受理し、SET の `$n` は単一表の EXPLAIN UPDATE と同じく `42601`。`EXPLAIN ANALYZE`・`EXPLAIN (オプション)` 前置は既存規則（#2054・#2058）のまま `42601`。
- **Describe 用ダミー値**: WHERE の連言分ごとに「`$n` 由来か」のフラグを作り（`sql::params::joined_where_conjunct_flags`）、`sql::join::dml_plan` が対象側・source 側の述語列へ束縛の等価序数に揃えて振り分ける。`$n` 由来のダミー値に限り DATE・UUID・ENUM 等の値検証を省く。実リテラルは常に検証し、連言分の数が構文解析後と食い違うときはフラグを使わない（fail-closed）。Execute と Bind は従来どおり空のフラグ。
- **公開名重複**: 対象表と FROM／USING の公開名（別名または表名）が重複したら `42712`（PG の `duplicate_alias`。文言は `table name "<name>" specified more than once`。内部キーはテナントの SCHEMA 名へ戻す）。新しい `SqlSurfaceError::DuplicateAlias`／`ErrorClass::DuplicateAlias` を使い、NoSQL 表層からは到達しない（射影は 400）。**SELECT の JOIN・派生表の別名重複は `42601` のまま**（[inner-join.md](inner-join.md)・[multi-relation-plan-foundation.md](multi-relation-plan-foundation.md) の判断を維持。テストで固定）。`42712` は ERR-6（エラー契約）に属する変更で、SSOT 側の定義は spec リポ側で管理し本リポでは変更しない（対応づけ: ERR-6・SQL-41・SQL-45。ポインタ表記のみ）。
- **PG 17 との照合**: PG 17 の実機は使えない環境のため、`duplicate_alias` が `42712`（"table name ... specified more than once"）であることを PG のエラーコード定義に基づいて確認した。実機での全形の照合は未検証。
- **テスト**: `crates/engine/tests/sql_t40_up/sql53_dml_from_using_params.rs`（拡張クエリと簡易クエリの一致・型推論・拒否位置・台帳・EXPLAIN・Describe・RLS・明示 Tx・公開名重複と据え置きの固定）、`crates/wire-server/tests/wire_sql/wire_sql53_dml_from_using_params.rs`（ParameterDescription の OID・タグ・`42712`）、`sql::params` の単体テスト。

## SET の行値代入と FROM 側の式（Issue #3296）

SQL-53 (b)・RLS-10・RECOVER-10・RECOVER-11・SQL-28 のポインタ。依存・unsafe の変更なし。

- **構文段**: SET 右辺の式を解析する間だけ `Parser::dml_from_set_refs` を有効にし、式中の列参照を出現順に `ColumnRef`（修飾の有無とも保持）へ積んで、式には NUL 始まりのプレースホルダ識別子（`dml_from::SET_REF_PREFIX` + 通し番号）を置く。名前ベースの書き換えをしないため、対象表修飾の参照と FROM 側の同名列を取り違えない。`RETURNING` の式項目は従来経路のまま（FROM 側の修飾は `0A000`）
- **行値代入**: `parse_update_row_assignment`（Issue #1747）の本体を、右辺要素の解析器を引数に取る `parse_row_assignment_with` へ抽出し、単一表 UPDATE と FROM 形が同じ規則（`ROW`・1 列は `ROW` 必須で `42601`・個数不一致 `42601`・部分クエリ源 `0A000`・代入数の累計上限）を共有する。要素ごとの列参照 1 個は従来の `Column`、式は `Expr`、`DEFAULT` は `Value`
- **束縛**: 参照を同じスコープで解決する。対象表の列だけの式はプレースホルダを列名へ戻して従来の式束縛へ、FROM 側を含む式は `BoundUpdateFrom::set_joined_exprs` へ載せる
- **変換**: `plan_set` が参照ごとに合成スキーマ上の名前へ写して `bind_set_expr` で束縛する（型の照合・暗黙キャスト・式非対応の代入先型の `0A000`・範囲外 `22003` は単一表 SET と同じ）。source 列の登録は `register_source_column` に集約した。source の疑似列 `id` と写像できない型は `0A000`。実行器は変更しない
- **SQLSTATE**: 列数の不一致・1 列の括弧形 `42601`、型の不整合 `42804`、部分クエリ源 `0A000`、未知の修飾子 `42P01`、曖昧な非修飾名 `42702`、未知列 `42703`
- **権限・RLS**: 変更なし。FROM 側の SELECT 権限はトークン走査の読み取りゲート、source の走査は書き込むセッションの `PolicyContext`
- **判定順**: 式の型検査は変換器が行うため、`RETURNING` の束縛エラーが式の `42804` より先に返ることがある（どちらも書き込み前の拒否）
- **まだ拒否する形**: `RETURNING` の式の中で FROM／USING 側の列を参照する形（`0A000` のまま。受け皿の整理は別途）
