//! DB ファイルの整合検査（RECOVER-13 (6) のポインタ。Issue #2155。親 #2154）。
//!
//! # 役割
//!
//! 読み取りトランザクション 1 本（MVCC により単一 commit 点のスナップショット）の上で全表を走査し、
//! カタログ・行・二次索引の矛盾を種類と件数で報告する。[`Storage::verify_integrity`] が公開入口で、
//! 呼び出し元は運用者向けの整合検査 CLI（#2156）を想定する。DB・ファイルは一切変更しない
//! （書き込みトランザクションを開かず、`writer_gate` も通さない）。
//!
//! # 検査項目
//!
//! 1. redb の全表（型の組の一覧 `table_image::SHAPES` で解決できること・全エントリを読めること。
//!    multimap 表と未知の型の表は「未対応」）
//! 2. カタログの各エントリが復号でき、行ストア（`user_rows/<表>`）が存在し形式が合うこと。
//!    カタログに無い行ストア（孤児）
//! 3. 全行が復号でき、行キーのテナントと行ヘッダのテナントが一致すること
//! 4. 主キー・UNIQUE 制約・一意索引（式・部分を含む）の重複（索引に依存せず行だけから判定）
//! 5. 永続一意索引と行の双方向の突き合わせ（行に対する正引きの欠落 / 行の無い正引き）
//! 6. GIN 相当索引と行の双方向の突き合わせ（`gin_index::audit_tenant_streaming`）
//! 7. `FOREIGN KEY` の孤児行（参照先に存在しない子行）
//! 8. 採番方式の表の rowid 採番状態が既存の最大 rowid より進んでいること
//!
//! # 対象外（理由）
//!
//! - シーケンスの現在値: 行から導出できる不変条件が無い（手動挿入で値が後ろに残るのは正常）
//! - FK 用のキー索引（`key_index/…`）: 登録簿が遅延構築で、欠落は書き込み側が再構築する設計。
//!   FK の判定は本検査が行から直接行う（4・7）ため、索引の状態に依存しない
//! - 一意索引・GIN 索引の逆引き（`0x02`）: 後片付け用の衛生データで正しさに関与しない
//! - マテリアライズドビューの実体: クエリの再実行なしには導出元と照合できない（表の型と全エントリの
//!   可読性のみ項目 1 で確認する）
//! - 操作 ID 台帳・バッチ台帳・世代カウンタ・索引カタログ: 他のデータから導出できる不変条件が無い
//! - 行を 1 件も持たないテナントに残る索引エントリ: 衛生指標で、行から辿れる範囲だけを検査する
//! - 一度も行が書かれていない表の行ストアの不在: 作成直後の表は行ストアを持たない設計のため、
//!   一意索引・GIN 索引に行の痕跡がある場合に限り欠落として報告する
//!
//! # 重大度と fail-closed
//!
//! 報告は [`Severity::Error`]（矛盾）・[`Severity::Warning`]（衛生指標・遅延再構築の待ち。正しさに
//! 影響しない設計）・[`Severity::Incomplete`]（検査できなかった。未対応・読み取り失敗・予算超過）の
//! 3 段。[`IntegrityReport::is_consistent`] は Error と Incomplete がどちらも 0 件のときだけ真で、
//! 判定できなかったものを「整合」と報告しない。
//!
//! # 秘匿と資源
//!
//! - 報告は表の識別（カタログの内部キー。テナント固有の値ではない）・矛盾の種類・件数だけで、行の値・
//!   キーの値・行 id・テナント名を含まない。ログにも出さない。未知の型の表は名前の最初の `/` までの
//!   系統名だけを示す
//! - 走査はすべてストリーミングで、行数に比例するメモリを使わない。重複・参照先キーの集合は
//!   [`IntegrityOptions::memory_budget_bytes`] を上限とし、超えるときはキーのハッシュで空間を分割して
//!   再走査する（分割数の上限を超えたら [`FindingKind::CheckBudgetExceeded`] で未検査と報告する）。
//!   報告に載せる (種類, 表) の組の数は [`IntegrityOptions::max_finding_groups`] で打ち切り、
//!   打ち切ったことを [`IntegrityReport::findings_truncated`] で示す（種類ごとの総件数は打ち切られない）

use std::collections::BTreeMap;
use std::fmt;

use redb::{Key, ReadTransaction, ReadableTable, TableHandle, Value};

use super::Storage;
use crate::catalog::{self, TableSchema};
use crate::constraint::integrity_audit::{self as audit, PassResult};
use crate::storage::rowid::{row_key, split_row_key, RowId};

/// 報告に載せる (種類, 表) の組の数の既定の上限。
pub const DEFAULT_MAX_FINDING_GROUPS: usize = 1024;

/// 重複・参照先キーの集合に使うバイト数の既定の上限（1 つの走査あたり）。
pub const DEFAULT_MEMORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// ハッシュ分割による再走査の分割数の上限。
const MAX_PARTITIONS: u32 = 64;

/// 検査の資源上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrityOptions {
    /// 報告に載せる (種類, 表) の組の最大数。超えた分は載せず `findings_truncated` を立てる。
    pub max_finding_groups: usize,
    /// 重複・参照先キーの集合に使うバイト数の上限（1 つの走査あたり）。
    pub memory_budget_bytes: usize,
}

impl Default for IntegrityOptions {
    fn default() -> Self {
        Self {
            max_finding_groups: DEFAULT_MAX_FINDING_GROUPS,
            memory_budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
        }
    }
}

/// 報告の重大度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// 矛盾（整合していない）。
    Error,
    /// 衛生指標・遅延再構築の待ち。設計上、正しさには影響しない。
    Warning,
    /// 検査できなかった（未対応・読み取り失敗・予算超過）。整合とは報告しない。
    Incomplete,
}

/// 報告の種類。`code()` は運用ツール向けの安定した識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingKind {
    /// カタログのエントリを復号できない。
    CatalogEntryUndecodable,
    /// カタログにある表の行ストアが無い（索引に行の痕跡がある）。
    RowStoreMissing,
    /// 行ストアの物理キー形式が現行と合わない。
    RowStoreFormatMismatch,
    /// カタログに無い表の行ストアが残っている。
    OrphanRowStore,
    /// 復号できない行（ヘッダ・本体・列・次元の不整合）。
    RowUndecodable,
    /// 行キーのテナントと行ヘッダのテナントが一致しない。
    RowTenantMismatch,
    /// 主キーの重複。
    PrimaryKeyDuplicate,
    /// UNIQUE 制約・一意索引の重複。
    UniqueKeyDuplicate,
    /// 行のキーに対する永続一意索引の正引きエントリが無い。
    UniqueIndexEntryMissing,
    /// 永続一意索引の正引きエントリが指す行が無い、または行のキーと一致しない。
    UniqueIndexEntryDangling,
    /// GIN 相当索引に、行から抽出されるポスティングが無い。
    GinIndexEntryMissing,
    /// GIN 相当索引のポスティングが指す行が無い、または行から抽出されない。
    GinIndexEntryDangling,
    /// 索引のマーカーが無効（次の書き込みで遅延再構築される設計。読み取りは索引を使わない）。
    IndexRebuildPending,
    /// 参照先に存在しない子行。
    ForeignKeyOrphanRow,
    /// `FOREIGN KEY` の参照先の表がカタログに無い、または参照先の列を解決できない。
    ForeignKeyParentMissing,
    /// 採番方式の表の rowid 採番状態が既存の最大 rowid より進んでいない。
    RowIdAllocatorBehind,
    /// 未対応の表（multimap、または型の組の一覧にない型）。
    UnsupportedTableType,
    /// 表の全エントリを読めなかった。
    TableUnreadable,
    /// 検査に必要な情報（宣言・参照先の表など）を読めず、検査を実行できなかった。
    CheckUnavailable,
    /// 行・エントリのキーを計算できず、突き合わせられなかった。
    CheckUnevaluable,
    /// メモリ予算内に収まらず、分割数の上限まで分けても検査できなかった。
    CheckBudgetExceeded,
}

impl FindingKind {
    /// 重大度。
    pub fn severity(self) -> Severity {
        use FindingKind::*;
        match self {
            CatalogEntryUndecodable
            | RowStoreMissing
            | RowStoreFormatMismatch
            | OrphanRowStore
            | RowUndecodable
            | RowTenantMismatch
            | PrimaryKeyDuplicate
            | UniqueKeyDuplicate
            | UniqueIndexEntryMissing
            | GinIndexEntryMissing
            | ForeignKeyOrphanRow
            | ForeignKeyParentMissing
            | RowIdAllocatorBehind => Severity::Error,
            UniqueIndexEntryDangling | GinIndexEntryDangling | IndexRebuildPending => {
                Severity::Warning
            }
            UnsupportedTableType | TableUnreadable | CheckUnavailable | CheckUnevaluable
            | CheckBudgetExceeded => Severity::Incomplete,
        }
    }

    /// 運用ツール向けの安定した識別子。
    pub fn code(self) -> &'static str {
        use FindingKind::*;
        match self {
            CatalogEntryUndecodable => "catalog_entry_undecodable",
            RowStoreMissing => "row_store_missing",
            RowStoreFormatMismatch => "row_store_format_mismatch",
            OrphanRowStore => "orphan_row_store",
            RowUndecodable => "row_undecodable",
            RowTenantMismatch => "row_tenant_mismatch",
            PrimaryKeyDuplicate => "primary_key_duplicate",
            UniqueKeyDuplicate => "unique_key_duplicate",
            UniqueIndexEntryMissing => "unique_index_entry_missing",
            UniqueIndexEntryDangling => "unique_index_entry_dangling",
            GinIndexEntryMissing => "gin_index_entry_missing",
            GinIndexEntryDangling => "gin_index_entry_dangling",
            IndexRebuildPending => "index_rebuild_pending",
            ForeignKeyOrphanRow => "foreign_key_orphan_row",
            ForeignKeyParentMissing => "foreign_key_parent_missing",
            RowIdAllocatorBehind => "row_id_allocator_behind",
            UnsupportedTableType => "unsupported_table_type",
            TableUnreadable => "table_unreadable",
            CheckUnavailable => "check_unavailable",
            CheckUnevaluable => "check_unevaluable",
            CheckBudgetExceeded => "check_budget_exceeded",
        }
    }
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// (種類, 表) ごとの報告 1 件。行の値・キー・テナントは含まない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityFinding {
    /// 種類。
    pub kind: FindingKind,
    /// 表の識別（カタログの内部キー。表に紐づかない報告は `None`）。
    pub table: Option<String>,
    /// 件数。
    pub count: u64,
}

/// 整合検査の結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrityReport {
    /// 走査した redb の表の数。
    pub storage_tables_scanned: u64,
    /// 検査したカタログの表の数。
    pub catalog_tables_checked: u64,
    /// 走査した行の数。
    pub rows_scanned: u64,
    /// (種類, 表) ごとの報告（種類・表の順）。上限で打ち切られうる（`findings_truncated`）。
    pub findings: Vec<IntegrityFinding>,
    /// 報告の組の数が上限に達し、一部を載せていない。
    pub findings_truncated: bool,
    totals: BTreeMap<FindingKind, u64>,
}

impl IntegrityReport {
    /// 種類 `kind` の総件数（`findings` の打ち切りの影響を受けない）。
    pub fn total(&self, kind: FindingKind) -> u64 {
        self.totals.get(&kind).copied().unwrap_or(0)
    }

    /// 重大度 `severity` の総件数。
    pub fn total_by_severity(&self, severity: Severity) -> u64 {
        self.totals
            .iter()
            .filter(|(k, _)| k.severity() == severity)
            .fold(0u64, |a, (_, n)| a.saturating_add(*n))
    }

    /// 矛盾（Error）が無く、検査できなかったもの（Incomplete）も無いか。真のときだけ「整合」と言える。
    pub fn is_consistent(&self) -> bool {
        self.total_by_severity(Severity::Error) == 0
            && self.total_by_severity(Severity::Incomplete) == 0
    }
}

/// [`Storage::verify_integrity`] の失敗（検査を始められなかった）。個々の表の読み取り失敗は
/// 報告の `Incomplete` として返り、本エラーにはならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityError {
    /// 読み取りトランザクションを開始できなかった。
    Backend,
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("storage backend error during integrity check")
    }
}

impl std::error::Error for IntegrityError {}

/// 報告の収集器。
struct Collector {
    max_groups: usize,
    groups: BTreeMap<(FindingKind, Option<String>), u64>,
    totals: BTreeMap<FindingKind, u64>,
    truncated: bool,
}

impl Collector {
    fn add(&mut self, kind: FindingKind, table: Option<&str>, n: u64) {
        if n == 0 {
            return;
        }
        let total = self.totals.entry(kind).or_insert(0);
        *total = total.saturating_add(n);
        let key = (kind, table.map(str::to_string));
        if let Some(c) = self.groups.get_mut(&key) {
            *c = c.saturating_add(n);
        } else if self.groups.len() < self.max_groups {
            self.groups.insert(key, n);
        } else {
            self.truncated = true;
        }
    }
}

/// 読み取り失敗（詳細は表名・値を含みうるため保持しない）。
struct ReadFail;

impl<E: std::error::Error> From<E> for ReadFail {
    fn from(_: E) -> Self {
        ReadFail
    }
}

/// 型の組 `(K, V)` で表を開いて全エントリを読んだ結果（`table_image::SHAPES` の探査用）。
pub(super) enum ScanProbe {
    /// この型の組ではない。
    Mismatch,
    /// 読めた（エントリ数）。
    Scanned(u64),
}

/// `(K, V)` 型で `name` を開き、全エントリを読む（`table_image` の型の組の一覧から呼ばれる）。
pub(super) fn scan_table<K: Key + 'static, V: Value + 'static>(
    read: &ReadTransaction,
    name: &str,
) -> Result<ScanProbe, ()> {
    let table = match read.open_table(redb::TableDefinition::<K, V>::new(name)) {
        Ok(t) => t,
        Err(redb::TableError::TableTypeMismatch { .. }) => return Ok(ScanProbe::Mismatch),
        Err(_) => return Err(()),
    };
    let mut n = 0u64;
    for entry in table.iter().map_err(|_| ())? {
        let (k, v) = entry.map_err(|_| ())?;
        let _ = (k.value(), v.value());
        n = n.saturating_add(1);
    }
    Ok(ScanProbe::Scanned(n))
}

type RowsRo = redb::ReadOnlyTable<(&'static str, u64), &'static [u8]>;
type IndexRo = redb::ReadOnlyTable<(&'static str, &'static [u8]), &'static [u8]>;

/// 行ストアの `after` より後ろの最小のテナントを返す（`None` は先頭から）。
fn next_tenant(rows: &RowsRo, after: Option<&str>) -> Result<Option<String>, ReadFail> {
    let lower = match after {
        Some(a) => std::ops::Bound::Excluded((a, u64::MAX)),
        None => std::ops::Bound::Unbounded,
    };
    let mut iter = rows.range::<(&str, u64)>((lower, std::ops::Bound::Unbounded))?;
    match iter.next() {
        Some(entry) => Ok(Some(entry?.0.value().0.to_string())),
        None => Ok(None),
    }
}

/// ハッシュ分割で `pass` を走らせ、集合が予算内に収まる最小の分割数で全分割の結果を合算する。
/// 分割数の上限でも収まらなければ `Ok(None)`。
fn run_partitioned<T: Copy + Default>(
    mut pass: impl FnMut(u32, u32) -> Result<PassResult<T>, ReadFail>,
    merge: impl Fn(&mut T, T),
) -> Result<Option<T>, ReadFail> {
    let mut parts = 1u32;
    loop {
        let mut acc = T::default();
        let mut over = false;
        for part in 0..parts {
            match pass(part, parts)? {
                PassResult::Done(c) => merge(&mut acc, c),
                PassResult::OverBudget => {
                    over = true;
                    break;
                }
            }
        }
        if !over {
            return Ok(Some(acc));
        }
        parts = parts.saturating_mul(2);
        if parts > MAX_PARTITIONS {
            return Ok(None);
        }
    }
}

impl Storage {
    /// DB ファイル全体の整合を**読み取りだけで**検査する（RECOVER-13 のポインタ。整合検査 CLI #2156 の
    /// 入口）。DB は変更せず、書き込みトランザクションも書き込みロックも使わないため、稼働中のサーバーが
    /// 開いている `Storage` に対しても呼べる（その時点の単一スナップショットを検査する）。
    ///
    /// 戻り値の [`IntegrityReport::is_consistent`] が真のときだけ「整合」と言える。個々の表の読み取り
    /// 失敗・未対応の型・予算超過は `Err` ではなく `Incomplete` の報告として返る（黙って飛ばさない）。
    /// 報告に行の値・キー・テナント名は含まれず、ログにも出さない。
    pub fn verify_integrity(
        &self,
        options: &IntegrityOptions,
    ) -> Result<IntegrityReport, IntegrityError> {
        let snapshot = self.begin_read().map_err(|_| IntegrityError::Backend)?;
        let read = snapshot.raw_txn();
        let mut c = Collector {
            max_groups: options.max_finding_groups,
            groups: BTreeMap::new(),
            totals: BTreeMap::new(),
            truncated: false,
        };
        let mut report = IntegrityReport::default();
        check_storage_tables(read, &mut c, &mut report);
        check_catalog(read, options, &mut c, &mut report);
        report.findings = c
            .groups
            .into_iter()
            .map(|((kind, table), count)| IntegrityFinding { kind, table, count })
            .collect();
        report.findings_truncated = c.truncated;
        report.totals = c.totals;
        Ok(report)
    }
}

/// 項目 1・孤児行ストア: redb の全表を型の組の一覧で解決して全エントリを読む。
fn check_storage_tables(read: &ReadTransaction, c: &mut Collector, report: &mut IntegrityReport) {
    match read.list_multimap_tables() {
        Ok(it) => {
            let n = it.count() as u64;
            c.add(FindingKind::UnsupportedTableType, Some("multimap"), n);
        }
        Err(_) => c.add(FindingKind::TableUnreadable, None, 1),
    }
    let handles = match read.list_tables() {
        Ok(h) => h,
        Err(_) => {
            c.add(FindingKind::TableUnreadable, None, 1);
            return;
        }
    };
    for handle in handles {
        let name = handle.name();
        report.storage_tables_scanned = report.storage_tables_scanned.saturating_add(1);
        // 系統名（最初の `/` まで）だけを報告に使う。表名の残りは動的な識別子を含みうる。
        let family = name.split('/').next().unwrap_or("");
        match super::table_image::scan_by_shape(read, name) {
            super::table_image::ScanOutcome::Scanned(_) => {}
            super::table_image::ScanOutcome::Unknown => {
                c.add(FindingKind::UnsupportedTableType, Some(family), 1)
            }
            super::table_image::ScanOutcome::Unreadable => {
                c.add(FindingKind::TableUnreadable, Some(family), 1)
            }
        }
        if let Some(key) = name.strip_prefix("user_rows/") {
            match catalog::catalog_has_key(read, key) {
                Ok(true) => {}
                Ok(false) => c.add(FindingKind::OrphanRowStore, Some(key), 1),
                Err(_) => c.add(FindingKind::TableUnreadable, Some(family), 1),
            }
        }
    }
}

/// カタログの全エントリを順に検査する。
fn check_catalog(
    read: &ReadTransaction,
    options: &IntegrityOptions,
    c: &mut Collector,
    report: &mut IntegrityReport,
) {
    let mut after: Option<String> = None;
    loop {
        let key = match catalog::next_catalog_key_after(read, after.as_deref()) {
            Ok(Some(k)) => k,
            Ok(None) => break,
            Err(_) => {
                c.add(FindingKind::CheckUnavailable, None, 1);
                break;
            }
        };
        report.catalog_tables_checked = report.catalog_tables_checked.saturating_add(1);
        match catalog::get_table_schema_in_txn(read, &key) {
            Ok(schema) => {
                if check_table(read, &key, &schema, options, c, report).is_err() {
                    c.add(FindingKind::CheckUnavailable, Some(&key), 1);
                }
            }
            Err(_) => c.add(FindingKind::CatalogEntryUndecodable, Some(&key), 1),
        }
        after = Some(key);
    }
}

/// 表 1 つ分の検査（項目 2〜8）。
fn check_table(
    read: &ReadTransaction,
    key: &str,
    schema: &TableSchema,
    options: &IntegrityOptions,
    c: &mut Collector,
    report: &mut IntegrityReport,
) -> Result<(), ReadFail> {
    let rows_name = catalog::user_rows_table_name(key);
    let uniq_name = catalog::user_uniq_table_name(key);
    let uniq: Option<IndexRo> = match read.open_table(catalog::user_uniq_table_def(&uniq_name)) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(redb::TableError::TableTypeMismatch { .. }) => {
            c.add(FindingKind::CheckUnavailable, Some(key), 1);
            None
        }
        Err(_) => return Err(ReadFail),
    };
    let decls = catalog::table_expression_decls_in_read_txn(read, key)?;
    let rows: RowsRo = match read.open_table(catalog::user_rows_table_def(&rows_name)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            // 行ストアは初回の挿入まで作られない設計。索引に行の痕跡があるときだけ欠落とする。
            let mut evidence = match &uniq {
                Some(u) => has_entry_with_tag(u, 0x01)?,
                None => false,
            };
            for decl in &decls.gins {
                let name = catalog::user_gin_table_name(&decl.index_name);
                match read.open_table(catalog::user_gin_table_def(&name)) {
                    Ok(t) => evidence = evidence || has_entry_with_tag(&t, 0x01)?,
                    Err(redb::TableError::TableDoesNotExist(_)) => {}
                    Err(_) => return Err(ReadFail),
                }
            }
            if evidence {
                c.add(FindingKind::RowStoreMissing, Some(key), 1);
            }
            return Ok(());
        }
        Err(redb::TableError::TableTypeMismatch { .. }) => {
            c.add(FindingKind::RowStoreFormatMismatch, Some(key), 1);
            return Ok(());
        }
        Err(_) => return Err(ReadFail),
    };

    scan_rows(&rows, key, schema, c, report)?;

    // 以降はテナントごと。
    let plan = match audit::UniqueAuditPlan::build(read, key, schema) {
        Ok(p) => Some(p),
        Err(_) => {
            c.add(FindingKind::CheckUnavailable, Some(key), 1);
            None
        }
    };
    let mut gin_tables = Vec::with_capacity(decls.gins.len());
    for decl in &decls.gins {
        let name = catalog::user_gin_table_name(&decl.index_name);
        let t: Option<IndexRo> = match read.open_table(catalog::user_gin_table_def(&name)) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(_) => return Err(ReadFail),
        };
        gin_tables.push(t);
    }
    let fks = resolve_foreign_keys(read, key, schema, c)?;
    let rowid_state = match read.open_table(catalog::rowid_alloc::ROWID_STATE_TABLE) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(_) => return Err(ReadFail),
    };
    let allocated = matches!(schema.row_identity(), catalog::RowIdentity::Allocated);

    let mut after: Option<String> = None;
    while let Some(tenant) = next_tenant(&rows, after.as_deref())? {
        if let Some(plan) = &plan {
            check_unique(plan, schema, &rows, uniq.as_ref(), &tenant, key, options, c)?;
        }
        for (decl, table) in decls.gins.iter().zip(gin_tables.iter()) {
            let limits = crate::gin_index::GinLimits::default();
            let (a, unevaluable) = crate::gin_index::audit_tenant_streaming(
                &rows,
                table.as_ref(),
                schema,
                &tenant,
                decl,
                &limits,
            )?;
            if !a.marker_valid {
                c.add(FindingKind::IndexRebuildPending, Some(key), 1);
            }
            c.add(
                FindingKind::GinIndexEntryMissing,
                Some(key),
                a.missing as u64,
            );
            c.add(
                FindingKind::GinIndexEntryDangling,
                Some(key),
                a.stale as u64,
            );
            c.add(FindingKind::CheckUnevaluable, Some(key), unevaluable);
        }
        for fk in &fks {
            check_foreign_key(read, schema, fk, &rows, &tenant, key, options, c)?;
        }
        if allocated {
            let max = rows
                .range::<(&str, u64)>((
                    std::ops::Bound::Included(row_key(&tenant, RowId::from_index_raw(0))),
                    std::ops::Bound::Included(row_key(&tenant, RowId::from_index_raw(u64::MAX))),
                ))?
                .next_back()
                .transpose()?
                .map(|(k, _)| split_row_key(k.value()).1.to_index_raw());
            if let Some(max) = max {
                let state = match &rowid_state {
                    Some(t) => t.get((key, tenant.as_str()))?.map(|g| g.value()),
                    None => None,
                };
                // 状態が未作成なら最初の払い出しで既存の最大 rowid から初期化される設計（矛盾ではない）。
                if state.is_some_and(|last| last < max) {
                    c.add(FindingKind::RowIdAllocatorBehind, Some(key), 1);
                }
            }
        }
        after = Some(tenant);
    }
    Ok(())
}

/// 索引表に先頭バイトが `tag` のサブキーのエントリが 1 つでもあるか。
fn has_entry_with_tag(index: &IndexRo, tag: u8) -> Result<bool, ReadFail> {
    for entry in index.iter()? {
        let (k, _) = entry?;
        if k.value().1.first() == Some(&tag) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 項目 3: 全テナントの全行を復号する（ストリーミング）。
fn scan_rows(
    rows: &RowsRo,
    key: &str,
    schema: &TableSchema,
    c: &mut Collector,
    report: &mut IntegrityReport,
) -> Result<(), ReadFail> {
    let (mut undecodable, mut mismatch) = (0u64, 0u64);
    for entry in rows.iter()? {
        let (k, v) = entry?;
        report.rows_scanned = report.rows_scanned.saturating_add(1);
        let (key_tenant, _) = k.value();
        let buf = v.value();
        let decoded = (|| {
            let (header_tenant, _, _) = crate::storage::decode_row_header(buf).ok()?;
            let tenant_ok = header_tenant == key_tenant;
            let (dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf).ok()?;
            crate::row_codec::validate_scalar_columns(schema, metadata).ok()?;
            schema
                .validate_row_embedding_dim(usize::try_from(dim).ok()?)
                .ok()?;
            Some(tenant_ok)
        })();
        match decoded {
            Some(true) => {}
            Some(false) => mismatch = mismatch.saturating_add(1),
            None => undecodable = undecodable.saturating_add(1),
        }
    }
    c.add(FindingKind::RowUndecodable, Some(key), undecodable);
    c.add(FindingKind::RowTenantMismatch, Some(key), mismatch);
    Ok(())
}

/// 項目 4・5: 主キー・一意キーの重複と、永続一意索引との突き合わせ。
#[allow(clippy::too_many_arguments)] // 走査ドライバ内の検査部品。表・テナント・資源上限を明示する。
fn check_unique(
    plan: &audit::UniqueAuditPlan,
    schema: &TableSchema,
    rows: &RowsRo,
    uniq: Option<&IndexRo>,
    tenant: &str,
    key: &str,
    options: &IntegrityOptions,
    c: &mut Collector,
) -> Result<(), ReadFail> {
    if plan.is_empty() {
        return Ok(());
    }
    let dup = run_partitioned(
        |part, parts| {
            Ok(audit::duplicate_keys_in_tenant(
                plan,
                schema,
                rows,
                tenant,
                part,
                parts,
                options.memory_budget_bytes,
            )?)
        },
        |acc: &mut audit::DuplicateCounts, d| {
            acc.primary_key = acc.primary_key.saturating_add(d.primary_key);
            acc.unique = acc.unique.saturating_add(d.unique);
            acc.unevaluable = acc.unevaluable.saturating_add(d.unevaluable);
        },
    )?;
    match dup {
        Some(d) => {
            c.add(FindingKind::PrimaryKeyDuplicate, Some(key), d.primary_key);
            c.add(FindingKind::UniqueKeyDuplicate, Some(key), d.unique);
            c.add(FindingKind::CheckUnevaluable, Some(key), d.unevaluable);
        }
        None => c.add(FindingKind::CheckBudgetExceeded, Some(key), 1),
    }
    match uniq {
        None => c.add(FindingKind::IndexRebuildPending, Some(key), 1),
        Some(index) => {
            let a = audit::unique_index_audit(plan, schema, rows, index, tenant)?;
            if !a.marker_valid {
                c.add(FindingKind::IndexRebuildPending, Some(key), 1);
            }
            c.add(FindingKind::UniqueIndexEntryMissing, Some(key), a.missing);
            c.add(FindingKind::UniqueIndexEntryDangling, Some(key), a.dangling);
            c.add(FindingKind::CheckUnevaluable, Some(key), a.unevaluable);
        }
    }
    Ok(())
}

/// 子表の `FOREIGN KEY` 1 件と、参照先の解決結果。
struct ResolvedFk<'a> {
    fk: &'a catalog::ForeignKeyDef,
    parent_schema: TableSchema,
    parent_rows: Option<RowsRo>,
}

/// 子表の `FOREIGN KEY` の参照先（スキーマと行ストア）を解決する。解決できないものは
/// `ForeignKeyParentMissing` として報告し、検査対象から外す。
fn resolve_foreign_keys<'a>(
    read: &ReadTransaction,
    key: &str,
    schema: &'a TableSchema,
    c: &mut Collector,
) -> Result<Vec<ResolvedFk<'a>>, ReadFail> {
    let mut out = Vec::with_capacity(schema.foreign_keys().len());
    for fk in schema.foreign_keys() {
        let parent_schema = if fk.parent_table() == key {
            schema.clone()
        } else {
            match catalog::get_table_schema_in_txn(read, fk.parent_table()) {
                Ok(s) => s,
                Err(_) => {
                    c.add(FindingKind::ForeignKeyParentMissing, Some(key), 1);
                    continue;
                }
            }
        };
        let rows_name = catalog::user_rows_table_name(fk.parent_table());
        let parent_rows = match read.open_table(catalog::user_rows_table_def(&rows_name)) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(_) => {
                c.add(FindingKind::CheckUnavailable, Some(key), 1);
                continue;
            }
        };
        out.push(ResolvedFk {
            fk,
            parent_schema,
            parent_rows,
        });
    }
    Ok(out)
}

/// 項目 7: 子表 1 つ・FK 1 つ・テナント 1 つの孤児行。
#[allow(clippy::too_many_arguments)] // 走査ドライバ内の検査部品。表・テナント・資源上限を明示する。
fn check_foreign_key(
    _read: &ReadTransaction,
    schema: &TableSchema,
    fk: &ResolvedFk<'_>,
    rows: &RowsRo,
    tenant: &str,
    key: &str,
    options: &IntegrityOptions,
    c: &mut Collector,
) -> Result<(), ReadFail> {
    let result = run_partitioned(
        |part, parts| {
            Ok(audit::foreign_key_orphans_in_tenant(
                schema,
                &fk.parent_schema,
                fk.fk,
                rows,
                fk.parent_rows.as_ref(),
                tenant,
                part,
                parts,
                options.memory_budget_bytes,
            )?)
        },
        |acc: &mut audit::ForeignKeyCounts, d| {
            acc.orphans = acc.orphans.saturating_add(d.orphans);
            acc.unevaluable = acc.unevaluable.saturating_add(d.unevaluable);
        },
    )?;
    match result {
        Some(d) => {
            c.add(FindingKind::ForeignKeyOrphanRow, Some(key), d.orphans);
            c.add(FindingKind::CheckUnevaluable, Some(key), d.unevaluable);
        }
        None => c.add(FindingKind::CheckBudgetExceeded, Some(key), 1),
    }
    Ok(())
}
