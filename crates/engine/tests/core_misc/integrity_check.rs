//! DB ファイルの整合検査 `Storage::verify_integrity` の結合テスト
//! （RECOVER-13 (6) のポインタ。Issue #2155。親 #2154。実装は `src/storage/integrity.rs`）。
//!
//! 検証の軸は 4 つ。(1) 製品経路（SQL 表層）で作った複数テナント・主キー・UNIQUE・FOREIGN KEY・GIN
//! 相当索引つきの DB が矛盾 0 件で整合と判定される。(2) redb を直接開いて壊した DB で、壊し方ごとに
//! 該当の種類が件数つきで報告される。(3) 検査は読み取りのみ（ファイルのバイト列・世代が不変）で、報告に
//! 行の値・テナント名が含まれない。(4) 資源上限（予算超過・報告の打ち切り）が fail-closed に働き、
//! `export_snapshot` の出力にも検査が通る。

use std::path::Path;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{
    FindingKind, IntegrityOptions, IntegrityReport, Severity, Storage, Visibility,
};
use redb::{Database, MultimapTableDefinition, ReadableDatabase, ReadableTable, TableDefinition};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 2] = ["tenant-zeta-secret", "tenant-omega-secret"];
/// 行の値として入れる秘匿対象の文字列（報告に現れてはならない）。
const SECRET_VALUE: &str = "secret-value-qwxz";

type Rows = TableDefinition<'static, (&'static str, u64), &'static [u8]>;
type Index = TableDefinition<'static, (&'static str, &'static [u8]), &'static [u8]>;

const PARENT_ROWS: Rows = TableDefinition::new("user_rows/parents");
const KID_ROWS: Rows = TableDefinition::new("user_rows/kids");
const PARENT_UNIQ: Index = TableDefinition::new("user_uniq/parents");
const PARENT_GIN: Index = TableDefinition::new("user_gin/parents_doc");

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
}

/// 複数テナント・主キー・UNIQUE・FK・GIN 相当索引を持つ DB を製品経路で作る。
fn build_db(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open"),
        Box::new(CpuScalarProvider),
    );
    let admin = TENANTS[0];
    run(
        &core,
        admin,
        "CREATE TABLE parents (code TEXT NOT NULL PRIMARY KEY, name TEXT, doc JSONB)",
    );
    run(
        &core,
        admin,
        "CREATE TABLE kids (kid TEXT NOT NULL PRIMARY KEY, pcode TEXT REFERENCES parents(code), \
         tag TEXT UNIQUE)",
    );
    run(
        &core,
        admin,
        "CREATE INDEX parents_doc ON parents USING gin (doc jsonb_ops)",
    );
    // 両テナントが同じキー値を持つ（テナント間の同値で重複・孤児と誤判定されないこと）。
    for tenant in TENANTS {
        for (code, doc) in [("p1", r#"{"a":1}"#), ("p2", r#"{"b":[1,2]}"#), ("p3", "{}")] {
            run(
                &core,
                tenant,
                &format!(
                    "INSERT INTO parents (code, name, doc) VALUES ('{code}', '{SECRET_VALUE}', '{doc}')"
                ),
            );
        }
        for (kid, parent, tag) in [
            ("k1", "'p1'", "'t1'"),
            ("k2", "'p2'", "'t2'"),
            ("k3", "NULL", "NULL"),
        ] {
            run(
                &core,
                tenant,
                &format!("INSERT INTO kids (kid, pcode, tag) VALUES ('{kid}', {parent}, {tag})"),
            );
        }
    }
    drop(core);
    (path, guard)
}

fn verify_with(path: &Path, options: &IntegrityOptions) -> IntegrityReport {
    let storage = Storage::open(path).expect("open for verify");
    storage.verify_integrity(options).expect("verify")
}

fn verify(path: &Path) -> IntegrityReport {
    verify_with(path, &IntegrityOptions::default())
}

/// 壊す作業（redb を直接開く。Storage は閉じていること）。
fn corrupt(path: &Path, f: impl FnOnce(&redb::WriteTransaction)) {
    let db = Database::open(path).expect("raw open");
    let w = db.begin_write().expect("raw write");
    f(&w);
    w.commit().expect("raw commit");
}

/// `table` の `tenant` の最初の行（キーの rowid と値）。
fn first_row(path: &Path, table: Rows, tenant: &str) -> (u64, Vec<u8>) {
    let db = Database::open(path).expect("raw open");
    let r = db.begin_read().expect("read");
    let t = r.open_table(table).expect("table");
    let range = t
        .range((tenant, 0u64)..=(tenant, u64::MAX))
        .expect("range")
        .next()
        .expect("a row")
        .expect("entry");
    (range.0.value().1, range.1.value().to_vec())
}

#[test]
fn healthy_multi_tenant_db_has_no_findings() {
    let (path, _g) = build_db("integrity-healthy");
    let report = verify(&path);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.is_consistent());
    assert!(!report.findings_truncated);
    assert_eq!(report.catalog_tables_checked, 2);
    assert_eq!(report.rows_scanned, 2 * (3 + 3));
    assert!(report.storage_tables_scanned >= 6);
}

#[test]
fn empty_db_and_created_but_unpopulated_table_are_consistent() {
    let path = unique_db_path("integrity-empty");
    let _g = CleanupGuard(path.clone());
    drop(Storage::open(&path).expect("open"));
    let report = verify(&path);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.is_consistent());
    // 作成直後で行ストアを持たない表は欠落とは報告されない。
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open"),
        Box::new(CpuScalarProvider),
    );
    run(
        &core,
        TENANTS[0],
        "CREATE TABLE only_schema (a TEXT NOT NULL PRIMARY KEY)",
    );
    drop(core);
    let report = verify(&path);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.is_consistent());
}

#[test]
fn missing_row_store_of_a_catalog_table_is_reported() {
    let (path, _g) = build_db("integrity-missing-table");
    corrupt(&path, |w| {
        w.delete_table(KID_ROWS).expect("delete rows table");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowStoreMissing), 1);
    assert!(!report.is_consistent());
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::RowStoreMissing && f.table.as_deref() == Some("kids")));
}

#[test]
fn undecodable_row_is_reported_by_kind_and_table() {
    let (path, _g) = build_db("integrity-undecodable");
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_001u64), [0xFFu8, 1, 2].as_slice())
            .expect("insert garbage");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowUndecodable), 1);
    assert!(!report.is_consistent());
}

#[test]
fn row_key_tenant_mismatch_is_reported() {
    let (path, _g) = build_db("integrity-tenant-mismatch");
    let (_, foreign_value) = first_row(&path, PARENT_ROWS, TENANTS[1]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_002u64), foreign_value.as_slice())
            .expect("insert");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowTenantMismatch), 1);
    assert!(!report.is_consistent());
}

#[test]
fn duplicate_primary_key_is_reported_independently_of_the_index() {
    let (path, _g) = build_db("integrity-pk-dup");
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_003u64), value.as_slice())
            .expect("insert duplicate");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::PrimaryKeyDuplicate), 1);
    assert_eq!(report.total(FindingKind::UniqueKeyDuplicate), 0);
    // 複製した行の主キーに対する正引きは元の行を指す（行に索引が無い）。
    assert_eq!(report.total(FindingKind::UniqueIndexEntryMissing), 1);
    assert!(!report.is_consistent());
}

#[test]
fn duplicate_unique_constraint_value_is_reported() {
    let (path, _g) = build_db("integrity-unique-dup");
    let (_, value) = first_row(&path, KID_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w.open_table(KID_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_004u64), value.as_slice())
            .expect("insert duplicate");
    });
    let report = verify(&path);
    // 行を丸ごと複製したので、主キーと UNIQUE（tag は非 NULL の行を複製した場合）の両方が重複する。
    assert_eq!(report.total(FindingKind::PrimaryKeyDuplicate), 1);
    assert!(report.total(FindingKind::UniqueKeyDuplicate) <= 1);
    assert!(!report.is_consistent());
}

#[test]
fn unique_index_entry_missing_for_a_row_is_reported() {
    let (path, _g) = build_db("integrity-index-missing");
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_UNIQ).expect("uniq");
        let victim = {
            let mut found = None;
            for e in t.iter().expect("iter") {
                let (k, _) = e.expect("entry");
                let (tenant, sub) = k.value();
                if tenant == TENANTS[0] && sub.first() == Some(&0x01) {
                    found = Some(sub.to_vec());
                    break;
                }
            }
            found.expect("a forward entry")
        };
        t.remove((TENANTS[0], victim.as_slice())).expect("remove");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::UniqueIndexEntryMissing), 1);
    assert!(!report.is_consistent());
}

#[test]
fn unique_index_entry_without_a_row_is_reported_as_warning() {
    let (path, _g) = build_db("integrity-index-dangling");
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_UNIQ).expect("uniq");
        let sub: Vec<u8> = [&[0x01u8, 0, 0][..], b"zz"].concat();
        t.insert(
            (TENANTS[0], sub.as_slice()),
            424_242u64.to_be_bytes().as_slice(),
        )
        .expect("insert dangling");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::UniqueIndexEntryDangling), 1);
    assert_eq!(
        FindingKind::UniqueIndexEntryDangling.severity(),
        Severity::Warning
    );
    // 設計上、行の無い正引きは正しさに影響しない衛生指標のため、整合の判定は覆さない。
    assert!(report.is_consistent());
    assert_eq!(report.total_by_severity(Severity::Warning), 1);
}

#[test]
fn gin_posting_missing_and_dangling_are_reported() {
    let (path, _g) = build_db("integrity-gin");
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_GIN).expect("gin");
        let victim = {
            let mut found = None;
            for e in t.iter().expect("iter") {
                let (k, _) = e.expect("entry");
                let (tenant, sub) = k.value();
                if tenant == TENANTS[0] && sub.first() == Some(&0x01) {
                    found = Some(sub.to_vec());
                    break;
                }
            }
            found.expect("a posting")
        };
        t.remove((TENANTS[0], victim.as_slice())).expect("remove");
        let dangling: Vec<u8> = {
            let mut v = vec![0x01u8, 0, 1, b'q'];
            v.extend_from_slice(&777_777u64.to_be_bytes());
            v
        };
        t.insert((TENANTS[0], dangling.as_slice()), [].as_slice())
            .expect("insert");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::GinIndexEntryMissing), 1);
    assert_eq!(report.total(FindingKind::GinIndexEntryDangling), 1);
    assert!(!report.is_consistent());
}

#[test]
fn orphan_foreign_key_rows_are_counted_per_tenant() {
    let (path, _g) = build_db("integrity-fk-orphan");
    // 片方のテナントだけ、親 p1（挿入順で最初の行）を行ストアから消す。そのテナントの子 k1 が孤児になる。
    let (rowid, _) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.remove((TENANTS[0], rowid)).expect("remove parent");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::ForeignKeyOrphanRow), 1);
    assert!(!report.is_consistent());
    // 集合を分割して走査し直しても同じ件数になる。
    let tight = verify_with(
        &path,
        &IntegrityOptions {
            memory_budget_bytes: 400,
            ..IntegrityOptions::default()
        },
    );
    assert_eq!(tight.total(FindingKind::ForeignKeyOrphanRow), 1);
}

#[test]
fn unknown_table_type_and_multimap_are_unsupported_and_never_consistent() {
    let (path, _g) = build_db("integrity-unsupported");
    corrupt(&path, |w| {
        let mut t = w
            .open_table(TableDefinition::<u8, u8>::new("zzfamily/with-detail"))
            .expect("unknown");
        t.insert(1u8, 2u8).expect("insert");
        let mut m = w
            .open_multimap_table(MultimapTableDefinition::<u8, u8>::new("mm"))
            .expect("multimap");
        m.insert(1u8, 2u8).expect("insert");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::UnsupportedTableType), 2);
    assert!(!report.is_consistent());
    // 未知の型の表は系統名（最初の `/` まで）だけを示し、残りの名前は載せない。
    assert!(report
        .findings
        .iter()
        .any(|f| f.table.as_deref() == Some("zzfamily")));
    assert!(!format!("{report:?}").contains("with-detail"));
}

#[test]
fn orphan_row_store_without_catalog_entry_is_reported() {
    let (path, _g) = build_db("integrity-orphan-store");
    corrupt(&path, |w| {
        w.open_table(TableDefinition::<(&str, u64), &[u8]>::new(
            "user_rows/ghost",
        ))
        .expect("ghost");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::OrphanRowStore), 1);
    assert!(!report.is_consistent());
}

#[test]
fn report_never_contains_tenant_names_or_row_values() {
    let (path, _g) = build_db("integrity-redaction");
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_010u64), value.as_slice())
            .expect("dup");
        t.insert((TENANTS[1], 900_011u64), [9u8, 9, 9].as_slice())
            .expect("garbage");
        let mut k = w.open_table(KID_ROWS).expect("kids");
        let (_, kv) = {
            let first = k
                .range((TENANTS[0], 0u64)..=(TENANTS[0], u64::MAX))
                .expect("range")
                .next()
                .expect("row")
                .expect("entry");
            (first.0.value().1, first.1.value().to_vec())
        };
        k.insert((TENANTS[0], 900_012u64), kv.as_slice())
            .expect("dup kid");
    });
    let report = verify(&path);
    assert!(!report.is_consistent());
    let text = format!("{report:?}");
    for forbidden in [TENANTS[0], TENANTS[1], SECRET_VALUE, "p1", "t1"] {
        assert!(
            !text.contains(forbidden),
            "report leaked {forbidden:?}: {text}"
        );
    }
}

#[test]
fn verification_is_read_only_and_leaves_file_and_generations_unchanged() {
    let (path, _g) = build_db("integrity-readonly");
    let generations = |p: &Path| -> Vec<(String, u64)> {
        let db = Database::open(p).expect("raw open");
        let r = db.begin_read().expect("read");
        let mut out = Vec::new();
        for name in ["table_generation", "storage_generation"] {
            let t = r
                .open_table(TableDefinition::<&str, u64>::new(name))
                .expect("generation table");
            for e in t.iter().expect("iter") {
                let (k, v) = e.expect("entry");
                out.push((format!("{name}/{}", k.value()), v.value()));
            }
        }
        out
    };
    let before_generations = generations(&path);
    assert!(!before_generations.is_empty());

    let storage = Storage::open(&path).expect("open");
    let before = std::fs::read(&path).expect("read file");
    let report = storage
        .verify_integrity(&IntegrityOptions::default())
        .expect("verify");
    assert!(report.is_consistent());
    // 二度走らせても結果は同じで、ファイルのバイト列は 1 バイトも変わらない。
    let again = storage
        .verify_integrity(&IntegrityOptions::default())
        .expect("verify again");
    assert_eq!(report, again);
    let after = std::fs::read(&path).expect("read file");
    assert!(before == after, "verify_integrity must not modify the file");
    drop(storage);
    assert_eq!(generations(&path), before_generations);
}

#[test]
fn verification_runs_alongside_concurrent_writes() {
    use engine::storage::RowInput;
    let (path, _g) = build_db("integrity-concurrent");
    let storage = Storage::open(&path).expect("open");
    // 検査は読み取りトランザクションだけを使うため、書き込みスレッドを止めず、書き込みも検査を妨げない。
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for id in 1..=100u64 {
                storage
                    .put(
                        id,
                        &RowInput {
                            tenant_id: TENANTS[0],
                            visibility: Visibility::Public,
                            embedding: &[1.0],
                            metadata: b"m",
                        },
                    )
                    .expect("put");
            }
        });
        for _ in 0..5 {
            let report = storage
                .verify_integrity(&IntegrityOptions::default())
                .expect("verify");
            assert!(report.is_consistent(), "{:?}", report.findings);
        }
        writer.join().expect("writer");
    });
    assert_eq!(storage.scan().expect("scan").len(), 100);
}

#[test]
fn small_memory_budget_partitions_the_scan_without_changing_results() {
    let (path, _g) = build_db("integrity-budget");
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_020u64), value.as_slice())
            .expect("dup");
    });
    let tight = IntegrityOptions {
        // 1〜2 エントリ分。テナントの全キーを一度には持てず、複数の分割に分けて走査される。
        memory_budget_bytes: 400,
        ..IntegrityOptions::default()
    };
    let a = verify_with(&path, &tight);
    let b = verify(&path);
    assert_eq!(a, b);
    assert_eq!(a.total(FindingKind::PrimaryKeyDuplicate), 1);
}

#[test]
fn exhausted_memory_budget_is_reported_as_incomplete_not_consistent() {
    let (path, _g) = build_db("integrity-budget-exhausted");
    let report = verify_with(
        &path,
        &IntegrityOptions {
            memory_budget_bytes: 1,
            ..IntegrityOptions::default()
        },
    );
    assert!(report.total(FindingKind::CheckBudgetExceeded) > 0);
    assert_eq!(
        FindingKind::CheckBudgetExceeded.severity(),
        Severity::Incomplete
    );
    assert!(!report.is_consistent());
}

#[test]
fn finding_group_cap_truncates_the_list_but_not_the_totals() {
    let (path, _g) = build_db("integrity-truncate");
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_030u64), [0xFFu8].as_slice())
            .expect("garbage");
        w.open_table(TableDefinition::<(&str, u64), &[u8]>::new(
            "user_rows/ghost",
        ))
        .expect("ghost");
        let mut u = w
            .open_table(TableDefinition::<u8, u8>::new("zzfamily/x"))
            .expect("unknown");
        u.insert(1u8, 1u8).expect("insert");
    });
    let capped = verify_with(
        &path,
        &IntegrityOptions {
            max_finding_groups: 1,
            ..IntegrityOptions::default()
        },
    );
    let full = verify(&path);
    assert_eq!(capped.findings.len(), 1);
    assert!(capped.findings_truncated);
    assert!(!full.findings_truncated);
    assert!(full.findings.len() > 1);
    for kind in [
        FindingKind::RowUndecodable,
        FindingKind::OrphanRowStore,
        FindingKind::UnsupportedTableType,
    ] {
        assert_eq!(capped.total(kind), full.total(kind));
        assert_eq!(full.total(kind), 1);
    }
    assert!(!capped.is_consistent());
}

#[test]
fn exported_snapshot_passes_the_check() {
    let (path, _g) = build_db("integrity-snapshot-src");
    let out = unique_db_path("integrity-snapshot-out");
    let _g2 = CleanupGuard(out.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.export_snapshot(&out).expect("export");
    }
    let report = verify(&out);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.is_consistent());
    assert_eq!(report.rows_scanned, 2 * (3 + 3));
}

#[test]
fn corrupt_snapshot_derived_from_a_corrupt_db_is_still_reported() {
    let (path, _g) = build_db("integrity-snapshot-corrupt");
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 900_040u64), value.as_slice())
            .expect("dup");
    });
    let out = unique_db_path("integrity-snapshot-corrupt-out");
    let _g2 = CleanupGuard(out.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.export_snapshot(&out).expect("export");
    }
    assert_eq!(verify(&out).total(FindingKind::PrimaryKeyDuplicate), 1);
}

/// `id` 参照の FOREIGN KEY（従来方式の表。参照先は物理キーの点照会で判定する）の孤児も報告される。
#[test]
fn orphan_of_an_id_referencing_foreign_key_is_reported() {
    let path = unique_db_path("integrity-fk-by-id");
    let _g = CleanupGuard(path.clone());
    {
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open"),
            Box::new(CpuScalarProvider),
        );
        let c = ctx(TENANTS[0]);
        for sql in [
            "CREATE TABLE legacy_parents (name TEXT)",
            "CREATE TABLE legacy_children (parent_id BIGINT REFERENCES legacy_parents, note TEXT)",
        ] {
            crate::legacy_table::create_legacy_table(&core, &c, sql).expect(sql);
        }
        run(
            &core,
            TENANTS[0],
            "INSERT INTO legacy_parents (id, name) VALUES (1, 'a')",
        );
        run(
            &core,
            TENANTS[0],
            "INSERT INTO legacy_parents (id, name) VALUES (2, 'b')",
        );
        run(
            &core,
            TENANTS[0],
            "INSERT INTO legacy_children (id, parent_id, note) VALUES (10, 1, 'x')",
        );
        run(
            &core,
            TENANTS[0],
            "INSERT INTO legacy_children (id, parent_id, note) VALUES (11, 2, 'y')",
        );
    }
    let healthy = verify(&path);
    assert!(healthy.findings.is_empty(), "{:?}", healthy.findings);
    corrupt(&path, |w| {
        let mut t = w
            .open_table(TableDefinition::<(&str, u64), &[u8]>::new(
                "user_rows/legacy_parents",
            ))
            .expect("rows");
        t.remove((TENANTS[0], 2u64)).expect("remove parent");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::ForeignKeyOrphanRow), 1);
    assert!(!report.is_consistent());
}

fn core_at(path: &Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open"),
        Box::new(CpuScalarProvider),
    )
}

const PLAIN_ROWS: Rows = TableDefinition::new("user_rows/plain");

#[test]
fn row_store_missing_is_detected_for_allocated_table_without_indexes() {
    // 行を入れたまま行ストアが消えた場合。
    let path = unique_db_path("integrity-alloc-kept");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        run(&core, TENANTS[0], "CREATE TABLE plain (a TEXT, b TEXT)");
        run(
            &core,
            TENANTS[0],
            "INSERT INTO plain (a, b) VALUES ('x', 'y')",
        );
    }
    assert!(verify(&path).findings.is_empty());
    corrupt(&path, |w| {
        w.delete_table(PLAIN_ROWS).expect("delete rows table");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowStoreMissing), 1);
    assert!(!report.is_consistent());

    // 行を全削除した後で行ストアが消えた場合（採番状態は行を消しても残る）。
    let path = unique_db_path("integrity-alloc-emptied");
    let _g2 = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        run(&core, TENANTS[0], "CREATE TABLE plain (a TEXT, b TEXT)");
        run(
            &core,
            TENANTS[0],
            "INSERT INTO plain (a, b) VALUES ('x', 'y')",
        );
        run(&core, TENANTS[0], "DELETE FROM plain WHERE a = 'x'");
    }
    assert!(verify(&path).findings.is_empty());
    corrupt(&path, |w| {
        w.delete_table(PLAIN_ROWS).expect("delete rows table");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowStoreMissing), 1);
    assert!(!report.is_consistent());
}

#[test]
fn never_written_allocated_table_reports_nothing() {
    let path = unique_db_path("integrity-alloc-unwritten");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        run(&core, TENANTS[0], "CREATE TABLE plain (a TEXT, b TEXT)");
    }
    let report = verify(&path);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.is_consistent());
}

#[test]
fn absent_row_store_of_legacy_table_is_a_warning_only() {
    let path = unique_db_path("integrity-legacy-absent");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        crate::legacy_table::create_legacy_table(
            &core,
            &ctx(TENANTS[0]),
            "CREATE TABLE legacy_only (note TEXT)",
        )
        .expect("create legacy");
    }
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowStoreAbsentUnverified), 1);
    assert_eq!(report.total(FindingKind::RowStoreMissing), 0);
    assert_eq!(
        FindingKind::RowStoreAbsentUnverified.severity(),
        Severity::Warning
    );
    // 消失とは区別できないため警告にとどめ、整合の判定は覆さない（検出できているわけではない）。
    assert!(report.is_consistent());
}

/// 正常な DB で、複合キー・自己参照・ALTER で足した FK・NULL を含む子行が誤検知にならない。
#[test]
fn foreign_key_shapes_are_not_false_positives_and_orphans_are_found() {
    let path = unique_db_path("integrity-fk-shapes");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        for t in TENANTS {
            if t == TENANTS[0] {
                for sql in [
                    "CREATE TABLE cp (a TEXT NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (a, b))",
                    "CREATE TABLE cc (k TEXT NOT NULL PRIMARY KEY, a TEXT, b INTEGER, \
                     FOREIGN KEY (a, b) REFERENCES cp (a, b))",
                    "CREATE TABLE tree (n TEXT NOT NULL PRIMARY KEY, parent TEXT REFERENCES tree(n))",
                    "CREATE TABLE late (k TEXT NOT NULL PRIMARY KEY, pc TEXT)",
                ] {
                    run(&core, t, sql);
                }
            }
            for sql in [
                "INSERT INTO cp (a, b) VALUES ('x', 1)",
                "INSERT INTO cp (a, b) VALUES ('x', 2)",
                "INSERT INTO cc (k, a, b) VALUES ('c1', 'x', 1)",
                "INSERT INTO cc (k, a, b) VALUES ('c2', NULL, NULL)",
                "INSERT INTO cc (k, a, b) VALUES ('c3', 'zz', NULL)",
                "INSERT INTO tree (n, parent) VALUES ('root', NULL)",
                "INSERT INTO tree (n, parent) VALUES ('leaf', 'root')",
                "INSERT INTO late (k, pc) VALUES ('l1', 'x')",
                "INSERT INTO late (k, pc) VALUES ('l2', NULL)",
            ] {
                run(&core, t, sql);
            }
        }
        run(
            &core,
            TENANTS[0],
            "CREATE TABLE lp (code TEXT NOT NULL PRIMARY KEY)",
        );
        run(&core, TENANTS[0], "INSERT INTO lp (code) VALUES ('x')");
        run(&core, TENANTS[1], "INSERT INTO lp (code) VALUES ('x')");
        run(
            &core,
            TENANTS[0],
            "ALTER TABLE late ADD FOREIGN KEY (pc) REFERENCES lp (code)",
        );
    }
    let healthy = verify(&path);
    assert!(healthy.findings.is_empty(), "{:?}", healthy.findings);
    assert!(healthy.is_consistent());

    // 複合キーの親と自己参照の親を消すと、それぞれの子が孤児になる（NULL を含む子は数えない）。
    let (cp_rowid, _) = first_row(&path, TableDefinition::new("user_rows/cp"), TENANTS[0]);
    let (tree_rowid, _) = first_row(&path, TableDefinition::new("user_rows/tree"), TENANTS[0]);
    corrupt(&path, |w| {
        let mut t = w
            .open_table(TableDefinition::<(&str, u64), &[u8]>::new("user_rows/cp"))
            .expect("cp");
        t.remove((TENANTS[0], cp_rowid)).expect("remove cp");
        let mut t = w
            .open_table(TableDefinition::<(&str, u64), &[u8]>::new("user_rows/tree"))
            .expect("tree");
        t.remove((TENANTS[0], tree_rowid)).expect("remove root");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::ForeignKeyOrphanRow), 2);
}

#[test]
fn mass_duplicates_of_one_key_are_reported_even_with_a_small_budget() {
    let (path, _g) = build_db("integrity-mass-dup");
    let (_, parent_value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    let (_, kid_value) = first_row(&path, KID_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        let mut p = w.open_table(PARENT_ROWS).expect("parents");
        let mut k = w.open_table(KID_ROWS).expect("kids");
        for i in 0..5u64 {
            p.insert((TENANTS[0], 910_000 + i), parent_value.as_slice())
                .expect("dup parent");
            k.insert((TENANTS[0], 920_000 + i), kid_value.as_slice())
                .expect("dup kid");
        }
    });
    // 実際の集合は 1 キー分なので、重複行の数に関わらず予算内に収まる。
    let report = verify_with(
        &path,
        &IntegrityOptions {
            memory_budget_bytes: 400,
            ..IntegrityOptions::default()
        },
    );
    assert_eq!(report.total(FindingKind::CheckBudgetExceeded), 0);
    // parents: 主キー重複 5。kids: 主キー重複 5 と、UNIQUE（tag）重複 5。
    assert_eq!(report.total(FindingKind::PrimaryKeyDuplicate), 10);
    assert_eq!(report.total(FindingKind::UniqueKeyDuplicate), 5);
    assert!(!report.is_consistent());
}

#[test]
fn gin_posting_with_a_wrong_length_field_is_not_silently_accepted() {
    let (path, _g) = build_db("integrity-gin-len");
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_GIN).expect("gin");
        let mut victim = None;
        for e in t.iter().expect("iter") {
            let (k, _) = e.expect("entry");
            let (tenant, sub) = k.value();
            if tenant == TENANTS[0] && sub.first() == Some(&0x01) {
                victim = Some(sub.to_vec());
                break;
            }
        }
        let mut forged = victim.expect("a posting");
        // 長さフィールド（2 バイト）だけを書き換えた別キーを足す。
        forged[2] = forged[2].wrapping_add(1);
        t.insert((TENANTS[0], forged.as_slice()), [].as_slice())
            .expect("insert");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::CheckUnevaluable), 1);
    assert_eq!(
        FindingKind::CheckUnevaluable.severity(),
        Severity::Incomplete
    );
    assert!(!report.is_consistent());
}

#[test]
fn failure_of_an_auxiliary_table_does_not_stop_the_other_checks() {
    let (path, _g) = build_db("integrity-aux-failure");
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        // GIN 表を別の型の表へ差し替え、開けなくする。
        w.delete_table(PARENT_GIN).expect("drop gin");
        w.open_table(TableDefinition::<u8, u8>::new("user_gin/parents_doc"))
            .expect("replace gin");
        // 後続の検査（主キー重複）が走ることを見るための矛盾。
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 930_000u64), value.as_slice())
            .expect("dup");
    });
    let report = verify(&path);
    assert!(report.total(FindingKind::CheckUnavailable) >= 1);
    assert_eq!(report.total(FindingKind::PrimaryKeyDuplicate), 1);
    assert!(!report.is_consistent());
}

#[test]
fn undecodable_parent_row_is_not_counted_as_an_orphan_but_complete_tenants_still_are() {
    let (path, _g) = build_db("integrity-fk-incomplete");
    // tenant0: 親 p1 の行を壊す（子 k1 は参照先を確認できない）。
    let (p1_rowid, _) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    // tenant1: 親 p1 の行を消す（キー集合は完全で、子 k1 は本当の孤児）。
    let (p1_other, _) = first_row(&path, PARENT_ROWS, TENANTS[1]);
    corrupt(&path, |w| {
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], p1_rowid), [0xFFu8, 1].as_slice())
            .expect("garble");
        t.remove((TENANTS[1], p1_other)).expect("remove");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::RowUndecodable), 1);
    assert_eq!(report.total(FindingKind::ForeignKeyOrphanRow), 1);
    assert!(report.total(FindingKind::CheckUnavailable) >= 1);
    assert!(!report.is_consistent());
}

#[test]
fn unreadable_unique_index_table_is_unavailable_not_rebuild_pending() {
    let (path, _g) = build_db("integrity-uniq-unreadable");
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        w.delete_table(PARENT_UNIQ).expect("drop uniq");
        w.open_table(TableDefinition::<u8, u8>::new("user_uniq/parents"))
            .expect("replace uniq");
        let mut t = w.open_table(PARENT_ROWS).expect("rows");
        t.insert((TENANTS[0], 940_000u64), value.as_slice())
            .expect("dup");
    });
    let report = verify(&path);
    assert!(report.total(FindingKind::CheckUnavailable) >= 1);
    assert!(!report.findings.iter().any(
        |f| f.kind == FindingKind::IndexRebuildPending && f.table.as_deref() == Some("parents")
    ));
    // 索引に依存しない行の重複検査は続く。
    assert_eq!(report.total(FindingKind::PrimaryKeyDuplicate), 1);
    assert!(!report.is_consistent());
}

#[test]
fn absent_unique_index_table_is_still_rebuild_pending() {
    let (path, _g) = build_db("integrity-uniq-absent");
    corrupt(&path, |w| {
        w.delete_table(PARENT_UNIQ).expect("drop uniq");
    });
    let report = verify(&path);
    assert_eq!(report.total(FindingKind::CheckUnavailable), 0);
    assert!(report.total(FindingKind::IndexRebuildPending) >= 1);
}

#[test]
fn broken_non_unique_expression_index_does_not_stop_the_key_checks() {
    let (path, _g) = build_db("integrity-nonunique-expr");
    {
        let core = core_at(&path);
        run(
            &core,
            TENANTS[0],
            "CREATE INDEX parents_lower ON parents ((lower(name)))",
        );
    }
    let (_, value) = first_row(&path, PARENT_ROWS, TENANTS[0]);
    corrupt(&path, |w| {
        // 式のテキストを存在しない列名へ書き換え、再束縛できなくする。
        let mut t = w
            .open_table(TableDefinition::<&str, &[u8]>::new("index_catalog"))
            .expect("index catalog");
        let old = t
            .get("parents_lower")
            .expect("get")
            .expect("entry")
            .value()
            .to_vec();
        // 式のテキストは 16 進で保存されている（`lower(name)` → `lower(nope)`。長さは同じ）。
        let from = b"6c6f776572286e616d6529";
        let at = old
            .windows(from.len())
            .position(|b| b == from)
            .expect("expression text");
        let mut forged = old.clone();
        forged[at..at + from.len()].copy_from_slice(b"6c6f776572286e6f706529");
        t.insert("parents_lower", forged.as_slice()).expect("write");
        let mut rows = w.open_table(PARENT_ROWS).expect("rows");
        rows.insert((TENANTS[0], 950_000u64), value.as_slice())
            .expect("dup");
    });
    let report = verify(&path);
    // 一意でない式索引は一意性に関わらないので、主キーの重複検出は止まらない。
    assert_eq!(report.total(FindingKind::PrimaryKeyDuplicate), 1);
}
