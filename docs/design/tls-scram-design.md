# ADR: TLS・SCRAM 実運用に関する設計調査（WIRE-9）

- ステータス: Accepted（TLS 1.3 サーバー側は TASK-228（親 Issue #941・
  分解 20 件）で自作実装済み。Issue #971 でセキュリティ監査を実施し、
  監査結果を下記「セキュリティ監査（#971）」節に記録した上で本 ADR を
  Accepted へ更新した。SCRAM 採否・チャネルバインディングの既定値は
  WIRE-18・`docs/design/tls-channel-binding.md` ポインタ）
- 対応: TASK-72（WIRE-9）・TASK-174（HTTP-10。WIRE-9 と同一方針）・
  TASK-228
- 関連: TASK-70（WIRE-7）・WIRE-18

## 背景

wire プロトコル層は PostgreSQL wire プロトコル v3 互換の自作実装であり、
psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済みである
（README.md「実装方針（要点）」）。非 loopback な環境へ配置する運用を
見据え、通信路の暗号化（TLS）と認証方式（SCRAM）の扱いを事前に整理する
本設計調査を行い、その後 TASK-228（親 Issue #941）で実装した。

`crates/wire-server` の bind ガード（`src/bind_guard.rs`。TASK-70・WIRE-7）が
`main.rs::run_server` の唯一の bind 経路であり、`--surface sql|nosql`
（TASK-171・HTTP-1）で選択する SQL 表層・NoSQL 表層のいずれも同じ経路を
共有する（下記「NoSQL 表層（HTTP-10）」節参照）。

## 論点

外部クレートを追加せず TLS 1.3 サーバー側を自作する方針は、依存最小方針
（[dependency-policy](../../.claude/rules/dependency-policy.md)）に基づく
オーナー判断であり、暗号プリミティブ自作の実装リスクは本 Issue（#971）の
セキュリティ監査実施を条件に受容済みである（判断根拠のポインタ:
`docs/spec/04-behavior/records/rdbms-parity-decision-2026-09-22.md` 項目3・
TASK-228）。採用範囲は X25519（鍵交換）・`TLS_AES_128_GCM_SHA256`（暗号
スイート）・Ed25519（証明書の唯一の受理鍵種別）の TLS 1.3 のみであり、
TLS 1.2 以下・0-RTT・セッション再開（PSK）・KeyUpdate・クライアント証明書
認証は対象外とする（詳細は下記「スコープ外」節）。

SCRAM 採否・チャネルバインディング（`tls-server-end-point`）の既定値は
WIRE-18・TASK-222 ポインタとし、`PLUS` 提示既定 `false`（オーナー判断
2026-09-26）の確定経緯は [`docs/design/tls-channel-binding.md`] を参照する。

WIRE-7（TASK-70）との関係: `TransportSecurity`（`bind_guard.rs`）が TLS
未構成時の非ループバック bind 拒否を担い、TLS 導入後もこの契約は不変。

## 影響

TLS 1.3 サーバー側は以下のモジュール群として実装済み
（`crates/wire-server/src/tls/`）。各実装記録 ADR へのポインタ:

| 領域 | モジュール | 実装記録 ADR |
| ---- | ---------- | ------------ |
| レコード層・レコード保護 | `record.rs`・`record_protection.rs` | [`tls-record-protection.md`] |
| ハンドシェイク codec・`ClientHello` | `handshake.rs`・`client_hello.rs` | [`tls-client-hello.md`] |
| 鍵交換（X25519） | `x25519.rs`・`field25519.rs` | [`tls-x25519.md`] |
| HKDF・鍵スケジュール | `hkdf.rs`・`key_schedule.rs` | [`tls-hkdf-key-schedule.md`] |
| AEAD（AES-128-GCM） | `aes.rs`・`aes_gcm.rs` | [`tls-aes.md`]・[`tls-aes-gcm.md`] |
| 署名（Ed25519） | `ed25519.rs`・`sha512.rs` | [`tls-ed25519.md`]・[`tls-sha512.md`] |
| 証明書・鍵ファイル | `x509.rs`・`der.rs`・`pem.rs`・`pkcs8.rs` | [`tls-pem-pkcs8.md`]・[`tls-x509-certificate.md`] |
| transcript・Finished | `transcript.rs`・`finished.rs` | [`tls-transcript-finished.md`] |
| サーバー側状態機械 | `server_handshake.rs` | [`tls-server-handshake.md`] |
| wire 接続結線（`SSLRequest`） | `stream.rs`・`wire_stream.rs` | [`tls-wire-connection.md`] |
| SCRAM チャネルバインディング | `channel_binding.rs` | [`tls-channel-binding.md`] |

CLI 結線（`--tls-cert`・`--tls-key`・`--tls-mode`）は `tls_opt.rs`、
NoSQL 表層への TLS 終端は `http/tls_transport.rs` が担う（下記「NoSQL
表層」節）。

## NoSQL 表層（HTTP-10）

- NoSQL 表層（`--surface nosql`・HTTP/1.1 最小サブセット。TASK-171／HTTP-1・
  HTTP-9）も SQL 表層と同じ通信路保護状態
  [`wire_server::bind_guard::TransportSecurity`] を共有する。
  `main.rs::run_server` は両表層共通の `GuardedBindAddrs::resolve`／`bind()`
  を通した**後**に accept ループのみを分岐する構造であり（Issue #735・
  PR #795）、bind 経路自体は表層ごとに分岐しない。
- 非ループバック運用では TLS を必須とする設計要件を、WIRE-9 と同一のまま
  NoSQL 表層へ適用する（HTTP-10）。TLS 未構成の間は
  `TransportSecurity::Cleartext` により、SQL 表層・NoSQL 表層のいずれを
  選んでも非ループバックアドレスへの bind は起動時に fail-closed で拒否
  される（WIRE-7・HTTP-9 の既存契約）。
- NoSQL 表層側の TLS 終端は `http/tls_transport.rs` が担う（先頭バイトで
  TLS レコードか平文 HTTP かを判定し、`--tls-mode` を SQL 表層と共有する。
  Issue #968）。SQL 表層と異なり SASL 往復を持たないため、SCRAM チャネル
  バインディング（`--tls-scram-channel-binding`）は NoSQL 表層では
  no-op として扱われる（[`docs/design/tls-channel-binding.md`] 参照）。
- 認証方式（SCRAM 採否）は通信路暗号化とは別軸として扱う点も WIRE-9 と
  同一とする。

## セキュリティ監査（#971）

TASK-228 で自作した TLS 1.3 実装（対象コミット: origin/main
`3726dfaf`）について、下記 2 観点を security-auditor 相当の読み取り専用
監査として実施した。

### A1: 秘密値依存の定数時間性

| 対象 | ファイル:関数 | 方式 | 判定 |
| ---- | -------------- | ---- | ---- |
| X25519 スカラー倍 | `x25519.rs`（Montgomery ladder）・`field25519.rs::cswap`/`select` | 固定 255 回ループ・マスクによる条件選択（分岐なし） | OK |
| X25519 中止条件 | `x25519.rs`（全ゼロ共有秘密判定） | RFC 7748 が要求する中止条件であり、結果（拒否可否）自体が仕様上の公開情報。秘密ビットへの分岐ではない | OK（既知の仕様上の性質。是正対象ではない） |
| Ed25519 スカラー倍 | `ed25519.rs::scalar_mul`（256 回固定 double-and-add-always）・`EdwardsPoint::select` | マスクによる条件選択 | OK |
| Ed25519 スカラー mod L | `ed25519.rs`（`conditional_sub_l` 等） | マスク方式 | OK |
| AES S-box | `aes.rs::sbox_bitsliced` | ブールビットスライス回路。本番コードにテーブル参照なし（256 要素の参照 S-box は `#[cfg(test)]` 内のみ） | OK |
| AES MixColumns | `aes.rs`（`xtime`） | マスク実装 | OK |
| GHASH | `aes_gcm.rs::gf128_mul` | ビット直列 shift-and-add（秘密依存の分岐・添字なし）。分岐ありの `gf128_mul_reference` はテスト専用 | OK |
| AEAD タグ検証順序 | `aes_gcm.rs::open`（203 行目付近） | `ct_eq` によるタグ比較が成功した場合のみ復号結果を返す（検証前復号なし） | OK |
| Finished 比較 | `finished.rs::verify_client_finished` | `hkdf::ct_eq` による比較 | OK |
| 定数時間比較の基盤 | `hkdf.rs::ct_eq` | OR 畳み込み＋`black_box`。長さ不一致は早期 return するが、到達するのは GCM の短い入力・Finished の固定 32 バイトという公開長のみ | OK |
| レコードパディング除去 | `record_protection.rs`（`ct_select_*` 系） | 内容型パディング除去を定数時間走査で実施 | OK |
| SCRAM proof 比較（参考） | `auth/scram.rs` | `ct_eq` 相当の定数時間比較 | OK |

### A2: untrusted 長さフィールドの上限検証

| 対象 | ファイル | 検証内容 | 判定 |
| ---- | -------- | -------- | ---- |
| TLS レコード長 | `record.rs`（`MAX_CIPHERTEXT_LEN`／`MAX_PLAINTEXT_LEN`） | 確保・`read_exact` の前に上限と照合 | OK |
| ハンドシェイクメッセージ長 | `handshake.rs`（`MAX_HANDSHAKE_WIRE_BODY_LEN`／`MAX_HANDSHAKE_MESSAGE_LEN`） | 24 ビット長を確保前にクランプ・再組み立てバッファに上限 | OK |
| `ClientHello` 拡張ベクタ長 | `client_hello.rs` | 各ベクタ長を残りバッファ長と照合。TLS 1.3 以外は fail-closed 拒否。未知拡張は RFC 8446 §4.2/§4.1.2 のとおり中身を見ずに無視して受理する（拒否ではない。長さ検証自体は行われるため未検証アロケーションにはならない） | OK |
| DER 長 | `der.rs`（`MAX_DER_NESTING_DEPTH`） | long-form 長のオクテット数上限・checked 演算・不定長形式の拒否 | OK |
| 証明書 DER 長 | `x509.rs`（`MAX_CERTIFICATE_DER_LEN`） | 確保前に照合 | OK |
| PEM/PKCS#8 | `pem.rs`・`pkcs8.rs` | ファイル長・ブロック数上限 | OK |
| レコード保護鍵の使用上限 | `record_protection.rs`（`MAX_RECORDS_PER_KEY` = 2^24） | KeyUpdate 非対応のため上限到達で接続クローズ | OK |
| 受信バッファ全般 | `stream.rs`・`wire_stream.rs`・`http/tls_transport.rs` | 読み取りバッファ・ハンドシェイク中の資源上限 | OK |
| untrusted 経路の `unwrap`/`expect`/添字 | `src/tls/**`（非テストコード） | 機械的走査（grep）で確認した範囲では、本番コード中の `unwrap`/`expect` 呼び出しはすべて `#[cfg(test)]` モジュール内に限定される | OK（網羅的な静的解析ツールによる検証ではない点は下記「受容リスク」参照） |

その他確認: テナント境界・`wire_code`（ERR-1/2/4）契約は TLS 層の追加で
変わっていない。alert・エラー応答に鍵・証明書内容を含めない。

**総合判定**: 上記範囲で P0 欠陥は検出されなかった。

## 受容リスク

暗号プリミティブを自作する実装リスクは、本監査（#971）の実施を条件に
オーナー判断（2026-09-22）で受容済み
（`docs/spec/04-behavior/records/rdbms-parity-decision-2026-09-22.md`
項目3 ポインタ）。加えて、各モジュールのドキュメンテーションコメントに
既に記載されている以下の残存リスクをここに集約する。

- ゼロ化（鍵材料の破棄）は `write_volatile` を用いない best-effort である
- PEM/base64 の鍵デコードはテーブル駆動実装だが、起動時に高々 1 回しか
  実行されないため untrusted 入力の連続処理経路ではない
- `u64`/`u128` の AND/XOR/シフト/乗算が x86_64・aarch64 の対象命令セットで
  定数時間であることを前提としている（コンパイラ・マイクロアーキテク
  チャの実装依存）
- 第三者による暗号実装レビュー・形式検証は未実施
- GHASH はビット直列実装であり、性能よりも定数時間性を優先している
- untrusted 経路の `unwrap`/`expect`/添字禁止の確認は grep ベースの
  機械的走査によるものであり、`clippy::indexing_slicing` 等の deny
  属性によるコンパイル時強制ではない

## スコープ外

- クライアント証明書認証
- セッション再開（PSK・0-RTT）
- KeyUpdate（`MAX_RECORDS_PER_KEY` 到達時は再鍵更新せず接続を閉じる）
- TLS 1.2 以下
- SHA-384 系署名アルゴリズム（`aes-gcm` 等と異なり自作 SHA-384 は未実装）
- HTTPS 表層（NoSQL）へのチャネルバインディング適用
- HTTP-10 の確定化判定そのもの（spec 側・TASK-174 系の後続タスク）
- `docs/spec` 側ドキュメントへの反映（spec リポジトリ側の作業）

## 参照

- `docs/spec/05-tasks.md`（TASK-72・TASK-70・TASK-174・TASK-228）
- `docs/spec/04-behavior/wire-protocol.md`（WIRE-9・WIRE-7・WIRE-18）
- `docs/spec/04-behavior/http-transport.md`（HTTP-9・HTTP-10）
- `docs/spec/04-behavior/records/rdbms-parity-decision-2026-09-22.md`
- `docs/spec/06-roadmap.md`（MS-3）
- `crates/wire-server/src/bind_guard.rs`
- `crates/wire-server/src/http/listener.rs`・`http/tls_transport.rs`
- `crates/wire-server/src/tls/`（実装本体）
- [`docs/design/tls-channel-binding.md`]
- Issue #941（親）・Issue #971（本監査）

[`docs/design/tls-channel-binding.md`]: ./tls-channel-binding.md
[`tls-record-protection.md`]: ./tls-record-protection.md
[`tls-client-hello.md`]: ./tls-client-hello.md
[`tls-x25519.md`]: ./tls-x25519.md
[`tls-hkdf-key-schedule.md`]: ./tls-hkdf-key-schedule.md
[`tls-aes.md`]: ./tls-aes.md
[`tls-aes-gcm.md`]: ./tls-aes-gcm.md
[`tls-ed25519.md`]: ./tls-ed25519.md
[`tls-sha512.md`]: ./tls-sha512.md
[`tls-pem-pkcs8.md`]: ./tls-pem-pkcs8.md
[`tls-x509-certificate.md`]: ./tls-x509-certificate.md
[`tls-transcript-finished.md`]: ./tls-transcript-finished.md
[`tls-server-handshake.md`]: ./tls-server-handshake.md
[`tls-wire-connection.md`]: ./tls-wire-connection.md
