//! 性能計測プロトコル基盤（TASK-158。ポインタ: `docs/spec/05-tasks.md` TASK-158）。
//!
//! TASK-127（性能・Recall 受け入れ基準の回帰ベンチ）・TASK-130（GPU vs CPU-SIMD A/B 回帰）・TASK-83 等、
//! 性能系の受け入れ基準を測定・再測定するタスクは、独自に計測ループを書かず
//! 必ず本モジュール経由で計測する契約とする。
//!
//! [`accept`] は測定結果（p95・Recall・対照比）を数値基準と突き合わせる時間非依存の
//! 判定ヘルパを提供する（TASK-127）。計測そのもの（`protocol`・`ab`）とは責務を分け、
//! `tests/bench_accept.rs` から実測タイマーに依存せず単体検証できるようにしている。
//!
//! [`contrast`]（`contrast-bench` feature 限定）は CORE-5（対照エンジン比較・
//! Issue #176）の対照エンジンアダプタを提供する。`contrast_bench.rs` からのみ
//! 呼ばれ、`simd_bench.rs`（CORE-3/CORE-4）・`accept.rs` の他判定関数からは
//! 独立している（対照エンジン側の障害〔C++ FFI 含む〕が CORE-3/CORE-4 のゲートへ
//! 波及しない failure domain 分離。`contrast.rs` 冒頭コメント参照）。
//!
//! 対象ビヘイビア ID なし（基盤タスク。CORE-3〜6・SEARCH-4・SQL-1 等の測定条件を
//! 担保する下支え）。
//!
//! # 利用側との契約
//!
//! - **決定性**: 入力生成は [`rng::DeterministicRng`] を通し、同一シードから常に
//!   同一の測定入力を再生成できること。
//! - **fail-closed**: プロトコル下限（warmup・計測回数）を回避できる構築経路を
//!   設けない。[`protocol::MeasurementConfig::new`] は下限未満を `Err` で拒否する。
//! - **同期完了の責務**: [`protocol::run`]・[`ab::run_ab`] に渡す `workload` は
//!   呼び出し元が 1 回分の作業を同期的に完了させること（本基盤は完了同期を検証しない）。
//! - **統合テストからの取り込み**: `cargo bench` 経由のベンチ入口だけでなく
//!   `tests/bench_harness.rs` からも `#[path = "../benches/harness/mod.rs"]` で
//!   同一ソースを取り込み、`cargo test` でプロトコル遵守の回帰を検証する
//!   （crates/engine が内部モジュールを 2 経路から共有する構成。新規クレートは
//!   追加しない）。
//!
//! # 暗号用途禁止
//!
//! 本モジュール群の RNG（[`rng::DeterministicRng`]）は非暗号 PRNG である。
//! ベンチ・テストの入力生成専用とし、鍵・トークン等のセキュリティ用途に転用しない
//! （OWASP A02）。

// クレートルート直下の再 export は置かない: 本モジュールは `cargo bench`
// バイナリ（`benches/measurement.rs`）と統合テスト（`tests/bench_harness.rs`）の
// 2 つの独立したコンパイル単位から `#[path]` で取り込まれ、双方が使う識別子の
// 集合が異なる。`pub use` でまとめて再 export すると、一方の単位でしか使わない
// 識別子が他方で unused import として `-D warnings` に引っかかるため、
// 各利用側はサブモジュール経由（`harness::protocol::...` 等）で必要な識別子のみ
// 個別に import する。
pub mod ab;
pub mod accept;
pub mod bench_engine;
pub mod chip;
#[cfg(feature = "contrast-bench")]
pub mod contrast;
pub mod dot_block;
pub mod dot_kernel;
pub mod env_report;
pub mod gpu_scaling;
pub mod hybrid_latency;
// Issue #387 PR #416 codex-review P2 指摘対応（2 巡目）: `hybrid_profile` の
// 大半（コーパス生成・SQL 文組み立て・tokenize 複製・fail-closed 判定等）は
// `engine::hybrid::sparse_refetch_observed`（非既定 feature `bench-internals`
// 限定・`crates/engine/src/hybrid.rs` 参照）に依存しない時間非依存の複製
// ロジックであり、既定 feature の `cargo test -p fandhe-vector-db-engine` でも検証すべき対象
// のため、モジュール自体は無条件でコンパイルする。`sparse_refetch_observed`
// に依存する関数（`sparse_refetch_schedule`）のみモジュール内で個別に
// `#[cfg(feature = "bench-internals")]` を付ける。
pub mod hnsw_build;
pub mod hnsw_compare;
pub mod hnsw_parallel_profile;
pub mod hnsw_search_latency;
pub mod hybrid_profile;
pub mod ingest_profile;
pub mod knn_profile;
pub mod parse_bind;
pub mod proc_stats;
pub mod protocol;
pub mod relational_p95;
pub mod rng;
pub mod scalar_reference;
pub mod scan_stage_profile;
pub mod sql_c1;
pub mod stats;
pub mod tier;
