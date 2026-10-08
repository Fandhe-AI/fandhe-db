//! ingest 経路（`crate::tenant::insert_rows` → `insert_rows_unchecked`）の段別
//! 内訳プロファイル（Issue #396。記録先は `docs/design/ingest-stage-profile.md`）。
//! `ingest_profile_bench.rs`（時間依存の実測入口）と `tests/ingest_profile_accept.rs`
//! （時間非依存の回帰）の双方から `#[path]` で取り込まれる共有ソース
//! （`harness/mod.rs` 冒頭コメント・`knn_profile.rs` と同じ取り込み方針）。
//!
//! # 段の再実装（ドリフト対策）
//!
//! `insert_rows_unchecked`（`crates/engine/src/tenant.rs`）の内部段（`storage.rs::
//! encode_row`・`recovery::content_hash::for_insert_batch_encoded`（Issue #397 で
//! 事前エンコード共有化）・`recovery::ledger::record_in_txn` の台帳エントリ符号化）は
//! いずれも `pub(crate)` で、独立コンパイル
//! 単位であるベンチからは呼べない（`knn_profile.rs` と同じ制約。`docs/design/
//! ingest-stage-profile.md`「前提調査の要点」節参照）。本モジュールはこれらを
//! `std` のみで再実装し、`tests/ingest_profile_accept.rs` が正本（`engine::storage::
//! Storage`・実際に書き込まれた台帳エントリ）とのバイト単位一致でドリフトを検出する。
//!
//! SHA-256 は `recovery::content_hash.rs` と同じ理由（依存追加が承認制のため
//! 自作する。`.claude/rules/dependency-policy.md`）で本モジュール内に安全 Rust で
//! 独立実装する。FIPS 180-4 の公開テストベクタで正当性を検証する
//! （`tests/ingest_profile_accept.rs`）。鍵・トークン等のセキュリティ用途には
//! 転用しない（OWASP A02。ベンチ・テストの照合目的専用）。
//!
//! `std` のみに依存する（`harness` の他モジュールと同じ理由。`engine::` を
//! 参照しない）。

use std::fmt;
use std::time::Duration;

/// [`encode_row_reimpl`] が拒否する行フォーマットの上限（`knn_profile.rs` と同じ
/// 設計判断: 本番の上限〔`storage.rs::MAX_EMBEDDING_DIM`・`MAX_METADATA_LEN`〕より
/// 十分大きく取り、正当な計測入力を拒否しないようにしつつ、破損バイト列からの
/// 無制限確保だけを防ぐ）。
pub const MAX_REIMPL_DIM: u32 = 1_000_000;
pub const MAX_REIMPL_METADATA_LEN: u32 = 64 * 1024 * 1024;

/// `storage.rs::MAX_TENANT_ID_LEN`（256）と同値の独立コピー（`knn_profile.rs` と
/// 同じ理由）。
pub const MAX_REIMPL_TENANT_ID_LEN: u16 = 256;

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestProfileError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// env 変数の解析・範囲検証に失敗した（未設定→既定へ倒す一方、空文字・
    /// 非数値・範囲外は黙って既定へフォールバックせず拒否する。coding-rust.md
    /// 「untrusted 入力の扱い」）。
    InvalidEnv { name: &'static str, reason: String },
    /// ベンチ内再実装（encode／content_hash／台帳値）が入力・出力を解釈できなかった。
    Codec(String),
    /// 行数が 0 のため ns/行への換算ができない。
    ZeroRows,
    /// 段別合計 (Σ) が e2e 実測値を上回った（測定異常。負の残差は「未確定」として
    /// 呼び出し側が表示継続する。[`knn_profile.rs::stage_diff_ns_per_row`] と同方針）。
    NonMonotonic,
}

impl fmt::Display for IngestProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestProfileError::RefusedUnderGitHubActions => write!(
                f,
                "ingest_profile_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); this bench is manual-only and not wired into any workflow"
            ),
            IngestProfileError::InvalidEnv { name, reason } => {
                write!(f, "invalid env {name}: {reason}")
            }
            IngestProfileError::Codec(msg) => write!(f, "codec error: {msg}"),
            IngestProfileError::ZeroRows => write!(f, "cannot compute ns/row for zero rows"),
            IngestProfileError::NonMonotonic => write!(
                f,
                "stage sum exceeds e2e median: residual is not monotonically non-negative"
            ),
        }
    }
}

impl std::error::Error for IngestProfileError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`knn_profile.rs` と同一パターン）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), IngestProfileError> {
    if under_github_actions {
        return Err(IngestProfileError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// env 変数を上限検証付きで解析する（R2）。未設定 (`raw == None`) は `default` を
/// 採用し、空文字・非数値・`[min, max]` 範囲外は `Err`（黙って既定へ倒さない。
/// coding-rust.md「untrusted 入力の扱い」）。
pub fn parse_bounded_env(
    name: &'static str,
    raw: Option<&str>,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, IngestProfileError> {
    let Some(s) = raw else {
        return Ok(default);
    };
    if s.is_empty() {
        return Err(IngestProfileError::InvalidEnv {
            name,
            reason: "value is empty".to_string(),
        });
    }
    let value: usize = s.parse().map_err(|_| IngestProfileError::InvalidEnv {
        name,
        reason: format!("not a valid non-negative integer: {s:?}"),
    })?;
    if value < min || value > max {
        return Err(IngestProfileError::InvalidEnv {
            name,
            reason: format!("value {value} out of range [{min}, {max}]"),
        });
    }
    Ok(value)
}

/// I6（redb insert）段の A/B 計測モード（Issue #400）。`Insert` は既存の
/// `Table::insert`（スクラッチ → ページコピー）、`Reserve` は redb 4.2.0
/// `Table::insert_reserve`（`&[u8]` が `MutInPlaceValue` を実装済みのため
/// `unsafe` 不要で呼べる）で予約した `AccessGuardMutInPlace::as_mut()` へ、
/// I5 で作成済みのエンコード結果（`row_encoded`）をそのまま
/// `copy_from_slice` で書き込む（計測範囲の訂正・codex-review 指摘・
/// PR #420。訂正前は [`encode_row_reimpl_into_slice`] で予約済みバッファへ
/// 再エンコードしており、`Insert` 側と処理範囲が揃わない二重エンコードに
/// なっていた）。両モードとも I5 のエンコード結果（`row_encoded`）を
/// 再利用する契約は不変。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertMode {
    /// 既定。`Table::insert(key, encoded.as_slice())`。
    Insert,
    /// 試作。`Table::insert_reserve(key, encoded.len())` へ直接書き込む。
    Reserve,
}

/// `BENCH_INGEST_PROFILE_INSERT_MODE` を解釈する（R2・fail-closed）。未設定は
/// 既定 `Insert` へ倒す一方、空文字・大文字小文字違い・未知値は黙って既定へ
/// フォールバックせず拒否する（`parse_bounded_env` と同じ untrusted 入力方針。
/// coding-rust.md「untrusted 入力の扱い」）。
pub fn parse_insert_mode(raw: Option<&str>) -> Result<InsertMode, IngestProfileError> {
    match raw {
        None => Ok(InsertMode::Insert),
        Some("insert") => Ok(InsertMode::Insert),
        Some("reserve") => Ok(InsertMode::Reserve),
        Some(other) => Err(IngestProfileError::InvalidEnv {
            name: "BENCH_INGEST_PROFILE_INSERT_MODE",
            reason: format!("unknown insert mode: {other:?} (expected \"insert\" or \"reserve\")"),
        }),
    }
}

/// ベンチのモード（Issue #484）。`Batch`（既定・Issue #396 の既存挙動）は
/// `tenant::insert_rows`（複数行 1 write txn）の段別内訳、`Single` は crossdb
/// ベンチ（`docs/design/crossdb-bench.md`）が実際に通る単文 wire 経路
/// （`INSERT` 1 文 ＝ 1 write txn）の段別内訳を計測する
/// （`ingest_profile_bench.rs` モジュール冒頭コメント「Issue #484 追記」節参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileMode {
    Batch,
    Single,
}

/// `BENCH_INGEST_PROFILE_MODE` を解釈する（R2・fail-closed）。未設定は既定
/// `Batch`（後方互換）へ倒す一方、空文字・大文字小文字違い・未知値は黙って
/// 既定へフォールバックせず拒否する（[`parse_insert_mode`] と同じ方針）。
pub fn parse_profile_mode(raw: Option<&str>) -> Result<ProfileMode, IngestProfileError> {
    match raw {
        None => Ok(ProfileMode::Batch),
        Some("batch") => Ok(ProfileMode::Batch),
        Some("single") => Ok(ProfileMode::Single),
        Some(other) => Err(IngestProfileError::InvalidEnv {
            name: "BENCH_INGEST_PROFILE_MODE",
            reason: format!("unknown mode: {other:?} (expected \"batch\" or \"single\")"),
        }),
    }
}

/// single モードの durability A/B 計測モード（Issue #851）。`engine::storage::
/// WriteDurability`（Issue #849）の値を harness 側で独立に表現する（`InsertMode`
/// と同じ理由: ベンチは engine クレートと独立コンパイル単位のため、production の
/// enum をそのまま re-export せず値だけを写像する）。既定は `Immediate`（redb 自身・
/// `WriteDurability::default()` と同一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchDurability {
    /// 既定。1 commit ごとに fsync 相当の同期を伴う（`WriteDurability::Immediate`）。
    Immediate,
    /// opt-in。commit はプロセス内バッファに留まり write すら発行しない
    /// （`WriteDurability::None`。`engine::storage` モジュールドキュメント参照）。
    None,
}

/// `BENCH_INGEST_PROFILE_DURABILITY` を解釈する（R2・fail-closed）。未設定は
/// 既定 `Immediate` へ倒す一方、空文字・大文字小文字違い・未知値は黙って既定へ
/// フォールバックせず拒否する（[`parse_insert_mode`] と同じ方針）。
pub fn parse_durability(raw: Option<&str>) -> Result<BenchDurability, IngestProfileError> {
    match raw {
        None => Ok(BenchDurability::Immediate),
        Some("immediate") => Ok(BenchDurability::Immediate),
        Some("none") => Ok(BenchDurability::None),
        Some(other) => Err(IngestProfileError::InvalidEnv {
            name: "BENCH_INGEST_PROFILE_DURABILITY",
            reason: format!("unknown durability: {other:?} (expected \"immediate\" or \"none\")"),
        }),
    }
}

impl BenchDurability {
    /// ヘッダ行・summarize 側の arm 照合に使う短いトークン表現
    /// （`durability_opt.rs::token_for` と同じ語彙）。
    pub fn token(self) -> &'static str {
        match self {
            BenchDurability::Immediate => "immediate",
            BenchDurability::None => "none",
        }
    }
}

/// single モードの既定単文数（crossdb ベンチと同じ 25,000 行。
/// `docs/design/crossdb-bench.md` 参照）。
pub const DEFAULT_SINGLE_STATEMENTS: usize = 25_000;
/// single モードの単文数下限（統計的に意味のある warmup／計測サンプル数を
/// 確保するための下限。[`SINGLE_WARMUP_STATEMENTS`] の 2 倍以上を要求する）。
pub const MIN_SINGLE_STATEMENTS: usize = 2_000;
/// single モードの単文数上限（redb 既定 durability 下では 1 文ごとに fsync が
/// 走るため、無制限な長時間実行を防ぐ実装上の上限。INDEX-4・`batch_limits.rs`
/// とは独立の、本ベンチ専用の安全弁）。
pub const MAX_SINGLE_STATEMENTS: usize = 100_000;
/// single モードの warmup 単文数（統計から除外するが投入自体は行う。先頭
/// ページキャッシュ・アロケータのウォームアップを計測対象から外す）。
pub const SINGLE_WARMUP_STATEMENTS: usize = 1_000;

/// 投入文数と所要時間から集計 rows/s を算出する（crossdb ベンチの
/// `ingest_single_stmt`（rows/s）と同じ単位で並記できるようにする）。
/// 文数 0・所要時間 0 はいずれも意味のある比率を持たないため拒否する
/// （`ns_per_row` の `ZeroRows` と同じ契約を流用）。
pub fn rows_per_sec(stmts: usize, elapsed: Duration) -> Result<f64, IngestProfileError> {
    if stmts == 0 {
        return Err(IngestProfileError::ZeroRows);
    }
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return Err(IngestProfileError::ZeroRows);
    }
    Ok(stmts as f64 / secs)
}

/// `recovery::content_hash.rs::OpTag::Insert` の独立コピー（[`content_hash_insert_batch_reimpl`]
/// の `CONTENT_HASH_OP_TAG_INSERT_BATCH` と同じ位置づけ。型付き挿入は行形
/// `INSERT` と同じ「新規挿入」操作としてタグを共有する。`for_typed_insert`
/// ドキュメント参照）。
const CONTENT_HASH_OP_TAG_INSERT: u8 = 1;

/// 可変長バイト列を「4 バイト LE 長さ＋本体」で連結する（`recovery::
/// content_hash.rs::HashInputBuilder::push_bytes` の独立コピー）。
fn push_len_prefixed(buf: &mut Vec<u8>, field: &[u8]) -> Result<(), IngestProfileError> {
    let len = u32::try_from(field.len())
        .map_err(|_| IngestProfileError::Codec("content hash field too large".to_string()))?;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(field);
    Ok(())
}

/// `recovery::content_hash.rs::for_typed_insert` の再実装（single モード I3 用。
/// `insert_typed_row_unchecked` が実際に渡す入力と同じ形——id・visibility・
/// VECTOR 列の埋め込み・非 VECTOR 列の（列名, 値）ペア列——を受け取るが、本
/// モジュールは `engine::row_codec::Value` を参照しない契約（モジュール冒頭
/// コメント）のため、Text 列のみを `Option<&str>`（`None` は
/// `push_named_scalar_columns` と同じく素通しで除外する SQL 上の未指定/NULL
/// 相当）として受け取る（本ベンチが対象とする `docs` スキーマの非 VECTOR 列は
/// `body`（Text）のみで、Null／Vector 型の非 VECTOR 列は対象外）。
///
/// バイトレイアウト（`for_typed_insert`／`push_vector`／`push_named_scalar_columns`
/// と同一）: ドメインタグ ＋ `OpTag::Insert`(=1) ＋ `id`(u64 LE) ＋
/// visibility(1 バイト。`0x01`=Public／`0x02`=Private) ＋ 埋め込み
/// （4 バイト LE 長さ ＋ f32 LE 列。長さプレフィクスは [`push_len_prefixed`] の
/// ものではなく生の 4 バイトのみ） ＋ 非 Null 列ごとに（列名を長さ
/// プレフィクス付きで ＋ `Value::Text` タグ(=1) ＋ 本文を長さプレフィクス付きで）。
pub fn content_hash_typed_insert_reimpl(
    id: u64,
    is_public: bool,
    embedding: &[f32],
    columns: &[(&str, Option<&str>)],
) -> Result<[u8; 32], IngestProfileError> {
    let mut buf = Vec::new();
    buf.extend_from_slice(CONTENT_HASH_DOMAIN_TAG);
    buf.push(CONTENT_HASH_OP_TAG_INSERT);
    buf.extend_from_slice(&id.to_le_bytes());
    buf.push(if is_public { 0x01 } else { 0x02 }); // Visibility::{PUBLIC,PRIVATE}_BYTE
    let dim = u32::try_from(embedding.len()).map_err(|_| {
        IngestProfileError::Codec("embedding dim too large for content hash".to_string())
    })?;
    buf.extend_from_slice(&dim.to_le_bytes());
    for f in embedding {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    for (name, value) in columns {
        let Some(text) = value else {
            // `Value::Null` の列は push_named_scalar_columns と同じく素通しで除外する。
            continue;
        };
        push_len_prefixed(&mut buf, name.as_bytes())?;
        buf.push(1u8); // Value::Text タグ
        push_len_prefixed(&mut buf, text.as_bytes())?;
    }
    Ok(sha256_reimpl(&buf))
}

/// `storage.rs::encode_row` の行フォーマット v2 再実装（モジュール冒頭コメント
/// 参照）。`storage.rs` と同じフィールド検証順序・エラー条件を踏襲する。
pub fn encode_row_reimpl(
    tenant_id: &str,
    is_public: bool,
    embedding: &[f32],
    metadata: &[u8],
) -> Result<Vec<u8>, IngestProfileError> {
    if tenant_id.is_empty() {
        return Err(IngestProfileError::Codec(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_bytes = tenant_id.as_bytes();
    let tenant_len = u16::try_from(tenant_bytes.len()).map_err(|_| {
        IngestProfileError::Codec(format!("tenant_id too long: {} bytes", tenant_bytes.len()))
    })?;
    if tenant_len > MAX_REIMPL_TENANT_ID_LEN {
        return Err(IngestProfileError::Codec(format!(
            "tenant_id length {tenant_len} exceeds reimpl limit {MAX_REIMPL_TENANT_ID_LEN}"
        )));
    }

    let dim = u32::try_from(embedding.len()).map_err(|_| {
        IngestProfileError::Codec(format!("embedding dim too large: {}", embedding.len()))
    })?;
    if dim > MAX_REIMPL_DIM {
        return Err(IngestProfileError::Codec(format!(
            "embedding dim {dim} exceeds reimpl limit {MAX_REIMPL_DIM}"
        )));
    }
    let metadata_len = u32::try_from(metadata.len()).map_err(|_| {
        IngestProfileError::Codec(format!("metadata too large: {}", metadata.len()))
    })?;
    if metadata_len > MAX_REIMPL_METADATA_LEN {
        return Err(IngestProfileError::Codec(format!(
            "metadata length {metadata_len} exceeds reimpl limit {MAX_REIMPL_METADATA_LEN}"
        )));
    }

    let mut buf = Vec::with_capacity(
        1 + 2 + tenant_bytes.len() + 1 + 4 + embedding.len() * 4 + 4 + metadata.len(),
    );
    buf.push(2u8); // storage.rs::ROW_FORMAT_VERSION
    buf.extend_from_slice(&tenant_len.to_le_bytes());
    buf.extend_from_slice(tenant_bytes);
    buf.push(if is_public { 0x01 } else { 0x02 }); // Visibility::{PUBLIC,PRIVATE}_BYTE
    buf.extend_from_slice(&dim.to_le_bytes());
    for v in embedding {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(&metadata_len.to_le_bytes());
    buf.extend_from_slice(metadata);
    Ok(buf)
}

/// [`encode_row_reimpl`] と同一の検証・フィールド順序を、呼び出し元が確保した
/// 固定長スライス `dst` へ直接書き込む版（Issue #400。`Table::insert_reserve`
/// が返す `AccessGuardMutInPlace::as_mut()` の書き込み先を模す）。`dst.len()`
/// は事前に呼び出し元が算出した符号化後長（[`encode_row_reimpl`] が返す
/// `Vec` の長さと同じ計算式）と一致していなければならず、不一致は `Err`
/// にする（`unsafe` を使わず `get_mut` による範囲検証付きスライスで書く。
/// coding-rust.md「受信データ経路での添字アクセス禁止」と同方針をベンチ内
/// 再実装にも適用する）。
pub fn encode_row_reimpl_into_slice(
    dst: &mut [u8],
    tenant_id: &str,
    is_public: bool,
    embedding: &[f32],
    metadata: &[u8],
) -> Result<(), IngestProfileError> {
    if tenant_id.is_empty() {
        return Err(IngestProfileError::Codec(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_bytes = tenant_id.as_bytes();
    let tenant_len = u16::try_from(tenant_bytes.len()).map_err(|_| {
        IngestProfileError::Codec(format!("tenant_id too long: {} bytes", tenant_bytes.len()))
    })?;
    if tenant_len > MAX_REIMPL_TENANT_ID_LEN {
        return Err(IngestProfileError::Codec(format!(
            "tenant_id length {tenant_len} exceeds reimpl limit {MAX_REIMPL_TENANT_ID_LEN}"
        )));
    }
    let dim = u32::try_from(embedding.len()).map_err(|_| {
        IngestProfileError::Codec(format!("embedding dim too large: {}", embedding.len()))
    })?;
    if dim > MAX_REIMPL_DIM {
        return Err(IngestProfileError::Codec(format!(
            "embedding dim {dim} exceeds reimpl limit {MAX_REIMPL_DIM}"
        )));
    }
    let metadata_len = u32::try_from(metadata.len()).map_err(|_| {
        IngestProfileError::Codec(format!("metadata too large: {}", metadata.len()))
    })?;
    if metadata_len > MAX_REIMPL_METADATA_LEN {
        return Err(IngestProfileError::Codec(format!(
            "metadata length {metadata_len} exceeds reimpl limit {MAX_REIMPL_METADATA_LEN}"
        )));
    }

    let expected_len =
        1 + 2 + tenant_bytes.len() + 1 + 4 + embedding.len() * 4 + 4 + metadata.len();
    if dst.len() != expected_len {
        return Err(IngestProfileError::Codec(format!(
            "dst length {} does not match encoded row length {expected_len}",
            dst.len()
        )));
    }

    let mut offset = 0usize;
    let write =
        |src: &[u8], dst: &mut [u8], offset: &mut usize| -> Result<(), IngestProfileError> {
            let end = offset
                .checked_add(src.len())
                .ok_or_else(|| IngestProfileError::Codec("offset overflow".to_string()))?;
            let slot = dst.get_mut(*offset..end).ok_or_else(|| {
                IngestProfileError::Codec("dst slice out of bounds while encoding".to_string())
            })?;
            slot.copy_from_slice(src);
            *offset = end;
            Ok(())
        };

    write(&[2u8], dst, &mut offset)?; // storage.rs::ROW_FORMAT_VERSION
    write(&tenant_len.to_le_bytes(), dst, &mut offset)?;
    write(tenant_bytes, dst, &mut offset)?;
    write(&[if is_public { 0x01 } else { 0x02 }], dst, &mut offset)?; // Visibility::{PUBLIC,PRIVATE}_BYTE
    write(&dim.to_le_bytes(), dst, &mut offset)?;
    for v in embedding {
        write(&v.to_le_bytes(), dst, &mut offset)?;
    }
    write(&metadata_len.to_le_bytes(), dst, &mut offset)?;
    write(metadata, dst, &mut offset)?;
    Ok(())
}

/// `recovery::content_hash.rs::DOMAIN_TAG` の独立コピー。
// プロジェクト名変更（vector-db → fandhe-db）後も値を固定する。永続化済みデータの整合性検証に使う識別子でありブランド名ではないため、変更すると既存 redb の recovery が不整合判定される。
const CONTENT_HASH_DOMAIN_TAG: &[u8] = b"vector-db/op_ledger/content_hash/v1";
/// `recovery::content_hash.rs::OpTag::InsertBatch` の独立コピー。
const CONTENT_HASH_OP_TAG_INSERT_BATCH: u8 = 2;

/// `recovery::content_hash.rs::for_insert_batch` の再実装。入力は要求記載順の
/// `(id, encoded_row)` 列（呼び出し元が [`encode_row_reimpl`] で符号化済みの行を渡す）。
pub fn content_hash_insert_batch_reimpl(
    rows: &[(u64, &[u8])],
) -> Result<[u8; 32], IngestProfileError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| IngestProfileError::Codec("content hash batch too large".to_string()))?;
    let mut buf = Vec::new();
    buf.extend_from_slice(CONTENT_HASH_DOMAIN_TAG);
    buf.push(CONTENT_HASH_OP_TAG_INSERT_BATCH);
    buf.extend_from_slice(&count.to_le_bytes());
    for (id, encoded) in rows {
        buf.extend_from_slice(&id.to_le_bytes());
        let len = u32::try_from(encoded.len())
            .map_err(|_| IngestProfileError::Codec("content hash field too large".to_string()))?;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(encoded);
    }
    Ok(sha256_reimpl(&buf))
}

/// `recovery::ledger.rs::encode_entry_v2` の独立コピー（バージョンバイト `0x02` ＋
/// 32 バイト内容ハッシュ）。
pub fn ledger_entry_v2_reimpl(hash: &[u8; 32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 32);
    buf.push(2u8); // recovery::ledger.rs::LEDGER_ENTRY_FORMAT_VERSION_V2
    buf.extend_from_slice(hash);
    buf
}

/// [`ledger_entry_v2_reimpl`] の逆写像。空値・未知バージョン・長さ不一致は
/// fail-closed に拒否する（`recovery::ledger.rs::decode_entry` と同方針）。
pub fn decode_ledger_entry_v2_reimpl(value: &[u8]) -> Result<[u8; 32], IngestProfileError> {
    match value.split_first() {
        Some((&2u8, rest)) => rest.try_into().map_err(|_| {
            IngestProfileError::Codec("ledger v2 entry has wrong hash length".to_string())
        }),
        Some((other, _)) => Err(IngestProfileError::Codec(format!(
            "unknown ledger entry format version: {other}"
        ))),
        None => Err(IngestProfileError::Codec(
            "ledger entry value is empty".to_string(),
        )),
    }
}

/// `recovery::ledger.rs::encode_last_op` の独立コピー（バージョンバイト `0x01` ＋
/// `operation_id` の UTF-8 バイト列）。
pub fn last_op_entry_reimpl(op_id: &str) -> Vec<u8> {
    let bytes = op_id.as_bytes();
    let mut buf = Vec::with_capacity(1 + bytes.len());
    buf.push(1u8); // recovery::ledger.rs::LAST_OP_FORMAT_VERSION_V1
    buf.extend_from_slice(bytes);
    buf
}

/// ingest 経路の段（`insert_rows_unchecked` の内部段分解。モジュール冒頭コメント・
/// `docs/design/ingest-stage-profile.md`「段の定義」節参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageId {
    /// I1: 所有権検査（`PolicyContext::is_owner` 全件）＋ バッチ内 id 重複検出。
    Precheck,
    /// I2: `begin_write`。
    BeginWrite,
    /// I3: content_hash（I5 のエンコード済みバイト列に対する SHA-256 再実装のみ。
    /// Issue #397 以前は本段の内部でも全行を再度 encode していたが、production の
    /// 事前エンコード共有化に追随してレプリカも I5 の結果を共有する形へ変更した）。
    ContentHash,
    /// I4: 台帳記録（`op_ledger` get+insert・`last_op` insert）。
    Ledger,
    /// I5: encode（行ごとに 1 回のみ。I3・I6 の双方がこの結果を共有する。
    /// `ingest_profile_bench.rs` では I2 の直後・I3 より前に実行する順序へ変更済み
    /// （Issue #397）。モジュール冒頭コメント「段の再実装」節参照）。
    Encode,
    /// I6: redb insert（`insert_unique_row` 相当）。
    RedbInsert,
    /// I7: 世代更新（`table_generation` get→checked_add→insert）。
    GenerationBump,
    /// I8: `commit`。
    Commit,
}

impl StageId {
    pub const ALL: [StageId; 8] = [
        StageId::Precheck,
        StageId::BeginWrite,
        StageId::ContentHash,
        StageId::Ledger,
        StageId::Encode,
        StageId::RedbInsert,
        StageId::GenerationBump,
        StageId::Commit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StageId::Precheck => "I1_precheck",
            StageId::BeginWrite => "I2_begin_write",
            StageId::ContentHash => "I3_content_hash",
            StageId::Ledger => "I4_ledger",
            StageId::Encode => "I5_encode",
            StageId::RedbInsert => "I6_redb_insert",
            StageId::GenerationBump => "I7_generation_bump",
            StageId::Commit => "I8_commit",
        }
    }
}

/// 段別のサンプル列（1 バッチ = 1 サンプル）を蓄積する。`StageId::ALL` の各段に
/// 対応する `Vec<Duration>` を保持し、`push` で追記する。
#[derive(Debug, Default)]
pub struct StageSamples {
    samples: [Vec<Duration>; 8],
}

impl StageSamples {
    pub fn new() -> Self {
        Self::default()
    }

    fn index_of(stage: StageId) -> usize {
        StageId::ALL.iter().position(|s| *s == stage).unwrap_or(0)
    }

    pub fn push(&mut self, stage: StageId, duration: Duration) {
        let idx = Self::index_of(stage);
        if let Some(v) = self.samples.get_mut(idx) {
            v.push(duration);
        }
    }

    pub fn samples_for(&self, stage: StageId) -> &[Duration] {
        let idx = Self::index_of(stage);
        self.samples.get(idx).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// 段別中央値の合計（Σ(I1..I8)）を計算する。
pub fn sum_durations(values: &[Duration]) -> Duration {
    values.iter().fold(Duration::ZERO, |acc, d| acc + *d)
}

/// 1 段あたりの ns/行換算（`total` を `rows` で割る）。
pub fn ns_per_row(total: Duration, rows: usize) -> Result<f64, IngestProfileError> {
    if rows == 0 {
        return Err(IngestProfileError::ZeroRows);
    }
    Ok(total.as_secs_f64() * 1e9 / rows as f64)
}

/// e2e 実測値（E0 median）と段別合計（Σ(I1..I8) median）から残差を求める
/// （`E0 - Σ`）。Σ が E0 を上回る場合は測定異常として `Err(NonMonotonic)`
/// を返し、呼び出し元は「未確定」として出力継続する（fail-closed に落とさない。
/// `knn_profile.rs::stage_diff_ns_per_row` と同方針）。
pub fn residual_ns_per_row(
    e2e_median: Duration,
    stage_sum_median: Duration,
    rows: usize,
) -> Result<f64, IngestProfileError> {
    let diff = e2e_median
        .checked_sub(stage_sum_median)
        .ok_or(IngestProfileError::NonMonotonic)?;
    ns_per_row(diff, rows)
}

/// 1 段の実測結果を人間可読な 1 行へ整形する（stdout 出力用。本ベンチは spec 由来の
/// 閾値を持たない情報提供専用のため、実測値をそのまま出力してよい
/// （`.claude/rules/spec-confidentiality.md` のオーナー判断範囲）。
pub fn render_stage_line(name: &str, rows: usize, median: Duration, ns_per_row: f64) -> String {
    format!(
        "stage({name}): rows={rows} median={:.3}ms ns_per_row={ns_per_row:.1}",
        median.as_secs_f64() * 1e3
    )
}

// ---------------------------------------------------------------------------
// SHA-256（FIPS 180-4）自作実装。`recovery::content_hash.rs` と同一の理由
// （依存追加が承認制。dependency-policy.md）で独立に再実装する。`unsafe` は
// 使わず、固定サイズ配列・`wrapping_*` 演算のみで構成する。
// ---------------------------------------------------------------------------

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn pad(input: &[u8]) -> Vec<u8> {
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    msg
}

/// SHA-256（FIPS 180-4）。`recovery::content_hash_reimpl` 用に本モジュール内で
/// 完結させる（`pub` にして `tests/ingest_profile_accept.rs` の既知ダイジェスト
/// テストからも直接呼べるようにする）。
pub fn sha256_reimpl(input: &[u8]) -> [u8; 32] {
    let padded = pad(input);
    let mut h = H0;

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            if let Some(slot) = w.get_mut(i) {
                *slot = u32::from_be_bytes(*word);
            }
        }
        for i in 16..64 {
            let w15 = w[i - 15];
            let w2 = w[i - 2];
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        let bytes = word.to_be_bytes();
        let start = i * 4;
        if let Some(slot) = out.get_mut(start..start + 4) {
            slot.copy_from_slice(&bytes);
        }
    }
    out
}
