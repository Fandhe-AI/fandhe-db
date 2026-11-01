//! 明示トランザクションの書き込み時直列化検査（SQL-38・TASK-333・Issue #2404）が使う
//! 「commit 記録」。`Storage` が持つインメモリの有界な記録で、redb へは永続化しない
//! （設計: `docs/design/explicit-transaction.md` 1a-5・1a-7）。
//!
//! 役割と呼び出し関係:
//!
//! - [`CommitLog`] は commit ごとに「書き込んだ表と、表ごとの行の足跡（書き換えた行の前後の
//!   `(tenant_id, visibility)` の組）・DDL フラグ」を世代 `G_c` 順に保持する。追記は
//!   `recovery::commit_boundary::commit_and_finish`（と `storage::bump_generation_and_commit`）が、
//!   redb の commit 成功後・`WriterPermit` 解放前に [`CommitSession::finish`] 経由で行う
//!   （1a-5 の順序不変条件。writer を取得したセッションから見て、取得時点の世代までの
//!   commit がすべて記録済みになる）。
//! - 保持範囲は「生存中で writer 未取得の明示トランザクション」が [`SnapshotRegistration`] で
//!   登録した `G_s` の最小値より新しい記録だけ。登録が無ければ何も保持しない。件数と足跡の
//!   要素数の両方に上限を持ち、超えたら最古から捨てて「床」を上げる（1a-7）。
//! - 行の足跡は書き込み点（`tenant.rs`・`catalog.rs` ほか）が [`note_row`] などで write
//!   トランザクション単位の frame へ積む。frame は `GatedWriteTxn` の寿命に結び付く。
//!   書き込まれた表そのものは表世代の差分（`catalog::changed_table_generations_in_write_txn`）
//!   で確定するため、frame に足跡が無い書き込み表は commit 前に拒否する（fail-closed。
//!   書き込み点の結線漏れは取りこぼしではなく拒否として検出される）。
//! - 照会 API [`CommitLog::records_between`] は #2405 の `40001` 判定が呼ぶ。
//!
//! テナント境界（P0）: 足跡は書き込んだ行の実際の `(tenant_id, visibility)` を持つ。
//! 全テナント共通で見える `Public` 行だけは、テナントを区別しない 1 要素へ畳む
//! （`PolicyContext::is_visible` は `Public` 行のテナントを見ないため判定は変わらない）。
//! `Private` 行は畳まない。記録できない書き込みは commit 前に拒否し、不可視の組を可視扱いで
//! 記録することはしない。エラー・ログへ表名・テナント ID・世代・件数を含めない。
//!
//! 数値の上限は実装既定値（spec 由来の値ではない）。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use redb::ReadableDatabase;

use super::{Result, StorageError, Visibility};

/// 保持する commit 記録の件数上限。
pub(crate) const MAX_COMMIT_RECORDS: usize = 16_384;
/// 保持する全記録の足跡要素数の合計上限。
pub(crate) const MAX_FOOTPRINT_ELEMENTS: usize = 262_144;
/// 1 commit が持てる足跡要素数の上限。超える commit は redb の commit 前に拒否する。
/// RLS 下の書き込みは所有者の行に限られるため、通常の 1 commit が持つのは表ごとに
/// 「自テナントの Private」と「Public」の 2 要素までで、この上限は複数テナントにまたがる
/// 管理系経路のためのもの（通常の負荷では到達しない）。
pub(crate) const MAX_FOOTPRINT_ELEMENTS_PER_COMMIT: usize = 1024;

/// 上限の組。本番は定数、テストは小さい値を注入できる。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) max_records: usize,
    pub(crate) max_elements: usize,
    pub(crate) max_per_commit: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_records: MAX_COMMIT_RECORDS,
            max_elements: MAX_FOOTPRINT_ELEMENTS,
            max_per_commit: MAX_FOOTPRINT_ELEMENTS_PER_COMMIT,
        }
    }
}

/// 1 つの表についての足跡。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TableFootprint {
    /// 全セッションから可視な種別（`Public`）の行を書き換えたか。テナントは区別しない。
    pub(crate) public: bool,
    /// `Private` 行を書き換えたテナントの集合（畳まない）。
    pub(crate) private_tenants: BTreeSet<Box<str>>,
    /// 表のスキーマやカタログを変えたか。
    pub(crate) ddl: bool,
}

impl TableFootprint {
    /// 上限判定に使う要素数（表自身を 1 と数える）。
    fn elements(&self) -> usize {
        1usize
            .saturating_add(usize::from(self.public))
            .saturating_add(self.private_tenants.len())
    }

    /// 行の足跡が 1 つも無く DDL でもないか（0 行更新で表だけを触った場合）。
    fn is_empty(&self) -> bool {
        !self.public && self.private_tenants.is_empty() && !self.ddl
    }
}

/// 1 commit 分の記録。`generation` は commit が書き込んだ世代 `G_c`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitRecord {
    pub(crate) generation: u64,
    /// 書き込んだ表（小文字化した名前）と足跡。
    pub(crate) tables: Vec<(Box<str>, TableFootprint)>,
}

impl CommitRecord {
    fn elements(&self) -> usize {
        self.tables
            .iter()
            .fold(0usize, |acc, (_, fp)| acc.saturating_add(fp.elements()))
    }
}

/// 記録から判定できない（床より古い・記録の欠け・mutex の汚染）ことを表す。
/// 呼び出し側（#2405）は衝突として扱う（fail-closed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Unverifiable;

#[derive(Default)]
struct State {
    next_id: u64,
    /// 登録ハンドル ID → `G_s`。
    registrations: HashMap<u64, u64>,
    records: VecDeque<CommitRecord>,
    elements: usize,
    /// 捨てた記録の最大世代。`G_s` がこれより古い登録は判定できない。
    floor: u64,
    /// 最後に記録した（または欠けとして扱った）commit の世代。
    last_generation: u64,
    /// mutex の汚染を検知したか。
    tainted: bool,
}

/// `Storage` ごとの commit 記録と、生存中スナップショットの登録簿。
pub(crate) struct CommitLog {
    state: Mutex<State>,
    limits: Limits,
    /// `Storage::open` の最後で立てる。open 中の移行 commit は記録対象外。
    armed: AtomicBool,
    /// 登録の数（ロック無しで記録要否を判定するための写し。更新はロック下）。
    live: AtomicUsize,
}

impl CommitLog {
    pub(crate) fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub(crate) fn with_limits(limits: Limits) -> Self {
        Self {
            state: Mutex::new(State::default()),
            limits,
            armed: AtomicBool::new(false),
            live: AtomicUsize::new(0),
        }
    }

    /// ロックを取る。汚染されていても中身を回収し、`tainted` を立てる（post-commit 区間や
    /// Drop から呼ぶため panic させない）。
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                let mut g = poisoned.into_inner();
                g.tainted = true;
                g
            }
        }
    }

    /// 記録を始める。`current_generation` は open 時点の確定世代。
    pub(crate) fn arm(&self, current_generation: u64) {
        let mut st = self.lock();
        st.last_generation = current_generation;
        st.floor = st.floor.max(current_generation);
        drop(st);
        self.armed.store(true, Ordering::SeqCst);
    }

    /// 記録を始めているか（`Storage::open` の最後で arm 済みか）。write トランザクションの開始時に
    /// 開始時点の確定状態（`base`）を開くかの判定に使う。登録の有無によらず開くのは、write
    /// トランザクションの開始後に登録が現れても、その commit の記録を取れるようにするため
    /// （取れない窓を「開始から commit まで」ではなく「準備段階から追記まで」へ縮める）。
    pub(crate) fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    /// 今この commit の記録を計算する必要があるか。登録が無ければ不要。
    /// テストビルドでは、全既存テストが結線漏れ検査を通るよう常に計算する。
    pub(crate) fn should_record(&self) -> bool {
        if !self.armed.load(Ordering::SeqCst) {
            return false;
        }
        cfg!(test) || self.live.load(Ordering::SeqCst) > 0
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits
    }

    /// 確定済み読み取りスナップショットを開いて登録する（1a-7 の登録順序）。
    /// ロックを取り、保持したまま `ReadTransaction` を開いて `G_s` を読み、登録してから
    /// ロックを放す。刈り込みも同じロックの下で行うため、取得と登録の間に `G_s` より
    /// 新しい記録が刈り込まれることはない。
    pub(crate) fn open_registered_snapshot(
        self: &Arc<Self>,
        db: &redb::Database,
    ) -> Result<(redb::ReadTransaction, SnapshotRegistration)> {
        let mut st = self.lock();
        let read_txn = db.begin_read()?;
        let g_s = super::current_generation_in_txn(&read_txn)?;
        let id = st.next_id;
        st.next_id = st.next_id.saturating_add(1);
        st.registrations.insert(id, g_s);
        self.live.store(st.registrations.len(), Ordering::SeqCst);
        drop(st);
        Ok((
            read_txn,
            SnapshotRegistration {
                log: Arc::clone(self),
                id,
                g_s,
            },
        ))
    }

    fn unregister(&self, id: u64) {
        let mut st = self.lock();
        st.registrations.remove(&id);
        self.live.store(st.registrations.len(), Ordering::SeqCst);
        self.prune_locked(&mut st);
    }

    /// 刈り込みの規則（1 本）。捨てた記録の最大世代を必ず床へ反映する。
    fn prune_locked(&self, st: &mut State) {
        if st.registrations.is_empty() {
            st.records.clear();
            st.elements = 0;
            return;
        }
        let min_gs = st.registrations.values().copied().min().unwrap_or(0);
        while st.records.front().is_some_and(|r| r.generation <= min_gs) {
            Self::pop_front_locked(st);
        }
        while st.records.len() > self.limits.max_records || st.elements > self.limits.max_elements {
            if st.records.is_empty() {
                break;
            }
            Self::pop_front_locked(st);
        }
    }

    fn pop_front_locked(st: &mut State) {
        if let Some(r) = st.records.pop_front() {
            st.elements = st.elements.saturating_sub(r.elements());
            st.floor = st.floor.max(r.generation);
        }
    }

    /// commit 成功後・permit 解放前に呼ぶ追記（panic・失敗させない）。
    ///
    /// `record` が `None` は「この commit の記録を計算しなかった」。計算しなかった commit の
    /// 後に登録が存在するなら、その記録は無いので床を `g_c` まで上げる（fail-closed）。
    pub(crate) fn finish_commit(&self, g_c: u64, record: Option<CommitRecord>) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let mut st = self.lock();
        if g_c != st.last_generation.saturating_add(1) {
            // 世代の欠け（記録を経ない commit があった）。それ以前は判定できない。
            st.floor = st.floor.max(g_c.saturating_sub(1));
        }
        st.last_generation = g_c;
        if st.registrations.is_empty() {
            st.records.clear();
            st.elements = 0;
            return;
        }
        match record {
            Some(mut r) => {
                r.generation = g_c;
                if !r.tables.is_empty() {
                    st.elements = st.elements.saturating_add(r.elements());
                    st.records.push_back(r);
                }
            }
            None => st.floor = st.floor.max(g_c),
        }
        self.prune_locked(&mut st);
    }

    /// `g_s < G_c <= g_now` の記録を返す。床より古い・記録の欠け・汚染のときは
    /// [`Unverifiable`]（#2405 が衝突として扱う）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn records_between(
        &self,
        g_s: u64,
        g_now: u64,
    ) -> std::result::Result<Vec<CommitRecord>, Unverifiable> {
        let st = self.lock();
        if st.tainted || g_s < st.floor || st.last_generation < g_now {
            return Err(Unverifiable);
        }
        Ok(st
            .records
            .iter()
            .filter(|r| r.generation > g_s && r.generation <= g_now)
            .cloned()
            .collect())
    }

    #[cfg(test)]
    pub(crate) fn snapshot_stats(&self) -> (usize, usize, u64) {
        let st = self.lock();
        (st.records.len(), st.elements, st.floor)
    }

    #[cfg(test)]
    pub(crate) fn last_generation(&self) -> u64 {
        self.lock().last_generation
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(self: &Arc<Self>) {
        let log = Arc::clone(self);
        let _ = std::thread::spawn(move || {
            let _g = log.state.lock().unwrap();
            panic!("poison");
        })
        .join();
    }
}

/// 生存中の明示トランザクションの `G_s` の登録（RAII。Drop で外して刈り込む）。
pub(crate) struct SnapshotRegistration {
    log: Arc<CommitLog>,
    id: u64,
    g_s: u64,
}

impl SnapshotRegistration {
    /// 登録時に読んだ読み取りスナップショットの世代 `G_s`。
    pub(crate) fn snapshot_generation(&self) -> u64 {
        self.g_s
    }
}

impl Drop for SnapshotRegistration {
    fn drop(&mut self) {
        self.log.unregister(self.id);
    }
}

// ---------------------------------------------------------------------------
// 書き込みトランザクション単位の frame（行の足跡の収集）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FrameState {
    tables: HashMap<Box<str>, TableFootprint>,
    elements: usize,
    /// 1 commit の上限を超えた。
    overflow: bool,
    /// 足跡を取れない書き込み（ヘッダを読めない行など）があった。
    unrecordable: bool,
}

pub(crate) struct Frame {
    state: Mutex<FrameState>,
    max_per_commit: usize,
}

impl Frame {
    fn lock(&self) -> MutexGuard<'_, FrameState> {
        match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                let mut g = poisoned.into_inner();
                g.unrecordable = true;
                g
            }
        }
    }

    fn entry_mut<'a>(
        st: &'a mut FrameState,
        table: &str,
    ) -> Option<(&'a mut TableFootprint, &'a mut usize)> {
        if !st.tables.contains_key(table) {
            st.tables.insert(table.into(), TableFootprint::default());
            st.elements = st.elements.saturating_add(1);
        }
        let FrameState {
            tables, elements, ..
        } = st;
        tables.get_mut(table).map(|t| (t, elements))
    }

    fn touch(&self, table: &str) {
        let table = lower(table);
        let mut st = self.lock();
        if st.overflow {
            return;
        }
        let _ = Self::entry_mut(&mut st, &table);
        self.check_overflow(&mut st);
    }

    fn add_row(&self, table: &str, tenant: &str, visibility: Visibility) {
        let table = lower(table);
        let mut st = self.lock();
        if st.overflow {
            return;
        }
        if let Some((fp, elements)) = Self::entry_mut(&mut st, &table) {
            match visibility {
                Visibility::Public => {
                    if !fp.public {
                        fp.public = true;
                        *elements = elements.saturating_add(1);
                    }
                }
                Visibility::Private => {
                    if !fp.private_tenants.contains(tenant) {
                        fp.private_tenants.insert(tenant.into());
                        *elements = elements.saturating_add(1);
                    }
                }
            }
        }
        self.check_overflow(&mut st);
    }

    fn add_ddl(&self, table: &str) {
        let table = lower(table);
        let mut st = self.lock();
        if st.overflow {
            return;
        }
        if let Some((fp, _)) = Self::entry_mut(&mut st, &table) {
            fp.ddl = true;
        }
        self.check_overflow(&mut st);
    }

    fn mark_unrecordable(&self) {
        self.lock().unrecordable = true;
    }

    fn check_overflow(&self, st: &mut FrameState) {
        if st.elements > self.max_per_commit {
            st.overflow = true;
            // 以降は積まない。メモリを無制限に使わないよう既に積んだ分も捨てる。
            st.tables.clear();
        }
    }
}

/// 表名の比較は ASCII の大文字小文字を区別しない（書き込み点の綴りの揺れで結線漏れ扱いに
/// ならないようにする）。
fn lower(name: &str) -> std::borrow::Cow<'_, str> {
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(name.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// 進行中の write トランザクション（`Box` 内のアドレスをキーにする）→ frame。
/// 書き込み点は `&redb::WriteTransaction` しか持たないため、アドレスで frame を引く。
/// キーは `usize` として使うだけで逆参照しない。プロセス共有にするのは、明示トランザクションの
/// writer が文ごとに別スレッドから使われても frame を見失わないため。
static FRAMES: LazyLock<Mutex<HashMap<usize, Arc<Frame>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn frames_lock() -> MutexGuard<'static, HashMap<usize, Arc<Frame>>> {
    FRAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// frame の登録の RAII。Drop で登録簿から外す。
pub(crate) struct FootprintHandle {
    frame: Arc<Frame>,
    addr: usize,
}

impl Drop for FootprintHandle {
    fn drop(&mut self) {
        frames_lock().remove(&self.addr);
    }
}

impl FootprintHandle {
    /// `write_txn`（`Box` に入れて寿命中アドレスが動かないもの）の frame を登録する。
    pub(crate) fn register(write_txn: &redb::WriteTransaction, max_per_commit: usize) -> Self {
        let addr = addr_of(write_txn);
        let frame = Arc::new(Frame {
            state: Mutex::new(FrameState::default()),
            max_per_commit,
        });
        frames_lock().insert(addr, Arc::clone(&frame));
        Self { frame, addr }
    }
}

fn addr_of(write_txn: &redb::WriteTransaction) -> usize {
    std::ptr::from_ref(write_txn) as usize
}

fn frame_for(write_txn: &redb::WriteTransaction) -> Option<Arc<Frame>> {
    frames_lock().get(&addr_of(write_txn)).cloned()
}

/// 行を 1 件書き換える（挿入・更新の前後・削除）ことを足跡へ積む。frame が無い write
/// トランザクション（一時表・テスト fixture）では何もしない。
pub(crate) fn note_row(
    write_txn: &redb::WriteTransaction,
    table: &str,
    tenant: &str,
    visibility: Visibility,
) {
    if let Some(frame) = frame_for(write_txn) {
        frame.add_row(table, tenant, visibility);
    }
}

/// 行バイト列のヘッダから `(tenant_id, visibility)` を読んで足跡へ積む。ヘッダを読めない
/// ときは足跡を取れない書き込みとして記録し、commit 前に拒否させる（fail-closed）。
pub(crate) fn note_row_bytes(write_txn: &redb::WriteTransaction, table: &str, row_bytes: &[u8]) {
    let Some(frame) = frame_for(write_txn) else {
        return;
    };
    match super::decode_row_tenant_and_visibility(row_bytes) {
        Ok((tenant, visibility)) => frame.add_row(table, tenant, visibility),
        Err(_) => frame.mark_unrecordable(),
    }
}

/// 表の行を書き換えうる文が実行されたが、書き換えた行が 0 件でも commit が表世代を進める経路
/// （0 行更新など）のために、足跡が空のまま表を記録する。
pub(crate) fn note_table(write_txn: &redb::WriteTransaction, table: &str) {
    if let Some(frame) = frame_for(write_txn) {
        frame.touch(table);
    }
}

/// 表のスキーマやカタログを変える書き込み（DDL）を記録する。
pub(crate) fn note_ddl(write_txn: &redb::WriteTransaction, table: &str) {
    if let Some(frame) = frame_for(write_txn) {
        frame.add_ddl(table);
    }
}

// ---------------------------------------------------------------------------
// commit 境界との結線
// ---------------------------------------------------------------------------

/// `GatedWriteTxn` が持つ記録の付帯物を commit 境界へ運ぶ束。
pub(crate) struct CommitSession {
    log: Arc<CommitLog>,
    handle: FootprintHandle,
    /// write トランザクション開始時点の確定状態（記録が要る場合だけ開く）。
    base: Option<redb::ReadTransaction>,
}

impl CommitSession {
    pub(crate) fn new(
        log: Arc<CommitLog>,
        handle: FootprintHandle,
        base: Option<redb::ReadTransaction>,
    ) -> Self {
        Self { log, handle, base }
    }

    /// commit 前の検査と記録の組み立て（`prepare_generation_bump` の後に呼ぶ）。
    /// `Err` は commit 前の拒否で、durable な書き込みは発生していない。
    /// `Ok(None)` は記録を計算しなかった（開始時に記録が要らなかった）こと。
    pub(crate) fn prepare(
        &self,
        write_txn: &redb::WriteTransaction,
    ) -> Result<Option<CommitRecord>> {
        let Some(base) = self.base.as_ref() else {
            return Ok(None);
        };
        // 登録が無いなら表世代の差分（表数に比例）も取らず記録も計算しない。準備段階の後に登録が
        // 現れた場合は、追記側（`finish_commit`）が `None` を「記録なし」として床を上げる。
        if !self.log.should_record() {
            return Ok(None);
        }
        let written = crate::catalog::changed_table_generations_in_write_txn(write_txn, base)
            .map_err(|_| StorageError::CommitFootprintUnrecordable)?;
        let st = self.handle.frame.lock();
        if st.overflow || st.unrecordable {
            return Err(StorageError::CommitFootprintUnrecordable);
        }
        let mut tables = Vec::with_capacity(written.len().min(self.log.limits.max_per_commit));
        for name in &written {
            let key = lower(name);
            // 足跡も DDL フラグも積まれていない書き込み表は結線漏れ。取りこぼすより拒否する。
            let Some(fp) = st.tables.get(key.as_ref()) else {
                return Err(StorageError::CommitFootprintUnrecordable);
            };
            if fp.is_empty() {
                continue;
            }
            tables.push((Box::<str>::from(key.as_ref()), fp.clone()));
        }
        Ok(Some(CommitRecord {
            generation: 0,
            tables,
        }))
    }

    /// commit 成功後・permit 解放前の追記。
    pub(crate) fn finish(&self, g_c: u64, record: Option<CommitRecord>) {
        self.log.finish_commit(g_c, record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(g: u64, tenant: &str) -> CommitRecord {
        let mut fp = TableFootprint::default();
        fp.private_tenants.insert(tenant.into());
        CommitRecord {
            generation: g,
            tables: vec![("t".into(), fp)],
        }
    }

    /// テスト用: DB を開かずに `G_s` を直接登録する。
    fn register(log: &Arc<CommitLog>, g_s: u64) -> SnapshotRegistration {
        let mut st = log.lock();
        let id = st.next_id;
        st.next_id += 1;
        st.registrations.insert(id, g_s);
        log.live.store(st.registrations.len(), Ordering::SeqCst);
        drop(st);
        SnapshotRegistration {
            log: Arc::clone(log),
            id,
            g_s,
        }
    }

    fn armed(limits: Limits, g: u64) -> Arc<CommitLog> {
        let log = Arc::new(CommitLog::with_limits(limits));
        log.arm(g);
        log
    }

    #[test]
    fn nothing_is_retained_without_registrations() {
        let log = armed(Limits::default(), 0);
        for g in 1..=100 {
            log.finish_commit(g, Some(rec(g, "a")));
        }
        assert_eq!(log.snapshot_stats().0, 0);
        assert_eq!(log.last_generation(), 100);
    }

    #[test]
    fn records_are_kept_for_a_live_registration_and_dropped_with_it() {
        let log = armed(Limits::default(), 5);
        let reg = register(&log, 5);
        for g in 6..=10 {
            log.finish_commit(g, Some(rec(g, "a")));
        }
        let got = log.records_between(5, 10).unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got.first().unwrap().generation, 6);
        // G_s 以下の記録は返らない。
        assert_eq!(log.records_between(7, 10).unwrap().len(), 3);
        drop(reg);
        assert_eq!(log.snapshot_stats().0, 0);
    }

    #[test]
    fn records_at_or_below_the_minimum_snapshot_are_pruned() {
        let log = armed(Limits::default(), 0);
        let _old = register(&log, 0);
        for g in 1..=4 {
            log.finish_commit(g, Some(rec(g, "a")));
        }
        let young = register(&log, 4);
        let old_id = 0;
        // 最古の登録を外すと G_c <= 4 の記録が捨てられる。
        {
            let mut st = log.lock();
            st.registrations.remove(&old_id);
        }
        log.finish_commit(5, Some(rec(5, "a")));
        assert_eq!(log.snapshot_stats().0, 1);
        assert_eq!(log.records_between(4, 5).unwrap().len(), 1);
        drop(young);
    }

    #[test]
    fn record_count_cap_evicts_oldest_and_raises_the_floor_monotonically() {
        let limits = Limits {
            max_records: 3,
            max_elements: usize::MAX,
            max_per_commit: 1024,
        };
        let log = armed(limits, 0);
        let _reg = register(&log, 0);
        let mut last_floor = 0;
        for g in 1..=20 {
            log.finish_commit(g, Some(rec(g, "a")));
            let (n, _, floor) = log.snapshot_stats();
            assert!(n <= 3, "retained {n}");
            assert!(floor >= last_floor);
            last_floor = floor;
        }
        assert_eq!(log.snapshot_stats().2, 17);
        // 床より古い G_s は判定できない。
        assert_eq!(log.records_between(0, 20), Err(Unverifiable));
        assert!(log.records_between(17, 20).is_ok());
    }

    #[test]
    fn element_cap_bounds_total_footprint_size() {
        let limits = Limits {
            max_records: usize::MAX,
            max_elements: 10,
            max_per_commit: 1024,
        };
        let log = armed(limits, 0);
        let _reg = register(&log, 0);
        for g in 1..=50 {
            log.finish_commit(g, Some(rec(g, "a")));
            assert!(log.snapshot_stats().1 <= 10);
        }
        // 1 記録 = 表 1 + Private 1 = 2 要素。
        assert_eq!(log.snapshot_stats().0, 5);
    }

    #[test]
    fn generation_gap_raises_the_floor() {
        let log = armed(Limits::default(), 0);
        let _reg = register(&log, 0);
        log.finish_commit(1, Some(rec(1, "a")));
        // 世代 2 が記録を経ずに commit された。
        log.finish_commit(3, Some(rec(3, "a")));
        assert_eq!(log.records_between(0, 3), Err(Unverifiable));
        // G_s = 2 の登録は、世代 3 の記録だけで足りる。
        assert_eq!(log.records_between(2, 3).unwrap().len(), 1);
    }

    #[test]
    fn skipped_record_with_a_registration_raises_the_floor_to_the_commit() {
        let log = armed(Limits::default(), 0);
        let _reg = register(&log, 0);
        log.finish_commit(1, None);
        assert_eq!(log.records_between(0, 1), Err(Unverifiable));
        assert!(log.records_between(1, 1).is_ok());
    }

    #[test]
    fn records_between_is_unverifiable_when_now_is_ahead_of_the_log() {
        let log = armed(Limits::default(), 0);
        let _reg = register(&log, 0);
        log.finish_commit(1, Some(rec(1, "a")));
        assert_eq!(log.records_between(0, 2), Err(Unverifiable));
    }

    #[test]
    fn poisoned_state_is_tainted_and_unverifiable() {
        let log = armed(Limits::default(), 0);
        let _reg = register(&log, 0);
        log.poison_for_test();
        // 追記は panic せず続行できる。
        log.finish_commit(1, Some(rec(1, "a")));
        assert_eq!(log.records_between(0, 1), Err(Unverifiable));
    }

    #[test]
    fn unarmed_log_records_nothing() {
        let log = Arc::new(CommitLog::new());
        let _reg = register(&log, 0);
        log.finish_commit(1, Some(rec(1, "a")));
        assert_eq!(log.snapshot_stats().0, 0);
        assert!(!log.should_record());
    }

    #[test]
    fn empty_table_records_do_not_occupy_the_log() {
        let log = armed(Limits::default(), 0);
        let _reg = register(&log, 0);
        log.finish_commit(
            1,
            Some(CommitRecord {
                generation: 1,
                tables: Vec::new(),
            }),
        );
        assert_eq!(log.snapshot_stats().0, 0);
        assert!(log.records_between(0, 1).unwrap().is_empty());
    }

    #[test]
    fn public_rows_collapse_to_one_element_and_private_tenants_do_not() {
        let frame = Frame {
            state: Mutex::new(FrameState::default()),
            max_per_commit: 1024,
        };
        for _ in 0..100 {
            frame.add_row("T", "a", Visibility::Public);
            frame.add_row("t", "b", Visibility::Public);
        }
        frame.add_row("t", "a", Visibility::Private);
        frame.add_row("t", "b", Visibility::Private);
        let st = frame.lock();
        let fp = st.tables.get("t").unwrap();
        assert!(fp.public);
        assert_eq!(fp.private_tenants.len(), 2);
        // 表 1 + Public 1 + Private 2。
        assert_eq!(st.elements, 4);
    }

    #[test]
    fn per_commit_overflow_is_flagged_instead_of_collapsing_private_rows() {
        let frame = Frame {
            state: Mutex::new(FrameState::default()),
            max_per_commit: 8,
        };
        for i in 0..100 {
            frame.add_row("t", &format!("tenant-{i}"), Visibility::Private);
        }
        let st = frame.lock();
        assert!(st.overflow);
        assert!(st.tables.is_empty());
    }

    /// `Public` 行の判定がテナントに依存しないことの固定。足跡を `Public` 1 要素へ畳む根拠が
    /// `PolicyContext::is_visible` の実装に対して成り立ち続けるかを検査する。
    #[test]
    fn public_collapse_matches_policy_is_visible() {
        use crate::policy::PolicyContext;
        let ctx = PolicyContext::with_visibilities(
            "tenant-me",
            [Visibility::Public, Visibility::Private],
        )
        .expect("ctx");
        for row_tenant in ["tenant-me", "tenant-other", "x"] {
            assert!(ctx.is_visible(row_tenant, Visibility::Public));
        }
        // Private は自テナントだけ可視。畳めない。
        assert!(ctx.is_visible("tenant-me", Visibility::Private));
        assert!(!ctx.is_visible("tenant-other", Visibility::Private));
    }
}
