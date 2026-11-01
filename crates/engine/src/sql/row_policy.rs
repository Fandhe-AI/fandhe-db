//! 利用者定義の行セキュリティポリシー（`CREATE POLICY ... USING`）を読み取り・UPDATE・DELETE の対象行選択へ
//! 適用する評価段（RLS-13 のポインタ、Issue #2276。設計記録: `docs/design/row-security-policy-composition.md`）。
//!
//! 役割と責務境界:
//! - 各実行器（`sql::scan`・`sql::aggregate`・`sql::group_by`・`sql::window`・`sql::relation_snapshot`、
//!   `tenant` の述語形 DML 列挙・id 形 DML・`core` の点取得）が、行を読むのと同じスナップショットから
//!   [`resolve_row_policy`] で [`BoundRowPolicy`] を 1 回だけ解決し、各行へ [`BoundRowPolicy::passes`] を
//!   適用する。評価順は「テナント境界（`PolicyContext::is_visible`）→ ポリシー → 利用者の `WHERE`」で、
//!   ポリシーは常にテナント境界の内側で AND され、行を狭めることしかできない。
//! - ポリシーは文の `WHERE` の AST へ混ぜない。混ぜると、評価順（利用者のエラーを起こしうる式が先に走ると
//!   隠れた行の存在がエラーで推測できる）と台帳の内容ハッシュ（文の AST から出す）の両方が崩れるため。
//!   式の評価は `sql::where_tree` の既存評価器を再利用し、第 2 の評価器は作らない。
//! - 合成規則は PostgreSQL 17 と同じ。`(PERMISSIVE の OR) AND RESTRICTIVE の AND`、適用される PERMISSIVE が
//!   1 件も無ければ全行拒否、式の NULL は偽。式を省略した PERMISSIVE は TRUE の分岐として OR に加わる。
//! - 所有者の扱いは暫定で、常に「所有者ではない」として適用する（狭める側＝fail-closed）。初期ロールの
//!   `BYPASSRLS`・所有者除外・`row_security` は #2278 がこの前段へ足す。
//!
//! 適用できない経路（ランキング付き検索・索引・キャッシュ依存の経路。#2279 の担当）は、適用表に限り
//! [`reject_if_policies_apply`] で固定文言の `0A000` として拒否し、ポリシーを黙って素通りさせない。

use crate::catalog::policy::{list_policies_in_source, PolicyCommand, PolicyDef};
use crate::catalog::row_security::row_security_state_in_txn;
use crate::catalog::TableSchema;
use crate::policy::PolicyContext;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::expr_program::StackValue;
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};
use crate::storage::read_source::ReadSource;

/// 適用できない経路を拒否するときの固定文言（表名・ポリシー名・式を含めない）。
const MSG_NOT_SUPPORTED: &str =
    "row-level security policies are not yet supported for this query form";
/// ポリシーの解決・再束縛に失敗したときの固定文言（`XX000`。式本文や名前を含めない）。
const MSG_UNRESOLVABLE: &str = "row-level security policy could not be applied";

/// ポリシーを選ぶ文の種別。UPDATE・DELETE は、既存行を読む（`WHERE` が列を参照する・`RETURNING` がある・
/// `SET` の右辺が列を参照する）ときに限り、SELECT のポリシーも AND で加わる（PostgreSQL 17 と同じ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyCmd {
    Select,
    Update { reads_existing: bool },
    Delete { reads_existing: bool },
}

impl PolicyCmd {
    /// 評価の層ごとのコマンド（`FOR ALL` は各層へ常に加わる）。
    fn layers(self) -> Vec<PolicyCommand> {
        match self {
            Self::Select => vec![PolicyCommand::Select],
            Self::Update { reads_existing } => {
                let mut v = vec![PolicyCommand::Update];
                if reads_existing {
                    v.push(PolicyCommand::Select);
                }
                v
            }
            Self::Delete { reads_existing } => {
                let mut v = vec![PolicyCommand::Delete];
                if reads_existing {
                    v.push(PolicyCommand::Select);
                }
                v
            }
        }
    }
}

/// 束縛済みの利用者ポリシー（1 文・1 表・1 テナント分）。各層は 1 本の連言へ畳み済みで、全層を満たした
/// 行だけが通る。層が 1 つでも「適用される PERMISSIVE なし」なら全行拒否。
#[derive(Debug)]
pub(crate) struct BoundRowPolicy {
    deny_all: bool,
    layers: Vec<BoundConjunction>,
}

/// ポリシー段が行のデコードへ求める列・embedding。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PolicyNeeds {
    /// スカラー列を 1 つ以上参照する（`scalar_mask` へ反映した）。
    pub(crate) scalar: bool,
    /// `VECTOR` 列を参照する式を持つ（embedding のデコードが要る）。
    pub(crate) embedding: bool,
}

impl BoundRowPolicy {
    /// ポリシー段が参照するスカラー列を `mask` へ反映し、必要なデコード段階を返す。マスク外の列を実 NULL と
    /// 取り違えさせない（fail-closed）ため、各走査はポリシー適用時に必ず通す。
    pub(crate) fn mark_needs(&self, mask: &mut [bool]) -> PolicyNeeds {
        let mut needs = PolicyNeeds::default();
        for layer in &self.layers {
            layer.visit_column_indices(&mut |i| {
                if let Some(slot) = mask.get_mut(i) {
                    *slot = true;
                }
                needs.scalar = true;
            });
            if layer.references_embedding() {
                needs.embedding = true;
            }
        }
        needs
    }

    /// 行がポリシーを通るか。`scanned` は [`Self::mark_needs`] を反映したマスクでデコードした値。
    /// 式の NULL は偽（`where_tree` の既存評価器の契約）。
    pub(crate) fn passes(
        &self,
        scanned: &[Option<ScalarRef<'_>>],
        id: u64,
        embedding: &[f32],
        dim: usize,
        scratch: &mut Vec<StackValue>,
    ) -> Result<bool, SqlSurfaceError> {
        if self.deny_all {
            return Ok(false);
        }
        for layer in &self.layers {
            if !layer.matches(scanned, id, embedding, dim, scratch)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 全行を拒否するポリシーか（テスト・計画判定用）。
    #[cfg(test)]
    pub(crate) fn is_deny_all(&self) -> bool {
        self.deny_all
    }
}

fn unresolvable() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: MSG_UNRESOLVABLE.to_string(),
    }
}

/// `scope`（テナント）の表 `table_key` に利用者ポリシーが適用されるか。判定は書き込みと同じ単一の判定点
/// （`policy_applicability::decide`）で行い、RLS 有効かつ `row_security = off` のときは行の探索・返却より前に
/// `42501`（`RowSecurityOffRejected`）で拒否する（部分的な結果を黙って返さない。RLS-13・Issue #2278）。
/// 所有者の扱いは暫定で常に `false`（モジュールドキュメント参照）。
pub(crate) fn policies_apply<R: ReadSource>(
    src: &R,
    ctx: &PolicyContext,
    table_key: &str,
) -> Result<bool, SqlSurfaceError> {
    use crate::sql::policy_applicability::PolicyDecision;
    let state =
        row_security_state_in_txn(src, ctx.tenant_id(), table_key).map_err(|_| unresolvable())?;
    match crate::sql::policy_eval::user_policies_decision_for_write(state) {
        PolicyDecision::Skip => Ok(false),
        PolicyDecision::Apply => Ok(true),
        PolicyDecision::RejectRowSecurityOff => Err(SqlSurfaceError::RowSecurityOffRejected {
            table: table_key.to_string(),
        }),
    }
}

/// ポリシーを適用できない経路の入口に置く。適用表なら固定文言の `0A000`、そうでなければ何もしない。
pub(crate) fn reject_if_policies_apply<R: ReadSource>(
    src: &R,
    ctx: &PolicyContext,
    table_key: &str,
) -> Result<(), SqlSurfaceError> {
    if policies_apply(src, ctx, table_key)? {
        return Err(SqlSurfaceError::feature_not_supported(MSG_NOT_SUPPORTED));
    }
    Ok(())
}

/// 保存済みの正規化テキストを untrusted として再解析・再束縛し、連言 1 本にする。
fn bind_conjunction(sql: &str, schema: &TableSchema) -> Result<BoundConjunction, SqlSurfaceError> {
    let preds =
        crate::sql::allowlist::parse_index_predicate_text(sql).map_err(|_| unresolvable())?;
    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata, exprs, rls_present, or_filters) =
        crate::sql::parser::bind_where_predicates(&preds, schema, &udfs, &mut budget, &[])
            .map_err(|_| unresolvable())?;
    // 部分クエリ・`visible()` は DDL が拒否済み。出てきたら破損として拒否する（fail-closed）。
    if rls_present {
        return Err(unresolvable());
    }
    Ok(BoundConjunction::new(metadata, exprs, or_filters))
}

/// 1 層（1 コマンド分）を合成する。`None` は全行拒否（適用される PERMISSIVE が無い）。
fn compose_layer(
    defs: &[PolicyDef],
    command: PolicyCommand,
    schema: &TableSchema,
) -> Result<Option<BoundConjunction>, SqlSurfaceError> {
    let mut permissive: Vec<BoundConjunction> = Vec::new();
    let mut restrictive: Vec<BoundConjunction> = Vec::new();
    for def in defs {
        if def.command != command && def.command != PolicyCommand::All {
            continue;
        }
        // 式を省略した適用ポリシーは TRUE として合成する（書き込み側 `sql::policy_eval` と同じ意味）。
        // PERMISSIVE の TRUE は OR を恒真にして「適用される PERMISSIVE なし」の既定拒否と区別する。
        // RESTRICTIVE の TRUE は AND に何も足さないので捨てる。
        let Some(sql) = def.using_sql.as_deref() else {
            if def.permissive {
                permissive.push(BoundConjunction::new(Vec::new(), Vec::new(), Vec::new()));
            }
            continue;
        };
        let conj = bind_conjunction(sql, schema)?;
        if def.permissive {
            permissive.push(conj);
        } else {
            restrictive.push(conj);
        }
    }
    if permissive.is_empty() {
        return Ok(None);
    }
    let mut groups = vec![BoundOrGroup::new(permissive)];
    for r in restrictive {
        groups.push(BoundOrGroup::new(vec![r]));
    }
    Ok(Some(BoundConjunction::new(Vec::new(), Vec::new(), groups)))
}

/// `src` から `ctx` のテナントの表 `table_key` のポリシーを解決する。適用されなければ `None`。
///
/// 行を読むのと同じスナップショット（読み取り txn・書き込み txn・明示トランザクションの窓口）から
/// 有効化状態と定義を引くため、TOCTOU を作らない。再束縛の失敗・破損は固定文言の `XX000` で拒否し、
/// ポリシーを無視して素通しにはしない。`schema` はその表の現行スキーマ。
pub(crate) fn resolve_row_policy<R: ReadSource>(
    src: &R,
    ctx: &PolicyContext,
    schema: &TableSchema,
    table_key: &str,
    cmd: PolicyCmd,
) -> Result<Option<BoundRowPolicy>, SqlSurfaceError> {
    if !policies_apply(src, ctx, table_key)? {
        return Ok(None);
    }
    let defs =
        list_policies_in_source(src, ctx.tenant_id(), table_key).map_err(|_| unresolvable())?;
    let mut layers = Vec::new();
    let mut deny_all = false;
    for command in cmd.layers() {
        match compose_layer(&defs, command, schema)? {
            Some(layer) => layers.push(layer),
            None => {
                deny_all = true;
                break;
            }
        }
    }
    if deny_all {
        layers.clear();
    }
    Ok(Some(BoundRowPolicy { deny_all, layers }))
}

#[cfg(test)]
mod tests;
