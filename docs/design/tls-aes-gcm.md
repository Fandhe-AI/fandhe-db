# AES-128-GCM（GHASH 定数時間・AEAD）実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #958・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）が対象とする唯一の暗号
スイート `TLS_AES_128_GCM_SHA256` の AEAD 本体（NIST SP 800-38D）を、
`#957` で実装済みの [`tls::aes::Aes128`]（暗号化方向のみ・定数時間）の
上に構築する。CTR 鍵ストリーム生成・`H = E_K(0^128)` の計算に
`Aes128::encrypt_block`／`encrypt_blocks4` をそのまま使い、GCM 独自の
暗号プリミティブは追加しない。per-record nonce（iv XOR seq）の導出・
`TLSInnerPlaintext` の内容型パディング・レコード保護本体は
`tls::key_schedule`・`#959` の担当であり、本 Issue は AEAD の
`seal`／`open`・タグ検証までに閉じる。

## 採用方式: ビット直列 GHASH（テーブル方式不採用）

GHASH の GF(2^128) 乗算は、SP 800-38D Algorithm 1 のビット直列
shift-and-add をそのまま実装する（`aes_gcm.rs::gf128_mul`）。

- 4 ビット／8 ビットのテーブル方式（Shoup 法）は、添字が秘密値 `H`
  （TLS セッション鍵から導出される値）に依存し、キャッシュタイミング
  攻撃の対象になるため不採用。
- BearSSL の `ctmul64` 型（穴あき整数乗算・Karatsuba 分割）は性能面の
  後続候補として認識しているが、`#957` の AES S-box 実装と同じ理由
  （外部方式を記憶から転記する誤りのリスクを避け、導出・検証が容易な
  方式を優先する）で今回は採らなかった。

`gf128_mul` はループ 128 回固定で、秘密値（`H`・暗号文由来のブロック）
に依存する分岐・配列添字・整数乗算は存在しない。条件分岐（アキュムレー
タへの XOR・還元多項式の条件付き XOR）はすべてマスク演算
（`0u64.wrapping_sub(bit)`）に置き換えている。ループカウンタ `i` に
よる `i < 64` の分岐は公開値（呼び出し時点で確定するループ回数）にのみ
依存するため、定数時間性を損なわない。

## ビット順の注意（実装時に踏んだ落とし穴）

GCM の GF(2^128) 表現は反射順（reflected order）で、ブロック先頭バイト
の最上位ビットが多項式の定数項（x^0）の係数になる。本実装は 16 バイト
ブロックをビッグエンディアンで `(hi, lo)` の 2 語表現へ読み込み、
`gf128_mul` は `hi` の最上位ビットから消費する。乗法単位元
（反射表現では先頭バイトが `0x80` の block）・可換性・分配則といった
`gf128_mul` 単体の代数的性質は、ビット順を取り違えていても成立して
しまうため、これらのテストだけでは検出できない。実際にビット順が
合っているかは公開テストベクタとの照合（後述）でのみ確認できる。

## 処理順序: 検証してから復号する

`Aes128Gcm::open` は次の順序を厳守する:

1. AAD 長・入力長（`[TAG_LEN, MAX_CIPHERTEXT_LEN]`）を検証する（確保前）
2. 入力を `(暗号文, タグ)` に分割する（`split_at_checked`。添字直指定なし）
3. `GHASH(H, A, C) ^ E_K(J0)` で期待タグを計算する（**この時点では
   平文バッファを一切確保・生成していない**）
4. `tls::hkdf::ct_eq`（定数時間比較）でタグを検証する
5. 不一致なら `Err(AeadError::TagMismatch)` を返す（復号は行わない）
6. 一致した場合に限り、平文バッファを確保して CTR 復号する

「先に復号してから検証し、失敗時にゼロ化する」順序は採らない。GCM の
タグは暗号文に対して計算されるため、検証を先に行えば、改ざんされた
暗号文から平文が構造的に一度も生成されない（復号オラクルを作らない）。

## 長さ上限

| 定数 | 値 | 根拠 |
| ---- | -- | ---- |
| `MAX_AAD_LEN` | 64 バイト | TLS 1.3 の AAD は 5 バイトのレコードヘッダ（`tls::record::RECORD_HEADER_LEN`）だが、公開テストベクタ（Test Case 4）には 20 バイトの AAD があるため、それを収める最小限の余裕として 64 とした |
| `MAX_PLAINTEXT_LEN` | `tls::record::MAX_CIPHERTEXT_LEN - TAG_LEN` | レコード 1 件に収まる `TLSInnerPlaintext` の上限 |
| `open` の入力長 | `[TAG_LEN, tls::record::MAX_CIPHERTEXT_LEN]` | 暗号文＋タグがレコード 1 件の暗号文上限に収まる範囲 |

`const _: () = assert!(...)` により、ブロック数が 32 ビットカウンタ
（`inc32`）の上限（`2^32 - 2`）を大きく下回ること、バイト長からビット長
への換算（`×8`）が `u64` で溢れないことを静的に固定する（`aes_gcm.rs`
冒頭の const assertion 群）。

## nonce（96 ビット固定）

`Aes128Gcm::seal`／`open` は nonce を `&[u8; 12]`（96 ビット）に固定
する。GHASH から `J0` を導出する 96 ビット以外の nonce の経路（SP
800-38D の Test Case 5・6 が対象とする形）は実装しない。TLS 1.3 は
常に 12 バイト nonce を使うため、この範囲に限定して問題ない。

**nonce の一意性は呼び出し側（`#959` のシーケンス番号管理）の契約**
であり、本モジュールは強制しない（強制するには呼び出し履歴の保持が
必要になり、AEAD プリミティブ自体の責務を超えるため）。

## タグ検証: `tls::hkdf::ct_eq` の共有

秘密値どうしの比較を 1 箇所（`tls::hkdf::ct_eq`）に集約し、GCM のタグ
検証（本 Issue）と Finished の verify_data 検証（`#964`）の双方が同じ
実装を共有する。長さが異なる場合は早期 return する（長さは公開値の
ため定数時間性を損なわない）。長さが等しい場合は全バイトを OR 畳み込み
で比較し、一致した時点で打ち切る分岐を持たない。

## エラーと alert の写像

`AeadError` は `Display` に長さ・内容などの詳細を含めない固定の英語
文言のみを返す。`alert_description` は `tls::hkdf::HkdfError` と同じ
作法（`tls::record::AlertDescription` は変更せず、独立した `u8` 定数
写像を提供する）:

| バリアント | 意味 | alert |
| ---------- | ---- | ----- |
| `TagMismatch` | タグ検証失敗（改ざん・鍵/nonce 不一致） | `bad_record_mac`（20） |
| `CiphertextLength` | `open` への入力がタグ長未満／上限超過（復号できない） | `bad_record_mac`（20） |
| `PlaintextTooLong` | `seal` への平文が上限超過（呼び出し契約違反） | `internal_error`（80） |
| `AadTooLong` | AAD が上限超過（呼び出し契約違反） | `internal_error`（80） |

`TagMismatch` と「長さ不足で復号できない」（`CiphertextLength`）を同じ
`bad_record_mac` へ収束させることで、攻撃者に「タグが違う」のか
「そもそも長さがおかしい」のかを区別させない。alert の実送出は `#965`
の担当で、本 Issue は写像のみを提供する。

## テストベクタの出典

McGrew & Viega, "The Galois/Counter Mode of Operation" の AES-128
Test Case 1〜4（`tls/aes_gcm.rs` の単体テスト・
`crates/wire-server/tests/tls_aes_gcm_vectors.rs` の公開 API 結合
テストの双方に固定）。値は一次資料・複数の独立した実装（NSS の
`gcm-vectors.h` 等）で相互確認したうえでテストへ直接転記し、記憶からの
再構成には頼らない。

- Test Case 1: 鍵・nonce とも全 0、P・AAD とも空
- Test Case 2: 鍵・nonce とも全 0、16 バイトのゼロ平文・AAD なし
- Test Case 3: 専用鍵・nonce、64 バイト平文・AAD なし
- Test Case 4: Test Case 3 と同じ鍵・nonce、60 バイト平文・20 バイト
  AAD（ブロック長の非整数倍）

Test Case 2 の `H = E_K(0^128)` は `#957`（`tls/aes.rs`）が既に固定
した全 0 鍵の参照値
（`encrypt_all_zero_key_and_block_reference_value`）と一致することを
確認済みで、`#957`／`#958` の境界（`Aes128` の暗号化方向のみを GCM が
再利用する設計）が正しく接続されていることの追加的な裏付けになって
いる。

Test Case 5・6（96 ビット以外の nonce）は本 API の対象外（前述）の
ため実装・テストしない。

## ゼロ化の限界

`Aes128Gcm` は `Drop` で `H` を `tls::hkdf::zeroize` により
best-effort にゼロ化する（内包する `Aes128` 自身も Drop でラウンド鍵を
ゼロ化する）。CTR 鍵ストリームのブロック・`E_K(J0)` の一時値も使用後に
ゼロ化する。いずれも `unsafe`（`write_volatile` 等）を使わないため、
最適化により消去が省略されない保証はない（`tls::aes::Aes128`・
`tls::hkdf::Secret32` と同じ限界）。

## スループット参考値（informational）

`#[ignore]` の手動専用テスト
`aes_gcm.rs::tests::aes128_gcm_throughput_reference` を追加した。
実行方法:

```sh
cargo test -p fandhe-db-wire-server --release \
  aes128_gcm_throughput_reference -- --ignored --nocapture
```

本開発環境（共有 QEMU）での実測（1 レコード分の平文
`MAX_PLAINTEXT_LEN` バイトの `seal` を 2,000 回）:

```text
aes128_gcm_throughput_reference: 33248000 bytes in 6.371954704s (5.22 MB/s, 共有環境の参考値・採否根拠にしない)
```

`docs/design/benchmark-judgement-policy.md` の方針どおり、この値は
共有環境の参考値であり方式採否の根拠にしない。`#957` と同様、
ビット直列 GHASH・1 ラウンドごとの SubBytes ビットプレーン転置により
AES-NI/PCLMULQDQ 命令や T-table 方式に比べて大幅に遅いことが想定
されるが、本 Issue の受け入れ基準は定数時間性とテストベクタ一致で
あり、性能改善は対象外。

## 対象外・申し送り

- per-record nonce（iv XOR seq）・シーケンス番号管理・
  `TLSInnerPlaintext` の内容型パディング・レコード保護本体は `#959`
  の担当
- alert の実送出・ハンドシェイク状態機械への結線は `#965`・`#966`
  以降の担当
- 96 ビット以外の nonce（GHASH から J0 を導出する経路）・in-place／
  detached API は実装しない
- 性能改善（GHASH のテーブル化・PCLMULQDQ 等のハードウェア
  アクセラレーション）は本 Issue の対象外。必要であれば別 Issue で
  実測に基づいて検討する
