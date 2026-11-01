//! 利用者定義ポリシー（`CREATE POLICY`）の選択・合成・評価（RLS-13、Issue #2277。設計記録:
//! `docs/design/row-security-policy-composition.md`）。
//!
//! 役割と責務境界:
//! - 書き込み経路の単一の検査点 `constraint::enforce_row_constraints_in_txn`（と `tenant.rs` の
//!   UPSERT）が、同じ write txn の中で [`compile_in_txn`] を呼び、書いた行ごとに
//!   [`CompiledPolicyChecks::enforce`] を呼ぶ。第 2 の評価器は作らず、述語の束縛・評価は
//!   `sql::view_check::CompiledLayer`（`WHERE` と同じ束縛経路）を共有する。
//! - ポリシーの保存・読み出しは `catalog::policy`、有効化状態は `catalog::row_security` の責務。
//!   本ファイルはそれらを読んで「この書き込みで、どの式をどう合成して評価するか」だけを決める。
//!
//! 合成規則: `(PERMISSIVE_1 OR ... OR PERMISSIVE_n) AND RESTRICTIVE_1 AND ...`。式が NULL なら偽、
//! 適用される PERMISSIVE が 0 件なら既定拒否（常に偽）。式を省略した適用ポリシー（`WITH CHECK` を
//! `USING` で代用してもなお式が無い場合を含む）は PostgreSQL と同じく TRUE として合成する
//! （「ポリシーなし」と「式なしポリシー」を区別する）。テナント境界はこの外側で常に適用され、本ファイルの
//! 合成は境界の内側で狭めるだけで、広げる経路を持たない。
//!
//! 暫定（#2278 まで）: ロール・所有者・`BYPASSRLS` の機構が無いため、全セッションを「所有者でも
//! `BYPASSRLS` でもない」として扱う（[`user_policies_apply_for_write`]。狭める側）。

use crate::catalog::policy::{PolicyCommand, PolicyDef};
use crate::catalog::row_security::RowSecurityState;
use crate::catalog::{CatalogError, TableSchema};
use crate::sql::index_predicate::map_predicate_eval_error;
use crate::sql::view_check::CompiledLayer;
use crate::tenant::TenantWriteError;

/// 評価する検査の種類。選ぶ `FOR` 句と、見る式（`WITH CHECK` か `USING` か）が決まる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyCheckKind {
    /// `INSERT` の新しい行（`FOR ALL`・`FOR INSERT` の `WITH CHECK`。無ければ `USING`）。
    InsertCheck,
    /// `UPDATE` の新しい行（`FOR ALL`・`FOR UPDATE` の `WITH CHECK`。無ければ `USING`）。
    UpdateCheck,
    /// `ON CONFLICT DO UPDATE` の既存行に掛ける UPDATE 側の `USING`。
    ConflictUpdateUsing,
    /// `ON CONFLICT DO UPDATE` の既存行に掛ける SELECT 側の `USING`。
    ConflictSelectUsing,
}

impl PolicyCheckKind {
    fn command_applies(self, command: PolicyCommand) -> bool {
        match self {
            Self::InsertCheck => matches!(command, PolicyCommand::All | PolicyCommand::Insert),
            Self::UpdateCheck | Self::ConflictUpdateUsing => {
                matches!(command, PolicyCommand::All | PolicyCommand::Update)
            }
            Self::ConflictSelectUsing => {
                matches!(command, PolicyCommand::All | PolicyCommand::Select)
            }
        }
    }

    /// このポリシーがこの検査で見る式。`WITH CHECK` 系は `WITH CHECK`、無ければ `USING` で代用する。
    fn expr(self, def: &PolicyDef) -> Option<&str> {
        match self {
            Self::InsertCheck | Self::UpdateCheck => {
                def.check_sql.as_deref().or(def.using_sql.as_deref())
            }
            Self::ConflictUpdateUsing | Self::ConflictSelectUsing => def.using_sql.as_deref(),
        }
    }

    fn is_using_expression(self) -> bool {
        matches!(self, Self::ConflictUpdateUsing | Self::ConflictSelectUsing)
    }
}

/// 利用者定義ポリシーを書き込みに適用するか。判定は 1 か所に閉じる（#2278 がここだけを差し替える）。
/// 暫定では全セッションを非所有者・`BYPASSRLS` なしとして扱う。
pub(crate) fn user_policies_apply_for_write(state: RowSecurityState) -> bool {
    state.user_policies_apply(false)
}

/// 合成済みの 1 検査（1 文につき種類ごとに 1 回コンパイルする）。
pub(crate) struct CompiledPolicyChecks {
    table: String,
    using_expression: bool,
    /// 式を省略した PERMISSIVE ポリシーが適用される（TRUE として OR に合成される）。
    permissive_always_true: bool,
    permissive: Vec<CompiledLayer>,
    restrictive: Vec<(String, CompiledLayer)>,
}

fn corrupt_policy() -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::CorruptSchema(
        "invalid row security policy record".to_string(),
    ))
}

impl CompiledPolicyChecks {
    /// `policies`（名前の昇順）から `kind` に該当する式を選んで束縛する。保存済みの式を今のスキーマで
    /// 束縛できないときは `CorruptSchema`（適用しない側へ倒さない）。
    pub(crate) fn compile(
        table: &str,
        schema: &TableSchema,
        policies: &[PolicyDef],
        kind: PolicyCheckKind,
    ) -> Result<Self, TenantWriteError> {
        let mut permissive = Vec::new();
        let mut restrictive = Vec::new();
        let mut permissive_always_true = false;
        for def in policies {
            if !kind.command_applies(def.command) {
                continue;
            }
            let Some(text) = kind.expr(def) else {
                // 式の省略は TRUE。RESTRICTIVE の TRUE は AND に何も足さないので捨ててよい。
                if def.permissive {
                    permissive_always_true = true;
                }
                continue;
            };
            let preds = crate::sql::allowlist::parse_index_predicate_text(text)
                .map_err(|_| corrupt_policy())?;
            let layer = CompiledLayer::bind(&preds, schema).map_err(|_| corrupt_policy())?;
            if def.permissive {
                permissive.push(layer);
            } else {
                restrictive.push((def.name.clone(), layer));
            }
        }
        Ok(Self {
            table: table.to_string(),
            using_expression: kind.is_using_expression(),
            permissive_always_true,
            permissive,
            restrictive,
        })
    }

    /// 1 行が合成を満たすか検査する。PERMISSIVE の合成を先に、続けて RESTRICTIVE を名前順に評価する。
    /// 違反は [`TenantWriteError::RowSecurityPolicyViolation`]、評価エラーは SQLSTATE を保って透過する。
    pub(crate) fn enforce(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Result<(), TenantWriteError> {
        let mut scratch = Vec::new();
        let mut any_permissive = self.permissive_always_true;
        for layer in self.permissive.iter().filter(|_| !any_permissive) {
            if layer
                .eval(schema, id, embedding, metadata, &mut scratch)
                .map_err(map_predicate_eval_error)?
            {
                any_permissive = true;
                break;
            }
        }
        if !any_permissive {
            return Err(self.violation(None));
        }
        for (name, layer) in &self.restrictive {
            if !layer
                .eval(schema, id, embedding, metadata, &mut scratch)
                .map_err(map_predicate_eval_error)?
            {
                return Err(self.violation(Some(name.clone())));
            }
        }
        Ok(())
    }

    fn violation(&self, policy: Option<String>) -> TenantWriteError {
        TenantWriteError::RowSecurityPolicyViolation {
            table: self.table.clone(),
            policy,
            using_expression: self.using_expression,
        }
    }
}

/// write txn の中で表 `table` の有効化状態とポリシーを読み、`kind` の検査をコンパイルする。適用しない
/// （RLS 無効・暫定の所有者扱いで対象外）ときは `None`。`tenant_id` はサーバー側導出の値で、他テナントの
/// 定義・状態は引けない。RLS が無効な表の費用は状態の get 1 回だけ。
pub(crate) fn compile_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    schema: &TableSchema,
    kind: PolicyCheckKind,
) -> Result<Option<CompiledPolicyChecks>, TenantWriteError> {
    let state =
        crate::catalog::row_security::row_security_state_in_txn(write_txn, tenant_id, table)?;
    if !user_policies_apply_for_write(state) {
        return Ok(None);
    }
    let policies = crate::catalog::policy::list_policies_in_txn(write_txn, tenant_id, table)?;
    CompiledPolicyChecks::compile(table, schema, &policies, kind).map(Some)
}

#[cfg(test)]
mod tests;
