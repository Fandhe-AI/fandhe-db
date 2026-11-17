//! カーソル（`DECLARE`/`FETCH`/`MOVE`/`CLOSE`。WIRE-15・TASK-218・WIRE-27）の構文受理と
//! セッション内保持状態を提供する。
//!
//! 責務境界: `DECLARE <name> [SCROLL | NO SCROLL | INSENSITIVE | ASENSITIVE]... CURSOR
//! [WITHOUT HOLD] FOR <SELECT>` の内側 SELECT（集計
//! `SELECT`〔SQL-13・SQL-14〕・広域取得 `SELECT`〔SQL-15〕・JOIN〔CTE 入力を含む〕・集合演算に限る）の
//! 構造検証は既存の [`crate::sql::allowlist::validate_cursor_query_tokens`] へそのまま委譲する
//! （第 2 のパーサを作らない設計）。本モジュールは
//! (1) `DECLARE`/`FETCH`/`MOVE`/`CLOSE` 自体の規範形トークン検証（`FETCH`/`MOVE` は向きの文法を共有）、
//! (2) カーソル名の識別子検証、
//! (3) 確定済み [`crate::sql::exec::QueryResult`] を保持・払い出す
//! [`CursorRegistry`]、の 3 点を担う。
//!
//! カーソルの実体は `DECLARE` 実行時点で内側 SELECT を 1 回だけ実行して
//! 確定させた行集合であり（INSENSITIVE。以降の自トランザクションの書き込みで
//! 内容が変わらない）、`FETCH` は 0〜N+1 の位置（`CursorPosition`）を動かして行を払い出すだけで
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

/// FETCH の件数指定（Issue #3174・WIRE-15）。`ALL` を sentinel の `usize` で
/// 表さず明示的に区別し、上限判定を実行時（[`CursorRegistry::fetch`]）へ寄せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchCount {
    /// `validate_search_limit_with` 検証済みの件数（`NEXT` は 1）。
    Rows(usize),
    /// `ALL`／`FORWARD ALL`: 残り全行。上限は実行時に判定する。
    All,
}

/// FETCH の向き（Issue #3335・WIRE-27 (1)(2)）。構文解析段（[`validate_fetch_tokens`]）が
/// 生成し、実行段（`core.rs` の `Fetch` 実行アーム）が [`CursorRegistry::fetch`] へ渡す。
/// 位置の計算・NO SCROLL の拒否判定は [`CursorPosition`] が担う（構文と意味を分離する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchDirection {
    /// 前方向に進む。`NEXT`・`FETCH n`・`FORWARD [n]`・`FETCH ALL`・`FORWARD ALL`・`BACKWARD -n`。
    Forward(FetchCount),
    /// 逆方向に戻る（降順で返す）。`PRIOR`・`BACKWARD [n]`・`BACKWARD ALL`・`FETCH -n`・`FORWARD -n`。
    Backward(FetchCount),
    /// 件数 0。位置を動かさず現在の行を返す（行の上でなければ 0 行）。`FETCH 0`・`FORWARD 0`・
    /// `RELATIVE 0`（`backward: false`）と `BACKWARD 0`（`backward: true`）。`backward` は
    /// NO SCROLL の判定にだけ使う（`BACKWARD 0` は常に拒否）。
    Current {
        /// `BACKWARD 0` かどうか。
        backward: bool,
    },
    /// 絶対位置。`ABSOLUTE k`・`FIRST`（1）・`LAST`（-1）。負は末尾から数える。0 は先頭の前。
    Absolute(i64),
    /// 相対位置（0 以外）。`RELATIVE k`。途中の行は返さず移動先の 1 行だけを返す。
    Relative(i64),
}

/// `DECLARE` の `SCROLL`／`NO SCROLL` 指定（Issue #3334・WIRE-27 (2)）。指定なしは
/// [`CursorScroll::NoScroll`] と同じ扱い（PG は計画により指定なしでも逆方向を許すが、
/// 本 DB は許さない。意図した差。`docs/design/sql-cursor.md` 参照）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CursorScroll {
    /// 前方向のみ（`NO SCROLL`・指定なし）。逆方向の取得は `55000`。
    #[default]
    NoScroll,
    /// 逆方向の取得を許す（`SCROLL`）。
    Scroll,
}

/// `DECLARE`/`FETCH`/`CLOSE`（許可リスト検証済み。TASK-218・WIRE-15）。
///
/// `Declare` の `query` は集計 `SELECT`（[`Statement::Aggregate`]）・広域取得
/// `SELECT`（[`Statement::Scan`]）・JOIN（[`Statement::Join`]。CTE 入力を含む）・集合演算
/// （[`Statement::SetOperation`]）のいずれかとして構造検証を通ったトークン列に
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
        /// `SCROLL`／`NO SCROLL` 指定（指定なしは [`CursorScroll::NoScroll`]）。
        scroll: CursorScroll,
    },
    Fetch {
        name: String,
        /// 向きと件数（Issue #3335）。行数として扱う件数は絶対値が
        /// `1..=max_result_rows` の範囲検証済み（[`crate::sql::parser::validate_search_limit_with`]
        /// を再利用する）。`ALL` は [`FetchCount::All`]。
        direction: FetchDirection,
    },
    /// `MOVE`（Issue #3336・WIRE-27）。向き・件数は `Fetch` と同一の意味で、行を返さず
    /// 位置だけを動かす。BREAKING CHANGE: 公開 enum への variant 追加（網羅 match の更新が必要）。
    Move {
        name: String,
        direction: FetchDirection,
    },
    Close {
        name: String,
    },
}

/// `FETCH`／`MOVE` の構文解析を共有するための文の種別（エラー文言の出し分け専用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorMotion {
    Fetch,
    Move,
}

impl CursorMotion {
    /// エラー文言に埋め込む文名（英語の固定文言）。
    fn label(self) -> &'static str {
        match self {
            CursorMotion::Fetch => "FETCH",
            CursorMotion::Move => "MOVE",
        }
    }
}

/// カーソル名の識別子検証（テーブル名・列名と同じ [`crate::catalog::
/// validate_identifier`] 規則）。`CLOSE ALL`（バッチクローズ。本実装は
/// 対象外）はここで明示的に拒否する——`ALL` 自体は識別子として妥当な形の
/// ため、明示的に締め出さないと「たまたま `ALL` という名前のカーソルが
/// 存在する場合にだけ動く曖昧な構文」になってしまう。
fn validate_cursor_name(raw: &str) -> Result<String, SqlSurfaceError> {
    if raw.eq_ignore_ascii_case("ALL") {
        return Err(SqlSurfaceError::unsupported(
            "ALL is not allowed as a cursor name",
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

/// `DECLARE <name> [SCROLL | NO SCROLL | INSENSITIVE | ASENSITIVE | BINARY]...
/// CURSOR [WITH HOLD | WITHOUT HOLD] FOR <SELECT>`（Issue #3334・WIRE-27。PG17 の文法順）。
///
/// 修飾語の扱い: `SCROLL`／`NO SCROLL` は [`CursorScroll`] へ、`INSENSITIVE`／`ASENSITIVE`／
/// `WITHOUT HOLD` は受理して何もしない（もともと INSENSITIVE。同じ修飾語の重複は冪等）。
/// `SCROLL` と `NO SCROLL`、`INSENSITIVE` と `ASENSITIVE` の併記および構文の誤りは `42601`
/// （PG17 の `42P11` との差は `docs/design/sql-cursor.md` 参照）。`WITH HOLD`・`BINARY` は
/// `0A000`（未対応。fail-closed）。判定順は 修飾語・hold の構文 → 内側 SELECT の構造検証 →
/// `0A000`（構文が壊れた文は常に構文エラー）。`FOR` より後ろのトークン列は再トークナイズせず
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
    limits: crate::nesting_limits::NestingLimits,
) -> Result<CursorStatement, SqlSurfaceError> {
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let (name_token, rest) = rest
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("malformed DECLARE statement"))?;
    let (Token::Ident(raw_name) | Token::QuotedNull(raw_name)) = name_token else {
        return Err(SqlSurfaceError::unsupported("malformed DECLARE statement"));
    };
    let name = validate_cursor_name(raw_name)?;
    let (options, rest) = parse_declare_options(rest)?;
    let (with_hold, rest) = parse_declare_hold(rest)?;
    let rest = expect_contextual_ident(rest, "FOR")?;
    // 構造検証のみ（結果の `Statement` は保持しない。実カタログでの再検証は
    // `Active` 内の実行時に [`validate_declare_inner`] で行う）。
    validate_declare_inner(rest, lookup, limits)?;
    if with_hold {
        return Err(SqlSurfaceError::feature_not_supported(
            "WITH HOLD cursors are not supported",
        ));
    }
    if options.binary {
        return Err(SqlSurfaceError::feature_not_supported(
            "BINARY cursors are not supported",
        ));
    }
    Ok(CursorStatement::Declare {
        name,
        query: rest.to_vec(),
        scroll: options.scroll.unwrap_or_default(),
    })
}

/// `CURSOR` の前に並ぶ修飾語の解析結果（[`parse_declare_options`] 用）。
#[derive(Default)]
struct DeclareOptions {
    scroll: Option<CursorScroll>,
    insensitivity: Option<bool>,
    binary: bool,
}

/// `DECLARE <name>` の直後から `CURSOR` までの修飾語を読み、`CURSOR` の次のトークン列を返す
/// （[`validate_declare_tokens`] から呼ばれる）。許可リスト照合のみで、トークンの再結合・
/// 再トークナイズはしない。矛盾指定・未知の語・`CURSOR` 不在は `42601`。
fn parse_declare_options(
    mut rest: &[Token],
) -> Result<(DeclareOptions, &[Token]), SqlSurfaceError> {
    let malformed = || SqlSurfaceError::unsupported("malformed DECLARE statement");
    let conflict = || SqlSurfaceError::unsupported("conflicting DECLARE cursor options");
    let mut opts = DeclareOptions::default();
    loop {
        let Some((Token::Ident(word), tail)) = rest.split_first() else {
            return Err(malformed());
        };
        rest = tail;
        if word.eq_ignore_ascii_case("CURSOR") {
            return Ok((opts, rest));
        }
        if word.eq_ignore_ascii_case("SCROLL") {
            if opts.scroll == Some(CursorScroll::NoScroll) {
                return Err(conflict());
            }
            opts.scroll = Some(CursorScroll::Scroll);
        } else if word.eq_ignore_ascii_case("NO") {
            rest = expect_contextual_ident(rest, "SCROLL")?;
            if opts.scroll == Some(CursorScroll::Scroll) {
                return Err(conflict());
            }
            opts.scroll = Some(CursorScroll::NoScroll);
        } else if word.eq_ignore_ascii_case("INSENSITIVE")
            || word.eq_ignore_ascii_case("ASENSITIVE")
        {
            let insensitive = word.eq_ignore_ascii_case("INSENSITIVE");
            if opts.insensitivity.is_some_and(|prev| prev != insensitive) {
                return Err(conflict());
            }
            opts.insensitivity = Some(insensitive);
        } else if word.eq_ignore_ascii_case("BINARY") {
            opts.binary = true;
        } else {
            return Err(malformed());
        }
    }
}

/// `CURSOR` の直後の `WITH HOLD`／`WITHOUT HOLD`（省略可）を読み、保持指定の有無と残りの
/// トークン列を返す。`WITH`／`WITHOUT` の後に `HOLD` が無い場合は `42601`。
fn parse_declare_hold(rest: &[Token]) -> Result<(bool, &[Token]), SqlSurfaceError> {
    match rest.split_first() {
        Some((Token::Ident(w), tail)) if w.eq_ignore_ascii_case("WITH") => {
            Ok((true, expect_contextual_ident(tail, "HOLD")?))
        }
        Some((Token::Ident(w), tail)) if w.eq_ignore_ascii_case("WITHOUT") => {
            Ok((false, expect_contextual_ident(tail, "HOLD")?))
        }
        _ => Ok((false, rest)),
    }
}

/// `DECLARE` の内側 SELECT（`FOR` より後ろのトークン列）を
/// [`crate::sql::allowlist::validate_cursor_query_tokens`] で検証し、検証済み
/// [`Statement`] を返す（受理は集計・広域取得・JOIN〔CTE 入力を含む〕・集合演算。Issue #3175・#3176）。
///
/// 呼び出し元は 2 つ: (1) [`validate_declare_tokens`]（構文解析段。
/// [`AlwaysExistsLookup`] による構造検証のみ）、(2) `core.rs::EngineCore::
/// execute_cursor_in_active_txn`（`Active` なトランザクション内。実カタログ
/// ＝`catalog::Storage` を `lookup` に渡す）。(2) は通常の広域取得 `SELECT` と
/// 完全に同じ `validate_sql_tokens` の `Statement::Scan` 分岐を通るため、
/// FROM がビューを指す場合は `sql::view::resolve_from` による連鎖の畳み込み
/// （基底テーブル名＋ビュー由来 `WHERE` 述語・列スコープ検査）がそのまま
/// 適用される（第 2 のビュー展開を作らない）。読み取り源は呼び出し元が
/// `SessionTransaction::read_source` の三分岐で選ぶため、基底テーブル名は返さない
/// （JOIN は複数表で単一名を返せない）。
pub(crate) fn validate_declare_inner(
    query: &[Token],
    lookup: &impl TableLookup,
    limits: crate::nesting_limits::NestingLimits,
) -> Result<Statement, SqlSurfaceError> {
    if query.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "DECLARE requires a query after FOR",
        ));
    }
    let inner = crate::sql::allowlist::validate_cursor_query_tokens(query, lookup, limits)?;
    // 許可リストは `core.rs::execute_cursor_inner_query` の上限付き実行アームと 1 対 1 に対応する
    // （ここへ足す形は必ず実行側にも残容量で縛るアームを足す。無いものは `Internal` で止まる）。
    match &inner {
        Statement::Aggregate(_) | Statement::Scan(_) => {}
        // Issue #3175・#3176: JOIN 形（INNER／OUTER／CROSS／NATURAL／USING・集計形・LIMIT 省略形）。
        // CTE を入力に持つ JOIN（`cte_inputs` あり）も含む。
        Statement::Join(_) => {}
        // Issue #3176: 集合演算（UNION／UNION ALL／INTERSECT／EXCEPT）。
        Statement::SetOperation(_) => {}
        _ => {
            return Err(SqlSurfaceError::unsupported(
                "DECLARE only supports aggregate, wide-retrieval, JOIN or set operation SELECT statements",
            ));
        }
    }
    Ok(inner)
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
    limits: crate::nesting_limits::NestingLimits,
) -> Result<CursorStatement, SqlSurfaceError> {
    validate_declare_tokens(tokens, &AlwaysExistsLookup, limits)
}

/// `FETCH [ <direction> ] { FROM | IN } <name>`（Issue #3173・#3174・#3335・WIRE-15・WIRE-27 (1)）。
///
/// `<direction>` は `NEXT`・`PRIOR`・`FIRST`・`LAST`・`ABSOLUTE <n>`・`RELATIVE <n>`・
/// `<n>`・`ALL`・`FORWARD [<n> | ALL]`・`BACKWARD [<n> | ALL]`（`<n>` は符号付き整数）。
/// 向きの省略は `NEXT`、件数なしの `FORWARD` は `NEXT`、件数なしの `BACKWARD` は `PRIOR` と同じ。
/// `IN` は `FROM` の別名。負の件数は向きを反転する。件数 0 は現在の行の再取得
/// （[`FetchDirection::Current`]）。行数として扱う件数（`<n>`・`FORWARD <n>`・`BACKWARD <n>`）は
/// 絶対値を `validate_search_limit_with` に通し、上限設定との整合を 1 経路に揃える（0 を除く）。
/// `ABSOLUTE`／`RELATIVE` の数値は位置であり返す行は高々 1 行のため行数上限は適用せず、
/// `u32` に収まらない値のみ `42601`。
///
/// `FROM`／`IN` の省略・`PRIOR 1` のような余剰な件数・符号の重複などは `42601` で
/// fail-closed に拒否する（未知の形を前方向へ読み替える fail-open 経路を作らない）。
/// 向き語と `IN` は `Token::Ident`（大小無視）のみ受理する。字句解析は `IN` 等の引用を
/// 区別しない（従来挙動と同じ）が、受理後の意味は引用なしと同一で無害。
/// `$n`（拡張クエリのパラメータ）は `sql::params::validate_param_positions` が
/// 先頭トークンで判定し fail-closed に拒否する。
///
/// 先頭トークンが `FETCH`（大小無視）であることは呼び出し元
/// （`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_fetch_tokens(
    tokens: &[Token],
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<CursorStatement, SqlSurfaceError> {
    let (name, direction) = parse_motion_tokens(tokens, result_limits, CursorMotion::Fetch)?;
    Ok(CursorStatement::Fetch { name, direction })
}

/// `MOVE [ <direction> ] { FROM | IN } <name>`（Issue #3336・WIRE-27・WIRE-15）。
///
/// 向きと件数の文法・上限検証・拒否集合は `FETCH` と完全に同一（[`validate_fetch_tokens`] と
/// 共通の [`parse_motion_tokens`] を呼ぶ薄い入口）。違いはエラー文言が `MOVE` になることだけ。
/// 先頭トークンが `MOVE`（大小無視）であることは呼び出し元（`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_move_tokens(
    tokens: &[Token],
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<CursorStatement, SqlSurfaceError> {
    let (name, direction) = parse_motion_tokens(tokens, result_limits, CursorMotion::Move)?;
    Ok(CursorStatement::Move { name, direction })
}

/// `FETCH`／`MOVE` が共有する構文解析本体。`kind` はエラー文言の出し分けにだけ使う。
fn parse_motion_tokens(
    tokens: &[Token],
    result_limits: &crate::result_limits::ResultLimits,
    kind: CursorMotion,
) -> Result<(String, FetchDirection), SqlSurfaceError> {
    // 末尾の単一 `;` を受理する（Issue #3171）。
    let tokens = crate::sql::lexer::strip_one_trailing_semicolon(tokens);
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let (direction, rest) = parse_fetch_direction(rest, result_limits, kind)?;
    let rest = match rest.split_first() {
        Some((Token::Keyword(Keyword::From), tail)) => tail,
        Some((Token::Ident(w), tail)) if w.eq_ignore_ascii_case("IN") => tail,
        _ => return Err(SqlSurfaceError::unsupported("expected FROM or IN")),
    };
    let (name_token, rest) = rest.split_first().ok_or_else(|| {
        SqlSurfaceError::unsupported(format!("malformed {} statement", kind.label()))
    })?;
    let (Token::Ident(raw_name) | Token::QuotedNull(raw_name)) = name_token else {
        return Err(SqlSurfaceError::unsupported(format!(
            "malformed {} statement",
            kind.label()
        )));
    };
    let name = validate_cursor_name(raw_name)?;
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(format!(
            "unsupported clause after {} statement",
            kind.label()
        )));
    }
    Ok((name, direction))
}

/// `FROM`／`IN` の直前（向き語の後ろに件数が無い）かどうか。
fn at_from_or_in(rest: &[Token]) -> bool {
    match rest.first() {
        Some(Token::Keyword(Keyword::From)) => true,
        Some(Token::Ident(w)) => w.eq_ignore_ascii_case("IN"),
        _ => false,
    }
}

/// 先頭の `+`／`-` 符号（任意）と `u32` に収まる整数を読み、`(負か, 絶対値, 残り)` を返す。
/// 符号の重複・符号のみ・小数・`u32` 超過はすべて `42601`。
fn parse_signed_u32(
    rest: &[Token],
    kind: CursorMotion,
) -> Result<(bool, u32, &[Token]), SqlSurfaceError> {
    let malformed =
        || SqlSurfaceError::unsupported(format!("malformed {} statement", kind.label()));
    let (negative, rest) = match rest.split_first() {
        Some((Token::Punct('-'), tail)) => (true, tail),
        Some((Token::Punct('+'), tail)) => (false, tail),
        _ => (false, rest),
    };
    let (Token::Number(raw), after) = rest.split_first().ok_or_else(malformed)? else {
        return Err(malformed());
    };
    let magnitude: u32 = raw.parse().map_err(|_| {
        SqlSurfaceError::unsupported(format!("malformed {} count: {raw}", kind.label()))
    })?;
    Ok((negative, magnitude, after))
}

/// 行数として扱う符号付き件数から [`FetchDirection`] を作る。`backward` は語が
/// `BACKWARD` か（負の件数はさらに反転する）。絶対値 0 は [`FetchDirection::Current`]。
fn rows_direction(
    backward: bool,
    negative: bool,
    magnitude: u32,
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<FetchDirection, SqlSurfaceError> {
    if magnitude == 0 {
        // `FETCH -0`／`FORWARD -0` は PG と同じく 0 として前方向扱い。`BACKWARD 0` だけ後ろ向き。
        return Ok(FetchDirection::Current { backward });
    }
    let n = FetchCount::Rows(crate::sql::parser::validate_search_limit_with(
        magnitude,
        result_limits,
    )?);
    Ok(if backward != negative {
        FetchDirection::Backward(n)
    } else {
        FetchDirection::Forward(n)
    })
}

/// FETCH の向き・件数部を読み、[`FetchDirection`] と残りトークンを返す。
/// `validate_fetch_tokens` 専用。語は `Token::Ident`（大小無視）で許可リスト照合し、
/// 文字列の再結合・再トークナイズはしない。
fn parse_fetch_direction<'a>(
    rest: &'a [Token],
    result_limits: &crate::result_limits::ResultLimits,
    kind: CursorMotion,
) -> Result<(FetchDirection, &'a [Token]), SqlSurfaceError> {
    let malformed =
        || SqlSurfaceError::unsupported(format!("malformed {} statement", kind.label()));
    let next_one = || -> Result<FetchCount, SqlSurfaceError> {
        crate::sql::parser::validate_search_limit_with(1, result_limits).map(FetchCount::Rows)
    };
    // 向きの省略（`FETCH FROM c`・`FETCH IN c`）は NEXT。
    if at_from_or_in(rest) {
        return Ok((FetchDirection::Forward(next_one()?), rest));
    }
    let (first, tail) = rest.split_first().ok_or_else(malformed)?;
    let word = match first {
        Token::Ident(w) => Some(w.as_str()),
        _ => None,
    };
    let is = |kw: &str| word.is_some_and(|w| w.eq_ignore_ascii_case(kw));
    if is("NEXT") {
        return Ok((FetchDirection::Forward(next_one()?), tail));
    }
    if is("PRIOR") {
        return Ok((FetchDirection::Backward(next_one()?), tail));
    }
    if is("FIRST") {
        return Ok((FetchDirection::Absolute(1), tail));
    }
    if is("LAST") {
        return Ok((FetchDirection::Absolute(-1), tail));
    }
    if is("ALL") {
        return Ok((FetchDirection::Forward(FetchCount::All), tail));
    }
    if is("ABSOLUTE") || is("RELATIVE") {
        let absolute = is("ABSOLUTE");
        let (negative, magnitude, after) = parse_signed_u32(tail, kind)?;
        let magnitude = i64::from(magnitude);
        let value = if negative { -magnitude } else { magnitude };
        let direction = if absolute {
            FetchDirection::Absolute(value)
        } else if value == 0 {
            FetchDirection::Current { backward: false }
        } else {
            FetchDirection::Relative(value)
        };
        return Ok((direction, after));
    }
    if is("FORWARD") || is("BACKWARD") {
        let backward = is("BACKWARD");
        if at_from_or_in(tail) {
            let n = next_one()?;
            let direction = if backward {
                FetchDirection::Backward(n)
            } else {
                FetchDirection::Forward(n)
            };
            return Ok((direction, tail));
        }
        if let Some((Token::Ident(w), after)) = tail.split_first() {
            if w.eq_ignore_ascii_case("ALL") {
                let direction = if backward {
                    FetchDirection::Backward(FetchCount::All)
                } else {
                    FetchDirection::Forward(FetchCount::All)
                };
                return Ok((direction, after));
            }
        }
        let (negative, magnitude, after) = parse_signed_u32(tail, kind)?;
        return Ok((
            rows_direction(backward, negative, magnitude, result_limits)?,
            after,
        ));
    }
    // 件数のみ（`FETCH n`・`FETCH -n`・`FETCH +n`）。
    let (negative, magnitude, after) = parse_signed_u32(rest, kind)?;
    Ok((
        rows_direction(false, negative, magnitude, result_limits)?,
        after,
    ))
}

/// `CLOSE <name>`（規範形のみ）。先頭トークンが `CLOSE`（大小無視）であることは
/// 呼び出し元（`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_close_tokens(tokens: &[Token]) -> Result<CursorStatement, SqlSurfaceError> {
    // 末尾の単一 `;` を受理する（Issue #3171）。
    let tokens = crate::sql::lexer::strip_one_trailing_semicolon(tokens);
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let (name_token, rest) = rest
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("malformed CLOSE statement"))?;
    let (Token::Ident(raw_name) | Token::QuotedNull(raw_name)) = name_token else {
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

pub(crate) fn estimate_cell_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Null | Cell::Bool(_) => 1,
        Cell::Integer(_)
        | Cell::SignedInteger(_)
        | Cell::Float(_)
        | Cell::Timestamp(_)
        | Cell::Time(_)
        | Cell::TimestampTz(_) => 8,
        Cell::Date(_) => 4,
        Cell::Interval(_) => 16,
        // テキスト整形（projection・wire text・COPY・JSON）は n 文字の `0`／`1` へ展開されるため、
        // packed バイト数ではなくビット数で課金する。このため packed で 16 MiB 保持上限に
        // 収まる最大長付近の BIT(n) でも、展開後が上限を超える行は意図どおり 54000 で拒否する
        // （保持時にテキスト展開して返すメモリ量を上限で縛るための一貫した基準）。
        Cell::Bit(v) => usize::try_from(v.bit_len())
            .unwrap_or(usize::MAX)
            .saturating_add(8),
        // SPARSEVEC はテキスト整形後のおおよその大きさ（非ゼロ 1 要素あたり 32 バイト）で課金する。
        Cell::SparseVec(v) => v.nnz().saturating_mul(32).saturating_add(16),
        Cell::Text(s) => s.len().saturating_add(8),
        Cell::Bytes(b) => b.len().saturating_add(8),
        Cell::Json(s) => s.len().saturating_add(8),
        Cell::Vector(v) => v.len().saturating_mul(4).saturating_add(8),
        Cell::Array(arr) => estimate_array_bytes(arr),
        // `Numeric`／`Uuid` は内部表現の詳細を問わず一律の概算を用いる
        // （厳密なメモリ使用量ではなく DoS 対策の上限判定用途のため）。
        Cell::Numeric(_) => 32,
        Cell::Uuid(_) => 16,
        // SEARCH 順序列（SQL-35・Issue #2213）。経路長に比例して増えるため要素ごとに積み上げる。
        Cell::Record(fields) => fields
            .iter()
            .fold(8usize, |acc, c| acc.saturating_add(estimate_cell_bytes(c))),
        Cell::RecordArray(items) => items.iter().fold(8usize, |acc, rec| {
            rec.iter().fold(acc.saturating_add(8), |a, c| {
                a.saturating_add(estimate_cell_bytes(c))
            })
        }),
    }
}

fn estimate_array_bytes(arr: &crate::row_codec::ArrayValue) -> usize {
    arr.approx_heap_bytes().saturating_add(8)
}

pub(crate) fn estimate_result_bytes(result: &QueryResult) -> usize {
    result.rows.iter().fold(0usize, |acc, row| {
        acc.saturating_add(estimate_row_bytes(row))
    })
}

/// カーソルの位置（Issue #3334・WIRE-27 (5)(6)）。行数 `len`（N）に対し `0..=len+1` の値を取り、
/// `0` は先頭の前、`k`（1..=N）は k 行目の上、`N+1` は末尾の後を表す。
///
/// 不変条件: `pos <= len + 1`。すべての遷移は `saturating_*`・`min` で行い、範囲外へ出ない。
/// [`CursorRegistry`] の `fetch` 系が使う内部状態で、行集合の複製を持たない（O(1)）。
#[derive(Debug, Clone, Copy)]
struct CursorPosition {
    pos: usize,
    len: usize,
}

impl CursorPosition {
    /// 先頭の前（位置 0）から始める。
    fn new(len: usize) -> Self {
        Self { pos: 0, len }
    }

    /// 現在位置（`0..=len+1`）。
    fn get(&self) -> usize {
        self.pos
    }

    /// 前方向に最大 `n` 行進み、返す行の添字範囲（0 始まり）を返す。`p+n <= N` なら位置は
    /// `p+n`、それ以外（末尾を越える要求）は `N+1`。
    fn forward(&mut self, n: usize) -> std::ops::Range<usize> {
        let start = self.pos.min(self.len);
        let want = self.pos.saturating_add(n);
        let end = want.min(self.len);
        self.pos = if want <= self.len {
            want
        } else {
            self.len.saturating_add(1)
        };
        start..end
    }

    /// 残り全行を返し、位置を `N+1` へ進める。
    fn forward_all(&mut self) -> std::ops::Range<usize> {
        let start = self.pos.min(self.len);
        self.pos = self.len.saturating_add(1);
        start..self.len
    }

    /// 逆方向に最大 `n` 行戻り、返す行の添字範囲（0 始まり・昇順。呼び出し側が降順に並べる）を
    /// 返す。新しい位置は `p.saturating_sub(n)`。`p` が 0 または 1 なら 0 行。
    /// `N+1` からは N 行目から返す。
    fn backward(&mut self, n: usize) -> std::ops::Range<usize> {
        let old = self.pos.min(self.len.saturating_add(1));
        let new = old.saturating_sub(n);
        self.pos = new;
        // 位置 p は「p 行目の上」なので、戻って返す行は p-1 行目（添字 p-2）から
        // max(new,1) 行目（添字 max(new,1)-1）まで。添字範囲は [max(new,1)-1, p-1)。
        let hi = old.saturating_sub(1).min(self.len);
        let lo = new.max(1).saturating_sub(1).min(hi);
        lo..hi
    }

    /// 状態を変えずに `dir` の移動結果（新位置・返す行の添字範囲・降順か）を計算する。
    /// 行の切り出しと位置の計算を分けるための純粋関数で、`MOVE`（#3336）も再利用する。
    /// 返す行は常に 0 または 1 行（`Absolute`／`Relative`／`Current`）か、`Forward`／`Backward` の
    /// 件数分。`ABSOLUTE`／`RELATIVE` の巨大な値は O(1) の clamp で処理する（走査しない）。
    fn plan(&self, dir: FetchDirection) -> PlannedMove {
        let n = self.len;
        let end = n.saturating_add(1);
        // 位置 `t` の 1 行（`1 <= t <= N` のときだけ）。範囲外は空。
        let single = |t: usize| {
            if (1..=n).contains(&t) {
                t - 1..t
            } else {
                0..0
            }
        };
        let mut sim = *self;
        match dir {
            FetchDirection::Forward(FetchCount::Rows(k)) => {
                let range = sim.forward(k);
                PlannedMove {
                    new_pos: sim.pos,
                    range,
                    descending: false,
                }
            }
            FetchDirection::Forward(FetchCount::All) => {
                let range = sim.forward_all();
                PlannedMove {
                    new_pos: sim.pos,
                    range,
                    descending: false,
                }
            }
            FetchDirection::Backward(count) => {
                let k = match count {
                    FetchCount::Rows(k) => k,
                    FetchCount::All => usize::MAX,
                };
                let range = sim.backward(k);
                PlannedMove {
                    new_pos: sim.pos,
                    range,
                    descending: true,
                }
            }
            FetchDirection::Current { .. } => PlannedMove {
                new_pos: self.pos,
                range: single(self.pos),
                descending: false,
            },
            FetchDirection::Absolute(k) => {
                let mag = usize::try_from(k.unsigned_abs()).unwrap_or(usize::MAX);
                let t = if k > 0 {
                    mag.min(end)
                } else if k == 0 {
                    0
                } else if mag <= n {
                    end - mag
                } else {
                    0
                };
                PlannedMove {
                    new_pos: t,
                    range: single(t),
                    descending: false,
                }
            }
            FetchDirection::Relative(k) => {
                let mag = usize::try_from(k.unsigned_abs()).unwrap_or(usize::MAX);
                let t = if k > 0 {
                    self.pos.saturating_add(mag).min(end)
                } else {
                    self.pos.saturating_sub(mag)
                };
                PlannedMove {
                    new_pos: t,
                    range: single(t),
                    descending: false,
                }
            }
        }
    }

    /// [`Self::plan`] の結果を確定する（不変条件 `pos <= len + 1` を再確認して頭打ちにする）。
    fn apply(&mut self, planned: &PlannedMove) {
        self.pos = planned.new_pos.min(self.len.saturating_add(1));
    }

    /// NO SCROLL カーソルで `dir` が「後ろへ戻る走査」を要するか（要するなら `55000`）。
    /// PG17 の portal 実行の挙動から導いた判定で、PostgreSQL 17.11 の実測と一致を確認済み
    /// （Issue #3337。`tests/data/cursor_directions_pg17.tsv`・`docs/design/sql-cursor.md` §9）。
    /// `ABSOLUTE k`（k>0）は k が現在位置以前なら後退走査（末尾の後 `pos = len + 1` を含む）、
    /// より先なら前方向で済む。
    fn requires_backward_scan(&self, dir: FetchDirection) -> bool {
        let on_row = (1..=self.len).contains(&self.pos);
        match dir {
            FetchDirection::Forward(_) => false,
            FetchDirection::Backward(_) => true,
            FetchDirection::Current { backward } => backward || on_row,
            FetchDirection::Absolute(k) => {
                if k < 0 {
                    true
                } else if k == 0 {
                    self.pos != 0
                } else {
                    // 現在位置より先へ進む場合だけ前方向で済む（PG17 実測。末尾の後からでも
                    // k >= len + 2 は受理される）。
                    let target = usize::try_from(k).unwrap_or(usize::MAX);
                    target <= self.pos
                }
            }
            FetchDirection::Relative(k) => k < 0,
        }
    }
}

/// [`CursorPosition::plan`] の結果（状態を変えない移動の計画）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedMove {
    /// 移動後の位置（`0..=len+1`）。
    new_pos: usize,
    /// 返す行の添字範囲（0 始まり・昇順）。
    range: std::ops::Range<usize>,
    /// `true` なら降順（逆方向の取得）で返す。
    descending: bool,
}

/// `cursor` の払い出し範囲 `range`（行の添字）を複製して返す。範囲外は空。
/// [`CursorRegistry::fetch`] の切り出し処理。
fn take_rows(cursor: &OpenCursor, range: std::ops::Range<usize>) -> QueryResult {
    let rows = cursor.result.rows.get(range).unwrap_or(&[]).to_vec();
    QueryResult {
        columns: cursor.result.columns.clone(),
        rows,
    }
}

/// 1 本の開いているカーソル（確定済み [`QueryResult`] と払い出し済み位置）。
#[derive(Debug)]
struct OpenCursor {
    result: QueryResult,
    /// 0〜N+1 の位置（[`CursorPosition`]）。末尾の後（N+1）以降の前方向 `FETCH` は常に 0 行。
    position: CursorPosition,
    /// `SCROLL` 指定（逆方向の取得の可否。バイト課金には影響しない。WIRE-27 (5)）。
    scroll: CursorScroll,
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
        scroll: CursorScroll,
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
                position: CursorPosition::new(result.rows.len()),
                result,
                scroll,
                bytes,
                id,
            },
        );
        Ok(())
    }

    /// 不在（`34000`）と NO SCROLL での後退走査（`55000`）の共通前段。`fetch`／`move_cursor` が
    /// 同一の判定順を共有する（拒否時は位置を変えない）。
    fn checked_cursor_mut(
        &mut self,
        name: &str,
        direction: FetchDirection,
        check_scroll: bool,
    ) -> Result<&mut OpenCursor, SqlSurfaceError> {
        let cursor = self
            .cursors
            .get_mut(name)
            .ok_or_else(SqlSurfaceError::invalid_cursor_name)?;
        if check_scroll
            && cursor.scroll != CursorScroll::Scroll
            && cursor.position.requires_backward_scan(direction)
        {
            return Err(SqlSurfaceError::CursorCanOnlyScanForward);
        }
        Ok(cursor)
    }

    /// `MOVE`（Issue #3336・WIRE-27）。`fetch` と同じ前段検査・位置計算で位置だけを動かし、
    /// 同じ向きの `FETCH` が返す行数（`MOVE n` の n）を返す。行は複製しないため `ALL` の
    /// 行数上限（`54000`）は適用しない（結果メモリが増えない）。拒否時は位置不変。
    pub(crate) fn move_cursor(
        &mut self,
        name: &str,
        direction: FetchDirection,
    ) -> Result<u64, SqlSurfaceError> {
        // 件数 0 の MOVE は走査せず現在位置の行の有無だけを数えるため、NO SCROLL でも受理する。
        // `MOVE BACKWARD ALL` は先頭の前（位置 0）では巻き戻す行が無く、同様に受理される。
        // いずれも PG17 17.11 の実測（`FETCH` 側は同じ形でも `55000` のまま）。不在 `34000` の検査は保つ。
        let at_start = self
            .cursors
            .get(name)
            .is_some_and(|c| c.position.get() == 0);
        let check_scroll = !(matches!(direction, FetchDirection::Current { .. })
            || (at_start && direction == FetchDirection::Backward(FetchCount::All)));
        let cursor = self.checked_cursor_mut(name, direction, check_scroll)?;
        let planned = cursor.position.plan(direction);
        cursor.position.apply(&planned);
        Ok(u64::try_from(planned.range.len()).unwrap_or(u64::MAX))
    }

    /// `name` のカーソルから `direction` の向きに行を払い出し、位置を動かす
    /// （Issue #3335・WIRE-27 (1)(2)。検索本体は再実行しない）。判定順は
    /// (1) 不在は `34000`（他セッション所有・不在を区別しない固定文言。security.md
    /// 「存在情報を漏らさない」対応）→ (2) NO SCROLL で後ろへ戻る取得は `55000`（位置不変）→
    /// (3) `ALL` の行数が `max_rows` 超なら部分払い出しをせず `54000`（位置不変。行集合は
    /// `DECLARE` 時点で `max_result_rows` により cap 済みのため SQL 経路からは通常到達しない
    /// 多層防御）→ (4) 位置の更新と行の切り出し。エラー文言は固定で名前・件数を含めない。
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
        direction: FetchDirection,
        max_rows: usize,
    ) -> Result<QueryResult, SqlSurfaceError> {
        let cursor = self.checked_cursor_mut(name, direction, true)?;
        let total_rows = cursor.result.rows.len();
        let over_cap = match direction {
            FetchDirection::Forward(FetchCount::All) => {
                total_rows.saturating_sub(cursor.position.get().min(total_rows)) > max_rows
            }
            FetchDirection::Backward(FetchCount::All) => {
                cursor.position.get().saturating_sub(1).min(total_rows) > max_rows
            }
            _ => false,
        };
        if over_cap {
            return Err(SqlSurfaceError::payload_too_large(
                "result exceeds the row limit",
            ));
        }
        let planned = cursor.position.plan(direction);
        cursor.position.apply(&planned);
        let mut out = take_rows(cursor, planned.range);
        if planned.descending {
            out.rows.reverse();
        }
        Ok(out)
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

    /// 次に払い出すカーソル識別子（savepoint 作成時の高水位として記録する）。
    pub(crate) fn next_id(&self) -> u64 {
        self.next_id
    }

    /// 識別子が `watermark` 以上のカーソル（その時点より後に宣言されたもの）をすべて閉じる。
    /// `ROLLBACK TO SAVEPOINT` が、取り消した書き込みを具体化済みのカーソルを残さないために使う
    /// （`transaction.rs::restore_to_savepoint` から呼ばれる。Issue #1946・SQL-69 (5)）。
    ///
    /// 不変条件: `next_id` は戻さない（portal 束縛の `cursor_id` が再利用されず、閉じた
    /// カーソルを指す portal は照合で必ず拒否される）。残るカーソルの位置も戻さない
    /// （行集合は `DECLARE` 時点で確定済みで、巻き戻しの影響を受けない）。
    pub(crate) fn close_declared_since(&mut self, watermark: u64) {
        self.cursors.retain(|_, c| c.id < watermark);
        self.total_bytes = self.cursors.values().map(|c| c.bytes).sum();
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

    /// 旧 API 相当の薄いテスト用ヘルパー（`fetch` の向きを前方向／逆方向へ固定して呼ぶ）。
    impl CursorRegistry {
        fn fwd(&mut self, name: &str, n: usize) -> Result<QueryResult, SqlSurfaceError> {
            self.fetch(name, FetchDirection::Forward(FetchCount::Rows(n)), 100)
        }
        fn fwd_all(&mut self, name: &str, max: usize) -> Result<QueryResult, SqlSurfaceError> {
            self.fetch(name, FetchDirection::Forward(FetchCount::All), max)
        }
        fn back(
            &mut self,
            name: &str,
            count: FetchCount,
            max: usize,
        ) -> Result<QueryResult, SqlSurfaceError> {
            self.fetch(name, FetchDirection::Backward(count), max)
        }
    }

    struct FakeCatalog;
    impl TableLookup for FakeCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(name == "documents")
        }
    }

    /// カーソル課金は BIT をテキスト展開後（ビット数）で数える。最大長 BIT の 1 行は
    /// 保持上限（16 MiB）を超えるため課金額が上限超過になる（意図した拒否）。
    #[test]
    fn estimate_cell_bytes_charges_bit_by_text_length() {
        let v = crate::bit_string::BitString::parse_text(&"1".repeat(64), 64).expect("parse");
        assert_eq!(estimate_cell_bytes(&Cell::Bit(v)), 72);
        let big = vec![
            0u8;
            crate::bit_string::byte_len_for_bits(crate::bit_string::MAX_BIT_LENGTH)
                .expect("len")
        ];
        let v = crate::bit_string::BitString::from_parts(crate::bit_string::MAX_BIT_LENGTH, big)
            .expect("max bit");
        assert!(estimate_cell_bytes(&Cell::Bit(v)) > MAX_CURSOR_BYTES_PER_SESSION);
    }

    fn tokens_of(sql: &str) -> Vec<Token> {
        crate::sql::lexer::tokenize(sql).expect("tokenize")
    }

    #[test]
    fn declare_accepts_scan_and_aggregate_inner_select() {
        let stmt = validate_declare_tokens(
            &tokens_of("DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 10"),
            &FakeCatalog,
            Default::default(),
        )
        .expect("scan form accepted");
        match stmt {
            CursorStatement::Declare { name, query, .. } => {
                assert_eq!(name, "c");
                let inner = validate_declare_inner(&query, &FakeCatalog, Default::default())
                    .expect("inner revalidates");
                assert!(matches!(inner, Statement::Scan(_)));
            }
            _ => panic!("expected Declare"),
        }

        let stmt = validate_declare_tokens(
            &tokens_of("DECLARE c2 CURSOR FOR SELECT COUNT(*) AS n FROM documents"),
            &FakeCatalog,
            Default::default(),
        )
        .expect("aggregate form accepted");
        assert!(matches!(stmt, CursorStatement::Declare { .. }));
    }

    /// Issue #3175: JOIN 形の内側を受理する。
    #[test]
    fn declare_accepts_join_inner_select() {
        let stmt = validate_declare_tokens(
            &tokens_of(
                "DECLARE c CURSOR FOR SELECT a.id FROM documents a JOIN documents b ON a.id = b.id LIMIT 5",
            ),
            &FakeCatalog,
            Default::default(),
        )
        .expect("join form accepted");
        let CursorStatement::Declare { query, .. } = stmt else {
            panic!("expected Declare");
        };
        let inner = validate_declare_inner(&query, &FakeCatalog, Default::default())
            .expect("inner revalidates");
        assert!(matches!(inner, Statement::Join(_)));
    }

    fn inner_of(sql: &str) -> Result<Statement, SqlSurfaceError> {
        validate_declare_inner(&tokens_of(sql), &FakeCatalog, Default::default())
    }

    /// Issue #3176: CTE 入力の JOIN を受理し、`cte_inputs` を保持する。
    #[test]
    fn declare_accepts_cte_join_inner_select() {
        let inner = inner_of(
            "WITH w AS (SELECT id FROM documents LIMIT 5) SELECT w.id FROM w JOIN documents b ON w.id = b.id LIMIT 5",
        )
        .expect("cte join accepted");
        match inner {
            Statement::Join(v) => assert!(v.cte_inputs.is_some()),
            other => panic!("expected Join, got {other:?}"),
        }
    }

    /// Issue #3176: 集合演算（UNION／UNION ALL／INTERSECT／EXCEPT）を受理する。
    #[test]
    fn declare_accepts_set_operation_inner_select() {
        for op in [
            "UNION",
            "UNION ALL",
            "UNION DISTINCT",
            "INTERSECT",
            "INTERSECT ALL",
            "INTERSECT DISTINCT",
            "EXCEPT",
            "EXCEPT ALL",
            "EXCEPT DISTINCT",
        ] {
            let sql = format!("SELECT id FROM documents {op} SELECT id FROM documents LIMIT 5");
            let inner = inner_of(&sql).unwrap_or_else(|e| panic!("{op} rejected: {e:?}"));
            assert!(matches!(inner, Statement::SetOperation(_)), "{op}");
        }
    }

    /// Issue #3176: 受理範囲外の形は fail-closed。JOIN を枝に持つ集合演算・`WITH` ＋ 集合演算・
    /// `WITH RECURSIVE`・FROM の派生表は従来どおり拒否する（通常の SELECT でも同じ判定）。
    #[test]
    fn declare_rejects_unsupported_forms() {
        for sql in [
            "SELECT a.id FROM documents a JOIN documents b ON a.id = b.id UNION SELECT id FROM documents",
            "WITH w AS (SELECT id FROM documents LIMIT 5) SELECT id FROM w UNION SELECT id FROM documents",
            "SELECT id FROM (SELECT id FROM documents) AS d",
        ] {
            let err = inner_of(sql).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
        let err = inner_of(
            "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        )
        .expect_err("recursive rejected");
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn declare_rejects_vector_ranking_select() {
        let err = validate_declare_tokens(
            &tokens_of(
                "DECLARE c CURSOR FOR SELECT id FROM documents ORDER BY embedding <=> '[1.0]' LIMIT 10",
            ),
            &FakeCatalog,
            Default::default(),
        )
        .expect_err("vector ranking SELECT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    fn declare_scroll_of(sql: &str) -> CursorScroll {
        match validate_declare_tokens(&tokens_of(sql), &FakeCatalog, Default::default())
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
        {
            CursorStatement::Declare { scroll, .. } => scroll,
            _ => panic!("expected Declare"),
        }
    }

    #[test]
    fn declare_accepts_scroll_and_sensitivity_options() {
        const Q: &str = "FOR SELECT id FROM documents LIMIT 1";
        for (head, expected) in [
            ("DECLARE c CURSOR", CursorScroll::NoScroll),
            ("DECLARE c SCROLL CURSOR", CursorScroll::Scroll),
            ("DECLARE c NO SCROLL CURSOR", CursorScroll::NoScroll),
            ("declare c scroll cursor", CursorScroll::Scroll),
            ("DECLARE c INSENSITIVE CURSOR", CursorScroll::NoScroll),
            ("DECLARE c ASENSITIVE CURSOR", CursorScroll::NoScroll),
            ("DECLARE c INSENSITIVE SCROLL CURSOR", CursorScroll::Scroll),
            ("DECLARE c SCROLL ASENSITIVE CURSOR", CursorScroll::Scroll),
            (
                "DECLARE c NO SCROLL INSENSITIVE CURSOR",
                CursorScroll::NoScroll,
            ),
            ("DECLARE c SCROLL SCROLL CURSOR", CursorScroll::Scroll),
            (
                "DECLARE c NO SCROLL NO SCROLL CURSOR",
                CursorScroll::NoScroll,
            ),
            (
                "DECLARE c INSENSITIVE INSENSITIVE CURSOR",
                CursorScroll::NoScroll,
            ),
            ("DECLARE c CURSOR WITHOUT HOLD", CursorScroll::NoScroll),
            ("DECLARE c SCROLL CURSOR WITHOUT HOLD", CursorScroll::Scroll),
        ] {
            assert_eq!(
                declare_scroll_of(&format!("{head} {Q}")),
                expected,
                "{head}"
            );
            assert_eq!(
                declare_scroll_of(&format!("{head} {Q};")),
                expected,
                "{head};"
            );
        }
        // カーソル名が修飾語と同じ綴りでも名前の位置が先なので曖昧にならない。
        assert_eq!(
            declare_scroll_of(&format!("DECLARE scroll SCROLL CURSOR {Q}")),
            CursorScroll::Scroll
        );
    }

    // tracked: #3340
    // intentional: docs/design/sql-cursor.md
    #[test]
    fn declare_rejects_with_hold_and_binary_with_0a000() {
        for sql in [
            "DECLARE c CURSOR WITH HOLD FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c SCROLL CURSOR WITH HOLD FOR SELECT id FROM documents LIMIT 1;",
            "DECLARE c BINARY CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c BINARY SCROLL CURSOR FOR SELECT id FROM documents LIMIT 1",
        ] {
            let err = validate_declare_tokens(&tokens_of(sql), &FakeCatalog, Default::default())
                .expect_err(sql);
            assert_eq!(err.wire_code(), "0A000", "{sql}");
        }
        // 構文が壊れた文は 0A000 より先に構文エラーになる。
        let err = validate_declare_tokens(
            &tokens_of("DECLARE c CURSOR WITH HOLD FOR SELECT nope"),
            &FakeCatalog,
            Default::default(),
        )
        .expect_err("malformed inner");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn declare_rejects_malformed_or_conflicting_options_with_42601() {
        for sql in [
            "DECLARE c NO CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c NO FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c SCROLL NO SCROLL CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c NO SCROLL SCROLL CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c INSENSITIVE ASENSITIVE CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c WITH HOLD CURSOR FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c CURSOR SCROLL FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c CURSOR WITH FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c CURSOR WITHOUT FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c CURSOR WITH HOLD WITHOUT HOLD FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c CURSOR WITH HOLD WITH HOLD FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c SCROLL FOR SELECT id FROM documents LIMIT 1",
            "DECLARE c SCROLL CURSOR SELECT id FROM documents LIMIT 1",
            "DECLARE c SCROLL",
            "DECLARE c FOO CURSOR FOR SELECT id FROM documents LIMIT 1",
        ] {
            let err = validate_declare_tokens(&tokens_of(sql), &FakeCatalog, Default::default())
                .expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
    }

    fn fetch_dir(sql: &str) -> Result<FetchDirection, SqlSurfaceError> {
        match validate_fetch_tokens(&tokens_of(sql), &Default::default())? {
            CursorStatement::Fetch { name, direction } => {
                assert_eq!(name, "c", "{sql}");
                Ok(direction)
            }
            other => panic!("not a FETCH: {other:?}"),
        }
    }

    #[test]
    fn fetch_accepts_all_directions() {
        use FetchCount::{All, Rows};
        use FetchDirection::{Absolute, Backward, Current, Forward, Relative};
        for (sql, expect) in [
            ("FETCH 10 FROM c", Forward(Rows(10))),
            ("FETCH FORWARD 10 FROM c", Forward(Rows(10))),
            ("FETCH NEXT FROM c", Forward(Rows(1))),
            ("FETCH NEXT IN c", Forward(Rows(1))),
            ("fetch next in c", Forward(Rows(1))),
            ("FETCH ALL FROM c", Forward(All)),
            ("FETCH ALL IN c", Forward(All)),
            ("fetch all from c", Forward(All)),
            ("FETCH FORWARD ALL FROM c", Forward(All)),
            ("FETCH FORWARD ALL IN c", Forward(All)),
            ("FETCH 1 IN c", Forward(Rows(1))),
            ("FETCH FORWARD 3 FROM c", Forward(Rows(3))),
            ("FETCH FORWARD 3 IN c", Forward(Rows(3))),
            ("FETCH PRIOR FROM c", Backward(Rows(1))),
            ("fetch prior in c", Backward(Rows(1))),
            ("FETCH FIRST FROM c", Absolute(1)),
            ("FETCH LAST FROM c", Absolute(-1)),
            ("FETCH ABSOLUTE 3 FROM c", Absolute(3)),
            ("FETCH ABSOLUTE -2 FROM c", Absolute(-2)),
            ("FETCH ABSOLUTE 0 FROM c", Absolute(0)),
            ("FETCH ABSOLUTE +1 FROM c", Absolute(1)),
            ("FETCH RELATIVE 2 FROM c", Relative(2)),
            ("FETCH RELATIVE -1 FROM c", Relative(-1)),
            ("FETCH RELATIVE 0 FROM c", Current { backward: false }),
            ("FETCH FORWARD FROM c", Forward(Rows(1))),
            ("FETCH FORWARD 0 FROM c", Current { backward: false }),
            ("FETCH FORWARD -2 FROM c", Backward(Rows(2))),
            ("FETCH FORWARD ALL FROM c", Forward(All)),
            ("FETCH BACKWARD FROM c", Backward(Rows(1))),
            ("FETCH BACKWARD 2 FROM c", Backward(Rows(2))),
            ("FETCH BACKWARD -2 FROM c", Forward(Rows(2))),
            ("FETCH BACKWARD 0 FROM c", Current { backward: true }),
            ("FETCH BACKWARD ALL FROM c", Backward(All)),
            ("FETCH -3 FROM c", Backward(Rows(3))),
            ("FETCH +3 FROM c", Forward(Rows(3))),
            // 件数 0 は現在の行の再取得（WIRE-27 (1)。従来の 22000 から受理へ変更）。
            ("FETCH 0 FROM c", Current { backward: false }),
            ("FETCH FROM c", Forward(Rows(1))),
            ("FETCH IN c", Forward(Rows(1))),
            ("FETCH LAST FROM c;", Absolute(-1)),
            ("FETCH BACKWARD ALL IN c;", Backward(All)),
        ] {
            assert_eq!(fetch_dir(sql).expect(sql), expect, "{sql}");
        }
    }

    #[test]
    fn fetch_rejects_malformed_forms_with_42601() {
        for sql in [
            "FETCH ALL c",
            "FETCH ALL 3 FROM c",
            "FETCH ALL ALL FROM c",
            "FETCH FORWARD ALL ALL FROM c",
            "FETCH NEXT ALL FROM c",
            "FETCH ALL FROM c extra",
            "FETCH ALL FROM ALL",
            "FETCH PRIOR 1 FROM c",
            "FETCH FIRST 1 FROM c",
            "FETCH LAST 1 FROM c",
            "FETCH ABSOLUTE FROM c",
            "FETCH ABSOLUTE ALL FROM c",
            "FETCH RELATIVE FROM c",
            "FETCH - FROM c",
            "FETCH - - 1 FROM c",
            "FETCH + - 1 FROM c",
            "FETCH ABSOLUTE 1.5 FROM c",
            "FETCH ABSOLUTE 4294967296 FROM c",
            "FETCH RELATIVE -4294967296 FROM c",
            "FETCH 4294967296 FROM c",
            "FETCH BACKWARD BACKWARD 1 FROM c",
            "FETCH FORWARD NEXT FROM c",
            "FETCH FORWARD FORWARD 1 FROM c",
            "FETCH NEXT 3 FROM c",
            "FETCH c",
            "FETCH PRIOR c",
            "FETCH NEXT c",
            "FETCH 1 c",
            "FETCH 1 IN FROM c",
            "FETCH 1 FROM IN c",
            "FETCH 1 FROM c extra",
            "FETCH FROM",
            "FETCH",
        ] {
            let err = fetch_dir(sql).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
        for sql in ["FETCH 1 FROM c;;", "FETCH LAST FROM c;;"] {
            let err = fetch_dir(sql).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
    }

    #[test]
    fn fetch_row_count_limit_applies_to_absolute_value() {
        // 行数として扱う件数は絶対値が上限（既定 10000）を超えると 22000（従来どおり）。
        for sql in [
            "FETCH 10001 FROM c",
            "FETCH -10001 FROM c",
            "FETCH FORWARD 10001 FROM c",
            "FETCH FORWARD -10001 FROM c",
            "FETCH BACKWARD 10001 FROM c",
            "FETCH BACKWARD -10001 FROM c",
            "FETCH 4294967295 FROM c",
        ] {
            let err = fetch_dir(sql).expect_err(sql);
            assert_eq!(err.wire_code(), "22000", "{sql}");
        }
        // ABSOLUTE／RELATIVE は位置であり行数上限の対象外（u32 に収まれば受理）。
        assert_eq!(
            fetch_dir("FETCH ABSOLUTE 4294967295 FROM c").expect("abs"),
            FetchDirection::Absolute(4_294_967_295)
        );
        assert_eq!(
            fetch_dir("FETCH RELATIVE -4294967295 FROM c").expect("rel"),
            FetchDirection::Relative(-4_294_967_295)
        );
    }

    #[test]
    fn quoted_value_function_names_are_accepted_as_cursor_names() {
        // 引用付きの値関数予約語は demote_quoted_null 後も QuotedNull で残るため、
        // カーソル名位置でも受理する（識別子としての従来挙動の維持）。
        let toks = |sql: &str| {
            crate::sql::lexer::demote_quoted_null(
                crate::sql::lexer::tokenize_marked(sql).expect("tokenize"),
            )
        };
        let stmt = validate_close_tokens(&toks("CLOSE \"current_date\"")).expect("close");
        assert_eq!(
            stmt,
            CursorStatement::Close {
                name: "current_date".to_string()
            }
        );
        assert!(
            validate_fetch_tokens(&toks("FETCH 1 FROM \"current_date\""), &Default::default())
                .is_ok()
        );
        let declare = validate_declare_tokens(
            &toks("DECLARE \"current_date\" CURSOR FOR SELECT COUNT(*) FROM documents"),
            &FakeCatalog,
            Default::default(),
        );
        assert!(declare.is_ok(), "{declare:?}");
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
                .declare(format!("c{i}"), empty.clone(), CursorScroll::NoScroll)
                .expect("within limit");
        }
        let err = registry
            .declare(
                "overflow".to_string(),
                empty.clone(),
                CursorScroll::NoScroll,
            )
            .expect_err("count limit exceeded");
        assert_eq!(err.wire_code(), "54000");

        registry.close("c0").expect("close existing");
        registry
            .declare("c0".to_string(), empty, CursorScroll::NoScroll)
            .expect("slot freed after close");
    }

    /// `close_declared_since` は水位以上のカーソルだけを閉じ、残りの位置・バイト合計を保ち、
    /// `next_id` を戻さない（Issue #1946）。
    #[test]
    fn close_declared_since_closes_only_newer_cursors() {
        let row = |id: u64| crate::sql::exec::ResultRow {
            id,
            score: 0.0,
            cells: vec![],
        };
        let result = |ids: &[u64]| QueryResult {
            columns: vec![],
            rows: ids.iter().map(|i| row(*i)).collect(),
        };
        let mut registry = CursorRegistry::new();
        registry
            .declare(
                "old".to_string(),
                result(&[1, 2, 3]),
                CursorScroll::NoScroll,
            )
            .unwrap();
        registry.fwd("old", 1).unwrap();
        let old_bytes = registry.total_bytes;
        let watermark = registry.next_id();
        registry
            .declare("new1".to_string(), result(&[4, 5]), CursorScroll::NoScroll)
            .unwrap();
        registry
            .declare("new2".to_string(), result(&[6]), CursorScroll::NoScroll)
            .unwrap();
        let next_before = registry.next_id();

        registry.close_declared_since(watermark);

        assert_eq!(registry.fwd("new1", 1).unwrap_err().wire_code(), "34000");
        assert_eq!(registry.close("new2").unwrap_err().wire_code(), "34000");
        assert_eq!(registry.total_bytes, old_bytes);
        assert_eq!(registry.next_id(), next_before);
        // 位置は保たれ、続きの 2 件が返る。
        let rest = registry.fwd("old", 10).unwrap();
        assert_eq!(
            rest.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn cursor_registry_rejects_duplicate_name() {
        let mut registry = CursorRegistry::new();
        let empty = QueryResult {
            columns: vec![],
            rows: vec![],
        };
        registry
            .declare("c".to_string(), empty.clone(), CursorScroll::NoScroll)
            .expect("first declare");
        let err = registry
            .declare("c".to_string(), empty, CursorScroll::NoScroll)
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
            .declare("c".to_string(), empty.clone(), CursorScroll::NoScroll)
            .expect("first declare");
        let first_id = registry.cursor_id("c").expect("cursor exists");

        registry.close("c").expect("close succeeds");
        assert_eq!(
            registry.cursor_id("c"),
            None,
            "closed cursor must not resolve an id"
        );

        registry
            .declare("c".to_string(), empty, CursorScroll::NoScroll)
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
                CursorScroll::NoScroll,
            )
            .expect("declare");

        let page1 = registry.fwd("c", 2).expect("fetch page 1");
        assert_eq!(
            page1.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let page2 = registry.fwd("c", 2).expect("fetch page 2");
        assert_eq!(
            page2.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        let page3 = registry.fwd("c", 2).expect("fetch page 3");
        assert_eq!(page3.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![4]);
        let page4 = registry.fwd("c", 2).expect("fetch page 4 (empty)");
        assert!(page4.rows.is_empty());
    }

    fn registry_with_rows(n: i64) -> CursorRegistry {
        let mut registry = CursorRegistry::new();
        let rows: Vec<crate::sql::exec::ResultRow> = (0..n)
            .map(|i| crate::sql::exec::ResultRow {
                id: i as _,
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
                CursorScroll::NoScroll,
            )
            .expect("declare");
        registry
    }

    /// Issue #3174: FETCH ALL は残り全行を返して末尾へ進み、以降は 0 行。
    #[test]
    fn fetch_all_returns_remaining_rows_and_moves_to_end() {
        let mut registry = registry_with_rows(5);
        registry.fwd("c", 2).expect("fetch 2");
        let rest = registry.fwd_all("c", 10).expect("fetch all");
        assert_eq!(
            rest.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(registry.fwd_all("c", 10).expect("again").rows.is_empty());
        assert!(registry.fwd("c", 1).expect("next").rows.is_empty());
    }

    /// Issue #3174: cap 超過は 54000 で部分払い出しも位置変更もしない。
    #[test]
    fn fetch_all_over_row_cap_is_54000_without_advancing() {
        let mut registry = registry_with_rows(5);
        let err = registry.fwd_all("c", 4).expect_err("over cap");
        assert_eq!(err.wire_code(), "54000");
        let page = registry.fwd("c", 2).expect("fetch 2");
        assert_eq!(
            page.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let rest = registry.fwd_all("c", 3).expect("exactly cap");
        assert_eq!(rest.rows.len(), 3);
    }

    #[test]
    fn fetch_all_rejects_unknown_name_with_34000() {
        let mut registry = CursorRegistry::new();
        let err = registry.fwd_all("missing", 10).expect_err("not found");
        assert_eq!(err.wire_code(), "34000");
    }

    #[test]
    fn fetch_and_close_reject_unknown_name_with_34000() {
        let mut registry = CursorRegistry::new();
        let err = registry.fwd("missing", 1).expect_err("not found");
        assert_eq!(err.wire_code(), "34000");
        let err = registry.close("missing").expect_err("not found");
        assert_eq!(err.wire_code(), "34000");
    }

    /// Issue #3171: 末尾の単一 `;` を受理し、`;;` は 42601 のまま。
    #[test]
    fn trailing_semicolon_is_accepted_for_cursor_statements() {
        assert_eq!(
            validate_fetch_tokens(&tokens_of("FETCH 1 FROM c;"), &Default::default())
                .expect("fetch"),
            CursorStatement::Fetch {
                name: "c".to_string(),
                direction: FetchDirection::Forward(FetchCount::Rows(1))
            }
        );
        assert!(
            validate_fetch_tokens(&tokens_of("FETCH FORWARD 1 FROM c;"), &Default::default())
                .is_ok()
        );
        for sql in [
            "FETCH NEXT FROM c;",
            "FETCH 1 IN c;",
            "FETCH ALL FROM c;",
            "FETCH FORWARD ALL IN c;",
        ] {
            assert!(
                validate_fetch_tokens(&tokens_of(sql), &Default::default()).is_ok(),
                "{sql}"
            );
        }
        assert_eq!(
            validate_close_tokens(&tokens_of("CLOSE c;")).expect("close"),
            CursorStatement::Close {
                name: "c".to_string()
            }
        );
        let declare = validate_declare_tokens(
            &tokens_of("DECLARE c CURSOR FOR SELECT COUNT(*) FROM documents;"),
            &FakeCatalog,
            Default::default(),
        );
        assert!(declare.is_ok(), "{declare:?}");
        for sql in ["CLOSE c;;", "CLOSE ALL;"] {
            let err = validate_close_tokens(&tokens_of(sql)).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
        for sql in ["FETCH 1 FROM c;;", "FETCH ALL FROM c;;"] {
            let err = validate_fetch_tokens(&tokens_of(sql), &Default::default()).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
    }

    fn rows_of(r: &QueryResult) -> Vec<u64> {
        r.rows.iter().map(|x| x.id).collect()
    }

    /// 位置 1 始まりの素朴な参照モデル（PG の portal 位置の式。WIRE-27 (5)(6)）。
    #[derive(Clone, Copy)]
    enum Op {
        Fwd(usize),
        FwdAll,
        Back(usize),
    }

    fn reference_step(pos: usize, len: usize, op: Op) -> (usize, Vec<usize>) {
        // 戻り値は新しい位置と返す行番号（1 始まり・払い出し順）。
        match op {
            Op::Fwd(n) => {
                let rows: Vec<usize> = (pos + 1..=(pos + n).min(len)).collect();
                (if pos + n <= len { pos + n } else { len + 1 }, rows)
            }
            Op::FwdAll => ((len + 1), (pos + 1..=len).collect()),
            Op::Back(n) => {
                let new = pos.saturating_sub(n);
                let mut rows = Vec::new();
                let mut r = pos.saturating_sub(1);
                while r >= new.max(1) && r >= 1 {
                    rows.push(r);
                    r -= 1;
                }
                (new, rows)
            }
        }
    }

    #[test]
    fn cursor_position_stays_within_bounds_and_matches_reference_model() {
        let ops: Vec<Op> = (0..=7)
            .map(Op::Fwd)
            .chain(std::iter::once(Op::FwdAll))
            .chain((0..=7).map(Op::Back))
            .collect();
        for len in 0..=5usize {
            // 長さ 3 の全操作列を列挙する。
            for a in &ops {
                for b in &ops {
                    for c in &ops {
                        let mut p = CursorPosition::new(len);
                        let mut model = 0usize;
                        for op in [*a, *b, *c] {
                            let (new_model, expect_rows) = reference_step(model, len, op);
                            let range = match op {
                                Op::Fwd(n) => p.forward(n),
                                Op::FwdAll => p.forward_all(),
                                Op::Back(n) => p.backward(n),
                            };
                            assert!(p.get() <= len + 1, "pos bound");
                            assert!(range.start <= range.end && range.end <= len, "range");
                            let mut got: Vec<usize> = range.map(|i| i + 1).collect();
                            if matches!(op, Op::Back(_)) {
                                got.reverse();
                            }
                            assert_eq!(got, expect_rows, "len={len}");
                            assert_eq!(p.get(), new_model, "len={len}");
                            model = new_model;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cursor_position_boundary_examples() {
        // N 行ちょうど取得した後は位置 N、続く FETCH は 0 行で位置 N+1。
        let mut p = CursorPosition::new(3);
        assert_eq!(p.forward(3), 0..3);
        assert_eq!(p.get(), 3);
        assert_eq!(p.forward(1), 3..3);
        assert_eq!(p.get(), 4);
        // N+1 からの backward(1) は N 行目（添字 2）。
        assert_eq!(p.backward(1), 2..3);
        assert_eq!(p.get(), 3);
        // N=0 の forward は位置 1（= N+1）で 0 行。
        let mut e = CursorPosition::new(0);
        assert_eq!(e.forward(1), 0..0);
        assert_eq!(e.get(), 1);
        assert_eq!(e.backward(5), 0..0);
        assert_eq!(e.get(), 0);
    }

    fn registry_with_scroll(n: i64, scroll: CursorScroll) -> CursorRegistry {
        let mut registry = CursorRegistry::new();
        let rows: Vec<crate::sql::exec::ResultRow> = (0..n)
            .map(|i| crate::sql::exec::ResultRow {
                id: i as _,
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
                scroll,
            )
            .expect("declare");
        registry
    }

    #[test]
    fn fetch_backward_on_no_scroll_is_55000_and_keeps_position() {
        for scroll in [CursorScroll::NoScroll, CursorScroll::default()] {
            let mut registry = registry_with_scroll(5, scroll);
            registry.fwd("c", 2).expect("fetch");
            for count in [FetchCount::Rows(1), FetchCount::All] {
                let err = registry.back("c", count, 100).expect_err("no scroll");
                assert_eq!(err.wire_code(), "55000");
                assert_eq!(err.to_string(), "cursor can only scan forward");
            }
            assert_eq!(rows_of(&registry.fwd("c", 2).expect("next")), vec![2, 3]);
        }
    }

    #[test]
    fn fetch_backward_on_scroll_returns_rows_in_reverse() {
        let mut registry = registry_with_scroll(5, CursorScroll::Scroll);
        registry.fwd("c", 4).expect("fetch");
        let back = registry
            .back("c", FetchCount::Rows(2), 100)
            .expect("backward");
        // 4 行取得後は 4 行目の上（位置 4）。戻ると 3 行目・2 行目（id 2・1）。
        assert_eq!(rows_of(&back), vec![2, 1]);
        let all = registry
            .back("c", FetchCount::All, 100)
            .expect("backward all");
        assert_eq!(rows_of(&all), vec![0]);
        assert!(registry
            .back("c", FetchCount::Rows(1), 100)
            .expect("at start")
            .rows
            .is_empty());
        assert_eq!(rows_of(&registry.fwd("c", 1).expect("fwd")), vec![0]);
    }

    #[test]
    fn fetch_backward_unknown_name_is_34000_before_55000() {
        let mut registry = registry_with_scroll(1, CursorScroll::NoScroll);
        let err = registry
            .back("nope", FetchCount::Rows(1), 10)
            .expect_err("missing");
        assert_eq!(err.wire_code(), "34000");
    }

    #[test]
    fn fetch_backward_all_over_row_cap_is_54000_without_moving() {
        let mut registry = registry_with_scroll(5, CursorScroll::Scroll);
        registry.fwd_all("c", 10).expect("to end");
        let err = registry
            .back("c", FetchCount::All, 2)
            .expect_err("over cap");
        assert_eq!(err.wire_code(), "54000");
        let back = registry
            .back("c", FetchCount::Rows(1), 2)
            .expect("still at end");
        assert_eq!(rows_of(&back), vec![4]);
    }

    #[test]
    fn scroll_and_no_scroll_charge_the_same_bytes() {
        let a = registry_with_scroll(4, CursorScroll::Scroll);
        let b = registry_with_scroll(4, CursorScroll::NoScroll);
        assert_eq!(a.total_bytes, b.total_bytes);
        assert_eq!(a.remaining_bytes(), b.remaining_bytes());
    }

    /// `Absolute`／`Relative`／`Current` の参照モデル（1 始まり。PG の定義どおり
    /// ABSOLUTE は先頭へ戻してから進む／末尾から戻る、RELATIVE は |k| 歩進めてから 1 行）。
    fn reference_move(pos: usize, len: usize, dir: FetchDirection) -> (usize, Vec<usize>) {
        let row = |t: usize| {
            if (1..=len).contains(&t) {
                vec![t]
            } else {
                vec![]
            }
        };
        let step_fwd = |mut p: usize, k: usize| {
            for _ in 0..k {
                if p <= len {
                    p += 1;
                }
            }
            p
        };
        let step_back = |mut p: usize, k: usize| {
            for _ in 0..k {
                p = p.saturating_sub(1);
            }
            p
        };
        match dir {
            FetchDirection::Absolute(k) if k > 0 => {
                let t = step_fwd(0, k as usize);
                (t, row(t))
            }
            FetchDirection::Absolute(k) if k < 0 => {
                let t = step_back(len + 1, k.unsigned_abs() as usize);
                (t, row(t))
            }
            FetchDirection::Absolute(_) => (0, vec![]),
            FetchDirection::Relative(k) if k > 0 => {
                let t = step_fwd(pos, k as usize);
                (t, row(t))
            }
            FetchDirection::Relative(k) => {
                let t = step_back(pos, k.unsigned_abs() as usize);
                (t, row(t))
            }
            FetchDirection::Current { .. } => (pos, row(pos)),
            _ => unreachable!("covered by reference_step"),
        }
    }

    #[test]
    fn plan_absolute_relative_current_match_reference_model() {
        let mut dirs = vec![FetchDirection::Current { backward: false }];
        for k in -7i64..=7 {
            dirs.push(FetchDirection::Absolute(k));
            if k != 0 {
                dirs.push(FetchDirection::Relative(k));
            }
        }
        for len in 0..=5usize {
            for start in 0..=len + 1 {
                for a in &dirs {
                    for b in &dirs {
                        let mut p = CursorPosition::new(len);
                        p.pos = start;
                        let mut model = start;
                        for dir in [*a, *b] {
                            let (new_model, expect_rows) = reference_move(model, len, dir);
                            let planned = p.plan(dir);
                            // plan は状態を変えない。
                            assert_eq!(p.get(), model, "plan must be pure");
                            p.apply(&planned);
                            assert!(p.get() <= len + 1, "pos bound");
                            assert!(!planned.descending);
                            let got: Vec<usize> = planned.range.map(|i| i + 1).collect();
                            assert_eq!(got, expect_rows, "len={len} start={start} {dir:?}");
                            assert_eq!(p.get(), new_model, "len={len} start={start} {dir:?}");
                            model = new_model;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn plan_handles_huge_absolute_and_relative_without_overflow() {
        let mut p = CursorPosition::new(3);
        for k in [i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1] {
            for dir in [FetchDirection::Absolute(k), FetchDirection::Relative(k)] {
                let planned = p.plan(dir);
                p.apply(&planned);
                assert!(p.get() <= 4, "{dir:?}");
                assert!(planned.range.len() <= 1, "{dir:?}");
            }
        }
    }

    /// 位置 `p`（0..=N+1）での NO SCROLL 判定の期待値（PG17 の portal 実行からの導出を表にしたもの。
    /// PostgreSQL 17.11 の実測と一致を確認済み。Issue #3337）。
    fn expect_requires_backward(p: usize, n: usize, dir: FetchDirection) -> bool {
        let on_row = (1..=n).contains(&p);
        match dir {
            FetchDirection::Forward(_) => false,
            FetchDirection::Backward(_) => true,
            FetchDirection::Current { backward: true } => true,
            FetchDirection::Current { backward: false } => on_row,
            FetchDirection::Relative(k) => k < 0,
            FetchDirection::Absolute(k) if k < 0 => true,
            FetchDirection::Absolute(0) => p != 0,
            FetchDirection::Absolute(k) => (k as usize) <= p,
        }
    }

    #[test]
    fn requires_backward_scan_matches_table_for_every_position() {
        let mut dirs = vec![
            FetchDirection::Forward(FetchCount::Rows(1)),
            FetchDirection::Forward(FetchCount::All),
            FetchDirection::Backward(FetchCount::Rows(1)),
            FetchDirection::Backward(FetchCount::All),
            FetchDirection::Current { backward: false },
            FetchDirection::Current { backward: true },
        ];
        for k in -7i64..=7 {
            dirs.push(FetchDirection::Absolute(k));
            if k != 0 {
                dirs.push(FetchDirection::Relative(k));
            }
        }
        for n in 0..=5usize {
            for p in 0..=n + 1 {
                for dir in &dirs {
                    let mut pos = CursorPosition::new(n);
                    pos.pos = p;
                    assert_eq!(
                        pos.requires_backward_scan(*dir),
                        expect_requires_backward(p, n, *dir),
                        "n={n} p={p} {dir:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_scroll_rejection_keeps_position_for_every_direction() {
        let dirs = [
            FetchDirection::Backward(FetchCount::Rows(1)),
            FetchDirection::Backward(FetchCount::All),
            FetchDirection::Current { backward: false },
            FetchDirection::Current { backward: true },
            FetchDirection::Absolute(-1),
            FetchDirection::Absolute(2),
            FetchDirection::Absolute(0),
            FetchDirection::Relative(-1),
        ];
        for dir in dirs {
            let mut registry = registry_with_scroll(5, CursorScroll::NoScroll);
            registry.fwd("c", 2).expect("move to 2");
            let err = registry.fetch("c", dir, 100).expect_err("rejected");
            assert_eq!(err.wire_code(), "55000", "{dir:?}");
            assert_eq!(
                rows_of(&registry.fwd("c", 1).expect("next")),
                vec![2],
                "{dir:?}"
            );
        }
    }

    #[test]
    fn no_scroll_accepts_forward_moves_and_scroll_accepts_all() {
        let mut registry = registry_with_scroll(5, CursorScroll::NoScroll);
        // 先頭の前: FIRST・ABSOLUTE 0・FETCH 0 はいずれも前方向で済む。
        assert!(registry
            .fetch("c", FetchDirection::Current { backward: false }, 100)
            .expect("cur")
            .rows
            .is_empty());
        assert!(registry
            .fetch("c", FetchDirection::Absolute(0), 100)
            .expect("abs0")
            .rows
            .is_empty());
        assert_eq!(
            rows_of(
                &registry
                    .fetch("c", FetchDirection::Absolute(1), 100)
                    .expect("first")
            ),
            vec![0]
        );
        assert_eq!(
            rows_of(
                &registry
                    .fetch("c", FetchDirection::Relative(2), 100)
                    .expect("rel")
            ),
            vec![2]
        );
        assert_eq!(
            rows_of(
                &registry
                    .fetch("c", FetchDirection::Absolute(5), 100)
                    .expect("abs")
            ),
            vec![4]
        );
        let err = registry
            .fetch("c", FetchDirection::Absolute(5), 100)
            .expect_err("same pos");
        assert_eq!(err.wire_code(), "55000");

        let mut scroll = registry_with_scroll(5, CursorScroll::Scroll);
        assert_eq!(
            rows_of(
                &scroll
                    .fetch("c", FetchDirection::Absolute(-1), 100)
                    .expect("last")
            ),
            vec![4]
        );
        assert_eq!(
            rows_of(
                &scroll
                    .fetch("c", FetchDirection::Absolute(1), 100)
                    .expect("first")
            ),
            vec![0]
        );
        assert_eq!(
            rows_of(
                &scroll
                    .fetch("c", FetchDirection::Current { backward: true }, 100)
                    .expect("cur")
            ),
            vec![0]
        );
    }

    #[test]
    fn scroll_positions_beyond_the_ends_behave_like_pg() {
        let mut registry = registry_with_scroll(3, CursorScroll::Scroll);
        // ABSOLUTE N+5 は末尾の後（0 行）。続く PRIOR は N 行目。
        assert!(registry
            .fetch("c", FetchDirection::Absolute(8), 100)
            .expect("beyond")
            .rows
            .is_empty());
        let prior = FetchDirection::Backward(FetchCount::Rows(1));
        assert_eq!(
            rows_of(&registry.fetch("c", prior, 100).expect("prior")),
            vec![2]
        );
        // ABSOLUTE -(N+1) は先頭の前（0 行）。続く NEXT は 1 行目。
        assert!(registry
            .fetch("c", FetchDirection::Absolute(-4), 100)
            .expect("before")
            .rows
            .is_empty());
        assert_eq!(rows_of(&registry.fwd("c", 1).expect("next")), vec![0]);
        // RELATIVE は途中の行を返さない。
        assert_eq!(
            rows_of(
                &registry
                    .fetch("c", FetchDirection::Relative(2), 100)
                    .expect("rel")
            ),
            vec![2]
        );
        // 末尾の後からの BACKWARD ALL は N 行すべて（降順）。
        registry
            .fetch("c", FetchDirection::Absolute(9), 100)
            .expect("end");
        let all = registry
            .fetch("c", FetchDirection::Backward(FetchCount::All), 100)
            .expect("all");
        assert_eq!(rows_of(&all), vec![2, 1, 0]);
    }

    #[test]
    fn fetch_unknown_name_is_34000_for_every_direction() {
        let mut registry = registry_with_scroll(1, CursorScroll::NoScroll);
        for dir in [
            FetchDirection::Backward(FetchCount::All),
            FetchDirection::Absolute(-1),
            FetchDirection::Relative(1),
            FetchDirection::Current { backward: false },
        ] {
            let err = registry.fetch("nope", dir, 10).expect_err("missing");
            assert_eq!(err.wire_code(), "34000", "{dir:?}");
        }
    }

    /// Issue #3336: `MOVE` の構文は `FETCH` と同じ向き集合を受理し、同じ [`FetchDirection`] になる。
    #[test]
    fn move_accepts_same_directions_as_fetch() {
        for dir in [
            "",
            "NEXT ",
            "PRIOR ",
            "FIRST ",
            "LAST ",
            "ABSOLUTE 3 ",
            "ABSOLUTE -1 ",
            "RELATIVE -2 ",
            "RELATIVE 0 ",
            "2 ",
            "-2 ",
            "0 ",
            "ALL ",
            "FORWARD ",
            "FORWARD 2 ",
            "FORWARD ALL ",
            "BACKWARD ",
            "BACKWARD 2 ",
            "BACKWARD ALL ",
        ] {
            for kw in ["FROM", "IN"] {
                let fetch = validate_fetch_tokens(
                    &tokens_of(&format!("FETCH {dir}{kw} c")),
                    &Default::default(),
                )
                .expect("fetch");
                let mv = validate_move_tokens(
                    &tokens_of(&format!("MOVE {dir}{kw} c;")),
                    &Default::default(),
                )
                .expect("move");
                let (
                    CursorStatement::Fetch { direction: f, .. },
                    CursorStatement::Move { name, direction: m },
                ) = (fetch, mv)
                else {
                    panic!("unexpected statement kind");
                };
                assert_eq!(f, m, "{dir}{kw}");
                assert_eq!(name, "c");
            }
        }
    }

    /// Issue #3336: 拒否集合も `FETCH` と同じ（`42601`）で、文言だけ `MOVE` になる。
    #[test]
    fn move_rejects_malformed_forms_with_42601() {
        for sql in [
            "MOVE",
            "MOVE c",
            "MOVE NEXT c",
            "MOVE FROM",
            "MOVE FROM c extra",
            "MOVE PRIOR 1 IN c",
            "MOVE FORWARD --1 IN c",
            "MOVE 4294967296 IN c",
            "MOVE ABSOLUTE 4294967296 IN c",
            "MOVE FROM ALL",
            "MOVE FROM c; MOVE FROM c",
        ] {
            let err = validate_move_tokens(&tokens_of(sql), &Default::default()).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
            assert!(!err.to_string().contains("FETCH"), "{sql}: {err}");
        }
    }

    #[test]
    fn move_row_count_limit_matches_fetch_22000() {
        let limits = crate::result_limits::ResultLimits::default();
        let over = limits.max_result_rows().saturating_add(1);
        let f = validate_fetch_tokens(&tokens_of(&format!("FETCH {over} FROM c")), &limits)
            .expect_err("fetch");
        let m = validate_move_tokens(&tokens_of(&format!("MOVE {over} FROM c")), &limits)
            .expect_err("move");
        assert_eq!(f.wire_code(), m.wire_code());
    }

    fn mv(registry: &mut CursorRegistry, dir: FetchDirection) -> u64 {
        registry.move_cursor("c", dir).expect("move")
    }

    /// Issue #3336: タグ件数は同じ向きの FETCH が返す行数で、後続 FETCH が移動先から始まる。
    #[test]
    fn move_counts_and_following_fetch_start_after_target() {
        let rows = |r: Result<QueryResult, SqlSurfaceError>| rows_of(&r.expect("fetch"));
        let mut r = registry_with_scroll(5, CursorScroll::Scroll);
        assert_eq!(mv(&mut r, FetchDirection::Forward(FetchCount::Rows(2))), 2);
        assert_eq!(
            rows(r.fetch("c", FetchDirection::Forward(FetchCount::Rows(1)), 100)),
            vec![2]
        );
        assert_eq!(mv(&mut r, FetchDirection::Backward(FetchCount::Rows(1))), 1);
        assert_eq!(mv(&mut r, FetchDirection::Absolute(0)), 0);
        assert_eq!(
            rows(r.fetch("c", FetchDirection::Forward(FetchCount::Rows(1)), 100)),
            vec![0]
        );
        let mut r = registry_with_scroll(5, CursorScroll::Scroll);
        assert_eq!(mv(&mut r, FetchDirection::Forward(FetchCount::Rows(10))), 5);
        assert_eq!(mv(&mut r, FetchDirection::Backward(FetchCount::All)), 5);
        assert_eq!(mv(&mut r, FetchDirection::Absolute(3)), 1);
        assert_eq!(mv(&mut r, FetchDirection::Absolute(10)), 0);
        assert_eq!(mv(&mut r, FetchDirection::Absolute(0)), 0);
        assert_eq!(mv(&mut r, FetchDirection::Absolute(1)), 1);
        assert_eq!(mv(&mut r, FetchDirection::Relative(0)), 1);
    }

    /// Issue #3336: `MOVE ALL` は行を返さないので `max_rows` 超でも `54000` にならない。
    #[test]
    fn move_all_is_not_limited_by_row_cap() {
        let mut r = registry_with_rows(5);
        assert_eq!(mv(&mut r, FetchDirection::Forward(FetchCount::All)), 5);
        assert!(r.fwd("c", 1).expect("next").rows.is_empty());
    }

    /// Issue #3336: 不在は `34000`、NO SCROLL の逆方向は `55000`。いずれも位置不変。
    #[test]
    fn move_rejections_keep_position() {
        let mut r = registry_with_scroll(5, CursorScroll::NoScroll);
        mv(&mut r, FetchDirection::Forward(FetchCount::Rows(2)));
        let err = r
            .move_cursor("c", FetchDirection::Backward(FetchCount::Rows(1)))
            .expect_err("no scroll");
        assert_eq!(err.wire_code(), "55000");
        let err = r
            .move_cursor("nope", FetchDirection::Backward(FetchCount::All))
            .expect_err("missing");
        assert_eq!(err.wire_code(), "34000");
        assert_eq!(rows_of(&r.fwd("c", 1).expect("next")), vec![2]);
    }

    /// Issue #3337: 件数 0 の MOVE は NO SCROLL でも受理され、件数は行の上なら 1・それ以外は 0、
    /// 位置は不変（PG17 17.11 実測）。不在は `34000` のまま。
    #[test]
    fn no_scroll_move_count_zero_is_accepted_and_keeps_position() {
        let zero_dirs = [
            FetchDirection::Current { backward: false },
            FetchDirection::Current { backward: true },
        ];
        for dir in zero_dirs {
            let mut r = registry_with_scroll(5, CursorScroll::NoScroll);
            assert_eq!(mv(&mut r, dir), 0, "before first {dir:?}");
            mv(&mut r, FetchDirection::Forward(FetchCount::Rows(3)));
            assert_eq!(mv(&mut r, dir), 1, "on row {dir:?}");
            assert_eq!(rows_of(&r.fwd("c", 1).expect("next")), vec![3], "{dir:?}");
            mv(&mut r, FetchDirection::Forward(FetchCount::All));
            assert_eq!(mv(&mut r, dir), 0, "after last {dir:?}");
        }
        let mut r = registry_with_scroll(5, CursorScroll::NoScroll);
        // 先頭の前の `MOVE BACKWARD ALL` は受理（0 件）、行の上では `55000`。
        assert_eq!(mv(&mut r, FetchDirection::Backward(FetchCount::All)), 0);
        mv(&mut r, FetchDirection::Forward(FetchCount::Rows(1)));
        assert_eq!(
            r.move_cursor("c", FetchDirection::Backward(FetchCount::All))
                .expect_err("on row")
                .wire_code(),
            "55000"
        );
        let mut r = registry_with_scroll(5, CursorScroll::NoScroll);
        let err = r
            .move_cursor("nope", FetchDirection::Current { backward: true })
            .expect_err("missing");
        assert_eq!(err.wire_code(), "34000");
        // FETCH 側は行の上で従来どおり 55000。
        mv(&mut r, FetchDirection::Forward(FetchCount::Rows(2)));
        let err = r
            .fetch("c", FetchDirection::Current { backward: true }, 100)
            .expect_err("fetch 0");
        assert_eq!(err.wire_code(), "55000");
    }

    /// Issue #3337: NO SCROLL で末尾の後からの `ABSOLUTE k` は k <= N+1 が `55000`、k >= N+2 が受理。
    #[test]
    fn no_scroll_absolute_after_last_matches_pg17() {
        for k in [1i64, 3, 5, 6] {
            let mut r = registry_with_scroll(5, CursorScroll::NoScroll);
            mv(&mut r, FetchDirection::Forward(FetchCount::All));
            let err = r
                .fetch("c", FetchDirection::Absolute(k), 100)
                .expect_err("rejected");
            assert_eq!(err.wire_code(), "55000", "k={k}");
        }
        for k in [7i64, 100] {
            let mut r = registry_with_scroll(5, CursorScroll::NoScroll);
            mv(&mut r, FetchDirection::Forward(FetchCount::All));
            assert!(r
                .fetch("c", FetchDirection::Absolute(k), 100)
                .expect("accepted")
                .rows
                .is_empty());
            assert_eq!(mv(&mut r, FetchDirection::Absolute(k)), 0);
        }
    }
}
