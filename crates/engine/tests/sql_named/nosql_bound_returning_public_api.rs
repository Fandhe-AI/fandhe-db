//! `EngineCore::execute_bound_{insert,update,delete_checked,predicate_update,predicate_delete}
//! _returning_in_session`（NoSQL 表層 `returning` 向けの束縛済みセッション入口。NOSQL-22・
//! Issue #2091。ポインタ: SQL-21・SQL-40・RLS-10・RECOVER-10／11）が、SQL 表層の
//! `... RETURNING` と**同一の実行器**に到達し、返却行の内容と件数が一致することを固定する
//! 結合テスト。
//!
//! 検査の骨子: 同じ seed の 2 つの core（双子）を用意し、片方へ束縛済み入口、もう片方へ
//! SQL テキストを流して `ReturningOutcome` 全体（列メタ・行・`rows_affected`）を比較する。
//! 他テナント id・不存在 id・不可視行では件数 0・返却 0 行が SQL と一致することも固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::row_codec::Value;
use engine::sql::allowlist::{
    InsertLiteral, Projection, SetValue, ValidatedUpdate, WherePredicate,
};
use engine::sql::exec::ReturningOutcome;
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_update, BoundDelete, BoundInsert};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::BigInt, true),
        ],
    )
}

fn viewer() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn other() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT_B, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn op(raw: &str) -> OperationId {
    OperationId::parse(raw).expect("valid operation_id")
}

/// 双子の片割れ。閲覧テナントの id 1〜3（`ja`,`ja`,`en`）と他テナントの id 1,9 を seed する。
fn open_twin(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Ledgered);
    for (id, lang) in [(1, "ja"), (2, "ja"), (3, "en")] {
        sql(
            &core,
            &viewer(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, n) VALUES ({id}, '[0.5,0.25]', '{lang}', {id}) \
                 USING OPERATION_ID 'seed-a-{id}'"
            ),
        );
    }
    for id in [1, 9] {
        sql(
            &core,
            &other(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, n) VALUES ({id}, '[0.1,0.1]', 'ja', 99) \
                 USING OPERATION_ID 'seed-b-{id}'"
            ),
        );
    }
    (core, guard)
}

fn sql(core: &EngineCore, ctx: &PolicyContext, text: &str) -> SqlOutcome {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn sql_returning(core: &EngineCore, text: &str) -> ReturningOutcome {
    match sql(core, &viewer(), text) {
        SqlOutcome::Returning(r) => r,
        other => panic!("expected Returning, got {other:?}"),
    }
}

fn projections() -> Vec<(Projection, &'static str)> {
    vec![
        (Projection::All, "*"),
        (
            Projection::Columns(vec!["id".to_string(), "lang".to_string()]),
            "id, lang",
        ),
    ]
}

fn insert_bounds(ids: &[u64], op_id: &str) -> Vec<BoundInsert> {
    ids.iter()
        .map(|&id| BoundInsert {
            table: TABLE.to_string(),
            id,
            values: vec![
                Value::Vector(vec![0.5, 0.25]),
                Value::Text("fr".to_string()),
                Value::BigInt(id as i64),
            ],
            operation_id: Some(op(op_id)),
            volatile_default_columns: Vec::new(),
            id_omitted: false,
        })
        .collect()
}

fn bound_update(
    id: u64,
    lang: &str,
    op_id: &str,
) -> impl FnOnce(
    &TableSchema,
) -> Result<engine::sql::parser::BoundUpdate, engine::sql::allowlist::SqlSurfaceError> {
    let lang = lang.to_string();
    let op_id = op_id.to_string();
    move |schema| {
        let validated = ValidatedUpdate {
            table_name: TABLE.to_string(),
            assignments: vec![(
                "lang".to_string(),
                SetValue::Literal(InsertLiteral::String(lang)),
            )],
            id_literal: id.to_string(),
            operation_id: Some(op(&op_id)),
            returning: None,
        };
        bind_update(&validated, schema)
    }
}

fn lang_eq(value: &str) -> Vec<WherePredicate> {
    vec![WherePredicate::Equality {
        column: "lang".to_string(),
        value: value.to_string(),
    }]
}

#[test]
fn insert_returning_matches_sql() {
    for (i, (proj, sql_cols)) in projections().into_iter().enumerate() {
        let (bound_core, _g1) = open_twin(&format!("nosql-ret-ins-b-{i}"));
        let (sql_core, _g2) = open_twin(&format!("nosql-ret-ins-s-{i}"));
        let got = bound_core
            .execute_bound_insert_returning_in_session(
                &viewer(),
                &SessionState::default(),
                TABLE,
                2,
                Some(&op("ins-1")),
                &proj,
                |_| Ok(insert_bounds(&[10, 11], "ins-1")),
            )
            .expect("bound insert returning");
        let want = sql_returning(
            &sql_core,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, n) VALUES \
                 (10, '[0.5,0.25]', 'fr', 10), (11, '[0.5,0.25]', 'fr', 11) \
                 RETURNING {sql_cols} USING OPERATION_ID 'ins-1'"
            ),
        );
        assert_eq!(got, want, "insert returning {sql_cols}");
        assert_eq!(got.rows_affected, 2);
        assert_eq!(got.result.rows.len(), 2);
    }
}

#[test]
fn single_row_update_and_delete_returning_match_sql() {
    for (i, (proj, sql_cols)) in projections().into_iter().enumerate() {
        let (bound_core, _g1) = open_twin(&format!("nosql-ret-one-b-{i}"));
        let (sql_core, _g2) = open_twin(&format!("nosql-ret-one-s-{i}"));
        // 自テナントの id 1・他テナントにも存在する id 9・不存在 id 77 を順に流す。
        for (id, expect_rows) in [(1u64, 1u64), (9, 0), (77, 0)] {
            let op_id = format!("upd-{id}");
            let got = bound_core
                .execute_bound_update_returning_in_session(
                    &viewer(),
                    &SessionState::default(),
                    TABLE,
                    Some(&op(&op_id)),
                    &proj,
                    bound_update(id, "en", &op_id),
                )
                .expect("bound update returning");
            let want = sql_returning(
                &sql_core,
                &format!(
                    "UPDATE {TABLE} SET lang = 'en' WHERE id = {id} RETURNING {sql_cols} \
                     USING OPERATION_ID '{op_id}'"
                ),
            );
            assert_eq!(got, want, "update id={id} {sql_cols}");
            assert_eq!(got.rows_affected, expect_rows);
            assert_eq!(got.result.rows.len() as u64, expect_rows);

            let del_op = format!("del-{id}");
            let bound = BoundDelete {
                table: TABLE.to_string(),
                id,
                operation_id: Some(op(&del_op)),
            };
            let got = bound_core
                .execute_bound_delete_checked_returning_in_session(
                    &viewer(),
                    &SessionState::default(),
                    &bound,
                    &proj,
                    |_| Ok(()),
                )
                .expect("bound delete returning");
            let want = sql_returning(
                &sql_core,
                &format!(
                    "DELETE FROM {TABLE} WHERE id = {id} RETURNING {sql_cols} \
                     USING OPERATION_ID '{del_op}'"
                ),
            );
            assert_eq!(got, want, "delete id={id} {sql_cols}");
            assert_eq!(got.rows_affected, expect_rows);
        }
    }
}

#[test]
fn predicate_update_and_delete_returning_match_sql() {
    for (i, (proj, sql_cols)) in projections().into_iter().enumerate() {
        let (bound_core, _g1) = open_twin(&format!("nosql-ret-pred-b-{i}"));
        let (sql_core, _g2) = open_twin(&format!("nosql-ret-pred-s-{i}"));

        let got = bound_core
            .execute_bound_predicate_update_returning_in_session(
                &viewer(),
                &SessionState::default(),
                TABLE,
                Some(&op("pu-1")),
                &proj,
                |_| {
                    Ok((
                        vec![("n".to_string(), InsertLiteral::Number("7".to_string()))],
                        lang_eq("ja"),
                    ))
                },
            )
            .expect("bound predicate update returning");
        let want = sql_returning(
            &sql_core,
            &format!(
                "UPDATE {TABLE} SET n = 7 WHERE lang = 'ja' RETURNING {sql_cols} \
                 USING OPERATION_ID 'pu-1'"
            ),
        );
        assert_eq!(got, want, "predicate update {sql_cols}");
        // 他テナントの `ja` 行（id 1・9）は候補に入らない。
        assert_eq!(got.rows_affected, 2);

        let got = bound_core
            .execute_bound_predicate_delete_returning_in_session(
                &viewer(),
                &SessionState::default(),
                TABLE,
                Some(&op("pd-1")),
                &proj,
                |_| Ok(lang_eq("ja")),
            )
            .expect("bound predicate delete returning");
        let want = sql_returning(
            &sql_core,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING {sql_cols} \
                 USING OPERATION_ID 'pd-1'"
            ),
        );
        assert_eq!(got, want, "predicate delete {sql_cols}");
        assert_eq!(got.rows_affected, 2);
    }
}

#[test]
fn unknown_returning_column_and_undefined_table_use_sql_codes() {
    let (core, _g) = open_twin("nosql-ret-errors");
    let err = core
        .execute_bound_predicate_delete_returning_in_session(
            &viewer(),
            &SessionState::default(),
            TABLE,
            Some(&op("e-1")),
            &Projection::Columns(vec!["no_such".to_string()]),
            |_| Ok(lang_eq("ja")),
        )
        .expect_err("unknown column");
    let sql_err = core
        .execute_sql_in_session(
            &viewer(),
            &mut SessionState::default(),
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING no_such USING OPERATION_ID 'e-2'"
            ),
        )
        .expect_err("sql unknown column");
    assert_eq!(err.wire_code(), sql_err.wire_code());

    let err = core
        .execute_bound_predicate_delete_returning_in_session(
            &viewer(),
            &SessionState::default(),
            "missing_table",
            Some(&op("e-3")),
            &Projection::All,
            |_| Ok(lang_eq("ja")),
        )
        .expect_err("undefined table");
    assert_eq!(err.wire_code(), "42P01");

    // 未知列の拒否では何も削除されていない。
    let rows = sql_returning(
        &core,
        &format!("DELETE FROM {TABLE} WHERE lang = 'zz' RETURNING * USING OPERATION_ID 'e-4'"),
    );
    assert_eq!(rows.rows_affected, 0);
}

#[test]
fn operation_id_replay_is_independent_of_returning() {
    // 内容照合ハッシュは RETURNING を材料にしない: returning あり → なしの再送は、なし → なしの
    // 再送と同じ結果になる（RECOVER-10・11）。
    let (a, _g1) = open_twin("nosql-ret-replay-a");
    let (b, _g2) = open_twin("nosql-ret-replay-b");
    let preds = |_: &TableSchema| {
        Ok((
            vec![("n".to_string(), InsertLiteral::Number("5".to_string()))],
            lang_eq("ja"),
        ))
    };
    a.execute_bound_predicate_update_returning_in_session(
        &viewer(),
        &SessionState::default(),
        TABLE,
        Some(&op("rp-1")),
        &Projection::All,
        preds,
    )
    .expect("first (returning)");
    b.execute_bound_predicate_update_in_session(
        &viewer(),
        &SessionState::default(),
        TABLE,
        Some(&op("rp-1")),
        preds,
    )
    .expect("first (plain)");
    let replay_a = a
        .execute_bound_predicate_update_in_session(
            &viewer(),
            &SessionState::default(),
            TABLE,
            Some(&op("rp-1")),
            preds,
        )
        .map(|o| o.rows_affected)
        .map_err(|e| e.wire_code().to_string());
    let replay_b = b
        .execute_bound_predicate_update_in_session(
            &viewer(),
            &SessionState::default(),
            TABLE,
            Some(&op("rp-1")),
            preds,
        )
        .map(|o| o.rows_affected)
        .map_err(|e| e.wire_code().to_string());
    assert_eq!(replay_a, replay_b);
}
