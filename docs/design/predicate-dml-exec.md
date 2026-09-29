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

## 6. 上限 API の統合と既定値・CLI 設定可能化（Issue #997。オーナー判断の改訂〔2026-09-27〕が正本）

以前は `DELETE` 側が `DEFAULT_MAX_DML_AFFECTED_ROWS`＋`check_affected_row_count(count,
limit)`、`UPDATE` 側が `MAX_DML_AFFECTED_ROWS`＋`check_dml_affected_rows(count)`と
いう、シグネチャの異なる 2 つの上限 API に並立していた（申し送り。ADR §6）。
Issue #997 でこれを解消した。オーナー判断は本 Issue の実装期間中に 2 回示され、
本節は**改訂後（2 回目・最終）の判断**を正本として記録する（1 回目の判断
「既定値 1,000 を維持したまま CLI で上書き可能にする」は開発途中で置き換えられ、
一部は PR #1122 の中間コミットにのみ残る。squash 後の履歴には残らない）。

### 6.1 最終判断（オーナー判断の改訂・2026-09-27）

- **理由**: 汎用 RDB（PostgreSQL 等）の挙動に合わせる。述語形 `UPDATE`／
  `DELETE` の 1 文あたり影響行数上限・複数行 `VALUES` の 1 文あたり行数上限は
  **既定で無効（上限なし）**とする。
- **設定可能化**: `wire-server` 起動時 CLI フラグで明示指定した場合のみ有効に
  なる（指定可能範囲 `1..=1,000,000`。範囲外・不正値・多重指定は起動時に
  fail-closed で拒否する）。セッション・テナント単位の設定は対象外。
- **資源上限**: 上限を明示指定しない場合でも、既存の SQL 文長上限
  （`sql::lexer` の入力長上限）・1 文あたり総走査行数上限
  （`crate::tenant::MAX_SCANNED_ROWS`＝1,000,000）は変更せず、引き続き資源
  枯渇（計算量 DoS）を防ぐ役割を担う。
- **契約は不変**: 上限を設定して超過した場合の `54000`
  （`SqlSurfaceError::PayloadTooLarge`）・副作用ゼロ（RECOVER-11）は変えない。

### 6.2 実装

- **唯一の判定 API**: `crates/engine/src/sql/parser.rs` の
  `check_dml_affected_rows_with_limit(count: usize, limit: Option<NonZeroUsize>)
  -> Result<(), SqlSurfaceError>`。`limit` が `None`（既定・上限なし）なら常に
  成功、`Some(limit)`（CLI 明示指定時）なら `count > limit.get()` で `54000`。
  `UPDATE`・`DELETE`（述語形）の両実行結線（`sql/exec.rs` の
  `execute_predicate_update`／`execute_predicate_delete`）が、呼び出し元
  （`core.rs::EngineCore` の `execute_predicate_update_form`／
  `execute_predicate_delete_form`）から渡された
  `self.dml_limits.max_affected_rows`（`Option<NonZeroUsize>`）とともにこれを
  呼ぶ。**BREAKING CHANGE**: 「既定＝上限なし」は `usize` では表現できないため、
  旧 pub API `MAX_DML_AFFECTED_ROWS`（`pub const usize` ＝ 1,000）・
  `check_dml_affected_rows(count: usize)`（`limit` 引数を取らない版）は削除した
  （`DEFAULT_MAX_DML_AFFECTED_ROWS`・`check_affected_row_count`・
  `BoundPredicateDelete::max_affected_rows()`〔および `BoundPredicateDelete::new`
  の同名引数〕は本 Issue のより前〔PR #1116〕の時点で既に削除済み）。
- **`DmlLimits`**: `crates/engine/src/sql/parser.rs::DmlLimits`
  （`max_affected_rows: Option<NonZeroUsize>`・
  `max_insert_rows_per_statement: Option<NonZeroUsize>`。`Default` は両方
  `None`）を `core.rs::EngineCore::with_dml_limits`（`crate::batch_limits::
  BatchLimits` と同じビルダー流儀）経由で注入する。`wire-server` 側は
  `crates/wire-server/src/dml_limits_opt.rs` が `--max-dml-affected-rows`・
  `--max-insert-rows` の 2 フラグを解析し、`main.rs` が起動時に 1 回だけ
  `core.with_dml_limits(..)` を呼ぶ（未指定は `resolve(None, None)` が
  `DmlLimits::default()` と同値を返す）。
- **範囲検証**: 指定可能範囲は `1..=1,000,000`（`crate::tenant::
  MAX_SCANNED_ROWS`＝総走査行数上限と同値。定数を共有し値のドリフトを防ぐ）。
  範囲外・非数値・多重指定はいずれも起動時に fail-closed で拒否する
  （`engine::sql::parser::validate_dml_row_limit` が範囲判定して
  `NonZeroUsize` へ変換、`dml_limits_opt::parse_strict_decimal` が untrusted な
  CLI 文字列の厳密パース、`main.rs` の引数走査ループが多重指定拒否を担う——
  他の閉じた語彙フラグ〔`--search-engine` 等〕と同じ「2 回目以降の指定を
  fail-closed に拒否する」流儀）。
- **総走査上限との関係**: `tenant::enumerate_dml_candidates` は `limit` が
  `Some` の場合のみ `limit + 1` 件で列挙を早期打ち切りする（副作用ゼロ判定の
  ための最小超過分の蓄積）。`limit` が `None` の場合はこの早期打ち切りを行わず、
  `MAX_SCANNED_ROWS`（対象テナント所有行を 1 行デコードするたびに加算する
  総走査行数上限。`limit` の設定有無に関わらず常に適用）のみで頭打ちになる。
  これにより、上限を明示指定しない構成でも「一致しない広い述語による無制限
  走査」は資源上限として防がれたまま、一致件数自体には上限が掛からない。
- **INSERT 側との対称性**: 複数行 `VALUES` の 1 文あたり行数上限
  （旧 `sql::allowlist::MAX_INSERT_ROWS_PER_STATEMENT`。SQL-16・TASK-190）も
  同じ `DmlLimits`・同じ CLI 起動時設定の仕組みで揃えた。構文解析段
  （`Parser::parse_insert`）が判定するため、`Parser` に `max_insert_rows:
  Option<NonZeroUsize>` フィールドを追加し（既定 `None`。`Parser::new` の
  初期化のみで既存の他の呼び出しに影響を与えない）、
  `validate_insert_tokens_with_limit`（`pub(crate)`）・
  `validate_insert_with_limit`（`pub`。`validate_insert` は本関数へ `None`
  〔上限なし・既定〕を渡すだけの薄い委譲）を新設して `core.rs` の 3 つの
  INSERT 実行エントリポイント（`execute_insert_sql`・`parse_tokens` の INSERT
  分岐・`execute_insert_sql_batch`）すべてから到達させた。旧
  `MAX_INSERT_ROWS_PER_STATEMENT` 定数は非テストコードから参照しなくなった
  ため `#[cfg(test)]` 限定・`pub(crate)` を外して残す（外部への破壊的変更には
  当たらない。元々 `pub(crate)` で crate 外から到達不能だったため）。
- **実行時の再検査（codex-review P1 指摘・PR #1122 対応）**: 行数上限判定は
  本来 `Parser::parse_insert`（構文解析段）が一元的に担う契約だが、`pub fn
  validate_insert`（常に `max_insert_rows: None` で解析する）が返した
  `ValidatedInsert` を `ParsedSql::Insert` へ包んで `pub fn
  execute_parsed_in_session`（拡張クエリプロトコルの Parse／Execute 分離
  〔Issue #933〕が正当に使う経路）へ渡すと、解析時の上限判定を経由しない
  まま実行されてしまう。`EngineCore::check_insert_row_count_limit`（新設）を
  `execute_insert_form`・`execute_insert_returning_form` の冒頭（スキーマ
  取得・書き込みトランザクション開始より前）で呼び、到達経路に関わらず
  `self.dml_limits.max_insert_rows_per_statement` を実行時にも必ず検査する
  （超過は `54000`・副作用ゼロ。構文解析段のメッセージ形式と同一）。
- **`batch_limits.max_files_per_batch` との二重ゲート（codex-review P1 指摘・
  PR #1122 対応）**: 複数行 `VALUES`（`BoundInsertForm::RowBatch`）は本節の
  `max_insert_rows_per_statement`（`Some` のときのみ判定する構文解析段の上限）
  に加え、`self.batch_limits.max_files_per_batch`（既定 64。Issue #860
  SQL/NoSQL 機能パリティ。`Self::validate_insert_row_batch_limits`）の
  独立した上限を常に通る。`max_insert_rows_per_statement` を引き上げる、また
  既定（`None`＝上限なし）のままにするだけでは既定 64 行を超える複数行
  `VALUES` は `batch_limits` 側で `54000` になるため、1,000 行超を単一の
  複数行 `VALUES` で受理させるテストでは両方を引き上げる必要がある
  （`crates/engine/tests/insert_multi_row.rs::
  multi_row_insert_respects_configured_higher_insert_row_limit`・
  `multi_row_insert_default_has_no_row_count_cap` 参照）。`max_files_per_batch`
  は Issue #1166 で CLI フラグ `--batch-max-files`（範囲 `1..=1,000,000`。優先
  順位は CLI 明示 > 環境変数 `VECTOR_DB_BATCH_MAX_FILES` > 既定 64）からも設定
  できる。複数行 `VALUES` は `max_batch_chunks`（既定 4096。環境変数
  `VECTOR_DB_BATCH_MAX_CHUNKS`）の判定も受けるため、実効行数上限は
  `min(max_files_per_batch, max_batch_chunks)` になる。`--max-insert-rows` を
  明示指定し、その値がこの実効上限を超える場合、CLI
  の引き上げが黙って無効化される事故を防ぐため、`wire_server::dml_limits_opt::
  insert_rows_cap_warning` が起動ログへ `WARNING` 行を出す（`--durability
  none` の `WARNING` と同じ「非既定値を明示選択したときだけ警告する」設計
  判断。エラーにはしない。`--max-insert-rows` 未指定〔既定〕では警告しない
  ——`batch_limits.max_files_per_batch` は本 Issue 以前から常に適用されて
  きた既存の暗黙上限であり、フラグ未指定という「何も選択していない」状態を
  毎回警告すると通常起動のたびにノイズになるため）。
- **NoSQL（HTTP）表層の `insert` op・単一行 `update`／`delete`・ファイル形
  INSERT は `max_affected_rows`／`max_insert_rows_per_statement` の対象外
  （理由）**: NoSQL `update`／`delete` op は `TargetForm::Predicate`（`filter`
  指定。Issue #1062）の場合のみ `EngineCore::
  execute_bound_predicate_update_in_session`／
  `execute_bound_predicate_delete_in_session` 経由で述語形 DML
  （`execute_predicate_delete`／`execute_predicate_update`）へ到達し、
  `self.dml_limits.max_affected_rows` を共有する（§9 参照）。`id` 指定の
  単一行形（`TargetForm` の他 variant）は引き続き `BoundDelete`／
  `execute_update_with_schema` を経由し、`max_affected_rows` は無関係
  （単一行形は常に `rows_affected` が `0`／`1` のいずれかで、複数行への
  上限判定自体が意味を持たない）。NoSQL `insert` op は
  `max_insert_rows_per_statement`（構文解析段の上限）自体を経由せず、既存の
  `batch_limits.max_files_per_batch` のみで行数を制御する（SQL 表層の複数行
  `VALUES` と同じ土俵——上記の二重ゲート注記参照）。ファイル形 INSERT
  （`execute_file_insert`）は 1 文＝1 ファイルであり複数行 `VALUES` 構文を
  持たないため `max_insert_rows_per_statement` の対象外（チャンク数・バイト量の
  上限は `incremental.rs`／`batch_limits.rs` が別途担う）。

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
  既定（`--max-dml-affected-rows` 未指定＝上限なし）で旧既定値（1,000）超の
  一致でも成功すること（BREAKING CHANGE の外部観測。DELETE 側
  `predicate_delete_default_has_no_affected_rows_cap`・UPDATE 側
  `predicate_update_default_has_no_affected_rows_cap`）・CLI 起動時設定値の
  反映（`EngineCore::with_dml_limits` 経由。設定値より低い一致件数でも
  `54000`・副作用ゼロになること／設定値の範囲内なら 1,000 件超でも成功する
  こと——DELETE・UPDATE それぞれ引き上げ・引き下げの両方向を
  `predicate_delete_respects_configured_lower_affected_rows_limit`／
  `predicate_delete_respects_configured_higher_affected_rows_limit`／
  `predicate_update_respects_configured_lower_affected_rows_limit`／
  `predicate_update_respects_configured_higher_affected_rows_limit` で固定）。
- `crates/engine/tests/insert_multi_row.rs`: 複数行 `VALUES` の行数上限の
  CLI 起動時設定値対応。既定（`--max-insert-rows` 未指定＝上限なし。
  `batch_limits.max_files_per_batch` を明示的に引き上げて構文段の判定のみを
  切り分ける）で旧既定値（1,000）超でも成功すること
  （`multi_row_insert_default_has_no_row_count_cap`）・明示指定した値を
  超える行数は `54000`・副作用ゼロになること
  （`multi_row_insert_exceeding_configured_row_limit_is_rejected_with_54000_and_no_side_effects`）・
  設定値の引き上げ・引き下げ双方向
  （`multi_row_insert_respects_configured_lower_insert_row_limit`／
  `multi_row_insert_respects_configured_higher_insert_row_limit`——引き上げ側は
  `batch_limits.max_files_per_batch` も同時に引き上げる必要があることを含めて
  固定。§6「`batch_limits.max_files_per_batch` との二重ゲート」参照）・
  構文解析段の上限判定を経由しない到達経路（`validate_insert` が返した
  `ValidatedInsert` を `ParsedSql::Insert` へ包んで `execute_parsed_in_session`
  へ渡す）でも実行時に上限を再検査すること（`execute_parsed_in_session_
  rejects_insert_over_configured_row_limit_even_when_parsed_without_a_limit`。
  §6「実行時の再検査」参照）。
- `crates/wire-server/tests/wire_dml_limits_cli.rs`: `--max-dml-affected-rows`・
  `--max-insert-rows` の CLI 解析の外形確認（範囲内値での起動成功・値欠落／
  多重指定／範囲外／非数値の起動時拒否）。
- `crates/wire-server/src/dml_limits_opt.rs`（`#[cfg(test)]`）・
  `crates/engine/src/sql/parser.rs`（`#[cfg(test)]`）: `resolve`／
  `validate_dml_row_limit`／`check_dml_affected_rows_with_limit`／
  `DmlLimits::default` の単体テスト。
- `crates/engine/tests/predicate_dml_failure_injection.rs`: 候補列挙途中の式評価
  エラー（0 除算）が write トランザクション全体を副作用ゼロで拒否すること（RLS 可視
  列は `TEXT` を算術に使えないため、疑似列 `id` の算術で誘発）・台帳未記録（同一
  `operation_id` の再利用が可能）・drop→再オープン後も整合。
- `crates/engine/tests/sql_delete_predicate_bind.rs::
  session_executes_predicate_delete_statement`: #870 が固定していた「まだ拒否され
  る」テストを「0 件一致で成功する」へ反転。

## 9. NoSQL 表層からの到達経路（Issue #1062）

NoSQL `update`／`delete` op の `filter`（述語形。TASK-186・NOSQL-12）は、
本ドキュメントが記す SQL 表層の実行本体を**そのまま**共有する。到達経路:

- `EngineCore` に `execute_bound_predicate_update_in_session`／
  `execute_bound_predicate_delete_in_session`（セッション対応の束縛済み
  入口。[`Self::execute_bound_update_in_session`] と同型の closure 方式）を
  追加した。判定順序は `operation_id` 必須化ガード → スキーマ取得 → `bind`
  closure（`wire-server` が JSON `filter` から `WherePredicate` を構築する）
  → 述語形の多層防御（`reject_unsupported_predicate_dml_forms`。空列・
  `PredicateCall`／`Expression`／`Or`／`InSubquery`／`Exists` を `42601` で
  拒否）→ `ValidatedPredicateUpdate`／`ValidatedPredicateDelete` を engine
  内部で構築（`pub(crate)` フィールドへの struct リテラル。公開コンストラクタは
  追加しない）→ 本ドキュメント §5〜7 の共通実行本体（`Self::
  run_predicate_update`／`run_predicate_delete`）。
- `core.rs::execute_predicate_update_form`／`execute_predicate_delete_form`
  （SQL 表層。§5）は、スキーマ取得より後の部分をこの共通実行本体へ切り出した
  だけで、挙動は本 Issue 導入前と完全に同一（既存の engine テストで回帰確認
  済み）。
- `parser.rs::bind_update_form` の `Predicate` 分岐も同様に
  `bind_predicate_update`（新設）へ切り出し、`core.rs` の
  `run_predicate_update` から直接呼べるようにした。
- NoSQL `filter` → `WherePredicate` の写像・content_hash 一致条件は
  `docs/design/nosql-update-delete-mapping.md`「D1」節を参照（spec 本文は
  転記しない）。
- `run_predicate_update`／`run_predicate_delete` はいずれも
  `self.dml_limits.max_affected_rows`（§6。Issue #997。既定 `None`＝
  上限なし・`wire-server` の `--max-dml-affected-rows` で明示指定時のみ
  有効）を `sql::exec::execute_predicate_update`／`execute_predicate_delete`
  へ渡す。したがって NoSQL 表層からの述語形 `UPDATE`／`DELETE`（`filter`）も
  SQL 表層と同じ process-wide 設定値を共有する（Issue #997・#1062 の統合。
  §6「NoSQL（HTTP）表層・ファイル形 INSERT は対象外」の記述は、NoSQL
  `insert` op・ファイル形 INSERT に限る注記として引き続き有効）。

## 10. 申し送り・スコープ外

- NoSQL `update`／`delete` op の束縛・結線（#876・述語形は #1062 で実装済み）・
  SQL/NoSQL パリティ（#877。読み取り専用シナリオのみ。述語形パリティは層 A
  `nosql12_update_delete.rs` が #1062 で固定）。
- 上限 API の統合・CLI 設定可能化はいずれも Issue #997 で解消済み（§6 参照。
  オーナー判断の改訂〔2026-09-27〕で最終確定。既定は上限なし・CLI 明示指定時
  のみ有効）。
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
