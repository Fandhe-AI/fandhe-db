# private spec の機密保持規約（リポ固有・P0）

## 前提

- 本リポジトリは **public**
- 仕様・ビヘイビア定義の SSOT は **private** リポジトリ [fandhe-db-spec](https://github.com/Fandhe-AI/fandhe-db-spec)（`docs/spec` submodule）にあり、**意図的に非公開を維持**する

## 禁止事項（AGENTS.md P0 準拠）

以下を public な資産（コード・コメント・ドキュメント・Issue・PR 本文・コミットメッセージ）へ持ち込まない:

- spec 本文の長文引用・ファイルコピー
- 非公開の内部判断・設計議論の転記
- spec の構成・内容を実質的に復元できる要約

## 許可される参照（ポインタ表記）

- spec 内のファイルパス（例: `docs/spec/04-behavior/...`）
- タスク ID（TASK-nn）・ビヘイビア ID・マイルストーン ID（MS-n）
- 1〜2 行程度の、README「実装方針（要点）」で**既にオーナーが公開済みの範囲**に収まる要約
- **数値基準・実測値**（受け入れ基準の閾値・テスト/ベンチの実測値・導出係数）。オーナー判断
  （2026-08-29・AGENTS.md「private spec 内容の漏えい」項）により公開可。層 A の固定値
  アサーション・docs の実測表・Issue への pass/fail と実測値の並記はいずれも許可される
- **方式評価・受け入れ基準未達の原因分析**（方式 × fixture の実測比較・未達原因の
  構造分析・fixture 設計判断・Issue 差し戻し経緯・オーナー承認済み spec 改訂の事実と
  理由の要約）。オーナー判断（2026-08-31・AGENTS.md 同項・spec 側記録は
  `docs/spec/04-behavior/search.md` SEARCH-7 の公開境界注記）により公開可

## 運用

- 公開可否に迷った場合は**転記しない側**に倒し、ユーザーに確認する
- subagent への指示・subagent からの報告でも同じ規約を適用する（報告が PR 等へ転記されうるため）
- レビュー時（reviewer / security-auditor）は spec 漏えいを P0 として検査する

## コミットメッセージ・PR 本文での運用

- コミットメッセージ（件名・本文）・PR 本文・レビュー返信も public 資産であり、上記の禁止事項・ポインタ表記が同様に適用される
- squash merge 後もブランチ上の中間コミットは GitHub の PR refs（`refs/pull/<n>/head`）から参照可能であり、
  main の history rewrite では除去できない（rewrite 自体も branch ruleset で禁止されている）。**マージ前の検査で防ぐ**ことを原則とする
- レビュー指摘（spec 転記）への対応コミットでは、「何を削除したか」の説明に spec 規則の内容・規則が定める対象を
  再転記しない。「private spec 由来の記述を削除（ビヘイビア ID のポインタのみ残置）」のように ID ポインタと削除の事実だけを書く
- squash merge の件名・本文は PR 本文から生成される。PR 本文（Summary・対象外・自動生成の要約を含む）にも
  spec 本文・内部判断の転記が無いことを PR 作成前チェック（create-pr の OWASP 観点）に含める
- **base..head の全コミットメッセージ検査（マージ前必須）**: PR 作成時（create-pr スキル実行時）に、
  実装担当（自動運転では実装 Agent、手動では作業者本人）が `git log <base>..HEAD --format="%H %s%n%b"` 等で
  base ブランチから HEAD までの**すべての中間コミット**の件名・本文を検査し、本規約の禁止事項（spec 本文の
  長文引用・非公開の内部判断の転記・spec の構成/内容を実質復元できる要約）への抵触が無いことを確認する。
  squash merge 後は PR refs（`refs/pull/<n>/*`）に中間コミットが永続し history rewrite でも除去できないため、
  この検査は squash 前・マージ前に完了させる。抵触を検出した場合は該当コミットを含む history をポインタ表記へ
  書き換える等で解消するまで**マージを停止**する。レビュー担当（reviewer / security-auditor）は PR レビュー時に
  この検査が実施された旨（または自ら `git log <base>..HEAD` を確認した旨）をレビュー観点の一つとして確認し、
  未実施・未解消のまま P0 として承認しない
- マージ後に発見した場合は Issue で扱いを決め、history rewrite は原則行わない（判断記録: `docs/design/spec-pointer-in-commit-messages.md`）
