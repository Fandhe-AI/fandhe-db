# ADR: 復元の世代番号の決め方と参照の構文（ポインタ記録）

- ステータス: Accepted（#2448 で永続化と物理の復元、#2449 で参照構文と変更の拒否を実装。#2450 で PITR の完了時の進行を確認（物理と共通の完了点）。#2451 は未実装）
- 対応 Issue: #2447（親 #2446）。後続: #2448（永続化・物理の復元）・#2449（参照構文・変更拒否）・
  #2450（PITR）・#2451（論理の復元）
- ビヘイビア ID ポインタ: RECOVER-17（`docs/spec/04-behavior/recovery.md`）、
  関連 RECOVER-13・RECOVER-14・RECOVER-16、TASK-340（`docs/spec/05-tasks.md`）

## 位置づけ

本書は public リポのため、世代番号の決め方・永続化の順序・エラー契約・参照構文の比較と不採用理由の
詳細を載せない。これらの本文は private spec（上記ポインタ）を正とし、
[spec-confidentiality](../../.claude/rules/spec-confidentiality.md) に従って転記しない。
確定した契約は後続 Issue（#2448〜#2451）の実装とテストで具体化し、実装記録は
[implementation-status.md](implementation-status.md) に追記する。

## 参照先

- 契約の本文は private spec の RECOVER-17 を正とする（関連 RECOVER-13・RECOVER-14・RECOVER-16、TASK-340）
- 公開範囲を広げる場合は、管理者の承認を得てから本書へ追記する

## 実装で確定した参照構文（Issue #2449）

- 参照は `SHOW restore_generation`（列名 `restore_generation`・`bigint`・1 行。全テナントで同じ値）
- 変更は `SET [SESSION|LOCAL] restore_generation ...` と `RESET restore_generation` を構文段で
  `42501`（固定文言）に拒否する。StartupMessage の同名パラメータは他の未知パラメータと同様に読み捨てる
- 世代を読めない場合は `XX000`（固定文言）で拒否し、値を丸めない（fail-closed）
- 取得時点の commit 番号は返さない（RECOVER-13 のポインタ）。NoSQL（HTTP）表層からの参照は対象外

## 検証担当

| 親 #2446 の目標 | 検証する Issue |
| ---- | ---- |
| 永続化と物理の復元での進行 | #2448 |
| 参照構文と変更の拒否 | #2449 |
| PITR の完了時の進行 | #2450 |
| 論理の復元の完了時の進行 | #2451 |
