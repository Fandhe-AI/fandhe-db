# TLS 1.3 レコード保護（nonce・シーケンス番号・内容型パディング）実装記録

- ステータス: Accepted（実装記録）
- 対応: TASK-228（WIRE-9・HTTP-10 ポインタ）・Issue #959・親 Issue #941

## 背景

親 Issue #941（TLS 1.3 サーバー側自作実装）の分解 8/20。既存の 3 部品

- `tls::record`（#952）: 鍵・状態を持たないレコードのフレーミング
- `tls::key_schedule`（#956）: traffic secret → `TrafficKeys{key, iv}`
- `tls::aes_gcm`（#958）: `Aes128Gcm::seal`／`open`（鍵一つ分の AEAD）

をつなぎ、RFC 8446 §5.2〜§5.5 のレコード保護層（per-record nonce・
シーケンス番号・`TLSInnerPlaintext` の内容型・パディング・AAD・
handshake 鍵→application 鍵の切替）を `tls::record_protection` として
実装した。

## モジュール配置

Issue には「`tls/record.rs` へ結線」と書かれていたが、`record.rs` の
module doc は「レコード層は鍵・暗号・状態を一切持たない」と明言して
おり、これを崩さないため新規ファイル `tls/record_protection.rs` を
追加した（`record`・`key_schedule`・`aes_gcm` を利用する側）。既存
サブ Issue（#956・#958）と同じ流儀で `mod.rs` へ `pub mod
record_protection;` と doc 箇条書きを 1 行追加した。

## 方向ごとに独立した状態（`Sealer`／`Opener`）

送信（write）鍵と受信（read）鍵は別々のタイミングで切り替わる
（送信は server Finished を seal した直後、受信は client Finished を
open した直後）。「いつ切り替えるか」の判断は呼び出し元（#965 の状態
機械）が担い、本モジュールが提供するのは切替の**仕組み**——
`install_handshake_keys`／`install_application_keys` による
`Plaintext → Handshake(RecordCipher) → Application(RecordCipher)` の
一方向遷移の強制——だけである。

- 逆戻り・同じ epoch への二重 install・`Plaintext` から `Application`
  への直接遷移はいずれも `Err(InvalidTransition)`（alert は
  `internal_error`）。
- 遷移するたびにシーケンス番号は 0 にリセットされる（`RecordCipher::
  new` が毎回新しい状態を作るため）。
- `Opener::record_kind()` は現在の epoch に応じて呼び出し元が
  `RecordBuffer::next_record`／`read_record` へ渡すべき `RecordKind`
  （`Plaintext` epoch は `Plaintext`、それ以外は `Ciphertext`）を返す。

## `RecordCipher`（1 方向・1 epoch 分の AEAD 状態）

`Aes128Gcm` ＋ `iv: [u8; 12]` ＋ `seq: u64` を持つ。`Clone`／`Copy` は
導出せず、`Debug` は内容を秘匿し、`Drop` で `iv` を
`tls::hkdf::zeroize` により best-effort ゼロ化する（`cipher` 自身の
`H` は `Aes128Gcm` の `Drop` が別途ゼロ化する）。

nonce の導出（`RecordCipher::nonce`）は `seq.to_be_bytes()`（8 バイト）
を 12 バイトの末尾へ配置し、`iv` と XOR するだけの分岐なし演算
（`iter_mut().zip()`。添字直指定は避ける）。

## シーケンス番号の上限（2 段階・いずれも致命的）

1. **wrap の防止**: `checked_add(1)` が失敗すれば
   `Err(SequenceExhausted)`。
2. **AEAD の使用上限**（RFC 8446 §5.5。`TLS_AES_128_GCM_SHA256` の
   AEAD 使用上限は約 2^24.5 レコード——これは IETF の公開仕様値であり
   `docs/spec` の内容ではない）。`KeyUpdate` による rekey を実装しない
   （対象外）ため、この上限が実際に接続を終了させる条件になる。余裕を
   持たせた実装既定値として `MAX_RECORDS_PER_KEY = 1 << 24` を定義し、
   `RecordCipher::peek_nonce`（seal・open 双方が呼ぶ共通の「これから
   使う nonce を確認する」関数）が nonce を返す**前**に判定する。

上限到達時の alert は `None`（alert を送らずに切断する。
`ProtectionError::alert_description` が `None` を返す唯一のバリアント）
。使い切った鍵のもとでは保護された alert を安全に送れる保証がないため、
`tls::record::RecordError::Truncated`／`Io` と同じ扱いにした。

`open` が復号（タグ検証）に失敗した場合、シーケンス番号は**進めない**
（`RecordCipher::advance` は `peek_nonce` とは別関数で、`seal` は常に
呼ぶが `open` は復号成功時のみ呼ぶ）。同一シーケンス番号を再利用した
状態のまま接続が終了するため問題にならない。

テストで上限へ到達させるため、`#[cfg(test)] fn
RecordCipher::with_seq_for_test(keys, seq)` を用意した（0 から
2^24 回 seal／open するのは現実的でないため、この仕組みが無いと受入
基準「上限到達時の接続終了」は実際には検証できず vacuous になる）。

## `TLSInnerPlaintext`（RFC 8446 §5.4）

- 送信（`build_inner_plaintext`）: `content || content_type ||
  0-padding` を `checked_add` で長さ検査してから組み立てる。
  `MAX_INNER_PLAINTEXT_LEN = tls::record::MAX_PLAINTEXT_LEN + 1`
  （2^14+1）。`const _: () = assert!(MAX_INNER_PLAINTEXT_LEN +
  aes_gcm::TAG_LEN <= record::MAX_CIPHERTEXT_LEN);` で静的に固定する。
- 受信（`decode_inner_plaintext`）: 復号後の平文全体を**定数時間**で
  走査し、末尾の非ゼロバイト（内容型）とその手前までの `content` を
  取り出す。非ゼロバイトが 1 つも無い（全ゼロ）平文は
  `NoContentType` として拒否する。

### 定数時間パディング走査

素直に末尾から走査すると処理時間からパディング長が漏れる（パディングは
まさにその長さを隠すために存在する）。そこで復号後のバッファ全体
（最大 2^14+1 バイト）をビット演算のマスクだけで 1 回、固定長で走査
する。

```text
for (i, b) in plaintext:
    nz = ct_is_nonzero(b)              # 0x00 または 0xFF（分岐なし）
    last_idx = ct_select_usize(nz, i, last_idx)
    content_type_byte = ct_select_u8(nz, b, content_type_byte)
    found |= nz
```

`ct_is_nonzero` は `(x | wrapping_neg(x)) >> 31` という標準的な
ビットトリックで非ゼロ判定を作り、`ct_select_*` は `mask` を
符号拡張して all-1／all-0 の `usize`／`u8` マスクへ変換してから
`(a & mask) | (b & !mask)` で選ぶ。走査後に分岐するのは「受理するか
拒否するか」「切り詰め後の長さ」という、どのみち公開される結果のみ
（切り詰め後の `content.len()` は呼び出し元へ返す `Vec` の長さとして
結局公開されるため、走査自体を定数時間にする意味はパディング長その
ものを処理時間の差から推測できないようにする点にある）。

### 受信側の判定順序

`Opener::open` は次の順で判定する（外側 content type・暗号文長の検査は
`Handshake`／`Application` epoch 共通の `open_protected` が担い、
application epoch 固有の追加拒否は呼び出し元で行う）:

1. 外側 content type が `ApplicationData` でない → `UnexpectedOuterType`
2. 暗号文長が `[TAG_LEN+1, MAX_CIPHERTEXT_LEN]` の範囲外 → `BadRecordMac`
3. AEAD 復号（タグ検証失敗 → `BadRecordMac`。**シーケンス番号は
   進めない**）
4. `decode_inner_plaintext`: 全ゼロ → `NoContentType`／内側型が
   `{Handshake, Alert, ApplicationData}` 以外（`ChangeCipherSpec`・
   未知値を含む）→ `ForbiddenInnerType`／`Handshake`・`Alert` の 0 長
   content → `EmptyContent`／`TLSInnerPlaintext` 長超過 →
   `InnerOverflow`
5. （`Application` epoch のみ）内側型が `Handshake` →
   `ForbiddenInnerType`

手順 5 により、`KeyUpdate`（type 24）を含む post-handshake の
Handshake メッセージは application epoch では構造的に受理できない。
`tls::handshake` が type 24 を閉じた語彙の外として既に拒否している
ことと合わせ、「`KeyUpdate` を受信したら接続を終了する」契約を満たす。
`KeyUpdate` を処理する API（rekey）は実装していない。

`Plaintext` epoch の受信は外側が `Handshake`／`Alert` ならそのまま
通し、`ApplicationData`（鍵導入前に届き得ない）・`ChangeCipherSpec`
は `UnexpectedOuterType` とする。**middlebox 互換の
`ChangeCipherSpec` レコードは、`Opener::open` が呼ばれる前に呼び出し元
（#965）が読み捨てる契約**とし、この層では扱わない。

## AAD の構成

外側レコードヘッダ 5 バイト（`RecordHeader::to_bytes()`）。`length` は
「暗号文＋タグ」の長さ。

- 送信（`seal_aad`）: `content_type = ApplicationData`・
  `legacy_version = LEGACY_RECORD_VERSION`（0x0303）で新規に組み立てる。
- 受信（`open_aad`）: **受信した値のまま**（`record.content_type`・
  `record.legacy_version`）で組み立てる。`Record::serialize_into` は
  version を 0x0303 へ正規化するため AAD の組み立てには使わない。
  `legacy_version` は RFC の指示どおり検査しない（値が違えばタグ検証で
  失敗する）。

## `Sealer`

- `seal(content_type, content, padding_len)`: 通常呼び出しは
  `padding_len = 0`。パディング方針を決める仕組み（いつ・どれだけ
  パディングするか）はこのモジュールでは提供しない。`Plaintext` epoch
  では `Handshake`／`Alert` の平文レコードだけを許可する
  （`ApplicationData`・`ChangeCipherSpec` は `SendContractViolation`。
  CCS は鍵状態を持たない固定レコードのため、呼び出し元が `Sealer` を
  経由せず `tls::record::Record` を直接組み立てる契約とする）。
  保護 epoch では内側型に CCS を許可しない。
- epoch ごとに送出できる内側 content type（Issue #1284。`seal`／
  `seal_fragmented`／`seal_detached` 共通。拒否はシーケンス番号の消費・
  nonce 導出・暗号化より前に行うため、拒否後も seq は進まない）:

  | epoch | 許可 | 拒否 |
  | ----- | ---- | ---- |
  | `Plaintext` | `Handshake`／`Alert` | `ApplicationData`・CCS（`SendContractViolation`） |
  | `Handshake` | `Handshake`／`Alert` | `ApplicationData`（`SendContractViolation`）・CCS（`ForbiddenInnerType`） |
  | `Application` | `Alert`／`ApplicationData` | `Handshake`（`SendContractViolation`）・CCS（`ForbiddenInnerType`） |

  `Application` epoch の内側 Handshake 拒否は、受信側 `Opener::open` が
  同 epoch の内側 Handshake を拒否する（post-handshake の
  NewSessionTicket・KeyUpdate 非対応）のと対称にするための fail-closed。
  `seal_detached` は `Application` epoch の `Alert`／`ApplicationData`
  のみ許可する。
- `seal_fragmented(content_type, payload)`: `payload` を
  `MAX_INNER_PLAINTEXT_LEN - 1`（内側 content type 分を差し引いた長さ）
  ごとに分割して seal する。`Alert` は分割禁止（超過は `Err`）。
  `ApplicationData` の空 payload は空の `Vec`（0 長レコードを送らない）
  。`Handshake` の空 payload は `Err`。挙動は `tls::record::
  fragment_plaintext` と同じ規則に揃える。

送信後は必ずシーケンス番号を進める（seal 自体が失敗しなければ）。

## エラーと alert の写像

`ProtectionError` は `Display` に長さ・内容・鍵情報を含めない
（`ForbiddenInnerType(u8)` が保持する content type バイト値は、ワイヤ
上で公開されている値であり例外的に含める）。

| バリアント | 意味 | alert |
| ---------- | ---- | ----- |
| `UnexpectedOuterType` | 外側 content type がこの epoch で不正 | `unexpected_message`（10） |
| `BadRecordMac` | AEAD タグ検証失敗・暗号文長不正 | `bad_record_mac`（20） |
| `NoContentType` | `TLSInnerPlaintext` が全ゼロ | `unexpected_message`（10） |
| `ForbiddenInnerType` | 内側型が禁止された値 | `unexpected_message`（10） |
| `EmptyContent` | `Handshake`／`Alert` の 0 長 content | `unexpected_message`（10） |
| `InnerOverflow` | `TLSInnerPlaintext` 長超過 | `record_overflow`（22） |
| `SequenceExhausted` | シーケンス番号上限 | なし（alert を送らず切断） |
| `InvalidTransition` | 鍵切替 API の誤用 | `internal_error`（80） |
| `SendContractViolation` | 呼び出し契約違反（送信側長さ超過・epoch で許可されない content type 等） | `internal_error`（80） |

`AeadError::TagMismatch`／`CiphertextLength` はいずれも `BadRecordMac`
へ収束させ（`tls::aes_gcm` の設計をそのまま踏襲。攻撃者にタグ不一致か
長さ不正かを区別させない）、`PlaintextTooLong`／`AadTooLong` は
`SendContractViolation` へ写像する。

`tls::record::AlertDescription`（`#[non_exhaustive]`）に
`BadRecordMac`(20)・`InternalError`(80) を追加した（既存 3 値
——`UnexpectedMessage`・`RecordOverflow`・`DecodeError`——は無変更）。

`wire_code`（ERR-1／ERR-2／ERR-4）への写像は追加しない。TLS 層の失敗は
`ErrorResponse` ではなく TLS alert と切断で表す方針を `tls::record`・
`tls::aes_gcm` から引き継ぐ。alert の実送出・ハンドシェイク状態機械
への結線は `#965` の担当。

## テストの出典と非 vacuous 性

- 単体テスト（`record_protection.rs` 内）: nonce の導出（seq=0/1・
  `u64::MAX` 近傍）・epoch 遷移の正当性/誤用・seal/open 往復
  （パディング 0・任意長・上限ちょうど）・全ゼロ／禁止型／0 長内側
  content の拒否・改ざん（暗号文・タグの反転／切り詰め・誤った
  seq）・`seal_fragmented` の分割・往復・空/0長規則・シーケンス番号
  上限到達（`with_seq_for_test` で 2^24 直前から開始し、実際に
  `MAX_RECORDS_PER_KEY` 回目で `SequenceExhausted` になることを固定）
  ・`u64::MAX` からの wrap 防止・`Debug` 出力の鍵非露出（鍵全体 32 桁の
  16 進表現という衝突しない単位で検査。個々のバイトの 2 桁表現は
  "redacted" 等の英単語中の文字列と偶然一致しうるため使わない）。
- 結合テスト `crates/wire-server/tests/tls_record_protection_rfc8448.rs`
  : RFC 8448 §3 の ECDHE 入力・transcript hash から `tls::key_schedule`
  の公開 API のみで client handshake traffic key/iv を導出し（値は
  `tls_key_schedule_rfc8448.rs` の固定値と一致することも再確認）、
  `tls::record::record.rs` が既に固定している client Finished の
  暗号化レコード（58 octets）を `Opener::open` で実際に復号する。
  verify_data の具体値は決め打ちせず、復号結果が RFC 8446 §4.4.4 の
  Finished メッセージ構造（`type(1)=0x14 || length(3)=0x000020 ||
  verify_data(32)` = 36 octets）と一致することで、AEAD タグ検証・
  復号・`TLSInnerPlaintext` の内容型復元が公開ベクタに対して正しく
  機能することを固定する（Finished の verify_data 計算自体は `#964`
  の担当のため、値そのものの一致検証は対象外）。同じレコードを
  シーケンス番号が進んだ後に再度 open すると `BadRecordMac` になる
  こと、暗号文 1 ビット反転が `BadRecordMac` になることもあわせて
  固定する。

## 対象外・申し送り

- 状態機械・alert の実送出・CCS の破棄判定・「いつ切り替えるか」の
  判断は `#965` の担当
- transcript hash／Finished の verify_data 計算・検証は `#964` の担当
- 接続への結線（`handshake.rs`・`server.rs`）は `#966`・`#968` の担当
- `KeyUpdate` による rekey・0-RTT・送信パディング方針の決定は対象外
