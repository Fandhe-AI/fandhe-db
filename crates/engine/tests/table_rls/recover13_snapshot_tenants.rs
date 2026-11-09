//! 書き出した出力から再び開いたとき、全テナントのデータが読め、テナント境界（RLS 相当の
//! メタデータを含む）が保たれることの結合テスト（RECOVER-13・Issue #2153。ポインタ:
//! `docs/spec/04-behavior/recovery.md` の RECOVER-13、方式は `docs/design/backup-snapshot.md`）。
//!
//! 検査する契約:
//! 1. 全テナントの行（同じ表に複数テナントの行がある場合を含む）が出力から読める
//! 2. 他テナントの行は見えない（行を持たないテナントには 0 行。元の DB と同じ）
//! 3. 行セキュリティの有効化とポリシーが往復し、出力でも効く（他テナントには効かない）
//! 4. ロールのログイン情報（SCRAM 検証子）は保存の形式のまま往復し、出力に平文の
//!    パスワードが現れない。バイト列の列（暗号化済みの値のような不透明な値）は
//!    そのまま往復する。暗号化済み列を定める仕組みは本リポジトリに無いため、バックアップは
//!    保存の形式のまま含める契約（RECOVER-13 (4)）をこの形で確認する

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const PASSWORD_MARKER: &str = "pw-recover13-plaintext-marker";
/// 暗号化済みの値に見立てた不透明なバイト列（NUL・0xff・UTF-8 として不正な並びを含む）。
const OPAQUE: [u8; 12] = [
    0x00, 0xff, 0xfe, 0x80, 0x01, 0xc3, 0x28, 0xa0, 0xa1, 0x00, 0x7f, 0x9c,
];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

/// (id, tag) の昇順一覧。
fn notes(core: &EngineCore, tenant: &str) -> Vec<(i64, String)> {
    match run(core, tenant, "SELECT id, tag FROM notes LIMIT 1000").expect("select notes") {
        SqlOutcome::Query(q) => {
            let mut v: Vec<(i64, String)> = q
                .rows
                .iter()
                .map(|r| match (&r.cells[0], &r.cells[1]) {
                    (Cell::SignedInteger(i), Cell::Text(t)) => (*i, t.clone()),
                    other => panic!("unexpected cells {other:?}"),
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn blob_of(core: &EngineCore, tenant: &str, id: i64) -> Cell {
    match run(
        core,
        tenant,
        &format!("SELECT blob FROM notes WHERE id = {id} LIMIT 1"),
    )
    .expect("select blob")
    {
        SqlOutcome::Query(q) => q.rows[0].cells[0].clone(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn verifier(core: &EngineCore, tenant: &str, role: &str) -> Option<String> {
    let ident = EngineCore::login_identifier(tenant, role).expect("identifier");
    core.resolve_login_role(&ident)
        .expect("lookup")
        .map(|c| c.scram_verifier.clone())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn snapshot_reopens_with_all_tenants_policies_roles_and_opaque_bytes() {
    let src_path = unique_db_path("recover13-tenants-src");
    let _g1 = CleanupGuard(src_path.clone());
    // 出力先の親は共有でない（0700）ディレクトリにする（Issue #3135）。
    let out_dir = temp_db::TempDir::new("recover13-tenants-out");
    let out_path = out_dir.path().join("out.redb");
    let src = open_core(&src_path);

    // tenant-a: 行セキュリティ有効・ポリシーで tag = 'vis' の行だけが見える。
    ok(
        &src,
        "tenant-a",
        "CREATE TABLE notes (id BIGINT PRIMARY KEY, tag TEXT, blob BYTEA)",
    );
    ok(
        &src,
        "tenant-a",
        &format!(
            "INSERT INTO notes (id, tag, blob) VALUES (1, 'vis', '\\x{}'), (2, 'hid', '\\x00'), (3, 'vis', NULL)",
            hex(&OPAQUE)
        ),
    );
    ok(
        &src,
        "tenant-a",
        "ALTER TABLE notes ENABLE ROW LEVEL SECURITY",
    );
    ok(
        &src,
        "tenant-a",
        "CREATE POLICY only_vis ON notes FOR ALL USING (tag = 'vis')",
    );
    // tenant-b: 同じ表へ自分の行を入れる（表は DB 全体の名前空間・行はテナントごと。ポリシーは a だけの定義）。
    ok(
        &src,
        "tenant-b",
        "INSERT INTO notes (id, tag, blob) VALUES (10, 'hid', '\\xabcd'), (11, 'b-only', NULL)",
    );
    ok(
        &src,
        "tenant-a",
        &format!("CREATE ROLE app LOGIN PASSWORD '{PASSWORD_MARKER}'"),
    );
    ok(
        &src,
        "tenant-b",
        "CREATE ROLE app LOGIN PASSWORD 'other-pw'",
    );

    // 元の DB での各テナントの応答（正解）。
    let a_rows = vec![(1, "vis".to_string()), (3, "vis".to_string())];
    let b_rows = vec![(10, "hid".to_string()), (11, "b-only".to_string())];
    assert_eq!(notes(&src, "tenant-a"), a_rows);
    assert_eq!(notes(&src, "tenant-b"), b_rows);
    assert!(notes(&src, "tenant-c").is_empty());
    let va = verifier(&src, "tenant-a", "app").expect("a role");
    let vb = verifier(&src, "tenant-b", "app").expect("b role");
    assert_ne!(va, vb);

    src.export_snapshot(&out_path).expect("export snapshot");

    // 出力ファイルに平文のパスワードを書かない（保存の形式のまま）。
    let raw = std::fs::read(&out_path).expect("read output");
    assert!(!raw
        .windows(PASSWORD_MARKER.len())
        .any(|w| w == PASSWORD_MARKER.as_bytes()));
    // バイト列の列の中身は保存の形式のままファイルに現れる（加工されない）。
    assert!(raw.windows(OPAQUE.len()).any(|w| w == OPAQUE));

    // 出力から開き直す（元の DB とは独立）。
    let copy = open_core(&out_path);
    // 1. 全テナントのデータが読め、2./3. 境界とポリシーが元と同じ。
    assert_eq!(notes(&copy, "tenant-a"), a_rows);
    assert_eq!(notes(&copy, "tenant-b"), b_rows);
    // 行を持たないテナントには 1 行も見えない（元の DB と同じ）。
    assert!(notes(&copy, "tenant-c").is_empty());
    // ポリシーは出力でも書き込みに効く（tag = 'hid' の行は tenant-a に見えず、更新対象にならない）。
    let upd = run(&copy, "tenant-a", "UPDATE notes SET tag = 'x' WHERE id = 2").expect("update");
    match upd {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected Update, got {other:?}"),
    }
    assert_eq!(notes(&copy, "tenant-b"), b_rows, "b is unaffected");

    // ポリシーで不可視の行（id=2）が出力に保存されていること。復元先だけで所有者が行セキュリティを
    // 無効にして読む（元の DB は触らない）。不可視の行を落とす回帰はここで検知する。
    ok(
        &copy,
        "tenant-a",
        "ALTER TABLE notes DISABLE ROW LEVEL SECURITY",
    );
    assert_eq!(
        notes(&copy, "tenant-a"),
        vec![
            (1, "vis".to_string()),
            (2, "hid".to_string()),
            (3, "vis".to_string())
        ],
        "hidden row must be stored in the snapshot"
    );
    assert_eq!(blob_of(&copy, "tenant-a", 2), Cell::Bytes(vec![0x00]));
    // 元の DB のポリシーは無効化されていない。
    assert_eq!(notes(&src, "tenant-a"), a_rows);

    // 4. ロールの検証子は同一、他テナントからは引けず、バイト列は一致。
    assert_eq!(verifier(&copy, "tenant-a", "app"), Some(va));
    assert_eq!(verifier(&copy, "tenant-b", "app"), Some(vb));
    assert_eq!(verifier(&copy, "tenant-c", "app"), None);
    assert_eq!(blob_of(&copy, "tenant-a", 1), Cell::Bytes(OPAQUE.to_vec()));
    assert_eq!(blob_of(&copy, "tenant-a", 3), Cell::Null);
    assert_eq!(
        blob_of(&copy, "tenant-b", 10),
        Cell::Bytes(vec![0xab, 0xcd])
    );

    // 書き出し後の元の DB への書き込みは出力に現れない。
    ok(
        &src,
        "tenant-b",
        "INSERT INTO notes (id, tag, blob) VALUES (12, 'late', NULL)",
    );
    drop(copy);
    let copy2 = open_core(&out_path);
    assert_eq!(notes(&copy2, "tenant-b").len(), b_rows.len());
}
