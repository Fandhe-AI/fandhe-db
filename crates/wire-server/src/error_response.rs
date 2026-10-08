//! `engine::error_format::ErrorClass` → PostgreSQL wire `ErrorResponse`（'E'）バイト列の
//! 横断写像（TASK-153、対象ビヘイビア: ERR-1。ポインタ: `docs/spec/05-tasks.md` TASK-153・
//! `docs/spec/04-behavior/error-format.md`）。
//!
//! 責務境界: engine 側の `ErrorClass`（`wire_code`・SSOT は `engine::error_format`）を
//! 入力に取り、wire プロトコルの `ErrorResponse` フィールド構成（severity/SQLSTATE/
//! message、および `RECOVER-5` (3) 該当時限定の detail）へ整形するのが本モジュールの
//! 唯一の責務。ソケットへの書き込みは行わず、`Vec<u8>` を返す純関数のみで構成する
//! （`crate::result_encoder` と同じ方針。呼び出し元が I/O を担う）。
//!
//! [`encode`] は通常エラー応答（`S`/`C`/`M` の 3 フィールド）を組み立てる、
//! **本 crate 内で `ErrorResponse` を送出する全経路が共有する唯一の実体**である
//! （`handshake::write_error_response`・`simple_query::respond_error_and_ready`・
//! `simple_query` の緊急応答チャネル（`build_emergency_response_bytes`）はいずれも
//! `&str` の SQLSTATE を直接扱わず、engine 側の `SqlSurfaceError`／固定の
//! `ErrorClass` 定数を本関数へ渡す。以前は通常応答が `crate::result_encoder::
//! encode_error_response`〔`&str` 受け取り・severity `ERROR` 固定〕を独自に経由し、
//! `ErrorClass` による severity 一元化・NUL 拒否が実際の送出経路に反映されない
//! 横断写像の不整合があった。codex-review P1 指摘対応・PR #258）。
//! `S`（severity）は `ErrorClass` ごとに [`severity_for`] が決定する（既定は
//! `ERROR`、接続を閉じて終了する `ErrorClass::ConnectionLimitExceeded`〔`53300`〕
//! のみ `limits.rs::reject_too_many_connections` の独自実装と同じ `FATAL`）。
//! バイト列組み立ては [`crate::result_encoder::push_s_c_m_fields`] を直接使うことで
//! severity を明示的に渡す（codex-review Low 指摘対応・PR #101 の「フィールド
//! レイアウトの実体を共有する」方針は維持しつつ、severity の決定は呼び出し元＝
//! 本モジュールに閉じる）。
//!
//! `D`（detail）フィールド（`RECOVER-5` (3)・commit 後 panic 時の `state=
//! may_be_committed` 相当の情報）の wire 形式は ERR-5（2026-09-14 確定・
//! `fandhe-db-spec#15`。ポインタ: TASK-153）により確定し、[`encode_with_detail`]
//! で追加できるようになった。通常応答（[`encode`]）は従来どおり `S`/`C`/`M` の
//! 3 フィールドのみで `D` を付けない契約を維持する。`crate::simple_query::
//! build_emergency_response_bytes`（緊急応答チャネル）は本モジュールの
//! [`MAY_BE_COMMITTED_DETAIL`] 定数と [`encode_with_detail`] を使って配線済み
//! （TASK-97・RECOVER-6・ERR-5）。
//!
//! フレーム長は [`crate::result_encoder::frame_len`]（`checked` 方式）を再利用し、
//! `as i32` によるオーバーフローを起こさない（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」）。メッセージへの NUL バイト混入はフィールド区切り
//! （NUL 終端）を破壊しフレーム構造を壊すため、[`encode`] は NUL を含む `message`
//! を fail-closed に拒否する（本モジュールの `message` 引数は固定英語文言のみを
//! 渡す契約だが、防御的に検証する）。

use engine::error_format::ErrorClass;

use crate::result_encoder::{frame_len, push_d_field, push_s_c_m_fields, EncodeError};

/// `message` に NUL バイトが含まれないか検証する（fail-closed）。フィールドは
/// NUL 終端のため、混入するとフレーム構造そのものが壊れる（後続フィールドの
/// 消失・意図しない終端）。
fn reject_embedded_nul(message: &str) -> Result<(), EncodeError> {
    if message.as_bytes().contains(&0) {
        return Err(EncodeError);
    }
    Ok(())
}

/// `ErrorClass` ごとの `S`（severity）フィールド値を決定する。
///
/// `ErrorClass::ConnectionLimitExceeded`（`53300`）は接続を閉じて終了する契約
/// （`wire-server/src/limits.rs::reject_too_many_connections` の既存独自実装が
/// `FATAL` 固定で送出している契約と同一）のため `FATAL` を返し、他の全分類は
/// `ERROR` を返す。以前は本モジュールの [`encode`] が [`push_s_c_m_fields`] へ
/// 委譲する `crate::result_encoder::encode_error_response` 経由で全分類一律
/// `S`=`ERROR` に固定していたため、`limits.rs` の独自経路（`FATAL`）と本経路
/// （`ERROR`）とで同じ `ErrorClass::ConnectionLimitExceeded` から異なる
/// `ErrorResponse` が生成される横断写像の不整合があった（codex-review P1
/// 指摘対応・PR #258）。
const fn severity_for(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::ConnectionLimitExceeded => "FATAL",
        _ => "ERROR",
    }
}

/// `body`（フィールド終端込みで組み立て済み）を `ErrorResponse`（'E'）フレームへ
/// 包む。フレーム長は [`frame_len`] の `checked` 方式に従う。
fn wrap_frame(body: Vec<u8>) -> Result<Vec<u8>, EncodeError> {
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// 通常エラー応答。`S`=[`severity_for`]（分類ごとに決定。既定は `ERROR`、
/// `ConnectionLimitExceeded` のみ `FATAL`）・`C`=`class.wire_code()`・
/// `M`=`message` の 3 フィールドのみを含む（他テナント・存在情報は含めない契約。
/// `.claude/rules/security.md` P0）。フィールド書き込みの実体は
/// [`push_s_c_m_fields`] を共有する（`limits.rs` の独自実装〔`FATAL` 固定〕とは
/// 別経路だが、`severity_for` により同じ `ErrorClass` から同じ severity を返す
/// 契約を維持する。codex-review P1 指摘対応・PR #258）。
pub fn encode(class: ErrorClass, message: &str) -> Result<Vec<u8>, EncodeError> {
    reject_embedded_nul(message)?;
    let mut body = Vec::new();
    push_s_c_m_fields(&mut body, severity_for(class), class.wire_code(), message);
    body.push(0); // フィールド終端
    wrap_frame(body)
}

/// 緊急応答（`RECOVER-5` (3)。commit 後 panic 時に「commit は成功している
/// かもしれない」ことを伝える固定文字列）の `D`（detail）値（ERR-5・TASK-153
/// ポインタ）。値の内容は他テナントのデータ・存在情報を一切含まない固定文言
/// であり、`crate::simple_query::build_emergency_response_bytes` が唯一の
/// 呼び出し元として [`encode_with_detail`] へ渡す（定数を 1 箇所に集約し、
/// 呼び出し元・テストでの生リテラル重複を避ける）。
pub const MAY_BE_COMMITTED_DETAIL: &str = "state=may_be_committed";

/// `D`（detail）フィールド付きエラー応答（ERR-5・TASK-153 ポインタ）。
/// `S`/`C`/`M` は [`encode`] と同一契約（[`severity_for`]・`class.wire_code()`）
/// に `D`=`detail` を追記する。想定呼び出し元は commit 後 panic の緊急応答
/// 経路（`RECOVER-5` (3)。`state=may_be_committed` 固定文字列の搬送）だが、
/// 本関数自体は任意の `detail: &str` を受け取る汎用 API であるため、呼び出し元は
/// 内部エラー詳細・他テナントのデータや存在情報を `detail` へ流し込まないこと
/// （`.claude/rules/security.md` P0。`encode` の message と同じ注意）。
///
/// `message`・`detail` いずれも NUL バイト混入は fail-closed に拒否する
/// （フィールド区切りの NUL 終端を破壊するため）。改行は許容する——PostgreSQL の
/// `DETAIL` は複数行を許容するのが通常の意味論であり、本 wire 実装のフィールド
/// 終端は NUL のみに依存するため改行があってもフレーム構造は壊れない
/// （`message` と同じ制約に揃える）。
pub fn encode_with_detail(
    class: ErrorClass,
    message: &str,
    detail: &str,
) -> Result<Vec<u8>, EncodeError> {
    reject_embedded_nul(message)?;
    reject_embedded_nul(detail)?;
    let mut body = Vec::new();
    push_s_c_m_fields(&mut body, severity_for(class), class.wire_code(), message);
    push_d_field(&mut body, detail);
    body.push(0); // フィールド終端
    wrap_frame(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// body 中の 1 フィールド（タグ 1 バイト＋NUL 終端文字列）を機械的に抽出する
    /// テスト専用ヘルパー。`.claude/rules/coding-rust.md` の添字アクセス禁止は
    /// untrusted 受信入力経路が対象のため、テストコードでは可読性を優先し
    /// `iter`/`position` ベースで実装する（`unwrap`/`expect` は許容）。
    fn find_field(body: &[u8], tag: u8) -> Option<String> {
        let mut idx = 0;
        while idx < body.len() {
            let this_tag = *body.get(idx)?;
            if this_tag == 0 {
                return None; // フィールド終端に到達
            }
            let value_start = idx + 1;
            let nul_offset = body.get(value_start..)?.iter().position(|&b| b == 0)?;
            let value_end = value_start + nul_offset;
            if this_tag == tag {
                let bytes = body.get(value_start..value_end)?;
                return std::str::from_utf8(bytes).ok().map(str::to_string);
            }
            idx = value_end + 1;
        }
        None
    }

    fn body_of(msg: &[u8]) -> &[u8] {
        // 'E'（1 バイト）+ length（4 バイト）の直後が body。
        msg.get(5..).expect("message too short")
    }

    /// ERR-1: `ErrorClass::ALL` 全件を [`encode`] し、`E` タグ・フレーム長・
    /// `S`=[`severity_for`]（`ConnectionLimitExceeded` のみ `FATAL`、他は
    /// `ERROR`）・`C`=各 `wire_code`・`M` 非空・終端 0 を機械的に検証する
    /// （分類追加時も `ErrorClass::ALL` 経由で自動的に網羅が追随する。
    /// codex-review P1 指摘対応・PR #258）。
    #[test]
    fn encode_covers_all_error_classes_with_s_c_m_fields() {
        for class in ErrorClass::ALL {
            let msg = encode(class, "test message").expect("encode");
            assert_eq!(msg.first().copied(), Some(b'E'));

            let declared_len = i32::from_be_bytes(
                msg.get(1..5)
                    .expect("length field")
                    .try_into()
                    .expect("4 bytes"),
            ) as usize;
            assert_eq!(
                declared_len,
                msg.len() - 1,
                "length field excludes only the leading 'E' type byte (class={class:?})"
            );

            let body = body_of(&msg);
            assert_eq!(
                find_field(body, b'S').as_deref(),
                Some(severity_for(class)),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'C').as_deref(),
                Some(class.wire_code()),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'M').as_deref(),
                Some("test message"),
                "class={class:?}"
            );
            // 通常応答は D フィールドを含まない。
            // フィールド種別（タグ）として `D` が無いことを確認する（SQLSTATE `VD001` のように
            // 値の中に文字 `D` を含む分類があるため、バイト列の単純な検索はしない）。
            assert!(find_field(body, b'D').is_none(), "class={class:?}");
            assert_eq!(body.last().copied(), Some(0), "field terminator");
        }
    }

    #[test]
    fn encode_rejects_message_with_embedded_nul() {
        let result = encode(ErrorClass::InternalError, "bad\0message");
        assert!(result.is_err(), "embedded NUL must be rejected fail-closed");
    }

    /// 通常応答（[`encode`]）は ERR-5 確定後も `D` フィールドを追加しない契約を
    /// 維持する（`D` を追加したい呼び出し元は [`encode_with_detail`] を使う）。
    #[test]
    fn encode_never_includes_a_detail_field() {
        for class in ErrorClass::ALL {
            let msg = encode(class, "internal error").expect("encode");
            assert!(find_field(body_of(&msg), b'D').is_none(), "class={class:?}");
        }
    }

    /// codex-review P1 指摘（PR #258）の再発防止: `ErrorClass::
    /// ConnectionLimitExceeded`（`53300`）は接続を閉じる契約のため、
    /// `wire-server/src/limits.rs::reject_too_many_connections` の独自実装
    /// （`FATAL` 固定）と同じく本経路（[`encode`]）でも `S`=`FATAL` を返す
    /// （`ERROR` に丸められない）。
    #[test]
    fn encode_connection_limit_exceeded_uses_fatal_severity() {
        let msg =
            encode(ErrorClass::ConnectionLimitExceeded, "too many connections").expect("encode");
        let body = body_of(&msg);
        assert_eq!(find_field(body, b'S').as_deref(), Some("FATAL"));
    }

    /// [`severity_for`] は `ConnectionLimitExceeded` 以外の全分類で `ERROR` を
    /// 返す（`FATAL` へ丸められる分類が意図せず増えないことの網羅検証）。
    #[test]
    fn severity_for_is_error_for_all_classes_except_connection_limit_exceeded() {
        for class in ErrorClass::ALL {
            let expected = if class == ErrorClass::ConnectionLimitExceeded {
                "FATAL"
            } else {
                "ERROR"
            };
            assert_eq!(severity_for(class), expected, "class={class:?}");
        }
    }

    /// ERR-5: `encode_with_detail` が `ErrorClass::ALL` 全件で `S`/`C`/`M`/`D`
    /// の 4 フィールドをちょうど 1 個ずつ含み、`D` の値が渡した `detail` と一致
    /// し、`D` なしの [`encode`] とはバイト列が異なる（先頭の `S`/`C`/`M` 部分は
    /// 共通）ことを検証する。
    #[test]
    fn encode_with_detail_includes_single_d_field_and_preserves_normal_encode() {
        for class in ErrorClass::ALL {
            let with_detail =
                encode_with_detail(class, "msg", MAY_BE_COMMITTED_DETAIL).expect("encode");
            let without_detail = encode(class, "msg").expect("encode");

            let body = body_of(&with_detail);
            assert_eq!(
                find_field(body, b'S').as_deref(),
                Some(severity_for(class)),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'C').as_deref(),
                Some(class.wire_code()),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'M').as_deref(),
                Some("msg"),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'D').as_deref(),
                Some(MAY_BE_COMMITTED_DETAIL),
                "class={class:?}"
            );
            assert_eq!(body.last().copied(), Some(0), "field terminator");

            assert_ne!(
                with_detail, without_detail,
                "class={class:?}: detail 付きと通常応答はバイト列が異なるはず"
            );
        }
    }

    /// `message`・`detail` いずれに NUL が混入していても fail-closed に拒否する
    /// （フィールド区切りの NUL 終端破壊を防ぐ）。
    #[test]
    fn encode_with_detail_rejects_message_or_detail_with_embedded_nul() {
        let bad_message = encode_with_detail(ErrorClass::InternalError, "bad\0message", "detail");
        assert!(
            bad_message.is_err(),
            "embedded NUL in message must be rejected fail-closed"
        );

        let bad_detail = encode_with_detail(ErrorClass::InternalError, "message", "bad\0detail");
        assert!(
            bad_detail.is_err(),
            "embedded NUL in detail must be rejected fail-closed"
        );
    }

    /// PostgreSQL の `DETAIL` は複数行を許容するのが通常の意味論であり、本 wire
    /// 実装のフィールド終端は NUL のみに依存するため、改行を含む `detail` でも
    /// 1 フィールドとして正常にエンコードされ値がそのまま（改行含む）復元できる。
    #[test]
    fn encode_with_detail_allows_newline_in_detail() {
        let detail_with_newline = "state=may_be_committed\nline2";
        let msg = encode_with_detail(ErrorClass::InternalError, "message", detail_with_newline)
            .expect("encode");
        let body = body_of(&msg);
        assert_eq!(find_field(body, b'D').as_deref(), Some(detail_with_newline));
    }

    /// 受け入れ条件「通常応答のバイト列が変更前と同一」を機械的に固定する。
    /// golden bytes は `D` フィールド追加前の実装から `encode(ErrorClass::
    /// InternalError, "internal error")` を実際に呼び出して採取した固定値。
    #[test]
    fn encode_normal_response_byte_sequence_is_unchanged_by_detail_addition() {
        let msg = encode(ErrorClass::InternalError, "internal error").expect("encode");
        let golden: Vec<u8> = vec![
            69, 0, 0, 0, 35, 83, 69, 82, 82, 79, 82, 0, 67, 88, 88, 48, 48, 48, 0, 77, 105, 110,
            116, 101, 114, 110, 97, 108, 32, 101, 114, 114, 111, 114, 0, 0,
        ];
        assert_eq!(msg, golden);
    }
}
