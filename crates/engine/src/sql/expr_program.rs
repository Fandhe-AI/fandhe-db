//! 束縛済み式（[`crate::sql::udf_call::BoundExpr`]）の行ループ実行を、毎行の
//! 再帰ツリーウォークから「束縛時に一度だけ平坦化したステップ列を、行ループでは
//! 明示スタックで線形実行する」形へ変える（Issue #353・式評価のステップ列
//! コンパイル化。WHERE 事前/事後フィルタ・投影の `Computed` 列・集計の
//! `ScalarExpr` が共通で使う）。
//!
//! 責務境界: 本モジュールは `BoundExpr` 木から [`ExprProgram`] を導出する
//! [`ExprProgram::compile`] と、行コンテキスト（`id`・`embedding`）に対して
//! それを実行する [`ExprProgram::eval`] のみを提供する。値レベルの評価規則
//! （0 除算・非有限値・`f32` キャスト後 Infinity の fail-closed 判定、
//! `try_reserve_exact` による確保失敗時の `54000` 写像）はすべて
//! `sql::udf_call`（[`crate::sql::udf_call::eval_binary`]・
//! [`crate::sql::udf_call::apply_builtin`]）を共有し、本モジュールでは複製しない。
//! `sql::udf_call::eval`（再帰版）はセマンティクスの参照実装として残し、
//! 本モジュールの [`ExprProgram::compile`]／[`ExprProgram::eval`] が同じ結果を
//! 返すことを差分テストで検証する。
//!
//! # 評価順序の保存
//!
//! `BoundExpr::Binary` は lhs→rhs、`Builtin`/`WasmCall` の引数は左から右へ、
//! いずれも無条件（短絡評価なし）に評価する（`sql::udf_call::eval` 参照。
//! `BoundExpr` に `And`/`Or` 相当の分岐評価ノードは存在しない——複合 `WHERE` は
//! 呼び出し元ループが `Vec<BoundExpr>` を順に AND 適用する形で短絡する）。
//! そのため [`compile_node`] の後行順（postorder）平坦化は、再帰 `eval` と
//! 同一の評価順・エラー発生順を厳密に保つ。
//!
//! # 定数畳み込み（defer-on-error）
//!
//! 行に依存しない部分式（数値リテラルのみからなるスカラー算術・比較）は
//! [`try_fold_scalar`] で束縛時に 1 回だけ評価し、結果を `ConstScalar`/
//! `ConstBool` ステップへ置換する。畳み込み中にエラーになる部分式（例:
//! 定数 0 除算）は畳み込まず、平坦化のみ行って実行時評価に委ねる
//! （defer-on-error）。可視行が 1 行も評価されなければエラーが発生しないという
//! 既存契約（`sql::exec` の SCALAR 段は可視行にのみ到達する）を、畳み込みの
//! 導入で変えないための判断。`WasmCall` はバックエンドが非決定的でありうる
//! ことと EXT-6 の行単位 deadline/中断契約を維持するため、畳み込み対象に含めない
//! （`try_fold_scalar` は `WasmCall`/`IdRef`/`VectorRef`/`Builtin` を素通りし
//! `None` を返す）。

use std::borrow::Cow;
use std::sync::Arc;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::{
    self, apply_builtin, finite_scalar, id_as_finite_scalar, BinOp, BoundExpr, BuiltinFn,
    ExprValue, MAX_BUILTIN_ARITY,
};
use crate::wasm_udf::WasmUdfBackend;

/// ステップ列 1 個分の命令（密な enum。`ExprProgram::eval` の行ループでは
/// `match` による線形ディスパッチのみを行い、再帰しない）。
#[derive(Debug, Clone)]
pub(crate) enum ExprStep {
    /// 定数畳み込み済みのスカラー値（数値リテラル、またはリテラルのみから
    /// なる算術部分式の畳み込み結果）。
    ConstScalar(f64),
    /// 定数畳み込み済みの真偽値（リテラルのみからなる比較部分式の畳み込み結果）。
    ConstBool(bool),
    /// 行 `id` を [`id_as_finite_scalar`] 経由でスカラー値として push する。
    PushId,
    /// テーブルの `VECTOR` 列（行の `embedding`）への参照を push する。
    /// スタック格納値は [`StackValue::VectorRef`]（マーカーのみ。借用そのものは
    /// 保持しない）で、実際の `Cow::Borrowed(embedding)` は `ExprProgram::eval`
    /// が当該ステップを消費する時点で `embedding` 引数から都度組み立てる
    /// （`sql::udf_call::eval` の `BoundExpr::VectorRef` 分岐（Issue #352）と
    /// 同じ契約——`vec_norm(embedding)` 等の読み取り専用式では確保・複製が
    /// 一切発生しない）。PR #373 codex-review 指摘対応: 当初は
    /// `Vec<ExprValue<'a>>` をスタックに使い `embedding` と同一ライフタイム `'a`
    /// で行ループの外から使い回そうとしたが、行フックの呼び出し境界ごとに
    /// `'a` が変わる（`sql::exec::on_visible_row` のようにクロージャで
    /// `for<'r> Fn(..., &'r [f32], ...)` 相当になる、または
    /// `sql::aggregate`/`sql::group_by` のように `embedding_scratch` を毎行
    /// `&mut` 上書きデコードする）呼び出し元では `Vec<ExprValue<'a>>` を
    /// 行ループの外に persist できず（invariance・NLL の限界）、結局行ごとに
    /// 新規確保していた。[`StackValue`] は借用を保持しないためこの制約を受けず、
    /// `Vec<StackValue>` は行ループの外で 1 回だけ確保して使い回せる。
    PushVector,
    /// 組み込み関数呼び出し。arity 分（[`udf_call::builtin_signature`]）を
    /// スタックから pop し、[`apply_builtin`] へ渡す。
    Builtin(BuiltinFn),
    /// 2 項演算。rhs・lhs の順にスタックから pop し
    /// [`crate::sql::udf_call::eval_binary`] へ渡す。
    Binary(BinOp),
    /// WASM UDF 呼び出し（ABI 固定: `(Vector, Scalar) -> Scalar`。TASK-149・
    /// EXT-5）。scalar 引数（後に push された方）から先に pop する。
    WasmCall { backend: Arc<dyn WasmUdfBackend> },
}

impl PartialEq for ExprStep {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ExprStep::ConstScalar(a), ExprStep::ConstScalar(b)) => a == b,
            (ExprStep::ConstBool(a), ExprStep::ConstBool(b)) => a == b,
            (ExprStep::PushId, ExprStep::PushId) => true,
            (ExprStep::PushVector, ExprStep::PushVector) => true,
            (ExprStep::Builtin(a), ExprStep::Builtin(b)) => a == b,
            (ExprStep::Binary(a), ExprStep::Binary(b)) => a == b,
            // `Arc<dyn WasmUdfBackend>` は `dyn` 型のため構造的な `PartialEq` を
            // 導出できない。同一性判定は `Arc::ptr_eq`（`BoundExpr::WasmCall` の
            // 既存方針〔`udf_call.rs`〕を踏襲）。
            (ExprStep::WasmCall { backend: a }, ExprStep::WasmCall { backend: b }) => {
                Arc::ptr_eq(a, b)
            }
            _ => false,
        }
    }
}

/// `ExprProgram::eval` の明示スタックが積む値表現。`ExprValue<'a>` と異なり
/// 行 embedding への借用をスタック要素の型として持たない（[`ExprStep::PushVector`]
/// 参照）。これにより `Vec<StackValue>` は行ごとに変わる借用ライフタイムに
/// 紐付かず、呼び出し元の行ループの外で 1 回だけ確保し使い回せる
/// （Issue #353・PR #373 codex-review 指摘対応: 行ごとの `Vec::new()` 確保・
/// 解放を排除する）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StackValue {
    Scalar(f64),
    Bool(bool),
    /// 現在評価中の行の `embedding` への参照を表すマーカー
    /// （[`ExprStep::PushVector`] が push する）。実体は `ExprProgram::eval` の
    /// `embedding: &'a [f32]` 引数から都度解決する。
    VectorRef,
    /// 組み込み関数・二項演算が新規構築したベクトル（例: `vec_div`・
    /// vector×scalar 演算）。行データとは独立した所有データのため、そのまま
    /// スタックへ持ち回れる。
    VectorOwned(Vec<f32>),
}

/// [`StackValue`] を、eval 呼び出しスコープに閉じたライフタイム `'a` を持つ
/// [`ExprValue`] へ変換する（[`apply_builtin`]・[`udf_call::eval_binary`] へ渡す
/// 直前にのみ使う。変換結果を `'a` を超えて `Vec<StackValue>` へ書き戻すことは
/// ない——書き戻しは必ず [`expr_value_to_stack`] を経由し、借用ではなく
/// マーカー／所有データへ変換し直す）。
fn stack_to_expr_value(v: StackValue, embedding: &[f32]) -> ExprValue<'_> {
    match v {
        StackValue::Scalar(s) => ExprValue::Scalar(s),
        StackValue::Bool(b) => ExprValue::Bool(b),
        StackValue::VectorRef => ExprValue::Vector(Cow::Borrowed(embedding)),
        StackValue::VectorOwned(v) => ExprValue::Vector(Cow::Owned(v)),
    }
}

/// [`ExprValue`] を [`StackValue`] へ変換する（[`stack_to_expr_value`] の逆。
/// [`apply_builtin`]・[`udf_call::eval_binary`] の評価結果をスタックへ積み戻す
/// ときに使う）。`Cow::Borrowed` は [`ExprStep::PushVector`] 以外の経路
/// （`apply_builtin`・`udf_call::eval_binary` の各実装）が生成することはなく
/// （常に `Cow::Owned` で新規構築するか `Scalar`/`Bool` を返す。`udf_call.rs`
/// 参照）、常に現在行の `embedding` を指す。そのため `Cow::Borrowed` は
/// [`StackValue::VectorRef`] マーカーへ戻して問題ない。
fn expr_value_to_stack(v: ExprValue<'_>) -> StackValue {
    match v {
        ExprValue::Scalar(s) => StackValue::Scalar(s),
        ExprValue::Bool(b) => StackValue::Bool(b),
        ExprValue::Vector(Cow::Borrowed(_)) => StackValue::VectorRef,
        ExprValue::Vector(Cow::Owned(v)) => StackValue::VectorOwned(v),
    }
}

/// 束縛済み式 1 本をコンパイルした平坦なステップ列。束縛時（`sql::parser::bind_*`
/// 経由）に 1 回だけ構築し、行ループでは [`ExprProgram::eval`] を呼ぶだけにする
/// ことで、行ごとの再帰ツリーウォーク・enum マッチの分岐分散をなくす。
#[derive(Debug, Clone)]
pub(crate) struct ExprProgram {
    steps: Vec<ExprStep>,
    /// 実行時に値スタックが到達しうる最大深さ（コンパイル時に確定）。
    /// `ExprProgram::eval` は行ループの外で確保したスクラッチバッファを
    /// 使い回すため直接は使わないが、`steps.len()` と共に構造的な健全性の
    /// 検証（テスト）に用いる。
    pub(crate) max_stack: usize,
}

impl PartialEq for ExprProgram {
    fn eq(&self, other: &Self) -> bool {
        self.steps == other.steps && self.max_stack == other.max_stack
    }
}

/// 定数畳み込みの結果値。`ExprValue` は `Vector` variant を持つため、畳み込み
/// 対象をスカラー算術・比較のみへ構造的に限定するために専用の小さな型を使う
/// （`Vector` が生じうるかどうかを呼び出し側で判定する必要をなくす。P0
/// 「ライブラリコードで panic させない」対応・`.claude/rules/coding-rust.md`）。
enum FoldedConst {
    Scalar(f64),
    Bool(bool),
}

/// 定数畳み込みの対象を、行に依存しないスカラー算術・比較のみに限定した
/// 純粋関数（副作用なし。`ExprProgram::compile` から呼ばれる）。`IdRef`・
/// `VectorRef`・`Builtin`・`WasmCall` は行依存または非決定的なため常に
/// `None`（畳み込まない）を返す。畳み込み中の評価が `Err`（0 除算・非有限値）
/// になる場合も `None` を返し、defer-on-error を保つ（§モジュールドキュメント
/// 参照）。
fn try_fold_scalar(expr: &BoundExpr) -> Option<FoldedConst> {
    match expr {
        BoundExpr::Number(v) => Some(FoldedConst::Scalar(*v)),
        BoundExpr::Binary { op, lhs, rhs } => {
            let l = match try_fold_scalar(lhs)? {
                FoldedConst::Scalar(v) => ExprValue::Scalar(v),
                FoldedConst::Bool(b) => ExprValue::Bool(b),
            };
            let r = match try_fold_scalar(rhs)? {
                FoldedConst::Scalar(v) => ExprValue::Scalar(v),
                FoldedConst::Bool(b) => ExprValue::Bool(b),
            };
            match udf_call::eval_binary(*op, l, r).ok()? {
                ExprValue::Scalar(v) => Some(FoldedConst::Scalar(v)),
                ExprValue::Bool(b) => Some(FoldedConst::Bool(b)),
                // `l`/`r` は `try_fold_scalar` の再帰でスカラー・真偽値に限定
                // 済みのため、四則演算・比較の結果は理論上 `Vector` になり
                // 得ない。`unreachable!` ではなく畳み込み対象外（`None`）として
                // fail-safe に扱う（`compile_node` は通常のステップ平坦化へ
                // フォールバックする）。
                ExprValue::Vector(_) => None,
            }
        }
        BoundExpr::Builtin { f, args } => {
            // 数値スカラー関数群（SQL-26、Issue #920）のうち行に依存しない
            // ものだけを畳み込む（`udf_call::is_foldable_builtin` 参照。
            // `vec_norm` 等の Vector 引数を取る組み込みは常に `VectorRef` を
            // 引数に持つため実質畳み込み対象にならないが、念のため名前空間で
            // 明示的に除外する）。引数のいずれかが畳み込めない（行依存・
            // 非決定的）場合は `None` を返して通常のステップ平坦化へ委ねる。
            if !udf_call::is_foldable_builtin(*f) {
                return None;
            }
            let mut values: Vec<Option<ExprValue<'static>>> = Vec::with_capacity(args.len());
            for a in args {
                match try_fold_scalar(a)? {
                    FoldedConst::Scalar(v) => values.push(Some(ExprValue::Scalar(v))),
                    // 数値スカラー関数群の引数は常に Scalar のため理論上
                    // 到達しないが、`Bool` が畳み込まれてきた場合は畳み込み
                    // 対象外として defer-on-error にフォールバックする
                    // （fail-safe。`unreachable!` は使わない）。
                    FoldedConst::Bool(_) => return None,
                }
            }
            // 畳み込み中にエラーになる部分式（0 除算・非有限値等）は畳み込まず、
            // `None` を返して実行時評価に委ねる（defer-on-error。§モジュール
            // ドキュメント参照）。
            match apply_builtin(*f, &mut values).ok()? {
                ExprValue::Scalar(v) => Some(FoldedConst::Scalar(v)),
                // 数値スカラー関数群の戻り値は常に Scalar のため理論上到達
                // しないが、他の値種別が返ってきた場合も畳み込み対象外として
                // fail-safe に扱う。
                _ => None,
            }
        }
        BoundExpr::IdRef | BoundExpr::VectorRef | BoundExpr::WasmCall { .. } => None,
    }
}

/// `BoundExpr` を後行順（postorder）で `steps` へ平坦化する（[`ExprProgram::compile`]
/// の内部再帰）。`current_depth`／`max_stack` は実行時の値スタックの深さを
/// コンパイル時にシミュレートし、`ExprProgram::max_stack` を確定させる。
fn compile_node(
    expr: &BoundExpr,
    steps: &mut Vec<ExprStep>,
    current_depth: &mut usize,
    max_stack: &mut usize,
) {
    if let Some(folded) = try_fold_scalar(expr) {
        match folded {
            FoldedConst::Scalar(v) => steps.push(ExprStep::ConstScalar(v)),
            FoldedConst::Bool(b) => steps.push(ExprStep::ConstBool(b)),
        }
        *current_depth += 1;
        *max_stack = (*max_stack).max(*current_depth);
        return;
    }
    match expr {
        // `try_fold_scalar` が `Number` を必ず畳み込むため実行時には到達しない
        // 分岐だが、`BoundExpr` の全 variant を網羅する `match` として残す。
        BoundExpr::Number(v) => {
            steps.push(ExprStep::ConstScalar(*v));
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::IdRef => {
            steps.push(ExprStep::PushId);
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::VectorRef => {
            steps.push(ExprStep::PushVector);
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::Builtin { f, args } => {
            for a in args {
                compile_node(a, steps, current_depth, max_stack);
            }
            steps.push(ExprStep::Builtin(*f));
            *current_depth = current_depth.saturating_sub(args.len()).saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::Binary { op, lhs, rhs } => {
            compile_node(lhs, steps, current_depth, max_stack);
            compile_node(rhs, steps, current_depth, max_stack);
            steps.push(ExprStep::Binary(*op));
            *current_depth = current_depth.saturating_sub(2).saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::WasmCall { backend, args, .. } => {
            // ABI 固定（`bind_call` が保証）: args[0] = Vector, args[1] = Scalar。
            // 再帰 `eval` は `eval_vector_arg(args, 0, ...)` の後に
            // `eval_scalar_arg(args, 1, ...)` を呼ぶ（左→右）ため、同じ順で
            // push する。
            if let Some(vector_arg) = args.first() {
                compile_node(vector_arg, steps, current_depth, max_stack);
            }
            if let Some(scalar_arg) = args.get(1) {
                compile_node(scalar_arg, steps, current_depth, max_stack);
            }
            steps.push(ExprStep::WasmCall {
                backend: Arc::clone(backend),
            });
            // 束縛段（`bind_call`）が args.len() == 2 を保証する。ここで args が
            // 2 要素に満たない場合でも push 済みステップ数分だけ減算する
            // （負方向へ折り込まず `saturating_sub` で 0 未満にしない）。
            *current_depth = current_depth
                .saturating_sub(args.len().min(2))
                .saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
    }
}

impl ExprProgram {
    /// `BoundExpr` 木を平坦なステップ列へコンパイルする（束縛時に 1 回だけ
    /// 呼ぶ想定。`sql::parser::bind_where_predicates` 等から呼ばれる）。
    pub(crate) fn compile(expr: &BoundExpr) -> ExprProgram {
        let mut steps = Vec::new();
        let mut current_depth = 0usize;
        let mut max_stack = 0usize;
        compile_node(expr, &mut steps, &mut current_depth, &mut max_stack);
        ExprProgram { steps, max_stack }
    }

    /// ステップ列を明示スタック（`scratch`）で線形実行する（再帰しない）。
    /// `scratch` は呼び出し元の行ループの外で確保し、行ごとに使い回す想定
    /// （呼び出し前に空である必要はない。本関数の先頭で `clear` する）。
    ///
    /// エラー契約は再帰 `eval`（[`crate::sql::udf_call::eval`]）と同一:
    /// スタック underflow・型不一致は `SqlSurfaceError::Internal`（固定文言。
    /// 束縛段の不変条件が崩れた場合の保険であり、束縛済み式に対しては
    /// 通常発生しない）、0 除算・非有限値・確保失敗はそれぞれ既存の
    /// `22000`／`54000` 写像を共有する（[`apply_builtin`]・
    /// [`crate::sql::udf_call::eval_binary`] 経由）。
    ///
    /// `scratch` は行に依存しない借用のない値表現（[`StackValue`]）を積む
    /// ため、`embedding` の借用ライフタイム `'a` に紐付かない。呼び出し元は
    /// `scratch` を行ループの外で 1 回だけ確保し、行ごとに使い回してよい
    /// （PR #373 codex-review 指摘対応。以前は `Vec<ExprValue<'a>>` をスタックに
    /// 使っており、行フックの呼び出し境界ごとに変わる `'a` を持つ呼び出し元では
    /// 行ループの外へ persist できず行ごとの新規確保が必要だった。詳細は
    /// [`ExprStep::PushVector`] のドキュメント参照）。
    pub(crate) fn eval<'a>(
        &self,
        id: u64,
        embedding: &'a [f32],
        scratch: &mut Vec<StackValue>,
    ) -> Result<ExprValue<'a>, SqlSurfaceError> {
        scratch.clear();
        for step in &self.steps {
            match step {
                ExprStep::ConstScalar(v) => scratch.push(StackValue::Scalar(*v)),
                ExprStep::ConstBool(b) => scratch.push(StackValue::Bool(*b)),
                ExprStep::PushId => {
                    scratch.push(StackValue::Scalar(id_as_finite_scalar(id)?));
                }
                ExprStep::PushVector => {
                    // マーカーのみを push する（Issue #352 の「借用のみで確保・
                    // 複製なし」契約は、このマーカーを `stack_to_expr_value` で
                    // 消費する際に `Cow::Borrowed(embedding)` として復元する
                    // ことで維持する）。
                    scratch.push(StackValue::VectorRef);
                }
                ExprStep::Builtin(f) => {
                    let arity = udf_call::builtin_signature(*f).0.len();
                    if scratch.len() < arity {
                        return Err(stack_underflow());
                    }
                    if arity > MAX_BUILTIN_ARITY {
                        // 束縛時（`udf_call::builtin_signature`）が保証する
                        // 不変条件が崩れた場合の保険（実行時には到達しない）。
                        // 固定長バッファの上限を超える場合は fail-closed に拒否
                        // する（PR #373 codex-review 指摘対応・追加 `Vec`
                        // 確保なしで引数を受け渡すための固定配列。
                        // `builtin_arities_fit_max_arity` 参照）。
                        return Err(SqlSurfaceError::Internal {
                            detail: "builtin arity exceeds compiled argument buffer".to_string(),
                        });
                    }
                    let split_at = scratch.len() - arity;
                    // `Vec::drain` はタプル末尾（`split_at..`）を in-place で
                    // 取り除くだけで新規バッファを確保しない（`Vec::split_off`
                    // と異なり、取り除いた要素用の別 `Vec` を作らない。PR #373
                    // codex-review 指摘対応）。取り出した [`StackValue`] は
                    // 行ループの外に持ち出さない固定長配列（スタック確保）へ
                    // 積み替えてから `apply_builtin` へ渡す。
                    let mut arg_buf: [Option<ExprValue<'a>>; MAX_BUILTIN_ARITY] = [None, None];
                    for (slot, value) in arg_buf.iter_mut().zip(scratch.drain(split_at..)) {
                        *slot = Some(stack_to_expr_value(value, embedding));
                    }
                    let result = apply_builtin(*f, &mut arg_buf[..arity])?;
                    scratch.push(expr_value_to_stack(result));
                }
                ExprStep::Binary(op) => {
                    let r = scratch.pop().ok_or_else(stack_underflow)?;
                    let l = scratch.pop().ok_or_else(stack_underflow)?;
                    let result = udf_call::eval_binary(
                        *op,
                        stack_to_expr_value(l, embedding),
                        stack_to_expr_value(r, embedding),
                    )?;
                    scratch.push(expr_value_to_stack(result));
                }
                ExprStep::WasmCall { backend } => {
                    let scalar_val = scratch.pop().ok_or_else(stack_underflow)?;
                    let vector_val = scratch.pop().ok_or_else(stack_underflow)?;
                    let v = match vector_val {
                        StackValue::VectorRef => Cow::Borrowed(embedding),
                        StackValue::VectorOwned(v) => Cow::Owned(v),
                        StackValue::Scalar(_) | StackValue::Bool(_) => return Err(type_mismatch()),
                    };
                    let s = match scalar_val {
                        StackValue::Scalar(s) => s,
                        StackValue::Bool(_)
                        | StackValue::VectorRef
                        | StackValue::VectorOwned(_) => return Err(type_mismatch()),
                    };
                    // バックエンドの失敗（deadline 超過・トラップ・メモリ確保
                    // 失敗・`Mutex` poison 等）は種別を問わずすべて `22000` へ
                    // 写像する（行値・テナント情報を含まない固定文言。
                    // `sql::udf_call::eval` の `WasmCall` 分岐と同じ契約。
                    // EXT-6 の拒否・強制中断はここで行単位のエラーへ収束し、
                    // プロセスは生存する）。
                    let result = backend
                        .call_vector_scalar(&v, s)
                        .map_err(|e| SqlSurfaceError::invalid_input(e.to_string()))?;
                    scratch.push(expr_value_to_stack(finite_scalar(result, "wasm udf")?));
                }
            }
        }
        let result = scratch.pop().ok_or_else(stack_underflow)?;
        Ok(stack_to_expr_value(result, embedding))
    }
}

fn stack_underflow() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "expression program stack underflow at evaluation time".to_string(),
    }
}

fn type_mismatch() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "function argument type mismatch at evaluation time".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::udf_call::MAX_EXPR_NODES;

    fn num(v: f64) -> BoundExpr {
        BoundExpr::Number(v)
    }

    fn bin(op: BinOp, lhs: BoundExpr, rhs: BoundExpr) -> BoundExpr {
        BoundExpr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    /// 差分テスト用の共通比較（コンパイル実行結果と再帰 `eval` の結果が
    /// 一致することを検証する）。
    fn assert_matches_recursive_eval(expr: &BoundExpr, id: u64, embedding: &[f32]) {
        let program = ExprProgram::compile(expr);
        let mut scratch = Vec::new();
        let compiled = program.eval(id, embedding, &mut scratch);
        let recursive = udf_call::eval(expr, id, embedding);
        match (compiled, recursive) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "compiled/recursive eval diverged"),
            (Err(_), Err(_)) => {} // 両者ともエラー（詳細メッセージの一致までは要求しない）
            (a, b) => panic!("compiled={a:?} recursive={b:?} diverged in Ok/Err"),
        }
    }

    #[test]
    fn number_literal_folds_to_const_scalar() {
        let expr = num(3.5);
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstScalar(3.5)]);
        assert_eq!(program.max_stack, 1);
    }

    #[test]
    fn constant_arithmetic_folds_to_single_step() {
        let expr = bin(BinOp::Mul, num(2.0), num(3.0));
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstScalar(6.0)]);
        assert_eq!(program.max_stack, 1);
    }

    #[test]
    fn constant_comparison_folds_to_const_bool() {
        let expr = bin(BinOp::Lt, num(1.0), num(2.0));
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstBool(true)]);
    }

    #[test]
    fn constant_division_by_zero_is_not_folded_defer_on_error() {
        // defer-on-error: 定数 0 除算はコンパイル時に畳み込まず、平坦化のみ
        // 行う（§モジュールドキュメント参照）。畳み込まれていれば
        // `ConstScalar`/`ConstBool` 1 ステップになるはずが、ここでは
        // `Binary` ステップが残ることを確認する。
        let expr = bin(BinOp::Div, num(1.0), num(0.0));
        let program = ExprProgram::compile(&expr);
        assert!(matches!(
            program.steps.last(),
            Some(ExprStep::Binary(BinOp::Div))
        ));
        // 実行時（行が評価された時点）でのみ 22000 相当のエラーになる。
        let mut scratch = Vec::new();
        assert!(program.eval(1, &[], &mut scratch).is_err());
    }

    #[test]
    fn id_ref_is_not_folded_and_reflects_row_id() {
        let expr = BoundExpr::IdRef;
        assert_matches_recursive_eval(&expr, 42, &[]);
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::PushId]);
    }

    #[test]
    fn id_beyond_exact_f64_range_is_rejected_fail_closed() {
        let expr = BoundExpr::IdRef;
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert!(program.eval(1u64 << 60, &[], &mut scratch).is_err());
    }

    #[test]
    fn vector_ref_matches_recursive_eval() {
        let expr = BoundExpr::VectorRef;
        assert_matches_recursive_eval(&expr, 1, &[1.0, 2.0, 3.0]);
    }

    /// PR #373 codex-review 指摘対応: `ExprStep::PushVector` が行の embedding を
    /// 確保・複製せず借用のまま返すことを検証する（Issue #352 の
    /// `sql::udf_call::eval` の `Cow::Borrowed` 契約と同じ挙動を、ステップ列
    /// コンパイル経由の評価でも維持することの回帰防止）。ポインタ一致
    /// （`as_ptr`）まで確認し、`Cow::Owned` へコピーされていないことを保証する。
    #[test]
    fn vector_ref_borrows_embedding_without_copy() {
        let expr = BoundExpr::VectorRef;
        let program = ExprProgram::compile(&expr);
        let embedding = [1.0f32, 2.0, 3.0];
        let mut scratch = Vec::new();
        let value = program
            .eval(1, &embedding, &mut scratch)
            .expect("VectorRef eval should succeed");
        match value {
            ExprValue::Vector(Cow::Borrowed(borrowed)) => {
                assert_eq!(borrowed.as_ptr(), embedding.as_ptr());
            }
            other => panic!("expected Cow::Borrowed vector, got {other:?}"),
        }
    }

    #[test]
    fn mixed_row_dependent_and_constant_subexpr_matches_recursive_eval() {
        // `id * (2.0 * 3.0)`: 右部分木のみ畳み込まれ、左部分木（行依存）は
        // 実行時にのみ確定する。
        let expr = bin(
            BinOp::Mul,
            BoundExpr::IdRef,
            bin(BinOp::Mul, num(2.0), num(3.0)),
        );
        assert_matches_recursive_eval(&expr, 4, &[]);
        let program = ExprProgram::compile(&expr);
        assert_eq!(
            program.steps,
            vec![
                ExprStep::PushId,
                ExprStep::ConstScalar(6.0),
                ExprStep::Binary(BinOp::Mul)
            ]
        );
    }

    #[test]
    fn builtin_vec_norm_matches_recursive_eval() {
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::VecNorm,
            args: vec![BoundExpr::VectorRef],
        };
        assert_matches_recursive_eval(&expr, 1, &[3.0, 4.0]);
    }

    #[test]
    fn builtin_vec_div_by_zero_is_fail_closed_and_matches_recursive_eval() {
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::VecDiv,
            args: vec![BoundExpr::VectorRef, num(0.0)],
        };
        assert_matches_recursive_eval(&expr, 1, &[1.0, 2.0]);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert!(program.eval(1, &[1.0, 2.0], &mut scratch).is_err());
    }

    #[test]
    fn comparison_expression_matches_recursive_eval() {
        let expr = bin(BinOp::Gt, BoundExpr::IdRef, num(10.0));
        assert_matches_recursive_eval(&expr, 5, &[]);
        assert_matches_recursive_eval(&expr, 20, &[]);
    }

    /// Issue #353 の受け入れ条件 3（前後比較の記録）用の手動専用ベンチマーク。
    /// 本リポには計画が名指した `feature_bench` 相当の汎用ベンチ例が存在せず
    /// （`crates/engine/examples/` を参照）、既存の SQL 表層ベンチ
    /// （`benches/sql_c1_bench.rs`）は spec 由来の非公開閾値環境変数が前提のため
    /// 本セッションからは実行できない。そのため、行ループの分岐機構そのもの
    /// （再帰ツリーウォーク vs 平坦ステップ列の線形実行）を同一プロセス内で
    /// 直接比較する。両実装は本 PR で並存する（`udf_call::eval` は参照実装として
    /// 残置）ため、この比較は「Issue #353 が変えた部分」の前後差を厳密に表す。
    /// `cargo test -p fandhe-vector-db-engine --lib sql::expr_program -- --ignored --nocapture`
    /// で手動実行し、実測値を `docs/design/expr-step-compilation.md` へ転記する
    /// （CI では実行しない。デフォルトでは無視される診断用ベンチのため）。
    #[test]
    #[ignore]
    fn bench_recursive_eval_vs_compiled_program() {
        use std::time::Instant;

        // WHERE 述語で典型的な複合式（`vec_norm(embedding) > 2.0 AND vec_sum(...) ...`
        // 相当の 1 述語分に近い深さ）を模した式木。定数畳み込みが効かない
        // 行依存の組み込み関数呼び出し中心（実クエリの主要コストである行ごとの
        // 評価そのものを測る）。
        let embedding: Vec<f32> = (0..16).map(|i| i as f32 * 0.5 + 1.0).collect();
        let expr = bin(
            BinOp::Gt,
            BoundExpr::Builtin {
                f: BuiltinFn::VecNorm,
                args: vec![BoundExpr::VectorRef],
            },
            num(2.0),
        );
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();

        const ITERS: u64 = 200_000;

        // ウォームアップ（ページフォールト・分岐予測のコールドスタートを両者から除く）。
        for id in 0..1000u64 {
            let _ = udf_call::eval(&expr, id, &embedding);
            let _ = program.eval(id, &embedding, &mut scratch);
        }

        let start = Instant::now();
        for id in 0..ITERS {
            std::hint::black_box(udf_call::eval(&expr, id, &embedding).unwrap());
        }
        let recursive_elapsed = start.elapsed();

        let start = Instant::now();
        for id in 0..ITERS {
            std::hint::black_box(program.eval(id, &embedding, &mut scratch).unwrap());
        }
        let compiled_elapsed = start.elapsed();

        eprintln!(
            "bench_recursive_eval_vs_compiled_program: recursive={recursive_elapsed:?} \
             compiled={compiled_elapsed:?} iters={ITERS}"
        );
    }

    #[test]
    fn step_count_never_exceeds_bound_expr_node_budget() {
        // `steps.len()` はコンパイル時に 1 ノード → 高々 1 ステップの対応で
        // 平坦化するため、束縛段の `MAX_EXPR_NODES`（node_budget）を超えない
        // （境界の検証。DoS 耐性の裏付け）。ここでは深いが定数畳み込みされない
        // （id 参照を交互に挟む）連鎖式で検証する。
        let mut expr = BoundExpr::IdRef;
        let depth = 64; // MAX_EXPR_DEPTH 内に収まる程度の深さ
        for _ in 0..depth {
            expr = bin(BinOp::Add, expr, num(1.0));
        }
        let program = ExprProgram::compile(&expr);
        assert!(program.steps.len() <= MAX_EXPR_NODES);
        assert!(program.max_stack <= program.steps.len());
    }

    // --- 数値スカラー関数群の定数畳み込み（Issue #920・AC3） --------------------

    #[test]
    fn deterministic_numeric_builtin_folds_to_const_scalar() {
        // `abs(0 - 1)` は行に依存しない部分式のみで構成されるため 1 ステップへ
        // 畳み込まれる。
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::Abs,
            args: vec![bin(BinOp::Sub, num(0.0), num(1.0))],
        };
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstScalar(1.0)]);
    }

    #[test]
    fn numeric_builtin_over_row_dependent_argument_is_not_folded() {
        // `abs(id)` は行 `id` に依存するため畳み込まれず、`Builtin` ステップが
        // 実行時まで残る（defer-on-error と同じ「行に依存する式は畳み込まない」
        // 方針）。
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::Abs,
            args: vec![BoundExpr::IdRef],
        };
        let program = ExprProgram::compile(&expr);
        assert!(matches!(
            program.steps.last(),
            Some(ExprStep::Builtin(BuiltinFn::Abs))
        ));
    }

    #[test]
    fn numeric_builtin_that_errors_when_folded_defers_to_runtime() {
        // `sqrt(0 - 1)` は定数のみから成るが計算がエラーになるため、
        // defer-on-error により畳み込まれず実行時評価に委ねられる。可視行が
        // 0 件のテーブルでは実行時評価自体が起きないため、これによって
        // クエリ全体が失敗することはない（既存の defer-on-error 契約）。
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::Sqrt,
            args: vec![bin(BinOp::Sub, num(0.0), num(1.0))],
        };
        let program = ExprProgram::compile(&expr);
        assert!(matches!(
            program.steps.last(),
            Some(ExprStep::Builtin(BuiltinFn::Sqrt))
        ));
        let mut scratch = Vec::new();
        assert!(program.eval(1, &[], &mut scratch).is_err());
    }

    #[test]
    fn vec_norm_builtin_is_never_folded_regardless_of_argument() {
        // `is_foldable_builtin` が Vector 系組み込みを畳み込み対象外にしている
        // ことの直接的な回帰固定。
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::VecNorm,
            args: vec![BoundExpr::VectorRef],
        };
        let program = ExprProgram::compile(&expr);
        assert!(matches!(
            program.steps.last(),
            Some(ExprStep::Builtin(BuiltinFn::VecNorm))
        ));
    }

    /// PR #373 codex-review 指摘 1 対応の回帰テスト: `scratch`（[`StackValue`]
    /// スタック）が行ループの外で 1 回だけ確保され、`eval` 呼び出しのたびに
    /// `Vec::new()` で再確保されないことを検証する。`eval` は先頭で `clear()`
    /// するのみで容量は保つ契約（本モジュールドキュメント参照）のため、
    /// 事前に `with_capacity` で確保した容量が複数回の呼び出しを経ても
    /// 縮小しない（`Vec::new()` に置き換わっていれば容量は 0 へ戻る）ことを
    /// 確認する。`Builtin`（`vec_norm`）ステップを含む式で検証し、
    /// `ExprStep::Builtin` の実行（固定長引数バッファ経由。指摘 2 対応）が
    /// スタック自体の再確保を引き起こさないことも合わせて確かめる。
    #[test]
    fn scratch_buffer_retains_capacity_across_repeated_eval_calls() {
        let expr = bin(
            BinOp::Gt,
            BoundExpr::Builtin {
                f: BuiltinFn::VecNorm,
                args: vec![BoundExpr::VectorRef],
            },
            num(2.0),
        );
        let program = ExprProgram::compile(&expr);
        let embedding = [3.0f32, 4.0];

        let mut scratch: Vec<StackValue> = Vec::with_capacity(8);
        let reserved_capacity = scratch.capacity();
        assert!(reserved_capacity >= 8);

        for id in 0..100u64 {
            let result = program
                .eval(id, &embedding, &mut scratch)
                .expect("vec_norm(embedding) > 2.0 should evaluate successfully");
            assert_eq!(result, ExprValue::Bool(true));
            // `eval` は `clear()` のみを行うため、事前に確保した容量を
            // 下回ることはない（`Vec::new()` による再確保であれば容量は 0 に
            // 戻り、このアサーションが失敗する）。
            assert!(
                scratch.capacity() >= reserved_capacity,
                "scratch capacity shrank at id={id}, indicating a fresh Vec allocation \
                 inside eval() rather than buffer reuse"
            );
        }
    }
}
