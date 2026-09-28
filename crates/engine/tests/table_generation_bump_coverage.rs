//! `crate::catalog::table_generation_in_txn`（テーブル単位の世代照合。`USING PLAN`
//! の I/O 前後照合が対象テーブルのみを見るための土台。TASK-77・SQL-5・codex-review
//! P1 再指摘・PR #266）の「呼び忘れ検出」構造テスト。
//!
//! `bump_table_generation_in_txn` のドキュメントコメント（`catalog.rs`）が挙げる
//! 契約は「対象テーブルの `CATALOG_TABLE`／`user_rows/{table_name}` を変更する
//! すべての write commit の直前で呼ぶ」だが、この契約はコンパイラでは強制されない
//! （呼び忘れても型エラーにならない）。呼び忘れは `USING PLAN` の世代照合が対象
//! テーブルの実変更を見逃す fail-open（テナント境界の失効検出漏れと同種の重大度。
//! `.claude/rules/security.md` P0「テナント分離の検査を外す/緩める/バイパス経路を
//! 作らない」に準ずる）に直結するため、`tests/isa.rs`
//! `unsafe_is_confined_to_isa_module_with_safety_comments` と同じ「ソーステキスト
//! 走査」の手法で、`crate::recovery::commit_boundary` の commit 系公開関数
//! （`commit`/`commit_and_finish`/`commit_write_txn_guarded`）の呼び出し箇所を
//! 悉皆列挙し、各呼び出し箇所が (a) 直前の近傍行に `bump_table_generation_in_txn`
//! 呼び出しを持つか、(b) 明示的なアローリスト（`user_rows/{table}` を経由しない
//! 旧・非テーブルスコープ経路であることが確認済みの箇所）に含まれることを固定する。
//! 新たな書き込み経路（新しい commit 呼び出し）を追加した場合、本テストが
//! アローリストへの追記または `bump_table_generation_in_txn` 呼び出しの追加を
//! 強制する。
//!
//! 走査対象の呼び出し形は 2 通り: `commit_boundary::commit_write_txn_guarded(...)`
//! のようなモジュール修飾つき呼び出しと、`crates/engine/src/txn.rs` のように
//! `use crate::recovery::commit_boundary::commit_write_txn_guarded;` で import した
//! うえで裸名（`commit_write_txn_guarded(...)`）で呼ぶ呼び出しの両方を検出する
//! （codex-review 再指摘・PR #266。裸名 import 経路は旧実装では悉皆走査から漏れて
//! おり、将来 import スタイルで `user_rows/{table}` を書く経路が追加された場合に
//! バンプ漏れを検出できない fail-open だった）。commit 系公開関数名は
//! [`commit_boundary_call_names`] が `recovery/commit_boundary.rs` のソースから
//! 機械的に取得するため、関数追加時に本ファイルを個別に追随する必要はない。
//! `recovery/commit_boundary.rs` 自身の中で行われる裸名呼び出し（モジュール内部の
//! 委譲実装。例: `commit_write_txn_guarded` から `commit` への委譲）はモジュール
//! 修飾を要求しない自己参照であり、上記の「import 漏れ検出」の対象外のため
//! 裸名走査からは除外する（モジュール修飾つき呼び出しの走査は他ファイルと同様に
//! 及ぶ）。
//!
//! `assert_no_commit_boundary_module_alias_import` はモジュール自体の alias
//! import（`use ...commit_boundary as cb;`）のみを禁止しており、関数単位の alias
//! import（`use ...commit_boundary::commit as finish;` → `finish(write_txn)`）は
//! 対象外だった（codex-review P1 再指摘・PR #266）。`assert_no_commit_boundary_fn_alias_import`
//! がこの経路も禁止し、alias import 全般を fail-closed に倒す。

use std::path::{Path, PathBuf};

/// commit_boundary モジュール自身の定義ファイル（走査対象ではあるが、裸名呼び出し
/// 走査からは除外する。モジュール冒頭の doc コメント参照）。
const COMMIT_BOUNDARY_MODULE_FILE: &str = "recovery/commit_boundary.rs";

/// `commit_boundary::commit(...)` 系の呼び出しを直接行うが、意図的に
/// `bump_table_generation_in_txn` を伴わない箇所（呼び出し元ファイル名・行番号）。
///
/// - `storage.rs` の `Storage::put`/`Storage::put_batch`: `storage.rs::ROWS_TABLE`
///   （旧・非テーブルスコープの単一 redb テーブル）を書く経路で、`catalog.rs`
///   の `user_rows/{table_name}` とは別テーブル。SQL 表層（`USING PLAN` を含む
///   すべてのクエリ実行）は `user_rows/{table_name}` のみを読み、`ROWS_TABLE`
///   を経由しない（`crates/engine/src/sql/exec.rs`・`arena.rs`・`rls.rs` に
///   `ROWS_TABLE` への直接依存がないことをコメントで確認済み。PR #266 レビュー
///   対応）。
/// - `recovery/panic_hook.rs`: `#[cfg(test)]` 内のテスト fixture が
///   `storage.rs::ROWS_TABLE` へ直接書き込む箇所で、上記と同じ理由で対象外。
/// - `txn.rs` の `WriteTxn::commit`/`BatchWriteTxn::commit`
///   （`commit_write_txn_guarded` を裸名 import で呼ぶ。codex-review 再指摘・
///   PR #266 で新たに悉皆走査へ含まれるようになった呼び出し箇所）:
///   これらが書き込むのも `WriteTxn::put`/`BatchWriteTxn::put` 経由の
///   `storage.rs::ROWS_TABLE`（`txn.rs` 冒頭の import 一覧・`ROWS_TABLE` 使用箇所
///   参照）であり、上記の `storage.rs` エントリと同一テーブル・同一理由で
///   `user_rows/{table_name}` を経由しない旧・非テーブルスコープ経路のため対象外。
const ALLOWLIST: &[(&str, u32)] = &[
    // Issue #849（`Storage::begin_write_txn` choke point 追加）で `Storage::put`/
    // `Storage::put_batch` の行番号が移動したための追随（旧: 553／584）。
    // Issue #865（PR #989 マージ）で `MAX_METADATA_LEN` のドキュメンテーション
    // コメントが `pub(crate)` 化に伴い増量し、行番号がさらに移動したための追随
    // （旧: 648／679）。
    // Issue #942（SQL-31・TASK-221。`writer_gate` choke point 追加）で
    // `Storage::put`/`Storage::put_batch` の行番号がさらに移動したための追随
    // （旧: 652／683）。
    ("storage.rs", 804),
    ("storage.rs", 835),
    ("recovery/panic_hook.rs", 404),
    ("txn.rs", 191),
    ("txn.rs", 362),
    // `Storage::create_enum_type`（TABLE-14・TASK-198、Issue #890）: 新規
    // ENUM 型の登録は [`ENUM_TYPES_TABLE`]（`catalog.rs`）のみを書き、
    // `CATALOG_TABLE`／`user_rows/{table_name}` のいずれにも触れない。新規
    // 型は定義時点で参照列を 1 つも持ち得ない（型が存在しない列は宣言でき
    // ない）ため、影響を受けるテーブルが構造的に存在せずバンプ対象がない。
    // Issue #890 実装後の base（main）マージ取り込み（ARRAY 列型・Issue #888）で
    // `catalog.rs` に行が追加され、以下 2 件の行番号がさらに移動したための追随
    // （旧: 1403／1482）。PR #1015 レビュー対応（`RESERVED_TYPE_NAMES` へ
    // `enum`／`array` を追加）でさらに 5 行増え、再度追随（旧: 1506／1585）。
    // 追加され、再度追随（旧: 1511／1590）。Issue #884（DATE／TIMESTAMP 列型）の
    // base（main）取り込みマージで `catalog.rs` 冒頭側にさらに行が追加され、
    // 再度追随（旧: 1540／1619）。PR #1007（Issue #882・REAL/DOUBLE 列型）・
    // Issue #885（NUMERIC / DECIMAL 列型）・Issue #887（UUID 列型）の base
    // 取り込みマージ・Issue #881（INTEGER／BIGINT 列型）マージ取り込みで
    // `catalog.rs` に行が追加され、再度追随
    // （旧: 1567／1591／1639／1653／1677）。Issue #901（`ALTER TABLE ... DROP
    // COLUMN`／`ALTER COLUMN ... TYPE` の追加。`DroppedSlot`／`PhysicalSlot`・
    // `Storage::alter_table_drop_column`／`alter_table_widen_numeric_precision`）
    // で `catalog.rs` 冒頭側に行が追加され、再度追随（旧: 1701）。Issue #899
    // （CREATE TABLE 構文）・Issue #902（`DROP TABLE` の DDL 実行権限ゲート）の
    // base 取り込みマージで `catalog.rs` 冒頭側・`Storage::drop_table` の
    // ドキュメンテーションコメントが増え、以下 2 件の行番号がさらに移動した
    // ための追随（旧: 2173／2252）。PR #1045 レビュー対応（`TableSchema` の
    // 公開 API 互換性ドキュメンテーションコメント追加、Issue #901）で
    // `catalog.rs` 冒頭側にさらに 7 行追加され、再度追随（旧: 2177／2256）。
    // Issue #995（`VECTOR` 列を持たないテーブルへの INSERT 系書き込み受理）の
    // base 取り込みマージで `TableSchema::validate_embedding_dim`／
    // `validate_row_embedding_dim`・`Storage::insert_row_into_table`／
    // `insert_typed_row` のドキュメンテーションコメントが計 33 行増え、以下
    // 2 件の行番号がさらに移動したための追随（旧: 2184／2263）。PR #1044
    // レビュー対応（`DROP TABLE` 配線済み記述への訂正コメント）で `catalog.rs`
    // 冒頭側にさらに 2 行増え、再度追随（旧: 2217／2296）。
    // Issue #942（SQL-31・TASK-221。`convert_storage_error` ドキュメント拡充）の
    // 取り込みで行番号がさらに移動したための追随（旧: 2225）。TABLE-18・
    // SQL-23・TASK-205（Issue #909。`VIEWS_TABLE`・`Storage::create_view`／
    // `drop_view` 追加）と Issue #942 系変更の base（main）取り込みマージ統合・
    // PR #1048 レビュー対応（`Storage::create_view` の `body_sql`／
    // `base_relation` 自己検証追加、codex-review 指摘）で `catalog.rs` 冒頭側に
    // さらに行が追加され、再度追随（旧: 2543）。Issue #903（`PRIMARY KEY`
    // 宣言構文）・Issue #904（NOT NULL／DEFAULT 宣言構文。カタログ v4・v5 形式）
    // の base（main）取り込みマージ（PR #1048 手動統合）で `catalog.rs` 冒頭側に
    // さらに行が追加され、再度追随。Issue #900（`CatalogError::TooManyColumns`
    // 追加）の base 取り込みマージで以下 4 件がさらに 12 行移動し再度追随
    // （旧: 3147／3226／3303／3331）。
    // Issue #905（UNIQUE 制約。`UniqueConstraint`・カタログ v6・
    // `Storage::alter_table_add_unique_constraint` 等）の base 取り込みマージで
    // `catalog.rs` 冒頭側に行が追加され、再度追随（旧: 3159）。
    // TASK-206・INDEX-7（Issue #908。索引宣言の追加）の取り込みで再度追随
    // （旧: 3506）。
    // Issue #906（`CHECK` 制約。`CheckConstraint`・カタログ v7 等）の取り込みで
    // 再度追随（旧: 3854）。
    // Issue #907（`FOREIGN KEY` 制約。`ForeignKeyDef`・カタログ v8 等）の追加で
    // 再度追随（旧: 4138）。Issue #1067（`ALTER TABLE ADD／DROP CONSTRAINT
    // UNIQUE` と制約名。`UniqueConstraint` への名前追加・カタログ v9・
    // `Storage::alter_table_add_named_unique_constraint`／
    // `alter_table_drop_constraint` 新設等）で `catalog.rs` 冒頭側に行が
    // 追加され、再度追随（旧: 4676）。
    // Issue #1073（UNIQUE 制約の対象型拡張。`is_unique_constraint_allowed`・
    // `unique_key_tag`・型ごとの正準キー生成のドキュメンテーションコメント
    // 追加）で `catalog.rs` 冒頭側に行が追加され、再度追随。
    // Issue #1077（`ForeignKeyDef` への `MATCH`・遅延属性フィールド追加。カタログ
    // v10 の新設。本 PR〔#1067〕の base（main）取り込みマージで v9／v10 の版選択
    // ロジックを統合）で `catalog.rs` 冒頭側に行が追加され、再度追随。
    // Issue #1079（`validate_row_embedding_dim` 経路への doc コメント更新）で
    // `catalog.rs` 冒頭側の行数が変化し、再度追随。
    ("catalog.rs", 5394),
    // `Storage::drop_enum_type`（同上）: 削除前に依存列（当該型を参照する
    // `ColumnType::Enum` 列）が 1 つも無いことを `dependent_tables_in_txn`
    // で検証済みのため、こちらも `CATALOG_TABLE`／`user_rows/{table_name}`
    // のいずれにも触れない（`alter_enum_type_add_value` の commit 呼び出しは
    // 依存テーブルの世代を明示的に進行させるため ALLOWLIST 対象外のまま）。
    // Issue #901 の行追加で再度追随（旧: 1780）。Issue #902 の base 取り込み
    // マージでさらに追随（旧: 2252）。PR #1045 レビュー対応（同上）で再度
    // 追随（旧: 2256）。Issue #995 の base 取り込みマージでさらに追随
    // （旧: 2263）。PR #1044 レビュー対応（同上）でさらに追随（旧: 2296）。
    // TABLE-18・SQL-23・TASK-205（Issue #909）の行追加で さらに追随
    // （旧: 2304）。本 PR（#909）の base（main）取り込みマージ（Issue #901
    // 系変更との統合）で再度追随（旧: 2566）。PR #1048 レビュー
    // 対応（同上）で再度追随（旧: 2609）。Issue #942（SQL-31・TASK-221）系
    // 変更との base（main）取り込みマージ統合で再度追随（旧: 2622）。
    // Issue #903・#904 の base（main）取り込みマージで再度追随。
    // Issue #905（UNIQUE 制約。`UniqueConstraint`・カタログ v6・
    // `Storage::alter_table_add_unique_constraint` 等）の base 取り込みマージで
    // `catalog.rs` 冒頭側に行が追加され、再度追随（旧: 3238）。
    // TASK-206・INDEX-7（Issue #908。索引宣言の追加）の取り込みで再度追随
    // （旧: 3585）。
    // Issue #906（`CHECK` 制約。`CheckConstraint`・カタログ v7 等）の取り込みで
    // 再度追随（旧: 3933）。
    // Issue #907（`FOREIGN KEY` 制約。`ForeignKeyDef`・カタログ v8 等）の追加で
    // 再度追随（旧: 4217）。Issue #1067（同上）で再度追随。
    // Issue #1073（同上。UNIQUE 制約の対象型拡張）で再度追随。
    // Issue #1077（同上。`ForeignKeyDef` への `MATCH`・遅延属性フィールド追加。
    // カタログ v10 の新設・本 PR〔#1067〕の base 取り込みマージでの統合）で
    // 再度追随。
    // Issue #1079（同上。`catalog.rs` 冒頭側の行数が変化）再度追随。
    ("catalog.rs", 5473),
    // `Storage::create_view`（TABLE-18・SQL-23・TASK-205、Issue #909）: ビューは
    // `[VIEWS_TABLE]` のみを書き、`CATALOG_TABLE`／`user_rows/{table_name}` の
    // いずれにも触れない（行を持たない非マテリアライズド定義のため対象
    // テーブルが存在せずバンプ対象がない）。PR #1048 レビュー対応（同上）で
    // 再度追随（旧: 2627）。Issue #942 系変更との base（main）取り込みマージ
    // 統合で再度追随（旧: 2686）。Issue #903・#904 の base（main）取り込み
    // マージで再度追随（旧: 2699）。
    // Issue #905（UNIQUE 制約。`UniqueConstraint`・カタログ v6・
    // `Storage::alter_table_add_unique_constraint` 等）の base 取り込みマージで
    // `catalog.rs` 冒頭側に行が追加され、再度追随（旧: 3315）。
    // TASK-206・INDEX-7（Issue #908。索引宣言の追加）の取り込みで再度追随
    // （旧: 3662）。
    // Issue #906（`CHECK` 制約。`CheckConstraint`・カタログ v7 等）の取り込みで
    // 再度追随（旧: 4014）。
    // Issue #907（`FOREIGN KEY` 制約。`ForeignKeyDef`・カタログ v8 等）の追加で
    // 再度追随（旧: 4298）。Issue #1067（同上）で再度追随。
    // Issue #1073（同上。UNIQUE 制約の対象型拡張）で再度追随。
    // Issue #1077（同上。`ForeignKeyDef` への `MATCH`・遅延属性フィールド追加。
    // カタログ v10 の新設・本 PR〔#1067〕の base 取り込みマージでの統合）で
    // 再度追随。
    // Issue #1079（同上。`catalog.rs` 冒頭側の行数が変化）再度追随。
    ("catalog.rs", 5554),
    // `Storage::drop_view`（同上）: 削除前に依存するビューが 1 つも無いことを
    // `views_depending_on_in_txn` で検証済みのうえで `[VIEWS_TABLE]` のみを
    // 書く。同じ理由でバンプ対象がない。PR #1048 レビュー対応（同上）で
    // 再度追随（旧: 2655）。Issue #942 系変更との base（main）取り込みマージ
    // 統合で再度追随（旧: 2714）。Issue #903・#904 の base（main）取り込み
    // マージで再度追随（旧: 2727）。
    // Issue #905（UNIQUE 制約。`UniqueConstraint`・カタログ v6・
    // `Storage::alter_table_add_unique_constraint` 等）の base 取り込みマージで
    // `catalog.rs` 冒頭側に行が追加され、再度追随（旧: 3343）。
    // TASK-206・INDEX-7（Issue #908。索引宣言の追加）の取り込みで再度追随
    // （旧: 3690）。
    // Issue #906（`CHECK` 制約。`CheckConstraint`・カタログ v7 等）の取り込みで
    // 再度追随（旧: 4047）。
    // Issue #907（`FOREIGN KEY` 制約。`ForeignKeyDef`・カタログ v8 等）の追加で
    // 再度追随（旧: 4331）。Issue #1067（同上）で再度追随。
    // Issue #1073（同上。UNIQUE 制約の対象型拡張）で再度追随。
    // Issue #1077（同上。`ForeignKeyDef` への `MATCH`・遅延属性フィールド追加。
    // カタログ v10 の新設・本 PR〔#1067〕の base 取り込みマージでの統合）で
    // 再度追随。
    // Issue #1079（同上。`catalog.rs` 冒頭側の行数が変化）再度追随。
    ("catalog.rs", 5587),
    // `sql::transaction::SessionTransaction::commit`（SQL-31・TASK-221）:
    // ここで commit する共有 `write_txn` に対象テーブルの `user_rows/{table}`
    // 変更が含まれる場合、その変更を書いた文自身（`tenant::insert_typed_row_
    // unchecked`／`truncate_table_unchecked` の `WriteTarget::InTxn` 経路）が
    // 既に同一 `write_txn` 内で `bump_table_generation_in_txn` を呼び終えている
    // （`sql/exec.rs::execute_insert_with_schema_in`／`execute_truncate_in` →
    // `tenant.rs` 各関数のドキュメント参照）。本呼び出し箇所自体は複数文を
    // まとめて確定させるだけで、新たな `user_rows/{table}` 書き込みを行わない。
    // base（main）取り込みマージ（PR #1041 の `max_duration` 超過チェック追加）で
    // 行番号がさらに移動したための追随（旧: 248）。PR #1049 レビュー指摘対応
    // （`begin()` への世代カウンタ加算・`active_generation`／`cursor_id`
    // アクセサ追加）でさらに移動（旧: 256）。COMMIT 時の遅延 `FOREIGN KEY` 検査
    // （`constraint::enforce_deferred_foreign_keys_in_txn`。TABLE-17・TASK-205、
    // Issue #1077）を commit の直前に追加したことでさらに移動（旧: 293）。この
    // 検査自体は参照先の行ストアを読むだけで `user_rows/{table}` へ書き込まない。
    ("sql/transaction.rs", 330),
    // `tenant::WriteTarget::with_txn`（SQL-31・TASK-221。`insert_row_unchecked`・
    // `insert_rows_unchecked`・`insert_typed_row_unchecked`・
    // `truncate_table_unchecked` が autocommit／明示トランザクションの本体を
    // 共有するための choke point）: この汎用ヘルパー自身は `f`（呼び出し元が
    // 渡すクロージャ）を実行してから commit するだけで、`user_rows/{table}` へ
    // 直接触れない。近傍走査（テキスト上の近さ）では検出できないが、`f` を渡す
    // 4 呼び出し元はいずれも自分のクロージャの最後で
    // `bump_table_generation_in_txn` を呼んでから `Ok(())` を返すことを目視で
    // 確認済み（各関数のドキュメントコメント参照）。Issue #905（UNIQUE 制約。
    // `TenantWriteError::UniqueViolation` のドキュメント拡充）で 1 行移動した
    // ための追随（旧: 405）。Issue #906（`CHECK` 制約。`TenantWriteError::
    // CheckViolation` 等の追加）で行が移動したための追随（旧: 406）。
    // Issue #907（`FOREIGN KEY` 制約。`TenantWriteError::ForeignKeyViolation`
    // の追加）で行が移動したための追随（旧: 417）。
    // Issue #997（`MAX_SCANNED_ROWS` の `pub(crate)` 化に伴うドキュメント
    // コメント追記）で行が移動したための追随（旧: 425）。
    // Issue #1077（`WriteTarget` の `InTxn` 経路の doc コメント更新・
    // `fk_check_mode` アクセサ追加）で行が移動したための追随（旧: 428）。
    ("tenant.rs", 429),
];

/// `recovery/commit_boundary.rs` の `pub(crate) fn`/`pub fn` シグネチャを
/// `(関数名, 引数リストの生文字列)` として機械的に列挙する。引数リストは括弧の
/// 深さを数えて対応する `)` まで抜き出すため、`impl FnOnce(&T) -> ...` のような
/// 引数内の入れ子括弧があっても壊れない（[`commit_boundary_call_names`]・
/// [`assert_commit_names_cover_by_value_write_txn_params`] の共通基盤）。
fn commit_boundary_pub_fn_signatures(content: &str) -> Vec<(String, String)> {
    const MARKERS: [&str; 2] = ["pub(crate) fn ", "pub fn "];
    let mut sigs = Vec::new();
    let mut search_from = 0usize;
    while search_from < content.len() {
        let next = MARKERS
            .iter()
            .filter_map(|m| {
                content[search_from..]
                    .find(m)
                    .map(|rel| (search_from + rel, *m))
            })
            .min_by_key(|(start, _)| *start);
        let Some((start, marker)) = next else {
            break;
        };
        let after = &content[start + marker.len()..];
        let name_end = after
            .find(|c: char| c == '(' || c == '<' || c.is_whitespace())
            .unwrap_or(after.len());
        let name = after[..name_end].to_string();
        let Some(open_rel) = after[name_end..].find('(') else {
            search_from = start + marker.len();
            continue;
        };
        let params_start = name_end + open_rel + 1;
        let mut depth = 1i32;
        let mut close_idx = None;
        for (i, ch) in after[params_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close_idx = Some(params_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close_idx) = close_idx else {
            search_from = start + marker.len();
            continue;
        };
        sigs.push((name, after[params_start..close_idx].to_string()));
        search_from = start + marker.len() + close_idx;
    }
    sigs
}

/// `recovery/commit_boundary.rs` のソースから、commit 系の公開関数名
/// （`pub(crate) fn commit...`）を機械的に取得する。関数を追加・改名しても本テスト
/// ファイルを個別に追随させずに悉皆走査の対象へ含めるための抽出（PR #266
/// codex-review 再指摘対応）。
fn commit_boundary_call_names(src_dir: &Path) -> Vec<String> {
    let path = src_dir.join(COMMIT_BOUNDARY_MODULE_FILE);
    let content = std::fs::read_to_string(&path).expect("read commit_boundary.rs");
    commit_boundary_pub_fn_signatures(&content)
        .into_iter()
        .map(|(name, _params)| name)
        .filter(|name| name.starts_with("commit"))
        .collect()
}

/// [`commit_boundary_call_names`] の命名ヒューリスティック（`commit` 接頭辞）が
/// 取りこぼしていないかを、シグネチャの型情報（`redb::WriteTransaction` を値渡し
/// する引数を持つか）で交差検証する。`redb::WriteTransaction` を値で受け取る
/// 公開関数は「commit 呼び出しの起点になり得る関数」の必要条件であり、これが
/// `commit_names` に含まれていなければ命名規則側の抽出漏れ（例: 将来
/// `guarded_commit`/`finish_and_commit` のような `commit` 非接頭辞の名前へ改名
/// された場合）を検出できる（advisor 指摘対応）。
fn assert_commit_names_cover_by_value_write_txn_params(src_dir: &Path, commit_names: &[String]) {
    let path = src_dir.join(COMMIT_BOUNDARY_MODULE_FILE);
    let content = std::fs::read_to_string(&path).expect("read commit_boundary.rs");
    for (name, params) in commit_boundary_pub_fn_signatures(&content) {
        let takes_write_txn_by_value = params.contains("redb::WriteTransaction")
            && !params.contains("&redb::WriteTransaction");
        if takes_write_txn_by_value {
            assert!(
                commit_names.iter().any(|n| n == &name),
                "pub(crate)/pub fn {name} in {COMMIT_BOUNDARY_MODULE_FILE} takes \
                 redb::WriteTransaction by value but is not recognized as a commit-shaped \
                 function by the \"commit\" name-prefix heuristic; update \
                 commit_boundary_call_names or its callers so this function's call sites are \
                 covered by the scan",
                name = name
            );
        }
    }
}

/// 走査対象の全ファイル中に、`commit_boundary` モジュールを別名で import する
/// エイリアス（例: `use crate::recovery::commit_boundary as cb;`）が存在しないこと
/// を確認する。エイリアス経由の呼び出し（`cb::commit(...)`）は、本テストが検出する
/// 「`commit_boundary::` 修飾つき呼び出し」にも「裸名呼び出し」にも一致せず走査から
/// 漏れるため、エイリアス import そのものを禁止することで fail-closed に倒す
/// （advisor 指摘対応。エイリアスが必要になった場合は本テストの走査ロジックへ
/// 対応を追加したうえで許可すること）。
fn assert_no_commit_boundary_module_alias_import(rs_files: &[PathBuf], src_dir: &Path) {
    for path in rs_files {
        let rel_name = path
            .strip_prefix(src_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel_name == COMMIT_BOUNDARY_MODULE_FILE {
            continue;
        }
        let content = std::fs::read_to_string(path).expect("read source file");
        assert!(
            !content.contains("commit_boundary as "),
            "{rel_name} imports commit_boundary under an alias, which escapes this test's \
             \"commit_boundary::\"-prefixed and bare-name call site scan; extend the scan logic \
             before introducing an alias import"
        );
    }
}

/// `use` 文の生文字列を、識別子（英数字・`_`）の並びだけを抜き出したトークン列へ
/// 変換する。空白・改行・`::`・`{`/`}`/`,` はすべて区切りとして落ちるため、
/// 複数行 `use` や `{commit as x, commit_and_finish}` のようなネスト braces を含む
/// import リストでも、識別子の並びだけを見れば alias 記法（`<name> as <alias>`）を
/// 一様に検出できる（[`assert_no_commit_boundary_fn_alias_import`] の下請け）。
fn use_stmt_tokens(stmt: &str) -> Vec<&str> {
    stmt.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 走査対象の全ファイル中に、`commit_boundary` の commit 系公開関数を「関数 alias
/// import」（例: `use crate::recovery::commit_boundary::commit as finish;`）で
/// 取り込む箇所が存在しないことを確認する。`line_has_bare_call` は import 元の
/// 関数名（`name`）でしか呼び出し箇所を探索しないため、alias 経由の呼び出し
/// （`finish(write_txn)`）は裸名走査からもモジュール修飾走査からも漏れる
/// fail-open だった（codex-review P1 再指摘・PR #266）。ここでは `use` 文単位で
/// 生文字列を `;` まで切り出し（`use` 文の中に `;` を含む式は現れないため単純な
/// 文字列探索で安全に文の境界を取れる）、[`use_stmt_tokens`] でトークン化した上で
/// `<commit 系関数名>` の直後トークンが `as` であるものを検出する。
/// [`assert_no_commit_boundary_module_alias_import`]（モジュール自体の alias
/// import 禁止）と対になり、alias 経路を包括的に禁止することで fail-closed に
/// 倒す（alias が必要になった場合は本テストの走査ロジックへ対応を追加した上で
/// 許可すること）。
fn assert_no_commit_boundary_fn_alias_import(
    rs_files: &[PathBuf],
    src_dir: &Path,
    commit_names: &[String],
) {
    for path in rs_files {
        let rel_name = path
            .strip_prefix(src_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel_name == COMMIT_BOUNDARY_MODULE_FILE {
            continue;
        }
        let content = std::fs::read_to_string(path).expect("read source file");

        let mut search_from = 0usize;
        while let Some(rel) = content[search_from..].find("use ") {
            let start = search_from + rel;
            let after = &content[start..];
            let Some(semi_rel) = after.find(';') else {
                break;
            };
            let stmt = &after[..=semi_rel];
            search_from = start + semi_rel + 1;

            if !stmt.contains("commit_boundary") {
                continue;
            }
            let tokens = use_stmt_tokens(stmt);
            for window in tokens.windows(2) {
                let [tok, next] = window else { continue };
                if *next == "as" && commit_names.iter().any(|n| n == tok) {
                    panic!(
                        "{rel_name} imports commit_boundary::{tok} under an alias (`{tok} as \
                         ...`), which escapes this test's \"commit_boundary::\"-prefixed and \
                         bare-name call site scan; extend the scan logic before introducing an \
                         alias import"
                    );
                }
            }
        }
    }
}

/// `line` 中の `name(` が、`use` で import した裸名の関数呼び出しであるかを判定する
/// （モジュール修飾つき呼び出し `commit_boundary::name(` や、`.name(` のような
/// メソッド呼び出し、`fn name(` のような定義行は対象外）。
fn line_has_bare_call(line: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    // 定義行（`pub(crate) fn commit_write_txn_guarded(` 等）は呼び出しではない。
    if line.contains(&format!("fn {needle}")) {
        return false;
    }
    let bytes = line.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find(needle.as_str()) {
        let idx = search_from + rel;
        let preceding = if idx == 0 {
            None
        } else {
            bytes.get(idx - 1).copied()
        };
        let is_bare = match preceding {
            None => true,
            Some(b) => {
                let ch = b as char;
                // メソッド呼び出し（`.name(`）・モジュール修飾（`::name(`）・識別子の
                // 一部（例: `re_commit(` の `commit(` 誤検出防止）を除外する。
                !(ch == '.' || ch == ':' || ch.is_ascii_alphanumeric() || ch == '_')
            }
        };
        if is_bare {
            return true;
        }
        search_from = idx + needle.len();
    }
    false
}

#[test]
fn every_commit_boundary_commit_call_bumps_table_generation_or_is_allowlisted() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut rs_files = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);
    assert!(!rs_files.is_empty(), "no .rs files found under src/");

    let commit_names = commit_boundary_call_names(&src_dir);
    assert!(
        !commit_names.is_empty(),
        "no commit-shaped pub(crate) fn found in commit_boundary.rs; extraction logic is stale"
    );
    assert_commit_names_cover_by_value_write_txn_params(&src_dir, &commit_names);
    assert_no_commit_boundary_module_alias_import(&rs_files, &src_dir);
    assert_no_commit_boundary_fn_alias_import(&rs_files, &src_dir, &commit_names);
    let prefixed_needles: Vec<String> = commit_names
        .iter()
        .map(|n| format!("commit_boundary::{n}("))
        .collect();

    let mut checked_call_sites = 0usize;
    let mut allowlist_hits: std::collections::HashSet<(&str, u32)> =
        std::collections::HashSet::new();

    for path in &rs_files {
        let content = std::fs::read_to_string(path).expect("read source file");
        let lines: Vec<&str> = content.lines().collect();
        let rel_name = path
            .strip_prefix(&src_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_commit_boundary_module = rel_name == COMMIT_BOUNDARY_MODULE_FILE;

        for (idx, line) in lines.iter().enumerate() {
            let is_call_site = prefixed_needles.iter().any(|n| line.contains(n.as_str()))
                || (!is_commit_boundary_module
                    && commit_names
                        .iter()
                        .any(|n| line_has_bare_call(line, n.as_str())));
            if !is_call_site {
                continue;
            }
            checked_call_sites += 1;
            let line_no = (idx + 1) as u32;

            if let Some(entry) = ALLOWLIST
                .iter()
                .find(|(name, no)| *name == rel_name.as_str() && *no == line_no)
            {
                allowlist_hits.insert(*entry);
                continue;
            }

            // アローリスト外の呼び出しは、同一関数内で近傍（直前 60 行以内）に
            // `bump_table_generation_in_txn` を伴うことを要求する（関数の長さは
            // `tenant.rs::replace_typed_rows_by_text_key` が最長で、60 行あれば
            // 同一トランザクションのスコープ内に収まる。関数境界を跨いで誤検出
            // しないよう、直前に `pub`/`pub(crate) fn` の宣言行が現れたら
            // 探索を打ち切る）。
            let start = idx.saturating_sub(60);
            let mut has_bump = false;
            for l in lines[start..idx].iter().rev() {
                if l.contains("bump_table_generation_in_txn") {
                    has_bump = true;
                    break;
                }
                if (l.contains("fn ") && (l.contains("pub fn") || l.contains("pub(crate) fn")))
                    && !l.contains("bump_table_generation_in_txn")
                {
                    break;
                }
            }
            assert!(
                has_bump,
                "{}:{} calls commit_boundary::commit* without a preceding \
                 bump_table_generation_in_txn call and is not in the ALLOWLIST; either add the \
                 bump call before commit, or add (\"{}\", {}) to ALLOWLIST with a documented \
                 reason (this file's module doc comment)",
                rel_name, line_no, rel_name, line_no
            );
        }
    }

    assert!(
        checked_call_sites >= ALLOWLIST.len(),
        "expected to find at least as many commit_boundary::commit* call sites as ALLOWLIST \
         entries (found {checked_call_sites}); ALLOWLIST may be stale"
    );
    assert_eq!(
        allowlist_hits.len(),
        ALLOWLIST.len(),
        "some ALLOWLIST entries did not match any commit_boundary::commit* call site; \
         ALLOWLIST is stale (line numbers likely shifted) and must be updated"
    );
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
