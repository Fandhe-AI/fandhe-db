//! 論理ダンプの「復元専用の指示」の描画と厳格な解析（RECOVER-16・Issue #3096。親モジュール: [`super`]）。
//!
//! 責務境界: ロール・権限の状態（メンバーシップ・所有者・ACL・既定権限・初期権限・旧形式ハッシュ・
//! ブートストラップ済みマーカー）は SQL の `GRANT` では付与者・所有者・実体化印を忠実に戻せないため、
//! ダンプには SQL ではない指示文（`RESTORE ...`）として書き出す。本モジュールは指示文の文法の唯一の定義で、
//! 書き出し側（[`super::privilege`]・[`super::access`]）が [`render_*`](self) で組み立て、復元側
//! （[`super::restore_apply`]）が [`parse`] で読む。SQL 表層は `RESTORE` を受理しないため、誤って SQL として
//! 流されても字句の段階で拒否される（安全側）。
//!
//! 入力の扱い: 指示文は untrusted なダンプファイル由来。語は大文字固定の完全一致、識別子・文字列は
//! 引用の規則（`""`・`''` の二重化）だけを受け付け、余分な語・未知の語・権限語の重複・NUL は拒否する。
//! 失敗は [`DirectiveError`]（固定文言。入力の断片を含めない）。値にパスワード検証子が入りうるため
//! [`Directive`] の `Debug` は中身を伏せる。

use std::fmt;

use super::super::privilege::default_acl::DefaultAclKind;
use super::super::privilege::{
    PRIV_CREATE, PRIV_DELETE, PRIV_EXECUTE, PRIV_INSERT, PRIV_MAINTAIN, PRIV_REFERENCES,
    PRIV_SELECT, PRIV_TRIGGER, PRIV_TRUNCATE, PRIV_UPDATE, PRIV_USAGE,
};
use super::quote::{ident, literal};
use super::{unsupported, SchemaDdlError};

/// 指示文の先頭の語（書き出す種別ごとの接頭辞は [`prefix_is_allowed`] が持つ）。
pub const DIRECTIVE_KEYWORD: &str = "RESTORE";

/// 権限ビットと語の対応（描画・解析の唯一の表。全ビットを網羅していることは単体試験で固定する）。
const PRIVILEGE_WORDS: [(u16, &str); 11] = [
    (PRIV_SELECT, "SELECT"),
    (PRIV_INSERT, "INSERT"),
    (PRIV_UPDATE, "UPDATE"),
    (PRIV_DELETE, "DELETE"),
    (PRIV_TRUNCATE, "TRUNCATE"),
    (PRIV_REFERENCES, "REFERENCES"),
    (PRIV_TRIGGER, "TRIGGER"),
    (PRIV_MAINTAIN, "MAINTAIN"),
    (PRIV_USAGE, "USAGE"),
    (PRIV_CREATE, "CREATE"),
    (PRIV_EXECUTE, "EXECUTE"),
];

/// 既定権限の対象種別と語の対応。
const KIND_WORDS: [(DefaultAclKind, &str); 4] = [
    (DefaultAclKind::Tables, "TABLES"),
    (DefaultAclKind::Sequences, "SEQUENCES"),
    (DefaultAclKind::Functions, "FUNCTIONS"),
    (DefaultAclKind::Schemas, "SCHEMAS"),
];

/// 指示文の解析の失敗（固定文言。入力の断片・名前を含まない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectiveError;

impl fmt::Display for DirectiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed restore directive")
    }
}

impl std::error::Error for DirectiveError {}

/// relation 名（`public` は `schema` が `None`、SCHEMA 配下は `Some`）。
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Rel {
    pub(super) schema: Option<String>,
    pub(super) name: String,
}

/// 権限・所有者の対象。
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Obj {
    Table(Rel),
    Column(Rel, String),
    Schema(String),
    /// 共有（`public`）の SEQUENCE の名前。
    Sequence(String),
    /// `名前(型, ...)` の署名（描画は書き出し側と復元側で同じ関数を使う）。
    Function(String),
}

/// 解析済みの指示。名前のままで、ロール ID などへの解決は復元側が自テナント内で行う。
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Directive {
    RoleLegacy {
        role: String,
        phc: String,
    },
    RoleBootstrap {
        role: String,
        all_objects: bool,
        ddl: bool,
    },
    BootstrapMarker,
    Membership {
        role: String,
        member: String,
        /// `None` は付与者なし（版 1 の行）。
        grantor: Option<String>,
        /// 所有者相当を根拠とした付与か（版 3）。
        root: bool,
        admin: bool,
        inherit: bool,
        set: bool,
    },
    Owner {
        obj: Obj,
        owner: String,
    },
    Acl {
        obj: Obj,
        /// `None` は PUBLIC。
        grantee: Option<String>,
        grantor: String,
        privileges: u16,
        options: u16,
    },
    AclInit {
        function: String,
    },
    DefaultAcl {
        role: String,
        /// `None` は全体（`IN ALL`）。
        schema: Option<String>,
        kind: DefaultAclKind,
        grantee: Option<String>,
        privileges: u16,
        options: u16,
    },
    DefaultAclInit {
        role: String,
    },
}

impl fmt::Debug for Directive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Directive(<redacted>)")
    }
}

/// 書き出す種別ごとの、本文が取りうる先頭の語句（復元側の許可表の根拠）。
pub fn prefix_is_allowed(kind: PrefixKind, text: &str) -> bool {
    let prefixes: &[&str] = match kind {
        PrefixKind::RoleBootstrap => &["RESTORE ROLE ", "RESTORE BOOTSTRAP MARKER"],
        PrefixKind::RoleMembership => &["RESTORE MEMBERSHIP "],
        PrefixKind::Owner => &["RESTORE OWNER "],
        PrefixKind::Privilege => &["RESTORE ACL "],
        PrefixKind::DefaultPrivilege => &["RESTORE DEFAULT ACL "],
    };
    prefixes.iter().any(|p| text.starts_with(p))
}

/// 指示文の種別（wire-server のダンプファイルの枠の種別と対応する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixKind {
    RoleBootstrap,
    RoleMembership,
    Owner,
    Privilege,
    DefaultPrivilege,
}

// ---- 描画 ----------------------------------------------------------------------------------

fn words_of(mask: u16) -> Result<String, ()> {
    let known: u16 = PRIVILEGE_WORDS.iter().fold(0, |a, (b, _)| a | b);
    if mask == 0 || mask & !known != 0 {
        return Err(());
    }
    let words: Vec<&str> = PRIVILEGE_WORDS
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, w)| *w)
        .collect();
    Ok(words.join(", "))
}

fn bad_privilege<E>() -> SchemaDdlError<E> {
    unsupported("privilege", "privilege bits cannot be represented")
}

fn rel_text<E>(r: &Rel) -> Result<String, SchemaDdlError<E>> {
    Ok(match &r.schema {
        Some(s) => format!("{}.{}", ident::<E>(s)?, ident::<E>(&r.name)?),
        None => ident::<E>(&r.name)?,
    })
}

fn obj_text<E>(o: &Obj) -> Result<String, SchemaDdlError<E>> {
    Ok(match o {
        Obj::Table(r) => format!("TABLE {}", rel_text::<E>(r)?),
        Obj::Column(r, c) => format!("COLUMN {} {}", rel_text::<E>(r)?, ident::<E>(c)?),
        Obj::Schema(s) => format!("SCHEMA {}", ident::<E>(s)?),
        Obj::Sequence(s) => format!("SEQUENCE {}", ident::<E>(s)?),
        Obj::Function(sig) => format!("FUNCTION {}", literal::<E>(sig)?),
    })
}

fn grantee_text<E>(g: &Option<String>) -> Result<String, SchemaDdlError<E>> {
    match g {
        Some(n) => ident::<E>(n),
        None => Ok("PUBLIC".to_string()),
    }
}

fn with_options<E>(mut s: String, options: u16) -> Result<String, SchemaDdlError<E>> {
    if options != 0 {
        s.push_str(" OPTION ");
        s.push_str(&words_of(options).map_err(|_| bad_privilege())?);
    }
    Ok(s)
}

pub(super) fn render_role_legacy<E>(role: &str, phc: &str) -> Result<String, SchemaDdlError<E>> {
    Ok(format!(
        "RESTORE ROLE {} LEGACY PASSWORD {}",
        ident::<E>(role)?,
        literal::<E>(phc)?
    ))
}

pub(super) fn render_role_bootstrap<E>(
    role: &str,
    all_objects: bool,
    ddl: bool,
) -> Result<String, SchemaDdlError<E>> {
    let mut words = Vec::new();
    if all_objects {
        words.push("ALL_OBJECTS");
    }
    if ddl {
        words.push("DDL");
    }
    if words.is_empty() {
        return Err(unsupported("role", "empty bootstrap privileges"));
    }
    Ok(format!(
        "RESTORE ROLE {} BOOTSTRAP PRIVILEGES {}",
        ident::<E>(role)?,
        words.join(", ")
    ))
}

pub(super) fn render_bootstrap_marker() -> String {
    "RESTORE BOOTSTRAP MARKER".to_string()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_membership<E>(
    role: &str,
    member: &str,
    grantor: Option<&str>,
    root: bool,
    admin: bool,
    inherit: bool,
    set: bool,
) -> Result<String, SchemaDdlError<E>> {
    let mut s = format!(
        "RESTORE MEMBERSHIP {} MEMBER {} GRANTOR ",
        ident::<E>(role)?,
        ident::<E>(member)?
    );
    match grantor {
        Some(g) => {
            s.push_str(&ident::<E>(g)?);
            if root {
                s.push_str(" ROOT");
            }
        }
        None => s.push_str("NONE"),
    }
    let mut words = Vec::new();
    if admin {
        words.push("ADMIN");
    }
    if inherit {
        words.push("INHERIT");
    }
    if set {
        words.push("SET");
    }
    s.push_str(" OPTIONS ");
    s.push_str(&if words.is_empty() {
        "NONE".to_string()
    } else {
        words.join(", ")
    });
    Ok(s)
}

pub(super) fn render_owner<E>(obj: &Obj, owner: &str) -> Result<String, SchemaDdlError<E>> {
    Ok(format!(
        "RESTORE OWNER {} TO {}",
        obj_text::<E>(obj)?,
        ident::<E>(owner)?
    ))
}

pub(super) fn render_acl<E>(
    obj: &Obj,
    grantee: &Option<String>,
    grantor: &str,
    privileges: u16,
    options: u16,
) -> Result<String, SchemaDdlError<E>> {
    let s = format!(
        "RESTORE ACL {} TO {} BY {} PRIVILEGES {}",
        obj_text::<E>(obj)?,
        grantee_text::<E>(grantee)?,
        ident::<E>(grantor)?,
        words_of(privileges).map_err(|_| bad_privilege())?
    );
    with_options::<E>(s, options)
}

pub(super) fn render_acl_init<E>(function: &str) -> Result<String, SchemaDdlError<E>> {
    Ok(format!(
        "RESTORE ACL INIT FUNCTION {}",
        literal::<E>(function)?
    ))
}

pub(super) fn render_default_acl<E>(
    role: &str,
    schema: Option<&str>,
    kind: DefaultAclKind,
    grantee: &Option<String>,
    privileges: u16,
    options: u16,
) -> Result<String, SchemaDdlError<E>> {
    let kind_word = KIND_WORDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, w)| *w)
        .ok_or_else(bad_privilege)?;
    let scope = match schema {
        Some(s) => format!("SCHEMA {}", ident::<E>(s)?),
        None => "ALL".to_string(),
    };
    let s = format!(
        "RESTORE DEFAULT ACL ROLE {} IN {} KIND {} TO {} PRIVILEGES {}",
        ident::<E>(role)?,
        scope,
        kind_word,
        grantee_text::<E>(grantee)?,
        words_of(privileges).map_err(|_| bad_privilege())?
    );
    with_options::<E>(s, options)
}

pub(super) fn render_default_acl_init<E>(role: &str) -> Result<String, SchemaDdlError<E>> {
    Ok(format!(
        "RESTORE DEFAULT ACL INIT ROLE {}",
        ident::<E>(role)?
    ))
}

// ---- 解析 ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Ident(String),
    Str(String),
    Comma,
    Dot,
}

/// 引用された名前・文字列を読む（`quote` の二重化の規則。閉じていない・NUL は拒否）。
fn read_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, q: char) -> Option<String> {
    let mut out = String::new();
    loop {
        match chars.next()? {
            c if c == q => {
                if chars.peek() == Some(&q) {
                    chars.next();
                    out.push(q);
                } else {
                    return Some(out);
                }
            }
            '\0' => return None,
            c => out.push(c),
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<Tok>, DirectiveError> {
    let mut chars = text.chars().peekable();
    let mut out = Vec::new();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' => {
                chars.next();
            }
            ',' => {
                chars.next();
                out.push(Tok::Comma);
            }
            '.' => {
                chars.next();
                out.push(Tok::Dot);
            }
            '"' | '\'' => {
                chars.next();
                let v = read_quoted(&mut chars, c).ok_or(DirectiveError)?;
                out.push(if c == '"' { Tok::Ident(v) } else { Tok::Str(v) });
            }
            'A'..='Z' => {
                let mut w = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_uppercase() || d == '_' {
                        w.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Word(w));
            }
            _ => return Err(DirectiveError),
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn word(&mut self, w: &str) -> Result<(), DirectiveError> {
        match self.next() {
            Some(Tok::Word(x)) if x == w => Ok(()),
            _ => Err(DirectiveError),
        }
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Word(x)) if x == w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn ident(&mut self) -> Result<String, DirectiveError> {
        match self.next() {
            Some(Tok::Ident(s)) => Ok(s),
            _ => Err(DirectiveError),
        }
    }
    fn string(&mut self) -> Result<String, DirectiveError> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            _ => Err(DirectiveError),
        }
    }
    fn end(&self) -> Result<(), DirectiveError> {
        if self.pos == self.toks.len() {
            Ok(())
        } else {
            Err(DirectiveError)
        }
    }
    fn rel(&mut self) -> Result<Rel, DirectiveError> {
        let first = self.ident()?;
        if matches!(self.peek(), Some(Tok::Dot)) {
            self.pos += 1;
            let name = self.ident()?;
            Ok(Rel {
                schema: Some(first),
                name,
            })
        } else {
            Ok(Rel {
                schema: None,
                name: first,
            })
        }
    }
    fn obj(&mut self) -> Result<Obj, DirectiveError> {
        match self.next() {
            Some(Tok::Word(w)) => match w.as_str() {
                "TABLE" => Ok(Obj::Table(self.rel()?)),
                "COLUMN" => {
                    let r = self.rel()?;
                    Ok(Obj::Column(r, self.ident()?))
                }
                "SCHEMA" => Ok(Obj::Schema(self.ident()?)),
                "SEQUENCE" => Ok(Obj::Sequence(self.ident()?)),
                "FUNCTION" => Ok(Obj::Function(self.string()?)),
                _ => Err(DirectiveError),
            },
            _ => Err(DirectiveError),
        }
    }
    fn grantee(&mut self) -> Result<Option<String>, DirectiveError> {
        if self.eat_word("PUBLIC") {
            Ok(None)
        } else {
            Ok(Some(self.ident()?))
        }
    }
    /// 権限語のコンマ区切り（1 語以上・重複不可）をビット集合へ。
    fn privileges(&mut self) -> Result<u16, DirectiveError> {
        let mut mask = 0u16;
        loop {
            let Some(Tok::Word(w)) = self.next() else {
                return Err(DirectiveError);
            };
            let bit = PRIVILEGE_WORDS
                .iter()
                .find(|(_, name)| *name == w)
                .map(|(b, _)| *b)
                .ok_or(DirectiveError)?;
            if mask & bit != 0 {
                return Err(DirectiveError);
            }
            mask |= bit;
            if matches!(self.peek(), Some(Tok::Comma)) {
                self.pos += 1;
            } else {
                return Ok(mask);
            }
        }
    }
    /// `allowed` の語のコンマ区切り（1 語以上・重複不可・許可語のみ）を、許可語ごとの真偽の列へ。
    fn flags<const N: usize>(&mut self, allowed: [&str; N]) -> Result<[bool; N], DirectiveError> {
        let mut out = [false; N];
        loop {
            let Some(Tok::Word(w)) = self.next() else {
                return Err(DirectiveError);
            };
            let i = allowed.iter().position(|a| *a == w).ok_or(DirectiveError)?;
            if out[i] {
                return Err(DirectiveError);
            }
            out[i] = true;
            if matches!(self.peek(), Some(Tok::Comma)) {
                self.pos += 1;
            } else {
                return Ok(out);
            }
        }
    }
    fn optional_options(&mut self) -> Result<u16, DirectiveError> {
        if self.eat_word("OPTION") {
            self.privileges()
        } else {
            Ok(0)
        }
    }
}

/// 指示文 1 件を解析する。厳格な文法に合わない入力はすべて [`DirectiveError`]。
pub(super) fn parse(text: &str) -> Result<Directive, DirectiveError> {
    let mut p = Parser {
        toks: tokenize(text)?,
        pos: 0,
    };
    p.word(DIRECTIVE_KEYWORD)?;
    let d = match p.next() {
        Some(Tok::Word(w)) => match w.as_str() {
            "ROLE" => {
                let role = p.ident()?;
                match p.next() {
                    Some(Tok::Word(x)) if x == "LEGACY" => {
                        p.word("PASSWORD")?;
                        Directive::RoleLegacy {
                            role,
                            phc: p.string()?,
                        }
                    }
                    Some(Tok::Word(x)) if x == "BOOTSTRAP" => {
                        p.word("PRIVILEGES")?;
                        let [all_objects, ddl] = p.flags(["ALL_OBJECTS", "DDL"])?;
                        Directive::RoleBootstrap {
                            role,
                            all_objects,
                            ddl,
                        }
                    }
                    _ => return Err(DirectiveError),
                }
            }
            "BOOTSTRAP" => {
                p.word("MARKER")?;
                Directive::BootstrapMarker
            }
            "MEMBERSHIP" => {
                let role = p.ident()?;
                p.word("MEMBER")?;
                let member = p.ident()?;
                p.word("GRANTOR")?;
                let (grantor, root) = if p.eat_word("NONE") {
                    (None, false)
                } else {
                    let g = p.ident()?;
                    (Some(g), p.eat_word("ROOT"))
                };
                p.word("OPTIONS")?;
                let (admin, inherit, set) = if p.eat_word("NONE") {
                    (false, false, false)
                } else {
                    let [a, i, s] = p.flags(["ADMIN", "INHERIT", "SET"])?;
                    (a, i, s)
                };
                Directive::Membership {
                    role,
                    member,
                    grantor,
                    root,
                    admin,
                    inherit,
                    set,
                }
            }
            "OWNER" => {
                let obj = p.obj()?;
                if matches!(obj, Obj::Column(..) | Obj::Sequence(_)) {
                    return Err(DirectiveError);
                }
                p.word("TO")?;
                Directive::Owner {
                    obj,
                    owner: p.ident()?,
                }
            }
            "ACL" => {
                if p.eat_word("INIT") {
                    p.word("FUNCTION")?;
                    Directive::AclInit {
                        function: p.string()?,
                    }
                } else {
                    let obj = p.obj()?;
                    p.word("TO")?;
                    let grantee = p.grantee()?;
                    p.word("BY")?;
                    let grantor = p.ident()?;
                    p.word("PRIVILEGES")?;
                    let privileges = p.privileges()?;
                    let options = p.optional_options()?;
                    Directive::Acl {
                        obj,
                        grantee,
                        grantor,
                        privileges,
                        options,
                    }
                }
            }
            "DEFAULT" => {
                p.word("ACL")?;
                if p.eat_word("INIT") {
                    p.word("ROLE")?;
                    Directive::DefaultAclInit { role: p.ident()? }
                } else {
                    p.word("ROLE")?;
                    let role = p.ident()?;
                    p.word("IN")?;
                    let schema = if p.eat_word("ALL") {
                        None
                    } else {
                        p.word("SCHEMA")?;
                        Some(p.ident()?)
                    };
                    p.word("KIND")?;
                    let Some(Tok::Word(kw)) = p.next() else {
                        return Err(DirectiveError);
                    };
                    let kind = KIND_WORDS
                        .iter()
                        .find(|(_, w)| *w == kw)
                        .map(|(k, _)| *k)
                        .ok_or(DirectiveError)?;
                    p.word("TO")?;
                    let grantee = p.grantee()?;
                    p.word("PRIVILEGES")?;
                    let privileges = p.privileges()?;
                    let options = p.optional_options()?;
                    Directive::DefaultAcl {
                        role,
                        schema,
                        kind,
                        grantee,
                        privileges,
                        options,
                    }
                }
            }
            _ => return Err(DirectiveError),
        },
        _ => return Err(DirectiveError),
    };
    p.end()?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    type E = ();

    fn rel(s: Option<&str>, n: &str) -> Rel {
        Rel {
            schema: s.map(str::to_string),
            name: n.to_string(),
        }
    }

    #[test]
    fn privilege_words_cover_every_bit() {
        let all = PRIV_SELECT
            | PRIV_INSERT
            | PRIV_UPDATE
            | PRIV_DELETE
            | PRIV_TRUNCATE
            | PRIV_REFERENCES
            | PRIV_TRIGGER
            | PRIV_MAINTAIN
            | PRIV_USAGE
            | PRIV_CREATE
            | PRIV_EXECUTE;
        let covered: u16 = PRIVILEGE_WORDS.iter().fold(0, |a, (b, _)| a | b);
        assert_eq!(covered, all);
        assert_eq!(all.count_ones() as usize, PRIVILEGE_WORDS.len());
    }

    #[test]
    fn every_directive_round_trips() {
        let texts = vec![
            render_role_legacy::<E>("a", "$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA'x").unwrap(),
            render_role_bootstrap::<E>("a", true, true).unwrap(),
            render_role_bootstrap::<E>("a", false, true).unwrap(),
            render_bootstrap_marker(),
            render_membership::<E>("r", "m", Some("g"), true, true, false, true).unwrap(),
            render_membership::<E>("r", "m", None, false, false, false, false).unwrap(),
            render_owner::<E>(&Obj::Table(rel(Some("s"), "t")), "o").unwrap(),
            render_owner::<E>(&Obj::Function("public_f(integer)".into()), "o").unwrap(),
            render_acl::<E>(
                &Obj::Column(rel(None, "t"), "c".into()),
                &None,
                "g",
                PRIV_SELECT | PRIV_UPDATE,
                PRIV_SELECT,
            )
            .unwrap(),
            render_acl::<E>(
                &Obj::Schema("s".into()),
                &Some("b".into()),
                "g",
                PRIV_USAGE,
                0,
            )
            .unwrap(),
            render_acl::<E>(&Obj::Sequence("q".into()), &None, "g", PRIV_USAGE, 0).unwrap(),
            render_acl_init::<E>("f(integer, text)").unwrap(),
            render_default_acl::<E>(
                "r",
                Some("s"),
                DefaultAclKind::Tables,
                &None,
                PRIV_SELECT,
                0,
            )
            .unwrap(),
            render_default_acl::<E>(
                "r",
                None,
                DefaultAclKind::Functions,
                &Some("x".into()),
                PRIV_EXECUTE,
                PRIV_EXECUTE,
            )
            .unwrap(),
            render_default_acl_init::<E>("r").unwrap(),
        ];
        for t in &texts {
            let d = parse(t).unwrap_or_else(|_| panic!("parse failed: {t}"));
            // 往復: 解析した値から同じ文が再描画できる（代表として ACL とメンバーシップを確かめる）。
            match d {
                Directive::Acl {
                    obj,
                    grantee,
                    grantor,
                    privileges,
                    options,
                } => assert_eq!(
                    &render_acl::<E>(&obj, &grantee, &grantor, privileges, options).unwrap(),
                    t
                ),
                Directive::Membership {
                    role,
                    member,
                    grantor,
                    root,
                    admin,
                    inherit,
                    set,
                } => assert_eq!(
                    &render_membership::<E>(
                        &role,
                        &member,
                        grantor.as_deref(),
                        root,
                        admin,
                        inherit,
                        set
                    )
                    .unwrap(),
                    t
                ),
                Directive::RoleLegacy { role, phc } => {
                    assert_eq!(&render_role_legacy::<E>(&role, &phc).unwrap(), t)
                }
                _ => {}
            }
        }
    }

    #[test]
    fn malformed_directives_are_rejected() {
        for bad in [
            "",
            "RESTORE",
            "restore bootstrap marker",
            "RESTORE BOOTSTRAP MARKER extra",
            "RESTORE ROLE \"a\" BOOTSTRAP PRIVILEGES",
            "RESTORE ROLE \"a\" BOOTSTRAP PRIVILEGES DDL, DDL",
            "RESTORE ROLE \"a\" BOOTSTRAP PRIVILEGES SUPERUSER",
            "RESTORE ROLE \"a\" LEGACY PASSWORD 'x\0y'",
            "RESTORE ROLE \"a\" LEGACY PASSWORD 'unterminated",
            "RESTORE ROLE a LEGACY PASSWORD 'x'",
            "RESTORE OWNER SEQUENCE \"q\" TO \"o\"",
            "RESTORE OWNER COLUMN \"t\" \"c\" TO \"o\"",
            "RESTORE ACL TABLE \"t\" TO PUBLIC BY \"g\" PRIVILEGES SELECT, SELECT",
            "RESTORE ACL TABLE \"t\" TO PUBLIC BY \"g\" PRIVILEGES ALL",
            "RESTORE ACL TABLE \"t\" TO PUBLIC BY \"g\" PRIVILEGES SELECT OPTION",
            "RESTORE ACL TABLE \"t\" TO PUBLIC BY \"g\" PRIVILEGES SELECT; DROP",
            "RESTORE MEMBERSHIP \"r\" MEMBER \"m\" GRANTOR NONE OPTIONS",
            "RESTORE DEFAULT ACL ROLE \"r\" IN ALL KIND VIEWS TO PUBLIC PRIVILEGES SELECT",
            "RESTORE UNKNOWN",
        ] {
            assert!(parse(bad).is_err(), "must reject: {bad:?}");
        }
    }

    #[test]
    fn debug_redacts_the_password_hash() {
        let d = parse(&render_role_legacy::<E>("a", "SECRET-HASH-VALUE").unwrap()).unwrap();
        let shown = format!("{d:?}");
        assert!(!shown.contains("SECRET-HASH-VALUE"));
        assert!(!format!("{DirectiveError:?}{DirectiveError}").contains("SECRET"));
    }

    #[test]
    fn prefixes_match_the_rendered_forms() {
        assert!(prefix_is_allowed(
            PrefixKind::RoleBootstrap,
            &render_bootstrap_marker()
        ));
        assert!(!prefix_is_allowed(PrefixKind::Owner, "RESTORE ACL TABLE"));
    }
}
