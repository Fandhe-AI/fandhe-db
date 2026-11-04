//! `hnsw.rs::NodeVectors::I8` 専用の per-search `NodeSource` 実装
//! （Issue #522・親 #520・前提 #521。ポインタ: TASK-132・TASK-156・
//! CORE-16）。
//!
//! `hnsw.rs::HnswIndex::search_masked_with_hop` が探索の冒頭で 1 回だけ
//! [`PreparedI8Source::new`] を呼び、`crate::sq8::prepare_query` によるクエリの
//! 二重量子化（モジュール `sq8.rs` 冒頭「整数 i8×i8 dot」節参照）を 1 回だけ
//! 行ってから、探索本体（`greedy_descend_masked`／`search_layer_with_hop`）へ
//! `&dyn super::NodeSource` として渡す。`prepare_query` が失敗した場合
//! （次元過大・スケール溢れ）は [`I8QueryPlan::Dequant`]（既存の
//! `sq8::dot_i8_f32` 復号 dot）へ fail-closed に縮退し、索引の再構築や
//! 空集合の誤返却を招かない。
//!
//! `Integer` 経路の実際のカーネル選択は `isa::current_i8()` の実行時検出に
//! 委ねる（x86_64: VNNI／i16 widen、aarch64: NEON dotprod〔Issue #525〕、
//! いずれも未対応なら Scalar）。本モジュールは ISA を意識しない。

use std::sync::Arc;

use crate::isa::{self, I8QueryOperands};
use crate::sq8::{self, Sq8DimParams, Sq8QueryCodes};

use super::metric::finite_score;
use super::{node_vector_i8, HnswError, HnswMetric, NodeSource};

/// [`PreparedI8Source`] が使うスコア計算方式。
enum I8QueryPlan {
    /// `crate::sq8::prepare_query` が成功した場合の整数 dot 経路。
    Integer(Sq8QueryCodes),
    /// `prepare_query` が失敗した場合の復号 dot 縮退経路（既存
    /// `NodeVectors::score` の I8 分岐と同じ計算）。
    Dequant,
}

/// 準備済みクエリを保持する per-search の `NodeSource` 実装。
pub(super) struct PreparedI8Source<'a> {
    codes: &'a [i8],
    row_sums: &'a [i32],
    params: &'a Sq8DimParams,
    /// この探索が使うクエリの参照（`ptr::eq` による同一性検査に使う。
    /// §構造体ドキュメンテーションコメント参照）。
    source_query: &'a [f32],
    kernel: isa::I8Kernel,
    plan: I8QueryPlan,
}

impl<'a> PreparedI8Source<'a> {
    /// `codes`・`row_sums`・`params` は `hnsw.rs::NodeVectors::I8` が保持する
    /// 3 つ組（`freeze_from` が同時に生成し寿命・対応関係が一致する——
    /// `NodeVectors::I8` ドキュメンテーションコメント参照）。`query` は
    /// この探索呼び出し（`search_masked_with_hop`）の間、変わらないことを
    /// [`NodeSource::score`] が `ptr::eq` で検査する。
    pub(super) fn new(
        codes: &'a [i8],
        row_sums: &'a Arc<[i32]>,
        params: &'a Arc<Sq8DimParams>,
        query: &'a [f32],
    ) -> Self {
        let kernel = isa::current_i8();
        let plan = match sq8::prepare_query(params, query) {
            Ok(qc) => I8QueryPlan::Integer(qc),
            // `prepare_query` の失敗（次元過大・スケール溢れ）はこのクエリに
            // 限った fail-closed 縮退。索引・row_sums はそのまま使い続ける。
            Err(_) => I8QueryPlan::Dequant,
        };
        Self {
            codes,
            row_sums,
            params,
            source_query: query,
            kernel,
            plan,
        }
    }
}

impl NodeSource for PreparedI8Source<'_> {
    fn score(
        &self,
        metric: HnswMetric,
        dim: usize,
        node: u32,
        query: &[f32],
    ) -> Result<f32, HnswError> {
        // `search_masked_with_hop` は本ソースを構築した直後の 1 回の呼び出し
        // 内でのみ使い、そのすべての `score` 呼び出しに同一の `query`
        // スライス（同一アドレス・同一長）を渡す契約（`greedy_descend_masked`・
        // `search_layer_in` はいずれも呼び出し元から受け取った `query` を
        // そのまま下流へ渡すのみで複製・変更しない）。この契約が破られる
        // （準備済みクエリと異なるクエリで呼ばれる）と、`Integer` 経路の
        // `Sq8QueryCodes` が別クエリの近似値になり、静かに誤ったスコアを
        // 返しかねない——`ptr::eq`（アドレス＋長さの比較。スライスに対する
        // `std::ptr::eq` はメタデータも比較する）で検出し fail-closed に
        // 拒否する。
        if !std::ptr::eq(query, self.source_query) {
            return Err(HnswError::InvalidParams {
                reason: "query does not match the prepared i8 query",
            });
        }
        let row = node_vector_i8(self.codes, dim, node)?;
        let score =
            match &self.plan {
                I8QueryPlan::Integer(qc) => {
                    let row_sum = self.row_sums.get(node as usize).copied().ok_or(
                        HnswError::DimMismatch {
                            dim: dim as u32,
                            len: self.row_sums.len(),
                        },
                    )?;
                    let operands = I8QueryOperands {
                        signed: &qc.signed,
                        shifted: &qc.shifted,
                    };
                    // 整数オペランドの準備は内積専用。Cosine は正規化済み内積なので同じ腕。
                    // L2・L1 は凍結時に F32 へ縮退するため到達しない（防御として NaN →
                    // `finite_score` で fail-closed に拒否する）。
                    match metric {
                        HnswMetric::InnerProduct | HnswMetric::Cosine => {
                            let int_dot = self.kernel.dot_i8(row, row_sum, operands);
                            sq8::score_from_int_dot(int_dot, qc.scale_q)
                        }
                        HnswMetric::L2 | HnswMetric::L1 => f32::NAN,
                    }
                }
                I8QueryPlan::Dequant => metric.score_sq8_dequant(row, self.params.scales(), query),
            };
        finite_score(score, node)
    }

    fn touch_prefetch(&self, dim: usize, node: u32) {
        super::prefetch::touch_node_vector_i8(self.codes, dim, node);
    }
}
