//! 密側の返却 id 検証で、高速経路が選ばれる入力（非 subset の恒等の候補列）と従来経路の入力（subset）が
//! SQL 経由で同じ結果を返すことの結合テスト（Issue #3220・親 #3218。TABLE-12・TABLE-26・
//! SQL-33 のポインタ。PR #3375 の対象外節「ビット一致を比べる結合テストが未作成」の回収）。
//!
//! ## 本テストが固定するもの・しないもの
//!
//! 固定するのは「`from_ids_checked` が高速経路を選ぶ入力（`dense_slots` 無しで非 subset の恒等の候補列
//! `0..n`）と、従来経路の入力（subset の候補列）で、SQL 経由の結果がビット一致する」ことと、その入力が
//! provider へ実際に渡ったこと（`Probe`）。**実際に選ばれた variant（`Identity`／`Counts`）は直接観測して
//! いない**（本体に観測点が無い）。仮に常に `Counts` を返す退行があっても、本テストの結果比較は通る。
//! variant の選択そのもの（恒等の候補列なら `Identity`、そうでなければ `Counts`）は PR #3375 の単体テスト
//! `core::searched_ids_tests::from_ids_checked_picks_identity_only_for_exact_identity` が、Identity と
//! HashMap の検証結果の一致は同 `identity_matches_hashmap_for_all_short_sequences` が固定する。
//!
//! ## どちらの入力になるか（`crates/engine/src/sql/exec.rs`）
//!
//! `dense_id_counts` は `SearchedIds::from_ids_checked(&slot_ids, dense_slots)` で作られる
//! （`execute_statement_with_cache_observed` の「Issue #3220」コメント付近）。
//!
//! - 高速経路が選ばれる入力: `dense_slots` が `None`（その文の arena に NULL ベクトル行が無い）かつ `slot_ids` が
//!   ちょうど `0..n`。`slot_ids` はマスク無しなら常に `0..arena.len()`、マスクありなら `kept`
//!   （元スロット番号の列）なので、マスクが先頭区間でなければ恒等にならない。
//! - 従来経路の入力: `dense_slots` が `Some`（NULL 行あり）または `slot_ids` が非恒等（マスクで
//!   1 行おきなど先頭区間でない部分集合に絞った場合）。
//!
//! 切り替えに使った条件と、使っていない条件:
//!
//! - **マスク（本テストで使用）**: 同じ表・同じ文を 1 回目（走査索引が未構築。縮約 arena を
//!   作る cold。`slot_ids = 0..5`）と 2 回目（索引が構築済みで `mask_kept_slots` が
//!   `Some`。`slot_ids = kept = [1,3,5,7,9]`）で流して比べる。
//! - **NULL ベクトル行（本テストで使用）**: 同じ行に NULL 行 2 件（先頭と途中）を足した表と、足していない表を
//!   比べる。足した側は `dense_slots = Some`（従来経路の入力）、足していない側は恒等列（高速経路の入力）。
//!   NULL 行は密側の候補に入らないため、論理的な結果集合は同じになる。
//! - **削除による slot の穴は使わない**: arena の slot は可視行の並び位置で、削除しても詰まって
//!   `0..n` のままなので、経路は切り替わらない。
//! - 先頭区間になるマスク（`id <= 5` 型・全行一致・空）と、WHERE で NULL 行を落とす文は、縮約 arena が
//!   NULL 無しの恒等列に戻るため切り替えにならない（比較に使わない）。
//!
//! - **hybrid（RRF）の密側アダプタ**: スカラー WHERE のマスクは密側へ渡らず（cold/hot とも恒等列で
//!   `Full`）、トリガは NULL 行だけ。検索語を NULL 行・`b` 行の本文に無い語にして 2 表を比べる。
//! - 距離演算子は kNN の汎用アームに届く負の内積と、コサインアームの `<=>` を使う。L2・L1 の演算子は
//!   SQL 実行経路に未接続で拒否されるため使えない（本テストは拒否を固定しない）。
//!
//! 経路の証拠は実行ごとに [`Probe`]（`SearchProvider` の記録ラッパ）で取る。provider に渡った
//! `ids`／`slots` の中身を記録し、高速側は非 subset 呼び出しで `ids == 0..n`、従来側は subset 呼び出しで
//! 期待したスロット列、であることをアサートする。厳密全走査（`DESC`・`LIMIT` 省略）は provider を通らず
//! 検証の variant を直接観測できない（上記のとおり他の形も同様）。「provider 呼び出しが 0 件」と、マスク側は `index_mask_scans` の増分で
//! マスク経路に入ったことを示し、NULL 側は上記の導出（`null_row_count != 0`）に拠る。
//!
//! 結果の比較は行・id・距離（score は `f64::to_bits`）・投影セル（`Float`・`Vector` は f64/f32 のビット）・
//! 行順・結果の列メタ（型）を完全一致で見る。

use std::sync::{Arc, Mutex};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{
    CandidateHit, CosineSearchInput, CpuScalarProvider, DistanceHit, KernelError, SearchInput,
    SearchProvider, SubsetSearchInput,
};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 他テナント（`bob`）の行 id。alice の可視集合に現れてはならない。
const BOB_IDS: [u64; 3] = [15, 25, 35];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// provider へ渡った呼び出しの記録。`ids`／`slots` は中身そのものを持つ（長さだけで経路を判定しない）。
#[derive(Debug, Clone, PartialEq)]
enum Call {
    /// `search`／`search_cosine`（`ids` を位置対応で渡す従来の全走査呼び出し）。
    Full {
        cosine: bool,
        ids: Vec<u64>,
        rows: usize,
    },
    /// `search_subset`／`search_cosine_subset`（行番号を候補マスクとして渡す呼び出し）。
    Subset {
        cosine: bool,
        slots: Vec<u64>,
        rows: usize,
    },
}

/// 記録つき provider。`forge` が `Some(id)` のときは結果の末尾へ捏造ヒットを足す
/// （返却 id 検証の拒否を SQL 経由で固定するため）。計算は `CpuScalarProvider` へ委譲する。
#[derive(Default)]
struct Probe {
    calls: Mutex<Vec<Call>>,
    forge: Mutex<Option<u64>>,
}

impl Probe {
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().expect("lock"))
    }

    fn forged(&self) -> Option<u64> {
        *self.forge.lock().expect("lock")
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("lock").push(call);
    }
}

struct SharedProbe(Arc<Probe>);

fn rows_of(vectors: &[f32], dim: u32) -> usize {
    vectors.len().checked_div(dim as usize).unwrap_or(0)
}

impl SearchProvider for SharedProbe {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.0.record(Call::Full {
            cosine: false,
            ids: input.ids.to_vec(),
            rows: rows_of(input.vectors, input.dim),
        });
        let mut hits = CpuScalarProvider.search(input)?;
        if let Some(id) = self.0.forged() {
            hits.push(CandidateHit { id, score: -1.0e30 });
        }
        Ok(hits)
    }

    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        self.0.record(Call::Subset {
            cosine: false,
            slots: input.slots.iter().map(|&s| u64::from(s)).collect(),
            rows: rows_of(input.vectors, input.dim),
        });
        let mut hits = CpuScalarProvider.search_subset(input)?;
        if let Some(id) = self.0.forged() {
            hits.push(CandidateHit { id, score: -1.0e30 });
        }
        Ok(hits)
    }

    fn search_cosine(&self, input: CosineSearchInput<'_>) -> Result<Vec<DistanceHit>, KernelError> {
        self.0.record(Call::Full {
            cosine: true,
            ids: input.ids.to_vec(),
            rows: rows_of(input.vectors, input.dim),
        });
        let mut hits = CpuScalarProvider.search_cosine(input)?;
        if let Some(id) = self.0.forged() {
            forge_distance_hit(&mut hits, id);
        }
        Ok(hits)
    }

    fn search_cosine_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<DistanceHit>, KernelError> {
        self.0.record(Call::Subset {
            cosine: true,
            slots: input.slots.iter().map(|&s| u64::from(s)).collect(),
            rows: rows_of(input.vectors, input.dim),
        });
        let mut hits = CpuScalarProvider.search_cosine_subset(input)?;
        if let Some(id) = self.0.forged() {
            forge_distance_hit(&mut hits, id);
        }
        Ok(hits)
    }
}

/// 捏造ヒットを足す。距離昇順・NaN は末尾の連続区間・同点は id 昇順・件数と重複の契約は保ち、
/// 候補集合への所属だけに違反する返却列にする（順序違反で同じ拒否になって所属検査の退行を
/// 隠さないため）。NaN の候補を含むなら捏造も NaN 距離にして NaN 区間へ入れる。
fn forge_distance_hit(hits: &mut Vec<DistanceHit>, id: u64) {
    let has_nan = hits.iter().any(|h| h.distance.is_nan());
    let distance = if has_nan { f64::NAN } else { 1.0e300 };
    hits.push(DistanceHit { id, distance });
    hits.sort_by(|a, b| {
        a.distance
            .is_nan()
            .cmp(&b.distance.is_nan())
            .then_with(|| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), true),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, false),
            ColumnDef::new("b", ColumnType::BigInt, false),
            ColumnDef::new("r", ColumnType::Real, false),
            ColumnDef::new("d", ColumnType::Double, false),
        ],
    )
}

/// alice の通常行 10 件（id = 10, 20, ..., 100）。`i` は 1..=10。偶数 `i` は `kind = 'a'`。
/// 距離が同点になる組（`<#>`・`<=>` で `i` 奇数の一部・偶数すべて）、ゼロベクトル（`<=>` で NaN。`i=5`）、
/// f32 の上限に近い値（`i=7`。距離の桁あふれ）、`b = i64::MAX`（`i=2`。式のオーバーフロー）を含める。
fn embedding_of(i: u64) -> [f32; 2] {
    match i {
        1 | 3 | 9 => [1.0, 0.0],
        5 => [0.0, 0.0],
        7 => [3.0e38, 3.0e38],
        _ => [0.0, 1.0],
    }
}

#[allow(clippy::too_many_arguments)]
fn insert(
    storage: &Storage,
    table: &str,
    tenant: &str,
    id: u64,
    embedding: Option<[f32; 2]>,
    kind: &str,
    b: i64,
) {
    let vec_value = match embedding {
        Some(e) => Value::Vector(e.to_vec()),
        None => Value::Null,
    };
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        // 他テナントの行は Private（Public は他テナントからも正当に見えるため、境界の検証にならない）。
        if tenant == "bob" {
            Visibility::Private
        } else {
            Visibility::Public
        },
        &[
            vec_value,
            Value::Text(kind.to_string()),
            Value::Text(format!("{kind} doc {id}")),
            Value::Integer(i32::try_from(id).expect("small id")),
            Value::BigInt(b),
            Value::Real(id as f32 * 0.1),
            Value::Double(id as f64 * 0.25),
        ],
        &OperationId::parse(&format!("seed-{table}-{tenant}-{id}")).expect("operation id"),
    )
    .expect("insert row");
}

/// 表 `table` へ alice の通常 10 行と、`null_ids` の NULL ベクトル行（`kind = 'n'`）と、他テナントの行を入れる。
/// 他テナントの行はクエリ `[1,0]` に最も近い（距離 0）ため、テナント境界が破れれば結果の先頭に出る。
fn seed(storage: &Storage, table: &str, null_ids: &[u64]) {
    storage.create_table(&schema(table)).expect("create table");
    for i in 1..=10_u64 {
        let kind = if i % 2 == 0 { "a" } else { "b" };
        let b = if i == 2 { i64::MAX } else { i as i64 };
        insert(
            storage,
            table,
            "alice",
            i * 10,
            Some(embedding_of(i)),
            kind,
            b,
        );
    }
    for &id in null_ids {
        insert(storage, table, "alice", id, None, "n", 1);
    }
    for id in BOB_IDS {
        insert(storage, table, "bob", id, Some([1.0, 0.0]), "a", 1);
    }
}

fn open(label: &str, probe: &Arc<Probe>, tables: &[(&str, &[u64])]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    for (table, null_ids) in tables {
        seed(&storage, table, null_ids);
    }
    let core = EngineCore::from_storage(storage, Box::new(SharedProbe(Arc::clone(probe))));
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    core.execute_sql(&ctx("alice"), sql)
}

/// 比較用の指紋。行順・id・score のビット・投影セル（`Float`・`Vector` はビット）・列メタを保つ。
type Fingerprint = (String, Vec<(u64, u64, Vec<String>)>);

fn fingerprint(r: &QueryResult) -> Fingerprint {
    let cols = format!("{:?}", r.columns);
    let rows = r
        .rows
        .iter()
        .map(|row| {
            let cells = row
                .cells
                .iter()
                .map(|c| match c {
                    Cell::Float(f) => format!("Float#{:016x}", f.to_bits()),
                    Cell::Vector(v) => format!(
                        "Vector#{:?}",
                        v.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
                    ),
                    other => format!("{other:?}"),
                })
                .collect();
            (row.id, row.score.to_bits(), cells)
        })
        .collect();
    (cols, rows)
}

fn ids_of(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|row| row.id).collect()
}

fn assert_no_other_tenant(r: &QueryResult, sql: &str) {
    for id in ids_of(r) {
        assert!(
            !BOB_IDS.contains(&id),
            "other tenant row {id} leaked: {sql}"
        );
    }
}

/// kNN 本体の返却値検証（`distance_result_is_valid_with`／`provider_result_is_valid_with`）が
/// 違反時に返す固定の detail（`sql/exec.rs` のコサインアームと汎用アームで同一文言）。
const TOP_K_CONTRACT_DETAIL: &str =
    "search provider returned a result violating the top-k contract";

const COLS: &str = "id, kind, n, b, r, d, embedding";

/// provider を通る形（`(ラベル, 末尾句, cosine か)`）。WHERE は呼び出し側が足す。
const PROVIDER_SHAPES: &[(&str, &str, bool)] = &[
    (
        "knn_neg_inner_limit3",
        "ORDER BY embedding <#> '[1.0,0.0]' LIMIT 3",
        false,
    ),
    (
        "knn_cosine_limit3",
        "ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
        true,
    ),
    // 全件（5 行・10 行）より大きい LIMIT。
    (
        "knn_neg_inner_limit_over_all",
        "ORDER BY embedding <#> '[1.0,0.0]' LIMIT 1000",
        false,
    ),
    (
        "knn_cosine_limit_over_all",
        "ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1000",
        true,
    ),
];

/// マスクで切り替わる形。同じ表・同じ文の 1 回目（恒等列の入力）と 2 回目（subset の入力）が一致する。
#[test]
fn sql3220_mask_path_matches_identity_path_bitwise() {
    for (label, tail, cosine) in PROVIDER_SHAPES {
        let probe = Arc::new(Probe::default());
        let (core, _g) = open("sql3220-mask", &probe, &[("docs", &[])]);
        let sql = format!("SELECT {COLS} FROM docs WHERE kind = 'a' {tail}");

        let cold = run(&core, &sql).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let cold_calls = probe.take();
        let before = core.scalar_index_cache_stats().index_mask_scans;
        let hot = run(&core, &sql).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let hot_calls = probe.take();
        let after = core.scalar_index_cache_stats().index_mask_scans;

        // 経路の証拠: cold は縮約 arena（恒等 0..5）の全走査呼び出し、hot は元スロット
        // [1,3,5,7,9]（id 20,40,60,80,100）をマスクで渡す subset 呼び出し。
        assert_eq!(
            cold_calls,
            vec![Call::Full {
                cosine: *cosine,
                ids: vec![0, 1, 2, 3, 4],
                rows: 5
            }],
            "{label}: cold must take the identity path"
        );
        assert_eq!(
            hot_calls,
            vec![Call::Subset {
                cosine: *cosine,
                slots: vec![1, 3, 5, 7, 9],
                rows: 10
            }],
            "{label}: hot must take the mask (non-identity) path"
        );
        assert_eq!(after, before + 1, "{label}: mask scan counter must advance");

        assert_eq!(fingerprint(&cold), fingerprint(&hot), "{label}");
        assert_no_other_tenant(&hot, &sql);
        assert!(!hot.rows.is_empty(), "{label}: non-vacuous");
    }
}

/// NULL ベクトル行の有無で切り替わる形。NULL 行は密側の候補に入らないので結果集合は同じ。
#[test]
fn sql3220_null_rows_path_matches_identity_path_bitwise() {
    // NULL 行は id 5（先頭スロット 0）と id 55（スロット 6）。通常行は 10, 20, ..., 100。
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-null",
        &probe,
        &[("plain", &[]), ("nulls", &[5, 55])],
    );
    let dense_slots: Vec<u64> = vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11];
    for (label, tail, cosine) in PROVIDER_SHAPES {
        let sql = |t: &str| format!("SELECT {COLS} FROM {t} {tail}");
        let fast = run(&core, &sql("plain")).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let fast_calls = probe.take();
        let slow = run(&core, &sql("nulls")).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let slow_calls = probe.take();

        assert_eq!(
            fast_calls,
            vec![Call::Full {
                cosine: *cosine,
                ids: (0..10).collect(),
                rows: 10
            }],
            "{label}: table without NULL rows must take the identity path"
        );
        assert_eq!(
            slow_calls,
            vec![Call::Subset {
                cosine: *cosine,
                slots: dense_slots.clone(),
                rows: 12
            }],
            "{label}: table with NULL rows must take the dense-slots (non-identity) path"
        );
        assert_eq!(fingerprint(&fast), fingerprint(&slow), "{label}");
        assert_no_other_tenant(&slow, &sql("nulls"));
        assert!(!fast.rows.is_empty(), "{label}: non-vacuous");
        // NULL 行（id 5・55）は密側の結果に出ない。
        assert!(!ids_of(&slow).contains(&5) && !ids_of(&slow).contains(&55));
    }
}

/// 厳密全走査（`DESC`・`LIMIT` 省略）の所属検査。provider は通らない（呼び出し 0 件）。
/// マスク側はカウンタで、NULL 側は NULL 行の有無で従来経路に入ったことを示す。
#[test]
fn sql3220_exact_scan_membership_matches_across_paths() {
    let shapes = [
        "ORDER BY embedding <#> '[1.0,0.0]' DESC LIMIT 4",
        "ORDER BY embedding <=> '[1.0,0.0]' DESC LIMIT 1000",
        "ORDER BY embedding <#> '[1.0,0.0]'",
    ];
    for tail in shapes {
        // ケースごとに索引の無い core（1 回目の実行で索引ができる）。
        let probe = Arc::new(Probe::default());
        let (core, _g) = open(
            "sql3220-exact",
            &probe,
            &[("docs", &[]), ("plain", &[]), ("nulls", &[5, 55])],
        );
        // マスク: 同じ文の 1 回目と 2 回目。
        let sql = format!("SELECT {COLS} FROM docs WHERE kind = 'a' {tail}");
        let c_init = core.scalar_index_cache_stats().index_mask_scans;
        let cold = run(&core, &sql).unwrap_or_else(|e| panic!("{tail}: {e:?}"));
        let c0 = core.scalar_index_cache_stats().index_mask_scans;
        let hot = run(&core, &sql).unwrap_or_else(|e| panic!("{tail}: {e:?}"));
        let c1 = core.scalar_index_cache_stats().index_mask_scans;
        assert_eq!(c0, c_init, "{tail}: first run must not use the mask path");
        assert!(
            probe.take().is_empty(),
            "{tail}: exact scan bypasses provider"
        );
        assert_eq!(c1, c0 + 1, "{tail}: second run must consume the mask path");
        assert_eq!(fingerprint(&cold), fingerprint(&hot), "{tail}");
        assert_no_other_tenant(&hot, &sql);
        assert!(!hot.rows.is_empty());

        // NULL 行。
        let q = |t: &str| format!("SELECT {COLS} FROM {t} {tail}");
        let fast = run(&core, &q("plain")).unwrap_or_else(|e| panic!("{tail}: {e:?}"));
        let slow = run(&core, &q("nulls")).unwrap_or_else(|e| panic!("{tail}: {e:?}"));
        assert!(probe.take().is_empty());
        assert_eq!(fingerprint(&fast), fingerprint(&slow), "{tail}");
        assert!(!fast.rows.is_empty());
        assert_no_other_tenant(&slow, &q("nulls"));
    }
}

/// 空集合（該当なし）と `LIMIT 0`。空結果の列メタ（型）が経路間で一致し、`LIMIT 0` は同じ拒否になる。
/// 検証 variant の比較ではなく、空結果と列メタの一致を固定する（候補 0 件・k=0 は検証に届かない場合がある）。
#[test]
fn sql3220_empty_results_match_across_paths() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-empty",
        &probe,
        &[("docs", &[]), ("plain", &[]), ("nulls", &[5, 55])],
    );
    // 該当なし（マスクは空になる。cold・hot ともに空）。
    let none = format!(
        "SELECT {COLS} FROM docs WHERE kind = 'zzz' ORDER BY embedding <#> '[1.0,0.0]' LIMIT 5"
    );
    let cold = run(&core, &none).expect("cold none");
    let hot = run(&core, &none).expect("hot none");
    assert!(cold.rows.is_empty() && hot.rows.is_empty());
    assert_eq!(fingerprint(&cold), fingerprint(&hot));

    // k = 0 の `LIMIT 0` は束縛が範囲外として拒否する（検証へ届かない）。経路によらず同じ拒否になる。
    for op in ["<#>", "<=>"] {
        let q =
            |t: &str| format!("SELECT {COLS} FROM {t} ORDER BY embedding {op} '[1.0,0.0]' LIMIT 0");
        let fast = run(&core, &q("plain")).expect_err("fast limit 0");
        let slow = run(&core, &q("nulls")).expect_err("slow limit 0");
        assert_eq!(fast.wire_code(), slow.wire_code(), "{op}");
        assert_eq!(format!("{fast:?}"), format!("{slow:?}"), "{op}");
    }
    // 空の結果でも列メタは非空（型が公告される）。
    assert_eq!(fingerprint(&cold).0, fingerprint(&hot).0);
    assert!(!cold.columns.is_empty());
}

/// 距離が同点の行の順序（id 昇順）が経路間で一致する。`kind = 'a'` の 5 行（`i` 偶数）は
/// すべて同一ベクトルで、距離が完全同点になる。
#[test]
fn sql3220_distance_ties_keep_id_order_across_paths() {
    for op in ["<#>", "<=>"] {
        // 演算子ごとに新しい core（1 回目が cold になる）。
        let probe = Arc::new(Probe::default());
        let (core, _g) = open(
            "sql3220-ties",
            &probe,
            &[("docs", &[]), ("plain", &[]), ("nulls", &[5, 55])],
        );
        let tail = format!("ORDER BY embedding {op} '[0.0,1.0]' LIMIT 4");
        // マスク: kind='a' の 5 行が完全同点。先頭 4 件は id 昇順。
        let sql = format!("SELECT {COLS} FROM docs WHERE kind = 'a' {tail}");
        let cold = run(&core, &sql).expect("cold");
        let cold_calls = probe.take();
        let hot = run(&core, &sql).expect("hot");
        let hot_calls = probe.take();
        assert!(matches!(cold_calls.as_slice(), [Call::Full { .. }]), "{op}");
        assert!(
            matches!(hot_calls.as_slice(), [Call::Subset { .. }]),
            "{op}"
        );
        assert_eq!(ids_of(&cold), vec![20, 40, 60, 80], "{op}");
        assert_eq!(fingerprint(&cold), fingerprint(&hot), "{op}");

        // NULL: 全件でも偶数 `i` の行（同点）は id 昇順で並ぶ。両経路で一致する。
        let q = |t: &str| format!("SELECT {COLS} FROM {t} {tail}");
        let fast = run(&core, &q("plain")).expect("fast");
        let slow = run(&core, &q("nulls")).expect("slow");
        assert!(matches!(
            probe.take().as_slice(),
            [Call::Full { .. }, Call::Subset { .. }]
        ));
        assert_eq!(fingerprint(&fast), fingerprint(&slow), "{op}");
        let evens: Vec<u64> = ids_of(&fast).into_iter().filter(|i| i % 20 == 0).collect();
        assert!(evens.windows(2).all(|w| w[0] < w[1]), "{op}: {evens:?}");
        assert!(evens.len() >= 3, "{op}: ties are present");
    }
}

/// 式のオーバーフロー（`b = i64::MAX` の行を `b + 1` で投影）は、経路によらず同じ SQLSTATE で拒否される。
#[test]
fn sql3220_expression_overflow_is_identical_across_paths() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-overflow",
        &probe,
        &[("docs", &[]), ("plain", &[]), ("nulls", &[5, 55])],
    );
    let code = |sql: &str| match run(&core, sql) {
        Ok(r) => panic!("expected an error, got {} rows: {sql}", r.rows.len()),
        Err(e) => e.wire_code().to_string(),
    };
    let tail = "ORDER BY embedding <#> '[0.0,1.0]' LIMIT 20";
    let masked = format!("SELECT id, b + 1 FROM docs WHERE kind = 'a' {tail}");
    let cold = code(&masked);
    let cold_calls = probe.take();
    let hot = code(&masked);
    let hot_calls = probe.take();
    let fast = code(&format!("SELECT id, b + 1 FROM plain {tail}"));
    let fast_calls = probe.take();
    let slow = code(&format!("SELECT id, b + 1 FROM nulls {tail}"));
    let slow_calls = probe.take();
    // 経路の証拠（provider まで届いたうえで投影段が拒否している）。
    let full = |n: u64| Call::Full {
        cosine: false,
        ids: (0..n).collect(),
        rows: n as usize,
    };
    assert_eq!(cold_calls, vec![full(5)]);
    assert_eq!(
        hot_calls,
        vec![Call::Subset {
            cosine: false,
            slots: vec![1, 3, 5, 7, 9],
            rows: 10
        }]
    );
    assert_eq!(fast_calls, vec![full(10)]);
    assert_eq!(
        slow_calls,
        vec![Call::Subset {
            cosine: false,
            slots: vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11],
            rows: 12
        }]
    );
    assert_eq!(cold, "22003");
    assert_eq!(cold, hot);
    assert_eq!(cold, fast);
    assert_eq!(cold, slow);
}

/// hybrid（RRF）の密側アダプタ（`CosineDenseAdapter`）。スカラー WHERE のマスクは hybrid の密側へ
/// 渡らない（cold/hot とも `Full` の恒等列）ため、トリガは NULL ベクトル行だけ。NULL 行は疎側の
/// コーパス統計を変えるので、検索語は `kind = 'a'` の本文にだけ現れる語（NULL 行・`b` 行の本文には無い）にし、
/// 融合の順位が変わらない形で 2 表を比べる。
#[test]
fn sql3220_hybrid_dense_adapter_null_rows_match_identity_path() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-hybrid",
        &probe,
        &[("plain", &[]), ("nulls", &[5, 55])],
    );
    let sql = |t: &str| {
        format!(
            "SELECT id, kind FROM {t} \
             ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'a') LIMIT 20"
        )
    };
    let fast = run(&core, &sql("plain")).unwrap_or_else(|e| panic!("hybrid plain: {e:?}"));
    let fast_calls = probe.take();
    let slow = run(&core, &sql("nulls")).unwrap_or_else(|e| panic!("hybrid nulls: {e:?}"));
    let slow_calls = probe.take();
    assert_eq!(
        fast_calls,
        vec![Call::Full {
            cosine: true,
            ids: (0..10).collect(),
            rows: 10
        }]
    );
    assert_eq!(
        slow_calls,
        vec![Call::Subset {
            cosine: true,
            slots: vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11],
            rows: 12
        }]
    );
    assert!(!fast.rows.is_empty());
    assert_eq!(fingerprint(&fast), fingerprint(&slow));
    assert_no_other_tenant(&slow, &sql("nulls"));
}

/// 最適化の対象外の形（NULL 行あり・マスクで非恒等）でも従来経路のまま fail-closed に動く。
/// provider が候補外の id（他テナントの行・NULL 行・範囲外のスロット）を返したら、結果を 1 行も返さず
/// `XX000` で拒否する。恒等列の入力でも範囲外 id は拒否される。拒否は kNN 本体の返却値検証の固定 detail と provider 呼び出しで特定する。他テナントの行は正常時にも出ない。
#[test]
fn sql3220_forged_ids_are_rejected_on_both_paths_and_never_cross_tenants() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-forge",
        &probe,
        &[("docs", &[]), ("nulls", &[5, 55])],
    );
    let tails = [
        ("neg_inner", "ORDER BY embedding <#> '[1.0,0.0]' LIMIT 100"),
        ("cosine", "ORDER BY embedding <=> '[1.0,0.0]' LIMIT 100"),
    ];
    for (label, tail) in tails {
        let plain = format!("SELECT {COLS} FROM docs {tail}");
        let nulls = format!("SELECT {COLS} FROM nulls {tail}");
        let masked = format!("SELECT {COLS} FROM docs WHERE kind = 'a' {tail}");
        // 正常時: 他テナントの行は 3 経路のどれにも出ない。
        for sql in [&plain, &nulls, &masked, &masked] {
            let ok = run(&core, sql).unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert!(!ok.rows.is_empty());
            assert_no_other_tenant(&ok, sql);
        }
        probe.take();

        // 捏造: 候補集合の外の id を 1 件足す。期待する provider 呼び出しと、本体の返却値検証が返す
        // 固定の detail（`sql/exec.rs` の kNN 本体。`<=>` のコサインアームと汎用アームで同じ文言）を照合する。
        let cosine = label == "cosine";
        let full10 = Call::Full {
            cosine,
            ids: (0..10).collect(),
            rows: 10,
        };
        let dense = Call::Subset {
            cosine,
            slots: vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11],
            rows: 12,
        };
        let masked_call = Call::Subset {
            cosine,
            slots: vec![1, 3, 5, 7, 9],
            rows: 10,
        };
        let cases: [(&str, &str, u64, Call); 4] = [
            // 恒等経路（10 行）。id 10 は範囲外。
            ("identity out of range", &plain, 10, full10),
            // NULL 行あり（12 行。dense は NULL スロット 0・6 を除く）。NULL 行のスロットを返す。
            ("dense excludes null slot 0", &nulls, 0, dense.clone()),
            ("dense excludes null slot 6", &nulls, 6, dense),
            // マスク経路（kept = [1,3,5,7,9]）。kept に無い元スロット 0（arena 内の可視行）を返す。
            ("mask excludes slot 0", &masked, 0, masked_call),
        ];
        for (why, sql, forged, expected_call) in cases {
            probe.take();
            *probe.forge.lock().expect("lock") = Some(forged);
            let res = run(&core, sql);
            *probe.forge.lock().expect("lock") = None;
            let calls = probe.take();
            assert_eq!(calls, vec![expected_call], "{label} {why}: provider call");
            match res {
                Err(SqlSurfaceError::Internal { detail }) => assert_eq!(
                    detail, TOP_K_CONTRACT_DETAIL,
                    "{label} {why}: rejected by a different check"
                ),
                other => panic!("{label} {why}: forged id {forged} must be rejected: {other:?}"),
            }
        }
        probe.take();
    }
}
