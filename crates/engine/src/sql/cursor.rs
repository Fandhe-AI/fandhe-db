//! カーソル（`DECLARE`/`FETCH`/`CLOSE`。WIRE-15・TASK-218）の構文受理と
//! セッション内保持状態を提供する。
//!
//! 責務境界: `DECLARE <name> CURSOR FOR <SELECT>` の内側 SELECT（集計
//! `SELECT`〔SQL-13・SQL-14〕・広域取得 `SELECT`〔SQL-15〕に限る）の構造検証は
//! 既存の [`crate::sql::allowlist::validate_sql_tokens`] へそのまま委譲する
//! （第 2 のパーサを作らない設計）。本モジュールは
//! (1) `DECLARE`/`FETCH`/`CLOSE` 自体の規範形トークン検証、
//! (2) カーソル名の識別子検証、
//! (3) 確定済み [`crate::sql::exec::QueryResult`] を保持・払い出す
//! [`CursorRegistry`]、の 3 点を担う。
//!
//! カーソルの実体は `DECLARE` 実行時点で内側 SELECT を 1 回だけ実行して
//! 確定させた行集合であり（INSENSITIVE。以降の自トランザクションの書き込みで
//! 内容が変わらない）、`FETCH` は先頭からの払い出し位置を進めるだけで
//! 検索本体を再実行しない。カーソルの寿命はトランザクションの寿命に一致し、
//! [`crate::sql::transaction::ActiveTxn`] に埋め込まれるため、`COMMIT`／
//! `ROLLBACK`／`fail`／期限切れ／接続断のいずれでも自動的に消える（`Drop`
//! に委ねる。漏れる経路を持たない）。詳細な判断記録は
//! `docs/design/sql-cursor.md` 参照。

use crate::sql::allowlist::{SqlSurfaceError, Statement, TableLookup};
use crate::sql::exec::{Cell, QueryResult};
use crate::sql::lexer::{Keyword, Token};

/// 接続（セッション）が同時に開けるカーソル数の上限（実装既定値。
/// `docs/design/sql-cursor.md` 参照。内側 SELECT を実行する**前**に判定する）。
pub const MAX_CURSORS_PER_SESSION: usize = 16;

/// セッション内でカーソルが保持する確定済み行の合計バイト数上限（実装既定値）。
/// `wire-server::limits::MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION` と同じ
/// 16 MiB を採用する（同じ「未読のまま保持され続ける応答データを有界にする」
/// 設計根拠）。
pub const MAX_CURSOR_BYTES_PER_SESSION: usize = 16 * 1024 * 1024;

/// `DECLARE`/`FETCH`/`CLOSE`（許可リスト検証済み。TASK-218・WIRE-15）。
///
/// `Declare` の `query` は集計 `SELECT`（[`Statement::Aggregate`]）・広域取得
/// `SELECT`（[`Statement::Scan`]）のいずれかとして構造検証を通ったトークン列に
/// 限る（構造検証段で保証済み。実行時の再検証でも同じ制約を再確認する）。
#[derive(Debug, Clone, PartialEq)]
pub enum CursorStatement {
    Declare {
        name: String,
        /// `FOR` より後ろの内側 SELECT のトークン列（構文解析段では
        /// [`AlwaysExistsLookup`] による構造検証のみを通したもの）。実カタログ
        /// との照合——テーブル存在確認・ビュー展開（TABLE-18・SQL-23）——は
        /// `Active` なトランザクション内で [`validate_declare_inner`] を実
        /// カタログに対して再実行する時点まで遅延する（PR #1049 レビュー指摘
        /// codex P1 対応: 構造検証段の結果を保持すると、ビュー名がテーブル名と
        /// して残り実行時に `UndefinedTable` になるため）。
        query: Vec<Token>,
    },
    Fetch {
        name: String,
        /// `1..=`[`crate::core::MAX_SEARCH_K`] の範囲検証済み
        /// （[`crate::sql::parser::validate_search_limit`] を再利用する）。
        count: usize,
    },
    Close {
        name: String,
    },
}

/// カーソル名の識別子検証（テーブル名・列名と同じ [`crate::catalog::
/// validate_identifier`] 規則）。`CLOSE ALL`（バッチクローズ。本実装は
/// 対象外）はここで明示的に拒否する——`ALL` 自体は識別子として妥当な形の
/// ため、明示的に締め出さないと「たまたま `ALL` という名前のカーソルが
/// 存在する場合にだけ動く曖昧な構文」になってしまう。
fn validate_cursor_name(raw: &str) -> Result<String, SqlSurfaceError> {
    if raw.eq_ignore_ascii_case("ALL") {
        return Err(SqlSurfaceError::unsupported(
            "CLOSE ALL / FETCH ALL are not supported",
        ));
    }
    crate::catalog::validate_identifier(raw)
        .map_err(|_| SqlSurfaceError::unsupported("invalid cursor name"))?;
    Ok(raw.to_string())
}

/// `tokens` の先頭が識別子 `word`（大文字小文字を無視）であることを検証し、
/// 残りのトークン列を返す。
fn expect_contextual_ident<'a>(
    tokens: &'a [Token],
    word: &str,
) -> Result<&'a [Token], SqlSurfaceError> {
    match tokens.split_first() {
        Some((Token::Ident(w), tail)) if w.eq_ignore_ascii_case(word) => Ok(tail),
        _ => Err(SqlSurfaceError::unsupported(format!(
            "malformed cursor statement: expected {word}"
        ))),
    }
}

/// `DECLARE <name> CURSOR FOR <SELECT>`（規範形のみ。`SCROLL`／`NO SCROLL`／
/// `BINARY`／`INSENSITIVE`／`ASENSITIVE`／`WITH HOLD`／`WITHOUT HOLD` はいずれも
/// 未対応構文として `42601` へ落ちる——`name` の直後が必ず `CURSOR` であることを
/// 要求する構造上、これらの修飾語が割り込むと `expect_contextual_ident` が
/// 失敗する）。`FOR` より後ろのトークン列は再トークナイズせず
/// [`crate::sql::allowlist::validate_sql_tokens`] へそのまま渡す（`validate_insert_tokens`
/// 等と同じ「トークン列を受け取る本体」の方針）。
///
/// 受理する内側 SELECT は集計 `SELECT`（SQL-13・SQL-14）・広域取得 `SELECT`
/// （SQL-15）のみ（WIRE-15 の確定範囲）。ベクトル順位付けの検索 `SELECT`
/// （`Statement::Select`）・`EXPLAIN`・`SET`／`CREATE FUNCTION` は許可形状に
/// 一致しないものとして `42601` で拒否する。
///
/// 先頭トークンが `DECLARE`（大小無視）であることは呼び出し元
/// （`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_declare_tokens(
    tokens: &[Token],
    lookup: &impl TableLookup,
) -> Result<CursorStatement, SqlSurfaceError> {
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let (name_token, rest) = rest
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("malformed DECLARE statement"))?;
    let Token::Ident(raw_name) = name_token else {
        return Err(SqlSurfaceError::unsupported("malformed DECLARE statement"));
    };
    let name = validate_cursor_name(raw_name)?;
    let rest = expect_contextual_ident(rest, "CURSOR")?;
    let rest = expect_contextual_ident(rest, "FOR")?;
    // 構造検証のみ（結果の `Statement` は保持しない。実カタログでの再検証は
    // `Active` 内の実行時に [`validate_declare_inner`] で行う）。
    validate_declare_inner(rest, lookup)?;
    Ok(CursorStatement::Declare {
        name,
        query: rest.to_vec(),
    })
}

/// `DECLARE` の内側 SELECT（`FOR` より後ろのトークン列）を
/// [`crate::sql::allowlist::validate_sql_tokens`] で検証し、検証済み
/// [`Statement`] とその読み取り対象の基底テーブル名を返す。
///
/// 呼び出し元は 2 つ: (1) [`validate_declare_tokens`]（構文解析段。
/// [`AlwaysExistsLookup`] による構造検証のみ）、(2) `core.rs::EngineCore::
/// execute_cursor_in_active_txn`（`Active` なトランザクション内。実カタログ
/// ＝`catalog::Storage` を `lookup` に渡す）。(2) は通常の広域取得 `SELECT` と
/// 完全に同じ `validate_sql_tokens` の `Statement::Scan` 分岐を通るため、
/// FROM がビューを指す場合は `sql::view::resolve_from` による連鎖の畳み込み
/// （基底テーブル名＋ビュー由来 `WHERE` 述語・列スコープ検査）がそのまま
/// 適用される（第 2 のビュー展開を作らない）。返すテーブル名はビュー展開後の
/// 基底テーブル名であり、明示トランザクション内の `written_tables` 判定
/// （`sql::transaction` モジュールドキュメント「読み取りの既知の逸脱」参照）
/// にはこの名前を使う——ビュー名で判定すると、書き込み済みの基底テーブルを
/// ビュー越しに読む経路が判定をすり抜けるため。
pub(crate) fn validate_declare_inner(
    query: &[Token],
    lookup: &impl TableLookup,
) -> Result<(Statement, String), SqlSurfaceError> {
    if query.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "DECLARE requires a query after FOR",
        ));
    }
    let inner = crate::sql::allowlist::validate_sql_tokens(query, lookup)?;
    let table = match &inner {
        Statement::Aggregate(v) => v.table_name().to_string(),
        Statement::Scan(v) => v.table_name().to_string(),
        _ => {
            return Err(SqlSurfaceError::unsupported(
                "DECLARE only supports aggregate or wide-retrieval SELECT statements",
            ));
        }
    };
    Ok((inner, table))
}

/// 構造検証だけを行い、実カタログへは一切問い合わせない
/// [`TableLookup`]（常にテーブルが存在するとみなす）。
/// [`validate_declare_tokens_structure_only`] 専用。
struct AlwaysExistsLookup;

impl TableLookup for AlwaysExistsLookup {
    fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(true)
    }
}

/// `core.rs::parse_tokens`（`DECLARE` の唯一の構造検証入口。簡易・拡張
/// クエリ両プロトコルの Parse／実行が共有する）専用。[`validate_declare_tokens`]
/// は内側 SELECT の構造検証に続けてカタログ照会（対象テーブルの存在確認）まで
/// 一度に行うが、`DECLARE` はトランザクション文脈が確定するまで実行され
/// ない・トランザクション外では常に `25P01`（[`SqlSurfaceError::
/// NoActiveSqlTransaction`]）を返すべき契約（`core.rs::execute_parsed_in_session`
/// の `Cursor` 分岐参照）であるため、`parse_tokens` の時点でカタログへ
/// 問い合わせてしまうと、存在しないテーブルを指す `DECLARE` がトランザクション
/// 外でも `25P01` ではなく `UndefinedTable` を返してしまう（wire_code の
/// 互換性契約違反。PR #1049 レビュー指摘 P1 対応）。`DropTable`／
/// `CreateTable`（`sql::allowlist::validate_drop_table_tokens`／
/// `validate_create_table_tokens`）と同じ「構造検証はカタログを一切参照
/// しない・カタログ照会は実行本体が担う」設計を踏襲し、[`AlwaysExistsLookup`]
/// で内側 SELECT の構造検証だけを行う。実カタログとの照合（テーブル存在
/// 確認・ビュー展開・列解決）は `Active` なトランザクション内で内側 SELECT を
/// 実際に実行する時点まで遅延する（`core.rs::EngineCore::
/// execute_cursor_in_active_txn` が [`validate_declare_inner`] を実カタログ
/// に対して再実行し、続く `execute_cursor_inner_query` →
/// `read_txn_with_schema`／`bind_scan`／`bind_aggregate` が列を解決する）。
pub(crate) fn validate_declare_tokens_structure_only(
    tokens: &[Token],
) -> Result<CursorStatement, SqlSurfaceError> {
    validate_declare_tokens(tokens, &AlwaysExistsLookup)
}

/// `FETCH [FORWARD] <n> FROM <name>`（規範形のみ。`IN`／`ALL`／`NEXT`／
/// `BACKWARD`／`ABSOLUTE` はいずれも `n` の位置に `Token::Number` を要求する
/// 構造上、自然に `42601` へ落ちる）。`$n`（拡張クエリプロトコルのパラメータ
/// プレースホルダ）は本バージョンのスコープ外（`sql::params::
/// validate_param_positions` が先頭トークンで判定し fail-closed に拒否する）。
///
/// 先頭トークンが `FETCH`（大小無視）であることは呼び出し元
/// （`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_fetch_tokens(tokens: &[Token]) -> Result<CursorStatement, SqlSurfaceError> {
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let rest = match rest.split_first() {
        Some((Token::Ident(w), tail)) if w.eq_ignore_ascii_case("FORWARD") => tail,
        _ => rest,
    };
    let (count_token, rest) = rest
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("malformed FETCH statement"))?;
    let Token::Number(raw_count) = count_token else {
        return Err(SqlSurfaceError::unsupported("malformed FETCH statement"));
    };
    let raw: u32 = raw_count
        .parse()
        .map_err(|_| SqlSurfaceError::unsupported(format!("malformed FETCH count: {raw_count}")))?;
    let count = crate::sql::parser::validate_search_limit(raw)?;
    let rest = match rest.split_first() {
        Some((Token::Keyword(Keyword::From), tail)) => tail,
        _ => return Err(SqlSurfaceError::unsupported("expected FROM")),
    };
    let (name_token, rest) = rest
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("malformed FETCH statement"))?;
    let Token::Ident(raw_name) = name_token else {
        return Err(SqlSurfaceError::unsupported("malformed FETCH statement"));
    };
    let name = validate_cursor_name(raw_name)?;
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "unsupported clause after FETCH statement",
        ));
    }
    Ok(CursorStatement::Fetch { name, count })
}

/// `CLOSE <name>`（規範形のみ）。先頭トークンが `CLOSE`（大小無視）であることは
/// 呼び出し元（`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_close_tokens(tokens: &[Token]) -> Result<CursorStatement, SqlSurfaceError> {
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let (name_token, rest) = rest
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("malformed CLOSE statement"))?;
    let Token::Ident(raw_name) = name_token else {
        return Err(SqlSurfaceError::unsupported("malformed CLOSE statement"));
    };
    let name = validate_cursor_name(raw_name)?;
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "unsupported clause after CLOSE statement",
        ));
    }
    Ok(CursorStatement::Close { name })
}

/// 1 行の確定済み行が消費するバイト数の概算（DoS 対策の見積りであり厳密な
/// メモリ使用量ではない。`sql/group_by.rs` のグループキー累計バイト数判定と
/// 同じ「概算で頭打ちにする」設計方針）。整数演算はすべて `saturating_*` で行い
/// オーバーフローで未定義動作にしない（`.claude/rules/coding-rust.md`）。
fn estimate_row_bytes(row: &crate::sql::exec::ResultRow) -> usize {
    // `id`（u64）・`score`（f64）の固定オーバーヘッド。
    let mut total = 16usize;
    for cell in &row.cells {
        total = total.saturating_add(estimate_cell_bytes(cell));
    }
    total
}

fn estimate_cell_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Null | Cell::Bool(_) => 1,
        Cell::Integer(_) | Cell::SignedInteger(_) | Cell::Float(_) | Cell::Timestamp(_) => 8,
        Cell::Date(_) => 4,
        Cell::Text(s) => s.len().saturating_add(8),
        Cell::Bytes(b) => b.len().saturating_add(8),
        Cell::Json(s) => s.len().saturating_add(8),
        Cell::Vector(v) => v.len().saturating_mul(4).saturating_add(8),
        Cell::Array(arr) => estimate_array_bytes(arr),
        // `Numeric`／`Uuid` は内部表現の詳細を問わず一律の概算を用いる
        // （厳密なメモリ使用量ではなく DoS 対策の上限判定用途のため）。
        Cell::Numeric(_) => 32,
        Cell::Uuid(_) => 16,
    }
}

fn estimate_array_bytes(arr: &crate::row_codec::ArrayValue) -> usize {
    arr.approx_heap_bytes().saturating_add(8)
}

fn estimate_result_bytes(result: &QueryResult) -> usize {
    result.rows.iter().fold(0usize, |acc, row| {
        acc.saturating_add(estimate_row_bytes(row))
    })
}

/// 1 本の開いているカーソル（確定済み [`QueryResult`] と払い出し済み位置）。
#[derive(Debug)]
struct OpenCursor {
    result: QueryResult,
    /// 次回 `FETCH` が返す先頭行の添字（末尾に達したら
    /// `result.rows.len()` に一致し、以降の `FETCH` は常に 0 行を返す）。
    next_row: usize,
    /// [`estimate_result_bytes`] によるこのカーソルの概算バイト数
    /// （`CursorRegistry::total_bytes` から差し引くために保持する）。
    bytes: usize,
    /// この `DECLARE` インスタンス固有の識別子（[`CursorRegistry::next_id`]
    /// から払い出す単調増加値）。`CLOSE` 後に同じ名前で再 `DECLARE` した
    /// 別インスタンスと区別するために使う（PR #1049 レビュー指摘対応。
    /// `docs/design/sql-cursor.md`「wire-server 拡張クエリの portal 束縛」節
    /// 参照）。
    id: u64,
}

/// 明示トランザクション（[`crate::sql::transaction::ActiveTxn`]）が保持する
/// カーソルの集合。トランザクションの終了（`COMMIT`／`ROLLBACK`／`fail`／
/// 期限切れ／接続断）とともに `Drop` され、開いていたカーソルはすべて自動的に
/// 消える（`sql::exec::QueryResult` 自体の寿命はこれで尽きる設計。ただし
/// wire-server 拡張クエリプロトコルの portal は既に送出済みフレームを別途
/// バッファするため、この `Drop` だけでは portal 側の残存を防げない——
/// portal 側は [`crate::sql::transaction::SessionTransaction::cursor_id`] で
/// 個体識別子を突き合わせて無効化する。`docs/design/sql-cursor.md` 参照）。
#[derive(Debug, Default)]
pub(crate) struct CursorRegistry {
    cursors: std::collections::BTreeMap<String, OpenCursor>,
    total_bytes: usize,
    /// 次に `declare` するカーソルへ払い出す識別子（単調増加。オーバーフロー
    /// 目前で頭打ちにし `id` の再利用を避ける——`MAX_CURSORS_PER_SESSION` に
    /// より現実的には到達し得ないが、`.claude/rules/coding-rust.md` の
    /// checked 演算方針に従う）。
    next_id: u64,
}

impl CursorRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `DECLARE` 実行前チェック（内側 SELECT を実行する**前**に呼ぶ）。
    /// 重複名（`22000`）・同時カーソル数の上限超過（`54000`）を、内側 SELECT の
    /// 実行コストをかける前に検出する。
    fn check_can_declare(&self, name: &str) -> Result<(), SqlSurfaceError> {
        if self.cursors.contains_key(name) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "cursor \"{name}\" already exists"
            )));
        }
        if self.cursors.len() >= MAX_CURSORS_PER_SESSION {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "cursor count exceeds limit {MAX_CURSORS_PER_SESSION}"
            )));
        }
        Ok(())
    }

    /// [`Self::check_can_declare`] と同じ判定を、内側 SELECT を実行する前に
    /// `core.rs` から呼べるよう公開する（`pub(crate)`）。
    pub(crate) fn ensure_capacity_for_declare(&self, name: &str) -> Result<(), SqlSurfaceError> {
        self.check_can_declare(name)
    }

    /// セッションのカーソル合計バイト上限（[`MAX_CURSOR_BYTES_PER_SESSION`]）から
    /// 既存カーソルの保持量を差し引いた残容量（`DECLARE` の内側 SELECT の生成
    /// 予算。`core.rs::EngineCore::execute_cursor_in_active_txn` が実行前に呼ぶ。
    /// PR #1049 レビュー指摘 codex P1 対応）。
    pub(crate) fn remaining_bytes(&self) -> usize {
        MAX_CURSOR_BYTES_PER_SESSION.saturating_sub(self.total_bytes)
    }

    /// 内側 SELECT の実行結果を確定済みカーソルとして登録する。呼び出し元は
    /// 直前に [`Self::ensure_capacity_for_declare`] を呼び出し済みであること
    /// （同期実行のため、両者の間に他の変更が割り込む余地はない）。
    pub(crate) fn declare(
        &mut self,
        name: String,
        result: QueryResult,
    ) -> Result<(), SqlSurfaceError> {
        self.check_can_declare(&name)?;
        let bytes = estimate_result_bytes(&result);
        let total = self.total_bytes.saturating_add(bytes);
        if total > MAX_CURSOR_BYTES_PER_SESSION {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "cursor byte budget exceeds limit {MAX_CURSOR_BYTES_PER_SESSION}"
            )));
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("cursor identifier space exhausted".to_string())
        })?;
        self.total_bytes = total;
        self.cursors.insert(
            name,
            OpenCursor {
                result,
                next_row: 0,
                bytes,
                id,
            },
        );
        Ok(())
    }

    /// `name` のカーソルから未取得の先頭行を最大 `count` 件払い出す（末尾に
    /// 達していれば 0 行）。取得位置は本呼び出しで確定的に 1 回だけ進む
    /// （検索本体は再実行しない）。`name` が存在しない場合は `34000`
    /// （他セッション所有・不在のいずれも区別しない固定文言。security.md
    /// 「存在情報を漏らさない」対応）。
    ///
    /// 取得位置はエンジン実行の成功時点で進む（応答の送出確定を待たない）。これが
    /// 安全なのは、wire 側で応答生成・送出に失敗した場合は必ず明示トランザクションが
    /// `Failed` へ遷移（または切断で破棄）し、本レジストリごとカーソルが消える
    /// ためで、行を飛ばした位置から次の `FETCH` が再開する経路は存在しない
    /// （`wire-server::extended_query::execute_portal`・`simple_query` の応答失敗
    /// 経路・`handshake::post_auth_loop` 参照。PR #1049 レビュー指摘 codex P1）。
    pub(crate) fn fetch(
        &mut self,
        name: &str,
        count: usize,
    ) -> Result<QueryResult, SqlSurfaceError> {
        let cursor = self
            .cursors
            .get_mut(name)
            .ok_or_else(SqlSurfaceError::invalid_cursor_name)?;
        let total_rows = cursor.result.rows.len();
        let end = cursor.next_row.saturating_add(count).min(total_rows);
        let rows = cursor
            .result
            .rows
            .get(cursor.next_row..end)
            .unwrap_or(&[])
            .to_vec();
        cursor.next_row = end;
        Ok(QueryResult {
            columns: cursor.result.columns.clone(),
            rows,
        })
    }

    /// `name` のカーソルが存在すれば、その結果列メタデータを返す（Describe
    /// 専用の読み取り専用アクセサ。取得位置には一切触れない）。
    pub(crate) fn columns(&self, name: &str) -> Option<Vec<crate::sql::exec::ColumnMeta>> {
        self.cursors.get(name).map(|c| c.result.columns.clone())
    }

    /// `name` のカーソルが存在すれば、その `DECLARE` インスタンス固有の
    /// 識別子を返す（読み取り専用アクセサ。取得位置には一切触れない）。
    /// wire-server 拡張クエリの portal が、`FETCH` 実行時に束縛したカーソルが
    /// その後 `CLOSE`／再 `DECLARE` されていないか突き合わせるために使う
    /// （PR #1049 レビュー指摘対応。[`crate::sql::transaction::
    /// SessionTransaction::cursor_id`] 参照）。
    pub(crate) fn cursor_id(&self, name: &str) -> Option<u64> {
        self.cursors.get(name).map(|c| c.id)
    }

    /// `name` のカーソルを閉じる。不在は `34000`（[`Self::fetch`] と同じ
    /// 固定文言・同じ fail-closed 契約）。
    pub(crate) fn close(&mut self, name: &str) -> Result<(), SqlSurfaceError> {
        let removed = self
            .cursors
            .remove(name)
            .ok_or_else(SqlSurfaceError::invalid_cursor_name)?;
        self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::allowlist::SqlSurfaceError;

    struct FakeCatalog;
    impl TableLookup for FakeCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(name == "documents")
        }
    }

    fn tokens_of(sql: &str) -> Vec<Token> {
        crate::sql::lexer::tokenize(sql).expect("tokenize")
    }

    #[test]
    fn declare_accepts_scan_and_aggregate_inner_select() {
        let stmt = validate_declare_tokens(
            &tokens_of("DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 10"),
            &FakeCatalog,
        )
        .expect("scan form accepted");
        match stmt {
            CursorStatement::Declare { name, query } => {
                assert_eq!(name, "c");
                let (inner, table) =
                    validate_declare_inner(&query, &FakeCatalog).expect("inner revalidates");
                assert!(matches!(inner, Statement::Scan(_)));
                assert_eq!(table, "documents");
            }
            _ => panic!("expected Declare"),
        }

        let stmt = validate_declare_tokens(
            &tokens_of("DECLARE c2 CURSOR FOR SELECT COUNT(*) AS n FROM documents"),
            &FakeCatalog,
        )
        .expect("aggregate form accepted");
        assert!(matches!(stmt, CursorStatement::Declare { .. }));
    }

    #[test]
    fn declare_rejects_vector_ranking_select() {
        let err = validate_declare_tokens(
            &tokens_of(
                "DECLARE c CURSOR FOR SELECT id FROM documents ORDER BY embedding <=> '[1.0]' LIMIT 10",
            ),
            &FakeCatalog,
        )
        .expect_err("vector ranking SELECT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn declare_rejects_scroll_and_with_hold_modifiers() {
        for sql in [
            "DECLARE c SCROLL CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c BINARY CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c INSENSITIVE CURSOR FOR SELECT id FROM documents LIMIT 1",
        ] {
            let err = validate_declare_tokens(&tokens_of(sql), &FakeCatalog).expect_err("rejected");
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn fetch_accepts_optional_forward_and_rejects_other_forms() {
        let stmt = validate_fetch_tokens(&tokens_of("FETCH 10 FROM c")).expect("accepted");
        assert_eq!(
            stmt,
            CursorStatement::Fetch {
                name: "c".to_string(),
                count: 10
            }
        );
        let stmt = validate_fetch_tokens(&tokens_of("FETCH FORWARD 10 FROM c")).expect("accepted");
        assert_eq!(
            stmt,
            CursorStatement::Fetch {
                name: "c".to_string(),
                count: 10
            }
        );

        for sql in [
            "FETCH ALL FROM c",
            "FETCH NEXT FROM c",
            "FETCH BACKWARD 1 FROM c",
            "FETCH 0 FROM c",
        ] {
            let err = validate_fetch_tokens(&tokens_of(sql)).expect_err("rejected");
            assert!(matches!(err.wire_code(), "42601" | "22000"));
        }
    }

    #[test]
    fn close_rejects_all() {
        let err = validate_close_tokens(&tokens_of("CLOSE ALL")).expect_err("rejected");
        assert_eq!(err.wire_code(), "42601");
        let stmt = validate_close_tokens(&tokens_of("CLOSE c")).expect("accepted");
        assert_eq!(
            stmt,
            CursorStatement::Close {
                name: "c".to_string()
            }
        );
    }

    #[test]
    fn cursor_registry_enforces_count_and_byte_limits() {
        let mut registry = CursorRegistry::new();
        let empty = QueryResult {
            columns: vec![],
            rows: vec![],
        };
        for i in 0..MAX_CURSORS_PER_SESSION {
            registry
                .declare(format!("c{i}"), empty.clone())
                .expect("within limit");
        }
        let err = registry
            .declare("overflow".to_string(), empty.clone())
            .expect_err("count limit exceeded");
        assert_eq!(err.wire_code(), "54000");

        registry.close("c0").expect("close existing");
        registry
            .declare("c0".to_string(), empty)
            .expect("slot freed after close");
    }

    #[test]
    fn cursor_registry_rejects_duplicate_name() {
        let mut registry = CursorRegistry::new();
        let empty = QueryResult {
            columns: vec![],
            rows: vec![],
        };
        registry
            .declare("c".to_string(), empty.clone())
            .expect("first declare");
        let err = registry
            .declare("c".to_string(), empty)
            .expect_err("duplicate name rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    /// `CLOSE` 後に同名で再 `DECLARE` すると、`cursor_id` は必ず前のインスタンス
    /// と異なる値になる（単調増加。PR #1049 レビュー指摘対応: wire-server
    /// 拡張クエリの portal 束縛検証が名前だけでなくこの識別子で「別インスタンス」
    /// を判別できることの前提）。
    #[test]
    fn cursor_id_is_monotonic_across_close_and_redeclare_with_same_name() {
        let mut registry = CursorRegistry::new();
        let empty = QueryResult {
            columns: vec![],
            rows: vec![],
        };
        registry
            .declare("c".to_string(), empty.clone())
            .expect("first declare");
        let first_id = registry.cursor_id("c").expect("cursor exists");

        registry.close("c").expect("close succeeds");
        assert_eq!(
            registry.cursor_id("c"),
            None,
            "closed cursor must not resolve an id"
        );

        registry
            .declare("c".to_string(), empty)
            .expect("redeclare succeeds after close");
        let second_id = registry.cursor_id("c").expect("cursor exists again");

        assert_ne!(
            first_id, second_id,
            "redeclare with the same name must yield a distinct instance id"
        );
        assert!(
            second_id > first_id,
            "cursor id must increase monotonically"
        );
    }

    #[test]
    fn fetch_paginates_and_stops_at_end() {
        let mut registry = CursorRegistry::new();
        let rows: Vec<crate::sql::exec::ResultRow> = (0..5)
            .map(|i| crate::sql::exec::ResultRow {
                id: i,
                score: 0.0,
                cells: vec![],
            })
            .collect();
        registry
            .declare(
                "c".to_string(),
                QueryResult {
                    columns: vec![],
                    rows,
                },
            )
            .expect("declare");

        let page1 = registry.fetch("c", 2).expect("fetch page 1");
        assert_eq!(
            page1.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let page2 = registry.fetch("c", 2).expect("fetch page 2");
        assert_eq!(
            page2.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        let page3 = registry.fetch("c", 2).expect("fetch page 3");
        assert_eq!(page3.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![4]);
        let page4 = registry.fetch("c", 2).expect("fetch page 4 (empty)");
        assert!(page4.rows.is_empty());
    }

    #[test]
    fn fetch_and_close_reject_unknown_name_with_34000() {
        let mut registry = CursorRegistry::new();
        let err = registry.fetch("missing", 1).expect_err("not found");
        assert_eq!(err.wire_code(), "34000");
        let err = registry.close("missing").expect_err("not found");
        assert_eq!(err.wire_code(), "34000");
    }
}
