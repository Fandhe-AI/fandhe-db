//! `COPY` の `WITH` オプション（新構文 `WITH (opt [value], ...)` と旧構文 `CSV HEADER` 等）の
//! 解決・検証（Issue #1763・WIRE-22・TASK-294 のポインタ）。
//!
//! 責務境界: [`crate::sql::allowlist`] の COPY 検証（`validate_copy_from_tokens`／
//! `validate_copy_to_tokens`）がトークン走査で作る生のオプション列 [`RawCopyOption`] を受け取り、
//! 重複・未知名・値の型・モード制約・方向制約を PostgreSQL 17 と同じ分類で検証して、
//! 解決済みの [`CopyOptions`] と [`CopyFormat`] を返す。トークン型（`Parser`）には依存しない。
//!
//! 適用状況: HEADER・DELIMITER・NULL・QUOTE・ESCAPE は両方向とも適用済み
//! （`COPY FROM` は [`crate::sql::copy`]・Issue #1764、`COPY TO` は wire-server の
//! 出力経路・Issue #1765）。解決結果は `CopyPlan` が運び、未適用ゲートは無い。
//! `FORMAT binary` は `COPY FROM` のみ受理する（WIRE-23・Issue #1769。DELIMITER／NULL／
//! HEADER／QUOTE／ESCAPE との併用は `0A000`）。`COPY TO` の binary は Issue #1770 が
//! 実装するまで従来どおり `42601` のまま先送りする。
//!
//! エラー文言にはユーザー入力の値を含めない（`22023`・`0A000` は固定文言。`42601` に含めるのは
//! 切り詰め済みのオプション名だけで、構文エラーのトークンは載せない）。

use super::allowlist::{CopyFormat, SqlSurfaceError};

/// エラー文言に載せるオプション名の最大文字数。
const MAX_OPTION_NAME_ECHO: usize = 32;

/// エラー文言用にオプション名を切り詰める（入力由来の長い識別子を応答へ反映しないため）。
fn truncate_option_name(name: &str) -> String {
    name.chars().take(MAX_OPTION_NAME_ECHO).collect()
}

/// 1 文が持てるオプション数の上限。既知名の数を十分上回る値で、走査中の防御（DoS 対策）に使う。
pub(crate) const MAX_COPY_OPTIONS: usize = 32;

/// 解決済みの `HEADER` 指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyHeader {
    False,
    True,
    /// `HEADER MATCH`（`COPY FROM` 専用）。
    Match,
}

/// 走査段が取り出した生のオプション値。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawCopyValue {
    Ident(String),
    String(String),
    Number(String),
    Star,
    List(Vec<String>),
}

/// 走査段が取り出した生のオプション（名前は小文字に正規化済み）。旧構文の項目も同じ形へ写像する
/// （`CSV` は `format`、`HEADER` は `header`、`FORCE QUOTE` は `force_quote` 等）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawCopyOption {
    pub(crate) name: String,
    pub(crate) value: Option<RawCopyValue>,
}

/// COPY の方向。方向依存の検査（`HEADER MATCH` は `FROM` 専用）に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyDirection {
    From,
    To,
}

/// 検証・既定値適用済みの COPY オプション。フィールドは非公開で、検証を経ない値を
/// クレート外が組み立てられない（読み取りはアクセサ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOptions {
    header: CopyHeader,
    delimiter: u8,
    null_string: String,
    quote: u8,
    escape: u8,
}

impl CopyOptions {
    /// `format` の既定値だけで構成したオプション。
    fn defaults_for(format: CopyFormat) -> Self {
        match format {
            // binary は行形式のオプションを参照しない。ダミーとして text の既定値を置く。
            CopyFormat::Text | CopyFormat::Binary => Self {
                header: CopyHeader::False,
                delimiter: b'\t',
                null_string: "\\N".to_string(),
                quote: b'"',
                escape: b'"',
            },
            CopyFormat::Csv => Self {
                header: CopyHeader::False,
                delimiter: b',',
                null_string: String::new(),
                quote: b'"',
                escape: b'"',
            },
        }
    }

    pub fn header(&self) -> CopyHeader {
        self.header
    }

    pub fn delimiter(&self) -> u8 {
        self.delimiter
    }

    pub fn null_string(&self) -> &str {
        &self.null_string
    }

    pub fn quote(&self) -> u8 {
        self.quote
    }

    pub fn escape(&self) -> u8 {
        self.escape
    }

    /// 解決結果が `format` の既定値と一致するか（一致すれば既存のデータ経路で挙動が変わらない）。
    pub fn is_default_for(&self, format: CopyFormat) -> bool {
        *self == Self::defaults_for(format)
    }
}

/// `format` の既定値だけで構成したオプション（`COPY FROM` のデータ経路の単体テストと、
/// オプション未指定を表す呼び出し元が使う）。
#[cfg(test)]
pub(crate) fn defaults_for_format(format: CopyFormat) -> CopyOptions {
    CopyOptions::defaults_for(format)
}

fn invalid(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::InvalidParameterValue { detail }
}

fn not_supported(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

const KNOWN_OPTIONS: &[&str] = &[
    "format",
    "header",
    "delimiter",
    "null",
    "quote",
    "escape",
    "encoding",
    "force_quote",
    "force_not_null",
    "force_null",
    "default",
    "on_error",
    "freeze",
    "log_verbosity",
];

/// 値が必須のオプション（値なしは PostgreSQL の "requires a parameter" 相当）。
const VALUE_REQUIRED: &[&str] = &[
    "format",
    "delimiter",
    "null",
    "quote",
    "escape",
    "encoding",
    // 対象外（0A000）のオプションも、値なしは先に 42601 とする（値欠落が最優先の契約）。
    "force_quote",
    "force_not_null",
    "force_null",
    "default",
    "on_error",
    "log_verbosity",
];

/// 本 Issue では解決せず、認識したうえで `0A000` にするオプション。
const UNSUPPORTED_OPTIONS: &[&str] = &[
    "force_quote",
    "force_not_null",
    "force_null",
    "default",
    "on_error",
    "freeze",
    "log_verbosity",
];

/// 生のオプション列を検証し、解決済みの format とオプションを返す。検証順序は決定的
/// （重複・未知・値欠落 → format → 値の型 → 対象外 → モード・制約 → 方向）。
/// 構文エラーは走査段（`allowlist`）が先に `42601` で返す。
pub(crate) fn resolve(
    raw: Vec<RawCopyOption>,
    direction: CopyDirection,
) -> Result<(CopyFormat, CopyOptions), SqlSurfaceError> {
    if raw.len() > MAX_COPY_OPTIONS {
        return Err(SqlSurfaceError::unsupported("too many COPY options"));
    }

    // 第 1 段: 重複・未知名・値の欠落。
    let mut seen: Vec<&str> = Vec::new();
    for opt in &raw {
        if !KNOWN_OPTIONS.contains(&opt.name.as_str()) {
            return Err(SqlSurfaceError::unsupported(format!(
                "unknown COPY option: {}",
                truncate_option_name(&opt.name)
            )));
        }
        if seen.contains(&opt.name.as_str()) {
            return Err(SqlSurfaceError::unsupported(format!(
                "conflicting or redundant COPY option: {}",
                truncate_option_name(&opt.name)
            )));
        }
        seen.push(opt.name.as_str());
        if opt.value.is_none() && VALUE_REQUIRED.contains(&opt.name.as_str()) {
            return Err(SqlSurfaceError::unsupported(format!(
                "COPY option requires a parameter: {}",
                opt.name
            )));
        }
    }
    let find = |name: &str| raw.iter().find(|o| o.name == name);

    // 第 2 段: format（他オプションの出現順に依存しない）。
    let format = match find("format").and_then(|o| o.value.as_ref()) {
        None => CopyFormat::Text,
        Some(RawCopyValue::Ident(s)) | Some(RawCopyValue::String(s)) => {
            match s.to_ascii_lowercase().as_str() {
                "text" => CopyFormat::Text,
                "csv" => CopyFormat::Csv,
                // binary は COPY FROM のみ受理する（WIRE-23・Issue #1769）。COPY TO は
                // Issue #1770 が実装するまで従来どおり 42601 で先送りする。
                "binary" if direction == CopyDirection::From => CopyFormat::Binary,
                "binary" => {
                    return Err(SqlSurfaceError::unsupported(
                        "COPY FORMAT binary is not supported",
                    ))
                }
                _ => return Err(invalid("COPY format is not recognized")),
            }
        }
        Some(_) => return Err(invalid("COPY format is not recognized")),
    };

    // 値の型の検証。
    let header = match find("header") {
        None => CopyHeader::False,
        Some(o) => parse_header(o.value.as_ref())?,
    };
    let delimiter_s = string_value(find("delimiter"))?;
    let null_s = string_value(find("null"))?;
    let quote_s = string_value(find("quote"))?;
    let escape_s = string_value(find("escape"))?;
    let encoding_s = string_value(find("encoding"))?;

    // 対象外のオプションは認識したうえで 0A000。
    if raw
        .iter()
        .any(|o| UNSUPPORTED_OPTIONS.contains(&o.name.as_str()))
    {
        return Err(not_supported("COPY option is not supported"));
    }
    if let Some(enc) = encoding_s {
        let e = enc.to_ascii_uppercase();
        if e != "UTF8" && e != "UTF-8" {
            return Err(not_supported("COPY encoding is not supported"));
        }
    }

    // binary は行形式のオプションを持たない（PostgreSQL 17 の ProcessCopyOptions と同様に、
    // 既定値を入れる前に併用を拒否する。WIRE-23・Issue #1769。エラー文言に値は含めない）。
    if format == CopyFormat::Binary {
        if delimiter_s.is_some() {
            return Err(not_supported("cannot specify DELIMITER in BINARY mode"));
        }
        if null_s.is_some() {
            return Err(not_supported("cannot specify NULL in BINARY mode"));
        }
        if header != CopyHeader::False {
            return Err(not_supported("cannot specify HEADER in BINARY mode"));
        }
        if quote_s.is_some() {
            return Err(not_supported("COPY QUOTE requires CSV mode"));
        }
        if escape_s.is_some() {
            return Err(not_supported("COPY ESCAPE requires CSV mode"));
        }
    }

    // 既定値の適用とモード・制約の検査（PostgreSQL 17 の ProcessCopyOptions と同順）。
    let defaults = CopyOptions::defaults_for(format);
    let delimiter = match delimiter_s {
        Some(d) => single_byte(d),
        None => Some(defaults.delimiter),
    }
    .ok_or_else(|| not_supported("COPY delimiter must be a single one-byte character"))?;
    if delimiter == b'\n' || delimiter == b'\r' {
        return Err(invalid(
            "COPY delimiter cannot be newline or carriage return",
        ));
    }
    let null_string = null_s.map(str::to_string).unwrap_or(defaults.null_string);
    if null_string.contains(['\n', '\r']) {
        return Err(invalid(
            "COPY null representation cannot use newline or carriage return",
        ));
    }
    if format == CopyFormat::Text
        && (delimiter == b'\\'
            || delimiter == b'.'
            || delimiter.is_ascii_lowercase()
            || delimiter.is_ascii_digit())
    {
        return Err(invalid("COPY delimiter character is not allowed"));
    }
    if header == CopyHeader::Match && direction == CopyDirection::To {
        return Err(not_supported("COPY HEADER MATCH is only for COPY FROM"));
    }
    if format == CopyFormat::Text && quote_s.is_some() {
        return Err(not_supported("COPY QUOTE requires CSV mode"));
    }
    let quote = match quote_s {
        Some(q) => single_byte(q)
            .ok_or_else(|| not_supported("COPY quote must be a single one-byte character"))?,
        None => defaults.quote,
    };
    if format == CopyFormat::Text && escape_s.is_some() {
        return Err(not_supported("COPY ESCAPE requires CSV mode"));
    }
    let escape = match escape_s {
        Some(e) => single_byte(e)
            .ok_or_else(|| not_supported("COPY escape must be a single one-byte character"))?,
        // ESCAPE の既定値は解決後の QUOTE。
        None => quote,
    };
    if format == CopyFormat::Csv && delimiter == quote {
        return Err(invalid("COPY delimiter and quote must be different"));
    }
    if null_string.as_bytes().contains(&delimiter) {
        return Err(invalid(
            "COPY delimiter must not appear in the NULL specification",
        ));
    }
    if format == CopyFormat::Csv && null_string.as_bytes().contains(&quote) {
        return Err(invalid(
            "COPY quote must not appear in the NULL specification",
        ));
    }

    Ok((
        format,
        CopyOptions {
            header,
            delimiter,
            null_string,
            quote,
            escape,
        },
    ))
}

/// 1 バイトの文字列だけを `u8` にする（マルチバイト文字・空文字列は `None`）。
fn single_byte(s: &str) -> Option<u8> {
    match s.as_bytes() {
        [b] => Some(*b),
        _ => None,
    }
}

/// 文字列系オプションの値を取り出す。PostgreSQL の `copy_generic_opt_arg` と `defGetString` に
/// 合わせ、文字列リテラル・引用符なしの識別子・数値を受理する（`*`・列リストは `22023`）。
fn string_value(opt: Option<&RawCopyOption>) -> Result<Option<&str>, SqlSurfaceError> {
    match opt.and_then(|o| o.value.as_ref()) {
        None => Ok(None),
        Some(RawCopyValue::String(s))
        | Some(RawCopyValue::Ident(s))
        | Some(RawCopyValue::Number(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(invalid("COPY option value must be a string")),
    }
}

/// `HEADER` の値（省略は true。PostgreSQL の真偽値表記と `match`。大小を区別しない）。
fn parse_header(value: Option<&RawCopyValue>) -> Result<CopyHeader, SqlSurfaceError> {
    let text = match value {
        None => return Ok(CopyHeader::True),
        Some(RawCopyValue::Ident(s))
        | Some(RawCopyValue::String(s))
        | Some(RawCopyValue::Number(s)) => s.to_ascii_lowercase(),
        Some(_) => return Err(invalid("COPY HEADER requires a Boolean value or match")),
    };
    if text == "match" {
        return Ok(CopyHeader::Match);
    }
    match parse_bool(&text) {
        Some(true) => Ok(CopyHeader::True),
        Some(false) => Ok(CopyHeader::False),
        None => Err(invalid("COPY HEADER requires a Boolean value or match")),
    }
}

/// PostgreSQL の `parse_bool` と同じ規則の真偽値解釈（入力は小文字化済み）。
/// `true`・`false`・`yes`・`no` は空でない接頭辞を許し、`on`・`off` は 2 文字以上の接頭辞
/// （`o` 単独は曖昧なので不正）、`1`・`0` は完全一致のみ。`parse_header` から呼ぶ。
fn parse_bool(text: &str) -> Option<bool> {
    let is_prefix = |full: &str| !text.is_empty() && full.starts_with(text);
    if is_prefix("true") || is_prefix("yes") || text == "1" {
        return Some(true);
    }
    if is_prefix("false") || is_prefix("no") || text == "0" {
        return Some(false);
    }
    if text.len() >= 2 {
        if "on".starts_with(text) {
            return Some(true);
        }
        if "off".starts_with(text) {
            return Some(false);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, value: Option<RawCopyValue>) -> RawCopyOption {
        RawCopyOption {
            name: name.to_string(),
            value,
        }
    }
    fn s(v: &str) -> Option<RawCopyValue> {
        Some(RawCopyValue::String(v.to_string()))
    }
    fn id(v: &str) -> Option<RawCopyValue> {
        Some(RawCopyValue::Ident(v.to_string()))
    }
    fn code(v: Vec<RawCopyOption>, d: CopyDirection) -> &'static str {
        resolve(v, d).expect_err("must be rejected").wire_code()
    }
    fn ok(v: Vec<RawCopyOption>) -> (CopyFormat, CopyOptions) {
        resolve(v, CopyDirection::From).expect("must resolve")
    }

    #[test]
    fn defaults_for_text_and_csv() {
        let (f, o) = ok(vec![]);
        assert_eq!(f, CopyFormat::Text);
        assert_eq!(o.delimiter(), b'\t');
        assert_eq!(o.null_string(), "\\N");
        assert!(o.is_default_for(f));
        let (f, o) = ok(vec![raw("format", id("CSV"))]);
        assert_eq!(f, CopyFormat::Csv);
        assert_eq!(o.delimiter(), b',');
        assert_eq!(o.null_string(), "");
        assert_eq!((o.quote(), o.escape()), (b'"', b'"'));
        assert!(o.is_default_for(f));
    }

    #[test]
    fn escape_defaults_to_resolved_quote_and_order_is_irrelevant() {
        let (_, o) = ok(vec![raw("quote", s("'")), raw("format", id("csv"))]);
        assert_eq!((o.quote(), o.escape()), (b'\'', b'\''));
        let (_, o) = ok(vec![
            raw("delimiter", s(";")),
            raw("format", s("csv")),
            raw("header", id("match")),
        ]);
        assert_eq!(o.delimiter(), b';');
        assert_eq!(o.header(), CopyHeader::Match);
    }

    #[test]
    fn header_values() {
        for (v, want) in [
            ("true", CopyHeader::True),
            ("ON", CopyHeader::True),
            ("false", CopyHeader::False),
            ("off", CopyHeader::False),
        ] {
            assert_eq!(ok(vec![raw("header", id(v))]).1.header(), want);
        }
        let one = Some(RawCopyValue::Number("1".to_string()));
        assert_eq!(ok(vec![raw("header", one)]).1.header(), CopyHeader::True);
        assert_eq!(ok(vec![raw("header", None)]).1.header(), CopyHeader::True);
        assert_eq!(
            code(vec![raw("header", id("maybe"))], CopyDirection::From),
            "22023"
        );
    }

    #[test]
    fn duplicate_unknown_and_missing_value_are_42601() {
        let dup = vec![raw("header", None), raw("header", None)];
        assert_eq!(code(dup, CopyDirection::From), "42601");
        let unk = vec![raw("header", None), raw("foo", None)];
        assert_eq!(code(unk, CopyDirection::From), "42601");
        let nov = vec![raw("delimiter", None)];
        assert_eq!(code(nov, CopyDirection::From), "42601");
        let many: Vec<_> = (0..=MAX_COPY_OPTIONS)
            .map(|_| raw("header", None))
            .collect();
        assert_eq!(code(many, CopyDirection::From), "42601");
    }

    #[test]
    fn header_accepts_postgres_boolean_spellings() {
        let h = |v: &str| resolve(vec![raw("header", s(v))], CopyDirection::From);
        for t in ["t", "tr", "true", "y", "yes", "on", "1", "ON", "Yes"] {
            assert_eq!(h(t).expect(t).1.header(), CopyHeader::True, "{t}");
        }
        for f in ["f", "fa", "false", "n", "no", "of", "off", "0", "OFF"] {
            assert_eq!(h(f).expect(f).1.header(), CopyHeader::False, "{f}");
        }
        assert_eq!(h("match").expect("match").1.header(), CopyHeader::Match);
        for bad in ["o", "", "x", "truex", "yess", "2", "onn", "secretbool"] {
            let e = h(bad).expect_err(bad);
            assert_eq!(e.wire_code(), "22023", "{bad}");
            assert!(!e.to_string().contains("secretbool"), "{e}");
        }
    }

    #[test]
    fn string_options_accept_identifier_and_number_values() {
        for v in [id("UTF8"), id("utf8"), s("UTF8")] {
            ok(vec![raw("encoding", v)]);
        }
        let latin = |v| code(vec![raw("encoding", v)], CopyDirection::From);
        assert_eq!(latin(id("latin1")), "0A000");
        assert_eq!(latin(Some(RawCopyValue::Star)), "22023");
        assert_eq!(latin(Some(RawCopyValue::List(vec!["a".into()]))), "22023");
        let (_, o) = ok(vec![
            raw("format", id("csv")),
            raw("delimiter", Some(RawCopyValue::Number("1".into()))),
            raw("null", id("nil")),
        ]);
        assert_eq!((o.delimiter(), o.null_string()), (b'1', "nil"));
    }

    #[test]
    fn format_errors() {
        let f = |v| vec![raw("format", id(v))];
        assert_eq!(code(f("json"), CopyDirection::From), "22023");
        assert_eq!(code(f("binary"), CopyDirection::To), "42601");
    }

    #[test]
    fn binary_is_accepted_for_from_and_rejects_row_format_options() {
        let (f, _) = ok(vec![raw("format", id("binary"))]);
        assert_eq!(f, CopyFormat::Binary);
        let b = || raw("format", id("binary"));
        for opt in [
            raw("delimiter", s(",")),
            raw("null", s("x")),
            raw("header", None),
            raw("quote", s("'")),
            raw("escape", s("\\")),
        ] {
            assert_eq!(
                code(vec![b(), opt.clone()], CopyDirection::From),
                "0A000",
                "{opt:?}"
            );
        }
        // HEADER false は指定なしと同じ。ENCODING UTF8 は従来どおり受理する。
        ok(vec![
            b(),
            raw("header", id("false")),
            raw("encoding", s("UTF8")),
        ]);
    }

    #[test]
    fn unsupported_options_are_0a000_and_utf8_encoding_is_accepted() {
        for name in [
            "force_quote",
            "force_not_null",
            "freeze",
            "on_error",
            "default",
        ] {
            let v = vec![
                raw("format", id("csv")),
                raw(name, Some(RawCopyValue::Star)),
            ];
            assert_eq!(code(v, CopyDirection::From), "0A000", "{name}");
        }
        let latin = vec![raw("encoding", s("LATIN1"))];
        assert_eq!(code(latin, CopyDirection::To), "0A000");
        ok(vec![raw("encoding", s("utf-8"))]);
        ok(vec![raw("encoding", s("UTF8"))]);
    }

    #[test]
    fn mode_and_constraint_checks() {
        let csv = || raw("format", id("csv"));
        let cases: Vec<(Vec<RawCopyOption>, &str)> = vec![
            (vec![raw("quote", s("'"))], "0A000"),
            (vec![raw("escape", s("\\"))], "0A000"),
            (vec![raw("delimiter", s(";;"))], "0A000"),
            (vec![raw("delimiter", s("é"))], "0A000"),
            (vec![raw("delimiter", s(""))], "0A000"),
            (vec![csv(), raw("quote", s("ab"))], "0A000"),
            (vec![raw("delimiter", s("\n"))], "22023"),
            (vec![raw("null", s("a\nb"))], "22023"),
            (vec![raw("delimiter", s("a"))], "22023"),
            (vec![raw("delimiter", s("\\"))], "22023"),
            (vec![csv(), raw("delimiter", s("\""))], "22023"),
            (vec![csv(), raw("null", s("a,b"))], "22023"),
            (vec![csv(), raw("null", s("a\"b"))], "22023"),
        ];
        for (v, want) in cases {
            assert_eq!(code(v.clone(), CopyDirection::From), want, "{v:?}");
        }
        let m = vec![raw("header", id("match"))];
        assert_eq!(code(m.clone(), CopyDirection::To), "0A000");
        resolve(m, CopyDirection::From).expect("MATCH is allowed for FROM");
    }
}
