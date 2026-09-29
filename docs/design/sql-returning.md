# `INSERT`／`UPDATE`／`DELETE`／UPSERT の `RETURNING` 句（Issue #873・#1182・SQL-21・TASK-193）

## 背景・スコープ

書き込み文（DML）が「その文で実際に変更した行」を結果セットとして返せる
ようにする。Issue #873 で行形 `INSERT`（単一行・複数行 `VALUES`。SQL-10・
SQL-16）と単一行・`id` 完全一致形 `DELETE`（SQL-18・TASK-191）に、Issue #1182 で
`UPDATE`（単一行・述語形。SQL-17・SQL-19）・述語形 `DELETE`（SQL-19）・UPSERT
（`INSERT ... ON CONFLICT`。SQL-20）に `RETURNING <投影>` を実行結線した。

## 構文

`USING OPERATION_ID '<id>'` 句の**直前**に `RETURNING <投影>` を 1 回だけ
置ける（`sql::allowlist::Parser::parse_returning_clause`）。

```text
INSERT INTO <table> (<col>[, <col>]*) VALUES (<lit>[, <lit>]*)[, (...)]*
  [RETURNING <投影>] USING OPERATION_ID '<id>' [;]

DELETE FROM <table> WHERE <id 完全一致または述語>
  [RETURNING <投影>] USING OPERATION_ID '<id>' [;]

UPDATE <table> SET <col> = <lit>[, ...] WHERE <id 完全一致または述語>
  [RETURNING <投影>] USING OPERATION_ID '<id>' [;]

INSERT INTO <table> (...) VALUES (...)[, ...] ON CONFLICT (...) DO NOTHING | DO UPDATE SET ...
  [RETURNING <投影>] USING OPERATION_ID '<id>' [;]
```

`UPDATE ... WHERE id = <n> RETURNING ...` は `RETURNING` の有無に関わらず単一行形へ
分類する（`sql::allowlist::Parser::parse_update_where` が `RETURNING` を終端として
扱う。分類が `RETURNING` の有無で変わると内容照合ハッシュが変わり再送判定が
崩れるため。`peek_single_row_delete_id` と同じ判定）。

`<投影>` は `*` または裸の列名リスト（疑似列 `id` を含む。`SELECT` の
`Projection::Columns`／`Projection::All` を再利用）。関数呼び出し項目
（`Projection::Items`）は構文段・束縛段の両方で多層防御として `42601`
拒否する（`RETURNING` が返す行は書き込み結果そのものであり、式評価用の
セッション UDF レジストリを経由する必要がないため）。

## 受理・拒否表

| 入力 | 結果 |
| ---- | ---- |
| `... RETURNING * USING OPERATION_ID '...'` | 受理 |
| `... RETURNING id, body USING OPERATION_ID '...'` | 受理 |
| `... USING OPERATION_ID '...' RETURNING id` | `42601`（`USING` 句より後ろ。余剰トークン） |
| `... RETURNING USING OPERATION_ID '...'`（投影なし） | `42601` |
| `... RETURNING id RETURNING body USING OPERATION_ID '...'`（重複） | `42601` |
| `... RETURNING vec_norm(embedding) USING OPERATION_ID '...'`（関数呼び出し項目） | `42601` |
| `TRUNCATE TABLE <table> ... RETURNING *` | `42601`（`TRUNCATE` は対象外） |
| ファイル形 `INSERT`（`path`/`body` 列指定）＋ `RETURNING` | `42601`（束縛段。サーバー側チャンク化行を返す応答形が未定義のため fail-closed） |
| 述語つき `DELETE ... WHERE <非 id 述語> RETURNING ...` | 受理（Issue #1182。削除前の値を返す） |
| 単一行・述語形いずれの `UPDATE ... RETURNING ...`（セッション経路） | 受理（Issue #1182。更新後の値を返す） |
| `INSERT ... ON CONFLICT ... RETURNING ...` | 受理（Issue #1182。挿入行・`DO UPDATE` 行のみ返し、`DO NOTHING` で衝突した行は返さない） |
| 未知列・式項目の `RETURNING`（全 DML） | 書き込み前に `22000`／`42601`（台帳は消費しない） |
| `EngineCore::execute_insert_sql`／`execute_insert_sql_batch`／`execute_delete_sql`／`execute_update_sql`（非セッション入口）＋ `RETURNING` | `42601`（検証直後・書き込み前。台帳は消費しない。`RETURNING` を黙って落とす fail-open を避ける） |
| 明示トランザクション内（`BEGIN ... COMMIT`）の `UPDATE`／`DELETE`／UPSERT（`RETURNING` の有無を問わず）・`RETURNING` 付き `INSERT` | `0A000`（従来どおり未対応。fail-closed） |

## 実行経路

- `INSERT`: `core.rs::EngineCore::execute_insert_returning_form` →
  `sql::parser::bind_returning`・`bind_insert_form` → 行形（単一行・複数行
  `VALUES`）は `sql::exec::execute_insert_returning` が既存の書き込み経路
  （`execute_insert_batch_with_schema`）をそのまま通したうえで、**書き込んだ
  値そのもの**（`BoundInsert::values`。既定値・トリガの類は存在しないため
  常に書き込んだ値と一致する）を再読み込みなしで投影する。
- `DELETE`（単一行）: `core.rs::EngineCore::execute_delete_returning_form` →
  `sql::exec::execute_delete_returning` → `tenant::
  delete_row_ledgered_capturing_unchecked`（`capture: Some(schema)`）が
  `row_table.remove` の**直前**・同一 write トランザクション内で対象行を
  完全デコードし `tenant::CapturedRow` として捕捉する（削除**前**の値）。
  物理行フォーマットは `storage.rs::decode_row`（`ROW_FORMAT_VERSION`）で
  あり、`row_codec::decode_row`（別バージョン・本モジュールの通常の書き込み
  経路では使われない別フォーマット）ではない点に注意（実装時に取り違えて
  デコード失敗を起こした反省点。`tenant.rs` のコメント参照）。`metadata`
  バイト列は `row_codec::decode_scalar_columns` で `schema.columns` 順の
  `Value` 列へ変換し（`VECTOR` 列位置は契約により常に `Value::Null`）、
  `VECTOR` 列位置だけを `Row::embedding` で明示的に差し替える
  （`tenant::captured_values_from_parts`。全 DML 経路で共有）。
- `UPDATE`（単一行）: `core.rs::execute_update_returning_form` →
  `sql::exec::execute_update_returning` → `tenant::
  update_row_columns_capturing_unchecked`。書いた値（更新後）そのものを捕捉し、
  制約検査の後・commit の前に投影する。不可視・不存在は捕捉も投影もせず
  `UPDATE 0`・空結果（他テナント行と不存在 id の応答差を作らない。RLS-9・RLS-10）。
- 述語形 `UPDATE`／`DELETE`: `execute_update_returning_form`／
  `execute_predicate_delete_returning_form` →
  `sql::exec::execute_predicate_update_returning`／`execute_predicate_delete_returning`
  → `tenant::update_rows_where_capturing_unchecked`／
  `delete_rows_where_capturing_unchecked`。行ごとにインラインで投影する
  （全件をバッファしない）。返却順は候補列挙順（テナント内 `id` 昇順）。
  述語評価は `sql::exec::run_where_predicate` を `RETURNING` の有無に関わらず共有し、
  内容照合ハッシュは `EngineCore::prepare_predicate_update`／
  `prepare_predicate_delete` の 1 箇所で `RETURNING` を入力に含めずに計算する。
- UPSERT: `execute_insert_returning_form` の `Upsert` 腕 →
  `sql::exec::execute_upsert_returning` → `tenant::
  upsert_typed_rows_capturing_unchecked`。新規挿入行は挿入した値、`DO UPDATE` 行は
  更新後の値を `VALUES` 記述順に返す。`DO NOTHING` で衝突した行は返さない。
  INDEX-4 バッチ上限は `validate_upsert_batch_limits` で `RETURNING` なしと共有。
- 投影コールバックは `sql::exec::returning_collector` が全経路で共有する
  （RLS 再判定・文全体で累計する結果バイト予算・行バッファの `try_reserve`）。
- `sql::returning` モジュールが投影（`column_meta`・`project_row`）を担う。
  `SELECT` の投影束縛規則（実カラム優先・疑似列 `id`）をそのまま再利用し、
  第 2 の投影実装を作らない。
- **commit 成功境界（codex-review P1 指摘・PR #991 対応）**: `DELETE` の
  `project_row`（文字列・ベクトルの `try_reserve_exact` 失敗や結果容量超過
  で失敗しうる）は、捕捉行の実体が write トランザクション内でしか得られない
  ため `column_meta` と違って書き込みより前には呼べない。旧実装は commit
  **後**に呼んでいたため、投影失敗時に「DELETE は失敗応答なのに行は既に
  永続化されている」という一貫性違反が起こり得た。修正後は `tenant::
  delete_row_impl` に `project` コールバックとして渡し、行削除・台帳追記と
  **同じ write トランザクション内・commit の直前**に呼ばせる——投影が
  失敗すれば `write_txn` は commit されず abort されるため、削除も台帳追記
  も一切永続化されない。`INSERT` は書き込み予定値が呼び出し前から既知の
  ため引き続き書き込みより前に投影する（対称ではない別経路）。

## RLS 再判定（多層防御）

返却行はいずれも書き込み経路（テナント名前空間キー `(tenant_id, id)`・
TABLE-12）由来のため通常は `ctx` から可視だが、投影の直前に
`PolicyContext::is_visible(row_tenant, row_visibility)` を再適用する
（RLS-7・RLS-8 と同じ判定。security.md「テナント境界」多層防御方針）。
不可視の場合は `result.rows` を空にするが、`rows_affected`（実際に変更した
行数）は変えない——`PolicyContext::new`（`Public` のみ可視）で `Private`
行（挿入行は常に `Private` 固定）を `RETURNING` した場合、
`INSERT 0 1` は返るが `DataRow` は 0 件になる。この非対称は意図した設計
判断であり、`crates/engine/tests/sql_returning.rs::
insert_returning_rows_affected_is_independent_of_result_row_visibility` が
固定する。

## `CommandComplete` タグと件数の独立性

`wire-server::simple_query` は `SqlOutcome::Returning` を受け取ると
`respond_rows_with_tag`（`respond_query_result` から切り出した共通本体）で
`RowDescription`→`DataRow`*→`CommandComplete` を送出する。タグの件数は
`result.rows.len()`（RLS 再判定後の投影行数）ではなく必ず
`outcome.rows_affected`（実際に変更した行数）を使う——`SELECT`/`EXPLAIN` の
`format!("{tag} {}", result.rows.len())` は再利用できない（両者が一致しない
ケースがあるため）。

| DML | タグ |
| --- | ---- |
| `INSERT`（UPSERT を含む。`<n>` は挿入行数＋更新行数） | `INSERT 0 <rows_affected>` |
| `UPDATE` | `UPDATE <rows_affected>` |
| `DELETE` | `DELETE <rows_affected>` |

## 内容照合ハッシュ非依存

台帳の内容照合ハッシュ（TASK-101・RECOVER-10。`recovery::content_hash`）は
SQL テキストではなく符号化済み行・`id` から計算するため、`RETURNING` の
有無は再送判定に一切影響しない。同一 `operation_id`・同一内容で
「`RETURNING` あり → なし」の順（逆順も）に送ると 2 回目は `23505`
（`crates/engine/tests/sql_returning.rs::
insert_returning_content_hash_is_independent_of_returning_clause` が固定）。
Issue #1182 で結線した単一行 `UPDATE`・述語形 `UPDATE`／`DELETE`・UPSERT も
同様（`dml_returning_content_hash_is_independent_of_returning_clause` が
「あり→なし」「なし→あり」の両順を固定）。

## 非セッション入口の拒否

`EngineCore::execute_insert_sql`／`execute_insert_sql_batch`／
`execute_delete_sql`（`SqlOutcome` を持たず戻り値型が固定の非セッション
API）は、`RETURNING` 付き文を検証直後・書き込みトランザクション開始前に
`42601` で拒否し、台帳を一切消費しない（同一 `operation_id` をその後
セッション経由・`RETURNING` なしで再送すると成功することをテストで固定）。
`RETURNING` はセッション経由の実行経路（`EngineCore::
execute_sql_in_session`）専用。

## 結果セット上限

`sql::returning::MAX_RETURNING_RESULT_BYTES`（`sql::scan::
MAX_SCAN_RESULT_BYTES`・`sql::exec::MAX_CANDIDATE_SCALAR_BYTES` と同じ
`crate::arena::MAX_ARENA_TOTAL_BYTES`）を、テキスト・ベクトル各セルの
複製バイト量の累計へ確保前に検証する。行数は既存の 1 文あたり行数上限
（`MAX_INSERT_ROWS_PER_STATEMENT`）・`batch_limits`（INDEX-4）で有界。

## 投影と制約検査の優先順位（Issue #1182）

述語形 `UPDATE`／`DELETE`・UPSERT の投影は行ごとにインラインで呼ぶため、
投影由来のエラー（`54000` 結果容量超過・`XX000`）が制約違反（`23505`・`23503` 等）
より先に報告されうる。いずれも write トランザクションの abort により行・台帳とも
永続化されない（副作用ゼロ）ため、クライアントに見える差は返却コードのみ。
単一行 `UPDATE` は制約検査の後に投影する（単一行 `DELETE` と同じ順序）。

## 対象外・申し送り

- **明示トランザクション内の DML `RETURNING`**: `execute_in_active_txn` は
  `UPDATE`／`DELETE`／UPSERT を従来どおり未対応（`0A000`）として拒否する。
  トランザクション内の DML を追加する際（Issue #1179 系）は、`INSERT` 腕と同様に
  `returning.is_none()` ガードを持たせ、`RETURNING` を黙って落とさないこと。
- **ファイル形 `INSERT`**: 従来どおり `42601`（サーバー側チャンク化行を返す応答形が未定義）。
- **NoSQL 表層**: `op: insert`／`update`／`delete`／`search`／`scan`／`aggregate` の
  いずれにも `returning` キーは無い（`http/query/update.rs`・`delete.rs` は
  `returning: None` 固定）。spec 側の規範化待ち。
- **3 クライアント層 B の実測実行**: `make e2e-three-client` は運用者作業。
