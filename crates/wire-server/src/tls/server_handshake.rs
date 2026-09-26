//! TLS 1.3 サーバー側ハンドシェイク状態機械（RFC 8446 §2・§4。TASK-228・
//! WIRE-9・HTTP-10 ポインタ。Issue #965・親 #941。分解 14/20）。
//!
//! [`super::record`]（レコード層）・[`super::handshake`]（メッセージ層）・
//! [`super::client_hello`]（`ClientHello` 受理判定）・[`super::x25519`]・
//! [`super::key_schedule`]（鍵スケジュール）・[`super::record_protection`]
//! （[`Sealer`](super::record_protection::Sealer)／[`Opener`]
//! (super::record_protection::Opener)）・[`super::transcript`]・
//! [`super::finished`]・[`super::certificate_verify`]・[`super::x509`]を
//! つなぎ、ClientHello 受信から client Finished 検証までを進める。
//! [`super::alert`] と組み合わせて fatal alert の送出・受信 alert の
//! 分類（close_notify／user_canceled／その他）を担う。
//!
//! **意図的なモジュール配置の逸脱**: 親 Issue の記述は「`tls/handshake.rs`
//! の状態機械」だが、`handshake.rs`（Issue #953）は「鍵・状態を持たない
//! 純粋な codec」であることをモジュール doc で確定済みのため、状態を
//! 持つ本体は独立モジュールとして新設する（`docs/design/
//! tls-server-handshake.md` に記録）。
//!
//! # 対象外
//!
//! - 3 クライアント接続テスト（#969）
//!
//! （CLI からの証明書・鍵読み込み〔#967〕・HTTPS 表層〔#968〕は本モジュール
//! 新設当時は対象外だったが、その後の Issue で実装済み）
//! - KeyUpdate・NewSessionTicket・0-RTT・クライアント証明書（親 #941 の方針）
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いず `get()`・
//! `checked_*`・`Result` で処理する（`.claude/rules/coding-rust.md` P0）。
//! `unsafe` は使わない。秘密値に依存する分岐は作らない（暗号処理は各
//! サブモジュールへ委譲し、本モジュール自身は公開値のみで分岐する）。

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::alert::{self, Alert, ReceivedAlert};
use super::certificate_verify;
use super::channel_binding::TlsServerEndPoint;
use super::client_hello::{self, ClientHelloDecision, ClientHelloError, NegotiatedClientHello};
use super::ed25519::SigningKey;
use super::finished;
use super::handshake::{self, HandshakeError, HandshakeType, RawHandshake};
use super::hkdf::{ct_eq, HkdfError};
use super::key_schedule::{EarlySecret, TrafficSecret};
use super::record::{self, AlertDescription, ContentType, Record, RecordKind};
use super::record_protection::{Opener, ProtectionError, Sealer};
use super::transcript::{Transcript, TranscriptError};
use super::x25519::{EphemeralSecret, X25519Error};
use super::x509::ServerCertificateChain;

/// ハンドシェイク中の読み取りタイムアウト（WIRE-5 の簡易クエリ応答と同値。
/// `crate::limits::READ_TIMEOUT` を単一情報源とする）。
pub const HANDSHAKE_READ_TIMEOUT: Duration = crate::limits::READ_TIMEOUT;

/// サーバー証明書チェーンと署名鍵の組。接続間で共有する（`Arc`）。
/// CLI からの構築は #967 の担当で、本モジュールは
/// [`TlsServerConfig::new`] による整合性検査までを提供する。
///
/// `channel_binding` は葉証明書から起動時に 1 回だけ算出する
/// `tls-server-end-point`（RFC 5929 §4。[`super::channel_binding`]・
/// Issue #970）。証明書は公開データのため接続ごとに再計算しない。算出に
/// 失敗した場合（非対応の署名アルゴリズム・構造違反）は起動失敗にはせず
/// `None`（チャネルバインディング非提供。TLS なしの接続と同じ SCRAM 挙動）
/// へ縮退させる（`docs/design/tls-channel-binding.md` 参照）。
pub struct TlsServerConfig {
    chain: ServerCertificateChain,
    key: SigningKey,
    channel_binding: Option<TlsServerEndPoint>,
    /// SCRAM-SHA-256-PLUS（`p=tls-server-end-point`）を機構リストへ
    /// 提示するか（[`Self::with_scram_channel_binding`]）。既定 `false`
    /// （非提示）。TLS ハンドシェイク自体は本フラグの値に関わらず同じ
    /// Ed25519 葉証明書で成立する（本フラグは TLS 確立後の SASL 機構
    /// リストのみを変える）。psql 18.6・OpenSSL 3.5.5 での実測（Issue #970
    /// §3 ブロック C。`tests/wire_scram_plus_psql_interop.rs`）では、本
    /// フラグを有効化して `PLUS` を提示した場合に、葉証明書の署名アルゴリズムに
    /// RFC 5929 が定義するハッシュが無い（Ed25519 など。
    /// [`super::channel_binding::has_rfc5929_defined_hash`]）と、libpq の
    /// 既定設定（`channel_binding=prefer`）・`channel_binding=require` の
    /// 両方が TLS 確立後の SCRAM 交換（tls-server-end-point の算出）で
    /// `could not find digest for NID UNDEF` により失敗する（Issue #970 の
    /// psql 18.6 実測）。この組合せは CLI 経由（`tls_opt::
    /// check_scram_channel_binding`）では起動時に fail-closed で拒否する
    /// （Issue #1088）。本フィールド・本関数はライブラリ API としては
    /// 従来どおり任意の bool を受け付ける（自前クライアントでの PLUS 検証
    /// テスト用。CLI 以外の呼び出し元に安全側の判断を強制しない）。
    /// 詳細・実測結果・opt-in 手順は `docs/design/tls-channel-binding.md`
    /// 参照。
    advertise_scram_channel_binding: bool,
}

/// [`TlsServerConfig::new`] の失敗理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsServerConfigError {
    /// 署名鍵の公開鍵と、証明書チェーンの葉が保持する公開鍵が一致しない。
    PublicKeyMismatch,
}

impl std::fmt::Display for TlsServerConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsServerConfigError::PublicKeyMismatch => {
                write!(
                    f,
                    "TLS server signing key does not match certificate leaf public key"
                )
            }
        }
    }
}

impl std::error::Error for TlsServerConfigError {}

impl TlsServerConfig {
    /// `key.public_key()` と `chain.leaf_public_key()` の一致を定数時間で
    /// 検査してから構築する（起動時エラー。#967 が呼ぶ）。あわせて葉証明書
    /// から `tls-server-end-point`（Issue #970）を 1 回だけ算出する。算出に
    /// 失敗しても起動失敗にはせず `channel_binding` を `None` にする
    /// （[`Self::channel_binding`] のドキュメンテーションコメント参照）。
    /// SCRAM-SHA-256-PLUS の提示は既定で無効（libpq 相互運用ゲートの実測結果。
    /// [`Self::with_scram_channel_binding`] のドキュメンテーションコメント
    /// 参照）。
    pub fn new(
        chain: ServerCertificateChain,
        key: SigningKey,
    ) -> Result<Self, TlsServerConfigError> {
        if ct_eq(&key.public_key(), chain.leaf_public_key()) {
            let channel_binding =
                super::channel_binding::tls_server_end_point(chain.leaf_der()).ok();
            Ok(TlsServerConfig {
                chain,
                key,
                channel_binding,
                advertise_scram_channel_binding: false,
            })
        } else {
            Err(TlsServerConfigError::PublicKeyMismatch)
        }
    }

    /// SCRAM-SHA-256-PLUS の機構リスト提示可否を明示的に設定する
    /// （libpq 相互運用ゲートの実測結果により既定 `false`。opt-in で `true`
    /// にできる。#967 が CLI から呼ぶ想定。詳細は
    /// `docs/design/tls-channel-binding.md` 参照）。CLI 経由の `enable` は
    /// `tls_opt::check_scram_channel_binding` が別途、葉証明書の署名
    /// アルゴリズムを見て拒否しうる（Issue #1088）。本メソッド自体は
    /// ライブラリ API として引き続き無条件に `enabled` を受け付ける。
    pub fn with_scram_channel_binding(mut self, enabled: bool) -> Self {
        self.advertise_scram_channel_binding = enabled;
        self
    }

    /// 算出済みの `tls-server-end-point`（[`super::channel_binding`]）。
    /// 非対応の署名アルゴリズム・DER 構造違反の場合は `None`
    /// （チャネルバインディング非提供）。#967 が起動時警告の判定に使う。
    pub fn tls_server_end_point(&self) -> Option<&TlsServerEndPoint> {
        self.channel_binding.as_ref()
    }

    /// SCRAM-SHA-256-PLUS を機構リストへ提示するか。`tls_server_end_point()`
    /// が `None` の場合は提示可否の設定に関わらず実際には提示できない
    /// （[`Self::tls_server_end_point`] と合わせて呼び出し元が判定する）。
    pub fn scram_channel_binding_enabled(&self) -> bool {
        self.advertise_scram_channel_binding
    }

    /// 葉証明書の署名アルゴリズムが RFC 5929 の定義するハッシュを持つか
    /// （[`super::channel_binding::has_rfc5929_defined_hash`]）を、
    /// `tls_server_end_point()` が算出済み（`Some`）であることとあわせて
    /// 判定する。`tls_opt::check_scram_channel_binding`（Issue #1088）が
    /// `--tls-scram-channel-binding enable` の起動時拒否判定に使う唯一の
    /// 入口で、これ以外の呼び出し元は想定しない。
    pub fn tls_server_end_point_is_rfc5929_defined(&self) -> bool {
        self.channel_binding.is_some()
            && super::channel_binding::has_rfc5929_defined_hash(self.chain.leaf_der())
    }

    /// [`Self::tls_server_end_point_is_rfc5929_defined`] が `false` を返す
    /// 理由を区別して返す（Issue #1089 レビュー是正）。`tls_opt::
    /// check_scram_channel_binding` が拒否メッセージを「証明書を別の CA で
    /// 再発行すべきケース」と「本実装側が未対応なハッシュのケース（例:
    /// SHA-384）」とで書き分けるために使う唯一の入口。`tls_server_end_point()`
    /// が `Some`（`channel_binding` 算出済み）かどうかに関わらず、常に
    /// 葉証明書 DER から独立に再判定する（[`super::channel_binding::
    /// rfc5929_hash_gap`] に委譲。`None` を返すのは
    /// `tls_server_end_point_is_rfc5929_defined()` が `true` の場合のみ）。
    pub fn tls_server_end_point_rfc5929_gap(
        &self,
    ) -> Option<super::channel_binding::Rfc5929HashGap> {
        super::channel_binding::rfc5929_hash_gap(self.chain.leaf_der())
    }
}

/// サーバー側ハンドシェイクが必要とする乱数源（server random・一時鍵）を
/// 抽象化する。本番実装は [`OsEntropy`]、テストは固定値を注入する。
pub trait HandshakeEntropy {
    fn server_random(&mut self) -> io::Result<[u8; 32]>;
    fn ephemeral(&mut self) -> io::Result<EphemeralSecret>;
}

/// OS の CSPRNG（[`crate::auth::read_urandom`]）を使う本番実装。
#[derive(Debug, Default)]
pub struct OsEntropy;

impl HandshakeEntropy for OsEntropy {
    fn server_random(&mut self) -> io::Result<[u8; 32]> {
        let raw = crate::auth::read_urandom(32)?;
        raw.try_into()
            .map_err(|_| io::Error::other("urandom read returned an unexpected length"))
    }

    fn ephemeral(&mut self) -> io::Result<EphemeralSecret> {
        EphemeralSecret::generate()
    }
}

/// [`ServerHandshake::handle_record`] が検出しうるエラー全体。全 variant が
/// `Copy` であり、poison 状態としてそのまま保持・再送出できる
/// （`handshake::HandshakeBuffer` と同じ fail-closed 流儀）。
///
/// `wire_code`（ERR-1/2/4）への写像は追加しない。TLS 層の失敗は
/// `ErrorResponse` ではなく alert と切断で表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerHandshakeError {
    Handshake(HandshakeError),
    ClientHello(ClientHelloError),
    Transcript(TranscriptError),
    X25519(X25519Error),
    Hkdf(HkdfError),
    Finished(finished::FinishedError),
    Protection(ProtectionError),
    /// 乱数取得（server random・一時鍵）の I/O 失敗。
    Entropy,
    /// 状態外のメッセージ（型は正しいが時期が違う）・整列違反。
    UnexpectedMessage,
    /// 受信 alert の parse 失敗（2 バイト長・level 値の違反）。
    AlertDecode(AlertDescription),
    /// HelloRetryRequest 送出済みの接続で再度 `RetryRequestX25519` が
    /// 返った（防御用の分岐。`negotiate`／`Transcript` の契約上到達しない）。
    DuplicateHelloRetryRequest,
    /// 相手から fatal alert を受け取った（応答は送らない）。
    ReceivedFatalAlert(u8),
    /// レコード層で検出された違反（[`record::RecordError`] 由来。
    /// 呼び出し元（driver）がこの alert 種別をそのまま渡す）。
    RecordLayer(AlertDescription),
    /// 一度でも `Err` を返した後の呼び出し（poison）。
    AlreadyFailed,
    /// `ClosedByPeer` 後の呼び出し。
    AlreadyClosed,
}

impl std::fmt::Display for ServerHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerHandshakeError::Handshake(e) => write!(f, "{e}"),
            ServerHandshakeError::ClientHello(e) => write!(f, "{e}"),
            ServerHandshakeError::Transcript(e) => write!(f, "{e}"),
            ServerHandshakeError::X25519(e) => write!(f, "{e}"),
            ServerHandshakeError::Hkdf(e) => write!(f, "{e}"),
            ServerHandshakeError::Finished(e) => write!(f, "{e}"),
            ServerHandshakeError::Protection(e) => write!(f, "{e}"),
            ServerHandshakeError::Entropy => write!(f, "entropy source failed"),
            ServerHandshakeError::UnexpectedMessage => {
                write!(f, "unexpected TLS handshake message or ordering violation")
            }
            ServerHandshakeError::AlertDecode(_) => write!(f, "received alert failed to parse"),
            ServerHandshakeError::DuplicateHelloRetryRequest => {
                write!(f, "HelloRetryRequest already sent for this connection")
            }
            ServerHandshakeError::ReceivedFatalAlert(d) => {
                write!(f, "peer sent a fatal TLS alert (description {d})")
            }
            ServerHandshakeError::RecordLayer(_) => write!(f, "TLS record layer violation"),
            ServerHandshakeError::AlreadyFailed => write!(f, "TLS handshake already failed"),
            ServerHandshakeError::AlreadyClosed => write!(f, "TLS handshake already closed"),
        }
    }
}

impl std::error::Error for ServerHandshakeError {}

/// クライアントへ送出すべき fatal alert の種別。`None` は alert を送らずに
/// 切断する種別（[`ProtectionError::SequenceExhausted`]・受信済み fatal
/// alert への応答・poison 後の再呼び出し）を表す。
fn fatal_alert_of(err: &ServerHandshakeError) -> Option<AlertDescription> {
    match err {
        ServerHandshakeError::Handshake(e) => e.alert_description(),
        ServerHandshakeError::ClientHello(e) => Some(e.alert_description()),
        ServerHandshakeError::Transcript(e) => Some(e.alert_description()),
        ServerHandshakeError::X25519(_) => Some(AlertDescription::HandshakeFailure),
        ServerHandshakeError::Hkdf(_) => Some(AlertDescription::InternalError),
        ServerHandshakeError::Finished(e) => Some(e.alert_description()),
        ServerHandshakeError::Protection(e) => e.alert_description(),
        ServerHandshakeError::Entropy => Some(AlertDescription::InternalError),
        ServerHandshakeError::UnexpectedMessage => Some(AlertDescription::UnexpectedMessage),
        ServerHandshakeError::AlertDecode(desc) => Some(*desc),
        ServerHandshakeError::DuplicateHelloRetryRequest => {
            Some(AlertDescription::HandshakeFailure)
        }
        ServerHandshakeError::RecordLayer(desc) => Some(*desc),
        ServerHandshakeError::ReceivedFatalAlert(_)
        | ServerHandshakeError::AlreadyFailed
        | ServerHandshakeError::AlreadyClosed => None,
    }
}

/// [`negotiate`](client_hello::negotiate) の判定結果を、HelloRetryRequest
/// を 1 回までに制限する状態機械の観点で分類する。`negotiate` 自身が
/// 「`after_hrr` が `Some` のときは `RetryRequestX25519` を返さない」契約を
/// 持つため、`hrr_already_sent` な状態でこの分岐に到達することは
/// `negotiate`／[`Transcript`] の契約上構造的に到達しないが、多層防御として
/// 直接テストできる純粋関数へ切り出す。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientHelloAction {
    SendHelloRetryRequest,
    Accept(NegotiatedClientHello),
    RejectDuplicateHelloRetryRequest,
}

fn decide_after_client_hello(
    decision: ClientHelloDecision,
    hrr_already_sent: bool,
) -> ClientHelloAction {
    match decision {
        ClientHelloDecision::Accept(negotiated) => ClientHelloAction::Accept(negotiated),
        ClientHelloDecision::RetryRequestX25519 => {
            if hrr_already_sent {
                ClientHelloAction::RejectDuplicateHelloRetryRequest
            } else {
                ClientHelloAction::SendHelloRetryRequest
            }
        }
    }
}

/// ハンドシェイクの現在位置。
#[derive(Debug, Clone, Copy)]
enum HandshakeState {
    /// `after_hrr`: HelloRetryRequest を送出済みか（1 回のみ許可）。
    ExpectClientHello {
        after_hrr: bool,
    },
    ExpectClientFinished,
    /// client Finished 検証まで完了した状態（`handle_client_finished` が
    /// この状態へ遷移させる）。[`Step::Complete`] を返した時点で呼び出し元が
    /// [`TlsSession`] を受け取り、以後このインスタンスへは触れない契約と
    /// する。この契約に反してこの状態のまま再度 [`ServerHandshake::
    /// handle_record`] を呼んだ場合は [`ServerHandshakeError::
    /// AlreadyFailed`]（他の poison 系 variant と名称を共有するが、実際には
    /// 「完了後の契約違反な再呼び出し」であり失敗ではない）を返す。
    Complete,
    /// fatal alert を送出済み（またはこれから送出を試みる）。以後の
    /// `handle_record` は同じ理由の `Err` を返し続ける。
    Failed(ServerHandshakeError),
    /// `close_notify` を受けて正常終了した。
    Closed,
    /// ハンドシェイク中に `user_canceled` を受けた（PR #1046 レビュー指摘）。
    /// RFC 8446 §6.1 によりこの alert の後には `close_notify` が続き、
    /// closure alert 受信後に届いたデータは無視しなければならないため、
    /// ハンドシェイクはもう進めず、`close_notify` が届くまで後続レコードを
    /// 読み捨てる（[`ServerHandshake::step_after_user_canceled`]）。
    /// 待機の有界性は driver の絶対期限（[`DeadlineReader`]）と
    /// レコード長上限（[`record::read_record`]）が保証する。
    Canceled {
        /// `user_canceled` 受信時点でダミー CCS の受理窓（ClientHello 受信後
        /// から client Finished 受信前まで）内にいたか。取り消し後もダミー
        /// CCS の時期・値（ちょうど `[0x01]`）の検証は通常時と同じに保つ
        /// （PR #1046 レビュー指摘）。
        ccs_window: bool,
    },
}

/// [`ServerHandshake::handle_record`] の戻り値。
#[derive(Debug)]
pub enum Step {
    /// ハンドシェイク継続中。`0` 件のこともある（次のレコードを待つ、
    /// ダミー CCS を読み捨てただけ、または `user_canceled` を受けて
    /// `close_notify` を待っている）。
    Continue(Vec<Record>),
    /// client Finished の検証まで完了した。
    Complete(Vec<Record>, Box<TlsSession>),
    /// 相手から `close_notify` を受けた（正常終了。`user_canceled` の後に
    /// 続く `close_notify` を含む）。
    ClosedByPeer(Vec<Record>),
}

/// TLS 1.3 サーバー側ハンドシェイク状態機械本体。ソケットを持たず、
/// レコード単位の push 型 API（[`ServerHandshake::handle_record`]）のみを
/// 提供する。I/O は [`perform_server_handshake`] が担う。
pub struct ServerHandshake<E: HandshakeEntropy = OsEntropy> {
    config: Arc<TlsServerConfig>,
    entropy: E,
    state: HandshakeState,
    buffer: handshake::HandshakeBuffer,
    transcript: Transcript,
    sealer: Sealer,
    opener: Opener,
    /// 1 回目の ClientHello（HRR 経由の 2 回目 `negotiate` 呼び出しへ渡す）。
    first_client_hello: Option<handshake::ClientHello>,
    /// client Finished 検証用（`ExpectClientFinished` に入った時点で必ず
    /// `Some`）。
    client_hs_traffic: Option<TrafficSecret>,
    /// client Finished 検証対象の transcript hash（ClientHello..server
    /// Finished）。
    th_ch_sf: Option<[u8; 32]>,
    /// application 鍵切替用（`ExpectClientFinished` に入った時点で必ず
    /// `Some`）。
    client_ap_secret: Option<TrafficSecret>,
    dummy_ccs_sent: bool,
    /// [`ServerHandshake::handle_record`] が `Err` を返した際に、直前に
    /// 送出（を試行）した fatal alert のレコード列を一時的に保持する。
    /// `Result` の `Err` 側へ出力バイト列を同時に載せる型を作らず、
    /// 呼び出し元は `Err` を受け取った直後に
    /// [`ServerHandshake::take_pending_alert_output`] で取り出す 2 段構え
    /// とする。
    pending_alert_output: Vec<Record>,
}

impl ServerHandshake<OsEntropy> {
    /// OS の CSPRNG を乱数源とするサーバー側状態機械を構築する。
    pub fn new(config: Arc<TlsServerConfig>) -> Self {
        Self::with_entropy(config, OsEntropy)
    }
}

impl<E: HandshakeEntropy> ServerHandshake<E> {
    /// 乱数源を明示的に注入して構築する（テスト用の入口。本番は
    /// [`ServerHandshake::new`] を使う）。
    pub fn with_entropy(config: Arc<TlsServerConfig>, entropy: E) -> Self {
        ServerHandshake {
            config,
            entropy,
            state: HandshakeState::ExpectClientHello { after_hrr: false },
            buffer: handshake::HandshakeBuffer::new(),
            transcript: Transcript::new(),
            sealer: Sealer::new(),
            opener: Opener::new(),
            first_client_hello: None,
            client_hs_traffic: None,
            th_ch_sf: None,
            client_ap_secret: None,
            dummy_ccs_sent: false,
            pending_alert_output: Vec::new(),
        }
    }

    /// 直前の `handle_record` の `Err` に伴って送出（を試行）した alert の
    /// レコード列を取り出す。呼び出し元（driver）は `Err` を受けた直後に
    /// 一度だけ呼ぶ契約（呼ばなくても次の `handle_record` 呼び出しで
    /// 上書きされるため、内部状態が壊れることはない）。
    pub fn take_pending_alert_output(&mut self) -> Vec<Record> {
        std::mem::take(&mut self.pending_alert_output)
    }

    /// 呼び出し元（driver）が次のレコードを読む際に使うべき [`RecordKind`]。
    pub fn record_kind(&self) -> RecordKind {
        self.opener.record_kind()
    }

    /// 呼び出し元（driver）が、この状態機械が送出したレコード列
    /// （[`Step::Continue`]／[`Step::Complete`] の出力・[`Self::
    /// take_pending_alert_output`]・[`Self::fail_on_record_error`] の
    /// 戻り値）を書き込む際に使うべき [`RecordKind`]。[`Self::record_kind`]
    /// （受信 [`Opener`] 側の epoch）とは独立した送信 [`Sealer`] 側の値で
    /// あり、書き込み検証に受信側の値を流用しない（#965 レビュー指摘。
    /// 本実装は `Sealer`／`Opener` が常に同時に鍵切替する構成のため現状は
    /// 両者が同値になるが、将来非対称な鍵切替が入っても壊れないよう
    /// 区別しておく）。
    fn write_record_kind(&self) -> RecordKind {
        self.sealer.record_kind()
    }

    /// レコード層で検出された違反（[`record::RecordError`]）を、この
    /// ハンドシェイクの現在の送信鍵で fatal alert として送出しようと試み、
    /// この接続を poison する。driver が `read_record` の `Err` を受けた
    /// 際に呼ぶ（[`ServerHandshake::handle_record`] にはレコード層の生の
    /// バイト列は渡らないため、この経路だけ別に公開する）。
    pub fn fail_on_record_error(&mut self, alert_desc: Option<AlertDescription>) -> Vec<Record> {
        let Some(desc) = alert_desc else {
            self.state = HandshakeState::Failed(ServerHandshakeError::RecordLayer(
                AlertDescription::InternalError,
            ));
            return Vec::new();
        };
        let output = self
            .sealer
            .seal_fragmented(ContentType::Alert, &Alert::encode_fatal(desc))
            .unwrap_or_default();
        self.state = HandshakeState::Failed(ServerHandshakeError::RecordLayer(desc));
        output
    }

    /// 1 レコードを処理する。
    pub fn handle_record(&mut self, record: &Record) -> Result<Step, ServerHandshakeError> {
        match self.state {
            HandshakeState::Failed(e) => return Err(e),
            HandshakeState::Closed => return Err(ServerHandshakeError::AlreadyClosed),
            HandshakeState::Complete => return Err(ServerHandshakeError::AlreadyFailed),
            _ => {}
        }
        match self.step_inner(record) {
            Ok(step) => Ok(step),
            Err(err) => {
                let output = match fatal_alert_of(&err) {
                    Some(desc) => self
                        .sealer
                        .seal_fragmented(ContentType::Alert, &Alert::encode_fatal(desc))
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                self.state = HandshakeState::Failed(err);
                // `Result::Err` には送出すべき alert のバイト列を同時に
                // 載せられないため、フィールドへ一時保存し、呼び出し元は
                // `Err` を受け取った直後に
                // [`ServerHandshake::take_pending_alert_output`] で取り出す
                // 契約とする。
                self.pending_alert_output = output;
                Err(err)
            }
        }
    }

    fn step_inner(&mut self, record: &Record) -> Result<Step, ServerHandshakeError> {
        if let HandshakeState::Canceled { ccs_window } = self.state {
            return self.step_after_user_canceled(record, ccs_window);
        }
        // ダミー CCS（middlebox 互換。RFC 8446 §5・付録 D.4）は Opener を
        // 経由せず、ClientHello 受信後から client Finished 受信前までに
        // 限り読み捨てる。RFC 8446 §5 は「最初の ClientHello を送信／受信
        // した後から相手の Finished を受信するまでの間に届いた値
        // `0x01` の平文 CCS は、件数の上限を設けず単純に読み捨てる」
        // ことを要求する（#965 レビュー指摘。HelloRetryRequest を伴う
        // middlebox 互換モードのクライアントは、1 回目の ClientHello 前後
        // と 2 回目の ClientHello 後の計 2 回 CCS を送り得るため、受理数を
        // 1 回に制限すると 2 回目が `unexpected_message` になり正当な
        // ハンドシェイクを中断させてしまう）。時期外・値違反（0x01 以外の
        // fragment）は引き続き `unexpected_message` として拒否する。
        if record.content_type == ContentType::ChangeCipherSpec {
            if !self.in_ccs_window() || record.fragment != [0x01] {
                return Err(ServerHandshakeError::UnexpectedMessage);
            }
            return Ok(Step::Continue(Vec::new()));
        }

        let inner = self
            .opener
            .open(record)
            .map_err(ServerHandshakeError::Protection)?;

        match inner.content_type {
            ContentType::Alert => self.handle_alert(&inner.content),
            ContentType::Handshake => self.handle_handshake_content(&inner.content),
            // `Opener::open` は epoch ごとに ApplicationData の内側 type を
            // 既に拒否済み（Plaintext は外側 ApplicationData を
            // UnexpectedOuterType、Handshake epoch は内側 ApplicationData
            // を ForbiddenInnerType として拒否する）。Application epoch は
            // ハンドシェイク完了後にのみ到達するため、この経路には
            // 到達しない（`Complete` 状態は `handle_record` 冒頭で
            // `Err(AlreadyFailed)` に短絡する）。
            ContentType::ApplicationData | ContentType::ChangeCipherSpec => {
                Err(ServerHandshakeError::UnexpectedMessage)
            }
        }
    }

    /// ダミー CCS の受理窓（RFC 8446 §5: 最初の ClientHello 受信後から
    /// client Finished 受信前まで）内にいるか。通常時と `user_canceled`
    /// 受信後（[`HandshakeState::Canceled`]）の双方の CCS 検証が共有する
    /// 単一の判定。
    fn in_ccs_window(&self) -> bool {
        matches!(
            self.state,
            HandshakeState::ExpectClientHello { after_hrr: true }
                | HandshakeState::ExpectClientFinished
        )
    }

    fn handle_alert(&mut self, content: &[u8]) -> Result<Step, ServerHandshakeError> {
        let alert = Alert::parse(content).map_err(ServerHandshakeError::AlertDecode)?;
        match alert::classify_received(alert) {
            ReceivedAlert::Closed => {
                self.state = HandshakeState::Closed;
                Ok(Step::ClosedByPeer(Vec::new()))
            }
            // `user_canceled` は終了通知ではなく、後続の `close_notify` を
            // 待つ取り消し通知（RFC 8446 §6.1。PR #1046 レビュー指摘: 従来は
            // この時点で `ClosedByPeer` を返し、後続の `close_notify` を
            // 読まずに切断していた）。ハンドシェイクは中断して `Canceled`
            // へ移り、`close_notify` を待つ。
            ReceivedAlert::UserCanceled => {
                self.state = HandshakeState::Canceled {
                    ccs_window: self.in_ccs_window(),
                };
                Ok(Step::Continue(Vec::new()))
            }
            ReceivedAlert::Fatal(code) => Err(ServerHandshakeError::ReceivedFatalAlert(code)),
        }
    }

    /// `user_canceled` 受信後（[`HandshakeState::Canceled`]）の 1 レコード
    /// 処理。RFC 8446 §6.1 は closure alert 受信後に届いたデータを無視する
    /// ことを要求するため、ハンドシェイクメッセージ・再度の
    /// `user_canceled` は解釈せずに読み捨て、`close_notify` で正常終了、
    /// それ以外の alert は従来どおり fatal として扱う。レコード保護
    /// （復号）・alert の構造検証・ダミー CCS の時期と値の検証
    /// （`ccs_window` 内かつちょうど `[0x01]` のみ読み捨て、違反は
    /// `unexpected_message`。PR #1046 レビュー指摘）は通常時と同じく
    /// fail-closed で行う。読み捨ては内容を保持しないためメモリは増えず、
    /// 待機時間は driver の絶対期限（[`DeadlineReader`]）で打ち切られる。
    fn step_after_user_canceled(
        &mut self,
        record: &Record,
        ccs_window: bool,
    ) -> Result<Step, ServerHandshakeError> {
        if record.content_type == ContentType::ChangeCipherSpec {
            if !ccs_window || record.fragment != [0x01] {
                return Err(ServerHandshakeError::UnexpectedMessage);
            }
            return Ok(Step::Continue(Vec::new()));
        }
        let inner = self
            .opener
            .open(record)
            .map_err(ServerHandshakeError::Protection)?;
        if inner.content_type != ContentType::Alert {
            return Ok(Step::Continue(Vec::new()));
        }
        let alert = Alert::parse(&inner.content).map_err(ServerHandshakeError::AlertDecode)?;
        match alert::classify_received(alert) {
            ReceivedAlert::Closed => {
                self.state = HandshakeState::Closed;
                Ok(Step::ClosedByPeer(Vec::new()))
            }
            ReceivedAlert::UserCanceled => Ok(Step::Continue(Vec::new())),
            ReceivedAlert::Fatal(code) => Err(ServerHandshakeError::ReceivedFatalAlert(code)),
        }
    }

    fn handle_handshake_content(&mut self, content: &[u8]) -> Result<Step, ServerHandshakeError> {
        self.buffer
            .feed(content)
            .map_err(ServerHandshakeError::Handshake)?;
        let Some(raw) = self
            .buffer
            .next_message()
            .map_err(ServerHandshakeError::Handshake)?
        else {
            // まだメッセージがそろっていない（レコード境界をまたぐ途中）。
            return Ok(Step::Continue(Vec::new()));
        };

        match self.state {
            HandshakeState::ExpectClientHello { after_hrr } => {
                self.handle_client_hello(raw, after_hrr)
            }
            HandshakeState::ExpectClientFinished => self.handle_client_finished(raw),
            HandshakeState::Complete
            | HandshakeState::Failed(_)
            | HandshakeState::Closed
            | HandshakeState::Canceled { .. } => Err(ServerHandshakeError::AlreadyFailed),
        }
    }

    /// 現在バッファに残っている・またはこれから取り出せるメッセージが
    /// 無いことを確認する（RFC 8446 §5.1 の鍵変更境界の整列検査）。
    fn check_buffer_alignment(&mut self) -> Result<(), ServerHandshakeError> {
        if self.buffer.has_partial() {
            return Err(ServerHandshakeError::UnexpectedMessage);
        }
        match self.buffer.next_message() {
            Ok(Some(_)) => Err(ServerHandshakeError::UnexpectedMessage),
            Ok(None) => Ok(()),
            Err(e) => Err(ServerHandshakeError::Handshake(e)),
        }
    }

    fn handle_client_hello(
        &mut self,
        raw: RawHandshake,
        after_hrr: bool,
    ) -> Result<Step, ServerHandshakeError> {
        if raw.msg_type != HandshakeType::ClientHello {
            return Err(ServerHandshakeError::UnexpectedMessage);
        }
        let ch =
            handshake::ClientHello::parse(&raw.body).map_err(ServerHandshakeError::Handshake)?;
        self.transcript
            .append_client_hello(&raw)
            .map_err(ServerHandshakeError::Transcript)?;

        let first_ch_ref = if after_hrr {
            self.first_client_hello.as_ref()
        } else {
            None
        };
        let decision = client_hello::negotiate(&ch, first_ch_ref)
            .map_err(ServerHandshakeError::ClientHello)?;

        self.check_buffer_alignment()?;

        match decide_after_client_hello(decision, after_hrr) {
            ClientHelloAction::RejectDuplicateHelloRetryRequest => {
                Err(ServerHandshakeError::DuplicateHelloRetryRequest)
            }
            ClientHelloAction::SendHelloRetryRequest => {
                let hrr = client_hello::build_hello_retry_request(&ch.legacy_session_id);
                let mut body = Vec::new();
                hrr.serialize_body_into(&mut body)
                    .map_err(ServerHandshakeError::Handshake)?;
                let raw_hrr = RawHandshake {
                    msg_type: HandshakeType::ServerHello,
                    body,
                };
                self.transcript
                    .append_hello_retry_request(&raw_hrr)
                    .map_err(ServerHandshakeError::Transcript)?;
                let wire = raw_hrr
                    .to_bytes()
                    .map_err(ServerHandshakeError::Handshake)?;
                let mut output = self
                    .sealer
                    .seal_fragmented(ContentType::Handshake, &wire)
                    .map_err(ServerHandshakeError::Protection)?;
                output.extend(self.maybe_dummy_ccs(&ch.legacy_session_id));
                self.first_client_hello = Some(ch);
                self.state = HandshakeState::ExpectClientHello { after_hrr: true };
                Ok(Step::Continue(output))
            }
            ClientHelloAction::Accept(negotiated) => self.accept_client_hello(&ch, negotiated),
        }
    }

    /// 互換モード（`legacy_session_id` が空でない。RFC 8446 付録 D.4）かつ
    /// 未送出であれば、ダミー `ChangeCipherSpec` レコードを 1 個返す。
    fn maybe_dummy_ccs(&mut self, legacy_session_id: &[u8]) -> Vec<Record> {
        if !legacy_session_id.is_empty() && !self.dummy_ccs_sent {
            self.dummy_ccs_sent = true;
            vec![Record {
                content_type: ContentType::ChangeCipherSpec,
                legacy_version: record::LEGACY_RECORD_VERSION,
                fragment: vec![0x01],
            }]
        } else {
            Vec::new()
        }
    }

    fn accept_client_hello(
        &mut self,
        ch: &handshake::ClientHello,
        negotiated: NegotiatedClientHello,
    ) -> Result<Step, ServerHandshakeError> {
        let mut output = Vec::new();

        let server_random = self
            .entropy
            .server_random()
            .map_err(|_| ServerHandshakeError::Entropy)?;
        let ephemeral = self
            .entropy
            .ephemeral()
            .map_err(|_| ServerHandshakeError::Entropy)?;
        let server_x25519_public = *ephemeral.public_key().as_bytes();

        let server_hello = build_server_hello(
            server_random,
            &negotiated.legacy_session_id,
            server_x25519_public,
        );
        let mut sh_body = Vec::new();
        server_hello
            .serialize_body_into(&mut sh_body)
            .map_err(ServerHandshakeError::Handshake)?;
        let raw_sh = RawHandshake {
            msg_type: HandshakeType::ServerHello,
            body: sh_body,
        };
        self.transcript
            .append_server_hello(&raw_sh)
            .map_err(ServerHandshakeError::Transcript)?;

        let shared = ephemeral
            .diffie_hellman(&negotiated.client_x25519_public)
            .map_err(ServerHandshakeError::X25519)?;
        let early = EarlySecret::new_without_psk();
        let handshake_secret = early
            .into_handshake(&shared)
            .map_err(ServerHandshakeError::Hkdf)?;
        let th_ch_sh = self
            .transcript
            .hash_through_server_hello()
            .map_err(ServerHandshakeError::Transcript)?;
        let traffic = handshake_secret
            .traffic_secrets(&th_ch_sh)
            .map_err(ServerHandshakeError::Hkdf)?;

        let sh_wire = raw_sh.to_bytes().map_err(ServerHandshakeError::Handshake)?;
        output.extend(
            self.sealer
                .seal_fragmented(ContentType::Handshake, &sh_wire)
                .map_err(ServerHandshakeError::Protection)?,
        );
        output.extend(self.maybe_dummy_ccs(&ch.legacy_session_id));

        let server_hs_keys = traffic
            .server
            .traffic_keys()
            .map_err(ServerHandshakeError::Hkdf)?;
        let client_hs_keys = traffic
            .client
            .traffic_keys()
            .map_err(ServerHandshakeError::Hkdf)?;
        self.sealer
            .install_handshake_keys(&server_hs_keys)
            .map_err(ServerHandshakeError::Protection)?;
        self.opener
            .install_handshake_keys(&client_hs_keys)
            .map_err(ServerHandshakeError::Protection)?;

        // EncryptedExtensions（拡張なし）。
        let ee = handshake::EncryptedExtensions {
            extensions: Vec::new(),
        };
        let mut ee_body = Vec::new();
        ee.serialize_body_into(&mut ee_body)
            .map_err(ServerHandshakeError::Handshake)?;
        let raw_ee = RawHandshake {
            msg_type: HandshakeType::EncryptedExtensions,
            body: ee_body,
        };
        self.transcript
            .append_encrypted_extensions(&raw_ee)
            .map_err(ServerHandshakeError::Transcript)?;

        // Certificate。
        let cert_msg = self.config.chain.certificate_message();
        let mut cert_body = Vec::new();
        cert_msg
            .serialize_body_into(&mut cert_body)
            .map_err(ServerHandshakeError::Handshake)?;
        let raw_cert = RawHandshake {
            msg_type: HandshakeType::Certificate,
            body: cert_body,
        };
        self.transcript
            .append_certificate(&raw_cert)
            .map_err(ServerHandshakeError::Transcript)?;
        let th_ch_cert = self
            .transcript
            .hash_through_certificate()
            .map_err(ServerHandshakeError::Transcript)?;

        // CertificateVerify。
        let cv = certificate_verify::build_server_certificate_verify(&self.config.key, &th_ch_cert);
        let mut cv_body = Vec::new();
        cv.serialize_body_into(&mut cv_body)
            .map_err(ServerHandshakeError::Handshake)?;
        let raw_cv = RawHandshake {
            msg_type: HandshakeType::CertificateVerify,
            body: cv_body,
        };
        self.transcript
            .append_certificate_verify(&raw_cv)
            .map_err(ServerHandshakeError::Transcript)?;
        let th_ch_cv = self
            .transcript
            .hash_through_certificate_verify()
            .map_err(ServerHandshakeError::Transcript)?;

        // server Finished。
        let server_finished = finished::build_server_finished(&traffic.server, &th_ch_cv)
            .map_err(ServerHandshakeError::Finished)?;
        let mut sf_body = Vec::new();
        server_finished
            .serialize_body_into(&mut sf_body)
            .map_err(ServerHandshakeError::Handshake)?;
        let raw_sf = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: sf_body,
        };
        self.transcript
            .append_server_finished(&raw_sf)
            .map_err(ServerHandshakeError::Transcript)?;
        let th_ch_sf = self
            .transcript
            .hash_through_server_finished()
            .map_err(ServerHandshakeError::Transcript)?;

        // server flight（EE・Certificate・CertificateVerify・Finished）を
        // 1 まとめに seal する。
        let mut flight = Vec::new();
        flight.extend(raw_ee.to_bytes().map_err(ServerHandshakeError::Handshake)?);
        flight.extend(
            raw_cert
                .to_bytes()
                .map_err(ServerHandshakeError::Handshake)?,
        );
        flight.extend(raw_cv.to_bytes().map_err(ServerHandshakeError::Handshake)?);
        flight.extend(raw_sf.to_bytes().map_err(ServerHandshakeError::Handshake)?);
        output.extend(
            self.sealer
                .seal_fragmented(ContentType::Handshake, &flight)
                .map_err(ServerHandshakeError::Protection)?,
        );

        // Master secret → application traffic secret（server Finished 送出
        // 直後。client Finished の受信は不要）。
        let master = handshake_secret
            .into_master()
            .map_err(ServerHandshakeError::Hkdf)?;
        let app = master
            .application_traffic_secrets(&th_ch_sf)
            .map_err(ServerHandshakeError::Hkdf)?;
        let server_ap_keys = app
            .server
            .traffic_keys()
            .map_err(ServerHandshakeError::Hkdf)?;
        self.sealer
            .install_application_keys(&server_ap_keys)
            .map_err(ServerHandshakeError::Protection)?;

        self.client_hs_traffic = Some(traffic.client);
        self.th_ch_sf = Some(th_ch_sf);
        self.client_ap_secret = Some(app.client);
        self.state = HandshakeState::ExpectClientFinished;

        Ok(Step::Continue(output))
    }

    fn handle_client_finished(&mut self, raw: RawHandshake) -> Result<Step, ServerHandshakeError> {
        if raw.msg_type != HandshakeType::Finished {
            return Err(ServerHandshakeError::UnexpectedMessage);
        }
        let received =
            handshake::Finished::parse(&raw.body).map_err(ServerHandshakeError::Handshake)?;

        let client_hs_traffic = self
            .client_hs_traffic
            .as_ref()
            .ok_or(ServerHandshakeError::UnexpectedMessage)?;
        let th_ch_sf = self
            .th_ch_sf
            .ok_or(ServerHandshakeError::UnexpectedMessage)?;
        finished::verify_client_finished(client_hs_traffic, &th_ch_sf, &received)
            .map_err(ServerHandshakeError::Finished)?;

        self.transcript
            .append_client_finished(&raw)
            .map_err(ServerHandshakeError::Transcript)?;
        self.check_buffer_alignment()?;

        let client_ap_secret = self
            .client_ap_secret
            .take()
            .ok_or(ServerHandshakeError::UnexpectedMessage)?;
        let client_ap_keys = client_ap_secret
            .traffic_keys()
            .map_err(ServerHandshakeError::Hkdf)?;
        self.opener
            .install_application_keys(&client_ap_keys)
            .map_err(ServerHandshakeError::Protection)?;

        let sealer = std::mem::take(&mut self.sealer);
        let opener = std::mem::take(&mut self.opener);
        self.state = HandshakeState::Complete;

        // SCRAM-SHA-256-PLUS へ提示する値は「算出済みの tls-server-end-point」
        // と「提示可否設定」の両方が揃った場合のみ `Some`（Issue #970）。
        // 提示無効化設定（libpq 相互運用ゲート opt-out）はここで畳み込み、
        // `TlsSession::tls_server_end_point` の呼び出し元（`handshake.rs`）が
        // 別途設定を意識しなくてよい形にする。
        let channel_binding = if self.config.scram_channel_binding_enabled() {
            self.config.tls_server_end_point().cloned()
        } else {
            None
        };

        Ok(Step::Complete(
            Vec::new(),
            Box::new(TlsSession {
                sealer,
                opener,
                poisoned: false,
                sent_close_notify: false,
                received_close_notify: false,
                received_user_canceled: false,
                fatal_alert_sent: false,
                channel_binding,
            }),
        ))
    }
}

fn build_server_hello(
    server_random: [u8; 32],
    legacy_session_id: &[u8],
    server_x25519_public: [u8; 32],
) -> handshake::ServerHello {
    let mut key_share_data = Vec::with_capacity(4 + 32);
    key_share_data.extend_from_slice(&client_hello::GROUP_X25519.to_be_bytes());
    key_share_data.extend_from_slice(&(32u16).to_be_bytes());
    key_share_data.extend_from_slice(&server_x25519_public);
    let key_share_ext = handshake::Extension {
        extension_type: 0x0033,
        extension_data: key_share_data,
    };
    let supported_versions_ext = handshake::Extension {
        extension_type: 0x002b,
        extension_data: client_hello::TLS13_VERSION.to_be_bytes().to_vec(),
    };
    handshake::ServerHello {
        legacy_version: 0x0303,
        random: server_random,
        legacy_session_id_echo: legacy_session_id.to_vec(),
        cipher_suite: client_hello::TLS_AES_128_GCM_SHA256,
        legacy_compression_method: 0,
        extensions: vec![key_share_ext, supported_versions_ext],
    }
}

/// ハンドシェイク完了後のアプリケーションデータ往復（#966 が接続する
/// 最小 API）。
#[derive(Debug)]
pub struct TlsSession {
    sealer: Sealer,
    opener: Opener,
    /// 接続が致命的な失敗状態に入ったら `true` にする（fail-closed）。
    /// 以後の `seal_application_data`／`open_record`／`close_notify` を
    /// **送受信の両方向とも**拒否する。true にする経路: (1) 送信側で fatal
    /// alert を送出した後（[`Self::mark_poisoned`]）、(2) [`Self::open_record`]
    /// が fatal alert を受信した後、または復号・alert 解析に失敗した後
    /// （#965 レビュー指摘: これらを poison にしないと破損した接続で
    /// アプリケーションデータの送受信を続けられてしまう）。
    poisoned: bool,
    /// 自ら `close_notify` を送出した（送信方向の終了。RFC 8446 §6.1）。
    /// 以後の送信操作（`seal_application_data`・`close_notify`）だけを
    /// 拒否し、受信側（相手のデータ・`close_notify`）は引き続き受け取れる
    /// （PR #1046 レビュー指摘: 単一の `poisoned` で表していたため
    /// 送信後に受信まで拒否していた）。
    sent_close_notify: bool,
    /// 相手の `close_notify` を受信した（受信方向の終了）。以後の
    /// `open_record` を拒否する。応答の `close_notify` 送出は許可し
    /// （RFC 8446 §6.1: 書き込み側を閉じる前に `close_notify` を送る）、
    /// アプリケーションデータの送信は fail-closed 側に倒して拒否する。
    received_close_notify: bool,
    /// 相手の `user_canceled` を受信した（PR #1046 レビュー指摘）。
    /// RFC 8446 §6.1 により closure alert 受信後に届いたアプリケーション
    /// データは無視しなければならないため、以後の `ApplicationData` は
    /// [`AppEvent::Ignored`] として内容を返さずに読み捨てる。後続の
    /// `close_notify` は通常どおり [`AppEvent::CloseNotify`] になる。
    received_user_canceled: bool,
    /// [`Self::seal_fatal_alert`] を呼び済み（Issue #966）。多重呼び出しでも
    /// 実際の alert 送出は高々 1 回に抑える（呼び出し元が誤って 2 回
    /// 呼んでも、2 回目以降は `Err(Poisoned)` になり追加の暗号操作を
    /// 行わない）。
    fatal_alert_sent: bool,
    /// この接続の `tls-server-end-point`（[`TlsServerConfig::
    /// tls_server_end_point`] のスナップショット。Issue #970）。SCRAM-
    /// SHA-256-PLUS がチャネルバインディングとして使う。`None` は
    /// 非対応の署名アルゴリズム・提示無効化設定を含め「チャネルバインディング
    /// 非提供」を表す（`crate::wire_stream::WireStream::
    /// tls_server_end_point` の平文接続と同じ意味）。
    channel_binding: Option<TlsServerEndPoint>,
}

/// [`TlsSession::open_record`] の戻り値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    ApplicationData(Vec<u8>),
    CloseNotify,
    /// 相手が `user_canceled` を送った（RFC 8446 §6.1。後続に
    /// `close_notify` が続く通知であり、この時点では受信方向を終了しない）。
    UserCanceled,
    /// `user_canceled` 受信後に届いたアプリケーションデータを読み捨てた
    /// （内容は返さない）。
    Ignored,
}

/// [`TlsSession`] の操作が起こしうる失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsSessionError {
    Protection(ProtectionError),
    AlertDecode(AlertDescription),
    ReceivedFatalAlert(u8),
    /// 致命的な失敗（poison）・方向別の終了（`close_notify` 送出後の
    /// 送信操作、`close_notify` 受信後の受信操作等）により、要求された
    /// 操作がこの接続ではもう許されない。
    Poisoned,
}

impl std::fmt::Display for TlsSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsSessionError::Protection(e) => write!(f, "{e}"),
            TlsSessionError::AlertDecode(_) => write!(f, "received alert failed to parse"),
            TlsSessionError::ReceivedFatalAlert(d) => {
                write!(f, "peer sent a fatal TLS alert (description {d})")
            }
            TlsSessionError::Poisoned => {
                write!(f, "TLS session already failed or closed in this direction")
            }
        }
    }
}

impl std::error::Error for TlsSessionError {}

#[cfg(test)]
impl TlsSession {
    /// テスト専用: 実ハンドシェイクを経由せず、鍵スケジュールから直接得た
    /// `Sealer`/`Opener` から組み立てる（[`super::stream`] の単体テストが
    /// レコード保護層以上の挙動だけを検証するために使う）。
    pub(crate) fn new_for_tests(sealer: Sealer, opener: Opener) -> Self {
        Self {
            sealer,
            opener,
            poisoned: false,
            sent_close_notify: false,
            received_close_notify: false,
            received_user_canceled: false,
            fatal_alert_sent: false,
            channel_binding: None,
        }
    }
}

impl TlsSession {
    /// この接続で SCRAM-SHA-256-PLUS へ提示すべき `tls-server-end-point`
    /// （Issue #970）。`TlsServerConfig::scram_channel_binding_enabled`
    /// が無効化されている場合は算出済みの値があっても `None`（提示無効化を
    /// 契約として畳み込み済み。[`ServerHandshake::handle_client_finished`]
    /// 参照）。[`super::stream::TlsStream::tls_server_end_point`]・
    /// `crate::wire_stream::WireStream::tls_server_end_point` から
    /// 委譲される。
    pub fn tls_server_end_point(&self) -> Option<&[u8]> {
        self.channel_binding
            .as_ref()
            .map(TlsServerEndPoint::as_bytes)
    }

    /// アプリケーションデータを 1 個以上のレコードへ seal する。
    pub fn seal_application_data(
        &mut self,
        payload: &[u8],
    ) -> Result<Vec<Record>, TlsSessionError> {
        if self.poisoned || self.sent_close_notify || self.received_close_notify {
            return Err(TlsSessionError::Poisoned);
        }
        self.sealer
            .seal_fragmented(ContentType::ApplicationData, payload)
            .map_err(TlsSessionError::Protection)
    }

    /// 受信した 1 レコードを open する。`close_notify` は
    /// [`AppEvent::CloseNotify`]、`user_canceled` は [`AppEvent::UserCanceled`]
    /// （受信は継続し、以後のアプリケーションデータは [`AppEvent::Ignored`]
    /// として読み捨てる）へ写像し、それ以外の alert は
    /// `Err(ReceivedFatalAlert)`。
    ///
    /// 復号失敗・alert 解析失敗・fatal alert の受信はこの接続を両方向とも
    /// 終端させ、以後の呼び出しはすべて `Err(Poisoned)` になる（#965
    /// レビュー指摘: 破損した接続でアプリケーションデータの送受信を
    /// 続けさせない）。`close_notify` の受信は受信方向だけを終端させる
    /// （以後の `open_record` は `Err(Poisoned)`）。自ら `close_notify` を
    /// 送出済みでも受信は続けられる（送信方向の終了は受信に影響しない。
    /// PR #1046 レビュー指摘）。
    pub fn open_record(&mut self, record: &Record) -> Result<AppEvent, TlsSessionError> {
        if self.poisoned || self.received_close_notify {
            return Err(TlsSessionError::Poisoned);
        }
        let inner = match self.opener.open(record) {
            Ok(inner) => inner,
            Err(e) => {
                self.poisoned = true;
                return Err(TlsSessionError::Protection(e));
            }
        };
        match inner.content_type {
            ContentType::ApplicationData if self.received_user_canceled => Ok(AppEvent::Ignored),
            ContentType::ApplicationData => Ok(AppEvent::ApplicationData(inner.content)),
            ContentType::Alert => {
                let alert = match Alert::parse(&inner.content) {
                    Ok(alert) => alert,
                    Err(e) => {
                        self.poisoned = true;
                        return Err(TlsSessionError::AlertDecode(e));
                    }
                };
                match alert::classify_received(alert) {
                    ReceivedAlert::Closed => {
                        self.received_close_notify = true;
                        Ok(AppEvent::CloseNotify)
                    }
                    ReceivedAlert::UserCanceled => {
                        self.received_user_canceled = true;
                        Ok(AppEvent::UserCanceled)
                    }
                    ReceivedAlert::Fatal(code) => {
                        self.poisoned = true;
                        Err(TlsSessionError::ReceivedFatalAlert(code))
                    }
                }
            }
            ContentType::Handshake | ContentType::ChangeCipherSpec => {
                self.poisoned = true;
                Err(TlsSessionError::Protection(
                    ProtectionError::UnexpectedOuterType,
                ))
            }
        }
    }

    /// `close_notify` レコードを組み立てる。RFC 8446 §6.1 により、
    /// 送信後はこの接続でこれ以上データを送ってはならないため、組み立てに
    /// 成功したら以後の送信操作（`seal_application_data`／`close_notify`）
    /// を拒否する（#965 レビュー指摘）。`close_notify` は送信方向の終了に
    /// すぎないため、`open_record` による相手のデータ・`close_notify` の
    /// 受信は引き続き許可する（PR #1046 レビュー指摘）。相手の
    /// `close_notify` 受信後の応答としての送出も許可する。
    pub fn close_notify(&mut self) -> Result<Vec<Record>, TlsSessionError> {
        if self.poisoned || self.sent_close_notify {
            return Err(TlsSessionError::Poisoned);
        }
        let records = self
            .sealer
            .seal_fragmented(ContentType::Alert, &Alert::close_notify())
            .map_err(TlsSessionError::Protection)?;
        self.sent_close_notify = true;
        Ok(records)
    }

    /// 送信側で fatal alert を送出した後に呼ぶ（以後は送受信の両方向とも
    /// poison）。
    pub fn mark_poisoned(&mut self) {
        self.poisoned = true;
    }

    /// ハンドシェイク完了後（接続の実運用中）に fatal alert を 1 個 seal する
    /// （Issue #966。[`TlsStream`](super::stream::TlsStream) が復号失敗・
    /// 不正な受信データを検出した際に呼ぶ）。RFC 8446 §5.2 は復号失敗を
    /// `bad_record_mac` で終了することを求めており、ハンドシェイク中の
    /// [`ServerHandshake::fail_on_record_error`] と対称な役割を持つ。
    ///
    /// 既に poison 済みでも fatal alert 自体は 1 回だけ送出を許す（呼び出し元
    /// が「失敗を検出した直後の 1 回」だけ呼ぶ契約のため、`open_record` が
    /// 既に poison 済みにしている経路でも送出できる）。送出に成功した場合は
    /// [`Self::mark_poisoned`] と同じ状態へ遷移し、以後の呼び出しはすべて
    /// `Err(Poisoned)` になる。鍵を使い切っている場合
    /// （[`super::record_protection::ProtectionError::SequenceExhausted`]）は
    /// 安全に送れないため呼び出し元が `alert_description() == None` で
    /// 判定し、本メソッド自体を呼ばない契約とする（[`TlsStream`]
    /// (super::stream::TlsStream) 参照）。
    pub fn seal_fatal_alert(
        &mut self,
        desc: AlertDescription,
    ) -> Result<Vec<Record>, TlsSessionError> {
        // RFC 8446 §6.1: `close_notify` を送出した後はこの接続でこれ以上
        // データを送ってはならない（`seal_application_data`／`close_notify`
        // と同じ送信方向の終了契約。#966 レビュー指摘: `shutdown_write` で
        // `close_notify` を送った直後に受信側の復号失敗が起きた場合でも
        // fatal alert を送ってしまう経路があった）。
        if self.fatal_alert_sent || self.sent_close_notify {
            return Err(TlsSessionError::Poisoned);
        }
        let records = self
            .sealer
            .seal_fragmented(ContentType::Alert, &Alert::encode_fatal(desc))
            .map_err(TlsSessionError::Protection)?;
        self.poisoned = true;
        self.fatal_alert_sent = true;
        Ok(records)
    }
}

/// blocking な `Read`／`Write` に加え、ハンドシェイク中の読み取り
/// タイムアウト設定・切断を提供するトランスポート抽象。`TcpStream` に
/// 実装する（#966 の実接続結線がこの trait を経由する）。
pub trait HandshakeTransport: Read + Write {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
    /// 送信側の絶対期限強制（[`DeadlineWriter`]）が使う書き込みタイムアウト
    /// 設定。読み取り側の [`set_read_timeout`](Self::set_read_timeout) と対。
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
    fn shutdown(&mut self) -> io::Result<()>;
}

impl HandshakeTransport for std::net::TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        std::net::TcpStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        std::net::TcpStream::set_write_timeout(self, timeout)
    }

    fn shutdown(&mut self) -> io::Result<()> {
        std::net::TcpStream::shutdown(self, std::net::Shutdown::Both)
    }
}

/// [`perform_server_handshake_with`] がハンドシェイク全体の**絶対期限**を
/// 強制するための `Read` ラッパー（#965 レビュー指摘）。
///
/// [`record::read_record`] は 1 レコードを読むだけでも `Read::read`／
/// `read_exact` を複数回（可変長フィールドを含むため）呼び出す。ループの
/// 外側で `set_read_timeout` を 1 回だけ設定する実装だと、この個々の
/// `read` 呼び出し単位のタイムアウトしか働かず、相手が期限の直前まで
/// 待ってから 1 バイトずつ送り続ければ、1 回の `read_record` 呼び出しが
/// 何倍にも間延びし得る。本ラッパーは `read` を呼ぶたびに `deadline`
/// までの残り時間を再計算して都度ソケットへ反映することで、
/// 個々の低レベル `read` 呼び出しの粒度で絶対期限を強制する。期限切れ
/// 後は OS を呼ばず即座に `TimedOut` を返す（`set_read_timeout` へ
/// ゼロ Duration を渡すとプラットフォームによってはエラーになるため）。
struct DeadlineReader<'a, S: HandshakeTransport> {
    stream: &'a mut S,
    deadline: Instant,
}

impl<S: HandshakeTransport> Read for DeadlineReader<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake exceeded overall time budget",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

/// [`perform_server_handshake_with`] がハンドシェイク全体の**絶対期限**を
/// 送信側にも強制する `Write` ラッパー（PR #1046 レビュー指摘）。
///
/// [`DeadlineReader`] は受信側の各 `read` 呼び出しで残り時間を都度
/// ソケットへ反映するが、送信側（`write_all_records` が `Step::Continue`
/// で ServerHello・証明書等を含む server flight を書き出す経路）には
/// 対応する期限強制がなく、相手が受信を止めれば `write_all` が無期限に
/// ブロックしハンドシェイク全体の絶対期限を超えて接続処理を占有し得た。
/// 本ラッパーは `write` を呼ぶたびに `deadline` までの残り時間を
/// 再計算して都度ソケットへ反映することで、`DeadlineReader` と対称な
/// 絶対期限強制を送信側にも与える。期限切れ後は OS を呼ばず即座に
/// `TimedOut` を返す（`DeadlineReader` と同じ理由でゼロ Duration は
/// 渡さない）。
struct DeadlineWriter<'a, S: HandshakeTransport> {
    stream: &'a mut S,
    deadline: Instant,
}

impl<S: HandshakeTransport> Write for DeadlineWriter<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake exceeded overall time budget",
            ));
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// [`perform_server_handshake`] の失敗理由。
#[derive(Debug)]
pub enum ServerHandshakeDriverError {
    Handshake(ServerHandshakeError),
    Record(record::RecordError),
    /// 接続が `close_notify`（`user_canceled` の後に続くものを含む）で
    /// 正常終了した。
    ClosedByPeer,
}

impl std::fmt::Display for ServerHandshakeDriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerHandshakeDriverError::Handshake(e) => write!(f, "{e}"),
            ServerHandshakeDriverError::Record(e) => write!(f, "{e}"),
            ServerHandshakeDriverError::ClosedByPeer => {
                write!(f, "TLS handshake closed by peer before completion")
            }
        }
    }
}

impl std::error::Error for ServerHandshakeDriverError {}

/// OS の CSPRNG・[`HANDSHAKE_READ_TIMEOUT`] を使ってサーバー側ハンドシェイクを
/// 進める（#966 が実接続へ結線する入口）。
pub fn perform_server_handshake<S: HandshakeTransport>(
    stream: &mut S,
    config: Arc<TlsServerConfig>,
) -> Result<Box<TlsSession>, ServerHandshakeDriverError> {
    perform_server_handshake_with(stream, config, OsEntropy, HANDSHAKE_READ_TIMEOUT)
}

/// テスト用: タイムアウトを明示的に注入する（乱数源は本番と同じ
/// [`OsEntropy`]）。結合テスト（`tests/tls_server_handshake.rs`）から
/// 短いタイムアウトを注入するために `pub` とする。
pub fn perform_server_handshake_with_timeout<S: HandshakeTransport>(
    stream: &mut S,
    config: Arc<TlsServerConfig>,
    timeout: Duration,
) -> Result<Box<TlsSession>, ServerHandshakeDriverError> {
    perform_server_handshake_with(stream, config, OsEntropy, timeout)
}

/// テスト用: 乱数源・タイムアウトを両方注入する完全形。
///
/// `timeout` は個々の読み取り呼び出しの上限ではなく、ハンドシェイク
/// 開始からの**絶対期限**として扱う（[`DeadlineReader`] 参照。#965
/// レビュー指摘: `set_read_timeout` をループの外側で 1 回だけ呼ぶ実装だと、
/// 相手が期限ぎりぎりの間隔で少量ずつバイトを送り続けた場合に
/// ハンドシェイク全体が `timeout` を大幅に超えて占有され得る）。
pub(crate) fn perform_server_handshake_with<S: HandshakeTransport, E: HandshakeEntropy>(
    stream: &mut S,
    config: Arc<TlsServerConfig>,
    entropy: E,
    timeout: Duration,
) -> Result<Box<TlsSession>, ServerHandshakeDriverError> {
    let deadline = Instant::now() + timeout;
    let mut core = ServerHandshake::with_entropy(config, entropy);
    loop {
        let record_result = {
            let mut deadline_reader = DeadlineReader {
                stream: &mut *stream,
                deadline,
            };
            record::read_record(&mut deadline_reader, core.record_kind())
        };
        match record_result {
            Ok(Some(record)) => match core.handle_record(&record) {
                Ok(Step::Continue(output)) => {
                    // 送出失敗（`DeadlineWriter` の絶対期限超過を含む）は
                    // 受信側の期限超過（下の `Err(e)` 腕）と同じく接続を
                    // shutdown してから `Record` エラーで返す。相手が受信を
                    // 止めている以上 alert は届かないため送らない。
                    if let Err(e) =
                        write_all_records(stream, &output, core.write_record_kind(), deadline)
                    {
                        let _ = stream.shutdown();
                        return Err(ServerHandshakeDriverError::Record(e));
                    }
                }
                Ok(Step::Complete(output, session)) => {
                    if let Err(e) =
                        write_all_records(stream, &output, core.write_record_kind(), deadline)
                    {
                        let _ = stream.shutdown();
                        return Err(ServerHandshakeDriverError::Record(e));
                    }
                    // DeadlineReader はハンドシェイク中の各読み取りで
                    // 「絶対期限までの残り時間」を set_read_timeout へ設定する
                    // （上記コメント参照）。ハンドシェイクが期限直前まで
                    // かかった接続では、この短いタイムアウトが読み取り
                    // タイムアウトとしてソケットへ残ったまま次のアプリ
                    // ケーションデータ読み取りへ入ってしまい、意図
                    // （`limits::READ_TIMEOUT`）より大幅に早くタイムアウト
                    // する（#965 レビュー指摘）。ハンドシェイク成功時は
                    // 呼び出し元へ返す直前に通常運用値へ明示的に戻す。
                    //
                    // 送信側も同様: `DeadlineWriter` が残り時間で設定した
                    // 書き込みタイムアウトを、接続受理時に
                    // `limits::apply_read_timeout` が読み書き双方へ設定する
                    // 通常運用値（`limits::READ_TIMEOUT`）へ戻す。戻さないと
                    // 以降のアプリケーションデータ送出がハンドシェイクの
                    // 残り時間で早期にタイムアウトし得る。
                    //
                    // いずれかの復元に失敗した場合は、他の driver 失敗経路と
                    // 同じく接続を shutdown してから `Err` を返す（PR #1046
                    // レビュー指摘: 従来は `?` で返し、確立済みの
                    // `TlsSession` を drop するだけで接続を開いたまま残して
                    // いた）。タイムアウトを保証できない接続を呼び出し元へ
                    // 渡さない fail-closed。
                    let restored = stream
                        .set_read_timeout(Some(crate::limits::READ_TIMEOUT))
                        .and_then(|()| stream.set_write_timeout(Some(crate::limits::READ_TIMEOUT)));
                    if let Err(e) = restored {
                        drop(session);
                        let _ = stream.shutdown();
                        return Err(ServerHandshakeDriverError::Record(record::RecordError::Io(
                            e,
                        )));
                    }
                    return Ok(session);
                }
                Ok(Step::ClosedByPeer(output)) => {
                    let _ = write_all_records(stream, &output, core.write_record_kind(), deadline);
                    let _ = stream.shutdown();
                    return Err(ServerHandshakeDriverError::ClosedByPeer);
                }
                Err(err) => {
                    let alert_output = core.take_pending_alert_output();
                    let _ = write_all_records(
                        stream,
                        &alert_output,
                        core.write_record_kind(),
                        deadline,
                    );
                    let _ = stream.shutdown();
                    return Err(ServerHandshakeDriverError::Handshake(err));
                }
            },
            Ok(None) => {
                let _ = stream.shutdown();
                return Err(ServerHandshakeDriverError::Record(
                    record::RecordError::Truncated,
                ));
            }
            Err(e) => {
                let alert_output = core.fail_on_record_error(e.alert_description());
                let _ =
                    write_all_records(stream, &alert_output, core.write_record_kind(), deadline);
                let _ = stream.shutdown();
                return Err(ServerHandshakeDriverError::Record(e));
            }
        }
    }
}

/// server flight（ServerHello・証明書・Finished 等）を `deadline`（ハンド
/// シェイク全体の絶対期限。[`perform_server_handshake_with`] 参照）の
/// 制約下で送出する。[`DeadlineWriter`] 経由で書き込むため、相手が受信を
/// 止めても `deadline` を超えて `write_all` がブロックし続けることはない。
fn write_all_records<S: HandshakeTransport>(
    stream: &mut S,
    records: &[Record],
    kind: RecordKind,
    deadline: Instant,
) -> Result<(), record::RecordError> {
    let mut buf = Vec::new();
    for record in records {
        record.serialize_into(&mut buf, kind)?;
    }
    if !buf.is_empty() {
        let mut deadline_writer = DeadlineWriter { stream, deadline };
        deadline_writer
            .write_all(&buf)
            .map_err(record::RecordError::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::client_hello::NegotiatedClientHello;

    fn dummy_negotiated() -> NegotiatedClientHello {
        NegotiatedClientHello {
            client_random: [0u8; 32],
            legacy_session_id: Vec::new(),
            client_x25519_public: [0u8; 32],
            server_name: None,
        }
    }

    /// HelloRetryRequest 未送出であれば `RetryRequestX25519` を素直に
    /// `SendHelloRetryRequest` へ写像する。
    #[test]
    fn decide_after_client_hello_sends_hrr_when_not_yet_sent() {
        let action = decide_after_client_hello(ClientHelloDecision::RetryRequestX25519, false);
        assert_eq!(action, ClientHelloAction::SendHelloRetryRequest);
    }

    /// 防御用の分岐: HRR 送出済みの接続で再度 `RetryRequestX25519` が
    /// 返った場合は `RejectDuplicateHelloRetryRequest`（handshake_failure）。
    /// `negotiate`／`Transcript` の契約上到達しないが、多層防御として
    /// 直接固定する。
    #[test]
    fn decide_after_client_hello_rejects_second_hrr() {
        let action = decide_after_client_hello(ClientHelloDecision::RetryRequestX25519, true);
        assert_eq!(action, ClientHelloAction::RejectDuplicateHelloRetryRequest);
    }

    /// `Accept` はそのまま素通しする。
    #[test]
    fn decide_after_client_hello_accepts_regardless_of_hrr_state() {
        let negotiated = dummy_negotiated();
        let action =
            decide_after_client_hello(ClientHelloDecision::Accept(negotiated.clone()), false);
        assert_eq!(action, ClientHelloAction::Accept(negotiated.clone()));
        let action =
            decide_after_client_hello(ClientHelloDecision::Accept(negotiated.clone()), true);
        assert_eq!(action, ClientHelloAction::Accept(negotiated));
    }

    /// `fatal_alert_of` の網羅的な写像固定（`RecordLayer`／`AlertDecode` は
    /// 渡された `AlertDescription` をそのまま返す。`ReceivedFatalAlert`・
    /// poison 系は応答しない）。
    #[test]
    fn fatal_alert_of_covers_all_variants() {
        assert_eq!(
            fatal_alert_of(&ServerHandshakeError::UnexpectedMessage),
            Some(AlertDescription::UnexpectedMessage)
        );
        assert_eq!(
            fatal_alert_of(&ServerHandshakeError::Entropy),
            Some(AlertDescription::InternalError)
        );
        assert_eq!(
            fatal_alert_of(&ServerHandshakeError::X25519(
                X25519Error::AllZeroSharedSecret
            )),
            Some(AlertDescription::HandshakeFailure)
        );
        assert_eq!(
            fatal_alert_of(&ServerHandshakeError::DuplicateHelloRetryRequest),
            Some(AlertDescription::HandshakeFailure)
        );
        assert_eq!(
            fatal_alert_of(&ServerHandshakeError::ReceivedFatalAlert(10)),
            None
        );
        assert_eq!(fatal_alert_of(&ServerHandshakeError::AlreadyFailed), None);
        assert_eq!(fatal_alert_of(&ServerHandshakeError::AlreadyClosed), None);
        assert_eq!(
            fatal_alert_of(&ServerHandshakeError::RecordLayer(
                AlertDescription::RecordOverflow
            )),
            Some(AlertDescription::RecordOverflow)
        );
    }

    /// PR #1046 レビュー指摘の回帰: 相手が受信を止めた場合でも
    /// `write_all_records`（server flight の送出）はハンドシェイク全体の
    /// 絶対期限に束縛される。修正前は送信側に一切タイムアウトを設定
    /// しておらず、相手が受信を止めると `write_all` が無期限にブロックし
    /// 得た。このモックは「OS が `set_write_timeout` を尊重し、相手が
    /// 読まないまま指定時間だけブロックしたのちタイムアウトを返す」という
    /// 実ソケットの典型的挙動を再現する。
    #[test]
    fn write_all_records_is_bounded_by_absolute_deadline_when_peer_stops_reading() {
        struct StalledPeer {
            write_timeout: Option<Duration>,
        }

        impl Read for StalledPeer {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }

        impl Write for StalledPeer {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                // 相手が受信を止めた状況では、OS は `set_write_timeout` で
                // 指定された時間だけブロックしたのちタイムアウトを返す。
                std::thread::sleep(self.write_timeout.unwrap_or(Duration::from_secs(5)));
                Err(io::Error::new(io::ErrorKind::TimedOut, "peer never reads"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl HandshakeTransport for StalledPeer {
            fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
                Ok(())
            }

            fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
                self.write_timeout = timeout;
                Ok(())
            }

            fn shutdown(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut peer = StalledPeer {
            write_timeout: None,
        };
        let record = Record {
            content_type: ContentType::Handshake,
            legacy_version: 0x0303,
            fragment: vec![0u8; 16],
        };
        let deadline = Instant::now() + Duration::from_millis(50);

        let started = Instant::now();
        let result = write_all_records(&mut peer, &[record], RecordKind::Plaintext, deadline);
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a peer that never reads must not appear to succeed"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "write must be bounded by the absolute deadline, got {elapsed:?}"
        );
    }
}
