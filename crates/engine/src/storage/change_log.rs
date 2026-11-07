//! 永続変更ログ（増分バックアップ・PITR の土台。ポインタ: RECOVER-14。設計は
//! `docs/design/change-log-pitr.md` の D1・D2・D3・D5・D8）。
//!
//! # 役割と責務境界
//!
//! - 書き込み側の表ハンドルのラッパ [`CapturingTable`] と表単位の [`delete_table`]・[`rename_table`]
//!   が、書き込んだキーと表のライフサイクルを capture frame（[`CaptureFrame`]）へ積む
//! - [`CaptureSession`] は `GatedWriteTxn` が寿命を結び付けて持つ付帯物で、commit 境界
//!   （`recovery::commit_boundary::commit_and_finish`・`storage::bump_generation_and_commit`）の
//!   prepare 段へ**明示的に渡される**。prepare は frame の登録簿（`Box` のアドレス引き）を使わず、
//!   session が持つ frame へ直接アクセスして after-image を読み、1 commit = 1 レコードを
//!   同じ redb トランザクションの `change_log` 表へ追記する（commit と原子的）
//! - 無効時（[`ChangeLogState`] が `Storage` に無い）は frame・session を確保しない。ラッパの
//!   open は原子的カウンタを 1 回読むだけで戻る
//!
//! 本モジュールは土台で、既存の書き込み点はまだラッパへ移行していない（後続 Issue #3068）。
//! 有効化の入口（`Storage::enable_change_log`）も crate 内に閉じ、運用者の CLI からは
//! 拒否される（#3069 まで）。そのため未使用項目の警告を抑止している。
//!
//! # 記録範囲の例外
//!
//! [`CHANGE_LOG_TABLE_NAME`] 自身と [`CHANGE_LOG_META_TABLE_NAME`]（DB 識別子・timeline・後続の
//! アーカイブ水位）は記録対象外で、SQL・カタログから名前解決されない（利用者の表は redb 上では
//! `user_rows/{表名}` 等の接頭辞つきの名前になるため衝突しない。構造テストで固定する）。
//!
//! # 資源上限と fail-closed
//!
//! frame のキー数・バイト数、レコードのペイロードには上限（[`CaptureLimits`]。実装既定値で
//! spec 由来ではない）を置き、超過・ロック汚染・未知の型の表・multimap 表は commit 前に
//! `StorageError::CommitFootprintUnrecordable` で拒否する（durable な書き込みは発生しない）。
//! エラー文言には表名・件数・LSN を含めない。

#![allow(dead_code)]

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use redb::{Key, MutInPlaceValue, ReadableTable, TableDefinition, TableHandle, Value};

use super::table_image::{self, AfterImage, Meter, ReadMode, ReadOutcome};
use super::{Result, StorageError, GENERATION_KEY, GENERATION_TABLE};

/// 変更ログの本体表の redb 名。キーは LSN（`storage_generation` の値）、値は [`encode_record`] の出力。
pub(crate) const CHANGE_LOG_TABLE_NAME: &str = "change_log";
/// DB 識別子・timeline（後続でアーカイブ水位）を持つ表の redb 名。
pub(crate) const CHANGE_LOG_META_TABLE_NAME: &str = "change_log_meta";

const CHANGE_LOG_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new(CHANGE_LOG_TABLE_NAME);
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new(CHANGE_LOG_META_TABLE_NAME);
const META_KEY_DB_ID: &str = "db_id";
const META_KEY_TIMELINE: &str = "timeline";

/// timeline の初期値（PITR・復元のたびに分岐する番号。#2446 と整合させる決め方は後続）。
pub(crate) const INITIAL_TIMELINE: u64 = 1;
/// DB 識別子のバイト数。
pub(crate) const DB_ID_LEN: usize = 16;

/// レコードの形式版数。未知の版数はデコードで拒否する。
pub(crate) const RECORD_FORMAT_VERSION: u16 = 1;
/// ストレージ形式の版数（`Storage::open` の移行などで redb の中身の形が変わったときに上げる。
/// 版数が一致しないレコードの再生は fail-closed とする。D2）。
pub(crate) const STORAGE_FORMAT_VERSION: u32 = 1;
const RECORD_MAGIC: [u8; 4] = *b"FCLR";
const CHECKSUM_LEN: usize = 32;
/// マジック 4 + 形式版数 2 + DB 識別子 16 + timeline 8 + LSN 8 + prev LSN 8 + 時刻 8 +
/// ストレージ版数 4 + ペイロード長 4。
const HEADER_LEN: usize = 4 + 2 + DB_ID_LEN + 8 + 8 + 8 + 8 + 4 + 4;

/// 変更ログが記録対象から外す redb 表か（本体表とメタ表。D3 の記録範囲の例外）。
pub(crate) fn is_excluded_table(name: &str) -> bool {
    name == CHANGE_LOG_TABLE_NAME || name == CHANGE_LOG_META_TABLE_NAME
}

// ---------------------------------------------------------------------------
// 資源上限
// ---------------------------------------------------------------------------

/// 捕捉の資源上限（実装既定値。spec 由来ではない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureLimits {
    /// 1 commit の frame に積むキーの個数の上限。
    pub(crate) max_keys: usize,
    /// 1 commit の frame に積むキーのバイト数の合計の上限。
    pub(crate) max_key_bytes: usize,
    /// 1 commit の表イベント（DROP・rename）の個数の上限。
    pub(crate) max_events: usize,
    /// レコードのペイロード（after-image の合計）のバイト数の上限。デコードの上限も兼ねる。
    pub(crate) max_payload_bytes: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_keys: 4 * 1024 * 1024,
            max_key_bytes: 128 * 1024 * 1024,
            max_events: 65_536,
            max_payload_bytes: 256 * 1024 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// レコード形式（D3）
// ---------------------------------------------------------------------------

/// レコードのヘッダ（チェックサムとペイロード長はエンコード時に計算する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordHeader {
    pub(crate) db_id: [u8; DB_ID_LEN],
    pub(crate) timeline: u64,
    /// この commit の番号（`storage_generation` の値 `G_c`）。
    pub(crate) lsn: u64,
    /// 直前の commit 番号（`lsn - 1`）。再生側が `prev == 現在の storage_generation` を検査する。
    pub(crate) prev_lsn: u64,
    /// commit 時刻（壁時計 UTC マイクロ秒）。
    pub(crate) commit_time_us: i64,
    pub(crate) storage_format: u32,
}

/// 1 つの表の after-image（`table_image::SHAPES` の番号つき）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableChange {
    pub(crate) name: String,
    pub(crate) shape: u32,
    pub(crate) upserts: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) removes: Vec<Vec<u8>>,
}

/// レコード本体。`dropped` の削除 → `created` の作成 → `tables` の upsert／remove の順で
/// 適用する（`table_image::apply` と同じ順序）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordBody {
    pub(crate) dropped: Vec<String>,
    pub(crate) created: Vec<(String, u32)>,
    pub(crate) tables: Vec<TableChange>,
}

/// デコード済みの 1 レコード。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeRecord {
    pub(crate) header: RecordHeader,
    pub(crate) body: RecordBody,
}

/// エンコードの失敗（ペイロードが上限超過・長さが `u32` に収まらない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodeError;

/// デコードの失敗。入力（セグメントファイル等）は untrusted として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// 長さが足りない・余りがある。
    Truncated,
    BadMagic,
    UnsupportedVersion,
    /// ペイロード長が上限を超える。
    TooLarge,
    ChecksumMismatch,
    /// 文字列が UTF-8 でない・入れ子の長さが不整合など。
    Malformed,
}

fn put_u32(out: &mut Vec<u8>, n: usize) -> std::result::Result<(), EncodeError> {
    let n = u32::try_from(n).map_err(|_| EncodeError)?;
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) -> std::result::Result<(), EncodeError> {
    put_u32(out, b.len())?;
    out.extend_from_slice(b);
    Ok(())
}

fn encode_payload(body: &RecordBody, max: usize) -> std::result::Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    put_u32(&mut out, body.dropped.len())?;
    for n in &body.dropped {
        put_bytes(&mut out, n.as_bytes())?;
    }
    put_u32(&mut out, body.created.len())?;
    for (n, shape) in &body.created {
        put_bytes(&mut out, n.as_bytes())?;
        out.extend_from_slice(&shape.to_le_bytes());
    }
    put_u32(&mut out, body.tables.len())?;
    for t in &body.tables {
        put_bytes(&mut out, t.name.as_bytes())?;
        out.extend_from_slice(&t.shape.to_le_bytes());
        put_u32(&mut out, t.upserts.len())?;
        for (k, v) in &t.upserts {
            put_bytes(&mut out, k)?;
            put_bytes(&mut out, v)?;
            if out.len() > max {
                return Err(EncodeError);
            }
        }
        put_u32(&mut out, t.removes.len())?;
        for k in &t.removes {
            put_bytes(&mut out, k)?;
            if out.len() > max {
                return Err(EncodeError);
            }
        }
    }
    if out.len() > max {
        return Err(EncodeError);
    }
    Ok(out)
}

fn checksum(header_bytes: &[u8], payload: &[u8]) -> [u8; CHECKSUM_LEN] {
    let mut h = crate::crypto::sha256::Sha256::new();
    h.update(header_bytes);
    h.update(payload);
    h.finalize()
}

/// レコードを `[ヘッダ | チェックサム | ペイロード]` のバイト列へ符号化する。
/// `max_payload` を超えるペイロードは [`EncodeError`]。
pub(crate) fn encode_record(
    header: &RecordHeader,
    body: &RecordBody,
    max_payload: usize,
) -> std::result::Result<Vec<u8>, EncodeError> {
    let payload = encode_payload(body, max_payload)?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| EncodeError)?;
    let mut head = Vec::with_capacity(HEADER_LEN);
    head.extend_from_slice(&RECORD_MAGIC);
    head.extend_from_slice(&RECORD_FORMAT_VERSION.to_le_bytes());
    head.extend_from_slice(&header.db_id);
    head.extend_from_slice(&header.timeline.to_le_bytes());
    head.extend_from_slice(&header.lsn.to_le_bytes());
    head.extend_from_slice(&header.prev_lsn.to_le_bytes());
    head.extend_from_slice(&header.commit_time_us.to_le_bytes());
    head.extend_from_slice(&header.storage_format.to_le_bytes());
    head.extend_from_slice(&payload_len.to_le_bytes());
    let sum = checksum(&head, &payload);
    let total = HEADER_LEN
        .checked_add(CHECKSUM_LEN)
        .and_then(|n| n.checked_add(payload.len()))
        .ok_or(EncodeError)?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&head);
    out.extend_from_slice(&sum);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 長さ検証つきの前方読み取り器（添字アクセスを使わず `get` で境界を検査する）。
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> std::result::Result<&'a [u8], DecodeError> {
        if n > self.buf.len() {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> std::result::Result<[u8; N], DecodeError> {
        self.take(N)?.try_into().map_err(|_| DecodeError::Truncated)
    }

    fn u32(&mut self) -> std::result::Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// 長さ前置きのバイト列。長さは残りのバイト数で検証してから切り出す（確保しない）。
    fn bytes(&mut self) -> std::result::Result<&'a [u8], DecodeError> {
        let n = usize::try_from(self.u32()?).map_err(|_| DecodeError::Truncated)?;
        self.take(n)
    }

    fn string(&mut self) -> std::result::Result<String, DecodeError> {
        let b = self.bytes()?;
        std::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| DecodeError::Malformed)
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

fn decode_payload(payload: &[u8]) -> std::result::Result<RecordBody, DecodeError> {
    let mut r = Reader { buf: payload };
    let mut body = RecordBody::default();
    // 各要素は最低 4 バイトを消費するため、件数が偽でも読み取り中の `Truncated` で止まる
    // （件数を事前確保に使わない）。
    for _ in 0..r.u32()? {
        body.dropped.push(r.string()?);
    }
    for _ in 0..r.u32()? {
        let name = r.string()?;
        body.created.push((name, r.u32()?));
    }
    for _ in 0..r.u32()? {
        let name = r.string()?;
        let shape = r.u32()?;
        let mut upserts = Vec::new();
        for _ in 0..r.u32()? {
            let k = r.bytes()?.to_vec();
            let v = r.bytes()?.to_vec();
            upserts.push((k, v));
        }
        let mut removes = Vec::new();
        for _ in 0..r.u32()? {
            removes.push(r.bytes()?.to_vec());
        }
        body.tables.push(TableChange {
            name,
            shape,
            upserts,
            removes,
        });
    }
    if !r.is_empty() {
        return Err(DecodeError::Malformed);
    }
    Ok(body)
}

/// [`encode_record`] の出力を検証して復号する。ペイロード長は確保の前に `max_payload` で検証し、
/// チェックサム・形式版数・余りの有無を検査する。ストレージ形式の版数の一致は再生側の責務
/// （`header.storage_format` を比較する）。
pub(crate) fn decode_record(
    bytes: &[u8],
    max_payload: usize,
) -> std::result::Result<ChangeRecord, DecodeError> {
    let mut r = Reader { buf: bytes };
    let head = r.take(HEADER_LEN)?;
    let sum: [u8; CHECKSUM_LEN] = r.array()?;
    let mut h = Reader { buf: head };
    if h.array::<4>()? != RECORD_MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if u16::from_le_bytes(h.array()?) != RECORD_FORMAT_VERSION {
        return Err(DecodeError::UnsupportedVersion);
    }
    let header = RecordHeader {
        db_id: h.array()?,
        timeline: u64::from_le_bytes(h.array()?),
        lsn: u64::from_le_bytes(h.array()?),
        prev_lsn: u64::from_le_bytes(h.array()?),
        commit_time_us: i64::from_le_bytes(h.array()?),
        storage_format: u32::from_le_bytes(h.array()?),
    };
    let payload_len =
        usize::try_from(u32::from_le_bytes(h.array()?)).map_err(|_| DecodeError::TooLarge)?;
    if payload_len > max_payload {
        return Err(DecodeError::TooLarge);
    }
    let payload = r.take(payload_len)?;
    if !r.is_empty() {
        return Err(DecodeError::Malformed);
    }
    if !crate::crypto::constant_time_eq(&checksum(head, payload), &sum) {
        return Err(DecodeError::ChecksumMismatch);
    }
    Ok(ChangeRecord {
        header,
        body: decode_payload(payload)?,
    })
}

// ---------------------------------------------------------------------------
// DB 識別子・有効状態（D5・D8）
// ---------------------------------------------------------------------------

/// 変更ログが有効な `Storage` が持つ不変の設定。無効な `Storage` はこれを持たない（`None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeLogState {
    pub(crate) db_id: [u8; DB_ID_LEN],
    pub(crate) timeline: u64,
    pub(crate) limits: CaptureLimits,
}

/// メタ表から DB 識別子と timeline を読む。無ければ生成して書く（既存 DB は変更ログを初めて
/// 有効にしたときに生成する。D5）。`Ok((state, wrote))` の `wrote` が真なら呼び出し側が
/// 世代を進めない保守 commit で確定する。
///
/// `Storage::enable_change_log` が、有効化前の（capture session を持たない）書き込み
/// トランザクション上で呼ぶ。
pub(crate) fn load_or_init_identity(
    write_txn: &redb::WriteTransaction,
    limits: CaptureLimits,
) -> Result<(ChangeLogState, bool)> {
    let mut meta = write_txn.open_table(META_TABLE)?;
    let invalid = || StorageError::Codec("invalid change log identity".to_string());
    // 読み取りの借用を閉じてから書く（`get` のガードは `meta` を借用する）。
    let stored_id: Option<Vec<u8>> = meta.get(META_KEY_DB_ID)?.map(|v| v.value().to_vec());
    let stored_timeline: Option<Vec<u8>> = meta.get(META_KEY_TIMELINE)?.map(|v| v.value().to_vec());
    let mut wrote = false;
    let db_id: [u8; DB_ID_LEN] = match stored_id {
        Some(v) => v.as_slice().try_into().map_err(|_| invalid())?,
        None => {
            let random = crate::crypto::random::read_urandom(DB_ID_LEN)
                .map_err(redb::StorageError::Io)
                .map_err(StorageError::from)?;
            let id: [u8; DB_ID_LEN] = random.as_slice().try_into().map_err(|_| invalid())?;
            meta.insert(META_KEY_DB_ID, id.as_slice())?;
            wrote = true;
            id
        }
    };
    let timeline = match stored_timeline {
        Some(v) => u64::from_le_bytes(v.as_slice().try_into().map_err(|_| invalid())?),
        None => {
            meta.insert(META_KEY_TIMELINE, INITIAL_TIMELINE.to_le_bytes().as_slice())?;
            wrote = true;
            INITIAL_TIMELINE
        }
    };
    Ok((
        ChangeLogState {
            db_id,
            timeline,
            limits,
        },
        wrote,
    ))
}

// ---------------------------------------------------------------------------
// capture frame（D1）
// ---------------------------------------------------------------------------

/// 表単位のイベント。作成は prepare 段で最終の表集合と基底の表集合の差から導くため積まない。
#[derive(Debug, Clone, PartialEq, Eq)]
enum TableEvent {
    Dropped(Box<str>),
    Renamed { from: Box<str>, to: Box<str> },
}

#[derive(Default)]
struct FrameState {
    /// redb 表名 → 書いたキーのバイト列（上位集合でよい。`ROLLBACK TO` でも巻き戻さない）。
    keys: HashMap<Box<str>, BTreeSet<Vec<u8>>>,
    key_count: usize,
    key_bytes: usize,
    events: Vec<TableEvent>,
    /// 上限超過・ロック汚染。立ったら以降は積まず、prepare が commit 前に拒否する。
    overflow: bool,
}

/// 1 つの書き込みトランザクションが書いたキーと表イベントの収集器。
pub(crate) struct CaptureFrame {
    state: Mutex<FrameState>,
    limits: CaptureLimits,
}

impl CaptureFrame {
    fn new(limits: CaptureLimits) -> Self {
        Self {
            state: Mutex::new(FrameState::default()),
            limits,
        }
    }

    /// 汚染されたロックは「積み漏れの可能性」なので overflow 扱いにして拒否側へ倒す。
    fn lock(&self) -> MutexGuard<'_, FrameState> {
        match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                let mut g = poisoned.into_inner();
                g.overflow = true;
                g
            }
        }
    }

    fn mark_overflow(st: &mut FrameState) {
        st.overflow = true;
        // 以降は積まない。メモリを無制限に使わないよう積んだ分も捨てる。
        st.keys.clear();
        st.events.clear();
        st.key_count = 0;
        st.key_bytes = 0;
    }

    /// `table`（redb 表名）の `key`（`K::as_bytes` のバイト列）を積む。
    pub(crate) fn note_key(&self, table: &str, key: &[u8]) {
        if is_excluded_table(table) {
            return;
        }
        let mut st = self.lock();
        if st.overflow {
            return;
        }
        if st.keys.get(table).is_some_and(|set| set.contains(key)) {
            return;
        }
        let new_count = st.key_count.saturating_add(1);
        let new_bytes = st.key_bytes.saturating_add(key.len());
        if new_count > self.limits.max_keys || new_bytes > self.limits.max_key_bytes {
            Self::mark_overflow(&mut st);
            return;
        }
        st.key_count = new_count;
        st.key_bytes = new_bytes;
        if let Some(set) = st.keys.get_mut(table) {
            set.insert(key.to_vec());
        } else {
            let mut set = BTreeSet::new();
            set.insert(key.to_vec());
            st.keys.insert(table.into(), set);
        }
    }

    fn note_event(&self, event: TableEvent) {
        let mut st = self.lock();
        if st.overflow {
            return;
        }
        if st.events.len() >= self.limits.max_events {
            Self::mark_overflow(&mut st);
            return;
        }
        st.events.push(event);
    }
}

/// frame の登録簿。書き込み点は `&redb::WriteTransaction` しか持たないため、`Box` 内の
/// アドレスで frame を引く（`commit_log` の frame と同じ置き場の流儀）。**引くのは書き込み時の
/// ラッパだけ**で、prepare は [`CaptureSession`] 経由で frame を持つ（`split_for_commit` が
/// トランザクションを移動するためアドレスが変わる）。
static CAPTURE_FRAMES: LazyLock<Mutex<HashMap<usize, Arc<CaptureFrame>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 登録中の frame の個数。0 のとき（変更ログ無効のプロセス）はラッパの open が登録簿のロックを
/// 取らずに戻るための印（無効時のホットパスに確保もロックも持ち込まない）。
static ACTIVE_FRAMES: AtomicUsize = AtomicUsize::new(0);

fn frames_lock() -> MutexGuard<'static, HashMap<usize, Arc<CaptureFrame>>> {
    CAPTURE_FRAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn addr_of(write_txn: &redb::WriteTransaction) -> usize {
    std::ptr::from_ref(write_txn) as usize
}

fn frame_for(write_txn: &redb::WriteTransaction) -> Option<Arc<CaptureFrame>> {
    if ACTIVE_FRAMES.load(Ordering::Relaxed) == 0 {
        return None;
    }
    frames_lock().get(&addr_of(write_txn)).cloned()
}

/// frame の登録の RAII。Drop で登録簿から外す。
struct FrameRegistration {
    frame: Arc<CaptureFrame>,
    addr: usize,
}

impl Drop for FrameRegistration {
    fn drop(&mut self) {
        let mut frames = frames_lock();
        // Box の解放後に別のトランザクションが同じアドレスで登録している場合、それを消さない。
        if frames
            .get(&self.addr)
            .is_some_and(|f| Arc::ptr_eq(f, &self.frame))
        {
            frames.remove(&self.addr);
        }
        ACTIVE_FRAMES.fetch_sub(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// capture session（prepare への明示渡し）
// ---------------------------------------------------------------------------

/// `GatedWriteTxn` が寿命を結び付けて持つ捕捉の付帯物。commit 境界の prepare 段へ渡される。
pub(crate) struct CaptureSession {
    state: Arc<ChangeLogState>,
    frame: Arc<CaptureFrame>,
    /// 書き込みトランザクション開始時点の確定状態（単一ライタなので基底の表集合を与える）。
    base: redb::ReadTransaction,
    /// 宣言順が drop 順（登録解除 → 基底の読み取りの解放）。
    _registration: FrameRegistration,
}

impl CaptureSession {
    /// `write_txn`（`Box` に入れて寿命中アドレスが動かないもの）の frame を登録して session を作る。
    /// `Storage::wrap_gated` が、変更ログが有効なときだけ呼ぶ。
    pub(crate) fn begin(
        state: Arc<ChangeLogState>,
        write_txn: &redb::WriteTransaction,
        base: redb::ReadTransaction,
    ) -> Self {
        let frame = Arc::new(CaptureFrame::new(state.limits));
        let addr = addr_of(write_txn);
        ACTIVE_FRAMES.fetch_add(1, Ordering::Relaxed);
        frames_lock().insert(addr, Arc::clone(&frame));
        Self {
            state,
            frame: Arc::clone(&frame),
            base,
            _registration: FrameRegistration { frame, addr },
        }
    }

    /// commit 準備段階（`prepare_generation_bump` の直後・point of no return の前）で、積まれたキーの
    /// after-image を読んでレコードを組み、同じトランザクションの `change_log` 表へ追記する。
    /// `Err` は commit 前の拒否で、durable な書き込みは発生しない。
    ///
    /// `g_c` は `prepare_generation_bump` が採番した世代（= LSN）。`GENERATION_TABLE` の書き込みは
    /// 登録簿に載らないため、ここで固定のエントリとして積む（D1）。
    pub(crate) fn prepare(&self, write_txn: &redb::WriteTransaction, g_c: u64) -> Result<()> {
        self.frame
            .note_key(GENERATION_TABLE.name(), generation_key_bytes());
        let body = self.build_body(write_txn)?;
        let prev_lsn = g_c
            .checked_sub(1)
            .ok_or(StorageError::CommitFootprintUnrecordable)?;
        let commit_time_us = crate::sql::time_fn::now_micros()
            .map_err(|_| StorageError::CommitFootprintUnrecordable)?;
        let header = RecordHeader {
            db_id: self.state.db_id,
            timeline: self.state.timeline,
            lsn: g_c,
            prev_lsn,
            commit_time_us,
            storage_format: STORAGE_FORMAT_VERSION,
        };
        let bytes = encode_record(&header, &body, self.state.limits.max_payload_bytes)
            .map_err(|_| StorageError::CommitFootprintUnrecordable)?;
        let mut table = write_txn.open_table(CHANGE_LOG_TABLE)?;
        table.insert(g_c, bytes.as_slice())?;
        Ok(())
    }

    /// D1 の正規化。基底の表集合と最終の表集合の差から作成・削除を導き、作成・rename を経た表は
    /// 全行、それ以外は積まれたキーの現在値を読む。
    fn build_body(&self, write_txn: &redb::WriteTransaction) -> Result<RecordBody> {
        let unrecordable = || StorageError::CommitFootprintUnrecordable;
        let st = self.frame.lock();
        if st.overflow {
            return Err(unrecordable());
        }
        // multimap 表は再生器が扱えないため、存在するだけで拒否する（`table_image::capture` と同じ扱い）。
        if write_txn.list_multimap_tables()?.next().is_some()
            || self.base.list_multimap_tables()?.next().is_some()
        {
            return Err(unrecordable());
        }
        let final_names = table_names(write_txn.list_tables()?);
        let base_names = table_names(self.base.list_tables()?);

        // 名前が DROP・rename のイベントに現れた表は、最終に在れば別実体の可能性があるため全行で記録する。
        let mut touched: BTreeSet<&str> = BTreeSet::new();
        for e in &st.events {
            match e {
                TableEvent::Dropped(n) => {
                    touched.insert(n);
                }
                TableEvent::Renamed { from, to } => {
                    touched.insert(from);
                    touched.insert(to);
                }
            }
        }
        let full: BTreeSet<&str> = final_names
            .iter()
            .filter(|n| !base_names.contains(*n) || touched.contains(n.as_str()))
            .map(String::as_str)
            .collect();
        let dropped: Vec<String> = base_names
            .iter()
            .filter(|n| !final_names.contains(*n) || full.contains(n.as_str()))
            .cloned()
            .collect();

        let mut meter = Meter::new(self.state.limits.max_payload_bytes, None);
        let mut body = RecordBody {
            dropped,
            ..RecordBody::default()
        };
        let mut changes: BTreeMap<&str, TableChange> = BTreeMap::new();
        for name in &full {
            let img = read_image(write_txn, name, ReadMode::All, &mut meter)?;
            body.created.push(((*name).to_string(), shape_no(&img)?));
            changes.insert(name, into_change(name, img)?);
        }
        for (name, keys) in &st.keys {
            // 最終に無い表（削除済み）のキーは読む対象にしない。全行で読む表は上位集合として包含する。
            if !final_names.contains(name.as_ref()) || full.contains(name.as_ref()) {
                continue;
            }
            let img = read_image(write_txn, name, ReadMode::Keys(keys), &mut meter)?;
            changes.insert(name, into_change(name, img)?);
        }
        body.tables = changes.into_values().collect();
        Ok(body)
    }
}

fn generation_key_bytes() -> &'static [u8] {
    // `&str` キーの `Value::as_bytes` は UTF-8 そのもの。
    GENERATION_KEY.as_bytes()
}

fn table_names<H: TableHandle>(handles: impl Iterator<Item = H>) -> BTreeSet<String> {
    handles
        .map(|h| h.name().to_string())
        .filter(|n| !is_excluded_table(n))
        .collect()
}

fn read_image(
    write_txn: &redb::WriteTransaction,
    name: &str,
    mode: ReadMode<'_>,
    meter: &mut Meter,
) -> Result<AfterImage> {
    match table_image::read_after_image(write_txn, name, mode, meter) {
        Ok(ReadOutcome::Image(img)) => Ok(img),
        // 未知の型・上限超過・読み取り失敗はいずれも記録できないので commit 前に拒否する。
        Ok(ReadOutcome::UnknownShape | ReadOutcome::OverLimit) | Err(_) => {
            Err(StorageError::CommitFootprintUnrecordable)
        }
    }
}

fn shape_no(img: &AfterImage) -> Result<u32> {
    u32::try_from(img.shape).map_err(|_| StorageError::CommitFootprintUnrecordable)
}

fn into_change(name: &str, img: AfterImage) -> Result<TableChange> {
    Ok(TableChange {
        name: name.to_string(),
        shape: shape_no(&img)?,
        upserts: img.upserts,
        removes: img.removes,
    })
}

// ---------------------------------------------------------------------------
// 書き込み側の表ハンドルのラッパ
// ---------------------------------------------------------------------------

struct TableCapture {
    frame: Arc<CaptureFrame>,
    name: Box<str>,
}

impl TableCapture {
    fn note<K: Key + 'static>(&self, key: &K::SelfType<'_>) {
        self.frame.note_key(&self.name, K::as_bytes(key).as_ref());
    }
}

/// `pop_first`／`pop_last` が返す取り除いた行（キーと値のガード）。
type Popped<'a, K, V> = Option<(redb::AccessGuard<'a, K>, redb::AccessGuard<'a, V>)>;

/// `redb::Table` の書き込み側ラッパ。書いた（書く）キーを capture frame へ積んでから redb へ
/// 委譲する。変更ログが無効なトランザクションでは `capture` が `None` で、素の `Table` と同じ動作。
///
/// 読み取りは `Deref` で素の `Table` の `ReadableTable` 系メソッドをそのまま使える。書き込みは
/// `&mut self` を要する素のメソッドが `Deref` 越しには呼べないため、このラッパのメソッドを
/// 経由するしかない（計装漏れを型で防ぐ。D1 の層 1）。未提供の変更系（`extract_if`・`entry`・
/// カーソル）が要る書き込み点は、削除・更新したキーを積む実装をここへ足してから移行する。
pub(crate) struct CapturingTable<'txn, K: Key + 'static, V: Value + 'static> {
    table: redb::Table<'txn, K, V>,
    capture: Option<TableCapture>,
}

impl<'txn, K: Key + 'static, V: Value + 'static> Deref for CapturingTable<'txn, K, V> {
    type Target = redb::Table<'txn, K, V>;
    fn deref(&self) -> &Self::Target {
        &self.table
    }
}

impl<'txn, K: Key + 'static, V: Value + 'static> CapturingTable<'txn, K, V> {
    /// `write_txn` の `def` を開く。frame が登録されたトランザクション（変更ログ有効）なら
    /// 以降の書き込みを積む。
    pub(crate) fn open(
        write_txn: &'txn redb::WriteTransaction,
        def: TableDefinition<'_, K, V>,
    ) -> std::result::Result<Self, redb::TableError> {
        let table = write_txn.open_table(def)?;
        let capture = frame_for(write_txn).map(|frame| TableCapture {
            frame,
            name: def.name().into(),
        });
        Ok(Self { table, capture })
    }

    /// 変更ログの捕捉が有効か（テスト用の観測）。
    pub(crate) fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    pub(crate) fn insert<'k, 'v>(
        &mut self,
        key: impl Borrow<K::SelfType<'k>>,
        value: impl Borrow<V::SelfType<'v>>,
    ) -> std::result::Result<Option<redb::AccessGuard<'_, V>>, redb::StorageError> {
        if let Some(c) = &self.capture {
            c.note::<K>(key.borrow());
        }
        self.table.insert(key, value)
    }

    pub(crate) fn remove<'a>(
        &mut self,
        key: impl Borrow<K::SelfType<'a>>,
    ) -> std::result::Result<Option<redb::AccessGuard<'_, V>>, redb::StorageError> {
        if let Some(c) = &self.capture {
            c.note::<K>(key.borrow());
        }
        self.table.remove(key)
    }

    /// 最小キーの行を取り除く。取り除いたキーを積む。
    pub(crate) fn pop_first(
        &mut self,
    ) -> std::result::Result<Popped<'_, K, V>, redb::StorageError> {
        let popped = self.table.pop_first()?;
        if let (Some(c), Some((k, _))) = (&self.capture, &popped) {
            c.note::<K>(&k.value());
        }
        Ok(popped)
    }

    /// 最大キーの行を取り除く。取り除いたキーを積む。
    pub(crate) fn pop_last(&mut self) -> std::result::Result<Popped<'_, K, V>, redb::StorageError> {
        let popped = self.table.pop_last()?;
        if let (Some(c), Some((k, _))) = (&self.capture, &popped) {
            c.note::<K>(&k.value());
        }
        Ok(popped)
    }

    /// `predicate` が `false` を返した行を取り除く。取り除いたキーだけを積む（`predicate` は
    /// panic しないこと。redb の契約）。
    pub(crate) fn retain<F>(
        &mut self,
        mut predicate: F,
    ) -> std::result::Result<(), redb::StorageError>
    where
        F: for<'f> FnMut(K::SelfType<'f>, V::SelfType<'f>) -> bool,
    {
        let Some(c) = &self.capture else {
            return self.table.retain(predicate);
        };
        // 行ごとの確保を避けるため、キーのバイト列は使い回しの緩衝へ写す。
        let mut buf: Vec<u8> = Vec::new();
        self.table.retain(|k, v| {
            buf.clear();
            buf.extend_from_slice(K::as_bytes(&k).as_ref());
            let keep = predicate(k, v);
            if !keep {
                c.frame.note_key(&c.name, &buf);
            }
            keep
        })
    }
}

impl<K: Key + 'static, V: MutInPlaceValue + 'static> CapturingTable<'_, K, V> {
    /// 値の領域を確保して返す（ゼロコピー書き込み）。値は prepare で読むため記録は正しい。
    pub(crate) fn insert_reserve<'a>(
        &mut self,
        key: impl Borrow<K::SelfType<'a>>,
        value_length: usize,
    ) -> std::result::Result<redb::AccessGuardMutInPlace<'_, V>, redb::StorageError> {
        if let Some(c) = &self.capture {
            c.note::<K>(key.borrow());
        }
        self.table.insert_reserve(key, value_length)
    }
}

/// 表を削除する。`Dropped` イベントを積んでから redb へ委譲する（`write_txn.delete_table` の直呼びを置き換える）。
pub(crate) fn delete_table(
    write_txn: &redb::WriteTransaction,
    table: impl TableHandle,
) -> std::result::Result<bool, redb::TableError> {
    if !is_excluded_table(table.name()) {
        if let Some(frame) = frame_for(write_txn) {
            frame.note_event(TableEvent::Dropped(table.name().into()));
        }
    }
    write_txn.delete_table(table)
}

/// 表の名前を付け替える。`Renamed` イベントを積んでから redb へ委譲する（`write_txn.rename_table` の直呼びを置き換える）。
pub(crate) fn rename_table(
    write_txn: &redb::WriteTransaction,
    from: impl TableHandle,
    to: impl TableHandle,
) -> std::result::Result<(), redb::TableError> {
    if !is_excluded_table(from.name()) && !is_excluded_table(to.name()) {
        if let Some(frame) = frame_for(write_txn) {
            frame.note_event(TableEvent::Renamed {
                from: from.name().into(),
                to: to.name().into(),
            });
        }
    }
    write_txn.rename_table(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("t_users");
    const OTHER: TableDefinition<&str, &[u8]> = TableDefinition::new("t_other");
    const NEWER: TableDefinition<&str, &[u8]> = TableDefinition::new("t_new");
    const RENAMED: TableDefinition<&str, &[u8]> = TableDefinition::new("t_renamed");
    const SHAPE_STR_BYTES: u32 = 1;

    fn enabled(label: &str) -> (CleanupGuard, super::super::Storage) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let mut storage = super::super::Storage::open(&path).expect("open");
        storage.enable_change_log().expect("enable");
        (guard, storage)
    }

    fn records(storage: &super::super::Storage) -> Vec<ChangeRecord> {
        let read = storage.db.begin_read().expect("read");
        let table = match read.open_table(CHANGE_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Vec::new(),
            Err(e) => panic!("open change_log: {e}"),
        };
        table
            .iter()
            .expect("iter")
            .map(|e| {
                let (_, v) = e.expect("entry");
                decode_record(v.value(), usize::MAX).expect("decode")
            })
            .collect()
    }

    fn change<'a>(rec: &'a ChangeRecord, name: &str) -> &'a TableChange {
        rec.body
            .tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no change for {name}"))
    }

    /// テストの commit 入口（`commit_boundary` の呼び出し箇所を 1 つに保つ。
    /// `table_generation_bump_coverage` の許可リストを増やさないため）。
    fn commit_result(txn: super::super::GatedWriteTxn) -> Result<()> {
        crate::recovery::commit_boundary::commit(txn)
    }

    fn do_commit(txn: super::super::GatedWriteTxn) {
        commit_result(txn).expect("commit");
    }

    fn commit_maint(txn: super::super::GatedWriteTxn) -> Result<()> {
        crate::recovery::commit_boundary::commit_maintenance(txn)
    }

    fn header() -> RecordHeader {
        RecordHeader {
            db_id: [7; DB_ID_LEN],
            timeline: 3,
            lsn: 9,
            prev_lsn: 8,
            commit_time_us: 1_700_000_000_000_000,
            storage_format: STORAGE_FORMAT_VERSION,
        }
    }

    fn body() -> RecordBody {
        RecordBody {
            dropped: vec!["gone".into()],
            created: vec![("fresh".into(), 2)],
            tables: vec![TableChange {
                name: "t".into(),
                shape: 1,
                upserts: vec![(b"k".to_vec(), b"v".to_vec())],
                removes: vec![b"x".to_vec()],
            }],
        }
    }

    // ---- レコード形式（D3）----

    #[test]
    fn record_round_trips() {
        let bytes = encode_record(&header(), &body(), usize::MAX).expect("encode");
        let rec = decode_record(&bytes, usize::MAX).expect("decode");
        assert_eq!(rec.header, header());
        assert_eq!(rec.body, body());
    }

    #[test]
    fn decode_rejects_tampering_truncation_and_unknown_versions() {
        let good = encode_record(&header(), &body(), usize::MAX).expect("encode");
        // ペイロード 1 バイトの改ざん。
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(
            decode_record(&bad, usize::MAX),
            Err(DecodeError::ChecksumMismatch)
        );
        // ヘッダの改ざん（LSN）もチェックサムで検出する。
        let mut bad = good.clone();
        bad[4 + 2 + DB_ID_LEN + 8] ^= 1;
        assert_eq!(
            decode_record(&bad, usize::MAX),
            Err(DecodeError::ChecksumMismatch)
        );
        // 途中で切れている・余りがある。
        for cut in [0, 10, HEADER_LEN, good.len() - 1] {
            assert_eq!(
                decode_record(&good[..cut], usize::MAX),
                Err(DecodeError::Truncated)
            );
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(
            decode_record(&long, usize::MAX),
            Err(DecodeError::Malformed)
        );
        // マジック・形式版数。
        let mut bad = good.clone();
        bad[0] ^= 1;
        assert_eq!(decode_record(&bad, usize::MAX), Err(DecodeError::BadMagic));
        let mut bad = good.clone();
        bad[4] = 99;
        assert_eq!(
            decode_record(&bad, usize::MAX),
            Err(DecodeError::UnsupportedVersion)
        );
        // ペイロード長の上限は確保の前に検証する。
        assert_eq!(decode_record(&good, 4), Err(DecodeError::TooLarge));
    }

    #[test]
    fn decode_rejects_forged_counts_without_allocating() {
        // 件数だけ巨大でデータが無いペイロード（チェックサムは正しく計算し直す）。
        let payload = u32::MAX.to_le_bytes().to_vec();
        let mut head = Vec::new();
        head.extend_from_slice(&RECORD_MAGIC);
        head.extend_from_slice(&RECORD_FORMAT_VERSION.to_le_bytes());
        head.extend_from_slice(&[0; DB_ID_LEN]);
        head.extend_from_slice(&[0; 8 * 4]);
        head.extend_from_slice(&STORAGE_FORMAT_VERSION.to_le_bytes());
        head.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let sum = checksum(&head, &payload);
        let mut bytes = head;
        bytes.extend_from_slice(&sum);
        bytes.extend_from_slice(&payload);
        assert_eq!(
            decode_record(&bytes, usize::MAX),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn encode_enforces_the_payload_limit() {
        assert_eq!(encode_record(&header(), &body(), 8), Err(EncodeError));
    }

    // ---- DB 識別子（D5）----

    #[test]
    fn identity_is_generated_once_and_survives_reopen_without_bumping_the_generation() {
        let path = unique_db_path("change-log-identity");
        let _guard = CleanupGuard(path.clone());
        let first = {
            let mut s = super::super::Storage::open(&path).expect("open");
            assert!(s.change_log_identity().is_none());
            let before = s.current_generation().expect("gen");
            s.enable_change_log().expect("enable");
            assert_eq!(s.current_generation().expect("gen"), before);
            s.change_log_identity().expect("identity")
        };
        assert_eq!(first.1, INITIAL_TIMELINE);
        let mut s = super::super::Storage::open(&path).expect("reopen");
        s.enable_change_log().expect("enable again");
        assert_eq!(s.change_log_identity(), Some(first));
        assert!(records(&s).is_empty(), "enabling must not emit a record");
    }

    #[test]
    fn temporary_storage_cannot_enable_the_change_log() {
        let mut s = super::super::Storage::open_with_backend(
            redb::backends::InMemoryBackend::new(),
            1 << 20,
        )
        .expect("temp storage");
        assert!(s.enable_change_log().is_err());
    }

    // ---- 捕捉とレコード生成（D1・D3）----

    #[test]
    fn commit_appends_one_record_with_after_images_in_the_same_transaction() {
        let (_g, storage) = enabled("change-log-basic");
        let (db_id, timeline) = storage.change_log_identity().expect("identity");

        // 1 回目: 2 行を挿入する。
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            assert!(t.is_capturing());
            t.insert("a", b"1".as_slice()).expect("insert");
            t.insert("b", b"2".as_slice()).expect("insert");
        }
        do_commit(txn);
        let g1 = storage.current_generation().expect("gen");

        // 2 回目: a を更新し b を消し、c を入れる。
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", b"11".as_slice()).expect("insert");
            t.remove("b").expect("remove");
            t.insert("c", b"3".as_slice()).expect("insert");
        }
        do_commit(txn);
        let g2 = storage.current_generation().expect("gen");
        assert_eq!(g2, g1 + 1);

        let recs = records(&storage);
        assert_eq!(recs.len(), 2);
        // 1 レコード = 1 commit。LSN は世代、prev は 1 つ前。
        assert_eq!(recs[0].header.lsn, g1);
        assert_eq!(recs[1].header.lsn, g2);
        assert_eq!(recs[1].header.prev_lsn, g1);
        assert_eq!(recs[1].header.db_id, db_id);
        assert_eq!(recs[1].header.timeline, timeline);
        assert!(recs[1].header.commit_time_us > 0);

        // 1 回目は表が新規なので作成 + 全行。
        assert!(recs[0]
            .body
            .created
            .contains(&("t_users".to_string(), SHAPE_STR_BYTES)));
        let c0 = change(&recs[0], "t_users");
        assert_eq!(c0.upserts.len(), 2);

        // 2 回目はキー差分だけ（作成は無い）。値は prepare 時点の最終内容。
        assert!(recs[1].body.created.is_empty());
        assert!(recs[1].body.dropped.is_empty());
        let c1 = change(&recs[1], "t_users");
        assert_eq!(
            c1.upserts,
            vec![
                (b"a".to_vec(), b"11".to_vec()),
                (b"c".to_vec(), b"3".to_vec())
            ]
        );
        assert_eq!(c1.removes, vec![b"b".to_vec()]);
        // 世代カウンタの after-image が必ず入る（次の prev 検査の材料）。
        let gen = change(&recs[1], GENERATION_TABLE.name());
        assert_eq!(
            gen.upserts,
            vec![(
                GENERATION_KEY.as_bytes().to_vec(),
                g2.to_le_bytes().to_vec()
            )]
        );
        // 本体表とメタ表は記録対象外。
        for r in &recs {
            assert!(r.body.tables.iter().all(|t| !is_excluded_table(&t.name)));
            assert!(r.body.created.iter().all(|(n, _)| !is_excluded_table(n)));
        }
    }

    #[test]
    fn rolled_back_and_rewritten_keys_record_the_final_content() {
        let (_g, storage) = enabled("change-log-final-content");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("k", b"1".as_slice()).expect("insert");
        }
        do_commit(txn);
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            // 書いてから戻す・書いてから消す（キー集合は上位集合。最終内容だけが記録される）。
            t.insert("k", b"2".as_slice()).expect("insert");
            t.insert("k", b"1".as_slice()).expect("insert");
            t.insert("tmp", b"x".as_slice()).expect("insert");
            t.remove("tmp").expect("remove");
        }
        do_commit(txn);
        let recs = records(&storage);
        let c = change(&recs[1], "t_users");
        assert_eq!(c.upserts, vec![(b"k".to_vec(), b"1".to_vec())]);
        assert_eq!(c.removes, vec![b"tmp".to_vec()]);
    }

    #[test]
    fn retain_pop_and_reserve_capture_their_keys() {
        let (_g, storage) = enabled("change-log-retain");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            for k in ["a", "b", "c", "d"] {
                t.insert(k, b"v".as_slice()).expect("insert");
            }
        }
        do_commit(txn);
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.retain(|k, _| k != "b").expect("retain");
            assert!(t.pop_first().expect("pop").is_some());
            assert!(t.pop_last().expect("pop").is_some());
            // ゼロコピー書き込み（値は prepare で読む）。
            let mut g = t.insert_reserve("z", 2).expect("reserve");
            g.as_mut().copy_from_slice(b"zz");
        }
        do_commit(txn);
        let recs = records(&storage);
        let c = change(&recs[1], "t_users");
        assert_eq!(c.removes, vec![b"a".to_vec(), b"b".to_vec(), b"d".to_vec()]);
        assert_eq!(c.upserts, vec![(b"z".to_vec(), b"zz".to_vec())]);
    }

    #[test]
    fn table_lifecycle_is_normalized_to_created_and_dropped() {
        let (_g, storage) = enabled("change-log-lifecycle");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", b"1".as_slice()).expect("insert");
            let mut o = CapturingTable::open(&txn, OTHER).expect("open");
            o.insert("x", b"1".as_slice()).expect("insert");
        }
        do_commit(txn);

        let txn = storage.begin_write_txn().expect("txn");
        {
            // 作成して消した表は記録に現れない。
            let mut n = CapturingTable::open(&txn, NEWER).expect("open");
            n.insert("tmp", b"1".as_slice()).expect("insert");
        }
        assert!(delete_table(&txn, NEWER).expect("delete"));
        // 既存の表を削除する。
        assert!(delete_table(&txn, OTHER).expect("delete"));
        // 既存の表を rename し、改名後の表を更新してから元の名前へ戻す（A→B→A）。
        rename_table(&txn, USERS, RENAMED).expect("rename");
        {
            let mut r = CapturingTable::open(&txn, RENAMED).expect("open");
            r.insert("b", b"2".as_slice()).expect("insert");
        }
        rename_table(&txn, RENAMED, USERS).expect("rename back");
        do_commit(txn);

        let rec = records(&storage).pop().expect("record");
        assert!(rec.body.dropped.contains(&"t_other".to_string()));
        // A→B→A は全行の after-image（基底に同名があるので dropped と created を併載）。
        assert!(rec.body.dropped.contains(&"t_users".to_string()));
        assert!(rec.body.created.iter().any(|(n, _)| n == "t_users"));
        let users = change(&rec, "t_users");
        assert_eq!(
            users.upserts,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec())
            ]
        );
        assert!(rec.body.tables.iter().all(|t| t.name != "t_new"));
        assert!(!rec.body.dropped.contains(&"t_new".to_string()));
        assert!(rec.body.created.iter().all(|(n, _)| n != "t_new"));
    }

    #[test]
    fn rename_to_a_new_name_becomes_drop_plus_full_create() {
        let (_g, storage) = enabled("change-log-rename");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", b"1".as_slice()).expect("insert");
        }
        do_commit(txn);
        let txn = storage.begin_write_txn().expect("txn");
        rename_table(&txn, USERS, RENAMED).expect("rename");
        do_commit(txn);
        let rec = records(&storage).pop().expect("record");
        assert_eq!(rec.body.dropped, vec!["t_users".to_string()]);
        assert_eq!(
            rec.body.created,
            vec![("t_renamed".to_string(), SHAPE_STR_BYTES)]
        );
        assert_eq!(
            change(&rec, "t_renamed").upserts,
            vec![(b"a".to_vec(), b"1".to_vec())]
        );
    }

    // ---- コミットの原子性・fail-closed ----

    #[test]
    fn rejected_prepare_leaves_no_record_no_data_and_no_generation_change() {
        let path = unique_db_path("change-log-atomic");
        let _guard = CleanupGuard(path.clone());
        let mut storage = super::super::Storage::open(&path).expect("open");
        storage
            .enable_change_log_with(CaptureLimits {
                max_keys: 2,
                ..CaptureLimits::default()
            })
            .expect("enable");
        let before = storage.current_generation().expect("gen");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            for k in ["a", "b", "c"] {
                t.insert(k, b"v".as_slice()).expect("insert");
            }
        }
        // frame の上限超過は commit 前に拒否される（durable な書き込みは無い）。
        let err = commit_result(txn).expect_err("must reject");
        assert!(matches!(err, StorageError::CommitFootprintUnrecordable));
        assert_eq!(storage.current_generation().expect("gen"), before);
        assert!(records(&storage).is_empty());
        let read = storage.db.begin_read().expect("read");
        assert!(matches!(
            read.open_table(USERS),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
        // 拒否の後も次の commit は成功し、LSN は連続する。
        drop(read);
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", b"v".as_slice()).expect("insert");
        }
        do_commit(txn);
        let recs = records(&storage);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].header.lsn, before + 1);
    }

    #[test]
    fn aborted_transaction_leaves_no_record() {
        let (_g, storage) = enabled("change-log-abort");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", b"v".as_slice()).expect("insert");
        }
        txn.abort().expect("abort");
        assert!(records(&storage).is_empty());
    }

    #[test]
    fn bump_generation_and_commit_path_also_appends_the_record() {
        let (_g, storage) = enabled("change-log-bump-path");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", b"v".as_slice()).expect("insert");
        }
        super::super::bump_generation_and_commit(txn).expect("commit");
        let recs = records(&storage);
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].header.lsn,
            storage.current_generation().expect("gen")
        );
        assert_eq!(change(&recs[0], "t_users").upserts.len(), 1);
    }

    #[test]
    fn maintenance_commit_does_not_advance_the_generation_or_emit_a_record() {
        let (_g, storage) = enabled("change-log-maintenance");
        let before = storage.current_generation().expect("gen");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut meta = txn.open_table(META_TABLE).expect("meta");
            meta.insert("probe", b"1".as_slice()).expect("insert");
        }
        commit_maint(txn).expect("commit");
        assert_eq!(storage.current_generation().expect("gen"), before);
        assert!(records(&storage).is_empty());
    }

    #[test]
    fn oversized_after_images_are_rejected_before_commit() {
        let path = unique_db_path("change-log-payload-cap");
        let _guard = CleanupGuard(path.clone());
        let mut storage = super::super::Storage::open(&path).expect("open");
        storage
            .enable_change_log_with(CaptureLimits {
                max_payload_bytes: 64,
                ..CaptureLimits::default()
            })
            .expect("enable");
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            t.insert("a", vec![0u8; 256].as_slice()).expect("insert");
        }
        assert!(matches!(
            commit_result(txn),
            Err(StorageError::CommitFootprintUnrecordable)
        ));
        assert!(records(&storage).is_empty());
    }

    // ---- 無効時 ----

    #[test]
    fn disabled_storage_allocates_no_frame_and_writes_no_log_table() {
        let path = unique_db_path("change-log-disabled");
        let _guard = CleanupGuard(path.clone());
        let storage = super::super::Storage::open(&path).expect("open");
        assert!(storage.change_log_identity().is_none());
        let txn = storage.begin_write_txn().expect("txn");
        assert!(txn.capture.is_none());
        {
            let mut t = CapturingTable::open(&txn, USERS).expect("open");
            assert!(!t.is_capturing());
            t.insert("a", b"v".as_slice()).expect("insert");
        }
        do_commit(txn);
        // 変更ログの表（本体・メタ）は作られない。
        let read = storage.db.begin_read().expect("read");
        let names: Vec<String> = read
            .list_tables()
            .expect("list")
            .map(|h| h.name().to_string())
            .collect();
        assert!(names.iter().all(|n| !is_excluded_table(n)), "{names:?}");
    }

    // ---- SQL・カタログからの名前解決 ----

    #[test]
    fn change_log_tables_are_not_resolvable_from_sql_or_the_catalog() {
        use crate::core::EngineCore;
        use crate::policy::PolicyContext;
        use crate::sql::mode::SessionState;
        use crate::storage::Visibility;

        let path = unique_db_path("change-log-name-resolution");
        let _guard = CleanupGuard(path.clone());
        let mut storage = super::super::Storage::open(&path).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider));
        let ctx =
            PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
                .expect("ctx");

        // 本体表・メタ表は SQL の relation として解決されない（未定義の表として拒否される）。
        for name in [CHANGE_LOG_TABLE_NAME, CHANGE_LOG_META_TABLE_NAME] {
            for sql in [
                format!("SELECT * FROM {name} LIMIT 1"),
                format!("SELECT * FROM public.{name} LIMIT 1"),
            ] {
                let err = core
                    .execute_sql(&ctx, &sql)
                    .expect_err("must not resolve a change log table");
                assert_eq!(err.wire_code(), "42P01", "{sql}");
            }
        }
        // カタログの一覧にも現れない。
        let listed = core.storage_for_test().list_tables().expect("list");
        assert!(listed.iter().all(|n| !is_excluded_table(n)), "{listed:?}");
        let _ = SessionState::default();
    }

    #[test]
    fn user_tables_named_like_the_log_tables_do_not_collide_with_them() {
        use crate::core::EngineCore;
        use crate::policy::PolicyContext;
        use crate::sql::mode::SessionState;
        use crate::storage::Visibility;

        let path = unique_db_path("change-log-name-collision");
        let _guard = CleanupGuard(path.clone());
        let mut storage = super::super::Storage::open(&path).expect("open");
        storage.enable_change_log().expect("enable");
        let core = EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider));
        let ctx =
            PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
                .expect("ctx");
        let mut session = SessionState::default();
        session.allow_ddl();
        for name in [CHANGE_LOG_TABLE_NAME, CHANGE_LOG_META_TABLE_NAME] {
            core.execute_sql_in_session(
                &ctx,
                &mut session,
                &format!(
                    "CREATE TABLE {name} (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL)"
                ),
            )
            .expect("a user table may use the name");
        }
        for (i, name) in [CHANGE_LOG_TABLE_NAME, CHANGE_LOG_META_TABLE_NAME]
            .iter()
            .enumerate()
        {
            core.execute_sql_in_session(
                &ctx,
                &mut session,
                &format!(
                    "INSERT INTO {name} (id, embedding) VALUES (1, '[0.1,0.2]') \
                     USING OPERATION_ID 'op-collision-{i}'"
                ),
            )
            .expect("insert into the user table");
        }
        // 利用者の表は redb 上では別名（接頭辞つき）で、本体表の内容を汚さない。
        let recs = records(core.storage_for_test());
        assert!(!recs.is_empty());
        for r in &recs {
            for (n, _) in &r.body.created {
                assert!(!is_excluded_table(n), "{n}");
            }
            for t in &r.body.tables {
                assert!(!is_excluded_table(&t.name), "{}", t.name);
            }
        }
        let user_physical: Vec<String> = recs
            .iter()
            .flat_map(|r| r.body.created.iter().map(|(n, _)| n.clone()))
            .filter(|n| n.contains(CHANGE_LOG_TABLE_NAME) || n.contains(CHANGE_LOG_META_TABLE_NAME))
            .collect();
        assert!(
            !user_physical.is_empty(),
            "the user tables must be recorded under their own names"
        );
    }
}
