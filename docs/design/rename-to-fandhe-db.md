# プロジェクト名の fandhe-db への変更

- ステータス: Accepted
- 決定日: 2026-10-08

## 背景

プロジェクト名を vector-db から fandhe-db へ変更する。名称は GitHub リポジトリ・spec リポジトリ・crate 名・環境変数・ドキュメントに現れるため、何を新名へ移し何を据え置くかを一箇所に記録する。

## 決定

プロジェクト名を **fandhe-db** とする。新旧の対応は次のとおり。

| 対象 | 旧 | 新 |
| ---- | -- | -- |
| GitHub リポジトリ | `Fandhe-AI/vector-db` | `Fandhe-AI/fandhe-db` |
| spec リポジトリ（private） | `Fandhe-AI/vector-db-spec` | `Fandhe-AI/fandhe-db-spec` |
| crate 名（engine） | `fandhe-vector-db-engine` | `fandhe-db-engine` |
| crate 名（wire-server） | `fandhe-vector-db-wire-server` | `fandhe-db-wire-server` |
| 環境変数の接頭辞 | `VECTOR_DB_` | `FANDHE_DB_`（改名は別 PR で実施。移行手順は README に記載） |

crate 名は `build!: crate 名を fandhe-db-engine / fandhe-db-wire-server へ変更` で変更済み。本 ADR と同時の PR はドキュメント・コメント上の表記だけを対象とする。

## 変えないもの

| 対象 | 理由 |
| ---- | ---- |
| lib 名 `engine` / `wire_server` と bin 名 `wire-server` | 利用箇所（`use` パス・ベンチスクリプトの照合等）が膨大で、package 名から独立している。変えても利用者に得がない |
| submodule パス `docs/spec` | `ai-review.yml` の許可判定がこのパスに依存している。リポジトリ名の変更とは無関係に維持する |
| 永続化 domain tag（`vector-db/op_ledger/...`・`vector-db/scram/...`） | 変えると既存 redb の recovery 整合性（op_ledger）と SCRAM ダミー応答の決定性が崩れる。名称は識別子であり、プロダクト名の表記とは別物として据え置く |
| 履歴記録（CHANGELOG.md・`implementation-status.md` の既存行・`bench-data/`・日付付きの過去の判断記録） | 当時の名前で書かれた記録であり、書き換えると事実と食い違う |
| Issue／PR／Actions run の URL | GitHub が旧リポジトリ名からリダイレクトするため、置換しない |

## 旧 crate 0.1.0 の扱い

旧名 `fandhe-vector-db-engine` / `fandhe-vector-db-wire-server` の 0.1.0 は crates.io に**残す**。yank はせず、以後の更新もしない。0.2.0 以降は新名のみで公開する。旧名の利用者は README「旧名からの移行」節に従い依存名を差し替える。

## 注意

- `Fandhe-AI/vector-db` の名前で新しいリポジトリを作らない。作ると GitHub の旧名からのリダイレクトが消え、既存の Issue／PR／clone の URL が壊れる
- spec Issue へのポインタ（`vector-db-spec#n`）は、Issue 番号が改名後も変わらないため `fandhe-db-spec#n` へ置き換えた（CHANGELOG・`implementation-status.md` の既存行など履歴記録は除く）
