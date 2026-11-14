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
UPDATE <表> [[AS] <別名>] SET <列> = <右辺>[, ...] FROM <rel>[, <rel>]* [WHERE <join_where>]
       [USING OPERATION_ID '<id>' | NULL]
DELETE FROM <表> [[AS] <別名>] USING <rel>[, <rel>]* [WHERE <join_where>]
       [USING OPERATION_ID '<id>' | NULL]
<rel> := <表> [[AS] <別名>]
```

- `WHERE` は JOIN の `WHERE` 文法（`JoinWhereExpr`）を再利用する。列同士の比較・`AND`／`OR`・括弧を受理し、数値リテラル・`IS NULL`・`NOT`・サブクエリは構造的に `42601`
  になる。#1750 のサブクエリ形と FROM 形は当面同時に使えない
- SET 左辺は非修飾の列名だけ（`SET t.a = ...` は `42601`）。行値の代入 `(a, b) = (...)` は `0A000`
- SET 右辺は 2 系統に分ける
  - 列参照 1 個（`DmlFromSetValue::Column`）: 修飾あり・なしとも FROM 側の列を指せる。直後が句境界（`,`・`FROM`・`WHERE`・`USING`・`RETURNING`・文末）のときだけ
  - それ以外（`DmlFromSetValue::Value`）: 既存の `SetValue`。式中の修飾列参照は、対象表の公開名で修飾されたものだけを非修飾へ正規化し、FROM 側を指す修飾は `0A000`
- 参照数（対象表を含む）は `MAX_TABLE_REFS` で積む前に検査する（超過は `54000`）。公開名の重複は `42601`（`BindingScope::new` と同じ契約）。別名で公開名を分ければ自己結合は受理する
- `FROM` 内の `JOIN ... ON`・派生表・関数呼び出しは `42601`
- `PARTITIONED` 併用は `42601`、`RETURNING` は `*`・`<公開名>.*`・列参照を受理する（#3204。式・関数・`AS` 別名は `42601`。束縛・実行は [sql-returning.md](sql-returning.md) の「結線」節）、`$n` は拡張クエリの Parse 段で `42601`
- `--require-where-for-dml` は FROM／USING 形でも `WHERE` 省略を `42601` で拒否する（opt-in のときだけ）

## 既存解析器との振り分け

- 入口（`EngineCore::parse_tokens_in`）が `looks_like_update_from`／`looks_like_delete_using` で先読みし、専用の解析器へ回す。既存の `parse_update`／`parse_delete` は変えない
- `looks_like_update_from`: 括弧深さ 0 の `FROM`（`IS [NOT] DISTINCT FROM` を除く）がある `UPDATE` 文
- `looks_like_delete_using`: 括弧深さ 0 の `USING` の直後が `OPERATION_ID` でない `DELETE FROM` 文。`DELETE FROM t USING OPERATION_ID '...'`（WHERE 省略形）は従来どおり既存の経路を通る。`operation_id` という名前の表を USING に書く形は operation_id 句として扱われ、既存の解析器が `42601` で拒否する（fail-closed）
- 非セッション入口（`validate_update`・`validate_delete_statement` など）と `EXPLAIN` は FROM／USING 形を従来どおり `42601` で拒否する
- 別名付きで FROM／USING が無い単一表 DML（`UPDATE t AS x SET ... WHERE ...`）は従来どおり `42601`（受理範囲を広げない）

## 新しい `ParsedSql` variant

`ParsedSql::UpdateFrom`／`ParsedSql::DeleteUsing` を追加した（BREAKING CHANGE）。既存の `ValidatedPredicateUpdate`／`ValidatedPredicateDelete` に `from` を足すと、既存の実行腕が
FROM を黙って無視する（単一表として処理する）fail-open 経路ができるため、別 variant にして網羅 `match` の追随漏れをコンパイラに検出させる。

- `parsed_operation_id` は `None` を返す。拒否される文の operation_id を `SessionTransaction` に登録しないため。SQL 経路の dispatch 結線時に見直す（#1755・#1757 は実行入口と台帳ハッシュまで）
- 一時表が関与する形は `is_routable` が `false` を返し `0A000`。ビューが関与する形は `0A000`

## 束縛（`sql::dml_from`）

対象表と FROM／USING 側の表を 1 つの `BindingScope` にまとめ、純関数で次を行う。カタログ照会・行の読み取り・RLS・台帳には触れない。

1. スコープ構築（参照数 `54000`・公開名の重複 `42601`）
2. `WHERE` の全列参照を解決（修飾子の不一致 `42P01`・非修飾で複数表に該当 `42702`・未知列 `42703`）
3. SET 左辺の検査（重複・疑似列・未知列・`GENERATED ALWAYS`）
4. SET 右辺: 列参照は解決と代入可否の判定（不一致は `42804`）、式は非修飾の列名をスコープ全体で照合（対象表と FROM 側の両方に該当で `42702`、FROM 側だけなら `0A000`）

結合条件の抽出・プッシュダウン・実行計画は #1755 の担当。

## 変換器（Issue #3201。`sql::join::dml_plan`）

束縛結果（`BoundUpdateFrom`／`BoundDeleteUsing`）を結合 DML の実行入力（`JoinedDmlPlan`）へ変換する純関数。行・RLS・台帳・書き込みトランザクションには触れない。SQL 経路の dispatch は #3202（`core::dml_from_exec::plan_*_stmt` が入口。下記「dispatch」節）。

WHERE は最上位の `AND` で conjunct に平坦化し、解決済みの relation 添字だけで（表名では判定せず）次のとおり全域に振り分ける。

| conjunct | 行き先 |
| -------- | ------ |
| 別 relation の列同士の等値 | 結合キー（対象側・source 側へ向きを正規化。複合キー可） |
| 対象側だけを参照（`OR` 可） | 対象側フィルタ（既存の述語形 DML と同じ束縛器） |
| source 側だけを参照（`OR` 可） | source の走査へ押し下げる述語 |
| 上記以外 | `0A000` |

`0A000` で拒否する形（第 1 弾の設計上の対象外・fail-closed の安全弁。取りこぼした述語は書き込み範囲を広げるため、未分類は必ず拒否する）: WHERE 省略・等値結合キー無し、relation をまたぐ非等値・`OR`、同じ relation 内の列比較、FROM／USING が 2 表以上、キー化できない型（REAL／DOUBLE／NUMERIC 等）の結合キー、非バイト列順の照合つきキー列、embedding を参照する対象側述語、SET 右辺の source 疑似列 `id`・結合ビューへ写像できない型の source 列。キー列の型クラスが比較できない組は `42804`。

SET は、リテラルと対象表の列だけを使う式をそのまま運び、列参照（source 列を含む）を「対象列 ++ source の取り出し列」の合成スキーマ上の式として束縛する（合成名は利用者の識別子と衝突しない綴り）。Describe 経路で変換エラーを返すかどうかは #3202 の方針の範囲。

## セキュリティ

- 束縛・変換の段は行を読まず書かない。実行は #3202 の dispatch 経由で結合実行器だけが行い、`0A000`（変換できない形・op id 付き・暗黙 Tx を通らない経路）では書き込みトランザクション・台帳・世代に触れない
- RLS は結合実行器が適用する。dispatch でもバイパス経路を作らない
- 修飾子の不一致は、カタログを引かない `42P01`（存在オラクルにならない）
- `search_path` ゲート（TABLE-27）: `relation_positions` が `DELETE ... USING` の表参照リストを認識し、非修飾参照のゲート素通り（fail-open）を防ぐ

## dispatch（Issue #3202）

SQL の入口を結合実行器（`execute_joined_dml_in`）へ結線した。ポインタ: SQL-53 (b)・RLS-10・RECOVER-10/11・SQL-28。

- **autocommit**: `execute_parsed_in_session_inner` が `is_joined_dml` を見て、サブクエリ付き述語 DML と同じ文単位の暗黙 Tx（`execute_subquery_dml_autocommit`）へ振り分ける。実行本体は明示 Tx と共通
- **明示 Tx**: 共有 write txn 上で実行し、対象表とトリガが書いた表へ `mark_written` する（漏れると COMMIT が write txn を破棄し、遅延 FK の検査も取りこぼす）。コマンドタグは autocommit と同じ `UPDATE n`／`DELETE n`
- **暗黙 Tx を通らない経路**（`EXPLAIN` 等のネスト）: `execute_parsed_statement` の腕は固定文言の `0A000`（fail-closed）
- **Describe**: Execute と同じ準備段（束縛・変換）を通す。実行できる形は結果列なし、変換器が拒否する形は `0A000`（op id 付きも通常の Describe と同じ）、束縛エラーは従来のコード。これで Describe と Execute のコードがそろう
- **`USING OPERATION_ID` 付き**（Issue #3203・RECOVER-10/11）: 台帳へ結線済み。構文段で検証した `operation_id` と content_hash を実行器へ渡し、FROM／USING 側の走査より前の事前照合で再送は `23505`、別文への再利用は `22023`、確定記録は書き込み本体と同じ txn で行う。束縛・変換・実行のエラーは txn が commit されないため台帳を消費しない。`parsed_operation_id` は実値を返し、同一 Tx 内の再利用は既存 DML と同じく拒否される（Tx は `Failed`）。`READ ONLY` Tx・一時表ルーティング・PL/pgSQL 本体は op id 付きを拒否側に倒す。`--require-operation-id` 構成でも使える（省略は `23502`）
- **`PARTITIONED`**: 構文段が `42601` で拒否する。`ValidatedUpdateFrom`／`ValidatedDeleteUsing` は `partitioned` を持たず、`run_partitioned_*` は述語形だけを受け取るため、結合形が分割実行器へ入る経路は構造上存在しない（autocommit・明示 Tx の両方を結合テストで固定）
- **多重一致**: 実行器の規約どおり、対象行ごとに 1 回だけ更新・削除し、UPDATE の値は走査順で最初の source 行を採る
- **まだ拒否する形**: 変換器の対象外（上記の変換器節。`0A000`）、ビュー・一時表が関与する形、データ変更 CTE の本文と `EXPLAIN`。いずれも第 1 弾の設計上の対象外で、fail-closed の安全弁

## テスト

`crates/engine/tests/sql53_dml_from_using_bind.rs`（結合）と、`allowlist.rs`・`relation_name.rs` の単体テスト。
