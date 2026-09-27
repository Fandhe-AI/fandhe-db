# 述語つき UPDATE/DELETE の実行結線（Issue #871）

- 対象 Issue: #871（親 #861「Phase 1 書き込み DML の表層公開」・ルート #860）
- 前提 Issue: #869（述語つき `UPDATE ... WHERE` の許可リスト・束縛）・#870（述語つき
  `DELETE ... WHERE` の許可リスト・束縛。`docs/design/predicate-dml-where.md`・
  `docs/design/delete-predicate-form.md`）
- 関連 ADR: `docs/design/multi-row-dml-operation-id.md`（Issue #868・RECOVER-11。
  本実装は自動運転モードのため承認を待たずこの ADR を作業前提として実行結線し、
  PR #993 のレビューでオーナー承認〔2026-09-23・ADR ステータス Accepted〕を得た）
- 関連ポインタ: `docs/spec/05-tasks.md` TASK-192・`docs/spec/04-behavior/sql-surface.md`
  SQL-19・`docs/spec/04-behavior/recovery.md` RECOVER-11（2026-09-23 確定）・RECOVER-9・
  RECOVER-10・`docs/spec/04-behavior/rls.md` RLS-7・RLS-9・RLS-10・TABLE-12。
  spec 本文は転記しない。
- 関連コード: `crates/engine/src/recovery/content_hash.rs`（`OpTag::UpdateWhere`／
  `DeleteWhere`・`for_update_where`／`for_delete_where`）・`crates/engine/src/tenant.rs`
  （`PredicateDmlOutcome`・`PredicateDmlError`・`DmlCandidate`・
  `delete_rows_where_unchecked`／`update_rows_where_unchecked`）・
  `crates/engine/src/sql/exec.rs`（`execute_predicate_delete`／
  `execute_predicate_update`・`map_write_error`）・`crates/engine/src/core.rs`
  （`execute_sql_in_session` の `DELETE`／`UPDATE` 分岐・
  `execute_predicate_delete_form`／`execute_predicate_update_form`）

## 1. 背景・目的

述語つき `UPDATE ... WHERE`（#869）・`DELETE ... WHERE`（#870）は許可リスト検証・
束縛まで実装済みだったが、実行結線（候補行列挙・1 トランザクション一括適用・
台帳照合・影響行数上限の実測判定）が存在せず、`EngineCore::execute_sql_in_session`
は述語形 `DELETE` を `42601` で拒否し、`UPDATE` 文自体は先頭トークン分岐を持たな
かった（`validate_sql` へフォールスルーし `42601`）。本 Issue はこの実行結線を追加
する。

## 2. 内容照合ハッシュ（RECOVER-11）

ADR §4 のレイアウトをそのまま `crates/engine/src/recovery/content_hash.rs` へ実装
した（`for_update_where`／`for_delete_where`）。ADR からの意図的な変更点は 2 点:

1. **計算位置**（ADR §5.1 は `bind_update_form`／`bind_predicate_delete` 内での計算
   を推奨するが、`BoundPredicateDelete`／`BoundPredicateUpdate` のコンストラクタが
   既に固定シグネチャを持ち、将来 NoSQL 表層（#876）が生の `WherePredicate`／
   `UdfRegistry` を経由しない別入口を持つ設計であるため）: `core.rs::EngineCore`
   （`Validated*` 形と `session.udfs()` の両方を持つ唯一の呼び出し元）が束縛の直前
   に 1 回だけ計算し、`sql::exec::execute_predicate_delete`／
   `execute_predicate_update` へ `&ContentHash` として渡す。
2. **エラー型**（ADR §4.2 は `StorageError` を示すが）: `sql::allowlist::
   SqlSurfaceError` を直接返す。WASM UDF 呼び出しの拒否（ADR §4.4.1）は
   `sql::allowlist::SqlSurfaceError` に `0A000` 相当の variant が存在しないため
   `SqlSurfaceError::unsupported`（`42601`）へ写像する（`0A000` は NoSQL 表層の op
   許可リスト専用。`grep -n '"0A000"' crates/engine/src/sql/allowlist.rs` で不在を
   確認済み）。

その他のレイアウト（`OpTag::UpdateWhere = 9`／`DeleteWhere = 10`・`SET` 割当の宣言順
直列化・`WHERE` 述語の種別タグ付き直列化・`Expr` のタグ付き前置順直列化・参照 UDF
定義セクションの推移閉包・WASM UDF 呼び出しの拒否判定順序）は ADR §4.3／§4.4／
§4.4.1 のとおり実装した。

3. **`VECTOR` 列 `SET` 割当の正準化と `schema` 引数（Issue #1061）**:
   `for_update_where` に `schema: &TableSchema` を追加し、`SET` 割当のうち
   対象列がスキーマ上 `VECTOR(dim)` の `InsertLiteral::String` は
   `sql::parser::parse_vector_literal` でパースしてから `InsertLiteral::Vector`
   と同一のタグ 5・f32 LE 列レイアウトへ正準化する（`VECTOR` 以外の列は
   従来どおりタグ 1・生テキスト）。SQL・NoSQL 表層跨ぎの同一 `operation_id`
   再送で `content_hash` が食い違い `22023` に誤判定される問題の是正。
   正規化前に記録されえた台帳エントリとの互換は `legacy_hashes`
   （`ledger::record_in_txn_accepting`）で扱う。詳細は
   `docs/design/multi-row-dml-operation-id.md`「9.6 改訂（Issue #1061）」・
   `docs/design/nosql-update-delete-mapping.md`「述語形 VECTOR 割当の表現
   統一と既存台帳エントリの互換性」参照。

## 3. 実行契約（ADR §6）

`tenant.rs::delete_rows_where_unchecked`／`update_rows_where_unchecked` は以下の順序
を 1 write トランザクション内で守る:

1. `begin_write_txn` → スキーマ取得（`expected_schema` 照合で並行 `ALTER TABLE` を
   検知）。
2. `ledger::record_in_txn`（候補列挙より**先**。使用済み `operation_id` は可視集合を
   一切走査せず `23505`／`22023` へ短絡する）。
3. テナント**所有**スコープ（`(tenant, 0)..=(tenant, u64::MAX)`。`is_owner` の二重
   防御）を走査し、呼び出し元が注入した述語クロージャで候補 `id` を確定する
   （`limit + 1` 件で打ち切り）。
4. `limit` 超過なら `write_txn` を drop し `LimitExceeded` を返す（行・台帳とも
   痕跡ゼロ）。
5. 候補 `id` をすべて適用（DELETE は `remove`、UPDATE は read-merge-write。
   単一行 UPDATE と共有する `merge_row_for_update` 経由。Issue #996）。
6. 影響行数が 1 件以上のときのみ `bump_table_generation_in_txn`。
7. `commit_boundary::commit`。

`WHERE` 述語の評価は `sql::exec::execute_predicate_delete`／
`execute_predicate_update` が候補列挙クロージャとして注入し、`sql/scan.rs::
execute_scan` の走査ループと同一の意味論（`declarative_filter::matches_all` → 各
`expr_filters` を `ExprProgram::eval`。`references_embedding && dim == 0` の行は無条
件除外）を共有する（第 2 の述語評価器を作らない）。

## 4. 削除・更新スコープ

候補列挙は「RLS 可視行」ではなく「テナント**所有**（`(tenant_id, id)` キー名前空間
＋ `is_owner`）」スコープを対象とする（単一行 DELETE・`TRUNCATE` と同じ判断。
`docs/design/sql-delete-single-row.md`「削除スコープ」節参照）。

- wire 経由では RLS-11（認証主体は自テナント `Private` 行が可視）により「所有 ⊆
  可視」が成立し、両者は一致する。
- 他テナントの `Public` 行は「可視だが所有ではない」ため候補にならない
  （`SELECT` では見えるが述語つき `UPDATE`／`DELETE` の対象外）。
- engine 直呼び出しの既定 ctx（`Public` のみ）でも自テナント `Private` 行は候補に
  なる（単一行 DELETE と同じ）。
- `enumerate_dml_candidates` は物理キー `(tenant_id, id)`（TABLE-12）が redb の
  タプル `Key` 比較で第 1 要素（`tenant_id`）を主キーとする辞書順になる性質
  （`catalog.rs::scan_table_page` のカーソルが同じ前提に依拠）を利用し、
  `row_table.range` を対象テナントの閉区間 `(tenant, 0)..=(tenant, u64::MAX)`
  に限定して走査する（codex-review P0 指摘・Issue #871: 終端を
  `Bound::Unbounded` にしていた実装は、対象テナントの行が 0 件の場合に限り
  最初の反復で辞書順で後続する別テナントの先頭エントリを取得してしまい、
  「テナントが変わった時点で打ち切る」判定に至る前に他テナント領域の
  キー・値を読んでいた。閉区間終端はこの経路を構造的に塞ぎ、他テナントの
  行はキー・値のいずれも読み進めない・デコードもされないことを保証する。
  テナントが変わった時点で打ち切る判定は defense-in-depth として残置する）。
  他テナント行を全走査してから
  `is_owner` 判定で除外する実装ではない（codex-review P0 指摘・Issue #871:
  以前の全走査実装は総走査上限のカウンタを `is_owner` 判定より前に加算して
  いたため、他テナントの行数が閾値を超えると対象テナントの行が少なくても
  `54000` になり、応答から他テナントのデータ量を推測できてしまっていた）。
  `ctx.is_owner` は `verify_row_key_tenant` が保証するキー↔ヘッダ整合の
  帰結として走査範囲内では常に真になる不変条件を、defense-in-depth として
  明示検査するのみ。
- `enumerate_dml_candidates` は対象テナント所有行のみに対する総走査行数上限
  （`tenant::MAX_SCANNED_ROWS`。`visible_rows` と共有する同一値）を適用する
  （codex-review P1 指摘）。影響行数上限（`MAX_DML_AFFECTED_ROWS`）は述語に
  一致した行にしか作用しないため、一致行が 0 件のまま推移する述語では認証
  済みテナントが単一 writer を占有したまま自テナントの行を任意規模で走査
  し続けられる経路があり、この総走査上限で塞ぐ（他テナントのデータ量には
  一切依存しない）。超過時は `TenantWriteError::TooManyRowsScanned`
  （`54000`）で副作用ゼロ（`write_txn` を commit せず破棄）のまま終端する。

`WHERE visible() のみ` の述語つき DELETE は #870 の既存決定（自テナント全行を候補
にする。歯止めは影響行数上限のみ）をそのまま継承し、本 Issue で再決定していない
（`docs/design/delete-predicate-form.md`「記録する判断」節参照）。述語つき UPDATE の
`WHERE visible() のみ` は #869 の既存契約どおり束縛段で `42601` のまま。

## 5. エラー優先順位

`core.rs::execute_predicate_delete_form`／`execute_predicate_update_form`:

構造検証・`operation_id` 必須化・カタログ存在確認（`sql::allowlist::
validate_delete_statement_tokens`／`validate_update_form_tokens` が呼び出し元で
既に適用済み） → スキーマ取得（`42P01`。並行 `DROP TABLE` の防御的経路） → 束縛
（`22000`。`sql/scan.rs` と同じ失敗点） → 内容照合ハッシュ計算（WASM UDF 呼び出し
の拒否・`42601`） → 実行本体（台帳照合 `23505`／`22023`・上限超過 `54000`）。

## 6. 上限 API の並立（申し送り。ADR §6 の既存申し送り）

`DELETE` 側は `DEFAULT_MAX_DML_AFFECTED_ROWS`＋`check_affected_row_count(count,
limit)`、`UPDATE` 側は `MAX_DML_AFFECTED_ROWS`＋`check_dml_affected_rows(count)`と
いう、シグネチャの異なる 2 つの上限 API が並立している（いずれも
`crates/engine/src/sql/parser.rs`。値はいずれも 1,000）。両者の統合は本 Issue の対
象外のまま。

## 7. PR #989（#865 単一行 UPDATE 実行結線）・PR #991（RETURNING）との整合ルール

実装開始時点（origin/main `b790abd`）で PR #989（単一行 `id` 完全一致形 UPDATE の
実行結線）・PR #991（`RETURNING`）はいずれも未マージ（OPEN）だったため、本 Issue
は当初「PR #989 未マージ」の経路（計画 §4.6-B）で実装した。その後 origin/main への
追随（PR #989・PR #991 の順にマージ済み）により、本ブランチは両 PR の定義をそのまま
再利用する形へ整合させた:

- `exec::UpdateOutcome { rows_affected: u64 }`・`SqlOutcome::Update
  (exec::UpdateOutcome)`・`simple_query.rs` の `UPDATE <n>` アームは PR #989
  （#865）が導入した定義をそのまま再利用する（本 Issue が独自に導入していた
  同名定義は PR #989 マージ時に置き換え済み）。`core.rs::execute_sql_in_session`
  の `UPDATE` 分岐は `bind_update_form` の戻り値（`BoundUpdateForm`）を
  `Single` 腕（PR #989 の `execute_update_with_schema` へ委譲）・`Predicate`
  腕（本 Issue の `execute_predicate_update_form` へ委譲）へ振り分ける。
- `SqlOutcome::Update` の追加は PR #989 で **BREAKING CHANGE** として導入済み。
  本 Issue はこの型を変更しない。
- `DELETE` は `crate::sql::allowlist::DeleteStatement`（`SingleRow`／
  `Predicate`）で振り分ける。`SingleRow` 腕はさらに `RETURNING`（PR #991・
  Issue #873）の有無で `execute_delete_returning_form`（PR #991 導入）／
  `execute_delete_form`（既存）へ分岐し、`Predicate` 腕は常に
  `execute_predicate_delete_form`（本 Issue）へ委譲する。述語形 DELETE／UPDATE
  と `RETURNING` の組合せは、構造検証段（`sql::allowlist::
  validate_delete_statement_tokens`／`validate_update_form_tokens`）が
  `RETURNING` 併用を `42601` で拒否するため、実行結線側では到達しない
  （PR #991 が導入した契約をそのまま維持）。
- `crates/wire-server/tests/wire_error_response.rs::err1_update_returns_42601_fields`
  の入力へ `USING OPERATION_ID` を付与した（`validate_update_form_tokens` が
  `operation_id` 必須化ガードを構造検証の直後に行うため、欠落時は `42601` ではなく
  `23502` になる。この変更は PR #989 で正式に取り込み済み）。

## 8. テスト

- `crates/engine/tests/sql_predicate_dml_exec.rs`: 候補列挙・応答件数の同値性
  （DELETE／UPDATE）・RLS 境界（他テナント行の非影響・非漏えい）・0 行一致の台帳
  記録と再送拒否・内容照合ハッシュ（述語順入替での `22023`）・`WHERE visible()`
  のみの DELETE・`operation_id` 欠落・`execute_sql`（セッション無し）の既存拒否・
  影響行数上限超過（`54000`・副作用ゼロ。DELETE 側
  `predicate_delete_over_limit_is_rejected_with_no_side_effects`・UPDATE 側
  `predicate_update_over_limit_is_rejected_with_no_side_effects`——§6 の上限
  API 並立を踏まえ両者を独立に固定）。
- `crates/engine/tests/predicate_dml_failure_injection.rs`: 候補列挙途中の式評価
  エラー（0 除算）が write トランザクション全体を副作用ゼロで拒否すること（RLS 可視
  列は `TEXT` を算術に使えないため、疑似列 `id` の算術で誘発）・台帳未記録（同一
  `operation_id` の再利用が可能）・drop→再オープン後も整合。
- `crates/engine/tests/sql_delete_predicate_bind.rs::
  session_executes_predicate_delete_statement`: #870 が固定していた「まだ拒否され
  る」テストを「0 件一致で成功する」へ反転。

## 9. 申し送り・スコープ外

- NoSQL `update`／`delete` op の束縛・結線（#876）・SQL/NoSQL パリティ（#877）。
- 上限 API（§6）の統合・既定値の確定（オーナー判断）。
- spec 側 RECOVER-11 は 2026-09-23 に確定済み（`docs/spec` submodule を確定後の参照へ更新。
  確定は本 PR のマージを条件とする）。
- `WasmUdfBackend` への安定な定義識別子の追加（wasmtime 接続時）。
- スカラー列二次索引（`sql::scalar_index`）による候補削減の適用（本 Issue は write
  txn 内の全走査で正しさを優先。性能改善は後続）。
- `EXPLAIN UPDATE/DELETE`・`OR`／括弧付き述語・層 B の 3 クライアント e2e への追加。
- `tenant.rs` 内部の `#[cfg(test)]` 失敗注入シーム（`arena.rs` の先例と同型）は
  本 Issue の時間的スコープでは追加せず、公開 API 経由の式評価エラー注入のみで
  atomicity を検証した（§8 参照）。
- Issue #996 で述語つき UPDATE の read-merge-write（本 Issue が predicate UPDATE
  専用にインライン実装していた部分）を単一行 UPDATE（`update_row_columns_
  unchecked`）と共有する `tenant::merge_row_for_update` へ統一済み。`decode_
  scalar_columns`（全列複製）ではなく借用版 `scan_scalar_columns` 経由の
  `merge_encode_scalar_columns` を通るため、部分 UPDATE 1 回あたりの確保量が
  単一行 UPDATE 側（PR #989）と同水準になった。`tenant::upsert_typed_rows_
  unchecked`（`DoUpdate` 腕）との共通化は引き続き別スコープ。書き込まれる
  バイト列・エラー分類・台帳契約はいずれも不変であることを回帰テストで固定
  （`crates/engine/src/row_codec.rs::merge_encode_scalar_columns_matches_
  decode_then_encode_scalar_columns`・`crates/engine/src/tenant.rs::update_
  rows_where_unchecked_writes_byte_identical_rows_to_legacy_reencode_
  algorithm`）。SET 列が重複指定された場合の意味論が後勝ちから先勝ちへ変わる
  （束縛段で拒否済みのため表層からは到達しない）。
