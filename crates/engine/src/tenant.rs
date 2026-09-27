//! 行単位テナント境界の行ストア統合層（TASK-89・対象ビヘイビア: TABLE-9, TABLE-11。
//! TASK-95・対象ビヘイビア: RECOVER-4 の書き込みガード API を追加）。
//!
//! ## `operation_id` 必須化ガードと公開 API の構造（TASK-92・対象ビヘイビア: RECOVER-1）
//!
//! `pub fn` の [`insert_row`]・[`insert_rows`]・[`insert_typed_row`]・[`update_row`]・
//! [`delete_row`] は `operation_id: &OperationId` を必須引数として要求する
//! （codex-review P1 指摘・PR #217 対応。詳細は `recovery::required_op_id`
//! モジュールドキュメント参照）。ガード検証を持たない `pub(crate)` の
//! `*_unchecked` 版はクレート外へ公開しない。
//!
//! `policy.rs::PolicyContext::is_visible` の単一照合パス（CORE-2）へすべての可視性
//! 判定を委譲し、本モジュール独自のテナント比較は持たない（security.md P0）。
//! 提供する API は大きく 2 系統:
//!
//! - 読み取り側（[`visible_rows`]・[`verify_hits`]）: 行ストア（`catalog.rs` のテーブル
//!   スコープ行 API）を安全な上限内で走査し、可視行だけを列挙・検証する統合層。
//!   呼び出し元は主に `tests/tenant_isolation.rs`（TABLE-11 の 200 試行 × 4 テナント
//!   巡回検証）で、独立に期待集合を算出するための参照実装として使う。
//! - 書き込み側（[`insert_row`]・[`update_row`]・[`delete_row`]）: `PolicyContext::is_owner`
//!   （書き込み認可の単一照合パス）による所有権判定を経由してのみ行ストアを変更する
//!   ガード API（RECOVER-4）。`crate::core::EngineCore` の薄い委譲メソッドを経由して
//!   wire 層が DML を行う唯一の入口として設計している。生の UPDATE/DELETE を
//!   `Storage` の公開 API として新設しない（ガードを迂回できる経路を増やさない）。
//!
//! ## 設計記録: テーブル単位の物理分離は本タスクのスコープ外
//!
//! テナント境界は本モジュールが提供する「行単位」の可視性フィルタ（`PolicyContext`
//! 経由）を主軸として MVP を構成し、テナントごとにテーブルを動的構築する物理分離は
//! 実装しない（対象ビヘイビア: TABLE-9。詳細は spec 側のポインタ参照）。将来
//! テーブル単位分離を検討する場合は、本モジュールの可視性フィルタと独立した設計判断
//! として扱うこと。

use redb::{ReadableDatabase, ReadableTable};

use crate::catalog::{
    map_row_table_error, require_table_schema_write, user_rows_table_def, user_rows_table_name,
    validate_identifier, CatalogError,
};
use crate::kernel::SearchHit;
use crate::policy::PolicyContext;
use crate::recovery::content_hash;
use crate::recovery::ledger::{self, LedgerRecordError, LedgerWrite};
use crate::recovery::required_op_id::{LedgerMode, OperationId};
use crate::storage::{
    decode_row_tenant_and_visibility, encode_row, Row, RowInput, Storage, StorageError,
};

/// 1 ページあたりの走査件数（`catalog.rs::Storage::scan_table_page` の内部上限
/// `MAX_SCAN_PAGE_LIMIT` と同じ桁）。
const PAGE_LIMIT: u32 = 10_000;

/// [`visible_rows`] が保持してよい可視行数の上限。無制限 `Vec` 確保を避ける
/// （coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」対応）。
/// テーブル全体の総行数ではなく可視行数を上限にすることで、大量の不可視行を持つ
/// テーブルでも呼び出し元テナントの可視行数だけに比例した確保量に収まる。
const MAX_VISIBLE_ROWS: usize = 100_000;

/// [`visible_rows`] が 1 回の呼び出しで走査してよい総行数（可視・不可視を問わない）の
/// 上限。`MAX_VISIBLE_ROWS` は出力（確保量）を抑えるが、他テナントの不可視行を
/// 大量に格納したテーブルでは出力がほぼ増えないまま `next` が尽きるまで全ページの
/// デコード・`PolicyContext::is_visible` 評価が実行され、計算量 DoS 経路になる
/// （codex-review 指摘・PR #153）。総走査行数にも明示的な上限を設け、超過時は
/// 部分結果を返さず [`TenantError::TooManyRowsScanned`] で fail-closed に拒否する。
///
/// `pub(crate)`（Issue #997）: `sql::parser::MAX_DML_ROW_LIMIT`（CLI で設定可能な
/// DML 行数上限の上限値）が同じ定数を参照し、値のドリフトを防ぐ。
pub(crate) const MAX_SCANNED_ROWS: usize = 1_000_000;

/// [`visible_rows`]・[`verify_hits`] のエラー型。`Display`・`Debug`・
/// `std::error::Error::source` のいずれにもテナント ID・行 id・テーブル名を含めず、
/// 他テナントの存在情報を漏らさない（`rls.rs::RlsError` と同じ契約。security.md P0）。
/// `CatalogError` を内部に保持するが、識別子を含む詳細は外部へ一切露出しない
/// （下記 `Debug`・`Error::source` の手書き実装を参照）。
///
/// `#[non_exhaustive]` は付与しない（[`TenantWriteError`] と同じ判断・同じ理由。
/// Issue #282・`docs/design/error-enum-non-exhaustive-policy.md` 参照）。
pub enum TenantError {
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// 可視行数が [`MAX_VISIBLE_ROWS`] を超えたため、走査を打ち切って fail-closed に
    /// 拒否した（部分的な結果を黙って返さない）。
    TooManyVisibleRows { max: usize },
    /// 総走査行数（可視・不可視を問わない）が [`MAX_SCANNED_ROWS`] を超えたため、
    /// 走査を打ち切って fail-closed に拒否した。大量の不可視行を持つテーブルに対する
    /// 計算量 DoS（出力は増えないまま全ページのデコード・ポリシー評価を強制される
    /// 経路）を防ぐ（security.md テナント境界 P0。codex-review 指摘・PR #153）。
    TooManyRowsScanned { max: usize },
    /// [`verify_hits`] に渡された id が、走査対象テーブルの可視行集合に含まれない
    /// （不可視行・捏造 id のいずれも区別せず本 variant に統一する。他テナントの
    /// 存在情報を漏らさないため。security.md P0）。
    HitOutsideVisibleSet,
}

impl std::fmt::Display for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `CatalogError` の `Display`（`TableNotFound` のテーブル名・`RowNotFound` の
            // 行 ID を含む）をそのまま展開しない。認可前の呼び出し・エラーログ経由で
            // 他テナントの存在情報が漏れるのを防ぐため、識別子・バックエンド詳細を含まない
            // 固定文言に丸める（security.md テナント境界 P0）。原因の詳細は本型の外へは
            // 一切公開しない（`Debug`・`Error::source` も同様にサニタイズ済み。内部診断が
            // 必要な場合は本型を経由しない別経路を用意すること）。
            TenantError::Catalog(_) => write!(f, "tenant boundary catalog error"),
            TenantError::TooManyVisibleRows { max } => {
                write!(f, "too many visible rows: limit={max}")
            }
            TenantError::TooManyRowsScanned { max } => {
                write!(f, "too many rows scanned: limit={max}")
            }
            TenantError::HitOutsideVisibleSet => {
                write!(f, "hit id is outside the policy-visible row set")
            }
        }
    }
}

impl std::fmt::Debug for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `#[derive(Debug)]` は `CatalogError`（`TableNotFound` のテーブル名・
        // `RowNotFound` の行 ID 等）をそのまま展開してしまい、`Display` で
        // 隠した情報がパニック出力・`{:?}` ログ経由で再露出する（security.md
        // テナント境界 P0）。variant 名のみを出力し、内部の識別情報は含めない。
        match self {
            TenantError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantError::TooManyVisibleRows { max } => f
                .debug_struct("TooManyVisibleRows")
                .field("max", max)
                .finish(),
            TenantError::TooManyRowsScanned { max } => f
                .debug_struct("TooManyRowsScanned")
                .field("max", max)
                .finish(),
            TenantError::HitOutsideVisibleSet => f.write_str("HitOutsideVisibleSet"),
        }
    }
}

impl std::error::Error for TenantError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `CatalogError` をそのまま `source()` で返すと、`Display` で固定文言に
        // 丸めた識別情報（テーブル名・行 ID 等）が一般的なエラーチェーン出力
        // （`anyhow` 等の `{:#}` 展開・ログ収集基盤）経由で再露出する
        // （security.md テナント境界 P0）。原因チェーンはここで打ち切り、常に
        // `None` を返す。
        None
    }
}

impl From<CatalogError> for TenantError {
    fn from(e: CatalogError) -> Self {
        TenantError::Catalog(e)
    }
}

/// `table` の全行を上限付きページング（`Storage::scan_table_page`）で走査し、`ctx`
/// （[`PolicyContext::is_visible`]）が可視と判定する行だけを列挙する（TABLE-9・
/// TABLE-11 の参照実装）。
///
/// 可視行数が [`MAX_VISIBLE_ROWS`] を超える場合は部分結果を返さず
/// [`TenantError::TooManyVisibleRows`] で拒否する。総走査行数（可視・不可視を
/// 問わない）が [`MAX_SCANNED_ROWS`] を超える場合も同様に部分結果を返さず
/// [`TenantError::TooManyRowsScanned`] で拒否する（他テナントの不可視行を大量に
/// 格納したテーブルに対する計算量 DoS を防ぐ。security.md テナント境界 P0）。
/// テーブル不存在は [`CatalogError::TableNotFound`] のまま [`TenantError::Catalog`]
/// へ伝播する（存在情報の扱いは呼び出し元の責務）。
pub fn visible_rows(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
) -> Result<Vec<Row>, TenantError> {
    let mut out = Vec::new();
    // カーソルは行ストアの物理キーと同じ `(tenant_id, id)` 形（TABLE-12）。`id` 単独では
    // 再開位置を表現できず、テナントをまたぐ走査で行を取りこぼす。
    let mut after: Option<(String, u64)> = None;
    let mut scanned: usize = 0;
    loop {
        let cursor = after.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (page, next) = storage.scan_table_page(table, cursor, PAGE_LIMIT)?;
        if page.is_empty() && next.is_none() {
            break;
        }
        scanned = scanned.saturating_add(page.len());
        if scanned > MAX_SCANNED_ROWS {
            return Err(TenantError::TooManyRowsScanned {
                max: MAX_SCANNED_ROWS,
            });
        }
        for row in page {
            if ctx.is_visible(&row.tenant_id, row.visibility) {
                if out.len() >= MAX_VISIBLE_ROWS {
                    return Err(TenantError::TooManyVisibleRows {
                        max: MAX_VISIBLE_ROWS,
                    });
                }
                out.push(row);
            }
        }
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(out)
}

/// 検索結果 `hits` が、`table` に対する `ctx` の可視集合へすべて収まって
/// いることを **`(tenant_id, id)` の完全な行キー**で fail-closed に検証する
/// （TABLE-11: 200 試行 × 4 テナント巡回検証の
/// 混入 0 件アサーションを、`EngineCore::search`/`PrefilterIndex::search` の内部実装と
/// 独立した経路で裏付けるためのヘルパ）。
///
/// 1 件でも可視集合外の id があれば、走査を打ち切り即座に
/// [`TenantError::HitOutsideVisibleSet`] を返す。
pub fn verify_hits(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    hits: &[SearchHit],
) -> Result<(), TenantError> {
    let visible = visible_rows(storage, table, ctx)?;
    // 照合キーは行 `id` 単独ではなく完全な行キー `(tenant_id, id)`（対象ビヘイビア:
    // TABLE-12・RLS-9。codex-review P1 指摘・PR #194）。`id` だけの集合で照合すると、
    // 「可視な行（例: 他テナントの `Public` 行）と同じ `id` を持つ不可視行（別テナントの
    // `Private` 行）」由来のヒットを見逃す——`id` は可視集合に存在してしまうため。
    // ヒット側のテナントは `SearchHit::tenant_id`（検索経路が行の帰属として付与した値）
    // を使う。
    let visible_keys: std::collections::HashSet<(&str, u64)> = visible
        .iter()
        .map(|r| (r.tenant_id.as_str(), r.id))
        .collect();
    if hits
        .iter()
        .all(|hit| visible_keys.contains(&(hit.tenant_id.as_str(), hit.id)))
    {
        Ok(())
    } else {
        Err(TenantError::HitOutsideVisibleSet)
    }
}

/// [`insert_row`]・[`update_row`]・[`delete_row`] のエラー型（TASK-95・対象ビヘイビア:
/// RECOVER-4）。`Display`・`Debug`・`std::error::Error::source` のいずれにもテナント ID・
/// 行 id・テーブル名を含めず、他テナントの存在情報を漏らさない（[`TenantError`] と同じ
/// 契約。security.md P0）。
///
/// `#[non_exhaustive]` は付与しない: 本 enum は既に公開済みであり、後付けで
/// `#[non_exhaustive]` を付けると下流の網羅的 `match` がコンパイル不能になる
/// （それ自体が破壊的変更のため、`#[non_exhaustive]` 化で互換性を装うのではなく
/// 付けないままにする。`core.rs::CoreError` の PR #252 判断と同方針。Issue #282・
/// `docs/design/error-enum-non-exhaustive-policy.md` 参照）。variant の追加は
/// `non_exhaustive` 化の有無に関わらず既存の網羅的 `match` を壊す破壊的変更である
/// ことに変わりはなく、PR 本文の Breaking changes 節と `BREAKING CHANGE:` フッタで
/// 明示する。
pub enum TenantWriteError {
    /// 呼び出し元が入力した `RowInput::tenant_id` が `ctx` のテナントと不一致
    /// （クライアント自身の入力に起因するため存在情報を含まない）。他テナント名義の
    /// 新規行の書き込み・自テナント行の他テナントへの付け替え試行の両方がここに入る。
    Forbidden,
    /// UPDATE/DELETE 対象行が不存在、または `ctx` が所有しない行（区別しない。
    /// 存在情報を漏らさないため fail-closed に統一する。security.md P0）。
    NotFound,
    /// INSERT 先 id に既存行がある（所有者を問わず同一 variant。上書きによる他テナント
    /// 行の破壊を遮断しつつ、所有テナントの存在情報を漏らさない）。
    IdConflict,
    /// `operation_id` の省略（句の欠落・明示 `NULL` を含む）。台帳あり構成
    /// （`recovery::required_op_id::LedgerMode::Ledgered`、既定）では書き込み系操作に
    /// `operation_id` の指定を必須とする（TASK-92・対象ビヘイビア: RECOVER-1）。
    /// `crate::core::EngineCore::{insert_row, update_row, delete_row}` が
    /// `crate::tenant::*_unchecked` へ委譲する**前**に `self.ledger_mode` でガードを
    /// 適用し、本モジュールの `pub fn insert_row`/`insert_rows`/`insert_typed_row`/
    /// `update_row`/`delete_row` は `operation_id` を必須引数として要求したうえで
    /// `LedgerMode::Ledgered` で内部ガードするため、いずれの経路でも本 variant が
    /// 実際に返る時点で書き込みトランザクションは未開始（ERR-2: `23502`）。
    MissingOperationId,
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// [`crate::storage`] 側のエンコード/デコードエラー（`RowInput` の入力検証失敗等）。
    Storage(StorageError),
    /// `operation_id` 台帳（`crate::recovery::ledger`）テーブルの読み書きで検出した
    /// 内部エラー（未知フォーマットバージョンの混入・redb バックエンド障害）。
    /// `Storage(StorageError::Codec)` と型を分ける（Cursor Bugbot 指摘・PR #226）:
    /// 台帳の破損はクライアントが送った行データとは無関係のサーバー内部事象であり、
    /// `sql::exec::execute_insert` の呼び出し元マッピングが `StorageError::Codec` を
    /// 「行データ不正（`22000`）」として丸めてしまうと、台帳破損を「送った行が不正」
    /// という誤ったクライアント向けエラーへ変換してしまう。台帳エラーは常に
    /// `wire_code` `XX000`（内部事象）に固定し、クライアントへ再試行を促す誤情報を
    /// 出さない（fail-closed）。
    LedgerCorrupted(StorageError),
    /// 台帳（TASK-93）に記録済みの `operation_id` へ、**内容が一致する**書き込みが
    /// 再送された（TASK-101・対象ビヘイビア: RECOVER-10。TASK-94・RECOVER-3 の
    /// 重複拒否契約を包含する）。commit 済み確定の根拠として扱ってよく、`23505`
    /// （`UniqueViolation` と同じ分類。error_format.rs のコメント参照）へ写像する。
    /// 行キー衝突（[`TenantWriteError::IdConflict`]）とは別 variant にすることで、
    /// クライアントが「先行実行が commit 済み」（RECOVER-7 が使う判定）を行キー衝突と
    /// 取り違えない固定文言を返せるようにする。
    DuplicateOperationId,
    /// 台帳に記録済みの `operation_id` へ、**内容が異なる**書き込みが再送された、
    /// または内容一致を証明できない旧フォーマット（v1）エントリへ再送された
    /// （TASK-101・RECOVER-10）。commit 済み確定の根拠にしない fail-closed 判定
    /// （`22023`）。行内容・テナント・他テナントの存在情報は含まない。
    OperationIdContentMismatch,
    /// `RETURNING` 句（Issue #873・SQL-21）向けの捕捉行投影（呼び出し元が
    /// [`delete_row_impl`] の `project` コールバックへ渡す処理。実体は
    /// `sql::returning::project_row` の `SqlSurfaceError::Internal`）が失敗した
    /// （型不整合・実装バグ検出等、untrusted 入力起因ではない事象）。
    /// **write トランザクションが commit される前**にこの `Err` を返すことで
    /// `write_txn` を drop・abort させ、「削除は永続化されたのにエラー応答」
    /// という commit 成功境界違反（codex-review P1 指摘・PR #991）を防ぐ。
    /// `XX000`（内部事象）へ写像する。
    ReturningProjectionFailed(String),
    /// 上記と同じ commit 前 abort 契約だが、原因が `RETURNING` 結果セットの
    /// バイト量上限超過（`sql::returning::MAX_RETURNING_RESULT_BYTES`）である
    /// 場合の専用 variant。`54000`（`PayloadTooLarge`）へ写像し、クライアントが
    /// 内部事象（`XX000`）と取り違えないようにする。
    ReturningProjectionTooLarge(String),
    /// `RETURNING` 句（Issue #873・SQL-21）向けに削除直前の**既存**行を捕捉する際の
    /// デコード失敗（[`delete_row_impl`] が `storage::decode_row`／
    /// `row_codec::decode_scalar_columns` を呼ぶ箇所）。対象は今回のクライアント入力
    /// ではなく「既に永続化済みの行」であるため、失敗原因はストレージ破損・過去の
    /// エンコード不整合等のサーバー内部事象であり、クライアントが送った値の不正では
    /// ない（codex-review P1・Bugbot 指摘・PR #991: 汎用の `Storage`/`Catalog(Invalid)`
    /// を共用すると `map_insert_write_error` がクライアント入力エラー `22000` へ
    /// 丸めてしまい、クライアントに誤った再試行判断を誘発する）。専用 variant として
    /// 分離し `XX000`（内部事象）へ固定する。
    CapturedRowDecodeFailed(String),
    /// 述語つき `UPDATE`／`DELETE ... WHERE`（[`enumerate_dml_candidates`]）の
    /// 候補列挙が、対象テナントの物理キー領域（`(tenant, 0)` からの `range`
    /// 走査。テナント境界を跨いだ時点で打ち切り、他テナント領域には触れない）
    /// を走査した総行数（対象テナント所有行のみを計数。可視・不可視は問わない）
    /// で [`MAX_SCANNED_ROWS`] を超えた（codex-review P1 指摘・PR #993 系・
    /// Issue #871。一致行数の上限（[`PredicateDmlOutcome::LimitExceeded`]）とは
    /// 独立: 一致しない述語では一致件数上限に到達しないまま対象テナント名前空間
    /// 内で任意規模の走査が繰り返せてしまう経路を塞ぐ。[`visible_rows`] の
    /// `TooManyRowsScanned` と同じ「部分結果を返さず fail-closed に拒否する」
    /// 判断。この上限は他テナントのデータ量に一切依存しない。`write_txn` は
    /// commit せず破棄する（行・台帳とも痕跡ゼロ）ため `54000`
    /// （`PayloadTooLarge`）へ写像する。
    TooManyRowsScanned,
    /// `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。いずれも TABLE-16・
    /// TASK-204）を宣言したテーブルへの書き込みがテナント内一意性制約に違反した
    /// （`constraint::enforce_unique_keys_in_txn`）。
    /// 行キー衝突（[`TenantWriteError::IdConflict`]。物理キー `(tenant_id, id)`
    /// の衝突）とは原因が異なる別 variant だが、`ErrorClass::UniqueViolation`
    /// （`23505`）は共有する。`Display`／`Debug` はいずれもキー値・行 id・
    /// テナント名・テーブル名を含まない固定文言（security.md P0「エラー・ログ
    /// 経由で他テナントのデータ・存在情報を漏らさない」）。
    UniqueViolation,
    /// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）が宣言する述語を、書き込もうと
    /// した行の値が満たさない。`constraint` は制約名のみ（行の値・id・テナントは
    /// 含めない。security.md P0）。一意性制約と同じ単一の検査点
    /// （`constraint::enforce_row_constraints_in_txn`）が、台帳照合・行の書き込みの
    /// **後**・テーブル世代 bump／commit の**前**に返す（`write_txn` は commit
    /// されないため、行・台帳とも痕跡が残らない）。
    CheckViolation { constraint: String },
    /// `CHECK` 制約の述語評価自体が失敗した（`vec_div` の 0 除算等、値依存で
    /// 発生しうる）。違反（`CheckViolation`）に丸めず `XX000`（内部事象）として
    /// 書き込みを拒否する（fail-closed。値・詳細はクライアントへ渡さない）。
    CheckEvaluationFailed,
    /// `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）の参照整合性違反
    /// （`23503`）: 参照元の書き込みで参照先の値の組が同一テナント内に存在しない、
    /// または参照先の削除・更新・TRUNCATE・置換で参照元の行が残る。一意性制約と
    /// 同じ単一の検査点（`constraint` モジュール）が台帳照合・行の書き込みの**後**・
    /// commit の**前**に返す。`Display`／`Debug` はキー値・行 id・テナント名・
    /// テーブル名・参照先の有無の理由（不在なのか他テナント所有なのか）を一切
    /// 含まない固定文言（RLS-9・RLS-10 (c)。security.md P0）。
    ForeignKeyViolation,
    /// `FOREIGN KEY` の参照アクション（`CASCADE`／`SET NULL`／`SET DEFAULT`。
    /// Issue #1076）の連鎖が、実装既定の上限（連鎖の深さ・1 文あたりの連鎖対象
    /// 行数。いずれも spec 由来ではない実装値。`crate::constraint` モジュール
    /// ドキュメント参照）を超えた。`54000`（`PayloadTooLarge`）へ写像し、
    /// `write_txn` は commit しない（行・台帳とも痕跡ゼロ）。`Display`／`Debug`
    /// は上限値のみを含む固定文言（テナント・値・テーブル名は含めない。
    /// RLS-9・security.md P0）。
    ReferentialActionLimitExceeded,
    /// 明示トランザクション（SQL-31・TASK-221）の単一ライタ占有により、書き込み
    /// トランザクションの取得（[`Storage::begin_write_txn`]）がロック待ちの上限を
    /// 超過した（`55P03`）。`Storage(StorageError::WriteLockTimeout)` へ一般化せず
    /// 専用 variant にすることで、他の内部ストレージエラー（`XX000`）と
    /// 取り違えないようにする。
    WriteLockTimeout,
}

/// [`Storage::begin_write_txn`]（choke point。SQL-31・TASK-221 の単一ライタ占有に
/// より `55P03` を返しうる）が返す `StorageError` を `TenantWriteError` へ写像する。
/// `TenantWriteError::from(StorageError)` は汎用の `Storage(e)` へ包むだけで
/// ロック待ちタイムアウトの意味を保てないため、`*_unchecked` 各関数はこの専用関数を
/// 経由する。
fn convert_write_txn_err(e: StorageError) -> TenantWriteError {
    match e {
        StorageError::WriteLockTimeout | StorageError::WriteTxnHeldByCurrentSession => {
            TenantWriteError::WriteLockTimeout
        }
        other => TenantWriteError::Storage(other),
    }
}

/// 明示トランザクション（SQL-31・TASK-221）と autocommit の間で書き込み本体を
/// 共有するための入口。`sql::transaction::SessionTransaction` が `Active` の間は
/// `InTxn` で共有 `redb::WriteTransaction` を渡し、通常時（autocommit）は
/// `Autocommit` として [`Storage::begin_write_txn`] 経由で新規トランザクションを
/// 開いて 1 文ごとに commit する。
///
/// 現時点で `InTxn` を受理するのは [`insert_row_unchecked`]・
/// [`insert_rows_unchecked`]・[`insert_typed_row_unchecked`]・
/// [`truncate_table_unchecked`] の 4 経路のみ（`docs/design/explicit-transaction.md`
/// 参照。他の書き込み系 API は明示トランザクション内では `0A000` で拒否され、
/// 本 enum に到達しない）。
pub(crate) enum WriteTarget<'a> {
    Autocommit(&'a Storage),
    InTxn(&'a redb::WriteTransaction),
}

impl<'a> WriteTarget<'a> {
    /// `f` を書き込みトランザクションのスコープで実行する。`Autocommit` は
    /// [`Storage::begin_write_txn`] choke point で新規トランザクションを開き、
    /// `f` が `Ok` を返した場合のみ [`crate::recovery::commit_boundary::commit`]
    /// で commit する（`f` が `Err` を返せば `write_txn` は drop され abort する。
    /// 既存 `*_unchecked` 関数群と同じ「TOCTOU なし・部分書き込みを残さない」
    /// 契約を保つ）。`InTxn` は commit を一切呼ばず、`f` の結果をそのまま返す
    /// （commit／abort は `COMMIT`／`ROLLBACK` 文が一括で行う。SQL-31・RECOVER-12）。
    ///
    /// `f` が [`TxnEffect::NoOp`] を返した場合（空バッチ等、何も書き込まなかった
    /// 場合）、`Autocommit` は commit せず abort して閉じる。commit すると
    /// グローバル世代が進み、世代で失効するキャッシュを無駄に捨ててしまうため
    /// （main の `insert_rows_unchecked` が空バッチで commit 前に早期 return
    /// していた挙動を保つ。PR #1041 レビュー指摘）。
    fn with_txn<T>(
        &self,
        f: impl FnOnce(&redb::WriteTransaction) -> Result<(T, TxnEffect), TenantWriteError>,
    ) -> Result<T, TenantWriteError> {
        match self {
            WriteTarget::Autocommit(storage) => {
                let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
                let (value, effect) = f(&write_txn)?;
                match effect {
                    TxnEffect::Wrote => crate::recovery::commit_boundary::commit(write_txn)?,
                    // 何も書いていないため drop（abort）で閉じ、世代を進めない。
                    TxnEffect::NoOp => drop(write_txn),
                }
                Ok(value)
            }
            WriteTarget::InTxn(write_txn) => f(write_txn).map(|(value, _)| value),
        }
    }

    /// この `target` の下で `FOREIGN KEY` の文単位検査をどこまで行うか
    /// （`constraint::FkCheckMode`。TABLE-17・TASK-205、Issue #1077）。
    /// `Autocommit`（1 文＝1 トランザクション）は `INITIALLY DEFERRED` の
    /// 宣言でも必ず文単位で検査する（`All`）。`InTxn`（明示トランザクション。
    /// 上記 4 経路のみ）は `INITIALLY DEFERRED` の FK を文単位検査から除外し
    /// （`ImmediateOnly`）、COMMIT 時
    /// （`constraint::enforce_deferred_foreign_keys_in_txn`）へ先送りする。
    fn fk_check_mode(&self) -> crate::constraint::FkCheckMode {
        match self {
            WriteTarget::Autocommit(_) => crate::constraint::FkCheckMode::All,
            WriteTarget::InTxn(_) => crate::constraint::FkCheckMode::ImmediateOnly,
        }
    }
}

/// [`WriteTarget::with_txn`] に渡すクロージャが、書き込みを行ったかどうかを
/// 返すための印。`NoOp` のとき autocommit 経路は commit しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxnEffect {
    Wrote,
    NoOp,
}

impl TenantWriteError {
    /// SQLSTATE 風 `wire_code`（coding-rust.md「エラー型は SQLSTATE 風 wire_code の設計に
    /// 従う」）。対象ビヘイビア: RECOVER-4・ERR-2（`docs/spec/04-behavior/error-format.md`
    /// をポインタ参照。写像の具体値・採用理由は spec 側の管理事項であり、本コメントへは
    /// 転記しない。spec-confidentiality.md 参照）。TASK-152 で単一真実源化した
    /// [`crate::error_format::ErrorClass`] へ委譲する（既存の返値は 1 つも変えない）。
    pub fn wire_code(&self) -> &'static str {
        crate::error_format::ClassifiedError::wire_code(self)
    }
}

/// TASK-152（ERR-2）: `wire_code` 写像の単一真実源 [`crate::error_format::ErrorClass`]
/// へ委譲する。variant → `ErrorClass` の対応は既存 `wire_code()` の返値と 1:1 で
/// 一致させ、委譲化で応答コードを変えない。
impl crate::error_format::ClassifiedError for TenantWriteError {
    fn error_class(&self) -> crate::error_format::ErrorClass {
        use crate::error_format::ErrorClass;
        match self {
            TenantWriteError::Forbidden => ErrorClass::ForbiddenTenantMismatch,
            TenantWriteError::NotFound => ErrorClass::RowNotFound,
            TenantWriteError::IdConflict => ErrorClass::UniqueViolation,
            TenantWriteError::DuplicateOperationId => ErrorClass::UniqueViolation,
            TenantWriteError::MissingOperationId => ErrorClass::MissingOperationId,
            TenantWriteError::Catalog(_)
            | TenantWriteError::Storage(_)
            | TenantWriteError::LedgerCorrupted(_) => ErrorClass::InternalError,
            TenantWriteError::OperationIdContentMismatch => ErrorClass::OperationIdContentMismatch,
            TenantWriteError::ReturningProjectionFailed(_) => ErrorClass::InternalError,
            TenantWriteError::ReturningProjectionTooLarge(_) => ErrorClass::PayloadTooLarge,
            TenantWriteError::CapturedRowDecodeFailed(_) => ErrorClass::InternalError,
            TenantWriteError::TooManyRowsScanned => ErrorClass::PayloadTooLarge,
            TenantWriteError::UniqueViolation => ErrorClass::UniqueViolation,
            TenantWriteError::CheckViolation { .. } => ErrorClass::CheckViolation,
            TenantWriteError::CheckEvaluationFailed => ErrorClass::InternalError,
            TenantWriteError::ForeignKeyViolation => ErrorClass::ForeignKeyViolation,
            TenantWriteError::ReferentialActionLimitExceeded => ErrorClass::PayloadTooLarge,
            TenantWriteError::WriteLockTimeout => ErrorClass::LockNotAvailable,
        }
    }

    /// `Display` は既にテナント境界の秘匿契約（テナント ID・行 id・テーブル名を
    /// 含めない。上記 struct doc 参照）を満たしているため、そのまま返す。
    fn client_message(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for TenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TenantWriteError::Forbidden => {
                write!(f, "tenant write forbidden: not the row owner")
            }
            TenantWriteError::NotFound => write!(f, "tenant write target row not found"),
            TenantWriteError::IdConflict => write!(f, "tenant write id conflict"),
            TenantWriteError::MissingOperationId => write!(f, "missing operation_id"),
            // `CatalogError`/`StorageError` の `Display` をそのまま展開しない（`TenantError`
            // と同じ理由。security.md テナント境界 P0）。
            TenantWriteError::Catalog(_) => write!(f, "tenant write catalog error"),
            TenantWriteError::Storage(_) => write!(f, "tenant write storage error"),
            TenantWriteError::LedgerCorrupted(_) => write!(f, "tenant write ledger error"),
            // 行キー衝突（`IdConflict`）とは別の固定文言にすることで、クライアントが
            // 「`operation_id` の重複拒否＝先行実行が commit 済み」（RECOVER-7 が使う
            // 判定）を行キー衝突と取り違えないようにする（TASK-94・RECOVER-3・
            // TASK-101・RECOVER-10）。
            TenantWriteError::DuplicateOperationId => {
                write!(f, "operation_id already recorded with the same content")
            }
            TenantWriteError::OperationIdContentMismatch => {
                write!(f, "operation_id already recorded with different content")
            }
            TenantWriteError::ReturningProjectionFailed(_) => {
                write!(f, "tenant write returning projection failed")
            }
            TenantWriteError::ReturningProjectionTooLarge(_) => {
                write!(f, "tenant write returning projection exceeds capacity")
            }
            TenantWriteError::CapturedRowDecodeFailed(_) => {
                write!(f, "tenant write captured row decode failed")
            }
            TenantWriteError::TooManyRowsScanned => {
                write!(f, "too many rows scanned: limit={MAX_SCANNED_ROWS}")
            }
            TenantWriteError::UniqueViolation => write!(f, "unique constraint violation"),
            // 制約名のみを含む固定文言（行の値・id・テナントは含めない。
            // security.md P0）。
            TenantWriteError::CheckViolation { constraint } => {
                write!(f, "new row violates check constraint {constraint:?}")
            }
            TenantWriteError::CheckEvaluationFailed => {
                write!(f, "check constraint evaluation failed")
            }
            TenantWriteError::ForeignKeyViolation => {
                write!(f, "foreign key constraint violation")
            }
            TenantWriteError::ReferentialActionLimitExceeded => {
                write!(
                    f,
                    "referential action limit exceeded: depth={}, rows={}",
                    crate::constraint::MAX_REFERENTIAL_ACTION_DEPTH,
                    crate::constraint::MAX_REFERENTIAL_ACTION_ROWS
                )
            }
            TenantWriteError::WriteLockTimeout => {
                write!(f, "write lock not available: timed out waiting for writer")
            }
        }
    }
}

impl std::fmt::Debug for TenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `#[derive(Debug)]` は内部の `CatalogError`/`StorageError` をそのまま展開し、
        // `Display` で隠した情報がパニック出力・`{:?}` ログ経由で再露出する
        // （security.md テナント境界 P0）。variant 名のみを出力する。
        match self {
            TenantWriteError::Forbidden => f.write_str("Forbidden"),
            TenantWriteError::NotFound => f.write_str("NotFound"),
            TenantWriteError::IdConflict => f.write_str("IdConflict"),
            TenantWriteError::DuplicateOperationId => f.write_str("DuplicateOperationId"),
            TenantWriteError::MissingOperationId => f.write_str("MissingOperationId"),
            TenantWriteError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantWriteError::Storage(_) => f.write_str("Storage(<redacted>)"),
            TenantWriteError::LedgerCorrupted(_) => f.write_str("LedgerCorrupted(<redacted>)"),
            TenantWriteError::OperationIdContentMismatch => {
                f.write_str("OperationIdContentMismatch")
            }
            TenantWriteError::ReturningProjectionFailed(_) => {
                f.write_str("ReturningProjectionFailed(<redacted>)")
            }
            TenantWriteError::ReturningProjectionTooLarge(_) => {
                f.write_str("ReturningProjectionTooLarge(<redacted>)")
            }
            TenantWriteError::CapturedRowDecodeFailed(_) => {
                f.write_str("CapturedRowDecodeFailed(<redacted>)")
            }
            TenantWriteError::TooManyRowsScanned => f.write_str("TooManyRowsScanned"),
            TenantWriteError::UniqueViolation => f.write_str("UniqueViolation"),
            TenantWriteError::CheckViolation { .. } => f.write_str("CheckViolation(<redacted>)"),
            TenantWriteError::CheckEvaluationFailed => f.write_str("CheckEvaluationFailed"),
            TenantWriteError::ForeignKeyViolation => f.write_str("ForeignKeyViolation"),
            TenantWriteError::ReferentialActionLimitExceeded => {
                f.write_str("ReferentialActionLimitExceeded")
            }
            TenantWriteError::WriteLockTimeout => f.write_str("WriteLockTimeout"),
        }
    }
}

impl std::error::Error for TenantWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `TenantError::source` と同じ理由で原因チェーンをここで打ち切る
        // （security.md テナント境界 P0）。
        None
    }
}

impl From<CatalogError> for TenantWriteError {
    fn from(e: CatalogError) -> Self {
        TenantWriteError::Catalog(e)
    }
}

impl From<StorageError> for TenantWriteError {
    fn from(e: StorageError) -> Self {
        TenantWriteError::Storage(e)
    }
}

/// `row_codec` の構造検証エラーを `TenantWriteError` へ写像する
/// （`constraint::enforce_unique_keys_in_txn` が `row_codec::scan_scalar_columns_masked`
/// の失敗を `?` で自然に変換できるようにする）。既存の
/// `crate::row_codec::RowCodecError -> CatalogError::Invalid -> TenantWriteError::Catalog`
/// という手動変換パターン（`upsert_typed_rows_unchecked` 等）と最終的に
/// 同じ分類（`22000`）へ落ちる。
impl From<crate::row_codec::RowCodecError> for TenantWriteError {
    fn from(e: crate::row_codec::RowCodecError) -> Self {
        TenantWriteError::Catalog(CatalogError::Invalid(e.to_string()))
    }
}

/// `ledger::record_in_txn`（TASK-101・RECOVER-10）の結果を `TenantWriteError` へ写像
/// する。呼び出し元 6 箇所（本ファイル `*_unchecked`）が `?` で自然に変換できるように
/// する。
impl From<LedgerRecordError> for TenantWriteError {
    fn from(e: LedgerRecordError) -> Self {
        match e {
            LedgerRecordError::Corrupted(storage_err) => {
                TenantWriteError::LedgerCorrupted(storage_err)
            }
            LedgerRecordError::Duplicate => TenantWriteError::DuplicateOperationId,
            LedgerRecordError::ContentMismatch => TenantWriteError::OperationIdContentMismatch,
        }
    }
}

/// 行ストア（`user_rows/{table_name}`）への一意挿入ヘルパ（TABLE-12・TASK-130・
/// PR #194 の申し送り対応）。[`insert_row_unchecked`]・[`insert_rows_unchecked`]・
/// [`insert_typed_row_unchecked`] が共有する唯一の実装。
///
/// 旧来は `get` で存在確認してから `insert` する 2 回の B-tree 探索だったが、
/// `redb::Table::insert` が上書き前の旧値を `Option<AccessGuard>` として返す性質を
/// 使い、`insert` 一回の走査結果だけで存在判定する（`get` を省略）。返る旧値は
/// 中身を読まず即座に破棄する（存在の有無だけが関心事）。
///
/// # 呼び出し元が守るべき前提（Err 時は commit しない）
///
/// `Some` を返した時点で該当キーには **新しい値がすでに書き込まれている**（redb の
/// `insert` は探索と書き込みを同一パスで行うため、後から取り消す API はない）。
/// この上書きを外部から観測させないのは、本関数の `Err` を受け取った呼び出し元が
/// 属する write トランザクションを **commit せずに drop（abort）する**という契約に
/// よる（`redb::WriteTransaction` の drop 契約。[`insert_row_unchecked`] 等はいずれも
/// 自身が `begin_write` した txn をこの関数の外側でだけ commit するため、この契約を
/// 満たす）。将来の呼び出し元を追加する場合もこの前提を破らないこと。
///
/// キーは呼び出し元がサーバー側導出テナント（`ctx.tenant_id()`）で組み立てる
/// （本関数はキー生成に関与しない）。他テナントの行キーへ触れる経路を持たないため、
/// 他テナント行の有無で分岐する処理は本関数にも存在しない（RLS-9・fail-closed）。
fn insert_unique_row(
    row_table: &mut redb::Table<'_, (&'static str, u64), &'static [u8]>,
    key: (&str, u64),
    encoded: &[u8],
) -> Result<(), TenantWriteError> {
    if row_table
        .insert(key, encoded)
        .map_err(CatalogError::from)?
        .is_some()
    {
        return Err(TenantWriteError::IdConflict);
    }
    Ok(())
}

/// `table` へ新規行を 1 件挿入する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// `row.tenant_id` が `ctx` のテナントと不一致なら
/// [`TenantWriteError::Forbidden`]（他テナント名義での新規行書き込み・テナント
/// 付け替えの試行を遮断。判定は [`PolicyContext::is_owner`] の単一照合パス経由）。
///
/// 重複検出のスコープ（対象ビヘイビア: TABLE-12・RLS-9。codex-review P0 指摘・PR #194）:
/// 行ストアの物理キーは `(tenant_id, id)` で名前空間化されており、既存行の照会は
/// **サーバー側導出テナント（`ctx.tenant_id()`）の名前空間内だけ**を対象とする
/// （クライアント自己申告の `row.tenant_id` はキー構築に用いない）。したがって
/// 同一テナント内の重複のみ [`TenantWriteError::IdConflict`]（`23505`）となり、
/// 他テナントが同じ `id` を保持していても本経路は通常どおり成功する。他テナント行の
/// 有無で分岐する処理を一切持たないため、応答（成否・`wire_code`・文言）からも
/// 実行経路の分岐からも他テナントの存在情報を観測できない（fail-closed）。
///
/// スキーマ取得・次元検証・所有権判定・書き込みを単一の write トランザクション内で
/// 行い、失敗時は commit せずトランザクションを破棄する（`redb::WriteTransaction` の
/// drop 契約により abort。判定と書き込みの間に TOCTOU を作らない。redb は単一
/// ライタで書き込みを直列化する）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`insert_row_unchecked`] へ委譲する（TASK-92・対象ビヘイビア: RECOVER-1・
/// codex-review P1 指摘・PR #217）。本関数はモジュール冒頭ドキュメントの「公開 API」
/// 層であり、`operation_id` を省略できる経路を型で塞ぐ。
pub fn insert_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    insert_row_unchecked(
        WriteTarget::Autocommit(storage),
        table,
        ctx,
        id,
        row,
        ledger_write,
    )
}

/// [`insert_row`] のガードなし実体（`pub(crate)`。TASK-92・RECOVER-1）。
/// `operation_id` 必須化ガードを持たないため、クレート外から直接呼べない
/// （`pub(crate)` によりガードを迂回できる経路を閉じる）。呼び出し元は
/// [`insert_row`]（本モジュール内でガード済み）と
/// `crate::core::EngineCore::insert_row`（`self.ledger_mode` でガード済み）の 2 か所。
///
/// `ledger`（TASK-93・対象ビヘイビア: RECOVER-2）: 行書き込みと**同一の write
/// トランザクション**内で台帳へ追記する（順序: スキーマ取得 → 台帳追記 → 行書き込み →
/// commit）。台帳を先に触っておくことで、行側の `IdConflict` によりトランザクションが
/// drop された場合に台帳も一緒に破棄される（原子性）ことを結合テストで直接検証できる
/// （`recovery::ledger` モジュールドキュメント参照）。
pub(crate) fn insert_row_unchecked(
    target: WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    ledger_write: LedgerWrite<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    // ストレージへ触れる前に、クライアント自己申告の `tenant_id` を ctx と照合する
    // （security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    if !ctx.is_owner(row.tenant_id) {
        return Err(TenantWriteError::Forbidden);
    }
    // 本体は [`WriteTarget::with_txn`] が choke point の
    // autocommit（1 文ごとに commit）／明示トランザクション（呼び出し元の
    // `COMMIT`/`ROLLBACK` まで持ち越し）のいずれとも共有する（SQL-31・TASK-221）。
    target.with_txn(|write_txn| {
        let schema = require_table_schema_write(write_txn, table)?;
        schema.validate_row_embedding_dim(row.embedding.len())?;
        // エンコードは 1 回のみ（Issue #397）: 以前は台帳ハッシュ計算用と redb 書き込み用で
        // それぞれ `encode_row` していた二重実行を排除し、ここで計算した結果を
        // `content_hash::for_insert_encoded` と `insert_unique_row` の双方で共有する。
        let encoded = encode_row(row)?;
        // 台帳照合用ハッシュ（TASK-101・RECOVER-10）はクライアント要求由来の内容
        // （id・行データ）のみから計算する（DB 状態に依存しない決定性の担保。
        // `content_hash` モジュールドキュメント参照）。同一 write トランザクション内で
        // 即座に判定する（TOCTOU なし。redb 単一ライタ直列化により、この
        // get→insert→判定がそのまま「トランザクション内再確認」になる）。`Err` の場合は
        // 行の書き込みへ進まず、autocommit なら `write_txn` が commit されない
        // （[`WriteTarget::with_txn`] が drop → abort）ため台帳追記も破棄され、
        // 部分書き込みが残らない（fail-closed。TASK-94・RECOVER-3 の原子性契約を
        // 包含する。明示トランザクション中は呼び出し元の文実行が失敗した時点で
        // トランザクション全体を abort し `Failed` へ遷移させる契約
        // （`sql::transaction` 参照）のため、ここでも部分書き込みは残らない）。
        let content_hash = content_hash::for_insert_encoded(id, &encoded)?;
        ledger::record_in_txn(
            write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // 物理キーはサーバー側導出テナントで名前空間化する（TABLE-12・RLS-9）。
        let key = (ctx.tenant_id(), id);
        insert_unique_row(&mut row_table, key, encoded.as_slice())?;
        // `CHECK` 制約（Issue #906）と `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905）の
        // テナント内一意性制約（いずれも TABLE-16・TASK-204）は、
        // 台帳照合（上記 `ledger::record_in_txn`）と行の書き込み（`insert_unique_row`）が
        // 済んだ後・テーブル世代 bump／commit の前に検査する（RECOVER-12。判定順序の
        // 詳細は `constraint.rs` モジュールドキュメント参照）。検査前に `row_table`
        // ハンドルを drop し、同一 `write_txn` 内で読み取り専用の検査を行う。
        drop(row_table);
        crate::constraint::enforce_row_constraints_in_txn(
            write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            &[id],
            target.fk_check_mode(),
        )?;
        crate::catalog::bump_table_generation_in_txn(write_txn, table)?;
        Ok(((), TxnEffect::Wrote))
    })
}

/// `table` へ複数行をまとめて挿入する（[`insert_row`] のバッチ版。TASK-95・
/// 対象ビヘイビア: RECOVER-4, TABLE-12, RLS-9）。
///
/// 認可・重複検出の契約は [`insert_row`] と同一で、バッチ全体を単一の write
/// トランザクションで処理する（1 件でも拒否されれば commit せず全体が未反映になる。
/// `redb::WriteTransaction` の drop 契約）。
///
/// 存在確認は行ごとに `get` → `insert` の 2 回 B-tree 探索する旧実装ではなく、
/// [`insert_unique_row`]（`insert` の戻り値で既存行の有無を判定）を使い 1 回に
/// 削減している（TASK-130・PR #194 の申し送り対応）。
///
/// - `row.tenant_id` が `ctx` と不一致な行が 1 件でもあれば [`TenantWriteError::Forbidden`]
///   （ストレージへ触れる前に全件を検査する）
/// - 物理キーは `(ctx.tenant_id(), id)`（TABLE-12）。既存行との衝突、および
///   **同一バッチ内の id 重複**はいずれも [`TenantWriteError::IdConflict`]。後者を
///   検出しないと、バッチ内の後勝ちで先行行が黙って上書きされ、[`insert_row`] が
///   守っている「既存行を上書きしない」契約をバッチ経由で迂回できてしまう
/// - 他テナントが同じ `id` を保持していても成功する（別キーのため。RLS-9）
///
/// 空バッチはテーブル存在確認のみを行い、世代を進めずに成功する
/// （`catalog.rs::Storage::insert_rows_into_table` と同じ扱い。無変更コミットで
/// 既存インデックスを不要に失効させない）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`insert_rows_unchecked`] へ委譲する（[`insert_row`] と同じ設計。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217。バッチ経路にガード付き公開入口が
/// 存在しなかった点も本対応で塞ぐ）。
pub fn insert_rows(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    rows: &[(u64, RowInput<'_>)],
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    insert_rows_unchecked(
        WriteTarget::Autocommit(storage),
        table,
        ctx,
        rows,
        ledger_write,
    )
}

/// [`insert_rows`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計。呼び出し元は本モジュール内の [`insert_rows`] のみ）。
///
/// `ledger`（TASK-93・RECOVER-2）: 空バッチは台帳も書かず世代も進めない現行方針を
/// 維持する（[`insert_row_unchecked`] のドキュメント参照。順序はスキーマ取得 →
/// 空バッチ早期 return → 台帳追記 → 行書き込み → commit）。
pub(crate) fn insert_rows_unchecked(
    target: WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    rows: &[(u64, RowInput<'_>)],
    ledger_write: LedgerWrite<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    // ストレージへ触れる前に、クライアント自己申告の `tenant_id` を全件検査する
    // （security.md P0。[`insert_row`] と同じ単一照合パス `PolicyContext::is_owner`）。
    if rows.iter().any(|(_, row)| !ctx.is_owner(row.tenant_id)) {
        return Err(TenantWriteError::Forbidden);
    }
    // バッチ内の id 重複検出（上記ドキュメント参照）。件数は呼び出し元のスライス長で
    // 上限が決まるため、確保はフォールブルにする（無制限 `with_capacity` を使わない。
    // coding-rust.md）。
    let mut seen_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    seen_ids.try_reserve(rows.len()).map_err(|_| {
        TenantWriteError::Storage(StorageError::Codec(
            "failed to reserve batch id set".to_string(),
        ))
    })?;
    for (id, _) in rows {
        if !seen_ids.insert(*id) {
            return Err(TenantWriteError::IdConflict);
        }
    }

    target.with_txn(|write_txn| {
        let schema = require_table_schema_write(write_txn, table)?;
        if rows.is_empty() {
            // 空バッチは書き込みを一切行わない。`NoOp` を返し、autocommit 経路では
            // commit せずに abort させる（グローバル世代を進めない。main と同じ挙動）。
            return Ok(((), TxnEffect::NoOp));
        }
        // エンコードは行ごとに 1 回のみ（Issue #397）: 台帳ハッシュ計算用
        // （`content_hash::for_insert_batch` 内部）と redb 書き込み用で
        // `encode_row` を二重実行しない。要求記載順のまま事前に全行をエンコードし、
        // その結果をハッシュ計算・行書き込みの双方で共有する。
        //
        // 1 バッチ 1 バッファ（Issue #398）: 行数分の `Vec<u8>` 確保
        // （`encoded_rows: Vec<Vec<u8>>`）を、連続 arena（`arena: Vec<u8>`）＋
        // 各行の範囲表（`ranges: Vec<Range<usize>>`）へ置換する。総サイズは
        // `encoded_row_len` で事前に `checked_add` 積算し `try_reserve_exact` で
        // 1 回だけ確保する（無制限 `with_capacity` を使わない。coding-rust.md）。
        // 範囲は `encode_row_into` 呼び出し前後の `arena.len()` から機械的に
        // 導出するため、行の取り違え（別行のバイト列を書き込む事故）は起きない。
        //
        // エンコードの失敗（tenant 空・上限超過等）は、以前 `for_insert_batch` が
        // 行ごとに `encode_row` していた際と同じ要求記載順で最初に発生するため、
        // どの行が最初に失敗するか・エラー種別は変更前と同一になる（サイズ計算
        // 段階の `encoded_row_len` も `encode_row_into` と同一の検証条件を使う
        // ため、事前積算でも失敗する行・エラー種別は変わらない）。
        let mut total_len: usize = 0;
        for (_, row) in rows {
            let len = crate::storage::encoded_row_len(row)?;
            total_len = total_len.checked_add(len).ok_or_else(|| {
                TenantWriteError::Storage(StorageError::Codec(
                    "batch encoded length overflow".to_string(),
                ))
            })?;
        }
        let mut arena: Vec<u8> = Vec::new();
        arena.try_reserve_exact(total_len).map_err(|_| {
            TenantWriteError::Storage(StorageError::Codec(
                "failed to reserve batch encode buffer".to_string(),
            ))
        })?;
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        ranges.try_reserve_exact(rows.len()).map_err(|_| {
            TenantWriteError::Storage(StorageError::Codec(
                "failed to reserve batch range table".to_string(),
            ))
        })?;
        for (_, row) in rows {
            let start = arena.len();
            crate::storage::encode_row_into(&mut arena, row)?;
            ranges.push(start..arena.len());
        }
        // バッチ全体で 1 ハッシュ（TASK-101・RECOVER-10。`content_hash` モジュール
        // ドキュメント参照。要求記載順を含めて連結する）。`Err` の場合は行の書き込みへ
        // 進まず、この後 `write_txn` が commit されない（呼び出し元の `?` で早期
        // return → drop）ため台帳追記も破棄され、部分書き込みが残らない（fail-closed。
        // TASK-94・RECOVER-3 の原子性契約を包含する）。
        let mut hash_input: Vec<(u64, &[u8])> = Vec::new();
        hash_input.try_reserve_exact(rows.len()).map_err(|_| {
            TenantWriteError::Storage(StorageError::Codec(
                "failed to reserve batch hash input".to_string(),
            ))
        })?;
        for ((id, _), range) in rows.iter().zip(ranges.iter()) {
            let encoded = arena.get(range.clone()).ok_or_else(|| {
                TenantWriteError::Storage(StorageError::Codec(
                    "batch encode arena range out of bounds".to_string(),
                ))
            })?;
            hash_input.push((*id, encoded));
        }
        let content_hash = content_hash::for_insert_batch_encoded(&hash_input)?;
        ledger::record_in_txn(
            write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        for ((id, row), range) in rows.iter().zip(ranges.iter()) {
            schema.validate_row_embedding_dim(row.embedding.len())?;
            let key = (ctx.tenant_id(), *id);
            let encoded = arena.get(range.clone()).ok_or_else(|| {
                TenantWriteError::Storage(StorageError::Codec(
                    "batch encode arena range out of bounds".to_string(),
                ))
            })?;
            insert_unique_row(&mut row_table, key, encoded)?;
        }
        drop(row_table);
        // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約検査
        // （[`insert_row_unchecked`] と同じ判定順序。`constraint.rs` モジュール
        // ドキュメント参照）。
        let ids: Vec<u64> = rows.iter().map(|(id, _)| *id).collect();
        crate::constraint::enforce_row_constraints_in_txn(
            write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            &ids,
            target.fk_check_mode(),
        )?;
        crate::catalog::bump_table_generation_in_txn(write_txn, table)?;
        Ok(((), TxnEffect::Wrote))
    })
}

/// スキーマ列順の型付き値列から 1 行挿入する（`catalog.rs::Storage::insert_typed_row` の
/// テナント境界付き版。TASK-95・対象ビヘイビア: RECOVER-4, TABLE-12）。
///
/// 行の `tenant_id` は**引数で受け取らず** `ctx`（サーバー側導出テナント。WIRE-2・
/// RLS-6）から導出する（クライアント自己申告のテナントを書き込みへ持ち込む経路を
/// 作らない。security.md P0）。重複検出・物理キーの扱いは [`insert_row`] と同一。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`insert_typed_row_unchecked`] へ委譲する（[`insert_row`] と同じ設計。
/// TASK-92・RECOVER-1・codex-review P1 指摘・PR #217）。`crate::sql::exec::execute_insert`
/// は `sql::allowlist::validate_insert` が既にガード済みであることを前提に
/// [`insert_typed_row_unchecked`] を直接呼ぶため、本関数を経由しない
/// （`sql/exec.rs` のドキュメント参照）。
pub fn insert_typed_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    visibility: crate::storage::Visibility,
    values: &[crate::row_codec::Value],
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    insert_typed_row_unchecked(
        WriteTarget::Autocommit(storage),
        table,
        ctx,
        id,
        visibility,
        values,
        ledger_write,
        None,
    )
}

/// [`insert_typed_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と
/// 同じ設計）。呼び出し元は本モジュール内の [`insert_typed_row`] と
/// `crate::sql::exec::execute_insert_with_schema`（`allowlist::validate_insert` で
/// ガード済み。`LedgerMode::resolve` の結果をそのまま渡す。TASK-93・RECOVER-2）。
///
/// `expected_schema`（codex-review P1 指摘・PR #823）: 呼び出し元が値配列
/// `values` を束縛した時点のスキーマ（`TableSchema`。`PartialEq`／`Eq` 実装
/// 済みで列名・型・`nullable`・宣言順を丸ごと比較できる）。`Some` の場合、
/// 本関数が `require_table_schema_write` で write トランザクション内に
/// **改めて**取得したスキーマと不一致なら fail-closed に拒否する
/// （`CatalogError::Invalid` → `sql::exec::map_insert_write_error` 経由で
/// 既存の `22000` へ収束。新規 `wire_code` は追加しない）。
///
/// この検証が無いと、呼び出し元が読み取り専用トランザクションで束縛した
/// `values`（列の**位置**で `schema.columns` に対応づけられる）と、本関数が
/// 実際に書き込み時点で参照するスキーマとの間に競合（束縛後・書き込み前に
/// 同名テーブルが `DROP`・再作成され `TEXT` 列の宣言順が入れ替わった等）が
/// 生じても検出できず、`VECTOR` 列の位置・次元さえ一致していれば
/// `validate_row_embedding_dim` を素通りしたうえで値が意図しない列へ保存され得る
/// （`core::EngineCore::execute_bound_insert_in_session` が read トランザクション
/// で束縛した後にトランザクションを閉じ、実書き込みは別の write トランザクション
/// で行う構造のため発生しうる TOCTOU）。`values` 自体を使い回さない他の呼び出し元
/// （テスト専用の [`insert_typed_row`] 等）は `None` を渡し既存動作のまま不変。
// `expected_schema`（codex-review P1 指摘・PR #823）追加で 8 引数。既存の
// `arena.rs`・`hnsw.rs` と同じ方針で許容する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_typed_row_unchecked(
    target: WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    visibility: crate::storage::Visibility,
    values: &[crate::row_codec::Value],
    ledger_write: LedgerWrite<'_>,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    target.with_txn(|write_txn| {
        let schema = require_table_schema_write(write_txn, table)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "table schema changed after the insert values were bound".to_string(),
                )));
            }
        }
        let vector_idx = schema
            .columns
            .iter()
            .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)));
        // Issue #995: `VECTOR` 列を持たないスキーマでは embedding を空のまま扱う
        // （読み取り側の dim==0 モデル・`sql/scan.rs`・`sql/aggregate.rs` の
        // `expected_dim: Option<u32>` と整合。列がある場合の「値が欠落・非
        // Vector なら拒否」という fail-closed 判定は変えない）。
        //
        // Issue #485: `Vec<f32>` の複製（dim 128 で 512 B）を避けるため
        // `values` を所有する呼び出し元のバッファから借用する（`RowInput`・
        // `content_hash::for_typed_insert` はいずれも `&[f32]` で受けられる
        // ため、この関数の生存期間内で借用を保持するだけで足りる）。
        let embedding: &[f32] = match vector_idx {
            Some(idx) => match values.get(idx) {
                Some(crate::row_codec::Value::Vector(v)) => v.as_slice(),
                _ => {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "VECTOR column value missing or not a Vector".to_string(),
                    )))
                }
            },
            None => &[],
        };
        schema.validate_row_embedding_dim(embedding.len())?;
        let metadata = crate::row_codec::encode_scalar_columns(&schema, values)
            .map_err(|e| CatalogError::Invalid(e.to_string()))?;
        let row = RowInput {
            tenant_id: ctx.tenant_id(),
            visibility,
            embedding,
            metadata: &metadata,
        };
        // 型付き挿入も行形 INSERT と同じ「新規挿入」操作としてハッシュ化する
        // （TASK-101・RECOVER-10。`content_hash::for_typed_insert` ドキュメント参照）。
        // `Err` の場合は行の書き込みへ進まず、`write_txn` が commit されない（早期
        // return → drop）ため台帳追記も破棄される（fail-closed。TASK-94・RECOVER-3
        // の原子性契約を包含する）。
        //
        // ハッシュ入力には `values`（`schema.columns.len()` 幅・位置インデックス
        // 基準の配列。`sql::parser::bind_insert` が構築）をそのまま渡さず、非
        // VECTOR 列を列名付きペアへ変換してから渡す（cursor bugbot 指摘・PR #248。
        // `content_hash::push_named_scalar_columns` ドキュメント参照。位置基準の
        // ままだと `ALTER TABLE ADD COLUMN` を挟んだ再送で配列幅がずれ、内容一致の
        // 再送が `22023` に誤判定される）。
        let named_columns: Vec<(&str, &crate::row_codec::Value)> = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(idx, column)| {
                Some(*idx) != vector_idx
                    && !matches!(column.ty, crate::catalog::ColumnType::Vector(_))
            })
            .filter_map(|(idx, column)| values.get(idx).map(|value| (column.name.as_str(), value)))
            .collect();
        let content_hash =
            content_hash::for_typed_insert(id, visibility, embedding, &named_columns)?;
        ledger::record_in_txn(
            write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        let key = (ctx.tenant_id(), id);
        let encoded = encode_row(&row)?;
        insert_unique_row(&mut row_table, key, encoded.as_slice())?;
        drop(row_table);
        // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約検査
        // （[`insert_row_unchecked`] と同じ判定順序。`constraint.rs` モジュール
        // ドキュメント参照）。
        crate::constraint::enforce_row_constraints_in_txn(
            write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            &[id],
            target.fk_check_mode(),
        )?;
        crate::catalog::bump_table_generation_in_txn(write_txn, table)?;
        Ok(((), TxnEffect::Wrote))
    })
}

/// [`insert_typed_row_unchecked`] の複数行版（Issue #771・TASK-178・NOSQL-6）。
/// NoSQL 表層（`wire-server::http::query::insert`）の `insert` op が 1 回の要求で
/// `rows` 配列全体を 1 つの `operation_id` に対応づけて書き込むために必要とする
/// （`crate::sql::exec::execute_insert`／`insert_typed_row_unchecked` は 1 呼び出し
/// = 1 台帳エントリのため、複数行を同一 `operation_id` で逐次呼ぶと 2 行目以降が
/// 誤って再送判定される。`insert_rows_unchecked` が生 `RowInput` 向けに持つのと
/// 同型のバッチ経路を、型付き値列向けに提供する）。
///
/// ガードなし実体（`pub(crate)`。[`insert_typed_row_unchecked`] と同じ設計）。
/// クレート外の唯一の呼び出し元は [`crate::sql::exec::execute_insert_batch`]
/// （`operation_id` 必須化ガードを自己完結して適用する）。
///
/// 各行の `values` はスキーマ列順（`id` 疑似列を含まない）。バッチ内 `id` 重複は
/// [`insert_rows_unchecked`] と同じく [`TenantWriteError::IdConflict`] で拒否する
/// （行ストアへ触れる前に検出。同一テナント名前空間内のスコープであることも同じ）。
/// 単一の write トランザクションで完結し、失敗時は部分書き込みが残らない
/// （台帳追記は各行の書き込みより前に同一トランザクション内で行う。
/// `for_typed_insert_batch` のドキュメント参照）。
///
/// `expected_schema`（codex-review P1 指摘・PR #823）は
/// [`insert_typed_row_unchecked`] と同じ契約（`Some` なら束縛時スキーマと
/// write トランザクション内で再取得したスキーマの不一致を `22000` で拒否）。
/// 詳細・不一致時の TOCTOU シナリオは [`insert_typed_row_unchecked`] の
/// ドキュメント参照。
pub(crate) fn insert_typed_rows_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    visibility: crate::storage::Visibility,
    rows: &[(u64, &[crate::row_codec::Value])],
    ledger_write: LedgerWrite<'_>,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    if rows.is_empty() {
        return Ok(());
    }

    // バッチ内 id 重複検出（`insert_rows_unchecked` と同じ設計。確保はフォール
    // ブルにする。coding-rust.md）。
    let mut seen_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    seen_ids.try_reserve(rows.len()).map_err(|_| {
        TenantWriteError::Storage(StorageError::Codec(
            "failed to reserve batch id set".to_string(),
        ))
    })?;
    for (id, _) in rows {
        if !seen_ids.insert(*id) {
            return Err(TenantWriteError::IdConflict);
        }
    }

    let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "table schema changed after the insert values were bound".to_string(),
                )));
            }
        }
        // Issue #995: `VECTOR` 列を持たないスキーマでは embedding を空のまま扱う
        // （`insert_typed_row_unchecked` と同じ設計。読み取り側の dim==0 モデルと整合）。
        let vector_idx = schema
            .columns
            .iter()
            .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)));

        // ハッシュ材料（`(id, visibility, embedding, 列名付きペア列)`）を要求記載順で
        // 事前に組み立てる（`for_typed_insert_batch` ドキュメント参照）。列名付き
        // ペア列は行ごとに新規確保するため、[`content_hash::TypedInsertBatchRow`]
        // （スライス版）ではなく `Vec` を保持する中間型を使う
        // （`clippy::type_complexity` 回避のためのエイリアス）。
        type HashRow<'a> = (
            u64,
            crate::storage::Visibility,
            &'a [f32],
            Vec<(&'a str, &'a crate::row_codec::Value)>,
        );
        let mut hash_rows: Vec<HashRow<'_>> = Vec::new();
        hash_rows.try_reserve_exact(rows.len()).map_err(|_| {
            TenantWriteError::Storage(StorageError::Codec(
                "failed to reserve batch hash input".to_string(),
            ))
        })?;
        for (id, values) in rows {
            let embedding: &[f32] = match vector_idx {
                Some(idx) => match values.get(idx) {
                    Some(crate::row_codec::Value::Vector(v)) => v.as_slice(),
                    _ => {
                        return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                            "VECTOR column value missing or not a Vector".to_string(),
                        )))
                    }
                },
                None => &[],
            };
            schema.validate_row_embedding_dim(embedding.len())?;
            let named_columns: Vec<(&str, &crate::row_codec::Value)> = schema
                .columns
                .iter()
                .enumerate()
                .filter(|(idx, column)| {
                    Some(*idx) != vector_idx
                        && !matches!(column.ty, crate::catalog::ColumnType::Vector(_))
                })
                .filter_map(|(idx, column)| {
                    values.get(idx).map(|value| (column.name.as_str(), value))
                })
                .collect();
            hash_rows.push((*id, visibility, embedding, named_columns));
        }
        let hash_input: Vec<content_hash::TypedInsertBatchRow<'_>> = hash_rows
            .iter()
            .map(|(id, vis, emb, cols)| (*id, *vis, *emb, cols.as_slice()))
            .collect();
        let content_hash = content_hash::for_typed_insert_batch(&hash_input)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        for (id, values) in rows {
            let embedding: &[f32] = match vector_idx {
                Some(idx) => match values.get(idx) {
                    Some(crate::row_codec::Value::Vector(v)) => v.as_slice(),
                    _ => {
                        return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                            "VECTOR column value missing or not a Vector".to_string(),
                        )))
                    }
                },
                None => &[],
            };
            let metadata = crate::row_codec::encode_scalar_columns(&schema, values)
                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
            let row = RowInput {
                tenant_id: ctx.tenant_id(),
                visibility,
                embedding,
                metadata: &metadata,
            };
            let key = (ctx.tenant_id(), *id);
            let encoded = encode_row(&row)?;
            insert_unique_row(&mut row_table, key, encoded.as_slice())?;
        }
    }
    // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約検査
    // （[`insert_row_unchecked`] と同じ判定順序。`constraint.rs` モジュール
    // ドキュメント参照）。
    {
        let ids: Vec<u64> = rows.iter().map(|(id, _)| *id).collect();
        let schema_for_pk = require_table_schema_write(&write_txn, table)?;
        crate::constraint::enforce_row_constraints_in_txn(
            &write_txn,
            table,
            &schema_for_pk,
            ctx.tenant_id(),
            &ids,
            crate::constraint::FkCheckMode::All,
        )?;
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(())
}

/// [`upsert_typed_rows_unchecked`] の `DO UPDATE SET` 右辺（SQL-20・TASK-193、
/// Issue #872）。`sql::parser::BoundUpsertValue` に対応する最小表現（本モジュールは
/// `sql` に依存しない設計を維持するため独自 enum を持つ。`upsert_typed_rows_
/// unchecked` の呼び出し元〔`sql::exec::execute_upsert`〕が
/// `BoundUpsertValue` から変換する）。
pub(crate) enum UpsertSetValue<'a> {
    /// 新規挿入しようとした行（`upsert_typed_rows_unchecked` の呼び出し元の
    /// `rows` スライスにおける同じ行の値列）の列インデックス参照。
    Excluded(usize),
    Literal(&'a crate::row_codec::Value),
}

/// [`upsert_typed_rows_unchecked`] の衝突分岐（SQL-20・TASK-193、Issue #872）。
/// `sql::parser::BoundConflictAction` に対応する最小表現。
pub(crate) enum UpsertAction<'a> {
    DoNothing,
    /// (`schema.columns` の対象列インデックス, 右辺) の宣言順スライス
    /// （並べ替えない。`content_hash::for_typed_upsert` の再送判定がこの順序に
    /// 依存する）。
    DoUpdate(&'a [(usize, UpsertSetValue<'a>)]),
}

/// [`upsert_typed_rows_unchecked`] の衝突対象（TABLE-16・Issue #1074）。
/// `sql::parser::BoundConflictTarget` に対応する最小表現（`sql` に依存しない
/// 設計を維持するため独自 enum を持つ。`sql::exec::execute_upsert` が変換する）。
pub(crate) enum UpsertTarget<'a> {
    /// `ON CONFLICT (id)`（既存挙動。SQL-20・TASK-193、Issue #872）。衝突判定は
    /// `(ctx.tenant_id(), id)` の物理キー所有で行う。
    RowId,
    /// `ON CONFLICT (<UNIQUE 制約の構成列>)`（Issue #1074）。`indices` は
    /// `schema.columns` に対する論理インデックス（解決した制約の宣言順。
    /// `sql::parser::BoundConflictTarget::Unique` と同じ規約）。衝突判定は
    /// `docs/design/sql-upsert.md`「衝突判定スコープ」節参照——テナントが
    /// 所有する**全行**（可視性を問わない）を母集合に、書き込み**前**の 1 回の
    /// 走査で対象キーの一致を判定する（`constraint::enforce_unique_keys_in_txn`
    /// と同じ RLS-9・RLS-10 (c) の判断）。
    Unique(&'a [usize]),
}

/// [`upsert_typed_rows_unchecked`] の成功時の結果（SQL-20・TASK-193、
/// Issue #872）。`inserted + updated` が `sql::exec::InsertOutcome::rows_affected`
/// へそのまま写像される（`DO NOTHING` で衝突した行はいずれにも数えない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct UpsertOutcome {
    pub(crate) inserted: u64,
    pub(crate) updated: u64,
}

/// `table` へ `INSERT ... ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`
/// を実行する（SQL-20・TASK-193、Issue #872）。[`insert_typed_rows_unchecked`]
/// の複数行版と同じ「1 write トランザクション・1 台帳エントリ」構造を持つが、
/// 行ごとの衝突判定・分岐（新規挿入／`DO NOTHING`／`DO UPDATE` の read-merge-
/// write）を追加で行う。
///
/// ## 衝突判定スコープ（TABLE-12・RLS-9。`docs/design/sql-upsert.md`「衝突判定
/// スコープ」節参照）
///
/// 物理キー `(ctx.tenant_id(), id)` の**所有**で判定する（RLS 可視性ではない）。
/// [`update_row_unchecked`]／[`delete_row_impl`] と同じ二重防御
/// （`decode_row_for_key` によるキー↔ヘッダ tenant 整合検査＋`ctx.is_owner`）を
/// 使う。物理キーが既にテナント名前空間化されているため、他テナントの同一
/// `id` は取得すらされず、常に「非衝突＝新規挿入」として扱われる（他テナント
/// 行の存在で分岐するコードを一切持たない）。
///
/// ## 判定順序（`docs/design/sql-upsert.md`「判定順序」節参照）
///
/// 台帳照合・追記（[`content_hash::for_typed_upsert`]・`ledger::record_in_txn`）
/// を行ごとの衝突分岐より**前**に行う（[`update_row_unchecked`]・
/// [`delete_row_impl`] と同じ契約。TASK-101・RECOVER-10。commit 済み操作の
/// 再送が行状態の変化に左右されず検出される）。台帳へ書き込む内容ハッシュは
/// 「新規挿入しようとした値」（`rows` そのもの。既存行の内容には依存しない）
/// から決定的に計算するため、本関数は**同一の `rows`／`action` の再送に対して
/// 常に同一ハッシュを返す**（再送検知の前提条件。`content_hash` モジュール
/// ドキュメントの「正規化の方針」参照）。
///
/// 本関数が変更を加えた行が 1 件もない場合（全行 `DO NOTHING` で衝突）は
/// テーブル世代を進行させない（`DeleteRowOutcome::NotFound` と同じ設計。
/// 内容が変わらないため世代整合キャッシュ〔`SqlArenaCache` 等〕は有効のまま
/// でよい。台帳エントリ自体は変更ゼロでも commit する）。
///
/// `expected_schema` は [`insert_typed_rows_unchecked`] と同じ契約（`Some` なら
/// 束縛時スキーマと write トランザクション内で再取得したスキーマの不一致を
/// `22000` で拒否）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn upsert_typed_rows_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    insert_visibility: crate::storage::Visibility,
    rows: &[(u64, &[crate::row_codec::Value])],
    target: &UpsertTarget<'_>,
    action: &UpsertAction<'_>,
    ledger_write: LedgerWrite<'_>,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<UpsertOutcome, TenantWriteError> {
    validate_identifier(table)?;
    if rows.is_empty() {
        return Ok(UpsertOutcome::default());
    }

    // バッチ内 id 重複検出（`insert_typed_rows_unchecked` と同じ設計。呼び出し元
    // `sql::parser::bind_upsert_form` が束縛時点で既に `22000` として拒否済みの
    // ため通常は到達しないが、本関数単体でも fail-closed を保つ）。`VALUES` の
    // `id` は対象が `Unique`（Issue #1074）の場合も衝突判定には使わないが、
    // 新規挿入分岐で使う一意な行識別子であることに変わりはないため、対象に
    // 関わらず検出する。
    let mut seen_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    seen_ids.try_reserve(rows.len()).map_err(|_| {
        TenantWriteError::Storage(StorageError::Codec(
            "failed to reserve upsert batch id set".to_string(),
        ))
    })?;
    for (id, _) in rows {
        if !seen_ids.insert(*id) {
            return Err(TenantWriteError::IdConflict);
        }
    }

    // バッチ内の対象キー重複検出（Issue #1074。UNIQUE 対象のみ）。呼び出し元
    // `sql::parser::bind_upsert_form` が束縛時点で既に `22000` として拒否済みの
    // ため通常は到達しないが、`(id)` 対象の `seen_ids` と同じ理由で本関数単体
    // でも fail-closed を保つ（防御的二重検査。indices の解決自体は
    // `expected_schema` 照合により本 write トランザクション内の `schema` と
    // 整合することが保証されている）。NULL を含むキー（`unique_key_from_values`
    // が `Ok(None)` を返す行）は NULLS DISTINCT のため対象外。
    if let UpsertTarget::Unique(indices) = target {
        let mut seen_keys: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        seen_keys.try_reserve(rows.len()).map_err(|_| {
            TenantWriteError::Storage(StorageError::Codec(
                "failed to reserve upsert batch target key set".to_string(),
            ))
        })?;
        for (_, values) in rows {
            let Some(key) = crate::constraint::unique_key_from_values(indices, values)
                .map_err(|m| TenantWriteError::Catalog(CatalogError::Invalid(m.to_string())))?
            else {
                continue;
            };
            if !seen_keys.insert(key) {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "duplicate ON CONFLICT target key within the same UPSERT statement".to_string(),
                )));
            }
        }
    }

    let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    let mut inserted: u64 = 0;
    let mut updated: u64 = 0;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "table schema changed after the insert values were bound".to_string(),
                )));
            }
        }
        // Issue #995: `VECTOR` 列を持たないスキーマでは embedding を空のまま扱う
        // （`insert_typed_rows_unchecked` と同じ設計。読み取り側の dim==0 モデルと整合）。
        let vector_idx = schema
            .columns
            .iter()
            .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)));

        // ハッシュ材料（`(id, visibility, embedding, 列名付きペア列)`）を要求記載順で
        // 事前に組み立てる（`insert_typed_rows_unchecked` と同じ構造。`content_hash::
        // for_typed_upsert` ドキュメント参照）。
        type HashRow<'a> = (
            u64,
            crate::storage::Visibility,
            &'a [f32],
            Vec<(&'a str, &'a crate::row_codec::Value)>,
        );
        let mut hash_rows: Vec<HashRow<'_>> = Vec::new();
        hash_rows.try_reserve_exact(rows.len()).map_err(|_| {
            TenantWriteError::Storage(StorageError::Codec(
                "failed to reserve upsert batch hash input".to_string(),
            ))
        })?;
        for (id, values) in rows {
            let embedding: &[f32] = match vector_idx {
                Some(idx) => match values.get(idx) {
                    Some(crate::row_codec::Value::Vector(v)) => v.as_slice(),
                    _ => {
                        return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                            "VECTOR column value missing or not a Vector".to_string(),
                        )))
                    }
                },
                None => &[],
            };
            schema.validate_row_embedding_dim(embedding.len())?;
            let named_columns: Vec<(&str, &crate::row_codec::Value)> = schema
                .columns
                .iter()
                .enumerate()
                .filter(|(idx, column)| {
                    Some(*idx) != vector_idx
                        && !matches!(column.ty, crate::catalog::ColumnType::Vector(_))
                })
                .filter_map(|(idx, column)| {
                    values.get(idx).map(|value| (column.name.as_str(), value))
                })
                .collect();
            hash_rows.push((*id, insert_visibility, embedding, named_columns));
        }
        let hash_input: Vec<content_hash::TypedInsertBatchRow<'_>> = hash_rows
            .iter()
            .map(|(id, vis, emb, cols)| (*id, *vis, *emb, cols.as_slice()))
            .collect();

        // `UpsertAction::DoUpdate` の右辺をハッシュ材料の最小表現へ変換する
        // （宣言順を保持。`content_hash::UpsertHashAction::DoUpdate` が参照を
        // 借用するため、`hash_assignments` は `match` の外側で寿命を確保する）。
        let mut hash_assignments: Vec<(String, content_hash::UpsertAssignmentHashValue<'_>)> =
            Vec::new();
        if let UpsertAction::DoUpdate(assignments) = action {
            hash_assignments
                .try_reserve_exact(assignments.len())
                .map_err(|_| {
                    TenantWriteError::Storage(StorageError::Codec(
                        "failed to reserve upsert hash assignments".to_string(),
                    ))
                })?;
            for (col_idx, value) in assignments.iter() {
                let col_name = schema
                    .columns
                    .get(*col_idx)
                    .map(|c| c.name.as_str())
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "unknown SET target column index".to_string(),
                        ))
                    })?;
                let hash_value = match value {
                    UpsertSetValue::Excluded(src_idx) => {
                        let src_name = schema
                            .columns
                            .get(*src_idx)
                            .map(|c| c.name.as_str())
                            .ok_or_else(|| {
                                TenantWriteError::Catalog(CatalogError::Invalid(
                                    "unknown EXCLUDED source column index".to_string(),
                                ))
                            })?;
                        content_hash::UpsertAssignmentHashValue::Excluded(src_name)
                    }
                    UpsertSetValue::Literal(v) => {
                        content_hash::UpsertAssignmentHashValue::Literal(v)
                    }
                };
                hash_assignments.push((col_name.to_string(), hash_value));
            }
        }
        let hash_action = match action {
            UpsertAction::DoNothing => content_hash::UpsertHashAction::DoNothing,
            UpsertAction::DoUpdate(_) => {
                content_hash::UpsertHashAction::DoUpdate(&hash_assignments)
            }
        };
        // UNIQUE 対象の列名（解決した制約の宣言順。`target` の `indices` と同じ
        // 並び）を `content_hash::for_typed_upsert` へ渡す（Issue #1074）。
        let hash_target_columns: Vec<&str> = match target {
            UpsertTarget::RowId => Vec::new(),
            UpsertTarget::Unique(indices) => indices
                .iter()
                .map(|&idx| {
                    schema
                        .columns
                        .get(idx)
                        .map(|c| c.name.as_str())
                        .ok_or_else(|| {
                            TenantWriteError::Catalog(CatalogError::Invalid(
                                "internal: ON CONFLICT target column index out of range"
                                    .to_string(),
                            ))
                        })
                })
                .collect::<Result<_, _>>()?,
        };
        let hash_target = match target {
            UpsertTarget::RowId => content_hash::UpsertHashTarget::RowId,
            UpsertTarget::Unique(_) => content_hash::UpsertHashTarget::Unique(&hash_target_columns),
        };
        let content_hash_value =
            content_hash::for_typed_upsert(&hash_target, &hash_action, &hash_input)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash_value,
        )?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;

        // UNIQUE 対象（Issue #1074）は書き込み**前**のスナップショットで対象
        // テナントの既存行を 1 回だけ走査し、対象キー → 既存行 id の対応表を
        // 作る（`docs/design/sql-upsert.md`「衝突判定スコープ」節。可視性を
        // 問わない全行が母集合。`row_table` はまだ本文で 1 行も書き込んでいない
        // ためこの時点のスキャンが「この文の書き込みより前」の状態と一致する）。
        let unique_existing: Option<std::collections::HashMap<Vec<u8>, u64>> = match target {
            UpsertTarget::RowId => None,
            UpsertTarget::Unique(indices) => {
                Some(crate::constraint::scan_tenant_rows_by_unique_key(
                    &row_table,
                    &schema,
                    ctx.tenant_id(),
                    indices,
                )?)
            }
        };

        // 主キー制約検査（下記ブロック終端）の対象 id。`DO NOTHING` で実際には
        // 書かなかった行は含めない（TABLE-16・TASK-204、Issue #903。
        // `constraint.rs` モジュールドキュメント参照）。
        let mut written_ids: Vec<u64> = Vec::new();
        // `FOREIGN KEY` の `ON UPDATE` 参照アクション（Issue #1076）の連鎖起点。
        // `DO UPDATE` で実際に既存行を書き換えた場合のみ、書き換え前の全列値を
        // 積む（新規挿入行は「更新」ではないため対象外）。
        let mut pre_images = crate::constraint::UpdatedKeyPreImages::new();
        for (id, values) in rows {
            // 衝突判定の対象行 id を対象ごとに決定する（Issue #1074）。`(id)`
            // 対象は常に VALUES 自身の `id` で判定する（既存挙動、無変更）。
            // UNIQUE 対象は上記の事前走査で一致した既存行の `id` を使う（対象
            // キーに NULL を含む行・未一致はいずれも非衝突＝新規挿入分岐へ）。
            let conflict_id: Option<u64> = match target {
                UpsertTarget::RowId => Some(*id),
                UpsertTarget::Unique(indices) => {
                    crate::constraint::unique_key_from_values(indices, values)
                        .map_err(|m| {
                            TenantWriteError::Catalog(CatalogError::Invalid(m.to_string()))
                        })?
                        .and_then(|key| unique_existing.as_ref().and_then(|m| m.get(&key).copied()))
                }
            };
            // `AccessGuard` の借用をこのブロック内に閉じ込め、後続の可変借用
            // （`insert`）と衝突しないようにする（`update_row_unchecked` と
            // 同じパターン）。所有権判定は `decode_row_for_key`（キー↔ヘッダ
            // tenant 整合検査。TABLE-12）＋ `ctx.is_owner` の二重防御。
            let existing_owned: Option<crate::storage::Row> = match conflict_id {
                Some(cid) => match row_table
                    .get(&(ctx.tenant_id(), cid))
                    .map_err(CatalogError::from)?
                {
                    Some(guard) => {
                        let row =
                            crate::storage::decode_row_for_key(ctx.tenant_id(), cid, guard.value())
                                .map_err(TenantWriteError::Storage)?;
                        Some(row)
                    }
                    None => None,
                },
                None => None,
            };
            let owns_existing = existing_owned
                .as_ref()
                .map(|row| ctx.is_owner(row.tenant_id.as_str()))
                .unwrap_or(false);

            if owns_existing {
                // `owns_existing` が真になり得るのは `conflict_id` が `Some`
                // の場合のみ（`existing_owned` の構築規則）であり、以降の
                // 分岐は常に `Some` の中身（衝突した既存行の id）を使う。
                let write_id = match conflict_id {
                    Some(cid) => cid,
                    None => {
                        return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                            "internal: owns_existing without a conflict id".to_string(),
                        )));
                    }
                };
                let key = (ctx.tenant_id(), write_id);
                let existing = match existing_owned {
                    Some(row) => row,
                    None => {
                        // `owns_existing` は `existing_owned.is_some()` の場合に
                        // のみ真になり得ない（`unwrap_or(false)` の契約）ため
                        // 到達しない。untrusted 経路の添字禁止（coding-rust.md）
                        // に従い `unwrap` の代わりに fail-closed な内部エラーで
                        // 閉じる。
                        return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                            "internal: owns_existing without an existing row".to_string(),
                        )));
                    }
                };
                match action {
                    UpsertAction::DoNothing => {
                        // 変更なし（新規挿入もしない）。
                    }
                    UpsertAction::DoUpdate(assignments) => {
                        // read-merge-write: 既存行のスカラー列を復元し、SET 対象
                        // 列だけを新しい値で上書きする（宣言順を保持する必要は
                        // ない——最終的な行内容は適用順に依存しない一意な値へ
                        // 収束する。同一列への重複代入は `bind_upsert_assignments`
                        // が束縛時に `22000` で拒否済み）。
                        let mut merged_values =
                            crate::row_codec::decode_scalar_columns(&schema, &existing.metadata)
                                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
                        // 書き換える前の全列値を ON UPDATE 連鎖の pre-image として
                        // 積む（Issue #1076）。UNIQUE 対象の衝突では実際に書き換える
                        // 既存行が VALUES 自身の `id`（`*id`）と異なり得るため、必ず
                        // 衝突判定で確定した `write_id` をキーに記録する（`*id` で
                        // 記録すると、連鎖側の `collect_action_targets` が入力 id で
                        // 親行を読み直して新キーを取得できず、`ON UPDATE CASCADE` 等が
                        // 発火せず参照元行が残留し `23503` で失敗する。codex/review・
                        // cursor bugbot 指摘）。`id` 参照 FK は ON UPDATE で発火しない
                        // （A7）ため、主キー・UNIQUE を宣言しないテーブルは複製コストを
                        // 払わない（早期 return と同じ判定条件）。
                        if schema.primary_key().is_some() || !schema.unique_constraints().is_empty()
                        {
                            pre_images.record(write_id, merged_values.clone());
                        }
                        let mut embedding_value: Vec<f32> = existing.embedding.clone();

                        for (col_idx, value) in assignments.iter() {
                            // `src_idx`／`col_idx` は束縛時点のスキーマ（`bind_upsert_assignments`）
                            // が `schema.columns` に対して検証済みの位置インデックスであり、
                            // `expected_schema` 照合（`Some` の場合。呼び出し元
                            // `sql::exec::execute_upsert` ドキュメント参照）によって
                            // 本 write トランザクション内の `schema` と一致することが
                            // 保証されている。したがって範囲外・型不一致はいずれも
                            // 到達しないはずの内部不変条件違反であり、値を黙って
                            // `Null` へ差し替えたり SET を無視したりせず fail-closed に
                            // 拒否する（cursor bugbot 指摘・PR #990。黙って無視すると
                            // `updated` カウント・テーブル世代だけが進み、実際には
                            // 適用されなかった SET が適用されたかのような不整合が
                            // 生じる）。
                            let new_value = match value {
                                UpsertSetValue::Excluded(src_idx) => {
                                    values.get(*src_idx).cloned().ok_or_else(|| {
                                        CatalogError::Invalid(
                                            "internal: EXCLUDED source column index out of range"
                                                .to_string(),
                                        )
                                    })?
                                }
                                UpsertSetValue::Literal(v) => (*v).clone(),
                            };
                            if Some(*col_idx) == vector_idx {
                                match new_value {
                                    crate::row_codec::Value::Vector(v) => embedding_value = v,
                                    _ => {
                                        return Err(TenantWriteError::Catalog(
                                            CatalogError::Invalid(
                                                "VECTOR column SET value must be a vector"
                                                    .to_string(),
                                            ),
                                        ))
                                    }
                                }
                            } else {
                                let slot = merged_values.get_mut(*col_idx).ok_or_else(|| {
                                    CatalogError::Invalid(
                                        "internal: SET target column index out of range"
                                            .to_string(),
                                    )
                                })?;
                                *slot = new_value;
                            }
                        }
                        schema.validate_row_embedding_dim(embedding_value.len())?;
                        let metadata =
                            crate::row_codec::encode_scalar_columns(&schema, &merged_values)
                                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
                        let row = RowInput {
                            tenant_id: ctx.tenant_id(),
                            // 既存行の可視性を保持する（SET で触れない列と同じ
                            // 扱い。新規挿入行のみ `insert_visibility` を使う）。
                            visibility: existing.visibility,
                            embedding: &embedding_value,
                            metadata: &metadata,
                        };
                        let encoded = encode_row(&row)?;
                        row_table
                            .insert(key, encoded.as_slice())
                            .map_err(CatalogError::from)?;
                        updated = updated.checked_add(1).ok_or_else(|| {
                            TenantWriteError::Storage(StorageError::Codec(
                                "upsert updated row counter overflow".to_string(),
                            ))
                        })?;
                        written_ids.push(write_id);
                    }
                }
            } else {
                // 非衝突（新規挿入）。VALUES 自身の `id` を書き込み先の物理キーに
                // 使う（対象に関わらず不変。UNIQUE 対象で同じ `id` を持つ既存行が
                // 既にあれば `insert_unique_row` が `IdConflict`〔`23505`〕で
                // 拒否する——PostgreSQL の「arbiter でない制約の違反は吸収しない」
                // 挙動と同じ）。既存 `insert_typed_rows_unchecked` と同じ組み立て
                // （Issue #995: `VECTOR` 列なしスキーマは embedding 空）。
                let key = (ctx.tenant_id(), *id);
                let embedding: &[f32] = match vector_idx {
                    Some(idx) => match values.get(idx) {
                        Some(crate::row_codec::Value::Vector(v)) => v.as_slice(),
                        _ => {
                            return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                                "VECTOR column value missing or not a Vector".to_string(),
                            )))
                        }
                    },
                    None => &[],
                };
                let metadata = crate::row_codec::encode_scalar_columns(&schema, values)
                    .map_err(|e| CatalogError::Invalid(e.to_string()))?;
                let row = RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: insert_visibility,
                    embedding,
                    metadata: &metadata,
                };
                let encoded = encode_row(&row)?;
                insert_unique_row(&mut row_table, key, encoded.as_slice())?;
                inserted = inserted.checked_add(1).ok_or_else(|| {
                    TenantWriteError::Storage(StorageError::Codec(
                        "upsert inserted row counter overflow".to_string(),
                    ))
                })?;
                written_ids.push(*id);
            }
        }
        // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約
        // 検査。行ストアの `mut row_table` ハンドルはこのブロックを抜けた
        // 時点で解放されるため、その直前（同一ブロック内の最後）で読み取り
        // ハンドルとして再度開く（`insert_row_unchecked` と異なり、ここは
        // `row_table` を drop する前にまだブロック内にいるため、别途スコープを
        // 切って再オープンする）。
        if !written_ids.is_empty() {
            drop(row_table);
            crate::constraint::enforce_row_constraints_in_txn(
                &write_txn,
                table,
                &schema,
                ctx.tenant_id(),
                &written_ids,
                crate::constraint::FkCheckMode::All,
            )?;
        }
        // `DO UPDATE` で既存行を更新した場合、このテーブルを参照先とする
        // `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）の参照先側を検査する
        // （更新した列が主キー・UNIQUE 制約の構成列を含む場合のみ実際に走査する）。
        if updated > 0 {
            if let UpsertAction::DoUpdate(assignments) = action {
                let updated_columns: Vec<usize> = assignments.iter().map(|(idx, _)| *idx).collect();
                crate::constraint::enforce_referencing_rows_in_txn(
                    &write_txn,
                    table,
                    &schema,
                    ctx.tenant_id(),
                    crate::constraint::ReferencedRowsChange::ColumnsUpdated(&updated_columns),
                    Some(&pre_images),
                    crate::constraint::FkCheckMode::All,
                )?;
            }
        }
    }
    if inserted > 0 || updated > 0 {
        crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    }
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(UpsertOutcome { inserted, updated })
}

/// `table` の既存行を 1 件更新する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// `row.tenant_id` が `ctx` のテナントと不一致なら
/// [`TenantWriteError::Forbidden`]（自テナント行を他テナントへ付け替える試行を含む）。
/// 対象行が不存在、または既存行の所有者が `ctx` と一致しない場合は
/// **区別せず** [`TenantWriteError::NotFound`]（他テナントの存在情報を漏らさない。
/// security.md P0）。
///
/// スキーマ取得・次元検証・既存行の所有権判定・書き込みを単一の write トランザクション
/// 内で行う（[`insert_row`] と同じ TOCTOU 対策）。次元検証は
/// [`crate::catalog::TableSchema::validate_row_embedding_dim`] を使うため、`VECTOR`
/// 列を持たないテーブルでは embedding 空（dim 0）の行全体置換のみを受理する
/// （TABLE-1・Issue #1079）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`update_row_unchecked`] へ委譲する（[`insert_row`] と同じ設計。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217）。
pub fn update_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    update_row_unchecked(storage, table, ctx, id, row, ledger_write)
}

/// [`update_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計）。呼び出し元は本モジュール内の [`update_row`] と
/// `crate::core::EngineCore::update_row`（`self.ledger_mode` でガード済み）。
///
/// `ledger`（TASK-93・RECOVER-2、TASK-101・RECOVER-10）: 台帳照合・追記を所有権判定
/// （`owns_existing`。`NotFound` 判定）より**前**に行う（TASK-93 時点の元設計から
/// TASK-101 で反転）。commit 済み操作の再送は行状態が既に変化済み（削除済み行の
/// 再更新等）のことがあり、所有権判定を先に行うと `NotFound`（`P0002`）が返って
/// しまい、ハッシュ一致による再送検知（`23505`）に到達できない。台帳照合を先行
/// させることで、再送検知が行状態の変化に左右されなくなる。「失敗した書き込みは
/// 台帳へ残らない」不変条件は、エラー時に write トランザクションが commit されず
/// drop（abort）される既存契約でそのまま保たれる（台帳照合が先でも、後続で
/// `NotFound` を返せば同じ txn 内の台帳挿入も一緒に破棄される）。
pub(crate) fn update_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
    ledger_write: LedgerWrite<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    if !ctx.is_owner(row.tenant_id) {
        return Err(TenantWriteError::Forbidden);
    }
    let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    // `FOREIGN KEY` の `ON UPDATE` 参照アクション（Issue #1076）の連鎖起点。
    // 全列置換は参照先キーを変え得るため、既存行の全列値を書き換える前に捕捉する。
    let mut pre_images = crate::constraint::UpdatedKeyPreImages::new();
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        schema.validate_row_embedding_dim(row.embedding.len())?;
        // エンコードは 1 回のみ（Issue #397）: `for_update` が内部で `encode_row` し、
        // 書き込み側で同じ行をもう一度 `encode_row` していた二重実行を排除する。
        // ここでの `encode_row` は変更前も `owns_existing` 判定より前（台帳ハッシュ
        // 計算の内部）で無条件に走っていたため、この位置へ移してもエラー優先順位
        // （`encode_row` のエラー → 台帳の内容照合 → `NotFound`）は変わらない。
        let encoded = encode_row(row)?;
        let content_hash = content_hash::for_update_encoded(id, &encoded)?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // 物理キーの名前空間化（TABLE-12）により、他テナントの行はそもそも別キーで
        // 到達不能（構造的な遮断）。既存行の所有者照合は `is_owner` の単一照合パスに
        // 残し、二重防御とする（旧フォーマット行の混在等でヘッダのテナントが
        // キーと食い違う場合も fail-closed 側に倒れる）。
        // `AccessGuard` の借用をこのブロック内に閉じ込め、後続の可変借用（`insert`）と
        // 衝突しないようにする。
        let key = (ctx.tenant_id(), id);
        let owns_existing = match row_table.get(&key).map_err(CatalogError::from)? {
            Some(guard) => {
                let (existing_tenant, _existing_visibility) =
                    decode_row_tenant_and_visibility(guard.value())?;
                if ctx.is_owner(existing_tenant) {
                    // 上書きする前の全列値を ON UPDATE 連鎖の pre-image として
                    // 積む（Issue #1076）。所有者判定が確定した後にのみフル
                    // デコードを試みる（`update_row_columns_unchecked` と同じ
                    // 「不可視・不存在は内容に触れない」設計）。デコード失敗
                    // （テスト専用の生 API 等が投入した非正規メタデータ）は
                    // 全列置換自体の成否には影響させず、この行を pre-image 無し
                    // として扱う（ON UPDATE アクションは発火しないが、事後検証
                    // 〔NO ACTION 相当〕は変わらず有効なため fail-open にはならない）。
                    // `id` 参照 FK は ON UPDATE で発火しない（A7）ため、主キー・
                    // UNIQUE を宣言しないテーブルはこのデコードを一切行わない。
                    if schema.primary_key().is_some() || !schema.unique_constraints().is_empty() {
                        if let Ok(existing) =
                            crate::storage::decode_row_for_key(ctx.tenant_id(), id, guard.value())
                        {
                            if let Ok(old_values) =
                                crate::row_codec::decode_scalar_columns(&schema, &existing.metadata)
                            {
                                pre_images.record(id, old_values);
                            }
                        }
                    }
                    true
                } else {
                    false
                }
            }
            None => false,
        };
        if !owns_existing {
            return Err(TenantWriteError::NotFound);
        }
        row_table
            .insert(key, encoded.as_slice())
            .map_err(CatalogError::from)?;
    }
    // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約検査
    // （[`insert_row_unchecked`] と同じ判定順序。`constraint.rs` モジュール
    // ドキュメント参照）。
    {
        let schema_for_pk = require_table_schema_write(&write_txn, table)?;
        crate::constraint::enforce_row_constraints_in_txn(
            &write_txn,
            table,
            &schema_for_pk,
            ctx.tenant_id(),
            &[id],
            crate::constraint::FkCheckMode::All,
        )?;
        // 全列置換は参照先キー（主キー・UNIQUE 構成列）を変え得るため、このテーブルを
        // 参照先とする `FOREIGN KEY` の参照先側も検査する（TABLE-17・TASK-205、
        // Issue #907）。
        crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            table,
            &schema_for_pk,
            ctx.tenant_id(),
            crate::constraint::ReferencedRowsChange::AllColumnsReplaced,
            Some(&pre_images),
            crate::constraint::FkCheckMode::All,
        )?;
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(())
}

/// SQL `UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>`（SQL-17・TASK-191。
/// 実行結線は Issue #865）専用の書き込み入口。[`update_row_unchecked`]（全行置換 API）
/// と**同一の書き込みプリミティブ**（`validate_identifier`・
/// `require_table_schema_write`・`content_hash`・`ledger::record_in_txn`・
/// `user_rows_table_def`・`decode_row_for_key`・`encode_row`・
/// `bump_table_generation_in_txn`・`recovery::commit_boundary::commit`）だけで
/// 組み立てた**列指定の入口**であり、第 2 の書き込み経路ではない。
///
/// `assignments` は `sql::parser::BoundUpdate::assignments`（束縛時スキーマの列
/// インデックス・宣言順を保持する部分更新表現）をそのまま受け取る。
/// `update_row_unchecked` が要求する `RowInput`（全列を埋めた全行置換）とは異なり、
/// SET で指定されなかった列は本関数が既存行から読み取って維持する
/// （read-merge-write）。read（対象行の既存 `metadata`／`embedding`）→
/// merge（SET 対象列だけを上書き）→ encode → write を**単一の write トランザクション
/// 内**で行う設計は必須: 別 read トランザクションで先に読んでから
/// `update_row_unchecked` へ渡す 2 段構成にすると、同一行への並行 UPDATE（列が
/// 互いに素）で read スナップショットと write の間に他セッションの commit が挟まり
/// lost update が起きる。
///
/// 0 行更新（他テナントの行・未存在 id・RLS 可視集合外の行のいずれも区別しない。
/// security.md P0）でも台帳記録・テーブル世代進行・commit は**必ず**行う
/// （`truncate_table_unchecked` と同じ非対称設計。`update_row_unchecked` の
/// `NotFound` 早期 return とは意図的に異なる契約: 応答・`wire_code`・
/// レイテンシ・台帳の有無のいずれからも「対象行の有無」を観測できないようにする
/// ため）。台帳の内容照合ハッシュ（`content_hash::for_update_columns`。TASK-101・
/// RECOVER-10）は DB の現在状態に依存しないクライアント入力のみから計算する
/// ため、同一 `(id, assignments)` の送信は対象行の状態に関わらず同一ハッシュに
/// なる。
///
/// 前提: 対象行の既存 `metadata` は `row_codec::encode_scalar_columns` が書いた
/// 正規レイアウト（SQL 表層の `INSERT`・型付き挿入 API はすべてこの経路を通る）
/// であることを要求する。旧フォーマットの raw metadata（全行置換版 `RowInput` を
/// 直接構築する Rust API 経由。本モジュール外の非 SQL 呼び出し元専用）が書いた行を
/// 対象にした場合は `decode_scalar_columns` が構造不整合を検出し、格納済みデータの
/// 破損・実装不整合として `CatalogError::CorruptSchema`（`sql::exec::map_write_error`
/// 経由で `XX000`）で fail-closed に拒否する（黙ってスカラー列を欠損させたり
/// 誤ったオフセットで読まない。クライアント入力エラー `22000` に丸めない。
/// codex-review P1 指摘・PR #989）。
///
/// 戻り値は更新行数（`0` または `1`）。呼び出し元は `sql::exec::execute_update`。
/// SET 句の値をスキーマに対して検証する（列 index 境界・型一致・`VECTOR` 次元・
/// `TEXT` 列単体長・`TEXT` 列の累計フレームサイズ）。[`update_row_columns_unchecked`]
/// （単一行・id 指定形）・[`update_rows_where_unchecked`]（述語形。SQL-19・TASK-192・
/// Issue #871）が共有する。
///
/// 呼び出し元は必ず**対象行の探索より前**（かつ台帳記録より前）にこの検証を行うこと。
/// 対象行の存在・可視性を一切参照せずスキーマのみから判定できるため、対象の有無に
/// 関わらず常に同一の拒否（またはいずれも合格）になる fail-closed 契約を保てる
/// （`update_row_columns_unchecked` の同種コメント参照）。述語形でこの順序を守らない
/// 場合、候補行が 0 件（＝一致行なし）だと検証が一切実行されないまま `UPDATE 0` の
/// 成功として `operation_id` が消費されてしまう（codex-review P1 指摘・PR #993 系・
/// Issue #871）。
fn validate_set_assignments(
    schema: &crate::catalog::TableSchema,
    assignments: &[(usize, crate::row_codec::Value)],
) -> Result<(), TenantWriteError> {
    let mut set_text_payload_total: u32 = 0;
    for (idx, value) in assignments {
        let column = schema.columns.get(*idx).ok_or_else(|| {
            TenantWriteError::Catalog(CatalogError::Invalid(
                "SET column index out of range for the current table schema".to_string(),
            ))
        })?;
        match (&column.ty, value) {
            (crate::catalog::ColumnType::Vector(_), crate::row_codec::Value::Vector(v)) => {
                // SET 値の妥当性（次元）は対象行の有無に関わらず常に同じ拒否を
                // 返す（呼び出し元の対象行探索より前に弾く）。
                schema
                    .validate_embedding_dim(v.len())
                    .map_err(TenantWriteError::Catalog)?;
            }
            (crate::catalog::ColumnType::Text, crate::row_codec::Value::Text(t)) => {
                // SET 値の TEXT 長上限検証（対象行の探索より前に行う）。
                let text_len = u32::try_from(t.len()).map_err(|_| {
                    TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "text field too long: {} bytes",
                        t.len()
                    )))
                })?;
                if text_len > crate::row_codec::MAX_TEXT_FIELD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "text field length {text_len} exceeds limit {}",
                        crate::row_codec::MAX_TEXT_FIELD_LEN
                    ))));
                }
                let entry_len = crate::row_codec::scalar_text_entry_len(text_len)
                    .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
                set_text_payload_total =
                    set_text_payload_total
                        .checked_add(entry_len)
                        .ok_or_else(|| {
                            TenantWriteError::Catalog(CatalogError::Invalid(
                                "scalar payload length overflow".to_string(),
                            ))
                        })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Integer, crate::row_codec::Value::Integer(_)) => {
                // 固定幅（presence(1) + 本体(4)）のため、事前検証は累計バイト
                // 予算への計上だけでよい（値域は束縛段
                // `sql::parser::bind_integer_literal` が既に検証済み）。
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_INT4_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::BigInt, crate::row_codec::Value::BigInt(_)) => {
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_INT8_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            // REAL/DOUBLE の SET 値は固定長ペイロード（`SCALAR_REAL_ENTRY_LEN`／
            // `SCALAR_DOUBLE_ENTRY_LEN`）だが、BOOLEAN と同様に累計へ加算し
            // 対象行の探索より前に上限判定を完了させる（codex-review P0 指摘・
            // PR #1007。累計へ計上しないと、TEXT 列と REAL/DOUBLE 列を多数
            // 同時に SET した場合に事前検証は通過するが対象行が実在するときのみ
            // `merge_encode_scalar_columns` が `MAX_SCALAR_PAYLOAD_LEN` 超過で
            // 失敗し、未存在／他テナント所有では `UPDATE 0` の成功へ分岐して
            // 対象行の存在情報が漏れる）。
            (crate::catalog::ColumnType::Real, crate::row_codec::Value::Real(v)) => {
                if !v.is_finite() {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "REAL value must be finite".to_string(),
                    )));
                }
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_REAL_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Double, crate::row_codec::Value::Double(v)) => {
                if !v.is_finite() {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "DOUBLE PRECISION value must be finite".to_string(),
                    )));
                }
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_DOUBLE_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Boolean, crate::row_codec::Value::Bool(_)) => {
                // BOOLEAN 値は行コーデック上 1 バイト固定
                // （`row_codec::SCALAR_BOOL_ENTRY_LEN`）のため、TEXT のような
                // 長さ検証は不要（Issue #883・D-a）。
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_BOOL_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (
                crate::catalog::ColumnType::Numeric { precision, scale },
                crate::row_codec::Value::Numeric(d),
            ) => {
                // NUMERIC 値は行コーデック上 presence(1) + i128(16) の固定長
                // （`row_codec::SCALAR_NUMERIC_ENTRY_LEN`）のため、TEXT のような
                // 長さ検証は不要だが、列の scale・precision との整合は
                // ここで先取り検証する（TABLE-13〔検討中〕・TASK-197、
                // Issue #885）。
                if d.scale() != *scale || !d.fits_precision(*precision) {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "SET NUMERIC value does not match the column precision/scale".to_string(),
                    )));
                }
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_NUMERIC_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Date, crate::row_codec::Value::Date(_)) => {
                // DATE 値は行コーデック上 4 バイト固定
                // （`row_codec::SCALAR_DATE_ENTRY_LEN`）のため、TEXT のような
                // 長さ検証は不要（TABLE-13・TASK-197、Issue #884・D-3）。
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_DATE_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Timestamp, crate::row_codec::Value::Timestamp(_)) => {
                // TIMESTAMP 値は行コーデック上 8 バイト固定
                // （`row_codec::SCALAR_TIMESTAMP_ENTRY_LEN`）のため、同上の理由で
                // 長さ検証は不要（Issue #884・D-3）。
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_TIMESTAMP_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Array(array_ty), crate::row_codec::Value::Array(av)) => {
                // 配列 SET 値のフレーム長検証（対象行の探索より前に行う。
                // Issue #888・D-A3。`row_codec::scalar_array_entry_len` を
                // 実エンコード（`encode_scalar_columns`）と共有し、事前検証と
                // 実エンコードの乖離によるテナント境界漏えいを防ぐ）。
                if av.elem() != array_ty.elem() {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "SET column type does not match the current table schema".to_string(),
                    )));
                }
                if av.len() as u64 > array_ty.max_len() as u64 {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "array element count exceeds limit {}",
                        array_ty.max_len()
                    ))));
                }
                let entry_len = crate::row_codec::scalar_array_entry_len(array_ty.elem(), av)
                    .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
                set_text_payload_total =
                    set_text_payload_total
                        .checked_add(entry_len)
                        .ok_or_else(|| {
                            TenantWriteError::Catalog(CatalogError::Invalid(
                                "scalar payload length overflow".to_string(),
                            ))
                        })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Bytea, crate::row_codec::Value::Bytes(b)) => {
                // SET 値の BYTEA 長上限検証（対象行の探索より前に行う）。フレーミングは
                // TEXT と同一のため `scalar_text_entry_len` を共有する（Issue #886）。
                let byte_len = u32::try_from(b.len()).map_err(|_| {
                    TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "bytea field too long: {} bytes",
                        b.len()
                    )))
                })?;
                if byte_len > crate::bytea::MAX_BYTEA_FIELD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "bytea field length {byte_len} exceeds limit {}",
                        crate::bytea::MAX_BYTEA_FIELD_LEN
                    ))));
                }
                let entry_len = crate::row_codec::scalar_text_entry_len(byte_len)
                    .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
                set_text_payload_total =
                    set_text_payload_total
                        .checked_add(entry_len)
                        .ok_or_else(|| {
                            TenantWriteError::Catalog(CatalogError::Invalid(
                                "scalar payload length overflow".to_string(),
                            ))
                        })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (
                crate::catalog::ColumnType::Json | crate::catalog::ColumnType::Jsonb,
                crate::row_codec::Value::Json(t),
            ) => {
                // SET 値の JSON／JSONB 長上限検証（対象行の探索より前に行う）。
                // フレーミングは TEXT と同一のため `scalar_text_entry_len` を
                // 共有する（Issue #889）。構文検証・JSONB 正規化の一致検証は
                // `row_codec` の encode チョークポイント（多層防御）で行う。
                let text_len = u32::try_from(t.len()).map_err(|_| {
                    TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "json field too long: {} bytes",
                        t.len()
                    )))
                })?;
                if text_len > crate::row_codec::MAX_TEXT_FIELD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "json field length {text_len} exceeds limit {}",
                        crate::row_codec::MAX_TEXT_FIELD_LEN
                    ))));
                }
                let entry_len = crate::row_codec::scalar_text_entry_len(text_len)
                    .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
                set_text_payload_total =
                    set_text_payload_total
                        .checked_add(entry_len)
                        .ok_or_else(|| {
                            TenantWriteError::Catalog(CatalogError::Invalid(
                                "scalar payload length overflow".to_string(),
                            ))
                        })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Enum(def), crate::row_codec::Value::Enum(label)) => {
                // SET 値の語彙検証（対象行の探索より前に行う。多層防御。
                // 束縛層〔`sql::parser::bind_enum_literal`〕で既に検査済みだが、
                // Rust API から直接渡された `Value::Enum` もここで拒否する。
                // Issue #890。
                if def.validate_label(label).is_err() {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "SET value {label:?} is not a member of enum type {:?}",
                        def.name()
                    ))));
                }
                let text_len = u32::try_from(label.len()).map_err(|_| {
                    TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "enum label too long: {} bytes",
                        label.len()
                    )))
                })?;
                let entry_len = crate::row_codec::scalar_text_entry_len(text_len)
                    .map_err(|e| TenantWriteError::Catalog(CatalogError::Invalid(e.to_string())))?;
                set_text_payload_total =
                    set_text_payload_total
                        .checked_add(entry_len)
                        .ok_or_else(|| {
                            TenantWriteError::Catalog(CatalogError::Invalid(
                                "scalar payload length overflow".to_string(),
                            ))
                        })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            // 明示的な SQL `NULL`（`bind_set_assignments`〔SQL-17・SQL-19〕が
            // nullable 列向けに構築する。PR #1014 レビュー指摘対応・Issue #889。
            // `VECTOR` 列は nullable の値に関わらず常に必須として扱うため対象外
            // ——下の catch-all で従来どおり拒否する）。呼び出し元
            // （`bind_set_assignments`）は既に `column.nullable` を検査済みだが、
            // write トランザクション内で再取得したスキーマとの多層防御として
            // ここでも再検査する（`schema.columns` の型検証と同じ設計）。
            (
                crate::catalog::ColumnType::Text
                | crate::catalog::ColumnType::Integer
                | crate::catalog::ColumnType::BigInt
                | crate::catalog::ColumnType::Real
                | crate::catalog::ColumnType::Double
                | crate::catalog::ColumnType::Boolean
                | crate::catalog::ColumnType::Date
                | crate::catalog::ColumnType::Timestamp
                | crate::catalog::ColumnType::Array(_)
                | crate::catalog::ColumnType::Bytea
                | crate::catalog::ColumnType::Json
                | crate::catalog::ColumnType::Jsonb
                | crate::catalog::ColumnType::Enum(_)
                | crate::catalog::ColumnType::Numeric { .. }
                | crate::catalog::ColumnType::Uuid,
                crate::row_codec::Value::Null,
            ) if column.nullable => {}
            (crate::catalog::ColumnType::Uuid, crate::row_codec::Value::Uuid(_)) => {
                // UUID 値は行コーデック上 presence(1) + 16 バイト固定
                // （`row_codec::SCALAR_UUID_ENTRY_LEN`）のため、TEXT のような
                // 長さ検証は不要（TABLE-13〔検討中〕・TASK-197、Issue #887）。
                set_text_payload_total = set_text_payload_total
                    .checked_add(crate::row_codec::SCALAR_UUID_ENTRY_LEN)
                    .ok_or_else(|| {
                        TenantWriteError::Catalog(CatalogError::Invalid(
                            "scalar payload length overflow".to_string(),
                        ))
                    })?;
                if set_text_payload_total > crate::row_codec::MAX_SCALAR_PAYLOAD_LEN {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(format!(
                        "scalar payload length {set_text_payload_total} exceeds limit {}",
                        crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
                    ))));
                }
            }
            (crate::catalog::ColumnType::Vector(_), _)
            | (crate::catalog::ColumnType::Text, _)
            | (crate::catalog::ColumnType::Integer, _)
            | (crate::catalog::ColumnType::BigInt, _)
            | (crate::catalog::ColumnType::Real, _)
            | (crate::catalog::ColumnType::Double, _)
            | (crate::catalog::ColumnType::Boolean, _)
            | (crate::catalog::ColumnType::Date, _)
            | (crate::catalog::ColumnType::Timestamp, _)
            | (crate::catalog::ColumnType::Array(_), _)
            | (crate::catalog::ColumnType::Bytea, _)
            | (crate::catalog::ColumnType::Json, _)
            | (crate::catalog::ColumnType::Jsonb, _)
            | (crate::catalog::ColumnType::Enum(_), _)
            | (crate::catalog::ColumnType::Numeric { .. }, _)
            | (crate::catalog::ColumnType::Uuid, _) => {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "SET column type does not match the current table schema".to_string(),
                )))
            }
        }
    }
    Ok(())
}

/// 単一行 UPDATE（[`update_row_columns_unchecked`]）・述語つき UPDATE
/// （[`update_rows_where_unchecked`]）が共有する read-merge-write 本体
/// （Issue #996・SQL-19・TASK-192・RECOVER-11 ポインタ）。呼び出し元が
/// 「所有・可視」と確定済みの既存行 `existing` を受け取り、
/// [`validate_set_assignments`] 済みの `assignments` を適用した
/// embedding・metadata（再エンコード済みバイト列）を組み立てて返す。
///
/// `existing.metadata` は今回の UPDATE 要求ではなく、過去に書き込まれ済みの
/// 行データである。ここでのデコード失敗（[`crate::row_codec::scan_scalar_columns`]
/// が返すエラー）はクライアント入力の不正ではなく、ストレージ側の破損・実装
/// 不整合を示すため [`CatalogError::CorruptSchema`] へ丸める
/// （`sql/exec.rs::map_write_error` の `_` 節経由で `XX000`／
/// `SqlSurfaceError::Internal` へ写像され、detail はクライアントへ渡らない。
/// `CatalogError::Invalid` へ丸めると `22000`「UPDATE の入力が不正」という
/// 誤ったクライアントエラーになってしまう。codex-review P1 指摘・PR #989）。
/// 一方、再エンコード自体の失敗（[`crate::row_codec::merge_encode_scalar_columns`]
/// の累計上限超過等。クライアント入力である SET 値に起因し得る）は
/// [`CatalogError::Invalid`] へ丸める。
///
/// [`crate::row_codec::decode_scalar_columns`]（全 `TEXT`／`BYTEA` 列を複製）
/// ではなく借用版 [`crate::row_codec::scan_scalar_columns`] を使う
/// （codex-review P1 指摘・PR #989: 部分 UPDATE 1 回あたり「対象行の全列を
/// 複製する decode バッファ」＋「同程度を確保する encode バッファ」という
/// 2 重のピーク確保を避ける）。SET 対象でない列は
/// [`crate::row_codec::merge_encode_scalar_columns`] が借用のまま直接
/// 書き込むため、複製されるのは SET 句の値（クライアント入力）のみに抑え
/// られる。
///
/// VECTOR 列の SET があった場合に限り次元検証する（`validate_embedding_dim`
/// は VECTOR 列を持たないテーブルで常に `Err` を返すため、TEXT 列等のみの
/// UPDATE では既存 embedding を無検証のまま維持し、VECTOR 列なしテーブルを
/// 壊さない。Issue #454 の VECTOR 列なしテーブルと同じ前提）。
///
/// SET 列が重複指定された場合（[`validate_set_assignments`] より前段の
/// 束縛（`sql::parser`／NoSQL JSON 束縛）で拒否済みのため、表層からは到達
/// しない）は [`crate::row_codec::merge_encode_scalar_columns`] の仕様どおり
/// 先勝ち（`overrides` の宣言順走査で最初に一致した SET 値を採用）になる。
/// 旧・述語つき UPDATE 実装は独自ループで後勝ちだったが、本関数への統一
/// （Issue #996）により単一行 UPDATE と同じ意味論に揃った。
///
/// `pub(crate)`: `crate::constraint`（`FOREIGN KEY` の参照アクション
/// `CASCADE`／`SET NULL`／`SET DEFAULT` の適用〔Issue #1076〕）からも、子行の
/// FK 列を書き換える read-merge-write の本体として呼ばれる。
pub(crate) fn merge_row_for_update(
    schema: &crate::catalog::TableSchema,
    existing: crate::storage::Row,
    assignments: &[(usize, crate::row_codec::Value)],
) -> Result<(Vec<f32>, Vec<u8>), CatalogError> {
    let scanned = crate::row_codec::scan_scalar_columns(schema, &existing.metadata)
        .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
    let mut embedding = existing.embedding;
    let mut vector_assigned = false;
    let mut overrides: Vec<(usize, &crate::row_codec::Value)> =
        Vec::with_capacity(assignments.len());
    for (idx, value) in assignments {
        match value {
            crate::row_codec::Value::Vector(v) => {
                embedding = v.clone();
                vector_assigned = true;
            }
            other => {
                overrides.push((*idx, other));
            }
        }
    }
    if vector_assigned {
        schema.validate_embedding_dim(embedding.len())?;
    }
    let metadata = crate::row_codec::merge_encode_scalar_columns(schema, &scanned, &overrides)
        .map_err(|e| CatalogError::Invalid(e.to_string()))?;
    Ok((embedding, metadata))
}

pub(crate) fn update_row_columns_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    assignments: &[(usize, crate::row_codec::Value)],
    ledger_write: LedgerWrite<'_>,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<usize, TenantWriteError> {
    validate_identifier(table)?;
    let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    let rows_affected: usize;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "table schema changed after the update assignments were bound".to_string(),
                )));
            }
        }

        // `assignments` の列 index をスキーマ幅・型で再検証する（untrusted 経路の
        // 添字アクセス回避。coding-rust.md「受信データ経路では unwrap/expect/[]
        // を禁止」）。`bind_update` は束縛時点のスキーマで検証済みだが、write
        // トランザクション内で再取得したスキーマと不一致がありうる場合に備え、
        // `get()` で境界外アクセスを構造的に防ぐ（`insert_typed_row_unchecked` の
        // `expected_schema` 比較と多層防御）。検証本体は [`validate_set_assignments`]
        // （述語形 [`update_rows_where_unchecked`] と共有。codex-review P1 指摘・
        // PR #993 系・Issue #871）。
        validate_set_assignments(&schema, assignments)?;

        // `(スキーマ列 index, 列名, 値)` の順で構築し、ハッシュ計算前に列
        // index でソートして宣言順非依存にする（下記コメント参照）。列 index の
        // 境界・型は上記 `validate_set_assignments` で検証済みのため、ここでの
        // `get()` は理論上失敗しないが、添字アクセスを避けるため引き続き
        // `ok_or_else` で明示的に処理する（coding-rust.md）。
        let mut named_columns: Vec<(usize, &str, &crate::row_codec::Value)> =
            Vec::with_capacity(assignments.len());
        for (idx, value) in assignments {
            let column = schema.columns.get(*idx).ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "SET column index out of range for the current table schema".to_string(),
                ))
            })?;
            named_columns.push((*idx, column.name.as_str(), value));
        }
        // 台帳の内容照合ハッシュ（`content_hash::for_update_columns`）へ渡す前に
        // スキーマの列 index（宣言順）で安定ソートする（Issue #876 レビュー指摘。
        // `named_columns` はここまで `assignments`（SET 句の宣言順）の順序で
        // 構築されており、SQL 表層の `UPDATE ... SET col1=.., col2=..` はクライアント
        // が書いた宣言順をそのまま保持する一方、NoSQL 表層（`wire-server::http::
        // query::update::map_set_assignments`）は JSON `set` オブジェクトを
        // `engine::json` の `BTreeMap` でパースするためキーが常にアルファベット順へ
        // 正規化される。ハッシュが宣言順に依存したままだと、SQL 表層が非アルファ
        // ベット順で書いた `UPDATE` と同一内容の NoSQL `update`（常にアルファベット
        // 順）を同一 `operation_id` で再送した場合に、本来は同一内容の再送（`23505`・
        // TASK-101・RECOVER-10 の契約）であるべきところが内容不一致（`22023`）へ
        // 誤判定される。列の並び順は書き込み対象・SET 意味論に一切影響しない
        // （`assignments` は index 基準で適用済み）ため、ハッシュ入力のみをスキーマ
        // 列順へ正規化することで SQL・NoSQL 双方の入力順序に依存しない決定的な
        // ハッシュにする（`for_typed_insert` がスキーマ列順を渡す既存契約と同じ
        // 考え方）。
        //
        // 互換性（PR #992 レビュー指摘）: この正規化の変更前は `named_columns` を
        // 宣言順のままハッシュ計算へ渡していた（旧 `for_update_columns` 呼び出し
        // 契約。SQL 表層のみが到達可能で NoSQL `update` は未接続だった）ため、
        // 既に台帳へ記録済みのエントリは宣言順ハッシュを保持している場合がある。
        // 正規化後のコードがそれをそのまま「内容不一致」（`22023`）へ倒すと、
        // アップグレード前に記録済みの `operation_id` を同一 SQL で再送しただけの
        // 正当な操作が誤って拒否されてしまう（AGENTS.md「公開 API・エラー契約の
        // 互換性」）。宣言順のまま（ソート前）のビューを `legacy_hash` として
        // 保持しておき、`ledger::record_in_txn_accepting` が正準ハッシュ
        // （スキーマ列順）に加えてこの宣言順ハッシュとも照合することで、
        // アップグレード前に記録されたエントリへの同一 SQL 再送も
        // `Duplicate`（`23505`）と判定できるようにする。新規記録・以降の照合には
        // 常に正準ハッシュ（スキーマ列順）のみを使う（keep-first 契約は変えない）。
        let declared_order_columns: Vec<(&str, &crate::row_codec::Value)> = named_columns
            .iter()
            .map(|(_, name, value)| (*name, *value))
            .collect();
        named_columns.sort_by_key(|(idx, _, _)| *idx);
        let named_columns: Vec<(&str, &crate::row_codec::Value)> = named_columns
            .into_iter()
            .map(|(_, name, value)| (name, value))
            .collect();
        // SET 値の形状検証（上記ループ内の次元・TEXT 長上限）は、対象行の存在・
        // RLS 可視性を一切参照せずスキーマのみから判定できる。存在しない／
        // 他テナント所有／不可視な行に対する UPDATE は本来 `rows_affected: 0` の
        // 成功として区別なく扱う契約（RLS-9・RLS-10）だが、もし形状検証を
        // 対象行 lookup の**後**（`Some(row) => { ... }` 分岐内）でのみ行うと、
        // 同一の不正な SET 値でも「対象行が存在する場合は `22000` エラー」
        // 「対象行が存在しない／不可視な場合は `UPDATE 0` 成功」という応答の
        // 分岐が生じ、エラー有無そのものが行の存在を漏らす識別子となってしまう
        // （codex-review／Cursor Bugbot 指摘・PR #989。security.md「テナント境界」）。
        // 上記ループで対象行探索より前に検証を終えることで、対象の有無に関わらず
        // 常に同一の拒否（またはいずれも合格）になる fail-closed 契約を保つ。

        // 台帳照合（TASK-101・RECOVER-10）を所有権判定より**前**に行う
        // （`update_row_unchecked` と同じ順序契約。同ドキュメント参照）。単一列
        // SET は宣言順・スキーマ列順が一致するため `legacy_hash` は常に
        // `content_hash` と等しくなるが、`record_in_txn_accepting` 呼び出し自体は
        // 統一して行い分岐を増やさない。
        let content_hash = content_hash::for_update_columns(id, &named_columns)?;
        let legacy_hash = content_hash::for_update_columns(id, &declared_order_columns)?;
        ledger::record_in_txn_accepting(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
            &[legacy_hash],
        )?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // `AccessGuard` の借用はこのブロック内に閉じ込め、後続の可変借用
        // （`insert`）と衝突しないようにする（`update_row_unchecked` と同じ設計）。
        let key = (ctx.tenant_id(), id);
        // ヘッダのみを検証してから可視性を判定する（codex-review P0 再々指摘・
        // PR #989・`crates/engine/src/tenant.rs:1696`）: フル本体デコード
        // （`decode_row_for_key`。embedding・metadata を含む）を `is_visible`
        // 判定より**前**に無条件実行すると、不可視な既存行の embedding・
        // metadata が破損している場合のデコード失敗（`XX000`）が「不存在
        // （`UPDATE 0`）」と区別できてしまい、可視性の狭いセッションへ
        // 「不可視な行が存在し、かつ壊れている」ことを漏らす（security.md
        // 「テナント境界（RLS 相当）の弱体化」）。`decode_row_tenant_and_
        // visibility`（tenant_id・visibility の固定長フィールドのみを読む
        // ヘッダ専用デコード。embedding dim・metadata 長には依存しないため
        // 可視性判定そのものが内容依存にならない）で `is_owner && is_visible`
        // を先に確定し、それを満たす行だけをフル本体デコードの対象にする。
        // TABLE-12 の名前空間キー（`key = (ctx.tenant_id(), id)`）により
        // ヘッダの `tenant_id` は常に `ctx.tenant_id()` と一致する（他テナント
        // の行はこのキーでは物理的に取得できない）ため、他テナント行の混入は
        // 構造的に起こらない。
        let visible_row: Option<Row> = match row_table.get(&key).map_err(CatalogError::from)? {
            Some(guard) => {
                let raw = guard.value();
                match crate::storage::decode_row_tenant_and_visibility(raw) {
                    Ok((tenant_id, visibility))
                        if ctx.is_owner(tenant_id) && ctx.is_visible(tenant_id, visibility) =>
                    {
                        // ヘッダで所有・可視と確認済みの行のみ、フル本体
                        // （embedding・metadata）をデコードする。対象は既に
                        // 「存在し、かつ可視」と確定しているため、ここでの
                        // デコード失敗（ストレージ破損等）を `XX000` として
                        // 伝播しても、不存在の id との応答差にはならない。
                        Some(crate::storage::decode_row_for_key(
                            ctx.tenant_id(),
                            id,
                            raw,
                        )?)
                    }
                    // ヘッダのデコード自体が失敗した場合（フォーマット不整合等）、
                    // または所有・可視のいずれかを満たさない場合は区別せず
                    // 「不可視」として扱う（本体には一切触れない）。
                    _ => None,
                }
            }
            None => None,
        };

        // 判断 D 再改訂（codex-review P0 指摘・PR #989 再々指摘）: 内容依存の
        // 処理（`scan_scalar_columns`・`merge_encode_scalar_columns`。
        // `MAX_SCALAR_PAYLOAD_LEN` 超過判定・格納済みデータのデコード失敗を
        // 含む）は `is_owner && is_visible`（実際に書き込む対象）を満たす
        // 行に対してのみ実行する。`update_row_unchecked`・
        // `delete_row_unchecked` と同じ「所有・可視でない対象は探索直後に
        // 打ち切り、内容には一切触れない」設計に揃える。
        //
        // 内容依存の超過判定を対象行の有無に関わらず静的に決定することは、
        // `MAX_TEXT_FIELD_LEN` と `MAX_SCALAR_PAYLOAD_LEN` が同値である
        // 現行の列長上限設計では 2 列以上の `TEXT` 列を持つ任意のスキーマで
        // 事実上すべての部分更新を拒否する退化した契約になってしまうため
        // 採用しない。代わりに、不可視・不存在のいずれも「内容に一切触れず
        // `UPDATE 0`」で統一し、可視な既存行だけがマージ・再エンコード
        // （超過判定・デコード失敗を含む）の対象になる契約とする。これにより
        // 「可視な大きな既存行は `22000`、同じ SET 値を不可視な既存行へ送って
        // も不存在と同じ `UPDATE 0`」となり、可視・不可視の応答差は解消される
        // 一方、「不可視な既存行」と「不存在な行」はいずれも `UPDATE 0`・
        // 内容無参照で完全に同一になる。詳細は
        // `docs/design/update-single-row.md`「判断 D」参照。
        // `FOREIGN KEY` の `ON UPDATE` 参照アクション（Issue #1076）の連鎖起点。
        let mut pre_images = crate::constraint::UpdatedKeyPreImages::new();
        rows_affected = match visible_row {
            Some(row) => {
                // read-merge-write 本体は述語つき UPDATE
                // （`update_rows_where_unchecked`）と共有する [`merge_row_for_update`]
                // へ委譲済み（Issue #996。エラー分類・借用版デコードによる
                // ピーク確保回避などの設計判断は同関数のドキュメント参照）。
                //
                // クライアントは `visibility` を SET 対象にできない
                // （`sql::parser::bind_update` が `42601` で拒否済み。判断 D）。
                // 既存値をそのまま維持する。
                let visibility = row.visibility;
                // 書き換える前の全列値を pre-image として積む（`row` は直後の
                // `merge_row_for_update` へ move するため先に読む）。デコード失敗は
                // ここでは無視する（本来のデコードエラー分類は直後の
                // `merge_row_for_update` が引き続き担い、`CorruptSchema` へ正しく
                // 写像される。pre-image はベストエフォートで、無くても ON UPDATE
                // アクションが発火しないだけで事後検証は変わらず有効）。`id` 参照
                // FK は ON UPDATE で発火しない（A7）ため、主キー・UNIQUE を宣言
                // しないテーブルはこのデコードを一切行わない。
                if schema.primary_key().is_some() || !schema.unique_constraints().is_empty() {
                    if let Ok(old_values) =
                        crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                    {
                        pre_images.record(id, old_values);
                    }
                }
                let (embedding, metadata) = merge_row_for_update(&schema, row, assignments)?;
                let row_input = RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility,
                    embedding: &embedding,
                    metadata: &metadata,
                };
                let encoded = encode_row(&row_input)?;
                row_table
                    .insert(key, encoded.as_slice())
                    .map_err(CatalogError::from)?;
                1
            }
            // 対象行が不存在、またはヘッダ検査の時点で所有・可視のいずれかを
            // 満たさない（ヘッダのデコード失敗を含む）場合は区別せず
            // `UPDATE 0`（内容には一切触れない。RLS-9・RLS-10）。
            None => 0,
        };
        // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約
        // 検査。実際に書き込んだ場合（`rows_affected == 1`）のみ検査する
        // （`UPDATE 0` は行に一切触れていないため対象外。`row_table` の `mut`
        // ハンドルはこのブロックを抜けた時点で解放されるため、それより前に
        // 明示的に drop してから読み取りハンドルとして再度開く）。
        if rows_affected > 0 {
            drop(row_table);
            crate::constraint::enforce_row_constraints_in_txn(
                &write_txn,
                table,
                &schema,
                ctx.tenant_id(),
                &[id],
                crate::constraint::FkCheckMode::All,
            )?;
            // このテーブルを参照先とする `FOREIGN KEY` の参照先側（TABLE-17・
            // TASK-205、Issue #907。SET 列が主キー・UNIQUE 構成列を含む場合のみ走査）。
            let updated_columns: Vec<usize> = assignments.iter().map(|(idx, _)| *idx).collect();
            crate::constraint::enforce_referencing_rows_in_txn(
                &write_txn,
                table,
                &schema,
                ctx.tenant_id(),
                crate::constraint::ReferencedRowsChange::ColumnsUpdated(&updated_columns),
                Some(&pre_images),
                crate::constraint::FkCheckMode::All,
            )?;
        }
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(rows_affected)
}

/// `table` の既存行を 1 件削除する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// 対象行が不存在、または既存行の所有者が `ctx` と一致しない場合は
/// **区別せず** [`TenantWriteError::NotFound`]（[`update_row`] と同じ契約。
/// security.md P0）。
///
/// `operation_id` を必須引数として要求し、[`LedgerMode::Ledgered`] で内部ガードして
/// から [`delete_row_unchecked`] へ委譲する（[`insert_row`] と同じ設計。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217）。
pub fn delete_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    operation_id: &OperationId,
) -> Result<(), TenantWriteError> {
    let ledger_write = LedgerMode::Ledgered.resolve(Some(operation_id))?;
    delete_row_unchecked(storage, table, ctx, id, ledger_write)
}

/// [`delete_row`] のガードなし実体（`pub(crate)`。[`insert_row_unchecked`] と同じ
/// 設計）。呼び出し元は本モジュール内の [`delete_row`]・
/// `crate::core::EngineCore::delete_row`（`self.ledger_mode` でガード済み）。
///
/// 対象行が不存在／他テナント所有（`NotFound`）の場合は台帳への tentative
/// 追記を**破棄**する（[`DeleteNotFoundLedger::Discard`]。`write_txn` が
/// commit されないため副作用は残らない）。この「不存在と他テナント所有を
/// 区別しない」Rust API 契約は `recover4_cross_tenant_update_and_delete_are_
/// uniformly_not_found`（`tests/row_id_tenant_scope.rs`）が固定するため変更
/// しない——同テストは同一 `operation_id`（`"test-op"`）を 4 回（update ×2・
/// delete ×2）使い回しており、NotFound 時に台帳を commit すると 2 回目以降が
/// `DuplicateOperationId`／`OperationIdContentMismatch` へ変わってしまう。
///
/// SQL 表層 `DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'`
/// （SQL-18・TASK-191・#867）の唯一の到達経路は本関数ではなく
/// [`delete_row_ledgered_unchecked`]（`NotFound` でも台帳を commit する版。
/// codex-review P1 指摘・PR #983 対応。詳細は同関数のドキュメント参照）。
///
/// `ledger`（TASK-93・RECOVER-2、TASK-94・RECOVER-3、TASK-101・RECOVER-10）:
/// [`update_row_unchecked`] と同じく、台帳照合・追記を所有権判定（`owns_existing`）
/// より**前**に行う。DELETE は「対象行を消す」副作用が 1 回目の commit で完了する
/// ため、同一 `operation_id` の 2 回目以降の再送は対象行が既に不存在
/// （`owns_existing == false`）になっている。所有権判定を先に見て `NotFound` を
/// 返すと、この正当な重複再送がハッシュ一致による再送検知（`DuplicateOperationId`・
/// `23505`）ではなく `NotFound` として観測され、RECOVER-3 の「同一 `operation_id` の
/// 2 回目以降は重複として拒否する」契約を壊す（codex-review P1 指摘・PR #247）。
pub(crate) fn delete_row_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    ledger_write: LedgerWrite<'_>,
) -> Result<(), TenantWriteError> {
    match delete_row_impl(
        storage,
        table,
        ctx,
        id,
        ledger_write,
        DeleteNotFoundLedger::Discard,
        None,
    )?
    .0
    {
        DeleteRowOutcome::Deleted => Ok(()),
        DeleteRowOutcome::NotFound => Err(TenantWriteError::NotFound),
    }
}

/// [`delete_row_impl`] の成功時の結果（対象行を実際に削除できたか）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteRowOutcome {
    /// 対象行を削除した（テーブル世代も進行させた）。
    Deleted,
    /// 対象行が不存在、または他テナント所有だった（`owns_existing == false`）。
    NotFound,
}

/// [`delete_row_impl`] が `NotFound`（`owns_existing == false`）の場合に
/// 台帳への tentative 追記をどう扱うかの選択（codex-review P1 指摘・PR #983。
/// 元は [`delete_row_unchecked`] 1 本だったが、SQL 表層専用の
/// [`delete_row_ledgered_unchecked`] を追加する際に分岐を抽出した）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteNotFoundLedger {
    /// 台帳追記を破棄する（`write_txn` を commit しない）。[`delete_row_unchecked`]
    /// （Rust API・`recover4_*` テストが固定する既存契約）が使う。
    Discard,
    /// 台帳追記を commit する（テーブル世代は進行させない）。
    /// [`delete_row_ledgered_unchecked`]（SQL 表層専用）が使う。
    Record,
}

/// `RETURNING` 句（Issue #873・SQL-21）向けに、削除**前**の行内容を捕捉した
/// 結果（`delete_row_impl` の `capture` が `Some` かつ対象行を実際に削除した
/// 場合のみ `Some`）。`sql::exec::execute_delete_returning` がこれを RLS 再判定
/// （`PolicyContext::is_visible`）・投影へ渡す。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CapturedRow {
    pub id: u64,
    pub tenant_id: String,
    pub visibility: crate::storage::Visibility,
    pub values: Vec<crate::row_codec::Value>,
}

/// [`delete_row_unchecked`]・[`delete_row_ledgered_unchecked`] が共有する実体。
/// `not_found_ledger` で `NotFound` 時の台帳追記の扱いのみを分岐する
/// （それ以外の判定順序・二重防御はいずれのモードでも同一）。
///
/// `ledger`（TASK-93・RECOVER-2、TASK-94・RECOVER-3、TASK-101・RECOVER-10）:
/// 台帳照合・追記を所有権判定（`owns_existing`）より**前**に行う（両モード共通。
/// [`delete_row_unchecked`]・[`delete_row_ledgered_unchecked`] のドキュメント
/// 参照）。
///
/// [`DeleteCapture::project`] の関数型（clippy::type_complexity 対応で型別名化）。
type ReturningProjectFn<'a> = dyn FnMut(&CapturedRow) -> Result<(), TenantWriteError> + 'a;

/// `delete_row_impl` の `capture` 引数（Issue #991・clippy::too_many_arguments
/// 対応で `capture`・`project` の 2 引数を 1 引数へ集約。両者は常に一緒に使う
/// ——`project` は `capture` が捕捉した行にしか適用できないため）。
struct DeleteCapture<'a> {
    /// [`delete_row_impl`] の `capture` ドキュメント参照（束縛時スキーマ）。
    schema: &'a crate::catalog::TableSchema,
    /// [`delete_row_impl`] の `project` ドキュメント参照（commit 前フック）。
    /// `None` は「捕捉のみ行い投影しない」（現状の呼び出し元では未使用だが、
    /// `capture: Some` かつ `project: None` を表現できるよう分離しておく）。
    project: Option<&'a mut ReturningProjectFn<'a>>,
}

/// [`delete_row_unchecked`]・[`delete_row_ledgered_unchecked`] が共有する実体。
/// `not_found_ledger` で `NotFound` 時の台帳追記の扱いのみを分岐する
/// （それ以外の判定順序・二重防御はいずれのモードでも同一）。
///
/// `ledger`（TASK-93・RECOVER-2、TASK-94・RECOVER-3、TASK-101・RECOVER-10）:
/// 台帳照合・追記を所有権判定（`owns_existing`）より**前**に行う（両モード共通。
/// [`delete_row_unchecked`]・[`delete_row_ledgered_unchecked`] のドキュメント
/// 参照）。
///
/// `capture`（Issue #873・SQL-21。`RETURNING` 句）: `Some(DeleteCapture {
/// schema, .. })` の場合のみ、`row_table.remove` の**直前**に対象行を完全
/// デコードして [`CapturedRow`] として返す（削除前の値）。`schema` は
/// [`insert_typed_row_unchecked`] の `expected_schema` と同じ TOCTOU 対策——
/// 呼び出し元が束縛した時点のスキーマと、本関数が write トランザクション内で
/// 改めて取得したスキーマが不一致なら `CatalogError::Invalid`（`22000`）で
/// 拒否する。`None`（既存の 2 呼び出し元）では捕捉を一切行わずビット同一の
/// 挙動を保つ。
///
/// `capture.project`（Issue #991・codex-review P1 指摘対応）: 実際に行を
/// 捕捉できた場合（対象行を削除できた場合のみ）、**`bump_table_generation_in_txn`・
/// `commit_boundary::commit` の呼び出しより前**に 1 回だけ呼ぶコールバック。
/// `RETURNING` 行の投影（`sql::returning::project_row`。文字列・ベクトルの
/// `try_reserve_exact` 失敗や結果容量超過で失敗しうる）を、行削除・台帳追記の
/// **commit 成功境界の内側**で行わせるための注入点——`Err` を返すと本関数は
/// その `Err`（[`TenantWriteError::ReturningProjectionFailed`]／
/// [`TenantWriteError::ReturningProjectionTooLarge`] を想定）をそのまま伝播し、
/// `write_txn` は commit されずに drop（abort）される。これにより「DELETE は
/// 失敗応答なのに行は既に永続化されている」という commit 成功境界違反
/// （RECOVER-5・RECOVER-6・TASK-96/97 の一貫性契約）を防ぐ——`sql::returning`
/// 側（呼び出し元）が確定させた列メタデータ（`column_meta`）は redb I/O を
/// 伴わない純粋計算のため書き込み**前**に呼べるが（[`crate::sql::exec::
/// execute_delete_returning`] 参照）、行の値そのもの（`CapturedRow::values`）は
/// この write トランザクション内でしか得られないため、投影自体を同じ
/// トランザクション内・commit 前に実行する必要がある。`None`（既存呼び出し元）
/// では一切呼ばずビット同一の挙動を保つ。
fn delete_row_impl(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    ledger_write: LedgerWrite<'_>,
    not_found_ledger: DeleteNotFoundLedger,
    mut capture: Option<DeleteCapture<'_>>,
) -> Result<(DeleteRowOutcome, Option<CapturedRow>), TenantWriteError> {
    validate_identifier(table)?;
    let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    let mut captured_row: Option<CapturedRow> = None;
    // `ON DELETE` 参照アクション（Issue #1076 A14）の連鎖起点。削除された行が
    // `FOREIGN KEY` の参照先キー（主キー・UNIQUE 構成列）を持ちうる場合のみ、
    // `remove` の直前に読み取った旧値を積む——`collect_action_targets` の
    // `Removed` 分岐がこれを使い、「今回削除で実際に失われたキー」のみを
    // CASCADE 対象へ限定する（`present_keys` を全走査してその時点で親に無い
    // キー全体を対象にすると、`INITIALLY DEFERRED` の `FOREIGN KEY` が同一
    // トランザクション内で許す一時的な合法孤立行まで誤って連鎖削除してしまう。
    // codex-review 指摘）。主キー・UNIQUE を宣言しないテーブルは FK の参照先に
    // なり得ないため記録コストを払わない（ON UPDATE 側の `pre_images.record`
    // と同じ判定条件）。
    let needs_fk_removed_pre_image;
    let mut removed_pre_images = crate::constraint::UpdatedKeyPreImages::new();
    let (owns_existing, schema) = {
        // 次元検証は不要だが、テーブル不存在の判定・並行 DDL との整合のため
        // `insert_row`/`update_row` と同じ前段を通す。
        let schema = require_table_schema_write(&write_txn, table)?;
        needs_fk_removed_pre_image =
            schema.primary_key().is_some() || !schema.unique_constraints().is_empty();
        if let Some(expected) = capture.as_ref() {
            if expected.schema != &schema {
                return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                    "table schema changed after the delete was bound".to_string(),
                )));
            }
        }
        // 削除要求のクライアント由来の内容は id のみ（`content_hash::for_delete`
        // ドキュメント参照）。
        let content_hash = content_hash::for_delete(id);
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        // `update_row` と同じく `(tenant_id, id)` キー（TABLE-12）＋ `is_owner` の二重防御。
        let key = (ctx.tenant_id(), id);
        let owns_existing = match row_table.get(&key).map_err(CatalogError::from)? {
            Some(guard) => {
                let (existing_tenant, _existing_visibility) =
                    decode_row_tenant_and_visibility(guard.value())?;
                let owns = ctx.is_owner(existing_tenant);
                if owns && (capture.is_some() || needs_fk_removed_pre_image) {
                    // `remove` の直前・同一 `guard` 生存期間内にフルデコードする
                    // （削除後では対象バイト列が失われるため）。物理行フォーマット
                    // は `storage.rs::decode_row`（`ROW_FORMAT_VERSION`。tenant_id・
                    // visibility・embedding・不透明な `metadata` バイト列を持つ）で
                    // あり、`row_codec::decode_row`（別バージョン・別フォーマット。
                    // 本モジュールの通常の書き込み経路では使われない）ではない。
                    // `metadata` は `row_codec::decode_scalar_columns` で
                    // `schema.columns` 順の `Value` 列へ変換する（`VECTOR` 列の位置は
                    // 常に `Value::Null` を返す契約——`sql::scan` の同じ規約参照）
                    // ため、`VECTOR` 列位置だけは `Row::embedding` を明示的に
                    // 差し替える（`dim == 0`／embedding 空は列が NULL である
                    // 既存契約のため差し替えない）。`StorageError`／`RowCodecError`
                    // は「既に永続化された行のデコード失敗」＝クライアント入力の
                    // 不正ではなくサーバー内部事象として扱う（codex-review P1・
                    // Bugbot 指摘・PR #991: 汎用の `Storage`/`Catalog(Invalid)` を
                    // 共用すると `map_insert_write_error` が `22000`（クライアント
                    // 入力不正）へ丸めてしまうため、専用 variant
                    // `CapturedRowDecodeFailed` で `XX000` に固定する）。
                    let row = crate::storage::decode_row(id, guard.value()).map_err(|e| {
                        TenantWriteError::CapturedRowDecodeFailed(format!(
                            "captured row decode failed: {e}"
                        ))
                    })?;
                    let mut values =
                        crate::row_codec::decode_scalar_columns(&schema, &row.metadata).map_err(
                            |e| {
                                TenantWriteError::CapturedRowDecodeFailed(format!(
                                    "captured row decode failed: {e}"
                                ))
                            },
                        )?;
                    if !row.embedding.is_empty() {
                        if let Some(vec_idx) = schema
                            .columns
                            .iter()
                            .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)))
                        {
                            if let Some(slot) = values.get_mut(vec_idx) {
                                *slot = crate::row_codec::Value::Vector(row.embedding);
                            }
                        }
                    }
                    if needs_fk_removed_pre_image {
                        removed_pre_images.record(id, values.clone());
                    }
                    if capture.is_some() {
                        captured_row = Some(CapturedRow {
                            id,
                            tenant_id: row.tenant_id,
                            visibility: row.visibility,
                            values,
                        });
                    }
                }
                owns
            }
            None => false,
        };
        if owns_existing {
            row_table.remove(&key).map_err(CatalogError::from)?;
        } else if not_found_ledger == DeleteNotFoundLedger::Discard {
            // 台帳への tentative 追記はこの早期 `return` により `write_txn` が
            // commit されず破棄されるため、副作用として残らない
            // （[`delete_row_unchecked`] のドキュメント参照）。
            return Err(TenantWriteError::NotFound);
        }
        (owns_existing, schema)
    };
    // このテーブルを参照先とする `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）の
    // 参照先側の検査。行を実際に削除した場合のみ行う——対象行が不存在・他テナント
    // 所有（いずれも区別しない `NotFound`）の場合は何も変化していないため走査
    // 自体を行わず、他テナントの行の有無で処理経路が分岐しない（RLS-9・RLS-10）。
    if owns_existing {
        crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            crate::constraint::ReferencedRowsChange::Removed,
            needs_fk_removed_pre_image.then_some(&removed_pre_images),
            crate::constraint::FkCheckMode::All,
        )?;
    }
    // `project` は commit **前**・`row_table`（可変借用）が上記ブロックの終端で
    // 既に解放された後に呼ぶ（`delete_row_impl` ドキュメントの `project` 節参照）。
    // `captured_row` が `Some` になるのは `owns_existing && capture.is_some()` の
    // 場合のみ（上記ブロック参照）であり、`project` が `Some` でも対象行を実際に
    // 削除できなかった場合（`NotFound`）は呼ばない——`RETURNING` は削除できた行
    // のみを返す契約（[`crate::sql::exec::execute_delete_returning`] ドキュメント
    // 参照）。
    if let (Some(project), Some(captured)) = (
        capture.as_mut().and_then(|c| c.project.as_mut()),
        captured_row.as_ref(),
    ) {
        project(captured)?;
    }
    if owns_existing {
        crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    }
    // `Record` モードで `owns_existing == false` の場合もここへ到達し commit
    // する（台帳の tentative 追記のみを確定させる。テーブル世代は進行させない
    // ため既存のキャッシュ失効契約に影響しない）。
    crate::recovery::commit_boundary::commit(write_txn)?;
    let outcome = if owns_existing {
        DeleteRowOutcome::Deleted
    } else {
        DeleteRowOutcome::NotFound
    };
    Ok((outcome, captured_row))
}

/// SQL 表層 `DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'`
/// （SQL-18・TASK-191・#867）の唯一の到達経路 `crate::sql::exec::execute_delete`
/// （`core.rs::EngineCore::execute_delete_sql`／`execute_sql_in_session` の
/// `DELETE` 分岐から呼ばれる）専用のガードなし実体。
///
/// [`delete_row_unchecked`] との違いは `NotFound`（対象行が不存在／他テナント
/// 所有。いずれも区別しない）の場合の台帳の扱いのみ: 本関数は `NotFound` でも
/// 台帳への tentative 追記を同一トランザクションで **commit する**
/// （[`truncate_table_unchecked`] の「0 件でも必ず台帳記録」契約と同じ考え方。
/// テーブル世代は進行させない——対象行が変化していないため既存のキャッシュ
/// 失効契約はそのまま維持する）。
///
/// この commit により、`execute_delete` が `NotFound` を `0` 行成功へ写像した
/// 後でも、同一 `operation_id` の再送は台帳照合（TASK-101・RECOVER-10）で
/// `DuplicateOperationId`（同一 `id` への再送・`23505`）／
/// `OperationIdContentMismatch`（異なる `id` への再送・`22023`）のいずれかへ
/// 確定的に収束する。旧実装（[`delete_row_unchecked`] を流用し `NotFound` は
/// 台帳を commit しない設計）では、0 行 DELETE の `operation_id` を台帳が一切
/// 覚えていなかったため、後から対象 `id` が INSERT され、通信断等で同じ
/// `operation_id` が再送されると、2 回目は実際に行が存在し実削除が発生して
/// しまう再送安全性違反があった（codex-review P1 指摘・PR #983。
/// `docs/design/sql-delete-single-row.md`「台帳記録（NotFound を含む。#983 で
/// 修正）」節参照）。
///
/// [`delete_row_unchecked`]（Rust API・`recover4_*` テストが固定する既存契約。
/// `NotFound` は台帳を commit しない）とは意図的に別関数とし、その呼び出し元
/// （`delete_row`・`EngineCore::delete_row`）の挙動は変更しない。
pub(crate) fn delete_row_ledgered_unchecked(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    ledger_write: LedgerWrite<'_>,
) -> Result<DeleteRowOutcome, TenantWriteError> {
    delete_row_ledgered_capturing_unchecked(storage, table, ctx, id, ledger_write, None, None)
        .map(|(outcome, _)| outcome)
}

/// [`delete_row_ledgered_unchecked`] の捕捉版（Issue #873・SQL-21。`RETURNING`
/// 句）。`capture`（束縛時スキーマ）が `Some` の場合のみ削除前の行内容を
/// [`CapturedRow`] として返す（[`delete_row_impl`] の `capture` ドキュメント
/// 参照）。`project`（Issue #991）は [`delete_row_impl`] の同名引数へそのまま
/// 委譲する（commit 前・同一トランザクション内で `RETURNING` 行を投影させる
/// ための注入点。`delete_row_impl` ドキュメント参照）。`sql::exec::
/// execute_delete_returning` の唯一の到達経路。`capture: None`・`project: None`
/// を渡すと [`delete_row_ledgered_unchecked`] とビット同一。
pub(crate) fn delete_row_ledgered_capturing_unchecked<'a>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    ledger_write: LedgerWrite<'_>,
    capture: Option<&'a crate::catalog::TableSchema>,
    project: Option<&'a mut ReturningProjectFn<'a>>,
) -> Result<(DeleteRowOutcome, Option<CapturedRow>), TenantWriteError> {
    delete_row_impl(
        storage,
        table,
        ctx,
        id,
        ledger_write,
        DeleteNotFoundLedger::Record,
        capture.map(|schema| DeleteCapture { schema, project }),
    )
}

/// SQL 表層 `TRUNCATE TABLE <table> USING OPERATION_ID '<id>'`（SQL-22、TASK-195）
/// の実体。テーブル定義（カタログ）は残したまま、セッションのテナントが所有する
/// 全行（`Visibility` を問わない）を単一 write トランザクションで削除する
/// （[`delete_row_unchecked`]・[`replace_typed_rows_by_text_key`] と並ぶ 3 つ目の
/// 削除系プリミティブ）。唯一の到達経路は `sql::exec::execute_truncate`
/// （`operation_id` 必須化ガードを適用済み）。
///
/// - 削除スコープは「RLS 可視集合（`is_visible`）」ではなく「テナント所有
///   （`(tenant_id, id)` の物理キー名前空間。TABLE-12）」全体。`Public`／`Private`
///   を問わず自テナント行はすべて削除対象になる（SQL-22 の要件。RLS-7 が既定で
///   適用する可視性フィルタとは独立の削除スコープ判断であり、意図的に緩めていない）。
/// - `row_table.retain_in` の述語（`bool` 返却・panic 不可）内で行ヘッダを
///   デコードして所有権を再チェックしない。`retain_in` の範囲引数
///   `(tenant_id, 0)..=(tenant_id, u64::MAX)`（`(tenant_id, &str)` 辞書順の
///   キー名前空間分離。TABLE-12）自体が唯一の境界であり、フォールブルな
///   デコード失敗を `retain_in` 内から `Err` として伝播する手段がないため
///   （panic はトランザクション破損＝coding-rust.md 違反）、レンジそのものを
///   境界とする設計が安全側かつ [`replace_typed_rows_by_text_key`] のテナント
///   名前空間走査と同じ既存規約に整合する。
/// - `MAX_SCANNED_ROWS`／`MAX_VISIBLE_ROWS` は適用しない（SQL-22 は TRUNCATE を
///   1 文あたり影響行数上限〔SQL-19〕の対象外とする。`retain_in` は候補 id を
///   `Vec` へ materialize しないため、DoS 上限が本来的に不要な操作でもある）。
/// - 台帳記録を行削除より**先**に行う（[`delete_row_unchecked`] と同じ理由。
///   再送時に `23505`／`22023` を正しく検出するため）。
/// - 削除対象が 0 件でも、台帳記録が必ず発生する（write トランザクションが
///   commit される）ため、常にテーブル世代を進める（`insert_rows` の空バッチ
///   ショートカットとは意図的に非対称。0 件 TRUNCATE の再送冪等性を台帳側だけで
///   保証し、コード分岐の非対称性によるバグを避ける）。
///
/// 述語つき `UPDATE`／`DELETE ... WHERE`（SQL-19・TASK-192、Issue #871・対象
/// ビヘイビア: RECOVER-11）の実行結果。呼び出し元（`sql::exec::
/// execute_predicate_delete`／`execute_predicate_update`）はこの enum を
/// `wire_code` へ写像する（`Applied` は `DELETE n`／`UPDATE n`、
/// `LimitExceeded` は `54000`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredicateDmlOutcome {
    /// 影響行数（`0` を含む）。write トランザクションは commit 済み
    /// （台帳エントリも commit されている）。
    Applied { rows_affected: usize },
    /// 一致行数が上限を超えた（`limit` が `Some` のときのみ発生し、`limit + 1`
    /// 件目で列挙を打ち切った時点の件数をそのまま運ぶ。呼び出し元が
    /// `check_dml_affected_rows_with_limit`（UPDATE／DELETE 共通の唯一の上限
    /// 判定 API。Issue #997）へ渡す。`limit` が `None`〔既定。オーナー判断の
    /// 改訂 2026-09-27〕の場合は本 variant に到達せず、`MAX_SCANNED_ROWS` の
    /// 総走査上限のみが効く）。write トランザクションは commit されず、行・
    /// 台帳エントリのいずれにも痕跡が残らない。
    LimitExceeded { count: usize },
}

/// [`delete_rows_where_unchecked`]／[`update_rows_where_unchecked`] のエラー。
/// テナント境界・台帳照合由来のエラー（[`TenantWriteError`]）と、呼び出し元が
/// 注入する述語クロージャ由来のエラー（`E`）を型で区別する。本モジュールは
/// `sql` 型（`SqlSurfaceError` 等）に依存しないため、`E` はジェネリックのまま
/// 運ぶ（`sql::exec` 側が `SqlSurfaceError` として具体化する）。
#[derive(Debug)]
pub(crate) enum PredicateDmlError<E> {
    Write(TenantWriteError),
    Predicate(E),
}

fn dml_write_err<E>(e: impl Into<TenantWriteError>) -> PredicateDmlError<E> {
    PredicateDmlError::Write(e.into())
}

/// 候補行 1 件分の借用ビュー（述語クロージャへ渡す入力。行データを複製しない）。
/// `embedding` は呼び出し元が `needs_embedding = false` を渡した場合は常に空
/// スライス（`WHERE` が embedding を参照しない場合、デコードコストを避ける。
/// `sql/scan.rs` の `DecodeTier` と同じ判断）。
pub(crate) struct DmlCandidate<'a> {
    pub id: u64,
    pub dim: u32,
    pub embedding: &'a [f32],
    pub metadata: &'a [u8],
}

/// [`delete_rows_where_unchecked`]／[`update_rows_where_unchecked`] が共有する
/// 候補行列挙本体。対象スコープはテナント**所有**（RLS 可視性フィルタではな
/// く、単一行 DELETE・TRUNCATE と同じテナント所有スコープ——
/// `docs/design/predicate-dml-exec.md`「削除・更新スコープ」参照）。
///
/// 物理キーは `(tenant_id, id)`（TABLE-12）であり、redb のタプル `Key` 実装
/// は第 1 要素（`tenant_id`）を主キーとして辞書順比較するため、同一テナント
/// の行は物理キー空間上で連続領域を成す（`catalog.rs::scan_table_page` の
/// カーソルが `(tenant_id, id)` 順で全テナントを跨いで前進できることと同じ
/// 事実）。この性質を利用し、走査は `row_table.range` で対象テナントの
/// 先頭 `(tenant, 0)` から開始し、キーのテナントが変わった時点（＝対象テナ
/// ントの連続領域を抜けた時点）で打ち切る（他テナント領域には触れない。
/// codex-review P0 指摘・Issue #871: 総走査上限のカウンタを `is_owner` 判定
/// より前に加算していたため、同一物理テーブルに他テナントの行が大量に存在
/// すると対象テナントの行が少なくても上限超過で拒否され、他テナントの行数
/// を応答から推測できてしまっていた）。`ctx.is_owner` は物理走査を離れて
/// 他テナント領域まで読み進めることがなくなった後も、`verify_row_key_tenant`
/// が保証するキー↔ヘッダ整合の帰結として常に真になる不変条件を defense-in-
/// depth として明示検査する（TABLE-12・security.md）。`predicate` が真を
/// 返した行の `id` を `limit` が `Some` の場合のみ `limit + 1` 件に達するまで
/// `Vec` へ蓄積する（早期終了。行データそのものは複製せず `id` のみを保持
/// する）。`limit` が `None`（Issue #997・オーナー判断の改訂〔汎用 RDB 整合〕
/// で一致行数上限を CLI 未指定＝無効にした既定状態）の場合はこの早期打ち切り
/// を行わず、[`MAX_SCANNED_ROWS`] による総走査上限のみで頭打ちにする。
///
/// 総走査行数上限（[`MAX_SCANNED_ROWS`]。対象テナント所有行のみを 1 行デコ
/// ードするたびに加算する）は [`visible_rows`] と同じ計算量 DoS 対策
/// （codex-review P1 指摘・Issue #871）で、`limit` の設定有無に関わらず常に
/// 適用する。`limit`（一致行数上限。`Some` のときのみ）は述語に一致した行に
/// しか効かないため、一致しない述語では上限に到達しないまま任意規模の走査を
/// 繰り返せてしまう経路を、この独立した総走査上限で塞ぐ。走査が対象テナント
/// の名前空間内に限定された結果、この上限は他テナントのデータ量に一切
/// 依存しない（テナント境界越しの情報漏えいを構造的に排除）。超過時は
/// [`TenantWriteError::TooManyRowsScanned`] で部分結果を返さず fail-closed に
/// 拒否し、呼び出し元が `write_txn` を commit せず破棄することで副作用ゼロを
/// 保つ。
///
/// `predicate` が `Err(e)` を返した場合はその時点で呼び出し元へ伝播する
/// （呼び出し元が `write_txn` を破棄することで副作用ゼロを保つ）。
fn enumerate_dml_candidates<E>(
    row_table: &redb::Table<'_, (&'static str, u64), &'static [u8]>,
    ctx: &PolicyContext,
    needs_embedding: bool,
    limit: Option<std::num::NonZeroUsize>,
    predicate: &mut impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<Vec<u64>, PredicateDmlError<E>> {
    let mut candidate_ids: Vec<u64> = Vec::new();
    let mut embedding_scratch: Vec<f32> = Vec::new();
    // 総走査行数（対象テナント所有行のみを対象に加算する）。`visible_rows` の
    // `MAX_SCANNED_ROWS` と同じ計算量 DoS 対策（codex-review P1 指摘・Issue #871）:
    // 一致行数の上限（`limit`）は述語に一致した行にしか効かないため、一致しない
    // 述語では上限に到達しないまま任意規模の走査を繰り返せてしまう。
    let mut scanned: usize = 0;

    let tenant = ctx.tenant_id();
    // 対象テナントの名前空間 `(tenant, 0)..=(tenant, u64::MAX)` に走査を閉じる
    // （codex-review P0 指摘・Issue #871）。物理キーは `(tenant_id, id)` の
    // 辞書順であり `u64::MIN == 0`／`u64::MAX` が対象テナントの id 空間の
    // 両端を覆うため、この閉区間は対象テナント所有行のみを列挙し他テナント
    // 領域のキー・値には一切触れない（`Bound::Unbounded` 終端だと対象テナント
    // に行が 0 件の場合に限り最初の反復で辞書順で後続する別テナントの先頭
    // エントリを取得してしまい、下記の break 前に他テナント領域を読んでいた）。
    // `replace_rows_for_reingest`（2938 行目付近）と同型の閉区間。
    let range_start = std::ops::Bound::Included((tenant, 0u64));
    let range_end = std::ops::Bound::Included((tenant, u64::MAX));
    for entry in row_table
        .range::<(&str, u64)>((range_start, range_end))
        .map_err(|e| dml_write_err(CatalogError::from(e)))?
    {
        let (k, v) = entry.map_err(|e| dml_write_err(CatalogError::from(e)))?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant {
            // 閉区間により理論上到達しないが、defense-in-depth として維持する
            // （物理キー比較の実装詳細に依存しない不変条件の二重化）。
            break;
        }

        scanned = scanned.saturating_add(1);
        if scanned > MAX_SCANNED_ROWS {
            // 部分結果を返さず fail-closed に拒否する（`visible_rows` と同じ判断）。
            // 呼び出し元（`delete_rows_where_unchecked`／`update_rows_where_unchecked`）
            // は `write_txn` を commit せず破棄するため、行・台帳のいずれにも
            // 痕跡が残らない。
            return Err(dml_write_err(TenantWriteError::TooManyRowsScanned));
        }
        let buf = v.value();

        let (row_tenant, _visibility, offset) =
            crate::storage::decode_row_header(buf).map_err(|e| dml_write_err(e))?;
        crate::storage::verify_row_key_tenant(key_tenant, row_tenant)
            .map_err(|e| dml_write_err(e))?;
        // `range` の走査範囲を対象テナントの物理キー領域に限定した結果として
        // 常に真になる不変条件を defense-in-depth で明示検査する（テナント
        // **所有**スコープ。RLS 可視性フィルタではない。TRUNCATE・単一行
        // DELETE と同じ判断。`docs/design/predicate-dml-exec.md` 参照）。
        if !ctx.is_owner(row_tenant) {
            continue;
        }

        let (dim, metadata): (u32, &[u8]) = if needs_embedding {
            crate::storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                .map_err(|e| dml_write_err(e))?
        } else {
            crate::storage::decode_row_dim_and_metadata_borrowed(buf)
                .map_err(|e| dml_write_err(e))?
        };
        let embedding: &[f32] = if needs_embedding {
            embedding_scratch.as_slice()
        } else {
            &[]
        };

        let candidate = DmlCandidate {
            id,
            dim,
            embedding,
            metadata,
        };
        if predicate(&candidate).map_err(PredicateDmlError::Predicate)? {
            candidate_ids.push(id);
            // `limit` が `Some` のときのみ `limit + 1` 件に達した時点で打ち切る
            // （副作用ゼロで `54000` を返すために、呼び出し元が超過を判定できる
            // 最小限の 1 件超過分だけ余分に蓄積する。ADR `docs/design/
            // multi-row-dml-operation-id.md` §6「3.」）。`None`（上限無効。
            // Issue #997 オーナー判断の改訂）の場合は打ち切らず、
            // `MAX_SCANNED_ROWS` の総走査上限のみで頭打ちにする。
            if let Some(limit) = limit {
                if candidate_ids.len() > limit.get() {
                    break;
                }
            }
        }
    }
    Ok(candidate_ids)
}

/// 述語つき `DELETE FROM <table> WHERE ... USING OPERATION_ID '<id>'`
/// （SQL-19・TASK-192、Issue #871）の実体。唯一の到達経路は
/// `sql::exec::execute_predicate_delete`。
///
/// 処理順序（ADR `docs/design/multi-row-dml-operation-id.md` §6）:
/// 1. `begin_write_txn` → スキーマ取得（`expected_schema` があれば束縛時点の
///    スキーマと一致するか照合し、並行 `ALTER TABLE` を検知する。
///    `upsert_typed_rows_unchecked` と同じ判断）。
/// 2. `ledger::record_in_txn`（候補列挙より**先**。使用済み `operation_id` は
///    可視集合を一切走査せず `23505`／`22023` へ短絡する）。
/// 3. [`enumerate_dml_candidates`] で候補 `id` を確定する。
/// 4. `limit` を超えていれば `write_txn` を drop し
///    [`PredicateDmlOutcome::LimitExceeded`] を返す（行・台帳とも痕跡ゼロ）。
/// 5. 候補 `id` をすべて `remove`。
/// 6. 影響行数が 1 件以上のときのみ `bump_table_generation_in_txn`。
/// 7. `commit_boundary::commit`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn delete_rows_where_unchecked<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    content_hash_value: &content_hash::ContentHash,
    expected_schema: Option<&crate::catalog::TableSchema>,
    needs_embedding: bool,
    limit: Option<std::num::NonZeroUsize>,
    mut predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<PredicateDmlOutcome, PredicateDmlError<E>> {
    validate_identifier(table).map_err(dml_write_err)?;
    let write_txn = storage
        .begin_write_txn()
        .map_err(|e| dml_write_err(convert_write_txn_err(e)))?;

    let (candidate_ids, schema) = {
        let schema = require_table_schema_write(&write_txn, table).map_err(dml_write_err)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(dml_write_err(CatalogError::Invalid(
                    "table schema changed after the statement was bound".to_string(),
                )));
            }
        }

        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            content_hash_value,
        )
        .map_err(dml_write_err)?;

        let row_table_name = user_rows_table_name(table);
        let row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(|e| dml_write_err(map_row_table_error(e)))?;
        let candidate_ids =
            enumerate_dml_candidates(&row_table, ctx, needs_embedding, limit, &mut predicate)?;
        (candidate_ids, schema)
    };

    if limit.is_some_and(|limit| candidate_ids.len() > limit.get()) {
        // `write_txn` をここで drop する（commit しない）。台帳の tentative
        // 追記・行変更のいずれも痕跡が残らない（ADR §6「4.」）。
        return Ok(PredicateDmlOutcome::LimitExceeded {
            count: candidate_ids.len(),
        });
    }

    // `ON DELETE` 参照アクション（Issue #1076 A14）の連鎖起点。`delete_row_impl`
    // と同じ判定条件・同じ理由（`removed_pre_images` のドキュメント参照）で、
    // 主キー・UNIQUE を宣言するテーブルのみ `remove` の戻り値（削除前の物理行）
    // から旧値を復元して積む。
    let needs_fk_removed_pre_image =
        schema.primary_key().is_some() || !schema.unique_constraints().is_empty();
    let mut removed_pre_images = crate::constraint::UpdatedKeyPreImages::new();
    {
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(|e| dml_write_err(map_row_table_error(e)))?;
        for id in &candidate_ids {
            let key = (ctx.tenant_id(), *id);
            let removed_guard = row_table
                .remove(&key)
                .map_err(|e| dml_write_err(CatalogError::from(e)))?;
            if needs_fk_removed_pre_image {
                if let Some(guard) = removed_guard {
                    let row = crate::storage::decode_row(*id, guard.value()).map_err(|e| {
                        dml_write_err(TenantWriteError::CapturedRowDecodeFailed(format!(
                            "removed row decode failed: {e}"
                        )))
                    })?;
                    let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                        .map_err(|e| {
                            dml_write_err(TenantWriteError::CapturedRowDecodeFailed(format!(
                                "removed row decode failed: {e}"
                            )))
                        })?;
                    removed_pre_images.record(*id, values);
                }
            }
        }
    }

    // このテーブルを参照先とする `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）の
    // 参照先側の検査（行を 1 件以上削除した場合のみ。候補は自テナント所有の行に
    // 限られる）。
    if !candidate_ids.is_empty() {
        crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            crate::constraint::ReferencedRowsChange::Removed,
            needs_fk_removed_pre_image.then_some(&removed_pre_images),
            crate::constraint::FkCheckMode::All,
        )
        .map_err(dml_write_err)?;
    }

    let rows_affected = candidate_ids.len();
    if rows_affected > 0 {
        crate::catalog::bump_table_generation_in_txn(&write_txn, table).map_err(dml_write_err)?;
    }
    crate::recovery::commit_boundary::commit(write_txn).map_err(dml_write_err)?;
    Ok(PredicateDmlOutcome::Applied { rows_affected })
}

/// 述語つき `UPDATE <table> SET ... WHERE ... USING OPERATION_ID '<id>'`
/// （SQL-19・TASK-192、Issue #871）の実体。唯一の到達経路は
/// `sql::exec::execute_predicate_update`。
///
/// 処理順序は [`delete_rows_where_unchecked`] と同一（ADR §6）。適用段のみが
/// 異なり、候補 `id` ごとに既存行を read-merge-write する
/// （単一行 UPDATE と共有する [`merge_row_for_update`] 経由。Issue #996。
/// `assignments` は束縛済みの `(列インデックス, 値)` 対応——`VECTOR` 列を
/// 対象とする割当は embedding を差し替え、それ以外は既存の scalar 列群の
/// 対応スロットを上書きする。SET で触れない列・embedding・可視性は既存行の
/// 値を保持する。SET 列が重複指定された場合は先勝ち——束縛段で拒否済みの
/// ため表層からは到達しない）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_rows_where_unchecked<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    content_hash_value: &content_hash::ContentHash,
    // Issue #1061: 述語形 UPDATE の VECTOR 列 SET 割当ハッシュを正準表現
    // （タグ 5・f32 LE 列）へ揃えたことに伴う既存台帳エントリとの互換用。
    // 正準化前のレイアウト（VECTOR 列への `String` をタグ 1・生テキストで
    // 連結）で記録された台帳エントリを「同一内容の再送」として引き続き
    // 受理するために `ledger::record_in_txn_accepting` の `legacy_hashes` へ
    // そのまま渡す。新規記録・上書きには常に `content_hash_value`（正準
    // ハッシュ）を使うため、内容不一致判定（`22023`）は弱まらない
    // （`content_hash::needs_legacy_vector_hash` が false の呼び出し元は
    // 空スライスを渡す）。
    legacy_hashes: &[content_hash::ContentHash],
    expected_schema: Option<&crate::catalog::TableSchema>,
    assignments: &[(usize, crate::row_codec::Value)],
    needs_embedding: bool,
    limit: Option<std::num::NonZeroUsize>,
    mut predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<PredicateDmlOutcome, PredicateDmlError<E>> {
    validate_identifier(table).map_err(dml_write_err)?;
    let write_txn = storage
        .begin_write_txn()
        .map_err(|e| dml_write_err(convert_write_txn_err(e)))?;

    let (candidate_ids, schema) = {
        let schema = require_table_schema_write(&write_txn, table).map_err(dml_write_err)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(dml_write_err(CatalogError::Invalid(
                    "table schema changed after the statement was bound".to_string(),
                )));
            }
        }

        // SET 値をスキーマに対して検証する（[`validate_set_assignments`] を単一行
        // [`update_row_columns_unchecked`] と共有）。候補行列挙・台帳記録より
        // **前**に行うことで、一致行が 0 件の場合でも不正な SET 値（`MAX_TEXT_
        // FIELD_LEN` 超過・累計 payload 上限超過・`VECTOR` 次元不一致・列型不一致
        // 等）は必ず拒否され、台帳へ記録される前に `write_txn` を破棄できる
        // （codex-review P1 指摘・PR #993 系・Issue #871: この検証が候補行の
        // 適用ループ内にしか無いと、一致行 0 件のまま台帳記録・`UPDATE 0` 成功が
        // 通ってしまい、同じ `operation_id` が正当な後続再送に使えなくなる）。
        validate_set_assignments(&schema, assignments).map_err(dml_write_err)?;

        ledger::record_in_txn_accepting(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            content_hash_value,
            legacy_hashes,
        )
        .map_err(dml_write_err)?;

        let row_table_name = user_rows_table_name(table);
        let row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(|e| dml_write_err(map_row_table_error(e)))?;
        let candidate_ids =
            enumerate_dml_candidates(&row_table, ctx, needs_embedding, limit, &mut predicate)?;
        (candidate_ids, schema)
    };

    if limit.is_some_and(|limit| candidate_ids.len() > limit.get()) {
        return Ok(PredicateDmlOutcome::LimitExceeded {
            count: candidate_ids.len(),
        });
    }

    // `FOREIGN KEY` の `ON UPDATE` 参照アクション（Issue #1076）の連鎖起点。
    let mut pre_images = crate::constraint::UpdatedKeyPreImages::new();
    {
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(|e| dml_write_err(map_row_table_error(e)))?;
        for id in &candidate_ids {
            let key = (ctx.tenant_id(), *id);
            let existing = match row_table
                .get(&key)
                .map_err(|e| dml_write_err(CatalogError::from(e)))?
            {
                Some(guard) => {
                    crate::storage::decode_row_for_key(ctx.tenant_id(), *id, guard.value())
                        .map_err(dml_write_err)?
                }
                None => {
                    // 列挙後・適用前の並行削除（同一トランザクション内で候補行は
                    // 列挙時に読み取り済みのため、redb の単一ライター制約下では
                    // 通常到達しないが、fail-closed に内部エラーとして拒否する
                    // （`unwrap`/添字禁止・coding-rust.md）。
                    return Err(dml_write_err(CatalogError::Invalid(
                        "internal: candidate row disappeared before apply".to_string(),
                    )));
                }
            };
            let visibility = existing.visibility;
            // 書き換える前の全列値を pre-image として積む（`existing` は直後の
            // `merge_row_for_update` へ move するため先に読む）。デコード失敗は
            // ここでは無視する（本来のデコードエラー分類は直後の
            // `merge_row_for_update` が引き続き担う。pre-image はベストエフォート）。
            // `id` 参照 FK は ON UPDATE で発火しない（A7）ため、主キー・UNIQUE を
            // 宣言しないテーブルはこのデコードを一切行わない。
            if schema.primary_key().is_some() || !schema.unique_constraints().is_empty() {
                if let Ok(old_values) =
                    crate::row_codec::decode_scalar_columns(&schema, &existing.metadata)
                {
                    pre_images.record(*id, old_values);
                }
            }

            // read-merge-write 本体は単一行 UPDATE
            // （`update_row_columns_unchecked`）と共有する
            // [`merge_row_for_update`] へ委譲済み（Issue #996。エラー分類・
            // 借用版デコードによるピーク確保回避・SET 列重複時の意味論
            // （先勝ち）などの設計判断は同関数のドキュメント参照）。
            let (embedding_value, metadata) =
                merge_row_for_update(&schema, existing, assignments).map_err(dml_write_err)?;
            let row = RowInput {
                tenant_id: ctx.tenant_id(),
                // 既存行の可視性を保持する（SET で触れない列と同じ扱い）。
                visibility,
                embedding: &embedding_value,
                metadata: &metadata,
            };
            let encoded = encode_row(&row).map_err(dml_write_err)?;
            row_table
                .insert(key, encoded.as_slice())
                .map_err(|e| dml_write_err(CatalogError::from(e)))?;
        }
    }

    // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約検査。
    // 一致行が 0 件（何も書き込んでいない）場合は呼ばない（`constraint.rs`
    // モジュールドキュメント参照）。
    if !candidate_ids.is_empty() {
        crate::constraint::enforce_row_constraints_in_txn(
            &write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            &candidate_ids,
            crate::constraint::FkCheckMode::All,
        )
        .map_err(dml_write_err)?;
        // このテーブルを参照先とする `FOREIGN KEY` の参照先側（TABLE-17・TASK-205、
        // Issue #907。SET 列が主キー・UNIQUE 構成列を含む場合のみ走査）。
        let updated_columns: Vec<usize> = assignments.iter().map(|(idx, _)| *idx).collect();
        crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            table,
            &schema,
            ctx.tenant_id(),
            crate::constraint::ReferencedRowsChange::ColumnsUpdated(&updated_columns),
            Some(&pre_images),
            crate::constraint::FkCheckMode::All,
        )
        .map_err(dml_write_err)?;
    }

    let rows_affected = candidate_ids.len();
    if rows_affected > 0 {
        crate::catalog::bump_table_generation_in_txn(&write_txn, table).map_err(dml_write_err)?;
    }
    crate::recovery::commit_boundary::commit(write_txn).map_err(dml_write_err)?;
    Ok(PredicateDmlOutcome::Applied { rows_affected })
}

pub(crate) fn truncate_table_unchecked(
    target: WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    target.with_txn(|write_txn| {
        // テーブル不存在の判定・並行 DDL との整合のため他の書き込み系操作と
        // 同じ前段を通す。
        let schema = require_table_schema_write(write_txn, table)?;
        // TRUNCATE 要求のクライアント由来の内容はテーブル名（台帳キー
        // `(tenant, table, operation_id)` に既に含まれる）以外に存在しない
        // （`content_hash::for_truncate` ドキュメント参照）。
        let content_hash = content_hash::for_truncate();
        ledger::record_in_txn(
            write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

        let tenant = ctx.tenant_id();
        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;
        let start = std::ops::Bound::Included((tenant, 0u64));
        let end = std::ops::Bound::Included((tenant, u64::MAX));
        row_table
            .retain_in((start, end), |_, _| false)
            .map_err(CatalogError::from)?;
        drop(row_table);
        // このテーブルを参照先とする `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）
        // の参照先側の検査。自テナントの参照元行が 1 件でも残れば `23503`（他テナントの
        // 行は削除も走査もしないため、他テナントの参照元行の有無は結果に影響しない）。
        // `TRUNCATE` は参照アクション（Issue #1076 A4）を発火させないため
        // `Truncated`（`Removed` とは異なり連鎖を起こさない）を渡す。
        crate::constraint::enforce_referencing_rows_in_txn(
            write_txn,
            table,
            &schema,
            tenant,
            crate::constraint::ReferencedRowsChange::Truncated,
            None,
            target.fk_check_mode(),
        )?;
        crate::catalog::bump_table_generation_in_txn(write_txn, table)?;
        Ok(((), TxnEffect::Wrote))
    })
}

/// [`replace_typed_rows_by_text_key`] の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceOutcome {
    /// テキスト列 `key_column` が `key_value` に一致した既存行のうち削除した件数。
    pub removed: usize,
    /// 新規挿入した行数（`rows.len()` と一致する）。
    pub inserted: usize,
    /// 新規挿入した行のうち最小の `id`（`rows` が空なら `None`）。
    pub first_id: Option<u64>,
}

/// テキスト列 `key_column` の値が `key_value` に一致するテナント内の既存行を
/// すべて削除し、代わりに `rows` を新規挿入する（TASK-120・対象ビヘイビア:
/// INDEX-1, INDEX-2。ファイル形 `INSERT` の同一パス再送時の置換セマンティクスの
/// 決定を担う。ポインタ: `docs/design/resend-semantics.md`）。
///
/// `crate::incremental::index_file` から、チャンク化・埋め込み計算をすべて終えた
/// 後にのみ呼ばれる（write トランザクションの外で外部 I/O・CPU 計算を終えてから
/// 単一の write トランザクションへ入る設計。coding-rust.md「不安全な設計 / DoS」:
/// 単一ライタの長時間占有を避ける）。
///
/// - 削除・採番・挿入・世代更新をすべて単一の write トランザクション内で行う
///   （途中失敗は `redb::WriteTransaction` の drop 契約により abort。副作用ゼロ）
/// - 走査範囲はテナント名前空間 `(ctx.tenant_id(), ..)` に限定する（TABLE-12・RLS-9。
///   他テナントの同一 `key_value` 行は走査対象にも削除対象にも含まれない）
/// - 新規行の `id` は同一テナント名前空間内の既存最大 `id`（削除対象・対象外を問わず、
///   走査中に観測した全行）+ 1 から連番で採番する（[`insert_row`] の「既存行を
///   上書きしない」契約と衝突しない。`checked_add` でオーバーフローを `Err` に倒す）
/// - `rows` が空かつ削除対象も 0 件なら世代を進めずに成功する（[`insert_rows`] の
///   空バッチと同じ扱い。無変更コミットで既存インデックスを不要に失効させない）
/// - テナント名前空間内の走査は `visible_rows` と同じ上限（[`MAX_SCANNED_ROWS`]・
///   [`MAX_VISIBLE_ROWS`]）を適用し、超過時は副作用ゼロで `Err`。各行は
///   `crate::storage::decode_row_metadata_borrowed` で `metadata`（スカラー列
///   ペイロード）のみを借用取得し、比較に不要な embedding は確保しない
///   （coding-rust.md「不安全な設計 / DoS」対応）
/// - PRIMARY KEY・UNIQUE 制約（TABLE-16・TASK-204）を持つテーブルも対象とする
///   （Issue #1072）。旧チャンク行の削除（上記）は一意性検査（下記の
///   `crate::constraint::enforce_row_constraints_in_txn` 呼び出し）より前に同一
///   write トランザクション内で完了しているため、旧チャンクは検査時の母集合に
///   含まれない。違反時は commit 前の `?` でトランザクションごと abort し、台帳記録・
///   削除・挿入のいずれも残らない（副作用ゼロ）。チャンク間で値が変わらない列
///   のみで構成される UNIQUE は、複数チャンクに分割されるファイルを宣言どおり
///   `23505` で拒否する（特別扱いはしない）
///
/// エラー契約は [`insert_row`]/[`delete_row`] と同一（`TenantWriteError`。他テナントの
/// 存在情報を漏らさない fail-closed）。`key_column` がスキーマに存在しない・
/// テーブルに VECTOR 列がない場合は [`TenantWriteError::Catalog`]（`CatalogError::Invalid`）。
///
/// 可視性: `operation_id` 必須化ガード（TASK-92・RECOVER-1）を自身では適用しない
/// 内部結線用 API のため `pub(crate)` に閉じる（ガードは唯一の到達経路である
/// `core::EngineCore::execute_insert_sql` が `sql::allowlist::validate_insert` 経由で
/// 書き込み前に適用済み）。クレート外へ公開するとガードを迂回する書き込み入口に
/// なる（codex-review P1 指摘・PR #221。security.md P0）。
/// [`replace_typed_rows_by_text_key`] の入力一式（引数の取り違えを型で防ぎ、
/// 引数個数を抑えるためのまとまり。`incremental::index_file` が構築する）。
pub(crate) struct ReplaceByTextKey<'a> {
    pub table: &'a str,
    /// 置換キーにするテキスト列名（ファイル形 `INSERT` では常に `path`）。
    pub key_column: &'a str,
    pub key_value: &'a str,
    pub visibility: crate::storage::Visibility,
    pub rows: &'a [Vec<crate::row_codec::Value>],
    /// 内容照合ハッシュ（TASK-101・RECOVER-10）専用の raw クライアント要求。
    /// `rows`（チャンク化・埋め込み後の派生行データ）とは意図的に分離する
    /// （codex-review P1 指摘・PR #248。`content_hash::for_replace_by_text_key`
    /// ドキュメント参照）。
    pub content_hash_path: &'a str,
    pub content_hash_body: &'a str,
    pub content_hash_template_values: &'a [crate::row_codec::Value],
    /// 台帳への記録指示（TASK-93・RECOVER-2）。行の削除・挿入と同一 write
    /// トランザクション内で適用される。
    pub ledger_write: LedgerWrite<'a>,
}

pub(crate) fn replace_typed_rows_by_text_key(
    storage: &Storage,
    ctx: &PolicyContext,
    req: ReplaceByTextKey<'_>,
) -> Result<ReplaceOutcome, TenantWriteError> {
    let ReplaceByTextKey {
        table,
        key_column,
        key_value,
        visibility,
        rows,
        content_hash_path,
        content_hash_body,
        content_hash_template_values,
        ledger_write,
    } = req;
    validate_identifier(table)?;
    let write_txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    // `ON DELETE` 参照アクション（Issue #1076 A14）の連鎖起点。クロージャ内で
    // 可変参照として捕捉し、削除した旧チャンク行の削除前の値を積む（下記の
    // `needs_fk_removed_pre_image` 代入箇所のドキュメント参照）。
    let mut needs_fk_removed_pre_image = false;
    let mut removed_pre_images = crate::constraint::UpdatedKeyPreImages::new();
    // `row_table` の借用（`write_txn.open_table(..)`）をこのブロック内に閉じ込め、
    // ブロックを抜けた後に `write_txn` を（成功なら commit、無変更なら drop で
    // abort）自由に扱えるようにする（`insert_rows` の空バッチ早期 return と異なり、
    // 「削除対象 0 件」は行を走査するまで判定できないため、走査後に判定する）。
    let outcome: Result<ReplaceOutcome, TenantWriteError> = (|| {
        let schema = require_table_schema_write(&write_txn, table)?;
        let vector_idx = schema
            .columns
            .iter()
            .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)))
            .ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "table has no VECTOR column".to_string(),
                ))
            })?;
        let key_idx = schema
            .columns
            .iter()
            .position(|c| c.name == key_column)
            .ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(format!(
                    "unknown key column: {key_column}"
                )))
            })?;

        let row_table_name = user_rows_table_name(table);
        let mut row_table = write_txn
            .open_table(user_rows_table_def(&row_table_name))
            .map_err(map_row_table_error)?;

        let tenant = ctx.tenant_id();
        // テナント名前空間内の既存行を走査し、削除対象 id・既存最大 id（削除対象・
        // 対象外を問わない）を同時に収集する。`range` の借用は本ブロックで閉じ、
        // 後続の `remove`/`insert`（`&mut` 借用）と衝突しないようにする
        // （`update_row`/`delete_row` の `AccessGuard` スコープと同じ方針）。
        let mut to_remove: Vec<u64> = Vec::new();
        let mut max_id: Option<u64> = None;
        let mut scanned_count: usize = 0;
        {
            let start = std::ops::Bound::Included((tenant, 0u64));
            let end = std::ops::Bound::Included((tenant, u64::MAX));
            let mut iter = row_table
                .range::<(&str, u64)>((start, end))
                .map_err(CatalogError::from)?;
            for entry in &mut iter {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (_key_tenant, id) = k.value();
                let raw = v.value();
                scanned_count = scanned_count.saturating_add(1);
                if scanned_count > MAX_SCANNED_ROWS {
                    return Err(TenantWriteError::Storage(StorageError::Codec(format!(
                        "too many rows scanned for replace: max {MAX_SCANNED_ROWS}"
                    ))));
                }
                // embedding は比較に不要なため、metadata（スカラー列ペイロード）のみを
                // 借用で取り出す（`decode_row` は行ごとに `Vec<f32>` を確保するため、
                // テナント全行走査のホットパスでは使わない。`storage.rs`
                // `decode_row_metadata_borrowed` モジュールドキュメント参照。
                // coding-rust.md「不安全な設計 / DoS」対応）。
                let metadata = crate::storage::decode_row_metadata_borrowed(raw)
                    .map_err(TenantWriteError::Storage)?;
                max_id = Some(max_id.map_or(id, |m: u64| m.max(id)));
                let scanned = crate::row_codec::scan_scalar_columns(&schema, metadata)
                    .map_err(|e| TenantWriteError::Storage(StorageError::Codec(e.to_string())))?;
                if scanned
                    .get(key_idx)
                    .copied()
                    .flatten()
                    .and_then(|v| v.as_text())
                    == Some(key_value)
                {
                    to_remove.push(id);
                    if to_remove.len() > MAX_VISIBLE_ROWS {
                        return Err(TenantWriteError::Storage(StorageError::Codec(format!(
                            "too many matching rows for replace: max {MAX_VISIBLE_ROWS}"
                        ))));
                    }
                }
            }
        }

        let removed = to_remove.len();
        if removed == 0 && rows.is_empty() {
            // 変更ゼロ。`insert_rows` の空バッチと同じく世代を進めずに成功する
            // （呼び出し元が commit せず drop することを、戻り値の 0/0/None から判断する）。
            return Ok(ReplaceOutcome {
                removed: 0,
                inserted: 0,
                first_id: None,
            });
        }

        // 台帳記録は行の削除・挿入と同一の write トランザクション内で行う
        // （TASK-93・RECOVER-2。`insert_typed_row_unchecked` と同型。失敗すれば
        // トランザクションごと abort し、行変更も台帳も残さない）。ハッシュ入力は
        // 要求由来フィールド（`key_column`・`key_value`・`visibility`・
        // `content_hash_path`・`content_hash_body`・`content_hash_template_values`）
        // のみ（TASK-101・RECOVER-10。削除対象集合・採番 id 等の DB 状態由来の値に加え、
        // チャンク化・埋め込み後の派生行データ（`rows`）も含めない。
        // `content_hash::for_replace_by_text_key` ドキュメント参照）。同一内容の再送は
        // `23505`、内容不一致は `22023` へ写像される（呼び出し元の共通 `TenantWriteError`
        // 契約に従う。行形 `INSERT` 経路と同じ扱い。TASK-94・RECOVER-3 の重複拒否契約を
        // 包含する）。
        //
        // `content_hash_template_values`（`schema.columns.len()` 幅・位置インデックス
        // 基準の配列。`sql::parser::bind_file_insert` が構築）をそのまま渡さず、
        // 列名付きペアへ変換してから渡す（cursor bugbot 指摘・PR #248。
        // `content_hash::push_named_scalar_columns` ドキュメント参照。`insert_typed_row_unchecked`
        // と同じ理由: 位置基準のままだと `ALTER TABLE ADD COLUMN` を挟んだ再送で
        // 配列幅がずれ、内容一致の再送が `22023` に誤判定される）。
        let named_template_columns: Vec<(&str, &crate::row_codec::Value)> = schema
            .columns
            .iter()
            .enumerate()
            .filter(|(idx, column)| {
                *idx != vector_idx && !matches!(column.ty, crate::catalog::ColumnType::Vector(_))
            })
            .filter_map(|(idx, column)| {
                content_hash_template_values
                    .get(idx)
                    .map(|value| (column.name.as_str(), value))
            })
            .collect();
        let content_hash = content_hash::for_replace_by_text_key(
            key_column,
            key_value,
            visibility,
            content_hash_path,
            content_hash_body,
            &named_template_columns,
        )?;
        ledger::record_in_txn(
            &write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            &content_hash,
        )?;

        // `ON DELETE` 参照アクション（Issue #1076 A14）の連鎖起点。`delete_row_impl`・
        // `delete_rows_where_unchecked` と同じ判定条件・同じ理由
        // （`removed_pre_images` のドキュメント参照）で、主キー・UNIQUE を宣言する
        // テーブルのみ `remove` の戻り値（削除前の物理行）から旧値を復元して積む。
        // `needs_fk_removed_pre_image`・`removed_pre_images` は本クロージャの外で
        // 宣言し可変参照で捕捉する（`ReplaceOutcome` は他クレートも参照する
        // 公開構造体のため、フィールド追加で契約を広げない）。
        needs_fk_removed_pre_image =
            schema.primary_key().is_some() || !schema.unique_constraints().is_empty();
        for id in &to_remove {
            let removed_guard = row_table
                .remove(&(tenant, *id))
                .map_err(CatalogError::from)?;
            if needs_fk_removed_pre_image {
                if let Some(guard) = removed_guard {
                    let row = crate::storage::decode_row(*id, guard.value()).map_err(|e| {
                        TenantWriteError::CapturedRowDecodeFailed(format!(
                            "removed row decode failed: {e}"
                        ))
                    })?;
                    let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                        .map_err(|e| {
                            TenantWriteError::CapturedRowDecodeFailed(format!(
                                "removed row decode failed: {e}"
                            ))
                        })?;
                    removed_pre_images.record(*id, values);
                }
            }
        }

        // clear 再利用の 1 面スクラッチ（Issue #398）: 行ごとの `encode_row`
        // （`Vec<u8>` 新規確保）を避け、`scratch.clear()` → `encode_row_into` で
        // 同一バッファへ追記する。この経路（`execute_insert_sql_batch`）の台帳
        // ハッシュ `for_replace_by_text_key` はエンコード済みバイト列を使わないため、
        // `insert_rows_unchecked`（Issue #398）の連続 arena は不要（ハッシュ計算
        // 完了後の行ループでのみエンコードすれば足りる）。
        let mut scratch: Vec<u8> = Vec::new();
        let mut next_id = max_id.map_or(Ok(0u64), |m| {
            m.checked_add(1).ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "id namespace exhausted".to_string(),
                ))
            })
        })?;
        let mut first_id: Option<u64> = None;
        let mut inserted = 0usize;
        for values in rows {
            let embedding = match values.get(vector_idx) {
                Some(crate::row_codec::Value::Vector(v)) => v.clone(),
                _ => {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "VECTOR column value missing or not a Vector".to_string(),
                    )))
                }
            };
            schema.validate_embedding_dim(embedding.len())?;
            let metadata = crate::row_codec::encode_scalar_columns(&schema, values)
                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
            let row = RowInput {
                tenant_id: ctx.tenant_id(),
                visibility,
                embedding: &embedding,
                metadata: &metadata,
            };
            let id = next_id;
            let key = (ctx.tenant_id(), id);
            // 上の採番規則により既存行との衝突は起こらないはずだが、`insert_row` と
            // 同じ防御（TOCTOU 対策の単一 write トランザクション内チェック）を残す。
            if row_table.get(&key).map_err(CatalogError::from)?.is_some() {
                return Err(TenantWriteError::IdConflict);
            }
            scratch.clear();
            crate::storage::encode_row_into(&mut scratch, &row)?;
            row_table
                .insert(key, scratch.as_slice())
                .map_err(CatalogError::from)?;
            if first_id.is_none() {
                first_id = Some(id);
            }
            inserted += 1;
            next_id = id.checked_add(1).ok_or_else(|| {
                TenantWriteError::Catalog(CatalogError::Invalid(
                    "id namespace exhausted".to_string(),
                ))
            })?;
        }

        Ok(ReplaceOutcome {
            removed,
            inserted,
            first_id,
        })
    })();
    let outcome = outcome?;
    if outcome.removed == 0 && outcome.inserted == 0 {
        drop(write_txn);
        return Ok(outcome);
    }
    // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905・#1072。TABLE-16・TASK-204）のテナント内一意性制約検査。
    // このファイル形 `INSERT`（同一パス置換）は採番 id が `first_id` から連番で
    // 割り当てられる（上記クロージャの `next_id` 採番規則）ため、書き込んだ id
    // 集合は `first_id..first_id + inserted` の連続範囲として再構築できる。
    // `removed` のみで `inserted == 0` の場合（`rows` が空で既存行を削除しただけ）
    // は新規に書き込んだ行がないため検査不要（削除は一意性制約に違反し得ない）。
    // 旧チャンク行の削除は上記クロージャ内（この検査より前）で同一 write
    // トランザクション内に完了しているため、走査時の母集合から除外済み
    // （関数 doc コメント参照。redb の write トランザクションは自分が消した
    // 行をそのまま読める）。
    if outcome.inserted > 0 {
        if let Some(first_id) = outcome.first_id {
            let ids: Vec<u64> = (0..outcome.inserted as u64)
                .filter_map(|offset| first_id.checked_add(offset))
                .collect();
            let schema_for_pk = require_table_schema_write(&write_txn, table)?;
            crate::constraint::enforce_row_constraints_in_txn(
                &write_txn,
                table,
                &schema_for_pk,
                ctx.tenant_id(),
                &ids,
                crate::constraint::FkCheckMode::All,
            )?;
        }
    }
    // 置換で旧行を削除した場合、このテーブルを参照先とする `FOREIGN KEY`
    // （TABLE-17・TASK-205、Issue #907）の参照先側を検査する（旧行 id を参照する
    // 参照元行が残れば `23503`）。
    if outcome.removed > 0 {
        let schema_for_fk = require_table_schema_write(&write_txn, table)?;
        crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            table,
            &schema_for_fk,
            ctx.tenant_id(),
            crate::constraint::ReferencedRowsChange::Removed,
            needs_fk_removed_pre_image.then_some(&removed_pre_images),
            crate::constraint::FkCheckMode::All,
        )?;
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, table)?;
    crate::recovery::commit_boundary::commit(write_txn)?;
    Ok(outcome)
}

/// `table` に `op_id` が台帳記録済みかを照会する（TASK-93、対象ビヘイビア: RECOVER-2）。
/// `crate::core::EngineCore::operation_recorded` からの薄い委譲先。`pub(crate)` に
/// 限定する（codex-review P1 指摘・PR #226）: `EngineCore::operation_recorded` は
/// `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）で台帳へ一切触れず
/// `LedgerLookup::NoLedger` を返す契約だが、本関数は `Storage` を直接受け取り
/// `ledger_mode` の状態を知らないため、DB に過去（`Ledgered` 構成時）の記録が
/// 残っていればそれをそのまま観測してしまう。これを公開したままだと
/// `EngineCore` の「台帳を持たない構成では照会しない」という fail-closed な
/// 区別を呼び出し元が迂回できてしまう。`EngineCore::operation_recorded` 経由の
/// 委譲（`LedgerLookup` 判定込み）に一本化し、モード非依存の生の照会結果を
/// クレート外へ公開しない。
///
/// 照会範囲は呼び出し元テナント（`ctx.tenant_id()`）の名前空間に閉じる。他テナントの
/// `operation_id` 存在を成否・文言・経路差で観測できる経路にはならない（TABLE-12・
/// RLS-9 と同型。security.md P0）。
pub(crate) fn operation_recorded(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    op_id: &OperationId,
) -> Result<bool, TenantWriteError> {
    validate_identifier(table)?;
    let read_txn = storage.db().begin_read().map_err(CatalogError::from)?;
    ledger::contains_in_read_txn(&read_txn, ctx.tenant_id(), table, op_id)
        .map_err(TenantWriteError::LedgerCorrupted)
}

/// `table` の最終 commit 済み `operation_id` を照会する（TASK-98、対象ビヘイビア:
/// RECOVER-7。契約の詳細は spec 参照）。`crate::core::EngineCore::last_operation_id`
/// からの薄い委譲先。[`operation_recorded`] と同じ理由で `pub(crate)` に限定する:
/// `ledger_mode` の `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）判定は
/// `EngineCore::last_operation_id` 側が担い、本関数はモード非依存の生の照会結果
/// （[`ledger::LastOperationRaw`]。詳細は `recovery::ledger` モジュールドキュメント
/// 参照。codex-review P1 指摘対応）のみを返す。この区別をクレート外から迂回できない
/// よう `pub(crate)` に留める。
///
/// 照会範囲は呼び出し元テナント（`ctx.tenant_id()`）の名前空間に閉じる（TABLE-12・
/// RLS-9 と同型。security.md P0）。
pub(crate) fn last_operation(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
) -> Result<ledger::LastOperationRaw, TenantWriteError> {
    validate_identifier(table)?;
    let read_txn = storage.db().begin_read().map_err(CatalogError::from)?;
    ledger::last_operation_in_read_txn(&read_txn, ctx.tenant_id(), table)
        .map_err(TenantWriteError::LedgerCorrupted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::storage::{RowInput, Visibility};

    // 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn schema(table: &str) -> TableSchema {
        TableSchema::new(
            table,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        )
    }

    // 対象ビヘイビア: TABLE-9。
    #[test]
    fn visible_rows_includes_other_tenant_public_rows() {
        let path = unique_db_path("visible-public");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        storage
            .insert_rows_into_table(
                "docs",
                &[
                    (
                        1,
                        RowInput {
                            tenant_id: "tenant-a",
                            visibility: Visibility::Public,
                            embedding: &[1.0, 0.0],
                            metadata: &[],
                        },
                    ),
                    (
                        2,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Public,
                            embedding: &[0.0, 1.0],
                            metadata: &[],
                        },
                    ),
                    (
                        3,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Private,
                            embedding: &[1.0, 1.0],
                            metadata: &[],
                        },
                    ),
                ],
            )
            .expect("seed rows");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut rows = visible_rows(&storage, "docs", &ctx).expect("visible_rows ok");
        rows.sort_by_key(|r| r.id);
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2],
            "tenant-a ctx must see its own row and the other tenant's Public row, \
             but not the other tenant's Private row"
        );
    }

    // 対象ビヘイビア: TABLE-11。`verify_hits` は可視集合外の id を fail-closed に拒否する。
    #[test]
    fn verify_hits_rejects_id_outside_visible_set() {
        let path = unique_db_path("verify-hits");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        storage
            .insert_rows_into_table(
                "docs",
                &[
                    (
                        1,
                        RowInput {
                            tenant_id: "tenant-a",
                            visibility: Visibility::Public,
                            embedding: &[1.0, 0.0],
                            metadata: &[],
                        },
                    ),
                    (
                        2,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Private,
                            embedding: &[0.0, 1.0],
                            metadata: &[],
                        },
                    ),
                ],
            )
            .expect("seed rows");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 照合キーは `(tenant_id, id)`（TABLE-12・RLS-9）。
        let own_hit = SearchHit::new("tenant-a", 1, 1.0);
        let foreign_private_hit = SearchHit::new("tenant-b", 2, 0.5);
        assert!(verify_hits(&storage, "docs", &ctx, std::slice::from_ref(&own_hit)).is_ok());
        assert!(matches!(
            verify_hits(&storage, "docs", &ctx, &[own_hit, foreign_private_hit]),
            Err(TenantError::HitOutsideVisibleSet)
        ));
    }

    // 対象ビヘイビア: RECOVER-4（負方向・生 API の到達範囲確認）。
    // `crate::catalog::Storage::insert_row_into_table` は codex-review P0 指摘
    // （PR #194）を受けて `pub(crate)` 化し、クレート外（`tests/` 配下の結合テスト・
    // wire-server 等）からは到達不能にした。この生 API は本モジュール内では
    // （例: 将来の移行ツール等で）引き続き参照しうるため、クレート内ユニットテストとして
    // 「テナント境界チェックを経由しない書き込みは実際に行を書き換える」ことを確認する。
    // 旧・結合テスト版（`tests/tenant_breach.rs::recover4_checker_detects_unguarded_mutation`）
    // は `pub(crate)` 化に伴いクレート外から呼べなくなったため、このユニットテストへ
    // 移設した。
    #[test]
    fn raw_insert_row_into_table_bypasses_tenant_guard() {
        let path = unique_db_path("raw-insert-bypass");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");

        // ガード付き経路（`insert_row_unchecked`。`operation_id` 必須化ガードは本テストの
        // 対象外なので、ガードを内包する `pub fn insert_row` ではなくガードなし実体を
        // 直接使う）で tenant-b 名義の行を正規に投入する。
        let owner = PolicyContext::new("tenant-b").expect("valid tenant");
        insert_row_unchecked(
            WriteTarget::Autocommit(&storage),
            "docs",
            &owner,
            1,
            &RowInput {
                tenant_id: "tenant-b",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: &[],
            },
            // 本テストの主眼は台帳ではなくテナント境界の到達範囲確認のため、台帳は
            // 使わない（`LedgerWrite::Disabled`）。
            LedgerWrite::Disabled,
        )
        .expect("seed tenant-b row via guarded path");

        // ガードを経由しない生の `Storage::insert_row_into_table`（`pub(crate)`）で
        // 同じ id を tenant-a 名義へ上書きできてしまうことを確認する（クレート内から
        // 到達可能である以上、この経路自体は塞がっていないことの記録。クレート外からの
        // 到達不能性が本対応の主眼）。
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[9.0, 9.0],
                    metadata: &[],
                },
            )
            .expect("unguarded write succeeds by construction");

        let after = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("read back row");
        assert_eq!(after.tenant_id, "tenant-a");
    }

    // --- replace_typed_rows_by_text_key（TASK-120・対象ビヘイビア: INDEX-1, INDEX-2） --

    fn file_schema(table: &str) -> TableSchema {
        TableSchema::new(
            table,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn row_values(embedding: [f32; 2], path: &str, body: &str) -> Vec<crate::row_codec::Value> {
        vec![
            crate::row_codec::Value::Vector(embedding.to_vec()),
            crate::row_codec::Value::Text(path.to_string()),
            crate::row_codec::Value::Text(body.to_string()),
        ]
    }

    #[test]
    fn replace_same_path_replaces_rows_and_leaves_other_paths_untouched() {
        let path = unique_db_path("replace-same-path");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "other.txt",
                visibility: Visibility::Private,
                rows: &[row_values([9.0, 9.0], "other.txt", "unrelated")],
                content_hash_path: "other.txt",
                content_hash_body: "unrelated",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("seed other path");

        let first = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "note.txt",
                visibility: Visibility::Private,
                rows: &[row_values([1.0, 0.0], "note.txt", "v1 chunk a")],
                content_hash_path: "note.txt",
                content_hash_body: "v1 body",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("first replace should succeed");
        assert_eq!(first.removed, 0);
        assert_eq!(first.inserted, 1);
        assert!(first.first_id.is_some());

        let second = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "note.txt",
                visibility: Visibility::Private,
                rows: &[
                    row_values([2.0, 0.0], "note.txt", "v2 chunk a"),
                    row_values([2.0, 1.0], "note.txt", "v2 chunk b"),
                ],
                content_hash_path: "note.txt",
                content_hash_body: "v2 body",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("second replace should succeed");
        assert_eq!(second.removed, 1);
        assert_eq!(second.inserted, 2);
        // 採番は既存最大 id + 1 から連番であり、他パス・旧チャンクの id と衝突しない。
        assert!(second.first_id.unwrap() > first.first_id.unwrap());

        // 他パスは無変更。
        let visible_ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let rows = visible_rows(&storage, "docs", &visible_ctx).expect("visible rows");
        let bodies: Vec<&str> = rows
            .iter()
            .map(|r| {
                let scanned =
                    crate::row_codec::scan_scalar_columns(&file_schema("docs"), &r.metadata)
                        .expect("scan scalar columns");
                scanned
                    .get(2)
                    .copied()
                    .flatten()
                    .and_then(|v| v.as_text())
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(rows.len(), 3);
        assert!(bodies.contains(&"unrelated"));
        assert!(bodies.contains(&"v2 chunk a"));
        assert!(bodies.contains(&"v2 chunk b"));
        assert!(!bodies.contains(&"v1 chunk a"));
    }

    #[test]
    fn replace_does_not_touch_other_tenants_same_path_rows() {
        let path = unique_db_path("replace-tenant-isolation");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

        replace_typed_rows_by_text_key(
            &storage,
            &ctx_b,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "shared.txt",
                visibility: Visibility::Private,
                rows: &[row_values([1.0, 1.0], "shared.txt", "tenant-b content")],
                content_hash_path: "shared.txt",
                content_hash_body: "tenant-b content",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("tenant-b seed should succeed");

        replace_typed_rows_by_text_key(
            &storage,
            &ctx_a,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "shared.txt",
                visibility: Visibility::Private,
                rows: &[row_values([2.0, 2.0], "shared.txt", "tenant-a content")],
                content_hash_path: "shared.txt",
                content_hash_body: "tenant-a content",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("tenant-a replace should succeed");

        let visible_ctx_b =
            PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let rows_b = visible_rows(&storage, "docs", &visible_ctx_b).expect("visible rows");
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_b[0].tenant_id, "tenant-b");
    }

    #[test]
    fn replace_empty_rows_and_no_match_does_not_bump_generation() {
        let path = unique_db_path("replace-empty-noop");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let before = storage.current_generation().expect("read generation");
        let outcome = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "absent.txt",
                visibility: Visibility::Private,
                rows: &[],
                content_hash_path: "absent.txt",
                content_hash_body: "",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("no-op replace should succeed");
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.inserted, 0);
        assert_eq!(outcome.first_id, None);
        let after = storage.current_generation().expect("read generation");
        assert_eq!(before, after);
    }

    #[test]
    fn replace_rejects_table_without_vector_column() {
        let path = unique_db_path("replace-no-vector-column");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("path", ColumnType::Text, false)],
            ))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let err = replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "a.txt",
                visibility: Visibility::Private,
                rows: &[vec![crate::row_codec::Value::Text("a.txt".to_string())]],
                content_hash_path: "a.txt",
                content_hash_body: "body",
                content_hash_template_values: &[],
                // 本テストは台帳の記録有無を検証対象にしないため無効化する。
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TenantWriteError::Catalog(CatalogError::Invalid(_))
        ));
    }

    // 対象ビヘイビア: TABLE-12・TASK-130。[`insert_unique_row`] 導入（`get` を省き
    // `insert` の戻り値で衝突判定）により、衝突行より前に処理される行が実際に write
    // txn 内へ書き込まれても、txn が commit されなければ何も永続化されないことを固定
    // する（PR #194 の「既存行と衝突するバッチは IdConflict・既存行は不変」契約の
    // 単体テスト版。バッチ末尾で衝突するケースを対象に、先行行が残らないことを確認）。
    #[test]
    fn insert_rows_with_trailing_conflict_persists_nothing() {
        let path = unique_db_path("trailing-conflict");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");

        let a = PolicyContext::new("tenant-a").expect("valid tenant");
        // TASK-101（RECOVER-10）: 台帳照合は operation_id 単位でハッシュを持つため、
        // 同一 operation_id を別内容の書き込みへ使い回すと本テストの意図（行 id 衝突の
        // 検証）より先に `OperationIdContentMismatch` を検出してしまう。行 id 衝突を
        // 単独で検証するため、シード投入とバッチ投入で別々の operation_id を使う。
        let seed_op_id = OperationId::parse("test-op-seed").expect("valid operation_id");
        let batch_op_id = OperationId::parse("test-op-batch").expect("valid operation_id");

        // id=3 を事前に投入しておき、バッチ末尾でこの id と衝突させる。
        insert_row(
            &storage,
            "docs",
            &a,
            3,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[9.0, 9.0],
                metadata: b"original",
            },
            &seed_op_id,
        )
        .expect("seed id=3");

        let batch = [
            (
                1u64,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: b"one",
                },
            ),
            (
                2u64,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: b"two",
                },
            ),
            (
                3u64,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 1.0],
                    metadata: b"overwrite-attempt",
                },
            ),
        ];
        let err = insert_rows(&storage, "docs", &a, &batch, &batch_op_id)
            .expect_err("trailing id=3 conflicts with the seeded row");
        assert!(matches!(err, TenantWriteError::IdConflict));

        // id=1・2 は「衝突より前に処理された行」だが、txn が commit されていない
        // ため一切書き込まれていない（all-or-nothing）。
        assert!(
            storage.get_row_from_table("docs", "tenant-a", 1).is_err(),
            "id=1 must not have been persisted"
        );
        assert!(
            storage.get_row_from_table("docs", "tenant-a", 2).is_err(),
            "id=2 must not have been persisted"
        );
        // id=3 は元の内容のまま（insert-then-abort でも上書きは永続化されない）。
        let row3 = storage
            .get_row_from_table("docs", "tenant-a", 3)
            .expect("id=3 must still exist with its original content");
        assert_eq!(row3.metadata, b"original".to_vec());
    }

    // codex-review P1 再指摘（PR #266）「新設する場合は書き込み経路での更新漏れが
    // ないことをテストで担保」対応: `bump_table_generation_in_txn` を呼ぶすべての
    // テナント境界付き書き込み API（`insert_row`・`insert_rows`・`insert_typed_row`・
    // `update_row`・`delete_row`・`replace_typed_rows_by_text_key`）が対象テーブル
    // （`docs`）の世代を実際に進めること、かつ無関係な別テーブル（`sibling`）・
    // 同一テーブルへの他テナント（`tenant-b`）の書き込みには影響を与えないことを
    // 固定する（`catalog.rs` 側の DDL・生の書き込み API は
    // `catalog_write_apis_bump_only_the_written_tables_generation` で別途カバーする。
    // `docs` への他テナント書き込みは意図的に「無関係」扱いしない設計判断＝
    // `core.rs` `Statement::Select` アームの `USING PLAN` 世代照合コメント参照）。
    #[test]
    fn write_apis_bump_only_the_written_tables_generation() {
        let path = unique_db_path("table-generation-bump-coverage-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema("docs"))
            .expect("create table docs");
        storage
            .create_table(&schema("sibling"))
            .expect("create table sibling");

        let read_gen = |name: &str| -> u64 {
            let read_txn = storage.db().begin_read().expect("begin read");
            crate::catalog::table_generation_in_txn(&read_txn, name).expect("read table generation")
        };

        let a = PolicyContext::new("tenant-a").expect("valid tenant");
        let sibling_gen = read_gen("sibling");
        let mut prev = read_gen("docs");

        let op = |suffix: &str| OperationId::parse(suffix).expect("valid operation_id");

        insert_row(
            &storage,
            "docs",
            &a,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: b"one",
            },
            &op("bump-insert-row"),
        )
        .expect("insert_row");
        let next = read_gen("docs");
        assert!(next > prev, "insert_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        insert_rows(
            &storage,
            "docs",
            &a,
            &[(
                2,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: b"two",
                },
            )],
            &op("bump-insert-rows"),
        )
        .expect("insert_rows (non-empty)");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "insert_rows (non-empty) must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 空バッチは commit 自体を行わない既存契約（`insert_rows_unchecked` の
        // ドキュメントコメント参照）のとおり、世代を進めない。テーブル単位世代に
        // 加え、commit でのみ進むグローバル世代も進まないこと（空の write_txn を
        // commit しない）を確認する（PR #1041 レビュー指摘）。
        let global_before = storage.current_generation().expect("global generation");
        insert_rows(&storage, "docs", &a, &[], &op("bump-insert-rows-empty"))
            .expect("insert_rows (empty)");
        assert_eq!(
            read_gen("docs"),
            prev,
            "insert_rows with an empty batch must not bump the generation"
        );
        assert_eq!(
            storage.current_generation().expect("global generation"),
            global_before,
            "insert_rows with an empty batch must not commit (global generation advanced)"
        );
        assert_eq!(read_gen("sibling"), sibling_gen);

        insert_typed_row(
            &storage,
            "docs",
            &a,
            3,
            Visibility::Public,
            &[crate::row_codec::Value::Vector(vec![0.2, 0.3])],
            &op("bump-insert-typed-row"),
        )
        .expect("insert_typed_row");
        let next = read_gen("docs");
        assert!(next > prev, "insert_typed_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        update_row(
            &storage,
            "docs",
            &a,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[9.0, 9.0],
                metadata: b"one-updated",
            },
            &op("bump-update-row"),
        )
        .expect("update_row");
        let next = read_gen("docs");
        assert!(next > prev, "update_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        // `update_row_columns_unchecked`（Issue #865・SQL-17・TASK-191。列指定の
        // 部分更新入口）は 1 行更新・0 行更新のいずれでも世代を進める
        // （`update_row_columns_unchecked` ドキュメントの非対称設計）。
        let update_columns_op = op("bump-update-row-columns");
        let ledger_write = LedgerWrite::Record(&update_columns_op);
        update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            // id=3 は `insert_typed_row`（`encode_scalar_columns` 経由の正規
            // metadata レイアウト）で書き込まれた行。id=1 は本テスト冒頭で
            // `insert_row`／`update_row`（全行置換 API・任意バイト列の raw
            // metadata）が上書き済みのため、スキーマ整合前提の
            // `decode_scalar_columns` を要する列指定更新の対象には使えない。
            3,
            &[(0, crate::row_codec::Value::Vector(vec![5.0, 5.0]))],
            ledger_write,
            None,
        )
        .expect("update_row_columns_unchecked (1 row)");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "update_row_columns_unchecked (1 row) must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 0 行更新（未存在 id）でも台帳記録・世代進行が必ず発生する（判断 B。
        // `truncate_table_unchecked` と同じ非対称設計）。
        let update_columns_zero_op = op("bump-update-row-columns-zero");
        let ledger_write_zero = LedgerWrite::Record(&update_columns_zero_op);
        let rows_affected = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            999,
            &[(0, crate::row_codec::Value::Vector(vec![6.0, 6.0]))],
            ledger_write_zero,
            None,
        )
        .expect("update_row_columns_unchecked (0 rows)");
        assert_eq!(rows_affected, 0);
        let next = read_gen("docs");
        assert!(
            next > prev,
            "update_row_columns_unchecked (0 rows) must still bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        delete_row(&storage, "docs", &a, 2, &op("bump-delete-row")).expect("delete_row");
        let next = read_gen("docs");
        assert!(next > prev, "delete_row must bump docs' generation");
        assert_eq!(read_gen("sibling"), sibling_gen);

        let file_docs_gen_before_replace = read_gen("docs");
        replace_typed_rows_by_text_key(
            &storage,
            &a,
            ReplaceByTextKey {
                table: "docs",
                key_column: "path",
                key_value: "nonexistent-key",
                visibility: Visibility::Private,
                rows: &[],
                content_hash_path: "irrelevant",
                content_hash_body: "",
                content_hash_template_values: &[],
                ledger_write: LedgerWrite::Disabled,
            },
        )
        // `schema("docs")` は `path` 列を持たないため、`key_column` 探索が
        // 見つからない列として `Err(Invalid)` を返す。これは意図的（`docs` は
        // 埋め込み専用スキーマのため）で、本テストの関心は「世代を進めないこと」
        // のみなので `Err` を許容し、世代不変のみ確認する。
        .ok();
        assert_eq!(
            read_gen("docs"),
            file_docs_gen_before_replace,
            "a rejected replace_typed_rows_by_text_key call must not bump the generation"
        );

        // `replace_typed_rows_by_text_key` の実変更経路は
        // `path`/`body` 列を持つ別テーブル（`file_schema` 相当）で確認する
        // （`replace_same_path_replaces_rows_and_leaves_other_paths_untouched` と
        // 同型のスキーマ）。
        let file_table = "docs_file";
        storage
            .create_table(&file_schema(file_table))
            .expect("create file-shaped table");
        let file_gen_before = read_gen(file_table);
        replace_typed_rows_by_text_key(
            &storage,
            &a,
            ReplaceByTextKey {
                table: file_table,
                key_column: "path",
                key_value: "note.txt",
                visibility: Visibility::Private,
                rows: &[row_values([1.0, 0.0], "note.txt", "v1")],
                content_hash_path: "note.txt",
                content_hash_body: "v1",
                content_hash_template_values: &[],
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("replace_typed_rows_by_text_key (inserting)");
        let file_gen_after = read_gen(file_table);
        assert!(
            file_gen_after > file_gen_before,
            "replace_typed_rows_by_text_key must bump the target table's generation when it \
             inserts/removes rows"
        );
        // 対象外のテーブル（`docs`・`sibling`）はいずれも無変化。
        assert_eq!(read_gen("docs"), file_docs_gen_before_replace);
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 同一テーブル（`docs`）への他テナント（`tenant-b`）の書き込みは
        // 「無関係」ではなく引き続き `docs` の世代へ影響する（上記コメント参照。
        // `user_rows/{table}` は複数テナントの行を同居させる単一の物理テーブル
        // であり、辞書スナップショットは `tenant::visible_rows` を経由するため
        // 他テナントの可視行の増減が要求元テナントの辞書内容にも影響しうる）。
        let b = PolicyContext::new("tenant-b").expect("valid tenant");
        let docs_gen_before_other_tenant = read_gen("docs");
        insert_row(
            &storage,
            "docs",
            &b,
            100,
            &RowInput {
                tenant_id: "tenant-b",
                visibility: Visibility::Public,
                embedding: &[5.0, 5.0],
                metadata: b"other-tenant",
            },
            &op("bump-other-tenant-insert-row"),
        )
        .expect("insert_row (other tenant, same table)");
        assert!(
            read_gen("docs") > docs_gen_before_other_tenant,
            "a write to the same table by a different tenant must still bump the table's \
             generation (same-table writes are not treated as unrelated)"
        );
        assert_eq!(read_gen("sibling"), sibling_gen);
    }

    // Cursor Bugbot Medium 指摘・PR #989（Issue #865）: `TEXT` 列への SET 値が
    // ちょうど `row_codec::MAX_TEXT_FIELD_LEN`（4 MiB）の場合、列単体の長さ
    // 検査は通過するが `row_codec::encode_scalar_columns` のフレーミング
    // オーバーヘッド（presence(1)+長さ(4)）込みで
    // `row_codec::MAX_SCALAR_PAYLOAD_LEN` を超える。この超過判定を対象行
    // 探索より前の累計フレームサイズ検証（`update_row_columns_unchecked` 冒頭の
    // ループ）で行うことで、対象行の存在有無に関わらず同一の拒否になることを
    // 固定する（`sql_update_single_row.rs` の SQL 経由テストは `sql::lexer::
    // MAX_INPUT_LEN`（1 MiB）により 4 MiB の SET リテラルを構成できないため、
    // `update_row_columns_unchecked` を直接呼ぶ本テストでのみ再現できる）。
    #[test]
    fn update_row_columns_set_text_at_max_field_len_rejects_identically_regardless_of_row_existence(
    ) {
        let path = unique_db_path("update-columns-max-text-len");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let a = PolicyContext::new("tenant-a").expect("valid tenant");

        insert_typed_row(
            &storage,
            "docs",
            &a,
            1,
            Visibility::Private,
            &row_values([0.1, 0.2], "note.txt", "v1"),
            &OperationId::parse("seed-max-text-len").expect("valid operation_id"),
        )
        .expect("seed row");

        // `MAX_TEXT_FIELD_LEN`（4 MiB）ちょうどの SET 値（`body` 列、index=2）。
        let max_len_text = "x".repeat(4 * 1024 * 1024);
        let assignments = [(2, crate::row_codec::Value::Text(max_len_text))];

        // ケース (a): 対象行が存在する（id=1）。
        let op_existing = OperationId::parse("op-maxlen-existing").expect("valid operation_id");
        let err_existing = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            1,
            &assignments,
            LedgerWrite::Record(&op_existing),
            None,
        )
        .expect_err("SET value at MAX_TEXT_FIELD_LEN must overflow the scalar payload cap");

        // ケース (b): 対象行が存在しない（id=999）。
        let op_missing = OperationId::parse("op-maxlen-missing").expect("valid operation_id");
        let err_missing = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            999,
            &assignments,
            LedgerWrite::Record(&op_missing),
            None,
        )
        .expect_err(
            "SET value at MAX_TEXT_FIELD_LEN must overflow identically for a nonexistent row",
        );

        // 対象行の有無に関わらず同一のエラー文言（= 同一の判定経路）になる
        // ことを固定する（`Debug` 表現の比較。存在有無で異なる variant/detail
        // に分岐していないことを機械的に確認する）。
        assert_eq!(format!("{err_existing:?}"), format!("{err_missing:?}"));
        assert!(
            matches!(
                &err_existing,
                TenantWriteError::Catalog(CatalogError::Invalid(msg))
                    if msg.contains("scalar payload length")
            ),
            "unexpected error shape: {err_existing:?}"
        );

        // 対象行（id=1）は無変更のまま（束縛/検証段の拒否であり書き込みは
        // 発生しない）。`Storage::get` は単一グローバル行テーブル
        // （`ROWS_TABLE`）専用のため、名前付きテーブル（`user_rows/{table}`）の
        // 行は `get_row_from_table` で読む（`insert_typed_row` 系はこちらの
        // 物理テーブルへ書く。`arena.rs`・`core.rs` の既存利用箇所と同じ経路）。
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("row must still exist");
        let values = crate::row_codec::decode_scalar_columns(&file_schema("docs"), &row.metadata)
            .expect("decode scalar columns");
        assert_eq!(
            values[2],
            crate::row_codec::Value::Text("v1".to_string()),
            "row must be unchanged when the SET value is rejected before the write"
        );
    }

    // codex-review P0 指摘（PR #1007・Issue #882）: `validate_set_assignments` は
    // `TEXT`／`BOOLEAN` の SET 値を累計スカラーペイロード上限
    // （`MAX_SCALAR_PAYLOAD_LEN`）へ加算する一方、`REAL`／`DOUBLE PRECISION` の
    // 固定長エントリ（`SCALAR_REAL_ENTRY_LEN`／`SCALAR_DOUBLE_ENTRY_LEN`）を
    // 加算していなかった。TEXT 単体では上限を跨がないが REAL/DOUBLE を加えると
    // 跨ぐ長さの SET を構成すると、対象行が実在する場合のみ
    // `merge_encode_scalar_columns` が実データ構築時に上限超過で失敗し、
    // 未存在／他テナント所有の場合は対象行の探索前チェックを素通りしたまま
    // `UPDATE 0` の成功へ分岐してしまう（対象行の存在情報が漏れる）。本テストは
    // REAL/DOUBLE の固定長エントリも累計へ加算し、対象行の有無に関わらず
    // 対象行探索より前の同一チェックで同一の拒否になることを固定する
    // （上記 `update_row_columns_set_text_at_max_field_len_rejects_identically_
    // regardless_of_row_existence` と同じ流儀。SQL 経由の統合テストでは
    // `sql::lexer::MAX_INPUT_LEN`（1 MiB）により 4 MiB 級の SET リテラルを
    // 構成できないため、本テストのように `update_row_columns_unchecked` を
    // 直接呼ぶ必要がある）。
    #[test]
    fn update_row_columns_set_real_and_double_are_counted_toward_the_scalar_payload_cap() {
        let path = unique_db_path("update-columns-real-double-payload-cap");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("note", ColumnType::Text, false),
                ColumnDef::new("score", ColumnType::Real, false),
                ColumnDef::new("weight", ColumnType::Double, false),
            ],
        );
        storage.create_table(&schema).expect("create table");
        let a = PolicyContext::new("tenant-a").expect("valid tenant");

        insert_typed_row(
            &storage,
            "docs",
            &a,
            1,
            Visibility::Private,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("v1".to_string()),
                crate::row_codec::Value::Real(1.0),
                crate::row_codec::Value::Double(1.0),
            ],
            &OperationId::parse("seed-real-double-payload-cap").expect("valid operation_id"),
        )
        .expect("seed row");

        // `note` 単体では `MAX_SCALAR_PAYLOAD_LEN`（4 * 1024 * 1024 バイト）を
        // 跨がないが、`score`（REAL）・`weight`（DOUBLE）の固定長エントリを
        // 加算すると跨ぐ長さに調整する。
        let big_text = "x".repeat(4 * 1024 * 1024 - 10);
        let assignments = [
            (1, crate::row_codec::Value::Text(big_text)),
            (2, crate::row_codec::Value::Real(1.0)),
            (3, crate::row_codec::Value::Double(1.0)),
        ];

        // ケース (a): 対象行が存在する（id=1）。
        let op_existing =
            OperationId::parse("op-real-double-cap-existing").expect("valid operation_id");
        let err_existing = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            1,
            &assignments,
            LedgerWrite::Record(&op_existing),
            None,
        )
        .expect_err(
            "SET value combining TEXT with REAL/DOUBLE must overflow the scalar payload cap",
        );

        // ケース (b): 対象行が存在しない（id=999）。REAL/DOUBLE を累計へ
        // 加算していない場合、この呼び出しは対象行不在のまま `Ok(0)` を返して
        // しまい、上のケース (a) とは異なる応答（存在情報の漏えい）になる。
        let op_missing =
            OperationId::parse("op-real-double-cap-missing").expect("valid operation_id");
        let err_missing = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            999,
            &assignments,
            LedgerWrite::Record(&op_missing),
            None,
        )
        .expect_err(
            "SET value combining TEXT with REAL/DOUBLE must overflow identically for a \
             nonexistent row",
        );

        // 対象行の有無に関わらず同一のエラー文言（= 対象行探索より前の同一
        // チェックで拒否されたこと）を固定する。
        assert_eq!(format!("{err_existing:?}"), format!("{err_missing:?}"));
        assert!(
            matches!(
                &err_existing,
                TenantWriteError::Catalog(CatalogError::Invalid(msg))
                    if msg.contains("scalar payload length")
            ),
            "unexpected error shape: {err_existing:?}"
        );
    }

    // codex-review P1 指摘（PR #993 系・Issue #871）: 述語つき `UPDATE ... WHERE`
    // （[`update_rows_where_unchecked`]）の SET 値検証（[`validate_set_assignments`]。
    // `update_row_columns_unchecked` と共有）が、一致行の適用ループ内にしか無いと、
    // 一致行が 0 件のまま台帳へ記録され `UPDATE 0` の成功として `operation_id` が
    // 消費されてしまう。候補列挙・台帳記録より**前**に検証することで、一致行の
    // 有無に関わらず同一の拒否（不正な SET 値は台帳に一切痕跡を残さない）になる
    // ことを固定する。
    #[test]
    fn update_rows_where_unchecked_rejects_oversized_set_value_even_with_zero_matching_candidates()
    {
        let path = unique_db_path("predicate-update-max-text-len-zero-candidates");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let a = PolicyContext::new("tenant-a").expect("valid tenant");

        // テーブルは空のまま（＝どんな述語でも一致行は 0 件）。
        let max_len_text = "x".repeat(4 * 1024 * 1024);
        let assignments = [(2, crate::row_codec::Value::Text(max_len_text))];
        let content_hash_value =
            content_hash::ContentHash::for_test(b"predicate-update-oversized-set");
        let never_matches =
            |_c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> { Ok(false) };

        let op_id =
            OperationId::parse("op-pred-maxlen-zero-candidates").expect("valid operation_id");
        let err = update_rows_where_unchecked(
            &storage,
            "docs",
            &a,
            LedgerWrite::Record(&op_id),
            &content_hash_value,
            &[],
            None,
            &assignments,
            false,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            never_matches,
        )
        .expect_err("oversized SET value must be rejected even when zero rows would match");
        match err {
            PredicateDmlError::Write(TenantWriteError::Catalog(CatalogError::Invalid(msg))) => {
                assert!(
                    msg.contains("scalar payload length"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!(
                "expected TenantWriteError::Catalog(Invalid(..)) (client input error), got {other:?}"
            ),
        }

        // 台帳には一切記録されていないはず: 同じ `operation_id` を正当な SET 値で
        // 再送すると成功する（記録済みなら `DuplicateOperationId`／
        // `OperationIdContentMismatch` になるはず）。
        let ok_assignments = [(2, crate::row_codec::Value::Text("ok".to_string()))];
        let outcome = update_rows_where_unchecked(
            &storage,
            "docs",
            &a,
            LedgerWrite::Record(&op_id),
            &content_hash_value,
            &[],
            None,
            &ok_assignments,
            false,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            never_matches,
        )
        .expect(
            "operation_id must remain reusable because the rejected attempt left no ledger trace",
        );
        assert_eq!(outcome, PredicateDmlOutcome::Applied { rows_affected: 0 });
    }

    // codex-review P1 指摘（PR #993 系・Issue #871）: 述語つき `UPDATE` の適用段
    // （一致した既存行の `metadata` デコード）が失敗した場合、単一行版
    // `update_row_columns_unchecked`（`sql_update_single_row.rs`
    // `update_against_row_with_corrupt_stored_metadata_is_rejected_with_xx000`）と
    // 同じく `CatalogError::CorruptSchema`（サーバー内部事象・`XX000`）へ分類し、
    // `CatalogError::Invalid`（クライアント入力エラー・`22000`）に丸めないことを
    // 固定する。`sql::exec::execute_predicate_update` が実際に注入する述語
    // クロージャは候補判定時に必ず `row_codec::scan_scalar_columns` を実行するため
    // 本番経路ではこの適用段の破損検出には到達しないが（適用段の
    // `merge_row_for_update`〔Issue #996〕も内部で同じ `scan_scalar_columns` を
    // 呼ぶため候補判定時点で先に失敗する）、既存行の
    // metadata を参照しない述語（本テストの `match_all`）を注入する呼び出し元にも
    // 同じ分類契約を保証する API 契約として固定する。
    #[test]
    fn update_rows_where_unchecked_classifies_corrupt_stored_metadata_as_internal_error() {
        let path = unique_db_path("predicate-update-corrupt-metadata");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let a = PolicyContext::new("tenant-a").expect("valid tenant");

        let seed_op = OperationId::parse("seed-pred-corrupt").expect("valid operation_id");
        insert_row(
            &storage,
            "docs",
            &a,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[0.1, 0.2],
                metadata: b"\xff\xff not a valid scalar column encoding \xff\xff",
            },
            &seed_op,
        )
        .expect("seed row with corrupt stored metadata");

        let assignments = [(2, crate::row_codec::Value::Text("en".to_string()))];
        let content_hash_value =
            content_hash::ContentHash::for_test(b"predicate-update-corrupt-metadata");
        let match_all =
            |_c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> { Ok(true) };
        let op_id = OperationId::parse("op-pred-corrupt").expect("valid operation_id");

        let err = update_rows_where_unchecked(
            &storage,
            "docs",
            &a,
            LedgerWrite::Record(&op_id),
            &content_hash_value,
            &[],
            None,
            &assignments,
            false,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            match_all,
        )
        .expect_err("decode failure on stored data must not succeed");
        match err {
            PredicateDmlError::Write(TenantWriteError::Catalog(CatalogError::CorruptSchema(_))) => {
            }
            other => panic!(
                "expected TenantWriteError::Catalog(CorruptSchema(..)) (internal error), \
                 got {other:?}"
            ),
        }
    }

    // codex-review P1 指摘（Issue #871）: `update_rows_where_unchecked` が
    // 一致行ごとに `schema.validate_embedding_dim(embedding_value.len())` を
    // `VECTOR` 列への SET 有無にかかわらず無条件で呼んでいたため、
    // `validate_embedding_dim` が `VECTOR` 列を持たないスキーマで常に `Err` を
    // 返す契約と衝突し、正当な `TEXT` 列のみの述語つき `UPDATE` が一致行を
    // 持つだけで失敗していた。単一行版 `update_row_columns_unchecked` は
    // `VECTOR` 列への SET があった場合のみ次元検証しており、本テストは述語形を
    // 同じ契約に揃えたことを固定する。
    //
    // Issue #995 で全 INSERT 系入口（`insert_row`/`insert_typed_row`/
    // `insert_typed_row_unchecked`/`Storage::insert_row_into_table` 等）が
    // `VECTOR` 列なしテーブルへの書き込みを受理するようになったため、現在は
    // それらを経由してもよいが、本テストは `update_rows_where_unchecked`
    // （適用対象の関数そのもの）の検証に的を絞るため、引き続き `redb` への
    // 直接書き込みで種行を用意する。
    #[test]
    fn update_rows_where_unchecked_applies_text_assignments_on_table_without_vector_column() {
        let path = unique_db_path("predicate-update-no-vector-column");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "notes",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        storage
            .create_table(&schema)
            .expect("create table without a VECTOR column");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let write_txn = storage.begin_write_txn().expect("begin seed write txn");
        {
            let row_table_name = user_rows_table_name("notes");
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .expect("open row table for seeding");
            for (id, lang, body) in [(1u64, "ja", "a"), (2u64, "en", "b"), (3u64, "ja", "c")] {
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[
                        crate::row_codec::Value::Text(lang.to_string()),
                        crate::row_codec::Value::Text(body.to_string()),
                    ],
                )
                .expect("encode scalar columns");
                let row = RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[],
                    metadata: &metadata,
                };
                let encoded = encode_row(&row).expect("encode seeded row");
                row_table
                    .insert(("tenant-a", id), encoded.as_slice())
                    .expect("seed row without a VECTOR column");
            }
        }
        crate::catalog::bump_table_generation_in_txn(&write_txn, "notes")
            .expect("bump table generation for seeded rows");
        crate::recovery::commit_boundary::commit(write_txn).expect("commit seed txn");

        // SET 対象は `lang`（宣言順インデックス 0）。`body`（インデックス 1）は
        // 対象外のまま残ることも下で確認する。
        let assignments = [(0usize, crate::row_codec::Value::Text("fr".to_string()))];
        let content_hash_value =
            content_hash::ContentHash::for_test(b"predicate-update-no-vector-column");
        let match_lang_ja = |c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> {
            let existing = crate::row_codec::scan_scalar_columns(&schema, c.metadata)
                .expect("decode seeded metadata");
            Ok(matches!(
                existing
                    .first()
                    .copied()
                    .flatten()
                    .and_then(|v| v.as_text()),
                Some("ja")
            ))
        };
        let op_id = OperationId::parse("op-pred-no-vector-column").expect("valid operation_id");

        let outcome = update_rows_where_unchecked(
            &storage,
            "notes",
            &ctx,
            LedgerWrite::Record(&op_id),
            &content_hash_value,
            &[],
            None,
            &assignments,
            false,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            match_lang_ja,
        )
        .expect("predicate UPDATE on a table without a VECTOR column should succeed");
        assert_eq!(outcome, PredicateDmlOutcome::Applied { rows_affected: 2 });

        for (id, expected_lang, expected_body) in
            [(1u64, "fr", "a"), (2u64, "en", "b"), (3u64, "fr", "c")]
        {
            let row = storage
                .get_row_from_table("notes", "tenant-a", id)
                .expect("read back row");
            let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                .expect("decode updated metadata");
            assert_eq!(
                values,
                vec![
                    crate::row_codec::Value::Text(expected_lang.to_string()),
                    crate::row_codec::Value::Text(expected_body.to_string()),
                ],
                "row {id} must reflect the expected lang/body after the predicate UPDATE"
            );
        }
    }

    /// [`update_rows_where_unchecked`] の raw 読み出し（`user_rows/{table}` の
    /// 生バイト列）を取得する（Issue #996 のバイト同一性テスト専用ヘルパー。
    /// `storage.get_row_from_table` はデコード済み [`crate::storage::Row`] しか
    /// 返さないため、生バイトの比較にはこちらを使う）。
    fn raw_row_bytes(storage: &Storage, table: &str, tenant: &str, id: u64) -> Vec<u8> {
        let read_txn = storage.db().begin_read().expect("begin read txn");
        let row_table_name = user_rows_table_name(table);
        let row_table = read_txn
            .open_table(user_rows_table_def(&row_table_name))
            .expect("open row table for raw read");
        row_table
            .get(&(tenant, id))
            .expect("get row")
            .expect("row must exist")
            .value()
            .to_vec()
    }

    /// 述語つき UPDATE を `merge_row_for_update` へ統一する（Issue #996）前の
    /// 旧実装アルゴリズムを再現する参照オラクル（`crates/engine/src/row_codec.rs`
    /// の `apply_overrides_via_decode_then_encode` と対になる、embedding も
    /// 含めた行全体版）。
    fn legacy_predicate_update_reencode(
        schema: &TableSchema,
        existing_metadata: &[u8],
        existing_embedding: &[f32],
        assignments: &[(usize, crate::row_codec::Value)],
    ) -> (Vec<f32>, Vec<u8>) {
        let vector_idx = schema
            .columns
            .iter()
            .position(|c| matches!(c.ty, ColumnType::Vector(_)));
        let mut merged_values = crate::row_codec::decode_scalar_columns(schema, existing_metadata)
            .expect("decode scalar columns (legacy oracle)");
        let mut embedding_value: Vec<f32> = existing_embedding.to_vec();
        for (col_idx, value) in assignments {
            if Some(*col_idx) == vector_idx {
                match value {
                    crate::row_codec::Value::Vector(v) => embedding_value = v.clone(),
                    _ => panic!("VECTOR column SET value must be a vector (legacy oracle)"),
                }
            } else {
                let slot = merged_values
                    .get_mut(*col_idx)
                    .expect("SET target column index out of range (legacy oracle)");
                *slot = value.clone();
            }
        }
        let metadata = crate::row_codec::encode_scalar_columns(schema, &merged_values)
            .expect("encode scalar columns (legacy oracle)");
        (embedding_value, metadata)
    }

    /// Issue #996: 述語つき UPDATE の適用段（`update_rows_where_unchecked`）を
    /// 単一行 UPDATE（`update_row_columns_unchecked`）と共有する
    /// `merge_row_for_update` へ統一した後も、書き込まれる行の生バイト列が
    /// 旧実装（[`legacy_predicate_update_reencode`]）と完全に同一であることを
    /// 固定する。TEXT のみ SET・既存 NULL 列を値へ SET・VECTOR のみ SET・
    /// VECTOR と TEXT を同時に SET の 4 ケースを、各ケース専用の行 id へ
    /// 適用して検証する（`Value::Null` を SET 対象値とする経路は `bind_update`
    /// の `InsertLiteral` に `NULL` 相当の variant が無く SQL 表層から到達
    /// しないため、`validate_set_assignments` の既存の型検証〔本 Issue の
    /// スコープ外〕がそのまま拒否する。「既存 NULL → SET NULL」の同型経路は
    /// `row_codec::tests::merge_encode_scalar_columns_matches_decode_then_
    /// encode_scalar_columns` が `validate_set_assignments` を経由せず直接
    /// 固定済み）。非一致行（述語に一致しない行）の生バイトが適用前と同一の
    /// ままであることもあわせて確認する。
    #[test]
    fn update_rows_where_unchecked_writes_byte_identical_rows_to_legacy_reencode_algorithm() {
        // clippy::type_complexity 対応（`(u64, Value, Vec<(usize, Value)>)` を
        // 直接配列要素型に書くとネストが深く可読性を損なうため、この
        // テストローカルな型エイリアスへ分解する）。
        type UpdateScenario = (
            u64,
            crate::row_codec::Value,
            Vec<(usize, crate::row_codec::Value)>,
        );

        let path = unique_db_path("predicate-update-byte-identical");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        );
        storage.create_table(&schema).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // (schema 列 index): embedding=0, path=1, tag=2。各シナリオは専用の
        // id・既存 tag 値を持つ行に対して単独で適用する。
        let scenarios: [UpdateScenario; 4] = [
            (
                10,
                crate::row_codec::Value::Text("orig-10".to_string()),
                vec![(2, crate::row_codec::Value::Text("updated-10".to_string()))],
            ),
            (
                11,
                crate::row_codec::Value::Null,
                vec![(2, crate::row_codec::Value::Text("filled-11".to_string()))],
            ),
            (
                12,
                crate::row_codec::Value::Text("orig-12".to_string()),
                vec![(0, crate::row_codec::Value::Vector(vec![9.0, 9.5]))],
            ),
            (
                13,
                crate::row_codec::Value::Text("orig-13".to_string()),
                vec![
                    (0, crate::row_codec::Value::Vector(vec![1.5, 2.5])),
                    (2, crate::row_codec::Value::Text("both-13".to_string())),
                ],
            ),
        ];

        for (id, tag, _) in &scenarios {
            insert_typed_row(
                &storage,
                "docs",
                &ctx,
                *id,
                Visibility::Private,
                &[
                    crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                    crate::row_codec::Value::Text(format!("path-{id}")),
                    tag.clone(),
                ],
                &OperationId::parse(&format!("seed-byte-identical-{id}"))
                    .expect("valid operation_id"),
            )
            .unwrap_or_else(|e| panic!("seed row {id} must succeed: {e:?}"));
        }

        // 述語に一切一致させない対照行（非改変の確認用）。
        insert_typed_row(
            &storage,
            "docs",
            &ctx,
            99,
            Visibility::Private,
            &[
                crate::row_codec::Value::Vector(vec![0.5, 0.6]),
                crate::row_codec::Value::Text("path-99".to_string()),
                crate::row_codec::Value::Text("untouched".to_string()),
            ],
            &OperationId::parse("seed-byte-identical-99").expect("valid operation_id"),
        )
        .expect("seed control row");
        let control_before = raw_row_bytes(&storage, "docs", "tenant-a", 99);

        for (id, _, assignments) in scenarios {
            // 適用前の生バイトを読み、参照オラクルで期待値を計算してから
            // 実装を実行し、書き込まれた生バイトと突き合わせる。
            let before = raw_row_bytes(&storage, "docs", "tenant-a", id);
            let existing = crate::storage::decode_row(id, &before).expect("decode seeded row");
            let (expected_embedding, expected_metadata) = legacy_predicate_update_reencode(
                &schema,
                &existing.metadata,
                &existing.embedding,
                &assignments,
            );
            let expected_row = RowInput {
                tenant_id: "tenant-a",
                visibility: existing.visibility,
                embedding: &expected_embedding,
                metadata: &expected_metadata,
            };
            let expected_bytes = encode_row(&expected_row).expect("encode expected row");

            let match_only_target_id =
                |c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> { Ok(c.id == id) };
            let content_hash_value =
                content_hash::ContentHash::for_test(format!("byte-identical-{id}").as_bytes());
            let op_id =
                OperationId::parse(&format!("op-byte-identical-{id}")).expect("valid operation_id");

            let outcome = update_rows_where_unchecked(
                &storage,
                "docs",
                &ctx,
                LedgerWrite::Record(&op_id),
                &content_hash_value,
                &[],
                None,
                &assignments,
                assignments.iter().any(|(idx, _)| *idx == 0),
                Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
                match_only_target_id,
            )
            .unwrap_or_else(|e| match e {
                PredicateDmlError::Write(w) => {
                    panic!("scenario id={id} must succeed (write error): {w}")
                }
                PredicateDmlError::Predicate(_) => {
                    panic!("scenario id={id} must succeed (predicate error)")
                }
            });
            assert_eq!(
                outcome,
                PredicateDmlOutcome::Applied { rows_affected: 1 },
                "scenario id={id} must match exactly its own row"
            );

            let actual_bytes = raw_row_bytes(&storage, "docs", "tenant-a", id);
            assert_eq!(
                actual_bytes, expected_bytes,
                "scenario id={id}: row bytes written by update_rows_where_unchecked must be \
                 byte-identical to the legacy re-encode algorithm"
            );
        }

        // 一度も述語に一致しなかった対照行は生バイトが完全に不変。
        let control_after = raw_row_bytes(&storage, "docs", "tenant-a", 99);
        assert_eq!(
            control_after, control_before,
            "a row that never matched the predicate must remain byte-identical"
        );
    }

    // PR #992 レビュー指摘（Issue #876）: `named_columns` をハッシュ計算前に
    // スキーマ列 index 順へ正規化する変更（表層をまたいだ再送の一貫性のため）
    // により、正規化を導入する**前**に宣言順のままハッシュ計算されて台帳へ
    // 記録されたエントリを同一 SQL で再送した場合に、内容不一致（`22023`）へ
    // 誤判定されないことを固定する。`ledger::record_in_txn`（正規化前の呼び出し
    // 契約と同じ、宣言順の列スライスをそのまま渡す形）で「アップグレード前に
    // 記録されたエントリ」を直接構築し、`update_row_columns_unchecked` を
    // 宣言順が非アルファベット順・非スキーマ列順の複数列 SET で呼び出す。
    #[test]
    fn update_row_columns_resend_matches_pre_normalization_declared_order_ledger_entry() {
        let path = unique_db_path("update-columns-legacy-hash-compat");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let a = PolicyContext::new("tenant-a").expect("valid tenant");

        insert_typed_row(
            &storage,
            "docs",
            &a,
            1,
            Visibility::Public,
            &row_values([0.1, 0.2], "note.txt", "v1"),
            &OperationId::parse("seed-legacy-hash-compat").expect("valid operation_id"),
        )
        .expect("seed row");

        // `file_schema("docs")` の列 index: embedding=0, path=1, body=2。
        // 宣言順（body, path）はスキーマ列順（path, body）と一致しない。
        let declared_order_columns: [(&str, &crate::row_codec::Value); 2] = [
            ("body", &crate::row_codec::Value::Text("v2".to_string())),
            (
                "path",
                &crate::row_codec::Value::Text("note2.txt".to_string()),
            ),
        ];
        let legacy_hash =
            crate::recovery::content_hash::for_update_columns(1, &declared_order_columns)
                .expect("legacy content hash");

        // 正規化導入前のコード（宣言順のままハッシュ計算する旧契約）が記録した
        // であろう台帳エントリを、旧 `record_in_txn` 呼び出し契約（`legacy_hashes`
        // なし）で直接再現する。行の書き込みは伴わない（台帳エントリの有無のみが
        // 本テストの関心事）。
        let legacy_op = OperationId::parse("op-legacy-declared-order").expect("valid operation_id");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        crate::recovery::ledger::record_in_txn(
            &write_txn,
            a.tenant_id(),
            "docs",
            LedgerWrite::Record(&legacy_op),
            &legacy_hash,
        )
        .expect("seed legacy ledger entry");
        write_txn
            .commit_raw_for_test()
            .expect("commit legacy ledger entry");

        // 同一 `operation_id`・同一内容（宣言順 body, path）を現行コード経由で
        // 再送する。現行コードはハッシュ計算前にスキーマ列順（path, body）へ
        // 正規化するため、正準ハッシュは `legacy_hash` と異なるが、
        // `record_in_txn_accepting` が宣言順の legacy_hash とも照合するため
        // 内容一致の再送（`Duplicate`）として扱われるはずである。
        let resend_assignments = [
            (2, crate::row_codec::Value::Text("v2".to_string())),
            (1, crate::row_codec::Value::Text("note2.txt".to_string())),
        ];
        let err = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            1,
            &resend_assignments,
            LedgerWrite::Record(&legacy_op),
            None,
        )
        .expect_err(
            "resend against a pre-normalization ledger entry must not succeed as a fresh write",
        );
        assert!(
            matches!(err, TenantWriteError::DuplicateOperationId),
            "resend with the same declared order as the pre-normalization entry must be \
             treated as a duplicate (23505), not a content mismatch (22023): {err:?}"
        );

        // 対照: 同一 `operation_id` だが内容が異なる再送は引き続き内容不一致に
        // なる（legacy_hash とのフォールバック照合が `22023` 契約を弱めていない
        // ことの確認）。
        let mismatched_assignments = [
            (2, crate::row_codec::Value::Text("different".to_string())),
            (1, crate::row_codec::Value::Text("note2.txt".to_string())),
        ];
        let err_mismatch = update_row_columns_unchecked(
            &storage,
            "docs",
            &a,
            1,
            &mismatched_assignments,
            LedgerWrite::Record(&legacy_op),
            None,
        )
        .expect_err("content-mismatched resend must still be rejected");
        assert!(
            matches!(err_mismatch, TenantWriteError::OperationIdContentMismatch),
            "unexpected error shape for mismatched resend: {err_mismatch:?}"
        );
    }

    /// 述語形 UPDATE の VECTOR 列 SET ハッシュ正規化（Issue #1061）に伴う
    /// legacy 互換テスト。[`update_row_columns_resend_matches_pre_normalization_declared_order_ledger_entry`]
    /// の predicate-form 版: 正規化前のコードが記録したであろう「`VECTOR` 列への
    /// `String` 割当をタグ 1・生テキストのまま連結したハッシュ」の台帳エントリを
    /// 直接シードし、現行コード（正準ハッシュ計算 + `legacy_hashes` 経由の照合）が
    /// 同一内容の再送を `Duplicate`（`23505`）として受理し、異なる内容の再送は
    /// 引き続き `ContentMismatch`（`22023`）として拒否することを固定する。
    #[test]
    fn update_rows_where_unchecked_resend_matches_pre_normalization_vector_string_ledger_entry() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let path = unique_db_path("predicate-update-legacy-vector-hash-compat");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");
        let a = PolicyContext::new("tenant-a").expect("valid tenant");

        insert_typed_row(
            &storage,
            "docs",
            &a,
            1,
            Visibility::Public,
            &row_values([0.1, 0.2], "note.txt", "v1"),
            &OperationId::parse("seed-legacy-vector-hash-compat").expect("valid operation_id"),
        )
        .expect("seed row");

        let schema = file_schema("docs");
        let registry = UdfRegistry::default();
        let vector_literal = InsertLiteral::String("[1,2]".to_string());
        let literal_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &vector_literal)];

        // 正規化前のコードが実際に記録したであろうハッシュ（タグ 1・生テキスト）。
        let legacy_hash = crate::recovery::content_hash::for_update_where_legacy_text_vector(
            "docs",
            &literal_assignments,
            &[],
            &registry,
        )
        .expect("legacy content hash");
        // 現行コードが新規記録に使う正準ハッシュ（タグ 5・f32 LE 列）。
        let canonical_hash = crate::recovery::content_hash::for_update_where(
            "docs",
            &literal_assignments,
            &[],
            &registry,
            &schema,
        )
        .expect("canonical content hash");
        assert_ne!(
            legacy_hash.as_bytes(),
            canonical_hash.as_bytes(),
            "legacy と canonical のハッシュ表現は異なるはず（正規化の前提）"
        );

        // 正規化前のコードが記録したであろう台帳エントリを、旧 `record_in_txn`
        // 契約（`legacy_hashes` なし）で直接シードする。
        let legacy_op = OperationId::parse("op-legacy-vector-string").expect("valid operation_id");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        ledger::record_in_txn(
            &write_txn,
            a.tenant_id(),
            "docs",
            LedgerWrite::Record(&legacy_op),
            &legacy_hash,
        )
        .expect("seed legacy ledger entry");
        write_txn
            .commit_raw_for_test()
            .expect("commit legacy ledger entry");

        // 同一 `operation_id`・同一内容（embedding=[1,2]）を現行コード経由で
        // 再送する。`content_hash_value` は正準ハッシュだが `legacy_hashes` に
        // シード済みの旧ハッシュを渡すため、`Duplicate` として扱われるはず。
        let resend_assignments = [(0usize, crate::row_codec::Value::Vector(vec![1.0, 2.0]))];
        let match_target =
            |c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> { Ok(c.id == 1) };
        let err = update_rows_where_unchecked(
            &storage,
            "docs",
            &a,
            LedgerWrite::Record(&legacy_op),
            &canonical_hash,
            std::slice::from_ref(&legacy_hash),
            None,
            &resend_assignments,
            true,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            match_target,
        )
        .expect_err(
            "resend against a pre-normalization ledger entry must not succeed as a fresh write",
        );
        assert!(
            matches!(
                err,
                PredicateDmlError::Write(TenantWriteError::DuplicateOperationId)
            ),
            "resend matching the pre-normalization legacy hash must be Duplicate (23505), \
             not ContentMismatch (22023): {err:?}"
        );

        // 対照: 内容が異なる再送は引き続き内容不一致になる（legacy 受理が
        // `22023` 契約を弱めていないことの確認）。同一 `operation_id` で
        // 異なる値（`[9,9]`）を送るため、正準・legacy 両ハッシュとも
        // 「その内容」から計算し直す（実運用の呼び出し元と同じ手順）。
        let mismatched_literal = InsertLiteral::String("[9,9]".to_string());
        let mismatched_literal_assignments: [(&str, &InsertLiteral); 1] =
            [("embedding", &mismatched_literal)];
        let mismatched_canonical_hash = crate::recovery::content_hash::for_update_where(
            "docs",
            &mismatched_literal_assignments,
            &[],
            &registry,
            &schema,
        )
        .expect("canonical content hash for mismatched content");
        let mismatched_legacy_hash =
            crate::recovery::content_hash::for_update_where_legacy_text_vector(
                "docs",
                &mismatched_literal_assignments,
                &[],
                &registry,
            )
            .expect("legacy content hash for mismatched content");

        let mismatched_assignments = [(0usize, crate::row_codec::Value::Vector(vec![9.0, 9.0]))];
        let match_target2 =
            |c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> { Ok(c.id == 1) };
        let err_mismatch = update_rows_where_unchecked(
            &storage,
            "docs",
            &a,
            LedgerWrite::Record(&legacy_op),
            &mismatched_canonical_hash,
            std::slice::from_ref(&mismatched_legacy_hash),
            None,
            &mismatched_assignments,
            true,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            match_target2,
        )
        .expect_err("content-mismatched resend must still be rejected");
        assert!(
            matches!(
                err_mismatch,
                PredicateDmlError::Write(TenantWriteError::OperationIdContentMismatch)
            ),
            "unexpected error shape for mismatched resend: {err_mismatch:?}"
        );
    }

    // codex-review P0 再指摘（PR #989・Issue #865。`crates/engine/src/tenant.rs:1750`
    // 指摘）: SET 対象の TEXT 値単体は上限内でも、対象行に既に格納されている
    // **未変更**の TEXT 列（`body`）と組み合わさって初めて `MAX_SCALAR_PAYLOAD_LEN`
    // を超えるケースは、対象行の有無に関わらず判定できる冒頭ループの範囲外である。
    // 判断 D 再改訂により、内容依存の処理（`scan_scalar_columns`・
    // `merge_encode_scalar_columns`）は `is_owner && is_visible` を満たす行に
    // 対してのみ実行する契約へ戻した（`update_row_unchecked` と同じ設計）。
    // 本テストは、同一の実データ（同一 id・同一 SET 値・同一の既存 `body`）に
    // 対し、対象行を見える `ctx`（`Private` 許可）では超過エラーになる一方、
    // 見えない `ctx`（`Public` のみ許可）では内容に一切触れず `UPDATE 0`
    // （不存在の id と同一の応答）になることを固定する
    // （`docs/design/update-single-row.md`「判断 D」参照）。
    #[test]
    fn update_row_columns_overflow_from_unchanged_column_is_rejected_only_when_visible_and_invisible_matches_not_found(
    ) {
        let path = unique_db_path("update-columns-overflow-visibility-parity");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");

        // `ctx_owner` は書き込み用（`insert_typed_row` は所有権のみを見るため
        // 可視性は問わない）。行の可視性そのものは `Visibility::Private` で
        // 固定し、読み取り側の 2 つの `ctx`（可視／不可視）で挙動を比較する。
        let ctx_owner = PolicyContext::new("tenant-a").expect("valid tenant");
        // `body`（未変更列）をほぼ上限一杯まで埋めておく。SET 対象の `path` は
        // 個別には上限内でも、この未変更 `body` と組み合わせると
        // `MAX_SCALAR_PAYLOAD_LEN` を超える大きさにする。
        let body_len = (4 * 1024 * 1024) - 1000;
        let large_body = "b".repeat(body_len);
        insert_typed_row(
            &storage,
            "docs",
            &ctx_owner,
            1,
            Visibility::Private,
            &row_values([0.1, 0.2], "orig", &large_body),
            &OperationId::parse("seed-overflow-visibility-parity").expect("valid operation_id"),
        )
        .expect("seed row");

        // SET 対象（`path`、index=1）。単体では `MAX_TEXT_FIELD_LEN` を大きく
        // 下回るが、上記 `large_body` との合算では上限を超える。
        let set_path_text = "p".repeat(2000);
        let assignments = [(1, crate::row_codec::Value::Text(set_path_text))];

        // ケース (a): 行が見える `ctx`（`Private` 許可）。
        let ctx_visible =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_visible = OperationId::parse("op-overflow-visible").expect("valid operation_id");
        let err_visible = update_row_columns_unchecked(
            &storage,
            "docs",
            &ctx_visible,
            1,
            &assignments,
            LedgerWrite::Record(&op_visible),
            None,
        )
        .expect_err(
            "SET path combined with the existing large body must overflow the scalar payload cap",
        );

        assert!(
            matches!(
                &err_visible,
                TenantWriteError::Catalog(CatalogError::Invalid(msg))
                    if msg.contains("scalar payload length")
            ),
            "unexpected error shape: {err_visible:?}"
        );

        // ケース (b): 同じ行だが見えない `ctx`（`Public` のみ許可・行は `Private`）。
        // 内容依存の処理（decode・merge・超過判定）には一切触れず、不存在の id と
        // 区別できない `UPDATE 0` 成功になる（P0 指摘の核心: エラー有無・種別で
        // 「不可視な行が存在し、かつ大きい／壊れている」ことを漏らさない）。
        let ctx_invisible = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_invisible = OperationId::parse("op-overflow-invisible").expect("valid operation_id");
        let rows_affected_invisible = update_row_columns_unchecked(
            &storage,
            "docs",
            &ctx_invisible,
            1,
            &assignments,
            LedgerWrite::Record(&op_invisible),
            None,
        )
        .expect(
            "an RLS-invisible existing row must not surface the unchanged-column overflow; it \
             must behave exactly like a nonexistent row (UPDATE 0)",
        );
        assert_eq!(
            rows_affected_invisible, 0,
            "RLS-invisible existing row must report UPDATE 0, identical to a nonexistent id"
        );

        // 対象行は無変更のまま（可視 ctx の呼び出しはマージ検証段で拒否され
        // 書き込みが発生せず、不可視 ctx の呼び出しは内容に一切触れない）。
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("row must still exist");
        let values = crate::row_codec::decode_scalar_columns(&file_schema("docs"), &row.metadata)
            .expect("decode scalar columns");
        assert_eq!(
            values[1],
            crate::row_codec::Value::Text("orig".to_string()),
            "row must be unchanged by either call"
        );
    }

    // codex-review P0 指摘（PR #1007・Issue #882・`crates/engine/src/tenant.rs:1695`
    // 指摘）: nullable `REAL`／`DOUBLE PRECISION` 列の SET は、対象行が現在 `NULL`
    // なら presence バイトのみ（1 byte）だが値ありへ更新すると固定長エントリ
    // （REAL: 5 bytes・DOUBLE: 9 bytes）へ増える。`validate_set_assignments` は
    // SET 句自身の値（更新後サイズ）は正しく累計するが、対象行に**既に格納
    // されている未変更列**の payload までは考慮しない冒頭ループの範囲外であり、
    // この増分は「未変更 TEXT 列と組み合わさって初めて上限を超える」ケース
    // （`update_row_columns_overflow_from_unchanged_column_is_rejected_only_when_
    // visible_and_invisible_matches_not_found`、判断 D 再改訂・
    // `docs/design/update-single-row.md`）と同じ内容依存クラスに属する。本テストは
    // その既存の受け入れ済み契約——`merge_encode_scalar_columns` を含む内容依存の
    // 処理は `is_owner && is_visible` を満たす行にのみ実行し、それ以外
    // （RLS 不可視・未存在・他テナント所有）は内容に一切触れず `UPDATE 0` と
    // 区別できない——が REAL の NULL→値あり遷移でも成立することを固定する
    // （単一行 UPDATE・述語 UPDATE の双方。述語 UPDATE の候補列挙スコープは
    // テナント所有のみ〔RLS 可視性フィルタではない〕ため他テナント行はそもそも
    // 候補にならず、可視性差の検証は単一行 UPDATE 側でのみ行う）。
    #[test]
    fn update_row_columns_nullable_real_null_to_value_transition_overflow_matches_visibility_parity(
    ) {
        let path = unique_db_path("update-columns-nullable-real-presence-parity");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("score", ColumnType::Real, true),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let ctx_owner = PolicyContext::new("tenant-a").expect("valid tenant");
        // `body` を、`score`（REAL）が NULL（presence 1 byte）の間は上限内だが
        // 値あり（5 bytes。+4 byte 増）へ遷移すると `MAX_SCALAR_PAYLOAD_LEN` を
        // 超えるちょうどの大きさに調整する
        // （text エントリ = presence(1)+len(4)+本文、`SCALAR_TEXT_ENTRY_OVERHEAD`)。
        let text_len = crate::row_codec::MAX_SCALAR_PAYLOAD_LEN - 8;
        let large_body = "b".repeat(text_len as usize);
        insert_typed_row(
            &storage,
            "docs",
            &ctx_owner,
            1,
            Visibility::Private,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text(large_body),
                crate::row_codec::Value::Null,
            ],
            &OperationId::parse("seed-real-presence-parity").expect("valid operation_id"),
        )
        .expect("seed row");

        let assignments = [(2, crate::row_codec::Value::Real(1.0))];

        // ケース (a): 行が見える `ctx`（`Private` 許可）。NULL→値あり遷移の
        // +4 byte 増分により上限を超え、merge 段でエラーになる。
        let ctx_visible =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_visible =
            OperationId::parse("op-real-presence-parity-visible").expect("valid operation_id");
        let err_visible = update_row_columns_unchecked(
            &storage,
            "docs",
            &ctx_visible,
            1,
            &assignments,
            LedgerWrite::Record(&op_visible),
            None,
        )
        .expect_err(
            "REAL NULL-to-value transition combined with the existing large body must overflow \
             the scalar payload cap",
        );
        assert!(
            matches!(
                &err_visible,
                TenantWriteError::Catalog(CatalogError::Invalid(msg))
                    if msg.contains("scalar payload length")
            ),
            "unexpected error shape: {err_visible:?}"
        );

        // ケース (b): 同じ行だが見えない `ctx`（`Public` のみ許可・行は
        // `Private`）。内容依存の処理（decode・merge・超過判定）には一切
        // 触れず、不存在の id と区別できない `UPDATE 0` 成功になる。
        let ctx_invisible = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_invisible =
            OperationId::parse("op-real-presence-parity-invisible").expect("valid operation_id");
        let rows_affected_invisible = update_row_columns_unchecked(
            &storage,
            "docs",
            &ctx_invisible,
            1,
            &assignments,
            LedgerWrite::Record(&op_invisible),
            None,
        )
        .expect(
            "an RLS-invisible existing row must not surface the presence-byte overflow; it must \
             behave exactly like a nonexistent row (UPDATE 0)",
        );
        assert_eq!(
            rows_affected_invisible, 0,
            "RLS-invisible existing row must report UPDATE 0, identical to a nonexistent id"
        );

        // ケース (c): 未存在の id（同じ可視 ctx）。存在する不可視行と同じ
        // `UPDATE 0` になり、応答から存在有無を区別できない。
        let op_missing =
            OperationId::parse("op-real-presence-parity-missing").expect("valid operation_id");
        let rows_affected_missing = update_row_columns_unchecked(
            &storage,
            "docs",
            &ctx_visible,
            999,
            &assignments,
            LedgerWrite::Record(&op_missing),
            None,
        )
        .expect("nonexistent id must report UPDATE 0");
        assert_eq!(rows_affected_missing, 0);

        // 対象行は無変更のまま（可視 ctx の呼び出しはマージ検証段で拒否され
        // 書き込みが発生せず、不可視 ctx の呼び出しは内容に一切触れない）。
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("row must still exist");
        let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
            .expect("decode scalar columns");
        assert_eq!(
            values[2],
            crate::row_codec::Value::Null,
            "row must be unchanged by either call (score remains NULL)"
        );
    }

    // 述語 UPDATE（`update_rows_where_unchecked`）版: 候補列挙スコープが
    // テナント所有のみ（RLS 可視性フィルタではない。上記単体テストのコメント
    // 参照）であるため、他テナント所有行はそもそも候補にならず存在情報が
    // 漏れる余地がない。本テストは、対象行が候補に含まれる場合の overflow
    // エラーと、対象行が候補に含まれない場合（未存在 id）の `rows_affected: 0`
    // が、それぞれの前提のもとで一貫することを固定する。
    #[test]
    fn update_rows_where_unchecked_nullable_real_null_to_value_transition_overflow() {
        let path = unique_db_path("update-where-nullable-real-presence-parity");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("score", ColumnType::Real, true),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let text_len = crate::row_codec::MAX_SCALAR_PAYLOAD_LEN - 8;
        let large_body = "b".repeat(text_len as usize);
        insert_typed_row(
            &storage,
            "docs",
            &ctx,
            1,
            Visibility::Private,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text(large_body),
                crate::row_codec::Value::Null,
            ],
            &OperationId::parse("seed-where-real-presence-parity").expect("valid operation_id"),
        )
        .expect("seed row");

        let assignments = [(2, crate::row_codec::Value::Real(1.0))];
        let op = OperationId::parse("op-where-real-presence-parity").expect("valid operation_id");
        let content_hash_value =
            content_hash::ContentHash::for_test(b"predicate-update-real-presence-parity");
        let matches_seeded_id =
            |c: &DmlCandidate<'_>| -> Result<bool, std::convert::Infallible> { Ok(c.id == 1) };
        let err = update_rows_where_unchecked(
            &storage,
            "docs",
            &ctx,
            LedgerWrite::Record(&op),
            &content_hash_value,
            &[],
            None,
            &assignments,
            false,
            Some(std::num::NonZeroUsize::new(100).expect("100 is nonzero")),
            matches_seeded_id,
        )
        .expect_err(
            "REAL NULL-to-value transition combined with the existing large body must overflow \
             the scalar payload cap for the matched candidate",
        );
        match err {
            PredicateDmlError::Write(TenantWriteError::Catalog(CatalogError::Invalid(msg))) => {
                assert!(
                    msg.contains("scalar payload length"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("unexpected error shape: {other:?}"),
        }

        // 対象行は無変更のまま（write_txn が commit されず破棄される）。
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("row must still exist");
        let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
            .expect("decode scalar columns");
        assert_eq!(
            values[2],
            crate::row_codec::Value::Null,
            "row must be unchanged after the aborted predicate UPDATE"
        );
    }

    #[test]
    // codex-review P0 指摘（PR #989・Issue #865。`crates/engine/src/tenant.rs:1696`
    // 指摘）: フル本体デコード（`decode_row_for_key`。embedding・metadata を
    // 含む）が `is_visible` 判定より前に無条件実行されると、不可視な既存行の
    // 本体（embedding・metadata）が破損している場合のデコード失敗（`XX000`）が
    // 「不存在（`UPDATE 0`）」と区別できてしまう。本テストは、行の物理データを
    // 直接（`encode_row`・`insert_row_unchecked` を経由せず、末尾を切り詰めて
    // `decode_row` が確実に失敗する形へ）破損させたうえで、その行を見えない
    // `ctx`（`Public` のみ許可・行は `Private`）で `update_row_columns_unchecked`
    // を呼び、エラーではなく不存在の id と区別できない `UPDATE 0` 成功になる
    // ことを固定する（ヘッダ〔tenant_id・visibility〕自体は健全なまま保つため、
    // 破損は本体デコード段のみで顕在化する）。
    fn update_row_columns_corrupt_invisible_row_body_is_indistinguishable_from_not_found() {
        let path = unique_db_path("update-columns-corrupt-invisible-body");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&file_schema("docs"))
            .expect("create table");

        let ctx_owner = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_typed_row(
            &storage,
            "docs",
            &ctx_owner,
            1,
            Visibility::Private,
            &row_values([0.1, 0.2], "orig", "body"),
            &OperationId::parse("seed-corrupt-invisible-body").expect("valid operation_id"),
        )
        .expect("seed row");

        // 正規にエンコードされた行バイト列の末尾を切り詰め、`decode_row` が
        // 確実に `Err`（`row buffer truncated at metadata field`）になる形へ
        // 破損させる。ヘッダ（version・tenant_len・tenant_id・visibility）は
        // 先頭側のため無傷のまま残る——本体デコード段のみが失敗する状況を
        // 再現するため。
        {
            let write_txn = storage.begin_write_txn().expect("begin write txn");
            {
                let row_table_name = user_rows_table_name("docs");
                let mut row_table = write_txn
                    .open_table(user_rows_table_def(&row_table_name))
                    .expect("open row table");
                let key = ("tenant-a", 1u64);
                let existing = row_table
                    .get(&key)
                    .expect("get existing row")
                    .expect("row must exist")
                    .value()
                    .to_vec();
                assert!(
                    existing.len() > 4,
                    "encoded row must be long enough to truncate meaningfully"
                );
                let corrupted = &existing[..existing.len() - 4];
                // ヘッダ（tenant_id・visibility）はまだ健全にデコードできる
                // ことを確認する（本体デコードのみを破損させる意図の担保）。
                let (header_tenant, header_visibility) = decode_row_tenant_and_visibility(
                    corrupted,
                )
                .expect("truncated row must still decode a valid header (tenant_id/visibility)");
                assert_eq!(header_tenant, "tenant-a");
                assert_eq!(header_visibility, Visibility::Private);
                // 一方でフル本体デコードは失敗する（末尾切り詰めにより
                // `metadata` フィールドが宣言長に届かない）。
                assert!(crate::storage::decode_row(1, corrupted).is_err());
                row_table
                    .insert(key, corrupted)
                    .expect("overwrite with corrupted body");
            }
            crate::catalog::bump_table_generation_in_txn(&write_txn, "docs")
                .expect("bump generation");
            crate::recovery::commit_boundary::commit(write_txn).expect("commit corruption");
        }

        let assignments = [(1, crate::row_codec::Value::Text("new-path".to_string()))];

        // 見えない `ctx`（`Public` のみ許可・行は `Private`）: 本体には一切
        // 触れず、エラーにもならず `UPDATE 0`（不存在の id と同一の応答）。
        let ctx_invisible = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_invisible =
            OperationId::parse("op-corrupt-invisible-body").expect("valid operation_id");
        let rows_affected = update_row_columns_unchecked(
            &storage,
            "docs",
            &ctx_invisible,
            1,
            &assignments,
            LedgerWrite::Record(&op_invisible),
            None,
        )
        .expect(
            "a corrupted RLS-invisible row body must not surface a decode error; it must \
             behave exactly like a nonexistent row (UPDATE 0)",
        );
        assert_eq!(
            rows_affected, 0,
            "corrupted invisible row body must report UPDATE 0, identical to a nonexistent id"
        );
    }

    // Issue #398: `insert_rows_unchecked` を連続 arena（`arena: Vec<u8>` ＋
    // `ranges: Vec<Range<usize>>`）へ置換した際の最大リスクは「範囲の取り違え
    // （別行のバイト列を書き込む・読み戻す）」であるため、metadata 長・
    // visibility が行ごとに異なる複数行を投入し、読み戻した各行が投入した
    // 値と一致することを機械的に検証する。
    #[test]
    fn insert_rows_batch_rows_round_trip_with_varying_lengths() {
        let path = unique_db_path("insert-rows-batch-round-trip");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let metadatas: Vec<Vec<u8>> = (0..8u32).map(|i| vec![i as u8; (i as usize) * 3]).collect();
        let visibilities = [
            Visibility::Public,
            Visibility::Private,
            Visibility::Public,
            Visibility::Private,
            Visibility::Public,
            Visibility::Private,
            Visibility::Public,
            Visibility::Private,
        ];
        let embeddings: Vec<[f32; 2]> = (0..8).map(|i| [i as f32, -(i as f32)]).collect();

        let rows: Vec<(u64, RowInput<'_>)> = (0..8u64)
            .map(|i| {
                (
                    i,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: visibilities[i as usize],
                        embedding: &embeddings[i as usize],
                        metadata: &metadatas[i as usize],
                    },
                )
            })
            .collect();

        insert_rows(
            &storage,
            "docs",
            &ctx,
            &rows,
            &OperationId::parse("round-trip-batch").expect("valid operation_id"),
        )
        .expect("insert_rows batch");

        for i in 0..8u64 {
            let row = storage
                .get_row_from_table("docs", "tenant-a", i)
                .expect("row must exist");
            assert_eq!(
                row.visibility, visibilities[i as usize],
                "row {i} visibility"
            );
            assert_eq!(
                row.embedding,
                embeddings[i as usize].to_vec(),
                "row {i} embedding"
            );
            assert_eq!(row.metadata, metadatas[i as usize], "row {i} metadata");
        }
    }

    /// 取りこぼし検査（Issue #849）: 本ファイルが提供する全書き込み経路
    /// （単文/バッチ INSERT・型付き単文/バッチ INSERT・UPDATE・DELETE・
    /// ファイル形 INSERT）が [`Storage::begin_write_txn`] choke point を
    /// 1 回ずつ経由することを、呼び出し前後の `write_txn_creations()` の
    /// 差分で非 vacuous に確認する（受け入れ条件2「取りこぼしがないことを
    /// 非 vacuous なテストで固定」対応。台帳〔`ledger::record_in_txn`〕は
    /// 各書き込み関数が開いた同一トランザクション内で呼ばれる設計のため、
    /// 台帳分の choke point 通過は本テストのカウンタ増分に自動的に含まれる）。
    #[test]
    fn all_tenant_write_paths_go_through_storage_choke_point() {
        let path = unique_db_path("durability-choke-point-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 生 `RowInput` 経路（メタデータ形が任意）用の素のテーブルと、型付き値配列
        // 経路（スキーマ列に沿った `TEXT` 列を要求）用のテーブルを分ける。両テーブルの
        // `create_table` 自体も choke point を経由するため、ここでは 2 テーブル作成後の
        // 値を基準に取り直し、以降の各操作が「+1」であることだけを確認する。
        storage
            .create_table(&schema("raw_docs"))
            .expect("create raw table");
        storage
            .create_table(&file_schema("typed_docs"))
            .expect("create typed table");
        let mut expected = storage.write_txn_creations();

        // 単文 INSERT（生 RowInput 経路）。
        insert_row(
            &storage,
            "raw_docs",
            &ctx,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: &[],
            },
            &OperationId::parse("choke-insert-row").expect("valid operation_id"),
        )
        .expect("insert_row");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "insert_row");

        // バッチ INSERT（生 RowInput 経路）。複数行でも単一トランザクション。
        insert_rows(
            &storage,
            "raw_docs",
            &ctx,
            &[
                (
                    2,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.0, 1.0],
                        metadata: &[],
                    },
                ),
                (
                    3,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[1.0, 1.0],
                        metadata: &[],
                    },
                ),
            ],
            &OperationId::parse("choke-insert-rows").expect("valid operation_id"),
        )
        .expect("insert_rows");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "insert_rows");

        // UPDATE。
        update_row(
            &storage,
            "raw_docs",
            &ctx,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[9.0, 9.0],
                metadata: &[],
            },
            &OperationId::parse("choke-update-row").expect("valid operation_id"),
        )
        .expect("update_row");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "update_row");

        // DELETE。
        delete_row(
            &storage,
            "raw_docs",
            &ctx,
            2,
            &OperationId::parse("choke-delete-row").expect("valid operation_id"),
        )
        .expect("delete_row");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "delete_row");

        // 単文 INSERT（型付き値配列経路。SQL 表層の単文 INSERT が最終的に通る経路）。
        insert_typed_row(
            &storage,
            "typed_docs",
            &ctx,
            4,
            Visibility::Public,
            &row_values([2.0, 0.0], "a.txt", "body-a"),
            &OperationId::parse("choke-insert-typed-row").expect("valid operation_id"),
        )
        .expect("insert_typed_row");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "insert_typed_row");

        // バッチ INSERT（型付き値配列経路。SQL 表層のバッチ INSERT が通る経路）。
        let op_id = OperationId::parse("choke-insert-typed-rows").expect("valid operation_id");
        let ledger_write = LedgerMode::Ledgered
            .resolve(Some(&op_id))
            .expect("resolve ledger write");
        insert_typed_rows_unchecked(
            &storage,
            "typed_docs",
            &ctx,
            Visibility::Public,
            &[(5, &row_values([0.0, 2.0], "b.txt", "body-b"))],
            ledger_write,
            None,
        )
        .expect("insert_typed_rows_unchecked");
        expected += 1;
        assert_eq!(
            storage.write_txn_creations(),
            expected,
            "insert_typed_rows_unchecked"
        );

        // ファイル形 INSERT（TASK-120 の同一パス置換書き込み経路）。
        replace_typed_rows_by_text_key(
            &storage,
            &ctx,
            ReplaceByTextKey {
                table: "typed_docs",
                key_column: "path",
                key_value: "c.txt",
                visibility: Visibility::Public,
                rows: &[row_values([3.0, 3.0], "c.txt", "body-c")],
                content_hash_path: "c.txt",
                content_hash_body: "body-c",
                content_hash_template_values: &[],
                ledger_write: LedgerWrite::Disabled,
            },
        )
        .expect("replace_typed_rows_by_text_key");
        expected += 1;
        assert_eq!(
            storage.write_txn_creations(),
            expected,
            "replace_typed_rows_by_text_key"
        );
    }

    /// RECOVER-5・台帳契約不変（Issue #849 受け入れ条件3）: `WriteDurability::None`
    /// を明示指定した `Storage` でも、`operation_id` 台帳照合による再送判定
    /// （同一内容の再送は [`TenantWriteError::DuplicateOperationId`]・内容不一致は
    /// [`TenantWriteError::OperationIdContentMismatch`]。TASK-101・RECOVER-10）が
    /// 既定（[`WriteDurability::Immediate`]）と同じ判定結果になることを固定する。
    /// durability は commit 前の設定であり、commit 成功境界（RECOVER-5）・台帳照合
    /// ロジック自体には影響しない契約を確認する。
    #[test]
    fn ledger_duplicate_and_mismatch_contract_is_unchanged_under_none_durability() {
        let path = unique_db_path("durability-none-ledger-contract");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open_with_durability(&path, crate::storage::WriteDurability::None)
            .expect("open storage with WriteDurability::None");
        storage.create_table(&schema("docs")).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("dur-none-ledger").expect("valid operation_id");

        // 初回は成功する。
        insert_row(
            &storage,
            "docs",
            &ctx,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: b"content-a",
            },
            &op_id,
        )
        .expect("first insert_row with WriteDurability::None must succeed");

        // 同一 operation_id・同一内容の再送は DuplicateOperationId（23505 相当）。
        let dup = insert_row(
            &storage,
            "docs",
            &ctx,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: b"content-a",
            },
            &op_id,
        );
        assert!(
            matches!(dup, Err(TenantWriteError::DuplicateOperationId)),
            "同一内容の再送は DuplicateOperationId であるべき: {dup:?}"
        );

        // 同一 operation_id・内容不一致は OperationIdContentMismatch（22023 相当）。
        let mismatch = insert_row(
            &storage,
            "docs",
            &ctx,
            2,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[9.0, 9.0],
                metadata: b"content-b",
            },
            &op_id,
        );
        assert!(
            matches!(mismatch, Err(TenantWriteError::OperationIdContentMismatch)),
            "内容不一致の再送は OperationIdContentMismatch であるべき: {mismatch:?}"
        );

        // 台帳照合が Err を返した書き込みは commit されない（行 id=2 は残らない）
        // ことを確認する。fail-closed の原子性契約（TASK-94・RECOVER-3）が
        // durability の値に関わらず維持されることの証跡。
        assert!(
            visible_rows(&storage, "docs", &ctx)
                .expect("visible_rows")
                .iter()
                .all(|r| r.id != 2),
            "内容不一致で拒否された書き込みは id=2 の行を残してはならない"
        );
    }

    /// `RETURNING` の投影コールバック（`delete_row_impl` の `project`。Issue #991・
    /// codex-review P1 指摘対応）が `Err` を返した場合に、行削除・台帳追記の
    /// **どちらも commit されない**（`write_txn` が abort される）ことを固定する。
    /// 修正前は `project_row` を commit **後**に呼んでいたため、この `Err` は
    /// 「DELETE は失敗応答なのに行は既に永続化されている」という commit
    /// 成功境界違反を起こし得た。
    #[test]
    fn delete_row_impl_aborts_commit_when_project_callback_fails() {
        let path = unique_db_path("delete-project-abort");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema("docs");
        storage.create_table(&schema).expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let insert_op_id = OperationId::parse("test-op-insert").expect("valid operation_id");
        insert_row(
            &storage,
            "docs",
            &ctx,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: &[],
            },
            &insert_op_id,
        )
        .expect("seed row");

        // 削除の operation_id は挿入と別にする（同一 op_id を使い回すと台帳の
        // 内容照合（TASK-101・RECOVER-10）が `for_insert` と `for_delete` の
        // ハッシュ不一致で `OperationIdContentMismatch` を返してしまい、本テストが
        // 検証したい「project コールバック失敗による abort」を確認できないため）。
        let delete_op_id = OperationId::parse("test-op-delete").expect("valid operation_id");
        let ledger_write = LedgerMode::Ledgered
            .resolve(Some(&delete_op_id))
            .expect("resolve ledger write");
        let mut project = |_: &CapturedRow| -> Result<(), TenantWriteError> {
            Err(TenantWriteError::ReturningProjectionFailed(
                "forced failure for test".to_string(),
            ))
        };
        let result = delete_row_ledgered_capturing_unchecked(
            &storage,
            "docs",
            &ctx,
            1,
            ledger_write,
            Some(&schema),
            Some(&mut project),
        );
        assert!(
            matches!(result, Err(TenantWriteError::ReturningProjectionFailed(_))),
            "project コールバックの Err はそのまま伝播するべき: {result:?}"
        );

        // 行削除は commit されず、行はまだ存在する。
        assert!(
            visible_rows(&storage, "docs", &ctx)
                .expect("visible_rows")
                .iter()
                .any(|r| r.id == 1),
            "project コールバック失敗時、行 id=1 は削除されてはならない \
             （commit 成功境界違反の防止）"
        );

        // 台帳への tentative 追記も commit されていないため、同一 operation_id を
        // 再送すると DuplicateOperationId/OperationIdContentMismatch ではなく
        // 通常の削除として成功する（1 回目が commit されていた場合、この 2 回目は
        // Record モードの「NotFound でも台帳を commit する」契約により
        // DuplicateOperationId になってしまうはずの操作）。
        let ledger_write2 = LedgerMode::Ledgered
            .resolve(Some(&delete_op_id))
            .expect("resolve ledger write");
        let retry = delete_row_ledgered_capturing_unchecked(
            &storage,
            "docs",
            &ctx,
            1,
            ledger_write2,
            None,
            None,
        );
        assert!(
            matches!(retry, Ok((DeleteRowOutcome::Deleted, None))),
            "project 失敗で abort された操作の operation_id は台帳に残ってはならず、\
             同一 operation_id での再送は通常の削除として成功するべき: {retry:?}"
        );
        assert!(
            visible_rows(&storage, "docs", &ctx)
                .expect("visible_rows")
                .iter()
                .all(|r| r.id != 1),
            "2 回目の削除は正常に commit され行 id=1 を削除するべき"
        );
    }

    // cursor bugbot 指摘（PR #990・Issue #872）の回帰テスト: `DO UPDATE SET` の
    // 右辺が `VECTOR` 列に対して `Value::Vector` 以外へ解決した場合、
    // `upsert_typed_rows_unchecked` は既存 embedding を黙って維持したまま
    // `updated` カウント・テーブル世代だけを進めてはならない（fail-closed に
    // 拒否し、行・世代とも変更しない）。`upsert_typed_rows_unchecked` は
    // `pub(crate)` のため、束縛段階の型検査（`sql::parser::bind_upsert_
    // assignments`）を経由せず直接 `UpsertAction::DoUpdate` を組み立てて
    // 呼び出すことで、この分岐を直接検証する。
    #[test]
    fn upsert_do_update_rejects_non_vector_value_for_vector_column_instead_of_ignoring_it() {
        let path = unique_db_path("upsert-vector-set-type-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");

        // `PolicyContext::new` は `Public` のみ可視（既定・最小権限。`policy.rs`
        // ドキュメント参照）。`Private` 行の可視化には明示的な許可が必要なため、
        // 本テストは擬似的なオーナー確認に `Public` 行を使う（衝突分岐に到達
        // できれば十分で、可視性ラベル自体は本テストの検証対象ではない）。
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("op-seed").expect("valid operation_id");
        insert_row(
            &storage,
            "docs",
            &ctx,
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: &[],
            },
            &op_id,
        )
        .expect("seed row insert must succeed");

        // vector_idx（唯一の VECTOR 列。位置 0）へ非 Vector（Null）を SET しようと
        // する不正な `UpsertAction`（本来は `bind_upsert_assignments` が束縛時に
        // 拒否するはずの形。本関数単体の fail-closed 契約を直接検証する）。
        let null_value = crate::row_codec::Value::Null;
        let assignments = [(0usize, UpsertSetValue::Literal(&null_value))];
        let action = UpsertAction::DoUpdate(&assignments);
        // 行の `values`（`(id, values)`）は衝突判定より前のハッシュ材料組み立て
        // でも `values[vector_idx]` を `Value::Vector` として要求するため、
        // 実際には使われない（`DO UPDATE` は SET 対象列だけを反映し `values` の
        // vector_idx はハッシュ用途のみ）が妥当な Vector 値を渡しておく。
        let seed_values = [crate::row_codec::Value::Vector(vec![9.0, 9.0])];
        let rows: [(u64, &[crate::row_codec::Value]); 1] = [(1, &seed_values)];

        let result = upsert_typed_rows_unchecked(
            &storage,
            "docs",
            &ctx,
            Visibility::Private,
            &rows,
            &UpsertTarget::RowId,
            &action,
            LedgerWrite::Disabled,
            None,
        );
        assert!(
            matches!(result, Err(TenantWriteError::Catalog(_))),
            "VECTOR 列への非 Vector SET は fail-closed に拒否されるべき: {result:?}"
        );

        // 拒否された呼び出しは commit されない（write_txn が早期 return で drop
        // される）ため、既存行の embedding は変更されない。
        let rows_after = visible_rows(&storage, "docs", &ctx).expect("visible_rows");
        let row1 = rows_after
            .iter()
            .find(|r| r.id == 1)
            .expect("seed row must still exist");
        assert_eq!(
            row1.embedding,
            vec![1.0, 0.0],
            "拒否された SET が既存 embedding を書き換えてはならない"
        );
    }
}
