# 実装対象外の追跡規約

## 目的

実装・レビュー中にスコープ外と判断した事項を放置しない。
後で埋もれるのを防ぐため、発見したタイミングで Issue に記録して追跡可能にする。

## フロー

1. **検出**: 実装・レビュー中にスコープ外と判断した事項をメモする
2. **既存 Issue の確認**:

   ```bash
   gh issue list --search "${KEYWORD}" --state open
   ```

   キーワードは `"${KEYWORD}"` でクォートして渡す。
3. **ユーザーに承認を得る**: 既存 Issue への追記か新規起票かをユーザーと確認する
4. **記録**:
   - 既存 Issue がある場合:

     ```bash
     gh issue comment "${ISSUE_NUMBER}" --body "$(cat <<'EOF'
     （本文をここに記述）
     EOF
     )"
     ```

   - 存在しない場合: `create-issue-tree` または `create-issue` で適切な親 Issue 配下に起票
5. **元 PR に切り出し先を記録**: PR 本文の「対象外」節（見出しに「対象外」または "out of scope" を含める）の各項目に切り出し先の Issue 番号を書く。レビューのコメントにも書いてよいが補助扱い。PR は `Closes #<子 Issue>` で子 Issue に結び付ける（結び付かない PR は突合の対象にならない）

## 対象外の書き方（作法）

- **受け皿を書く**: 対象外の各項目に、受け皿にする open の Issue 番号（`#N`）を必ず書く。次は受け皿と認めない（突合では UNTRACKED または CLOSED-ONLY になる）
  - 番号の無い「後続で対応」「別 Issue で」
  - PR 番号だけ
  - close 済みの Issue だけ
  - 今回 close する親 Issue
- **委譲するときは相手の受入基準に転記する**: 「相手 Issue が結線する」として送るときは、結線する内容を相手 Issue の受入基準へ書き足し、相手側の合意を取る。片側だけの記述にしない（A → B → A の循環を防ぐ）
  - 相手 Issue の本文編集は public への書き込みなので、新規起票と同じくユーザー承認を経る
  - 書く内容は spec の ID・TASK-nn のポインタだけにする（[spec-confidentiality](./spec-confidentiality.md)）
- **対応しない項目は理由を書く**: 「対応しない」と判断した項目は、その旨と理由（設計記録へのリンク等）を書く。突合ではこの項目も UNTRACKED として出るので、close 時に人が確認する
- **節の形式を守る**: 見出しは「対象外」または "out of scope" を含む ATX 見出しにし、コードフェンスの中に書かない。該当なしのときは「なし」と書く（空として扱われる）

## 親 Issue close 前の突合

親 Issue（ツリーのルート・Phase 親を含む）を close する前、および implement-issue-tree 等でツリーを完了したときに、子 Issue の PR 本文の対象外節と受け皿 Issue を突合する。

```bash
scripts/check_issue_closeout.sh "${PARENT_ISSUE}"
scripts/check_issue_closeout.sh "${PARENT_ISSUE}" --verbose
scripts/check_issue_closeout.sh --check-closed
scripts/check_issue_closeout.sh "${PARENT_ISSUE}" --check-closed
```

- 読むのは、Issue を close したマージ済み PR の本文だけ（コメントは読まない）。出力は番号・見出し・分類だけで、本文は再掲しない。結果を Issue や PR へ転記するときも spec ポインタ規約に従う
- `--verbose`: OK の項目も出す
- `--check-closed`: close 済み Issue を指す `tracked:` 注記を列挙する（書式は `docs/design/rejection-pin-annotation.md` の D2・D10）。親番号と併用できる
- `--repo OWNER/NAME`: 対象リポ。`--max-hops N`: 辿る深さ（0〜3、既定 1）。`--max-nodes N`: 走査する Issue 数の上限（1〜5000、既定 500）
- `gh` の認証とネットワークが要るため、make・CI には載せない手動ツール

| 出力 | 意味 | 対処 |
| ---- | ---- | ---- |
| `UNTRACKED` | 項目に Issue 番号が無い | フロー 2〜4 に従い、ユーザー承認のうえ既存 Issue へ追記または起票する。そのあとマージ済み PR 本文の対象外節へ番号を書き足す（`gh pr edit`・承認を経る）。「対応しない」項目は理由が書かれていれば人が判断してよく、判断は親 Issue のコメント等に残す |
| `CLOSED-ONLY` | close 済み Issue・PR・親 Issue しか指していない | 受け皿が実際に完了しているか確認する。残作業があれば reopen または新規起票（承認を経る）。完了していれば人が確認のうえ close してよい |
| `CYCLE` | Issue 同士が互いを受け皿として指している | 結線する側を 1 つに決め、その Issue の受入基準へ転記する |
| `TRACKED-CLOSED`・`TRACKED-NOT-ISSUE` | `tracked:` 注記が close 済み Issue または Issue ではない番号を指している | reopen するか、注記を open の Issue に付け替える（別 PR） |
| `note: issue #N is still open`・`note: issue #N has no merged closing PR` | 子孫 Issue が open、または結び付いたマージ済み PR が無い | 子孫 Issue の状態と PR との結び付けを確認する |

- 終了コード: 0 は検出なし、1 は検出あり、2 は使用法の誤り、3 は取得失敗
  - 3 は検査を実行できなかったという意味で、検出なしではない。fail-closed として、3 のまま親を close しない
  - 1 は「必ず起票せよ」ではなく、上の表に従って判断する
- 既知の限界
  - 「前例 #N」のような受け皿ではない言及も参照として数える
  - 「対応しない」旨の項目も UNTRACKED になる
  - 見出しに「対象外」も "out of scope" も含まない節は拾わない

## 本リポ固有の注意

- Issue は public リポに作られる。`docs/spec`（private）の本文を Issue 本文へ転記せず、TASK-nn・ビヘイビア ID のポインタ表記に留める（[spec-confidentiality](./spec-confidentiality.md)）
- spec 側（タスク定義・ロードマップ）の変更が必要と判断した事項は、本リポの Issue ではなく spec リポ側の課題としてユーザーに報告する

## 注意

- ユーザーの承認なしに勝手に Issue を起票しない
- スコープ外の修正を現在の PR に混入させない（別 Issue・別 PR で対処する）
- 親 Issue は突合の結果を確認するまで close しない
