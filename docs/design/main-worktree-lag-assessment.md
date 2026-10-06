# メイン worktree の遅れの評価手順と評価結果（Issue #1329）

## 目的

メイン worktree（リポジトリルートの `main` チェックアウト）が `origin/main` からどれだけ遅れているかを把握し、pull 前に競合・作業消失・環境更新漏れのリスクを評価する。評価は `scripts/check_worktree_lag.sh`（読み取り専用）で誰でも同じ手順で再現できる。

Issue タイトルの「69 コミット」は起票時点（2026-10-02）の値で、`main` は並列実装で進み続けるため、評価のたびに測り直す。

## 評価観点とリスク分類

| 区分 | 基準 | 例 |
| ---- | ---- | -- |
| HIGH | pull が失敗する、または作業を失いうる | ローカルが ahead（ff 不可）、取り込む側と同じファイルへの未コミット変更、取り込む側が追加するパスと衝突する untracked、submodule 内のローカル変更・ローカルコミット |
| MEDIUM | pull は通るが、追随作業が必要 | `Cargo.lock`・依存行の変更、`rust-toolchain.toml` の変更、`lefthook.yml` の変更（`make hooks` 再実行）、submodule の gitlink 変更（`git submodule update`） |
| INFO | 判断材料 | untracked の非衝突ファイル、submodule チェックアウトが gitlink より古いだけ、stash・worktree 件数、Makefile／CI／skills の変更 |

判定できない状態（submodule のオブジェクト未取得など）は OK とせず INFO（UNKNOWN）として明示する。

## スクリプトの使い方

```bash
make worktree-lag-check REPO=<メイン worktree> REMOTE_CHECK=1
make worktree-lag-check STRICT=1   # HIGH があれば exit 1
make worktree-lag-check-selftest   # セルフテスト
```

| 変数 | 既定 | 意味 |
| ---- | ---- | ---- |
| `REPO` | カレントの toplevel | 評価対象の worktree |
| `BASE_REF` | `origin/main` | 比較先 ref（不正なら exit 2） |
| `REMOTE_CHECK` | 未指定 | `1` で `git ls-remote` によりローカル `origin/main` の鮮度を確認（ネットワーク使用・ref は更新しない） |
| `STRICT` | 未指定 | `1` で HIGH 検出時に exit 1 |

- fetch・pull・checkout・stash・clean・submodule update・reset は実行しない。セルフテストが実行前後の status・HEAD・refs・stash の不変を検証する
- private submodule（`docs/spec`）はコミット件名を出さず SHA と件数のみ表示する
- `make ci`・CI には含めない（環境依存の運用ツールのため。`check-cross`・`bench-*` と同方針）

## 推奨する pull 手順

1. `REMOTE_CHECK=1` で評価し、ahead=0 かつ HIGH=0 を確認する（`origin/main` が STALE と出たら先に `git fetch origin`）
2. `git pull --ff-only`
3. `git submodule update --init`（private のためアクセス権が無ければスキップ）
4. `lefthook.yml` の変更が出ていれば `make hooks`
5. `make ci`

ahead>0 または HIGH がある場合は pull せず、個別に判断する。

## 評価スナップショット（2026-10-06）

| 項目 | 結果 |
| ---- | ---- |
| behind / ahead | 129 / 0（ローカルの `origin/main` はリモート main と一致していた） |
| fast-forward | 可能 |
| HIGH / MEDIUM / INFO | 0 / 2 / 4 |
| tracked 変更 | なし（`docs/spec` は gitlink より古いコミットがチェックアウトされているだけで、ローカル編集ではない） |
| untracked | 一時ログ 1 件。取り込む側と衝突しない（整理は #1330 で扱う） |
| submodule | gitlink が取り込み側で更新される（MEDIUM）。チェックアウトは現 gitlink より 9 コミット古い（INFO） |
| 依存・ツールチェーン | `Cargo.lock`・`rust-toolchain.toml`・`lefthook.yml`・`deny.toml` の変更なし。`Cargo.toml` に依存風の行変更が 1 件（feature／bench 定義の追加で、依存の追加・更新ではない。MEDIUM は保守的な検出） |
| その他の取り込み変更 | Makefile・CI・commitlint 設定・skills 関連の変更（INFO）。変更ファイル数 430 |
| stash | 7 件（全 worktree 共有。pull の影響なし） |

結論: 競合・作業消失のリスクは無く、推奨手順どおり `git pull --ff-only` で取り込める。追随作業は `git submodule update --init`（アクセス権がある場合）。

## 対象外

- 実際の pull・submodule update（並列実行中の共有状態を変えないため実施しない）
- 一時ログの整理（#1330）、stash の整理、ドキュメント更新漏れの集約（#1331）
