//! `sql::exec` の SCALAR 段（`WHERE` の等価・前方一致・`id` 単純比較条件）が
//! 全行走査（O(N)）の代わりに参照する、スカラー列二次索引の**構築・テーブル
//! 世代整合キャッシュ・候補削減 API**（構築・キャッシュは Issue #473、候補削減
//! への結線は Issue #474。対応 ADR: `docs/design/scalar-secondary-index.md`。
//! 親 Issue #359・#472。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-12・`docs/spec/04-behavior/rls.md`）。
//!
//! **消費経路**: 本モジュールの照会 API（[`ScalarIndex::candidates_for`]・
//! [`ScalarIndex::candidates_id_range`]・[`ScalarIndex::resolve_candidates`]）は
//! `sql::exec::execute_statement_with_cache` の SCALAR 事前フィルタから、索引
//! 対応述語（`sql::scalar_plan::classify_scalar_plan` が `PlainScan` 以外へ
//! 分類した形状。TEXT 列の等価・前方一致・`id` の単純比較とその組合せ）を持つ
//! クエリに限って消費される（詳細は `docs/design/scalar-index-prune.md` 参照）。
//! 索引は候補スロットを「絞る」ことしかできず「通す」ことはできないため、
//! 候補行にも `on_visible_row`（`matches_all`＋式述語）が引き続き適用される。
//!
//! **データモデル**（[`ScalarIndex`]）: 構築元は
//! [`crate::sql::arena_cache::SqlArenaSnapshot`]（RLS 段適用済み・ctx 可視行の
//! みを含むスナップショット）。索引のスロット番号は**このスナップショットの
//! スロット**（`snapshot.arena().ids()[slot]`／`snapshot.metadata()[slot]` の
//! 添字）であり、クエリごとに異なる SCALAR 段適用後アリーナのスロットではない
//! （#474 はスナップショット経由でこの写像を扱う）。`TEXT` 列ごとに値の辞書
//! （バイト列昇順）と、値ごとの一致スロット列（CSR: `offsets`/`slots`）・
//! 等価直引き用 `HashMap` を持つ。加えて全行 `id` が [`crate::sql::udf_call::
//! id_as_finite_scalar`] を満たす場合に限り、`id` 昇順の順序索引（`id_index`）を
//! 保持する（1 件でも `id > 2^53` があれば `None`。fail-closed。#474 が全走査へ
//! 縮退する契機になる）。`NULL` 値はいずれの索引にもエントリを作らない
//! （`declarative_filter::MetadataFilter::matches` の NULL 常時不一致と同じ
//! 判定になることが本モジュールの単体テストの不変条件）。**列単位の索引対象
//! 除外**（Issue #632）: `TEXT` 列は平均値長（非 `NULL` 値の累積バイト量 ÷
//! 件数）が [`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`] を超えた時点で、それ以降
//! 一切複製・索引化されず `ColumnType::Vector` の列と同じ「未索引」
//! （`columns[i] = None`）へ合流する。これは fail-closed（構築全体の失敗）
//! ではなく列単位の fail-soft な縮退であり、[`ScalarIndex::candidates_for`]・
//! [`ScalarIndex::column_groups`] は除外列に対して常に `None` を返し、
//! 呼び出し元（`sql::exec`・`sql::aggregate`・`sql::group_by`）は既存の
//! 「列が索引未対応」契約のまま plain scan へ縮退する（呼び出し側 3 箇所は
//! 無変更）。長文自由記述列（例: crossdb fixture の `body`）を索引化すると
//! 索引が肥大化しヒット後のクエリのアロケータ状態が悪化する実測
//! （Issue #632 本文）を踏まえた対策で、短い分類値の列（例: `lang`/`topic`）は
//! 従来どおり索引化される。
//!
//! **キャッシュ（[`ScalarIndexCache`]）**: キー・世代源泉・fail-closed 契約は
//! [`crate::sql::arena_cache::SqlArenaCache`]（Issue #363）と同型
//! （`(table, ctx)` 完全一致 × テーブル単位世代
//! `catalog::table_generation_in_txn`）。ただし [`ScalarIndexCache::insert`] は
//! **`SqlArenaCache::insert` とは意図的に非対称**で、`core.rs::PrefilterCache::
//! insert`（Issue #280）と同じく世代不一致・ロック毒化・世代読み取り失敗を
//! すべて `None` として扱い、キャッシュへ反映しないだけでなく呼び出し元へも
//! 一切渡さない（本索引はまだ誰にも消費されない派生データであり、`SqlArenaCache`
//! のように「このクエリの応答に限って stale でも使ってよい」対象が存在しない
//! ため。挿入失敗時は呼び出し元が単に索引なしとして扱う）。
//!
//! **fail-closed の適用範囲**: RLS 可視行のみから構築するため候補は構造的に
//! 可視集合の部分集合になる（TABLE-12・RLS 系ポインタ）。構築失敗・容量超過・
//! `id` 桁あふれはいずれも「索引なし」への縮退であり、クエリの成否・結果には
//! 影響しない（本モジュールは fail-soft な派生キャッシュ）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::catalog::{ColumnType, TableSchema};
use crate::declarative_filter::{FilterOp, MetadataFilter};
use crate::policy::PolicyContext;
use crate::row_codec::{scan_scalar_columns, ScalarRef};
use crate::sql::arena_cache::SqlArenaSnapshot;
use crate::storage::Storage;

/// [`ScalarIndex::resolve_candidates`] の選択度切替閾値（Issue #474）。候補比
/// （交差後の候補数 ÷ 索引行数）が `numerator / denominator` を超える場合、
/// 索引経路より全走査（`O(N)`。候補列挙・交差のオーバーヘッドを持たない）が
/// 有利と判断し [`CandidateResolution::FallbackSelectivity`] へ縮退する。
/// 暫定既定値は 1/2（根拠は `docs/design/scalar-index-prune.md`「選択度切替の
/// 既定値」節。索引経路のコストは概ね `O(|hits|)`、全走査は `O(N + |hits|)`
/// のため損益分岐は `hits ≈ N` 近傍にしかなく、`sql::hnsw_cache` の ANN 先例
/// （1/10）をそのまま採ると本ユースケースの主要フェーズが索引経路に乗らず
/// 受入条件が vacuous になるため、より緩い閾値を採用する）。
const DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_NUMERATOR: u64 = 1;
const DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_DENOMINATOR: u64 = 2;

/// [`ScalarIndex::build`] が `TEXT` 列を索引対象から除外する平均値長の閾値
/// （バイト。本リポジトリの実装既定値・spec 非関与。Issue #632）。
///
/// crossdb fixture（25,000 行・`lang`/`topic`/`body`）では、長文自由記述列
/// `body` まで無条件に索引化すると索引が約 8MiB に達し、以降の hybrid
/// クエリのアロケータ状態が悪化して p50 が約 10% 劣化することが実測で
/// 判明した（`body` を索引対象から外した実験ビルドで回復を確認済み。
/// WHERE を伴わないクエリでは差がない）。本定数は列ごとの平均値長
/// （非 `NULL` 値の累積バイト量 ÷ 件数。行数ではなく実際に索引化を試みた
/// 非 `NULL` 値の件数を分母にすることで、NULL の多い疎な列を不当に除外
/// しない）がこの値を超えた時点でその列を索引対象から除外する。
///
/// `lang`/`topic`（短い分類値）と `body`（長文）を分離する閾値として #632
/// では暫定 128 を採用したが、#633 の crossdb 前後比較実測で `body` 列の
/// 全体平均が 126.3 バイトと判明し、128 では実質的にゲートが発火せず
/// （RSS 実測が不変）対策が no-op になっていたことが確定した。これを受け
/// オーナー判断（2026-09-08）で **64** へ引き下げる。判定基準（平均値長。
/// 行単位の累積平均であり、値の並び順によっては先頭の数行時点で既に
/// 累積平均が閾値を超えて以降の全値が除外され得る）自体は変更しない。
/// 64 であれば `lang`（数バイト）/`topic`（十数バイト）は引き続き索引
/// 対象に残り、crossdb `body` 相当（100 バイト超）は確実に除外される。
/// crossdb fixture での RSS 低下・`hybrid_rrf` p50 回復の前後比較実測は
/// 本変更（Issue #644）の対象外・後続 Issue の担当である。
const MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN: usize = 64;

/// [`ScalarIndexCache`] のエントリ数上限（`sql::arena_cache::SqlArenaCache`・
/// `core.rs::PrefilterCache` と同じ DoS 対策方針を踏襲する）。
const MAX_SCALAR_INDEX_CACHE_ENTRIES: usize = 32;

/// [`ScalarIndexCache`] が保持する索引群の概算バイト量の合計上限
/// （`crate::arena::MAX_ARENA_TOTAL_BYTES` と同じ桁に揃える）。
const MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// 単体の [`ScalarIndex`] が超えてはならない概算バイト量上限。総量上限と同じ値を
/// 採用する（1 テーブル分の索引が総予算を単独で使い切る事態を許すが、複数
/// テーブル・複数 ctx 分を同時に常駐させない選択と両立する。`core.rs::
/// PrefilterCache::insert` の単体上限判定と同じ考え方）。
const MAX_SCALAR_INDEX_BYTES: usize = MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES;

/// [`ScalarIndex::build`] の失敗要因。いずれも呼び出し元（`sql::exec`）が
/// 「索引なし」へ縮退する契機として扱うのみで、クエリ自体を失敗させない
/// （モジュールドキュメント参照）。
#[derive(Debug)]
pub(crate) enum ScalarIndexBuildError {
    /// スロット番号が `u32` に収まらない（[`crate::arena::MAX_ARENA_ROWS`] は
    /// `u32::MAX` 未満のため通常到達しないが、多層防御として検査する）。
    SlotOverflow,
    /// untrusted な行 metadata のデコードに失敗した（`scan_scalar_columns` が
    /// 検出する presence タグ不正・宣言長超過・UTF-8 不正等）。
    RowDecode(crate::row_codec::RowCodecError),
    /// アロケーション失敗（`try_reserve` 系）。
    AllocationFailed,
    /// 構築結果の概算バイト量が [`MAX_SCALAR_INDEX_BYTES`] を超えた。
    TooLarge,
}

impl std::fmt::Display for ScalarIndexBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlotOverflow => write!(f, "scalar index slot count exceeds u32 range"),
            Self::RowDecode(e) => write!(f, "scalar index row decode failed: {e}"),
            Self::AllocationFailed => write!(f, "scalar index allocation failed"),
            Self::TooLarge => write!(
                f,
                "scalar index approx size exceeds limit {MAX_SCALAR_INDEX_BYTES}"
            ),
        }
    }
}

impl std::error::Error for ScalarIndexBuildError {}

/// `s` の複製を、失敗しうるアロケーションとして構築する（codex-review P1
/// 対応・PR #569。`String::to_string`/`String::clone` はグローバルアロケータの
/// infallible な成長経路を辿り、失敗時は [`ScalarIndexBuildError::AllocationFailed`]
/// へ変換されずプロセスを異常終了させ得るため、`try_reserve_exact` で確保して
/// から `push_str` する明示的に fallible な経路に置き換える）。
fn try_owned_string(s: &str) -> Result<String, ScalarIndexBuildError> {
    let mut out = String::new();
    out.try_reserve_exact(s.len())
        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
    out.push_str(s);
    Ok(out)
}

/// 1 件の文字列値を索引へ追加する際の概算バイト量（[`TextColumnIndex::
/// approx_heap_bytes`] の `values`/`equality` の計上方法と揃える）。
fn approx_string_entry_bytes(s: &str) -> usize {
    s.len().saturating_add(std::mem::size_of::<String>())
}

/// 構築中の索引が確保しようとしている概算バイト量が [`MAX_SCALAR_INDEX_BYTES`]
/// を超えないことを、実際に文字列を複製する（`to_string`/`clone` で新規確保が
/// 起こる）**前**に検証する（codex-review P1 対応・PR #569。全構築完了後の
/// [`ScalarIndex::approx_heap_bytes`] 判定だけでは、長い異なる `TEXT` 値を多数
/// 含むスナップショットに対して判定前に上限超過分のメモリを確保してしまう）。
/// `running` は呼び出し元が管理する累計カウンタで、本関数が返す `Ok` を受けて
/// 呼び出し元が `additional` 分だけ加算する契約とする。
fn check_scalar_index_budget(
    running: usize,
    additional: usize,
) -> Result<(), ScalarIndexBuildError> {
    if running.saturating_add(additional) > MAX_SCALAR_INDEX_BYTES {
        return Err(ScalarIndexBuildError::TooLarge);
    }
    Ok(())
}

/// `values`/`offsets`/`slots`/`equality`（[`TextColumnIndex`] の 4 配列）を
/// 重複排除前の `pair_count` 件分だけ一括で `try_reserve_exact` する際に、
/// **実際に確保される**構造体サイズ分のバイト量（codex-review P1 対応・
/// PR #569 未解決分。`values: Vec<String>` 等の容量は `String`/`u32`/
/// `(String, u32)` の構造体サイズ分だけ確保され、その中身（文字列バイト列）は
/// 別途 [`approx_string_entry_bytes`] で行走査時に予算検証・計上済みだが、
/// 高重複 TEXT 列では重複排除後の実要素数（[`TextColumnIndex::approx_heap_bytes`]
/// が計上する対象）が `pair_count` を大幅に下回り、この**確保容量そのもの**が
/// 予算計上から漏れて `MAX_SCALAR_INDEX_BYTES` を実質バイパスし得た。呼び出し元は
/// 4 配列を確保する**前**に本関数の結果を [`check_scalar_index_budget`] で検証し、
/// 通れば `running` へ加算する契約とする）。
fn text_column_reservation_bytes(pair_count: usize) -> usize {
    let values_bytes = pair_count.saturating_mul(std::mem::size_of::<String>());
    let offsets_bytes = pair_count
        .saturating_add(1)
        .saturating_mul(std::mem::size_of::<u32>());
    let slots_bytes = pair_count.saturating_mul(std::mem::size_of::<u32>());
    // `equality: HashMap<String, u32>` は `pair_count` 件分の
    // `try_reserve(pair_count)` を行うが、実際に確保されるバケット数は
    // `pair_count` そのものではなく、hashbrown の負荷率・2 のべき乗丸めを
    // 経た `hashmap_bucket_count(pair_count)` 件（[`hashmap_reservation_bytes`]
    // 参照。codex-review P1 対応・PR #569 未解決分）。
    let equality_bytes = hashmap_reservation_bytes(pair_count);
    values_bytes
        .saturating_add(offsets_bytes)
        .saturating_add(slots_bytes)
        .saturating_add(equality_bytes)
}

/// `std::collections::HashMap`（hashbrown 実装）が最低 `min_capacity` 件を
/// 保持できるよう確保する**実バケット数**の見積り（codex-review P1 対応・
/// PR #569。hashbrown v0.15.5 の `RawTableInner::fallible_with_capacity` /
/// `capacity_to_buckets`〔`raw/mod.rs`〕と同じ規則を踏襲する: 最大負荷率
/// 7/8 を満たすようバケット数を切り上げたうえで 2 のべき乗へ丸める
/// （`min_capacity < 8` は 4 または 8 に固定）。`pair_count × エントリ
/// サイズ` という単純計算では、この丸めによる追加確保
/// （例: 100 万件 → 実バケット数 2^21 ≈ 210 万）が計上から漏れ、
/// [`MAX_SCALAR_INDEX_BYTES`] の予算検査を実質バイパスし得た。
/// hashbrown の内部実装はバージョン依存で将来変わり得るため、丸め則の
/// 変化があっても過小評価側に倒れないよう本関数は意図的に保守的
/// （実バケット数以上）に倒す。
fn hashmap_bucket_count(min_capacity: usize) -> usize {
    if min_capacity == 0 {
        return 0;
    }
    if min_capacity < 8 {
        return if min_capacity < 4 { 4 } else { 8 };
    }
    let adjusted = match min_capacity.checked_mul(8) {
        Some(v) => v / 7,
        // オーバーフローする規模はそもそも `MAX_SCALAR_INDEX_BYTES` を
        // 大幅に超えるため、`usize::MAX` を返し予算検査で確実に拒否させる
        // （fail-closed）。
        None => return usize::MAX,
    };
    adjusted.next_power_of_two()
}

/// 1 バケットあたりの hashbrown 制御バイト分を含む保守的な確保バイト量
/// （codex-review P1 対応・PR #569）。hashbrown はバケット配列の直後に
/// SIMD グループ幅（実行環境依存。最大でも数十バイト程度）分の制御バイト
/// パディングを追加確保するため、その分を固定オーバーヘッドとして
/// 加算し過小評価を避ける。
const HASHBROWN_GROUP_PADDING_BYTES: usize = 32;

/// `equality: HashMap<String, u32>` が最低 `min_capacity` 件を保持できる
/// よう確保する概算バイト量（[`hashmap_bucket_count`] 参照）。
fn hashmap_reservation_bytes(min_capacity: usize) -> usize {
    let buckets = hashmap_bucket_count(min_capacity);
    if buckets == 0 {
        // `min_capacity == 0` はテーブル未確保（`HashMap::new()` 相当）で
        // 実際に確保は起こらないため、固定オーバーヘッドも計上しない。
        return 0;
    }
    // 1 バケットにつきエントリ本体（`(String, u32)`）+ 制御バイト 1。
    let per_bucket = std::mem::size_of::<(String, u32)>().saturating_add(1);
    buckets
        .saturating_mul(per_bucket)
        .saturating_add(HASHBROWN_GROUP_PADDING_BYTES)
}

/// 行走査中に `TEXT` 列ごとの作業領域 `acc: Vec<(String, u32)>`
/// （[`ScalarIndex::build`]）が確保する**タプル全体**の容量分バイト量
/// （codex-review P1 対応・PR #569 未解決分）。
///
/// 修正前は `acc` へ 1 件ずつ `try_reserve(1)` してから `push` しており、
/// 予算計上も文字列本体長＋`String` 構造体サイズ（[`approx_string_entry_bytes`]）
/// のみで、(1) タプルの `u32` 分・アラインメント詰め物と (2) `try_reserve` の
/// 内部成長戦略（要求量ちょうどではなく現容量の倍増などで余剰確保され得る）の
/// 双方が計上から漏れていた。64bit 環境で全行・全 `TEXT` 列が空文字列の場合、
/// 文字列本体バイトは 0 で予算上「ほぼ無料」に見える一方、`acc` 自体の確保
/// 容量（タプル構造体サイズ×要素数、かつ倍増成長の余剰込み）は無視できない
/// 量に達し、既存の行数・列数・スナップショット容量上限内の入力でも
/// [`MAX_SCALAR_INDEX_BYTES`] 判定より前に未検証の大容量確保が起こり得た。
///
/// 本関数は列ごとに `acc` の必要行数上限（1 行につき列あたり高々 1 値なので
/// `row_count` が上限）を**確保前に一括**でバイト量へ変換し、
/// [`check_scalar_index_budget`] で検証してから
/// `Vec::try_reserve_exact(row_count)` する契約にすることで、成長戦略の余剰
/// 確保そのものを起こさせない（倍増ではなく厳密量の 1 回確保に固定するため、
/// 見積りバイト量と実確保バイト量が一致する）。
fn per_column_accumulator_reservation_bytes(row_count: usize) -> usize {
    row_count.saturating_mul(std::mem::size_of::<(String, u32)>())
}

/// `2^53`（`f64` の仮数部が整数値を正確に表現できる上限）。`BIGINT`／
/// `TIMESTAMP` 列の順序索引化は、まだ確定していない `#891` の評価器の比較
/// 意味論（f64 経由の損失ある比較になる可能性がある）に対して保守的に倒し、
/// 絶対値がこれを超える値を 1 件でも含む列は索引全体を対象外（`None`）へ
/// 縮退する（`sql::scalar_plan::MAX_EXACT_ID`・`id_index` と同じ判断基準）。
/// `INTEGER`（`i32`）・`DATE`（`i32` の日数）はこの上限に構造的に収まるため
/// 対象外にしない。
const MAX_EXACT_TYPED_I64_ABS: u64 = 1u64 << 53;

/// `v` の絶対値が [`MAX_EXACT_TYPED_I64_ABS`] 以内かどうか。`i64::MIN` は
/// `unsigned_abs()` で安全に扱う（`i64::MIN.abs()` は overflow する）。
fn i64_within_exact_f64_range(v: i64) -> bool {
    v.unsigned_abs() <= MAX_EXACT_TYPED_I64_ABS
}

/// 数値・日時・`NUMERIC`・`UUID` 列向けの順序索引（Issue #893。モジュール
/// ドキュメント「データモデル」の `TEXT` 用 [`TextColumnIndex`] に相当する、
/// 全順序キーを持つスカラー型向けの索引本体）。各 variant は「キー昇順・
/// 同一キー内はスロット昇順」に整列した `(key, slot)` 列を保持する。
/// `INTEGER`／`BIGINT`／`DATE`／`TIMESTAMP` は `i64` へ昇格して `I64` を、
/// `REAL`／`DOUBLE` は全順序比較可能な `u64` ビット表現（[`f64_to_sortable_bits`]）
/// へ写像して `F64Sortable` を、`NUMERIC` は列固定の `scale` のもとで
/// `unscaled`（`i128`）の大小がそのまま値の大小になることを利用して `I128` を、
/// `UUID` はネットワークバイトオーダーのバイト列を `u128`（ビッグエンディアン）
/// として `U128` を、それぞれ使う。
///
/// `NULL` はいずれの variant にもエントリを作らない（`TextColumnIndex` と同じ
/// 契約）。
///
/// `I128`（`NUMERIC` 列）のみ列固定の `scale` を第 2 要素として保持する
/// （Issue #893 production 接続。`declarative_filter::TypedLiteral::Numeric`
/// が保持するリテラルは `parse_literal_exact` 由来の任意 `scale` を持つため、
/// `candidates_for` が境界計算時に列側の `scale` を必要とする——
/// `numeric::rescale_bounds_for_column` 参照）。他の variant は列型から
/// キー型が一意に決まるため付随情報を持たない。
enum OrderedColumnIndex {
    I64(Vec<(i64, u32)>),
    F64Sortable(Vec<(u64, u32)>),
    I128(Vec<(i128, u32)>, u8),
    U128(Vec<(u128, u32)>),
}

/// `f64`（有限値・`-0.0` は `+0.0` へ正規化済みである契約——`scalar_float.rs`
/// 参照）を、ビット表現の大小関係が元の数値の大小関係と一致する `u64` へ
/// 写像する。IEEE 754 の符号なし整数としてのビットパターンは、正の値同士では
/// 数値順と一致するが、符号ビット・負数側の並びは数値順と逆になるため、
/// 符号ビットが立っている（負）場合は全ビット反転、立っていない（非負）場合は
/// 符号ビットだけを立てる（標準的な「浮動小数点を符号なし整数の全順序へ写像
/// する」変換）。`NaN`・無限大は呼び出し元が事前に拒否する契約（`is_finite`
/// を満たさない値を渡すと結果は未定義の順序になり得るが panic はしない）。
fn f64_to_sortable_bits(v: f64) -> u64 {
    let bits = v.to_bits();
    if bits & (1u64 << 63) != 0 {
        !bits
    } else {
        bits | (1u64 << 63)
    }
}

/// `UUID` の 16 バイト（ネットワークバイトオーダー）を `u128`（ビッグエンディアン
/// 解釈）へ変換する。[`crate::uuid::Uuid`] の `Ord`（derive によるバイト列辞書順）
/// と同じ大小関係になる（ビッグエンディアン変換はバイト列辞書順を保つ）。
fn uuid_to_sortable_u128(bytes: &[u8; 16]) -> u128 {
    u128::from_be_bytes(*bytes)
}

/// [`ScalarIndex::candidates_typed_range`]（テスト専用。モジュール
/// ドキュメント参照）の照会に使う正規化済みキー（Issue #893）。列側の
/// variant と一致する場合に限り照会を実行する（型不一致は `None`＝
/// 索引未対応として全走査へ縮退する契機。呼び出し元はこの型不一致を
/// 「一致 0 件」と混同しない）。production 経路（`Self::candidates_for` の
/// `TypedCompare` 分岐）は `MetadataFilter::TypedCompare` を直接消費し
/// 本型へは正規化しないため、`candidates_typed_range` と同じくテスト専用
/// 到達パターンになる。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedKey {
    I64(i64),
    F64Sortable(u64),
    I128(i128),
    U128(u128),
}

/// `pairs`（キー昇順・同一キー内はスロット昇順に整列済み）から `[lower, upper]`
/// 範囲に一致する候補スロットを求める（[`ScalarIndex::candidates_id_range`]
/// と同じ二分探索による区間複製。区間内はキー昇順だがスロット昇順とは限らない
/// ため、返す前に `sort_unstable` でスロット昇順へ揃える）。
fn range_slots<K: Ord + Copy>(
    pairs: &[(K, u32)],
    lower: std::ops::Bound<K>,
    upper: std::ops::Bound<K>,
) -> Option<Vec<u32>> {
    use std::ops::Bound;
    let start = match lower {
        Bound::Unbounded => 0,
        Bound::Included(l) => pairs.partition_point(|(k, _)| *k < l),
        Bound::Excluded(l) => pairs.partition_point(|(k, _)| *k <= l),
    };
    let end = match upper {
        Bound::Unbounded => pairs.len(),
        Bound::Included(u) => pairs.partition_point(|(k, _)| *k <= u),
        Bound::Excluded(u) => pairs.partition_point(|(k, _)| *k < u),
    };
    if start >= end {
        return Some(Vec::new());
    }
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(end - start).ok()?;
    out.extend(pairs[start..end].iter().map(|(_, slot)| *slot));
    out.sort_unstable(); // sort-determinism: allow u32 スロット番号の全順序（重複なし）による昇順整列
    Some(out)
}

/// 数値・日時・`NUMERIC`・`UUID` 列 1 件分の順序索引が確保する `(key, slot)`
/// 配列の概算バイト量（`text_column_reservation_bytes` の固定長キー版。
/// これらの型は文字列のような可変長の中身を持たないため、行走査前に
/// `row_count` 件分をちょうど一括確保すれば実確保量と一致し、`TEXT` 列のような
/// 重複排除後の再確保・別建ての `equality` テーブルは不要）。
fn typed_column_reservation_bytes(row_count: usize, entry_size: usize) -> usize {
    row_count.saturating_mul(entry_size)
}

/// `value`（`scan_scalar_columns` が返した非 `NULL` のスキャン結果）から
/// `accum`（[`OrderedColumnIndex`] の作業領域。`schema` 由来の列型と対応する
/// variant が呼び出し元〔[`ScalarIndex::build`]〕によって選ばれている契約）へ
/// `(key, slot)` を 1 件追加する。
///
/// `false` を返すのは次のいずれか（呼び出し元はこれを「この列は索引対象外」
/// として列全体を `None` へ縮退する契機として扱う。fail-closed。モジュール
/// ドキュメント「`MAX_EXACT_TYPED_I64_ABS`」参照）:
/// - `BIGINT`／`TIMESTAMP` の絶対値が `2^53` を超える。
/// - `REAL`／`DOUBLE` が非有限（`scalar_float.rs` の不変条件への多層防御）。
/// - `accum` の variant と `value` の型が一致しない（構造的に到達しない
///   はずだが、untrusted な行 metadata に由来する分岐のため防御的に扱う）。
fn push_typed_value(accum: &mut OrderedColumnIndex, value: &ScalarRef<'_>, slot: u32) -> bool {
    match (accum, value) {
        (OrderedColumnIndex::I64(acc), ScalarRef::Integer(v)) => {
            acc.push((i64::from(*v), slot));
            true
        }
        (OrderedColumnIndex::I64(acc), ScalarRef::BigInt(v)) => {
            if !i64_within_exact_f64_range(*v) {
                return false;
            }
            acc.push((*v, slot));
            true
        }
        (OrderedColumnIndex::I64(acc), ScalarRef::Date(v)) => {
            acc.push((i64::from(*v), slot));
            true
        }
        (OrderedColumnIndex::I64(acc), ScalarRef::Timestamp(v)) => {
            if !i64_within_exact_f64_range(*v) {
                return false;
            }
            acc.push((*v, slot));
            true
        }
        (OrderedColumnIndex::F64Sortable(acc), ScalarRef::Real(v)) => {
            let widened = f64::from(*v);
            if !widened.is_finite() {
                return false;
            }
            acc.push((f64_to_sortable_bits(widened), slot));
            true
        }
        (OrderedColumnIndex::F64Sortable(acc), ScalarRef::Double(v)) => {
            if !v.is_finite() {
                return false;
            }
            acc.push((f64_to_sortable_bits(*v), slot));
            true
        }
        (OrderedColumnIndex::I128(acc, _scale), ScalarRef::Numeric(d)) => {
            acc.push((d.unscaled(), slot));
            true
        }
        (OrderedColumnIndex::U128(acc), ScalarRef::Uuid(u)) => {
            acc.push((uuid_to_sortable_u128(u.as_bytes()), slot));
            true
        }
        _ => false,
    }
}

/// 1 つの `TEXT` 列に対する索引（等価直引き＋前方一致範囲走査の両方を支える
/// 共有データ構造。モジュールドキュメント「データモデル」参照）。
///
/// `values`（重複排除・バイト列昇順の辞書）・`offsets`（CSR。`values[i]` の
/// 一致スロットは `slots[offsets[i]..offsets[i+1]]`。長さは `values.len() + 1`）・
/// `slots`（値ごとにスロット昇順）・`equality`（値 → `values` の添字。等価述語の
/// O(1) 直引き用）を持つ。
struct TextColumnIndex {
    values: Vec<String>,
    offsets: Vec<u32>,
    slots: Vec<u32>,
    equality: HashMap<String, u32>,
}

impl TextColumnIndex {
    /// `value_index`（`values`/`equality` の添字）に対応するスロット列（昇順）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn slots_for_value_index(&self, value_index: u32) -> Option<&[u32]> {
        let vi = usize::try_from(value_index).ok()?;
        let start = *self.offsets.get(vi)?;
        let end = *self.offsets.get(vi.checked_add(1)?)?;
        let start = usize::try_from(start).ok()?;
        let end = usize::try_from(end).ok()?;
        self.slots.get(start..end)
    }

    /// `prefix` に前方一致する全ての値のスロット列を連結して返す（値をまたぐと
    /// 昇順であることは保証しない。呼び出し元がソートする契約。モジュール
    /// ドキュメント「データモデル」・[`ScalarIndex::candidates_for`] 参照）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn prefix_slots(&self, prefix: &str) -> Vec<u32> {
        // `values` はバイト列昇順の辞書のため、`prefix` 自身が最初に現れうる
        // 位置を二分探索で求め、そこから前方一致が途切れるまで線形に辿る
        // （辞書順では前方一致する値は連続する）。
        let start = self.values.partition_point(|v| v.as_str() < prefix);
        let mut out = Vec::new();
        for (idx, value) in self.values.iter().enumerate().skip(start) {
            if !value.starts_with(prefix) {
                break;
            }
            let value_index = match u32::try_from(idx) {
                Ok(v) => v,
                Err(_) => break,
            };
            if let Some(s) = self.slots_for_value_index(value_index) {
                out.extend_from_slice(s);
            }
        }
        out
    }

    /// この列が保持する文字列実体・補助配列の概算ヒープバイト量。
    ///
    /// `values`/`offsets`/`slots`/`equality` はいずれも構築時に
    /// `try_reserve`/`try_reserve_exact` で重複排除前の `pair_count` 件分を
    /// 一括確保するため、重複が多い列では実要素数（`len()`）が確保容量
    /// （`capacity()`）を大きく下回る。`len()` だけを計上すると実際に確保
    /// 済みのメモリを過小計上し、[`ScalarIndexCache`] の総量上限（1 GiB）に
    /// よる追い出しが実メモリ量に対して働かなくなる（codex-review P1 対応・
    /// PR #569 未解決分）。確保容量ベースで保守的に計上し、この値をキャッシュ
    /// 登録・追い出し判定にも使う契約とする。
    fn approx_heap_bytes(&self) -> usize {
        // `values: Vec<String>` は外側 Vec の確保容量（`String` 構造体サイズ
        // 分）に加え、各 `String` 自身の確保容量（中身のバイト列）も計上する。
        // 個々の `String` は `try_owned_string` で複製された時点の長さぴったり
        // に確保される想定だが、`len()` ではなく `capacity()` を使うことで
        // 標準ライブラリの確保戦略が変わっても過小計上側に倒れないようにする。
        let values_outer_bytes = self
            .values
            .capacity()
            .saturating_mul(std::mem::size_of::<String>());
        let values_inner_bytes: usize = self
            .values
            .iter()
            .map(String::capacity)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let values_bytes = values_outer_bytes.saturating_add(values_inner_bytes);
        let offsets_bytes = self
            .offsets
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let slots_bytes = self
            .slots
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        // `HashMap` のキーは `values` と同じ文字列を複製保持する（`equality`
        // が値 → 添字の直引き専用であり `values` への参照を持たないため）。
        // 各キー文字列の確保容量（中身のバイト列）に加え、テーブル本体
        // （エントリ配列＋制御バイト。使用・未使用バケット双方を含む）を
        // [`hashmap_bucket_count`] で計上する。`HashMap::capacity()` は
        // hashbrown の負荷率適用後の「保持可能要素数」であり**実バケット数
        // ではない**ため（codex-review P1 対応・PR #569 未解決分）、これを
        // `hashmap_bucket_count` へ逆算入力することで実バケット数を復元する
        // （`capacity()` は `hashmap_bucket_count` と同じ丸め則で導出される
        // ため、往復させても実バケット数と一致する）。
        let equality_inner_bytes: usize = self
            .equality
            .keys()
            .map(String::capacity)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let equality_table_bytes = hashmap_reservation_bytes(self.equality.capacity());
        let equality_bytes = equality_inner_bytes.saturating_add(equality_table_bytes);
        values_bytes
            .saturating_add(offsets_bytes)
            .saturating_add(slots_bytes)
            .saturating_add(equality_bytes)
    }
}

/// [`crate::sql::arena_cache::SqlArenaSnapshot`] から構築するスカラー列二次索引
/// 本体（モジュールドキュメント「データモデル」参照）。
pub(crate) struct ScalarIndex {
    built_ctx: PolicyContext,
    built_table_generation: u64,
    row_count: usize,
    /// `schema.columns` と同じ長さ・順序。`TEXT`／`ENUM` 列のみ `Some`。
    columns: Vec<Option<TextColumnIndex>>,
    /// `schema.columns` と同じ長さ・順序。`INTEGER`／`BIGINT`／`REAL`／
    /// `DOUBLE`／`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID` 列のみ `Some`
    /// （Issue #893。`columns` と排他——同じ添字が両方 `Some` になることは
    /// ない。`BOOLEAN`／`BYTEA`／`JSON`／`JSONB`／`ARRAY`／`VECTOR` 列は
    /// いずれも `None` のまま索引対象外。モジュールドキュメント参照）。
    typed_columns: Vec<Option<OrderedColumnIndex>>,
    /// `id` 昇順（同一 `id` 内はスロット昇順）に整列した `(id, slot)`。全行が
    /// `id_as_finite_scalar` を満たす場合のみ `Some`（1 件でも `id > 2^53` が
    /// あれば `None`。モジュールドキュメント参照）。
    id_index: Option<Vec<(u64, u32)>>,
    /// [`Self::build`] 完了時に 1 回だけ計算した概算ヒープバイト量
    /// （[`Self::compute_approx_heap_bytes`] の結果）。索引は構築後不変
    /// （`columns`/`id_index` を変更する API を持たない）ため、この値も
    /// 索引の寿命を通じて不変。[`ScalarIndexCache::insert`] が
    /// 書き込みロック保持中に既存索引すべてを毎回再走査してしまう問題
    /// （codex-review P2 対応・PR #569）を解消するため、
    /// [`Self::approx_heap_bytes`] はこのフィールドを返すだけの O(1) アクセサ
    /// にする。
    approx_bytes: usize,
}

/// `TextColumnIndex` 構築時の並び替えキー: バイト列昇順、同値はスロット昇順。
/// スロット（`u32`。同一列内で重複しない）が全順序の明示的タイブレークを
/// 提供するため、この比較関数のもとで同値ペアは存在しない（決定的ソート）。
fn cmp_value_then_slot(a: &(String, u32), b: &(String, u32)) -> std::cmp::Ordering {
    a.0.as_bytes().cmp(b.0.as_bytes()).then(a.1.cmp(&b.1))
}

/// [`ScalarIndex::resolve_candidates`] の結果（Issue #474: `sql::exec` が
/// 候補削減を行うか全走査へ縮退するかの唯一の分岐点）。
pub(crate) enum CandidateResolution {
    /// 交差済み候補スロット（昇順）を使う。空 `Vec` は「一致 0 件」（全走査と
    /// 同じ結果になる。呼び出し元は空アリーナを構築するだけでよい）。
    Use(Vec<u32>),
    /// 索引対応述語のうち 1 つ以上が「列未索引」または `id_index` が `None`
    /// （`id > 2^53` を含む世代）で判定不能だったため、索引を一切使わず
    /// 全走査へ縮退する。
    FallbackNoIndex,
    /// 交差後の候補比が選択度切替閾値
    /// （[`DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_NUMERATOR`]/
    /// [`DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_DENOMINATOR`]）を超えた
    /// （索引経路より全走査が有利と判断）ため縮退する。
    FallbackSelectivity,
}

/// 2 本の昇順スライスの交差（両方に存在する要素のみ）を昇順で返す
/// （2 本指マージ。両リストとも重複要素を持たない契約——`candidates_for`／
/// `candidates_id_range` はいずれもスロット重複のない集合を返す）。
/// アロケーション失敗（`try_reserve`）は `None`（呼び出し元は索引なしへ
/// 縮退する）。
fn intersect_sorted(a: &[u32], b: &[u32]) -> Option<Vec<u32>> {
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve(a.len().min(b.len())).ok()?;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    Some(out)
}

/// [`ScalarIndex::build_targeted`] の構築対象列選択（Issue #1065・TASK-206・
/// INDEX-7）。`Auto` は現行どおり全対応列（`ScalarIndex::build` と同じ）、
/// `Declared` はカタログの索引宣言（`catalog::declared_index_targets_in_txn`）が
/// 返した列集合だけを構築対象にする。
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScalarIndexTarget<'a> {
    Auto,
    Declared(&'a [String]),
}

impl ScalarIndexTarget<'_> {
    /// `column_name` が構築対象かを判定する。`Auto` は常に `true`。
    fn includes(&self, column_name: &str) -> bool {
        match self {
            ScalarIndexTarget::Auto => true,
            ScalarIndexTarget::Declared(cols) => cols.iter().any(|c| c == column_name),
        }
    }
}

/// [`ScalarIndexTarget`] の所有権付きの版。呼び出し元（`sql::aggregate`・
/// `sql::exec`）は [`resolve_scalar_index_target_in_txn`] の戻り値をこの型で
/// 一旦保持し、`ScalarIndex::build_targeted` へ渡す直前に
/// [`Self::as_target`] で借用へ変換する（宣言列 `Vec<String>` の生存期間を
/// 呼び出し元のスコープに紐付けるため）。
#[derive(Debug, Clone)]
pub(crate) enum ScalarIndexTargetOwned {
    Auto,
    Declared(Vec<String>),
}

impl ScalarIndexTargetOwned {
    pub(crate) fn as_target(&self) -> ScalarIndexTarget<'_> {
        match self {
            ScalarIndexTargetOwned::Auto => ScalarIndexTarget::Auto,
            ScalarIndexTargetOwned::Declared(cols) => ScalarIndexTarget::Declared(cols),
        }
    }

    /// [`ScalarIndexTarget::includes`] へ委譲する（所有権付きの値をそのまま
    /// 保持する呼び出し元――`sql::explain`・`sql::aggregate` の EXPLAIN 経路
    /// ――が借用へ変換せずに判定できるようにする）。
    pub(crate) fn includes(&self, column_name: &str) -> bool {
        self.as_target().includes(column_name)
    }
}

/// `ScalarCacheAccess::declarations_enabled` と対象テーブルの索引宣言から
/// [`ScalarIndexTarget`] を決定する（Issue #1065）。呼び出し元
/// （`sql::aggregate::ensure_scalar_index_snapshot`・`sql::exec` の SELECT
/// 経路）はクエリが使っているのと同一の `read_txn` を渡し、以下の順で解決する:
///
/// - `declarations_enabled == false`（起動時 opt-in なし）: 常に `Auto`
///   （既存の opt-in なし利用者の挙動を変えない）
/// - `true` かつ対象テーブルにスカラー宣言あり: `Declared`
/// - `true` かつ宣言なし: `Auto`（現行の自動挙動を維持）
/// - カタログ読み取り失敗（走査上限超過・デコード破損）: `Err`。呼び出し元は
///   「宣言なし→自動」へは倒さず、索引を使わない側（構築失敗扱い→plain
///   scan）へ fail-closed に倒す。
pub(crate) fn resolve_scalar_index_target_in_txn(
    read_txn: &redb::ReadTransaction,
    table: &str,
    declarations_enabled: bool,
) -> Result<ScalarIndexTargetOwned, ()> {
    if !declarations_enabled {
        return Ok(ScalarIndexTargetOwned::Auto);
    }
    match crate::catalog::declared_index_targets_in_txn(read_txn, table) {
        Ok(targets) => Ok(match targets.scalar_columns {
            Some(cols) => ScalarIndexTargetOwned::Declared(cols),
            None => ScalarIndexTargetOwned::Auto,
        }),
        Err(_) => Err(()),
    }
}

/// `sql::explain` の `scalar_plan:`（[`crate::sql::scalar_plan::
/// classify_scalar_plan`]）・`access_path:`（[`crate::sql::aggregate::
/// classify_aggregate_access`]）の各静的判定を、実行時の索引構築対象選択
/// （[`resolve_scalar_index_target_in_txn`]・[`ScalarIndex::build_targeted`]）と
/// 同じ結果へ補正する純粋関数（Issue #1153・TASK-206・INDEX-7・SQL-27・
/// NOSQL-16・NOSQL-10）。`classify_scalar_plan` 自体は述語の形だけを見る
/// カタログ非依存の判定のままとし（`sql::exec` が引き続き使う単一情報源。
/// モジュールドキュメント参照）、本関数だけがカタログ由来の [`target`] を
/// 追加入力として受け取り「実行時は索引を使わない」ケースを `PlainScan` へ
/// 縮退させる。
///
/// - `plan == PlainScan`: 常に `PlainScan`（補正の必要なし）。
/// - `target == Err(())`（カタログ読み取り失敗）: [`resolve_scalar_index_target_in_txn`]
///   のドキュメントどおり「宣言なし→自動」へは倒さず、`sql::exec` が索引構築を
///   断念し以降のクエリが plain scan へ縮退するのと同じ `PlainScan` を返す
///   （fail-closed。`id` 述語のみの形〔`IndexIdRange`〕も含めて降格する—— 実行時は
///   `ScalarIndex::build_targeted` 自体が呼ばれず `id_index` も含め一切構築
///   されないため）。
/// - `target == Ok(Auto)`: `plan` をそのまま返す（宣言なし・opt-in なしの
///   従来挙動をビット単位で変えない。受け入れ条件 2）。
/// - `target == Ok(Declared(cols))`: `metadata_filter_columns`（`plan` の元に
///   なった `bound.metadata_filters` それぞれの列添字。解決できない添字は
///   `None` で渡す）を `schema.columns` の列名へ変換し、1 件でも `cols` に
///   含まれない・解決できない列があれば `PlainScan`。`id` 述語
///   （`expr_filters` 由来。`metadata_filter_columns` には現れない）は
///   `id_index` が宣言の有無によらず常に構築されるため降格の対象にしない
///   （`IndexIdRange`・`IndexConjunction` に混ざっていても、`metadata_filters`
///   側がすべて宣言列に含まれていれば据え置く）。
pub(crate) fn scalar_plan_under_target(
    plan: crate::sql::scalar_plan::ScalarPlan,
    metadata_filter_columns: impl IntoIterator<Item = Option<usize>>,
    schema: &TableSchema,
    target: &Result<ScalarIndexTargetOwned, ()>,
) -> crate::sql::scalar_plan::ScalarPlan {
    use crate::sql::scalar_plan::ScalarPlan;

    if plan == ScalarPlan::PlainScan {
        return ScalarPlan::PlainScan;
    }
    let declared = match target {
        Err(()) => return ScalarPlan::PlainScan,
        Ok(ScalarIndexTargetOwned::Auto) => return plan,
        Ok(owned @ ScalarIndexTargetOwned::Declared(_)) => owned,
    };
    for column_index in metadata_filter_columns {
        let Some(column_index) = column_index else {
            // 添字を解決できない（`ExplainShape` のビット集合が範囲外・
            // 判定不能を表すフラグを立てた場合）: fail-closed に降格する。
            return ScalarPlan::PlainScan;
        };
        let Some(column) = schema.columns.get(column_index) else {
            return ScalarPlan::PlainScan;
        };
        if !declared.includes(&column.name) {
            return ScalarPlan::PlainScan;
        }
    }
    plan
}

impl ScalarIndex {
    /// `schema`（対象テーブルのスキーマ）と `snapshot`（RLS 段適用済みスナップ
    /// ショット）から索引を構築する。`snapshot` の各スロットを 1 回だけ走査し、
    /// untrusted な行 metadata のデコード検証は [`scan_scalar_columns`] が
    /// 一切弱めずに行う（`.claude/rules/coding-rust.md`「untrusted 入力の
    /// 扱い」）。
    ///
    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID` 列の順序索引（Issue #893。
    /// レーン B——`declarative_filter::FilterOp::TypedCompare`。Issue #891・
    /// TASK-199 で production 結線済み）は、[`Self::candidates_for`] が
    /// `TypedCompare` 述語を直接照会できるため常に構築する。一方
    /// `INTEGER`／`BIGINT`／`REAL`／`DOUBLE` 列（レーン A——算術式・
    /// `sql::udf_call`／`sql::expr_program` 経由の `WHERE` 比較。#891 の
    /// 対象外として別 Issue へ切り出し済み・未着手）は、これらの型を
    /// 対象にした `WHERE` 述語表現が SQL 表層にまだ存在せず
    /// `resolve_candidates` から一度も照会されないため、構築自体を
    /// 遅延させ `BOOLEAN` 等と同じ非索引化 (`None`) 扱いとする
    /// （未接続の typed 索引だけが原因で `MAX_SCALAR_INDEX_BYTES` 超過に
    /// より既存の `TEXT`／`ENUM` 索引まで巻き添えで `TooLarge`（plain scan
    /// 縮退）になるのを防ぐ。codex-review P2 指摘・PR #1032）。レーン A の
    /// 構築ロジック自体（`OrderedColumnIndex::I64`／`F64Sortable` の値
    /// 変換・`2^53` ゲート等）を検証するテスト専用の入口は
    /// [`Self::build_including_unwired_typed_range_columns`]（`#[cfg(test)]`）
    /// を使う。
    ///
    /// production 経路（`sql::exec`／`sql::aggregate`）は Issue #1065 以降
    /// [`Self::build_targeted`]（`ScalarIndexTarget::Auto`／`Declared` を
    /// 明示選択）を直接呼ぶため、本関数はテストの簡便な入口としてのみ残す
    /// （`#[cfg(test)]`）。
    #[cfg(test)]
    fn build(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
    ) -> Result<Self, ScalarIndexBuildError> {
        Self::build_targeted(schema, snapshot, ScalarIndexTarget::Auto)
    }

    /// [`Self::build`] の構築対象列を宣言（`CREATE INDEX`・Issue #1065・
    /// TASK-206・INDEX-7）で絞り込める版。`sql::exec`／`sql::aggregate` の
    /// 呼び出し元は、起動時 opt-in（`SearchEngineKind::Hnsw`）が有効かつ
    /// 対象テーブルにスカラー宣言があるときのみ `Declared` を渡し、それ以外は
    /// 現行どおり `Auto`（全対応列）を渡す（`docs/design/
    /// index-declaration-effects.md`）。`Declared` は「絞り込み」であり
    /// 「強制」ではない: 宣言列にも平均値長ゲート・バイト予算・`2^53` ゲートを
    /// 変わらず適用し、`id` の順序索引（`id_index`）は宣言の有無によらず常に
    /// 構築する。
    pub(crate) fn build_targeted(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
        target: ScalarIndexTarget<'_>,
    ) -> Result<Self, ScalarIndexBuildError> {
        Self::build_internal_targeted(schema, snapshot, false, target)
    }

    /// [`Self::build`] のうち、レーン A（`INTEGER`／`BIGINT`／`REAL`／
    /// `DOUBLE`。まだ `WHERE` 述語表現を持たず production からは一度も
    /// 照会されない）の順序索引構築ロジックそのものを検証するテスト専用の
    /// 入口。production 経路（`sql::exec`／`sql::aggregate`／
    /// `sql::group_by`）はレーン A 列を索引化しない [`Self::build`] を使い、
    /// この入口は呼ばない（codex-review P2 指摘・PR #1032。レーン A の
    /// `WHERE` 述語表現が実装され次第 [`Self::build`] 側へ合流する想定）。
    #[cfg(test)]
    fn build_including_unwired_typed_range_columns(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
    ) -> Result<Self, ScalarIndexBuildError> {
        Self::build_internal_targeted(schema, snapshot, true, ScalarIndexTarget::Auto)
    }

    fn build_internal_targeted(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
        include_unwired_lane_a_columns: bool,
        target: ScalarIndexTarget<'_>,
    ) -> Result<Self, ScalarIndexBuildError> {
        let row_count = snapshot.arena().len();
        let column_count = schema.columns.len();

        // 構築完了まで確保した（概算）文字列バイト量の累計。値の複製
        // （`to_string`/`clone`）を行う**前**に [`check_scalar_index_budget`] で
        // 検証してから加算する（codex-review P1 対応・PR #569。モジュール
        // ドキュメント「fail-closed の適用範囲」参照）。
        let mut approx_bytes: usize = 0;

        // 列ごとに (value, slot) を蓄積する作業領域（`TEXT`／`ENUM` 列のみ
        // `Some`）。
        let mut per_column: Vec<Option<Vec<(String, u32)>>> = Vec::new();
        per_column
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;

        // 列ごとに (key, slot) を蓄積する作業領域（Issue #893。数値・日時・
        // `NUMERIC`・`UUID` 列のみ `Some`。`per_column` と排他）。固定長キー
        // のため `TEXT` 列と異なり `row_count` ちょうどの一括確保のみで
        // 十分（重複排除・別建ての `equality` テーブルは不要）。
        let mut per_column_typed: Vec<Option<OrderedColumnIndex>> = Vec::new();
        per_column_typed
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        // 列単位の `2^53` ゲート（`BIGINT`／`TIMESTAMP`。モジュールドキュメント
        // 「`MAX_EXACT_TYPED_I64_ABS`」参照）・非有限値の防御ゲート（`REAL`／
        // `DOUBLE`。`scalar_float.rs` の不変条件が崩れた場合の多層防御）で
        // 列を除外する際に差し戻す、初期確保分の予算計上額。
        let mut typed_col_reservation_bytes: Vec<usize> = Vec::new();
        typed_col_reservation_bytes
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        typed_col_reservation_bytes.resize(column_count, 0);

        // 列単位の平均値長ゲート（Issue #632。モジュールドキュメント「列単位の
        // 索引対象除外」参照）が使う作業領域。列ごとに (a) 初期 `acc` 確保分
        // として `approx_bytes` へ計上済みのバイト量（除外時の返還額の基準）・
        // (b) これまでに索引化した非 `NULL` 値の累積バイト量・(c) 同じく件数
        // を追跡する。`column_count` は `schema.columns.len()`（untrusted な
        // 行数ではなくスキーマ由来のため小さく、通常のアロケーションで十分だが
        // 他の列配列との一貫性のため `try_reserve_exact` で確保する）。
        let mut col_reservation_bytes: Vec<usize> = Vec::new();
        col_reservation_bytes
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        col_reservation_bytes.resize(column_count, 0);
        let mut col_running_bytes: Vec<usize> = Vec::new();
        col_running_bytes
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        col_running_bytes.resize(column_count, 0);
        let mut col_nonnull_count: Vec<usize> = Vec::new();
        col_nonnull_count
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        col_nonnull_count.resize(column_count, 0);

        for (col_index, column) in schema.columns.iter().enumerate() {
            // 宣言で絞り込む場合（`ScalarIndexTarget::Declared`）、宣言列に
            // 無い列は型に関わらず「列が索引未対応」（`per_column`／
            // `per_column_typed` とも `None`）へ合流させ、バイト予算の計上も
            // 行わない（Issue #1065。宣言は「絞り込み」であり、対象外の列は
            // 構築コストを一切払わない）。`id` は本ループの対象外（`id_index`
            // として別途・宣言によらず常に構築する）。
            if !target.includes(&column.name) {
                per_column.push(None);
                per_column_typed.push(None);
                continue;
            }
            match &column.ty {
                // ENUM 列は TEXT と同じ辞書表現を共有する（Issue #890 D3。
                // ラベルは短いため平均値長ゲートで除外されることは実質ない）。
                ColumnType::Text | ColumnType::Enum(_) => {
                    // `acc` は 1 行につき列あたり高々 1 値しか追加されないため
                    // `row_count` が確保上限になる。倍増などの成長戦略による
                    // 余剰確保を避けるため、行走査を始める前に必要量ちょうどを
                    // 一括で `try_reserve_exact` し、その容量分（タプル全体の
                    // サイズ）を確保前にバイト予算へ計上する（codex-review P1
                    // 対応・PR #569 未解決分。`per_column_accumulator_reservation_bytes`
                    // 参照）。
                    let reservation_bytes = per_column_accumulator_reservation_bytes(row_count);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    if let Some(slot) = col_reservation_bytes.get_mut(col_index) {
                        *slot = reservation_bytes;
                    }
                    let mut acc: Vec<(String, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column.push(Some(acc));
                    per_column_typed.push(None);
                }
                // `INTEGER`／`BIGINT` 列（レーン A: 算術式・`sql::udf_call`／
                // `sql::expr_program` 経由の `WHERE` 比較。#891 の対象外として
                // 切り出し済み・未着手）は `WHERE` 述語表現が SQL 表層に
                // まだ無く `resolve_candidates` から一度も照会されないため、
                // production 経路（`include_unwired_lane_a_columns == false`）
                // では構築自体を遅延させる（`Self::build` ドキュメント参照。
                // codex-review P2・PR #1032）。構築ロジック自体は
                // [`Self::build_including_unwired_typed_range_columns`]
                // からのみ検証する。
                ColumnType::Integer | ColumnType::BigInt => {
                    per_column.push(None);
                    if !include_unwired_lane_a_columns {
                        per_column_typed.push(None);
                        continue;
                    }
                    let entry_size = std::mem::size_of::<(i64, u32)>();
                    let reservation_bytes = typed_column_reservation_bytes(row_count, entry_size);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    if let Some(slot) = typed_col_reservation_bytes.get_mut(col_index) {
                        *slot = reservation_bytes;
                    }
                    let mut acc: Vec<(i64, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column_typed.push(Some(OrderedColumnIndex::I64(acc)));
                }
                // `DATE`／`TIMESTAMP` 列（レーン B:
                // `declarative_filter::FilterOp::TypedCompare`。Issue #891・
                // TASK-199 で production 結線済み）は `i64` へ昇格した
                // 順序索引（`OrderedColumnIndex::I64`。Issue #893）で扱い、
                // `Self::candidates_for` から常に照会されるため常に構築する。
                // `TIMESTAMP` は行走査中に `2^53` を超える値が 1 件でも見つかった
                // 時点で列単位に `None` へ縮退する（モジュールドキュメント
                // 「`MAX_EXACT_TYPED_I64_ABS`」参照）。
                ColumnType::Date | ColumnType::Timestamp => {
                    per_column.push(None);
                    let entry_size = std::mem::size_of::<(i64, u32)>();
                    let reservation_bytes = typed_column_reservation_bytes(row_count, entry_size);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    if let Some(slot) = typed_col_reservation_bytes.get_mut(col_index) {
                        *slot = reservation_bytes;
                    }
                    let mut acc: Vec<(i64, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column_typed.push(Some(OrderedColumnIndex::I64(acc)));
                }
                // `REAL`／`DOUBLE` 列（レーン A。上と同じ理由で production
                // 経路では構築を遅延させる）は全順序比較可能な `u64` ビット
                // 表現（[`f64_to_sortable_bits`]）で扱う。値は `scalar_float.rs`
                // の不変条件（常に有限・`-0.0` は `+0.0` に正規化済み）を前提と
                // するが、崩れていた場合は防御的に列単位で `None` へ縮退する。
                ColumnType::Real | ColumnType::Double => {
                    per_column.push(None);
                    if !include_unwired_lane_a_columns {
                        per_column_typed.push(None);
                        continue;
                    }
                    let entry_size = std::mem::size_of::<(u64, u32)>();
                    let reservation_bytes = typed_column_reservation_bytes(row_count, entry_size);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    if let Some(slot) = typed_col_reservation_bytes.get_mut(col_index) {
                        *slot = reservation_bytes;
                    }
                    let mut acc: Vec<(u64, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column_typed.push(Some(OrderedColumnIndex::F64Sortable(acc)));
                }
                // `NUMERIC(p, s)` 列（レーン B。production 結線済み）は列固定の
                // `scale` のもとで `unscaled`（`i128`）の大小がそのまま値の
                // 大小になるため、`unscaled` をそのままキーとする
                // （`OrderedColumnIndex::I128`。`Self::candidates_for` が
                // `TypedCompare` 述語の境界計算に使う列固定 `scale` を第 2
                // 要素として一緒に保持する）。常に構築する。
                ColumnType::Numeric { scale, .. } => {
                    per_column.push(None);
                    let entry_size = std::mem::size_of::<(i128, u32)>();
                    let reservation_bytes = typed_column_reservation_bytes(row_count, entry_size);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    if let Some(slot) = typed_col_reservation_bytes.get_mut(col_index) {
                        *slot = reservation_bytes;
                    }
                    let mut acc: Vec<(i128, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column_typed.push(Some(OrderedColumnIndex::I128(acc, *scale)));
                }
                // `UUID` 列（レーン B。production 結線済み）はネットワーク
                // バイトオーダーのバイト列を `u128`（ビッグエンディアン）として
                // 扱う（`Uuid` の `Ord` 導出——バイト列辞書順——と同じ大小関係に
                // なる。`OrderedColumnIndex::U128`）。常に構築する。
                ColumnType::Uuid => {
                    per_column.push(None);
                    let entry_size = std::mem::size_of::<(u128, u32)>();
                    let reservation_bytes = typed_column_reservation_bytes(row_count, entry_size);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    if let Some(slot) = typed_col_reservation_bytes.get_mut(col_index) {
                        *slot = reservation_bytes;
                    }
                    let mut acc: Vec<(u128, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column_typed.push(Some(OrderedColumnIndex::U128(acc)));
                }
                // `BOOLEAN` 列は索引対象外（Issue #883・D-e。値域が 2 値のため
                // 索引化コストに見合わず、対応述語 `BoolEquals` は常に
                // plain scan——`scalar_plan.rs` 参照——のまま据え置く）。
                // `ARRAY`（TABLE-14・Issue #888）・`BYTEA`（Issue #886）・
                // `JSON`／`JSONB`（TABLE-14・Issue #889）列はいずれも等価・
                // 前方一致・範囲述語を持たないため同じく非索引化（判断根拠は
                // `docs/design/scalar-index-prune.md`「Issue #893」節参照）。
                // `VECTOR` 列も従来どおり索引対象外。
                ColumnType::Vector(_)
                | ColumnType::Boolean
                | ColumnType::Array(_)
                | ColumnType::Bytea
                | ColumnType::Json
                | ColumnType::Jsonb => {
                    per_column.push(None);
                    per_column_typed.push(None);
                }
            }
        }

        for slot in 0..row_count {
            let slot_u32 = u32::try_from(slot).map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
            let metadata = snapshot
                .metadata()
                .get(slot)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let scanned =
                scan_scalar_columns(schema, metadata).map_err(ScalarIndexBuildError::RowDecode)?;
            for (col_index, value) in scanned.into_iter().enumerate() {
                let Some(v) = value else { continue };
                // この列がまだ `TEXT`／`ENUM` の索引対象か（平均値長ゲートで
                // 未除外か）を先に確認する。`TEXT` でない列・既に除外済みの列は
                // 数値・日時・`NUMERIC`・`UUID` 列（Issue #893）の候補として
                // 別枠で扱う（モジュールドキュメント「列単位の索引対象除外」
                // 参照）。
                let is_indexed_column = matches!(per_column.get(col_index), Some(Some(_)));
                if !is_indexed_column {
                    // 数値・日時・`NUMERIC`・`UUID` 列の候補か（Issue #893）。
                    // 該当しない・既に除外済みの列は静かにスキップする。
                    if matches!(per_column_typed.get(col_index), Some(Some(_))) {
                        let keep = if let Some(Some(accum)) = per_column_typed.get_mut(col_index) {
                            push_typed_value(accum, &v, slot_u32)
                        } else {
                            true
                        };
                        if !keep {
                            // `2^53` 超過・非有限値・型不一致のいずれか
                            // （`push_typed_value` 参照）: 列単位で `None` へ
                            // 縮退する（fail-closed。予算は初期一括確保分
                            // ちょうどを差し戻す。固定長キーのため `TEXT` 列の
                            // ような複製済み文字列本体分の追加返還は不要）。
                            let refund = typed_col_reservation_bytes
                                .get(col_index)
                                .copied()
                                .unwrap_or(0);
                            approx_bytes = approx_bytes.saturating_sub(refund);
                            if let Some(slot_acc) = per_column_typed.get_mut(col_index) {
                                *slot_acc = None;
                            }
                        }
                    }
                    continue;
                }
                // 索引対象列は常に `TEXT`／`ENUM`。`as_dictionary_text` で
                // 両者を同じ辞書表現として扱う（Issue #890 D3）。
                let Some(v) = v.as_dictionary_text() else {
                    continue;
                };
                let v_len = v.len();
                let (Some(&running), Some(&nonnull)) = (
                    col_running_bytes.get(col_index),
                    col_nonnull_count.get(col_index),
                ) else {
                    // `col_running_bytes`/`col_nonnull_count` は `column_count`
                    // ちょうどの長さで確保済みであり、`col_index` は同じ
                    // `schema.columns` 由来のため通常到達しないが、untrusted
                    // 入力経路の添字アクセス回避方針に従い defensive に
                    // スキップする（この列を索引化しないだけで構築全体は
                    // 失敗させない）。
                    continue;
                };
                let prospective_count = nonnull.saturating_add(1);
                let prospective_bytes = running.saturating_add(v_len);
                let avg_threshold_bytes =
                    prospective_count.saturating_mul(MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN);
                if prospective_bytes > avg_threshold_bytes {
                    // 平均値長が閾値を超過: この列を索引対象から除外する
                    // （fail-closed な構築全体の失敗ではなく、列単位の
                    // fail-soft な縮退。モジュールドキュメント参照）。
                    // 除外時点までに計上済みのバイト量（初期 `acc` 確保分＋
                    // これまでに複製した文字列本体分）を予算から差し戻す
                    // （このスロット以降、当該列の値は一切複製・索引化しない
                    // ため、確保済みメモリは `per_column[col_index] = None`
                    // の代入で解放される）。
                    let reservation = col_reservation_bytes.get(col_index).copied().unwrap_or(0);
                    let refund = reservation.saturating_add(running);
                    approx_bytes = approx_bytes.saturating_sub(refund);
                    if let Some(slot_acc) = per_column.get_mut(col_index) {
                        *slot_acc = None;
                    }
                    continue;
                }
                if let Some(Some(acc)) = per_column.get_mut(col_index) {
                    // タプル・`Vec` の確保容量分は上記の事前一括確保
                    // （`per_column_accumulator_reservation_bytes`）で
                    // 既に予算計上済みのため、ここでは文字列本体（ヒープ）の
                    // バイト量のみを追加計上する（二重計上を避ける）。
                    let additional = v_len;
                    check_scalar_index_budget(approx_bytes, additional)?;
                    let owned = try_owned_string(v)?;
                    approx_bytes = approx_bytes.saturating_add(additional);
                    // `acc` は列ごとに `row_count` ちょうどの容量を
                    // 事前に厳密確保済みで、1 行につき列あたり高々 1 回しか
                    // push されないため、この push が容量を超えて再確保
                    // （＝未計上の追加確保）を起こすことはない。
                    acc.push((owned, slot_u32));
                    if let Some(slot) = col_running_bytes.get_mut(col_index) {
                        *slot = prospective_bytes;
                    }
                    if let Some(slot) = col_nonnull_count.get_mut(col_index) {
                        *slot = prospective_count;
                    }
                }
            }
        }

        let mut columns: Vec<Option<TextColumnIndex>> = Vec::new();
        columns
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        for entries in per_column {
            match entries {
                None => columns.push(None),
                Some(mut pairs) => {
                    pairs.sort_unstable_by(cmp_value_then_slot); // sort-determinism: allow キーは (バイト列, スロット昇順) のタプルでスロットが全順序の明示的タイブレーク
                                                                 // 重複排除後の値種類数は `pairs.len()`（行の重複値込みの総数）を
                                                                 // 上回らないため、この上限で `values`/`offsets`/`equality` を
                                                                 // 事前に一括確保し、高カーディナリティ列で値ごとに
                                                                 // `try_reserve_exact(1)` を呼ぶ再確保コストを避ける
                                                                 // （codex-review P2 対応・PR #569）。`slots` は値と無関係に
                                                                 // 行数分（`pairs.len()`）で確定するため同様に一括確保する。
                    let pair_count = pairs.len();
                    // 上記 4 配列の確保容量（重複排除前の pair_count 件分）は
                    // 実メモリとして確保されるため、確保前にバイト予算へ計上して
                    // 検証する（codex-review P1 対応・PR #569 未解決分。
                    // `text_column_reservation_bytes` 参照）。
                    let reservation_bytes = text_column_reservation_bytes(pair_count);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    let mut values: Vec<String> = Vec::new();
                    values
                        .try_reserve_exact(pair_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    let mut offsets: Vec<u32> = Vec::new();
                    offsets
                        .try_reserve_exact(pair_count.saturating_add(1))
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    let mut slots: Vec<u32> = Vec::new();
                    slots
                        .try_reserve_exact(pair_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    let mut equality: HashMap<String, u32> = HashMap::new();
                    equality
                        .try_reserve(pair_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    offsets.push(0);
                    let mut iter = pairs.into_iter().peekable();
                    while let Some((value, slot)) = iter.next() {
                        slots
                            .try_reserve(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        slots.push(slot);
                        while iter
                            .peek()
                            .is_some_and(|(next_value, _)| next_value == &value)
                        {
                            // untrusted な行 metadata 由来の値列に対する走査のため
                            // `unwrap`/`expect` を使わない（`.claude/rules/
                            // coding-rust.md`）。直前の `is_some_and` で `Some` を
                            // 確認済みだが、`if let` で明示的に分岐しコード上も
                            // 添字アクセス相当を避ける。
                            let Some((_, next_slot)) = iter.next() else {
                                break;
                            };
                            slots
                                .try_reserve(1)
                                .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                            slots.push(next_slot);
                        }
                        let value_index = u32::try_from(values.len())
                            .map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
                        // `equality` は `values` と同じ文字列を複製保持する
                        // （[`TextColumnIndex::approx_heap_bytes`] の
                        // `equality_bytes` 計上と対応）ため、複製前に同じ予算
                        // 検証を経る（codex-review P1 対応・PR #569）。
                        let additional = approx_string_entry_bytes(&value);
                        check_scalar_index_budget(approx_bytes, additional)?;
                        let equality_key = try_owned_string(&value)?;
                        approx_bytes = approx_bytes.saturating_add(additional);
                        equality
                            .try_reserve(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        equality.insert(equality_key, value_index);
                        values
                            .try_reserve_exact(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        values.push(value);
                        let slots_len = u32::try_from(slots.len())
                            .map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
                        offsets
                            .try_reserve_exact(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        offsets.push(slots_len);
                    }
                    columns.push(Some(TextColumnIndex {
                        values,
                        offsets,
                        slots,
                        equality,
                    }));
                }
            }
        }

        // 数値・日時・`NUMERIC`・`UUID` 列の順序索引を確定させる（Issue #893）。
        // `TEXT` 列と異なり重複排除は行わない（キーは固定長で辞書テーブルを
        // 持たない）ため、`(key, slot)` を昇順ソートするだけでよい。
        let mut typed_columns: Vec<Option<OrderedColumnIndex>> = Vec::new();
        typed_columns
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        for entry in per_column_typed {
            match entry {
                None => typed_columns.push(None),
                Some(OrderedColumnIndex::I64(mut pairs)) => {
                    pairs.sort_unstable(); // sort-determinism: allow (i64 キー, u32 スロット) のタプル全順序でスロットが明示的タイブレーク（同一列内でスロットは重複しない）
                    typed_columns.push(Some(OrderedColumnIndex::I64(pairs)));
                }
                Some(OrderedColumnIndex::F64Sortable(mut pairs)) => {
                    pairs.sort_unstable(); // sort-determinism: allow (u64 ソート可能ビット表現, u32 スロット) のタプル全順序でスロットが明示的タイブレーク
                    typed_columns.push(Some(OrderedColumnIndex::F64Sortable(pairs)));
                }
                Some(OrderedColumnIndex::I128(mut pairs, scale)) => {
                    pairs.sort_unstable(); // sort-determinism: allow (i128 キー, u32 スロット) のタプル全順序でスロットが明示的タイブレーク
                    typed_columns.push(Some(OrderedColumnIndex::I128(pairs, scale)));
                }
                Some(OrderedColumnIndex::U128(mut pairs)) => {
                    pairs.sort_unstable(); // sort-determinism: allow (u128 キー, u32 スロット) のタプル全順序でスロットが明示的タイブレーク
                    typed_columns.push(Some(OrderedColumnIndex::U128(pairs)));
                }
            }
        }

        // `id` 順序索引: 1 件でも `id_as_finite_scalar` に失敗する行があれば
        // 索引全体を `None` にする（fail-closed。モジュールドキュメント参照）。
        let mut id_index: Option<Vec<(u64, u32)>> = None;
        'id_index: {
            let mut pairs: Vec<(u64, u32)> = Vec::new();
            if pairs.try_reserve_exact(row_count).is_err() {
                break 'id_index;
            }
            for (slot, &id) in snapshot.arena().ids().iter().enumerate() {
                if crate::sql::udf_call::id_as_finite_scalar(id).is_err() {
                    break 'id_index;
                }
                let Ok(slot_u32) = u32::try_from(slot) else {
                    break 'id_index;
                };
                pairs.push((id, slot_u32));
            }
            pairs.sort_unstable();
            id_index = Some(pairs);
        }

        let mut built = Self {
            built_ctx: snapshot.built_ctx_for_index().clone(),
            built_table_generation: snapshot.built_table_generation_for_index(),
            row_count,
            columns,
            typed_columns,
            id_index,
            // 直後に `compute_approx_heap_bytes` の結果で確定させるまでの
            // 仮値。この構造体は `build` の外へ `0` のまま漏れ出さない。
            approx_bytes: 0,
        };
        let computed_bytes = built.compute_approx_heap_bytes();
        if computed_bytes > MAX_SCALAR_INDEX_BYTES {
            return Err(ScalarIndexBuildError::TooLarge);
        }
        built.approx_bytes = computed_bytes;
        Ok(built)
    }

    /// 等価述語向け直引き（列が `TEXT` でない・未知の列は `None`。値が辞書に
    /// 無い場合は `None`。存在有無を区別しない照会は [`Self::candidates_for`]
    /// を使う）。
    #[cfg(test)]
    fn candidates_equals(&self, column_index: usize, value: &str) -> Option<&[u32]> {
        let column = self.columns.get(column_index)?.as_ref()?;
        let value_index = *column.equality.get(value)?;
        column.slots_for_value_index(value_index)
    }

    /// `column_index` 列が実際に索引化されているか（`TEXT` 列かつ平均値長
    /// ゲート〔Issue #632〕で除外されていないか）を返すテスト専用アクセサ。
    #[cfg(test)]
    fn column_is_indexed(&self, column_index: usize) -> bool {
        matches!(self.columns.get(column_index), Some(Some(_)))
    }

    /// `column_index` 列が数値・日時・`NUMERIC`・`UUID` の順序索引として
    /// 実際に索引化されているか（`2^53` ゲート等で除外されていないか）を
    /// 返すテスト専用アクセサ（Issue #893。[`Self::column_is_indexed`] の
    /// typed 版）。
    #[cfg(test)]
    fn typed_column_is_indexed(&self, column_index: usize) -> bool {
        matches!(self.typed_columns.get(column_index), Some(Some(_)))
    }

    /// [`MetadataFilter`] を評価し、一致スロットの**昇順** `Vec<u32>` を返す。
    /// 列が対応する索引を持たない・未知の列は `None`。一致 0 件（列は索引済み
    /// だが値が存在しない）は `Some(vec![])` を返す（`None` と区別する）。
    pub(crate) fn candidates_for(&self, filter: &MetadataFilter) -> Option<Vec<u32>> {
        // `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID` 列の範囲比較
        // （`FilterOp::TypedCompare`。TABLE-13・TASK-199、Issue #891 で
        // production 結線済み）は `self.columns`（`TEXT`／`ENUM`）ではなく
        // `self.typed_columns` を参照するため、`TEXT` 用の共有ブロックへ入る
        // 前に個別に処理する（Issue #893 production 接続）。
        if let FilterOp::TypedCompare { op, value } = filter.op() {
            return self.typed_compare_candidates(filter.column_index(), *op, value);
        }
        // `BETWEEN`（`FilterOp::Between`。SQL-24・TASK-208 ポインタ）は
        // `Ge`∧`Le` の交差として導出する（`typed_compare_candidates` を 2 回
        // 呼ぶ。`BYTEA` は `OrderedColumnIndex` に対応 variant が無く
        // `typed_compare_candidates` 自身が `None` を返すため、ここでも
        // 未対応のまま fail-closed に縮退する）。
        if let FilterOp::Between { low, high } = filter.op() {
            use crate::declarative_filter::CompareOp;
            let lower = self.typed_compare_candidates(filter.column_index(), CompareOp::Ge, low)?;
            let upper =
                self.typed_compare_candidates(filter.column_index(), CompareOp::Le, high)?;
            return intersect_sorted(&lower, &upper);
        }
        let column = self.columns.get(filter.column_index())?.as_ref()?;
        let mut result = match filter.op() {
            FilterOp::Equals(value) => column
                .equality
                .get(value)
                .and_then(|&vi| column.slots_for_value_index(vi))
                .map(|s| s.to_vec())
                .unwrap_or_default(),
            FilterOp::StartsWith(prefix) => column.prefix_slots(prefix),
            // `IN`（`TEXT`／`ENUM` 列。SQL-24・TASK-208 ポインタ）: 値ごとの
            // 等価スロットの和集合を取る。束縛段（`declarative_filter::
            // bind_filter_op`）が値を重複除去済みで渡すため本来互いに素だが、
            // 防御的に `sort_unstable`＋`dedup`（呼び出し元と同じ後処理）で
            // 冪等にする。
            FilterOp::InText(values) => {
                let mut union = Vec::new();
                for value in values {
                    if let Some(slots) = column
                        .equality
                        .get(value)
                        .and_then(|&vi| column.slots_for_value_index(vi))
                    {
                        union.extend_from_slice(slots);
                    }
                }
                union
            }
            // BOOLEAN 列は索引化しない（`per_column` が常に `None`。上の
            // `?` で既にここへ到達しない）ため構造的に到達しないが、
            // 網羅性のため fail-closed に `None` を返す。
            FilterOp::BoolEquals(_) => return None,
            // 上の早期リターンで処理済みのため構造的に到達しないが、
            // 網羅性のため fail-closed に `None` を返す。
            FilterOp::TypedCompare { .. } | FilterOp::Between { .. } => return None,
            // `Compare`／`InListLiteral`／`BetweenLiteral`（未束縛）・
            // `InTyped`／`IsNull`／`IsNotNull`／`Not`（索引未対応。SQL-24・
            // TASK-208 ポインタ。`sql::scalar_plan::classify_scalar_plan` が
            // 事前ゲートで `PlainScan` へ倒すため通常はここへ来ない）は
            // `bind` を経た `MetadataFilter` に現れうる／現れない契約に
            // 関わらず同じ腕で扱う。索引で解決しないことで `sql::exec` は
            // 候補削減を信頼せず既存の全行走査（フィルタ事前/事後適用）へ
            // フォールバックする（fail-closed。索引未対応が誤って
            // 「一致 0 件」に化けない）。
            //
            // `LIKE` の一般形（中間一致・後方一致・`_`。SQL-24／TASK-208、
            // Issue #914）は二次索引が対応しない（`OrderedColumnIndex`／
            // `column.equality`・`prefix_slots` のいずれの表現にも一般形の
            // 照会手段が無い）ため同じく `None` を返す（fail-closed。索引の
            // 有無で結果が変わらない契約を守る）。`LikeUnbound`（未束縛）も
            // `Compare` と同じ網羅性のための保険腕。
            FilterOp::Compare { .. }
            | FilterOp::InListLiteral { .. }
            | FilterOp::BetweenLiteral { .. }
            | FilterOp::InTyped(_)
            | FilterOp::IsNull
            | FilterOp::IsNotNull
            | FilterOp::Not(_)
            | FilterOp::Like(_)
            | FilterOp::LikeUnbound(_) => return None,
        };
        result.sort_unstable();
        result.dedup();
        Some(result)
    }

    /// [`FilterOp::TypedCompare`] 述語（`DATE`／`TIMESTAMP`／`NUMERIC`／
    /// `UUID`。`BYTEA` は [`OrderedColumnIndex`] に対応 variant を持たず
    /// 未対応のまま——モジュールドキュメント「データモデル」参照）の一致
    /// スロットを返す（Issue #893 production 接続）。`DATE`／`TIMESTAMP`／
    /// `UUID` は列・リテラルともに同一表現の厳密な整数キーのため、境界は
    /// 単純な `checked_add`/`checked_sub` で導出する。`NUMERIC` はリテラルが
    /// 列と異なる `scale` を持ちうる（[`declarative_filter::TypedLiteral::
    /// Numeric`] は `parse_literal_exact` 由来の任意 `scale`）ため
    /// [`crate::numeric::rescale_bounds_for_column`] へ委譲する。列が typed
    /// 索引の対象でない・未索引（`None`）・境界計算がオーバーフローする
    /// 場合は `None`（判定不能。呼び出し元は全走査へ縮退する契機として
    /// 扱う。fail-closed）。
    fn typed_compare_candidates(
        &self,
        column_index: usize,
        op: crate::declarative_filter::CompareOp,
        value: &crate::declarative_filter::TypedLiteral,
    ) -> Option<Vec<u32>> {
        use crate::declarative_filter::{CompareOp, TypedLiteral};
        use std::ops::Bound;

        /// キー値と無関係に常に空集合になる正準表現（`start == end` が
        /// 常に成立する。`numeric::rescale_bounds_for_column`・
        /// `sql::scalar_plan::id_bounds` と同じ手法）。
        fn empty<K: Copy + Default>() -> (Bound<K>, Bound<K>) {
            (Bound::Included(K::default()), Bound::Excluded(K::default()))
        }

        /// `Date`／`Timestamp` 列（`i64` へ昇格済み）・リテラルともに小数を
        /// 持たない厳密な整数キーのための境界導出（`sql::scalar_plan::
        /// id_bounds` の非整数リテラル処理を要しない単純版）。
        fn exact_i64_bounds(op: CompareOp, v: i64) -> (Bound<i64>, Bound<i64>) {
            match op {
                CompareOp::Eq => (Bound::Included(v), Bound::Included(v)),
                CompareOp::Gt => v
                    .checked_add(1)
                    .map(|l| (Bound::Included(l), Bound::Unbounded))
                    .unwrap_or_else(empty),
                CompareOp::Ge => (Bound::Included(v), Bound::Unbounded),
                CompareOp::Lt => v
                    .checked_sub(1)
                    .map(|u| (Bound::Unbounded, Bound::Included(u)))
                    .unwrap_or_else(empty),
                CompareOp::Le => (Bound::Unbounded, Bound::Included(v)),
            }
        }

        /// `Uuid` 列（`u128` へ写像済み）向けの [`exact_i64_bounds`] の
        /// `u128` 版。
        fn exact_u128_bounds(op: CompareOp, v: u128) -> (Bound<u128>, Bound<u128>) {
            match op {
                CompareOp::Eq => (Bound::Included(v), Bound::Included(v)),
                CompareOp::Gt => v
                    .checked_add(1)
                    .map(|l| (Bound::Included(l), Bound::Unbounded))
                    .unwrap_or_else(empty),
                CompareOp::Ge => (Bound::Included(v), Bound::Unbounded),
                CompareOp::Lt => v
                    .checked_sub(1)
                    .map(|u| (Bound::Unbounded, Bound::Included(u)))
                    .unwrap_or_else(empty),
                CompareOp::Le => (Bound::Unbounded, Bound::Included(v)),
            }
        }

        let column = self.typed_columns.get(column_index)?.as_ref()?;
        // `range_slots` が返す時点でスロット昇順に整列済み
        // （`candidates_id_range`/`candidates_typed_range` と同じ契約）
        // のため、ここで再整列しない。
        match (column, value) {
            (OrderedColumnIndex::I64(pairs), TypedLiteral::Date(d)) => {
                let (lower, upper) = exact_i64_bounds(op, i64::from(*d));
                range_slots(pairs, lower, upper)
            }
            (OrderedColumnIndex::I64(pairs), TypedLiteral::Timestamp(t)) => {
                let (lower, upper) = exact_i64_bounds(op, *t);
                range_slots(pairs, lower, upper)
            }
            (OrderedColumnIndex::I128(pairs, col_scale), TypedLiteral::Numeric(d)) => {
                let (lower, upper) = crate::numeric::rescale_bounds_for_column(d, *col_scale, op)?;
                range_slots(pairs, lower, upper)
            }
            (OrderedColumnIndex::U128(pairs), TypedLiteral::Uuid(u)) => {
                let (lower, upper) = exact_u128_bounds(op, uuid_to_sortable_u128(u.as_bytes()));
                range_slots(pairs, lower, upper)
            }
            // `Bytea`（`OrderedColumnIndex` に対応 variant を持たず未対応の
            // まま）・列型とリテラル型の不一致（構造的に到達しないはずだが
            // untrusted 由来の対応関係への多層防御）はいずれも索引未対応
            // として扱う。
            _ => None,
        }
    }

    /// `column_index` 列（`TEXT` 列限定）の値ごとのグループを、値のバイト列
    /// 昇順（[`Self::build`] の `TextColumnIndex.values` と同じ順序）で列挙する
    /// （Issue #475: `sql::group_by` の `WHERE` なし `GROUP BY` 列挙形が使う）。
    /// 列が `TEXT` でない・未知の列は `None`。各グループのスロット列は昇順。
    /// [`Self::build`] は個々の `TEXT` 列について、非 `NULL` 値を**すべて**
    /// 索引化する（`Some`）か、まったく索引化しない（`None`。予算超過等に
    /// よる索引全体の構築失敗、または平均値長ゲート〔Issue #632・モジュール
    /// ドキュメント「列単位の索引対象除外」参照〕による当該列の除外の
    /// いずれか）かのいずれかであり、**ある列が `Some` として返る場合に**
    /// 一部の値だけを欠落させたまま索引を返すことはないため、ここで列挙
    /// される値は当該列の可視行が実際に持つ非 `NULL` 値の**全体**である
    /// （一部スキップによる取りこぼしを呼び出し元が心配する必要はない）。
    pub(crate) fn column_groups(
        &self,
        column_index: usize,
    ) -> Option<impl Iterator<Item = (&str, &[u32])> + '_> {
        let column = self.columns.get(column_index)?.as_ref()?;
        Some((0..column.values.len()).filter_map(move |i| {
            let value = column.values.get(i)?.as_str();
            let value_index = u32::try_from(i).ok()?;
            let slots = column.slots_for_value_index(value_index)?;
            Some((value, slots))
        }))
    }

    /// `column_index` 列（`TEXT` 列限定）が `NULL`（＝索引のどの値エントリにも
    /// 現れない）である可視行のスロットを昇順で返す（Issue #475:
    /// `sql::group_by` の `WHERE` なし `GROUP BY` 列挙形が NULL グループを
    /// 補完するために使う）。列が `TEXT` でない・未知の列は `None`。
    ///
    /// `row_count`（索引構築時の全スロット数）長のビットマップで索引済み全値の
    /// スロットを被覆し、被覆されなかったスロットを NULL とみなす。
    /// [`Self::build`] のドキュメントどおり「索引化された値の集合」は当該列の
    /// 非 `NULL` 値の全体であるため、この差分計算は正確に NULL 行と一致する
    /// （`declarative_filter::MetadataFilter::matches` の NULL 常時不一致判定と
    /// 同じ意味論。モジュールドキュメント参照）。
    pub(crate) fn slots_without_value(&self, column_index: usize) -> Option<Vec<u32>> {
        let column = self.columns.get(column_index)?.as_ref()?;
        let mut covered: Vec<bool> = Vec::new();
        covered.try_reserve_exact(self.row_count).ok()?;
        covered.resize(self.row_count, false);
        for &slot in &column.slots {
            if let Some(flag) = covered.get_mut(slot as usize) {
                *flag = true;
            }
        }
        let mut out: Vec<u32> = Vec::new();
        for (idx, &is_covered) in covered.iter().enumerate() {
            if !is_covered {
                let slot = u32::try_from(idx).ok()?;
                out.try_reserve(1).ok()?;
                out.push(slot);
            }
        }
        Some(out)
    }

    /// `id` に対する範囲述語（単純比較）向け照会。`id_index` が `None`
    /// （`id > 2^53` を含む行がある）の場合は `None`（呼び出し元が全走査へ
    /// 縮退する契機。モジュールドキュメント参照）。戻り値は昇順 `Vec<u32>`。
    ///
    /// `id_index` は `id` 昇順（同一 `id` 内はスロット昇順）に整列済みのため、
    /// 範囲の両端を [`slice::partition_point`]（二分探索）で求め、対象区間
    /// だけを複製する（Issue #474。旧実装は全件の線形走査＋フィルタだった）。
    /// 区間内は `id` 昇順だがスロット昇順とは限らない（異なる `id` 間で挿入
    /// 順が保たれるだけ）ため、返す前に `sort_unstable` で候補列の共通契約
    /// （スロット昇順）へ揃える。
    pub(crate) fn candidates_id_range(
        &self,
        lower: std::ops::Bound<u64>,
        upper: std::ops::Bound<u64>,
    ) -> Option<Vec<u32>> {
        use std::ops::Bound;
        let idx = self.id_index.as_ref()?;
        let start = match lower {
            Bound::Unbounded => 0,
            Bound::Included(l) => idx.partition_point(|(id, _)| *id < l),
            Bound::Excluded(l) => idx.partition_point(|(id, _)| *id <= l),
        };
        let end = match upper {
            Bound::Unbounded => idx.len(),
            Bound::Included(u) => idx.partition_point(|(id, _)| *id <= u),
            Bound::Excluded(u) => idx.partition_point(|(id, _)| *id < u),
        };
        if start >= end {
            return Some(Vec::new());
        }
        let mut out: Vec<u32> = Vec::new();
        out.try_reserve_exact(end - start).ok()?;
        out.extend(idx[start..end].iter().map(|(_, slot)| *slot));
        out.sort_unstable(); // sort-determinism: allow u32 スロット番号の全順序（重複なし）による昇順整列
        Some(out)
    }

    /// 数値・日時・`NUMERIC`・`UUID` 列（Issue #893）に対する範囲述語（等価・
    /// 単純比較）向け照会。`column_index` が `TEXT`／`ENUM`（`candidates_for`
    /// の対象）である・未知の列である・[`OrderedColumnIndex`] の variant が
    /// `lower`／`upper` の [`TypedKey`] 型と一致しない、のいずれかは `None`
    /// （判定不能＝索引未対応。呼び出し元は全走査へ縮退する契機として扱う。
    /// `id` を除く型不一致は `sql::scalar_plan` の預言的述語構築の契約により
    /// 通常到達しないが、防御的に fail-closed とする）。一致 0 件は
    /// `Some(vec![])` を返し `None` と区別する。戻り値は昇順 `Vec<u32>`。
    ///
    /// production 経路（`Self::candidates_for` の `TypedCompare` 分岐）は
    /// `MetadataFilter::TypedCompare` を直接消費するため `TypedKey` へ正規化
    /// せず本メソッドを経由しない。本メソッドは `OrderedColumnIndex` の
    /// 構築・照会ロジックそのものを型ごとに検証する単体テスト専用の入口
    /// として残す（`#[cfg_attr(not(test), allow(dead_code))]`。`TypedKey`
    /// と同型のテスト専用到達パターン）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn candidates_typed_range(
        &self,
        column_index: usize,
        lower: std::ops::Bound<TypedKey>,
        upper: std::ops::Bound<TypedKey>,
    ) -> Option<Vec<u32>> {
        use std::ops::Bound;
        let column = self.typed_columns.get(column_index)?.as_ref()?;
        /// `Bound<TypedKey>` を `Bound<K>` へ変換する。variant が一致しない
        /// 場合は `None`（呼び出し元は索引未対応として扱う）。
        fn map_bound<K>(
            bound: std::ops::Bound<TypedKey>,
            extract: impl Fn(TypedKey) -> Option<K>,
        ) -> Option<std::ops::Bound<K>> {
            Some(match bound {
                Bound::Unbounded => Bound::Unbounded,
                Bound::Included(k) => Bound::Included(extract(k)?),
                Bound::Excluded(k) => Bound::Excluded(extract(k)?),
            })
        }
        match column {
            OrderedColumnIndex::I64(pairs) => {
                let extract = |k: TypedKey| match k {
                    TypedKey::I64(v) => Some(v),
                    _ => None,
                };
                range_slots(
                    pairs,
                    map_bound(lower, extract)?,
                    map_bound(upper, extract)?,
                )
            }
            OrderedColumnIndex::F64Sortable(pairs) => {
                let extract = |k: TypedKey| match k {
                    TypedKey::F64Sortable(v) => Some(v),
                    _ => None,
                };
                range_slots(
                    pairs,
                    map_bound(lower, extract)?,
                    map_bound(upper, extract)?,
                )
            }
            OrderedColumnIndex::I128(pairs, _scale) => {
                let extract = |k: TypedKey| match k {
                    TypedKey::I128(v) => Some(v),
                    _ => None,
                };
                range_slots(
                    pairs,
                    map_bound(lower, extract)?,
                    map_bound(upper, extract)?,
                )
            }
            OrderedColumnIndex::U128(pairs) => {
                let extract = |k: TypedKey| match k {
                    TypedKey::U128(v) => Some(v),
                    _ => None,
                };
                range_slots(
                    pairs,
                    map_bound(lower, extract)?,
                    map_bound(upper, extract)?,
                )
            }
        }
    }

    /// `metadata_filters`（`TEXT` 列の等価・前方一致）と `id_preds`（`id` の
    /// 単純比較。`sql::scalar_plan::classify_scalar_plan` が
    /// `ScalarPlan::PlainScan` 以外へ分類した述語のみを渡す契約）から候補
    /// スロット集合を導出する（Issue #474）。各述語の候補を個別に取得し
    /// （`None` は「判定不能」＝列未索引または `id_index` が `None`）、複数
    /// 述語は昇順ベクトルの交差（2 本指マージ）で結合する。交差はスロットを
    /// 「絞る」ことしかできず「通す」ことはできないため、呼び出し元
    /// （`sql::exec`）が候補行にも引き続き `on_visible_row`（`matches_all`＋
    /// 式述語）を適用する契約と組み合わさって fail-closed が成立する（本
    /// メソッド自体が正しさの唯一の防御ではない）。
    ///
    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID` 列の範囲述語（`FilterOp::
    /// TypedCompare`。Issue #891・TASK-199 で production 結線済み）は
    /// `metadata_filters` に混在したまま渡ってくる（`sql::parser::
    /// bind_where_predicates` が `TEXT` 等価・前方一致と同じ
    /// `Vec<MetadataFilter>` へ束縛するため）。[`Self::candidates_for`] が
    /// 述語の種類（`Equals`／`StartsWith`／`TypedCompare`）ごとに参照する
    /// 索引（`self.columns`／`self.typed_columns`）を内部で切り替えるため、
    /// この関数側で型ごとに分岐する必要はない（Issue #893 production 接続）。
    pub(crate) fn resolve_candidates(
        &self,
        metadata_filters: &[MetadataFilter],
        id_preds: &[crate::sql::scalar_plan::IdPredicate],
    ) -> CandidateResolution {
        if metadata_filters.is_empty() && id_preds.is_empty() {
            // `classify_scalar_plan` が `PlainScan` 以外を返す限り到達しない
            // 呼び出し規約違反だが、防御的に fail-closed へ倒す。
            return CandidateResolution::FallbackNoIndex;
        }
        // 述語ごとの候補列を全件 `Vec<Vec<u32>>` に集めてから交差する実装は、
        // 交差前の累計保持量に上限が無かった（codex-review P1 指摘・PR #601）。
        // 許可リスト上限の述語数（最大 256 件・`sql::allowlist` 参照）それぞれが
        // 索引済み行の大半に一致する入力（同一 TEXT 値の等価条件を 256 個
        // 並べる等）では、選択度切替（縮退判定）が交差**後**にしか働かないため、
        // 交差前に 256 本の候補列（各最大 `row_count` 件）を同時保持し
        // `MAX_ARENA_ROWS`（約 100 万行）規模ではコピーだけで約 1 GiB を
        // 追加確保しうる。交差は「述語を追加するほど結果が単調非増加になる」
        // （2 本指マージの結果は常に両オペランド以下の長さ）性質を持つため、
        // 述語ごとの候補列を生成するたびにその場で累積へ交差し、次の述語へ
        // 進む前に前の候補列を破棄する（保持するのは累積候補列と直近生成した
        // 1 本のみ）。これにより述語数に依存せず、保持量は個々の候補列の
        // 最大サイズ（`row_count` に比例）で頭打ちになる。
        //
        // 交差前に最小の候補列から処理する最適化（Issue #474 時点の実装）は
        // 全列の長さを事前に知る必要があり本対応と両立しないため撤去した。
        // 累積が空集合になった時点で以降の述語を評価せず打ち切る（交差は
        // 単調非増加のため以降の交差結果も必ず空集合）ことで、代わりに早期
        // 打ち切りによる実用上の性能劣化を抑える。
        let mut accumulated: Option<Vec<u32>> = None;
        for filter in metadata_filters {
            if accumulated.as_deref().is_some_and(<[u32]>::is_empty) {
                break;
            }
            let slots = match self.candidates_for(filter) {
                Some(slots) => slots,
                None => return CandidateResolution::FallbackNoIndex,
            };
            accumulated = Some(match accumulated {
                None => slots,
                Some(acc) => match intersect_sorted(&acc, &slots) {
                    Some(v) => v,
                    None => return CandidateResolution::FallbackNoIndex,
                },
            });
        }
        for pred in id_preds {
            if accumulated.as_deref().is_some_and(<[u32]>::is_empty) {
                break;
            }
            let Some((lower, upper)) = crate::sql::scalar_plan::id_bounds(pred) else {
                return CandidateResolution::FallbackNoIndex;
            };
            let slots = match self.candidates_id_range(lower, upper) {
                Some(slots) => slots,
                None => return CandidateResolution::FallbackNoIndex,
            };
            accumulated = Some(match accumulated {
                None => slots,
                Some(acc) => match intersect_sorted(&acc, &slots) {
                    Some(v) => v,
                    None => return CandidateResolution::FallbackNoIndex,
                },
            });
        }
        // 上の 2 ループは冒頭の空チェックにより少なくとも 1 回は候補列を
        // 生成するため、ここで `None` のままということはない。
        let intersected = accumulated.unwrap_or_default();
        let hits = intersected.len() as u64;
        let row_count = self.row_count as u64;
        // 選択度切替（`sql::hnsw_cache` の `full_scan_ratio` と同型の
        // `checked_mul` による整数比較。丸め誤差を避けるため両辺を分母倍する）。
        let exceeds = match hits.checked_mul(DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_DENOMINATOR) {
            Some(lhs) => {
                match row_count.checked_mul(DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_NUMERATOR) {
                    Some(rhs) => lhs > rhs,
                    // `row_count` は `MAX_ARENA_ROWS`（100 万行程度）で頭打ちの
                    // ため現実的には到達しないが、桁あふれた場合は判定不能
                    // として選択度超過側（保守的に全走査）へ倒す。
                    None => true,
                }
            }
            None => true,
        };
        if exceeds {
            return CandidateResolution::FallbackSelectivity;
        }
        CandidateResolution::Use(intersected)
    }

    /// 索引全体（`TEXT` 列の辞書・CSR・`equality`・`id_index`）の概算ヒープ
    /// バイト量を実走査で計算する（`O(索引サイズ)`）。[`Self::build`] 完了時に
    /// 1 回だけ呼び、結果を `approx_bytes` フィールドへ確定させる。索引は
    /// 構築後不変のためこの計算はここでしか行わない
    /// （[`Self::approx_heap_bytes`] のドキュメント参照。codex-review P2
    /// 対応・PR #569）。
    fn compute_approx_heap_bytes(&self) -> usize {
        let columns_bytes: usize = self
            .columns
            .iter()
            .filter_map(|c| c.as_ref())
            .map(TextColumnIndex::approx_heap_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let id_index_bytes = self
            .id_index
            .as_ref()
            .map(|v| v.len().saturating_mul(std::mem::size_of::<(u64, u32)>()))
            .unwrap_or(0);
        // 数値・日時・`NUMERIC`・`UUID` 列の順序索引（Issue #893）。固定長
        // キーのため `capacity()`（構築時に `row_count` ちょうどへ厳密確保
        // 済み）が実確保量とそのまま一致する。
        let typed_columns_bytes: usize = self
            .typed_columns
            .iter()
            .filter_map(|c| c.as_ref())
            .map(|c| match c {
                OrderedColumnIndex::I64(v) => v
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(i64, u32)>()),
                OrderedColumnIndex::F64Sortable(v) => v
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(u64, u32)>()),
                OrderedColumnIndex::I128(v, _scale) => v
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(i128, u32)>()),
                OrderedColumnIndex::U128(v) => v
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(u128, u32)>()),
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        columns_bytes
            .saturating_add(id_index_bytes)
            .saturating_add(typed_columns_bytes)
    }

    /// 概算ヒープバイト量（容量判定用）を返す O(1) アクセサ。
    /// [`Self::build`] 完了時に確定した `approx_bytes` フィールドをそのまま
    /// 返すだけで索引を再走査しない（[`ScalarIndexCache::insert`] が書き込み
    /// ロック保持中に呼んでも既存索引の再走査コストを引き起こさない。
    /// codex-review P2 対応・PR #569）。
    fn approx_heap_bytes(&self) -> usize {
        self.approx_bytes
    }

    /// 索引構築時のスナップショット行数（Issue #474: `sql::exec` の索引↔
    /// スナップショット同一性ガードが `snapshot.arena().len()` と突き合わせる）。
    pub(crate) fn row_count(&self) -> usize {
        self.row_count
    }

    /// 索引構築時のテーブル世代（Issue #474: `sql::exec` の索引↔スナップショット
    /// 同一性ガードが `snapshot.built_table_generation_for_index()` と突き合わせる）。
    pub(crate) fn built_table_generation(&self) -> u64 {
        self.built_table_generation
    }
}

/// [`ScalarIndexCache`] の観測用統計。テナント ID・行 ID・値等の機微情報は一切
/// 含まない（`core.rs::PrefilterCacheStats` と同じ方針）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScalarIndexCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stale_evictions: u64,
    pub capacity_evictions: u64,
    pub builds: u64,
    pub build_failures: u64,
    pub entries: usize,
    /// Issue #474: `sql::exec` が索引経路（`ScalarIndex::resolve_candidates`
    /// の `CandidateResolution::Use`）を実際に消費してクエリを実行した回数。
    pub index_scans: u64,
    /// Issue #654: `index_scans` のうち、候補行を `VectorArena` へ複製せず
    /// キャッシュ済みスナップショットの `VectorArena` を借用したまま
    /// スロットマスクで直接探索できた回数（`index_scans` の部分集合）。
    pub index_mask_scans: u64,
    /// Issue #660 系（索引経路の残存コスト削減。Step 4）: `index_mask_scans`
    /// のうち、候補スロットへの `on_visible_row` 再適用（masked decode ＋
    /// `matches_all` ＋ 式述語評価）自体を省き、索引の候補集合をそのまま
    /// 一致集合として `SearchProvider::search_subset` のマスクへ渡した回数
    /// （`index_mask_scans` の部分集合）。信頼してよい不変条件は
    /// `sql::aggregate::count_star_only` のドキュメント（1〜3）と同じで、
    /// DISTANCE 経路では「`COUNT(*)` のみ」の代わりに「hybrid でない・
    /// HNSW `Subset` 形状でない」を要求する（`sql::exec` の該当分岐参照）。
    pub index_trusted_mask_scans: u64,
    /// Issue #474: `sql::exec` が索引対応述語を持つクエリで全走査へ縮退した
    /// 回数（`FallbackNoIndex`／`FallbackSelectivity`／同一性ガード不一致
    /// いずれも含む）。
    pub plain_scan_fallbacks: u64,
    /// Issue #475: `sql::aggregate`／`sql::group_by` が索引経路（候補走査・
    /// `GROUP BY` キー列挙形）を実際に消費して集計クエリを実行した回数
    /// （`index_scans`/`plain_scan_fallbacks` と別枠。SELECT 経路の消費と
    /// 区別する）。
    pub aggregate_index_scans: u64,
    /// Issue #475: 集計・`GROUP BY` クエリが索引対応述語・形状を持ちながら
    /// 全走査へ縮退した回数（`FallbackNoIndex`／`FallbackSelectivity`／
    /// 同一性ガード不一致／構築断念／NULL 補完不能のいずれも含む）。
    pub aggregate_plain_scan_fallbacks: u64,
}

struct ScalarIndexCacheEntry {
    table: String,
    index: Arc<ScalarIndex>,
    last_used: u64,
}

/// [`UnbuildableEntry`] が記録する失敗の起点（PR #603 codex-review P2 指摘
/// 対応）。`(table, ctx)` あたり最新 1 件のみを保持する既存の上書き方式は
/// 変えず、どちらの起点で「同一世代では再試行しても無駄」と判定したかを
/// 区別して記録するためだけに使う（[`ScalarIndexCache::is_capture_known_
/// unbuildable`] によるゲート判定自体は起点を問わず同一世代のヒットで
/// ブロックする——採取済みでも構築側の予算超過等で不能なら、次に採取から
/// やり直しても同じ結果になるため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnbuildableReason {
    /// `sql::aggregate::capture_scalar_index_snapshot` 自体が採取失敗
    /// （容量超過・復号不能・NULL 行混在等）で `None` を返した。
    Capture,
    /// 採取（アリーナスナップショット構築）自体は成功した——`SqlArenaCache`
    /// ヒットにより既に温かいスナップショットが手元にある——が、そこから
    /// [`ScalarIndex::build`] を実行した結果が失敗した（索引固有の容量超過
    /// 等）。
    Build,
}

/// Issue #475 codex-review P2 対応: `sql::aggregate::capture_scalar_index_snapshot`
/// が「この `(table, ctx)`・世代では索引の piggyback 構築が採取失敗（容量超過・
/// 復号不能・NULL 行混在等）で確定的に不可能だった」ことを記録するエントリ。
/// `ScalarIndexCacheEntry`（構築成功した索引本体）とは別枠——同一世代内は
/// [`ScalarIndexCache::is_capture_known_unbuildable`] がこの記録をヒットさせ、
/// 集計・`GROUP BY` クエリのたびに同じ全走査・全量デコードの採取を繰り返す
/// ことを避ける（構築成功索引のキャッシュとは独立に世代が進めば自然に無効化
/// される。テーブル単位世代のみをキーにし、行内容・存在情報は保持しない）。
///
/// PR #603 codex-review P2 指摘対応: `reason`（[`UnbuildableReason`]）は、
/// アリーナ採取自体の失敗（`Capture`）と、温かいスナップショットからの
/// [`ScalarIndex::build`] 失敗（`Build`）を記録上区別する。以前は後者を
/// このエントリへ一切記録しておらず、`SqlArenaCache` がヒットし続ける限り
/// 同一世代の集計・`GROUP BY` クエリごとに毎回スナップショット全体の
/// デコード・文字列複製をやり直してから全走査へ縮退していた。
struct UnbuildableEntry {
    table: String,
    ctx: PolicyContext,
    generation: u64,
    last_used: u64,
    reason: UnbuildableReason,
}

#[derive(Default)]
struct ScalarIndexCacheState {
    entries: Vec<ScalarIndexCacheEntry>,
    unbuildable: Vec<UnbuildableEntry>,
}

/// `(table, ctx)` × テーブル単位世代でキャッシュする [`ScalarIndex`] キャッシュ
/// 本体（モジュールドキュメント「キャッシュ」参照）。
pub(crate) struct ScalarIndexCache {
    state: RwLock<ScalarIndexCacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
    builds: AtomicU64,
    build_failures: AtomicU64,
    index_scans: AtomicU64,
    index_mask_scans: AtomicU64,
    index_trusted_mask_scans: AtomicU64,
    plain_scan_fallbacks: AtomicU64,
    aggregate_index_scans: AtomicU64,
    aggregate_plain_scan_fallbacks: AtomicU64,
}

impl ScalarIndexCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(ScalarIndexCacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
            builds: AtomicU64::new(0),
            build_failures: AtomicU64::new(0),
            index_scans: AtomicU64::new(0),
            index_mask_scans: AtomicU64::new(0),
            index_trusted_mask_scans: AtomicU64::new(0),
            plain_scan_fallbacks: AtomicU64::new(0),
            aggregate_index_scans: AtomicU64::new(0),
            aggregate_plain_scan_fallbacks: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、`read_txn` のスナップショットにおけるテーブル
    /// 世代と整合するエントリを探す。契約は
    /// [`crate::sql::arena_cache::SqlArenaCache::lookup`] と同一（`read_txn` が
    /// 古いだけの可能性があるため、破棄は `storage` から読んだ真の最新世代より
    /// 厳密に古いと確認できた場合のみ行う）。
    pub(crate) fn lookup(
        &self,
        storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<Arc<ScalarIndex>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        // 以下、ヒットで早期 `return` する 1 経路を除き `None` に到達する分岐
        // （ロック毒化・世代読み取り失敗・未登録・世代不一致のいずれも）で
        // `self.misses` を明示的に加算する（codex-review P2 対応・PR #569。
        // 以前は `stats()` 側の `fetch_add(0)` のみで実質常にゼロだった）。
        let Ok(mut guard) = self.state.write() else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Some(position) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.index.built_ctx == *ctx)
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Some(built_generation) = guard
            .entries
            .get(position)
            .map(|e| e.index.built_table_generation())
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if built_generation == current_generation {
            let Some(entry) = guard.entries.get_mut(position) else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            entry.last_used = seq;
            let index = Arc::clone(&entry.index);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(index);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let Ok(true_current_generation) = storage.table_generation(table) else {
            return None;
        };
        if built_generation < true_current_generation {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
        }
        None
    }

    /// 新規構築した索引を挿入する。**`SqlArenaCache::insert` とは意図的に非対称**
    /// （モジュールドキュメント「キャッシュ」参照）: 世代不一致・ロック毒化・
    /// 世代読み取り失敗のいずれも `None` を返し、キャッシュへ反映しないだけで
    /// なく呼び出し元へも一切渡さない（`core.rs::PrefilterCache::insert`・
    /// Issue #280 と同じ契約）。
    pub(crate) fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        index: ScalarIndex,
    ) -> Option<Arc<ScalarIndex>> {
        let index = Arc::new(index);
        self.builds.fetch_add(1, Ordering::Relaxed);

        let mut guard = self.state.write().ok()?;
        let Ok(read_txn) = storage.db().begin_read() else {
            return None;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table)
        else {
            return None;
        };
        if index.built_table_generation() != current_generation {
            return None;
        }

        let own_bytes = index.approx_heap_bytes();

        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.index.built_ctx == *ctx)
        {
            guard.entries.remove(pos);
        }

        let before = guard.entries.len();
        guard
            .entries
            .retain(|e| e.table != table || e.index.built_table_generation() == current_generation);
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        if own_bytes > MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超える索引は常駐させないが、世代整合済みなので
            // 呼び出し元へは `Some` で返す（この 1 回のクエリ限りで使ってよい。
            // `SqlArenaCache::insert` と同じ「単体超過時の縮退」方針）。
            return Some(index);
        }

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.index.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_SCALAR_INDEX_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return Some(index);
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.index.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(ScalarIndexCacheEntry {
            table: table.to_string(),
            index: Arc::clone(&index),
            last_used: seq,
        });
        Some(index)
    }

    pub(crate) fn stats(&self) -> ScalarIndexCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        ScalarIndexCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            builds: self.builds.load(Ordering::Relaxed),
            build_failures: self.build_failures.load(Ordering::Relaxed),
            entries,
            index_scans: self.index_scans.load(Ordering::Relaxed),
            index_mask_scans: self.index_mask_scans.load(Ordering::Relaxed),
            index_trusted_mask_scans: self.index_trusted_mask_scans.load(Ordering::Relaxed),
            plain_scan_fallbacks: self.plain_scan_fallbacks.load(Ordering::Relaxed),
            aggregate_index_scans: self.aggregate_index_scans.load(Ordering::Relaxed),
            aggregate_plain_scan_fallbacks: self
                .aggregate_plain_scan_fallbacks
                .load(Ordering::Relaxed),
        }
    }

    /// [`ScalarIndex::build`] が失敗したことを観測用統計へ計上する
    /// （`sql::exec::execute_statement_with_cache` の gated 構築が呼ぶ。
    /// モジュールドキュメント「fail-closed の適用範囲」参照）。
    pub(crate) fn record_build_failure(&self) {
        self.build_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #474: `sql::exec` が索引経路（`CandidateResolution::Use`）を
    /// 実際に消費してクエリを実行したことを観測用統計へ計上する。
    pub(crate) fn record_index_scan(&self) {
        self.index_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #654: `sql::exec` が候補削減を「絞る」だけに使い、`VectorArena`
    /// への複製（`build_from_cached_rls_rows_subset`）を経ずキャッシュ済み
    /// スナップショットの `VectorArena` を借用したままスロットマスクで直接
    /// 探索できたことを観測用統計へ計上する（`index_scans` の部分集合）。
    pub(crate) fn record_index_mask_scan(&self) {
        self.index_mask_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #660 系（Step 4）: `sql::exec` が索引の候補集合を一致集合として
    /// 信頼し、候補スロットへの `on_visible_row` 再適用を省いたことを観測用
    /// 統計へ計上する（`index_mask_scans` の部分集合）。テストが「索引信頼
    /// 経路が実際に使われた」ことを非 vacuous に固定するために参照する。
    pub(crate) fn record_index_trusted_mask_scan(&self) {
        self.index_trusted_mask_scans
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #474: `sql::exec` が索引対応述語を持つクエリで全走査へ縮退した
    /// ことを観測用統計へ計上する（`FallbackNoIndex`／`FallbackSelectivity`／
    /// 索引↔スナップショット同一性ガード不一致のいずれも呼ぶ）。
    pub(crate) fn record_plain_scan_fallback(&self) {
        self.plain_scan_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #475: `sql::aggregate`／`sql::group_by` が索引経路（候補走査・
    /// `GROUP BY` キー列挙形）を実際に消費して集計クエリを実行したことを
    /// 観測用統計へ計上する。
    pub(crate) fn record_aggregate_index_scan(&self) {
        self.aggregate_index_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #475: 集計・`GROUP BY` クエリが索引対応述語・形状を持ちながら
    /// 全走査へ縮退したことを観測用統計へ計上する。
    pub(crate) fn record_aggregate_plain_scan_fallback(&self) {
        self.aggregate_plain_scan_fallbacks
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #475 codex-review P2 対応: この `(table, ctx)` の現在のテーブル
    /// 世代について、集計・`GROUP BY` の索引構築（`sql::aggregate::
    /// capture_scalar_index_snapshot` による piggyback 採取、または温かい
    /// `SqlArenaSnapshot` からの [`ScalarIndex::build`]。PR #603 codex-review
    /// P2 指摘対応で後者も対象に含めた）が失敗済みと確定しているかを調べる。
    /// ヒットした場合、呼び出し元は採取・構築のいずれも試みず即座に全走査
    /// フォールバックへ委ねてよい（起点を区別せず判定する理由は
    /// [`UnbuildableReason`] のドキュメント参照）。世代が進んだ古い記録は
    /// 一致しないため自然にミスとして扱う（明示的な無効化は不要。世代不一致の
    /// 記録は次の [`Self::mark_capture_unbuildable`]／[`Self::mark_build_
    /// unbuildable`] 呼び出しで上書きされるまで残るが、`table` は非機微情報で
    /// ありサイズは [`MAX_SCALAR_INDEX_CACHE_ENTRIES`] で有界のため安全性に
    /// 影響しない）。
    pub(crate) fn is_capture_known_unbuildable(
        &self,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> bool {
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
        else {
            return false;
        };
        let Ok(guard) = self.state.read() else {
            return false;
        };
        guard
            .unbuildable
            .iter()
            .any(|e| e.table == table && e.ctx == *ctx && e.generation == current_generation)
    }

    /// Issue #475 codex-review P2 対応: `capture_scalar_index_snapshot` が
    /// 採取失敗（`None`）を返した直後に呼び出し、[`Self::is_capture_known_
    /// unbuildable`] が次回以降の同一世代呼び出しをこの採取試行自体をスキップ
    /// させられるよう記録する（[`UnbuildableReason::Capture`]）。テーブル
    /// 世代の読み取りに失敗した場合は記録せず終える（fail-closed。記録できな
    /// くても呼び出し元の集計クエリ自体は全走査へフォールバック済みで正しさに
    /// 影響しない）。
    ///
    /// PR #603 codex-review P2 指摘対応: 記録する世代は、実際に採取（全走査）
    /// を試みた**その** `read_txn` のスナップショット世代
    /// （`catalog::table_generation_in_txn`）でなければならない。以前は
    /// `Storage::table_generation`（最新コミット世代）を使っていたため、採取中
    /// に別トランザクションがコミットすると世代 `G` での採取失敗が `G+1` へ
    /// 誤記録されることがあった（[`Self::is_capture_known_unbuildable`] は
    /// `read_txn` のスナップショット世代と突き合わせるため、この不一致により
    /// 実際には構築可能な `G+1` の採取が次の更新までスキップされてしまう）。
    pub(crate) fn mark_capture_unbuildable(
        &self,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) {
        self.mark_unbuildable(read_txn, table, ctx, UnbuildableReason::Capture);
    }

    /// PR #603 codex-review P2 指摘対応: `sql::aggregate::
    /// ensure_scalar_index_snapshot` が、`SqlArenaCache` ヒットにより既に
    /// 温かい `SqlArenaSnapshot` を持っている状態から [`ScalarIndex::build`]
    /// を実行し、その構築自体が失敗した直後に呼び出す
    /// （[`UnbuildableReason::Build`]）。この記録がないと、同一世代のうちは
    /// `SqlArenaCache` がヒットし続ける限り集計・`GROUP BY` クエリのたびに
    /// 同じ（確定的に失敗する）構築を繰り返し試み、スナップショット全体の
    /// デコード・文字列複製コストを毎回払ってから全走査へ縮退していた。
    /// [`Self::mark_capture_unbuildable`] と同じ fail-closed 方針（世代を
    /// 読めない場合は記録しないだけで、呼び出し元の集計クエリ自体は失敗
    /// しない）を踏襲する。
    pub(crate) fn mark_build_unbuildable(
        &self,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) {
        self.mark_unbuildable(read_txn, table, ctx, UnbuildableReason::Build);
    }

    /// [`Self::mark_capture_unbuildable`]／[`Self::mark_build_unbuildable`]
    /// が共有する実処理（`(table, ctx)` あたり最新 1 件のみを保持する既存の
    /// 上書き方式・LRU 追い出しはいずれも起点によらず共通のため、`reason` の
    /// 違いのみをパラメータ化する）。
    fn mark_unbuildable(
        &self,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
        reason: UnbuildableReason,
    ) {
        let Ok(generation) = crate::catalog::table_generation_in_txn(read_txn, table) else {
            return;
        };
        let Ok(mut guard) = self.state.write() else {
            return;
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(entry) = guard
            .unbuildable
            .iter_mut()
            .find(|e| e.table == table && e.ctx == *ctx)
        {
            entry.generation = generation;
            entry.last_used = seq;
            entry.reason = reason;
            return;
        }
        while guard.unbuildable.len() >= MAX_SCALAR_INDEX_CACHE_ENTRIES {
            let Some((idx, _)) = guard
                .unbuildable
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
            else {
                break;
            };
            guard.unbuildable.remove(idx);
        }
        guard.unbuildable.push(UnbuildableEntry {
            table: table.to_string(),
            ctx: ctx.clone(),
            generation,
            last_used: seq,
            reason,
        });
    }
}

/// `sql::exec::execute_statement_with_cache` へ渡すキャッシュアクセス束
/// （`sql::arena_cache::ArenaCacheAccess`・`sql::sparse_cache::SparseCacheAccess`
/// と同じ理由・構造）。
pub(crate) struct ScalarCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a ScalarIndexCache,
    /// 索引宣言（`CREATE INDEX`）を構築対象の絞り込みに使うか（Issue #1065）。
    /// 起動時 opt-in（`SearchEngineKind::Hnsw`）を上位スイッチとし、`core.rs`
    /// が `self.hnsw_state.is_some()` を渡す。`false`（opt-in なし）では宣言の
    /// 有無によらず常に `ScalarIndexTarget::Auto`（現行の全対応列自動）を使い、
    /// 既存の opt-in なし利用者の挙動を変えない
    /// （`docs/design/index-declaration-effects.md`）。
    pub(crate) declarations_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::VectorArena;
    use crate::catalog::ColumnDef;
    use crate::recovery::required_op_id::OperationId;
    use crate::row_codec::Value;
    use crate::storage::Visibility;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use std::ops::Bound;

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn op_id(label: &str) -> OperationId {
        OperationId::parse(label).expect("valid operation id")
    }

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("path", ColumnType::Text, true),
            ],
        )
    }

    fn create_table(storage: &Storage) {
        storage.create_table(&schema()).expect("create table");
    }

    fn insert(
        storage: &Storage,
        ctx: &PolicyContext,
        id: u64,
        kind: Option<&str>,
        path: Option<&str>,
        visibility: Visibility,
    ) {
        crate::tenant::insert_typed_row(
            storage,
            "docs",
            ctx,
            id,
            visibility,
            &[
                Value::Vector(vec![0.0, 0.0]),
                kind.map(|k| Value::Text(k.to_string()))
                    .unwrap_or(Value::Null),
                path.map(|p| Value::Text(p.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &op_id(&format!("seed-{id}")),
        )
        .expect("insert row");
    }

    /// テストのみに公開するアクセサ（`SqlArenaSnapshot` は crate 内部型のため、
    /// 索引側から世代・ctx を取り出す薄い橋渡し）。
    fn snapshot_from(
        storage: &Storage,
        ctx: &PolicyContext,
    ) -> (crate::sql::arena_cache::SqlArenaSnapshot, TableSchema) {
        let read_txn = storage.db().begin_read().expect("begin read");
        let schema = crate::catalog::get_table_schema_in_txn(&read_txn, "docs").expect("schema");
        let expected_dim = schema.vector_dim().expect("vector dim");
        let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
            expected_dim,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        );
        let hook = crate::rls::ImplicitRlsHook::new(ctx);
        let mut rls_capture = |id: u64,
                               tenant_id: &str,
                               visibility,
                               embedding: &[f32],
                               metadata: &[u8],
                               response_arena_bytes_in_use: usize|
         -> std::result::Result<(), crate::arena::ArenaError> {
            capture.push(
                id,
                tenant_id,
                visibility,
                embedding,
                metadata,
                response_arena_bytes_in_use,
            );
            Ok(())
        };
        let _built: VectorArena =
            crate::arena::VectorArena::build_filtered_with_rows_in_txn_capturing(
                &read_txn,
                "docs",
                hook.predicate(),
                |_, _, _, _| Ok(true),
                &mut rls_capture,
            )
            .expect("build arena");
        let table_generation =
            crate::catalog::table_generation_in_txn(&read_txn, "docs").expect("table generation");
        let (cache_arena, cache_metadata) = capture.finish("docs").expect("capture snapshot");
        (
            crate::sql::arena_cache::SqlArenaSnapshot::new(
                cache_arena,
                cache_metadata,
                ctx.clone(),
                table_generation,
            ),
            schema,
        )
    }

    #[test]
    fn build_indexes_equality_and_prefix_matching_matches_all_oracle() {
        let path = unique_db_path("scalar-index-basic");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(
            &storage,
            &ctx_a,
            1,
            Some("alpha"),
            Some("src/a.rs"),
            Visibility::Public,
        );
        insert(
            &storage,
            &ctx_a,
            2,
            Some("beta"),
            Some("src/b.rs"),
            Visibility::Public,
        );
        insert(&storage, &ctx_a, 3, Some("alpha"), None, Visibility::Public);
        insert(
            &storage,
            &ctx_a,
            4,
            None,
            Some("src/ab.rs"),
            Visibility::Public,
        );

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert_eq!(index.row_count(), 4);

        let kind_col = 1;
        let path_col = 2;

        let eq_alpha = index
            .candidates_equals(kind_col, "alpha")
            .expect("alpha indexed");
        assert_eq!(eq_alpha.len(), 2);

        let filters = declarative_filter_all(&schema);
        for filter in &filters {
            let expected = oracle_matches(&schema, &snapshot, filter);
            let actual = index.candidates_for(filter).expect("indexed column");
            assert_eq!(
                actual, expected,
                "filter {filter:?} must match full-scan oracle"
            );
        }

        let prefix_filter = MetadataFilter_starts_with(&schema, "path", "src/a");
        let expected = oracle_matches(&schema, &snapshot, &prefix_filter);
        let actual = index
            .candidates_for(&prefix_filter)
            .expect("indexed column");
        assert_eq!(actual, expected);

        // 存在しない値・NULL 列は空集合。
        let missing = index.candidates_for(&MetadataFilter_equals(&schema, "kind", "gamma"));
        assert_eq!(missing, Some(Vec::new()));
        let _ = path_col;
    }

    /// `IN`（`FilterOp::InText`。SQL-24・TASK-208 ポインタ）の候補が全走査
    /// オラクル（[`oracle_matches`]。`MetadataFilter::matches` を経由するため
    /// `eval` の三値論理をそのまま踏襲する）と完全一致することを固定する
    /// （値ごとの等価スロット和集合の導出が正しいことの検証）。
    #[test]
    fn in_text_candidates_matches_full_scan_oracle() {
        let path = unique_db_path("scalar-index-in-text-oracle");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(
            &storage,
            &ctx_a,
            1,
            Some("alpha"),
            Some("src/a.rs"),
            Visibility::Public,
        );
        insert(
            &storage,
            &ctx_a,
            2,
            Some("beta"),
            Some("src/b.rs"),
            Visibility::Public,
        );
        insert(&storage, &ctx_a, 3, Some("gamma"), None, Visibility::Public);
        insert(&storage, &ctx_a, 4, None, None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let in_filter = crate::declarative_filter::DeclarativeFilter::in_list(
            "kind",
            vec!["alpha".to_string(), "gamma".to_string(), "zzz".to_string()],
        )
        .bind(&schema)
        .expect("bind IN filter");
        let expected = oracle_matches(&schema, &snapshot, &in_filter);
        let actual = index.candidates_for(&in_filter).expect("indexed column");
        assert_eq!(actual, expected);
        assert_eq!(actual, vec![0, 2]);
    }

    /// SQL-24・TASK-208、Issue #914: `LIKE` の一般形（中間一致・後方一致・`_`）は
    /// 二次索引が対応せず `candidates_for` が `None` を返す（「一致 0 件」
    /// `Some(vec![])` と区別する。呼び出し元は全行走査へ縮退する）。
    #[test]
    fn candidates_for_like_general_form_is_none() {
        let path = unique_db_path("scalar-index-like-not-indexed");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(
            &storage,
            &ctx_a,
            1,
            Some("alpha"),
            Some("src/a.rs"),
            Visibility::Public,
        );

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let like_filter = crate::declarative_filter::DeclarativeFilter::like("path", "%a%")
            .bind(&schema)
            .expect("bind like filter");
        assert_eq!(index.candidates_for(&like_filter), None);
    }

    #[test]
    fn null_values_never_appear_in_any_index_entry() {
        let path = unique_db_path("scalar-index-null");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, None, None, Visibility::Public);
        insert(&storage, &ctx_a, 2, Some("x"), None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let eq_x = index.candidates_for(&MetadataFilter_equals(&schema, "kind", "x"));
        assert_eq!(eq_x, Some(vec![1]));
    }

    #[test]
    fn rls_partial_visibility_excludes_private_rows_from_dictionary() {
        let path = unique_db_path("scalar-index-rls");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        // `ctx_b` は自テナントの Private 行も見える構成にする（`PolicyContext::new`
        // は Public のみ可視の既定コンストラクタのため、Private 行を検証するには
        // `with_visibilities` で明示的に許可する必要がある）。
        let ctx_b =
            PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
                .expect("valid tenant with visibilities");
        insert(
            &storage,
            &ctx_a,
            1,
            Some("A-PRIVATE-1"),
            None,
            Visibility::Private,
        );
        insert(
            &storage,
            &ctx_b,
            2,
            Some("B-PRIVATE-1"),
            None,
            Visibility::Private,
        );
        insert(
            &storage,
            &ctx_b,
            3,
            Some("B-PUBLIC-1"),
            None,
            Visibility::Public,
        );

        let (snapshot, schema) = snapshot_from(&storage, &ctx_b);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        // ctx_b（自テナント Private+Public 可視）は自身の private を含み他テナントの
        // private は見えない契約（RLS-1〜4 相当）。A の private 行が索引の辞書へ
        // 混入しないことを確認する。
        assert_eq!(
            index.candidates_for(&MetadataFilter_equals(&schema, "kind", "A-PRIVATE-1")),
            Some(Vec::new()),
            "other tenant's private row must not leak into this ctx's dictionary"
        );
        assert!(index
            .candidates_for(&MetadataFilter_equals(&schema, "kind", "B-PRIVATE-1"))
            .map(|v| !v.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn empty_table_builds_empty_index() {
        let path = unique_db_path("scalar-index-empty");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert_eq!(index.row_count(), 0);
        assert_eq!(
            index.candidates_for(&MetadataFilter_equals(&schema, "kind", "anything")),
            Some(Vec::new())
        );
    }

    #[test]
    fn id_index_is_none_when_any_id_exceeds_exact_f64_range() {
        let path = unique_db_path("scalar-index-id-overflow");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);
        insert(
            &storage,
            &ctx_a,
            (1u64 << 53) + 1,
            Some("y"),
            None,
            Visibility::Public,
        );
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert!(index
            .candidates_id_range(Bound::Unbounded, Bound::Unbounded)
            .is_none());
    }

    #[test]
    fn id_index_supports_range_queries_when_all_ids_in_range() {
        let path = unique_db_path("scalar-index-id-range");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        for id in [1u64, 5, 10, 20] {
            insert(&storage, &ctx_a, id, Some("x"), None, Visibility::Public);
        }
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let ids_in_range = |lower, upper| {
            let slots = index.candidates_id_range(lower, upper).expect("id index");
            let mut ids: Vec<u64> = slots
                .iter()
                .map(|&s| snapshot.arena().ids()[s as usize])
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(
            ids_in_range(Bound::Included(5), Bound::Included(10)),
            vec![5, 10]
        );
        assert_eq!(
            ids_in_range(Bound::Excluded(5), Bound::Unbounded),
            vec![10, 20]
        );
    }

    // ---------- 容量検証ヘルパーの単体テスト（codex-review P1 対応・PR #569:
    // 「確保前に容量を検証する」契約を、1 GiB 規模のデータを実際に確保せず
    // 直接固定する） ----------

    #[test]
    fn check_scalar_index_budget_rejects_before_exceeding_limit() {
        // 上限ちょうどまでは許可し、1 byte でも超えたら拒否する（境界値）。
        assert!(check_scalar_index_budget(0, MAX_SCALAR_INDEX_BYTES).is_ok());
        assert!(matches!(
            check_scalar_index_budget(0, MAX_SCALAR_INDEX_BYTES + 1),
            Err(ScalarIndexBuildError::TooLarge)
        ));
        assert!(matches!(
            check_scalar_index_budget(MAX_SCALAR_INDEX_BYTES, 1),
            Err(ScalarIndexBuildError::TooLarge)
        ));
        // `saturating_add` によるオーバーフロー耐性（巨大な累計値でもパニックしない）。
        assert!(matches!(
            check_scalar_index_budget(usize::MAX, 1),
            Err(ScalarIndexBuildError::TooLarge)
        ));
    }

    #[test]
    fn text_column_reservation_bytes_scales_with_pair_count_regardless_of_dedup() {
        // 重複排除前の pair_count のみに依存する（実際に何種類の値が
        // 含まれるかは無関係）。高重複列でも確保容量分の予算計上を
        // バイパスできないことを固定する（codex-review P1 対応・PR #569
        // 未解決分）。
        // pair_count=0 でも offsets は 1 件（先頭の 0）分だけ確保される。
        assert_eq!(text_column_reservation_bytes(0), std::mem::size_of::<u32>());
        let per_entry = std::mem::size_of::<String>()
            + std::mem::size_of::<u32>() // offsets（pair_count + 1 分だが定数項は無視）
            + std::mem::size_of::<u32>()
            + std::mem::size_of::<(String, u32)>();
        let pair_count = 1_000;
        let bytes = text_column_reservation_bytes(pair_count);
        // offsets の +1 分だけ厳密な等式ではなく下限として確認する。
        assert!(bytes >= pair_count.saturating_mul(per_entry));
    }

    #[test]
    fn text_column_reservation_bytes_budget_rejects_high_duplication_column() {
        // 1 GiB 上限に対し、重複排除後は 1 件しか実データが残らない列でも、
        // 確保容量（pair_count 件分）だけで上限を超えるケースを検証する。
        // 一致し得ない大きさの pair_count（例: 全行が同一値の高重複 TEXT 列）を
        // 想定し、確保**前**の予算検証でバイパスされないことを固定する。
        let huge_pair_count = MAX_SCALAR_INDEX_BYTES; // 明らかに 1 GiB を超える確保容量になる件数
        let reservation_bytes = text_column_reservation_bytes(huge_pair_count);
        assert!(reservation_bytes > MAX_SCALAR_INDEX_BYTES);
        assert!(matches!(
            check_scalar_index_budget(0, reservation_bytes),
            Err(ScalarIndexBuildError::TooLarge)
        ));
    }

    #[test]
    fn hashmap_bucket_count_matches_hashbrown_rounding_for_known_capacities() {
        // hashbrown の負荷率 7/8・2 のべき乗丸めの既知の境界値を固定する
        // （codex-review P1 対応・PR #569）。100 万件は指摘で挙げられた
        // 具体例（1,000,000 * 8 / 7 ≈ 1,142,857 → 次のべき乗 2^21）。
        assert_eq!(hashmap_bucket_count(0), 0);
        assert_eq!(hashmap_bucket_count(1), 4);
        assert_eq!(hashmap_bucket_count(4), 8);
        assert_eq!(hashmap_bucket_count(7), 8);
        assert_eq!(hashmap_bucket_count(8), 16);
        assert_eq!(hashmap_bucket_count(1_000_000), 1 << 21);
    }

    #[test]
    fn hashmap_reservation_bytes_exceeds_naive_pair_count_times_entry_size() {
        // 修正前の単純計算（pair_count × エントリサイズ）を常に上回ることを
        // 固定する（codex-review P1 対応・PR #569。バケット数丸め・制御バイト
        // 分が計上されない旧実装への回帰防止）。
        let pair_count = 1_000_000usize;
        let naive = pair_count.saturating_mul(std::mem::size_of::<(String, u32)>());
        let actual = hashmap_reservation_bytes(pair_count);
        assert!(
            actual > naive,
            "actual={actual} naive={naive} (バケット数丸め・制御バイト分が計上されているはず)"
        );
    }

    #[test]
    fn text_column_reservation_bytes_rejects_one_million_rows_all_empty_string_scenario() {
        // codex-review P1 指摘（PR #569・threadId: PRRT_kwDOUAKASM6fu0DC）の
        // 再現ケース: 64bit 環境で 100 万行・全 TEXT 列が空文字列の場合、
        // 文字列本体バイトはほぼ 0 で「予算上ほぼ無料」に見えるが、
        // `equality: HashMap<String, u32>` の実バケット確保（2^21 件、
        // 制御バイト込み）だけで 1 列あたり優に前提の百バイト単位を超える。
        // 旧実装（pair_count × size_of::<(String, u32)>() の単純計算）では
        // この超過分が計上されず 1 GiB 予算をバイパスし得たため、
        // 新しい見積りが hashbrown の丸めを踏まえて十分大きいことを固定する。
        let pair_count = 1_000_000usize;
        let reservation_bytes = text_column_reservation_bytes(pair_count);
        // 2^21 バケット × (エントリサイズ + 制御バイト 1) 以上であること
        // （[`hashmap_bucket_count`] の既知境界値と対応）。
        let expected_floor = (1usize << 21) * (std::mem::size_of::<(String, u32)>() + 1);
        assert!(
            reservation_bytes >= expected_floor,
            "reservation_bytes={reservation_bytes} expected_floor={expected_floor}"
        );
    }

    #[test]
    fn build_handles_empty_string_text_values_via_preallocated_accumulator() {
        // codex-review P1 指摘（PR #569 未解決分）の是正後も、空文字列 `TEXT`
        // 値を含む通常規模の入力で正しく構築できることを固定する
        // （`acc` の事前一括確保が実データの push を壊していないことの確認）。
        let path = unique_db_path("scalar-index-empty-values");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some(""), Some(""), Visibility::Public);
        insert(&storage, &ctx_a, 2, Some(""), None, Visibility::Public);
        insert(&storage, &ctx_a, 3, Some("x"), None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert_eq!(index.row_count(), 3);
        let empty_matches = index.candidates_for(&MetadataFilter_equals(&schema, "kind", ""));
        let mut empty_slots = empty_matches.expect("index available");
        empty_slots.sort_unstable();
        assert_eq!(empty_slots, vec![0, 1]);
        assert_eq!(
            index.candidates_for(&MetadataFilter_equals(&schema, "kind", "x")),
            Some(vec![2])
        );
    }

    #[test]
    fn per_column_accumulator_reservation_bytes_scales_with_row_count() {
        // acc: Vec<(String, u32)> の確保上限は列あたり高々 row_count 件
        // （1 行につき列あたり高々 1 値）。タプル全体のサイズ（アラインメント
        // 詰め物込み）で計上されることを固定する（codex-review P1 対応・
        // PR #569 未解決分）。
        assert_eq!(per_column_accumulator_reservation_bytes(0), 0);
        let row_count = 1_000;
        assert_eq!(
            per_column_accumulator_reservation_bytes(row_count),
            row_count.saturating_mul(std::mem::size_of::<(String, u32)>())
        );
    }

    #[test]
    fn per_column_accumulator_reservation_bytes_budget_rejects_before_row_scan() {
        // 100 万行 × 全列空文字列のような「文字列本体はほぼ無料だが acc 自体の
        // 確保容量が大きい」入力で、確保**前**の予算検証がバイパスされない
        // ことを固定する（codex-review P1 指摘: scalar_index.rs:343）。
        let huge_row_count = MAX_SCALAR_INDEX_BYTES; // 明らかに上限を超える確保容量になる行数
        let reservation_bytes = per_column_accumulator_reservation_bytes(huge_row_count);
        assert!(reservation_bytes > MAX_SCALAR_INDEX_BYTES);
        assert!(matches!(
            check_scalar_index_budget(0, reservation_bytes),
            Err(ScalarIndexBuildError::TooLarge)
        ));
    }

    #[test]
    fn text_column_index_approx_heap_bytes_counts_reserved_capacity_not_len() {
        // codex-review P1 対応（PR #569）: `values`/`offsets`/`slots`/`equality`
        // は重複排除前の pair_count 件分を一括確保するため、重複が多い列では
        // `len()` が `capacity()` を大きく下回る。この回帰テストは `len()` を
        // 使う実装（旧実装）だと小さく計上されるが `capacity()` ベースの実装
        // では確保容量分がきちんと計上されることを固定する。
        let pair_count = 1_000usize;
        let mut values: Vec<String> = Vec::with_capacity(pair_count);
        values.push("dup".to_string());
        let mut offsets: Vec<u32> = Vec::with_capacity(pair_count.saturating_add(1));
        offsets.push(0);
        offsets.push(1);
        let mut slots: Vec<u32> = Vec::with_capacity(pair_count);
        slots.push(0);
        let mut equality: HashMap<String, u32> = HashMap::with_capacity(pair_count);
        equality.insert("dup".to_string(), 0);
        let index = TextColumnIndex {
            values,
            offsets,
            slots,
            equality,
        };

        // `len()` ベースで計上した場合の下限（旧実装相当）。
        let len_based_lower_bound = "dup".len()
            + std::mem::size_of::<String>()
            + 2 * std::mem::size_of::<u32>()
            + std::mem::size_of::<u32>()
            + ("dup".len() + std::mem::size_of::<(String, u32)>());

        let actual = index.approx_heap_bytes();
        assert!(
            actual > len_based_lower_bound,
            "capacity ベースの計上は len ベースの計上より大きいはず: actual={actual} len_based_lower_bound={len_based_lower_bound}"
        );
        // 確保容量（pair_count 件分）に見合う規模まで計上されていることを
        // 大まかに確認する（下限は values/offsets/slots の capacity 分のみ。
        // equality の未使用バケット分は HashMap の実装依存のため含めない）。
        let capacity_floor = pair_count.saturating_mul(std::mem::size_of::<u32>()) * 2;
        assert!(
            actual >= capacity_floor,
            "actual={actual} capacity_floor={capacity_floor}"
        );
    }

    #[test]
    fn try_owned_string_copies_content_without_infallible_allocation_path() {
        let owned = try_owned_string("scalar-index").expect("small string must succeed");
        assert_eq!(owned, "scalar-index");
        assert_eq!(owned.capacity(), "scalar-index".len());
    }

    // ---------- キャッシュ契約テスト（`arena_cache.rs`・`core.rs::PrefilterCache`
    // のテストを雛形に。世代整合・非対称 insert 契約を固定する） ----------

    #[test]
    fn cache_hits_same_generation_and_evicts_on_write() {
        let path = unique_db_path("scalar-index-cache-basic");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        // insert 前の初回 lookup は未登録によるミス（codex-review P2 対応・
        // PR #569: `misses` が実際のミス経路で加算されることの回帰）。
        let read_txn0 = storage.db().begin_read().expect("begin read");
        let before_insert = cache.lookup(&storage, &read_txn0, "docs", &ctx_a);
        assert!(before_insert.is_none());
        assert_eq!(cache.stats().misses, 1);

        let inserted = cache
            .insert(&storage, "docs", &ctx_a, index)
            .expect("insert must succeed on fresh generation");
        assert_eq!(cache.stats().entries, 1);

        let read_txn = storage.db().begin_read().expect("begin read");
        let hit = cache
            .lookup(&storage, &read_txn, "docs", &ctx_a)
            .expect("lookup must hit same generation");
        assert!(Arc::ptr_eq(&hit, &inserted));
        assert_eq!(cache.stats().hits, 1);
        // ヒットでは加算されないため、直前のミス 1 件のまま変化しない。
        assert_eq!(cache.stats().misses, 1);

        // 書き込みで世代を進めるとミスになり、stale eviction が発生する。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);
        let read_txn2 = storage.db().begin_read().expect("begin read");
        let miss = cache.lookup(&storage, &read_txn2, "docs", &ctx_a);
        assert!(miss.is_none());
        assert_eq!(cache.stats().stale_evictions, 1);
        assert_eq!(cache.stats().misses, 2);
    }

    #[test]
    fn insert_returns_none_on_generation_conflict() {
        let path = unique_db_path("scalar-index-cache-conflict");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        // 構築後、挿入前に別の書き込みで世代を進める（並行書き込みを模す）。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);

        let result = cache.insert(&storage, "docs", &ctx_a, index);
        assert!(
            result.is_none(),
            "stale insert must be rejected (None), not returned to caller"
        );
        assert_eq!(cache.stats().entries, 0);
    }

    // Issue #475 codex-review P2 対応（`sql::aggregate::capture_scalar_index_
    // snapshot` の piggyback 採取失敗の記録・照会）: `mark_capture_unbuildable`
    // で記録した世代は `is_capture_known_unbuildable` がヒットし、書き込みで
    // 世代が進むと自動的にミスへ戻ることを固定する。
    #[test]
    fn capture_unbuildable_memo_hits_same_generation_and_misses_after_write() {
        let path = unique_db_path("scalar-index-cache-unbuildable");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();

        let read_txn = storage.db().begin_read().expect("begin read");
        assert!(
            !cache.is_capture_known_unbuildable(&read_txn, "docs", &ctx_a),
            "no record yet must not be treated as known-unbuildable"
        );
        cache.mark_capture_unbuildable(&read_txn, "docs", &ctx_a);
        drop(read_txn);

        let read_txn = storage.db().begin_read().expect("begin read");
        assert!(
            cache.is_capture_known_unbuildable(&read_txn, "docs", &ctx_a),
            "same generation must hit the recorded memo"
        );
        // 別テナント ctx・別テーブルは記録を共有しない。
        let ctx_b = ctx("tenant-b");
        assert!(!cache.is_capture_known_unbuildable(&read_txn, "docs", &ctx_b));
        assert!(!cache.is_capture_known_unbuildable(&read_txn, "other_table", &ctx_a));
        drop(read_txn);

        // 書き込みで世代が進むと記録は現世代と一致しなくなり、ミスへ戻る
        // （採取の再試行が世代進行後に自動的に許される）。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);
        let read_txn2 = storage.db().begin_read().expect("begin read");
        assert!(!cache.is_capture_known_unbuildable(&read_txn2, "docs", &ctx_a));
    }

    // PR #603 codex-review P2 指摘対応: `mark_capture_unbuildable` は採取に
    // 実際に使った `read_txn`（世代 G）のスナップショット世代で記録しなけれ
    // ばならない。採取中（`read_txn` を開いた後、`mark_capture_unbuildable`
    // を呼ぶまでの間）に別トランザクションがコミットして世代が G+1 へ進んで
    // も、記録は G のまま——`Storage::table_generation`（最新コミット世代）を
    // 使っていた旧実装ではここで G+1 が誤記録され、実際には構築可能な G+1 の
    // 採取が次の更新まで誤ってスキップされていた。
    #[test]
    fn capture_unbuildable_memo_records_read_txn_generation_not_latest_commit() {
        let path = unique_db_path("scalar-index-cache-unbuildable-stale-txn");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();

        // 採取に使った read_txn（世代 G）を開いたまま保持する。
        let read_txn_g = storage.db().begin_read().expect("begin read");

        // 採取中（read_txn_g を開いた後）に別トランザクションがコミットし、
        // テーブル世代が G+1 へ進む（並行書き込みを模す）。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);

        // 採取失敗を、採取に実際に使った read_txn_g（世代 G）で記録する。
        cache.mark_capture_unbuildable(&read_txn_g, "docs", &ctx_a);
        drop(read_txn_g);

        // 世代 G+1（コミット済み最新）のスナップショットでは記録が一致せず、
        // 誤ってミスとして扱われない（＝再試行が許される）ことを確認する。
        let read_txn_g_plus_1 = storage.db().begin_read().expect("begin read");
        assert!(
            !cache.is_capture_known_unbuildable(&read_txn_g_plus_1, "docs", &ctx_a),
            "the memo must be keyed by the read_txn's own generation (G), \
             not the latest committed generation (G+1) observed after it started"
        );
        drop(read_txn_g_plus_1);
    }

    #[test]
    fn cache_key_separates_by_ctx() {
        let path = unique_db_path("scalar-index-cache-ctx");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();
        let (snapshot_a, schema) = snapshot_from(&storage, &ctx_a);
        let index_a = ScalarIndex::build(&schema, &snapshot_a).expect("build index a");
        cache
            .insert(&storage, "docs", &ctx_a, index_a)
            .expect("insert a");

        let read_txn = storage.db().begin_read().expect("begin read");
        assert!(cache.lookup(&storage, &read_txn, "docs", &ctx_b).is_none());
        assert_eq!(cache.stats().entries, 1);
    }

    /// codex-review P2 対応（PR #569）の固定テスト: `approx_heap_bytes()` は
    /// [`ScalarIndex::build`] 完了時に確定した `approx_bytes` フィールドを
    /// 返すだけの O(1) アクセサであり、実走査（`compute_approx_heap_bytes`）
    /// と常に一致する。`insert` が書き込みロック保持中に既存索引すべてを
    /// 再走査しなくなったことを、両者の値が同一であり続けることで間接的に
    /// 固定する（`insert` が誤った値を積み上げていればここで乖離する）。
    #[test]
    fn approx_heap_bytes_matches_build_time_computed_value_and_is_not_recomputed() {
        let path = unique_db_path("scalar-index-approx-bytes-cached");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let cached = index.approx_heap_bytes();
        let recomputed = index.compute_approx_heap_bytes();
        assert_eq!(
            cached, recomputed,
            "approx_heap_bytes() must equal a fresh full traversal"
        );

        let cache = ScalarIndexCache::new();
        let inserted = cache
            .insert(&storage, "docs", &ctx_a, index)
            .expect("insert must succeed on fresh generation");
        // insert 後もキャッシュ登録済みインスタンスの値は build 時点のまま
        // （insert 経路が値を書き換えたり再走査結果へ差し替えたりしない）。
        assert_eq!(inserted.approx_heap_bytes(), cached);
    }

    // ---------- 補助（テスト専用のフィルタ構築ヘルパ） ----------

    #[allow(non_snake_case)]
    fn MetadataFilter_equals(schema: &TableSchema, column: &str, value: &str) -> MetadataFilter {
        crate::declarative_filter::DeclarativeFilter::equals(column, value)
            .bind(schema)
            .expect("bind equals filter")
    }

    #[allow(non_snake_case)]
    fn MetadataFilter_starts_with(
        schema: &TableSchema,
        column: &str,
        prefix: &str,
    ) -> MetadataFilter {
        crate::declarative_filter::DeclarativeFilter::starts_with(column, prefix)
            .bind(schema)
            .expect("bind starts_with filter")
    }

    fn declarative_filter_all(schema: &TableSchema) -> Vec<MetadataFilter> {
        vec![
            MetadataFilter_equals(schema, "kind", "alpha"),
            MetadataFilter_equals(schema, "kind", "beta"),
            MetadataFilter_starts_with(schema, "path", "src/"),
        ]
    }

    /// 全スロットを `scan_scalar_columns` ＋ `MetadataFilter::matches` で走査する
    /// オラクル（索引と完全一致することを検証する対照実装）。
    fn oracle_matches(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
        filter: &MetadataFilter,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        for slot in 0..snapshot.arena().len() {
            let metadata = snapshot
                .metadata()
                .get(slot)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let scanned = scan_scalar_columns(schema, metadata).expect("decode row");
            let value = scanned.get(filter.column_index()).copied().flatten();
            if filter.matches(value) {
                out.push(slot as u32);
            }
        }
        out
    }

    // Issue #475: `column_groups`／`slots_without_value`（`sql::group_by` の
    // WHERE なし GROUP BY 列挙形が使う API）の単体テスト。

    #[test]
    fn column_groups_enumerates_values_in_byte_order_with_correct_slots() {
        let path = unique_db_path("scalar-index-column-groups");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        // わざと非バイト順で挿入し、列挙がバイト列昇順であることを固定する。
        insert(&storage, &c, 1, Some("zulu"), None, Visibility::Public);
        insert(&storage, &c, 2, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 3, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 4, None, None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        let groups: Vec<(String, Vec<u32>)> = index
            .column_groups(kind_col)
            .expect("text column")
            .map(|(v, slots)| (v.to_string(), slots.to_vec()))
            .collect();
        let values: Vec<&str> = groups.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(values, vec!["alpha", "zulu"]);
        for (_, slots) in &groups {
            let mut sorted = slots.clone();
            sorted.sort_unstable();
            assert_eq!(slots, &sorted, "slots must be ascending");
        }
        let alpha_slots = &groups[0].1;
        assert_eq!(alpha_slots.len(), 2);

        // id=4 (kind=NULL) はどの値グループにも現れない。
        for (_, slots) in &groups {
            for &slot in slots {
                let arena_id = snapshot.arena().ids()[slot as usize];
                assert_ne!(arena_id, 4);
            }
        }
    }

    #[test]
    fn slots_without_value_returns_null_rows_only() {
        let path = unique_db_path("scalar-index-slots-without-value");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 2, None, None, Visibility::Public);
        insert(&storage, &c, 3, None, None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        let null_slots = index
            .slots_without_value(kind_col)
            .expect("text column supports null slots");
        let null_ids: Vec<u64> = null_slots
            .iter()
            .map(|&slot| snapshot.arena().ids()[slot as usize])
            .collect();
        let mut sorted_ids = null_ids.clone();
        sorted_ids.sort_unstable();
        assert_eq!(sorted_ids, vec![2, 3]);
        // 昇順契約。
        let mut sorted_slots = null_slots.clone();
        sorted_slots.sort_unstable();
        assert_eq!(null_slots, sorted_slots);
    }

    #[test]
    fn slots_without_value_empty_when_no_nulls() {
        let path = unique_db_path("scalar-index-slots-without-value-empty");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 2, Some("beta"), None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        assert_eq!(
            index.slots_without_value(kind_col).expect("text column"),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn column_groups_and_slots_without_value_none_for_vector_column() {
        let path = unique_db_path("scalar-index-column-groups-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let embedding_col = schema
            .columns
            .iter()
            .position(|col| col.name == "embedding")
            .expect("embedding column");
        assert!(index.column_groups(embedding_col).is_none());
        assert!(index.slots_without_value(embedding_col).is_none());
    }

    // ---------- 列単位の索引対象除外（平均値長ゲート。Issue #632） ----------

    #[test]
    fn build_keeps_indexing_text_column_with_short_values() {
        // 短い分類値のみの列（`kind` 相当）は従来どおり索引化される回帰確認。
        let db_path = unique_db_path("scalar-index-avg-len-short-kept");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 2, Some("beta"), None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        assert!(
            index.column_is_indexed(kind_col),
            "短い値のみの列は平均値長ゲートで除外されない"
        );
        assert_eq!(
            index
                .candidates_for(&MetadataFilter_equals(&schema, "kind", "alpha"))
                .expect("indexed column"),
            vec![0]
        );
    }

    #[test]
    fn build_excludes_text_column_exceeding_avg_length_threshold() {
        // 平均値長が閾値を大きく超える列（`body` 相当。ここでは既存スキーマの
        // `path` 列を長文専用に使う）は索引対象から除外され、
        // `candidates_for`／`column_groups` がいずれも `None` を返す。
        let db_path = unique_db_path("scalar-index-avg-len-excluded");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        let long_value = "x".repeat(MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN * 4);
        insert(
            &storage,
            &c,
            1,
            Some("alpha"),
            Some(long_value.as_str()),
            Visibility::Public,
        );
        insert(
            &storage,
            &c,
            2,
            Some("beta"),
            Some(long_value.as_str()),
            Visibility::Public,
        );
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        let path_col = schema
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(
            index.column_is_indexed(kind_col),
            "長文列の除外が他の短い列の索引化に影響してはならない"
        );
        assert!(
            !index.column_is_indexed(path_col),
            "平均値長が閾値超過の列は索引対象から除外される"
        );
        assert!(index
            .candidates_for(&MetadataFilter_equals(&schema, "path", &long_value))
            .is_none());
        assert!(index.column_groups(path_col).is_none());
    }

    #[test]
    fn build_avg_length_gate_uses_nonnull_value_count_not_row_count() {
        // 除外判定は「行数」ではなく「非 NULL 値の件数」基準であることの確認。
        // NULL の多い疎な列に短い値のみを混在させても、平均値長は非 NULL 値の
        // 件数を分母に計算されるため除外されない。
        let db_path = unique_db_path("scalar-index-avg-len-sparse-nulls");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        // 大量の NULL 行（`path` 列が NULL）を挟んでも、実際に値を持つ行が
        // いずれも短ければ列は除外されない。
        for id in 1..=50u64 {
            insert(&storage, &c, id, Some("alpha"), None, Visibility::Public);
        }
        insert(
            &storage,
            &c,
            51,
            Some("alpha"),
            Some("short"),
            Visibility::Public,
        );
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let path_col = schema
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(
            index.column_is_indexed(path_col),
            "NULL の多い疎な列でも非 NULL 値が短ければ除外されない（行数基準ではない）"
        );
    }

    #[test]
    fn build_excluded_column_does_not_contribute_to_approx_heap_bytes() {
        // 除外後も approx_heap_bytes() が除外列のコストを含まない小さい値に
        // なることの確認（除外によりメモリを消費しないことの直接証跡）。
        let excluded_db = unique_db_path("scalar-index-avg-len-bytes-excluded");
        let _excluded_guard = CleanupGuard(excluded_db.clone());
        let storage_excluded = Storage::open(&excluded_db).expect("open storage");
        create_table(&storage_excluded);
        let c = ctx("tenant-a");
        let long_value = "x".repeat(MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN * 8);
        for id in 1..=5u64 {
            insert(
                &storage_excluded,
                &c,
                id,
                Some("alpha"),
                Some(long_value.as_str()),
                Visibility::Public,
            );
        }
        let (snapshot_excluded, schema_excluded) = snapshot_from(&storage_excluded, &c);
        let index_excluded =
            ScalarIndex::build(&schema_excluded, &snapshot_excluded).expect("build index");
        let path_col = schema_excluded
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(!index_excluded.column_is_indexed(path_col));

        // 対照: 同じ行数・同じ `kind` 値だが `path` が常に `NULL`（除外対象
        // 列そのものを持たない）の索引と比べ、除外後の索引がおおむね同水準
        // まで小さいことを確認する（除外列の長文コストを含んでいれば、この
        // 対照よりずっと大きくなるはず）。
        let baseline_db = unique_db_path("scalar-index-avg-len-bytes-baseline");
        let _baseline_guard = CleanupGuard(baseline_db.clone());
        let storage_baseline = Storage::open(&baseline_db).expect("open storage");
        create_table(&storage_baseline);
        for id in 1..=5u64 {
            insert(
                &storage_baseline,
                &c,
                id,
                Some("alpha"),
                None,
                Visibility::Public,
            );
        }
        let (snapshot_baseline, schema_baseline) = snapshot_from(&storage_baseline, &c);
        let index_baseline =
            ScalarIndex::build(&schema_baseline, &snapshot_baseline).expect("build index");

        // 除外により長文コスト（`long_value.len() * 5` 相当）を含まない
        // ため、除外後の索引は長文 1 件分未満の差に収まる。
        assert!(
            index_excluded.approx_heap_bytes()
                < index_baseline.approx_heap_bytes() + long_value.len(),
            "除外列の長文コストが approx_heap_bytes() に計上されてはならない: excluded={} baseline={}",
            index_excluded.approx_heap_bytes(),
            index_baseline.approx_heap_bytes()
        );
    }

    // ---------- 閾値見直し（128 → 64。Issue #644）の crossdb 相当固定値 ----------

    #[test]
    fn build_excludes_text_column_with_crossdb_like_avg_length_126() {
        // Issue #633 の crossdb 前後比較実測で確定した `body` 列の全体平均
        // （126.3 バイト）相当を固定値（126 バイト・全行同長）で再現する。
        // 旧閾値 128 では発火しなかったが、64 への引き下げにより最初の値の
        // 時点で累積平均が閾値を超え、以降ずっと除外され続けることを確認する
        // （行単位の累積平均判定のため、この fixture 相当では #633 が指摘した
        // 「累積平均 vs 全体平均」の論点自体が問題にならない）。
        let db_path = unique_db_path("scalar-index-avg-len-crossdb-like-126");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        for id in 1..=5u64 {
            let prefix = format!("doc-{id}-");
            let padded_len = 126usize
                .checked_sub(prefix.len())
                .expect("prefix shorter than 126 bytes");
            let value = format!("{prefix}{}", "x".repeat(padded_len));
            assert_eq!(value.len(), 126, "fixture 値は正確に 126 バイトで固定する");
            insert(
                &storage,
                &c,
                id,
                Some("alpha"),
                Some(value.as_str()),
                Visibility::Public,
            );
        }
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        let path_col = schema
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(
            !index.column_is_indexed(path_col),
            "crossdb fixture 相当（平均 126 バイト）は閾値 64 で除外される"
        );
        assert!(index.column_groups(path_col).is_none());
        assert!(index
            .candidates_for(&MetadataFilter_equals(&schema, "path", "doc-1-xxx"))
            .is_none());
        assert!(
            index.column_is_indexed(kind_col),
            "長文列の除外が他の短い列の索引化に影響してはならない"
        );
    }

    #[test]
    fn build_keeps_indexing_text_column_with_avg_length_32() {
        // `lang`/`topic` 相当の短い分類値列（32 バイト・全行同長）は閾値 64
        // を下回るため、64 への引き下げ後も索引対象に残ることを固定する。
        let db_path = unique_db_path("scalar-index-avg-len-short-32");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        let mut last_value = String::new();
        for id in 1..=5u64 {
            let prefix = format!("p-{id:02}-");
            let padded_len = 32usize
                .checked_sub(prefix.len())
                .expect("prefix shorter than 32 bytes");
            let value = format!("{prefix}{}", "y".repeat(padded_len));
            assert_eq!(value.len(), 32, "fixture 値は正確に 32 バイトで固定する");
            insert(
                &storage,
                &c,
                id,
                Some("alpha"),
                Some(value.as_str()),
                Visibility::Public,
            );
            last_value = value;
        }
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let path_col = schema
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(
            index.column_is_indexed(path_col),
            "平均値長 32 バイトの列は閾値 64 で索引対象に残る"
        );
        assert_eq!(
            index
                .candidates_for(&MetadataFilter_equals(&schema, "path", &last_value))
                .expect("indexed column"),
            vec![4]
        );
    }

    #[test]
    fn build_avg_length_gate_keeps_column_at_exact_threshold() {
        // 平均値長がちょうど閾値バイトの列は、厳密な `>` 比較（境界値含まず）
        // により索引対象に残ることを固定する（将来の閾値再変更に耐える境界
        // テスト。定数相対で書くことで 64 以外の値へ変わっても意味を保つ）。
        let db_path = unique_db_path("scalar-index-avg-len-exact-threshold");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        let value = "z".repeat(MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN);
        insert(
            &storage,
            &c,
            1,
            Some("alpha"),
            Some(value.as_str()),
            Visibility::Public,
        );
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let path_col = schema
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(
            index.column_is_indexed(path_col),
            "平均値長がちょうど閾値の列は除外されない（厳密な `>` 比較）"
        );
    }

    #[test]
    fn build_avg_length_gate_excludes_column_one_byte_over_threshold() {
        // 平均値長が閾値を 1 バイトでも超えると除外されることを固定する
        // （境界テスト。定数相対で書く）。
        let db_path = unique_db_path("scalar-index-avg-len-over-threshold-by-one");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        let value = "z".repeat(MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN + 1);
        insert(
            &storage,
            &c,
            1,
            Some("alpha"),
            Some(value.as_str()),
            Visibility::Public,
        );
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let path_col = schema
            .columns
            .iter()
            .position(|col| col.name == "path")
            .expect("path column");
        assert!(
            !index.column_is_indexed(path_col),
            "平均値長が閾値を 1 バイトでも超える列は除外される"
        );
    }

    // ------------------------------------------------------------------
    // Issue #893: 数値・日時・`NUMERIC`・`UUID` 列の順序索引
    // ------------------------------------------------------------------

    /// 新スカラー型の列を一通り含むテスト用テーブル。`flag`（`BOOLEAN`）は
    /// 索引対象外の型が引き続き `None` のままであることの対照として使う。
    fn typed_schema() -> TableSchema {
        TableSchema::new(
            "typed_docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("big", ColumnType::BigInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("dt", ColumnType::Date, true),
                ColumnDef::new("ts", ColumnType::Timestamp, true),
                ColumnDef::new(
                    "num",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("uid", ColumnType::Uuid, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        )
    }

    fn create_typed_table(storage: &Storage) {
        storage
            .create_table(&typed_schema())
            .expect("create typed table");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_typed(
        storage: &Storage,
        c: &PolicyContext,
        id: u64,
        n: Option<i32>,
        big: Option<i64>,
        r: Option<f32>,
        d: Option<f64>,
        dt: Option<i32>,
        ts: Option<i64>,
        num: Option<i128>,
        uid: Option<[u8; 16]>,
        flag: Option<bool>,
        visibility: Visibility,
    ) {
        crate::tenant::insert_typed_row(
            storage,
            "typed_docs",
            c,
            id,
            visibility,
            &[
                Value::Vector(vec![0.0, 0.0]),
                n.map(Value::Integer).unwrap_or(Value::Null),
                big.map(Value::BigInt).unwrap_or(Value::Null),
                r.map(Value::Real).unwrap_or(Value::Null),
                d.map(Value::Double).unwrap_or(Value::Null),
                dt.map(Value::Date).unwrap_or(Value::Null),
                ts.map(Value::Timestamp).unwrap_or(Value::Null),
                num.map(|unscaled| {
                    Value::Numeric(
                        crate::numeric::Decimal::from_parts(unscaled, 2)
                            .expect("valid decimal for scale 2"),
                    )
                })
                .unwrap_or(Value::Null),
                uid.map(|bytes| Value::Uuid(crate::uuid::Uuid::from_bytes(bytes)))
                    .unwrap_or(Value::Null),
                flag.map(Value::Bool).unwrap_or(Value::Null),
            ],
            &op_id(&format!("typed-seed-{id}")),
        )
        .expect("insert typed row");
    }

    /// [`snapshot_from`] の typed テーブル版（テーブル名のみ異なる）。
    fn typed_snapshot_from(
        storage: &Storage,
        c: &PolicyContext,
    ) -> (crate::sql::arena_cache::SqlArenaSnapshot, TableSchema) {
        let read_txn = storage.db().begin_read().expect("begin read");
        let schema =
            crate::catalog::get_table_schema_in_txn(&read_txn, "typed_docs").expect("schema");
        let expected_dim = schema.vector_dim().expect("vector dim");
        let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
            expected_dim,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        );
        let hook = crate::rls::ImplicitRlsHook::new(c);
        let mut rls_capture = |id: u64,
                               tenant_id: &str,
                               visibility,
                               embedding: &[f32],
                               metadata: &[u8],
                               response_arena_bytes_in_use: usize|
         -> std::result::Result<(), crate::arena::ArenaError> {
            capture.push(
                id,
                tenant_id,
                visibility,
                embedding,
                metadata,
                response_arena_bytes_in_use,
            );
            Ok(())
        };
        let _built: VectorArena =
            crate::arena::VectorArena::build_filtered_with_rows_in_txn_capturing(
                &read_txn,
                "typed_docs",
                hook.predicate(),
                |_, _, _, _| Ok(true),
                &mut rls_capture,
            )
            .expect("build arena");
        let table_generation = crate::catalog::table_generation_in_txn(&read_txn, "typed_docs")
            .expect("table generation");
        let (cache_arena, cache_metadata) = capture.finish("typed_docs").expect("capture snapshot");
        (
            crate::sql::arena_cache::SqlArenaSnapshot::new(
                cache_arena,
                cache_metadata,
                c.clone(),
                table_generation,
            ),
            schema,
        )
    }

    fn typed_col(schema: &TableSchema, name: &str) -> usize {
        schema
            .columns
            .iter()
            .position(|col| col.name == name)
            .unwrap_or_else(|| panic!("column {name} not found"))
    }

    /// 各型で「等価・範囲述語」の [`ScalarIndex::candidates_typed_range`] が
    /// `scan_scalar_columns` を総当たりした brute-force オラクルと完全一致
    /// することを固定する（境界値込み）。
    #[test]
    fn candidates_typed_range_matches_brute_force_oracle_for_each_type() {
        let db_path = unique_db_path("scalar-index-typed-oracle");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");

        // `n`（INTEGER）: i32::MIN/MAX を含む値域。
        // `big`（BIGINT）: 2^53 未満の値のみ（超過は別テストで確認）。
        // `r`（REAL）・`d`（DOUBLE）: 負・正・ゼロ・小数を混在。
        // `dt`（DATE）・`ts`（TIMESTAMP）: 任意の i32/i64（TIMESTAMP は 2^53 未満）。
        // `num`（NUMERIC(10,2)）: 負・正の unscaled 値。
        // `uid`（UUID）: nil・全 1・中間値。
        // clippy::type_complexity 対応でタプル型に別名を付ける（この単体
        // テストのみで使う一時的な型）。
        type TypedRow = (u64, i32, i64, f32, f64, i32, i64, i128, [u8; 16]);
        let rows: Vec<TypedRow> = vec![
            (
                1,
                i32::MIN,
                -1_000_000,
                -1.5,
                -1.5,
                -100,
                -1_000_000,
                -12345,
                [0u8; 16],
            ),
            (2, 0, 0, 0.0, 0.0, 0, 0, 0, [0xffu8; 16]),
            (3, i32::MAX, 1_000_000, 1.5, 1.5, 100, 1_000_000, 12345, {
                let mut b = [0u8; 16];
                b[15] = 1;
                b
            }),
            (4, 5, 5, 2.5, 2.5, 5, 5, 500, {
                let mut b = [0u8; 16];
                b[0] = 0x80;
                b
            }),
        ];
        for (id, n, big, r, d, dt, ts, num, uid) in &rows {
            insert_typed(
                &storage,
                &c,
                *id,
                Some(*n),
                Some(*big),
                Some(*r),
                Some(*d),
                Some(*dt),
                Some(*ts),
                Some(*num),
                Some(*uid),
                Some(true),
                Visibility::Public,
            );
        }

        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build_including_unwired_typed_range_columns(&schema, &snapshot)
            .expect("build index");

        // 列ごとに全行を brute force でスキャンし、`>=` 境界（各列の中央値
        // 相当）で候補を求めるオラクルと索引の結果を突き合わせる。
        let read_txn = storage.db().begin_read().expect("begin read");

        fn oracle_slots<T: PartialOrd + Copy>(
            arena: &VectorArena,
            metadata: &[Vec<u8>],
            schema: &TableSchema,
            col: usize,
            extract: impl Fn(ScalarRef<'_>) -> Option<T>,
            threshold: T,
        ) -> Vec<u32> {
            let mut out = Vec::new();
            for slot in 0..arena.len() {
                let meta = metadata.get(slot).map(Vec::as_slice).unwrap_or(&[]);
                let scanned = scan_scalar_columns(schema, meta).expect("decode row");
                if let Some(Some(v)) = scanned.get(col) {
                    if let Some(key) = extract(*v) {
                        if key >= threshold {
                            out.push(slot as u32);
                        }
                    }
                }
            }
            out.sort_unstable();
            out
        }

        // `snapshot_from`/`typed_snapshot_from` はテスト専用の再走査のため、
        // `arena`/`metadata` を直接読める `SqlArenaCaptureBuilder` の結果
        // ではなく、索引本体が構築時に読んだのと同じ `snapshot` を使う。
        let arena_len = snapshot.arena().len();
        let _ = read_txn; // 明示的に drop 順序を保つ（`storage` の借用と独立）。
        assert_eq!(arena_len, rows.len());

        let n_col = typed_col(&schema, "n");
        let big_col = typed_col(&schema, "big");
        let r_col = typed_col(&schema, "r");
        let d_col = typed_col(&schema, "d");
        let dt_col = typed_col(&schema, "dt");
        let ts_col = typed_col(&schema, "ts");
        let num_col = typed_col(&schema, "num");
        let uid_col = typed_col(&schema, "uid");

        // INTEGER: n >= 5
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            n_col,
            |v| match v {
                ScalarRef::Integer(x) => Some(x),
                _ => None,
            },
            5,
        );
        let actual = index
            .candidates_typed_range(n_col, Bound::Included(TypedKey::I64(5)), Bound::Unbounded)
            .expect("indexed integer column");
        assert_eq!(actual, expected, "INTEGER >= 5");

        // BIGINT: big >= 0
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            big_col,
            |v| match v {
                ScalarRef::BigInt(x) => Some(x),
                _ => None,
            },
            0i64,
        );
        let actual = index
            .candidates_typed_range(big_col, Bound::Included(TypedKey::I64(0)), Bound::Unbounded)
            .expect("indexed bigint column");
        assert_eq!(actual, expected, "BIGINT >= 0");

        // REAL: r >= 1.5（`f64_to_sortable_bits` の全順序性を確認）
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            r_col,
            |v| match v {
                ScalarRef::Real(x) => Some(f64_to_sortable_bits(f64::from(x))),
                _ => None,
            },
            f64_to_sortable_bits(1.5),
        );
        let actual = index
            .candidates_typed_range(
                r_col,
                Bound::Included(TypedKey::F64Sortable(f64_to_sortable_bits(1.5))),
                Bound::Unbounded,
            )
            .expect("indexed real column");
        assert_eq!(actual, expected, "REAL >= 1.5");

        // DOUBLE: d < 0.0
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            d_col,
            |v| match v {
                ScalarRef::Double(x) => Some(f64_to_sortable_bits(x)),
                _ => None,
            },
            u64::MIN,
        )
        .into_iter()
        .filter(|&slot| {
            let meta = snapshot
                .metadata()
                .get(slot as usize)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let scanned = scan_scalar_columns(&schema, meta).expect("decode row");
            matches!(scanned.get(d_col), Some(Some(ScalarRef::Double(x))) if *x < 0.0)
        })
        .collect::<Vec<_>>();
        let actual = index
            .candidates_typed_range(
                d_col,
                Bound::Unbounded,
                Bound::Excluded(TypedKey::F64Sortable(f64_to_sortable_bits(0.0))),
            )
            .expect("indexed double column");
        assert_eq!(actual, expected, "DOUBLE < 0.0");

        // DATE: dt <= 0
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            dt_col,
            |v| match v {
                ScalarRef::Date(x) => Some(-(x as i64)),
                _ => None,
            },
            0i64,
        );
        let actual = index
            .candidates_typed_range(dt_col, Bound::Unbounded, Bound::Included(TypedKey::I64(0)))
            .expect("indexed date column");
        assert_eq!(actual, expected, "DATE <= 0");

        // TIMESTAMP: ts = 0
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            ts_col,
            |v| match v {
                ScalarRef::Timestamp(0) => Some(0i64),
                _ => None,
            },
            0i64,
        );
        let actual = index
            .candidates_typed_range(
                ts_col,
                Bound::Included(TypedKey::I64(0)),
                Bound::Included(TypedKey::I64(0)),
            )
            .expect("indexed timestamp column");
        assert_eq!(actual, expected, "TIMESTAMP = 0");

        // NUMERIC(10,2): num > 0
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            num_col,
            |v| match v {
                ScalarRef::Numeric(dec) => Some(dec.unscaled()),
                _ => None,
            },
            1i128,
        );
        let actual = index
            .candidates_typed_range(
                num_col,
                Bound::Excluded(TypedKey::I128(0)),
                Bound::Unbounded,
            )
            .expect("indexed numeric column");
        assert_eq!(actual, expected, "NUMERIC > 0");

        // UUID: uid >= nil（全行を含む）
        let expected = oracle_slots(
            snapshot.arena(),
            snapshot.metadata(),
            &schema,
            uid_col,
            |v| match v {
                ScalarRef::Uuid(u) => Some(uuid_to_sortable_u128(u.as_bytes())),
                _ => None,
            },
            u128::MIN,
        );
        let actual = index
            .candidates_typed_range(
                uid_col,
                Bound::Included(TypedKey::U128(0)),
                Bound::Unbounded,
            )
            .expect("indexed uuid column");
        assert_eq!(actual, expected, "UUID >= nil");
    }

    /// `BETWEEN`（`FilterOp::Between`。SQL-24・TASK-208 ポインタ）の候補が
    /// 全走査オラクル（[`oracle_matches`]）と完全一致することを固定する
    /// （`Ge`∧`Le` の交差導出が正しいことの検証。レーン B の `DATE` 列で
    /// 確認する——production 結線済みのため通常の [`ScalarIndex::build`] を
    /// 使う）。
    #[test]
    fn between_typed_range_candidates_matches_full_scan_oracle() {
        let db_path = unique_db_path("scalar-index-between-oracle");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        // `dt` は 1970-01-01 起点の日数。id=N の `dt = N - 1`（1970-01-01 〜
        // 1970-01-05）とし、`BETWEEN '1970-01-02' AND '1970-01-04'` が
        // id=2..4（dt=1..3）だけに一致することを検証する。
        for (id, dt) in [(1u64, 0i32), (2, 1), (3, 2), (4, 3), (5, 4)] {
            insert_typed(
                &storage,
                &c,
                id,
                None,
                None,
                None,
                None,
                Some(dt),
                None,
                None,
                None,
                None,
                Visibility::Public,
            );
        }
        // NULL の `dt` 行（`BETWEEN` は NULL に対して UNKNOWN を返し、
        // 一致に含まれないことを確認する）。
        insert_typed(
            &storage,
            &c,
            6,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Public,
        );

        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let dt_col = typed_col(&schema, "dt");
        assert!(index.typed_column_is_indexed(dt_col));

        let between_filter =
            crate::declarative_filter::DeclarativeFilter::between("dt", "1970-01-02", "1970-01-04")
                .bind(&schema)
                .expect("bind BETWEEN filter");
        let expected = oracle_matches(&schema, &snapshot, &between_filter);
        let actual = index
            .candidates_for(&between_filter)
            .expect("indexed column");
        assert_eq!(actual, expected);
        assert_eq!(actual, vec![1, 2, 3]);
    }

    /// production 経路が実際に呼ぶ [`ScalarIndex::build`] は、レーン B
    /// （`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`。`FilterOp::TypedCompare` で
    /// production 結線済み——Issue #891・TASK-199）の順序索引を構築する
    /// 一方、レーン A（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE`。まだ `WHERE`
    /// 述語表現を持たず一度も照会されない）は `BOOLEAN` 等と同じ非索引化の
    /// ままであることを固定する。未接続のレーン A 列構築が既存の索引の
    /// 予算・可用性を巻き添えにしない（codex-review P2 指摘・PR #1032）
    /// という不変条件が、レーン B 接続後も維持されていることを示す。
    #[test]
    fn build_indexes_lane_b_but_defers_unwired_lane_a_typed_range_columns() {
        let db_path = unique_db_path("scalar-index-typed-lane-split");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        insert_typed(
            &storage,
            &c,
            1,
            Some(1),
            Some(1),
            Some(1.0),
            Some(1.0),
            Some(1),
            Some(1),
            Some(100),
            Some([0u8; 16]),
            Some(true),
            Visibility::Public,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        // production 経路と同じ既定入口。
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        for name in ["n", "big", "r", "d"] {
            let col = typed_col(&schema, name);
            assert!(
                !index.typed_column_is_indexed(col),
                "column {name} (lane A) must stay unindexed via the default build() \
                 entry until a WHERE predicate representation exists for it"
            );
        }
        for name in ["dt", "ts", "num", "uid"] {
            let col = typed_col(&schema, name);
            assert!(
                index.typed_column_is_indexed(col),
                "column {name} (lane B) must be indexed via the default build() \
                 entry now that TypedCompare is wired to production (Issue #891)"
            );
        }
    }

    /// `NULL` はいずれの typed 列索引にもエントリを作らない
    /// （`TextColumnIndex` と同じ契約。モジュールドキュメント参照）。
    #[test]
    fn typed_columns_exclude_null_rows() {
        let db_path = unique_db_path("scalar-index-typed-null");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        insert_typed(
            &storage,
            &c,
            1,
            Some(1),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Public,
        );
        insert_typed(
            &storage,
            &c,
            2,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Public,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build_including_unwired_typed_range_columns(&schema, &snapshot)
            .expect("build index");
        let n_col = typed_col(&schema, "n");
        let all = index
            .candidates_typed_range(n_col, Bound::Unbounded, Bound::Unbounded)
            .expect("indexed integer column");
        assert_eq!(all, vec![0u32], "NULL 行（id=2）は候補に現れない");
    }

    /// RLS 部分可視: 他テナントの `Private` 行は snapshot に含まれず、当然
    /// typed 索引の候補にも現れない（`(table, ctx)` 可視スナップショットから
    /// のみ構築するという索引全体の契約——モジュールドキュメント「fail-closed
    /// の適用範囲」——が typed 列でも成立することを固定する）。
    #[test]
    fn typed_columns_respect_rls_visibility() {
        let db_path = unique_db_path("scalar-index-typed-rls");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let tenant_a = ctx("tenant-a");
        let tenant_b = ctx("tenant-b");
        insert_typed(
            &storage,
            &tenant_a,
            1,
            Some(10),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Public,
        );
        insert_typed(
            &storage,
            &tenant_b,
            2,
            Some(20),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Private,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &tenant_a);
        let index = ScalarIndex::build_including_unwired_typed_range_columns(&schema, &snapshot)
            .expect("build index");
        let n_col = typed_col(&schema, "n");
        let all = index
            .candidates_typed_range(n_col, Bound::Unbounded, Bound::Unbounded)
            .expect("indexed integer column");
        assert_eq!(
            all,
            vec![0u32],
            "tenant-b の Private 行は tenant-a の索引に現れない"
        );
    }

    /// `BOOLEAN`／`BYTEA`／`JSON`／`JSONB`／`ARRAY`／`VECTOR` 列は typed 索引の
    /// 対象外のまま（`candidates_typed_range` は常に `None`）。
    #[test]
    fn typed_columns_exclude_non_indexable_types() {
        let db_path = unique_db_path("scalar-index-typed-excluded-types");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        insert_typed(
            &storage,
            &c,
            1,
            Some(1),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            Visibility::Public,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build_including_unwired_typed_range_columns(&schema, &snapshot)
            .expect("build index");
        let flag_col = typed_col(&schema, "flag");
        let embedding_col = typed_col(&schema, "embedding");
        assert!(!index.typed_column_is_indexed(flag_col));
        assert!(!index.typed_column_is_indexed(embedding_col));
        assert_eq!(
            index.candidates_typed_range(flag_col, Bound::Unbounded, Bound::Unbounded),
            None
        );
        assert_eq!(
            index.candidates_typed_range(embedding_col, Bound::Unbounded, Bound::Unbounded),
            None
        );
    }

    /// `BIGINT`／`TIMESTAMP` の絶対値が `2^53` を 1 件でも超えると、列単位で
    /// 索引対象外（`None`）へ縮退する（モジュールドキュメント
    /// 「`MAX_EXACT_TYPED_I64_ABS`」参照）。他の typed 列（`n`）は影響を
    /// 受けない。
    #[test]
    fn typed_columns_drop_bigint_and_timestamp_beyond_2_53() {
        let db_path = unique_db_path("scalar-index-typed-2-53");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        let over = (1i64 << 53) + 1;
        insert_typed(
            &storage,
            &c,
            1,
            Some(1),
            Some(over),
            None,
            None,
            None,
            Some(over),
            None,
            None,
            None,
            Visibility::Public,
        );
        insert_typed(
            &storage,
            &c,
            2,
            Some(2),
            Some(-over),
            None,
            None,
            None,
            Some(-over),
            None,
            None,
            None,
            Visibility::Public,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build_including_unwired_typed_range_columns(&schema, &snapshot)
            .expect("build index");
        let n_col = typed_col(&schema, "n");
        let big_col = typed_col(&schema, "big");
        let ts_col = typed_col(&schema, "ts");
        assert!(
            index.typed_column_is_indexed(n_col),
            "2^53 超過と無関係な列は影響を受けない"
        );
        assert!(
            !index.typed_column_is_indexed(big_col),
            "2^53 を超える BIGINT を含む列は索引対象外へ縮退する"
        );
        assert!(
            !index.typed_column_is_indexed(ts_col),
            "2^53 を超える TIMESTAMP を含む列は索引対象外へ縮退する"
        );
    }

    /// `2^53` ちょうど・その 1 手前は境界として索引に残る（`MAX_EXACT_TYPED_I64_ABS`
    /// は「超える」場合のみ除外する契約——`i64_within_exact_f64_range` の
    /// `<=` 判定——を固定する）。
    #[test]
    fn typed_columns_keep_bigint_at_exact_2_53_boundary() {
        let db_path = unique_db_path("scalar-index-typed-2-53-boundary");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        let at_boundary = 1i64 << 53;
        insert_typed(
            &storage,
            &c,
            1,
            None,
            Some(at_boundary),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Public,
        );
        insert_typed(
            &storage,
            &c,
            2,
            None,
            Some(-at_boundary),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Visibility::Public,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build_including_unwired_typed_range_columns(&schema, &snapshot)
            .expect("build index");
        let big_col = typed_col(&schema, "big");
        assert!(
            index.typed_column_is_indexed(big_col),
            "ちょうど 2^53（絶対値）は境界として索引に残る"
        );
        let all = index
            .candidates_typed_range(big_col, Bound::Unbounded, Bound::Unbounded)
            .expect("indexed bigint column");
        assert_eq!(all, vec![0u32, 1u32]);
    }

    /// `resolve_candidates` が `metadata_filters` に混在する `TEXT` 等価
    /// フィルタと `FilterOp::TypedCompare`（`DATE` 列。Issue #891・TASK-199
    /// で production 結線済み。Issue #893 production 接続）を、`id_preds` と
    /// 同じ交差契約で絞ることを固定する。`Self::candidates_for` が
    /// `TypedCompare` を内部で `self.typed_columns` へ振り向けることの
    /// 統合的な検証（`build()` 経由——`build_including_unwired_typed_range_
    /// columns` は使わない——production 経路と同一の入口）。
    #[test]
    fn resolve_candidates_intersects_typed_compare_with_metadata_filter() {
        let db_path = unique_db_path("scalar-index-typed-resolve-candidates");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        // `TEXT` 列を持つ既存の `docs` テーブルに `dt`（DATE）を混ぜたいが
        // 本ファイルの `schema()` は `docs` 名を占有しているため、TEXT 列を
        // 1 本持つ専用スキーマを使う。
        let mixed_schema = TableSchema::new(
            "mixed_docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("dt", ColumnType::Date, true),
            ],
        );
        storage.create_table(&mixed_schema).expect("create table");
        let c = ctx("tenant-a");
        let rows: [(u64, &str, i32); 4] = [
            (1, "alpha", 1),
            (2, "alpha", 5),
            (3, "beta", 5),
            (4, "alpha", 5),
        ];
        for (id, kind, dt) in rows {
            crate::tenant::insert_typed_row(
                &storage,
                "mixed_docs",
                &c,
                id,
                Visibility::Public,
                &[
                    Value::Vector(vec![0.0, 0.0]),
                    Value::Text(kind.to_string()),
                    Value::Date(dt),
                ],
                &op_id(&format!("mixed-seed-{id}")),
            )
            .expect("insert row");
        }

        let read_txn = storage.db().begin_read().expect("begin read");
        let schema =
            crate::catalog::get_table_schema_in_txn(&read_txn, "mixed_docs").expect("schema");
        let expected_dim = schema.vector_dim().expect("vector dim");
        let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
            expected_dim,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        );
        let hook = crate::rls::ImplicitRlsHook::new(&c);
        let mut rls_capture = |id: u64,
                               tenant_id: &str,
                               visibility,
                               embedding: &[f32],
                               metadata: &[u8],
                               response_arena_bytes_in_use: usize|
         -> std::result::Result<(), crate::arena::ArenaError> {
            capture.push(
                id,
                tenant_id,
                visibility,
                embedding,
                metadata,
                response_arena_bytes_in_use,
            );
            Ok(())
        };
        let _built: VectorArena =
            crate::arena::VectorArena::build_filtered_with_rows_in_txn_capturing(
                &read_txn,
                "mixed_docs",
                hook.predicate(),
                |_, _, _, _| Ok(true),
                &mut rls_capture,
            )
            .expect("build arena");
        let table_generation = crate::catalog::table_generation_in_txn(&read_txn, "mixed_docs")
            .expect("table generation");
        let (cache_arena, cache_metadata) = capture.finish("mixed_docs").expect("capture snapshot");
        let snapshot = crate::sql::arena_cache::SqlArenaSnapshot::new(
            cache_arena,
            cache_metadata,
            c.clone(),
            table_generation,
        );
        // production 経路と同一の既定入口（`build_including_unwired_typed_
        // range_columns` ではない）。
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let kind_filter = MetadataFilter_equals(&schema, "kind", "alpha");
        let dt_filter = crate::declarative_filter::DeclarativeFilter::compare(
            "dt",
            crate::declarative_filter::CompareOp::Ge,
            "1970-01-06", // エポック（1970-01-01）+5 日 = dt >= 5
        )
        .bind(&schema)
        .expect("bind compare filter");
        let resolution = index.resolve_candidates(&[kind_filter, dt_filter], &[]);
        let CandidateResolution::Use(slots) = resolution else {
            panic!("expected CandidateResolution::Use, got a fallback");
        };
        // id=1: kind=alpha, dt=1（dt 条件で除外）。id=2: kind=alpha, dt=5（一致）。
        // id=3: kind=beta（kind 条件で除外）。id=4: kind=alpha, dt=5（一致）。
        assert_eq!(slots, vec![1u32, 3u32], "kind='alpha' AND dt >= 5 の交差");
    }

    /// `NUMERIC` 列の `TypedCompare` は、リテラルが列より細かい `scale` を
    /// 持つ（列の格子に乗らない）場合でも [`crate::numeric::cmp_exact`] と
    /// 一致する候補を返すことを、`resolve_candidates`（production 経路と
    /// 同一の `ScalarIndex::build` 入口）経由で固定する
    /// （`numeric::tests::rescale_bounds_for_column_matches_cmp_exact_oracle`
    /// の単体レベル検証を索引照会レベルで裏付ける）。
    #[test]
    fn resolve_candidates_numeric_typed_compare_respects_finer_literal_scale() {
        let db_path = unique_db_path("scalar-index-numeric-fine-literal");
        let _guard = CleanupGuard(db_path.clone());
        let storage = Storage::open(&db_path).expect("open storage");
        create_typed_table(&storage);
        let c = ctx("tenant-a");
        // `num` は `NUMERIC(10,2)`。1.50・1.51 を列の格子（0.01 単位）で表現する。
        insert_typed(
            &storage,
            &c,
            1,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(150),
            None,
            None,
            Visibility::Public,
        );
        insert_typed(
            &storage,
            &c,
            2,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(151),
            None,
            None,
            Visibility::Public,
        );
        let (snapshot, schema) = typed_snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        // リテラル `1.505`（scale=3）は列の格子（scale=2）に乗らない。
        // `num > 1.505` は 1.51（id=2）のみに一致し、1.50（id=1）は除外される。
        let gt_filter = crate::declarative_filter::DeclarativeFilter::compare_numeric_literal(
            "num",
            crate::declarative_filter::CompareOp::Gt,
            "1.505",
        )
        .bind(&schema)
        .expect("bind compare filter");
        let resolution = index.resolve_candidates(&[gt_filter], &[]);
        let CandidateResolution::Use(slots) = resolution else {
            panic!("expected CandidateResolution::Use, got a fallback");
        };
        assert_eq!(slots, vec![1u32], "num > 1.505 は 1.51（slot 1）のみに一致");

        // `num <= 1.505` は 1.50（id=1）のみに一致する。
        let le_filter = crate::declarative_filter::DeclarativeFilter::compare_numeric_literal(
            "num",
            crate::declarative_filter::CompareOp::Le,
            "1.505",
        )
        .bind(&schema)
        .expect("bind compare filter");
        let resolution = index.resolve_candidates(&[le_filter], &[]);
        let CandidateResolution::Use(slots) = resolution else {
            panic!("expected CandidateResolution::Use, got a fallback");
        };
        assert_eq!(
            slots,
            vec![0u32],
            "num <= 1.505 は 1.50（slot 0）のみに一致"
        );
    }

    /// [`scalar_plan_under_target`]（Issue #1153）の単体テスト。`sql::explain`・
    /// `sql::aggregate` から呼ばれる補正が、実行時の索引構築対象選択
    /// （`resolve_scalar_index_target_in_txn`・`ScalarIndex::build_targeted`）と
    /// 一致する結果を返すことを固定する。
    mod scalar_plan_under_target_tests {
        use super::*;
        use crate::sql::scalar_plan::ScalarPlan;

        // `kind`（列添字 1）・`path`（列添字 2）を持つ既存 `schema()` を流用する。

        #[test]
        fn auto_target_passes_plan_through_unchanged() {
            let schema = schema();
            let target = Ok(ScalarIndexTargetOwned::Auto);
            for plan in [
                ScalarPlan::IndexEquality,
                ScalarPlan::IndexConjunction,
                ScalarPlan::IndexIdRange,
                ScalarPlan::PlainScan,
            ] {
                assert_eq!(
                    scalar_plan_under_target(plan, [Some(1)], &schema, &target),
                    plan,
                    "Auto は既存挙動（受け入れ条件 2）をビット単位で変えない"
                );
            }
        }

        #[test]
        fn declared_target_downgrades_excluded_column_to_plain_scan() {
            let schema = schema();
            // `kind`（添字 1）のみを宣言対象にする。述語は `path`（添字 2）。
            let target = Ok(ScalarIndexTargetOwned::Declared(vec!["kind".to_string()]));
            assert_eq!(
                scalar_plan_under_target(ScalarPlan::IndexEquality, [Some(2)], &schema, &target),
                ScalarPlan::PlainScan,
                "宣言で対象外にした列への述語は plain scan へ降格する"
            );
        }

        #[test]
        fn declared_target_keeps_plan_when_all_columns_included() {
            let schema = schema();
            let target = Ok(ScalarIndexTargetOwned::Declared(vec!["kind".to_string()]));
            assert_eq!(
                scalar_plan_under_target(ScalarPlan::IndexEquality, [Some(1)], &schema, &target),
                ScalarPlan::IndexEquality,
                "全列が宣言内なら元の plan を維持する"
            );
        }

        #[test]
        fn catalog_read_failure_downgrades_to_plain_scan_even_for_id_range() {
            let schema = schema();
            let target: Result<ScalarIndexTargetOwned, ()> = Err(());
            assert_eq!(
                scalar_plan_under_target(ScalarPlan::IndexIdRange, [], &schema, &target),
                ScalarPlan::PlainScan,
                "カタログ読み取り失敗は id 述語のみの形も含め fail-closed に \
                 plain scan へ倒す（`id_index` も含め構築されないため）"
            );
        }

        #[test]
        fn id_only_predicate_is_not_downgraded_under_declared() {
            let schema = schema();
            let target = Ok(ScalarIndexTargetOwned::Declared(vec!["kind".to_string()]));
            assert_eq!(
                scalar_plan_under_target(ScalarPlan::IndexIdRange, [], &schema, &target),
                ScalarPlan::IndexIdRange,
                "id 述語のみ（metadata_filter_columns が空）は id_index が宣言の \
                 有無によらず常に構築されるため降格しない"
            );
        }

        #[test]
        fn unresolvable_column_index_downgrades_to_plain_scan() {
            let schema = schema();
            let target = Ok(ScalarIndexTargetOwned::Declared(vec!["kind".to_string()]));
            assert_eq!(
                scalar_plan_under_target(ScalarPlan::IndexEquality, [None], &schema, &target),
                ScalarPlan::PlainScan,
                "解決不能な添字（ビット集合の範囲外）は fail-closed に降格する"
            );
        }

        #[test]
        fn plain_scan_input_is_always_plain_scan() {
            let schema = schema();
            for target in [
                Ok(ScalarIndexTargetOwned::Auto),
                Ok(ScalarIndexTargetOwned::Declared(vec!["kind".to_string()])),
                Err(()),
            ] {
                assert_eq!(
                    scalar_plan_under_target(ScalarPlan::PlainScan, [Some(1)], &schema, &target),
                    ScalarPlan::PlainScan
                );
            }
        }
    }
}
