//! [`super::bind_merge`] の単体テスト（SQL-79・Issue #3352）。スコープの規則（D4）・relation 添字の規約・
//! 判定順・INSERT の規則を、カタログ・エンジンを使わずスキーマだけで固定する。

use super::*;
use crate::catalog::{ColumnDef, ColumnType, RowIdentity};
use crate::sql::allowlist::{validate_merge_tokens_with_limits, TableLookup};
use crate::sql::lexer;

struct Cat;
impl TableLookup for Cat {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(["t", "s"].contains(&name))
    }
}

fn schema(name: &str, value_col: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("k", ColumnType::Text, false),
            ColumnDef::new(value_col, ColumnType::Integer, true),
        ],
    )
}

fn bind_with(
    sql: &str,
    target: &TableSchema,
    source: &TableSchema,
    flags: &[bool],
) -> Result<BoundMerge, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql).expect("lex");
    let stmt = validate_merge_tokens_with_limits(
        &tokens,
        crate::recovery::required_op_id::LedgerMode::Optional,
        crate::nesting_limits::NestingLimits::default(),
        &Cat,
    )
    .expect("syntax");
    bind_merge(
        &stmt,
        &MergeBindInput {
            target_schema: target,
            target_view: None,
            source_schema: source,
            source_visible: None,
            dummy_flags: flags,
        },
        &UdfRegistry::default(),
    )
}

/// 従来方式（疑似列 `id` あり）の `t` と `s`。
fn bind(sql: &str) -> Result<BoundMerge, SqlSurfaceError> {
    bind_with(sql, &schema("t", "x"), &schema("s", "v"), &[])
}

fn code(sql: &str) -> &'static str {
    bind(sql).expect_err(sql).wire_code()
}

const HEAD: &str = "MERGE INTO t USING s ON t.k = s.k";

#[test]
fn relation_indexes_are_target_then_source() {
    let b = bind(&format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = s.v")).expect("bind");
    assert_eq!(b.relations().len(), 2);
    assert_eq!(b.relations()[0].table(), "t");
    assert_eq!(b.relations()[1].table(), "s");
    let (t, s) = &b.on_keys()[0];
    assert_eq!((t.relation(), s.relation()), (0, 1));
    let BoundMergeAction::Update(a) = b.clauses()[0].action() else {
        panic!("update");
    };
    assert_eq!(a.columns().len(), 1);
    assert_eq!(a.columns()[0].1.relation(), 1);
}

#[test]
fn each_when_kind_sees_only_its_own_side() {
    // MATCHED は両側、BY TARGET は元だけ、BY SOURCE は対象だけ。
    assert!(bind(&format!("{HEAD} WHEN MATCHED AND t.x < s.v THEN DELETE")).is_ok());
    assert_eq!(
        code(&format!(
            "{HEAD} WHEN NOT MATCHED AND t.x < 1 THEN DO NOTHING"
        )),
        "42P01"
    );
    assert_eq!(
        code(&format!(
            "{HEAD} WHEN NOT MATCHED AND x < 1 THEN DO NOTHING"
        )),
        "42703"
    );
    assert_eq!(
        code(&format!(
            "{HEAD} WHEN NOT MATCHED BY SOURCE AND s.v < 1 THEN DELETE"
        )),
        "42P01"
    );
    assert_eq!(
        code(&format!(
            "{HEAD} WHEN NOT MATCHED BY SOURCE AND v < 1 THEN DELETE"
        )),
        "42703"
    );
    // 片側だけで一意な非修飾名は、両側スコープでも誤って曖昧にならない。
    assert!(bind(&format!(
        "{HEAD} WHEN NOT MATCHED BY SOURCE AND x < 1 THEN DELETE"
    ))
    .is_ok());
}

#[test]
fn ambiguous_unqualified_columns_are_42702_only_where_both_sides_are_visible() {
    assert_eq!(
        code("MERGE INTO t USING s ON k = s.k WHEN MATCHED THEN DELETE"),
        "42702"
    );
    assert_eq!(
        code(&format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = k")),
        "42702"
    );
    // 元だけが見える INSERT の値では曖昧にならない。
    let t = schema("t", "x").with_row_identity(RowIdentity::Allocated);
    assert!(bind_with(
        &format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, x) VALUES (k, v)"),
        &t,
        &schema("s", "v"),
        &[]
    )
    .is_ok());
}

#[test]
fn error_order_is_on_then_clauses_in_declaration_order() {
    // ON の未知列が、後ろの句の型エラーより先に返る。
    assert_eq!(
        code("MERGE INTO t USING s ON t.nope = s.k WHEN MATCHED THEN UPDATE SET x = s.k"),
        "42703"
    );
    // 句の条件のエラーが、同じ句の操作のエラーより先に返る。
    assert_eq!(
        code(&format!(
            "{HEAD} WHEN MATCHED AND s.nope = 1 THEN UPDATE SET x = s.k"
        )),
        "42703"
    );
    // 先の句のエラーが後ろの句のエラーより先に返る。
    assert_eq!(
        code(&format!(
            "{HEAD} WHEN MATCHED AND s.v = 1 THEN UPDATE SET x = s.k \
             WHEN NOT MATCHED BY SOURCE AND s.v = 1 THEN DELETE"
        )),
        "42804"
    );
}

#[test]
fn on_keys_require_comparable_types() {
    assert_eq!(
        code("MERGE INTO t USING s ON t.x = s.k WHEN MATCHED THEN DELETE"),
        "42804"
    );
    // 疑似列 id どうしの等値は結合キー候補として受理する。
    let b = bind("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE").expect("bind");
    assert_eq!(b.on_keys().len(), 1);
    // 元の疑似列 id を残余の条件に使う形は固定文言の 0A000（MERGE 用の文言）。
    let e = bind("MERGE INTO t USING s ON t.k = s.k AND s.id > 1 WHEN MATCHED THEN DELETE")
        .expect_err("id in residual");
    assert_eq!(e.wire_code(), "0A000");
    assert!(
        e.client_message().contains("MERGE"),
        "{}",
        e.client_message()
    );
}

#[test]
fn insert_follows_the_single_insert_rules() {
    let legacy = schema("t", "x");
    let allocated = schema("t", "x").with_row_identity(RowIdentity::Allocated);
    let sql = format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, x) VALUES (s.k, s.v)");
    // 従来方式の表は疑似列 id の省略が 22000。採番方式は受理する。
    assert_eq!(
        bind_with(&sql, &legacy, &schema("s", "v"), &[])
            .expect_err("legacy")
            .wire_code(),
        "22000"
    );
    let b = bind_with(&sql, &allocated, &schema("s", "v"), &[]).expect("allocated");
    let BoundMergeAction::Insert(i) = b.clauses()[0].action() else {
        panic!("insert");
    };
    assert_eq!(i.slots().len(), 2);
    assert!(!i.id_omitted() || allocated.has_pseudo_id());
    // 列リストの重複は 22000、値の個数の不一致は 42601（構文段が検査済みの形は構文段で返る）。
    assert_eq!(
        bind_with(
            &format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, k) VALUES (s.k, s.k)"),
            &allocated,
            &schema("s", "v"),
            &[]
        )
        .expect_err("dup")
        .wire_code(),
        "22000"
    );
    // 列リスト省略は宣言順の先頭へ当てる。
    assert!(bind_with(
        &format!("{HEAD} WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v)"),
        &allocated,
        &schema("s", "v"),
        &[]
    )
    .is_ok());
    assert_eq!(
        bind_with(
            &format!("{HEAD} WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 1)"),
            &allocated,
            &schema("s", "v"),
            &[]
        )
        .expect_err("too many")
        .wire_code(),
        "42601"
    );
}

/// 裸の `DEFAULT` は通常の INSERT と同じく列の省略と等価で、GENERATED ALWAYS 列でも受理する
/// （SET の `DEFAULT` 規則の 428C9・0A000 には従わない）。利用者指定の値は 428C9 のまま。
#[test]
fn insert_bare_default_is_an_omitted_column() {
    let always = ColumnDef::new("g", ColumnType::BigInt, false).with_default(
        crate::catalog::ColumnDefault::Identity {
            sequence: "g_seq".to_string(),
            kind: crate::catalog::IdentityKind::Always,
        },
    );
    let mut cols = vec![ColumnDef::new("k", ColumnType::Text, false), always];
    cols.push(ColumnDef::new("x", ColumnType::Integer, true).with_default(
        crate::catalog::ColumnDefault::CurrentTime(crate::catalog::TimeDefault::Now),
    ));
    let target = TableSchema::new("t", cols).with_row_identity(RowIdentity::Allocated);
    let src = schema("s", "v");
    let b = bind_with(
        &format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, g, x) VALUES (s.k, DEFAULT, DEFAULT)"),
        &target,
        &src,
        &[],
    );
    assert!(b.is_ok(), "{:?}", b.err());
    let err = bind_with(
        &format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, g) VALUES (s.k, 1)"),
        &target,
        &src,
        &[],
    )
    .expect_err("explicit value");
    assert_eq!(err.wire_code(), "428C9");
}

#[test]
fn dummy_flags_defer_value_validation_only_when_the_count_matches() {
    // `$n` のダミー（文字列リテラル）を整数列へ代入する。フラグが合えば値の検証を遅延する。
    let sql = format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = 'dummy'");
    let (t, s) = (schema("t", "x"), schema("s", "v"));
    assert_eq!(
        bind_with(&sql, &t, &s, &[])
            .expect_err("validated")
            .wire_code(),
        "22P02"
    );
    assert!(bind_with(&sql, &t, &s, &[true]).is_ok());
    // 個数が合わないフラグは使わない（全て検証側へ倒す）。
    assert_eq!(
        bind_with(&sql, &t, &s, &[true, true])
            .expect_err("mismatched flags")
            .wire_code(),
        "22P02"
    );
}

#[test]
fn set_left_hand_side_rules_match_update() {
    for (sql, want) in [
        (
            format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = 1, x = 2"),
            "22000",
        ),
        (
            format!("{HEAD} WHEN MATCHED THEN UPDATE SET id = 1"),
            "42601",
        ),
        (
            format!("{HEAD} WHEN MATCHED THEN UPDATE SET nope = 1"),
            "42703",
        ),
    ] {
        assert_eq!(code(&sql), want, "{sql}");
    }
}

/// 移行済みの表では `id` への裸の `DEFAULT` も `id` の省略として束縛される（単発の INSERT と同じ）。
/// 従来方式の表では省略できないため、固定文言の `0A000` のまま拒否する。
#[test]
fn insert_id_bare_default_is_omission_on_migrated_tables() {
    let sql = format!("{HEAD} WHEN NOT MATCHED THEN INSERT (id, k) VALUES (DEFAULT, s.k)");
    let src = schema("s", "v");
    let migrated = schema("t", "x").with_row_identity(RowIdentity::MigratedId);
    let b = bind_with(&sql, &migrated, &src, &[]).expect("migrated id DEFAULT");
    let ins = b
        .clauses
        .iter()
        .find_map(|c| match &c.action {
            BoundMergeAction::Insert(i) => Some(i),
            _ => None,
        })
        .expect("insert clause");
    assert!(ins.id_omitted);
    assert!(ins.id_value.is_none());
    let legacy = bind_with(&sql, &schema("t", "x"), &src, &[]).expect_err("legacy id DEFAULT");
    assert_eq!(legacy.wire_code(), "0A000");
}

/// 生成列への `SET g = DEFAULT` は受理し（代入を省略）、値の指定は 428C9 のまま（`UPDATE ... FROM` と同じ）。
#[test]
fn set_default_on_generated_column_is_accepted_but_values_are_rejected() {
    let mut g = ColumnDef::new("g", ColumnType::Integer, true);
    g.generated = Some(crate::catalog::GeneratedColumnDef {
        expression: "x + 1".to_string(),
        referenced_columns: vec!["x".to_string()],
    });
    let mut target = schema("t", "x");
    target.columns.push(g);
    let source = schema("s", "v");
    let ok = bind_with(
        &format!("{HEAD} WHEN MATCHED THEN UPDATE SET g = DEFAULT"),
        &target,
        &source,
        &[],
    );
    assert!(ok.is_ok(), "{:?}", ok.err());
    let e = bind_with(
        &format!("{HEAD} WHEN MATCHED THEN UPDATE SET g = 1"),
        &target,
        &source,
        &[],
    )
    .expect_err("value for generated column");
    assert_eq!(e.wire_code(), "428C9");
}

/// ハッシュできない型（浮動小数）どうしの列等値は結合キーにせず残余へ回す。
#[test]
fn non_hashable_equality_is_not_a_join_key() {
    let mk = |n: &str| {
        TableSchema::new(
            n,
            vec![
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("f", ColumnType::Double, true),
            ],
        )
    };
    let b = bind_with(
        "MERGE INTO t USING s ON t.f = s.f WHEN MATCHED THEN DELETE",
        &mk("t"),
        &mk("s"),
        &[],
    )
    .expect("bind");
    assert!(b.on_keys().is_empty());
}
