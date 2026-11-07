//! DDL 文書き出しで使う識別子・文字列リテラルのクォートと、カタログ内部キーの描画
//! （RECOVER-16 (1)。親モジュール: [`super`]）。
//!
//! 責務境界: 文字列を SQL 文へ連結する唯一の入口。識別子は常にダブルクォートで囲み内部の `"` を
//! 二重化する。文字列リテラルは `'` を二重化する（本リポの字句解析器はバックスラッシュを
//! エスケープとして扱わないため、`''` の二重化だけで閉じる）。NUL 文字は字句解析器が扱えない
//! 値のため、識別子・リテラルのどちらでも書き出せないものとして呼び出し元へ拒否を返す
//! （黙って落とさない）。

use super::super::namespace::RelationKey;
use super::{unsupported, SchemaDdlError};
use std::collections::BTreeMap;

/// 識別子が本リポの字句解析器で「引用符の有無によらず 1 個の素の識別子」として読める名前かを確かめる。
/// 字句解析器は二重引用符識別子を、引用符なしでも同じ 1 個の `Ident` になる語に限って受理する
/// （予約語・記号入りの名前は引用しても受理されない）。受理されない名前や、引用すると別の意味を
/// 持つ語（`NULL`・`DEFAULT`・`ARRAY`・`TRUE`・`FALSE`・SQL 値関数名）は、再実行で別物になるか
/// 失敗するため書き出せない（黙って別の名前にしない）。
fn check_name<E>(name: &str) -> Result<(), SchemaDdlError<E>> {
    let plain = matches!(
        crate::sql::lexer::tokenize(name).as_deref(),
        Ok([crate::sql::lexer::Token::Ident(n)]) if n == name
    );
    let special = ["null", "array", "default", "true", "false"]
        .iter()
        .any(|w| name.eq_ignore_ascii_case(w))
        || crate::sql::lexer::is_value_function_keyword(name);
    if !plain || special {
        return Err(unsupported(
            "identifier",
            "name cannot be represented in SQL by this engine",
        ));
    }
    Ok(())
}

/// `name` をダブルクォートで囲んだ識別子として `out` へ追記する。内部の `"` は `""` に二重化する
/// （[`check_name`] を通る名前は `"` を含まないため二重化は防御的な処理）。
pub(super) fn push_ident<E>(out: &mut String, name: &str) -> Result<(), SchemaDdlError<E>> {
    check_name(name)?;
    out.push('"');
    out.push_str(&name.replace('"', "\"\""));
    out.push('"');
    Ok(())
}

/// [`push_ident`] の `String` を返す版。
pub(super) fn ident<E>(name: &str) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::new();
    push_ident(&mut s, name)?;
    Ok(s)
}

/// 照合名を `"名前"` の形で `out` へ追記する。照合名は本リポの固定の許可リスト（`Collation`）の値だけで、
/// ハイフン入りの名前（`und-x-icu`）は識別子の検査（[`check_name`]）を通らないため別の入口にする。
pub(super) fn push_collation(out: &mut String, c: crate::collation::Collation) {
    out.push('"');
    out.push_str(c.sql_name());
    out.push('"');
}

/// 引用符なしの識別子（SCHEMA 修飾名の各部と `nextval('...')`・`setval('...')` の文字列内の名前。
/// これらの位置は字句解析器・構文解析器が引用符付きを受理しない）。[`check_name`] を通った名前だけを返す。
pub(super) fn bare_ident<E>(name: &str) -> Result<String, SchemaDdlError<E>> {
    check_name(name)?;
    Ok(name.to_string())
}

/// `value` を文字列リテラル（`'...'`）として `out` へ追記する。内部の `'` は `''` に二重化する。
pub(super) fn push_literal<E>(out: &mut String, value: &str) -> Result<(), SchemaDdlError<E>> {
    if value.contains('\0') {
        return Err(unsupported("literal", "contains a NUL character"));
    }
    out.push('\'');
    out.push_str(&value.replace('\'', "''"));
    out.push('\'');
    Ok(())
}

/// [`push_literal`] の `String` を返す版。
pub(super) fn literal<E>(value: &str) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::new();
    push_literal(&mut s, value)?;
    Ok(s)
}

/// 保存済みの正規化 SQL 本文（VIEW 本文・CHECK 述語・式索引・ポリシー式）に、SCHEMA 配下の
/// relation 内部キー（`"#<id>.<name>"` の引用形）が混ざっていないことを確かめる。混ざる本文は
/// 利用者向けの名前へ戻す手段が無いため、書き出さず全体を失敗させる（fail-closed）。
pub(super) fn ensure_no_internal_key<E>(
    kind: &'static str,
    sql: &str,
) -> Result<(), SchemaDdlError<E>> {
    if sql.contains("\"#") {
        return Err(unsupported(
            kind,
            "stored body references a schema-scoped relation key",
        ));
    }
    Ok(())
}

/// relation 内部キーの所属判定の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum KeyScope {
    /// DB 全体で共有される `public` の relation（裸の名前）。
    Shared,
    /// 選択したテナントの SCHEMA 配下（SCHEMA 名と relation 名）。
    Tenant { schema: String, name: String },
    /// 選択したテナントの SCHEMA に属さない `#` 付きのキー（他テナントのもの）。存在を示す
    /// 出力・エラーを一切作らず、呼び出し元は黙って対象外にする。
    Foreign,
}

/// テナントの SCHEMA 一覧（内部 ID → 名前）を持ち、relation 内部キーを描画する。
#[derive(Debug, Clone, Default)]
pub(super) struct Namer {
    pub(super) schemas: BTreeMap<u64, String>,
}

impl Namer {
    /// 内部キーの所属を判定する。キー形式が不正なら `Catalog` 系エラー（破損）として返す。
    pub(super) fn scope<E>(&self, key: &str) -> Result<KeyScope, SchemaDdlError<E>> {
        match RelationKey::parse(key).map_err(SchemaDdlError::Catalog)? {
            RelationKey::Public(_) => Ok(KeyScope::Shared),
            RelationKey::InSchema { schema_id, name } => match self.schemas.get(&schema_id) {
                Some(schema) => Ok(KeyScope::Tenant {
                    schema: schema.clone(),
                    name,
                }),
                None => Ok(KeyScope::Foreign),
            },
        }
    }

    /// relation の SQL 上の参照名。`public` は `"name"`、SCHEMA 配下は `"schema"."name"`。
    /// 他テナントのキーは `Ok(None)`。
    pub(super) fn qualified<E>(&self, key: &str) -> Result<Option<String>, SchemaDdlError<E>> {
        match self.scope(key)? {
            KeyScope::Shared => Ok(Some(ident(key)?)),
            // SCHEMA 修飾名は引用符なし（`schema.name`）でしか受理されない。
            KeyScope::Tenant { schema, name } => Ok(Some(format!(
                "{}.{}",
                bare_ident::<E>(&schema)?,
                bare_ident::<E>(&name)?
            ))),
            KeyScope::Foreign => Ok(None),
        }
    }

    /// 自テナントまたは共有の relation だけを描画する。他テナントのキーは「参照できない」ので
    /// 呼び出し元が整合しない参照として扱えるよう `Unsupported` にする。
    pub(super) fn qualified_required<E>(&self, key: &str) -> Result<String, SchemaDdlError<E>> {
        self.qualified(key)?.ok_or_else(|| {
            unsupported("relation", "reference to a relation outside the dump scope")
        })
    }

    /// `nextval('...')`・`setval('...')` の文字列内に書く SEQUENCE 名。これらは引用符付き・SCHEMA 修飾の名前を
    /// 受理しないため、共有（`public`）の SEQUENCE の素の名前だけを返し、SCHEMA 配下は書き出せない
    /// （`Unsupported`）。他テナントのキーは `Ok(None)`。
    pub(super) fn sequence_arg<E>(&self, key: &str) -> Result<Option<String>, SchemaDdlError<E>> {
        match self.scope(key)? {
            KeyScope::Shared => Ok(Some(bare_ident(key)?)),
            KeyScope::Tenant { .. } => Err(unsupported(
                "sequence",
                "schema-scoped sequence name cannot be written in nextval or setval",
            )),
            KeyScope::Foreign => Ok(None),
        }
    }

    /// 保存済みの正規化 SQL 本文に現れる SCHEMA 配下の relation 内部キー（引用形 `"#<id>.<name>"`）を、
    /// 利用者向けの修飾名（`schema.name`）へ戻す。文字列リテラル（`'...'`）の中は書き換えない。
    /// 選択したテナントの SCHEMA に解決できないキー（他テナントのもの・破損）が現れる本文は、
    /// 戻す手段が無いので `Unsupported`（存在を示す情報は含めない）。
    pub(super) fn externalize<E>(
        &self,
        kind: &'static str,
        sql: &str,
    ) -> Result<String, SchemaDdlError<E>> {
        let mut out = String::with_capacity(sql.len());
        let mut in_literal = false;
        let mut rest = sql;
        while let Some(c) = rest.chars().next() {
            if c == '\'' {
                in_literal = !in_literal;
            }
            if !in_literal && rest.starts_with("\"#") {
                let body = rest.get(1..).unwrap_or("");
                let Some(end) = body.find('"') else {
                    return Err(unsupported(kind, "unterminated internal relation key"));
                };
                let key = body.get(..end).unwrap_or("");
                match self.qualified::<E>(key)? {
                    Some(q) if key.starts_with('#') => out.push_str(&q),
                    _ => {
                        return Err(unsupported(
                            kind,
                            "stored body references a relation outside the dump scope",
                        ));
                    }
                }
                rest = rest.get(end + 2..).unwrap_or("");
                continue;
            }
            out.push(c);
            rest = rest.get(c.len_utf8()..).unwrap_or("");
        }
        Ok(out)
    }
}
