//! TASK-132（対象ビヘイビア: CORE-10）の完了条件を機械検証する回帰テスト。
//!
//! `docs/ann-future-work.md`（ANN 併用時の RLS 再評価タスクを記録した設計メモ）が
//! 存在し、再評価タスクとして明記すべき必須事項（TASK-132・CORE-10 のポインタと、
//! 「再評価」「将来検討」「ANN」のキーワード）を含んでいることを確認する。
//! `include_str!` でコンパイル時に取り込むため、メモが欠落・改名された場合は
//! ビルド自体が失敗する（fail-closed にドキュメント欠落を検出する）。

const ANN_FUTURE_WORK_DOC: &str = include_str!("../../docs/ann-future-work.md");

#[test]
fn ann_future_work_doc_references_task_132_and_core_10() {
    assert!(
        ANN_FUTURE_WORK_DOC.contains("TASK-132"),
        "設計メモに TASK-132 のポインタ表記が含まれていない"
    );
    assert!(
        ANN_FUTURE_WORK_DOC.contains("CORE-10"),
        "設計メモに対象ビヘイビア CORE-10 のポインタ表記が含まれていない"
    );
}

#[test]
fn ann_future_work_doc_records_reevaluation_task() {
    assert!(
        ANN_FUTURE_WORK_DOC.contains("再評価"),
        "設計メモに再評価タスクである旨の記載が含まれていない"
    );
    assert!(
        ANN_FUTURE_WORK_DOC.contains("将来検討"),
        "設計メモに将来検討課題である旨の記載が含まれていない"
    );
    assert!(
        ANN_FUTURE_WORK_DOC.contains("ANN"),
        "設計メモに ANN への言及が含まれていない"
    );
}
