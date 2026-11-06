//! SQL／NoSQL 検索実行器（`sql::exec`）のリランク段の純粋関数部分（Issue #2142。
//! 設計記録: `docs/design/rerank-wiring-policy.md` D3・D4・D6。ビヘイビア: SEARCH-15 を
//! ポインタで参照）。
//!
//! `exec.rs::execute_statement_with_cache_observed` が、RLS 暗黙適用後・事後 SCALAR フィルタ後の
//! ハイブリッド候補（スロット番号と融合スコアの列）と、窓内の行テキストをこの段へ渡す。
//! [`rerank_window`] は storage・catalog・RLS を一切知らず、`rerank::rerank_candidates` の入出力
//! 検証を経由して「並べ替え後の列」または「縮退理由」を返すだけにとどめる（テストしやすさと、
//! 候補の可視性判定を上位層に閉じる境界を保つため）。[`fetch_window_texts`] だけが、呼び出し元が
//! RLS 適用後に絞り込んだ窓内のスロットについて、アリーナのテナント・行 id をキーにテキスト列を
//! 読む（認可済みの行・既にハイブリッド検索が参照する列に限り、列権限を広げない）。
//!
//! 縮退は検索全体の拒否ではなく「リランキング前の順位で応答する」ことを意味するが、その復元
//! （off と同一の集合へ戻す処理）は呼び出し元の責務。

use crate::arena::VectorArena;
use crate::catalog::TableSchema;
use crate::rerank::{
    rerank_candidates, CrossEncoderError, RerankCandidate, RerankConfig, RerankCounters,
    RerankDegradeReason, RerankError, RerankInput, RerankSettings, Reranker,
    MAX_CANDIDATE_TEXT_BYTES, MAX_TOTAL_CANDIDATE_TEXT_BYTES,
};
use crate::row_codec;
use crate::storage::read_source::ReadSource;

/// 実行器へ渡すリランク設定と統計の束。`EngineCore::run_select_plan` が構築して
/// `execute_statement_with_cache(_observed)` へ渡す（`None` ならリランク経路を通らない）。
#[derive(Clone, Copy)]
pub(crate) struct RerankAccess<'a> {
    pub(crate) settings: &'a RerankSettings,
    pub(crate) counters: &'a RerankCounters,
}

/// 縮退を文の実行ログへ 1 行記録する。固定の英語メッセージと閉じた語彙（理由・種別）だけを
/// 出し、id・本文・表名・テナント情報は含めない（ADR D7）。
pub(crate) fn log_degraded(reason: RerankDegradeReason, kind: &'static str) {
    crate::log_stderr!(
        "rerank: degraded to unreranked order (reason={}, kind={})",
        reason.as_str(),
        kind
    );
}

/// リランク段の結果。
#[derive(Debug, PartialEq)]
pub(crate) enum StageOutcome {
    /// リランクを適用した列（`k_eff` で切り詰め済み。各行のスコアは融合スコアのまま）。
    Applied(Vec<(u64, f64)>),
    /// リランク対象の行（テキストを持つ行）が無く、順位を変えずに素通しした列。
    Unchanged,
    /// 契約違反・上限超過などで縮退する。呼び出し元が off と同一の列へ戻す。
    Degraded(RerankDegradeReason),
}

/// `RerankError` を縮退理由の閉じた語彙へ写す（ADR D6・D7）。
pub(crate) fn degrade_reason(err: &RerankError) -> RerankDegradeReason {
    match err {
        RerankError::QueryTextTooLong { .. }
        | RerankError::CandidateTextTooLong { .. }
        | RerankError::TotalCandidateTextTooLong { .. } => RerankDegradeReason::InputLimit,
        RerankError::CrossEncoder(CrossEncoderError::Timeout) => RerankDegradeReason::Timeout,
        RerankError::CrossEncoder(_) => RerankDegradeReason::BackendError,
        _ => RerankDegradeReason::ContractViolation,
    }
}

/// 窓内の 1 行分のリランカー入力（Issue #2143）。`text` は `RerankInput::Text` の方式だけが、
/// `embedding` は `RerankInput::Embedding` の方式だけが読む（読まない側は空／`None`）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RowInput<'a> {
    pub(crate) text: &'a str,
    pub(crate) embedding: Option<&'a [f32]>,
}

/// 窓内の行をリランクし、窓より後ろの行を元の順に連結して `k_eff` で切り詰めた列を返す。
///
/// - `hits`: スコア降順・同点スロット昇順の候補（`(スロット番号, スコア)`）。密のみの文は
///   呼び出し元（[`run_stage`]）が位置由来の人工スコアへ置き換えて渡す。
/// - `rows`: 窓内の各行の入力。長さは `min(window, hits.len())` でなければならない
///   （不一致は契約違反として縮退）。`None`（対象外の行。NULL・デコード失敗・埋め込み無し）の
///   行は元の位置に固定する。
/// - 出力は `rerank_candidates` が検証し、さらに件数が `final_k` ちょうどであることを
///   本関数で検査する（欠落も契約違反）。
pub(crate) fn rerank_window(
    reranker: &dyn Reranker,
    query_text: &str,
    hits: &[(u64, f64)],
    rows: &[Option<RowInput<'_>>],
    window: usize,
    k_eff: usize,
) -> StageOutcome {
    let w_eff = window.min(hits.len());
    if rows.len() != w_eff {
        return StageOutcome::Degraded(RerankDegradeReason::ContractViolation);
    }
    let (win, rest) = hits.split_at(w_eff);

    // 入力を持つ行（リランク対象 F）の窓内位置。
    let positions: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.as_ref().map(|_| i))
        .collect();
    let final_k = k_eff.min(w_eff).min(positions.len());
    if final_k == 0 {
        return StageOutcome::Unchanged;
    }

    let mut candidates: Vec<RerankCandidate<'_>> = Vec::with_capacity(positions.len());
    for &pos in &positions {
        let (Some(&(id, score)), Some(Some(row))) = (win.get(pos), rows.get(pos)) else {
            return StageOutcome::Degraded(RerankDegradeReason::ContractViolation);
        };
        candidates.push(RerankCandidate {
            id,
            fused_score: score,
            text: row.text,
            embedding: row.embedding,
        });
    }
    let cfg = match RerankConfig::new(candidates.len(), final_k) {
        Ok(c) => c,
        Err(e) => return StageOutcome::Degraded(degrade_reason(&e)),
    };
    let out = match rerank_candidates(reranker, query_text, &candidates, &cfg) {
        Ok(o) => o,
        Err(e) => return StageOutcome::Degraded(degrade_reason(&e)),
    };
    if out.len() != final_k {
        return StageOutcome::Degraded(RerankDegradeReason::ContractViolation);
    }

    // 並べ替え後の列 = 出力 ++ 出力に入らなかった F の行（元の順）。スコアは融合スコアのまま。
    let score_of: std::collections::HashMap<u64, f64> =
        candidates.iter().map(|c| (c.id, c.fused_score)).collect();
    let out_ids: std::collections::HashSet<u64> = out.iter().map(|h| h.id).collect();
    let mut ordered: Vec<(u64, f64)> = Vec::with_capacity(candidates.len());
    for h in &out {
        let Some(&s) = score_of.get(&h.id) else {
            return StageOutcome::Degraded(RerankDegradeReason::ContractViolation);
        };
        ordered.push((h.id, s));
    }
    for c in &candidates {
        if !out_ids.contains(&c.id) {
            ordered.push((c.id, c.fused_score));
        }
    }

    // 窓の各位置へ詰め直す。テキストの無い行は元の位置に固定する。
    let mut ordered_iter = ordered.into_iter();
    let mut result: Vec<(u64, f64)> = Vec::with_capacity(hits.len());
    for (i, original) in win.iter().enumerate() {
        match rows.get(i) {
            Some(Some(_)) => match ordered_iter.next() {
                Some(row) => result.push(row),
                None => return StageOutcome::Degraded(RerankDegradeReason::ContractViolation),
            },
            _ => result.push(*original),
        }
    }
    result.extend_from_slice(rest);
    result.truncate(k_eff);
    StageOutcome::Applied(result)
}

/// [`run_stage`] が行の入力（テキスト・埋め込み）の取得に使う、文の実行コンテキスト（借用のみ）。
pub(crate) struct StageSource<'a, R: ReadSource> {
    pub(crate) read_txn: &'a R,
    pub(crate) schema: &'a TableSchema,
    pub(crate) table: &'a str,
    pub(crate) arena: &'a VectorArena,
    /// アリーナがキャッシュのスナップショットそのものであるときの行 metadata。
    pub(crate) snapshot_metadata: Option<&'a [Vec<u8>]>,
    pub(crate) text_column_index: usize,
    pub(crate) query_text: &'a str,
    /// 密のみ（`Ranking::Distance`）の文か。入力のスコアが「小さいほど近い」距離なので、
    /// 位置由来の人工スコアへ置き換えてからリランカーへ渡し、結果は元の距離へ戻す。
    pub(crate) dense: bool,
}

/// [`run_stage`] の結果（Issue #2143 で密のみの再検索を追加）。
#[derive(Debug, PartialEq)]
pub(crate) enum StageResult {
    /// `k_eff` 以下の列。縮退したハイブリッドは off と同一の列に戻した結果。
    Rows(Vec<(u64, f64)>),
    /// 密のみの文が縮退した。広げた取得幅の結果は off と同一ではないため、呼び出し元が
    /// `k_eff` で DISTANCE 段をやり直す（ADR D6）。
    RefetchOff,
}

/// 窓内スロットの埋め込みを借用で取り出す。NULL（空スライス）・次元不一致・ノルム² が 0 以下
/// または非有限の行は `None`（元の位置に固定される）。アリーナは RLS 適用後の可視行だけを
/// 持つため、他テナントの行はここへ到達しない。
pub(crate) fn window_embeddings<'a>(
    arena: &'a VectorArena,
    window_slots: &[u64],
) -> Vec<Option<&'a [f32]>> {
    let dim = usize::try_from(arena.dim()).unwrap_or(0);
    window_slots
        .iter()
        .map(|&slot_id| {
            let slot = usize::try_from(slot_id).ok()?;
            let e = arena.vector_or_null(slot)?;
            if e.is_empty() || e.len() != dim {
                return None;
            }
            let n = crate::kernel::dot(e, e);
            (n.is_finite() && n > 0.0).then_some(e)
        })
        .collect()
}

/// リランク段の本体。`hits` は RLS 暗黙適用後・事後 SCALAR フィルタ後の候補で、長さは
/// `k_eff` 以上になりうる（窓のため広く取っている）。戻り値は `k_eff` 以下の列で、
/// ハイブリッドの縮退時は `off_prefix`（off が返したはずの先頭 `k_eff` 件のスロット集合）に
/// 属する行だけの元の順位の列に戻す（ADR D6。off と同一の結果）。密のみ（`off_prefix` が
/// `None`）の縮退は [`StageResult::RefetchOff`] を返し、再検索を呼び出し元に委ねる。
/// 件数を統計へ、縮退を実行ログへ記録する。
pub(crate) fn run_stage<R: ReadSource>(
    access: RerankAccess<'_>,
    src: &StageSource<'_, R>,
    mut hits: Vec<(u64, f64)>,
    k_eff: usize,
    off_prefix: Option<&std::collections::HashSet<u64>>,
) -> StageResult {
    let method = access.settings.method();
    let Some(reranker) = method.reranker() else {
        hits.truncate(k_eff);
        return StageResult::Rows(hits);
    };
    let window = access.settings.window();
    let w_eff = window.min(hits.len());
    let slots: Vec<u64> = hits.iter().take(w_eff).map(|h| h.0).collect();

    // 密のみの距離は昇順（小さいほど近い）で、`rerank_candidates` の入力契約（スコア降順）に
    // 合わない。位置由来の狭義単調減少値を渡し、元の距離はスロットで引いて戻す。
    let originals: Option<std::collections::HashMap<u64, f64>> = src
        .dense
        .then(|| hits.iter().map(|&(slot, d)| (slot, d)).collect());
    let artificial: Vec<(u64, f64)>;
    let work: &[(u64, f64)] = if src.dense {
        let n = hits.len();
        artificial = hits
            .iter()
            .enumerate()
            .map(|(i, &(slot, _))| (slot, n.saturating_sub(i) as f64))
            .collect();
        &artificial
    } else {
        &hits
    };

    let outcome = match method.required_input() {
        RerankInput::Text => match fetch_window_texts(
            src.read_txn,
            src.schema,
            src.table,
            src.arena,
            src.snapshot_metadata,
            src.text_column_index,
            &slots,
        ) {
            Ok(texts) => {
                let rows: Vec<Option<RowInput<'_>>> = texts
                    .iter()
                    .map(|t| {
                        t.as_deref().map(|text| RowInput {
                            text,
                            embedding: None,
                        })
                    })
                    .collect();
                rerank_window(reranker, src.query_text, work, &rows, window, k_eff)
            }
            Err(reason) => StageOutcome::Degraded(reason),
        },
        RerankInput::Embedding => {
            // 行テーブルのテキスト列は読まない（デコードコストを避ける）。
            let embs = window_embeddings(src.arena, &slots);
            let rows: Vec<Option<RowInput<'_>>> = embs
                .iter()
                .map(|e| {
                    e.map(|embedding| RowInput {
                        text: "",
                        embedding: Some(embedding),
                    })
                })
                .collect();
            let q = if src.dense { "" } else { src.query_text };
            rerank_window(reranker, q, work, &rows, window, k_eff)
        }
    };
    match outcome {
        StageOutcome::Applied(rows) => {
            let restored = match &originals {
                None => Some(rows),
                Some(orig) => rows
                    .into_iter()
                    .map(|(slot, _)| orig.get(&slot).map(|&d| (slot, d)))
                    .collect::<Option<Vec<_>>>(),
            };
            match restored {
                Some(rows) => {
                    access.counters.record_applied();
                    StageResult::Rows(rows)
                }
                None => degrade(
                    access,
                    method.kind_name(),
                    RerankDegradeReason::ContractViolation,
                    hits,
                    off_prefix,
                ),
            }
        }
        StageOutcome::Unchanged => {
            hits.truncate(k_eff);
            StageResult::Rows(hits)
        }
        StageOutcome::Degraded(reason) => {
            degrade(access, method.kind_name(), reason, hits, off_prefix)
        }
    }
}

/// 縮退の記録（統計・ログ）と結果の決定。ハイブリッドは `off_prefix` で off と同一の列へ戻し、
/// 密のみは再検索を要求する。
fn degrade(
    access: RerankAccess<'_>,
    kind: &'static str,
    reason: RerankDegradeReason,
    mut hits: Vec<(u64, f64)>,
    off_prefix: Option<&std::collections::HashSet<u64>>,
) -> StageResult {
    access.counters.record_degraded(reason);
    log_degraded(reason, kind);
    match off_prefix {
        Some(prefix) => {
            hits.retain(|h| prefix.contains(&h.0));
            StageResult::Rows(hits)
        }
        None => StageResult::RefetchOff,
    }
}

/// 窓内スロットのテキスト列を 1 件ずつ取り出す。戻り値は `window_slots` と同じ長さで、
/// NULL・行欠落・デコード失敗・テナント不整合の行は `None`（その行だけ元の位置に固定される）。
///
/// - `snapshot_metadata` が `Some` のとき（キャッシュヒットでアリーナがスナップショットそのもの）は
///   その行 metadata を使い、`None` のときは `read_txn` の行テーブルをテナント・行 id で引く。
///   書き込みトランザクション由来でも `ReadSource` 経由で未 commit 行を読める。
/// - 1 件 [`MAX_CANDIDATE_TEXT_BYTES`]・累計 [`MAX_TOTAL_CANDIDATE_TEXT_BYTES`] を確保前に検査し、
///   超えたら `Err(InputLimit)`（縮退）。
pub(crate) fn fetch_window_texts<R: ReadSource>(
    read_txn: &R,
    schema: &TableSchema,
    table: &str,
    arena: &VectorArena,
    snapshot_metadata: Option<&[Vec<u8>]>,
    text_column_index: usize,
    window_slots: &[u64],
) -> Result<Vec<Option<String>>, RerankDegradeReason> {
    use redb::ReadableTable;

    let mut mask = vec![false; schema.columns.len()];
    match mask.get_mut(text_column_index) {
        Some(m) => *m = true,
        None => return Ok(vec![None; window_slots.len()]),
    }
    let rows_table = match snapshot_metadata {
        Some(_) => None,
        None => match read_txn.open_table(crate::catalog::user_rows_table_def(
            &crate::catalog::user_rows_table_name(table),
        )) {
            Ok(t) => Some(t),
            // 行テーブルが無ければ候補も存在しない。全行を固定扱いにする。
            Err(_) => return Ok(vec![None; window_slots.len()]),
        },
    };

    // 上限検査（確保前）と複製を 1 箇所に集約する。借用元（redb のガード）が生きている
    // スコープ内で呼ぶ。
    let mut total: usize = 0;
    let mut take = |text: Option<&str>| -> Result<Option<String>, RerankDegradeReason> {
        let Some(t) = text else {
            return Ok(None);
        };
        if t.len() > MAX_CANDIDATE_TEXT_BYTES {
            return Err(RerankDegradeReason::InputLimit);
        }
        total = total
            .checked_add(t.len())
            .filter(|n| *n <= MAX_TOTAL_CANDIDATE_TEXT_BYTES)
            .ok_or(RerankDegradeReason::InputLimit)?;
        Ok(Some(t.to_string()))
    };

    let mut out: Vec<Option<String>> = Vec::with_capacity(window_slots.len());
    for &slot_id in window_slots {
        let Ok(slot) = usize::try_from(slot_id) else {
            out.push(None);
            continue;
        };
        let fetched = match (&rows_table, snapshot_metadata) {
            (_, Some(meta)) => take(
                meta.get(slot)
                    .and_then(|m| text_of(schema, m, &mask, text_column_index)),
            )?,
            (Some(t), None) => {
                let (Some(tenant), Some(rowid)) = (arena.tenant_id(slot), arena.row_id(slot))
                else {
                    out.push(None);
                    continue;
                };
                let guard = match t.get(crate::storage::rowid::row_key(tenant, rowid)) {
                    Ok(Some(g)) => g,
                    _ => {
                        out.push(None);
                        continue;
                    }
                };
                let buf = guard.value();
                let tenant_ok = crate::storage::decode_row_tenant_and_visibility(buf)
                    .ok()
                    .is_some_and(|(header_tenant, _)| {
                        crate::storage::verify_row_key_tenant(tenant, header_tenant).is_ok()
                    });
                if tenant_ok {
                    let text = crate::storage::decode_row_metadata_borrowed(buf)
                        .ok()
                        .and_then(|m| text_of(schema, m, &mask, text_column_index));
                    take(text)?
                } else {
                    None
                }
            }
            (None, None) => None,
        };
        out.push(fetched);
    }
    Ok(out)
}

/// 行 metadata からテキスト列 1 本の `&str` を借用して取り出す（NULL・非テキスト・
/// デコード失敗は `None`）。複製は呼び出し元が上限検査の後に行う。
fn text_of<'a>(
    schema: &'a TableSchema,
    metadata: &'a [u8],
    mask: &[bool],
    text_column_index: usize,
) -> Option<&'a str> {
    let scanned = row_codec::scan_scalar_columns_masked(schema, metadata, Some(mask)).ok()?;
    scanned
        .get(text_column_index)
        .copied()
        .flatten()
        .and_then(|v| v.as_text())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rerank::{IdentityReranker, RerankedHit};

    fn hits(n: u64) -> Vec<(u64, f64)> {
        (0..n).map(|i| (i, 1.0 - (i as f64) * 0.01)).collect()
    }

    fn texts(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|t| t.map(str::to_string)).collect()
    }

    /// テキスト列を窓の入力へ変換して `rerank_window` を呼ぶ。
    fn run(
        r: &dyn Reranker,
        h: &[(u64, f64)],
        t: &[Option<String>],
        window: usize,
        k: usize,
    ) -> StageOutcome {
        let rows: Vec<Option<RowInput<'_>>> = t
            .iter()
            .map(|x| {
                x.as_deref().map(|text| RowInput {
                    text,
                    embedding: None,
                })
            })
            .collect();
        rerank_window(r, "q", h, &rows, window, k)
    }

    /// 固定の出力を返す（契約違反の注入用）。
    struct Fixed(Vec<RerankedHit>);
    impl Reranker for Fixed {
        fn rerank(
            &self,
            _q: &str,
            _c: &[RerankCandidate<'_>],
            _k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn cross_encoder_timeout_maps_to_timeout_and_others_to_backend_error() {
        assert_eq!(
            degrade_reason(&RerankError::CrossEncoder(CrossEncoderError::Timeout)),
            RerankDegradeReason::Timeout
        );
        assert_eq!(
            degrade_reason(&RerankError::CrossEncoder(CrossEncoderError::Backend(
                "x".to_string()
            ))),
            RerankDegradeReason::BackendError
        );
    }

    #[test]
    fn identity_keeps_order_and_truncates() {
        let h = hits(5);
        let t = texts(&[Some("a"), Some("b"), Some("c")]);
        let out = run(&IdentityReranker, &h, &t, 3, 4);
        assert_eq!(out, StageOutcome::Applied(h[..4].to_vec()));
    }

    #[test]
    fn rest_beyond_window_is_kept_in_order_when_k_exceeds_window() {
        let h = hits(6);
        let t = texts(&[Some("a"), Some("b")]);
        let out = run(&IdentityReranker, &h, &t, 2, 5);
        assert_eq!(out, StageOutcome::Applied(h[..5].to_vec()));
    }

    #[test]
    fn rows_without_text_stay_pinned() {
        // 窓 3 件の真ん中が NULL。逆順を返すリランカーでも位置 1 は動かない。
        let h = hits(3);
        let t = texts(&[Some("a"), None, Some("c")]);
        let rev = Fixed(vec![
            RerankedHit { id: 2, score: 0.9 },
            RerankedHit { id: 0, score: 0.8 },
        ]);
        let out = run(&rev, &h, &t, 3, 3);
        let StageOutcome::Applied(rows) = out else {
            panic!("expected applied");
        };
        let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
        assert_eq!(ids, vec![2, 1, 0]);
        // スコアは融合スコアのまま。
        assert_eq!(rows[0].1, h[2].1);
    }

    #[test]
    fn no_text_rows_means_unchanged() {
        let h = hits(3);
        let t = texts(&[None, None, None]);
        assert_eq!(
            run(&IdentityReranker, &h, &t, 3, 3),
            StageOutcome::Unchanged
        );
    }

    #[test]
    fn missing_output_is_contract_violation() {
        let h = hits(3);
        let t = texts(&[Some("a"), Some("b"), Some("c")]);
        let short = Fixed(vec![RerankedHit { id: 0, score: 0.5 }]);
        assert_eq!(
            run(&short, &h, &t, 3, 3),
            StageOutcome::Degraded(RerankDegradeReason::ContractViolation)
        );
    }

    #[test]
    fn foreign_and_duplicate_ids_are_contract_violation() {
        let h = hits(2);
        let t = texts(&[Some("a"), Some("b")]);
        let foreign = Fixed(vec![
            RerankedHit { id: 99, score: 0.9 },
            RerankedHit { id: 0, score: 0.5 },
        ]);
        assert_eq!(
            run(&foreign, &h, &t, 2, 2),
            StageOutcome::Degraded(RerankDegradeReason::ContractViolation)
        );
        let dup = Fixed(vec![
            RerankedHit { id: 0, score: 0.5 },
            RerankedHit { id: 0, score: 0.5 },
        ]);
        assert_eq!(
            run(&dup, &h, &t, 2, 2),
            StageOutcome::Degraded(RerankDegradeReason::ContractViolation)
        );
    }

    #[test]
    fn oversized_text_maps_to_input_limit() {
        let h = hits(1);
        let long = "x".repeat(crate::rerank::MAX_CANDIDATE_TEXT_BYTES + 1);
        let t = vec![Some(long)];
        assert_eq!(
            run(&IdentityReranker, &h, &t, 1, 1),
            StageOutcome::Degraded(RerankDegradeReason::InputLimit)
        );
    }

    #[test]
    fn embedding_rows_without_embedding_stay_pinned() {
        // 窓 4 件の位置 1 が埋め込み無し（NULL 行）。λ=0 の MMR でも位置 1 は動かない。
        let h = hits(4);
        let embs: [Option<Vec<f32>>; 4] = [
            Some(vec![1.0, 0.0]),
            None,
            Some(vec![0.99, 0.1]),
            Some(vec![0.0, 1.0]),
        ];
        let rows: Vec<Option<RowInput<'_>>> = embs
            .iter()
            .map(|e| {
                e.as_deref().map(|embedding| RowInput {
                    text: "",
                    embedding: Some(embedding),
                })
            })
            .collect();
        let mmr = crate::rerank::MmrReranker::new(0.0).unwrap();
        let out = rerank_window(&mmr, "", &h, &rows, 4, 4);
        let StageOutcome::Applied(rows) = out else {
            panic!("expected applied");
        };
        let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
        // 対象行 0・2・3 は多様性順 0, 3, 2 に並び、位置 1（id 1）は固定。
        assert_eq!(ids, vec![0, 1, 3, 2]);
    }

    #[test]
    fn embedding_input_missing_for_all_rows_means_unchanged() {
        let h = hits(2);
        let rows: Vec<Option<RowInput<'_>>> = vec![None, None];
        let mmr = crate::rerank::MmrReranker::new(0.5).unwrap();
        assert_eq!(
            rerank_window(&mmr, "", &h, &rows, 2, 2),
            StageOutcome::Unchanged
        );
    }

    #[test]
    fn texts_length_mismatch_is_contract_violation() {
        let h = hits(3);
        let t = texts(&[Some("a")]);
        assert_eq!(
            run(&IdentityReranker, &h, &t, 3, 3),
            StageOutcome::Degraded(RerankDegradeReason::ContractViolation)
        );
    }
}
