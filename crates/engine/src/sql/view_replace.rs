//! `CREATE OR REPLACE VIEW` の互換性検査（TABLE-41 (2)・Issue #1660。ポインタ:
//! `docs/spec/04-behavior/data-model.md`）。
//!
//! 役割: 既存ビューの出力列と新定義の出力列を突き合わせ、「既存列を同じ名前・同じ型・同じ順で
//! 保ち、末尾に列を足すだけ」の置換だけを通す純粋関数を持つ。列の導出（カタログ・Describe の
//! 呼び出し）は `core::EngineCore::view_output_columns` が担い、本モジュールはカタログに触れない。
//! 判定不能な場合（静的な型を持たない列）は受理側に倒さず `0A000` で拒否する（fail-closed）。

use super::allowlist::SqlSurfaceError;
use crate::catalog::ColumnType;

/// ビューの出力列 1 つ（名前と静的な型）。`ty` が `None` の列は静的な型を持たない
/// （ベクトル式など）ため、同一性を検証できない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewColumn {
    pub(crate) name: String,
    pub(crate) ty: Option<ColumnType>,
}

/// 既存ビューの出力列 `old` に対し、新定義の出力列 `new` が置換可能かを判定する。
///
/// 列数の減少・位置ごとの名前不一致（改名・順序変更）・型不一致は `42P16`。
/// 比較対象の位置に静的な型を持たない列があれば `0A000`。末尾への列追加は許可する。
/// エラー文言は英語固定＋クライアント自身が書いた列名のみで、他テナントの情報を含めない。
pub(crate) fn check_replace_compatible(
    old: &[ViewColumn],
    new: &[ViewColumn],
) -> Result<(), SqlSurfaceError> {
    if new.len() < old.len() {
        return Err(SqlSurfaceError::InvalidViewReplacement {
            detail: "cannot drop columns from view".to_string(),
        });
    }
    for (o, n) in old.iter().zip(new.iter()) {
        if o.name != n.name {
            return Err(SqlSurfaceError::InvalidViewReplacement {
                detail: format!(
                    "cannot change name of view column \"{}\" to \"{}\"",
                    o.name, n.name
                ),
            });
        }
        match (&o.ty, &n.ty) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => {
                return Err(SqlSurfaceError::InvalidViewReplacement {
                    detail: format!("cannot change data type of view column \"{}\"", o.name),
                });
            }
            _ => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "cannot verify type compatibility of a view column without a static type",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: Option<ColumnType>) -> ViewColumn {
        ViewColumn {
            name: name.to_string(),
            ty,
        }
    }

    fn text(name: &str) -> ViewColumn {
        col(name, Some(ColumnType::Text))
    }

    fn is_42p16(r: Result<(), SqlSurfaceError>) -> bool {
        matches!(r, Err(SqlSurfaceError::InvalidViewReplacement { .. }))
    }

    #[test]
    fn identical_and_appended_columns_are_compatible() {
        let old = [text("a"), text("b")];
        assert!(check_replace_compatible(&old, &old).is_ok());
        assert!(check_replace_compatible(&old, &[text("a"), text("b"), text("c")]).is_ok());
        assert!(check_replace_compatible(&[], &[text("x")]).is_ok());
    }

    #[test]
    fn dropping_renaming_and_reordering_are_rejected() {
        let old = [text("a"), text("b")];
        assert!(is_42p16(check_replace_compatible(&old, &[text("a")])));
        assert!(is_42p16(check_replace_compatible(
            &old,
            &[text("a"), text("z")]
        )));
        assert!(is_42p16(check_replace_compatible(
            &old,
            &[text("b"), text("a")]
        )));
    }

    #[test]
    fn type_changes_including_typmod_are_rejected() {
        let old = [col("a", Some(ColumnType::Varchar(Some(5))))];
        assert!(is_42p16(check_replace_compatible(
            &old,
            &[col("a", Some(ColumnType::Varchar(Some(6))))]
        )));
        assert!(is_42p16(check_replace_compatible(
            &old,
            &[col("a", Some(ColumnType::Integer))]
        )));
    }

    #[test]
    fn columns_without_static_type_fail_closed_as_unsupported() {
        let old = [col("a", None)];
        assert!(matches!(
            check_replace_compatible(&old, &[col("a", None)]),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
        assert!(matches!(
            check_replace_compatible(&[text("a")], &[col("a", None)]),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
    }
}
