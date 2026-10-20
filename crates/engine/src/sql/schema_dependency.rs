//! public のオブジェクトからテナント SCHEMA 配下のオブジェクトへの依存の拒否（TABLE-27・
//! Issue #1717。ADR `docs/design/schema-tenant-namespace.md` D7 のポインタ）。
//!
//! 役割: `public`（全テナント共有のカタログ）の表・ビュー・materialized view が、特定テナントの
//! SCHEMA 配下の relation（内部キー `#<id>.<name>`）を参照する定義を `42501`
//! （[`SqlSurfaceError::SchemaDependencyForbidden`]）で拒否する。許すと、SCHEMA の削除や他テナントの
//! DDL が public の定義を壊し、依存の有無が他テナントへ漏れる経路になる。逆向き（SCHEMA 配下から
//! public）は許可する。
//!
//! 呼び出し元: `core::EngineCore::execute_parsed_in_session_inner` の `CREATE TABLE`・
//! `ALTER TABLE`・`CREATE VIEW`・`CREATE MATERIALIZED VIEW` の各腕が、DDL 権限ゲート
//! （`sql::ddl::require_ddl_permission`）の直後・カタログ書き込みの前に呼ぶ。判定はトークン由来の
//! 名前だけで行い、カタログを照会しない（存在オラクルにならない）。エラーは固定文言で、名前・
//! 内部 ID を含めない。内部キーを作れるのは名前解決器だけ（`sql::relation_name`）なので、
//! 「キーが `#` で始まる」ことが SCHEMA 配下であることの判定になる。

use crate::catalog::ColumnDefault;
use crate::sql::allowlist::{
    InsertLiteral, SqlSurfaceError, ValidatedAlterTable, ValidatedCreateTable,
};
use crate::sql::lexer::Token;

/// relation キーが SCHEMA 配下（内部キー）か。
fn is_schema_key(name: &str) -> bool {
    name.starts_with('#')
}

/// `owner` が public のとき、`deps` に SCHEMA 配下のキーが 1 つでもあれば拒否する。
fn forbid<'a>(owner: &str, mut deps: impl Iterator<Item = &'a str>) -> Result<(), SqlSurfaceError> {
    if !is_schema_key(owner) && deps.any(is_schema_key) {
        return Err(SqlSurfaceError::SchemaDependencyForbidden);
    }
    Ok(())
}

/// `DEFAULT nextval('<名前>')` が参照する SEQUENCE 名（それ以外のリテラルは `None`）。
fn sequence_of_literal(literal: Option<&InsertLiteral>) -> Option<&str> {
    match literal {
        Some(InsertLiteral::SequenceCall { name, .. }) => Some(name.as_str()),
        _ => None,
    }
}

/// `CREATE TABLE`: 外部キーの参照先と `DEFAULT nextval` の SEQUENCE が SCHEMA 配下でないこと
/// （表が public の場合）。暗黙 SEQUENCE（SERIAL・IDENTITY）は表と同じ SCHEMA に作られるため対象外。
pub(crate) fn check_create_table(stmt: &ValidatedCreateTable) -> Result<(), SqlSurfaceError> {
    let fk_parents = stmt.foreign_keys.iter().map(|fk| fk.parent_table());
    let sequences = stmt.columns.iter().filter_map(|c| match &c.default_expr {
        Some(ColumnDefault::Sequence(name)) => Some(name.as_str()),
        _ => None,
    });
    forbid(&stmt.table_name, fk_parents.chain(sequences))
}

/// `ALTER TABLE`: 追加する外部キーの参照先・`ADD COLUMN`／`SET DEFAULT` の `DEFAULT nextval` が
/// SCHEMA 配下でないこと（表が public の場合）。複数アクションは各アクションを検査する。
pub(crate) fn check_alter_table(stmt: &ValidatedAlterTable) -> Result<(), SqlSurfaceError> {
    match stmt {
        ValidatedAlterTable::Multiple(m) => m.actions.iter().try_for_each(check_alter_table),
        ValidatedAlterTable::AddColumn(a) => forbid(
            &a.table_name,
            sequence_of_literal(a.default.as_ref()).into_iter(),
        ),
        ValidatedAlterTable::AddForeignKey(a) => {
            forbid(&a.table_name, std::iter::once(a.foreign_key.parent_table()))
        }
        ValidatedAlterTable::AlterColumnSetDefault(a) => forbid(
            &a.table_name,
            sequence_of_literal(a.default.as_ref()).into_iter(),
        ),
        _ => Ok(()),
    }
}

/// `CREATE [OR REPLACE] [MATERIALIZED] VIEW`: 本文が SCHEMA 配下の relation を読まないこと
/// （ビューが public の場合）。本文は保存形（正規化描画済み）で、内部キーは引用形の `Ident`
/// として現れる（`lexer::tokenize_stored_view_body` だけが受理する）。字句解析できない本文は
/// 拒否側に倒す（fail-closed）。
pub(crate) fn check_view_body(owner: &str, body_sql: &str) -> Result<(), SqlSurfaceError> {
    if is_schema_key(owner) {
        return Ok(());
    }
    let tokens = crate::sql::lexer::tokenize_stored_view_body(body_sql)
        .map_err(|_| SqlSurfaceError::SchemaDependencyForbidden)?;
    forbid(
        owner,
        tokens.iter().filter_map(|t| match t {
            Token::Ident(s) => Some(s.as_str()),
            _ => None,
        }),
    )
}
