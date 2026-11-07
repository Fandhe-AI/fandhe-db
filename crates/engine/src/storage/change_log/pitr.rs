//! 指定 LSN／時刻までの PITR 復元（Issue #2169・ポインタ: RECOVER-14。設計は
//! `docs/design/change-log-pitr.md` の D12）。
//!
//! # 役割と責務境界
//!
//! ベースの DB ファイルの複製（呼び出し側が一時名へ作ったもの。wire-server の `restore` が
//! `restore_opt` で用意する）へ、増分バックアップ（`incremental`）と変更ログのアーカイブ（`archive`）の
//! セグメント列を、**指定点のコミット境界まで**再生する。再生器そのもの
//! （[`replay_segments`](super::incremental::replay_segments)）は増分の適用と共有し、本ファイルは
//! 「入力の検証・選択」と「目標（LSN・時刻）から停止 LSN への解決」だけを持つ。
//!
//! # 停止点（コミット境界）
//!
//! 1 レコード = 1 commit（D3・D4）なので、レコードの途中で止まることはない。目標が
//!
//! - **LSN** `n`: LSN `n` のレコードを適用した直後で止まる（`n` 以下の最後のコミット境界）
//! - **時刻** `T`（UTC のマイクロ秒）: コミット時刻が `T` を超える**最初の**レコードの直前で止まる。
//!   壁時計が逆行していても、結果は commit 順の接頭辞になり決定的（D7）。時刻はレコードのヘッダに
//!   記録済みの値（コミット時に採った壁時計）だけを使い、推測しない
//!
//! # fail-closed
//!
//! - 目標がベースの終点（増分があればその末尾）以前、または入力したログの末尾より後なら
//!   [`IncrementalError::TargetOutOfRange`]。時刻指定で `T` を超えるレコードが無い（末尾まで行ってしまう）
//!   場合も同じ。黙って末尾まで適用しない
//! - アーカイブのディレクトリ全体を、DB 識別子・timeline をベースに合わせて検証してから使う（LSN の飛び・
//!   重複・欠落、チェックサム、別 DB・別 timeline、名前とヘッダの不一致）。他者が書けるディレクトリは拒否する
//! - ベースの終点の次から始まるセグメントが無い（アーカイブの先頭が後ろにずれている・刈り込み済み）場合は
//!   `ChainBroken`。再生中も `prev_lsn` の連鎖とセグメントの差し替え（ヘッダ・全体チェックサム）を検査する
//! - 停止点より後ろのセグメントも、ディレクトリ全体の検証で破損・改ざんを検出する
//! - エラーの `Display` は固定の英語文言で、パス・LSN・時刻を含めない
//!
//! 復元後の世代番号の進行・timeline の分岐（#2446）は行わない。復元した DB は元と同じ DB 識別子・
//! timeline を持つ（D7 の「分岐の未検出」と同じ扱い）。

use std::path::{Path, PathBuf};

use super::archive::{
    check_dir_not_shared, count_segment_files, verified_segments, ArchiveExpectation, SegmentReader,
};
use super::incremental::{
    chain_point_of, replay_segments, verify_incrementals, ApplySummary, IncrementalError,
    ReplaySegment,
};
use super::{peek_record, CaptureLimits, STORAGE_FORMAT_VERSION};
use crate::storage::change_log::archive::ArchiveError;

type Result<T> = std::result::Result<T, IncrementalError>;

/// 1 回の復元で使うセグメント数の上限（untrusted なアーカイブ向けの資源上限。実装既定値）。
const MAX_PITR_SEGMENTS: usize = 100_000;

/// PITR の目標。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitrTarget {
    /// この LSN のレコード（commit）を適用した直後で止める。0 は不正。
    Lsn(u64),
    /// コミット時刻（UTC・1970-01-01 起点のマイクロ秒）が、この値を超える最初のレコードの直前で止める。
    TimeMicros(i64),
}

/// `db_path`（ベースの複製）へ、`incrementals`（古い順の増分バックアップ）と `archive_dir`（変更ログの
/// アーカイブ。省略可）のセグメントを `target` のコミット境界まで再生する（モジュール doc）。
/// 増分とアーカイブの両方があれば、増分の末尾より後ろをアーカイブから補う（重なる区間は読み飛ばす）。
/// どちらも無ければ `BadSegmentList`。成功時の [`ApplySummary::last_lsn`] が停止した LSN。
/// 失敗時 `db_path` は途中まで適用された状態になりうるので、呼び出し側が破棄する（元のバックアップ・
/// アーカイブには書かない）。wire-server の `restore` が呼ぶ。
pub fn restore_to_point(
    db_path: &Path,
    incrementals: &[PathBuf],
    archive_dir: Option<&Path>,
    target: PitrTarget,
) -> Result<ApplySummary> {
    if incrementals.is_empty() && archive_dir.is_none() {
        return Err(IncrementalError::BadSegmentList);
    }
    if target == PitrTarget::Lsn(0) {
        return Err(IncrementalError::InvalidTarget);
    }
    let max_payload = CaptureLimits::default().max_payload_bytes;
    // ベースの識別子と終点（読み取り専用で開く。再生で書き込みを開く前に閉じる）。
    let base = chain_point_of(db_path)?;

    // 1. 入力の検証と並び（DB に触れる前）。増分 → アーカイブの順。
    let mut list: Vec<ReplaySegment> = if incrementals.is_empty() {
        Vec::new()
    } else {
        verify_incrementals(incrementals)?
    };
    let covered_end = list.last().map_or(base.end_lsn, |r| r.info.header.last_lsn);
    let mut archive: Vec<ReplaySegment> = Vec::new();
    if let Some(dir) = archive_dir {
        check_dir_not_shared(dir).map_err(IncrementalError::from)?;
        // 件数の上限は、アーカイブ全体を読んで検証する前に（名前の一覧だけで）確かめる。
        if count_segment_files(dir)? > MAX_PITR_SEGMENTS {
            return Err(IncrementalError::BadSegmentList);
        }
        let expect = ArchiveExpectation {
            db_id: Some(base.db_id),
            timeline: Some(base.timeline),
            start_lsn: None,
        };
        let (_, segments) = verified_segments(dir, &expect)?;
        for (path, info) in segments {
            if info.header.last_lsn <= covered_end {
                continue;
            }
            if info.header.storage_format != STORAGE_FORMAT_VERSION {
                return Err(IncrementalError::UnsupportedFormat);
            }
            archive.push(ReplaySegment {
                path,
                info,
                overlap_ok: true,
            });
        }
        // 続きが欠けなく始まること（ベース・増分の終点の次を覆う先頭のセグメントがある）。
        if let Some(first) = archive.first() {
            let next = covered_end
                .checked_add(1)
                .ok_or(IncrementalError::ChainBroken)?;
            if first.info.header.first_lsn > next {
                return Err(IncrementalError::ChainBroken);
            }
        }
    }

    // 2. 目標を停止 LSN に解決する。
    let stop = match target {
        PitrTarget::Lsn(n) => n,
        PitrTarget::TimeMicros(t) => {
            let all: Vec<&ReplaySegment> = list.iter().chain(archive.iter()).collect();
            resolve_time(&all, base.end_lsn, t, max_payload)?
        }
    };
    // 停止点より後ろのセグメントは使わない（ディレクトリ全体の検証は済んでいる）。
    archive.retain(|r| r.info.header.first_lsn <= stop);
    list.extend(archive);
    if list.is_empty() {
        return Err(IncrementalError::TargetOutOfRange);
    }
    if list.len() > MAX_PITR_SEGMENTS {
        return Err(IncrementalError::BadSegmentList);
    }

    // 3. 再生（停止 LSN の範囲検査・連鎖の検査・チェックサムの照合は再生器）。
    replay_segments(db_path, &list, Some(stop))
}

/// コミット時刻が `t` を超える最初のレコードの直前の LSN を求める（ヘッダだけを読む）。ベースの終点
/// `base_end` 以下のレコードと、既に見た LSN 以下のレコード（アーカイブが増分と重なる区間）は数えない。
/// 読むのはヘッダだけで、全体チェックサム（`finish`）は照合しない。安全の根拠は、(1) 呼び出し前に
/// アーカイブ全体を検証済み、(2) 結果は停止 LSN にすぎず、再生（`replay_segments`）が各セグメントの
/// ヘッダ・全体チェックサムと、重なり区間の内容（適用先の `change_log` の行）を再検証することにある。
/// 該当するレコードが無い（末尾まで行く）、または最初の候補がすでに `t` を超えている（停止点がベース以前）
/// なら `TargetOutOfRange`。
fn resolve_time(
    segments: &[&ReplaySegment],
    base_end: u64,
    t: i64,
    max_payload: usize,
) -> Result<u64> {
    let mut seen = base_end;
    for seg in segments {
        let mut reader = SegmentReader::open(&seg.path, max_payload)?;
        if *reader.header() != seg.info.header {
            return Err(ArchiveError::ChecksumMismatch.into());
        }
        while let Some(raw) = reader.next_record()? {
            let h = peek_record(&raw, max_payload)
                .map_err(super::archive::map_decode)
                .map_err(IncrementalError::from)?;
            if h.lsn <= seen {
                // 重なり区間。内容の照合は再生側が行う（ここでは連鎖の形だけ確かめる）。
                if h.lsn.checked_sub(1) != Some(h.prev_lsn) {
                    return Err(IncrementalError::ChainBroken);
                }
                continue;
            }
            if h.commit_time_us > t {
                // 直前の LSN。ベース以前（= 適用するレコードが 0 件）なら範囲外。
                let stop = h.lsn.checked_sub(1).ok_or(IncrementalError::ChainBroken)?;
                if stop <= base_end {
                    return Err(IncrementalError::TargetOutOfRange);
                }
                return Ok(stop);
            }
            seen = h.lsn;
        }
    }
    Err(IncrementalError::TargetOutOfRange)
}

#[cfg(test)]
mod tests;
