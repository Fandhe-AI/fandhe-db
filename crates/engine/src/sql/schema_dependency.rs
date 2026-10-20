//! public と SCHEMA 配下のオブジェクトの間をまたぐ依存の拒否（TABLE-27・
//! Issue #1717。ADR `docs/design/schema-tenant-namespace.md` D7 のポインタ）。
//!
//! 役割: `public`（全テナント共有のカタログ）と特定テナントの SCHEMA 配下（内部キー `#<id>.<name>`）の
//! 間をまたぐ依存を、どちらの向きでも `42501`（[`SqlSurfaceError::SchemaDependencyForbidden`]）で拒否する。
//! `public` → SCHEMA は、SCHEMA の削除や他テナントの DDL が public の定義を壊し依存の有無が他テナントへ
//! 漏れる経路になる。SCHEMA → `public` は、public 側の `DROP`（RESTRICT／CASCADE）の依存判定が全テナントを
//! 走査するため、他テナントの非公開の依存の有無で結果（成功／`2BP01`）が分かれて存在情報が漏れ、
//! CASCADE では他テナントの定義まで消える。public 側 DDL のテナント境界対応（認可・依存処理）までは
//! この向きも拒否する（fail-closed。ADR D7）。
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

/// `deps` に、`owner` と所属（public か SCHEMA 配下か）が異なるものが 1 つでもあれば拒否する。
fn forbid<'a>(owner: &str, mut deps: impl Iterator<Item = &'a str>) -> Result<(), SqlSurfaceError> {
    let owner_in_schema = is_schema_key(owner);
    if deps.any(|d| is_schema_key(d) != owner_in_schema) {
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

/// `CREATE TABLE`: 外部キーの参照先と `DEFAULT nextval` の SEQUENCE が表と同じ所属（public か
/// SCHEMA 配下か）であること。暗黙 SEQUENCE（SERIAL・IDENTITY）は表と同じ SCHEMA に作られるため対象外。
pub(crate) fn check_create_table(stmt: &ValidatedCreateTable) -> Result<(), SqlSurfaceError> {
    let fk_parents = stmt.foreign_keys.iter().map(|fk| fk.parent_table());
    let sequences = stmt.columns.iter().filter_map(|c| match &c.default_expr {
        Some(ColumnDefault::Sequence(name)) => Some(name.as_str()),
        _ => None,
    });
    forbid(&stmt.table_name, fk_parents.chain(sequences))
}

/// `ALTER TABLE`: 追加する外部キーの参照先・`ADD COLUMN`／`SET DEFAULT` の `DEFAULT nextval` が
/// 表と同じ所属であること。複数アクションは各アクションを検査する。
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

/// `CREATE [OR REPLACE] [MATERIALIZED] VIEW`: 本文が読む relation がビューと同じ所属であること。
/// 本文は保存形（正規化描画済み）で、内部キーは引用形の `Ident` として現れる
/// （`lexer::tokenize_stored_view_body` だけが受理する）。public のビューは本文中の `Ident` に
/// 内部キーが 1 つでもあれば拒否し、SCHEMA 配下のビューは DROP の依存判定と同じ導出
/// （`catalog::view_body_relations`）の relation がすべて内部キーであることを求める
/// （列名は relation ではないため対象外）。字句解析・構造検証できない本文は拒否側に倒す（fail-closed）。
pub(crate) fn check_view_body(owner: &str, body_sql: &str) -> Result<(), SqlSurfaceError> {
    if is_schema_key(owner) {
        let relations = crate::catalog::view_body_relations(body_sql)
            .map_err(|_| SqlSurfaceError::SchemaDependencyForbidden)?;
        return forbid(owner, relations.iter().map(String::as_str));
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
