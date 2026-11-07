//! ingest 経路（`engine::tenant::insert_rows` → `insert_rows_unchecked`）の段別
//! 内訳プロファイル（Issue #396）。書き込み経路には既存の専用ベンチが無く
//! （`examples/feature_bench.rs` の `ingest` フェーズが唯一の計測手段で、e2e の
//! 合計レイテンシしか出さない）、`insert_rows_unchecked` の内部段（所有権検査・
//! バッチ内 id 重複検出・`begin_write`・content_hash・台帳記録・encode・redb
//! insert・世代更新・commit）のどれが支配的かの実測が存在しない。本ベンチは
//! Issue #356（hybrid 段別）・#362（KNN 段別）と同型の手動専用・情報提供専用
//! ベンチとして、その内訳を実測する。
//!
//! # 測定設計
//!
//! `insert_rows_unchecked` の内部段はいずれも `pub(crate)` で、独立コンパイル
//! 単位であるベンチから直接計測できない（`harness::ingest_profile` 冒頭コメント
//! 参照）。そのため 2 つの手段を組み合わせる:
//!
//! - **E0**: `engine::tenant::insert_rows`（pub API。`examples/feature_bench.rs`
//!   と同じ入口）を丸ごと計測する e2e 計測。
//! - **I1〜I8**: 生 `redb::Database`（既存 dev-dependency）を用いた計装レプリカ。
//!   `insert_rows_unchecked` の各段を同じテーブル構成（`user_rows/docs`・
//!   `op_ledger`・`last_op`・`table_generation`）・同じ順序で再現し、段ごとに
//!   `Instant` で区切って計測する。
//!
//! 段の実行順序は production（`insert_rows_unchecked`）に合わせて
//! I1 → I2 → **I5 → I3** → I4 → I6 → I7 → I8 とする（Issue #397 で production が
//! 「行ごとに 1 回だけ encode し、その結果を台帳ハッシュと redb 書き込みで共有する」
//! 構成へ変更されたことに追随。段の識別子・ラベル・8 段構成そのものは Issue #396 の
//! ものを維持し、実行順序のみ入れ替える）。
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | I1 | 所有権検査（`PolicyContext::is_owner` 全件）＋ バッチ内 id 重複検出 |
//! | I2 | `begin_write` |
//! | I5 | encode（行ごとに 1 回のみ。Issue #397 以前は I3 の内部でも再度 encode
//! しており **encode が 2 回**走っていたが、現在は行ごとに 1 回のみで
//! I3・I6 の双方が I5 の結果を共有する） |
//! | I3 | content_hash（I5 のエンコード済みバイト列に対する SHA-256 再実装のみ） |
//! | I4 | 台帳記録（`op_ledger` get+insert・`last_op` insert） |
//! | I6 | 次元検証（行ごと）＋ redb insert（`insert` の戻り値が `None` であることを
//! 検査） |
//! | I7 | 世代更新（`table_generation` get→checked_add→insert） |
//! | I8 | `commit` |
//! | 残差 | E0 − Σ(I1..I8)。スキーマ取得（カタログデコード）・`commit_boundary`
//! ガード・抽象化コストに相当する |
//!
//! E0・I1〜I8 いずれも、warmup 20 バッチ ＋ 計測 20 バッチ ＝ 40 バッチ
//! （既定 1,000 行/バッチ ＝ 40,000 行）を同一テーブルへ連続投入する成長テーブル
//! 条件で計測する（`harness::protocol::MeasurementConfig` の下限検証を利用）。
//! E0・レプリカは別々の一時 DB へ書き込み、入力データ（id 範囲・可視性・埋め込み・
//! metadata）はバッチ番号から `DeterministicRng` で毎回再生成し両者で一致させる
//! （メモリに保持しない）。
//!
//! # 整合性検証（fail-closed）
//!
//! 1. 計測後、E0 DB・レプリカ DB の `user_rows/docs` 全エントリがバイト単位で一致し、
//!    件数が投入バッチ数 × 行数と一致すること（encode 再実装のドリフト検出）。
//! 2. E0 DB の `op_ledger` エントリ（計測フェーズの各バッチ分）を復号し、
//!    [`content_hash_insert_batch_reimpl`] の結果と一致すること（content_hash
//!    再実装のドリフト検出）。
//! 3. E0 DB・レプリカ DB それぞれの `table_generation["docs"]` が投入バッチ数と
//!    一致すること（世代更新モデルの検証）。
//! 4. I6 段の `insert` 戻り値が常に `None`（新規行）であること。
//!
//! いずれかが不一致ならベンチはエラー終了し測定値を出力しない。
//!
//! # env による可変化（R2）
//!
//! - `BENCH_INGEST_PROFILE_ROWS`: 1 バッチの行数。既定 1,000・許容 1..=10,000。
//! - `BENCH_INGEST_PROFILE_DIM`: 次元。既定 128・許容 1..=4,096。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線しない。
//! `GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。`make
//! bench-ingest-profile`（Makefile）から実行する。判定ロジック自体（時間非依存）は
//! `harness::ingest_profile` にあり `tests/ingest_profile_accept.rs` で
//! `make ci` 側から回帰検証する。redb は本クレートの既存 dev-dependency
//! （`=4.2.0`）を再利用し、新規依存の追加は行わない。

// 許可（clippy::disallowed_methods）: 本ファイルは engine の公開 API の外から redb を直接開き、
// 永続内容の検査・破壊・裏口の対照を行う。変更ログの捕捉（`storage::change_log`）を通す
// 対象は engine 製品コードの書き込み点で、外部クレートとしての直接操作は対象外。
#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::ingest_profile::{
    content_hash_insert_batch_reimpl, content_hash_typed_insert_reimpl,
    decode_ledger_entry_v2_reimpl, encode_row_reimpl, last_op_entry_reimpl, ledger_entry_v2_reimpl,
    ns_per_row, parse_bounded_env, parse_durability, parse_insert_mode, parse_profile_mode,
    refuse_under_github_actions, render_stage_line, residual_ns_per_row, rows_per_sec,
    sum_durations, BenchDurability, IngestProfileError, InsertMode, ProfileMode, StageId,
    StageSamples, DEFAULT_SINGLE_STATEMENTS, MAX_SINGLE_STATEMENTS, MIN_SINGLE_STATEMENTS,
    SINGLE_WARMUP_STATEMENTS,
};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;
use harness::sql_c1::vector_literal;
use harness::stats;

use std::collections::HashSet;
use std::time::Instant;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::row_codec::{encode_scalar_columns, Value};
use engine::sql::allowlist::validate_insert;
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_insert_form, BoundInsertForm};
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility, WriteDurability};
use engine::tenant;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-ingest";
const TABLE: &str = "docs";
const COLUMN: &str = "embedding";

/// レプリカが直接操作するテーブル定義（`crates/engine/src/catalog.rs::
/// user_rows_table_name` (`"user_rows/{table}"`)・`storage.rs::RowStoreTableDef`・
/// `recovery/ledger.rs::OP_LEDGER_TABLE`/`LAST_OP_TABLE`・
/// `catalog.rs::TABLE_GENERATION_TABLE` の契約をベンチ内で複製したもの。ドリフト
/// 検出は `tests/ingest_profile_accept.rs` が正本との突き合わせで行う
/// （`harness::ingest_profile` 冒頭コメント参照）。
const ROW_TABLE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("user_rows/docs");
const OP_LEDGER_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("op_ledger");
const LAST_OP_TABLE: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("last_op");
const TABLE_GENERATION_TABLE: TableDefinition<&str, u64> = TableDefinition::new("table_generation");

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("ingest_profile_bench: {msg}");
    std::process::exit(1);
}

/// `std::env::var` を fail-closed に読む（codex-review 指摘・PR #415）。
/// `std::env::var(...).ok()` は未設定（`VarError::NotPresent`）だけでなく
/// 非 UTF-8 値（`VarError::NotUnicode`）も一律 `None` に変換してしまい、
/// [`parse_bounded_env`] の「未設定→既定値」経路へ誤って合流する
/// （不正な非 UTF-8 値が既定値へフォールバックし、モジュール冒頭コメント
/// 「R2」節が宣言する fail-closed 契約に反する）。ここで両者を区別し、
/// `NotUnicode` は [`IngestProfileError::InvalidEnv`] として明示的に拒否する。
fn read_env_var(name: &'static str) -> Result<Option<String>, IngestProfileError> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(IngestProfileError::InvalidEnv {
            name,
            reason: "value is not valid UTF-8".to_string(),
        }),
    }
}

/// 1 バッチ分の投入データ（id・可視性・embedding・metadata）。呼び出し元が
/// `DeterministicRng::new(seed_base + batch_idx)` から毎回再生成し、E0・レプリカの
/// 双方で同一内容になるようにする（メモリに保持しない。モジュール冒頭コメント参照）。
struct Batch {
    ids: Vec<u64>,
    is_public: Vec<bool>,
    embeddings: Vec<Vec<f32>>,
    metadata: Vec<Vec<u8>>,
}

fn make_batch(
    schema: &TableSchema,
    seed_base: u64,
    batch_idx: u64,
    rows: usize,
    dim: usize,
) -> Batch {
    let mut rng = DeterministicRng::new(seed_base.wrapping_add(batch_idx));
    let start_id = batch_idx * rows as u64 + 1;
    let mut ids = Vec::with_capacity(rows);
    let mut is_public = Vec::with_capacity(rows);
    let mut embeddings = Vec::with_capacity(rows);
    let mut metadata = Vec::with_capacity(rows);
    for i in 0..rows {
        let id = start_id + i as u64;
        ids.push(id);
        // feature_bench と同様、10 件に 1 件を Private にする。
        is_public.push(!id.is_multiple_of(10));
        let embedding = rng.next_vector(dim);
        let body = format!(
            "ingest profile bench row id={id} batch={batch_idx} filler text for metadata payload sizing"
        );
        let encoded = encode_scalar_columns(
            schema,
            &[Value::Vector(embedding.clone()), Value::Text(body)],
        )
        .expect("encode_scalar_columns for ingest profile batch");
        embeddings.push(embedding);
        metadata.push(encoded);
    }
    Batch {
        ids,
        is_public,
        embeddings,
        metadata,
    }
}

fn schema(dim: usize) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new(COLUMN, ColumnType::Vector(dim as u32), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// エントリポイント（Issue #484）: `BENCH_INGEST_PROFILE_MODE` で `batch`
/// （既定・Issue #396 の既存挙動。[`run_batch_mode`]）／`single`（crossdb ベンチが
/// 通る単文 wire 経路の段別内訳。[`run_single_mode`]）を切り替える。
/// `GITHUB_ACTIONS` 拒否はモード分岐より前に行う（両モード共通の安全弁）。
fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }
    let mode_raw = match read_env_var("BENCH_INGEST_PROFILE_MODE") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let mode = match parse_profile_mode(mode_raw.as_deref()) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    match mode {
        ProfileMode::Batch => run_batch_mode(),
        ProfileMode::Single => run_single_mode(),
    }
}

/// Issue #396 の既存モード（既定）。`engine::tenant::insert_rows`（複数行 1 write
/// txn）の段別内訳を計測する。モジュール冒頭コメント参照。
fn run_batch_mode() {
    let rows_raw = match read_env_var("BENCH_INGEST_PROFILE_ROWS") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let rows = match parse_bounded_env(
        "BENCH_INGEST_PROFILE_ROWS",
        rows_raw.as_deref(),
        1_000,
        1,
        10_000,
    ) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let dim_raw = match read_env_var("BENCH_INGEST_PROFILE_DIM") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let dim = match parse_bounded_env(
        "BENCH_INGEST_PROFILE_DIM",
        dim_raw.as_deref(),
        128,
        1,
        4_096,
    ) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let insert_mode_raw = match read_env_var("BENCH_INGEST_PROFILE_INSERT_MODE") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let insert_mode = match parse_insert_mode(insert_mode_raw.as_deref()) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let insert_mode_label = match insert_mode {
        InsertMode::Insert => "insert",
        InsertMode::Reserve => "reserve",
    };
    // batch モードは durability A/B（`run_single_mode` 専用機能。Issue #851）に
    // 対応しない。未設定・明示 "immediate" のみ受理し、"none" は fail-closed に
    // 拒否する（`insert_mode_raw` と同じ「他モード専用 env を黙って無視しない」
    // 方針。coding-rust.md「untrusted 入力の扱い」）。
    let durability_raw = match read_env_var("BENCH_INGEST_PROFILE_DURABILITY") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    match durability_raw.as_deref() {
        None | Some("immediate") => {}
        Some(other) => fail_closed(format!(
            "BENCH_INGEST_PROFILE_DURABILITY={other:?} is not supported in batch mode (only \"immediate\" or unset)"
        )),
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "ingest_profile_bench: rows_per_batch={rows} dim={dim} tenant={TENANT} table={TABLE} insert_mode={insert_mode_label}"
    );

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let warmup = config.warmup_iterations() as u64;
    let measured = config.measured_iterations() as u64;
    let total_batches = warmup + measured;

    let table_schema = schema(dim);
    let ctx = PolicyContext::new(TENANT).expect("valid tenant id");

    // --- E0: pub API 経由の e2e 計測 --------------------------------------------
    let e2e_path = unique_db_path("issue396-ingest-profile-e2e");
    let _e2e_guard = CleanupGuard(e2e_path.clone());
    let e2e_summary = {
        let e2e_storage = Storage::open(&e2e_path).expect("open e2e storage");
        e2e_storage
            .create_table(&table_schema)
            .expect("create e2e table");

        for batch_idx in 0..warmup {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            insert_e2e_batch(&e2e_storage, &ctx, &batch, batch_idx, "warmup");
        }
        let mut e2e_samples = Vec::with_capacity(measured as usize);
        for batch_idx in warmup..total_batches {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            let elapsed = insert_e2e_batch(&e2e_storage, &ctx, &batch, batch_idx, "measured");
            e2e_samples.push(elapsed);
        }
        if e2e_samples.is_empty() {
            fail_closed("E0 measurement produced no samples");
        }
        let summary = stats::summarize(&e2e_samples).expect("E0 summarize must succeed");
        // `e2e_storage`（内部に redb の書き込み可能ハンドルを保持）をここで明示的に
        // drop してファイルロックを解放する。redb は書き込み可能ハンドルを同一
        // プロセスから同時に複数開けない契約（`redb::DatabaseError::
        // DatabaseAlreadyOpen`）のため、後続の整合性検証（`write_txn_read_only_open`
        // による生 `redb::Database::open` 再オープン）と同時生存させられない
        // （`knn_profile_bench.rs` と同じ理由・同じ対処）。
        drop(e2e_storage);
        summary
    };

    // --- レプリカ: 生 redb による段別計装 -----------------------------------
    let replica_path = unique_db_path("issue396-ingest-profile-replica");
    let _replica_guard = CleanupGuard(replica_path.clone());
    {
        // レプリカ用テーブルは初回 open_table（書き込みモード）で自動作成される
        // （`redb::WriteTransaction::open_table` の契約。`user_rows_table_name`
        // ドキュメント参照）ため、`Storage::create_table` は経由しない
        // （レプリカはカタログ層を再現しない設計。モジュール冒頭コメント参照）。
        let replica_db = Database::create(&replica_path).expect("create replica db");

        for batch_idx in 0..warmup {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            run_replica_batch(
                &replica_db,
                &ctx,
                &table_schema,
                &batch,
                batch_idx,
                None,
                insert_mode,
            );
        }
        let mut stage_samples = StageSamples::new();
        for batch_idx in warmup..total_batches {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            run_replica_batch(
                &replica_db,
                &ctx,
                &table_schema,
                &batch,
                batch_idx,
                Some(&mut stage_samples),
                insert_mode,
            );
        }

        // --- 段別集計（出力はまだ行わない） --------------------------------------
        // fail-closed 契約（モジュール冒頭コメント「いずれかが不一致ならベンチは
        // エラー終了し測定値を出力しない」）を満たすため、ここでは中央値の算出
        // （＝ `fail_closed` を伴いうる集計処理そのもの）のみ行い、実際の
        // `println!` は後続の整合性検証 1〜3 がすべて通過した後にまとめて行う
        // （Issue #396・Bugbot 指摘: 整合性検証より前に測定値を stdout へ出力
        // してはならない）。
        let mut stage_lines: Vec<String> = Vec::with_capacity(StageId::ALL.len());
        let mut stage_medians = Vec::with_capacity(StageId::ALL.len());
        for stage in StageId::ALL {
            let samples = stage_samples.samples_for(stage);
            let summary = stats::summarize(samples).unwrap_or_else(|e| {
                fail_closed(format!("stage {:?} summarize failed: {e}", stage))
            });
            let npr = ns_per_row(summary.median, rows).unwrap_or_else(|e| fail_closed(e));
            stage_lines.push(render_stage_line(stage.label(), rows, summary.median, npr));
            stage_medians.push(summary.median);
        }
        let stage_sum = sum_durations(&stage_medians);
        let stage_sum_npr = ns_per_row(stage_sum, rows).unwrap_or_else(|e| fail_closed(e));
        stage_lines.push(format!(
            "stage(SUM_I1_I8): rows={rows} median={:.3}ms ns_per_row={stage_sum_npr:.1}",
            stage_sum.as_secs_f64() * 1e3
        ));
        let e0_npr = ns_per_row(e2e_summary.median, rows).unwrap_or_else(|e| fail_closed(e));
        stage_lines.push(format!(
            "stage(E0_insert_rows): rows={rows} median={:.3}ms ns_per_row={e0_npr:.1}",
            e2e_summary.median.as_secs_f64() * 1e3
        ));
        match residual_ns_per_row(e2e_summary.median, stage_sum, rows) {
                Ok(residual_npr) => stage_lines.push(format!(
                    "residual(E0-SUM): ns_per_row={residual_npr:.1} (schema fetch / commit_boundary guard / abstraction overhead)"
                )),
                Err(e) => stage_lines.push(format!(
                    "residual(E0-SUM): n/a (Σ(I1..I8) の中央値が E0 の中央値を上回った。独立計測どうしの比較のため測定ノイズにより逆転しうる: {e})"
                )),
            }

        // --- 整合性検証 1: user_rows/docs のバイト単位一致 -----------------------
        // `redb::Database` を drop すると内部状態が「closed」になり、それより前に
        // 取得済みの `ReadTransaction` も後続操作が `StorageError::DatabaseClosed`
        // で失敗する（`redb::db.rs` の実装契約）。そのため `e2e_db` はこのブロックの
        // 終わりまで生存させる（`write_txn_read_only_open` のような「開いて
        // ReadTransaction だけ返す」ヘルパー関数には切り出さない）。
        let e2e_db = Database::open(&e2e_path).expect("reopen E0 db read-only for integrity check");
        let e2e_read = e2e_db.begin_read().expect("begin_read on E0 db");
        let replica_read = replica_db
            .begin_read()
            .expect("begin_read on replica db for integrity check");
        let e2e_rows = collect_row_table(&e2e_read);
        let replica_rows = collect_row_table(&replica_read);
        let expected_count = (total_batches as usize) * rows;
        if e2e_rows.len() != expected_count {
            fail_closed(format!(
                "E0 row count mismatch: expected {expected_count}, got {}",
                e2e_rows.len()
            ));
        }
        if replica_rows.len() != expected_count {
            fail_closed(format!(
                "replica row count mismatch: expected {expected_count}, got {}",
                replica_rows.len()
            ));
        }
        if e2e_rows != replica_rows {
            fail_closed(
                    "user_rows/docs entries differ between E0 and replica DBs (encode_row_reimpl drift)",
                );
        }
        println!(
            "integrity: user_rows/docs byte-identical across E0/replica (count={expected_count})"
        );

        // --- 整合性検証 3: table_generation ----------------------------------
        // E0 側は `Storage::create_table`（DDL）自体も `bump_table_generation_in_txn`
        // を 1 回呼ぶ（`catalog.rs::bump_table_generation_in_txn` ドキュメント
        // 「呼び出し元」列挙参照）ため、投入バッチ数 + 1 になる。レプリカは
        // カタログ層（`create_table`）を再現しない設計（モジュール冒頭コメント
        // 「レプリカ用テーブルは...」参照）のため投入バッチ数のまま。
        let e2e_gen = read_table_generation(&e2e_read);
        let replica_gen = read_table_generation(&replica_read);
        let expected_e2e_gen = total_batches + 1;
        if e2e_gen != expected_e2e_gen {
            fail_closed(format!(
                "E0 table_generation mismatch: expected {expected_e2e_gen}, got {e2e_gen}"
            ));
        }
        if replica_gen != total_batches {
            fail_closed(format!(
                "replica table_generation mismatch: expected {total_batches}, got {replica_gen}"
            ));
        }
        println!(
            "integrity: table_generation == {expected_e2e_gen} (E0, includes create_table) / {total_batches} (replica)"
        );

        // --- 整合性検証 2: 計測フェーズの op_ledger エントリ ↔ content_hash 再実装 --
        let ledger_table = e2e_read
            .open_table(OP_LEDGER_TABLE)
            .expect("open op_ledger table on E0 db");
        let last_op_table = e2e_read
            .open_table(LAST_OP_TABLE)
            .expect("open last_op table on E0 db");
        for batch_idx in warmup..total_batches {
            let batch = make_batch(&table_schema, 1, batch_idx, rows, dim);
            let op_label = format!("ingest-profile-e2e-{batch_idx}");
            let key = (TENANT, TABLE, op_label.as_str());
            let stored = ledger_table
                .get(key)
                .expect("read op_ledger entry")
                .unwrap_or_else(|| {
                    fail_closed(format!("op_ledger entry missing for batch {batch_idx}"))
                });
            let stored_hash = decode_ledger_entry_v2_reimpl(stored.value())
                .unwrap_or_else(|e| fail_closed(format!("op_ledger entry decode failed: {e}")));
            let encoded_rows: Vec<Vec<u8>> = (0..rows)
                .map(|i| {
                    encode_row_reimpl(
                        TENANT,
                        batch.is_public[i],
                        &batch.embeddings[i],
                        &batch.metadata[i],
                    )
                    .expect("encode_row_reimpl for content_hash cross-check")
                })
                .collect();
            let rows_for_hash: Vec<(u64, &[u8])> = batch
                .ids
                .iter()
                .zip(encoded_rows.iter())
                .map(|(id, enc)| (*id, enc.as_slice()))
                .collect();
            let recomputed = content_hash_insert_batch_reimpl(&rows_for_hash)
                .expect("content_hash_insert_batch_reimpl for cross-check");
            if recomputed != stored_hash {
                fail_closed(format!(
                    "content_hash mismatch for batch {batch_idx} (content_hash reimpl drift)"
                ));
            }
            let last_op_value = last_op_table
                .get((TENANT, TABLE))
                .expect("read last_op entry")
                .unwrap_or_else(|| fail_closed("last_op entry missing"));
            if batch_idx == total_batches - 1
                && last_op_value.value() != last_op_entry_reimpl(&op_label)
            {
                fail_closed("last_op entry does not match last committed batch's operation_id");
            }
        }
        println!("integrity: op_ledger content_hash matches content_hash_insert_batch_reimpl for all measured batches");

        // --- 段別集計の出力（整合性検証 1〜3 をすべて通過した後） ------------------
        // fail-closed 契約（モジュール冒頭コメント参照）を満たすため、測定値の
        // stdout 出力はここまで遅延する（上の「段別集計（出力はまだ行わない）」
        // ブロック参照）。
        for line in &stage_lines {
            println!("{line}");
        }
    }

    println!("ingest_profile_bench: OK");
}
/// crossdb ベンチ（`docs/design/crossdb-bench.md`）が実際に通る単文 `INSERT`
/// 経路（wire 簡易クエリ → `EngineCore::execute_sql_in_session` →
/// `execute_insert_sql`〔`validate_insert` → `get_table_schema` →
/// `bind_insert_form`〕→ `sql::exec::execute_insert` →
/// `tenant::insert_typed_row_unchecked`〔1 文 1 write txn〕）の段別内訳を計測する
/// （Issue #484。親 Issue #483）。`docs/design/ingest-stage-profile.md`
/// 「Issue #484 追記」節の判断により、production への計測フック追加は行わず
/// （`bench-internals` feature 限定であっても `crates/engine/src/` の変更に
/// 変わりないため。CLAUDE.md ステータス行の production 定義に従う）、
/// 公開 API のみを使う tier（P0/E0/S0）と生 redb レプリカ（I1〜I8）の組み合わせで
/// 内訳を得る（`run_batch_mode` と同じ測定設計方針）。
///
/// tier:
/// - P0 `parse_bind`: `validate_insert` → `get_table_schema` → `bind_insert_form`
///   （書き込みなし。パース・束縛だけを単独計測する）
/// - E0 `typed_row_api`: `tenant::insert_typed_row`（Rust API 経由の単文 e2e）
/// - S0 `sql_surface`: `EngineCore::execute_sql_in_session`（wire と同一入口）
/// - I1〜I8: 生 redb レプリカ（`insert_typed_row_unchecked` と同順序の再現。
///   `StageId`・ラベルは `run_batch_mode` と共有するが、single モードでは
///   「バッチ内 id 重複検出」に相当する処理が無いため I1 の内容は
///   「VECTOR 列位置探索 ＋ `validate_embedding_dim`」に読み替える
///   （`run_replica_single` のコメント参照）。
///
/// P0/E0/S0/レプリカはそれぞれ別々の一時 DB で単独計測する（redb は書き込み
/// 可能ハンドルを同一プロセスから同時に複数開けないため。`run_batch_mode` の
/// E0 とレプリカの関係と同じ制約。§3.3「クレート境界」参照）。
///
/// 帰属: パース・束縛 ≒ S0 − E0（P0 の直接計測値と突き合わせて妥当性確認）、
/// engine 内部段 ＝ Σ(I1..I8)、残差 ＝ E0 − Σ(I1..I8)。
fn run_single_mode() {
    let statements_raw = match read_env_var("BENCH_INGEST_PROFILE_STATEMENTS") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let statements = match parse_bounded_env(
        "BENCH_INGEST_PROFILE_STATEMENTS",
        statements_raw.as_deref(),
        DEFAULT_SINGLE_STATEMENTS,
        MIN_SINGLE_STATEMENTS,
        MAX_SINGLE_STATEMENTS,
    ) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    // MIN_SINGLE_STATEMENTS（README・ingest-stage-profile.md で公開している
    // 下限。現状 2,000 ＝ SINGLE_WARMUP_STATEMENTS の 2 倍）を計測フェーズ
    // 長さ SINGLE_WARMUP_STATEMENTS 件以上を要求する下限としてそのまま
    // 受理できるよう、境界を「未満のみ拒否」にする（`<=` だと
    // MIN_SINGLE_STATEMENTS ちょうどが自己矛盾的に拒否されていた）。
    if statements < SINGLE_WARMUP_STATEMENTS.saturating_mul(2) {
        fail_closed(format!(
            "BENCH_INGEST_PROFILE_STATEMENTS={statements} too small relative to warmup {SINGLE_WARMUP_STATEMENTS} (need at least {} for a meaningful measured phase)",
            SINGLE_WARMUP_STATEMENTS * 2
        ));
    }
    match read_env_var("BENCH_INGEST_PROFILE_ROWS") {
        Ok(Some(_)) => {
            println!("ingest_profile_bench: BENCH_INGEST_PROFILE_ROWS is ignored in single mode");
        }
        Ok(None) => {}
        Err(e) => fail_closed(e),
    }
    let dim_raw = match read_env_var("BENCH_INGEST_PROFILE_DIM") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let dim = match parse_bounded_env(
        "BENCH_INGEST_PROFILE_DIM",
        dim_raw.as_deref(),
        128,
        1,
        4_096,
    ) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    // single モードは I6 の insert/reserve A/B（`run_batch_mode` 専用機能）に
    // 対応しない。未設定・明示 "insert" のみ受理し、"reserve" は fail-closed に
    // 拒否する（黙って batch モード用の値を無視しない。coding-rust.md
    // 「untrusted 入力の扱い」）。
    let insert_mode_raw = match read_env_var("BENCH_INGEST_PROFILE_INSERT_MODE") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    match insert_mode_raw.as_deref() {
        None | Some("insert") => {}
        Some(other) => fail_closed(format!(
            "BENCH_INGEST_PROFILE_INSERT_MODE={other:?} is not supported in single mode (only \"insert\" or unset)"
        )),
    }
    // durability A/B（Issue #851）。未設定は既定 `Immediate` のままビット同一の
    // 出力を保つ。`BenchDurability` から `engine::storage::WriteDurability`
    // （Issue #849）への写像は本関数内でのみ行う（harness は engine に依存しない
    // 独立コンパイル単位のため）。
    let durability_raw = match read_env_var("BENCH_INGEST_PROFILE_DURABILITY") {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let bench_durability = match parse_durability(durability_raw.as_deref()) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let write_durability = match bench_durability {
        BenchDurability::Immediate => WriteDurability::Immediate,
        BenchDurability::None => WriteDurability::None,
    };

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "ingest_profile_bench: mode=single statements={statements} dim={dim} tenant={TENANT} table={TABLE} durability={}",
        bench_durability.token()
    );

    let warmup = SINGLE_WARMUP_STATEMENTS as u64;
    let total_stmts = statements as u64;
    let table_schema = schema(dim);
    let ctx = PolicyContext::new(TENANT).expect("valid tenant id");

    // --- P0: parse_bind（読み取り専用。書き込みを一切行わない） -----------------
    let p0_path = unique_db_path("issue484-ingest-single-p0");
    let _p0_guard = CleanupGuard(p0_path.clone());
    let (p0_summary, p0_total, p0_min) = {
        let p0_storage = Storage::open(&p0_path).expect("open P0 storage");
        p0_storage
            .create_table(&table_schema)
            .expect("create P0 table");
        let mut samples = Vec::with_capacity((total_stmts - warmup) as usize);
        for n in 0..total_stmts {
            let row = make_single_row(1, n, dim);
            let literal = vector_literal(&row.embedding).expect("vector_literal for P0");
            let sql = single_stmt_sql(
                row.id,
                literal.as_str(),
                &row.body,
                &format!("ingest-single-p0-{n}"),
            );
            let t = Instant::now();
            let validated = validate_insert(&sql, &p0_storage, LedgerMode::default())
                .expect("validate_insert for P0");
            let schema_for_bind = p0_storage
                .get_table_schema(&validated.table_name)
                .expect("get_table_schema for P0");
            let bound =
                bind_insert_form(&validated, &schema_for_bind).expect("bind_insert_form for P0");
            let elapsed = t.elapsed();
            if !matches!(bound, BoundInsertForm::Row(_)) {
                fail_closed("P0: expected row-form INSERT binding");
            }
            if n >= warmup {
                samples.push(elapsed);
            }
        }
        drop(p0_storage);
        let min = samples.iter().copied().min().unwrap_or_else(|| {
            fail_closed("P0 samples must be non-empty (protocol minimums satisfied)")
        });
        let summary = stats::summarize(&samples)
            .unwrap_or_else(|e| fail_closed(format!("P0 summarize failed: {e}")));
        let total = sum_durations(&samples);
        (summary, total, min)
    };

    // --- E0: tenant::insert_typed_row（Rust API 経由の単文 e2e） -----------------
    let e0_path = unique_db_path("issue484-ingest-single-e0");
    let _e0_guard = CleanupGuard(e0_path.clone());
    let (e0_summary, e0_total, e0_min) = {
        let e0_storage = Storage::open_with_durability(&e0_path, write_durability)
            .expect("open E0 storage with durability");
        e0_storage
            .create_table(&table_schema)
            .expect("create E0 table");
        let mut samples = Vec::with_capacity((total_stmts - warmup) as usize);
        for n in 0..total_stmts {
            let row = make_single_row(1, n, dim);
            let op_id = OperationId::parse(&format!("ingest-single-e0-{n}"))
                .expect("valid operation id for E0");
            let values = vec![
                Value::Vector(row.embedding.clone()),
                Value::Text(row.body.clone()),
            ];
            let t = Instant::now();
            tenant::insert_typed_row(
                &e0_storage,
                TABLE,
                &ctx,
                row.id,
                Visibility::Private,
                &values,
                &op_id,
            )
            .expect("insert_typed_row for E0");
            let elapsed = t.elapsed();
            if n >= warmup {
                samples.push(elapsed);
            }
        }
        // 整合性検証（後段）のため read-only 再オープンする前に書き込みハンドルを
        // 解放する（`run_batch_mode` の E0 と同じ理由。redb は同一プロセスから
        // 書き込みハンドルを同時に複数開けない）。
        drop(e0_storage);
        let min = samples.iter().copied().min().unwrap_or_else(|| {
            fail_closed("E0 samples must be non-empty (protocol minimums satisfied)")
        });
        let summary = stats::summarize(&samples)
            .unwrap_or_else(|e| fail_closed(format!("E0 summarize failed: {e}")));
        let total = sum_durations(&samples);
        (summary, total, min)
    };

    // --- S0: EngineCore::execute_sql_in_session（wire と同一入口） --------------
    let s0_path = unique_db_path("issue484-ingest-single-s0");
    let _s0_guard = CleanupGuard(s0_path.clone());
    let (s0_summary, s0_total, s0_min) = {
        let s0_storage = Storage::open_with_durability(&s0_path, write_durability)
            .expect("open S0 storage with durability");
        s0_storage
            .create_table(&table_schema)
            .expect("create S0 table");
        let core = EngineCore::from_storage(s0_storage, engine::search_engine::default_engine());
        let mut session = SessionState::default();
        let mut samples = Vec::with_capacity((total_stmts - warmup) as usize);
        for n in 0..total_stmts {
            let row = make_single_row(1, n, dim);
            let literal = vector_literal(&row.embedding).expect("vector_literal for S0");
            let sql = single_stmt_sql(
                row.id,
                literal.as_str(),
                &row.body,
                &format!("ingest-single-s0-{n}"),
            );
            let t = Instant::now();
            let outcome = core
                .execute_sql_in_session(&ctx, &mut session, &sql)
                .expect("execute_sql_in_session for S0");
            let elapsed = t.elapsed();
            match outcome {
                SqlOutcome::Insert(o) if o.rows_affected == 1 => {}
                other => fail_closed(format!("S0: unexpected outcome for n={n}: {other:?}")),
            }
            if n >= warmup {
                samples.push(elapsed);
            }
        }
        let min = samples.iter().copied().min().unwrap_or_else(|| {
            fail_closed("S0 samples must be non-empty (protocol minimums satisfied)")
        });
        let summary = stats::summarize(&samples)
            .unwrap_or_else(|e| fail_closed(format!("S0 summarize failed: {e}")));
        let total = sum_durations(&samples);
        (summary, total, min)
    };

    // --- レプリカ: 生 redb による段別計装（I1〜I8） -----------------------------
    let replica_path = unique_db_path("issue484-ingest-single-replica");
    let _replica_guard = CleanupGuard(replica_path.clone());
    let replica_db = Database::create(&replica_path).expect("create replica db");
    let mut stage_samples = StageSamples::new();
    for n in 0..total_stmts {
        let row = make_single_row(1, n, dim);
        if n < warmup {
            run_replica_single(&replica_db, &table_schema, &row, n, None, bench_durability);
        } else {
            run_replica_single(
                &replica_db,
                &table_schema,
                &row,
                n,
                Some(&mut stage_samples),
                bench_durability,
            );
        }
    }
    let mut stage_lines: Vec<String> = Vec::with_capacity(StageId::ALL.len() + 3);
    let mut stage_medians = Vec::with_capacity(StageId::ALL.len());
    for stage in StageId::ALL {
        let samples = stage_samples.samples_for(stage);
        let summary = stats::summarize(samples)
            .unwrap_or_else(|e| fail_closed(format!("stage {:?} summarize failed: {e}", stage)));
        stage_lines.push(render_stage_line(
            stage.label(),
            1,
            summary.median,
            summary.median.as_secs_f64() * 1e9,
        ));
        stage_medians.push(summary.median);
    }
    let stage_sum = sum_durations(&stage_medians);

    // --- 整合性検証（fail-closed。すべて通過するまで測定値を出力しない） -----------
    // 1. user_rows/docs のバイト単位一致（E0 ↔ レプリカ。encode_row_reimpl の
    //    ドリフト検出。`run_batch_mode` の整合性検証 1 と同型）。
    let e0_db = Database::open(&e0_path).expect("reopen E0 db read-only for integrity check");
    let e0_read = e0_db.begin_read().expect("begin_read on E0 db");
    let replica_read = replica_db
        .begin_read()
        .expect("begin_read on replica db for integrity check");
    let e0_rows = collect_row_table(&e0_read);
    let replica_rows = collect_row_table(&replica_read);
    let expected_count = total_stmts as usize;
    if e0_rows.len() != expected_count {
        fail_closed(format!(
            "E0 row count mismatch: expected {expected_count}, got {}",
            e0_rows.len()
        ));
    }
    if replica_rows.len() != expected_count {
        fail_closed(format!(
            "replica row count mismatch: expected {expected_count}, got {}",
            replica_rows.len()
        ));
    }
    if e0_rows != replica_rows {
        fail_closed(
            "user_rows/docs entries differ between E0 and replica DBs (encode_row_reimpl drift)",
        );
    }
    println!("integrity: user_rows/docs byte-identical across E0/replica (count={expected_count})");

    // 2. table_generation（E0 は create_table 分 +1、レプリカはカタログ層を
    //    再現しないため投入文数のまま。`run_batch_mode` の整合性検証 3 と同型）。
    let e0_gen = read_table_generation(&e0_read);
    let replica_gen = read_table_generation(&replica_read);
    let expected_e0_gen = total_stmts + 1;
    if e0_gen != expected_e0_gen {
        fail_closed(format!(
            "E0 table_generation mismatch: expected {expected_e0_gen}, got {e0_gen}"
        ));
    }
    if replica_gen != total_stmts {
        fail_closed(format!(
            "replica table_generation mismatch: expected {total_stmts}, got {replica_gen}"
        ));
    }
    println!(
        "integrity: table_generation == {expected_e0_gen} (E0, includes create_table) / {total_stmts} (replica)"
    );

    // 3. op_ledger の content_hash ↔ content_hash_typed_insert_reimpl（計測フェーズの
    //    先頭 200 件をサンプル照合する。全件照合は本ベンチの所要時間を大きく
    //    伸ばす一方、ドリフトが有れば同一形式のエントリすべてに現れるため、
    //    サンプルで十分検出できる）。
    let e0_ledger = e0_read
        .open_table(OP_LEDGER_TABLE)
        .expect("open op_ledger table on E0 db");
    let sample_count = (total_stmts - warmup).min(200);
    for i in 0..sample_count {
        let n = warmup + i;
        let row = make_single_row(1, n, dim);
        let op_label = format!("ingest-single-e0-{n}");
        let key = (TENANT, TABLE, op_label.as_str());
        let stored = e0_ledger
            .get(key)
            .expect("read op_ledger entry on E0 db")
            .unwrap_or_else(|| fail_closed(format!("E0 op_ledger entry missing for n={n}")));
        let stored_hash = decode_ledger_entry_v2_reimpl(stored.value())
            .unwrap_or_else(|e| fail_closed(format!("E0 op_ledger entry decode failed: {e}")));
        let recomputed = content_hash_typed_insert_reimpl(
            row.id,
            false,
            &row.embedding,
            &[("body", Some(row.body.as_str()))],
        )
        .expect("content_hash_typed_insert_reimpl for cross-check");
        if recomputed != stored_hash {
            fail_closed(format!(
                "content_hash mismatch for E0 n={n} (content_hash_typed_insert_reimpl drift)"
            ));
        }
    }
    println!(
        "integrity: op_ledger content_hash matches content_hash_typed_insert_reimpl for {sample_count} sampled measured statements (E0)"
    );
    drop(e0_read);
    drop(e0_db);
    drop(replica_read);
    drop(replica_db);

    // --- 出力（整合性検証をすべて通過した後） ------------------------------------
    let measured = (total_stmts - warmup) as usize;
    let p0_rps = rows_per_sec(measured, p0_total).unwrap_or(f64::NAN);
    let e0_rps = rows_per_sec(measured, e0_total).unwrap_or(f64::NAN);
    let s0_rps = rows_per_sec(measured, s0_total).unwrap_or(f64::NAN);
    println!(
        "tier(P0_parse_bind): stmts={measured} min={:.3}ms median={:.3}ms rows_per_sec={p0_rps:.1}",
        p0_min.as_secs_f64() * 1e3,
        p0_summary.median.as_secs_f64() * 1e3
    );
    println!(
        "tier(E0_typed_row_api): stmts={measured} min={:.3}ms median={:.3}ms rows_per_sec={e0_rps:.1}",
        e0_min.as_secs_f64() * 1e3,
        e0_summary.median.as_secs_f64() * 1e3
    );
    println!(
        "tier(S0_sql_surface): stmts={measured} min={:.3}ms median={:.3}ms rows_per_sec={s0_rps:.1}",
        s0_min.as_secs_f64() * 1e3,
        s0_summary.median.as_secs_f64() * 1e3
    );
    for line in &stage_lines {
        println!("{line}");
    }
    println!(
        "stage(SUM_I1_I8): stmts=1 median={:.3}ms",
        stage_sum.as_secs_f64() * 1e3
    );
    match e0_summary.median.checked_sub(stage_sum) {
        Some(residual) => println!(
            "residual(E0-SUM): median={:.3}ms (schema fetch / commit_boundary guard / abstraction overhead)",
            residual.as_secs_f64() * 1e3
        ),
        None => println!(
            "residual(E0-SUM): n/a (Σ(I1..I8) の中央値が E0 の中央値を上回った。独立計測どうしの比較のため測定ノイズにより逆転しうる)"
        ),
    }
    match s0_summary.median.checked_sub(e0_summary.median) {
        Some(diff) => println!(
            "attribution(S0-E0, parse/bind/dispatch informational): median={:.3}ms (P0 direct measurement: {:.3}ms)",
            diff.as_secs_f64() * 1e3,
            p0_summary.median.as_secs_f64() * 1e3
        ),
        None => println!(
            "attribution(S0-E0): n/a (S0 の中央値が E0 の中央値を下回った。独立計測どうしの比較のため測定ノイズにより逆転しうる)"
        ),
    }

    println!("ingest_profile_bench: OK");
}

/// single モード 1 文分の合成入力（決定的に再生成する。メモリに保持しない）。
struct SingleRow {
    id: u64,
    embedding: Vec<f32>,
    body: String,
}

/// 文番号 `n`（0 起点）から 1 文分の入力を決定的に再生成する（P0/E0/S0/レプリカの
/// 4 tier すべてが同一の `seed_base` から同一内容を再生成し、一致させる）。
fn make_single_row(seed_base: u64, n: u64, dim: usize) -> SingleRow {
    let mut rng = DeterministicRng::new(seed_base.wrapping_add(n));
    SingleRow {
        id: n + 1,
        embedding: rng.next_vector(dim),
        body: format!("ingest single stmt bench row {n}"),
    }
}

/// single モードの規範形 `INSERT` 文を組み立てる（`docs/spec/04-behavior/
/// sql-surface.md` SQL-10 の行形 `INSERT ... USING OPERATION_ID` 構文）。
/// `id`・`op_label` はベンチ内部の `u64`／決定的な数値サフィックス付き固定語彙、
/// `literal` は [`vector_literal`]（検証済み型 `VectorLiteral`）、`body` は
/// [`make_single_row`] が生成する固定書式の文字列のみから構成し、外部・untrusted
/// 入力を連結しない（coding-rust.md「SQL 文字列の組み立てに未検証入力を連結しない」）。
fn single_stmt_sql(id: u64, literal: &str, body: &str, op_label: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, body) VALUES ({id}, '{literal}', '{body}') USING OPERATION_ID '{op_label}'"
    )
}

/// レプリカ 1 文分を `insert_typed_row_unchecked` と同順序（I1..I8）で再現する
/// （single モード版。`run_replica_batch` のバッチ版と対をなす）。
///
/// production の `insert_typed_row_unchecked` はバッチ内 id 重複検出を持たない
/// （1 文 1 行のため対象が存在しない）ため、I1 の内容は「VECTOR 列位置探索 ＋
/// `values.get(vector_idx)` の参照 ＋ `validate_embedding_dim`」に読み替える
/// （production では該当処理がスキーマ取得直後・encode 共有化前に行われる。
/// `tenant.rs::insert_typed_row_unchecked` 参照）。
fn run_replica_single(
    db: &Database,
    table_schema: &TableSchema,
    row: &SingleRow,
    n: u64,
    mut stage_samples: Option<&mut StageSamples>,
    durability: BenchDurability,
) {
    // I1: VECTOR 列位置探索 ＋ validate_embedding_dim。
    let t = Instant::now();
    let vector_idx = table_schema
        .columns
        .iter()
        .position(|c| matches!(c.ty, ColumnType::Vector(_)))
        .expect("schema has a VECTOR column");
    std::hint::black_box(vector_idx);
    table_schema
        .validate_embedding_dim(row.embedding.len())
        .expect("validate_embedding_dim for I1");
    record(&mut stage_samples, StageId::Precheck, t.elapsed());

    // I2: begin_write（durability none の場合は `set_durability` 呼び出しも
    // I2 の計時内に含める。production の `Storage::begin_write_txn` が
    // `begin_write` と `set_durability` を同一関数内で連続実行する契約
    // と対応させるため。Issue #851）。
    let t = Instant::now();
    let mut write_txn = db
        .begin_write()
        .expect("begin_write for replica single stmt");
    if matches!(durability, BenchDurability::None) {
        write_txn
            .set_durability(redb::Durability::None)
            .expect("set_durability(None) for replica single stmt");
    }
    record(&mut stage_samples, StageId::BeginWrite, t.elapsed());

    // I5: encode（storage::encode_row 相当。行ごとに 1 回のみ。I3・I6 の双方が
    // この結果を共有する。`run_replica_batch` の I5 と同じ設計）。
    let t = Instant::now();
    let metadata = encode_scalar_columns(
        table_schema,
        &[
            Value::Vector(row.embedding.clone()),
            Value::Text(row.body.clone()),
        ],
    )
    .expect("encode_scalar_columns for I5");
    let row_encoded = encode_row_reimpl(TENANT, false, &row.embedding, &metadata)
        .expect("encode_row_reimpl for I5");
    record(&mut stage_samples, StageId::Encode, t.elapsed());

    // I3: content_hash（`for_typed_insert` 再実装。I5 のエンコード済みバイト列
    // ではなく、生の embedding・非 VECTOR 列値から直接計算する契約
    // （`content_hash_typed_insert_reimpl` ドキュメント参照）。
    let t = Instant::now();
    let hash = content_hash_typed_insert_reimpl(
        row.id,
        false,
        &row.embedding,
        &[("body", Some(row.body.as_str()))],
    )
    .expect("content_hash_typed_insert_reimpl for I3");
    record(&mut stage_samples, StageId::ContentHash, t.elapsed());

    // I4: 台帳記録。
    let t = Instant::now();
    let op_label = format!("ingest-single-replica-{n}");
    {
        let mut ledger_table = write_txn
            .open_table(OP_LEDGER_TABLE)
            .expect("open op_ledger table for I4");
        let key = (TENANT, TABLE, op_label.as_str());
        if ledger_table
            .get(key)
            .expect("read op_ledger for I4")
            .is_some()
        {
            fail_closed(format!("unexpected duplicate op_ledger key for n={n}"));
        }
        ledger_table
            .insert(key, ledger_entry_v2_reimpl(&hash).as_slice())
            .expect("insert op_ledger entry for I4");
    }
    {
        let mut last_op_table = write_txn
            .open_table(LAST_OP_TABLE)
            .expect("open last_op table for I4");
        last_op_table
            .insert((TENANT, TABLE), last_op_entry_reimpl(&op_label).as_slice())
            .expect("insert last_op entry for I4");
    }
    record(&mut stage_samples, StageId::Ledger, t.elapsed());

    // I6: redb insert（`insert_unique_row` 相当。戻り値が `None` であることを検査）。
    let t = Instant::now();
    {
        let mut row_table = write_txn
            .open_table(ROW_TABLE)
            .expect("open user_rows/docs table for I6");
        let prev = row_table
            .insert((TENANT, row.id), row_encoded.as_slice())
            .expect("insert row for I6");
        if prev.is_some() {
            fail_closed(format!(
                "unexpected existing row for id={} (I6 uniqueness check)",
                row.id
            ));
        }
    }
    record(&mut stage_samples, StageId::RedbInsert, t.elapsed());

    // I7: 世代更新。
    let t = Instant::now();
    {
        let mut gen_table = write_txn
            .open_table(TABLE_GENERATION_TABLE)
            .expect("open table_generation table for I7");
        let current = gen_table
            .get(TABLE)
            .expect("read table_generation for I7")
            .map(|v| v.value())
            .unwrap_or(0);
        let next = current
            .checked_add(1)
            .expect("table_generation counter overflow");
        gen_table
            .insert(TABLE, next)
            .expect("insert table_generation for I7");
    }
    record(&mut stage_samples, StageId::GenerationBump, t.elapsed());

    // I8: commit。
    let t = Instant::now();
    write_txn.commit().expect("commit for I8");
    record(&mut stage_samples, StageId::Commit, t.elapsed());
}

/// E0: `insert_rows` 1 バッチ分を計測する。`op_id` はレプリカ側の台帳キーと
/// 対応させるため `format!("ingest-profile-e2e-{batch_idx}")` に固定する
/// （整合性検証 2 が同じラベルで `op_ledger` を照会する）。
fn insert_e2e_batch(
    storage: &Storage,
    ctx: &PolicyContext,
    batch: &Batch,
    batch_idx: u64,
    _phase: &str,
) -> std::time::Duration {
    let inputs: Vec<(u64, RowInput<'_>)> = batch
        .ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                *id,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: if batch.is_public[i] {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
                    embedding: &batch.embeddings[i],
                    metadata: &batch.metadata[i],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("ingest-profile-e2e-{batch_idx}"))
        .expect("valid operation id for E0 batch");
    let start = Instant::now();
    tenant::insert_rows(storage, TABLE, ctx, &inputs, &op_id).expect("insert_rows for E0 batch");
    start.elapsed()
}

/// レプリカ 1 バッチ分を `insert_rows_unchecked` と同順序（I1..I8）で再現する。
/// `stage_samples` が `Some` のときだけ段別 `Instant` を記録する（warmup 時は
/// `None` で計測しない）。
fn run_replica_batch(
    db: &Database,
    ctx: &PolicyContext,
    schema: &TableSchema,
    batch: &Batch,
    batch_idx: u64,
    mut stage_samples: Option<&mut StageSamples>,
    insert_mode: InsertMode,
) {
    let rows = batch.ids.len();

    // I1: 所有権検査 ＋ バッチ内 id 重複検出。production
    // （`insert_rows_unchecked`）は `rows.iter().any(|(_, row)| !ctx.is_owner(
    // row.tenant_id))` として単一の `PolicyContext`（バッチ外・呼び出し元が
    // 生成）を全行で使い回すため、ここでも呼び出し元から受け取った `ctx` を
    // バッチ内ループの外で 1 度だけ生成された値として再利用する
    // （`PolicyContext::new` を行ごとに呼ばない）。各行の実際の tenant 値
    // （すべて `TENANT`。`insert_e2e_batch` が `RowInput::tenant_id` を
    // `ctx.tenant_id()` から組み立てるため E0 と一致）を渡した `is_owner` の
    // 戻り値を `black_box` し、判定コストが最適化で消えないようにする
    // （codex-review P2・Issue #396 指摘）。
    let t = Instant::now();
    for _ in &batch.ids {
        std::hint::black_box(ctx.is_owner(TENANT));
    }
    let mut seen_ids: HashSet<u64> = HashSet::new();
    seen_ids.try_reserve(rows).expect("reserve id set");
    for id in &batch.ids {
        if !seen_ids.insert(*id) {
            fail_closed(format!("unexpected duplicate id in synthetic batch: {id}"));
        }
    }
    record(&mut stage_samples, StageId::Precheck, t.elapsed());

    // I2: begin_write。
    let t = Instant::now();
    let write_txn = db.begin_write().expect("begin_write for replica batch");
    record(&mut stage_samples, StageId::BeginWrite, t.elapsed());

    // I5: encode（行ごとに 1 回のみ。Issue #397 で production
    //〔`insert_rows_unchecked`〕が「ハッシュ計算用と書き込み用でそれぞれ
    // encode し合計 2 回走らせる」構成から「1 回 encode し、その結果をハッシュと
    // 書き込みで共有する」構成へ変更されたことに追随し、レプリカでも I3 より
    // 前で 1 回だけ encode する。`validate_embedding_dim` は production では
    // encode 共有化後も行ループ内（I6 側）に残っているため、ここでは呼ばない）。
    let t = Instant::now();
    let row_encoded: Vec<Vec<u8>> = (0..rows)
        .map(|i| {
            encode_row_reimpl(
                TENANT,
                batch.is_public[i],
                &batch.embeddings[i],
                &batch.metadata[i],
            )
            .expect("encode_row_reimpl for I5")
        })
        .collect();
    record(&mut stage_samples, StageId::Encode, t.elapsed());

    // I3: content_hash（I5 で得たエンコード済みバイト列に対する SHA-256 再実装のみ。
    // Issue #397 以前はここで全行を再度 encode していたが、事前エンコード共有化に
    // 伴い不要になった）。
    let t = Instant::now();
    let rows_for_hash: Vec<(u64, &[u8])> = batch
        .ids
        .iter()
        .zip(row_encoded.iter())
        .map(|(id, enc)| (*id, enc.as_slice()))
        .collect();
    let hash = content_hash_insert_batch_reimpl(&rows_for_hash).expect("content_hash for I3");
    record(&mut stage_samples, StageId::ContentHash, t.elapsed());

    // I4: 台帳記録。
    let t = Instant::now();
    let op_label = format!("ingest-profile-e2e-{batch_idx}");
    {
        let mut ledger_table = write_txn
            .open_table(OP_LEDGER_TABLE)
            .expect("open op_ledger table for I4");
        let key = (TENANT, TABLE, op_label.as_str());
        if ledger_table
            .get(key)
            .expect("read op_ledger for I4")
            .is_some()
        {
            fail_closed(format!(
                "unexpected duplicate op_ledger key for batch {batch_idx}"
            ));
        }
        ledger_table
            .insert(key, ledger_entry_v2_reimpl(&hash).as_slice())
            .expect("insert op_ledger entry for I4");
    }
    {
        let mut last_op_table = write_txn
            .open_table(LAST_OP_TABLE)
            .expect("open last_op table for I4");
        last_op_table
            .insert((TENANT, TABLE), last_op_entry_reimpl(&op_label).as_slice())
            .expect("insert last_op entry for I4");
    }
    record(&mut stage_samples, StageId::Ledger, t.elapsed());

    // I6: 次元検証 ＋ redb insert（`insert_unique_row` 相当。戻り値が `None`
    // であることを検査）。production の行ループが `validate_embedding_dim` を
    // encode・insert の直前ではなく I3・I4 の後（台帳記録後）に行うため
    // （`tenant.rs::insert_rows_unchecked`。Issue #397 で事前エンコードへ変更
    // した後も位置は不変）、レプリカでもここへ含める（codex-review P2・
    // Issue #396 指摘の測定契約「残差＝スキーマ取得・commit_boundary ガード・
    // 抽象化コスト」を維持するため。E0 の残差へ検証コストが混入しないようにする）。
    let t = Instant::now();
    {
        let mut row_table = write_txn
            .open_table(ROW_TABLE)
            .expect("open user_rows/docs table for I6");
        match insert_mode {
            InsertMode::Insert => {
                for (i, (id, encoded)) in batch.ids.iter().zip(row_encoded.iter()).enumerate() {
                    schema
                        .validate_embedding_dim(batch.embeddings[i].len())
                        .expect("validate_embedding_dim for I6 (matches production per-row check)");
                    let prev = row_table
                        .insert((TENANT, *id), encoded.as_slice())
                        .expect("insert row for I6");
                    if prev.is_some() {
                        fail_closed(format!(
                            "unexpected existing row for id={id} (I6 uniqueness check)"
                        ));
                    }
                }
            }
            InsertMode::Reserve => {
                // Issue #400: `insert_reserve` は `insert` と異なり既存値を
                // 返さない契約（redb 4.2.0 `Table::insert_reserve`）ため、
                // `insert_unique_row` 相当の一意性検査を事前 `get` で代替する
                // （試作限定の許容コスト。計画「契約面の制約」節参照）。
                //
                // codex-review 指摘（PR #420）: 以前はここで `encoded`
                // （I5 で作成済み）を使わず `encode_row_reimpl_into_slice` で
                // 予約済みバッファへ再度エンコードしており、Insert 側
                // （I5 の結果をそのまま insert するだけ）と処理範囲が
                // 揃わない二重エンコードになっていた。両モードとも I6 では
                // 「I5 のエンコード結果を書き込むだけ」に処理範囲を揃えるため、
                // 予約済みバッファへは encode し直さず `encoded` をコピーする
                // （`insert_reserve` に渡した長さと `guard.as_mut()` の長さは
                // 常に一致するため `copy_from_slice` は長さ不一致で panic しない）。
                for (i, (id, encoded)) in batch.ids.iter().zip(row_encoded.iter()).enumerate() {
                    schema
                        .validate_embedding_dim(batch.embeddings[i].len())
                        .expect("validate_embedding_dim for I6 (matches production per-row check)");
                    if row_table
                        .get((TENANT, *id))
                        .expect("read row for I6 reserve-mode uniqueness check")
                        .is_some()
                    {
                        fail_closed(format!(
                            "unexpected existing row for id={id} (I6 uniqueness check, reserve mode)"
                        ));
                    }
                    let mut guard = row_table
                        .insert_reserve((TENANT, *id), encoded.len())
                        .expect("insert_reserve row for I6");
                    guard.as_mut().copy_from_slice(encoded.as_slice());
                }
            }
        }
    }
    record(&mut stage_samples, StageId::RedbInsert, t.elapsed());

    // I7: 世代更新。
    let t = Instant::now();
    {
        let mut gen_table = write_txn
            .open_table(TABLE_GENERATION_TABLE)
            .expect("open table_generation table for I7");
        let current = gen_table
            .get(TABLE)
            .expect("read table_generation for I7")
            .map(|v| v.value())
            .unwrap_or(0);
        let next = current
            .checked_add(1)
            .expect("table_generation counter overflow");
        gen_table
            .insert(TABLE, next)
            .expect("insert table_generation for I7");
    }
    record(&mut stage_samples, StageId::GenerationBump, t.elapsed());

    // I8: commit。
    let t = Instant::now();
    write_txn.commit().expect("commit for I8");
    record(&mut stage_samples, StageId::Commit, t.elapsed());
}

fn record(
    stage_samples: &mut Option<&mut StageSamples>,
    stage: StageId,
    duration: std::time::Duration,
) {
    if let Some(samples) = stage_samples.as_mut() {
        samples.push(stage, duration);
    }
}

fn collect_row_table(read_txn: &redb::ReadTransaction) -> Vec<((String, u64), Vec<u8>)> {
    let table = read_txn
        .open_table(ROW_TABLE)
        .expect("open user_rows/docs table for integrity check");
    let mut out = Vec::new();
    for entry in table.iter().expect("iterate user_rows/docs table") {
        let (key_guard, value_guard) = entry.expect("row entry");
        let (tenant, id) = key_guard.value();
        out.push(((tenant.to_string(), id), value_guard.value().to_vec()));
    }
    out.sort();
    out
}

fn read_table_generation(read_txn: &redb::ReadTransaction) -> u64 {
    match read_txn.open_table(TABLE_GENERATION_TABLE) {
        Ok(t) => t.get(TABLE).ok().flatten().map(|v| v.value()).unwrap_or(0),
        Err(redb::TableError::TableDoesNotExist(_)) => 0,
        Err(e) => fail_closed(format!(
            "open table_generation for integrity check failed: {e}"
        )),
    }
}
