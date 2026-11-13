//! `<=>` の並列化（Issue #3216。ポインタ: SQL-33・CORE-3・TABLE-26）の SQL 経由の差分テスト。
//!
//! 単一スレッドの参照 provider（`CpuScalarProvider`）と既定エンジンの
//! `ParallelSearchProvider` に同じデータを投入した 2 つの `EngineCore` で、`<=>` の各形
//! （LIMIT k・スカラー条件〔候補マスク〕・`USING MODE 'precision'`・HYBRID の密側・空集合）が
//! id 列・セルの型と値・距離のビットまで一致することを固定する。他テナントの行（クエリと
//! 完全一致する最良行を含む）が tenant-a の結果へ現れないことも併せて検証する。
//!
//! データ: 可視 5000 行（並列経路に入る 2 × 1024 超）。NULL 行・ゼロノルム行・f32 オーバーフロー行・
//! 重複ベクトル（同点）を混ぜる。ポート・環境変数・時間計測には依存しない。

use engine::core::EngineCore;
use engine::kernel::{CpuScalarProvider, SearchProvider};
use engine::parallel_search::{expected_thread_count, ParallelSearchProvider};
use engine::policy::PolicyContext;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 8;
const ROWS: u64 = 5000;
const SECRET: &str = "tenant-b-secret";
const Q: &str = "[0.5,-0.25,1,0.125,-0.75,0.3,0.9,-0.1]";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match exec(core, tenant, sql) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

/// 決定的な xorshift による成分（`[-1, 1]`）。
fn vector_literal(state: &mut u32) -> String {
    let comps: Vec<String> = (0..DIM)
        .map(|_| {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            format!("{}", (*state % 2001) as f32 / 1000.0 - 1.0)
        })
        .collect();
    format!("[{}]", comps.join(","))
}

fn seed(core: &EngineCore, with_index: bool) {
    exec(
        core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(8), grp INTEGER, body TEXT)",
    );
    if with_index {
        exec(core, "sys", "CREATE INDEX t_grp ON t (grp)");
    }
    let mut state = 0x9E37_79B9u32;
    let dup = vector_literal(&mut state);
    let mut values: Vec<String> = Vec::new();
    for i in 1..=ROWS {
        let v = match i {
            _ if i % 50 == 7 => "NULL".to_string(),
            11 | 12 => format!("'[{}]'", ["0"; DIM].join(",")),
            13 | 14 => format!("'[{}]'", ["1e30"; DIM].join(",")),
            _ if i % 97 == 0 => format!("'{dup}'"),
            _ => format!("'{}'", vector_literal(&mut state)),
        };
        values.push(format!("({i}, {v}, {}, 'doc zebra {i}')", i % 4));
    }
    for (n, chunk) in values.chunks(60).enumerate() {
        exec(
            core,
            "tenant-a",
            &format!(
                "INSERT INTO t (id, v, grp, body) VALUES {} USING OPERATION_ID 'seed-a-{n}'",
                chunk.join(",")
            ),
        );
    }
    // 他テナントの行。うち 1 行は tenant-a のクエリにとって最良（距離 0）になる配置にする。
    let mut b: Vec<String> = vec![format!("(900001, '{Q}', 0, '{SECRET}')")];
    for i in 2..=60u64 {
        b.push(format!(
            "({}, '{}', {}, '{SECRET}')",
            900000 + i,
            vector_literal(&mut state),
            i % 4
        ));
    }
    exec(
        core,
        "tenant-b",
        &format!(
            "INSERT INTO t (id, v, grp, body) VALUES {} USING OPERATION_ID 'seed-b'",
            b.join(",")
        ),
    );
}

fn open(
    label: &str,
    provider: Box<dyn SearchProvider>,
    with_index: bool,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, provider);
    seed(&core, with_index);
    (core, guard)
}

/// 結果を比較可能な形（id・距離のビット・セルの Debug 表現・列の型）へ落とす。
fn canon(r: &QueryResult) -> (Vec<String>, Vec<(u64, u64, String)>) {
    let cols = r.columns.iter().map(|c| format!("{c:?}")).collect();
    let rows = r
        .rows
        .iter()
        .map(|x| (x.id, x.score.to_bits(), format!("{:?}", x.cells)))
        .collect();
    (cols, rows)
}

fn assert_parity(label: &str, with_index: bool) {
    let (reference, _g1) = open(
        &format!("cos-par-ref-{label}"),
        Box::new(CpuScalarProvider),
        with_index,
    );
    let (parallel, _g2) = open(
        &format!("cos-par-par-{label}"),
        Box::new(ParallelSearchProvider),
        with_index,
    );
    // 並列経路に入ったかの可視化（2 未満の環境でも等価性のアサーションは弱めない）。
    eprintln!(
        "expected_thread_count({ROWS}) = {}",
        expected_thread_count(ROWS as usize)
    );

    let proj = format!("SELECT id, body, v <=> '{Q}' AS d FROM t");
    let order = format!("ORDER BY v <=> '{Q}'");
    let sqls: Vec<String> = vec![
        format!("{proj} {order} LIMIT 10"),
        format!("{proj} {order} LIMIT 400"),
        format!("{proj} {order} LIMIT 9000"),
        format!("{proj} {order} LIMIT 10 USING MODE 'precision'"),
        format!("{proj} WHERE grp <> 0 {order} LIMIT 10"),
        format!("{proj} WHERE grp <> 0 {order} LIMIT 400"),
        format!("{proj} WHERE grp = 1 {order} LIMIT 9000"),
        format!("{proj} WHERE grp = 99 {order} LIMIT 10"),
        format!("SELECT id, body FROM t ORDER BY HYBRID(v, '{Q}', body, 'zebra') LIMIT 10"),
        format!("SELECT id, body FROM t ORDER BY HYBRID(v, '{Q}', body, 'zebra') LIMIT 100"),
    ];
    for sql in &sqls {
        let want = query(&reference, "tenant-a", sql);
        let got = query(&parallel, "tenant-a", sql);
        assert_eq!(canon(&got), canon(&want), "{sql}");
        // テナント境界: tenant-b の行（距離 0 の最良行を含む）は結果へ現れない。
        for row in &got.rows {
            assert!(
                !format!("{:?}", row.cells).contains(SECRET),
                "tenant-b row leaked for {sql}"
            );
        }
        assert!(!want.rows.is_empty() || sql.contains("grp = 99"), "{sql}");
    }
    // 空集合（可視行が 0 件のテナント）。
    for sql in [
        format!("{proj} {order} LIMIT 10"),
        format!("{proj} WHERE grp <> 0 {order} LIMIT 10"),
    ] {
        let want = query(&reference, "tenant-c", &sql);
        let got = query(&parallel, "tenant-c", &sql);
        assert!(got.rows.is_empty(), "{sql}");
        assert_eq!(canon(&got), canon(&want), "{sql}");
    }
}

// SQL-33 (1)(3): 並列版 `<=>` は単一スレッド版とビット一致する（索引なし。事後・事前フィルタ経路）。
#[test]
fn sql33_cosine_parallel_matches_single_thread_without_index() {
    assert_parity("noidx", false);
}

// SQL-33 (1)(3)・TABLE-26: スカラー索引の候補マスク（`search_cosine_subset`）経路でも一致する。
#[test]
fn sql33_cosine_parallel_matches_single_thread_with_scalar_index() {
    assert_parity("idx", true);
}
