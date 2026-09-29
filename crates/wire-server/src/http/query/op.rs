//! `POST /v1/query` の受理語彙を許可リストとして表現するモジュール
//! （Issue #759・TASK-179。`update`／`delete` 追加は Issue #875・NOSQL-12。
//! `create_table`／`alter_table`／`drop_table`（DDL 3 op）追加は Issue #910・
//! NOSQL-13・TASK-207。対象ビヘイビア NOSQL-1・NOSQL-9・NOSQL-12・NOSQL-13。
//! ポインタ: `docs/spec/05-tasks.md` TASK-179・TASK-207・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1・NOSQL-9・NOSQL-12・
//! NOSQL-13）。
//!
//! 責務境界: [`super::schema::extract_op`] が取り出した `op` 文字列を、
//! [`Op::parse`] により **完全一致（大文字小文字・trim の読み替えなし）**
//! でこの 9 値のいずれかへ分類する。これ以外の値（UDF 呼び出し・
//! トランザクション制御・`create_index`／`drop_index`／`create_view`／
//! `drop_view`（NOSQL-13 の対象外）・表記揺れをすべて含む）は
//! [`UnsupportedOp`] として `ErrorClass::FeatureNotSupported`（`0A000`）へ
//! fail-closed に写像する。この判定は SQL 表層の許可リスト検証
//! （`engine::sql::allowlist`・SQL-8）と同じ設計原則（許可リスト方式。
//! 「既知の未対応名」を列挙する拒否リストにしない）に従う。
//!
//! `update`／`delete` は語彙へ加わり許可リストを通過し、`where`（単一行・
//! `id` 完全一致形）は Issue #876（TASK-186・NOSQL-6・NOSQL-12）で束縛・
//! 実行結線済み。`filter`（述語形）は実行器未接続（Issue #871 の担当）の
//! ため [`super::gate::handle`] の当該アーム内部で `0A000`／501 のまま
//! 拒否する（実行器なしで成功を偽装しない）。
//!
//! `create_table`／`alter_table`／`drop_table`（Issue #910）は
//! [`super::ddl`] が SQL 表層の DDL と**同一の実行器**
//! （`engine::core::EngineCore::execute_parsed_in_session`）へ、JSON を
//! トークン列へ写像したうえで到達させる（第 2 の DDL 実行器・第 2 の権限
//! 判定を作らない設計）。`alter_table` は `add_column`／`drop_column`
//! （Issue #1167）を実行し、`create_table` の `check` 制約は
//! `0A000` のまま据え置く（`super::ddl` モジュール doc 参照）。
//!
//! [`super::schema::schema_for`] はこのモジュールの表引き（[`Op::schema`]）
//! へ委譲し、`op` 名 → [`super::schema::ObjectSchema`] の対応表を単一情報源
//! として保つ。

use engine::error_format::{ClassifiedError, ErrorClass};

use super::schema::ObjectSchema;

/// `POST /v1/query` の `op` が取りうる閉じた語彙。
///
/// この 9 値以外を表す variant を追加しない（許可リストの意味が崩れる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    Scan,
    Aggregate,
    Insert,
    Update,
    Delete,
    /// `CREATE TABLE`（NOSQL-13・TASK-207、Issue #910）。[`super::ddl`] が
    /// 実行を担う。
    CreateTable,
    /// `ALTER TABLE ... ADD COLUMN`／`DROP COLUMN`（NOSQL-13・
    /// TASK-207、Issue #910）。
    AlterTable,
    /// `DROP TABLE`（NOSQL-13・TASK-207、Issue #910）。
    DropTable,
}

impl Op {
    /// 全 variant（宣言順）。[`super::schema::OP_SCHEMAS`] と名前列が
    /// 順序込みで一致することをテストで固定する。
    pub const ALL: [Op; 9] = [
        Op::Search,
        Op::Scan,
        Op::Aggregate,
        Op::Insert,
        Op::Update,
        Op::Delete,
        Op::CreateTable,
        Op::AlterTable,
        Op::DropTable,
    ];

    /// `raw` を語彙へ分類する。完全一致のみ（大文字小文字の読み替え・
    /// 前後空白のトリムを行わない）。語彙外は `None`。
    pub fn parse(raw: &str) -> Option<Op> {
        match raw {
            "search" => Some(Op::Search),
            "scan" => Some(Op::Scan),
            "aggregate" => Some(Op::Aggregate),
            "insert" => Some(Op::Insert),
            "update" => Some(Op::Update),
            "delete" => Some(Op::Delete),
            "create_table" => Some(Op::CreateTable),
            "alter_table" => Some(Op::AlterTable),
            "drop_table" => Some(Op::DropTable),
            _ => None,
        }
    }

    /// JSON 上の正準名（`op` フィールドの受理値そのもの）。
    pub fn name(self) -> &'static str {
        match self {
            Op::Search => "search",
            Op::Scan => "scan",
            Op::Aggregate => "aggregate",
            Op::Insert => "insert",
            Op::Update => "update",
            Op::Delete => "delete",
            Op::CreateTable => "create_table",
            Op::AlterTable => "alter_table",
            Op::DropTable => "drop_table",
        }
    }

    /// 対応する [`ObjectSchema`]（`super::schema::OP_SCHEMAS` と同一の表を
    /// 参照。表引きの単一情報源は本モジュール）。
    pub fn schema(self) -> &'static ObjectSchema {
        match self {
            Op::Search => &super::schema::SEARCH_SCHEMA,
            Op::Scan => &super::schema::SCAN_SCHEMA,
            Op::Aggregate => &super::schema::AGGREGATE_SCHEMA,
            Op::Insert => &super::schema::INSERT_SCHEMA,
            Op::Update => &super::schema::UPDATE_SCHEMA,
            Op::Delete => &super::schema::DELETE_SCHEMA,
            Op::CreateTable => &super::schema::CREATE_TABLE_SCHEMA,
            Op::AlterTable => &super::schema::ALTER_TABLE_SCHEMA,
            Op::DropTable => &super::schema::DROP_TABLE_SCHEMA,
        }
    }
}

/// 語彙外の `op` を表す分類済みエラー。
///
/// untrusted な `op` 文字列は一切保持しない（固定文言のみを返す）。
/// [`super::schema::SchemaError::UnknownKey`] が理由と同じ:
/// JSON 文字列はバックスラッシュ NUL エスケープ経由で NUL を含み得る
/// ため、untrusted 値をエラー文言へ埋め込むと `error_response::encode` の
/// NUL 拒否で `XX000` へ縮退しうる。`.claude/rules/security.md`
/// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」方針にも
/// 沿う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedOp;

/// 語彙外 `op` に返す固定文言。
pub const UNSUPPORTED_OP_MESSAGE: &str = "unsupported op";

impl ClassifiedError for UnsupportedOp {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::FeatureNotSupported
    }

    fn client_message(&self) -> String {
        UNSUPPORTED_OP_MESSAGE.to_string()
    }
}

/// `raw`（[`super::schema::extract_op`] が返した `op` 文字列）を [`Op`] へ
/// 分類する入口。語彙外は `Err(UnsupportedOp)`。
pub fn classify_op(raw: &str) -> Result<Op, UnsupportedOp> {
    Op::parse(raw).ok_or(UnsupportedOp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::query::schema::OP_SCHEMAS;

    #[test]
    fn parse_accepts_exact_nine_values() {
        assert_eq!(Op::parse("search"), Some(Op::Search));
        assert_eq!(Op::parse("scan"), Some(Op::Scan));
        assert_eq!(Op::parse("aggregate"), Some(Op::Aggregate));
        assert_eq!(Op::parse("insert"), Some(Op::Insert));
        assert_eq!(Op::parse("update"), Some(Op::Update));
        assert_eq!(Op::parse("delete"), Some(Op::Delete));
        assert_eq!(Op::parse("create_table"), Some(Op::CreateTable));
        assert_eq!(Op::parse("alter_table"), Some(Op::AlterTable));
        assert_eq!(Op::parse("drop_table"), Some(Op::DropTable));
    }

    #[test]
    fn parse_rejects_vocabulary_outside_nine_values() {
        let negatives = [
            "",
            " search",
            "search ",
            "SEARCH",
            "Search",
            "select",
            "explain",
            "CREATE_TABLE",
            " create_table",
            "create_table ",
            "ALTER_TABLE",
            "DROP_TABLE",
            "create_index",
            "drop_index",
            "create_view",
            "drop_view",
            "call",
            "udf",
            "begin",
            "commit",
            "rollback",
            "set",
            "UPDATE",
            " update",
            "update ",
            "DELETE",
            " delete",
            "delete ",
            "upsert",
            "truncate",
            "merge",
            "patch",
        ];
        for raw in negatives {
            assert_eq!(Op::parse(raw), None, "unexpected accept for {raw:?}");
        }
    }

    #[test]
    fn classify_op_maps_to_unsupported_op_error() {
        let err = classify_op("begin").unwrap_err();
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
        assert_eq!(err.client_message(), UNSUPPORTED_OP_MESSAGE);
    }

    #[test]
    fn unsupported_op_message_is_always_the_fixed_literal() {
        for raw in ["begin", "create_index", "SEARCH", ""] {
            let err = classify_op(raw).unwrap_err();
            assert_eq!(err.client_message(), "unsupported op");
        }
    }

    #[test]
    fn all_matches_op_schemas_names_in_order() {
        let all_names: Vec<&str> = Op::ALL.iter().map(|op| op.name()).collect();
        let schema_names: Vec<&str> = OP_SCHEMAS.iter().map(|(name, _)| *name).collect();
        assert_eq!(all_names, schema_names);
    }

    #[test]
    fn schema_matches_op_schemas_table_by_pointer() {
        for (name, schema) in OP_SCHEMAS.iter() {
            let op = Op::parse(name).expect("OP_SCHEMAS name must be in vocabulary");
            assert!(std::ptr::eq(op.schema(), *schema));
        }
    }
}
