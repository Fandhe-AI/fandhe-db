//! PEM（RFC 7468）ブロックのデコードと、鍵・証明書ファイルの上限付き読み込み
//! （TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #962・親 #941）。
//!
//! 自作 TLS 1.3 サーバー（[`super`] を参照）が運用者から受け取るサーバー
//! 秘密鍵・証明書チェーンのファイルは、いずれも PEM テキストとして渡される
//! ことを前提にする。本モジュールはその外側の皮（base64 でエンコードされた
//! バイト列を BEGIN/END ラベルで区切る構文）だけを解く。PKCS#8 の DER
//! パースは [`super::pkcs8`]、X.509 証明書の DER パース・Certificate
//! メッセージへの組み込みは #963 の担当であり、いずれも本モジュールへは
//! 依存しない（本モジュールが返すのはデコード済みの生 DER バイト列のみ）。
//!
//! ## lax／strict の線引き
//!
//! - 改行は LF・CRLF のいずれも受け付け、行の長さは強制しない
//!   （RFC 8410 §10.2 の証明書例が 66 文字行を使うため、64 文字固定にすると
//!   公開テストベクタ自体が通らなくなる）
//! - ブロックの外側に許すのは空白・改行のみで、`Bag Attributes` のような
//!   説明文は受け付けない（fail-closed を優先する意図的な選択）
//! - 本文中に `:` を含む行（RFC 1421 形式の暗号化ヘッダ）があれば拒否する
//! - ASCII 範囲外のバイトが 1 つでもあれば拒否する
//!
//! ## 定数時間 base64 デコード
//!
//! 秘密鍵（`PRIVATE KEY` ブロック）の base64 は、既存の `auth::base64_std`・
//! `http::query::base64_std`（いずれも文字ごとの `match` 分岐でデコードする）
//! とは意図的に別実装にし、sextet 値を算術マスクで求める（秘密鍵バイト由来の
//! base64 文字に依存する分岐・テーブル参照を作らない。[`.claude/rules/coding-rust.md`]
//! の「untrusted 入力の扱い」および本 Issue の共通条件）。証明書（公開データ）
//! にも同じデコーダを使い、実装を 1 つに保つ。

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 秘密鍵 PEM ファイルの読み込み上限（バイト）。Ed25519 PKCS#8 は 48 バイト
/// の DER に収まるため、改行・base64 膨張を踏まえても十分な余裕を持たせた
/// 本リポ独自の実装既定値（spec 由来の数値ではない）。
pub const MAX_PRIVATE_KEY_FILE_LEN: u64 = 16 * 1024;

/// 証明書チェーン PEM ファイルの読み込み上限（バイト）。本リポ独自の
/// 実装既定値。
pub const MAX_CERTIFICATE_FILE_LEN: u64 = 1024 * 1024;

/// 証明書チェーンに含めてよい `CERTIFICATE` ブロック数の上限。
/// 本リポ独自の実装既定値。
pub const MAX_CERTIFICATE_CHAIN_LEN: usize = 8;

/// [`scan_blocks`] が受理する PEM テキストの最大バイト数。
///
/// `decode_certificate_chain_pem`／`decode_private_key_pem`（延いては
/// `decode_ed25519_private_key_pem`）は `pub` API であり、
/// `load_certificate_chain_file`／`load_private_key_file` が課す
/// [`read_bounded_file`] 由来の上限（呼び出し元側の防御）を経由しない
/// 直接呼び出しに対しては、これまでファイルサイズ上限が一切存在しなかった
/// （PR #1030 codex-review P1 指摘）。本モジュールが正当に扱う最大の入力は
/// 証明書チェーン PEM（[`MAX_CERTIFICATE_FILE_LEN`]）であるため、
/// `scan_blocks` 自体の入口でこの値を上限として fail-closed に拒否する。
pub const MAX_PEM_SCAN_INPUT_LEN: usize = MAX_CERTIFICATE_FILE_LEN as usize;

/// [`scan_blocks`] が 1 回の走査で受理する BEGIN/END ブロック数の上限。
///
/// 証明書チェーンの意味的な上限（[`MAX_CERTIFICATE_CHAIN_LEN`]）は
/// 走査完了後に `decode_certificate_chain_pem` が判定するため、走査自体は
/// ここでブロック数を独立に打ち切らないと、短い空ブロックを大量に並べる
/// ことで「入力サイズ × ブロック数」規模のメモリ確保を誘発できてしまう
/// （PR #1030 codex-review P1 指摘）。秘密鍵は 1 ブロックしか許さないため、
/// 証明書チェーンの上限より十分大きい値を汎用の走査時上限として設定し、
/// 既存の意味的上限（証明書チェーン 8 本・秘密鍵 1 本）の挙動・エラー
/// メッセージは変えない。
const MAX_PEM_BLOCKS_PER_SCAN: usize = 64;

/// 秘密値（ファイルバッファ・base64 除去後テキスト・デコード後の DER）を
/// 保持するバッファ。Drop 時に [`super::hkdf::zeroize`] で best-effort に
/// ゼロ化する（`unsafe` を使わないため最適化による消去省略は排除できない。
/// [`super::hkdf::Secret32`] と同じ限界）。`Clone` は導出せず、`Debug` は
/// 内容を伏せる。
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct SecretBuf(Vec<u8>);

impl SecretBuf {
    fn from_vec(bytes: Vec<u8>) -> Self {
        SecretBuf(bytes)
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretBuf").field(&"<redacted>").finish()
    }
}

impl Drop for SecretBuf {
    fn drop(&mut self) {
        super::hkdf::zeroize(&mut self.0);
    }
}

/// PEM ブロックのラベル分類。`PRIVATE KEY`（PKCS#8）・`CERTIFICATE` 以外は
/// 種別名を持つ専用エラーで拒否し、起動時にどの鍵形式が非対応かを運用者へ
/// 明示できるようにする（受入基準 3・fail-closed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyKeyFormat {
    /// PKCS#1 (`RSA PRIVATE KEY`)。
    Pkcs1Rsa,
    /// SEC1 (`EC PRIVATE KEY`)。
    Sec1Ec,
    /// PKCS#8 暗号化鍵 (`ENCRYPTED PRIVATE KEY`)。
    EncryptedPkcs8,
    /// OpenSSH 独自形式 (`OPENSSH PRIVATE KEY`)。
    OpenSsh,
}

impl fmt::Display for LegacyKeyFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            LegacyKeyFormat::Pkcs1Rsa => "PKCS#1 (RSA PRIVATE KEY)",
            LegacyKeyFormat::Sec1Ec => "SEC1 (EC PRIVATE KEY)",
            LegacyKeyFormat::EncryptedPkcs8 => "encrypted PKCS#8 (ENCRYPTED PRIVATE KEY)",
            LegacyKeyFormat::OpenSsh => "OpenSSH (OPENSSH PRIVATE KEY)",
        };
        write!(f, "{name}")
    }
}

/// PEM デコードで検出した拒否理由。位置・実際の文字は含めない
/// （既存の `Base64StdError`・`Base64UrlError` と同じ情報最小の設計）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PemError {
    /// ブロックの外側（BEGIN より前・END より後・ブロック間）に、空白・
    /// 改行以外の文字がある。
    UnexpectedContent,
    /// ASCII 範囲外のバイトが含まれる。
    NonAscii,
    /// BEGIN はあるが対応する END が無い。
    MissingEnd,
    /// BEGIN の無い END、あるいは入れ子になった BEGIN。
    UnexpectedBoundary,
    /// BEGIN と END のラベルが一致しない。
    LabelMismatch,
    /// ブロックが 1 つも無い（空ファイル・境界行が無い）。
    NoBlocks,
    /// RFC 1421 形式の暗号化ヘッダ（`Proc-Type:` 等）を検出した。
    HeadersNotSupported,
    /// `PRIVATE KEY`／`CERTIFICATE` 以外の未知のラベル。
    UnexpectedLabel(String),
    /// 既知だが非対応の鍵形式（RSA・EC・暗号化・OpenSSH）。
    UnsupportedKeyFormat(LegacyKeyFormat),
    /// 秘密鍵ブロックが 0 個または複数個ある（ちょうど 1 個を要求する
    /// 呼び出し元向け）。
    ExpectedSingleBlock { found: usize },
    /// 証明書チェーンのブロック数が [`MAX_CERTIFICATE_CHAIN_LEN`] を超える。
    TooManyBlocks { max: usize },
    /// 走査中のブロック数が [`MAX_PEM_BLOCKS_PER_SCAN`]（用途を問わない
    /// 走査時の汎用上限）を超える。
    TooManyBlocksInScan { max: usize },
    /// 入力テキストが [`MAX_PEM_SCAN_INPUT_LEN`] を超える。
    InputTooLarge { max: usize },
    /// base64 本体のデコードに失敗した。
    Base64(Base64DecodeError),
}

impl fmt::Display for PemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PemError::UnexpectedContent => {
                write!(f, "PEM input contains content outside of a block")
            }
            PemError::NonAscii => write!(f, "PEM input contains non-ASCII bytes"),
            PemError::MissingEnd => write!(f, "PEM input has a BEGIN line with no matching END"),
            PemError::UnexpectedBoundary => {
                write!(f, "PEM input has an unexpected or nested boundary line")
            }
            PemError::LabelMismatch => write!(f, "PEM BEGIN and END labels do not match"),
            PemError::NoBlocks => write!(f, "PEM input contains no blocks"),
            PemError::HeadersNotSupported => {
                write!(f, "PEM input uses unsupported encrypted-PEM headers")
            }
            PemError::UnexpectedLabel(label) => {
                write!(f, "PEM input has an unexpected label: {label}")
            }
            PemError::UnsupportedKeyFormat(format) => {
                write!(
                    f,
                    "private key format {format} is not supported (PKCS#8 Ed25519 only)"
                )
            }
            PemError::ExpectedSingleBlock { found } => {
                write!(f, "expected exactly one PRIVATE KEY block, found {found}")
            }
            PemError::TooManyBlocks { max } => {
                write!(f, "PEM input has more than {max} CERTIFICATE blocks")
            }
            PemError::TooManyBlocksInScan { max } => {
                write!(f, "PEM input has more than {max} blocks")
            }
            PemError::InputTooLarge { max } => {
                write!(f, "PEM input exceeds the maximum size of {max} bytes")
            }
            PemError::Base64(e) => write!(f, "PEM body is not valid base64: {e}"),
        }
    }
}

impl std::error::Error for PemError {}

/// 上限付きファイル読み込み・鍵ファイル固有のエラー。[`read_bounded_file`]
/// が `open` した記述子から `fstat` で通常ファイルを確認し、`Read::take`
/// で上限+1 バイトまで読む（TOCTOU 回避のため `open` 前のパス単体への
/// `stat` は行わない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsFileError {
    NotFound,
    PermissionDenied,
    NotRegularFile,
    TooLarge { max: u64 },
    Io { kind: String },
}

impl fmt::Display for TlsFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsFileError::NotFound => write!(f, "file not found"),
            TlsFileError::PermissionDenied => write!(f, "permission denied"),
            TlsFileError::NotRegularFile => write!(f, "not a regular file"),
            TlsFileError::TooLarge { max } => {
                write!(f, "file exceeds the maximum allowed size ({max} bytes)")
            }
            TlsFileError::Io { kind } => write!(f, "I/O error: {kind}"),
        }
    }
}

impl std::error::Error for TlsFileError {}

/// [`TlsFileError`] に運用者指定のパスを添えた表示用ラッパー。パスは
/// 運用者が CLI で渡した値であり、テナントのデータではない。ファイルの
/// 内容や読み込めたバイト数はここに含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLoadError {
    pub path: PathBuf,
    pub error: TlsFileError,
}

impl fmt::Display for FileLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

impl std::error::Error for FileLoadError {}

/// 証明書チェーンファイルの読み込みエラー（ファイル入出力 or PEM 構文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateLoadError {
    File(FileLoadError),
    Pem(PemError),
}

impl fmt::Display for CertificateLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CertificateLoadError::File(e) => write!(f, "{e}"),
            CertificateLoadError::Pem(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CertificateLoadError {}

/// `open` → 同一ハンドルの `fstat`（`File::metadata`）→ `Read::take(max + 1)`
/// の順で読み込む上限付きファイル読み込み。パスに対する `stat` と `open` を
/// 分けると、その間にパスを差し替えられ（TOCTOU）通常ファイル検査
/// （`NotRegularFile` の fail-closed 契約）を迂回されうるため、`open` した
/// ファイル記述子そのものから `is_file()` を判定し、以降ファイルシステムへ
/// パス経由でアクセスしない（`std::fs::metadata(path)` は使わない。エラー
/// 表示用の `path.to_path_buf()` はパスの文字列コピーのみで I/O を伴わない）。
/// なお `open` 自体が FIFO に対してブロックしうる余地は本修正の範囲外
/// （`O_NONBLOCK` 指定には `libc` 定数の追加依存が要るため見送り。パスは
/// 運用者が起動時に CLI で渡す設定値であり、wire プロトコル越しの
/// untrusted な実行時入力ではない）。
pub(crate) fn read_bounded_file(path: &Path, max: u64) -> Result<SecretBuf, FileLoadError> {
    let file = File::open(path).map_err(|e| FileLoadError {
        path: path.to_path_buf(),
        error: map_io_error(&e),
    })?;
    let metadata = file.metadata().map_err(|e| FileLoadError {
        path: path.to_path_buf(),
        error: map_io_error(&e),
    })?;
    if !metadata.is_file() {
        return Err(FileLoadError {
            path: path.to_path_buf(),
            error: TlsFileError::NotRegularFile,
        });
    }
    // 確保量を上限 + 1 で頭打ちにしてから読む（untrusted なファイル長を
    // 無制限確保に使わない。`.claude/rules/coding-rust.md`）。
    let cap = (max.saturating_add(1)) as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(cap).map_err(|_| FileLoadError {
        path: path.to_path_buf(),
        error: TlsFileError::TooLarge { max },
    })?;
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| FileLoadError {
            path: path.to_path_buf(),
            error: map_io_error(&e),
        })?;
    if buf.len() as u64 > max {
        return Err(FileLoadError {
            path: path.to_path_buf(),
            error: TlsFileError::TooLarge { max },
        });
    }
    Ok(SecretBuf::from_vec(buf))
}

fn map_io_error(e: &std::io::Error) -> TlsFileError {
    match e.kind() {
        std::io::ErrorKind::NotFound => TlsFileError::NotFound,
        std::io::ErrorKind::PermissionDenied => TlsFileError::PermissionDenied,
        other => TlsFileError::Io {
            kind: other.to_string(),
        },
    }
}

/// 標準 base64 のデコードで検出した拒否理由（`http::query::base64_std` の
/// `Base64StdError` と同じ分類。実装は秘密値の分岐を避けるため独立させる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64DecodeError {
    TooLong,
    InvalidLength,
    InvalidCharacter,
    InvalidPadding,
    NonCanonical,
}

impl fmt::Display for Base64DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Base64DecodeError::TooLong => write!(f, "base64 input exceeds the length limit"),
            Base64DecodeError::InvalidLength => write!(f, "base64 input has an invalid length"),
            Base64DecodeError::InvalidCharacter => {
                write!(f, "base64 input contains a character outside the alphabet")
            }
            Base64DecodeError::InvalidPadding => write!(f, "base64 input has invalid padding"),
            Base64DecodeError::NonCanonical => write!(f, "base64 input is not in canonical form"),
        }
    }
}

impl std::error::Error for Base64DecodeError {}

/// 定数時間の sextet 変換。公知の branchless 手法（区間 `[lo, hi]` の
/// 包含判定を符号ビットのマスク `((lo - c) & (c - hi)) >> 8` で表し、5 つの
/// アルファベット区間（`A-Z`・`a-z`・`0-9`・`+`・`/`）分を OR で積み上げる）
/// で、入力バイト（秘密鍵の base64 文字でありうる）に依存する分岐・
/// テーブル参照を作らない。該当区間が無ければ -1 を返す。
fn ct_base64_sextet(c: u8) -> i16 {
    let c = c as i16;
    // `value` accumulates the sextet contribution from whichever range
    // matches (at most one, since the ranges are disjoint); `matched`
    // accumulates the range masks themselves so we can tell "no range
    // matched" (`matched == 0`) apart from "the matching range's value
    // happens to be 0" (`A` decodes to sextet 0). ORing directly into an
    // accumulator that starts at all-ones (-1) would be a no-op for
    // non-matching ranges and could never produce a "no match" state, so
    // both accumulators start at 0.
    let mut value: i16 = 0;
    let mut matched: i16 = 0;

    // A-Z (65..=90) -> 0..=25
    let mask = ((64 - c) & (c - 91)) >> 8;
    value |= mask & (c - 65);
    matched |= mask;

    // a-z (97..=122) -> 26..=51
    let mask = ((96 - c) & (c - 123)) >> 8;
    value |= mask & (c - 97 + 26);
    matched |= mask;

    // 0-9 (48..=57) -> 52..=61
    let mask = ((47 - c) & (c - 58)) >> 8;
    value |= mask & (c - 48 + 52);
    matched |= mask;

    // + (43) -> 62
    let mask = ((42 - c) & (c - 44)) >> 8;
    value |= mask & 62;
    matched |= mask;

    // / (47) -> 63
    let mask = ((46 - c) & (c - 48)) >> 8;
    value |= mask & 63;
    matched |= mask;

    // `matched` is either 0 (no range matched) or -1 (all bits set, since
    // exactly one disjoint range's mask contributed it). This branch is on
    // a public fact (whether the byte is in the base64 alphabet at all),
    // not on the byte's actual sextet value.
    if matched == 0 {
        -1
    } else {
        value
    }
}

/// 定数時間の標準 base64（`=` パディング必須）デコード。
///
/// 検査順は `http::query::base64_std::decode_base64_std` と同じ（長さ上限 →
/// 4 文字単位 → アルファベット外の文字 → パディング → 非正準表現）だが、
/// アルファベット判定・非正準判定は途中で打ち切らずフラグを OR で積み上げ、
/// 最後に 1 回だけ判定する（秘密鍵の base64 文字に依存する早期 return を
/// 作らない）。
fn decode_base64_const_time(input: &[u8], max_decoded: u64) -> Result<Vec<u8>, Base64DecodeError> {
    let max_input_len = (max_decoded.saturating_add(2) / 3).saturating_mul(4);
    if input.len() as u64 > max_input_len {
        return Err(Base64DecodeError::TooLong);
    }
    if !input.len().is_multiple_of(4) {
        return Err(Base64DecodeError::InvalidLength);
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let pad_count = input.iter().rev().take_while(|&&b| b == b'=').count();
    if pad_count > 2 {
        return Err(Base64DecodeError::InvalidPadding);
    }
    let body_len = input.len().saturating_sub(pad_count);
    let body = input
        .get(..body_len)
        .ok_or(Base64DecodeError::InvalidPadding)?;
    if body.contains(&b'=') {
        return Err(Base64DecodeError::InvalidPadding);
    }

    let decoded_len = (input.len() / 4)
        .saturating_mul(3)
        .saturating_sub(pad_count);
    if decoded_len as u64 > max_decoded {
        return Err(Base64DecodeError::TooLong);
    }

    let mut out = Vec::new();
    out.try_reserve_exact(decoded_len)
        .map_err(|_| Base64DecodeError::TooLong)?;

    let group_count = input.len() / 4;
    let mut invalid_char = false;
    let mut non_canonical = false;
    for (group_idx, chunk) in input.chunks(4).enumerate() {
        let is_last_group = group_idx + 1 == group_count;
        let group_pad = if is_last_group { pad_count } else { 0 };
        let (a, b, c, d) = match chunk {
            [a, b, c, d] => (*a, *b, *c, *d),
            // `input.len() % 4 == 0` を上で確認済みのため到達しない
            // （受信データ経路につき防御的に扱う）。
            _ => return Err(Base64DecodeError::InvalidLength),
        };
        let v0 = ct_base64_sextet(a);
        let v1 = ct_base64_sextet(b);
        let v2 = ct_base64_sextet(c);
        let v3 = ct_base64_sextet(d);
        match group_pad {
            0 => {
                invalid_char |= v0 < 0 || v1 < 0 || v2 < 0 || v3 < 0;
                let n =
                    ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | (v3 as u32);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            1 => {
                invalid_char |= v0 < 0 || v1 < 0 || v2 < 0;
                non_canonical |= v2 & 0x03 != 0;
                let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            2 => {
                invalid_char |= v0 < 0 || v1 < 0;
                non_canonical |= v1 & 0x0F != 0;
                let n = ((v0 as u32) << 18) | ((v1 as u32) << 12);
                out.push((n >> 16) as u8);
            }
            _ => return Err(Base64DecodeError::InvalidPadding),
        }
    }

    if invalid_char {
        return Err(Base64DecodeError::InvalidCharacter);
    }
    if non_canonical {
        return Err(Base64DecodeError::NonCanonical);
    }
    Ok(out)
}

/// PEM ブロックのラベル分類結果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockLabel {
    PrivateKey,
    Certificate,
    Legacy(LegacyKeyFormat),
    Other(String),
}

fn classify_label(label: &str) -> BlockLabel {
    match label {
        "PRIVATE KEY" => BlockLabel::PrivateKey,
        "CERTIFICATE" => BlockLabel::Certificate,
        "RSA PRIVATE KEY" => BlockLabel::Legacy(LegacyKeyFormat::Pkcs1Rsa),
        "EC PRIVATE KEY" => BlockLabel::Legacy(LegacyKeyFormat::Sec1Ec),
        "ENCRYPTED PRIVATE KEY" => BlockLabel::Legacy(LegacyKeyFormat::EncryptedPkcs8),
        "OPENSSH PRIVATE KEY" => BlockLabel::Legacy(LegacyKeyFormat::OpenSsh),
        other => BlockLabel::Other(other.to_string()),
    }
}

/// `Vec<u8>` を Drop 時に [`super::hkdf::zeroize`] で best-effort ゼロ化する
/// ラッパー。秘密鍵ブロックの base64 本体（[`RawBlock::body`]・
/// [`scan_blocks`] の走査中バッファ・デコード後の中間 DER）は、通常の
/// `Vec<u8>` のまま確保すると `SecretBuf` のゼロ化を経由しないコピーが
/// ヒープに残りうるため、証明書（公開データ）分も含めて実装を 1 つに
/// 保ったままこの型で統一する。
#[derive(Default)]
struct ZeroizingBytes(Vec<u8>);

impl ZeroizingBytes {
    /// 内部の `Vec<u8>` を取り出す（`self` は空のまま Drop されるため
    /// 二重ゼロ化にはならない）。デコード済みの秘密鍵 DER を
    /// [`SecretBuf`] へ引き継ぐ経路でのみ使う。
    fn into_inner(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl std::ops::Deref for ZeroizingBytes {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl std::ops::DerefMut for ZeroizingBytes {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        super::hkdf::zeroize(&mut self.0);
    }
}

/// 1 つの PEM ブロック（BEGIN〜END）から抽出した情報。
struct RawBlock {
    label: BlockLabel,
    /// base64 本体（改行・空白を除去済み。まだデコードしていない）。
    body: ZeroizingBytes,
}

/// PEM テキスト全体を構文レベルで走査し、ブロック列を返す
/// （ラベルの意味解釈・base64 デコードは呼び出し元が行う）。
///
/// - ASCII 範囲外のバイトは全体を通して拒否する
/// - ブロックの外側に空白・改行以外の文字があれば拒否する
/// - BEGIN と END のラベルが一致しなければ拒否する
/// - 本文中に `:` を含む行（RFC 1421 暗号化ヘッダ）があれば拒否する
/// - BEGIN の入れ子・END 単独の出現は拒否する
fn scan_blocks(text: &[u8]) -> Result<Vec<RawBlock>, PemError> {
    // `text` は `pub` API（`decode_certificate_chain_pem`・
    // `decode_private_key_pem`）へ直接渡されうるため、呼び出し元
    // （`read_bounded_file` 経由の `load_*_file`）を経由しない大入力にも
    // ここで fail-closed に上限を課す（PR #1030 codex-review P1 指摘）。
    if text.len() > MAX_PEM_SCAN_INPUT_LEN {
        return Err(PemError::InputTooLarge {
            max: MAX_PEM_SCAN_INPUT_LEN,
        });
    }
    if !text.is_ascii() {
        return Err(PemError::NonAscii);
    }
    // ASCII のみと確認済みのため `str` として扱える。
    let text = std::str::from_utf8(text).map_err(|_| PemError::NonAscii)?;

    let mut blocks = Vec::new();
    let mut current_label: Option<String> = None;
    let mut current_body = ZeroizingBytes::default();
    // 走査済みブロック数（用途を問わない汎用上限。下記 try_reserve_exact
    // の合計確保量を打ち切るための独立した歯止め）。
    let mut block_count: usize = 0;
    // `text` 中で走査済みの先頭からのバイト数（`split('\n')` は末尾の
    // 改行を含まないため、行ごとに `+1` して近似する。ブロック本体の
    // 確保量を「残り入力長」に抑えるための目安であり、多少の過大評価
    // （最終行の `+1` 分）があっても安全側にしか振れない）。
    let mut consumed: usize = 0;
    let total_len = text.len();

    for raw_line in text.split('\n') {
        let advance = raw_line.len() + 1;
        // CRLF の CR を取り除く（行末以外に CR が来る形は下の判定で
        // 空白以外の文字として自然に拒否される）。
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let trimmed = line.trim();

        if let Some(label) = trimmed
            .strip_prefix("-----BEGIN ")
            .and_then(|rest| rest.strip_suffix("-----"))
        {
            if current_label.is_some() {
                return Err(PemError::UnexpectedBoundary);
            }
            // ブロック本体の確保（下記）を行う前に上限を検査する
            // （検査を走査完了後まで遅延させると、短い空ブロックを大量に
            // 並べるだけで「入力サイズ × ブロック数」規模のメモリ確保を
            // 誘発できてしまう。PR #1030 codex-review P1 指摘）。
            block_count += 1;
            if block_count > MAX_PEM_BLOCKS_PER_SCAN {
                return Err(PemError::TooManyBlocksInScan {
                    max: MAX_PEM_BLOCKS_PER_SCAN,
                });
            }
            current_label = Some(label.to_string());
            current_body.clear();
            // ブロック本体は必ず「残りの入力」の部分集合であるため、
            // その長さ（`text.len()` 全体ではなく現在位置からの残り）を
            // 上限に一度だけ確保しておく（`extend_from_slice` の段階的な
            // 再確保を避け、秘密鍵 base64 のゼロ化されない旧確保領域が
            // 複数世代ヒープに残ることを防ぐ。失敗時は best-effort のため
            // 無視し、通常の再確保に委ねる）。
            let remaining = total_len.saturating_sub(consumed);
            let _ = current_body.try_reserve_exact(remaining);
            consumed += advance;
            continue;
        }
        if let Some(label) = trimmed
            .strip_prefix("-----END ")
            .and_then(|rest| rest.strip_suffix("-----"))
        {
            let Some(begin_label) = current_label.take() else {
                return Err(PemError::UnexpectedBoundary);
            };
            if begin_label != label {
                return Err(PemError::LabelMismatch);
            }
            blocks.push(RawBlock {
                label: classify_label(&begin_label),
                body: std::mem::take(&mut current_body),
            });
            consumed += advance;
            continue;
        }

        if current_label.is_some() {
            // ブロック本体行。RFC 1421 の暗号化ヘッダ（`Proc-Type:` 等）は
            // 本文に紛れ込ませない形式のため、`:` を含む行は拒否する。
            if trimmed.contains(':') {
                return Err(PemError::HeadersNotSupported);
            }
            current_body.extend_from_slice(trimmed.as_bytes());
        } else if !trimmed.is_empty() {
            // ブロックの外側は空白・改行のみ許容する。
            return Err(PemError::UnexpectedContent);
        }
        consumed += advance;
    }

    if current_label.is_some() {
        return Err(PemError::MissingEnd);
    }
    if blocks.is_empty() {
        return Err(PemError::NoBlocks);
    }
    Ok(blocks)
}

/// `CERTIFICATE` ブロック（1 個以上・[`MAX_CERTIFICATE_CHAIN_LEN`] 以下）を
/// ファイル内の出現順（先頭が葉証明書）で DER として返す。
pub fn decode_certificate_chain_pem(text: &[u8]) -> Result<Vec<Vec<u8>>, PemError> {
    let blocks = scan_blocks(text)?;
    let mut chain = Vec::new();
    for block in blocks {
        match block.label {
            BlockLabel::Certificate => {
                let der = decode_base64_const_time(&block.body, MAX_CERTIFICATE_FILE_LEN)
                    .map_err(PemError::Base64)?;
                chain.push(der);
            }
            BlockLabel::PrivateKey => {
                return Err(PemError::UnexpectedLabel("PRIVATE KEY".to_string()));
            }
            BlockLabel::Legacy(format) => return Err(PemError::UnsupportedKeyFormat(format)),
            BlockLabel::Other(label) => return Err(PemError::UnexpectedLabel(label)),
        }
        if chain.len() > MAX_CERTIFICATE_CHAIN_LEN {
            return Err(PemError::TooManyBlocks {
                max: MAX_CERTIFICATE_CHAIN_LEN,
            });
        }
    }
    Ok(chain)
}

/// 上限付きでファイルを読み込み、証明書チェーン PEM としてデコードする。
/// CLI からの結線（`--tls-cert-file` 等）は #967 の担当。
pub fn load_certificate_chain_file(path: &Path) -> Result<Vec<Vec<u8>>, CertificateLoadError> {
    let buf =
        read_bounded_file(path, MAX_CERTIFICATE_FILE_LEN).map_err(CertificateLoadError::File)?;
    decode_certificate_chain_pem(buf.as_slice()).map_err(CertificateLoadError::Pem)
}

/// `PRIVATE KEY`（PKCS#8）ブロックがちょうど 1 つであることを要求して
/// デコードする。呼び出し元（[`super::pkcs8`]）は返った DER バイト列を
/// さらに PKCS#8 として構文解析する。
pub(crate) fn decode_private_key_pem(text: &[u8]) -> Result<SecretBuf, PemError> {
    let blocks = scan_blocks(text)?;
    let mut private_key_der: Option<ZeroizingBytes> = None;
    let mut private_key_count = 0usize;
    for block in blocks {
        match block.label {
            BlockLabel::PrivateKey => {
                private_key_count += 1;
                // 複数 `PRIVATE KEY` ブロックが見つかった場合（後段で
                // `ExpectedSingleBlock` として拒否する）でも、2 個目以降の
                // デコード結果を破棄する前に `ZeroizingBytes` でラップし、
                // 破棄時（スコープ末尾の Drop）にゼロ化する。
                let der = ZeroizingBytes(
                    decode_base64_const_time(&block.body, MAX_PRIVATE_KEY_FILE_LEN)
                        .map_err(PemError::Base64)?,
                );
                if private_key_der.is_none() {
                    private_key_der = Some(der);
                }
            }
            BlockLabel::Certificate => {
                return Err(PemError::UnexpectedLabel("CERTIFICATE".to_string()));
            }
            BlockLabel::Legacy(format) => return Err(PemError::UnsupportedKeyFormat(format)),
            BlockLabel::Other(label) => return Err(PemError::UnexpectedLabel(label)),
        }
    }
    if private_key_count != 1 {
        return Err(PemError::ExpectedSingleBlock {
            found: private_key_count,
        });
    }
    // `private_key_count == 1` を確認済みのため必ず `Some`。
    let der = private_key_der.ok_or(PemError::ExpectedSingleBlock { found: 0 })?;
    Ok(SecretBuf::from_vec(der.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8410 §10.3 v1 の Ed25519 秘密鍵 PEM。
    const ED25519_V1_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";

    // 決定的な合成 DER（SEQUENCE ヘッダ + 適当な中身。304 バイト）を、
    // RFC 8410 §10.2 の証明書 PEM 例と同じ 66 文字幅で改行した PEM。
    // ラベル解析・複数ブロック連結の検証が目的であり、X.509 の意味論
    // そのものの検証は対象外（本 Issue のスコープ外・#963 の担当）。
    const CERT_PEM_66_COL: &str = "-----BEGIN CERTIFICATE-----\nMIIBLAMKERgfJi00O0JJUFdeZWxzeoGIj5adpKuyucDHztXc4+rx+P8GDRQbIikwNz\n5FTFNaYWhvdn2Ei5KZoKeutbzDytHY3+bt9PsCCRAXHiUsMzpBSE9WXWRrcnmAh46V\nnKOqsbi/xs3U2+Lp8Pf+BQwTGiEoLzY9REtSWWBnbnV8g4qRmJ+mrbS7wsnQ197l7P\nP6AQgPFh0kKzI5QEdOVVxjanF4f4aNlJuiqbC3vsXM09rh6O/2/QQLEhkgJy41PENK\nUVhfZm10e4KJkJeepayzusHIz9bd5Ovy+QAHDhUcIyoxOD9GTVRbYmlwd36FjJOaoa\nivtr3Ey9LZ4Ofu9fwDChEYHyYtNDtCSVBXXmVsc3qBiI+WnaSrsrnAx87V3OPq8fj/\nBg0UGyIpMA==\n-----END CERTIFICATE-----\n";

    #[test]
    fn decode_ed25519_v1_pem_yields_48_byte_der() {
        let key = decode_private_key_pem(ED25519_V1_PEM.as_bytes()).expect("valid v1 PEM");
        assert_eq!(key.as_slice().len(), 48);
        assert_eq!(&key.as_slice()[..2], &[0x30, 0x2e]);
    }

    #[test]
    fn decode_certificate_chain_single_block() {
        let chain = decode_certificate_chain_pem(CERT_PEM_66_COL.as_bytes()).expect("valid cert");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].len(), 304);
        assert_eq!(&chain[0][..4], &[0x30, 0x82, 0x01, 0x2c]);
    }

    #[test]
    fn decode_certificate_chain_two_blocks_preserves_order() {
        let doubled = format!("{CERT_PEM_66_COL}{CERT_PEM_66_COL}");
        let chain = decode_certificate_chain_pem(doubled.as_bytes()).expect("valid chain");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0], chain[1]);
    }

    #[test]
    fn scan_blocks_rejects_input_over_max_scan_len() {
        // `text` は `pub` API へ直接渡されうるため、`read_bounded_file` を
        // 経由しない呼び出しでも `MAX_PEM_SCAN_INPUT_LEN` で拒否される
        // ことを固定する（PR #1030 codex-review P1 指摘）。
        let oversized = vec![b'a'; MAX_PEM_SCAN_INPUT_LEN + 1];
        assert_eq!(
            decode_certificate_chain_pem(&oversized),
            Err(PemError::InputTooLarge {
                max: MAX_PEM_SCAN_INPUT_LEN
            })
        );
    }

    #[test]
    fn scan_blocks_rejects_many_short_blocks_before_reserving_per_block_memory() {
        // 短い（本文なしの）BEGIN/END ブロックを大量に並べても、
        // ブロック数上限が走査完了を待たずに発火することを固定する。
        // 修正前は `decode_certificate_chain_pem` の
        // `chain.len() > MAX_CERTIFICATE_CHAIN_LEN` 判定が走査完了後にしか
        // 行われず、各ブロックが `text.len()` バイトを確保していたため、
        // 1 MiB 以下の入力でも「入力サイズ × ブロック数」規模のメモリ
        // 確保を誘発できた（PR #1030 codex-review P1 指摘）。
        let mut text = String::new();
        for _ in 0..(MAX_PEM_BLOCKS_PER_SCAN + 1) {
            text.push_str("-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n");
        }
        assert!(text.len() < MAX_PEM_SCAN_INPUT_LEN);
        assert_eq!(
            decode_certificate_chain_pem(text.as_bytes()),
            Err(PemError::TooManyBlocksInScan {
                max: MAX_PEM_BLOCKS_PER_SCAN
            })
        );
    }

    #[test]
    fn decode_certificate_chain_rejects_too_many_blocks() {
        let mut text = String::new();
        for _ in 0..=MAX_CERTIFICATE_CHAIN_LEN {
            text.push_str(CERT_PEM_66_COL);
        }
        assert_eq!(
            decode_certificate_chain_pem(text.as_bytes()),
            Err(PemError::TooManyBlocks {
                max: MAX_CERTIFICATE_CHAIN_LEN
            })
        );
    }

    #[test]
    fn decode_private_key_rejects_rsa_label() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nAA==\n-----END RSA PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnsupportedKeyFormat(LegacyKeyFormat::Pkcs1Rsa))
        );
    }

    #[test]
    fn decode_private_key_rejects_ec_label() {
        let text = "-----BEGIN EC PRIVATE KEY-----\nAA==\n-----END EC PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnsupportedKeyFormat(LegacyKeyFormat::Sec1Ec))
        );
    }

    #[test]
    fn decode_private_key_rejects_encrypted_label() {
        let text =
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAA==\n-----END ENCRYPTED PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnsupportedKeyFormat(
                LegacyKeyFormat::EncryptedPkcs8
            ))
        );
    }

    #[test]
    fn decode_private_key_rejects_openssh_label() {
        let text = "-----BEGIN OPENSSH PRIVATE KEY-----\nAA==\n-----END OPENSSH PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnsupportedKeyFormat(LegacyKeyFormat::OpenSsh))
        );
    }

    #[test]
    fn decode_private_key_rejects_unexpected_label() {
        let text = "-----BEGIN PUBLIC KEY-----\nAA==\n-----END PUBLIC KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnexpectedLabel("PUBLIC KEY".to_string()))
        );
    }

    #[test]
    fn decode_private_key_rejects_two_blocks() {
        let doubled = format!("{ED25519_V1_PEM}{ED25519_V1_PEM}");
        assert_eq!(
            decode_private_key_pem(doubled.as_bytes()),
            Err(PemError::ExpectedSingleBlock { found: 2 })
        );
    }

    #[test]
    fn decode_private_key_rejects_empty_file() {
        assert_eq!(decode_private_key_pem(b""), Err(PemError::NoBlocks));
    }

    #[test]
    fn decode_private_key_rejects_missing_end() {
        let text = "-----BEGIN PRIVATE KEY-----\nAA==\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::MissingEnd)
        );
    }

    #[test]
    fn decode_private_key_rejects_label_mismatch() {
        let text = "-----BEGIN PRIVATE KEY-----\nAA==\n-----END CERTIFICATE-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::LabelMismatch)
        );
    }

    #[test]
    fn decode_private_key_rejects_nested_begin() {
        let text =
            "-----BEGIN PRIVATE KEY-----\n-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnexpectedBoundary)
        );
    }

    #[test]
    fn decode_private_key_rejects_dangling_end() {
        let text = "-----END PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnexpectedBoundary)
        );
    }

    #[test]
    fn decode_private_key_rejects_content_outside_block() {
        let text = format!("garbage\n{ED25519_V1_PEM}");
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::UnexpectedContent)
        );
    }

    #[test]
    fn decode_private_key_rejects_encrypted_pem_headers() {
        let text = "-----BEGIN PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nAA==\n-----END PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::HeadersNotSupported)
        );
    }

    #[test]
    fn decode_private_key_rejects_non_ascii() {
        let mut text = ED25519_V1_PEM.as_bytes().to_vec();
        text.push(0xff);
        assert_eq!(decode_private_key_pem(&text), Err(PemError::NonAscii));
    }

    #[test]
    fn decode_private_key_rejects_invalid_base64_character() {
        let text = "-----BEGIN PRIVATE KEY-----\n!!!!\n-----END PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::Base64(Base64DecodeError::InvalidCharacter))
        );
    }

    #[test]
    fn decode_private_key_rejects_invalid_padding() {
        let text = "-----BEGIN PRIVATE KEY-----\nAA=A\n-----END PRIVATE KEY-----\n";
        assert_eq!(
            decode_private_key_pem(text.as_bytes()),
            Err(PemError::Base64(Base64DecodeError::InvalidPadding))
        );
    }

    #[test]
    fn ct_base64_sextet_matches_strict_decoder_for_all_bytes() {
        for byte in 0u16..=255 {
            let b = byte as u8;
            let expected = match b {
                b'A'..=b'Z' => Some((b - b'A') as i16),
                b'a'..=b'z' => Some((b - b'a' + 26) as i16),
                b'0'..=b'9' => Some((b - b'0' + 52) as i16),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            };
            let actual = ct_base64_sextet(b);
            match expected {
                Some(v) => assert_eq!(actual, v, "byte={b}"),
                None => assert!(actual < 0, "byte={b} should be invalid"),
            }
        }
    }

    #[test]
    fn round_trip_all_lengths_up_to_64_bytes_via_const_time_decoder() {
        for len in 0..=64usize {
            let bytes: Vec<u8> = (0..len).map(|i| ((i * 37 + 7) % 256) as u8).collect();
            let encoded = crate::http::query::base64_std::encode_base64_std(&bytes);
            let decoded =
                decode_base64_const_time(encoded.as_bytes(), u64::MAX).expect("round trip decode");
            assert_eq!(decoded, bytes, "len={len}");
        }
    }

    #[test]
    fn decode_base64_const_time_rejects_over_max_decoded() {
        let encoded = crate::http::query::base64_std::encode_base64_std(b"foobar");
        assert_eq!(
            decode_base64_const_time(encoded.as_bytes(), 5),
            Err(Base64DecodeError::TooLong)
        );
        assert!(decode_base64_const_time(encoded.as_bytes(), 6).is_ok());
    }

    #[test]
    fn read_bounded_file_rejects_missing_file() {
        let path = crate::test_util::unique_temp_path("tls-pem-missing", ".pem");
        let err = read_bounded_file(&path, MAX_PRIVATE_KEY_FILE_LEN).unwrap_err();
        assert_eq!(err.error, TlsFileError::NotFound);
    }

    #[test]
    fn read_bounded_file_rejects_directory() {
        let dir = std::env::temp_dir();
        let err = read_bounded_file(&dir, MAX_PRIVATE_KEY_FILE_LEN).unwrap_err();
        assert_eq!(err.error, TlsFileError::NotRegularFile);
    }

    // TOCTOU 是正（open → 同一ハンドルの fstat）の回帰テスト。
    // `/dev/null` は「open は成功するが通常ファイルではない」対象の代表例
    // （FIFO はテストプロセスが `File::open` した時点でブロックしうるため
    // 使わない）で、`is_file()` 判定が確かに `open` 後のファイル記述子
    // 経由（`file.metadata()`）で行われていることを固定する。
    #[test]
    #[cfg(unix)]
    fn read_bounded_file_rejects_non_regular_openable_file() {
        let err = read_bounded_file(Path::new("/dev/null"), MAX_PRIVATE_KEY_FILE_LEN).unwrap_err();
        assert_eq!(err.error, TlsFileError::NotRegularFile);
    }

    #[test]
    fn read_bounded_file_rejects_oversized_file() {
        let path = crate::test_util::unique_temp_path("tls-pem-oversized", ".pem");
        std::fs::write(&path, vec![b'a'; 16]).expect("write temp file");
        let result = read_bounded_file(&path, 8);
        let _ = std::fs::remove_file(&path);
        let err = result.unwrap_err();
        assert_eq!(err.error, TlsFileError::TooLarge { max: 8 });
    }

    #[test]
    fn read_bounded_file_reads_within_limit() {
        let path = crate::test_util::unique_temp_path("tls-pem-ok", ".pem");
        std::fs::write(&path, ED25519_V1_PEM.as_bytes()).expect("write temp file");
        let result = read_bounded_file(&path, MAX_PRIVATE_KEY_FILE_LEN);
        let _ = std::fs::remove_file(&path);
        let buf = result.expect("read succeeds");
        assert_eq!(buf.as_slice(), ED25519_V1_PEM.as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn read_bounded_file_rejects_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let path = crate::test_util::unique_temp_path("tls-pem-denied", ".pem");
        std::fs::write(&path, b"secret").expect("write temp file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("set permissions");
        let result = read_bounded_file(&path, MAX_PRIVATE_KEY_FILE_LEN);
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_file(&path);
        // root（またはモード制限が効かない環境）ではパーミッションによる
        // 拒否そのものが再現できないため、その場合はテストを飛ばす
        // （`unsafe` を使わずに判定するため、実際に読めてしまったかどうかで
        // 判定する。`io::ErrorKind` -> variant の写像自体は他の分岐で
        // 機械検証済み）。
        match result {
            Ok(_) => {}
            Err(err) => assert_eq!(err.error, TlsFileError::PermissionDenied),
        }
    }
}
