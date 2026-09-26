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
use crate::sql::allowlist::SqlSurfaceError;
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
    let integer_digits: Option<&str> = if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit())
    {
        Some(raw)
    } else if let Some(stripped) = raw.strip_suffix('.') {
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
}

impl PartialEq for BoundExpr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (BoundExpr::Number(a), BoundExpr::Number(b)) => a == b,
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
    /// SQL `NULL`（対象ビヘイビア: SQL-26。Issue #921）。`CASE`／`COALESCE`／
    /// `NULLIF` の評価結果としてのみ生じる。呼び出し元（`sql::exec`・`sql::scan`・
    /// `sql::group_by`・`sql::aggregate`・`sql::check_constraint`）は UNKNOWN
    /// （PostgreSQL の 3 値論理）として扱う: `WHERE`/`CHECK` では非該当・充足、
    /// 投影では `Cell::Null`、`SUM`/`AVG`/`MIN`/`MAX`/`COUNT(expr)` では
    /// 当該行をスキップする。
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

/// 組み込み関数（対象ビヘイビア SQL-9。`sqrt`/`abs` 等の追加は本タスクのスコープ外
/// （out-of-scope-tracking 参照）で、UDF 本体を書くのに十分な最小集合に絞る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinFn {
    /// `vec_norm(v: Vector) -> Scalar`（L2 ノルム）。
    VecNorm,
    /// `vec_sum(v: Vector) -> Scalar`（成分和）。
    VecSum,
    /// `vec_div(v: Vector, s: Scalar) -> Vector`（成分ごとの除算）。
    VecDiv,
}

/// `name` が組み込み関数（[`BuiltinFn`]）の名前かどうかを判定する。`pub(crate)`:
/// `sql::check_constraint`（TABLE-16・TASK-204、Issue #906）が `CHECK` 述語中の
/// `Expr::Call` を束縛より**前**に検査し、組み込み関数以外（セッション UDF・
/// WASM UDF・未知関数）の呼び出しを `42601` として拒否するために使う
/// （空の [`UdfRegistry`] で束縛すると「未知の関数」として `22000` へ丸まって
/// しまい、CHECK の禁止要素として区別できないため）。
pub(crate) fn is_builtin_function_name(name: &str) -> bool {
    builtin_from_name(name).is_some()
}

fn builtin_from_name(name: &str) -> Option<BuiltinFn> {
    match name.to_ascii_lowercase().as_str() {
        "vec_norm" => Some(BuiltinFn::VecNorm),
        "vec_sum" => Some(BuiltinFn::VecSum),
        "vec_div" => Some(BuiltinFn::VecDiv),
        _ => None,
    }
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
    }
}

/// 組み込み関数の arity（[`builtin_signature`] が返す引数個数）の上限。
/// `sql::expr_program::ExprStep::Builtin` の実行時、この上限個数の固定配列
/// （ヒープ確保なし）へ引数を積んでから [`apply_builtin`] を呼ぶために使う
/// （PR #373 codex-review 指摘対応・追加 `Vec` 確保の排除）。新しい組み込み関数を
/// 追加してこの上限を超える arity になる場合はここも合わせて引き上げる
/// （`builtin_arities_fit_max_arity` で全 [`BuiltinFn`] 網羅的に検証する）。
pub(crate) const MAX_BUILTIN_ARITY: usize = 2;

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
    ) || builtin_from_name(name).is_some()
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
            if let Some(builtin) = builtin_from_name(name) {
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
        BoundExpr::Number(_) | BoundExpr::IdRef | BoundExpr::VectorRef | BoundExpr::Null => 1,
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
        BoundExpr::Number(_) | BoundExpr::IdRef | BoundExpr::VectorRef | BoundExpr::Null => 0,
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
        BoundExpr::Number(_) | BoundExpr::IdRef | BoundExpr::Null => false,
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
/// 演算子と同じく `Scalar` 限定（`Vector`/`Bool` の等価比較は式層に存在しない）。
fn bind_nullif(
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    bind_with_case_nesting(env, |env| {
        let (lhs_b, lhs_t) = bind_null_aware(lhs, env, node_budget)?;
        let (rhs_b, rhs_t) = bind_null_aware(rhs, env, node_budget)?;
        for t in [lhs_t, rhs_t].into_iter().flatten() {
            if t != ExprType::Scalar {
                return Err(SqlSurfaceError::DatatypeMismatch {
                    detail: "NULLIF arguments must be scalar".to_string(),
                });
            }
        }
        if lhs_t.is_none() && rhs_t.is_none() {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "NULLIF(NULL, NULL) has no determinable type".to_string(),
            });
        }
        Ok((
            BoundExpr::NullIf {
                lhs: Box::new(lhs_b),
                rhs: Box::new(rhs_b),
            },
            ExprType::Scalar,
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
                    ColumnType::Text => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (TEXT columns are not supported)"
                    ))),
                    // `INTEGER`／`BIGINT` 列の式参照対応は Issue #891 の担当。
                    // 本 Issue（#881）では TEXT 列と同じ fail-closed 拒否に倒す。
                    ColumnType::Integer | ColumnType::BigInt => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} cannot be used in an expression yet"
                        )))
                    }
                    // F10（Issue #882 計画）: REAL/DOUBLE 列の式評価対応は #891 の
                    // 担当。現時点では TEXT 列と同じ「式内で参照できない」拒否へ
                    // 合流させる。
                    ColumnType::Real | ColumnType::Double => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} cannot be used in an expression (REAL/DOUBLE columns are not supported)"
                        )))
                    }
                    ColumnType::Boolean => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (BOOLEAN columns are not supported)"
                    ))),
                    ColumnType::Date => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (DATE columns are not supported)"
                    ))),
                    ColumnType::Timestamp => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} cannot be used in an expression (TIMESTAMP columns are not supported)"
                    ))),
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
            _ => Err(SqlSurfaceError::invalid_input(
                "'+'/'-' require both operands to be scalar",
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
            _ => Err(SqlSurfaceError::invalid_input(
                "comparison operators require both operands to be scalar",
            )),
        },
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

pub fn eval<'a>(
    expr: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    match expr {
        BoundExpr::Number(v) => Ok(ExprValue::Scalar(*v)),
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
        BoundExpr::Builtin { f, args } => eval_builtin(*f, args, id, embedding),
        BoundExpr::Binary { op, lhs, rhs } => {
            let l = eval(lhs, id, embedding)?;
            let r = eval(rhs, id, embedding)?;
            eval_binary(*op, l, r)
        }
        BoundExpr::WasmCall { backend, args, .. } => {
            // ABI 固定シグネチャ（bind_call が保証）: args[0] = Vector, args[1] = Scalar。
            let v_val = match args.first() {
                Some(e) => eval(e, id, embedding)?,
                None => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "missing function argument at evaluation time".to_string(),
                    })
                }
            };
            let s_val = match args.get(1) {
                Some(e) => eval(e, id, embedding)?,
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
        BoundExpr::Case { whens, else_result } => {
            for (cond, result) in whens {
                match eval(cond, id, embedding)? {
                    ExprValue::Bool(true) => return eval(result, id, embedding),
                    ExprValue::Bool(false) | ExprValue::Null => continue,
                    _ => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "CASE condition did not evaluate to boolean".to_string(),
                        })
                    }
                }
            }
            eval(else_result, id, embedding)
        }
        BoundExpr::Coalesce(args) => {
            for a in args {
                match eval(a, id, embedding)? {
                    ExprValue::Null => continue,
                    other => return Ok(other),
                }
            }
            Ok(ExprValue::Null)
        }
        BoundExpr::NullIf { lhs, rhs } => {
            let l = eval(lhs, id, embedding)?;
            let r = eval(rhs, id, embedding)?;
            eval_nullif(l, r)
        }
    }
}

/// `NULLIF(lhs, rhs)` を値ベースで評価する（`CASE WHEN lhs = rhs THEN NULL ELSE
/// lhs END` と等価な意味論。対象ビヘイビア: SQL-26）。再帰 `eval` の
/// `BoundExpr::NullIf` 分岐と `sql::expr_program::ExprProgram::eval` の
/// `ExprStep::NullIf` 分岐が共有する（Issue #353 と同じ「値ベース評価を 1 箇所に
/// 保つ」方針）。束縛段（[`bind_nullif`]）が両辺を `Scalar` 限定済みのため、
/// 非 `Null`・非 `Scalar` の組み合わせは束縛段の不変条件が崩れた場合の保険として
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
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    // 参照実装（再帰 `eval`）専用の非ホットパス。ステップ列実行
    // （`sql::expr_program::ExprProgram::eval`）は固定長スタック配列を使う別経路
    // （下記 `apply_builtin` のシグネチャ参照）を通るため、ここでの `Vec` 確保は
    // 行ごとのホットパスには影響しない。
    let mut values: Vec<Option<ExprValue<'a>>> = Vec::with_capacity(args.len());
    for a in args {
        values.push(Some(eval(a, id, embedding)?));
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
    if args.iter().any(|a| matches!(a, Some(ExprValue::Null))) {
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
    // NULL 伝播（対象ビヘイビア: SQL-26。Issue #921）: 算術・比較のいずれかの
    // オペランドが NULL なら結果は NULL（PostgreSQL の 3 値論理と同じ扱い。
    // 比較の NULL は WHERE 述語側で UNKNOWN として非該当扱いになる）。
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
            _ => Err(SqlSurfaceError::Internal {
                detail: "operand type mismatch at evaluation time".to_string(),
            }),
        },
    }
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
    fn text_column_reference_is_rejected() {
        let schema = schema_with_vector();
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&ident("label"), &schema, &registry, &mut budget).unwrap_err();
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
        // 解決され、TEXT 列であるため「TEXT 列は式内で使えない」エラーになるべき
        // （黙って行キー疑似列 `BoundExpr::IdRef` へフォールバックしてはならない）。
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ],
        );
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        let err = bind_expr(&ident("id"), &schema, &registry, &mut budget).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
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
        for f in [BuiltinFn::VecNorm, BuiltinFn::VecSum, BuiltinFn::VecDiv] {
            let (params, _) = builtin_signature(f);
            assert!(
                params.len() <= MAX_BUILTIN_ARITY,
                "{f:?} arity {} exceeds MAX_BUILTIN_ARITY {MAX_BUILTIN_ARITY}",
                params.len()
            );
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
        ];
        let result = apply_builtin(BuiltinFn::VecNorm, &mut args[..1]).expect("should evaluate");
        match result {
            ExprValue::Scalar(v) => assert!((v - 5.0).abs() < 1e-9),
            other => panic!("expected scalar, got {other:?}"),
        }
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
}
