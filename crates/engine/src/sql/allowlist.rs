//! AST 許可リスト検証（TASK-74・SQL-8・ERR-2 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md・docs/spec/04-behavior/error-format.md）。
//!
//! 責務境界: [`lexer`](crate::sql::lexer) が返すトークン列を、明示的に許可した
//! 形状だけに一致するか再帰下降で判定する。**許可リスト**として実装するため、
//! 期待しない字句・構文は個別に検出せず、期待する位置に来ないというだけで
//! 構造的に拒否する（fail-closed。未知・未対応構文は既定で拒否側に落ちる）。
//!
//! 受理側（実在テーブルに対する検索・取得の実行）は本モジュールの管轄外で、
//! 後続タスクが [`ValidatedStatement`] を土台に実装する。本モジュールは
//! 「許可形状の構造判定を通過させる」ところまでに責務を留める。

use crate::catalog::{ColumnDef, ColumnDefault, ColumnType, MAX_COLUMN_DEFAULT_LEN};
use crate::error_format::{ClassifiedError, ErrorClass};
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::lexer::{self, Keyword, LexError, Token};
use crate::sql::plan::{self, EvaluationOrder, Stage};
use crate::sql::udf_call::{
    BinOp, Expr, MAX_CALL_ARGS, MAX_EXPR_DEPTH, MAX_EXPR_NODES, MAX_UDF_PARAMS,
};
use crate::sql::using_operation_id::OperationId;

/// エラーメッセージへ含める入力断片の長さ上限。untrusted 入力をそのまま無加工で
/// 長大にエラーへ埋め込まない（security.md「情報漏えい」対応）。値は
/// [`crate::error_format`] の単一真実源を参照し、wire 応答直前の最終切り詰め
/// （`WireError::new`）と本モジュールの構築時切り詰めが別々の上限を持たないようにする
/// （TASK-152・ERR-2）。
const MAX_ERROR_DETAIL_LEN: usize = crate::error_format::MAX_MESSAGE_LEN;

/// INSERT の列リスト・VALUES リストがそれぞれ持てる要素数の上限（SQL-10、TASK-80）。
/// 無制限 `Vec` 確保を避ける（`.claude/rules/security.md`「不安全な設計｜無制限
/// リソース確保（DoS）」対応）。`catalog::MAX_COLUMN_COUNT` と同値を採用する。
const MAX_INSERT_COLUMNS: usize = 256;

/// `CREATE INDEX` の列リストが持てる要素数の上限（TASK-206・INDEX-7、Issue #908）。
/// `MAX_INSERT_COLUMNS` と同値を採用する（無制限 `Vec` 確保を避ける。
/// `.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応）。
pub(crate) const MAX_INDEX_DDL_COLUMNS: usize = 256;

/// サブクエリ（`IN (SELECT ...)`・`EXISTS (SELECT ...)`）のネスト深さ上限
/// （Issue #927・SQL-29 (a)・TASK-213。実装既定値）。`Parser::subquery_ctx` の
/// 深さがこれを超える箇所を構文解析段で `54000` へ落とし、`sql::subquery` の
/// 解決（再帰的な内側実行）がスタック・実行コストとも定数段に収まるようにする。
pub(crate) const MAX_SUBQUERY_DEPTH: usize = 4;

/// `CREATE TABLE`（SQL-23・TASK-85、Issue #899）の列リストが持てる列数の上限。
/// `catalog::MAX_COLUMN_COUNT` と同値を採用する（`MAX_INSERT_COLUMNS` と同じ
/// 「無制限 `Vec` 確保を避ける」設計方針。列を `Vec` へ push する**前**に判定し、
/// アロケーション前の上限検証（`.claude/rules/security.md`）を満たす）。
const MAX_CREATE_TABLE_COLUMNS: usize = 256;

/// `CREATE TABLE` の列定義が受理する列型キーワード（[`Parser::parse_create_table_column`]
/// が照合する語と一致させる契約。型を追加する際はここも同時に拡張する）。
/// `CHECK` 制約名がこれらと一致する場合は拒否する（TABLE-16・TASK-204、
/// Issue #906。[`Parser::peek_check_clause_start`] の「曖昧さの排除」参照）。
const CREATE_TABLE_COLUMN_TYPE_KEYWORDS: &[&str] = &["TEXT", "VECTOR", "INTEGER", "BIGINT"];

/// `PRIMARY KEY (<col>[, <col>]*)` に宣言できる列数の上限（TABLE-16・
/// TASK-204、Issue #903）。`catalog::validate_schema` が同じ上限
/// （[`crate::catalog::MAX_PRIMARY_KEY_COLUMNS`]）で再検証するため、単一の
/// 定数を共有しドリフトを防ぐ。
use crate::catalog::MAX_PRIMARY_KEY_COLUMNS;

/// 複数行 `VALUES (...), (...), ...`（SQL-16、TASK-190）が 1 文に持てる行数の上限。
/// `sql/group_by.rs::MAX_GROUPS` と同じ「本リポ独自の実装既定値・無制限 `Vec`
/// 確保を避ける」設計方針を踏襲する（security.md「不安全な設計｜無制限リソース
/// 確保（DoS）」対応）。超過は行を `rows` へ追加する直前（書き込みトランザクション
/// 開始のはるか手前・構造検証段階）に検出し、[`SqlSurfaceError::payload_too_large`]
/// （`54000`）で fail-closed に拒否する（副作用ゼロ）。
const MAX_INSERT_ROWS_PER_STATEMENT: usize = 1_000;

/// UPDATE の SET 句が持てる代入要素数の上限（SQL-17、TASK-191）。`MAX_INSERT_COLUMNS`
/// とは独立した定数にする（UPDATE は部分更新であり INSERT の列数上限とは意味論が
/// 異なるため、将来どちらかだけを見直す際に互いへ波及しないようにする）。無制限
/// `Vec` 確保を避ける（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。
const MAX_UPDATE_SET_ASSIGNMENTS: usize = 256;

/// ORDER BY の関数呼び出し形で許可する関数名を照合する（大文字小文字を区別しない）。
/// 未知の名前は fail-closed に拒否し、識別子であれば任意の名前を関数呼び出しとして
/// 受理してしまう構造上の抜け穴を作らない。
fn is_allowed_order_by_function_name(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "HYBRID_RRF" | "HYBRID")
}

/// WHERE の述語呼び出し形（空引数）で許可する述語名を照合する（大文字小文字を
/// 区別しない）。未知の名前は fail-closed に拒否する。
///
/// `pub`: `wire-server::http::query::filter`（NoSQL 表層の `filter` 配列。
/// Issue #761・TASK-175・NOSQL-7）が、クライアント指定の `column` が RLS
/// 述語名と衝突しないことを確認するために呼ぶ（RLS はサーバー側暗黙適用の
/// みであり、`filter` 経由で述語名を指定・解除できる経路を作らないための
/// 判定。RLS 述語名を 2 クレートにハードコードしない単一情報源）。
pub fn is_allowed_where_predicate_name(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "VISIBLE")
}

/// `WHERE flag`（BOOLEAN 列の裸参照。Issue #883・D-c）を受理してよい直後の
/// トークンかどうかを判定する。`WHERE` 句を持つ 4 箇所（SELECT／集計／広域取得
/// scan・UPDATE・DELETE）のいずれでも、`WHERE` 句の直後に続き得る構文
/// （`AND`・`ORDER BY`・`LIMIT`・文末・`;`・`USING`／`RETURNING`／`GROUP`／
/// `HAVING` の各文脈キーワード）の開始位置に限って裸識別子を BOOLEAN 述語と
/// みなす。受理範囲をこの集合に限定することで、`flag + 1` のような式の一部を
/// 誤って BOOLEAN 述語と解釈しない（既存の式フォールバックへそのまま委譲する）。
/// `extra_close_paren` が `true` の場合に限り `)` も境界として扱う（`CHECK (...)`
/// の本体を [`Parser::parse_check_body`] が解析する場合のみ。通常の `WHERE` 句
/// 解析は `false` を渡し、既存の受理範囲を一切変えない。TABLE-16・TASK-204、
/// Issue #906）。
fn is_where_predicate_boundary_token(token: Option<&Token>, extra_close_paren: bool) -> bool {
    match token {
        None => true,
        Some(Token::Punct(';')) => true,
        Some(Token::Punct(')')) if extra_close_paren => true,
        Some(Token::Keyword(Keyword::And)) => true,
        Some(Token::Keyword(Keyword::Order)) => true,
        Some(Token::Keyword(Keyword::Limit)) => true,
        Some(Token::Ident(w)) => {
            w.eq_ignore_ascii_case("USING")
                || w.eq_ignore_ascii_case("HINT")
                || w.eq_ignore_ascii_case("RETURNING")
                || w.eq_ignore_ascii_case("GROUP")
                || w.eq_ignore_ascii_case("HAVING")
                || w.eq_ignore_ascii_case("OR")
        }
        _ => false,
    }
}

/// `WHERE`（・`CHECK` 本体）述語ツリーが持てる葉（`Or` を含まない末端述語）の
/// 総数上限（TASK-208・SQL-24、Issue #912）。`declarative_filter::
/// MAX_METADATA_FILTERS` と同じ値を採用する（下流の索引・束縛段が同じ上限を
/// 前提にできるよう単一の数値基準に揃える）。`push` の**前**に検査し、超過分の
/// アロケーションを発生させない（security.md「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。
const MAX_WHERE_LEAVES: usize = crate::declarative_filter::MAX_METADATA_FILTERS;

/// `WHERE` 述語ツリーの括弧グルーピング（`Or` の入れ子）が持てる最大深さ
/// （TASK-208・SQL-24、Issue #912）。`sql::udf_call::MAX_EXPR_DEPTH` と同じ実装
/// 既定値を採用する（式の再帰深さ上限と同じ設計判断）。再帰呼び出しの**前**に
/// 検査し、深いネスト入力によるスタック消費を定数に抑える。
const MAX_WHERE_GROUP_DEPTH: usize = MAX_EXPR_DEPTH;

/// `WHERE` の括弧グループ `(...)` の直後のトークンが、値式（`(id + 1) > 5` 等）の
/// 一部であることを示す比較・算術演算子かどうかを判定する（TASK-208・SQL-24、
/// Issue #912。[`Parser::parse_where_atom`] の決定的先読みが使う）。それ以外の
/// トークンは BOOLEAN グループ（`(a OR b)`）として解析する。
fn is_where_group_operator_token(token: &Token) -> bool {
    matches!(
        token,
        Token::Punct('=')
            | Token::Punct('<')
            | Token::Punct('>')
            | Token::Le
            | Token::Ge
            | Token::Punct('+')
            | Token::Punct('-')
            | Token::Punct('*')
            | Token::Punct('/')
    )
}

/// `predicates`（`AND` 列）が `visible()` 述語呼び出しを直接含むかどうかを判定する
/// （RLS-7・Issue #912）。`Or` の腕へは再帰しない（`Or` 分岐内の `visible()` は
/// 呼び出し元（[`Parser::parse_where_or`]）が分岐生成時点で個別に拒否するため、
/// ここでは「1 つの AND 列の直下」だけを見れば十分）。
fn where_predicates_contain_visible(predicates: &[WherePredicate]) -> bool {
    predicates.iter().any(|predicate| {
        matches!(
            predicate,
            WherePredicate::PredicateCall { name } if name.eq_ignore_ascii_case("VISIBLE")
        )
    })
}

/// `predicates` が [`WherePredicate::Or`] を（直接またはネストした分岐の内部に）
/// 1 つでも含むかどうかを判定する（TASK-208・Issue #912）。`CHECK (...)` 本体
/// （[`Parser::parse_check_clause`]）が `OR` を明示的に拒否するために使う。
fn where_predicates_contain_or(predicates: &[WherePredicate]) -> bool {
    predicates
        .iter()
        .any(|predicate| matches!(predicate, WherePredicate::Or(_)))
}

/// `token` が `WHERE` 述語の範囲比較演算子（`< > <= >=`）トークンであれば
/// 対応する [`CompareOp`] を返す（TABLE-13・TASK-199、Issue #891）。`=` は
/// 既存の [`WherePredicate::Equality`] 判定が別腕で扱うためここには含めない。
fn where_compare_op_token(token: &Token) -> Option<CompareOp> {
    match token {
        Token::Punct('<') => Some(CompareOp::Lt),
        Token::Punct('>') => Some(CompareOp::Gt),
        Token::Le => Some(CompareOp::Le),
        Token::Ge => Some(CompareOp::Ge),
        _ => None,
    }
}

/// 集計関数（TASK-166・SQL-13）で許可する関数名を照合する（大文字小文字を区別
/// しない）。未知の名前は fail-closed に拒否する（[`is_allowed_where_predicate_name`]
/// と同方針）。`sql::udf_call::is_reserved_function_name` から名前空間一本化の
/// ため参照される（CREATE FUNCTION での集計関数名との衝突を防ぐ。Cursor Bugbot
/// 指摘対応・PR #229）。
pub(crate) fn is_aggregate_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

/// `DISTINCT` が字句解析上 `Token::Ident` であること（[`catalog::validate_identifier`]
/// は列名 `distinct` を許可している）に由来する文脈判定（SQL-25 (c)・TASK-209）。
/// `tokens[pos]` が大文字小文字を無視して `DISTINCT` に一致し、かつ次のトークンが
/// `Ident`（`AS` を除く）または `'*'` の場合に限り修飾子とみなす。次が `FROM`・
/// `,`・`)`・`(`・文末のときは列名として扱う（`SELECT distinct FROM t`・
/// `COUNT(distinct)`・`WHERE distinct = 'x'` 等の既存の列参照としての解釈を
/// 壊さない）。
///
/// 次トークンが `Ident` として字句解析される `AS` の場合はさらに 2 つの読みが
/// 衝突する（`catalog::validate_identifier` は列名 `as` も許可しているため）。
/// PR #1098 レビュー対応（codex/review P1・Cursor Bugbot 指摘）:
/// - `DISTINCT AS <alias>`（`tokens[pos+2]` が `Ident` で、かつそれ自身が
///   `AS` 由来の連鎖ではない）: `DISTINCT` 自体が列名で、`AS <alias>` は
///   その別名（[`Parser::parse_aggregate_select_item`] が受理する「裸の
///   識別子 ＋ 任意の `AS <alias>`」の形。GROUP BY 対象列として使う
///   `SELECT distinct AS d, COUNT(*) FROM t GROUP BY distinct` 等）。
///   この場合は列名側（`false`）へ振り分ける。
/// - `DISTINCT AS`（`tokens[pos+2]` が `Ident` でない＝`FROM`・`,`・`)`・
///   文末等）: 列名 `as` の前に置かれた `DISTINCT` 修飾子（
///   `COUNT(DISTINCT as)`・`SELECT DISTINCT as FROM t`）。この場合は修飾子側
///   （`true`）へ振り分ける。
/// - `DISTINCT as AS <alias>`（`tokens[pos+2]` も `AS` 由来の `Ident` で、
///   その次（`tokens[pos+3]`）が `Ident`）: `tokens[pos+1]` の `as` は列名
///   （`DISTINCT` は修飾子）、`tokens[pos+2]` の `AS` は列名 `as` に付く
///   別名節の導入（`SELECT DISTINCT as AS alias FROM t`）。列名 `AS` を
///   単なる別名（上記 1 つ目のケース）と誤読すると、2 段目の `AS <alias>`
///   が余剰トークンとして構文エラーになり、この形状が拒否されてしまう
///   （PR #1098 レビュー対応・codex/review P1 再指摘）。この場合は修飾子側
///   （`true`）へ振り分ける。
pub(crate) fn is_distinct_modifier(tokens: &[Token], pos: usize) -> bool {
    matches!(tokens.get(pos), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("DISTINCT"))
        && match tokens.get(pos + 1) {
            Some(Token::Ident(next)) if next.eq_ignore_ascii_case("AS") => {
                match tokens.get(pos + 2) {
                    Some(Token::Ident(next2))
                        if next2.eq_ignore_ascii_case("AS")
                            && matches!(tokens.get(pos + 3), Some(Token::Ident(_))) =>
                    {
                        true
                    }
                    Some(Token::Ident(_)) => false,
                    _ => true,
                }
            }
            Some(Token::Ident(_)) => true,
            Some(Token::Punct('*')) => true,
            _ => false,
        }
}

/// 1 文の集計項目リストが持てる要素数の上限（TASK-166・SQL-13）。無制限 `Vec` 確保を
/// 避ける（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」
/// 対応）。
///
/// `pub`（TASK-186・NOSQL-4）: `wire-server::http::query::aggregate` が
/// `POST /v1/query`（`op: aggregate`）の `aggregates` 配列を写像する**前**
/// （`Vec` 確保・`String` 複製より前）に、SQL 表層と同じ上限を検査するために
/// 参照する（[`check_aggregate_item_count`] 経由。`declarative_filter::
/// MAX_METADATA_FILTERS`／`check_filter_count` と同じ設計判断）。
pub const MAX_AGGREGATE_ITEMS: usize = 32;

/// ウィンドウ項目（SQL-30・TASK-214）の `PARTITION BY`・`ORDER BY` 句が持てる
/// 列数の上限。SQL-25 (d) の列数上限と同じ実装既定値を流用する（無制限 `Vec`
/// 確保を避ける方針。security.md「不安全な設計」対応）。
pub(crate) const MAX_WINDOW_KEYS: usize = 8;

/// `count` 件の集計項目が [`MAX_AGGREGATE_ITEMS`] を超えないことを検証する
/// （`54000`）。`Vec` 確保・要素の複製より**前**に呼べる形にする
/// （[`crate::declarative_filter::check_filter_count`] と同じ設計判断。
/// TASK-186・NOSQL-4: `wire-server::http::query::aggregate::bind` が
/// JSON 配列要素を写像する前に呼ぶ）。
pub fn check_aggregate_item_count(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_AGGREGATE_ITEMS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "aggregate item count {count} exceeds limit {MAX_AGGREGATE_ITEMS}"
        )));
    }
    Ok(())
}

/// `count` 件の `HAVING` 述語が [`MAX_AGGREGATE_ITEMS`] を超えないことを検証する
/// （`54000`）。[`Parser::parse_having`] の構文層と同じ上限値を用いる
/// （TASK-186・NOSQL-5: `wire-server::http::query::aggregate::bind` が `having`
/// 配列要素を写像する**前**に呼ぶ。[`check_aggregate_item_count`] と同じ
/// 設計判断）。
pub fn check_having_predicate_count(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_AGGREGATE_ITEMS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "HAVING predicate count {count} exceeds limit {MAX_AGGREGATE_ITEMS}"
        )));
    }
    Ok(())
}

/// `GROUP BY` 句が持てるグループキー列数の上限（TASK-167・SQL-25 (d) の実装既定値）。
/// [`check_group_by_column_count`] が確保前検査に使う（`.claude/rules/security.md`
/// 「不安全な設計｜無制限リソース確保（DoS）」対応）。
///
/// `pub`（TASK-186 と同じ設計判断）: `wire-server::http::query::aggregate` が
/// NoSQL 表層の `group_by` 配列形（NOSQL-16 (b)）を写像する前に、SQL 表層と同じ
/// 上限を検査するために参照できるようにする。
pub const MAX_GROUP_BY_COLUMNS: usize = 8;

/// `count` 件の `GROUP BY` 列が [`MAX_GROUP_BY_COLUMNS`] を超えないことを検証する
/// （`54000`）。[`Parser::parse_group_by_clause`] が列を `push` する**前**に呼ぶ
/// （[`check_having_predicate_count`] と同じ「確保前検査」の設計判断）。
pub fn check_group_by_column_count(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_GROUP_BY_COLUMNS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "GROUP BY column count {count} exceeds limit {MAX_GROUP_BY_COLUMNS}"
        )));
    }
    Ok(())
}

/// `USING PLAN('<query>')`（TASK-77・SQL-5）に渡せる自然言語クエリ本文のバイト長
/// 上限。アロケーション（字句解析・LLM プロンプトへの組み込み）に入る前に拒否する
/// （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応）。
/// [`crate::sql::parser::MAX_VECTOR_LITERAL_BYTES`] の既存前例と同じ 64 KiB を採用する
/// （意味論側の決定的切り詰めは `query_planner::MAX_QUESTION_CHARS` が別途担う）。
const MAX_USING_PLAN_LEN: usize = 64 * 1024;

/// `USING PLAN('<query>')` のリテラル値検証本体（空文字・[`MAX_USING_PLAN_LEN`]
/// 超過を拒否する）を、SQL 表層のパーサ内 inline 判定
/// （[`Parser::parse_using_plan_clause`]）と NoSQL 表層（`wire-server::http::
/// query::search`。Issue #763・TASK-175・NOSQL-2）の `search.plan` 束縛とで
/// 共有する単一実装。SQL 表層は字句解析済みの文字列リテラルを渡すため本関数
/// より前に受理済みだが、NoSQL 表層は JSON 文字列（1 MiB 上限まで到達し得る）を
/// そのまま渡すため、この検証で SQL 側と同じ上限（`54000`）まで縮小する。
///
/// 空リテラルは [`SqlSurfaceError::invalid_input`]（`22000`）、
/// [`MAX_USING_PLAN_LEN`] 超過は [`SqlSurfaceError::payload_too_large`]
/// （`54000`）で拒否する。
pub fn validate_using_plan_question(question: &str) -> Result<(), SqlSurfaceError> {
    if question.is_empty() {
        return Err(SqlSurfaceError::invalid_input(
            "USING PLAN value must not be empty",
        ));
    }
    if question.len() > MAX_USING_PLAN_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "USING PLAN value length {} exceeds limit {MAX_USING_PLAN_LEN}",
            question.len()
        )));
    }
    Ok(())
}

fn truncate_for_error(s: &str) -> String {
    if s.len() <= MAX_ERROR_DETAIL_LEN {
        return s.to_string();
    }
    // 文字境界で安全に切り詰める（マルチバイト文字の途中で切らない）。添字直接
    // アクセスをせず `get()` で明示的に処理する（coding-rust.md 「untrusted 入力の扱い」）。
    let mut end = MAX_ERROR_DETAIL_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match s.get(..end) {
        Some(prefix) => format!("{prefix}..."),
        None => "...".to_string(),
    }
}

/// SQL 表層のエラー型。ERR-2 参照（docs/spec/04-behavior/error-format.md）。
/// engine 全体の共通エラー型統合は他タスクの管轄のため、本モジュールローカルの
/// 型として定義する。
#[derive(Debug, Clone)]
pub enum SqlSurfaceError {
    /// 許可リスト外の構文（構文解析失敗を含む）。
    UnsupportedSyntax { detail: String },
    /// FROM に指定したテーブルがスキーマカタログに存在しない。
    UndefinedTable { name: String },
    /// カタログ照会（`TableLookup`）側の内部エラー（redb I/O 等）。受理・拒否のいずれにも
    /// 倒さず、fail-closed にエラー伝播する（`.claude/rules/security.md`
    /// 「不安全な設計」対応）。
    Internal { detail: String },
    /// 構造は許可リストを通過したが、束縛（`sql::parser::bind`、TASK-75）が値・引数を
    /// 意味論的に不正と判定した（未知の列名・列型不一致・ベクトルリテラルの不正形式・
    /// 非有限値・次元不一致、`LIMIT` 範囲外、hybrid の 2 引数形（実行不能）等。ERR-2:
    /// `22000`）。
    InvalidInput { detail: String },
    /// untrusted 入力のサイズがアロケーション前の上限を超過した（ベクトルリテラル
    /// 64 KiB 超過、候補集合の容量上限超過等。ERR-2: `54000`）。
    PayloadTooLarge { detail: String },
    /// 書き込み系 SQL 文の文末専用句 `USING OPERATION_ID '<id>'`（SQL-10、TASK-80）の
    /// 省略（空文字値を含む）。RECOVER-1 の必須化ガードの前段として、SQL 表層が
    /// 書き込みトランザクションを開始する**前**に構造検証段階で fail-closed に
    /// 拒否する（ERR-2: `23502`）。
    MissingOperationId,
    /// INSERT 先の行 `id` が**呼び出し元テナントの名前空間内で**既に使われている
    /// （`tenant::insert_typed_row` の [`crate::tenant::TenantWriteError::IdConflict`]
    /// を SQL 表層へ写像したもの。ERR-2: `23505`）。行ストアの物理キーは
    /// `(tenant_id, id)` で名前空間化されているため（TABLE-12・RLS-9）、他テナントが
    /// 同じ `id` を保持していても本 variant にはならない。`operation_id` 単位の
    /// 冪等判定（台帳による重複拒否・内容不一致検出）は [`SqlSurfaceError::DuplicateOperationId`]・
    /// [`SqlSurfaceError::OperationIdContentMismatch`] が担う（TASK-101・RECOVER-10。
    /// TASK-94・RECOVER-3 の重複拒否契約を包含する）。
    IdConflict,
    /// `operation_id` 台帳（TASK-93）に記録済みの `operation_id` へ、**内容が一致する**
    /// 書き込みが再送された（TASK-101・対象ビヘイビア: RECOVER-10。
    /// [`crate::tenant::TenantWriteError::DuplicateOperationId`] の写像。ERR-2: `23505`）。
    /// 行キー衝突（[`SqlSurfaceError::IdConflict`]）とは別の固定文言を返し、クライアント
    /// が両者を取り違えないようにする。
    DuplicateOperationId,
    /// 台帳に記録済みの `operation_id` へ、**内容が異なる**書き込みが再送された、
    /// または内容一致を証明できない旧フォーマットの台帳エントリへ再送された
    /// （TASK-101・RECOVER-10。[`crate::tenant::TenantWriteError::OperationIdContentMismatch`]
    /// の写像。ERR-3（TASK-154）: `22023`。他のいかなる分類（特に `23505`）にも写像しない
    /// ことは `tests/error_format_err3.rs` で検証する。fail-closed:
    /// commit 済み確定の根拠にしない）。
    OperationIdContentMismatch,
    /// 集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`、TASK-166・SQL-13）の数値演算が
    /// `u64`/`f64` の表現範囲を超過した（`checked_add` 失敗・`f64` 側の非有限値化）。
    /// 黙って wrap・非有限値化せず fail-closed に拒否する（`.claude/rules/coding-rust.md`
    /// 「整数演算は checked_*/saturating_* を使う」対応）。`22003` は ERR-2
    /// （`docs/spec/04-behavior/error-format.md`）の表に未掲載のコードであり、
    /// SQL-13 が ERR-2 の拡張規則に基づいて独自定義する。
    NumericOutOfRange { detail: String },
    /// `DATE`／`TIMESTAMP` リテラルが文法上は解析できたが、値が受理範囲外、
    /// または暦上不正（月 13・2 月 30 日・非閏年の 2/29・時 24・分 60・秒 60・
    /// 年 0000・年 10000 以上等。TABLE-13・TASK-197、Issue #884・D-1）。
    /// 文法違反（区切り文字違い・TZ 接尾辞・桁数不足等）は既存の `InvalidInput`
    /// （`22000`）のまま変えない。ERR-6 の管轄表にある `22008`
    /// （`DATETIME_FIELD_OVERFLOW`）へ写像する新規分類。
    DatetimeFieldOverflow { detail: String },
    /// 構文上受理された値が、宣言済み型の表現として不正（TABLE-14・TASK-198、
    /// Issue #890）。ENUM 列の語彙外ラベル（[`crate::catalog::EnumLabelError`]）に
    /// 加え、UUID 列（TABLE-13〔検討中〕・TASK-197、Issue #887）の厳密文法違反
    /// （`sql::parser::bind_uuid_literal`）も同じ発生経路を共有する。ERR-2 拡張:
    /// `22P02`（[`crate::error_format::ErrorClass::InvalidTextRepresentation`]）。
    InvalidTextRepresentation { detail: String },
    /// 明示トランザクション（SQL-31・TASK-221）内で発生した一般的な状態不整合
    /// （同一トランザクション内での `operation_id` の再利用等）。`25000`。
    InvalidTransactionState,
    /// `Active` なトランザクション中に再度 `BEGIN` を送った（`25001`）。
    ActiveSqlTransaction,
    /// `Idle`（トランザクション外）で `COMMIT`／`ROLLBACK` を送った（`25P01`）。
    NoActiveSqlTransaction,
    /// `Failed` なトランザクション中に `ROLLBACK` 以外の文を送った（`25P02`）。
    InFailedSqlTransaction,
    /// 明示トランザクションの単一ライタ占有により、書き込みトランザクションの
    /// 取得（[`crate::storage::Storage::begin_write_txn`]）がロック待ちの上限を
    /// 超過した（`55P03`）。
    LockNotAvailable,
    /// 明示トランザクション（SQL-31・TASK-221）内で構文としては受理されるものの、
    /// 本実装がトランザクション文脈での実行に未対応な文（複数行/ファイル形/
    /// `ON CONFLICT` の `INSERT`・`UPDATE`・`DELETE`・`UPSERT`・COPY・既に書き込んだ
    /// テーブルへの読み取り等）を拒否する（`0A000`。`42601`
    /// ［`UnsupportedSyntax`。構文自体が許可リスト外］とは区別する）。
    TransactionFeatureNotSupported { detail: String },
    /// 構文・入力としては正しいが、この表層ではまだ実装されていない機能
    /// （NoSQL `filter` の `eq` を `INTEGER`／`BIGINT`／`REAL`／
    /// `DOUBLE PRECISION` 列へ適用する等。式レーンの入口が無いための対象外。
    /// Issue #945）。ERR-2: `0A000`（[`crate::error_format::ErrorClass::
    /// FeatureNotSupported`]）。`UnsupportedSyntax`（`42601`。構文そのものが
    /// 許可リスト外）とは意味論的に異なるため、`wire-server` 側の分類縮退
    /// （Issue #896 レビュー指摘）を避けるために独立させた。
    FeatureNotSupported { detail: String },
    /// `CREATE TABLE`（SQL-23・TASK-85、Issue #899）が指定したテーブル名が既に
    /// カタログに存在する。既存スキーマは変更しない（TABLE-4）。`CREATE VIEW`
    /// （TABLE-18・SQL-23・TASK-205、Issue #909）が既存のテーブル名・ビュー名と
    /// 衝突した場合も同じ分類を共有する（ビューはテーブルと名前空間を共有する）。
    /// ERR-6: `42P07`。
    DuplicateTable { name: String },
    /// `CREATE TABLE` の列リストに同名の列が複数回宣言された（TABLE-6、
    /// Issue #899）。ERR-6: `42701`。
    DuplicateColumn { name: String },
    /// DDL 文（`CREATE TABLE`・`DROP TABLE` 等。SQL-23、TASK-202・TASK-203、
    /// Issue #899・#902）を、DDL 実行権限（[`crate::sql::mode::SessionState::
    /// ddl_allowed`]）を持たないセッションが実行しようとした（ERR-2 拡張:
    /// `42501`。[`crate::error_format::ErrorClass::ForbiddenTenantMismatch`] を
    /// 再利用する——テナント帰属不一致とは原因が異なるが `wire_code` は同じ
    /// `42501` であり、`ErrorClass` は分類名ではなく `wire_code` の一意対応を
    /// 保証する単位のため、新規分類は追加しない）。判定順序は構文検証の
    /// 直後・カタログ照会（対象テーブルの存在確認）より前
    /// （`sql::ddl::require_ddl_permission` 参照）: 権限を持たない主体には対象
    /// テーブルの有無を問わず常にこの分類を返し、DDL 権限をテーブル存在の
    /// オラクルにしない（security.md「エラー・ログ経由で他テナントのデータ・
    /// 存在情報を漏らさない」対応）。固定文言のみを保持し、テーブル名・
    /// ユーザー名を含めない。
    InsufficientPrivilege,
    /// `FETCH`／`CLOSE` が参照したカーソル名が、現在のトランザクション内に
    /// 存在しない（WIRE-15・TASK-218）。他セッション所有のカーソル名・単に
    /// 存在しない名前のいずれも区別しない固定文言のみを保持し、カーソル名
    /// 自体を含めない（security.md「存在情報を漏らさない」対応。ERR-6: `34000`）。
    InvalidCursorName,
    /// `DROP TABLE`／`DROP VIEW` の対象を、それを参照するビューが 1 つ以上
    /// 残っているため削除できない（TABLE-18・SQL-23・TASK-205、Issue #909。
    /// ERR-6: `2BP01`）。依存元の名前一覧はエラー文言に含めない
    /// （security.md P0）。
    DependentObjectsStillExist { name: String },
    /// 名前は存在するが、要求された操作が期待するオブジェクト種別と一致
    /// しない（`DROP TABLE` にビュー名、`DROP VIEW` にテーブル名、ビューへの
    /// 書き込み系文。TABLE-18・SQL-23・TASK-205、Issue #909。ERR-6: `42809`）。
    WrongObjectType { name: String },
    /// 列に `NOT NULL` 制約が宣言されているにもかかわらず、値が省略された、
    /// または明示的に `NULL` として書き込まれた（TABLE-16・TASK-204、
    /// Issue #904）。`DEFAULT` 句を持つ列は省略時に既定値が補われるため
    /// 本 variant にならない（明示 `NULL` は `DEFAULT` を適用せずこちらへ
    /// 倒す。TABLE-16 の確定契約）。ERR-6: `23502`
    /// （[`crate::error_format::ErrorClass::NotNullViolation`]。`wire_code` は
    /// [`SqlSurfaceError::MissingOperationId`] と共有し `code` ラベルでのみ
    /// 区別する）。
    NotNullViolation { column: String },
    /// `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約に
    /// 違反した（[`crate::tenant::TenantWriteError::UniqueViolation`] の写像。
    /// ERR-6: `23505`）。行キー衝突（[`SqlSurfaceError::IdConflict`]。物理キー
    /// `(tenant_id, id)` の衝突）とは別の固定文言を返す。
    UniqueViolation,
    /// `DROP INDEX` で指定した名前が索引として存在しない（TASK-206・INDEX-7、
    /// Issue #908。[`crate::catalog::CatalogError::IndexNotFound`] の写像。
    /// ERR-6: `42704`）。
    UndefinedObject { name: String },
    /// 索引 DDL が参照した列が対象テーブルに存在しない（TASK-206・INDEX-7、
    /// Issue #908。[`crate::catalog::CatalogError::ColumnNotFound`] の写像。
    /// ERR-6: `42703`）。
    UndefinedColumn { name: String },
    /// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）が宣言する述語を、書き込もう
    /// とした行の値が満たさない。ERR-6: `23514`。
    /// [`crate::tenant::TenantWriteError::CheckViolation`] の写像。制約名のみを
    /// 保持する（行の値・id・テナントは含めない。security.md P0）。
    CheckViolation { constraint: String },
    /// `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）の参照整合性違反
    /// （[`crate::tenant::TenantWriteError::ForeignKeyViolation`] の写像。ERR-6:
    /// `23503`）。値・行 id・テナント・参照先テーブル名、および参照先が「不在」か
    /// 「他テナント所有」かを区別する情報を一切含めない固定文言（RLS-9・
    /// RLS-10 (c)。security.md P0）。
    ForeignKeyViolation,
    /// `FOREIGN KEY` 宣言の参照先列が主キー・UNIQUE 制約（または `id` 疑似列）と
    /// 一致しない、あるいは参照元列と型が一致しない（TABLE-17・TASK-205、
    /// Issue #907。[`crate::catalog::CatalogError::InvalidForeignKey`] の写像。
    /// ERR-6: `42830`）。`detail` はカタログ情報（列名・テーブル名）のみ。
    InvalidForeignKey { detail: String },
    /// 複数テーブル参照スコープ（`sql::relation::BindingScope`、SQL-28・RLS-10・
    /// Issue #924）で、非修飾列参照が 2 つ以上の参照テーブルに一致した
    /// （候補が曖昧で一意に解決できない）。ERR-6: `42702`。文言には列名のみを
    /// 含め、候補テーブルの列挙はしない（security.md P0「存在情報を漏らさない」
    /// 対応。曖昧な列は「どのテーブルの候補があるか」自体が情報になり得る）。
    AmbiguousColumn { name: String },
}

impl SqlSurfaceError {
    /// ERR-2（docs/spec/04-behavior/error-format.md）の wire_code 写像。
    /// TASK-152 で単一真実源化した [`ClassifiedError::wire_code`] へ委譲する
    /// （既存の返値は 1 つも変えない。委譲先は `error_class()` の `match` のみを
    /// 単一の判定点として持つ）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }

    /// クライアント（wire 層 `ErrorResponse`）へそのまま返してよい文言を返す。
    /// `Internal`（`wire_code() == "XX000"`）は redb I/O エラー等の内部ストレージ
    /// 詳細を保持しているため固定の一般化メッセージへ丸め、それ以外の variant は
    /// 通常の `Display` 文言（テナント越境の存在情報を含まないよう各コンストラクタ
    /// 側で既に切り詰め・一般化済み）をそのまま返す（security.md P0「private
    /// 情報の漏えい」対応。`wire-server::simple_query` はエラー応答の整形時に
    /// `to_string()` ではなく必ずこちらを使うこと）。TASK-152 で
    /// [`ClassifiedError::client_message`] へ委譲する（返値は不変）。
    pub fn client_message(&self) -> String {
        ClassifiedError::client_message(self)
    }

    /// `pub(crate)`: `sql::allowlist::Parser::parse_operation_id_clause`・
    /// `sql::using_operation_id::OperationId::parse` が文末句の省略（空文字値を
    /// 含む）を報告するために使う（SQL-10、TASK-80）。
    pub(crate) fn missing_operation_id() -> Self {
        SqlSurfaceError::MissingOperationId
    }

    /// `pub(crate)`: `catalog.rs::impl TableLookup for Storage` が `CatalogError::Invalid`
    /// を `42601` へ写像する際にも、同じ切り詰め規約を経由させるために公開する。
    pub(crate) fn unsupported(detail: impl Into<String>) -> Self {
        SqlSurfaceError::UnsupportedSyntax {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `core.rs::EngineCore::execute_in_active_txn`／
    /// `read_only_in_active_txn`（SQL-31・TASK-221）が、明示トランザクション内で
    /// 未対応の文を `0A000` で拒否するために使う。
    pub(crate) fn transaction_feature_not_supported(detail: impl Into<String>) -> Self {
        SqlSurfaceError::TransactionFeatureNotSupported {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// FROM に指定されたテーブルがカタログ未存在（ERR-2: `42P01`）。テーブル名は
    /// untrusted な字句解析結果のため、`UnsupportedSyntax` と同様に長さを切り詰めて
    /// エラーへ含める（security.md「情報漏えい」対応）。`pub(crate)`:
    /// `sql::view::resolve_from`（TABLE-18・SQL-23・TASK-205、Issue #909）も
    /// FROM 解決失敗を同じ形へ写像するために使う。
    pub(crate) fn undefined_table(name: impl Into<String>) -> Self {
        SqlSurfaceError::UndefinedTable {
            name: truncate_for_error(&name.into()),
        }
    }

    /// `pub(crate)`: `sql::ddl::execute_create_table`（Issue #899）が
    /// `catalog::CatalogError::TableAlreadyExists` を写像するために使う。
    /// テーブル名は untrusted な字句解析結果のため長さを切り詰める。
    pub(crate) fn duplicate_table(name: impl Into<String>) -> Self {
        SqlSurfaceError::DuplicateTable {
            name: truncate_for_error(&name.into()),
        }
    }

    /// `pub(crate)`: `CREATE TABLE`（Issue #899）の列リスト構造検証が同一文内の
    /// 列名重複を報告するために使う。
    pub(crate) fn duplicate_column(name: impl Into<String>) -> Self {
        SqlSurfaceError::DuplicateColumn {
            name: truncate_for_error(&name.into()),
        }
    }

    /// `pub(crate)`: `sql::parser::bind`（TASK-75）が束縛時の値・引数不正を報告するために
    /// 使う。他の variant と同じ切り詰め規約を経由する。
    pub(crate) fn invalid_input(detail: impl Into<String>) -> Self {
        SqlSurfaceError::InvalidInput {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::parser::bind`・`sql::exec`（TASK-75）がアロケーション前の
    /// サイズ上限超過を報告するために使う。
    pub(crate) fn payload_too_large(detail: impl Into<String>) -> Self {
        SqlSurfaceError::PayloadTooLarge {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `sql::cursor::CursorRegistry::fetch`／`close`（WIRE-15・TASK-218）が、
    /// 現在のトランザクション内に存在しないカーソル名を報告するために使う。
    /// 固定 variant（データを持たない）のため引数はない。`pub`（`pub(crate)`
    /// から昇格。PR #1049 レビュー指摘対応）——`wire-server::extended_query`
    /// が、カーソル `FETCH` 由来 portal の中断保持分を再送出する前に
    /// `CLOSE`／`COMMIT`／`ROLLBACK`／再 `DECLARE` を挟んでいないか検証する
    /// 経路で、同じ `34000` を直接構築するために使う（第 2 の実行器・第 2 の
    /// エラー分類を作らない設計）。
    pub fn invalid_cursor_name() -> Self {
        SqlSurfaceError::InvalidCursorName
    }

    /// `pub(crate)`: `sql::aggregate`（TASK-166・SQL-13）が集計の数値演算オーバー
    /// フロー（`u64` の `checked_add` 失敗・`f64` の非有限値化）を報告するために使う。
    pub(crate) fn numeric_out_of_range(detail: impl Into<String>) -> Self {
        SqlSurfaceError::NumericOutOfRange {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::parser::bind_datetime_literal`（TABLE-13・TASK-197、
    /// Issue #884）が `DATE`／`TIMESTAMP` リテラルの範囲外・暦上不正を報告する
    /// ために使う。
    pub(crate) fn datetime_field_overflow(detail: impl Into<String>) -> Self {
        SqlSurfaceError::DatetimeFieldOverflow {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::parser::bind_enum_literal`（Issue #890）が ENUM 列の
    /// 語彙外ラベルを報告するために使う。エラーメッセージには語彙の一覧を
    /// 含めない（型名とクライアント自身の入力値のみ。security.md P0）。
    pub(crate) fn invalid_text_representation(detail: impl Into<String>) -> Self {
        SqlSurfaceError::InvalidTextRepresentation {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::parser`（`bind_literal_for_column`・`fill_omitted_columns`
    /// 等）が NOT NULL 制約違反（列の省略・明示 NULL）を報告するために使う
    /// （TABLE-16・TASK-204、Issue #904）。他テナントの情報・値そのものは
    /// 含めず列名のみを保持する（security.md P0）。
    pub(crate) fn not_null_violation(column: impl Into<String>) -> Self {
        SqlSurfaceError::NotNullViolation {
            column: truncate_for_error(&column.into()),
        }
    }

    /// `pub(crate)`: `sql::exec::map_write_error` が
    /// [`crate::tenant::TenantWriteError::UniqueViolation`] を写像するために使う
    /// （`PRIMARY KEY`〔Issue #903〕・UNIQUE 制約〔Issue #905〕。TABLE-16・TASK-204）。
    pub(crate) fn unique_violation() -> Self {
        SqlSurfaceError::UniqueViolation
    }

    /// `pub(crate)`: `sql::exec::map_write_error`・`map_incremental_error`
    /// （Issue #906）が [`crate::tenant::TenantWriteError::CheckViolation`] を
    /// 写像するために使う。制約名は catalog 由来（識別子として検証済み）だが、
    /// 他 variant と同じ切り詰め規約を適用する。
    pub(crate) fn check_violation(constraint: impl Into<String>) -> Self {
        SqlSurfaceError::CheckViolation {
            constraint: truncate_for_error(&constraint.into()),
        }
    }

    /// `pub(crate)`: `sql::ddl::execute_create_table`（TABLE-17・TASK-205、
    /// Issue #907）が [`crate::catalog::CatalogError::InvalidForeignKey`] を写像する
    /// ために使う。他 variant と同じ切り詰め規約を経由する。
    pub(crate) fn invalid_foreign_key(detail: impl Into<String>) -> Self {
        SqlSurfaceError::InvalidForeignKey {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::relation::BindingScope::resolve`（SQL-28・RLS-10、
    /// Issue #924）が非修飾列参照の曖昧な解決を報告するために使う。列名は
    /// untrusted な字句解析結果のため他 variant と同じ切り詰め規約を経由する。
    pub(crate) fn ambiguous_column(name: impl Into<String>) -> Self {
        SqlSurfaceError::AmbiguousColumn {
            name: truncate_for_error(&name.into()),
        }
    }
}

/// TASK-152（ERR-2）: `wire_code` 写像の単一真実源 [`ErrorClass`] へ委譲する。
/// variant → `ErrorClass` の対応は既存 `wire_code()` の返値と 1:1 で一致させ、
/// 委譲化で応答コードを変えない（`IdConflict` は行 `id` 衝突であり、原因を問わない
/// 一意制約違反の分類 [`ErrorClass::UniqueViolation`]（`23505`）へ写像する）。
impl ClassifiedError for SqlSurfaceError {
    fn error_class(&self) -> ErrorClass {
        match self {
            SqlSurfaceError::UnsupportedSyntax { .. } => ErrorClass::UnsupportedSqlSyntax,
            SqlSurfaceError::UndefinedTable { .. } => ErrorClass::TableNotFound,
            SqlSurfaceError::Internal { .. } => ErrorClass::InternalError,
            SqlSurfaceError::InvalidInput { .. } => ErrorClass::InvalidInput,
            SqlSurfaceError::PayloadTooLarge { .. } => ErrorClass::PayloadTooLarge,
            SqlSurfaceError::MissingOperationId => ErrorClass::MissingOperationId,
            SqlSurfaceError::IdConflict => ErrorClass::UniqueViolation,
            SqlSurfaceError::DuplicateOperationId => ErrorClass::UniqueViolation,
            SqlSurfaceError::NumericOutOfRange { .. } => ErrorClass::NumericOutOfRange,
            SqlSurfaceError::OperationIdContentMismatch => ErrorClass::OperationIdContentMismatch,
            SqlSurfaceError::DatetimeFieldOverflow { .. } => ErrorClass::DatetimeFieldOverflow,
            SqlSurfaceError::InvalidTextRepresentation { .. } => {
                ErrorClass::InvalidTextRepresentation
            }
            SqlSurfaceError::InvalidTransactionState => ErrorClass::InvalidTransactionState,
            SqlSurfaceError::ActiveSqlTransaction => ErrorClass::ActiveSqlTransaction,
            SqlSurfaceError::NoActiveSqlTransaction => ErrorClass::NoActiveSqlTransaction,
            SqlSurfaceError::InFailedSqlTransaction => ErrorClass::InFailedSqlTransaction,
            SqlSurfaceError::LockNotAvailable => ErrorClass::LockNotAvailable,
            SqlSurfaceError::TransactionFeatureNotSupported { .. } => {
                ErrorClass::FeatureNotSupported
            }
            SqlSurfaceError::FeatureNotSupported { .. } => ErrorClass::FeatureNotSupported,
            SqlSurfaceError::DuplicateTable { .. } => ErrorClass::DuplicateTable,
            SqlSurfaceError::DuplicateColumn { .. } => ErrorClass::DuplicateColumn,
            SqlSurfaceError::InsufficientPrivilege => ErrorClass::ForbiddenTenantMismatch,
            SqlSurfaceError::InvalidCursorName => ErrorClass::InvalidCursorName,
            SqlSurfaceError::DependentObjectsStillExist { .. } => {
                ErrorClass::DependentObjectsStillExist
            }
            SqlSurfaceError::WrongObjectType { .. } => ErrorClass::WrongObjectType,
            SqlSurfaceError::NotNullViolation { .. } => ErrorClass::NotNullViolation,
            SqlSurfaceError::UniqueViolation => ErrorClass::UniqueViolation,
            SqlSurfaceError::UndefinedObject { .. } => ErrorClass::UndefinedObject,
            SqlSurfaceError::UndefinedColumn { .. } => ErrorClass::UndefinedColumn,
            SqlSurfaceError::CheckViolation { .. } => ErrorClass::CheckViolation,
            SqlSurfaceError::ForeignKeyViolation => ErrorClass::ForeignKeyViolation,
            SqlSurfaceError::InvalidForeignKey { .. } => ErrorClass::InvalidForeignKey,
            SqlSurfaceError::AmbiguousColumn { .. } => ErrorClass::AmbiguousColumn,
        }
    }

    fn client_message(&self) -> String {
        match self {
            SqlSurfaceError::Internal { .. } => "internal error".to_string(),
            other => other.to_string(),
        }
    }
}

impl std::fmt::Display for SqlSurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlSurfaceError::UnsupportedSyntax { detail } => {
                write!(f, "unsupported SQL syntax: {detail}")
            }
            SqlSurfaceError::UndefinedTable { name } => write!(f, "undefined table: {name}"),
            SqlSurfaceError::Internal { detail } => write!(f, "internal error: {detail}"),
            SqlSurfaceError::InvalidInput { detail } => write!(f, "invalid input: {detail}"),
            SqlSurfaceError::PayloadTooLarge { detail } => {
                write!(f, "payload too large: {detail}")
            }
            SqlSurfaceError::MissingOperationId => {
                write!(f, "missing USING OPERATION_ID clause")
            }
            // 所有テナント・行内容・他テナントの存在有無を一切含めない固定文言
            // （security.md P0）。同一テナント内の重複でのみ返るため、この応答自体が
            // 他テナントの行 id 存在オラクルにならない。
            SqlSurfaceError::IdConflict => {
                write!(f, "row id already exists")
            }
            // operation_id・行内容・テナントを含めない固定文言（security.md P0。
            // `crate::tenant::TenantWriteError::DuplicateOperationId`/
            // `OperationIdContentMismatch` と同じ契約。`IdConflict` の文言と区別できる
            // ことが目的）。
            SqlSurfaceError::DuplicateOperationId => {
                write!(f, "operation_id already recorded with the same content")
            }
            SqlSurfaceError::NumericOutOfRange { detail } => {
                write!(f, "numeric value out of range: {detail}")
            }
            SqlSurfaceError::OperationIdContentMismatch => {
                write!(f, "operation_id already recorded with different content")
            }
            SqlSurfaceError::DatetimeFieldOverflow { detail } => {
                write!(f, "datetime field overflow: {detail}")
            }
            SqlSurfaceError::InvalidTextRepresentation { detail } => {
                write!(f, "invalid text representation: {detail}")
            }
            SqlSurfaceError::InvalidTransactionState => {
                write!(f, "invalid transaction state")
            }
            SqlSurfaceError::ActiveSqlTransaction => {
                write!(f, "transaction already in progress")
            }
            SqlSurfaceError::NoActiveSqlTransaction => {
                write!(f, "no transaction in progress")
            }
            SqlSurfaceError::InFailedSqlTransaction => {
                write!(
                    f,
                    "current transaction is aborted, commands ignored until end of transaction block"
                )
            }
            SqlSurfaceError::LockNotAvailable => {
                write!(f, "write lock not available: timed out waiting for writer")
            }
            SqlSurfaceError::TransactionFeatureNotSupported { detail } => {
                write!(f, "not supported inside an explicit transaction: {detail}")
            }
            SqlSurfaceError::FeatureNotSupported { detail } => {
                write!(f, "feature not supported: {detail}")
            }
            // 名前はクライアント自身が指定した識別子であり秘匿情報ではない
            // （既存の `UndefinedTable`／`WrongObjectType` と同じ扱い）。
            // `CREATE TABLE`（TASK-85）・`CREATE VIEW`（TASK-205、Issue #909）
            // 双方の名前衝突を共有する分類のため "relation" と汎称する。
            SqlSurfaceError::DuplicateTable { name } => {
                write!(f, "relation already exists: {name}")
            }
            SqlSurfaceError::DuplicateColumn { name } => {
                write!(f, "duplicate column name: {name}")
            }
            // テーブル名・ユーザー名を含めない固定文言（security.md P0。
            // `SqlSurfaceError::InsufficientPrivilege` ドキュメント参照）。
            SqlSurfaceError::InsufficientPrivilege => {
                write!(f, "permission denied for DDL statement")
            }
            // カーソル名・他セッション所有かどうかを一切含めない固定文言
            // （security.md P0。`SqlSurfaceError::InvalidCursorName` ドキュメント
            // 参照）。
            SqlSurfaceError::InvalidCursorName => {
                write!(f, "cursor does not exist")
            }
            // 依存元の名前一覧は含めない固定文言（security.md P0）。
            SqlSurfaceError::DependentObjectsStillExist { name } => {
                write!(f, "cannot drop {name} because other objects depend on it")
            }
            SqlSurfaceError::WrongObjectType { name } => {
                write!(f, "wrong object type: {name}")
            }
            // 値そのもの・他テナントの情報を含めない固定形式（security.md P0）。
            SqlSurfaceError::NotNullViolation { column } => {
                write!(
                    f,
                    "null value in column {column:?} violates not-null constraint"
                )
            }
            // 行キー衝突（`IdConflict`）とは別の固定文言（`TenantWriteError::
            // UniqueViolation` の `Display` と同じ考え方）。キー値・行 id・
            // テナント名は含めない（security.md P0）。
            SqlSurfaceError::UniqueViolation => write!(f, "unique constraint violation"),
            // 名前はクライアント自身が指定した識別子（`UndefinedTable` と同じ扱い）。
            SqlSurfaceError::UndefinedObject { name } => {
                write!(f, "index does not exist: {name}")
            }
            SqlSurfaceError::UndefinedColumn { name } => {
                write!(f, "column does not exist: {name}")
            }
            // 制約名のみを含む固定文言（行の値・id・テナントは含めない。
            // security.md P0。`TenantWriteError::CheckViolation` と同じ文言）。
            SqlSurfaceError::CheckViolation { constraint } => {
                write!(f, "new row violates check constraint {constraint:?}")
            }
            // 固定文言（`TenantWriteError::ForeignKeyViolation` と同じ。値・参照先の
            // 有無の理由を含めない。RLS-9・RLS-10 (c)）。
            SqlSurfaceError::ForeignKeyViolation => {
                write!(f, "foreign key constraint violation")
            }
            SqlSurfaceError::InvalidForeignKey { detail } => {
                write!(f, "invalid foreign key declaration: {detail}")
            }
            SqlSurfaceError::AmbiguousColumn { name } => {
                write!(f, "column reference {name:?} is ambiguous")
            }
        }
    }
}

impl std::error::Error for SqlSurfaceError {}

impl From<LexError> for SqlSurfaceError {
    fn from(e: LexError) -> Self {
        SqlSurfaceError::unsupported(format!("{} (near byte {})", e.message, e.byte_offset))
    }
}

/// FROM に指定したテーブルがスキーマカタログに実在するかを確認するための抽象。
/// `catalog.rs::Storage` に対して実装し（`impl TableLookup for Storage`）、
/// allowlist の単体テストを実 `redb` ストレージ非依存で書けるようにする軽量な境界。
pub trait TableLookup {
    /// `name` が定義済みテーブルなら `Ok(true)`、未定義なら `Ok(false)`。
    /// カタログ照会自体が失敗した場合（redb I/O 等）は `Err` とし、
    /// 存在するとも存在しないとも判定しない（fail-closed）。
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError>;

    /// `name` が定義済みビュー（`CREATE VIEW`。TABLE-18・SQL-23・TASK-205、
    /// Issue #909）なら `Ok(Some(_))`、テーブル・ビューいずれでもなければ
    /// `Ok(None)`。既定実装は常に `Ok(None)` を返すため、ビュー機能を持たない
    /// 既存の `TableLookup` 実装（テスト用モック等）は無変更のままコンパイル
    /// できる（`catalog.rs::impl TableLookup for Storage` のみが実データを
    /// 返す）。カタログ照会自体が失敗した場合（redb I/O・カタログ破損等）は
    /// `Err`（fail-closed。[`crate::sql::view::resolve_from`] 参照）。
    fn view_definition(
        &self,
        name: &str,
    ) -> Result<Option<crate::catalog::ViewDef>, SqlSurfaceError> {
        let _ = name;
        Ok(None)
    }

    /// `name` が実テーブルであれば、束縛時（`sql::parser::bind_projection`）に
    /// 許可される投影列名の集合（実カラム名 ＋ `id` 疑似列。スキーマが実カラム
    /// `id` を宣言していれば重複させない）を返す。テーブルが存在しない場合・
    /// 呼び出し側がこの照会に対応していない場合は `Ok(None)`（「列集合が
    /// 分からないため、ここでは検査しない」を意味する。既定実装。`table_exists`
    /// 自体の存在確認とは独立）。
    ///
    /// 参照される CTE・VIEW は最終的に `sql::parser::bind` が実テーブル
    /// スキーマに対して列存在を検査するため、このメソッドが `None` を返しても
    /// fail-open にはならない。唯一の例外はどこからも参照されない leaf CTE
    /// （`sql::allowlist::validate_sql_tokens` の WITH 事前検証ループ）で、
    /// これは bind に到達しないため、実テーブル直下の場合はこのメソッドの
    /// 戻り値で列存在を検査する（Issue #928 レビュー指摘: Codex P1、
    /// PR #1100 追加指摘）。既定実装が `Ok(None)` を返す既存の `TableLookup`
    /// 実装（テスト用モック等）は、この場合に限り列存在検査を省略したまま
    /// 動作し続ける（無変更でコンパイル・実行可能。`view_definition` の
    /// 既定実装と同じ後方互換の方針）。
    fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        let _ = name;
        Ok(None)
    }
}

/// ORDER BY 関数呼び出し形（`FunctionCall`, TASK-75）の 1 引数。本モジュールは
/// トークン種別（識別子／文字列リテラル）のみを構造として保持し、列名としての
/// 妥当性・リテラルの意味論的解釈は `sql::parser::bind`（TASK-75）の責務とする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionArg {
    Ident(String),
    StringLiteral(String),
}

/// `ORDER BY` 式の許可形状。TASK-74・SQL-8 参照（docs/spec/05-tasks.md）。
/// TASK-75 でリテラル値・関数引数を保持するよう拡張した（構造判定だけでなく、
/// 後続の束縛（`sql::parser::bind`）がベクトルリテラル解析・hybrid 引数解釈に使う）。
///
/// **TASK-77（SQL-5）で追加した破壊的変更（BREAKING CHANGE、codex-review P1 指摘対応、
/// PR #266）**: `UsingPlan` variant を追加した。本 enum は `#[non_exhaustive]` を
/// 付けていない公開型のため、この型に対して網羅的 `match` を書いている下流コードは
/// 本バージョンで追加された variant に対応するまでコンパイルが通らなくなる
/// （[`WherePredicate`] の `Expression`（TASK-79）・`Prefix`（TASK-147）追加時と同じ
/// 既存の破壊的変更運用に倣う）。移行方針: 既存の網羅的 `match` に `UsingPlan` の腕
/// （`USING PLAN` 文には意味を持つフィールドが無く、通常到達しない防御的経路として
/// 扱ってよい）を追加する。spec 側の定義変更は不要（TASK-77・SQL-5 のスコープ内の
/// 追加であり、`docs/spec/05-tasks.md` の対応タスクに包含される）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderByForm {
    /// 距離演算子形（`<列> <=> '<ベクトルリテラル>'`）。
    Distance { column: String, literal: String },
    /// 関数呼び出し形。引数トークン列は構造（括弧の対応・許可トークン種別のみ）を
    /// 保持し、個数・意味の解釈は `sql::parser::bind` が行う（TASK-75 時点で
    /// `hybrid_rrf`/`HYBRID` は 2 引数形・4 引数形の両方を構造上受理するが、
    /// 実行可能（束縛成功）なのは 4 引数形のみ。2 引数形は構造は受理しつつ束縛時に
    /// `SqlSurfaceError::InvalidInput`（`22000`）で拒否する。既存 2 引数形の
    /// マーシャリング・許可リスト受理そのものは変更しない）。
    FunctionCall {
        name: String,
        args: Vec<FunctionArg>,
    },
    /// `USING PLAN('<query>')`（TASK-77・SQL-5）が選ばれた文のプレースホルダ。
    /// `ORDER BY` 節自体が構文上存在しない（`USING PLAN` は `ORDER BY` と相互排他）
    /// ため意味を持つフィールドを持たず、ランキングは
    /// [`ValidatedStatement::using_plan`] から `sql::using_plan` が独立に導出する。
    /// `sql::parser::bind_ranking` はこの variant に到達すると内部エラーで拒否する
    /// （到達は `core.rs` の分岐が壊れた場合のみの防御的経路）。
    UsingPlan,
}

/// WHERE 句の許可形状。名前を照合する述語呼び出し形は、許可された名前
/// （[`is_allowed_where_predicate_name`]）のみを通過させる。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Expression`
/// variant を追加した（宣言的 UDF・組み込み関数呼び出しを含む比較式
/// `<expr> <cmp> <expr>`。式の意味論検証は `sql::parser::bind_in_session` の責務）。
///
/// **TASK-147（EXT-3）で追加した破壊的変更（BREAKING CHANGE）**: `Prefix` variant
/// を追加した（`<col> LIKE '<pattern>'` の LIKE 条件。網羅的 `match` を持つ
/// 外部コードは要対応）。パターン文字列は無加工で保持し、意味論的な検証・
/// 振り分け（等価・前方一致・SQL-24・TASK-208・Issue #914 で追加した中間一致・
/// 後方一致・`_` を含む一般形）は `declarative_filter::DeclarativeFilter::like`
/// （内部で `parse_like_pattern` を呼ぶ。`sql::parser::bind_in_session` から
/// 呼ばれる）の責務とする。`ESCAPE` 句は本構文層で受理しない（後続トークンが
/// 境界と一致せず `42601` になる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WherePredicate {
    /// 列と文字列リテラルの等価条件（TASK-75: リテラル値を保持する）。
    Equality { column: String, value: String },
    /// `LIKE` 条件（TASK-147・EXT-3。SQL-24・TASK-208・Issue #914 で前方一致
    /// 限定から中間一致・後方一致・`_` を含む一般形へ拡張）。`pattern` は
    /// 生パターン（無加工）を保持する。名前は互換性のため `Prefix` のまま
    /// 据え置く。`LIKE` は [`Keyword`] へ追加せず、TASK-80 と同じ
    /// 「`Token::Ident` をパーサー位置でのみ文脈照合」方式にして `like` という
    /// 列名を壊さない（[`Parser::parse_where`] 参照）。
    Prefix { column: String, pattern: String },
    /// 許可された名前の述語呼び出し形（空引数）。
    PredicateCall { name: String },
    /// 式の比較述語（TASK-79・SQL-9）。`Expr::Binary` の比較演算子（`> < >= <= =`）
    /// を頂点に持つ木のみを許可する（`parse_where` が構造的に保証する）。
    Expression(Expr),
    /// BOOLEAN 列の明示等価条件（`<col> = true|false`。TABLE-13・TASK-196、
    /// Issue #883・D-c）。
    BoolEquality { column: String, value: bool },
    /// BOOLEAN 列の裸参照（`WHERE flag`。`value = true` と同義。同 Issue）。
    BoolColumn { column: String },
    /// 列と文字列リテラルの範囲比較条件（`< > <= >=`。TABLE-13・TASK-199、
    /// Issue #891）。`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列向けの
    /// 宣言的経路（レーン B）で束縛する。`=` は既存の [`WherePredicate::Equality`]
    /// のまま据え置き、逆向き（`'2024-01-01' < d`）は受理しない（構文段で
    /// 式フォールバックへ回り `42601` になる。既知の制約）。
    Compare {
        column: String,
        op: CompareOp,
        value: String,
    },
    /// `OR` で結ぶ分岐の集合（TASK-208・SQL-24、Issue #912）。各分岐は
    /// `Vec<WherePredicate>`（`AND` で結ぶ述語列。分岐の中にさらに `Or` を
    /// 含めてよい＝ネスト可）で、`Or` は分岐が 2 個以上あるときのみ生成される
    /// （分岐 1 個・`AND` だけの括弧グループは呼び出し元が親の列へ平坦化する。
    /// [`Parser::parse_where_or`] 参照）。この構造により、`AND` だけの文は
    /// 本 variant 追加前と完全に同じ AST になる（content hash 不変）。
    ///
    /// **BREAKING CHANGE**: 本 variant の追加は非網羅的 `match` を破壊する
    /// （[`WherePredicate::Expression`]・[`WherePredicate::Prefix`] 追加時と同じ
    /// 既存の破壊的変更運用）。
    Or(Vec<Vec<WherePredicate>>),
    /// `<列> IN (SELECT ...)`（Issue #927・SQL-29 (a)・TASK-213）。内側の生
    /// トークン列（開き括弧の次〜対応する閉じ括弧の前）のみを保持し、構文解析
    /// 段では一切評価・再帰解析しない。`sql::subquery::resolve_where_predicates`
    /// が束縛（`sql::parser::bind_where_predicates`）の**前**に、外側と同じ
    /// `PolicyContext`（RLS 暗黙適用）で内側を実行し、具体的な
    /// [`WherePredicate::Or`]／[`WherePredicate::Equality`] 等へ書き換える
    /// （第 2 の評価器を作らない設計。CLAUDE.md「委譲方針」）。`depth` は
    /// このサブクエリ自身のネスト深さ（最外側 SELECT が 1）で、
    /// [`MAX_SUBQUERY_DEPTH`] を超える構文は構文解析段で `54000` になるため、
    /// ここへ到達する時点で既に上限内であることが不変条件。
    ///
    /// 構文的に受理するのは [`Parser::subquery_ctx`] が `Some` の文脈（読み取り
    /// SELECT の WHERE。`sql::allowlist::validate_sql_tokens` のトップレベル
    /// 呼び出しのみが設定する）のみで、CHECK 本体・`CREATE VIEW` 本体・述語形
    /// `UPDATE`/`DELETE`・カーソル・`COPY`・`EXPLAIN` からは構文解析段で `42601`
    /// になる。`NOT IN` は対象外（`42601`。#913 の `NOT` 対応後の課題）。
    ///
    /// **BREAKING CHANGE**: 本 variant の追加は非網羅的 `match` を破壊する
    /// （既存の破壊的変更運用を踏襲）。
    InSubquery {
        column: String,
        inner_tokens: Vec<Token>,
        depth: usize,
    },
    /// `EXISTS (SELECT ...)`（Issue #927・SQL-29 (a)・TASK-213）。意味論・解決
    /// 契約は [`WherePredicate::InSubquery`] と同じ（`sql::subquery` が束縛前に
    /// 解決する）。`EXISTS` は [`Keyword`] へ追加せず、`LIKE`・`OR` と同じ
    /// 「`Token::Ident` をパーサー位置でのみ文脈照合」方式にする（`exists` という
    /// 列名を壊さない）。`NOT EXISTS` は対象外（`42601`。同上）。
    ///
    /// **BREAKING CHANGE**: 本 variant の追加は非網羅的 `match` を破壊する
    /// （既存の破壊的変更運用を踏襲）。
    Exists {
        inner_tokens: Vec<Token>,
        depth: usize,
    },
}

/// [`WherePredicate::Compare`] の比較演算子（TABLE-13・TASK-199、Issue #891）。
/// `crate::declarative_filter::CompareOp` と 1 対 1 に対応する（字句表現から
/// 意味表現への写像を分離するための構文層専用の複製）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Lt,
    Le,
    Gt,
    Ge,
}

/// SELECT リストの 1 項目（TASK-79・SQL-9 で式項目を追加する際の共通表現）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    /// 既存の単純な列名項目（`*` 展開・裸の列名）。
    Column(String),
    /// 関数呼び出しを頂点に持つ式項目（TASK-79・SQL-9: 宣言的 UDF・組み込み関数の
    /// 結果列位置での呼び出し）。`alias` 省略時の列名は `sql::parser` が関数名から
    /// 導出する。
    Expr { expr: Expr, alias: Option<String> },
}

/// SELECT リストの許可形状（TASK-75）。`*`・単純な列名リストに加え、TASK-79（SQL-9）
/// で式項目（少なくとも 1 項目が関数呼び出しを含む形）を [`Items`] として追加した。
/// 全項目が単純な列名の場合は従来どおり [`Columns`] のまま（後方互換）。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Items` variant を
/// 追加した。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    All,
    Columns(Vec<String>),
    Items(Vec<SelectItem>),
}

/// 許可形状の構造判定を通過した SQL 文（後続タスクのパーサー・実行計画の土台）。
/// 本モジュールが保証するのはここまでの構造情報のみで、列名・リテラル値の意味論的な
/// 妥当性は検証しない（`sql::parser::bind` の責務）。
///
/// **TASK-161 で意図的に非公開化した破壊的変更（BREAKING CHANGE）**: 全フィールドを
/// `pub` から `pub(crate)` へ変更し `#[non_exhaustive]` を付与した。クレート外からの
/// 直接のフィールド参照・構造体リテラル構築は今後不可能。構築は
/// [`ValidatedStatement::new`]／[`ValidatedStatement::with_search_mode`]、読み取りは
/// [`ValidatedStatement::table_name`] 等の各アクセサーメソッドを使う（詳細は PR #188 の
/// Breaking Changes 節を参照。TASK-164 拡張点の前方互換確保とカプセル化のため）。
///
/// `#[non_exhaustive]`: TASK-161（SQL-12）で `search_mode` フィールドを追加した際、
/// 既存の構造体リテラル構築コードが必須フィールド不足でコンパイル不能になる破壊的
/// 変更となった（AGENTS.md「公開 API・エラー契約の互換性（P1）」）。今後のフィールド
/// 追加が同様の破壊を再発させないよう、外部クレートからの構造体リテラル構築を非対応
/// にする。フィールドはカプセル化のため `pub(crate)` とし（クレート外からの直読み・
/// 直書きは不可。コード内では [`ValidatedStatement::table_name`] 等のアクセサー
/// メソッドを経由する）、クレート外からの構築は [`ValidatedStatement::new`]（既存
/// フィールド相当の引数を取る）と [`ValidatedStatement::with_search_mode`]（TASK-161
/// で追加した `search_mode` を設定するビルダー的メソッド）を経由する。本構造体は
/// 通常 [`validate_sql`] の戻り値として取得するが、上記 constructor 経由でも構築
/// できる（PR #188 レビュー指摘対応: 破壊的変更の移行経路を用意しつつ、直接の
/// フィールド読み書きは許可しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ValidatedStatement {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    pub(crate) projection: Projection,
    pub(crate) order_by: OrderByForm,
    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub(crate) where_predicates: Vec<WherePredicate>,
    pub(crate) limit: u32,
    /// `LIMIT` 直後の文末専用句 `USING MODE '<literal>'`（TASK-161・SQL-12）の生
    /// リテラル値。省略時は `None`。値の意味論的妥当性（`recall`／`precision` の
    /// 2 値のみ有効）は本モジュールの管轄外で、`sql::mode::SearchMode::parse_literal`
    /// を経由する `sql::parser::bind_with_session` が検証する。
    pub(crate) search_mode: Option<String>,
    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。未指定時は
    /// [`EvaluationOrder::DEFAULT`]（既存 TASK-75 の固定順 RLS→SCALAR→DISTANCE）。
    pub(crate) evaluation_order: EvaluationOrder,
    /// `USING PLAN('<query>')`（TASK-77・SQL-5）で指定された自然言語クエリの生
    /// リテラル値。省略時は `None`。`ORDER BY` と相互排他（`Some` のとき
    /// `order_by` は必ず [`OrderByForm::UsingPlan`]）。値の展開（LLM クエリ
    /// プランニング、TASK-110・PLAN-1）・ハイブリッド実行形への束縛は
    /// `sql::using_plan` の管轄で、本モジュールは構文構造の受理までを担う。
    pub(crate) using_plan: Option<String>,
}

impl ValidatedStatement {
    /// クレート外から構築するための constructor（TASK-161 で `search_mode`
    /// フィールドを追加する以前の既存フィールド相当の引数を取る）。`search_mode`
    /// は未指定（`None`）で構築され、必要なら [`Self::with_search_mode`] を続けて
    /// 呼ぶ。フィールドが `pub(crate)` のため、クレート外から `ValidatedStatement`
    /// を得るにはこの constructor か [`validate_sql`] の戻り値を経由するしかない。
    pub fn new(
        table_name: String,
        projection: Projection,
        order_by: OrderByForm,
        where_predicates: Vec<WherePredicate>,
        limit: u32,
        evaluation_order: EvaluationOrder,
    ) -> Self {
        Self {
            table_name,
            projection,
            order_by,
            where_predicates,
            limit,
            search_mode: None,
            evaluation_order,
            using_plan: None,
        }
    }

    /// `search_mode`（TASK-161・SQL-12）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `search_mode` を含む値を外部から構築する。
    #[must_use]
    pub fn with_search_mode(mut self, search_mode: Option<String>) -> Self {
        self.search_mode = search_mode;
        self
    }

    /// `using_plan`（TASK-77・SQL-5）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `using_plan` を含む値を外部から構築する。
    ///
    /// **不変条件**（codex-review P1 指摘対応、PR #266）: `using_plan` に `Some`
    /// を渡した場合、`order_by` を無条件で [`OrderByForm::UsingPlan`] へ揃える。
    /// `USING PLAN` と `ORDER BY` は構文上相互排他であり、
    /// `core.rs::EngineCore::execute_sql_in_session` は `order_by` の値ではなく
    /// `using_plan()` の有無のみで束縛経路を分岐するため、この揃え込みが無いと
    /// 呼び出し元が [`Self::new`] へ渡した `order_by`（例:
    /// [`OrderByForm::Distance`]）が無言で無視され、意図せず `USING PLAN` 経路
    /// （呼び出し元が想定していないハイブリッド実行形）が実行される事故になり得た
    /// （公開 builder が矛盾した状態を構築できてしまう問題）。`None` を渡した
    /// 場合は `order_by` を変更しない（[`Self::new`] で渡された値をそのまま保つ）。
    /// 逆方向（`order_by` に [`OrderByForm::UsingPlan`] を渡しつつ `using_plan` を
    /// 設定しない）の矛盾は本メソッドだけでは防げないため、`execute_sql_in_session`
    /// 側でも分岐前に防御的に検証する（同メソッドのドキュメント参照）。
    #[must_use]
    pub fn with_using_plan(mut self, using_plan: Option<String>) -> Self {
        if using_plan.is_some() {
            self.order_by = OrderByForm::UsingPlan;
        }
        self.using_plan = using_plan;
        self
    }

    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// SELECT リストの許可形状。
    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// ORDER BY 句の許可形状。
    pub fn order_by(&self) -> &OrderByForm {
        &self.order_by
    }

    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// `LIMIT` 句の値。
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// `USING MODE '<literal>'`（TASK-161・SQL-12）の生リテラル値。未指定時は `None`。
    pub fn search_mode(&self) -> Option<&str> {
        self.search_mode.as_deref()
    }

    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。
    pub fn evaluation_order(&self) -> EvaluationOrder {
        self.evaluation_order
    }

    /// `USING PLAN('<query>')`（TASK-77・SQL-5）の生リテラル値。未指定時は `None`。
    pub fn using_plan(&self) -> Option<&str> {
        self.using_plan.as_deref()
    }
}

/// [`validate_sql`]（TASK-161 の公開 API）が返す statement 種別。`SELECT` 以外の
/// 文が増えても [`ValidatedStatement`] 自体は SELECT 専用の構造を保つため、
/// 統一的な enum で包む。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `CreateFunction`
/// variant を追加した。`Aggregate` variant が `GroupByClause`（TASK-167・SQL-14）
/// 経由で `f64`（HAVING リテラル）を保持するため `Eq` は導出しない
/// （`PartialEq` のみ。`Statement` の値比較はテストでのみ使う）。
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(ValidatedStatement),
    /// `SET search_mode = '<literal>'`（TASK-161・SQL-12）。カタログ照会を必要と
    /// しないためテーブル存在確認は行わない。リテラル値の意味論的妥当性検証は
    /// `core.rs::EngineCore::execute_sql_in_session` が `SearchMode::parse_literal`
    /// で行う（本モジュールは構造の受理までを担う）。
    SetSearchMode {
        value: String,
    },
    /// `CREATE FUNCTION <name>(<param>[, <param>...]) AS <expr>`（TASK-79・SQL-9）。
    /// カタログ照会を必要としない（セッションのみに影響するため FROM テーブルの
    /// 存在確認は行わない）。パラメータ名重複・登録済み名との衝突・列参照の禁止
    /// 等の意味論的妥当性検証は `sql::udf_call::define_function`（呼び出し元
    /// `core.rs::EngineCore::execute_sql_in_session`）が行う（本モジュールは構造の
    /// 受理までを担う）。
    CreateFunction {
        name: String,
        params: Vec<String>,
        body: Expr,
    },
    /// 集計関数のみを結果列とする `GROUP BY` なし・単一行結果の `SELECT`
    /// （TASK-166・SQL-13。C6a）。`FROM` 単一テーブルのカタログ存在確認を通過済み。
    Aggregate(ValidatedAggregate),
    /// `EXPLAIN <target>`（TASK-78・SQL-6、Issue #922・SQL-27）。対象文
    /// （[`ExplainTarget`]）は検索 SELECT（`USING PLAN` の有無いずれも）・集計
    /// （`GROUP BY`・`DISTINCT` の脱糖形いずれも）・広域取得のいずれかで、
    /// `FROM` 単一テーブルのカタログ存在確認を通過済み。`EXPLAIN` は検索本体・
    /// 集計走査・広域取得走査のいずれも実行しない（LLM クエリ展開・モード解決・
    /// 静的な走査方式判定を可視化する応答を構築するのみ。`core.rs::EngineCore::
    /// execute_sql_in_session` の管轄）。`SET`・`CREATE FUNCTION` への `EXPLAIN`
    /// 前置は許可リスト外として `42601` で拒否する。
    Explain(ExplainTarget),
    /// `SELECT <投影> FROM <table> [WHERE ...] LIMIT n`（`ORDER BY`・`USING PLAN`
    /// のいずれも伴わない、ソートなしのフィルタ取得。Issue #454。本 DB の
    /// 「正解を含むデータ群を広く返す」設計思想を SQL 表層で直接表現する経路で、
    /// ランキング段・取得モード（`recall`／`precision`）の適用対象を持たない。
    /// `FROM` 単一テーブルのカタログ存在確認を通過済み。契約の詳細は
    /// `docs/design/wide-retrieval-scan.md`（spec ビヘイビア ID は SQL-15・
    /// TASK-170 として付与済み〔vector-db-spec#12〕。確定化は TASK-170 が担う）参照。
    ///
    /// **本 variant の追加は破壊的変更（BREAKING CHANGE）**: 既存の網羅的
    /// `match` はワイルドカードアームの追加が必要（`Aggregate`・`Explain` 追加時と
    /// 同じ運用）。
    Scan(ValidatedScan),
}

/// `EXPLAIN` の対象文（Issue #922・SQL-27。TASK-78・SQL-6 の `USING PLAN` 付き
/// 検索 SELECT 限定から、通常検索・集計・広域取得へ対象を拡大した際に
/// [`Statement::Explain`] のペイロードへ導入した）。いずれの variant も
/// 検索本体・集計走査・広域取得走査を実行しない契約は共通で、`core.rs::
/// EngineCore::execute_sql_in_session` の `Statement::Explain` アームが
/// variant ごとに異なる静的分類・応答整形（[`crate::sql::explain`]）へ振り分ける。
///
/// **本 enum の追加および [`Statement::Explain`] のペイロード変更（`ValidatedStatement`
/// → `ExplainTarget`）は破壊的変更（BREAKING CHANGE）**: `Statement::Explain(v)` を
/// 分解して `ValidatedStatement` のメソッドを直接呼んでいた既存コードは
/// `ExplainTarget::Search(v)` へのパターンマッチを経由するよう修正が必要。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ExplainTarget {
    /// 検索 SELECT（`USING PLAN` の有無いずれも受理。TASK-78・SQL-6／Issue #922・
    /// SQL-27）。`using_plan()` が `Some` なら既存の `USING PLAN` 経路（LLM クエリ
    /// 展開・モード解決の可視化。行の形式は不変）、`None` なら `ORDER BY <=>`・
    /// `HYBRID` 検索の静的判定のみを可視化する新経路（`core.rs::EngineCore::
    /// run_search_explain`）を通る。
    Search(ValidatedStatement),
    /// 集計 SELECT（`GROUP BY` の有無・`SELECT DISTINCT` の脱糖形のいずれも。
    /// Issue #922・SQL-27）。
    Aggregate(ValidatedAggregate),
    /// 広域取得（ソートなしのフィルタ取得。ビュー展開後の形・`OFFSET` を含む。
    /// Issue #922・SQL-27）。
    Scan(ValidatedScan),
}

impl ExplainTarget {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        match self {
            ExplainTarget::Search(v) => v.table_name(),
            ExplainTarget::Aggregate(v) => v.table_name(),
            ExplainTarget::Scan(v) => v.table_name(),
        }
    }
}

/// 集計関数の種別（TASK-166・SQL-13）。関数名は [`is_aggregate_function_name`] で
/// 大文字小文字を区別せず照合済み。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunc {
    /// `name` が [`is_aggregate_function_name`] を通過済みの前提で呼ぶ。
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggregateFunc::Count),
            "SUM" => Some(AggregateFunc::Sum),
            "AVG" => Some(AggregateFunc::Avg),
            "MIN" => Some(AggregateFunc::Min),
            "MAX" => Some(AggregateFunc::Max),
            _ => None,
        }
    }

    /// `AS <alias>` を省略した場合の既定結果列名（関数名の小文字）。
    pub(crate) fn default_alias(self) -> &'static str {
        match self {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
        }
    }
}

/// 集計関数の引数（TASK-166・SQL-13）。`Star` は `COUNT(*)` 専用（[`Parser::parse_aggregate_item`]
/// が `COUNT` 以外での出現を構造的に拒否する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateArg {
    Star,
    Expr(Expr),
}

/// SELECT リストの集計項目 1 つ（TASK-166・SQL-13）。`alias` 省略時の列名は
/// [`AggregateFunc::default_alias`] を使う（`sql::parser::bind_aggregate` の責務）。
/// `distinct`（SQL-25 (c)・TASK-209）は `COUNT(DISTINCT <expr>)` の修飾子。
/// `COUNT` 以外の関数で `true` になることはない（[`Parser::parse_aggregate_item`]
/// が構造的に絞り込む）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateItem {
    pub(crate) func: AggregateFunc,
    pub(crate) arg: AggregateArg,
    pub(crate) alias: Option<String>,
    pub(crate) distinct: bool,
}

/// ウィンドウ関数（SQL-30・TASK-214、Issue #930）の種別。順位関数
/// （`RowNumber`/`Rank`/`DenseRank`）は引数を取らず、集計関数
/// （`Count`/`Sum`/`Avg`/`Min`/`Max`）は [`AggregateFunc`] と同じ名前を共有するが、
/// `OVER (...)` 句を伴う点・`GROUP BY` と併用できない点が異なるため独立の enum に
/// する（`allowlist::validate_sql_tokens` の振り分け・`sql::parser::bind_window_item`
/// が本 enum の variant で分岐する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl WindowFunc {
    /// ウィンドウ関数名の許可リスト照合（大文字小文字を区別しない）。未知の
    /// 名前は `None`（呼び出し元が `42601` へ変換する）。
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "ROW_NUMBER" => Some(WindowFunc::RowNumber),
            "RANK" => Some(WindowFunc::Rank),
            "DENSE_RANK" => Some(WindowFunc::DenseRank),
            "COUNT" => Some(WindowFunc::Count),
            "SUM" => Some(WindowFunc::Sum),
            "AVG" => Some(WindowFunc::Avg),
            "MIN" => Some(WindowFunc::Min),
            "MAX" => Some(WindowFunc::Max),
            _ => None,
        }
    }

    /// 順位関数（引数を取らない。[`Parser::parse_window_item`] が引数の有無を
    /// この判定で分岐する）。
    pub(crate) fn is_ranking(self) -> bool {
        matches!(
            self,
            WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank
        )
    }

    /// `alias` 省略時の既定列名（[`AggregateFunc::default_alias`] と同じ命名規則）。
    pub(crate) fn default_alias(self) -> &'static str {
        match self {
            WindowFunc::RowNumber => "row_number",
            WindowFunc::Rank => "rank",
            WindowFunc::DenseRank => "dense_rank",
            WindowFunc::Count => "count",
            WindowFunc::Sum => "sum",
            WindowFunc::Avg => "avg",
            WindowFunc::Min => "min",
            WindowFunc::Max => "max",
        }
    }
}

/// SELECT リストのウィンドウ項目 1 つ（SQL-30・TASK-214、Issue #930）。`position`
/// は SELECT リスト全体（通常項目・ウィンドウ項目を通した 0 始まりの通し番号）
/// における出現位置で、実行時（`sql::window::execute_window_scan`）が通常の
/// 投影列と組み合わせて元の並び順を復元するために使う
/// （[`Projection`] はウィンドウ項目を保持しないため、位置情報を別途持たせる
/// 必要がある）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowSelectItem {
    pub(crate) position: usize,
    pub(crate) func: WindowFunc,
    /// 順位関数は常に `None`。集計関数は [`AggregateArg`]（`*`・裸の列名のみ。
    /// 複合式は対象外——TASK-214 のスコープ縮小、`docs/design/window-functions.md`
    /// 参照）。
    pub(crate) arg: Option<AggregateArg>,
    /// `PARTITION BY` の列名一覧（順序保持。最大 [`MAX_WINDOW_KEYS`]）。
    pub(crate) partition_by: Vec<String>,
    /// `ORDER BY` の (列名, 降順か) 一覧（順序保持。最大 [`MAX_WINDOW_KEYS`]）。
    pub(crate) order_by: Vec<(String, bool)>,
    pub(crate) alias: Option<String>,
}

/// 集計 `SELECT` リストの 1 項目（TASK-167・SQL-14 で `AggregateItem` 単独から拡張）。
/// `GroupKey` は `GROUP BY` 句がある場合にのみ現れ、`GROUP BY` 列と同名の裸の
/// 識別子（任意で `AS <alias>`）だけを構造上受理する（`allowlist::Parser::parse_select_item`
/// ではなく [`Parser::parse_aggregate_select_item`] が列名一致を検査する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateSelectItem {
    Aggregate(AggregateItem),
    /// `column` は SELECT リストに書かれた識別子そのもの（構文解析時点では
    /// まだ `GROUP BY` 句を読んでいないため）。`GROUP BY` 句の列名と一致するかは
    /// [`parse_aggregate_shape`] が全体を読み終えた後に検査する。
    GroupKey {
        column: String,
        alias: Option<String>,
    },
}

/// `HAVING` 述語 1 つ（TASK-167・SQL-14）。左辺は SELECT リストに現れる集計項目の
/// 実効名（別名または既定名）への参照のみを許可し、右辺は数値リテラルに限定する
/// （集計関数呼び出し形の直接記述・列同士の比較・文字列リテラルはいずれも許可
/// リスト外）。意味論的な名前解決（存在確認・型検査）は
/// `sql::parser::bind_aggregate` の責務。
#[derive(Debug, Clone, PartialEq)]
pub struct HavingPredicate {
    pub(crate) item_name: String,
    pub(crate) op: BinOp,
    pub(crate) literal: f64,
}

/// `GROUP BY` 集計の `ORDER BY` 対象（TASK-167・SQL-14）。`GROUP BY` 列名、または
/// SELECT リストの集計項目の実効名のいずれかの識別子を指す（解決は
/// `sql::parser::bind_aggregate`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateOrderBy {
    pub(crate) target: String,
    pub(crate) descending: bool,
}

/// `GROUP BY <column> (',' <column>)* [HAVING ...] [ORDER BY ...] [LIMIT ...]`
/// （TASK-167・SQL-14。SQL-25 (d) で複数列へ拡張）の許可形状。`columns` は宣言順
/// を保持したカタログ照会前の識別子のまま保持し（`TEXT` 列限定・重複検査済み等の
/// 意味論的検査は束縛段）、`having`/`order_by`/`limit` はいずれも省略可能。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupByClause {
    pub(crate) columns: Vec<String>,
    pub(crate) having: Vec<HavingPredicate>,
    pub(crate) order_by: Option<AggregateOrderBy>,
    pub(crate) limit: Option<u32>,
    /// `OFFSET` の生値（Issue #916・SQL-25 (b)・TASK-209）。`limit` が `None` のとき
    /// `OFFSET` 単独は構文段（[`parse_aggregate_shape`]）で `42601` に落ちるため常に
    /// `0`。範囲検証は `sql::parser::bind_group_by_clause` が束縛時に行う。
    pub(crate) offset: u32,
}

/// 許可形状の構造判定を通過した集計 `SELECT` 文（TASK-166・SQL-13。TASK-167・
/// SQL-14 で `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT` を追加）。[`ValidatedStatement`]
/// と同様、本モジュールが保証するのはここまでの構造情報のみで、列名・式の
/// 意味論的妥当性は検証しない（`sql::parser::bind_aggregate` の責務）。
/// フィールドは `pub(crate)`（クレート外からの直読み・直書き不可。カプセル化の方針は
/// [`ValidatedStatement`] と同じ）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAggregate {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    /// SELECT リスト項目（順序保持。1..=[`MAX_AGGREGATE_ITEMS`]）。
    pub(crate) items: Vec<AggregateSelectItem>,
    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。既存の
    /// [`ValidatedStatement::where_predicates`] と同一の許可形状を再利用する。
    pub(crate) where_predicates: Vec<WherePredicate>,
    /// `GROUP BY` 句（TASK-167・SQL-14）。`None` なら TASK-166・SQL-13 の
    /// 単一行集計（既存の受理形）のまま。
    pub(crate) group_by: Option<GroupByClause>,
}

impl ValidatedAggregate {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// SELECT リスト項目（順序保持）。
    pub fn items(&self) -> &[AggregateSelectItem] {
        &self.items
    }

    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// `GROUP BY` 句（TASK-167・SQL-14）。`None` なら `GROUP BY` なしの単一行集計。
    pub fn group_by(&self) -> Option<&GroupByClause> {
        self.group_by.as_ref()
    }
}

/// 許可形状の構造判定を通過した広域取得（ソートなしのフィルタ取得）`SELECT` 文
/// （Issue #454）。[`ValidatedStatement`]・[`ValidatedAggregate`] と同様、本モジュール
/// が保証するのはここまでの構造情報のみで、列名・式の意味論的妥当性は検証しない
/// （`sql::parser::bind_scan` の責務）。ランキング段（`ORDER BY`・`USING PLAN`）・
/// 取得モード（`USING MODE`）・評価順（`HINT ORDER`）のいずれも持たない
/// （構文上 `LIMIT n` の直後は文末のみを許可する。§3.1「本リポの実装既定値」）。
/// フィールドは `pub(crate)`（クレート外からの直読み・直書き不可。カプセル化の方針は
/// [`ValidatedStatement`]・[`ValidatedAggregate`] と同じ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedScan {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    /// SELECT リストの許可形状。[`ValidatedStatement::projection`] と同一の意味論
    /// （`*`・列名リスト・式項目）を共有する。
    pub(crate) projection: Projection,
    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。既存の
    /// [`ValidatedStatement::where_predicates`] と同一の許可形状を再利用する。
    pub(crate) where_predicates: Vec<WherePredicate>,
    /// `LIMIT` 句の値。可視かつ WHERE を満たす行を先頭から最大この件数だけ返す
    /// （早期終了。順序保証はスナップショット内の物理走査順のみで、`ORDER BY`
    /// 相当の意味的順序は持たない）。
    pub(crate) limit: u32,
    /// `OFFSET` 句の生値（既定 0。Issue #916・SQL-25 (b)・TASK-209）。可視かつ
    /// WHERE を満たす行のうち先頭からこの件数だけ読み飛ばしてから `limit` を
    /// 適用する（`sql::scan::execute_scan_with_budget`）。範囲検証は
    /// `sql::parser::bind_scan_with_dummy_flags` が束縛時に行う。
    pub(crate) offset: u32,
    /// ウィンドウ項目（SQL-30・TASK-214、Issue #930）。空なら通常の広域取得
    /// （既存契約を完全に維持）。`Vec<SelectItem>` を返す `projection` とは別に
    /// 保持し、実行時（`sql::window::execute_window_scan`）が
    /// [`WindowSelectItem::position`] で元の並び順へ合流する。
    pub(crate) window_items: Vec<WindowSelectItem>,
}

impl ValidatedScan {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// SELECT リストの許可形状。
    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// `LIMIT` 句の値（構造検証済みの生値。範囲検証は `sql::parser::validate_search_limit`
    /// が束縛時に行う）。
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// `OFFSET` 句の値（構造検証済みの生値。既定 0。範囲検証は
    /// `sql::parser::validate_search_offset` が束縛時に行う）。
    pub fn offset(&self) -> u32 {
        self.offset
    }
}

/// `CREATE TABLE` の列制約（`NOT NULL`／`DEFAULT <literal>`〔Issue #904〕・
/// `UNIQUE`〔Issue #905〕。TABLE-16・TASK-204）の構文木。列型ごとの適用可否
/// （`DEFAULT` の型整合・`VECTOR` への `DEFAULT`／`UNIQUE` 禁止等）は呼び出し元
/// （`Parser::parse_create_table_column`）が判定する。
struct ColumnConstraints {
    not_null: bool,
    default: Option<InsertLiteral>,
    unique: bool,
}

/// `CREATE TABLE` の列 1 個ぶんの構文解析結果（[`Parser::parse_create_table_column`]
/// の戻り値）。列制約 `PRIMARY KEY`・`UNIQUE` の有無は列定義本体と分けて返し、
/// 呼び出し元（[`Parser::parse_create_table`]）が表制約と同じ集約経路へ流す。
struct ParsedCreateTableColumn {
    column: ColumnDef,
    primary_key: bool,
    unique: bool,
    /// 列定義の後ろに続く列制約 `[CONSTRAINT <name>] CHECK (...)`（0 個以上。
    /// TABLE-16・TASK-204、Issue #906）。
    checks: Vec<ParsedCheck>,
    /// 列制約 `REFERENCES <table> [(<col>[, <col>]*)]`（TABLE-17・TASK-205、
    /// Issue #907）の参照先テーブル名と参照先列名（省略時は空）。
    references: Option<(String, Vec<String>)>,
}

/// INSERT の VALUES リストの 1 リテラル（SQL-10、TASK-80）。トークン種別
/// （文字列リテラル／数値）のみを構造として保持し、列型との照合・意味論的解釈は
/// `sql::parser::bind_insert` の責務とする。UPDATE の SET 句のリテラル値表現としても
/// 共有する（SQL-17、TASK-191。`sql::parser::bind_update` の責務）。
#[derive(Debug, Clone, PartialEq)]
pub enum InsertLiteral {
    String(String),
    Number(String),
    /// BOOLEAN 列向けの `true`/`false` リテラル（TABLE-13・TASK-196、Issue #883）。
    Bool(bool),
    /// SQL `NULL`（明示的な NULL 指定。Issue #889 レビュー指摘・PR #1014）。
    /// SQL の `UPDATE ... SET` 構文には現状 `NULL` リテラルの字句・構文規則が
    /// 無く（`sql::allowlist` の `SET` 句パーサーは `NULL` トークンを生成
    /// しない）、SQL テキストの `INSERT ... VALUES` 構文からも構築されない
    /// （VALUES 要素パーサーが `NULL` トークンを受理しない）。本 variant は
    /// NoSQL 表層 `update`／`insert` op（`wire-server::http::query::update::
    /// map_set_assignments`・`insert::bind_row`。TABLE-16・TASK-204、
    /// Issue #904 D5）が JSON `null` から構築する。
    ///
    /// 明示的な `NULL` には `DEFAULT` を適用しない契約（TABLE-16）のため、
    /// 「省略」（`DEFAULT` 適用対象。`sql::parser::fill_omitted_columns`）とは
    /// 独立の経路として扱う。`bind_insert_row`・`bind_set_assignments`
    /// （UPDATE）・`bind_upsert_assignments`（UPSERT `DO UPDATE SET`）・
    /// `bind_file_insert` はいずれも `column.nullable` に応じて
    /// `Value::Null`（nullable）／`SqlSurfaceError::NotNullViolation`
    /// （非 nullable。`23502`）へ写像する（列型を問わない一律拒否ではない）。
    Null,
    /// `VECTOR` 列向けの、既に要素ごとに検証済みの `f32` 列（NoSQL 表層
    /// `insert`／`update` op が JSON 配列から直接構築する。Issue #896
    /// レビュー指摘〔PR #1038〕対応）。SQL テキスト・COPY・ファイル形
    /// `INSERT` はいずれも `VECTOR` 列を `InsertLiteral::String`（`[f1,f2,...]`
    /// 形のテキストリテラル）として構築するため到達しないが、`sql::parser`
    /// の `(ColumnType::Vector(dim), ...)` 束縛は本 variant も明示的に
    /// 受理し、`InsertLiteral::String` 経由の [`crate::sql::parser::
    /// parse_vector_literal`]（64 KiB のテキスト長上限）を経由せずに次元・
    /// 有限性のみを検証してから [`crate::row_codec::Value::Vector`] へ束縛
    /// する（テキスト長上限は SQL リテラルの構文制約であり、JSON 配列から
    /// 直接届く既に解析済みの数値列には適用対象が無い設計判断。
    /// `docs/design/nosql-typed-json-binding.md` 参照）。
    Vector(Vec<f32>),
}

/// `ON CONFLICT (id) DO UPDATE SET <col> = <value>` の SET 右辺（SQL-20・
/// TASK-193、Issue #872）。`EXCLUDED.<col>`（新規挿入しようとした行の束縛済み
/// 値。列名は `sql::parser::bind_upsert_assignments` が `id`/`tenant_id`/
/// `visibility` を含む禁止列・未知列・型不一致を検証する）か、`UPDATE ... SET`
/// と同じリテラル値のいずれか。式・関数呼び出し・他列参照は許可リスト外。
#[derive(Debug, Clone, PartialEq)]
pub enum UpsertValue {
    /// `EXCLUDED.<col>`（大小無視で照合した `EXCLUDED` 修飾子。列名は宣言どおりの
    /// 大小を保持する）。
    Excluded(String),
    Literal(InsertLiteral),
}

/// `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...` の衝突分岐（SQL-20・
/// TASK-193、Issue #872）。衝突判定スコープは `(tenant_id, id)` の**所有**
/// （`tenant::upsert_typed_rows_unchecked` が既存 DML 実行器
/// `update_row_unchecked`／`delete_row_impl` と同じ二重防御で判定する。
/// 可視性ではない）。`ValidatedInsert::on_conflict` が `None` の場合は本 Issue
/// 導入前と完全に同じ「行 `id` 衝突は常に `23505`」の挙動になる。
#[derive(Debug, Clone, PartialEq)]
pub enum OnConflictAction {
    /// 衝突した行はそのまま変更しない（新規挿入もしない）。
    DoNothing,
    /// 衝突した行の指定列だけを上書きする（宣言順を保持。read-merge-write）。
    DoUpdate(Vec<(String, UpsertValue)>),
}

/// 許可形状の構造判定を通過した INSERT 文（SQL-10・SQL-16、TASK-80・TASK-190）。
/// `ValidatedStatement` と同様、本モジュールが保証するのはここまでの構造情報のみで、
/// 列名・値の意味論的妥当性は検証しない（`sql::parser::bind_insert` の責務）。
///
/// 受理する形は `INSERT INTO <table> (<col>[, <col>]*)
/// VALUES (<lit>[, <lit>]*)[, (<lit>[, <lit>]*)]*
/// USING OPERATION_ID '<id>' [;]`（複数行 `VALUES` を許容する。SQL-16、TASK-190）。
/// 各行のリテラル数は列数と一致することを行ごとに検証済み（`rows.len() >= 1`）。
/// `USING OPERATION_ID` 句の直前に任意で `RETURNING <投影>` を置ける
/// （Issue #873・SQL-21。`returning` フィールド参照）。可視性ラベル指定は
/// 引き続き許可リスト外。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedInsert {
    /// INTO に指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    /// 列リストの宣言順（`columns[i]` は `rows[r][i]` に対応する。個数は
    /// `parse_insert` が行ごとに一致を確認済み）。
    pub columns: Vec<String>,
    /// `VALUES` 句の行の宣言順（`rows.len() >= 1`。単一行形も `rows.len() == 1`
    /// として同じ形で保持する。SQL-16、TASK-190）。
    pub rows: Vec<Vec<InsertLiteral>>,
    /// 文末専用句で搬送された、検証済みの `operation_id`（SQL-10）。句の欠落・明示
    /// `NULL` はいずれも `None`（TASK-92・RECOVER-1）。`validate_insert` は
    /// `LedgerMode::Ledgered`（既定）では `None` を書き込みトランザクション開始前に
    /// `23502` で拒否するため、この構成では常に `Some` になる。
    /// `LedgerMode::CompareOnlyWithoutLedger` では `None` を許す。
    pub operation_id: Option<OperationId>,
    /// `RETURNING` 句（Issue #873・SQL-21）。省略時は `None`。`USING OPERATION_ID`
    /// 句の直前にのみ置ける（[`Parser::parse_returning_clause`] 参照）。関数呼び出し
    /// 項目（[`Projection::Items`]）はここには到達しない（構造検証段で `42601`）。
    pub returning: Option<Projection>,
    /// `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`（SQL-20・TASK-193、
    /// Issue #872）。句の省略は `None`（本 Issue 導入前と完全に同じ「行 `id`
    /// 衝突は常に `23505`」の挙動）。複数行 `VALUES` と併用可能（全行が同じ
    /// 衝突分岐を共有する）。ファイル形 INSERT との併用は
    /// `sql::parser::bind_insert_form` が `42601` で拒否する。
    pub on_conflict: Option<OnConflictAction>,
}

/// 許可形状の構造判定を通過した単一行・`id` 指定形 `DELETE` 文（SQL-18・
/// TASK-191）。`ValidatedInsert` と同様、本モジュールが保証するのはここまでの
/// 構造情報のみで、`id` 値の意味論的妥当性（`u64` として解釈可能か）は
/// `sql::parser::bind_delete` の責務とする。
///
/// 受理する形は `DELETE FROM <table> WHERE id = <number>
/// USING OPERATION_ID '<id>' [;]` の単一行・`id` 等価指定形のみ（`id` 以外の
/// 列に対する述語・`AND` 結合・`WHERE` 省略は許可リスト外）。述語つき `DELETE`
/// （`id` 以外の列・`AND` 結合を伴う `WHERE`）は [`ValidatedPredicateDelete`]・
/// [`validate_delete_statement`] の管轄（Issue #870・TASK-192・SQL-19）。本型・
/// [`validate_delete`] 自体の受理範囲はそちらの追加後も一切変わらない。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedDelete {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    /// `WHERE id = <n>` の `<n>`（`Token::Number` の生文字列）。`u64` への
    /// 意味論的解釈は `sql::parser::bind_delete` の責務。
    pub id_literal: String,
    /// 文末専用句で搬送された、検証済みの `operation_id`。句の欠落・明示
    /// `NULL` はいずれも `None`（TASK-92・RECOVER-1）。`validate_delete` は
    /// `LedgerMode::Ledgered`（既定）では `None` を書き込みトランザクション
    /// 開始前に `23502` で拒否するため、この構成では常に `Some`。
    pub operation_id: Option<OperationId>,
    /// `RETURNING` 句（Issue #873・SQL-21）。単一行・`id` 完全一致形は実行結線
    /// 済み（[`crate::sql::exec::execute_delete_returning`]）のため受理する。
    /// 述語形（[`ValidatedPredicateDelete`]）はフィールドを持たず、構造検証段
    /// （`validate_delete_statement_tokens` の `Predicate` 腕）で常に `42601`。
    pub returning: Option<Projection>,
}

/// 許可形状の構造判定を通過した述語つき `DELETE ... WHERE` 文（Issue #870・
/// TASK-192・SQL-19）。単一行・`id` 完全一致形（[`ValidatedDelete`]）とは別の
/// 型として保持し、既存の `ValidatedDelete`／[`validate_delete`] の受理範囲・
/// 挙動を一切変えない（[`DeleteStatement`] が両者を束ねる）。
///
/// 受理する述語形状は `SELECT`／集計／広域取得（scan）が共有する
/// [`Parser::parse_where`]（等価・前方一致・`visible()`・式比較の `Vec<WherePredicate>`）
/// そのものであり、第 2 の述語実装を持たない（R1: `sql::parser::bind_predicate_delete`
/// が `sql::parser::bind_scan` と同じ [`sql::parser::bind_where_predicates`] を
/// 共有する）。`WHERE` 句自体の省略は許可リスト外（[`Parser::parse_delete`] が
/// `Keyword::Where` を必須とする）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedPredicateDelete {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    /// `WHERE` 句に含まれる述語（AND 結合の宣言順のまま保持し並べ替えない。
    /// RECOVER-11（#868 の担当）の内容照合ハッシュが「正規化した文」を入力と
    /// する際の情報源はこの宣言順そのもの。`ValidatedUpdate::assignments` と
    /// 同じ判断）。
    pub(crate) where_predicates: Vec<WherePredicate>,
    /// 文末専用句で搬送された、検証済みの `operation_id`。句の欠落・明示
    /// `NULL` はいずれも `None`（TASK-92・RECOVER-1）。[`validate_delete_statement`]
    /// は `LedgerMode::Ledgered`（既定）では `None` を書き込みトランザクション
    /// 開始前に `23502` で拒否するため、この構成では常に `Some`。
    pub(crate) operation_id: Option<OperationId>,
}

impl ValidatedPredicateDelete {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// `WHERE` 句に含まれる述語（AND 結合順・宣言順保持）。空にはならない
    /// （空述語列は先読みにより単一行形と誤認しない限り [`ValidatedDelete`] 側
    /// には落ちないが、`WHERE` 自体の省略は許可リスト外のため本型が構築される
    /// 時点で `parse_where` は必ず 1 件以上を返す）。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// 文末専用句で搬送された、検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }
}

/// [`validate_delete_statement`]（Issue #870・TASK-192・SQL-19 の公開 API）が
/// 返す `DELETE` statement 種別。単一行・`id` 完全一致形（既存 SQL-18・
/// [`ValidatedDelete`]）と述語形（[`ValidatedPredicateDelete`]）を束ねる。
/// 実行結線（可視行列挙・1 トランザクション一括適用・影響行数上限の実測判定・
/// 台帳照合）は #871 の担当（本 Issue の成果物は束縛済み実行計画までで、
/// `core.rs`・`sql/exec.rs` の実行経路は変更しない）。
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteStatement {
    SingleRow(ValidatedDelete),
    Predicate(ValidatedPredicateDelete),
}

/// 許可形状の構造判定を通過した `CREATE TABLE` 文（SQL-23・TASK-85、Issue #899）。
/// カタログ照会（既存テーブル名との衝突判定）は本モジュールの管轄外——
/// `sql::ddl::execute_create_table` が `catalog::Storage::create_table` の
/// 単一 write トランザクション内で行う（事前の `table_exists` 照会は TOCTOU を
/// 避けるため行わない。`sql::ddl` モジュールドキュメント参照）。
///
/// 受理する形は `CREATE TABLE <table> (<col> <type>[, <col> <type>]*) [;]` の
/// みで、`<type>` は `TEXT`／`VECTOR ( <N> )` のみ（`IF NOT EXISTS`・
/// `REFERENCES`・`USING OPERATION_ID` の付与はいずれも許可リスト外。
/// `CONSTRAINT <name>` は `CHECK` の前置にのみ受理する）。列制約・表制約
/// `[CONSTRAINT <name>] CHECK ( <述語> )`（TABLE-16・TASK-204、Issue #906。
/// `docs/design/sql-check-constraint.md` 参照）も受理する。列制約 `NOT NULL`／`DEFAULT <literal>`（Issue #904）・
/// `UNIQUE`（Issue #905）・`PRIMARY KEY`（Issue #903）と、表制約
/// `PRIMARY KEY (<col>[, <col>]*)`・`UNIQUE (<col>[, <col>]*)`（複合キーを含む）
/// を許可形状として追加受理する（TABLE-16・TASK-204。詳細は
/// `docs/design/sql-primary-key.md`・`docs/design/unique-constraint.md` 参照）。
/// `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）の列制約
/// `REFERENCES <table> [(<col>[, <col>]*)]`・表制約
/// `FOREIGN KEY (<col>[, <col>]*) REFERENCES <table> [(<col>[, <col>]*)]`
/// （いずれも後続に `ON DELETE`／`ON UPDATE` の `NO ACTION`／`RESTRICT` のみ可）と、
/// `id` 参照の参照元列に使う列型 `INTEGER`／`BIGINT` も受理する
/// （`docs/design/foreign-key.md` 参照）。`REFERENCES` を上記以外の文
/// （`ALTER TABLE ... ADD COLUMN` 等）に付与する形は許可リスト外のまま。
/// TABLE-13/14 のその他の追加型は別 Issue の管轄。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCreateTable {
    /// カタログ存在確認前のテーブル名（識別子形式のみ検証済み）。
    pub table_name: String,
    /// 宣言順を保持した列定義（1 件以上）。`VECTOR` 列は `nullable = false`・
    /// `TEXT` 列は `nullable = true`（PostgreSQL の既定に倣う。TABLE-1）。
    /// 主キー構成列（[`Self::primary_key`] が `Some` の場合）は列制約・表制約の
    /// いずれで宣言されても `nullable = false` へ書き換え済み。
    pub columns: Vec<ColumnDef>,
    /// `PRIMARY KEY` 宣言列名（宣言順。TABLE-16・TASK-204、Issue #903）。
    /// 未宣言（`id` 暗黙主キーのまま）は `None`。`PRIMARY KEY (id)`（`id` 単独）
    /// は暗黙主キーの明示宣言として受理したうえで `None` へ正規化する
    /// （`docs/design/sql-primary-key.md`「id 暗黙主キーとの共存規約」参照）。
    pub primary_key: Option<Vec<String>>,
    /// 列制約（`<col> TEXT UNIQUE`）・表制約（`UNIQUE (<col>[, <col>]*)`）の
    /// いずれかで宣言された UNIQUE 制約（宣言順。TABLE-16・TASK-204、
    /// Issue #905）。参照列の実在・型適格性は構造検証段階
    /// （[`finalize_unique_constraints`]）で判定済み。
    pub unique_constraints: Vec<crate::catalog::UniqueConstraint>,
    /// 宣言順の `CHECK` 制約（列制約・表制約のいずれも本フィールドへ集約する。
    /// TABLE-16・TASK-204、Issue #906）。構文段では述語の意味論的妥当性
    /// （列の存在・型・許可関数）を検証しない——`sql::ddl::execute_create_table`
    /// が `sql::check_constraint::validate_and_build` で検証する。
    pub checks: Vec<ParsedCheck>,
    /// 宣言順の `FOREIGN KEY` 制約（列制約・表制約のいずれも本フィールドへ集約する。
    /// TABLE-17・TASK-205、Issue #907）。参照元列の実在は構造検証段階
    /// （[`finalize_foreign_keys`]）で判定済み。参照先列を省略した宣言は
    /// `parent_columns()` が空のまま運ばれ、参照先の名前解決・主キー／UNIQUE
    /// 制約との照合とともに `catalog::Storage::create_table` の write トランザクション
    /// 内で解決される。
    pub foreign_keys: Vec<crate::catalog::ForeignKeyDef>,
}

/// [`Parser::parse_create_table`] が列リスト全体の構文判定を終えた後に呼ぶ、
/// `FOREIGN KEY` 制約の参照元列の解決（TABLE-17・TASK-205、Issue #907）。表制約は
/// 宣言順に関わらず任意位置の列を参照できるため、全列が出揃った後にまとめて行う。
/// 未宣言列（`id` 疑似列を含む）の参照は `42601`（UNIQUE の
/// [`finalize_unique_constraints`] と同じ分類）。列型の適格性・参照先との照合は
/// カタログ層（`catalog::validate_schema`・`Storage::create_table`）が `42830` として
/// 判定する。
fn finalize_foreign_keys(
    foreign_keys: Vec<crate::catalog::ForeignKeyDef>,
    columns: &[ColumnDef],
) -> Result<Vec<crate::catalog::ForeignKeyDef>, SqlSurfaceError> {
    for fk in &foreign_keys {
        for name in fk.columns() {
            if !columns.iter().any(|c| &c.name == name) {
                return Err(SqlSurfaceError::unsupported(format!(
                    "FOREIGN KEY references unknown column: {name}"
                )));
            }
        }
    }
    Ok(foreign_keys)
}

/// `CREATE TABLE` の `CHECK` 制約 1 件分の構文段中間表現（TABLE-16・TASK-204、
/// Issue #906）。`sql::check_constraint::validate_and_build` が意味論検証・
/// 制約名の確定（省略時の自動生成）・カタログ表現への変換を行う。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCheck {
    /// `CONSTRAINT <name>` で明示された制約名（省略時は `None`）。
    pub name: Option<String>,
    /// 列制約として宣言された場合のみ `Some(<列名>)`（表制約は `None`）。
    /// 制約名の自動生成（`<table>_<col>_check`）に使う。
    pub column: Option<String>,
    /// 括弧内の述語列（`AND` 連結。`WHERE` と同一文法）。
    pub predicates: Vec<WherePredicate>,
}

/// [`Parser::parse_create_table`] が列リスト全体の構文判定を終えた後に呼ぶ、
/// UNIQUE 制約の参照解決（TABLE-16・TASK-204、Issue #905）。表制約は宣言順に
/// 関わらず任意位置の列を参照できるため、全列が出揃った後にまとめて行う。
/// 未宣言列（`id` 疑似列を含む。`id` は元からテナント内一意の行識別子のため
/// UNIQUE の対象にしない）・一意キー許可型
/// （[`crate::catalog::ColumnType::is_primary_key_allowed`]。主キーと共有する
/// 単一の許可リスト）外の列の参照は `42601`。同一列リストの制約重複等の残る
/// 不変条件は `catalog::validate_schema` が再検証する。
fn finalize_unique_constraints(
    unique_constraints: Vec<Vec<String>>,
    columns: &[ColumnDef],
) -> Result<Vec<crate::catalog::UniqueConstraint>, SqlSurfaceError> {
    for cols in &unique_constraints {
        for name in cols {
            let column = columns.iter().find(|c| &c.name == name).ok_or_else(|| {
                SqlSurfaceError::unsupported(format!(
                    "UNIQUE constraint references unknown column: {name}"
                ))
            })?;
            if !column.ty.is_primary_key_allowed() {
                return Err(SqlSurfaceError::unsupported(format!(
                    "column {name} has a type that cannot be used in a UNIQUE constraint"
                )));
            }
        }
    }
    Ok(unique_constraints
        .into_iter()
        .map(crate::catalog::UniqueConstraint::new)
        .collect())
}

/// [`Parser::parse_create_table`] が列リスト全体の構文判定を終えた後に呼ぶ、
/// `PRIMARY KEY` 宣言の最終判定・正規化（TABLE-16・TASK-204、Issue #903）。
///
/// - `None`（未宣言）はそのまま `Ok(None)`。
/// - `id` 単独宣言（`PRIMARY KEY (id)`）は暗黙主キーの明示宣言として `Ok(None)`
///   （何も永続化しない。`docs/design/sql-primary-key.md` 参照）。
/// - `id` と他列の混在は曖昧さを避けるため `42601` で拒否する。
/// - それ以外の各列名は `columns`（生存列のみ。表制約は宣言順に関わらず任意
///   位置の列を参照できる）に存在しなければならず、存在すれば
///   `nullable = false` へ書き換える（列制約・表制約いずれの宣言経路でも
///   同じ最終状態にする）。列型自体の許可判定
///   （[`crate::catalog::ColumnType::is_primary_key_allowed`]）はここでは行わず
///   `catalog::validate_schema`（`sql::ddl::execute_create_table` が書き込み
///   トランザクション内で通す）に委ねる（同じ `42601` 分類のため二重実装
///   しない）。
fn finalize_primary_key(
    primary_key: Option<Vec<String>>,
    columns: &mut [ColumnDef],
) -> Result<Option<Vec<String>>, SqlSurfaceError> {
    let Some(pk_cols) = primary_key else {
        return Ok(None);
    };
    if pk_cols.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "PRIMARY KEY must declare at least one column",
        ));
    }
    let contains_id = pk_cols.iter().any(|name| name.eq_ignore_ascii_case("id"));
    if contains_id {
        if pk_cols.len() > 1 {
            return Err(SqlSurfaceError::unsupported(
                "PRIMARY KEY must not combine the implicit id column with other columns",
            ));
        }
        return Ok(None);
    }
    for name in &pk_cols {
        let column = columns
            .iter_mut()
            .find(|c| &c.name == name)
            .ok_or_else(|| {
                SqlSurfaceError::unsupported(format!(
                    "PRIMARY KEY references unknown column: {name}"
                ))
            })?;
        column.nullable = false;
    }
    Ok(Some(pk_cols))
}

/// 許可形状の構造判定を通過した TRUNCATE 文（SQL-22、TASK-195）。テーブル定義
/// （カタログ）は残したまま、セッションのテナントが所有する全行を削除する
/// 書き込み系操作（DDL ではない）として扱う。
///
/// 受理する形は `TRUNCATE TABLE <table> USING OPERATION_ID '<id>' [;]` のみ
/// （複数テーブル指定・`CASCADE`/`RESTART IDENTITY` 等の PostgreSQL 拡張句は
/// 許可リスト外）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedTruncate {
    /// TABLE に指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    /// 文末専用句で搬送された、検証済みの `operation_id`（SQL-22）。句の欠落・明示
    /// `NULL` はいずれも `None`（`ValidatedInsert::operation_id` と同じ契約）。
    /// `validate_truncate` は `LedgerMode::Ledgered`（既定）では `None` を
    /// 書き込みトランザクション開始前に `23502` で拒否するため、この構成では
    /// 常に `Some` になる。
    pub operation_id: Option<OperationId>,
}

/// 許可形状の構造判定を通過した `ALTER TABLE ... ADD COLUMN ...` 文
/// （TASK-202・SQL-23。Issue #900）。DDL（テーブル定義の変更）であり、
/// `USING OPERATION_ID` 句は取らない（`ValidatedTruncate`／`ValidatedInsert`
/// とは異なり `operation_id` を保持しない。SQL-23 の DDL は台帳〔TASK-93〕の
/// 対象外）。
///
/// 受理する形は `ALTER TABLE <table> ADD COLUMN <column> <type> [;]` のみ
/// （`IF NOT EXISTS`・複数 `ADD`・列制約〔`NOT NULL`／`DEFAULT`／`PRIMARY KEY`
/// 等〕・`DROP COLUMN`／`ALTER COLUMN`・`RETURNING`・`USING OPERATION_ID` の
/// 併用はいずれも許可リスト外。構造検証段階ではカタログ照会を一切行わない
/// （テーブル・列の存在確認は `sql::ddl::execute_alter_table_add_column` が
/// DDL 権限ゲート通過後に行う——権限の無い主体への存在オラクル化を防ぐ
/// ため）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAlterTableAddColumn {
    pub table_name: String,
    pub column_name: String,
    /// 型名の構文木。意味づけ（ENUM 型名の存在確認・`ColumnType` への変換）は
    /// `sql::ddl::execute_alter_table_add_column` の責務。
    pub column_type: crate::sql::ddl_column_type::SqlColumnTypeName,
}

/// 許可形状の構造判定を通過した UPDATE 文（SQL-17、TASK-191）。`ValidatedInsert` と
/// 同様、本モジュールが保証するのはここまでの構造情報のみで、列名・値の意味論的
/// 妥当性は検証しない（`sql::parser::bind_update` の責務）。
///
/// 受理する形は `UPDATE <table> SET <col> = <lit>[, <col> = <lit>]* WHERE id = <n>
/// USING OPERATION_ID '<id>' [;]` の単一行・id 指定形のみ（述語形 WHERE・複数テーブル・
/// サブクエリは許可リスト外。`RETURNING` は構造上受理できるが実行結線（#865）
/// が未着手のため `validate_update_tokens`／`validate_update_form_tokens` が
/// 一律 `42601` 拒否する〔Issue #873・SQL-21〕。実行結線・可視性判定は #865 の担当）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedUpdate {
    /// UPDATE に指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    /// SET 句の (列名, リテラル) 対応。宣言順を保持する（`bind_update` の出力順・
    /// 将来の `operation_id` 内容照合の正規化基準になるため並べ替えない）。
    pub assignments: Vec<(String, InsertLiteral)>,
    /// `WHERE id = <n>` の生数値文字列。範囲検証は `sql::parser::bind_update` が行う。
    pub id_literal: String,
    /// 文末専用句で搬送された、検証済みの `operation_id`（SQL-17）。句の欠落・明示
    /// `NULL` はいずれも `None`（TASK-92・RECOVER-1）。`validate_update` は
    /// `LedgerMode::Ledgered`（既定）では `None` を書き込みトランザクション開始前に
    /// `23502` で拒否するため、この構成では常に `Some` になる。
    /// `LedgerMode::CompareOnlyWithoutLedger` では `None` を許す。
    pub operation_id: Option<OperationId>,
    /// `RETURNING` 句（Issue #873・SQL-21）。`UPDATE` は実行結線（#865）が
    /// 未着手のため、`validate_update_tokens`／`validate_update_form_tokens` が
    /// `Some` を構造検証段で常に `42601` 拒否する（黙って保持し将来の実行器が
    /// 無視する fail-open を防ぐチョークポイント）。この型が構築される時点では
    /// 常に `None`。
    pub returning: Option<Projection>,
}

/// 許可形状の構造判定を通過した `DROP TABLE` 文（SQL-23、TASK-203、Issue #902）。
/// カタログ定義（`CATALOG_TABLE` エントリ）と全テナントの行ストア・
/// `operation_id` 台帳エントリを不可逆に削除する DDL であり、`TRUNCATE`
/// （[`ValidatedTruncate`]。テナントスコープの書き込み系操作）とは異なり
/// [`crate::policy::PolicyContext`] を取らない（`catalog::Storage::drop_table`
/// ドキュメント参照）。
///
/// 受理する形は `DROP TABLE <table> [;]` のみ（`IF EXISTS`・`CASCADE`・
/// `RESTRICT`・複数テーブル列挙・`USING OPERATION_ID` 句は許可リスト外として
/// `42601`。`operation_id` を要求しない——`CREATE TABLE`・`ALTER TABLE` と
/// 同じく DDL は台帳の対象外。詳細は `docs/design/drop-table.md` 参照）。
///
/// **本構造体はカタログ照会を一切行わない**（`validate_drop_table_tokens` の
/// ドキュメント参照）: DDL 実行権限ゲート（[`crate::sql::ddl::
/// require_ddl_permission`]）を対象テーブルの存在確認より先に通す契約
/// （security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」）
/// を維持するため、将来ここへ `TableLookup` を追加して存在確認を前倒しし
/// ないこと。存在確認は `sql::ddl::execute_drop_table` が書き込みトランザクション
/// 内で行う（TOCTOU を避ける設計）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDropTable {
    /// DROP TABLE に指定されたテーブル名（識別子構文の検証のみ済み。カタログ
    /// 存在確認は未実施）。
    pub table_name: String,
}

/// 1 文の最大トークン数を超えない前提の下で使うパーサーカーソル。
/// 再帰下降だが文法の深さは定数（statement → select_list/where/order_by の 1 階層）で、
/// 深いネストによるスタック消費は発生しない。
struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// 式ノード（`Expr::Number` / `Ident` / `Binary` / `Call`）の残り生成可能数。
    /// `parse_add_expr` / `parse_mul_expr` の左結合ループは `depth` を増やさず
    /// `lhs` に木を積み続けるため、`MAX_EXPR_DEPTH`（構文解析の再帰段数）だけでは
    /// "1+1+...+1" のような同一深さの連鎖入力を制限できない。ノード生成のたびに
    /// 本フィールドを課金し、AST 全体のノード数を UDF 本体と同じ [`MAX_EXPR_NODES`]
    /// で頭打ちにすることで、ノード予算枯渇後の `Box<Expr>` 再帰的 drop による
    /// スタック消費も定数に抑える（security.md「不安全な設計｜無制限リソース確保
    /// （DoS）」対応。1 文（`Parser` 1 インスタンス）につき共有）。
    expr_node_budget: usize,
    /// サブクエリ（`IN (SELECT ...)`／`EXISTS (SELECT ...)`）を構文的に受理して
    /// よい文脈かどうかのゲート（Issue #927・SQL-29 (a)・TASK-213）。`None`
    /// （既定）は不許可＝`parse_where_leaf` が検出時点で `42601` を返す
    /// （CHECK 本体・`CREATE VIEW` 本体・カーソル・`COPY`・述語形
    /// `UPDATE`/`DELETE` はいずれも `Parser::new` の既定のまま呼ぶため、
    /// 個別に拒否腕を書かなくても fail-closed になる）。`Some(depth)` は
    /// 現在の入れ子深さ（最外側は 0）で、[`MAX_SUBQUERY_DEPTH`] 超過を
    /// 検出したら `54000` にする。`sql::allowlist::validate_sql_tokens` の
    /// トップレベル呼び出し（`parse_select_shape`/`parse_aggregate_shape` の
    /// 呼び出し元）だけが `Some(0)` を設定する。
    subquery_ctx: Option<usize>,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            expr_node_budget: MAX_EXPR_NODES,
            subquery_ctx: None,
        }
    }

    /// サブクエリを許可する文脈で構築する（Issue #927・TASK-213）。`depth` は
    /// 現在位置の入れ子深さ（`sql::subquery` が内側 SELECT を再検証する際に
    /// 1 つ進めて渡す）。
    fn with_subquery_ctx(mut self, depth: usize) -> Self {
        self.subquery_ctx = Some(depth);
        self
    }

    /// `IN (SELECT ...)`／`EXISTS (SELECT ...)` を検出した位置で呼ぶ。現在の
    /// 文脈が許可されていなければ `42601`、許可されていても深さ上限
    /// （[`MAX_SUBQUERY_DEPTH`]）を超えるなら `54000`。返り値は新設する
    /// [`WherePredicate::InSubquery`]/[`WherePredicate::Exists`] へ格納する
    /// `depth`（このサブクエリ自身の深さ＝現在の文脈深さ＋1）。
    fn require_subquery_depth(&self) -> Result<usize, SqlSurfaceError> {
        let ctx_depth = self.subquery_ctx.ok_or_else(|| {
            SqlSurfaceError::unsupported("subquery is not allowed in this context")
        })?;
        let next_depth = ctx_depth
            .checked_add(1)
            .filter(|d| *d <= MAX_SUBQUERY_DEPTH);
        next_depth.ok_or_else(|| {
            SqlSurfaceError::payload_too_large(format!(
                "subquery nesting exceeds limit {MAX_SUBQUERY_DEPTH}"
            ))
        })
    }

    /// 式ノードを 1 つ生成する直前に呼び、予算を消費する。予算枯渇時は
    /// fail-closed に拒否する（[`Self::expr_node_budget`] 参照）。
    fn consume_expr_node(&mut self) -> Result<(), SqlSurfaceError> {
        self.expr_node_budget = self.expr_node_budget.checked_sub(1).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("expression exceeds the allowed node count")
        })?;
        Ok(())
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<(), SqlSurfaceError> {
        match self.advance() {
            Some(Token::Keyword(k)) if *k == kw => Ok(()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected keyword {kw:?}, got {other:?}"
            ))),
        }
    }

    /// パーサー位置に応じた文脈的キーワード照合（PR #189 レビュー指摘対応・P1）。
    /// `INSERT`/`INTO`/`VALUES`/`USING`/`OPERATION_ID` は
    /// [`lexer::Keyword`] へ含めない（`lexer.rs` の設計メモ参照）ため、
    /// [`Token::Ident`] を大文字小文字を区別せず文字列比較して INSERT 許可形状の
    /// 期待位置でのみキーワードとして扱う。同名の一般識別子（テーブル名・列名）は
    /// `expect_ident` を通る位置に置かれる限り、本メソッドの対象外として素通しする。
    fn expect_contextual_keyword(&mut self, word: &str) -> Result<(), SqlSurfaceError> {
        match self.advance() {
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word) => Ok(()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected keyword {word}, got {other:?}"
            ))),
        }
    }

    /// 次のトークンが文脈的キーワード `word` に一致するかを消費せずに判定する
    /// （`parse_operation_id_clause` が `USING` 句の有無で分岐するために使う）。
    fn peek_contextual_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
    }

    fn expect_punct(&mut self, c: char) -> Result<(), SqlSurfaceError> {
        match self.advance() {
            Some(Token::Punct(p)) if *p == c => Ok(()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected '{c}', got {other:?}"
            ))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, SqlSurfaceError> {
        match self.advance() {
            Some(Token::Ident(name)) => Ok(name.clone()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected identifier, got {other:?}"
            ))),
        }
    }

    /// 現在位置のトークンが `Token::Ident` かつ大文字小文字を区別せず `word` と
    /// 一致するかを消費せずに判定する。TASK-161（SQL-12）修正: `USING`・`SET` は
    /// 字句解析段階では予約語化せず（[`lexer`] のモジュールコメント参照）、構文上
    /// その語が必須の位置でのみ本メソッドで文脈的にキーワードとして判定する。
    /// これにより、それ以外の識別子位置（`FROM`・投影・`ORDER BY` 等）では
    /// `using`・`set` を従来どおり通常の識別子として扱える。
    fn peek_ident_matches(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case(word))
    }

    /// [`Self::peek_ident_matches`] の 2 トークン先読み版。`CREATE TABLE` の表
    /// 制約 `PRIMARY KEY (...)`（TABLE-16・TASK-204、Issue #903）を、列名
    /// `primary`（`Ident("PRIMARY")` を消費せず単独の列名として使うケース）と
    /// 曖昧さなく判定するために使う——`offset` 個先のトークンが `KEY` である
    /// 場合にのみ表制約として扱う。
    fn peek_ident_matches_at(&self, offset: usize, word: &str) -> bool {
        matches!(
            self.tokens.get(self.pos.saturating_add(offset)),
            Some(Token::Ident(name)) if name.eq_ignore_ascii_case(word)
        )
    }

    /// 現在位置が `Token::Ident` かつ大文字小文字を区別せず `word` と一致する場合のみ
    /// 消費して成功とする。TASK-161（SQL-12）修正: `SET` を statement 先頭という
    /// 文脈でのみキーワードとして判定するために使う（[`Parser::peek_ident_matches`]
    /// 参照）。
    fn expect_ident_matching(&mut self, word: &str) -> Result<(), SqlSurfaceError> {
        if !self.peek_ident_matches(word) {
            let other = self.peek();
            return Err(SqlSurfaceError::unsupported(format!(
                "expected '{word}', got {other:?}"
            )));
        }
        self.advance();
        Ok(())
    }

    fn expect_string_literal(&mut self) -> Result<String, SqlSurfaceError> {
        match self.advance() {
            Some(Token::StringLiteral(s)) => Ok(s.clone()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected string literal, got {other:?}"
            ))),
        }
    }

    fn expect_number(&mut self) -> Result<String, SqlSurfaceError> {
        match self.advance() {
            Some(Token::Number(n)) => Ok(n.clone()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected number, got {other:?}"
            ))),
        }
    }

    /// `LIMIT n` の直後に現れうる任意の `OFFSET m` を判定・消費する（Issue #916・
    /// SQL-25 (b)・TASK-209）。`USING`・`SET` と同じ理由（[`Self::peek_ident_matches`]
    /// 参照）で `OFFSET` は [`crate::sql::lexer::Keyword`] に追加せず、この位置限定の
    /// 文脈識別子として扱う（既存の列名・テーブル名 `offset` を壊さない）。値の構文
    /// エラー（小数・`u32` 超過）は呼び出し元と同じ `42601` に分類する。範囲上限の
    /// 検証はここでは行わない（束縛段の `sql::parser::validate_search_offset`）。
    fn parse_optional_offset(&mut self) -> Result<Option<u32>, SqlSurfaceError> {
        if !self.peek_ident_matches("OFFSET") {
            return Ok(None);
        }
        self.advance();
        let offset_str = self.expect_number()?;
        let offset: u32 = offset_str.parse().map_err(|_| {
            SqlSurfaceError::unsupported(format!("malformed OFFSET value: {offset_str}"))
        })?;
        Ok(Some(offset))
    }

    /// SELECT リストの許可形状（`*`・単純な列名リスト・TASK-79（SQL-9）で追加した
    /// 式項目〔関数呼び出しを頂点に持つ式、任意で `AS <alias>`〕の混在リスト）。
    /// 全項目が単純な列名の場合は従来どおり [`Projection::Columns`]（後方互換。
    /// `AS` を列名として使う既存形を壊さないため、列名項目には `AS` を付けない）。
    fn parse_select_list(&mut self) -> Result<Projection, SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct('*'))) {
            self.advance();
            return Ok(Projection::All);
        }
        let mut items = vec![self.parse_select_item()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            items.push(self.parse_select_item()?);
        }
        if items.iter().all(|it| matches!(it, SelectItem::Column(_))) {
            let columns = items
                .into_iter()
                .map(|it| match it {
                    SelectItem::Column(name) => name,
                    SelectItem::Expr { .. } => unreachable!("filtered above"),
                })
                .collect();
            return Ok(Projection::Columns(columns));
        }
        Ok(Projection::Items(items))
    }

    /// SELECT リストの 1 項目。次のトークンが `ident '('` なら式項目（関数呼び出し。
    /// 続けて `AS <alias>` を任意で受理する）、それ以外は従来どおり裸の列名として
    /// 受理する。
    fn parse_select_item(&mut self) -> Result<SelectItem, SqlSurfaceError> {
        if let Some(Token::Ident(name)) = self.peek() {
            let name = name.clone();
            if matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('('))) {
                self.advance();
                let expr = self.parse_call_expr(name, 0)?;
                let alias = if self.peek_ident_matches("AS") {
                    self.advance();
                    Some(self.expect_ident()?)
                } else {
                    None
                };
                return Ok(SelectItem::Expr { expr, alias });
            }
        }
        Ok(SelectItem::Column(self.expect_ident()?))
    }

    /// SQL-30・TASK-214（Issue #930）: 広域取得（`ParsedSelect::Scan`）専用の SELECT
    /// リスト解析。[`Self::parse_select_list`] と異なり、ウィンドウ項目
    /// （`<func>(...) OVER (...)`）を通常項目（[`SelectItem`]）とは別に
    /// [`WindowSelectItem`] へ振り分けて返す（[`Projection`] 自体はウィンドウ項目を
    /// 保持できないため）。呼び出し元（[`parse_select_shape`]）は、ウィンドウ項目が
    /// 1 つでもあれば広域取得（`LIMIT` 直接。ランキング段を持たない）のみを許可し、
    /// `ORDER BY`／`USING PLAN` を伴う検索 SELECT 形状とは併用させない
    /// （§計画 2「対象外」）。
    ///
    /// `*` とウィンドウ項目の混在（`SELECT *, ROW_NUMBER() OVER () ...`）は
    /// 受理しない: 先頭が `*` 単独なら [`Self::parse_select_list`] と同じく即座に
    /// `Projection::All` を返す（後続のカンマ区切り項目を読まない）ため、
    /// `SELECT *, ...` の残りトークンが `expect_end_of_statement` で `42601` に
    /// 落ちる。
    fn parse_select_list_with_windows(
        &mut self,
    ) -> Result<(Projection, Vec<WindowSelectItem>), SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct('*'))) {
            self.advance();
            return Ok((Projection::All, Vec::new()));
        }
        let mut plain_items: Vec<SelectItem> = Vec::new();
        let mut window_items: Vec<WindowSelectItem> = Vec::new();
        let mut position: usize = 0;
        loop {
            if self.peek_is_window_item_start() {
                if window_items.len() >= MAX_AGGREGATE_ITEMS {
                    return Err(SqlSurfaceError::payload_too_large("too many window items"));
                }
                let mut item = self.parse_window_item()?;
                item.position = position;
                window_items.push(item);
            } else {
                plain_items.push(self.parse_select_item()?);
            }
            position = position
                .checked_add(1)
                .ok_or_else(|| SqlSurfaceError::payload_too_large("too many SELECT list items"))?;
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        let projection = if plain_items
            .iter()
            .all(|it| matches!(it, SelectItem::Column(_)))
        {
            Projection::Columns(
                plain_items
                    .into_iter()
                    .map(|it| match it {
                        SelectItem::Column(name) => name,
                        SelectItem::Expr { .. } => unreachable!("filtered above"),
                    })
                    .collect(),
            )
        } else {
            Projection::Items(plain_items)
        };
        Ok((projection, window_items))
    }

    /// 現在位置が「`<ident> '(' ... ')' OVER '('`」の並びで始まるかを、消費せず
    /// 判定する（[`Self::find_matching_close_paren`] で対応する `')'` を探し、その
    /// 直後が文脈識別子 `OVER`・その次が `'('` かを見る）。対応する `')'` が
    /// 見つからない場合は通常項目として扱う（`parse_select_item`／
    /// `parse_call_expr` 側の既存エラー処理に委ねる）。
    fn peek_is_window_item_start(&self) -> bool {
        let Some(Token::Ident(_)) = self.peek() else {
            return false;
        };
        if !matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('('))) {
            return false;
        }
        let Some(close_idx) = self.find_matching_close_paren(self.pos + 1) else {
            return false;
        };
        matches!(
            self.tokens.get(close_idx + 1),
            Some(Token::Ident(name)) if name.eq_ignore_ascii_case("OVER")
        ) && matches!(self.tokens.get(close_idx + 2), Some(Token::Punct('(')))
    }

    /// ウィンドウ項目 1 つ（SQL-30・TASK-214、Issue #930）:
    /// `<func>(<args>) OVER ( [PARTITION BY <ident>[, ...]] [ORDER BY <ident>
    /// [ASC|DESC][, ...]] ) [AS <alias>]`。呼び出し元
    /// （[`Self::parse_select_list_with_windows`]）が [`Self::peek_is_window_item_start`]
    /// で先読み確定済みの前提で呼ぶ。`position` は呼び出し元が後から設定する
    /// （ここでは常に `0`）。
    ///
    /// スコープ縮小（`docs/design/window-functions.md` 参照。§計画 2「対象外」）:
    /// フレーム句（`ROWS`/`RANGE`/`GROUPS`）・`NULLS FIRST/LAST`・名前付き
    /// ウィンドウ（`WINDOW w AS (...)`/`OVER w`）・`FILTER (...)`・関数内
    /// `DISTINCT`・複合式の引数はいずれも許可リスト外として `42601` に落ちる
    /// （該当する構文を消費する分岐を意図的に持たない）。
    fn parse_window_item(&mut self) -> Result<WindowSelectItem, SqlSurfaceError> {
        let name = self.expect_ident()?;
        let func = WindowFunc::from_name(&name).ok_or_else(|| {
            SqlSurfaceError::unsupported(format!("unsupported window function: {name}"))
        })?;
        self.expect_punct('(')?;
        let arg = if func.is_ranking() {
            None
        } else {
            // `DISTINCT` は本 Issue のスコープ外（`COUNT(DISTINCT ...) OVER (...)`
            // は非対応。§計画 2「対象外」）。
            if is_distinct_modifier(self.tokens, self.pos) {
                return Err(SqlSurfaceError::unsupported(
                    "DISTINCT is not supported inside a window aggregate function",
                ));
            }
            if matches!(self.peek(), Some(Token::Punct('*'))) {
                if func != WindowFunc::Count {
                    return Err(SqlSurfaceError::unsupported(
                        "'*' is only allowed inside COUNT(*)",
                    ));
                }
                self.advance();
                Some(AggregateArg::Star)
            } else {
                match self.peek() {
                    // 引数は裸の列名のみを受理する（複合式・UDF 呼び出しは
                    // §計画 2「対象外」）。
                    Some(Token::Ident(_)) => {
                        let column = self.expect_ident()?;
                        Some(AggregateArg::Expr(Expr::Ident(column)))
                    }
                    Some(Token::Punct(')')) => {
                        return Err(SqlSurfaceError::unsupported(
                            "window aggregate function requires exactly one argument",
                        ));
                    }
                    other => {
                        return Err(SqlSurfaceError::unsupported(format!(
                            "window aggregate function argument must be a bare column reference, got {other:?}"
                        )));
                    }
                }
            }
        };
        self.expect_punct(')')?;
        self.expect_ident_matching("OVER")?;
        self.expect_punct('(')?;
        let mut partition_by = Vec::new();
        if self.peek_contextual_keyword("PARTITION") {
            self.advance();
            self.expect_keyword(Keyword::By)?;
            partition_by.push(self.expect_ident()?);
            while matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                if partition_by.len() >= MAX_WINDOW_KEYS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many PARTITION BY columns",
                    ));
                }
                partition_by.push(self.expect_ident()?);
            }
        }
        let mut order_by = Vec::new();
        if matches!(self.peek(), Some(Token::Keyword(Keyword::Order))) {
            self.advance();
            self.expect_keyword(Keyword::By)?;
            loop {
                if order_by.len() >= MAX_WINDOW_KEYS {
                    return Err(SqlSurfaceError::payload_too_large("too many ORDER BY keys"));
                }
                let column = self.expect_ident()?;
                let descending = if self.peek_ident_matches("DESC") {
                    self.advance();
                    true
                } else if self.peek_ident_matches("ASC") {
                    self.advance();
                    false
                } else {
                    false
                };
                order_by.push((column, descending));
                if matches!(self.peek(), Some(Token::Punct(','))) {
                    self.advance();
                    continue;
                }
                break;
            }
        }
        self.expect_punct(')')?;
        let alias = if self.peek_ident_matches("AS") {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(WindowSelectItem {
            position: 0,
            func,
            arg,
            partition_by,
            order_by,
            alias,
        })
    }

    /// 集計 SELECT リストの 1 項目（TASK-166・SQL-13。SQL-25 (c)・TASK-209 で
    /// `COUNT(DISTINCT <expr>)` を追加）:
    /// `<agg_name> '(' [DISTINCT] ('*' | <expr>) ')' [AS <alias>]`。`*` は
    /// `COUNT` 専用（それ以外の関数での出現は `42601`）。空引数（`COUNT()`）・
    /// 複数引数はいずれも構造的に受理しない（`)` を期待する位置で不一致となり
    /// `42601` へ落ちる）。`DISTINCT` は `COUNT` 以外の関数・`COUNT(DISTINCT *)`
    /// では `42601`（SQL-25 (c) の対象は `COUNT` のみ）。
    fn parse_aggregate_item(&mut self) -> Result<AggregateItem, SqlSurfaceError> {
        let name = self.expect_ident()?;
        let func = AggregateFunc::from_name(&name).ok_or_else(|| {
            SqlSurfaceError::unsupported(format!("unsupported aggregate function: {name}"))
        })?;
        self.expect_punct('(')?;
        // `DISTINCT` は `'('` の直後・引数の直前という文脈でのみ修飾子として
        // 消費する（[`is_distinct_modifier`] と同じ判定規則。列名 `distinct` を
        // 引数に持つ `COUNT(distinct)` を壊さないよう、次のトークンが識別子・`*`
        // でない場合は消費しない）。
        let distinct = if is_distinct_modifier(self.tokens, self.pos) {
            self.advance();
            true
        } else {
            false
        };
        if distinct && func != AggregateFunc::Count {
            return Err(SqlSurfaceError::unsupported(
                "DISTINCT is only allowed inside COUNT(DISTINCT ...)",
            ));
        }
        let arg = if matches!(self.peek(), Some(Token::Punct('*'))) {
            if func != AggregateFunc::Count {
                return Err(SqlSurfaceError::unsupported(
                    "'*' is only allowed inside COUNT(*)",
                ));
            }
            if distinct {
                return Err(SqlSurfaceError::unsupported(
                    "COUNT(DISTINCT *) is not supported",
                ));
            }
            self.advance();
            AggregateArg::Star
        } else {
            if matches!(self.peek(), Some(Token::Punct(')'))) {
                return Err(SqlSurfaceError::unsupported(
                    "aggregate function requires exactly one argument",
                ));
            }
            let expr = self.parse_value_expr(0)?;
            AggregateArg::Expr(expr)
        };
        self.expect_punct(')')?;
        let alias = if self.peek_ident_matches("AS") {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(AggregateItem {
            func,
            arg,
            alias,
            distinct,
        })
    }

    /// 集計 SELECT リストの 1 項目（TASK-167・SQL-14 で拡張）。次のトークンが
    /// 集計関数名 `'('` なら従来どおり集計項目（[`Parser::parse_aggregate_item`]）、
    /// それ以外は `GROUP BY` 列と同名の裸の識別子（任意で `AS <alias>`）として
    /// [`AggregateSelectItem::GroupKey`] へ構造上受理する（列名一致・`GROUP BY` 句
    /// 自体の有無は [`parse_aggregate_shape`] が全体を読み終えてから検査する。
    /// 許可リストとして「集計項目か裸の識別子か」の 2 形にのみ絞り込み、それ以外
    /// （式・関数呼び出しの混在等）は `expect_ident` の失敗で `42601` に落ちる）。
    fn parse_aggregate_select_item(&mut self) -> Result<AggregateSelectItem, SqlSurfaceError> {
        if let Some(Token::Ident(name)) = self.peek() {
            if is_aggregate_function_name(name)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('(')))
            {
                return Ok(AggregateSelectItem::Aggregate(self.parse_aggregate_item()?));
            }
        }
        let column = self.expect_ident()?;
        let alias = if self.peek_ident_matches("AS") {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(AggregateSelectItem::GroupKey { column, alias })
    }

    /// `GROUP BY <column> (',' <column>)*`（TASK-167・SQL-14。SQL-25 (d) で複数列へ
    /// 拡張）。裸識別子のカンマ区切り列のみ受理する（式・関数・位置番号・`$n` は
    /// いずれも `expect_ident` の失敗で `42601`）。`GROUP` は予約語化せず
    /// [`Parser::expect_contextual_keyword`] で文脈的に照合する（PR #189 の方針）。
    /// 列数は [`check_group_by_column_count`] で `push` 前に検査し（`54000`。
    /// [`Parser::parse_having`] と同じ「確保前検査」）、重複する列名は `42601` で
    /// 拒否する（spec に規定が無いため fail-closed に倒す）。
    fn parse_group_by_clause(&mut self) -> Result<Vec<String>, SqlSurfaceError> {
        self.expect_contextual_keyword("GROUP")?;
        self.expect_keyword(Keyword::By)?;
        let mut columns = Vec::new();
        loop {
            let column = self.expect_ident()?;
            if columns.contains(&column) {
                return Err(SqlSurfaceError::unsupported(format!(
                    "duplicate GROUP BY column {column:?}"
                )));
            }
            check_group_by_column_count(columns.len() + 1)?;
            columns.push(column);
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(columns)
    }

    /// `HAVING <having_pred> [AND <having_pred>]*`（TASK-167・SQL-14）。
    /// `<having_pred> := <ident> <cmp> ['-'] <number>`。左辺は SELECT リスト集計
    /// 項目の実効名への参照のみを構造上許可し（存在確認・型検査は束縛段）、右辺は
    /// 数値リテラル限定（文字列リテラル・両辺集計・括弧・`OR` はいずれも許可リスト
    /// 外）。条件数は [`MAX_AGGREGATE_ITEMS`] で頭打ちにする（`54000`。無制限
    /// `Vec` 確保を避ける方針を HAVING 条件にも適用）。
    fn parse_having(&mut self) -> Result<Vec<HavingPredicate>, SqlSurfaceError> {
        self.expect_contextual_keyword("HAVING")?;
        let mut predicates = Vec::new();
        loop {
            if predicates.len() >= MAX_AGGREGATE_ITEMS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many HAVING predicates",
                ));
            }
            let item_name = self.expect_ident()?;
            let op = self.expect_cmp_op()?;
            let negative = matches!(self.peek(), Some(Token::Punct('-')));
            if negative {
                self.advance();
            }
            let raw = self.expect_number()?;
            let mut literal = crate::sql::udf_call::parse_number_literal(&raw)?;
            if negative {
                literal = -literal;
            }
            predicates.push(HavingPredicate {
                item_name,
                op,
                literal,
            });
            if matches!(self.peek(), Some(Token::Keyword(Keyword::And))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(predicates)
    }

    /// 集計 `GROUP BY` の `ORDER BY <target> [ASC|DESC]`（TASK-167・SQL-14）。
    /// `<target>` は `GROUP BY` 列名または SELECT リスト集計項目の実効名のいずれか
    /// 1 つの識別子（意味論的な解決は束縛段）。`ASC`/`DESC` は予約語化せず文脈的に
    /// 照合し、省略時は昇順として扱う。
    fn parse_aggregate_order_by(&mut self) -> Result<AggregateOrderBy, SqlSurfaceError> {
        self.expect_keyword(Keyword::Order)?;
        self.expect_keyword(Keyword::By)?;
        let target = self.expect_ident()?;
        let descending = if self.peek_ident_matches("DESC") {
            self.advance();
            true
        } else if self.peek_ident_matches("ASC") {
            self.advance();
            false
        } else {
            false
        };
        Ok(AggregateOrderBy { target, descending })
    }

    /// 集計 `GROUP BY` の `LIMIT <n>`（TASK-167・SQL-14）。構文段では `u32` として
    /// 受理するのみで、範囲検査（`1..=MAX_GROUPS`）は束縛段
    /// （`sql::parser::bind_aggregate`）が行う。
    fn parse_aggregate_limit(&mut self) -> Result<u32, SqlSurfaceError> {
        self.expect_keyword(Keyword::Limit)?;
        let raw = self.expect_number()?;
        raw.parse()
            .map_err(|_| SqlSurfaceError::unsupported(format!("malformed LIMIT value: {raw}")))
    }

    /// WHERE 句の許可形状（等価条件・前方一致条件（TASK-147・EXT-3）・述語呼び出し形・
    /// TASK-79（SQL-9）で追加した式の比較述語 `<expr> <cmp> <expr>` の 4 種。
    /// `OR`・括弧によるネストは引き続き許可しない）。述語呼び出し形は許可された名前
    /// （[`is_allowed_where_predicate_name`]）のみを受理し、未知の名前は拒否する。
    ///
    /// 既存形との曖昧さ回避: 先頭が `ident '=' <string literal>` なら等価条件、
    /// `ident <文脈的 'LIKE'> <string literal>` なら前方一致条件、`ident '(' ')'`
    /// （許可名のみ）なら述語呼び出し形として確定的に判定し、いずれにも一致しない
    /// 場合のみ式の比較述語として再解析する（`pos` を巻き戻してから解析し直す。
    /// 式文法の `primary` は `ident '(' <args> ')'` も受理するため、`visible()`
    /// 以外の名前の呼び出し形はここで初めて式として解釈される）。`LIKE` は
    /// [`Keyword`] へ追加せず `Token::Ident` を本メソッド内でのみ文脈的に照合する
    /// （TASK-80 と同じ方式。`like` という列名の等価条件を壊さない）。`NOT LIKE`・
    /// `ILIKE`・`LIKE` の右辺が非リテラルの各形は、この確定判定に一致しないため
    /// 式述語フォールバックへ流れ、通常は `42601` で拒否される。
    fn parse_where(&mut self) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let mut leaf_count = 0usize;
        self.parse_where_or(false, 0, &mut leaf_count)
    }

    /// `CHECK (<body>)` の本体（TABLE-16・TASK-204、Issue #906）を [`Self::parse_where`]
    /// と同じ文法で解析する。`WHERE` との唯一の違いは、末尾の `)` を境界トークン
    /// として扱う点（`is_where_predicate_boundary_token` の `extra_close_paren`）で、
    /// これにより裸の BOOLEAN 列参照（`CHECK (flag)`）が式フォールバックへ誤って
    /// 落ちずに受理される。呼び出し元（[`Self::parse_check_clause`]）が `(` を消費
    /// した直後に呼び、本体解析の完了後に `expect_punct(')')` で閉じ括弧を消費する。
    fn parse_check_body(&mut self) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let mut leaf_count = 0usize;
        self.parse_where_or(true, 0, &mut leaf_count)
    }

    /// [`Self::parse_where`]・[`Self::parse_check_body`] が共有する述語ツリーの
    /// 文法入口（TASK-208・SQL-24、Issue #912）: `or_expr := and_expr { OR
    /// and_expr }`。`OR` は [`Keyword`] へ追加せず `Token::Ident` を文脈的に照合する
    /// （`LIKE` と同じ方式。`or` という列名の等価条件を壊さない）。
    ///
    /// 分岐が 1 個だけなら親の列へそのまま平坦化し（`AND` だけの文は本機能追加前と
    /// 完全に同じ AST になる）、2 個以上なら 1 要素の [`WherePredicate::Or`] として
    /// 返す。`visible()`（RLS 述語）が 2 分岐以上の `Or` の中に現れる場合は
    /// `42601` で拒否する（RLS-7: `visible() OR ...` で RLS を解除したように見せる
    /// 式を作らせない）。
    fn parse_where_or(
        &mut self,
        extra_close_paren: bool,
        depth: usize,
        leaf_count: &mut usize,
    ) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let mut branches = vec![self.parse_where_and(extra_close_paren, depth, leaf_count)?];
        while matches!(self.peek(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("OR")) {
            self.advance();
            branches.push(self.parse_where_and(extra_close_paren, depth, leaf_count)?);
        }
        if branches.len() == 1 {
            Ok(branches
                .into_iter()
                .next()
                .expect("branches has exactly one element in this arm"))
        } else {
            if branches
                .iter()
                .any(|branch| where_predicates_contain_visible(branch))
            {
                return Err(SqlSurfaceError::unsupported(
                    "visible() predicate is not allowed inside an OR branch",
                ));
            }
            Ok(vec![WherePredicate::Or(branches)])
        }
    }

    /// `and_expr := atom { AND atom }`。
    fn parse_where_and(
        &mut self,
        extra_close_paren: bool,
        depth: usize,
        leaf_count: &mut usize,
    ) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let mut predicates = Vec::new();
        loop {
            predicates.extend(self.parse_where_atom(extra_close_paren, depth, leaf_count)?);
            if matches!(self.peek(), Some(Token::Keyword(Keyword::And))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(predicates)
    }

    /// `atom := '(' or_expr ')' | leaf`。`(` の直後が値式グループ
    /// （`(id + 1) > 5` 等。既存の式フォールバックへ委譲する）か BOOLEAN
    /// グループ（`(a OR b)`）かを、後戻りせず決定的な先読みで判定する
    /// （対応する `)` をトークン走査で探し、直後のトークンが比較・算術演算子
    /// なら値式、それ以外なら BOOLEAN グループ。ネストした括弧で「グループとして
    /// 解析し、失敗したら葉として解析し直す」後戻りをすると指数時間になるため
    /// 禁止する。security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    fn parse_where_atom(
        &mut self,
        extra_close_paren: bool,
        depth: usize,
        leaf_count: &mut usize,
    ) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct('('))) {
            let close_idx = self.find_matching_close_paren(self.pos).ok_or_else(|| {
                SqlSurfaceError::unsupported("unmatched parenthesis in WHERE clause")
            })?;
            let is_value_group = matches!(
                self.tokens.get(close_idx + 1),
                Some(token) if is_where_group_operator_token(token)
            );
            if !is_value_group {
                let next_depth = depth
                    .checked_add(1)
                    .filter(|d| *d <= MAX_WHERE_GROUP_DEPTH)
                    .ok_or_else(|| {
                        SqlSurfaceError::payload_too_large(format!(
                            "WHERE grouping nesting exceeds limit {MAX_WHERE_GROUP_DEPTH}"
                        ))
                    })?;
                self.advance(); // '(' を消費する
                let inner = self.parse_where_or(true, next_depth, leaf_count)?;
                self.expect_punct(')')?;
                return Ok(inner);
            }
            // 値式グループ（`(id + 1) > 5` 等）。既存の式フォールバックへ委譲する
            // （`parse_primary_expr` が '(' expr ')' を再帰的に処理する）。
        }
        Ok(vec![self.parse_where_leaf(extra_close_paren, leaf_count)?])
    }

    /// `(` の位置（`open_idx`）に対応する `)` のトークン位置を探す。`get()` のみを
    /// 使い（添字アクセス・`unwrap`・`expect` 禁止。coding-rust.md）、対応する
    /// 閉じ括弧が無い場合は `None`（呼び出し元が `42601` に変換する）。
    fn find_matching_close_paren(&self, open_idx: usize) -> Option<usize> {
        let mut depth: u32 = 0;
        let mut idx = open_idx;
        loop {
            match self.tokens.get(idx)? {
                Token::Punct('(') => depth = depth.checked_add(1)?,
                Token::Punct(')') => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(idx);
                    }
                }
                _ => {}
            }
            idx = idx.checked_add(1)?;
        }
    }

    /// 述語ツリーの 1 葉（末端述語）を解析する。既存の等価条件・前方一致条件・
    /// 述語呼び出し形・BOOLEAN 列条件・範囲比較条件・式フォールバックの判定は
    /// TASK-208 以前の `parse_where_predicates` と同一のまま維持する（`OR`・括弧の
    /// 導入で AND だけの文の受理形状・優先順位を変えないため）。葉を返す**前**に
    /// 総数上限（[`MAX_WHERE_LEAVES`]）を検査し、超過時は `54000`（`push` 前に
    /// 検査し、超過分のアロケーションを増やさない）。
    fn parse_where_leaf(
        &mut self,
        extra_close_paren: bool,
        leaf_count: &mut usize,
    ) -> Result<WherePredicate, SqlSurfaceError> {
        let start = self.pos;
        let mut result: Option<WherePredicate> = None;
        if let Some(Token::Ident(name)) = self.peek().cloned() {
            if name.eq_ignore_ascii_case("EXISTS")
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('(')))
                && matches!(
                    self.tokens.get(self.pos + 2),
                    Some(Token::Keyword(Keyword::Select))
                )
            {
                // `EXISTS (SELECT ...)`（Issue #927・SQL-29 (a)・TASK-213）。`EXISTS`
                // は `LIKE`・`OR` と同じ文脈照合方式（`Keyword` へ追加しない）で、
                // `exists` という列名の裸参照（`WHERE exists`）は直後が `(` かつ
                // `SELECT` のときのみサブクエリ側へ振り分けられる（それ以外は
                // 既存どおり `BoolColumn` 等の通常の識別子解析へ流れる）。
                let depth = self.require_subquery_depth()?;
                let open_idx = self.pos + 1;
                let close_idx = self.find_matching_close_paren(open_idx).ok_or_else(|| {
                    SqlSurfaceError::unsupported("unmatched parenthesis in EXISTS subquery")
                })?;
                let inner_tokens = self
                    .tokens
                    .get((open_idx + 1)..close_idx)
                    .ok_or_else(|| {
                        SqlSurfaceError::unsupported("malformed EXISTS subquery boundary")
                    })?
                    .to_vec();
                self.pos = close_idx + 1;
                result = Some(WherePredicate::Exists {
                    inner_tokens,
                    depth,
                });
            } else if matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("IN"))
                && matches!(self.tokens.get(self.pos + 2), Some(Token::Punct('(')))
                && matches!(
                    self.tokens.get(self.pos + 3),
                    Some(Token::Keyword(Keyword::Select))
                )
            {
                // `<col> IN (SELECT ...)`（Issue #927・SQL-29 (a)・TASK-213）。
                // `IN` の直後が `SELECT` の場合のみサブクエリとして扱い、
                // リテラル列 `IN (...)`（#913・未マージ）とは構造的に衝突しない。
                let depth = self.require_subquery_depth()?;
                let open_idx = self.pos + 2;
                let close_idx = self.find_matching_close_paren(open_idx).ok_or_else(|| {
                    SqlSurfaceError::unsupported("unmatched parenthesis in IN subquery")
                })?;
                let inner_tokens = self
                    .tokens
                    .get((open_idx + 1)..close_idx)
                    .ok_or_else(|| SqlSurfaceError::unsupported("malformed IN subquery boundary"))?
                    .to_vec();
                self.pos = close_idx + 1;
                result = Some(WherePredicate::InSubquery {
                    column: name.clone(),
                    inner_tokens,
                    depth,
                });
            } else if matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('=')))
                && matches!(self.tokens.get(self.pos + 2), Some(Token::StringLiteral(_)))
            {
                self.advance();
                self.advance();
                let value = self.expect_string_literal()?;
                result = Some(WherePredicate::Equality {
                    column: name.clone(),
                    value,
                });
            } else if matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("LIKE"))
                && matches!(self.tokens.get(self.pos + 2), Some(Token::StringLiteral(_)))
            {
                self.advance();
                self.advance();
                let pattern = self.expect_string_literal()?;
                result = Some(WherePredicate::Prefix {
                    column: name.clone(),
                    pattern,
                });
            } else if is_allowed_where_predicate_name(&name)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('(')))
                && matches!(self.tokens.get(self.pos + 2), Some(Token::Punct(')')))
            {
                self.advance();
                self.advance();
                self.advance();
                result = Some(WherePredicate::PredicateCall { name: name.clone() });
            } else if matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('=')))
                && matches!(self.tokens.get(self.pos + 2), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false"))
            {
                // BOOLEAN 列の明示等価条件（`<col> = true|false`。Issue #883・
                // D-c）。大小無視は expect_literal の bool リテラルと同じ方針。
                self.advance();
                self.advance();
                let value = match self.advance() {
                    Some(Token::Ident(w)) if w.eq_ignore_ascii_case("true") => true,
                    Some(Token::Ident(w)) if w.eq_ignore_ascii_case("false") => false,
                    // 上の peek 済み条件と同じ判定のため到達しない。
                    other => {
                        return Err(SqlSurfaceError::unsupported(format!(
                            "expected true/false literal, got {other:?}"
                        )))
                    }
                };
                result = Some(WherePredicate::BoolEquality {
                    column: name.clone(),
                    value,
                });
            } else if let Some(op) = self
                .tokens
                .get(self.pos + 1)
                .and_then(where_compare_op_token)
            {
                if matches!(self.tokens.get(self.pos + 2), Some(Token::StringLiteral(_))) {
                    // `<col> (< | > | <= | >=) '<literal>'`（TABLE-13・
                    // TASK-199、Issue #891・レーン B）。逆向き
                    // （`'x' < col`）は本腕では扱わず式フォールバックへ回す
                    // （既知の制約。詳細は `docs/design/scalar-types-predicates.md`）。
                    self.advance();
                    self.advance();
                    let value = self.expect_string_literal()?;
                    result = Some(WherePredicate::Compare {
                        column: name.clone(),
                        op,
                        value,
                    });
                }
            }
            if result.is_none()
                && is_where_predicate_boundary_token(
                    self.tokens.get(self.pos + 1),
                    extra_close_paren,
                )
            {
                // BOOLEAN 列の裸参照（`WHERE flag`）。直後のトークンが WHERE 句の
                // 終端（`AND`・`OR`・`ORDER`・`LIMIT`・`;`・EOF・後続構文キーワード・
                // グループの `)`）である場合に限り受理する。受理範囲の拡大を最小限に
                // とどめ、それ以外（`flag + 1` 等）は式フォールバックへ回す
                // （Issue #883・D-c）。
                self.advance();
                result = Some(WherePredicate::BoolColumn { column: name });
            }
        }
        let predicate = match result {
            Some(predicate) => predicate,
            None => {
                self.pos = start;
                let lhs = self.parse_value_expr(0)?;
                let op = self.expect_cmp_op()?;
                let rhs = self.parse_value_expr(0)?;
                WherePredicate::Expression(Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
        };
        *leaf_count = leaf_count.checked_add(1).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("WHERE predicate leaf count overflow")
        })?;
        if *leaf_count > MAX_WHERE_LEAVES {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "WHERE predicate leaf count exceeds limit {MAX_WHERE_LEAVES}"
            )));
        }
        Ok(predicate)
    }

    /// 比較演算子トークン（`> < >= <= =`）を消費して [`BinOp`] へ写像する
    /// （TASK-79・SQL-9）。
    fn expect_cmp_op(&mut self) -> Result<BinOp, SqlSurfaceError> {
        match self.advance() {
            Some(Token::Punct('>')) => Ok(BinOp::Gt),
            Some(Token::Punct('<')) => Ok(BinOp::Lt),
            Some(Token::Ge) => Ok(BinOp::Ge),
            Some(Token::Le) => Ok(BinOp::Le),
            Some(Token::Punct('=')) => Ok(BinOp::Eq),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected comparison operator, got {other:?}"
            ))),
        }
    }

    /// 再帰深さ上限（[`MAX_EXPR_DEPTH`]）を検査する。構文解析自体の再帰段数を
    /// 制限することで、深いネスト入力によるスタック消費を定数に抑える
    /// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    fn check_expr_depth(&self, depth: usize) -> Result<(), SqlSurfaceError> {
        if depth > MAX_EXPR_DEPTH {
            return Err(SqlSurfaceError::payload_too_large(
                "expression nesting exceeds the allowed depth",
            ));
        }
        Ok(())
    }

    /// 式文法（`add → mul → primary`）の入口。`CREATE FUNCTION` の本体・SELECT の
    /// 式項目・WHERE 式述語の両辺・関数呼び出しの引数のいずれからも共通で使う
    /// （TASK-79・SQL-9）。
    fn parse_value_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        self.check_expr_depth(depth)?;
        self.parse_add_expr(depth)
    }

    fn parse_add_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        let mut lhs = self.parse_mul_expr(depth + 1)?;
        loop {
            let op = match self.peek() {
                Some(Token::Punct('+')) => BinOp::Add,
                Some(Token::Punct('-')) => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul_expr(depth + 1)?;
            self.consume_expr_node()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        let mut lhs = self.parse_primary_expr(depth + 1)?;
        loop {
            let op = match self.peek() {
                Some(Token::Punct('*')) => BinOp::Mul,
                Some(Token::Punct('/')) => BinOp::Div,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_primary_expr(depth + 1)?;
            self.consume_expr_node()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `primary := number | ident | ident '(' [expr {',' expr}] ')' | '(' expr ')'`。
    fn parse_primary_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        self.check_expr_depth(depth)?;
        match self.peek().cloned() {
            Some(Token::Number(n)) => {
                self.advance();
                self.consume_expr_node()?;
                Ok(Expr::Number(n))
            }
            Some(Token::Punct('(')) => {
                self.advance();
                let inner = self.parse_value_expr(depth + 1)?;
                self.expect_punct(')')?;
                Ok(inner)
            }
            Some(Token::Ident(name)) => {
                self.advance();
                if matches!(self.peek(), Some(Token::Punct('('))) {
                    self.parse_call_expr(name, depth)
                } else {
                    self.consume_expr_node()?;
                    Ok(Expr::Ident(name))
                }
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported expression term near {other:?}"
            ))),
        }
    }

    /// 関数呼び出し式 `<name> '(' [expr {',' expr}] ')'` を解析する。呼び出し元は
    /// 直前に `name` を消費済みで、次のトークンが `'('` である前提（`peek` 済み）。
    ///
    /// TASK-166（SQL-13）: 集計関数名（[`is_aggregate_function_name`]）はここでは
    /// 常に拒否する（`42601`）。集計関数の頂点呼び出しは
    /// [`Parser::parse_aggregate_item`] が本メソッドを経由せず直接消費するため、
    /// この経路に到達する集計名はすべて「集計項目の頂点以外」（SELECT の非集計項目・
    /// WHERE 式述語・`CREATE FUNCTION` 本体・集計引数のネスト呼び出し）での出現であり、
    /// いずれも許可形状外（`GROUP BY` を持たない集計のみを SQL-13 の受理形とする）。
    fn parse_call_expr(&mut self, name: String, depth: usize) -> Result<Expr, SqlSurfaceError> {
        if is_aggregate_function_name(&name) {
            return Err(SqlSurfaceError::unsupported(format!(
                "aggregate function {name} is only allowed as a top-level SELECT item"
            )));
        }
        self.expect_punct('(')?;
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Token::Punct(')'))) {
            args.push(self.parse_value_expr(depth + 1)?);
            while matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                if args.len() >= MAX_CALL_ARGS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many call arguments",
                    ));
                }
                args.push(self.parse_value_expr(depth + 1)?);
            }
        }
        self.expect_punct(')')?;
        self.consume_expr_node()?;
        Ok(Expr::Call { name, args })
    }

    /// ORDER BY 式の許可形状（距離演算子形または関数呼び出し形）。関数呼び出し形は
    /// 許可された名前（[`is_allowed_order_by_function_name`]）のみを受理し、
    /// 未知の名前は拒否する。
    fn parse_order_by(&mut self) -> Result<OrderByForm, SqlSurfaceError> {
        let name = self.expect_ident()?;
        match self.peek() {
            Some(Token::DistanceOp) => {
                self.advance();
                let literal = self.expect_string_literal()?;
                Ok(OrderByForm::Distance {
                    column: name,
                    literal,
                })
            }
            Some(Token::Punct('(')) => {
                if !is_allowed_order_by_function_name(&name) {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "unsupported ORDER BY function: {name}"
                    )));
                }
                self.advance();
                let args = self.parse_order_by_function_args(&name)?;
                self.expect_punct(')')?;
                Ok(OrderByForm::FunctionCall { name, args })
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported ORDER BY expression near {other:?}"
            ))),
        }
    }

    /// 許可された ORDER BY 関数ごとに、引数の個数・位置・トークン種別を明示的に
    /// 解析する。`name` は [`is_allowed_order_by_function_name`] を通過済みの
    /// 前提で呼ばれる。呼び出し元の `expect_punct(')')` が閉じ括弧を消費するため、
    /// ここでは許可した引数トークン列のみを消費し、過不足があれば
    /// （空引数・余剰引数・意味を解釈しない括弧グループを含め）その時点で拒否する
    /// （fail-closed）。
    ///
    /// `hybrid_rrf`/`HYBRID` は 2 引数形（`(<vec列>, '<query text>')`。TASK-74 で
    /// 受理済みの既存構造。マージ済み挙動を変えないため構造としては引き続き受理する）と
    /// 4 引数形（`(<vec列>, '<vec リテラル>', <text列>, '<query text>')`。TASK-75・
    /// SQL-4 の実行可能形）の両方を構造上受理する。実行可能かどうか（束縛成功）は
    /// `sql::parser::bind` が判定し、2 引数形は `SqlSurfaceError::InvalidInput`
    /// （`22000`。「実行不能」）で拒否する（advisor 方針: 既存の 2 引数形受理を壊さず
    /// 追加する）。
    fn parse_order_by_function_args(
        &mut self,
        name: &str,
    ) -> Result<Vec<FunctionArg>, SqlSurfaceError> {
        match name.to_ascii_uppercase().as_str() {
            "HYBRID_RRF" | "HYBRID" => {
                let mut args = Vec::new();
                args.push(FunctionArg::Ident(self.expect_ident()?));
                self.expect_punct(',')?;
                args.push(FunctionArg::StringLiteral(self.expect_string_literal()?));
                if matches!(self.peek(), Some(Token::Punct(','))) {
                    self.advance();
                    args.push(FunctionArg::Ident(self.expect_ident()?));
                    self.expect_punct(',')?;
                    args.push(FunctionArg::StringLiteral(self.expect_string_literal()?));
                }
                Ok(args)
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported ORDER BY function: {other}"
            ))),
        }
    }

    /// `LIMIT n` 直後の省略可能な文末専用句 `USING MODE '<literal>'`（TASK-161・
    /// SQL-12）。`USING` は字句解析段階のキーワードではなく `Ident` のため、
    /// [`Parser::peek_ident_matches`] で文脈的（この位置限定）に判定する。続かなければ
    /// 句なし（`Ok(None)`）として扱う。
    /// `USING` の直後は `MODE`（大文字小文字非区別の文脈識別子）のみ許可し、それ以外
    /// （`PLAN`・`OPERATION_ID` 等）は fail-closed に拒否する。**この位置**
    /// （`ORDER BY ... LIMIT n` の直後）に限った制約であり、`USING PLAN(...)` 自体は
    /// `ORDER BY` の代替として別の位置（`WHERE` 直後、[`Parser::
    /// parse_using_plan_clause`]）で受理する（TASK-77・SQL-5）。ここで `PLAN` を
    /// 拒否するのは、`LIMIT` 後は既に `USING PLAN` の受理位置を過ぎているため（両者
    /// 併用・非規範形 `USING PLAN 'x'`（括弧なし）はいずれもここで `42601` へ落ちる）。
    /// 句を高々 1 回だけ消費するため、2 回目以降の `USING MODE ...` は本メソッドでは
    /// なく後続の [`Parser::expect_end_of_statement`] が「余剰トークン」として拒否する。
    fn parse_using_clause(&mut self) -> Result<Option<String>, SqlSurfaceError> {
        if !self.peek_ident_matches("USING") {
            return Ok(None);
        }
        self.advance();
        let name = self.expect_ident()?;
        if !name.eq_ignore_ascii_case("MODE") {
            return Err(SqlSurfaceError::unsupported(format!(
                "unsupported USING clause: {name}"
            )));
        }
        let value = self.expect_string_literal()?;
        Ok(Some(value))
    }

    /// `ORDER BY` の代わりに置ける文末専用句 `USING PLAN('<query>')`（TASK-77・
    /// SQL-5）の構造パース。呼び出し元 [`parse_select_shape`] が `WHERE`（省略可）
    /// の直後で `USING` 識別子を先読みして本メソッドへ分岐した後に呼ぶ（`ORDER BY`
    /// 経路は必ずキーワード `ORDER` から始まるため、先読み 1 トークンで衝突なく
    /// 判定できる＝`USING PLAN` と `ORDER BY` は構文上相互排他）。
    ///
    /// 受理する規範形は `PLAN('<文字列リテラル>')`（**括弧必須**の関数呼び出し形）
    /// のみ。以下はすべて構造上の許可リスト外として `42601`（`SqlSurfaceError::
    /// unsupported`）で拒否する（fail-closed）:
    /// - `PLAN` 以外の識別子（`unsupported USING clause`）
    /// - 括弧を伴わない形（例: 非規範形の `USING PLAN 'x'`）
    /// - パラメータ形式 `USING PLAN($1)`（拡張クエリプロトコル対応後の将来形式。
    ///   `$` は字句解析段階で [`LexError`] となり、本メソッドへ到達する前に
    ///   `validate_sql` の `tokenize` 呼び出しが `42601` へ写像する）
    /// - 文字列リテラル以外の引数・複数引数
    ///
    /// 空リテラルは [`SqlSurfaceError::invalid_input`]（`22000`）、
    /// [`MAX_USING_PLAN_LEN`] 超過は [`SqlSurfaceError::payload_too_large`]
    /// （`54000`）で拒否する。
    fn parse_using_plan_clause(&mut self) -> Result<String, SqlSurfaceError> {
        self.advance(); // `USING`（呼び出し元が `peek_ident_matches("USING")` で確認済み）
        let name = self.expect_ident()?;
        if !name.eq_ignore_ascii_case("PLAN") {
            return Err(SqlSurfaceError::unsupported(format!(
                "unsupported USING clause: {name}"
            )));
        }
        self.expect_punct('(')?;
        let value = self.expect_string_literal()?;
        self.expect_punct(')')?;
        validate_using_plan_question(&value)?;
        Ok(value)
    }

    /// `LIMIT <n>` の直後・文末に 1 箇所だけ許可する `HINT ORDER(<段>, <段>, <段>)`
    /// を解析する（TASK-76・SQL-7）。`HINT` は予約語化せず、この位置でのみ文脈依存で
    /// 認識する（次のトークンが識別子 `HINT`（大文字小文字不問）かつその次が
    /// `ORDER` キーワードの場合のみ消費する）。それ以外（`hint` を列名・テーブル名等の
    /// 通常の識別子として使う既存 SQL を含む）は何も消費せず `None` を返し、
    /// `HINT ORDER` 自体が省略可能な文法として扱う（公開 API・エラー契約の互換性、
    /// AGENTS.md P1）。段名は [`plan::parse_stage_name`] で識別子トークンを閉じた
    /// [`Stage`] へ写像し、未知の名前・個数不正・重複は許可リスト外として拒否する
    /// （`Stage`/`EvaluationOrder` を経由するため、パーサーを迂回しても不完全な順序が
    /// 下流へ渡らない）。
    fn parse_hint_order(&mut self) -> Result<Option<EvaluationOrder>, SqlSurfaceError> {
        let is_hint_ident =
            matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("HINT"));
        if !is_hint_ident
            || !matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Keyword(Keyword::Order))
            )
        {
            return Ok(None);
        }
        self.advance();
        self.expect_keyword(Keyword::Order)?;
        self.expect_punct('(')?;

        let mut stages: Vec<Stage> = Vec::new();
        loop {
            let name = self.expect_ident()?;
            let stage = plan::parse_stage_name(&name).ok_or_else(|| {
                SqlSurfaceError::unsupported(format!("unsupported HINT ORDER stage: {name}"))
            })?;
            stages.push(stage);
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect_punct(')')?;

        let order = EvaluationOrder::try_from_stages(&stages).map_err(|e| {
            SqlSurfaceError::unsupported(format!("invalid HINT ORDER permutation: {e:?}"))
        })?;
        Ok(Some(order))
    }

    /// 省略可能な単一末尾セミコロンの後に余剰トークンがあれば複数 statement と
    /// みなして拒否する。
    fn expect_end_of_statement(&mut self) -> Result<(), SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct(';'))) {
            self.advance();
        }
        if self.peek().is_some() {
            return Err(SqlSurfaceError::unsupported(
                "unexpected trailing tokens after statement (multiple statements are not supported)",
            ));
        }
        Ok(())
    }

    /// VALUES リストの 1 要素（文字列リテラルまたは数値リテラルのみ。関数呼び出し・
    /// 括弧・`NULL` キーワード等は許可リスト外）。
    /// `INTEGER`／`BIGINT` 列（Issue #881・TABLE-13・TASK-196）の負数リテラルを
    /// 受理するため、`-` の直後に `Number` トークンが続く形（`parse_having` の
    /// 単項マイナス処理と同じ規範。空白を挟む形も許容）だけを単項マイナスとして
    /// 認め、`InsertLiteral::Number("-<digits>")` へ正規化する。`- -1`・`-'x'`・
    /// `+1` はいずれも従来どおり構造的に受理しない（`42601`）。実際の値域検証・
    /// パースは束縛段（`sql::parser::bind_integer_literal`）が行う。
    fn expect_literal(&mut self) -> Result<InsertLiteral, SqlSurfaceError> {
        // F7（Issue #882 計画）: `REAL`/`DOUBLE PRECISION` の負リテラル
        // （`-1.5` 等）も同じ規範で受理する（`HAVING` 述語〔約 L1359〕と同じ
        // `['-'] <Number>` の文法を先読みで判定）。`Number` 以外（文字列・
        // ベクトルリテラル）の直前の `-` は許可リスト外のまま拒否する。
        if matches!(self.peek(), Some(Token::Punct('-'))) {
            self.advance();
            return match self.advance() {
                Some(Token::Number(n)) => Ok(InsertLiteral::Number(format!("-{n}"))),
                other => Err(SqlSurfaceError::unsupported(format!(
                    "expected numeric literal after unary minus, got {other:?}"
                ))),
            };
        }
        match self.advance() {
            Some(Token::StringLiteral(s)) => Ok(InsertLiteral::String(s.clone())),
            Some(Token::Number(n)) => Ok(InsertLiteral::Number(n.clone())),
            // BOOLEAN 列向けの `true`/`false` リテラル（大小無視、Issue #883・D-d）。
            // 従来 `Token::Ident` はここで一律拒否されていたため、この 2 語のみを
            // 追加で受理しても既存の受理範囲は変わらない（純粋な追加）。
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("true") => {
                Ok(InsertLiteral::Bool(true))
            }
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("false") => {
                Ok(InsertLiteral::Bool(false))
            }
            // 符号付き数値リテラル（`-1.5`・`+1.5` 等。TABLE-13〔検討中〕・TASK-197、
            // Issue #885・D6、および PR #1020 codex-review 指摘対応で `+` も追加）。
            // 字句解析器は `-`/`+` を独立した `Punct` として出すため、直後に
            // `Number` が続く場合のみ 1 つの符号付き数値リテラルとして受理する
            // （`InsertLiteral` の variant は増やさず `Number` へ符号を連結する。
            // `+` は `NUMERIC` の解析側〔`numeric::parse_for_column`〕がそのまま
            // 受理する表記のため符号文字を保持したまま連結する）。
            // 非 NUMERIC 列（`id`・TEXT・VECTOR・BOOLEAN）へ与えた場合、従来は
            // ここで構文エラー（`42601`）だったが、以降は束縛時の型不一致・
            // 不正値（`22000`）で拒否される（拒否されること自体は変わらない）。
            Some(&Token::Punct(sign @ ('-' | '+'))) => match self.advance() {
                Some(Token::Number(n)) => Ok(InsertLiteral::Number(format!("{sign}{n}"))),
                other => Err(SqlSurfaceError::unsupported(format!(
                    "expected literal value, got {other:?}"
                ))),
            },
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected literal value, got {other:?}"
            ))),
        }
    }

    /// `INSERT INTO <table> (<col>[, <col>]*)
    /// VALUES (<lit>[, <lit>]*)[, (<lit>[, <lit>]*)]*
    /// USING OPERATION_ID '<id>' [;]` を受理する（SQL-10・SQL-16、TASK-80・TASK-190）。
    /// 複数行 `VALUES` は行の繰り返し（`, (...)`）として受理し、行数は
    /// [`MAX_INSERT_ROWS_PER_STATEMENT`] を超えない（超過は `54000`）。各行の
    /// リテラル数は列数と一致する必要がある（不一致は行ごとに検出して拒否）。
    /// RETURNING・可視性ラベル指定は引き続き構造的に受理しない。
    fn parse_insert(&mut self) -> Result<ParsedInsertShape, SqlSurfaceError> {
        self.expect_contextual_keyword("INSERT")?;
        self.expect_contextual_keyword("INTO")?;
        let table_name = self.expect_ident()?;

        self.expect_punct('(')?;
        let mut columns = vec![self.expect_ident()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if columns.len() >= MAX_INSERT_COLUMNS {
                return Err(SqlSurfaceError::unsupported("too many INSERT columns"));
            }
            columns.push(self.expect_ident()?);
        }
        self.expect_punct(')')?;

        self.expect_contextual_keyword("VALUES")?;
        let mut rows: Vec<Vec<InsertLiteral>> = vec![self.parse_insert_values_row(&columns)?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if rows.len() >= MAX_INSERT_ROWS_PER_STATEMENT {
                return Err(SqlSurfaceError::payload_too_large(format!(
                    "INSERT statement exceeds the allowed row count ({MAX_INSERT_ROWS_PER_STATEMENT})"
                )));
            }
            rows.push(self.parse_insert_values_row(&columns)?);
        }

        // `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`（SQL-20・TASK-193、
        // Issue #872）: 任意句。`VALUES` 群の直後・文末専用句 `USING
        // OPERATION_ID` の**前**に置く（`USING OPERATION_ID` の後ろに書いた形は
        // `parse_operation_id_clause` が先に `USING ...` を消費してしまい、
        // 残った `ON CONFLICT ...` が `expect_end_of_statement` の余剰トークンと
        // して `42601` へ落ちる。優先順位の明示は `docs/design/sql-upsert.md`
        // 参照）。
        let on_conflict = self.parse_on_conflict_clause()?;

        // `RETURNING`（Issue #873・SQL-21）は `ON CONFLICT` の後・`USING
        // OPERATION_ID` 句の直前（`docs/design/sql-returning.md`「UPSERT（#872）
        // との併用」節。RETURNING の文法上の位置＝`USING OPERATION_ID` の直前
        // という契約を維持したまま `ON CONFLICT` を挿入する形）。
        let returning = self.parse_returning_clause()?;

        // 文末専用句の構造パースのみをここで行う（省略・明示 `NULL` はいずれも
        // `None`）。必須化の判定（`23502`）は `validate_insert` が
        // `LedgerMode::require` へ委譲し、この時点でまだ FROM/INTO テーブルの
        // カタログ照会を一切行っていない＝書き込みトランザクションは絶対に
        // 開始されていない段階で行われる（TASK-92・RECOVER-1）。全行の VALUES
        // 解析が終わった後に 1 回だけ呼ぶ（行ごとに書かれた場合は次の `(` の
        // 期待が外れて構文エラーとして自然に拒否される）。
        let operation_id = self.parse_operation_id_clause()?;

        Ok(ParsedInsertShape {
            table_name,
            columns,
            returning,
            rows,
            operation_id,
            on_conflict,
        })
    }

    /// `ON CONFLICT (id) DO NOTHING | DO UPDATE SET <col> = <value>[, ...]`
    /// を受理する（SQL-20・TASK-193、Issue #872）。任意句のため、先頭が
    /// 文脈的キーワード `ON` でなければ `None` を返し呼び出し元の位置を進めない。
    ///
    /// 対象列リストは `(id)` のみを受理する（複数列・他列名・`ON CONSTRAINT` は
    /// いずれも `42601`）。`DO UPDATE SET` の後ろに `WHERE` を続けた形・句の重複は
    /// 専用の判定を持たず、構造的に余剰トークンとして
    /// `Parser::expect_end_of_statement`（呼び出し元 [`parse_delete_statement_shape`]
    /// 等と同じ、本メソッドの直接の呼び出し元 [`Parser::parse_insert`] が最終的に
    /// 委ねる契約）が `42601` で拒否する。
    ///
    /// `id` は列識別子であり `ON`/`CONFLICT`/`DO`/`EXCLUDED`（文脈的キーワード。
    /// `eq_ignore_ascii_case` で判定）とは扱いが異なる。本 SQL 表層の字句解析は
    /// 識別子を大文字小文字保存のまま字句化し（`sql/lexer.rs` は識別子の
    /// 正規化を行わない）、列名解決（`schema.columns.iter().position(|c| &c.name
    /// == name)`。`sql/parser.rs`）・単一行 `DELETE`/`UPDATE` の `id` 指定形
    /// （[`Self::peek_single_row_delete_id`]・[`Self::parse_update_where`]）を含む
    /// SQL 表層全体で識別子は一貫して大文字小文字を区別する。したがって
    /// `ON CONFLICT (ID)` / `(Id)` は実在しない列名として `42601` になる
    /// （キーワードの大文字小文字非依存と矛盾しない、識別子側の既定契約
    /// どおりの挙動）。
    fn parse_on_conflict_clause(&mut self) -> Result<Option<OnConflictAction>, SqlSurfaceError> {
        if !self.peek_contextual_keyword("ON") {
            return Ok(None);
        }
        self.advance();
        self.expect_contextual_keyword("CONFLICT")?;
        self.expect_punct('(')?;
        match self.advance() {
            Some(Token::Ident(name)) if name == "id" => {}
            other => {
                return Err(SqlSurfaceError::unsupported(format!(
                    "ON CONFLICT target list must be (id), got {other:?}"
                )))
            }
        }
        self.expect_punct(')')?;
        self.expect_contextual_keyword("DO")?;
        if self.peek_contextual_keyword("NOTHING") {
            self.advance();
            return Ok(Some(OnConflictAction::DoNothing));
        }
        self.expect_contextual_keyword("UPDATE")?;
        self.expect_contextual_keyword("SET")?;
        let mut assignments = vec![self.parse_upsert_assignment()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if assignments.len() >= MAX_UPDATE_SET_ASSIGNMENTS {
                return Err(SqlSurfaceError::unsupported(
                    "too many ON CONFLICT DO UPDATE SET assignments",
                ));
            }
            assignments.push(self.parse_upsert_assignment()?);
        }
        Ok(Some(OnConflictAction::DoUpdate(assignments)))
    }

    /// `ON CONFLICT ... DO UPDATE SET` の 1 要素（`<col> = (EXCLUDED.<col> |
    /// <lit>)`）を構造パースする（SQL-20・TASK-193、Issue #872。
    /// `Parser::parse_update_assignment` と同じ形だが、右辺に
    /// [`Token::QualifiedIdent`]（`EXCLUDED.<col>`）も受理する点が異なる）。
    fn parse_upsert_assignment(&mut self) -> Result<(String, UpsertValue), SqlSurfaceError> {
        let column = self.expect_ident()?;
        self.expect_punct('=')?;
        let value = if let Some(Token::QualifiedIdent { qualifier, name }) = self.peek().cloned() {
            self.advance();
            if !qualifier.eq_ignore_ascii_case("EXCLUDED") {
                return Err(SqlSurfaceError::unsupported(format!(
                    "unsupported ON CONFLICT SET qualifier: {qualifier}"
                )));
            }
            UpsertValue::Excluded(name)
        } else {
            UpsertValue::Literal(self.expect_literal()?)
        };
        Ok((column, value))
    }

    /// `VALUES` の 1 行分 `(<lit>[, <lit>]*)` を解析し、リテラル数が列数と一致する
    /// ことを検証する（SQL-16、TASK-190）。複数行形・単一行形のいずれからも
    /// 呼ばれる共有ヘルパ。
    fn parse_insert_values_row(
        &mut self,
        columns: &[String],
    ) -> Result<Vec<InsertLiteral>, SqlSurfaceError> {
        self.expect_punct('(')?;
        let mut values = vec![self.expect_literal()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if values.len() >= MAX_INSERT_COLUMNS {
                return Err(SqlSurfaceError::unsupported("too many INSERT values"));
            }
            values.push(self.expect_literal()?);
        }
        self.expect_punct(')')?;

        if columns.len() != values.len() {
            return Err(SqlSurfaceError::unsupported(format!(
                "INSERT column count {} does not match value count {}",
                columns.len(),
                values.len()
            )));
        }

        Ok(values)
    }

    /// `DELETE FROM <table> WHERE <where> USING OPERATION_ID '<id>' [;]` を
    /// 受理する（SQL-18・TASK-191 の単一行・`id` 完全一致形、Issue #870・
    /// TASK-192・SQL-19 の述語形の両方をここで構造判定する）。`WHERE` 句の
    /// 省略は両形式共通で許可リスト外（`Keyword::Where` を必須とする）。
    ///
    /// `WHERE` 直後が [`Self::peek_single_row_delete_id`] に一致する場合
    /// （`id = <number>` の直後が文末／`;`／`USING`）のみ [`ParsedDeleteWhere::RowId`]
    /// とし、それ以外はすべて汎用 `WHERE` パーサー（[`Self::parse_where`]。
    /// `SELECT`／集計／広域取得と共有）へ委譲して [`ParsedDeleteWhere::Predicates`]
    /// とする。AST 分類ではなくこの狭いトークン先読みにすることで、単一行形
    /// （[`ValidatedDelete`]）の受理範囲を SQL-18・TASK-191 の既存契約から
    /// 1 バイトも広げない（`parse_where` は `id = 1` と `id = 1 AND ...` を
    /// 区別なく式として正規化できてしまうため）。
    fn parse_delete(&mut self) -> Result<ParsedDeleteShape, SqlSurfaceError> {
        self.expect_contextual_keyword("DELETE")?;
        self.expect_keyword(Keyword::From)?;
        let table_name = self.expect_ident()?;

        self.expect_keyword(Keyword::Where)?;

        let where_clause = if self.peek_single_row_delete_id() {
            self.advance(); // "id"
            self.advance(); // '='
            let id_literal = self.expect_number()?;
            ParsedDeleteWhere::RowId { id_literal }
        } else {
            let predicates = self.parse_where()?;
            ParsedDeleteWhere::Predicates(predicates)
        };

        // `RETURNING`（Issue #873・SQL-21）は `USING OPERATION_ID` 句の直前。
        let returning = self.parse_returning_clause()?;
        let operation_id = self.parse_operation_id_clause()?;

        Ok(ParsedDeleteShape {
            table_name,
            where_clause,
            operation_id,
            returning,
        })
    }

    /// `WHERE` 直後（消費前）が単一行・`id` 完全一致指定形（`id = <number>` の
    /// 直後のトークンが文末／`;`／文脈的キーワード `USING`／`RETURNING`〔Issue
    /// #873・SQL-21〕）かどうかを、トークンを一切消費せずに判定する（Issue
    /// #870・§3.2）。添字アクセス（`[]`）は使わず `self.tokens.get` のみで
    /// 先読みする（untrusted 入力経由のパーサーの原則。
    /// `.claude/rules/coding-rust.md`）。`RETURNING` を終端として受理しないと
    /// `WHERE id = 1 RETURNING ...` が `parse_where`（述語形）へ流れてしまい、
    /// `RETURNING` を実行結線済みの単一行形（[`ValidatedDelete`]）で使えなく
    /// なる。
    fn peek_single_row_delete_id(&self) -> bool {
        let is_id_eq_number = matches!(
            self.tokens.get(self.pos),
            Some(Token::Ident(name)) if name == "id"
        ) && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('=')))
            && matches!(self.tokens.get(self.pos + 2), Some(Token::Number(_)));
        if !is_id_eq_number {
            return false;
        }
        match self.tokens.get(self.pos + 3) {
            None => true,
            Some(Token::Punct(';')) => true,
            Some(Token::Ident(w)) => {
                w.eq_ignore_ascii_case("USING") || w.eq_ignore_ascii_case("RETURNING")
            }
            _ => false,
        }
    }

    /// `UPDATE <table> SET <col> = <lit>[, <col> = <lit>]* WHERE <where_form>
    /// [RETURNING <投影>] USING OPERATION_ID '<id>' [;]` を受理する。`WHERE`
    /// 句は [`Self::parse_update_where`] が単一行・id 指定形（SQL-17、
    /// TASK-191）と述語形（SQL-19、TASK-192）を振り分ける（[`UpdateWhereForm`]
    /// のドキュメント参照）。`RETURNING`（Issue #873・SQL-21）は構造として
    /// 受理するが、`UPDATE` の実行結線（#865）が未着手のため呼び出し元
    /// （[`validate_update_tokens`]・[`validate_update_form_tokens`]）が
    /// 一律 `42601` で拒否する単一のチョークポイントを持つ。振り分け後の
    /// 受理判定（id 指定形以外は許可しない等）も同じ呼び出し元の責務とし、
    /// 本メソッドは構造パースのみを行う。複数テーブル・サブクエリは本メソッド
    /// が生成できる文法にそもそも存在しないため構造的に受理しない（個別の
    /// 拒否コードを持たず、`expect_end_of_statement` が余剰トークンとして
    /// `42601` へ落とす）。
    fn parse_update(&mut self) -> Result<ParsedUpdateShape, SqlSurfaceError> {
        self.expect_contextual_keyword("UPDATE")?;
        let table_name = self.expect_ident()?;

        self.expect_ident_matching("SET")?;
        let mut assignments = vec![self.parse_update_assignment()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if assignments.len() >= MAX_UPDATE_SET_ASSIGNMENTS {
                return Err(SqlSurfaceError::unsupported(
                    "too many UPDATE SET assignments",
                ));
            }
            assignments.push(self.parse_update_assignment()?);
        }

        self.expect_keyword(Keyword::Where)?;
        let where_form = self.parse_update_where()?;

        // `RETURNING`（Issue #873・SQL-21）は `USING OPERATION_ID` 句の直前。
        // 構造パースのみ行い、実行結線（#865）未着手のため常に `42601` で
        // 拒否する判定は呼び出し元（`validate_update_tokens`／
        // `validate_update_form_tokens`）のチョークポイントに委ねる。
        let returning = self.parse_returning_clause()?;

        // 文末専用句の構造パースのみをここで行う（INSERT と同じ順序契約。
        // 必須化の判定は `validate_update`／`validate_update_form` が
        // `LedgerMode::require` へ委譲する）。
        let operation_id = self.parse_operation_id_clause()?;

        Ok(ParsedUpdateShape {
            table_name,
            assignments,
            where_form,
            operation_id,
            returning,
        })
    }

    /// `UPDATE` の `WHERE`（`Keyword::Where` 消費済み）を [`UpdateWhereForm`] へ
    /// 振り分ける（SQL-17・SQL-19、TASK-191・TASK-192）。
    ///
    /// 判定は決定的: 直後の 3 トークンが `Ident("id")`・`Punct('=')`・
    /// `Token::Number` で、かつその次のトークンが文末（`None`）・`Punct(';')`・
    /// 文脈的キーワード `USING` のいずれかである場合に限り [`UpdateWhereForm::Id`]
    /// （単一行・id 指定形）とし、それ以外はすべて位置を巻き戻して
    /// [`Self::parse_where`]（`SELECT`・集計 `SELECT`・広域取得 `SELECT` と同一の
    /// 許可述語列表現）で [`UpdateWhereForm::Predicates`] を構築する。`id = 'x'`
    /// （数値以外の id 比較）・`lang = 'ja'` は先頭 3 トークン一致条件（`Ident("id")`・
    /// `Punct('=')`・`Number`）そのものに外れるため述語形へ流れる。`id = 5 AND
    /// lang = 'ja'` は先頭 3 トークンには一致するが、4 番目のトークンが終端
    /// （`None`・`Punct(';')`・`USING`）ではなく `AND` であるため 4 番目の条件で
    /// 述語形へ流れる——`validate_update`（既存の id 指定形専用エントリ
    /// ポイント）はこの結果が `Predicates` なら `42601` で拒否することで、
    /// 旧来の狭い受理形をそのまま維持する（後方互換）。
    fn parse_update_where(&mut self) -> Result<UpdateWhereForm, SqlSurfaceError> {
        let start = self.pos;
        let is_id_simple_prefix = matches!(self.tokens.get(self.pos), Some(Token::Ident(name)) if name == "id")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('=')))
            && matches!(self.tokens.get(self.pos + 2), Some(Token::Number(_)));
        let has_terminator = matches!(
            self.tokens.get(self.pos + 3),
            None | Some(Token::Punct(';'))
        ) || matches!(self.tokens.get(self.pos + 3), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("USING"));

        if is_id_simple_prefix && has_terminator {
            self.advance(); // "id"
            self.advance(); // '='
            let id_literal = self.expect_number()?;
            return Ok(UpdateWhereForm::Id(id_literal));
        }

        self.pos = start;
        let predicates = self.parse_where()?;
        Ok(UpdateWhereForm::Predicates(predicates))
    }

    /// UPDATE の SET 句の 1 要素（`<col> = <lit>`）を構造パースする。
    fn parse_update_assignment(&mut self) -> Result<(String, InsertLiteral), SqlSurfaceError> {
        let column = self.expect_ident()?;
        self.expect_punct('=')?;
        let literal = self.expect_literal()?;
        Ok((column, literal))
    }

    /// `RETURNING <投影>`（Issue #873・SQL-21）の構造パースのみを行う。書き込み系
    /// 文（`INSERT`／`DELETE`／`UPDATE`）の `USING OPERATION_ID` 句の**直前**
    /// にのみ置ける（`parse_insert`／`parse_delete`／`parse_update` が
    /// [`Self::parse_operation_id_clause`] の直前で呼ぶ。それより後ろに置いた
    /// `RETURNING` は `expect_end_of_statement` が余剰トークンとして `42601` へ
    /// 落とす）。省略時は `Ok(None)`。投影は `SELECT` と同じ許可形状
    /// （[`Self::parse_select_list`]）を再利用するが、関数呼び出し項目
    /// （[`Projection::Items`]）はここでは受理しない（`RETURNING` が返す行は
    /// 書き込み結果そのものであり、式評価のためのセッション UDF レジストリを
    /// 持たないこのパーサー段では意味を持たないため）。
    fn parse_returning_clause(&mut self) -> Result<Option<Projection>, SqlSurfaceError> {
        if !self.peek_contextual_keyword("RETURNING") {
            return Ok(None);
        }
        self.advance();
        let projection = self.parse_select_list()?;
        if matches!(projection, Projection::Items(_)) {
            return Err(SqlSurfaceError::unsupported(
                "RETURNING does not support function-call items",
            ));
        }
        Ok(Some(projection))
    }

    /// 文末専用句 `USING OPERATION_ID '<id>'`（SQL-10、TASK-80）の構造パースのみを
    /// 行う（値の意味論的検証は [`OperationId::parse`]）。句の省略・明示
    /// `USING OPERATION_ID NULL`（大小無視。字句解析上は `Token::Ident("NULL")`）は
    /// いずれも `Ok(None)` として返し、`23502` への判定はここでは行わない
    /// （TASK-92・RECOVER-1: 必須化の可否はサーバー構成 `LedgerMode` が決める。
    /// 呼び出し元 [`validate_insert`] が `LedgerMode::require` へ委譲する）。`USING` の
    /// 後に `OPERATION_ID` キーワードが続かない形（`$n` プレースホルダ由来の字句解析
    /// 拒否を含む）・`OPERATION_ID` に文字列リテラルでも `NULL` でもない形
    /// （数値・他の識別子等）は許可リスト外として `42601` へ落ちる
    /// （`expect_contextual_keyword`/`expect_string_literal` が `UnsupportedSyntax` を
    /// 返す）。
    fn parse_operation_id_clause(&mut self) -> Result<Option<OperationId>, SqlSurfaceError> {
        if self.peek_contextual_keyword("USING") {
            self.advance();
            self.expect_contextual_keyword("OPERATION_ID")?;
            if self.peek_contextual_keyword("NULL") {
                self.advance();
                return Ok(None);
            }
            let raw = self.expect_string_literal()?;
            OperationId::parse(&raw).map(Some)
        } else {
            Ok(None)
        }
    }

    /// `TRUNCATE TABLE <table> USING OPERATION_ID '<id>' [;]` の単一テーブル形
    /// のみを受理する（SQL-22、TASK-195）。複数テーブル指定・`CASCADE`／
    /// `RESTART IDENTITY` 等の PostgreSQL 拡張句は構造的に受理しない
    /// （`expect_end_of_statement` が余剰トークンとして `42601` で拒否する）。
    fn parse_truncate(&mut self) -> Result<ParsedTruncateShape, SqlSurfaceError> {
        self.expect_contextual_keyword("TRUNCATE")?;
        self.expect_contextual_keyword("TABLE")?;
        let table_name = self.expect_ident()?;

        // 文末専用句の構造パースのみをここで行う（`parse_insert` と同じ設計。
        // 必須化の判定は `validate_truncate` が `mode.require` へ委譲する。
        // TASK-92・RECOVER-1 と同じ理由で、この時点ではまだカタログ照会を
        // 一切行っておらず書き込みトランザクションは開始されていない）。
        let operation_id = self.parse_operation_id_clause()?;

        Ok(ParsedTruncateShape {
            table_name,
            operation_id,
        })
    }

    /// `ALTER TABLE <table> ADD COLUMN <column> <type> [;]` の単一列追加形の
    /// みを受理する（TASK-202・SQL-23。Issue #900）。`ALTER`／`TABLE`／`ADD`／
    /// `COLUMN` は `lexer::Keyword` へ含めない設計方針（`lexer.rs` の
    /// モジュールドキュメント参照）のため、いずれも `expect_contextual_keyword`
    /// で文脈的に照合する。`IF NOT EXISTS`・複数 `ADD`・列制約・`DROP COLUMN`／
    /// `ALTER COLUMN`・`USING OPERATION_ID` はいずれも構造的に受理しない
    /// （`expect_end_of_statement` が余剰トークンとして `42601` で拒否するか、
    /// `ADD` の直後に `COLUMN` 以外が続いた時点で `expect_contextual_keyword`
    /// が拒否する）。
    fn parse_alter_table_add_column(
        &mut self,
    ) -> Result<ParsedAlterTableAddColumnShape, SqlSurfaceError> {
        self.expect_contextual_keyword("ALTER")?;
        self.expect_contextual_keyword("TABLE")?;
        let table_name = self.expect_ident()?;
        self.expect_contextual_keyword("ADD")?;
        self.expect_contextual_keyword("COLUMN")?;
        let column_name = self.expect_ident()?;
        // 予約列名（`id`／`tenant_id`／`visibility`。ASCII の大文字小文字を無視）は
        // `parse_create_table_column` と同じく構造検証段階で拒否する（`sql::parser`
        // がこれら 3 語を疑似列・RLS 内部列として扱う契約と整合させ、DDL で
        // これらを隠蔽する列を作らせない。fail-closed。security.md「アクセス
        // 制御の不備」対応）。カタログを参照しない判定のため権限ゲートより
        // 前に置いても存在オラクルにならない。`check`／`constraint`（Issue #906）も
        // `CREATE TABLE` と同じ予約列名として揃える（同じ列定義を `CREATE TABLE` で
        // 再現できない列を ALTER 経由で作らせない）。
        if column_name.eq_ignore_ascii_case("id")
            || column_name.eq_ignore_ascii_case("tenant_id")
            || column_name.eq_ignore_ascii_case("visibility")
            || column_name.eq_ignore_ascii_case("check")
            || column_name.eq_ignore_ascii_case("constraint")
        {
            return Err(SqlSurfaceError::unsupported(format!(
                "column name {column_name:?} is reserved"
            )));
        }
        // `sql::allowlist::Parser` の内部状態（`tokens`／`pos`）を共有する
        // 独立実装（`sql::ddl_column_type` モジュールドキュメント参照）。
        let column_type =
            crate::sql::ddl_column_type::parse_column_type_name(self.tokens, &mut self.pos)?;

        Ok(ParsedAlterTableAddColumnShape {
            table_name,
            column_name,
            column_type,
        })
    }

    /// `CREATE TABLE <table> (<col> <type>[, <col> <type>]*) [;]`（SQL-23・
    /// TASK-85、Issue #899）の許可形状。カタログ照会は行わない（`sql::ddl`
    /// モジュールドキュメント・[`ValidatedCreateTable`] 参照）。列数の上限判定
    /// （`MAX_CREATE_TABLE_COLUMNS`）は、列定義 1 個を実際にパースする直前に
    /// 「既に確定した列数」だけで行う。表制約（`PRIMARY KEY (...)`・
    /// `UNIQUE (...)`）は列を追加しないため判定の対象外で、制約が列リスト中の
    /// どこ（先頭・中間・末尾）にあっても判定結果が変わらない（位置非依存）。
    /// ちょうど上限数の列は受理し、上限を超える列はパース・アロケーションの
    /// 前に `54000` で拒否する（`.claude/rules/security.md`「不安全な設計｜
    /// 無制限リソース確保（DoS）」対応。PR #1044 レビューの off-by-one 是正と、
    /// 表制約の直後の列が上限判定をすり抜けていた不具合〔Issue #905 レビュー
    /// 指摘〕の是正を兼ねる。カンマ直後の先読みで表制約を除外する旧判定は
    /// 表制約の後ろに続く列を数え漏らしていたため撤去した）。
    fn parse_create_table(&mut self) -> Result<ValidatedCreateTable, SqlSurfaceError> {
        self.expect_contextual_keyword("CREATE")?;
        self.expect_contextual_keyword("TABLE")?;
        let table_name = self.expect_ident()?;
        crate::catalog::validate_identifier(&table_name)
            .map_err(|e| SqlSurfaceError::unsupported(format!("invalid table name: {e}")))?;

        self.expect_punct('(')?;
        let mut columns: Vec<ColumnDef> = Vec::new();
        // `PRIMARY KEY` 宣言（列制約・表制約のいずれか一方のみ。TABLE-16・
        // TASK-204、Issue #903）。列名の存在検証・`id` 混在検査・nullable の
        // 強制は列リスト全体の構文判定を終えた後（このループを抜けた後）に
        // まとめて行う（表制約は宣言順に関わらず任意位置の列を参照できる
        // ため）。
        let mut primary_key: Option<Vec<String>> = None;
        // UNIQUE 制約（列制約・表制約。TABLE-16・TASK-204、Issue #905）。参照列の
        // 解決は `finalize_unique_constraints` が列リスト全体の構文判定後に行う。
        let mut unique_constraints: Vec<Vec<String>> = Vec::new();
        // `CHECK` 制約（列制約・表制約。TABLE-16・TASK-204、Issue #906）。意味論
        // 検証は `sql::check_constraint::validate_and_build`（`sql::ddl` から）が行う。
        let mut checks: Vec<ParsedCheck> = Vec::new();
        // `FOREIGN KEY` 制約（列制約・表制約。TABLE-17・TASK-205、Issue #907）。
        // 参照元列の解決は `finalize_foreign_keys`、参照先の解決・照合は
        // `catalog::Storage::create_table` が行う。
        let mut foreign_keys: Vec<crate::catalog::ForeignKeyDef> = Vec::new();
        loop {
            // 表制約 `PRIMARY KEY (<col>[, <col>]*)` は要素先頭が文脈的識別子
            // `PRIMARY` かつ次のトークンが `KEY` の場合にのみ判定する
            // （`KEY` は列型キーワードではないため、列名 `primary` との構文上の
            // 曖昧さは生じない）。
            if self.peek_ident_matches("PRIMARY") && self.peek_ident_matches_at(1, "KEY") {
                if primary_key.is_some() {
                    return Err(SqlSurfaceError::unsupported(
                        "CREATE TABLE must declare at most one PRIMARY KEY",
                    ));
                }
                primary_key = Some(self.parse_primary_key_table_constraint()?);
            } else if self.peek_ident_matches("UNIQUE")
                && matches!(
                    self.tokens.get(self.pos.saturating_add(1)),
                    Some(Token::Punct('('))
                )
            {
                // 表制約 `UNIQUE (<col>[, <col>]*)`。要素先頭が文脈的識別子
                // `UNIQUE` かつ次のトークンが `(` の場合にのみ判定する（列名
                // `unique` の列定義は次が列型キーワードになるため曖昧さはない）。
                if unique_constraints.len() >= crate::catalog::MAX_UNIQUE_CONSTRAINTS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many UNIQUE constraints in CREATE TABLE",
                    ));
                }
                unique_constraints.push(self.parse_unique_table_constraint()?);
            } else if self.peek_ident_matches("FOREIGN") && self.peek_ident_matches_at(1, "KEY") {
                // 表制約 `FOREIGN KEY (<col>[, <col>]*) REFERENCES ...`（TABLE-17・
                // TASK-205、Issue #907）。`KEY` は列型キーワードではないため、列名
                // `foreign` の列定義との曖昧さは生じない。列を追加しないため列数上限の
                // 判定対象外（位置非依存）。件数上限はパース前に判定する。
                if foreign_keys.len() >= crate::catalog::MAX_FOREIGN_KEYS_PER_TABLE {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many FOREIGN KEY constraints in CREATE TABLE",
                    ));
                }
                foreign_keys.push(self.parse_foreign_key_table_constraint()?);
            } else if self.peek_check_clause_start() {
                // 表制約 `[CONSTRAINT <name>] CHECK (...)`（TABLE-16・TASK-204、
                // Issue #906）。列を追加しないため列数上限の判定対象外（位置
                // 非依存）。件数上限はパース前に判定する（UNIQUE と同じ）。
                if checks.len() >= crate::catalog::MAX_CHECK_CONSTRAINTS_PER_TABLE {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many CHECK constraints in CREATE TABLE",
                    ));
                }
                let (name, predicates) = self.parse_check_clause()?;
                checks.push(ParsedCheck {
                    name,
                    column: None,
                    predicates,
                });
            } else {
                // 列定義を 1 つ確定させる前に、確定済みの列数だけで上限を判定する
                // （位置非依存。`parse_create_table` のドキュメント参照）。
                if columns.len() >= MAX_CREATE_TABLE_COLUMNS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many columns in CREATE TABLE",
                    ));
                }
                let remaining_checks =
                    crate::catalog::MAX_CHECK_CONSTRAINTS_PER_TABLE.saturating_sub(checks.len());
                let parsed = self.parse_create_table_column(&columns, remaining_checks)?;
                checks.extend(parsed.checks);
                if parsed.primary_key {
                    if primary_key.is_some() {
                        return Err(SqlSurfaceError::unsupported(
                            "CREATE TABLE must declare at most one PRIMARY KEY",
                        ));
                    }
                    primary_key = Some(vec![parsed.column.name.clone()]);
                }
                if parsed.unique {
                    if unique_constraints.len() >= crate::catalog::MAX_UNIQUE_CONSTRAINTS {
                        return Err(SqlSurfaceError::payload_too_large(
                            "too many UNIQUE constraints in CREATE TABLE",
                        ));
                    }
                    unique_constraints.push(vec![parsed.column.name.clone()]);
                }
                if let Some((parent_table, parent_columns)) = parsed.references {
                    if foreign_keys.len() >= crate::catalog::MAX_FOREIGN_KEYS_PER_TABLE {
                        return Err(SqlSurfaceError::payload_too_large(
                            "too many FOREIGN KEY constraints in CREATE TABLE",
                        ));
                    }
                    foreign_keys.push(crate::catalog::ForeignKeyDef::new(
                        vec![parsed.column.name.clone()],
                        parent_table,
                        parent_columns,
                    ));
                }
                columns.push(parsed.column);
            }
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect_punct(')')?;
        // 列定義は 1 件以上必須（表制約 `PRIMARY KEY`／`UNIQUE`／`CHECK` だけの
        // 列リストは列を持たないテーブルになる）。カタログの `validate_schema` も
        // 拒否するが、構文段の不変条件としてここで `42601` にする（fail-closed。
        // PR #1055 Bugbot 指摘: 表制約 `CHECK` の追加で列なしの列リストが構文段を
        // 通過し得た）。
        if columns.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "CREATE TABLE requires at least one column definition",
            ));
        }

        let primary_key = finalize_primary_key(primary_key, &mut columns)?;
        let unique_constraints = finalize_unique_constraints(unique_constraints, &columns)?;
        let foreign_keys = finalize_foreign_keys(foreign_keys, &columns)?;

        Ok(ValidatedCreateTable {
            table_name,
            columns,
            primary_key,
            unique_constraints,
            checks,
            foreign_keys,
        })
    }

    /// 現在位置が `CREATE TABLE` の列リスト要素としての `CHECK` 句の開始位置
    /// （`CHECK (`、または `CONSTRAINT`）であるかを消費せずに判定する（TABLE-16・
    /// TASK-204、Issue #906）。`CHECK`／`CONSTRAINT` はキーワード化しない
    /// （`sql::lexer` のモジュール方針。この位置でのみ文脈的に照合する）。
    ///
    /// 曖昧さの排除: 列名 `check`／`constraint` は [`Self::parse_create_table_column`]
    /// が予約語として `42601` で拒否するため、要素先頭の `CONSTRAINT` は常に制約
    /// 宣言の開始として扱ってよい（`CONSTRAINT` の後ろが `<name> CHECK (` の形で
    /// なければ [`Self::parse_check_clause`] が `42601` で拒否する）。さらに制約名が
    /// 列型キーワード（[`CREATE_TABLE_COLUMN_TYPE_KEYWORDS`]）と一致する場合も
    /// 拒否するため、`constraint TEXT CHECK (...)` のように「列 `constraint`
    /// の定義」とも「制約名 `TEXT` の表制約」とも読める入力は、どちらの解釈でも
    /// 黙って受理されず必ず `42601` になる（サイレントなスキーマ改変を防ぐ
    /// fail-closed。PR レビュー指摘）。
    fn peek_check_clause_start(&self) -> bool {
        (self.peek_ident_matches("CHECK")
            && matches!(
                self.tokens.get(self.pos.saturating_add(1)),
                Some(Token::Punct('('))
            ))
            || self.peek_ident_matches("CONSTRAINT")
    }

    /// [`Self::peek_check_clause_start`] が `true` を返した位置から
    /// `[CONSTRAINT <name>] CHECK ( <述語> )` を消費する（TABLE-16・TASK-204、
    /// Issue #906）。括弧内の述語文法は `WHERE` と同一（[`Self::parse_check_body`]）。
    /// 制約名は `catalog::validate_identifier` で検証し、列型キーワードとの一致は
    /// 拒否する（`peek_check_clause_start` の「曖昧さの排除」参照）。同一テーブル内の
    /// 制約名の重複は `sql::check_constraint::validate_and_build` が検査する。
    fn parse_check_clause(
        &mut self,
    ) -> Result<(Option<String>, Vec<WherePredicate>), SqlSurfaceError> {
        let name = if self.peek_ident_matches("CONSTRAINT") {
            self.advance();
            let n = self.expect_ident()?;
            crate::catalog::validate_identifier(&n).map_err(|e| {
                SqlSurfaceError::unsupported(format!("invalid constraint name: {e}"))
            })?;
            if CREATE_TABLE_COLUMN_TYPE_KEYWORDS
                .iter()
                .any(|kw| n.eq_ignore_ascii_case(kw))
            {
                return Err(SqlSurfaceError::unsupported(format!(
                    "constraint name {n:?} collides with a column type keyword"
                )));
            }
            Some(n)
        } else {
            None
        };
        self.expect_ident_matching("CHECK")?;
        self.expect_punct('(')?;
        let predicates = self.parse_check_body()?;
        self.expect_punct(')')?;
        if where_predicates_contain_or(&predicates) {
            // TASK-208・SQL-24（Issue #912）の対象は読み取り文・書き込み文の
            // `WHERE` のみで、`CHECK (...)` 本体（TABLE-16）は含まない。
            // 明示的に `42601` で拒否する（将来解禁する場合に備え、
            // `render_predicate` 側は既に `Or` を網羅描画できる）。
            return Err(SqlSurfaceError::unsupported(
                "OR is not supported inside a CHECK clause",
            ));
        }
        Ok((name, predicates))
    }

    /// `CREATE TABLE` の列 1 個ぶんの許可形状: `<col> (TEXT | VECTOR '(' <N> ')')
    /// [PRIMARY KEY]`（Issue #899・#903）。`TEXT`／`VECTOR`／`PRIMARY`／`KEY` は
    /// `lexer::Keyword` へ含めない（`SET`・`CREATE` と同方針。statement 中の
    /// この位置でのみ文脈的キーワードとして照合する）。予約列名（`id`／
    /// `tenant_id`／`visibility`。ASCII の大文字小文字を無視して照合）・
    /// 同一文内の列名重複はここで拒否する（`sql::parser` がこれら 3 語を
    /// 疑似列・RLS 内部列として扱う契約と整合させ、SQL 表層の DDL でこれらを
    /// 隠蔽する列を作らせない。fail-closed。`.claude/rules/security.md`
    /// 「アクセス制御の不備」対応）。`VECTOR` の次元は `Token::Number` を `u32` へ
    /// checked に parse する（先頭 `-`・小数点・オーバーフローはいずれも
    /// `UnsupportedSyntax` へ落ちる。範囲（`1..=MAX_VECTOR_DIM`）検証自体は
    /// `catalog::validate_schema`（`sql::ddl::execute_create_table` が書き込み
    /// トランザクション内で通す）に委ねる——同じ `42601` 分類のため二重実装しない）。
    /// 戻り値は列定義と、列制約 `PRIMARY KEY`・`UNIQUE`（Issue #905）が付与
    /// されたかどうか（`finalize_primary_key` 呼び出し前の nullable 書き換えは
    /// 行わない）。`UNIQUE` は `VECTOR` 列には付与できない（一意キー許可型外。
    /// 構文段階で `42601`）。
    fn parse_create_table_column(
        &mut self,
        existing: &[ColumnDef],
        max_checks: usize,
    ) -> Result<ParsedCreateTableColumn, SqlSurfaceError> {
        let name = self.expect_ident()?;
        crate::catalog::validate_identifier(&name)
            .map_err(|e| SqlSurfaceError::unsupported(format!("invalid column name: {e}")))?;
        // `check`／`constraint`（TABLE-16・TASK-204、Issue #906）も予約列名として
        // 拒否する。列リスト要素の先頭に現れる `CHECK (`／`CONSTRAINT` を常に
        // 制約宣言として解釈できるようにし、列定義との曖昧さを構造的に排除する
        // （[`Self::peek_check_clause_start`] 参照。PostgreSQL でも両語は予約語）。
        if name.eq_ignore_ascii_case("id")
            || name.eq_ignore_ascii_case("tenant_id")
            || name.eq_ignore_ascii_case("visibility")
            || name.eq_ignore_ascii_case("check")
            || name.eq_ignore_ascii_case("constraint")
        {
            return Err(SqlSurfaceError::unsupported(format!(
                "column name {name:?} is reserved"
            )));
        }
        if existing.iter().any(|c| c.name == name) {
            return Err(SqlSurfaceError::duplicate_column(name));
        }

        let mut unique = false;
        let column = if self.peek_ident_matches("TEXT") {
            self.advance();
            let constraints = self.parse_column_constraints()?;
            unique = constraints.unique;
            let default = match constraints.default {
                None => None,
                Some(InsertLiteral::String(s)) => {
                    if s.len() > MAX_COLUMN_DEFAULT_LEN {
                        return Err(SqlSurfaceError::payload_too_large(format!(
                            "column {name:?} DEFAULT literal exceeds length limit"
                        )));
                    }
                    Some(ColumnDefault::Text(s))
                }
                Some(_) => {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "column {name:?} DEFAULT expects a text literal"
                    )))
                }
            };
            let mut column = ColumnDef::new(name, ColumnType::Text, !constraints.not_null);
            if let Some(default) = default {
                column = column.with_default(default);
            }
            column
        } else if self.peek_ident_matches("VECTOR") {
            self.advance();
            self.expect_punct('(')?;
            let raw_dim = self.expect_number()?;
            let dim: u32 = raw_dim.parse().map_err(|_| {
                SqlSurfaceError::unsupported(format!("invalid VECTOR dimension: {raw_dim:?}"))
            })?;
            self.expect_punct(')')?;
            let constraints = self.parse_column_constraints()?;
            if constraints.default.is_some() {
                return Err(SqlSurfaceError::unsupported(format!(
                    "column {name:?}: VECTOR columns do not support DEFAULT"
                )));
            }
            if constraints.unique {
                return Err(SqlSurfaceError::unsupported(format!(
                    "column {name:?}: VECTOR columns do not support UNIQUE"
                )));
            }
            // VECTOR は常に非 nullable。`NOT NULL` の明示指定は冗長だが受理する
            // （`constraints.not_null` の値に関わらず `nullable = false` のまま）。
            ColumnDef::new(name, ColumnType::Vector(dim), false)
        } else if self.peek_ident_matches("INTEGER") || self.peek_ident_matches("BIGINT") {
            // `INTEGER`／`BIGINT`（TABLE-17・TASK-205、Issue #907。`id` を参照する
            // `FOREIGN KEY` の参照元列に使う整数型）。列制約は `TEXT` と同じく
            // `NOT NULL`／`DEFAULT <数値リテラル>`／`UNIQUE` を受理する（数値の範囲・
            // 形式は `sql::parser::bind_literal_for_column` が束縛時に検証する）。
            let ty = if self.peek_ident_matches("INTEGER") {
                ColumnType::Integer
            } else {
                ColumnType::BigInt
            };
            self.advance();
            let constraints = self.parse_column_constraints()?;
            unique = constraints.unique;
            let default = match constraints.default {
                None => None,
                Some(InsertLiteral::Number(n)) => {
                    if n.len() > MAX_COLUMN_DEFAULT_LEN {
                        return Err(SqlSurfaceError::payload_too_large(format!(
                            "column {name:?} DEFAULT literal exceeds length limit"
                        )));
                    }
                    Some(ColumnDefault::Number(n))
                }
                Some(_) => {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "column {name:?} DEFAULT expects a numeric literal"
                    )))
                }
            };
            let mut column = ColumnDef::new(name, ty, !constraints.not_null);
            if let Some(default) = default {
                column = column.with_default(default);
            }
            column
        } else {
            return Err(SqlSurfaceError::unsupported(
                "expected column type TEXT, VECTOR(<dim>), INTEGER or BIGINT",
            ));
        };

        // 列制約 `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）。`CONSTRAINT
        // <name> PRIMARY KEY` 形・`REFERENCES` はいずれも許可リスト外のまま
        // （受理しない。`CONSTRAINT <name>` は後段の `CHECK` にのみ前置できる）。`NOT NULL`／`DEFAULT`（Issue #904）・`UNIQUE`
        // （Issue #905）は列型キーワードの直後（`parse_column_constraints` 内）で
        // 先に受理済みで、`PRIMARY KEY` は
        // その後段の独立した列制約として構文上共存できる（`finalize_primary_key`
        // が主キー列の `nullable` を最終的に `false` へ強制する）。
        let is_pk = if self.peek_ident_matches("PRIMARY") && self.peek_ident_matches_at(1, "KEY") {
            self.advance();
            self.advance();
            true
        } else {
            false
        };

        // 列制約 `REFERENCES <table> [(<col>[, <col>]*)]`（TABLE-17・TASK-205、
        // Issue #907）。`PRIMARY KEY` の後ろ・`CHECK` の前に高々 1 個置ける。
        let references = if self.peek_ident_matches("REFERENCES") {
            Some(self.parse_references_clause()?)
        } else {
            None
        };

        // 列制約 `[CONSTRAINT <name>] CHECK (...)`（TABLE-16・TASK-204、Issue #906）。
        // 他の列制約（`NOT NULL`／`DEFAULT`／`UNIQUE`／`PRIMARY KEY`）の後ろに
        // 0 個以上置ける。列名を自動生成名の材料として保持する。件数は呼び出し元
        // から渡された残り枠（`max_checks`）で打ち切り、`Vec` へ積む前に `54000`。
        let mut checks: Vec<ParsedCheck> = Vec::new();
        while self.peek_check_clause_start() {
            if checks.len() >= max_checks {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many CHECK constraints in CREATE TABLE",
                ));
            }
            let (check_name, predicates) = self.parse_check_clause()?;
            checks.push(ParsedCheck {
                name: check_name,
                column: Some(column.name.clone()),
                predicates,
            });
        }

        Ok(ParsedCreateTableColumn {
            column,
            primary_key: is_pk,
            unique,
            checks,
            references,
        })
    }

    /// `FOREIGN KEY` の列リスト `( <col>[, <col>]* )`（参照元・参照先の双方。
    /// TABLE-17・TASK-205、Issue #907）。同一リスト内の列名重複は `42701`
    /// （`parse_unique_table_constraint` と同じ分類）、列数上限
    /// （[`crate::catalog::MAX_FOREIGN_KEY_COLUMNS`]）超過は `Vec` へ積む前に `54000`。
    fn parse_foreign_key_column_list(&mut self) -> Result<Vec<String>, SqlSurfaceError> {
        self.expect_punct('(')?;
        let mut cols: Vec<String> = Vec::new();
        loop {
            let name = self.expect_ident()?;
            crate::catalog::validate_identifier(&name).map_err(|e| {
                SqlSurfaceError::unsupported(format!("invalid FOREIGN KEY column name: {e}"))
            })?;
            if cols.contains(&name) {
                return Err(SqlSurfaceError::duplicate_column(name));
            }
            if cols.len() >= crate::catalog::MAX_FOREIGN_KEY_COLUMNS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many columns in FOREIGN KEY constraint",
                ));
            }
            cols.push(name);
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect_punct(')')?;
        Ok(cols)
    }

    /// `REFERENCES <table> [(<col>[, <col>]*)] [ON DELETE <act>] [ON UPDATE <act>]`
    /// （TABLE-17・TASK-205、Issue #907）。列制約・表制約の双方から呼ばれる。
    /// 参照先テーブルの存在・参照先列の一意性・型の照合はカタログ照会を要するため
    /// 構造検証の対象外（`catalog::Storage::create_table` の write トランザクション内で
    /// 判定する）。参照先列を省略した場合は空リストを返す（参照先の主キー、未宣言
    /// なら `id` へ解決される）。
    ///
    /// 参照動作は既定の `NO ACTION`（非遅延の文単位検査のため `RESTRICT` と同値）
    /// のみを実装するため、`ON DELETE`／`ON UPDATE` には `NO ACTION`／`RESTRICT` だけを
    /// 各 1 回まで受理し、`CASCADE`／`SET NULL`／`SET DEFAULT`・重複指定は `42601`。
    /// `MATCH`・`DEFERRABLE`／`INITIALLY` 等は本メソッドが消費しないため、呼び出し元の
    /// 後続判定（カンマ・閉じ括弧）が余剰トークンとして `42601` で拒否する。
    fn parse_references_clause(&mut self) -> Result<(String, Vec<String>), SqlSurfaceError> {
        self.expect_contextual_keyword("REFERENCES")?;
        let parent_table = self.expect_ident()?;
        crate::catalog::validate_identifier(&parent_table).map_err(|e| {
            SqlSurfaceError::unsupported(format!("invalid referenced table name: {e}"))
        })?;
        let parent_columns = if matches!(self.peek(), Some(Token::Punct('('))) {
            self.parse_foreign_key_column_list()?
        } else {
            Vec::new()
        };
        let mut seen_delete = false;
        let mut seen_update = false;
        while self.peek_ident_matches("ON") {
            self.advance();
            let seen = if self.peek_ident_matches("DELETE") {
                &mut seen_delete
            } else if self.peek_ident_matches("UPDATE") {
                &mut seen_update
            } else {
                return Err(SqlSurfaceError::unsupported(
                    "expected DELETE or UPDATE after ON in FOREIGN KEY",
                ));
            };
            if *seen {
                return Err(SqlSurfaceError::unsupported(
                    "duplicate referential action in FOREIGN KEY",
                ));
            }
            *seen = true;
            self.advance();
            if self.peek_ident_matches("NO") && self.peek_ident_matches_at(1, "ACTION") {
                self.advance();
                self.advance();
            } else if self.peek_ident_matches("RESTRICT") {
                self.advance();
            } else {
                return Err(SqlSurfaceError::unsupported(
                    "only NO ACTION or RESTRICT is supported as a FOREIGN KEY referential action",
                ));
            }
        }
        Ok((parent_table, parent_columns))
    }

    /// 表制約 `FOREIGN KEY (<col>[, <col>]*) REFERENCES ...`（TABLE-17・TASK-205、
    /// Issue #907）。参照元列の実在は [`finalize_foreign_keys`] が列リスト全体の
    /// 構文判定後に判定する（表制約は宣言順に関わらず任意位置の列を参照できる）。
    fn parse_foreign_key_table_constraint(
        &mut self,
    ) -> Result<crate::catalog::ForeignKeyDef, SqlSurfaceError> {
        self.expect_contextual_keyword("FOREIGN")?;
        self.expect_contextual_keyword("KEY")?;
        let columns = self.parse_foreign_key_column_list()?;
        let (parent_table, parent_columns) = self.parse_references_clause()?;
        Ok(crate::catalog::ForeignKeyDef::new(
            columns,
            parent_table,
            parent_columns,
        ))
    }

    /// 表制約 `UNIQUE (<col>[, <col>]*)`（TABLE-16・TASK-204、Issue #905）の
    /// 許可形状。列名の存在検証・型適格性は `finalize_unique_constraints` へ
    /// 委譲する（表制約は宣言順に関わらず任意位置の列を参照できるため）。
    /// 同一制約内の列名重複は `42701`（`parse_primary_key_table_constraint` と
    /// 同じ分類）、空リストは `42601`、列数上限
    /// （[`crate::catalog::MAX_UNIQUE_CONSTRAINT_COLUMNS`]）超過は `Vec` へ積む
    /// 前に `54000`。
    fn parse_unique_table_constraint(&mut self) -> Result<Vec<String>, SqlSurfaceError> {
        self.expect_contextual_keyword("UNIQUE")?;
        self.expect_punct('(')?;
        let mut cols: Vec<String> = Vec::new();
        loop {
            let name = self.expect_ident()?;
            crate::catalog::validate_identifier(&name).map_err(|e| {
                SqlSurfaceError::unsupported(format!("invalid UNIQUE column name: {e}"))
            })?;
            if cols.contains(&name) {
                return Err(SqlSurfaceError::duplicate_column(name));
            }
            if cols.len() >= crate::catalog::MAX_UNIQUE_CONSTRAINT_COLUMNS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many columns in UNIQUE constraint",
                ));
            }
            cols.push(name);
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect_punct(')')?;
        Ok(cols)
    }

    /// 表制約 `PRIMARY KEY (<col>[, <col>]*)`（TABLE-16・TASK-204、Issue #903）の
    /// 許可形状。`PRIMARY`・`KEY` の消費のみを行い、列名の存在検証は
    /// `finalize_primary_key` へ委譲する（表制約は列リスト中の宣言順に
    /// 関わらず任意位置の列を参照できるため）。同一主キー内の列名重複は
    /// ここで拒否する（`42701`。列リスト全体の重複列名検査
    /// `parse_create_table_column` と同じ分類）。
    fn parse_primary_key_table_constraint(&mut self) -> Result<Vec<String>, SqlSurfaceError> {
        self.expect_contextual_keyword("PRIMARY")?;
        self.expect_contextual_keyword("KEY")?;
        self.expect_punct('(')?;
        let mut cols: Vec<String> = Vec::new();
        loop {
            let name = self.expect_ident()?;
            crate::catalog::validate_identifier(&name).map_err(|e| {
                SqlSurfaceError::unsupported(format!("invalid primary key column name: {e}"))
            })?;
            if cols.contains(&name) {
                return Err(SqlSurfaceError::duplicate_column(name));
            }
            cols.push(name);
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                if cols.len() >= MAX_PRIMARY_KEY_COLUMNS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many columns in PRIMARY KEY",
                    ));
                }
                continue;
            }
            break;
        }
        self.expect_punct(')')?;
        if cols.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "PRIMARY KEY must declare at least one column",
            ));
        }
        Ok(cols)
    }

    /// `CREATE TABLE` の列定義に続く列制約（`NOT NULL`／`DEFAULT <literal>`
    /// 〔Issue #904〕・`UNIQUE`〔Issue #905〕。TABLE-16・TASK-204）を、順序自由・
    /// 各々最大 1 回まで受理する。
    /// それぞれの重複指定は `42601`。列型ごとの適用可否（`VECTOR` への
    /// `DEFAULT` 禁止等）は呼び出し元（[`Parser::parse_create_table_column`]）が
    /// 判定する。`DEFAULT NULL` は `expect_literal` が `NULL` トークンを
    /// リテラルとして受理しないため、構造的に `42601` で拒否される
    /// （TABLE-16: 明示 `NULL` は `DEFAULT` の対象外）。
    fn parse_column_constraints(&mut self) -> Result<ColumnConstraints, SqlSurfaceError> {
        let mut not_null = false;
        let mut default: Option<InsertLiteral> = None;
        let mut unique = false;
        loop {
            if self.peek_ident_matches("UNIQUE") {
                self.advance();
                if unique {
                    return Err(SqlSurfaceError::unsupported("duplicate UNIQUE constraint"));
                }
                unique = true;
                continue;
            }
            if self.peek_ident_matches("NOT") {
                self.advance();
                self.expect_contextual_keyword("NULL")?;
                if not_null {
                    return Err(SqlSurfaceError::unsupported(
                        "duplicate NOT NULL constraint",
                    ));
                }
                not_null = true;
                continue;
            }
            if self.peek_ident_matches("DEFAULT") {
                self.advance();
                if default.is_some() {
                    return Err(SqlSurfaceError::unsupported("duplicate DEFAULT constraint"));
                }
                default = Some(self.expect_literal()?);
                continue;
            }
            break;
        }
        Ok(ColumnConstraints {
            not_null,
            default,
            unique,
        })
    }

    /// `DROP TABLE <table> [;]` の単一テーブル形のみを受理する（SQL-23、
    /// TASK-203、Issue #902）。`IF EXISTS`・`CASCADE`・`RESTRICT`・複数テーブル
    /// 列挙・`USING OPERATION_ID` 句は構造的に受理しない（`expect_end_of_statement`
    /// が余剰トークンとして `42601` で拒否する）。
    fn parse_drop_table(&mut self) -> Result<String, SqlSurfaceError> {
        self.expect_contextual_keyword("DROP")?;
        self.expect_contextual_keyword("TABLE")?;
        self.expect_ident()
    }

    /// 現在位置から末尾までの未消費トークン列（TABLE-18・SQL-23・TASK-205、
    /// Issue #909）。`CREATE VIEW <name> AS <body>` の `<body>` 部分を
    /// [`parse_view_body`] へ独立した文として渡すために使う。
    fn remaining(&self) -> &'a [Token] {
        match self.tokens.get(self.pos..) {
            Some(rest) => rest,
            None => &[],
        }
    }

    /// `CREATE VIEW <name> AS <body> [;]`（TABLE-18・SQL-23・TASK-205、
    /// Issue #909）。`CREATE OR REPLACE`・`IF NOT EXISTS`・`TEMP`／
    /// `MATERIALIZED`・列別名リスト `v (a, b)` はいずれも構造的に受理しない。
    fn parse_create_view(&mut self) -> Result<(String, ParsedViewBody), SqlSurfaceError> {
        self.expect_ident_matching("CREATE")?;
        self.expect_ident_matching("VIEW")?;
        let name = self.expect_ident()?;
        self.expect_ident_matching("AS")?;
        let body = parse_view_body(self.remaining())?;
        // `parse_view_body` が本文トークン列（`self.remaining()`）の終端まで
        // 消費し尽くしたことを既に検証済みのため（`expect_end_of_statement`
        // 呼び出し）、この Parser 側で追加のトークンを消費する必要はない。
        Ok((name, body))
    }

    /// `DROP VIEW <name> [;]`（TABLE-18・SQL-23・TASK-205、Issue #909）。
    /// `IF EXISTS`・`CASCADE`／`RESTRICT`・複数ビュー列挙は構造的に受理しない。
    fn parse_drop_view(&mut self) -> Result<String, SqlSurfaceError> {
        self.expect_ident_matching("DROP")?;
        self.expect_ident_matching("VIEW")?;
        self.expect_ident()
    }

    /// `WITH <name> AS (<body>)[, <name> AS (<body>)]*`（非再帰 CTE。SQL-29 (b)・
    /// RLS-10 (b)、TASK-213、Issue #928）を切り出し、各定義を
    /// [`super::cte::CteDef`] へ積む。`<body>` は [`parse_view_body`]（`CREATE
    /// VIEW` 本文と同一の許可パーサー）で検証する——本関数は第 2 の SELECT
    /// パーサーを持たない。呼び出し元（[`validate_sql_tokens`] の `WITH`
    /// 分岐）が、返した `Vec<CteDef>` と本関数消費後の残りトークン列
    /// （主クエリ）を [`super::cte::resolve_relation`] へ渡す。
    ///
    /// 受理しない形（いずれも `42601`。SQL-29 (b) の対象外規定）:
    /// `WITH RECURSIVE`・列名リスト `<name>(a, b)`・`MATERIALIZED`／
    /// `NOT MATERIALIZED` 指定・本文内の `;`（`split_parenthesized` で切り出した
    /// 括弧内トークン列に対して明示的に検査する。`parse_view_body` の
    /// `expect_end_of_statement` は末尾の単一 `;` を許容してしまうため、
    /// ここで先に弾かないと `WITH x AS (SELECT * FROM t;) ...` のような入力を
    /// 誤って受理しうる）。定義数の上限は [`super::cte::check_definition_count`]、
    /// 名前重複は [`super::cte::check_no_duplicate_name`] が判定する。
    fn parse_with_clause(&mut self) -> Result<Vec<super::cte::CteDef>, SqlSurfaceError> {
        self.expect_ident_matching("WITH")?;
        if self.peek_ident_matches("RECURSIVE") {
            return Err(SqlSurfaceError::unsupported(
                "WITH RECURSIVE is not supported",
            ));
        }
        let mut ctes: Vec<super::cte::CteDef> = Vec::new();
        loop {
            let name = self.expect_ident()?;
            // `AS` を消費する前に `(` が現れるのは列名リスト形のみ（本文の
            // `(` は必ず `AS` の後）。
            if matches!(self.peek(), Some(Token::Punct('('))) {
                return Err(SqlSurfaceError::unsupported(
                    "CTE column name list is not supported",
                ));
            }
            self.expect_ident_matching("AS")?;
            if self.peek_ident_matches("MATERIALIZED") || self.peek_ident_matches("NOT") {
                return Err(SqlSurfaceError::unsupported(
                    "CTE MATERIALIZED hint is not supported",
                ));
            }
            if !matches!(self.peek(), Some(Token::Punct('('))) {
                return Err(SqlSurfaceError::unsupported(
                    "expected '(' to start CTE body",
                ));
            }
            let (inner, after) = split_parenthesized(self.remaining())?;
            if inner.iter().any(|t| matches!(t, Token::Punct(';'))) {
                return Err(SqlSurfaceError::unsupported(
                    "CTE body must not contain a statement separator",
                ));
            }
            let body = parse_view_body(inner)?;
            super::cte::check_no_duplicate_name(&ctes, &name)?;
            ctes.push(super::cte::CteDef { name, body });
            super::cte::check_definition_count(ctes.len())?;
            // `split_parenthesized` はスライスのみを返す（`Parser` の内部位置を
            // 持たない）ため、残りトークン数から `self.pos` を復元する。`after`
            // は構造的に `self.tokens` の suffix であり `after.len()` は
            // `self.tokens.len()` を超えないが、coding-rust.md の checked/
            // saturating 演算方針に従い `saturating_sub` で明示する。
            self.pos = self.tokens.len().saturating_sub(after.len());
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(ctes)
    }
}

/// `CREATE VIEW ... AS` 本文の許可形状（TABLE-18・SQL-23・TASK-205、
/// Issue #909）。`SELECT <* | 列名リスト> FROM <table | view> [WHERE
/// <単純述語> [AND ...]]` のみを受理する（`LIMIT`・`ORDER BY`・`USING PLAN`・
/// 集計・式項目〔`Projection::Items`〕・UDF 呼び出し述語
/// 〔`WherePredicate::PredicateCall`／`Expression`〕はいずれも許可リスト外。
/// §2.1「本リポの実装既定値」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedViewBody {
    pub(crate) table_name: String,
    pub(crate) projection: Projection,
    pub(crate) where_predicates: Vec<WherePredicate>,
}

/// [`ParsedViewBody`] の構造検証本体。`CREATE VIEW` の本文パース
/// （[`Parser::parse_create_view`]）と、格納済みビュー定義の再検証
/// （`sql::view::resolve_from` が [`crate::catalog::ViewDef::body_sql`] を
/// 再トークン化して渡す。第 2 の SQL パーサーを作らない設計）の双方が共有する
/// 唯一の実装。
pub(crate) fn parse_view_body(tokens: &[Token]) -> Result<ParsedViewBody, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_keyword(Keyword::Select)?;
    let projection = p.parse_select_list()?;
    if let Projection::Items(_) = projection {
        return Err(SqlSurfaceError::unsupported(
            "view body does not support expression projection items",
        ));
    }
    p.expect_keyword(Keyword::From)?;
    let table_name = p.expect_ident()?;
    let where_predicates = if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        p.advance();
        p.parse_where()?
    } else {
        Vec::new()
    };
    for pred in &where_predicates {
        match pred {
            WherePredicate::Equality { .. }
            | WherePredicate::Prefix { .. }
            | WherePredicate::BoolEquality { .. }
            | WherePredicate::BoolColumn { .. }
            | WherePredicate::Compare { .. } => {}
            WherePredicate::PredicateCall { .. } | WherePredicate::Expression(_) => {
                return Err(SqlSurfaceError::unsupported(
                    "view body WHERE predicate form is not supported",
                ));
            }
            WherePredicate::Or(_) => {
                // TASK-208・SQL-24（Issue #912）の対象は SQL-19 の書き込み文と
                // 読み取り SELECT で、TABLE-18 のビュー定義文本体は含まない
                // （ビューに対する**外側クエリ**の OR は `sql::view::resolve_from`
                // 経由で通常どおり受理される。ここで拒否するのは定義文本体のみ）。
                return Err(SqlSurfaceError::unsupported(
                    "view body WHERE predicate form is not supported",
                ));
            }
            // Issue #927・SQL-29 (a)・TASK-213: `CREATE VIEW` 本体は `Parser::new`
            // の既定（`subquery_ctx == None`）で解析するため、`parse_where` 自体が
            // サブクエリ構文を `42601` で拒否し、ここへ到達しない
            // （fail-closed の防御的経路）。
            WherePredicate::InSubquery { .. } | WherePredicate::Exists { .. } => {
                return Err(SqlSurfaceError::unsupported(
                    "view body WHERE predicate form is not supported",
                ));
            }
        }
    }
    p.expect_end_of_statement()?;
    Ok(ParsedViewBody {
        table_name,
        projection,
        where_predicates,
    })
}

/// [`ParsedViewBody`] を再パース可能な正規化 SQL テキストへ描画する
/// （TABLE-18・SQL-23・TASK-205、Issue #909）。「描画 → 再トークン化 →
/// [`parse_view_body`]」が元と等価な AST を復元することを
/// `sql::view` の round-trip テストで固定する（[`crate::catalog::Storage::
/// create_view`] が保存する `body_sql` はこの関数の出力のみ）。文字列
/// リテラルは `'` を `''` へ二重化してエスケープする（[`crate::sql::lexer`]
/// の読み取り側〔`''` → `'`〕と対称）。
pub(crate) fn render_view_body(body: &ParsedViewBody) -> String {
    let mut out = String::from("SELECT ");
    match &body.projection {
        Projection::All => out.push('*'),
        Projection::Columns(cols) => out.push_str(&cols.join(", ")),
        // `parse_view_body` が構造的に拒否するため到達しない。
        Projection::Items(_) => out.push('*'),
    }
    out.push_str(" FROM ");
    out.push_str(&body.table_name);
    if !body.where_predicates.is_empty() {
        out.push_str(" WHERE ");
        let rendered: Vec<String> = body
            .where_predicates
            .iter()
            .map(render_where_predicate)
            .collect();
        out.push_str(&rendered.join(" AND "));
    }
    out
}

/// 文字列リテラルを `'` の二重化でエスケープする（[`render_view_body`] 参照）。
fn escape_string_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// [`WherePredicate`] のうち [`parse_view_body`] が受理する形状のみを描画する
/// （`PredicateCall`／`Expression` は到達しない）。
fn render_where_predicate(pred: &WherePredicate) -> String {
    match pred {
        WherePredicate::Equality { column, value } => {
            format!("{column} = '{}'", escape_string_literal(value))
        }
        // `pattern` は LIKE の生パターン全般（SQL-24・TASK-208、Issue #914 で
        // 前方一致限定から拡張）をそのまま無加工で保持する（`Parser::
        // parse_where` の LIKE 分岐参照）。意味論的な検証・振り分けは
        // `declarative_filter::DeclarativeFilter::like` の責務で、ここでは
        // 追加のワイルドカードを付与しない。
        WherePredicate::Prefix { column, pattern } => {
            format!("{column} LIKE '{}'", escape_string_literal(pattern))
        }
        WherePredicate::BoolEquality { column, value } => {
            format!("{column} = {}", if *value { "true" } else { "false" })
        }
        WherePredicate::BoolColumn { column } => column.clone(),
        WherePredicate::Compare { column, op, value } => {
            let op_str = match op {
                CompareOp::Lt => "<",
                CompareOp::Le => "<=",
                CompareOp::Gt => ">",
                CompareOp::Ge => ">=",
            };
            format!("{column} {op_str} '{}'", escape_string_literal(value))
        }
        // `parse_view_body` が構造的に拒否するため到達しない
        // （`render_view_body` は常に [`parse_view_body`] の出力のみを描画する）。
        WherePredicate::PredicateCall { name } => format!("{name}()"),
        WherePredicate::Expression(_) => String::new(),
        // `parse_view_body` が Or も拒否するため現時点では到達しない。将来の
        // ビュー本体 OR 解禁（別 Issue）に備え、往復可能な形で網羅描画だけ
        // 先に用意しておく（TASK-208・Issue #912）。
        WherePredicate::Or(branches) => {
            let rendered_branches: Vec<String> = branches
                .iter()
                .map(|branch| {
                    let rendered: Vec<String> = branch.iter().map(render_where_predicate).collect();
                    rendered.join(" AND ")
                })
                .collect();
            format!("({})", rendered_branches.join(" OR "))
        }
        // `parse_view_body` がサブクエリを構造的に拒否するため到達しない
        // （Issue #927・SQL-29 (a)・TASK-213。上記 `Or` と同じ理由）。
        WherePredicate::InSubquery { .. } | WherePredicate::Exists { .. } => String::new(),
    }
}

/// `CREATE VIEW <name> AS <body>` の許可形状構造検証結果（TABLE-18・SQL-23・
/// TASK-205、Issue #909）。カタログ照会は一切行わない（構文検証段はカタログを
/// 照会しない契約。DDL 実行権限ゲート・参照先の存在確認・ネスト深さ判定は
/// いずれも `crate::sql::ddl::execute_create_view` が担う）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCreateView {
    pub(crate) name: String,
    /// `body` の直接の参照先（テーブルまたは別のビュー。連鎖の畳み込みは
    /// 参照時の [`crate::sql::view::resolve_from`] が担う）。
    pub(crate) base_relation: String,
    /// [`render_view_body`] で描画した正規化 SQL（永続化される値そのもの）。
    pub(crate) body_sql: String,
}

impl ValidatedCreateView {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn base_relation(&self) -> &str {
        &self.base_relation
    }

    pub fn body_sql(&self) -> &str {
        &self.body_sql
    }
}

/// `DROP VIEW <name>` の許可形状構造検証結果（TABLE-18・SQL-23・TASK-205、
/// Issue #909）。[`ValidatedDropTable`] と同じくカタログ照会を一切行わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDropView {
    pub(crate) name: String,
}

impl ValidatedDropView {
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// [`ValidatedCreateView`] の構造検証本体（[`sql::ddl::execute_create_view`]
/// から呼ばれる。TABLE-18・SQL-23・TASK-205、Issue #909）。
pub(crate) fn validate_create_view_tokens(
    tokens: &[Token],
) -> Result<ValidatedCreateView, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let (name, body) = p.parse_create_view()?;
    let body_sql = render_view_body(&body);
    if body_sql.len() > crate::catalog::MAX_VIEW_BODY_BYTES {
        return Err(SqlSurfaceError::payload_too_large(
            "view body exceeds the allowed size",
        ));
    }
    Ok(ValidatedCreateView {
        name,
        base_relation: body.table_name,
        body_sql,
    })
}

/// `CREATE INDEX <name> ON <table> [USING hnsw] (<col>[, ...])` の許可形状構造
/// 検証結果（TASK-206・INDEX-7・SQL-23、Issue #908）。[`ValidatedDropTable`] と
/// 同じくカタログ照会を一切行わない（列の存在・型整合・名前衝突・対象の種別は
/// `sql::ddl::execute_create_index` → `catalog::Storage::create_index` が単一の
/// 書き込みトランザクション内で判定する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCreateIndex {
    pub(crate) name: String,
    pub(crate) table: String,
    /// `USING hnsw` 指定の有無（`hnsw` 以外の `USING` 値は構造検証段で `0A000`
    /// 拒否済み）。
    pub(crate) hnsw: bool,
    pub(crate) columns: Vec<String>,
}

impl ValidatedCreateIndex {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn is_hnsw(&self) -> bool {
        self.hnsw
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// `DROP INDEX <name>` の許可形状構造検証結果（TASK-206・INDEX-7・SQL-23、
/// Issue #908）。[`ValidatedDropView`] と同じくカタログ照会を一切行わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDropIndex {
    pub(crate) name: String,
}

impl ValidatedDropIndex {
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// 先頭 2 トークンが `CREATE INDEX` か（`core.rs::EngineCore::parse_tokens` の
/// 分岐判定。`CREATE`・`INDEX` は予約語化せず文頭でのみ文脈的に照合する）。
pub(crate) fn is_create_index_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("CREATE"))
        && matches!(tokens.get(1), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("INDEX"))
}

/// 先頭 2 トークンが `DROP INDEX` か（[`is_create_index_statement`] と同じ流儀）。
pub(crate) fn is_drop_index_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("DROP"))
        && matches!(tokens.get(1), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("INDEX"))
}

/// [`ValidatedCreateIndex`] の構造検証本体（TASK-206・INDEX-7、Issue #908）。
/// カタログを参照しない範囲の判定はここで完結させる（fail-closed。権限の無い
/// 呼び出し元へ存在情報を漏らさないよう、カタログ照会を要する検査は一切行わない）:
/// - `USING <ident>` が `hnsw` 以外 → `0A000`（`FeatureNotSupported`）。
/// - `USING hnsw` なのに列が複数 → `0A000`。
/// - 列リストの要素が式・関数呼び出し・リテラル → `0A000`。
/// - 部分索引の `WHERE` 句 → `0A000`。
/// - 列リストに重複する列名 → `42601`。列数上限超過 → `54000`。
///
/// `UNIQUE`／`CONCURRENTLY`／`IF NOT EXISTS`／`INCLUDE`／`WITH (...)`／
/// `ASC|DESC`／opclass・スキーマ修飾名・`$n` 等は、いずれも期待するトークン列と
/// 一致しないため `expect_*` ヘルパー経由で構造的に `42601` へ落ちる。
pub(crate) fn validate_create_index_tokens(
    tokens: &[Token],
) -> Result<ValidatedCreateIndex, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("CREATE")?;
    p.expect_ident_matching("INDEX")?;
    let name = p.expect_ident()?;
    p.expect_ident_matching("ON")?;
    let table = p.expect_ident()?;
    let hnsw = if p.peek_ident_matches("USING") {
        p.advance();
        let method = p.expect_ident()?;
        if !method.eq_ignore_ascii_case("hnsw") {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: truncate_for_error(&format!("index method {method} is not supported")),
            });
        }
        true
    } else {
        false
    };
    p.expect_punct('(')?;
    let mut columns = vec![parse_index_column(&mut p)?];
    while matches!(p.peek(), Some(Token::Punct(','))) {
        p.advance();
        if columns.len() >= MAX_INDEX_DDL_COLUMNS {
            return Err(SqlSurfaceError::payload_too_large("too many index columns"));
        }
        columns.push(parse_index_column(&mut p)?);
    }
    p.expect_punct(')')?;
    if hnsw && columns.len() != 1 {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "USING hnsw requires exactly one column".to_string(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    for c in &columns {
        if !seen.insert(c.as_str()) {
            return Err(SqlSurfaceError::unsupported(format!(
                "duplicate index column: {c}"
            )));
        }
    }
    // 部分索引（PostgreSQL 拡張構文）の `WHERE` 句は許可形状外の「機能」として
    // `0A000`（`expect_end_of_statement` の汎用検査に落とすと `42601` になる）。
    if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "partial indexes (WHERE clause) are not supported".to_string(),
        });
    }
    p.expect_end_of_statement()?;
    Ok(ValidatedCreateIndex {
        name,
        table,
        hnsw,
        columns,
    })
}

/// [`validate_create_index_tokens`] の列リスト 1 要素。裸の識別子のみを受理する
/// （式・関数呼び出し・括弧・リテラルは `0A000` で拒否する）。
fn parse_index_column(p: &mut Parser<'_>) -> Result<String, SqlSurfaceError> {
    if !matches!(p.peek(), Some(Token::Ident(_))) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "index columns must be plain identifiers".to_string(),
        });
    }
    let name = p.expect_ident()?;
    if matches!(p.peek(), Some(Token::Punct('('))) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "expression indexes are not supported".to_string(),
        });
    }
    Ok(name)
}

/// [`ValidatedDropIndex`] の構造検証本体。`IF EXISTS`・`CASCADE`・複数名の同時
/// 指定はいずれもトレイリングトークン検査により構造的に `42601` へ落ちる。
pub(crate) fn validate_drop_index_tokens(
    tokens: &[Token],
) -> Result<ValidatedDropIndex, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("DROP")?;
    p.expect_ident_matching("INDEX")?;
    let name = p.expect_ident()?;
    p.expect_end_of_statement()?;
    Ok(ValidatedDropIndex { name })
}

/// [`ValidatedDropView`] の構造検証本体。
pub(crate) fn validate_drop_view_tokens(
    tokens: &[Token],
) -> Result<ValidatedDropView, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let name = p.parse_drop_view()?;
    p.expect_end_of_statement()?;
    Ok(ValidatedDropView { name })
}

/// 構文木（[`ValidatedTruncate`] の元）。カタログ存在確認前の中間結果
/// （SQL-22、TASK-195）。
struct ParsedTruncateShape {
    table_name: String,
    operation_id: Option<OperationId>,
}

/// 構文木（[`ValidatedAlterTableAddColumn`] の元）。カタログ存在確認前の
/// 中間結果（TASK-202・SQL-23。Issue #900）。
struct ParsedAlterTableAddColumnShape {
    table_name: String,
    column_name: String,
    column_type: crate::sql::ddl_column_type::SqlColumnTypeName,
}

/// 構文木（[`ValidatedStatement`] の元）。カタログ存在確認前の中間結果。
struct ParsedShape {
    table_name: String,
    projection: Projection,
    where_predicates: Vec<WherePredicate>,
    order_by: OrderByForm,
    limit: u32,
    search_mode: Option<String>,
    evaluation_order: EvaluationOrder,
    /// `USING PLAN('<query>')`（TASK-77・SQL-5）。`Some` のとき `order_by` は必ず
    /// [`OrderByForm::UsingPlan`]。
    using_plan: Option<String>,
}

/// 構文木（[`ValidatedScan`] の元）。カタログ存在確認前の中間結果（Issue #454。
/// SQL-30・TASK-214、Issue #930 で `window_items` を追加）。
struct ParsedScanShape {
    table_name: String,
    projection: Projection,
    where_predicates: Vec<WherePredicate>,
    limit: u32,
    offset: u32,
    /// ウィンドウ項目（SELECT リスト全体での出現位置つき）。空なら通常の広域取得。
    window_items: Vec<WindowSelectItem>,
}

/// [`parse_select_shape`] の戻り値。`WHERE`（省略可）の直後に現れる分岐トークン
/// （`USING`／`ORDER`／`LIMIT`）で、ランキング段を持つ検索 SELECT（[`Search`]。
/// 既存の [`ParsedShape`]）か、ランキング段を持たない広域取得（[`Scan`]。Issue #454
/// の [`ParsedScanShape`]）かを振り分ける。
enum ParsedSelect {
    Search(ParsedShape),
    Scan(ParsedScanShape),
}

/// 許可した `SELECT` statement 形状を先頭から再帰下降で判定する（TASK-74 由来。
/// TASK-161 で `LIMIT` 直後の `USING MODE` 句判定を追加した。TASK-77・SQL-5 で
/// `WHERE`（省略可）直後の `USING PLAN(...)` 分岐を追加した。Issue #454 で
/// `WHERE`（省略可）の直後に `LIMIT` が直接現れる、ランキング段を持たない広域
/// 取得の分岐を追加した）。
fn parse_select_shape(
    tokens: &[Token],
    subquery_ctx: Option<usize>,
) -> Result<ParsedSelect, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    if let Some(depth) = subquery_ctx {
        p = p.with_subquery_ctx(depth);
    }

    p.expect_keyword(Keyword::Select)?;
    // SQL-30・TASK-214（Issue #930）: ウィンドウ項目は SELECT リストの一部として
    // 通常項目と混在しうるため、`ORDER BY`／`USING PLAN` を伴う検索 SELECT 経路か
    // ランキング段を持たない広域取得かを判定する**前**にリストを読み終える必要が
    // ある（既存の `parse_select_list` 呼び出しをそのまま置き換える）。
    let (projection, window_items) = p.parse_select_list_with_windows()?;
    p.expect_keyword(Keyword::From)?;
    let table_name = p.expect_ident()?;

    let where_predicates = if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        p.advance();
        p.parse_where()?
    } else {
        Vec::new()
    };

    // SQL-30・TASK-214: ウィンドウ項目は WHERE の中で自分自身の別名を参照できない
    // （実在列と同名なら列として解釈する。§計画 3.4「WHERE がウィンドウ別名を
    // 参照する場合」）。ここでは束縛前の構造検証段のため列の実在確認はできず、
    // 意味論的な判定は `sql::parser::bind_scan_with_dummy_flags` へ委譲する
    // （構造検証段は `window_items` を `ParsedScanShape` へそのまま伝播するのみ）。

    // TASK-77・SQL-5: `USING PLAN(...)` は `ORDER BY` の代替経路（相互排他）。
    // `ORDER BY` 経路は必ずキーワード `ORDER` から始まるため、この位置で文脈的
    // 識別子 `USING` が現れるかどうかだけで両者を衝突なく判定できる。
    if p.peek_ident_matches("USING") {
        // SQL-30・TASK-214: `USING PLAN` はランキング段を持つ検索 SELECT 専用の
        // 経路で、ウィンドウ項目とは構造上両立しない（§計画 2「対象外」）。
        if !window_items.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "window functions cannot be combined with USING PLAN",
            ));
        }
        let using_plan = p.parse_using_plan_clause()?;

        p.expect_keyword(Keyword::Limit)?;
        let limit_str = p.expect_number()?;
        let limit: u32 = limit_str.parse().map_err(|_| {
            SqlSurfaceError::unsupported(format!("malformed LIMIT value: {limit_str}"))
        })?;

        // `HINT ORDER(...)` はランキング段順のヒントであり、ランキング自体を
        // `USING PLAN` の展開結果が決める本経路では意味を持たない。構造上も
        // 受理しない（`ORDER BY` 経路の既存文法を変えず、`USING PLAN` 側だけを
        // 素通しで「`USING MODE` のみ許容」に保つ）。
        let search_mode = p.parse_using_clause()?;

        p.expect_end_of_statement()?;

        return Ok(ParsedSelect::Search(ParsedShape {
            table_name,
            projection,
            where_predicates,
            order_by: OrderByForm::UsingPlan,
            limit,
            search_mode,
            evaluation_order: EvaluationOrder::DEFAULT,
            using_plan: Some(using_plan),
        }));
    }

    // Issue #454: `WHERE`（省略可）の直後に `ORDER`（既存の検索 SELECT 経路）でも
    // 文脈識別子 `USING`（`USING PLAN`、上の分岐）でもなく `LIMIT` キーワードが
    // 直接現れる形は、ランキング段（`ORDER BY`・`USING PLAN` いずれも）を持たない
    // 広域取得（ソートなしのフィルタ取得）として受理する。`LIMIT` 直後は文末のみを
    // 許可し（`USING MODE`・`HINT ORDER` は受理しない。§3.1「本リポの実装既定値」）、
    // それ以外の余剰トークンは `expect_end_of_statement` が `42601` へ落とす。
    if matches!(p.peek(), Some(Token::Keyword(Keyword::Limit))) {
        p.advance();
        let limit_str = p.expect_number()?;
        let limit: u32 = limit_str.parse().map_err(|_| {
            SqlSurfaceError::unsupported(format!("malformed LIMIT value: {limit_str}"))
        })?;

        // Issue #916・SQL-25 (b)・TASK-209: `LIMIT n` の直後に任意で `OFFSET m` を
        // 受理する（広域取得のみ。検索 SELECT の `ORDER BY`／`USING PLAN` 経路は
        // 対象外のまま `expect_end_of_statement` が `42601` に落とす）。
        let offset = p.parse_optional_offset()?.unwrap_or(0);

        p.expect_end_of_statement()?;

        return Ok(ParsedSelect::Scan(ParsedScanShape {
            table_name,
            projection,
            where_predicates,
            limit,
            offset,
            window_items,
        }));
    }

    // SQL-30・TASK-214: 残る経路は文全体のスカラー `ORDER BY` を伴う検索 SELECT
    // （`docs/spec/04-behavior/sql-surface.md` SQL-25 の管轄。本 Issue のスコープ外
    // ——TASK-209〔#915〕未マージのため §計画 2「対象外」）で、ウィンドウ項目とは
    // 併用しない。
    if !window_items.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "window functions cannot be combined with ORDER BY",
        ));
    }

    p.expect_keyword(Keyword::Order)?;
    p.expect_keyword(Keyword::By)?;
    let order_by = p.parse_order_by()?;

    p.expect_keyword(Keyword::Limit)?;
    let limit_str = p.expect_number()?;
    let limit: u32 = limit_str
        .parse()
        .map_err(|_| SqlSurfaceError::unsupported(format!("malformed LIMIT value: {limit_str}")))?;

    let evaluation_order = p.parse_hint_order()?.unwrap_or(EvaluationOrder::DEFAULT);
    let search_mode = p.parse_using_clause()?;

    p.expect_end_of_statement()?;

    Ok(ParsedSelect::Search(ParsedShape {
        table_name,
        projection,
        where_predicates,
        order_by,
        limit,
        search_mode,
        evaluation_order,
        using_plan: None,
    }))
}

/// 構文木（[`ValidatedAggregate`] の元）。カタログ存在確認前の中間結果
/// （TASK-166・SQL-13。TASK-167・SQL-14 で `group_by` を追加）。
struct ParsedAggregateShape {
    table_name: String,
    items: Vec<AggregateSelectItem>,
    where_predicates: Vec<WherePredicate>,
    group_by: Option<GroupByClause>,
}

/// 許可した集計 `SELECT` statement 形状を先頭から再帰下降で判定する（TASK-166・
/// SQL-13。TASK-167・SQL-14 で `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT` を追加）。
/// `HINT ORDER`／`USING MODE` はいずれの形でも受理しない（集計結果は取得モードの
/// 余地を持たない）。呼び出し元（[`validate_sql`]）は先頭 2 トークンが集計関数名
/// `'('` であるか、`SELECT ... GROUP BY` の並びを含むかのいずれかを確認済みの
/// 前提で呼ぶ。
fn parse_aggregate_shape(
    tokens: &[Token],
    subquery_ctx: Option<usize>,
) -> Result<ParsedAggregateShape, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    if let Some(depth) = subquery_ctx {
        p = p.with_subquery_ctx(depth);
    }

    p.expect_keyword(Keyword::Select)?;
    let mut items = vec![p.parse_aggregate_select_item()?];
    while matches!(p.peek(), Some(Token::Punct(','))) {
        if items.len() >= MAX_AGGREGATE_ITEMS {
            return Err(SqlSurfaceError::payload_too_large(
                "too many aggregate items",
            ));
        }
        p.advance();
        items.push(p.parse_aggregate_select_item()?);
    }
    // SELECT リストに集計項目が 1 つも無い（`GroupKey` のみ、いわゆる
    // `SELECT DISTINCT` 相当）形は許可しない（TASK-167・SQL-14 の受理形は集計
    // 結果を持つ行のみを対象とする）。
    if !items
        .iter()
        .any(|i| matches!(i, AggregateSelectItem::Aggregate(_)))
    {
        return Err(SqlSurfaceError::unsupported(
            "aggregate SELECT list requires at least one aggregate item",
        ));
    }
    p.expect_keyword(Keyword::From)?;
    let table_name = p.expect_ident()?;

    let where_predicates = if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        p.advance();
        p.parse_where()?
    } else {
        Vec::new()
    };

    let has_group_by =
        matches!(p.peek(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("GROUP"));
    let group_by = if has_group_by {
        let columns = p.parse_group_by_clause()?;
        // SELECT リストの `GroupKey` 項目は、いずれかの `GROUP BY` 列と同名で
        // なければならない（§計画 3.1）。不一致・`GROUP BY` 句を持たない
        // `GroupKey` 項目（下の `else` 分岐）はいずれも許可リスト外として
        // `42601` に落とす。
        for item in &items {
            if let AggregateSelectItem::GroupKey { column: c, .. } = item {
                if !columns.contains(c) {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "SELECT list bare identifier {c:?} does not match any GROUP BY column {columns:?}"
                    )));
                }
            }
        }
        let having = if matches!(p.peek(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("HAVING"))
        {
            p.parse_having()?
        } else {
            Vec::new()
        };
        let order_by = if matches!(p.peek(), Some(Token::Keyword(Keyword::Order))) {
            Some(p.parse_aggregate_order_by()?)
        } else {
            None
        };
        let (limit, offset) = if matches!(p.peek(), Some(Token::Keyword(Keyword::Limit))) {
            let limit = p.parse_aggregate_limit()?;
            // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` は `LIMIT` を伴う場合のみ
            // 受理する（`LIMIT` なしの `OFFSET` 単独は後続の `expect_end_of_statement`
            // が `42601` へ落とす。§計画 3.1）。
            let offset = p.parse_optional_offset()?.unwrap_or(0);
            (Some(limit), offset)
        } else {
            (None, 0)
        };
        Some(GroupByClause {
            columns,
            having,
            order_by,
            limit,
            offset,
        })
    } else {
        // `GROUP BY` 句が無いのに SELECT リストへ裸の識別子（`GroupKey` 候補）が
        // 混在する形（例: `SELECT lang, COUNT(*) FROM t`）は許可しない。
        if items
            .iter()
            .any(|i| matches!(i, AggregateSelectItem::GroupKey { .. }))
        {
            return Err(SqlSurfaceError::unsupported(
                "bare column reference in aggregate SELECT list requires GROUP BY",
            ));
        }
        None
    };

    p.expect_end_of_statement()?;

    Ok(ParsedAggregateShape {
        table_name,
        items,
        where_predicates,
        group_by,
    })
}

/// `SELECT DISTINCT <column> [AS <alias>] FROM <table> [WHERE ...]
/// [ORDER BY ...] [LIMIT ...]`（SQL-25 (c)・TASK-209）の許可形状。`GROUP BY`
/// 実行器（[`ValidatedAggregate`]）へ直接脱糖する（SELECT リストに集計項目を
/// 持たない `GroupKey` 単独の形。[`validate_sql_tokens`] がここで組み立てた
/// `ParsedAggregateShape` を `parse_aggregate_shape` と同じ `Statement::
/// Aggregate` へ写像するため、実行経路〔`bind_aggregate`・
/// `execute_grouped_aggregate`〕は完全に共有される）。
///
/// 対象は単一の裸列参照のみ（複数列・`*`・式は `42601`。列名一致の判定を
/// 経ないため `GroupByClause::column` はここでの唯一の列名をそのまま使う）。
/// `GROUP BY`・`HAVING`・`OFFSET`・ベクトル順位付け（`ORDER BY <=>`・`HYBRID`・
/// `USING PLAN`・`USING MODE`・`HINT ORDER`）はいずれもこの構文自体が持たない
/// ため、併用は構造的に `42601` へ落ちる（SQL-25 (a) 参照）。列の型が
/// `TEXT` であることの検査は意味論層（`sql::parser::bind_group_by_clause`）が
/// 担う（`VECTOR` 列・`TEXT` 以外のスカラー列はいずれも `22000`）。
fn parse_distinct_shape(tokens: &[Token]) -> Result<ParsedAggregateShape, SqlSurfaceError> {
    let mut p = Parser::new(tokens);

    p.expect_keyword(Keyword::Select)?;
    p.expect_ident_matching("DISTINCT")?;
    let column = p.expect_ident()?;
    let alias = if p.peek_ident_matches("AS") {
        p.advance();
        Some(p.expect_ident()?)
    } else {
        None
    };
    p.expect_keyword(Keyword::From)?;
    let table_name = p.expect_ident()?;

    let where_predicates = if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        p.advance();
        p.parse_where()?
    } else {
        Vec::new()
    };
    let order_by = if matches!(p.peek(), Some(Token::Keyword(Keyword::Order))) {
        Some(p.parse_aggregate_order_by()?)
    } else {
        None
    };
    let limit = if matches!(p.peek(), Some(Token::Keyword(Keyword::Limit))) {
        Some(p.parse_aggregate_limit()?)
    } else {
        None
    };
    p.expect_end_of_statement()?;

    Ok(ParsedAggregateShape {
        table_name,
        items: vec![AggregateSelectItem::GroupKey {
            column: column.clone(),
            alias,
        }],
        where_predicates,
        group_by: Some(GroupByClause {
            columns: vec![column],
            having: Vec::new(),
            order_by,
            limit,
            // `SELECT DISTINCT <column> ...` 構文自体が `OFFSET` を持たない
            // ため常に `0`（Issue #916・SQL-25 (b)・TASK-209 の `offset` 追加に
            // 伴う base 取り込みでの構造体フィールド整合）。
            offset: 0,
        }),
    })
}

/// `SET search_mode = '<literal>'`（TASK-161・SQL-12）の許可形状。規範形は
/// `=` ＋ 文字列リテラルの完全一致のみ（`TO` 形・非引用値・`RESET`/`SHOW` 等の緩和は
/// SQL-12 に規範がないため、本実装は最も厳格な形に倒す。緩和は spec 側の判断事項）。
/// 変数名 `search_mode` は大文字小文字を区別せず照合する。
fn parse_set_search_mode(tokens: &[Token]) -> Result<String, SqlSurfaceError> {
    let mut p = Parser::new(tokens);

    p.expect_ident_matching("SET")?;
    let name = p.expect_ident()?;
    if !name.eq_ignore_ascii_case("search_mode") {
        return Err(SqlSurfaceError::unsupported(format!(
            "unsupported SET variable: {name}"
        )));
    }
    p.expect_punct('=')?;
    let value = p.expect_string_literal()?;
    p.expect_end_of_statement()?;

    Ok(value)
}

/// `CREATE FUNCTION <name>(<param>[, <param>...]) AS <expr> [;]`（TASK-79・SQL-9）の
/// 許可形状。`CREATE`／`FUNCTION`／`AS` は `SET`・`USING` と同方針で予約語化せず、
/// statement 先頭・所定位置でのみ文脈的に照合する（既存の列名・テーブル名として
/// これらの語を使う SQL を破壊しない）。パラメータ数は構造検証段階でも
/// [`MAX_UDF_PARAMS`] を超えないことを確認する（意味論検証は
/// `sql::udf_call::define_function` が担うが、アロケーション前の上限検証は
/// `.claude/rules/security.md`「長さフィールドは上限検証してからアロケーションに
/// 使う」に従い構造検証段階でも行う）。
fn parse_create_function(tokens: &[Token]) -> Result<(String, Vec<String>, Expr), SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("CREATE")?;
    p.expect_ident_matching("FUNCTION")?;
    let name = p.expect_ident()?;
    p.expect_punct('(')?;
    let mut params = Vec::new();
    if !matches!(p.peek(), Some(Token::Punct(')'))) {
        params.push(p.expect_ident()?);
        while matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
            if params.len() >= MAX_UDF_PARAMS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many function parameters",
                ));
            }
            params.push(p.expect_ident()?);
        }
    }
    p.expect_punct(')')?;
    p.expect_ident_matching("AS")?;
    let body = p.parse_value_expr(0)?;
    p.expect_end_of_statement()?;
    Ok((name, params, body))
}

/// SQL 文をトークン化し、許可リスト形式で構造検証する（TASK-161 の公開 API。
/// TASK-74 の `validate_statement` を `SELECT`／`SET search_mode`／
/// `CREATE FUNCTION`（TASK-79・SQL-9）の 3 statement 種別へ拡張したもの）。先頭
/// トークンで statement 種別を判定し、`SELECT` のみ `lookup` を通じて FROM テーブルの
/// カタログ存在確認まで行う（`SET`・`CREATE FUNCTION` はカタログ照会を要しない）。
///
/// 検証順序（決定的。同一入力には常に同一の [`SqlSurfaceError`] を返す）:
/// 1. 字句解析（入力長・トークン数上限を含む。失敗は [`SqlSurfaceError::UnsupportedSyntax`]）
/// 2. 構造の許可リスト判定（失敗は `UnsupportedSyntax`）
/// 3. `SELECT` の場合のみ、FROM 単一テーブルのカタログ存在確認
///    （不存在は [`SqlSurfaceError::UndefinedTable`]）
pub fn validate_sql(sql: &str, lookup: &impl TableLookup) -> Result<Statement, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_sql_tokens(&tokens, lookup)
}

/// [`validate_sql`] のトップレベル読み取り SELECT／集計 SELECT 経路でのみ
/// サブクエリ（`IN (SELECT ...)`／`EXISTS (SELECT ...)`）を許可するエントリ
/// ポイント（Issue #927・SQL-29 (a)・TASK-213）。`sql::subquery` が内側の
/// `Statement::Scan` を再検証する際にも、深さを 1 つ進めて
/// [`validate_sql_tokens_with_subquery_ctx`] を直接呼ぶ（本関数は深さ 0 固定の
/// トップレベル用薄いラッパー）。
pub(crate) fn validate_sql_with_subquery_ctx(
    sql: &str,
    lookup: &impl TableLookup,
) -> Result<Statement, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_sql_tokens_with_subquery_ctx(&tokens, lookup, 0)
}

/// [`validate_sql`] の本体（Issue #939・WIRE-17。COPY プロトコル対応の一環）。
/// トークン列を受け取ることで、`COPY (<SELECT>) TO STDOUT`
/// （[`validate_copy_to_tokens`]）の内側 SELECT のように、外側の許可リストが
/// 括弧で括り出した部分トークン列を再トークナイズせずに検証できる
/// （`validate_insert_tokens`・Issue #485 と同じ「トークン列を受け取る本体 /
/// 文字列を受け取り委譲する公開 API」という分割方針）。`validate_sql` は本関数へ
/// 委譲するだけで挙動・エラー契約は分割前と不変。
///
/// `sql::params`（Issue #935・WIRE-12。拡張クエリプロトコルの `$n` 束縛）も、Bind 時に
/// `Token::Param` を実値のトークンへ置換したトークン列を SQL テキストを経由せず
/// この関数へ渡し、[`validate_sql`] と同一の判定順序・エラー分類を再利用する。
/// `sql::view::resolve_from`・`sql::cte::resolve_relation` いずれの名前解決
/// 結果（[`super::view::Resolved`]）からも、広域取得クエリ自身の形（射影・
/// `WHERE`・`LIMIT`・`OFFSET`・ウィンドウ項目）を合成して [`ValidatedScan`] を
/// 組み立てる唯一の実装（TABLE-18・TASK-205、TASK-213・Issue #928、
/// SQL-30・TASK-214・Issue #930）。ビュー経由・CTE 経由のいずれの参照も
/// 畳み込み後は完全に同じ形になり、束縛・実行・RLS 適用はすべて既存経路を
/// そのまま通る（第 2 の実行器を作らない設計）。`window_items` が参照する列
/// （PARTITION BY／ORDER BY／集計引数）も、通常の投影・WHERE と同じく
/// ビュー・CTE の公開列集合の範囲内であることを検査する。
fn build_scan_from_resolved(
    table_name: String,
    resolved: super::view::Resolved,
    projection: Projection,
    where_predicates: Vec<WherePredicate>,
    limit: u32,
    offset: u32,
    window_items: Vec<WindowSelectItem>,
) -> Result<ValidatedScan, SqlSurfaceError> {
    match resolved {
        super::view::Resolved::Table => Ok(ValidatedScan {
            table_name,
            projection,
            where_predicates,
            limit,
            offset,
            window_items,
        }),
        super::view::Resolved::View {
            base_table,
            view_predicates,
            view_columns,
        } => {
            super::view::check_columns_within_view(
                view_columns.as_deref(),
                &projection,
                &where_predicates,
            )?;
            super::view::check_window_columns_within_view(view_columns.as_deref(), &window_items)?;
            let projection = if let (Projection::All, Some(cols)) = (&projection, &view_columns) {
                Projection::Columns(cols.clone())
            } else {
                projection
            };
            let mut merged = view_predicates;
            merged.extend(where_predicates);
            Ok(ValidatedScan {
                table_name: base_table,
                projection,
                where_predicates: merged,
                limit,
                offset,
                window_items,
            })
        }
    }
}

pub(crate) fn validate_sql_tokens(
    tokens: &[Token],
    lookup: &impl TableLookup,
) -> Result<Statement, SqlSurfaceError> {
    validate_sql_tokens_impl(tokens, lookup, None)
}

/// [`validate_sql_tokens`] のサブクエリ許可版（Issue #927・TASK-213）。
/// `depth` は現在位置の入れ子深さ（`sql::subquery::resolve_where_predicates`
/// が内側 `Statement::Scan` を再検証する際に 1 つ進めて渡す。トップレベルは
/// [`validate_sql_with_subquery_ctx`] が 0 を渡す）。
pub(crate) fn validate_sql_tokens_with_subquery_ctx(
    tokens: &[Token],
    lookup: &impl TableLookup,
    depth: usize,
) -> Result<Statement, SqlSurfaceError> {
    validate_sql_tokens_impl(tokens, lookup, Some(depth))
}

/// [`validate_sql_tokens`]／[`validate_sql_tokens_with_subquery_ctx`] が共有する
/// 本体。`subquery_ctx` が `None` の呼び出し（cursor・view・copy・params・
/// aggregate 等の既存の内部呼び出しがすべてここへ委譲する）は本 Issue 追加前と
/// 完全に同じ挙動（サブクエリ構文は `Parser::require_subquery_depth` が
/// `42601` で拒否する）を維持する。
fn validate_sql_tokens_impl(
    tokens: &[Token],
    lookup: &impl TableLookup,
    subquery_ctx: Option<usize>,
) -> Result<Statement, SqlSurfaceError> {
    // `SET`・`CREATE` は字句解析段階のキーワードではなく `Ident` のため
    // （TASK-161・SQL-12 修正と同方針）、statement 先頭という文脈でのみ大文字小文字を
    // 区別せず判定する。
    let is_set_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("SET"));
    // TASK-213・SQL-29 (b)（Issue #928）: `WITH`（非再帰 CTE）も `SET`・`CREATE`
    // と同方針で、statement 先頭という文脈でのみ大文字小文字を区別せず判定する。
    let is_with_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("WITH"));
    let is_create_function_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("CREATE"));
    // `EXPLAIN` も `SET`・`CREATE` と同方針（字句解析段階のキーワードにせず、
    // statement 先頭という文脈でのみ大文字小文字を区別せず判定する。TASK-78・SQL-6）。
    let is_explain_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("EXPLAIN"));
    match tokens.first() {
        // Issue #927・SQL-29 (a)・TASK-213 と Issue #922・SQL-27（EXPLAIN 対象
        // 拡大）の統合: 通常 SELECT・集計 SELECT・`SELECT DISTINCT`・広域取得の
        // 振り分けは [`validate_select_statement`] へ委譲する（第 2 の実装を
        // 持たない）。`subquery_ctx` をそのまま渡し、`IN (SELECT ...)`／
        // `EXISTS (SELECT ...)` を許可する文脈（トップレベルの読み取り
        // SELECT／集計 SELECT）でのみサブクエリを解決できるようにする
        // （`EXPLAIN` 経由の呼び出しは常に `None` を渡す。後述）。
        Some(Token::Keyword(Keyword::Select)) => {
            validate_select_statement(tokens, lookup, subquery_ctx)
        }
        // TASK-213・SQL-29 (b)・RLS-10 (b)（Issue #928）: 非再帰 CTE。CTE は
        // 「クエリの中だけで有効な名前なしビュー」として、`sql::cte::
        // resolve_relation` を経由し `sql::view::resolve_from` と同じ
        // `build_scan_from_resolved` へ合流させる（第 2 の実行器を作らない）。
        // 主クエリは広域取得（`ParsedSelect::Scan`）のみを受理し、順位付き
        // （`ORDER BY`／`USING PLAN`）・集計は明示的に拒否する（`WITH` 句を
        // 剥がして後段へ流すと同名の実テーブルを黙って読む危険があるため、
        // 絶対に行わない）。CTE と `EXPLAIN`・集計 SELECT との併用は
        // `is_explain_statement`／`validate_select_statement` 側の先読みが
        // 先頭トークンで振り分けるため、本アームには到達しない
        // （out-of-scope。Issue #928 対象外事項）。
        _ if is_with_statement => {
            let mut p = Parser::new(tokens);
            let ctes = p.parse_with_clause()?;
            let main_tokens = p.remaining();

            if !matches!(main_tokens.first(), Some(Token::Keyword(Keyword::Select))) {
                return Err(SqlSurfaceError::unsupported(
                    "WITH must be followed by a SELECT statement",
                ));
            }
            let main_contains_group_by = main_tokens.windows(2).any(|w| {
                matches!(&w[0], Token::Ident(name) if name.eq_ignore_ascii_case("GROUP"))
                    && matches!(w[1], Token::Keyword(Keyword::By))
            });
            // SQL-30・TASK-214（Issue #930）: トップレベル SELECT の判定
            // （`contains_window_over`）と同じ理由で、`WITH` の主クエリでも
            // `COUNT(*) OVER (...)` のようなウィンドウ項目を集計 SELECT と
            // 誤判定しない（`OVER` の有無を見ずに関数名 + `'('` だけで判定すると、
            // 集計関数名を使うウィンドウ項目を主クエリとして受理できなくなる）。
            let main_contains_window_over = main_tokens.windows(3).any(|w| {
                matches!(w[0], Token::Punct(')'))
                    && matches!(&w[1], Token::Ident(name) if name.eq_ignore_ascii_case("OVER"))
                    && matches!(w[2], Token::Punct('('))
            });
            let main_is_aggregate_select = !main_contains_window_over
                && ((matches!(main_tokens.get(1), Some(Token::Ident(name)) if is_aggregate_function_name(name))
                    && matches!(main_tokens.get(2), Some(Token::Punct('('))))
                    || main_contains_group_by);
            if main_is_aggregate_select {
                return Err(SqlSurfaceError::unsupported(
                    "WITH does not support an aggregate main query",
                ));
            }
            // Issue #927（サブクエリ）との併用は対象外（`docs/design/cte.md`
            // 対象外節）。`WITH` 主クエリは常に `subquery_ctx: None` で解析し、
            // 主クエリ内の `IN (SELECT ...)`／`EXISTS (...)` は既存の
            // `Parser::require_subquery_depth` 経路で一律 `42601` に落とす。
            let shape = match parse_select_shape(main_tokens, None)? {
                ParsedSelect::Scan(shape) => shape,
                ParsedSelect::Search(_) => {
                    return Err(SqlSurfaceError::unsupported(
                        "WITH does not support a ranked main query (ORDER BY / USING PLAN)",
                    ));
                }
            };

            // 参照されない CTE も含め、すべての定義本文を検証する（決定性・
            // fail-closed のため。構造検証・存在確認を省略しない）。
            // 上限カウンタ（`ResolveBudget`）は「1 回のトップレベル呼び出し
            // （1 つの CTE 定義の事前検証、または主クエリの解決）」ごとに
            // 新規生成する（`cte::MAX_CTE_REFERENCES` のドキュメント参照。
            // Issue #928 レビュー指摘）。文全体で 1 つのカウンタを使い回すと、
            // このループが各定義のチェーンを再帰的に辿るたびに参照回数が
            // 名前ごとではなく呼び出し回数分累積し、定義数・連鎖の深さの
            // どちらも上限内の有効なクエリを誤って `54000` で拒否する。
            // 連鎖の深さは `MAX_CTE_NESTING_DEPTH` で呼び出しごとに独立して
            // 上限が掛かるため、リセットしても DoS 対策としての上限は失われない。
            //
            // `resolve_relation` は `def.body.table_name`（FROM）の名前解決
            // 連鎖だけを検証し、`def` 自身の射影・`WHERE`（`compose` が本来
            // 適用する `check_columns_within_view`）は「他の CTE・主クエリから
            // 参照され `compose` を通る」場合にしか検証されない。参照されない
            // leaf CTE はどこからも `compose` されないため、FROM 解決結果
            // （`resolved` の公開列集合）に対して `def` 自身の射影・`WHERE` を
            // ここで明示的に検証しないと、先行 CTE が非公開列を隠していても
            // 参照されない後続 CTE がそれを射影・条件に使う文を通してしまう
            // （Issue #928 レビュー指摘: Codex P1・Cursor Bugbot Low、同一欠陥）。
            //
            // `resolved` の公開列集合が `None`（`Resolved::Table` か、連鎖の
            // どの段も列を絞り込んでいない `Resolved::View { view_columns: None,
            // .. }`）の場合、実テーブル直下の列存在検査は通常
            // `sql::parser::bind` に委ねている（`sql::view::resolve_from` の
            // 同種コメント参照）。これは**参照される** CTE・VIEW には妥当
            // （最終的に `ValidatedScan` へ畳み込まれ bind を通る）だが、
            // どこからも参照されない leaf CTE は bind に到達しないため、
            // ここで実テーブルのスキーマへ問い合わせて代わりに検証する
            // （PR #1100 追加レビュー指摘: Codex P1・Cursor Bugbot Low）。
            // `TableLookup::table_columns` の既定実装は `Ok(None)`（検査省略・
            // 後方互換）を返すため、これに対応しない `TableLookup` 実装
            // （テスト用モック等）は無変更のまま今まで通り動作する。
            for (idx, def) in ctes.iter().enumerate() {
                let mut budget = super::cte::ResolveBudget::new();
                let resolved = super::cte::resolve_relation(
                    lookup,
                    &ctes,
                    idx,
                    &def.body.table_name,
                    0,
                    &mut budget,
                )?;
                let (base_table, view_columns): (&str, Option<Vec<String>>) = match &resolved {
                    super::view::Resolved::Table => (def.body.table_name.as_str(), None),
                    super::view::Resolved::View {
                        base_table,
                        view_columns,
                        ..
                    } => (base_table.as_str(), view_columns.clone()),
                };
                let exposed = match view_columns {
                    Some(cols) => Some(cols),
                    None => lookup.table_columns(base_table)?,
                };
                super::view::check_columns_within_view(
                    exposed.as_deref(),
                    &def.body.projection,
                    &def.body.where_predicates,
                )?;
            }
            let mut budget = super::cte::ResolveBudget::new();
            let resolved = super::cte::resolve_relation(
                lookup,
                &ctes,
                ctes.len(),
                &shape.table_name,
                0,
                &mut budget,
            )?;
            Ok(Statement::Scan(build_scan_from_resolved(
                shape.table_name,
                resolved,
                shape.projection,
                shape.where_predicates,
                shape.limit,
                shape.offset,
                shape.window_items,
            )?))
        }
        _ if is_set_statement => {
            let value = parse_set_search_mode(tokens)?;
            Ok(Statement::SetSearchMode { value })
        }
        _ if is_create_function_statement => {
            let (name, params, body) = parse_create_function(tokens)?;
            Ok(Statement::CreateFunction { name, params, body })
        }
        // Issue #922（SQL-27）: `EXPLAIN` の対象を通常検索・集計・広域取得へ
        // 拡大した。先頭の `EXPLAIN` トークンを消費した残りを、非 EXPLAIN の
        // `SELECT` と完全に同じ振り分け・パース・カタログ存在確認
        // （[`validate_select_statement`]）へそのまま渡すことで、42601 → 42P01 →
        // 束縛エラーという判定順序が EXPLAIN の有無で変わらないことを構造的に
        // 保証する（第 2 の実装を持たない）。残り先頭が `SELECT` でない場合
        // （`EXPLAIN EXPLAIN`／`EXPLAIN SET`／`EXPLAIN CREATE FUNCTION`／
        // `EXPLAIN INSERT`／`EXPLAIN WITH` 等の DML・DDL・CTE）は許可リスト外
        // として一律 `42601`（`EXPLAIN` と CTE の併用は Issue #928 の対象外
        // 事項であり、`validate_select_statement` は `WITH` 句を扱わないため
        // 自然にここへ落ちる）。untrusted 入力経路のため添字アクセスではなく
        // `get` でスライスする（`.claude/rules/coding-rust.md`）。
        _ if is_explain_statement => {
            let rest = tokens.get(1..).unwrap_or(&[]);
            if !matches!(rest.first(), Some(Token::Keyword(Keyword::Select))) {
                return Err(SqlSurfaceError::unsupported(
                    "EXPLAIN requires a SELECT statement",
                ));
            }
            // Issue #927・SQL-29 (a)・TASK-213: `EXPLAIN` はサブクエリを許可する
            // 文脈に含めない（`docs/design/sql-subquery.md` 参照。`subquery_ctx`
            // に `None` を渡し、`IN (SELECT ...)`／`EXISTS (SELECT ...)` は
            // 通常どおり `Parser::require_subquery_depth` が `42601` で拒否する）。
            let target = match validate_select_statement(rest, lookup, None)? {
                Statement::Select(v) => ExplainTarget::Search(v),
                Statement::Aggregate(v) => ExplainTarget::Aggregate(v),
                // SQL-30・TASK-214（Issue #930）: `EXPLAIN` はウィンドウ関数に
                // 未対応（§計画 2「対象外」）。`validate_select_statement` は
                // 非 EXPLAIN 経路と共有のため window_items を持つ `ValidatedScan`
                // も返しうるが、`sql::explain` 側に対応する記述がなくレビュー
                // 未了のため、ここで明示的に `42601` へ倒す。
                Statement::Scan(v) if !v.window_items.is_empty() => {
                    return Err(SqlSurfaceError::unsupported(
                        "EXPLAIN is not supported for window function queries",
                    ));
                }
                Statement::Scan(v) => ExplainTarget::Scan(v),
                // `validate_select_statement` は `SELECT` 先頭のトークン列に
                // 対して常に `Select`／`Aggregate`／`Scan` のいずれかを返す
                // （関数ドキュメント参照）。到達は同関数の契約違反時のみの
                // 防御的経路として fail-closed に拒否する（`unreachable!` の
                // panic ではなくエラー応答にする。ライブラリコードは panic
                // させない方針。`.claude/rules/coding-rust.md`）。
                Statement::SetSearchMode { .. }
                | Statement::CreateFunction { .. }
                | Statement::Explain(_) => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "validate_select_statement returned a non-SELECT statement"
                            .to_string(),
                    });
                }
            };
            Ok(Statement::Explain(target))
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected SELECT, SET, CREATE FUNCTION, or EXPLAIN, got {other:?}"
        ))),
    }
}

/// 通常 SELECT・集計 SELECT（`GROUP BY` の有無いずれも）・`SELECT DISTINCT`
/// の脱糖形・広域取得（ビュー展開後の形を含む）の共有振り分け（TASK-166・
/// SQL-13／TASK-167・SQL-14／SQL-25 (c)・TASK-209／Issue #454 の先読み判定と
/// パースを 1 箇所へ集約したもの。Issue #922・SQL-27）。`tokens` の先頭は
/// `SELECT` キーワードであることが前提（呼び出し元が
/// `matches!(tokens.first(), Some(Token::Keyword(Keyword::Select)))` を
/// 確認済みであること。違反時の挙動は各内部パーサーの構文エラーに委ねる）。
/// 戻り値は常に [`Statement::Select`]・[`Statement::Aggregate`]・
/// [`Statement::Scan`] のいずれか（[`Statement::SetSearchMode`]・
/// [`Statement::CreateFunction`]・[`Statement::Explain`] を返すことはない）。
///
/// [`validate_sql_tokens`] の非 EXPLAIN 経路と `EXPLAIN`（[`ExplainTarget`]）
/// の両方がこの関数を呼ぶことで、`EXPLAIN` の対象拡大が既存の判定順序
/// （`42601` → `42P01` → 束縛エラー）を変えないことを構造的に保証する
/// （第 2 の実装を持たない）。
///
/// `subquery_ctx`（Issue #927・SQL-29 (a)・TASK-213）は集計 SELECT・広域取得
/// （検索 SELECT・`SELECT DISTINCT` は非対応のまま）の `WHERE` に
/// `IN (SELECT ...)`／`EXISTS (SELECT ...)` を許可するかどうか
/// （`None`＝不許可）。`sql::allowlist::validate_sql_tokens_impl`（トップレベル
/// の読み取り SELECT／集計 SELECT）は自身が受け取った値をそのまま渡し、
/// `EXPLAIN` 経由の呼び出しは常に `None` を渡す（`docs/design/sql-subquery.md`
/// 参照。`EXPLAIN` はサブクエリを許可する文脈に含めない）。
fn validate_select_statement(
    tokens: &[Token],
    lookup: &impl TableLookup,
    subquery_ctx: Option<usize>,
) -> Result<Statement, SqlSurfaceError> {
    // TASK-166（SQL-13）: `SELECT` の直後（2 番目・3 番目のトークン）が
    // 集計関数名 `'('` なら集計 SELECT 形状（[`parse_aggregate_shape`]）へ、それ
    // 以外は既存の検索 SELECT 形状（[`parse_select_shape`]）へ分岐する。バック
    // トラックせず先読みだけで確定させる（`Parser::pos` の巻き戻しに依存しない）。
    // TASK-167（SQL-14）: トークン列中に文脈キーワード `GROUP` → `BY` の並びが
    // あれば、集計項目が SELECT リストの先頭に来ない形（`SELECT <col>, <agg>(...)
    // FROM t GROUP BY <col>`）も集計 SELECT 形状へ振り分ける。`GROUP`/`BY` は
    // どちらも他の文脈で通常の識別子・既存の `ORDER BY` の一部として現れうるが、
    // 「`Ident("GROUP")` の直後に `Keyword::By`」という並びは既存の許可形状には
    // 存在しないため、フォールス・ポジティブなく集計形状の目印として使える。
    let contains_group_by = tokens.windows(2).any(|w| {
        matches!(&w[0], Token::Ident(name) if name.eq_ignore_ascii_case("GROUP"))
            && matches!(w[1], Token::Keyword(Keyword::By))
    });
    // SQL-30・TASK-214（Issue #930）: `SELECT COUNT(*) OVER (...) ...` のような
    // ウィンドウ項目は、先頭の集計関数名 `'('` という同じ字面を持つため
    // 集計 SELECT と誤判定される（`COUNT` の直後の `'('` を見るだけでは `OVER`
    // の有無が分からない）。トークン列全体に「`')'` の直後に文脈識別子
    // `OVER`、その直後に `'('`」という並びがあるかを走査して判定する。この
    // 並びは既存の許可形状（式・集計・WHERE のいずれも）には現れないため、
    // フォールス・ポジティブなくウィンドウ項目の目印として使える。
    let contains_window_over = tokens.windows(3).any(|w| {
        matches!(w[0], Token::Punct(')'))
            && matches!(&w[1], Token::Ident(name) if name.eq_ignore_ascii_case("OVER"))
            && matches!(w[2], Token::Punct('('))
    });
    let is_aggregate_select = !contains_window_over
        && ((matches!(tokens.get(1), Some(Token::Ident(name)) if is_aggregate_function_name(name))
            && matches!(tokens.get(2), Some(Token::Punct('('))))
            || contains_group_by);
    // SQL-30・TASK-214: ウィンドウ項目は `GROUP BY`・集計 SELECT・`SELECT DISTINCT`
    // のいずれとも併用しない（§計画 2「対象外」）。`is_aggregate_select` の判定
    // から除外しただけでは `contains_group_by` 単独の形（`SELECT ... OVER (...)
    // ... GROUP BY ...`）が `parse_select_shape`（広域取得）へ素通りしてしまうため、
    // ここで明示的に `42601` へ倒す。
    if contains_window_over && contains_group_by {
        return Err(SqlSurfaceError::unsupported(
            "window functions cannot be combined with GROUP BY",
        ));
    }
    // SQL-25 (c)・TASK-209: `SELECT` の直後（2 番目のトークン）が
    // [`is_distinct_modifier`] の判定する `DISTINCT` 修飾子なら `SELECT DISTINCT`
    // 形状（[`parse_distinct_shape`]）へ振り分ける。`is_aggregate_select`
    // （`GROUP BY` を含む形）より前に判定する（`SELECT DISTINCT lang, COUNT(*)
    // FROM t GROUP BY lang` のような両方に一致しうる入力は存在しない——
    // `parse_distinct_shape` は単一の裸列参照のみを受理するため、集計項目や
    // 複数列を伴う形は自然に `42601` へ落ちる）。
    let is_distinct_select = is_distinct_modifier(tokens, 1);

    if is_distinct_select {
        let shape = parse_distinct_shape(tokens)?;
        let exists = lookup.table_exists(&shape.table_name)?;
        if !exists {
            return Err(SqlSurfaceError::undefined_table(shape.table_name));
        }
        return Ok(Statement::Aggregate(ValidatedAggregate {
            table_name: shape.table_name,
            items: shape.items,
            where_predicates: shape.where_predicates,
            group_by: shape.group_by,
        }));
    }
    if is_aggregate_select {
        let shape = parse_aggregate_shape(tokens, subquery_ctx)?;
        let exists = lookup.table_exists(&shape.table_name)?;
        if !exists {
            return Err(SqlSurfaceError::undefined_table(shape.table_name));
        }
        return Ok(Statement::Aggregate(ValidatedAggregate {
            table_name: shape.table_name,
            items: shape.items,
            where_predicates: shape.where_predicates,
            group_by: shape.group_by,
        }));
    }
    match parse_select_shape(tokens, subquery_ctx)? {
        ParsedSelect::Search(shape) => {
            let exists = lookup.table_exists(&shape.table_name)?;
            if !exists {
                return Err(SqlSurfaceError::undefined_table(shape.table_name));
            }
            Ok(Statement::Select(ValidatedStatement {
                table_name: shape.table_name,
                projection: shape.projection,
                order_by: shape.order_by,
                where_predicates: shape.where_predicates,
                limit: shape.limit,
                search_mode: shape.search_mode,
                evaluation_order: shape.evaluation_order,
                using_plan: shape.using_plan,
            }))
        }
        // Issue #454: `ORDER BY`・`USING PLAN` のいずれも伴わない
        // `SELECT ... [WHERE ...] LIMIT n`（広域取得）。TABLE-18・SQL-23・
        // TASK-205（Issue #909）: FROM がビュー（`CREATE VIEW`）を指す場合、
        // `sql::view::resolve_from` が連鎖を畳み込んで基底テーブル名＋
        // 合成済み `WHERE` 述語へ書き換える。書き換え後は通常のテーブル
        // 参照と完全に同じ `ValidatedScan` になり、束縛・実行・RLS 適用は
        // すべて既存経路をそのまま通る（第 2 の実行器を作らない）。
        ParsedSelect::Scan(shape) => {
            let resolved = super::view::resolve_from(lookup, &shape.table_name)?;
            Ok(Statement::Scan(build_scan_from_resolved(
                shape.table_name,
                resolved,
                shape.projection,
                shape.where_predicates,
                shape.limit,
                shape.offset,
                shape.window_items,
            )?))
        }
    }
}

/// `SELECT` 文のみを受理する後方互換 API（TASK-74・TASK-75 が既に依存している
/// シグネチャを維持する）。[`validate_sql`]（TASK-161）へ委譲し、`SELECT` 以外
/// （`SET search_mode` 等）は「このエントリポイントでは受理しない statement 形」
/// として `42601` で拒否する（`SET` のリテラル値自体が妥当でも、それを保持する
/// セッションを持たないこのエントリポイントでは意味を持たないため。黙った
/// no-op にはしない）。
pub fn validate_statement(
    sql: &str,
    lookup: &impl TableLookup,
) -> Result<ValidatedStatement, SqlSurfaceError> {
    match validate_sql(sql, lookup)? {
        Statement::Select(stmt) => Ok(stmt),
        Statement::SetSearchMode { .. } => Err(SqlSurfaceError::unsupported(
            "SET is not a query statement (use a session-aware entry point)",
        )),
        Statement::CreateFunction { .. } => Err(SqlSurfaceError::unsupported(
            "CREATE FUNCTION is not a query statement (use a session-aware entry point)",
        )),
        // TASK-166（SQL-13）: 集計 SELECT は `ValidatedStatement`（検索 SELECT 専用の
        // 形）を持たないため、このエントリポイントでは受理しない（`SET`・
        // `CREATE FUNCTION` と同じ「このエントリポイントでは非対応」の一律 `42601`）。
        Statement::Aggregate(_) => Err(SqlSurfaceError::unsupported(
            "aggregate SELECT is not a search query statement (use a session-aware entry point)",
        )),
        // TASK-78（SQL-6）: `EXPLAIN` は `ValidatedStatement` を包んで返すものの、
        // 「検索本体を実行しない」という別の実行契約を持つため、`SET`・
        // `CREATE FUNCTION`・`Aggregate` と同じくこのセッションなしエントリ
        // ポイントでは受理しない（一律 `42601`）。
        Statement::Explain(_) => Err(SqlSurfaceError::unsupported(
            "EXPLAIN is not a search query statement (use a session-aware entry point)",
        )),
        // Issue #454: 広域取得は `ValidatedStatement`（検索 SELECT 専用の形）を
        // 持たないため、`Aggregate` と同じくこのエントリポイントでは受理しない
        // （一律 `42601`）。
        Statement::Scan(_) => Err(SqlSurfaceError::unsupported(
            "wide-retrieval scan is not a search query statement (use a session-aware entry point)",
        )),
    }
}

/// 構文木（[`ValidatedInsert`] の元）。カタログ存在確認前の中間結果
/// （SQL-10・SQL-16、TASK-80・TASK-190）。
struct ParsedInsertShape {
    table_name: String,
    columns: Vec<String>,
    rows: Vec<Vec<InsertLiteral>>,
    operation_id: Option<OperationId>,
    returning: Option<Projection>,
    on_conflict: Option<OnConflictAction>,
}

/// `DELETE` の `WHERE` 句の構造形状（Issue #870・SQL-19）。単一行・`id` 完全
/// 一致指定形（[`RowId`](Self::RowId)。SQL-18・TASK-191 の既存受理範囲）と、
/// `SELECT`／集計／広域取得が共有する [`WherePredicate`] 列で表す述語形
/// （[`Predicates`](Self::Predicates)）の 2 種。[`Parser::parse_delete`] の
/// 先読み（[`Parser::peek_single_row_delete_id`]）が単一行形と完全に一致する
/// 入力だけを `RowId` へ分類し、それ以外はすべて `Predicates` へ落ちる。
enum ParsedDeleteWhere {
    RowId { id_literal: String },
    Predicates(Vec<WherePredicate>),
}

/// 構文木（[`ValidatedDelete`]／[`ValidatedPredicateDelete`] の元）。カタログ
/// 存在確認前の中間結果（SQL-18・SQL-19、TASK-191・TASK-192）。
struct ParsedDeleteShape {
    table_name: String,
    where_clause: ParsedDeleteWhere,
    operation_id: Option<OperationId>,
    returning: Option<Projection>,
}

/// [`Parser::parse_delete`] ＋ 文末検証を共有する private ヘルパー（Issue #870）。
/// [`validate_delete_tokens`]（単一行形専用入口）・
/// [`validate_delete_statement_tokens`]（述語形を含む入口）の双方がここを
/// 通ることで、構文解析そのものを複製しない。
fn parse_delete_statement_shape(
    tokens: &[lexer::Token],
) -> Result<ParsedDeleteShape, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let shape = p.parse_delete()?;
    p.expect_end_of_statement()?;
    Ok(shape)
}

/// `DELETE` 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を通じて
/// FROM テーブルがカタログに実在するかを確認する（SQL-18・TASK-191 の公開 API）。
/// `validate_sql`（SELECT 系専用）・`validate_insert` とは独立した
/// エントリポイントとする（`operation_id` 必須化ガードをカタログ照会より前に
/// 評価する必要があるため。[`validate_insert`] のドキュメント参照）。単一行・
/// `id` 完全一致形のみを受理し、述語形（Issue #870・TASK-192・SQL-19）は
/// [`validate_delete_statement`] の管轄として構造段（`mode.require`・カタログ
/// 照会より前）で `42601` を返す（優先順位の保存は [`validate_delete_tokens`]
/// のドキュメント参照）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）:
/// 構造検証 → `operation_id` 必須化ガード（`mode.require`。TASK-92・RECOVER-1）
/// → FROM テーブルのカタログ存在確認。
pub fn validate_delete(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedDelete, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_delete_tokens(&tokens, lookup, mode)
}

/// [`validate_delete`] の本体。トークン列を受け取ることで、呼び出し元
/// （#867 が結線する `core.rs::execute_sql_in_session` 相当）が既に先頭
/// トークン判定のためにトークナイズ済みの場合、同一 SQL 文字列の再
/// トークナイズを避けられる（`validate_insert_tokens`／Issue #485 と同じ設計）。
///
/// 述語形（[`ParsedDeleteWhere::Predicates`]）は、[`validate_delete`] 導入時
/// （SQL-18・TASK-191）からの受理範囲を一切広げないため、`mode.require`・
/// カタログ照会に進む前の構造判定の時点で `42601` を返す（Issue #870 追加後も
/// `validate_delete` のエラー優先順位契約 —— 述語形は `operation_id` の
/// 有無・テーブルの実在によらず常に `42601` —— を保存する）。
pub(crate) fn validate_delete_tokens(
    tokens: &[lexer::Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedDelete, SqlSurfaceError> {
    let shape = parse_delete_statement_shape(tokens)?;

    let id_literal = match shape.where_clause {
        ParsedDeleteWhere::RowId { id_literal } => id_literal,
        ParsedDeleteWhere::Predicates(_) => {
            return Err(SqlSurfaceError::unsupported(
                "predicate DELETE must be dispatched via validate_delete_statement",
            ));
        }
    };

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(ValidatedDelete {
        table_name: shape.table_name,
        id_literal,
        operation_id: shape.operation_id,
        returning: shape.returning,
    })
}

/// `DELETE` 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を
/// 通じて FROM テーブルがカタログに実在するかを確認する（Issue #870・
/// TASK-192・SQL-19 の公開 API）。単一行・`id` 完全一致形（[`validate_delete`]
/// の既存受理範囲）に加え、述語形 `WHERE`（等価・前方一致・`visible()`・式
/// 比較の `AND` 結合。`SELECT`／集計／広域取得と共有する述語表現）を受理する
/// 唯一の入口。実行結線（可視行列挙・1 トランザクション一括適用・台帳照合。
/// #871 の担当）はこの入口を使う。
///
/// 検証順序は [`validate_delete`] と同一（決定的）: 構造検証 →
/// `operation_id` 必須化ガード（`mode.require`。TASK-92・RECOVER-1）→
/// FROM テーブルのカタログ存在確認。
pub fn validate_delete_statement(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<DeleteStatement, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_delete_statement_tokens(&tokens, lookup, mode)
}

/// [`validate_delete_statement`] の本体。トークン列を受け取ることで、呼び出し
/// 元が既にトークナイズ済みの場合の再トークナイズを避ける
/// （[`validate_delete_tokens`]・`validate_insert_tokens` と同じ設計）。
pub(crate) fn validate_delete_statement_tokens(
    tokens: &[lexer::Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<DeleteStatement, SqlSurfaceError> {
    let shape = parse_delete_statement_shape(tokens)?;

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(match shape.where_clause {
        ParsedDeleteWhere::RowId { id_literal } => DeleteStatement::SingleRow(ValidatedDelete {
            table_name: shape.table_name,
            id_literal,
            operation_id: shape.operation_id,
            returning: shape.returning,
        }),
        ParsedDeleteWhere::Predicates(where_predicates) => {
            // 述語形 DELETE の実行結線（#871）は未着手のため、`RETURNING` を
            // 黙って保持し将来の実行器が無視する fail-open を防ぐ単一の
            // チョークポイント（Issue #873・SQL-21）。単一行形（上の腕）は
            // 実行結線済み（`sql::exec::execute_delete_returning`）のため
            // 受理する。
            if shape.returning.is_some() {
                return Err(SqlSurfaceError::unsupported(
                    "RETURNING is not supported for predicate-form DELETE",
                ));
            }
            DeleteStatement::Predicate(ValidatedPredicateDelete {
                table_name: shape.table_name,
                where_predicates,
                operation_id: shape.operation_id,
            })
        }
    })
}

/// INSERT 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を通じて
/// INTO テーブルがカタログに実在するかを確認する（SQL-10、TASK-80 の公開 API）。
/// `validate_statement`（SELECT 専用、TASK-74）とは独立したエントリポイントとする
/// （SELECT 文に `USING OPERATION_ID` を付けた入力は `validate_statement` 側の
/// `expect_end_of_statement` が余剰トークンとして `42601` で拒否するため、
/// SELECT/INSERT を誤って混同受理する経路は構造的に存在しない）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）。
/// `operation_id` 必須化ガード（`mode.require`。TASK-92・RECOVER-1）を含む段階構成の
/// 詳細は `recovery::required_op_id` モジュールドキュメント参照。
pub fn validate_insert(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedInsert, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_insert_tokens(&tokens, lookup, mode)
}

/// [`validate_insert`] の本体（Issue #485・単文 INSERT 経路の上位段改善）。
/// トークン列を受け取ることで、呼び出し元
/// （`core.rs::execute_sql_in_session`）が既に先頭トークン判定のために
/// `tokenize` 済みの場合、同一 SQL 文字列の再トークナイズを避けられる
/// （TASK-83 条件7・Issue #314 で `execute_validated_in_session` が SELECT 側に
/// 行った「二重パース排除」の INSERT 側対応）。`validate_insert`（`sql: &str`
/// を受け取る公開 API）は内部でトークナイズしてから本関数へ委譲するため、
/// 挙動・エラー契約・検証順序はいずれも分割前と不変。
pub(crate) fn validate_insert_tokens(
    tokens: &[lexer::Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedInsert, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let shape = p.parse_insert()?;
    p.expect_end_of_statement()?;

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(ValidatedInsert {
        table_name: shape.table_name,
        columns: shape.columns,
        rows: shape.rows,
        operation_id: shape.operation_id,
        returning: shape.returning,
        on_conflict: shape.on_conflict,
    })
}

/// TRUNCATE 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を
/// 通じて対象テーブルがカタログに実在するかを確認する（SQL-22、TASK-195 の
/// 公開 API）。`validate_insert` と全く同じ設計（構造検証のみを担当し、意味論
/// 検証・実行本体は呼び出し元 `sql::exec::execute_truncate` へ委譲する）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）。
/// `operation_id` 必須化ガード（`mode.require`。TASK-92・RECOVER-1）を含む段階構成の
/// 詳細は `recovery::required_op_id` モジュールドキュメント参照。
/// 先頭 2 トークンが文脈的キーワード `CREATE`／`TABLE`（大文字小文字を区別しない。
/// `is_create_function_statement`〔`validate_sql_tokens`〕と同じ「statement 先頭
/// という文脈でのみキーワードとして判定する」方針）に一致するかを判定する
/// （SQL-23・TASK-202、Issue #899）。`core.rs::EngineCore::parse_tokens` が
/// [`validate_create_table_tokens`]（構造検証のみ・カタログ照会なし）へ分岐
/// するために使う。`parse_tokens` は本関数（構造検証のみ・カタログ照会なし）を
/// DDL 実行権限ゲートより先に実行するため、不正な構文は権限の有無に関わらず
/// 常に構文エラー（`42601`）になる。DDL 実行権限ゲート
/// （`sql::ddl::require_ddl_permission`）は `DROP TABLE` と同じく
/// `execute_parsed_in_session` の `ParsedSql::CreateTable` 分岐が、カタログ照会を
/// 含む実行本体より前に適用する——構文検証を通過した文に限り、未許可の主体は
/// テーブルが存在するかに関わらず同じ `InsufficientPrivilege`（`42501`）のみを
/// 受け取り、その情報を一切観測できない（fail-closed。`sql::ddl` モジュール
/// ドキュメント参照）。
pub(crate) fn is_create_table_statement(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("CREATE"))
        && matches!(tokens.get(1), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("TABLE"))
}

/// [`is_create_table_statement`] が真を返したトークン列から `CREATE TABLE`
/// の許可形状を構造検証する（SQL-23・TASK-85、Issue #899）。カタログ照会
/// （既存テーブル名との衝突判定）は行わない——[`ValidatedCreateTable`] の
/// ドキュメント参照のとおり、`sql::ddl::execute_create_table` が単一の
/// 書き込みトランザクション内で TOCTOU なく判定する。
///
/// `pub`（crate 外部へ公開。NOSQL-13・TASK-207、Issue #910）: NoSQL 表層
/// （`wire-server` の `http/query/ddl.rs`）が JSON の DDL 要求をトークン列へ
/// 写像し、本関数へ直接渡す入口として使う。`Self::parse_sql_prepared`／
/// `bind_prepared` が `$n` を実値の `Token::StringLiteral` へ置換した
/// トークン列を「SQL テキストへ戻さず」パーサーへ渡す前例と同じ設計
/// （文字列連結ではなく構造化トークン列を渡す）。この入口はカタログ照会を
/// 一切行わない契約を維持する——呼び出し元は本関数の戻り値を
/// [`crate::core::ParsedSql::CreateTable`] へ包んで
/// `EngineCore::execute_parsed_in_session` に渡し、DDL 実行権限ゲート
/// （`sql::ddl::require_ddl_permission`）・カタログ照会を含む実行本体は
/// 単一の実行器（`execute_parsed_in_session`）に委ねる（第 2 の DDL 実行器・
/// 第 2 の権限判定を作らない）。
pub fn validate_create_table_tokens(
    tokens: &[Token],
) -> Result<ValidatedCreateTable, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let validated = p.parse_create_table()?;
    p.expect_end_of_statement()?;
    Ok(validated)
}

/// カタログに永続化された `CHECK` 制約の正規化述語テキスト（`sql::check_constraint::
/// render_predicates` が生成する）を、`WHERE` 句と同じ文法で再パースする
/// （TABLE-16・TASK-204、Issue #906）。`CREATE TABLE` 実行時（往復一致検証）と
/// 書き込み時（`sql::check_constraint::CompiledChecks::compile`）の双方が呼ぶ
/// 唯一の再パース経路（第 2 のパーサーを作らない）。末尾に余剰トークンが残る
/// 場合は `42601`（往復不能な入力・手書きの破損データのいずれもここで検知する）。
pub(crate) fn parse_check_predicate_text(
    sql: &str,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    let mut p = Parser::new(&tokens);
    let predicates = p.parse_where()?;
    p.expect_end_of_statement()?;
    Ok(predicates)
}

pub fn validate_truncate(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedTruncate, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_truncate_tokens(&tokens, lookup, mode)
}

/// [`validate_truncate`] の本体。トークン列を受け取ることで、呼び出し元
/// （`core.rs::execute_sql_in_session`）が既に先頭トークン判定のために
/// `tokenize` 済みの場合、同一 SQL 文字列の再トークナイズを避けられる
/// （`validate_insert_tokens` と同じ設計。Issue #485 の INSERT 側対応を踏襲）。
pub(crate) fn validate_truncate_tokens(
    tokens: &[lexer::Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedTruncate, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let shape = p.parse_truncate()?;
    p.expect_end_of_statement()?;

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(ValidatedTruncate {
        table_name: shape.table_name,
        operation_id: shape.operation_id,
    })
}

/// `ALTER TABLE ADD COLUMN` 文をトークン化し、許可リスト形式で構造検証する
/// （TASK-202・SQL-23。Issue #900 の公開 API）。`validate_truncate` とは異なり
/// **カタログ照会（`TableLookup::table_exists`）を一切行わない**——DDL 権限
/// ゲート（`sql::ddl::require_ddl_permission`）より先にテーブルの存在有無を
/// 返すと、権限の無い主体に対する存在オラクルになるため（`ValidatedAlterTableAddColumn`
/// のドキュメント参照）。テーブル・列の存在確認、型名解決（ENUM 型名の存在確認・
/// `VECTOR` 列の `0A000` 拒否を含む）は権限ゲート通過後の実行段
/// （`sql::ddl::execute_alter_table_add_column`）が担う。
pub fn validate_alter_table(sql: &str) -> Result<ValidatedAlterTableAddColumn, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_alter_table_tokens(&tokens)
}

/// [`validate_alter_table`] の本体。トークン列を受け取ることで、呼び出し元
/// （`core.rs::EngineCore::parse_tokens`）が既に先頭トークン判定のために
/// `tokenize` 済みの場合、同一 SQL 文字列の再トークナイズを避けられる
/// （`validate_truncate_tokens` と同じ設計）。
/// `pub`（crate 外部へ公開。NOSQL-13・TASK-207、Issue #910）:
/// [`validate_create_table_tokens`] と同じ契約で NoSQL 表層（`wire-server`
/// `http/query/ddl.rs`）が JSON の `alter_table.add_column` をトークン列へ
/// 写像して渡す入口とする。
pub fn validate_alter_table_tokens(
    tokens: &[lexer::Token],
) -> Result<ValidatedAlterTableAddColumn, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let shape = p.parse_alter_table_add_column()?;
    p.expect_end_of_statement()?;

    Ok(ValidatedAlterTableAddColumn {
        table_name: shape.table_name,
        column_name: shape.column_name,
        column_type: shape.column_type,
    })
}

/// `DROP TABLE` 文をトークン化し、許可リスト形式で構造検証する（SQL-23、
/// TASK-203、Issue #902 の公開 API）。`validate_truncate`・`validate_insert` と
/// 異なり **`TableLookup` を取らない**——DDL 実行権限ゲート
/// （[`crate::sql::ddl::require_ddl_permission`]）を対象テーブルの存在確認より
/// 先に通す契約（`ValidatedDropTable` ドキュメント参照）のため、本関数は
/// カタログへ一切問い合わせない。存在確認は `sql::ddl::execute_drop_table` が
/// 書き込みトランザクション内で行う。
pub fn validate_drop_table(sql: &str) -> Result<ValidatedDropTable, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_drop_table_tokens(&tokens)
}

/// [`validate_drop_table`] の本体。`core.rs::EngineCore::parse_tokens` が既に
/// 字句解析済みの場合、同一 SQL 文字列の再トークナイズを避けるために使う
/// （`validate_truncate_tokens` と同じ設計）。
/// `pub`（crate 外部へ公開。NOSQL-13・TASK-207、Issue #910）:
/// [`validate_create_table_tokens`] と同じ契約で NoSQL 表層（`wire-server`
/// `http/query/ddl.rs`）が JSON の `drop_table` をトークン列へ写像して渡す
/// 入口とする。
pub fn validate_drop_table_tokens(
    tokens: &[lexer::Token],
) -> Result<ValidatedDropTable, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let table_name = p.parse_drop_table()?;
    p.expect_end_of_statement()?;
    Ok(ValidatedDropTable { table_name })
}

/// `COPY ... FROM STDIN`／`COPY (...) TO STDOUT` の転送形式（Issue #939・
/// WIRE-17）。`text`（PostgreSQL 互換のタブ区切り・バックスラッシュエスケープ）・
/// `csv`（RFC4180 風のカンマ区切り・二重引用符エスケープ）の 2 値のみを受理し、
/// `HEADER`・`DELIMITER`・`NULL`・`QUOTE`・binary 形式（WIRE-14）は許可リスト外
/// （`42601`）。フィールド分割・エスケープ解決の実体は [`crate::sql::copy`] が
/// 担う（本モジュールは構文の許可リスト判定のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Text,
    Csv,
}

/// 許可形状の構造判定を通過した `COPY <table> (<col>[, <col>]*) FROM STDIN
/// [WITH] [(FORMAT text|csv)] USING OPERATION_ID '<id>'` 文（Issue #939・
/// WIRE-17・TASK-220）。`ValidatedInsert` と同様、本モジュールが保証するのは
/// ここまでの構造情報のみで、列名・値の意味論的妥当性・実際のフレーム
/// デコードは [`crate::sql::copy`] の責務とする。`RETURNING`・`ON CONFLICT`・
/// ファイル名指定（`FROM '<path>'`）は許可リスト外。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCopyFrom {
    /// INTO 相当で指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    /// 列リストの宣言順（`id` 疑似列を必ず含む。重複はここで拒否済み）。
    pub columns: Vec<String>,
    pub format: CopyFormat,
    /// 文末専用句で搬送された、検証済みの `operation_id`。句の欠落・明示
    /// `NULL` はいずれも `None`（`ValidatedInsert::operation_id` と同じ契約。
    /// `LedgerMode::Ledgered`（既定）では `None` を書き込みトランザクション
    /// 開始前に `23502` で拒否するため、この構成では常に `Some`）。
    pub operation_id: Option<OperationId>,
}

/// 許可形状の構造判定を通過した `COPY (<SELECT>) TO STDOUT [[WITH]
/// (FORMAT text|csv)]` 文（Issue #939・WIRE-17・TASK-220）。内側 `SELECT` は
/// 広域取得（[`ValidatedScan`]。SQL-15・Issue #454）の形のみを受理し
/// （順位付け `ORDER BY`／`USING PLAN`・集計は `42601`）、[`validate_sql_tokens`]
/// と単一の実装を共有する（第 2 の SELECT パーサーを持たない）。テーブル形
/// `COPY <table> TO STDOUT` は対象外（許可リスト外）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCopyTo {
    pub inner: Box<ValidatedScan>,
    pub format: CopyFormat,
}

/// [`validate_copy`]（Issue #939・WIRE-17）が返す COPY statement 種別。
#[derive(Debug, Clone, PartialEq)]
pub enum CopyStatement {
    From(ValidatedCopyFrom),
    To(ValidatedCopyTo),
}

/// `(<items>)` の対応する丸括弧を見つけ、内側・外側後続のトークン列へ分割する
/// （`COPY (<SELECT>) TO STDOUT` の内側 SELECT、`WITH <name> AS (<body>)`
/// の CTE 本文〔TASK-213・SQL-29 (b)、Issue #928〕の双方が使う唯一の実装。
/// 文字列リテラル内の `(`/`)` は字句解析時点で既に 1 個の `Token::StringLiteral`
/// へ吸収されているため、本関数はトークン列上の `Token::Punct('('/')')` だけを
/// 深さで数えれば安全に対応を取れる）。`tokens` の先頭は必ず `(` であること
/// （呼び出し元が確認済み）。添字直接アクセス（`coding-rust.md`）を避け
/// `enumerate`＋`get` で処理する。
fn split_parenthesized(tokens: &[Token]) -> Result<(&[Token], &[Token]), SqlSurfaceError> {
    let mut depth: i32 = 0;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    let inner = tokens.get(1..i).ok_or_else(|| {
                        SqlSurfaceError::unsupported("malformed parenthesized clause")
                    })?;
                    let after = tokens.get(i + 1..).ok_or_else(|| {
                        SqlSurfaceError::unsupported("malformed parenthesized clause")
                    })?;
                    return Ok((inner, after));
                }
            }
            _ => {}
        }
    }
    Err(SqlSurfaceError::unsupported(
        "unterminated parenthesized expression",
    ))
}

/// `[WITH] (FORMAT text|csv)` を受理する（省略時は [`CopyFormat::Text`]）。
/// `WITH` を書いた場合は直後の `(FORMAT ...)` を必須とし、無ければ `42601` で
/// 拒否する（値を伴わない `WITH` 句を既定 `Text` として誤受理しない）。
/// `COPY ... FROM STDIN`・`COPY (...) TO STDOUT` の両方から共有する（Issue #939）。
fn parse_optional_copy_format(p: &mut Parser) -> Result<CopyFormat, SqlSurfaceError> {
    // `WITH` を消費したら `(FORMAT ...)` の丸括弧を必須とする。`WITH` の直後に
    // `(` が続かない場合（値を伴わない不正な `WITH` 句）は許可リスト外として
    // `42601` で拒否する（WIRE-17 の許可形状〔`FORMAT text|csv` のみ〕・
    // fail-closed 規約。Issue #939 codex-review 指摘の是正）。
    let with_seen = p.peek_contextual_keyword("WITH");
    if with_seen {
        p.advance();
    }
    if !matches!(p.peek(), Some(Token::Punct('('))) {
        if with_seen {
            return Err(SqlSurfaceError::unsupported(
                "COPY WITH clause must be followed by (FORMAT ...)",
            ));
        }
        return Ok(CopyFormat::Text);
    }
    p.advance();
    p.expect_contextual_keyword("FORMAT")?;
    let raw = p.expect_ident()?;
    let format = match raw.to_ascii_lowercase().as_str() {
        "text" => CopyFormat::Text,
        "csv" => CopyFormat::Csv,
        _ => {
            return Err(SqlSurfaceError::unsupported(format!(
                "unsupported COPY FORMAT: {raw}"
            )))
        }
    };
    p.expect_punct(')')?;
    Ok(format)
}

/// `COPY <table> (<col>[, <col>]*) FROM STDIN [WITH] [(FORMAT text|csv)]
/// USING OPERATION_ID '<id>' [;]` を構造判定する（Issue #939・WIRE-17）。
fn validate_copy_from_tokens(
    rest: &[Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<CopyStatement, SqlSurfaceError> {
    let mut p = Parser::new(rest);
    let table_name = p.expect_ident()?;

    p.expect_punct('(')?;
    let mut columns = vec![p.expect_ident()?];
    while matches!(p.peek(), Some(Token::Punct(','))) {
        p.advance();
        if columns.len() >= MAX_INSERT_COLUMNS {
            return Err(SqlSurfaceError::unsupported("too many COPY columns"));
        }
        columns.push(p.expect_ident()?);
    }
    p.expect_punct(')')?;

    p.expect_keyword(Keyword::From)?;
    p.expect_contextual_keyword("STDIN")?;

    let format = parse_optional_copy_format(&mut p)?;

    // 文末専用句の構造パースのみをここで行う（`parse_insert`／`parse_truncate`
    // と同じ設計。必須化の判定は `mode.require` へ委譲する）。
    let operation_id = p.parse_operation_id_clause()?;
    p.expect_end_of_statement()?;

    mode.require(operation_id.as_ref())?;

    let exists = lookup.table_exists(&table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(table_name));
    }

    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for c in &columns {
        if !seen.insert(c.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in COPY column list: {c}"
            )));
        }
    }
    if !columns.iter().any(|c| c == "id") {
        return Err(SqlSurfaceError::invalid_input(
            "COPY column list must include the id pseudo-column",
        ));
    }

    Ok(CopyStatement::From(ValidatedCopyFrom {
        table_name,
        columns,
        format,
        operation_id,
    }))
}

/// `COPY (<SELECT>) TO STDOUT [[WITH] (FORMAT text|csv)] [;]` を構造判定する
/// （Issue #939・WIRE-17）。`rest` の先頭は必ず `(` であること（呼び出し元
/// [`validate_copy_tokens`] が確認済み）。内側 `SELECT` は
/// [`validate_sql_tokens`] が返す [`Statement::Scan`]（広域取得。SQL-15）のみを
/// 受理する（第 2 の SELECT パーサーを持たない設計。`Select`（順位付き）・
/// `Aggregate`・`Explain`・`SetSearchMode`・`CreateFunction` はいずれも
/// `42601` へ落とす）。
fn validate_copy_to_tokens(
    rest: &[Token],
    lookup: &impl TableLookup,
) -> Result<CopyStatement, SqlSurfaceError> {
    let (inner, after) = split_parenthesized(rest)?;
    let scan = match validate_sql_tokens(inner, lookup)? {
        Statement::Scan(v) => v,
        _ => {
            return Err(SqlSurfaceError::unsupported(
                "COPY (...) TO STDOUT only supports a non-ranked SELECT (no ORDER BY / USING PLAN / aggregate)",
            ))
        }
    };

    let mut p = Parser::new(after);
    p.expect_contextual_keyword("TO")?;
    p.expect_contextual_keyword("STDOUT")?;
    let format = parse_optional_copy_format(&mut p)?;
    p.expect_end_of_statement()?;

    Ok(CopyStatement::To(ValidatedCopyTo {
        inner: Box::new(scan),
        format,
    }))
}

/// `COPY` 文（`FROM STDIN`／`TO STDOUT` の両形。Issue #939・WIRE-17・TASK-220）を
/// トークン化し、許可リスト形式で構造検証してから、`FROM STDIN` 形は `lookup` を
/// 通じて対象テーブルがカタログに実在するかまで確認する公開 API。`validate_sql`・
/// `validate_insert` と同じく独立したエントリポイントとする（`COPY` は
/// [`crate::sql::copy::is_copy_statement`] による先頭トークンの覗き見判定を
/// 経て `core.rs::execute_sql_in_session` から呼ばれる想定であり、`validate_sql`
/// の許可形状には含めない）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）。
/// `FROM STDIN` 形の `operation_id` 必須化ガード（`mode.require`。TASK-92・
/// RECOVER-1）はカタログ照会より前に評価する（[`validate_insert`] と同じ理由。
/// `TO STDOUT` 形は書き込みを伴わないため `operation_id` を持たない）。
pub fn validate_copy(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<CopyStatement, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_copy_tokens(&tokens, lookup, mode)
}

/// [`validate_copy`] の本体。トークン列を受け取ることで、呼び出し元
/// （`core.rs::execute_sql_in_session` が先頭トークン判定のために既に
/// トークナイズ済みの場合）が同一 SQL 文字列を再トークナイズせずに済む
/// （`validate_insert_tokens`・Issue #485 と同じ設計）。
pub(crate) fn validate_copy_tokens(
    tokens: &[Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<CopyStatement, SqlSurfaceError> {
    let (first, rest) = tokens
        .split_first()
        .ok_or_else(|| SqlSurfaceError::unsupported("empty COPY statement"))?;
    match first {
        Token::Ident(name) if name.eq_ignore_ascii_case("COPY") => {}
        other => {
            return Err(SqlSurfaceError::unsupported(format!(
                "expected COPY, got {other:?}"
            )))
        }
    }

    if matches!(rest.first(), Some(Token::Punct('('))) {
        validate_copy_to_tokens(rest, lookup)
    } else {
        validate_copy_from_tokens(rest, lookup, mode)
    }
}

/// `UPDATE` の `WHERE` 句が単一行・id 指定形（SQL-17、TASK-191）か述語形
/// （SQL-19、TASK-192）かを表す内部表現。[`Parser::parse_update_where`] が
/// 決定的に振り分ける。`validate_update`（既存の id 指定形専用エントリ
/// ポイント）は `Predicates` を `42601` で拒否し、`validate_update_form`
/// （SQL-19 の新エントリポイント）は両方を受理して [`ValidatedUpdateForm`]
/// の該当 variant へ写像する。
enum UpdateWhereForm {
    /// `WHERE id = <n>`（`USING OPERATION_ID`／文末／`;` が直後に続く狭い形）。
    /// 生数値文字列を保持し、`u64` への意味論的解釈は `sql::parser` の責務。
    Id(String),
    /// 述語形（`SELECT`・集計 `SELECT`・広域取得 `SELECT` と同一の
    /// [`WherePredicate`] 列。`AND` 結合順を保持）。
    Predicates(Vec<WherePredicate>),
}

/// 構文木（[`ValidatedUpdate`]／[`ValidatedPredicateUpdate`] の元）。カタログ
/// 存在確認前の中間結果（SQL-17・SQL-19、TASK-191・TASK-192）。
struct ParsedUpdateShape {
    table_name: String,
    assignments: Vec<(String, InsertLiteral)>,
    where_form: UpdateWhereForm,
    operation_id: Option<OperationId>,
    returning: Option<Projection>,
}

/// 許可形状の構造判定を通過した述語つき `UPDATE` 文（SQL-19、TASK-192）。
/// [`ValidatedUpdate`]（単一行・id 指定形）とは別 variant として扱う
/// （[`ValidatedUpdateForm`] 参照）。本モジュールが保証するのはここまでの
/// 構造情報のみで、列名・値・述語の意味論的妥当性は検証しない
/// （`sql::parser::bind_update_form` の責務）。
///
/// 受理する形は `UPDATE <table> SET <col> = <lit>[, <col> = <lit>]* WHERE
/// <述語>[ AND <述語>]* USING OPERATION_ID '<id>' [;]`（`<述語>` は `SELECT` の
/// `WHERE` と同一形状。[`WherePredicate`]）。`WHERE` 句自体の省略は本モジュールの
/// `expect_keyword(Keyword::Where)` が構造的に拒否する（`42601`）ため本型は
/// 構築されない。`WHERE` 句が存在しても中身が `visible()` 単独の恒等述語のみ
/// （非 `visible()` 述語が 1 つも無い）場合は許可リスト構造としては受理するが、
/// 実質的な全行更新になるため [`crate::sql::parser::bind_update_form`] が
/// 束縛時に `42601` で拒否する。
///
/// フィールドは `pub(crate)` のまま公開しない（[`crate::sql::parser::
/// BoundPredicateUpdate`] と同じ作法）。本型は `validate_update_form_tokens`
/// 内でのみ構築され、構築前に `mode.require(shape.operation_id.as_ref())`
/// （`operation_id` 必須化ガード。TASK-92・RECOVER-1）を必ず通す。フィールドを
/// `pub` にすると、クレート外の呼び出し元が `operation_id: None` を含む値を
/// この検証を経ずに直接組み立て、`bind_update_form`（同ゲートを再検証しない。
/// 検証済み入力である本型の契約を信頼する設計）へそのまま渡してガードを
/// 迂回できてしまう（codex-review 指摘・PR #985）。クレート外からはアクセサー
/// メソッド経由で読み取る。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ValidatedPredicateUpdate {
    /// UPDATE に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    /// SET 句の (列名, リテラル) 対応。宣言順を保持する（[`ValidatedUpdate::assignments`]
    /// と同じ契約）。
    pub(crate) assignments: Vec<(String, InsertLiteral)>,
    /// `WHERE` 句に含まれる述語（`AND` 結合順）。`SELECT`（[`ValidatedStatement::
    /// where_predicates`]）と同一の許可形状を再利用する。
    pub(crate) where_predicates: Vec<WherePredicate>,
    /// 文末専用句で搬送された、検証済みの `operation_id`。契約は
    /// [`ValidatedUpdate::operation_id`] と同一。
    pub(crate) operation_id: Option<OperationId>,
}

impl ValidatedPredicateUpdate {
    /// UPDATE に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// SET 句の (列名, リテラル) 対応（宣言順）。
    pub fn assignments(&self) -> &[(String, InsertLiteral)] {
        &self.assignments
    }

    /// `WHERE` 句に含まれる述語（`AND` 結合順）。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// 文末専用句で搬送された、検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }
}

/// [`validate_update_form`] の戻り値。`UPDATE` の `WHERE` 句が単一行・id 指定形
/// （SQL-17）か述語形（SQL-19）かで variant を分ける（破壊的変更を避けるため
/// 既存 [`validate_update`]／[`ValidatedUpdate`] はそのまま残し、本 enum は
/// 追加型として提供する）。
#[derive(Debug, Clone, PartialEq)]
pub enum ValidatedUpdateForm {
    /// 単一行・id 指定形（既存の [`ValidatedUpdate`] と同一の意味論）。
    Single(ValidatedUpdate),
    /// 述語形（SQL-19、TASK-192）。
    Predicate(ValidatedPredicateUpdate),
}

/// UPDATE 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を通じて
/// UPDATE 対象テーブルがカタログに実在するかを確認する（SQL-17、TASK-191 の公開 API）。
/// `validate_statement`（SELECT 専用、TASK-74）・`validate_insert`（SQL-10、TASK-80）
/// とは独立したエントリポイントとする（`operation_id` 必須化ガードを関数内部で
/// 自己完結して適用する必要があるため、INSERT と同じ理由で `Statement`／
/// `validate_sql` へ統合しない。他の文種別に `USING OPERATION_ID`／`SET` 句を付けた
/// 入力は各エントリポイントの `expect_end_of_statement` が余剰トークンとして `42601`
/// で拒否するため、文種別を誤って混同受理する経路は構造的に存在しない）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）。
/// `operation_id` 必須化ガード（`mode.require`。TASK-92・RECOVER-1）を含む段階構成の
/// 詳細は `recovery::required_op_id` モジュールドキュメント参照。実行結線・意味論的
/// 束縛（`sql::parser::bind_update`）・RLS 可視集合に基づく実行は別 Issue の担当。
pub fn validate_update(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedUpdate, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_update_tokens(&tokens, lookup, mode)
}

/// [`validate_update`] の本体。トークン列を受け取ることで、呼び出し元が既に
/// 先頭トークン判定のために `tokenize` 済みの場合、同一 SQL 文字列の再トークナイズを
/// 避けられる（Issue #485 が INSERT に施した「二重パース排除」と同型の設計を
/// UPDATE 側でも最初から可能にしておく）。
pub(crate) fn validate_update_tokens(
    tokens: &[lexer::Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedUpdate, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let shape = p.parse_update()?;
    p.expect_end_of_statement()?;

    // `RETURNING`（Issue #873・SQL-21）: `UPDATE` の実行結線（#865）は未着手
    // のため、構造検証段で常に `42601` 拒否する単一のチョークポイント（黙って
    // 保持し将来の実行器が無視する fail-open を防ぐ）。述語形 WHERE の判定
    // よりも前に置く——`parse_update_where` は `RETURNING` を終端として
    // 特別扱いしないため、`WHERE id = 1 RETURNING ...` は構造上
    // `UpdateWhereForm::Predicates` へ分類されうるが、`RETURNING` の拒否は
    // WHERE 形状に関わらず常に同じ「単一のチョークポイント」で行う
    // （`validate_update_form_tokens` と同じ判定順序に揃える）。
    if shape.returning.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "RETURNING is not supported for UPDATE",
        ));
    }

    // 述語形 WHERE（SQL-19、TASK-192）は本エントリポイントのスコープ外
    // （`validate_update` は id 指定形〔SQL-17〕専用のまま維持する。後方互換）。
    // `operation_id` 必須化ガード・カタログ存在確認より前に判定することで、
    // 既存テスト（`rejects_update_with_predicate_where_clause` 等）の
    // エラーコード（`42601`）・判定順序を変えない。
    let id_literal = match shape.where_form {
        UpdateWhereForm::Id(id_literal) => id_literal,
        UpdateWhereForm::Predicates(_) => {
            return Err(SqlSurfaceError::unsupported(
                "UPDATE WHERE clause must be the form: WHERE id = <n> (predicate-form WHERE is supported by the predicate-form entry point, SQL-19)",
            ));
        }
    };

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(ValidatedUpdate {
        table_name: shape.table_name,
        assignments: shape.assignments,
        id_literal,
        operation_id: shape.operation_id,
        // 直前のガードで `shape.returning.is_some()` は既に `42601` で
        // 拒否済みのため、ここへ到達する時点で常に `None`
        // （`validate_update_form_tokens` の同型ガードと表記を揃える）。
        returning: None,
    })
}

/// `UPDATE` 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を
/// 通じて UPDATE 対象テーブルがカタログに実在するかを確認する（SQL-19、
/// TASK-192 の公開 API）。単一行・id 指定形（SQL-17）・述語形（SQL-19）の
/// 両方を受理し、[`ValidatedUpdateForm`] の該当 variant へ振り分ける。
/// 既存の [`validate_update`]（id 指定形専用・[`ValidatedUpdate`] を返す）は
/// 挙動・シグネチャとも変更しない（破壊的変更を避けるための追加型 API。
/// `sql.rs` モジュールドキュメント参照）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）:
/// 構造検証（`WHERE` 省略は `42601`） → `operation_id` 必須化ガード
/// （`mode.require`。TASK-92・RECOVER-1） → UPDATE 対象テーブルのカタログ
/// 存在確認。意味論的束縛（`sql::parser::bind_update_form`）・RLS 可視集合に
/// 基づく実行・影響行数上限の適用は別 Issue の担当（#871）。
pub fn validate_update_form(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedUpdateForm, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    validate_update_form_tokens(&tokens, lookup, mode)
}

/// [`validate_update_form`] の本体。トークン列を受け取ることで、呼び出し元が
/// 既に先頭トークン判定のために `tokenize` 済みの場合、同一 SQL 文字列の
/// 再トークナイズを避けられる（`validate_update_tokens` と同じ設計）。
pub(crate) fn validate_update_form_tokens(
    tokens: &[lexer::Token],
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedUpdateForm, SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    let shape = p.parse_update()?;
    p.expect_end_of_statement()?;

    // `RETURNING`（Issue #873・SQL-21）: `validate_update_tokens` と同じ
    // チョークポイント。`UPDATE` は単一行・述語形いずれも実行結線（#865）が
    // 未着手のため、`WHERE` 形状の判定より前に一律拒否する。
    if shape.returning.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "RETURNING is not supported for UPDATE",
        ));
    }

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(match shape.where_form {
        UpdateWhereForm::Id(id_literal) => ValidatedUpdateForm::Single(ValidatedUpdate {
            table_name: shape.table_name,
            assignments: shape.assignments,
            id_literal,
            operation_id: shape.operation_id,
            returning: None,
        }),
        UpdateWhereForm::Predicates(where_predicates) => {
            ValidatedUpdateForm::Predicate(ValidatedPredicateUpdate {
                table_name: shape.table_name,
                assignments: shape.assignments,
                where_predicates,
                operation_id: shape.operation_id,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// storage 非依存の単体テスト用フェイク（`Storage` を必要としないための軽量抽象）。
    struct FakeCatalog {
        tables: HashSet<&'static str>,
    }

    impl TableLookup for FakeCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(self.tables.contains(name))
        }
    }

    struct FailingCatalog;
    impl TableLookup for FailingCatalog {
        fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
            Err(SqlSurfaceError::Internal {
                detail: "simulated backend failure".to_string(),
            })
        }
    }

    // codex-review P0・PR #210 指摘の再発防止: `Internal`（`wire_code() ==
    // "XX000"`）の `client_message()` は redb I/O エラー等の内部詳細
    // （`detail`）を一切含まない固定文言へ丸めること。`wire-server::simple_query`
    // は `to_string()` ではなく必ずこちらを使う契約（security.md P0）。
    #[test]
    fn internal_error_client_message_does_not_leak_detail() {
        let err = SqlSurfaceError::Internal {
            detail: "redb I/O error: disk quota exceeded at /var/lib/vector-db/data.redb"
                .to_string(),
        };
        assert_eq!(err.wire_code(), "XX000");
        assert_eq!(err.client_message(), "internal error");
        assert!(!err.client_message().contains("redb"));
        assert!(!err.client_message().contains("disk quota"));
    }

    // 対照確認: `Internal` 以外の variant は通常の `Display` 文言をそのまま
    // `client_message()` として返す（各コンストラクタで既に切り詰め・一般化
    // 済みのため、丸め不要）。
    #[test]
    fn non_internal_error_client_message_matches_display() {
        let err = SqlSurfaceError::UndefinedTable {
            name: "ghost_table".to_string(),
        };
        assert_eq!(err.client_message(), err.to_string());
        assert!(err.client_message().contains("ghost_table"));
    }

    fn catalog_with(tables: &[&'static str]) -> FakeCatalog {
        FakeCatalog {
            tables: tables.iter().copied().collect(),
        }
    }

    // --- 受理系（許可した SQL 表層の構造判定通過） -------------------------

    #[test]
    fn accepts_basic_select_with_order_by_distance() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2]' LIMIT 20",
            &lookup,
        )
        .expect("basic shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert!(stmt.where_predicates.is_empty());
        assert_eq!(stmt.limit, 20);
        assert_eq!(
            stmt.order_by,
            OrderByForm::Distance {
                column: "embedding".to_string(),
                literal: "[0.1,0.2]".to_string(),
            }
        );
    }

    #[test]
    fn accepts_select_with_where_equality() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("WHERE equality shape should be accepted");
        assert_eq!(
            stmt.where_predicates,
            vec![WherePredicate::Equality {
                column: "lang".to_string(),
                value: "ja".to_string(),
            }]
        );
    }

    #[test]
    fn accepts_select_with_where_predicate_call() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE visible() ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("WHERE predicate-call shape should be accepted");
        assert_eq!(
            stmt.where_predicates,
            vec![WherePredicate::PredicateCall {
                name: "visible".to_string()
            }]
        );
    }

    #[test]
    fn accepts_select_with_combined_where_predicates() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE lang = 'ja' AND visible() ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("combined WHERE predicates should be accepted");
        assert_eq!(
            stmt.where_predicates,
            vec![
                WherePredicate::Equality {
                    column: "lang".to_string(),
                    value: "ja".to_string(),
                },
                WherePredicate::PredicateCall {
                    name: "visible".to_string()
                },
            ]
        );
    }

    #[test]
    fn accepts_select_with_order_by_function_call() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("function-call shape should be accepted");
        assert_eq!(
            stmt.order_by,
            OrderByForm::FunctionCall {
                name: "hybrid_rrf".to_string(),
                args: vec![
                    FunctionArg::Ident("embedding".to_string()),
                    FunctionArg::StringLiteral("query text".to_string()),
                ],
            }
        );
    }

    #[test]
    fn accepts_select_with_order_by_function_call_alternate_allowed_name() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY HYBRID(embedding, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("alternate allowed function name should be accepted");
        assert_eq!(
            stmt.order_by,
            OrderByForm::FunctionCall {
                name: "HYBRID".to_string(),
                args: vec![
                    FunctionArg::Ident("embedding".to_string()),
                    FunctionArg::StringLiteral("query text".to_string()),
                ],
            }
        );
    }

    // TASK-75・SQL-4: 4 引数形（`<vec列>, '<vec リテラル>', <text列>, '<query text>'`）を
    // 構造として受理する（既存 2 引数形の受理は変更しない。実行可能性の判定は
    // `sql::parser::bind` の管轄）。
    #[test]
    fn accepts_select_with_order_by_function_call_four_args() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, '[0.1,0.2]', body, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("4-arg function-call shape should be accepted");
        assert_eq!(
            stmt.order_by,
            OrderByForm::FunctionCall {
                name: "hybrid_rrf".to_string(),
                args: vec![
                    FunctionArg::Ident("embedding".to_string()),
                    FunctionArg::StringLiteral("[0.1,0.2]".to_string()),
                    FunctionArg::Ident("body".to_string()),
                    FunctionArg::StringLiteral("query text".to_string()),
                ],
            }
        );
    }

    #[test]
    fn accepts_select_with_order_by_function_call_four_args_alternate_name() {
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT * FROM documents ORDER BY HYBRID(embedding, '[0.1,0.2]', body, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("4-arg HYBRID shape should be accepted");
    }

    #[test]
    fn rejects_order_by_function_call_with_unknown_name() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY attacker_controlled(embedding) LIMIT 5",
        );
    }

    #[test]
    fn rejects_where_predicate_call_with_unknown_name() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE unknown() ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    // --- TASK-208・SQL-24（Issue #912）: `WHERE` の `OR` 結合・括弧グルーピング --

    fn where_predicates_for(sql: &str) -> Vec<WherePredicate> {
        let lookup = catalog_with(&["documents"]);
        validate_statement(sql, &lookup)
            .unwrap_or_else(|e| panic!("expected acceptance, got {e:?} for sql={sql:?}"))
            .where_predicates()
            .to_vec()
    }

    #[test]
    fn accepts_simple_or() {
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE lang = 'ja' OR lang = 'en' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(
            preds,
            vec![WherePredicate::Or(vec![
                vec![WherePredicate::Equality {
                    column: "lang".to_string(),
                    value: "ja".to_string(),
                }],
                vec![WherePredicate::Equality {
                    column: "lang".to_string(),
                    value: "en".to_string(),
                }],
            ])]
        );
    }

    #[test]
    fn and_only_parenthesized_group_flattens_to_the_same_ast_as_without_parens() {
        // `(a AND b)` は AND だけのグループなので、括弧なしと完全に同じ AST になる
        // （TASK-208 導入前の content hash・既存受理形状との後方互換の核心）。
        let with_parens = where_predicates_for(
            "SELECT * FROM documents WHERE (lang = 'ja' AND flag) ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        let without_parens = where_predicates_for(
            "SELECT * FROM documents WHERE lang = 'ja' AND flag ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(with_parens, without_parens);
    }

    #[test]
    fn accepts_and_of_two_or_groups() {
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE (lang = 'ja' OR lang = 'en') AND (flag OR active = true) ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(preds.len(), 2, "two independent OR groups ANDed together");
        assert!(matches!(preds[0], WherePredicate::Or(_)));
        assert!(matches!(preds[1], WherePredicate::Or(_)));
    }

    #[test]
    fn accepts_nested_or_inside_and_branch() {
        // `a OR (b AND c)`: 分岐 2 の中に AND、`Or` 自体はネストしない。
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE lang = 'ja' OR (flag AND active = true) ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        match &preds[..] {
            [WherePredicate::Or(branches)] => {
                assert_eq!(branches.len(), 2);
                assert_eq!(branches[1].len(), 2);
            }
            other => panic!("expected a single Or predicate, got {other:?}"),
        }
    }

    #[test]
    fn accepts_value_expression_parenthesized_group_unaffected_by_or_support() {
        // `(id + 1) > 5`: 既存の式フォールバック経路（値式グループ）が壊れて
        // いないことを固定する（決定的先読みが値式グループと BOOLEAN グループを
        // 取り違えない）。
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE (id + 1) > 5 ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(preds.len(), 1);
        assert!(matches!(preds[0], WherePredicate::Expression(_)));
    }

    #[test]
    fn accepts_doubly_parenthesized_value_expression() {
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE ((id)) > 5 ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(preds.len(), 1);
        assert!(matches!(preds[0], WherePredicate::Expression(_)));
    }

    #[test]
    fn accepts_bare_boolean_column_wrapped_in_parens() {
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE (flag) ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(
            preds,
            vec![WherePredicate::BoolColumn {
                column: "flag".to_string(),
            }]
        );
    }

    #[test]
    fn accepts_column_named_or_as_equality() {
        // 列名 `or` はキーワード化しないため、通常の等価条件として通る。
        let preds = where_predicates_for(
            "SELECT * FROM documents WHERE or = 'x' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
        assert_eq!(
            preds,
            vec![WherePredicate::Equality {
                column: "or".to_string(),
                value: "x".to_string(),
            }]
        );
    }

    #[test]
    fn rejects_dangling_or() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE lang = 'ja' OR ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_unmatched_open_paren_in_where() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE (lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_empty_paren_group_in_where() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE () ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn accepts_where_leaf_count_at_the_limit() {
        let clause = (0..MAX_WHERE_LEAVES)
            .map(|i| format!("lang = 'v{i}'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT * FROM documents WHERE {clause} ORDER BY embedding <=> '[0.1]' LIMIT 5"
        );
        let lookup = catalog_with(&["documents"]);
        validate_statement(&sql, &lookup)
            .expect("exactly MAX_WHERE_LEAVES leaves must be accepted");
    }

    #[test]
    fn rejects_where_leaf_count_over_the_limit() {
        let clause = (0..=MAX_WHERE_LEAVES)
            .map(|i| format!("lang = 'v{i}'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT * FROM documents WHERE {clause} ORDER BY embedding <=> '[0.1]' LIMIT 5"
        );
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(&sql, &lookup).expect_err("must exceed MAX_WHERE_LEAVES");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn accepts_where_group_depth_at_the_limit() {
        let mut clause = "flag".to_string();
        for _ in 0..MAX_WHERE_GROUP_DEPTH {
            clause = format!("({clause})");
        }
        let sql = format!(
            "SELECT * FROM documents WHERE {clause} ORDER BY embedding <=> '[0.1]' LIMIT 5"
        );
        let lookup = catalog_with(&["documents"]);
        validate_statement(&sql, &lookup)
            .expect("exactly MAX_WHERE_GROUP_DEPTH nesting must be accepted");
    }

    #[test]
    fn rejects_where_group_depth_over_the_limit() {
        let mut clause = "flag".to_string();
        for _ in 0..=MAX_WHERE_GROUP_DEPTH {
            clause = format!("({clause})");
        }
        let sql = format!(
            "SELECT * FROM documents WHERE {clause} ORDER BY embedding <=> '[0.1]' LIMIT 5"
        );
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(&sql, &lookup).expect_err("must exceed MAX_WHERE_GROUP_DEPTH");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_visible_inside_or_branch() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE visible() OR lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn accepts_visible_inside_and_only_group() {
        // `visible()` は「2 分岐以上の OR」の中にだけ現れなければ許可される
        // （AND だけのグループはそもそも `Or` へ包まれない）。
        where_predicates_for(
            "SELECT * FROM documents WHERE (visible() AND lang = 'ja') ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_visible_inside_and_branch_of_an_or() {
        // `(visible() AND x) OR y`: OR 分岐の 1 つが visible() を含むため拒否する。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE (visible() AND flag) OR lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_or_inside_check_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "CREATE TABLE t (kind TEXT, CHECK (kind = 'a' OR kind = 'b'))",
            &lookup,
        )
        .expect_err("OR must remain rejected inside CHECK bodies");
        assert_eq!(err.wire_code(), "42601");
    }

    // 許可された ORDER BY 関数の引数形状回帰テスト。
    #[test]
    fn rejects_order_by_function_call_with_empty_args() {
        assert_rejected_as_syntax_error("SELECT * FROM documents ORDER BY HYBRID() LIMIT 5");
    }

    #[test]
    fn rejects_order_by_function_call_with_single_arg() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding) LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_too_many_args() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'q', 'extra') LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_missing_comma_between_args() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding 'q') LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_trailing_comma() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'q',) LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_wrong_second_arg_type() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 123) LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_nested_paren_group_as_arg() {
        // 括弧グループは意味を持たないため、第 1 引数の位置に来ても拒否する。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf((embedding), 'q') LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_nested_call_as_arg() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, foo('q')) LIMIT 5",
        );
    }

    #[test]
    fn accepts_explicit_column_list() {
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("explicit column list should be accepted");
    }

    #[test]
    fn accepts_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5;",
            &lookup,
        )
        .expect("single trailing semicolon should be accepted");
    }

    // --- 拒否系（SQL-8 列挙） ------------------------------------------------

    fn assert_rejected_as_syntax_error(sql: &str) {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(sql, &lookup).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql={sql:?} err={err:?}");
    }

    #[test]
    fn rejects_cte() {
        assert_rejected_as_syntax_error(
            "WITH x AS (SELECT * FROM documents) SELECT * FROM x ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_distinct() {
        assert_rejected_as_syntax_error(
            "SELECT DISTINCT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_group_by() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents GROUP BY lang ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_having() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents HAVING lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_join() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents JOIN other ON documents.id = other.id ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_multiple_from_tables() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents, other ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_offset() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 OFFSET 10",
        );
    }

    #[test]
    fn rejects_multiple_order_by_expressions() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]', lang LIMIT 5",
        );
    }

    #[test]
    fn rejects_unsupported_where_condition() {
        // 単一等価・単一 RLS 呼び出し以外（比較演算子・OR 等）は許可リスト外。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE lang != 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_long_left_associative_arithmetic_chain_by_node_budget() {
        // `parse_add_expr`/`parse_mul_expr` の左結合ループは `depth` を増やさずに
        // `lhs` へ木を積み続けるため、"1+1+...+1" のような同一深さの連鎖入力は
        // `MAX_EXPR_DEPTH`（構文解析の再帰段数チェック）をすり抜けうる。ノード数
        // 予算（`Parser::expr_node_budget`、[`MAX_EXPR_NODES`] 共有）がこの形の
        // 入力も頭打ちにすることを確認する（`54000`。ノード予算エラー後の
        // `Box<Expr>` 再帰的 drop によるスタック消費を定数に抑える対応）。
        let chain: String = "1+".repeat(600) + "1";
        let sql = format!(
            "SELECT * FROM documents WHERE {chain} > 0 ORDER BY embedding <=> '[0.1]' LIMIT 5"
        );
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(&sql, &lookup).unwrap_err();
        assert_eq!(err.wire_code(), "54000", "err={err:?}");
    }

    #[test]
    fn rejects_multiple_statements() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5; SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_non_select_statement() {
        assert_rejected_as_syntax_error("INSERT INTO documents (embedding) VALUES ('[0.1]')");
    }

    #[test]
    fn rejects_dollar_parameter_placeholder_in_order_by() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> $1 LIMIT 5",
        );
    }

    #[test]
    fn rejects_lowercase_distinct_case_insensitively() {
        assert_rejected_as_syntax_error(
            "SELECT distinct * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_semicolon_inside_argument_list_injection_attempt() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding; DROP) LIMIT 5",
        );
    }

    // --- HINT ORDER（TASK-76・SQL-7） ----------------------------------------

    #[test]
    fn accepts_all_six_hint_order_permutations() {
        let lookup = catalog_with(&["documents"]);
        for perm in [
            "RLS, SCALAR, DISTANCE",
            "RLS, DISTANCE, SCALAR",
            "SCALAR, RLS, DISTANCE",
            "SCALAR, DISTANCE, RLS",
            "DISTANCE, RLS, SCALAR",
            "DISTANCE, SCALAR, RLS",
        ] {
            let sql = format!(
                "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER({perm})"
            );
            validate_statement(&sql, &lookup)
                .unwrap_or_else(|e| panic!("perm={perm:?} must be accepted, got {e:?}"));
        }
    }

    #[test]
    fn accepts_lowercase_hint_order_stage_names_and_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(rls, scalar, distance);",
            &lookup,
        )
        .expect("lowercase stage names should be accepted");
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn accepts_hint_as_an_ordinary_column_name() {
        // `HINT` は LIMIT 直後の所定位置でのみ文脈依存で認識するため、通常の列名
        // としての `hint` は引き続き受理する（後方互換性、AGENTS.md P1）。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT hint FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("hint should be usable as an ordinary column name");
        assert_eq!(
            stmt.projection,
            Projection::Columns(vec!["hint".to_string()])
        );
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn accepts_hint_as_a_where_equality_column_name() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE hint = 'x' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("hint should be usable as an ordinary WHERE column name");
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn no_hint_order_defaults_to_rls_scalar_distance() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("must be accepted");
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn hint_order_populates_evaluation_order_field() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(DISTANCE, SCALAR, RLS)",
            &lookup,
        )
        .expect("must be accepted");
        assert_eq!(
            stmt.evaluation_order.stages(),
            [Stage::Distance, Stage::Scalar, Stage::Rls]
        );
    }

    #[test]
    fn rejects_hint_order_with_two_stages() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR)",
        );
    }

    #[test]
    fn rejects_hint_order_with_four_stages() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, DISTANCE, RLS)",
        );
    }

    #[test]
    fn rejects_hint_order_with_duplicate_stage() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, RLS, SCALAR)",
        );
    }

    #[test]
    fn rejects_hint_order_with_unknown_stage_name() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, ATTACKER)",
        );
    }

    #[test]
    fn rejects_hint_order_with_empty_parens() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER()",
        );
    }

    #[test]
    fn rejects_hint_alone_without_order() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT",
        );
    }

    #[test]
    fn rejects_hint_order_without_parens() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER RLS, SCALAR, DISTANCE",
        );
    }

    #[test]
    fn rejects_hint_order_before_limit() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' HINT ORDER(RLS, SCALAR, DISTANCE) LIMIT 5",
        );
    }

    #[test]
    fn rejects_hint_order_specified_twice() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, DISTANCE) HINT ORDER(RLS, SCALAR, DISTANCE)",
        );
    }

    #[test]
    fn rejects_trailing_tokens_after_hint_order() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, DISTANCE) extra",
        );
    }

    #[test]
    fn rejects_dollar_parameter_in_hint_order() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER($1, SCALAR, DISTANCE)",
        );
    }

    // `hint` を列名として使う既存 SQL の後方互換性は
    // `accepts_hint_as_an_ordinary_column_name` / `accepts_hint_as_a_where_equality_column_name`
    // で検証する（codex-review P1 指摘・AGENTS.md「公開 API・エラー契約の互換性」
    // 対応。`HINT` は LIMIT 直後の所定位置でのみ文脈依存で認識し、予約語化はしない）。

    // --- 未知テーブル（42P01） -----------------------------------------------

    #[test]
    fn rejects_undefined_table() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM nope ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn structurally_invalid_and_undefined_table_is_classified_as_syntax_error() {
        // 構造違反とテーブル不存在の両方に該当する入力は、検証順序（構造判定が先）に
        // より常に 42601 として決定的に分類される（GROUP BY は許可リスト外）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM nope GROUP BY x ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    // --- カタログ照会失敗（XX000）・fail-closed ------------------------------

    #[test]
    fn catalog_backend_failure_is_not_treated_as_acceptance() {
        let err = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &FailingCatalog,
        )
        .expect_err("backend failure must not fall open into acceptance");
        assert_eq!(err.wire_code(), "XX000");
    }

    // --- 決定性 ---------------------------------------------------------------

    #[test]
    fn same_input_yields_same_classification_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "SELECT * FROM documents GROUP BY x ORDER BY embedding <=> '[0.1]' LIMIT 5";
        let first = validate_statement(sql, &lookup).unwrap_err().wire_code();
        let second = validate_statement(sql, &lookup).unwrap_err().wire_code();
        assert_eq!(first, second);
    }

    // --- 頑健性（panic せず Err を返す） ---------------------------------------

    #[test]
    fn does_not_panic_on_empty_input() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement("", &lookup).is_err());
    }

    #[test]
    fn does_not_panic_on_only_whitespace() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement("   \n\t  ", &lookup).is_err());
    }

    #[test]
    fn does_not_panic_on_unterminated_quotes_and_nested_quotes() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents WHERE lang = 'ja ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup
        )
        .is_err());
        assert!(validate_statement(
            "SELECT * FROM documents WHERE lang = '''' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup
        )
        .is_ok());
    }

    #[test]
    fn does_not_panic_on_huge_input() {
        let lookup = catalog_with(&["documents"]);
        let huge = format!(
            "SELECT * FROM documents WHERE lang = '{}' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            "x".repeat(2_000_000)
        );
        assert!(validate_statement(&huge, &lookup).is_err());
    }

    // --- validate_insert（SQL-10、TASK-80） -----------------------------------

    #[test]
    fn accepts_insert_with_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("basic INSERT shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert_eq!(
            stmt.columns,
            vec!["id".to_string(), "embedding".to_string()]
        );
        assert_eq!(
            stmt.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    // --- 単項マイナス（Issue #881・TABLE-13・TASK-196） ---

    /// `-` の直後に数値トークンが続く形は単項マイナスとして受理し、
    /// `InsertLiteral::Number("-<digits>")` へ正規化する（`INTEGER`／`BIGINT`
    /// 列の負数リテラルを許可リストの構造段で通すための変更。値域検証・
    /// パースは束縛段（`sql::parser::bind_integer_literal`）が行う）。
    #[test]
    fn accepts_negative_number_literal_in_values() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding, n) VALUES (1, '[0.1,0.2]', -5) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("negative number literal should be accepted");
        assert_eq!(
            stmt.rows,
            vec![vec![
                InsertLiteral::Number("1".to_string()),
                InsertLiteral::String("[0.1,0.2]".to_string()),
                InsertLiteral::Number("-5".to_string()),
            ]]
        );
    }

    /// 空白を挟んだ単項マイナス（`- 5`）も同じ形として受理する。
    #[test]
    fn accepts_negative_number_literal_with_whitespace_between_minus_and_digits() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding, n) VALUES (1, '[0.1,0.2]', - 5) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("negative number literal with whitespace should be accepted");
        assert_eq!(stmt.rows[0][2], InsertLiteral::Number("-5".to_string()));
    }

    /// `- -1`（二重マイナス）は構造的に受理しない（`42601`）。単項マイナスの
    /// 直後は数値トークンのみを許すため、2 個目の `-` はそこで構文エラーになる。
    #[test]
    fn rejects_double_minus_number_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding, n) VALUES (1, '[0.1,0.2]', - -1) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    /// `+1`（単項プラス）は `NUMERIC` 列の符号付きリテラル（Issue #885・D6・
    /// PR #1020 codex-review 指摘対応）を許可リストの構造段で通すために現在は
    /// 受理し、`InsertLiteral::Number("+1")` へ正規化する（本テストでの
    /// `catalog_with` は列型を持たないため、非 NUMERIC 列に対する拒否
    /// （束縛段の型不一致・不正値 `22000`）はここでは検証しない。詳細は
    /// `expect_literal` のドキュメンテーションコメント参照）。
    #[test]
    fn accepts_unary_plus_number_literal_structurally() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding, n) VALUES (1, '[0.1,0.2]', +1) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("unary plus number literal should be accepted structurally");
        assert_eq!(stmt.rows[0][2], InsertLiteral::Number("+1".to_string()));
    }

    /// `-'x'`（マイナスの直後に文字列リテラル）は構造的に受理しない（`42601`）。
    #[test]
    fn rejects_minus_followed_by_string_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding, n) VALUES (1, '[0.1,0.2]', -'x') USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    // --- RETURNING（Issue #873・SQL-21） ---

    #[test]
    fn accepts_insert_with_returning_star_before_using_clause() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             RETURNING * USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("RETURNING * before USING should be accepted");
        assert_eq!(stmt.returning, Some(Projection::All));
    }

    #[test]
    fn accepts_insert_with_returning_column_list() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             RETURNING id, embedding USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("RETURNING column list should be accepted");
        assert_eq!(
            stmt.returning,
            Some(Projection::Columns(vec![
                "id".to_string(),
                "embedding".to_string()
            ]))
        );
    }

    #[test]
    fn insert_without_returning_clause_has_none() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("plain INSERT should be accepted");
        assert_eq!(stmt.returning, None);
    }

    #[test]
    fn rejects_insert_with_returning_after_using_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             USING OPERATION_ID 'op-0001' RETURNING id",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("RETURNING after USING must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_with_missing_returning_projection() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             RETURNING USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("RETURNING without a projection must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_with_duplicate_returning_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             RETURNING id RETURNING embedding USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("duplicate RETURNING clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_with_function_call_returning_item() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             RETURNING vec_norm(embedding) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("function-call RETURNING items must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_truncate_with_returning_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate(
            "TRUNCATE TABLE documents USING OPERATION_ID 'op-0001' RETURNING *",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("TRUNCATE does not support RETURNING");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_delete_single_row_with_returning_before_using_clause() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete(
            "DELETE FROM documents WHERE id = 1 RETURNING * USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("single-row DELETE RETURNING should be accepted");
        assert_eq!(stmt.returning, Some(Projection::All));
    }

    #[test]
    fn validate_delete_statement_single_row_with_returning_yields_single_row_variant() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE id = 1 RETURNING id USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("single-row DELETE RETURNING via validate_delete_statement should be accepted");
        match stmt {
            DeleteStatement::SingleRow(inner) => {
                assert_eq!(
                    inner.returning,
                    Some(Projection::Columns(vec!["id".to_string()]))
                );
            }
            other => panic!("expected DeleteStatement::SingleRow, got {other:?}"),
        }
    }

    #[test]
    fn rejects_predicate_delete_with_returning_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("predicate-form DELETE RETURNING must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_single_row_with_returning_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 RETURNING id USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("single-row UPDATE RETURNING must be rejected (execution not wired, #865)");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_predicate_form_with_returning_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET lang = 'en' WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("predicate-form UPDATE RETURNING must be rejected (execution not wired, #865)");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_insert_with_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'op-0001';",
            &lookup,
            LedgerMode::Ledgered,
        )
        .is_ok());
    }

    // --- ON CONFLICT（SQL-20・TASK-193、Issue #872） ---

    #[test]
    fn accepts_upsert_do_nothing() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-1'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("DO NOTHING should be accepted");
        assert_eq!(stmt.on_conflict, Some(OnConflictAction::DoNothing));
    }

    #[test]
    fn accepts_upsert_do_update_set_excluded_and_literal_mixed_case_qualifier() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
             ON CONFLICT (id) DO UPDATE SET embedding = excluded.embedding, lang = 'en' \
             USING OPERATION_ID 'op-upsert-2'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("DO UPDATE SET should be accepted");
        assert_eq!(
            stmt.on_conflict,
            Some(OnConflictAction::DoUpdate(vec![
                (
                    "embedding".to_string(),
                    UpsertValue::Excluded("embedding".to_string())
                ),
                (
                    "lang".to_string(),
                    UpsertValue::Literal(InsertLiteral::String("en".to_string()))
                ),
            ]))
        );
    }

    #[test]
    fn accepts_upsert_with_multi_row_values() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]'), (2, '[0.3,0.4]') \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-3'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("multi-row VALUES with ON CONFLICT should be accepted");
        assert_eq!(stmt.rows.len(), 2);
        assert_eq!(stmt.on_conflict, Some(OnConflictAction::DoNothing));
    }

    #[test]
    fn rejects_upsert_missing_target_column_list() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT DO NOTHING \
             USING OPERATION_ID 'op-upsert-4'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_non_id_target_column() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, lang) VALUES (1, 'ja') ON CONFLICT (lang) DO NOTHING \
             USING OPERATION_ID 'op-upsert-5'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_target_column_with_mismatched_case() {
        // `id` は列識別子であり、`ON`/`CONFLICT`/`DO` のような文脈的キーワード
        // ではない（大文字小文字保存・区別。`parse_on_conflict_clause` の
        // ドキュメンテーションコメント参照。cursor(Bugbot) 指摘
        // https://github.com/Fandhe-AI/vector-db/pull/990#discussion_r4075151521）。
        let lookup = catalog_with(&["documents"]);
        for target in ["ID", "Id"] {
            let sql = format!(
                "INSERT INTO documents (id) VALUES (1) ON CONFLICT ({target}) DO NOTHING \
                 USING OPERATION_ID 'op-upsert-case'"
            );
            let err = validate_insert(&sql, &lookup, LedgerMode::Ledgered).unwrap_err();
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn rejects_upsert_multiple_target_columns() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, lang) VALUES (1, 'ja') ON CONFLICT (id, lang) DO NOTHING \
             USING OPERATION_ID 'op-upsert-6'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_on_constraint_form() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT ON CONSTRAINT documents_pkey \
             DO NOTHING USING OPERATION_ID 'op-upsert-7'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_do_update_without_set() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT (id) DO UPDATE \
             USING OPERATION_ID 'op-upsert-8'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_do_update_set_with_empty_assignment_list() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET \
             USING OPERATION_ID 'op-upsert-9'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_do_update_set_where_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, lang) VALUES (1, 'ja') ON CONFLICT (id) \
             DO UPDATE SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-upsert-10'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_duplicate_on_conflict_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT (id) DO NOTHING \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-11'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_on_conflict_after_using_operation_id() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'op-upsert-12' \
             ON CONFLICT (id) DO NOTHING",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_excluded_qualifier_used_bare() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET id = EXCLUDED \
             USING OPERATION_ID 'op-upsert-13'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_upsert_missing_operation_id_before_catalog_lookup() {
        let lookup = FailingCatalog;
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) ON CONFLICT (id) DO NOTHING",
            &lookup,
            LedgerMode::Ledgered,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_missing_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1)",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing clause must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_with_explicit_null_operation_id() {
        // 明示 `NULL` は句の欠落と同様に扱う（TASK-92・RECOVER-1）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("explicit NULL must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_with_explicit_null_operation_id_case_insensitive() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID null",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("lowercase null must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn explicit_null_operation_id_does_not_reach_catalog_lookup() {
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1) USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_insert_with_empty_operation_id_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID ''",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("empty value must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_operation_id_dollar_placeholder() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID $1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("$n placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_operation_id_non_string_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 123",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("non-string value must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_with_duplicate_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'a' USING OPERATION_ID 'b'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("duplicate clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn compare_only_without_ledger_accepts_missing_operation_id_clause() {
        // サーバー構成のみが必須化の可否を決める（TASK-92・RECOVER-1）:
        // `CompareOnlyWithoutLedger` では句の省略を許す。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id) VALUES (1)",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    #[test]
    fn compare_only_without_ledger_accepts_explicit_null() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    #[test]
    fn compare_only_without_ledger_still_validates_control_characters() {
        // 値検証（制御文字混入は `22000`）はサーバー構成に依存しない
        // （`LedgerMode` は必須化の可否のみを制御し、値の意味論的妥当性検証
        // 〔`OperationId::parse`〕を迂回させない）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'op-\u{0007}'",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect_err("control character must still be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    // --- validate_delete（SQL-18、TASK-191） -----------------------------------

    #[test]
    fn accepts_delete_with_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("basic DELETE shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert_eq!(stmt.id_literal, "1");
        assert_eq!(
            stmt.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    #[test]
    fn accepts_delete_with_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-0001';",
            &lookup,
            LedgerMode::Ledgered,
        )
        .is_ok());
    }

    #[test]
    fn rejects_delete_missing_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing clause must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_delete_with_explicit_null_operation_id() {
        // 明示 `NULL` は句の欠落と同様に扱う（TASK-92・RECOVER-1）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("explicit NULL must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_delete_with_explicit_null_operation_id_case_insensitive() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID null",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("lowercase null must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn explicit_null_operation_id_does_not_reach_catalog_lookup_for_delete() {
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_delete(
            "DELETE FROM nope WHERE id = 1 USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_delete_with_empty_operation_id_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID ''",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("empty value must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_delete_operation_id_dollar_placeholder() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID $1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("$n placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_operation_id_non_string_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 123",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("non-string value must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_with_duplicate_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'a' USING OPERATION_ID 'b'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("duplicate clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn compare_only_without_ledger_accepts_missing_operation_id_clause_for_delete() {
        // サーバー構成のみが必須化の可否を決める（TASK-92・RECOVER-1）:
        // `CompareOnlyWithoutLedger` では句の省略を許す。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete(
            "DELETE FROM documents WHERE id = 1",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    #[test]
    fn rejects_delete_undefined_table() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM ghost WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn rejects_delete_without_where_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("DELETE without WHERE must be rejected (full-table delete is out of scope)");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_with_and_predicate() {
        // 述語つき DELETE（`id` 以外の列・`AND` 結合）は別 Issue の管轄。
        // ここで受理範囲を広げないことを固定する。
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = 1 AND lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("AND-combined predicate must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_with_non_id_column() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("non-id predicate column must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_with_string_id_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = '1' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("string id literal must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_with_negative_id_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete(
            "DELETE FROM documents WHERE id = -1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("negative id literal must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    // --- validate_delete_statement（述語つき DELETE、Issue #870・TASK-192・SQL-19） ---

    #[test]
    fn accepts_delete_statement_with_equality_predicate() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("equality predicate DELETE should be accepted");
        match stmt {
            DeleteStatement::Predicate(pd) => {
                assert_eq!(pd.table_name(), "documents");
                assert_eq!(
                    pd.where_predicates(),
                    &[WherePredicate::Equality {
                        column: "lang".to_string(),
                        value: "ja".to_string(),
                    }]
                );
            }
            DeleteStatement::SingleRow(_) => panic!("must classify as predicate form"),
        }
    }

    #[test]
    fn accepts_delete_statement_with_prefix_predicate() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE path LIKE 'src/%' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("prefix predicate DELETE should be accepted");
        assert!(matches!(stmt, DeleteStatement::Predicate(_)));
    }

    #[test]
    fn accepts_delete_statement_with_expression_predicate() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE id > 5 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("expression predicate DELETE should be accepted");
        assert!(matches!(stmt, DeleteStatement::Predicate(_)));
    }

    #[test]
    fn accepts_delete_statement_with_and_combined_predicates_preserving_declared_order() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE id = 1 AND lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("AND-combined predicate DELETE should be accepted");
        match stmt {
            DeleteStatement::Predicate(pd) => {
                // `id = 1` は先頭で `AND` 結合されるため単一行形には分類され
                // ない（§3.2）。宣言順（id → lang）を保持する。
                assert_eq!(
                    pd.where_predicates(),
                    &[
                        WherePredicate::Expression(Expr::Binary {
                            op: BinOp::Eq,
                            lhs: Box::new(Expr::Ident("id".to_string())),
                            rhs: Box::new(Expr::Number("1".to_string())),
                        }),
                        WherePredicate::Equality {
                            column: "lang".to_string(),
                            value: "ja".to_string(),
                        },
                    ]
                );
            }
            DeleteStatement::SingleRow(_) => panic!("must classify as predicate form"),
        }
    }

    #[test]
    fn accepts_delete_statement_with_visible_predicate() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE visible() USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("visible() predicate DELETE should be accepted (scan/aggregate と同じく受理のみ)");
        assert!(matches!(stmt, DeleteStatement::Predicate(_)));
    }

    #[test]
    fn accepts_delete_statement_single_row_form_with_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-0001';",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("single-row form must still be accepted via validate_delete_statement");
        match stmt {
            DeleteStatement::SingleRow(sr) => {
                assert_eq!(sr.table_name, "documents");
                assert_eq!(sr.id_literal, "1");
            }
            DeleteStatement::Predicate(_) => panic!("must classify as single-row form"),
        }
    }

    /// R1: 同一述語テキストについて、`validate_delete_statement` の
    /// `where_predicates()` と `SELECT ... LIMIT` の広域取得（scan、Issue #454）
    /// 側 `where_predicates()` が構造的に一致することを機械検証する
    /// （第 2 の述語実装を作らない契約の固定）。
    #[test]
    fn where_predicates_match_scan_for_same_predicate_text() {
        let lookup = catalog_with(&["documents"]);
        let predicate_text = "lang = 'ja' AND id > 5";

        let delete_stmt = validate_delete_statement(
            &format!("DELETE FROM documents WHERE {predicate_text} USING OPERATION_ID 'op-0001'"),
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("predicate DELETE should be accepted");
        let delete_predicates = match delete_stmt {
            DeleteStatement::Predicate(pd) => pd.where_predicates,
            DeleteStatement::SingleRow(_) => panic!("must classify as predicate form"),
        };

        let scan_stmt = validate_sql(
            &format!("SELECT id FROM documents WHERE {predicate_text} LIMIT 1"),
            &lookup,
        )
        .expect("SELECT ... LIMIT should be accepted as a scan statement");
        let scan_predicates = match scan_stmt {
            Statement::Scan(scan) => scan.where_predicates,
            other => panic!("must classify as Statement::Scan, got {other:?}"),
        };

        assert_eq!(delete_predicates, scan_predicates);
    }

    #[test]
    fn rejects_delete_statement_without_where_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("DELETE without WHERE must be rejected (全行削除は TRUNCATE の管轄)");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_statement_with_hint_order_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' HINT ORDER(RLS, SCALAR, VECTOR) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("HINT ORDER suffix is out of the allowed shape");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_statement_with_order_by_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' ORDER BY id USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("ORDER BY suffix is out of the allowed shape");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_statement_with_limit_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' LIMIT 5 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("LIMIT suffix is out of the allowed shape");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_statement_with_using_mode_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' USING MODE 'precision' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("USING MODE suffix is out of the allowed shape");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_delete_statement_with_returning_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("RETURNING suffix is out of the allowed shape");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_delete_statement_with_or_combined_predicate() {
        // TASK-208・SQL-24（Issue #912）: `OR` 結合は述語つき DELETE の許可形状に
        // 含まれるようになった（従来は `42601` で拒否していた）。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' OR lang = 'en' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("OR-combined predicate is now accepted");
        match stmt {
            DeleteStatement::Predicate(predicate) => {
                assert_eq!(
                    predicate.where_predicates,
                    vec![WherePredicate::Or(vec![
                        vec![WherePredicate::Equality {
                            column: "lang".to_string(),
                            value: "ja".to_string(),
                        }],
                        vec![WherePredicate::Equality {
                            column: "lang".to_string(),
                            value: "en".to_string(),
                        }],
                    ])]
                );
            }
            other => panic!("expected DeleteStatement::Predicate, got {other:?}"),
        }
    }

    #[test]
    fn rejects_delete_statement_duplicate_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja' USING OPERATION_ID 'a' USING OPERATION_ID 'b'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("duplicate operation_id clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn explain_delete_statement_is_rejected() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "EXPLAIN DELETE FROM documents WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("EXPLAIN prefix on DELETE is out of the allowed shape");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_delete_rejects_predicate_form_regardless_of_operation_id_and_table_existence() {
        // 述語形は `validate_delete`（単一行形専用入口）へ渡すと、`operation_id`
        // の有無・テーブルの実在によらず常に構造段で `42601` になる
        // （`validate_delete` のエラー優先順位契約を Issue #870 追加後も保存する）。
        let lookup = catalog_with(&["documents"]);

        let err = validate_delete(
            "DELETE FROM documents WHERE lang = 'ja'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("predicate form without operation_id must still be 42601, not 23502");
        assert_eq!(err.wire_code(), "42601");

        let err = validate_delete(
            "DELETE FROM ghost WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("predicate form against an undefined table must still be 42601, not 42P01");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn predicate_delete_structural_validation_never_queries_catalog_when_operation_id_is_missing() {
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_delete_statement(
            "DELETE FROM nope WHERE lang = 'ja'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_delete_statement_undefined_table_for_predicate_form() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_delete_statement(
            "DELETE FROM ghost WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn compare_only_without_ledger_accepts_missing_operation_id_clause_for_predicate_delete() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_delete_statement(
            "DELETE FROM documents WHERE lang = 'ja'",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        match stmt {
            DeleteStatement::Predicate(pd) => assert_eq!(pd.operation_id(), None),
            DeleteStatement::SingleRow(_) => panic!("must classify as predicate form"),
        }
    }

    #[test]
    fn validate_delete_statement_is_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "DELETE FROM documents WHERE lang = 'ja' AND id > 5 USING OPERATION_ID 'op-0001'";
        let first = validate_delete_statement(sql, &lookup, LedgerMode::Ledgered)
            .expect("first call should succeed");
        let second = validate_delete_statement(sql, &lookup, LedgerMode::Ledgered)
            .expect("second call should succeed");
        assert_eq!(first, second);
    }

    #[test]
    fn validate_sql_still_rejects_delete_statement() {
        // #867 が dispatch を結線するまで DELETE は `execute_sql_in_session`
        // 相当の SELECT/SET 専用エントリポイント（`validate_sql`）経由では
        // 実行できないことを明示的に固定する（現状維持の確認）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
        )
        .expect_err("validate_sql must not accept DELETE statements yet");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_select_with_using_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING OPERATION_ID 'op-0001'",
            &lookup,
        )
        .expect_err("USING OPERATION_ID on a SELECT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_column_value_count_mismatch() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("column/value count mismatch must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_into_undefined_table() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn missing_operation_id_is_classified_before_undefined_table() {
        // 構造違反（句省略）とテーブル不存在の両方に該当する入力は、検証順序
        // （構造判定が先）により常に 23502 として決定的に分類される
        // （SQL-10 の要件: 省略は書き込みトランザクション開始前に拒否）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1)",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn insert_structural_validation_never_queries_catalog_when_operation_id_is_missing() {
        // 23502 が「行数不変」のような弱い代理指標ではなく、カタログ照会（＝write
        // txn 開始の前段）そのものに到達していないことを直接確認する
        // （advisor 指摘対応: catalog lookup が一度も呼ばれていないことをフラグで検証）。
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1)",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_insert_exceeding_max_columns() {
        let lookup = catalog_with(&["documents"]);
        let cols: Vec<String> = (0..MAX_INSERT_COLUMNS + 1)
            .map(|i| format!("c{i}"))
            .collect();
        let vals: Vec<String> = (0..MAX_INSERT_COLUMNS + 1).map(|i| i.to_string()).collect();
        let sql = format!(
            "INSERT INTO documents ({}) VALUES ({}) USING OPERATION_ID 'op-0001'",
            cols.join(", "),
            vals.join(", ")
        );
        let err =
            validate_insert(&sql, &lookup, LedgerMode::Ledgered).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_insert_at_max_row_count_boundary() {
        // SQL-16・TASK-190: 実装既定値ちょうど（[`MAX_INSERT_ROWS_PER_STATEMENT`]）の
        // 行数は受理される（境界値検証。`tests/insert_multi_row.rs` の結合テストは
        // 定数値へ依存せず「明らかに超過する規模」を使う方針のため、境界値ちょうどの
        // 検証は本ユニットテストが担う）。
        let lookup = catalog_with(&["documents"]);
        let rows: Vec<String> = (0..MAX_INSERT_ROWS_PER_STATEMENT)
            .map(|i| format!("({i})"))
            .collect();
        let sql = format!(
            "INSERT INTO documents (id) VALUES {} USING OPERATION_ID 'op-0001'",
            rows.join(", ")
        );
        let stmt = validate_insert(&sql, &lookup, LedgerMode::Ledgered)
            .expect("row count at the limit must be accepted");
        assert_eq!(stmt.rows.len(), MAX_INSERT_ROWS_PER_STATEMENT);
    }

    #[test]
    fn rejects_insert_exceeding_max_row_count_before_catalog_lookup() {
        // SQL-16・TASK-190: [`MAX_INSERT_ROWS_PER_STATEMENT`] を 1 行超えると
        // `54000` で拒否され、かつ判定は行を積む最中（構造検証段階）に完結する
        // ため `TableLookup::table_exists` へは一切到達しない（副作用ゼロの
        // 根拠。`explicit_null_operation_id_does_not_reach_catalog_lookup` と
        // 同じ `FlaggingCatalog` パターン）。
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let rows: Vec<String> = (0..MAX_INSERT_ROWS_PER_STATEMENT + 1)
            .map(|i| format!("({i})"))
            .collect();
        let sql = format!(
            "INSERT INTO documents (id) VALUES {} USING OPERATION_ID 'op-0001'",
            rows.join(", ")
        );
        let err =
            validate_insert(&sql, &lookup, LedgerMode::Ledgered).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "54000");
        assert!(
            !lookup.called.get(),
            "row count limit must be rejected before the catalog lookup"
        );
    }

    #[test]
    fn same_insert_input_yields_same_classification_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "INSERT INTO documents (id) VALUES (1)";
        let first = validate_insert(sql, &lookup, LedgerMode::Ledgered)
            .unwrap_err()
            .wire_code();
        let second = validate_insert(sql, &lookup, LedgerMode::Ledgered)
            .unwrap_err()
            .wire_code();
        assert_eq!(first, second);
    }

    // --- validate_truncate（SQL-22、TASK-195） ---------------------------------

    #[test]
    fn accepts_truncate_with_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_truncate(
            "TRUNCATE TABLE documents USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("basic TRUNCATE shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert_eq!(
            stmt.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    #[test]
    fn accepts_truncate_with_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_truncate(
            "TRUNCATE TABLE documents USING OPERATION_ID 'op-0001';",
            &lookup,
            LedgerMode::Ledgered,
        )
        .is_ok());
    }

    #[test]
    fn rejects_truncate_missing_table_keyword() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate(
            "TRUNCATE documents USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing TABLE keyword must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_truncate_with_multiple_tables() {
        let lookup = catalog_with(&["documents", "other"]);
        let err = validate_truncate(
            "TRUNCATE TABLE documents, other USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("multiple tables must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_truncate_missing_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate("TRUNCATE TABLE documents", &lookup, LedgerMode::Ledgered)
            .expect_err("missing clause must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_truncate_with_explicit_null_operation_id() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate(
            "TRUNCATE TABLE documents USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("explicit NULL must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_truncate_operation_id_dollar_placeholder() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate(
            "TRUNCATE TABLE documents USING OPERATION_ID $1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("$n placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_truncate_with_trailing_tokens() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate(
            "TRUNCATE TABLE documents USING OPERATION_ID 'op-0001' CASCADE",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("trailing tokens must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_truncate_of_undefined_table() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_truncate(
            "TRUNCATE TABLE ghost USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn compare_only_without_ledger_accepts_truncate_missing_operation_id_clause() {
        // サーバー構成のみが必須化の可否を決める（TASK-92・RECOVER-1）:
        // `CompareOnlyWithoutLedger` では句の省略を許す（validate_insert と同じ契約）。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_truncate(
            "TRUNCATE TABLE documents",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    // --- TASK-161（SQL-12: `USING MODE`／`SET search_mode`）------------------------

    #[test]
    fn accepts_using_mode_clause_after_limit() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 'precision'",
            &lookup,
        )
        .expect("USING MODE clause should be accepted");
        assert_eq!(stmt.search_mode.as_deref(), Some("precision"));
    }

    #[test]
    fn using_mode_clause_is_case_insensitive_on_mode_keyword_only() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 using mode 'recall'",
            &lookup,
        )
        .expect("USING/mode keywords should be case-insensitive");
        // リテラル値自体は完全一致判定（`sql::mode::SearchMode::parse_literal`）の管轄で、
        // 本モジュールは構造のみを見る。ここでは構造受理のみ検査する。
        assert_eq!(stmt.search_mode.as_deref(), Some("recall"));
    }

    #[test]
    fn select_without_using_mode_has_no_search_mode() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("plain SELECT should still be accepted");
        assert_eq!(stmt.search_mode, None);
    }

    #[test]
    fn rejects_using_mode_with_identifier_instead_of_literal() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE recall",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_with_number_literal() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 123",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_dollar_parameter_form() {
        // SQL-12: `USING MODE $n` は MVP では構文エラーで拒否する（拡張クエリプロトコル
        // 対応後の将来形式。`$` はレキサー側で既に許可リスト外として拒否される）。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE $1",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_clause_with_unsupported_word() {
        // `LIMIT` 直後の `USING` は `MODE` のみ許可する。`USING PLAN(...)`（TASK-77・
        // SQL-5）は `ORDER BY` の代替として別の位置でのみ受理するため、`ORDER BY`
        // 併用かつ非規範形（括弧なし）のこの入力はここで `42601` へ落ちる。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING PLAN 'x'",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_duplicate_using_mode_clause() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 'recall' USING MODE 'precision'",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_clause_before_limit() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' USING MODE 'recall' LIMIT 5",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_on_statement_that_is_not_select() {
        // 書き込み系文（`INSERT` 等）は本モジュールが `SELECT`／`SET` 以外を一切
        // 構文として認識しないため、`USING MODE` の有無に関わらず先頭キーワードの
        // 時点で拒否される（SQL-8 の許可リスト検証への統合。SQL-12 の R6）。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "INSERT INTO documents VALUES (1) USING MODE 'recall'",
            &lookup
        )
        .is_err());
    }

    // --- TASK-77（SQL-5: `USING PLAN(...)`）----------------------------------------

    #[test]
    fn accepts_using_plan_normative_form() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents USING PLAN('find the auth handler') LIMIT 5",
            &lookup,
        )
        .expect("normative USING PLAN form should be accepted");
        assert_eq!(stmt.using_plan(), Some("find the auth handler"));
        assert!(matches!(stmt.order_by(), OrderByForm::UsingPlan));
    }

    #[test]
    fn with_using_plan_forces_order_by_to_using_plan_variant() {
        // codex-review P1 指摘対応（PR #266）: 公開 builder が矛盾した
        // `ValidatedStatement`（`using_plan` は `Some` なのに `order_by` は
        // `OrderByForm::Distance` のまま）を構築できてしまうと、
        // `core.rs::EngineCore::execute_sql_in_session` は `using_plan()` の有無
        // だけで分岐するため、呼び出し元が意図した `order_by` が無言で無視され
        // 意図しない `USING PLAN` 経路が実行される事故になり得た。`with_using_plan`
        // が `order_by` を自動的に揃えることを固定する。
        let stmt = ValidatedStatement::new(
            "documents".to_string(),
            Projection::All,
            OrderByForm::Distance {
                column: "embedding".to_string(),
                literal: "[0.1]".to_string(),
            },
            Vec::new(),
            5,
            EvaluationOrder::DEFAULT,
        )
        .with_using_plan(Some("find auth".to_string()));
        assert!(matches!(stmt.order_by(), OrderByForm::UsingPlan));
        assert_eq!(stmt.using_plan(), Some("find auth"));
    }

    #[test]
    fn accepts_using_plan_with_where_and_using_mode() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE visible() USING PLAN('q') LIMIT 5 USING MODE 'precision'",
            &lookup,
        )
        .expect("USING PLAN with WHERE/USING MODE should be accepted");
        assert_eq!(stmt.using_plan(), Some("q"));
        assert_eq!(stmt.search_mode(), Some("precision"));
    }

    #[test]
    fn rejects_using_plan_parameter_form() {
        // `USING PLAN($1)` は拡張クエリプロトコル対応後の将来形式。`$` は字句解析
        // 段階で拒否される（TASK-77・SQL-5）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN($1) LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_using_plan_without_parens() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN 'q' LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_using_plan_together_with_order_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' USING PLAN('q') LIMIT 5",
            &lookup,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_duplicate_using_plan_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM documents USING PLAN('q') USING PLAN('q') LIMIT 5",
            &lookup,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_using_plan_empty_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN('') LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_using_plan_oversized_literal() {
        let lookup = catalog_with(&["documents"]);
        let huge = format!(
            "SELECT * FROM documents USING PLAN('{}') LIMIT 5",
            "x".repeat(MAX_USING_PLAN_LEN + 1)
        );
        let err = validate_statement(&huge, &lookup).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    // --- validate_using_plan_question（Issue #763・NOSQL-2） --------------------
    // NoSQL 表層（`wire-server::http::query::search`）の `search.plan` 束縛が
    // SQL 表層と同一の検証を共有することを直接固定する（第 2 の実行器を
    // 作らない方針の裏付け）。

    #[test]
    fn validate_using_plan_question_accepts_nonempty_within_limit() {
        assert!(validate_using_plan_question("hello world").is_ok());
    }

    #[test]
    fn validate_using_plan_question_rejects_empty() {
        let err = validate_using_plan_question("").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn validate_using_plan_question_accepts_at_limit() {
        let at_limit = "x".repeat(MAX_USING_PLAN_LEN);
        assert!(validate_using_plan_question(&at_limit).is_ok());
    }

    #[test]
    fn validate_using_plan_question_rejects_over_limit() {
        let over_limit = "x".repeat(MAX_USING_PLAN_LEN + 1);
        let err = validate_using_plan_question(&over_limit).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_using_plan_non_string_argument() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN(1) LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_set_search_mode_statement() {
        let lookup = catalog_with(&["documents"]);
        match validate_sql("SET search_mode = 'precision'", &lookup)
            .expect("SET search_mode should be accepted")
        {
            Statement::SetSearchMode { value } => assert_eq!(value, "precision"),
            other => panic!("expected SetSearchMode, got {other:?}"),
        }
    }

    #[test]
    fn set_search_mode_variable_name_is_case_insensitive() {
        let lookup = catalog_with(&["documents"]);
        match validate_sql("SET SEARCH_MODE = 'recall'", &lookup)
            .expect("variable name should be case-insensitive")
        {
            Statement::SetSearchMode { value } => assert_eq!(value, "recall"),
            other => panic!("expected SetSearchMode, got {other:?}"),
        }
    }

    #[test]
    fn rejects_set_of_unsupported_variable() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SET other_variable = 'x'", &lookup).is_err());
    }

    #[test]
    fn rejects_set_search_mode_to_form() {
        // 規範形は `=`。`TO` 形は SQL-12 に規範がないため受理しない。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SET search_mode TO 'recall'", &lookup).is_err());
    }

    #[test]
    fn rejects_set_search_mode_unquoted_value() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SET search_mode = recall", &lookup).is_err());
    }

    #[test]
    fn rejects_reset_search_mode() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("RESET search_mode", &lookup).is_err());
    }

    #[test]
    fn rejects_show_search_mode() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SHOW search_mode", &lookup).is_err());
    }

    #[test]
    fn rejects_set_with_trailing_using_mode_clause() {
        let lookup = catalog_with(&["documents"]);
        assert!(
            validate_sql("SET search_mode = 'recall' USING MODE 'precision'", &lookup).is_err()
        );
    }

    #[test]
    fn validate_statement_rejects_set_search_mode_as_query() {
        // `validate_statement`（後方互換 API）はセッションを持たないため `SET` を
        // 拒否する（R: 黙った no-op にしない）。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement("SET search_mode = 'recall'", &lookup).is_err());
    }

    #[test]
    fn using_mode_and_set_search_mode_are_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 'precision'";
        let first = validate_statement(sql, &lookup).expect("should be accepted");
        let second = validate_statement(sql, &lookup).expect("should be accepted");
        assert_eq!(first, second);

        // 失敗系（未知の SET 変数）も同一入力に対し同一 `wire_code` を返すことを確認する。
        let err_a = validate_sql("SET other_variable = 'x'", &lookup)
            .expect_err("unsupported variable should be rejected")
            .wire_code();
        let err_b = validate_sql("SET other_variable = 'x'", &lookup)
            .expect_err("unsupported variable should be rejected")
            .wire_code();
        assert_eq!(err_a, err_b);
    }

    #[test]
    fn using_and_set_remain_usable_as_table_and_column_identifiers() {
        // codex-review P1 の回帰テスト: `USING`／`SET` を字句解析段階で無条件に
        // キーワード化すると、カタログ上有効な識別子（`[A-Za-z_][A-Za-z0-9_]*`）
        // である `using`／`set` というテーブル名・列名が `FROM`・投影・`ORDER BY`
        // などの識別子位置で使えなくなる未告知の破壊的変更になる。`USING`／`SET` が
        // 構文上必須の位置（`LIMIT` 直後・statement 先頭）以外では、従来どおり
        // `Ident` として通ることを確認する。
        let lookup = catalog_with(&["using", "set"]);

        // テーブル名としての `using`／`set`（FROM 句の識別子位置）。
        let stmt = validate_statement(
            "SELECT * FROM using ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("table named `using` should remain a valid identifier");
        assert_eq!(stmt.table_name, "using");
        let stmt = validate_statement(
            "SELECT * FROM set ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("table named `set` should remain a valid identifier");
        assert_eq!(stmt.table_name, "set");

        // 投影リストの列名としての `using`／`set`。
        validate_statement(
            "SELECT using, set FROM using ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("columns named `using`/`set` should remain valid identifiers");
    }

    // --- 集計 SELECT（TASK-166・SQL-13） ------------------------------------

    fn expect_aggregate(sql: &str, lookup: &impl TableLookup) -> ValidatedAggregate {
        match validate_sql(sql, lookup).expect("expected the aggregate shape to be accepted") {
            Statement::Aggregate(agg) => agg,
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        }
    }

    /// [`AggregateSelectItem::Aggregate`] であることを前提に中身を取り出す
    /// （TASK-167・SQL-14 で `items()` の要素型が `AggregateSelectItem` へ変わった
    /// ことに伴うテストヘルパ）。
    fn expect_agg_item(item: &AggregateSelectItem) -> &AggregateItem {
        match item {
            AggregateSelectItem::Aggregate(item) => item,
            AggregateSelectItem::GroupKey { .. } => {
                panic!("expected an aggregate item, got a GroupKey item")
            }
        }
    }

    #[test]
    fn accepts_count_star() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate("SELECT COUNT(*) FROM documents", &lookup);
        assert_eq!(agg.table_name(), "documents");
        assert_eq!(agg.items().len(), 1);
        let item = expect_agg_item(&agg.items()[0]);
        assert_eq!(item.func, AggregateFunc::Count);
        assert_eq!(item.arg, AggregateArg::Star);
        assert_eq!(item.alias, None);
    }

    #[test]
    fn accepts_count_star_case_insensitive_function_name() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate("SELECT count(*) FROM documents", &lookup);
        assert_eq!(expect_agg_item(&agg.items()[0]).func, AggregateFunc::Count);
    }

    #[test]
    fn accepts_multiple_aggregate_items_with_alias_and_where() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT COUNT(lang), SUM(id) AS total, MIN(lang), MAX(id), AVG(id) FROM documents WHERE visible() AND lang = 'en'",
            &lookup,
        );
        assert_eq!(agg.items().len(), 5);
        let second = expect_agg_item(&agg.items()[1]);
        assert_eq!(second.func, AggregateFunc::Sum);
        assert_eq!(second.alias.as_deref(), Some("total"));
        assert_eq!(agg.where_predicates().len(), 2);
    }

    #[test]
    fn accepts_trailing_semicolon_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        expect_aggregate("SELECT COUNT(*) FROM documents;", &lookup);
    }

    #[test]
    fn accepts_group_by_with_having_order_by_and_limit() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 10",
            &lookup,
        );
        let group_by = agg.group_by().expect("GROUP BY clause must be accepted");
        assert_eq!(group_by.columns, vec!["lang".to_string()]);
        assert_eq!(group_by.having.len(), 1);
        assert_eq!(group_by.having[0].item_name, "n");
        assert_eq!(group_by.having[0].literal, 1.0);
        let order_by = group_by.order_by.as_ref().expect("ORDER BY must be parsed");
        assert_eq!(order_by.target, "n");
        assert!(order_by.descending);
        assert_eq!(group_by.limit, Some(10));
    }

    #[test]
    fn accepts_group_by_limit_with_offset() {
        // Issue #916・SQL-25 (b)・TASK-209: GROUP BY 集計の `LIMIT n OFFSET m`。
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang LIMIT 3 OFFSET 2",
            &lookup,
        );
        let group_by = agg.group_by().expect("GROUP BY clause must be accepted");
        assert_eq!(group_by.limit, Some(3));
        assert_eq!(group_by.offset, 2);
    }

    #[test]
    fn rejects_group_by_offset_without_limit() {
        // Issue #916・SQL-25 (b)・TASK-209: `LIMIT` を伴わない `OFFSET` 単独は
        // `GROUP BY` 集計でも受理しない（後続の `expect_end_of_statement` が拒否）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT lang, COUNT(*) FROM documents GROUP BY lang OFFSET 2",
            &lookup,
        )
        .expect_err("OFFSET without LIMIT must be rejected for GROUP BY aggregates");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_group_key_not_in_select_list_alone() {
        // 集計項目を 1 つも持たない SELECT リスト（`DISTINCT` 相当）は許可しない。
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT lang FROM documents GROUP BY lang", &lookup)
            .expect_err("aggregate-less GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_bare_column_in_select_list_without_group_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT lang, COUNT(*) FROM documents", &lookup)
            .expect_err("bare column reference without GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_select_list_bare_identifier_mismatching_group_by_column() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT id, COUNT(*) FROM documents GROUP BY lang", &lookup)
            .expect_err("mismatching bare identifier must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_multiple_group_by_columns_at_syntax_layer() {
        // SQL-25 (d): 複数列 `GROUP BY` は構文層を通過する（列数上限・重複検査は
        // 別テストで確認する）。`id` 列は非 TEXT のため束縛段（`sql::parser`）で
        // `22000` になるが、それは構文層の関知するところではない
        // （`bind_group_by_column_type_mismatch_is_rejected` 相当。本モジュールは
        // 構造の受理までを担う）。
        let lookup = catalog_with(&["documents"]);
        let statement = validate_sql(
            "SELECT lang, COUNT(*) FROM documents GROUP BY lang, id",
            &lookup,
        )
        .expect("multi-column GROUP BY must be accepted at the syntax layer");
        let Statement::Aggregate(aggregate) = statement else {
            panic!("expected Aggregate statement");
        };
        assert_eq!(
            aggregate.group_by().expect("GROUP BY clause").columns,
            vec!["lang".to_string(), "id".to_string()]
        );
    }

    #[test]
    fn rejects_duplicate_group_by_columns() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT lang, COUNT(*) FROM documents GROUP BY lang, lang",
            &lookup,
        )
        .expect_err("duplicate GROUP BY column must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_group_by_column_count_at_limit() {
        let lookup = catalog_with(&["documents"]);
        let columns: Vec<String> = (0..MAX_GROUP_BY_COLUMNS).map(|i| format!("c{i}")).collect();
        let sql = format!(
            "SELECT {}, COUNT(*) FROM documents GROUP BY {}",
            columns[0],
            columns.join(", ")
        );
        let statement =
            validate_sql(&sql, &lookup).expect("GROUP BY column count at limit must be accepted");
        let Statement::Aggregate(aggregate) = statement else {
            panic!("expected Aggregate statement");
        };
        assert_eq!(
            aggregate.group_by().expect("GROUP BY clause").columns.len(),
            MAX_GROUP_BY_COLUMNS
        );
    }

    #[test]
    fn rejects_group_by_column_count_over_limit() {
        let lookup = catalog_with(&["documents"]);
        let columns: Vec<String> = (0..=MAX_GROUP_BY_COLUMNS)
            .map(|i| format!("c{i}"))
            .collect();
        let sql = format!(
            "SELECT {}, COUNT(*) FROM documents GROUP BY {}",
            columns[0],
            columns.join(", ")
        );
        let err = validate_sql(&sql, &lookup)
            .expect_err("GROUP BY column count over limit must be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_having_without_group_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents HAVING COUNT(*) > 1",
            &lookup,
        )
        .expect_err("HAVING without GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_having_with_string_literal_rhs() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING n > 'x'",
            &lookup,
        )
        .expect_err("HAVING right-hand side must be numeric");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_having_with_negative_literal() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING n > -1",
            &lookup,
        );
        let group_by = agg.group_by().expect("GROUP BY clause must be accepted");
        assert_eq!(group_by.having[0].literal, -1.0);
    }

    #[test]
    fn rejects_group_by_over_max_aggregate_items_worth_of_having_predicates() {
        let lookup = catalog_with(&["documents"]);
        let having: String = std::iter::repeat_n("n > 1", MAX_AGGREGATE_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql =
            format!("SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING {having}");
        let err = validate_sql(&sql, &lookup).expect_err("HAVING predicate count must be bounded");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_order_by_and_limit_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents ORDER BY id LIMIT 10",
            &lookup,
        )
        .expect_err("ORDER BY/LIMIT must be rejected on an aggregate SELECT");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_hint_order_and_using_mode_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents HINT ORDER(rls, scalar, distance)",
            &lookup,
        )
        .expect_err("HINT ORDER must be rejected on an aggregate SELECT");
        assert_eq!(err.wire_code(), "42601");
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents USING MODE 'recall'",
            &lookup,
        )
        .expect_err("USING MODE must be rejected on an aggregate SELECT");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_mixed_aggregate_and_non_aggregate_items_either_order() {
        let lookup = catalog_with(&["documents"]);
        assert_eq!(
            validate_sql("SELECT id, COUNT(*) FROM documents", &lookup)
                .expect_err("mixing non-aggregate then aggregate must be rejected")
                .wire_code(),
            "42601"
        );
        assert_eq!(
            validate_sql("SELECT COUNT(*), id FROM documents", &lookup)
                .expect_err("mixing aggregate then non-aggregate must be rejected")
                .wire_code(),
            "42601"
        );
    }

    #[test]
    fn rejects_aggregate_call_inside_where_expression() {
        let lookup = catalog_with(&["documents"]);
        // `WHERE COUNT(id) > 1` は先頭トークンが集計形の判定に一致しない
        // （`SELECT` の直後は `WHERE` ではない）ため通常 SELECT として構文解析
        // され、`parse_call_expr` が集計名を拒否して `42601` になる。
        let err = validate_sql(
            "SELECT id FROM documents WHERE COUNT(id) > 1 ORDER BY id <=> '[0.1]' LIMIT 1",
            &lookup,
        )
        .expect_err("aggregate call in WHERE must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_nested_aggregate_call() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT(SUM(id)) FROM documents", &lookup)
            .expect_err("nested aggregate call must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_star_outside_count() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT SUM(*) FROM documents", &lookup)
            .expect_err("'*' outside COUNT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_empty_argument_list() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT() FROM documents", &lookup)
            .expect_err("COUNT() must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    /// SQL-25 (c)・TASK-209 で `COUNT(DISTINCT <expr>)` を受理するよう仕様変更
    /// （旧名 `rejects_distinct_modifier` から反転。アサーションの弱体化ではなく
    /// 許可リストの構造判定のみを確認する回帰テスト）。
    #[test]
    fn accepts_count_distinct_modifier() {
        let lookup = catalog_with(&["documents"]);
        validate_sql("SELECT COUNT(DISTINCT lang) FROM documents", &lookup)
            .expect("COUNT(DISTINCT ...) must be accepted (SQL-25 (c))");
    }

    #[test]
    fn rejects_count_distinct_star() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT(DISTINCT *) FROM documents", &lookup)
            .expect_err("COUNT(DISTINCT *) must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_sum_distinct() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT SUM(DISTINCT id) FROM documents", &lookup)
            .expect_err("DISTINCT is only allowed inside COUNT");
        assert_eq!(err.wire_code(), "42601");
    }

    /// `distinct` という列名の後方互換（SQL-25 (c) 追加前の既存解釈を維持する）。
    #[test]
    fn accepts_count_of_column_named_distinct() {
        let lookup = catalog_with(&["documents"]);
        validate_sql("SELECT COUNT(distinct) FROM documents", &lookup)
            .expect("COUNT(distinct) (column named 'distinct') must remain accepted");
    }

    #[test]
    fn accepts_select_distinct_column() {
        let lookup = catalog_with(&["documents"]);
        validate_sql("SELECT DISTINCT lang FROM documents", &lookup)
            .expect("SELECT DISTINCT <column> must be accepted (SQL-25 (c))");
    }

    #[test]
    fn accepts_select_distinct_bare_column_named_distinct_as_projection() {
        // `distinct` という列名を持つ表への `SELECT distinct FROM t LIMIT n`
        // （既存の広域取得としての解釈）は `is_distinct_modifier` の「次が
        // `FROM` なら列名」判定で維持される。
        let lookup = catalog_with(&["documents"]);
        validate_sql("SELECT distinct FROM documents LIMIT 5", &lookup)
            .expect("bare column named 'distinct' must remain accepted as a projection");
    }

    #[test]
    fn accepts_select_distinct_column_with_alias_in_group_by_projection() {
        // PR #1098 レビュー対応（codex/review P1）: `is_distinct_modifier` が
        // 次トークンを任意の `Ident` とみなして `DISTINCT` 修飾子と誤判定すると、
        // `distinct` という列名に `AS <alias>` を付けた既存の集計 SELECT リスト
        // 項目（[`Parser::parse_aggregate_select_item`] が受理する「裸の識別子
        // ＋ 任意の `AS <alias>`」の形。GROUP BY 対象列として使う場合に現れる）
        // まで `parse_distinct_shape` へ誤って振り分けられ `42601` で拒否されて
        // いた。次トークンが `AS` の場合は列名側へ振り分けることで、この形状が
        // 引き続き受理されることを固定する。
        let lookup = catalog_with(&["documents"]);
        let statement = validate_sql(
            "SELECT distinct AS d, COUNT(*) FROM documents GROUP BY distinct",
            &lookup,
        )
        .expect(
            "column named 'distinct' with AS alias in GROUP BY projection must remain accepted",
        );
        match statement {
            Statement::Aggregate(agg) => {
                assert_eq!(
                    agg.group_by
                        .as_ref()
                        .and_then(|g| g.columns.first().map(String::as_str)),
                    Some("distinct")
                );
            }
            other => panic!("expected Aggregate statement, got {other:?}"),
        }
    }

    #[test]
    fn accepts_select_distinct_column_named_as() {
        // PR #1098 レビュー対応（codex/review P1・Cursor Bugbot 指摘）:
        // `is_distinct_modifier` が次トークン `Ident("AS")` を常に列名側の目印と
        // 誤判定すると、列名 `as`（`catalog::validate_identifier` が許可する
        // 識別子）を対象にした `SELECT DISTINCT as ...` まで列名 `distinct` の
        // 投影として解釈され `42601` で拒否されていた。`AS` の 1 つ先の
        // トークンまで見て「別名なし＝列名 as に対する DISTINCT 修飾子」と
        // 判定することで、この形状が受理されることを固定する。
        let lookup = catalog_with(&["documents"]);
        let statement = validate_sql("SELECT DISTINCT as FROM documents", &lookup)
            .expect("SELECT DISTINCT <column named 'as'> must be accepted");
        match statement {
            Statement::Aggregate(agg) => {
                assert_eq!(
                    agg.group_by
                        .as_ref()
                        .and_then(|g| g.columns.first().map(String::as_str)),
                    Some("as")
                );
            }
            other => panic!("expected Aggregate statement, got {other:?}"),
        }
    }

    #[test]
    fn accepts_count_distinct_column_named_as() {
        // 上記と同じ曖昧さの `COUNT(DISTINCT <expr>)`（[`Parser::parse_aggregate_item`]）
        // 側での固定（PR #1098 レビュー対応）。
        let lookup = catalog_with(&["documents"]);
        validate_sql("SELECT COUNT(DISTINCT as) FROM documents", &lookup)
            .expect("COUNT(DISTINCT <column named 'as'>) must be accepted");
    }

    #[test]
    fn accepts_select_distinct_column_named_as_with_alias() {
        // PR #1098 レビュー再指摘（codex/review P1）: `is_distinct_modifier` が
        // `tokens[pos+1]` の `Ident("AS")` を見た時点で `tokens[pos+2]` が
        // `Ident` なら無条件に「`DISTINCT` 自体が列名」と判定すると、列名 `as`
        // （`AS` と字句上区別できない）自体に別名を付ける
        // `SELECT DISTINCT as AS alias FROM t` まで、2 段目の `AS alias` を
        // 余剰トークンとして `42601` で拒否していた。`tokens[pos+2]` 自体が
        // さらに `AS` 由来で `tokens[pos+3]` が識別子（真の別名）の場合は
        // 列名 `as` に対する `DISTINCT` 修飾子と判定することで、この形状が
        // 受理されることを固定する。
        let lookup = catalog_with(&["documents"]);
        let statement = validate_sql("SELECT DISTINCT as AS alias FROM documents", &lookup)
            .expect("SELECT DISTINCT <column named 'as'> AS <alias> must be accepted");
        match statement {
            Statement::Aggregate(agg) => {
                assert_eq!(
                    agg.group_by
                        .as_ref()
                        .and_then(|g| g.columns.first().map(String::as_str)),
                    Some("as")
                );
                match agg.items.as_slice() {
                    [AggregateSelectItem::GroupKey { column, alias }] => {
                        assert_eq!(column, "as");
                        assert_eq!(alias.as_deref(), Some("alias"));
                    }
                    other => panic!("expected single GroupKey item, got {other:?}"),
                }
            }
            other => panic!("expected Aggregate statement, got {other:?}"),
        }
    }

    #[test]
    fn rejects_select_distinct_star_with_vector_ranking() {
        // `SELECT DISTINCT *` とベクトル順位付けの併用は引き続き `42601` で
        // 拒否する（SQL-25 (a) 参照。既存の `rejects_distinct` と同じ入力を、
        // `SELECT DISTINCT` 対応後も一貫して拒否することを確認する）。
        assert_rejected_as_syntax_error(
            "SELECT DISTINCT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_select_distinct_with_explicit_group_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT DISTINCT lang FROM documents GROUP BY lang", &lookup)
            .expect_err("SELECT DISTINCT does not accept an explicit GROUP BY clause");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_explain_select_distinct() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "EXPLAIN SELECT DISTINCT lang FROM documents USING PLAN(full_scan())",
            &lookup,
        )
        .expect_err("EXPLAIN does not support SELECT DISTINCT");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_aggregate_call_in_create_function_body() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("CREATE FUNCTION f(x) AS COUNT(x)", &lookup)
            .expect_err("aggregate call in CREATE FUNCTION body must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_multiple_aggregate_statements() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents; SELECT COUNT(*) FROM documents",
            &lookup,
        )
        .expect_err("multiple statements must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_undefined_table_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT(*) FROM ghost", &lookup)
            .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn rejects_too_many_aggregate_items() {
        let lookup = catalog_with(&["documents"]);
        let items = std::iter::repeat_n("COUNT(*)", MAX_AGGREGATE_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {items} FROM documents");
        let err = validate_sql(&sql, &lookup)
            .expect_err("exceeding MAX_AGGREGATE_ITEMS must be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn accepts_exactly_max_aggregate_items() {
        let lookup = catalog_with(&["documents"]);
        let items = std::iter::repeat_n("COUNT(*)", MAX_AGGREGATE_ITEMS)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {items} FROM documents");
        let agg = expect_aggregate(&sql, &lookup);
        assert_eq!(agg.items().len(), MAX_AGGREGATE_ITEMS);
    }

    #[test]
    fn validate_statement_rejects_aggregate_select_as_search_query() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT COUNT(*) FROM documents", &lookup)
            .expect_err("aggregate SELECT must be rejected by the SELECT-only entry point");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn aggregate_classification_is_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "SELECT COUNT(*) FROM documents WHERE lang = 'en'";
        let first = validate_sql(sql, &lookup).expect("first call should succeed");
        let second = validate_sql(sql, &lookup).expect("second call should succeed");
        assert_eq!(first, second);
    }

    // --- 広域取得（ソートなしのフィルタ取得。Issue #454） -----------------------

    fn expect_scan(sql: &str, lookup: &impl TableLookup) -> ValidatedScan {
        match validate_sql(sql, lookup).expect("expected the scan shape to be accepted") {
            Statement::Scan(scan) => scan,
            other => panic!("expected Statement::Scan, got {other:?}"),
        }
    }

    #[test]
    fn accepts_bare_select_star_limit() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan("SELECT * FROM documents LIMIT 500", &lookup);
        assert_eq!(scan.table_name(), "documents");
        assert_eq!(scan.limit(), 500);
        assert!(scan.where_predicates().is_empty());
    }

    #[test]
    fn accepts_scan_with_where_and_column_list() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "SELECT id, body FROM documents WHERE lang = 'ja' LIMIT 10",
            &lookup,
        );
        assert_eq!(scan.where_predicates().len(), 1);
        assert!(matches!(scan.projection(), Projection::Columns(cols) if cols == &["id", "body"]));
    }

    // --- OFFSET（Issue #916・SQL-25 (b)・TASK-209） ----------------------------

    #[test]
    fn accepts_scan_with_limit_and_offset() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan("SELECT * FROM documents LIMIT 5 OFFSET 10", &lookup);
        assert_eq!(scan.limit(), 5);
        assert_eq!(scan.offset(), 10);
    }

    #[test]
    fn accepts_scan_with_offset_zero() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan("SELECT * FROM documents LIMIT 5 OFFSET 0", &lookup);
        assert_eq!(scan.offset(), 0);
    }

    #[test]
    fn accepts_scan_without_offset_defaults_to_zero() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan("SELECT * FROM documents LIMIT 5", &lookup);
        assert_eq!(scan.offset(), 0);
    }

    #[test]
    fn offset_does_not_shadow_offset_as_column_or_table_name() {
        // `OFFSET` は `lexer::Keyword` に追加していない（`peek_ident_matches` による
        // この位置限定の文脈識別子）ため、列名・テーブル名としての `offset` は
        // 従来どおり使える。
        let lookup = catalog_with(&["offset"]);
        let scan = expect_scan("SELECT offset FROM offset LIMIT 5", &lookup);
        assert_eq!(scan.table_name(), "offset");
        assert!(matches!(scan.projection(), Projection::Columns(cols) if cols == &["offset"]));
    }

    #[test]
    fn rejects_offset_before_limit() {
        assert_rejected_as_syntax_error("SELECT * FROM documents OFFSET 10 LIMIT 5");
    }

    #[test]
    fn rejects_offset_without_limit() {
        assert_rejected_as_syntax_error("SELECT * FROM documents OFFSET 10");
    }

    #[test]
    fn rejects_offset_rows_suffix() {
        assert_rejected_as_syntax_error("SELECT * FROM documents LIMIT 5 OFFSET 10 ROWS");
    }

    #[test]
    fn rejects_negative_offset() {
        assert_rejected_as_syntax_error("SELECT * FROM documents LIMIT 5 OFFSET -1");
    }

    #[test]
    fn rejects_fractional_offset() {
        assert_rejected_as_syntax_error("SELECT * FROM documents LIMIT 5 OFFSET 1.5");
    }

    #[test]
    fn rejects_offset_exceeding_u32() {
        assert_rejected_as_syntax_error("SELECT * FROM documents LIMIT 5 OFFSET 4294967296");
    }

    #[test]
    fn rejects_offset_with_trailing_using_mode() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents LIMIT 5 OFFSET 10 USING MODE 'precision'",
        );
    }

    #[test]
    fn rejects_search_select_with_using_plan_and_offset() {
        // `USING PLAN(...)` 経路（ランキング段を `USING PLAN` の展開結果が決める）も
        // 検索 SELECT の一種であり、`OFFSET` は構造上受理しない（§計画 3.1）。
        assert_rejected_as_syntax_error("SELECT * FROM documents USING PLAN('q') LIMIT 5 OFFSET 1");
    }

    #[test]
    fn rejects_scan_missing_limit() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT * FROM documents", &lookup)
            .expect_err("SELECT without ORDER BY/USING PLAN/LIMIT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_scan_with_using_mode_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT * FROM documents LIMIT 10 USING MODE 'precision'",
            &lookup,
        )
        .expect_err("bare LIMIT scan must not accept USING MODE");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_scan_with_hint_order_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT * FROM documents LIMIT 10 HINT ORDER(DISTANCE, SCALAR, RLS)",
            &lookup,
        )
        .expect_err("bare LIMIT scan must not accept HINT ORDER");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_undefined_table_for_scan_with_42p01() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT * FROM ghost LIMIT 10", &lookup)
            .expect_err("scan against an undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn validate_statement_rejects_scan_as_search_query() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents LIMIT 10", &lookup)
            .expect_err("scan must be rejected by the SELECT-only (ORDER BY) entry point");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn explain_accepts_scan_shape() {
        // Issue #922（SQL-27）: `EXPLAIN` の対象を広域取得へ拡大したため、
        // bare LIMIT scan の前置はもはや拒否されず `ExplainTarget::Scan` として
        // 受理される（受理テストへ反転）。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_sql("EXPLAIN SELECT * FROM documents LIMIT 10", &lookup)
            .expect("EXPLAIN over a bare LIMIT scan must be accepted");
        match stmt {
            Statement::Explain(ExplainTarget::Scan(scan)) => {
                assert_eq!(scan.table_name(), "documents");
            }
            other => panic!("expected ExplainTarget::Scan, got {other:?}"),
        }
    }

    #[test]
    fn scan_classification_is_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "SELECT * FROM documents WHERE lang = 'en' LIMIT 10";
        let first = validate_sql(sql, &lookup).expect("first call should succeed");
        let second = validate_sql(sql, &lookup).expect("second call should succeed");
        assert_eq!(first, second);
    }

    // --- validate_update（SQL-17、TASK-191） -----------------------------------

    #[test]
    fn accepts_update_with_single_assignment() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("basic UPDATE shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert_eq!(
            stmt.assignments,
            vec![("lang".to_string(), InsertLiteral::String("en".to_string()))]
        );
        assert_eq!(stmt.id_literal, "1");
        assert_eq!(
            stmt.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    #[test]
    fn accepts_update_with_multiple_assignments_preserving_declared_order() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_update(
            "UPDATE documents SET body = 'x', lang = 'ja' WHERE id = 7 USING OPERATION_ID 'op-0002'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("multiple SET assignments should be accepted");
        assert_eq!(
            stmt.assignments,
            vec![
                ("body".to_string(), InsertLiteral::String("x".to_string())),
                ("lang".to_string(), InsertLiteral::String("ja".to_string())),
            ]
        );
    }

    #[test]
    fn accepts_update_with_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001';",
            &lookup,
            LedgerMode::Ledgered,
        )
        .is_ok());
    }

    #[test]
    fn rejects_update_missing_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing clause must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_update_with_explicit_null_operation_id() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("explicit NULL must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn update_structural_validation_never_queries_catalog_when_operation_id_is_missing() {
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_update(
            "UPDATE nope SET lang = 'en' WHERE id = 1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_update_operation_id_dollar_placeholder() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID $1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("$n placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_with_duplicate_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'a' USING OPERATION_ID 'b'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("duplicate clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_with_returning_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001' RETURNING id",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("RETURNING must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_with_multiple_tables() {
        let lookup = catalog_with(&["documents", "notes"]);
        let err = validate_update(
            "UPDATE documents, notes SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("multiple tables must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_with_subquery_set_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = (SELECT lang FROM documents) WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("subquery SET value must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_with_predicate_where_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("predicate-form WHERE must be rejected (SQL-19 scope, not SQL-17)");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_missing_where_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing WHERE clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_where_id_with_non_numeric_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 'x' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("non-numeric id literal must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_undefined_table_for_update_with_42p01() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update(
            "UPDATE ghost SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    // --- SQL-19・TASK-192: 述語つき UPDATE ... WHERE の許可リスト・振り分け ---

    #[test]
    fn validate_update_form_accepts_equality_predicate() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("equality predicate WHERE must be accepted");
        match form {
            ValidatedUpdateForm::Predicate(p) => {
                assert_eq!(p.where_predicates.len(), 1);
                assert!(matches!(
                    &p.where_predicates[0],
                    WherePredicate::Equality { column, value }
                        if column == "lang" && value == "ja"
                ));
            }
            ValidatedUpdateForm::Single(_) => panic!("expected Predicate variant"),
        }
    }

    #[test]
    fn validate_update_form_accepts_prefix_predicate() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE path LIKE 'src/%' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("prefix predicate WHERE must be accepted");
        assert!(matches!(form, ValidatedUpdateForm::Predicate(_)));
    }

    #[test]
    fn validate_update_form_accepts_id_range_and_equality_predicate() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE id > 10 AND lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("id range combined with equality must be accepted as predicate form");
        match form {
            ValidatedUpdateForm::Predicate(p) => assert_eq!(p.where_predicates.len(), 2),
            ValidatedUpdateForm::Single(_) => panic!("expected Predicate variant"),
        }
    }

    #[test]
    fn validate_update_form_accepts_id_equality_combined_with_and_as_predicate() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE id = 5 AND lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("id = 5 AND ... must fall through to predicate form");
        match form {
            ValidatedUpdateForm::Predicate(p) => {
                assert_eq!(p.where_predicates.len(), 2);
                assert!(matches!(
                    &p.where_predicates[0],
                    WherePredicate::Expression(Expr::Binary { op: BinOp::Eq, .. })
                ));
            }
            ValidatedUpdateForm::Single(_) => panic!("expected Predicate variant"),
        }
    }

    #[test]
    fn validate_update_form_accepts_visible_combined_with_predicate() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE visible() AND lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("visible() combined with a predicate must be accepted");
        match form {
            ValidatedUpdateForm::Predicate(p) => assert_eq!(p.where_predicates.len(), 2),
            ValidatedUpdateForm::Single(_) => panic!("expected Predicate variant"),
        }
    }

    #[test]
    fn validate_update_form_dispatches_id_simple_form_with_operation_id_suffix() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET lang = 'en' WHERE id = 5 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("id simple form must be accepted");
        match form {
            ValidatedUpdateForm::Single(u) => assert_eq!(u.id_literal, "5"),
            ValidatedUpdateForm::Predicate(_) => panic!("expected Single variant"),
        }
    }

    #[test]
    fn validate_update_form_dispatches_id_simple_form_with_semicolon() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET lang = 'en' WHERE id = 5;",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("id simple form followed by ';' must be accepted");
        assert!(matches!(form, ValidatedUpdateForm::Single(_)));
    }

    #[test]
    fn validate_update_form_dispatches_id_simple_form_with_end_of_statement() {
        let lookup = catalog_with(&["documents"]);
        let form = validate_update_form(
            "UPDATE documents SET lang = 'en' WHERE id = 5",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("id simple form at end of statement must be accepted");
        assert!(matches!(form, ValidatedUpdateForm::Single(_)));
    }

    #[test]
    fn validate_update_form_still_rejects_missing_where_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET lang = 'en' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing WHERE clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_still_rejects_hint_order_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' HINT ORDER(RLS, SCALAR, DISTANCE) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("HINT ORDER suffix must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_still_rejects_using_mode_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' USING MODE 'recall' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("USING MODE suffix must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_still_rejects_order_by_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' ORDER BY id LIMIT 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("ORDER BY suffix must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_still_rejects_limit_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' LIMIT 1 USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("LIMIT suffix must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_still_rejects_returning_suffix() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-0001' RETURNING id",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("RETURNING suffix must be rejected as trailing tokens");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_still_rejects_multiple_tables() {
        let lookup = catalog_with(&["documents", "notes"]);
        let err = validate_update_form(
            "UPDATE documents, notes SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("multiple tables must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_update_form_predicate_operation_id_guard_precedes_catalog_lookup() {
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_update_form(
            "UPDATE nope SET lang = 'en' WHERE lang = 'ja'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing operation_id must be rejected before catalog lookup");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn validate_update_form_rejects_undefined_table_for_predicate_form() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_update_form(
            "UPDATE ghost SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn validate_update_form_is_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "UPDATE documents SET body = 'x' WHERE lang = 'ja' AND path LIKE 'src/%' AND id > 10 USING OPERATION_ID 'op-0001'";
        let first = validate_update_form(sql, &lookup, LedgerMode::Ledgered)
            .expect("first call must succeed");
        let second = validate_update_form(sql, &lookup, LedgerMode::Ledgered)
            .expect("second call must succeed");
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_update_set_assignment_count_over_limit() {
        let lookup = catalog_with(&["documents"]);
        let assignments: Vec<String> = (0..=MAX_UPDATE_SET_ASSIGNMENTS)
            .map(|i| format!("col{i} = 'v'"))
            .collect();
        let sql = format!(
            "UPDATE documents SET {} WHERE id = 1 USING OPERATION_ID 'op-0001'",
            assignments.join(", ")
        );
        let err = validate_update(&sql, &lookup, LedgerMode::Ledgered)
            .expect_err("SET assignment count over limit must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn update_validation_is_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'";
        let first =
            validate_update(sql, &lookup, LedgerMode::Ledgered).expect("first call should succeed");
        let second = validate_update(sql, &lookup, LedgerMode::Ledgered)
            .expect("second call should succeed");
        assert_eq!(first, second);
    }

    #[test]
    fn compare_only_without_ledger_accepts_missing_operation_id_clause_for_update() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_update(
            "UPDATE documents SET lang = 'en' WHERE id = 1",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    /// PR #1044 レビュー時の境界値誤検知（ちょうど `MAX_CREATE_TABLE_COLUMNS`
    /// 列の `CREATE TABLE` が off-by-one で誤って `54000` 拒否されるという
    /// 懸念）に対する回帰テスト。ちょうど上限数の列は受理される。
    #[test]
    fn create_table_accepts_exactly_max_columns() {
        let cols: Vec<String> = (0..MAX_CREATE_TABLE_COLUMNS)
            .map(|i| format!("c{i} TEXT"))
            .collect();
        let sql = format!("CREATE TABLE t ({})", cols.join(", "));
        let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
        let result = validate_create_table_tokens(&tokens);
        match &result {
            Ok(v) => assert_eq!(v.columns.len(), MAX_CREATE_TABLE_COLUMNS),
            Err(e) => {
                panic!("expected ok for exactly {MAX_CREATE_TABLE_COLUMNS} columns, got err: {e:?}")
            }
        }
    }

    /// 上限を 1 つ超える列数は `payload_too_large`（`54000`）で拒否される
    /// （上記回帰テストと対の境界値検証）。
    #[test]
    fn create_table_rejects_max_columns_plus_one() {
        let cols: Vec<String> = (0..MAX_CREATE_TABLE_COLUMNS + 1)
            .map(|i| format!("c{i} TEXT"))
            .collect();
        let sql = format!("CREATE TABLE t ({})", cols.join(", "));
        let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
        let result = validate_create_table_tokens(&tokens);
        match result {
            Err(SqlSurfaceError::PayloadTooLarge { .. }) => {}
            other => panic!("expected PayloadTooLarge, got: {other:?}"),
        }
    }

    /// Cursor Bugbot 指摘（PR #1050）に対する回帰テスト: ちょうど上限数の
    /// 列を宣言したテーブルへ末尾で表制約 `PRIMARY KEY (...)` を付けても、
    /// 表制約は `columns` を消費しないため受理される（先頭・中間に置いた
    /// 場合と対称な挙動になることを固定する）。
    #[test]
    fn create_table_accepts_trailing_primary_key_constraint_at_max_columns() {
        let cols: Vec<String> = (0..MAX_CREATE_TABLE_COLUMNS)
            .map(|i| format!("c{i} TEXT"))
            .collect();
        let sql = format!("CREATE TABLE t ({}, PRIMARY KEY (c0))", cols.join(", "));
        let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
        let result = validate_create_table_tokens(&tokens);
        match &result {
            Ok(v) => {
                assert_eq!(v.columns.len(), MAX_CREATE_TABLE_COLUMNS);
                assert_eq!(v.primary_key, Some(vec!["c0".to_string()]));
            }
            Err(e) => panic!(
                "expected ok for exactly {MAX_CREATE_TABLE_COLUMNS} columns with trailing PRIMARY KEY, got err: {e:?}"
            ),
        }
    }

    /// `c0..cN` の `N` 列ぶんの `TEXT` 列定義を返す（列数上限テスト用）。
    fn text_columns(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("c{i} TEXT")).collect()
    }

    /// ちょうど上限数の列に表制約 `UNIQUE (...)` を先頭・中間・末尾のいずれに
    /// 置いても受理される（表制約は列を追加しないため列数判定の対象外。
    /// TABLE-16・TASK-204、Issue #905）。
    #[test]
    fn create_table_accepts_max_columns_with_unique_table_constraint_anywhere() {
        let head = text_columns(0..MAX_CREATE_TABLE_COLUMNS / 2).join(", ");
        let tail = text_columns(MAX_CREATE_TABLE_COLUMNS / 2..MAX_CREATE_TABLE_COLUMNS).join(", ");
        for sql in [
            format!("CREATE TABLE t (UNIQUE (c0), {head}, {tail})"),
            format!("CREATE TABLE t ({head}, UNIQUE (c0, c1), {tail})"),
            format!("CREATE TABLE t ({head}, {tail}, UNIQUE (c0))"),
            format!("CREATE TABLE t ({head}, {tail}, UNIQUE (c0), PRIMARY KEY (c1))"),
        ] {
            let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
            let v = validate_create_table_tokens(&tokens)
                .unwrap_or_else(|e| panic!("expected ok for max columns + UNIQUE, got: {e:?}"));
            assert_eq!(v.columns.len(), MAX_CREATE_TABLE_COLUMNS);
            assert!(!v.unique_constraints.is_empty());
        }
    }

    /// 上限を 1 列超える列定義は、表制約 `UNIQUE (...)`／`PRIMARY KEY (...)` の
    /// 前・間・後ろのどこに超過列があっても `54000` で拒否される（Issue #905
    /// レビュー指摘の回帰: 表制約の直後に続く最後の超過列が、カンマ直後の
    /// 先読みに依存した旧判定をすり抜けて 257 列を受理していた）。
    #[test]
    fn create_table_rejects_excess_column_regardless_of_unique_constraint_position() {
        let max = MAX_CREATE_TABLE_COLUMNS;
        let all = text_columns(0..max).join(", ");
        let head = text_columns(0..max / 2).join(", ");
        let tail = text_columns(max / 2..max).join(", ");
        let extra = format!("c{max} TEXT");
        for sql in [
            // 超過列が UNIQUE の前。
            format!("CREATE TABLE t ({all}, {extra}, UNIQUE (c0))"),
            // 超過列が UNIQUE の後ろ（かつ最後の列）。レビュー指摘の再現形。
            format!("CREATE TABLE t ({all}, UNIQUE (c0), {extra})"),
            // 超過列が 2 つの表制約の間。
            format!("CREATE TABLE t ({all}, UNIQUE (c0), {extra}, UNIQUE (c1))"),
            // 表制約が列の途中にあり、超過列が末尾。
            format!("CREATE TABLE t ({head}, UNIQUE (c0), {tail}, {extra})"),
            // 主キー表制約の直後に続く最後の超過列（同じ判定を共有する）。
            format!("CREATE TABLE t ({all}, PRIMARY KEY (c0), {extra})"),
        ] {
            let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
            match validate_create_table_tokens(&tokens) {
                Err(SqlSurfaceError::PayloadTooLarge { .. }) => {}
                other => panic!("expected PayloadTooLarge, got: {other:?}"),
            }
        }
    }

    /// 列制約・表制約の `UNIQUE` 構文の受理形と拒否形（TABLE-16・TASK-204、
    /// Issue #905）。
    #[test]
    fn create_table_unique_syntax_accept_and_reject_forms() {
        let ok = |sql: &str| {
            let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
            validate_create_table_tokens(&tokens)
                .unwrap_or_else(|e| panic!("expected {sql:?} to be accepted, got: {e:?}"))
        };
        let v =
            ok("CREATE TABLE t (a TEXT UNIQUE, b TEXT NOT NULL UNIQUE DEFAULT 'x', UNIQUE (a, b))");
        let lists: Vec<Vec<String>> = v
            .unique_constraints
            .iter()
            .map(|u| u.columns().to_vec())
            .collect();
        assert_eq!(
            lists,
            vec![
                vec!["a".to_string()],
                vec!["b".to_string()],
                vec!["a".to_string(), "b".to_string()],
            ]
        );
        // 列名 `unique` は列定義として解釈される（表制約とは `(` の有無で区別）。
        let v = ok("CREATE TABLE t (unique TEXT)");
        assert!(v.unique_constraints.is_empty());

        for (sql, code) in [
            ("CREATE TABLE t (a TEXT, UNIQUE (z))", "42601"),
            ("CREATE TABLE t (a TEXT, UNIQUE (id))", "42601"),
            ("CREATE TABLE t (e VECTOR(4) UNIQUE)", "42601"),
            ("CREATE TABLE t (e VECTOR(4), UNIQUE (e))", "42601"),
            ("CREATE TABLE t (a TEXT UNIQUE UNIQUE)", "42601"),
            ("CREATE TABLE t (a TEXT, UNIQUE ())", "42601"),
            ("CREATE TABLE t (a TEXT, UNIQUE (a, a))", "42701"),
        ] {
            let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
            let err = validate_create_table_tokens(&tokens)
                .expect_err("expected UNIQUE form to be rejected");
            assert_eq!(
                crate::error_format::ClassifiedError::wire_code(&err),
                code,
                "{sql:?}: {err:?}"
            );
        }
    }

    /// 制約数・制約あたり列数の上限超過は `Vec` へ積む前に `54000` で拒否される。
    #[test]
    fn create_table_rejects_unique_limits_exceeded() {
        let cols = text_columns(0..crate::catalog::MAX_UNIQUE_CONSTRAINT_COLUMNS + 1);
        let names: Vec<String> = (0..=crate::catalog::MAX_UNIQUE_CONSTRAINT_COLUMNS)
            .map(|i| format!("c{i}"))
            .collect();
        let too_wide = format!(
            "CREATE TABLE t ({}, UNIQUE ({}))",
            cols.join(", "),
            names.join(", ")
        );
        let too_many = format!(
            "CREATE TABLE t (a TEXT, {})",
            (0..=crate::catalog::MAX_UNIQUE_CONSTRAINTS)
                .map(|_| "UNIQUE (a)")
                .collect::<Vec<_>>()
                .join(", ")
        );
        for sql in [too_wide, too_many] {
            let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
            match validate_create_table_tokens(&tokens) {
                Err(SqlSurfaceError::PayloadTooLarge { .. }) => {}
                other => panic!("expected PayloadTooLarge, got: {other:?}"),
            }
        }
    }

    // --- CHECK 制約構文（TABLE-16・TASK-204、Issue #906） -----------------

    fn parse_create_table_ok(sql: &str) -> ValidatedCreateTable {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        validate_create_table_tokens(&tokens).unwrap_or_else(|e| panic!("expected ok, got {e:?}"))
    }

    fn parse_create_table_err(sql: &str) -> SqlSurfaceError {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        match validate_create_table_tokens(&tokens) {
            Ok(v) => panic!("expected error, got ok: {v:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn create_table_accepts_column_level_check_without_name() {
        let v = parse_create_table_ok("CREATE TABLE t (kind TEXT CHECK (kind = 'a'), body TEXT)");
        assert_eq!(v.columns.len(), 2);
        assert_eq!(v.checks.len(), 1);
        assert_eq!(v.checks[0].name, None);
        assert_eq!(v.checks[0].column.as_deref(), Some("kind"));
    }

    #[test]
    fn create_table_accepts_column_level_check_with_named_constraint() {
        let v = parse_create_table_ok(
            "CREATE TABLE t (kind TEXT CONSTRAINT kind_ck CHECK (kind = 'a'))",
        );
        assert_eq!(v.checks.len(), 1);
        assert_eq!(v.checks[0].name.as_deref(), Some("kind_ck"));
        assert_eq!(v.checks[0].column.as_deref(), Some("kind"));
    }

    #[test]
    fn create_table_accepts_check_after_other_column_constraints() {
        // `NOT NULL`／`UNIQUE` 等の列制約の後ろに複数の `CHECK` を置ける。
        let v = parse_create_table_ok(
            "CREATE TABLE t (kind TEXT NOT NULL UNIQUE CHECK (kind = 'a') CHECK (kind LIKE 'a%'))",
        );
        assert_eq!(v.checks.len(), 2);
        assert!(!v.columns[0].nullable);
        assert_eq!(v.unique_constraints.len(), 1);
    }

    #[test]
    fn create_table_accepts_table_level_check() {
        let v = parse_create_table_ok(
            "CREATE TABLE t (kind TEXT, status TEXT, CHECK (kind = 'a' AND status = 'b'))",
        );
        assert_eq!(v.columns.len(), 2);
        assert_eq!(v.checks.len(), 1);
        assert_eq!(v.checks[0].column, None);
        assert_eq!(v.checks[0].predicates.len(), 2);
    }

    #[test]
    fn create_table_accepts_table_level_check_with_named_constraint_anywhere() {
        let v = parse_create_table_ok(
            "CREATE TABLE t (CONSTRAINT t_check CHECK (kind = 'a'), kind TEXT, PRIMARY KEY (kind))",
        );
        assert_eq!(v.checks[0].name.as_deref(), Some("t_check"));
        assert_eq!(v.columns.len(), 1);
        assert_eq!(v.primary_key.as_deref(), Some(&["kind".to_string()][..]));
    }

    #[test]
    fn create_table_accepts_boolean_bare_column_check_body() {
        // `)` を境界トークンとして扱う `parse_check_body`（`parse_where` との
        // 唯一の違い）が、裸の BOOLEAN 列参照を式フォールバックへ誤って
        // 落とさないことを固定する。
        let v = parse_create_table_ok("CREATE TABLE t (flag TEXT, CHECK (flag))");
        assert!(matches!(
            v.checks[0].predicates.as_slice(),
            [WherePredicate::BoolColumn { column }] if column == "flag"
        ));
    }

    /// 列名 `check`／`constraint` は予約語として `42601` で拒否する（PR
    /// レビュー指摘: 列リスト要素先頭の `CONSTRAINT`／`CHECK (` を制約宣言と
    /// 曖昧さなく解釈するため）。
    #[test]
    fn create_table_rejects_check_and_constraint_as_column_names() {
        for sql in [
            "CREATE TABLE t (check TEXT)",
            "CREATE TABLE t (constraint TEXT)",
            "CREATE TABLE t (body TEXT, Constraint TEXT)",
            "CREATE TABLE t (CHECK VECTOR(3))",
        ] {
            assert_eq!(parse_create_table_err(sql).wire_code(), "42601", "{sql}");
        }
    }

    /// 回帰: 列 `constraint`（型 `TEXT`・列制約 `CHECK`）とも、制約名 `TEXT` の
    /// 表制約とも読める入力を、黙って「制約名 TEXT の表制約」として受理し列
    /// `constraint` を消す誤パースをしない（どちらの解釈でも `42601`）。
    #[test]
    fn create_table_rejects_ambiguous_constraint_column_with_check() {
        for sql in [
            "CREATE TABLE t (constraint TEXT CHECK (body = 'a'), body TEXT)",
            "CREATE TABLE t (body TEXT, constraint TEXT CHECK (body = 'a'))",
            "CREATE TABLE t (body TEXT, CONSTRAINT vector CHECK (body = 'a'))",
            "CREATE TABLE t (body TEXT CONSTRAINT Text CHECK (body = 'a'))",
        ] {
            assert_eq!(parse_create_table_err(sql).wire_code(), "42601", "{sql}");
        }
    }

    /// 回帰（PR #1055 Bugbot 指摘）: 表制約だけで列定義を持たない列リストは
    /// 構文段で `42601` として拒否する。
    #[test]
    fn create_table_rejects_column_less_list_with_only_table_constraints() {
        for sql in [
            "CREATE TABLE t (CHECK (id > 0))",
            "CREATE TABLE t (CONSTRAINT c1 CHECK (body = 'a'))",
            "CREATE TABLE t (CHECK (a = 'x'), CHECK (b = 'y'))",
            "CREATE TABLE t (UNIQUE (a))",
            "CREATE TABLE t (PRIMARY KEY (a))",
        ] {
            assert_eq!(parse_create_table_err(sql).wire_code(), "42601", "{sql}");
        }
    }

    #[test]
    fn create_table_rejects_constraint_without_check() {
        for sql in [
            "CREATE TABLE t (body TEXT, CONSTRAINT c1 UNIQUE (body))",
            "CREATE TABLE t (body TEXT CONSTRAINT c1 NOT NULL)",
            "CREATE TABLE t (body TEXT, CONSTRAINT)",
            "CREATE TABLE t (body TEXT, CHECK body = 'a')",
        ] {
            assert_eq!(parse_create_table_err(sql).wire_code(), "42601", "{sql}");
        }
    }

    #[test]
    fn create_table_rejects_too_many_check_constraints() {
        let limit = crate::catalog::MAX_CHECK_CONSTRAINTS_PER_TABLE;
        let table_level: Vec<String> = (0..=limit)
            .map(|i| format!("CONSTRAINT c{i} CHECK (kind = 'a')"))
            .collect();
        let column_level: Vec<String> = (0..=limit)
            .map(|i| format!("CONSTRAINT c{i} CHECK (kind = 'a')"))
            .collect();
        for sql in [
            format!("CREATE TABLE t (kind TEXT, {})", table_level.join(", ")),
            format!("CREATE TABLE t (kind TEXT {})", column_level.join(" ")),
        ] {
            assert!(
                matches!(
                    parse_create_table_err(&sql),
                    SqlSurfaceError::PayloadTooLarge { .. }
                ),
                "{sql}"
            );
        }
        // ちょうど上限数は受理する。
        let exact: Vec<String> = (0..limit)
            .map(|i| format!("CONSTRAINT c{i} CHECK (kind = 'a')"))
            .collect();
        let v = parse_create_table_ok(&format!("CREATE TABLE t (kind TEXT, {})", exact.join(", ")));
        assert_eq!(v.checks.len(), limit);
    }

    /// 表制約 `CHECK` は列を追加しないため、列数上限の判定（位置非依存）に
    /// 影響しない（ちょうど上限数の列の前後に CHECK があっても受理する）。
    #[test]
    fn create_table_check_does_not_affect_column_limit() {
        let cols: Vec<String> = (0..MAX_CREATE_TABLE_COLUMNS)
            .map(|i| format!("c{i} TEXT"))
            .collect();
        let sql = format!(
            "CREATE TABLE t (CHECK (c0 = 'a'), {}, CHECK (c1 = 'b'))",
            cols.join(", ")
        );
        let v = parse_create_table_ok(&sql);
        assert_eq!(v.columns.len(), MAX_CREATE_TABLE_COLUMNS);
        assert_eq!(v.checks.len(), 2);
    }

    // --- 非再帰 CTE（`WITH` 句。SQL-29 (b)・RLS-10 (b)、TASK-213、Issue #928） ---

    #[test]
    fn accepts_single_cte() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "WITH x AS (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
            &lookup,
        );
        assert_eq!(scan.table_name(), "documents");
        assert_eq!(scan.limit(), 10);
    }

    #[test]
    fn accepts_cte_chain() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "WITH a AS (SELECT id FROM documents), b AS (SELECT id FROM a) SELECT * FROM b LIMIT 10",
            &lookup,
        );
        assert_eq!(scan.table_name(), "documents");
    }

    #[test]
    fn accepts_cte_referencing_view() {
        struct ViewCatalog;
        impl TableLookup for ViewCatalog {
            fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
                Ok(name == "documents")
            }
            fn view_definition(
                &self,
                name: &str,
            ) -> Result<Option<crate::catalog::ViewDef>, SqlSurfaceError> {
                if name == "docs_view" {
                    Ok(Some(crate::catalog::ViewDef {
                        base_relation: "documents".to_string(),
                        body_sql: "SELECT id FROM documents".to_string(),
                    }))
                } else {
                    Ok(None)
                }
            }
        }
        let scan = expect_scan(
            "WITH x AS (SELECT id FROM docs_view) SELECT * FROM x LIMIT 10",
            &ViewCatalog,
        );
        assert_eq!(scan.table_name(), "documents");
    }

    #[test]
    fn accepts_cte_composed_with_main_query_where() {
        // CTE 本文が `SELECT *`（列を絞り込まない）であれば、主クエリの
        // `WHERE` は CTE 本文が公開しない列という制約を受けない。
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "WITH x AS (SELECT * FROM documents WHERE lang = 'ja') SELECT * FROM x WHERE flag LIMIT 10",
            &lookup,
        );
        assert_eq!(
            scan.where_predicates(),
            &[
                WherePredicate::Equality {
                    column: "lang".to_string(),
                    value: "ja".to_string(),
                },
                WherePredicate::BoolColumn {
                    column: "flag".to_string(),
                },
            ]
        );
    }

    #[test]
    fn accepts_select_star_over_column_restricted_cte() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "WITH x AS (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
            &lookup,
        );
        assert_eq!(
            scan.projection(),
            &Projection::Columns(vec!["id".to_string()])
        );
    }

    #[test]
    fn accepts_unreferenced_cte() {
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "WITH unused AS (SELECT id FROM documents) SELECT * FROM documents LIMIT 10",
            &lookup,
        );
        assert_eq!(scan.table_name(), "documents");
    }

    #[test]
    fn cte_name_hides_real_table_of_the_same_name() {
        // `documents` という名前の CTE を定義すると、主クエリの `FROM
        // documents` は実テーブルではなく CTE を指す（PostgreSQL と同じ
        // 名前解決の意味論）。CTE 本文は実テーブルを参照する。
        let lookup = catalog_with(&["documents"]);
        let scan = expect_scan(
            "WITH documents AS (SELECT id FROM documents WHERE lang = 'ja') SELECT * FROM documents LIMIT 10",
            &lookup,
        );
        assert_eq!(scan.table_name(), "documents");
        assert_eq!(
            scan.where_predicates(),
            &[WherePredicate::Equality {
                column: "lang".to_string(),
                value: "ja".to_string(),
            }]
        );
    }

    #[test]
    fn accepts_cte_definition_count_at_limit_rejects_over_limit() {
        let lookup = catalog_with(&["documents"]);
        let defs_at_limit: Vec<String> = (0..crate::sql::cte::MAX_CTE_DEFINITIONS)
            .map(|i| format!("c{i} AS (SELECT id FROM documents)"))
            .collect();
        let sql_ok = format!(
            "WITH {} SELECT * FROM c0 LIMIT 10",
            defs_at_limit.join(", ")
        );
        expect_scan(&sql_ok, &lookup);

        let defs_over_limit: Vec<String> = (0..=crate::sql::cte::MAX_CTE_DEFINITIONS)
            .map(|i| format!("c{i} AS (SELECT id FROM documents)"))
            .collect();
        let sql_over = format!(
            "WITH {} SELECT * FROM c0 LIMIT 10",
            defs_over_limit.join(", ")
        );
        let err = validate_sql(&sql_over, &lookup).expect_err("must exceed CTE definition limit");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn accepts_many_definitions_referencing_a_shared_chain_within_documented_limits() {
        // Issue #928 レビュー指摘の回帰テスト: 定義数（<= MAX_CTE_DEFINITIONS =
        // 16）・連鎖の深さ（主クエリが参照する d11 の解決は d11→c2→c1→c0→
        // documents の 4 回のリレー、すなわち MAX_CTE_NESTING_DEPTH = 4 の
        // 境界ちょうど）のどちらも文書化された上限内に収まる有効なクエリが、
        // 事前検証ループでの参照回数の誤積算により `54000`（"CTE reference
        // count exceeds limit"）へ誤って拒否されないことを確認する。
        // c0..c2 の 3 段連鎖に加え、末尾の c2 を直接参照する d0..d11 の
        // 12 定義（計 15 定義）を用意する。事前検証ループは各 d_i の FROM を
        // 独立に解決し（c2→c1→c0 の 3 回の CTE 名マッチ）、ResolveBudget が
        // 文全体で 1 つに共有されていた旧実装では d_i 12 件分だけで 36 回
        // （> MAX_CTE_REFERENCES = 32）を消費し誤って拒否されていた。
        let lookup = catalog_with(&["documents"]);
        let mut defs = vec!["c0 AS (SELECT id FROM documents)".to_string()];
        for i in 1..3 {
            defs.push(format!("c{i} AS (SELECT id FROM c{})", i - 1));
        }
        for i in 0..12 {
            defs.push(format!("d{i} AS (SELECT id FROM c2)"));
        }
        assert_eq!(defs.len(), 15);
        let sql = format!("WITH {} SELECT * FROM d11 LIMIT 10", defs.join(", "));
        expect_scan(&sql, &lookup);
    }

    #[test]
    fn accepts_cte_nesting_depth_at_limit_rejects_over_limit() {
        let lookup = catalog_with(&["documents"]);
        // `MAX_CTE_NESTING_DEPTH` 件の CTE 連鎖（c0 は基底テーブルを直接参照し、
        // c1..c{depth-1} はそれぞれ 1 つ前を参照する）はちょうど上限内で受理する。
        // 1 件多い連鎖（`depth + 1` 件）は基底テーブル解決の 1 手前で深さが
        // 上限を超え `54000` になる。
        let depth = crate::sql::cte::MAX_CTE_NESTING_DEPTH as usize;
        let mut defs = vec!["c0 AS (SELECT id FROM documents)".to_string()];
        for i in 1..depth {
            defs.push(format!("c{i} AS (SELECT id FROM c{})", i - 1));
        }
        let last = depth - 1;
        let sql_ok = format!("WITH {} SELECT * FROM c{last} LIMIT 10", defs.join(", "));
        expect_scan(&sql_ok, &lookup);

        defs.push(format!("c{depth} AS (SELECT id FROM c{last})"));
        let sql_over = format!("WITH {} SELECT * FROM c{depth} LIMIT 10", defs.join(", "));
        let err = validate_sql(&sql_over, &lookup).expect_err("must exceed CTE nesting depth");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_with_recursive() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "WITH RECURSIVE x AS (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
            &lookup,
        )
        .expect_err("WITH RECURSIVE must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_cte_column_name_list() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "WITH x (a) AS (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
            &lookup,
        )
        .expect_err("CTE column name list must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_cte_materialized_hint() {
        let lookup = catalog_with(&["documents"]);
        for sql in [
            "WITH x AS MATERIALIZED (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
            "WITH x AS NOT MATERIALIZED (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
        ] {
            let err = validate_sql(sql, &lookup).expect_err("MATERIALIZED hint must be rejected");
            assert_eq!(err.wire_code(), "42601", "sql={sql}");
        }
    }

    #[test]
    fn rejects_duplicate_cte_name() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "WITH x AS (SELECT id FROM documents), x AS (SELECT id FROM documents) SELECT * FROM x LIMIT 10",
            &lookup,
        )
        .expect_err("duplicate CTE name must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_cte_body_with_order_by_limit_aggregate_or_semicolon() {
        let lookup = catalog_with(&["documents"]);
        for sql in [
            "WITH x AS (SELECT id FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5) SELECT * FROM x LIMIT 10",
            "WITH x AS (SELECT id FROM documents LIMIT 5) SELECT * FROM x LIMIT 10",
            "WITH x AS (SELECT COUNT(*) FROM documents) SELECT * FROM x LIMIT 10",
            "WITH x AS (SELECT id FROM documents;) SELECT * FROM x LIMIT 10",
        ] {
            let err = validate_sql(sql, &lookup).expect_err("must be rejected");
            assert_eq!(err.wire_code(), "42601", "sql={sql}");
        }
    }

    #[test]
    fn rejects_main_query_ranked_or_aggregate_over_cte() {
        let lookup = catalog_with(&["documents"]);
        for sql in [
            "WITH x AS (SELECT id FROM documents) SELECT * FROM x ORDER BY embedding <=> '[0.1]' LIMIT 5",
            "WITH x AS (SELECT id FROM documents) SELECT COUNT(*) FROM x",
        ] {
            let err = validate_sql(sql, &lookup)
                .expect_err("ranked or aggregate main query over CTE must be rejected");
            assert_eq!(err.wire_code(), "42601", "sql={sql}");
        }
    }

    #[test]
    fn rejects_data_modifying_cte() {
        let lookup = catalog_with(&["documents"]);
        for sql in [
            "WITH x AS (DELETE FROM documents) SELECT * FROM x LIMIT 10",
            "WITH x AS (SELECT id FROM documents) INSERT INTO documents (id) VALUES ('1')",
        ] {
            let err = validate_sql(sql, &lookup).expect_err("data-modifying CTE must be rejected");
            assert_eq!(err.wire_code(), "42601", "sql={sql}");
        }
    }

    #[test]
    fn rejects_cte_referencing_undefined_relation() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "WITH x AS (SELECT id FROM ghost) SELECT * FROM x LIMIT 10",
            &lookup,
        )
        .expect_err("undefined relation in CTE body must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn rejects_out_of_scope_column_over_cte() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "WITH x AS (SELECT id FROM documents) SELECT * FROM x WHERE body = 'y' LIMIT 10",
            &lookup,
        )
        .expect_err("column outside CTE's exposed set must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_unreferenced_cte_projecting_column_outside_preceding_ctes_exposed_set() {
        // Issue #928 レビュー指摘の回帰テスト（Codex P1・Cursor Bugbot Low、
        // 同一欠陥）: 先行 CTE `x` が `id` のみを公開していても、後続 CTE
        // `unused` がどこからも参照されない（`compose` を通らない）ことを
        // 悪用して非公開列を射影・条件に使う文を通してはならない。事前検証
        // ループは各定義の FROM 解決結果に対して定義自身の射影・`WHERE` も
        // 検証すること（`check_columns_within_view` 相当）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "WITH x AS (SELECT id FROM documents), unused AS (SELECT body FROM x) SELECT * FROM documents LIMIT 10",
            &lookup,
        )
        .expect_err("unreferenced CTE projecting a column outside its FROM's exposed set must be rejected");
        assert_eq!(err.wire_code(), "22000");

        let err = validate_sql(
            "WITH x AS (SELECT id FROM documents), unused AS (SELECT id FROM x WHERE body = 'y') SELECT * FROM documents LIMIT 10",
            &lookup,
        )
        .expect_err("unreferenced CTE filtering on a column outside its FROM's exposed set must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    /// `table_columns` を実装するフェイク（PR #1100 追加レビュー指摘の
    /// 回帰テスト専用）。既存の `FakeCatalog` は `table_columns` 未実装
    /// （既定実装 `Ok(None)` のまま）で、それらのテストは本回帰の対象外
    /// （後方互換の確認を兼ねる）。
    struct SchemaCatalog {
        tables: std::collections::HashMap<&'static str, &'static [&'static str]>,
    }

    impl TableLookup for SchemaCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(self.tables.contains_key(name))
        }
        fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
            // `catalog::Storage` の実装契約（実カラム ＋ 未宣言なら `id` 疑似列）
            // を模す（`TableLookup::table_columns` のドキュメント参照）。
            Ok(self.tables.get(name).map(|cols| {
                let mut columns: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
                if !columns.iter().any(|c| c == "id") {
                    columns.push("id".to_string());
                }
                columns
            }))
        }
    }

    #[test]
    fn rejects_unreferenced_cte_projecting_unknown_real_table_column() {
        // PR #1100 追加レビュー指摘の回帰テスト（Codex P1・Cursor Bugbot Low、
        // 同一欠陥）: 事前検証ループは `Resolved::Table`（FROM が実テーブル）の
        // 場合に公開列集合を `None` のまま `check_columns_within_view` へ渡して
        // いたため、実テーブルに存在しない列を射影・条件に使う未参照 CTE も
        // 検査をすり抜けて受理していた。`TableLookup::table_columns` で実テーブル
        // のスキーマへ問い合わせて検証すること。
        let lookup = SchemaCatalog {
            tables: [("documents", ["id", "body"].as_slice())].into(),
        };
        let err = validate_sql(
            "WITH unused AS (SELECT missing FROM documents) SELECT id FROM documents LIMIT 1",
            &lookup,
        )
        .expect_err("unreferenced CTE projecting an unknown real table column must be rejected");
        assert_eq!(err.wire_code(), "22000");

        let err = validate_sql(
            "WITH unused AS (SELECT id FROM documents WHERE missing = 'y') SELECT id FROM documents LIMIT 1",
            &lookup,
        )
        .expect_err("unreferenced CTE filtering on an unknown real table column must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn accepts_unreferenced_cte_projecting_id_pseudo_column_over_real_table() {
        // 対照確認（regression guard）: `table_columns` は実カラムに加えて
        // 疑似列 `id` も許可列へ含めること（`sql::parser::bind_projection` が
        // `id` を疑似列として受理する契約と一致させる）。スキーマが `id` という
        // 実カラムを宣言していない場合の対照。
        let lookup = SchemaCatalog {
            tables: [("documents", ["body"].as_slice())].into(),
        };
        expect_scan(
            "WITH unused AS (SELECT id FROM documents) SELECT * FROM documents LIMIT 1",
            &lookup,
        );
    }

    #[test]
    fn accepts_with_statement_containing_literal_where_param_placeholder_check_is_deferred() {
        // TASK-213: `$n` を含む WITH 文は `sql::params::validate_param_positions`
        // が拒否する（本テストは構造検証自体〔`validate_sql`〕が受理し得ることを
        // 確認するのみで、`$n` 拒否は `sql::params` の単体テストが担う）。
        let lookup = catalog_with(&["documents"]);
        expect_scan(
            "WITH x AS (SELECT id FROM documents) SELECT * FROM x WHERE id = 'lit' LIMIT 10",
            &lookup,
        );
    }
}
