# Rust コーディング規約

## ツールチェーン

- `rust-toolchain.toml`（stable・rustfmt・clippy）を単一真実源とする
- `cargo fmt`・`cargo clippy --workspace --all-targets -- -D warnings` を通してからコミットする

## エラーハンドリング

- ライブラリコード（engine）では `Result` を返し、panic させない
- **受信データ（wire プロトコル入力）経路では `unwrap` / `expect` / 添字アクセス（`[]`）を禁止**する（AGENTS.md P0）。`get()`・`try_into()`・checked 演算で明示的に処理する
- エラー契約は fail-closed とする。曖昧な場合は拒否側に倒し、エラー応答に他テナントのデータ・存在情報を含めない
- エラー型は SQLSTATE 風 `wire_code` の設計に従う

## untrusted 入力の扱い

- 長さフィールドは上限検証してからアロケーションに使う（無制限 `Vec::with_capacity` 禁止）
- 整数演算は `checked_*` / `saturating_*` を使い、オーバーフローを未定義動作にしない
- SQL / プラン文字列（`USING PLAN(...)` 含む）の組み立てに未検証入力を連結しない

## 設計

- workspace 構成: `engine`（コアロジック）＋ `wire-server`（バイナリ）を維持し、責務境界を跨ぐ依存を作らない
- `unsafe` は原則禁止。必要な場合は理由・不変条件をコメントで明記しユーザー承認を得る
- 依存は最小限。追加・更新は必ずユーザー承認＋ `=x.y.z` 完全固定（[dependency-policy](./dependency-policy.md)）

## テスト

- 挙動は `docs/spec` のビヘイビア定義（ID 参照）に対応づけてテストする
- テストの skip・ignore・アサーション弱体化で CI を通さない
- engine の結合テストは `tests/<束ね先>/main.rs` で少数のバイナリへ束ねている。プロセス共有状態・環境変数・時間計測・fault-injection に依存するテストは束ねず `tests/` 直下に単独で置く（基準と個別実行方法は README「engine の結合テストの束ね方と個別実行」）

## コメント

- [code-comment-style](./code-comment-style.md) に従う（Rust は `///` / `//!` のドキュメンテーションコメント）
