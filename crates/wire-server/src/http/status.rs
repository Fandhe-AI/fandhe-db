//! `ErrorClass` → HTTP ステータスの決定的射影（Issue #744。対象ビヘイビア ERR-4。
//! ポインタ: `docs/spec/05-tasks.md` TASK-180・`docs/spec/04-behavior/error-format.md`
//! ERR-4）。
//!
//! NoSQL 表層は SQL 表層とエラー契約（`wire_code`）を完全共有し、新規 `wire_code` を
//! 追加しない方針のもと、分類済みの [`ErrorClass`] を受け取って対応する HTTP ステータス
//! を返すだけの純関数を提供する。分類そのもの（どの失敗をどの `ErrorClass` に分類するか）
//! の決定にはここでは関与しない——上位層（要求パーサ・認証・SQL 表層の委譲結果）が
//! 既に `ErrorClass` を確定させたあとの写像のみを担う。
//!
//! 呼び出し元は後続 Issue（#745 の JSON エラー本文エンコーダ・#746 の応答エンコーダの
//! ステータス行）を想定する。

use engine::error_format::ErrorClass;

/// `ErrorClass` から HTTP ステータスコードへの決定的射影。
///
/// 「1 つの `wire_code`（＝ `ErrorClass`）→ 常に 1 つのステータス」の方向にのみ 1:1 で、
/// 逆方向（ステータス → `wire_code`）は 1:1 ではない（例えば 400 は複数分類が共有する）。
///
/// 外部状態・時刻・乱数を参照しない `const fn` の網羅 `match`。[`ErrorClass`] へ
/// variant が追加されるとこの `match` はコンパイルエラーになる（`#[non_exhaustive]`
/// を付けない設計方針。`docs/design/error-enum-non-exhaustive-policy.md`）。
/// `_` アームを置かないことは `#[deny(clippy::wildcard_enum_match_arm)]` によって
/// `make lint`（`-D warnings`）でも機械的に強制する。
#[deny(clippy::wildcard_enum_match_arm)]
pub const fn http_status(class: ErrorClass) -> u16 {
    match class {
        ErrorClass::AuthRequired | ErrorClass::AuthInvalid => 401,
        ErrorClass::ForbiddenTenantMismatch => 403,
        // `InvalidCursorName`（`34000`。WIRE-15・TASK-218）は NoSQL 表層の `op`
        // 語彙にカーソル操作が無く構造的に到達しないが、`ErrorClass` の網羅性
        // のため「対象が存在しない」という同じ意味論を持つ `TableNotFound`／
        // `RowNotFound` と同じ 404 へ寄せる。
        ErrorClass::TableNotFound | ErrorClass::RowNotFound | ErrorClass::InvalidCursorName => {
            404
        }
        // `DuplicateTable`（`42P07`。SQL-23・TASK-85、Issue #899）は
        // `CREATE TABLE` が指定したテーブル名の既存衝突であり、`UniqueViolation`
        // と同じ「対象が既に存在する」意味論のため同じ 409 とする。
        // `CheckViolation`（`23514`。TABLE-16・TASK-204、Issue #906）は書き込む
        // 行の値が宣言済み制約と矛盾するという `UniqueViolation` と同じ
        // 「対象の状態と矛盾する」意味論のため、同じ 409 とする（ERR-6）。
        // `ForeignKeyViolation`（`23503`。TABLE-17・TASK-205、Issue #907）も同じ
        // 意味論（参照整合性と矛盾する書き込み）のため 409 とする（ERR-6）。
        ErrorClass::UniqueViolation
        | ErrorClass::DuplicateTable
        | ErrorClass::CheckViolation
        | ErrorClass::ForeignKeyViolation => 409,
        ErrorClass::PayloadTooLarge => 413,
        ErrorClass::InternalError => 500,
        ErrorClass::FeatureNotSupported => 501,
        ErrorClass::ConnectionLimitExceeded => 503,
        // 明示トランザクション（SQL-31・TASK-221）の単一ライタ占有によるロック
        // 待ちタイムアウト。NoSQL 表層からは `insert`／`update`／`delete` op が
        // 待たされて到達しうる（一時的なサーバー側の輻輳として 503 に射影する）。
        ErrorClass::LockNotAvailable => 503,
        ErrorClass::ProtocolViolation
        | ErrorClass::UnsupportedSqlSyntax
        | ErrorClass::InvalidInput
        | ErrorClass::NumericOutOfRange
        | ErrorClass::OperationIdContentMismatch
        | ErrorClass::MissingOperationId
        | ErrorClass::DatetimeFieldOverflow
        | ErrorClass::InvalidTextRepresentation
        // `DuplicateColumn`（`42701`。Issue #899）は `CREATE TABLE` の列リスト
        // 自体が不正という構文的な分類のため、他の 42xxx 系と同じ 400 とする。
        | ErrorClass::DuplicateColumn
        // TABLE-18・SQL-23・TASK-205（Issue #909）: `CREATE VIEW`／`DROP VIEW`
        // が新設する 2 分類（`DuplicateTable` は上で `UniqueViolation` と
        // 同じ 409 に既に分類済みのためここには含めない）。いずれも
        // クライアント入力（対象名・依存関係）に起因する拒否のため、既存の
        // SQL 構文エラー系と同じ 400 とする。
        | ErrorClass::DependentObjectsStillExist
        | ErrorClass::WrongObjectType
        // トランザクション状態エラー（SQL-31・TASK-221）は NoSQL 表層の `op` 語彙に
        // トランザクション制御が無く構造的に到達しないが、`ErrorClass` の網羅性の
        // ため他の `42601`／`22000` 系と同じ 400 へ寄せる。
        | ErrorClass::InvalidTransactionState
        | ErrorClass::ActiveSqlTransaction
        | ErrorClass::NoActiveSqlTransaction
        | ErrorClass::InFailedSqlTransaction
        // `NotNullViolation`（`23502`。TABLE-16・TASK-204、Issue #904）は
        // `wire_code` を共有する `MissingOperationId` と同じくクライアント側の
        // 入力不備であり 400 とする。
        | ErrorClass::NotNullViolation
        // TASK-206・INDEX-7・SQL-23（Issue #908）: 索引 DDL の対象不在（`42704`）・
        // 参照列不在（`42703`）。ERR-4 の射影規則（ERR-6 新設行は 400）に従う。
        // NoSQL 表層の `op` 語彙に索引 DDL が無く構造的に到達しないが、
        // `ErrorClass` の網羅性のため射影を定める。
        | ErrorClass::UndefinedObject
        | ErrorClass::UndefinedColumn
        // `InvalidForeignKey`（`42830`。TABLE-17・TASK-205、Issue #907）は
        // `FOREIGN KEY` 宣言（`CREATE TABLE`。NoSQL 表層の `op` 語彙に DDL が無く
        // 構造的に到達しない）の不正。ERR-6 新設行の射影規則に従い 400 とする。
        | ErrorClass::InvalidForeignKey
        // `AmbiguousColumn`（`42702`。SQL-28・RLS-10、Issue #924）は複数テーブル
        // 参照スコープでの非修飾列の曖昧解決。SQL 表層は本 Issue でまだ受理し
        // ないため到達しないが（許可リストは JOIN・複数 FROM を拒否）、
        // `ErrorClass` の網羅性のため ERR-6 新設行の射影規則（400）を定める。
        | ErrorClass::AmbiguousColumn => 400,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期待表を明示的に列挙し、`ErrorClass::ALL` との突き合わせで非 vacuous に検証する。
    /// `match` にアームを足したが期待表の更新を忘れた、という乖離を (a)(b) が検出する。
    const EXPECTED: [(ErrorClass, u16); 35] = [
        (ErrorClass::InvalidInput, 400),
        (ErrorClass::AuthInvalid, 401),
        (ErrorClass::AuthRequired, 401),
        (ErrorClass::ForbiddenTenantMismatch, 403),
        (ErrorClass::TableNotFound, 404),
        (ErrorClass::RowNotFound, 404),
        (ErrorClass::UniqueViolation, 409),
        (ErrorClass::MissingOperationId, 400),
        (ErrorClass::PayloadTooLarge, 413),
        (ErrorClass::ConnectionLimitExceeded, 503),
        (ErrorClass::FeatureNotSupported, 501),
        (ErrorClass::UnsupportedSqlSyntax, 400),
        (ErrorClass::ProtocolViolation, 400),
        (ErrorClass::InternalError, 500),
        (ErrorClass::NumericOutOfRange, 400),
        (ErrorClass::OperationIdContentMismatch, 400),
        (ErrorClass::DatetimeFieldOverflow, 400),
        (ErrorClass::InvalidTextRepresentation, 400),
        (ErrorClass::LockNotAvailable, 503),
        (ErrorClass::InvalidTransactionState, 400),
        (ErrorClass::ActiveSqlTransaction, 400),
        (ErrorClass::NoActiveSqlTransaction, 400),
        (ErrorClass::InFailedSqlTransaction, 400),
        (ErrorClass::DuplicateTable, 409),
        (ErrorClass::DuplicateColumn, 400),
        (ErrorClass::InvalidCursorName, 404),
        (ErrorClass::DependentObjectsStillExist, 400),
        (ErrorClass::WrongObjectType, 400),
        (ErrorClass::NotNullViolation, 400),
        (ErrorClass::UndefinedObject, 400),
        (ErrorClass::UndefinedColumn, 400),
        (ErrorClass::CheckViolation, 409),
        (ErrorClass::ForeignKeyViolation, 409),
        (ErrorClass::InvalidForeignKey, 400),
        (ErrorClass::AmbiguousColumn, 400),
    ];

    #[test]
    fn expected_table_length_matches_all_classes() {
        // (a) 表の長さが `ErrorClass::ALL` と一致すること。
        assert_eq!(EXPECTED.len(), ErrorClass::ALL.len());
    }

    #[test]
    fn expected_table_covers_every_class_exactly_once() {
        // (b) `ALL` の各 variant が期待表にちょうど 1 回現れること。
        for class in ErrorClass::ALL {
            let occurrences = EXPECTED.iter().filter(|(c, _)| *c == class).count();
            assert_eq!(
                occurrences, 1,
                "class {:?} は期待表にちょうど 1 回現れるべき",
                class
            );
        }
    }

    #[test]
    fn http_status_matches_expected_table() {
        // (c) 各行で実装の返値が期待値と一致すること。
        for (class, expected) in EXPECTED {
            assert_eq!(
                http_status(class),
                expected,
                "class {:?} の射影が期待値と不一致",
                class
            );
        }
    }

    #[test]
    fn projection_is_deterministic_across_wire_code_round_trip() {
        // 「1 つの wire_code → 常に 1 つのステータス」の固定。`wire_code()` で
        // 一度シリアライズしてから `from_wire_code` で復元しても射影が変わらないこと、
        // かつ同一入力の 2 回呼び出しが一致することを確認する。
        for class in ErrorClass::ALL {
            let round_tripped = ErrorClass::from_wire_code(class.wire_code())
                .expect("ALL の各 class の wire_code は必ず逆引きできる");
            assert_eq!(http_status(class), http_status(round_tripped));
            assert_eq!(http_status(class), http_status(class));
        }
    }

    #[test]
    fn all_projections_are_client_error_or_server_error_range() {
        // エラー分類が 2xx/3xx へ誤って射影されないことの範囲健全性チェック。
        for class in ErrorClass::ALL {
            let status = http_status(class);
            assert!(
                (400..=599).contains(&status),
                "class {:?} のステータス {} が 400..=599 の範囲外",
                class,
                status
            );
        }
    }

    #[test]
    fn auth_required_projects_to_401_with_connected_send_path() {
        // `AuthRequired` の送出経路は TASK-174／HTTP-8（Issue #753）で
        // `http::session::close`／`http::session::bearer` から接続済み
        // （`has_connected_send_path() == true`）。射影テーブル上は他の
        // 認証系分類と同じ 401 として確定している。
        assert!(ErrorClass::AuthRequired.has_connected_send_path());
        assert_eq!(http_status(ErrorClass::AuthRequired), 401);
    }

    /// `const fn` であることの固定。#745/#746 が定数文脈から利用する形態を先取りして
    /// コンパイル時に検証する。
    const _: u16 = http_status(ErrorClass::InternalError);
}
