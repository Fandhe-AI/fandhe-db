//! PKCS#8 v1（RFC 5208 の枠組み・RFC 8410 §7 の Ed25519 特化）の最小 DER
//! パースと、鍵ファイルの入口（TASK-228・WIRE-9・HTTP-10 ポインタ。
//! Issue #962・親 #941）。
//!
//! [`super::pem`] が返す生 DER バイト列から、Ed25519 の 32 バイト seed
//! （[`Ed25519Seed`]）だけを取り出す。X.509 証明書の DER パース・鍵導出
//! （公開鍵の計算）・署名は本モジュールの対象外であり、それぞれ #963・
//! #961 が担当する（#961 は本モジュールが公開する [`Ed25519Seed`] を
//! 入力として使う）。
//!
//! ## 想定する DER バイト列（RFC 8410 §7・§10.3。Ed25519 PKCS#8 v1・全 48
//! バイト。数値・レイアウトの詳細は spec 本文ではなく上記 RFC 由来）
//!
//! ```text
//! 30 2e                 SEQUENCE (46)
//!    02 01 00           INTEGER version = 0 (v1)
//!    30 05              SEQUENCE AlgorithmIdentifier
//!       06 03 2b 65 70  OID 1.3.101.112 (Ed25519)。parameters は「無い」
//!    04 22              OCTET STRING privateKey (34)
//!       04 20 <32B>     CurvePrivateKey ::= OCTET STRING (32) ← seed
//! ```
//!
//! 外側の OCTET STRING（34 バイト）は seed そのものではない点に注意。
//! seed は内側の `04 20` の後ろにある 32 バイトである。
//!
//! ## 定数時間性
//!
//! DER の構造部分（タグ・長さ・version・OID）は形式として公開された固定値
//! であり秘密ではないため、これらでの分岐は問題ない。秘密値は seed の
//! 32 バイトとその base64 表現のみで、前者はコピーするだけで分岐せず、
//! 後者は [`super::pem`] の定数時間デコーダが処理する。

use std::fmt;
use std::path::Path;

/// PKCS#8 の DER 上で判別したアルゴリズム種別。RSA・ECDSA など Ed25519 以外
/// は起動時にどの種別かを報告したうえで拒否する（受入基準 3・fail-closed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAlgorithm {
    Rsa,
    Ecdsa,
    Ed448,
    X25519,
    Other,
}

impl fmt::Display for KeyAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            KeyAlgorithm::Rsa => "RSA",
            KeyAlgorithm::Ecdsa => "ECDSA",
            KeyAlgorithm::Ed448 => "Ed448",
            KeyAlgorithm::X25519 => "X25519",
            KeyAlgorithm::Other => "unknown",
        };
        write!(f, "{name}")
    }
}

/// PKCS#8 DER のパースで検出した拒否理由。位置・実バイトは含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pkcs8Error {
    /// タグ・長さ・後続バイトなど DER 構文そのものの違反。
    Malformed,
    /// Ed25519 以外のアルゴリズム（種別が判明した場合）。
    UnsupportedAlgorithm(KeyAlgorithm),
    /// version が 1（OneAsymmetricKey・v2 形式）。attributes／publicKey の
    /// 照合には鍵導出（#961）が必要なため、対応するまで明示的に拒否する。
    UnsupportedVersion,
    /// Ed25519 の AlgorithmIdentifier に parameters が付いている
    /// （RFC 8410 は「無い」ことを要求する）。
    UnexpectedParameters,
    /// privateKey 内側の OCTET STRING（seed）の長さが 32 でない。
    InvalidSeedLength,
}

impl fmt::Display for Pkcs8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pkcs8Error::Malformed => write!(f, "PKCS#8 DER is malformed"),
            Pkcs8Error::UnsupportedAlgorithm(algo) => {
                write!(
                    f,
                    "private key algorithm {algo} is not supported (Ed25519 only)"
                )
            }
            Pkcs8Error::UnsupportedVersion => {
                write!(f, "PKCS#8 v2 (OneAsymmetricKey) is not supported")
            }
            Pkcs8Error::UnexpectedParameters => {
                write!(f, "Ed25519 AlgorithmIdentifier must not have parameters")
            }
            Pkcs8Error::InvalidSeedLength => {
                write!(f, "Ed25519 private key seed has an invalid length")
            }
        }
    }
}

impl std::error::Error for Pkcs8Error {}

/// ファイル入出力・PEM 構文・PKCS#8 構文のいずれかで失敗した秘密鍵読み込み
/// エラー。[`super::pem::FileLoadError`] はパスを含み、他はいずれも
/// 内容非依存の分類のみを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateKeyLoadError {
    File(super::pem::FileLoadError),
    Pem(super::pem::PemError),
    Pkcs8(Pkcs8Error),
}

impl fmt::Display for PrivateKeyLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrivateKeyLoadError::File(e) => write!(f, "{e}"),
            PrivateKeyLoadError::Pem(e) => write!(f, "{e}"),
            PrivateKeyLoadError::Pkcs8(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PrivateKeyLoadError {}

/// Ed25519 の 32 バイト seed（PKCS#8 の `CurvePrivateKey`）。`Clone`／`Copy`
/// は導出せず、`Debug` は内容を伏せ、Drop 時に best-effort でゼロ化する
/// （[`super::hkdf::Secret32`] と同じ設計判断）。
pub struct Ed25519Seed([u8; 32]);

impl Ed25519Seed {
    #[cfg(test)]
    pub(crate) fn from_bytes_for_test(bytes: [u8; 32]) -> Self {
        Ed25519Seed(bytes)
    }

    /// 生の 32 バイト表現への参照。#961（鍵導出・署名）がここから公開鍵・
    /// 署名用のスカラーを導出する。
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Ed25519Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ed25519Seed").field(&"<redacted>").finish()
    }
}

impl Drop for Ed25519Seed {
    fn drop(&mut self) {
        super::hkdf::zeroize(&mut self.0);
    }
}

/// Ed25519 の AlgorithmIdentifier OID（1.3.101.112。RFC 8410 §3）の
/// DER エンコード本体（タグ・長さを除く値部分）。
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// Ed448（1.3.101.113）。
const OID_ED448: &[u8] = &[0x2b, 0x65, 0x71];
/// X25519（1.3.101.110）。
const OID_X25519: &[u8] = &[0x2b, 0x65, 0x6e];
/// rsaEncryption（1.2.840.113549.1.1.1）。
const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// id-ecPublicKey（1.2.840.10045.2.1）。
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

fn classify_oid(oid: &[u8]) -> KeyAlgorithm {
    if oid == OID_ED25519 {
        // 呼び出し元（[`parse_ed25519_pkcs8_der`]）が Ed25519 の場合を
        // 個別に扱うため、ここに来るのは分類のみを使う経路である。
        KeyAlgorithm::Other
    } else if oid == OID_RSA {
        KeyAlgorithm::Rsa
    } else if oid == OID_EC {
        KeyAlgorithm::Ecdsa
    } else if oid == OID_ED448 {
        KeyAlgorithm::Ed448
    } else if oid == OID_X25519 {
        KeyAlgorithm::X25519
    } else {
        KeyAlgorithm::Other
    }
}

/// 固定形状の DER 値だけを読む最小リーダー。再帰・可変長の一般的な
/// TLV 走査は行わず、本モジュールが要求する固定シーケンスのみを検証する
/// （#963 が X.509 向けに一般化する際は、この最小実装を出発点にできる）。
///
/// 添字アクセス（`[]`）は使わず `get()`／`split_at_checked` のみで進める
/// （受信データ経路。`.claude/rules/coding-rust.md`）。
struct DerReader<'a> {
    data: &'a [u8],
}

impl<'a> DerReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        DerReader { data }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// 長さオクテット（definite 形式のみ）を読み、値の長さを返す。
    /// - `0x80`（indefinite）は拒否する
    /// - long form（`0x81`／`0x82`）は 2 バイトまでとし、最小符号化
    ///   （1 バイトで表現できる値を long form で書いていない・0x82 の値が
    ///   0x100 未満でない）を要求する
    /// - 残りバイト数を超える長さは拒否する
    fn read_length(&mut self) -> Result<usize, Pkcs8Error> {
        let (first, rest) = self.data.split_first().ok_or(Pkcs8Error::Malformed)?;
        if *first & 0x80 == 0 {
            self.data = rest;
            return Ok(*first as usize);
        }
        let octet_count = (*first & 0x7f) as usize;
        if octet_count == 0 || octet_count > 2 {
            // indefinite（0x80）または本実装が対応しない長さ表現。
            return Err(Pkcs8Error::Malformed);
        }
        let (len_bytes, after_len) = rest
            .split_at_checked(octet_count)
            .ok_or(Pkcs8Error::Malformed)?;
        let value = len_bytes
            .iter()
            .try_fold(0usize, |acc, b| {
                acc.checked_shl(8).map(|v| v | (*b as usize))
            })
            .ok_or(Pkcs8Error::Malformed)?;
        if octet_count == 1 && value < 0x80 {
            // 1 バイトで表現できる値を long form で書いた非最小符号化。
            return Err(Pkcs8Error::Malformed);
        }
        if octet_count == 2 && value < 0x100 {
            return Err(Pkcs8Error::Malformed);
        }
        self.data = after_len;
        Ok(value)
    }

    /// `expected_tag` の TLV を読み、値部分を返す。読んだ分だけ内部カーソル
    /// を進める。
    fn read_tlv(&mut self, expected_tag: u8) -> Result<&'a [u8], Pkcs8Error> {
        let (tag, rest) = self.data.split_first().ok_or(Pkcs8Error::Malformed)?;
        if *tag != expected_tag {
            return Err(Pkcs8Error::Malformed);
        }
        self.data = rest;
        let len = self.read_length()?;
        let (value, remaining) = self
            .data
            .split_at_checked(len)
            .ok_or(Pkcs8Error::Malformed)?;
        self.data = remaining;
        Ok(value)
    }
}

const TAG_SEQUENCE: u8 = 0x30;
const TAG_INTEGER: u8 = 0x02;
const TAG_OID: u8 = 0x06;
const TAG_OCTET_STRING: u8 = 0x04;

/// PKCS#8 v1 の Ed25519 秘密鍵を DER から取り出す。
///
/// 拒否の判定順（どの理由で拒否されたかが決定的に決まるよう固定する）:
/// 1. 外側の SEQUENCE（後続バイトがあれば拒否）
/// 2. version（`0` 以外の 1 バイト INTEGER は [`Pkcs8Error::Malformed`]）
/// 3. AlgorithmIdentifier の SEQUENCE・OID
/// 4. OID による種別判定（Ed25519 以外は
///    [`Pkcs8Error::UnsupportedAlgorithm`]）
/// 5. Ed25519 の parameters 残存チェック
/// 6. version が 1（v2・OneAsymmetricKey）なら
///    [`Pkcs8Error::UnsupportedVersion`]（黙って無視せず明示的に拒否する）
/// 7. privateKey の OCTET STRING → 内側 OCTET STRING（seed）の長さ検証
/// 8. privateKey の後ろに残りバイトがあれば拒否する
pub fn parse_ed25519_pkcs8_der(der: &[u8]) -> Result<Ed25519Seed, Pkcs8Error> {
    let mut outer = DerReader::new(der);
    let body = outer.read_tlv(TAG_SEQUENCE)?;
    if !outer.is_empty() {
        return Err(Pkcs8Error::Malformed);
    }

    let mut reader = DerReader::new(body);

    let version_bytes = reader.read_tlv(TAG_INTEGER)?;
    let version = match version_bytes {
        [0x00] => 0u8,
        [0x01] => 1u8,
        _ => return Err(Pkcs8Error::Malformed),
    };

    let algorithm = reader.read_tlv(TAG_SEQUENCE)?;
    let mut algo_reader = DerReader::new(algorithm);
    let oid = algo_reader.read_tlv(TAG_OID)?;

    if oid != OID_ED25519 {
        return Err(Pkcs8Error::UnsupportedAlgorithm(classify_oid(oid)));
    }
    // RFC 8410 は Ed25519 の AlgorithmIdentifier に parameters を
    // 「持たない」ことを要求する（NULL であっても不可）。
    if !algo_reader.is_empty() {
        return Err(Pkcs8Error::UnexpectedParameters);
    }

    // 種別が Ed25519 だと判明した後で version を検査する
    // （RSA・EC 鍵の種別名を先に報告できるようにするための順序）。
    if version == 1 {
        return Err(Pkcs8Error::UnsupportedVersion);
    }

    let private_key_outer = reader.read_tlv(TAG_OCTET_STRING)?;
    let mut inner_reader = DerReader::new(private_key_outer);
    let seed_bytes = inner_reader.read_tlv(TAG_OCTET_STRING)?;
    if !inner_reader.is_empty() {
        // 外側 OCTET STRING の中に、内側 OCTET STRING 以外の余りがある。
        return Err(Pkcs8Error::Malformed);
    }
    let seed: [u8; 32] = seed_bytes
        .try_into()
        .map_err(|_| Pkcs8Error::InvalidSeedLength)?;

    // privateKey（attributes `[0]` 等）の後ろに残りがあれば拒否する。
    if !reader.is_empty() {
        return Err(Pkcs8Error::Malformed);
    }

    Ok(Ed25519Seed(seed))
}

/// PEM でラップされた Ed25519 PKCS#8 秘密鍵をデコードする
/// （[`super::pem::decode_private_key_pem`] → [`parse_ed25519_pkcs8_der`]
/// の合成）。
pub fn decode_ed25519_private_key_pem(text: &[u8]) -> Result<Ed25519Seed, PrivateKeyLoadError> {
    let der = super::pem::decode_private_key_pem(text).map_err(PrivateKeyLoadError::Pem)?;
    parse_ed25519_pkcs8_der(der.as_slice()).map_err(PrivateKeyLoadError::Pkcs8)
}

/// 上限付きでファイルを読み込み、Ed25519 PKCS#8 秘密鍵としてデコードする。
/// CLI からの結線（`--tls-key-file` 等）は #967 の担当。
pub fn load_ed25519_private_key_file(path: &Path) -> Result<Ed25519Seed, PrivateKeyLoadError> {
    let buf = super::pem::read_bounded_file(path, super::pem::MAX_PRIVATE_KEY_FILE_LEN)
        .map_err(PrivateKeyLoadError::File)?;
    decode_ed25519_private_key_pem(buf.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8410 §10.3 v1 の Ed25519 秘密鍵 PEM／期待 seed。
    const ED25519_V1_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";
    const ED25519_V1_SEED_HEX: &str =
        "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842";

    // version=1（v2・OneAsymmetricKey）を宣言する Ed25519 AlgorithmIdentifier
    // の最小 DER（privateKey 本体は本 Issue のパース順序では読まれない。
    // version の検査を AlgorithmIdentifier 確認の直後・privateKey 読み取り
    // より前に固定していることの確認が目的）。
    const ED25519_V2_PEM: &str =
        "-----BEGIN PRIVATE KEY-----\nMAoCAQEwBQYDK2Vw\n-----END PRIVATE KEY-----\n";

    // RFC 8032 §7.1 TEST 1 の秘密鍵（seed）。
    const RFC8032_TEST1_SEED_HEX: &str =
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

    fn hex_decode(hex: &str) -> Vec<u8> {
        // テスト専用の 16 進デコード（`0x` 桁境界を正しく判定するため
        // 奇数長は末尾を無視せず assert する）。
        assert!(hex.len().is_multiple_of(2));
        let mut out = Vec::with_capacity(hex.len() / 2);
        let bytes = hex.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let hi = (bytes[i] as char).to_digit(16).expect("hex digit");
            let lo = (bytes[i + 1] as char).to_digit(16).expect("hex digit");
            out.push(((hi << 4) | lo) as u8);
            i += 2;
        }
        out
    }

    fn build_ed25519_pkcs8_der(seed: &[u8; 32]) -> Vec<u8> {
        // RFC 8410 §10.3 のバイトレイアウトをそのまま手組みする
        // （テスト専用のエンコーダ。本番コードには存在しない）。
        let mut der = vec![
            0x30, 0x2e, // SEQUENCE (46)
            0x02, 0x01, 0x00, // INTEGER version = 0
            0x30, 0x05, // SEQUENCE AlgorithmIdentifier
            0x06, 0x03, 0x2b, 0x65, 0x70, // OID Ed25519
            0x04, 0x22, // OCTET STRING (34)
            0x04, 0x20, // OCTET STRING (32)
        ];
        der.extend_from_slice(seed);
        der
    }

    #[test]
    fn parse_rfc8410_v1_vector_yields_expected_seed() {
        let expected = hex_decode(&ED25519_V1_SEED_HEX[..64]);
        let seed = parse_ed25519_pkcs8_der(&{
            // §10.3 v1 の DER は PEM から改めてデコードして得る
            // （decode_private_key_pem は別モジュールのため、ここでは
            // 直接組み立てた DER を使う）。
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&expected);
            build_ed25519_pkcs8_der(&arr)
        })
        .expect("valid v1 DER");
        assert_eq!(seed.as_bytes().as_slice(), expected.as_slice());
    }

    #[test]
    fn decode_pem_rfc8410_v1_matches_direct_der_parse() {
        let seed = decode_ed25519_private_key_pem(ED25519_V1_PEM.as_bytes()).expect("valid PEM");
        let expected = hex_decode(&ED25519_V1_SEED_HEX[..64]);
        assert_eq!(seed.as_bytes().as_slice(), expected.as_slice());
    }

    #[test]
    fn decode_pem_rfc8032_test1_seed_round_trips() {
        let expected = hex_decode(&RFC8032_TEST1_SEED_HEX[..64]);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&expected);
        let der = build_ed25519_pkcs8_der(&arr);
        let seed = parse_ed25519_pkcs8_der(&der).expect("valid DER");
        assert_eq!(seed.as_bytes().as_slice(), expected.as_slice());
    }

    #[test]
    fn decode_pem_rfc8410_v2_example_is_rejected() {
        let err = decode_ed25519_private_key_pem(ED25519_V2_PEM.as_bytes()).unwrap_err();
        assert_eq!(
            err,
            PrivateKeyLoadError::Pkcs8(Pkcs8Error::UnsupportedVersion)
        );
    }

    #[test]
    fn parse_rejects_rsa_oid() {
        let mut der = vec![
            0x30, 0x00, // SEQUENCE placeholder（後で長さを直す）
        ];
        der.clear();
        // version
        let mut body = vec![0x02, 0x01, 0x00];
        // AlgorithmIdentifier: SEQUENCE { OID rsaEncryption, NULL }
        let mut alg = vec![0x06, 0x09];
        alg.extend_from_slice(OID_RSA);
        alg.extend_from_slice(&[0x05, 0x00]); // NULL parameters
        body.push(0x30);
        body.push(alg.len() as u8);
        body.extend_from_slice(&alg);
        // privateKey OCTET STRING（内容は問わない。OID 判定が先に発生する）
        body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
        der.push(0x30);
        der.push(body.len() as u8);
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Rsa));
    }

    #[test]
    fn parse_rejects_ec_oid() {
        let mut body = vec![0x02, 0x01, 0x00];
        let mut alg = vec![0x06, 0x07];
        alg.extend_from_slice(OID_EC);
        body.push(0x30);
        body.push(alg.len() as u8);
        body.extend_from_slice(&alg);
        body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Ecdsa));
    }

    #[test]
    fn parse_rejects_ed448_oid() {
        let mut body = vec![0x02, 0x01, 0x00];
        let mut alg = vec![0x06, 0x03];
        alg.extend_from_slice(OID_ED448);
        body.push(0x30);
        body.push(alg.len() as u8);
        body.extend_from_slice(&alg);
        body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Ed448));
    }

    #[test]
    fn parse_rejects_x25519_oid() {
        let mut body = vec![0x02, 0x01, 0x00];
        let mut alg = vec![0x06, 0x03];
        alg.extend_from_slice(OID_X25519);
        body.push(0x30);
        body.push(alg.len() as u8);
        body.extend_from_slice(&alg);
        body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::X25519));
    }

    #[test]
    fn parse_rejects_unknown_oid() {
        let mut body = vec![0x02, 0x01, 0x00];
        let mut alg = vec![0x06, 0x03, 0x2b, 0x06, 0x01]; // 1.3.6.1 (適当な未知 OID)
        let _ = &mut alg;
        body.push(0x30);
        body.push(alg.len() as u8);
        body.extend_from_slice(&alg);
        body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Other));
    }

    #[test]
    fn parse_rejects_ed25519_with_null_parameters() {
        let mut body = vec![0x02, 0x01, 0x00];
        let mut alg = vec![0x06, 0x03, 0x2b, 0x65, 0x70, 0x05, 0x00]; // OID + NULL
        let _ = &mut alg;
        body.push(0x30);
        body.push(alg.len() as u8);
        body.extend_from_slice(&alg);
        body.extend_from_slice(&[0x04, 0x22, 0x04, 0x20]);
        body.extend_from_slice(&[0u8; 32]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::UnexpectedParameters);
    }

    #[test]
    fn parse_rejects_seed_length_31() {
        let mut body = vec![0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
        body.extend_from_slice(&[0x04, 0x21, 0x04, 0x1f]); // outer 33, inner 31
        body.extend_from_slice(&[0u8; 31]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::InvalidSeedLength);
    }

    #[test]
    fn parse_rejects_seed_length_33() {
        let mut body = vec![0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
        body.extend_from_slice(&[0x04, 0x23, 0x04, 0x21]); // outer 35, inner 33
        body.extend_from_slice(&[0u8; 33]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::InvalidSeedLength);
    }

    #[test]
    fn parse_rejects_missing_inner_octet_string() {
        // privateKey が単一の OCTET STRING（32 バイト）で、内側の
        // CurvePrivateKey ラッパーを欠く形。
        let mut body = vec![0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
        body.push(0x04);
        body.push(32);
        body.extend_from_slice(&[0u8; 32]);
        let mut der = vec![0x30, body.len() as u8];
        der.extend_from_slice(&body);

        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn parse_rejects_indefinite_length() {
        let der = [0x30, 0x80];
        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn parse_rejects_non_minimal_length_encoding() {
        // 0x10（16）は 1 バイトで表現できるのに long form（0x81 0x10）で
        // 書いた非最小符号化。
        let der = [0x30, 0x81, 0x10];
        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn parse_rejects_length_exceeding_remaining_bytes() {
        let der = [0x30, 0x10, 0x02, 0x01, 0x00];
        let err = parse_ed25519_pkcs8_der(&der).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn parse_rejects_trailing_bytes_after_outer_sequence() {
        let good = build_ed25519_pkcs8_der(&[0u8; 32]);
        let mut with_trailer = good.clone();
        with_trailer.push(0x00);
        let err = parse_ed25519_pkcs8_der(&with_trailer).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn parse_rejects_empty_der() {
        let err = parse_ed25519_pkcs8_der(&[]).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn parse_rejects_trailing_bytes_after_private_key() {
        let mut good = build_ed25519_pkcs8_der(&[0u8; 32]);
        // 外側 SEQUENCE の長さを 1 バイト分だけ伸ばして attributes 相当の
        // 余りバイトを追加する。
        good.push(0x00);
        good[1] = (good.len() - 2) as u8;
        let err = parse_ed25519_pkcs8_der(&good).unwrap_err();
        assert_eq!(err, Pkcs8Error::Malformed);
    }

    #[test]
    fn ed25519_seed_debug_does_not_leak_bytes() {
        let seed = Ed25519Seed::from_bytes_for_test([0xabu8; 32]);
        let debug = format!("{seed:?}");
        assert!(!debug.contains("ab"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn load_ed25519_private_key_file_reads_valid_key() {
        let path = crate::test_util::unique_temp_path("tls-pkcs8-ok", ".pem");
        std::fs::write(&path, ED25519_V1_PEM.as_bytes()).expect("write temp file");
        let result = load_ed25519_private_key_file(&path);
        let _ = std::fs::remove_file(&path);
        let seed = result.expect("valid key file");
        let expected = hex_decode(&ED25519_V1_SEED_HEX[..64]);
        assert_eq!(seed.as_bytes().as_slice(), expected.as_slice());
    }

    #[test]
    fn load_ed25519_private_key_file_reports_missing_file() {
        let path = crate::test_util::unique_temp_path("tls-pkcs8-missing", ".pem");
        let err = load_ed25519_private_key_file(&path).unwrap_err();
        assert!(matches!(err, PrivateKeyLoadError::File(_)));
    }

    #[test]
    fn private_key_load_error_display_has_no_secret_bytes() {
        let err = PrivateKeyLoadError::Pkcs8(Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Rsa));
        let text = format!("{err}");
        assert!(text.contains("RSA"));
        assert!(!text.contains("2a8648"));
    }
}
