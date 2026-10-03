// commitlint 設定。
//
// CI では Fandhe-AI/actions の lint-docs reusable workflow（commitlint）が
// `--extends @commitlint/config-conventional` 付きで PR の commit 範囲を検証し、
// 本ファイルのルールが extends 側を上書きする（agent-cli-skills の同名設定と同方針）。
import { createHash } from 'node:crypto';

export default {
  rules: {
    // 日本語 subject は「Claude Code スキル体系を導入」のように英大文字始まりの
    // 固有名詞・識別子で始まることが多く、config-conventional の subject-case
    // （sentence-case 等の禁止）と構造的に衝突するため大文字小文字の検査は無効化する
    'subject-case': [0],
    // 標準の body-max-line-length は下記プラグインの限定版へ置き換える。
    // 対象は本文行の長さ検査のみで、他の全規則は全コミットへ適用し続ける。
    'body-max-line-length': [0],
    'local/body-max-line-length-with-exemptions': [2, 'always', 100],
  },
  plugins: [
    {
      rules: {
        // PR #1386（Issue #1354）の中間コミット 2 件は本文に 100 文字超の行を持つ。
        // 共有済みブランチの force-push を避けるため、この 2 件に限り本文行長の検査だけを
        // 免除する。免除は件名ではなく本文全体の SHA-256 ダイジェストで固定する
        // （commitlint のプラグインにはコミット ID が渡らないため。同じ件名を再利用しても
        // 本文が異なれば免除されず、検査回避に使えない）。type・subject・footer 等の
        // 検査は免除せず、それ以外のコミットは 1 行 100 文字（既定。設定値で上書き）で検査する。
        // 当該 2 件が PR 範囲から外れた時点（squash merge 後など）で本免除は削除してよい。
        'local/body-max-line-length-with-exemptions': (parsed, _when, value) => {
          const exemptBodyDigests = new Set([
            '4a3ea2c0919d2917748c037d3d8d9e2f77059f6f9aabfbbc5cae91cb8d9f852c',
            '0b96744561ffde8c32124f7967b8cba2b0c720f0063c78be606cdbe70fa793ed',
          ]);
          const body = parsed.body ?? '';
          const digest = createHash('sha256').update(body).digest('hex');
          if (exemptBodyDigests.has(digest)) return [true];
          const limit = typeof value === 'number' ? value : 100;
          // 標準の body-max-line-length と同じく URL（http:// か https://）を含む行は免除する
          const bad = body
            .split('\n')
            .find((l) => !/https?:\/\//.test(l) && [...l].length > limit);
          return [bad === undefined, `body's lines must not be longer than ${limit} characters`];
        },
      },
    },
  ],
  // `git merge --no-edit`（origin/main 取り込み）が生成する既定のマージコミット
  // メッセージ（「Merge branch '...' into ...」等）は commitlint の
  // `defaultIgnores` により既に対象外だが、過去に本リポで使われていた
  // `merge: origin/main を取り込み` 形式（type-enum に無い `merge` を type として
  // 使う）はこのデフォルトパターンに一致せず type-enum で fail していた
  // （PR #249 CI 指摘）。履歴の書き換え（reword）は共有済みブランチの force-push を
  // 要し安全側でないため行わず、代わりにこの既存パターンの merge コミットのみを
  // commitlint の検証対象から明示的に除外する。以降の base 取り込みコミットは
  // 通常の Conventional Commits 形式（例: `chore(engine): base ブランチの変更を
  // 取り込む`）を使うため、本 ignore は過去コミットの後方互換のためだけに残す。
  // 正規表現を subject 接頭辞（`/^merge:\s/i`）にすると、今後 subject 内容を
  // 問わず追加される任意の `merge: ...` コミットまで恒久的に検証対象外にして
  // しまう（PR #249 codex-review P1 指摘）ため、実在する既知の履歴コミットの
  // subject 行（1 行目）への完全一致に限定する。`ignores` の各関数には commit
  // の生メッセージ全体（本文・フッターを含む）が渡されるため、本文側の内容は
  // 問わず 1 行目だけを比較する。
  ignores: [
    (commit) => commit.split('\n', 1)[0] === 'merge: origin/main を取り込み',
  ],
};
