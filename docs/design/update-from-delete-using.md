# UPDATE ... FROM／DELETE ... USING の parser と束縛

Issue #1754（親 #1753）。ビヘイビア SQL-53 (b) のポインタ。関連: RLS-10 (b)・SQL-28（JOIN）・SQL-36（WHERE 省略）・RECOVER-11。
spec 本文は転記せず、ID のポインタのみを記す。

## 範囲

本 Issue は **構文と束縛だけ**を担当する。束縛まで通った文は、実行腕が `0A000` で止める（副作用ゼロのスタブ）。

| 担当 | 内容 |
| ---- | ---- |
| #1754（本書） | 構文の解析、表の存在確認、列参照の解決、SET 右辺の型照合 |
| #1755 | 結合の実行、多重一致時の 1 回だけの更新、影響行数上限 |
| #1756 | 全参照への RLS 適用、`RETURNING` |
| #1757 | 台帳の content_hash と operation_id の再送冪等 |

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
- `PARTITIONED` 併用は `42601`、`RETURNING` 併用は `0A000`（#1756）、`$n` は拡張クエリの Parse 段で `42601`
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

- `parsed_operation_id` は `None` を返す。拒否される文の operation_id を `SessionTransaction` に登録しないため。#1755／#1757 で見直す
- 一時表が関与する形は `is_routable` が `false` を返し `0A000`。ビューが関与する形は `0A000`

## 束縛（`sql::dml_from`）

対象表と FROM／USING 側の表を 1 つの `BindingScope` にまとめ、純関数で次を行う。カタログ照会・行の読み取り・RLS・台帳には触れない。

1. スコープ構築（参照数 `54000`・公開名の重複 `42601`）
2. `WHERE` の全列参照を解決（修飾子の不一致 `42P01`・非修飾で複数表に該当 `42702`・未知列 `42703`）
3. SET 左辺の検査（重複・疑似列・未知列・`GENERATED ALWAYS`）
4. SET 右辺: 列参照は解決と代入可否の判定（不一致は `42804`）、式は非修飾の列名をスコープ全体で照合（対象表と FROM 側の両方に該当で `42702`、FROM 側だけなら `0A000`）

結合条件の抽出・プッシュダウン・実行計画は #1755 の担当。

## セキュリティ

- 行を一切読まず書かない。実行腕は束縛後に必ず `0A000` で止まり、書き込みトランザクション・台帳・世代に触れない
- RLS は #1755／#1756 が JOIN 実行器を経由して適用する。本 Issue ではバイパス経路を作らない
- 修飾子の不一致は、カタログを引かない `42P01`（存在オラクルにならない）
- `search_path` ゲート（TABLE-27）: `relation_positions` が `DELETE ... USING` の表参照リストを認識し、非修飾参照のゲート素通り（fail-open）を防ぐ

## テスト

`crates/engine/tests/sql53_dml_from_using_bind.rs`（結合）と、`allowlist.rs`・`relation_name.rs` の単体テスト。
