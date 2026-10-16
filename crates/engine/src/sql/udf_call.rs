//! 宣言的 UDF 呼び出しの式層（TASK-79、対象ビヘイビア: SQL-9。ポインタ:
//! `docs/spec/05-tasks.md` TASK-79・`docs/spec/04-behavior/sql-surface.md` SQL-9）。
//!
//! 責務境界: `CREATE FUNCTION <name>(<params>) AS <expr>` で定義した宣言的 UDF を、
//! `SELECT` の結果列・`WHERE` 条件のいずれの位置からも単一文で呼び出せるようにする
//! ための式 AST（[`Expr`]、`allowlist::Parser` が構築する構文情報のみを持つ）、
//! セッション単位のレジストリ（[`UdfRegistry`]）、意味論的な束縛・インライン展開
//! （[`bind_expr`]）、行コンテキストでの評価（[`eval`]）を提供する。
//!
//! - 構文（`Expr`）は `sql::allowlist::Parser` が組み立て、列名・関数名の意味論的な
//!   妥当性は検証しない（許可リスト層の既存分業を踏襲）。
//! - 束縛（[`bind_expr`]）は `sql::parser` の束縛段から呼ばれ、列参照の解決・
//!   関数名の解決（組み込み／登録済み UDF）・静的型検査・UDF 本体のインライン展開を
//!   行う。展開後は [`BoundExpr`]（レジストリを参照しない自己完結した木）になる。
//! - 評価（[`eval`]）は `sql::exec` の RLS→SCALAR 段のフック（結果列・`WHERE` の
//!   両方から呼ばれる）・投影段から呼ばれる。可視行（RLS-8 の暗黙適用を通過した行）
//!   にしか到達しない前提を [`eval`] 自体は検査しない（呼び出し元の契約）。
//!
//! untrusted な SQL 入力を扱うため `unwrap`/`expect`/添字アクセス `[]` を使わない
//! （`.claude/rules/coding-rust.md`）。0 除算・非有限値（NaN/∞）の生成は行単位で
//! fail-closed に拒否し、黙って 0 や NULL へ丸めない（security.md「不安全な設計」）。

use std::borrow::Cow;
use std::sync::Arc;

use crate::catalog;
use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::datetime_fn::{self, DatePartField, DateTruncUnit};
use crate::sql::string_fn;
use crate::sql::time_fn::{self, StatementClock};
use crate::wasm_udf::WasmUdfBackend;

/// UDF 定義が持てるパラメータ数の上限（`54000` で拒否）。
pub const MAX_UDF_PARAMS: usize = 32;
/// 関数呼び出し（組み込み・UDF 問わず）1 回あたりの引数数上限（`54000`）。
pub const MAX_CALL_ARGS: usize = 32;
/// 式の構文解析時の再帰深さ上限（スタック消費の上限。`54000`）。
pub const MAX_EXPR_DEPTH: usize = 32;
/// UDF インライン展開後の式ノード数上限（多段呼び出しによる指数的膨張への歯止め）。
/// `sql::allowlist::Parser` はこれと同一の値を構文解析時のノード数予算
/// （`Parser::expr_node_budget`）としても共有し、左結合ループが `MAX_EXPR_DEPTH`
/// をすり抜けて木を積み続ける入力（"1+1+...+1" 等）を頭打ちにする（`54000`）。
pub const MAX_EXPR_NODES: usize = 1024;
/// `CASE` 式 1 個が持てる `WHEN` 分岐数の上限（対象ビヘイビア: SQL-26。Issue #921）。
/// `sql::allowlist::Parser::parse_case_expr` が構文段で、[`bind_case`] が
/// 束縛段（UDF インライン展開後）で独立に検査する（`54000`）。
pub const MAX_CASE_BRANCHES: usize = 64;
/// `CASE`／`COALESCE`／`NULLIF` の入れ子段数上限（対象ビヘイビア: SQL-26。
/// Issue #921）。構文段（`sql::allowlist::Parser`）では字面上のネストを、
/// 束縛段（[`BindEnv::case_nesting`]）では UDF 本体インライン展開後の実際の
/// ネストを、それぞれ独立に検査する（構文段の計測値だけでは UDF 呼び出しの
/// 展開によって実際のネストがすり抜けうるため。`54000`）。
pub const MAX_CASE_NESTING: usize = 8;
/// セッションが保持できる UDF 定義数上限（宣言的・WASM 合算。`54000`）。
pub const MAX_SESSION_UDFS: usize = 64;
/// WASM UDF 呼び出しの引数数（TASK-149。ABI 固定シグネチャ
/// `(Vector, Scalar) -> Scalar` のため常に 2）。
const WASM_CALL_ARITY: usize = 2;
/// `f64` の 52 bit 仮数部で整数値を正確に表現できる上限（`2^53`）。行 `id`
/// （[`id_as_finite_scalar`]）・整数の数値リテラル（[`bind_expr_in`] の
/// `Expr::Number` 束縛）の双方で同一の正確表現境界として共有する。
const MAX_EXACT_F64_INT: u64 = 1u64 << 53;

/// 数値リテラルの生文字列（符号なし。`-` は呼び出し元が別トークンとして処理する）を
/// `f64` へ束縛する（TASK-79・SQL-9 の `Expr::Number` 束縛から TASK-167・SQL-14
/// （`sql::group_by` の HAVING/LIMIT リテラル束縛）が共有できるよう切り出した）。
/// 整数リテラル（桁のみ、または `NUMERIC` 列の受理文法に合わせた末尾ドット付き
/// 整数〔`1.` 形〕。いずれも数学的には整数）は `f64::from_str` が黙って最近接値へ
/// 丸めうる（`raw.parse::<f64>()` はエラーにならない）。`2^53` を超える整数は
/// `f64` で正確に表現できないという境界を、丸め変換の *前* に整数として検査する
/// ことで、大きな整数リテラルが精度欠落によって別の値と黙って同一視されるのを
/// 防ぐ（fail-closed。security.md「不安全な設計」対応）。
pub(crate) fn parse_number_literal(raw: &str) -> Result<f64, SqlSurfaceError> {
    // `sql::lexer::lex_number`（Issue #885・D5）は `NUMERIC` 列の受理文法に
    // 合わせ、末尾ドット付き整数（`1.` 形。小数部が空）も 1 トークンとして
    // 生成するようになった。この形は数学的には整数だが、桁だけを見る
    // `bytes().all(is_ascii_digit)` 判定はドットの分だけ弾いてしまい、
    // 2^53 を超える値（`9007199254740993.` 等）が exactness ガードを
    // 素通りして下の `raw.parse::<f64>()` で無音に丸められる（Cursor Bugbot
    // 指摘・PR #1020）。末尾ドットを取り除いた残りが 1 桁以上の数字のみで
    // あれば、同じ整数として exactness 判定の対象に含める。
    // Issue #1183: NoSQL の `filter`（JSON の `NegInt`）は負数の生テキスト
    // （`-9007199254740993` 等）をそのまま渡すため、先頭の `-` を除いた絶対値にも
    // 同じ exactness 判定を適用する（負の数値リテラルは構文段で `-n` の文字列へ畳まれて渡る。Issue #1430）。
    let abs_raw = match raw.strip_prefix('-') {
        Some(unsigned) if !unsigned.is_empty() => unsigned,
        _ => raw,
    };
    let integer_digits: Option<&str> = if !abs_raw.is_empty()
        && abs_raw.bytes().all(|b| b.is_ascii_digit())
    {
        Some(abs_raw)
    } else if let Some(stripped) = abs_raw.strip_suffix('.') {
        (!stripped.is_empty() && stripped.bytes().all(|b| b.is_ascii_digit())).then_some(stripped)
    } else {
        None
    };
    if let Some(digits) = integer_digits {
        let as_int: u64 = digits.parse().map_err(|_| {
            // 桁数が多すぎて `u64` にも収まらない（`u64::MAX` 超）場合も、
            // `f64` で正確に表現できないことに変わりはない。
            SqlSurfaceError::numeric_out_of_range(
                "integer literal exceeds the range that can be exactly represented",
            )
        })?;
        if as_int > MAX_EXACT_F64_INT {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "integer literal exceeds the range that can be exactly represented",
            ));
        }
    }
    // 小数表記（`9007199254740993.0`）・指数表記（`9.007199254740993e15`）でも、値が
    // 数学的に整数なら同じ exactness 判定を丸め変換の前に適用する（NoSQL `filter` が
    // JSON の生数値を渡すため到達する。Issue #1183・codex-review P1）。
    if integral_decimal_exceeds_exact_f64(abs_raw) {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "integer literal exceeds the range that can be exactly represented",
        ));
    }
    let v: f64 = raw
        .parse()
        .map_err(|_| SqlSurfaceError::unsupported(format!("malformed number: {raw}")))?;
    if !v.is_finite() {
        return Err(SqlSurfaceError::invalid_input(
            "numeric literal is not finite",
        ));
    }
    Ok(v)
}

/// 符号なしの 10 進表記（`d+[.d*][e[+-]d+]`）が数学的に整数値で、かつ `2^53` を超える
/// か（`f64` で正確に表現できないか）を、`f64` への丸め変換を介さず判定する。
/// 非整数値（小数部が 0 でない）・不正形式は `false`（後段の通常経路に任せる）。
/// 整数の字面・末尾ドット形は [`parse_number_literal`] が別途検査済みだが、
/// 本関数はそれらも同じ結果を返す。
fn integral_decimal_exceeds_exact_f64(abs_raw: &str) -> bool {
    let (mantissa, exp_part) = match abs_raw.find(['e', 'E']) {
        Some(i) => (&abs_raw[..i], Some(&abs_raw[i + 1..])),
        None => (abs_raw, None),
    };
    let exp: i64 = match exp_part {
        None => 0,
        Some(e) => match e.parse() {
            Ok(v) => v,
            // 符号付き 10 進数字列だが i64 に収まらない指数は表現範囲外として拒否側へ倒す
            // （fail-closed）。それ以外の不正形式は後段の通常経路（malformed）に任せる。
            Err(_) => {
                let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
                return !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
            }
        },
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return false;
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let digits: String = int_part.chars().chain(frac_part.chars()).collect();
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return false; // 0
    }
    let trimmed = trimmed.trim_end_matches('0');
    let trailing_zeros = (digits.trim_start_matches('0').len() - trimmed.len()) as i64;
    // 値 = trimmed × 10^exp10（trimmed は末尾 0 なし）。
    // 指数は未信頼入力（NoSQL `filter` の JSON 数値）由来のため、加減算はすべて
    // checked で行い、i64 に収まらない指数は表現範囲外として拒否側（`true`）へ倒す
    // （panic・折り返しによる誤判定を作らない。fail-closed）。
    let Some(exp10) = i64::try_from(frac_part.len())
        .ok()
        .and_then(|frac_len| exp.checked_sub(frac_len))
        .and_then(|e| e.checked_add(trailing_zeros))
    else {
        return true;
    };
    if exp10 < 0 {
        return false; // 小数部が 0 でない（非整数）。
    }
    let Some(total_digits) = i64::try_from(trimmed.len())
        .ok()
        .and_then(|len| len.checked_add(exp10))
    else {
        return true;
    };
    if total_digits > 20 {
        return true; // `u64` 上限（20 桁）超は 2^53 を確実に超える。
    }
    let mut value: u128 = match trimmed.parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    for _ in 0..exp10 {
        value = match value.checked_mul(10) {
            Some(v) => v,
            None => return true,
        };
    }
    value > u128::from(MAX_EXACT_F64_INT)
}

/// 式の二項演算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
}

/// 構文段の式 AST（`allowlist::Parser` が構築する。列名・関数名の意味論的妥当性は
/// 未検証）。`CREATE FUNCTION` の本体・`SELECT` の式項目・`WHERE` の式述語が共通で使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// 数値リテラル（`lexer::Token::Number` の生文字列。小数を許容）。
    Number(String),
    /// 文字列リテラル（Issue #919・SQL-26。値は字句段でクォート解除済み）。
    /// 単独では TEXT 型として束縛する。比較演算子（`> < >= <= =`）の片側だけが
    /// 文字列リテラルで、反対側が数値のときに限り、PostgreSQL の unknown 型リテラルと
    /// 同様に反対側の型として解釈する（Issue #1408。`bind_unknown_literal_against`）。
    String(String),
    /// 識別子（列参照・UDF パラメータ参照のいずれかは束縛段で解決する）。
    Ident(String),
    /// 関数呼び出し（組み込みまたは登録済み UDF のいずれかは束縛段で解決する）。
    Call { name: String, args: Vec<Expr> },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `NULL` リテラル（対象ビヘイビア: SQL-26。Issue #921）。束縛段
    /// （[`bind_expr_in`]）は `CASE` の THEN／ELSE・`COALESCE`／`NULLIF` の
    /// 引数の 3 箇所でのみこの variant を受理し、型を兄弟枝から単一化する。
    /// それ以外の位置（`id + NULL` 等）は型を決められない形として
    /// `FeatureNotSupported`（`0A000`）で拒否する。
    Null,
    /// 検索形 `CASE WHEN <cond> THEN <result> {WHEN ...} [ELSE <result>] END`
    /// （対象ビヘイビア: SQL-26）。単純 CASE（`CASE x WHEN v ...`）・`cond` 中の
    /// 論理演算（`AND`/`OR`/`IS NULL` 等）は許可リスト（`sql::allowlist::Parser`）が
    /// 構文段で拒否するため、`cond` は常に比較の [`Expr::Binary`] になる。
    Case {
        whens: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    /// `COALESCE(<arg>, ...)`（対象ビヘイビア: SQL-26）。最初の非 NULL 引数を返す。
    Coalesce(Vec<Expr>),
    /// `NULLIF(<lhs>, <rhs>)`（対象ビヘイビア: SQL-26）。両辺が等しければ NULL、
    /// 異なれば `lhs` を返す。
    NullIf(Box<Expr>, Box<Expr>),
    /// `DATE '<literal>'`（型付きリテラル。対象ビヘイビア: SQL-26。Issue #920）。
    /// 構文段（`allowlist::Parser::parse_primary_expr`）で
    /// `crate::datetime::parse_date` を即座に解析済みの値を保持する（生文字列を
    /// 保持しないのは、CHECK・ビューの render→再パース往復で区切り文字の表記
    /// 揺れ〔`T` 区切り／空白区切り〕が食い違って壊れるのを避けるため）。
    DateLiteral(i32),
    /// `TIMESTAMP '<literal>'`（[`Expr::DateLiteral`] と同じ理由。値は
    /// `crate::datetime::parse_timestamp` の解析結果＝マイクロ秒）。
    TimestampLiteral(i64),
    /// `TIME '<literal>'`（[`Expr::DateLiteral`] と同じ理由。値は
    /// `crate::datetime::parse_time` の解析結果＝0 時起点のマイクロ秒。Issue #1570）。
    TimeLiteral(i64),
    /// jsonb の演算子（パス取得 `->` `->>` `#>` `#>>`・包含 `@>` `<@`・存在 `?` `?|` `?&`。
    /// 対象ビヘイビア: TABLE-25。Issue #1596・#1598）。構文段（`allowlist::Parser::parse_json_access_expr`）が
    /// PostgreSQL と同じ優先順位（加減算より弱く比較より強い）・左結合で生成する。
    /// [`BinOp`] とは別 variant にして、索引計画・宣言的述語・CHECK 等が
    /// 算術・比較の網羅 match へ誤って流し込まないようにする。
    JsonAccess {
        op: JsonAccessOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// 括弧なしの SQL 値関数（`CURRENT_TIMESTAMP`・`CURRENT_DATE`・`LOCALTIMESTAMP` 等。
    /// 対象ビヘイビア: SQL-37・Issue #1586）。構文段（`allowlist::Parser::parse_primary_expr`）が
    /// **引用されていない**識別子の予約語一致でだけ生成する（引用識別子は列参照のまま）。
    /// `precision` は `CURRENT_TIMESTAMP(p)` の小数秒桁数（括弧内が整数リテラル 1 個のとき）。
    /// 関数形の `now()` 等は従来どおり [`Expr::Call`]。
    ValueFunction {
        kind: ValueFunctionKind,
        precision: Option<u8>,
    },
    /// 式の照合指定 `<expr> COLLATE "<名前>"`（対象ビヘイビア: TABLE-28。Issue #1609）。
    /// 構文段（`allowlist::Parser`）が照合名を許可リスト（[`crate::collation::Collation`]）で
    /// 解決済みで、未知の名前は `42704` で拒否されている。Issue #1609 では `C`／`POSIX`
    /// （どちらも既定のバイト列順と同値）だけを受理するため、束縛後の意味は内側の式と同じ
    /// （束縛段は内側が文字列系であることだけを検査する）。比較・ソートへの照合の伝播と
    /// ICU 照合は #1610・#2190 が担う。
    Collate {
        expr: Box<Expr>,
        collation: crate::collation::Collation,
    },
}

/// 括弧なし SQL 値関数の種別（[`Expr::ValueFunction`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFunctionKind {
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    LocalTimestamp,
    LocalTime,
}

impl ValueFunctionKind {
    /// 予約語（大文字小文字不問）から種別を引く。
    pub fn from_keyword(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "current_timestamp" => Some(Self::CurrentTimestamp),
            "current_date" => Some(Self::CurrentDate),
            "current_time" => Some(Self::CurrentTime),
            "localtimestamp" => Some(Self::LocalTimestamp),
            "localtime" => Some(Self::LocalTime),
            _ => None,
        }
    }

    /// 小文字のキーワード表記（結果列名・CHECK／ビューの render 用。PostgreSQL の既定列名と同じ）。
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::CurrentTimestamp => "current_timestamp",
            Self::CurrentDate => "current_date",
            Self::CurrentTime => "current_time",
            Self::LocalTimestamp => "localtimestamp",
            Self::LocalTime => "localtime",
        }
    }

    /// 精度指定（`(p)`）を取りうる種別か（`CURRENT_DATE` は取らない）。
    pub fn accepts_precision(self) -> bool {
        !matches!(self, Self::CurrentDate)
    }

    /// `content_hash` 用の安定した 1 バイト識別子（変更禁止）。
    pub(crate) fn hash_tag(self) -> u8 {
        match self {
            Self::CurrentTimestamp => 1,
            Self::CurrentDate => 2,
            Self::CurrentTime => 3,
            Self::LocalTimestamp => 4,
            Self::LocalTime => 5,
        }
    }
}

/// jsonb 演算子の種別（[`Expr::JsonAccess`]。パス取得・包含・存在）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonAccessOp {
    /// `->`（右辺はキーまたは添字。結果は jsonb）。
    Get,
    /// `->>`（右辺はキーまたは添字。結果は text）。
    GetText,
    /// `#>`（右辺は text[] リテラル。結果は jsonb）。
    Path,
    /// `#>>`（右辺は text[] リテラル。結果は text）。
    PathText,
    /// `@>`（両辺 jsonb。結果は bool。Issue #1598）。
    Contains,
    /// `<@`（両辺 jsonb。結果は bool。Issue #1598）。
    ContainedBy,
    /// `?`（右辺は text。結果は bool。Issue #1598）。
    Exists,
    /// `?|`（右辺は text[] リテラル。結果は bool。Issue #1598）。
    ExistsAny,
    /// `?&`（右辺は text[] リテラル。結果は bool。Issue #1598）。
    ExistsAll,
    /// `||`（連結・浅いマージ。結果は jsonb。Issue #2181）。
    Concat,
    /// `#-`（右辺は text[] リテラル。結果は jsonb。Issue #2181）。`jsonb - x` は
    /// 加減算の段の [`BinOp::Sub`] のまま構文段が生成し、束縛段が左辺の型で振り分ける。
    DeletePath,
}

impl JsonAccessOp {
    /// 演算子の SQL 表記（CHECK／ビューの render・エラーメッセージ用）。
    pub fn as_sql(self) -> &'static str {
        match self {
            JsonAccessOp::Get => "->",
            JsonAccessOp::GetText => "->>",
            JsonAccessOp::Path => "#>",
            JsonAccessOp::PathText => "#>>",
            JsonAccessOp::Contains => "@>",
            JsonAccessOp::ContainedBy => "<@",
            JsonAccessOp::Exists => "?",
            JsonAccessOp::ExistsAny => "?|",
            JsonAccessOp::ExistsAll => "?&",
            JsonAccessOp::Concat => "||",
            JsonAccessOp::DeletePath => "#-",
        }
    }

    /// bool を返す演算子（包含・存在）かどうか。WHERE の裸の述語として受理できる
    /// （`allowlist::Parser::parse_expression_predicate`）。
    pub fn is_predicate(self) -> bool {
        matches!(
            self,
            JsonAccessOp::Contains
                | JsonAccessOp::ContainedBy
                | JsonAccessOp::Exists
                | JsonAccessOp::ExistsAny
                | JsonAccessOp::ExistsAll
        )
    }
}

/// 束縛済み（列参照・関数呼び出しの解決、UDF 本体のインライン展開が完了した）式。
/// レジストリを参照せずに単独で評価できる（`eval` の入力）。
///
/// `PartialEq` は手動実装する（[`WasmCall`](BoundExpr::WasmCall) が保持する
/// `Arc<dyn WasmUdfBackend>` は `dyn` 型のため構造的な `derive(PartialEq)` を
/// 導出できない。バックエンドの同一性は `Arc::ptr_eq` で判定する）。
#[derive(Debug, Clone)]
pub enum BoundExpr {
    Number(f64),
    /// 文字列リテラル（束縛済み。Issue #919・SQL-26）。
    Text(String),
    /// nullable TEXT 列の参照（Issue #919・SQL-26）。`index` は `schema.columns`
    /// と同じ論理列インデックス（`ScalarRef` を返す既存の走査 API・
    /// `AggregateInput::TextColumn` 等と同じ添字系列）。評価時は呼び出し元が
    /// 渡す行スカラー値（`eval_with_scalars` の `row_scalars`）から解決する。
    /// マスク外参照（呼び出し元がこの列をデコード対象に含め忘れた場合）は
    /// 実 NULL と取り違えず `Internal` として fail-closed に拒否する
    /// （[`eval_with_scalars`] のドキュメント参照）。
    TextColumnRef {
        index: usize,
    },
    /// 疑似列 `id`（行 `id` を `f64` として扱う）。
    IdRef,
    /// 疑似列 `id` と整数 `value` の厳密比較（Issue #1352）。`IdRef` は行 `id` を
    /// `f64` へ写すため `2^53` 超で `22003` になるが、本ノードは `u64` の行 `id` を
    /// `i128` へ無損失拡大して整数のまま比較する（`id` は NULL にならないため結果は
    /// 常に `Bool`）。`sql::subquery` が生成する `WherePredicate::IdCompare` の束縛
    /// 結果としてのみ作られる。`op` は `Eq`／`Lt`／`Le`／`Gt`／`Ge` のみ。
    IdCompare {
        op: BinOp,
        value: i128,
    },
    /// テーブルの `VECTOR` 列参照（1 テーブルにつき高々 1 本、TABLE-1。
    /// `catalog::encode_schema` が内部で呼ぶ `validate_schema` により
    /// `CREATE TABLE`・`ALTER TABLE ADD COLUMN` の双方で fail-closed に強制される
    /// ため列インデックスを保持する必要はないが、束縛側（`bind_expr_in`）でも
    /// 参照先がスキーマ中最初の VECTOR 列と一致することを重ねて検査し、
    /// この不変条件が崩れた場合に誤った列の値で黙って評価しないようにする）。
    VectorRef,
    Builtin {
        f: BuiltinFn,
        args: Vec<BoundExpr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<BoundExpr>,
        rhs: Box<BoundExpr>,
    },
    /// WASM UDF 呼び出し（TASK-149、対象ビヘイビア: EXT-5）。ABI は
    /// `crate::wasm_udf` が固定する `(Vector, Scalar) -> Scalar` の 1 種類のみ
    /// （`args` は常に長さ 2）。`registry` を実行時に参照しないという `BoundExpr`
    /// の設計を維持するため、束縛済みバックエンドを `Arc` で直接保持する。
    WasmCall {
        name: String,
        backend: Arc<dyn WasmUdfBackend>,
        args: Vec<BoundExpr>,
    },
    /// `NULL`（対象ビヘイビア: SQL-26。Issue #921）。[`Expr::Null`] の束縛結果。
    /// [`bind_case`]・[`bind_coalesce`]・[`bind_nullif`] の 3 箇所からのみ生成される。
    Null,
    /// 検索形 `CASE`（対象ビヘイビア: SQL-26）。`else_result` は構文上の `ELSE` 省略時
    /// にも [`BoundExpr::Null`] を補って正規化済みのため常に存在する
    /// （[`bind_case`] 参照）。
    Case {
        whens: Vec<(BoundExpr, BoundExpr)>,
        else_result: Box<BoundExpr>,
    },
    /// `COALESCE`（対象ビヘイビア: SQL-26）。
    Coalesce(Vec<BoundExpr>),
    /// `NULLIF`（対象ビヘイビア: SQL-26）。両辺は常に `Scalar` 型（[`bind_nullif`]
    /// が検査済み）。
    NullIf {
        lhs: Box<BoundExpr>,
        rhs: Box<BoundExpr>,
    },
    /// `DATE` 定数（対象ビヘイビア: SQL-26。Issue #920）。内部表現は
    /// 1970-01-01 起点の日数（`crate::datetime` 参照）。
    Date(i32),
    /// `TIMESTAMP` 定数（マイクロ秒。[`BoundExpr::Date`] 参照）。
    Timestamp(i64),
    /// `TIME` 定数（0 時起点のマイクロ秒。Issue #1570）。
    Time(i64),
    /// nullable `DATE` 列の参照。`index` は `TextColumnRef` と同じ論理列
    /// インデックス系列（`schema.columns` 添字）。値は行スカラー値
    /// （[`eval_with_scalars`] の `row_scalars`）から解決する。
    DateColumnRef {
        index: usize,
    },
    /// nullable `TIMESTAMP` 列の参照（[`BoundExpr::DateColumnRef`] 参照）。
    TimestampColumnRef {
        index: usize,
    },
    /// nullable `TIME` 列の参照（[`BoundExpr::DateColumnRef`] 参照。Issue #1570）。
    TimeColumnRef {
        index: usize,
    },
    /// nullable な数値列（INTEGER/BIGINT/REAL/DOUBLE）の参照（Issue #1075・
    /// TABLE-16 ポインタ）。`index` は `TextColumnRef` と同じ論理列インデックス
    /// 系列（`schema.columns` 添字）。`WHERE`／投影／`CHECK` の
    /// 式束縛（Issue #1183・SQL-24・SQL-26）で共通に生成される。値の変換規則は `numeric_scalar_from_ref` 参照。
    ColumnRef {
        index: usize,
    },
    /// nullable `jsonb` 列の参照（Issue #1596・TABLE-25）。パス演算子の左辺・比較・
    /// ORDER BY（Issue #1597）で生成され、jsonb を受け付けない位置へ流れた参照は
    /// `reject_json_operand` が `0A000` で拒否する。`index` は `TextColumnRef` と同じ論理列
    /// インデックス系列。マスク外参照は実 NULL と取り違えず `Internal` で拒否する。
    JsonbColumnRef {
        index: usize,
    },
    /// nullable `json`（原文保存）列の参照（Issue #2180・TABLE-25）。値は
    /// [`ExprValue::JsonText`]（格納テキストの借用。空白・キー順・重複キーを保持）で、
    /// 消費できるのはパス演算子の左辺・`json_*` 関数の引数・投影だけ（それ以外は
    /// `reject_json_operand`／比較系の `42883` で拒否する）。
    JsonColumnRef {
        index: usize,
    },
    /// `json_*` 関数・`to_json`・`json_build_*` の束縛結果（Issue #2180。束縛・評価は
    /// `sql::json_fn`）。
    JsonTextFn(Box<crate::sql::json_fn::BoundJsonFn>),
    /// jsonb として解釈済みの文字列リテラル（Issue #1597・TABLE-25）。jsonb 式と比較される
    /// 型なし文字列リテラル（PostgreSQL の unknown 型）を [`bind_jsonb_literal`] が
    /// 解析・正規化した結果で、正規化済み JSON テキストを保持する。
    JsonbLiteral(String),
    /// パス取得演算子の束縛結果（Issue #1596・TABLE-25）。`lhs` は jsonb を返す式
    /// （[`BoundExpr::JsonbColumnRef`] または入れ子の `JsonAccess`）。右辺の種別
    /// （キー／添字／パス）は束縛時に確定している。`as_text` が真なら `->>`／`#>>`。
    JsonAccess {
        lhs: Box<BoundExpr>,
        rhs: JsonAccessRhs,
        as_text: bool,
    },
    /// jsonb の包含・存在演算子の束縛結果（Issue #1598・TABLE-25）。結果は bool
    /// （[`ExprType::Bool`]）。`lhs` は jsonb を返す式。`<@` は評価時に左右を入れ替えて
    /// `@>` として判定する。左右いずれかが SQL の NULL なら結果も NULL（strict）。
    JsonPredicate {
        op: JsonPredicateOp,
        lhs: Box<BoundExpr>,
        rhs: JsonPredicateRhs,
    },
    /// jsonb の連結・削除演算子の束縛結果（Issue #2181・TABLE-25）。結果は jsonb
    /// （[`ExprType::Json`]）。左右いずれかが SQL の NULL なら結果も NULL（strict）。
    JsonModify {
        op: JsonModifyOp,
        lhs: Box<BoundExpr>,
        rhs: JsonModifyRhs,
    },
    /// jsonb 関数（`jsonb_typeof`・`jsonb_array_length`・`jsonb_extract_path(_text)`・
    /// `jsonb_build_object`・`jsonb_build_array`・`to_jsonb`。Issue #1599・TABLE-25）の束縛結果。
    /// 引数は多相・可変長・jsonb 値を取るため [`BuiltinFn`] では表せず専用 variant とする。
    /// 引数の型規則は束縛時（[`bind_json_fn`]）に確定済みで、評価（`eval_json_fn`）は値を
    /// 順に評価して関数ごとの NULL 規則に従う。
    JsonFn {
        f: JsonFnKind,
        args: Vec<BoundExpr>,
    },
}

/// [`BoundExpr::JsonModify`] の演算子（Issue #2181）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonModifyOp {
    /// `||`
    Concat,
    /// `jsonb - text`
    DeleteKey,
    /// `jsonb - integer`
    DeleteIndex,
    /// `#-`
    DeletePath,
}

/// [`BoundExpr::JsonModify`] の右辺（束縛時に種別を確定済み）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonModifyRhs {
    /// `||` の jsonb 式。
    Jsonb(Box<BoundExpr>),
    /// `- text` のキー（text を返す式）。
    Key(Box<BoundExpr>),
    /// `- integer` の添字（int4 を返す式）。
    Index(Box<BoundExpr>),
    /// `#-` のパス（束縛時に解析済み。NULL 要素は束縛段で拒否済みのため持たない）。
    Path(Vec<String>),
}

impl JsonModifyRhs {
    /// 右辺が評価する式（`Path` は式を持たない）。列参照・ノード数の走査用。
    fn expr(&self) -> Option<&BoundExpr> {
        match self {
            JsonModifyRhs::Jsonb(e) | JsonModifyRhs::Key(e) | JsonModifyRhs::Index(e) => Some(e),
            JsonModifyRhs::Path(_) => None,
        }
    }
}

/// [`BoundExpr::JsonFn`] の関数種別（Issue #1599）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFnKind {
    /// `jsonb_typeof(jsonb) -> text`
    Typeof,
    /// `jsonb_array_length(jsonb) -> int4`
    ArrayLength,
    /// `jsonb_extract_path(jsonb, text...) -> jsonb`
    ExtractPath,
    /// `jsonb_extract_path_text(jsonb, text...) -> text`
    ExtractPathText,
    /// `jsonb_build_object(any...) -> jsonb`
    BuildObject,
    /// `jsonb_build_array(any...) -> jsonb`
    BuildArray,
    /// `to_jsonb(anyelement) -> jsonb`
    ToJsonb,
}

/// [`BoundExpr::JsonPredicate`] の演算子（Issue #1598）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonPredicateOp {
    /// `@>`
    Contains,
    /// `<@`
    ContainedBy,
    /// `?`
    Exists,
    /// `?|`
    ExistsAny,
    /// `?&`
    ExistsAll,
}

/// [`BoundExpr::JsonPredicate`] の右辺（束縛時に種別を確定済み）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonPredicateRhs {
    /// `@>`／`<@` の jsonb 式。
    Jsonb(Box<BoundExpr>),
    /// `?` のキー（text を返す式）。
    Key(Box<BoundExpr>),
    /// `?|`／`?&` のキー集合（束縛時に解析済みの text[] リテラル。`None` は NULL 要素）。
    Keys(Vec<Option<String>>),
}

impl JsonPredicateRhs {
    /// 右辺が評価する式（`Keys` は式を持たない）。列参照・ノード数の走査用。
    fn expr(&self) -> Option<&BoundExpr> {
        match self {
            JsonPredicateRhs::Jsonb(e) | JsonPredicateRhs::Key(e) => Some(e),
            JsonPredicateRhs::Keys(_) => None,
        }
    }
}

/// [`BoundExpr::JsonAccess`] の右辺（束縛時に種別を確定済み）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonAccessRhs {
    /// `->`／`->>` のキー（text を返す式）。
    Key(Box<BoundExpr>),
    /// `->`／`->>` の添字（int4 を返す式。負値は末尾から数える）。
    Index(Box<BoundExpr>),
    /// `#>`／`#>>` のパス（束縛時に解析・検証済みの text[] リテラル。`None` は NULL 要素）。
    Path(Vec<Option<String>>),
}

impl PartialEq for BoundExpr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (BoundExpr::Number(a), BoundExpr::Number(b)) => a == b,
            (BoundExpr::Text(a), BoundExpr::Text(b)) => a == b,
            (BoundExpr::TextColumnRef { index: a }, BoundExpr::TextColumnRef { index: b }) => {
                a == b
            }
            (BoundExpr::Date(a), BoundExpr::Date(b)) => a == b,
            (BoundExpr::Timestamp(a), BoundExpr::Timestamp(b)) => a == b,
            (BoundExpr::Time(a), BoundExpr::Time(b)) => a == b,
            (BoundExpr::TimeColumnRef { index: a }, BoundExpr::TimeColumnRef { index: b }) => {
                a == b
            }
            (BoundExpr::DateColumnRef { index: a }, BoundExpr::DateColumnRef { index: b }) => {
                a == b
            }
            (
                BoundExpr::TimestampColumnRef { index: a },
                BoundExpr::TimestampColumnRef { index: b },
            ) => a == b,
            (BoundExpr::ColumnRef { index: a }, BoundExpr::ColumnRef { index: b }) => a == b,
            (BoundExpr::JsonbColumnRef { index: a }, BoundExpr::JsonbColumnRef { index: b }) => {
                a == b
            }
            (BoundExpr::JsonColumnRef { index: a }, BoundExpr::JsonColumnRef { index: b }) => {
                a == b
            }
            (BoundExpr::JsonTextFn(a), BoundExpr::JsonTextFn(b)) => a == b,
            (
                BoundExpr::JsonAccess {
                    lhs: la,
                    rhs: ra,
                    as_text: ta,
                },
                BoundExpr::JsonAccess {
                    lhs: lb,
                    rhs: rb,
                    as_text: tb,
                },
            ) => la == lb && ra == rb && ta == tb,
            (
                BoundExpr::JsonPredicate {
                    op: oa,
                    lhs: la,
                    rhs: ra,
                },
                BoundExpr::JsonPredicate {
                    op: ob,
                    lhs: lb,
                    rhs: rb,
                },
            ) => oa == ob && la == lb && ra == rb,
            (
                BoundExpr::JsonModify {
                    op: oa,
                    lhs: la,
                    rhs: ra,
                },
                BoundExpr::JsonModify {
                    op: ob,
                    lhs: lb,
                    rhs: rb,
                },
            ) => oa == ob && la == lb && ra == rb,
            (BoundExpr::JsonbLiteral(a), BoundExpr::JsonbLiteral(b)) => a == b,
            (BoundExpr::JsonFn { f: fa, args: aa }, BoundExpr::JsonFn { f: fb, args: ab }) => {
                fa == fb && aa == ab
            }
            (BoundExpr::IdRef, BoundExpr::IdRef) => true,
            (
                BoundExpr::IdCompare { op: oa, value: va },
                BoundExpr::IdCompare { op: ob, value: vb },
            ) => oa == ob && va == vb,
            (BoundExpr::VectorRef, BoundExpr::VectorRef) => true,
            (BoundExpr::Null, BoundExpr::Null) => true,
            (BoundExpr::Builtin { f: fa, args: aa }, BoundExpr::Builtin { f: fb, args: ab }) => {
                fa == fb && aa == ab
            }
            (
                BoundExpr::Binary {
                    op: opa,
                    lhs: la,
                    rhs: ra,
                },
                BoundExpr::Binary {
                    op: opb,
                    lhs: lb,
                    rhs: rb,
                },
            ) => opa == opb && la == lb && ra == rb,
            (
                BoundExpr::Case {
                    whens: wa,
                    else_result: ea,
                },
                BoundExpr::Case {
                    whens: wb,
                    else_result: eb,
                },
            ) => wa == wb && ea == eb,
            (BoundExpr::Coalesce(a), BoundExpr::Coalesce(b)) => a == b,
            (BoundExpr::NullIf { lhs: la, rhs: ra }, BoundExpr::NullIf { lhs: lb, rhs: rb }) => {
                la == lb && ra == rb
            }
            (
                BoundExpr::WasmCall {
                    name: na,
                    backend: ba,
                    args: aa,
                },
                BoundExpr::WasmCall {
                    name: nb,
                    backend: bb,
                    args: ab,
                },
            ) => na == nb && Arc::ptr_eq(ba, bb) && aa == ab,
            _ => false,
        }
    }
}

/// 束縛済み式の静的型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprType {
    Scalar,
    Vector,
    Bool,
    /// TEXT 値（Issue #919・SQL-26）。
    Text,
    /// `DATE` 値（対象ビヘイビア: SQL-26。Issue #920）。
    Date,
    /// `TIMESTAMP` 値（[`ExprType::Date`] 参照）。
    Timestamp,
    /// `TIME` 値（0 時起点のマイクロ秒。Issue #1570）。
    Time,
    /// jsonb 値（Issue #1596・TABLE-25）。消費できるのは投影・比較・ORDER BY・JSON 演算子の
    /// オペランド・jsonb 関数（Issue #1599）の引数だけで、算術・集計・関数引数・`CASE` 等・代入は `0A000` で拒否する（Issue #1597）。
    Json,
    /// `json`（原文保存）値（Issue #2180・TABLE-25）。値は入力の部分テキストをそのまま
    /// 保持する。比較・並べ替え・グループ化には演算子が無い（`42883`）。算術・集計・
    /// `CASE` 等・代入は `0A000` で拒否する。
    JsonText,
}

/// 評価結果の値。
///
/// `Vector` は `Cow<'a, [f32]>`（Issue #352）で保持する。`VectorRef`（テーブルの
/// `VECTOR` 列をそのまま参照する式）の評価は行データを複製せず `Cow::Borrowed` を
/// 返し、`vec_div`・vector×scalar 演算のように新しいベクトルを構築する評価だけが
/// `Cow::Owned` で `try_reserve_exact`（fail-closed）による確保を行う。呼び出し元が
/// 所有データを必要とする場合（投影段で応答セルへ変換する等）は
/// [`into_owned_vector`] を使う。
#[derive(Debug, Clone, PartialEq)]
pub enum ExprValue<'a> {
    Scalar(f64),
    Vector(Cow<'a, [f32]>),
    Bool(bool),
    /// TEXT 値（Issue #919・SQL-26、AC1）。列参照は借用（`Cow::Borrowed`）、
    /// 関数結果（`LOWER`/`CONCAT` 等）は新規構築のため所有（`Cow::Owned`）で返す
    /// （`Vector` と同じ借用/所有の使い分け方針）。
    Text(Cow<'a, str>),
    /// `DATE` 値（1970-01-01 起点の日数。対象ビヘイビア: SQL-26。Issue #920）。
    Date(i32),
    /// `TIMESTAMP` 値（マイクロ秒。[`ExprValue::Date`] 参照）。
    Timestamp(i64),
    /// `TIME` 値（0 時起点のマイクロ秒。`0..=86_400_000_000`。Issue #1570）。
    Time(i64),
    /// jsonb 値の正規化テキスト（Issue #1596・TABLE-25。`write_canonical` 済み）。
    /// 列参照は借用、パス演算子の結果は所有。
    Json(Cow<'a, str>),
    /// `json`（原文保存）値の原文テキスト（Issue #2180・TABLE-25）。列参照は借用、
    /// 取り出し・`json_build_*` の結果は所有。[`ExprValue::Json`]（jsonb の正規化テキスト）
    /// とは別 variant にして、型の取り違え（原文と正規形）を型で防ぐ。
    JsonText(Cow<'a, str>),
    /// SQL の NULL（Issue #919・SQL-26、AC2 と Issue #921・SQL-26 の共有
    /// variant）。nullable TEXT 列参照・strict な関数への NULL 入力、および
    /// `CASE`／`COALESCE`／`NULLIF` の評価結果から生じる。呼び出し元
    /// （`sql::exec`/`sql::scan`/`sql::aggregate`/`sql::group_by`/
    /// `sql::check_constraint`）は PostgreSQL の 3 値論理に基づく UNKNOWN として
    /// 扱う: `WHERE`/`CHECK` では非該当・充足、投影では `Cell::Null`、
    /// `SUM`/`AVG`/`MIN`/`MAX`/`COUNT(expr)` では当該行をスキップする。
    Null,
}

/// [`ExprValue::Vector`] を所有 `Vec<f32>` へ変換する（投影段など、評価結果を
/// 行データより長く保持する必要がある呼び出し元向け）。`Cow::Owned` はそのまま
/// move するため確保が発生しない（`vec_div` 等、評価内で既に新規構築済みの場合）。
/// `Cow::Borrowed`（`VectorRef` の借用評価）は `Vec::to_vec()`（内部で infallible
/// alloc を使い OOM 時にプロセスを abort しうる）ではなく `try_reserve_exact` で
/// 確保成否を確認してから複製する（同一ファイル内の `vec_div`・
/// `apply_vector_scalar_op` と同じ fail-closed 方針。確保失敗は `54000` へ写像し、
/// abort させない）。
pub fn into_owned_vector(v: Cow<'_, [f32]>) -> Result<Vec<f32>, SqlSurfaceError> {
    match v {
        Cow::Owned(v) => Ok(v),
        Cow::Borrowed(s) => {
            let mut out: Vec<f32> = Vec::new();
            out.try_reserve_exact(s.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("vector value exceeds available memory")
            })?;
            out.extend_from_slice(s);
            Ok(out)
        }
    }
}

/// 組み込み関数（対象ビヘイビア SQL-9・SQL-26。ポインタ:
/// `docs/spec/05-tasks.md` TASK-210・`docs/spec/04-behavior/sql-surface.md`
/// SQL-26）。数値スカラー関数群（`Abs`〜`Sqrt`）の値レベルの計算は
/// `sql::numeric_fn` へ委譲し、本 enum は解決済み関数の識別子のみを保持する。
/// 日時スカラー関数群（`EXTRACT`/`date_part`/`date_trunc`・`DATE`/`TIMESTAMP`
/// 算術）は本 Issue の対象外（後続課題。Issue #920 実装ノート参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinFn {
    /// `vec_norm(v: Vector) -> Scalar`（L2 ノルム）。
    VecNorm,
    /// `vec_sum(v: Vector) -> Scalar`（成分和）。
    VecSum,
    /// `vec_div(v: Vector, s: Scalar) -> Vector`（成分ごとの除算）。
    VecDiv,
    /// `lower(s: Text) -> Text`（Issue #919・SQL-26）。
    Lower,
    /// `upper(s: Text) -> Text`。
    Upper,
    /// `length(s: Text) -> Scalar`（文字数。Unicode スカラー値単位）。
    Length,
    /// `substr(s: Text, start: Scalar) -> Text`（`len` 省略形）。
    Substr2,
    /// `substr(s: Text, start: Scalar, len: Scalar) -> Text`。
    Substr3,
    /// `concat(a: Text, b: Text) -> Text`。可変長 `CONCAT` は束縛段
    /// （[`bind_call`]）でこの 2 引数版の左畳み込みへ展開する。
    Concat2,
    /// `trim(s: Text) -> Text`（半角空白のみ除去。PostgreSQL 既定）。
    Trim,
    /// `replace(s: Text, from: Text, to: Text) -> Text`。
    Replace,
    /// `position(haystack: Text, needle: Text) -> Scalar`（`POSITION(needle IN
    /// haystack)` 構文。`sql::allowlist::Parser` が引数順を並べ替えてこの順で
    /// `Expr::Call` を組み立てる）。
    Position,
    /// `abs(x: Scalar) -> Scalar`。
    Abs,
    /// `round(x: Scalar) -> Scalar`（1 引数形。`round` は arity で `Round1`／
    /// `Round2` へオーバーロード解決される。[`bind_call`] 参照）。
    Round1,
    /// `round(x: Scalar, n: Scalar) -> Scalar`（2 引数形。小数点以下 `n` 桁）。
    Round2,
    /// `floor(x: Scalar) -> Scalar`。
    Floor,
    /// `ceil(x: Scalar) -> Scalar`（`ceiling` はこの variant の別名として解決
    /// される）。
    Ceil,
    /// `mod(x: Scalar, y: Scalar) -> Scalar`（剰余。符号は被除数に従う）。
    Mod,
    /// `power(x: Scalar, y: Scalar) -> Scalar`。
    Power,
    /// `sqrt(x: Scalar) -> Scalar`。
    Sqrt,
    /// `date_part(field: Text, src: Timestamp) -> Scalar`／`EXTRACT(field FROM
    /// src)`（対象ビヘイビア: SQL-26。Issue #920）。`field` は束縛時に解決済み
    /// （`bind_call` 参照。実行時の引数は `src` 1 個のみ）。
    DatePart(DatePartField),
    /// `date_part(field: Text, src: Time) -> Scalar`／`EXTRACT(field FROM time)`（Issue #1570）。
    /// `date_part` は暦計算で `24:00:00` が日付境界へ折り返すため別 variant にする。`field` は
    /// 束縛時に `datetime_fn::time_supports_field` で検証済み。
    TimePart(DatePartField),
    /// `date_trunc(unit: Text, src: Timestamp) -> Timestamp`（`field` と同じく
    /// `unit` は束縛時に解決済み）。
    DateTrunc(DateTruncUnit),
    /// `DATE` を深夜 0 時の `TIMESTAMP` へ昇格する内部専用関数（名前では
    /// 解決できない。`bind_call`／`bind_binary` が `date_part`／`date_trunc` の
    /// `DATE` 引数、および `DATE`／`TIMESTAMP` 比較の `DATE` 側に挿入する）。
    DateToTimestamp,
    /// `CHAR(n)` 列参照を TEXT として扱うための内部専用関数（名前では解決できない）。
    /// 保存形（n 文字へ空白詰め）から末尾の半角空白（U+0020 のみ）を除く。PostgreSQL の
    /// bpchar→text 変換と同じで、`length`／`lower`／`||`／TEXT 列との比較が末尾空白を
    /// 無視した値で動く。`bind_expr_in` の列参照腕だけが挿入する（TABLE-31・Issue #1566）。
    BpcharToText,
    /// 投影位置の整数／単精度の型付き算術（`+ - * /`）を表す内部専用関数（名前では
    /// 解決できない。Issue #1539・SQL-42。`bind_expr_in` の `Expr::Binary` 腕が
    /// 投影モード〔[`BindEnv::typed_arith`]〕のときだけ挿入する）。整数除算の切り捨て
    /// と checked オーバーフロー（`22003`）をノード単位で行うため、f64 一括計算では
    /// なく専用の演算ノードにしている（`(7/2)*2` が PostgreSQL と同じく 6 になる）。
    TypedArith(BinOp, NumericKind),
    /// `clock_timestamp() -> Timestamp`（呼び出しごとに壁時計を読む。SQL-37・Issue #1586）。
    /// 他の時刻関数と違い束縛時に定数化せず、評価のたびに `sql::time_fn::now_micros` を呼ぶ。
    /// 畳み込み・共通部分式の 1 回化の対象外（[`is_foldable_builtin`]）。
    ClockTimestamp,
}

/// 型付き算術（[`BuiltinFn::TypedArith`]）の結果種別。`float8` は既存の
/// [`BoundExpr::Binary`] で表すためここには含めない（Issue #1539・SQL-42）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericKind {
    /// `smallint`（`int2`。OID 21。Issue #1560。整数リテラルは分類されず、列参照どうしの
    /// 算術の結果としてのみ現れる）。
    Int2,
    /// `integer`（`int4`。OID 23）。
    Int4,
    /// `bigint`（`int8`。OID 20）。
    Int8,
    /// `real`（`float4`。OID 700）。
    Float4,
}

/// `name` が組み込み関数（[`BuiltinFn`]）の名前かどうかを判定する。`pub(crate)`:
/// `sql::check_constraint`（TABLE-16・TASK-204、Issue #906）が `CHECK` 述語中の
/// `Expr::Call` を束縛より**前**に検査し、組み込み関数以外（セッション UDF・
/// WASM UDF・未知関数）の呼び出しを `42601` として拒否するために使う
/// （空の [`UdfRegistry`] で束縛すると「未知の関数」として `42883` へ丸まって
/// しまい、CHECK の禁止要素として区別できないため）。`round` は arity で
/// `Round1`／`Round2` へオーバーロード解決されるため [`builtin_from_name`] の
/// 対象外だが、名前としては組み込み扱いにする必要があるためここで別途判定する。
pub(crate) fn is_builtin_function_name(name: &str) -> bool {
    builtin_from_name(name).is_some()
        || is_variadic_or_overloaded_builtin_name(name)
        || is_current_time_function_name(name)
        || crate::sql::json_fn::is_json_fn_name(&name.to_ascii_lowercase())
        || json_fn_from_name(name).is_some()
        || is_deferred_json_srf_name(name)
}

/// jsonb 関数（[`JsonFnKind`]）の名前解決（Issue #1599）。大小は無視する。
pub(crate) fn json_fn_from_name(name: &str) -> Option<JsonFnKind> {
    match name.to_ascii_lowercase().as_str() {
        "jsonb_typeof" => Some(JsonFnKind::Typeof),
        "jsonb_array_length" => Some(JsonFnKind::ArrayLength),
        "jsonb_extract_path" => Some(JsonFnKind::ExtractPath),
        "jsonb_extract_path_text" => Some(JsonFnKind::ExtractPathText),
        "jsonb_build_object" => Some(JsonFnKind::BuildObject),
        "jsonb_build_array" => Some(JsonFnKind::BuildArray),
        "to_jsonb" => Some(JsonFnKind::ToJsonb),
        _ => None,
    }
}

/// 集合返却関数（SRF）である jsonb 関数の名前（Issue #1599）。SRF の基盤が無いため実行は
/// せず、名前だけ予約して束縛時に `0A000` で拒否する（スカラー関数として実装すると
/// PostgreSQL と意味が食い違うため。従来の「未知の関数」`42883` から変わる）。
pub(crate) fn is_deferred_json_srf_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "jsonb_object_keys"
            | "jsonb_array_elements"
            | "jsonb_array_elements_text"
            | "jsonb_each"
            | "jsonb_each_text"
    )
}

/// 現在時刻関数（関数形）の名前か（SQL-37・Issue #1586）。`now`・`transaction_timestamp`・
/// `statement_timestamp`・`clock_timestamp`。CHECK・定数式（INSERT VALUES）・連鎖ビュー本文が
/// 受理しない文脈で拒否判定に使う。
pub(crate) fn is_current_time_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "now" | "transaction_timestamp" | "statement_timestamp" | "clock_timestamp"
    )
}

/// 式木の中で最初に見つかった「組み込み関数以外の呼び出し」の関数名を返す
/// （無ければ `None`）。呼び出し元は 2 つ: `sql::check_constraint`（`CHECK` 述語の
/// 禁止要素検査。TABLE-16）と `sql::allowlist` の連鎖ビュー本文の `WHERE` 式述語検査
/// （TABLE-18・Issue #1436）。組み込み関数は [`bind_call`] でレジストリより先に
/// 解決され、同名 UDF は登録できない（`42723`）ため、組み込み関数だけの式は
/// 参照セッションの UDF レジストリに依存せず意味が一定になる。`Expr` の全 variant を
/// 網羅し `_ =>` を置かない（variant 追加時にコンパイルエラーで検査漏れに気づく。
/// fail-closed）。再帰の深さは構文段の式ネスト上限で抑えられる。
pub(crate) fn first_non_builtin_call(expr: &Expr) -> Option<&str> {
    first_non_builtin_call_with(expr, false, false)
}

/// [`first_non_builtin_call`] の本体。`allow_current_time` が真のときだけ現在時刻関数
/// （`now()` 等・括弧なし値関数）を禁止要素として報告しない。分割実行 DML の `SET` 右辺が、
/// トランザクション固定の時刻だけを受理するスコープ（[`time_fn::ClockPolicy::TxnFixedOnly`]）の
/// 中でのみ真にする（`sql::set_expr`。SQL-37・Issue #1587）。`statement_timestamp()`・
/// `clock_timestamp()` はここでは通し、束縛段（`reject_non_txn_fixed_clock`）が `0A000` にする。
pub(crate) fn first_non_builtin_call_with(
    expr: &Expr,
    allow_current_time: bool,
    allow_json_fns: bool,
) -> Option<&str> {
    match expr {
        Expr::Number(_)
        | Expr::Ident(_)
        | Expr::String(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_) => None,
        Expr::Call { name, args } => {
            // `json_*`／`to_json`（Issue #2180）は json 型の式を要し、CHECK 本体・連鎖ビュー本文には
            // json 列参照を置けないため、時刻関数と同じく禁止要素として報告する（`42601`）。
            if !is_builtin_function_name(name)
                || (!allow_current_time && is_current_time_function_name(name))
                || (!allow_json_fns
                    && crate::sql::json_fn::is_json_fn_name(&name.to_ascii_lowercase()))
            {
                return Some(name.as_str());
            }
            args.iter()
                .find_map(|a| first_non_builtin_call_with(a, allow_current_time, allow_json_fns))
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::NullIf(lhs, rhs)
        | Expr::JsonAccess { lhs, rhs, .. } => {
            first_non_builtin_call_with(lhs, allow_current_time, allow_json_fns)
                .or_else(|| first_non_builtin_call_with(rhs, allow_current_time, allow_json_fns))
        }
        // 照合指定（TABLE-28・Issue #1609）は内側の式を検査するだけ。
        Expr::Collate { expr, .. } => {
            first_non_builtin_call_with(expr, allow_current_time, allow_json_fns)
        }
        Expr::Case { whens, else_result } => whens
            .iter()
            .find_map(|(c, r)| {
                first_non_builtin_call_with(c, allow_current_time, allow_json_fns)
                    .or_else(|| first_non_builtin_call_with(r, allow_current_time, allow_json_fns))
            })
            .or_else(|| {
                else_result.as_deref().and_then(|e| {
                    first_non_builtin_call_with(e, allow_current_time, allow_json_fns)
                })
            }),
        Expr::Coalesce(args) => args
            .iter()
            .find_map(|a| first_non_builtin_call_with(a, allow_current_time, allow_json_fns)),
        // 現在時刻関数（SQL-37・Issue #1586）は組み込みだが、CHECK（TABLE-16）・連鎖ビュー本文
        // （TABLE-18）では決定的でないため禁止要素として報告する（`42601`）。
        Expr::ValueFunction { kind, .. } => {
            if allow_current_time {
                None
            } else {
                Some(kind.as_sql())
            }
        }
    }
}

/// `substr`／`concat` は arity に応じて `BuiltinFn` variant（`Substr2`/`Substr3`）
/// を選ぶか可変長を左畳み込みへ展開する必要があるため、この一意な名前解決には
/// 含めない（[`bind_call`]・[`validate_closed_expr`] が個別に扱う）。予約名判定
/// （[`is_reserved_function_name`]・[`is_builtin_function_name`]）はこの関数とは
/// 別に `substr`／`concat` を明示的に含める。
fn builtin_from_name(name: &str) -> Option<BuiltinFn> {
    match name.to_ascii_lowercase().as_str() {
        "vec_norm" => Some(BuiltinFn::VecNorm),
        "vec_sum" => Some(BuiltinFn::VecSum),
        "vec_div" => Some(BuiltinFn::VecDiv),
        "lower" => Some(BuiltinFn::Lower),
        "upper" => Some(BuiltinFn::Upper),
        "length" => Some(BuiltinFn::Length),
        "trim" => Some(BuiltinFn::Trim),
        "replace" => Some(BuiltinFn::Replace),
        "position" => Some(BuiltinFn::Position),
        "abs" => Some(BuiltinFn::Abs),
        "floor" => Some(BuiltinFn::Floor),
        "ceil" | "ceiling" => Some(BuiltinFn::Ceil),
        "mod" => Some(BuiltinFn::Mod),
        "power" => Some(BuiltinFn::Power),
        "sqrt" => Some(BuiltinFn::Sqrt),
        "clock_timestamp" => Some(BuiltinFn::ClockTimestamp),
        // `round` は arity オーバーロード（1 引数／2 引数）のため、名前だけでは
        // 一意に variant を決定できない。呼び出し元（`bind_call`・
        // `validate_closed_expr`）が引数個数を見て `Round1`／`Round2` を
        // 個別に解決する（[`is_builtin_function_name`] 参照）。
        _ => None,
    }
}

/// `name` が `substr`／`concat`／`round`（arity 依存で個別解決する組み込み
/// 関数名。`builtin_from_name` 単体では variant が一意に定まらない）かを
/// 判定する（大小無視）。main の数値スカラー関数群（Issue #920）マージ時に
/// `round` の追加漏れがあると `is_builtin_function_name` が `round` を予約名
/// として認識せず、`CREATE FUNCTION round(...)` の再定義を許してしまう
/// （`defining_a_udf_named_round_is_rejected_as_reserved` の回帰）。
fn is_variadic_or_overloaded_builtin_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        // `date_part`／`date_trunc`（対象ビヘイビア: SQL-26。Issue #920）は
        // field／unit の解決結果を `BuiltinFn` のペイロードとして持つため、
        // 名前だけでは variant が一意に定まらない（`round` と同じ理由。§2-4）。
        "substr" | "concat" | "round" | "date_part" | "date_trunc"
    )
}

/// 組み込み関数の引数個数・型シグネチャ（束縛時の検査に使う）。
/// `sql::expr_program::ExprStep::Builtin` の実行時（明示スタックからの pop 数決定）
/// でも束縛時と同じ arity 判定を使うため `pub(crate)` で共有する
/// （Issue #353・式評価のステップ列コンパイル化）。
pub(crate) fn builtin_signature(f: BuiltinFn) -> (&'static [ExprType], ExprType) {
    match f {
        BuiltinFn::VecNorm => (&[ExprType::Vector], ExprType::Scalar),
        BuiltinFn::VecSum => (&[ExprType::Vector], ExprType::Scalar),
        BuiltinFn::VecDiv => (&[ExprType::Vector, ExprType::Scalar], ExprType::Vector),
        BuiltinFn::Lower => (&[ExprType::Text], ExprType::Text),
        BuiltinFn::Upper => (&[ExprType::Text], ExprType::Text),
        BuiltinFn::Length => (&[ExprType::Text], ExprType::Scalar),
        BuiltinFn::Substr2 => (&[ExprType::Text, ExprType::Scalar], ExprType::Text),
        BuiltinFn::Substr3 => (
            &[ExprType::Text, ExprType::Scalar, ExprType::Scalar],
            ExprType::Text,
        ),
        BuiltinFn::Concat2 => (&[ExprType::Text, ExprType::Text], ExprType::Text),
        BuiltinFn::Trim => (&[ExprType::Text], ExprType::Text),
        BuiltinFn::Replace => (
            &[ExprType::Text, ExprType::Text, ExprType::Text],
            ExprType::Text,
        ),
        BuiltinFn::Position => (&[ExprType::Text, ExprType::Text], ExprType::Scalar),
        BuiltinFn::Abs => (&[ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Round1 => (&[ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Round2 => (&[ExprType::Scalar, ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Floor => (&[ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Ceil => (&[ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Mod => (&[ExprType::Scalar, ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Power => (&[ExprType::Scalar, ExprType::Scalar], ExprType::Scalar),
        BuiltinFn::Sqrt => (&[ExprType::Scalar], ExprType::Scalar),
        // `date_part`／`date_trunc` の実行時引数は `src`（`Timestamp`。`DATE`
        // 入力は束縛時に `DateToTimestamp` で昇格済み）1 個のみ。field／unit は
        // `BuiltinFn` のペイロードとして束縛時に解決済みのため実行時引数には
        // 含まれない（§2-4）。
        BuiltinFn::DatePart(_) => (&[ExprType::Timestamp], ExprType::Scalar),
        BuiltinFn::TimePart(_) => (&[ExprType::Time], ExprType::Scalar),
        BuiltinFn::DateTrunc(_) => (&[ExprType::Timestamp], ExprType::Timestamp),
        BuiltinFn::DateToTimestamp => (&[ExprType::Date], ExprType::Timestamp),
        BuiltinFn::BpcharToText => (&[ExprType::Text], ExprType::Text),
        BuiltinFn::TypedArith(_, _) => (&[ExprType::Scalar, ExprType::Scalar], ExprType::Scalar),
        // 戻り値型は TIMESTAMP（D8 の暫定。TIMESTAMPTZ 型の実装時に切り替える）。
        BuiltinFn::ClockTimestamp => (&[], ExprType::Timestamp),
    }
}

/// [`BuiltinFn`] が定数畳み込み（`sql::expr_program::try_fold_scalar`）の対象に
/// なりうるかを判定する。行依存（`VectorRef`・行 `id`）を引数に取りうる・
/// 非決定的な関数は対象外にする。`_ =>` を使わない網羅 `match` にすることで、
/// 将来 `BuiltinFn` に variant を追加した際にここへの追随漏れをコンパイル
/// エラーとして検出できるようにする（AC3・security.md「不安全な設計」対応の
/// ための明示化）。
pub(crate) fn is_foldable_builtin(f: BuiltinFn) -> bool {
    match f {
        // Vector を引数に取る組み込みは、実引数が定数（`Number`）になることが
        // 実務上ない（`vec_norm` 等は常に `VectorRef` を受け取る）ため、
        // 畳み込み対象に含める意味がない。
        BuiltinFn::VecNorm | BuiltinFn::VecSum | BuiltinFn::VecDiv => false,
        BuiltinFn::Abs
        | BuiltinFn::Round1
        | BuiltinFn::Round2
        | BuiltinFn::Floor
        | BuiltinFn::Ceil
        | BuiltinFn::Mod
        | BuiltinFn::Power
        | BuiltinFn::Sqrt => true,
        // Issue #919・SQL-26: 文字列組み込みは `try_fold_scalar`
        // （`FoldedConst::Scalar`/`Bool` のみを表現できる型）の畳み込み対象に
        // 含めない。正しさには影響しない（§3-8 は対象外事項）。
        BuiltinFn::Lower
        | BuiltinFn::Upper
        | BuiltinFn::Length
        | BuiltinFn::Substr2
        | BuiltinFn::Substr3
        | BuiltinFn::Concat2
        | BuiltinFn::Trim
        | BuiltinFn::Replace
        | BuiltinFn::Position => false,
        // 対象ビヘイビア: SQL-26（Issue #920）。`sql::expr_program::FoldedConst`
        // は `Scalar`／`Bool` のみを表現できる型（文字列組み込みと同じ制約。
        // 上記コメント参照）で `DATE`／`TIMESTAMP` 値を持てないため、
        // `date_trunc`／`DateToTimestamp`（戻り値が `Timestamp`／`Date`）は
        // 畳み込み対象に含めない。`date_part`（戻り値は `Scalar`）も対称性の
        // ため同様に対象外とする。正しさには影響しない（defer-on-error で
        // 実行時に評価される。対象外事項として `docs/design/
        // datetime-scalar-functions.md` に記録）。
        BuiltinFn::DatePart(_)
        | BuiltinFn::TimePart(_)
        | BuiltinFn::DateTrunc(_)
        | BuiltinFn::DateToTimestamp
        | BuiltinFn::BpcharToText => false,
        // 非決定的（呼び出しごとに値が変わる）ため畳み込まない（SQL-37・Issue #1586）。
        BuiltinFn::ClockTimestamp => false,
        // Issue #1539・SQL-42: 型付き算術は `Binary` と同じく `Scalar` を返す純関数で、
        // 0 除算・オーバーフローは defer-on-error（畳み込み失敗時は実行時に評価）。
        BuiltinFn::TypedArith(_, _) => true,
    }
}

/// 組み込み関数の arity（[`builtin_signature`] が返す引数個数）の上限。
/// `sql::expr_program::ExprStep::Builtin` の実行時、この上限個数の固定配列
/// （ヒープ確保なし）へ引数を積んでから [`apply_builtin`] を呼ぶために使う
/// （PR #373 codex-review 指摘対応・追加 `Vec` 確保の排除）。新しい組み込み関数を
/// 追加してこの上限を超える arity になる場合はここも合わせて引き上げる
/// （`builtin_arities_fit_max_arity` で全 [`BuiltinFn`] 網羅的に検証する）。
pub(crate) const MAX_BUILTIN_ARITY: usize = 3;

/// `WHERE`・`ORDER BY` の既存許可名（`allowlist::is_allowed_where_predicate_name`
/// 等）・組み込み関数名・集計関数名（`allowlist::is_aggregate_function_name`。
/// TASK-166・SQL-13）と衝突する UDF 名を拒否するための一覧。名前空間を一本化する
/// ことで「同じ字面が場所により異なる意味を持つ」曖昧さを構造的に排除する
/// （集計関数名を含めない版では `CREATE FUNCTION min(...)` が成功したまま
/// `SELECT min(id)` が集計として実行され、当該 UDF が呼び出し不能になる不整合が
/// あった。Cursor Bugbot 指摘対応・PR #229）。
fn is_reserved_function_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    // `CASE`／`COALESCE`／`NULLIF`（対象ビヘイビア: SQL-26。Issue #921）は式文法の
    // 専用ノード（`Expr::Case`／`Expr::Coalesce`／`Expr::NullIf`）として解析される
    // ため、同名の UDF を許すと呼び出し不能な定義が登録できてしまう（`CASE` は
    // `parse_primary_expr` が `'('` の有無を見ず常に文脈的キーワードとして
    // 消費するため、`case(...)` という呼び出し形は `WHEN` を期待する CASE
    // 構文解析へ吸われて `42601` になる。`is_reserved_function_name` docs の
    // 既存 `min`/`集計` と同じ理由。Bugbot 指摘対応）。
    matches!(
        upper.as_str(),
        "VISIBLE" | "HYBRID_RRF" | "HYBRID" | "CASE" | "COALESCE" | "NULLIF"
    ) || is_builtin_function_name(name)
        || crate::sql::allowlist::is_aggregate_function_name(name)
}

/// セッション内で登録された宣言的 UDF 1 件。本体は構文段の [`Expr`]（パラメータ参照は
/// [`Expr::Ident`] のまま。呼び出し側で束縛するたびインライン展開する）。
#[derive(Debug, Clone, PartialEq)]
pub struct UdfDefinition {
    pub params: Vec<String>,
    pub body: Expr,
}

/// セッション単位の UDF レジストリ（`sql::mode::SessionState` が保持する）。
/// 追記専用（再定義・`DROP` は許可しない。RLS-8 と同じ「認証済みテナントの接続単位」
/// の外へ漏れない構造にするため、他セッション・永続化とは無関係に保つ）。
///
/// `PartialEq` は手動実装する（`wasm` マップの値 `Arc<dyn WasmUdfBackend>` が
/// `dyn` 型のため構造的な `derive` を導出できない。宣言的 UDF 定義は構造等価、
/// WASM UDF は名前一致かつ `Arc::ptr_eq` で判定する）。
#[derive(Debug, Clone, Default)]
pub struct UdfRegistry {
    defs: std::collections::BTreeMap<String, UdfDefinition>,
    /// TASK-149（EXT-5, EXT-6）: WASM UDF のセッション単位レジストリ。宣言的 UDF
    /// （`defs`）とは別の名前空間ではなく同一の名前空間を共有する（衝突検査は
    /// [`define_wasm_function`]・[`define_function`] の双方が
    /// [`is_name_taken`] を通して行う）。
    wasm: std::collections::BTreeMap<String, Arc<dyn WasmUdfBackend>>,
}

impl PartialEq for UdfRegistry {
    fn eq(&self, other: &Self) -> bool {
        if self.defs != other.defs || self.wasm.len() != other.wasm.len() {
            return false;
        }
        self.wasm.iter().all(|(name, backend)| {
            other
                .wasm
                .get(name)
                .is_some_and(|other_backend| Arc::ptr_eq(backend, other_backend))
        })
    }
}

impl UdfRegistry {
    pub fn get(&self, name: &str) -> Option<&UdfDefinition> {
        self.defs.get(&name.to_ascii_lowercase())
    }

    /// 登録済みの WASM UDF バックエンドを名前で引く（`bind_call` の解決経路から
    /// 呼ばれる。存在しない場合は `None`）。
    pub fn get_wasm(&self, name: &str) -> Option<&Arc<dyn WasmUdfBackend>> {
        self.wasm.get(&name.to_ascii_lowercase())
    }

    pub fn len(&self) -> usize {
        self.defs.len() + self.wasm.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty() && self.wasm.is_empty()
    }

    /// 名前空間（組み込み・宣言的 UDF・WASM UDF を横断した名前）が既に使われて
    /// いるかを判定する（[`define_function`]・[`define_wasm_function`] が共有する
    /// 衝突検査）。
    fn is_name_taken(&self, lower: &str) -> bool {
        self.defs.contains_key(lower) || self.wasm.contains_key(lower)
    }
}

/// `CREATE FUNCTION <name>(<params>) AS <body>` を検証してセッションのレジストリへ
/// 登録する（`core.rs::EngineCore::execute_sql_in_session` の `CreateFunction` 分岐から
/// 呼ばれる）。検証→登録の順を守り、失敗時は `registry` を一切変更しない
/// （部分更新＝黙った既定化を防ぐ。`sql::mode::SessionState::set_search_mode` と
/// 同方針）。
///
/// 検証項目: 名前・パラメータ名が `catalog::validate_identifier` に適合、パラメータ
/// 重複なし、パラメータ数が [`MAX_UDF_PARAMS`] 以内、名前が組み込み・既存許可名と
/// 非衝突、同名の再定義でないこと、セッション UDF 数が [`MAX_SESSION_UDFS`] 未満、
/// 本体式がパラメータ参照・数値リテラル・算術/比較演算子・組み込み関数・登録済み
/// UDF 呼び出しのみで構成される（列参照は拒否＝閉じた関数。束縛は `schema: None`
/// で行うため、列名を参照すると `bind_expr` が `22000` を返す）。
pub fn define_function(
    registry: &mut UdfRegistry,
    name: &str,
    params: &[String],
    body: &Expr,
) -> Result<(), SqlSurfaceError> {
    catalog::validate_identifier(name)
        .map_err(|_| SqlSurfaceError::invalid_input(format!("invalid function name: {name}")))?;
    let lower = name.to_ascii_lowercase();
    if is_reserved_function_name(name) {
        return Err(SqlSurfaceError::duplicate_function(format!(
            "function name {name} collides with a built-in or reserved name"
        )));
    }
    if registry.is_name_taken(&lower) {
        return Err(SqlSurfaceError::duplicate_function(format!(
            "function {name} is already defined in this session"
        )));
    }
    if registry.len() >= MAX_SESSION_UDFS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many functions defined in this session",
        ));
    }
    if params.len() > MAX_UDF_PARAMS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many function parameters",
        ));
    }
    // パラメータ名は SQL 識別子として大文字小文字を区別しない扱いに正規化する
    // （引用なし識別子の大文字小文字はどちらで書いても同一パラメータを指す）。
    // 重複検査・本体の参照検証（`validate_closed_expr`）・呼び出し時の `BindEnv`
    // キー・参照解決（`bind_expr_in`）の全経路でこの正規形（小文字）に統一する
    // ことで、`CREATE FUNCTION f(V) AS v` のような引用なし識別子の大文字小文字が
    // 経路ごとに食い違い、本体参照が誤って undefined reference 判定される不整合
    // を構造的に防ぐ。
    let mut seen = std::collections::HashSet::new();
    let mut normalized_params = Vec::with_capacity(params.len());
    for p in params {
        catalog::validate_identifier(p)
            .map_err(|_| SqlSurfaceError::invalid_input(format!("invalid parameter name: {p}")))?;
        let normalized = p.to_ascii_lowercase();
        if !seen.insert(normalized.clone()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate parameter name: {p}"
            )));
        }
        normalized_params.push(normalized);
    }

    // 本体式は列参照を持たない閉じた関数であること（`schema: None`）を確認するため
    // 束縛を試みる。パラメータは全て `Scalar` として仮束縛して型検査する
    // （呼び出し位置により実際は `Vector` を渡す UDF もありうるため、定義時点の
    // 型検査は「構造的に閉じているか（列参照・未知名の不在）」の確認に留め、
    // 型の厳密な検査は呼び出し（インライン展開）時に呼び出し元の引数型で行う）。
    let mut node_budget = MAX_EXPR_NODES;
    validate_closed_expr(body, &normalized_params, registry, &mut node_budget)?;

    registry.defs.insert(
        lower,
        UdfDefinition {
            params: normalized_params,
            body: body.clone(),
        },
    );
    Ok(())
}

/// テスト専用: `define_function` のパラメータ名正規化（小文字化）を経由せず、
/// 呼び出し側が指定した綴りのまま `UdfDefinition` をレジストリへ直接登録する。
/// `recovery::content_hash` の直列化（`push_dml_where_predicates` の UDF 定義
/// セクション）が、`define_function` による正規化に依存せずそれ自体で大文字
/// 小文字を畳み込む契約を検証するための入口（`defs` フィールドが private のため
/// 同一モジュール内に置く）。production 経路（`define_function` 一択）はこの
/// ヘルパーを経由しない。
#[cfg(test)]
pub(crate) fn insert_raw_definition_for_test(
    registry: &mut UdfRegistry,
    name: &str,
    def: UdfDefinition,
) {
    registry.defs.insert(name.to_ascii_lowercase(), def);
}

/// 検証済みの `Arc<dyn WasmUdfBackend>` をセッションのレジストリへ登録する
/// （TASK-149、対象ビヘイビア: EXT-5, EXT-6。`sql::mode::SessionState::register_wasm_udf`
/// から呼ばれる）。名前空間・上限は宣言的 UDF（[`define_function`]）と共有し、
/// SQL からの `CREATE FUNCTION` 構文・wire 経由のモジュール搬送・モジュールバイト
/// 列からのバックエンド構築（wasmtime 依存のユーザー承認待ち。`crate::wasm_udf`
/// モジュールドキュメント参照）は本タスクのスコープ外（呼び出し元が検証済み
/// バックエンドの構築を担う）。
pub fn define_wasm_function(
    registry: &mut UdfRegistry,
    name: &str,
    backend: Arc<dyn WasmUdfBackend>,
) -> Result<(), SqlSurfaceError> {
    catalog::validate_identifier(name)
        .map_err(|_| SqlSurfaceError::invalid_input(format!("invalid function name: {name}")))?;
    let lower = name.to_ascii_lowercase();
    if is_reserved_function_name(name) {
        return Err(SqlSurfaceError::duplicate_function(format!(
            "function name {name} collides with a built-in or reserved name"
        )));
    }
    if registry.is_name_taken(&lower) {
        return Err(SqlSurfaceError::duplicate_function(format!(
            "function {name} is already defined in this session"
        )));
    }
    if registry.len() >= MAX_SESSION_UDFS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many functions defined in this session",
        ));
    }
    registry.wasm.insert(lower, backend);
    Ok(())
}

/// UDF 本体式が「パラメータ参照・数値リテラル・演算子・組み込み関数・登録済み UDF
/// 呼び出しのみ」で構成されているかを構造的に検査する（列参照禁止＝閉じた関数）。
/// 未登録の呼び出し名・引数個数の不整合はここで拒否する（`42883`。Issue #1349）。自己参照・
/// 前方参照は、レジストリが「検証成功後にのみ挿入する」追記専用のため構造上
/// 発生し得ない（本関数の時点で `registry` に現在定義中の名前はまだ存在しない）。
fn validate_closed_expr(
    expr: &Expr,
    params: &[String],
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(), SqlSurfaceError> {
    *node_budget = node_budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("function body expression is too large")
    })?;
    match expr {
        Expr::Number(_) => Ok(()),
        Expr::String(_) => Ok(()),
        // 照合指定（TABLE-28・Issue #1609）は内側の式を検査する（束縛段が文字列系か検査する）。
        Expr::Collate { expr, .. } => validate_closed_expr(expr, params, registry, node_budget),
        // jsonb パス演算子は列参照を要するため、列参照を持てない関数本体では常に未対応
        // （Issue #1596。jsonb 値を受け取る関数引数型も存在しない）。
        Expr::JsonAccess { .. } => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "JSON operators are not supported in a function body".to_string(),
        }),
        Expr::Ident(name) => {
            // `params` は `define_function` で正規化済み（小文字）。本体側の参照は
            // 引用なし識別子として書かれた原文字列のままなので、比較のたびに同じ
            // 正規形へそろえる（呼び出し時の `bind_expr_in` の参照解決と一貫させる）。
            if params.iter().any(|p| *p == name.to_ascii_lowercase()) {
                Ok(())
            } else {
                Err(SqlSurfaceError::invalid_input(format!(
                    "undefined reference in function body: {name}"
                )))
            }
        }
        Expr::Call { name, args } => {
            if args.len() > MAX_CALL_ARGS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many call arguments",
                ));
            }
            // 呼び出し先の存在だけでなく、引数個数（組み込みは `builtin_signature`、
            // 登録済み UDF は `UdfDefinition::params.len()`）も定義時に照合する。
            // ここを素通りすると `CREATE FUNCTION f() AS vec_norm()` のような
            // 引数数不一致の関数本体が「定義時検証」という公開契約に反して登録
            // されてしまう（呼び出し時の `bind_call` 側の検査だけでは間に合わない）。
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "now" | "transaction_timestamp" | "statement_timestamp"
            ) {
                // 現在時刻関数（SQL-37・Issue #1586）は 0 引数の組み込み。`clock_timestamp` は
                // 下の `builtin_from_name` 分岐で解決される。
                if !args.is_empty() {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects 0 argument(s), got {}",
                        args.len()
                    )));
                }
            } else if crate::sql::json_fn::is_json_fn_name(&name.to_ascii_lowercase()) {
                // json 型の式を要する関数（Issue #2180）。関数本体は列参照を持てず json 型の
                // 引数も存在しないため、定義時に fail-closed で拒否する。
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "JSON functions are not supported in a function body".to_string(),
                });
            } else if name.eq_ignore_ascii_case("concat") {
                if args.is_empty() {
                    return Err(SqlSurfaceError::undefined_function(
                        "function concat expects at least 1 argument, got 0",
                    ));
                }
            } else if name.eq_ignore_ascii_case("substr") {
                if !matches!(args.len(), 2 | 3) {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects 2 or 3 arguments, got {}",
                        args.len()
                    )));
                }
            } else if name.eq_ignore_ascii_case("date_part")
                || name.eq_ignore_ascii_case("date_trunc")
            {
                // 対象ビヘイビア: SQL-26（Issue #920）。field／unit の解決は
                // 呼び出し時（`bind_call`）に第 1 引数が Text リテラルか検査する
                // ため、定義時はここで arity（常に 2）のみ検査する。
                if args.len() != 2 {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects 2 argument(s), got {}",
                        args.len()
                    )));
                }
            } else if name.eq_ignore_ascii_case("round") {
                // `round` は arity オーバーロード（1／2 引数）。他の組み込みと
                // 異なり単一の `BuiltinFn` に定まらないため、ここでは引数個数の
                // 妥当性のみ検査する（実際の variant 解決は呼び出し時の
                // `bind_call` が行う）。
                if !(1..=2).contains(&args.len()) {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects 1 or 2 argument(s), got {}",
                        args.len()
                    )));
                }
            } else if json_fn_from_name(name).is_some() || is_deferred_json_srf_name(name) {
                // jsonb 関数（Issue #1599）は jsonb 値を取るため、列参照を持てない関数本体では
                // 常に未対応（jsonb パス演算子と同じ扱い）。
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "jsonb functions are not supported in a function body".to_string(),
                });
            } else if let Some(builtin) = builtin_from_name(name) {
                let (param_types, _ret) = builtin_signature(builtin);
                if args.len() != param_types.len() {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects {} argument(s), got {}",
                        param_types.len(),
                        args.len()
                    )));
                }
            } else if let Some(def) = registry.get(name) {
                if args.len() != def.params.len() {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects {} argument(s), got {}",
                        def.params.len(),
                        args.len()
                    )));
                }
            } else if registry.get_wasm(name).is_some() {
                // WASM UDF（TASK-149）は ABI 固定シグネチャ
                // `(Vector, Scalar) -> Scalar` のみのため、常に 2 引数固定。
                if args.len() != WASM_CALL_ARITY {
                    return Err(SqlSurfaceError::undefined_function(format!(
                        "function {name} expects {WASM_CALL_ARITY} argument(s), got {}",
                        args.len()
                    )));
                }
            } else {
                return Err(SqlSurfaceError::undefined_function(format!(
                    "function {name} does not exist"
                )));
            }
            for a in args {
                validate_closed_expr(a, params, registry, node_budget)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. } => {
            validate_closed_expr(lhs, params, registry, node_budget)?;
            validate_closed_expr(rhs, params, registry, node_budget)
        }
        Expr::Null => Ok(()),
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                validate_closed_expr(cond, params, registry, node_budget)?;
                validate_closed_expr(result, params, registry, node_budget)?;
            }
            if let Some(else_result) = else_result {
                validate_closed_expr(else_result, params, registry, node_budget)?;
            }
            Ok(())
        }
        Expr::Coalesce(args) => {
            for a in args {
                validate_closed_expr(a, params, registry, node_budget)?;
            }
            Ok(())
        }
        Expr::NullIf(lhs, rhs) => {
            validate_closed_expr(lhs, params, registry, node_budget)?;
            validate_closed_expr(rhs, params, registry, node_budget)
        }
        Expr::DateLiteral(_) | Expr::TimestampLiteral(_) | Expr::TimeLiteral(_) => Ok(()),
        // 時刻の値関数は呼び出し側の文脈（読み取り文）の時刻へ展開時に束縛される。
        Expr::ValueFunction { .. } => Ok(()),
    }
}

/// 束縛環境: 列参照の解決元（`SELECT`/`WHERE` の式では `Some(schema)`、UDF 本体の
/// 展開中は `None` に切り替わりパラメータ参照だけを解決する）と、パラメータ名 →
/// 既に束縛済みの実引数式（インライン展開用）の対応表。
pub(super) struct BindEnv<'a> {
    schema: Option<&'a TableSchema>,
    params: std::collections::HashMap<String, (BoundExpr, ExprType)>,
    registry: &'a UdfRegistry,
    /// `CASE`／`COALESCE`／`NULLIF` の現在の入れ子段数（対象ビヘイビア: SQL-26。
    /// Issue #921）。構文段（`sql::allowlist::Parser`）のネストカウンタとは別に
    /// 束縛段でも独立に検査する: UDF 本体をインライン展開すると、呼び出し元の
    /// ネストと呼び出し先本体のネストが合算され、構文段の計測値（各文が個別に
    /// 見た字面上のネスト）をすり抜けうるため（[`enter_case_nesting`] 参照）。
    case_nesting: usize,
    /// 投影モード（Issue #1539・SQL-42）。`true` のとき、`+ - * /` の両オペランドが
    /// 整数／単精度と判定できれば [`BuiltinFn::TypedArith`] で束縛し、結果型を
    /// PostgreSQL に揃える。`SELECT` 投影項目と `FROM` なし SELECT の入口
    /// （[`bind_projection_expr`]・[`bind_const_expr`]）だけが立て、`WHERE`／
    /// `ORDER BY`／`CHECK`／UDF 本体は従来の f64 意味論のまま（`false`）。
    typed_arith: bool,
    /// 読み取り文の入口が確定した時刻（SQL-37・Issue #1586。[`time_fn::current_clock`]）。
    /// `None` の文脈では時刻関数を `0A000` で拒否する。UDF 本体の展開（`inner_env`）は
    /// 呼び出し側の値を継承する。
    clock: Option<StatementClock>,
}

/// 展開済み [`BoundExpr`] のノード数を数える。UDF 連鎖のパラメータ参照展開時に
/// クローンする部分木のサイズを `node_budget` へ課金するために使う
/// （`bind_expr_in` の `Expr::Ident` 分岐を参照）。木の深さは束縛段で既に
/// `node_budget` により上限が掛かっているため、単純な再帰で数え上げてよい。
/// 行 `id`（`u64`）と整数 `value` を `i128` で厳密比較する（`BoundExpr::IdCompare` の
/// 評価本体。`udf_call::eval` と `sql::expr_program` の両評価器が共有する）。比較
/// 演算子以外（算術等）は束縛段が生成しないため、到達しても偽（fail-closed）とする。
pub(crate) fn id_compare(op: BinOp, id: u64, value: i128) -> bool {
    let id = i128::from(id);
    match op {
        BinOp::Eq => id == value,
        BinOp::Lt => id < value,
        BinOp::Le => id <= value,
        BinOp::Gt => id > value,
        BinOp::Ge => id >= value,
        _ => false,
    }
}

fn count_bound_nodes(expr: &BoundExpr) -> usize {
    match expr {
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::TextColumnRef { .. }
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::Time(_)
        | BoundExpr::DateColumnRef { .. }
        | BoundExpr::TimestampColumnRef { .. }
        | BoundExpr::TimeColumnRef { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::IdRef
        | BoundExpr::IdCompare { .. }
        | BoundExpr::VectorRef
        | BoundExpr::JsonbColumnRef { .. }
        | BoundExpr::JsonColumnRef { .. }
        | BoundExpr::JsonbLiteral(_)
        | BoundExpr::Null => 1,
        BoundExpr::JsonAccess { lhs, rhs, .. } => {
            1 + count_bound_nodes(lhs)
                + match rhs {
                    JsonAccessRhs::Key(e) | JsonAccessRhs::Index(e) => count_bound_nodes(e),
                    JsonAccessRhs::Path(p) => p.len(),
                }
        }
        BoundExpr::JsonPredicate { lhs, rhs, .. } => {
            1 + count_bound_nodes(lhs)
                + match rhs {
                    JsonPredicateRhs::Keys(k) => k.len(),
                    other => other.expr().map_or(0, count_bound_nodes),
                }
        }
        BoundExpr::JsonTextFn(f) => 1 + f.args.iter().map(count_bound_nodes).sum::<usize>(),
        BoundExpr::JsonModify { lhs, rhs, .. } => {
            1 + count_bound_nodes(lhs)
                + match rhs {
                    JsonModifyRhs::Path(p) => p.len(),
                    other => other.expr().map_or(0, count_bound_nodes),
                }
        }
        BoundExpr::Builtin { args, .. } => 1 + args.iter().map(count_bound_nodes).sum::<usize>(),
        BoundExpr::Binary { lhs, rhs, .. } => 1 + count_bound_nodes(lhs) + count_bound_nodes(rhs),
        BoundExpr::WasmCall { args, .. } | BoundExpr::JsonFn { args, .. } => {
            1 + args.iter().map(count_bound_nodes).sum::<usize>()
        }
        BoundExpr::Case { whens, else_result } => {
            1 + whens
                .iter()
                .map(|(c, r)| count_bound_nodes(c) + count_bound_nodes(r))
                .sum::<usize>()
                + count_bound_nodes(else_result)
        }
        BoundExpr::Coalesce(args) => 1 + args.iter().map(count_bound_nodes).sum::<usize>(),
        BoundExpr::NullIf { lhs, rhs } => 1 + count_bound_nodes(lhs) + count_bound_nodes(rhs),
    }
}

/// 既に束縛済みの `BoundExpr` 部分木が内部に持つ `CASE`／`COALESCE`／`NULLIF` の
/// 最大入れ子段数を数える（対象ビヘイビア: SQL-26。Issue #921 レビュー指摘対応）。
///
/// `BindEnv::case_nesting` は束縛「実行中」の構文木を辿る間だけ有効なカウンタで
/// あり、UDF 呼び出しの実引数はそれ自身の呼び出し元コンテキストで一度束縛
/// し終えた時点でカウンタが呼び出し前の段数へ戻る（[`bind_with_case_nesting`]）。
/// そのため `bind_call` が実引数を束縛済み `BoundExpr` として `env.params` へ
/// 格納したあと、UDF 本体側で `Expr::Ident`（パラメータ参照）を介してその部分木を
/// まるごと展開すると、呼び出し元での「実引数単体としては上限内」だった入れ子と
/// 展開先（本体側で既に `CASE`/`COALESCE`/`NULLIF` の内側にいる場合の現在段数）の
/// 入れ子が合算され、どちらの計測時点でも `MAX_CASE_NESTING` 超過として観測され
/// ないまま実効ネストだけが上限を超えてすり抜けうる。この関数で展開対象の部分木
/// が持つ最大追加段数を求め、`bind_expr_in` の `Expr::Ident` 分岐で
/// `env.case_nesting`（展開先での現在段数）に加算した和を検査することで、
/// UDF 引数展開後の実効ネストを直接検査する。
fn max_bound_case_nesting(expr: &BoundExpr) -> usize {
    match expr {
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::TextColumnRef { .. }
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::Time(_)
        | BoundExpr::DateColumnRef { .. }
        | BoundExpr::TimestampColumnRef { .. }
        | BoundExpr::TimeColumnRef { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::IdRef
        | BoundExpr::IdCompare { .. }
        | BoundExpr::VectorRef
        | BoundExpr::JsonbColumnRef { .. }
        | BoundExpr::JsonColumnRef { .. }
        | BoundExpr::JsonbLiteral(_)
        | BoundExpr::Null => 0,
        BoundExpr::JsonAccess { lhs, rhs, .. } => {
            let r = match rhs {
                JsonAccessRhs::Key(e) | JsonAccessRhs::Index(e) => max_bound_case_nesting(e),
                JsonAccessRhs::Path(_) => 0,
            };
            max_bound_case_nesting(lhs).max(r)
        }
        BoundExpr::JsonPredicate { lhs, rhs, .. } => {
            max_bound_case_nesting(lhs).max(rhs.expr().map_or(0, max_bound_case_nesting))
        }
        BoundExpr::JsonTextFn(f) => f.args.iter().map(max_bound_case_nesting).max().unwrap_or(0),
        BoundExpr::JsonModify { lhs, rhs, .. } => {
            max_bound_case_nesting(lhs).max(rhs.expr().map_or(0, max_bound_case_nesting))
        }
        BoundExpr::Builtin { args, .. } => {
            args.iter().map(max_bound_case_nesting).max().unwrap_or(0)
        }
        BoundExpr::Binary { lhs, rhs, .. } => {
            max_bound_case_nesting(lhs).max(max_bound_case_nesting(rhs))
        }
        BoundExpr::WasmCall { args, .. } | BoundExpr::JsonFn { args, .. } => {
            args.iter().map(max_bound_case_nesting).max().unwrap_or(0)
        }
        BoundExpr::Case { whens, else_result } => {
            let branches_max = whens
                .iter()
                .map(|(c, r)| max_bound_case_nesting(c).max(max_bound_case_nesting(r)))
                .max()
                .unwrap_or(0);
            1 + branches_max.max(max_bound_case_nesting(else_result))
        }
        BoundExpr::Coalesce(args) => 1 + args.iter().map(max_bound_case_nesting).max().unwrap_or(0),
        BoundExpr::NullIf { lhs, rhs } => {
            1 + max_bound_case_nesting(lhs).max(max_bound_case_nesting(rhs))
        }
    }
}

/// 式木が `VECTOR` 列（[`BoundExpr::VectorRef`]）へ到達するかを判定する
/// （Issue #350: 集計経路が `embedding` を実際にデコードすべきかの判定基盤）。
/// `embedding` アクセスは束縛段で `VectorRef` に一元化されている（本モジュールの
/// [`eval`] 参照。`Builtin`/`Binary`/`WasmCall` は自身では embedding を持たず
/// 引数式の評価結果のみを使う）ため、全 variant を網羅する `match`（`_` 禁止）で
/// 判定すれば過小評価が起きない。呼び出し元
/// （`sql::aggregate.rs`／`sql::group_by.rs`）はこの判定結果に基づき embedding の
/// デコードそのものをスキップするため、将来 `BoundExpr` に新 variant が
/// 追加された際にここへの追随漏れがあれば早期にコンパイルエラーとして検出できる
/// ことを意図して非網羅を許さない。
pub(crate) fn references_embedding(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::VectorRef => true,
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::TextColumnRef { .. }
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::Time(_)
        | BoundExpr::DateColumnRef { .. }
        | BoundExpr::TimestampColumnRef { .. }
        | BoundExpr::TimeColumnRef { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::IdRef
        | BoundExpr::IdCompare { .. }
        | BoundExpr::JsonbColumnRef { .. }
        | BoundExpr::JsonColumnRef { .. }
        | BoundExpr::JsonbLiteral(_)
        | BoundExpr::Null => false,
        BoundExpr::JsonAccess { lhs, rhs, .. } => {
            references_embedding(lhs)
                || match rhs {
                    JsonAccessRhs::Key(e) | JsonAccessRhs::Index(e) => references_embedding(e),
                    JsonAccessRhs::Path(_) => false,
                }
        }
        BoundExpr::JsonPredicate { lhs, rhs, .. } => {
            references_embedding(lhs) || rhs.expr().is_some_and(references_embedding)
        }
        BoundExpr::JsonTextFn(f) => f.args.iter().any(references_embedding),
        BoundExpr::JsonModify { lhs, rhs, .. } => {
            references_embedding(lhs) || rhs.expr().is_some_and(references_embedding)
        }
        BoundExpr::Builtin { args, .. } => args.iter().any(references_embedding),
        BoundExpr::Binary { lhs, rhs, .. } => {
            references_embedding(lhs) || references_embedding(rhs)
        }
        BoundExpr::WasmCall { args, .. } | BoundExpr::JsonFn { args, .. } => {
            args.iter().any(references_embedding)
        }
        BoundExpr::Case { whens, else_result } => {
            whens
                .iter()
                .any(|(c, r)| references_embedding(c) || references_embedding(r))
                || references_embedding(else_result)
        }
        BoundExpr::Coalesce(args) => args.iter().any(references_embedding),
        BoundExpr::NullIf { lhs, rhs } => references_embedding(lhs) || references_embedding(rhs),
    }
}

/// [`mark_referenced_scalar_columns`] のクロージャ版（`sql::where_tree` の
/// `BoundOrGroup::visit_column_indices` のように、事前に `schema.columns.len()`
/// を知らずマスク配列を確保できない呼び出し元向け）。式木が参照する `TEXT` 列
/// インデックスをすべて `visit` へ渡す。
pub(crate) fn visit_referenced_scalar_columns(expr: &BoundExpr, visit: &mut dyn FnMut(usize)) {
    match expr {
        BoundExpr::TextColumnRef { index }
        | BoundExpr::DateColumnRef { index }
        | BoundExpr::TimestampColumnRef { index }
        | BoundExpr::TimeColumnRef { index }
        | BoundExpr::JsonbColumnRef { index }
        | BoundExpr::JsonColumnRef { index }
        | BoundExpr::ColumnRef { index } => visit(*index),
        BoundExpr::JsonAccess { lhs, rhs, .. } => {
            visit_referenced_scalar_columns(lhs, visit);
            if let JsonAccessRhs::Key(e) | JsonAccessRhs::Index(e) = rhs {
                visit_referenced_scalar_columns(e, visit);
            }
        }
        BoundExpr::JsonPredicate { lhs, rhs, .. } => {
            visit_referenced_scalar_columns(lhs, visit);
            if let Some(e) = rhs.expr() {
                visit_referenced_scalar_columns(e, visit);
            }
        }
        BoundExpr::JsonModify { lhs, rhs, .. } => {
            visit_referenced_scalar_columns(lhs, visit);
            if let Some(e) = rhs.expr() {
                visit_referenced_scalar_columns(e, visit);
            }
        }
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::Time(_)
        | BoundExpr::IdRef
        | BoundExpr::IdCompare { .. }
        | BoundExpr::VectorRef
        | BoundExpr::JsonbLiteral(_)
        | BoundExpr::Null => {}
        BoundExpr::JsonTextFn(f) => {
            for a in &f.args {
                visit_referenced_scalar_columns(a, visit);
            }
        }
        BoundExpr::Builtin { args, .. }
        | BoundExpr::WasmCall { args, .. }
        | BoundExpr::JsonFn { args, .. } => {
            for a in args {
                visit_referenced_scalar_columns(a, visit);
            }
        }
        BoundExpr::Binary { lhs, rhs, .. } => {
            visit_referenced_scalar_columns(lhs, visit);
            visit_referenced_scalar_columns(rhs, visit);
        }
        // Issue #919・SQL-26 と Issue #921・SQL-26 の合流点: `CASE`／`COALESCE`／
        // `NULLIF` の分岐にも `TEXT` 列参照（`COALESCE(LOWER(text_col), 'x')` 等）が
        // 現れうるため、各分岐へ再帰する（取りこぼすとマスク外参照＝実 NULL の
        // 誤判定になる fail-closed 違反。§3-7 の他 variant と同じ契約）。
        BoundExpr::Case { whens, else_result } => {
            for (c, r) in whens {
                visit_referenced_scalar_columns(c, visit);
                visit_referenced_scalar_columns(r, visit);
            }
            visit_referenced_scalar_columns(else_result, visit);
        }
        BoundExpr::Coalesce(args) => {
            for a in args {
                visit_referenced_scalar_columns(a, visit);
            }
        }
        BoundExpr::NullIf { lhs, rhs } => {
            visit_referenced_scalar_columns(lhs, visit);
            visit_referenced_scalar_columns(rhs, visit);
        }
    }
}

/// 式木が参照する `TEXT` 列（[`BoundExpr::TextColumnRef`]）を `mask` へ反映する
/// （Issue #919・§3-7。`sql::aggregate::ReferencedColumns::derive`・`sql::scan`
/// の `decode_tier_for`・`sql::exec` の `needed_column_indices` 計算・
/// `sql::check_constraint` の列マスクが共有する）。戻り値は 1 つでも `TEXT` 列を
/// 参照すれば `true`（[`references_embedding`] と同じ「呼び出し元がデコード
/// tier・延期投影の判定に使う直接シグナル」の位置づけ）。`mask` への反映が
/// `get_mut` の範囲外判定で無視された場合でも、この戻り値を別途
/// `has_scalar_reference` 相当のフラグへ反映することで `DecodeTier::Fast` の
/// 誤選択・`defer_projection` の誤った真化を防ぐ（呼び出し元の責務。
/// security.md「不安全な設計」対応）。`BoundExpr` の全 variant を網羅する
/// `match`（`_` 禁止）とし、新 variant 追加時にここへの追随漏れを
/// コンパイルエラーとして検出する（`references_embedding` と同じ設計意図）。
pub(crate) fn mark_referenced_scalar_columns(expr: &BoundExpr, mask: &mut [bool]) -> bool {
    match expr {
        BoundExpr::TextColumnRef { index }
        | BoundExpr::DateColumnRef { index }
        | BoundExpr::TimestampColumnRef { index }
        | BoundExpr::TimeColumnRef { index }
        | BoundExpr::JsonbColumnRef { index }
        | BoundExpr::JsonColumnRef { index }
        | BoundExpr::ColumnRef { index } => {
            if let Some(slot) = mask.get_mut(*index) {
                *slot = true;
            }
            true
        }
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::Time(_)
        | BoundExpr::IdRef
        | BoundExpr::IdCompare { .. }
        | BoundExpr::VectorRef
        | BoundExpr::JsonbLiteral(_)
        | BoundExpr::Null => false,
        // 左辺の jsonb 列と、キー／添字の式が参照する列を両方マスクへ反映する
        // （取りこぼすとマスク外参照＝実 NULL の誤判定になるため全て走査する）。
        BoundExpr::JsonAccess { lhs, rhs, .. } => {
            let mut any = mark_referenced_scalar_columns(lhs, mask);
            if let JsonAccessRhs::Key(e) | JsonAccessRhs::Index(e) = rhs {
                if mark_referenced_scalar_columns(e, mask) {
                    any = true;
                }
            }
            any
        }
        // 包含・存在演算子（Issue #1598）。左辺・右辺の式が参照する列を両方反映する。
        BoundExpr::JsonPredicate { lhs, rhs, .. } => {
            let mut any = mark_referenced_scalar_columns(lhs, mask);
            if let Some(e) = rhs.expr() {
                if mark_referenced_scalar_columns(e, mask) {
                    any = true;
                }
            }
            any
        }
        // 連結・削除演算子（Issue #2181）。左辺・右辺の式が参照する列を両方反映する。
        BoundExpr::JsonModify { lhs, rhs, .. } => {
            let mut any = mark_referenced_scalar_columns(lhs, mask);
            if let Some(e) = rhs.expr() {
                if mark_referenced_scalar_columns(e, mask) {
                    any = true;
                }
            }
            any
        }
        // `any`/`fold` は短絡評価となり、先頭の一致以降の引数をマークし損ねる
        // （複数引数が別々の TEXT 列を参照しうる。例: `concat(a, b)`）ため、
        // 明示ループで全引数を必ず走査する（`references_embedding` の `any`
        // 使用とは異なり、ここでは「1 つでも該当するか」だけでなく「該当する
        // 添字をすべて `mask` へ反映する」副作用が主目的）。
        BoundExpr::JsonTextFn(f) => {
            let mut any = false;
            for a in &f.args {
                if mark_referenced_scalar_columns(a, mask) {
                    any = true;
                }
            }
            any
        }
        BoundExpr::Builtin { args, .. }
        | BoundExpr::WasmCall { args, .. }
        | BoundExpr::JsonFn { args, .. } => {
            let mut any = false;
            for a in args {
                if mark_referenced_scalar_columns(a, mask) {
                    any = true;
                }
            }
            any
        }
        BoundExpr::Binary { lhs, rhs, .. } => {
            let l = mark_referenced_scalar_columns(lhs, mask);
            let r = mark_referenced_scalar_columns(rhs, mask);
            l || r
        }
        // Issue #919・SQL-26 と Issue #921・SQL-26 の合流点（`visit_referenced_
        // scalar_columns` と同じ理由。`COALESCE(LOWER(text_col), 'x')` 等）。
        BoundExpr::Case { whens, else_result } => {
            let mut any = false;
            for (c, r) in whens {
                if mark_referenced_scalar_columns(c, mask) {
                    any = true;
                }
                if mark_referenced_scalar_columns(r, mask) {
                    any = true;
                }
            }
            if mark_referenced_scalar_columns(else_result, mask) {
                any = true;
            }
            any
        }
        BoundExpr::Coalesce(args) => {
            let mut any = false;
            for a in args {
                if mark_referenced_scalar_columns(a, mask) {
                    any = true;
                }
            }
            any
        }
        BoundExpr::NullIf { lhs, rhs } => {
            let l = mark_referenced_scalar_columns(lhs, mask);
            let r = mark_referenced_scalar_columns(rhs, mask);
            l || r
        }
    }
}

thread_local! {
    /// CHECK 述語の束縛中（[`CheckBindingGuard`] が生きている間）だけ `true`。構文段が型を知らず
    /// 拒否できない jsonb の `-`（[`bind_json_delete`]）を CHECK 本体で `0A000` にするために使う。
    static CHECK_BINDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// CHECK 述語の束縛区間を示す RAII ガード（`sql::parser::bind_check_predicates` が張る）。
/// drop（panic 時を含む）で直前の値へ戻す。
pub(crate) struct CheckBindingGuard(bool);

impl CheckBindingGuard {
    pub(crate) fn enter() -> Self {
        Self(CHECK_BINDING.with(|c| c.replace(true)))
    }
}

impl Drop for CheckBindingGuard {
    fn drop(&mut self) {
        CHECK_BINDING.with(|c| c.set(self.0));
    }
}

/// [`Expr`] を意味論的に束縛する（`sql::parser::bind_in_session` から呼ばれる公開 API）。
/// 列参照は `schema` から、UDF 呼び出しは `registry` から解決し、UDF はインライン
/// 展開して自己完結した [`BoundExpr`] を返す。`node_budget` は展開後のノード数上限
/// （[`MAX_EXPR_NODES`]）を、呼び出し全体（1 つの `SELECT`/`WHERE` 式項目）で共有する。
pub fn bind_expr(
    expr: &Expr,
    schema: &TableSchema,
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let mut env = BindEnv {
        schema: Some(schema),
        params: std::collections::HashMap::new(),
        registry,
        case_nesting: 0,
        typed_arith: false,
        clock: time_fn::current_clock(),
    };
    bind_expr_in(expr, &mut env, node_budget)
}

/// 投影項目の式を束縛し、結果の列型も返す（Issue #1539・SQL-42。`sql::parser::
/// bind_projection` の `SelectItem::Expr` 分岐から呼ばれる）。[`bind_expr`] との違いは
/// 投影モード（[`BindEnv::typed_arith`]）で束縛する点と、第 3 要素として RowDescription
/// に載せる結果列型（`None` は型を公告できない VECTOR）を返す点。
pub(crate) fn bind_projection_expr(
    expr: &Expr,
    schema: &TableSchema,
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType, Option<ColumnType>), SqlSurfaceError> {
    let r = bind_projection_expr_json_ok(expr, schema, registry, node_budget)?;
    reject_json_operand(r.1)?;
    Ok(r)
}

/// [`bind_projection_expr`] の jsonb 結果許容版（Issue #1596）。投影項目
/// （`sql::parser::bind_projection`）だけが使い、HAVING／ORDER BY 等の他の呼び出し元は
/// jsonb 結果を `0A000` で拒否する [`bind_projection_expr`] を使う。
pub(crate) fn bind_projection_expr_json_ok(
    expr: &Expr,
    schema: &TableSchema,
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType, Option<ColumnType>), SqlSurfaceError> {
    let mut env = BindEnv {
        schema: Some(schema),
        params: std::collections::HashMap::new(),
        registry,
        case_nesting: 0,
        typed_arith: true,
        clock: time_fn::current_clock(),
    };
    let (bound, ty) = bind_expr_any(expr, &mut env, node_budget)?;
    let col = expr_result_column_type(expr, &bound, ty);
    Ok((bound, ty, col))
}

/// ORDER BY の式キーを束縛する（Issue #1597・TABLE-25）。[`bind_expr`] との違いは jsonb
/// 結果（パス演算子・jsonb 列）を許す点（並べ替えは jsonb のソートキーで行う）。それ以外の
/// 位置の拒否規則は [`bind_expr`] と同じ。
pub(crate) fn bind_order_expr(
    expr: &Expr,
    schema: &TableSchema,
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let mut env = BindEnv {
        schema: Some(schema),
        params: std::collections::HashMap::new(),
        registry,
        case_nesting: 0,
        typed_arith: false,
        clock: time_fn::current_clock(),
    };
    bind_expr_any(expr, &mut env, node_budget)
}

/// 束縛済み式の結果列型（RowDescription の型 OID の元）を決める唯一の場所
/// （Issue #1539・SQL-42）。型付き算術の根は `Integer`／`BigInt`／`Real`、整数リテラル
/// 単独は `Integer`／`BigInt`、それ以外は [`ExprType`] の既存写像
/// （`Scalar` → `Double`、`Bool` → `Boolean`、`Text` → `Text`、`Date`、`Timestamp`、
/// `Vector` → `None`）。
pub(crate) fn expr_result_column_type(
    expr: &Expr,
    bound: &BoundExpr,
    ty: ExprType,
) -> Option<ColumnType> {
    // `jsonb_array_length` は int4（PostgreSQL と同じ。Issue #1599）。
    if matches!(
        bound,
        BoundExpr::JsonFn {
            f: JsonFnKind::ArrayLength,
            ..
        }
    ) {
        return Some(ColumnType::Integer);
    }
    if let BoundExpr::Builtin {
        f: BuiltinFn::TypedArith(_, kind),
        ..
    } = bound
    {
        return Some(match kind {
            NumericKind::Int2 => ColumnType::SmallInt,
            NumericKind::Int4 => ColumnType::Integer,
            NumericKind::Int8 => ColumnType::BigInt,
            NumericKind::Float4 => ColumnType::Real,
        });
    }
    if let Expr::Number(raw) = expr {
        if let Some((_, kind)) = classify_projection_integer_literal(raw) {
            return Some(match kind {
                NumericKind::Int8 => ColumnType::BigInt,
                _ => ColumnType::Integer,
            });
        }
    }
    // `json_array_length` は int4（Issue #2180）。
    if let BoundExpr::JsonTextFn(f) = bound {
        if f.result_is_int4() {
            return Some(ColumnType::Integer);
        }
    }
    column_type_for_expr_type(ty)
}

/// [`bind_const_expr`] の投影項目版（Issue #1599）。`FROM` なし SELECT の投影項目は jsonb
/// 結果（`jsonb_build_object` 等）を返せるため、jsonb を拒否しない束縛を使う。集計の引数・
/// `WHERE`・`INSERT ... VALUES`（`sql::const_expr`）は従来どおり [`bind_const_expr`]
/// （jsonb を `0A000` で拒否）を使う。
pub(crate) fn bind_const_projection_expr(
    expr: &Expr,
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType, Option<ColumnType>), SqlSurfaceError> {
    let mut env = BindEnv {
        schema: None,
        params: std::collections::HashMap::new(),
        registry,
        case_nesting: 0,
        typed_arith: true,
        clock: time_fn::current_clock(),
    };
    let (bound, ty) = bind_expr_any(expr, &mut env, node_budget)?;
    let col = expr_result_column_type(expr, &bound, ty);
    Ok((bound, ty, col))
}

/// [`ExprType`] から結果列型への既存写像（`sql::exec::computed_column_meta` と共有）。
pub(crate) fn column_type_for_expr_type(ty: ExprType) -> Option<ColumnType> {
    match ty {
        ExprType::Scalar => Some(ColumnType::Double),
        ExprType::Bool => Some(ColumnType::Boolean),
        ExprType::Text => Some(ColumnType::Text),
        ExprType::Date => Some(ColumnType::Date),
        ExprType::Timestamp => Some(ColumnType::Timestamp),
        ExprType::Time => Some(ColumnType::Time),
        ExprType::Json => Some(ColumnType::Jsonb),
        ExprType::JsonText => Some(ColumnType::Json),
        ExprType::Vector => None,
    }
}

/// 整数形の数値リテラル（先頭 `-` 任意＋数字のみ）を i64 と最小の整数型
/// （i32 に収まれば `Int4`、そうでなければ `Int8`）へ分類する。整数形でない・
/// i64 を超える場合は `None`。`sql::const_select` のリテラル型付けと投影の
/// 型付き算術が同じ判定を共有する（第 2 の実装を作らない。Issue #1539）。
pub(crate) fn classify_integer_literal(raw: &str) -> Option<(i64, NumericKind)> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = raw.parse::<i64>().ok()?;
    let kind = if i32::try_from(v).is_ok() {
        NumericKind::Int4
    } else {
        NumericKind::Int8
    };
    Some((v, kind))
}

/// 投影の型付き算術・結果型で使う整数リテラル分類。[`classify_integer_literal`] に加え、
/// 投影の評価値は f64 で運ばれ 2^53 を超える整数を厳密に保てないため、|v| > 2^53 の
/// リテラルは整数扱いにせず従来どおり倍精度（`None`）へ落とす（丸めて誤値を返さない。
/// 既知の PG 差分として文書化。Issue #1539）。
fn classify_projection_integer_literal(raw: &str) -> Option<(i64, NumericKind)> {
    let (v, kind) = classify_integer_literal(raw)?;
    (v.unsigned_abs() <= TYPED_INT_EXACT_MAX as u64).then_some((v, kind))
}

/// 列参照を持たない文脈（`FROM` なし定数式 SELECT。SQL-43・Issue #1531）で式を束縛する。
/// スキーマなしの [`BindEnv`] で [`bind_expr_in`] を呼ぶ薄いラッパーで、`sql::const_select`
/// から呼ばれる。列参照は呼び出し元が事前に拒否する前提だが、万一残っても
/// スキーマ未解決として束縛段が拒否する（fail-closed）。
pub(crate) fn bind_const_expr(
    expr: &Expr,
    registry: &UdfRegistry,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType, Option<ColumnType>), SqlSurfaceError> {
    // Issue #1539・SQL-42: `FROM` なし SELECT も投影と同じ型規則（整数算術は int4／int8）。
    let mut env = BindEnv {
        schema: None,
        params: std::collections::HashMap::new(),
        registry,
        case_nesting: 0,
        typed_arith: true,
        clock: time_fn::current_clock(),
    };
    let (bound, ty) = bind_expr_in(expr, &mut env, node_budget)?;
    let col = expr_result_column_type(expr, &bound, ty);
    Ok((bound, ty, col))
}

/// [`BindEnv::case_nesting`] を 1 段進め、[`MAX_CASE_NESTING`] を超えないか検査する
/// （`bind_case`／`bind_coalesce`／`bind_nullif` が共有する。対象ビヘイビア:
/// SQL-26。Issue #921）。呼び出し元は対応する `exit_case_nesting` を必ず対で呼ぶ
/// （[`bind_with_case_nesting`] 参照）。
fn enter_case_nesting(env: &mut BindEnv<'_>) -> Result<(), SqlSurfaceError> {
    let next = env.case_nesting.checked_add(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("CASE/COALESCE/NULLIF nesting exceeds the allowed depth")
    })?;
    if next > MAX_CASE_NESTING {
        return Err(SqlSurfaceError::payload_too_large(
            "CASE/COALESCE/NULLIF nesting exceeds the allowed depth",
        ));
    }
    env.case_nesting = next;
    Ok(())
}

/// `f` を [`BindEnv::case_nesting`] を 1 段進めた状態で実行し、成否によらず
/// 呼び出し前の段数へ戻す（`bind_case`／`bind_coalesce`／`bind_nullif` が共有する）。
fn bind_with_case_nesting<F>(
    env: &mut BindEnv<'_>,
    f: F,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError>
where
    F: FnOnce(&mut BindEnv<'_>) -> Result<(BoundExpr, ExprType), SqlSurfaceError>,
{
    enter_case_nesting(env)?;
    let result = f(env);
    env.case_nesting = env.case_nesting.saturating_sub(1);
    result
}

/// `CASE` の THEN／ELSE・`COALESCE` の引数を束縛する（対象ビヘイビア: SQL-26）。
/// `Expr::Null` はここでのみ特別扱いし、型が未確定のまま [`BoundExpr::Null`] を
/// 返す（`Ok` 側の `None` が「型未確定」を表す）。呼び出し元がすべての兄弟枝の
/// 型を突き合わせて単一化する。
pub(super) fn bind_null_aware(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, Option<ExprType>), SqlSurfaceError> {
    if matches!(expr, Expr::Null) {
        *node_budget = node_budget
            .checked_sub(1)
            .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
        return Ok((BoundExpr::Null, None));
    }
    let (bound, ty) = bind_expr_in(expr, env, node_budget)?;
    Ok((bound, Some(ty)))
}

/// 兄弟枝（`CASE` の THEN/ELSE・`COALESCE` の引数）の型を単一化する。NULL 由来の
/// `None` は無視し、非 NULL 同士の型が食い違えば `42804`（`DatatypeMismatch`）で
/// 拒否する（対象ビヘイビア: SQL-26）。
fn unify_branch_type(
    unified: &mut Option<ExprType>,
    ty: Option<ExprType>,
    mismatch_detail: &str,
) -> Result<(), SqlSurfaceError> {
    let Some(t) = ty else { return Ok(()) };
    match *unified {
        None => *unified = Some(t),
        Some(existing) if existing == t => {}
        Some(_) => {
            return Err(SqlSurfaceError::DatatypeMismatch {
                detail: mismatch_detail.to_string(),
            })
        }
    }
    Ok(())
}

/// 検索形 `CASE` を束縛する（対象ビヘイビア: SQL-26。`sql::allowlist::Parser`
/// が構文段で単純 CASE・WHEN 内の論理演算を拒否済みのため、`cond` は常に
/// 比較の [`Expr::Binary`] である）。
fn bind_case(
    whens: &[(Expr, Expr)],
    else_result: &Option<Box<Expr>>,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if whens.len() > MAX_CASE_BRANCHES {
        return Err(SqlSurfaceError::payload_too_large(
            "CASE has too many WHEN branches",
        ));
    }
    bind_with_case_nesting(env, |env| {
        let mut bound_whens = Vec::with_capacity(whens.len());
        let mut unified: Option<ExprType> = None;
        for (cond, result) in whens {
            let (cond_bound, cond_ty) = bind_expr_in(cond, env, node_budget)?;
            if cond_ty != ExprType::Bool {
                return Err(SqlSurfaceError::DatatypeMismatch {
                    detail: "CASE WHEN condition must be a boolean comparison".to_string(),
                });
            }
            let (result_bound, result_ty) = bind_null_aware(result, env, node_budget)?;
            unify_branch_type(
                &mut unified,
                result_ty,
                "CASE branches must have the same type",
            )?;
            bound_whens.push((cond_bound, result_bound));
        }
        let else_bound = match else_result {
            Some(expr) => {
                let (b, t) = bind_null_aware(expr, env, node_budget)?;
                unify_branch_type(&mut unified, t, "CASE branches must have the same type")?;
                b
            }
            None => BoundExpr::Null,
        };
        let final_ty = unified.ok_or_else(|| SqlSurfaceError::FeatureNotSupported {
            detail: "CASE expression with only NULL results has no determinable type".to_string(),
        })?;
        Ok((
            BoundExpr::Case {
                whens: bound_whens,
                else_result: Box::new(else_bound),
            },
            final_ty,
        ))
    })
}

/// `COALESCE(<arg>, ...)` を束縛する（対象ビヘイビア: SQL-26）。
fn bind_coalesce(
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if args.len() > MAX_CALL_ARGS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many call arguments",
        ));
    }
    bind_with_case_nesting(env, |env| {
        let mut bound_args = Vec::with_capacity(args.len());
        let mut unified: Option<ExprType> = None;
        for a in args {
            let (b, t) = bind_null_aware(a, env, node_budget)?;
            unify_branch_type(
                &mut unified,
                t,
                "COALESCE arguments must have the same type",
            )?;
            bound_args.push(b);
        }
        let final_ty = unified.ok_or_else(|| SqlSurfaceError::FeatureNotSupported {
            detail: "COALESCE with only NULL arguments has no determinable type".to_string(),
        })?;
        Ok((BoundExpr::Coalesce(bound_args), final_ty))
    })
}

/// `NULLIF(<lhs>, <rhs>)` を束縛する（対象ビヘイビア: SQL-26）。両辺は既存の `=`
/// 演算子と同じ型集合（`Scalar`／`Text`／`DATE`／`TIMESTAMP`）を受理する
/// （`Vector`/`Bool` の等価比較は式層に存在しない）。
fn bind_nullif(
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    // codex-review／Cursor Bugbot 指摘対応: この PR で `=` 演算子が
    // `(Text, Text) => Bool`・`(Date/Timestamp, Date/Timestamp) => Bool`
    // （`DATE`⋈`TIMESTAMP` は `DATE` 側を深夜 0 時の `TIMESTAMP` へ暗黙昇格）を
    // 新たに受理するようになった（`bind_binary` 参照）が、`NULLIF(a, b)` は
    // `a = b` と等価な意味論（PostgreSQL 互換。`docs/spec/04-behavior/
    // sql-surface.md` SQL-26 の「PostgreSQL 互換」契約）を持つため、比較演算子と
    // 同じ型の組を受理すべきである。`unify_branch_type` は完全一致の型統一
    // （`CASE` の分岐型統一と共通）しか扱えないため、`DATE`⋈`TIMESTAMP` の
    // 昇格は `bind_binary` の Eq 分岐と同じ規則をここで個別に適用する。
    bind_with_case_nesting(env, |env| {
        let (lhs_b, lhs_t) = bind_null_aware(lhs, env, node_budget)?;
        let (rhs_b, rhs_t) = bind_null_aware(rhs, env, node_budget)?;
        for t in [lhs_t, rhs_t].into_iter().flatten() {
            if !matches!(
                t,
                ExprType::Scalar
                    | ExprType::Text
                    | ExprType::Date
                    | ExprType::Timestamp
                    | ExprType::Time
            ) {
                return Err(SqlSurfaceError::DatatypeMismatch {
                    detail: "NULLIF arguments must be scalar, text, date, timestamp, or time"
                        .to_string(),
                });
            }
        }
        let (lhs_b, rhs_b, result_ty) = match (lhs_t, rhs_t) {
            (Some(ExprType::Date), Some(ExprType::Timestamp)) => {
                (wrap_date_to_timestamp(lhs_b), rhs_b, ExprType::Timestamp)
            }
            (Some(ExprType::Timestamp), Some(ExprType::Date)) => {
                (lhs_b, wrap_date_to_timestamp(rhs_b), ExprType::Timestamp)
            }
            _ => {
                let mut unified: Option<ExprType> = None;
                unify_branch_type(
                    &mut unified,
                    lhs_t,
                    "NULLIF arguments must have the same type",
                )?;
                unify_branch_type(
                    &mut unified,
                    rhs_t,
                    "NULLIF arguments must have the same type",
                )?;
                let ty = unified.ok_or_else(|| SqlSurfaceError::FeatureNotSupported {
                    detail: "NULLIF(NULL, NULL) has no determinable type".to_string(),
                })?;
                (lhs_b, rhs_b, ty)
            }
        };
        Ok((
            BoundExpr::NullIf {
                lhs: Box::new(lhs_b),
                rhs: Box::new(rhs_b),
            },
            result_ty,
        ))
    })
}

fn bind_expr_in(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let (bound, ty) = bind_expr_any(expr, env, node_budget)?;
    reject_json_operand(ty)?;
    Ok((bound, ty))
}

/// jsonb 値（[`ExprType::Json`]）を消費できない位置（算術・関数引数・`CASE`／
/// `COALESCE`／`NULLIF`・集計・代入）へ流れた場合の `0A000`（Issue #1596。比較・ORDER BY・
/// 投影・パス演算子の左辺は Issue #1597 までに対応済みで、それ以外は fail-closed に拒否する）。
fn reject_json_operand(ty: ExprType) -> Result<(), SqlSurfaceError> {
    if ty == ExprType::Json {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "a jsonb value can only be projected, compared, ordered, or used as an operand of a JSON operator or jsonb function"
                .to_string(),
        });
    }
    // `json` 値（Issue #2180）は比較・並べ替えの演算子を持たない（`42883` は比較・
    // ORDER BY・GROUP BY の各束縛が返す）。それ以外の位置は fail-closed に `0A000`。
    if ty == ExprType::JsonText {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "a json value can only be projected or used with a JSON path operator or json function"
                .to_string(),
        });
    }
    Ok(())
}

/// [`bind_expr_in`] の本体。jsonb 型の結果を返しうる（投影・パス演算子の左辺は
/// この関数を直接呼び、それ以外の消費側は [`bind_expr_in`] 経由で jsonb を拒否する）。
pub(super) fn bind_expr_any(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    match expr {
        // 照合指定（TABLE-28・Issue #1609）。照合名は構文段で許可リスト（`C`／`POSIX`）に
        // 解決済み。どちらも既定のバイト列順と同値なので、内側が文字列系（TEXT／VARCHAR／CHAR・
        // 文字列リテラル）であることを検査したうえで内側の `BoundExpr` をそのまま返す
        // （照合を持たない型は `42804`）。`BoundExpr` への照合の保持は #1610・#2190 が担う。
        Expr::Collate {
            expr: inner,
            collation,
        } => {
            let (bound, ty) = bind_expr_any(inner, env, node_budget)?;
            match collation {
                crate::collation::Collation::C | crate::collation::Collation::Posix => {}
            }
            if ty != ExprType::Text {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "collations are not supported by this expression type",
                ));
            }
            Ok((bound, ty))
        }
        Expr::Number(raw) => {
            let v = parse_number_literal(raw)?;
            Ok((BoundExpr::Number(v), ExprType::Scalar))
        }
        Expr::String(s) => {
            // 字句段は既に上限内の長さで 1 トークン化しているが、束縛時にも
            // 行の `TEXT` 列と同じ上限（`MAX_TEXT_FIELD_LEN`）で重ねて検査する
            // （fail-closed。`.claude/rules/security.md`「不安全な設計」対応）。
            if s.len() > crate::row_codec::MAX_TEXT_FIELD_LEN as usize {
                return Err(SqlSurfaceError::payload_too_large(
                    "string literal exceeds the maximum TEXT field length",
                ));
            }
            Ok((BoundExpr::Text(s.clone()), ExprType::Text))
        }
        Expr::Ident(name) => {
            // `env.params`（UDF 本体束縛時のみ非空）のキーは `define_function` で
            // 正規化済み（小文字）。本体の参照側も同じ正規形へそろえて引く
            // （`validate_closed_expr` の参照検証と一貫させる。外側コンテキストでは
            // `env.params` が常に空のため、列名の大文字小文字扱いには影響しない）。
            if let Some((bound, ty)) = env.params.get(name.to_ascii_lowercase().as_str()) {
                // パラメータ参照の展開は、構文上は 1 ノードでも実際には既に展開済みの
                // `BoundExpr` 部分木をまるごとクローンする（UDF 連鎖・多重参照時に
                // 展開結果が指数的に膨張しうる経路）。構文ノード数（直前の
                // `checked_sub(1)`）だけでなく、クローンされる展開後ノード数も
                // `node_budget` へ課金し、`MAX_EXPR_NODES` の「展開後の式ノード数上限」
                // という契約をこの経路でも成立させる（security.md「不安全な設計｜
                // 無制限リソース確保（DoS）」対応）。
                let expanded_size = count_bound_nodes(bound);
                *node_budget = node_budget
                    .checked_sub(expanded_size)
                    .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
                // 実引数（`bound`）は呼び出し元コンテキストで既に単体の入れ子段数
                // 検査を通過済みだが、その段数は展開先（ここに到達した時点の
                // `env.case_nesting`。本体側で既に CASE/COALESCE/NULLIF の内側に
                // いれば正）とは独立に計測されたものであり、単純な `bound.clone()`
                // ではこの 2 つの段数が合算されずすり抜ける（`max_bound_case_nesting`
                // のドキュメンテーションコメント参照。Issue #921 レビュー指摘対応）。
                // 展開後の実効ネストを `env.case_nesting + 実引数内部の最大段数` として
                // 直接検査し、`MAX_CASE_NESTING` 超過を fail-closed に拒否する。
                let expanded_nesting = max_bound_case_nesting(bound);
                let effective_nesting =
                    env.case_nesting
                        .checked_add(expanded_nesting)
                        .ok_or_else(|| {
                            SqlSurfaceError::payload_too_large(
                                "CASE/COALESCE/NULLIF nesting exceeds the allowed depth",
                            )
                        })?;
                if effective_nesting > MAX_CASE_NESTING {
                    return Err(SqlSurfaceError::payload_too_large(
                        "CASE/COALESCE/NULLIF nesting exceeds the allowed depth",
                    ));
                }
                return Ok((bound.clone(), *ty));
            }
            let schema = env.schema.ok_or_else(|| {
                SqlSurfaceError::invalid_input(format!(
                    "column reference is not allowed in a function body: {name}"
                ))
            })?;
            // カタログ上の実カラムを疑似列 `id` より優先して照合する（`parser.rs` の
            // 投影束縛（`Projection::Columns`/`Items` の `SelectItem::Column`
            // 分岐、Issue #56 レビュー指摘対応）と同じ優先順位に揃える。以前は
            // `name == "id"` を先に判定していたため、テーブルが実カラム `id` を
            // 宣言していても式内では常に行キー疑似列を参照してしまい、
            // `SELECT id` と `SELECT id + 1` で参照対象が食い違っていた
            // （codex-review PR #209 指摘）。
            if let Some((index, column)) = schema
                .columns
                .iter()
                .enumerate()
                .find(|(_, c)| &c.name == name)
            {
                return match &column.ty {
                    ColumnType::Vector(_) => {
                        // `BoundExpr::VectorRef` は「1 テーブルにつき `VECTOR` 列は
                        // 高々 1 本」（TABLE-1、`catalog::encode_schema` が
                        // `validate_schema` 経由で CREATE TABLE・ALTER TABLE ADD
                        // COLUMN の双方について fail-closed に強制する）という
                        // 不変条件に依存し、実行時は常に検索対象 embedding スロット
                        // （`arena.vector(slot)`）の値を返す。その不変条件が
                        // 何らかの理由で崩れていた場合に誤った列の値で
                        // 黙って評価するのを防ぐため、ここでも重ねて検査し、
                        // スキーマ中の最初の VECTOR 列以外を参照する式は
                        // fail-closed に拒否する（codex-review PR #209 指摘。
                        // security.md「不安全な設計」対応）。
                        let first_vector_index = schema
                            .columns
                            .iter()
                            .position(|c| matches!(c.ty, ColumnType::Vector(_)));
                        if first_vector_index != Some(index) {
                            return Err(SqlSurfaceError::invalid_input(format!(
                                "column {name:?} is not the table's VECTOR column"
                            )));
                        }
                        Ok((BoundExpr::VectorRef, ExprType::Vector))
                    }
                    // Issue #919・SQL-26（検討中）: TEXT 列参照を解禁し、文字列
                    // スカラー関数（`LOWER`/`UPPER`/`SUBSTR` 等）・TEXT 同士の比較で
                    // 使えるようにする。値は行コンテキスト（`eval_with_scalars` の
                    // `row_scalars`）から解決し、nullable 列の実 NULL は
                    // `ExprValue::Null` として伝播する（AC1・AC2）。
                    ColumnType::Text | ColumnType::Varchar(_) => Ok((BoundExpr::TextColumnRef { index }, ExprType::Text)),
                    // Issue #1183・SQL-24・SQL-26・TABLE-13 ポインタ: 数値 4 型
                    // （INTEGER／BIGINT／REAL／DOUBLE）の列参照を `WHERE`／投影／
                    // `CHECK` で共通に解禁する（#1075 の CHECK 専用 opt-in を撤廃）。
                    // 値は行スカラービュー（`row_scalars`）から解決し、評価時の
                    // 精度超過（BIGINT の |v| > 2^53）は `22003` で fail-closed。
                    // SMALLINT は INTEGER と値表現が同一（`ScalarRef::Integer`）のため同じ腕で
                    // 解禁する（Issue #1559・TABLE-30 ポインタ）。
                    ColumnType::SmallInt
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Real
                    | ColumnType::Double => Ok((BoundExpr::ColumnRef { index }, ExprType::Scalar)),
                    // `CHAR(n)` の式内参照は TEXT へ変換した値（末尾の半角空白を除く）として
                    // 扱う（PostgreSQL の bpchar→text 変換と同じ。TABLE-31・Issue #1566）。
                    ColumnType::Bpchar(_) => Ok((
                        BoundExpr::Builtin {
                            f: BuiltinFn::BpcharToText,
                            args: vec![BoundExpr::TextColumnRef { index }],
                        },
                        ExprType::Text,
                    )),
                    ColumnType::Boolean => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (BOOLEAN columns are not supported)"
                    ))),
                    // 対象ビヘイビア: SQL-26（Issue #920）。`DATE`／`TIMESTAMP`
                    // 列参照を解禁する（TABLE-13・TASK-197、Issue #884 で列型を
                    // 導入して以来、式内参照は本 Issue まで拒否してきた）。値は
                    // 行コンテキスト（`eval_with_scalars` の `row_scalars`）から
                    // 解決し、nullable 列の実 NULL は `ExprValue::Null` として
                    // 伝播する（TEXT 列と同じ契約）。
                    ColumnType::Date => Ok((BoundExpr::DateColumnRef { index }, ExprType::Date)),
                    ColumnType::Timestamp => Ok((
                        BoundExpr::TimestampColumnRef { index },
                        ExprType::Timestamp,
                    )),
                    // `TIME` 列の式内参照（Issue #1570。EXTRACT／date_part・比較の前提）。
                    // 値は行スカラービューから解決し、実 NULL は `ExprValue::Null` として伝播する。
                    ColumnType::Time => Ok((BoundExpr::TimeColumnRef { index }, ExprType::Time)),
                    ColumnType::Array(_) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (ARRAY columns are not supported)"
                    ))),
                    ColumnType::Bytea => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (BYTEA columns are not supported)"
                    ))),
                    // Issue #1597・TABLE-25: jsonb 列の裸の参照は式の中で受理する（値は
                    // `ExprType::Json`。比較・ORDER BY・投影以外の位置へ流れた場合は
                    // `reject_json_operand` が `0A000` で拒否する）。`json`（原文保存）列は
                    // 意味論が別 Issue のため従来どおり拒否する。
                    ColumnType::Jsonb => Ok((BoundExpr::JsonbColumnRef { index }, ExprType::Json)),
                    // Issue #2180・TABLE-25: `json`（原文保存）列は `ExprType::JsonText` として
                    // 受理する。消費できるのはパス演算子の左辺・`json_*` 関数・投影だけで、
                    // 比較等は `42883`、算術・CASE 等は `0A000` で拒否する。
                    ColumnType::Json => Ok((BoundExpr::JsonColumnRef { index }, ExprType::JsonText)),
                    ColumnType::Enum(_) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (ENUM columns are not supported)"
                    ))),
                    // 式中の NUMERIC 列参照は対象外（TABLE-13〔検討中〕・
                    // TASK-197、Issue #885。別 Issue #891 の担当）。
                    ColumnType::Numeric { .. } => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (NUMERIC columns are not supported)"
                    ))),
                    // 式中の UUID 列参照は対象外（TABLE-13〔検討中〕・
                    // TASK-197、Issue #887・U9。別 Issue #891 の担当）。
                    ColumnType::Uuid => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (UUID columns are not supported)"
                    ))),
                };
            }
            if name == "id" {
                return Ok((BoundExpr::IdRef, ExprType::Scalar));
            }
            Err(SqlSurfaceError::undefined_column(name))
        }
        Expr::Call { name, args } => bind_call(name, args, env, node_budget),
        Expr::Binary { op, lhs, rhs } => {
            // Issue #2181: 左辺が jsonb の `-` は削除演算子（キー・添字）。それ以外の左辺は
            // 従来の算術経路のまま（既存の算術の挙動を変えない）。
            // AST 形で判定できる左辺（jsonb／json 列・パス／連結／削除演算子）は先に振り分ける。
            // 関数呼び出し（`jsonb_build_object(..)` 等）の左辺は AST からは型が分からないため、
            // 左辺を 1 度だけ束縛して得た [`ExprType`] で振り分ける（`||` の `bind_expr_any` と同方式。
            // jsonb 以外は束縛済みの左辺を算術経路へ引き継ぎ、従来の挙動と fail-closed を保つ）。
            let mut prebound_lhs: Option<(BoundExpr, ExprType)> = None;
            if *op == BinOp::Sub {
                if is_json_expr(lhs, env) {
                    return bind_json_delete(lhs, rhs, env, node_budget);
                }
                let (l, lt) = bind_expr_any(lhs, env, node_budget)?;
                if lt == ExprType::Json {
                    return bind_json_delete_bound(l, rhs, env, node_budget);
                }
                // json（原文保存）の結果（`json_build_object(..)` 等）に `-` は無い。json 列・
                // パス演算子の左辺（`is_json_expr` 側）と同じく 42883 に揃え、算術経路の
                // 42804 へ落とさない。
                if lt == ExprType::JsonText {
                    return Err(SqlSurfaceError::undefined_function(
                        "operator does not exist: the operand types do not match the - operator",
                    ));
                }
                prebound_lhs = Some((l, lt));
            }
            if matches!(
                op,
                BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq
            ) {
                reject_numeric_literal_vs_operatorless_column(lhs, rhs, env)?;
                // Issue #1408・SQL-24: 片側だけが文字列リテラル（PostgreSQL の unknown 型）の
                // 比較は、反対側の型としてリテラルを解釈する。
                match (lhs.as_ref(), rhs.as_ref()) {
                    (other, Expr::String(s)) if !matches!(other, Expr::String(_)) => {
                        let (l, lt) = bind_expr_any(other, env, node_budget)?;
                        let (r, rt) = bind_unknown_literal_against(s, &l, lt, env, node_budget)?;
                        return bind_binary(*op, l, lt, r, rt);
                    }
                    (Expr::String(s), other) if !matches!(other, Expr::String(_)) => {
                        let (r, rt) = bind_expr_any(other, env, node_budget)?;
                        let (l, lt) = bind_unknown_literal_against(s, &r, rt, env, node_budget)?;
                        return bind_binary(*op, l, lt, r, rt);
                    }
                    // Issue #1438・SQL-24: REAL／DOUBLE 列と数値リテラルの比較は float8 同士の
                    // 比較なので 2^53 exactness 判定をかけない（整数列・`id` は従来どおり）。
                    (col, Expr::Number(raw)) if float_column_ident(col, env) => {
                        let (l, lt) = bind_expr_in(col, env, node_budget)?;
                        let (r, rt) = bind_float_number_literal(raw, node_budget)?;
                        return bind_binary(*op, l, lt, r, rt);
                    }
                    (Expr::Number(raw), col) if float_column_ident(col, env) => {
                        let (l, lt) = bind_float_number_literal(raw, node_budget)?;
                        let (r, rt) = bind_expr_in(col, env, node_budget)?;
                        return bind_binary(*op, l, lt, r, rt);
                    }
                    _ => {}
                }
            }
            // Issue #1597: 比較だけは jsonb 結果のオペランドを受理する（算術は
            // 従来どおり `bind_expr_in` が `0A000` で拒否する）。
            let is_comparison = matches!(
                op,
                BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq
            );
            let (l, lt) = if let Some(pre) = prebound_lhs {
                pre
            } else if is_comparison {
                bind_expr_any(lhs, env, node_budget)?
            } else {
                bind_expr_in(lhs, env, node_budget)?
            };
            let (r, rt) = if is_comparison {
                bind_expr_any(rhs, env, node_budget)?
            } else {
                bind_expr_in(rhs, env, node_budget)?
            };
            // Issue #1539・SQL-42: 投影モードの `Scalar` 同士の算術は、オペランドの数値
            // 種別（整数／単精度／倍精度）から結果型を PostgreSQL に揃える。
            if env.typed_arith
                && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
                && lt == ExprType::Scalar
                && rt == ExprType::Scalar
            {
                let lk = numeric_kind(lhs, &l, env);
                let rk = numeric_kind(rhs, &r, env);
                if let Some(kind) = combine_numeric_kind(lk, rk) {
                    return Ok((
                        BoundExpr::Builtin {
                            f: BuiltinFn::TypedArith(*op, kind),
                            args: vec![l, r],
                        },
                        ExprType::Scalar,
                    ));
                }
            }
            bind_binary(*op, l, lt, r, rt)
        }
        // `Expr::Null` は `CASE` の THEN/ELSE・`COALESCE`／`NULLIF` の引数の 3 箇所
        // でのみ [`bind_null_aware`] 経由で受理する。それ以外の位置（ここに直接
        // 到達する裸の `NULL`）は型を決められない形として `0A000` で拒否する
        // （対象ビヘイビア: SQL-26）。
        Expr::Null => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "NULL is only allowed as a CASE/COALESCE/NULLIF operand".to_string(),
        }),
        Expr::Case { whens, else_result } => bind_case(whens, else_result, env, node_budget),
        Expr::Coalesce(args) => bind_coalesce(args, env, node_budget),
        Expr::NullIf(lhs, rhs) => bind_nullif(lhs, rhs, env, node_budget),
        // `DATE`／`TIMESTAMP` 型付きリテラル（対象ビヘイビア: SQL-26。Issue #920）。
        // 構文段（`allowlist::Parser::parse_primary_expr`）が既に
        // `crate::datetime::parse_date`／`parse_timestamp` で解析・範囲検証済みの
        // 値を保持しているため、束縛段では単に値を引き継ぐだけでよい。
        Expr::DateLiteral(days) => Ok((BoundExpr::Date(*days), ExprType::Date)),
        Expr::TimestampLiteral(micros) => Ok((BoundExpr::Timestamp(*micros), ExprType::Timestamp)),
        Expr::TimeLiteral(micros) => Ok((BoundExpr::Time(*micros), ExprType::Time)),
        Expr::JsonAccess { op, lhs, rhs } => bind_json_access(*op, lhs, rhs, env, node_budget),
        Expr::ValueFunction { kind, precision } => bind_value_function(*kind, *precision, env),
    }
}

/// 時刻関数を束縛する文脈の確定時刻を取り出す。`None`（読み取り文の入口が時計を確定していない
/// 文脈。INSERT／UPDATE／DELETE／COPY／CHECK／DEFAULT／VIEW 本文等）は `0A000` で拒否する
/// （SQL-37・Issue #1586。許可リスト型の fail-closed）。
fn require_clock(env: &BindEnv<'_>) -> Result<StatementClock, SqlSurfaceError> {
    // 分割実行 DML が「時刻値関数を含む文か」を判定するための印（`time_fn::ClockScope::was_used`）。
    time_fn::mark_clock_used();
    env.clock
        .ok_or_else(|| SqlSurfaceError::FeatureNotSupported {
            detail: "current time functions are not supported in this context".to_string(),
        })
}

/// 「トランザクション固定のみ」スコープ（分割実行 DML。[`time_fn::ClockPolicy::TxnFixedOnly`]）で
/// `statement_timestamp()`・`clock_timestamp()` を `0A000` にする（再開時に値を再現できないため
/// fail-closed。SQL-37・RECOVER-11・Issue #1587。書き込み側の対応は #2188）。
fn reject_non_txn_fixed_clock(name: &str) -> Result<(), SqlSurfaceError> {
    if time_fn::current_clock_policy() == time_fn::ClockPolicy::TxnFixedOnly {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: format!("{name}() is not supported in a partitioned statement"),
        });
    }
    Ok(())
}

/// 括弧なし値関数（`CURRENT_TIMESTAMP`・`CURRENT_DATE`・`LOCALTIMESTAMP`）を束縛時に確定した
/// 定数へ解決する。`CURRENT_TIME`・`LOCALTIME` は TIME 型が未実装のため `0A000`。
fn bind_value_function(
    kind: ValueFunctionKind,
    precision: Option<u8>,
    env: &BindEnv<'_>,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    match kind {
        ValueFunctionKind::CurrentTime | ValueFunctionKind::LocalTime => {
            Err(SqlSurfaceError::FeatureNotSupported {
                detail: "TIME values are not supported".to_string(),
            })
        }
        ValueFunctionKind::CurrentDate => {
            let clock = require_clock(env)?;
            let days = time_fn::micros_to_days(clock.txn_start_micros).ok_or_else(|| {
                SqlSurfaceError::datetime_field_overflow("current date is out of range")
            })?;
            Ok((BoundExpr::Date(days), ExprType::Date))
        }
        ValueFunctionKind::CurrentTimestamp | ValueFunctionKind::LocalTimestamp => {
            let clock = require_clock(env)?;
            let micros = match precision {
                None => clock.txn_start_micros,
                Some(p) => time_fn::round_to_precision(clock.txn_start_micros, p).ok_or(
                    SqlSurfaceError::InvalidParameterValue {
                        detail: "timestamp precision must be between 0 and 6",
                    },
                )?,
            };
            Ok((BoundExpr::Timestamp(micros), ExprType::Timestamp))
        }
    }
}

/// `expr` が PostgreSQL の int4 になる式（int4 範囲の整数リテラル・INTEGER 列・
/// それらの四則演算）かを AST から判定する（`->`／`->>` の添字右辺の型規則。D6）。
fn is_int4_expr(expr: &Expr, env: &BindEnv<'_>) -> bool {
    match expr {
        Expr::Number(raw) => matches!(
            classify_projection_integer_literal(raw),
            Some((_, NumericKind::Int4))
        ),
        Expr::Ident(name) => env.schema.is_some_and(|sc| {
            sc.columns
                .iter()
                .find(|c| &c.name == name)
                .is_some_and(|c| matches!(c.ty, ColumnType::Integer))
        }),
        Expr::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div,
            lhs,
            rhs,
        } => is_int4_expr(lhs, env) && is_int4_expr(rhs, env),
        // `json_array_length` の戻り型は int4（Issue #2180）。
        Expr::Call { name, .. } => name.eq_ignore_ascii_case("json_array_length"),
        _ => false,
    }
}

/// jsonb パス取得演算子（`->` `->>` `#>` `#>>`。対象ビヘイビア: TABLE-25。Issue #1596）
/// の束縛。左辺は jsonb 列の参照または入れ子のパス演算子のみ（それ以外は `42883`、
/// `json` 列は原文保持の意味論が別 Issue のため `0A000`）。右辺は演算子ごとに型規則で
/// キー・添字・パスへ確定させ、評価時に型を再判定しない。
fn bind_json_access(
    op: JsonAccessOp,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if op.is_predicate() {
        return bind_json_predicate(op, lhs, rhs, env, node_budget);
    }
    if matches!(op, JsonAccessOp::Concat | JsonAccessOp::DeletePath) {
        return bind_json_modify(op, lhs, rhs, env, node_budget);
    }
    let operator_missing = || {
        SqlSurfaceError::undefined_function(format!(
            "operator does not exist: the operand types do not match the {} operator",
            op.as_sql()
        ))
    };
    // 左辺。jsonb 列は裸の識別子のときだけここで束縛する（それ以外の位置での jsonb 列参照は
    // 従来どおり束縛段が 22000 で拒否する）。
    let (l_bound, l_ty) = match lhs {
        Expr::Ident(name) if !env.params.contains_key(name.to_ascii_lowercase().as_str()) => {
            let column = env
                .schema
                .and_then(|sc| sc.columns.iter().enumerate().find(|(_, c)| &c.name == name));
            match column {
                Some((index, c)) if matches!(c.ty, ColumnType::Jsonb) => {
                    *node_budget = node_budget.checked_sub(1).ok_or_else(|| {
                        SqlSurfaceError::payload_too_large("expression is too large")
                    })?;
                    (BoundExpr::JsonbColumnRef { index }, ExprType::Json)
                }
                Some((index, c)) if matches!(c.ty, ColumnType::Json) => {
                    *node_budget = node_budget.checked_sub(1).ok_or_else(|| {
                        SqlSurfaceError::payload_too_large("expression is too large")
                    })?;
                    (BoundExpr::JsonColumnRef { index }, ExprType::JsonText)
                }
                _ => bind_expr_in(lhs, env, node_budget)?,
            }
        }
        _ => bind_expr_any(lhs, env, node_budget)?,
    };
    if l_ty != ExprType::Json && l_ty != ExprType::JsonText {
        return Err(operator_missing());
    }
    let as_text = matches!(op, JsonAccessOp::GetText | JsonAccessOp::PathText);
    // `->`／`#>` の結果型は左辺と同じ（jsonb は正規化値、json は原文の部分テキスト）。
    let result_ty = if as_text { ExprType::Text } else { l_ty };
    let rhs_bound = match op {
        JsonAccessOp::Get | JsonAccessOp::GetText => {
            // 添字が int4 式なら、WHERE／UPDATE／DELETE 等の非投影経路でも投影経路と同じ
            // 型付き整数演算（中間 int4 あふれは 22003・整数除算は切り捨て）で束縛し、
            // 経路間で評価結果が食い違わないようにする（f64 評価だと中間あふれを見逃す）。
            let subscript_is_int4 = is_int4_expr(rhs, env);
            let saved_typed_arith = env.typed_arith;
            if subscript_is_int4 {
                env.typed_arith = true;
            }
            let bound = bind_expr_in(rhs, env, node_budget);
            env.typed_arith = saved_typed_arith;
            let (r_bound, r_ty) = bound?;
            match r_ty {
                ExprType::Text => JsonAccessRhs::Key(Box::new(r_bound)),
                ExprType::Scalar if subscript_is_int4 => JsonAccessRhs::Index(Box::new(r_bound)),
                _ => return Err(operator_missing()),
            }
        }
        JsonAccessOp::Contains
        | JsonAccessOp::ContainedBy
        | JsonAccessOp::Exists
        | JsonAccessOp::ExistsAny
        | JsonAccessOp::ExistsAll
        | JsonAccessOp::Concat
        | JsonAccessOp::DeletePath => {
            return Err(SqlSurfaceError::Internal {
                detail: "JSON predicate or modify operator reached the path operator binder"
                    .to_string(),
            })
        }
        JsonAccessOp::Path | JsonAccessOp::PathText => {
            let Expr::String(literal) = rhs else {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "the right operand of #> and #>> must be a text[] literal".to_string(),
                });
            };
            *node_budget = node_budget
                .checked_sub(1)
                .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
            let array_ty = crate::catalog::ArrayType::new(
                crate::catalog::ArrayElemType::Text,
                crate::catalog::MAX_ARRAY_ELEMENTS,
            )
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "text[] type construction failed".to_string(),
            })?;
            match crate::sql::parser::parse_array_literal(literal, array_ty)? {
                crate::row_codec::ArrayValue::Text(items) => JsonAccessRhs::Path(items),
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "text[] literal parsed to a non-text array".to_string(),
                    })
                }
            }
        }
    };
    Ok((
        BoundExpr::JsonAccess {
            lhs: Box::new(l_bound),
            rhs: rhs_bound,
            as_text,
        },
        result_ty,
    ))
}

/// `||` の jsonb オペランドを束縛する。`json` 列は PostgreSQL では text 連結になるが
/// 本実装に文字列連結は無いため `0A000`（fail-closed）。それ以外は通常の式として束縛する。
fn bind_json_concat_operand(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if let Expr::Ident(name) = expr {
        if !env.params.contains_key(name.to_ascii_lowercase().as_str()) {
            let is_json_column = env.schema.is_some_and(|sc| {
                sc.columns
                    .iter()
                    .any(|c| &c.name == name && matches!(c.ty, ColumnType::Json))
            });
            if is_json_column {
                return Err(concat_not_supported());
            }
        }
    }
    // json 値（関数結果・パス演算子の結果）も json 列と同じく text 連結に当たるため `0A000`
    // に揃える（PR #2390 指摘。型で 42883／0A000 が割れないようにする）。
    let (bound, ty) = bind_expr_any(expr, env, node_budget)?;
    if ty == ExprType::JsonText {
        return Err(concat_not_supported());
    }
    Ok((bound, ty))
}

fn concat_not_supported() -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: "the || operator is only supported on jsonb operands".to_string(),
    }
}

/// jsonb の連結・パス削除演算子（`||` `#-`。Issue #2181・TABLE-25）の束縛。
/// `||` は両辺 jsonb（型なしリテラルは反対側が jsonb のとき jsonb として解釈）のみ受理し、
/// 文字列連結に当たる組は `0A000`、型の合わない組は `42883`。`#-` は左辺 jsonb（`json` 列は
/// `42883`）・右辺 text[] リテラル（NULL 要素は `22004` の分類が未実装のため `0A000`）。
fn bind_json_modify(
    op: JsonAccessOp,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let operator_missing = || {
        SqlSurfaceError::undefined_function(format!(
            "operator does not exist: the operand types do not match the {} operator",
            op.as_sql()
        ))
    };
    if op == JsonAccessOp::DeletePath {
        let (l, l_ty) = bind_json_predicate_operand(lhs, env, node_budget)?;
        if l_ty != ExprType::Json {
            return Err(operator_missing());
        }
        let Expr::String(literal) = rhs else {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "the right operand of #- must be a text[] literal".to_string(),
            });
        };
        *node_budget = node_budget
            .checked_sub(1)
            .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
        let array_ty = crate::catalog::ArrayType::new(
            crate::catalog::ArrayElemType::Text,
            crate::catalog::MAX_ARRAY_ELEMENTS,
        )
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "text[] type construction failed".to_string(),
        })?;
        let items = match crate::sql::parser::parse_array_literal(literal, array_ty)? {
            crate::row_codec::ArrayValue::Text(items) => items,
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "text[] literal parsed to a non-text array".to_string(),
                })
            }
        };
        let path = items
            .into_iter()
            .map(|e| {
                e.ok_or_else(|| SqlSurfaceError::FeatureNotSupported {
                    detail: "a NULL element in the #- path is not supported".to_string(),
                })
            })
            .collect::<Result<Vec<String>, _>>()?;
        return Ok((
            BoundExpr::JsonModify {
                op: JsonModifyOp::DeletePath,
                lhs: Box::new(l),
                rhs: JsonModifyRhs::Path(path),
            },
            ExprType::Json,
        ));
    }
    // `||`。
    let lhs_is_literal = matches!(lhs, Expr::String(_));
    let rhs_is_literal = matches!(rhs, Expr::String(_));
    if lhs_is_literal && rhs_is_literal {
        return Err(operator_missing());
    }
    // 型なしリテラルの反対側の型を確定させる。jsonb ならリテラルを jsonb として解釈、
    // text なら文字列連結（未実装）、それ以外は演算子なし。
    let (l_bound, r_bound) = if lhs_is_literal || rhs_is_literal {
        let (other_expr, literal_expr) = if lhs_is_literal {
            (rhs, lhs)
        } else {
            (lhs, rhs)
        };
        let (other, other_ty) = bind_json_concat_operand(other_expr, env, node_budget)?;
        match other_ty {
            ExprType::Json => {}
            ExprType::Text => return Err(concat_not_supported()),
            _ => return Err(operator_missing()),
        }
        let lit = bind_json_predicate_literal(literal_expr, node_budget)?;
        if lhs_is_literal {
            (lit, other)
        } else {
            (other, lit)
        }
    } else {
        let (l, l_ty) = bind_json_concat_operand(lhs, env, node_budget)?;
        let (r, r_ty) = bind_json_concat_operand(rhs, env, node_budget)?;
        match (l_ty, r_ty) {
            (ExprType::Json, ExprType::Json) => (l, r),
            (ExprType::Json, ExprType::Text) | (ExprType::Text, ExprType::Json) => {
                return Err(concat_not_supported())
            }
            (ExprType::Json, _) | (_, ExprType::Json) => return Err(operator_missing()),
            _ => return Err(concat_not_supported()),
        }
    };
    Ok((
        BoundExpr::JsonModify {
            op: JsonModifyOp::Concat,
            lhs: Box::new(l_bound),
            rhs: JsonModifyRhs::Jsonb(Box::new(r_bound)),
        },
        ExprType::Json,
    ))
}

/// 加減算の段の `-` の左辺が jsonb を返しうる式かを AST から静的に判定する
/// （[`bind_json_delete`] へ振り分けるか、従来の算術へ流すかの分岐。Issue #2181）。
/// jsonb／json 列の識別子・jsonb を返すパス／連結／削除演算子・それらを左辺とする `-`。
fn is_json_expr(expr: &Expr, env: &BindEnv<'_>) -> bool {
    match expr {
        Expr::Ident(name) => {
            !env.params.contains_key(name.to_ascii_lowercase().as_str())
                && env.schema.is_some_and(|sc| {
                    sc.columns.iter().any(|c| {
                        &c.name == name && matches!(c.ty, ColumnType::Jsonb | ColumnType::Json)
                    })
                })
        }
        Expr::JsonAccess { op, .. } => matches!(
            op,
            JsonAccessOp::Get
                | JsonAccessOp::Path
                | JsonAccessOp::Concat
                | JsonAccessOp::DeletePath
        ),
        Expr::Binary {
            op: BinOp::Sub,
            lhs,
            ..
        } => is_json_expr(lhs, env),
        _ => false,
    }
}

/// `jsonb - text`／`jsonb - integer` の束縛（Issue #2181・TABLE-25）。`-` は加減算の段に
/// あるため構文段は [`BinOp::Sub`] を生成し、左辺が jsonb のときだけここへ来る。
/// `json` 列は `42883`（jsonb 専用の演算子）。右辺は text（キー削除）か int4 式（添字削除）、
/// それ以外は `42883`。`jsonb - text[]` は text[] を書く手段（キャスト・`ARRAY[...]`）が
/// 無いため対象外。
fn bind_json_delete(
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    // CHECK 本体では `||`・`#-` と同じく拒否する（構文段は型を知らないため `-` だけは
    // 束縛段で判定する。fail-closed）。
    if CHECK_BINDING.with(std::cell::Cell::get) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "JSON operators are not supported in a CHECK constraint".to_string(),
        });
    }
    let operator_missing = || {
        SqlSurfaceError::undefined_function(
            "operator does not exist: the operand types do not match the - operator",
        )
    };
    let (l, l_ty) = bind_json_predicate_operand(lhs, env, node_budget)?;
    if l_ty != ExprType::Json {
        return Err(operator_missing());
    }
    bind_json_delete_rhs(l, rhs, env, node_budget)
}

/// 左辺を束縛済みの `jsonb - ...`（関数呼び出し等 AST 形で判定できない左辺。Issue #2181）。
fn bind_json_delete_bound(
    l: BoundExpr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if CHECK_BINDING.with(std::cell::Cell::get) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "JSON operators are not supported in a CHECK constraint".to_string(),
        });
    }
    bind_json_delete_rhs(l, rhs, env, node_budget)
}

/// `jsonb - text`／`jsonb - integer` の右辺の束縛と型振り分け（[`bind_json_delete`] の後半）。
fn bind_json_delete_rhs(
    l: BoundExpr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let operator_missing = || {
        SqlSurfaceError::undefined_function(
            "operator does not exist: the operand types do not match the - operator",
        )
    };
    let subscript_is_int4 = is_int4_expr(rhs, env);
    let saved_typed_arith = env.typed_arith;
    if subscript_is_int4 {
        env.typed_arith = true;
    }
    let bound = bind_expr_in(rhs, env, node_budget);
    env.typed_arith = saved_typed_arith;
    let (r, r_ty) = bound?;
    let (op, rhs_bound) = match r_ty {
        ExprType::Text => (JsonModifyOp::DeleteKey, JsonModifyRhs::Key(Box::new(r))),
        ExprType::Scalar if subscript_is_int4 => {
            (JsonModifyOp::DeleteIndex, JsonModifyRhs::Index(Box::new(r)))
        }
        _ => return Err(operator_missing()),
    };
    Ok((
        BoundExpr::JsonModify {
            op,
            lhs: Box::new(l),
            rhs: rhs_bound,
        },
        ExprType::Json,
    ))
}
/// jsonb 述語（`@>` `<@` `?` `?|` `?&`。Issue #1598・TABLE-25）の jsonb オペランドを束縛する。
/// `json` 列は `42883`（PostgreSQL に json 用の演算子は無い。パス演算子の `0A000` とは
/// 異なる）、それ以外は通常の式として束縛する（jsonb 以外の型は呼び出し元が `42883` にする）。
fn bind_json_predicate_operand(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if let Expr::Ident(name) = expr {
        if !env.params.contains_key(name.to_ascii_lowercase().as_str()) {
            let is_json_column = env.schema.is_some_and(|sc| {
                sc.columns
                    .iter()
                    .any(|c| &c.name == name && matches!(c.ty, ColumnType::Json))
            });
            if is_json_column {
                return Err(SqlSurfaceError::undefined_function(
                    "operator does not exist: a json column cannot be used with a jsonb containment or existence operator",
                ));
            }
        }
    }
    bind_expr_any(expr, env, node_budget)
}

/// 型なし文字列リテラルを jsonb 述語のオペランドとして解釈する（1 ノード課金）。
fn bind_json_predicate_literal(
    expr: &Expr,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    let Expr::String(s) = expr else {
        return Err(SqlSurfaceError::Internal {
            detail: "jsonb literal operand is not a string literal".to_string(),
        });
    };
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    bind_jsonb_literal(s).map(|(b, _)| b)
}

/// jsonb の包含・存在演算子の束縛（Issue #1598・TABLE-25）。型規則は PostgreSQL に合わせ、
/// 合わない組は `42883`、`?|`／`?&` の右辺が text[] リテラルでない場合は `0A000`
/// （`#>` の前例。配列式は TABLE-35 の担当）。型なし文字列リテラルは反対側が jsonb のとき
/// jsonb として解釈する（不正な JSON は `22P02`、長さ超過は `54000`。左右どちらでも同じ）。
fn bind_json_predicate(
    op: JsonAccessOp,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let operator_missing = || {
        SqlSurfaceError::undefined_function(format!(
            "operator does not exist: the operand types do not match the {} operator",
            op.as_sql()
        ))
    };
    let pred_op = match op {
        JsonAccessOp::Contains => JsonPredicateOp::Contains,
        JsonAccessOp::ContainedBy => JsonPredicateOp::ContainedBy,
        JsonAccessOp::Exists => JsonPredicateOp::Exists,
        JsonAccessOp::ExistsAny => JsonPredicateOp::ExistsAny,
        JsonAccessOp::ExistsAll => JsonPredicateOp::ExistsAll,
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "non-predicate JSON operator reached the predicate binder".to_string(),
            })
        }
    };
    let (l_bound, rhs_bound) = match pred_op {
        JsonPredicateOp::Contains | JsonPredicateOp::ContainedBy => {
            let lhs_is_literal = matches!(lhs, Expr::String(_));
            let rhs_is_literal = matches!(rhs, Expr::String(_));
            // 文字列リテラル同士は PostgreSQL では解決できるが、本実装は #1596 と同じ
            // `42883` に統一する（SQLSTATE 差は対象外）。
            if lhs_is_literal && rhs_is_literal {
                return Err(operator_missing());
            }
            // 非リテラル側を先に束縛して jsonb であることを確認し、その後にリテラル側を
            // jsonb として解釈する（左右どちらにリテラルを置いても同じ結果・同じ SQLSTATE）。
            let (l_bound, r_bound) = if lhs_is_literal {
                let (r, r_ty) = bind_json_predicate_operand(rhs, env, node_budget)?;
                if r_ty != ExprType::Json {
                    return Err(operator_missing());
                }
                (bind_json_predicate_literal(lhs, node_budget)?, r)
            } else {
                let (l, l_ty) = bind_json_predicate_operand(lhs, env, node_budget)?;
                if l_ty != ExprType::Json {
                    return Err(operator_missing());
                }
                if rhs_is_literal {
                    (l, bind_json_predicate_literal(rhs, node_budget)?)
                } else {
                    let (r, r_ty) = bind_json_predicate_operand(rhs, env, node_budget)?;
                    if r_ty != ExprType::Json {
                        return Err(operator_missing());
                    }
                    (l, r)
                }
            };
            (l_bound, JsonPredicateRhs::Jsonb(Box::new(r_bound)))
        }
        JsonPredicateOp::Exists => {
            let (l, l_ty) = bind_json_predicate_operand(lhs, env, node_budget)?;
            if l_ty != ExprType::Json {
                return Err(operator_missing());
            }
            let (r, r_ty) = bind_expr_in(rhs, env, node_budget)?;
            if r_ty != ExprType::Text {
                return Err(operator_missing());
            }
            (l, JsonPredicateRhs::Key(Box::new(r)))
        }
        JsonPredicateOp::ExistsAny | JsonPredicateOp::ExistsAll => {
            let (l, l_ty) = bind_json_predicate_operand(lhs, env, node_budget)?;
            if l_ty != ExprType::Json {
                return Err(operator_missing());
            }
            let Expr::String(literal) = rhs else {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "the right operand of ?| and ?& must be a text[] literal".to_string(),
                });
            };
            *node_budget = node_budget
                .checked_sub(1)
                .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
            let array_ty = crate::catalog::ArrayType::new(
                crate::catalog::ArrayElemType::Text,
                crate::catalog::MAX_ARRAY_ELEMENTS,
            )
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "text[] type construction failed".to_string(),
            })?;
            match crate::sql::parser::parse_array_literal(literal, array_ty)? {
                crate::row_codec::ArrayValue::Text(items) => (l, JsonPredicateRhs::Keys(items)),
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "text[] literal parsed to a non-text array".to_string(),
                    })
                }
            }
        }
    };
    Ok((
        BoundExpr::JsonPredicate {
            op: pred_op,
            lhs: Box::new(l_bound),
            rhs: rhs_bound,
        },
        ExprType::Bool,
    ))
}

/// 型付き算術のオペランドの数値種別（Issue #1539・SQL-42）。`None` は倍精度扱い
/// （小数リテラル・疑似列 `id`・関数・`CASE` など。現状の f64 意味論のまま = フェイル
/// セーフ）。整数リテラルは `expr`（字面）から、列参照は束縛済みの `ColumnRef` と
/// スキーマから、入れ子の型付き算術はそのノードの種別から決める。O(1)。
pub(super) fn numeric_kind(
    expr: &Expr,
    bound: &BoundExpr,
    env: &BindEnv<'_>,
) -> Option<NumericKind> {
    if let Expr::Number(raw) = expr {
        return classify_projection_integer_literal(raw).map(|(_, k)| k);
    }
    match bound {
        BoundExpr::ColumnRef { index } => {
            let column = env.schema?.columns.get(*index)?;
            match column.ty {
                ColumnType::SmallInt => Some(NumericKind::Int2),
                ColumnType::Integer => Some(NumericKind::Int4),
                ColumnType::BigInt => Some(NumericKind::Int8),
                ColumnType::Real => Some(NumericKind::Float4),
                _ => None,
            }
        }
        BoundExpr::Builtin {
            f: BuiltinFn::TypedArith(_, k),
            ..
        } => Some(*k),
        // `json_array_length` は int4（Issue #2180）。整数除算・添字の型解決に反映する。
        BoundExpr::JsonTextFn(f) if f.result_is_int4() => Some(NumericKind::Int4),
        // `jsonb_array_length` は int4 なので、算術でも整数演算に解決する（Issue #1599）。
        BoundExpr::JsonFn {
            f: JsonFnKind::ArrayLength,
            ..
        } => Some(NumericKind::Int4),
        _ => None,
    }
}

/// 二項算術の結果種別（PostgreSQL の演算子解決に合わせる）。`None` は倍精度
/// （既存の [`BoundExpr::Binary`] で束縛する）。`real + integer` は PostgreSQL でも
/// 暗黙キャスト候補のうち numeric カテゴリの優先型 float8 が選ばれ
/// `double precision` になるため、`(Float4, Int*)` は意図的に `None`（倍精度）とする。
fn combine_numeric_kind(l: Option<NumericKind>, r: Option<NumericKind>) -> Option<NumericKind> {
    use NumericKind::{Float4, Int2, Int4, Int8};
    match (l?, r?) {
        // PostgreSQL の演算子解決: smallint 同士は smallint、int4 が混ざれば integer、
        // bigint が混ざれば bigint（Issue #1560）。
        (Int2, Int2) => Some(Int2),
        (Int2 | Int4, Int2 | Int4) => Some(Int4),
        (Int2 | Int4 | Int8, Int2 | Int4 | Int8) => Some(Int8),
        (Float4, Float4) => Some(Float4),
        _ => None,
    }
}

/// 列の型名（エラーメッセージ用。リテラル本文は含めない）。
fn column_type_label(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "text",
        ColumnType::Varchar(_) => "character varying",
        ColumnType::Bpchar(_) => "character",
        ColumnType::Vector(_) => "vector",
        ColumnType::SmallInt => "smallint",
        ColumnType::Integer => "integer",
        ColumnType::BigInt => "bigint",
        ColumnType::Real => "real",
        ColumnType::Double => "double precision",
        ColumnType::Boolean => "boolean",
        ColumnType::Date => "date",
        ColumnType::Timestamp => "timestamp",
        ColumnType::Time => "time",
        ColumnType::Array(_) => "array",
        ColumnType::Bytea => "bytea",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Enum(_) => "enum",
        ColumnType::Numeric { .. } => "numeric",
        ColumnType::Uuid => "uuid",
    }
}

/// 比較の片側が数値リテラル、もう片側が「式中参照を持たない列型」（BOOLEAN／UUID／
/// BYTEA／ARRAY／JSON／JSONB／ENUM）の列である場合に、列参照の拒否（`22000`）より先に
/// `42883`（演算子が存在しない。PostgreSQL の `operator does not exist` 相当）を返す
/// （Issue #1408・SQL-24・ERR-2）。NUMERIC 列は PostgreSQL に演算子があるため対象外で、
/// 従来の拒否のまま据え置く。`bind_expr_in` の比較腕のみから呼ばれる。
fn reject_numeric_literal_vs_operatorless_column(
    lhs: &Expr,
    rhs: &Expr,
    env: &BindEnv<'_>,
) -> Result<(), SqlSurfaceError> {
    let ident = match (lhs, rhs) {
        (Expr::Ident(n), Expr::Number(_)) | (Expr::Number(_), Expr::Ident(n)) => n,
        _ => return Ok(()),
    };
    let Some(schema) = env.schema else {
        return Ok(());
    };
    if env.params.contains_key(ident.to_ascii_lowercase().as_str()) {
        return Ok(());
    }
    let Some(column) = schema.columns.iter().find(|c| &c.name == ident) else {
        return Ok(());
    };
    match column.ty {
        ColumnType::Boolean
        | ColumnType::Uuid
        | ColumnType::Bytea
        | ColumnType::Array(_)
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Enum(_) => Err(SqlSurfaceError::undefined_function(format!(
            "operator does not exist: {} and numeric",
            column_type_label(&column.ty)
        ))),
        _ => Ok(()),
    }
}

/// 浮動小数の文字列／数値リテラルを `scalar_float::parse_double` の閉じた文法へ正規化する
/// （前後空白除去・符号は高々 1 つ・末尾ドット `1.` と先頭ドット `.5` の補完・指数部保持）。
/// 不正形は `None`（呼び出し側が `22P02` にする）。`bind_unknown_literal_against` と
/// [`parse_float_operand_literal`] が共有する（Issue #1408・#1438・SQL-24）。
fn normalize_float_literal_text(s: &str) -> Option<String> {
    let trimmed = s.trim_matches(|c: char| c.is_ascii_whitespace());
    // 符号は高々 1 つ（`+-1`・`-+1` は不正形）
    let (neg, body) = match trimmed.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if body.starts_with(['+', '-']) {
        return None;
    }
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (body.get(..i).unwrap_or(""), body.get(i..).unwrap_or("")),
        None => (body, ""),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let frac_norm = frac_part.filter(|f| !f.is_empty());
    let int_norm = if int_part.is_empty() && frac_norm.is_some() {
        "0"
    } else {
        int_part
    };
    Some(match frac_norm {
        Some(f) => format!("{neg}{int_norm}.{f}{exp}"),
        None => format!("{neg}{int_norm}{exp}"),
    })
}

/// REAL／DOUBLE 列と比較される数値リテラル（数値形・文字列形の双方）を `f64` へ解釈する
/// （Issue #1438・SQL-24・TABLE-13・ERR-2・ERR-4 ポインタ）。PostgreSQL は
/// `float 列 <op> リテラル` を float8 同士の比較として解決するため、整数列・疑似列 `id` 用の
/// 2^53 exactness 判定（[`parse_number_literal`]）はかけない。書式不正は `22P02`、
/// オーバーフロー・非ゼロのアンダーフローは `22003`。`-0` は `+0` へ正規化される。
/// エラーにリテラル本文は含めない。`bind_expr_in` の比較腕と
/// `bind_unknown_literal_against` から呼ばれる。
fn parse_float_operand_literal(raw: &str) -> Result<f64, SqlSurfaceError> {
    let malformed = || {
        SqlSurfaceError::invalid_text_representation("invalid input syntax for a numeric operand")
    };
    let normalized = normalize_float_literal_text(raw).ok_or_else(malformed)?;
    crate::scalar_float::parse_double(&normalized).map_err(|e| match e {
        crate::scalar_float::ParseFloatError::Malformed => malformed(),
        crate::scalar_float::ParseFloatError::OutOfRange => SqlSurfaceError::numeric_out_of_range(
            "numeric literal is out of range for type double precision",
        ),
    })
}

/// `expr` が UDF 仮引数に遮られない REAL／DOUBLE 実列の参照かを構文的に判定する
/// （Ident 束縛の優先順〔仮引数 > 実列〕と一致させる。Issue #1438）。
fn float_column_ident(expr: &Expr, env: &BindEnv<'_>) -> bool {
    let Expr::Ident(name) = expr else {
        return false;
    };
    let Some(schema) = env.schema else {
        return false;
    };
    if env.params.contains_key(name.to_ascii_lowercase().as_str()) {
        return false;
    }
    schema
        .columns
        .iter()
        .find(|c| &c.name == name)
        .is_some_and(|c| matches!(c.ty, ColumnType::Real | ColumnType::Double))
}

/// 浮動小数列相手の数値リテラルを束縛する（1 ノード課金。[`parse_float_operand_literal`] 経由）。
fn bind_float_number_literal(
    raw: &str,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    Ok((
        BoundExpr::Number(parse_float_operand_literal(raw)?),
        ExprType::Scalar,
    ))
}

/// 比較の相手側が文字列リテラル（PostgreSQL の unknown 型）のとき、反対側 `other` の型に
/// 合わせてリテラルを束縛する（Issue #1408・SQL-24・ERR-2・ERR-6）。反対側が数値
/// （`ExprType::Scalar`）なら数値として解釈する: INTEGER／BIGINT 列は整数文法
/// （前後空白除去・任意の符号・1 桁以上の数字。`1.5` 等は `22P02`、列の範囲外は `22003`）、
/// それ以外（REAL／DOUBLE・式結果）は [`crate::scalar_float::parse_double`]。最終的に
/// [`parse_number_literal`] を通すため、数値リテラル形と同じ 2^53 の exactness 判定を共有する。
/// 反対側が数値以外のときは通常の文字列リテラル束縛（TEXT）へ委ね、型不一致は
/// [`bind_binary`] が `42883` にする。エラーにリテラル本文は含めない。
fn bind_unknown_literal_against(
    s: &str,
    other: &BoundExpr,
    other_ty: ExprType,
    env: &BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    if s.len() > crate::row_codec::MAX_TEXT_FIELD_LEN as usize {
        return Err(SqlSurfaceError::payload_too_large(
            "string literal exceeds the maximum TEXT field length",
        ));
    }
    // Issue #1597・TABLE-25: jsonb 式と比較される型なし文字列リテラルは jsonb として
    // 解析する（PostgreSQL の `docb -> 'k' = '1'` と同じ解決）。
    if other_ty == ExprType::Json {
        return bind_jsonb_literal(s);
    }
    // `TIME` 式と比較される型なし文字列リテラルは TIME として解析する（PostgreSQL と同じ
    // 解決。Issue #1570）。エラーにリテラル本文は含めない（`bind_datetime_literal` と同じ写像）。
    if other_ty == ExprType::Time {
        return match crate::datetime::parse_time(s) {
            Ok(micros) => Ok((BoundExpr::Time(micros), ExprType::Time)),
            Err(crate::datetime::DateTimeLiteralError::Format(_)) => Err(
                SqlSurfaceError::invalid_datetime_format("invalid input syntax for type time"),
            ),
            Err(crate::datetime::DateTimeLiteralError::Overflow(_)) => Err(
                SqlSurfaceError::datetime_field_overflow("time field value out of range"),
            ),
        };
    }
    if other_ty != ExprType::Scalar {
        // 相手が `CHAR(n)` 列参照（`BpcharToText`）なら、型なし文字列リテラルは bpchar として
        // 解決され末尾の半角空白が比較に影響しない（TABLE-31・Issue #1566）。
        if matches!(
            other,
            BoundExpr::Builtin {
                f: BuiltinFn::BpcharToText,
                ..
            }
        ) {
            return Ok((
                BoundExpr::Text(s.trim_end_matches(' ').to_string()),
                ExprType::Text,
            ));
        }
        return Ok((BoundExpr::Text(s.to_string()), ExprType::Text));
    }
    let column_ty = match other {
        BoundExpr::ColumnRef { index } => env
            .schema
            .and_then(|sc| sc.columns.get(*index))
            .map(|c| &c.ty),
        _ => None,
    };
    let invalid = || {
        SqlSurfaceError::invalid_text_representation("invalid input syntax for a numeric operand")
    };
    let out_of_range =
        || SqlSurfaceError::numeric_out_of_range("numeric literal is out of range for the operand");
    let text = match column_ty {
        Some(ColumnType::SmallInt) | Some(ColumnType::Integer) | Some(ColumnType::BigInt) => {
            let trimmed = s.trim_matches(|c: char| c.is_ascii_whitespace());
            let v: i64 = trimmed.parse().map_err(|e: std::num::ParseIntError| {
                use std::num::IntErrorKind::{NegOverflow, PosOverflow};
                match e.kind() {
                    PosOverflow | NegOverflow => out_of_range(),
                    _ => invalid(),
                }
            })?;
            if matches!(column_ty, Some(ColumnType::Integer)) && i32::try_from(v).is_err() {
                return Err(out_of_range());
            }
            if matches!(column_ty, Some(ColumnType::SmallInt)) && i16::try_from(v).is_err() {
                return Err(out_of_range());
            }
            v.to_string()
        }
        _ => {
            // 文字列からの変換文法は数値リテラル形に揃える（正規化・検証は
            // [`parse_float_operand_literal`] と共有）。REAL／DOUBLE 列相手の比較は
            // 浮動小数同士の比較なので、2^53 の exactness 判定（整数列・疑似列 `id` 用）は
            // かけない（Issue #1438）。式結果相手（`column_ty == None`）だけは従来どおり
            // 下の `parse_number_literal` を通す。
            if matches!(column_ty, Some(ColumnType::Real) | Some(ColumnType::Double)) {
                return Ok((
                    BoundExpr::Number(parse_float_operand_literal(s)?),
                    ExprType::Scalar,
                ));
            }
            let normalized = normalize_float_literal_text(s).ok_or_else(invalid)?;
            crate::scalar_float::parse_double(&normalized).map_err(|e| match e {
                crate::scalar_float::ParseFloatError::Malformed => invalid(),
                crate::scalar_float::ParseFloatError::OutOfRange => out_of_range(),
            })?;
            normalized
        }
    };
    let mut n = parse_number_literal(&text)?;
    if n == 0.0 {
        // `-0` 字面も `+0.0` へ正規化する（`parse_double` の canonicalize と同じ F4 契約）。
        n = 0.0;
    }
    Ok((BoundExpr::Number(n), ExprType::Scalar))
}
/// 型なし文字列リテラルを jsonb として解析・正規化して束縛する（Issue #1597・TABLE-25）。
/// 比較の相手が jsonb のときの暗黙解釈で使い、後続の `::jsonb` キャスト（#1593）からも
/// 再利用できる唯一の解析経路。不正な JSON は `22P02`、長さ超過は `54000`。エラーに
/// リテラル本文は含めない。
pub(crate) fn bind_jsonb_literal(s: &str) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    match crate::json::canonicalize_jsonb_text(s) {
        Ok(text) => Ok((BoundExpr::JsonbLiteral(text), ExprType::Json)),
        Err(crate::json::JsonColumnError::TooLong) => Err(SqlSurfaceError::payload_too_large(
            "jsonb literal exceeds the maximum JSON field length",
        )),
        Err(crate::json::JsonColumnError::Invalid) => Err(
            SqlSurfaceError::invalid_text_representation("invalid input syntax for type jsonb"),
        ),
    }
}

/// jsonb 関数（[`JsonFnKind`]。Issue #1599・TABLE-25）の束縛。引数の型規則はここで確定し、
/// 評価時に再判定しない。
///
/// - jsonb 位置: 型なし文字列リテラルは jsonb として解釈（不正は `22P02`、長さ超過は
///   `54000`）、裸の `NULL` は jsonb の NULL、jsonb 以外の型・`json` 列は `42883`。
/// - パス要素: text 式（裸の `NULL` 可）。非 text は `42883`。
/// - 「any」位置（`jsonb_build_*`・`to_jsonb`）: 型なし文字列リテラルは text、裸の `NULL` は
///   JSON の `null`。vector・`json` 列は `0A000`。`to_jsonb` だけは型なしリテラル・裸の
///   `NULL` を多相型が決まらないものとして `42804` にする。
fn bind_json_fn(
    name: &str,
    kind: JsonFnKind,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let arity_error = |expect: &str| {
        SqlSurfaceError::undefined_function(format!(
            "function {name} expects {expect}, got {}",
            args.len()
        ))
    };
    let mut bound_args = Vec::with_capacity(args.len());
    let result_ty = match kind {
        JsonFnKind::Typeof | JsonFnKind::ArrayLength => {
            let [first] = args else {
                return Err(arity_error("1 argument(s)"));
            };
            bound_args.push(bind_json_jsonb_arg(name, first, env, node_budget)?);
            if kind == JsonFnKind::Typeof {
                ExprType::Text
            } else {
                ExprType::Scalar
            }
        }
        JsonFnKind::ExtractPath | JsonFnKind::ExtractPathText => {
            let Some((first, rest)) = args.split_first().filter(|(_, r)| !r.is_empty()) else {
                return Err(arity_error("at least 2 arguments"));
            };
            bound_args.push(bind_json_jsonb_arg(name, first, env, node_budget)?);
            for a in rest {
                bound_args.push(bind_json_text_arg(name, a, env, node_budget)?);
            }
            if kind == JsonFnKind::ExtractPath {
                ExprType::Json
            } else {
                ExprType::Text
            }
        }
        JsonFnKind::BuildObject | JsonFnKind::BuildArray => {
            for a in args {
                bound_args.push(bind_json_any_arg(a, env, node_budget)?);
            }
            ExprType::Json
        }
        JsonFnKind::ToJsonb => {
            let [first] = args else {
                return Err(arity_error("1 argument(s)"));
            };
            if matches!(first, Expr::Null | Expr::String(_)) {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "could not determine polymorphic type because input has type unknown",
                ));
            }
            bound_args.push(bind_json_any_arg(first, env, node_budget)?);
            ExprType::Json
        }
    };
    Ok((
        BoundExpr::JsonFn {
            f: kind,
            args: bound_args,
        },
        result_ty,
    ))
}

/// 1 ノードぶんを式ノード予算へ課金する（裸の `NULL`・文字列リテラルの引数用）。
fn charge_one_node(node_budget: &mut usize) -> Result<(), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    Ok(())
}

/// jsonb 位置の引数を束縛する（[`bind_json_fn`] 参照）。
fn bind_json_jsonb_arg(
    name: &str,
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    match expr {
        Expr::Null => {
            charge_one_node(node_budget)?;
            Ok(BoundExpr::Null)
        }
        Expr::String(_) => bind_json_predicate_literal(expr, node_budget),
        _ => {
            let (bound, ty) = bind_json_predicate_operand(expr, env, node_budget)?;
            if ty != ExprType::Json {
                return Err(SqlSurfaceError::undefined_function(format!(
                    "function {name} requires a jsonb argument"
                )));
            }
            Ok(bound)
        }
    }
}

/// text 位置（パス要素）の引数を束縛する（[`bind_json_fn`] 参照）。
fn bind_json_text_arg(
    name: &str,
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    if matches!(expr, Expr::Null) {
        charge_one_node(node_budget)?;
        return Ok(BoundExpr::Null);
    }
    let (bound, ty) = bind_expr_in(expr, env, node_budget)?;
    if ty != ExprType::Text {
        return Err(SqlSurfaceError::undefined_function(format!(
            "function {name} requires text path arguments"
        )));
    }
    Ok(bound)
}

/// 「any」位置の引数を束縛する（[`bind_json_fn`] 参照）。
fn bind_json_any_arg(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    if matches!(expr, Expr::Null) {
        charge_one_node(node_budget)?;
        return Ok(BoundExpr::Null);
    }
    if let Expr::Ident(name) = expr {
        let is_json_column = !env.params.contains_key(name.to_ascii_lowercase().as_str())
            && env.schema.is_some_and(|sc| {
                sc.columns
                    .iter()
                    .any(|c| &c.name == name && matches!(c.ty, ColumnType::Json))
            });
        if is_json_column {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "a json column cannot be converted to jsonb here; use a jsonb column"
                    .to_string(),
            });
        }
    }
    let (bound, ty) = bind_expr_any(expr, env, node_budget)?;
    if ty == ExprType::Vector {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "a vector value cannot be converted to jsonb".to_string(),
        });
    }
    // json 型の式全般（演算子結果・`json_*` 結果を含む）は評価時に内部エラーになるため、
    // 束縛時にクライアント向けの `0A000` で拒否する。
    if ty == ExprType::JsonText {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "a json value cannot be converted to jsonb here; use a jsonb value".to_string(),
        });
    }
    Ok(bound)
}
fn bind_binary(
    op: BinOp,
    l: BoundExpr,
    lt: ExprType,
    r: BoundExpr,
    rt: ExprType,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let mk = |op, l, r| BoundExpr::Binary {
        op,
        lhs: Box::new(l),
        rhs: Box::new(r),
    };
    match op {
        BinOp::Add | BinOp::Sub => match (lt, rt) {
            (ExprType::Scalar, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Scalar)),
            // `DATE + n`／`DATE - n`（対象ビヘイビア: SQL-26。Issue #920）。
            // `n` は日数。`n - DATE`（`Sub` の左右反転）は PostgreSQL でも
            // 未定義のため受理しない。
            (ExprType::Date, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Date)),
            // `n + DATE`（`Add` のみ左右対称に受理。`n - DATE` は意味が
            // 定まらないため `Sub` では受理しない）。
            (ExprType::Scalar, ExprType::Date) if op == BinOp::Add => {
                Ok((mk(op, l, r), ExprType::Date))
            }
            // `DATE - DATE` は日数差（`Scalar`）を返す。`DATE + DATE` は
            // PostgreSQL でも未定義のため `Add` では受理しない。
            (ExprType::Date, ExprType::Date) if op == BinOp::Sub => {
                Ok((mk(op, l, r), ExprType::Scalar))
            }
            (ExprType::Text, _) | (_, ExprType::Text) => Err(text_arithmetic_error()),
            _ => Err(SqlSurfaceError::datatype_mismatch(
                "'+'/'-' require both operands to be scalar, or a DATE combined with a day-count scalar",
            )),
        },
        BinOp::Mul => match (lt, rt) {
            (ExprType::Scalar, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Scalar)),
            (ExprType::Vector, ExprType::Scalar) | (ExprType::Scalar, ExprType::Vector) => {
                Ok((mk(op, l, r), ExprType::Vector))
            }
            (ExprType::Text, _) | (_, ExprType::Text) => Err(text_arithmetic_error()),
            _ => Err(SqlSurfaceError::datatype_mismatch(
                "'*' requires scalar operands, or one vector and one scalar operand",
            )),
        },
        BinOp::Div => match (lt, rt) {
            (ExprType::Scalar, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Scalar)),
            (ExprType::Vector, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Vector)),
            (ExprType::Text, _) | (_, ExprType::Text) => Err(text_arithmetic_error()),
            _ => Err(SqlSurfaceError::datatype_mismatch(
                "'/' requires scalar operands, or a vector divided by a scalar",
            )),
        },
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq => match (lt, rt) {
            (ExprType::Scalar, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Bool)),
            // Issue #919・SQL-26: TEXT 同士の比較（バイト順＝UTF-8 コードポイント順。
            // PostgreSQL の `"C"` 照合相当）。TEXT と Scalar の混在は許可しない。
            (ExprType::Text, ExprType::Text) => Ok((mk(op, l, r), ExprType::Bool)),
            // 対象ビヘイビア: SQL-26（Issue #920）。`DATE`／`TIMESTAMP` 同士の
            // 比較は内部表現（日数／マイクロ秒）の全順序で行う。
            (ExprType::Date, ExprType::Date) | (ExprType::Timestamp, ExprType::Timestamp) => {
                Ok((mk(op, l, r), ExprType::Bool))
            }
            // `TIME` 同士の比較（0 時起点のマイクロ秒の全順序。Issue #1570）。PostgreSQL に
            // `TIME`⋈`TIMESTAMP`／`DATE` の暗黙変換は無いため、混在は下の腕で `42883`。
            (ExprType::Time, ExprType::Time) => Ok((mk(op, l, r), ExprType::Bool)),
            // `DATE` ⋈ `TIMESTAMP` は `DATE` 側を深夜 0 時の `TIMESTAMP` へ
            // 暗黙昇格してから比較する（PostgreSQL 互換。§2-3）。
            (ExprType::Date, ExprType::Timestamp) => {
                Ok((mk(op, wrap_date_to_timestamp(l), r), ExprType::Bool))
            }
            (ExprType::Timestamp, ExprType::Date) => {
                Ok((mk(op, l, wrap_date_to_timestamp(r)), ExprType::Bool))
            }
            // Issue #1597・TABLE-25: jsonb 同士の比較（PostgreSQL の jsonb btree 順序。
            // `json::jsonb_sort_key` が唯一の順序定義）。
            (ExprType::Json, ExprType::Json) => Ok((mk(op, l, r), ExprType::Bool)),
            // Issue #1408・SQL-24・ERR-2: 比較演算子が存在しない型の組（PostgreSQL の
            // `operator does not exist`）は `42883`（従来は `42804`）。算術腕は据え置き。
            _ => Err(SqlSurfaceError::undefined_function(
                "operator does not exist: comparison operands must be both scalar, both text, both date/timestamp, both time, or both jsonb",
            )),
        },
    }
}

/// TEXT を算術オペランドに取ったときのエラー（Issue #1539・SQL-42。PostgreSQL の
/// `operator does not exist` 相当で `42883`。値・列名は含めない固定文言）。
fn text_arithmetic_error() -> SqlSurfaceError {
    SqlSurfaceError::undefined_function("operator does not exist: arithmetic on text operand")
}

/// `DATE` 値を深夜 0 時の `TIMESTAMP` へ暗黙昇格する（`BuiltinFn::DateToTimestamp`
/// で包む）。`bind_binary`（`DATE`／`TIMESTAMP` 比較）・`bind_call`
/// （`date_part`／`date_trunc` の `DATE` 引数）が共有する（§2-4）。
fn wrap_date_to_timestamp(e: BoundExpr) -> BoundExpr {
    BoundExpr::Builtin {
        f: BuiltinFn::DateToTimestamp,
        args: vec![e],
    }
}

fn bind_call(
    name: &str,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if args.len() > MAX_CALL_ARGS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many call arguments",
        ));
    }
    let lower_name = name.to_ascii_lowercase();
    if crate::sql::json_fn::is_json_fn_name(&lower_name) {
        return crate::sql::json_fn::bind_json_fn(&lower_name, args, env, node_budget);
    }
    if lower_name == "concat" {
        return bind_concat(args, env, node_budget);
    }
    if let Some(kind) = json_fn_from_name(&lower_name) {
        return bind_json_fn(name, kind, args, env, node_budget);
    }
    if is_deferred_json_srf_name(&lower_name) {
        // 集合返却関数（SRF）の基盤が無いため実行しない（`is_deferred_json_srf_name` 参照）。
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "set-returning functions are not supported".to_string(),
        });
    }
    if lower_name == "substr" {
        return bind_substr(name, args, env, node_budget);
    }
    if name.eq_ignore_ascii_case("round") {
        // `round` は arity オーバーロード（SQL-26）: 1 引数形は
        // `BuiltinFn::Round1`、2 引数形（小数点以下 `n` 桁）は `BuiltinFn::Round2`
        // へ解決する。他の組み込みのように `builtin_from_name` 1 個には
        // 定まらないため、ここで名前を先に見て arity から variant を選ぶ。
        let builtin = match args.len() {
            1 => BuiltinFn::Round1,
            2 => BuiltinFn::Round2,
            n => {
                return Err(SqlSurfaceError::undefined_function(format!(
                    "function round expects 1 or 2 argument(s), got {n}"
                )));
            }
        };
        let (param_types, ret) = builtin_signature(builtin);
        let mut bound_args = Vec::with_capacity(args.len());
        for (a, expected) in args.iter().zip(param_types.iter()) {
            let (b, ty) = bind_expr_in(a, env, node_budget)?;
            if ty != *expected {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "function round argument type mismatch",
                ));
            }
            bound_args.push(b);
        }
        return Ok((
            BoundExpr::Builtin {
                f: builtin,
                args: bound_args,
            },
            ret,
        ));
    }

    if lower_name == "date_part" || lower_name == "date_trunc" {
        return bind_date_part_or_trunc(&lower_name, args, env, node_budget);
    }

    // 現在時刻関数（SQL-37・Issue #1586）。引数を取らない（`now(1)` は arity 違反の `42883`）。
    // `now`・`transaction_timestamp` は txn 開始時刻、`statement_timestamp` は文の開始時刻を
    // 束縛時の定数にする。`clock_timestamp` は下の組み込み解決で `BuiltinFn::ClockTimestamp`
    // （評価のたび壁時計を読む）になるが、時計の確定していない文脈では同じく拒否する。
    if is_current_time_function_name(&lower_name) {
        if !args.is_empty() {
            return Err(SqlSurfaceError::undefined_function(format!(
                "function {name} expects 0 argument(s), got {}",
                args.len()
            )));
        }
        let clock = require_clock(env)?;
        match lower_name.as_str() {
            "now" | "transaction_timestamp" => {
                return Ok((
                    BoundExpr::Timestamp(clock.txn_start_micros),
                    ExprType::Timestamp,
                ));
            }
            "statement_timestamp" => {
                reject_non_txn_fixed_clock(&lower_name)?;
                return Ok((
                    BoundExpr::Timestamp(clock.stmt_start_micros),
                    ExprType::Timestamp,
                ));
            }
            _ => {
                reject_non_txn_fixed_clock(&lower_name)?;
            }
        }
    }

    if let Some(builtin) = builtin_from_name(name) {
        let (param_types, ret) = builtin_signature(builtin);
        if args.len() != param_types.len() {
            return Err(SqlSurfaceError::undefined_function(format!(
                "function {name} expects {} argument(s), got {}",
                param_types.len(),
                args.len()
            )));
        }
        let mut bound_args = Vec::with_capacity(args.len());
        for (a, expected) in args.iter().zip(param_types.iter()) {
            let (b, ty) = bind_expr_in(a, env, node_budget)?;
            if ty != *expected {
                return Err(SqlSurfaceError::datatype_mismatch(format!(
                    "function {name} argument type mismatch"
                )));
            }
            bound_args.push(b);
        }
        return Ok((
            BoundExpr::Builtin {
                f: builtin,
                args: bound_args,
            },
            ret,
        ));
    }

    // 解決順（組み込み → 宣言的 UDF → WASM UDF）。組み込みは上で既に処理済みなので
    // ここでは宣言的 UDF を先に試し、次に WASM UDF を試す。
    if let Some(def) = env.registry.get(name).cloned() {
        // 登録済み宣言的 UDF 呼び出し: 実引数を呼び出し元の文脈（`env.schema`・
        // 現在の `env.params`）で先に束縛してから、UDF 本体をパラメータ名 →
        // 束縛済み実引数の対応表で束縛し直す（インライン展開。呼び出し元は
        // 展開後の `BoundExpr` のみを受け取り、`registry` を実行時に参照しない）。
        if args.len() != def.params.len() {
            return Err(SqlSurfaceError::undefined_function(format!(
                "function {name} expects {} argument(s), got {}",
                def.params.len(),
                args.len()
            )));
        }
        let mut bound_args = Vec::with_capacity(args.len());
        for a in args {
            bound_args.push(bind_expr_in(a, env, node_budget)?);
        }
        let mut inner_params = std::collections::HashMap::new();
        for (pname, bound) in def.params.iter().zip(bound_args) {
            inner_params.insert(pname.clone(), bound);
        }
        let mut inner_env = BindEnv {
            // UDF 本体は列参照を持たない閉じた関数であるべき契約（`define_function`
            // が定義時に検査済み）だが、束縛段でも `schema: None` にして構造的に
            // 強制する（定義時検査のバイパス・実装バグの双方に対する fail-closed
            // な多重防御）。
            schema: None,
            params: inner_params,
            registry: env.registry,
            // 呼び出し元の現在のネスト段数を引き継ぐ（UDF 本体のインライン展開
            // 後、呼び出し元の CASE/COALESCE/NULLIF と本体側のそれが合算される
            // ことを構造的に保証する。`enter_case_nesting` docs 参照）。
            case_nesting: env.case_nesting,
            // UDF 本体は従来の f64 意味論のまま（Issue #1539。投影モードは本体へ伝播しない）。
            typed_arith: false,
            // 呼び出し側の確定時刻を継承する（本体中の時刻関数は呼び出し文の時刻で束縛される）。
            clock: env.clock,
        };
        return bind_expr_in(&def.body, &mut inner_env, node_budget);
    }

    if let Some(backend) = env.registry.get_wasm(name).cloned() {
        // WASM UDF（TASK-149）: ABI 固定シグネチャ `(Vector, Scalar) -> Scalar`
        // のため引数個数・型は常にこの形で検査する（組み込み `vec_div` と同じ
        // 引数検査の流儀）。
        if args.len() != WASM_CALL_ARITY {
            return Err(SqlSurfaceError::undefined_function(format!(
                "function {name} expects {WASM_CALL_ARITY} argument(s), got {}",
                args.len()
            )));
        }
        let mut bound_args = Vec::with_capacity(args.len());
        for (a, expected) in args.iter().zip([ExprType::Vector, ExprType::Scalar].iter()) {
            let (b, ty) = bind_expr_in(a, env, node_budget)?;
            if ty != *expected {
                return Err(SqlSurfaceError::datatype_mismatch(format!(
                    "function {name} argument type mismatch"
                )));
            }
            bound_args.push(b);
        }
        return Ok((
            BoundExpr::WasmCall {
                name: name.to_string(),
                backend,
                args: bound_args,
            },
            ExprType::Scalar,
        ));
    }

    Err(SqlSurfaceError::undefined_function(format!(
        "function {name} does not exist"
    )))
}

/// `CONCAT(a, b, c, ...)`（可変長・1〜[`MAX_CALL_ARGS`] 引数）を束縛する
/// （Issue #919・SQL-26）。引数はすべて `TEXT` 型に限定する（数値等の暗黙
/// 文字列化は PostgreSQL の表現と一致しない恐れがあるため対象外とし `22000` で
/// 拒否する。既知の制約として `docs/design/implementation-status.md` に記す）。
/// 2 引数以上は [`BuiltinFn::Concat2`] の左畳み込み
/// （`concat2(concat2(a,b),c)`）へ展開し、1 引数は `concat2(a, "")` 相当にする
/// （CONCAT は NULL を空文字として扱い常に非 NULL を返す契約と整合する）。
fn bind_concat(
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if args.is_empty() {
        return Err(SqlSurfaceError::undefined_function(
            "function concat expects at least 1 argument, got 0",
        ));
    }
    // codex-review P1 指摘対応: `CONCAT` は NULL 引数を空文字として扱い常に
    // 非 NULL を返す契約（`apply_builtin` の `BuiltinFn::Concat2` 実装・
    // `take_text_or_null_arg` 参照）だが、素の `bind_expr_in` は裸の
    // `Expr::Null` を「型が決められない」として `0A000` で拒否するため
    // （`Expr::Null` は `CASE`/`COALESCE`/`NULLIF` 以外の位置では拒否する設計。
    // `bind_expr_in` 内の `Expr::Null` 腕のコメント参照）、`CONCAT(NULL, 'x')`
    // が束縛段で拒否されてしまい実行時契約に到達できなかった。`CASE`/
    // `COALESCE` と同じ [`bind_null_aware`] を使い、`Expr::Null` を型未確定の
    // `BoundExpr::Null` として受理する（実行時は `ExprStep::ConstNull` →
    // `take_text_or_null_arg` の NULL 分岐が空文字として扱う）。
    let mut bound_args = Vec::with_capacity(args.len());
    for a in args {
        let (b, ty) = bind_null_aware(a, env, node_budget)?;
        if let Some(ty) = ty {
            if ty != ExprType::Text {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "function concat arguments must be text",
                ));
            }
        }
        bound_args.push(b);
    }
    let mut iter = bound_args.into_iter();
    // `args.is_empty()` を上で拒否済みのため必ず 1 要素目が存在する。
    let Some(first) = iter.next() else {
        return Err(SqlSurfaceError::Internal {
            detail: "concat argument list unexpectedly empty after validation".to_string(),
        });
    };
    // codex-review P2 指摘対応: 左畳み込みは引数 1 個につき最大 1 個の
    // `BoundExpr::Builtin { f: Concat2, .. }` ラッパーノードを新規生成するが、
    // 元の `for a in args { bind_expr_in(...) }` ループは各引数それ自身の
    // ノード数しか `node_budget` へ課金しておらず、この畳み込みで追加生成
    // されるラッパーノード自体は未計上だった。`CONCAT(a, b, c, ...)` を多数の
    // 引数で呼ぶと、束縛結果のノード数が `MAX_EXPR_NODES`（式ノード数上限）を
    // 実際には超えているのに検査をすり抜けうる（security.md「不安全な設計｜
    // 無制限リソース確保（DoS）」対応）。ラッパーノードを 1 個生成するたびに
    // 確保前に `node_budget` から差し引き、超過は他の経路と同じ
    // `payload_too_large`（`54000`）で fail-closed に拒否する。
    let mut charge_one_node = || -> Result<(), SqlSurfaceError> {
        *node_budget = node_budget
            .checked_sub(1)
            .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
        Ok(())
    };
    let result = match iter.next() {
        None => {
            // 1 引数の CONCAT は空文字との結合として扱う（NULL を返さない契約と
            // 一貫させる）。ラッパーノードを 1 個生成する。
            charge_one_node()?;
            BoundExpr::Builtin {
                f: BuiltinFn::Concat2,
                args: vec![first, BoundExpr::Text(String::new())],
            }
        }
        Some(second) => {
            // `first` はここで 1 回だけ最初のラッパーノードへ move する
            // （複製しない。旧実装の `fold` はクロージャに `first` を参照
            // キャプチャしていたため 1 回目の反復でのみ `first.clone()` が
            // 必要だったが、この明示ループ構造では `first` を直接 move できる）。
            charge_one_node()?;
            let mut acc = BoundExpr::Builtin {
                f: BuiltinFn::Concat2,
                args: vec![first, second],
            };
            for next in iter {
                charge_one_node()?;
                acc = BoundExpr::Builtin {
                    f: BuiltinFn::Concat2,
                    args: vec![acc, next],
                };
            }
            acc
        }
    };
    Ok((result, ExprType::Text))
}

/// `date_part(field, src)`／`date_trunc(unit, src)`（対象ビヘイビア: SQL-26。
/// Issue #920。`EXTRACT(field FROM src)` は構文段でこの形へ脱糖しないため
/// 本 Issue の対象外——`docs/design/datetime-scalar-functions.md` 参照）。
/// `field`／`unit` は束縛後の第 1 引数が [`BoundExpr::Text`] リテラルである
/// 場合のみ解決する（UDF パラメータ経由でも、展開後に Text リテラルであれば
/// 受理する。§2-4）。`src`（第 2 引数）は `DATE` または `TIMESTAMP` を受理し、
/// `DATE` は [`wrap_date_to_timestamp`] で昇格する。
fn bind_date_part_or_trunc(
    lower_name: &str,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if args.len() != 2 {
        return Err(SqlSurfaceError::undefined_function(format!(
            "function {lower_name} expects 2 argument(s), got {}",
            args.len()
        )));
    }
    // 添字アクセスを避け get() で取得する（coding-rust.md: 受信 SQL 経路での [] 禁止）。
    // 直前の args.len() != 2 検査により両方 Some になるが、兄弟実装（bind_substr・
    // bind_concat）の様式に合わせ、ここでも fail-closed な Err 経路を明示する。
    let field_arg = args.first().ok_or_else(|| {
        SqlSurfaceError::undefined_function(format!(
            "function {lower_name} expects 2 argument(s), got 0"
        ))
    })?;
    let src_arg = args.get(1).ok_or_else(|| {
        SqlSurfaceError::undefined_function(format!(
            "function {lower_name} expects 2 argument(s), got 1"
        ))
    })?;
    // codex 指摘対応（PR #1120）: 本 ADR の「NULL 入力はすべて strict」契約
    // （`docs/design/datetime-scalar-functions.md`）は第 1 引数（field/unit）
    // にも適用される。`bind_null_aware`（`CASE`/`COALESCE`/`NULLIF`/`CONCAT`
    // と共有）で `Expr::Null` を型未確定の `BoundExpr::Null` として受理し、
    // `field_ty` が `None`（裸の NULL）の場合は `DatePartField`／
    // `DateTruncUnit::from_name` による field 名解決自体を行わず、結果型を
    // 固定したまま無条件で `BoundExpr::Null` を返す（field が定まらない以上
    // `BuiltinFn::DatePart`／`DateTrunc` のペイロードを構築できないため）。
    // 第 2 引数（src）は型検査のため引き続き束縛する（strict 関数として、
    // NULL でない側の型不正は変わらず bind 時エラーにする）。
    let (field_bound, field_ty) = bind_null_aware(field_arg, env, node_budget)?;
    let result_ty = if lower_name == "date_part" {
        ExprType::Scalar
    } else {
        ExprType::Timestamp
    };
    let field_name = match field_ty {
        None => None,
        Some(ExprType::Text) => {
            let BoundExpr::Text(name) = field_bound else {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "function {lower_name} requires its first argument to be a literal (not a column reference or expression)"
                )));
            };
            Some(name)
        }
        Some(_) => {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "function {lower_name} expects a text literal as its first argument"
            )));
        }
    };
    let (src_bound, src_ty) = bind_null_aware(src_arg, env, node_budget)?;
    // `TIME` 入力（Issue #1570）。`date_part` は時刻成分と `epoch` だけを `TimePart` で受理し、
    // 日付由来の field は PostgreSQL と同じく `0A000`（束縛時に拒否して行ごとの評価へ持ち込まない）。
    // `date_trunc(text, time)` は PostgreSQL に存在しないため `42883`。
    if src_ty == Some(ExprType::Time) {
        if lower_name != "date_part" {
            return Err(SqlSurfaceError::undefined_function(
                "function date_trunc(text, time) does not exist",
            ));
        }
        let Some(field_name) = field_name else {
            return Ok((BoundExpr::Null, result_ty));
        };
        let field = DatePartField::from_name(&field_name).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unknown date_part field: {field_name}"))
        })?;
        if !datetime_fn::time_supports_field(field) {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: format!("date_part unit {field_name:?} is not supported for type time"),
            });
        }
        return Ok((
            BoundExpr::Builtin {
                f: BuiltinFn::TimePart(field),
                args: vec![src_bound],
            },
            result_ty,
        ));
    }
    let src = match src_ty {
        Some(ExprType::Timestamp) => src_bound,
        Some(ExprType::Date) => wrap_date_to_timestamp(src_bound),
        None => src_bound,
        _ => {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "function {lower_name} expects a DATE or TIMESTAMP as its second argument"
            )))
        }
    };
    let Some(field_name) = field_name else {
        // 第 1 引数が裸の NULL: field/unit が定まらないため、第 2 引数の値に
        // 関わらず無条件で NULL を返す（strict NULL 伝播）。
        return Ok((BoundExpr::Null, result_ty));
    };
    if lower_name == "date_part" {
        let field = DatePartField::from_name(&field_name).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unknown date_part field: {field_name}"))
        })?;
        Ok((
            BoundExpr::Builtin {
                f: BuiltinFn::DatePart(field),
                args: vec![src],
            },
            result_ty,
        ))
    } else {
        let unit = DateTruncUnit::from_name(&field_name).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unknown date_trunc unit: {field_name}"))
        })?;
        Ok((
            BoundExpr::Builtin {
                f: BuiltinFn::DateTrunc(unit),
                args: vec![src],
            },
            result_ty,
        ))
    }
}

/// `SUBSTR(s, start[, len])`（Issue #919・SQL-26）。引数 2 個は
/// [`BuiltinFn::Substr2`]、3 個は [`BuiltinFn::Substr3`] へ束縛する。
fn bind_substr(
    name: &str,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let builtin = match args.len() {
        2 => BuiltinFn::Substr2,
        3 => BuiltinFn::Substr3,
        got => {
            return Err(SqlSurfaceError::undefined_function(format!(
                "function {name} expects 2 or 3 arguments, got {got}"
            )))
        }
    };
    let (param_types, ret) = builtin_signature(builtin);
    let mut bound_args = Vec::with_capacity(args.len());
    for (a, expected) in args.iter().zip(param_types.iter()) {
        let (b, ty) = bind_expr_in(a, env, node_budget)?;
        if ty != *expected {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "function {name} argument type mismatch"
            )));
        }
        bound_args.push(b);
    }
    Ok((
        BoundExpr::Builtin {
            f: builtin,
            args: bound_args,
        },
        ret,
    ))
}

/// 行コンテキスト（行 `id`・その行の `VECTOR` 列の embedding）で束縛済み式を評価する。
/// `sql::exec` の RLS→SCALAR 段のフック（`WHERE` の式述語）・投影段（結果列の式）の
/// 両方から呼ばれる。呼び出し元は、可視行（RLS-8 の暗黙適用を通過した行）にのみ
/// 到達させる契約を守ること（本関数自体はその契約を検査しない）。
///
/// fail-closed: 0 除算・非有限値（NaN/∞）の生成は黙って 0 や NULL に丸めず、行単位で
/// `Err` として伝播する（0 除算は `22012`、あふれ・`f64` 正確表現域外は `22003`、その他は `22000`）。
/// 行 `id`（`u64`）を `ExprValue::Scalar`（`f64`）へ変換する前に、`f64` の 52 bit
/// 仮数部で正確に表現できる範囲（`2^53` 以下）かを確認する。これを超える `id` を
/// 無条件に `as f64` で丸めると、`WHERE id = <literal>` のような等価述語が精度欠落
/// により別 ID の行にも一致しうる（fail-closed: 黙って丸めず `22003`（`NumericOutOfRange`）で拒否する。TABLE-16・ERR-2・Issue #1336）。
pub(crate) fn id_as_finite_scalar(id: u64) -> Result<f64, SqlSurfaceError> {
    if id > MAX_EXACT_F64_INT {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "row id exceeds the range that can be exactly represented for comparison",
        ));
    }
    Ok(id as f64)
}

/// [`ScalarRef`] の数値 variant（`Integer`／`BigInt`／`Real`／`Double`）を
/// `BoundExpr::ColumnRef` の評価値として `f64` へ変換する（Issue #1075・
/// TABLE-16 ポインタ）。`id_as_finite_scalar` と同じ「`f64` で正確に表現できる
/// 範囲か」の境界（`2^53`）を `BIGINT` にも適用し、黙って丸めない
/// （fail-closed。security.md「不安全な設計」対応）。`Integer`／`Real` は
/// `f64` の 52 bit 仮数部で常に正確に表現できるため境界検査は不要。
/// `Real`／`Double` の非有限値（NaN/±∞）は `row_codec` が encode/decode の
/// いずれでも拒否するため通常到達しないが、防御的に拒否する。数値以外の
/// `ScalarRef` variant は束縛段の不変条件が崩れた場合の保険として `Internal`
/// にする（型不一致。行の値・テナント情報を含めない固定文言）。
pub(crate) fn numeric_scalar_from_ref(v: &ScalarRef<'_>) -> Result<f64, SqlSurfaceError> {
    match v {
        ScalarRef::Integer(i) => Ok(f64::from(*i)),
        ScalarRef::Real(r) => {
            let d = f64::from(*r);
            if d.is_finite() {
                Ok(d)
            } else {
                Err(SqlSurfaceError::invalid_input(
                    "numeric column value is not finite",
                ))
            }
        }
        ScalarRef::Double(d) => {
            if d.is_finite() {
                Ok(*d)
            } else {
                Err(SqlSurfaceError::invalid_input(
                    "numeric column value is not finite",
                ))
            }
        }
        ScalarRef::BigInt(b) => {
            if b.unsigned_abs() > MAX_EXACT_F64_INT {
                Err(SqlSurfaceError::numeric_out_of_range(
                    "BIGINT column value exceeds the range that can be exactly represented",
                ))
            } else {
                Ok(*b as f64)
            }
        }
        _ => Err(SqlSurfaceError::Internal {
            detail: "numeric column reference resolved to a non-numeric row scalar value"
                .to_string(),
        }),
    }
}

/// パス取得演算子の評価（Issue #1596・TABLE-25）。左辺・キー・添字のいずれかが NULL なら
/// 結果は NULL。キー不在・範囲外・容器の型不一致も NULL（`json::jsonb_access*`）。
/// 格納値のパース失敗は格納契約違反のため固定文言の `Internal`（行の値は含めない）。
fn eval_json_access<'a>(
    lhs: &BoundExpr,
    rhs: &JsonAccessRhs,
    as_text: bool,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let (stored, original_text) = match eval_with_scalars(lhs, id, embedding, row_scalars)? {
        ExprValue::Json(j) => (j, false),
        ExprValue::JsonText(j) => (j, true),
        ExprValue::Null => return Ok(ExprValue::Null),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JSON path operator left operand is not a json or jsonb value".to_string(),
            })
        }
    };
    let step = match rhs {
        JsonAccessRhs::Key(e) => match eval_with_scalars(e, id, embedding, row_scalars)? {
            ExprValue::Text(k) => Some(crate::json::JsonAccessStep::Key(k.into_owned())),
            ExprValue::Null => None,
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "JSON path operator key is not a text value".to_string(),
                })
            }
        },
        JsonAccessRhs::Index(e) => match eval_with_scalars(e, id, embedding, row_scalars)? {
            ExprValue::Scalar(v) => {
                if v.fract() != 0.0 || v < f64::from(i32::MIN) || v > f64::from(i32::MAX) {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "JSON array subscript is out of range for integer",
                    ));
                }
                Some(crate::json::JsonAccessStep::Index(v as i32))
            }
            ExprValue::Null => None,
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "JSON path operator subscript is not a numeric value".to_string(),
                })
            }
        },
        JsonAccessRhs::Path(_) => None,
    };
    if original_text {
        // `json`（原文保存）: 部分テキストを原文のまま返す（Issue #2180）。
        let found = match rhs {
            JsonAccessRhs::Path(path) => crate::json_text::json_text_get_path(&stored, path),
            _ => match &step {
                Some(crate::json::JsonAccessStep::Key(k)) => {
                    crate::json_text::json_text_get_field(&stored, k)
                }
                Some(crate::json::JsonAccessStep::Index(i)) => {
                    crate::json_text::json_text_get_index(&stored, *i)
                }
                None => return Ok(ExprValue::Null),
            },
        }
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "stored json value could not be parsed".to_string(),
        })?;
        let Some(span) = found else {
            return Ok(ExprValue::Null);
        };
        if as_text {
            return Ok(
                match crate::json_text::json_text_as_text(span).map_err(|_| {
                    SqlSurfaceError::Internal {
                        detail: "stored json value could not be parsed".to_string(),
                    }
                })? {
                    Some(t) => ExprValue::Text(Cow::Owned(t)),
                    None => ExprValue::Null,
                },
            );
        }
        return Ok(ExprValue::JsonText(Cow::Owned(span.to_string())));
    }
    let result = match rhs {
        JsonAccessRhs::Path(path) => crate::json::jsonb_access_path(&stored, path, as_text),
        _ => match step {
            Some(step) => crate::json::jsonb_access(&stored, &step, as_text),
            None => return Ok(ExprValue::Null),
        },
    }
    .map_err(|_| SqlSurfaceError::Internal {
        detail: "stored jsonb value could not be parsed".to_string(),
    })?;
    Ok(match result {
        None => ExprValue::Null,
        Some(text) if as_text => ExprValue::Text(Cow::Owned(text)),
        Some(text) => ExprValue::Json(Cow::Owned(text)),
    })
}

/// [`crate::json::JsonColumnError`] を SQL 表層のエラーへ写す（評価時。上限超過は `54000`、
/// 格納値の破損は格納契約違反として固定文言の `Internal`。行の値・リテラル本文は含めない）。
fn json_predicate_error(e: crate::json::JsonColumnError) -> SqlSurfaceError {
    match e {
        crate::json::JsonColumnError::TooLong => SqlSurfaceError::payload_too_large(
            "jsonb containment evaluation exceeds the allowed size",
        ),
        crate::json::JsonColumnError::Invalid => SqlSurfaceError::Internal {
            detail: "stored jsonb value could not be parsed".to_string(),
        },
    }
}

/// 包含・存在演算子の評価（Issue #1598・TABLE-25）。左辺・右辺（jsonb 式・キー式）の
/// いずれかが NULL なら結果は NULL。`?|`／`?&` のキー集合の NULL 要素は読み飛ばす
/// （PostgreSQL の `jsonb_exists_any`／`jsonb_exists_all` と同じ。`#>` の NULL 伝播とは異なる）。
fn eval_json_predicate<'a>(
    op: JsonPredicateOp,
    lhs: &BoundExpr,
    rhs: &JsonPredicateRhs,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let stored = match eval_with_scalars(lhs, id, embedding, row_scalars)? {
        ExprValue::Json(j) => j,
        ExprValue::Null => return Ok(ExprValue::Null),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JSON predicate left operand is not a jsonb value".to_string(),
            })
        }
    };
    let result = match rhs {
        JsonPredicateRhs::Jsonb(e) => {
            let other = match eval_with_scalars(e, id, embedding, row_scalars)? {
                ExprValue::Json(j) => j,
                ExprValue::Null => return Ok(ExprValue::Null),
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JSON predicate right operand is not a jsonb value".to_string(),
                    })
                }
            };
            match op {
                JsonPredicateOp::ContainedBy => crate::json::jsonb_contains(&other, &stored),
                _ => crate::json::jsonb_contains(&stored, &other),
            }
        }
        JsonPredicateRhs::Key(e) => match eval_with_scalars(e, id, embedding, row_scalars)? {
            ExprValue::Text(k) => crate::json::jsonb_exists(&stored, &k),
            ExprValue::Null => return Ok(ExprValue::Null),
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "JSON predicate key is not a text value".to_string(),
                })
            }
        },
        JsonPredicateRhs::Keys(keys) => match op {
            JsonPredicateOp::ExistsAll => crate::json::jsonb_exists_all(&stored, keys),
            _ => crate::json::jsonb_exists_any(&stored, keys),
        },
    };
    result.map(ExprValue::Bool).map_err(json_predicate_error)
}

/// [`crate::json::JsonModifyError`] を SQL 表層のエラーへ写す（評価時。固定の英語文言のみで、
/// 行の値・リテラル本文・他テナントの情報は含めない）。
fn json_modify_error(e: crate::json::JsonModifyError) -> SqlSurfaceError {
    use crate::json::JsonModifyError as E;
    match e {
        E::TooLong => {
            SqlSurfaceError::payload_too_large("jsonb modification result exceeds the allowed size")
        }
        E::InvalidStored => SqlSurfaceError::Internal {
            detail: "stored jsonb value could not be parsed".to_string(),
        },
        E::DeleteFromScalar => SqlSurfaceError::InvalidParameterValue {
            detail: "cannot delete from scalar",
        },
        E::DeleteFromObjectByIndex => SqlSurfaceError::InvalidParameterValue {
            detail: "cannot delete from object using integer index",
        },
        E::DeletePathInScalar => SqlSurfaceError::InvalidParameterValue {
            detail: "cannot delete path in scalar",
        },
        E::PathElementNotInteger => SqlSurfaceError::InvalidParameterValue {
            detail: "path element is not an integer",
        },
    }
}

/// 連結・削除演算子の評価（Issue #2181・TABLE-25）。左辺・右辺の式のいずれかが NULL なら
/// 結果は NULL（strict）。添字は `eval_json_access` と同じ検査（小数部なし・`i32` 範囲内）を行う。
fn eval_json_modify<'a>(
    op: JsonModifyOp,
    lhs: &BoundExpr,
    rhs: &JsonModifyRhs,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let stored = match eval_with_scalars(lhs, id, embedding, row_scalars)? {
        ExprValue::Json(j) => j,
        ExprValue::Null => return Ok(ExprValue::Null),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JSON modify operator left operand is not a jsonb value".to_string(),
            })
        }
    };
    let result = match (op, rhs) {
        (JsonModifyOp::Concat, JsonModifyRhs::Jsonb(e)) => {
            match eval_with_scalars(e, id, embedding, row_scalars)? {
                ExprValue::Json(other) => crate::json::jsonb_concat(&stored, &other),
                ExprValue::Null => return Ok(ExprValue::Null),
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JSON modify operator right operand is not a jsonb value"
                            .to_string(),
                    })
                }
            }
        }
        (JsonModifyOp::DeleteKey, JsonModifyRhs::Key(e)) => {
            match eval_with_scalars(e, id, embedding, row_scalars)? {
                ExprValue::Text(k) => crate::json::jsonb_delete_keys(&stored, &[k.as_ref()]),
                ExprValue::Null => return Ok(ExprValue::Null),
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JSON modify operator key is not a text value".to_string(),
                    })
                }
            }
        }
        (JsonModifyOp::DeleteIndex, JsonModifyRhs::Index(e)) => {
            match eval_with_scalars(e, id, embedding, row_scalars)? {
                ExprValue::Scalar(v) => {
                    if v.fract() != 0.0 || v < f64::from(i32::MIN) || v > f64::from(i32::MAX) {
                        return Err(SqlSurfaceError::numeric_out_of_range(
                            "JSON array subscript is out of range for integer",
                        ));
                    }
                    crate::json::jsonb_delete_index(&stored, v as i32)
                }
                ExprValue::Null => return Ok(ExprValue::Null),
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "JSON modify operator subscript is not a numeric value".to_string(),
                    })
                }
            }
        }
        (JsonModifyOp::DeletePath, JsonModifyRhs::Path(path)) => {
            let refs: Vec<&str> = path.iter().map(String::as_str).collect();
            crate::json::jsonb_delete_path(&stored, &refs)
        }
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JSON modify operator and operand kind do not match".to_string(),
            })
        }
    };
    result
        .map(|text| ExprValue::Json(Cow::Owned(text)))
        .map_err(json_modify_error)
}

/// jsonb 構築・変換で起きた [`crate::json::JsonColumnError`] を SQL 表層のエラーへ写す
/// （上限超過は `54000`、入力 jsonb のパース失敗は格納契約違反として固定文言の `Internal`）。
fn json_build_error(e: crate::json::JsonColumnError) -> SqlSurfaceError {
    match e {
        crate::json::JsonColumnError::TooLong => {
            SqlSurfaceError::payload_too_large("jsonb result exceeds the allowed size")
        }
        crate::json::JsonColumnError::Invalid => SqlSurfaceError::Internal {
            detail: "stored jsonb value could not be parsed".to_string(),
        },
    }
}

/// 評価済みの値を JSON 値へ変換する（`to_jsonb`・`jsonb_build_*` の値側。Issue #1599）。
/// SQL の NULL は JSON の `null`（`to_jsonb` は呼び出し側が先に NULL を返す）。
fn json_value_from_expr_value(
    v: &ExprValue<'_>,
) -> Result<crate::json::JsonValue, SqlSurfaceError> {
    use crate::json::JsonValue;
    Ok(match v {
        ExprValue::Null => JsonValue::Null,
        ExprValue::Bool(b) => JsonValue::Bool(*b),
        ExprValue::Scalar(n) => crate::json::json_value_from_f64(*n),
        ExprValue::Text(t) => JsonValue::String(t.to_string()),
        ExprValue::Date(d) => crate::json::json_value_from_date(*d),
        ExprValue::Timestamp(t) => crate::json::json_value_from_timestamp(*t),
        ExprValue::Time(t) => JsonValue::String(crate::datetime::format_time(*t)),
        ExprValue::Json(j) => crate::json::parse_jsonb_for_build(j).map_err(json_build_error)?,
        ExprValue::Vector(_) => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "a vector value cannot be converted to jsonb".to_string(),
            })
        }
        // json（原文保存型）は束縛段で jsonb 関数の引数から弾かれる（42883）ため到達しない。
        ExprValue::JsonText(_) => {
            return Err(SqlSurfaceError::Internal {
                detail: "json text value reached a jsonb function".to_string(),
            })
        }
    })
}

/// `jsonb_build_object` のキーとして値を文字列化する。NULL は `22004`、jsonb（容器・
/// スカラーとも）は `22023`（キーはスカラーのみ）。
fn json_object_key(v: &ExprValue<'_>) -> Result<String, SqlSurfaceError> {
    match v {
        ExprValue::Null => Err(SqlSurfaceError::NullValueNotAllowed {
            detail: "argument of jsonb_build_object key position cannot be null",
        }),
        ExprValue::Text(t) => Ok(t.to_string()),
        ExprValue::Bool(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        ExprValue::Scalar(n) => Ok(crate::json::json_number_key_text(*n)),
        ExprValue::Date(d) => Ok(crate::datetime::format_date(*d)),
        ExprValue::Timestamp(t) => Ok(crate::json::json_timestamp_text(*t)),
        ExprValue::Time(t) => Ok(crate::datetime::format_time(*t)),
        ExprValue::Json(_) | ExprValue::JsonText(_) | ExprValue::Vector(_) => {
            Err(SqlSurfaceError::InvalidParameterValue {
                detail: "key value must be scalar, not array, composite, or json",
            })
        }
    }
}

/// jsonb 関数（[`BoundExpr::JsonFn`]。Issue #1599・TABLE-25）の評価。`typeof`・`array_length`・
/// `extract_path(_text)`・`to_jsonb` は strict（NULL 入力は NULL。`extract_path` はパス要素の
/// NULL も NULL）、`build_*` は値の NULL を JSON の `null` にする。エラーの文言は固定の英語で、
/// 行の値・キーは含めない。
fn eval_json_fn<'a>(
    f: JsonFnKind,
    args: &[BoundExpr],
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let internal = |detail: &str| SqlSurfaceError::Internal {
        detail: detail.to_string(),
    };
    let mut values: Vec<ExprValue<'a>> = Vec::with_capacity(args.len());
    for a in args {
        values.push(eval_with_scalars(a, id, embedding, row_scalars)?);
    }
    match f {
        JsonFnKind::Typeof | JsonFnKind::ArrayLength | JsonFnKind::ToJsonb => {
            let Some(v) = values.first() else {
                return Err(internal("missing function argument at evaluation time"));
            };
            if matches!(v, ExprValue::Null) {
                return Ok(ExprValue::Null);
            }
            match f {
                JsonFnKind::Typeof => {
                    let ExprValue::Json(j) = v else {
                        return Err(internal("jsonb_typeof argument is not a jsonb value"));
                    };
                    let t = crate::json::jsonb_typeof(j).map_err(json_build_error)?;
                    Ok(ExprValue::Text(Cow::Borrowed(t)))
                }
                JsonFnKind::ArrayLength => {
                    let ExprValue::Json(j) = v else {
                        return Err(internal("jsonb_array_length argument is not a jsonb value"));
                    };
                    match crate::json::jsonb_array_length(j) {
                        Ok(n) => Ok(ExprValue::Scalar(n as f64)),
                        Err(crate::json::JsonArrayLengthError::Scalar) => {
                            Err(SqlSurfaceError::InvalidParameterValue {
                                detail: "cannot get array length of a scalar",
                            })
                        }
                        Err(crate::json::JsonArrayLengthError::NotArray) => {
                            Err(SqlSurfaceError::InvalidParameterValue {
                                detail: "cannot get array length of a non-array",
                            })
                        }
                        Err(crate::json::JsonArrayLengthError::Stored(e)) => {
                            Err(json_build_error(e))
                        }
                    }
                }
                _ => {
                    check_json_build_input_size(&values)?;
                    let jv = json_value_from_expr_value(v)?;
                    let text =
                        crate::json::serialize_jsonb_bounded(&jv).map_err(json_build_error)?;
                    Ok(ExprValue::Json(Cow::Owned(text)))
                }
            }
        }
        JsonFnKind::ExtractPath | JsonFnKind::ExtractPathText => {
            let mut iter = values.iter();
            let stored = match iter.next() {
                Some(ExprValue::Json(j)) => j,
                Some(ExprValue::Null) => return Ok(ExprValue::Null),
                _ => return Err(internal("jsonb_extract_path argument is not a jsonb value")),
            };
            let mut path: Vec<Option<String>> = Vec::with_capacity(values.len());
            for v in iter {
                match v {
                    ExprValue::Text(t) => path.push(Some(t.to_string())),
                    ExprValue::Null => return Ok(ExprValue::Null),
                    _ => return Err(internal("jsonb_extract_path path element is not text")),
                }
            }
            let as_text = f == JsonFnKind::ExtractPathText;
            let result =
                crate::json::jsonb_access_path(stored, &path, as_text).map_err(json_build_error)?;
            Ok(match result {
                None => ExprValue::Null,
                Some(t) if as_text => ExprValue::Text(Cow::Owned(t)),
                Some(t) => ExprValue::Json(Cow::Owned(t)),
            })
        }
        JsonFnKind::BuildArray => {
            check_json_build_input_size(&values)?;
            let mut items = Vec::with_capacity(values.len());
            for v in &values {
                items.push(json_value_from_expr_value(v)?);
            }
            let text = crate::json::serialize_jsonb_bounded(&crate::json::JsonValue::Array(items))
                .map_err(json_build_error)?;
            Ok(ExprValue::Json(Cow::Owned(text)))
        }
        JsonFnKind::BuildObject => {
            // 引数個数の検査は PostgreSQL と同じく実行時（空の表では評価されず成功する）。
            if !values.len().is_multiple_of(2) {
                return Err(SqlSurfaceError::InvalidParameterValue {
                    detail: "argument list must have even number of elements",
                });
            }
            // 重複キーは後の値が勝つため、入力総量は出力サイズの下限にならない。先に全キーを
            // 評価して「キー → 最後の値の位置」を確定し（NULL／非スカラーのキーは上書きされる
            // ペアでも拒否する）、最終的に残るエントリだけをサイズ検査・パースする。
            let pairs = values.as_chunks::<2>().0;
            let mut last: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for (i, [k, v]) in pairs.iter().enumerate() {
                if matches!(v, ExprValue::Vector(_)) {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: "a vector value cannot be converted to jsonb".to_string(),
                    });
                }
                last.insert(json_object_key(k)?, i);
            }
            let mut map = std::collections::BTreeMap::new();
            let mut retained = 0usize;
            for (key, i) in last {
                let Some([_, v]) = pairs.get(i) else {
                    return Err(internal("object entry index out of range"));
                };
                let vlen = match v {
                    ExprValue::Text(t) => t.len(),
                    ExprValue::Json(j) => j.len(),
                    _ => 0,
                };
                let entry_len = key.len().saturating_add(vlen);
                retained = retained.saturating_add(entry_len);
                // 最終エントリの総量が上限超過なら、パース前に拒否する。
                if retained > crate::json::MAX_JSON_FIELD_LEN {
                    return Err(SqlSurfaceError::payload_too_large(
                        "jsonb result exceeds the allowed size",
                    ));
                }
                let parsed = json_value_from_expr_value(v)?;
                map.insert(key, (parsed, entry_len));
            }
            let map = map.into_iter().map(|(k, (v, _))| (k, v)).collect();
            let text = crate::json::serialize_jsonb_bounded(&crate::json::JsonValue::Object(map))
                .map_err(json_build_error)?;
            Ok(ExprValue::Json(Cow::Owned(text)))
        }
    }
}

/// 構築系の入力（text・jsonb 引数）のバイト長の総和が jsonb の上限を超えるときは、
/// パース・複製の前に `54000` で拒否する。出力は入力の正規化テキストを含むため総和未満には
/// ならず、正常な結果を誤って拒否することはない（DoS 対策: 引数 32 個ぶんの巨大値の
/// 同時展開を防ぐ）。
fn check_json_build_input_size(values: &[ExprValue<'_>]) -> Result<(), SqlSurfaceError> {
    let mut total = 0usize;
    for v in values {
        let len = match v {
            ExprValue::Text(t) => t.len(),
            ExprValue::Json(j) => j.len(),
            _ => 0,
        };
        total = total.saturating_add(len);
    }
    if total > crate::json::MAX_JSON_FIELD_LEN {
        return Err(SqlSurfaceError::payload_too_large(
            "jsonb result exceeds the allowed size",
        ));
    }
    Ok(())
}
pub fn eval<'a>(
    expr: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    eval_with_scalars(expr, id, embedding, &[])
}

/// [`eval`] の行スカラー対応版（Issue #919・SQL-26、§3-7。`DATE`／`TIMESTAMP`
/// 対応は対象ビヘイビア: SQL-26、Issue #920）。`row_scalars` は `schema.columns`
/// と同じ論理列インデックスを持つ行スカラー値（`TEXT`／`DATE`／`TIMESTAMP`）の
/// ビューで、`BoundExpr::TextColumnRef`／`DateColumnRef`／`TimestampColumnRef`
/// の解決に使う。要素は「その行のその列値」を表し、`None` は実 NULL を意味する。
/// 呼び出し元は、式が参照しうる列を事前に `mark_referenced_scalar_columns` で
/// マスクへ反映してから正しくデコードする責務を負う（`sql::scan`／
/// `sql::aggregate`／`sql::group_by`／`sql::exec`／`sql::check_constraint`
/// それぞれの呼び出し元を参照）。`row_scalars` の長さが参照インデックスに
/// 満たない場合（呼び出し元の配線不備）は、実 NULL と取り違えず `Internal` として
/// fail-closed に拒否する。列の型と参照 variant が食い違う場合（束縛段の
/// 不変条件が崩れた場合の保険）も同様に `Internal` とする。[`eval`]（引数 3 個版）
/// は `row_scalars` を空スライスで呼ぶ薄いラッパーとして残す（差分テスト・
/// 列参照を含まない式のみを評価する既存呼び出し元向け）。
pub(crate) fn eval_with_scalars<'a>(
    expr: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    match expr {
        BoundExpr::Number(v) => Ok(ExprValue::Scalar(*v)),
        // リテラルの寿命は `expr`（呼び出し元が指定する任意の借用）に紐付き、
        // 戻り値のライフタイム `'a`（`embedding`/`row_scalars` と共有）より
        // 短い場合がありうるため、常に複製して返す（`VectorRef`／列参照の
        // 借用最適化とは異なる）。
        BoundExpr::Text(s) => Ok(ExprValue::Text(Cow::Owned(s.clone()))),
        BoundExpr::Date(d) => Ok(ExprValue::Date(*d)),
        BoundExpr::Timestamp(t) => Ok(ExprValue::Timestamp(*t)),
        BoundExpr::Time(t) => Ok(ExprValue::Time(*t)),
        BoundExpr::TextColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "TEXT column reference is outside the decoded row scalar view".to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(v)) => match v.as_text() {
                Some(s) => Ok(ExprValue::Text(Cow::Borrowed(s))),
                None => Err(SqlSurfaceError::Internal {
                    detail: "TEXT column reference resolved to a non-TEXT row scalar value"
                        .to_string(),
                }),
            },
        },
        BoundExpr::DateColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "DATE column reference is outside the decoded row scalar view".to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(ScalarRef::Date(d))) => Ok(ExprValue::Date(*d)),
            Some(Some(_)) => Err(SqlSurfaceError::Internal {
                detail: "DATE column reference resolved to a non-DATE row scalar value".to_string(),
            }),
        },
        BoundExpr::TimestampColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "TIMESTAMP column reference is outside the decoded row scalar view"
                    .to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(ScalarRef::Timestamp(t))) => Ok(ExprValue::Timestamp(*t)),
            Some(Some(_)) => Err(SqlSurfaceError::Internal {
                detail: "TIMESTAMP column reference resolved to a non-TIMESTAMP row scalar value"
                    .to_string(),
            }),
        },
        BoundExpr::TimeColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "TIME column reference is outside the decoded row scalar view".to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(ScalarRef::Time(t))) => Ok(ExprValue::Time(*t)),
            Some(Some(_)) => Err(SqlSurfaceError::Internal {
                detail: "TIME column reference resolved to a non-TIME row scalar value".to_string(),
            }),
        },
        BoundExpr::ColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "NUMERIC column reference is outside the decoded row scalar view"
                    .to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(v)) => numeric_scalar_from_ref(v).map(ExprValue::Scalar),
        },
        BoundExpr::JsonbColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "JSONB column reference is outside the decoded row scalar view".to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(ScalarRef::Json(text))) => Ok(ExprValue::Json(Cow::Borrowed(text))),
            Some(Some(_)) => Err(SqlSurfaceError::Internal {
                detail: "JSONB column reference resolved to a non-JSON row scalar value"
                    .to_string(),
            }),
        },
        BoundExpr::JsonColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "JSON column reference is outside the decoded row scalar view".to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(ScalarRef::Json(text))) => Ok(ExprValue::JsonText(Cow::Borrowed(text))),
            Some(Some(_)) => Err(SqlSurfaceError::Internal {
                detail: "JSON column reference resolved to a non-JSON row scalar value".to_string(),
            }),
        },
        BoundExpr::JsonTextFn(f) => {
            crate::sql::json_fn::eval_json_fn(f, id, embedding, row_scalars)
        }
        BoundExpr::JsonbLiteral(text) => Ok(ExprValue::Json(Cow::Owned(text.clone()))),
        BoundExpr::JsonAccess { lhs, rhs, as_text } => {
            eval_json_access(lhs, rhs, *as_text, id, embedding, row_scalars)
        }
        BoundExpr::JsonModify { op, lhs, rhs } => {
            eval_json_modify(*op, lhs, rhs, id, embedding, row_scalars)
        }
        BoundExpr::JsonPredicate { op, lhs, rhs } => {
            eval_json_predicate(*op, lhs, rhs, id, embedding, row_scalars)
        }
        BoundExpr::JsonFn { f, args } => eval_json_fn(*f, args, id, embedding, row_scalars),
        BoundExpr::IdRef => id_as_finite_scalar(id).map(ExprValue::Scalar),
        BoundExpr::IdCompare { op, value } => Ok(ExprValue::Bool(id_compare(*op, id, *value))),
        BoundExpr::VectorRef => {
            // Issue #352: 行の embedding をそのまま借用する。テーブル `VECTOR` 列の
            // 素通し参照（`WHERE vec_norm(embedding) > x` 等の読み取り経路）では
            // 確保・複製が一切発生しない。新しいベクトルを構築する評価
            // （`vec_div`・vector×scalar 演算）だけが `try_reserve_exact` による
            // fail-closed な確保を行う（下記 `eval_builtin`・`apply_vector_scalar_op`
            // 参照）。呼び出し元が所有データを要する場合は [`into_owned_vector`] で
            // 変換する（確保は投影段など必要な箇所のみへ限定される）。
            //
            // `embedding` が空スライスの行は `VECTOR` 列が NULL（`dim == 0`。
            // 呼び出し元は常に「非 NULL なら実データ・NULL なら空スライス」で
            // 揃えて渡す契約。`sql::scan`・`sql::where_tree`・
            // `sql::check_constraint`・`sql::exec` 参照）であるため、ここで NULL
            // として評価する。`CASE`／`COALESCE` は選ばれない分岐を評価しない
            // （このモジュールの `Case`／`Coalesce` 分岐が短絡する）ため、
            // 実際に選択された枝が `VectorRef` を含む場合にのみ NULL が伝播する
            // （codex-review P1 指摘対応: 未選択分岐に embedding 参照があるだけで
            // 行全体を NULL 扱いにしていた旧実装の修正。呼び出し元の事前
            // `references_embedding && dim == 0` ゲートは静的な式木走査で
            // 選択されない分岐まで拾ってしまうため撤去し、この評価時点の判定へ
            // 一本化した）。
            if embedding.is_empty() {
                Ok(ExprValue::Null)
            } else {
                Ok(ExprValue::Vector(Cow::Borrowed(embedding)))
            }
        }
        BoundExpr::Builtin { f, args } => eval_builtin(*f, args, id, embedding, row_scalars),
        BoundExpr::Binary { op, lhs, rhs } => {
            let l = eval_with_scalars(lhs, id, embedding, row_scalars)?;
            let r = eval_with_scalars(rhs, id, embedding, row_scalars)?;
            eval_binary(*op, l, r)
        }
        BoundExpr::WasmCall { backend, args, .. } => {
            // ABI 固定シグネチャ（bind_call が保証）: args[0] = Vector, args[1] = Scalar。
            // どちらも `TEXT` 型を取らないため `row_scalars` はこの階層では
            // 未使用だが、更に内側の入れ子式（例: `wasm_fn(embedding,
            // length(label))`）が `TextColumnRef` を含みうるため下位呼び出しへは
            // 引き続き渡す（Issue #919・SQL-26）。
            let v_val = match args.first() {
                Some(e) => eval_with_scalars(e, id, embedding, row_scalars)?,
                None => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "missing function argument at evaluation time".to_string(),
                    })
                }
            };
            let s_val = match args.get(1) {
                Some(e) => eval_with_scalars(e, id, embedding, row_scalars)?,
                None => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "missing function argument at evaluation time".to_string(),
                    })
                }
            };
            // WASM UDF は RETURNS NULL ON NULL INPUT として扱う（対象ビヘイビア:
            // SQL-26。Issue #921）: いずれかの引数が NULL ならバックエンドを一切
            // 呼ばず NULL を返す。ABI（`(Vector, Scalar) -> Scalar`）に NULL を
            // 表現する値が無いため、NULL をバックエンド越しに送らない判断。
            if matches!(v_val, ExprValue::Null) || matches!(s_val, ExprValue::Null) {
                return Ok(ExprValue::Null);
            }
            let v = match v_val {
                ExprValue::Vector(v) => v,
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "function argument type mismatch at evaluation time".to_string(),
                    })
                }
            };
            let s = match s_val {
                ExprValue::Scalar(s) => s,
                _ => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "function argument type mismatch at evaluation time".to_string(),
                    })
                }
            };
            // バックエンドの失敗（deadline 超過・トラップ・メモリ確保失敗・
            // `Mutex` poison 等）は種別を問わずすべて `22000` に写像する（行値・
            // テナント情報を含まない固定文言。`crate::wasm_udf::WasmUdfError` の
            // `Display` を利用。EXT-6 の拒否・強制中断はここで行単位のエラーへ
            // 収束し、プロセスは生存する）。
            let result = backend
                .call_vector_scalar(&v, s)
                .map_err(|e| SqlSurfaceError::invalid_input(e.to_string()))?;
            finite_scalar(result, "wasm udf")
        }
        BoundExpr::Null => Ok(ExprValue::Null),
        // Issue #919・SQL-26 と Issue #921・SQL-26 の合流点: `CASE`／
        // `COALESCE`／`NULLIF` の分岐は `row_scalars` を持つ `eval_with_scalars`
        // で再帰する（引数 3 個版の `eval` は `row_scalars` を常に空スライスへ
        // 縮退させるため、`COALESCE(LOWER(text_col), 'x')` のように分岐が
        // `TextColumnRef` を含む式で誤った `Internal` 拒否になっていた）。
        BoundExpr::Case { whens, else_result } => {
            for (cond, result) in whens {
                match eval_with_scalars(cond, id, embedding, row_scalars)? {
                    ExprValue::Bool(true) => {
                        return eval_with_scalars(result, id, embedding, row_scalars)
                    }
                    ExprValue::Bool(false) | ExprValue::Null => continue,
                    _ => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "CASE condition did not evaluate to boolean".to_string(),
                        })
                    }
                }
            }
            eval_with_scalars(else_result, id, embedding, row_scalars)
        }
        BoundExpr::Coalesce(args) => {
            for a in args {
                match eval_with_scalars(a, id, embedding, row_scalars)? {
                    ExprValue::Null => continue,
                    other => return Ok(other),
                }
            }
            Ok(ExprValue::Null)
        }
        BoundExpr::NullIf { lhs, rhs } => {
            let l = eval_with_scalars(lhs, id, embedding, row_scalars)?;
            let r = eval_with_scalars(rhs, id, embedding, row_scalars)?;
            eval_nullif(l, r)
        }
    }
}

/// `NULLIF(lhs, rhs)` を値ベースで評価する（`CASE WHEN lhs = rhs THEN NULL ELSE
/// lhs END` と等価な意味論。対象ビヘイビア: SQL-26）。再帰 `eval` の
/// `BoundExpr::NullIf` 分岐と `sql::expr_program::ExprProgram::eval` の
/// `ExprStep::NullIf` 分岐が共有する（Issue #353 と同じ「値ベース評価を 1 箇所に
/// 保つ」方針）。束縛段（[`bind_nullif`]）が両辺を `Scalar`／`Text`／`Date`／
/// `Timestamp` のいずれか一方に限定・統一済み（`DATE`⋈`TIMESTAMP` の混在は
/// 束縛時に `DATE` 側を `TIMESTAMP` へ昇格済みのため、ここには来ない）のため、
/// 非 `Null`・非 `Scalar`・非 `Text`・非 `Date`・非 `Timestamp` の組み合わせ
/// （型が食い違う場合を含む）は束縛段の不変条件が崩れた場合の保険として
/// `Internal` に倒す。
pub(crate) fn eval_nullif<'a>(
    l: ExprValue<'a>,
    r: ExprValue<'a>,
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    match (l, r) {
        (ExprValue::Null, _) => Ok(ExprValue::Null),
        (l, ExprValue::Null) => Ok(l),
        (ExprValue::Scalar(a), ExprValue::Scalar(b)) => {
            if a == b {
                Ok(ExprValue::Null)
            } else {
                Ok(ExprValue::Scalar(a))
            }
        }
        // codex-review／Cursor Bugbot 指摘対応: `=` 演算子が `(Text, Text) =>
        // Bool` を受理するようになったことに合わせ、`NULLIF` も TEXT 同士の
        // 組を受理する（PostgreSQL 互換。`bind_nullif` 参照）。
        (ExprValue::Text(a), ExprValue::Text(b)) => {
            if a == b {
                Ok(ExprValue::Null)
            } else {
                Ok(ExprValue::Text(a))
            }
        }
        // Cursor Bugbot 指摘対応（PR #1120）: `=` 演算子が `DATE`／`TIMESTAMP`
        // 同士の比較を受理するようになったことに合わせ、`NULLIF` も同じ組を
        // 受理する（`bind_nullif` 参照。`DATE`⋈`TIMESTAMP` は束縛時に昇格済み
        // のためここでは同種同士のみを扱う）。
        (ExprValue::Date(a), ExprValue::Date(b)) => {
            if a == b {
                Ok(ExprValue::Null)
            } else {
                Ok(ExprValue::Date(a))
            }
        }
        (ExprValue::Timestamp(a), ExprValue::Timestamp(b)) => {
            if a == b {
                Ok(ExprValue::Null)
            } else {
                Ok(ExprValue::Timestamp(a))
            }
        }
        (ExprValue::Time(a), ExprValue::Time(b)) => {
            if a == b {
                Ok(ExprValue::Null)
            } else {
                Ok(ExprValue::Time(a))
            }
        }
        _ => Err(SqlSurfaceError::Internal {
            detail: "NULLIF operand type mismatch at evaluation time".to_string(),
        }),
    }
}

/// 組み込み関数を木の再帰評価から呼ぶ経路（引数式を左から順に評価してから
/// [`apply_builtin`] へ委譲する）。`args` の評価順は元の実装（`eval_vector_arg`／
/// `eval_scalar_arg` を順に呼ぶ）と同じ左→右を保つ（評価順に依存するエラー発生
/// 順序を変えないため。`sql_evaluation_order` 等の既存契約に対応）。
fn eval_builtin<'a>(
    f: BuiltinFn,
    args: &[BoundExpr],
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    // 参照実装（再帰 `eval`）専用の非ホットパス。ステップ列実行
    // （`sql::expr_program::ExprProgram::eval`）は固定長スタック配列を使う別経路
    // （下記 `apply_builtin` のシグネチャ参照）を通るため、ここでの `Vec` 確保は
    // 行ごとのホットパスには影響しない。
    let mut values: Vec<Option<ExprValue<'a>>> = Vec::with_capacity(args.len());
    for a in args {
        values.push(Some(eval_with_scalars(a, id, embedding, row_scalars)?));
    }
    apply_builtin(f, &mut values)
}

/// 組み込み関数を値ベースで評価する（引数はすでに評価済みの [`ExprValue`] を
/// `&mut [Option<ExprValue>]` スライスで受け取り、式木を再帰評価しない）。
/// [`eval_builtin`]（再帰 `eval` 経由）と `sql::expr_program::ExprProgram::eval`
/// （`ExprStep::Builtin`。明示スタックから arity 分 pop した値を、行ループの外で
/// 確保済みの固定長配列へ積んで渡す。PR #373 codex-review 指摘対応・追加 `Vec`
/// 確保の排除）の両方が本関数を共有することで、0 除算・非有限値（`f64`/`f32`
/// 双方）の fail-closed 契約（`22000`）と `try_reserve_exact` による確保失敗時の
/// `54000` 写像を 1 箇所に保つ（Issue #353）。引数はスライスの `Option::take`
/// で所有権を抜き取る（[`take_vector_arg`]・[`take_scalar_arg`]）ため、呼び出し元
/// が `Vec` を所有している必要はない。`args` の要素数が [`builtin_signature`] の
/// arity と不一致な場合（束縛段の不変条件が崩れた場合の保険）は `Internal` として
/// 拒否する。
pub(crate) fn apply_builtin<'a>(
    f: BuiltinFn,
    args: &mut [Option<ExprValue<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    // 組み込み関数は strict 関数として扱う（対象ビヘイビア: SQL-26。Issue #921）:
    // いずれかの引数が NULL なら NULL を返す。残りの引数のスロットは未使用のまま
    // 破棄してよい（呼び出し元はこの 1 回の呼び出し後にスロットを再利用しない）。
    // `CONCAT`（Issue #919・SQL-26、AC2）だけは例外で、NULL を空文字として扱い
    // 常に非 NULL を返す唯一の組み込み関数のため、この一律 strict ガードでは
    // なく後段の `BuiltinFn::Concat2` 自身の分岐（`take_text_or_null_arg` の
    // `unwrap_or(Cow::Borrowed(""))`）に NULL 処理を委ねる（origin/main（Issue
    // #921）取り込み時、この一律ガードが先に働き `CONCAT` の非 strict 契約を
    // 踏みつぶしていたため是正）。
    if f != BuiltinFn::Concat2 && args.iter().any(|a| matches!(a, Some(ExprValue::Null))) {
        return Ok(ExprValue::Null);
    }
    match f {
        BuiltinFn::VecNorm => {
            let v = take_vector_arg(args, 0)?;
            let sum_sq: f64 = v.iter().map(|&x| (x as f64) * (x as f64)).sum();
            let norm = sum_sq.sqrt();
            finite_scalar(norm, "vec_norm")
        }
        BuiltinFn::VecSum => {
            let v = take_vector_arg(args, 0)?;
            let sum: f64 = v.iter().map(|&x| x as f64).sum();
            finite_scalar(sum, "vec_sum")
        }
        BuiltinFn::VecDiv => {
            let v = take_vector_arg(args, 0)?;
            let s = take_scalar_arg(args, 1)?;
            if s == 0.0 {
                return Err(SqlSurfaceError::division_by_zero("vec_div"));
            }
            // `vec_div` は成分ごとに新しい値を作る（借用元をそのまま流用できない）
            // ため、ここでは Issue #352 の限定どおり `try_reserve_exact` による
            // fail-closed な新規確保を維持する。
            let mut out: Vec<f32> = Vec::new();
            out.try_reserve_exact(v.len()).map_err(|_| {
                SqlSurfaceError::payload_too_large("vec_div result exceeds available memory")
            })?;
            for x in v.iter() {
                let r = (*x as f64) / s;
                if !r.is_finite() {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "vec_div: result is not finite",
                    ));
                }
                // f64 では有限でも `f32` へキャストした結果が `f32::MAX` を超えて
                // Infinity 化しうる。「非有限値は 22003 で fail-closed」の契約を
                // キャスト後の値にも適用し、結果へ Infinity を流出させない。
                let r32 = r as f32;
                if !r32.is_finite() {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "vec_div: result is not finite",
                    ));
                }
                out.push(r32);
            }
            Ok(ExprValue::Vector(Cow::Owned(out)))
        }
        // Issue #919・SQL-26: 文字列スカラー関数群。NULL 伝播規約（AC2）:
        // `CONCAT` 以外は strict（引数のいずれかが NULL なら NULL を返し、
        // `crate::sql::string_fn` の純粋関数を呼び出さない）。`CONCAT` は NULL を
        // 空文字として扱い常に非 NULL を返す。
        BuiltinFn::Lower | BuiltinFn::Upper | BuiltinFn::Trim => {
            match take_text_or_null_arg(args, 0)? {
                None => Ok(ExprValue::Null),
                Some(s) => {
                    let out = match f {
                        BuiltinFn::Lower => string_fn::lower(&s)?,
                        BuiltinFn::Upper => string_fn::upper(&s)?,
                        BuiltinFn::Trim => string_fn::trim(&s)?,
                        _ => unreachable!("guarded by outer match arm"),
                    };
                    Ok(ExprValue::Text(Cow::Owned(out)))
                }
            }
        }
        BuiltinFn::Length => match take_text_or_null_arg(args, 0)? {
            None => Ok(ExprValue::Null),
            Some(s) => finite_scalar(string_fn::length(&s), "length"),
        },
        BuiltinFn::Substr2 => {
            match (
                take_text_or_null_arg(args, 0)?,
                take_scalar_or_null_arg(args, 1)?,
            ) {
                (Some(s), Some(start)) => {
                    let out = string_fn::substr(&s, start, None)?;
                    Ok(ExprValue::Text(Cow::Owned(out)))
                }
                _ => Ok(ExprValue::Null),
            }
        }
        BuiltinFn::Substr3 => {
            match (
                take_text_or_null_arg(args, 0)?,
                take_scalar_or_null_arg(args, 1)?,
                take_scalar_or_null_arg(args, 2)?,
            ) {
                (Some(s), Some(start), Some(len)) => {
                    let out = string_fn::substr(&s, start, Some(len))?;
                    Ok(ExprValue::Text(Cow::Owned(out)))
                }
                _ => Ok(ExprValue::Null),
            }
        }
        BuiltinFn::Concat2 => {
            let a = take_text_or_null_arg(args, 0)?.unwrap_or(Cow::Borrowed(""));
            let b = take_text_or_null_arg(args, 1)?.unwrap_or(Cow::Borrowed(""));
            let out = string_fn::concat2(&a, &b)?;
            Ok(ExprValue::Text(Cow::Owned(out)))
        }
        BuiltinFn::Replace => {
            match (
                take_text_or_null_arg(args, 0)?,
                take_text_or_null_arg(args, 1)?,
                take_text_or_null_arg(args, 2)?,
            ) {
                (Some(s), Some(from), Some(to)) => {
                    let out = string_fn::replace(&s, &from, &to)?;
                    Ok(ExprValue::Text(Cow::Owned(out)))
                }
                _ => Ok(ExprValue::Null),
            }
        }
        BuiltinFn::Position => {
            match (
                take_text_or_null_arg(args, 0)?,
                take_text_or_null_arg(args, 1)?,
            ) {
                (Some(haystack), Some(needle)) => {
                    finite_scalar(string_fn::position(&haystack, &needle), "position")
                }
                _ => Ok(ExprValue::Null),
            }
        }
        BuiltinFn::Abs => {
            let x = take_scalar_arg(args, 0)?;
            crate::sql::numeric_fn::abs(x).map(ExprValue::Scalar)
        }
        BuiltinFn::Round1 => {
            let x = take_scalar_arg(args, 0)?;
            crate::sql::numeric_fn::round1(x).map(ExprValue::Scalar)
        }
        BuiltinFn::Round2 => {
            let x = take_scalar_arg(args, 0)?;
            let n = take_scalar_arg(args, 1)?;
            crate::sql::numeric_fn::round2(x, n).map(ExprValue::Scalar)
        }
        BuiltinFn::Floor => {
            let x = take_scalar_arg(args, 0)?;
            crate::sql::numeric_fn::floor(x).map(ExprValue::Scalar)
        }
        BuiltinFn::Ceil => {
            let x = take_scalar_arg(args, 0)?;
            crate::sql::numeric_fn::ceil(x).map(ExprValue::Scalar)
        }
        BuiltinFn::Mod => {
            let x = take_scalar_arg(args, 0)?;
            let y = take_scalar_arg(args, 1)?;
            crate::sql::numeric_fn::modulo(x, y).map(ExprValue::Scalar)
        }
        BuiltinFn::Power => {
            let x = take_scalar_arg(args, 0)?;
            let y = take_scalar_arg(args, 1)?;
            crate::sql::numeric_fn::power(x, y).map(ExprValue::Scalar)
        }
        BuiltinFn::Sqrt => {
            let x = take_scalar_arg(args, 0)?;
            crate::sql::numeric_fn::sqrt(x).map(ExprValue::Scalar)
        }
        // 対象ビヘイビア: SQL-26（Issue #920）。`date_part`／`date_trunc` の
        // 実行時引数は `src`（`Timestamp`）1 個のみ（field／unit は束縛済みの
        // `BuiltinFn` ペイロード。§2-4）。
        BuiltinFn::DatePart(field) => {
            let t = take_timestamp_arg(args, 0)?;
            finite_scalar(datetime_fn::date_part(field, t), "date_part")
        }
        BuiltinFn::TimePart(field) => {
            let t = take_time_arg(args, 0)?;
            let v = datetime_fn::time_part(field, t).ok_or_else(|| SqlSurfaceError::Internal {
                detail: "date_part field is not supported for TIME at evaluation time".to_string(),
            })?;
            finite_scalar(v, "date_part")
        }
        BuiltinFn::DateTrunc(unit) => {
            let t = take_timestamp_arg(args, 0)?;
            datetime_fn::date_trunc(unit, t).map(ExprValue::Timestamp)
        }
        BuiltinFn::DateToTimestamp => {
            let d = take_date_arg(args, 0)?;
            Ok(ExprValue::Timestamp(datetime_fn::date_to_timestamp(d)))
        }
        BuiltinFn::BpcharToText => match take_text_or_null_arg(args, 0)? {
            None => Ok(ExprValue::Null),
            Some(Cow::Borrowed(s)) => Ok(ExprValue::Text(Cow::Borrowed(s.trim_end_matches(' ')))),
            Some(Cow::Owned(mut s)) => {
                s.truncate(s.trim_end_matches(' ').len());
                Ok(ExprValue::Text(Cow::Owned(s)))
            }
        },
        BuiltinFn::TypedArith(op, kind) => {
            let a = take_scalar_arg(args, 0)?;
            let b = take_scalar_arg(args, 1)?;
            typed_arith(op, kind, a, b).map(ExprValue::Scalar)
        }
        // SQL-37（Issue #1586）: 評価のたびに壁時計を読む（束縛時に定数化しない）。
        BuiltinFn::ClockTimestamp => time_fn::now_micros().map(ExprValue::Timestamp),
    }
}

/// f64 で運ばれる整数値の厳密性の上限（2^53）。
const TYPED_INT_EXACT_MAX: i64 = 1 << 53;

/// [`BuiltinFn::TypedArith`] の評価本体（Issue #1539・SQL-42）。`Int4`／`Int8` は
/// オペランドを i64 へ戻して `checked_*` で計算し、溢れは `22003`、0 除算は `22012`。
/// 整数除算は 0 方向への切り捨て（PostgreSQL と同じ）。`Float4` は f64 で 1 演算して
/// から f32 へ丸め、非有限・アンダーフローを `22003` にする。オペランドが整数でない・
/// 2^53 を超える場合は束縛段の不変条件違反として `Internal`（fail-closed。panic しない）。
fn typed_arith(op: BinOp, kind: NumericKind, a: f64, b: f64) -> Result<f64, SqlSurfaceError> {
    match kind {
        NumericKind::Int2 | NumericKind::Int4 | NumericKind::Int8 => {
            let (x, y) = (exact_int(a)?, exact_int(b)?);
            let r = match op {
                BinOp::Add => x.checked_add(y),
                BinOp::Sub => x.checked_sub(y),
                BinOp::Mul => x.checked_mul(y),
                BinOp::Div => {
                    if y == 0 {
                        return Err(SqlSurfaceError::division_by_zero("/"));
                    }
                    x.checked_div(y)
                }
                BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "comparison operator in typed arithmetic evaluation".to_string(),
                    });
                }
            };
            let label = match kind {
                NumericKind::Int2 => "smallint out of range",
                NumericKind::Int4 => "integer out of range",
                _ => "bigint out of range",
            };
            let r = r.ok_or_else(|| SqlSurfaceError::numeric_out_of_range(label))?;
            let in_range = match kind {
                NumericKind::Int2 => i16::try_from(r).is_ok(),
                NumericKind::Int4 => i32::try_from(r).is_ok(),
                _ => r.unsigned_abs() <= TYPED_INT_EXACT_MAX as u64,
            };
            if !in_range {
                return Err(SqlSurfaceError::numeric_out_of_range(label));
            }
            Ok(r as f64)
        }
        NumericKind::Float4 => {
            let r = match op {
                BinOp::Add => a + b,
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => {
                    if b == 0.0 {
                        return Err(SqlSurfaceError::division_by_zero("/"));
                    }
                    a / b
                }
                BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "comparison operator in typed arithmetic evaluation".to_string(),
                    });
                }
            };
            let r32 = r as f32;
            let underflow = r32 == 0.0 && r != 0.0 && matches!(op, BinOp::Mul | BinOp::Div);
            if !r32.is_finite() || underflow {
                return Err(SqlSurfaceError::numeric_out_of_range("real out of range"));
            }
            Ok(f64::from(r32))
        }
    }
}

/// f64 で運ばれた整数値を i64 へ戻す。整数でない・|v| > 2^53 は束縛段の不変条件違反
/// （[`typed_arith`] の呼び出し元は整数型のオペランドしか渡さない）として `Internal`。
fn exact_int(v: f64) -> Result<i64, SqlSurfaceError> {
    if v.is_finite() && v.fract() == 0.0 && v.abs() <= TYPED_INT_EXACT_MAX as f64 {
        Ok(v as i64)
    } else {
        Err(SqlSurfaceError::Internal {
            detail: "typed integer arithmetic received a non-integer operand".to_string(),
        })
    }
}

/// `args[idx]` を `Text`（あれば）または `Null`（`ExprValue::Null` だった場合）
/// として取り出す（strict な文字列関数の NULL 伝播（AC2）を呼び出し元で
/// 一様に判定できるようにする共通ヘルパー）。型不一致・欠落は `Internal`。
fn take_text_or_null_arg<'a>(
    args: &mut [Option<ExprValue<'a>>],
    idx: usize,
) -> Result<Option<Cow<'a, str>>, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Text(s)) => Ok(Some(s)),
        Some(ExprValue::Null) => Ok(None),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// [`take_text_or_null_arg`] のスカラー版（`SUBSTR` の `start`/`len` 引数用）。
fn take_scalar_or_null_arg(
    args: &mut [Option<ExprValue<'_>>],
    idx: usize,
) -> Result<Option<f64>, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Scalar(s)) => Ok(Some(s)),
        Some(ExprValue::Null) => Ok(None),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// `args[idx]` を `Vector` として取り出す（`Option::take` で所有権を移す。
/// 同一インデックスを 2 度取り出さない呼び出し規約のため、取り出し後の
/// スロットは `None` のまま残る）。型不一致・欠落は `Internal`。
fn take_vector_arg<'a>(
    args: &mut [Option<ExprValue<'a>>],
    idx: usize,
) -> Result<Cow<'a, [f32]>, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Vector(v)) => Ok(v),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// `args[idx]` を `Scalar` として取り出す（[`take_vector_arg`] と対の値ベース
/// 抽出ヘルパー）。
fn take_scalar_arg(args: &mut [Option<ExprValue<'_>>], idx: usize) -> Result<f64, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Scalar(s)) => Ok(s),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// `args[idx]` を `Date` として取り出す（[`take_scalar_arg`] と対の値ベース
/// 抽出ヘルパー。対象ビヘイビア: SQL-26。Issue #920）。
fn take_date_arg(args: &mut [Option<ExprValue<'_>>], idx: usize) -> Result<i32, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Date(d)) => Ok(d),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// `args[idx]` を `Time` として取り出す（[`take_date_arg`] 参照。Issue #1570）。
fn take_time_arg(args: &mut [Option<ExprValue<'_>>], idx: usize) -> Result<i64, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Time(t)) => Ok(t),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// `args[idx]` を `Timestamp` として取り出す（[`take_date_arg`] 参照）。
fn take_timestamp_arg(
    args: &mut [Option<ExprValue<'_>>],
    idx: usize,
) -> Result<i64, SqlSurfaceError> {
    match args.get_mut(idx).and_then(Option::take) {
        Some(ExprValue::Timestamp(t)) => Ok(t),
        Some(_) => Err(SqlSurfaceError::Internal {
            detail: "function argument type mismatch at evaluation time".to_string(),
        }),
        None => Err(SqlSurfaceError::Internal {
            detail: "missing function argument at evaluation time".to_string(),
        }),
    }
}

/// 非有限値（NaN/∞）を fail-closed に拒否してスカラー値へ包む共通ヘルパー。
/// `sql::expr_program::ExprProgram::eval` の `WasmCall` ステップも共有する
/// （Issue #353。fail-closed 判定を 1 箇所に保つ）。
pub(crate) fn finite_scalar<'a>(v: f64, fn_name: &str) -> Result<ExprValue<'a>, SqlSurfaceError> {
    if !v.is_finite() {
        return Err(SqlSurfaceError::invalid_input(format!(
            "{fn_name}: result is not finite"
        )));
    }
    Ok(ExprValue::Scalar(v))
}

/// 2 項演算を値ベースで評価する（引数式の再帰評価は行わない）。再帰 `eval` の
/// `BoundExpr::Binary` 分岐と `sql::expr_program::ExprProgram::eval` の
/// `ExprStep::Binary` 分岐が共有する（Issue #353）。定数畳み込み
/// （`expr_program::try_fold_scalar`）もここを経由することで、畳み込み結果と
/// 実行時評価が同一の fail-closed 契約（0 除算は `22012`、非有限値は `22003`）を持つ。
pub(crate) fn eval_binary<'a>(
    op: BinOp,
    l: ExprValue<'a>,
    r: ExprValue<'a>,
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    // NULL 伝播（Issue #919・SQL-26（AC2）と Issue #921・SQL-26 の共有契約）:
    // 算術・比較のどちらかのオペランド（nullable TEXT 列由来・`CASE`／
    // `COALESCE`／`NULLIF` 由来のいずれも）が NULL なら、束縛段の型検査を経た
    // 演算であっても NULL を返す（3 値論理の UNKNOWN。`WHERE` 側の消費は
    // 呼び出し元が `ExprValue::Null` を偽と同義に扱う）。これは
    // `try_fold_scalar` の定数畳み込み経由でも共有する契約であるため、
    // `eval_binary` の入口で一元的に処理する。
    if matches!(l, ExprValue::Null) || matches!(r, ExprValue::Null) {
        return Ok(ExprValue::Null);
    }
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => match (l, r) {
            (ExprValue::Scalar(a), ExprValue::Scalar(b)) => {
                let v = apply_scalar_op(op, a, b)?;
                Ok(ExprValue::Scalar(v))
            }
            (ExprValue::Vector(v), ExprValue::Scalar(s))
                if op == BinOp::Mul || op == BinOp::Div =>
            {
                apply_vector_scalar_op(op, &v, s)
            }
            (ExprValue::Scalar(s), ExprValue::Vector(v)) if op == BinOp::Mul => {
                apply_vector_scalar_op(op, &v, s)
            }
            // 対象ビヘイビア: SQL-26（Issue #920）。`DATE ± n`／`n + DATE`／
            // `DATE - DATE`（束縛段〔`bind_binary`〕が受理する組み合わせのみ
            // ここに到達する）。
            (ExprValue::Date(d), ExprValue::Scalar(n)) if op == BinOp::Add => {
                datetime_fn::date_add_days(d, n).map(ExprValue::Date)
            }
            (ExprValue::Date(d), ExprValue::Scalar(n)) if op == BinOp::Sub => {
                datetime_fn::date_sub_days(d, n).map(ExprValue::Date)
            }
            (ExprValue::Scalar(n), ExprValue::Date(d)) if op == BinOp::Add => {
                datetime_fn::date_add_days(d, n).map(ExprValue::Date)
            }
            (ExprValue::Date(a), ExprValue::Date(b)) if op == BinOp::Sub => {
                Ok(ExprValue::Scalar(datetime_fn::date_diff_days(a, b)))
            }
            _ => Err(SqlSurfaceError::Internal {
                detail: "operand type mismatch at evaluation time".to_string(),
            }),
        },
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq => match (l, r) {
            (ExprValue::Scalar(a), ExprValue::Scalar(b)) => {
                let result = match op {
                    BinOp::Gt => a > b,
                    BinOp::Lt => a < b,
                    BinOp::Ge => a >= b,
                    BinOp::Le => a <= b,
                    BinOp::Eq => a == b,
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "non-comparison operator in comparison evaluation".to_string(),
                        });
                    }
                };
                Ok(ExprValue::Bool(result))
            }
            // Issue #919・SQL-26: TEXT 同士の比較はバイト順（UTF-8 コードポイント
            // 順）で行う。束縛段（`bind_binary`）が TEXT/TEXT の組しか許可しない
            // ため、他の型混在はここでも `Internal`（束縛段の不変条件が崩れた
            // 場合の保険）。
            (ExprValue::Text(a), ExprValue::Text(b)) => {
                let result = match op {
                    BinOp::Gt => a > b,
                    BinOp::Lt => a < b,
                    BinOp::Ge => a >= b,
                    BinOp::Le => a <= b,
                    BinOp::Eq => a == b,
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "non-comparison operator in comparison evaluation".to_string(),
                        });
                    }
                };
                Ok(ExprValue::Bool(result))
            }
            // 対象ビヘイビア: SQL-26（Issue #920）。`DATE`／`TIMESTAMP` 同士の
            // 比較（`bind_binary` が `DATE`⋈`TIMESTAMP` を `DateToTimestamp` で
            // 昇格済みのため、ここに到達するのは同種同士のみ）。
            (ExprValue::Date(a), ExprValue::Date(b)) => Ok(ExprValue::Bool(compare(op, a, b)?)),
            (ExprValue::Timestamp(a), ExprValue::Timestamp(b)) => {
                Ok(ExprValue::Bool(compare(op, a, b)?))
            }
            (ExprValue::Time(a), ExprValue::Time(b)) => Ok(ExprValue::Bool(compare(op, a, b)?)),
            // Issue #1597・TABLE-25: jsonb 同士の比較。両辺をバイト比較可能なソートキーへ
            // 変換して比較する（格納値が壊れていれば誤順序へ縮退せず fail-closed）。
            (ExprValue::Json(a), ExprValue::Json(b)) => {
                let ka = jsonb_sort_key_for_eval(&a)?;
                let kb = jsonb_sort_key_for_eval(&b)?;
                Ok(ExprValue::Bool(compare(op, ka, kb)?))
            }
            _ => Err(SqlSurfaceError::Internal {
                detail: "operand type mismatch at evaluation time".to_string(),
            }),
        },
    }
}

/// 評価時の jsonb ソートキー生成。失敗は値を含まない固定文言（長さ超過は `54000`、
/// 格納値の破損は `Internal`）。
pub(crate) fn jsonb_sort_key_for_eval(text: &str) -> Result<Vec<u8>, SqlSurfaceError> {
    crate::json::jsonb_sort_key(text).map_err(|e| match e {
        crate::json::JsonColumnError::TooLong => {
            SqlSurfaceError::payload_too_large("jsonb value exceeds the maximum length")
        }
        crate::json::JsonColumnError::Invalid => SqlSurfaceError::Internal {
            detail: "stored jsonb value could not be parsed".to_string(),
        },
    })
}

/// `Ord` を実装する値同士の比較演算子を適用する共通ヘルパー
/// （`DATE`／`TIMESTAMP` の内部表現〔`i32`／`i64`〕比較で使う。対象ビヘイビア:
/// SQL-26。Issue #920）。
fn compare<T: PartialOrd>(op: BinOp, a: T, b: T) -> Result<bool, SqlSurfaceError> {
    Ok(match op {
        BinOp::Gt => a > b,
        BinOp::Lt => a < b,
        BinOp::Ge => a >= b,
        BinOp::Le => a <= b,
        BinOp::Eq => a == b,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
            return Err(SqlSurfaceError::Internal {
                detail: "non-comparison operator in comparison evaluation".to_string(),
            });
        }
    })
}

fn apply_scalar_op(op: BinOp, a: f64, b: f64) -> Result<f64, SqlSurfaceError> {
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                return Err(SqlSurfaceError::division_by_zero("/"));
            }
            a / b
        }
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq => {
            return Err(SqlSurfaceError::Internal {
                detail: "comparison operator in scalar arithmetic evaluation".to_string(),
            });
        }
    };
    if !v.is_finite() {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "arithmetic result is not finite",
        ));
    }
    Ok(v)
}

fn apply_vector_scalar_op<'a>(
    op: BinOp,
    v: &[f32],
    s: f64,
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    if op == BinOp::Div && s == 0.0 {
        return Err(SqlSurfaceError::division_by_zero("/"));
    }
    let mut out: Vec<f32> = Vec::new();
    out.try_reserve_exact(v.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("vector result exceeds available memory")
    })?;
    for &x in v {
        let r = match op {
            BinOp::Mul => (x as f64) * s,
            BinOp::Div => (x as f64) / s,
            BinOp::Add | BinOp::Sub | BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq => {
                return Err(SqlSurfaceError::Internal {
                    detail: "non-mul/div operator in vector-scalar evaluation".to_string(),
                });
            }
        };
        if !r.is_finite() {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "arithmetic result is not finite",
            ));
        }
        // f64 では有限でも `f32` へキャストした結果が `f32::MAX` を超えて Infinity
        // 化しうる。キャスト後の値にも `is_finite()` を適用し fail-closed を保つ
        // （`vec_div` 側の同種修正と同方針）。
        let r32 = r as f32;
        if !r32.is_finite() {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "arithmetic result is not finite",
            ));
        }
        out.push(r32);
    }
    Ok(ExprValue::Vector(Cow::Owned(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #1438: 浮動小数列相手のリテラルは 2^53 exactness 判定をかけず f64 範囲で解釈する。
    #[test]
    fn float_operand_literal_accepts_large_values_and_normalizes() {
        for (raw, want) in [
            ("1e21", 1e21),
            ("1E+21", 1e21),
            ("-1e21", -1e21),
            ("9007199254740993", 9007199254740992.0),
            ("1.", 1.0),
            (".5e1", 5.0),
            ("0e999", 0.0),
            ("1e-310", 1e-310),
        ] {
            assert_eq!(parse_float_operand_literal(raw).expect(raw), want, "{raw}");
        }
        let zero = parse_float_operand_literal("-0").expect("-0");
        assert!(zero == 0.0 && zero.is_sign_positive());
    }

    #[test]
    fn float_operand_literal_rejects_out_of_range_without_echoing_literal() {
        for raw in ["1e309", "-1e309", "1e-400", "1e99999999999999999999"] {
            let err = parse_float_operand_literal(raw).unwrap_err();
            assert_eq!(err.wire_code(), "22003", "{raw}");
            assert!(!format!("{err:?}").contains(raw), "{raw}");
        }
        for raw in ["", "abc", "+-1", "1e", "inf"] {
            assert_eq!(
                parse_float_operand_literal(raw).unwrap_err().wire_code(),
                "22P02",
                "{raw}"
            );
        }
    }
    use crate::catalog::ColumnDef;

    fn schema_with_vector() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("label", ColumnType::Text, false),
            ],
        )
    }

    fn num(s: &str) -> Expr {
        Expr::Number(s.to_string())
    }

    fn ident(s: &str) -> Expr {
        Expr::Ident(s.to_string())
    }

    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            name: name.to_string(),
            args,
        }
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }

    #[test]
    fn builtin_vec_norm_matches_independent_l2_norm() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(
            &call("vec_norm", vec![ident("embedding")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);
        let embedding = [3.0f32, 4.0, 0.0];
        let value = eval(&bound, 1, &embedding).expect("eval should succeed");
        match value {
            ExprValue::Scalar(v) => assert!((v - 5.0).abs() < 1e-9),
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    #[test]
    fn udf_call_inlines_body_and_evaluates_like_the_expanded_expression() {
        // norm_scale(v, s) = s * vec_sum(vec_div(v, vec_norm(v)))
        let schema = schema_with_vector();
        let mut registry = UdfRegistry::default();
        let body = bin(
            BinOp::Mul,
            ident("s"),
            call(
                "vec_sum",
                vec![call(
                    "vec_div",
                    vec![ident("v"), call("vec_norm", vec![ident("v")])],
                )],
            ),
        );
        define_function(
            &mut registry,
            "norm_scale",
            &["v".to_string(), "s".to_string()],
            &body,
        )
        .expect("definition should succeed");

        let call_expr = call("norm_scale", vec![ident("embedding"), num("2.0")]);
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) =
            bind_expr(&call_expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);

        let embedding = [3.0f32, 4.0, 0.0];
        let value = eval(&bound, 1, &embedding).expect("eval should succeed");
        // 独立計算: norm=5, v/norm = [0.6,0.8,0], sum=1.4, *2.0 = 2.8
        // 許容誤差は 1e-6（`vec_div` の中間結果が `Vector`＝`f32` として保持されるため、
        // `0.6`/`0.8` の丸め誤差が `f64` 演算より大きい）。
        match value {
            ExprValue::Scalar(v) => assert!((v - 2.8).abs() < 1e-6, "got {v}"),
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    #[test]
    fn where_expression_type_checks_to_bool() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(
            BinOp::Gt,
            call("vec_norm", vec![ident("embedding")]),
            num("1.0"),
        );
        let (_, ty) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(ty, ExprType::Bool);
    }

    #[test]
    fn text_column_reference_is_accepted_since_issue_919() {
        // Issue #919・SQL-26: TEXT 列参照は解禁され `TextColumnRef` として束縛
        // される（ENUM／JSON／BYTEA 等の他の非対応型は引き続き拒否を維持する。
        // 下の `non_text_scalar_columns_remain_rejected` 参照）。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(&ident("label"), &schema, &registry, &mut budget)
            .expect("TEXT column reference should now bind");
        assert_eq!(ty, ExprType::Text);
        assert_eq!(bound, BoundExpr::TextColumnRef { index: 1 });
    }

    #[test]
    fn non_text_scalar_columns_remain_rejected() {
        // BOOLEAN 列は本 Issue のスコープ外のまま拒否を維持する（TEXT 解禁の
        // 副作用で他の非対応型まで誤って通してしまわないことの回帰確認）。
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("flag", ColumnType::Boolean, false),
            ],
        );
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&ident("flag"), &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn division_by_zero_is_fail_closed_not_silently_zeroed() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(BinOp::Div, num("1.0"), num("0.0"));
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let err = eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(err.wire_code(), "22012");
    }

    // Issue #1163・SQL-26: スカラー／ベクトル演算の 0 除算は 22012、あふれは 22003。
    #[test]
    fn scalar_and_vector_division_by_zero_are_22012_and_overflow_is_22003() {
        assert_eq!(
            apply_scalar_op(BinOp::Div, 1.0, 0.0)
                .unwrap_err()
                .wire_code(),
            "22012"
        );
        assert_eq!(
            apply_scalar_op(BinOp::Mul, f64::MAX, 2.0)
                .unwrap_err()
                .wire_code(),
            "22003"
        );
        assert_eq!(
            apply_scalar_op(BinOp::Add, f64::MAX, f64::MAX)
                .unwrap_err()
                .wire_code(),
            "22003"
        );
        assert_eq!(
            apply_vector_scalar_op(BinOp::Div, &[1.0], 0.0)
                .err()
                .unwrap()
                .wire_code(),
            "22012"
        );
        // f64 では有限だが f32 キャスト後に Infinity 化する。
        assert_eq!(
            apply_vector_scalar_op(BinOp::Mul, &[1.0], 1e300)
                .err()
                .unwrap()
                .wire_code(),
            "22003"
        );
    }

    #[test]
    fn vec_div_by_zero_is_22012() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = call("vec_div", vec![ident("embedding"), num("0")]);
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let err = eval(&bound, 1, &[1.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(err.wire_code(), "22012");
    }

    #[test]
    fn undefined_function_is_rejected_at_bind_time() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(
            &call("mystery", vec![num("1.0")]),
            &schema,
            &registry,
            &mut budget,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42883");
    }

    #[test]
    fn param_name_case_is_ignored_between_declaration_and_body_reference() {
        // `CREATE FUNCTION f(V) AS v`: 引用なし識別子はパラメータ宣言（`V`）と
        // 本体参照（`v`）の大文字小文字が食い違っても同一パラメータとして解決
        // されるべき（定義時検証・呼び出し時のインライン展開の双方で一貫させる）。
        let mut registry = UdfRegistry::default();
        define_function(&mut registry, "f", &["V".to_string()], &ident("v")).unwrap();

        let schema = schema_with_vector();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(&call("f", vec![num("7")]), &schema, &registry, &mut budget)
            .expect("call should bind: parameter case must resolve regardless of declared case");
        assert_eq!(ty, ExprType::Scalar);
        let value = eval(&bound, 1, &[0.0, 0.0, 0.0]).expect("eval should succeed");
        assert_eq!(value, ExprValue::Scalar(7.0));
    }

    #[test]
    fn redefining_a_function_is_rejected() {
        let mut registry = UdfRegistry::default();
        define_function(&mut registry, "f", &["x".to_string()], &ident("x")).unwrap();
        let err = define_function(&mut registry, "f", &["x".to_string()], &ident("x")).unwrap_err();
        assert_eq!(err.wire_code(), "42723");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn defining_a_function_with_builtin_name_is_rejected() {
        let mut registry = UdfRegistry::default();
        let err = define_function(&mut registry, "vec_norm", &["x".to_string()], &ident("x"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "42723");
    }

    #[test]
    fn function_body_cannot_reference_columns() {
        let mut registry = UdfRegistry::default();
        let err = define_function(&mut registry, "f", &[], &ident("embedding")).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn too_many_parameters_is_rejected_with_payload_too_large() {
        let mut registry = UdfRegistry::default();
        let params: Vec<String> = (0..(MAX_UDF_PARAMS + 1)).map(|i| format!("p{i}")).collect();
        let err = define_function(&mut registry, "f", &params, &num("1.0")).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn session_udf_count_limit_is_enforced() {
        let mut registry = UdfRegistry::default();
        for i in 0..MAX_SESSION_UDFS {
            define_function(&mut registry, &format!("f{i}"), &[], &num("1.0")).unwrap();
        }
        let err = define_function(&mut registry, "one_too_many", &[], &num("1.0")).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn builtin_call_with_wrong_arity_is_rejected_at_definition_time() {
        // `vec_norm` は 1 引数だが 0 引数で呼び出す本体を定義しようとする。
        let mut registry = UdfRegistry::default();
        let err = define_function(&mut registry, "f", &[], &call("vec_norm", vec![])).unwrap_err();
        assert_eq!(err.wire_code(), "42883");
        assert!(registry.is_empty());
    }

    #[test]
    fn registered_udf_call_with_wrong_arity_is_rejected_at_definition_time() {
        // `g(v)` は 1 引数だが、`h` の本体は `g(v, v)`（2 引数）で呼び出す。
        let mut registry = UdfRegistry::default();
        define_function(&mut registry, "g", &["v".to_string()], &ident("v")).unwrap();
        let err = define_function(
            &mut registry,
            "h",
            &["v".to_string()],
            &call("g", vec![ident("v"), ident("v")]),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42883");
        assert_eq!(registry.len(), 1, "h should not have been registered");
    }

    #[test]
    fn trailing_dot_integer_beyond_f64_exact_range_is_rejected_not_silently_rounded() {
        // Cursor Bugbot 指摘（PR #1020）: `sql::lexer::lex_number`（Issue #885・D5）
        // が生成する末尾ドット付き整数トークン（`1.` 形）は、桁だけを見る旧判定
        // （`bytes().all(is_ascii_digit)`）だと非整数扱いになり exactness ガードを
        // 素通りしていた。`9007199254740993.`（2^53 超）が `22003` で拒否される
        // ことを固定する。
        let err = parse_number_literal("9007199254740993.").unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn trailing_dot_integer_within_f64_exact_range_still_parses() {
        // `1.` のような、小数部が空の末尾ドット付き整数は正確表現域内であれば
        // 引き続き受理される（拒否対象は exactness を失う場合のみ）。
        let value = parse_number_literal("42.").expect("should parse");
        assert_eq!(value, 42.0);
    }

    /// codex-review P1（Issue #1183）: 整数値を表す小数・指数表記も丸め変換の前に
    /// exactness 判定を受ける（NoSQL `filter` が JSON の生数値を渡すため）。
    #[test]
    fn integral_decimal_and_exponent_forms_beyond_exact_range_are_rejected() {
        for raw in [
            "9007199254740993.0",
            "-9007199254740993.0",
            "9007199254740993.000",
            "9.007199254740993e15",
            "9007199254740993e0",
            "90071992547409930e-1",
            "1e30",
            "0.9007199254740993e16",
        ] {
            let err = parse_number_literal(raw).unwrap_err();
            assert_eq!(err.wire_code(), "22003", "{raw}");
        }
    }

    /// 指数が i64 の端に達する未信頼入力でも panic せず `22003` で拒否する
    /// （codex-review P1。checked 演算・fail-closed）。
    #[test]
    fn extreme_exponents_are_rejected_with_22003_without_panicking() {
        for raw in [
            "1.0e-9223372036854775808",
            "10e9223372036854775807",
            "1e9223372036854775807",
            "1e99999999999999999999999",
            "1e-99999999999999999999999",
        ] {
            let err = parse_number_literal(raw).unwrap_err();
            assert_eq!(err.wire_code(), "22003", "{raw}");
        }
    }

    #[test]
    fn integral_decimal_and_exponent_forms_within_exact_range_still_parse() {
        for (raw, want) in [
            ("9007199254740992.0", 9007199254740992.0),
            ("9.007199254740992e15", 9007199254740992.0),
            ("1e3", 1000.0),
            ("1.5", 1.5),
            ("2.5e-1", 0.25),
            ("0.0", 0.0),
            ("100.000", 100.0),
        ] {
            assert_eq!(parse_number_literal(raw).expect(raw), want, "{raw}");
        }
    }

    #[test]
    fn id_beyond_f64_exact_range_is_rejected_not_silently_rounded() {
        // `id_as_finite_scalar` は評価時に大きな `id` を拒否するが、リテラル側も
        // `f64` へ暗黙丸め変換されたままだと `WHERE id = 9007199254740993` が
        // 精度欠落により `id = 9007199254740992` の行にも一致してしまう。整数
        // リテラルの正確表現域チェックは束縛（`bind_expr`）時点で先に働くべきなので、
        // ここでは bind 自体が `22003` で拒否されることを確認する
        // （評価まで到達させない、より早い fail-closed）。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(BinOp::Eq, ident("id"), num("9007199254740993"));
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn row_id_beyond_f64_exact_range_is_rejected_at_eval_time_independently_of_literal_check() {
        // 上のテストはリテラル側の正確表現域チェック（bind 時）を確認する。本テストは
        // `id_as_finite_scalar`（eval 時、行 `id` 側）が独立した多重防御として機能する
        // ことを確認する: リテラルは小さく bind を通過させ、行 `id` の方を
        // `2^53` 超に設定して eval が `22003` で拒否することを見る。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(BinOp::Eq, ident("id"), num("42"));
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let err = eval(&bound, 9_007_199_254_740_993, &[0.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn id_within_f64_exact_range_still_evaluates() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(BinOp::Eq, ident("id"), num("42"));
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let value = eval(&bound, 42, &[0.0, 0.0, 0.0]).expect("eval should succeed");
        assert_eq!(value, ExprValue::Bool(true));
    }

    #[test]
    fn vec_div_result_that_overflows_f32_after_cast_is_rejected() {
        // f64 中間値は有限だが `f32::MAX` を超えるため `r as f32` は Infinity になる。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = call("vec_div", vec![ident("embedding"), num("1e-300")]);
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let embedding = [1.0f32, 0.0, 0.0];
        let err = eval(&bound, 1, &embedding).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn real_id_column_takes_precedence_over_pseudo_column() {
        // codex-review PR #209 指摘対応: 実カラム `id`（TEXT 型）を宣言したスキーマで
        // `id` を参照すると、`parser.rs` の投影束縛と同じ優先順位で実カラムが
        // 解決される（黙って行キー疑似列 `BoundExpr::IdRef` へフォールバックしては
        // ならない）。Issue #919・SQL-26 以降 TEXT 列参照は解禁されたため、
        // 期待する解決結果は「エラー」から「`TextColumnRef`（`ExprType::Text`）」へ
        // 変わった（優先順位そのものの回帰確認としての意味は維持する）。
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ],
        );
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(&ident("id"), &schema, &registry, &mut budget)
            .expect("real TEXT column named id should now bind as TextColumnRef");
        assert_eq!(ty, ExprType::Text);
        assert_eq!(bound, BoundExpr::TextColumnRef { index: 0 });
    }

    #[test]
    fn pseudo_id_column_still_resolves_when_no_real_id_column_exists() {
        // 実カラム `id` が存在しないスキーマでは、従来どおり行キー疑似列
        // `BoundExpr::IdRef` へ解決される（既存挙動の非回帰確認）。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) =
            bind_expr(&ident("id"), &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);
        assert_eq!(bound, BoundExpr::IdRef);
    }

    #[test]
    fn non_first_vector_column_reference_is_rejected_fail_closed() {
        // codex-review PR #209 指摘対応: `catalog::validate_schema`（TABLE-1）は
        // 複数 VECTOR 列を持つスキーマの永続化を拒否するが、`bind_expr_in` 側でも
        // 独立に検査し、その不変条件が何らかの理由で崩れていた場合に
        // 2 本目以降の VECTOR 列参照が検索対象 embedding スロットの値で
        // 誤って評価されるのを防ぐ（fail-closed。security.md「不安全な設計」）。
        // `TableSchema::new` は `validate_schema` を経由しないため、ここでは
        // カタログ層では作れないはずの 2 VECTOR 列スキーマを直接構築して検査する。
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("other", ColumnType::Vector(3), false),
            ],
        );
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        // 最初の VECTOR 列（embedding）は従来どおり解決できる。
        let (_, ty) = bind_expr(&ident("embedding"), &schema, &registry, &mut budget)
            .expect("bind should succeed");
        assert_eq!(ty, ExprType::Vector);
        // 2 本目の VECTOR 列（other）は fail-closed に拒否される。
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&ident("other"), &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- references_embedding（Issue #350） -----------------------------------

    #[test]
    fn references_embedding_true_for_direct_vector_ref() {
        assert!(references_embedding(&BoundExpr::VectorRef));
    }

    #[test]
    fn references_embedding_false_for_id_and_number_only() {
        assert!(!references_embedding(&BoundExpr::IdRef));
        assert!(!references_embedding(&BoundExpr::Number(1.0)));
        assert!(!references_embedding(&BoundExpr::Binary {
            op: BinOp::Add,
            lhs: Box::new(BoundExpr::IdRef),
            rhs: Box::new(BoundExpr::Number(1.0)),
        }));
    }

    #[test]
    fn references_embedding_true_through_builtin_argument() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(
            &call("vec_norm", vec![ident("embedding")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("vec_norm(embedding) should bind");
        assert_eq!(ty, ExprType::Scalar);
        assert!(references_embedding(&bound));
    }

    #[test]
    fn references_embedding_true_through_binary_argument() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, _ty) = bind_expr(
            &bin(
                BinOp::Add,
                call("vec_norm", vec![ident("embedding")]),
                num("1"),
            ),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("vec_norm(embedding) + 1 should bind");
        assert!(references_embedding(&bound));
    }

    #[test]
    fn vector_ref_evaluation_borrows_the_row_embedding_without_allocating() {
        // Issue #352: `VectorRef`（テーブル VECTOR 列の素通し参照）の評価結果は
        // 行データを複製せず借用する（毎行 Vec 確保の排除）。`Cow::Borrowed` で
        // あることを固定し、確保が発生していないことを型レベルで検証する。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(&ident("embedding"), &schema, &registry, &mut budget)
            .expect("bind should succeed");
        assert_eq!(ty, ExprType::Vector);
        let embedding = [3.0f32, 4.0, 0.0];
        let value = eval(&bound, 1, &embedding).expect("eval should succeed");
        match value {
            ExprValue::Vector(Cow::Borrowed(v)) => assert_eq!(v, &embedding),
            other => panic!("expected borrowed vector, got {other:?}"),
        }
    }

    #[test]
    fn vec_norm_and_vec_sum_argument_evaluation_borrows_without_allocating() {
        // Issue #352: 読み取りのみの組み込み関数（`vec_norm`/`vec_sum`）の引数評価
        // （`eval_vector_arg` 経由）も、`VectorRef` を直接渡す限り借用のまま完結し
        // ヒープ確保が発生しないことを固定する。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let embedding = [3.0f32, 4.0, 0.0];

        let mut budget = MAX_EXPR_NODES;
        let (bound, _) = bind_expr(
            &call("vec_norm", vec![ident("embedding")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind should succeed");
        // `vec_norm` 自体は Scalar を返すため、引数評価の借用性は
        // `eval_vector_arg` を直接呼び出して検証する。
        if let BoundExpr::Builtin { args, .. } = &bound {
            let arg_value = eval(&args[0], 1, &embedding).expect("arg eval should succeed");
            match arg_value {
                ExprValue::Vector(Cow::Borrowed(v)) => assert_eq!(v, &embedding),
                other => panic!("expected borrowed vector, got {other:?}"),
            }
        } else {
            panic!("expected Builtin bound expr");
        }
    }

    #[test]
    fn vec_div_result_is_owned_not_borrowed() {
        // Issue #352: `vec_div` は新しいベクトルを構築するため、結果は
        // `Cow::Owned`（新規確保）であるべきで、借用元へのエイリアシングであっては
        // ならない。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = call("vec_div", vec![ident("embedding"), num("2.0")]);
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let embedding = [3.0f32, 4.0, 0.0];
        let value = eval(&bound, 1, &embedding).expect("eval should succeed");
        match value {
            ExprValue::Vector(Cow::Owned(v)) => assert_eq!(v, vec![1.5f32, 2.0, 0.0]),
            other => panic!("expected owned vector, got {other:?}"),
        }
    }

    #[test]
    fn into_owned_vector_copies_borrowed_without_aliasing_source() {
        let source = [1.0f32, 2.0, 3.0];
        let owned = into_owned_vector(Cow::Borrowed(&source[..])).expect("copy should succeed");
        assert_eq!(owned, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn into_owned_vector_moves_owned_without_reallocating() {
        let source = vec![1.0f32, 2.0, 3.0];
        let ptr_before = source.as_ptr();
        let owned = into_owned_vector(Cow::Owned(source)).expect("move should succeed");
        // move のみで再確保されていないことをポインタの同一性で確認する。
        assert_eq!(owned.as_ptr(), ptr_before);
        assert_eq!(owned, vec![1.0, 2.0, 3.0]);
    }

    /// PR #373 codex-review 指摘 2 対応の回帰テスト: `sql::expr_program::ExprStep::Builtin`
    /// の実行時（行ループ）は arity 分の引数を [`MAX_BUILTIN_ARITY`] 長の固定配列
    /// （ヒープ確保なし）へ積んでから [`apply_builtin`] を呼ぶ。すべての
    /// [`BuiltinFn`] の arity がこの固定長に収まっていることを網羅的に検証し、
    /// 将来 arity の大きい組み込み関数が追加された際に固定長バッファの上限
    /// （実行時は `sql::expr_program::ExprProgram::eval` が `Internal` へ
    /// fail-closed に拒否する）と乖離しないようにする。
    #[test]
    fn builtin_arities_fit_max_arity() {
        for f in [
            BuiltinFn::VecNorm,
            BuiltinFn::VecSum,
            BuiltinFn::VecDiv,
            BuiltinFn::Lower,
            BuiltinFn::Upper,
            BuiltinFn::Length,
            BuiltinFn::Substr2,
            BuiltinFn::Substr3,
            BuiltinFn::Concat2,
            BuiltinFn::Trim,
            BuiltinFn::Replace,
            BuiltinFn::Position,
            BuiltinFn::Abs,
            BuiltinFn::Round1,
            BuiltinFn::Round2,
            BuiltinFn::Floor,
            BuiltinFn::Ceil,
            BuiltinFn::Mod,
            BuiltinFn::Power,
            BuiltinFn::Sqrt,
            BuiltinFn::TypedArith(BinOp::Add, NumericKind::Int4),
        ] {
            let (params, _) = builtin_signature(f);
            assert!(
                params.len() <= MAX_BUILTIN_ARITY,
                "{f:?} arity {} exceeds MAX_BUILTIN_ARITY {MAX_BUILTIN_ARITY}",
                params.len()
            );
        }
    }

    /// codex-review P2 指摘の回帰テスト（`bind_concat`）: 多引数 `CONCAT` の
    /// 左畳み込みが新規生成する `BoundExpr::Builtin { f: Concat2, .. }`
    /// ラッパーノードが `node_budget` へ課金されることを固定する。
    /// `concat(label, label, label)` の課金内訳: 最上位の `Expr::Call` ノード
    /// 自身で 1（`bind_expr_in` が全 variant 共通で入口に課す 1 ノード分）、
    /// 各 `label` 参照で 1 ずつ計 3（同じく `bind_expr_in` の `Expr::Ident`
    /// 分岐）、畳み込みで新規生成する `Concat2` ラッパーノードが 2 個（合計
    /// 6 ノード）。修正前はラッパーノード分（2）が未計上のため実際のコストは
    /// 4 だったが、修正後は 6 ノード必要になる。予算 5 では（修正前の実コスト
    /// 4 なら成功するが）ラッパーノード込みで不足し `payload_too_large`
    /// （`54000`）で拒否されることを確認する。
    #[test]
    fn bind_concat_charges_fold_wrapper_nodes_against_node_budget() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();

        // 予算 6（Call ノード 1 + ident 参照 3 個 + Concat2 ラッパー 2 個）は
        // ちょうど足りる。
        let mut budget = 6usize;
        let (_, ty) = bind_expr(
            &call(
                "concat",
                vec![ident("label"), ident("label"), ident("label")],
            ),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("budget of exactly 6 nodes should be sufficient");
        assert_eq!(ty, ExprType::Text);
        assert_eq!(budget, 0, "all 6 charged nodes should exhaust the budget");

        // 予算 5 はラッパーノード分を課金すると不足するため拒否される
        // （修正前は実コストが 4〔ラッパーノード未計上〕だったため誤って
        // 成功していた）。
        let mut budget = 5usize;
        let err = bind_expr(
            &call(
                "concat",
                vec![ident("label"), ident("label"), ident("label")],
            ),
            &schema,
            &registry,
            &mut budget,
        )
        .expect_err("budget of 4 nodes must be insufficient once wrapper nodes are charged");
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review P1 指摘の回帰テスト（`bind_concat`）: `CONCAT` は NULL 引数を
    /// 空文字として扱い常に非 NULL を返す契約（AC2）だが、修正前は裸の
    /// `Expr::Null` を `bind_expr_in` に渡していたため `0A000`
    /// （`FeatureNotSupported`）で束縛段から拒否され、`CONCAT(NULL, 'x')` を
    /// 実行できなかった。`bind_null_aware`（`CASE`/`COALESCE` と共有）を使う
    /// ことで NULL リテラルを受理し、実行時は空文字として結合されることを
    /// 固定する。
    #[test]
    fn concat_with_null_literal_argument_binds_and_evaluates_as_empty_string() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = 100usize;
        let (bound, ty) = bind_expr(
            &call("concat", vec![Expr::Null, Expr::String("x".to_string())]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("concat(NULL, 'x') must bind successfully (NULL is empty string in CONCAT)");
        assert_eq!(ty, ExprType::Text);

        let value = eval_with_scalars(&bound, 1, &[], &[]).expect("evaluation must succeed");
        match value {
            ExprValue::Text(s) => assert_eq!(s.as_ref(), "x"),
            other => panic!("expected Text(\"x\"), got {other:?}"),
        }
    }

    /// [`concat_with_null_literal_argument_binds_and_evaluates_as_empty_string`]
    /// の追加ケース: 唯一の引数が NULL の場合（1 引数 CONCAT は自身と空文字の
    /// 結合として扱う既存契約と組み合わさる）も空文字を返す。
    #[test]
    fn concat_with_only_null_literal_argument_evaluates_as_empty_string() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = 100usize;
        let (bound, ty) = bind_expr(
            &call("concat", vec![Expr::Null]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("concat(NULL) must bind successfully");
        assert_eq!(ty, ExprType::Text);

        let value = eval_with_scalars(&bound, 1, &[], &[]).expect("evaluation must succeed");
        match value {
            ExprValue::Text(s) => assert_eq!(s.as_ref(), ""),
            other => panic!("expected Text(\"\"), got {other:?}"),
        }
    }

    /// [`apply_builtin`] がスライス（`&mut [Option<ExprValue>]`）で引数を受け取り、
    /// 呼び出し元が `Vec` を所有・生成する必要がないことを確認する
    /// （PR #373 codex-review 指摘 2 対応）。固定長配列をスタックに確保し、
    /// スライスとして渡すだけで組み込み関数が評価できることを型レベルで示す
    /// 実行例。
    #[test]
    fn apply_builtin_accepts_fixed_size_array_slice_without_owning_vec() {
        let mut args: [Option<ExprValue<'_>>; MAX_BUILTIN_ARITY] = [
            Some(ExprValue::Vector(Cow::Borrowed(&[3.0f32, 4.0][..]))),
            None,
            None,
        ];
        let result = apply_builtin(BuiltinFn::VecNorm, &mut args[..1]).expect("should evaluate");
        match result {
            ExprValue::Scalar(v) => assert!((v - 5.0).abs() < 1e-9),
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    // --- 数値スカラー関数群（Issue #920・SQL-26） -------------------------------

    #[test]
    fn round_resolves_to_round1_for_a_single_argument() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(
            &call("round", vec![num("2.5")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);
        assert!(matches!(
            bound,
            BoundExpr::Builtin {
                f: BuiltinFn::Round1,
                ..
            }
        ));
        let value = eval(&bound, 1, &[0.0, 0.0, 0.0]).expect("eval should succeed");
        assert_eq!(value, ExprValue::Scalar(3.0));
    }

    #[test]
    fn round_resolves_to_round2_for_two_arguments() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, _) = bind_expr(
            &call("round", vec![num("3.14159"), num("2")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind should succeed");
        assert!(matches!(
            bound,
            BoundExpr::Builtin {
                f: BuiltinFn::Round2,
                ..
            }
        ));
    }

    #[test]
    fn round_with_wrong_arity_is_rejected_at_bind_time() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&call("round", vec![]), &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "42883");
    }

    #[test]
    fn numeric_builtin_functions_evaluate_end_to_end() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let cases: &[(&str, Vec<Expr>, f64)] = &[
            ("abs", vec![bin(BinOp::Sub, num("0"), num("3"))], 3.0),
            ("floor", vec![num("2.9")], 2.0),
            ("ceil", vec![num("2.1")], 3.0),
            ("ceiling", vec![num("2.1")], 3.0),
            ("sqrt", vec![num("9")], 3.0),
        ];
        for (name, args, expected) in cases {
            let mut budget = MAX_EXPR_NODES;
            let (bound, ty) = bind_expr(&call(name, args.clone()), &schema, &registry, &mut budget)
                .unwrap_or_else(|e| panic!("bind {name} should succeed: {e:?}"));
            assert_eq!(ty, ExprType::Scalar);
            let value = eval(&bound, 1, &[0.0, 0.0, 0.0])
                .unwrap_or_else(|e| panic!("eval {name} should succeed: {e:?}"));
            assert_eq!(value, ExprValue::Scalar(*expected), "function {name}");
        }

        let mut budget = MAX_EXPR_NODES;
        let (bound, _) = bind_expr(
            &call("mod", vec![num("5"), num("3")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind mod should succeed");
        assert_eq!(
            eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(2.0)
        );

        let mut budget = MAX_EXPR_NODES;
        let (bound, _) = bind_expr(
            &call("power", vec![num("2"), num("10")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind power should succeed");
        assert_eq!(
            eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(1024.0)
        );
    }

    #[test]
    fn power_overflow_is_rejected_with_numeric_out_of_range_at_eval_time() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let (bound, _) = bind_expr(
            &call("power", vec![num("10"), num("400")]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind should succeed");
        let err = eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn sqrt_of_negative_is_rejected_with_22000_at_eval_time() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = call("sqrt", vec![bin(BinOp::Sub, num("0"), num("1"))]);
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let err = eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn defining_a_udf_named_round_is_rejected_as_reserved() {
        let mut registry = UdfRegistry::default();
        let err =
            define_function(&mut registry, "round", &["x".to_string()], &ident("x")).unwrap_err();
        assert_eq!(err.wire_code(), "42723");
    }

    #[test]
    fn defining_and_calling_a_udf_named_after_an_unimplemented_non_deterministic_function_succeeds()
    {
        // PR #1107 codex-review P1 是正の維持: 未実装の非決定的関数名（`random`）や、
        // 括弧付き呼び出しが `Expr::Call` のまま残る `current_timestamp` という名前の UDF は
        // 従来どおり登録・呼び出せる。`now` 等の現在時刻関数名は Issue #1586（SQL-37）で
        // 予約名になった（次のテスト）。
        let schema = schema_with_vector();
        for name in ["random", "current_timestamp"] {
            let mut registry = UdfRegistry::default();
            define_function(&mut registry, name, &[], &num("1.0"))
                .unwrap_or_else(|e| panic!("definition of {name} should succeed, got {e:?}"));

            let mut budget = MAX_EXPR_NODES;
            let (bound, ty) = bind_expr(&call(name, vec![]), &schema, &registry, &mut budget)
                .unwrap_or_else(|e| panic!("call to {name} should bind, got {e:?}"));
            assert_eq!(ty, ExprType::Scalar, "function name {name}");
            let value = eval(&bound, 1, &[0.0, 0.0, 0.0])
                .unwrap_or_else(|e| panic!("call to {name} should evaluate, got {e:?}"));
            assert_eq!(value, ExprValue::Scalar(1.0), "function name {name}");
        }
    }

    #[test]
    fn current_time_function_names_are_reserved_for_udfs() {
        // SQL-37・SQL-26（Issue #1586）: 同名 UDF の定義は `42723`。
        for name in [
            "now",
            "transaction_timestamp",
            "statement_timestamp",
            "clock_timestamp",
        ] {
            let mut registry = UdfRegistry::default();
            let err = define_function(&mut registry, name, &[], &num("1.0")).unwrap_err();
            assert_eq!(err.wire_code(), "42723", "function name {name}");
        }
    }

    #[test]
    fn calling_an_unknown_non_deterministic_function_is_rejected_with_42883() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&call("random", vec![]), &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "42883");
    }

    #[test]
    fn current_time_functions_without_a_clock_are_rejected_with_0a000() {
        // 時計を確定していない文脈（INSERT・UPDATE・CHECK・DEFAULT 等）は fail-closed。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        for name in [
            "now",
            "transaction_timestamp",
            "statement_timestamp",
            "clock_timestamp",
        ] {
            let mut budget = MAX_EXPR_NODES;
            let err = bind_expr(&call(name, vec![]), &schema, &registry, &mut budget).unwrap_err();
            assert_eq!(err.wire_code(), "0A000", "function name {name}");
        }
        let mut budget = MAX_EXPR_NODES;
        let kw = Expr::ValueFunction {
            kind: ValueFunctionKind::CurrentTimestamp,
            precision: None,
        };
        let err = bind_expr(&kw, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn current_time_functions_bind_to_the_scope_clock() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let clock = StatementClock {
            txn_start_micros: 1_700_000_123_456_789,
            stmt_start_micros: 1_700_000_999_000_000,
        };
        let _scope = time_fn::ClockScope::enter(clock);
        let bind = |e: Expr| {
            let mut budget = MAX_EXPR_NODES;
            bind_expr(&e, &schema, &registry, &mut budget)
        };
        for name in ["now", "transaction_timestamp"] {
            let (b, ty) = bind(call(name, vec![])).expect("bind");
            assert_eq!(b, BoundExpr::Timestamp(clock.txn_start_micros));
            assert_eq!(ty, ExprType::Timestamp);
        }
        let (b, _) = bind(call("statement_timestamp", vec![])).expect("bind");
        assert_eq!(b, BoundExpr::Timestamp(clock.stmt_start_micros));
        let (b, ty) = bind(Expr::ValueFunction {
            kind: ValueFunctionKind::CurrentTimestamp,
            precision: Some(0),
        })
        .expect("bind");
        assert_eq!(b, BoundExpr::Timestamp(1_700_000_123_000_000));
        assert_eq!(ty, ExprType::Timestamp);
        let (b, ty) = bind(Expr::ValueFunction {
            kind: ValueFunctionKind::CurrentDate,
            precision: None,
        })
        .expect("bind");
        assert_eq!(
            b,
            BoundExpr::Date(
                i32::try_from(clock.txn_start_micros / crate::datetime::MICROS_PER_DAY).unwrap()
            )
        );
        assert_eq!(ty, ExprType::Date);
        // 範囲外の精度は 22023、TIME 系は 0A000、引数付きは 42883。
        let err = bind(Expr::ValueFunction {
            kind: ValueFunctionKind::LocalTimestamp,
            precision: Some(7),
        })
        .unwrap_err();
        assert_eq!(err.wire_code(), "22023");
        let err = bind(Expr::ValueFunction {
            kind: ValueFunctionKind::CurrentTime,
            precision: None,
        })
        .unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
        let err = bind(call("now", vec![num("1")])).unwrap_err();
        assert_eq!(err.wire_code(), "42883");
    }

    #[test]
    fn clock_timestamp_is_not_folded_and_is_evaluated_per_call() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let clock = StatementClock {
            txn_start_micros: 1,
            stmt_start_micros: 1,
        };
        let _scope = time_fn::ClockScope::enter(clock);
        let mut budget = MAX_EXPR_NODES;
        let (bound, ty) = bind_expr(
            &call("clock_timestamp", vec![]),
            &schema,
            &registry,
            &mut budget,
        )
        .expect("bind");
        assert_eq!(ty, ExprType::Timestamp);
        assert!(matches!(
            bound,
            BoundExpr::Builtin {
                f: BuiltinFn::ClockTimestamp,
                ..
            }
        ));
        assert!(!is_foldable_builtin(BuiltinFn::ClockTimestamp));
        let a = eval(&bound, 1, &[0.0, 0.0, 0.0]).expect("eval");
        let b = eval(&bound, 1, &[0.0, 0.0, 0.0]).expect("eval");
        let (ExprValue::Timestamp(a), ExprValue::Timestamp(b)) = (a, b) else {
            panic!("expected timestamps");
        };
        assert!(a > clock.txn_start_micros && b >= a);
    }

    // --- CASE／COALESCE／NULLIF（対象ビヘイビア: SQL-26。Issue #921） ----------

    fn case_expr(whens: Vec<(Expr, Expr)>, else_result: Option<Expr>) -> Expr {
        Expr::Case {
            whens,
            else_result: else_result.map(Box::new),
        }
    }

    #[test]
    fn case_selects_matching_branch_and_evaluates_like_expanded_expression() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(
            vec![(bin(BinOp::Gt, ident("id"), num("1")), num("10"))],
            Some(num("0")),
        );
        let (bound, ty) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);
        assert_eq!(
            eval(&bound, 2, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(10.0)
        );
        assert_eq!(
            eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(0.0)
        );
    }

    #[test]
    fn case_without_else_evaluates_to_null_when_no_branch_matches() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(
            vec![(bin(BinOp::Gt, ident("id"), num("100")), num("1"))],
            None,
        );
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(), ExprValue::Null);
    }

    #[test]
    fn case_does_not_evaluate_unselected_branch_division_by_zero() {
        // 選ばれない分岐の 0 除算はエラーにならない（defer-on-error。
        // `sql::expr_program` のステップ列コンパイルと同じ契約）。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(
            vec![(bin(BinOp::Eq, num("1"), num("1")), num("1"))],
            Some(bin(BinOp::Div, num("1"), num("0"))),
        );
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(
            eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(1.0)
        );
    }

    #[test]
    fn case_when_condition_must_be_bool_else_datatype_mismatch() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(vec![(num("1"), num("1"))], Some(num("0")));
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "42804");
    }

    #[test]
    fn case_branch_type_mismatch_is_datatype_mismatch() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(
            vec![(bin(BinOp::Eq, num("1"), num("1")), ident("embedding"))],
            Some(num("0")),
        );
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "42804");
    }

    #[test]
    fn case_with_only_null_results_is_feature_not_supported() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(vec![(bin(BinOp::Eq, num("1"), num("1")), Expr::Null)], None);
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn bare_null_outside_case_coalesce_nullif_is_feature_not_supported() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&Expr::Null, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn coalesce_returns_first_non_null_argument() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::Coalesce(vec![Expr::Null, Expr::Null, num("7"), num("8")]);
        let (bound, ty) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);
        assert_eq!(
            eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(7.0)
        );
    }

    #[test]
    fn coalesce_all_null_arguments_is_feature_not_supported() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::Coalesce(vec![Expr::Null, Expr::Null]);
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn coalesce_argument_type_mismatch_is_datatype_mismatch() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::Coalesce(vec![num("1"), ident("embedding")]);
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "42804");
    }

    #[test]
    fn nullif_returns_null_when_equal_and_lhs_when_different() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::NullIf(Box::new(ident("id")), Box::new(num("2")));
        let (bound, ty) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(ty, ExprType::Scalar);
        assert_eq!(eval(&bound, 2, &[0.0, 0.0, 0.0]).unwrap(), ExprValue::Null);
        assert_eq!(
            eval(&bound, 3, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(3.0)
        );
    }

    #[test]
    fn nullif_rhs_null_returns_lhs() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::NullIf(Box::new(num("5")), Box::new(Expr::Null));
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert_eq!(
            eval(&bound, 1, &[0.0, 0.0, 0.0]).unwrap(),
            ExprValue::Scalar(5.0)
        );
    }

    #[test]
    fn nullif_both_null_is_feature_not_supported() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::NullIf(Box::new(Expr::Null), Box::new(Expr::Null));
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn nullif_vector_argument_is_datatype_mismatch() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = Expr::NullIf(Box::new(ident("embedding")), Box::new(num("1")));
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "42804");
    }

    #[test]
    fn defining_a_function_named_coalesce_or_nullif_is_rejected() {
        let mut registry = UdfRegistry::default();
        let err = define_function(&mut registry, "coalesce", &["x".to_string()], &ident("x"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "42723");
        let err =
            define_function(&mut registry, "NULLIF", &["x".to_string()], &ident("x")).unwrap_err();
        assert_eq!(err.wire_code(), "42723");
    }

    #[test]
    fn defining_a_function_named_case_is_rejected() {
        // Bugbot 指摘の回帰テスト（Issue #921）: `CASE` は `parse_primary_expr`
        // が `'('` の有無を見ず常に文脈的キーワードとして消費するため、同名の
        // UDF を許すと `case(...)` という呼び出しが構文解析段で CASE 式に
        // 吸われ、定義した UDF を呼び出す手段が無くなってしまう。`COALESCE`／
        // `NULLIF` と同じ理由で定義時に拒否する。
        let mut registry = UdfRegistry::default();
        let err =
            define_function(&mut registry, "case", &["x".to_string()], &ident("x")).unwrap_err();
        assert_eq!(err.wire_code(), "42723");
    }

    #[test]
    fn case_nesting_beyond_limit_is_rejected_at_bind_time() {
        // 束縛段のネスト上限（`MAX_CASE_NESTING`）を、構文段を経由せず直接
        // ネストした `Expr::Coalesce` を組み立てて検査する。
        let mut expr = num("1");
        for _ in 0..=MAX_CASE_NESTING {
            expr = Expr::Coalesce(vec![expr]);
        }
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn case_nesting_beyond_limit_is_rejected_after_udf_argument_inlining() {
        // codex-review PR #1101 指摘（対象ビヘイビア: SQL-26。Issue #921）:
        // 実引数が単体では上限 `MAX_CASE_NESTING` ちょうど（合法）でも、その
        // 実引数を本体側でさらに `COALESCE` に包む UDF に渡すと、展開後の
        // 実効ネストが上限を超える。呼び出し元の実引数束縛（合法）・UDF 本体の
        // 定義時検証（`ident("x")` のみで合法）のどちらの計測時点でも単独では
        // 超過が見えないため、`Expr::Ident` によるパラメータ展開時に検査しないと
        // すり抜ける（`max_bound_case_nesting` 参照）。
        let mut registry = UdfRegistry::default();
        define_function(
            &mut registry,
            "wrap_once",
            &["x".to_string()],
            &Expr::Coalesce(vec![ident("x")]),
        )
        .expect("defining a 1-level-nesting UDF body should succeed");

        let mut nested_arg = num("1");
        for _ in 0..MAX_CASE_NESTING {
            nested_arg = Expr::Coalesce(vec![nested_arg]);
        }
        // 実引数単体（`nested_arg`）はちょうど `MAX_CASE_NESTING` 段で合法。
        let schema = schema_with_vector();
        let mut budget = MAX_EXPR_NODES;
        bind_expr(&nested_arg, &schema, &UdfRegistry::default(), &mut budget)
            .expect("the argument alone must still be within the limit");

        let call_expr = call("wrap_once", vec![nested_arg]);
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&call_expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn references_embedding_true_through_case_branch() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = case_expr(
            vec![(bin(BinOp::Eq, num("1"), num("1")), ident("embedding"))],
            Some(ident("embedding")),
        );
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        assert!(references_embedding(&bound));
    }

    #[test]
    fn vector_ref_evaluates_to_null_when_embedding_is_empty() {
        // codex-review P1 指摘の回帰テスト（Issue #921）: `embedding` が空スライス
        // （`VECTOR` 列が NULL。呼び出し元は非 NULL なら実データ・NULL なら空
        // スライスで揃えて渡す契約）の行では `VectorRef` 自体が NULL として
        // 評価される。`vec_norm(embedding)` のような builtin もこの NULL を
        // strict 関数契約（`apply_builtin`）でそのまま伝播する。
        assert_eq!(
            eval(&BoundExpr::VectorRef, 1, &[]).unwrap(),
            ExprValue::Null
        );
        let vec_norm_expr = BoundExpr::Builtin {
            f: BuiltinFn::VecNorm,
            args: vec![BoundExpr::VectorRef],
        };
        assert_eq!(eval(&vec_norm_expr, 1, &[]).unwrap(), ExprValue::Null);
    }

    #[test]
    fn case_selected_branch_without_embedding_reference_ignores_empty_embedding() {
        // codex-review P1 指摘の回帰テスト（Issue #921）: `references_embedding`
        // は式木全体を静的に走査するため、`CASE` の選ばれない分岐にだけ
        // embedding 参照があるだけの式を「embedding を参照する式」と誤判定し、
        // 旧実装はこの誤判定に基づき `dim == 0`（`embedding` が空スライス）の
        // 行を無条件に NULL 扱いしていた。実際に選択される分岐（`THEN` 側）が
        // embedding を一切参照しない場合は、`embedding` が空スライスでも
        // 選ばれた分岐の値がそのまま返る必要がある（`Case` の短絡評価契約。
        // 本モジュールの `eval` の `Case` 分岐参照）。
        let expr = bound_case_expr(
            vec![(
                BoundExpr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(BoundExpr::IdRef),
                    rhs: Box::new(BoundExpr::Number(2.0)),
                },
                BoundExpr::Number(1.0),
            )],
            BoundExpr::Builtin {
                f: BuiltinFn::VecNorm,
                args: vec![BoundExpr::VectorRef],
            },
        );
        assert_eq!(eval(&expr, 2, &[]).unwrap(), ExprValue::Scalar(1.0));
    }

    /// [`references_embedding_true_through_case_branch`] 等の `case_expr`
    /// ヘルパーは `Expr`（構文段）向けのため、束縛後の `BoundExpr::Case` を
    /// 直接組み立てる本テスト専用の小さなヘルパー。
    fn bound_case_expr(whens: Vec<(BoundExpr, BoundExpr)>, else_result: BoundExpr) -> BoundExpr {
        BoundExpr::Case {
            whens,
            else_result: Box::new(else_result),
        }
    }

    fn schema_with_numeric_columns() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("qty", ColumnType::Integer, true),
                ColumnDef::new("total", ColumnType::BigInt, true),
                ColumnDef::new("ratio", ColumnType::Real, true),
                ColumnDef::new("score", ColumnType::Double, true),
            ],
        )
    }

    /// 数値 4 型の列参照は `BoundExpr::ColumnRef`（`ExprType::Scalar`）へ束縛される
    /// （Issue #1183。`WHERE`／投影／`CHECK` で共通）。
    #[test]
    fn bind_expr_binds_numeric_columns_as_column_ref() {
        let schema = schema_with_numeric_columns();
        let registry = UdfRegistry::default();
        for (name, index) in [("qty", 0), ("total", 1), ("ratio", 2), ("score", 3)] {
            let mut budget = MAX_EXPR_NODES;
            let (bound, ty) = bind_expr(&ident(name), &schema, &registry, &mut budget)
                .unwrap_or_else(|e| panic!("{name} must bind: {e:?}"));
            assert_eq!(ty, ExprType::Scalar, "{name}");
            assert_eq!(bound, BoundExpr::ColumnRef { index }, "{name}");
        }
    }

    /// 数値列以外（BOOLEAN 等）の式内参照は引き続き拒否される。
    #[test]
    fn bind_expr_still_rejects_non_numeric_columns() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("flag", ColumnType::Boolean, false)],
        );
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&ident("flag"), &schema, &registry, &mut budget)
            .expect_err("BOOLEAN column must still be rejected");
        assert!(err
            .to_string()
            .contains("BOOLEAN columns are not supported"));
    }

    /// [`eval_with_scalars`]（`BoundExpr::ColumnRef`）の値変換規則（設計 D-2）:
    /// INTEGER/REAL は常に正確に表現でき、BIGINT は `2^53` 以下でのみ正確に
    /// 表現できる。NULL は `ExprValue::Null` として伝播する（三値論理）。
    #[test]
    fn eval_with_scalars_numeric_column_ref_conversion_rules() {
        let expr = BoundExpr::ColumnRef { index: 0 };

        let scalars = [Some(ScalarRef::Integer(-7))];
        assert_eq!(
            eval_with_scalars(&expr, 1, &[], &scalars).unwrap(),
            ExprValue::Scalar(-7.0)
        );

        let scalars = [Some(ScalarRef::Real(1.5))];
        assert_eq!(
            eval_with_scalars(&expr, 1, &[], &scalars).unwrap(),
            ExprValue::Scalar(1.5)
        );

        let scalars = [Some(ScalarRef::Double(2.25))];
        assert_eq!(
            eval_with_scalars(&expr, 1, &[], &scalars).unwrap(),
            ExprValue::Scalar(2.25)
        );

        let scalars = [Some(ScalarRef::BigInt(1i64 << 52))];
        assert_eq!(
            eval_with_scalars(&expr, 1, &[], &scalars).unwrap(),
            ExprValue::Scalar((1i64 << 52) as f64)
        );

        let too_big = (1i64 << 53) + 1;
        let scalars = [Some(ScalarRef::BigInt(too_big))];
        assert!(eval_with_scalars(&expr, 1, &[], &scalars).is_err());

        let scalars: [Option<ScalarRef<'_>>; 1] = [None];
        assert_eq!(
            eval_with_scalars(&expr, 1, &[], &scalars).unwrap(),
            ExprValue::Null
        );

        // マスク外参照（呼び出し元の配線不備）は実 NULL と取り違えず
        // `Internal` として fail-closed に拒否する。
        let empty: [Option<ScalarRef<'_>>; 0] = [];
        assert!(eval_with_scalars(&expr, 1, &[], &empty).is_err());
    }

    #[test]
    fn first_non_builtin_call_accepts_builtin_only_exprs() {
        assert_eq!(
            first_non_builtin_call(&call("lower", vec![ident("lang")])),
            None
        );
        let arith = Expr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(call("length", vec![ident("x")])),
                rhs: Box::new(Expr::Number("1".into())),
            }),
            rhs: Box::new(Expr::Number("2".into())),
        };
        assert_eq!(first_non_builtin_call(&arith), None);
        let case = Expr::Case {
            whens: vec![(
                ident("a"),
                call("round", vec![ident("x"), Expr::Number("2".into())]),
            )],
            else_result: Some(Box::new(Expr::Null)),
        };
        assert_eq!(first_non_builtin_call(&case), None);
        let co = Expr::Coalesce(vec![
            ident("a"),
            Expr::NullIf(Box::new(ident("b")), Box::new(Expr::Null)),
        ]);
        assert_eq!(first_non_builtin_call(&co), None);
    }

    #[test]
    fn first_non_builtin_call_finds_nested_non_builtin() {
        assert_eq!(
            first_non_builtin_call(&call("myfn", vec![ident("x")])),
            Some("myfn")
        );
        let nested = call("lower", vec![call("myfn", vec![ident("x")])]);
        assert_eq!(first_non_builtin_call(&nested), Some("myfn"));
        let case = Expr::Case {
            whens: vec![(call("f", vec![ident("x")]), Expr::Null)],
            else_result: None,
        };
        assert_eq!(first_non_builtin_call(&case), Some("f"));
        let else_case = Expr::Case {
            whens: vec![],
            else_result: Some(Box::new(call("g", vec![]))),
        };
        assert_eq!(first_non_builtin_call(&else_case), Some("g"));
    }

    /// Issue #1539・SQL-42: 型付き算術の checked 演算・切り捨て除算・丸めを固定する。
    #[test]
    fn typed_arith_follows_postgres_integer_and_real_semantics() {
        use NumericKind::{Float4, Int4, Int8};
        let ok = |op, k, a, b| typed_arith(op, k, a, b).expect("should succeed");
        let code = |op, k, a, b| {
            typed_arith(op, k, a, b)
                .unwrap_err()
                .wire_code()
                .to_string()
        };
        assert_eq!(ok(BinOp::Div, Int4, 7.0, 2.0), 3.0);
        assert_eq!(ok(BinOp::Div, Int4, -7.0, 2.0), -3.0);
        assert_eq!(ok(BinOp::Sub, Int4, 0.0, 2147483647.0), -2147483647.0);
        assert_eq!(code(BinOp::Add, Int4, 2147483647.0, 1.0), "22003");
        assert_eq!(code(BinOp::Sub, Int4, -2147483648.0, 1.0), "22003");
        assert_eq!(code(BinOp::Div, Int4, -2147483648.0, -1.0), "22003");
        assert_eq!(code(BinOp::Div, Int4, 1.0, 0.0), "22012");
        // int8 は i32 を超えても良いが 2^53 を超える結果は fail-closed。
        assert_eq!(ok(BinOp::Mul, Int8, 2147483648.0, 2.0), 4294967296.0);
        assert_eq!(code(BinOp::Mul, Int8, 4503599627370497.0, 2.0), "22003");
        // 整数でないオペランドは不変条件違反（panic せず Internal = XX000）。
        assert_eq!(code(BinOp::Add, Int4, 1.5, 1.0), "XX000");
        // float4: f32 へ丸め、溢れ・アンダーフローは 22003。
        assert_eq!(ok(BinOp::Add, Float4, 1.5, 2.25), 3.75);
        assert_eq!(code(BinOp::Mul, Float4, 3.0e38, 10.0), "22003");
        assert_eq!(code(BinOp::Mul, Float4, 1.0e-30, 1.0e-30), "22003");
        assert_eq!(code(BinOp::Div, Float4, 1.0, 0.0), "22012");
    }

    /// Issue #1560: smallint 同士の算術は smallint（i16 の溢れは 22003）、混在は広い方。
    #[test]
    fn typed_arith_int2_and_combine_kinds() {
        use NumericKind::{Float4, Int2, Int4, Int8};
        assert_eq!(
            typed_arith(BinOp::Add, Int2, 32766.0, 1.0).unwrap(),
            32767.0
        );
        for (op, a, b) in [
            (BinOp::Add, 32767.0, 1.0),
            (BinOp::Sub, -32768.0, 1.0),
            (BinOp::Mul, 200.0, 200.0),
            (BinOp::Div, -32768.0, -1.0),
        ] {
            let e = typed_arith(op, Int2, a, b).unwrap_err();
            assert_eq!(e.wire_code().to_string(), "22003");
        }
        assert_eq!(combine_numeric_kind(Some(Int2), Some(Int2)), Some(Int2));
        assert_eq!(combine_numeric_kind(Some(Int2), Some(Int4)), Some(Int4));
        assert_eq!(combine_numeric_kind(Some(Int4), Some(Int2)), Some(Int4));
        assert_eq!(combine_numeric_kind(Some(Int2), Some(Int8)), Some(Int8));
        assert_eq!(combine_numeric_kind(Some(Int8), Some(Int4)), Some(Int8));
        assert_eq!(combine_numeric_kind(Some(Int2), Some(Float4)), None);
        assert_eq!(combine_numeric_kind(Some(Int2), None), None);
    }
}
