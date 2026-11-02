//! ベクトル検索経路への文単位デッドライン結線（Issue #1953。SQL-71 のポインタ）の結合テスト。
//!
//! kNN・WHERE 付き kNN・hybrid は、可視行の arena 構築（redb 走査のコールドパスと、キャッシュ済み
//! 行からのウォームパス）と DISTANCE 段の前後で期限を確認する。既に過ぎた期限（`at(now)`）なら
//! `57014`・固定文言、1 時間先の期限なら通常の結果になる。sleep を使わず決定的に動く。

use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::interrupt::{DeadlineScope, StatementDeadline};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TIMEOUT_MSG: &str = "canceling statement due to statement timeout";

const QUERIES: &[&str] = &[
    "SELECT id FROM docs ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3",
    "SELECT id FROM docs WHERE tag = 1 ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3",
    "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 3",
];

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn expired() -> StatementDeadline {
    StatementDeadline::at(Instant::now())
}

fn far() -> StatementDeadline {
    StatementDeadline::after(Instant::now(), Duration::from_secs(3600))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");
    let rows: [(&str, u64, [f32; 2], &str, i32); 6] = [
        ("tenant-a", 1, [1.0, 0.0], "vector database search", 1),
        ("tenant-a", 2, [0.95, 0.05], "graph database engine", 1),
        ("tenant-a", 3, [0.0, 1.0], "unrelated text content", 2),
        (
            "tenant-a",
            4,
            [0.5, 0.5],
            "hybrid vector and text search",
            1,
        ),
        // 他テナントの行も arena の走査コストを払う（結果には現れない）。
        ("tenant-b", 1, [1.0, 0.0], "other tenant row", 1),
        ("tenant-b", 2, [0.9, 0.1], "other tenant row", 1),
    ];
    for (tenant, id, emb, body, tag) in rows {
        let owner =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &owner,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(body.to_string()),
                Value::Integer(tag),
            ],
            &op_id(&format!("seed-{tenant}-{id}")),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, CleanupGuard(path))
}

fn query(core: &EngineCore, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    core.execute_sql(&ctx(), sql)
}

fn err_under(deadline: StatementDeadline, core: &EngineCore, sql: &str) -> SqlSurfaceError {
    let _scope = DeadlineScope::install(deadline);
    match query(core, sql) {
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
        Err(e) => e,
    }
}

/// SQL-71: 期限切れの検索（kNN・WHERE 付き kNN・hybrid）は `57014`・固定文言で止まる。
#[test]
fn sql71_expired_deadline_stops_vector_searches() {
    let (core, _g) = open("sql1953-vector-cold");
    for sql in QUERIES {
        let e = err_under(expired(), &core, sql);
        assert_eq!(e.wire_code(), "57014", "{sql}");
        assert_eq!(e.to_string(), TIMEOUT_MSG, "{sql}");
    }
}

/// SQL-71: arena キャッシュ済み（ウォームパス）でも期限切れは `57014` になる。
#[test]
fn sql71_expired_deadline_stops_vector_searches_on_the_cached_arena() {
    let (core, _g) = open("sql1953-vector-warm");
    for sql in QUERIES {
        // 期限なしで 1 回実行してキャッシュを温める。
        query(&core, sql).unwrap_or_else(|e| panic!("warm-up {sql}: {e:?}"));
        let e = err_under(expired(), &core, sql);
        assert_eq!(e.wire_code(), "57014", "{sql}");
        assert_eq!(e.to_string(), TIMEOUT_MSG, "{sql}");
    }
}

/// SQL-71: 十分先の期限なら結果は期限なしと同一。期限切れの後も通常どおり検索できる（ラッチが残らない）。
#[test]
fn sql71_far_deadline_does_not_change_vector_results() {
    let (core, _g) = open("sql1953-vector-far");
    for sql in QUERIES {
        let plain = query(&core, sql).unwrap_or_else(|e| panic!("plain {sql}: {e:?}"));
        let with_deadline = {
            let _scope = DeadlineScope::install(far());
            query(&core, sql).unwrap_or_else(|e| panic!("far {sql}: {e:?}"))
        };
        assert_eq!(plain, with_deadline, "{sql}");
        assert!(!plain.rows.is_empty(), "{sql}");
        assert_eq!(
            err_under(expired(), &core, sql).wire_code(),
            "57014",
            "{sql}"
        );
        assert_eq!(
            query(&core, sql).expect("after the guard is gone"),
            plain,
            "{sql}"
        );
    }
}
