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
    // 同じ exactness 判定を適用する（SQL 表層は単項マイナスが無く影響しない）。
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
            SqlSurfaceError::invalid_input(
                "integer literal exceeds the range that can be exactly represented",
            )
        })?;
        if as_int > MAX_EXACT_F64_INT {
            return Err(SqlSurfaceError::invalid_input(
                "integer literal exceeds the range that can be exactly represented",
            ));
        }
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
    /// PostgreSQL の unknown 型リテラルの暗黙型変換は行わず、常に TEXT 型として
    /// 束縛する（対象外事項。docs/design/implementation-status.md 参照）。
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
    /// nullable な数値列（INTEGER/BIGINT/REAL/DOUBLE）の参照（Issue #1075・
    /// TABLE-16 ポインタ）。`index` は `TextColumnRef` と同じ論理列インデックス
    /// 系列（`schema.columns` 添字）。`WHERE`／投影／`CHECK` の
    /// 式束縛（Issue #1183・SQL-24・SQL-26）で共通に生成される。値の変換規則は `numeric_scalar_from_ref` 参照。
    ColumnRef {
        index: usize,
    },
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
            (BoundExpr::DateColumnRef { index: a }, BoundExpr::DateColumnRef { index: b }) => {
                a == b
            }
            (
                BoundExpr::TimestampColumnRef { index: a },
                BoundExpr::TimestampColumnRef { index: b },
            ) => a == b,
            (BoundExpr::ColumnRef { index: a }, BoundExpr::ColumnRef { index: b }) => a == b,
            (BoundExpr::IdRef, BoundExpr::IdRef) => true,
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
    /// `date_trunc(unit: Text, src: Timestamp) -> Timestamp`（`field` と同じく
    /// `unit` は束縛時に解決済み）。
    DateTrunc(DateTruncUnit),
    /// `DATE` を深夜 0 時の `TIMESTAMP` へ昇格する内部専用関数（名前では
    /// 解決できない。`bind_call`／`bind_binary` が `date_part`／`date_trunc` の
    /// `DATE` 引数、および `DATE`／`TIMESTAMP` 比較の `DATE` 側に挿入する）。
    DateToTimestamp,
}

/// `name` が組み込み関数（[`BuiltinFn`]）の名前かどうかを判定する。`pub(crate)`:
/// `sql::check_constraint`（TABLE-16・TASK-204、Issue #906）が `CHECK` 述語中の
/// `Expr::Call` を束縛より**前**に検査し、組み込み関数以外（セッション UDF・
/// WASM UDF・未知関数）の呼び出しを `42601` として拒否するために使う
/// （空の [`UdfRegistry`] で束縛すると「未知の関数」として `22000` へ丸まって
/// しまい、CHECK の禁止要素として区別できないため）。`round` は arity で
/// `Round1`／`Round2` へオーバーロード解決されるため [`builtin_from_name`] の
/// 対象外だが、名前としては組み込み扱いにする必要があるためここで別途判定する。
pub(crate) fn is_builtin_function_name(name: &str) -> bool {
    builtin_from_name(name).is_some() || is_variadic_or_overloaded_builtin_name(name)
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
        BuiltinFn::DateTrunc(_) => (&[ExprType::Timestamp], ExprType::Timestamp),
        BuiltinFn::DateToTimestamp => (&[ExprType::Date], ExprType::Timestamp),
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
        BuiltinFn::DatePart(_) | BuiltinFn::DateTrunc(_) | BuiltinFn::DateToTimestamp => false,
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
        return Err(SqlSurfaceError::invalid_input(format!(
            "function name {name} collides with a built-in or reserved name"
        )));
    }
    if registry.is_name_taken(&lower) {
        return Err(SqlSurfaceError::invalid_input(format!(
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
        return Err(SqlSurfaceError::invalid_input(format!(
            "function name {name} collides with a built-in or reserved name"
        )));
    }
    if registry.is_name_taken(&lower) {
        return Err(SqlSurfaceError::invalid_input(format!(
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
/// 未登録の呼び出し名・引数個数の不整合はここで拒否する（`22000`）。自己参照・
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
            if name.eq_ignore_ascii_case("concat") {
                if args.is_empty() {
                    return Err(SqlSurfaceError::invalid_input(
                        "function concat expects at least 1 argument, got 0",
                    ));
                }
            } else if name.eq_ignore_ascii_case("substr") {
                if !matches!(args.len(), 2 | 3) {
                    return Err(SqlSurfaceError::invalid_input(format!(
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
                    return Err(SqlSurfaceError::invalid_input(format!(
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
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "function {name} expects 1 or 2 argument(s), got {}",
                        args.len()
                    )));
                }
            } else if let Some(builtin) = builtin_from_name(name) {
                let (param_types, _ret) = builtin_signature(builtin);
                if args.len() != param_types.len() {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "function {name} expects {} argument(s), got {}",
                        param_types.len(),
                        args.len()
                    )));
                }
            } else if let Some(def) = registry.get(name) {
                if args.len() != def.params.len() {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "function {name} expects {} argument(s), got {}",
                        def.params.len(),
                        args.len()
                    )));
                }
            } else if registry.get_wasm(name).is_some() {
                // WASM UDF（TASK-149）は ABI 固定シグネチャ
                // `(Vector, Scalar) -> Scalar` のみのため、常に 2 引数固定。
                if args.len() != WASM_CALL_ARITY {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "function {name} expects {WASM_CALL_ARITY} argument(s), got {}",
                        args.len()
                    )));
                }
            } else {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "unknown function: {name}"
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
        Expr::DateLiteral(_) | Expr::TimestampLiteral(_) => Ok(()),
    }
}

/// 束縛環境: 列参照の解決元（`SELECT`/`WHERE` の式では `Some(schema)`、UDF 本体の
/// 展開中は `None` に切り替わりパラメータ参照だけを解決する）と、パラメータ名 →
/// 既に束縛済みの実引数式（インライン展開用）の対応表。
struct BindEnv<'a> {
    schema: Option<&'a TableSchema>,
    params: std::collections::HashMap<String, (BoundExpr, ExprType)>,
    registry: &'a UdfRegistry,
    /// `CASE`／`COALESCE`／`NULLIF` の現在の入れ子段数（対象ビヘイビア: SQL-26。
    /// Issue #921）。構文段（`sql::allowlist::Parser`）のネストカウンタとは別に
    /// 束縛段でも独立に検査する: UDF 本体をインライン展開すると、呼び出し元の
    /// ネストと呼び出し先本体のネストが合算され、構文段の計測値（各文が個別に
    /// 見た字面上のネスト）をすり抜けうるため（[`enter_case_nesting`] 参照）。
    case_nesting: usize,
}

/// 展開済み [`BoundExpr`] のノード数を数える。UDF 連鎖のパラメータ参照展開時に
/// クローンする部分木のサイズを `node_budget` へ課金するために使う
/// （`bind_expr_in` の `Expr::Ident` 分岐を参照）。木の深さは束縛段で既に
/// `node_budget` により上限が掛かっているため、単純な再帰で数え上げてよい。
fn count_bound_nodes(expr: &BoundExpr) -> usize {
    match expr {
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::TextColumnRef { .. }
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::DateColumnRef { .. }
        | BoundExpr::TimestampColumnRef { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::IdRef
        | BoundExpr::VectorRef
        | BoundExpr::Null => 1,
        BoundExpr::Builtin { args, .. } => 1 + args.iter().map(count_bound_nodes).sum::<usize>(),
        BoundExpr::Binary { lhs, rhs, .. } => 1 + count_bound_nodes(lhs) + count_bound_nodes(rhs),
        BoundExpr::WasmCall { args, .. } => 1 + args.iter().map(count_bound_nodes).sum::<usize>(),
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
        | BoundExpr::DateColumnRef { .. }
        | BoundExpr::TimestampColumnRef { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::IdRef
        | BoundExpr::VectorRef
        | BoundExpr::Null => 0,
        BoundExpr::Builtin { args, .. } => {
            args.iter().map(max_bound_case_nesting).max().unwrap_or(0)
        }
        BoundExpr::Binary { lhs, rhs, .. } => {
            max_bound_case_nesting(lhs).max(max_bound_case_nesting(rhs))
        }
        BoundExpr::WasmCall { args, .. } => {
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
        | BoundExpr::DateColumnRef { .. }
        | BoundExpr::TimestampColumnRef { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::IdRef
        | BoundExpr::Null => false,
        BoundExpr::Builtin { args, .. } => args.iter().any(references_embedding),
        BoundExpr::Binary { lhs, rhs, .. } => {
            references_embedding(lhs) || references_embedding(rhs)
        }
        BoundExpr::WasmCall { args, .. } => args.iter().any(references_embedding),
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
        | BoundExpr::ColumnRef { index } => visit(*index),
        BoundExpr::Number(_)
        | BoundExpr::Text(_)
        | BoundExpr::Date(_)
        | BoundExpr::Timestamp(_)
        | BoundExpr::IdRef
        | BoundExpr::VectorRef
        | BoundExpr::Null => {}
        BoundExpr::Builtin { args, .. } | BoundExpr::WasmCall { args, .. } => {
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
        | BoundExpr::IdRef
        | BoundExpr::VectorRef
        | BoundExpr::Null => false,
        // `any`/`fold` は短絡評価となり、先頭の一致以降の引数をマークし損ねる
        // （複数引数が別々の TEXT 列を参照しうる。例: `concat(a, b)`）ため、
        // 明示ループで全引数を必ず走査する（`references_embedding` の `any`
        // 使用とは異なり、ここでは「1 つでも該当するか」だけでなく「該当する
        // 添字をすべて `mask` へ反映する」副作用が主目的）。
        BoundExpr::Builtin { args, .. } | BoundExpr::WasmCall { args, .. } => {
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
    };
    bind_expr_in(expr, &mut env, node_budget)
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
fn bind_null_aware(
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
                ExprType::Scalar | ExprType::Text | ExprType::Date | ExprType::Timestamp
            ) {
                return Err(SqlSurfaceError::DatatypeMismatch {
                    detail: "NULLIF arguments must be scalar, text, date, or timestamp".to_string(),
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
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    match expr {
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
                    ColumnType::Text => Ok((BoundExpr::TextColumnRef { index }, ExprType::Text)),
                    // Issue #1183・SQL-24・SQL-26・TABLE-13 ポインタ: 数値 4 型
                    // （INTEGER／BIGINT／REAL／DOUBLE）の列参照を `WHERE`／投影／
                    // `CHECK` で共通に解禁する（#1075 の CHECK 専用 opt-in を撤廃）。
                    // 値は行スカラービュー（`row_scalars`）から解決し、評価時の
                    // 精度超過（BIGINT の |v| > 2^53）は `22000` で fail-closed。
                    ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Real
                    | ColumnType::Double => Ok((BoundExpr::ColumnRef { index }, ExprType::Scalar)),
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
                    ColumnType::Array(_) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (ARRAY columns are not supported)"
                    ))),
                    ColumnType::Bytea => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (BYTEA columns are not supported)"
                    ))),
                    ColumnType::Json | ColumnType::Jsonb => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} cannot be used in an expression (JSON columns are not supported)"
                        )))
                    }
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
            Err(SqlSurfaceError::invalid_input(format!(
                "unknown column: {name}"
            )))
        }
        Expr::Call { name, args } => bind_call(name, args, env, node_budget),
        Expr::Binary { op, lhs, rhs } => {
            let (l, lt) = bind_expr_in(lhs, env, node_budget)?;
            let (r, rt) = bind_expr_in(rhs, env, node_budget)?;
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
    }
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
            _ => Err(SqlSurfaceError::invalid_input(
                "'+'/'-' require both operands to be scalar, or a DATE combined with a day-count scalar",
            )),
        },
        BinOp::Mul => match (lt, rt) {
            (ExprType::Scalar, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Scalar)),
            (ExprType::Vector, ExprType::Scalar) | (ExprType::Scalar, ExprType::Vector) => {
                Ok((mk(op, l, r), ExprType::Vector))
            }
            _ => Err(SqlSurfaceError::invalid_input(
                "'*' requires scalar operands, or one vector and one scalar operand",
            )),
        },
        BinOp::Div => match (lt, rt) {
            (ExprType::Scalar, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Scalar)),
            (ExprType::Vector, ExprType::Scalar) => Ok((mk(op, l, r), ExprType::Vector)),
            _ => Err(SqlSurfaceError::invalid_input(
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
            // `DATE` ⋈ `TIMESTAMP` は `DATE` 側を深夜 0 時の `TIMESTAMP` へ
            // 暗黙昇格してから比較する（PostgreSQL 互換。§2-3）。
            (ExprType::Date, ExprType::Timestamp) => {
                Ok((mk(op, wrap_date_to_timestamp(l), r), ExprType::Bool))
            }
            (ExprType::Timestamp, ExprType::Date) => {
                Ok((mk(op, l, wrap_date_to_timestamp(r)), ExprType::Bool))
            }
            _ => Err(SqlSurfaceError::invalid_input(
                "comparison operators require both operands to be scalar, both text, or both date/timestamp",
            )),
        },
    }
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
    if lower_name == "concat" {
        return bind_concat(args, env, node_budget);
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
                return Err(SqlSurfaceError::invalid_input(format!(
                    "function round expects 1 or 2 argument(s), got {n}"
                )));
            }
        };
        let (param_types, ret) = builtin_signature(builtin);
        let mut bound_args = Vec::with_capacity(args.len());
        for (a, expected) in args.iter().zip(param_types.iter()) {
            let (b, ty) = bind_expr_in(a, env, node_budget)?;
            if ty != *expected {
                return Err(SqlSurfaceError::invalid_input(
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

    if let Some(builtin) = builtin_from_name(name) {
        let (param_types, ret) = builtin_signature(builtin);
        if args.len() != param_types.len() {
            return Err(SqlSurfaceError::invalid_input(format!(
                "function {name} expects {} argument(s), got {}",
                param_types.len(),
                args.len()
            )));
        }
        let mut bound_args = Vec::with_capacity(args.len());
        for (a, expected) in args.iter().zip(param_types.iter()) {
            let (b, ty) = bind_expr_in(a, env, node_budget)?;
            if ty != *expected {
                return Err(SqlSurfaceError::invalid_input(format!(
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
            return Err(SqlSurfaceError::invalid_input(format!(
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
        };
        return bind_expr_in(&def.body, &mut inner_env, node_budget);
    }

    if let Some(backend) = env.registry.get_wasm(name).cloned() {
        // WASM UDF（TASK-149）: ABI 固定シグネチャ `(Vector, Scalar) -> Scalar`
        // のため引数個数・型は常にこの形で検査する（組み込み `vec_div` と同じ
        // 引数検査の流儀）。
        if args.len() != WASM_CALL_ARITY {
            return Err(SqlSurfaceError::invalid_input(format!(
                "function {name} expects {WASM_CALL_ARITY} argument(s), got {}",
                args.len()
            )));
        }
        let mut bound_args = Vec::with_capacity(args.len());
        for (a, expected) in args.iter().zip([ExprType::Vector, ExprType::Scalar].iter()) {
            let (b, ty) = bind_expr_in(a, env, node_budget)?;
            if ty != *expected {
                return Err(SqlSurfaceError::invalid_input(format!(
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

    Err(SqlSurfaceError::invalid_input(format!(
        "unknown function: {name}"
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
        return Err(SqlSurfaceError::invalid_input(
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
                return Err(SqlSurfaceError::invalid_input(
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
        return Err(SqlSurfaceError::invalid_input(format!(
            "function {lower_name} expects 2 argument(s), got {}",
            args.len()
        )));
    }
    // 添字アクセスを避け get() で取得する（coding-rust.md: 受信 SQL 経路での [] 禁止）。
    // 直前の args.len() != 2 検査により両方 Some になるが、兄弟実装（bind_substr・
    // bind_concat）の様式に合わせ、ここでも fail-closed な Err 経路を明示する。
    let field_arg = args.first().ok_or_else(|| {
        SqlSurfaceError::invalid_input(format!(
            "function {lower_name} expects 2 argument(s), got 0"
        ))
    })?;
    let src_arg = args.get(1).ok_or_else(|| {
        SqlSurfaceError::invalid_input(format!(
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
            return Err(SqlSurfaceError::invalid_input(format!(
                "function {lower_name} expects a text literal as its first argument"
            )));
        }
    };
    let (src_bound, src_ty) = bind_null_aware(src_arg, env, node_budget)?;
    let src = match src_ty {
        Some(ExprType::Timestamp) => src_bound,
        Some(ExprType::Date) => wrap_date_to_timestamp(src_bound),
        None => src_bound,
        _ => {
            return Err(SqlSurfaceError::invalid_input(format!(
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
            return Err(SqlSurfaceError::invalid_input(format!(
                "function {name} expects 2 or 3 arguments, got {got}"
            )))
        }
    };
    let (param_types, ret) = builtin_signature(builtin);
    let mut bound_args = Vec::with_capacity(args.len());
    for (a, expected) in args.iter().zip(param_types.iter()) {
        let (b, ty) = bind_expr_in(a, env, node_budget)?;
        if ty != *expected {
            return Err(SqlSurfaceError::invalid_input(format!(
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
/// `Err`（`22000`）として伝播する。
/// 行 `id`（`u64`）を `ExprValue::Scalar`（`f64`）へ変換する前に、`f64` の 52 bit
/// 仮数部で正確に表現できる範囲（`2^53` 以下）かを確認する。これを超える `id` を
/// 無条件に `as f64` で丸めると、`WHERE id = <literal>` のような等価述語が精度欠落
/// により別 ID の行にも一致しうる（fail-closed: 黙って丸めず `22000` で拒否する）。
pub(crate) fn id_as_finite_scalar(id: u64) -> Result<f64, SqlSurfaceError> {
    if id > MAX_EXACT_F64_INT {
        return Err(SqlSurfaceError::invalid_input(
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
                Err(SqlSurfaceError::invalid_input(
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
        BoundExpr::ColumnRef { index } => match row_scalars.get(*index) {
            None => Err(SqlSurfaceError::Internal {
                detail: "NUMERIC column reference is outside the decoded row scalar view"
                    .to_string(),
            }),
            Some(None) => Ok(ExprValue::Null),
            Some(Some(v)) => numeric_scalar_from_ref(v).map(ExprValue::Scalar),
        },
        BoundExpr::IdRef => id_as_finite_scalar(id).map(ExprValue::Scalar),
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
                return Err(SqlSurfaceError::invalid_input("vec_div: division by zero"));
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
                    return Err(SqlSurfaceError::invalid_input(
                        "vec_div: result is not finite",
                    ));
                }
                // f64 では有限でも `f32` へキャストした結果が `f32::MAX` を超えて
                // Infinity 化しうる。「非有限値は 22000 で fail-closed」の契約を
                // キャスト後の値にも適用し、結果へ Infinity を流出させない。
                let r32 = r as f32;
                if !r32.is_finite() {
                    return Err(SqlSurfaceError::invalid_input(
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
        BuiltinFn::DateTrunc(unit) => {
            let t = take_timestamp_arg(args, 0)?;
            datetime_fn::date_trunc(unit, t).map(ExprValue::Timestamp)
        }
        BuiltinFn::DateToTimestamp => {
            let d = take_date_arg(args, 0)?;
            Ok(ExprValue::Timestamp(datetime_fn::date_to_timestamp(d)))
        }
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
/// 実行時評価が同一の fail-closed 契約（0 除算・非有限値の `22000`）を持つ。
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
            _ => Err(SqlSurfaceError::Internal {
                detail: "operand type mismatch at evaluation time".to_string(),
            }),
        },
    }
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
                return Err(SqlSurfaceError::invalid_input("division by zero"));
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
        return Err(SqlSurfaceError::invalid_input(
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
        return Err(SqlSurfaceError::invalid_input("division by zero"));
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
            return Err(SqlSurfaceError::invalid_input(
                "arithmetic result is not finite",
            ));
        }
        // f64 では有限でも `f32` へキャストした結果が `f32::MAX` を超えて Infinity
        // 化しうる。キャスト後の値にも `is_finite()` を適用し fail-closed を保つ
        // （`vec_div` 側の同種修正と同方針）。
        let r32 = r as f32;
        if !r32.is_finite() {
            return Err(SqlSurfaceError::invalid_input(
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
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn defining_a_function_with_builtin_name_is_rejected() {
        let mut registry = UdfRegistry::default();
        let err = define_function(&mut registry, "vec_norm", &["x".to_string()], &ident("x"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
        assert_eq!(registry.len(), 1, "h should not have been registered");
    }

    #[test]
    fn trailing_dot_integer_beyond_f64_exact_range_is_rejected_not_silently_rounded() {
        // Cursor Bugbot 指摘（PR #1020）: `sql::lexer::lex_number`（Issue #885・D5）
        // が生成する末尾ドット付き整数トークン（`1.` 形）は、桁だけを見る旧判定
        // （`bytes().all(is_ascii_digit)`）だと非整数扱いになり exactness ガードを
        // 素通りしていた。`9007199254740993.`（2^53 超）が `22000` で拒否される
        // ことを固定する。
        let err = parse_number_literal("9007199254740993.").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn trailing_dot_integer_within_f64_exact_range_still_parses() {
        // `1.` のような、小数部が空の末尾ドット付き整数は正確表現域内であれば
        // 引き続き受理される（拒否対象は exactness を失う場合のみ）。
        let value = parse_number_literal("42.").expect("should parse");
        assert_eq!(value, 42.0);
    }

    #[test]
    fn id_beyond_f64_exact_range_is_rejected_not_silently_rounded() {
        // `id_as_finite_scalar` は評価時に大きな `id` を拒否するが、リテラル側も
        // `f64` へ暗黙丸め変換されたままだと `WHERE id = 9007199254740993` が
        // 精度欠落により `id = 9007199254740992` の行にも一致してしまう。整数
        // リテラルの正確表現域チェックは束縛（`bind_expr`）時点で先に働くべきなので、
        // ここでは bind 自体が `22000` で拒否されることを確認する
        // （評価まで到達させない、より早い fail-closed）。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(BinOp::Eq, ident("id"), num("9007199254740993"));
        let err = bind_expr(&expr, &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn row_id_beyond_f64_exact_range_is_rejected_at_eval_time_independently_of_literal_check() {
        // 上のテストはリテラル側の正確表現域チェック（bind 時）を確認する。本テストは
        // `id_as_finite_scalar`（eval 時、行 `id` 側）が独立した多重防御として機能する
        // ことを確認する: リテラルは小さく bind を通過させ、行 `id` の方を
        // `2^53` 超に設定して eval が `22000` で拒否することを見る。
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let expr = bin(BinOp::Eq, ident("id"), num("42"));
        let (bound, _) =
            bind_expr(&expr, &schema, &registry, &mut budget).expect("bind should succeed");
        let err = eval(&bound, 9_007_199_254_740_993, &[0.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn defining_and_calling_a_udf_named_after_a_non_deterministic_function_succeeds() {
        // PR #1107 codex-review P1 是正: `now`/`random` 等の名前を UDF 予約名に
        // 含めると、これらの名前で登録・呼び出しできていた既存の宣言的 UDF が
        // 登録時に拒否される破壊的変更になっていた（本 PR は数値関数のみを
        // 対象とし、非決定的関数の実装は含まないため予約化の理由がない）。
        // 数値関数の予約名（`round` 等）はそのまま維持しつつ、`now`／`random`／
        // `current_timestamp` という名前の UDF が従来どおり登録・呼び出し
        // できることを固定する。非決定的関数名の予約化は、日時スカラー関数群
        // （`docs/design/numeric-scalar-functions.md`「スコープ外・後続課題」
        // 参照）を実装する後続作業で spec の決定性要件と対にして判断する。
        let schema = schema_with_vector();
        for name in ["now", "random", "current_timestamp"] {
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
    fn calling_a_non_deterministic_function_is_rejected_with_22000() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&call("now", vec![]), &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
        let err =
            define_function(&mut registry, "NULLIF", &["x".to_string()], &ident("x")).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
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
        assert_eq!(err.wire_code(), "22000");
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
}
