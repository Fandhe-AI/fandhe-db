//! 一括投入（複数ファイルをまとめたバッチ）に対する処理量ガード。
//! ポインタ: `docs/spec/05-tasks.md` TASK-122・`docs/spec/04-behavior/indexing.md`
//! INDEX-4。本モジュールが定義する上限の分類・数値・判定順序は本リポ独自の
//! 実装上の上限であり、spec 側契約の構造をそのまま転記したものではない
//! （spec-confidentiality.md 準拠。数値の出典は下記 [`BatchLimits`] のドキュメント
//! 参照）。
//!
//! `chunking::MAX_INPUT_BYTES`・`incremental::MAX_CHUNKS_PER_FILE`・
//! `incremental::MAX_INDEX_TOTAL_BYTES` はいずれもファイル 1 件分（TASK-120）の
//! 上限であり、複数ファイル合計に対する上限を持たない。本モジュールは
//! [`BatchLimits`] の各フィールドとして、バッチ全体に対する処理量ガードを追加する。
//!
//! 各上限の判定タイミングは [`validate_batch_shape`]・[`validate_chunk_total`]・
//! [`validate_raw_sql_len`] の各関数ドキュメント、および呼び出し元
//! `core::EngineCore::execute_insert_sql_batch`・`incremental::index_file_batch` の
//! コメントを参照。いずれの超過も副作用ゼロ（redb・インメモリ索引・
//! `operation_id` 台帳とも変更なし）で `54000`（`PAYLOAD_TOO_LARGE`。ERR-2・
//! TASK-152）として拒否する。
//!
//! wire 層の 1 メッセージ 1 MiB 上限（WIRE-4、`wire-server/src/limits.rs`）は
//! transport 層の別防御であり、本モジュールの代替にはならない（engine 側の
//! ローカル API 経由の一括投入にも本ガードが独立して適用される）。
//!
//! 判定関数はいずれも純粋関数で、本文・パスの複製や追加確保を行わない
//! （長さ情報のみで判定する。coding-rust.md「不安全な設計 / DoS」対応）。
//! 累算はすべて `checked_add` を用い、オーバーフローは拒否側へ倒す
//! （coding-rust.md「整数演算は checked_*／saturating_* を使う」）。

/// 一括投入 4 上限の設定値。既定値は [`Default`] 実装を参照。
///
/// `max_files_per_batch` の優先順位は「wire-server の CLI `--batch-max-files`
/// 明示 > 環境変数 `FANDHE_DB_BATCH_MAX_FILES` > 既定 64」（Issue #1166）。
/// 値は `1..=`[`MAX_BATCH_MAX_FILES`] に限定され、この範囲では
/// `batch_raw_sql_len_budget` の飽和演算がオーバーフローしない。
///
/// **既定値の出典に関する注記**（spec-confidentiality.md 準拠）: ①③④の具体的な
/// 上限数値は private spec（INDEX-4）の判断事項であり、本ソース（public リポ）に
/// 数値そのものとして固定しない。[`Default`] は環境変数（下記）による注入を優先し、
/// 未設定時は spec の決定値ではない、本リポ独自の保守的なプレースホルダー値
/// （②は既存公開定数 [`crate::chunking::MAX_INPUT_BYTES`]・③④は同じく既存公開定数
/// [`crate::incremental::MAX_INDEX_TOTAL_BYTES`]／[`crate::incremental::MAX_CHUNKS_PER_FILE`]
/// を再利用するのみで、新たな private 由来の乗数・定数は導入しない）へ fail-closed で
/// フォールバックする。実運用値は運用環境側で環境変数として注入する。
#[derive(Debug, Clone, Copy)]
pub struct BatchLimits {
    /// ① バッチあたり最大ファイル数。
    pub max_files_per_batch: usize,
    /// ② 1 ファイルあたり最大本文サイズ（バイト）。単一ファイル経由の
    /// `chunking::MAX_INPUT_BYTES` と一致させ、バッチ経由で単発より大きい本文を
    /// 通さない。
    pub max_file_body_bytes: usize,
    /// ③ バッチ合計最大サイズ（バイト）。全ファイルの `path` 長 + 本文長の合計。
    pub max_batch_total_bytes: usize,
    /// ④ バッチあたり最大生成チャンク数（全ファイルの合計。サーバー側算定）。
    pub max_batch_chunks: usize,
}

/// 環境変数からの上限値注入。未設定・空文字・parse 失敗・0 はいずれも
/// フォールバックへ倒す（fail-closed。untrusted な環境変数値で `0` 等の
/// 意図しない全遮断上限を成立させない）。
fn env_usize_or(var_name: &str, fallback: usize) -> usize {
    match std::env::var(var_name) {
        Ok(raw) => raw
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or(fallback),
        Err(_) => fallback,
    }
}

/// `max_files_per_batch` の既定値（環境変数未設定・不正値時のフォールバック）。
/// 既存リテラルの単一情報源（Issue #1166）。
pub const DEFAULT_MAX_FILES_PER_BATCH: usize = 64;

/// `max_files_per_batch` に設定できる上限。`--max-insert-rows` と範囲を揃える
/// ため既存公開定数 [`crate::sql::parser::MAX_DML_ROW_LIMIT`] を再利用する
/// （新たな定数値を導入しない）。`batch_raw_sql_len_budget` の予算が際限なく
/// 大きくならないための DoS 上限でもある（Issue #1166）。
pub const MAX_BATCH_MAX_FILES: usize = crate::sql::parser::MAX_DML_ROW_LIMIT;

/// [`validate_max_files_per_batch`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct MaxFilesPerBatchOutOfRange {
    value: usize,
}

impl std::fmt::Display for MaxFilesPerBatchOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range 1..={MAX_BATCH_MAX_FILES}",
            self.value
        )
    }
}

impl std::error::Error for MaxFilesPerBatchOutOfRange {}

/// `max_files_per_batch` の設定値を `1..=`[`MAX_BATCH_MAX_FILES`] で検証する
/// 単一情報源。wire-server の CLI フラグ `--batch-max-files` と環境変数
/// `FANDHE_DB_BATCH_MAX_FILES` の両経路から使われる（優先順位は
/// CLI 明示 > 環境変数 > 既定）。
pub fn validate_max_files_per_batch(value: usize) -> Result<usize, MaxFilesPerBatchOutOfRange> {
    if (1..=MAX_BATCH_MAX_FILES).contains(&value) {
        Ok(value)
    } else {
        Err(MaxFilesPerBatchOutOfRange { value })
    }
}

/// 環境変数値（文字列）から `max_files_per_batch` を解決する純関数。未設定・
/// parse 失敗・範囲外（0・上限超過）はいずれも既定値へ fail-closed で倒す。
fn parse_env_max_files(raw: Option<&str>) -> usize {
    raw.and_then(|r| r.trim().parse::<usize>().ok())
        .and_then(|v| validate_max_files_per_batch(v).ok())
        .unwrap_or(DEFAULT_MAX_FILES_PER_BATCH)
}

/// 改名前（プロジェクト名 vector-db 時代）の環境変数名と、その後継名の対応表。
/// 旧名は読まない（値を一切適用しない）。wire-server の起動経路が
/// [`legacy_batch_env_vars_set`] の結果を警告として stderr へ出すための単一情報源。
pub const LEGACY_BATCH_ENV_VARS: [(&str, &str); 3] = [
    ("VECTOR_DB_BATCH_MAX_FILES", "FANDHE_DB_BATCH_MAX_FILES"),
    (
        "VECTOR_DB_BATCH_MAX_TOTAL_BYTES",
        "FANDHE_DB_BATCH_MAX_TOTAL_BYTES",
    ),
    ("VECTOR_DB_BATCH_MAX_CHUNKS", "FANDHE_DB_BATCH_MAX_CHUNKS"),
];

/// `is_set` が真を返す旧名を `(旧名, 新名)` で列挙する純関数。環境の参照を
/// 注入できるようにして単体テストをプロセス環境から切り離す。
fn legacy_batch_env_vars_where(is_set: impl Fn(&str) -> bool) -> Vec<(&'static str, &'static str)> {
    LEGACY_BATCH_ENV_VARS
        .iter()
        .copied()
        .filter(|(old, _)| is_set(old))
        .collect()
}

/// 現在のプロセス環境に設定されている旧名の一覧を返す。値は読まず存在のみを
/// 見る（警告に設定値を含めないため。非 UTF-8 値でも検出する）。wire-server の
/// 起動時に呼ばれ、1 変数 1 行の警告を出す。engine 自身は出力しない。
pub fn legacy_batch_env_vars_set() -> Vec<(&'static str, &'static str)> {
    legacy_batch_env_vars_where(|name| std::env::var_os(name).is_some())
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_files_per_batch: parse_env_max_files(
                std::env::var("FANDHE_DB_BATCH_MAX_FILES").ok().as_deref(),
            ),
            max_file_body_bytes: crate::chunking::MAX_INPUT_BYTES,
            max_batch_total_bytes: env_usize_or(
                "FANDHE_DB_BATCH_MAX_TOTAL_BYTES",
                crate::incremental::MAX_INDEX_TOTAL_BYTES,
            ),
            max_batch_chunks: env_usize_or(
                "FANDHE_DB_BATCH_MAX_CHUNKS",
                crate::incremental::MAX_CHUNKS_PER_FILE,
            ),
        }
    }
}

/// [`validate_batch_shape`]・[`validate_chunk_total`]・[`validate_raw_sql_len`] の
/// 失敗理由。全 variant が `wire_code() == "54000"`（`PAYLOAD_TOO_LARGE`）へ写像
/// される（ERR-2・TASK-152）。`detail`／`Display` には件数・上限値のみを含め、
/// 本文・パス・テナント情報は一切含めない（security.md「エラー・ログ経由で
/// 他テナントのデータ・存在情報を漏らさない」）。
#[derive(Debug)]
pub enum BatchLimitsError {
    /// ①超過。
    TooManyFiles { count: usize, max: usize },
    /// ②超過。`index` はバッチ内の 0 起点インデックス。
    FileBodyTooLarge {
        index: usize,
        len: usize,
        max: usize,
    },
    /// ③超過（累算オーバーフローも本 variant へ倒す。`total` はオーバーフロー時
    /// `usize::MAX` を用いる）。
    BatchTotalTooLarge { total: usize, max: usize },
    /// ④超過（累算オーバーフローも本 variant へ倒す）。
    TooManyChunks { total: usize, max: usize },
    /// 解析前ガード（[`validate_raw_sql_len`]）: 1 文の生 SQL テキスト長が
    /// [`raw_sql_len_budget`] を超過。`index` はバッチ内の 0 起点インデックス。
    /// codex-review 指摘・PR #242 対応（②の判定は束縛後の decode 済み本文長にしか
    /// 効かず、束縛（`lexer::tokenize`・`bind_insert_form` の文字列複製）自体が
    /// 入力サイズに比例した処理を先に行ってしまうための、束縛より前の粗い早期
    /// リジェクト）。
    SqlTextTooLarge {
        index: usize,
        len: usize,
        max: usize,
    },
    /// 解析前ガード: バッチ内の生 SQL テキスト長の累計が
    /// [`batch_raw_sql_len_budget`] ベースの合計予算を超過（累算オーバーフローも本
    /// variant へ倒す。`total` はオーバーフロー時 `usize::MAX` を用いる）。
    BatchSqlTextTooLarge { total: usize, max: usize },
}

impl BatchLimitsError {
    /// SQLSTATE 風 `wire_code`（ERR-2 の共通分類。`sql/exec.rs` が
    /// `SqlSurfaceError::payload_too_large` へ写像する際の根拠）。
    pub fn wire_code(&self) -> &'static str {
        "54000"
    }
}

impl std::fmt::Display for BatchLimitsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchLimitsError::TooManyFiles { count, max } => {
                write!(f, "batch file count {count} exceeds limit {max}")
            }
            BatchLimitsError::FileBodyTooLarge { index, len, max } => {
                write!(
                    f,
                    "file at batch index {index} body size {len} exceeds limit {max}"
                )
            }
            BatchLimitsError::BatchTotalTooLarge { total, max } => {
                write!(f, "batch total size {total} exceeds limit {max}")
            }
            BatchLimitsError::TooManyChunks { total, max } => {
                write!(f, "batch chunk count {total} exceeds limit {max}")
            }
            BatchLimitsError::SqlTextTooLarge { index, len, max } => {
                write!(
                    f,
                    "raw SQL text at batch index {index} length {len} exceeds pre-parse budget {max}"
                )
            }
            BatchLimitsError::BatchSqlTextTooLarge { total, max } => {
                write!(
                    f,
                    "batch raw SQL text total length {total} exceeds pre-parse budget {max}"
                )
            }
        }
    }
}

impl std::error::Error for BatchLimitsError {}

/// ①②③（ファイル数・ファイル単体本文サイズ・バッチ合計サイズ）を判定する
/// （バッチの解析段階。チャンク化・埋め込み・write トランザクションより前に
/// 呼ぶ契約。`incremental::index_file_batch` の唯一の呼び出し元）。
///
/// `files` はバッチ内の各ファイルの `(path.len(), body.len())`。本文・パス自体は
/// 受け取らない（長さ情報のみで判定し、複製・追加確保を行わない）。
pub(crate) fn validate_batch_shape(
    files: &[(usize, usize)],
    limits: &BatchLimits,
) -> Result<(), BatchLimitsError> {
    if files.len() > limits.max_files_per_batch {
        return Err(BatchLimitsError::TooManyFiles {
            count: files.len(),
            max: limits.max_files_per_batch,
        });
    }

    let mut total: usize = 0;
    for (index, (path_len, body_len)) in files.iter().enumerate() {
        if *body_len > limits.max_file_body_bytes {
            return Err(BatchLimitsError::FileBodyTooLarge {
                index,
                len: *body_len,
                max: limits.max_file_body_bytes,
            });
        }
        let file_total =
            path_len
                .checked_add(*body_len)
                .ok_or(BatchLimitsError::BatchTotalTooLarge {
                    total: usize::MAX,
                    max: limits.max_batch_total_bytes,
                })?;
        total = total
            .checked_add(file_total)
            .ok_or(BatchLimitsError::BatchTotalTooLarge {
                total: usize::MAX,
                max: limits.max_batch_total_bytes,
            })?;
    }

    if total > limits.max_batch_total_bytes {
        return Err(BatchLimitsError::BatchTotalTooLarge {
            total,
            max: limits.max_batch_total_bytes,
        });
    }
    Ok(())
}

/// ④（バッチあたり最大生成チャンク数）を判定する（チャンク分割後・埋め込み処理の
/// 開始前に呼ぶ契約。呼び出し元が全ファイルの `chunk_phase` 結果のチャンク数を
/// `checked_add` で累算した値を渡す）。
pub(crate) fn validate_chunk_total(
    total_chunks: usize,
    limits: &BatchLimits,
) -> Result<(), BatchLimitsError> {
    if total_chunks > limits.max_batch_chunks {
        return Err(BatchLimitsError::TooManyChunks {
            total: total_chunks,
            max: limits.max_batch_chunks,
        });
    }
    Ok(())
}

/// 生 SQL テキスト長（`str::len()`。デコード前）に対する保守的な予算。
///
/// `sql::lexer::lex_string_literal` の文字列リテラルエスケープは `''` → `'`
/// （2 生バイト → 1 デコード後バイト）のみであり、デコード後の内容が生テキストより
/// 長くなることはない（最悪でも生テキストの半分）。したがって
/// `2 * decoded_max + overhead` は「デコード後に `decoded_max` 以内へ収まり得る
/// 生テキストの上限」の安全側の見積りとなる（`overhead` は `INSERT INTO`・
/// テーブル名・列リスト・`USING OPERATION_ID '<id>'` 句等、本文・パス以外の
/// 構文分の余裕）。
///
/// 呼び出し元は 1 文単位の予算算出に `decoded_max` として
/// [`BatchLimits::max_batch_total_bytes`] を渡す（[`BatchLimits::max_file_body_bytes`]
/// ではない）。後段の正式な形状判定（[`validate_batch_shape`]）は `body` に②の
/// 個別上限を課す一方、`path` には個別上限を課さず `path.len() + body.len()` が
/// ③（バッチ合計上限）以内であれば 1 文として受理する。したがって 1 文が正当に
/// 取り得るデコード後の最大長（`path.len() + body.len()` の理論上限）は③であり、
/// ②を基礎にすると path が大きく body が小さい正当な入力を束縛前に誤って拒否する
/// （PR #242 レビュー対応）。
const RAW_SQL_OVERHEAD_BYTES: usize = 4096;

/// [`RAW_SQL_OVERHEAD_BYTES`] のドキュメント参照。1 文単位の予算算出に使う
/// （構文オーバーヘッドを 1 文分のみ加算する）。
fn raw_sql_len_budget(decoded_max: usize) -> usize {
    decoded_max
        .checked_mul(2)
        .and_then(|doubled| doubled.checked_add(RAW_SQL_OVERHEAD_BYTES))
        .unwrap_or(usize::MAX)
}

/// バッチ累計側の予算算出（codex-review P1 指摘・PR #242 対応）。
///
/// `running_raw_total` はバッチ内の複数文の生 SQL テキスト長を合算した値であり、
/// 各文にはそれぞれ独立した `INSERT INTO`・テーブル名・列リスト・
/// `USING OPERATION_ID '<id>'` 等の構文オーバーヘッド（[`RAW_SQL_OVERHEAD_BYTES`]）
/// が存在する。[`raw_sql_len_budget`] のように構文オーバーヘッドを 1 回しか
/// 加算しないと、デコード後の `path + body` 合計が③以内・各本文が②以内という
/// 後段契約を満たす正当な多文バッチでも、文数分のオーバーヘッドの分だけ解析前に
/// 誤って `54000` へ拒否されうる。呼び出し元がバッチ内で許容する最大文数
/// （`max_files_per_batch`。①の上限であり、本関数呼び出し時点では束縛ループの
/// 前段で既に `sqls.len() <= max_files_per_batch` を検証済み）を渡し、
/// オーバーヘッドをその文数分だけ含める。乗算・加算はいずれも `saturating_*`／
/// `checked_*` を用い、オーバーフロー時は `usize::MAX`（＝実質無制限）へ倒す
/// （拒否側ではなく緩める方向への飽和だが、`max_files_per_batch` はサーバー側
/// 構成値であり untrusted な単一リクエスト入力からは到達しない。加えて
/// `decoded_max` 自体の 2 倍加算により実運用上限では発生しない想定。
/// [`raw_sql_len_budget`] と同じ設計方針）。
fn batch_raw_sql_len_budget(decoded_max: usize, max_files_per_batch: usize) -> usize {
    let overhead = max_files_per_batch.saturating_mul(RAW_SQL_OVERHEAD_BYTES);
    decoded_max
        .checked_mul(2)
        .and_then(|doubled| doubled.checked_add(overhead))
        .unwrap_or(usize::MAX)
}

/// 束縛（`sql::allowlist::validate_insert` の `lexer::tokenize`・
/// `sql::parser::bind_insert_form` の `path`/`body` 文字列複製）より前に、1 文の
/// 生 SQL テキスト長とバッチ内の累計テキスト長を判定する（解析前の粗い早期
/// リジェクト。codex-review 指摘・PR #242 対応）。
///
/// ②③（[`validate_batch_shape`]）はデコード後の `path`/`body` 長にしか作用せず、
/// 束縛処理自体（構文木の構築・文字列複製）は入力サイズに比例した処理を
/// 判定より前に必ず行ってしまう。本関数は束縛前に呼ぶことで、極端に巨大な単一
/// SQL 文（③の実効上限からは通常あり得ない生テキスト長）に対する束縛処理
/// そのものを回避する。デコード後の実際の長さに対する正確な判定は
/// [`validate_batch_shape`]（束縛後、`incremental::index_file_batch` 呼び出し前の
/// 最終防衛線）が引き続き担い、本関数の予算判定に代わるものではない
/// （[`raw_sql_len_budget`] が示す通り本関数の予算は 2 倍 + 余裕を持たせた
/// 保守的な上限であり、正確な上限判定ではない）。1 文単位の予算は
/// [`BatchLimits::max_batch_total_bytes`] を基礎に算出する（`path` に個別上限が
/// ないため。[`RAW_SQL_OVERHEAD_BYTES`] のドキュメント参照）。
///
/// `running_raw_total` は呼び出し元がこれまでの生テキスト長を `checked_add` で
/// 累算した値。戻り値は今回の `sql_len` を加算した新しい累計（呼び出し元は次回
/// 呼び出しへそのまま渡す）。本文・パス自体は受け取らず、長さ情報のみで判定する
/// （複製・追加確保を行わない。`validate_batch_shape` と同じ設計）。
pub(crate) fn validate_raw_sql_len(
    index: usize,
    sql_len: usize,
    running_raw_total: usize,
    limits: &BatchLimits,
) -> Result<usize, BatchLimitsError> {
    // 1 文が正当に取り得るデコード後最大長は body の②ではなく、path に個別上限が
    // ない後段契約（③）である（RAW_SQL_OVERHEAD_BYTES ドキュメント参照）。
    let per_file_budget = raw_sql_len_budget(limits.max_batch_total_bytes);
    if sql_len > per_file_budget {
        return Err(BatchLimitsError::SqlTextTooLarge {
            index,
            len: sql_len,
            max: per_file_budget,
        });
    }

    // バッチ累計側の予算は文数分の構文オーバーヘッドを含める
    // （batch_raw_sql_len_budget ドキュメント参照。codex-review P1 指摘・PR #242
    // 対応）。`limits.max_files_per_batch` は呼び出し元 `execute_insert_sql_batch`
    // が本関数呼び出しループの前段で `sqls.len()` に対してすでに検証済みの上限。
    let batch_budget =
        batch_raw_sql_len_budget(limits.max_batch_total_bytes, limits.max_files_per_batch);
    let new_total =
        running_raw_total
            .checked_add(sql_len)
            .ok_or(BatchLimitsError::BatchSqlTextTooLarge {
                total: usize::MAX,
                max: batch_budget,
            })?;
    if new_total > batch_budget {
        return Err(BatchLimitsError::BatchSqlTextTooLarge {
            total: new_total,
            max: batch_budget,
        });
    }
    Ok(new_total)
}

/// ①（バッチあたり最大ファイル数。COPY では「行数」と読み替える）を、CopyDone
/// を待たず 1 行ずつ逐次判定する（Issue #939・WIRE-17。[`validate_batch_shape`]
/// が一括判定するのに対し、COPY はストリーミングであるため超過を CopyData
/// 受信中に検出する必要がある。判定内容自体は同一）。
pub(crate) fn check_row_count(count: usize, limits: &BatchLimits) -> Result<(), BatchLimitsError> {
    if count > limits.max_files_per_batch {
        return Err(BatchLimitsError::TooManyFiles {
            count,
            max: limits.max_files_per_batch,
        });
    }
    Ok(())
}

/// ②（1 ファイルあたり最大本文サイズ。COPY では「1 行あたりのデコード後
/// バイト量」と読み替える）を、CopyDone を待たず 1 行ずつ逐次判定する
/// （[`check_row_count`] と同じ理由）。
pub(crate) fn check_row_body_len(
    index: usize,
    len: usize,
    limits: &BatchLimits,
) -> Result<(), BatchLimitsError> {
    if len > limits.max_file_body_bytes {
        return Err(BatchLimitsError::FileBodyTooLarge {
            index,
            len,
            max: limits.max_file_body_bytes,
        });
    }
    Ok(())
}

/// ③（バッチ合計最大サイズ）を、CopyDone を待たず CopyData 受信ごとに逐次
/// 判定する（[`check_row_count`] と同じ理由。COPY の場合は生 CopyData
/// バイト量そのものを累算する——[`validate_batch_shape`] のようにデコード後の
/// `path.len() + body.len()` ではなく、デコード前のバイト量を上限判定の対象と
/// することで、レコード分割・エスケープ解決・`Vec<BoundInsert>` への確保より
/// 前にメモリ確保自体を有界に保つ）。戻り値は新しい累計（呼び出し元は次回
/// 呼び出しへそのまま渡す。`validate_raw_sql_len` と同じ設計）。
pub(crate) fn check_running_total(
    running: usize,
    add: usize,
    limits: &BatchLimits,
) -> Result<usize, BatchLimitsError> {
    let new_total = running
        .checked_add(add)
        .ok_or(BatchLimitsError::BatchTotalTooLarge {
            total: usize::MAX,
            max: limits.max_batch_total_bytes,
        })?;
    if new_total > limits.max_batch_total_bytes {
        return Err(BatchLimitsError::BatchTotalTooLarge {
            total: new_total,
            max: limits.max_batch_total_bytes,
        });
    }
    Ok(new_total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> BatchLimits {
        BatchLimits {
            max_files_per_batch: 4,
            max_file_body_bytes: 10,
            max_batch_total_bytes: 30,
            max_batch_chunks: 5,
        }
    }

    #[test]
    fn legacy_env_detection_lists_only_set_old_names() {
        let set = ["VECTOR_DB_BATCH_MAX_CHUNKS", "FANDHE_DB_BATCH_MAX_FILES"];
        let found = legacy_batch_env_vars_where(|n| set.contains(&n));
        assert_eq!(
            found,
            vec![("VECTOR_DB_BATCH_MAX_CHUNKS", "FANDHE_DB_BATCH_MAX_CHUNKS")]
        );
        assert!(legacy_batch_env_vars_where(|_| false).is_empty());
        assert_eq!(legacy_batch_env_vars_where(|_| true).len(), 3);
    }

    #[test]
    fn max_files_validation_bounds() {
        assert!(validate_max_files_per_batch(0).is_err());
        assert_eq!(validate_max_files_per_batch(1), Ok(1));
        assert_eq!(
            validate_max_files_per_batch(MAX_BATCH_MAX_FILES),
            Ok(MAX_BATCH_MAX_FILES)
        );
        assert!(validate_max_files_per_batch(MAX_BATCH_MAX_FILES + 1).is_err());
    }

    #[test]
    fn env_max_files_falls_back_when_invalid_or_out_of_range() {
        assert_eq!(parse_env_max_files(None), DEFAULT_MAX_FILES_PER_BATCH);
        assert_eq!(
            parse_env_max_files(Some("abc")),
            DEFAULT_MAX_FILES_PER_BATCH
        );
        assert_eq!(parse_env_max_files(Some("0")), DEFAULT_MAX_FILES_PER_BATCH);
        assert_eq!(
            parse_env_max_files(Some(&usize::MAX.to_string())),
            DEFAULT_MAX_FILES_PER_BATCH
        );
        assert_eq!(parse_env_max_files(Some(" 100 ")), 100);
    }

    #[test]
    fn budget_at_max_files_does_not_overflow() {
        let l = BatchLimits::default();
        let b = batch_raw_sql_len_budget(l.max_batch_total_bytes, MAX_BATCH_MAX_FILES);
        assert!(b > 0);
    }

    #[test]
    fn shape_accepts_within_all_limits() {
        // 4 ファイル・各本文 5 バイト・path 1 バイト = 合計 24 バイト（上限 30 以内）。
        let files = vec![(1, 5), (1, 5), (1, 5), (1, 5)];
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_one_file_over_count_limit() {
        let files = vec![(1, 1), (1, 1), (1, 1), (1, 1), (1, 1)];
        assert!(matches!(
            validate_batch_shape(&files, &limits()),
            Err(BatchLimitsError::TooManyFiles { count: 5, max: 4 })
        ));
    }

    #[test]
    fn shape_accepts_file_count_exactly_at_limit() {
        let files = vec![(1, 1), (1, 1), (1, 1), (1, 1)];
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_single_file_body_over_limit() {
        let files = vec![(1, 11)];
        assert!(matches!(
            validate_batch_shape(&files, &limits()),
            Err(BatchLimitsError::FileBodyTooLarge {
                index: 0,
                len: 11,
                max: 10
            })
        ));
    }

    #[test]
    fn shape_accepts_single_file_body_exactly_at_limit() {
        let files = vec![(1, 10)];
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_batch_total_over_limit_even_when_each_file_is_within_its_own_limit() {
        // 各ファイルは②の 10 バイト以内だが、合計 33 バイトは③の 30 を超える。
        let files = vec![(1, 10), (1, 10), (1, 10)];
        assert!(matches!(
            validate_batch_shape(&files, &limits()),
            Err(BatchLimitsError::BatchTotalTooLarge { total: 33, max: 30 })
        ));
    }

    #[test]
    fn shape_accepts_batch_total_exactly_at_limit() {
        let files = vec![(1, 10), (1, 9), (1, 8)];
        // 合計 = (1+10) + (1+9) + (1+8) = 30。
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_batch_total_overflow_instead_of_panicking() {
        let files = vec![(usize::MAX, usize::MAX)];
        // 本文サイズ自体は②を通らないため FileBodyTooLarge が先に出る場合もあるが、
        // ここでは②を無効化した上限で③のオーバーフロー処理を確認する。
        let overflow_limits = BatchLimits {
            max_file_body_bytes: usize::MAX,
            ..limits()
        };
        assert!(matches!(
            validate_batch_shape(&files, &overflow_limits),
            Err(BatchLimitsError::BatchTotalTooLarge { .. })
        ));
    }

    #[test]
    fn chunk_total_accepts_up_to_limit() {
        assert!(validate_chunk_total(5, &limits()).is_ok());
    }

    #[test]
    fn chunk_total_rejects_one_over_limit() {
        assert!(matches!(
            validate_chunk_total(6, &limits()),
            Err(BatchLimitsError::TooManyChunks { total: 6, max: 5 })
        ));
    }

    #[test]
    fn wire_code_is_payload_too_large_for_all_variants() {
        assert_eq!(
            BatchLimitsError::TooManyFiles { count: 1, max: 0 }.wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::FileBodyTooLarge {
                index: 0,
                len: 1,
                max: 0
            }
            .wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::BatchTotalTooLarge { total: 1, max: 0 }.wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::TooManyChunks { total: 1, max: 0 }.wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::SqlTextTooLarge {
                index: 0,
                len: 1,
                max: 0
            }
            .wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::BatchSqlTextTooLarge { total: 1, max: 0 }.wire_code(),
            "54000"
        );
    }

    #[test]
    fn raw_sql_len_accepts_within_per_file_budget() {
        // max_batch_total_bytes=30 → 予算 = 2*30 + 4096 = 4156。
        let result = validate_raw_sql_len(0, 100, 0, &limits());
        assert_eq!(result.unwrap(), 100);
    }

    #[test]
    fn raw_sql_len_rejects_single_statement_over_per_file_budget() {
        let budget = raw_sql_len_budget(limits().max_batch_total_bytes);
        assert!(matches!(
            validate_raw_sql_len(2, budget + 1, 0, &limits()),
            Err(BatchLimitsError::SqlTextTooLarge {
                index: 2,
                len,
                max
            }) if len == budget + 1 && max == budget
        ));
    }

    #[test]
    fn raw_sql_len_accepts_exactly_at_per_file_budget() {
        let budget = raw_sql_len_budget(limits().max_batch_total_bytes);
        assert!(validate_raw_sql_len(0, budget, 0, &limits()).is_ok());
    }

    #[test]
    fn raw_sql_len_rejects_when_running_total_exceeds_batch_budget() {
        let l = limits();
        let batch_budget = batch_raw_sql_len_budget(l.max_batch_total_bytes, l.max_files_per_batch);
        let per_file_budget = raw_sql_len_budget(l.max_batch_total_bytes);
        // 1 文単独では per-file 予算内でも、累計が batch 予算を超えれば拒否する。
        let per_file_ok = per_file_budget.min(batch_budget);
        assert!(matches!(
            validate_raw_sql_len(1, per_file_ok, batch_budget, &l),
            Err(BatchLimitsError::BatchSqlTextTooLarge { .. })
        ));
    }

    #[test]
    fn raw_sql_len_batch_budget_includes_per_statement_overhead() {
        // codex P1 追加指摘の回帰テスト（PR #242）: バッチ累計予算に文数分の
        // 構文オーバーヘッドを含めない旧実装なら、後段契約（②③）を満たす
        // 正当な多文バッチでも framing 分だけ解析前に拒否されうる。
        // limits() は max_files_per_batch=4・max_batch_total_bytes=30。
        // 旧実装の batch_budget = 2*30 + 4096 = 4156（文数分のオーバーヘッドを
        // 見込んでいないため、2 文分弱の長さしか積めない）。
        let l = limits();
        let old_wrong_batch_budget = raw_sql_len_budget(l.max_batch_total_bytes);
        // 各文は per-file 予算内（十分小さい）だが、4 文合計では旧実装の
        // batch_budget を超え、4 文分の framing を見込んだ新しい batch_budget
        // でなければ受理できない生テキスト長を積む。
        let per_statement_len = old_wrong_batch_budget / 2;
        let mut running = 0usize;
        for index in 0..l.max_files_per_batch {
            running = validate_raw_sql_len(index, per_statement_len, running, &l)
                .expect("statement within per-file and per-statement-overhead-aware batch budget should be accepted");
        }
        assert_eq!(running, per_statement_len * l.max_files_per_batch);
    }

    #[test]
    fn batch_raw_sql_len_budget_saturates_instead_of_overflowing() {
        assert_eq!(batch_raw_sql_len_budget(usize::MAX, usize::MAX), usize::MAX);
        assert_eq!(batch_raw_sql_len_budget(0, usize::MAX), usize::MAX);
    }

    #[test]
    fn raw_sql_len_accepts_large_path_small_body_within_batch_total() {
        // codex P1・Cursor Bugbot 対応の回帰テスト: path が本文上限よりはるかに
        // 大きく body が小さい正当な入力（後段の正式契約では path に個別上限が
        // なく、path.len() + body.len() <= max_batch_total_bytes であれば
        // validate_batch_shape を通過する）が、max_file_body_bytes だけを基礎に
        // した事前予算では誤って拒否されていたことの回帰テスト（PR #242 対応）。
        let l = BatchLimits {
            max_files_per_batch: 4,
            max_file_body_bytes: 100,
            max_batch_total_bytes: 1_000_000,
            max_batch_chunks: 5,
        };
        let old_wrong_budget = raw_sql_len_budget(l.max_file_body_bytes);
        // path 分の余裕を含む生 SQL 長。旧実装（body 基礎の予算）なら拒否される
        // 大きさだが、path + body は合計上限（③）を満たす正当な入力である。
        let sql_len = old_wrong_budget + 1;
        assert!(validate_raw_sql_len(0, sql_len, 0, &l).is_ok());
    }

    #[test]
    fn raw_sql_len_accumulates_running_total_across_calls() {
        let l = limits();
        let first = validate_raw_sql_len(0, 5, 0, &l).unwrap();
        assert_eq!(first, 5);
        let second = validate_raw_sql_len(1, 7, first, &l).unwrap();
        assert_eq!(second, 12);
    }

    #[test]
    fn raw_sql_len_budget_saturates_instead_of_overflowing() {
        assert_eq!(raw_sql_len_budget(usize::MAX), usize::MAX);
    }
}
