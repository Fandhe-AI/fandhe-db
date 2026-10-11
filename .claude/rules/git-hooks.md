---
paths:
  - "lefthook.yml"
---

# Git hooks（lefthook）編集ルール

## 目的

フックはコミットのたびに実行されるため、遅いフックはそのまま開発速度の低下になる。
フックはローカルでの早期検知専用であり、CI の代替ではない。**開発速度を落とさないことを最優先**し、
重い検査・網羅的な検査は CI（`.github/workflows/ci.yml`）に任せる。

フックで強制しないだけで、ローカルの品質規約（[coding-rust](./coding-rust.md) の `cargo fmt`・
`cargo clippy` を通してからコミットする等）自体は変わらない。push 前に CI 相当を手元で確かめたい場合は
任意で以下を実行する。

| コマンド | 内容 |
| -------- | ---- |
| `make ci` | CI（`ci.yml`）と同等のチェックを一括実行（C++17 コンパイラ必須。CLAUDE.md の Conventions 参照） |
| `make fmt-check` | `cargo fmt --check` のみ |
| `make lint` | `cargo clippy -D warnings` のみ |
| `make lint-docs` | markdownlint・yamllint・editorconfig-checker・commitlint |

## 時間予算

| フック | 予算 | 判定方法 |
| ------ | ---- | -------- |
| pre-commit | 通常のコミット（staged のみ）で全ジョブ合計 1 秒未満 | 実コミット時の `summary: (done in N seconds)`。最悪ケースとして `lefthook run pre-commit --all-files` も計測する |
| commit-msg | 即時（外部コマンド・ネットワーク不可） | 実コミット時に表示される commit-msg の summary（メッセージファイルを引数に取るため `--all-files` では計測しない） |
| pre-push・post-checkout・post-merge 等 | 原則追加しない | 追加する場合は理由と実測値を PR 本文に記載する |

`--all-files` は全追跡ファイル（約 3,000 件・`.rs` 約 700 件）を対象にする最悪ケースで、
実際のコミットはこれより大幅に速い。`--all-files` の値は下記「明示的な例外」の rustfmt-check が
支配的になるため、新しいジョブの追加時はジョブ別の所要時間（summary 下の各行）で判断する。

## フックに置かないもの（→ `ci.yml` へ）

- テストスイート・ビルド・clippy（`rust-ci` の `cargo test`・`cargo clippy`、crash-test 系・`test-default-build` 等の各ジョブ）
- 依存監査（`rust-ci` の `cargo deny check`）
- リポジトリ全体を走査する lint（`lint-docs` の markdownlint・yamllint・editorconfig-checker。`make ci`・`make lint-docs` 全体の呼び出しを含む）
- 依存パッケージのインストール・ネットワークアクセス（`npx`・`uvx` 経由のツール取得を含む）
- `ci.yml` のジョブと同じコマンドを同じ範囲で実行するもの

## フックに置いてよいもの

- `{staged_files}` に限定し、`glob` / `exclude` で対象を絞った 1 秒未満の検査
- CI ではコミット単位で検知できない、または push 後では遅いもの
  - secrets-guard: 資格情報ファイルはリモートへ送る前に止める必要がある（CI に同等の検査は無い）
  - conventional-commits（commit-msg）: CI の commitlint（`lint-docs`）は `pull_request` でのみ実行され、
    push 済みの中間コミットは PR refs に残るため、作成時点で形式を揃える

### 明示的な例外

- pre-commit の rustfmt-check は CI の `rust-ci` の `cargo fmt --check`（`make fmt-check`）と重複するが、
  staged の `.rs` に限定しており通常のコミットでは十分速いため維持する（2026-10-11 計測・決定）
  - 計測値: 一般的なファイル 5 件で 0.03 秒、`.rs` 27 件のコミットで 0.03 秒。
    rustfmt は `mod` 宣言を辿るため crate ルートを含むと伸び、`crates/engine/src/lib.rs` 単体で約 1.2 秒
  - `--all-files`（`.rs` 約 700 件）では約 7.3 秒かかるが、全ファイルを staged にするコミットは想定しない
  - Claude の編集は PostToolUse フック（`.claude/settings.json`）で整形済みのため、本ジョブは主に手編集の検知用

## 書き方の規約

- `pre-commit` の `parallel: true` を維持する（直列化しない）
- `{staged_files}` と `glob` / `exclude` で対象を絞る
- staged ファイル数・diff 行数に比例してプロセスを起動するループ（ファイルごとに `grep` を起動する等）は
  書かない。全件を 1 回のパイプラインで判定する（secrets-guard はこの形に置き換えて
  `--all-files` で約 11.8 秒 → 0.03 秒になった。2026-10-11 計測）
- 検査の高速化・書き換えでは検出範囲と fail-closed（判定できない場合は拒否する側に倒す）を維持する
- cargo 系ジョブは `Cargo.toml`・ツールの有無を判定してスキップできるようにする（既存の rustfmt-check と同方針）
- 条件付き実行が必要な場合は lefthook の `skip` / `only` を使ってよい
  （`.claude/skills/lefthook/references/examples/skip.md`）

## 検査を追加するときの判断手順

1. その検査は `ci.yml` で既に実施されているか
    - 実施済みで、staged 限定にしても 1 秒以上かかる → フックに入れない
2. staged ファイルに限定できるか
    - できない → CI へ置く
3. CI へ置く場合は `ci.yml` にジョブを追加する
    - 既存ジョブと同様、第三者 action は SHA 固定で参照する（AGENTS.md「CI・ワークフローの改変（P1）」）
    - 必須チェック（branch protection）への登録はユーザー作業のため、PR 本文に明記する
4. 編集後に以下を実行し、ジョブ別の所要時間と summary をユーザーへ報告する
   （staged 想定で 1 秒以上かかるなら構成を見直す）

```bash
time lefthook run pre-commit --all-files
```

## バイパス禁止

遅いからといって `--no-verify` や `LEFTHOOK=0` で回避しない。遅いフックはフック側を直す
（検査を CI へ移す・staged 限定にする・対象 glob を絞る）。
