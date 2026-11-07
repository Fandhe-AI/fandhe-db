//! 書き込みトランザクションの内容差分（`TableImage`）の取得と再適用
//! （Issue #2879・SQL-69 のポインタ。`docs/design/explicit-transaction.md`）。
//!
//! # 役割
//!
//! redb 4.2.0 の `ephemeral_savepoint` は writer が一度でもテーブルを開いた後は作れない。
//! そこで `sql::transaction` は明示 Tx の writer 取得直後に「基底 savepoint」を取っておき、
//! 書き込み後の `SAVEPOINT` では本モジュールで「確定済み状態（= 基底の状態。writer は排他）と
//! writer の内容の行単位差分」を保存する。`ROLLBACK TO` は基底へ復元してから差分を再適用して
//! savepoint 時点の状態を再構成する（「基底への復元 + 内容差分の再適用」方式）。
//!
//! # 完全性（fail-open を作らない根拠）
//!
//! 差分は engine が記録した書き込みではなく **redb の実際の内容**から計算する。書き込み経路を
//! 1 つも計装しないため、計装漏れで巻き戻しが過少になる経路が構造上存在しない。
//!
//! # 型の扱い
//!
//! `WriteTransaction` には型なしの `open_untyped_table` が無いため、engine が使う `(K, V)` の
//! 型の組（shape）を [`SHAPES`] に列挙し、テーブルごとに順に試して `TableTypeMismatch` でない
//! 最初のものを使う。全 shape が不一致のテーブル・multimap テーブルがあれば [`Capture::Gate`]
//! （呼び出し側は savepoint を巻き戻し不可に落とす = 従来どおり `0A000` の fail-closed）。
//!
//! 同じ一覧は稼働中バックアップの書き出し（`storage::snapshot`）も表の複製に共有する。
//!
//! # 資源上限
//!
//! 差分のバイト数は [`Meter`] で割り当て前に計上し、上限超過・期限超過は `Gate`。走査は排他 writer を
//! 保持したまま全テーブルを読むため、トランザクションの持続時間の期限で打ち切る。
//! 上限値は実装既定値（[`MAX_IMAGE_BYTES_PER_SAVEPOINT`]・[`MAX_IMAGE_BYTES_PER_TXN`]）で、
//! spec 由来の数値ではない。

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::Instant;

use redb::{
    Key, ReadTransaction, ReadableTable, TableDefinition, TableError, TableHandle, Value,
    WriteTransaction,
};

/// savepoint 1 個あたりの差分の合計バイト数の上限（実装既定値。spec 由来ではない）。
pub(crate) const MAX_IMAGE_BYTES_PER_SAVEPOINT: usize = 16 * 1024 * 1024;
/// 1 トランザクション内の全 savepoint の差分の合計バイト数の上限（実装既定値。spec 由来ではない）。
pub(crate) const MAX_IMAGE_BYTES_PER_TXN: usize = 64 * 1024 * 1024;

/// redb の読み書き失敗（詳細は呼び出し側で固定文言の内部エラーに写すため保持しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageError;

/// [`capture`] の結果。
#[derive(Debug)]
pub(crate) enum Capture {
    /// 差分を取得できた。
    Image(TableImage),
    /// 巻き戻しを保証できない（未知の shape・multimap・上限超過・期限超過）。呼び出し側は
    /// savepoint を巻き戻し不可にして fail-closed に倒す。
    Gate,
}

/// 差分のバイト数と走査の期限を管理する計器。
#[derive(Debug)]
pub(crate) struct Meter {
    remaining: usize,
    deadline: Option<Instant>,
    ticks: u32,
}

impl Meter {
    /// 上限 `limit` バイト・期限 `deadline`（`None` は無期限）の計器を作る。
    pub(crate) fn new(limit: usize, deadline: Option<Instant>) -> Self {
        Self {
            remaining: limit,
            deadline,
            ticks: 0,
        }
    }

    /// `n` バイトを計上する。上限超過なら `false`。
    fn charge(&mut self, n: usize) -> bool {
        match self.remaining.checked_sub(n) {
            Some(r) => {
                self.remaining = r;
                true
            }
            None => false,
        }
    }

    /// 1 行走査したことを数え、期限を超えていたら `false`（一定行数ごとにだけ時計を見る）。
    fn tick(&mut self) -> bool {
        self.ticks = self.ticks.wrapping_add(1);
        if !self.ticks.is_multiple_of(256) {
            return true;
        }
        self.deadline.is_none_or(|d| Instant::now() <= d)
    }
}

/// 1 テーブル分の差分。
#[derive(Debug, Clone)]
struct TableDelta {
    name: String,
    /// [`SHAPES`] の添字。
    shape: usize,
    /// 追加・更新された行（キー, 値）のバイト列。
    upserts: Vec<(Vec<u8>, Vec<u8>)>,
    /// writer で削除された行のキー。
    removes: Vec<Vec<u8>>,
}

/// writer の内容と確定済み状態の差分（全テーブル分）。
#[derive(Debug, Clone, Default)]
pub(crate) struct TableImage {
    deltas: Vec<TableDelta>,
    /// 確定済み状態にあり、writer で削除されたテーブル名。
    dropped: Vec<String>,
    bytes: usize,
}

impl TableImage {
    /// 差分が空（writer の内容が確定済み状態と一致）か。
    pub(crate) fn is_empty(&self) -> bool {
        self.deltas.is_empty() && self.dropped.is_empty()
    }

    /// 差分が占めるバイト数の見積り（キーと値の長さの合計）。Tx 全体の上限判定に使う。
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// shape ごとの操作（関数ポインタ表）。
struct Shape {
    capture: fn(
        &WriteTransaction,
        &ReadTransaction,
        &str,
        usize,
        &mut Meter,
    ) -> Result<Probe, ImageError>,
    apply: fn(&WriteTransaction, &TableDelta) -> Result<(), ImageError>,
    /// 稼働中バックアップの書き出し（`storage::snapshot`・RECOVER-13 のポインタ）が使う表の複製。
    /// 型の組の一覧を savepoint と共有し、新しい型の表の登録漏れを両機能で同時に検知する。
    copy: fn(
        &ReadTransaction,
        &WriteTransaction,
        &str,
        &mut u64,
    ) -> Result<super::snapshot::CopyProbe, super::snapshot::SnapshotExportError>,
    /// 整合検査（`storage::integrity`・RECOVER-13 のポインタ）の全エントリ読み取り。型の組の一覧を
    /// 共有し、一覧に無い型の表を「未対応」として検知する。
    scan: fn(&ReadTransaction, &str) -> Result<super::integrity::ScanProbe, ()>,
}

enum Probe {
    /// この shape ではない。
    Mismatch,
    /// 上限超過・期限超過・確定側との型不一致。
    Gate,
    /// 取得できた（`None` は確定済み状態と同一）。
    Delta(Option<TableDelta>),
}

fn capture_table<K: Key + 'static, V: Value + 'static>(
    writer: &WriteTransaction,
    committed: &ReadTransaction,
    name: &str,
    shape: usize,
    meter: &mut Meter,
) -> Result<Probe, ImageError> {
    let def = TableDefinition::<K, V>::new(name);
    let wt = match writer.open_table(def) {
        Ok(t) => t,
        Err(TableError::TableTypeMismatch { .. }) => return Ok(Probe::Mismatch),
        Err(_) => return Err(ImageError),
    };
    let ct = match committed.open_table(def) {
        Ok(t) => Some(t),
        Err(TableError::TableDoesNotExist(_)) => None,
        Err(TableError::TableTypeMismatch { .. }) => return Ok(Probe::Gate),
        Err(_) => return Err(ImageError),
    };
    let created = ct.is_none();
    let mut upserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut removes: Vec<Vec<u8>> = Vec::new();

    let mut wi = wt.iter().map_err(|_| ImageError)?;
    let mut ci = match &ct {
        Some(t) => Some(t.iter().map_err(|_| ImageError)?),
        None => None,
    };
    let mut w_cur = wi.next().transpose().map_err(|_| ImageError)?;
    let mut c_cur = match ci.as_mut() {
        Some(i) => i.next().transpose().map_err(|_| ImageError)?,
        None => None,
    };
    loop {
        if !meter.tick() {
            return Ok(Probe::Gate);
        }
        let order = match (&w_cur, &c_cur) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((wk, _)), Some((ck, _))) => {
                let wkv = wk.value();
                let ckv = ck.value();
                let wb = K::as_bytes(&wkv);
                let cb = K::as_bytes(&ckv);
                K::compare(wb.as_ref(), cb.as_ref())
            }
        };
        match order {
            // writer にだけある行（追加）。
            Ordering::Less => {
                if let Some((wk, wv)) = &w_cur {
                    let (kv, vv) = (wk.value(), wv.value());
                    let kb = K::as_bytes(&kv);
                    let vb = V::as_bytes(&vv);
                    let (kb, vb) = (kb.as_ref(), vb.as_ref());
                    if !meter.charge(kb.len().saturating_add(vb.len())) {
                        return Ok(Probe::Gate);
                    }
                    upserts.push((kb.to_vec(), vb.to_vec()));
                }
                w_cur = wi.next().transpose().map_err(|_| ImageError)?;
            }
            // 確定済み状態にだけある行（writer で削除）。
            Ordering::Greater => {
                if let Some((ck, _)) = &c_cur {
                    let kv = ck.value();
                    let kb = K::as_bytes(&kv);
                    let kb = kb.as_ref();
                    if !meter.charge(kb.len()) {
                        return Ok(Probe::Gate);
                    }
                    removes.push(kb.to_vec());
                }
                c_cur = match ci.as_mut() {
                    Some(i) => i.next().transpose().map_err(|_| ImageError)?,
                    None => None,
                };
            }
            // 両方にある行（値が違えば更新）。
            Ordering::Equal => {
                if let (Some((wk, wv)), Some((_, cv))) = (&w_cur, &c_cur) {
                    let (wvv, cvv) = (wv.value(), cv.value());
                    let wvb = V::as_bytes(&wvv);
                    let cvb = V::as_bytes(&cvv);
                    if wvb.as_ref() != cvb.as_ref() {
                        let kv = wk.value();
                        let kb = K::as_bytes(&kv);
                        let (kb, vb) = (kb.as_ref(), wvb.as_ref());
                        if !meter.charge(kb.len().saturating_add(vb.len())) {
                            return Ok(Probe::Gate);
                        }
                        upserts.push((kb.to_vec(), vb.to_vec()));
                    }
                }
                w_cur = wi.next().transpose().map_err(|_| ImageError)?;
                c_cur = match ci.as_mut() {
                    Some(i) => i.next().transpose().map_err(|_| ImageError)?,
                    None => None,
                };
            }
        }
    }
    if !created && upserts.is_empty() && removes.is_empty() {
        return Ok(Probe::Delta(None));
    }
    Ok(Probe::Delta(Some(TableDelta {
        name: name.to_string(),
        shape,
        upserts,
        removes,
    })))
}

fn apply_table<K: Key + 'static, V: Value + 'static>(
    writer: &WriteTransaction,
    delta: &TableDelta,
) -> Result<(), ImageError> {
    let mut table = writer
        .open_table(TableDefinition::<K, V>::new(&delta.name))
        .map_err(|_| ImageError)?;
    for k in &delta.removes {
        table.remove(K::from_bytes(k)).map_err(|_| ImageError)?;
    }
    for (k, v) in &delta.upserts {
        table
            .insert(K::from_bytes(k), V::from_bytes(v))
            .map_err(|_| ImageError)?;
    }
    Ok(())
}

macro_rules! shapes {
    ($(($k:ty, $v:ty)),+ $(,)?) => {
        &[$(Shape {
            capture: capture_table::<$k, $v>,
            apply: apply_table::<$k, $v>,
            copy: super::snapshot::copy_table::<$k, $v>,
            scan: super::integrity::scan_table::<$k, $v>,
        }),+]
    };
}

/// engine が使う redb テーブルの `(K, V)` の型の組。新しい型のテーブルを追加したら、ここへ
/// 足す（足し忘れは `Capture::Gate` = 書き込み後 savepoint の `ROLLBACK TO` が `0A000` になる
/// だけで fail-closed。`table_image` の網羅テストが検知する）。
static SHAPES: &[Shape] = shapes![
    (&'static str, u64),
    (&'static str, &'static [u8]),
    ((&'static str, &'static str), &'static [u8]),
    ((&'static str, &'static str), u64),
    ((&'static str, &'static str, &'static str), &'static [u8]),
    ((&'static str, &'static str, &'static str), ()),
    ((&'static str, u64), &'static [u8]),
    ((&'static str, u64, u64), &'static [u8]),
    ((&'static str, &'static [u8]), &'static [u8]),
    ((&'static str, &'static [u8], u64), ()),
    ((&'static [u8], u64), &'static [u8]),
    (u64, u64),
    (u64, &'static [u8]),
    ((u8, &'static str, &'static str), &'static [u8]),
    (
        (u8, &'static str, &'static str, &'static str, &'static str),
        ()
    ),
    (
        (&'static str, &'static str, &'static str, u8, &'static str),
        ()
    ),
    ((&'static str, u64, u64, u8, u64), &'static [u8]),
    ((&'static str, u64, u64, u8), &'static [u8]),
    (
        (&'static str, u8, &'static str, &'static str, u64, u64),
        &'static [u8]
    ),
    ((&'static str, u8, &'static str), &'static [u8]),
];

/// 型の組の一覧の件数（`storage::snapshot` の網羅テストが、型の追加漏れを検知するために使う）。
#[cfg(test)]
pub(super) fn shape_count() -> usize {
    SHAPES.len()
}

/// [`scan_by_shape`] の結果。
pub(super) enum ScanOutcome {
    /// 読めた（エントリ数）。
    Scanned(#[allow(dead_code)] u64),
    /// どの型の組でも開けない（未知の型の表）。
    Unknown,
    /// 型は合ったが全エントリを読めなかった。
    Unreadable,
}

/// 型の組の一覧を順に試して `name` の表の全エントリを読む（整合検査用。読み取り専用）。
pub(super) fn scan_by_shape(read: &ReadTransaction, name: &str) -> ScanOutcome {
    for shape in SHAPES {
        match (shape.scan)(read, name) {
            Ok(super::integrity::ScanProbe::Mismatch) => continue,
            Ok(super::integrity::ScanProbe::Scanned(n)) => return ScanOutcome::Scanned(n),
            Err(()) => return ScanOutcome::Unreadable,
        }
    }
    ScanOutcome::Unknown
}

/// 型の組の一覧を順に試して `name` の表を `src` から `dst` へ複製する（`storage::snapshot` 用）。
///
/// どの shape でも開けなければ `Ok(false)`（呼び出し側は未知の型の表として中止する）。
pub(super) fn copy_by_shape(
    src: &ReadTransaction,
    dst: &WriteTransaction,
    name: &str,
    entries: &mut u64,
) -> Result<bool, super::snapshot::SnapshotExportError> {
    for shape in SHAPES {
        match (shape.copy)(src, dst, name, entries)? {
            super::snapshot::CopyProbe::Mismatch => continue,
            super::snapshot::CopyProbe::Copied => return Ok(true),
        }
    }
    Ok(false)
}

/// writer の内容と確定済み状態（`committed`）の差分を取る。
///
/// 前提: `committed` は writer の基底（writer 取得前に確定済み）の状態を表す。writer は排他で
/// 保持されるため、writer 取得中に確定済み状態は変わらない。
pub(crate) fn capture(
    writer: &WriteTransaction,
    committed: &ReadTransaction,
    meter: &mut Meter,
) -> Result<Capture, ImageError> {
    if writer
        .list_multimap_tables()
        .map_err(|_| ImageError)?
        .next()
        .is_some()
        || committed
            .list_multimap_tables()
            .map_err(|_| ImageError)?
            .next()
            .is_some()
    {
        return Ok(Capture::Gate);
    }
    let wnames: Vec<String> = writer
        .list_tables()
        .map_err(|_| ImageError)?
        .map(|h| h.name().to_string())
        .collect();
    let cnames: BTreeSet<String> = committed
        .list_tables()
        .map_err(|_| ImageError)?
        .map(|h| h.name().to_string())
        .collect();
    let wset: BTreeSet<&String> = wnames.iter().collect();
    let dropped: Vec<String> = cnames
        .iter()
        .filter(|n| !wset.contains(n))
        .cloned()
        .collect();
    let mut image = TableImage {
        deltas: Vec::new(),
        dropped,
        bytes: 0,
    };
    for name in &wnames {
        let mut found = None;
        for (idx, shape) in SHAPES.iter().enumerate() {
            match (shape.capture)(writer, committed, name, idx, meter)? {
                Probe::Mismatch => continue,
                Probe::Gate => return Ok(Capture::Gate),
                Probe::Delta(d) => {
                    found = Some(d);
                    break;
                }
            }
        }
        match found {
            // どの shape でも開けない = 未知の型のテーブル。
            None => return Ok(Capture::Gate),
            Some(None) => {}
            Some(Some(d)) => image.deltas.push(d),
        }
    }
    let mut total = 0usize;
    for d in &image.deltas {
        for (k, v) in &d.upserts {
            total = total.saturating_add(k.len()).saturating_add(v.len());
        }
        for k in &d.removes {
            total = total.saturating_add(k.len());
        }
    }
    image.bytes = total;
    Ok(Capture::Image(image))
}

/// 基底へ復元済みの writer に差分を再適用する。呼び出し側は事前に `restore_savepoint` を
/// 済ませ、開いているテーブルのハンドルが無いこと（本関数内で開閉する）を保証する。
pub(crate) fn apply(writer: &WriteTransaction, image: &TableImage) -> Result<(), ImageError> {
    if !image.dropped.is_empty() {
        let handles: Vec<_> = writer
            .list_tables()
            .map_err(|_| ImageError)?
            .filter(|h| image.dropped.iter().any(|n| n == h.name()))
            .collect();
        for h in handles {
            writer.delete_table(h).map_err(|_| ImageError)?;
        }
    }
    for d in &image.deltas {
        let shape = SHAPES.get(d.shape).ok_or(ImageError)?;
        (shape.apply)(writer, d)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    const A: TableDefinition<&str, u64> = TableDefinition::new("a");
    const B: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("b");
    const C: TableDefinition<&str, ()> = TableDefinition::new("unknown_shape");

    fn db() -> (Database, tempfile_dir::Dir) {
        let dir = tempfile_dir::Dir::new();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        (db, dir)
    }

    /// 依存を増やさない最小の一時ディレクトリ。
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        pub(super) struct Dir(PathBuf);
        impl Dir {
            pub(super) fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "fandhe-table-image-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub(super) fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn dump_a(db: &Database) -> Vec<(String, u64)> {
        let r = db.begin_read().unwrap();
        match r.open_table(A) {
            Ok(t) => t
                .iter()
                .unwrap()
                .map(|e| {
                    let (k, v) = e.unwrap();
                    (k.value().to_string(), v.value())
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn capture_and_apply_round_trip_restores_savepoint_state() {
        let (db, _d) = db();
        {
            let w = db.begin_write().unwrap();
            {
                let mut t = w.open_table(A).unwrap();
                t.insert("keep", 1).unwrap();
                t.insert("upd", 2).unwrap();
                t.insert("del", 3).unwrap();
            }
            w.commit().unwrap();
        }
        let mut w = db.begin_write().unwrap();
        let base = w.ephemeral_savepoint().unwrap();
        {
            let mut t = w.open_table(A).unwrap();
            t.insert("upd", 20).unwrap();
            t.remove("del").unwrap();
            t.insert("new1", 7).unwrap();
            let mut b = w.open_table(B).unwrap();
            b.insert(("x", 1), b"v".as_slice()).unwrap();
        }
        let committed = db.begin_read().unwrap();
        let mut meter = Meter::new(1 << 20, None);
        let Capture::Image(img) = capture(&w, &committed, &mut meter).unwrap() else {
            panic!("gate");
        };
        assert!(!img.is_empty());
        // savepoint 後の書き込み。
        {
            let mut t = w.open_table(A).unwrap();
            t.insert("later", 99).unwrap();
            t.remove("keep").unwrap();
            t.insert("upd", 21).unwrap();
        }
        w.restore_savepoint(&base).unwrap();
        apply(&w, &img).unwrap();
        // 再適用後の writer の内容 = savepoint 時点（capture を再実行して同じ差分になる）。
        let again = match capture(&w, &committed, &mut Meter::new(1 << 20, None)).unwrap() {
            Capture::Image(i) => i,
            Capture::Gate => panic!("gate"),
        };
        assert_eq!(again.bytes(), img.bytes());
        {
            let t = w.open_table(A).unwrap();
            assert_eq!(t.get("upd").unwrap().unwrap().value(), 20);
            assert!(t.get("del").unwrap().is_none());
            assert_eq!(t.get("keep").unwrap().unwrap().value(), 1);
            assert_eq!(t.get("new1").unwrap().unwrap().value(), 7);
            assert!(t.get("later").unwrap().is_none());
        }
        drop(base);
        w.commit().unwrap();
        let rows = dump_a(&db);
        assert_eq!(
            rows,
            vec![
                ("keep".to_string(), 1),
                ("new1".to_string(), 7),
                ("upd".to_string(), 20)
            ]
        );
    }

    #[test]
    fn dropped_and_created_tables_round_trip() {
        let (db, _d) = db();
        {
            let w = db.begin_write().unwrap();
            w.open_table(A).unwrap().insert("k", 1).unwrap();
            w.commit().unwrap();
        }
        let mut w = db.begin_write().unwrap();
        let base = w.ephemeral_savepoint().unwrap();
        assert!(w.delete_table(A).unwrap());
        // 空のまま作るテーブルも再現される。
        drop(w.open_table(B).unwrap());
        let committed = db.begin_read().unwrap();
        let Capture::Image(img) = capture(&w, &committed, &mut Meter::new(1 << 20, None)).unwrap()
        else {
            panic!("gate");
        };
        w.open_table(A).unwrap().insert("zz", 5).unwrap();
        w.restore_savepoint(&base).unwrap();
        apply(&w, &img).unwrap();
        let names: Vec<String> = w
            .list_tables()
            .unwrap()
            .map(|h| h.name().to_string())
            .collect();
        assert_eq!(names, vec!["b".to_string()]);
    }

    #[test]
    fn unknown_shape_and_budget_and_deadline_gate() {
        let (db, _d) = db();
        let w = db.begin_write().unwrap();
        w.open_table(C).unwrap().insert("k", ()).unwrap();
        let committed = db.begin_read().unwrap();
        assert!(matches!(
            capture(&w, &committed, &mut Meter::new(1 << 20, None)).unwrap(),
            Capture::Gate
        ));
        drop(w);
        let w = db.begin_write().unwrap();
        w.open_table(A).unwrap().insert("key", 1).unwrap();
        // 上限 1 バイトでは超過。
        assert!(matches!(
            capture(&w, &committed, &mut Meter::new(1, None)).unwrap(),
            Capture::Gate
        ));
        // 上限内なら取得できる。
        assert!(matches!(
            capture(&w, &committed, &mut Meter::new(1 << 20, None)).unwrap(),
            Capture::Image(_)
        ));
    }

    #[test]
    fn identical_state_yields_empty_image() {
        let (db, _d) = db();
        {
            let w = db.begin_write().unwrap();
            w.open_table(A).unwrap().insert("k", 1).unwrap();
            w.commit().unwrap();
        }
        let w = db.begin_write().unwrap();
        drop(w.open_table(A).unwrap());
        let committed = db.begin_read().unwrap();
        let Capture::Image(img) = capture(&w, &committed, &mut Meter::new(1 << 20, None)).unwrap()
        else {
            panic!("gate");
        };
        assert!(img.is_empty());
        assert_eq!(img.bytes(), 0);
    }
}
