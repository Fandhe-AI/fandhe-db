# AES-128 ブロック暗号（定数時間）実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #957・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）が対象とする唯一の暗号
スイート `TLS_AES_128_GCM_SHA256` は、鍵ストリーム生成・タグ計算に
AES-128 を用いる。GCM の CTR 鍵ストリームと `H = E_K(0^128)`（GHASH
鍵）はどちらも AES-128 の**暗号化方向**だけで足りるため、本 Issue は
暗号化のみを実装する（`crates/wire-server/src/tls/aes.rs`）。復号
（`InvSubBytes`／`InvMixColumns`／`InvCipher`）・GCM/GHASH 本体
（#958）・レコード保護（#959）・接続への結線（#966 以降）はいずれも
対象外。

## 採用方式: GF(2^8) 逆元のビットスライス計算（T-table 不採用）

FIPS 197 の 256 要素 S-box テーブルを本番コードへ置くと、参照添字が
秘密値（平文・鍵に依存する中間値）になり、キャッシュタイミング攻撃の
対象になる（T-table 方式の既知の弱点）。本実装は S-box を次の 2 段の
ブール回路として計算し、秘密値に依存する分岐・配列添字を一切使わない:

1. **GF(2^8) 乗法逆元 `x^254`**: 状態を 64 バイト（4 ブロック分）
   まとめてビットプレーン表現 `[u64; 8]`（プレーン `j` のビット `i` は
   バイト `i` のビット `j`）へ転置し、プレーン上の多項式乗算
   （`0x11b` = `x^8+x^4+x^3+x+1` で mod 還元）を実装した
   `gf28_mul_bitsliced` を、左から右への二乗・乗算の固定連鎖
   （指数 254 = `0b11111110` は公開定数）で 8 回の二乗（初回は
   乗法単位元 `1` の二乗）・7 回の乗算に展開して計算する。
2. **アフィン変換**: FIPS 197 §5.1.1 の定数 `0x63` によるアフィン
   変換を、同じビットプレーン表現へビット単位の XOR で適用する。

当初の設計候補（Boyar–Peralta の公知回路。113 ゲート）ではなく、
計画に記載した代替方式（GF(2^8) 逆元をプレーン上の多項式乗算で計算し
アフィン変換を掛ける）を採用した。理由は、外部回路の正確なゲート列を
記憶から転記すると転記ミスのリスクが高く、多項式乗算＋固定加算連鎖の
方式は数学的に導出可能でテスト（256 入力全数照合）による検証も
容易だったため。

鍵展開の `SubWord` も同じ `sbox_apply_bytes`（ビットプレーン変換 +
`sbox_bitsliced` の唯一の入口）を経由させ、鍵展開にもテーブル参照を
残さない。

256 要素の参照 S-box テーブルは本番コードに一切存在しない。テストの
`reference_sbox` は GF(2^8) 逆元を素朴な繰り返し二乗なしの逐次乗算
（253 回。速度は問わずテスト専用）で総当たりに求め、アフィン変換を
掛けて独立に導出し、`sbox_bitsliced` と 256 入力全数照合する
（`aes.rs::tests::sbox_bitsliced_matches_reference_for_all_256_inputs`）。

## ShiftRows・MixColumns

- **ShiftRows**: 状態を列優先（FIPS 197 §3.4。バイト添字 `c*4+r`）で
  持ち、行 `r` を `r` バイト左に巡回シフトする。添字はすべて公開の
  ループカウンタのみで決まる固定置換。
- **MixColumns**: `xtime(b) = (b << 1) ^ (0x1b & 0u8.wrapping_sub(hi))`
  （`hi` は最上位ビット）という分岐なしのマスク演算で実装する。

## 定数時間の前提

`u64`／`u8` の AND・XOR・シフト演算は x86_64・aarch64 で定数時間で
あるとみなす（`tls/field25519.rs`・`tls/x25519.rs` と同じ前提）。
指数 254 のビット列・アフィン定数 `0x63`・還元多項式の折り返し先添字は
いずれも公開の固定値であり、`if` 分岐はこれら公開定数にのみ依存する
（秘密値のビットに依存する分岐・配列添字は存在しない）。

## 全体のラウンド構成

SubBytes 段だけがビットスライス表現を必要とするため、他の段
（ShiftRows・MixColumns・AddRoundKey）はバイト表現のまま計算する
設計とした。1 ラウンドごとに 64 バイト（4 ブロック）をビットプレーンへ
転置・逆転置するオーバーヘッドが生じるが、本 Issue の受け入れ基準は
定数時間性とテストベクタ一致であり、性能は手動ベンチによる参考値に
留める方針（後述）と整合する。

ラウンド構成は AddRoundKey → 9 ラウンド（SubBytes/ShiftRows/
MixColumns/AddRoundKey）→ 最終ラウンド（MixColumns なし）の 10
ラウンド固定（AES-128）。`encrypt_block`（1 ブロック）は
`encrypt_blocks4`（4 ブロック並列。GCM CTR モードでの利用を想定）の
lane 0 だけを使うラッパーとし、コードパスを 1 本にまとめている。

## 鍵スケジュールの往復の解釈

暗号化専用の実装であるため、本番コードには復号・逆鍵展開を追加しない。
「往復」の検証は次の 2 つで行う:

1. 正方向の展開が FIPS 197 Appendix A.1 の全 44 ワードと一致すること
   （`key_expansion_matches_fips197_appendix_a1`）。
2. テスト専用（`#[cfg(test)]`）の逆鍵展開により、最終ラウンド鍵から
   元の 128 ビット鍵を復元できること（AES-128 の鍵スケジュールは
   `w[i] = w[i-4] ^ f(w[i-1])` の形で可逆。
   `key_schedule_round_trips_via_test_only_inverse_expansion`）。

`wipe`（`Drop` 相当）で全ラウンド鍵がゼロ化されることも
`wipe_zeroes_all_round_key_bytes` で固定する。

## ゼロ化の限界

`Aes128` は `Drop` でラウンド鍵を `tls::hkdf::zeroize`（`pub(crate)`）
で best-effort にゼロ化する。`unsafe`（`write_volatile` 等）を使わない
ため、最適化により消去が省略されない保証はない
（`tls::hkdf::Secret32`・`tls::key_schedule::TrafficKeys` と同じ限界）。

## テストベクタの出典

- FIPS 197 Appendix A.1（AES-128 鍵展開の全 44 ワード）
- FIPS 197 Appendix B（暗号化の計算例。鍵 `2b7e151628aed2a6abf7158809cf4f3c`）
- FIPS 197 Appendix C.1（鍵 `000102...0f`、平文 `00112233...ff`）
- NIST SP 800-38A F.1.1（ECB-AES128.Encrypt。4 ブロック）
- 参考（informational）: 全 0 鍵・全 0 平文での `E_K(0^128)` は
  `#958` の GHASH 鍵 `H` 計算の先行確認に使う値として記録する
  （production コードのテストとしては固定値アサーションあり）。

いずれも一次資料（規格文書）に記載の値をテストへ直接転記し、記憶から
の再構成には頼らない。

## スループット参考値（informational）

`#[ignore]` の手動専用テスト
`aes.rs::tests::aes128_throughput_reference` を追加した。実行方法:

```sh
cargo test -p fandhe-db-wire-server --release \
  aes128_throughput_reference -- --ignored --nocapture
```

本開発環境（共有 QEMU）での実測（4 並列 × 200,000 回を 3 回実行）:

```text
run 1: 12800000 bytes in 1.183117621s (10.82 MB/s)
run 2: 12800000 bytes in 1.193262600s (10.73 MB/s)
run 3: 12800000 bytes in 1.235809299s (10.36 MB/s)
```

`docs/design/benchmark-judgement-policy.md` の方針どおり、この値は
共有環境の参考値であり、方式採否の根拠にはしない。1 ラウンドごとに
ビットプレーン転置・逆転置を行う設計上、AES-NI 命令や T-table 方式に
比べて大幅に遅いことが想定されるが、本 Issue の受け入れ基準は
定数時間性とテストベクタ一致であり、性能改善は対象外（必要であれば
別 Issue で検討する）。

## 対象外・申し送り

- 復号（`InvSubBytes`／`InvMixColumns`／`InvCipher`）は実装しない
  （親 Issue #941 の方針どおり暗号化方向のみ）
- GCM/GHASH 本体・AEAD としての結線は `#958` の担当
- レコード保護（暗号化されたレコードの送受信）は `#959` の担当
- 接続ハンドシェイクへの結線は `#966` 以降の担当
- 性能改善（ラウンドごとの転置オーバーヘッド削減・AES-NI 等の
  ハードウェアアクセラレーション）は本 Issue の対象外。必要であれば
  別 Issue で実測に基づいて検討する
