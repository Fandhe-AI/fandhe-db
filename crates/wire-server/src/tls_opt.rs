//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る TLS opt-in CLI
//! 引数（`--tls-cert`／`--tls-key`／`--tls-mode`／`--tls-scram-channel-
//! binding`）の閉じた語彙パーサとサーバー証明書・鍵の読み込み手順
//! （Issue #967・#970・親 #941・TASK-228。対象ビヘイビア WIRE-7, WIRE-9,
//! WIRE-18, HTTP-9, HTTP-10）。
//!
//! `--durability`（[`crate::durability_opt`]）・`--search-engine`
//! （[`crate::search_engine_opt`]）と同型の「プロセス起動時にのみ明示指定
//! する注入点」であり、`crate::tls::server_handshake::TlsServerConfig`
//! （Issue #965）へ untrusted な CLI 文字列・ファイルパスから到達する
//! 唯一の入口を本モジュールに置く。
//!
//! `--tls-mode` は TLS ハンドシェイク成立後の平文接続ポリシーではなく、
//! **`SSLRequest` を経ない平文 StartupMessage** を受理するか（`allow`）
//! 拒否するか（`require`）を選ぶ（`crate::handshake::
//! handle_connection_with_tls_mode` が参照する）。証明書・鍵ファイルの
//! バイト列・長さは本モジュールの外（ログ・エラーメッセージ）へ一切出さない。
//!
//! `--tls-scram-channel-binding`（`enable`／`disable`。既定 `disable`）は
//! SCRAM-SHA-256-PLUS（`p=tls-server-end-point`）を機構リストへ提示する
//! か（`TlsServerConfig::with_scram_channel_binding`。Issue #970）を CLI
//! から切り替える唯一の入口。`--tls-cert`／`--tls-key` を指定したときのみ
//! 意味を持ち、単独指定は `--tls-mode` と同じ理由で組合せ不正として
//! fail-closed 拒否する（`main::resolve_tls_options` 参照）。既定は
//! `disable`。`enable` を選び、かつ葉証明書の署名アルゴリズムに RFC 5929
//! が定義するハッシュが無い（Ed25519 など）場合は、libpq の
//! `channel_binding=prefer`／`require` が接続失敗しうる（`docs/design/
//! tls-channel-binding.md` の実測結果参照）ため、`main.rs` が
//! [`check_scram_channel_binding`] で起動時に fail-closed 拒否する
//! （Issue #1088）。RSA／ECDSA の CA が署名した葉証明書（Ed25519 鍵でも
//! 署名 OID が RSA／ECDSA 系であればよい）を使う構成では `enable` を許可する。

use std::path::Path;
use std::sync::Arc;

use crate::tls::ed25519::SigningKey;
use crate::tls::pkcs8::{self, PrivateKeyLoadError};
use crate::tls::server_handshake::{TlsServerConfig, TlsServerConfigError};
use crate::tls::x509::{self, ServerCertificateLoadError};

/// `--tls-cert` の CLI フラグ名。
pub const CERT_FLAG: &str = "--tls-cert";
/// `--tls-key` の CLI フラグ名。
pub const KEY_FLAG: &str = "--tls-key";
/// `--tls-mode` の CLI フラグ名。
pub const MODE_FLAG: &str = "--tls-mode";

/// `--tls-mode` が受理する語彙（順序は `parse` の分岐・エラーメッセージの
/// 一覧順・README 記載順の単一情報源。`durability_opt::TOKENS` と同じ流儀）。
pub const MODE_TOKENS: [&str; 2] = ["require", "allow"];

/// `--tls-scram-channel-binding` の CLI フラグ名（Issue #970）。
pub const SCRAM_CHANNEL_BINDING_FLAG: &str = "--tls-scram-channel-binding";

/// `--tls-scram-channel-binding` が受理する語彙（`MODE_TOKENS` と同じ流儀。
/// 既定は `disable` 相当。`main::resolve_tls_options` が未指定時にこの既定を
/// 適用する）。
pub const SCRAM_CHANNEL_BINDING_TOKENS: [&str; 2] = ["enable", "disable"];

/// `SSLRequest` を経ない平文 StartupMessage の受理ポリシー。
///
/// `#[non_exhaustive]` は将来 variant を追加する際、下流クレートの
/// exhaustive match を破壊しないための予防措置（`bind_guard::
/// TransportSecurity` と同じ方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsMode {
    /// `SSLRequest` を経ない平文 StartupMessage を `08P01` で拒否する
    /// （startup パラメータを解釈する前に拒否する。WIRE-9）。
    Require,
    /// 平文 StartupMessage も受理する（TLS 未設定時の既存経路と同じ
    /// 受理判定。非ループバック bind と組み合わせると `bind_guard` が
    /// 起動時に拒否する）。
    Allow,
}

impl TlsMode {
    /// [`MODE_TOKENS`] のうち `self` に対応する文字列表現（診断メッセージ用）。
    pub fn token(self) -> &'static str {
        match self {
            TlsMode::Require => "require",
            TlsMode::Allow => "allow",
        }
    }
}

/// `raw`（CLI 引数の値）を [`MODE_TOKENS`] の厳密一致でのみ受理する
/// （trim・大文字小文字の読み替えはしない。`durability_opt::parse`・
/// `search_engine_opt::parse` と同じ「厳密一致のみ受理」方針）。
pub fn parse(raw: &str) -> Result<TlsMode, String> {
    match raw {
        "require" => Ok(TlsMode::Require),
        "allow" => Ok(TlsMode::Allow),
        other => Err(format!(
            "{MODE_FLAG} must be one of {MODE_TOKENS:?} (got {other:?})"
        )),
    }
}

/// `raw`（CLI 引数の値）を [`SCRAM_CHANNEL_BINDING_TOKENS`] の厳密一致でのみ
/// 受理し、`TlsServerConfig::with_scram_channel_binding` へ渡す `bool` へ
/// 変換する（`parse` と同じ「厳密一致のみ受理」方針）。
pub fn parse_scram_channel_binding(raw: &str) -> Result<bool, String> {
    match raw {
        "enable" => Ok(true),
        "disable" => Ok(false),
        other => Err(format!(
            "{SCRAM_CHANNEL_BINDING_FLAG} must be one of {SCRAM_CHANNEL_BINDING_TOKENS:?} (got {other:?})"
        )),
    }
}

/// [`load_server_config`] の失敗理由。`Display` は各内部エラー型の内容
/// 非依存な `Display` へ委譲し、鍵・証明書の内容・長さは含めない
/// （呼び出し元がフラグ名を前置してエラーメッセージを組み立てる）。
#[derive(Debug)]
pub enum TlsConfigLoadError {
    Key(PrivateKeyLoadError),
    Clock,
    Certificate(ServerCertificateLoadError),
    Mismatch(TlsServerConfigError),
}

impl std::fmt::Display for TlsConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsConfigLoadError::Key(e) => write!(f, "{e}"),
            TlsConfigLoadError::Clock => {
                write!(f, "failed to read the current time for validity checks")
            }
            TlsConfigLoadError::Certificate(e) => write!(f, "{e}"),
            TlsConfigLoadError::Mismatch(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TlsConfigLoadError {}

/// [`check_scram_channel_binding`] の拒否理由（Issue #1088・WIRE-9・
/// TASK-228）。`#[non_exhaustive]`: crates.io 公開対象（`release.yml`）の
/// public enum へ将来 variant を追加する際、下流の exhaustive match を
/// 破壊しないための予防措置（`TlsMode`・`TlsServerConfigError` と同じ方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScramChannelBindingRejection {
    /// `--tls-scram-channel-binding enable` が指定されたが、葉証明書の
    /// 署名アルゴリズムに RFC 5929 が単一ハッシュを定義していない
    /// （Ed25519 など。[`crate::tls::channel_binding::Rfc5929HashGap::
    /// NotDefinedByRfc5929`]）ため、libpq の `channel_binding=prefer`／
    /// `require` が接続失敗しうる構成。CA を RSA／ECDSA 系へ替える以外に
    /// 解決策が無い。
    NoRfc5929Hash,
    /// `--tls-scram-channel-binding enable` が指定され、葉証明書の署名
    /// アルゴリズムに RFC 5929 がハッシュ（SHA-384）を定義してはいるが、
    /// 本リポがそのハッシュ関数を自作していないため算出できない
    /// （[`crate::tls::channel_binding::Rfc5929HashGap::
    /// UnsupportedHashAlgorithm`]）。`NoRfc5929Hash` と異なり CA の変更は
    /// 不要で、証明書の署名ハッシュを本実装が対応する SHA-256／SHA-512 系
    /// （例: `sha256WithRSAEncryption`）へ揃えれば解決する（Issue #1089
    /// レビュー是正: 拒否理由を運用者が誤認しないよう区別する）。
    UnsupportedHashAlgorithm,
    /// 葉証明書の署名アルゴリズムを判定できなかった（DER が整形式でない、
    /// または未知の OID）。fail-closed で拒否する。
    UnknownSignatureAlgorithm,
}

impl std::fmt::Display for ScramChannelBindingRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScramChannelBindingRejection::NoRfc5929Hash => write!(
                f,
                "{SCRAM_CHANNEL_BINDING_FLAG} enable requires a leaf certificate whose \
                 signature algorithm has a tls-server-end-point hash defined by RFC 5929 \
                 (e.g. RSA/ECDSA with SHA-256); the configured leaf certificate is signed \
                 with an algorithm without one (such as Ed25519), for which libpq clients \
                 cannot compute SCRAM-SHA-256-PLUS channel binding; use \
                 {SCRAM_CHANNEL_BINDING_FLAG} disable or a leaf certificate issued by an \
                 RSA/ECDSA CA (see docs/design/tls-channel-binding.md)"
            ),
            ScramChannelBindingRejection::UnsupportedHashAlgorithm => write!(
                f,
                "{SCRAM_CHANNEL_BINDING_FLAG} enable requires a leaf certificate signature \
                 hash this server implements for tls-server-end-point; RFC 5929 does define \
                 a hash for the configured leaf certificate's signature algorithm (e.g. \
                 SHA-384), but this server has not implemented that hash function, so \
                 SCRAM-SHA-256-PLUS channel binding cannot be computed; use \
                 {SCRAM_CHANNEL_BINDING_FLAG} disable or reissue the leaf certificate with a \
                 signature hash this server supports (e.g. SHA-256/SHA-512 with RSA/ECDSA; \
                 see docs/design/tls-channel-binding.md)"
            ),
            ScramChannelBindingRejection::UnknownSignatureAlgorithm => write!(
                f,
                "{SCRAM_CHANNEL_BINDING_FLAG} enable requires a leaf certificate with a \
                 signature algorithm this server can recognize for tls-server-end-point; the \
                 configured leaf certificate's signature algorithm could not be determined; \
                 use {SCRAM_CHANNEL_BINDING_FLAG} disable or a certificate with a recognized \
                 RSA/ECDSA signature algorithm (see docs/design/tls-channel-binding.md)"
            ),
        }
    }
}

impl std::error::Error for ScramChannelBindingRejection {}

/// `--tls-scram-channel-binding`（`enabled`）と `config`（読み込み済み
/// [`TlsServerConfig`]）の組合せが起動を許容できるかを判定する
/// （Issue #1088。`main.rs` が TLS 設定読み込み直後・bind 検証より前に
/// 1 回だけ呼ぶ）。
///
/// `enabled == false`（既定 `disable`・明示 `disable` のいずれも）は常に
/// `Ok(())`（R2: 挙動を一切変えない契約を型のうえでも自明にする）。
/// `enabled == true` は、葉証明書の署名アルゴリズムに RFC 5929 が定義する
/// ハッシュが無い場合（[`TlsServerConfig::tls_server_end_point_is_rfc5929_
/// defined`] が `false`）に限り拒否する。判定は `--auth-method` に依存
/// させない（`cleartext` との組合せでも同じく拒否し、「フラグが効かない
/// まま受理される」経路を作らない。fail-closed）。
pub fn check_scram_channel_binding(
    config: &TlsServerConfig,
    enabled: bool,
) -> Result<(), ScramChannelBindingRejection> {
    use crate::tls::channel_binding::Rfc5929HashGap;

    if !enabled {
        return Ok(());
    }
    match config.tls_server_end_point_rfc5929_gap() {
        None => Ok(()),
        Some(Rfc5929HashGap::NotDefinedByRfc5929) => {
            Err(ScramChannelBindingRejection::NoRfc5929Hash)
        }
        Some(Rfc5929HashGap::UnsupportedHashAlgorithm) => {
            Err(ScramChannelBindingRejection::UnsupportedHashAlgorithm)
        }
        Some(Rfc5929HashGap::Unknown) => {
            Err(ScramChannelBindingRejection::UnknownSignatureAlgorithm)
        }
    }
}

/// `cert`（証明書チェーン PEM）・`key`（Ed25519 PKCS#8 秘密鍵 PEM）から
/// [`TlsServerConfig`] を構築する（Issue #967）。`PLUS` 提示は既定 `false`
/// （非提示）のまま構築する後方互換の薄いラッパーで、実体は
/// [`load_server_config_with_options`] に委譲する（codex-review PR #1087
/// P1 是正: 公開関数へ必須引数を追加すると既存呼び出し元を破壊するため、
/// 2 引数のシグネチャ自体は変えない）。
///
/// 手順（鍵を先に読むのは、葉証明書の公開鍵照合に公開鍵が要るため）:
/// 1. `key` から Ed25519 seed を読み、署名鍵を導出する。
/// 2. 現在時刻（エポック秒）を取得する。
/// 3. `cert` を検証付きで読む（署名鍵の公開鍵との一致・有効期限を含む）。
/// 4. `TlsServerConfig::new` で組み立てる（公開鍵の再照合は定数時間）。
///
/// `Ed25519Seed`／`SigningKey` は複製せず、鍵のバイト列・長さをエラー
/// メッセージへ出さない（`Ed25519Seed` の `Drop` ゼロ化を活かす）。
pub fn load_server_config(cert: &Path, key: &Path) -> Result<TlsServerConfig, TlsConfigLoadError> {
    load_server_config_with_options(cert, key, false)
}

/// [`load_server_config`] に `--tls-scram-channel-binding`（Issue #970）の
/// 解決済み値を追加で渡せる入口。`main.rs` はこちらを呼ぶ
/// （`handle_connection_with_options`・`accept_loop_with_tls_mode` 等、
/// 既存関数を「オプション追加時は `_with_options` を新設し、元の関数は
/// 既定値で委譲する後方互換ラッパーへ変える」流儀に合わせる）。
///
/// `enable` と、RFC 5929 が定義するハッシュを持たない署名アルゴリズムの
/// 葉証明書（Ed25519 など）の組合せが起動を許容できるかは本関数の責務では
/// なく、返した [`TlsServerConfig`] に対して `main.rs` が
/// [`check_scram_channel_binding`] を別途呼んで判定する（Issue #1088）。
pub fn load_server_config_with_options(
    cert: &Path,
    key: &Path,
    scram_channel_binding: bool,
) -> Result<TlsServerConfig, TlsConfigLoadError> {
    let seed = pkcs8::load_ed25519_private_key_file(key).map_err(TlsConfigLoadError::Key)?;
    let signing_key = SigningKey::from_seed(&seed);
    let public_key = signing_key.public_key();

    let now = x509::current_unix_secs().map_err(|_| TlsConfigLoadError::Clock)?;
    let chain = x509::load_server_certificate_chain_file(cert, &public_key, now)
        .map_err(TlsConfigLoadError::Certificate)?;

    TlsServerConfig::new(chain, signing_key)
        .map(|cfg| cfg.with_scram_channel_binding(scram_channel_binding))
        .map_err(TlsConfigLoadError::Mismatch)
}

/// [`load_server_config`] の結果を `Arc` へ包むヘルパー（`server::
/// accept_loop_with_tls_mode` が要求する型と一致させる）。既定 `false`
/// のまま構築する後方互換ラッパー（実体は
/// [`load_server_config_arc_with_options`]）。
pub fn load_server_config_arc(
    cert: &Path,
    key: &Path,
) -> Result<Arc<TlsServerConfig>, TlsConfigLoadError> {
    load_server_config_arc_with_options(cert, key, false)
}

/// [`load_server_config_arc`] に `--tls-scram-channel-binding` の解決済み
/// 値を追加で渡せる入口（`main.rs` が呼ぶ）。
pub fn load_server_config_arc_with_options(
    cert: &Path,
    key: &Path,
    scram_channel_binding: bool,
) -> Result<Arc<TlsServerConfig>, TlsConfigLoadError> {
    load_server_config_with_options(cert, key, scram_channel_binding).map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_all_tokens() {
        for tok in MODE_TOKENS {
            assert!(parse(tok).is_ok(), "expected {tok:?} to be accepted");
        }
    }

    #[test]
    fn parse_require_and_allow_round_trip() {
        assert_eq!(parse("require"), Ok(TlsMode::Require));
        assert_eq!(parse("allow"), Ok(TlsMode::Allow));
    }

    #[test]
    fn parse_rejects_case_variants_and_whitespace() {
        for raw in [
            "Require",
            "REQUIRE",
            " allow",
            "allow ",
            "Allow",
            "",
            "prefer",
            "require\n",
        ] {
            assert!(
                parse(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_rejects_control_character_injection() {
        // untrusted な引数に制御文字が混じっても厳密一致で弾かれ、エラー文言へ
        // そのまま埋め込まれても Debug 表記（`{:?}`）でエスケープされることを
        // 確認する（`durability_opt::parse` と同じ防御的姿勢）。
        let err = parse("require\0bogus").expect_err("must reject control characters");
        assert!(err.contains(MODE_FLAG));
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in MODE_TOKENS {
            let mode = parse(tok).expect("valid token");
            assert_eq!(mode.token(), tok);
        }
    }

    #[test]
    fn load_server_config_reports_missing_key_file() {
        // `TlsServerConfig`（`Ok` 側）は鍵・証明書を保持するため `Debug` を
        // 導出しない（内容を誤ってログへ出さないための設計判断）。
        // `expect_err` は `Ok` 側に `Debug` を要求するため使わず、`match` で
        // 直接判定する。2 引数版の呼び出しが壊れないことも兼ねて確認する
        // （codex-review PR #1087 P1 是正: 破壊的シグネチャ変更の回帰確認）。
        match load_server_config(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        ) {
            Err(TlsConfigLoadError::Key(_)) => {}
            Err(other) => panic!("expected Key error, got a different error: {other}"),
            Ok(_) => panic!("missing files must fail"),
        }
    }

    #[test]
    fn load_server_config_with_options_reports_missing_key_file() {
        // `_with_options` 入口も同じ失敗を返すことを確認する（薄いラッパー
        // であることの回帰確認）。
        match load_server_config_with_options(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
            true,
        ) {
            Err(TlsConfigLoadError::Key(_)) => {}
            Err(other) => panic!("expected Key error, got a different error: {other}"),
            Ok(_) => panic!("missing files must fail"),
        }
    }

    #[test]
    fn parse_scram_channel_binding_accepts_all_tokens() {
        for tok in SCRAM_CHANNEL_BINDING_TOKENS {
            assert!(
                parse_scram_channel_binding(tok).is_ok(),
                "expected {tok:?} to be accepted"
            );
        }
    }

    #[test]
    fn parse_scram_channel_binding_enable_and_disable_round_trip() {
        assert_eq!(parse_scram_channel_binding("enable"), Ok(true));
        assert_eq!(parse_scram_channel_binding("disable"), Ok(false));
    }

    #[test]
    fn parse_scram_channel_binding_rejects_case_variants_and_whitespace() {
        for raw in [
            "Enable", "ENABLE", " disable", "disable ", "Disable", "", "true", "enable\n",
        ] {
            assert!(
                parse_scram_channel_binding(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_scram_channel_binding_rejects_control_character_injection() {
        let err = parse_scram_channel_binding("enable\0bogus")
            .expect_err("must reject control characters");
        assert!(err.contains(SCRAM_CHANNEL_BINDING_FLAG));
    }

    #[test]
    fn check_scram_channel_binding_disabled_is_always_ok() {
        // Issue #1088・R2: `enabled == false` は葉証明書の内容に関わらず
        // 常に `Ok(())`（既存挙動を一切変えない契約）。
        let ed25519 = tls_client_test_helpers::config_with_ed25519_leaf();
        assert_eq!(check_scram_channel_binding(&ed25519, false), Ok(()));
    }

    #[test]
    fn check_scram_channel_binding_enabled_rejects_ed25519_leaf() {
        let ed25519 = tls_client_test_helpers::config_with_ed25519_leaf();
        assert_eq!(
            check_scram_channel_binding(&ed25519, true),
            Err(ScramChannelBindingRejection::NoRfc5929Hash)
        );
    }

    #[test]
    fn scram_channel_binding_rejection_display_mentions_flag_and_rfc_and_ed25519() {
        let message = ScramChannelBindingRejection::NoRfc5929Hash.to_string();
        assert!(message.contains(SCRAM_CHANNEL_BINDING_FLAG));
        assert!(message.contains("RFC 5929"));
        assert!(message.contains("Ed25519"));
    }

    #[test]
    fn check_scram_channel_binding_enabled_rejects_sha384_rsa_signed_leaf_with_distinct_reason() {
        // Issue #1089 レビュー是正: sha384WithRSA 署名の葉証明書は
        // Ed25519（`NoRfc5929Hash`）とは異なる理由
        // （`UnsupportedHashAlgorithm`）で拒否される。RFC 5929 自体は
        // ハッシュを定義しているが、本実装が SHA-384 を自作していないだけ
        // なので、運用者向けメッセージも CA 変更ではなく署名ハッシュの
        // 変更を案内する内容にする。
        let sha384_rsa = tls_client_test_helpers::config_with_sha384_rsa_signed_leaf();
        assert_eq!(
            check_scram_channel_binding(&sha384_rsa, true),
            Err(ScramChannelBindingRejection::UnsupportedHashAlgorithm)
        );
    }

    #[test]
    fn scram_channel_binding_rejection_display_distinguishes_unsupported_hash_from_ed25519() {
        // sha384WithRSA の拒否メッセージは「RFC 5929 に定義が無い」とは
        // 言わず（Ed25519 用の文言と混同させない）、本実装側の制約として
        // SHA-384 に言及する。
        let message = ScramChannelBindingRejection::UnsupportedHashAlgorithm.to_string();
        assert!(message.contains(SCRAM_CHANNEL_BINDING_FLAG));
        assert!(message.contains("RFC 5929"));
        assert!(message.contains("SHA-384"));
        assert!(!message.contains("Ed25519"));
        assert!(message.contains("has not implemented"));
    }

    /// 本 crate の `tests/common/tls_client.rs` は統合テスト専用で
    /// unit テストから import できないため、`tls_opt` の unit テストに必要な
    /// 最小限（Ed25519 自己署名の `TlsServerConfig` 1 つ）だけを重複して
    /// 持つ（`server_handshake::tests` の構成要素と同型）。
    mod tls_client_test_helpers {
        use crate::tls::ed25519::SigningKey;
        use crate::tls::server_handshake::TlsServerConfig;
        use crate::tls::x509::ServerCertificateChain;

        const SEED: [u8; 32] = [0x11; 32];

        fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
            let mut out = vec![tag];
            let len = body.len();
            if len < 0x80 {
                out.push(len as u8);
            } else {
                out.push(0x81);
                out.push(len as u8);
            }
            out.extend_from_slice(body);
            out
        }

        fn sequence(parts: &[&[u8]]) -> Vec<u8> {
            let mut body = Vec::new();
            for part in parts {
                body.extend_from_slice(part);
            }
            tlv(0x30, &body)
        }

        const OID_ED25519: [u8; 3] = [0x2b, 0x65, 0x70];
        /// sha384WithRSAEncryption（1.2.840.113549.1.1.12）。RFC 5929 は
        /// 定義しているが本実装は SHA-384 を自作していない OID
        /// （`channel_binding::RFC5929_DEFINED_BUT_UNIMPLEMENTED_HASH_OIDS`
        /// と同一。Issue #1089 レビュー是正の回帰テスト用）。
        const OID_SHA384_WITH_RSA: [u8; 9] = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];

        fn ed25519_algorithm_identifier() -> Vec<u8> {
            sequence(&[&tlv(0x06, &OID_ED25519)])
        }

        fn build_ed25519_leaf_certificate_der(public_key: &[u8; 32]) -> Vec<u8> {
            build_leaf_certificate_der_with_signature_oid(public_key, &OID_ED25519)
        }

        /// [`build_ed25519_leaf_certificate_der`] の一般化版。SPKI は常に
        /// Ed25519（鍵は Ed25519 のまま）だが、`tbsCertificate.signature`・
        /// 外側 `signatureAlgorithm` に任意の OID を差し込める（CA が
        /// RSA／ECDSA で署名した Ed25519 葉証明書を模擬する。Issue #1089
        /// レビュー是正: sha384WithRSA 署名の回帰テストに使う）。
        fn build_leaf_certificate_der_with_signature_oid(
            public_key: &[u8; 32],
            signature_oid: &[u8],
        ) -> Vec<u8> {
            let signature_algorithm = sequence(&[&tlv(0x06, signature_oid)]);
            let mut spki_bits = vec![0x00u8];
            spki_bits.extend_from_slice(public_key);
            let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
            let validity = sequence(&[&tlv(0x17, b"160801121924Z"), &tlv(0x17, b"401231235959Z")]);
            let version = tlv(0xa0, &tlv(0x02, &[0x02]));
            let common_name =
                sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), &tlv(0x0c, b"test-issuer")]);
            let issuer = sequence(&[&tlv(0x31, &common_name)]);
            let empty_name = sequence(&[]);
            let tbs_certificate = sequence(&[
                &version,
                &tlv(0x02, &[0x01]),
                &signature_algorithm,
                &issuer,
                &validity,
                &empty_name,
                &spki,
            ]);
            let mut signature_bits = vec![0x00u8];
            signature_bits.extend_from_slice(&[0u8; 64]);
            sequence(&[
                &tbs_certificate,
                &signature_algorithm,
                &tlv(0x03, &signature_bits),
            ])
        }

        /// Ed25519 署名の自己署名葉証明書 1 枚から構築した [`TlsServerConfig`]
        /// （`check_scram_channel_binding` の unit テストが「RFC 5929 の
        /// ハッシュが無い」構成として使う）。
        pub fn config_with_ed25519_leaf() -> TlsServerConfig {
            let key = SigningKey::from_seed_bytes(SEED);
            let public_key = key.public_key();
            let der = build_ed25519_leaf_certificate_der(&public_key);
            let chain =
                ServerCertificateChain::from_der_chain(vec![der], &public_key, 1_600_000_000)
                    .expect("valid synthetic chain");
            TlsServerConfig::new(chain, key).expect("matching leaf/key")
        }

        /// `tbsCertificate.signature`／外側 `signatureAlgorithm` を
        /// sha384WithRSA にした葉証明書（鍵自体は Ed25519）から構築した
        /// [`TlsServerConfig`]。RFC 5929 がハッシュを定義していながら本実装
        /// が未対応（`Rfc5929HashGap::UnsupportedHashAlgorithm`）な構成の
        /// 回帰テスト用（Issue #1089 レビュー是正）。
        pub fn config_with_sha384_rsa_signed_leaf() -> TlsServerConfig {
            let key = SigningKey::from_seed_bytes(SEED);
            let public_key = key.public_key();
            let der =
                build_leaf_certificate_der_with_signature_oid(&public_key, &OID_SHA384_WITH_RSA);
            let chain =
                ServerCertificateChain::from_der_chain(vec![der], &public_key, 1_600_000_000)
                    .expect("valid synthetic chain");
            TlsServerConfig::new(chain, key).expect("matching leaf/key")
        }
    }
}
