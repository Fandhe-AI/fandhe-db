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

/// `kind = 'a'` で WHERE に一致する NULL ベクトル行の id（`seed` が参照する）。
const MATCHING_NULL_IDS: [u64; 2] = [25, 65];

/// 他テナント（`bob`）の行 id。alice の可視集合に現れてはならない。
const BOB_IDS: [u64; 3] = [1015, 1025, 1035];

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
    /// `Some(id)` のとき、コサイン結果の既存の NaN ヒット 1 件を id だけ候補外へ置き換える（件数を変えない）。
    replace_nan: Mutex<Option<u64>>,
    /// 各 provider 呼び出しの `(要求 k, 捏造後の返却件数)`。件数の契約（返却件数 <= k）を満たす根拠。
    ks: Mutex<Vec<(usize, usize)>>,
}

impl Probe {
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().expect("lock"))
    }

    fn forged(&self) -> Option<u64> {
        *self.forge.lock().expect("lock")
    }

    fn take_ks(&self) -> Vec<(usize, usize)> {
        std::mem::take(&mut *self.ks.lock().expect("lock"))
    }

    fn note_k(&self, k: usize, returned: usize) {
        self.ks.lock().expect("lock").push((k, returned));
    }

    /// コサイン結果へ捏造を適用する。`replace_nan` が設定されていれば既存の NaN ヒットの id を置き換える
    /// （件数・距離順・NaN 末尾・重複なしを保つ）。無ければ `forge` の捏造ヒットを足す。
    fn forge_cosine(&self, hits: &mut Vec<DistanceHit>, k: usize) {
        let replace = *self.replace_nan.lock().expect("lock");
        if let Some(id) = replace {
            let slot = hits
                .iter_mut()
                .find(|h| h.distance.is_nan())
                .expect("fixture has a NaN distance hit to replace");
            slot.id = id;
            sort_distance_hits(hits);
        } else if let Some(id) = self.forged() {
            forge_distance_hit(hits, id);
        }
        self.note_k(k, hits.len());
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
        let k = input.k;
        let mut hits = CpuScalarProvider.search(input)?;
        if let Some(id) = self.0.forged() {
            hits.push(CandidateHit { id, score: -1.0e30 });
        }
        self.0.note_k(k, hits.len());
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
        let k = input.k;
        let mut hits = CpuScalarProvider.search_subset(input)?;
        if let Some(id) = self.0.forged() {
            hits.push(CandidateHit { id, score: -1.0e30 });
        }
        self.0.note_k(k, hits.len());
        Ok(hits)
    }

    fn search_cosine(&self, input: CosineSearchInput<'_>) -> Result<Vec<DistanceHit>, KernelError> {
        self.0.record(Call::Full {
            cosine: true,
            ids: input.ids.to_vec(),
            rows: rows_of(input.vectors, input.dim),
        });
        let k = input.k;
        let mut hits = CpuScalarProvider.search_cosine(input)?;
        self.0.forge_cosine(&mut hits, k);
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
        let k = input.k;
        let mut hits = CpuScalarProvider.search_cosine_subset(input)?;
        self.0.forge_cosine(&mut hits, k);
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
    sort_distance_hits(hits);
}

/// 距離昇順・NaN は末尾の連続区間・同点は id 昇順に並べる（`cmp_distance_asc_then_tie` の契約と同じ）。
fn sort_distance_hits(hits: &mut [DistanceHit]) {
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
/// f32 の上限に近い値（`i=7`。距離の桁あふれ）、`n = i32::MAX`（id 20。式のオーバーフロー）を含める。
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
            // id 20 だけ `i32::MAX`（`n + 1` は被演算子を読めて、加算の結果だけが型の範囲を超える）。
            Value::Integer(if id == 20 && tenant == "alice" {
                i32::MAX
            } else {
                i32::try_from(id).expect("small id")
            }),
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
    // テナント間で行 id を重ねない（`assert_no_other_tenant` が行 id だけで出どころを判定するため）。
    // alice の通常行は 10..=100、NULL 行は `null_ids`。bob は 1000 番台。
    assert!(
        (1..=10_u64)
            .map(|i| i * 10)
            .chain(null_ids.iter().copied())
            .all(|id| !BOB_IDS.contains(&id)),
        "alice and bob row ids must be disjoint"
    );
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
    if table == "both_nan" {
        // WHERE に一致するゼロベクトル行（コサイン距離が NaN。`replace_nan` の置き換え対象になる）。
        insert(storage, table, "alice", 45, Some([0.0, 0.0]), "a", 1);
        // 一致率を下げ（索引のマスク経路は一致行が全体の半分程度までのときに使われる）、一致しない行を足す。
        for id in [110, 120, 130] {
            insert(storage, table, "alice", id, Some([0.0, 1.0]), "b", 1);
        }
    }
    for &id in null_ids {
        // `MATCHING_NULL_IDS` の NULL 行は `kind = 'a'`（WHERE に一致する NULL 行）。
        let kind = if MATCHING_NULL_IDS.contains(&id) {
            "a"
        } else {
            "n"
        };
        insert(storage, table, "alice", id, None, kind, 1);
    }
    for id in BOB_IDS {
        insert(storage, table, "bob", id, Some([1.0, 0.0]), "a", 1);
    }
}

fn open(label: &str, probe: &Arc<Probe>, tables: &[(&str, &[u64])]) -> (EngineCore, CleanupGuard) {
    // パスを払い出した直後にガードを作る（以降の `open`・seed が panic しても一時 DB を残さない）。
    let guard = CleanupGuard(unique_db_path(label));
    let storage = Storage::open(&guard.0).expect("open storage");
    for (table, null_ids) in tables {
        seed(&storage, table, null_ids);
    }
    let core = EngineCore::from_storage(storage, Box::new(SharedProbe(Arc::clone(probe))));
    (core, guard)
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

/// provider を通る形（`(ラベル, 末尾句, cosine か, LIMIT)`）。WHERE は呼び出し側が足す。
const PROVIDER_SHAPES: &[(&str, &str, bool, usize)] = &[
    (
        "knn_neg_inner_limit3",
        "ORDER BY embedding <#> '[1.0,0.0]' LIMIT 3",
        false,
        3,
    ),
    (
        "knn_cosine_limit3",
        "ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
        true,
        3,
    ),
    // 全件（5 行・10 行）より大きい LIMIT。
    (
        "knn_neg_inner_limit_over_all",
        "ORDER BY embedding <#> '[1.0,0.0]' LIMIT 1000",
        false,
        1000,
    ),
    (
        "knn_cosine_limit_over_all",
        "ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1000",
        true,
        1000,
    ),
];

/// fixture（`embedding_of`）から導いた期待 id 列（クエリ `[1,0]`。alice の通常 10 行は id = 10, ..., 100）。
/// 負の内積: `i=7`（[3e38,3e38]。内積が最大）、`i=1,3,9`（内積 1）、残り（内積 0。同点は id 昇順）。
const NEG_INNER_ALL: [u64; 10] = [70, 10, 30, 90, 20, 40, 50, 60, 80, 100];
/// コサイン距離: `i=1,3,9`（0）、偶数 `i`（1。同点は id 昇順）、NaN の 2 行（ゼロベクトル `i=5` と、f32 のノルム²が
/// 桁あふれして非有限になる `i=7`）は末尾で id 昇順。
const COSINE_ALL: [u64; 10] = [10, 30, 90, 20, 40, 60, 80, 100, 50, 70];
/// `kind = 'a'`（偶数 `i`）は全行が `[0,1]` で、どちらの演算子でも完全同点（id 昇順）。
const MASKED_ALL: [u64; 5] = [20, 40, 60, 80, 100];

/// 期待する id 列（順序つき）。`limit` が全件より大きければ全件。
fn expected_ids(all: &[u64], limit: usize) -> Vec<u64> {
    all.iter().copied().take(limit).collect()
}

/// マスクで切り替わる形。同じ表・同じ文の 1 回目（恒等列の入力）と 2 回目（subset の入力）が一致する。
#[test]
fn sql3220_mask_path_matches_identity_path_bitwise() {
    for (label, tail, cosine, limit) in PROVIDER_SHAPES {
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
        // fixture から導いた期待 id（順序つき・件数つき）を直接固定する。
        let want = expected_ids(&MASKED_ALL, *limit);
        assert_eq!(ids_of(&cold), want, "{label}: cold ids");
        assert_eq!(ids_of(&hot), want, "{label}: hot ids");
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
    for (label, tail, cosine, limit) in PROVIDER_SHAPES {
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
        let all = if *cosine { &COSINE_ALL } else { &NEG_INNER_ALL };
        let want = expected_ids(all, *limit);
        assert_eq!(ids_of(&fast), want, "{label}: fast ids");
        assert_eq!(ids_of(&slow), want, "{label}: slow ids");
        // NULL 行（id 5・55）は密側の結果に出ない。
        assert!(!ids_of(&slow).contains(&5) && !ids_of(&slow).contains(&55));
    }
}

/// 厳密全走査（`DESC`・`LIMIT` 省略）の、正常入力での経路間の結果一致。provider は通らない（呼び出し 0 件）。
/// マスク側はカウンタで、NULL 側は NULL 行の有無で従来経路に入ったことを示す。厳密全走査は provider を
/// 介さず（`Probe` で候補外 id を返させる手段が無い）、候補外 id の拒否分岐（`dense_id_counts.contains`）は
/// 本テストでは検証しない（その分岐の拒否は固定していない）。
#[test]
fn sql3220_exact_scan_results_match_across_paths() {
    // （末尾句, マスク側の期待 id, 全件側の期待 id）。fixture から導く: 降順は距離の大きい順で、
    // 負の内積は内積 0 の 6 行（偶数 `i` と `i=5`）が先頭の同点（id 昇順）、コサインは NaN の 2 行
    // （`i=5`・`i=7`）が最大扱いで先頭、次に距離 1 の偶数 `i`、最後に距離 0 の `i=1,3,9`。
    // `LIMIT` 省略は昇順の全件。
    let shapes: [(&str, Vec<u64>, Vec<u64>); 3] = [
        (
            "ORDER BY embedding <#> '[1.0,0.0]' DESC LIMIT 4",
            vec![20, 40, 60, 80],
            vec![20, 40, 50, 60],
        ),
        (
            "ORDER BY embedding <=> '[1.0,0.0]' DESC LIMIT 1000",
            MASKED_ALL.to_vec(),
            vec![50, 70, 20, 40, 60, 80, 100, 10, 30, 90],
        ),
        (
            "ORDER BY embedding <#> '[1.0,0.0]'",
            MASKED_ALL.to_vec(),
            NEG_INNER_ALL.to_vec(),
        ),
    ];
    for (tail, want_masked, want_all) in shapes {
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
        assert_eq!(ids_of(&cold), want_masked, "{tail}: cold ids");
        assert_eq!(ids_of(&hot), want_masked, "{tail}: hot ids");

        // NULL 行。
        let q = |t: &str| format!("SELECT {COLS} FROM {t} {tail}");
        let fast = run(&core, &q("plain")).unwrap_or_else(|e| panic!("{tail}: {e:?}"));
        let slow = run(&core, &q("nulls")).unwrap_or_else(|e| panic!("{tail}: {e:?}"));
        assert!(probe.take().is_empty());
        assert_eq!(fingerprint(&fast), fingerprint(&slow), "{tail}");
        assert_eq!(ids_of(&fast), want_all, "{tail}: fast ids");
        assert_eq!(ids_of(&slow), want_all, "{tail}: slow ids");
        assert_no_other_tenant(&slow, &q("nulls"));
    }
}

/// 空集合（該当なし。空マスクの経路に入ったことをカウンタで固定）と `LIMIT 0`（`22000` の直接固定）。
/// 検証 variant の比較ではなく、空結果と列メタの一致を固定する（候補 0 件・k=0 は検証に届かない場合がある）。
#[test]
fn sql3220_empty_results_match_across_paths() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-empty",
        &probe,
        &[("docs", &[]), ("plain", &[]), ("nulls", &[5, 55])],
    );
    // 該当なし（2 回目だけ空マスクの経路を通る）。`index_mask_scans` が 1 回目は不変・2 回目だけ +1。
    let none = format!(
        "SELECT {COLS} FROM docs WHERE kind = 'zzz' ORDER BY embedding <#> '[1.0,0.0]' LIMIT 5"
    );
    let c_init = core.scalar_index_cache_stats().index_mask_scans;
    let cold = run(&core, &none).expect("cold none");
    let c0 = core.scalar_index_cache_stats().index_mask_scans;
    let hot = run(&core, &none).expect("hot none");
    let c1 = core.scalar_index_cache_stats().index_mask_scans;
    assert_eq!(c0, c_init, "first run must not use the mask path");
    assert_eq!(c1, c0 + 1, "second run must consume the empty mask path");
    assert!(cold.rows.is_empty() && hot.rows.is_empty());
    assert_eq!(fingerprint(&cold), fingerprint(&hot));

    // k = 0 の `LIMIT 0` は束縛が範囲外として拒否する（検証へ届かない）。経路によらず同じ拒否になる。
    for op in ["<#>", "<=>"] {
        let q =
            |t: &str| format!("SELECT {COLS} FROM {t} ORDER BY embedding {op} '[1.0,0.0]' LIMIT 0");
        let fast = run(&core, &q("plain")).expect_err("fast limit 0");
        let slow = run(&core, &q("nulls")).expect_err("slow limit 0");
        // 期待する SQLSTATE を直接固定する（`sql/parser.rs` の LIMIT 範囲検査。InvalidInput = 22000）。
        for e in [&fast, &slow] {
            assert_eq!(e.wire_code(), "22000", "{op}");
            match e {
                SqlSurfaceError::InvalidInput { detail } => {
                    assert!(detail.starts_with("LIMIT 0 out of range"), "{op}: {detail}")
                }
                other => panic!("{op}: expected InvalidInput, got {other:?}"),
            }
        }
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

/// NULL ベクトル行とマスクが重なる形。WHERE に一致する NULL 行（id 25・65）と、一致しない非 NULL 行
/// （id 10・30・50・70・90）を持つ表で、実行器の `dense_subset(kept)` 分岐（NULL の除外とマスクの積集合）を通す。
/// fixture のスロット（id 昇順）: 5→0, 10→1, 20→2, 25→3, 30→4, 40→5, 50→6, 55→7, 60→8, 65→9, 70→10,
/// 80→11, 90→12, 100→13（14 行）。`kind = 'a'` は id 20・25・40・60・65・80・100（NULL は 25・65）。
/// - 索引の構築前（1 回目）: 縮約 arena（7 行: 20,25,40,60,65,80,100 → スロット 0..6。NULL は 1・4）なので
///   候補は dense の `[0,2,3,5,6]`（行列 7 行）。
/// - 構築後（2 回目）: マスク `kept = [2,3,5,8,9,11,13]` から NULL（3・9）を除いた `[2,5,8,11,13]`（行列 14 行）。
///
/// どちらも結果は NULL 行・非一致行を含まない id 20,40,60,80,100（全行が同点なので id 昇順）。
#[test]
fn sql3220_null_rows_and_mask_overlap_intersect_correctly() {
    for (label, tail, cosine, limit) in PROVIDER_SHAPES {
        let probe = Arc::new(Probe::default());
        let (core, _g) = open("sql3220-overlap", &probe, &[("both", &[5, 25, 55, 65])]);
        let sql = format!("SELECT {COLS} FROM both WHERE kind = 'a' {tail}");
        let c_init = core.scalar_index_cache_stats().index_mask_scans;
        let cold = run(&core, &sql).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let cold_calls = probe.take();
        let c0 = core.scalar_index_cache_stats().index_mask_scans;
        let hot = run(&core, &sql).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let hot_calls = probe.take();
        let c1 = core.scalar_index_cache_stats().index_mask_scans;

        assert_eq!(c0, c_init, "{label}: first run must not use the mask path");
        assert_eq!(c1, c0 + 1, "{label}: second run must use the mask path");
        assert_eq!(
            cold_calls,
            vec![Call::Subset {
                cosine: *cosine,
                slots: vec![0, 2, 3, 5, 6],
                rows: 7
            }],
            "{label}: cold"
        );
        assert_eq!(
            hot_calls,
            vec![Call::Subset {
                cosine: *cosine,
                slots: vec![2, 5, 8, 11, 13],
                rows: 14
            }],
            "{label}: hot"
        );
        let want = expected_ids(&MASKED_ALL, *limit);
        assert_eq!(ids_of(&cold), want, "{label}: cold ids");
        assert_eq!(ids_of(&hot), want, "{label}: hot ids");
        assert_eq!(fingerprint(&cold), fingerprint(&hot), "{label}");
        assert_no_other_tenant(&hot, &sql);
    }
}

/// 上のテストの拒否側。温まった経路（マスク `kept` から NULL 行を除いた候補）で、返却のうち既存の NaN ヒット
/// 1 件（WHERE に一致するゼロベクトル行 id 45）の id を候補外へ置き換える（足さない。件数・距離順・NaN 末尾・
/// 重複なしの契約は保つ）。置き換え先は
/// (a) WHERE に一致する NULL 行（id 25・65。マスクは通るが NULL 除外で落ちるべき）、
/// (b) WHERE に一致しない非 NULL 行（id 10・30。NULL 除外は通るがマスクで落ちるべき）。
/// 照合先がマスクだけ・NULL 除外だけに退行すると、(a)・(b) のどちらかが受理される。kNN のコサインアームには NaN
/// 除去が無いので、受理されれば捏造した行を結果へ返して成功するか、後段の別のエラーで落ちて detail が変わる。
/// どちらも本テストの `Err` かつ固定 detail の照合が落ちる。
/// fixture のスロット（id 昇順 18 行。110・120・130 は一致しない行でスロット 15・16・17）: 5→0, 10→1, 20→2, 25→3, 30→4, 40→5, 45→6, 50→7, 55→8, 60→9, 65→10,
/// 70→11, 80→12, 90→13, 100→14。マスク `kept = [2,3,5,6,9,10,12,14]` から NULL（3・10）を除いた
/// `[2,5,6,9,12,14]` が候補。
#[test]
fn sql3220_null_rows_and_mask_overlap_reject_forged_ids() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open(
        "sql3220-overlap-forge",
        &probe,
        &[("both_nan", &[5, 25, 55, 65])],
    );
    let sql = format!(
        "SELECT {COLS} FROM both_nan WHERE kind = 'a' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1000"
    );
    // 1 回目で索引を作り、2 回目（温まった経路）の正常結果を対照にする。
    run(&core, &sql).expect("build index");
    probe.take();
    probe.take_ks();
    let before = core.scalar_index_cache_stats().index_mask_scans;
    let honest = run(&core, &sql).expect("hot honest");
    assert_eq!(core.scalar_index_cache_stats().index_mask_scans, before + 1);
    let dense_call = Call::Subset {
        cosine: true,
        slots: vec![2, 5, 6, 9, 12, 14],
        rows: 18,
    };
    assert_eq!(probe.take(), vec![dense_call.clone()]);
    // 距離 1 の行は id 昇順、NaN のゼロベクトル行（id 45）は末尾。
    assert_eq!(ids_of(&honest), vec![20, 40, 60, 80, 100, 45]);
    let honest_ks = probe.take_ks();
    assert!(
        !honest_ks.is_empty() && honest_ks.iter().all(|&(k, n)| n <= k),
        "{honest_ks:?}"
    );

    let cases: [(&str, u64); 4] = [
        ("(a) matching NULL row id 25 (slot 3)", 3),
        ("(a) matching NULL row id 65 (slot 10)", 10),
        ("(b) non-matching non-NULL row id 10 (slot 1)", 1),
        ("(b) non-matching non-NULL row id 30 (slot 4)", 4),
    ];
    for (why, forged_slot) in cases {
        *probe.replace_nan.lock().expect("lock") = Some(forged_slot);
        let res = run(&core, &sql);
        *probe.replace_nan.lock().expect("lock") = None;
        assert_eq!(probe.take(), vec![dense_call.clone()], "{why}: call");
        assert_eq!(probe.take_ks(), honest_ks, "{why}: k and count");
        match res {
            Err(SqlSurfaceError::Internal { detail }) => {
                assert_eq!(detail, TOP_K_CONTRACT_DETAIL, "{why}")
            }
            other => panic!("{why}: must be rejected: {other:?}"),
        }
    }
}

/// 式のオーバーフロー（`n = i32::MAX` の行（id 20）を `n + 1` で投影）は、経路によらず同じ SQLSTATE で拒否される。
/// 被演算子（INTEGER 列。`numeric_scalar_from_ref` は `i32` を無条件で f64 へ運ぶ）は正常に読めて、加算結果
/// 2147483648 が投影の INTEGER 型へ戻す `sql/exec.rs` の `scalar_cell_for`（Integer アーム）で `22003` になる。
/// 同じ行を `n - 1` で投影すると成功する対照で、読み出し段の拒否ではないことを固定する。
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
    let masked = format!("SELECT id, n + 1 FROM docs WHERE kind = 'a' {tail}");
    let cold = code(&masked);
    let cold_calls = probe.take();
    let hot = code(&masked);
    let hot_calls = probe.take();
    let fast = code(&format!("SELECT id, n + 1 FROM plain {tail}"));
    let fast_calls = probe.take();
    let slow = code(&format!("SELECT id, n + 1 FROM nulls {tail}"));
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
    // 対照: 同じ行の `n - 1` は成功する（被演算子の読み出しは通る）。
    let ok = run(
        &core,
        &format!("SELECT id, n - 1 FROM plain WHERE kind = 'a' {tail}"),
    )
    .expect("n - 1 succeeds");
    assert!(ok.rows.iter().any(|r| r.id == 20
        && r.cells
            .contains(&Cell::SignedInteger(i64::from(i32::MAX) - 1))));
    probe.take();
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
    // 密側アダプタは NaN 距離の行（`i=5` のゼロベクトルと `i=7` の桁あふれ）を除くので、融合結果は
    // alice の通常行から id 50・70 を除いた 8 行（NULL 行・他テナントの行は無い）。
    for r in [&fast, &slow] {
        let mut ids = ids_of(r);
        ids.sort_unstable();
        assert_eq!(ids, vec![10, 20, 30, 40, 60, 80, 90, 100]);
    }
    assert_eq!(fingerprint(&fast), fingerprint(&slow));
    assert_no_other_tenant(&slow, &sql("nulls"));
}

/// 密側アダプタ（`CosineDenseAdapter`）は kNN 本体とは別に、渡した候補（`dense_slots`）で返却 id を検証する。
/// NULL 行のある hybrid で、provider の返却のうち既存の NaN ヒット 1 件の id を候補外（NULL 行のスロット
/// 0・6・範囲外 12）へ置き換える（足さない）。置き換えなので返却件数は正常時と同じで（`n <= k` も正常時と
/// 同じ値で成立）、距離昇順・NaN 末尾・重複なしも保つ。正常時と同じ件数・順序の入力で所属検査だけを
/// 確かめるため、足すのではなく置き換える。所属検査（`CosineDenseAdapter::search` の `distance_result_is_valid_with` による検証）を外すと、置き換えた NaN ヒットは
/// 直後の NaN 除去（同関数の `filter(|h| !h.distance.is_nan())`）で他の NaN ヒットと一緒に消え、問い合わせは成功する入力になる。
/// つまり拒否は所属検査だけが生む。アダプタは `KernelError::WorkerPanicked` を返し、融合段の
/// エラー写像（`sql/exec.rs` の hybrid エラー写像の既定アーム）が固定 detail `"hybrid search error"` の `Internal` に丸める。
/// この detail は他の融合段の失敗とも共通なので、拒否が「アダプタが inner を呼んだ後の検証」で起きた
/// ことは provider 呼び出し（`Subset` と dense スロット列）で特定する。
#[test]
fn sql3220_hybrid_dense_adapter_rejects_forged_ids() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open("sql3220-hybrid-forge", &probe, &[("nulls", &[5, 55])]);
    let sql = "SELECT id, kind FROM nulls \
               ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'a') LIMIT 20";
    // 正常時は通る（検証が拒否の原因になっていないことの対照）。
    run(&core, sql).unwrap_or_else(|e| panic!("hybrid normal: {e:?}"));
    probe.take();
    // 正常時の `(k, 返却件数)`。置き換えは件数を変えないので、捏造時も同じ値で `返却件数 <= k` が保たれる。
    let honest_ks = probe.take_ks();
    assert!(
        !honest_ks.is_empty() && honest_ks.iter().all(|&(k, n)| n <= k),
        "{honest_ks:?}"
    );
    for forged in [0_u64, 6, 12] {
        *probe.replace_nan.lock().expect("lock") = Some(forged);
        let res = run(&core, sql);
        *probe.replace_nan.lock().expect("lock") = None;
        assert_eq!(
            probe.take(),
            vec![Call::Subset {
                cosine: true,
                slots: vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11],
                rows: 12
            }],
            "forged {forged}: provider call"
        );
        assert_eq!(probe.take_ks(), honest_ks, "forged {forged}: k and count");
        match res {
            Err(SqlSurfaceError::Internal { detail }) => {
                assert_eq!(detail, "hybrid search error", "forged {forged}")
            }
            other => panic!("forged {forged} must be rejected: {other:?}"),
        }
    }
}

/// 上のテストの恒等候補列版（NULL 行なしの表。`dense_slots == None`）。アダプタが所属検査を `dense_slots`
/// ありのときだけ行う退行を検出するため、provider 呼び出しが `Full`（恒等 0..10）であることと、既存の NaN
/// ヒットを範囲外のスロット（10・12）へ置き換えた返却（件数・順序・重複の契約は満たす）が同じ detail で
/// 拒否されることを固定する。所属検査を外すと NaN 除去（`CosineDenseAdapter::search` の `filter`）で消えて成功する入力である。
#[test]
fn sql3220_hybrid_dense_adapter_rejects_forged_ids_on_identity_candidates() {
    let probe = Arc::new(Probe::default());
    let (core, _g) = open("sql3220-hybrid-forge-id", &probe, &[("plain", &[])]);
    let sql = "SELECT id, kind FROM plain \
               ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'a') LIMIT 20";
    run(&core, sql).unwrap_or_else(|e| panic!("hybrid normal: {e:?}"));
    probe.take();
    // 正常時の `(k, 返却件数)`。置き換えは件数を変えないので、捏造時も同じ値で `返却件数 <= k` が保たれる。
    let honest_ks = probe.take_ks();
    assert!(
        !honest_ks.is_empty() && honest_ks.iter().all(|&(k, n)| n <= k),
        "{honest_ks:?}"
    );
    for forged in [10_u64, 12] {
        *probe.replace_nan.lock().expect("lock") = Some(forged);
        let res = run(&core, sql);
        *probe.replace_nan.lock().expect("lock") = None;
        assert_eq!(
            probe.take(),
            vec![Call::Full {
                cosine: true,
                ids: (0..10).collect(),
                rows: 10
            }],
            "forged {forged}: provider call"
        );
        assert_eq!(probe.take_ks(), honest_ks, "forged {forged}: k and count");
        match res {
            Err(SqlSurfaceError::Internal { detail }) => {
                assert_eq!(detail, "hybrid search error", "forged {forged}")
            }
            other => panic!("forged {forged} must be rejected: {other:?}"),
        }
    }
}

/// 最適化の対象外の形（NULL 行あり・マスクで非恒等）でも従来経路のまま fail-closed に動く。
/// 次の 2 つは別々の保証として固定する。
/// - 返却値の検証: provider が候補外のスロット（可視 arena 内の NULL 行・マスク外のスロット、範囲外のスロット）を
///   返したら、結果を 1 行も返さず `XX000` で拒否する。恒等列の入力でも範囲外は拒否される。拒否は kNN 本体の
///   返却値検証の固定 detail と provider 呼び出しで特定する。他テナントの行を指す捏造は扱わない
/// - テナント境界: 正常な検索の結果に他テナント（`BOB_IDS`）の Private 行が出ない
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
            let mut ids = ids_of(&ok);
            ids.sort_unstable();
            let want: Vec<u64> = if sql == &masked {
                MASKED_ALL.to_vec()
            } else {
                (1..=10).map(|i| i * 10).collect()
            };
            assert_eq!(ids, want, "{label}: {sql}");
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
            probe.take_ks();
            *probe.forge.lock().expect("lock") = Some(forged);
            let res = run(&core, sql);
            *probe.forge.lock().expect("lock") = None;
            let calls = probe.take();
            // 件数の契約は満たす（捏造後の返却件数 <= 要求 k。`LIMIT 100` の k は 10〜12 行より大きい）。
            let ks = probe.take_ks();
            assert!(
                !ks.is_empty() && ks.iter().all(|&(k, n)| n <= k),
                "{label} {why}: {ks:?}"
            );
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
