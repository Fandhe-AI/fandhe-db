//! 宣言的スコアリングブースト API（TASK-148・EXT-4。ポインタ:
//! `docs/spec/05-tasks.md` TASK-148・`docs/spec/04-behavior/extensions.md` EXT-4）。
//!
//! 責務境界: `hybrid.rs` の [`crate::hybrid::BoostRule`]／[`crate::hybrid::apply_soft_boost`]
//! は「ヒント種別に依存しない候補 id 集合＋加点量」という汎用形で実装済み
//! （TASK-111・PLAN-1）だが、一致判定（`path_hint_matches`/`kind_hint_matches`）は
//! クエリ展開ヒント専用の 2 演算に閉じている。本モジュールは `declarative_filter.rs`
//! （TASK-147・EXT-3）と同じ構成でこれを一般化する（詳細は TASK-148・EXT-4 ポインタ
//! 参照）。
//!
//! 呼び出し文脈: 呼び出し元（Rust API 直接利用者。SQL 表層への構文露出は TASK-148 の
//! 成果物指定外のため対象外）が [`ScoringBoost`] を宣言し、対象テーブルの
//! [`crate::catalog::TableSchema`] へ [`ScoringBoost::bind`] で束縛したうえで、
//! [`apply_scoring_boosts`] を通じて `hybrid::rrf_fuse`/`hybrid_search` が返した融合済み
//! 候補列（`&mut [HybridHit]`）へ適用する。スコア調整そのものの意味論（加点合計の
//! 絶対上限・非有限拒否・決定的再ソート・「完全除外しない」候補集合不変）は
//! `hybrid::apply_soft_boost` へ一元化し、本モジュールでは再実装しない
//! （[`crate::hybrid::BoostRule`]・[`crate::hybrid::apply_soft_boost`] のモジュール
//! ドキュメント参照）。
//!
//! `unwrap`/`expect`/添字アクセス `[]` を使わず `get()`・`checked_*` で untrusted な
//! 列名・リテラル文字列を扱う（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。

use std::collections::BTreeSet;

use crate::catalog::{ColumnType, TableSchema};
use crate::hybrid::{
    apply_soft_boost, BoostRule, HybridError, HybridHit, RrfConfig, MAX_BOOST_IDS, MAX_BOOST_RULES,
};
use crate::row_codec::MAX_TEXT_FIELD_LEN;
use crate::sql::allowlist::SqlSurfaceError;

/// メタデータ列への一致演算。正規表現・glob は導入しない（untrusted 文字列に対する
/// ReDoS 類の余地を作らない既存方針。`declarative_filter::FilterOp` と対称の演算
/// 集合を採用しつつ、`Contains`（`hybrid::path_hint_matches` の一般化）を追加する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoostMatchOp {
    /// 完全一致（[`crate::hybrid::kind_hint_matches`] の一般化）。
    Equals(String),
    /// 前方一致（`declarative_filter::FilterOp::StartsWith` と対称の演算）。
    StartsWith(String),
    /// 部分文字列一致（[`crate::hybrid::path_hint_matches`] の一般化）。
    Contains(String),
}

/// 未束縛の宣言的スコアリングブースト（列名指定）。`ScoringBoost::equals("kind",
/// "doc", 0.0007).bind(&schema)` のように使う。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoringBoost {
    column: String,
    op: BoostMatchOp,
    amount: f64,
}

impl ScoringBoost {
    /// 完全一致ブーストを宣言する。
    pub fn equals(column: impl Into<String>, value: impl Into<String>, amount: f64) -> Self {
        Self {
            column: column.into(),
            op: BoostMatchOp::Equals(value.into()),
            amount,
        }
    }

    /// 前方一致ブーストを宣言する。
    pub fn starts_with(column: impl Into<String>, prefix: impl Into<String>, amount: f64) -> Self {
        Self {
            column: column.into(),
            op: BoostMatchOp::StartsWith(prefix.into()),
            amount,
        }
    }

    /// 部分文字列一致ブーストを宣言する。
    pub fn contains(column: impl Into<String>, needle: impl Into<String>, amount: f64) -> Self {
        Self {
            column: column.into(),
            op: BoostMatchOp::Contains(needle.into()),
            amount,
        }
    }

    /// `schema` と照合して [`BoundScoringBoost`] へ束縛する。列名解決失敗・`VECTOR`
    /// 列指定は `declarative_filter::bind` と同じ `22000` 系で拒否する。リテラル長は
    /// [`MAX_TEXT_FIELD_LEN`] で上限検証（`54000`）し、空リテラルは拒否する（無条件
    /// 一致・無条件不一致の無意味な宣言を黙って受理しない。`declarative_filter` の
    /// 空 prefix 拒否と同じ方針を 3 演算へ揃える）。`amount` は
    /// [`crate::hybrid::BoostRule::new`] と同じ値域（有限・`0.0 < amount`）を束縛時点
    /// でも検証し fail-fast にする（適用時の `BoostRule::new` 検証は二重の砦として
    /// 残る。上限値である `MAX_BOOST_AMOUNT` は非公開のため、ここでは「有限かつ正」
    /// までを検証し、絶対上限の判定は [`crate::hybrid::BoostRule::new`] に委譲する）。
    pub fn bind(&self, schema: &TableSchema) -> Result<BoundScoringBoost, SqlSurfaceError> {
        let column = schema
            .columns
            .iter()
            .find(|c| c.name == self.column)
            .ok_or_else(|| SqlSurfaceError::undefined_column(&self.column))?;
        match &column.ty {
            ColumnType::Text | ColumnType::Varchar => {}
            ColumnType::Vector(_)
            | ColumnType::Integer
            | ColumnType::BigInt
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Boolean
            | ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::Array(_)
            | ColumnType::Bytea
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Enum(_)
            | ColumnType::Numeric { .. }
            | ColumnType::Uuid => {
                // F10（Issue #882 計画）: REAL/DOUBLE 列は VECTOR 列と同じ
                // 「TEXT 列でない」拒否腕へ合流させる（対応は #891 へ申し送り）。
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {:?} is not a TEXT column",
                    self.column
                )));
            }
        }
        let op = match &self.op {
            BoostMatchOp::Equals(value) => {
                check_literal(value)?;
                BoostMatchOp::Equals(value.clone())
            }
            BoostMatchOp::StartsWith(prefix) => {
                check_literal(prefix)?;
                BoostMatchOp::StartsWith(prefix.clone())
            }
            BoostMatchOp::Contains(needle) => {
                check_literal(needle)?;
                BoostMatchOp::Contains(needle.clone())
            }
        };
        if !self.amount.is_finite() || self.amount <= 0.0 {
            return Err(SqlSurfaceError::invalid_input(
                "scoring boost amount must be a finite positive value",
            ));
        }
        Ok(BoundScoringBoost {
            column: self.column.clone(),
            op,
            amount: self.amount,
        })
    }
}

/// リテラルが空でなく、アロケーション前の上限（[`MAX_TEXT_FIELD_LEN`]・`54000`）を
/// 超えないことを検証する（`declarative_filter::check_literal_len` と同じ方針。
/// 空リテラルは全演算〔`Equals`/`StartsWith`/`Contains`〕で無条件一致・無条件不一致に
/// なり得るため、`declarative_filter` の空 prefix 拒否をここでは 3 演算全てへ揃える）。
fn check_literal(value: &str) -> Result<(), SqlSurfaceError> {
    if value.is_empty() {
        return Err(SqlSurfaceError::invalid_input(
            "scoring boost literal must not be empty",
        ));
    }
    let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
    if len > MAX_TEXT_FIELD_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "scoring boost literal length {len} exceeds limit {MAX_TEXT_FIELD_LEN}"
        )));
    }
    Ok(())
}

/// スキーマ照合済みのスコアリングブースト（[`ScoringBoost::bind`] の戻り値。
/// フィールドは非公開とし、検証付きコンストラクタ経由でのみ構築できる
/// （`declarative_filter::MetadataFilter` と同じ「構造体リテラルでの直接構築を許すと
/// 検証を迂回できる」流儀）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundScoringBoost {
    column: String,
    op: BoostMatchOp,
    amount: f64,
}

impl BoundScoringBoost {
    /// `value`（対象列の値。`NULL` は `None`）がこのブーストの一致条件を満たすか
    /// 判定する。`NULL` は常に不一致（fail-closed。`hybrid::path_hint_matches`/
    /// `kind_hint_matches` の空ヒント不一致と同じ方向）。
    fn matches(&self, value: Option<&str>) -> bool {
        let Some(value) = value else {
            return false;
        };
        match &self.op {
            BoostMatchOp::Equals(expected) => value == expected,
            BoostMatchOp::StartsWith(prefix) => value.starts_with(prefix.as_str()),
            BoostMatchOp::Contains(needle) => value.contains(needle.as_str()),
        }
    }
}

/// 1 件の候補（融合済み `id`）と、[`BoundScoringBoost::column`] に対応する対象列の
/// 値を結び付ける呼び出し元提供のメタデータ。`sql::exec` 等が RLS 通過後の可視行から
/// 構築する想定（`declarative_filter` の `matches_all` が行データを直接受け取るのと
/// 異なり、本 API は融合後の `id` 列に対して事後的に値を引く形を取る。理由は
/// `hybrid::HybridHit` が id のみを保持し行データを持たないため）。
pub struct BoostMetadata<'a> {
    id: u64,
    /// [`BoundScoringBoost::column`] の列名 → その候補における値（`NULL` は `None`）。
    values: &'a [(&'a str, Option<&'a str>)],
}

impl<'a> BoostMetadata<'a> {
    /// `id` の候補に対する列値集合 `values`（列名 → 値のペア列）を束ねる。
    pub fn new(id: u64, values: &'a [(&'a str, Option<&'a str>)]) -> Self {
        Self { id, values }
    }

    fn value_of(&self, column: &str) -> Option<&'a str> {
        self.values
            .iter()
            .find(|(name, _)| *name == column)
            .and_then(|(_, value)| *value)
    }
}

/// `apply_scoring_boosts` が一致判定・アロケーション前に検証する宣言件数の上限。
/// [`crate::hybrid::MAX_BOOST_RULES`] を転用する（`apply_soft_boost` 側の上限と
/// 二重に管理しない。無制限 `Vec` 確保回避は `.claude/rules/security.md`
/// 「不安全な設計｜無制限リソース確保（DoS）」対応）。
pub const MAX_SCORING_BOOSTS: usize = MAX_BOOST_RULES;

/// [`hybrid::rrf_fuse`](crate::hybrid::rrf_fuse)/[`hybrid::hybrid_search`]
/// (crate::hybrid::hybrid_search) が返した融合済み候補列 `hits` へ、`boosts`（束縛済み
/// スコアリングブースト列）と `metadata`（候補 id ごとの対象列値）から一致候補 id 集合
/// を構築し、[`apply_soft_boost`] へ委譲して加点を適用する。
///
/// `boosts.len() > MAX_SCORING_BOOSTS` はアロケーション・走査前に
/// [`HybridError::TooManyBoostRules`] で拒否する（`hybrid::apply_soft_boost` の長さ
/// 検証と同じ順序・同じエラー型。本 API 独自のエラー分類は増やさない）。`metadata`
/// についても、走査（各ブーストごとの線形走査）に先立って `metadata.len() >
/// MAX_BOOST_IDS` を [`HybridError::TooManyBoostIds`] で拒否する: この事前検証を
/// 欠くと `boosts.len() * metadata.len()` に比例した無制限 CPU/メモリ消費が
/// 発生したのち、最終的に一致 id 数超過で拒否されるだけでもリソース枯渇（DoS）を
/// 招きうる（PR #260 codex-review・cursor[bot] 指摘対応。`rrf_fuse` の
/// `TooManyCandidates` と同じ「アロケーション・走査前に長さを検証する」順序を
/// 踏襲する。融合済み候補 1 件につき高々 1 エントリが自然な形のため、候補 id
/// 集合の上限である `MAX_BOOST_IDS` をそのまま転用する）。この事前検証により、
/// 続く各ブーストの `metadata` 線形走査は `boosts.len() * metadata.len() <=
/// MAX_SCORING_BOOSTS * MAX_BOOST_IDS` に有界化される。一致 id 集合の構築後は
/// `hybrid::apply_soft_boost` へ委譲し、スコア調整の意味論（加点合計の絶対上限・
/// 非有限拒否・決定的再ソート・候補集合不変）はそちらへ一元化する。
///
/// なお `metadata.len() <= MAX_BOOST_IDS` を通過した時点で、各ブーストの一致 id
/// 集合（`metadata` 中のユニークな `id` の部分集合）は `metadata.len()` を超え
/// 得ないため `MAX_BOOST_IDS` を自動的に下回る。[`BoostRule::new`] の
/// `MAX_BOOST_IDS` 検査は本関数のこの不変条件により実質的に到達しないが、
/// 呼び出し契約を変えない（`ids` を直接構築できる他の呼び出し経路のための
/// 防御的検証として残す）ため削除しない。
pub fn apply_scoring_boosts(
    hits: &mut [HybridHit],
    boosts: &[BoundScoringBoost],
    metadata: &[BoostMetadata<'_>],
    cfg: &RrfConfig,
) -> Result<(), HybridError> {
    if boosts.len() > MAX_SCORING_BOOSTS {
        return Err(HybridError::TooManyBoostRules {
            len: boosts.len(),
            max: MAX_SCORING_BOOSTS,
        });
    }
    if metadata.len() > MAX_BOOST_IDS {
        return Err(HybridError::TooManyBoostIds {
            len: metadata.len(),
            max: MAX_BOOST_IDS,
        });
    }
    // `ids` 集合は各ブーストごとに構築し、`BoostRule::new` へ渡すまで生存させる
    // 必要があるため、ループ本体より前に確保する（`BoostRule<'a>` が借用する）。
    let mut id_sets: Vec<BTreeSet<u64>> = Vec::with_capacity(boosts.len());
    for boost in boosts {
        let mut ids = BTreeSet::new();
        for m in metadata {
            if boost.matches(m.value_of(&boost.column)) {
                ids.insert(m.id);
            }
        }
        id_sets.push(ids);
    }
    let mut rules: Vec<BoostRule<'_>> = Vec::with_capacity(boosts.len());
    for (boost, ids) in boosts.iter().zip(id_sets.iter()) {
        rules.push(BoostRule::new(ids, boost.amount)?);
    }
    apply_soft_boost(hits, &rules, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("body", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("kind", ColumnType::Text, true),
            ],
        )
    }

    #[test]
    fn bind_rejects_unknown_column() {
        let err = ScoringBoost::equals("missing", "doc", 0.001)
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "42703");
    }

    #[test]
    fn bind_rejects_vector_column() {
        let err = ScoringBoost::equals("body", "x", 0.001)
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_empty_literal() {
        assert!(ScoringBoost::equals("kind", "", 0.001)
            .bind(&schema())
            .is_err());
        assert!(ScoringBoost::starts_with("path", "", 0.001)
            .bind(&schema())
            .is_err());
        assert!(ScoringBoost::contains("path", "", 0.001)
            .bind(&schema())
            .is_err());
    }

    #[test]
    fn bind_rejects_invalid_amount() {
        assert!(ScoringBoost::equals("kind", "doc", 0.0)
            .bind(&schema())
            .is_err());
        assert!(ScoringBoost::equals("kind", "doc", -0.1)
            .bind(&schema())
            .is_err());
        assert!(ScoringBoost::equals("kind", "doc", f64::NAN)
            .bind(&schema())
            .is_err());
    }

    #[test]
    fn bind_accepts_valid_declaration() {
        assert!(ScoringBoost::equals("kind", "doc", 0.0007)
            .bind(&schema())
            .is_ok());
        assert!(ScoringBoost::starts_with("path", "src/", 0.0007)
            .bind(&schema())
            .is_ok());
        assert!(ScoringBoost::contains("path", "hybrid", 0.0007)
            .bind(&schema())
            .is_ok());
    }

    #[test]
    fn matches_null_metadata_is_fail_closed() {
        let bound = ScoringBoost::equals("kind", "doc", 0.0007)
            .bind(&schema())
            .expect("bind ok");
        assert!(!bound.matches(None));
        assert!(bound.matches(Some("doc")));
        assert!(!bound.matches(Some("docx")));
    }

    #[test]
    fn apply_scoring_boosts_raises_matching_candidate_without_excluding_others() {
        // `hybrid.rs::apply_soft_boost_changes_rank_order` と同じ流儀: 確定判定は
        // 加点合計を `soft_boost_confirm_cap`（既定 cfg では `1/61 ≈ 0.0164`）未満に
        // しか許さないため、真の 1 位（id=1）とプール最下位（id=3）はそのままに、
        // 中間 2 件（id=2・id=3）の順位だけを入れ替えて近接順位の逆転を確認する。
        let mut hits = vec![
            HybridHit { id: 1, score: 0.5 },
            HybridHit { id: 2, score: 0.45 },
            HybridHit {
                id: 3,
                score: 0.4495,
            },
        ];
        let before: BTreeSet<u64> = hits.iter().map(|h| h.id).collect();
        let cfg = RrfConfig::default();
        let bound = ScoringBoost::equals("kind", "doc", 0.001)
            .bind(&schema())
            .expect("bind ok");
        let values: [(&str, Option<&str>); 1] = [("kind", Some("doc"))];
        let metadata = vec![BoostMetadata::new(3, &values)];
        apply_scoring_boosts(&mut hits, &[bound], &metadata, &cfg).expect("apply ok");
        let after: BTreeSet<u64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(before, after, "candidate set must stay unchanged (EXT-4)");
        assert_eq!(
            hits.first().map(|h| h.id),
            Some(1),
            "true top rank preserved"
        );
        assert_eq!(
            hits.get(1).map(|h| h.id),
            Some(3),
            "boosted candidate should overtake the near-top rival"
        );
    }

    #[test]
    fn apply_scoring_boosts_rejects_too_many_boosts() {
        let mut hits = vec![HybridHit { id: 1, score: 0.1 }];
        let cfg = RrfConfig::default();
        let bound = ScoringBoost::equals("kind", "doc", 0.0004)
            .bind(&schema())
            .expect("bind ok");
        let boosts: Vec<BoundScoringBoost> =
            (0..=(MAX_SCORING_BOOSTS)).map(|_| bound.clone()).collect();
        let err = apply_scoring_boosts(&mut hits, &boosts, &[], &cfg).unwrap_err();
        assert_eq!(
            err,
            HybridError::TooManyBoostRules {
                len: MAX_SCORING_BOOSTS + 1,
                max: MAX_SCORING_BOOSTS,
            }
        );
    }

    #[test]
    fn contains_and_equals_align_with_hint_matches_helpers() {
        use crate::hybrid::{kind_hint_matches, path_hint_matches};

        let path_bound = ScoringBoost::contains("path", "src/hybrid", 0.0007)
            .bind(&schema())
            .expect("bind ok");
        assert_eq!(
            path_bound.matches(Some("src/hybrid/target.rs")),
            path_hint_matches("src/hybrid", "src/hybrid/target.rs")
        );
        assert_eq!(
            path_bound.matches(Some("src/other/alpha.rs")),
            path_hint_matches("src/hybrid", "src/other/alpha.rs")
        );

        let kind_bound = ScoringBoost::equals("kind", "doc", 0.0007)
            .bind(&schema())
            .expect("bind ok");
        assert_eq!(
            kind_bound.matches(Some("doc")),
            kind_hint_matches("doc", "doc")
        );
        assert_eq!(
            kind_bound.matches(Some("docx")),
            kind_hint_matches("doc", "docx")
        );
    }
}
