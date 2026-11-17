//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の宣言（`CREATE TABLE`）とカタログの永続形の結合テスト
//! （Issue #3327。ポインタ: `docs/spec/04-behavior/` の TABLE-50 (1)(4)(5)・RECOVER-11・TASK-345、
//! 設計記録: `docs/design/generated-column.md`）。
//!
//! 検証の軸は 5 つ。(1) 受理する形（定数式・算術・CASE・COALESCE・文字列関数・日時・各種制約との併記・前方参照）。
//! (2) 拒否する形が決めた SQLSTATE で返り、拒否後に表もカタログも残らないこと。(3) 再起動後と
//! `export_schema_ddl` の往復後も生成式が保たれること。(4) 計算点が未結線の間、生成列を持つ表への
//! 書き込みが固定文言の `0A000` で拒否され行・台帳が残らないこと（DELETE・TRUNCATE・SELECT は通る）。
//! (5) テナント境界（拒否の文言に他テナントの情報・表の存在情報を含まない）。

use engine::catalog::ddl_export::SchemaDdlScope;
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-a";
const OTHER: &str = "tenant-b";

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

fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    (path, guard, core)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()));
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn message(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.to_string(),
    }
}

/// 範囲（共有 + テナント）の DDL を文字列の列として集める。
fn dump(core: &EngineCore, tenant: &str) -> Vec<String> {
    let mut out = Vec::new();
    core.export_schema_ddl(&SchemaDdlScope::shared_and_tenant(tenant), |st| {
        out.push(st.sql.to_string());
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("export");
    out
}

fn create_stmt_of(core: &EngineCore, tenant: &str, table: &str) -> String {
    dump(core, tenant)
        .into_iter()
        .find(|s| s.starts_with("CREATE TABLE") && s.contains(&format!("\"{table}\"")))
        .unwrap_or_else(|| panic!("CREATE TABLE for {table} must be dumped"))
}

#[test]
fn accepts_documented_forms_and_keeps_canonical_expression() {
    let (_p, _g, core) = fixture("gen-accept");
    for sql in [
        // 定数式。
        "CREATE TABLE c1 (a INTEGER, b INTEGER GENERATED ALWAYS AS (1 + 1) STORED)",
        // 算術・括弧・単項マイナス。
        "CREATE TABLE c2 (a INTEGER, b INTEGER, s BIGINT GENERATED ALWAYS AS ((a + b) * 2 - -1) STORED)",
        // CASE・COALESCE。
        "CREATE TABLE c3 (a INTEGER, b TEXT GENERATED ALWAYS AS (CASE WHEN a > 0 THEN 'pos' ELSE 'neg' END) STORED, \
         c INTEGER GENERATED ALWAYS AS (COALESCE(a, 0)) STORED)",
        // 文字列関数・連結。
        "CREATE TABLE c4 (first TEXT, last TEXT, full_name TEXT GENERATED ALWAYS AS (lower(first) || ' ' || upper(last)) STORED)",
        // 日時。
        "CREATE TABLE c5 (d DATE, y INTEGER GENERATED ALWAYS AS (date_part('year', d)) STORED)",
        // 前方参照（生成列を先に宣言し、参照する列を後ろに宣言する）。
        "CREATE TABLE c6 (g INTEGER GENERATED ALWAYS AS (a + 1) STORED, a INTEGER)",
        // 同じ列に NOT NULL・UNIQUE・CHECK・COLLATE を併記する（宣言の順は問わない）。
        "CREATE TABLE c7 (a INTEGER, b TEXT GENERATED ALWAYS AS ('x' || a) STORED NOT NULL UNIQUE CHECK (b <> ''))",
        "CREATE TABLE c8 (a TEXT, b TEXT COLLATE \"C\" GENERATED ALWAYS AS (lower(a)) STORED)",
        // 主キー・FK の参照元（NO ACTION）・索引・CHECK から生成列を参照できる。
        "CREATE TABLE c9 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED PRIMARY KEY)",
        "CREATE TABLE c10 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, CHECK (b >= 0), UNIQUE (b))",
        "CREATE TABLE c11 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED REFERENCES c9 (b))",
        "CREATE TABLE c12 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED REFERENCES c9 (b) ON DELETE CASCADE)",
        // psql 形（末尾 `;`）。
        "CREATE TABLE c13 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED);",
    ] {
        ok(&core, TENANT, sql);
    }
    ok(&core, TENANT, "CREATE INDEX c10_b_idx ON c10 (b)");
    // 正規描画（空白・括弧・大小は描画器が決める）が DDL に出る。
    let c2 = create_stmt_of(&core, TENANT, "c2");
    assert!(c2.contains("GENERATED ALWAYS AS ("), "{c2}");
    assert!(c2.contains(") STORED"), "{c2}");
    // 生成列は DEFAULT を持たない。
    assert!(!c2.contains("DEFAULT"), "{c2}");
    let c7 = create_stmt_of(&core, TENANT, "c7");
    assert!(c7.contains("NOT NULL"), "{c7}");
}

#[test]
// `0A000` の固定: ADD COLUMN は #3330 が対応する。一時表・ベクトル系・式で代入できない型・式内 COLLATE は設計記録の対象外。
// tracked: #3330
// intentional: docs/design/generated-column.md
fn rejects_forms_with_decided_sqlstates_and_leaves_no_table() {
    let (_p, _g, core) = fixture("gen-reject");
    ok(&core, TENANT, "CREATE TABLE base (a INTEGER)");
    for (sql, expected) in [
        // 構文（42601）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) VIRTUAL)",
            "42601",
        ),
        ("CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a))", "42601"),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED BY DEFAULT AS (a) STORED)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER DEFAULT 1 GENERATED ALWAYS AS (a) STORED)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED DEFAULT 1)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED DEFAULT NULL)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED GENERATED ALWAYS AS (a) STORED)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED GENERATED ALWAYS AS IDENTITY)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS IDENTITY GENERATED ALWAYS AS (a) STORED)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b SERIAL GENERATED ALWAYS AS (a) STORED)",
            "42601",
        ),
        ("CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS () STORED)", "42601"),
        // 部分クエリ・集計・ウィンドウ・SRF（42P17。TABLE-50 (1)）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS ((SELECT 1)) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (count(a)) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (sum(a) OVER ()) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (generate_series(1, 2)) STORED)",
            "42P17",
        ),
        // 非決定的な式（42P17）。
        (
            "CREATE TABLE r (a INTEGER, b TIMESTAMPTZ GENERATED ALWAYS AS (now()) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b DOUBLE PRECISION GENERATED ALWAYS AS (random()) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b DATE GENERATED ALWAYS AS (CURRENT_DATE) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b BIGINT GENERATED ALWAYS AS (nextval('s')) STORED)",
            "42P17",
        ),
        // 代入キャストがセッション TZ 依存の変換を挟む宣言（42P17）。
        (
            "CREATE TABLE r (a TIMESTAMP, b TIMESTAMPTZ GENERATED ALWAYS AS (a) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a TIMESTAMPTZ, b TIMESTAMP GENERATED ALWAYS AS (a) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a TIMESTAMPTZ, b DATE GENERATED ALWAYS AS (a) STORED)",
            "42P17",
        ),
        // ARRAY[...] 内の後続要素に置いた部分クエリ・集計も拒否される（42P17）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER[] GENERATED ALWAYS AS (ARRAY[a, (SELECT 1)]) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER[] GENERATED ALWAYS AS (ARRAY[a, count(*)]) STORED)",
            "42P17",
        ),
        // 組み込み以外の関数（42P17）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (no_such_fn(a)) STORED)",
            "42P17",
        ),
        // 束縛段: 未知の列（42703）・生成列の参照（42P17）・自分自身の参照（42P17）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (zzz + 1) STORED)",
            "42703",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED, c INTEGER GENERATED ALWAYS AS (b) STORED)",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (b + 1) STORED)",
            "42P17",
        ),
        // 他の表・テナント内部列は参照できない（通常の列として解決できず 42703）。
        // 修飾名は構文段で `42601`（列の参照は同じ行の素の列名だけ）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (base.a) STORED)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b TEXT GENERATED ALWAYS AS (tenant_id) STORED)",
            "42703",
        ),
        (
            "CREATE TABLE r (a INTEGER, b TEXT GENERATED ALWAYS AS (visibility) STORED)",
            "42703",
        ),
        // 型の不一致（42804）。
        (
            "CREATE TABLE r (a INTEGER, b TEXT GENERATED ALWAYS AS (a + 1) STORED)",
            "42804",
        ),
        (
            "CREATE TABLE r (a TEXT, b INTEGER GENERATED ALWAYS AS (a || 'x') STORED)",
            "42804",
        ),
        // 式の中の COLLATE（0A000）。
        (
            "CREATE TABLE r (a TEXT, b TEXT GENERATED ALWAYS AS (a COLLATE \"C\") STORED)",
            "0A000",
        ),
        // 式で代入できない列型・ベクトル系（0A000）。
        (
            "CREATE TABLE r (a INTEGER, b NUMERIC(10, 2) GENERATED ALWAYS AS (a) STORED)",
            "0A000",
        ),
        (
            "CREATE TABLE r (a INTEGER, b VECTOR(2) GENERATED ALWAYS AS (a) STORED)",
            "0A000",
        ),
        (
            "CREATE TABLE r (v VECTOR(2), b INTEGER GENERATED ALWAYS AS (v) STORED)",
            "0A000",
        ),
        // 一時表・ALTER TABLE ADD COLUMN（0A000）。
        (
            "CREATE TEMP TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED)",
            "0A000",
        ),
        (
            "ALTER TABLE base ADD COLUMN b INTEGER GENERATED ALWAYS AS (a) STORED",
            "0A000",
        ),
        // 生成列を参照元に含む FK の参照アクション（42601）。
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED REFERENCES base (a) ON UPDATE CASCADE)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED REFERENCES base (a) ON DELETE SET NULL)",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED, FOREIGN KEY (b) REFERENCES base (a) ON DELETE SET DEFAULT)",
            "42601",
        ),
        // 式の長さ超過（54000）。
        (
            &format!(
                "CREATE TABLE r (a INTEGER, b TEXT GENERATED ALWAYS AS ({}) STORED)",
                (0..600)
                    .map(|_| "'abcdefgh'")
                    .collect::<Vec<_>>()
                    .join(" || ")
            ),
            "54000",
        ),
    ] {
        assert_eq!(code(&core, TENANT, sql), expected, "{sql}");
    }
    // 拒否された文は表もカタログも残さない（同名で作り直せる）。
    ok(&core, TENANT, "CREATE TABLE r (a INTEGER)");
}

/// 拒否の文言は固定で、他テナントの情報・表の存在情報・値を含まない。構文段の拒否はカタログを引かない。
#[test]
fn rejection_messages_do_not_leak_tenant_or_table_information() {
    let (_p, _g, core) = fixture("gen-leak");
    ok(&core, OTHER, "CREATE TABLE secret_table (a INTEGER)");
    for sql in [
        "CREATE TABLE secret_table (a INTEGER, b INTEGER GENERATED ALWAYS AS (now()) STORED)",
        "CREATE TABLE secret_table (a INTEGER, b INTEGER GENERATED ALWAYS AS ((SELECT 1)) STORED)",
        "CREATE TABLE fresh_table (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED NOT NULL DEFAULT 5)",
        "CREATE TABLE fresh_table (a INTEGER, b TEXT GENERATED ALWAYS AS (a + 1) STORED)",
        "CREATE TEMP TABLE fresh_table (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED)",
    ] {
        let m = message(&core, TENANT, sql);
        assert!(!m.contains(OTHER), "{m}");
        assert!(!m.contains("secret_table"), "{m}");
    }
    // 構文段で決まる拒否は、同名の表が他テナントに存在しても同じ結果になる（存在オラクルにならない）。
    let with_other = code(
        &core,
        TENANT,
        "CREATE TABLE secret_table (a INTEGER, b INTEGER GENERATED ALWAYS AS (now()) STORED)",
    );
    let without_other = code(
        &core,
        TENANT,
        "CREATE TABLE never_existed (a INTEGER, b INTEGER GENERATED ALWAYS AS (now()) STORED)",
    );
    assert_eq!(with_other, without_other);
    assert_eq!(with_other, "42P17");
}

/// 再起動後も生成式・列順が保たれる。`export_schema_ddl` の往復で別 DB へ復元でき、再ダンプが一致する。
#[test]
fn persists_across_reopen_and_roundtrips_through_schema_dump() {
    let path = unique_db_path("gen-persist");
    let _g = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        for sql in [
            "CREATE TABLE g (a INTEGER, g1 INTEGER GENERATED ALWAYS AS (a * 2 + 1) STORED NOT NULL, \
             name TEXT, up TEXT COLLATE \"C\" GENERATED ALWAYS AS (upper(name)) STORED, \
             pk INTEGER GENERATED ALWAYS AS (a) STORED, UNIQUE (pk))",
            "CREATE TABLE plain (x INTEGER)",
        ] {
            ok(&core, TENANT, sql);
        }
    }
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("g").expect("schema");
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a", "g1", "name", "up", "pk"]);
    assert!(schema.columns[1].is_generated());
    assert!(schema.columns[3].is_generated());
    assert!(schema.columns[1]
        .generation_expression()
        .is_some_and(|e| e.contains('a')));
    assert!(!schema.columns[0].is_generated());
    assert!(!storage
        .get_table_schema("plain")
        .expect("plain")
        .columns
        .iter()
        .any(|c| c.is_generated()));
    drop(storage);

    // ダンプの往復。
    let core = open_core(&path);
    let first = dump(&core, TENANT);
    let create = create_stmt_of(&core, TENANT, "g");
    assert!(create.contains("GENERATED ALWAYS AS ("), "{create}");
    assert!(create.contains(") STORED"), "{create}");
    drop(core);

    let restore_path = unique_db_path("gen-persist-restore");
    let _g2 = CleanupGuard(restore_path.clone());
    let restored = open_core(&restore_path);
    for sql in &first {
        if sql.starts_with("RESTORE ") {
            restored
                .apply_restore_directive(TENANT, sql)
                .unwrap_or_else(|e| panic!("restore directive must apply: {e}"));
        } else {
            ok(&restored, TENANT, sql);
        }
    }
    assert_eq!(dump(&restored, TENANT), first);
    // 復元後の表も生成列として動作する（書き込みは暫定の `0A000`）。
    assert_eq!(
        code(
            &restored,
            TENANT,
            "INSERT INTO g (a, g1, name) VALUES (1, 1, 'x')"
        ),
        "0A000"
    );
}

/// 計算点が未結線の間、生成列を持つ表への書き込みは固定文言の `0A000` で拒否され、行・台帳が残らない。
/// DELETE・TRUNCATE・SELECT は通る。
#[test]
// 書き込み経路の暫定停止（SQL 経路は #3328、COPY・ビュー等は #3329 が結線して外す）。
// tracked: #3328, #3329
fn writes_to_generated_tables_are_rejected_until_wired() {
    let (_p, _g, core) = fixture("gen-write-stop");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ok(&core, TENANT, "CREATE VIEW gv AS SELECT a FROM g");
    for sql in [
        "INSERT INTO g (a) VALUES (1)",
        "INSERT INTO g (a) VALUES (1), (2)",
        "INSERT INTO g (a, b) VALUES (1, DEFAULT)",
        "INSERT INTO g (a) SELECT 1",
        "INSERT INTO g (a) VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT INTO gv (a) VALUES (1)",
    ] {
        assert_eq!(code(&core, TENANT, sql), "0A000", "{sql}");
        let m = message(&core, TENANT, sql);
        assert!(!m.contains(TENANT), "{m}");
        assert!(!m.contains("\"g\""), "{m}");
    }
    // COPY FROM STDIN。
    let policy = ctx(TENANT);
    let session = SessionState::default();
    let copy = core
        .begin_copy(&policy, &session, "COPY g (a) FROM STDIN")
        .and_then(|plan| match plan {
            CopyPlan::From(mut s) => {
                s.feed(b"1\n")?;
                let batch = s.finish()?;
                core.commit_copy_in(&policy, batch)
            }
            CopyPlan::To(..) => panic!("expected COPY FROM plan"),
        });
    assert_eq!(
        copy.expect_err("COPY must be rejected").wire_code(),
        "0A000"
    );
    // 行は 1 件も残らない。
    match run(&core, TENANT, "SELECT a FROM g").expect("select") {
        SqlOutcome::Query(q) => assert!(q.rows.is_empty()),
        other => panic!("expected rows, got {other:?}"),
    }
    // 計算を伴わない文は通る。
    ok(&core, TENANT, "UPDATE g SET a = 1 WHERE a = 0");
    ok(&core, TENANT, "DELETE FROM g WHERE a = 0");
    ok(&core, TENANT, "TRUNCATE g");
    // 他テナントでも同じ結果で、拒否の後に別の表への書き込みは通る。
    assert_eq!(
        code(&core, OTHER, "INSERT INTO g (a) VALUES (1)"),
        "0A000",
        "the catalog is shared across tenants, so the rejection is the same"
    );
}

/// 参照列の DROP・RENAME は拒否され、参照しない列の変更・生成列自身の改名・表の改名の後も生成式が残る。
#[test]
// ALTER の暫定ガード（最終形は #3331）。
// tracked: #3331
fn alter_guards_and_surviving_alters_keep_the_expression() {
    let (_p, _g, core) = fixture("gen-alter");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, other TEXT, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    // 参照列: DROP は 2BP01、RENAME・型変更は 0A000。
    assert_eq!(code(&core, TENANT, "ALTER TABLE g DROP COLUMN a"), "2BP01");
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g RENAME COLUMN a TO a2"),
        "0A000"
    );
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g ALTER COLUMN a TYPE BIGINT"),
        "0A000"
    );
    // 生成列自身の型変更・DEFAULT の付け替えも拒否する。
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g ALTER COLUMN b TYPE BIGINT"),
        "0A000"
    );
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g ALTER COLUMN b SET DEFAULT 1"),
        "42601"
    );
    // 通る変更（参照しない列・NOT NULL・制約・改名）。
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ALTER COLUMN other SET NOT NULL",
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ALTER COLUMN other DROP NOT NULL",
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ADD CONSTRAINT g_a_pos CHECK (a > 0)",
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ALTER COLUMN other TYPE VARCHAR(10)",
    );
    ok(&core, TENANT, "ALTER TABLE g RENAME COLUMN b TO b2");
    ok(&core, TENANT, "ALTER TABLE g RENAME TO g2");
    let create = create_stmt_of(&core, TENANT, "g2");
    assert!(create.contains("GENERATED ALWAYS AS ("), "{create}");
    assert!(create.contains("\"b2\""), "{create}");
    // 参照列は CHECK からも参照される（2BP01）。生成列自身の DROP は通り、制約を外せば参照列も落とせる。
    ok(&core, TENANT, "ALTER TABLE g2 DROP COLUMN b2");
    assert_eq!(code(&core, TENANT, "ALTER TABLE g2 DROP COLUMN a"), "2BP01");
    ok(&core, TENANT, "ALTER TABLE g2 DROP CONSTRAINT g_a_pos");
    ok(&core, TENANT, "ALTER TABLE g2 DROP COLUMN a");
}

#[test]
// intentional: docs/design/generated-column.md
fn alter_add_foreign_key_applies_the_same_referential_action_guard() {
    let (_p, _g, core) = fixture("t50-alter-fk");
    ok(&core, TENANT, "CREATE TABLE par (k INTEGER PRIMARY KEY)");
    ok(
        &core,
        TENANT,
        "CREATE TABLE ch (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) STORED, c INTEGER)",
    );
    // 生成列を参照元に含む FK の禁止アクションは CREATE TABLE と同じ `42601`。拒否後に制約は残らない。
    for action in [
        "ON UPDATE CASCADE",
        "ON UPDATE SET NULL",
        "ON UPDATE SET DEFAULT",
        "ON DELETE SET NULL",
        "ON DELETE SET DEFAULT",
    ] {
        let sql = format!(
            "ALTER TABLE ch ADD CONSTRAINT ch_fk FOREIGN KEY (b) REFERENCES par (k) {action}"
        );
        assert_eq!(code(&core, TENANT, &sql), "42601", "{sql}");
    }
    // 許可アクション（ON DELETE CASCADE・NO ACTION）は通る。拒否された制約名が残っていれば
    // 同名の追加が衝突するため、同名で通ること自体が「拒否後に残らない」確認を兼ねる。
    ok(
        &core,
        TENANT,
        "ALTER TABLE ch ADD CONSTRAINT ch_fk FOREIGN KEY (b) REFERENCES par (k) ON DELETE CASCADE",
    );
    // 生成列を含まない FK は従来どおり全アクションを使える。
    ok(
        &core,
        TENANT,
        "ALTER TABLE ch ADD CONSTRAINT ch_fk3 FOREIGN KEY (c) REFERENCES par (k) ON DELETE SET NULL ON UPDATE CASCADE",
    );
}
