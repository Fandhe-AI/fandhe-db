//! `EngineCore::{execute,explain}_bound_search_with_index_metric_in_session` と
//! `BoundStatement::with_distance_kind` の公開に伴う整合検査の結合テスト
//! （Issue #2243。ポインタ: NOSQL-2・SQL-33 (7)・INDEX-7）。
//!
//! 固定する内容:
//! - 既定距離種別の解決（opt-in なし／`Declared`／`All` × 宣言の有無・opclass 集合）
//! - 非 Cosine × `precision` は `0A000`、非 Cosine × Hybrid は `42601`（execute・explain 共通）
//! - 既存入口 `execute_bound_search_in_session` の後方互換

use engine::catalog::{ColumnDef, ColumnType, IndexDef, IndexKind, TableSchema, VectorOpClass};
use engine::core::{EngineCore, IndexMetricDefault};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::search_engine::{self, HnswScope};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::lexer::DistanceKind;
use engine::sql::mode::{resolve_mode, SearchMode, SessionState};
use engine::sql::parser::{BoundStatement, ProjectedColumn, Ranking};
use engine::sql::udf_call::UdfRegistry;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("valid ctx")
}

fn storage_with_decls(path: &std::path::Path, decls: &[VectorOpClass]) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        ))
        .expect("create table");
    for (i, opclass) in decls.iter().enumerate() {
        storage
            .create_index(&IndexDef::new(
                format!("idx_{i}"),
                TABLE.to_string(),
                IndexKind::Hnsw(*opclass),
                vec!["embedding".to_string()],
            ))
            .expect("declare hnsw index");
    }
    storage
}

/// `scope` が `None` なら opt-in なし（既定エンジン）。
fn core_for(
    label: &str,
    scope: Option<HnswScope>,
    decls: &[VectorOpClass],
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = storage_with_decls(&path, decls);
    let core = match scope {
        None => EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        Some(scope) => {
            let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid hnsw params");
            EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope)
        }
    };
    (core, guard)
}

fn bound(ranking: Ranking, kind: Option<DistanceKind>, precision: bool) -> BoundStatement {
    let mut stmt = BoundStatement::new(
        TABLE.to_string(),
        vec![ProjectedColumn::Id],
        Vec::new(),
        false,
        ranking,
        5,
        engine::sql::plan::EvaluationOrder::DEFAULT,
    );
    if let Some(kind) = kind {
        stmt = stmt.with_distance_kind(kind);
    }
    if precision {
        stmt = stmt.with_mode(resolve_mode(Some(SearchMode::Precision), None));
    }
    stmt
}

fn distance() -> Ranking {
    Ranking::Distance {
        query: vec![1.0, 0.0, 0.0, 0.0],
    }
}

fn build_ip_precision(
    _: &TableSchema,
    _: &UdfRegistry,
    _: &IndexMetricDefault<'_>,
) -> Result<BoundStatement, SqlSurfaceError> {
    Ok(bound(
        distance(),
        Some(DistanceKind::NegativeInnerProduct),
        true,
    ))
}

fn build_hybrid_l2(
    _: &TableSchema,
    _: &UdfRegistry,
    _: &IndexMetricDefault<'_>,
) -> Result<BoundStatement, SqlSurfaceError> {
    Ok(bound(
        Ranking::Hybrid {
            query: vec![1.0, 0.0, 0.0, 0.0],
            text_column_index: 0,
            query_text: "q".to_string(),
            tuning: Default::default(),
        },
        Some(DistanceKind::L2),
        false,
    ))
}

/// 既定の解決結果を execute 経路で観測する。
fn resolved_default(core: &EngineCore) -> Result<DistanceKind, String> {
    let session = SessionState::default();
    let seen = std::cell::Cell::new(None);
    let result = core.execute_bound_search_with_index_metric_in_session(
        &ctx(),
        &session,
        TABLE,
        |_schema, _udfs, index_metric| {
            let kind = index_metric.distance_kind()?;
            seen.set(Some(kind));
            Ok(bound(distance(), Some(DistanceKind::Cosine), false))
        },
    );
    match (result, seen.get()) {
        (_, Some(kind)) => Ok(kind),
        (Err(e), None) => Err(e.wire_code().to_string()),
        (Ok(_), None) => Err("unresolved".to_string()),
    }
}

#[test]
fn default_metric_resolution_truth_table() {
    use VectorOpClass::{Cosine, Ip, L2};
    let declared = Some(HnswScope::Declared);
    let all = Some(HnswScope::All);
    let cases: [(&str, Option<HnswScope>, Vec<VectorOpClass>, DistanceKind); 7] = [
        ("none", None, vec![L2], DistanceKind::Cosine),
        ("decl-empty", declared, vec![], DistanceKind::Cosine),
        ("decl-l2", declared, vec![L2], DistanceKind::L2),
        (
            "decl-cos-l2",
            declared,
            vec![Cosine, L2],
            DistanceKind::Cosine,
        ),
        (
            "decl-ip",
            declared,
            vec![Ip],
            DistanceKind::NegativeInnerProduct,
        ),
        ("all-empty", all, vec![], DistanceKind::Cosine),
        ("all-l2", all, vec![L2], DistanceKind::Cosine),
    ];
    for (label, scope, decls, want) in cases {
        let (core, _guard) = core_for(&format!("bound-metric-{label}"), scope, &decls);
        assert_eq!(resolved_default(&core), Ok(want), "{label}");
    }
}

#[test]
fn precision_with_non_cosine_is_rejected_on_execute_and_explain() {
    let (core, _guard) = core_for("bound-metric-precision", None, &[]);
    let session = SessionState::default();
    let exec = core
        .execute_bound_search_with_index_metric_in_session(
            &ctx(),
            &session,
            TABLE,
            build_ip_precision,
        )
        .expect_err("precision with non-cosine");
    assert_eq!(exec.wire_code(), "0A000");
    let explain = core
        .explain_bound_search_with_index_metric_in_session(
            &ctx(),
            &session,
            TABLE,
            build_ip_precision,
        )
        .expect_err("precision with non-cosine");
    assert_eq!(explain.wire_code(), "0A000");
}

#[test]
fn hybrid_with_non_cosine_is_rejected_on_execute_and_explain() {
    let (core, _guard) = core_for("bound-metric-hybrid", None, &[]);
    let session = SessionState::default();
    let exec = core
        .execute_bound_search_with_index_metric_in_session(&ctx(), &session, TABLE, build_hybrid_l2)
        .expect_err("hybrid with non-cosine");
    assert_eq!(exec.wire_code(), "42601");
    let explain = core
        .explain_bound_search_with_index_metric_in_session(&ctx(), &session, TABLE, build_hybrid_l2)
        .expect_err("hybrid with non-cosine");
    assert_eq!(explain.wire_code(), "42601");
}

#[test]
fn legacy_entries_remain_compatible_with_cosine_default() {
    let (core, _guard) = core_for("bound-metric-legacy", None, &[]);
    let session = SessionState::default();
    let exec = core.execute_bound_search_in_session(&ctx(), &session, TABLE, |_s, _u| {
        Ok(bound(distance(), None, false))
    });
    assert!(exec.is_ok(), "{exec:?}");
    let explain = core.explain_bound_search_in_session(&ctx(), &session, TABLE, |_s, _u| {
        Ok(bound(distance(), None, false))
    });
    assert!(explain.is_ok(), "{explain:?}");
}
