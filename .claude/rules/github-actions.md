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

## キャッシュ方針

### 前提（上限と予算）

- リポジトリの Actions キャッシュ上限は **10 GB**（Org 設定も 10 GB）。超えると最終アクセスの古いものから LRU で追い出され、
  7 日間アクセスの無いエントリも削除される
- Org の Actions 予算は **$0（超過不可）**。10 GB を超える上限は使えない（上げても 10 GB を超えた時点でキャッシュが読み取り専用になる）。
  **10 GB の中でヒット率を最大化する**（重複保存を作らない・効かないキャッシュを持たない）。上限・予算の変更はユーザー判断
- 現状（2026-10-11）: 約 2.6 GB・11 件。すべて `rust-ci` の cargo レジストリ・`target`・cargo-deny 本体（GitHub ホステッドの ubuntu runner）

### 本リポで触れる範囲

- キャッシュを持つのは reusable の `rust-ci`（`Fandhe-AI/actions` の `rust-base-ci.yml`）だけ。独立ジョブはキャッシュを持たない（上のチェックリスト 6）
- キー設計・restore-keys・保存前 prune は**共有側で決まる**。本リポの `ci.yml` から変えられるのは `cache`（`true`）・
  `cache-key-prefix`（`fandhe-db`）・`cache-save` だけ。キーや保存条件を変えたいときは、本リポに独自の `actions/cache` を足さず共有側へ PR を出す
- 共有側の改善（キーに rustc の版を入れる・キーの入力をルートの `Cargo.lock` に絞る・restore / save の分割と
  `cache-save` 入力の追加）は [Fandhe-AI/actions#165](https://github.com/Fandhe-AI/actions/pull/165) で対応済み（2026-10-11 マージ、`@latest` に反映）
- **保存は main の push だけ**: `rust-ci` の `with:` に `cache-save: ${{ github.ref == 'refs/heads/main' }}` を渡し済み。
  PR は restore のみで、main の blob に exact hit するか restore-keys で部分復元する（PR の 2 回目以降の push も自分の保存は使えない）。
  `ci.yml` は `push: branches: [main]` でも走り、key が変わったとき（ルートの `Cargo.lock` か rustc の版の変更）に main で保存される
- 本リポに新しくキャッシュを足す場合（他のワークフロー・独立ジョブ）も保存は main の push だけにする。`actions/cache` なら
  restore は全イベントで `actions/cache/restore`（`id: <id>`）、保存は `actions/cache/save` に
  `if: github.event_name == 'push' && github.ref == 'refs/heads/main' && steps.<id>.outputs.cache-hit != 'true'` と
  `key: ${{ steps.<id>.outputs.cache-primary-key }}` を付ける。`Swatinem/rust-cache` なら `save-if: ${{ github.ref == 'refs/heads/main' }}`。
  `actions/setup-node` の `cache:` のような一体型は PR でも保存する点に注意する

### キャッシュを追加・変更するときの指針

- **入れる前・残す前に効果を測る**: 復元にも時間がかかる。ジョブログで依存の再ビルド数（`Compiling` / `Checking` 行）・`downloaded` 件数・
  復元の秒数を見て、節約が復元時間を上回ることを確かめる。self-hosted runner ではツールチェーン・パッケージストア・`CARGO_TARGET_DIR` が
  ジョブをまたいで残り、GitHub のキャッシュが無意味になることがある（他リポジトリの実例: mirror-ui は pnpm キャッシュの復元に
  1 ジョブ平均 75 秒かけて `downloaded 0` だったため撤去、team-hub は `Swatinem/rust-cache` が保存する `./target` と runner の
  `CARGO_TARGET_DIR` が食い違い registry しか保存されていなかったため撤去）。本リポで self-hosted を使うジョブにはキャッシュを付けない
- **キーの設計**: 中身のハッシュ（lockfile 等）をキーにし、`github.sha`・`run_id` など毎回変わる値は入れない。
  `hashFiles('**/Cargo.lock')` のような再帰 glob は使わず、ビルド対象に本当に入るルートの lockfile を指定する（下表の 2 行目の事故）。
  `target/` のようなビルド成果物を入れるならキーにツールチェーンの版（`rustc -vV` の release・commit-hash）を入れる（下表の 1 行目の事故）。
  restore-keys で前方一致の部分復元を効かせ、キーが変わっても cold にしない
- **サイズを抑える**: 1 世代の合計が 10 GB に余裕をもって収まるようにする。`rust-ci` は共有側が `CARGO_INCREMENTAL=0` と
  保存前の workspace メンバー成果物の prune を行う（`ci.yml` の独立ジョブも `CARGO_INCREMENTAL=0`）。キャッシュ種別を増やすときは
  1 件のサイズ × 種別 × OS × キーが変わる頻度（1 日あたり）× 7 日で見積もる
- **追い出されても hit する設計を保つ**: 上限に達しても、毎回読まれる main の最新キャッシュは LRU の最後まで残る。キーの切り替えで
  古い世代が追い出されても新しい世代が保存済みならミスにならない。この状態を保てていれば上限を上げる必要はない
- **PR 本文に書く**: ワークフローにキャッシュを追加・変更する PR では、効果の計測・保存を main に限ったこと・キーの設計・
  サイズの見積もりを確認したことを PR 本文に書く

### 旧キーの既知の問題と対応（2026-10-11）

[#165](https://github.com/Fandhe-AI/actions/pull/165) 以前のキーは `fandhe-db-Linux-{fmt|clippy-v3|test-v3|deny}-<hashFiles('**/Cargo.lock')>`。#165 以降は
`fandhe-db-Linux-{clippy|test}-v4-<rustc の release>-<commit-hash 先頭 12 桁>-<hashFiles('Cargo.lock')>`・`fandhe-db-Linux-{fmt|deny}-<hashFiles('Cargo.lock')>`。

| 問題 | 影響 | 対応 |
| ---- | ---- | ---- |
| キーに rustc の識別が無い（`rust-toolchain.toml` は `channel = "stable"`） | runner イメージの更新で rustc が変わっても旧 rustc の blob に exact hit し続ける。exact hit では保存し直されないため、`Cargo.lock` が変わるまで clippy / test が依存を毎回全再ビルドする | #165 で clippy / test のキーに rustc の版を追加 |
| `**/Cargo.lock` が `.agents/skills/make/samples/projects/*/Cargo.lock`（`.claude/skills/make` symlink 経由も）を拾う | ビルドに関係しないスキル同期 PR だけでキーが変わる | #165 でルートの `Cargo.lock` のみに変更 |
| PR からも保存する | `Cargo.lock`（上記サンプル含む）を変える PR ごとに main と同内容の blob（約 0.9 GB）が `refs/pull/<N>/merge` に重複保存される | `cache-save` で main のみ保存に変更済み |
| fmt ジョブにも target を含むキャッシュがある | `cargo fmt` は保存対象のパスを作らないため毎回 miss し、保存時に "Path Validation Error" の警告が出る（無害） | 共有側で fmt のキャッシュを外す（[Fandhe-AI/actions#166](https://github.com/Fandhe-AI/actions/pull/166) で対応中） |

### 実測（2026-10-11）

| 項目 | 変更前 | 変更後 |
| ---- | ------ | ------ |
| キャッシュ使用量 | 21 件・約 4.4 GB（旧 prefix `vector-db-*` 9 件・`fandhe-db-*` の PR スコープ 9 件を含む） | 8 件・2.24 GB（旧 prefix・旧 rustc の main キャッシュ・古い PR スコープを削除） |
| clippy / test が再ビルドした依存（`Compiling` / `Checking` 行） | 135 | 2 |
| `rust-ci / cargo clippy` ジョブ時間 | 3.9〜4.9 分（4 run） | 1.4 分 |
| `rust-ci / cargo test` ジョブ時間 | 12.2〜17.6 分（4 run） | 14.7 分（ばらつきの範囲内。workspace メンバーの optimized ビルドが支配的） |

- 経緯: runner イメージ 20260927 では依存の再ビルドは 2 件だったが、20261004 への更新後は main のキャッシュ（10-08 作成）に exact hit したまま
  毎回 135 件を再ビルドしていた。旧キャッシュを削除し、新しいイメージで main のキャッシュを作り直した後は 2 件に戻った
- 変更後も PR スコープ（#2288・#2290）に main と同一キーの blob が 5 件（約 1.3 GB）残っている。`cache-save` を main のみにした以降の PR は
  保存しないため、この重複は作られなくなる（7 日で消える）

### runner イメージ更新後に再ビルドが増えたとき

[Fandhe-AI/actions#165](https://github.com/Fandhe-AI/actions/pull/165) で clippy / test のキーに rustc の版が入ったため、通常は rustc の更新で新しいキーに切り替わり、この手順は不要。
それでも exact hit なのに再ビルドが多い場合（rustc 以外の runner イメージの変化など）は、次の手順で古い main キャッシュを手動で削除する。

1. 症状の確認: `rust-ci / cargo clippy` のログが `Cache restored from key: <primary key と同じキー>`（exact hit）なのに、
   `Compiling` / `Checking` 行が数件ではなく 100 件超になっている。runner イメージは `Set up job` の `Runner Image` 節の `Version` で分かる
2. 一覧: `gh cache list -R Fandhe-AI/fandhe-db --ref refs/heads/main --sort created_at --limit 50`（作成日時がイメージ更新より前のものが対象）
3. 削除: `gh cache delete <cache-id> -R Fandhe-AI/fandhe-db`（キー指定なら `gh cache delete <key> --ref refs/heads/main -R Fandhe-AI/fandhe-db`）。
   PR スコープの残骸は `gh cache delete --all --ref refs/pull/<N>/merge -R Fandhe-AI/fandhe-db` で消せる
4. 次の main push の CI が restore-keys で部分復元（または miss）→ 依存を 1 回再ビルド → 新しいキャッシュを保存する。
   PR は main のキャッシュを読むため、main で作り直されるまでは再ビルドが続く

ログの取得は `gh run view <run-id> --log --job <job-id>` でジョブ単位に絞る（REST API の上限は Org 内で共有）。
使用量は `gh api repos/Fandhe-AI/fandhe-db/actions/cache/usage`、一覧（key・サイズ・ref）は `gh cache list -R Fandhe-AI/fandhe-db -L 100` で確認できる。
ヒットの判定はジョブログの `Cache restored from key:`（primary key と一致なら exact、それ以外は restore-keys の部分復元）・
`Cache not found`（miss）・`Cache saved with key:`（保存）で行う。キャッシュは共有状態なので、削除は読むジョブが無いことを確かめてから行う。
