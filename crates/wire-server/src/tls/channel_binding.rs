//! RFC 5929 §4 の `tls-server-end-point` チャネルバインディング値の算出
//! （TASK-228・WIRE-9・WIRE-18・HTTP-10 ポインタ。Issue #970・親 #941）。
//!
//! SCRAM-SHA-256-PLUS（[`super::super::auth::scram`]。WIRE-18）がチャネル
//! バインディングとして提示・検証する値は、TLS サーバー証明書（葉）の
//! `signatureAlgorithm` に応じたハッシュ関数で、葉証明書の DER 全体を
//! ハッシュしたものである（RFC 5929 §4.1）。本モジュールはこの算出のみを
//! 担う純粋関数層で、TLS 接続状態・SCRAM の交渉判定は一切持たない。
//!
//! `TlsServerConfig::new`（[`super::server_handshake`]。#967 が起動時に
//! 構築する接続点）が起動時に 1 回だけ呼び出し、結果を [`super::server_handshake::
//! TlsSession`]／[`super::stream::TlsStream`]／[`crate::wire_stream::WireStream`]
//! へ伝搬する。非対応の署名アルゴリズムは呼び出し元が起動失敗にはせず
//! `None`（チャネルバインディング非提供）へ縮退させる契約とする
//! （`docs/design/tls-channel-binding.md` 参照）。
//!
//! ただし CLI の `--tls-scram-channel-binding enable`
//! （[`crate::tls_opt::check_scram_channel_binding`]。Issue #1088・
//! WIRE-9・TASK-228）は、この縮退に頼らず起動時に拒否する。
//! [`has_rfc5929_defined_hash`] はその判定に使う「RFC 5929 が定義する
//! ハッシュを持つ署名アルゴリズムか」だけを返す純粋関数で、実際の
//! ハッシュ選択（[`tls_server_end_point`]）や起動拒否の判断そのものは
//! 行わない。
//!
//! ## 定数時間性
//!
//! 証明書は公開データであり、`signatureAlgorithm` の OID による分岐は
//! 秘密値に依存しない（[`super::x509`] の「定数時間性」節と同じ整理）。

use super::x509::MAX_CERTIFICATE_DER_LEN;
use crate::tls::der::{DerError, DerReader};

const TAG_SEQUENCE: u8 = 0x30;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OID: u8 = 0x06;

/// rsaEncryption 系署名アルゴリズムの OID（本モジュール内での比較専用に
/// 個別定義する。[`super::x509`] の同名定数とは独立に持つ）。
const OID_MD5_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x04];
const OID_SHA1_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
const OID_SHA256_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
const OID_SHA512_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
const OID_ECDSA_WITH_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01];
const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OID_ECDSA_WITH_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
/// Ed25519（1.3.101.112。RFC 8410 §3）。RFC 5929 は単一ハッシュを持たない
/// 署名方式を定義していないため、Ed25519 署名の証明書へ SHA-256 を使うのは
/// 本リポの実装既定値（`docs/design/tls-channel-binding.md` 参照）。
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// sha384WithRSAEncryption（1.2.840.113549.1.1.12）。RFC 5929 はこの
/// アルゴリズムに対して SHA-384 を明確に定義しているが、本リポは自作
/// SHA-384 実装を持たないため [`classify_signature_oid`] では分類できない
/// （[`ChannelBindingError::UnsupportedSignatureAlgorithm`] へ縮退）。
/// [`recognized_but_unimplemented_hash_oid`] が「RFC 5929 は定義しているが
/// 本実装が未対応」の理由を Ed25519（RFC 5929 が単一ハッシュを定義しない
/// ケース）と区別するために使う（Issue #1089 レビュー是正）。
const OID_SHA384_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
/// ecdsa-with-SHA384（1.2.840.10045.4.3.3）。[`OID_SHA384_WITH_RSA`] と同じ
/// 理由で「RFC 5929 は定義しているが本実装が未対応」に分類する。
const OID_ECDSA_WITH_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];

/// RFC 5929 が単一ハッシュを定義しない署名アルゴリズムのうち、本リポが
/// [`classify_signature_oid`] で独自にハッシュを割り当てているものの一覧
/// （現時点では Ed25519 のみ）。[`has_rfc5929_defined_hash`] が「CLI の
/// `--tls-scram-channel-binding enable` を許容してよい構成か」を判定する
/// 際に、この独自割り当てを「RFC が定義したハッシュ」とは区別するために使う
/// （Issue #1088）。
const IMPLEMENTATION_ASSIGNED_SIGNATURE_OIDS: [&[u8]; 1] = [OID_ED25519];

/// RFC 5929 がハッシュを明確に定義しているが、本リポがそのハッシュ関数
/// （SHA-384）を自作していないために [`classify_signature_oid`] が分類
/// できない署名アルゴリズムの一覧（Issue #1089 レビュー是正）。
/// [`IMPLEMENTATION_ASSIGNED_SIGNATURE_OIDS`]（Ed25519 = RFC 5929 に
/// そもそも定義が無い）とは拒否理由が異なるため区別する。
const RFC5929_DEFINED_BUT_UNIMPLEMENTED_HASH_OIDS: [&[u8]; 2] =
    [OID_SHA384_WITH_RSA, OID_ECDSA_WITH_SHA384];

/// [`tls_server_end_point`] が返すハッシュ値（署名アルゴリズムに応じて
/// SHA-256／SHA-512 のいずれか。長さ固定のため可変長 `Vec` を使わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsServerEndPoint {
    Sha256([u8; 32]),
    Sha512([u8; 64]),
}

impl TlsServerEndPoint {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            TlsServerEndPoint::Sha256(digest) => digest.as_slice(),
            TlsServerEndPoint::Sha512(digest) => digest.as_slice(),
        }
    }
}

/// [`tls_server_end_point`] の拒否理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelBindingError {
    /// `Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
    /// signatureValue BIT STRING }` の外側構造が DER として不正、または
    /// DER 長が [`MAX_CERTIFICATE_DER_LEN`] を超える。
    Malformed,
    /// `signatureAlgorithm` の OID が下表（本モジュール内の対応表）に無い。
    UnsupportedSignatureAlgorithm,
}

impl std::fmt::Display for ChannelBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelBindingError::Malformed => {
                write!(f, "certificate DER is malformed for channel binding")
            }
            ChannelBindingError::UnsupportedSignatureAlgorithm => write!(
                f,
                "certificate signature algorithm is not supported for channel binding"
            ),
        }
    }
}

impl std::error::Error for ChannelBindingError {}

/// `signatureAlgorithm` OID から、`tls-server-end-point`（RFC 5929 §4.1）が
/// 使うべきハッシュ関数を選ぶ。表に無い OID は fail-closed に拒否する
/// （推測で別のハッシュを当てない）。
enum HashChoice {
    Sha256,
    Sha512,
}

fn classify_signature_oid(oid: &[u8]) -> Option<HashChoice> {
    if oid == OID_MD5_WITH_RSA
        || oid == OID_SHA1_WITH_RSA
        || oid == OID_ECDSA_WITH_SHA1
        || oid == OID_SHA256_WITH_RSA
        || oid == OID_ECDSA_WITH_SHA256
        || oid == OID_ED25519
    {
        Some(HashChoice::Sha256)
    } else if oid == OID_SHA512_WITH_RSA || oid == OID_ECDSA_WITH_SHA512 {
        Some(HashChoice::Sha512)
    } else {
        None
    }
}

/// `AlgorithmIdentifier ::= SEQUENCE { algorithm OBJECT IDENTIFIER,
/// parameters ANY OPTIONAL }` の構造を検査し、OID の値部分を返す
/// （[`super::x509::validate_algorithm_identifier_structure`] と同じ形の
/// 検査を、本モジュール独立の `DerReader` 呼び出しとして行う）。
fn read_algorithm_oid(tlv_value: &[u8]) -> Result<&[u8], ChannelBindingError> {
    let mut reader = DerReader::new(tlv_value);
    let oid = reader
        .read_expected(TAG_OID)
        .map_err(|_| ChannelBindingError::Malformed)?;
    if !reader.is_empty() {
        // parameters ANY: 中身の意味は解釈しないが、1 個の TLV として
        // 整形式であることだけ要求する。
        reader
            .read_any()
            .map_err(|_: DerError| ChannelBindingError::Malformed)?;
    }
    reader
        .expect_end()
        .map_err(|_| ChannelBindingError::Malformed)?;
    Ok(oid)
}

/// `cert_der`（葉証明書の DER 全体）から `signatureAlgorithm` の OID を
/// 取り出す。[`tls_server_end_point`]・[`has_rfc5929_defined_hash`] が共有
/// する検査手順（Issue #1088 で切り出し。挙動はそれまでの
/// `tls_server_end_point` 内インライン処理とビット同一）:
///
/// 1. `cert_der.len() > MAX_CERTIFICATE_DER_LEN` を確保前に拒否する
/// 2. `Certificate` 外側 `SEQUENCE` を読み、`tbsCertificate`（`SEQUENCE`。
///    中身は解釈しない）・`signatureAlgorithm`（`SEQUENCE`）・
///    `signatureValue`（`BIT STRING`）の 3 要素のみで構成されることを
///    検査する（後続バイトは拒否）
/// 3. `signatureAlgorithm` の中身から OID を読み取って返す
fn signature_algorithm_oid(cert_der: &[u8]) -> Result<&[u8], ChannelBindingError> {
    if cert_der.len() > MAX_CERTIFICATE_DER_LEN {
        return Err(ChannelBindingError::Malformed);
    }

    let mut outer = DerReader::new(cert_der);
    let certificate_seq = outer
        .read_expected(TAG_SEQUENCE)
        .map_err(|_| ChannelBindingError::Malformed)?;
    outer
        .expect_end()
        .map_err(|_| ChannelBindingError::Malformed)?;

    let mut fields = DerReader::new(certificate_seq);
    let _tbs_certificate = fields
        .read_expected(TAG_SEQUENCE)
        .map_err(|_| ChannelBindingError::Malformed)?;
    let signature_algorithm = fields
        .read_expected(TAG_SEQUENCE)
        .map_err(|_| ChannelBindingError::Malformed)?;
    let _signature_value = fields
        .read_expected(TAG_BIT_STRING)
        .map_err(|_| ChannelBindingError::Malformed)?;
    fields
        .expect_end()
        .map_err(|_| ChannelBindingError::Malformed)?;

    read_algorithm_oid(signature_algorithm)
}

/// RFC 5929 §4 の `tls-server-end-point` チャネルバインディング値を算出する。
///
/// `cert_der` は葉証明書の DER 全体（[`super::x509::ServerCertificateChain::
/// leaf_der`] が返す値）を想定する。[`signature_algorithm_oid`] で
/// `signatureAlgorithm` の OID を取り出し、[`classify_signature_oid`] で
/// ハッシュへ分類したうえで `cert_der` 全体をそのハッシュでダイジェストする。
pub fn tls_server_end_point(cert_der: &[u8]) -> Result<TlsServerEndPoint, ChannelBindingError> {
    let oid = signature_algorithm_oid(cert_der)?;
    match classify_signature_oid(oid) {
        Some(HashChoice::Sha256) => Ok(TlsServerEndPoint::Sha256(engine::crypto::sha256::digest(
            cert_der,
        ))),
        Some(HashChoice::Sha512) => Ok(TlsServerEndPoint::Sha512(super::sha512::digest(cert_der))),
        None => Err(ChannelBindingError::UnsupportedSignatureAlgorithm),
    }
}

/// [`rfc5929_hash_gap`] が返す、`--tls-scram-channel-binding enable` を
/// 拒否すべき理由の分類（Issue #1088 の是正・Issue #1089 レビュー指摘）。
/// `tls_opt::check_scram_channel_binding` がこれを見て、運用者へ提示する
/// 拒否理由を「証明書を別の CA で再発行すべきケース」（RFC 5929 に単一
/// ハッシュの定義が無い）と「本実装側の制約であるケース」（RFC 5929 は
/// ハッシュを定義しているが自作実装が未対応）とで区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rfc5929HashGap {
    /// 署名アルゴリズム（Ed25519 など）に RFC 5929 が単一ハッシュを定義
    /// していない。RSA／ECDSA 系の CA が署名した葉証明書へ替える以外に
    /// 解決策が無い。
    NotDefinedByRfc5929,
    /// RFC 5929 はハッシュ（SHA-384）を明確に定義しているが、本リポが
    /// そのハッシュ関数を自作していないため算出できない
    /// （[`RFC5929_DEFINED_BUT_UNIMPLEMENTED_HASH_OIDS`]）。CA の変更では
    /// なく、証明書の署名ハッシュを本実装が対応する SHA-256／SHA-512 系へ
    /// 変えることで解決する（もしくは本実装への SHA-384 対応追加を待つ）。
    UnsupportedHashAlgorithm,
    /// DER が整形式でない、または [`classify_signature_oid`] の表・上記
    /// いずれの一覧にも無い未知の OID（fail-closed。原因を確定できない）。
    Unknown,
}

/// `cert_der`（葉証明書の DER 全体）の `signatureAlgorithm` が RFC 5929 の
/// 定義するハッシュを持つ署名アルゴリズムかを判定し、そうでない場合は
/// 理由（[`Rfc5929HashGap`]）を返す（Issue #1088・#1089・WIRE-9・
/// TASK-228）。`has_rfc5929_defined_hash` は本関数の bool 版。
pub fn rfc5929_hash_gap(cert_der: &[u8]) -> Option<Rfc5929HashGap> {
    let Ok(oid) = signature_algorithm_oid(cert_der) else {
        return Some(Rfc5929HashGap::Unknown);
    };
    if IMPLEMENTATION_ASSIGNED_SIGNATURE_OIDS.contains(&oid) {
        return Some(Rfc5929HashGap::NotDefinedByRfc5929);
    }
    if RFC5929_DEFINED_BUT_UNIMPLEMENTED_HASH_OIDS.contains(&oid) {
        return Some(Rfc5929HashGap::UnsupportedHashAlgorithm);
    }
    if classify_signature_oid(oid).is_some() {
        None
    } else {
        Some(Rfc5929HashGap::Unknown)
    }
}

/// `cert_der`（葉証明書の DER 全体）の `signatureAlgorithm` が、RFC 5929
/// が定義するハッシュを持つ署名アルゴリズムかを判定する（Issue #1088・
/// WIRE-9・TASK-228）。`tls_opt::check_scram_channel_binding` が
/// `--tls-scram-channel-binding enable` の起動時拒否判定に使う。
///
/// `false` を返すのは次のいずれか（fail-closed。曖昧な場合は「RFC が
/// 定義していない」側に倒す）。理由を区別したい呼び出し元は
/// [`rfc5929_hash_gap`] を使う:
/// - DER が整形式でない、または非対応の OID（[`classify_signature_oid`]
///   の表に無い。例: sha384WithRSA）である
/// - OID が [`IMPLEMENTATION_ASSIGNED_SIGNATURE_OIDS`]（本リポの独自割り当て。
///   現時点では Ed25519 のみ）に含まれる
pub fn has_rfc5929_defined_hash(cert_der: &[u8]) -> bool {
    rfc5929_hash_gap(cert_der).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = body.len();
        if len < 0x80 {
            out.push(len as u8);
        } else if len < 0x100 {
            out.push(0x81);
            out.push(len as u8);
        } else {
            out.push(0x82);
            out.push((len >> 8) as u8);
            out.push((len & 0xff) as u8);
        }
        out.extend_from_slice(body);
        out
    }

    fn sequence(parts: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(part);
        }
        tlv(TAG_SEQUENCE, &body)
    }

    fn algorithm_identifier(oid: &[u8]) -> Vec<u8> {
        sequence(&[&tlv(TAG_OID, oid)])
    }

    /// 最小の合成 `Certificate` DER（`tbsCertificate` は中身を持たない空
    /// `SEQUENCE` で十分。本モジュールは中身を解釈しない）。署名値も
    /// 空のダミー `BIT STRING` で足りる。
    fn synthetic_certificate_der(signature_oid: &[u8]) -> Vec<u8> {
        let tbs_certificate = sequence(&[]);
        let signature_algorithm = algorithm_identifier(signature_oid);
        let signature_value = tlv(TAG_BIT_STRING, &[0x00, 0xaa]);
        sequence(&[&tbs_certificate, &signature_algorithm, &signature_value])
    }

    #[test]
    fn sha256_signature_algorithms_hash_with_sha256() {
        for oid in [
            OID_MD5_WITH_RSA,
            OID_SHA1_WITH_RSA,
            OID_ECDSA_WITH_SHA1,
            OID_SHA256_WITH_RSA,
            OID_ECDSA_WITH_SHA256,
            OID_ED25519,
        ] {
            let der = synthetic_certificate_der(oid);
            let result = tls_server_end_point(&der).expect("supported algorithm");
            let expected = engine::crypto::sha256::digest(&der);
            match result {
                TlsServerEndPoint::Sha256(digest) => assert_eq!(digest, expected),
                TlsServerEndPoint::Sha512(_) => panic!("expected SHA-256 for OID {oid:?}"),
            }
        }
    }

    #[test]
    fn sha512_signature_algorithms_hash_with_sha512() {
        for oid in [OID_SHA512_WITH_RSA, OID_ECDSA_WITH_SHA512] {
            let der = synthetic_certificate_der(oid);
            let result = tls_server_end_point(&der).expect("supported algorithm");
            let expected = crate::tls::sha512::digest(&der);
            match result {
                TlsServerEndPoint::Sha512(digest) => assert_eq!(digest, expected),
                TlsServerEndPoint::Sha256(_) => panic!("expected SHA-512 for OID {oid:?}"),
            }
        }
    }

    #[test]
    fn unsupported_signature_algorithm_is_rejected() {
        // sha384WithRSAEncryption（1.2.840.113549.1.1.12）。自作 SHA-384 が
        // 無いため非対応（`docs/design/tls-channel-binding.md` 参照）。
        let oid: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
        let der = synthetic_certificate_der(oid);
        assert_eq!(
            tls_server_end_point(&der),
            Err(ChannelBindingError::UnsupportedSignatureAlgorithm)
        );
    }

    #[test]
    fn has_rfc5929_defined_hash_is_true_for_rfc_defined_algorithms() {
        // Issue #1088: RFC 5929 が単一ハッシュを定義する署名アルゴリズム
        // （本モジュールの独自割り当てである Ed25519 を除く）はすべて
        // `true` を返す。
        for oid in [
            OID_MD5_WITH_RSA,
            OID_SHA1_WITH_RSA,
            OID_ECDSA_WITH_SHA1,
            OID_SHA256_WITH_RSA,
            OID_ECDSA_WITH_SHA256,
            OID_SHA512_WITH_RSA,
            OID_ECDSA_WITH_SHA512,
        ] {
            let der = synthetic_certificate_der(oid);
            assert!(
                has_rfc5929_defined_hash(&der),
                "expected OID {oid:?} to have an RFC 5929 defined hash"
            );
        }
    }

    #[test]
    fn has_rfc5929_defined_hash_is_false_for_ed25519() {
        // Issue #1088: Ed25519 は本モジュールの独自割り当て（SHA-256）で
        // あって RFC 5929 が定義したハッシュではないため `false`。
        let der = synthetic_certificate_der(OID_ED25519);
        assert!(!has_rfc5929_defined_hash(&der));
    }

    #[test]
    fn rfc5929_hash_gap_distinguishes_ed25519_from_sha384_and_unknown() {
        // Issue #1089 レビュー是正: 「RFC 5929 に定義が無い」（Ed25519）と
        // 「RFC 5929 は定義しているが本実装が未対応」（sha384WithRSA・
        // ecdsa-with-SHA384）は拒否理由が異なるため、`rfc5929_hash_gap` が
        // 別の variant を返すことを固定する。
        let ed25519_der = synthetic_certificate_der(OID_ED25519);
        assert_eq!(
            rfc5929_hash_gap(&ed25519_der),
            Some(Rfc5929HashGap::NotDefinedByRfc5929)
        );

        let sha384_rsa_oid: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
        let sha384_rsa_der = synthetic_certificate_der(sha384_rsa_oid);
        assert_eq!(
            rfc5929_hash_gap(&sha384_rsa_der),
            Some(Rfc5929HashGap::UnsupportedHashAlgorithm)
        );

        let ecdsa_sha384_oid: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
        let ecdsa_sha384_der = synthetic_certificate_der(ecdsa_sha384_oid);
        assert_eq!(
            rfc5929_hash_gap(&ecdsa_sha384_der),
            Some(Rfc5929HashGap::UnsupportedHashAlgorithm)
        );

        // まったく未知の OID・不正な DER は `Unknown`（fail-closed）。
        let bogus_oid: &[u8] = &[0x2a, 0x03, 0x04];
        let bogus_der = synthetic_certificate_der(bogus_oid);
        assert_eq!(rfc5929_hash_gap(&bogus_der), Some(Rfc5929HashGap::Unknown));

        let oversized = vec![0u8; MAX_CERTIFICATE_DER_LEN + 1];
        assert_eq!(rfc5929_hash_gap(&oversized), Some(Rfc5929HashGap::Unknown));

        // RFC 5929 が定義し、本実装も対応済みのアルゴリズムは `None`。
        let sha256_rsa_der = synthetic_certificate_der(OID_SHA256_WITH_RSA);
        assert_eq!(rfc5929_hash_gap(&sha256_rsa_der), None);
    }

    #[test]
    fn has_rfc5929_defined_hash_is_false_for_unsupported_or_malformed() {
        // sha384WithRSAEncryption（非対応 OID）。
        let unsupported_oid: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
        let der = synthetic_certificate_der(unsupported_oid);
        assert!(!has_rfc5929_defined_hash(&der));

        // 構造違反（長さ超過）は fail-closed に `false`。
        let oversized = vec![0u8; MAX_CERTIFICATE_DER_LEN + 1];
        assert!(!has_rfc5929_defined_hash(&oversized));

        // 切り詰め DER（malformed）。
        let truncated_source = synthetic_certificate_der(OID_SHA256_WITH_RSA);
        let truncated = &truncated_source[..truncated_source.len() - 1];
        assert!(!has_rfc5929_defined_hash(truncated));
    }

    #[test]
    fn oversized_certificate_der_is_rejected_before_parsing() {
        let der = vec![0u8; MAX_CERTIFICATE_DER_LEN + 1];
        assert_eq!(
            tls_server_end_point(&der),
            Err(ChannelBindingError::Malformed)
        );
    }

    #[test]
    fn trailing_bytes_after_certificate_are_rejected() {
        let mut der = synthetic_certificate_der(OID_ED25519);
        der.push(0x00);
        assert_eq!(
            tls_server_end_point(&der),
            Err(ChannelBindingError::Malformed)
        );
    }

    #[test]
    fn truncated_certificate_is_rejected() {
        let der = synthetic_certificate_der(OID_ED25519);
        let truncated = &der[..der.len() - 1];
        assert_eq!(
            tls_server_end_point(truncated),
            Err(ChannelBindingError::Malformed)
        );
    }

    #[test]
    fn signature_algorithm_with_extra_trailing_element_is_rejected() {
        // AlgorithmIdentifier に OID + parameters（高々 1 個）を超える
        // 余剰 TLV を持つ不正な構造（`super::x509::
        // validate_algorithm_identifier_structure` が拒否する形と同じ）。
        let malformed_algorithm =
            sequence(&[&tlv(TAG_OID, OID_ED25519), &tlv(0x05, &[]), &tlv(0x05, &[])]);
        let tbs_certificate = sequence(&[]);
        let signature_value = tlv(TAG_BIT_STRING, &[0x00, 0xaa]);
        let der = sequence(&[&tbs_certificate, &malformed_algorithm, &signature_value]);
        assert_eq!(
            tls_server_end_point(&der),
            Err(ChannelBindingError::Malformed)
        );
    }

    /// RFC 8410 §10.2 の公開テストベクタ（SPKI は X25519 だが署名は
    /// Ed25519。`tests/tls_x509.rs::RFC8410_10_2_X25519_CERT_PEM` と同一の
    /// PEM で、[`super::super::pem::decode_certificate_chain_pem`]〔本番の
    /// PEM デコーダ〕でそのまま読み直せることも兼ねて確認する）。OpenSSL
    /// `x509 -fingerprint -sha256` で独立に算出済みの `tls-server-end-point`
    /// 値と一致することを固定する（本モジュールが署名の主体
    /// `signatureAlgorithm` だけを見て SPKI アルゴリズムは見ないことの
    /// 確認も兼ねる）。
    const RFC8410_10_2_X25519_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBLDCB36ADAgECAghWAUdKKo3DMDAFBgMrZXAwGTEXMBUGA1UEAwwOSUVURiBUZX\n\
N0IERlbW8wHhcNMTYwODAxMTIxOTI0WhcNNDAxMjMxMjM1OTU5WjAZMRcwFQYDVQQD\n\
DA5JRVRGIFRlc3QgRGVtbzAqMAUGAytlbgMhAIUg8AmJMKdUdIt93LQ+91oNvzoNJj\n\
ga9OukqY6qm05qo0UwQzAPBgNVHRMBAf8EBTADAQEAMA4GA1UdDwEBAAQEAwIDCDAg\n\
BgNVHQ4BAQAEFgQUmx9e7e0EM4Xk97xiPFl1uQvIuzswBQYDK2VwA0EAryMB/t3J5v\n\
/BzKc9dNZIpDmAgs3babFOTQbs+BolzlDUwsPrdGxO3YNGhW7Ibz3OGhhlxXrCe1Cg\n\
w1AH9efZBw==\n\
-----END CERTIFICATE-----\n";

    #[test]
    fn rfc8410_10_2_certificate_matches_independently_computed_sha256() {
        let chain =
            crate::tls::pem::decode_certificate_chain_pem(RFC8410_10_2_X25519_CERT_PEM.as_bytes())
                .expect("valid PEM chain");
        let der = chain.first().expect("single certificate").clone();
        let result = tls_server_end_point(&der).expect("Ed25519 signature is supported");
        // OpenSSL `x509 -in cert.pem -noout -fingerprint -sha256` で独立に
        // 算出した値（コロン区切りの16進を連結したもの）。
        let expected_hex = "180516f0a03e4893d234a28f3ad28921bc35d1b12bd35134847240dafb715a11";
        let expected = hex_decode(expected_hex);
        match result {
            TlsServerEndPoint::Sha256(digest) => assert_eq!(digest.as_slice(), expected.as_slice()),
            TlsServerEndPoint::Sha512(_) => panic!("expected SHA-256 for Ed25519 signature"),
        }
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        digits
            .chunks(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ascii hex pair");
                u8::from_str_radix(text, 16).expect("valid hex byte")
            })
            .collect()
    }
}
