# ADR: ストリーミングレプリケーションの方式（ポインタ記録）

- ステータス: Proposed（実装は #2173〜#2177・#2245。本書の追加でコードは変えない）
- 対応 Issue: #2172（親 #2171）。後続: #2173・#2174・#2175・#2176・#2177・#2245。運用 doc は #2179、障害注入は #2178
- ビヘイビア ID ポインタ: RECOVER-15（`docs/spec/04-behavior/recovery.md`）、
  関連 RECOVER-2・RECOVER-13・RECOVER-14・RECOVER-17、WIRE-9・WIRE-18、TASK-258

## 位置づけ

本書は public リポのため、方式の比較・同期確認の段階と既定動作・待機上限・昇格時の契約・
履歴の検証・認証と TLS の詳細を載せない。これらの本文は private spec（上記ポインタ）を正とし、
[spec-confidentiality](../../.claude/rules/spec-confidentiality.md) に従って転記しない。
公開してよい範囲は README「実装方針（要点）」で公開済みのものに限る。
確定した契約は後続 Issue の実装とテストで具体化し、実装記録は
[implementation-status.md](implementation-status.md) に追記する。

## 参照先

- 契約の本文は private spec の RECOVER-15 を正とする（関連 RECOVER-2・RECOVER-13・RECOVER-14・RECOVER-17、WIRE-9・WIRE-18、TASK-258）
- 変更ログの ADR（#2165）とは、着手時に整合を確認する
- 公開範囲を広げる場合は、管理者の承認を得てから本書へ追記する

## 検証担当

| 親 #2171 の目標 | 検証する Issue |
| ---- | ---- |
| 送信側 endpoint | #2173 |
| 受信側の適用ループ | #2174 |
| 読み取りレプリカ | #2175 |
| 保持と再同期 | #2176 |
| 認証と TLS | #2177 |
| 同期モード | #2245 |
| 障害注入 | #2178 |
| 運用 doc | #2179 |
