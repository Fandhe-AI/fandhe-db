//! JOIN・サブクエリ・CTE・集合演算・ウィンドウ関数・ビューを横断する RLS 境界の
//! 機械検証（Issue #931。ポインタ: RLS-10 (b)・RLS-7・RLS-8・RLS-9・RLS-11、
//! SQL-28・SQL-29・SQL-30、TABLE-12・TABLE-18、TASK-212・TASK-213・TASK-214）。
//!
//! 既存テスト（`tests/sql28_inner_join.rs`・`tests/sql28_outer_join.rs`・
//! `tests/sql29_subquery.rs`・`tests/sql29_cte.rs`・`tests/sql29_set_operations.rs`・
//! `tests/sql30_window.rs`・`tests/table18_view.rs`）は各機能単位で 1〜3 件の RLS
//! テストを持つが、本ファイルは機能を横断して以下を単一のマトリクスで固定する
//! （`tests/rls_generalized.rs`（TASK-138）の流儀を関係演算経路へ一般化する）:
//!
//! 1. production の可視性判定（[`engine::policy::PolicyContext::is_visible`]）を
//!    一切呼ばない独立オラクルによる照合（T1）
//! 2. 他テナントの不可視行の有無で結果（行・順序・統計値）が変化しないことの
//!    完全差分比較（T2。件数・集計・順位・NULL 補完件数・EXISTS の真偽を含む）
//! 3. エラー応答（`wire_code`・`client_message`）が他テナント行の有無で変化
//!    しないこと（静的エラー＋データ依存カナリア、T3）
//! 4. 外部結合の NULL 補完・`EXISTS` の真偽が他テナントの行に依存しないこと（T4）
//! 5. `PolicyContext::new`（Public のみ）と `with_visibilities`（Private 込み）の
//!    双方（全テストが両モードで走る）
//! 6. 同一 `id`・同一結合キー・同一 `lang` を他テナントが保持する衝突（TABLE-12）
//! 7. キャッシュ（`relation_snapshot`／`generation_key`／`visible_cache`）の温め
//!    順序に依存しないこと（T5）
//!
//! 判定ヘルパ自体の健全性は T6（負の対照）で固定する。

use std::collections::HashSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 定数・フィクスチャテーブル ----------

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";
const TENANTS: [&str; 3] = [TENANT_A, TENANT_B, TENANT_C];

const AUTHORS: &str = "authors";
const DOCUMENTS: &str = "documents";
const LANGS: &str = "langs";
const OTHER_DOCS: &str = "other_docs";
const VIEW_JA_DOCS: &str = "ja_docs";

fn authors_schema() -> TableSchema {
    TableSchema::new(
        AUTHORS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("name", ColumnType::Text, false),
        ],
    )
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        DOCUMENTS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, false),
            ColumnDef::new("author_id", ColumnType::BigInt, false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("score", ColumnType::BigInt, false),
        ],
    )
}

fn langs_schema() -> TableSchema {
    TableSchema::new(
        LANGS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn other_docs_schema() -> TableSchema {
    TableSchema::new(
        OTHER_DOCS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `allow_private=false` は `PolicyContext::new`（Public のみ）、`true` は
/// `with_visibilities(tenant, [Public, Private])`（自テナント Private も可視）。
/// 全テストは両モードで走らせる（RLS-7・RLS-8 の両契約を固定するため）。
fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn unique_token(table: &str, tenant: &str, id: u64) -> String {
    format!("uniquetoken_{table}_{tenant}_{id}")
}

/// シード時の行の真実（オラクル用。production の可視性判定は一切通さない。
/// `tests/rls_generalized.rs` と同一方針）。
#[derive(Clone, Copy)]
struct RowTruth {
    table: &'static str,
    tenant: &'static str,
    id: u64,
    visibility: Visibility,
}

fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

fn insert_row(
    storage: &Storage,
    table: &str,
    tenant: &str,
    id: u64,
    visibility: Visibility,
    values: &[Value],
    truths: &mut Vec<RowTruth>,
) {
    // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容
    // ハッシュを持つため、テーブル・テナント・id の組から決定的に導出した
    // operation_id を使う（同一 3 つ組は本ファイル内で 1 度しか挿入しない）。
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let op_id =
        engine::recovery::required_op_id::OperationId::parse(&format!("op-{table}-{tenant}-{id}"))
            .expect("valid operation_id");
    engine::tenant::insert_typed_row(storage, table, &ctx, id, visibility, values, &op_id)
        .expect("insert row");
    truths.push(RowTruth {
        table: leak_table_name(table),
        tenant: leak_tenant_name(tenant),
        id,
        visibility,
    });
}

// `insert_row` はテスト定数（`&'static str`）のみを受け取るため、呼び出し側の
// 定数へ変換して `RowTruth` の寿命要件（`'static`）を満たす（未検証入力を
// 扱っているわけではない。SQL 文字列組み立てにも使わない）。
fn leak_table_name(table: &str) -> &'static str {
    match table {
        AUTHORS => AUTHORS,
        DOCUMENTS => DOCUMENTS,
        LANGS => LANGS,
        OTHER_DOCS => OTHER_DOCS,
        other => panic!("unexpected table in test fixture: {other}"),
    }
}

fn leak_tenant_name(tenant: &str) -> &'static str {
    match tenant {
        TENANT_A => TENANT_A,
        TENANT_B => TENANT_B,
        TENANT_C => TENANT_C,
        other => panic!("unexpected tenant in test fixture: {other}"),
    }
}

/// 共有フィクスチャを構築する。`viewer` は「Private 行を保持する自テナント」
/// （T2 の結果不変性比較を tenant-a 以外の閲覧テナントでも固定できるよう
/// 引数化してある。advisor 指摘）。`flood=false` は「`viewer` の Public/Private
/// 行 ＋ 他テナントの Public 行のみ」（baseline）、`flood=true` は baseline に
/// 「`viewer` 以外の 1 テナント（`TENANTS` 内で `viewer` の次に現れるもの。
/// 全呼び出しが `viewer=TENANT_A` の既存軸では従来どおり tenant-b になる）の
/// Private 行」（全テナント対称の Private 行 ＋ 結合キー・`lang` を故意に
/// 一致させた flood 行）を追加した状態（flooded）。baseline/flooded の差分が
/// 「他テナントの不可視行のみ」になるよう、Public 行は両方で同一内容にする
/// （TABLE-12: 同一 `id`／結合キー／`lang` を複数テナントが保持しても物理キー
/// `(tenant_id, id)` で分離されることの確認を兼ねる）。
fn seed_fixture(storage: &Storage, viewer: &str, flood: bool) -> Vec<RowTruth> {
    storage.create_table(&authors_schema()).expect("authors");
    storage
        .create_table(&documents_schema())
        .expect("documents");
    storage.create_table(&langs_schema()).expect("langs");
    storage
        .create_table(&other_docs_schema())
        .expect("other_docs");

    let mut truths = Vec::new();

    for &tenant in TENANTS.iter() {
        let include_private = tenant == viewer || flood;

        // authors: id=1 Public（結合キーとして全テナントが再利用）、id=2 Private。
        insert_row(
            storage,
            AUTHORS,
            tenant,
            1,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(unique_token(AUTHORS, tenant, 1)),
            ],
            &mut truths,
        );
        if include_private {
            insert_row(
                storage,
                AUTHORS,
                tenant,
                2,
                Visibility::Private,
                &[
                    Value::Vector(vec![2.0, 0.0]),
                    Value::Text(unique_token(AUTHORS, tenant, 2)),
                ],
                &mut truths,
            );
        }

        // documents: id=10 Public（author_id=1, lang=ja）、id=11 Private
        // （author_id=2, lang=ja）、id=12 Public（author_id=1, lang=en）、
        // id=13 Private（author_id=999: 一致する author が存在しない。
        // 外部結合の NULL 補完対象、T4 用）。
        insert_row(
            storage,
            DOCUMENTS,
            tenant,
            10,
            Visibility::Public,
            &[
                Value::Vector(vec![10.0, 0.0]),
                Value::Text(unique_token(DOCUMENTS, tenant, 10)),
                Value::BigInt(1),
                Value::Text("ja".to_string()),
                Value::BigInt(100),
            ],
            &mut truths,
        );
        if include_private {
            insert_row(
                storage,
                DOCUMENTS,
                tenant,
                11,
                Visibility::Private,
                &[
                    Value::Vector(vec![11.0, 0.0]),
                    Value::Text(unique_token(DOCUMENTS, tenant, 11)),
                    Value::BigInt(2),
                    Value::Text("ja".to_string()),
                    Value::BigInt(200),
                ],
                &mut truths,
            );
        }
        insert_row(
            storage,
            DOCUMENTS,
            tenant,
            12,
            Visibility::Public,
            &[
                Value::Vector(vec![12.0, 0.0]),
                Value::Text(unique_token(DOCUMENTS, tenant, 12)),
                Value::BigInt(1),
                Value::Text("en".to_string()),
                Value::BigInt(50),
            ],
            &mut truths,
        );
        if include_private {
            insert_row(
                storage,
                DOCUMENTS,
                tenant,
                13,
                Visibility::Private,
                &[
                    Value::Vector(vec![13.0, 0.0]),
                    Value::Text(unique_token(DOCUMENTS, tenant, 13)),
                    Value::BigInt(999),
                    Value::Text("en".to_string()),
                    Value::BigInt(999),
                ],
                &mut truths,
            );
        }

        // langs: id=1 Public('ja')、id=2 Private('en')。IN サブクエリの内側。
        insert_row(
            storage,
            LANGS,
            tenant,
            1,
            Visibility::Public,
            &[Value::Vector(vec![1.0, 1.0]), Value::Text("ja".to_string())],
            &mut truths,
        );
        if include_private {
            insert_row(
                storage,
                LANGS,
                tenant,
                2,
                Visibility::Private,
                &[Value::Vector(vec![2.0, 1.0]), Value::Text("en".to_string())],
                &mut truths,
            );
        }

        // other_docs: 集合演算の対向テーブル。Private 行の lang は意図的に
        // `documents` の lang 語彙（ja/en）と異なる値（de）にする。両者を
        // 同じ語彙にすると、allow_private=true で「自テナントの Private 行を
        // 含めた documents.lang 集合」と「同 other_docs.lang 集合」が完全一致
        // してしまい、EXCEPT が常に空集合になって非空虚性検査が意味を失う
        // （advisor 指摘: T0 の空虚化防止と両立させるための設計判断）。
        insert_row(
            storage,
            OTHER_DOCS,
            tenant,
            10,
            Visibility::Public,
            &[
                Value::Vector(vec![10.0, 1.0]),
                Value::Text("ja".to_string()),
            ],
            &mut truths,
        );
        if include_private {
            insert_row(
                storage,
                OTHER_DOCS,
                tenant,
                11,
                Visibility::Private,
                &[
                    Value::Vector(vec![11.0, 1.0]),
                    Value::Text("de".to_string()),
                ],
                &mut truths,
            );
        }
    }

    if flood {
        // `viewer` 以外の 1 テナント（flooder）を flood 行の保持者にする。
        // 既存呼び出し（`viewer=TENANT_A`）では `TENANTS` の並びから従来どおり
        // tenant-b になり、T4 のハードコードされた tenant-b 前提と両立する。
        let flooder = TENANTS
            .iter()
            .copied()
            .find(|&t| t != viewer)
            .expect("TENANTS has at least one entry other than viewer");

        // flooder の flood 行: viewer の author id=1 と同じ結合キーを持つ
        // documents を大量に Private で追加する（外部結合・カーディナリティが
        // 他テナントの不可視行数に依存しないことの確認用、T2・T4）。
        for i in 0..20u64 {
            insert_row(
                storage,
                DOCUMENTS,
                flooder,
                1000 + i,
                Visibility::Private,
                &[
                    Value::Vector(vec![(1000 + i) as f32, 0.0]),
                    Value::Text(unique_token(DOCUMENTS, flooder, 1000 + i)),
                    Value::BigInt(1),
                    Value::Text("ja".to_string()),
                    Value::BigInt(i as i64),
                ],
                &mut truths,
            );
        }
        // flooder だけが可視な langs 行（`EXISTS` の真偽が他テナント行に
        // 依存しないことの確認用、T4）。
        insert_row(
            storage,
            LANGS,
            flooder,
            2500,
            Visibility::Private,
            &[
                Value::Vector(vec![25.0, 0.0]),
                Value::Text(format!("zz-only-{flooder}")),
            ],
            &mut truths,
        );
    }

    truths
}

fn create_view(core: &EngineCore) {
    let mut session = SessionState::default();
    // CREATE VIEW は DDL（`sql::ddl::require_ddl_permission`）のため、
    // wire-server の handshake 相当（認証成功後の 1 回限りの許可付与）を模して
    // ここで明示的に許可する（テナント境界とは別軸の接続単位の権限）。
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx_for(TENANT_A, true),
        &mut session,
        &format!(
            "CREATE VIEW {VIEW_JA_DOCS} AS SELECT id, title, lang FROM {DOCUMENTS} WHERE lang = 'ja'"
        ),
    )
    .expect("CREATE VIEW should succeed");
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, tenant: &str, allow_private: bool, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    expect_query(
        core.execute_sql_in_session(&ctx_for(tenant, allow_private), &mut session, sql)
            .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}")),
    )
}

/// 検査対象の SQL 形。`check` は T1（独立オラクル照合）でどう混入検出するかを
/// 指定する（T0・T2・T5 は `check` を無視して全形を等しく扱う）。
#[derive(Clone, Copy)]
enum Check {
    /// 投影セル中の一意トークンが禁止集合に含まれないことを確認する（JOIN・
    /// CTE・ビュー等、TEXT 列を投影する形）。
    Token,
    /// `id` 疑似列（`ColumnMeta::Id`）が `documents` テーブルの許可 id 集合の
    /// 部分集合であることを確認する（`documents` 単独スキャン系の形）。
    /// codex 指摘（PRRT_kwDOUAKASM6mXX2L）: 複数テナントが同一 `documents.id`
    /// を再利用するフィクスチャでは、id 単独の部分集合検査は「別テナントの
    /// 同一 id 行が混入した」ケースを見逃す（混入 id が閲覧テナント自身の
    /// 許可 id 集合とたまたま一致するため）。このためこの形はすべて `title`
    /// （一意トークン）も併せて投影し、`assert_no_leak` の禁止トークン検査
    /// （`Token` と共通の経路）を主検出手段として使う。id の部分集合検査は
    /// 補助（過小返却ではない不許可 id の直接検出用）に留める。
    DocumentsId,
    /// 統計値のみを投影する形（T1 の混入検出は行わず、T2 の差分比較のみに
    /// 委ねる。値自体からテナント帰属を復元できないため）。
    StatisticOnly,
}

struct Shape {
    axis: &'static str,
    sql: &'static str,
    check: Check,
}

/// 形状マトリクス（正例）。各軸最低 1 件（JOIN は種別ごとに 1 件）を含む
/// （T0 の非空虚性ゲートが軸ごとの件数を assert する）。
fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            axis: "join_inner",
            sql: "SELECT documents.title, authors.name FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 50",
            check: Check::Token,
        },
        Shape {
            axis: "join_left",
            sql: "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 50",
            check: Check::Token,
        },
        Shape {
            axis: "join_right",
            sql: "SELECT documents.title, authors.name FROM documents RIGHT JOIN authors ON documents.author_id = authors.id LIMIT 50",
            check: Check::Token,
        },
        Shape {
            axis: "join_full",
            sql: "SELECT documents.title, authors.name FROM documents FULL JOIN authors ON documents.author_id = authors.id LIMIT 50",
            check: Check::Token,
        },
        Shape {
            axis: "subquery_in",
            sql: "SELECT id, title FROM documents WHERE lang IN (SELECT lang FROM langs LIMIT 1000) LIMIT 50",
            check: Check::DocumentsId,
        },
        Shape {
            axis: "subquery_exists",
            sql: "SELECT id, title FROM documents WHERE EXISTS (SELECT id FROM langs LIMIT 1) LIMIT 50",
            check: Check::DocumentsId,
        },
        Shape {
            axis: "cte",
            sql: "WITH ja AS (SELECT id, title, lang FROM documents WHERE lang = 'ja') SELECT id, title FROM ja LIMIT 50",
            check: Check::DocumentsId,
        },
        Shape {
            axis: "set_union",
            sql: "SELECT lang FROM documents UNION SELECT lang FROM other_docs",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "set_union_all",
            sql: "SELECT lang FROM documents UNION ALL SELECT lang FROM other_docs",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "set_intersect",
            sql: "SELECT lang FROM documents INTERSECT SELECT lang FROM other_docs",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "set_except",
            sql: "SELECT lang FROM documents EXCEPT SELECT lang FROM other_docs",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "window_row_number",
            sql: "SELECT id, title, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score DESC) FROM documents LIMIT 50",
            check: Check::DocumentsId,
        },
        Shape {
            axis: "window_count",
            sql: "SELECT id, title, COUNT(*) OVER (PARTITION BY lang) FROM documents LIMIT 50",
            check: Check::DocumentsId,
        },
        Shape {
            axis: "view",
            sql: "SELECT id, title FROM ja_docs LIMIT 50",
            check: Check::DocumentsId,
        },
        Shape {
            axis: "statistic",
            sql: "SELECT COUNT(*) FROM documents",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "statistic",
            sql: "SELECT SUM(score) FROM documents",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "statistic",
            sql: "SELECT COUNT(DISTINCT lang) FROM documents",
            check: Check::StatisticOnly,
        },
        Shape {
            axis: "statistic",
            sql: "SELECT lang, COUNT(*) FROM documents GROUP BY lang ORDER BY lang",
            check: Check::StatisticOnly,
        },
    ]
}

// ---------- T0: 受理ゲート（非空虚性） ----------

#[test]
fn shape_matrix_is_accepted_and_covers_each_axis() {
    let path = unique_db_path("rls10-t0-shapes");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let truths = seed_fixture(&storage, TENANT_A, true);
    let core = new_core(storage);
    create_view(&core);

    let shapes = shapes();
    let mut axes: HashSet<&'static str> = HashSet::new();
    for shape in &shapes {
        for &tenant in TENANTS.iter() {
            for allow_private in [false, true] {
                // 受理ゲート: 空虚化（全形が拒否されて T1/T2 が vacuous に通る
                // こと）を防ぐため、ここで成功を assert する。
                let result = run(&core, tenant, allow_private, shape.sql);
                // 非空虚性の要（advisor 指摘）: 全テナントが Public 行を持つ
                // フィクスチャ設計のため、いずれの形・テナント・モードでも
                // 空結果にはならない。RLS が過剰に絞り込んで常に空集合を
                // 返す退行（T1 の部分集合検査・T2 の差分比較は空集合同士でも
                // 素通りしてしまう）を検出する。
                assert!(
                    !result.rows.is_empty(),
                    "shape must not be vacuously empty: axis={} tenant={tenant} allow_private={allow_private}",
                    shape.axis
                );
            }
        }
        axes.insert(shape.axis);
    }

    // 非空虚性のさらなる要（advisor 指摘）: `COUNT(*)` は独立オラクルの許可
    // documents 件数と厳密に一致することを固定する（部分集合検査だけでは
    // 過小返却〔一部だけ返して残りを黙って落とす退行〕を見逃す）。
    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let result = run(
                &core,
                tenant,
                allow_private,
                "SELECT COUNT(*) FROM documents",
            );
            let got = match result.rows.first().and_then(|r| r.cells.first()) {
                Some(Cell::Integer(n)) => *n,
                other => panic!("expected Cell::Integer for COUNT(*), got {other:?}"),
            };
            let want = allowed_documents_count(&truths, tenant, allow_private);
            assert_eq!(
                got, want,
                "COUNT(*) must equal the independent oracle's allowed documents count: tenant={tenant} allow_private={allow_private}"
            );
        }
    }

    for required in [
        "join_inner",
        "join_left",
        "join_right",
        "join_full",
        "subquery_in",
        "subquery_exists",
        "cte",
        "set_union",
        "set_union_all",
        "set_intersect",
        "set_except",
        "window_row_number",
        "window_count",
        "view",
        "statistic",
    ] {
        assert!(axes.contains(required), "missing shape axis: {required}");
    }
}

// ---------- T1: 独立オラクル照合（3 テナント越境試行） ----------

fn allowed_documents_ids(truths: &[RowTruth], viewer: &str, allow_private: bool) -> HashSet<u64> {
    truths
        .iter()
        .filter(|t| t.table == DOCUMENTS && is_allowed(t, viewer, allow_private))
        .map(|t| t.id)
        .collect()
}

/// `documents` の許可済み**物理行**数（`(tenant, id)` 単位）。TABLE-12: 複数
/// テナントが同一 `id` を再利用するフィクスチャのため、`allowed_documents_ids`
/// の `HashSet<u64>` では異なるテナントの同一 `id` 行が 1 件に潰れてしまい
/// `COUNT(*)` との比較に使えない（advisor 指摘）。
fn allowed_documents_count(truths: &[RowTruth], viewer: &str, allow_private: bool) -> u64 {
    truths
        .iter()
        .filter(|t| t.table == DOCUMENTS && is_allowed(t, viewer, allow_private))
        .count() as u64
}

fn forbidden_tokens(truths: &[RowTruth], viewer: &str, allow_private: bool) -> HashSet<String> {
    truths
        .iter()
        .filter(|t| !is_allowed(t, viewer, allow_private))
        .map(|t| unique_token(t.table, t.tenant, t.id))
        .collect()
}

fn text_cells(row: &engine::sql::exec::ResultRow) -> Vec<&str> {
    row.cells
        .iter()
        .filter_map(|c| match c {
            Cell::Text(s) => Some(s.as_str()),
            _ => None,
        })
        .collect()
}

/// 結果に禁止トークン・不許可 `id` が混入していないかを検査する（独立オラクル
/// 照合。`PolicyContext::is_visible` は一切呼ばない）。
fn assert_no_leak(
    result: &QueryResult,
    check: Check,
    truths: &[RowTruth],
    viewer: &str,
    allow_private: bool,
    context: &str,
) {
    let forbidden = forbidden_tokens(truths, viewer, allow_private);
    for row in &result.rows {
        for cell in text_cells(row) {
            assert!(
                !forbidden.contains(cell),
                "disallowed row token leaked: context={context} viewer={viewer} allow_private={allow_private} token={cell:?}"
            );
        }
    }
    if matches!(check, Check::DocumentsId) && result.columns.first() == Some(&ColumnMeta::Id) {
        let allowed = allowed_documents_ids(truths, viewer, allow_private);
        for row in &result.rows {
            assert!(
                allowed.contains(&row.id),
                "disallowed documents id leaked: context={context} viewer={viewer} allow_private={allow_private} id={}",
                row.id
            );
        }
    }
}

#[test]
fn relational_paths_never_leak_rows_per_independent_oracle() {
    let path = unique_db_path("rls10-t1-oracle");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let truths = seed_fixture(&storage, TENANT_A, true);
    let core = new_core(storage);
    create_view(&core);

    for shape in shapes() {
        for &viewer in TENANTS.iter() {
            for allow_private in [false, true] {
                let result = run(&core, viewer, allow_private, shape.sql);
                // codex 指摘（PRRT_kwDOUAKASM6mXX2L）対応の回帰ガード:
                // `Check::DocumentsId` の形は id の部分集合検査だけでは
                // 同一 id を複数テナントが再利用するケースの混入を見逃す
                // ため、`title`（一意トークン）も併せて投影して禁止トークン
                // 検査（`Token` と共通の経路）で主検出させる設計にした
                // （`shapes()` のコメント参照）。この投影が将来の編集で
                // 誤って落とされ検査が id 単独へ静かに後退しないよう、
                // 実クエリ結果は必ず TEXT セルを持つことを固定する。
                if matches!(shape.check, Check::DocumentsId) {
                    assert!(
                        result.rows.iter().all(|row| !text_cells(row).is_empty()),
                        "DocumentsId shape must project a text column (title) so the token-based \
                         leak check stays the primary detector: axis={}",
                        shape.axis
                    );
                }
                assert_no_leak(
                    &result,
                    shape.check,
                    &truths,
                    viewer,
                    allow_private,
                    shape.axis,
                );
            }
        }
    }
}

// ---------- T2: baseline/flooded 完全差分比較（AC1・AC2） ----------

#[test]
fn relational_results_are_identical_with_and_without_other_tenant_private_rows() {
    // codex 指摘（PRRT_kwDOUAKASM6mXX2N）: 従来は閲覧テナントを tenant-a に
    // 固定していたため、tenant-b・tenant-c を閲覧者とする結果不変性
    // （他テナント Private 行の増減に対する自テナント行の不変性）が固定
    // されていなかった。`TENANTS` 全件を閲覧テナントとして回す。
    for &viewer in TENANTS.iter() {
        let path_baseline = unique_db_path(&format!("rls10-t2-baseline-{viewer}"));
        let _guard_baseline = CleanupGuard(path_baseline.clone());
        let storage_baseline = Storage::open(&path_baseline).expect("open storage");
        seed_fixture(&storage_baseline, viewer, false);
        let core_baseline = new_core(storage_baseline);
        create_view(&core_baseline);

        let path_flooded = unique_db_path(&format!("rls10-t2-flooded-{viewer}"));
        let _guard_flooded = CleanupGuard(path_flooded.clone());
        let storage_flooded = Storage::open(&path_flooded).expect("open storage");
        seed_fixture(&storage_flooded, viewer, true);
        let core_flooded = new_core(storage_flooded);
        create_view(&core_flooded);

        for shape in shapes() {
            for allow_private in [false, true] {
                let r1 = run(&core_baseline, viewer, allow_private, shape.sql);
                let r2 = run(&core_flooded, viewer, allow_private, shape.sql);
                assert_eq!(
                    r1, r2,
                    "result changed when other tenants' Private rows were added: axis={} viewer={viewer} allow_private={allow_private}",
                    shape.axis
                );
            }
        }
    }
}

// ---------- T4: 外部結合の NULL 補完・EXISTS の真偽が他テナント行に非依存 ----------

#[test]
fn outer_join_padding_and_exists_truth_do_not_depend_on_other_tenants() {
    let path_baseline = unique_db_path("rls10-t4-baseline");
    let _guard_baseline = CleanupGuard(path_baseline.clone());
    let storage_baseline = Storage::open(&path_baseline).expect("open storage");
    seed_fixture(&storage_baseline, TENANT_A, false);
    let core_baseline = new_core(storage_baseline);

    let path_flooded = unique_db_path("rls10-t4-flooded");
    let _guard_flooded = CleanupGuard(path_flooded.clone());
    let storage_flooded = Storage::open(&path_flooded).expect("open storage");
    seed_fixture(&storage_flooded, TENANT_A, true);
    let core_flooded = new_core(storage_flooded);

    // tenant-b が author_id=1（tenant-a の可視 author と同じ結合キー）の
    // Private documents を 20 件 flood しても、tenant-a の LEFT JOIN の
    // 未一致（NULL 補完）行数は変化しない（documents.id=13 の 1 件のみ）。
    let sql_left = "SELECT documents.title, authors.name FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 100";
    for allow_private in [false, true] {
        let r1 = run(&core_baseline, TENANT_A, allow_private, sql_left);
        let r2 = run(&core_flooded, TENANT_A, allow_private, sql_left);
        assert_eq!(
            r1, r2,
            "LEFT JOIN result changed under other-tenant flood: allow_private={allow_private}"
        );
        let null_padded = r1
            .rows
            .iter()
            .filter(|r| matches!(r.cells.get(1), Some(Cell::Null)))
            .count();
        // allow_private=true なら tenant-a 自身の不一致行（id=13）が見え 1 件、
        // allow_private=false ならその行自体が不可視で 0 件。
        let expected = if allow_private { 1 } else { 0 };
        assert_eq!(
            null_padded, expected,
            "unmatched row count must not depend on other tenants' flood: allow_private={allow_private}"
        );
    }

    // EXISTS の真偽: tenant-b だけが可視な langs 行（lang='zz-only-tenant-b'）が
    // 存在しても、tenant-a から見た EXISTS は常に偽（存在情報が漏れない）。
    let sql_exists = "SELECT id FROM documents WHERE EXISTS (SELECT id FROM langs WHERE lang = 'zz-only-tenant-b' LIMIT 1) LIMIT 5";
    for allow_private in [false, true] {
        let result = run(&core_flooded, TENANT_A, allow_private, sql_exists);
        assert!(
            result.rows.is_empty(),
            "EXISTS must be false for tenant-a regardless of tenant-b's invisible row: allow_private={allow_private}"
        );
    }
    // 陽性対照: tenant-b 自身から見れば EXISTS は真になる（判定機構が実在する
    // ことの裏付け）。
    let result = run(&core_flooded, TENANT_B, true, sql_exists);
    assert!(
        !result.rows.is_empty(),
        "positive control failed: tenant-b should see EXISTS = true for its own row"
    );
}

// ---------- T3: エラー応答の不変性（AC3） ----------

#[test]
fn error_responses_are_identical_with_and_without_other_tenant_private_rows() {
    let path_baseline = unique_db_path("rls10-t3-baseline");
    let _guard_baseline = CleanupGuard(path_baseline.clone());
    let storage_baseline = Storage::open(&path_baseline).expect("open storage");
    seed_fixture(&storage_baseline, TENANT_A, false);
    let core_baseline = new_core(storage_baseline);

    let path_flooded = unique_db_path("rls10-t3-flooded");
    let _guard_flooded = CleanupGuard(path_flooded.clone());
    let storage_flooded = Storage::open(&path_flooded).expect("open storage");
    seed_fixture(&storage_flooded, TENANT_A, true);
    let core_flooded = new_core(storage_flooded);

    let static_error_shapes = [
        // 構文解析段（データに触れる前）の拒否。他テナント行の有無で変化し
        // ようがないことを固定する。
        "SELECT id FROM nonexistent_table_xyz LIMIT 5",
        "SELECT * FROM documents NATURAL JOIN authors LIMIT 5",
        "SELECT * FROM documents CROSS JOIN authors LIMIT 5",
        "SELECT * FROM documents JOIN authors USING (id) LIMIT 5",
        // 意味検証段（`sql::subquery::validate_in_target_column` 相当。SQL-29
        // (a)）の拒否。対象列 `lang`（TEXT）と内側投影 `id`（疑似列。整数）の
        // 型不整合は列定義（スキーマ）のみで決まり、内側の行内容・行数
        // （他テナント行の有無を含む）に依存しないことを確認する（AC3 の
        // 対象を構文エラーだけに限定しない）。
        "SELECT id FROM documents WHERE lang IN (SELECT id FROM langs LIMIT 1) LIMIT 5",
    ];

    for sql in static_error_shapes {
        for allow_private in [false, true] {
            let mut session_a = SessionState::default();
            let err_baseline = core_baseline
                .execute_sql_in_session(&ctx_for(TENANT_A, allow_private), &mut session_a, sql)
                .expect_err("shape must be rejected");
            let mut session_b = SessionState::default();
            let err_flooded = core_flooded
                .execute_sql_in_session(&ctx_for(TENANT_A, allow_private), &mut session_b, sql)
                .expect_err("shape must be rejected");
            assert_eq!(
                err_baseline.wire_code(),
                err_flooded.wire_code(),
                "wire_code changed under flood: sql={sql:?} allow_private={allow_private}"
            );
            assert_eq!(
                err_baseline.client_message(),
                err_flooded.client_message(),
                "client_message changed under flood: sql={sql:?} allow_private={allow_private}"
            );
        }
    }
}

/// データ依存カナリア（AC3 (b)）: 他テナントの不可視行だけで閾値を超えさせても
/// 閲覧テナントの応答（成功・結果）が不変であること。判定機構自体が空虚でない
/// ことは陽性対照（閲覧テナント自身の行で同じ閾値を実際に超えさせる）で裏付ける。
mod data_dependent_canaries {
    use super::*;

    const CANARY_DOCS: &str = "canary_docs";
    const CANARY_LANGS: &str = "canary_langs";

    fn canary_docs_schema() -> TableSchema {
        TableSchema::new(
            CANARY_DOCS,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("score", ColumnType::BigInt, false),
            ],
        )
    }

    fn canary_langs_schema() -> TableSchema {
        TableSchema::new(
            CANARY_LANGS,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    fn insert_canary_doc(
        storage: &Storage,
        tenant: &str,
        id: u64,
        visibility: Visibility,
        lang: &str,
        score: i64,
    ) {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
            "canary-doc-{tenant}-{id}"
        ))
        .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            CANARY_DOCS,
            &ctx,
            id,
            visibility,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                Value::BigInt(score),
            ],
            &op_id,
        )
        .expect("insert canary doc");
    }

    fn insert_canary_lang(
        storage: &Storage,
        tenant: &str,
        id: u64,
        visibility: Visibility,
        lang: &str,
    ) {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
            "canary-lang-{tenant}-{id}"
        ))
        .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            CANARY_LANGS,
            &ctx,
            id,
            visibility,
            &[
                Value::Vector(vec![id as f32, 1.0]),
                Value::Text(lang.to_string()),
            ],
            &op_id,
        )
        .expect("insert canary lang");
    }

    /// `MAX_SUBQUERY_IN_VALUES`（`crate::core::MAX_SEARCH_K` = 10,000。公開済み
    /// 数値基準、spec-confidentiality 許可済み事項）超過は `54000`。他テナントの
    /// Private 行だけで内側 IN サブクエリの distinct 値数がこの上限を超えても、
    /// 閲覧テナントには影響しない（内側 SELECT は RLS を経てから値数へ数えられる
    /// ため。Issue #1165 で旧 256 葉上限から引き上げ）。
    #[test]
    fn subquery_in_value_budget_is_invariant_to_other_tenant_flood() {
        const OVERFLOW_COUNT: u64 = 5_001;
        // 2 つの IN サイトの distinct 値合計（2 × 5,001）が文全体の予算
        // 10,000 を超える形。
        let sql = format!(
            "SELECT id FROM {CANARY_DOCS} WHERE lang IN \
             (SELECT lang FROM {CANARY_LANGS} LIMIT 5001) OR lang IN \
             (SELECT lang FROM {CANARY_LANGS} LIMIT 5001) LIMIT 5"
        );

        // baseline: tenant-a は 1 件の可視 langs 行のみ持つ。
        let path_baseline = unique_db_path("rls10-canary-leaf-baseline");
        let _guard_baseline = CleanupGuard(path_baseline.clone());
        let storage_baseline = Storage::open(&path_baseline).expect("open storage");
        storage_baseline
            .create_table(&canary_docs_schema())
            .expect("create canary_docs");
        storage_baseline
            .create_table(&canary_langs_schema())
            .expect("create canary_langs");
        insert_canary_doc(&storage_baseline, TENANT_A, 1, Visibility::Public, "ja", 1);
        insert_canary_lang(&storage_baseline, TENANT_A, 1, Visibility::Public, "ja");
        let core_baseline = new_core(storage_baseline);
        let result_baseline = run(&core_baseline, TENANT_A, true, &sql);

        // flooded: tenant-b が同じテーブルへ 5,001 件の相異なる Private lang 値を
        // 追加する（tenant-a からは不可視）。件数は 1 サイトの上限 5,001 と同じ。
        let path_flooded = unique_db_path("rls10-canary-leaf-flooded");
        let _guard_flooded = CleanupGuard(path_flooded.clone());
        let storage_flooded = Storage::open(&path_flooded).expect("open storage");
        storage_flooded
            .create_table(&canary_docs_schema())
            .expect("create canary_docs");
        storage_flooded
            .create_table(&canary_langs_schema())
            .expect("create canary_langs");
        insert_canary_doc(&storage_flooded, TENANT_A, 1, Visibility::Public, "ja", 1);
        insert_canary_lang(&storage_flooded, TENANT_A, 1, Visibility::Public, "ja");
        for i in 0..OVERFLOW_COUNT {
            insert_canary_lang(
                &storage_flooded,
                TENANT_B,
                1000 + i,
                Visibility::Private,
                &format!("flood-lang-{i}"),
            );
        }
        let core_flooded = new_core(storage_flooded);
        let result_flooded = run(&core_flooded, TENANT_A, true, &sql);

        assert_eq!(
            result_baseline, result_flooded,
            "tenant-a's IN-subquery result must be invariant to tenant-b's invisible value flood"
        );

        // 陽性対照: 同じ 5,001 件を tenant-a 自身の可視行として投入すると、実際に
        // `54000`（value budget 超過）へ落ちる。
        let path_positive = unique_db_path("rls10-canary-leaf-positive");
        let _guard_positive = CleanupGuard(path_positive.clone());
        let storage_positive = Storage::open(&path_positive).expect("open storage");
        storage_positive
            .create_table(&canary_docs_schema())
            .expect("create canary_docs");
        storage_positive
            .create_table(&canary_langs_schema())
            .expect("create canary_langs");
        insert_canary_doc(&storage_positive, TENANT_A, 1, Visibility::Public, "ja", 1);
        for i in 0..OVERFLOW_COUNT {
            insert_canary_lang(
                &storage_positive,
                TENANT_A,
                2000 + i,
                Visibility::Public,
                &format!("own-lang-{i}"),
            );
        }
        let core_positive = new_core(storage_positive);
        let mut session = SessionState::default();
        let err = core_positive
            .execute_sql_in_session(&ctx_for(TENANT_A, true), &mut session, &sql)
            .expect_err("own-tenant value overflow must be rejected (positive control)");
        assert_eq!(err.wire_code(), "54000", "positive control err={err:?}");
    }

    /// `BIGINT` `SUM` は `i128` の `checked_add` で厳密に演算し、桁あふれは
    /// `22003`（`NumericOutOfRange`）。他テナントの Private 行が `i64::MAX`
    /// 近傍の値を持っていても、閲覧テナントの `SUM` 結果は不変。
    #[test]
    fn bigint_sum_overflow_is_invariant_to_other_tenant_flood() {
        let sql = format!("SELECT SUM(score) FROM {CANARY_DOCS}");

        let path_baseline = unique_db_path("rls10-canary-sum-baseline");
        let _guard_baseline = CleanupGuard(path_baseline.clone());
        let storage_baseline = Storage::open(&path_baseline).expect("open storage");
        storage_baseline
            .create_table(&canary_docs_schema())
            .expect("create canary_docs");
        insert_canary_doc(
            &storage_baseline,
            TENANT_A,
            1,
            Visibility::Public,
            "ja",
            100,
        );
        insert_canary_doc(
            &storage_baseline,
            TENANT_A,
            2,
            Visibility::Private,
            "ja",
            200,
        );
        let core_baseline = new_core(storage_baseline);
        let result_baseline = run(&core_baseline, TENANT_A, true, &sql);

        let path_flooded = unique_db_path("rls10-canary-sum-flooded");
        let _guard_flooded = CleanupGuard(path_flooded.clone());
        let storage_flooded = Storage::open(&path_flooded).expect("open storage");
        storage_flooded
            .create_table(&canary_docs_schema())
            .expect("create canary_docs");
        insert_canary_doc(&storage_flooded, TENANT_A, 1, Visibility::Public, "ja", 100);
        insert_canary_doc(
            &storage_flooded,
            TENANT_A,
            2,
            Visibility::Private,
            "ja",
            200,
        );
        insert_canary_doc(
            &storage_flooded,
            TENANT_B,
            3,
            Visibility::Private,
            "ja",
            i64::MAX,
        );
        let core_flooded = new_core(storage_flooded);
        let result_flooded = run(&core_flooded, TENANT_A, true, &sql);

        assert_eq!(
            result_baseline, result_flooded,
            "tenant-a's SUM result must be invariant to tenant-b's invisible near-overflow row"
        );

        // 陽性対照: tenant-a 自身が `i64::MAX` 近傍の値を持てば実際に `22003`
        // へ落ちる。
        let path_positive = unique_db_path("rls10-canary-sum-positive");
        let _guard_positive = CleanupGuard(path_positive.clone());
        let storage_positive = Storage::open(&path_positive).expect("open storage");
        storage_positive
            .create_table(&canary_docs_schema())
            .expect("create canary_docs");
        insert_canary_doc(
            &storage_positive,
            TENANT_A,
            1,
            Visibility::Public,
            "ja",
            i64::MAX,
        );
        insert_canary_doc(&storage_positive, TENANT_A, 2, Visibility::Public, "ja", 1);
        let core_positive = new_core(storage_positive);
        let mut session = SessionState::default();
        let err = core_positive
            .execute_sql_in_session(&ctx_for(TENANT_A, true), &mut session, &sql)
            .expect_err("own-tenant SUM overflow must be rejected (positive control)");
        assert_eq!(err.wire_code(), "22003", "positive control err={err:?}");
    }
}

// ---------- T5: キャッシュ温め順序に非依存 ----------

#[test]
fn cache_warming_order_does_not_change_results() {
    // 代表的な 3 軸（JOIN・サブクエリ・ウィンドウ）のみを検証する（全軸を
    // 温め順序ごとに検証すると実行時間が肥大化するため。他軸のキャッシュ
    // キー分離契約は `relation_snapshot`／`generation_key` が
    // per-(table, PolicyContext) で持つ設計により本質的に同型）。
    let representative_sql = [
        "SELECT documents.title, authors.name FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 50",
        "SELECT id FROM documents WHERE lang IN (SELECT lang FROM langs LIMIT 1000) LIMIT 50",
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score DESC) FROM documents LIMIT 50",
    ];

    // コールド（tenant-a のみ実行した新規 core）の結果を基準にする。
    let path_cold = unique_db_path("rls10-t5-cold");
    let _guard_cold = CleanupGuard(path_cold.clone());
    let storage_cold = Storage::open(&path_cold).expect("open storage");
    seed_fixture(&storage_cold, TENANT_A, true);
    let core_cold = new_core(storage_cold);
    let cold_results: Vec<QueryResult> = representative_sql
        .iter()
        .map(|sql| run(&core_cold, TENANT_A, true, sql))
        .collect();

    // tenant-b で温めてから tenant-a を実行（同一 core・同一 storage）。
    let path_warm_b_first = unique_db_path("rls10-t5-warm-b-first");
    let _guard_warm_b_first = CleanupGuard(path_warm_b_first.clone());
    let storage_warm_b_first = Storage::open(&path_warm_b_first).expect("open storage");
    seed_fixture(&storage_warm_b_first, TENANT_A, true);
    let core_warm_b_first = new_core(storage_warm_b_first);
    for sql in representative_sql {
        run(&core_warm_b_first, TENANT_B, true, sql);
        run(&core_warm_b_first, TENANT_B, false, sql);
    }
    for (i, sql) in representative_sql.iter().enumerate() {
        let result = run(&core_warm_b_first, TENANT_A, true, sql);
        assert_eq!(
            result, cold_results[i],
            "tenant-a result changed after warming cache with tenant-b first: sql={sql:?}"
        );
    }

    // 逆順（tenant-a → tenant-b → tenant-a 再実行）でも tenant-a の結果は不変。
    let path_warm_a_first = unique_db_path("rls10-t5-warm-a-first");
    let _guard_warm_a_first = CleanupGuard(path_warm_a_first.clone());
    let storage_warm_a_first = Storage::open(&path_warm_a_first).expect("open storage");
    seed_fixture(&storage_warm_a_first, TENANT_A, true);
    let core_warm_a_first = new_core(storage_warm_a_first);
    for (i, sql) in representative_sql.iter().enumerate() {
        let first = run(&core_warm_a_first, TENANT_A, true, sql);
        assert_eq!(first, cold_results[i], "sql={sql:?}");
        run(&core_warm_a_first, TENANT_B, true, sql);
        let second = run(&core_warm_a_first, TENANT_A, true, sql);
        assert_eq!(
            second, cold_results[i],
            "tenant-a result changed after tenant-b re-warmed the cache: sql={sql:?}"
        );
    }
}

// ---------- T6: 負の対照（判定ヘルパ自体が違反を見逃さないこと） ----------

#[test]
fn checker_negative_control_detects_fabricated_violation() {
    let path = unique_db_path("rls10-t6-negative-control");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let truths = seed_fixture(&storage, TENANT_A, true);

    // 実在する他テナント Private 行（tenant-b の documents id=11）を、本来
    // 不許可であるにもかかわらず「許可された結果」として検査ヘルパへ渡す。
    let forbidden_token = unique_token(DOCUMENTS, TENANT_B, 11);
    let fabricated_row = engine::sql::exec::ResultRow {
        id: 11,
        score: 0.0,
        cells: vec![Cell::Text(forbidden_token), Cell::Null],
    };
    let fabricated_result = QueryResult {
        columns: vec![
            ColumnMeta::Scalar {
                name: "title".to_string(),
                ty: ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "name".to_string(),
                ty: ColumnType::Text,
            },
        ],
        rows: vec![fabricated_row],
    };

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_leak(
            &fabricated_result,
            Check::Token,
            &truths,
            TENANT_A,
            false,
            "negative-control",
        );
    }))
    .is_err();

    assert!(
        caught,
        "the leak-detection assertion failed to catch a fabricated violation (checker is broken)"
    );

    // `Check::DocumentsId` 経路も同様に検査する（`documents` の不許可 id が
    // `id` 疑似列として混入したケース）。
    let fabricated_id_result = QueryResult {
        columns: vec![ColumnMeta::Id],
        rows: vec![engine::sql::exec::ResultRow {
            id: 11,
            score: 0.0,
            cells: vec![Cell::Integer(11)],
        }],
    };
    let caught_id = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_leak(
            &fabricated_id_result,
            Check::DocumentsId,
            &truths,
            TENANT_A,
            false,
            "negative-control-id",
        );
    }))
    .is_err();
    assert!(
        caught_id,
        "the DocumentsId leak-detection assertion failed to catch a fabricated violation"
    );

    // codex 指摘（PRRT_kwDOUAKASM6mXX2L）の収束確認: 旧オラクル（id 部分集合
    // 検査のみ）は、混入した他テナント行の `id` が閲覧テナント自身の許可 id
    // 集合とたまたま一致するケース（tenant-a・tenant-b の双方が id=11 の
    // Private 行を持つ TABLE-12 衝突）を見逃していた。`title` 投影を追加した
    // 現在の `Check::DocumentsId` 経路（禁止トークン検査が主検出手段）が、
    // その具体的な衝突ケースを検出できることを固定する。
    let collision_forbidden_token = unique_token(DOCUMENTS, TENANT_B, 11);
    let collision_result = QueryResult {
        columns: vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "title".to_string(),
                ty: ColumnType::Text,
            },
        ],
        rows: vec![engine::sql::exec::ResultRow {
            id: 11,
            score: 0.0,
            cells: vec![Cell::Text(collision_forbidden_token)],
        }],
    };
    let caught_collision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_leak(
            &collision_result,
            Check::DocumentsId,
            &truths,
            TENANT_A,
            true,
            "negative-control-id-collision",
        );
    }))
    .is_err();
    assert!(
        caught_collision,
        "the DocumentsId leak-detection assertion failed to catch a same-id cross-tenant \
         collision (tenant-b's id=11 row masquerading as tenant-a's own id=11 row)"
    );
}
