# 述語つき `DELETE ... WHERE` の許可リスト・束縛設計

Issue #870・対象ビヘイビア: SQL-19（TASK-192）。関連ポインタ: SQL-18（TASK-191。
単一行・`id` 完全一致形の既存契約）・RECOVER-11（複数行変更の内容照合）・RLS-10・
EXT-3（前方一致）・SQL-9（式述語）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

> **実行結線は実装済み**（Issue #871）。詳細は `docs/design/predicate-dml-exec.md`
> 参照。以下「実行結線は Issue #871 の担当」等の記述は本 Issue（#870）着手時点の
> 申し送りとして残置する。

## スコープ

本 Issue は述語つき `DELETE ... WHERE` の**許可リスト検証と束縛**のみを実装する
（`sql::allowlist::validate_delete_statement`・`sql::parser::bind_predicate_delete`）。
実行結線（可視行列挙・1 トランザクション一括適用・`DELETE <n>` 応答・影響行数上限の
実測判定・世代カウンタ／各索引キャッシュ失効・台帳照合）は Issue #871（実装済み）
の担当。`core.rs`・`sql/exec.rs` の実行経路は本 Issue では一切変更しない。

## 構文

```
DELETE FROM <table> WHERE <predicates> USING OPERATION_ID '<id>'
```

`<predicates>` は `SELECT`／集計／広域取得（scan）が共有する
`sql::allowlist::Parser::parse_where`（等価・前方一致・`visible()`・式比較の
`AND` 結合）そのもので、第 2 の述語実装を持たない。束縛側も同じく
`sql::parser::bind_where_predicates` を共有する（`sql::parser::bind_scan` と
同一のヘルパー）。

## 単一行形との判別: AST 分類ではなくトークン先読み

既存の単一行・`id` 完全一致形（SQL-18・TASK-191・`ValidatedDelete`）の受理範囲を
1 バイトも広げないため、`WHERE` 直後を「`id = <number>` に続くトークンが文末／
`;`／文脈的キーワード `USING` のいずれか」という狭いトークン先読み
（`Parser::peek_single_row_delete_id`）で判定してから分岐する。

`parse_where` は `id = 1` と `id = 1 AND ...` を区別なく式として正規化できて
しまうため、いったん `parse_where` に通してから AST 形状で判別する方式は
採らない。先読みが外れた入力（`AND` 結合・`id` 以外の列・`=` 以外の演算子を
含むものすべて）は `Parser::parse_where` へ委譲し、述語形
（`ValidatedPredicateDelete`）として扱う。

`validate_delete`（既存の単一行形専用入口）は、述語形に分類された入力を
`mode.require`・カタログ照会より**前**の構造判定の時点で `42601` を返す。これに
より、Issue #870 追加後も `validate_delete` のエラー優先順位契約（述語形は
`operation_id` の有無・テーブルの実在によらず常に `42601`）を保存する。

## エントリポイント: 追加のみ

| 入口 | 受理範囲 |
| ---- | -------- |
| `validate_delete`（既存・SQL-18） | 単一行・`id` 完全一致形のみ。挙動は本 Issue で無変更 |
| `validate_delete_statement`（新規・SQL-19） | 単一行形・述語形の両方（`DeleteStatement::SingleRow`／`Predicate`） |

`core.rs` の dispatch（#867・単一行／#871・述語形）は `validate_delete_statement`
（トークン列版 `validate_delete_statement_tokens`）を使う。両入口は構文解析
（`Parser::parse_delete` ＋ `expect_end_of_statement`）を private ヘルパー
`parse_delete_statement_shape` として共有し、複製しない。

## 受理・拒否の一覧（構造段）

| 入力 | 分類 | 結果 |
| ---- | ---- | ---- |
| `WHERE id = 1` | 単一行形 | `DeleteStatement::SingleRow` |
| `WHERE lang = 'ja'` | 述語形 | `DeleteStatement::Predicate`（`Equality`） |
| `WHERE path LIKE 'src/%'` | 述語形 | `DeleteStatement::Predicate`（`Prefix`） |
| `WHERE id > 5` | 述語形 | `DeleteStatement::Predicate`（`Expression`） |
| `WHERE id = 1 AND lang = 'ja'` | 述語形 | `DeleteStatement::Predicate`（宣言順 2 件） |
| `WHERE visible()` | 述語形 | `DeleteStatement::Predicate`（`PredicateCall`。受理して無視。下記参照） |
| `WHERE` 省略 | — | `42601`（全行削除の意図は `TRUNCATE TABLE` の管轄） |
| `... HINT ORDER(...)` / `ORDER BY ...` / `LIMIT ...` / `USING MODE ...` | — | `42601`（許可形状外の余剰トークン） |
| `... RETURNING ...`（述語形） | — | `42601`（構造上は受理するが、実行結線〔#871〕が未着手のため `validate_delete_statement_tokens` の `Predicate` 腕が単一のチョークポイントで拒否する。単一行形は実行結線済みのため受理する——Issue #873・SQL-21・`docs/design/sql-returning.md` 参照） |
| `WHERE ... OR ...` | — | `42601`（`OR` 非対応） |
| `EXPLAIN DELETE ...` | — | `42601`（`EXPLAIN` は `SELECT` 系専用） |

## 記録する判断

- **恒真述語**（例: `WHERE 1 = 1`）は `SELECT`／scan と同じく構造上は受理する。
  列参照を持たない述語を特別に拒否する分岐は追加しない（`SELECT`・scan・#869 の
  `UPDATE` と受理範囲を揃える）。全行削除に対する歯止めは `WHERE` 省略の
  `42601` と、実行結線（#871）が導入する影響行数上限の 2 点で構成する。
- **`visible()`**: `bind_where_predicates` は `rls_predicate_present` フラグを
  立てるのみで、フィルタとしては何も生成しない（scan／aggregate と同じ挙動）。
  述語に `visible()` を含めても RLS 暗黙適用を解除・緩和できない。
- **`id` 実列を持つテーブル**: 単一行形の先読みは疑似列 `id` として扱う（SQL-18
  と同じ）。述語形の式内 `id` は `bind_expr` が実列を優先する既存契約に従う
  （`sql::udf_call::bind_expr` の列名解決の既存挙動。本 Issue で変更しない）。

## 影響行数上限（実行結線への引き継ぎ）

許可リスト・束縛層では対象行数を知り得ないため、契約の**器**のみを本 Issue で
用意した（**Issue #997 で以下の別系統は削除し、`UPDATE` 側と共有する唯一の
上限 API へ統合済み**。詳細・既定値の確定は
`docs/design/predicate-dml-exec.md` §6 参照）:

- ~~`sql::parser::DEFAULT_MAX_DML_AFFECTED_ROWS`~~（削除。`1_000` という値自体は
  `sql::parser::MAX_DML_AFFECTED_ROWS` に引き継がれている）
- ~~`sql::parser::check_affected_row_count(count, limit)`~~（削除。
  `sql::parser::check_dml_affected_rows(count)` に統合）
- ~~`BoundPredicateDelete::max_affected_rows()`~~（削除。実行結線が
  `MAX_DML_AFFECTED_ROWS` を直接参照するため運搬値を廃止）

実際の判定（候補行を数え終えた直後・書き込み開始前・副作用ゼロの時点で
`check_dml_affected_rows` を呼ぶこと）は Issue #871 の担当。

## RECOVER-11: 内容照合の正規化情報源

`ValidatedPredicateDelete::where_predicates` は `AND` 結合の宣言順を保持し
**並べ替えない**（`ValidatedUpdate::assignments` と同じ判断）。複数行変更の
`operation_id` 内容照合ハッシュ（Issue #868 の担当）が「正規化した文」を入力と
する場合、その情報源はこの宣言順そのものになる。ハッシュ入力レイアウト・
実行時の記録順序・原子性契約は `docs/design/multi-row-dml-operation-id.md`
（Issue #868・ステータス Proposed）に記載。

## 対象外（本 Issue の範囲外）

- 実行結線一式（可視行列挙・1 トランザクション一括適用・`DELETE <n>` 応答・
  影響行数上限の実測判定・世代カウンタ／索引キャッシュ失効・台帳照合）: Issue #871
- 複数行変更の `operation_id` 内容照合ハッシュの入力仕様: Issue #868
- `core.rs::execute_sql_in_session` への `validate_delete_statement_tokens` dispatch:
  Issue #867（単一行）・#871（述語形）
- NoSQL 表層 `delete` op（`BoundPredicateDelete::new` の利用）: Issue #875・#876
- `EXPLAIN DELETE`・`OR`／括弧付き述語（SQL-24 拡張述語）は本 Issue では
  `42601` のまま。`RETURNING`（Issue #873・SQL-21）は述語形の実行結線
  （#871）が未着手のため引き続き `42601`（チョークポイントの実装自体は
  `sql::allowlist::validate_delete_statement_tokens` に追加済み。上記
  「受理・拒否の一覧」参照）
