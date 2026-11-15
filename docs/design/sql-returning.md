# `INSERT`／`UPDATE`／`DELETE`／UPSERT の `RETURNING` 句（Issue #873・#1182・SQL-21・TASK-193）

> **改訂注記（Issue #1251・2026-09-30）**: 「RLS 再判定」「`CommandComplete` タグの件数」の
> 2 節と UPSERT の不可視衝突の扱いを改訂した（親 Issue #1250）。実装追随は #1252
> （`INSERT`／UPSERT 新規行）・#1253（`UPDATE`／`DELETE` の対象選定）・#1254（UPSERT の
> 衝突先）・#1255／#1256（テスト）。#1253（`UPDATE`／`DELETE` の対象選定）・#1252（`INSERT`／UPSERT
> 新規行の不可視は `XX000`）は実装済み。#1254 のマージまでコードは旧挙動であり、本書の新方針が
> 実装の目標仕様となる。

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

`<投影>` は `*`・列名リスト（疑似列 `id` を含む）・`AS` 別名付き項目・スカラー式／組み込み関数
（`SELECT` の `Projection::Columns`／`Projection::All`／`Projection::Items` を再利用。Issue #3249 で
式・組み込み関数まで受理）。スカラーサブクエリ・集計関数は構文段・束縛段の両方で拒否する。
UDF の呼び出しは、セッションの UDF レジストリで解決する（Issue #3250。種類ごとの扱いは
「UDF の評価」節）。

## 受理・拒否表

| 入力 | 結果 |
| ---- | ---- |
| `... RETURNING * USING OPERATION_ID '...'` | 受理 |
| `... RETURNING id, body USING OPERATION_ID '...'` | 受理 |
| `... USING OPERATION_ID '...' RETURNING id` | `42601`（`USING` 句より後ろ。余剰トークン） |
| `... RETURNING USING OPERATION_ID '...'`（投影なし） | `42601` |
| `... RETURNING id RETURNING body USING OPERATION_ID '...'`（重複） | `42601` |
| `... RETURNING id AS rid, lang AS l USING OPERATION_ID '...'`（`AS` 別名付きの列参照。Issue #3248） | 受理（列名だけが別名になり、値・型は別名なしと同じ。別名の重複も受理） |
| `... RETURNING vec_norm(embedding), id + 1 AS m USING OPERATION_ID '...'`（組み込み関数・スカラー式。Issue #3249） | 受理（結果の型・値・列名は同じ式の `SELECT` と一致する） |
| `... RETURNING shout(lang) ...`（宣言的 UDF・WASM UDF。Issue #3250） | 受理（型・値・列名は同じ式の `SELECT` と一致する） |
| `... RETURNING w(lang) ...`（`LANGUAGE sql｜plpgsql` 関数。書き込みを試みうる） | `0A000`（書き込み前。行・台帳を残さない。意図的な拒否） |
| `... RETURNING my_udf(lang) ...`（未定義の関数） | `42883` |
| `... RETURNING (SELECT ...) ...`・式の内側のサブクエリ・集計関数 | `42601`（他表を読む経路を作らない） |
| `... RETURNING lang + 1 ...`（式の型エラー） | 書き込み前に `42883` 等（台帳は消費しない） |
| 式の評価時エラー（`1 / 0` 等） | `22012` 等（commit 前に書き込みを中止。行・台帳は残らない。`TenantWriteError::ReturningExpressionFailed`） |
| 採番列・`id` 省略の INSERT ＋ 式項目の `RETURNING` | `0A000`（書き込み前。仮値での評価と commit 後の失敗を避けるため fail-closed。列参照・別名だけの `RETURNING` は従来どおり受理） |
| `... RETURNING lang l ...`（`AS` を省いた別名） | `42601` |
| `... RETURNING *, lang AS l ...`（`*` と他の項目の併用） | `0A000`（Issue #3248 の対象外。`*` の展開と個別項目の併用は出力列の重複・順序の扱いを別途決める必要があり、現行は fail-closed で拒否したまま据え置く（対応しない。需要が出た時点でユーザー承認のうえ起票する）） |
| `TRUNCATE TABLE <table> ... RETURNING *` | `42601`（`TRUNCATE` は対象外） |
| ファイル形 `INSERT`（`path`/`body` 列指定）＋ `RETURNING` | `42601`（束縛段。サーバー側チャンク化行を返す応答形が未定義のため fail-closed） |
| 述語つき `DELETE ... WHERE <非 id 述語> RETURNING ...` | 受理（Issue #1182。削除前の値を返す） |
| 単一行・述語形いずれの `UPDATE ... RETURNING ...`（セッション経路） | 受理（Issue #1182。更新後の値を返す） |
| `INSERT ... ON CONFLICT ... RETURNING ...` | 受理（Issue #1182。挿入行・`DO UPDATE` 行のみ返し、`DO NOTHING` で衝突した行は返さない。所有だが不可視な衝突先は `DO NOTHING` ではスキップ、`DO UPDATE` では `42501`。下記「UPSERT の衝突先が不可視の場合」節） |
| 未知列・未定義関数の `RETURNING`（全 DML） | 書き込み前に `42703`／`42883`（台帳は消費しない） |
| `EngineCore::execute_insert_sql`／`execute_insert_sql_batch`／`execute_delete_sql`／`execute_update_sql`（非セッション入口）＋ `RETURNING` | `42601`（検証直後・書き込み前。台帳は消費しない。`RETURNING` を黙って落とす fail-open を避ける） |
| 明示トランザクション内（`BEGIN ... COMMIT`）の単一行・述語形 `UPDATE ... RETURNING`、述語形 `DELETE ... RETURNING`、UPSERT の `RETURNING` | 受理（Issue #1272・#1273。autocommit と同じ契約。詳細は `explicit-transaction.md`） |

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
  所有だが不可視な衝突先は `DO NOTHING` ではスキップ、`DO UPDATE` では `42501`
  （下記「UPSERT の衝突先が不可視の場合」節）。
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

## 対象選定・RLS 再判定と不変条件

### 書き込み対象の選定スコープ

`UPDATE`（`id` 指定・述語形）・`DELETE`（`id` 指定・述語形）・UPSERT の既存行は、
書き込み対象を次の**両方**を満たす行に絞る（RLS-10・SQL-21 と整合）。

- **所有**: 物理キー `(tenant_id, 0)..=(tenant_id, u64::MAX)` の範囲（TABLE-12。
  `is_owner` の二重防御）
- **可視**: `PolicyContext::is_visible`（RLS-7・RLS-8 と同じ判定）

「可視」単独ではなく「所有 ∩ 可視」である。他テナントの `Public` 行は可視だが
所有ではないため、従来どおり対象外となる（越境書き込みを認めない）。他テナント行は
物理キーの範囲走査で構造的に除外され、取得すらされない（RLS-9）。

- 可視性の判定は**ヘッダ専用デコード**（`storage::decode_row_tenant_and_visibility`）で
  先に行い、所有かつ可視の行だけを本体デコード・述語評価の対象にする
  （`docs/design/update-single-row.md`「判断 D 再改訂」と同じ設計）。不可視行の
  本体破損が `XX000` として観測され、存在オラクルになることを防ぐ。
  この「本体を読まない」契約は**対象選定**（述語評価・`RETURNING` 投影・書き込み対象
  の確定）の範囲に限る。制約検査の母集合（下記「据え置き」）と UPSERT の
  `UpsertTarget::Unique` 衝突表の事前走査（`scan_tenant_rows_by_unique_key`）は、衝突検出に
  必要な**キー列だけ**を所有行全体から読む（可視性を問わない）。**可視**行のキー列が
  破損していた場合は fail-closed で `XX000` とする。**不可視**行（ヘッダ不整合を含む）の
  キー列が破損していても、事前走査の時点ではエラーとして応答へ現さず、キーを復元できない
  行として記録する。記録した行の一意キーは永続索引の逆引きから補完し
  （`constraint::recover_unique_keys_of_corrupt_rows`）、健全な不可視行と同じ
  「所有だが不可視」の衝突先として扱う（破損状態が不可視行の観測オラクルになるのを
  防ぐ）。補完後のキーが健全行のキーと別の行 id で重複した場合は内部矛盾として
  `XX000` とする。索引が未構築・逆引きが無いなどでキーを補完できない行が 1 件でも
  残る場合は、対象キーに依らず一様に `XX000` で拒否する（キーが不明なため健全な
  不可視行の応答を再現できず、キー値ごとに応答が変わって破損状態が判別されるのを
  避ける fail-closed）。いずれの `XX000` も自テナント所有行の内部状態に由来し、
  他テナントの存在・内容は含まない。自テナントの不可視行の存在は、制約検査が
  従来から `23505` で観測させる範囲（RLS-10 の制約検査）に含まれ、新たな存在オラクルは
  生まない。キー列以外の本体は、衝突先が可視と判明した後にのみ読む。
- 総走査上限（`tenant::MAX_SCANNED_ROWS`）の加算は従来どおり対象テナント所有行の
  走査ごとに行い、可視性で数え方を変えない（他テナントのデータ量に依存しない性質の維持）。
- `id` 指定 `UPDATE` は既にこのスコープである。`id` 指定 `DELETE`・述語形
  `UPDATE`／`DELETE` をこれに揃える（実装は #1253）。
- **据え置き**: `TRUNCATE`（SQL-22）の削除母集合と、UNIQUE／PRIMARY KEY／FOREIGN KEY
  等の制約検査の母集合（RLS-10 の制約検査・TABLE-16・TABLE-17）は、所有行全体のまま
  変えない。母集合を可視集合へ狭めると、不可視行との一意性衝突を見逃すため。

### 新規挿入行の返却

`INSERT`・UPSERT の新規行は、書き込み経路（`(ctx.tenant_id(), id)` キー・`is_owner`
検査済み）由来の「文が挿入した行そのもの」であり、他テナント名義の行の挿入は
`is_owner` 検査（`42501`）で書き込み前に拒否されるため、返却行に他テナント行は
構造的に入らない。ただし `is_owner`（書き込み権限）は読み取り時の `is_visible` の
代わりにならない。`RETURNING` は読み取り経路でもあるため、**返却にも可視性の検査を
維持する**（RLS の可視性を迂回する例外は設けない）。

- 投影直前に `PolicyContext::is_visible` を適用する。可視なら返す。
- 不可視（`PolicyContext::new` 等で `Private` 行が不可視なコンテキストが
  `Private` 行を挿入した場合）は、通常の読み取りで見えない行の値を返さず、かつ
  黙って落とさず（影響行数との不一致を避ける）、内部エラー（`XX000`）で write
  トランザクションを abort する（行・台帳とも永続化しない。fail-closed）。
  wire／HTTP の認証経路は所有集合 ⊆ 可視集合（RLS-11）のため通常は発生しない。

### 投影直前の RLS 再判定（`sql::exec::returning_collector`）

`INSERT`・UPSERT（新規行・既存行）・`UPDATE`・`DELETE` のすべてで、投影直前の
`PolicyContext::is_visible` 再適用を**不変条件の検査**として残す（多層防御。
security.md「テナント境界」）。対象選定または書き込みを通過した行が再判定で不可視だった
場合は、行を黙って落とさず内部エラー（`XX000`）で write トランザクションを abort する
（行・台帳とも永続化しない）。黙って落とすと影響行数と返却行数が食い違い、#1250 の
不一致が再発するため。この再判定を所有検査に置き換えない。

### 不変条件

- (a) すべての DML（`INSERT`・UPSERT・`UPDATE`〔`id` 指定・述語形〕・`DELETE`〔`id`
  指定・述語形〕）で、`RETURNING` の `DataRow` 数と `rows_affected` は一致する。可視集合が
  `Public` のみでも `Public`＋`Private` でも成立し、`RETURNING` を返す全経路に適用する。
- (b) 他テナント行は候補集合・返却行・影響行数のいずれにも現れない（混入 0 件。
  RLS-7〜10）。
- (c) `TRUNCATE` の削除母集合（SQL-22）と制約検査の母集合（TABLE-16・TABLE-17・RLS-10 の
  制約検査）は所有行全体のまま不変。
- (d) wire／HTTP の認証経路は RLS-11 により「所有集合 ⊆ 可視集合」（自テナント所有行は
  常に可視。可視集合は他テナントの `Public` 行を含みうるため一致はしない）が成り立つ。
  書き込み対象は「所有 ∩ 可視」＝所有集合となり（他テナントの `Public` 行は書き込み時の
  所有検査で従来どおり除外される）、外部挙動は変わらない。差が観測されるのは所有行が
  不可視になりうる engine 直呼び出し（`PolicyContext::new` 等）のみであり、
  #1253・#1254 を BREAKING CHANGE とする理由である。この場合でも `RETURNING` は
  不可視行の値を返さず、`XX000` で abort する（上記「新規挿入行の返却」）。
- (e) `CommandComplete` タグは引き続き `rows_affected` を使う（(a) により
  `result.rows.len()` と一致する）。

固定するテストは #1255 の全経路テストと、#1252／#1253 で更新される既存テストである。

## UPSERT の衝突先が不可視の場合

本構文は `ON CONFLICT` の対象（`(id)` または `UpsertTarget::Unique`）で**検出した衝突**
について `23505` を出さない契約（`docs/design/sql-upsert.md`）のため、検出した衝突先が
不可視でも不存在扱いして新規挿入へ進む案（物理キー衝突で `23505` になる）は採れない。
なお UNIQUE キー不一致のまま挿入予定 `id` が既存行と重なる場合は、`sql-upsert.md` の
とおり従来どおり `23505` であり、本節の対象外（この契約は変えない）。
衝突の**検出**は物理キー（`UpsertTarget::Unique` では UNIQUE 列の事前走査）の所有
スコープのまま行い、検出した既存行の可視性で次のとおり分岐する。

| アクション | 衝突先が所有かつ可視 | 衝突先が所有だが不可視 |
| ---------- | -------------------- | ---------------------- |
| `DO NOTHING` | 変更なし（影響・返却とも数えない） | スキップ（書き込まない・影響行数に数えない・返却しない）。可視時と同じ応答形 |
| `DO UPDATE` | read-merge-write で更新し、更新後の値を返却 | 文全体を `42501` で拒否 |

- `DO UPDATE` の拒否は write トランザクションを abort し、全 `VALUES` 行・台帳とも
  副作用ゼロ（RECOVER-11）。台帳エントリは commit されないため、同一 `operation_id` は
  再利用できる（制約違反時と同じ扱い）。
- 根拠: 単一行 `UPDATE ... WHERE id = <n>` は不可視が `UPDATE 0` となり不存在と区別
  できないが、UPSERT には不存在と同形の結果がなく、どの選択肢でも自テナント内の存在は
  観測されうる。漏れる範囲は自テナント内（TABLE-12 のキー名前空間）に限られ、他テナント行
  はキーが異なり取得すらされない（RLS-9 の性質は不変）。そのうえで `DO UPDATE` の書き込み
  意図を黙って捨てる fail-open を避け、拒否側（fail-closed）に倒す。
- エラー形: `wire_code` は `42501`、エラー文言は固定の英語文字列で、対象行の
  `id`・列値を含めない。Rust 上は専用 variant（`TenantWriteError::ConflictTargetNotVisible`・
  `SqlSurfaceError::ConflictTargetNotVisible`）とし、分類は既存の
  `FORBIDDEN_TENANT_MISMATCH`（`ErrorClass` は新設しない）へ写像する。`Forbidden` の流用は
  SQL 経路で `XX000` へ落ちるため採らない（#1254 で実装済み）。
- 判定順序（#1254 向け）: ヘッダ専用デコードで所有かつ可視を先に確定し、可視な行だけ
  本体デコードする（上記「判断 D」と同じ設計）。

## `CommandComplete` タグの件数

`wire-server::simple_query` は `SqlOutcome::Returning` を受け取ると
`respond_rows_with_tag`（`respond_query_result` から切り出した共通本体）で
`RowDescription`→`DataRow`*→`CommandComplete` を送出する。タグの件数は
`outcome.rows_affected` を使う。不変条件 (a) により `result.rows.len()` と一致する
が、DML の件数の正本は書き込み経路の `rows_affected` であるため、`SELECT`/`EXPLAIN` の
`format!("{tag} {}", result.rows.len())` は再利用しない。

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

## FROM／USING つき UPDATE／DELETE の RETURNING（Issue #1756・SQL-53・RLS-10 のポインタ）

`sql::dml_from_returning` が束縛・投影・収集を担う（構文本体 #1754・結合実行 #1755 の結果を受ける）。

| 項目 | 方針 |
| ---- | ---- |
| 名前解決 | `sql::relation::BindingScope`（`relations[0]` = 対象表、以降 FROM／USING の宣言順）を再利用。第 2 の解決器は作らない |
| 受理する形 | `*`・`<公開名>.*`・列参照（修飾あり・なし）の並び。式・関数呼び出し・`AS` 別名は従来どおり `42601` |
| 裸の `*` | 対象表の展開に続けて FROM／USING の各表を宣言順に並べる（JOIN の `SELECT *` と同規則。採番方式の表は疑似列 `id` なし）。PG 17.11 の `RETURNING *`（UPDATE・DELETE とも対象表 → FROM／USING の全列）と列順が一致することを Issue #3204 で照合した |
| 出力列名・型 | 修飾つきでも列名のみ。型は `ColumnMeta` |
| 対象表の列 | `returning::project_row` へ委譲。対象表の列だけを参照する場合は従来と同じ列・行（単体テストで完全一致を固定） |
| RLS | 対象行は収集時に `PolicyContext::is_visible` で再判定。FROM／USING 側の各表は呼び出し側が書き込むセッションの `ctx` で独立に走査する（システム文脈を使わない）。結合の上限は可視行だけで数える |
| 予算 | FROM／USING 側セルも確保前に `MAX_RETURNING_RESULT_BYTES` で検証（`54000`）。範囲外参照・一致行欠落は `XX000` |

**結線（Issue #3204）**: 構文段（`parse_dml_from_returning_clause`）→ 束縛（`sql::dml_from`。SET の束縛の後に同じスコープで束縛し、
`42702`／`42P01`／`42703` は変換器の `0A000` より先に返る）→ 計画（`sql::join::dml_plan`）→ 実行（`core::dml_join_exec`）の順に接続した。

- **source のセル位置**: 計画が `RETURNING` の参照列を走査の投影（`JoinedDmlSource::columns`）の末尾へ追記し（SET の位置は動かさない）、
  疑似列 `id` が参照されるときは `include_pseudo_id` で取り出す。セル位置は `JoinedDmlPlan::source_cell_position` に一本化し、
  結合キー・SET・`RETURNING` が同じ判定を共有する。位置が未確定・範囲外なら `XX000`（副作用ゼロ）。
- **一致行の引き当て**: probe の走査結果と一致記録（対象行 id → 走査位置）を共有ハンドルで収集側へ渡す。述語クロージャ（書き込み）と
  収集クロージャ（読み取り）は書き込み経路の「候補列挙 → 適用」の順で重ならず、借用の衝突は panic せず `XX000` にする。
  返す source 行は SET の評価に使った行と同じ（多重一致は走査順の先頭）。
- **値の意味論**: UPDATE は更新後の対象値と一致した source 行、DELETE は削除前の対象値。source の列型は SET と違い制限しない（セルの複製のため）。
- **応答**: `SqlOutcome::Returning`。`rows_affected` は書き込み層の実変更行数で、RLS 再判定で返却行数が減っても独立。Describe は同じ列を返す。
- **PG 17.11 との照合**: 非修飾の両表共通列は `42702`、`<公開名>.*` は該当表の全列、`AS` 別名は本リポでは `42601`（受理範囲を広げない）。
- **併用**: `RETURNING ... USING OPERATION_ID` は台帳結線済み（#3203）で受理する。
- **まだ拒否する形**: トリガのある対象表は `0A000`（単一表の `RETURNING` は Issue #3192 で発火するが、結合形は通過証を渡さない）。
  FROM／USING が 2 表以上は変換器の `0A000`、式・関数・別名は `42601`（Phase 3）。FROM 側表の `SELECT` 権限が無いロールは `42501`（結合テストで固定）。

## 対象外・申し送り

- **ファイル形 `INSERT`**: 従来どおり `42601`（サーバー側チャンク化行を返す応答形が未定義）。
- **NoSQL 表層**: `op: insert`／`update`／`delete` の任意キー `returning`（NOSQL-22）は、Issue #2090 で
  スキーマと応答本文、Issue #2091 で実行を結線した。SQL 表層と同じ実行器
  （`EngineCore::execute_bound_*_returning_in_session`）を通すため、返却行の内容・件数は SQL の
  `RETURNING` と一致する。HTTP の束縛済み入口は返却列・SET 列を権限検査へ渡せず全列要求になるため、
  列権限は SQL より厳しい側（fail-closed）に倒れる（精密化は #3003 の申し送り）。`search`／`scan`／
  `aggregate` に `returning` は無い。`upsert` の `returning` は実行器（#2087）の結線後。
- **3 クライアント層 B**: Issue #1347 で `three_client_e2e.rs::three_clients_receive_returning_rows_and_command_tags` を追加済み（`make e2e-three-client`・opt-in。詳細は `three-client-e2e-harness.md`「RETURNING」節）。拡張クエリ・明示トランザクション内は対象外。

## トリガ付き表の RETURNING（Issue #3192・TABLE-45・SQL-21 のポインタ）

INSERT／UPDATE／DELETE の該当イベントにトリガのある表でも、`RETURNING` 付きの文はトリガを発火する。

- **返る行**: 投影は txn 内・commit 前に、トリガ適用段の行ごとループが書き込み直後に呼ぶ。INSERT・UPDATE は BEFORE の連鎖後・
  NOT NULL 検査後・行書き込み後の最終 NEW、DELETE は BEFORE を通って実際に削除した行の削除前の値を返す。BEFORE が `NULL` を返して
  スキップした行は影響行数にも返却にも出ない。AFTER 系（ROW・STATEMENT）は返却へ影響しない。
- **投影の実体は 1 つ**: 既存の `returning_collector`（RLS 再判定・文全体のバイト予算・`try_reserve`）をそのまま使い、第 2 の
  投影実装を作らない。投影の失敗は write txn の abort で、行・台帳・トリガが書いた行は残らない。
- **INSERT の 2 経路**: トリガの無い表は従来どおり書き込み前に束縛値を投影する（バイト不変）。トリガ経路は
  `InsertWriteReport::trigger_path` が真のとき、txn 内コールバックで集めた行を採用する（事前投影は fail-fast の予算検査として残す。
  採番値は `final_row` に入っているため書き込み後の再投影は行わず、採番の受け渡し値だけ排出する）。
- **テナント境界**: トリガの分類は書き込みテナント自身の定義だけを見る。不在・他テナント・不可視の id は空の候補で同じ経路を通り、
  応答に存在差を出さない（RLS-9・RLS-10）。
- **範囲外（`0A000` のまま・台帳を消費しない）**: UPSERT、結合形（`UPDATE ... FROM`／`DELETE ... USING`）の `RETURNING`、
  分割実行 DML、COPY、NoSQL、生の行 API、トリガ本体内の `RETURNING`。
- **テスト**: `core/trigger_returning_tests.rs`、`tests/table_rls/table45_trigger_fire.rs`、
  `crates/wire-server/tests/wire_sql/wire_trigger_fire.rs`（末尾 `;` の単独送信・拡張クエリ）。

## `AS` 別名の受理（Issue #3248・SQL-21・SQL-42 のポインタ）

- 基表への INSERT・UPDATE・DELETE・UPSERT の `RETURNING` は、裸の列参照（疑似列 `id` を含む）と
  `AS <別名>` 付きの列参照を受理する。束縛は SELECT と同じ `resolve_projected_column` を共有し
  （第 2 の投影実装を作らない）、`id AS rid` は `ProjectedColumn::AliasedId`、実列は別名を列名にした
  `Column` へ解決する。別名は出力列名を替えるだけで、読む列・行・RLS の再判定・列権限は別名なしと同じ。
- 構文段（`Parser::parse_returning_clause`）と束縛段（`bind_returning`）は判定関数
  `returning_item_rejection` を共有する（Issue #3249 で式を受理。サブクエリ・集計だけを拒否）。
- 旧文言「function-call items」は誤解を招くため廃止し、原因別の文言へ分けた。
- 未対応のまま残る面（受け皿 #3251）: データ変更 CTE（`0A000`）・ビューへの書き込み（`42601`）・
  `UPDATE ... FROM`／`DELETE ... USING`（`42601`）・NoSQL への展開。

## 式・組み込み関数の評価（Issue #3249・SQL-21 のポインタ）

- 束縛は `bind_returning` が SELECT と同じ `bind_projection_with_literals`（UDF レジストリは Issue #3250 でセッションのものを渡す。
  ハイブリッド詳細不可）へ委譲する。結果の型（`result_ty`）・既定の列名は SELECT と同じ規則になる。
- 評価は `sql::returning::ReturningProjector` が行う。式は文ごとに 1 回だけコンパイルし
  （`ExprProgram`）、行ごとには評価スタックを使い回す。セルへの写像とバイト予算は
  `sql::scan::computed_value_to_cell`（上限は `MAX_RETURNING_RESULT_BYTES`）を SELECT と共有する。
- UPDATE・DELETE・UPSERT・トリガ経路では、投影は write トランザクション内・commit 前に行う
  （`returning_collector`）。評価時エラーは `SqlSurfaceError` のまま `ReturningExpressionFailed` で
  透過し、書き込みを abort する（`Internal` は従来どおり `XX000`）。RLS の再判定は式の評価より前に行う。
- トリガの無い INSERT は書き込み前の事前投影で式を評価し、その行をそのまま返す（commit 後に式を
  評価しない）。採番列・`id` 省略の INSERT は払い出し値が commit 後にしか確定しないため、式項目と
  の組み合わせは書き込み前に `0A000` で拒否する（払い出し値を使った in-txn 投影への拡張は未対応）。
- 列権限は、RETURNING が読む列（式の中の列識別子を含む）を SELECT 権限で判定する。
- 未対応のまま残る面: データ変更 CTE・ビューへの書き込み・
  `UPDATE ... FROM`／`DELETE ... USING`・NoSQL への展開（#3251）、`RETURNING` 内の `$n`（位置判定で拒否）。

## UDF の評価（Issue #3250・SQL-21 のポインタ）

- 解決元: SQL 表層の DML 実行・Describe・EXPLAIN は、セッションの UDF レジストリで RETURNING を
  束縛する（`sql::parser::bind_returning_with_udfs`）。レジストリは認証済みテナントのセッションの
  ものだけで、他テナントの同名関数は見えず `42883` になる。UDF を解決しない経路（NoSQL・データ変更
  CTE・ビュー書き込み。#3251）は従来の `bind_returning`（空のレジストリ＝未登録扱いの `42883`）のまま
  で、fail-closed を保つ。
- 種類ごとの扱い（いずれも書き込み前に決まり、台帳を消費しない）:
  - 宣言的 UDF: 束縛時にインライン展開し、既存の式評価で評価する。
  - WASM UDF: `WasmCall` として評価する。サンドボックスの上限・トラップの写像は `SELECT` と同じ。
  - `LANGUAGE sql｜plpgsql` 関数: 揮発性によらず `0A000`。
- 書き込み試行の拒否: RETURNING は write トランザクション内・commit 前に行ごとに投影する。そこで
  ルーチン本体（DML を実行できる）を再入実行すると、書き込みの入れ子・トリガとの順序・台帳の一貫性
  が未定義になるため、ルーチンは束縛段で拒否する。宣言的 UDF と WASM UDF は構造上書き込めない。
  読み取り専用ルーチンを RETURNING で起動できるようにする拡張は未対応（受け皿の Issue は未起票）。
- 権限: 関数の EXECUTE 権限は呼び出し元ロールで判定する（`42501`）。引数に書いた列は SELECT 列権限で
  判定する。RLS の再判定は式の評価より前のままで、式は可視の確定行にしか届かない。
- 評価時エラー（例: 0 除算の `22012`）は commit 前に abort し、行も台帳も残さない。
- 採番列・`id` 省略の INSERT と式項目（UDF を含む）の組み合わせは従来どおり `0A000`。
