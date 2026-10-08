# SHA-512（Ed25519 用）実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #960・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）が採用する唯一の暗号スイート
`TLS_AES_128_GCM_SHA256` は、トランスクリプトハッシュ・HKDF に
SHA-256（[`hkdf.rs`]・`engine::crypto::sha256`）だけを使う。一方、後続の
Ed25519 署名生成・検証（Issue #961・RFC 8032）は、鍵アルゴリズム自体の
仕様として SHA-512 を要求する（秘密鍵の展開 `SHA-512(seed)`・署名計算
`SHA-512(prefix || M)`／`SHA-512(R || A || M)`）。既存の自作ハッシュは
SHA-256 のみだったため、本 Issue で SHA-512 を新規に実装する。

## 置き場所の判断: `engine::crypto` ではなく `wire-server::tls`

`engine::crypto::sha256` は SCRAM-SHA-256 認証（Issue #940・WIRE-18）の
下請けとして engine クレートの公開 API へ切り出したものであり、
`wire-server` 側の複数モジュールから再利用されている。しかし SHA-512 は
TLS 1.3 自作実装（親 Issue #941）の Ed25519 専用の要求であり、engine
クレート側に利用箇所が無い。Issue 本文の指定どおり、他の TLS 構成要素
（`aes.rs`・`hkdf.rs`・`x25519.rs` 等）と同じ `crates/wire-server/src/tls/`
配下に `sha512.rs` として置く。

## 採用方式: `engine::crypto::sha256` と同じストリーミング構成の 64 ビット化

`engine::crypto::sha256`（Issue #399 のストリーミング化・16 語ローリング
メッセージスケジュール方式）とまったく同じ構成を、32 ビット語→64 ビット語・
64 バイトブロック→128 バイトブロック・80 ラウンドへ機械的に置き換えて
再構成した:

- メッセージスケジュールは 80 語配列ではなく `w: [u64; 16]` のローリング
  バッファ（`w[t & 15]`）で保持する。
- ブロックバッファは固定長スタック配列 `[u8; 128]` のみで、入力長に
  比例したヒープ確保は行わない。
- 加算はすべて `wrapping_add`（FIPS 180-4 が定める mod 2^64 加算そのもの。
  未定義動作にはならない）。
- `total_len` は SHA-512 が `< 2^128` ビットのメッセージを扱う仕様
  （SHA-256 の `< 2^64` ビットより広い）に合わせて `u128` で保持する。

`absorb` 内で `engine::crypto::sha256::absorb` にあった
`let block = self.buffer; compress(&mut self.state, &block);` という
スタックコピーは作らず、`compress(&mut self.state, &self.buffer)` と
フィールドを直接借用して渡す（フィールドの分離借用でコンパイル可能）。
これは秘密入力のコピーを 1 つ減らす設計判断であり、`compress` 内の
メッセージスケジュール `w` も関数を抜ける前に best-effort でゼロ化する。

## 公開 API

`Sha512::new`／`update`／`finalize`・一括ヘルパー `digest`・定数
`DIGEST_LEN`（64）・`BLOCK_LEN`（128）。

### `Clone` を実装しない判断

`engine::crypto::sha256::Sha256` は HMAC-SHA-256（Issue #940・WIRE-18）が
ipad/opad 適用後の状態を複製して使い回すため `Clone` を実装しているが、
`Sha512` は `Clone` を実装しない。Ed25519（#961）の秘密鍵展開・署名計算は
いずれも状態の複製を必要とせず、[`hkdf::Secret32`]・
[`x25519::SharedSecret`] が「不用意な複製を防ぐ」ために `Clone` を持たない
方針と揃える判断とした。この設計は #961 へそのまま引き継がれる想定。

### `Drop` とゼロ化

`Sha512` は `Drop` で内部状態（`state`・`buffer`・`buffered`・
`total_len`）を best-effort にゼロ化する。`Drop` を実装しているため
`finalize(mut self)` はフィールドを move できないが、`state` 等は `Copy`
なので参照経由で読み出したうえで関数を抜け、その時点で `Drop` が走って
ゼロ化される（`finalize` 経路でも状態が消される）。戻り値のダイジェスト
`[u8; 64]` 自体のゼロ化は呼び出し側（#961 の秘密鍵展開）の責務とし、
本モジュールは関与しない。

`[u64; N]` のゼロ化には `tls::hkdf::zeroize`（`&mut [u8]` 専用）を使えない
ため、モジュール内 private の `zeroize_u64` を新設した（`hkdf.rs` は
無変更）。`buffer: [u8; 128]` は既存の `tls::hkdf::zeroize` をそのまま
再利用する。

`Debug` は内部状態を出さず `<redacted>` とする（`Aes128` と同じ方針）。

## 定数時間性の根拠

秘密値に依存する分岐・添字は無い。`K` の添字はラウンドカウンタ `t`
（呼び出し時点で公開の固定値）であり、`K.get(t).copied().unwrap_or(0)`
の形は `engine::crypto::sha256::compress` と同じ（`0..80` の範囲内で
`None` に到達することは無い、フォールバックは untrusted 入力経路の
安全側デフォルト）。`absorb`／`finalize` の分岐（`buffered <= 112` か
どうか等）は公開情報である入力長・累積バイト数にのみ依存し、秘密値
（ハッシュ対象のバイト内容）には依存しない。ローテート・XOR・AND・
`wrapping_add` だけで構成し、S-box のようなテーブル参照による置換は
使わない。

## ゼロ化の限界

`unsafe`（`write_volatile` 等）を使わないため、最適化により消去が
省略されない保証はない（`tls::hkdf::Secret32`・`tls::aes::Aes128` と
同じ限界）。

## 定数・テストベクタの出典

80 個の `K` と 8 個の `H0` は記憶から書かず、公開文書である RFC 6234
（`https://www.rfc-editor.org/rfc/rfc6234.txt`。§5.2「SHA-384 and
SHA-512」・§6.3「SHA-384 and SHA-512 Initialization」）から取得した本文を
そのまま転記した。テストベクタ（空入力・`abc`・112 バイト 2 ブロック・
`a` × 1,000,000）の期待値も同じ RFC 6234 §8.5（`TEST1`／`TEST2_2`／
`TEST3`）に記載の値を転記している。RFC 6234 は SHA-2 ファミリー・
HMAC-SHA・HKDF の公開仕様であり、本リポの private spec
（fandhe-db-spec）とは無関係の一次資料である。

## テスト一覧

### 単体テスト（`sha512.rs` 内 `#[cfg(test)]`）

- FIPS/RFC 公開ベクタ 4 件（空入力・`abc`・112 バイト 2 ブロック・
  `a` × 1,000,000）
- 境界長（`0, 1, 111, 112, 113, 127, 128, 129, 239, 240, 255, 256, 257,
  1000`）での `digest` と参照実装（一括処理版）の等価性
- 分割 `update` の等価性（チャンクサイズ
  `1, 3, 7, 16, 64, 111, 112, 127, 128, 129, 200`）
- 境界長（111/112/127/128）の入力をあらゆる分割点で 2 回の `update` に
  分けても一致すること（1 ブロックに収まるか 2 ブロックになるかの
  分かれ目を網羅）
- `wipe` によるゼロ化（`state`／`buffer`／`buffered`／`total_len` の
  全フィールドがゼロになること）
- `Debug` 出力に内部状態が含まれず `<redacted>` を含むこと

### 結合テスト（`crates/wire-server/tests/tls_sha512_vectors.rs`）

公開 API（`Sha512`・`digest`・`DIGEST_LEN`・`BLOCK_LEN`）だけを使い、
上記のベクタ 4 件・境界長 111/112/127/128 の分割 `update` 等価性・
`Debug` 秘匿を外部から見える契約として固定する。

加えて、パディング境界長（`'a'` を 111/112/113/127/128/129/239/240/256
バイト並べた入力）について、独立実装（coreutils `sha512sum`・Python
`hashlib.sha512`。両者一致を確認済み）の出力を期待値とし、一括 `digest`
と 1 バイト刻みの `update` の双方を照合する。単体テストの参照実装は
同じ `compress` を共有するため、パディング処理の誤りを参照実装とは独立に
検出する目的で置く。

## 対象外・申し送り

- SHA-384・SHA-512/256・SHA-512/224・HMAC-SHA-512（採用する暗号スイート
  `TLS_AES_128_GCM_SHA256` が要求しないため）
- Ed25519 本体（署名生成・検証。#961）
- トランスクリプトハッシュへの結線（#964。SHA-256 のまま不変）
- 接続への結線（#966 以降）
- engine クレート（`engine::crypto`）への配置・移設（本 Issue の時点で
  利用箇所が `wire-server::tls` に限られるため対象外）
