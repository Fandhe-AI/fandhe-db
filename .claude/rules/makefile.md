---
paths:
  - "Makefile"
  - "scripts/**"
---

# Makefile / scripts 編集ルール

## 目的

`Makefile` は人・CI・Claude が同じコマンドを叩くための**薄い入口**であり、処理の定義は `scripts/` に置く。
make skill（`.claude/skills/make`）の方針に従い、Makefile に処理を二重定義しない。
`scripts/hooks/**` は存在する場合も [git-hooks](./git-hooks.md) 側の担当とする（本ルールは lefthook 以外の `scripts/` を扱う）。

## 構成の原則

- 各レシピは `@scripts/<name>.sh [args]` または言語ネイティブの 1 行コマンド（`cargo test ...` 等）のみ。
  複数行・`if`/`for`・環境変数の検証・`curl`・`docker` などの処理は `scripts/` の bash スクリプトへ置く
- Make 変数は `make X VAR=...` で渡された値が環境変数として子プロセスへ届く。必須変数の検証はスクリプト側で
  `: "${VAR:?...}"` により行い、Makefile に `if [ -z ... ]` を書かない
- ツールの固定バージョン（lint ツール・lefthook・cargo-deny）は `scripts/lib.sh` に集約する。
  `CARGO_DENY_VERSION` は `Dockerfile` の先行導入と同期させる
- 中身の無いターゲット（`@true`・`@echo "done"` のみ）は置かない。呼び出し元が終了コードを信用するため
- GNU Make 3.81（macOS 標準）互換を保つ。`.ONESHELL`・`.RECIPEPREFIX` 等 3.82+ 専用機能は使わない
- 詳細な説明は各ターゲット直前の `# ` コメントに書き、`## ` は `make help` に出る 1 行（約 90 文字以内）にする

## ターゲットを追加・改名・削除するとき

1. `.PHONY: <name>` をターゲット直前に宣言する
2. `<name>: ## 1 行説明` の形で `## ` コメントを付ける（`make help` に出る。付けないと一覧に載らない）
3. `scripts/help.sh` に同じ変更を反映する（Make を介さない直接コマンドの一覧）。
   `scripts/tests/test_help_sync.py`（`make scripts-test`・CI の `scripts-test` ジョブ）が両者の不一致を検知する
4. 呼び出し元を洗い出して同じ PR で更新する

    ```bash
    grep -rn "make " .github .claude docs README* CLAUDE.md AGENTS.md lefthook.yml Dockerfile compose.yaml scripts
    ```

   既存のターゲット名・引数・環境変数・終了コードは `ci.yml`・`recall.yml`・`bench.yml`・`detect-features.yml`・
   README・`docs/design` が参照している。改名・削除は参照元の更新が前提
5. `make help` と `make -n <target>` で確認する。CI に影響するターゲットは `ci.yml` のジョブ名と対応を保つ

## 新規スクリプトの規約

- 先頭は `#!/usr/bin/env bash`、直後に役割・呼び出し元・使い方のコメント（日本語）、`set -euo pipefail`
- Makefile 経由と直接実行で同じ結果になるよう、リポジトリルートに依存する処理は `scripts/lib.sh` の `ROOT_DIR` を使う
  （`source "${DIR}/lib.sh"`。`.shellcheckrc` が `source-path=SCRIPTDIR` を設定済み）
- `shellcheck` を通す。実行権限（`chmod +x`）を付ける。インデントは 4 スペース（`.editorconfig`）
- エラー・ログなどプログラム出力文字列は英語（[japanese-style](./japanese-style.md)）
- 引数を取らないスクリプトは未知の引数を黙って無視しない（`scripts/help.sh` は使用法を出して終了コード 2）
- 既存の `bench_*.sh`・`check_*.sh` は自前の骨格を持つ。既存の挙動（終了コード・環境変数名）を変えない

## ターゲットの契約（本リポジトリの実在ターゲットに即す）

| 種別 | ターゲット | 契約 |
| ---- | ---------- | ---- |
| 検査（ソース不変） | `fmt-check`・`lint`・`test`・`lint-docs`・`core-api-check`・`sort-determinism-check`・`simd-codegen-check`・`deny`・`scripts-test` | ソース・設定を変更しない（ビルド成果物・キャッシュは可）。失敗は非 0 終了 |
| 修正 | `fmt` | 整形のみ。それ以外の自動修正をここに足さない |
| 環境構築 | `setup`・`rustup`・`submodule`・`hooks` | 再実行しても安全。`docs/spec` は private のため取得失敗は警告のみで成功終了（ビルドは spec 抜きで成立させる） |
| 集約 | `ci` | `ci.yml` と同等のチェックを一括実行する。`ci.yml` にジョブを足したら `ci` の依存にも足す（逆も同様） |
| 手動専用 | `bench-*`・`*-regression`・`e2e-three-client*`・`precision-report` ほか | 時間依存・環境依存のため `ci` に含めない。`GITHUB_ACTIONS` 下で拒否する既存の fail-closed を弱めない |
| コンテナ | `docker-build`・`docker-shell`・`docker-ci` | `compose.yaml` 経由。ホストの共有ディレクトリを消さない |

- `clean` に相当するターゲットは現状無い。追加する場合は当該リポジトリ作業ツリーの再生成可能な成果物のみを消し、
  他の git worktree・共有 `target/`・`.env`・認証情報には触れない
- 検査系は黙ってスキップしない。前提ツールが無い場合は導入方法を案内して失敗する（`lint-yaml` と同方針）。
  例外は意図して明示した `skip:` 出力（`lint-commits` の `origin/main` 未取得、`deny` の `deny.toml` 未配置）のみ

## 参照

- make skill: `.claude/skills/make`（`SKILL.md`・`references/architecture/command-contracts.md`）
- 運用方針: CLAUDE.md の Conventions
