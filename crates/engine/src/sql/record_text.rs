//! 再帰 CTE の `SEARCH ... SET` 順序列（`record`／`record[]`。SQL-35・Issue #2213）の
//! テキスト表現の組み立て（PostgreSQL の `record_out`／`array_out` の引用規則）。
//!
//! 責務境界: フィールド値を文字列へ整形する処理（型ごとの書式）は呼び出し元の wire-server
//! （`result_encoder`。公告型に応じた整形と共有）が担い、本モジュールは「整形済みフィールド列から
//! レコード・配列のテキストを作る引用規則」だけを純粋関数として提供する。値の比較は
//! `sql::view_buffered`（`ORDER BY`）が担う。

/// `record_out` / `array_out` が空白として扱う文字（C の `isspace`）。
fn is_pg_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}')
}

/// レコード 1 件を `(f1,f2,...)` へ整形する（`record_out`）。`None`（NULL）は空、フィールドが空文字列・
/// `"`・`\`・`(`・`)`・`,`・空白を含めば `"` で囲み、`"` と `\` は二重化する。
pub fn record_out(fields: &[Option<String>]) -> String {
    let mut out = String::from("(");
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let Some(value) = field else { continue };
        let needs_quote = value.is_empty()
            || value
                .chars()
                .any(|c| matches!(c, '"' | '\\' | '(' | ')' | ',') || is_pg_space(c));
        if needs_quote {
            out.push('"');
        }
        for c in value.chars() {
            if c == '"' || c == '\\' {
                out.push(c);
            }
            out.push(c);
        }
        if needs_quote {
            out.push('"');
        }
    }
    out.push(')');
    out
}

/// 要素が文字列表現のレコードである 1 次元配列を `{e1,e2,...}` へ整形する（`array_out`）。要素が空・
/// `NULL`（大文字小文字不問）・`{`・`}`・`,`・`"`・`\`・空白を含めば `"` で囲み、`"` と `\` は
/// バックスラッシュでエスケープする。空配列は `{}`。
pub fn record_array_out(elements: &[String]) -> String {
    let mut out = String::from("{");
    for (i, element) in elements.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let needs_quote = element.is_empty()
            || element.eq_ignore_ascii_case("NULL")
            || element
                .chars()
                .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || is_pg_space(c));
        if needs_quote {
            out.push('"');
        }
        for c in element.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        if needs_quote {
            out.push('"');
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn record_without_special_characters_is_unquoted() {
        assert_eq!(record_out(&[some("0"), some("1")]), "(0,1)");
        assert_eq!(record_out(&[some("1")]), "(1)");
    }

    #[test]
    fn record_quotes_empty_space_and_delimiters_and_doubles_quote_and_backslash() {
        assert_eq!(record_out(&[some("")]), "(\"\")");
        assert_eq!(record_out(&[some("a b")]), "(\"a b\")");
        assert_eq!(record_out(&[some("a,b")]), "(\"a,b\")");
        assert_eq!(record_out(&[some("(x)")]), "(\"(x)\")");
        assert_eq!(record_out(&[some("a\"b")]), "(\"a\"\"b\")");
        assert_eq!(record_out(&[some("a\\b")]), "(\"a\\\\b\")");
    }

    #[test]
    fn record_null_field_is_empty() {
        assert_eq!(record_out(&[None, some("1")]), "(,1)");
        assert_eq!(record_out(&[some("1"), None]), "(1,)");
    }

    #[test]
    fn array_of_records_quotes_elements_with_commas() {
        let one = record_out(&[some("1")]);
        let two = record_out(&[some("2")]);
        assert_eq!(record_array_out(&[one, two]), "{(1),(2)}");
        let a = record_out(&[some("1"), some("a")]);
        let b = record_out(&[some("2"), some("b")]);
        assert_eq!(record_array_out(&[a, b]), "{\"(1,a)\",\"(2,b)\"}");
    }

    #[test]
    fn array_escapes_quote_and_backslash_inside_quoted_element() {
        let rec = record_out(&[some("a b")]);
        assert_eq!(record_array_out(&[rec]), "{\"(\\\"a b\\\")\"}");
        assert_eq!(record_array_out(&[]), "{}");
        assert_eq!(record_array_out(&["NULL".to_string()]), "{\"NULL\"}");
    }
}
