# ADR: spec サブモジュールの反映経路とマージ方針

- ステータス: Accepted（本 PR のマージをもって確定）
- 対応: Issue #1333（親: #1332 / #1323）
- 関連規約: `.claude/rules/spec-confidentiality.md`・`.claude/rules/dependency-policy.md`・`AGENTS.md`（CI 改変・同期 PR の AI レビュー skip の例外）

## 経緯

private spec リポ（`docs/spec` submodule）側の変更を本リポ（main）へ反映する際の
経路とマージ方針が明文化されていなかった。実際の状態を調査した結果、
gitlink は既にリモート spec main と一致しており、ポインタを動かす必要はなかった。
本 ADR は調査結果と、今後の反映経路・マージ方針を記録する。spec の内容は一切記載せず、
短縮 SHA と本リポの PR 番号のポインタのみで示す。

## 調査結果

| # | 事実 |
| - | ---- |
| 1 | `origin/main` が記録する `docs/spec` の gitlink は `16136c84`。spec リポのリモート `refs/heads/main` も `16136c84` で一致する（確認コマンドの形: `git ls-tree origin/main docs/spec`・`git ls-remote <spec リポ> refs/heads/main`）。確認時点で差分なし |
| 2 | ポインタを進めた本リポ側 PR は #1208（`dd640e7d`）→ #1368（`9d8b71ac`）→ #1443（`16136c84`）。いずれも `update-external.yml` が生成した同期 PR を人手で squash merge したもの |
| 3 | メイン作業ツリーの `M docs/spec` は作業コピーが `f18bb6ac`（`16136c84` の祖先）に残る「遅れ」であり、新しい spec 更新ではない（#1325 参照） |
| 4 | リポ設定は squash merge のみ許可・マージ後ブランチ自動削除。branch ruleset により main への force push は不可 |
| 5 | `update-external.yml` は組織共通の reusable workflow を呼ぶ wrapper で、毎日 00:00 UTC と手動実行で起動する。auto-merge は組織変数 `SUBMODULE_AUTO_MERGE` が未設定なら `false` に倒れるオプトイン設計。変数の現在値は本調査では未確認（未設定時は `false`） |
| 6 | 同期 PR（`chore/submodule-update-*`）の AI レビューは `AGENTS.md` の例外規定で skip される |

## 判断（マージ方針）

1. **経路の一本化**: 反映経路は `update-external.yml` が生成する同期 PR とする。CI が全て green になってから人手で squash merge する。`SUBMODULE_AUTO_MERGE` の有効化はオーナー判断事項とし、本 ADR では有効化しない。
2. **手動更新**: 自動同期の障害時などにポインタを手動で動かす場合も、必ず PR 経由・単一関心事のコミット `chore(spec): ...` とする。直接 push・force push は行わない。
3. **前進条件**: 新しい gitlink は (a) spec リポの `main` から到達可能で、(b) 旧ポインタの子孫であること（`git merge-base --is-ancestor <旧> <新>` で確認）。トピックブランチ上のコミットへの付け替え・巻き戻しは不可とする。
4. **順序**: spec リポ側 PR が spec の main へマージされた後に本リポの gitlink を進める。spec の新しいビヘイビア ID に依存する実装 PR は、ポインタ更新 PR のマージ後に着手・マージする。
5. **公開範囲**: ポインタ更新 PR のコミットメッセージ・PR 本文には短縮 SHA・本リポの PR 番号・TASK-nn／ビヘイビア ID のポインタのみを書く。spec のコミット件名・本文・差分の要約は書かない。
6. **ビルド独立性**: 実装コードのビルド・テストは `docs/spec` 抜きで成立する状態を保つ。
7. **ローカル追従**: 作業コピーの追従は利用者側の手順とし、`git pull --ff-only` の後に `git submodule update --init docs/spec`（`make submodule` と同じ）を実行する。pull 前のリスク評価手順は #1329（PR #1454）で整備中。
8. **コード変更を伴う場合**: spec の更新が本リポのコード変更を要するときは、ポインタ更新 PR に実装を混ぜず、別 Issue・別 PR で扱う（起票はユーザー承認を経る）。

## 根拠

- gitlink は既に同期済みで、手動操作の必要がない。経路を自動同期 PR へ一本化すれば履歴の追跡が容易になる。
- 人手の squash merge と CI green を必須とするのは fail-closed 側の方針であり、ruleset・AI レビュー skip 条件を緩めない。
- 前進条件により、巻き戻しや任意コミットへの付け替えを防ぐ（ソフトウェアとデータの完全性）。
- 公開範囲の制限は spec の機密保持規約に従う。

## 境界・スコープ外・申し送り

- 本 ADR は「spec 変更が main に入る経路」のみを扱う。実装ブランチのリベース・マージ戦略は #1334 の担当。
- `SUBMODULE_AUTO_MERGE` の有効化可否はオーナー判断事項。`update-external.yml`・組織変数は本 Issue で変更しない。
- メイン作業ツリーの実際の追従は #1329・#1334・人手の担当。`push1230b.log` の整理は #1330 の担当。
