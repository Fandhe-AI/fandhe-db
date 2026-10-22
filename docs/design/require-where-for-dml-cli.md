# opt-in の `--require-where-for-dml`（ポインタ）

## ステータス・対象

- ステータス: 実装済み（Issue #2210）
- ビヘイビア ID: SQL-36（`docs/spec/04-behavior/` のポインタのみ。本文は転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連: [delete-predicate-form.md](delete-predicate-form.md)（`WHERE` 省略の受理。Issue #1743）・[operation-id-optional.md](operation-id-optional.md)（同型の起動時スイッチ `--require-operation-id`）

## 概要

SQL 表層は PostgreSQL と同じく `WHERE` を省略した `UPDATE`／`DELETE` を受理する（Issue #1743）。
全行の更新・削除に対する安全弁として、起動時 opt-in のスイッチ `--require-where-for-dml`
（既定 off。MySQL の `sql_safe_updates` 相当）を設ける。

| 項目 | 内容 |
| ---- | ---- |
| 指定方法 | `wire-server` の値なしスイッチ。重複指定・`=` 連結形・後続の値は起動エラー（fail-closed） |
| 有効時の挙動 | `WHERE` 句を書いていない SQL `UPDATE`／`DELETE` を、変更開始前に固定文言・SQLSTATE `42601` で拒否する |
| 影響しないもの | 明示した述語（`WHERE TRUE` を含む）・NoSQL（HTTP）表層（filter 必須のまま）・`TRUNCATE` |
| 変更経路 | プロセス起動時のみ。セッション・SQL・HTTP リクエストからは変更できない |
| 起動ログ | 指定時だけ英語の固定文言を 1 行（`listening on` より前）。未指定時の出力は不変 |

## 実装の要点

- 注入点: `EngineCore::with_require_where_for_dml(bool)`（所有権消費ビルダー。`with_ledger_mode` と同型）。
  `wire-server` の `require_where_for_dml_opt::resolve` に配線点を一本化し、`main.rs` が `Arc` 化前に 1 回だけ呼ぶ
- 判定: `sql::allowlist::reject_omitted_where_for_dml`。`WHERE` 省略は空の述語列で表され、明示した `WHERE` は
  必ず 1 件以上の述語列になる（`mark_explicit_where`）ため、空列の判定だけで省略を一意に識別できる
- 判定位置: `EngineCore::parse_tokens_in` の DELETE／UPDATE 分岐で構造検証の直後・表の存在確認より前、
  および `EXPLAIN` の DML 経路。次の経路は 1 か所の判定で覆われる
  - 基表・ビュー経由（ビュー述語との合成より前に、利用者が書いた文で判定する）
  - `PARTITIONED` 付き・拡張クエリ（Parse 時）・一時表（分類 parse が永続コアの同じ経路を通る）
- 判定順序: 構造検証 → 分割実行の `operation_id` 必須（`23502`）→ `operation_id` 必須化（`23502`）→ 本判定（`42601`）→
  表・ビューの存在確認。本判定は表の有無によらず返るため、`42601` と `42P01` の差から存在を推測できない。
  `--require-operation-id` と併用して `operation_id` を省略した `WHERE` 省略文は `23502` が先に返る
- 拒否は parse 段で行うため、台帳への記録・`operation_id` の消費は起きない。拒否された文と同じ ID で
  `WHERE` 付きの文を後から実行できる

## 後続・関連

- 恒真述語 `WHERE TRUE` 等の受理は #1744 の範囲。本スイッチは明示した述語を拒否しないため、その受理状況に依存しない
- 回帰テスト: `crates/engine/tests/sql36_require_where_for_dml.rs`・`crates/wire-server/tests/wire_require_where_for_dml_cli.rs`
