# HTTP 表層（NoSQL）と PostgREST・Elasticsearch `_bulk`・MySQL X DevAPI の比較

- ステータス: Accepted（実装済みの挙動と、他製品の公式ドキュメントの記述を並べた比較記録。本書の追加でコードは変えない）
- 対応 Issue: #2110（親 #2109）。前提の実装は #2085・#2089・#2092・#2096・#2099・#2103
- ビヘイビア ID ポインタ: HTTP-14・HTTP-15・NOSQL-1・NOSQL-6・NOSQL-18・NOSQL-20・NOSQL-21・NOSQL-22・NOSQL-23
  （`docs/spec/04-behavior/`）。契約本文は spec を正とし、本書へは転記しない
- 他製品の記述の基準: 「参照」節の公式ドキュメントを確認日（2026-11-05）に読んだ内容だけを書く。
  確認できなかった点は「公式ドキュメントで確認できず」と書き、記憶で補わない
- 関連コード: `crates/wire-server/src/http/query/schema.rs`・`dml_target.rs`・`filter.rs`・`upsert.rs`・
  `returning.rs`・`cursor.rs`・`explain.rs`・`openapi.rs`
- 関連 doc: `crates/wire-server/docs/nosql-api.md`・[idempotent-resend-comparison.md](idempotent-resend-comparison.md)・
  [partitioned-dml-comparison.md](partitioned-dml-comparison.md)・[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)・
  [nosql-filter-operators.md](nosql-filter-operators.md)・[nosql-update-delete-mapping.md](nosql-update-delete-mapping.md)・
  [sql-upsert.md](sql-upsert.md)・[sql-returning.md](sql-returning.md)

## 目的と読み方

fandhe の HTTP 表層（`wire-server --surface nosql` の `POST /v1/query`）を、PostgREST・Elasticsearch の `_bulk` API・
MySQL X DevAPI の相当機能と同じ軸で並べる。「同等以上」と書くのは、fandhe 側にテストで固定した根拠がある行に限る。
差異・未提供の点は隠さず書く。fandhe 列の先頭の区分は【持つ】【持たない】【計画中】で、【計画中】は本リポに open の
Issue が実在するものだけに付ける（確認できなければ【持たない】とする）。

## 前提

- 比較対象の性質が違う。PostgREST は PostgreSQL の上に載る REST 層で、HTTP 動詞と URL で表を操作する。
  Elasticsearch の `_bulk` は NDJSON による文書単位の一括 API。MySQL X DevAPI は MySQL のクライアント API で、HTTP ではない
- fandhe は単一の `POST /v1/query` と本文の `op`（現在 14 値）ですべての操作を表す。SQL 表層と実行器・RLS・台帳・
  エラー契約（`wire_code`）を共有し、新しい `wire_code` は足さない
- PostgREST の `Prefer: return=` に相当する fandhe のキーは `returning` である（名前が違う）
- 14 値: `search`・`scan`・`aggregate`・`insert`・`upsert`・`update`・`delete`・`create_table`・`alter_table`・
  `drop_table`・`create_index`・`drop_index`・`show_partitioned_dml`・`cancel_partitioned_dml`

## 比較表

| 比較軸 | PostgREST | Elasticsearch `_bulk` | MySQL X DevAPI | fandhe |
| ------ | --------- | --------------------- | -------------- | ------ |
| 操作の語彙・動詞 | 読み取りは GET（HEAD も可）、挿入は POST、更新は PATCH、削除は DELETE、単一行の upsert は PUT | 1 本の NDJSON に `index`・`create`・`update`・`delete` の行を並べる | Collection の `add`・`find`・`modify`・`remove`（単一文書用の関数は別節にあるが、本書では名称の確認に至らず） | 【持つ】`POST /v1/query` に `op` 14 値（`nosql1_op_vocabulary.rs`。NOSQL-1）。動詞では区別しない |
| フィルタ演算子と論理結合 | `eq`・`neq`・`gt`・`gte`・`lt`・`lte`・`like`・`ilike`・`match`・`in`・`is`・`isdistinct`・全文検索・配列／範囲演算子、`not`・`or`・`and` | 公式ドキュメントの該当ページで確認できず（`_bulk` は検索条件を持たず文書 ID 単位） | `find`・`modify`・`remove` に検索条件式を渡す（式の演算子一覧は公式ドキュメントで確認できず） | 【持つ】葉は 14 語彙（`eq`・`ne`・`prefix`・`like`・`between`・`is_null`・`not_null`・範囲比較 6 種・`in`）、グループは `or`・`not`、配列自体は AND（`nosql14_filter_operators.rs`・`nosql7_filter_mapping.rs`）。全文・配列／範囲演算子は持たない |
| 一括操作と項目別結果 | 配列または CSV の本文を 1 本の INSERT 文として実行 | 項目ごとに `status` を持つ `items[]` と、失敗の有無を示す `errors` を返す。応答の項目は送信順 | 公式ドキュメントで確認できず | 【持たない】`rows` は全か無か（上限は既定 64、超過は `54000`）。項目別の部分成功は返さない（`nosql6_insert.rs` の `rows_over_max_files_per_batch_is_54000_without_side_effects`・`multi_row_batch_resend_same_content_is_23505_and_mismatch_is_22023`。NOSQL-6）。理由は後述 |
| upsert | `Prefer: resolution=merge-duplicates`／`ignore-duplicates` の POST（既定は主キー、`on_conflict` で UNIQUE 列）。単一行は PUT | `index` は追加または置換、`create` は ID が無いときだけ追加、`update` は部分更新 | 公式ドキュメントで確認できず（単一文書の操作は別節に記載） | 【持つ】`upsert` op と `on_conflict`（`columns`・`do`=`nothing`／`update`・`set`）。SQL の `INSERT ... ON CONFLICT` と同じ実行器（`nosql21_upsert.rs` の `parity_do_update_with_default_set_updates_conflicting_row`・`conflict_target_not_matching_a_constraint_is_42p10_like_sql`。NOSQL-21） |
| 返却内容の制御 | `Prefer: return=minimal`（書き込みの既定）・`headers-only`・`representation` | 項目ごとの `result` を返す。更新後の文書本体を返す指定は公式ドキュメントで確認できず | 公式ドキュメントで確認できず | 【持つ】`insert`・`update`・`delete` の `returning`（列名の配列。`"*"` は全列）。SQL の `RETURNING` と応答が一致（`nosql22_returning_exec.rs` の `http_returning_matches_sql_returning_byte_for_byte`。NOSQL-22）。`upsert` は `returning` を持たず `42601` |
| 楽観的並行制御 | 該当する仕組みは公式ドキュメントで確認できず | `if_seq_no`・`if_primary_term` を `index`・`update`・`delete` の行に付ける | 公式ドキュメントで確認できず | 【持つ】`update`・`delete` の `if`（期待値の条件を AND で加える）。不一致・不在・他テナント・RLS で不可視はすべて `200` で `n=0` の同一応答（`nosql18_conditional_dml.rs` の `zero_row_responses_are_byte_identical_across_all_non_match_causes`。NOSQL-18）。`mode: "partitioned"` との併用は `42601` |
| ページング | `limit`・`offset`、または `Range-Unit: items` と `Range` ヘッダ。応答に `Content-Range` | `search_after` 等の検索側 API は公式ドキュメントで確認できず（`_bulk` は対象外） | 公式ドキュメントで確認できず（`find` の詳細ページを取得できず） | 【持つ】`offset`（`nosql15_offset.rs`）と、`after`／`next_cursor` のキーセット方式。カーソルはテナント・主体・表・`sort` に束縛し改ざんは `22000`。ページ間の書き込み・RLS・SQL 並びとの一致を検証（`nosql23_scan_keyset.rs`・`nosql23_scan_keyset_rls_concurrency.rs`。NOSQL-23） |
| 実行計画 | 実行計画を返す仕組みは公式ドキュメントの該当ページを取得できず確認できず | 公式ドキュメントで確認できず | 公式ドキュメントで確認できず | 【持つ】`search`・`scan`・`aggregate` の `explain: true`。本体を実行せず SQL の `EXPLAIN` と同一内容の `QUERY PLAN` を返す（`nosql10_explain.rs` の `explain_true_matches_sql_explain_rows_with_filter`・`aggregate_and_scan_explain_true_match_sql_explain_rows`） |
| 集約・GROUP BY | 集約関数 `avg`・`count`・`max`・`min`・`sum`。既定は無効で、設定 `db-aggregates-enabled` で有効化 | `_bulk` の対象外 | 公式ドキュメントで確認できず（`find` の詳細ページを取得できず） | 【持つ】`aggregate` op（`count`・`sum`・`avg`・`min`・`max`、`group_by` 1〜8 列、`having`、`sort`、`offset`）。既定で使える（`nosql4_aggregate.rs`・`nosql5_group_by.rs`・`nosql4_5_aggregate_wire_parity.rs`） |
| 書き込みの可視化 | 公式ドキュメントで確認できず | `refresh` に `true`（影響シャードを更新）・`wait_for`・`false`（既定）を指定 | 公式ドキュメントで確認できず | 【持たない】`refresh` 相当の指定は無い。成功応答の時点で書き込みは確定済み（応答の組み立て失敗時も確定済みとして緊急応答を返す。`nosql-api.md` 参照） |
| トランザクション | `Prefer: tx=rollback` は設定 `db-tx-end` が有効なときだけ。要求をまたぐ Tx は無い | 公式ドキュメントで確認できず | `startTransaction()`・`commitTransaction()`（例では `commit()`）・`rollbackTransaction()`（例では `rollback()`）、savepoint の節あり | 【持たない】op 語彙に明示トランザクション制御が無い。通常モードの書き込みは 1 要求 = 1 原子実行（`update`／`delete` の `mode: "partitioned"` は除く。後述）。`25000`・`3B001` は NoSQL から到達不能（`nosql-api.md` の `wire_code` 表）。明示 Tx は SQL 表層で使う |
| 認証・認可 | JWT の `role` クレーム、未送信時は匿名ロール | 公式ドキュメントの該当ページで確認できず | セッション認証（接続時） | 【持つ】`POST /v1/session` で Bearer を発行。TTL は 3600 秒固定でスライドせず、`POST /v1/session/close` で失効。`scope`（`ops`・`tables`・`read_only`）で権限を狭められ、広げる手段は無い。スコープ外は `42501`（`http15_session_scope.rs` の `scope_only_narrows_and_never_widens_permissions`・`widening_scope_in_body_never_turns_42501_into_success`。HTTP-15） |
| 冪等再送 | 公式ドキュメントで確認できず | 公式ドキュメントで確認できず | 公式ドキュメントで確認できず | 【持つ】本文の `operation_id` または `Idempotency-Key` ヘッダ。同内容の再送は `23505`、内容違いは `22023`（`http14_idempotency_key.rs` の `header_path_reaches_the_same_ledger_as_body`。HTTP-14）。詳細は [idempotent-resend-comparison.md](idempotent-resend-comparison.md) |
| 機械可読なスキーマ | ルートパスで OpenAPI を自動配信。既定では JWT の role の権限を反映 | 公式ドキュメントで確認できず | 該当なし（HTTP ではない） | 【持つ】`GET /v1/openapi.json`（Bearer 必須）。ゴールデン `crates/wire-server/docs/openapi.json` との一致を検査（`nosql20_openapi_golden.rs` の `nosql20_openapi_golden_matches_generator`・`nosql20_openapi_endpoint.rs`。NOSQL-20） |
| DDL | 公式ドキュメントで確認できず（表は DB 側で作る前提） | 索引作成は `_bulk` の対象外 | `createCollection` と索引の節（Indexing Collections）あり | 【持つ】`create_table`・`alter_table`・`drop_table` と `create_index`・`drop_index`。実行できるのは `--ddl-allowed-users` に列挙したユーザーのみ（`nosql13_ddl.rs`・`nosql19_index_ddl.rs`） |
| 全件 DML の誤爆防止 | 公式ドキュメントの該当ページでは既定の挙動を確認できず（全表操作の遮断は別節の設定として参照されている） | 該当なし | 公式ドキュメントで確認できず | 【持つ】`update`・`delete` は `where` か `filter` が必須で、無ければ `42601`（`dml_target.rs`。`nosql18_conditional_dml.rs` の `condition_alone_without_target_is_rejected_with_42601`） |
| テナント境界・エラー契約 | DB の RLS・ロールに委ねる | 該当なし | 該当なし | 【持つ】RLS 相当のテナント分離と `wire_code` を SQL 表層と共有。不可視の他テナントの行の存在・件数は、対象操作（`search`・`scan`・書き込み・`if` の不一致応答）の応答に出ない。ただし従来方式の表では他テナントの Public 行は可視で、`scan` の `rows` とページ位置に含まれる（`nosql23_scan_keyset_rls_concurrency.rs` の `legacy_table_cross_tenant_public_rows_page_without_gaps_or_private_leak`）。Private 行は出ない（`rls13_nosql_policy_nonleak.rs`・`nosql21_upsert.rs` の `rls9_foreign_collisions_do_not_change_response_or_foreign_rows`） |

## 書き分け

- 同等以上（根拠テストあり）:
  - 条件付き更新・削除で存在を秘匿すること（`zero_row_responses_are_byte_identical_across_all_non_match_causes`）
  - キーセットページングの RLS・並行変更・SQL との並び一致（`nosql23_scan_keyset_rls_concurrency.rs`）
  - `returning` と `explain` が SQL 表層と一致すること（`nosql22_returning_exec.rs`・`nosql10_explain.rs`）
  - `Idempotency-Key` と台帳による再送の内容照合（`http14_idempotency_key.rs`）
  - スコープが権限を狭める一方であること（`http15_session_scope.rs`）
  - 集約と GROUP BY が既定で使えること（PostgREST は設定で有効化が要る）
- 差異・未提供:
  - bulk の項目別結果（`items[]` 相当）
  - `refresh` 相当の可視化指定
  - 要求をまたぐ明示トランザクション・savepoint
  - 全文検索・配列／範囲演算子
  - `upsert` での `returning`
- 計画中: 本書の作成時点で、本リポに open の Issue が確認できた項目は無い。そのため上記の未提供はすべて【持たない】とする

## 意図的に持たないもの

### bulk の項目別結果

fandhe の通常モードの書き込み文は 1 文 = 1 台帳エントリで、全か無かで適用する。`operation_id` の再送では、内容が一致すれば `23505`、
内容が違えば `22023` を返す契約である。項目別の部分成功を認めると、再送時の「適用済み」の判定と、内容照合ハッシュの単位が
崩れる。このため `rows` は 1 つの原子的な文として扱い、失敗は文全体の拒否として返す。
根拠は [multi-row-dml-operation-id.md](multi-row-dml-operation-id.md) の実行時の順序・原子性の節と、
`nosql-api.md` の `operation_id` の規則である。部分成功が要る用途は、利用者が行を分けた複数要求で表現する。

### 要求をまたぐ明示トランザクション

HTTP 表層の通常モードは 1 要求 = 1 原子実行で、トークンに Tx 状態を持たせない。未対応の形は黙って受理せず、op 語彙外として
fail-closed（`42601` 等）で拒否する。トランザクション制御が要る場合は SQL 表層を使う。

例外は `update`／`delete` の `mode: "partitioned"`（分割実行）である。チャンクごとに別々に commit するため、1 チャンク以上
commit した後に止まると、commit 済みの変更は残ったまま `409`・`VD001` を返す（全体の巻き戻しではない）。取り消し済みの
ジョブへの再送などは `VD002` である。同じ `operation_id` での再送で再開できる。失敗応答を全件ロールバックと読まないこと。
契約は `nosql-api.md` の分割実行の節と [partitioned-dml-comparison.md](partitioned-dml-comparison.md) を参照。

## 参照

確認日は 2026-11-05。

- PostgREST「Tables and Views」（<https://docs.postgrest.org/en/stable/references/api/tables_views.html>）
- PostgREST「Prefer」（<https://docs.postgrest.org/en/stable/references/api/preferences.html>）
- PostgREST「Pagination and Count」（<https://docs.postgrest.org/en/stable/references/api/pagination_count.html>）
- PostgREST「Aggregate Functions」（<https://docs.postgrest.org/en/stable/references/api/aggregate_functions.html>）
- PostgREST「OpenAPI」（<https://docs.postgrest.org/en/stable/references/api/openapi.html>）
- Elasticsearch「Bulk API」（<https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-bulk>）
- Elasticsearch「Optimistic concurrency control」（<https://www.elastic.co/docs/reference/elasticsearch/rest-apis/optimistic-concurrency-control>）
- MySQL X DevAPI User Guide「Collection CRUD Function Overview」（<https://dev.mysql.com/doc/x-devapi-userguide/en/collection-crud-function-overview.html>）
- MySQL X DevAPI User Guide「Transaction Handling」（<https://dev.mysql.com/doc/x-devapi-userguide/en/transaction-handling.html>）
- 取得できず確認していない点: PostgREST の実行計画ページ、X DevAPI の `find` の詳細ページ、Elasticsearch の Explain・`search_after`
- fandhe 側はファイル名・関数名・テスト名で参照する（行番号は書かない）
