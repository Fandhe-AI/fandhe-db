# 永続一意索引による UNIQUE・主キー検査の高速化

Issue #1070（`perf(engine)`）。対象ビヘイビア: TABLE-16（TASK-204）。関連ポインタ:
TABLE-12（物理キー `(tenant_id, id)`）・RLS-9・RLS-10 (c)（他テナントの存在情報の
非漏えい・可視性を問わない判定母集合）・ERR-1・ERR-2・ERR-4・RECOVER-12。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本
ドキュメントは本リポ側の実装判断・設計記録のみを扱う。関連: `docs/design/unique-constraint.md`
（Issue #905）・`docs/design/sql-primary-key.md`（Issue #903）。

## 背景・目的

主キー・UNIQUE 制約の検査点（`constraint::enforce_unique_keys_in_txn`）は、導入時点
（PR #1053・Issue #905/#903）から「書き込み対象行を除いたテナント全行を 1 文あたり
1 回線形走査する」実装で、検査コストがテナントの保有行数に比例していた
（`docs/design/unique-constraint.md`・`sql-primary-key.md` の「既知の制約」節）。
本 Issue はこれを永続一意索引（redb 二次テーブル）への点照会に置き換え、検査コストを
テナントの保有行数に比例させないことを目的とする。公開 API・エラー契約
（`wire_code`）は変更しない。

## 決定事項

| # | 決定 | 理由 |
| --- | ---- | ---- |
| D1 | 索引はテーブルごとに 1 本の redb テーブル `user_uniq/{table}`（キー `(tenant_id, subkey)`、値はバイト列）に持つ。`subkey` の先頭 1 バイトでマーカー（`0x00`）・正引き（`0x01`）・逆引き（`0x02`）の 3 名前空間に分ける | `drop_table`・TRUNCATE のテナント範囲掃除がテーブル単位・範囲単位の 1 操作で済む（行ストア `user_rows/{table}` と同じ命名規則・同じライフサイクル方針） |
| D2 | 正引きキーは `[0x01][ordinal: u16 BE][正準キーバイト列]`（`ordinal` は主キー→UNIQUE 制約の宣言順）、値は行 `id`（u64 BE）。逆引きキーは `[0x02][id]`、値はその行が現在所有する正引きキー列 | 正引きで点照会、逆引きで「行のキー値が変わった／行が削除された」ときの後片付け対象を O(1) で特定できる |
| D3 | マーカー（`[0x00]` → `[format_version][signature]`）が無い、またはシグネチャ（宣言済み一意キーの構成・列型タグの正準エンコード）が現在のスキーマと不一致なテナントは、次回の検査時に自テナントの索引を丸ごと無効化し、既存行（今回の書き込み対象を除く）だけを対象に 1 回だけバックフィルする | 既存 DB（索引テーブル未作成）・`ALTER TABLE ... ADD UNIQUE` によるキー構成変更のいずれも、明示的なマイグレーション手順を要さず自動的に追随できる |
| D4 | 判定は「正引きが指す行を読み戻して現在のキーを再計算し、一致すれば違反、不一致または行が存在しなければ stale として上書きする」遅延検証で行う。削除・更新経路の索引後片付け（`unique_index::forget_rows_in_txn`）は衛生措置に留め、呼び出し漏れがあっても偽陽性・偽陰性を起こさない設計にする | 削除・更新の全経路（`delete_row_impl`・`delete_rows_where_unchecked`・`replace_typed_rows_by_text_key`・TRUNCATE）を漏れなく数え上げるより、読み戻し 1 回（1 エントリあたり高々 1 行）で正しさを保証する方が構造的に堅牢 |
| D5 | 索引キーの第 1 要素は常にサーバー側導出テナント。照会・範囲走査・読み戻し・バックフィル・TRUNCATE の掃除はすべて自テナントの物理キー範囲に閉じる | RLS-9・RLS-10 (c)。他テナントの索引エントリ・行データに一切触れない |
| D6 | `drop_table` は行ストアと同一 write txn・同一 commit で索引テーブルも `delete_table` する。`alter_table_add_unique_constraint` は制約追加の検証後、同一 txn で索引テーブルを削除して無効化する（D3 のマーカー不一致検出と二重の安全策） | 同名テーブル再作成時に旧索引が残留し、新テーブルの行を誤って旧索引と突き合わせる事故を防ぐ（`user_rows_table_def` ドキュメントの既存方針と同じ判断） |
| D7 | `ColumnType::primary_key_tag()` の値は本 Issue により索引キーの一部として**永続化される**（従来は検査用スクラッチに留まり非永続だった） | 既存タグの意味を将来変更してはならない制約が生じる（変更する場合は `FORMAT_VERSION` を上げ全テナントの索引再構築を強制する必要がある）。新しい一意キー許可型を追加する際は未使用の値を新規に採番する |

## 正しさの不変条件

マーカーが立っているテナントでは、生存行 R が持つ NULL でない各一意キー K につ
いて、正引き `(tenant, K) → R.id` が必ず存在する（漏れは許さない）。一方、削除
済みの行やキーが変わった行を指す stale な正引きエントリが残ることは許す（D4）。
この非対称性により、後片付けの呼び出し漏れは索引の肥大化（衛生上の問題）には
なり得ても、判定の正しさ（偽陽性・偽陰性）には影響しない。

## 後方互換・既知の制約

- 索引テーブルが存在しない既存 DB では、各テナントの当該テーブルへの最初の
  書き込みでマーカー不在を検出し、自テナントの既存行だけを対象に 1 回だけ
  バックフィルする（その回のみ O(自テナント行数)。以降は索引照会のみで完結する）。
- 索引導入後の DB を索引未対応の旧バイナリで書き込む運用（ダウングレードして
  書き、再度アップグレードする）はサポートしない（索引と行データの不整合を
  検出する機構を持たない）。
- `#[cfg(test)]` 限定の生書き込み API は索引を更新しない（`constraint.rs` モジュール
  ドキュメントに記載済みの既存の既知のギャップと同じ）。
- 巨大な `TEXT`／`BYTEA` キーはそのまま索引キーになる（長さ上限・ハッシュ化は
  将来の検討事項）。
- FOREIGN KEY 参照先側の照会（`verify_required_parent_keys`）は本 Issue の対象外
  で、引き続き全走査のまま（同じ索引で O(log n) 化できる余地は残るが別課題とする）。

## 性能改善の証拠

受け入れ条件（検査コストがテナントの保有行数に比例しない）の主な証拠は、環境
非依存の構造テスト（`crates/engine/src/constraint/unique_index.rs`
`single_insert_read_cost_does_not_scale_with_existing_row_count`）に置く。このテスト
は 1 行 INSERT が行ストアを読み戻す回数（`ROW_TABLE_GET_COUNT`。単体テスト限定の
計測フック）を、既存行数が 100 件のテナントと 3,000 件のテナントで比較し、両者が
同じ小さな定数に収まる（既存行数に依存して増えない）ことを確認する。

共有 CI 環境でのマイクロベンチマーク（`docs/design/benchmark-judgement-policy.md`
準拠の before/after 交互実行）は追加しない判断とする——単なる先送りではなく、
同 doc §5「環境別の証拠力」の表が明示するとおり、共有 QEMU 本環境（本開発環境の
実測プロファイルも同 doc §6 参照）は「perf 動機の production 変更の採用
（Accepted）」を **不可** と判定しており、この環境で得たマイクロベンチマークの
数値は採否根拠にならず参考値にしかならない。一方、読み戻し回数という構造的指標は
環境ノイズの影響を受けず「検査コストがテナントの保有行数に比例しない」という
受け入れ条件を直接・決定的に示せるため、本 Issue の主要な証拠として同 doc の
方針どおり構造テストを優先する。専有環境（`BENCH_DEDICATED_ENV=1`）でのマイクロ
ベンチマーク（`bench-unique-check` 相当）は、確保自体がオーナー作業である
専有環境を要するため、本ファイル「スコープ外・申し送り」節に残す。

## クラッシュ整合

索引の更新は行の書き込み・台帳記録と同じ redb write トランザクション・同じ
`commit_boundary::commit` の内側で行う（`constraint::enforce_unique_keys_in_txn`
は行ストア・索引テーブルの両方を同一 write txn 上で開き、違反時は commit 前に
`Err` を返して txn を破棄する）ため、原子性は構造的に保証される。

専用のクラッシュテストツール `crates/engine/examples/crash_tool_unique_index.rs`
（`scripts/crash_test_unique_index.sh`・Makefile `crash-test-unique-index`・CI
`crash-test-unique-index` ジョブ）を追加した。既存の `crash_test_cross_table.sh`
が使う `crash_tool_cross_table.rs` は `engine::txn::BatchWriteTxn`（`ROWS_TABLE`・
`BATCH_LOG_TABLE` のみ）を直叩きする旧経路であり、SQL 表層が読み書きしない
（`table_generation_bump_coverage.rs` の ALLOWLIST ドキュメント参照）ため UNIQUE
制約・永続一意索引には一切触れない——本変更の crash-consistency 受け入れ条件には
使えず、新規ツールが必要だった。新規ツールは `EngineCore::execute_insert_sql`
（`constraint::enforce_unique_keys_in_txn` → `unique_index::check_and_update` を
経由する production 経路）で `docs (code TEXT UNIQUE)` へ SIGKILL 耐性のある
バッチ INSERT を行い、再起動後に (a) 行 id の 0 起点連続性・バッチ整合に加えて
(b) 「正しさの不変条件」（生存行の各キー値は必ず正引きエントリを持つ）そのものを
検証する——既存の全 `code` 値を新しい id で再挿入しようと試み、必ず `23505`
（UNIQUE 制約違反）で拒否されることを確認する。索引エントリがクラッシュ後の
復旧で 1 件でも欠落していれば、その値だけ誤って受理されてしまうため、この
プローブは「クラッシュ後も索引と行データが整合する」という受け入れ条件を直接
検証するオラクルになる。

## テスト

- 既存の層 A テスト（`unique_constraint.rs`・`table16_primary_key.rs`・
  `table17_foreign_key.rs`・`truncate_table.rs`・`constraint.rs` 内の単体テスト）は
  無変更のまま全て通過する（実装置き換え前後で挙動契約が変わっていないことの
  証拠）。
- 新規 `crates/engine/tests/unique_index.rs`: 削除・TRUNCATE 後の committed な
  キー再利用、TRUNCATE のテナント境界、`DROP TABLE` 後の索引非残留、
  `ALTER TABLE ... ADD UNIQUE` の遅延バックフィル。
- 新規 `crates/engine/src/constraint/unique_index.rs` 内の単体テスト: エンコード
  往復・破損値の拒否、行数非依存性の構造テスト（上記「性能改善の証拠」）、
  UPDATE で一意キー対象列がすべて NULL になった行の旧索引エントリ後片付け
  （段 3 の逆引き差分削除が早期 return で迂回されないことの回帰。Codex レビュー
  指摘・PR #1123）。
- 新規 `crates/engine/examples/crash_tool_unique_index.rs` ＋
  `scripts/crash_test_unique_index.sh`: 上記「クラッシュ整合」節の SIGKILL
  耐性・索引の正しさの不変条件を検証する回帰テスト（Makefile
  `crash-test-unique-index`・CI 同名ジョブ）。

## スコープ外・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）でのマイクロベンチマーク
  （`bench-unique-check` 相当）。確保自体がオーナー作業のため引き継ぐ
  （上記「性能改善の証拠」節参照）。
- ダウングレード運用（旧バイナリでの書き込み）の検出・拒否機構。
- 巨大キー（`TEXT`／`BYTEA`）に対する索引キー長の上限・ハッシュ化。
- FOREIGN KEY 参照先側の照会の同索引への統合。
- SQL `ALTER TABLE ... ADD UNIQUE` / `DROP CONSTRAINT` を導入する際は、索引の
  無効化・`ordinal` の再採番を併せて設計する必要がある。
