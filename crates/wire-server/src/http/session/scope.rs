//! セッション発行 `POST /v1/session` の任意キー `scope`（op・表・読み取り専用の 3 軸で
//! トークンの権限を狭める宣言）の型・スキーマ・パーサ（Issue #2100・HTTP-15。親 #2099。
//! ポインタ: `docs/spec/04-behavior/http-transport.md` HTTP-15。関連 HTTP-4・HTTP-7・ERR-4・NOSQL-13）。
//!
//! 呼び出し文脈: [`super::issue`] が本文のスキーマ検証直後に [`parse_scope`] を呼び、形の不正を
//! `42601`、表名件数の超過を `54000` で拒否する。受理した [`SessionScope`] は
//! [`super::store::SessionStore::issue_with_scope`] でトークンに束縛され、
//! [`super::middleware::SessionPrincipal::scope`] から読める。本モジュールは**運ぶだけ**で、
//! 要求ごとの強制（`42501`）は後続 Issue #2101 の担当。
//!
//! 設計の要点:
//! - `ops` の語彙は [`Op::parse`] を単一情報源にする（第 2 の語彙表を作らない）
//! - `tables` は [`check_identifier`] で形だけを検証し、カタログ・engine には照会しない
//!   （存在しない表・他テナントの表を含めても応答が変わらない＝存在の秘匿）
//! - 表名は大文字小文字を区別して保持する（NoSQL の `table` と同じ完全一致照合）
//! - 型は不変で、setter・書き換え経路を持たない。`Debug` は伏せ字（表名の漏えい防止）
//! - 受信データ経路のため `unwrap`／`expect`／添字アクセスを使わない

use std::collections::BTreeSet;
use std::fmt;

use engine::error_format::ErrorClass;
use engine::json::JsonValue;

use crate::http::query::ident::check_identifier;
use crate::http::query::op::Op;
use crate::http::query::schema::{
    ElementType, FieldSpec, FieldType, ObjectSchema, Presence, Validated,
};
use crate::limits::MAX_SCOPE_TABLES;

/// 発行時に確定し以後変化しないスコープ宣言。`None` の軸は「その軸では制限なし」。
#[derive(Clone, PartialEq, Eq)]
pub struct SessionScope {
    ops: Option<BTreeSet<Op>>,
    tables: Option<BTreeSet<String>>,
    read_only: bool,
}

impl SessionScope {
    /// 許可する op の集合（`None` は op による制限なし。空集合は「どの op も許さない」）。
    pub fn ops(&self) -> Option<&BTreeSet<Op>> {
        self.ops.as_ref()
    }

    /// 許可する表名の集合（`None` は表による制限なし。完全一致で照合する）。
    pub fn tables(&self) -> Option<&BTreeSet<String>> {
        self.tables.as_ref()
    }

    /// 読み取り専用の宣言か。
    pub fn read_only(&self) -> bool {
        self.read_only
    }
}

impl fmt::Debug for SessionScope {
    /// 表名はテナント内のオブジェクト名のため印字しない（ログ経由の存在情報漏えい防止）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionScope(<redacted>)")
    }
}

/// `scope` オブジェクトのスキーマ。未知キー・`null`・型不一致は一般則で拒否される。
pub static SCOPE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "scope",
    fields: &[
        FieldSpec {
            key: "ops",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "tables",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "read_only",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
    ],
};

/// [`parse_scope`] の拒否理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeError {
    /// 形の不正（未知の op 名・不正な表名・型不一致）。`42601`。
    Invalid,
    /// `tables` の件数が [`MAX_SCOPE_TABLES`] を超えた。`54000`。
    TooManyTables,
}

impl ScopeError {
    /// この拒否の `wire_code` 分類。
    pub const fn error_class(&self) -> ErrorClass {
        match self {
            ScopeError::Invalid => ErrorClass::UnsupportedSqlSyntax,
            ScopeError::TooManyTables => ErrorClass::PayloadTooLarge,
        }
    }

    /// クライアント向け固定文言（キー名・op 名・表名を含めない）。
    pub const fn client_message(&self) -> &'static str {
        match self {
            ScopeError::Invalid => "invalid session request",
            ScopeError::TooManyTables => "too many tables in session scope",
        }
    }
}

/// 検証済みの発行本文から `scope` を取り出す。省略は `Ok(None)`（全権限）。
///
/// `tables` は確保より前に配列長を [`MAX_SCOPE_TABLES`] と比較する。`ops` は語彙が閉じており
/// 重複除去で有界になるため走査のみ（生の配列長は本文上限で有界）。
pub fn parse_scope(validated: &Validated<'_>) -> Result<Option<SessionScope>, ScopeError> {
    let Some(scope) = validated
        .optional_nested("scope")
        .map_err(|_| ScopeError::Invalid)?
    else {
        return Ok(None);
    };

    let ops = match scope
        .optional_array("ops")
        .map_err(|_| ScopeError::Invalid)?
    {
        None => None,
        Some(items) => {
            let mut set = BTreeSet::new();
            for item in items {
                let JsonValue::String(name) = item else {
                    return Err(ScopeError::Invalid);
                };
                set.insert(Op::parse(name).ok_or(ScopeError::Invalid)?);
            }
            Some(set)
        }
    };

    let tables = match scope
        .optional_array("tables")
        .map_err(|_| ScopeError::Invalid)?
    {
        None => None,
        Some(items) => {
            if items.len() > MAX_SCOPE_TABLES {
                return Err(ScopeError::TooManyTables);
            }
            let mut set = BTreeSet::new();
            for item in items {
                let JsonValue::String(name) = item else {
                    return Err(ScopeError::Invalid);
                };
                check_identifier(name).map_err(|_| ScopeError::Invalid)?;
                set.insert(name.clone());
            }
            Some(set)
        }
    };

    let read_only = scope
        .optional_bool("read_only")
        .map_err(|_| ScopeError::Invalid)?
        .unwrap_or(false);

    Ok(Some(SessionScope {
        ops,
        tables,
        read_only,
    }))
}

/// 他モジュールの単体テストが `SessionScope` を組み立てるための入口（本番コードからは使えない）。
#[cfg(test)]
pub(crate) fn scope_for_test(tables: &[&str], read_only: bool) -> SessionScope {
    SessionScope {
        ops: None,
        tables: Some(tables.iter().map(|t| t.to_string()).collect()),
        read_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::json::parse_json;

    static ROOT: ObjectSchema = ObjectSchema {
        name: "session",
        fields: &[FieldSpec {
            key: "scope",
            presence: Presence::Optional,
            ty: FieldType::Object(&SCOPE_SCHEMA),
            nullable: false,
        }],
    };

    fn parse(json: &str) -> Result<Option<SessionScope>, ScopeError> {
        let value = parse_json(json).expect("valid json");
        let validated = ROOT.validate(&value).map_err(|_| ScopeError::Invalid)?;
        parse_scope(&validated)
    }

    #[test]
    fn omitted_scope_is_none() {
        assert_eq!(parse("{}"), Ok(None));
    }

    #[test]
    fn full_scope_is_parsed_and_deduplicated() {
        let s = parse(
            r#"{"scope":{"ops":["scan","search","scan"],"tables":["docs","docs","Notes"],"read_only":true}}"#,
        )
        .expect("ok")
        .expect("some");
        let ops = s.ops().expect("ops");
        assert_eq!(ops.len(), 2);
        assert!(ops.contains(&Op::Scan) && ops.contains(&Op::Search));
        let tables: Vec<&str> = s
            .tables()
            .expect("tables")
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(tables, vec!["Notes", "docs"]);
        assert!(s.read_only());
    }

    #[test]
    fn partial_and_empty_scopes() {
        let s = parse(r#"{"scope":{}}"#).expect("ok").expect("some");
        assert!(s.ops().is_none() && s.tables().is_none() && !s.read_only());
        let s = parse(r#"{"scope":{"ops":[]}}"#).expect("ok").expect("some");
        assert_eq!(s.ops().map(BTreeSet::len), Some(0));
        let s = parse(r#"{"scope":{"read_only":false}}"#)
            .expect("ok")
            .expect("some");
        assert!(!s.read_only());
    }

    #[test]
    fn malformed_scopes_are_invalid() {
        for json in [
            r#"{"scope":{"ops":["create_index"]}}"#,
            r#"{"scope":{"ops":["Scan"]}}"#,
            r#"{"scope":{"ops":[""]}}"#,
            r#"{"scope":{"ops":"scan"}}"#,
            r#"{"scope":{"ops":[1]}}"#,
            r#"{"scope":{"tables":[1]}}"#,
            r#"{"scope":{"tables":"docs"}}"#,
            r#"{"scope":{"read_only":"yes"}}"#,
            r#"{"scope":{"extra":1}}"#,
            r#"{"scope":null}"#,
            r#"{"scope":[]}"#,
            r#"{"scope":"scan"}"#,
            r##"{"scope":{"tables":["#1.t"]}}"##,
            r#"{"scope":{"tables":["s.t"]}}"#,
            r#"{"scope":{"tables":[""]}}"#,
        ] {
            assert_eq!(parse(json), Err(ScopeError::Invalid), "{json}");
        }
        let long = "a".repeat(64);
        let json = format!(r#"{{"scope":{{"tables":["{long}"]}}}}"#);
        assert_eq!(parse(&json), Err(ScopeError::Invalid));
    }

    #[test]
    fn table_count_limit_is_enforced_before_allocation() {
        let names = |n: usize| {
            (0..n)
                .map(|i| format!("\"t{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        };
        let ok = format!(r#"{{"scope":{{"tables":[{}]}}}}"#, names(MAX_SCOPE_TABLES));
        assert!(parse(&ok).expect("at limit").is_some());
        let over = format!(
            r#"{{"scope":{{"tables":[{}]}}}}"#,
            names(MAX_SCOPE_TABLES + 1)
        );
        assert_eq!(parse(&over), Err(ScopeError::TooManyTables));
        assert_eq!(
            ScopeError::TooManyTables.error_class(),
            ErrorClass::PayloadTooLarge
        );
    }

    #[test]
    fn debug_does_not_leak_table_names() {
        let s = parse(r#"{"scope":{"tables":["secret_table"]}}"#)
            .expect("ok")
            .expect("some");
        assert!(!format!("{s:?}").contains("secret_table"));
    }
}
