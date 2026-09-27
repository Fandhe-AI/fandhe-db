# `CHECK` 制約（TABLE-16・TASK-204、Issue #906）

## 概要

`CREATE TABLE` に列制約・表制約として `CHECK` を宣言し、すべての書き込み経路
（SQL 表層・明示トランザクション・COPY・NoSQL・ファイル形 INSERT・Rust API）で
書き込み時に検査する。違反行を含む文全体を `23514` で副作用ゼロのまま拒否する
（`ErrorClass::CheckViolation`）。`PRIMARY KEY`（#903）・NOT NULL/DEFAULT（#904）・
UNIQUE（#905）の機構（`constraint` モジュールの単一検査点・カタログの版管理）の
上へ配線している。

## 受理する構文

```text
CREATE TABLE <table> (
  <col> <type> [CONSTRAINT <name>] CHECK (<述語>),  -- 列制約
  ...
  [CONSTRAINT <name>] CHECK (<述語>)                -- 表制約
)
```

`CHECK`／`CONSTRAINT` は `sql::lexer::Keyword` へ含めず、`CREATE`／`SET` 等と
同じ方針で構文上のこの位置のみ文脈的キーワードとして照合する
（`sql::allowlist::Parser::peek_check_clause_start`）。

- 列制約 `CHECK` は他の列制約（`NOT NULL`／`DEFAULT`／`UNIQUE`／`PRIMARY KEY`）の
  後ろに 0 個以上置ける。表制約は列リスト中の任意位置に置ける（列を追加しない
  ため、列数上限〔256〕の位置非依存な判定には影響しない）。
- 件数上限 `MAX_CHECK_CONSTRAINTS_PER_TABLE`（32）超過は `Vec` へ積む前に `54000`。

**曖昧さの排除（PR レビュー指摘 Medium の是正）**: `CREATE TABLE` の列名
`check`／`constraint` は予約語として `42601` で拒否する（PostgreSQL でも両語は
予約語。`ALTER TABLE ADD COLUMN` の予約列名も同じく揃える）。これにより列リスト要素先頭の `CHECK (`／`CONSTRAINT` は常に制約宣言の
開始として扱える。加えて、制約名が列型キーワード（`TEXT`／`VECTOR`。
`CREATE_TABLE_COLUMN_TYPE_KEYWORDS`）と一致する場合も `42601` とする。
`CREATE TABLE t (constraint TEXT CHECK (body = 'a'), body TEXT)` のように
「列 `constraint` の定義」とも「制約名 `TEXT` の表制約」とも読める入力は、
どちらの解釈でも黙って受理されず必ず拒否される（旧実装は後者として受理し、
列 `constraint` を黙って消した別スキーマを作っていた）。

## D1. 述語文法 = `WHERE` 述語文法（AND 連結）

`CHECK` の本体は `sql::allowlist::Parser::parse_check_body`（`parse_where` と
同一の文法。唯一の違いは `)` を境界トークンとして扱う点）で解析する。束縛は
`sql::parser::bind_check_predicates`（`sql::parser::bind_where_predicates` と
束縛経路〔`bind_where_predicates_recursive`〕を共有する CHECK 専用の薄い
ラッパー。空の `UdfRegistry` で呼び、セッションの UDF を解決させない）で行う
ため、評価器は完全に既存のものを再利用する（第 2 の実装を作らない）。

**数値列の式参照（Issue #1075・TABLE-16 ポインタ）**: `sql::udf_call::
bind_expr_in` の `Expr::Ident` 分岐は、束縛環境の列参照ポリシー
（`ColumnRefPolicy`）に応じて INTEGER/BIGINT/REAL/DOUBLE 列の式内参照を許可
する。`WHERE`／`SELECT`／Describe 系の汎用式束縛（`bind_expr`。常に
`ColumnRefPolicy::IdAndVectorOnly`）は挙動を変えず、`CHECK` の式述語束縛
（`bind_check_predicates`。`ColumnRefPolicy::AllowNumericColumns`）だけが
数値列の算術・比較（`CHECK (qty > 0)` 等）を受理する opt-in 拡張であり、
汎用の `WHERE`／`SELECT` へ数値列の式参照を広げるレーン A は依然として
別 Issue の対象（未起票）。TEXT/BOOLEAN/DATE/NUMERIC 等の他の列型は
ポリシーに関わらず引き続き拒否される。CREATE TABLE の SQL 表層は現状
TEXT／VECTOR／INTEGER／BIGINT 列のみ宣言できる（REAL/DOUBLE 列は Rust API
経由に限る）。

**禁止要素**（`sql::check_constraint::reject_forbidden_elements`。`42601`）:

- `visible()`（`WherePredicate::PredicateCall`）: RLS 文脈に依存し、CHECK の
  参照可能範囲（テーブル列と `id` のみ）を破るため。
- 組み込み関数以外の `Expr::Call`（セッション UDF・WASM UDF・未知関数）:
  空の `UdfRegistry` で束縛すると「未知の関数」（`22000`）へ丸まってしまい
  CHECK の禁止要素として区別できないため、束縛より前に検出する
  （`sql::udf_call::is_builtin_function_name`）。

## D1. 三値論理（SQL 標準・PostgreSQL 準拠）

CHECK が違反になるのは述語が FALSE のときだけで、UNKNOWN（NULL）は通過させる
（AND 連結のため「いずれかの連言が FALSE なら違反」と同値）。

- 宣言的フィルタ: 参照列の値が `None`（NULL）なら UNKNOWN としてスキップし、
  非 NULL で `matches() == false` のときのみ違反とする（`matches_all` は NULL
  を不一致扱いするため使わず、`MetadataFilter::matches` を連言ごとに直接
  呼ぶ）。
- 式フィルタ: `VECTOR` 列を参照する式は、その行の embedding が NULL
  （`dim == 0`）なら UNKNOWN としてスキップする（`sql/scan.rs` の `WHERE` 式
  評価と同じ判断。現状 CREATE TABLE の `VECTOR` 列は常に non-null のため
  実際には到達しないが、将来の拡張に備えた防御的処理）。

## D1'. 数値列比較の評価規則（Issue #1075・TABLE-16 ポインタ）

- **NULL**: 参照している数値列のいずれかが NULL なら、その連言は UNKNOWN
  になり違反にしない（D1 の三値論理を式にも一様に適用する。既存の宣言的
  フィルタ・`VECTOR` 列参照と同じ規則）。
- **非有限値**: 格納値としての NaN/±∞ は `row_codec` が encode 時にも decode
  時にも拒否するため実際には到達しない。評価の途中で生じる非有限値
  （オーバーフロー）・0 除算（例: `CHECK (100 / qty > 1)` で `qty = 0`）は、
  既存の評価器契約どおり `Err` になり、`TenantWriteError::
  CheckEvaluationFailed`（`XX000`、詳細はクライアントへ返さない）で書き込みを
  拒否する（違反〔`23514`〕に丸めない・黙って通過もさせない）。
- **精度**: `BIGINT` の絶対値が `2^53` を超える値は `f64` で正確に表現できない
  ため `XX000` で拒否する（`id`（`id_as_finite_scalar`）と同じ境界。黙って
  丸めて誤った判定をしない）。`INTEGER`／`REAL` は `f64` で常に正確に表現
  できる。
- **既知の PostgreSQL との相違**（記録のみ）: 算術は `f64` で行うため `/` は
  整数の切り捨て除算にならない。`REAL` と小数リテラルの比較は `f32` を
  `f64` へ昇格した値で行う。単項マイナスがないため `CHECK (qty > -1)` は
  `42601` になり、`CHECK (qty > 0 - 1)` なら受理される。
- **エラーコード**: 新しい variant も写像変更もない。SQL-26（`docs/spec/
  04-behavior/sql-surface.md`）の `22012`/`22003` 化は対象外（後続課題）。

## D2. 永続化（カタログ v7）

`TableSchema` に非公開フィールド `checks: Vec<CheckConstraint>` を追加した
（`pub(crate) fn with_checks`／`checks()`。任意の SQL テキストを差し込めない
よう `pub(crate)` に留める）。

- `checks` が空なら既存の v2〜v6 とバイト完全一致（既存ゴールデンテストに
  影響しない）。1 件以上なら main の最新（v6。UNIQUE）の次の `v7` で書く。
  v7 は v6 の上位集合で、`pk:` 行（主キー未宣言なら空）・6 フィールドの列行・
  `uniq:<n>` セクション（v7 に限り `n == 0` を許容）の後ろに `checks:<N>` 行
  （`N >= 1`）+ N 行の `check:<name>:<col1,col2,...>:<hex(predicate_sql)>` を
  追記する（`encode_check_section`／`parse_check_section`）。v2〜v7 は互いに
  排他な正規形（`checks:0` の v7 は形式の一意性違反として拒否）。
- 述語テキストは hex エンコードし、区切り文字・改行・引用符との衝突を避ける
  （`DEFAULT` の符号化と同じ `hex_encode_byte`／`hex_decode` を共有。依存
  追加なし）。
- デコードは fail-closed（件数上限 → 行形状 → 識別子検証 → 参照列数上限 →
  hex 長上限 → hex 検証 → UTF-8 検証の順。参照列の実在・制約名の一意性・
  空述語は `validate_schema` が再検証）。確保は宣言件数（上限検査済み）の
  範囲に限る。
- 上限（実装既定値）: `MAX_CHECK_CONSTRAINTS_PER_TABLE`＝32、
  `MAX_CHECK_PREDICATE_SQL_LEN`＝4096 バイト、
  `MAX_CHECK_REFERENCED_COLUMNS`＝32。式ノード数・深さは既存の
  `MAX_EXPR_NODES`／`MAX_EXPR_DEPTH` を再利用する。
- **正規化レンダリング**（`sql::check_constraint::render_predicates`）:
  `WherePredicate` から SQL テキストへ変換する。算術部分式（`Expr::Binary`）
  は常に丸括弧で囲み、木構造に依存しない一意な往復を保証する。文字列
  リテラルは `'` を `''` にエスケープする。**生の SQL 原文は保存しない**。
  DDL 時に `lex → parse(render(x)) == x` の往復一致を検証し、不一致なら
  `42601` で拒否する（キーワードと衝突する識別子など、往復できない入力を
  永続化しないためのガード）。

`ENUM` 型の依存判定（`catalog_value_references_enum_type`。完全デコードを
経由しない軽量テキストスキャナ）も v7 形式を認識するよう拡張した——認識
しないと `CHECK` 制約を持つすべてのテーブルで `DROP TYPE`／`ALTER TYPE` の
依存判定が無関係に `CorruptSchema` へ倒れてしまう。`uniq:` と同じく、行数を
読み飛ばすだけでなく共有パーサー `parse_check_section` で構造を検証し、参照列の
実在・制約名の一意性も確認する（壊れたセクションを持つ値を本関数だけが
「依存なし」に丸めないため）。

## D3. 単一の検査点（`constraint::enforce_row_constraints_in_txn`）

- `CompiledChecks::compile(schema)`: `schema.checks()` が空なら `None`
  （CHECK の無いテーブルはオーバーヘッドゼロ）。各 `predicate_sql` を
  再パース → 禁止要素検査 → `bind_where_predicates` の順で処理し、宣言的
  フィルタは `MetadataFilter`、式フィルタは `ExprProgram::compile` した形で
  保持する。永続化済みの CHECK が再束縛に失敗した場合（カタログの手書き
  改変・実装不整合による漂流）は `TenantWriteError::Catalog(CorruptSchema)`
  （`XX000`）で書き込みを拒否する（スキップしない。fail-closed）。
- `CompiledChecks::enforce(schema, id, embedding, metadata)`:
  `row_codec::scan_scalar_columns_masked` で参照列だけを借用デコードし
  （Issue #350 と同じ必要列限定デコード）、三値論理（D1）で評価する。最初の
  違反で `TenantWriteError::CheckViolation { constraint }` を返す。

**呼び出し位置**: main の一意性制約（#903・#905）と同じ単一の検査点
`constraint::enforce_row_constraints_in_txn` に統合した。`tenant.rs` の全書き込み
関数（`insert_row_unchecked`／`insert_rows_unchecked`／`insert_typed_row_unchecked`／
`insert_typed_rows_unchecked`／`upsert_typed_rows_unchecked`／`update_row_unchecked`／
`update_row_columns_unchecked`／`update_rows_where_unchecked`／
`replace_typed_rows_by_text_key`）は、台帳照合（`ledger::record_in_txn`）と行の
書き込みの**後**、テーブル世代 bump・commit の**前**に、書き込んだ行 id 集合を
渡してこの入口を呼ぶ。検査点は各行を同一 write トランザクション内で読み戻し、
書き込まれた最終値（UPSERT の `DO UPDATE`・UPDATE の SET 適用後の値を含む）に
対して `CompiledChecks::enforce` を評価してから、一意性検査
（`enforce_unique_keys_in_txn`）へ進む（CHECK を一意性より先に評価する。両方に
違反する行は `23514`。PostgreSQL と同じ順序）。

- 台帳照合が先に行われるため、同一 `operation_id` の再送判定〔`23505`／
  `22023`〕が CHECK 違反より優先される。
- 違反時は `write_txn` を commit しないため、行・台帳・テーブル世代のいずれにも
  痕跡が残らない（同一 `operation_id` で正しい行を再送すれば成功する）。複数行
  文（複数行 INSERT・COPY・述語つき UPDATE・ファイル形 INSERT）は 1 行でも違反
  すれば全体が未反映のまま拒否される。
- 明示トランザクション（SQL-31・TASK-221）中の書き込みは `WriteTarget::InTxn` が
  共有 write トランザクションを渡すため同じ検査点を通る。違反文は `23514` で
  トランザクションを `Failed` へ遷移させ、ROLLBACK 後は先行文の行も残らない。
- 旧実装（main 取り込み前）は各書き込みシームで「行を書き込む前」に個別検査し、
  Rust API の生 `RowInput` 経路（`insert_row_unchecked` 等）を「非構造化
  metadata 契約」を理由に対象外としていた。main 側で一意性検査が同経路にも
  `row_codec::scan_scalar_columns_masked` を適用する契約になったことを受け、
  単一検査点への統合で生 `RowInput` 経路も検査対象になった（CHECK を持つ
  テーブルに正規レイアウトでない metadata を書くと、デコード失敗として
  fail-closed に拒否される）。

## D4. エラー契約

- `ErrorClass::CheckViolation`（`23514`。main〔#908 の索引 DDL を含む〕統合後の分類数 30 → 31）。
- `TenantWriteError::CheckViolation { constraint }` → `23514`。
  `TenantWriteError::CheckEvaluationFailed`（式評価自体の失敗。0 除算等） →
  `XX000`（内部事象。値・詳細はクライアントへ渡さない）。
- `SqlSurfaceError::CheckViolation { constraint }` → `23514`
  （`sql::exec::map_write_error`・ファイル形 INSERT 用の `map_incremental_error`
  に写像アームを追加）。
- wire-server: `http::status::http_status` で `23514 → 409`（`UniqueViolation`・
  `DuplicateTable` と同じ「対象の状態と矛盾する」区分）。NoSQL API doc
  （`crates/wire-server/docs/nosql-api.md`）の射影表へ行を追加した
  （`insert`／`update` op 経由で到達可能。`DuplicateTable`／`DuplicateColumn`
  とは異なり NoSQL の実要求からも到達する）。
- `Display`／`client_message` は制約名のみを含む固定文言
  （`new row violates check constraint "<name>"`）。行の値・id・テナントは
  一切含めない（security.md P0）。

## D5. DDL・ALTER との相互作用

- スコープは `CREATE TABLE` のみ。`ALTER TABLE ADD/DROP CONSTRAINT` は
  対象外（既存行の全テナント再検証が必要になるため別 Issue の担当）。
  `ALTER TABLE ADD COLUMN`（#900）で追加した列は既存の `CHECK` から参照
  されないため、既存制約の意味は変わらない。
- `alter_table_add_column`／`alter_table_widen_numeric_precision`：
  再エンコード時に `checks` を保持する（`decode_schema_with_resolver` →
  変更 → `encode_schema` の往復で自動的に保存される）。
- 依存列（`CheckConstraint::columns`）は、宣言的フィルタの参照列に加え、式述語
  （組み込み関数の引数・算術式の内側を含む）が参照する列も集める（`vec_norm(embedding)`
  の `VECTOR` 列等。PR #1055 codex P1 の是正）。`ALTER TABLE` の依存検査は記録された
  依存列と、述語テキストからの再計算結果（`recompute_referenced_columns`）の両方で
  判定し、再計算に失敗した制約は依存ありとみなす（fail-closed）。書き込み時の
  コンパイルも記録が再計算結果を覆っていることを検査し、欠けていれば `XX000` で
  拒否する。
- `alter_table_drop_column`：CHECK が参照する列の削除は
  `CatalogError::DependentObjectsStillExist` で拒否する（PostgreSQL は制約を
  自動削除するが、制約を黙って弱める経路を作らないよう安全側に倒す）。
- `alter_table_widen_numeric_precision`：CHECK が参照する列の型変更も同様に
  拒否する。
- 索引宣言（TASK-206・INDEX-7、Issue #908）との関係: `alter_table_drop_column` は
  削除列を含む索引宣言を同一 write トランザクションで掃除するが、CHECK 参照列の
  拒否はその掃除より前に判定し commit しないため、拒否時は索引宣言も一切変わら
  ない（CHECK 参照列を含む索引宣言は残る）。参照しない列の削除では従来どおり
  その列を含む宣言だけが掃除される。
- 列名 `check`／`constraint` の予約は `CREATE TABLE`／`ADD COLUMN` の列宣言の
  曖昧さを排除するためのもので、`CREATE INDEX ... (<col>, ...)` の列指定には
  適用しない（列指定は既存列名の参照のみで曖昧さが無く、SQL 表層からはこれらの
  名前の列を作れない）。
- `DROP TABLE` → 同じ名前で CHECK なしのテーブルを再作成した場合、旧制約は
  残らない（同一のカタログ値のため自動的に消える）。

## 対象外（申し送り）

- レーン A（`WHERE`／`SELECT`／集計での INTEGER/BIGINT/REAL/DOUBLE 列の式
  参照。未起票）。`CHECK` は Issue #1075 で `ColumnRefPolicy::
  AllowNumericColumns` による opt-in 拡張として対応済みだが、汎用の
  `WHERE`／`SELECT` への拡大は別 Issue の対象。
- `CREATE TABLE` の列型へ `REAL`／`DOUBLE PRECISION` を追加すること
  （現状 SQL から宣言できる数値型は `INTEGER`／`BIGINT` のみ。`CHECK` も
  `CREATE TABLE` 限定〔`ALTER TABLE ADD CONSTRAINT CHECK` は別 Issue〕のため、
  `REAL`／`DOUBLE` の `CHECK` は SQL 表層からは現状到達できず単体テストで
  担保する）。
- SQL-26 準拠のエラーコード（`22012`/`22003`）・単項マイナス・整数の
  切り捨て除算（数値列比較の評価規則。D1' 参照）。
- `qty > '5'`（INTEGER 列に文字列リテラルの `Compare` 形）の受理。
- `ALTER TABLE ADD/DROP CONSTRAINT`。
- NoSQL 表層への DDL op（`create_table` 相当）。
- `CHECK` 参照列への `ALTER COLUMN TYPE` の許可（現状は安全側で拒否）。
- `catalog.rs` の生書き込み API（`#[cfg(test)]` 限定・production では到達不能）は
  検査点を経由しない。Issue #1078 で、これら 3 API 自身が `CHECK` を含む制約付き
  テーブルへの書き込みを fail-closed に拒否するガードを追加したため、検査点を
  経由しなくても制約違反状態は作れない（一意性制約・FOREIGN KEY も同様）。

## 自動生成名と明示名の衝突（PR レビュー指摘 Low の是正）

`sql::check_constraint::validate_and_build` は、明示 `CONSTRAINT` 名を宣言位置に
関わらず先に全件確定し（明示名同士の重複は `42601`）、自動生成名
（`<table>_<col>_check`／`<table>_check`。衝突時は `_2`・`_3`… の接尾辞）は明示名を
**すべて**避けて決める。旧実装は自動生成名の解決時に「それまでに現れた名前」しか
見ていなかったため、自動命名の制約が同名の明示 `CONSTRAINT` より前に宣言されると
重複名のままカタログ検証へ進み、宣言順によって結果が変わっていた。現在は同じ
入力集合から常に同じ結果（受理なら同じ名前集合、重複なら同じエラー）になる。

## テスト

- `crates/engine/src/sql/allowlist.rs`（構文段。列制約・表制約・複数制約・
  `CONSTRAINT` 名・列名 `check`/`constraint` の予約・曖昧入力の拒否・上限超過・
  列数上限との独立性）。
- `crates/engine/src/catalog.rs`（v2〜v6 バイト不変・v7 往復・破損データの
  fail-closed 拒否〔`DROP TYPE` 依存判定を含む〕・ALTER 相互作用）。
- `crates/engine/src/sql/check_constraint.rs`（意味論検証・制約名の自動生成
  と衝突解決〔宣言順非依存〕・禁止要素・往復一致検証・`CompiledChecks` の
  compile/enforce・三値論理・数値列比較〔INTEGER/BIGINT/REAL/DOUBLE の
  合否・列マスク OR の fail-open 回帰ガード・BIGINT 精度超過・0 除算の
  fail-closed・数値列以外は引き続き拒否〕）。
- `crates/engine/src/sql/udf_call.rs`・`crates/engine/src/sql/
  expr_program.rs`（`ColumnRefPolicy`・`BoundExpr::ColumnRef` の束縛・
  評価差分テスト。既定ポリシーでの回帰確認を含む）。
- `crates/engine/tests/table16_check_constraint.rs`（結合テスト。単一行
  INSERT・複数行 INSERT・UPSERT〔`DO UPDATE`／`DO NOTHING`〕・単一行
  UPDATE・述語つき UPDATE・ファイル形 INSERT・COPY FROM・明示トランザクション・
  Rust API の生 `RowInput` 経路で `23514`・副作用ゼロ・台帳再送成功・永続化
  再オープン・CHECK と UNIQUE の評価順・曖昧な列定義の拒否・数値列 CHECK の
  宣言/永続化/再オープン後の継続検査・NULL 通過・0 除算の `XX000`・
  `DROP COLUMN` 依存拒否・`WHERE` 側の既存拒否の非回帰を固定）。
