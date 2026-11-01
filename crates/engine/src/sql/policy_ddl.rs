//! `CREATE POLICY`／`ALTER POLICY`／`DROP POLICY` の実行本体（RLS-13、Issue #2274。設計記録:
//! `docs/design/row-security-policy-composition.md`）。
//!
//! 役割: 構造検証済みの文（`sql::allowlist::{ValidatedCreatePolicy, ValidatedAlterPolicy,
//! ValidatedDropPolicy}`）をポリシーカタログ（`catalog::policy`）への書き込みへ写し、`CatalogError` を
//! SQLSTATE の契約へ写像する。式の束縛は `sql::policy_expr` がカタログの write txn の中のスキーマに対して
//! 行う。呼び出し元は `core.rs` の `ParsedSql::{CreatePolicy, AlterPolicy, DropPolicy}` 腕で、DDL 権限ゲート
//! （`sql::ddl::require_ddl_permission`。権限なしは対象の有無を問わず `42501`）と明示トランザクション内の
//! `0A000` を通過した後にだけ来る。表の所有者の判定はロール機構が入るまでこの権限ゲートで代替する。
//!
//! 判定順（決定的・fail-closed）: 構文（`allowlist`）→ 権限 → 明示 Tx → カタログの単一 write txn 内で
//! 対象の種別（ビュー等は `42809`）→ 表の存在（`42P01`）→ 式の束縛（未知列 `42703`・非真偽値 `42804`）→
//! 同名の既存（`42710`）→ 件数上限（`54000`）。`ALTER`/`DROP` はポリシーの不在を `42704` とする。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントのポリシーは不在と
//! 同じ経路・同じ `42704` になる。エラー文言は固定で、内部キー・tenant_id・他テナントの定義を含めない。

use crate::catalog::policy::{
    PolicyDef, PolicyExprs, POLICY_ALREADY_EXISTS_MESSAGE, POLICY_NOT_FOUND_MESSAGE,
};
use crate::catalog::{CatalogError, TableSchema};
use crate::sql::allowlist::{
    AlterPolicyAction, SqlSurfaceError, ValidatedAlterPolicy, ValidatedCreatePolicy,
    ValidatedDropPolicy, WherePredicate,
};
use crate::sql::policy_expr::{
    bind_policy_expr, bind_stored_policy_expr, merge_referenced_columns, BoundPolicyExpr,
};
use crate::storage::Storage;

/// `CREATE POLICY` の成功応答（結果を持たない。wire はコマンドタグ `CREATE POLICY` を返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreatePolicyOutcome {}

/// `ALTER POLICY` の成功応答（結果を持たない。wire はコマンドタグ `ALTER POLICY` を返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AlterPolicyOutcome {}

/// `DROP POLICY` の成功応答。`IF EXISTS` で対象が無く no-op になったかだけを持つ
/// （CommandComplete のタグは通常の DROP と同一のため、wire 側は参照しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DropPolicyOutcome {
    pub(crate) skipped: bool,
}

impl DropPolicyOutcome {
    /// `IF EXISTS` により対象不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// カタログの束縛クロージャが返す失敗。カタログ由来と SQL 表層由来（式の束縛エラー）を区別して運び、
/// [`map_policy_error`] が SQLSTATE へ写す。
#[derive(Debug)]
pub(crate) enum PolicyDdlError {
    Catalog(CatalogError),
    Sql(SqlSurfaceError),
}

impl From<CatalogError> for PolicyDdlError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

impl From<SqlSurfaceError> for PolicyDdlError {
    fn from(e: SqlSurfaceError) -> Self {
        Self::Sql(e)
    }
}

/// 式を束縛する（`None` は未指定）。
fn bind_opt(
    preds: Option<&Vec<WherePredicate>>,
    schema: &TableSchema,
) -> Result<Option<BoundPolicyExpr>, SqlSurfaceError> {
    preds.map(|p| bind_policy_expr(p, schema)).transpose()
}

/// `CREATE POLICY` を実行する（権限・明示 Tx のゲートは呼び出し元で通過済み）。式の束縛は対象表の
/// スキーマを読んだ同じ write txn の中で行う。
pub(crate) fn execute_create_policy(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedCreatePolicy,
) -> Result<CreatePolicyOutcome, SqlSurfaceError> {
    let def = PolicyDef {
        id: 0,
        name: stmt.name.clone(),
        permissive: stmt.permissive,
        command: stmt.command,
        roles: stmt.roles.clone(),
        using_sql: None,
        check_sql: None,
        referenced_columns: Vec::new(),
    };
    storage
        .create_policy(tenant_id, &stmt.table, &def, |schema| {
            let using = bind_opt(stmt.using.as_ref(), schema)?;
            let check = bind_opt(stmt.check.as_ref(), schema)?;
            let referenced_columns = merge_referenced_columns(
                schema,
                using.as_ref().map_or(&[][..], |b| &b.referenced_columns),
                check.as_ref().map_or(&[][..], |b| &b.referenced_columns),
            );
            Ok::<_, PolicyDdlError>(PolicyExprs {
                using_sql: using.map(|b| b.sql),
                check_sql: check.map(|b| b.sql),
                referenced_columns,
            })
        })
        .map_err(|e| map_policy_error(e, &stmt.name))?;
    Ok(CreatePolicyOutcome {})
}

/// `ALTER POLICY` を実行する。`RENAME TO` は改名先の重複を `42710`、対象の不在を `42704` にする。
/// それ以外は指定した句だけを差し替え（式は同じ txn のスキーマで再束縛）、指定しなかった句は保持する。
/// 既存ポリシーの `FOR` と矛盾する式の追加（`SELECT` ポリシーへの `WITH CHECK` 等）は `42601`。
pub(crate) fn execute_alter_policy(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedAlterPolicy,
) -> Result<AlterPolicyOutcome, SqlSurfaceError> {
    match &stmt.action {
        AlterPolicyAction::Rename(new_name) => {
            storage
                .rename_policy(tenant_id, &stmt.table, &stmt.name, new_name)
                .map_err(|e| {
                    let e: PolicyDdlError = e.into();
                    // 不在は旧名、改名先の重複は新名を報告する。
                    let reported = match &e {
                        PolicyDdlError::Catalog(CatalogError::Invalid(m))
                            if m == POLICY_NOT_FOUND_MESSAGE =>
                        {
                            stmt.name.as_str()
                        }
                        _ => new_name.as_str(),
                    };
                    map_policy_error(e, reported)
                })?;
        }
        AlterPolicyAction::Modify {
            roles,
            using,
            check,
        } => {
            storage
                .alter_policy(tenant_id, &stmt.table, &stmt.name, |schema, current| {
                    if using.is_some() && !current.command.allows_using() {
                        return Err(PolicyDdlError::Sql(SqlSurfaceError::unsupported(
                            "only WITH CHECK expression allowed for INSERT",
                        )));
                    }
                    if check.is_some() && !current.command.allows_check() {
                        return Err(PolicyDdlError::Sql(SqlSurfaceError::unsupported(
                            "WITH CHECK cannot be applied to SELECT or DELETE",
                        )));
                    }
                    let new_using = match using {
                        Some(p) => Some(bind_policy_expr(p, schema)?),
                        None => current
                            .using_sql
                            .as_deref()
                            .map(|s| bind_stored_policy_expr(s, schema))
                            .transpose()?,
                    };
                    let new_check = match check {
                        Some(p) => Some(bind_policy_expr(p, schema)?),
                        None => current
                            .check_sql
                            .as_deref()
                            .map(|s| bind_stored_policy_expr(s, schema))
                            .transpose()?,
                    };
                    let referenced_columns = merge_referenced_columns(
                        schema,
                        new_using
                            .as_ref()
                            .map_or(&[][..], |b| &b.referenced_columns),
                        new_check
                            .as_ref()
                            .map_or(&[][..], |b| &b.referenced_columns),
                    );
                    Ok::<_, PolicyDdlError>(PolicyDef {
                        roles: roles.clone().unwrap_or_else(|| current.roles.clone()),
                        using_sql: new_using.map(|b| b.sql),
                        check_sql: new_check.map(|b| b.sql),
                        referenced_columns,
                        ..current.clone()
                    })
                })
                .map_err(|e| map_policy_error(e, &stmt.name))?;
        }
    }
    Ok(AlterPolicyOutcome {})
}

/// `DROP POLICY` を実行する。対象が無い場合（表が無い・他テナントにしか無い場合を含む）は `IF EXISTS`
/// なら成功の no-op、そうでなければ表の不在は `42P01`、ポリシーの不在は `42704`。
pub(crate) fn execute_drop_policy(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedDropPolicy,
) -> Result<DropPolicyOutcome, SqlSurfaceError> {
    match storage.drop_policy(tenant_id, &stmt.table, &stmt.name) {
        Ok(true) => Ok(DropPolicyOutcome { skipped: false }),
        Ok(false) | Err(CatalogError::TableNotFound(_)) if stmt.if_exists => {
            Ok(DropPolicyOutcome { skipped: true })
        }
        Ok(false) => Err(SqlSurfaceError::UndefinedObject {
            name: stmt.name.clone(),
        }),
        Err(e) => Err(map_policy_error(e.into(), &stmt.name)),
    }
}

/// 解決済みカタログキー（`#<id>.<表名>`）から利用者が書いた表名の部分だけを取り出す。内部 ID を
/// エラー文言へ出さないため（security.md P0。スキーマ ID の存在オラクル防止）。
fn user_facing_relation_name(key: &str) -> String {
    match key.strip_prefix('#').and_then(|r| r.split_once('.')) {
        Some((_, rel)) => rel.to_string(),
        None => key.to_string(),
    }
}

/// ポリシーカタログの失敗を SQL 表層の契約へ写像する（同名の既存 `42710`・ポリシー不在 `42704`・
/// 表不在 `42P01`・種別違い `42809`・列不在 `42703`・件数上限 `54000`・形状不正 `22000`・ロック timeout
/// `55P03`・その他 `XX000`）。式の束縛エラーは SQLSTATE をそのまま透過する。文言は固定で、内部キー・
/// tenant_id・他テナントの定義を含めない（security.md P0）。`policy_name` は利用者自身が書いた名前。
pub(crate) fn map_policy_error(e: PolicyDdlError, policy_name: &str) -> SqlSurfaceError {
    let e = match e {
        PolicyDdlError::Sql(s) => return s,
        PolicyDdlError::Catalog(c) => c,
    };
    match e {
        CatalogError::Invalid(m) if m == POLICY_ALREADY_EXISTS_MESSAGE => {
            SqlSurfaceError::duplicate_object(policy_name)
        }
        CatalogError::Invalid(m) if m == POLICY_NOT_FOUND_MESSAGE => {
            SqlSurfaceError::UndefinedObject {
                name: policy_name.to_string(),
            }
        }
        CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid policy definition"),
        CatalogError::ViewLimitExceeded(_) => {
            SqlSurfaceError::payload_too_large("too many policies")
        }
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable {
            name: user_facing_relation_name(&name),
        },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType {
            name: user_facing_relation_name(&name),
        },
        CatalogError::ColumnNotFound(name) => SqlSurfaceError::undefined_column(name),
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "policy catalog operation failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_error_reports_source_name_for_not_found() {
        let not_found =
            PolicyDdlError::Catalog(CatalogError::Invalid(POLICY_NOT_FOUND_MESSAGE.to_string()));
        let dup = PolicyDdlError::Catalog(CatalogError::Invalid(
            POLICY_ALREADY_EXISTS_MESSAGE.to_string(),
        ));
        assert!(
            matches!(map_policy_error(not_found, "old"), SqlSurfaceError::UndefinedObject { name } if name == "old")
        );
        assert_eq!(map_policy_error(dup, "new").wire_code(), "42710");
    }

    #[test]
    fn catalog_errors_map_to_fixed_sqlstates() {
        let code = |e| map_policy_error(PolicyDdlError::Catalog(e), "p").wire_code();
        assert_eq!(
            code(CatalogError::Invalid(
                POLICY_ALREADY_EXISTS_MESSAGE.to_string()
            )),
            "42710"
        );
        assert_eq!(
            code(CatalogError::Invalid(POLICY_NOT_FOUND_MESSAGE.to_string())),
            "42704"
        );
        assert_eq!(code(CatalogError::Invalid("x".to_string())), "22000");
        assert_eq!(
            code(CatalogError::ViewLimitExceeded("x".to_string())),
            "54000"
        );
        assert_eq!(code(CatalogError::TableNotFound("t".to_string())), "42P01");
        assert_eq!(
            code(CatalogError::WrongObjectKind("t".to_string())),
            "42809"
        );
        for e in [
            CatalogError::TableNotFound("#12.t".to_string()),
            CatalogError::WrongObjectKind("#12.t".to_string()),
        ] {
            let m = map_policy_error(PolicyDdlError::Catalog(e), "p").client_message();
            assert!(!m.contains("#12"), "{m}");
        }
        assert_eq!(code(CatalogError::ColumnNotFound("c".to_string())), "42703");
        assert_eq!(code(CatalogError::WriteLockTimeout), "55P03");
        let corrupt = map_policy_error(
            PolicyDdlError::Catalog(CatalogError::CorruptSchema("secret #1.f".to_string())),
            "p",
        );
        assert_eq!(corrupt.wire_code(), "XX000");
        assert!(!corrupt.client_message().contains("secret"));
    }

    #[test]
    fn rename_to_existing_name_reports_the_target_name() {
        use crate::catalog::policy::{PolicyCommand, PolicyDef, PolicyExprs, PolicyRole};
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        let path = crate::test_util::temp_db::unique_db_path("pol_rename_dup");
        let _g = crate::test_util::temp_db::CleanupGuard(path.clone());
        let s = Storage::open(&path).expect("open");
        s.create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("a", ColumnType::Text, true)],
        ))
        .expect("table");
        for name in ["src_pol", "dst_pol"] {
            let def = PolicyDef {
                id: 0,
                name: name.to_string(),
                permissive: true,
                command: PolicyCommand::All,
                roles: vec![PolicyRole::Public],
                using_sql: None,
                check_sql: None,
                referenced_columns: Vec::new(),
            };
            s.create_policy::<CatalogError, _>("t1", "docs", &def, |_| {
                Ok(PolicyExprs {
                    using_sql: None,
                    check_sql: None,
                    referenced_columns: Vec::new(),
                })
            })
            .expect("create");
        }
        let stmt = ValidatedAlterPolicy {
            name: "src_pol".to_string(),
            table: "docs".to_string(),
            action: AlterPolicyAction::Rename("dst_pol".to_string()),
        };
        let err = match execute_alter_policy(&s, "t1", &stmt) {
            Err(e) => e,
            Ok(_) => panic!("rename onto an existing name must fail"),
        };
        assert_eq!(err.wire_code(), "42710");
        let m = err.client_message();
        assert!(m.contains("dst_pol") && !m.contains("src_pol"), "{m}");
    }

    #[test]
    fn sql_errors_pass_through() {
        let e = map_policy_error(
            PolicyDdlError::Sql(SqlSurfaceError::datatype_mismatch("x")),
            "p",
        );
        assert_eq!(e.wire_code(), "42804");
    }
}
