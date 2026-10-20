//! 宣言 PRIMARY KEY の等価述語を永続一意索引の点検索で実行する広域取得の結合テスト
//! （Issue #2198。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-16・
//! TABLE-12、`rls.md` RLS-9・RLS-10、`sql-surface.md` SQL-27・SQL-15、`error-format.md` ERR-6）。
//!
//! 検証の軸は 2 つ。(1) PK の全構成列がリテラル等価のときだけ `EXPLAIN` が
//! `index_primary_key`／`primary_key_lookup` になり、それ以外は従来どおり
//! `plain_scan`／`full_scan` のまま変わらないこと。(2) どの経路でも結果（行・順序・`OFFSET` の数え方）
//! が、PK を宣言しない同形の表の全走査とバイト単位で一致すること（テナント境界を含む）。
//! 実際に索引経路を通ったことの構造的証拠（探査回数・縮退回数）は
//! `src/sql/pk_point.rs` の単体テストが計測フックで示す。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

/// 結果行の先頭セル（`SELECT id ...` の宣言済み `id` 列）を `u64` で返す。採番方式の表は
/// `ResultRow::id` に内部 rowid を載せない（TABLE-23）ため、利用者向けの値はセルから読む。
fn row_id(r: &engine::sql::exec::ResultRow) -> u64 {
    match r.cells.first() {
        Some(engine::sql::exec::Cell::SignedInteger(n)) => u64::try_from(*n).expect("id >= 0"),
        Some(engine::sql::exec::Cell::Integer(n)) => *n,
        other => panic!("unexpected id cell: {other:?}"),
    }
}

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// Public のみ可視な既定のセッション文脈。
fn public_only(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

/// Public と Private の両方を可視とする文脈。SQL の `INSERT` は可視性を常に Private に固定するため、
/// SQL 経由のテストはこの文脈（自テナントの行だけが見える）で読み書きする。
fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run(core: &EngineCore, ctx: &PolicyContext, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session_legacy(ctx, s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn code_of(core: &EngineCore, ctx: &PolicyContext, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session_legacy(ctx, s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 結果行を `(id, セルの Debug 表現)` の列にする（表名に依存しない比較用）。
fn rows_of(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<(u64, String)> {
    let mut s = SessionState::default();
    match run(core, ctx, &mut s, sql) {
        SqlOutcome::Query(QueryResult { rows, .. }) => rows
            .iter()
            .map(|r| (row_id(r), format!("{:?}", r.cells)))
            .collect(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn explain_lines(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let mut s = SessionState::default();
    match run(core, ctx, &mut s, &format!("EXPLAIN {sql}")) {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|r| match &r.cells[0] {
                Cell::Text(t) => t.clone(),
                other => panic!("expected text cell, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn assert_point(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let lines = explain_lines(core, ctx, sql);
    assert!(
        lines.iter().any(|l| l == "scalar_plan: index_primary_key"),
        "{sql}: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "access_path: primary_key_lookup"),
        "{sql}: {lines:?}"
    );
}

fn assert_full_scan(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let lines = explain_lines(core, ctx, sql);
    assert!(
        lines.iter().any(|l| l == "scalar_plan: plain_scan"),
        "{sql}: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "access_path: full_scan"),
        "{sql}: {lines:?}"
    );
}

/// `pk`（PK あり）と `plain`（PK なし）の 2 表へ同じ行を同じ id で入れる。
struct Pair {
    core: EngineCore,
    _guard: CleanupGuard,
}

fn pair(label: &str, columns: &str, pk_clause: &str) -> Pair {
    let (core, path) = new_core(label);
    let guard = CleanupGuard(path);
    let mut s = ddl_session();
    let owner = ctx("owner");
    run(
        &core,
        &owner,
        &mut s,
        &format!("CREATE TABLE pk ({columns}, {pk_clause})"),
    );
    run(
        &core,
        &owner,
        &mut s,
        &format!("CREATE TABLE plain ({columns})"),
    );
    Pair {
        core,
        _guard: guard,
    }
}

impl Pair {
    /// 両表へ同じ INSERT を実行する（`{T}` を表名に置換）。
    fn insert(&self, tenant: &str, template: &str) {
        let ctx = ctx(tenant);
        let mut s = SessionState::default();
        for table in ["pk", "plain"] {
            run(&self.core, &ctx, &mut s, &template.replace("{T}", table));
        }
    }

    /// 両表で同じ SELECT を実行し、結果が一致することを確認して返す。
    fn same(&self, ctx: &PolicyContext, template: &str) -> Vec<(u64, String)> {
        let a = rows_of(&self.core, ctx, &template.replace("{T}", "pk"));
        let b = rows_of(&self.core, ctx, &template.replace("{T}", "plain"));
        assert_eq!(a, b, "result mismatch for {template}");
        a
    }
}

// --- 結果の一致・EXPLAIN（単一列・複合） -------------------------------------

#[test]
fn single_column_pk_point_matches_full_scan_across_tenants() {
    let p = pair(
        "pkpt-single",
        "code TEXT NOT NULL, note TEXT",
        "PRIMARY KEY (code)",
    );
    // 3 テナントがそれぞれ同じキー集合を持つ（テナントごとの一意性）。
    for (n, tenant) in ["alice", "bob", "carol"].into_iter().enumerate() {
        for k in 1..=4 {
            let id = n * 10 + k;
            p.insert(
                tenant,
                &format!("INSERT INTO {{T}} (id, code, note) VALUES ({id}, 'k{k}', '{tenant}')"),
            );
        }
    }
    let ctx = ctx("alice");
    let hit = p.same(
        &ctx,
        "SELECT id, code, note FROM {T} WHERE code = 'k2' LIMIT 10",
    );
    // SQL の INSERT は可視性を Private に固定するため、各テナントは自分の行だけを見る。
    assert_eq!(hit.iter().map(|r| r.0).collect::<Vec<_>>(), vec![2]);
    for (tenant, expected) in [("bob", 12), ("carol", 22)] {
        let hit = p.same(
            &self::ctx(tenant),
            "SELECT id FROM {T} WHERE code = 'k2' LIMIT 10",
        );
        assert_eq!(hit.iter().map(|r| r.0).collect::<Vec<_>>(), vec![expected]);
    }
    let none = p.same(&ctx, "SELECT id FROM {T} WHERE code = 'zz' LIMIT 10");
    assert!(none.is_empty());
    assert_point(
        &p.core,
        &ctx,
        "SELECT id FROM pk WHERE code = 'k2' LIMIT 10",
    );
    // PK を宣言しない表は従来どおり。
    assert_full_scan(
        &p.core,
        &ctx,
        "SELECT id FROM plain WHERE code = 'k2' LIMIT 10",
    );
}

#[test]
fn composite_pk_requires_every_column_to_be_an_equality() {
    let p = pair(
        "pkpt-composite",
        "a TEXT NOT NULL, b INTEGER NOT NULL, note TEXT",
        "PRIMARY KEY (a, b)",
    );
    for (id, a, b) in [(1, "x", 1), (2, "x", 2), (3, "y", 1), (4, "y", 2)] {
        p.insert(
            "alice",
            &format!("INSERT INTO {{T}} (id, a, b, note) VALUES ({id}, '{a}', {b}, 'n')"),
        );
        p.insert(
            "bob",
            &format!(
                "INSERT INTO {{T}} (id, a, b, note) VALUES ({}, '{a}', {b}, 'm')",
                id + 100
            ),
        );
    }
    let ctx = ctx("alice");
    let full = "SELECT id, note FROM {T} WHERE a = 'y' AND b = 2 LIMIT 10";
    assert_eq!(
        p.same(&ctx, full).iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![4]
    );
    // 順序を入れ替えても、等価が他の述語と混在しても点検索になる。
    assert_point(
        &p.core,
        &ctx,
        "SELECT id FROM pk WHERE b = 2 AND note = 'n' AND a = 'y' LIMIT 10",
    );
    assert_eq!(
        p.same(
            &ctx,
            "SELECT id FROM {T} WHERE b = 2 AND note = 'n' AND a = 'y' LIMIT 10"
        )
        .iter()
        .map(|r| r.0)
        .collect::<Vec<_>>(),
        vec![4]
    );
    // 一部の列だけ・範囲比較・OR・NOT・ORDER BY・条件なしでは従来どおり全走査。
    for sql in [
        "SELECT id FROM pk WHERE a = 'y' LIMIT 10",
        "SELECT id FROM pk WHERE a = 'y' AND b > 1 LIMIT 10",
        "SELECT id FROM pk WHERE a = 'y' OR b = 2 LIMIT 10",
        "SELECT id FROM pk WHERE NOT (a = 'y') AND b = 2 LIMIT 10",
        "SELECT id FROM pk WHERE a = 'y' AND b = 2 ORDER BY id LIMIT 10",
        "SELECT id FROM pk LIMIT 10",
    ] {
        assert_full_scan(&p.core, &ctx, sql);
    }
    // 全走査経路でも結果は一致する。
    for sql in [
        "SELECT id FROM {T} WHERE a = 'y' LIMIT 10",
        "SELECT id FROM {T} WHERE a = 'y' AND b > 1 LIMIT 10",
        "SELECT id FROM {T} WHERE a = 'y' OR b = 2 LIMIT 10",
    ] {
        p.same(&ctx, sql);
    }
}

// --- 型ごと ----------------------------------------------------------------

/// `(列型 DDL, 3 件の値の SQL リテラル)`。比較に使うリテラルは INSERT と同じ文字列。
const TYPE_CASES: &[(&str, [&str; 3])] = &[
    ("TEXT", ["'a'", "'b'", "'c'"]),
    ("VARCHAR(8)", ["'a'", "'bb'", "'ccc'"]),
    ("CHAR(4)", ["'a'", "'bb'", "'ccc'"]),
    ("SMALLINT", ["1", "-2", "32767"]),
    ("INTEGER", ["1", "-2", "2147483647"]),
    ("BIGINT", ["1", "-2", "9007199254740991"]),
    ("BOOLEAN", ["true", "false", "true"]),
    ("DATE", ["'2024-01-02'", "'1999-12-31'", "'2030-06-15'"]),
    (
        "TIMESTAMP",
        [
            "'2024-01-02 03:04:05'",
            "'1999-12-31 23:59:59'",
            "'2030-06-15 00:00:00.5'",
        ],
    ),
    (
        "UUID",
        [
            "'123e4567-e89b-12d3-a456-426614174000'",
            "'00000000-0000-0000-0000-000000000001'",
            "'ffffffff-ffff-ffff-ffff-ffffffffffff'",
        ],
    ),
    ("BYTEA", ["'\\xdeadbeef'", "'\\x'", "'\\x00ff'"]),
];

#[test]
fn every_allowed_pk_type_uses_the_point_lookup_and_matches_full_scan() {
    for (ty, values) in TYPE_CASES {
        let p = pair(
            &format!("pkpt-type-{}", ty.to_lowercase().replace(['(', ')'], "")),
            &format!("k {ty} NOT NULL, note TEXT"),
            "PRIMARY KEY (k)",
        );
        // BOOLEAN は値が 2 種類しかないため、3 件目はテナントを分けて入れる。
        for (i, v) in values.iter().enumerate() {
            let tenant = if *ty == "BOOLEAN" && i == 2 {
                "bob"
            } else {
                "alice"
            };
            p.insert(
                tenant,
                &format!(
                    "INSERT INTO {{T}} (id, k, note) VALUES ({}, {v}, 'n{i}')",
                    i + 1
                ),
            );
        }
        let ctx = ctx("alice");
        for v in values {
            let sql = format!("SELECT id, k, note FROM {{T}} WHERE k = {v} LIMIT 10");
            let hit = p.same(&ctx, &sql);
            assert!(!hit.is_empty(), "{ty} {v}: expected a hit");
            assert_point(
                &p.core,
                &ctx,
                &format!("SELECT id FROM pk WHERE k = {v} LIMIT 10"),
            );
        }
    }
}

#[test]
fn enum_pk_uses_the_point_lookup() {
    let (core, path) = new_core("pkpt-enum");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let ctx = ctx("alice");
    run(
        &core,
        &ctx,
        &mut s,
        "CREATE TYPE mood AS ENUM ('happy', 'sad', 'angry')",
    );
    run(
        &core,
        &ctx,
        &mut s,
        "CREATE TABLE pk (k mood NOT NULL, PRIMARY KEY (k))",
    );
    for (i, v) in ["happy", "sad", "angry"].iter().enumerate() {
        run(
            &core,
            &ctx,
            &mut s,
            &format!("INSERT INTO pk (id, k) VALUES ({}, '{v}')", i + 1),
        );
    }
    let sql = "SELECT id, k FROM pk WHERE k = 'sad' LIMIT 10";
    assert_point(&core, &ctx, sql);
    assert_eq!(
        rows_of(&core, &ctx, sql)
            .iter()
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn char_n_pk_pads_the_literal_like_the_stored_value() {
    let p = pair("pkpt-char", "k CHAR(4) NOT NULL", "PRIMARY KEY (k)");
    p.insert("alice", "INSERT INTO {T} (id, k) VALUES (1, 'ab')");
    p.insert("alice", "INSERT INTO {T} (id, k) VALUES (2, 'abcd')");
    let ctx = ctx("alice");
    // 末尾空白の有無にかかわらず同じ行（CHAR(n) は末尾空白を無視して比較する）。
    for lit in ["'ab'", "'ab  '", "'ab '"] {
        let hit = p.same(
            &ctx,
            &format!("SELECT id FROM {{T}} WHERE k = {lit} LIMIT 10"),
        );
        assert_eq!(
            hit.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![1],
            "{lit}"
        );
    }
    // n 超過のリテラルは一致なし（結果は全走査と同じ）。
    assert!(p
        .same(&ctx, "SELECT id FROM {T} WHERE k = 'abcde' LIMIT 10")
        .is_empty());
}

#[test]
fn integer_literals_that_cannot_be_matched_exactly_fall_back_to_full_scan() {
    let p = pair("pkpt-int-edge", "k BIGINT NOT NULL", "PRIMARY KEY (k)");
    p.insert("alice", "INSERT INTO {T} (id, k) VALUES (1, 5)");
    p.insert(
        "alice",
        "INSERT INTO {T} (id, k) VALUES (2, 9007199254740991)",
    );
    let ctx = ctx("alice");
    assert_point(&p.core, &ctx, "SELECT id FROM pk WHERE k = 5 LIMIT 10");
    // |v| >= 2^53 は f64 比較と整数の等価が食い違いうるため点検索にしない。
    assert_full_scan(
        &p.core,
        &ctx,
        "SELECT id FROM pk WHERE k = 9007199254740992 LIMIT 10",
    );
    // 小数は一致する行が無い。
    assert_full_scan(&p.core, &ctx, "SELECT id FROM pk WHERE k = 5.5 LIMIT 10");
    assert!(p
        .same(&ctx, "SELECT id FROM {T} WHERE k = 5.5 LIMIT 10")
        .is_empty());
    assert_eq!(
        p.same(
            &ctx,
            "SELECT id FROM {T} WHERE k = 9007199254740991 LIMIT 10"
        )
        .len(),
        1
    );
}

#[test]
fn smallint_and_integer_out_of_range_literals_never_match() {
    let p = pair("pkpt-smallint", "k SMALLINT NOT NULL", "PRIMARY KEY (k)");
    p.insert("alice", "INSERT INTO {T} (id, k) VALUES (1, 7)");
    let ctx = ctx("alice");
    assert_point(&p.core, &ctx, "SELECT id FROM pk WHERE k = 7 LIMIT 10");
    assert_eq!(
        p.same(&ctx, "SELECT id FROM {T} WHERE k = 7 LIMIT 10")
            .len(),
        1
    );
    // 型の値域外（束縛がエラーにする場合も含め、PK あり・なしで同じ応答であること）。
    let mut s = SessionState::default();
    let a = code_of(
        &p.core,
        &ctx,
        &mut s,
        "SELECT id FROM pk WHERE k = 70000 LIMIT 10",
    );
    let b = code_of(
        &p.core,
        &ctx,
        &mut s,
        "SELECT id FROM plain WHERE k = 70000 LIMIT 10",
    );
    assert_eq!(a, b);
}

// --- OFFSET・投影・述語の併用 -----------------------------------------------

#[test]
fn offset_projection_and_extra_predicates_match_full_scan() {
    let p = pair(
        "pkpt-offset",
        "code TEXT NOT NULL, qty INTEGER",
        "PRIMARY KEY (code)",
    );
    for (n, tenant) in ["alice", "bob"].into_iter().enumerate() {
        p.insert(
            tenant,
            &format!(
                "INSERT INTO {{T}} (id, code, qty) VALUES ({}, 'k', {})",
                n + 1,
                (n + 1) * 10
            ),
        );
    }
    let ctx = ctx("alice");
    for sql in [
        "SELECT id FROM {T} WHERE code = 'k' LIMIT 10 OFFSET 1",
        "SELECT id FROM {T} WHERE code = 'k' LIMIT 10 OFFSET 2",
        "SELECT id FROM {T} WHERE code = 'k' LIMIT 1",
        "SELECT id, qty * 2 AS d, code FROM {T} WHERE code = 'k' LIMIT 10",
        "SELECT id FROM {T} WHERE code = 'k' AND qty > 15 LIMIT 10",
        "SELECT id FROM {T} WHERE code = 'k' AND (qty = 10 OR qty = 20) LIMIT 10",
        "SELECT id FROM {T} WHERE code = 'k' AND code = 'other' LIMIT 10",
    ] {
        p.same(&ctx, sql);
    }
    assert_point(
        &p.core,
        &ctx,
        "SELECT id FROM pk WHERE code = 'k' AND (qty = 10 OR qty = 20) LIMIT 10",
    );
}

// --- テナント境界 ------------------------------------------------------------

/// 3 テナントが同じキーの Public 行を持ち、別の 1 テナントが同じキーの Private 行を持つ。
/// 点検索の結果は PK なしの全走査と一致し、他テナントの Private 行は結果にも OFFSET の数にも
/// 現れず、応答は「該当行なし」と同一になる（RLS-9・RLS-10）。
#[test]
fn point_lookup_is_identical_to_full_scan_under_every_visibility_context() {
    let path = unique_db_path("pkpt-private");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    for table in ["pk", "plain"] {
        let mut schema = engine::catalog::TableSchema::new(
            table,
            vec![engine::catalog::ColumnDef::new(
                "code",
                engine::catalog::ColumnType::Text,
                false,
            )],
        );
        if table == "pk" {
            schema = schema.with_primary_key(vec!["code".to_string()]);
        }
        storage.create_table(&schema).expect("create table");
        for (id, tenant, vis) in [
            (1u64, "alice", Visibility::Public),
            (2, "bob", Visibility::Private),
            (3, "carol", Visibility::Public),
            (4, "dave", Visibility::Public),
        ] {
            let op_id =
                engine::recovery::required_op_id::OperationId::parse(&format!("{table}-op-{id}"))
                    .expect("op id");
            engine::tenant::insert_typed_row(
                &storage,
                table,
                &ctx(tenant),
                id,
                vis,
                &[engine::row_codec::Value::Text("k".to_string())],
                &op_id,
            )
            .expect("insert");
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ids = |c: &PolicyContext, sql: &str| -> Vec<u64> {
        rows_of(&core, c, sql).iter().map(|r| r.0).collect()
    };
    let expectations: Vec<(PolicyContext, Vec<u64>)> = vec![
        // Public のみ: 他テナントの Private 行（bob）は現れない。
        (public_only("alice"), vec![1, 3, 4]),
        (public_only("erin"), vec![1, 3, 4]),
        // bob 自身が Private を許可したときだけ自分の Private 行が見える。
        (public_only("bob"), vec![1, 3, 4]),
        (ctx("bob"), vec![1, 2, 3, 4]),
        (ctx("alice"), vec![1, 3, 4]),
    ];
    for (c, expected) in &expectations {
        for suffix in [
            "LIMIT 10",
            "LIMIT 10 OFFSET 1",
            "LIMIT 2",
            "LIMIT 1 OFFSET 2",
        ] {
            let on_pk = ids(
                c,
                &format!("SELECT id, code FROM pk WHERE code = 'k' {suffix}"),
            );
            let on_plain = ids(
                c,
                &format!("SELECT id, code FROM plain WHERE code = 'k' {suffix}"),
            );
            assert_eq!(on_pk, on_plain, "{suffix}");
        }
        assert_eq!(
            &ids(c, "SELECT id FROM pk WHERE code = 'k' LIMIT 10"),
            expected
        );
    }
    // 他テナントの Private 行にだけ存在するキー相当の問い合わせは、存在しないキーと応答が同一。
    let alice = public_only("alice");
    let only_private = rows_of(
        &core,
        &alice,
        "SELECT id, code FROM pk WHERE code = 'k' AND id = 2 LIMIT 10",
    );
    let nothing = rows_of(
        &core,
        &alice,
        "SELECT id, code FROM pk WHERE code = 'zzz' LIMIT 10",
    );
    assert_eq!(only_private, nothing);
    assert!(nothing.is_empty());
}

// --- stale な正引き・更新 ------------------------------------------------------

#[test]
fn delete_update_and_reinsert_keep_results_identical_to_full_scan() {
    let p = pair(
        "pkpt-stale",
        "code TEXT NOT NULL, v INTEGER",
        "PRIMARY KEY (code)",
    );
    let ctx = ctx("alice");
    let mut s = SessionState::default();
    p.insert("alice", "INSERT INTO {T} (id, code, v) VALUES (1, 'a', 1)");
    p.insert("alice", "INSERT INTO {T} (id, code, v) VALUES (2, 'b', 2)");
    let q = "SELECT id, code, v FROM {T} WHERE code = 'a' LIMIT 10";
    assert_eq!(p.same(&ctx, q).len(), 1);

    // DELETE の後は空。
    for t in ["pk", "plain"] {
        run(
            &p.core,
            &ctx,
            &mut s,
            &format!("DELETE FROM {t} WHERE id = 1"),
        );
    }
    assert!(p.same(&ctx, q).is_empty());

    // 同じキーで新しい行を入れると新しい行が返る。
    p.insert("alice", "INSERT INTO {T} (id, code, v) VALUES (3, 'a', 3)");
    assert_eq!(
        p.same(&ctx, q).iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![3]
    );

    // PK 列の UPDATE の後は旧キーが空、新キーが返る。
    for t in ["pk", "plain"] {
        run(
            &p.core,
            &ctx,
            &mut s,
            &format!("UPDATE {t} SET code = 'c' WHERE id = 3"),
        );
    }
    assert!(p.same(&ctx, q).is_empty());
    assert_eq!(
        p.same(&ctx, "SELECT id FROM {T} WHERE code = 'c' LIMIT 10")
            .iter()
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        vec![3]
    );
}

#[test]
fn duplicate_primary_key_is_23505_and_scoped_per_tenant() {
    let (core, path) = new_core("pkpt-dup");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        &mut s,
        "CREATE TABLE pk (a TEXT, b INTEGER, PRIMARY KEY (a, b))",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, a, b) VALUES (1, 'x', 1)",
    );
    assert_eq!(
        code_of(
            &core,
            &alice,
            &mut s,
            "INSERT INTO pk (id, a, b) VALUES (2, 'x', 1)"
        ),
        "23505"
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, a, b) VALUES (2, 'x', 2)",
    );
    // 別テナントの同値は違反にならない。
    let bob = ctx("bob");
    run(
        &core,
        &bob,
        &mut s,
        "INSERT INTO pk (id, a, b) VALUES (3, 'x', 1)",
    );
    // PK 列は暗黙に NOT NULL。
    assert_eq!(
        code_of(
            &core,
            &alice,
            &mut s,
            "INSERT INTO pk (id, a) VALUES (9, 'z')"
        ),
        "23502"
    );
}

// --- ADD／DROP PRIMARY KEY ----------------------------------------------------

#[test]
fn add_and_drop_primary_key_follow_the_current_schema_and_keep_results() {
    let (core, path) = new_core("pkpt-alter");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let alice = ctx("alice");
    let bob = ctx("bob");
    run(&core, &alice, &mut s, "CREATE TABLE t (code TEXT NOT NULL)");
    run(
        &core,
        &alice,
        &mut s,
        "CREATE TABLE ref (code TEXT NOT NULL)",
    );
    for (ctx, id, c) in [(&alice, 1, "a"), (&alice, 2, "b"), (&bob, 3, "a")] {
        for t in ["t", "ref"] {
            run(
                &core,
                ctx,
                &mut s,
                &format!("INSERT INTO {t} (id, code) VALUES ({id}, '{c}')"),
            );
        }
    }
    let q = |t: &str| format!("SELECT id FROM {t} WHERE code = 'a' LIMIT 10");
    assert_full_scan(&core, &alice, &q("t"));

    // 行がある表へ ADD した直後（テナントのマーカー無し）でも結果は全走査と一致する。
    run(
        &core,
        &alice,
        &mut s,
        "ALTER TABLE t ADD PRIMARY KEY (code)",
    );
    assert_point(&core, &alice, &q("t"));
    assert_eq!(
        rows_of(&core, &alice, &q("t")),
        rows_of(&core, &alice, &q("ref"))
    );

    // 一部のテナントだけ書き込むと、混在した状態（alice は索引、bob は縮退）でも一致する。
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO t (id, code) VALUES (4, 'c')",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO ref (id, code) VALUES (4, 'c')",
    );
    assert_eq!(
        rows_of(&core, &alice, &q("t")),
        rows_of(&core, &alice, &q("ref"))
    );
    assert_eq!(
        rows_of(&core, &alice, &q("t"))
            .iter()
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        vec![1]
    );

    // DROP 後は全走査に戻り、結果は変わらない。
    run(
        &core,
        &alice,
        &mut s,
        "ALTER TABLE t DROP CONSTRAINT t_pkey",
    );
    assert_full_scan(&core, &alice, &q("t"));
    assert_eq!(
        rows_of(&core, &alice, &q("t")),
        rows_of(&core, &alice, &q("ref"))
    );
}

// --- 文脈（明示トランザクション・一時表・ビュー・カーソル） ---------------------

#[test]
fn explicit_transaction_sees_its_own_uncommitted_rows_through_the_index() {
    let (core, path) = new_core("pkpt-txn");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        &mut s,
        "CREATE TABLE pk (code TEXT PRIMARY KEY)",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, code) VALUES (1, 'a')",
    );

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut s,
        &mut txn,
        "INSERT INTO pk (id, code) VALUES (2, 'b')",
    )
    .expect("insert");
    let ids = |out: SqlOutcome| match out {
        SqlOutcome::Query(r) => r.rows.iter().map(row_id).collect::<Vec<_>>(),
        other => panic!("expected Query, got {other:?}"),
    };
    let inside = core
        .execute_sql_in_txn(
            &alice,
            &mut s,
            &mut txn,
            "SELECT id FROM pk WHERE code = 'b' LIMIT 5",
        )
        .expect("select in txn");
    assert_eq!(ids(inside), vec![2]);
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(rows_of(&core, &alice, "SELECT id FROM pk WHERE code = 'b' LIMIT 5").is_empty());
    assert_eq!(
        rows_of(&core, &alice, "SELECT id FROM pk WHERE code = 'a' LIMIT 5").len(),
        1
    );
}

#[test]
fn view_over_pk_table_uses_the_point_lookup_and_matches() {
    let (core, path) = new_core("pkpt-view");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        &mut s,
        "CREATE TABLE pk (code TEXT PRIMARY KEY, note TEXT)",
    );
    run(&core, &alice, &mut s, "CREATE VIEW v AS SELECT * FROM pk");
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, code, note) VALUES (1, 'a', 'x')",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, code, note) VALUES (2, 'b', 'y')",
    );
    let via_view = rows_of(
        &core,
        &alice,
        "SELECT id, code FROM v WHERE code = 'b' LIMIT 5",
    );
    let direct = rows_of(
        &core,
        &alice,
        "SELECT id, code FROM pk WHERE code = 'b' LIMIT 5",
    );
    assert_eq!(via_view, direct);
    assert_eq!(direct.len(), 1);
}

#[test]
fn temp_table_with_pk_matches() {
    let (core, path) = new_core("pkpt-temp");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT NOT NULL UNIQUE, code TEXT PRIMARY KEY)",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO tmp (id, code) VALUES (1, 'a')",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO tmp (id, code) VALUES (2, 'b')",
    );
    let out = run(
        &core,
        &alice,
        &mut s,
        "SELECT id FROM tmp WHERE code = 'b' LIMIT 5",
    );
    match out {
        SqlOutcome::Query(r) => {
            assert_eq!(r.rows.iter().map(row_id).collect::<Vec<_>>(), vec![2])
        }
        other => panic!("expected Query, got {other:?}"),
    }
    let miss = run(
        &core,
        &alice,
        &mut s,
        "SELECT id FROM tmp WHERE code = 'zz' LIMIT 5",
    );
    match miss {
        SqlOutcome::Query(r) => assert!(r.rows.is_empty()),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn declared_cursor_over_pk_point_scan_matches_direct_execution() {
    let (core, path) = new_core("pkpt-cursor");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        &mut s,
        "CREATE TABLE pk (code TEXT PRIMARY KEY)",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, code) VALUES (1, 'a')",
    );
    run(
        &core,
        &alice,
        &mut s,
        "INSERT INTO pk (id, code) VALUES (2, 'b')",
    );
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut s,
        &mut txn,
        "DECLARE c CURSOR FOR SELECT id FROM pk WHERE code = 'b' LIMIT 10",
    )
    .expect("declare");
    let fetched = core
        .execute_sql_in_txn(&alice, &mut s, &mut txn, "FETCH 5 FROM c")
        .expect("fetch");
    let SqlOutcome::Fetch(result) = fetched else {
        panic!("expected Fetch");
    };
    assert_eq!(result.rows.iter().map(row_id).collect::<Vec<_>>(), vec![2]);
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "COMMIT")
        .expect("commit");
}
