//! 実行計測フック（Issue #2053・SQL-76 (2)(3) のポインタ）。
//!
//! EXPLAIN ANALYZE 系（親 Issue #2052）が使う「段ごとの出力行数・ループ回数・経過時間」の
//! 収集型 [`ExecStats`] と、実行器（`sql::exec` の kernel 経路・`sql::scan` の scan 経路）が
//! 段境界で呼ぶ計測ヘルパを提供する。出力（`QUERY PLAN` 行・JSON キー）への配線は本モジュールの
//! 責務外で、`ANALYZE` の受理（`explain_options`）や DML・aggregate・JOIN の計測は後続 Issue
//! （#2054・#2055・#2056）が同じ型を使って足す。
//!
//! # 無効時のコスト
//! 実行器は `Option<&mut ExecStats>` を受け取り、`None`（通常の実行）では段境界ごとの
//! `is_some()` 分岐（文 1 回あたり高々 6 段）しか払わない。`Instant::now()` もヒープ確保もしない。
//! 行ごとのフック・カウンタは置かない。行数は段境界にある既存の `Vec` の長さ等から取る。
//!
//! # 挙動不変の契約
//! 計測コードは読み取りだけで infallible（`?` を持たない）。計測の有無で結果の行・順序・
//! エラー・キャッシュの効き方は変わらない。
//!
//! # テナント境界（SQL-76 (3)）
//! `rows_out` / `rows_removed_by_filter` は RLS 暗黙適用の後の可視行からだけ取る。他テナントを
//! 含む走査件数は型にも変数にも持たない。経過時間は性質上データ量で変わりうるが、クエリの
//! 応答時間そのものが同じ情報を外へ出しており新しい漏えい路ではない。`timing == false` では
//! 時間を取らない。

use std::time::{Duration, Instant};

/// 計測する段の閉じた語彙。固定トークンだけを持ち、入力由来の文字列は持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // 読む側（#2054／#2055／#2056）が未配線。
pub(crate) enum ExecStage {
    /// kernel 経路: 候補の構築。scan 経路: 可視判定・WHERE・走査ループ。
    Scan,
    /// kernel 経路の DISTANCE 段。
    Distance,
    /// DISTANCE 後の SCALAR 事後適用（適用したときだけ記録）。
    PostFilter,
    /// precision ゲート（precision のときだけ記録）。
    PrecisionGate,
    /// RLS 安全網通過後の OFFSET・行数上限。
    Limit,
    /// 投影。
    Project,
}

impl ExecStage {
    /// 段の総数（`ExecStats` の固定長配列の長さ）。
    pub(crate) const COUNT: usize = 6;

    const fn index(self) -> usize {
        match self {
            ExecStage::Scan => 0,
            ExecStage::Distance => 1,
            ExecStage::PostFilter => 2,
            ExecStage::PrecisionGate => 3,
            ExecStage::Limit => 4,
            ExecStage::Project => 5,
        }
    }

    /// 固定トークン（EXPLAIN 語彙に揃える）。
    #[allow(dead_code)]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            ExecStage::Scan => "Scan",
            ExecStage::Distance => "Distance",
            ExecStage::PostFilter => "PostFilter",
            ExecStage::PrecisionGate => "PrecisionGate",
            ExecStage::Limit => "Limit",
            ExecStage::Project => "Project",
        }
    }
}

/// 1 段の実測値。後続が SQL-76 (6) の JSON キーへ 1:1 で写せる形。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub(crate) struct StageStats {
    /// 段が出力した行数（RLS 適用後の可視行だけ）。
    pub(crate) rows_out: u64,
    /// 段が実行された回数。
    pub(crate) loops: u64,
    /// 累計経過時間。`timing == false` では `None`。
    pub(crate) elapsed: Option<Duration>,
    /// フィルタで除かれた行数。既存の長さの差から出せる段だけ `Some`。
    pub(crate) rows_removed_by_filter: Option<u64>,
}

/// 段ごとの実測値の収集器。固定長配列で、確保量は型で決まる。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ExecStats {
    timing: bool,
    stages: [Option<StageStats>; ExecStage::COUNT],
}

#[allow(dead_code)]
impl ExecStats {
    /// 空の収集器を作る。`timing == false` なら `Instant::now()` を呼ばない。
    pub(crate) fn new(timing: bool) -> Self {
        Self {
            timing,
            stages: [None, None, None, None, None, None],
        }
    }

    /// 段の実測値（未記録なら `None`）。
    pub(crate) fn stage(&self, stage: ExecStage) -> Option<&StageStats> {
        self.stages.get(stage.index()).and_then(Option::as_ref)
    }

    /// 1 回の実行を積む。同じ段が複数回記録されたら `loops` を増やし、行数・時間は飽和加算する。
    fn record(
        &mut self,
        stage: ExecStage,
        started: Option<Instant>,
        rows_out: usize,
        removed: Option<usize>,
    ) {
        let rows_out = to_u64(rows_out);
        let removed = removed.map(to_u64);
        let elapsed = started.map(|t| t.elapsed());
        let Some(slot) = self.stages.get_mut(stage.index()) else {
            return;
        };
        match slot {
            None => {
                *slot = Some(StageStats {
                    rows_out,
                    loops: 1,
                    elapsed,
                    rows_removed_by_filter: removed,
                });
            }
            Some(s) => {
                s.rows_out = s.rows_out.saturating_add(rows_out);
                s.loops = s.loops.saturating_add(1);
                s.elapsed = match (s.elapsed, elapsed) {
                    (Some(a), Some(b)) => Some(a.saturating_add(b)),
                    (a, b) => a.or(b),
                };
                s.rows_removed_by_filter = match (s.rows_removed_by_filter, removed) {
                    (Some(a), Some(b)) => Some(a.saturating_add(b)),
                    (a, b) => a.or(b),
                };
            }
        }
    }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// 段の開始。無効時（`None`）・`timing == false` では `Instant::now()` を呼ばない。
#[inline]
pub(crate) fn stage_start(stats: &Option<&mut ExecStats>) -> Option<Instant> {
    match stats {
        Some(s) if s.timing => Some(Instant::now()),
        _ => None,
    }
}

/// 段の終了。無効時は何もしない。
#[inline]
pub(crate) fn stage_end(
    stats: &mut Option<&mut ExecStats>,
    stage: ExecStage,
    started: Option<Instant>,
    rows_out: usize,
    removed: Option<usize>,
) {
    if let Some(s) = stats {
        s.record(stage, started, rows_out, removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQL-76 (2): 同じ段の再記録で loops と行数が積まれ、飽和する。
    #[test]
    fn repeated_stage_accumulates_and_saturates() {
        let mut st = ExecStats::new(false);
        {
            let mut h = Some(&mut st);
            stage_end(&mut h, ExecStage::Scan, None, 3, Some(1));
            stage_end(&mut h, ExecStage::Scan, None, 4, Some(2));
        }
        let s = st.stage(ExecStage::Scan).expect("recorded");
        assert_eq!(
            (s.rows_out, s.loops, s.rows_removed_by_filter),
            (7, 2, Some(3))
        );
        st.stages[0].as_mut().expect("recorded").rows_out = u64::MAX;
        {
            let mut h = Some(&mut st);
            stage_end(&mut h, ExecStage::Scan, None, 1, None);
        }
        assert_eq!(
            st.stage(ExecStage::Scan).expect("recorded").rows_out,
            u64::MAX
        );
        assert!(st.stage(ExecStage::Distance).is_none());
    }

    /// timing=false では時間を取らず、true では取る。無効ハンドルでは何も起きない。
    #[test]
    fn timing_flag_controls_elapsed() {
        let mut off = ExecStats::new(false);
        {
            let mut h = Some(&mut off);
            let t = stage_start(&h);
            assert!(t.is_none());
            stage_end(&mut h, ExecStage::Project, t, 1, None);
        }
        assert!(off
            .stage(ExecStage::Project)
            .expect("recorded")
            .elapsed
            .is_none());

        let mut on = ExecStats::new(true);
        {
            let mut h = Some(&mut on);
            let t = stage_start(&h);
            assert!(t.is_some());
            stage_end(&mut h, ExecStage::Project, t, 1, None);
        }
        assert!(on
            .stage(ExecStage::Project)
            .expect("recorded")
            .elapsed
            .is_some());

        let mut none: Option<&mut ExecStats> = None;
        assert!(stage_start(&none).is_none());
        stage_end(&mut none, ExecStage::Scan, None, 1, None);
    }
}

#[cfg(test)]
mod observed_tests;
