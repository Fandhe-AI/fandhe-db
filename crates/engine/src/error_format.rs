//! エラー分類共通形式（TASK-152。対象ビヘイビア: ERR-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-152・`docs/spec/04-behavior/error-format.md`）。
//!
//! 責務境界: engine 各所（`sql::allowlist::SqlSurfaceError`・`tenant::TenantWriteError`
//! 等）が独立に持つ `wire_code()` 実装を、本モジュールの [`ErrorClass`] へ委譲させる
//! ための単一真実源（SSOT）を提供する。`wire_code` 写像が複数箇所へ文字列リテラルで
//! 分散し、決定的分類（同一入力に常に同一 `wire_code`）・一意対応（分類⇔`wire_code`）が
//! 構造的に保証されない状態を防ぐことが目的。
//!
//! 収録範囲は「engine・wire-server が現に返している `wire_code`」を基本とし、TASK-153
//! （対象ビヘイビア ERR-1）が wire-server 側の横断写像（`wire-server/src/
//! error_response.rs`）の網羅対象として `AuthRequired`（`28000`）を追加した。送出経路は
//! TASK-174／HTTP-8（Issue #753）で wire-server の `http::session::close`／`http::
//! session::bearer` から接続済み。分類の追加自体は TASK-101
//! （`operation_id` 内容照合。RECOVER-10）で行った。「他分類（特に `23505`）へ写像しない」
//! ことの正式検証は TASK-154（対象ビヘイビア ERR-3）が担い、`tests/error_format_err3.rs`
//! の結合テストで検証済み。分類の定義そのものは spec 側の管理事項であり、本コメント・
//! 本モジュールへ転記しない（`.claude/rules/spec-confidentiality.md`）。
//!
//! 分類リストは [`define_error_classes`] マクロの 1 箇所のみで宣言し、`ErrorClass` の
//! 定義・`ALL`・`wire_code`・`label` をそこから生成する。分類を追加・削除すると `ALL` の
//! 固定長（`count`）が合わなくなりコンパイルが失敗するため、「分類は増えたが `ALL` の
//! 更新を忘れる」乖離は構造的に発生しない。
//!
//! wire-server 側（`SQLSTATE_*` 定数・`ErrorResponse` 整形）は TASK-153 が
//! `wire-server/src/error_response.rs` として実装済みで、本モジュールはそれらを
//! 直接呼び出さない（workspace 責務境界: `.claude/rules/coding-rust.md`）。

/// [`ErrorClass`] の宣言と写像（`ALL`・`wire_code`・`label`）を**単一の分類リスト**から
/// 生成するマクロ。分類・`wire_code`・ラベルを 1 箇所に集約し、リスト間の乖離（分類を
/// 追加したのに `ALL` へ足し忘れる等）を構造的に起こせなくする（ERR-2 の一意対応・
/// 決定的分類を型で担保するのが本モジュールの責務）。`count` は `ALL` の固定長配列長であり、分類を増減させたのに更新しなければ
/// 配列長不一致でコンパイルが失敗する。
macro_rules! define_error_classes {
    (
        count = $count:literal;
        $(
            $(#[$variant_doc:meta])*
            $variant:ident => ($wire_code:literal, $label:literal),
        )+
    ) => {
        /// エラー分類の共通表現。engine・wire-server が現に返す `wire_code` に 1 対 1 で
        /// 対応する（ERR-2。ポインタ: `docs/spec/04-behavior/error-format.md`）。
        ///
        /// `#[non_exhaustive]` は付けない。分類の追加は
        /// [`define_error_classes`] のリストへの 1 行追加としてのみ行い、
        /// `wire_code`／`label`／`ALL` は同リストから生成されるため
        /// 更新漏れが起こり得ない（`StorageError` と同じ「網羅 `match` を強制する」既定方針）。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ErrorClass {
            $( $(#[$variant_doc])* $variant, )+
        }

        impl ErrorClass {
            /// 全分類。テストでの網羅・一意性検証、
            /// [`ErrorClass::from_wire_code`] の逆引きに使う。
            pub const ALL: [ErrorClass; $count] = [ $( ErrorClass::$variant, )+ ];

            /// SQLSTATE 風の 5 文字コード。ERR-2 が確定として保証する契約そのもの。
            /// 外部状態・時刻・乱数を参照しない純粋な `match`（決定的分類の保証）。
            pub const fn wire_code(self) -> &'static str {
                match self { $( ErrorClass::$variant => $wire_code, )+ }
            }

            /// 非規範の人間可読ラベル（`SCREAMING_SNAKE_CASE`）。診断・ログ用途に限り、
            /// wire プロトコル応答の契約には含めない（確定契約は `wire_code` のみ）。
            pub const fn label(self) -> &'static str {
                match self { $( ErrorClass::$variant => $label, )+ }
            }

        }
    };
}

/// `wire_code` の重複を明示的に許容する集合（ERR-6。TABLE-16・TASK-204、
/// Issue #904）。PostgreSQL の `not_null_violation` 相当コード `23502` は
/// 本リポでは先に `USING OPERATION_ID` 句の省略（[`ErrorClass::
/// MissingOperationId`]）へ割り当て済みだったため、新規に追加する NOT NULL
/// 違反（[`ErrorClass::NotNullViolation`]）も同じ `wire_code` を共有し、`code`
/// ラベル（[`ErrorClass::label`]）でのみ区別する。本モジュール下部の
/// `wire_codes_are_pairwise_distinct_except_shared_wire_codes`
/// テストがこの集合に載っていない `wire_code` の重複だけを偶発的な乖離として
/// 検出する（`23505`（`UniqueViolation`）のように単一分類が複数原因を束ねる
/// 既存パターンとは異なり、`SHARED_WIRE_CODES` は「分類そのものが複数、
/// `wire_code` のみ共有」のケース専用）。
///
/// 本体は `#[cfg(test)]` の単体テストからのみ参照される（`pub(crate)` のため
/// integration test crate からは到達できない）。通常ビルドでは未参照のため
/// `dead_code` を明示的に許容する。
#[allow(dead_code)]
pub(crate) const SHARED_WIRE_CODES: &[&str] = &["23502"];

define_error_classes! {
    count = 35;

    /// 構文上受理された SQL の値・引数が不正（`22000`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::InvalidInput`] の写像。
    InvalidInput => ("22000", "INVALID_INPUT"),
    /// 認証資格情報が無効（`28P01`）。wire-server の `auth::SQLSTATE_INVALID_PASSWORD`
    /// に対応する分類（engine 側に発生経路はなく、写像の集約のみ）。
    AuthInvalid => ("28P01", "AUTH_INVALID"),
    /// 認証資格情報が提示されなかった（`28000`）。TASK-153（対象ビヘイビア ERR-1）が
    /// wire-server 側の横断写像（`wire-server/src/error_response.rs`）の網羅対象へ
    /// 追加した分類。送出経路は TASK-174／HTTP-8（Issue #753）で wire-server の
    /// `http::session::close`／`http::session::bearer` から接続済み
    /// （`Authorization: Bearer` の欠落・不正・失効済み・二重 close、
    /// `POST /v1/session/close` の再送）。
    AuthRequired => ("28000", "AUTH_REQUIRED"),
    /// テナント帰属不一致（`42501`）。[`crate::tenant::TenantWriteError::Forbidden`]
    /// の写像。SQL-23・TASK-202・TASK-203（Issue #899・#902）の DDL 実行権限
    /// 不足（[`crate::sql::allowlist::SqlSurfaceError::InsufficientPrivilege`]）も
    /// 同分類へ写像する（`UniqueViolation` が複数原因を束ねているのと同じ運用）。
    ForbiddenTenantMismatch => ("42501", "FORBIDDEN_TENANT_MISMATCH"),
    /// 参照したテーブルがカタログ未存在（`42P01`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::UndefinedTable`] の写像。
    TableNotFound => ("42P01", "TABLE_NOT_FOUND"),
    /// 指定した行が存在しない（`P0002`）。[`crate::tenant::TenantWriteError::NotFound`]
    /// の写像。
    RowNotFound => ("P0002", "ROW_NOT_FOUND"),
    /// 一意制約の衝突（`23505`）。行キー `(tenant_id, id)` の衝突
    /// （[`crate::tenant::TenantWriteError::IdConflict`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::IdConflict`]）と、`operation_id` の
    /// 重複（TASK-93 の台帳）が共通で属する分類。原因を限定した命名にすると
    /// 別原因の衝突を誤った意味論で運ぶため、`23505` の意味そのもので命名する。
    UniqueViolation => ("23505", "UNIQUE_VIOLATION"),
    /// `USING OPERATION_ID` 句の省略（`23502`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::MissingOperationId`]・
    /// [`crate::tenant::TenantWriteError::MissingOperationId`] の写像。
    MissingOperationId => ("23502", "MISSING_OPERATION_ID"),
    /// untrusted 入力のサイズがアロケーション前の上限を超過（`54000`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::PayloadTooLarge`]・wire-server の
    /// `framing::SQLSTATE_PROGRAM_LIMIT_EXCEEDED` に対応する。
    PayloadTooLarge => ("54000", "PAYLOAD_TOO_LARGE"),
    /// 接続数上限超過（`53300`）。wire-server の `limits::SQLSTATE_TOO_MANY_CONNECTIONS`
    /// に対応する分類（engine 側に発生経路はなく、写像の集約のみ）。
    ConnectionLimitExceeded => ("53300", "CONNECTION_LIMIT_EXCEEDED"),
    /// 未対応のプロトコル機能（`0A000`）。wire-server の
    /// `handshake::SQLSTATE_FEATURE_NOT_SUPPORTED`・`protocol_dispatch` に対応する分類。
    /// engine 側では [`crate::sql::statement_splitter::MultiStatementError::
    /// WriteNotLast`]（WIRE-16・TASK-219。書き込み系文が複数文メッセージの最後
    /// 以外にある場合の拒否）が唯一の発生経路。
    FeatureNotSupported => ("0A000", "FEATURE_NOT_SUPPORTED"),
    /// 受理範囲外の SQL 構文（構文解析失敗・AST 許可リスト外。`42601`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::UnsupportedSyntax`] の写像。
    /// [`crate::json::JsonError`]（重複キー・非 RFC 8259 数値等。Issue #732・
    /// TASK-172・NOSQL-8 ポインタ）もこの分類へ写像する。
    UnsupportedSqlSyntax => ("42601", "UNSUPPORTED_SQL_SYNTAX"),
    /// プロトコル違反（`08P01`）。wire-server の `framing::SQLSTATE_PROTOCOL_VIOLATION`
    /// に対応する分類（engine 側に発生経路はなく、写像の集約のみ）。
    ProtocolViolation => ("08P01", "PROTOCOL_VIOLATION"),
    /// 予期しない内部エラー（`XX000`）。クライアントへは詳細を運ばない
    /// （[`WireError::internal`] 参照）。
    /// [`crate::sql::allowlist::SqlSurfaceError::Internal`]・
    /// [`crate::tenant::TenantWriteError::Catalog`]／`Storage` の写像。
    InternalError => ("XX000", "INTERNAL_ERROR"),
    /// 数値演算が表現範囲を超過（`22003`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::NumericOutOfRange`]（SQL-13 の集計関数）
    /// の写像。
    NumericOutOfRange => ("22003", "NUMERIC_OUT_OF_RANGE"),
    /// 台帳（TASK-93）に記録済みの `operation_id` へ、内容が異なる書き込みが再送された
    /// （`22023`）。TASK-101（RECOVER-10）が追加。ハッシュ一致の証明が取れない場合は
    /// 常にこちら側へ倒す（fail-closed。commit 済み確定の根拠にしない）。他のいかなる
    /// 分類（特に `23505`）にも写像しないことは対象ビヘイビア ERR-3（TASK-154）が
    /// 確定契約とし、`tests/error_format_err3.rs` で検証する。
    /// [`crate::tenant::TenantWriteError::OperationIdContentMismatch`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::OperationIdContentMismatch`] の写像。
    OperationIdContentMismatch => ("22023", "OPERATION_ID_CONTENT_MISMATCH"),
    /// `DATE`／`TIMESTAMP` リテラル・格納済み値の範囲外・暦上不正
    /// （`22008`）。TABLE-13・TASK-197（Issue #884）が ERR-6 の管轄表に基づき
    /// 追加。文法違反は `InvalidInput`（`22000`）のまま。
    /// [`crate::sql::allowlist::SqlSurfaceError::DatetimeFieldOverflow`] の写像。
    DatetimeFieldOverflow => ("22008", "DATETIME_FIELD_OVERFLOW"),
    /// 構文上受理された値が、宣言済み型の表現として不正（`22P02`）。ENUM 列
    /// （TABLE-14・TASK-198）の語彙外ラベルが最初の送出経路（[`crate::catalog::
    /// EnumLabelError`]・`sql::allowlist::SqlSurfaceError::InvalidTextRepresentation`）。
    /// UUID 列（TABLE-13〔検討中〕・TASK-197、Issue #887）の厳密文法違反
    /// （[`crate::uuid::UuidTextError`]）も同じ分類・同じ経路を共有する。
    /// `InvalidInput`（`22000`）が「値の種類・形式自体が列型と噛み合わない」を表すのに
    /// 対し、本分類は「値は文字列として妥当だが、宣言済み型が定める表現の集合に
    /// 属さない」ことを表す（PostgreSQL の `invalid_text_representation` と同じ区別）。
    InvalidTextRepresentation => ("22P02", "INVALID_TEXT_REPRESENTATION"),
    /// 明示トランザクション（SQL-31・TASK-221）内で発生した一般的な状態不整合
    /// （`25000`）。同一トランザクション内での `operation_id` 再利用など、他の
    /// より具体的な分類に属さないトランザクション状態エラーに使う。
    /// [`crate::sql::allowlist::SqlSurfaceError::InvalidTransactionState`] の写像。
    InvalidTransactionState => ("25000", "INVALID_TRANSACTION_STATE"),
    /// `Active` なトランザクション中に再度 `BEGIN` を送った（`25001`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::ActiveSqlTransaction`] の写像。
    ActiveSqlTransaction => ("25001", "ACTIVE_SQL_TRANSACTION"),
    /// `Idle`（トランザクション外）で `COMMIT`／`ROLLBACK` を送った（`25P01`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::NoActiveSqlTransaction`] の写像。
    NoActiveSqlTransaction => ("25P01", "NO_ACTIVE_SQL_TRANSACTION"),
    /// `Failed` なトランザクション中に `ROLLBACK` 以外の文を送った（`25P02`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::InFailedSqlTransaction`] の写像。
    InFailedSqlTransaction => ("25P02", "IN_FAILED_SQL_TRANSACTION"),
    /// 明示トランザクション（SQL-31・TASK-221）の単一ライタ占有により、書き込み
    /// トランザクションの取得がロック待ちの上限を超過した（`55P03`）。
    /// [`crate::storage::StorageError::WriteLockTimeout`]・
    /// [`crate::catalog::CatalogError::WriteLockTimeout`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::LockNotAvailable`] の写像。
    LockNotAvailable => ("55P03", "LOCK_NOT_AVAILABLE"),
    /// `CREATE TABLE` が指定したテーブル名が既に存在する（`42P07`）。TABLE-4・
    /// TASK-85（Issue #899）が追加。上書きしない設計（既存スキーマは変更されない）。
    /// `CREATE VIEW`（TABLE-18・SQL-23・TASK-205、Issue #909）が既存のテーブル
    /// 名・ビュー名と衝突した場合も同じ分類を共有する（ビューはテーブルと
    /// 名前空間を共有する）。[`crate::sql::allowlist::SqlSurfaceError::
    /// DuplicateTable`] の写像。
    DuplicateTable => ("42P07", "DUPLICATE_TABLE"),
    /// `CREATE TABLE` の列リストに同名の列が複数回宣言された（`42701`）。
    /// TABLE-6・TASK-85（Issue #899）が追加。
    /// [`crate::sql::allowlist::SqlSurfaceError::DuplicateColumn`] の写像。
    DuplicateColumn => ("42701", "DUPLICATE_COLUMN"),
    /// `FETCH`／`CLOSE` が参照したカーソル名が、現在のトランザクション内に
    /// 存在しない（`34000`）。WIRE-15・TASK-218 が追加。
    /// [`crate::sql::allowlist::SqlSurfaceError::InvalidCursorName`] の写像。
    InvalidCursorName => ("34000", "INVALID_CURSOR_NAME"),
    /// `DROP TABLE`／`DROP VIEW` の対象に、それを参照するビューが 1 つ以上残って
    /// いるため削除を拒否した（`2BP01`。TABLE-18・SQL-23・TASK-205、Issue #909）。
    /// [`crate::catalog::CatalogError::DependentViewsExist`] の写像。依存する
    /// オブジェクト名の一覧はエラー文言に含めない（security.md P0）。
    DependentObjectsStillExist => ("2BP01", "DEPENDENT_OBJECTS_STILL_EXIST"),
    /// 指定した名前は存在するが、要求された操作が期待する種別のオブジェクトでは
    /// ない（`42809`。`DROP TABLE` にビュー名、`DROP VIEW` にテーブル名、または
    /// ビューへの書き込み系文〔`INSERT`／UPSERT／`UPDATE`／`DELETE`／
    /// `TRUNCATE`〕。TABLE-18・SQL-23・TASK-205、Issue #909）。
    /// [`crate::catalog::CatalogError::WrongObjectKind`] の写像。
    WrongObjectType => ("42809", "WRONG_OBJECT_TYPE"),
    /// 列に NOT NULL 制約が宣言されているにもかかわらず、値が省略またはNULL
    /// として書き込まれた（TABLE-16・TASK-204、Issue #904）。`wire_code`
    /// （`23502`）は [`ErrorClass::MissingOperationId`] と共有する
    /// （[`SHARED_WIRE_CODES`] 参照。ERR-6 が `wire_code` の共有と `code`
    /// ラベルによる区別を認める）。
    /// [`crate::sql::allowlist::SqlSurfaceError::NotNullViolation`] の写像。
    NotNullViolation => ("23502", "NOT_NULL_VIOLATION"),
    /// `DROP INDEX` で指定した索引が存在しない（`42704`。TASK-206・INDEX-7、
    /// Issue #908）。[`crate::sql::allowlist::SqlSurfaceError::UndefinedObject`] の
    /// 写像。
    UndefinedObject => ("42704", "UNDEFINED_OBJECT"),
    /// 索引 DDL が参照した列が対象テーブルに存在しない（`42703`。TASK-206・
    /// INDEX-7、Issue #908）。[`crate::sql::allowlist::SqlSurfaceError::
    /// UndefinedColumn`] の写像。
    UndefinedColumn => ("42703", "UNDEFINED_COLUMN"),
    /// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）が宣言する述語を、書き込み
    /// しようとした行の値が満たさない（`23514`）。
    /// [`crate::tenant::TenantWriteError::CheckViolation`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::CheckViolation`] の写像。
    CheckViolation => ("23514", "CHECK_VIOLATION"),
    /// `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）の参照整合性違反
    /// （`23503`）: 参照元の書き込みで参照先の値の組が同一テナント内に存在しない、
    /// または参照先の削除・更新で参照元の行が残る。
    /// [`crate::tenant::TenantWriteError::ForeignKeyViolation`]・
    /// [`crate::sql::allowlist::SqlSurfaceError::ForeignKeyViolation`] の写像。
    ForeignKeyViolation => ("23503", "FOREIGN_KEY_VIOLATION"),
    /// `FOREIGN KEY` 宣言（TABLE-17・TASK-205、Issue #907）の参照先列が主キー・
    /// UNIQUE 制約（または `id` 疑似列）と一致しない、あるいは参照元列と型が
    /// 一致しない（`42830`）。
    /// [`crate::sql::allowlist::SqlSurfaceError::InvalidForeignKey`] の写像。
    InvalidForeignKey => ("42830", "INVALID_FOREIGN_KEY"),
    /// 複数テーブル参照スコープ（`sql::relation::BindingScope`、SQL-28・RLS-10、
    /// Issue #924）で、非修飾列参照が複数の参照テーブルへ一致し一意に解決
    /// できない（`42702`）。[`crate::sql::allowlist::SqlSurfaceError::
    /// AmbiguousColumn`] の写像。
    AmbiguousColumn => ("42702", "AMBIGUOUS_COLUMN"),
}

impl ErrorClass {
    /// `wire_code` からの逆引き。未知のコードは `None`（fail-closed。呼び出し元が
    /// 未知コードを既定分類へ丸めて誤った意味論を持たせることを防ぐ）。
    ///
    /// [`SHARED_WIRE_CODES`] に載る `wire_code`（現状 `23502` のみ）は複数分類が
    /// 共有するため、本関数は [`ErrorClass::ALL`] の宣言順で最初に一致した分類
    /// （`23502` の場合は [`ErrorClass::MissingOperationId`]）を返す。この関数は
    /// HTTP ステータス射影の往復確認（同じ `wire_code` は同じステータスへ写像
    /// される）にのみ使われ、応答本文の `code` ラベルは各エラー型の
    /// `error_class()` が直接返す分類から得るため、共有コードの逆引きが
    /// `code` ラベルの取り違えを起こすことはない。
    pub fn from_wire_code(code: &str) -> Option<ErrorClass> {
        ErrorClass::ALL.into_iter().find(|c| c.wire_code() == code)
    }

    /// この分類が engine・wire-server のいずれかから現に送出されているか
    /// （モジュール冒頭の「収録範囲は現に返している `wire_code` に限る」という
    /// 不変条件を、prose だけでなくコード側でも明示・網羅テスト可能にするための
    /// 判定。`AuthRequired` の送出経路が TASK-174／HTTP-8（Issue #753）で接続された
    /// ことにより、本メソッドが `false` を返す分類は現時点で存在しない。メソッド
    /// 自体は `wire-server::http::status` の既存参照・公開 API 後方互換のため残す。
    /// 将来 `false` を返すべき分類を追加する場合は、モジュール冒頭の不変条件
    /// コメントも合わせて更新すること（codex-review Low 指摘対応・PR #101）。
    pub const fn has_connected_send_path(self) -> bool {
        true
    }
}

/// エラーメッセージへ含める文言の長さ上限。untrusted 断片（テーブル名・SQL 片等）を
/// そのまま無加工で長大に埋め込まない（`.claude/rules/security.md`「情報漏えい」対応）。
/// engine 全体の切り詰め上限の単一真実源であり、`sql::allowlist` は構築時点
/// （コンストラクタ）の切り詰めにこの値を参照する（`WireError::new` 側は最終防波堤として
/// 同じ規約を適用する。2 箇所が独立の値を持って乖離することを構造的に防ぐ）。
pub(crate) const MAX_MESSAGE_LEN: usize = 200;

/// 切り詰めたことを示す省略記号。長さは [`MAX_MESSAGE_LEN`] の内数として確保する。
const ELLIPSIS: &str = "...";

/// 文字境界で安全に切り詰める（マルチバイト文字の途中で切らない）。添字直接アクセス
/// をせず `get()` で明示的に処理する（`.claude/rules/coding-rust.md`「untrusted 入力の
/// 扱い」）。省略記号を含めた返値全体が [`MAX_MESSAGE_LEN`] バイトを超えないよう、
/// prefix の上限から省略記号分を差し引く（上限が「メッセージ全体の長さ」を意味する
/// 契約を実装側でも守る。codex-review P1 指摘対応）。
fn truncate_message(s: &str) -> String {
    if s.len() <= MAX_MESSAGE_LEN {
        return s.to_string();
    }
    let mut end = MAX_MESSAGE_LEN.saturating_sub(ELLIPSIS.len());
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match s.get(..end) {
        Some(prefix) => format!("{prefix}{ELLIPSIS}"),
        None => ELLIPSIS.to_string(),
    }
}

/// engine の各エラー型が共通分類へ写像するための trait。`SqlSurfaceError`
/// （`sql::allowlist`）・`TenantWriteError`（`tenant`）が実装し、既存の
/// `wire_code()`／`client_message()` をこの trait 経由へ委譲する。
pub trait ClassifiedError {
    /// この値が属する [`ErrorClass`]。
    fn error_class(&self) -> ErrorClass;

    /// クライアント（wire 層 `ErrorResponse`）へそのまま返してよい文言。内部詳細・
    /// 他テナントのデータ・存在情報を含めない契約（`.claude/rules/security.md` P0）。
    fn client_message(&self) -> String;

    /// SQLSTATE 風 `wire_code`。既定実装は `error_class().wire_code()` に委譲する
    /// （実装型ごとに再定義しない。乖離を構造的に防ぐ）。
    fn wire_code(&self) -> &'static str {
        self.error_class().wire_code()
    }
}

/// wire 層（TASK-97・TASK-153）へ渡す最終形。`ClassifiedError` を実装する engine の
/// 各エラー型から `From`／`from_classified` で変換して得る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    class: ErrorClass,
    message: String,
}

impl WireError {
    /// 新規構築。`message` は [`MAX_MESSAGE_LEN`] で切り詰める（DoS・情報漏えい対応）。
    /// `class == InternalError` の詳細文言を運びたい場合はこの API を使わず、必ず
    /// [`WireError::internal`] を使うこと（内部ストレージ詳細等の漏えい経路を型で塞ぐ）。
    /// `InternalError` を渡された場合、渡された `message` は使わず
    /// [`WireError::internal`] へ差し替える（コメントの主張を実装でも強制し、
    /// 呼び出し側の実装漏れによる詳細漏えいを構造的に防ぐ）。
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        if matches!(class, ErrorClass::InternalError) {
            return WireError::internal();
        }
        WireError {
            class,
            message: truncate_message(&message.into()),
        }
    }

    /// 内部エラー用の固定文言。呼び出し元は詳細を渡せない（redb I/O エラー等の
    /// 内部ストレージ詳細をクライアントへ運ばないための構造的な防止策。
    /// `.claude/rules/security.md`「不安全な設計」対応）。
    pub fn internal() -> Self {
        WireError {
            class: ErrorClass::InternalError,
            message: "internal error".to_string(),
        }
    }

    /// この値が属する [`ErrorClass`]。
    pub fn class(&self) -> ErrorClass {
        self.class
    }

    /// SQLSTATE 風 `wire_code`。
    pub fn wire_code(&self) -> &'static str {
        self.class.wire_code()
    }

    /// クライアントへ返す文言。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WireError {}

impl<E: ClassifiedError> From<&E> for WireError {
    fn from(e: &E) -> Self {
        if matches!(e.error_class(), ErrorClass::InternalError) {
            return WireError::internal();
        }
        WireError::new(e.error_class(), e.client_message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // `ALL` が enum の全 variant を漏れなく列挙していることの最小限の確認
    // （網羅性の主な検証は `tests/error_format.rs` の結合テスト側で行う）。
    #[test]
    fn all_classes_are_distinct() {
        let set: HashSet<ErrorClass> = ErrorClass::ALL.into_iter().collect();
        assert_eq!(set.len(), ErrorClass::ALL.len());
    }

    #[test]
    fn wire_codes_are_pairwise_distinct_except_shared_wire_codes() {
        // `SHARED_WIRE_CODES`（ERR-6）に載らない `wire_code` は従来どおり全分類で
        // 一意でなければならない。偶発的な重複はこのテストが検出する。
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for class in ErrorClass::ALL {
            *counts.entry(class.wire_code()).or_insert(0) += 1;
        }
        for (code, count) in &counts {
            if SHARED_WIRE_CODES.contains(code) {
                assert!(
                    *count >= 2,
                    "SHARED_WIRE_CODES に載る {code:?} は 2 分類以上で共有される想定"
                );
            } else {
                assert_eq!(
                    *count, 1,
                    "{code:?} は SHARED_WIRE_CODES 外なので一意のはず"
                );
            }
        }
    }

    /// モジュール冒頭が宣言する「収録範囲は現に返している `wire_code` に限る」
    /// 不変条件について、`AuthRequired` の送出経路接続（TASK-174／HTTP-8・
    /// Issue #753）後は未接続分類の集合が空であることを機械的に固定する
    /// （codex-review Low 指摘対応・PR #101。将来、未接続分類が
    /// ドキュメント更新なしに紛れ込むことをこのテストが検出する）。
    #[test]
    fn no_unconnected_exceptions_remain() {
        let unconnected: Vec<ErrorClass> = ErrorClass::ALL
            .into_iter()
            .filter(|c| !c.has_connected_send_path())
            .collect();
        assert_eq!(unconnected, Vec::<ErrorClass>::new());
    }
}
