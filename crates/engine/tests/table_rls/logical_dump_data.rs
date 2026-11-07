//! 表データの書き出し（論理ダンプのデータ側。RECOVER-16・Issue #2161。ポインタ:
//! `docs/spec/04-behavior/recovery.md` の RECOVER-16・`wire-protocol.md` の WIRE-13、設計メモ:
//! `docs/design/logical-dump.md`）の結合テスト。
//!
//! 検査する契約:
//! 1. 多様な型・NULL・特殊文字を持つ表を書き出して空 DB へ流し込むと、値が一致する（COPY・INSERT 両形式）
//! 2. 他テナントの行は出力にもエラーにも現れない（共有の表でも自テナントの行だけ）
//! 3. 復元できない表・行（暗黙 id の表）があれば全体が失敗する
//! 4. 行はストリームで渡され、行数に上限が無い

use engine::catalog::ddl_export::{
    LogicalDumpItem, SchemaDdlError, SchemaDdlScope, TableDataFormat, TableDataPart,
};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

/// 出力を（種別、テナント、本文）の列として集める。
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Ddl(String),
    Data(TableDataPart, String, String),
}

type Infallible = std::convert::Infallible;

fn dump(
    core: &EngineCore,
    scope: &SchemaDdlScope,
    format: TableDataFormat,
) -> Result<Vec<Piece>, SchemaDdlError<Infallible>> {
    let mut out = Vec::new();
    core.export_logical_dump(scope, format, |item| {
        out.push(match item {
            LogicalDumpItem::Schema(st) => Piece::Ddl(st.sql.to_string()),
            LogicalDumpItem::Data(d) => {
                Piece::Data(d.part, d.tenant.to_string(), d.text.to_string())
            }
        });
        Ok::<(), Infallible>(())
    })?;
    Ok(out)
}

/// 出力を空 DB へ流し込む。COPY はヘッダ・データ行・終端行を 1 つの COPY として駆動する。
fn replay(core: &EngineCore, pieces: &[Piece]) {
    let mut copy: Option<(String, String, Vec<u8>)> = None;
    for p in pieces {
        match p {
            Piece::Ddl(sql) => ok(core, "tenant-a", sql),
            Piece::Data(TableDataPart::Insert, tenant, sql) => ok(core, tenant, sql),
            Piece::Data(TableDataPart::CopyHeader, tenant, sql) => {
                copy = Some((tenant.clone(), sql.clone(), Vec::new()));
            }
            Piece::Data(TableDataPart::CopyRow, _, line) => {
                let (_, _, buf) = copy.as_mut().expect("header before rows");
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            }
            Piece::Data(TableDataPart::CopyEnd, _, end) => {
                assert_eq!(end, "\\.");
                let (tenant, sql, buf) = copy.take().expect("header before end");
                let policy = ctx(&tenant);
                let plan = core
                    .begin_copy(&policy, &SessionState::default(), &sql)
                    .unwrap_or_else(|e| panic!("{sql} must begin: {e}"));
                let CopyPlan::From(mut s) = plan else {
                    panic!("expected COPY FROM");
                };
                s.feed(&buf).expect("feed");
                let batch = s.finish().expect("finish");
                core.commit_copy_in(&policy, batch).expect("commit copy");
            }
        }
    }
    assert!(copy.is_none(), "unterminated COPY");
}

fn select(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

const KITCHEN_COLS: &str = "t, v, c, i, n, b, r, d, f, dt, ts, tm, tz, iv, bits, bin, j, jb, u, \
                            nm, m, ta, ia, emb";

fn populate_kitchen(core: &EngineCore, t: &str) {
    ok(core, t, "CREATE TYPE mood AS ENUM ('sad', 'ok')");
    ok(
        core,
        t,
        "CREATE TABLE kitchen (t TEXT, v VARCHAR(40), c CHAR(5), i SMALLINT, n INTEGER, \
         b BIGINT, r REAL, d DOUBLE PRECISION, f BOOLEAN, dt DATE, ts TIMESTAMP, tm TIME, \
         tz TIMESTAMPTZ, iv INTERVAL, bits BIT(5), bin BYTEA, j JSON, jb JSONB, u UUID, \
         nm NUMERIC(10, 2), m mood, ta TEXT[], ia INTEGER[], emb VECTOR(3))",
    );
    // 特殊文字: タブ・改行・CR・バックスラッシュ・引用符・COPY の NULL 表記そのもの・空文字列・多バイト。
    let nasty = "a\tb\nc\rd\\e''f\"g\\N\u{8}\u{c}\u{b}日本語";
    ok(
        core,
        t,
        &format!(
            "INSERT INTO kitchen ({KITCHEN_COLS}) VALUES ('{nasty}', 'it''s', 'ab', -3, \
             2147483647, -9223372036854775807, 1.5e-7, 12345678.125, TRUE, '2026-03-04', \
             '2026-03-04 05:06:07.125', '23:59:59.5', '2026-03-04 05:06:07.5+09', \
             '1 year 2 mons 3 days 04:05:06.25', '10110', '\\x00ff5c0a09', \
             '{{\"a\": [1, 2, \"x\\ty\"]}}', '{{\"b\":   1}}', \
             '123e4567-e89b-12d3-a456-426614174000', 12345.60, 'ok', \
             '{{\"a b\",\"q\\\"r\",\"s\\\\t\",NULL,\"NULL\",\"\"}}', '{{1,NULL,-3}}', \
             '[1.5,-2,0.25]')"
        ),
    );
    ok(
        core,
        t,
        &format!(
            "INSERT INTO kitchen ({KITCHEN_COLS}) VALUES (NULL, NULL, NULL, NULL, NULL, NULL, \
             NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
             NULL, NULL, NULL, NULL)"
        ),
    );
    ok(
        core,
        t,
        &format!(
            "INSERT INTO kitchen ({KITCHEN_COLS}) VALUES ('', '', '', 0, 0, 0, 0, 0, FALSE, \
             '1970-01-01', '1970-01-01 00:00:00', '00:00:00', '1970-01-01 00:00:00+00', \
             '0', '00000', '\\x', '[]', '{{}}', '00000000-0000-0000-0000-000000000000', 0, \
             'sad', '{{}}', '{{}}', '[0,0,0]')"
        ),
    );
}

fn kitchen_rows(core: &EngineCore, t: &str) -> Vec<Vec<Cell>> {
    select(core, t, &format!("SELECT {KITCHEN_COLS} FROM kitchen"))
}

fn round_trip(format: TableDataFormat) {
    let tag = match format {
        TableDataFormat::Copy => "copy",
        TableDataFormat::Insert => "insert",
    };
    let p1 = unique_db_path(&format!("dump-data-src-{tag}"));
    let _g1 = CleanupGuard(p1.clone());
    let p2 = unique_db_path(&format!("dump-data-dst-{tag}"));
    let _g2 = CleanupGuard(p2.clone());
    let src = open_core(&p1);
    populate_kitchen(&src, "tenant-a");
    let pieces = dump(&src, &SchemaDdlScope::shared_and_tenant("tenant-a"), format).expect("dump");
    let parts: Vec<_> = pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Data(part, _, _) => Some(*part),
            Piece::Ddl(_) => None,
        })
        .collect();
    match format {
        TableDataFormat::Copy => {
            assert_eq!(parts.first(), Some(&TableDataPart::CopyHeader));
            assert_eq!(parts.last(), Some(&TableDataPart::CopyEnd));
            assert_eq!(
                parts
                    .iter()
                    .filter(|p| **p == TableDataPart::CopyRow)
                    .count(),
                3
            );
        }
        TableDataFormat::Insert => {
            assert_eq!(parts, vec![TableDataPart::Insert; 3]);
        }
    }
    let dst = open_core(&p2);
    replay(&dst, &pieces);
    let rows = kitchen_rows(&dst, "tenant-a");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows, kitchen_rows(&src, "tenant-a"));
    // 往復で値が実際に保たれている（比較が空振りしていない）ことの固定。
    assert_eq!(
        rows[0][0],
        Cell::Text("a\tb\nc\rd\\e'f\"g\\N\u{8}\u{c}\u{b}日本語".to_string())
    );
    assert_eq!(rows[0][1], Cell::Text("it's".to_string()));
    assert!(rows[1].iter().all(|c| *c == Cell::Null));
    assert_eq!(rows[2][0], Cell::Text(String::new()));
    // 書き出し直した出力も一致する（往復で同一）。
    let again = dump(&dst, &SchemaDdlScope::shared_and_tenant("tenant-a"), format).expect("redump");
    assert_eq!(pieces, again);
}

#[test]
fn copy_form_round_trips_every_value() {
    round_trip(TableDataFormat::Copy);
}

#[test]
fn insert_form_round_trips_every_value() {
    round_trip(TableDataFormat::Insert);
}

/// ベクトル系（HALFVEC・INT8VEC・SPARSEVEC）は COPY が受理しないため、COPY 指定でも INSERT になる。
#[test]
fn vector_family_tables_fall_back_to_insert_under_copy_format() {
    let p1 = unique_db_path("dump-data-vec-src");
    let _g1 = CleanupGuard(p1.clone());
    let p2 = unique_db_path("dump-data-vec-dst");
    let _g2 = CleanupGuard(p2.clone());
    let src = open_core(&p1);
    let t = "tenant-a";
    let cases = [
        ("fam_h", "HALFVEC(3)", "'[1.5,-2,0.25]'"),
        ("fam_q", "INT8VEC(3)", "'[1,-2,3]'"),
        ("fam_s", "SPARSEVEC(6)", "'{1:1,4:2.5}/6'"),
        ("fam_e", "VECTOR(2)", "'[0.5,1]'"),
    ];
    for (name, ty, lit) in cases {
        ok(&src, t, &format!("CREATE TABLE {name} (tag TEXT, v {ty})"));
        ok(
            &src,
            t,
            &format!("INSERT INTO {name} (tag, v) VALUES ('x', {lit})"),
        );
        ok(
            &src,
            t,
            &format!("INSERT INTO {name} (tag, v) VALUES ('y', NULL)"),
        );
    }
    let pieces = dump(
        &src,
        &SchemaDdlScope::shared_and_tenant(t),
        TableDataFormat::Copy,
    )
    .expect("dump");
    let inserts = pieces
        .iter()
        .filter(|p| matches!(p, Piece::Data(TableDataPart::Insert, _, _)))
        .count();
    let headers = pieces
        .iter()
        .filter(|p| matches!(p, Piece::Data(TableDataPart::CopyHeader, _, _)))
        .count();
    // 3 表（6 行）は INSERT、通常の VECTOR の表は COPY。
    assert_eq!((inserts, headers), (6, 1));
    let dst = open_core(&p2);
    replay(&dst, &pieces);
    for (name, _, _) in cases {
        let q = format!("SELECT tag, v FROM {name}");
        let rows = select(&dst, t, &q);
        assert_eq!(rows.len(), 2, "{name}");
        assert_eq!(rows, select(&src, t, &q), "{name}");
    }
}

/// 共有の表でも、書き出すのは指定テナントの行だけ。他テナントの行・存在は出力にもエラーにも出ない。
#[test]
fn other_tenants_rows_never_appear() {
    let p1 = unique_db_path("dump-data-tenants");
    let _g1 = CleanupGuard(p1.clone());
    let src = open_core(&p1);
    ok(&src, "tenant-a", "CREATE TABLE docs (body TEXT)");
    ok(
        &src,
        "tenant-a",
        "INSERT INTO docs (body) VALUES ('a_secret_row')",
    );
    ok(
        &src,
        "tenant-b",
        "INSERT INTO docs (body) VALUES ('b_secret_row')",
    );
    ok(&src, "tenant-b", "CREATE SCHEMA bonly");
    ok(&src, "tenant-b", "CREATE TABLE bonly.hidden (body TEXT)");
    ok(
        &src,
        "tenant-b",
        "INSERT INTO bonly.hidden (body) VALUES ('b_hidden_row')",
    );
    for format in [TableDataFormat::Copy, TableDataFormat::Insert] {
        let a = dump(&src, &SchemaDdlScope::shared_and_tenant("tenant-a"), format).expect("a");
        let text_a = format!("{a:?}");
        assert!(text_a.contains("a_secret_row"));
        for leak in ["b_secret_row", "b_hidden_row", "bonly", "hidden"] {
            assert!(!text_a.contains(leak), "{leak} leaked into tenant-a dump");
        }
        // 共有分の定義を別に出す場合の tenant_only でも、共有の表の自テナント行は出る。
        let a_only = dump(&src, &SchemaDdlScope::tenant_only("tenant-a"), format).expect("a only");
        assert!(format!("{a_only:?}").contains("a_secret_row"));
        let b = dump(&src, &SchemaDdlScope::shared_and_tenant("tenant-b"), format).expect("b");
        let text_b = format!("{b:?}");
        assert!(text_b.contains("b_secret_row") && text_b.contains("b_hidden_row"));
        assert!(!text_b.contains("a_secret_row"));
        // データを持つテナントがいない範囲（テナント無し）は何も出さない。
        let shared = dump(&src, &SchemaDdlScope::shared_only(), format).expect("shared");
        assert!(!shared.iter().any(|p| matches!(p, Piece::Data(..))));
        // 行を持たないテナント。
        let none = dump(&src, &SchemaDdlScope::tenant_only("tenant-none"), format).expect("none");
        assert!(none.is_empty());
    }
}

/// SCHEMA 配下の表の行も修飾名で往復する。
#[test]
fn schema_scoped_table_rows_round_trip() {
    let p1 = unique_db_path("dump-data-schema-src");
    let _g1 = CleanupGuard(p1.clone());
    let src = open_core(&p1);
    let t = "tenant-a";
    ok(&src, t, "CREATE SCHEMA app");
    ok(
        &src,
        t,
        "CREATE TABLE app.items (label TEXT NOT NULL, qty INTEGER)",
    );
    ok(
        &src,
        t,
        "INSERT INTO app.items (label, qty) VALUES ('x', 1)",
    );
    ok(
        &src,
        t,
        "INSERT INTO app.items (label, qty) VALUES ('y', NULL)",
    );
    let q = "SELECT label, qty FROM app.items";
    let want = select(&src, t, q);
    assert_eq!(want.len(), 2);
    let scope = SchemaDdlScope::shared_and_tenant(t);
    for (i, format) in [TableDataFormat::Copy, TableDataFormat::Insert]
        .into_iter()
        .enumerate()
    {
        let pieces = dump(&src, &scope, format).expect("dump");
        let p2 = unique_db_path(&format!("dump-data-schema-dst-{i}"));
        let _g2 = CleanupGuard(p2.clone());
        let dst = open_core(&p2);
        replay(&dst, &pieces);
        assert_eq!(select(&dst, t, q), want);
    }
}

/// IDENTITY（ALWAYS を含む）・SERIAL の値は COPY の明示値で保たれる。INSERT は ALWAYS 列へ明示値を
/// 書けないため、黙って落とさず失敗する。
#[test]
fn identity_columns_round_trip_through_copy_and_fail_closed_for_insert() {
    let p1 = unique_db_path("dump-data-ident-src");
    let _g1 = CleanupGuard(p1.clone());
    let p2 = unique_db_path("dump-data-ident-dst");
    let _g2 = CleanupGuard(p2.clone());
    let src = open_core(&p1);
    let t = "tenant-a";
    ok(
        &src,
        t,
        "CREATE TABLE items (label TEXT NOT NULL, qty INTEGER GENERATED ALWAYS AS IDENTITY, \
         seq INTEGER GENERATED BY DEFAULT AS IDENTITY, s SERIAL)",
    );
    ok(&src, t, "INSERT INTO items (label) VALUES ('x')");
    ok(&src, t, "INSERT INTO items (label) VALUES ('y')");
    let q = "SELECT label, qty, seq, s FROM items";
    let want = select(&src, t, q);
    assert_eq!(want.len(), 2);
    let scope = SchemaDdlScope::shared_and_tenant(t);
    let pieces = dump(&src, &scope, TableDataFormat::Copy).expect("copy dump");
    let dst = open_core(&p2);
    replay(&dst, &pieces);
    assert_eq!(select(&dst, t, q), want);
    match dump(&src, &scope, TableDataFormat::Insert) {
        Err(SchemaDdlError::Unsupported(u)) => {
            assert_eq!(u.kind, "table data");
            assert!(!u.reason.contains("items"));
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

/// 暗黙 id の表は復元できないため、行があってもなくても全体が失敗する。
#[test]
fn table_with_implicit_id_row_identity_fails_the_data_dump() {
    use engine::catalog::{ColumnDef, ColumnType, TableSchema};
    let p1 = unique_db_path("dump-data-legacy");
    let _g1 = CleanupGuard(p1.clone());
    let storage = Storage::open(&p1).expect("open");
    storage
        .create_table(&TableSchema::new(
            "legacy_docs",
            vec![ColumnDef::new("body", ColumnType::Text, true)],
        ))
        .expect("create legacy table");
    let err = storage
        .export_table_data(
            &SchemaDdlScope::shared_and_tenant("tenant-a"),
            TableDataFormat::Copy,
            |_| Ok::<(), Infallible>(()),
        )
        .expect_err("legacy row identity cannot be restored by SQL");
    match err {
        SchemaDdlError::Unsupported(u) => assert!(!u.reason.contains("legacy_docs")),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

/// 行はストリームで渡る。多数の行でも 1 行ずつで、行数に上限は無く、シンクの失敗で中止する。
#[test]
fn rows_stream_one_at_a_time_without_a_row_cap() {
    let p1 = unique_db_path("dump-data-stream");
    let _g1 = CleanupGuard(p1.clone());
    let src = open_core(&p1);
    let t = "tenant-a";
    ok(&src, t, "CREATE TABLE big (n INTEGER, body TEXT)");
    let total: usize = 2500;
    let mut values = Vec::new();
    for n in 0..total {
        values.push(format!("({n}, 'row {n}')"));
    }
    for chunk in values.chunks(64) {
        ok(
            &src,
            t,
            &format!("INSERT INTO big (n, body) VALUES {}", chunk.join(", ")),
        );
    }
    let mut rows = 0usize;
    let mut max_item = 0usize;
    let summary = src
        .export_table_data(
            &SchemaDdlScope::shared_and_tenant(t),
            TableDataFormat::Copy,
            |it| {
                max_item = max_item.max(it.text.len());
                if it.part == TableDataPart::CopyRow {
                    rows += 1;
                }
                Ok::<(), Infallible>(())
            },
        )
        .expect("dump");
    assert_eq!(rows, total);
    assert_eq!(summary.rows, total as u64);
    assert_eq!(summary.tables, 1);
    // 渡す単位は 1 行（COPY ヘッダ）で、表全体を 1 つにまとめない。
    assert!(max_item < 200);
    let err = src
        .export_table_data(
            &SchemaDdlScope::shared_and_tenant(t),
            TableDataFormat::Copy,
            |_| Err::<(), &str>("disk full"),
        )
        .expect_err("sink error");
    assert!(matches!(err, SchemaDdlError::Sink("disk full")));
}

/// 接頭辞が重なるテナント（`tenant-a` と `tenant-ab`）の行は、互いのダンプに現れない。
#[test]
fn tenants_sharing_a_name_prefix_stay_separate() {
    let p1 = unique_db_path("dump-data-prefix");
    let _g1 = CleanupGuard(p1.clone());
    let src = open_core(&p1);
    ok(&src, "tenant-a", "CREATE TABLE docs (body TEXT)");
    ok(
        &src,
        "tenant-a",
        "INSERT INTO docs (body) VALUES ('row_of_a')",
    );
    ok(
        &src,
        "tenant-ab",
        "INSERT INTO docs (body) VALUES ('row_of_ab')",
    );
    ok(
        &src,
        "tenant-",
        "INSERT INTO docs (body) VALUES ('row_of_stem')",
    );
    for format in [TableDataFormat::Copy, TableDataFormat::Insert] {
        let a = dump(&src, &SchemaDdlScope::tenant_only("tenant-a"), format).expect("dump");
        let rows: Vec<_> = a
            .iter()
            .filter_map(|p| match p {
                Piece::Data(TableDataPart::CopyRow | TableDataPart::Insert, tenant, t) => {
                    Some((tenant.clone(), t.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "tenant-a");
        assert!(rows[0].1.contains("row_of_a") && !rows[0].1.contains("row_of_ab"));
    }
}

/// NUL を含む値は COPY・INSERT のどちらでも運べないため、ダンプは黙って成功せず失敗する。
#[test]
fn nul_in_a_value_never_dumps_silently() {
    let p1 = unique_db_path("dump-data-nul");
    let _g1 = CleanupGuard(p1.clone());
    let src = open_core(&p1);
    ok(&src, "tenant-a", "CREATE TABLE n (body TEXT)");
    if run(&src, "tenant-a", "INSERT INTO n (body) VALUES ('a\u{0}b')").is_err() {
        // 入口で拒否される（そもそも保存されない）。保存できないので書き出しの対象にもならない。
        let pieces = dump(
            &src,
            &SchemaDdlScope::tenant_only("tenant-a"),
            TableDataFormat::Copy,
        )
        .expect("dump");
        assert!(pieces.is_empty());
        return;
    }
    for format in [TableDataFormat::Copy, TableDataFormat::Insert] {
        match dump(&src, &SchemaDdlScope::tenant_only("tenant-a"), format) {
            Err(SchemaDdlError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}

/// 2 次元配列（NULL 要素あり）・ベクトル系の負値と極値・整数と浮動小数の極値も往復する。
#[test]
fn extreme_values_round_trip_in_both_formats() {
    for (i, format) in [TableDataFormat::Copy, TableDataFormat::Insert]
        .into_iter()
        .enumerate()
    {
        let p1 = unique_db_path(&format!("dump-data-ext-src-{i}"));
        let _g1 = CleanupGuard(p1.clone());
        let p2 = unique_db_path(&format!("dump-data-ext-dst-{i}"));
        let _g2 = CleanupGuard(p2.clone());
        let src = open_core(&p1);
        let t = "tenant-a";
        let stmts = [
            "CREATE TABLE ext_int (a SMALLINT, b INTEGER, c BIGINT, r REAL, d DOUBLE PRECISION)",
            "INSERT INTO ext_int (a, b, c, r, d) VALUES (-32768, -2147483648, -9223372036854775808, \
             -3.4028235e38, -1.7976931348623157e308)",
            "INSERT INTO ext_int (a, b, c, r, d) VALUES (32767, 2147483647, 9223372036854775807, \
             3.4028235e38, 1.7976931348623157e308)",
            "INSERT INTO ext_int (a, b, c, r, d) VALUES (0, 0, 0, 1.17549435e-38, 2.2250738585072014e-308)",
            "INSERT INTO ext_int (a, b, c, r, d) VALUES (1, 1, 1, -0.1, 0.1)",
            "CREATE TABLE ext_arr (m INTEGER[][], tt TEXT[][])",
            "INSERT INTO ext_arr (m, tt) VALUES ('{{1,NULL},{3,-4}}', '{{\"a b\",NULL},{\"\",\"q\\\"r\"}}')",
            "CREATE TABLE ext_h (v HALFVEC(4))",
            "INSERT INTO ext_h (v) VALUES ('[65504,-65504,-0.5,0.00006103515625]')",
            "CREATE TABLE ext_q (v INT8VEC(3))",
            "INSERT INTO ext_q (v) VALUES ('[-128,127,0]')",
            "CREATE TABLE ext_s (v SPARSEVEC(6))",
            "INSERT INTO ext_s (v) VALUES ('{1:-1.5,6:3.4028235e38}/6')",
            "CREATE TABLE ext_v (v VECTOR(3))",
            "INSERT INTO ext_v (v) VALUES ('[-1.5,3.4028235e38,-0.000001]')",
        ];
        for s in stmts {
            ok(&src, t, s);
        }
        let pieces = dump(&src, &SchemaDdlScope::shared_and_tenant(t), format).expect("dump");
        let dst = open_core(&p2);
        replay(&dst, &pieces);
        for q in [
            "SELECT a, b, c, r, d FROM ext_int",
            "SELECT m, tt FROM ext_arr",
            "SELECT v FROM ext_h",
            "SELECT v FROM ext_q",
            "SELECT v FROM ext_s",
            "SELECT v FROM ext_v",
        ] {
            let want = select(&src, t, q);
            assert!(!want.is_empty(), "{q}");
            assert_eq!(select(&dst, t, q), want, "{q}");
        }
    }
}
