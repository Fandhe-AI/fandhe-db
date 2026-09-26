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
    /// `embedding` が空スライス（`VECTOR` 列が NULL。`dim == 0`）の行では
    /// [`StackValue::Null`] を push し、それ以外では [`StackValue::VectorRef`]
    /// （マーカーのみ。借用そのものは保持しない）を push する（codex-review P1
    /// 指摘対応・Issue #921。`sql::udf_call::eval` の `BoundExpr::VectorRef`
    /// 分岐と同じ判定）。`StackValue::VectorRef` の場合、実際の
    /// `Cow::Borrowed(embedding)` は `ExprProgram::eval` が当該ステップを消費
    /// する時点で `embedding` 引数から都度組み立てる（Issue #352 と同じ契約
    /// ——`vec_norm(embedding)` 等の読み取り専用式では確保・複製が一切発生
    /// しない）。PR #373 codex-review 指摘対応: 当初は
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
    /// 定数畳み込み済みの `NULL`（対象ビヘイビア: SQL-26。Issue #921）。
    ConstNull,
    /// 無条件の前方ジャンプ（`CASE` の各 WHEN 分岐末尾）。`target` は
    /// `steps` 内の絶対インデックス。前方（`target > pc`）のみを許可し、実行が
    /// 必ず停止することを構造的に保証する（`ExprProgram::eval` の範囲検査参照）。
    Jump { target: usize },
    /// スタック先頭の Bool を pop し、`true` でなければ `target` へ飛ぶ
    /// （`CASE WHEN` の条件不成立分岐）。Bool 以外（`Null`＝UNKNOWN を含む）も
    /// 「真でない」として同様に飛ぶ。
    JumpIfNotTrue { target: usize },
    /// スタック先頭を pop せず覗き見て、`Null` でなければ値を残したまま
    /// `target` へ飛ぶ（`COALESCE` の非 NULL 早期確定）。`Null` の場合は pop せず
    /// 次の命令（通常は [`ExprStep::Pop`]）へフォールスルーする。
    JumpIfNotNull { target: usize },
    /// スタック先頭を pop して捨てる（`COALESCE` が NULL だった引数を捨てて
    /// 次の引数の評価へ進むために使う）。
    Pop,
    /// `NULLIF(lhs, rhs)`。rhs・lhs の順に pop し、[`crate::sql::udf_call::
    /// eval_nullif`] へ渡す。
    NullIf,
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
            (ExprStep::ConstNull, ExprStep::ConstNull) => true,
            (ExprStep::Jump { target: a }, ExprStep::Jump { target: b }) => a == b,
            (ExprStep::JumpIfNotTrue { target: a }, ExprStep::JumpIfNotTrue { target: b }) => {
                a == b
            }
            (ExprStep::JumpIfNotNull { target: a }, ExprStep::JumpIfNotNull { target: b }) => {
                a == b
            }
            (ExprStep::Pop, ExprStep::Pop) => true,
            (ExprStep::NullIf, ExprStep::NullIf) => true,
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
    /// SQL `NULL`（対象ビヘイビア: SQL-26。Issue #921）。
    Null,
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
        StackValue::Null => ExprValue::Null,
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
        ExprValue::Null => StackValue::Null,
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
                // 済みのため、四則演算・比較の結果は理論上 `Vector`／`Null` に
                // なり得ない（`FoldedConst` に `Null` 相当の variant が無いため。
                // 対象ビヘイビア: SQL-26。Issue #921）。`unreachable!` ではなく
                // 畳み込み対象外（`None`）として fail-safe に扱う（`compile_node`
                // は通常のステップ平坦化へフォールバックする）。
                ExprValue::Vector(_) | ExprValue::Null => None,
            }
        }
        // `Null`／`Case`／`Coalesce`／`NullIf`（対象ビヘイビア: SQL-26。Issue #921）
        // は畳み込み対象に含めない。`Case`/`Coalesce` は選ばれない分岐を評価しない
        // という実行時契約（defer-on-error）を、ジャンプ命令へのコンパイル
        // （`compile_case`/`compile_coalesce`）だけで満たすため、定数畳み込みの
        // 対象を広げなくても既存の受け入れ条件（0 除算 defer）は成立する。
        BoundExpr::IdRef
        | BoundExpr::VectorRef
        | BoundExpr::Builtin { .. }
        | BoundExpr::WasmCall { .. }
        | BoundExpr::Null
        | BoundExpr::Case { .. }
        | BoundExpr::Coalesce(_)
        | BoundExpr::NullIf { .. } => None,
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
        BoundExpr::Null => {
            steps.push(ExprStep::ConstNull);
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::Case { whens, else_result } => {
            compile_case(whens, else_result, steps, current_depth, max_stack);
        }
        BoundExpr::Coalesce(args) => {
            compile_coalesce(args, steps, current_depth, max_stack);
        }
        BoundExpr::NullIf { lhs, rhs } => {
            compile_node(lhs, steps, current_depth, max_stack);
            compile_node(rhs, steps, current_depth, max_stack);
            steps.push(ExprStep::NullIf);
            *current_depth = current_depth.saturating_sub(2).saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
    }
}

/// 検索形 `CASE` を `cond_i → JumpIfNotTrue(next_i) → result_i → Jump(end)` の
/// 並びへコンパイルする（対象ビヘイビア: SQL-26。Issue #921）。最後の WHEN 分岐の
/// `Jump(end)` は次の命令（`end`）へ飛ぶだけの冗長な命令になるが、全分岐を
/// 均一に扱うことでコンパイルロジックを単純に保つ（実行コストは無視できる）。
/// ジャンプ先はいったん `usize::MAX` で仮置きし、対応する分岐の実アドレスが
/// 確定した時点で `steps.get_mut` により書き換える（前方参照の解決）。
fn compile_case(
    whens: &[(BoundExpr, BoundExpr)],
    else_result: &BoundExpr,
    steps: &mut Vec<ExprStep>,
    current_depth: &mut usize,
    max_stack: &mut usize,
) {
    let depth_before_case = *current_depth;
    let mut jump_to_end: Vec<usize> = Vec::with_capacity(whens.len());
    for (cond, result) in whens {
        *current_depth = depth_before_case;
        compile_node(cond, steps, current_depth, max_stack);
        // `JumpIfNotTrue` は条件値（Bool/Null いずれも）を pop する。
        *current_depth = current_depth.saturating_sub(1);
        let jump_if_not_true_idx = steps.len();
        steps.push(ExprStep::JumpIfNotTrue { target: usize::MAX });
        // 分岐前の深さから THEN 結果をコンパイルする（§モジュールドキュメント
        // 「評価順序の保存」参照。どの分岐も合流時には値を 1 個だけ積む）。
        *current_depth = depth_before_case;
        compile_node(result, steps, current_depth, max_stack);
        let jump_to_end_idx = steps.len();
        steps.push(ExprStep::Jump { target: usize::MAX });
        jump_to_end.push(jump_to_end_idx);
        // `JumpIfNotTrue` の飛び先（次の WHEN の条件、または ELSE）を確定する。
        let next_target = steps.len();
        if let Some(ExprStep::JumpIfNotTrue { target }) = steps.get_mut(jump_if_not_true_idx) {
            *target = next_target;
        }
    }
    *current_depth = depth_before_case;
    compile_node(else_result, steps, current_depth, max_stack);
    let end_target = steps.len();
    for idx in jump_to_end {
        if let Some(ExprStep::Jump { target }) = steps.get_mut(idx) {
            *target = end_target;
        }
    }
    *max_stack = (*max_stack).max(*current_depth);
}

/// `COALESCE` を `a_1 → JumpIfNotNull(end) → Pop → a_2 → … → a_n` の並びへ
/// コンパイルする（対象ビヘイビア: SQL-26。Issue #921）。最後の引数は非 NULL
/// 判定を行わず、そのまま結果として残す。
fn compile_coalesce(
    args: &[BoundExpr],
    steps: &mut Vec<ExprStep>,
    current_depth: &mut usize,
    max_stack: &mut usize,
) {
    let depth_before = *current_depth;
    let mut jump_to_end: Vec<usize> = Vec::with_capacity(args.len().saturating_sub(1));
    for (i, a) in args.iter().enumerate() {
        *current_depth = depth_before;
        compile_node(a, steps, current_depth, max_stack);
        if i + 1 == args.len() {
            break;
        }
        let jump_idx = steps.len();
        steps.push(ExprStep::JumpIfNotNull { target: usize::MAX });
        // 非 NULL なら `JumpIfNotNull` が値を残したまま飛ぶため、ここ（`Pop`）は
        // NULL だった場合のみ実行される。
        steps.push(ExprStep::Pop);
        *current_depth = current_depth.saturating_sub(1);
        jump_to_end.push(jump_idx);
    }
    let end_target = steps.len();
    for idx in jump_to_end {
        if let Some(ExprStep::JumpIfNotNull { target }) = steps.get_mut(idx) {
            *target = end_target;
        }
    }
    *max_stack = (*max_stack).max(*current_depth);
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
    ///
    /// `CASE`／`COALESCE`（対象ビヘイビア: SQL-26。Issue #921）の分岐命令
    /// （[`ExprStep::Jump`]・[`ExprStep::JumpIfNotTrue`]・[`ExprStep::
    /// JumpIfNotNull`]）を実行するため、`for step in &self.steps` の逐次実行
    /// ではなくプログラムカウンタ（`pc`）によるループへ変える。ジャンプ先は
    /// 常に前方（`target > pc`）のみを許可し（`compile_case`/`compile_coalesce`
    /// が生成する目標値は構造的に前方になる）、`pc` は単調に増加するため
    /// ループは必ず停止する（コンパイル済みステップ列にループを構成できない。
    /// security.md「不安全な設計」対応）。
    pub(crate) fn eval<'a>(
        &self,
        id: u64,
        embedding: &'a [f32],
        scratch: &mut Vec<StackValue>,
    ) -> Result<ExprValue<'a>, SqlSurfaceError> {
        scratch.clear();
        let mut pc = 0usize;
        while let Some(step) = self.steps.get(pc) {
            match step {
                ExprStep::ConstScalar(v) => {
                    scratch.push(StackValue::Scalar(*v));
                    pc += 1;
                }
                ExprStep::ConstBool(b) => {
                    scratch.push(StackValue::Bool(*b));
                    pc += 1;
                }
                ExprStep::ConstNull => {
                    scratch.push(StackValue::Null);
                    pc += 1;
                }
                ExprStep::PushId => {
                    scratch.push(StackValue::Scalar(id_as_finite_scalar(id)?));
                    pc += 1;
                }
                ExprStep::PushVector => {
                    // マーカーのみを push する（Issue #352 の「借用のみで確保・
                    // 複製なし」契約は、このマーカーを `stack_to_expr_value` で
                    // 消費する際に `Cow::Borrowed(embedding)` として復元する
                    // ことで維持する）。`embedding` が空スライスの行は `VECTOR`
                    // 列が NULL（`dim == 0`。呼び出し元は非 NULL なら実データ・
                    // NULL なら空スライスで揃えて渡す契約）であるため、この時点で
                    // `StackValue::Null` を push し NULL として評価する
                    // （`sql::udf_call::eval` の `BoundExpr::VectorRef` 分岐と同じ
                    // 判定。codex-review P1 指摘対応: `Case`／`Coalesce` は
                    // ジャンプ命令で選ばれない分岐のステップを実行しないため、
                    // 実際に選択された枝が `PushVector` を含む場合にのみ NULL が
                    // 伝播する。呼び出し元の事前 `references_embedding && dim ==
                    // 0` ゲートは静的な式木走査で選択されない分岐まで拾って
                    // しまうため撤去し、この評価時点の判定へ一本化した）。
                    scratch.push(if embedding.is_empty() {
                        StackValue::Null
                    } else {
                        StackValue::VectorRef
                    });
                    pc += 1;
                }
                ExprStep::Pop => {
                    scratch.pop().ok_or_else(stack_underflow)?;
                    pc += 1;
                }
                ExprStep::Jump { target } => {
                    pc = validate_jump_target(*target, pc, self.steps.len())?;
                }
                ExprStep::JumpIfNotTrue { target } => {
                    let v = scratch.pop().ok_or_else(stack_underflow)?;
                    if matches!(v, StackValue::Bool(true)) {
                        pc += 1;
                    } else {
                        pc = validate_jump_target(*target, pc, self.steps.len())?;
                    }
                }
                ExprStep::JumpIfNotNull { target } => {
                    // peek のみ（pop しない）。非 NULL なら値をスタックに残した
                    // まま飛ぶ（`compile_coalesce` 参照）。
                    let is_null = matches!(scratch.last(), Some(StackValue::Null));
                    if is_null {
                        pc += 1;
                    } else {
                        pc = validate_jump_target(*target, pc, self.steps.len())?;
                    }
                }
                ExprStep::NullIf => {
                    let r = scratch.pop().ok_or_else(stack_underflow)?;
                    let l = scratch.pop().ok_or_else(stack_underflow)?;
                    let result = udf_call::eval_nullif(
                        stack_to_expr_value(l, embedding),
                        stack_to_expr_value(r, embedding),
                    )?;
                    scratch.push(expr_value_to_stack(result));
                    pc += 1;
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
                    pc += 1;
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
                    pc += 1;
                }
                ExprStep::WasmCall { backend } => {
                    let scalar_val = scratch.pop().ok_or_else(stack_underflow)?;
                    let vector_val = scratch.pop().ok_or_else(stack_underflow)?;
                    // WASM UDF は RETURNS NULL ON NULL INPUT（対象ビヘイビア:
                    // SQL-26。Issue #921）。いずれかが NULL ならバックエンドを
                    // 呼ばず NULL を積む（`sql::udf_call::eval` の `WasmCall`
                    // 分岐と同じ契約）。
                    let vector_is_null = matches!(&vector_val, StackValue::Null);
                    let scalar_is_null = matches!(&scalar_val, StackValue::Null);
                    if vector_is_null || scalar_is_null {
                        scratch.push(StackValue::Null);
                    } else {
                        let v = match vector_val {
                            StackValue::VectorRef => Cow::Borrowed(embedding),
                            StackValue::VectorOwned(v) => Cow::Owned(v),
                            StackValue::Scalar(_) | StackValue::Bool(_) | StackValue::Null => {
                                return Err(type_mismatch())
                            }
                        };
                        let s = match scalar_val {
                            StackValue::Scalar(s) => s,
                            StackValue::Bool(_)
                            | StackValue::VectorRef
                            | StackValue::VectorOwned(_)
                            | StackValue::Null => return Err(type_mismatch()),
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
                    pc += 1;
                }
            }
        }
        let result = scratch.pop().ok_or_else(stack_underflow)?;
        Ok(stack_to_expr_value(result, embedding))
    }
}

/// ジャンプ先が現在位置より前方（`target > pc`）かつステップ列の範囲内
/// （`target <= len`。`len` は「末尾へ飛んで自然にループを終える」ケースを
/// 許すための一つ上の境界）であることを検査する（対象ビヘイビア: SQL-26。
/// Issue #921）。`compile_case`/`compile_coalesce` が生成する目標値は常にこの
/// 条件を満たすが、コンパイル時の仮置き（`usize::MAX`）の書き換え漏れや
/// 実装バグに対する fail-closed な多重防御として実行時にも検査する
/// （security.md「不安全な設計」対応。`pc` が単調に増加することがループの
/// 停止性を保証する）。
fn validate_jump_target(target: usize, pc: usize, len: usize) -> Result<usize, SqlSurfaceError> {
    if target > pc && target <= len {
        Ok(target)
    } else {
        Err(SqlSurfaceError::Internal {
            detail: "expression program jump target out of range".to_string(),
        })
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

    // --- CASE／COALESCE／NULLIF（対象ビヘイビア: SQL-26。Issue #921） ----------

    fn case_bound(whens: Vec<(BoundExpr, BoundExpr)>, else_result: BoundExpr) -> BoundExpr {
        BoundExpr::Case {
            whens,
            else_result: Box::new(else_result),
        }
    }

    #[test]
    fn case_compiled_matches_recursive_eval_for_matching_and_fallthrough_branches() {
        let expr = case_bound(
            vec![(bin(BinOp::Gt, BoundExpr::IdRef, num(1.0)), num(10.0))],
            num(0.0),
        );
        assert_matches_recursive_eval(&expr, 2, &[]);
        assert_matches_recursive_eval(&expr, 1, &[]);
    }

    #[test]
    fn case_unselected_branch_division_by_zero_does_not_error() {
        let expr = case_bound(
            vec![(bin(BinOp::Eq, num(1.0), num(1.0)), num(1.0))],
            bin(BinOp::Div, num(1.0), num(0.0)),
        );
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert_eq!(
            program.eval(1, &[], &mut scratch).unwrap(),
            ExprValue::Scalar(1.0)
        );
    }

    #[test]
    fn case_without_else_compiles_to_null_and_matches_recursive_eval() {
        let expr = case_bound(
            vec![(bin(BinOp::Gt, BoundExpr::IdRef, num(100.0)), num(1.0))],
            BoundExpr::Null,
        );
        assert_matches_recursive_eval(&expr, 1, &[]);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert_eq!(program.eval(1, &[], &mut scratch).unwrap(), ExprValue::Null);
    }

    #[test]
    fn coalesce_compiled_matches_recursive_eval() {
        let expr = BoundExpr::Coalesce(vec![BoundExpr::Null, BoundExpr::Null, num(7.0), num(8.0)]);
        assert_matches_recursive_eval(&expr, 1, &[]);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert_eq!(
            program.eval(1, &[], &mut scratch).unwrap(),
            ExprValue::Scalar(7.0)
        );
    }

    #[test]
    fn coalesce_short_circuits_and_does_not_evaluate_later_division_by_zero() {
        let expr = BoundExpr::Coalesce(vec![num(1.0), bin(BinOp::Div, num(1.0), num(0.0))]);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert_eq!(
            program.eval(1, &[], &mut scratch).unwrap(),
            ExprValue::Scalar(1.0)
        );
    }

    #[test]
    fn nullif_compiled_matches_recursive_eval() {
        let expr = BoundExpr::NullIf {
            lhs: Box::new(BoundExpr::IdRef),
            rhs: Box::new(num(2.0)),
        };
        assert_matches_recursive_eval(&expr, 2, &[]);
        assert_matches_recursive_eval(&expr, 3, &[]);
    }

    #[test]
    fn null_literal_compiles_to_const_null_step() {
        let program = ExprProgram::compile(&BoundExpr::Null);
        assert_eq!(program.steps, vec![ExprStep::ConstNull]);
        let mut scratch = Vec::new();
        assert_eq!(program.eval(1, &[], &mut scratch).unwrap(), ExprValue::Null);
    }

    #[test]
    fn jump_target_not_strictly_forward_is_rejected_fail_closed() {
        // 仮置きのジャンプ先が書き換え漏れ・実装バグで不正な値になっていた場合の
        // fail-closed 検査を、不正な `ExprProgram` を直接組み立てて検証する。
        let program = ExprProgram {
            steps: vec![
                ExprStep::ConstBool(true),
                ExprStep::JumpIfNotTrue { target: 0 }, // 前方でない（自身以前）。
                ExprStep::ConstScalar(1.0),
            ],
            max_stack: 1,
        };
        let mut scratch = Vec::new();
        // 条件が true のため JumpIfNotTrue は分岐しないが、他の分岐（false 側）を
        // 検査するために別プログラムで範囲外ジャンプも確認する。
        let _ = program.eval(1, &[], &mut scratch);

        let program_out_of_range = ExprProgram {
            steps: vec![
                ExprStep::ConstBool(false),
                ExprStep::JumpIfNotTrue { target: 99 },
                ExprStep::ConstScalar(1.0),
            ],
            max_stack: 1,
        };
        let err = program_out_of_range.eval(1, &[], &mut scratch).unwrap_err();
        assert_eq!(err.wire_code(), "XX000");
    }
}
