//! `MERGE` の 3 テナント越境の網羅試験（SQL-79・RLS-10・RLS-13・Issue #3358。設計記録
//! `docs/design/sql-merge.md` D8・§14）。
//!
//! alice・bob・carol が対象 `t` と元 `s` の両方に行を持ち、両表に利用者ポリシー（SELECT／UPDATE／DELETE／
//! INSERT）を置いた世界で、alice の MERGE を句の種類ごとに流す。他テナント（bob・carol）の行は、
//! 一致・BY SOURCE・INSERT の材料・`21000`／`42501` の引き金のどれにもならない「おとり」にしてある。
//! 検査は次の 4 点（期待値は直書きせず「alice の行だけを持つ世界」の結果との差分オラクルで取る）。
//!
//! 1. alice の結果（件数・最終状態・`RETURNING` 行・SQLSTATE と文言）が alice だけの世界と一致する。
//! 2. bob・carol から見た `t`・`s` の全行が MERGE の前後でバイト単位で同一。
//! 3. エラー応答の文言に他テナントの値・件数・テナント id が含まれず、3 テナントで同じ拒否を起こすと
//!    文言がバイト一致する。
//! 4. 他テナントの Public の元の行は SELECT の JOIN と同じく見える一方、他テナントの Public の対象行は
//!    書き込まれない（D8 の非対称性。別テストで実測値を固定する）。
//!
//! 流し方は autocommit・明示 Tx（BEGIN → MERGE → COMMIT）の両方で、末尾 `;` の有無も両方。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

type Failure = (String, String);

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 世界の構成。`others` が偽なら alice の行だけ（差分オラクル用）。`public_decoys` が真なら bob に
/// Public の行を足す。`alice_dup` が真なら alice の元に同じキーの行を足す（`21000` を起こす）。
#[derive(Clone, Copy)]
struct Shape {
    others: bool,
    public_decoys: bool,
    alice_dup: bool,
}

struct Seed {
    tenant: &'static str,
    table: &'static str,
    id: u64,
    k: &'static str,
    n: i32,
    public: bool,
}

fn seeds(shape: Shape) -> Vec<Seed> {
    let s = |tenant, table, id, k, n, public| Seed {
        tenant,
        table,
        id,
        k,
        n,
        public,
    };
    let mut v = vec![
        s("alice", "t", 1, "a", 10, false),
        s("alice", "t", 2, "b", 20, false),
        s("alice", "t", 3, "c", 30, false),
        s("alice", "s", 1, "a", 100, false),
        s("alice", "s", 3, "c", 300, false),
        s("alice", "s", 4, "d", 400, false),
    ];
    if shape.alice_dup {
        v.push(s("alice", "s", 5, "a", 150, false));
    }
    if shape.others {
        // おとり: 同じキー・複数の元（漏れれば 21000）・ポリシー違反の値（漏れれば 42501）・
        // BY SOURCE の対象になる対象行（漏れれば更新・削除される）。
        v.extend([
            s("bob", "t", 11, "a", 77, false),
            s("bob", "t", 13, "z", 79, false),
            s("bob", "t", 14, "b", 150, false),
            s("bob", "s", 21, "a", 900, false),
            s("bob", "s", 23, "a", 902, false),
            s("carol", "t", 31, "a", 55, false),
            s("carol", "t", 32, "q", 56, false),
            s("carol", "s", 41, "a", 800, false),
            s("carol", "s", 42, "b", 801, false),
        ]);
    }
    if shape.others && shape.public_decoys {
        v.extend([
            s("bob", "t", 12, "c", 78, true),
            s("bob", "s", 22, "d", 901, true),
        ]);
    }
    v
}

fn world(shape: Shape) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql79-merge-xtenant");
    let storage = Storage::open(&path).expect("open storage");
    for (name, col) in [("t", "x"), ("s", "v")] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("k", ColumnType::Text, false),
                    ColumnDef::new(col, ColumnType::Integer, true),
                ],
            ))
            .expect("create table");
    }
    for r in seeds(shape) {
        let op =
            OperationId::parse(&format!("seed-{}-{}-{}", r.table, r.tenant, r.id)).expect("op id");
        engine::tenant::insert_typed_row(
            &storage,
            r.table,
            &ctx_of(r.tenant),
            r.id,
            if r.public {
                Visibility::Public
            } else {
                Visibility::Private
            },
            &[
                Value::Vector(vec![0.1, 0.2]),
                Value::Text(r.k.to_string()),
                Value::Integer(r.n),
            ],
            &op,
        )
        .unwrap_or_else(|e| panic!("seed {} {} {}: {e:?}", r.table, r.tenant, r.id));
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 利用者ポリシーはテナントごとに持つため、3 テナントそれぞれの DDL セッションで同じ定義を置く。
    for tenant in ["alice", "bob", "carol"] {
        for ddl in [
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY t_sel ON t FOR SELECT USING (x > 0)",
            "CREATE POLICY t_upd ON t FOR UPDATE USING (x < 100) WITH CHECK (x < 500)",
            "CREATE POLICY t_del ON t FOR DELETE USING (x < 25)",
            "CREATE POLICY t_ins ON t FOR INSERT WITH CHECK (x < 500)",
            "ALTER TABLE s ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY s_sel ON s FOR SELECT USING (v > 0)",
        ] {
            let mut session = SessionState::default();
            session.allow_ddl();
            core.execute_sql_in_session(&ctx_of(tenant), &mut session, ddl)
                .unwrap_or_else(|e| panic!("{tenant}: {ddl}: {e:?}"));
        }
    }
    (core, CleanupGuard(path))
}

fn cell(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::SignedInteger(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn column_name(c: &ColumnMeta) -> String {
    match c {
        ColumnMeta::Id => "id".to_string(),
        ColumnMeta::AliasedId { name }
        | ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::Record { name, .. } => name.clone(),
    }
}

fn rows_text(r: &engine::sql::exec::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| row.cells.iter().map(cell).collect::<Vec<_>>().join(","))
        .collect::<Vec<_>>()
        .join(";")
}

/// MERGE の応答を `n=<件数> cols=<列名> rows=<行>`（`RETURNING` が無ければ件数だけ）か
/// `err:<SQLSTATE>:<文言>` に整える。
fn render(out: Result<SqlOutcome, Failure>) -> String {
    match out {
        Ok(SqlOutcome::Merge(o)) => format!("n={}", o.rows_affected),
        Ok(SqlOutcome::Returning(o)) => format!(
            "n={} cols={} rows={}",
            o.rows_affected,
            o.result
                .columns
                .iter()
                .map(column_name)
                .collect::<Vec<_>>()
                .join(","),
            rows_text(&o.result)
        ),
        Ok(other) => panic!("unexpected outcome {other:?}"),
        Err((code, msg)) => format!("err:{code}:{msg}"),
    }
}

fn run_auto(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, Failure> {
    core.execute_sql_in_session(&ctx_of(tenant), &mut SessionState::default(), sql)
        .map_err(|e| (e.wire_code().to_string(), e.client_message()))
}

/// 明示 Tx（BEGIN → 対象文 → COMMIT。対象文が失敗したら ROLLBACK）で流す。
fn run_txn(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, Failure> {
    let ctx = ctx_of(tenant);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(&ctx, &mut session, &mut txn, sql)
        .map_err(|e| (e.wire_code().to_string(), e.client_message()));
    let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, end)
        .unwrap_or_else(|e| panic!("{end}: {e:?}"));
    out
}

fn view(core: &EngineCore, tenant: &str) -> String {
    let mut parts = Vec::new();
    for sql in [
        "SELECT id, k, x FROM t ORDER BY id LIMIT 100",
        "SELECT id, k, v FROM s ORDER BY id LIMIT 100",
    ] {
        match run_auto(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
            SqlOutcome::Query(r) => parts.push(rows_text(&r)),
            other => panic!("unexpected {other:?}"),
        }
    }
    parts.join(" || ")
}

const ON: &str = "MERGE INTO t USING s ON t.k = s.k";
const INS: &str = "INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)";

/// 句の種類ごとの MERGE（alice が流す）。`(名前, 文)`。
fn statements() -> Vec<(&'static str, String)> {
    vec![
        (
            "matched-update",
            format!("{ON} WHEN MATCHED THEN UPDATE SET x = s.v"),
        ),
        (
            "matched-delete",
            format!("{ON} WHEN MATCHED AND t.x < 25 THEN DELETE"),
        ),
        (
            "not-matched-insert",
            format!("{ON} WHEN NOT MATCHED THEN {INS}"),
        ),
        (
            "by-source-update",
            format!("{ON} WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = t.x + 1"),
        ),
        (
            "by-source-delete",
            format!("{ON} WHEN NOT MATCHED BY SOURCE AND t.x < 25 THEN DELETE"),
        ),
        (
            "do-nothing",
            format!("{ON} WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN DO NOTHING"),
        ),
        (
            "all-clauses-returning",
            format!(
                "{ON} WHEN MATCHED AND t.x < 20 THEN UPDATE SET x = s.v \
                 WHEN MATCHED THEN DO NOTHING \
                 WHEN NOT MATCHED THEN {INS} \
                 WHEN NOT MATCHED BY SOURCE AND t.x < 25 THEN DELETE \
                 RETURNING merge_action(), t.id, t.k, t.x, s.id, s.v"
            ),
        ),
        (
            "subquery-source",
            "MERGE INTO t USING (SELECT * FROM s WHERE v > 0) s ON t.k = s.k \
             WHEN MATCHED THEN UPDATE SET x = s.v"
                .to_string(),
        ),
        (
            "operation-id",
            format!("{ON} WHEN MATCHED THEN UPDATE SET x = s.v USING OPERATION_ID 'xt-1'"),
        ),
        (
            "update-using-violation",
            // alice 自身の元の行 (3,c,300) と、更新後が WITH CHECK に反する値を作る。
            format!("{ON} WHEN MATCHED THEN UPDATE SET x = s.v + 400"),
        ),
        (
            "delete-using-violation",
            format!("{ON} WHEN MATCHED THEN DELETE"),
        ),
    ]
}

fn flavors(sql: &str) -> Vec<(String, bool)> {
    vec![
        (sql.to_string(), false),
        (format!("{sql};"), false),
        (sql.to_string(), true),
        (format!("{sql};"), true),
    ]
}

fn run_flavor(core: &EngineCore, tenant: &str, sql: &str, explicit: bool) -> String {
    render(if explicit {
        run_txn(core, tenant, sql)
    } else {
        run_auto(core, tenant, sql)
    })
}

/// 差分オラクル: おとりのある世界の alice の結果と最終状態が、alice だけの世界と同じ。他テナントの
/// 全行は MERGE の前後で不変。
#[test]
fn alices_merge_is_unaffected_by_other_tenants_private_rows() {
    let mut raised = std::collections::BTreeSet::new();
    for (name, stmt) in statements() {
        for (sql, explicit) in flavors(&stmt) {
            for alice_dup in [false, true] {
                let full = Shape {
                    others: true,
                    public_decoys: false,
                    alice_dup,
                };
                let alone = Shape {
                    others: false,
                    ..full
                };
                let (core_f, _g1) = world(full);
                let (core_a, _g2) = world(alone);
                // 世界がおとりを持つこと（vacuous でない）。
                assert_ne!(view(&core_f, "bob"), "||");
                assert_ne!(view(&core_f, "carol"), "||");
                let bob_before = view(&core_f, "bob");
                let carol_before = view(&core_f, "carol");
                let got = run_flavor(&core_f, "alice", &sql, explicit);
                let want = run_flavor(&core_a, "alice", &sql, explicit);
                assert_eq!(
                    got, want,
                    "{name} explicit={explicit} dup={alice_dup}: {sql}"
                );
                assert_eq!(
                    view(&core_f, "alice"),
                    view(&core_a, "alice"),
                    "{name}: alice's final state"
                );
                assert_eq!(bob_before, view(&core_f, "bob"), "{name}: bob changed");
                assert_eq!(
                    carol_before,
                    view(&core_f, "carol"),
                    "{name}: carol changed"
                );
                if let Some(rest) = got.strip_prefix("err:") {
                    raised.insert(rest.split(':').next().unwrap_or("").to_string());
                    // 文言に他テナントの値・テナント id・件数が出ない。
                    for leak in ["bob", "carol", "77", "900", "902", "800", "801"] {
                        assert!(!got.contains(leak), "{name}: leaked {leak} in {got}");
                    }
                }
            }
        }
    }
    // 拒否の種類が実際に起きていること（vacuous でない）。
    assert!(raised.contains("42501"), "no 42501 case: {raised:?}");
    assert!(raised.contains("21000"), "no 21000 case: {raised:?}");
}

/// 3 テナントが同じ MERGE を流すと、各自の行だけが変わる。拒否は文言までバイト一致する。
#[test]
fn each_tenant_only_changes_its_own_rows_and_rejections_are_byte_identical() {
    let shape = Shape {
        others: true,
        public_decoys: false,
        alice_dup: false,
    };
    let (core, _g) = world(shape);
    let before = ["alice", "bob", "carol"].map(|t| view(&core, t));
    // bob の対象 (11,a,77)・carol の対象 (31,a,55) は UPDATE の USING（x < 100）を満たすが、元の側の
    // 値（900・902・800）が WITH CHECK（x < 500）を満たさないため、bob・carol 自身が流すと拒否される。
    let upd = format!("{ON} WHEN MATCHED THEN UPDATE SET x = s.v");
    let bob = render(run_auto(&core, "bob", &upd));
    assert!(
        bob.starts_with("err:42501:") || bob.starts_with("err:21000:"),
        "{bob}"
    );
    assert_eq!(
        view(&core, "alice"),
        before[0],
        "bob's rejected MERGE moved alice"
    );
    assert_eq!(view(&core, "carol"), before[2]);
    // 同じ拒否（carol の元には a が 1 行・bob と別の値）。文言は他テナントの値を含まない。
    let carol = render(run_auto(&core, "carol", &upd));
    for (who, text) in [("bob", &bob), ("carol", &carol)] {
        for other in ["alice", "bob", "carol"] {
            if other != who {
                assert!(!text.contains(other), "{who}: {text}");
            }
        }
    }
    // alice の成功は alice の行だけを変える。
    assert_eq!(render(run_auto(&core, "alice", &upd)), "n=2");
    assert_eq!(view(&core, "bob"), before[1]);
    assert_eq!(view(&core, "carol"), before[2]);
}

/// D8 の非対称性: 他テナントの Public の元の行は JOIN から見える（NOT MATCHED の材料になりうる）。
/// 他テナントの Public の対象行は、一致・BY SOURCE のどちらでも書き込まれない。
#[test]
fn another_tenants_public_target_rows_are_never_written() {
    let shape = Shape {
        others: true,
        public_decoys: true,
        alice_dup: false,
    };
    for (sql, explicit) in flavors(&format!(
        "{ON} WHEN MATCHED THEN UPDATE SET x = s.v \
         WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = t.x + 1000"
    )) {
        let (core, _g) = world(shape);
        let bob_t_before =
            match run_auto(&core, "bob", "SELECT id, k, x FROM t ORDER BY id LIMIT 100")
                .expect("bob t")
            {
                SqlOutcome::Query(r) => rows_text(&r),
                other => panic!("{other:?}"),
            };
        let out = run_flavor(&core, "alice", &sql, explicit);
        assert!(!out.contains("leak"), "{out}");
        // bob の Public の対象行 (12,c,78) は alice の MERGE で変わらない。
        let bob_t_after =
            match run_auto(&core, "bob", "SELECT id, k, x FROM t ORDER BY id LIMIT 100")
                .expect("bob t")
            {
                SqlOutcome::Query(r) => rows_text(&r),
                other => panic!("{other:?}"),
            };
        assert_eq!(bob_t_before, bob_t_after, "{sql}: {out}");
        assert!(bob_t_after.contains("12,c,78"), "{bob_t_after}");
    }
}
