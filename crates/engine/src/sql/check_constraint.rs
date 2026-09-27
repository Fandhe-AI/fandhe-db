//! `CREATE TABLE` の `CHECK` 制約（TABLE-16・TASK-204、Issue #906）の意味論検証・
//! 正規化レンダリング・書き込み時コンパイル/評価を担う。
//!
//! 責務境界: 構文段（`sql::allowlist::Parser::parse_check_clause`）が組み立てた
//! [`ParsedCheck`] を受け取り、`sql::parser::bind_check_predicates`（`sql::parser::
//! bind_where_predicates` と束縛経路を共有する CHECK 専用の薄いラッパー。
//! Issue #1075・TABLE-16 ポインタ）へそのまま委譲して意味論検証する（第 2 の
//! 評価器を作らない。CLAUDE.md「委譲方針」）。CHECK 固有の追加検証は「参照可能な
//! 要素の絞り込み」（`visible()`・セッション UDF・WASM UDF・未知関数の拒否）と
//! 「正規化レンダリングの往復一致」のみ。
//!
//! `CompiledChecks` は書き込み時の単一検査点 `constraint::enforce_row_constraints_in_txn`
//! （`tenant.rs` の全書き込み関数が行の書き込み後・commit 前に呼ぶ。明示
//! トランザクション中の書き込みも同じ検査点を通る）が呼ぶ実行時コンパイル・評価
//! 本体で、`row_codec::scan_scalar_columns_masked`（宣言的フィルタ側）・
//! `sql::expr_program::ExprProgram`（式フィルタ側。TASK-79・SQL-9 の既存
//! コンパイラをそのまま再利用）で評価する。
//!
//! `CHECK` の式比較（`WherePredicate::Expression`）で参照できるのは疑似列
//! `id`・`VECTOR` 列（`vec_norm`/`vec_sum`/`vec_div` 経由）・INTEGER/BIGINT/
//! REAL/DOUBLE 列（Issue #1075・TABLE-16 ポインタ。`sql::udf_call::
//! ColumnRefPolicy::AllowNumericColumns` による CHECK 専用の opt-in 拡張）。
//! TEXT/BOOLEAN/DATE/NUMERIC 等の他の列型は引き続き拒否される。NULL・非有限値・
//! 精度の評価規則は `docs/design/sql-check-constraint.md` 参照。汎用の
//! `WHERE`／`SELECT` へ数値列の式参照を広げるレーン A は対象外（別 Issue）。

use crate::catalog::{CheckConstraint, TableSchema};
use crate::declarative_filter::MetadataFilter;
use crate::sql::allowlist::{ParsedCheck, SqlSurfaceError, WherePredicate};
use crate::sql::expr_program::{ExprProgram, StackValue};
use crate::sql::udf_call::{self, Expr, ExprValue};
use crate::tenant::TenantWriteError;

/// `predicates` を `sql::allowlist::Parser::parse_where`／`parse_check_body` と
/// 同じ文法の正規化 SQL テキストへレンダリングする（`AND` 連結。括弧を含まない）。
/// 生成したテキストは常に
/// [`crate::sql::allowlist::parse_check_predicate_text`] で再パースできる
/// （往復一致は呼び出し元 [`validate_and_build`] が検証する）。
pub(crate) fn render_predicates(predicates: &[WherePredicate]) -> String {
    predicates
        .iter()
        .map(render_predicate)
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn render_predicate(predicate: &WherePredicate) -> String {
    match predicate {
        WherePredicate::Equality { column, value } => {
            format!("{column} = '{}'", escape_literal(value))
        }
        WherePredicate::Prefix { column, pattern } => {
            format!("{column} LIKE '{}'", escape_literal(pattern))
        }
        WherePredicate::PredicateCall { name } => format!("{name}()"),
        WherePredicate::BoolEquality { column, value } => {
            format!("{column} = {}", if *value { "true" } else { "false" })
        }
        WherePredicate::BoolColumn { column } => column.clone(),
        WherePredicate::Compare { column, op, value } => {
            format!(
                "{column} {} '{}'",
                compare_op_str(*op),
                escape_literal(value)
            )
        }
        WherePredicate::Expression(expr) => render_expression_predicate(expr),
        // SQL-24（TASK-208 ポインタ）。CHECK 制約は `reject_forbidden_elements`
        // が構築（`validate_and_build`）の時点でこれらの形を `42601` として
        // 拒否するため、本関数へは実質到達しない。網羅性のためだけに正規化
        // レンダリングを用意する（往復一致の対象にはならない）。
        WherePredicate::InList { column, values } => {
            let items = values
                .iter()
                .map(|v| format!("'{}'", escape_literal(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{column} IN ({items})")
        }
        WherePredicate::Between { column, low, high } => {
            format!(
                "{column} BETWEEN '{}' AND '{}'",
                escape_literal(low),
                escape_literal(high)
            )
        }
        WherePredicate::IsNull { column, negated } => {
            if *negated {
                format!("{column} IS NOT NULL")
            } else {
                format!("{column} IS NULL")
            }
        }
        WherePredicate::Not(inner) => format!("NOT {}", render_predicate(inner)),
        // `parse_check_clause`（構文段）が `CHECK (...)` 本体の `OR` を既に
        // `42601` で拒否するため到達しない（TASK-208・Issue #912）。網羅性のため
        // 防御的に括弧付きで描画する。
        WherePredicate::Or(branches) => {
            let rendered: Vec<String> = branches
                .iter()
                .map(|branch| {
                    branch
                        .iter()
                        .map(render_predicate)
                        .collect::<Vec<_>>()
                        .join(" AND ")
                })
                .collect();
            format!("({})", rendered.join(" OR "))
        }
        // Issue #927・SQL-29 (a)・TASK-213: `CHECK (...)` 本体も `Parser::new`
        // の既定（`subquery_ctx == None`）で解析するため、構文段が既に `42601`
        // で拒否し到達しない（`Or` と同じ防御的経路）。
        WherePredicate::InSubquery { .. } | WherePredicate::Exists { .. } => String::new(),
    }
}

/// `WherePredicate::Expression` は常に `Expr::Binary { op: <比較演算子>, lhs, rhs }`
/// （`sql::allowlist::Parser::parse_where` が構造的に保証する）。頂点の比較は
/// 括弧で囲まず、両辺の算術部分式のみ [`render_expr`] が丸括弧を付けて
/// レンダリングする。
fn render_expression_predicate(expr: &Expr) -> String {
    match expr {
        Expr::Binary { op, lhs, rhs } => {
            format!(
                "{} {} {}",
                render_expr(lhs),
                binop_str(*op),
                render_expr(rhs)
            )
        }
        // 上記の構造的保証により到達しない（式述語の頂点は必ず比較演算子の
        // `Binary`）。防御的に生の式としてレンダリングする。
        other => render_expr(other),
    }
}

/// 算術部分式のレンダリング。`Expr::Binary`（`+ - * /`）は常に丸括弧で囲み、
/// 元の木構造に依存しない一意な往復（`parse(render(x)) == x`）を保証する
/// （括弧を省略すると演算子優先順位により異なる木が同一テキストへレンダリング
/// され得るため）。
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Number(s) => s.clone(),
        // Issue #919・SQL-26: 文字列リテラルは SQL の標準的な引用符エスケープ
        // （`'` → `''`）で往復可能にレンダリングする（`parse(render(x)) == x`
        // 契約。`escape_literal` は同モジュールの既存ヘルパーを共有する）。
        Expr::String(s) => format!("'{}'", escape_literal(s)),
        Expr::Ident(name) => name.clone(),
        // codex-review（Cursor Bugbot）P1 指摘対応: `POSITION` は組み込み関数の
        // 中で唯一カンマ区切りではない SQL 標準特殊構文（`POSITION(needle IN
        // haystack)`）を持つ（`sql::allowlist::Parser::parse_function_call_expr`
        // 参照。`args` は評価順どおり `[haystack, needle]` の順で格納される一方、
        // 構文はカンマ形 `POSITION(a, b)` を明示的に `42601` で拒否する）。他の
        // 組み込み関数（`LOWER`／`UPPER`／`LENGTH`／`SUBSTR`／`CONCAT`／`TRIM`／
        // `REPLACE`）はいずれも構文段でカンマ区切りの通常の関数呼び出し形のみを
        // 受理するため下の汎用腕でそのまま往復するが、`POSITION` だけは汎用の
        // カンマ形で出力すると永続化された CHECK 定義が再パースできず
        // `CREATE TABLE` 自体が失敗する（`validate_and_build` の往復検証が
        // fail-closed に拒否するため、サイレントな破損ではなく作成失敗という
        // 形で顕在化する）。
        Expr::Call { name, args } if name.eq_ignore_ascii_case("position") => match args.as_slice()
        {
            [haystack, needle] => {
                // `name` をそのまま使う（`eq_ignore_ascii_case` で判定している
                // ため、大文字小文字は元のテキストの綴りのまま。他の組み込み
                // 関数の汎用腕〔`format!("{name}(...)")`〕と同じ「呼び出し元の
                // 綴りをそのまま往復させる」契約に揃える。ここでリテラル
                // `"POSITION"` に固定すると、元の綴りが `"position"` 等だった
                // 場合に再パース結果の `name` フィールドが一致せず往復が
                // 壊れる）。
                format!(
                    "{name}({} IN {})",
                    render_expr(needle),
                    render_expr(haystack)
                )
            }
            // 束縛済み `Expr::Call { name: "position", .. }` は構文段が常に
            // 2 引数（`haystack`／`needle`）で構築する不変条件を持つ（`parse_
            // function_call_expr` 参照）。崩れた場合でも `unreachable!` にはせず
            // （coding-rust.md「panic させない」）、下の汎用カンマ形へフォール
            // スルーする。生成テキストは呼び出し元の往復検証
            // （`validate_and_build`）が再パースの不一致として fail-closed に
            // 拒否するため、誤ったテキストが永続化されることはない。
            _ => {
                let rendered_args: Vec<String> = args.iter().map(render_expr).collect();
                format!("{name}({})", rendered_args.join(", "))
            }
        },
        Expr::Call { name, args } => {
            let rendered_args: Vec<String> = args.iter().map(render_expr).collect();
            format!("{name}({})", rendered_args.join(", "))
        }
        Expr::Binary { op, lhs, rhs } => {
            format!(
                "({} {} {})",
                render_expr(lhs),
                binop_str(*op),
                render_expr(rhs)
            )
        }
        // `CASE`／`COALESCE`／`NULLIF`（対象ビヘイビア: SQL-26。Issue #921）。
        // いずれも決定的な式のため CHECK 述語での使用を許可する
        // （`reject_forbidden_expr` 参照）。このテキストは永続化されて
        // 再パースされるため、`parse(render(x)) == x` の往復を単体テストで
        // 固定する。
        Expr::Null => "NULL".to_string(),
        Expr::Case { whens, else_result } => {
            let mut s = String::from("(CASE");
            for (cond, result) in whens {
                s.push_str(&format!(
                    " WHEN {} THEN {}",
                    render_condition(cond),
                    render_expr(result)
                ));
            }
            if let Some(else_result) = else_result {
                s.push_str(&format!(" ELSE {}", render_expr(else_result)));
            }
            s.push_str(" END)");
            s
        }
        Expr::Coalesce(args) => {
            let rendered_args: Vec<String> = args.iter().map(render_expr).collect();
            format!("COALESCE({})", rendered_args.join(", "))
        }
        Expr::NullIf(lhs, rhs) => {
            format!("NULLIF({}, {})", render_expr(lhs), render_expr(rhs))
        }
        // `DATE`／`TIMESTAMP` 型付きリテラル（対象ビヘイビア: SQL-26。
        // Issue #920）。解析済みの内部表現から正規化済みテキストを再構成する
        // （生文字列を保持しないため、区切り文字の表記揺れが往復で発生しない）。
        Expr::DateLiteral(days) => format!("DATE '{}'", crate::datetime::format_date(*days)),
        Expr::TimestampLiteral(micros) => {
            format!("TIMESTAMP '{}'", crate::datetime::format_timestamp(*micros))
        }
    }
}

/// `CASE WHEN` の条件（許可リストが常に比較の [`Expr::Binary`] に限定する）を、
/// 周囲を括弧で囲まずレンダリングする（`render_expression_predicate` の
/// トップレベル比較と同じ形。`sql::allowlist::Parser::parse_case_expr_inner` の
/// `cond` 文法〔`<value_expr> <cmp_op> <value_expr>`〕は括弧で囲まれた比較全体を
/// 受理しないため、`render_expr` の `Binary` 腕（常に括弧で囲む）をそのまま
/// 使うと往復〔`parse(render(x)) == x`〕が壊れる）。
fn render_condition(cond: &Expr) -> String {
    match cond {
        Expr::Binary { op, lhs, rhs } => {
            format!(
                "{} {} {}",
                render_expr(lhs),
                binop_str(*op),
                render_expr(rhs)
            )
        }
        other => render_expr(other),
    }
}

fn binop_str(op: udf_call::BinOp) -> &'static str {
    use udf_call::BinOp;
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Gt => ">",
        BinOp::Lt => "<",
        BinOp::Ge => ">=",
        BinOp::Le => "<=",
        BinOp::Eq => "=",
    }
}

fn compare_op_str(op: crate::sql::allowlist::CompareOp) -> &'static str {
    use crate::sql::allowlist::CompareOp;
    match op {
        CompareOp::Lt => "<",
        CompareOp::Le => "<=",
        CompareOp::Gt => ">",
        CompareOp::Ge => ">=",
    }
}

/// 文字列リテラルの `'` を `''` へエスケープする（SQL 文字列リテラルの標準的な
/// エスケープ規約。`sql::lexer` の文字列リテラル字句解析と対になる）。
fn escape_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// `predicates`（[`WherePredicate::Expression`] のみ）・任意の
/// [`WherePredicate::PredicateCall`] を走査し、CHECK が禁止する要素を検出する。
/// `visible()`（`WherePredicate::PredicateCall`。RLS 文脈に依存し `id`／`VECTOR`
/// 列のみという CHECK の参照可能範囲〔テーブル列と `id` のみ〕を破る）と、
/// 組み込み関数以外の `Expr::Call`（セッション UDF・WASM UDF・未知関数。空の
/// `UdfRegistry` で束縛すると「未知の関数」〔`22000`〕へ丸まってしまい、CHECK の
/// 禁止要素として区別できないため、束縛より前にここで `42601` として検出する）
/// をいずれも `Err`（`42601`）として返す。
fn reject_forbidden_elements(predicates: &[WherePredicate]) -> Result<(), SqlSurfaceError> {
    for predicate in predicates {
        match predicate {
            WherePredicate::PredicateCall { name } => {
                return Err(SqlSurfaceError::unsupported(format!(
                    "CHECK constraint predicate must not call {name}() (RLS-dependent predicates are not allowed)"
                )));
            }
            WherePredicate::Expression(expr) => reject_forbidden_expr(expr)?,
            // SQL-24（TASK-208 ポインタ）: `IN`／`BETWEEN`／`IS [NOT] NULL`／`NOT`
            // は CHECK 制約では未対応のまま拒否する（`enforce` の
            // 「NULL なら常に合格」という短絡が `IS NOT NULL` の意味と相容れない
            // ため。TABLE-16 の既存挙動は不変。対応は別 Issue へ申し送り）。
            WherePredicate::InList { .. }
            | WherePredicate::Between { .. }
            | WherePredicate::IsNull { .. }
            | WherePredicate::Not(_) => {
                return Err(SqlSurfaceError::unsupported(
                    "IN/BETWEEN/IS NULL/NOT are not supported in CHECK constraints",
                ));
            }
            WherePredicate::Equality { .. }
            | WherePredicate::Prefix { .. }
            | WherePredicate::BoolEquality { .. }
            | WherePredicate::BoolColumn { .. }
            | WherePredicate::Compare { .. } => {}
            // `parse_check_clause` が構文段で既に拒否するため到達しない
            // （TASK-208・Issue #912）。防御的に fail-closed で拒否する。
            WherePredicate::Or(_) => {
                return Err(SqlSurfaceError::unsupported(
                    "CHECK constraint predicate must not contain OR",
                ));
            }
            // Issue #927・SQL-29 (a)・TASK-213: 同上（構文段が既に `42601`
            // で拒否するため到達しない防御的経路）。
            WherePredicate::InSubquery { .. } | WherePredicate::Exists { .. } => {
                return Err(SqlSurfaceError::unsupported(
                    "CHECK constraint predicate must not contain a subquery",
                ));
            }
        }
    }
    Ok(())
}

fn reject_forbidden_expr(expr: &Expr) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_) | Expr::Ident(_) | Expr::String(_) => Ok(()),
        Expr::Call { name, args } => {
            if !udf_call::is_builtin_function_name(name) {
                return Err(SqlSurfaceError::unsupported(format!(
                    "CHECK constraint predicate must not call non-builtin function {name}()"
                )));
            }
            for arg in args {
                reject_forbidden_expr(arg)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. } => {
            reject_forbidden_expr(lhs)?;
            reject_forbidden_expr(rhs)
        }
        // `CASE`／`COALESCE`／`NULLIF`（対象ビヘイビア: SQL-26。Issue #921）は
        // それ自体が決定的なので許可し、子を再帰的に検査する。
        Expr::Null => Ok(()),
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                reject_forbidden_expr(cond)?;
                reject_forbidden_expr(result)?;
            }
            if let Some(else_result) = else_result {
                reject_forbidden_expr(else_result)?;
            }
            Ok(())
        }
        Expr::Coalesce(args) => {
            for a in args {
                reject_forbidden_expr(a)?;
            }
            Ok(())
        }
        Expr::NullIf(lhs, rhs) => {
            reject_forbidden_expr(lhs)?;
            reject_forbidden_expr(rhs)
        }
        // `DATE`／`TIMESTAMP` 型付きリテラル（対象ビヘイビア: SQL-26。
        // Issue #920）は定数のため許可する。
        Expr::DateLiteral(_) | Expr::TimestampLiteral(_) => Ok(()),
    }
}

/// `CHECK` 述語が依存するテーブル列名（`ALTER TABLE` の依存検査に使う。疑似列
/// `id` は含めない）を宣言順・重複なしで返す。3 つの情報源の和集合を取る
/// （漏れは制約を黙って壊す経路になるため、過剰側に倒す。fail-closed）:
///
/// 1. 束縛済み宣言的フィルタ（[`MetadataFilter`]）の参照列
/// 2. 束縛済み式フィルタのうち `VECTOR` 列を参照するもの（`vec_norm(embedding)`
///    等。組み込み関数の引数・算術式の内側を含む。[`udf_call::references_embedding`]
///    が再帰的に判定する）→ スキーマの `VECTOR` 列
/// 3. 未束縛の式述語（[`WherePredicate::Expression`]）に現れる識別子
///    （関数引数・算術式を再帰的に走査）のうち、スキーマの生存列名と一致するもの
///    （式内で参照できる列が将来拡張されても、束縛側の表現に依存せず依存列を
///    取りこぼさないための保険。ASCII 大文字小文字を無視して照合する）
fn referenced_column_names(
    schema: &TableSchema,
    predicates: &[WherePredicate],
    filters: &[MetadataFilter],
    expr_filters: &[udf_call::BoundExpr],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    };
    for filter in filters {
        if let Some(column) = schema.columns.get(filter.column_index()) {
            push(&column.name);
        }
    }
    if expr_filters.iter().any(udf_call::references_embedding) {
        if let Some(column) = schema.columns.iter().find(|c| c.ty.is_vector()) {
            push(&column.name);
        }
    }
    // Issue #919・SQL-26、Issue #1075・TABLE-16 ポインタ: 束縛済み式フィルタが
    // 参照する TEXT/DATE/TIMESTAMP/数値（INTEGER/BIGINT/REAL/DOUBLE）列も同様に
    // 依存列へ含める（`lower(label)`・`qty > 0` 等。取りこぼすと `ALTER TABLE
    // DROP COLUMN` の依存検査をすり抜け、CHECK が参照する列を削除できてしまう）。
    let mut scalar_column_mask = vec![false; schema.columns.len()];
    for expr in expr_filters {
        udf_call::mark_referenced_scalar_columns(expr, &mut scalar_column_mask);
    }
    for (index, wanted) in scalar_column_mask.iter().enumerate() {
        if *wanted {
            if let Some(column) = schema.columns.get(index) {
                push(&column.name);
            }
        }
    }
    fn collect_idents<'a>(expr: &'a Expr, acc: &mut Vec<&'a str>) {
        match expr {
            Expr::Number(_) | Expr::String(_) => {}
            Expr::Ident(name) => acc.push(name.as_str()),
            Expr::Call { args, .. } => {
                for arg in args {
                    collect_idents(arg, acc);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                collect_idents(lhs, acc);
                collect_idents(rhs, acc);
            }
            Expr::Null => {}
            Expr::Case { whens, else_result } => {
                for (cond, result) in whens {
                    collect_idents(cond, acc);
                    collect_idents(result, acc);
                }
                if let Some(else_result) = else_result {
                    collect_idents(else_result, acc);
                }
            }
            Expr::Coalesce(args) => {
                for a in args {
                    collect_idents(a, acc);
                }
            }
            Expr::NullIf(lhs, rhs) => {
                collect_idents(lhs, acc);
                collect_idents(rhs, acc);
            }
            Expr::DateLiteral(_) | Expr::TimestampLiteral(_) => {}
        }
    }
    let mut idents: Vec<&str> = Vec::new();
    for predicate in predicates {
        if let WherePredicate::Expression(expr) = predicate {
            collect_idents(expr, &mut idents);
        }
    }
    for ident in idents {
        if let Some(column) = schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(ident))
        {
            push(&column.name);
        }
    }
    out
}

/// 永続化済み `CHECK` の述語テキストを再パース・再束縛し、依存列を再計算する
/// （[`referenced_column_names`] と同じ規則）。`catalog::Storage` の
/// `ALTER TABLE` 依存検査が、カタログに記録された `CheckConstraint::columns` と
/// 併用する（記録が欠けていても依存列を取りこぼさないための多層防御）。
/// 再計算自体の失敗は呼び出し元が「依存あり」として扱う（fail-closed）。
pub(crate) fn recompute_referenced_columns(
    schema: &TableSchema,
    check: &CheckConstraint,
) -> Result<Vec<String>, SqlSurfaceError> {
    let predicates = crate::sql::allowlist::parse_check_predicate_text(&check.predicate_sql)?;
    reject_forbidden_elements(&predicates)?;
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata_filters, expr_filters, _rls_predicate_present, _or_filters) =
        crate::sql::parser::bind_check_predicates(&predicates, schema, &mut node_budget)?;
    Ok(referenced_column_names(
        schema,
        &predicates,
        &metadata_filters,
        &expr_filters,
    ))
}

/// `table` と列 `column`（`Some` のとき列制約・`None` のとき表制約）から
/// PostgreSQL 風の既定制約名を導出する（`<table>_<col>_check`／`<table>_check`）。
fn default_check_name(table: &str, column: Option<&str>) -> String {
    match column {
        Some(col) => format!("{table}_{col}_check"),
        None => format!("{table}_check"),
    }
}

/// `candidate` が識別子として妥当（`crate::catalog::validate_identifier`）かつ
/// `used` に未登録なら採用し、そうでなければ `_2`・`_3`... の接尾辞を試す
/// （PostgreSQL の暗黙制約名衝突解決に倣う）。識別子長超過でどの接尾辞候補も
/// 妥当にならない場合は `check<N>`（`N` は `used.len()` 起点の連番）へ
/// フォールバックする（設計 D1 参照）。
fn resolve_unique_name(candidate: &str, used: &[String]) -> String {
    if crate::catalog::validate_identifier(candidate).is_ok()
        && !used.contains(&candidate.to_string())
    {
        return candidate.to_string();
    }
    for suffix in 2..=used.len() + 2 {
        let attempt = format!("{candidate}_{suffix}");
        if crate::catalog::validate_identifier(&attempt).is_ok() && !used.contains(&attempt) {
            return attempt;
        }
    }
    let mut fallback_index = used.len();
    loop {
        let attempt = format!("check{fallback_index}");
        if !used.contains(&attempt) {
            return attempt;
        }
        fallback_index += 1;
    }
}

/// `CREATE TABLE` の構文段が組み立てた [`ParsedCheck`] 一覧を意味論検証し、
/// カタログへ永続化する [`CheckConstraint`] 一覧へ変換する（TABLE-16・
/// TASK-204、Issue #906。`sql::ddl::execute_create_table` から呼ばれる唯一の
/// 呼び出し元）。
///
/// 手順（宣言順に処理）: (1) 禁止要素の検出（`42601`） (2) `bind_where_predicates`
/// による意味論検証（列の存在・型・非有限値等。既存の `WHERE` と同一の
/// `wire_code`） (3) 参照列名の抽出 (4) 制約名の確定（明示 `CONSTRAINT` 名は
/// 一意性を検証、省略時は自動生成し衝突を接尾辞で解決） (5) 正規化レンダリング
/// と往復一致検証（`42601`）。
///
/// `schema` は列定義のみを持つ（`checks` が空の）[`TableSchema`] を渡すこと
/// （呼び出し元は `CHECK` 検証**前**の列定義だけで束縛する。TABLE-16 は他の
/// `CHECK` を参照する `CHECK` を許可しない）。
pub(crate) fn validate_and_build(
    schema: &TableSchema,
    parsed: &[ParsedCheck],
) -> Result<Vec<CheckConstraint>, SqlSurfaceError> {
    if parsed.len() > crate::catalog::MAX_CHECK_CONSTRAINTS_PER_TABLE {
        return Err(SqlSurfaceError::payload_too_large(
            "too many CHECK constraints in CREATE TABLE",
        ));
    }

    // 明示 `CONSTRAINT` 名は宣言位置に関わらず先に全件を確定する（自動生成名の
    // 接尾辞解決より前）。明示名同士の重複は常にユーザー入力の誤りとして `42601`
    // で拒否し、自動生成名は明示名を**すべて**避けて決める——宣言順（自動命名の
    // 制約と明示名の制約のどちらが先に現れるか）によらず、同じ入力集合からは
    // 常に同じ結果（受理なら同じ名前集合、重複なら同じエラー）になる。
    let mut explicit_names: Vec<&str> = Vec::new();
    for check in parsed {
        if let Some(name) = &check.name {
            crate::catalog::validate_identifier(name).map_err(|e| {
                SqlSurfaceError::unsupported(format!("invalid constraint name: {e}"))
            })?;
            if explicit_names.contains(&name.as_str()) {
                return Err(SqlSurfaceError::unsupported(format!(
                    "duplicate CHECK constraint name: {name}"
                )));
            }
            explicit_names.push(name.as_str());
        }
    }

    let mut used_names: Vec<String> = Vec::with_capacity(parsed.len());
    used_names.extend(explicit_names.iter().map(|n| n.to_string()));
    let mut built = Vec::with_capacity(parsed.len());
    for check in parsed {
        reject_forbidden_elements(&check.predicates)?;

        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let (metadata_filters, expr_filters, _rls_predicate_present, _or_filters) =
            crate::sql::parser::bind_check_predicates(&check.predicates, schema, &mut node_budget)?;

        let columns =
            referenced_column_names(schema, &check.predicates, &metadata_filters, &expr_filters);
        if columns.len() > crate::catalog::MAX_CHECK_REFERENCED_COLUMNS {
            return Err(SqlSurfaceError::payload_too_large(
                "CHECK constraint references too many columns",
            ));
        }

        let name = match &check.name {
            // 明示名は `used_names` へ登録済み（上の事前確定）。
            Some(explicit) => explicit.clone(),
            None => {
                let candidate = default_check_name(&schema.name, check.column.as_deref());
                let resolved = resolve_unique_name(&candidate, &used_names);
                used_names.push(resolved.clone());
                resolved
            }
        };

        let predicate_sql = render_predicates(&check.predicates);
        if predicate_sql.len() > crate::catalog::MAX_CHECK_PREDICATE_SQL_LEN {
            return Err(SqlSurfaceError::payload_too_large(
                "CHECK constraint predicate exceeds the size limit",
            ));
        }
        // 往復一致検証（設計 D2）: レンダリングしたテキストを再パースした結果が
        // 元の述語列とビット一致することを確認する。キーワードと衝突する識別子等、
        // 往復できない入力を永続化しない安全弁（通常は到達しない——`render_expr`/
        // `render_predicate` は `sql::allowlist::Parser` が受理する文法のみを
        // 生成するよう構築済み）。
        let reparsed = crate::sql::allowlist::parse_check_predicate_text(&predicate_sql)?;
        if reparsed != check.predicates {
            return Err(SqlSurfaceError::unsupported(
                "CHECK constraint predicate does not round-trip through normalization",
            ));
        }

        built.push(CheckConstraint {
            name,
            columns,
            predicate_sql,
        });
    }
    Ok(built)
}

/// 書き込み文ごとに 1 回コンパイルする `CHECK` 制約群
/// （`constraint::enforce_row_constraints_in_txn` が同一 write トランザクション内で
/// 呼ぶ）。`schema.checks()` が空なら [`CompiledChecks::compile`] は `None` を
/// 返し（CHECK の無いテーブルはオーバーヘッドゼロ）、呼び出し元は `enforce` を
/// 呼ばずに書き込みを続けてよい。
pub(crate) struct CompiledChecks {
    checks: Vec<CompiledCheck>,
    /// 全 `CHECK` の宣言的フィルタが参照する列インデックスの和集合
    /// （`row_codec::scan_scalar_columns_masked` への `mask` として使う。
    /// Issue #350 の必要列限定デコードと同じ考え方で、CHECK が参照しない列は
    /// デコードしない）。
    column_mask: Vec<bool>,
}

struct CompiledCheck {
    name: String,
    conjuncts: Vec<CompiledConjunct>,
}

enum CompiledConjunct {
    Declarative(MetadataFilter),
    Expr { program: ExprProgram },
}

impl CompiledChecks {
    /// `schema.checks()` をすべて再束縛・コンパイルする。永続化済みの `CHECK`
    /// の再束縛が失敗した場合（カタログの手書き改変・実装不整合による漂流）は
    /// `TenantWriteError::Catalog(CorruptSchema)`（`XX000`）で書き込みを拒否する
    /// （スキップしない。fail-closed。`.claude/rules/security.md`）。
    pub(crate) fn compile(
        schema: &TableSchema,
    ) -> Result<Option<CompiledChecks>, TenantWriteError> {
        let raw_checks = schema.checks();
        if raw_checks.is_empty() {
            return Ok(None);
        }
        let mut column_mask = vec![false; schema.columns.len()];
        let mut compiled = Vec::with_capacity(raw_checks.len());
        for check in raw_checks {
            let predicates =
                crate::sql::allowlist::parse_check_predicate_text(&check.predicate_sql)
                    .map_err(corrupt_check)?;
            reject_forbidden_elements(&predicates).map_err(corrupt_check)?;
            let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
            let (metadata_filters, expr_filters, _rls_predicate_present, _or_filters) =
                crate::sql::parser::bind_check_predicates(&predicates, schema, &mut node_budget)
                    .map_err(corrupt_check)?;

            // 依存列の記録（`CheckConstraint::columns`）が再計算結果を覆っていること
            // を検査する。欠けていれば `ALTER TABLE` の依存検査が素通りし得た
            // 破損・漂流であり、書き込みを fail-closed に拒否する。
            let recomputed =
                referenced_column_names(schema, &predicates, &metadata_filters, &expr_filters);
            if let Some(missing) = recomputed.iter().find(|c| !check.columns.contains(c)) {
                return Err(corrupt_check(SqlSurfaceError::unsupported(format!(
                    "CHECK constraint {:?} does not record dependent column {missing:?}",
                    check.name
                ))));
            }

            let mut conjuncts = Vec::with_capacity(metadata_filters.len() + expr_filters.len());
            for filter in metadata_filters {
                if let Some(slot) = column_mask.get_mut(filter.column_index()) {
                    *slot = true;
                }
                conjuncts.push(CompiledConjunct::Declarative(filter));
            }
            for expr in &expr_filters {
                // Issue #919・SQL-26: 式述語が参照する `TEXT` 列も
                // `column_mask` へ反映する（`enforce` の `scan_scalar_columns_masked`
                // 呼び出しがこのマスクを使ってデコードするため、反映漏れは
                // マスク外参照＝実 NULL との取り違えという fail-closed 判定に
                // 落ちる）。
                udf_call::mark_referenced_scalar_columns(expr, &mut column_mask);
                conjuncts.push(CompiledConjunct::Expr {
                    program: ExprProgram::compile(expr),
                });
            }

            compiled.push(CompiledCheck {
                name: check.name.clone(),
                conjuncts,
            });
        }
        Ok(Some(CompiledChecks {
            checks: compiled,
            column_mask,
        }))
    }

    /// `id`・`embedding`・`metadata`（`row_codec::encode_scalar_columns` が
    /// 書いた正規レイアウト）を持つ 1 行を全 `CHECK` に対して検査する。
    /// 三値論理（SQL 標準）: 連言（宣言的フィルタ・式のいずれも）の参照列・
    /// 参照ベクトルが NULL（`scanned` が `None`、または `VECTOR` 列参照時に
    /// `dim == 0`）なら UNKNOWN としてスキップし、非 NULL で不一致
    /// （`matches() == false` または式評価が `Bool(false)`）のときのみ違反と
    /// する。AND 連結のため「いずれかの連言が FALSE なら違反」と同値。
    ///
    /// 呼び出し元（`constraint::enforce_row_constraints_in_txn`）は台帳照合
    /// （`ledger::record_in_txn`）・行の書き込みの**後**、テーブル世代 bump・commit
    /// の**前**に、書き込んだ行を読み戻して呼ぶ契約（違反時は `write_txn` を
    /// commit しないため、行・台帳とも痕跡を残さない）。
    pub(crate) fn enforce(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Result<(), TenantWriteError> {
        // 参照列を 1 回だけ borrowed デコードする（Issue #350 と同じ必要列限定
        // デコード。`scan_scalar_columns_masked` は presence タグ・宣言長上限・
        // UTF-8 妥当性の構造検証をマスク外の列でも一切弱めない契約を維持する）。
        let scanned =
            crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&self.column_mask))
                .map_err(|e| {
                    TenantWriteError::Catalog(crate::catalog::CatalogError::Invalid(e.to_string()))
                })?;
        let mut expr_scratch: Vec<StackValue> = Vec::new();
        // Issue #919・SQL-26: `column_mask` は `TEXT` 参照を反映済み
        // （`CompiledChecks::compile` 参照）。
        for check in &self.checks {
            for conjunct in &check.conjuncts {
                let satisfied_or_unknown = match conjunct {
                    CompiledConjunct::Declarative(filter) => {
                        match scanned.get(filter.column_index()).copied().flatten() {
                            // 参照列が NULL: UNKNOWN としてスキップ（違反にしない）。
                            None => true,
                            Some(value) => filter.matches(Some(value)),
                        }
                    }
                    CompiledConjunct::Expr { program } => {
                        // `VECTOR` 列が NULL（`dim == 0`）の行を式が実際に参照する
                        // 場合の NULL（UNKNOWN）伝播は `program.eval` 自身
                        // （`ExprStep::PushVector` の空スライス判定。
                        // `sql::expr_program` 参照）が行う（codex-review P1 指摘
                        // 対応: 静的な式木走査（`references_embedding`）による
                        // 事前判定は `CASE` の選ばれない分岐に embedding 参照が
                        // あるだけの行まで誤って UNKNOWN 扱いにしていたため撤去
                        // し、評価時点の判定へ一本化した）。
                        match program.eval(id, embedding, &scanned, &mut expr_scratch) {
                            Ok(ExprValue::Bool(b)) => b,
                            // UNKNOWN（NULL）は充足扱いにする（Issue #919・
                            // SQL-26（AC2）と Issue #921・SQL-26 の共有契約。
                            // PostgreSQL 互換。上の `Declarative` 腕「参照列
                            // NULL は違反にしない」と同じ意図的判断——NULL を
                            // 返す式を書けるのは DDL 権限を持つ主体のみのため
                            // 制約の迂回にはならない）。
                            Ok(ExprValue::Null) => true,
                            Ok(_) => {
                                // 束縛段（`bind_where_predicates`）が式述語の
                                // 型を Bool に限定済みのため到達しない。
                                return Err(TenantWriteError::CheckEvaluationFailed);
                            }
                            Err(_) => return Err(TenantWriteError::CheckEvaluationFailed),
                        }
                    }
                };
                if !satisfied_or_unknown {
                    return Err(TenantWriteError::CheckViolation {
                        constraint: check.name.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// 永続化済み `CHECK` の再束縛失敗（カタログ改変・実装不整合による漂流）を
/// `TenantWriteError`（`XX000`）へ写像する（fail-closed。詳細はクライアントへ
/// 渡さない）。
fn corrupt_check(e: SqlSurfaceError) -> TenantWriteError {
    TenantWriteError::Catalog(crate::catalog::CatalogError::CorruptSchema(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType};
    use crate::row_codec::Value;

    /// `CREATE TABLE` 文字列から `ValidatedCreateTable`（`checks` を含む）を得る
    /// テスト用ヘルパー。構文段（`sql::allowlist`）の受理を前提にする。
    fn parse_create_table(sql: &str) -> crate::sql::allowlist::ValidatedCreateTable {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        crate::sql::allowlist::validate_create_table_tokens(&tokens)
            .unwrap_or_else(|e| panic!("expected ok, got {e:?}"))
    }

    fn schema_of(validated: &crate::sql::allowlist::ValidatedCreateTable) -> TableSchema {
        TableSchema::new(validated.table_name.clone(), validated.columns.clone())
    }

    #[test]
    fn validate_and_build_generates_default_name_for_column_level_check() {
        let v = parse_create_table("CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))");
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "docs_kind_check");
        assert_eq!(checks[0].columns, vec!["kind".to_string()]);
        assert_eq!(checks[0].predicate_sql, "kind = 'a'");
    }

    /// 対象ビヘイビア: SQL-26（Issue #920）。`DATE`／`TIMESTAMP` 型付きリテラルを
    /// 含む CHECK 述語が `render_expr` → 永続化 → 再パースの往復
    /// （`parse(render(x)) == x`）で同じ判定を保つことを固定する（§2-8）。
    /// `CREATE TABLE` の SQL DDL は `DATE`／`TIMESTAMP` 列型を受理しない
    /// （TABLE-13・TASK-197 の列宣言は Rust API 経由に限る。別 Issue の対象）
    /// ため、ここでは列参照を持たない定数のみの述語で往復を検証する。
    #[test]
    fn check_constraint_with_datetime_literals_round_trips() {
        let v = parse_create_table(
            "CREATE TABLE docs (kind TEXT CHECK (DATE '2020-01-01' <= DATE '2024-01-01'))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(
            checks[0].predicate_sql,
            "DATE '2020-01-01' <= DATE '2024-01-01'"
        );

        let v2 = parse_create_table(&format!(
            "CREATE TABLE docs (kind TEXT CHECK ({}))",
            checks[0].predicate_sql
        ));
        let schema2 = schema_of(&v2);
        let checks2 = validate_and_build(&schema2, &v2.checks).expect("must re-validate");
        assert_eq!(checks[0].predicate_sql, checks2[0].predicate_sql);

        let v3 = parse_create_table(
            "CREATE TABLE docs (kind TEXT CHECK (TIMESTAMP '2020-01-01 12:00:00.5' > TIMESTAMP '2019-01-01 00:00:00'))",
        );
        let schema3 = schema_of(&v3);
        let checks3 = validate_and_build(&schema3, &v3.checks).expect("must validate");
        assert_eq!(
            checks3[0].predicate_sql,
            "TIMESTAMP '2020-01-01 12:00:00.5' > TIMESTAMP '2019-01-01 00:00:00'"
        );
    }

    #[test]
    fn validate_and_build_generates_default_name_for_table_level_check() {
        let v = parse_create_table("CREATE TABLE docs (kind TEXT, CHECK (kind = 'a'))");
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks[0].name, "docs_check");
    }

    #[test]
    fn validate_and_build_uses_explicit_constraint_name() {
        let v = parse_create_table(
            "CREATE TABLE docs (kind TEXT CONSTRAINT kind_ck CHECK (kind = 'a'))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks[0].name, "kind_ck");
    }

    /// SQL-24（TASK-208 ポインタ）: `IN`／`BETWEEN`／`IS [NOT] NULL`／`NOT` は
    /// 構文段（`sql::allowlist`）では受理されるが、CHECK 制約の構築時に
    /// `reject_forbidden_elements` が `42601` で拒否する（`enforce` の
    /// 「NULL なら常に合格」という短絡が `IS NOT NULL` の意味と相容れない
    /// ため。TABLE-16 の既存挙動は不変のまま）。
    #[test]
    fn validate_and_build_rejects_new_sql24_predicate_forms() {
        for sql in [
            "CREATE TABLE docs (kind TEXT CHECK (kind IN ('a', 'b')))",
            "CREATE TABLE docs (kind TEXT CHECK (kind NOT IN ('a', 'b')))",
            "CREATE TABLE docs (kind TEXT CHECK (kind BETWEEN 'a' AND 'z'))",
            "CREATE TABLE docs (kind TEXT CHECK (kind IS NULL))",
            "CREATE TABLE docs (kind TEXT CHECK (kind IS NOT NULL))",
            "CREATE TABLE docs (kind TEXT CHECK (NOT kind = 'a'))",
        ] {
            let v = parse_create_table(sql);
            let schema = schema_of(&v);
            let err = validate_and_build(&schema, &v.checks)
                .expect_err(&format!("{sql:?} must be rejected"));
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
    }

    #[test]
    fn validate_and_build_resolves_generated_name_collision_with_suffix() {
        // 2 つの列制約がいずれも自動生成名 `docs_check`（表制約の既定名）と
        // 衝突しないケース: 列制約の既定名は列名を含むため衝突しないが、
        // 複数の表制約（列指定なし）は同じ既定名候補になり、接尾辞で解決される。
        let v = parse_create_table(
            "CREATE TABLE docs (kind TEXT, status TEXT, CHECK (kind = 'a'), CHECK (status = 'b'))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "docs_check");
        assert_eq!(checks[1].name, "docs_check_2");
    }

    #[test]
    fn validate_and_build_rejects_duplicate_explicit_names() {
        let v = parse_create_table(
            "CREATE TABLE docs (kind TEXT CONSTRAINT ck CHECK (kind = 'a'), status TEXT CONSTRAINT ck CHECK (status = 'b'))",
        );
        let schema = schema_of(&v);
        let err = validate_and_build(&schema, &v.checks).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
    }

    /// 回帰（PR レビュー指摘 Low）: 自動命名の制約と、その自動生成名と同名の
    /// 明示 `CONSTRAINT` の宣言順を入れ替えても、結果（明示名はそのまま・
    /// 自動生成名は明示名を避けて接尾辞付き）が一致する。
    #[test]
    fn validate_and_build_naming_is_independent_of_declaration_order() {
        let auto_first = parse_create_table(
            "CREATE TABLE docs (kind TEXT, CHECK (kind = 'a'), CONSTRAINT docs_check CHECK (kind LIKE 'a%'))",
        );
        let explicit_first = parse_create_table(
            "CREATE TABLE docs (kind TEXT, CONSTRAINT docs_check CHECK (kind LIKE 'a%'), CHECK (kind = 'a'))",
        );
        let a = validate_and_build(&schema_of(&auto_first), &auto_first.checks)
            .expect("auto-first must validate");
        let b = validate_and_build(&schema_of(&explicit_first), &explicit_first.checks)
            .expect("explicit-first must validate");
        let names = |checks: &[CheckConstraint]| {
            let mut pairs: Vec<(String, String)> = checks
                .iter()
                .map(|c| (c.predicate_sql.clone(), c.name.clone()))
                .collect();
            pairs.sort();
            pairs
        };
        assert_eq!(names(&a), names(&b));
        assert_eq!(
            names(&a),
            vec![
                ("kind = 'a'".to_string(), "docs_check_2".to_string()),
                ("kind LIKE 'a%'".to_string(), "docs_check".to_string()),
            ]
        );
        // 永続化まで通す（`catalog::validate_schema` の名前一意性検査も通過する）。
        let schema = schema_of(&auto_first).with_checks(a);
        assert!(CompiledChecks::compile(&schema).expect("compile").is_some());
    }

    /// 明示名同士の重複は、自動命名の制約がどこに挟まっていても常に同じ
    /// `42601` になる（宣言順に依存しない）。
    #[test]
    fn validate_and_build_rejects_duplicate_explicit_names_regardless_of_position() {
        for sql in [
            "CREATE TABLE docs (kind TEXT, CONSTRAINT ck CHECK (kind = 'a'), CHECK (kind = 'b'), CONSTRAINT ck CHECK (kind = 'c'))",
            "CREATE TABLE docs (kind TEXT, CHECK (kind = 'b'), CONSTRAINT ck CHECK (kind = 'a'), CONSTRAINT ck CHECK (kind = 'c'))",
            "CREATE TABLE docs (kind TEXT, CONSTRAINT docs_check CHECK (kind = 'a'), CHECK (kind = 'b'), CONSTRAINT docs_check CHECK (kind = 'c'))",
            "CREATE TABLE docs (kind TEXT, CHECK (kind = 'b'), CONSTRAINT docs_check CHECK (kind = 'a'), CONSTRAINT docs_check CHECK (kind = 'c'))",
        ] {
            let v = parse_create_table(sql);
            let err = validate_and_build(&schema_of(&v), &v.checks).expect_err(sql);
            assert_eq!(err.wire_code(), "42601", "{sql}");
            assert!(err.to_string().contains("duplicate CHECK constraint name"), "{sql}");
        }
    }

    #[test]
    fn validate_and_build_rejects_visible_predicate() {
        let v = parse_create_table("CREATE TABLE docs (body TEXT, CHECK (visible()))");
        let schema = schema_of(&v);
        let err = validate_and_build(&schema, &v.checks).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn validate_and_build_rejects_unknown_column() {
        let v = parse_create_table("CREATE TABLE docs (body TEXT, CHECK (missing = 'a'))");
        let schema = schema_of(&v);
        let err = validate_and_build(&schema, &v.checks).expect_err("must reject");
        // 未知列は既存の WHERE 束縛エラー（`22000`）のまま。
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn validate_and_build_accepts_vector_builtin_expression() {
        let v = parse_create_table(
            "CREATE TABLE docs (embedding VECTOR(3), CHECK (vec_norm(embedding) < 100))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        // 回帰（PR #1055 codex P1）: 式述語が参照する `VECTOR` 列も依存列として
        // 記録する（旧実装は宣言的フィルタの列しか集めず空だった）。
        assert_eq!(checks[0].columns, vec!["embedding".to_string()]);
        assert_eq!(checks[0].predicate_sql, "vec_norm(embedding) < 100");
    }

    /// 式の内側（組み込み関数の引数・算術式）に現れる列参照も依存列として
    /// 収集し、宣言的フィルタの列と重複なく併合する。`id` は疑似列のため含めない。
    #[test]
    fn validate_and_build_collects_columns_referenced_inside_expressions() {
        let v = parse_create_table(
            "CREATE TABLE docs (embedding VECTOR(3), kind TEXT, \
             CHECK (kind = 'a' AND (vec_sum(embedding) + id) * 2 < 100 AND vec_norm(embedding) > 0))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(
            checks[0].columns,
            vec!["kind".to_string(), "embedding".to_string()]
        );
        // 再計算（`ALTER TABLE` の依存検査が併用する）も同じ結果になる。
        assert_eq!(
            recompute_referenced_columns(&schema, &checks[0]).expect("recompute"),
            checks[0].columns
        );
    }

    /// 記録された依存列が再計算結果を覆っていない（破損・漂流した）`CHECK` は、
    /// 書き込み時のコンパイルで fail-closed に拒否する。
    #[test]
    fn compiled_checks_reject_check_missing_recorded_dependency() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("kind", ColumnType::Text, true),
            ],
        );
        for (columns, predicate) in [
            (vec![], "vec_norm(embedding) < 100"),
            (vec![], "kind = 'a'"),
            (
                vec!["kind".to_string()],
                "kind = 'a' AND vec_norm(embedding) < 100",
            ),
        ] {
            let schema = schema.clone().with_checks(vec![CheckConstraint {
                name: "c".to_string(),
                columns,
                predicate_sql: predicate.to_string(),
            }]);
            assert!(
                matches!(
                    CompiledChecks::compile(&schema),
                    Err(TenantWriteError::Catalog(
                        crate::catalog::CatalogError::CorruptSchema(_)
                    ))
                ),
                "{predicate}"
            );
        }
    }

    #[test]
    fn compiled_checks_compile_returns_none_when_no_checks_declared() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        assert!(CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .is_none());
    }

    #[test]
    fn compiled_checks_enforce_passes_and_violates_correctly() {
        let v = parse_create_table("CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))");
        let checks = validate_and_build(&schema_of(&v), &v.checks).expect("must validate");
        let schema = schema_of(&v).with_checks(checks);
        let compiled = CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .expect("checks must be present");

        let metadata_ok =
            crate::row_codec::encode_scalar_columns(&schema, &[Value::Text("a".to_string())])
                .expect("encode");
        assert!(compiled.enforce(&schema, 1, &[], &metadata_ok).is_ok());

        let metadata_violation =
            crate::row_codec::encode_scalar_columns(&schema, &[Value::Text("b".to_string())])
                .expect("encode");
        let err = compiled
            .enforce(&schema, 1, &[], &metadata_violation)
            .expect_err("must violate");
        assert!(matches!(
            err,
            TenantWriteError::CheckViolation { constraint } if constraint == "docs_kind_check"
        ));
    }

    #[test]
    fn compiled_checks_enforce_treats_null_column_as_unknown_not_violation() {
        // 三値論理: 参照列が NULL のときは違反にしない（設計 D1）。
        let v = parse_create_table("CREATE TABLE docs (kind TEXT CHECK (kind = 'a'))");
        let checks = validate_and_build(&schema_of(&v), &v.checks).expect("must validate");
        let schema = schema_of(&v).with_checks(checks);
        let compiled = CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .expect("checks must be present");

        let metadata_null =
            crate::row_codec::encode_scalar_columns(&schema, &[Value::Null]).expect("encode");
        assert!(compiled.enforce(&schema, 1, &[], &metadata_null).is_ok());
    }

    #[test]
    fn compiled_checks_enforce_vector_builtin_check() {
        let v = parse_create_table(
            "CREATE TABLE docs (embedding VECTOR(3), CHECK (vec_norm(embedding) < 10))",
        );
        let checks = validate_and_build(&schema_of(&v), &v.checks).expect("must validate");
        let schema = schema_of(&v).with_checks(checks);
        let compiled = CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .expect("checks must be present");

        let metadata =
            crate::row_codec::encode_scalar_columns(&schema, &[Value::Vector(vec![0.0, 0.0, 0.0])])
                .expect("encode");
        assert!(compiled
            .enforce(&schema, 1, &[1.0, 0.0, 0.0], &metadata)
            .is_ok());
        let err = compiled
            .enforce(&schema, 1, &[100.0, 0.0, 0.0], &metadata)
            .expect_err("norm 100 must violate");
        assert!(matches!(err, TenantWriteError::CheckViolation { .. }));
    }

    /// 回帰ガード（設計 D-3）: 式述語が参照する `INTEGER` 列を `column_mask` へ
    /// OR し忘れると、`scan_scalar_columns_masked` はマスク外＝NULL として扱い
    /// `enforce` の NULL スキップ（UNKNOWN＝合格）で違反が黙って通過する
    /// fail-open になる。違反値で確実に `CheckViolation` になることを固定する。
    #[test]
    fn compiled_checks_enforce_integer_column_compare() {
        let v = parse_create_table("CREATE TABLE docs (qty INTEGER CHECK (qty > 0))");
        let checks = validate_and_build(&schema_of(&v), &v.checks).expect("must validate");
        assert_eq!(checks[0].columns, vec!["qty".to_string()]);
        let schema = schema_of(&v).with_checks(checks);
        let compiled = CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .expect("checks must be present");

        let violating = crate::row_codec::encode_scalar_columns(&schema, &[Value::Integer(-1)])
            .expect("encode");
        let err = compiled
            .enforce(&schema, 1, &[], &violating)
            .expect_err("qty=-1 must violate");
        assert!(matches!(
            err,
            TenantWriteError::CheckViolation { constraint } if constraint == "docs_qty_check"
        ));

        let ok =
            crate::row_codec::encode_scalar_columns(&schema, &[Value::Integer(1)]).expect("encode");
        assert!(compiled.enforce(&schema, 1, &[], &ok).is_ok());
    }

    /// 複数の数値列（INTEGER・BIGINT）を跨ぐ算術・比較式の往復一致
    /// （`parse(render(x)) == x`）と依存列記録（両列とも含む）を固定する。
    #[test]
    fn validate_and_build_round_trips_multi_column_numeric_arithmetic_predicate() {
        let v = parse_create_table(
            "CREATE TABLE docs (qty INTEGER, lim BIGINT, CHECK ((qty * 2) >= (lim + 1)))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks[0].predicate_sql, "(qty * 2) >= (lim + 1)");
        assert_eq!(
            checks[0].columns,
            vec!["qty".to_string(), "lim".to_string()]
        );

        let v2 = parse_create_table(&format!(
            "CREATE TABLE docs (qty INTEGER, lim BIGINT, CHECK ({}))",
            checks[0].predicate_sql
        ));
        let checks2 = validate_and_build(&schema_of(&v2), &v2.checks).expect("must re-validate");
        assert_eq!(checks[0].predicate_sql, checks2[0].predicate_sql);
    }

    /// `BIGINT` 列の値が `2^53` を超える場合、`f64` へ黙って丸めず `XX000`
    /// （`CheckEvaluationFailed`）で fail-closed に拒否する（設計 D-2・D-4）。
    #[test]
    fn compiled_checks_enforce_bigint_value_exceeding_exact_range_fails_closed() {
        let v = parse_create_table("CREATE TABLE docs (n BIGINT CHECK (n > 0))");
        let checks = validate_and_build(&schema_of(&v), &v.checks).expect("must validate");
        let schema = schema_of(&v).with_checks(checks);
        let compiled = CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .expect("checks must be present");

        let too_big = (1i64 << 53) + 1;
        let metadata = crate::row_codec::encode_scalar_columns(&schema, &[Value::BigInt(too_big)])
            .expect("encode");
        let err = compiled
            .enforce(&schema, 1, &[], &metadata)
            .expect_err("BIGINT exceeding 2^53 must fail closed");
        assert!(matches!(err, TenantWriteError::CheckEvaluationFailed));
    }

    /// 0 除算は違反（`23514`）にも通過にも丸めず `XX000` で fail-closed に
    /// 拒否する（設計 D-4）。
    #[test]
    fn compiled_checks_enforce_division_by_zero_fails_closed() {
        let v = parse_create_table("CREATE TABLE docs (qty INTEGER CHECK (100 / qty > 1))");
        let checks = validate_and_build(&schema_of(&v), &v.checks).expect("must validate");
        let schema = schema_of(&v).with_checks(checks);
        let compiled = CompiledChecks::compile(&schema)
            .expect("compile must succeed")
            .expect("checks must be present");

        let metadata =
            crate::row_codec::encode_scalar_columns(&schema, &[Value::Integer(0)]).expect("encode");
        let err = compiled
            .enforce(&schema, 1, &[], &metadata)
            .expect_err("division by zero must fail closed");
        assert!(matches!(err, TenantWriteError::CheckEvaluationFailed));
    }

    /// `REAL`／`DOUBLE` 列比較（対象外事項: `CREATE TABLE` の SQL DDL からは
    /// 宣言できないため、`TableSchema`／`CheckConstraint` を直接構築して
    /// `CompiledChecks::compile` の再束縛経路を固定する）。
    #[test]
    fn compiled_checks_enforce_real_and_double_column_compare() {
        for (ty, ok_value, bad_value) in [
            (ColumnType::Real, Value::Real(1.0), Value::Real(-1.0)),
            (ColumnType::Double, Value::Double(1.0), Value::Double(-1.0)),
        ] {
            let schema = TableSchema::new("docs", vec![ColumnDef::new("qty", ty, true)])
                .with_checks(vec![CheckConstraint {
                    name: "c".to_string(),
                    columns: vec!["qty".to_string()],
                    predicate_sql: "qty > 0".to_string(),
                }]);
            let compiled = CompiledChecks::compile(&schema)
                .expect("compile must succeed")
                .expect("checks must be present");

            let ok = crate::row_codec::encode_scalar_columns(&schema, &[ok_value]).expect("encode");
            assert!(compiled.enforce(&schema, 1, &[], &ok).is_ok());

            let bad =
                crate::row_codec::encode_scalar_columns(&schema, &[bad_value]).expect("encode");
            assert!(matches!(
                compiled.enforce(&schema, 1, &[], &bad),
                Err(TenantWriteError::CheckViolation { .. })
            ));
        }
    }

    /// 数値列（INTEGER/BIGINT/REAL/DOUBLE）以外の式内参照は、`CHECK` 専用
    /// ポリシーが追加された後も引き続き拒否される（TEXT/BOOLEAN/DATE/NUMERIC
    /// 等はポリシーに関わらず対象外のまま。設計 D-1・D-2）。
    #[test]
    fn validate_and_build_rejects_non_numeric_column_types_in_expressions() {
        // `bind_check_predicates`（`ColumnRefPolicy::AllowNumericColumns`）でも
        // 数値列（INTEGER/BIGINT/REAL/DOUBLE）以外の式内参照は引き続き拒否
        // されることを固定する（設計 D-1・D-2。BOOLEAN 列を式の一方の被演算子
        // に置く比較は `Expr::Ident` 経由で `bind_expr_in` に到達する）。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("flag", ColumnType::Boolean, false)],
        );
        let predicates =
            crate::sql::allowlist::parse_check_predicate_text("flag = id").expect("parse");
        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let err = crate::sql::parser::bind_check_predicates(&predicates, &schema, &mut node_budget)
            .expect_err("BOOLEAN column must still be rejected under the CHECK policy");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn render_predicates_round_trips_equality_and_prefix_predicates() {
        // CREATE TABLE で宣言できる列は TEXT／VECTOR のみ（BOOLEAN 列は未対応）
        // のため、TEXT 列に対する等価・前方一致の 2 連言で往復を固定する。
        let v = parse_create_table(
            "CREATE TABLE docs (kind TEXT, body TEXT, CHECK (kind = 'a' AND body LIKE 'ab%'))",
        );
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks[0].predicate_sql, "kind = 'a' AND body LIKE 'ab%'");
    }

    /// `render_expr`（`CASE`／`COALESCE`／`NULLIF`。対象ビヘイビア: SQL-26。
    /// Issue #921）が生成するテキストが再パースで同じ木へ戻ることを固定する
    /// （このテキストは永続化されて再パースされるため）。CHECK 述語の頂点は
    /// 常に比較の `Expr::Binary` という既存の構造的保証（`render_expression_predicate`
    /// docs 参照）に合わせ、`CASE`/`COALESCE`/`NULLIF` は比較の片辺として置く。
    #[test]
    fn render_expr_round_trips_case_coalesce_nullif_through_reparse() {
        use crate::sql::allowlist::{parse_check_predicate_text, WherePredicate};
        use crate::sql::udf_call::{BinOp, Expr};

        let cases: Vec<Expr> = vec![
            Expr::Case {
                whens: vec![(
                    Expr::Binary {
                        op: BinOp::Gt,
                        lhs: Box::new(Expr::Call {
                            name: "vec_norm".to_string(),
                            args: vec![Expr::Ident("embedding".to_string())],
                        }),
                        rhs: Box::new(Expr::Number("0".to_string())),
                    },
                    Expr::Number("1".to_string()),
                )],
                else_result: Some(Box::new(Expr::Number("0".to_string()))),
            },
            Expr::Case {
                whens: vec![(
                    Expr::Binary {
                        op: BinOp::Gt,
                        lhs: Box::new(Expr::Ident("id".to_string())),
                        rhs: Box::new(Expr::Number("1".to_string())),
                    },
                    Expr::Number("1".to_string()),
                )],
                else_result: None,
            },
            Expr::Coalesce(vec![
                Expr::Null,
                Expr::Number("1".to_string()),
                Expr::Number("2".to_string()),
            ]),
            Expr::NullIf(
                Box::new(Expr::Ident("id".to_string())),
                Box::new(Expr::Number("2".to_string())),
            ),
        ];

        for expr in cases {
            let top = Expr::Binary {
                op: BinOp::Eq,
                lhs: Box::new(expr.clone()),
                rhs: Box::new(Expr::Number("1".to_string())),
            };
            let rendered = render_expression_predicate(&top);
            let reparsed = parse_check_predicate_text(&rendered)
                .unwrap_or_else(|e| panic!("reparse of {rendered:?} failed: {e:?}"));
            assert_eq!(
                reparsed,
                vec![WherePredicate::Expression(top)],
                "round trip mismatch for rendered text {rendered:?}"
            );
        }
    }

    /// codex-review（Cursor Bugbot）P1 指摘の回帰テスト: 文字列スカラー関数群
    /// （Issue #919・SQL-26）8 種すべてについて `render_expr` が生成するテキストが
    /// 再パースで同じ木へ戻ることを固定する。`POSITION` は組み込み関数の中で
    /// 唯一カンマ区切りでない特殊構文（`POSITION(needle IN haystack)`）を持つため
    /// （`sql::allowlist::Parser` はカンマ形 `POSITION(a, b)` を `42601` で拒否する）、
    /// 汎用のカンマ形レンダリングでは往復できず `CREATE TABLE` 自体が失敗して
    /// いた（`render_expr` の `POSITION` 専用腕を参照）。他の 7 関数は構文段が
    /// 通常のカンマ区切り関数呼び出し形のみを受理するため、この単体テストで
    /// 既に往復が保証されていることも合わせて固定する。
    #[test]
    fn render_expr_round_trips_all_string_scalar_functions_through_reparse() {
        use crate::sql::allowlist::{parse_check_predicate_text, WherePredicate};
        use crate::sql::udf_call::{BinOp, Expr};

        fn call(name: &str, args: Vec<Expr>) -> Expr {
            Expr::Call {
                name: name.to_string(),
                args,
            }
        }

        let cases: Vec<Expr> = vec![
            call("lower", vec![Expr::Ident("label".to_string())]),
            call("upper", vec![Expr::Ident("label".to_string())]),
            call("length", vec![Expr::Ident("label".to_string())]),
            call(
                "substr",
                vec![
                    Expr::Ident("label".to_string()),
                    Expr::Number("1".to_string()),
                ],
            ),
            call(
                "substr",
                vec![
                    Expr::Ident("label".to_string()),
                    Expr::Number("1".to_string()),
                    Expr::Number("2".to_string()),
                ],
            ),
            call(
                "concat",
                vec![
                    Expr::Ident("label".to_string()),
                    Expr::String("suffix".to_string()),
                ],
            ),
            call("trim", vec![Expr::Ident("label".to_string())]),
            call(
                "replace",
                vec![
                    Expr::Ident("label".to_string()),
                    Expr::String("a".to_string()),
                    Expr::String("b".to_string()),
                ],
            ),
            // `args` は評価順どおり `[haystack, needle]`（`sql::allowlist::
            // Parser::parse_function_call_expr` の `POSITION` 専用構文が
            // 組み立てる順序と同じ）。
            call(
                "position",
                vec![
                    Expr::Ident("label".to_string()),
                    Expr::String("a".to_string()),
                ],
            ),
            // 大文字綴り（`POSITION`）でも綴りがそのまま往復することを固定する
            // （`render_expr` が `name` フィールドをそのまま使い、リテラル
            // `"POSITION"` に固定していない回帰の確認）。
            call(
                "POSITION",
                vec![
                    Expr::Ident("label".to_string()),
                    Expr::String("a".to_string()),
                ],
            ),
        ];

        for expr in cases {
            let top = Expr::Binary {
                op: BinOp::Gt,
                lhs: Box::new(expr.clone()),
                rhs: Box::new(Expr::Number("0".to_string())),
            };
            let rendered = render_expression_predicate(&top);
            let reparsed = parse_check_predicate_text(&rendered)
                .unwrap_or_else(|e| panic!("reparse of {rendered:?} failed: {e:?}"));
            assert_eq!(
                reparsed,
                vec![WherePredicate::Expression(top)],
                "round trip mismatch for rendered text {rendered:?}"
            );
        }
    }

    #[test]
    fn render_predicates_escapes_single_quote_in_literal() {
        let v = parse_create_table("CREATE TABLE docs (kind TEXT CHECK (kind = 'a''b'))");
        let schema = schema_of(&v);
        let checks = validate_and_build(&schema, &v.checks).expect("must validate");
        assert_eq!(checks[0].predicate_sql, "kind = 'a''b'");
        // 再パースでも同じ値へ戻ることを確認（往復一致検証が既に固定しているが
        // ここでも明示する）。
        let reparsed = crate::sql::allowlist::parse_check_predicate_text(&checks[0].predicate_sql)
            .expect("reparse");
        assert_eq!(
            reparsed,
            vec![WherePredicate::Equality {
                column: "kind".to_string(),
                value: "a'b".to_string(),
            }]
        );
    }
}
