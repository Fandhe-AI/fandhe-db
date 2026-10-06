# ブランチ管理の方針と次のアクションの決定記録

Issue #1334（親 #1332・ルート #1323）の成果物。メイン作業ツリーで `pull` を実行する時期、リベース・マージの戦略、次のアクションの順序を決めて残す。
本書は決定の記録であり、`pull`・submodule 更新・stash 削除・一時ログ削除は**本 PR では実行しない**（並列実行中の共有状態を変えないため）。
spec の内容は書かず、ビヘイビア ID・Issue 番号のポインタのみを使う（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。

## 1. 目的と責務境界

| Issue | 担当 |
| ----- | ---- |
| #1329 | メイン作業ツリーの遅れの診断（未マージの PR #1454） |
| #1330 | 一時ログの扱い |
| #1331 | 実装・テスト・レビュー面の残課題の棚卸し |
| #1333 | spec のポインタ同期とそのマージ方針 |
| #1334（本書） | ブランチ管理の方針（pull の時期・マージとリベースの戦略）と次のアクション |

## 2. 前提（サーバー側の強制）

いずれも GitHub の設定値であり、spec 由来ではない。

| 項目 | 値 |
| ---- | -- |
| ruleset `main-protection` | active。squash のみ許可、非高速転送と削除を禁止、レビュースレッドの解決は必須 |
| `strict_required_status_checks_policy` | false（base の最新化はマージの必須条件ではない） |
| `allow_squash_merge` / `allow_merge_commit` / `allow_rebase_merge` | true / false / false |
| `delete_branch_on_merge` | true |

既存の運用として、implement-issue-tree は push 前に `git fetch origin main:refs/remotes/origin/main` の後 `git merge origin/main` で base を取り込み、remote-ahead と diverged は fail-closed で停止する。
squash 後も中間コミットが PR refs に残る点は [spec-pointer-in-commit-messages.md](spec-pointer-in-commit-messages.md) に記録済み。

## 3. 現状スナップショット

計測日時は 2026-10-06 18:29（ローカル時刻）、基準は `origin/main`（`fbe23e7c`）。値は読み取りのみで測った。

| 項目 | 結果 |
| ---- | ---- |
| メイン作業ツリーの `main` と `origin/main` | behind 132・ahead 0（fast-forward 可能） |
| `docs/spec` の gitlink（`origin/main`） | `16136c84` |
| メイン作業ツリーの submodule チェックアウト | `f18bb6ac`（gitlink より古い。#1325 の記録どおり。差のコミット数は未計測のため UNKNOWN） |
| `git stash list` | 7 件（内容は記載しない。共有スタックのため他セッションのものを含みうる） |
| PR #1453 | CONFLICTING・check は SUCCESS 27 / SKIPPED 1 |
| PR #1454 | CONFLICTING・check は SUCCESS 25 / SKIPPED 1 / FAILURE 2 |
| PR #1457 | MERGEABLE・check は SUCCESS 26 / SKIPPED 1 / FAILURE 1 |

private submodule のコミット件名は記載しない。

## 4. 決定事項

| # | 決定 | 理由 | 代替案と却下理由 |
| - | ---- | ---- | ---------------- |
| D1 | メイン作業ツリーの取り込みは `git pull --ff-only` のみ。ahead が 1 以上なら pull せず、ローカルのコミットを別ブランチへ退避してから判断する | 遅れは 132 コミットで ahead は 0。履歴を書き換えず fail-closed に止まれる | `pull --rebase`・merge コミット: ローカル変更を暗黙に書き換える／無意味な merge コミットが残る |
| D2 | pull の時期は次の 3 条件がそろった後。(a) 並列の implement-issue-tree ランが動いていない (b) 本ツリーの open PR（#1453・#1454・#1457）がマージかクローズで片付いた (c) #1333 で gitlink が確定した。実行前に #1454 がマージ済みなら `make worktree-lag-check REMOTE_CHECK=1` で HIGH=0 を確認し、未マージなら同じ観点を手動で確認する | 並列ラン中の共有状態の変更を避ける。gitlink 確定前に取り込むと submodule のずれが再発する | 今すぐ pull: 並列ランと open PR の conflict 解消に干渉する |
| D3 | push 済み feature ブランチへの base の取り込みは `git merge origin/main`。push 後の rebase と force push はしない | remote-ahead／diverged 検査が fail-closed で止まる。PR refs に中間コミットが残るので rewrite に意味が無い。squash merge で main には 1 コミットしか残らないため merge コミットは main に残らない | rebase + force push: ruleset の非高速転送禁止と既存検査に反する |
| D4 | 古くなった open PR は D3 の取り込みと CI 再実行で更新する。strict が false なので最新化は必須ではないが、conflict がある場合や CI failure の原因が base 側にある場合は取り込む | 不要な再実行を避けつつ、conflict は必ず解消する | 全 PR を一律に最新化: CI コストが増える |
| D5 | `implementation-status.md` 末尾への追記どうしの conflict は、両方の記録を残し、Issue 番号順ではなくマージ順で並べて解消する | 追記専用ファイルなので内容の取捨は不要 | 片方を採用: 記録が欠落する |
| D6 | pull の後に `git submodule update --init` を実行する（アクセス権が無ければスキップ）。gitlink の更新方針は #1333 に任せ、本書では決めない | 作業コピーを gitlink に合わせる。方針の重複決定を避ける | gitlink を手元で更新: #1333 の責務と衝突する |

## 5. 次のアクション（順番付き）

すべての行で「本 PR では実行しない」。

| # | アクション | 担当 | 前提 |
| - | ---------- | ---- | ---- |
| 1 | #1453・#1454・#1457 の conflict・CI failure の解消とマージ | 各 Issue の実装者・マージする人間 | なし |
| 2 | #1333 の実施（spec のポインタ同期とマージ方針） | 人間／自動化可 | なし（1 と並行可） |
| 3 | メイン作業ツリーで D2 を確認し `git pull --ff-only` を実行 | 人間 | 1・2 の完了、並列ランの停止 |
| 4 | `git submodule update --init` を実行。`lefthook.yml` に変更があれば `make hooks` も実行 | 人間 | 3 |
| 5 | `make ci` で取り込み後の状態を確認 | 人間 | 4 |
| 6 | 一時ログ（#1330 で ignore 化）と stash の棚卸し（stash の削除はオーナー判断） | 人間 | 3 |
| 7 | ルート #1323 の Phase 親（#1324・#1328・#1332）のクローズ判断 | 人間 | 1〜6 |

## 6. 対象外

- `pull`・submodule 更新・stash 削除・一時ログ削除の実行
- Issue の起票、spec 側の変更、#1333 の決定事項
- CI・Makefile・スクリプト・依存の変更
