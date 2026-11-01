//! `sql::policy_eval` の単体テスト（RLS-13、Issue #2277）。PERMISSIVE の OR・RESTRICTIVE の AND・
//! NULL を偽とする扱い・既定拒否・`USING` による代用・`FOR` 句による選択を固定する。

use super::*;
use crate::catalog::policy::PolicyRole;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::row_codec::{encode_scalar_columns, Value};

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("owner", ColumnType::Text, true),
        ],
    )
}

fn policy(
    name: &str,
    permissive: bool,
    command: PolicyCommand,
    using: Option<&str>,
    check: Option<&str>,
) -> PolicyDef {
    PolicyDef {
        id: 0,
        name: name.to_string(),
        permissive,
        command,
        roles: vec![PolicyRole::Public],
        using_sql: using.map(str::to_string),
        check_sql: check.map(str::to_string),
        referenced_columns: Vec::new(),
    }
}

/// `(n, owner)` の行を評価した結果を返す。
fn run(
    policies: &[PolicyDef],
    kind: PolicyCheckKind,
    n: Option<i32>,
    owner: &str,
) -> Result<(), TenantWriteError> {
    let s = schema();
    let compiled = CompiledPolicyChecks::compile("t", &s, policies, kind).expect("compile");
    let meta = encode_scalar_columns(
        &s,
        &[
            n.map_or(Value::Null, Value::Integer),
            Value::Text(owner.to_string()),
        ],
    )
    .expect("encode");
    compiled.enforce(&s, 1, &[], &meta)
}

fn violation(r: Result<(), TenantWriteError>) -> (Option<String>, bool) {
    match r {
        Err(TenantWriteError::RowSecurityPolicyViolation {
            policy,
            using_expression,
            ..
        }) => (policy, using_expression),
        other => panic!("expected violation, got {other:?}"),
    }
}

#[test]
fn no_applicable_permissive_policy_is_default_deny() {
    let k = PolicyCheckKind::InsertCheck;
    assert_eq!(violation(run(&[], k, Some(1), "a")), (None, false));
    // FOR SELECT / FOR DELETE は INSERT の検査に関与しない。
    let ps = [
        policy("s", true, PolicyCommand::Select, Some("n > 0"), None),
        policy("d", true, PolicyCommand::Delete, Some("n > 0"), None),
    ];
    assert!(run(&ps, k, Some(1), "a").is_err());
}

#[test]
fn permissive_policies_are_ored() {
    let k = PolicyCheckKind::InsertCheck;
    let ps = [
        policy("a", true, PolicyCommand::Insert, None, Some("owner = 'x'")),
        policy("b", true, PolicyCommand::All, None, Some("n > 5")),
    ];
    assert!(run(&ps, k, Some(9), "z").is_ok());
    assert!(run(&ps, k, Some(1), "x").is_ok());
    assert!(run(&ps, k, Some(1), "z").is_err());
}

#[test]
fn restrictive_policies_are_anded_and_named() {
    let k = PolicyCheckKind::InsertCheck;
    let ps = [
        policy("p", true, PolicyCommand::All, None, Some("n > 0")),
        policy("r1", false, PolicyCommand::All, None, Some("n < 10")),
        policy(
            "r2",
            false,
            PolicyCommand::Insert,
            None,
            Some("owner = 'x'"),
        ),
    ];
    assert!(run(&ps, k, Some(5), "x").is_ok());
    assert_eq!(
        violation(run(&ps, k, Some(50), "x")),
        (Some("r1".to_string()), false)
    );
    assert_eq!(
        violation(run(&ps, k, Some(5), "y")),
        (Some("r2".to_string()), false)
    );
    // PERMISSIVE の合成が先に評価され、名前なしの違反になる。
    assert_eq!(violation(run(&ps, k, Some(-1), "y")), (None, false));
}

#[test]
fn null_result_is_treated_as_false() {
    let k = PolicyCheckKind::InsertCheck;
    let ps = [policy("p", true, PolicyCommand::All, None, Some("n > 0"))];
    assert!(run(&ps, k, None, "x").is_err());
}

#[test]
fn using_expression_substitutes_for_missing_with_check() {
    let ps = [policy("p", true, PolicyCommand::All, Some("n > 0"), None)];
    assert!(run(&ps, PolicyCheckKind::InsertCheck, Some(1), "x").is_ok());
    assert!(run(&ps, PolicyCheckKind::UpdateCheck, Some(-1), "x").is_err());
    // WITH CHECK があれば USING は使わない。
    let ps = [policy(
        "p",
        true,
        PolicyCommand::Update,
        Some("n > 0"),
        Some("n > 100"),
    )];
    assert!(run(&ps, PolicyCheckKind::UpdateCheck, Some(1), "x").is_err());
    assert!(run(&ps, PolicyCheckKind::UpdateCheck, Some(101), "x").is_ok());
}

#[test]
fn policy_without_expression_is_true() {
    // 式なしの適用 PERMISSIVE は TRUE（「ポリシーなし」の既定拒否とは区別する）。
    let ps = [policy("p", true, PolicyCommand::Insert, None, None)];
    assert!(run(&ps, PolicyCheckKind::InsertCheck, Some(1), "x").is_ok());
    // 適用されない FOR 句の式なしポリシーは「適用ポリシーなし」のまま既定拒否。
    let ps = [policy("p", true, PolicyCommand::Update, None, None)];
    assert!(run(&ps, PolicyCheckKind::InsertCheck, Some(1), "x").is_err());
    // 式を持たない RESTRICTIVE は合成を狭めない（PERMISSIVE が通れば通る）。
    let ps = [
        policy("p", true, PolicyCommand::Insert, None, Some("n > 0")),
        policy("r", false, PolicyCommand::Insert, None, None),
    ];
    assert!(run(&ps, PolicyCheckKind::InsertCheck, Some(1), "x").is_ok());
}

#[test]
fn with_check_only_policy_is_true_for_conflict_using() {
    let ps = [policy("p", true, PolicyCommand::All, None, Some("n > 100"))];
    assert!(run(&ps, PolicyCheckKind::ConflictUpdateUsing, Some(1), "z").is_ok());
    assert!(run(&ps, PolicyCheckKind::ConflictSelectUsing, Some(1), "z").is_ok());
    // WITH CHECK 側は従来どおり評価される。
    assert!(run(&ps, PolicyCheckKind::UpdateCheck, Some(1), "z").is_err());
}

#[test]
fn conflict_using_kinds_select_their_commands_and_flag_using_expression() {
    let ps = [
        policy("u", true, PolicyCommand::Update, Some("n > 0"), None),
        policy("s", true, PolicyCommand::Select, Some("owner = 'x'"), None),
    ];
    assert!(run(&ps, PolicyCheckKind::ConflictUpdateUsing, Some(1), "z").is_ok());
    assert_eq!(
        violation(run(&ps, PolicyCheckKind::ConflictSelectUsing, Some(1), "z")),
        (None, true)
    );
}

#[test]
fn user_policies_apply_only_when_enabled() {
    assert!(!user_policies_apply_for_write(RowSecurityState::default()));
    let s =
        RowSecurityState::default().apply(crate::catalog::row_security::RowSecurityAction::Enable);
    assert!(user_policies_apply_for_write(s));
}
