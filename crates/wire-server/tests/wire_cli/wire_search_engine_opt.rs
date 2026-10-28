//! `--search-engine` opt-in（Issue #656）の wire 経由・実行契約検証
//! （in-process。生バイトクライアント）。
//!
//! `crates/wire-server/src/search_engine_opt.rs` が閉じた語彙 4 値
//! （`default`／`hnsw`／`hnsw_f16`／`hnsw_i8`）から
//! `engine::search_engine::SearchEngineKind` を解決する経路そのものは
//! `crates/wire-server/src/main.rs` の `#[cfg(test)]`（`resolve_search_engine`）
//! が固定する。本ファイルは、その解決結果を `EngineCore::from_storage`／
//! `from_storage_with_engine` で構築した `EngineCore` へ実際に渡したときの
//! **wire 経由の観測結果** に絞って固定する:
//!
//! - R4: 選択結果が `EXPLAIN` の `engine:`／`hnsw_params:` 行（Issue #411）と
//!   一致すること（`tests/wire_explain.rs::
//!   explain_reports_hnsw_engine_and_full_visible_ann_plan` と同型の構成を
//!   4 エンジン分へ拡張する）
//! - R5: RLS 相当のテナント境界が選択エンジンによらず不変であること
//!   （3 テナント・`Public`/`Private` 混在コーパスで、他テナントの `Private`
//!   行の非漏えい・自テナントの `Private` 行の可視（RLS-11・TASK-195・
//!   read-your-writes）・3 テナントの Public 可視結果一致・HNSW opt-in 3 種は
//!   非 vacuous な索引構築（`hnsw_index_cache_stats()`）まで固定する）
//!
//! Issue #657（フィルタ付き ANN の探索パラメータ opt-in 露出）分:
//!
//! - R4 拡張: `--hnsw-*` 探索パラメータ opt-in（`full_scan_ratio`／
//!   `acorn_max_visible_ratio`／`sparse_visited_max`）を指定しても
//!   `EXPLAIN` の `hnsw_params:` 行は `sparse_visited_max=` のみ反映し
//!   `full_scan_ratio`／`acorn_max_visible_ratio` の値・キー名は一切出力へ
//!   現れないこと（Issue #411 のテナント存在情報非露出方針の維持を機械的に
//!   固定する）
//! - R5 拡張: `--hnsw-*` チューニング指定時も RLS 境界・非 vacuous な索引構築
//!   が不変であること
//!
//! `EXPLAIN` は `USING PLAN` 形のみ受理する契約（`sql::allowlist`）のため、
//! 決定的スタブ `LlmClient` を注入する（`wire_explain.rs` と同じ構成）。
//! バイナリ子プロセス経由の CLI 引数パース・起動可否は
//! `tests/wire_search_engine_cli.rs` が担う。本ファイルは production の
//! `search_engine_opt::parse`／`to_engine_kind_with`（`main.rs::
//! resolve_search_engine` と同じ経路）を直接呼び、untrusted 入力の唯一の
//! 検証入口（`ValidatedHnswParams::new`・`with_full_scan_ratio`・
//! `with_acorn_max_visible_ratio`）を迂回しない。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine::{self, SearchEngineKind};
use engine::storage::{Storage, Visibility};
use wire_server::search_engine_opt::{self, HnswTuning};

use common::*;

struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// 検索語・ソフトヒントを持たない展開結果（`EXPLAIN` の行数を固定しやすく
/// するため。`wire_explain.rs::EXPANSION_RESPONSE_NO_HINTS` と同構成）。
const EXPANSION_RESPONSE_NO_HINTS: &str =
    r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;

/// 4 トークンそれぞれに対応する `SearchEngineKind`（`None` は既定＝
/// `EngineCore::from_storage` 経由）。production の `search_engine_opt::parse`
/// → `SearchEngineChoice::to_engine_kind_with` をそのまま呼ぶ（`main.rs::
/// resolve_search_engine` と同じ経路。untrusted 入力の唯一の検証入口を
/// 迂回しない）。`tuning` が既定（`HnswTuning::default()`）のときは
/// Issue #656 時点の `kind_for_token` とビット同一（R2）。
fn kind_for_token_with(token: &str, tuning: HnswTuning) -> Option<SearchEngineKind> {
    let choice = search_engine_opt::parse(token).expect("known token in test fixture");
    choice
        .to_engine_kind_with(tuning)
        .unwrap_or_else(|e| panic!("token={token}: valid tuning must resolve, got error: {e}"))
}

fn resident_suffix_for(token: &str) -> &'static str {
    match token {
        "default" => {
            unreachable!("EXPLAIN assertions for default use engine: parallel_brute_force instead")
        }
        "hnsw" => "resident=f32",
        "hnsw_f16" => "resident=f16",
        "hnsw_i8" => "resident=i8",
        other => panic!("unexpected token: {other:?}"),
    }
}

/// `docs(embedding VECTOR(2), path TEXT, body TEXT)` を持つ `EngineCore` を
/// `token` に応じたエンジンで構築する（`wire_explain.rs::
/// new_hnsw_core_with_docs_table` と同型）。`tuning` が既定（全 `None`）の
/// ときは [`new_core_with_docs_table`] とビット同一。
fn new_core_with_docs_table(token: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    new_core_with_docs_table_and_tuning(token, HnswTuning::default())
}

/// [`new_core_with_docs_table`] へ `--hnsw-*` 探索パラメータ opt-in
/// （Issue #657）の `tuning` を加えたもの。
fn new_core_with_docs_table_and_tuning(
    token: &str,
    tuning: HnswTuning,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-search-engine-opt-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("docs/a.md".to_string()),
            Value::Text("alpha content".to_string()),
        ],
        &OperationId::parse("test-op").expect("valid operation_id"),
    )
    .expect("insert row");

    // `EXPLAIN` の `engine:` 行（Issue #411）は `search_engine_kind()` が
    // `Some` の場合にのみ具体名を出す契約のため、`default` トークンでも
    // `from_storage`（`kind` 不明・`(custom_provider)`）ではなく
    // `from_storage_with_engine(.., default_kind())` を使い、
    // `main.rs::run_server` が `--search-engine default`／未指定で
    // `EngineCore::open` を呼んだときと同じ `engine: parallel_brute_force`
    // 表示になるようにする。
    let kind = kind_for_token_with(token, tuning).unwrap_or_else(search_engine::default_kind);
    let core = EngineCore::from_storage_with_engine(storage, kind).with_query_planner(Box::new(
        StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        },
    ));
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// R4: `default` は既存契約どおり `engine: parallel_brute_force`。
#[test]
fn explain_reports_parallel_brute_force_for_default_token() {
    let (core, _guard) = new_core_with_docs_table("default");
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    );

    let _columns = read_row_description(&mut stream);
    let mut rows = Vec::new();
    for _ in 0..7 {
        rows.push(read_data_row(&mut stream)[0].clone().expect("cell"));
    }
    assert_eq!(rows[4], "engine: parallel_brute_force");
    assert_eq!(rows[5], "ann_plan: plain_scan_engine");
    assert_eq!(rows[6], "scalar_plan: plain_scan");

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}

/// R4: `hnsw`／`hnsw_f16`／`hnsw_i8` は `engine: hnsw`・`hnsw_params:` の
/// `resident=` がそれぞれ f32／f16／i8 になる（Issue #411・#514・#521 の
/// `Display` 契約が wire 経由でも観測できることの固定）。
#[test]
fn explain_reports_hnsw_engine_with_resident_precision_per_token() {
    for token in ["hnsw", "hnsw_f16", "hnsw_i8"] {
        let (core, _guard) = new_core_with_docs_table(token);
        let mut stream = spawn_with_alice(core);

        send_simple_query(
            &mut stream,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        );

        let _columns = read_row_description(&mut stream);
        let mut rows = Vec::new();
        for _ in 0..8 {
            rows.push(read_data_row(&mut stream)[0].clone().expect("cell"));
        }
        assert_eq!(rows[4], "engine: hnsw", "token={token}");
        assert!(
            rows[5].contains(resident_suffix_for(token)),
            "token={token}: expected {:?} to contain {:?}",
            rows[5],
            resident_suffix_for(token)
        );
        assert_eq!(rows[6], "ann_plan: hnsw_full_visible", "token={token}");
        assert_eq!(rows[7], "scalar_plan: plain_scan", "token={token}");

        assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
        read_ready_for_query(&mut stream);
    }
}

/// R4 拡張（Issue #657）: `--hnsw-*` 探索パラメータ opt-in を指定しても
/// `hnsw_params:` 行は `sparse_visited_max=` のみ反映し、
/// `full_scan_ratio`／`acorn_max_visible_ratio` の値・キー名は出力全体の
/// どこにも現れないこと（テナント存在情報に繋がる値の `EXPLAIN` 非露出
/// 方針〔Issue #411〕の維持を機械的に固定する）。
#[test]
fn explain_hnsw_params_reflects_sparse_visited_max_only_not_ratios() {
    for token in ["hnsw", "hnsw_f16", "hnsw_i8"] {
        let tuning = HnswTuning {
            full_scan_ratio: Some(engine::hnsw::Ratio {
                numerator: 1,
                denominator: 2,
            }),
            acorn_max_visible_ratio: Some(engine::hnsw::Ratio {
                numerator: 1,
                denominator: 1,
            }),
            sparse_visited_max: Some(8),
        };
        let (core, _guard) = new_core_with_docs_table_and_tuning(token, tuning);
        let mut stream = spawn_with_alice(core);

        send_simple_query(
            &mut stream,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        );

        let _columns = read_row_description(&mut stream);
        let mut rows = Vec::new();
        for _ in 0..8 {
            rows.push(read_data_row(&mut stream)[0].clone().expect("cell"));
        }
        let expected_resident = match token {
            "hnsw" => "f32",
            "hnsw_f16" => "f16",
            "hnsw_i8" => "i8",
            other => panic!("unexpected token: {other:?}"),
        };
        assert_eq!(
            rows[5],
            format!(
                "hnsw_params: m=16,ef_construction=100,ef_search=64,resident={expected_resident},sparse_visited_max=8"
            ),
            "token={token}"
        );

        let full_output = rows.join("\n");
        assert!(
            !full_output.contains("full_scan_ratio"),
            "token={token}: full_scan_ratio must not leak into EXPLAIN output, got: {full_output}"
        );
        assert!(
            !full_output.contains("acorn"),
            "token={token}: acorn_max_visible_ratio must not leak into EXPLAIN output, got: {full_output}"
        );
        assert!(
            !full_output.contains("1/2") && !full_output.contains("1/1"),
            "token={token}: ratio values must not leak into EXPLAIN output, got: {full_output}"
        );

        assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
        read_ready_for_query(&mut stream);
    }
}

/// R5: 3 テナント × `Public`（コーパス）+ 各テナント固有の `Private` 行を
/// 投入し、フィルタなし `DISTANCE` クエリが選択エンジンによらず
/// (a) `Private` 行を一切返さない、(b) 3 テナントの可視結果が一致する、
/// (c) HNSW opt-in 3 種は索引が実際に構築される（非 vacuous。build 失敗 0・
/// 自動縮退カウンタ 0）ことを固定する。
///
/// 行数は `sql::hnsw_cache` の非公開下限 `MIN_INDEXED_ROWS`（Issue #408。
/// `docs/design/hnsw-generation-cache.md` 参照。ここでは数値を転記せず、本
/// テストの投入行数がその下限を優に超える桁であることのみをコメントする）を
/// 上回るよう `Public` 1,200 行（400 行 × 3 テナント）を投入し、索引が
/// 構造的に brute-force へ縮退しない条件を満たす。
#[test]
fn rls_boundary_and_ann_non_vacuous_hold_across_all_search_engine_tokens() {
    for token in ["default", "hnsw", "hnsw_f16", "hnsw_i8"] {
        run_rls_boundary_check(token, HnswTuning::default());
    }
}

/// R5 拡張（Issue #657）: `--hnsw-*` 探索パラメータ opt-in を指定しても
/// RLS 境界（(a) `Private` 非漏えい・(b) 3 テナント可視結果一致）・非 vacuous
/// な索引構築 (c) が不変であること。ACORN／sparse visited の発火有無自体は
/// 本 Issue の対象外（D7）とし、指定してもクエリの正しさ・決定性が崩れない
/// ことのみを固定する。
#[test]
fn rls_boundary_holds_with_hnsw_tuning_opt_in() {
    let tuning = HnswTuning {
        full_scan_ratio: Some(engine::hnsw::Ratio {
            numerator: 1,
            denominator: 4,
        }),
        acorn_max_visible_ratio: Some(engine::hnsw::Ratio {
            numerator: 1,
            denominator: 1,
        }),
        sparse_visited_max: Some(8),
    };
    for token in ["hnsw", "hnsw_f16", "hnsw_i8"] {
        run_rls_boundary_check(token, tuning);
    }
}

/// [`rls_boundary_and_ann_non_vacuous_hold_across_all_search_engine_tokens`]・
/// [`rls_boundary_holds_with_hnsw_tuning_opt_in`] が共有する本体。
///
/// 3 テナント × `Public`（コーパス）+ 各テナント固有の `Private` 行を投入し、
/// フィルタなし `DISTANCE` クエリが選択エンジン・チューニングによらず
/// (a) 他テナントの `Private` 行を一切返さない一方、自テナントの `Private`
/// 行は可視である（RLS-11・TASK-195・read-your-writes）、(b) 3 テナントの
/// Public 可視結果（各自の Private 行を除く）が一致する、(c) HNSW opt-in
/// 3 種は索引が実際に構築される（非 vacuous。build 失敗 0・自動縮退
/// カウンタ 0）ことを固定する。
///
/// 行数は `sql::hnsw_cache` の非公開下限 `MIN_INDEXED_ROWS`（Issue #408。
/// `docs/design/hnsw-generation-cache.md` 参照。ここでは数値を転記せず、本
/// テストの投入行数がその下限を優に超える桁であることのみをコメントする）を
/// 上回るよう `Public` 1,200 行（400 行 × 3 テナント）を投入し、索引が
/// 構造的に brute-force へ縮退しない条件を満たす。
fn run_rls_boundary_check(token: &str, tuning: HnswTuning) {
    const ROWS_PER_TENANT: u64 = 400;
    const TENANTS: [&str; 3] = ["tenant-alice", "tenant-bob", "tenant-carol"];
    const USERS: [(&str, &str, &str); 3] = [
        ("alice", "tenant-alice", "pw-alice"),
        ("bob", "tenant-bob", "pw-bob"),
        ("carol", "tenant-carol", "pw-carol"),
    ];

    {
        let path = temp_db::unique_db_path("wire-search-engine-opt-rls");
        let _guard = temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
            ))
            .expect("create table");

        // クエリベクトルの最近傍となる `Private` 行をテナントごとに 1 件だけ
        // 混入させ、「返らないこと」を意味のある形で検証できるようにする
        // （全行を無関係なベクトルにすると、そもそも近傍に来ないだけの
        // 偽陽性 pass を作りかねない）。
        let query_vec = [1.0_f32, 0.0, 0.0, 0.0];
        let mut next_id: u64 = 1;
        for tenant in TENANTS {
            let ctx =
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            for i in 0..ROWS_PER_TENANT {
                // Public 行は最近傍から少し外れたベクトルにする（id によって
                // 角度をずらす決定的分布。クエリと厳密一致しない）。基準角度は
                // 0.3 rad（cos 距離 ≈ 0.045）とし、距離 0 の自テナント
                // `Private` 行（RLS-11・TASK-195・read-your-writes）との差を
                // ANN（HNSW opt-in）の量子化誤差より十分大きく取ることで、
                // 自テナント Private 行が常に厳密な最近傍（rank 0）になる
                // ことをエンジン非依存で保証する（僅差の近傍点で ANN の
                // 近似探索・タイブレークが競合し得る領域を避ける）。
                let angle = (i as f32) * 0.001 + 0.3;
                let vec = vec![angle.cos(), angle.sin(), 0.0, 0.0];
                let op_id =
                    OperationId::parse(&format!("seed-{tenant}-{i}")).expect("valid operation_id");
                engine::tenant::insert_typed_row(
                    &storage,
                    "docs",
                    &ctx,
                    next_id,
                    Visibility::Public,
                    &[Value::Vector(vec)],
                    &op_id,
                )
                .unwrap_or_else(|e| panic!("insert public row failed: {e}"));
                next_id += 1;
            }
            // このテナントだけに見える最近傍 Private 行（クエリと厳密一致）。
            let op_id =
                OperationId::parse(&format!("seed-{tenant}-private")).expect("valid operation_id");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                next_id,
                Visibility::Private,
                &[Value::Vector(query_vec.to_vec())],
                &op_id,
            )
            .unwrap_or_else(|e| panic!("insert private row failed: {e}"));
            next_id += 1;
        }

        let core = match kind_for_token_with(token, tuning) {
            None => EngineCore::from_storage(storage, search_engine::default_engine()),
            Some(kind) => EngineCore::from_storage_with_engine(storage, kind),
        };
        let core = Arc::new(core);

        let users_path = write_user_store_file(&USERS);
        let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));

        let mut result_sets: Vec<Vec<String>> = Vec::new();
        for (username, _tenant, password) in USERS {
            let mut stream = authenticate_to_ready_for_query(addr, username, password);
            send_simple_query(
                &mut stream,
                "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0,0.0]' LIMIT 20",
            );
            let _columns = read_row_description(&mut stream);
            let mut ids = Vec::new();
            for _ in 0..20 {
                let row = read_data_row(&mut stream);
                ids.push(row[0].clone().expect("id is not null"));
            }
            assert_eq!(read_command_complete(&mut stream), "SELECT 20");
            read_ready_for_query(&mut stream);
            result_sets.push(ids);
        }

        // (a) 他テナントの Private 行 id（`next_id` の各テナント最終行）が
        // 自テナント以外の結果に混入していないこと。RLS-11・TASK-195 により
        // 各テナント自身の Private 行（クエリと厳密一致・距離 0）は自分の
        // 結果に可視である（read-your-writes）。Public 行の基準角度（0.3 rad）
        // は量子化誤差より十分大きく取ってあるため、自テナント Private 行は
        // エンジン（ANN opt-in 含む）によらず常に厳密な最近傍（rank 0）になる。
        let private_ids: Vec<u64> = (1..=3).map(|k| k * (ROWS_PER_TENANT + 1)).collect();
        for (idx, ids) in result_sets.iter().enumerate() {
            let own_private_id = private_ids[idx];
            for id_str in ids {
                let id: u64 = id_str.parse().expect("numeric id");
                if private_ids.contains(&id) {
                    assert_eq!(
                        id, own_private_id,
                        "token={token} tenant_idx={idx}: other tenant's private row id {id} leaked into results"
                    );
                }
            }
            assert_eq!(
                ids.first().map(String::as_str),
                Some(own_private_id.to_string().as_str()),
                "token={token} tenant_idx={idx}: own-tenant Private row (id={own_private_id}) must rank first via read-your-writes, got {ids:?}"
            );
        }

        // (b) 3 テナントの Public 可視集合は完全一致するため、各自の Private
        // 行（rank 0）を除いた残り 19 件は同一クエリに対して一致する。
        let public_tail: Vec<&[String]> = result_sets.iter().map(|ids| &ids[1..]).collect();
        assert_eq!(
            public_tail[0], public_tail[1],
            "token={token}: tenant alice/bob Public-only result mismatch"
        );
        assert_eq!(
            public_tail[1], public_tail[2],
            "token={token}: tenant bob/carol Public-only result mismatch"
        );

        // (c) HNSW opt-in 3 種は非 vacuous（索引が実際に構築され、構築失敗・
        // 各常駐精度の自動縮退カウンタが 0）であることを固定する。`default`
        // トークンでは `hnsw_index_cache_stats()` は既定値（すべて 0）のまま
        // （`hnsw_state` を持たないため）。
        let stats = core.hnsw_index_cache_stats();
        if token == "default" {
            assert_eq!(stats.builds, 0, "token=default must never build an index");
        } else {
            assert!(
                stats.builds >= 1,
                "token={token}: expected at least one HNSW build"
            );
            assert_eq!(
                stats.build_failures, 0,
                "token={token}: HNSW build must not fail"
            );
            match token {
                "hnsw_f16" => assert_eq!(
                    stats.f16_residency_fallbacks, 0,
                    "token=hnsw_f16: must not fall back to F32 for this fixture's corpus"
                ),
                "hnsw_i8" => assert_eq!(
                    stats.i8_residency_fallbacks, 0,
                    "token=hnsw_i8: must not fall back to F32 for this fixture's corpus"
                ),
                _ => {}
            }
        }
    }
}
