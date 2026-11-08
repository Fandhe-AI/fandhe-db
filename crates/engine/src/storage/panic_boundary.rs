//! redb の入口呼び出しで起きる内部 panic を受け止める境界（RECOVER-13 のポインタ・Issue #3101）。
//!
//! redb 4.2 はファイルから読んだ値（アロケータ状態表など）に `assert!`・`unwrap()`・`unreachable!()`・
//! スライスの添字アクセスを使うため、壊れたファイルを開く・検査すると panic しうる。外から事前検証するには
//! redb のページ形式の再実装が要るので、open と検査の入口だけをここで包み、panic を固定のエラー
//! （fail-closed）へ倒す。呼び出し元は `Storage::open`・`ReadOnlyStorage::open`・両者の `verify_integrity`。
//!
//! 注意:
//! - unwind 前提。いずれかの profile を `panic = "abort"` にするとこの境界は効かない。
//! - engine 自身のコードは包まない（契約上 panic しない前提で、包むと本物のバグを隠すため）。
//! - 受け止めたあとの redb オブジェクトは再利用せず、呼び出し元が破棄する。
//! - panic のペイロードは redb の内部値を含みうるため捨てる（返すエラーは固定文言だけ）。

use std::panic::{catch_unwind, AssertUnwindSafe};

/// `f` を実行し、panic したら `None` を返す。ペイロードは捨てる。
pub(super) fn catch_backend_panic<T>(f: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_value_or_none_on_panic() {
        assert_eq!(catch_backend_panic(|| 7), Some(7));
        assert_eq!(
            catch_backend_panic(|| -> u8 { panic!("secret-payload") }),
            None
        );
    }
}
