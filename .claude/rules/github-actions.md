---
paths:
  - ".github/workflows/**"
  - ".github/actions/**"
---

# GitHub Actions 編集ルール

## 目的

ワークフロー数・ジョブ数・実行時間（課金分数）を増やさず、必須チェックを壊さない。
CI は PR ごとに走るため、遅いジョブはそのまま開発速度の低下になる。
`lefthook` と重複する検査の扱いは [git-hooks](./git-hooks.md) を参照する（CI 側が正）。

## runner 方針

- 本リポは **public**。runner は GitHub ホステッド（`ubuntu-latest`）を既定とする
- 例外 1: `detect-features.yml` の `detect-apple` は `macos-latest`（Apple Silicon 実機での feature 検出が目的の OS 要件）
- 例外 2（組織承認済み）: `ai-review.yml` の `codex / review` は self-hosted の codex 専用 runner（reusable workflow 既定のラベル `codex`）。
  fork PR では reusable workflow 側が fail-closed で実行を拒否する。これ以外のジョブで `self-hosted` を使わない
  （public リポで self-hosted を使うと、fork PR 経由で任意コードが self-hosted runner 上で実行されるリスクがある。
  `bench.yml` の self-hosted 経路は過去の指摘で撤去済み）
- reusable workflow（`Fandhe-AI/actions`）には `runner-label: ubuntu-latest` / `runner-json: '"ubuntu-latest"'` / `post-feedback-runner: ubuntu-latest` を明示する
- 第三者 action は SHA 固定（`# vX.Y.Z` コメント付き）。`@latest` を許すのは `Fandhe-AI/actions` のみ（AGENTS.md「CI・ワークフローの改変」）

## 必須チェックと集約ジョブ

必須チェックは ruleset（`gh api repos/Fandhe-AI/fandhe-db/rulesets`）で管理されている。
branch protection（classic）は未設定。主なジョブ名:

| 区分 | 必須チェック名 |
| ---- | -------------- |
| `ci.yml` の独立ジョブ | `test-default-build`・`crash-test`・`crash-test-interrupt`・`crash-test-cross-table`・`crash-test-unique-index`・`crash-test-partitioned-dml`・`tmp-leak-check`・`core-api-check`・`sort-determinism-check`・`scripts-test`・`simd-codegen-check`・`cross-check` |
| `ci.yml` の reusable（lint-docs） | `lint-docs / markdownlint`・`lint-docs / yamllint`・`lint-docs / editorconfig-checker`・`lint-docs / commitlint`・`lint-docs / lint-docs-complete`（集約） |
| `ci.yml` の reusable（rust-ci） | `rust-ci / cargo fmt --check`・`rust-ci / cargo clippy`・`rust-ci / cargo test`・`rust-ci / cargo deny check`・`rust-ci / rust-base-ci-complete`（集約） |
| `ai-review.yml` | `skip-guard`・`codex / preflight`・`codex / review`・`codex / post_feedback` |
| `detect-features.yml` | `detect-changes`・`detect-apple` |
| 外部 | `Cursor Bugbot` |

- **ジョブ名（reusable は `<呼び出しジョブ ID> / <内部ジョブ名>`）を変えると必須チェックが pending のままマージ不能になる**。
  改名・削除・追加は ruleset 側の更新が必要なため、PR 本文に「ユーザー判断事項」として明記する（ruleset は Claude が変更しない）
- 集約ジョブ（`lint-docs-complete`・`rust-base-ci-complete`）は reusable 側にある。新しい reusable を呼ぶ場合は集約ジョブを必須に加える
- 条件付きジョブが `skipped` になる場合、GitHub は必須チェックを満たしたものとして扱う。逆に **reusable workflow を呼ぶジョブ自体を `if:` で skipped にすると、内部ジョブの必須チェック名が報告されず pending になる**ため、`rust-ci`・`lint-docs` の呼び出しには `if:` を付けない

## 発火条件の方針

- `push` は default ブランチ（`main`）のみ。PR は `pull_request` で拾い、同一コミットの二重実行を作らない
- `concurrency`: group は `${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}`（PR 番号を含める。head_ref のみだと fork 間で同名ブランチの別 PR が cancel し合う）。
  `cancel-in-progress` は PR のみ（`${{ github.event_name == 'pull_request' }}`）。main への push・release・schedule は cancel しない
- **必須チェックを含む workflow に workflow 単位の `paths` / `paths-ignore` を付けない**（対象外 PR で check が生成されず pending のままマージ不能になる）。
  ジョブ内で変更検知する場合は `detect-features.yml` の `detect-changes` と同型にする（PR の変更ファイル一覧 API で判定し、失敗は fail-closed で「変更あり」）
- **検証ジョブの skip 化・弱体化は AGENTS.md「CI・ワークフローの改変」で P1 扱い**。docs のみの PR で `ci.yml` の独立ジョブを省略する案
  （`ci-changes` ジョブ + `if:`）は codex review が P1 として指摘したため採用していない。導入するには AGENTS.md の例外をオーナーが先に合意すること。
  同様に `permissions` の追加（`pull-requests: read` 等）も P1 のため、既存の `contents: read` の範囲で実現する
- draft PR は現状スキップしない（draft でも CI の早期フィードバックを得る運用）。スキップする場合は `ready_for_review` を `types` に含める
- schedule: `bench.yml`（毎週月曜 03:00 UTC）・`recall.yml`（毎週月曜 04:00 UTC）・`toolchain-canary.yml`（毎週月曜 05:00 UTC）・`update-external.yml`（毎日 00:00 UTC）。頻度を上げない。`merge_group` は使っていない（merge queue 未使用）

## ジョブ・ワークフローを追加するときのチェックリスト

1. 新しいワークフローを作らず、既存ワークフローへのジョブ追加を優先する（ワークフロー数を増やさない）
2. 必須にするジョブ名は ruleset の更新が要る。命名は既存ジョブ（kebab-case）に揃える
3. 独立ジョブ（生の steps）は `permissions: contents: read`・`timeout-minutes`・SHA 固定の `actions/checkout`（`fetch-depth` は既定の 1 のまま。submodule は取得しない）にする
4. 不要な `needs` を作らない（独立ジョブは並列）。逆に、セットアップ（checkout・toolchain）が実行時間を支配する小さなジョブは統合を検討する
5. チェックの中身は `Makefile`/`scripts/` の共通コマンド（`make <target>`）を呼ぶ。CI 独自にロジックを書かない（[makefile](./makefile.md)）。`ci` ターゲットとの対応も保つ
6. cargo ビルドを伴うジョブは `CARGO_INCREMENTAL=0`（`ci.yml` は workflow の `env:`）。reusable の rust-ci は `cache: true` で cargo キャッシュを使う。独立ジョブはキャッシュを持たないため、重いビルドを足す前に reusable 側（Fandhe-AI/actions）への取り込みを検討する
7. lefthook と同じ検査を CI にも置くのは可（CI が正）。逆にフックへ重い検査を足さない
8. 追加後に `actionlint` と `yamllint` を通し、PR 上の実測所要時間を PR 本文に記載する

## 計測方法

```bash
gh run list --workflow ci.yml --limit 30 --json databaseId,conclusion,createdAt,updatedAt,event
gh run view <run-id> --json jobs --jq '.jobs[] | "\(.name)\t\((.completedAt|fromdate)-(.startedAt|fromdate))s\t\(.conclusion)"'
```

### 棚卸し結果（2026-10-11・直近 30 件程度）

| ワークフロー | トリガー | 中央値 / 最大（秒） | 備考 |
| ------------ | -------- | ------------------- | ---- |
| `ci.yml` | pull_request / push(main) | 895 / 1089（PR）・877 / 1108（main） | クリティカルパスは `rust-ci / cargo test`（約 740 秒）。clippy 約 290 秒、独立ジョブは 5〜230 秒 |
| `ai-review.yml` | pull_request | 97 / 264 | |
| `detect-features.yml` | pull_request | 8 / 39 | 対象外 PR は `detect-changes` のみ |
| `bench.yml` | schedule | 239 / 256 | |
| `recall.yml` | schedule | 266 / 374 | |
| `update-external.yml` | schedule | 99 / 124 | |
| `sync-issue-behavior-ids.yml` | push(main)・手動 | 73（push）・1118（手動） | |
| `release.yml` | 手動 | 139 / 279 | |
| `toolchain-canary.yml` | schedule | 未計測（実行履歴なし） | |
