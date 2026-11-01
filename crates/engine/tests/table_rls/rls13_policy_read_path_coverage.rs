//! 利用者ポリシー（RLS-13・Issue #2276 の USING 適用）の読み取り経路の分類漏れを検出する構造テスト。
//! ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md` の「USING の適用（#2276）」節。
//!
//! `crates/engine/src` の非テストコードにある、行ストアを開く・テナント可視性を判定する呼び出し
//! （`.is_visible(`・`ImplicitRlsHook::new(`・`user_rows_table_name(`）をファイルごとの件数で数え、
//! 下の分類表と一致しなければ失敗する。新しい読み取り経路（#2279・#2280 以降）が足されたとき、
//! 「ポリシーを適用する」「適用できないので `0A000` で拒否する」「行を読んで露出しない（非読み取り）」の
//! いずれかを決めずに通ることを防ぐ。件数が変わった場合は、そのファイルの経路を分類し直して表を更新する。
//! `table_generation_bump_coverage.rs` と同じ「ソース走査」の手法で、行番号ではなく件数で照合する
//! （行の挿入で壊れにくくする）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// 読み取り結果を利用者へ返す経路で、利用者ポリシーを適用する（`sql::row_policy`）。
    Applied,
    /// ポリシーを反映できないため、適用表では固定文言の `0A000` で拒否する。
    Rejected,
    /// 利用者へ行を返さない（制約検査・索引保守・DDL の書き換え・採番・テスト補助等）。ポリシーを適用しない。
    NonRead,
}

/// `(ファイル, 件数, 分類, 理由)`。件数は 3 種の呼び出しの合計（コメント行・`#[cfg(test)]` 以降を除く）。
const TABLE: &[(&str, usize, Class, &str)] = &[
    (
        "arena.rs",
        1,
        Class::Rejected,
        "アリーナ構築。検索系の入口（ランキング付き SELECT・Rust API の検索・rls.rs）が適用表を拒否する",
    ),
    (
        "batch_fallback.rs",
        2,
        Class::NonRead,
        "呼び出し元が渡す配列への一括検索で、テーブルを読まない（表との接続時に #2279 が分類する）",
    ),
    (
        "batch_search.rs",
        4,
        Class::NonRead,
        "同上（常駐行列。engine 内部の取り込み用で、wire・SQL 表層から到達しない）",
    ),
    (
        "gpu_batch.rs",
        3,
        Class::NonRead,
        "同上（GPU 一括検索の作業量見積もり・検証。テーブルを読まない）",
    ),
    (
        "catalog.rs",
        19,
        Class::NonRead,
        "DDL・スキーマ書き換え・世代・件数・ページ走査の内部。SQL 表層は行を返さない",
    ),
    (
        "catalog/rowid_alloc.rs",
        1,
        Class::NonRead,
        "rowid 採番の内部",
    ),
    (
        "catalog/sequence/migrated_identity.rs",
        1,
        Class::NonRead,
        "採番移行の内部",
    ),
    (
        "column_rewrite.rs",
        2,
        Class::NonRead,
        "列の追加・削除・型変更に伴う行の書き換え（DDL）",
    ),
    (
        "constraint.rs",
        22,
        Class::NonRead,
        "一意性・参照整合性・CHECK の検査。テナント内の全行で判定する（ポリシーで隠れた行も含む。親 #2272 の規則）",
    ),
    (
        "gin_index.rs",
        2,
        Class::NonRead,
        "GIN 相当索引の保守（候補の削減は sql::gin_scan が行い、最終判定は scan のポリシー段）",
    ),
    (
        "key_index.rs",
        2,
        Class::NonRead,
        "永続キー索引の保守（制約検査用）",
    ),
    (
        "rls.rs",
        8,
        Class::Rejected,
        "PrefilterSnapshot::build・SearchTimeFilter::search が適用表を拒否する（マスク合成は #2279）",
    ),
    (
        "core.rs",
        5,
        Class::Applied,
        "get_row はポリシーを適用。辞書構築・VectorCore::search・生の全列置換/削除は適用表を拒否する",
    ),
    (
        "sql/aggregate.rs",
        5,
        Class::Applied,
        "集計の走査にポリシー段。適用表では可視 id ビットマップ・アリーナ・スカラー索引の高速経路を使わない",
    ),
    (
        "sql/group_by.rs",
        3,
        Class::Applied,
        "GROUP BY の走査にポリシー段。適用表では索引・アリーナ経路を使わない",
    ),
    (
        "sql/scan.rs",
        3,
        Class::Applied,
        "広域取得（順序なし・経路 A・B・PK 点取得・GIN 候補が共有する with_visible_row）にポリシー段",
    ),
    (
        "sql/window.rs",
        3,
        Class::Applied,
        "ウィンドウ関数の materialize 段にポリシー段",
    ),
    (
        "sql/relation_snapshot.rs",
        2,
        Class::Applied,
        "テーブル単位の可視スナップショットにポリシー段（世代キーのキャッシュはポリシー適用後を共有）",
    ),
    (
        "sql/dml_from_returning.rs",
        1,
        Class::NonRead,
        "UPDATE FROM・DELETE USING の RETURNING 整形。対象行は列挙段でポリシー通過済み",
    ),
    (
        "sql/exec.rs",
        4,
        Class::Applied,
        "ランキング付き検索（execute_statement_with_cache）は適用表を拒否。RETURNING 整形・INSERT は非読み取り",
    ),
    (
        "tenant.rs",
        20,
        Class::Applied,
        "述語形・id 形・結合形 UPDATE/DELETE の対象選択にポリシー。INSERT・UPSERT・TRUNCATE は #2277 または対象外",
    ),
    (
        "tenant/partitioned_dml.rs",
        2,
        Class::Applied,
        "分割実行 DML の走査にポリシー段",
    ),
    (
        "tenant/trigger_dml.rs",
        5,
        Class::Applied,
        "id 指定形の対象判定（id_is_target）にポリシー。トリガ経由の行操作は列挙済みの対象 id が入力",
    ),
    (
        "tenant/trigger_insert.rs",
        1,
        Class::NonRead,
        "トリガの INSERT 書き込み窓口（#2277 の WITH CHECK で扱う）",
    ),
];

const PATTERNS: [&str; 3] = [
    ".is_visible(",
    "ImplicitRlsHook::new(",
    "user_rows_table_name(",
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// 非テストコードの行だけを返す。`#[cfg(test)]` 以降（テストモジュール）・コメント行・`*_tests.rs`／`tests.rs`
/// は除く。
fn production_lines(path: &Path) -> Vec<String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "tests.rs"
        || name.ends_with("_tests.rs")
        || path.components().any(|c| c.as_os_str() == "test_util")
    {
        return Vec::new();
    }
    let text = std::fs::read_to_string(path).expect("read source");
    let all: Vec<&str> = text.lines().collect();
    let mut lines = Vec::new();
    for (i, line) in all.iter().enumerate() {
        // `#[cfg(test)]` の直後がインラインの `mod xxx {`（テストモジュール本体）ならここから先は
        // テストとして打ち切る。外部ファイル宣言（`mod xxx;`。ファイル名で除外する）や、それ以外の
        // `#[cfg(test)]` 項目（テスト専用の関数・use）は読み飛ばさず数える側に倒す。
        if line.trim_start().starts_with("#[cfg(test)]")
            && all.get(i + 1).is_some_and(|next| {
                let n = next.trim_start();
                n.starts_with("mod ") && n.trim_end().ends_with('{')
            })
        {
            break;
        }
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        lines.push((*line).to_string());
    }
    lines
}

fn scan() -> BTreeMap<String, usize> {
    let root = src_dir();
    let mut files = Vec::new();
    collect(&root, &mut files);
    let mut counts = BTreeMap::new();
    for f in files {
        let n: usize = production_lines(&f)
            .iter()
            .map(|l| PATTERNS.iter().map(|p| l.matches(p).count()).sum::<usize>())
            .sum();
        if n > 0 {
            let rel = f
                .strip_prefix(&root)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            counts.insert(rel, n);
        }
    }
    counts
}

#[test]
fn every_row_read_call_site_is_classified() {
    let counts = scan();
    let table: BTreeMap<&str, (usize, Class)> =
        TABLE.iter().map(|(f, n, c, _)| (*f, (*n, *c))).collect();
    let mut problems = Vec::new();
    for (file, n) in &counts {
        match table.get(file.as_str()) {
            None => problems.push(format!("{file}: {n} 件が未分類（表へ追加して分類する）")),
            Some((expected, _)) if expected != n => problems.push(format!(
                "{file}: 件数が {expected} から {n} に変わった（新しい経路を分類し直して表を更新する）"
            )),
            Some(_) => {}
        }
    }
    for file in table.keys() {
        if !counts.contains_key(*file) {
            problems.push(format!("{file}: 表にあるが呼び出しが無い（表から外す）"));
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

/// 適用する経路・拒否する経路の入口が、`sql::row_policy` を実際に呼んでいる（分類の宣言だけで終わらせない）。
#[test]
fn classified_entry_points_call_the_policy_module() {
    let root = src_dir();
    let applied: &[&str] = &[
        "sql/scan.rs",
        "sql/aggregate.rs",
        "sql/group_by.rs",
        "sql/window.rs",
        "sql/relation_snapshot.rs",
        "tenant.rs",
        "tenant/partitioned_dml.rs",
        "tenant/trigger_dml.rs",
    ];
    let rejecting: &[&str] = &["sql/exec.rs", "core.rs"];
    for rel in applied.iter().chain(rejecting) {
        let text = std::fs::read_to_string(root.join(rel)).expect("read source");
        assert!(
            text.contains("row_policy::")
                || text.contains("resolve_dml_row_policy")
                || text.contains("row_passes_policy"),
            "{rel}: ポリシー段・拒否の呼び出しが無い"
        );
    }
}
