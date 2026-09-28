//! 検索エンジンの選択・構築レイヤ（TASK-131・対象ビヘイビア: CORE-9）。
//!
//! `core.rs` の [`crate::core::EngineCore::open`] から呼ばれ、`Box<dyn SearchProvider>`
//! （`kernel.rs::SearchProvider`、CORE-13）を構築して返す。CORE-9 が指す「総当たり実装を
//! 差し替え可能なインターフェース越しに呼び出す」という差し替え点は、独立の trait 階層を
//! 新設せず CORE-13 の provider 注入機構（`SearchProvider` trait・TASK-124 実装済み）へ
//! 一本化する。将来の ANN 実装は本モジュールの [`SearchEngineKind`] に選択肢を追加し、
//! 新しい `SearchProvider` 実装を返す分岐を [`build`] に加えるだけで、`core.rs` 側の
//! コア API（`EngineCore`／`VectorCore`）を変更せずに追加できる。
//!
//! エンジンの選択はコード上の明示指定（[`SearchEngineKind`] の値）のみで決まる。
//! 環境変数・設定ファイルによる実行時の経路上書き機構は設けない（`kernel.rs` に明記の
//! 方針を踏襲。CORE-12）。CPU-SIMD／GPU の実行経路自体を決める決定表は
//! `dispatch.rs::select_execution_path`（TASK-155・CORE-11, 12）が担う。本モジュールが
//! 構築する provider（[`SearchEngineKind`]）はいずれも CPU 実装のため、
//! `core.rs::EngineCore::search` は provider を実行する前に `select_execution_path` を
//! 呼び、`ExecutionPath::CpuSimd` の場合だけ本モジュールの provider を実行する
//! （`ExecutionPath::Gpu` は fail-closed に拒否する。詳細は `core.rs::EngineCore::search`
//! の実装、`dispatch.rs` モジュールドキュメント参照）。SIMD 幅ごとに異なる provider を
//! 構築する配線（[`SearchEngineKind`] への variant 追加を伴う）は TASK-156・CORE-14 の
//! 管轄。
//!
//! # ANN（HNSW）の opt-in 結線（Issue #407・ADR `docs/design/ann-index-adoption.md` B 案）
//!
//! [`SearchEngineKind::Hnsw`] は #404〜#406 で実装した `hnsw.rs::HnswIndex` を選ぶ
//! variant で、`core.rs::EngineCore::open_with_engine`／`from_storage_with_engine`
//! （本 Issue で追加）を明示的に呼んだ場合のみ選択される opt-in 経路であり、
//! [`default_kind`]／[`default_engine`]（既定は不変。[`SearchEngineKind::ParallelBruteForce`]）
//! には影響しない。テーブル単位カタログ属性・wire-server CLI からの露出は対象外
//! （ADR「判断確定後のスコープ外」節、および本 Issue のスコープ外事項）。
//!
//! [`hnsw::provider::HnswSearchProvider`](crate::hnsw::provider::HnswSearchProvider)
//! 自体（`SearchProvider::search` の実装）は本 Issue 時点のまま索引を構築・保持
//! せず、常に `ParallelSearchProvider` へ全件フォールバックする。索引済み集合と
//! `SearchInput` の差分を判定する世代整合キャッシュは、provider の外側
//! （`sql::hnsw_cache::HnswIndexCache`）として Issue #408 で SQL 表層の
//! フィルタなし `Ranking::Distance` クエリに限り接続済み（詳細は
//! `hnsw/provider.rs` モジュールドキュメント・`docs/design/
//! hnsw-generation-cache.md` 参照。Rust API・フィルタ付き・hybrid クエリは
//! 引き続き本 provider の全件フォールバックのみ）。
//!
//! ## 不正な `HnswParams` を型で到達不能にする（codex-review P1 指摘・Issue #407・PR #433 追記）
//!
//! [`SearchEngineKind::Hnsw`] は `HnswParams` ではなく
//! [`crate::hnsw::ValidatedHnswParams`] を保持する。`ValidatedHnswParams` は
//! フィールドが private で、[`crate::hnsw::HnswParams::validate`] を必ず経由する
//! [`crate::hnsw::ValidatedHnswParams::new`] 以外の経路では構築できない。そのため
//! 不正な `HnswParams` を保持した `SearchEngineKind::Hnsw`・
//! [`crate::hnsw::provider::HnswSearchProvider`]・`EngineCore` はそもそも型として
//! 存在しえず、[`build`] は常に成功する infallible な関数になる。
//!
//! untrusted な `HnswParams`（設定値・外部入力）から `SearchEngineKind::Hnsw` を
//! 得たい呼び出し元は、検証が必要になる唯一の入口である [`hnsw_kind`] を使う。
//! [`hnsw_kind`] が返す [`SearchEngineError`] が「構築できなかった」ことを表現する
//! 唯一のエラー型であり、検証を通過した後の [`build`]・
//! `core.rs::EngineCore::open_with_engine`／`from_storage_with_engine` は
//! （`Storage::open` 等 Hnsw 検証と無関係な失敗要因を除き）到達不能な `Err` を
//! 抱えない。以前検討した「黙って既定エンジンへ縮退する」「`KernelError::
//! WorkerPanicked` を設定エラー用に転用する」はいずれも fail-open・エラー分類の
//! 偽装にあたるため採らない（判断の経緯は `docs/design/hnsw-search-engine-wiring.md`
//! 参照。本コメントには経緯を再掲しない）。

use crate::hnsw::provider::HnswSearchProvider;
use crate::hnsw::{HnswParams, ValidatedHnswParams};
use crate::kernel::{CpuScalarProvider, SearchProvider};
use crate::parallel_search::ParallelSearchProvider;
use std::fmt;

/// 選択可能な検索エンジン。
///
/// 各 variant はいずれも `kernel.rs::SearchProvider` の実装を返す（CORE-13 の
/// 単一 trait 階層への一本化。本モジュールは新規 trait を定義しない）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngineKind {
    /// 単一スレッド・スカラー演算の参照実装（[`crate::kernel::CpuScalarProvider`]）。
    /// 他 provider の正解値検証用途が主で、既定エンジンではない。
    CpuScalarBruteForce,
    /// マルチスレッド並列の総当たり Top-k（[`crate::parallel_search::ParallelSearchProvider`]、
    /// TASK-126）。既定エンジン（[`default_engine`] 参照）。
    ParallelBruteForce,
    /// HNSW 近似最近傍探索（`hnsw.rs::HnswIndex`、Issue #403 ADR B 案）。opt-in
    /// （モジュールドキュメント「ANN（HNSW）の opt-in 結線」節参照）。保持する
    /// [`crate::hnsw::ValidatedHnswParams`] は型として検証済みであることが
    /// 保証されている（モジュールドキュメント「不正な `HnswParams` を型で
    /// 到達不能にする」節参照）。
    Hnsw(ValidatedHnswParams),
}

/// 未検証 `HnswParams` から `SearchEngineKind`／`SearchProvider` を構築しようとして
/// 失敗した理由。
///
/// TASK-152・ERR-2 の分類リストへの新規登録・`wire_code()` の公開は行わない
/// （codex-review P1 指摘・Issue #407 追記）: 本 variant を SQLSTATE 風コードで
/// 表すなら `22023` が字面上は近いが、その値は既存分類
/// `error_format::ErrorClass::OperationIdContentMismatch`（TASK-101・RECOVER-10）が
/// 既に占有しており、流用すると ERR-2 の「分類 ⇔ `wire_code` 一意対応」契約
/// （`error_format.rs::wire_codes_are_pairwise_distinct`）が保証する一意性の外側で
/// コードが衝突する。本 Issue は wire／SQL 表層への露出を持たない
/// （モジュールドキュメント「ANN（HNSW）の opt-in 結線」節）ため、正式な
/// `ErrorClass` 登録・`wire_code()` の追加は spec 側のビヘイビア ID 確定後の
/// 別タスクへ申し送る（`docs/design/hnsw-search-engine-wiring.md` 参照）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEngineError {
    /// [`HnswParams::validate`] が拒否した。
    InvalidHnswParams(crate::hnsw::HnswError),
}

impl fmt::Display for SearchEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchEngineError::InvalidHnswParams(err) => {
                write!(f, "invalid HNSW search engine params: {err}")
            }
        }
    }
}

impl std::error::Error for SearchEngineError {
    /// 内包する [`crate::hnsw::HnswError`] をエラーチェーンへ接続する
    /// （codex-review 指摘・PR #433 追記。`core.rs::OpenWithEngineError::source` と
    /// 同じ方針。空の `impl Error` のままだと `HnswError` の詳細情報が
    /// `std::error::Error::source()` チェーン越しには辿れず失われるため）。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SearchEngineError::InvalidHnswParams(e) => Some(e),
        }
    }
}

impl fmt::Display for SearchEngineKind {
    /// 診断・`EXPLAIN`（#411 の担当。本 Issue は表示専用の生成元のみ用意する）向けの
    /// 人間可読表現。`FromStr` の対は本 Issue 時点で呼び出し元が存在しないため設けない
    /// （untrusted 文字列パーサを使う先が無いまま追加しない）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchEngineKind::CpuScalarBruteForce => write!(f, "cpu_scalar_brute_force"),
            SearchEngineKind::ParallelBruteForce => write!(f, "parallel_brute_force"),
            SearchEngineKind::Hnsw(params) => {
                let acorn = match params.acorn_max_visible_ratio() {
                    Some(r) => r.to_string(),
                    None => "none".to_string(),
                };
                let acorn_expansion = match params.acorn_max_expansion_ratio() {
                    Some(r) => r.to_string(),
                    None => "none".to_string(),
                };
                write!(
                    f,
                    "hnsw(m={},ef_construction={},ef_search={},full_scan_ratio={},resident={},sparse_visited_max={},acorn_max_visible_ratio={},acorn_max_expansion_ratio={})",
                    params.m,
                    params.ef_construction,
                    params.ef_search,
                    params.full_scan_ratio(),
                    params.resident_precision(),
                    params.sparse_visited_max(),
                    acorn,
                    acorn_expansion
                )
            }
        }
    }
}

/// 未検証の `HnswParams` から検証済み `SearchEngineKind::Hnsw` を構築する唯一の入口
/// （codex-review P1 指摘・Issue #407・PR #433 追記）。
///
/// `params` が [`HnswParams::validate`] を拒否する値の場合は [`SearchEngineError`] を
/// 返し、通過した場合のみ [`SearchEngineKind::Hnsw`] を返す。untrusted な
/// 文字列・設定値から `SearchEngineKind::Hnsw` を組み立てる経路
/// （`core.rs::EngineCore::open_with_engine` 等の呼び出し元）は必ずこの関数を経由する。
/// 一度 [`SearchEngineKind::Hnsw`] が構築されれば、その値を渡す [`build`]・
/// `open_with_engine`／`from_storage_with_engine` は Hnsw 検証を理由に失敗しない
/// （検証は型 [`crate::hnsw::ValidatedHnswParams`] が保証する）。
pub fn hnsw_kind(params: HnswParams) -> Result<SearchEngineKind, SearchEngineError> {
    let validated =
        ValidatedHnswParams::new(params).map_err(SearchEngineError::InvalidHnswParams)?;
    Ok(SearchEngineKind::Hnsw(validated))
}

/// `kind` に対応する `SearchProvider` 実装を構築する（infallible）。
///
/// `SearchEngineKind::Hnsw` が保持する [`crate::hnsw::ValidatedHnswParams`] は
/// 構築時点で検証済みであることが型で保証されているため（モジュールドキュメント
/// 「不正な `HnswParams` を型で到達不能にする」節）、本関数は `Err` を返す必要が
/// ない。未検証の `HnswParams` から `SearchEngineKind::Hnsw` を得たい場合は
/// [`hnsw_kind`] を先に呼ぶこと。
///
/// 呼び出し元（`core.rs::EngineCore::open` 等）はここで返る `Box<dyn SearchProvider>` を
/// そのまま `EngineCore::with_provider` へ渡す想定（object-safe な trait のため
/// ジェネリクスなしで受け渡しできる）。
pub fn build(kind: SearchEngineKind) -> Box<dyn SearchProvider> {
    match kind {
        SearchEngineKind::CpuScalarBruteForce => Box::new(CpuScalarProvider),
        SearchEngineKind::ParallelBruteForce => Box::new(ParallelSearchProvider),
        SearchEngineKind::Hnsw(params) => Box::new(HnswSearchProvider::new(params)),
    }
}

/// HNSW opt-in（[`SearchEngineKind::Hnsw`]）時に HNSW 経路を使うテーブルの
/// 範囲（Issue #1065・オーナー判断 2026-09-28。`docs/design/
/// index-declaration-effects.md`「HNSW（テーブル単位）」）。
///
/// `crate::core::EngineCore::with_hnsw_scope` で設定し、`wire-server` の起動時
/// CLI フラグ `--hnsw-scope`（プロセス全体・1 回限り）から到達する。HNSW
/// opt-in が無効な構築（既定エンジン）では参照されず、全テーブル厳密
/// （brute-force）のまま。判定本体は `crate::catalog::hnsw_targeted_in_txn`
/// で、SQL 表層・Rust API・`EXPLAIN` の全経路がこれを共有する。いずれの値でも
/// 判定はテーブル単位で、他テーブルの `USING hnsw` 宣言はクエリ対象テーブルの
/// 経路・結果に影響しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HnswScope {
    /// 全テーブルで HNSW を使う（既定。`USING hnsw` 宣言はカタログへ記録される
    /// だけで経路を変えない）。宣言導入前の opt-in 挙動と同一。
    #[default]
    All,
    /// `CREATE INDEX ... USING hnsw` を宣言したテーブルだけ HNSW を使い、
    /// 未宣言テーブルは厳密（brute-force）。`DROP INDEX` で厳密へ戻る。
    Declared,
}

/// 既定の検索エンジン種別（`ParallelBruteForce`）。[`EngineCore::open`]
/// （`core.rs`）・[`default_engine`] が参照する唯一の既定値の源泉であり、
/// 既定エンジンを固定するテストはこの関数の戻り値を検証する。
pub fn default_kind() -> SearchEngineKind {
    SearchEngineKind::ParallelBruteForce
}

/// 既定の検索エンジンを構築する（`EngineCore::open` から呼ばれる既定経路）。
///
/// 現時点の既定は [`default_kind`]（[`SearchEngineKind::ParallelBruteForce`]、
/// マルチスレッド並列総当たり）。挙動は `EngineCore::open` が従来
/// `ParallelSearchProvider` を直接生成していたときと同一で、性能・結果の回帰は
/// 発生しない。
pub fn default_engine() -> Box<dyn SearchProvider> {
    build(default_kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    // object-safety の固定（CORE-13 の一本化要件）: build/default_engine の戻り値型が
    // `Box<dyn SearchProvider>` であることをコンパイル時代入で固定する。
    #[test]
    fn build_and_default_return_boxed_search_provider() {
        let _cpu: Box<dyn SearchProvider> = build(SearchEngineKind::CpuScalarBruteForce);
        let _parallel: Box<dyn SearchProvider> = build(SearchEngineKind::ParallelBruteForce);
        let _hnsw: Box<dyn SearchProvider> =
            build(SearchEngineKind::Hnsw(ValidatedHnswParams::default()));
        let _default: Box<dyn SearchProvider> = default_engine();
    }

    // 既定エンジン不変（受け入れ条件 (a) の型レベル部分）。
    #[test]
    fn default_kind_is_parallel_brute_force() {
        assert_eq!(default_kind(), SearchEngineKind::ParallelBruteForce);
    }

    #[test]
    fn hnsw_default_params_pass_validation() {
        assert!(HnswParams::default().validate().is_ok());
        assert!(hnsw_kind(HnswParams::default()).is_ok());
    }

    // `hnsw_kind` が唯一の検証入口であることの回帰（codex-review P1 指摘・Issue #407・
    // PR #433 追記）。不正な `HnswParams` は `SearchEngineKind::Hnsw` へすら
    // 到達できず、`hnsw_kind` の時点で拒否される。
    #[test]
    fn hnsw_kind_rejects_invalid_params() {
        let invalid = HnswParams {
            m: 1, // HnswParams::validate は m < 2 を拒否する
            ..HnswParams::default()
        };
        let err = match hnsw_kind(invalid) {
            Ok(_) => panic!("m=1 must be rejected"),
            Err(e) => e,
        };
        assert!(matches!(err, SearchEngineError::InvalidHnswParams(_)));
    }

    #[test]
    fn hnsw_kind_accepts_valid_params() {
        let kind = hnsw_kind(HnswParams::default()).expect("default params must validate");
        let _provider: Box<dyn SearchProvider> = build(kind);
    }

    // `SearchEngineError::source()` が内包 `HnswError` を返すことの固定
    // （Cursor Bugbot Low 指摘・PR #433 追記。空の `impl Error` により `source()` が
    // 常に `None` を返し、`OpenWithEngineError::source()`（`core.rs`）から
    // `InvalidHnswParams` 経由で `HnswError` の詳細まで辿れなかった不備の回帰防止）。
    #[test]
    fn search_engine_error_source_returns_hnsw_error() {
        use std::error::Error as _;
        let inner = crate::hnsw::HnswError::InvalidParams {
            reason: "test reason",
        };
        let err = SearchEngineError::InvalidHnswParams(inner.clone());
        let source = err
            .source()
            .expect("InvalidHnswParams variant must expose a source");
        assert_eq!(
            source.to_string(),
            inner.to_string(),
            "source() must return the wrapped HnswError"
        );
    }

    #[test]
    fn display_hnsw_includes_params() {
        let kind = SearchEngineKind::Hnsw(
            ValidatedHnswParams::new(HnswParams {
                m: 32,
                ef_construction: 200,
                ef_search: 128,
            })
            .unwrap(),
        );
        assert_eq!(
            kind.to_string(),
            "hnsw(m=32,ef_construction=200,ef_search=128,full_scan_ratio=1/10,resident=f32,sparse_visited_max=0,acorn_max_visible_ratio=none,acorn_max_expansion_ratio=none)"
        );
        assert_eq!(
            SearchEngineKind::ParallelBruteForce.to_string(),
            "parallel_brute_force"
        );
        assert_eq!(
            SearchEngineKind::CpuScalarBruteForce.to_string(),
            "cpu_scalar_brute_force"
        );
    }
}
