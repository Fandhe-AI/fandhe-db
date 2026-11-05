//! 列単位の権限制御の全経路越境マトリクス（Issue #2084。ポインタ: RLS-12・SQL-40。ADR
//! `docs/design/column-level-privilege.md` の C1・C5・C6 と §8・§9）。
//!
//! `src/core/column_privilege_tests.rs`（#2083）は文の形ごとの単発テストにとどまる。本ファイルは
//! 「文の形（列を置く位置）× 入口（経路）× 付与状態」の全組み合わせを 1 つの行列で回し、許可外の列の
//! 値・存在・付与状態が応答に漏れないことを、`rls10_relational_paths.rs` と同じ枠組みで固定する。
//! HTTP（NoSQL）表層の対応する行列は wire-server の `tests/http_nosql/rls12_nosql_column_privilege.rs`。
//!
//! - **T0 非空虚性**: 表単位の付与（陽性対照）で全セルが成功し、軸（位置・入口）ごとに成功と `42501` が
//!   1 件以上ある。構文未対応で拒否側が空振りする事態を防ぐ。
//! - **T1 独立オラクル**: 本ファイルの真理値表（`expected`）が、production の `cover_bits`・`resolve`・
//!   `is_visible` を一切呼ばずに期待コードを持つ。成功セルの出力には許可外の列の秘密トークンも
//!   他テナントのトークンも現れない。PG の意味のオラクル（`pg_allows`）は「production が成功なら PG も成功」
//!   を全セルで確かめ、fail-open を検出する。
//! - **T2 不変性**: `42501` の `client_message` は全セルでバイト一致し列名・表名・ロール名・テナントを含まない。
//!   他テナントの付与と行（baseline／flooded／foreign-grant の 3 世界）は閲覧テナントの応答を変えない。
//! - **T3 応答の境界**: 他テナントだけにある列名と、どこにも無い列名の応答が一致する。拡張クエリの拒否は
//!   Describe 以前に起きる。
//! - **T6 負の対照**: 漏えい検出器・オラクル照合に捏造した違反を与えて検出できることを確かめる。
//!
//! ロール ID は作成順で決まる（運用者が `r0..r6`・`w0..w6` を閲覧テナント → 他テナントの順に作るため、
//! 付与者 `adm` = 1、閲覧側は `r<i>` = `2 + i`、`w<i>` = `9 + i`）。付与が効いていること（ID の取り違えが無いこと）は T0 の陽性対照で
//! 毎回確かめる。

use std::collections::BTreeMap;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 実列（`ct`・`ca`・`cj` 共通）。疑似列 `id` は含まない。
const REAL_COLS: [&str; 4] = ["embedding", "pub_c", "secret", "n"];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

// ---------------------------------------------------------------------------------------------
// 付与状態
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Gs {
    None,
    PubCol,
    PubEmb,
    PubN,
    AllReal,
    Table,
    /// 表単位の付与を `REVOKE` した後に、列単位の付与（`pub_c`）だけが残る状態。
    TableThenRevoked,
}

const STATES: [Gs; 7] = [
    Gs::None,
    Gs::PubCol,
    Gs::PubEmb,
    Gs::PubN,
    Gs::AllReal,
    Gs::Table,
    Gs::TableThenRevoked,
];

impl Gs {
    fn idx(self) -> u64 {
        STATES.iter().position(|s| *s == self).expect("state") as u64
    }

    /// オラクル側が持つ「実効の付与」（表単位か）。
    fn table_level(self) -> bool {
        self == Gs::Table
    }

    /// オラクル側が持つ「実効の列単位の付与」。
    fn cols(self) -> &'static [&'static str] {
        match self {
            // 表単位の `REVOKE` は列単位の付与も取り消す（PG と同じ）ため、実効の付与は無い。
            Gs::None | Gs::Table | Gs::TableThenRevoked => &[],
            Gs::PubCol => &["pub_c"],
            Gs::PubEmb => &["pub_c", "embedding"],
            Gs::PubN => &["pub_c", "n"],
            Gs::AllReal => &REAL_COLS,
        }
    }

    fn any_grant(self) -> bool {
        self.table_level() || !self.cols().is_empty()
    }
}

/// 閲覧側の読み取りロール ID（`r<i>`）。
fn read_role(s: Gs) -> u64 {
    2 + s.idx()
}

/// 閲覧側の書き込みロール ID（`w<i>`。表単位の INSERT／UPDATE／DELETE を常に持つ）。
fn write_role(s: Gs) -> u64 {
    2 + STATES.len() as u64 + s.idx()
}

// ---------------------------------------------------------------------------------------------
// フィクスチャ
// ---------------------------------------------------------------------------------------------

struct World {
    _g: CleanupGuard,
    core: EngineCore,
    tenant: &'static str,
}

fn operator(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

fn bound(tenant: &str, id: u64) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(tenant));
    s
}

/// 付与者 `adm`（運用者権限つきで束縛したロール）の ID。
const ADMIN: u64 = 1;

fn setup(core: &EngineCore, tenant: &str, sql: &str) {
    setup_with(core, tenant, ADMIN, sql);
}

/// 他テナント側の付与者 `adm` の ID（閲覧側 15 件の次）。
const FOREIGN_ADMIN: u64 = 16;

fn setup_with(core: &EngineCore, tenant: &str, admin: u64, sql: &str) {
    let mut s = if sql.starts_with("GRANT") || sql.starts_with("REVOKE") {
        let mut b = bound(tenant, admin);
        b.allow_ddl();
        b
    } else {
        operator(tenant)
    };
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), &mut s, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

fn token(tenant: &str, table: &str, i: u64) -> String {
    format!("zzs_{tenant}_{table}_{i}")
}

fn seed_tables(core: &EngineCore, tenant: &str, create: bool) {
    if create {
        for t in ["ca", "cj"] {
            setup(
                core,
                tenant,
                &format!(
                    "CREATE TABLE {t} (embedding vector(2), pub_c TEXT, secret TEXT, n BIGINT)"
                ),
            );
        }
        setup(core, tenant, "CREATE VIEW v_pub AS SELECT pub_c FROM ca");
    }
    for i in 1..=2u64 {
        setup(
            core,
            tenant,
            &format!(
                "INSERT INTO ct (id, embedding, pub_c, secret, n) VALUES ({i}, '[1.0, 0.0]', 'pubtok_{i}', '{}', {i})",
                token(tenant, "ct", i)
            ),
        );
        for t in ["ca", "cj"] {
            setup(
                core,
                tenant,
                &format!(
                    "INSERT INTO {t} (embedding, pub_c, secret, n) VALUES ('[1.0, 0.0]', 'pubtok_{i}', '{}', {i})",
                    token(tenant, t, i)
                ),
            );
        }
    }
}

/// 状態 `s` の付与を `role` へ与える（`tables` の各表に同じ列／表単位の付与を行う）。
fn apply_grants(core: &EngineCore, tenant: &str, role: &str, s: Gs, with_view: bool) {
    let tables = ["ct", "ca", "cj"];
    let g = |sql: String| setup(core, tenant, &sql);
    for t in tables {
        match s {
            Gs::None => {}
            Gs::Table => g(format!("GRANT SELECT ON {t} TO {role}")),
            Gs::TableThenRevoked => {
                g(format!("GRANT SELECT ON {t} TO {role}"));
                g(format!("GRANT SELECT (pub_c) ON {t} TO {role}"));
                g(format!("REVOKE SELECT ON {t} FROM {role}"));
            }
            _ => {
                for c in s.cols() {
                    g(format!("GRANT SELECT ({c}) ON {t} TO {role}"));
                }
            }
        }
    }
    if with_view {
        match s {
            Gs::Table => g(format!("GRANT SELECT ON v_pub TO {role}")),
            Gs::TableThenRevoked => {
                g(format!("GRANT SELECT ON v_pub TO {role}"));
                g(format!("REVOKE SELECT ON v_pub FROM {role}"));
            }
            _ => {}
        }
    }
}

/// 世界。`flood` は他テナントが同名の表・同じ id と `pub_c` の行を持つか、`foreign_grant` は他テナントの
/// 同名ロールへ同名表の表単位の付与があるか。ロールは全世界で同じ順に作り、ID を揃える。
fn world(label: &str, viewer_idx: usize, flood: bool, foreign_grant: bool) -> World {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "ct",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("pub_c", ColumnType::Text, false),
                ColumnDef::new("secret", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::BigInt, false),
            ],
        ))
        .expect("ct");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let tenant = TENANTS[viewer_idx];
    let foreign = TENANTS[(viewer_idx + 1) % 3];
    for t in [tenant, foreign] {
        setup(&core, t, "CREATE ROLE adm");
        for p in ["r", "w"] {
            for s in STATES {
                setup(&core, t, &format!("CREATE ROLE {p}{}", s.idx()));
            }
        }
    }
    seed_tables(&core, tenant, true);
    for s in STATES {
        apply_grants(&core, tenant, &format!("r{}", s.idx()), s, true);
        apply_grants(&core, tenant, &format!("w{}", s.idx()), s, true);
        for t in ["ct", "ca", "cj"] {
            for op in ["INSERT", "UPDATE", "DELETE"] {
                setup(&core, tenant, &format!("GRANT {op} ON {t} TO w{}", s.idx()));
            }
        }
    }
    if flood {
        seed_tables(&core, foreign, false);
        setup(&core, foreign, "CREATE SCHEMA zz_s");
        setup(
            &core,
            foreign,
            "CREATE TABLE zz_s.ft (zz_only_foreign TEXT)",
        );
        if foreign_grant {
            for s in STATES {
                for p in ["r", "w"] {
                    for t in ["ct", "ca", "cj", "v_pub"] {
                        setup_with(
                            &core,
                            foreign,
                            FOREIGN_ADMIN,
                            &format!("GRANT SELECT ON {t} TO {p}{}", s.idx()),
                        );
                    }
                }
            }
        }
    }
    World {
        _g: guard,
        core,
        tenant,
    }
}

// ---------------------------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Entry {
    Auto,
    Txn,
    Ext,
    Declare,
    Explain,
    ExplainJson,
    CopyTo,
    PlpgsqlDo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Out {
    code: String,
    msg: String,
    /// 成功時の結果の Debug 表現（行・列メタデータを含む）。拒否時は空。
    body: String,
    /// 拒否した段（拡張クエリの Describe 以前の拒否を確かめる）。
    stage: &'static str,
}

fn denied(stage: &'static str, e: &engine::sql::allowlist::SqlSurfaceError) -> Out {
    Out {
        code: e.wire_code().to_string(),
        msg: e.client_message(),
        body: String::new(),
        stage,
    }
}

fn okay(body: String) -> Out {
    Out {
        code: "ok".into(),
        msg: String::new(),
        body,
        stage: "ok",
    }
}

fn do_lit(body: &str) -> String {
    format!("DO '{}'", body.replace('\'', "''"))
}

fn run(w: &World, entry: Entry, role: u64, sql: &str) -> Out {
    let c = ctx(w.tenant);
    let mut s = bound(w.tenant, role);
    match entry {
        Entry::Auto => match w.core.execute_sql_in_session(&c, &mut s, sql) {
            Ok(o) => okay(format!("{o:?}")),
            Err(e) => denied("exec", &e),
        },
        Entry::Txn | Entry::Declare | Entry::Explain | Entry::ExplainJson | Entry::PlpgsqlDo => {
            let stmt = match entry {
                Entry::Txn => sql.to_string(),
                Entry::Declare => {
                    let lim = if sql.contains("LIMIT") {
                        ""
                    } else {
                        " LIMIT 100"
                    };
                    format!("DECLARE c CURSOR FOR {sql}{lim}")
                }
                Entry::Explain => format!("EXPLAIN {sql}"),
                Entry::ExplainJson => format!("EXPLAIN (FORMAT JSON) {sql}"),
                _ => do_lit(&format!(
                    "DECLARE r record; BEGIN FOR r IN {sql} LOOP NULL; END LOOP; END"
                )),
            };
            let mut txn = w.core.new_session_transaction();
            let mut result = okay(String::new());
            let mut steps = vec!["BEGIN".to_string(), stmt];
            if entry == Entry::Declare {
                steps.push("FETCH 10 FROM c".to_string());
            }
            steps.push("ROLLBACK".to_string());
            let mut body = String::new();
            for (i, step) in steps.iter().enumerate() {
                match w.core.execute_sql_in_txn(&c, &mut s, &mut txn, step) {
                    Ok(o) => body.push_str(&format!("{o:?}|")),
                    Err(e) => {
                        if i == 1 || (entry == Entry::Declare && i == 2) {
                            result = denied("txn", &e);
                            // 失敗した Tx は ROLLBACK で閉じる。
                            let _ = w.core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK");
                            return result;
                        }
                        panic!("unexpected txn failure at step {i}: {}", e.wire_code());
                    }
                }
            }
            result.body = body;
            result
        }
        Entry::Ext => {
            let p = match w
                .core
                .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            {
                Ok(p) => p,
                Err(e) => return denied("parse", &e),
            };
            let desc = match w.core.describe_prepared_in_session(&s, &p) {
                Ok(d) => format!("{d:?}"),
                Err(e) => return denied("describe", &e),
            };
            let b = match w.core.bind_prepared_in_session(&s, &p, &[]) {
                Ok(b) => b,
                Err(e) => return denied("bind", &e),
            };
            match w.core.execute_parsed_in_session(&c, &mut s, &b) {
                Ok(o) => okay(format!("{desc}|{o:?}")),
                Err(e) => denied("execute", &e),
            }
        }
        Entry::CopyTo => match w.core.begin_copy(&c, &s, sql) {
            Ok(CopyPlan::To(_, r, _)) => okay(format!("{r:?}")),
            Ok(CopyPlan::From(_)) => panic!("expected TO plan"),
            Err(e) => denied("copy", &e),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// 文の形
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// 名指しした列（`reads`）だけを要求する。
    Named,
    /// 全列を要求する。
    AllCols,
    /// 「いずれかの列」を要求する。
    AnyCol,
    /// ビュー自身の表単位の付与だけで判定する。
    ViewOnly,
    /// 存在しない列（付与が 1 つも無ければ `42501`、あれば `42703`）。
    Nosuch,
    /// 他テナントだけにある SCHEMA／どこにも無い SCHEMA（閲覧テナントには区別が付かない）。
    Schema,
    /// 読む列が無い（書き込み権限は表単位で常に持つ）。
    NoRead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// 読み取りの SELECT。全入口に適用する。
    Select,
    /// 書き込み文。Txn 入口でだけ実行する（`ROLLBACK` で状態を戻す）。
    Write,
    /// `COPY ... TO STDOUT`。
    Copy,
}

struct Shape {
    name: &'static str,
    /// 軸（列を置く位置）。
    axis: &'static str,
    kind: Kind,
    /// `{t}` は対象表。
    sql: &'static str,
    class: Class,
    /// 名指しで読む列（`Class::Named`）。
    reads: &'static [&'static str],
    /// 疑似列 `id` を参照する（従来方式の表では表単位でしか満たせない）。
    pseudo: bool,
    /// 結合先 `cj` も全列を要求するか。
    with_cj: bool,
    /// 特定の表だけに適用する（`None` は `ct`・`ca` の両方）。
    only: Option<&'static str>,
    /// PG の意味で実際に読む列（`t` 側・`cj` 側）。fail-open 検出に使う。
    pg_t: &'static [&'static str],
    pg_j: &'static [&'static str],
}

#[allow(clippy::too_many_arguments)]
const fn sh(
    name: &'static str,
    axis: &'static str,
    kind: Kind,
    sql: &'static str,
    class: Class,
    reads: &'static [&'static str],
) -> Shape {
    Shape {
        name,
        axis,
        kind,
        sql,
        class,
        reads,
        pseudo: false,
        with_cj: false,
        only: None,
        pg_t: reads,
        pg_j: &[],
    }
}

const INS_CT: &str = "(id, embedding, pub_c, secret, n) VALUES (900, '[0.5, 0.5]', 'p', 's', 99)";
const INS_CA: &str = "(embedding, pub_c, secret, n) VALUES ('[0.5, 0.5]', 'p', 's', 99)";

fn shapes() -> Vec<Shape> {
    use Class::*;
    use Kind::*;
    let mut v = vec![
        sh(
            "proj_secret",
            "projection",
            Select,
            "SELECT secret FROM {t}",
            Named,
            &["secret"],
        ),
        sh(
            "proj_pub",
            "projection",
            Select,
            "SELECT pub_c FROM {t}",
            Named,
            &["pub_c"],
        ),
        sh(
            "proj_pub_n",
            "projection",
            Select,
            "SELECT pub_c, n FROM {t}",
            Named,
            &["pub_c", "n"],
        ),
        sh(
            "proj_mix",
            "projection",
            Select,
            "SELECT pub_c, secret FROM {t}",
            Named,
            &["pub_c", "secret"],
        ),
        sh(
            "where_secret",
            "where",
            Select,
            "SELECT pub_c FROM {t} WHERE secret = 'zzx'",
            Named,
            &["pub_c", "secret"],
        ),
        sh(
            "where_pub",
            "where",
            Select,
            "SELECT pub_c FROM {t} WHERE pub_c = 'pubtok_1'",
            Named,
            &["pub_c"],
        ),
        sh(
            "where_n",
            "where",
            Select,
            "SELECT pub_c FROM {t} WHERE n = 1",
            Named,
            &["pub_c", "n"],
        ),
        sh(
            "order_secret",
            "order_by",
            Select,
            "SELECT pub_c FROM {t} ORDER BY secret",
            Named,
            &["pub_c", "secret"],
        ),
        sh(
            "order_pub",
            "order_by",
            Select,
            "SELECT pub_c FROM {t} ORDER BY pub_c",
            Named,
            &["pub_c"],
        ),
        sh(
            "group_secret",
            "group_by",
            Select,
            "SELECT secret, count(*) FROM {t} GROUP BY secret",
            Named,
            &["secret"],
        ),
        sh(
            "group_pub",
            "group_by",
            Select,
            "SELECT pub_c, count(*) FROM {t} GROUP BY pub_c",
            Named,
            &["pub_c"],
        ),
        sh(
            "having_secret",
            "having",
            Select,
            "SELECT pub_c FROM {t} GROUP BY pub_c HAVING max(secret) > 'a'",
            Named,
            &["pub_c", "secret"],
        ),
        sh(
            "agg_secret",
            "aggregate_arg",
            Select,
            "SELECT count(secret) FROM {t}",
            Named,
            &["secret"],
        ),
        sh(
            "agg_n",
            "aggregate_arg",
            Select,
            "SELECT max(n) FROM {t}",
            Named,
            &["n"],
        ),
        sh(
            "window_secret",
            "window",
            Select,
            "SELECT row_number() OVER (ORDER BY secret) FROM {t}",
            Named,
            &["secret"],
        ),
        sh(
            "window_n",
            "window",
            Select,
            "SELECT row_number() OVER (ORDER BY n) FROM {t}",
            Named,
            &["n"],
        ),
        sh(
            "knn",
            "knn",
            Select,
            "SELECT pub_c FROM {t} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3",
            Named,
            &["pub_c", "embedding"],
        ),
        sh("star", "star", Select, "SELECT * FROM {t}", AllCols, &[]),
        sh(
            "table_star",
            "star",
            Select,
            "SELECT {t}.* FROM {t}",
            AllCols,
            &[],
        ),
        sh(
            "count_star",
            "columnless",
            Select,
            "SELECT count(*) FROM {t}",
            AnyCol,
            &[],
        ),
        sh(
            "select_one",
            "columnless",
            Select,
            "SELECT 1 FROM {t}",
            AnyCol,
            &[],
        ),
        sh(
            "nosuch",
            "missing_column",
            Select,
            "SELECT nosuch FROM {t}",
            Nosuch,
            &[],
        ),
        sh(
            "nosuch_foreign",
            "missing_column",
            Select,
            "SELECT zz_only_foreign FROM {t}",
            Nosuch,
            &[],
        ),
        sh(
            "schema_foreign",
            "missing_schema",
            Select,
            "SELECT zz_only_foreign FROM zz_s.ft",
            Schema,
            &[],
        ),
        sh(
            "schema_missing",
            "missing_schema",
            Select,
            "SELECT zz_only_foreign FROM zz_m.ft",
            Schema,
            &[],
        ),
        sh(
            "view_pub",
            "view",
            Select,
            "SELECT pub_c FROM v_pub",
            ViewOnly,
            &[],
        ),
        sh(
            "w_update_where_n",
            "write_read_side",
            Write,
            "UPDATE {t} SET pub_c = 'x' WHERE n = 99",
            Named,
            &["n"],
        ),
        sh(
            "w_update_where_secret",
            "write_read_side",
            Write,
            "UPDATE {t} SET pub_c = 'x' WHERE secret = 'zzx'",
            Named,
            &["secret"],
        ),
        sh(
            "w_update_rhs_n",
            "write_read_side",
            Write,
            "UPDATE {t} SET n = n + 1 WHERE n = 99",
            Named,
            &["n"],
        ),
        sh(
            "w_update_returning_secret",
            "write_returning",
            Write,
            "UPDATE {t} SET pub_c = 'x' WHERE n = 99 RETURNING secret",
            Named,
            &["n", "secret"],
        ),
        sh(
            "w_delete_where_n",
            "write_read_side",
            Write,
            "DELETE FROM {t} WHERE n = 99",
            Named,
            &["n"],
        ),
        sh(
            "w_delete_returning_secret",
            "write_returning",
            Write,
            "DELETE FROM {t} WHERE n = 99 RETURNING secret",
            Named,
            &["n", "secret"],
        ),
        sh(
            "w_insert_plain",
            "write_no_read",
            Write,
            "INSERT INTO {t} {ins}",
            NoRead,
            &[],
        ),
        sh(
            "w_insert_returning_pub",
            "write_returning",
            Write,
            "INSERT INTO {t} {ins} RETURNING pub_c",
            Named,
            &["pub_c"],
        ),
        sh(
            "w_insert_returning_secret",
            "write_returning",
            Write,
            "INSERT INTO {t} {ins} RETURNING secret",
            Named,
            &["secret"],
        ),
        sh(
            "w_insert_returning_star",
            "write_returning",
            Write,
            "INSERT INTO {t} {ins} RETURNING *",
            AllCols,
            &[],
        ),
        sh(
            "c_copy_pub",
            "copy",
            Copy,
            "COPY {t} (pub_c) TO STDOUT",
            Named,
            &["pub_c"],
        ),
        sh(
            "c_copy_secret",
            "copy",
            Copy,
            "COPY {t} (secret) TO STDOUT",
            Named,
            &["secret"],
        ),
        sh(
            "c_copy_mix",
            "copy",
            Copy,
            "COPY {t} (pub_c, n) TO STDOUT",
            Named,
            &["pub_c", "n"],
        ),
        sh(
            "c_copy_all",
            "copy",
            Copy,
            "COPY {t} TO STDOUT",
            AllCols,
            &[],
        ),
    ];
    // 疑似列 `id`（従来方式の表だけ）。
    let mut id_shape = sh(
        "pseudo_id",
        "pseudo_id",
        Select,
        "SELECT id FROM {t}",
        Named,
        &[],
    );
    id_shape.pseudo = true;
    id_shape.only = Some("ct");
    v.push(id_shape);
    let mut id_where = sh(
        "pseudo_id_where",
        "pseudo_id",
        Select,
        "SELECT pub_c FROM {t} WHERE id = 1",
        Named,
        &["pub_c"],
    );
    id_where.pseudo = true;
    id_where.only = Some("ct");
    v.push(id_where);
    // 全列を要求する形（結合先 `cj` も全列）。
    let mut joins = vec![
        sh(
            "join",
            "join",
            Select,
            "SELECT a.pub_c FROM {t} a JOIN cj b ON a.n = b.n",
            AllCols,
            &[],
        ),
        sh(
            "subquery_in",
            "subquery",
            Select,
            "SELECT pub_c FROM {t} WHERE n IN (SELECT n FROM cj)",
            AllCols,
            &[],
        ),
        sh(
            "subquery_exists",
            "subquery",
            Select,
            "SELECT pub_c FROM {t} WHERE EXISTS (SELECT 1 FROM cj WHERE cj.n = {t}.n)",
            AllCols,
            &[],
        ),
        sh(
            "subquery_scalar",
            "subquery",
            Select,
            "SELECT pub_c FROM {t} WHERE n = (SELECT max(n) FROM cj)",
            AllCols,
            &[],
        ),
        sh(
            "setop_union",
            "set_operation",
            Select,
            "SELECT pub_c FROM {t} UNION SELECT pub_c FROM cj",
            AllCols,
            &[],
        ),
    ];
    for j in joins.iter_mut() {
        j.with_cj = true;
    }
    v.extend(joins);
    // 単一 relation の CTE・派生表は、内側が名指しする列だけを要求する（実装の精密化の範囲。ADR §9）。
    for (name, axis, sql, col) in [
        (
            "cte_pub",
            "cte",
            "WITH w AS (SELECT pub_c FROM {t}) SELECT pub_c FROM w",
            "pub_c",
        ),
        (
            "cte_secret",
            "cte",
            "WITH w AS (SELECT secret FROM {t}) SELECT secret FROM w",
            "secret",
        ),
        (
            "derived_pub",
            "derived_table",
            "SELECT d.pub_c FROM (SELECT pub_c FROM {t}) d",
            "pub_c",
        ),
        (
            "derived_secret",
            "derived_table",
            "SELECT d.secret FROM (SELECT secret FROM {t}) d",
            "secret",
        ),
    ] {
        let reads: &'static [&'static str] = if col == "pub_c" {
            &["pub_c"]
        } else {
            &["secret"]
        };
        v.push(sh(name, axis, Select, sql, Named, reads));
    }
    // 従来方式の表だけの `ON CONFLICT DO UPDATE`。
    let mut oc = sh(
        "w_on_conflict_update",
        "write_on_conflict",
        Write,
        "INSERT INTO {t} (id, embedding, pub_c, secret, n) VALUES (1, '[0.5, 0.5]', 'p', 's', 99) ON CONFLICT (id) DO UPDATE SET pub_c = 'x'",
        AllCols,
        &[],
    );
    oc.only = Some("ct");
    v.push(oc);
    // PG の意味で実際に読む列を宣言する（fail-open 検出用オラクル。production の期待 `expected` とは独立）。
    // `*`・`COPY ... TO STDOUT`・`RETURNING *` は全実列、結合・副問い合わせ・集合演算は両 relation の参照列。
    for s in v.iter_mut() {
        let (t, j): (&'static [&'static str], &'static [&'static str]) = match s.name {
            "join" | "subquery_in" | "subquery_exists" | "subquery_scalar" => {
                (&["pub_c", "n"], &["n"])
            }
            "setop_union" => (&["pub_c"], &["pub_c"]),
            _ if s.class == Class::AllCols => (&REAL_COLS, &[]),
            _ => continue,
        };
        s.pg_t = t;
        s.pg_j = j;
    }
    v
}

fn expand(shape: &Shape, t: &str) -> String {
    let ins = if t == "ct" { INS_CT } else { INS_CA };
    shape.sql.replace("{t}", t).replace("{ins}", ins)
}

fn base_entries(shape: &Shape) -> &'static [Entry] {
    match shape.kind {
        Kind::Select if matches!(shape.class, Class::Nosuch | Class::Schema) => {
            &[Entry::Auto, Entry::Txn, Entry::Ext]
        }
        Kind::Select => &[
            Entry::Auto,
            Entry::Txn,
            Entry::Ext,
            Entry::Declare,
            Entry::Explain,
            Entry::ExplainJson,
            Entry::PlpgsqlDo,
        ],
        Kind::Write => &[Entry::Txn],
        Kind::Copy => &[Entry::CopyTo],
    }
}

/// 現行の構文表層が受け付けない（T0 の陽性対照でも `42601` になる）形と入口の組。拒否側が空振りしない
/// ように、受理されない組は行列から外す（権限の検査とは無関係の未対応）。
fn syntax_unsupported(shape: &Shape, entry: Entry) -> bool {
    let n = shape.name;
    match entry {
        // `DECLARE ... CURSOR FOR` は単純な SELECT（集約・結合・集合演算・副問い合わせ・距離順を除く）だけ。
        Entry::Declare => {
            n.starts_with("agg_")
                || n.starts_with("subquery_")
                || n.starts_with("derived_")
                || matches!(n, "count_star" | "join" | "knn" | "setop_union")
        }
        // PL/pgSQL の `FOR ... IN <query>` は派生表を受け付けない。
        Entry::PlpgsqlDo => n.starts_with("derived_"),
        // 拡張クエリ（Parse）は副問い合わせを受け付けない。
        Entry::Ext => n.starts_with("subquery_"),
        _ => false,
    }
}

fn entries_for(shape: &Shape) -> Vec<Entry> {
    base_entries(shape)
        .iter()
        .copied()
        .filter(|e| !syntax_unsupported(shape, *e))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// 独立オラクル（production の判定関数は一切呼ばない）
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    Ok,
    Code(&'static str),
}

fn covers_all(table: &str, g: Gs) -> bool {
    if g.table_level() {
        return true;
    }
    // 従来方式の表は疑似列 `id` を含むため表単位でしか満たせない。採番方式は全実列への列単位で満たせる。
    table != "ct" && REAL_COLS.iter().all(|c| g.cols().contains(c))
}

fn covers_named(table: &str, g: Gs, reads: &[&str], pseudo: bool) -> bool {
    let _ = table;
    if g.table_level() {
        return true;
    }
    !pseudo && reads.iter().all(|c| g.cols().contains(c))
}

fn expected(shape: &Shape, table: &str, entry: Entry, g: Gs) -> Expect {
    let mut class = shape.class;
    // EXPLAIN・DECLARE は全列を要求する（ADR §8）。
    if matches!(entry, Entry::Declare | Entry::Explain | Entry::ExplainJson)
        && matches!(class, Class::Named | Class::AnyCol)
    {
        class = Class::AllCols;
    }
    let ok = match class {
        Class::Nosuch => {
            return if g.any_grant() {
                Expect::Code("42703")
            } else {
                Expect::Code("42501")
            };
        }
        Class::Schema => {
            // SCHEMA の名前解決は権限ゲートより前にあり、付与の有無で応答は変わらない。
            return Expect::Code("3F000");
        }
        Class::NoRead => true,
        Class::ViewOnly => g.table_level(),
        Class::AnyCol => g.any_grant(),
        Class::Named => covers_named(table, g, shape.reads, shape.pseudo),
        Class::AllCols => covers_all(table, g) && (!shape.with_cj || covers_all("cj", g)),
    };
    if ok {
        Expect::Ok
    } else {
        Expect::Code("42501")
    }
}

/// PostgreSQL の意味での成功可否（読む列が全て付与済み、または表単位の付与）。production が成功した
/// セルでこれが偽なら fail-open。
fn pg_allows(shape: &Shape, g: Gs) -> bool {
    match shape.class {
        Class::Nosuch | Class::Schema => false,
        Class::NoRead => true,
        Class::ViewOnly => g.table_level(),
        _ => {
            if g.table_level() {
                return true;
            }
            let t_ok = if shape.class == Class::AnyCol {
                g.any_grant()
            } else {
                !shape.pseudo && shape.pg_t.iter().all(|c| g.cols().contains(c))
            };
            t_ok && shape.pg_j.iter().all(|c| g.cols().contains(c))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 検出器（T6 で負の対照を与える）
// ---------------------------------------------------------------------------------------------

/// 拒否文言の違反（列名・表名・ロール名・テナント名・`column` の語を含む）を返す。
fn msg_violation(msg: &str) -> Option<String> {
    const BAD: &[&str] = &[
        "pub_c",
        "secret",
        "embedding",
        "n",
        "id",
        "ct",
        "ca",
        "cj",
        "v_pub",
        "column",
        "tenant",
        "zz_only_foreign",
        "nosuch",
    ];
    for tok in msg.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
        let l = tok.to_ascii_lowercase();
        if BAD.contains(&l.as_str())
            || l.starts_with("tenant-")
            || (l.len() == 2
                && (l.starts_with('r') || l.starts_with('w'))
                && l[1..].parse::<u8>().is_ok())
        {
            return Some(format!("forbidden token {tok:?}"));
        }
    }
    None
}

/// 成功セルの出力に現れてはならないトークン（他テナントの秘密と、許可外の閲覧テナントの `secret`）。
fn body_leak(body: &str, viewer: &str, foreign: &str, secret_granted: bool) -> Option<String> {
    if body.contains(&format!("zzs_{foreign}_")) {
        return Some("foreign tenant token".into());
    }
    if body.contains("zz_only_foreign") {
        return Some("foreign-only column name".into());
    }
    if !secret_granted && body.contains(&format!("zzs_{viewer}_")) {
        return Some("ungranted secret token".into());
    }
    None
}

/// 1 セルの判定。違反があれば説明（位置と種別だけ。秘密トークンは出さない）を返す。
fn check_cell(
    shape: &Shape,
    table: &str,
    entry: Entry,
    g: Gs,
    out: &Out,
    viewer: &str,
    foreign: &str,
) -> Result<(), String> {
    let want = expected(shape, table, entry, g);
    let got = if out.code == "ok" {
        Expect::Ok
    } else {
        match out.code.as_str() {
            "42501" => Expect::Code("42501"),
            "42703" => Expect::Code("42703"),
            "3F000" => Expect::Code("3F000"),
            _ => return Err(format!("unexpected code {}", out.code)),
        }
    };
    if got != want {
        return Err(format!("oracle mismatch: want {want:?} got {got:?}"));
    }
    if out.code == "ok" {
        if !pg_allows(shape, g)
            && !matches!(entry, Entry::Declare | Entry::Explain | Entry::ExplainJson)
        {
            return Err("fail-open versus PG oracle".into());
        }
        let secret_granted = g.table_level() || g.cols().contains(&"secret");
        if let Some(l) = body_leak(&out.body, viewer, foreign, secret_granted) {
            return Err(format!("leak: {l}"));
        }
    } else if out.code == "42501" {
        if let Some(v) = msg_violation(&out.msg) {
            return Err(format!("message: {v}"));
        }
    }
    if entry == Entry::Ext && out.code == "42501" && matches!(out.stage, "bind" | "execute") {
        return Err("denied only after Describe".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 行列の実行
// ---------------------------------------------------------------------------------------------

type Key = (Gs, &'static str, &'static str, Entry);

/// 世界 `w` で行列全体を実行し、`(状態, 形, 表, 入口) -> 応答` を返す。
fn run_matrix(w: &World) -> BTreeMap<Key, Out> {
    let mut m = BTreeMap::new();
    for g in STATES {
        for shape in shapes() {
            let tables: &[&'static str] = match shape.only {
                Some("ct") => &["ct"],
                Some(_) => &["ca"],
                None if shape.name == "view_pub" || shape.name.starts_with("schema_") => &["ca"],
                None => &["ct", "ca"],
            };
            for &t in tables {
                for e in entries_for(&shape) {
                    let role = if shape.kind == Kind::Write {
                        write_role(g)
                    } else {
                        read_role(g)
                    };
                    let out = run(w, e, role, &expand(&shape, t));
                    m.insert((g, shape.name, t, e), out);
                }
            }
        }
    }
    m
}

fn shape_by_name(all: &[Shape], name: &str) -> usize {
    all.iter().position(|s| s.name == name).expect("shape")
}

#[test]
fn column_privilege_cross_path_matrix() {
    let all = shapes();
    let mut attempts = 0usize;
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    let mut denial_msgs: BTreeMap<String, usize> = BTreeMap::new();
    let mut axis_counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for viewer_idx in 0..3 {
        let foreign = TENANTS[(viewer_idx + 1) % 3];
        let base = world("colpriv-matrix-base", viewer_idx, false, false);
        let flood = world("colpriv-matrix-flood", viewer_idx, true, false);
        let fgrant = world("colpriv-matrix-fgrant", viewer_idx, true, true);
        let m_base = run_matrix(&base);
        let m_flood = run_matrix(&flood);
        let m_fgrant = run_matrix(&fgrant);
        assert_eq!(m_base.len(), m_flood.len());
        for (key, out) in &m_base {
            attempts += 3;
            let (g, name, t, e) = *key;
            let shape = &all[shape_by_name(&all, name)];
            // T2(b)(c): 他テナントの行・同名表・付与は閲覧テナントの応答を変えない。
            let o_flood = &m_flood[key];
            let o_fgrant = &m_fgrant[key];
            for (label, o) in [("flood", o_flood), ("foreign-grant", o_fgrant)] {
                // 欠落列名の語だけは文言に含まれうるため、成功・拒否の判定と他の語の一致を見る。
                let same = if out.code == "42703" || out.code == "3F000" {
                    o.code == out.code && o.stage == out.stage
                } else {
                    o == out
                };
                assert!(
                    same,
                    "tenant {viewer_idx}: {label} world differs at {g:?}/{name}/{t}/{e:?}"
                );
            }
            for o in [out, o_flood, o_fgrant] {
                if let Err(why) = check_cell(shape, t, e, g, o, TENANTS[viewer_idx], foreign) {
                    failures.insert(
                        format!("{name}/{t}/{e:?}: {why}"),
                        format!("{g:?} {}", o.code),
                    );
                }
            }
            if out.code == "42501" {
                *denial_msgs.entry(out.msg.clone()).or_default() += 1;
            }
            for label in [shape.axis.to_string(), format!("entry:{e:?}")] {
                let c = axis_counts.entry(label).or_default();
                // T0: 表単位の付与の陽性対照は、`Nosuch`・`Schema` 以外の全セルで成功する。
                if g == Gs::Table
                    && !matches!(shape.class, Class::Nosuch | Class::Schema)
                    && out.code != "ok"
                {
                    failures.insert(
                        format!("T0 positive control {name}/{t}/{e:?}"),
                        out.code.clone(),
                    );
                }
                match out.code.as_str() {
                    "ok" => c.0 += 1,
                    "42501" => c.1 += 1,
                    _ => {}
                }
            }
        }
        // T2(d): 別の列への付与を足しても、期待が同じセルの応答は変わらない。
        for (key, out) in &m_base {
            let (g, name, t, e) = *key;
            if !matches!(g, Gs::PubCol | Gs::PubEmb | Gs::PubN) {
                continue;
            }
            let shape = &all[shape_by_name(&all, name)];
            let want = expected(shape, t, e, g);
            for other in [Gs::PubCol, Gs::PubEmb, Gs::PubN] {
                if other != g && expected(shape, t, e, other) == want && out.code != "42703" {
                    assert_eq!(
                        out,
                        &m_base[&(other, name, t, e)],
                        "extra unrelated grant changed the response at {g:?}/{other:?}/{name}/{t}/{e:?}"
                    );
                }
            }
        }
        // T3: 他テナントだけにある SCHEMA とどこにも無い SCHEMA も、付与状態・入口ごとに同じ応答。
        for g in STATES {
            for e in [Entry::Auto, Entry::Txn, Entry::Ext] {
                let a = &m_fgrant[&(g, "schema_foreign", "ca", e)];
                let b = &m_fgrant[&(g, "schema_missing", "ca", e)];
                assert_eq!(
                    a.code, b.code,
                    "foreign vs missing schema code at {g:?}/{e:?}"
                );
                assert_eq!(a.stage, b.stage);
                let norm = |s: &str| s.replace("zz_s", "X").replace("zz_m", "X");
                assert_eq!(norm(&a.msg), norm(&b.msg));
            }
        }
        // T3: 他テナントだけにある列名とどこにも無い列名は、付与状態・入口ごとに同じ応答。
        for g in STATES {
            for t in ["ct", "ca"] {
                for e in [Entry::Auto, Entry::Txn, Entry::Ext] {
                    let a = &m_fgrant[&(g, "nosuch", t, e)];
                    let b = &m_fgrant[&(g, "nosuch_foreign", t, e)];
                    assert_eq!(
                        a.code, b.code,
                        "missing vs foreign-only column code at {g:?}/{t}/{e:?}"
                    );
                    assert_eq!(a.stage, b.stage);
                    let norm = |s: &str| s.replace("zz_only_foreign", "X").replace("nosuch", "X");
                    assert_eq!(norm(&a.msg), norm(&b.msg));
                    if a.code == "42703" {
                        for cand in ["pub_c", "secret", "embedding"] {
                            assert!(!a.msg.contains(cand), "candidate column suggested");
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "cell failures: {failures:#?}");
    // T2(a): 拒否文言は、不許可の列の種類・数・位置・入口によらず全セルでバイト一致する。
    assert_eq!(
        denial_msgs.len(),
        1,
        "42501 client_message must be one fixed text, got {} variants",
        denial_msgs.len()
    );
    // T0: 軸（位置・入口）ごとに成功と `42501` が 1 件以上ある。
    for (axis, (ok, deny)) in &axis_counts {
        if matches!(
            axis.as_str(),
            "missing_column" | "missing_schema" | "write_no_read"
        ) {
            continue;
        }
        assert!(*ok >= 1, "axis {axis} has no successful cell");
        assert!(*deny >= 1, "axis {axis} has no denied cell");
    }
    // 試行総数の固定（空回り防止）。
    assert_eq!(attempts, EXPECTED_ATTEMPTS, "matrix size drifted");
}

/// 試行総数（閲覧テナント 3 × 世界 3 × 付与状態 7 × 適用セル）。形・入口を増減したら更新する。
const EXPECTED_ATTEMPTS: usize = 28728;

// ---------------------------------------------------------------------------------------------
// T6 負の対照
// ---------------------------------------------------------------------------------------------

#[test]
fn detectors_flag_fabricated_violations() {
    // 漏えい検出器。
    assert!(body_leak("row zzs_tenant-b_ca_1", "tenant-a", "tenant-b", true).is_some());
    assert!(body_leak("row zzs_tenant-a_ca_1", "tenant-a", "tenant-b", false).is_some());
    assert!(body_leak("row zzs_tenant-a_ca_1", "tenant-a", "tenant-b", true).is_none());
    assert!(body_leak("zz_only_foreign", "tenant-a", "tenant-b", true).is_some());
    // 文言検出器。
    for bad in [
        "permission denied for column secret",
        "permission denied for table ca",
        "role r3 lacks privilege",
        "tenant-b denied",
        "column list rejected",
    ] {
        assert!(msg_violation(bad).is_some(), "{bad}");
    }
    assert!(msg_violation("permission denied").is_none());
    // オラクル照合・PG オラクル。
    let all = shapes();
    let star = &all[shape_by_name(&all, "star")];
    let fake_ok = okay(String::new());
    assert!(check_cell(
        star,
        "ca",
        Entry::Auto,
        Gs::PubCol,
        &fake_ok,
        "tenant-a",
        "tenant-b"
    )
    .is_err());
    let fake_deny = Out {
        code: "42501".into(),
        msg: "permission denied".into(),
        body: String::new(),
        stage: "exec",
    };
    assert!(check_cell(
        star,
        "ca",
        Entry::Auto,
        Gs::Table,
        &fake_deny,
        "tenant-a",
        "tenant-b"
    )
    .is_err());
    assert!(check_cell(
        star,
        "ca",
        Entry::Auto,
        Gs::PubCol,
        &fake_deny,
        "tenant-a",
        "tenant-b"
    )
    .is_ok());
    let leaky = okay("zzs_tenant-b_ca_2".into());
    assert!(check_cell(
        star,
        "ca",
        Entry::Auto,
        Gs::Table,
        &leaky,
        "tenant-a",
        "tenant-b"
    )
    .is_err());
    let proj = &all[shape_by_name(&all, "proj_secret")];
    assert!(!pg_allows(proj, Gs::PubCol));
    assert!(pg_allows(proj, Gs::AllReal));
    // PG オラクルは各文が実際に読む列で判定する（部分付与は全列の形・結合の両 relation で拒否側）。
    assert!(!pg_allows(star, Gs::PubCol));
    assert!(pg_allows(star, Gs::AllReal));
    let join = &all[shape_by_name(&all, "join")];
    assert!(!pg_allows(join, Gs::PubCol));
    assert!(pg_allows(join, Gs::AllReal));
    let setop = &all[shape_by_name(&all, "setop_union")];
    assert!(pg_allows(setop, Gs::PubCol));
    assert!(!pg_allows(setop, Gs::None));
    assert!(pg_allows(join, Gs::PubN));
    let copy_all = &all[shape_by_name(&all, "c_copy_all")];
    assert!(!pg_allows(copy_all, Gs::PubCol));
    let ret_star = &all[shape_by_name(&all, "w_insert_returning_star")];
    assert!(!pg_allows(ret_star, Gs::PubCol));
    // PubCol のみで成功した全列の形は fail-open として検出する。
    assert!(check_cell(
        star,
        "ca",
        Entry::Auto,
        Gs::PubCol,
        &okay(String::new()),
        "tenant-a",
        "tenant-b"
    )
    .is_err());
    // 拡張クエリの拒否が Describe より後（execute 段）になる応答は違反。
    let late = Out {
        code: "42501".into(),
        msg: "permission denied".into(),
        body: String::new(),
        stage: "execute",
    };
    assert!(check_cell(
        star,
        "ca",
        Entry::Ext,
        Gs::PubCol,
        &late,
        "tenant-a",
        "tenant-b"
    )
    .is_err());
    // Describe が成功し Bind で初めて拒否される応答も違反。
    let bind_late = Out {
        stage: "bind",
        ..late.clone()
    };
    assert!(check_cell(
        star,
        "ca",
        Entry::Ext,
        Gs::PubCol,
        &bind_late,
        "tenant-a",
        "tenant-b"
    )
    .is_err());
    // Describe 以前（describe 段）の拒否は正常。
    let early = Out {
        stage: "describe",
        ..late
    };
    assert!(check_cell(
        star,
        "ca",
        Entry::Ext,
        Gs::PubCol,
        &early,
        "tenant-a",
        "tenant-b"
    )
    .is_ok());
}
