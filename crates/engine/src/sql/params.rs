//! 拡張クエリプロトコルの `$n` パラメータ束縛（Issue #935・WIRE-12・TASK-217）。
//!
//! 責務境界: `sql::lexer::tokenize_with_params` が生成した `Token::Param` を
//! 含むトークン列に対し、(a) 許可されたプレースホルダ位置だけを構造的に受理し
//! （Parse 時点。値はまだ知らない）、(b) Bind 時点で実値を検証済みの
//! `Token::StringLiteral` へ 1 トークンとして置換する、という 2 段の
//! トークン変換だけを提供する。置換後のトークン列を `sql::allowlist::
//! validate_sql_tokens`（または `core.rs::EngineCore::parse_tokens` が委譲する
//! `validate_insert_tokens` 等）へ通すのは呼び出し元（`core.rs`）の責務であり、
//! 本モジュール自体は許可リスト検証器・実行器を一切持たない（第 2 の実行器を
//! 作らない設計。値は常に 1 個の `Token::StringLiteral` の中身としてのみ格納され、
//! SQL テキストへも字句解析器へも戻らないため、値経由のインジェクション経路は
//! 構造的に存在しない）。
//!
//! ## 受理するプレースホルダ位置（WIRE-12 の規範形。これ以外は `42601`）
//!
//! 1. `ORDER BY <vec列> <=> $n`（ベクトル位置。直前トークンが [`Token::DistanceOp`]）
//! 2. `USING PLAN($n)`（`Ident("USING") Ident("PLAN") '(' $n ')'`）
//! 3. `USING OPERATION_ID $n`（INSERT／TRUNCATE／DELETE／UPDATE 共通の文末句。
//!    `Ident("USING") Ident("OPERATION_ID") $n`）
//! 4. `WHERE <列> = $n`（トップレベル `WHERE` 節の内側、`Ident '=' $n` の形のみ。
//!    `LIKE`・式比較・BOOLEAN 列条件・`SET`／`ON CONFLICT ... SET` の同型文字列は
//!    対象外——`WHERE` 節の外側にはこのパターンを一切適用しない）
//! 5. `INSERT ... VALUES ($1, $2, ...)`（複数行含む。`VALUES` 節内の `(`/`,` と
//!    `,`/`)` に挟まれた位置のみ）
//!
//! `LIMIT $n`・`USING MODE $n`・`HINT ORDER($n, ...)`・`SET search_mode = $n`・
//! `UPDATE ... SET a = $n`・`ON CONFLICT ... DO UPDATE SET a = $n`・hybrid 関数
//! 引数（`HYBRID_RRF(col, $n, ...)`）・非等価比較（`id > $n` 等）はいずれも
//! スコープ外として `42601` で拒否する（後続 Issue へ申し送り。詳細は
//! `docs/design/wire-extended-query-param-binding.md` 参照）。
//!
//! ## 値の形式
//!
//! すべてのプレースホルダは常に [`Token::StringLiteral`] として置換する。
//! `id` 列・`BOOLEAN` 列など、生の [`Token::Number`]／`Token::Ident("true"/"false")`
//! を要求する位置（例: `INSERT ... VALUES ($1)` の `$1` が `id` 列や `BOOLEAN` 列に
//! 対応する場合）は本バージョンのスコープ外であり、束縛すると「同じ値をクォート
//! したリテラルで書いた SQL」と同一の型不一致エラー（構造検証・束縛の既存契約
//! そのまま）で拒否される——これは fail-closed な既知の制約であり誤動作ではない。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{self, Token};

/// 1 文で許容するパラメータ番号の上限（実装既定値）。wire 層（Parse の宣言型
/// 件数・Bind の値件数）もこの定数を単一情報源として参照する。
pub const MAX_PARAMS: u16 = 64;

/// `WHERE` 節（パターン 4）の境界を判定するための、句の開始を示す文脈識別子
/// （字句解析段階ではキーワード化されていない語。大文字小文字を無視して照合）。
const WHERE_CLAUSE_BOUNDARY_IDENTS: [&str; 5] = ["GROUP", "HAVING", "RETURNING", "USING", "ON"];

/// `INSERT` の `VALUES` 節（パターン 5）の終端を示す文脈識別子。
const VALUES_CLAUSE_BOUNDARY_IDENTS: [&str; 2] = ["ON", "USING"];

fn ident_eq_ignore_case(token: Option<&Token>, word: &str) -> bool {
    matches!(token, Some(Token::Ident(name)) if name.eq_ignore_ascii_case(word))
}

/// `token` が比較演算子（`=`／`>`／`<`／`<=`／`>=`／`<=>`）かどうか。
///
/// `GROUP`/`HAVING`/`RETURNING`/`USING`/`ON`/`USING`（`VALUES` 節側の `ON`）は
/// `sql::lexer::keyword_from_str` が意図的にキーワード化していない語であり
/// （`catalog::validate_identifier` がこれらを正当な列名として許可しているため）、
/// `WHERE col = $n` の `col` にこれらの語がそのまま使われた場合、直後の演算子を
/// 見ずに文字列一致だけで句境界と誤判定すると `WHERE using = $1` のような
/// 正当なクエリを構造的に拒否してしまう（PR #1012 Bugbot 指摘）。実際の
/// `GROUP BY`／`HAVING <cond>`／`RETURNING <cols>`／`USING OPERATION_ID|PLAN`／
/// `ON CONFLICT` はいずれも比較演算子を直後に伴わないため、直後が比較演算子の
/// ときに限り「列名としての出現」とみなして境界判定から除外する。
///
/// この判定だけでは UDF 呼び出しの関数名として使われた場合（直後が `(`）を
/// 見逃す（PR #1012 codex 指摘）。UDF 呼び出しの直後は比較演算子でも `(` を
/// 伴わない列参照でもないため、`is_where_clause_boundary`／`values_region_end`
/// 側で句ごとの具体的な字句形（[`is_where_clause_boundary`] 参照）を追加検査し、
/// `(` が続く出現を関数呼び出しとして境界から除外する。
fn is_comparison_operator(token: Option<&Token>) -> bool {
    matches!(
        token,
        Some(Token::Punct('=') | Token::Punct('>') | Token::Punct('<'))
            | Some(Token::Le)
            | Some(Token::Ge)
            | Some(Token::DistanceOp)
    )
}

/// `tokens[idx]`（[`WHERE_CLAUSE_BOUNDARY_IDENTS`] のいずれかに一致する
/// `Ident`）が、実際に `sql::allowlist::Parser` の受理する句形を開始している
/// かどうかを判定する（PR #1012 codex 指摘対応）。
///
/// `is_comparison_operator` による「直後が比較演算子でなければ境界」という
/// 判定だけでは、これらの語を関数名とする UDF 呼び出し（`sql::udf_call`・
/// `catalog::validate_identifier` がこれらを正当な UDF 名としても許可して
/// いる）を区別できない。UDF 呼び出しは直後が `(` になり比較演算子ではない
/// ため、例えば `WHERE using(body) = 'x' AND lang = $1` の `using` が
/// `WHERE` 領域を誤って終端し、本来許可される `lang = $1` まで `42601` で
/// 拒否されてしまう。
///
/// ここでは各句を実際に受理する `Parser` の構造（`parse_group_by_clause`・
/// `parse_having`・`parse_returning_clause`・`parse_operation_id_clause`・
/// `parse_on_conflict_clause` 各コメント参照）に対応する最小限の字句形のみを
/// 「本物の句」として認め、それ以外（UDF 呼び出し・比較演算子を伴う列参照）は
/// 境界とみなさない。安全側に倒す設計のため、この判定を追加しても許可されて
/// いなかった位置の `$n` が新たに通ることはない（既存の `is_where_equality`
/// 等の受理条件は無変更）。
fn is_where_clause_boundary(tokens: &[Token], idx: usize) -> bool {
    let next1 = tokens.get(idx + 1);
    let next2 = tokens.get(idx + 2);
    let Some(Token::Ident(name)) = tokens.get(idx) else {
        return false;
    };
    match name.to_ascii_uppercase().as_str() {
        // `GROUP BY <column>`（`Parser::parse_group_by_clause`）。UDF 呼び出し
        // `group(...)` は直後が `(` になり `BY` に一致しない。`BY` は
        // `sql::lexer::keyword_from_str` がキーワード化する語であり、字句解析後は
        // `Token::Ident("BY")` ではなく `Token::Keyword(Keyword::By)` として現れる
        // （`Parser::parse_group_by_clause` も `expect_keyword(Keyword::By)` で照合
        // する）。`Ident` として照合すると本物の `GROUP BY` を句境界と認識できず、
        // `WHERE` 領域が `GROUP BY` 以降まで延びて範囲外の `<col> = $n` を
        // パターン 4 として誤受理してしまう（PR #1012 Cursor Bugbot 指摘）。
        "GROUP" => matches!(next1, Some(Token::Keyword(crate::sql::lexer::Keyword::By))),
        // `HAVING <ident> <cmp> ...`（`Parser::parse_having`）。UDF 呼び出し
        // `having(...)` は直後が `(` になり `Ident` に一致しない。
        "HAVING" => matches!(next1, Some(Token::Ident(_))) && is_comparison_operator(next2),
        // `RETURNING <投影>`（`Parser::parse_returning_clause`。`*` または
        // 列名の列挙のみを受理し関数呼び出し項目は拒否する）。UDF 呼び出し
        // `returning(...)` は直後が `(` になり、列名としての比較
        // `returning = $1` は直後が `=` になるため、いずれも一致しない。
        "RETURNING" => matches!(next1, Some(Token::Ident(_) | Token::Punct('*'))),
        // `USING OPERATION_ID $n` / `USING PLAN(...)`（`Parser` がこの 2 語の
        // みを文脈的キーワードとして照合する）。UDF 呼び出し `using(...)` は
        // 直後が `(` になり `OPERATION_ID`／`PLAN` に一致しない。
        "USING" => {
            ident_eq_ignore_case(next1, "OPERATION_ID") || ident_eq_ignore_case(next1, "PLAN")
        }
        // `ON CONFLICT ...`（INSERT 専用。`WHERE` を持つ文には現れないが、
        // 定数配列を [`VALUES_CLAUSE_BOUNDARY_IDENTS`] と共有する構成に
        // 合わせ安全側の判定を残す）。
        "ON" => ident_eq_ignore_case(next1, "CONFLICT"),
        _ => false,
    }
}

/// `tokens` 中のトップレベル `WHERE`（パターン 4）の受理範囲 `(start, end)` を
/// 判定する単一情報源。`start` は `WHERE` キーワード自身の添字、`end` は
/// 後続句の境界（`ORDER`／`LIMIT`／[`is_where_clause_boundary`] が認める
/// 実句境界）の添字（無ければ `tokens.len()`）。`WHERE` を含まない文は
/// `None`。[`validate_param_positions`]・[`where_equality_literal_is_param`]
/// の双方がこの 1 箇所を参照することで、境界判定がずれる余地をなくす
/// （PR #1012 レビュー指摘の再発防止）。
fn where_region(tokens: &[Token]) -> Option<(usize, usize)> {
    let start = tokens
        .iter()
        .position(|t| matches!(t, Token::Keyword(crate::sql::lexer::Keyword::Where)))?;
    let end = tokens
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(idx, t)| match t {
            Token::Keyword(crate::sql::lexer::Keyword::Order)
            | Token::Keyword(crate::sql::lexer::Keyword::Limit) => Some(idx),
            Token::Ident(name)
                if WHERE_CLAUSE_BOUNDARY_IDENTS
                    .iter()
                    .any(|b| name.eq_ignore_ascii_case(b))
                    // 列名としての出現（例: `WHERE using = $1`）・UDF 呼び出しの
                    // 関数名としての出現（例: `WHERE using(body) = 'x'`）はいずれも
                    // 境界とみなさない。実際に対応する句を開始している場合
                    // （`GROUP BY`・`HAVING <ident> <cmp>`・`RETURNING <col|*>`・
                    // `USING OPERATION_ID|PLAN`・`ON CONFLICT`）に限り境界とする
                    // （PR #1012 Bugbot・codex 指摘。詳細は
                    // [`is_where_clause_boundary`] 参照）。
                    && is_where_clause_boundary(tokens, idx) =>
            {
                Some(idx)
            }
            _ => None,
        })
        .unwrap_or(tokens.len());
    Some((start, end))
}

/// `tokens`（`$n` 置換前の元トークン列）の `WHERE` 節内に現れる等価述語
/// （[`validate_param_positions`] パターン 4。`Ident '=' (StringLiteral|Param)`
/// の形。`sql::allowlist::Parser::parse_where` の legacy 分岐が受理する
/// 唯一の形で、`WherePredicate::Equality` へ束縛される）を出現順に走査し、
/// その右辺値が `$n`（`Token::Param`）由来かどうかを並べたベクタを返す。
///
/// [`crate::core::EngineCore::parse_sql_prepared`] が [`crate::core::
/// PreparedSql`] へ結果を持たせ、`sql::parser::bind_where_predicates` の
/// `dummy_equality_flags` 引数として「ダミー値へ置換された等価述語の値に
/// 限り ENUM 列の語彙照合（`declarative_filter`）を Describe 時点では省略して
/// よい」ことを機械的に判定するために使う（PR #1012 Cursor Bugbot 指摘対応:
/// `$n` を含まない文——WHERE 等価条件の値が元の SQL テキストに書かれた実
/// リテラルである文——では、`substitute_dummy` は当該位置を一切変更しない
/// ため `dummy_parsed` にもその実リテラルがそのまま残る。この場合にまで
/// 検証を省略すると、通常の Describe なら `22P02` で弾かれるはずの語彙外
/// ENUM ラベルが Prepared Describe だけ素通りしてしまう——
/// [`order_by_distance_literal_is_param`] と同じ設計）。
///
/// `WherePredicate::Prefix`（`LIKE`）・`BoolEquality`・`BoolColumn` の右辺値は
/// `sql::params` が受理する `$n` 位置（パターン 4）の対象外（`Ident '=' $n`
/// の形にしかならない）であり、本関数の走査対象にもならない。
pub fn where_equality_literal_is_param(tokens: &[Token]) -> Vec<bool> {
    let Some((start, end)) = where_region(tokens) else {
        return Vec::new();
    };
    let mut flags = Vec::new();
    let mut i = start + 1;
    while i < end {
        if matches!(tokens.get(i), Some(Token::Ident(_)))
            && matches!(tokens.get(i + 1), Some(Token::Punct('=')))
        {
            match tokens.get(i + 2) {
                Some(Token::Param(_)) => {
                    flags.push(true);
                    i += 3;
                    continue;
                }
                Some(Token::StringLiteral(_)) => {
                    flags.push(false);
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    flags
}

/// `tokens` 中の `Token::Param` がすべて許可位置に収まっていることを検証し、
/// 文が要求するパラメータ数（最大の `$n` 番号。1 始まり。`$n` が 1 つも
/// 無ければ 0）を返す。
///
/// - 許可位置以外に現れた `$n` は [`SqlSurfaceError::unsupported`]（`42601`）。
/// - `$n` の番号が [`MAX_PARAMS`] を超える場合は
///   [`SqlSurfaceError::payload_too_large`]（`54000`）。
pub fn validate_param_positions(tokens: &[Token]) -> Result<u16, SqlSurfaceError> {
    // WIRE-15・TASK-218: カーソル（`DECLARE`/`FETCH`/`CLOSE`）は `$n` 束縛を
    // 本バージョンのスコープ外として一律拒否する（fail-closed。範囲を縮小）。
    // 特に `DECLARE ... FOR SELECT ... WHERE col = $1` は、後段のパターン 4
    // （`WHERE` 等価述語）判定が文の先頭語を見ずに `WHERE` キーワードの位置
    // だけで判定するため、ここで先頭語を見て弾かないと DECLARE の内側 SELECT
    // 経由で誤って受理されてしまう。
    if matches!(
        tokens.first(),
        Some(Token::Ident(name))
            if name.eq_ignore_ascii_case("DECLARE")
                || name.eq_ignore_ascii_case("FETCH")
                || name.eq_ignore_ascii_case("CLOSE")
    ) && tokens.iter().any(|t| matches!(t, Token::Param(_)))
    {
        return Err(SqlSurfaceError::unsupported(
            "parameter placeholders are not supported in cursor statements",
        ));
    }

    // TASK-213・SQL-29 (b)（Issue #928）: `WITH`（非再帰 CTE）を先頭語に持つ文は
    // `$n` を含む場合、一律拒否する（カーソルと同じ fail-closed 方針）。
    // 理由: 本モジュールのパターン判定（特にパターン 4「WHERE 等価」）は
    // 文の先頭語を見ずトークン列上の位置だけで許可位置を判定するが、CTE を
    // 含む文では `sql::cte::resolve_relation` が CTE の `WHERE` 述語を主クエリの
    // 述語の前へ挿入する（`build_scan_from_resolved`）ため、束縛時に `$n` の
    // 対応先が組み立て後の述語列とトークン列上の位置とでずれる恐れがある。
    if matches!(
        tokens.first(),
        Some(Token::Ident(name)) if name.eq_ignore_ascii_case("WITH")
    ) && tokens.iter().any(|t| matches!(t, Token::Param(_)))
    {
        return Err(SqlSurfaceError::unsupported(
            "parameter placeholders are not supported in WITH statements",
        ));
    }

    let is_insert_statement = ident_eq_ignore_case(tokens.first(), "INSERT");

    // パターン 5（INSERT VALUES）の受理範囲: `VALUES` キーワード（文脈識別子。
    // 大文字小文字を無視して最初の出現のみを見る——本許可形状は `VALUES` 節を
    // 1 つしか持たない）の直後から、`ON`／`USING` のいずれかが現れる直前まで。
    let values_region = if is_insert_statement {
        tokens.iter().position(|t| match t {
            Token::Ident(name) => name.eq_ignore_ascii_case("VALUES"),
            _ => false,
        })
    } else {
        None
    };
    let values_region_end = values_region.map(|start| {
        tokens
            .iter()
            .enumerate()
            .skip(start + 1)
            .find_map(|(idx, t)| match t {
                Token::Ident(name)
                    if VALUES_CLAUSE_BOUNDARY_IDENTS
                        .iter()
                        .any(|b| name.eq_ignore_ascii_case(b))
                        // 列名としての出現（直後が比較演算子）は境界とみなさない。
                        && !is_comparison_operator(tokens.get(idx + 1)) =>
                {
                    Some(idx)
                }
                _ => None,
            })
            .unwrap_or(tokens.len())
    });

    // パターン 4（WHERE 等価）の受理範囲は [`where_region`] が単一情報源として
    // 判定する（[`where_equality_literal_is_param`] も同じ境界判定を再利用し、
    // 両関数間で境界判定がずれる余地をなくす）。`SET`（statement 全体）である
    // 場合は `WHERE` を含まないため自然に対象外になる。
    let (where_idx, where_region_end) = match where_region(tokens) {
        Some((start, end)) => (Some(start), Some(end)),
        None => (None, None),
    };

    let mut max_index: u16 = 0;
    for (i, token) in tokens.iter().enumerate() {
        let Token::Param(n) = token else { continue };
        let n = *n;
        max_index = max_index.max(n);

        let prev1 = tokens.get(i.wrapping_sub(1));
        let prev2 = i.checked_sub(2).and_then(|j| tokens.get(j));
        let prev3 = i.checked_sub(3).and_then(|j| tokens.get(j));
        let next1 = tokens.get(i + 1);

        // パターン 1: `<col> <=> $n`。
        let is_distance = matches!(prev1, Some(Token::DistanceOp));

        // パターン 2: `USING PLAN($n)`。
        let is_using_plan = ident_eq_ignore_case(prev3, "USING")
            && ident_eq_ignore_case(prev2, "PLAN")
            && matches!(prev1, Some(Token::Punct('(')))
            && matches!(next1, Some(Token::Punct(')')));

        // パターン 3: `USING OPERATION_ID $n`。
        let is_operation_id =
            ident_eq_ignore_case(prev2, "USING") && ident_eq_ignore_case(prev1, "OPERATION_ID");

        // パターン 4: `WHERE` 節内の `<ident> = $n`。
        let is_where_equality = where_idx.is_some_and(|start| {
            let end = where_region_end.unwrap_or(tokens.len());
            i > start
                && i < end
                && matches!(prev1, Some(Token::Punct('=')))
                && matches!(prev2, Some(Token::Ident(_)))
        });

        // パターン 5: `INSERT ... VALUES (...)` の行内リテラル位置。
        let is_insert_value = values_region.is_some_and(|start| {
            let end = values_region_end.unwrap_or(tokens.len());
            i > start
                && i < end
                && matches!(prev1, Some(Token::Punct('(')) | Some(Token::Punct(',')))
                && matches!(next1, Some(Token::Punct(')')) | Some(Token::Punct(',')))
        });

        if !(is_distance
            || is_using_plan
            || is_operation_id
            || is_where_equality
            || is_insert_value)
        {
            return Err(SqlSurfaceError::unsupported(format!(
                "parameter placeholder ${n} is not supported at this position"
            )));
        }
    }

    if max_index > MAX_PARAMS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "parameter number ${max_index} exceeds the allowed maximum (${MAX_PARAMS})"
        )));
    }

    Ok(max_index)
}

/// `tokens`（`$n` 置換前の元トークン列）に `ORDER BY <vec列> <=> $n`
/// （[`validate_param_positions`] パターン 1）の `$n` が含まれるかどうかを
/// 判定する。[`crate::core::EngineCore::parse_sql_prepared`] が
/// [`crate::core::PreparedSql`] へ結果を持たせ、`describe_prepared_in_session`
/// が「ダミー値へ置換された distance 位置に限りベクトルリテラルの実パースを
/// 省略してよい」ことを機械的に判定するために使う（PR #1012 レビュー指摘
/// 対応: `$n` を含まない文——`ORDER BY` のベクトルリテラルが元の SQL テキスト
/// に書かれた実リテラルである文——では、`substitute_dummy` は当該位置を一切
/// 変更しないため `dummy_parsed` にもその実リテラルがそのまま残る。この場合に
/// まで検証を省略すると、通常の Describe（`skip_vector_literal_validation ==
/// false`）なら `22000` で弾かれるはずの不正なベクトルリテラルが Prepared
/// Describe だけ素通りし、エラーが Execute まで遅延してしまう）。
///
/// パターン 1 は `HYBRID_RRF(...)` 等の 4 引数 `ORDER BY` 関数呼び出し形
/// （[`crate::sql::allowlist::OrderByForm::FunctionCall`]）のベクトルリテラルを
/// 対象にしない（`sql::params` モジュールドキュメント: hybrid 関数引数への
/// `$n` はスコープ外として `42601` で拒否するため、この形のベクトルリテラルは
/// 常に元の SQL テキストの実リテラルのまま——本関数の対象外で構わない）。
pub fn order_by_distance_literal_is_param(tokens: &[Token]) -> bool {
    tokens.iter().enumerate().any(|(i, token)| {
        matches!(token, Token::Param(_))
            && matches!(tokens.get(i.wrapping_sub(1)), Some(Token::DistanceOp))
    })
}

/// [`Token::Param`] をすべて `value_for` が返す文字列の [`Token::StringLiteral`]
/// へ置換したトークン列を返す。`value_for` は 1 始まりのパラメータ番号を受け取る。
fn substitute_with<E>(
    tokens: &[Token],
    mut value_for: impl FnMut(u16) -> Result<String, E>,
) -> Result<Vec<Token>, E> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token {
            Token::Param(n) => out.push(Token::StringLiteral(value_for(*n)?)),
            other => out.push(other.clone()),
        }
    }
    Ok(out)
}

/// Parse 時点の構造検証専用: すべての `$n` を、位置に依存しない固定ダミー値へ
/// 置換する。ダミーは非空・非制御文字（[`crate::sql::using_operation_id::
/// OperationId::parse`]・[`crate::sql::allowlist::validate_using_plan_question`]
/// のいずれも満たす）であり、それ以外の位置（ベクトルリテラル・WHERE 等価・
/// INSERT VALUES）では値の意味論的妥当性が Bind まで遅延されるため内容を問わない。
/// 得られたトークン列を実行・Describe に使ってはならない（値未確定のダミーの
/// ため、呼び出し元は構造検証の結果だけを見て破棄する）。
pub fn substitute_dummy(tokens: &[Token]) -> Vec<Token> {
    substitute_with(tokens, |_| {
        Ok::<_, std::convert::Infallible>("0".to_string())
    })
    .unwrap_or_default()
}

/// Bind 時点: `values`（`$1` から順に 1 始まりで対応する実値）で全 `$n` を
/// 置換する。呼び出し元は事前に `values.len()` が [`validate_param_positions`]
/// の返す `param_count` と一致することを確認していること（本関数は添字
/// アクセスに `checked_sub`／`get` を使うが、この事前条件自体は検査しない）。
pub fn substitute_values(
    tokens: &[Token],
    values: &[String],
) -> Result<Vec<Token>, SqlSurfaceError> {
    substitute_with(tokens, |n| {
        let idx = usize::from(n).checked_sub(1).ok_or_else(|| {
            SqlSurfaceError::unsupported("parameter placeholder $0 is not valid".to_string())
        })?;
        values.get(idx).cloned().ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("missing bind value for parameter ${n}"))
        })
    })
}

/// Bind 値の UTF-8 検証・NUL 拒否・置換後総バイト長の上限判定
/// （[`crate::sql::lexer::MAX_INPUT_LEN`]）を行い、`Token::StringLiteral` へ
/// 格納できる文字列へ変換する。`tokens` は `$n` の出現回数を数えるために使う
/// （同一 `$n` を大量に参照させて 1 回の値から巨大な置換結果を作らせる
/// メモリ増幅を、値の複製（[`String::to_string`]）より前に防ぐ）。
///
/// P0（codex-review・Issue #935 PR #1012 指摘）: `values` は
/// [`validate_param_positions`] が返す `param_count`（文中で参照される
/// 最大の `$n` 番号）と同数だけ渡ってくる契約のため、文が実際に参照して
/// いない `$n`（例: `$64` だけを使う文に対する `$1`〜`$63`）に巨大な値が
/// 含まれ得る。この関数はまず UTF-8／NUL 検証を借用 `&str` のまま行い
/// （複製しない）、置換後総バイト数（参照回数込み）を checked 演算で
/// 確定させたうえで、実際に参照されている位置の値だけを複製する。未参照の
/// 値は（検証は受けるが）一切複製されないため、その内容量はメモリ増幅
/// 攻撃の入力にならない。
///
/// - `NULL`（`values` の要素が `None`）は本バージョンのスコープ外として
///   [`SqlSurfaceError::invalid_input`]（`22000`）で拒否する（未参照の
///   位置も含め、渡された全値に対して検証する）。
/// - 非 UTF-8・NUL 文字混入も同じ `22000`。
/// - 置換後総バイト数超過は [`SqlSurfaceError::payload_too_large`]（`54000`）。
pub fn decode_bind_values(
    tokens: &[Token],
    values: &[Option<Vec<u8>>],
) -> Result<Vec<String>, SqlSurfaceError> {
    // 借用のまま UTF-8 検証・NUL 拒否を行う（値の複製より前）。
    let mut borrowed: Vec<&str> = Vec::with_capacity(values.len());
    for raw in values {
        let bytes = raw.as_ref().ok_or_else(|| {
            SqlSurfaceError::invalid_input("NULL parameter values are not supported".to_string())
        })?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| SqlSurfaceError::invalid_input("parameter value is not valid UTF-8"))?;
        if text.contains('\0') {
            return Err(SqlSurfaceError::invalid_input(
                "parameter value must not contain a NUL byte",
            ));
        }
        borrowed.push(text);
    }

    // 総置換バイト数（$n の出現回数 × 対応する値の長さ、の総和）を、値の
    // 複製より前に借用済みスライスの長さだけを見て checked 演算で確定
    // させる。未参照の $n（`tokens` に一切現れない番号）の値はここでも
    // 一切参照されないため、集計・複製いずれのコストにも計上されない。
    //
    // codex-review P2（Issue #935 PR #1012 指摘）: `$n` 以外のトークン
    // （識別子・既存の文字列リテラル・数値リテラル等）が置換後のトークン列に
    // 残す分のバイト長もここで加算する。これを含めないと、`total`（束縛値の
    // 長さのみ）は `MAX_INPUT_LEN` 以内でも、置換後のトークン列全体（元の
    // SQL テキストに由来する非 `Param` 部分＋束縛値）が実質的にこの上限を
    // 超えうる（`tokenize_with_params` が検証する「元の SQL テキストの長さ」
    // と「置換後のトークン列が表す総バイト長」は別物であるため）。
    let mut total: usize = 0;
    for token in tokens {
        let len = match token {
            Token::Param(n) => {
                let idx = usize::from(*n).checked_sub(1);
                idx.and_then(|i| borrowed.get(i))
                    .map(|s| s.len())
                    .unwrap_or(0)
            }
            Token::Ident(s) | Token::StringLiteral(s) | Token::Number(s) => s.len(),
            Token::QualifiedIdent { qualifier, name } => {
                qualifier.len().checked_add(name.len()).ok_or_else(|| {
                    SqlSurfaceError::payload_too_large(
                        "substituted parameter payload size overflowed",
                    )
                })?
            }
            Token::Keyword(_) | Token::Punct(_) | Token::DistanceOp | Token::Le | Token::Ge => 0,
        };
        total = total.checked_add(len).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("substituted parameter payload size overflowed")
        })?;
    }
    if total > lexer::MAX_INPUT_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "substituted parameter payload ({total} bytes) exceeds the allowed limit ({} bytes)",
            lexer::MAX_INPUT_LEN
        )));
    }

    // サイズ検証を通過した後、実際に参照されている位置の値だけを複製する。
    // 未参照の位置は空文字列のまま残す（`substitute_values` は `tokens` の
    // `Token::Param` からしか添字アクセスしないため参照されず、複製コストも
    // メモリ上の保持コストも発生しない）。
    let mut decoded: Vec<String> = vec![String::new(); values.len()];
    for token in tokens {
        let Token::Param(n) = token else { continue };
        let Some(idx) = usize::from(*n).checked_sub(1) else {
            continue;
        };
        if let Some(slot) = decoded.get_mut(idx) {
            if slot.is_empty() {
                if let Some(text) = borrowed.get(idx) {
                    *slot = (*text).to_string();
                }
            }
        }
    }

    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params;

    fn positions(sql: &str) -> Result<u16, SqlSurfaceError> {
        let tokens = tokenize_with_params(sql).expect("tokenize_with_params should succeed");
        validate_param_positions(&tokens)
    }

    #[test]
    fn accepts_vector_distance_position() {
        assert_eq!(
            positions("SELECT * FROM documents ORDER BY embedding <=> $1 LIMIT 5").unwrap(),
            1
        );
    }

    #[test]
    fn order_by_distance_literal_is_param_detects_dollar_param() {
        let tokens =
            tokenize_with_params("SELECT * FROM documents ORDER BY embedding <=> $1 LIMIT 5")
                .expect("tokenize_with_params should succeed");
        assert!(order_by_distance_literal_is_param(&tokens));
    }

    #[test]
    fn order_by_distance_literal_is_param_false_for_real_literal() {
        let tokens = tokenize_with_params(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("tokenize_with_params should succeed");
        assert!(!order_by_distance_literal_is_param(&tokens));
    }

    #[test]
    fn order_by_distance_literal_is_param_false_when_dollar_param_is_unrelated() {
        let tokens = tokenize_with_params(
            "SELECT * FROM documents WHERE lang = $1 ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("tokenize_with_params should succeed");
        assert!(!order_by_distance_literal_is_param(&tokens));
    }

    #[test]
    fn order_by_distance_literal_is_param_false_for_hybrid_function_argument() {
        // hybrid 関数引数への `$n` はそもそも `validate_param_positions` が
        // `42601` で拒否するスコープ外だが、`order_by_distance_literal_is_param`
        // 単体はトークン列の形だけを見るため、このパターン（`DistanceOp` を
        // 伴わない）を誤って `true` としないことも確認する。
        let tokens = tokenize_with_params(
            "SELECT * FROM documents ORDER BY HYBRID_RRF(embedding, '[0.1,0.2,0.3]', body, 'q') LIMIT 5",
        )
        .expect("tokenize_with_params should succeed");
        assert!(!order_by_distance_literal_is_param(&tokens));
    }

    // PR #1012 Cursor Bugbot 指摘の回帰: `where_equality_literal_is_param` は
    // `WHERE` 等価述語（パターン 4）の値が `$n` 由来か実リテラルかを出現順で
    // 正しく判定しなければならない。
    #[test]
    fn where_equality_literal_is_param_detects_dollar_param() {
        let tokens = tokenize_with_params("SELECT * FROM documents WHERE mood = $1 LIMIT 5")
            .expect("tokenize_with_params should succeed");
        assert_eq!(where_equality_literal_is_param(&tokens), vec![true]);
    }

    #[test]
    fn where_equality_literal_is_param_false_for_real_literal() {
        let tokens = tokenize_with_params("SELECT * FROM documents WHERE mood = 'happy' LIMIT 5")
            .expect("tokenize_with_params should succeed");
        assert_eq!(where_equality_literal_is_param(&tokens), vec![false]);
    }

    #[test]
    fn where_equality_literal_is_param_preserves_occurrence_order_for_multiple_predicates() {
        let tokens = tokenize_with_params(
            "SELECT * FROM documents WHERE lang = $1 AND mood = 'happy' LIMIT 5",
        )
        .expect("tokenize_with_params should succeed");
        assert_eq!(where_equality_literal_is_param(&tokens), vec![true, false]);
    }

    #[test]
    fn where_equality_literal_is_param_empty_when_no_where_clause() {
        let tokens = tokenize_with_params("SELECT * FROM documents ORDER BY embedding <=> $1")
            .expect("tokenize_with_params should succeed");
        assert!(where_equality_literal_is_param(&tokens).is_empty());
    }

    #[test]
    fn accepts_using_plan_position() {
        assert_eq!(
            positions("SELECT * FROM documents USING PLAN($1) LIMIT 5").unwrap(),
            1
        );
    }

    #[test]
    fn accepts_using_operation_id_position() {
        assert_eq!(
            positions("INSERT INTO documents (id, body) VALUES (1, 'x') USING OPERATION_ID $1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn accepts_where_equality_position() {
        assert_eq!(
            positions("SELECT * FROM documents WHERE lang = $1 LIMIT 5").unwrap(),
            1
        );
    }

    #[test]
    fn accepts_insert_values_position() {
        assert_eq!(
            positions("INSERT INTO documents (id, body) VALUES ($1, $2) USING OPERATION_ID 'op-1'")
                .unwrap(),
            2
        );
    }

    #[test]
    fn accepts_multi_row_insert_values_positions() {
        assert_eq!(
            positions(
                "INSERT INTO documents (id, body) VALUES ($1, $2), ($3, $4) USING OPERATION_ID 'op-1'"
            )
            .unwrap(),
            4
        );
    }

    #[test]
    fn rejects_limit_position() {
        assert!(
            positions("SELECT * FROM documents ORDER BY embedding <=> '[0]' LIMIT $1").is_err()
        );
    }

    #[test]
    fn rejects_offset_position() {
        // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` は `$n` プレースホルダの許可
        // 位置（本モジュールドキュメントの表）に含まれない。広域取得の `OFFSET` へ
        // `$n` を渡す形は本モジュールの変更なしに構造上拒否される（許可リストに
        // 無い位置は一律 `42601`）。
        assert!(positions("SELECT * FROM documents LIMIT 5 OFFSET $1").is_err());
    }

    #[test]
    fn rejects_using_mode_position() {
        let err =
            positions("SELECT * FROM documents ORDER BY embedding <=> '[0]' LIMIT 5 USING MODE $1")
                .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_param_in_with_statement() {
        // TASK-213・SQL-29 (b)（Issue #928）。
        let err =
            positions("WITH x AS (SELECT id FROM documents) SELECT * FROM x WHERE id = $1 LIMIT 5")
                .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_hint_order_position() {
        assert!(positions(
            "SELECT * FROM documents ORDER BY embedding <=> '[0]' LIMIT 5 HINT ORDER($1)"
        )
        .is_err());
    }

    #[test]
    fn rejects_set_search_mode_position() {
        assert!(positions("SET search_mode = $1").is_err());
    }

    #[test]
    fn rejects_update_set_position() {
        assert!(positions("UPDATE documents SET lang = $1 WHERE id = 1").is_err());
    }

    #[test]
    fn accepts_where_equality_for_update_predicate_form() {
        // `UPDATE ... SET ... WHERE <col> = $n` は SET 側を拒否しつつ WHERE 側の
        // 等価条件は受理する（両者が同じ `Ident '=' $n` の字面を持つため、
        // WHERE 節の範囲判定でのみ区別する）。
        assert_eq!(
            positions("UPDATE documents SET lang = 'ja' WHERE id_tag = $1").unwrap(),
            1
        );
    }

    #[test]
    fn rejects_on_conflict_do_update_set_position() {
        assert!(positions(
            "INSERT INTO documents (id, body) VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET body = $1 USING OPERATION_ID 'op-1'"
        )
        .is_err());
    }

    #[test]
    fn rejects_hybrid_function_argument_position() {
        assert!(positions(
            "SELECT * FROM documents ORDER BY HYBRID_RRF(embedding, $1, body, 'q') LIMIT 5"
        )
        .is_err());
    }

    #[test]
    fn rejects_non_equality_where_comparison_position() {
        assert!(positions("SELECT * FROM documents WHERE id > $1 LIMIT 5").is_err());
    }

    // PR #1012 Bugbot 指摘: `GROUP`/`HAVING`/`RETURNING`/`USING`/`ON` は
    // キーワード化されておらず正当な列名としても使えるため、`WHERE <col> = $n`
    // の `col` がこれらの語と一致する場合でも句境界と誤認せず等価条件として
    // 受理しなければならない。
    #[test]
    fn accepts_where_equality_when_column_name_matches_boundary_word() {
        for word in ["using", "group", "having", "returning", "on"] {
            let sql = format!("SELECT * FROM documents WHERE {word} = $1 LIMIT 5");
            assert_eq!(
                positions(&sql).unwrap_or_else(|e| panic!("{word} should be accepted: {e:?}")),
                1,
                "column named `{word}` should not be treated as a clause boundary"
            );
        }
    }

    #[test]
    fn accepts_where_equality_when_column_name_matches_boundary_word_with_trailing_clause() {
        // 境界語が列名として使われたあとに、本物の句境界（`ORDER BY`）が
        // 続く場合でも WHERE 領域が正しく閉じられ、当該 `ORDER BY` の
        // ベクトル距離パラメータは通常どおり受理されることを確認する。
        assert_eq!(
            positions("SELECT * FROM documents WHERE using = $1 ORDER BY embedding <=> $2 LIMIT 5")
                .unwrap(),
            2
        );
    }

    // PR #1012 codex 指摘の回帰: `GROUP`/`HAVING`/`RETURNING`/`USING`/`ON` を
    // 関数名とする UDF 呼び出し（直後が `(`）は、比較演算子を直後に伴わない
    // ため旧実装では無条件に句境界と誤認され、その後に続く本来許可される
    // `$n`（`lang = $n`）まで `42601` で拒否されていた。UDF 呼び出しの直後に
    // `WHERE` 等価条件の `$n` が続く場合、正しく受理されなければならない
    // （安全側の判定は崩さない——境界語が UDF 呼び出し以外の形で現れる場合の
    // 挙動は他のテストが固定するとおり無変更）。
    #[test]
    fn accepts_where_equality_after_udf_call_named_like_boundary_word() {
        for word in ["using", "group", "having", "returning", "on"] {
            let sql =
                format!("SELECT * FROM documents WHERE {word}(body) = 'x' AND lang = $1 LIMIT 5");
            assert_eq!(
                positions(&sql).unwrap_or_else(|e| panic!(
                    "UDF call named `{word}` should not close the WHERE region: {e:?}"
                )),
                1,
                "UDF call named `{word}` must not be treated as a clause boundary"
            );
        }
    }

    // 同上の裏返し: 本物の句境界（`GROUP BY`・`USING OPERATION_ID`）はこの変更
    // 後も引き続き `WHERE` 領域を正しく終端し、句境界より後ろに現れる
    // `$n`（許可形状外の位置）は従来どおり `42601` のまま拒否されなければ
    // ならない（fail-closed の維持）。
    #[test]
    fn rejects_dollar_param_placed_after_real_group_by_boundary() {
        let sql = "SELECT COUNT(*) FROM documents WHERE lang = 'ja' GROUP BY lang HAVING $1 = 1";
        let err = positions(sql).unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    // PR #1012 Cursor Bugbot 指摘の回帰: `BY` は字句解析で `Keyword::By` になる
    // ため、`GROUP` の直後を `Ident("BY")` で照合していた旧実装では本物の
    // `GROUP BY` が句境界と認識されず、`WHERE` 領域が `GROUP BY` 以降まで延びて
    // `GROUP BY <col> = $n` の `$n` をパターン 4（WHERE 等価）として誤受理して
    // いた。`HAVING` 等の他の境界に頼らず `GROUP BY` だけで領域が閉じることを
    // 固定する（既存の範囲外パラメータと同じ `42601`）。
    #[test]
    fn rejects_dollar_param_in_equality_form_after_group_by_boundary() {
        for sql in [
            "SELECT lang, COUNT(*) AS n FROM documents WHERE lang = 'ja' GROUP BY lang = $1",
            "SELECT lang, COUNT(*) AS n FROM documents WHERE lang = 'ja' group by lang = $1",
            "SELECT lang, COUNT(*) AS n FROM documents WHERE lang = $1 GROUP BY lang = $2",
        ] {
            let err = positions(sql).unwrap_err();
            assert_eq!(err.wire_code(), "42601", "sql = {sql}");
        }
    }

    #[test]
    fn where_region_closes_at_real_group_by_boundary() {
        let tokens = tokenize_with_params(
            "SELECT lang, COUNT(*) AS n FROM documents WHERE lang = $1 GROUP BY lang = 'x'",
        )
        .expect("tokenize_with_params should succeed");
        let (start, end) = where_region(&tokens).expect("WHERE region should exist");
        assert!(matches!(
            tokens.get(start),
            Some(Token::Keyword(crate::sql::lexer::Keyword::Where))
        ));
        assert!(matches!(tokens.get(end), Some(Token::Ident(name)) if name == "GROUP"));
        // `GROUP BY` より後ろの等価形は WHERE 等価述語として数えない。
        assert_eq!(where_equality_literal_is_param(&tokens), vec![true]);
    }

    #[test]
    fn accepts_real_using_operation_id_boundary_after_where_predicate() {
        // 本物の `USING OPERATION_ID` 句（直後が `OPERATION_ID`）は、UDF 呼び出し
        // （直後が `(`）と区別され、引き続き `WHERE` 領域を正しく終端したうえで
        // パターン 3 として許可されることを固定する。
        let sql = "UPDATE documents SET lang = 'en' WHERE id_tag = 'x' USING OPERATION_ID $1";
        assert_eq!(positions(sql).unwrap(), 1);
    }

    #[test]
    fn rejects_parameter_number_over_max() {
        let sql = format!(
            "SELECT * FROM documents WHERE lang = ${}",
            (MAX_PARAMS as u32) + 1
        );
        let err = positions(&sql).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn accepts_parameter_number_at_max() {
        let sql = format!("SELECT * FROM documents WHERE lang = ${MAX_PARAMS}");
        assert!(positions(&sql).is_ok());
    }

    #[test]
    fn no_params_yields_zero_count() {
        assert_eq!(positions("SELECT * FROM documents LIMIT 5").unwrap(), 0);
    }

    #[test]
    fn substitute_dummy_replaces_every_param_with_string_literal() {
        let tokens = tokenize_with_params("embedding <=> $1").expect("tokenize should succeed");
        let dummy = substitute_dummy(&tokens);
        assert_eq!(
            dummy,
            vec![
                Token::Ident("embedding".to_string()),
                Token::DistanceOp,
                Token::StringLiteral("0".to_string()),
            ]
        );
    }

    #[test]
    fn substitute_values_maps_each_param_to_its_value() {
        let tokens =
            tokenize_with_params("WHERE lang = $1 AND lang = $2").expect("tokenize should succeed");
        let substituted =
            substitute_values(&tokens, &["ja".to_string(), "en".to_string()]).unwrap();
        assert_eq!(
            substituted,
            vec![
                Token::Keyword(crate::sql::lexer::Keyword::Where),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("ja".to_string()),
                Token::Keyword(crate::sql::lexer::Keyword::And),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("en".to_string()),
            ]
        );
    }

    #[test]
    fn substitute_values_repeated_param_reuses_same_value() {
        let tokens =
            tokenize_with_params("WHERE lang = $1 AND lang = $1").expect("tokenize should succeed");
        let substituted = substitute_values(&tokens, &["ja".to_string()]).unwrap();
        assert_eq!(
            substituted,
            vec![
                Token::Keyword(crate::sql::lexer::Keyword::Where),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("ja".to_string()),
                Token::Keyword(crate::sql::lexer::Keyword::And),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("ja".to_string()),
            ]
        );
    }

    #[test]
    fn decode_bind_values_rejects_null() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[None]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_rejects_non_utf8() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[Some(vec![0xff, 0xfe])]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_rejects_embedded_nul() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[Some(b"a\0b".to_vec())]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_accepts_valid_utf8() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let decoded = decode_bind_values(&tokens, &[Some("日本語".as_bytes().to_vec())]).unwrap();
        assert_eq!(decoded, vec!["日本語".to_string()]);
    }

    #[test]
    fn decode_bind_values_rejects_amplified_total_payload_before_cloning_values() {
        // 同一 `$1` を大量に参照させて、1 回の値から巨大な置換結果を作らせる
        // メモリ増幅を、値の複製より前に検出する。`tokenize_with_params` の
        // `MAX_TOKEN_COUNT`（20,000）に収まる範囲（1 参照 = 4 トークン）で、
        // 出現回数 × 値の長さが `lexer::MAX_INPUT_LEN` を超えるようにする。
        let mut sql = "WHERE lang = $1".to_string();
        for _ in 0..4_999 {
            sql.push_str(" AND lang = $1");
        }
        let tokens = tokenize_with_params(&sql).expect("tokenize should succeed");
        let big_value = "x".repeat(300);
        let err = decode_bind_values(&tokens, &[Some(big_value.into_bytes())]).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn decode_bind_values_ignores_unreferenced_values_in_size_and_content() {
        // P0（codex-review・Issue #935 PR #1012 指摘）の回帰: 文が `$2` しか
        // 参照しない場合、`$1` に渡された巨大な値はサイズ集計にも複製にも
        // 一切影響しない（複製前の借用スライスの長さだけで検証するため）。
        // 修正前は全 `values` を検証前に無条件で `to_string()` 複製していた
        // ため、この巨大な未参照値がメモリ増幅攻撃の入力になり得た。
        let tokens = tokenize_with_params("WHERE lang = $2").expect("tokenize should succeed");
        let huge_unreferenced = "x".repeat(lexer::MAX_INPUT_LEN * 4);
        let decoded = decode_bind_values(
            &tokens,
            &[Some(huge_unreferenced.into_bytes()), Some(b"ja".to_vec())],
        )
        .expect("unreferenced oversized value must not affect size validation");

        // 参照されている `$2` の値は正しく複製される。
        assert_eq!(decoded[1], "ja");
        // 参照されていない `$1` の値は複製されない（プレースホルダのまま）。
        assert_eq!(decoded[0], "");
    }

    #[test]
    fn decode_bind_values_still_rejects_null_at_unreferenced_position() {
        // 未参照の位置であっても NULL・非 UTF-8・NUL 混入の検証は全値に対して
        // 行う（fail-closed。参照有無で入力検証の強度を変えない）。
        let tokens = tokenize_with_params("WHERE lang = $2").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[None, Some(b"ja".to_vec())]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_counts_non_param_token_bytes_toward_the_limit() {
        // codex-review P2（Issue #935 PR #1012 指摘）の回帰: 置換後総バイト長の
        // 判定が束縛値（`$n`）の長さのみを合計し、元トークン列に残る非
        // `Param` 部分（ここでは巨大な既存文字列リテラル）を含めないままだと、
        // この文字列リテラルだけで `lexer::MAX_INPUT_LEN` 近くまで達していても
        // 小さな束縛値を足すだけの拒否漏れが起きる（`total` が値の長さのみを
        // 数えていた旧実装ではこのテストは失敗していた）。
        let big_literal_len = lexer::MAX_INPUT_LEN - 64;
        let sql = format!("'{}' AND lang = $1", "x".repeat(big_literal_len));
        assert!(
            sql.len() <= lexer::MAX_INPUT_LEN,
            "test setup must keep the original SQL text within tokenize_with_params's own limit"
        );
        let tokens = tokenize_with_params(&sql).expect("tokenize should succeed");
        // 束縛値自体は小さいが、既存の巨大な文字列リテラルと合算すると
        // `MAX_INPUT_LEN` を超える。
        let small_value = "y".repeat(128);
        let err = decode_bind_values(&tokens, &[Some(small_value.into_bytes())]).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }
}
