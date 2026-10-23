# Changelog

`crates/engine`（`fandhe-db-engine`）・`crates/wire-server`
（`fandhe-db-wire-server`）の公開 API に影響する変更を記録する。
両クレートは同一バージョンで同期して公開する（`crates/wire-server/Cargo.toml`
の `engine` path 依存が完全固定バージョンで参照するため）。

## 0.2.0

### Added

- `ORDER BY <key> [ASC|DESC] NULLS FIRST|LAST` を広域取得・ウィンドウ・`GROUP BY` 集計・`SELECT DISTINCT`・JOIN・
  ビューの各経路で受理する（Issue #1791・SQL-56）。指定なしは従来どおり（ASC は末尾・DESC は先頭）。
  不正な指定と kNN 距離順への併用は `42601`。詳細は [order-by-having-expressions.md](docs/design/order-by-having-expressions.md)。
- `WHERE <col> [NOT] ILIKE '<pattern>'` を受理して評価する（Issue #1778・SQL-54）。ASCII 英字のみを畳み込み、
  非 ASCII はバイト完全一致で比べる。ICU 照合の列は `0A000`。索引は使わない。
  詳細は [like-wildcard-patterns.md](docs/design/like-wildcard-patterns.md)。
- 複数文メッセージの暗黙トランザクション内の `COPY` を受理する（Issue #1773・WIRE-20）。COPY の文で CopyIn／CopyOut へ入り、
  CopyDone の後に続きの文へ進む（従来は `0A000`）。最後の文が COPY のときは応答の前に commit する。
  詳細は [wire-copy-protocol.md](docs/design/wire-copy-protocol.md)。
- 複数文メッセージ中の `COPY` の原子性とエラー伝播の検証を網羅した（Issue #1774・WIRE-20。挙動変更なし）。
  失敗要因と位置の組み合わせで先行文の変更が残らないこと、明示トランザクションの `Failed`／`25P02` の整合を固定。
- `COPY ... FROM STDIN` が `HEADER`（`MATCH` を含む）・`DELIMITER`・`NULL`・`QUOTE`・`ESCAPE` を適用する（Issue #1764・WIRE-22）。
  text 形式では `\<区切り文字>` を区切り文字そのものとして受理する（既定のタブ区切りでも `\<TAB>` がリテラルのタブになる）。
  詳細は [wire-copy-protocol.md](docs/design/wire-copy-protocol.md)。
- `COPY (...) TO STDOUT` が `HEADER`・`DELIMITER`・`NULL`・`QUOTE`・`ESCAPE` を適用する（Issue #1765・WIRE-22）。
  ヘッダ行は `COPY n` の n に含めない。非既定のオプションを `0A000` で拒否していたゲートは撤去した。
- テーブル形 `COPY <table> [(<col>, ...)] TO STDOUT`（Issue #1771・WIRE-22 (5)・SQL-34）: `validate_copy` が受理し、
  `--max-result-rows` 超過は行を送る前に `54000` で拒否する。ビューは `42809`・不在表は `42P01`・重複列は `42701`。
  詳細は [wire-copy-protocol.md](docs/design/wire-copy-protocol.md)。
- opt-in の `--require-where-for-dml`（Issue #2210・SQL-36）: `EngineCore::with_require_where_for_dml(bool)` を追加し、
  有効時は `WHERE` 省略の SQL `UPDATE`／`DELETE` を変更開始前に `42601` で拒否する（既定は無効で挙動不変）。
  wire-server に値なしスイッチ `--require-where-for-dml` を追加した。明示した述語・NoSQL 表層・`TRUNCATE` は対象外。
  詳細は [require-where-for-dml-cli.md](docs/design/require-where-for-dml-cli.md)。

### Breaking Changes

- `sql::allowlist::SqlSurfaceError`（Issue #1792・SQL-56 (2)）: variant `OrderByPositionNotInSelectList { position: i64 }`
  （`42P10`。`ORDER BY` の序数が出力列の範囲外）を追加した。列挙型を網羅的に `match` する外部コードは
  コンパイルできなくなる。移行: 腕を追加する（`wire_code()`／`classify()` 経由で扱うコードは変更不要）。
- `sql::allowlist::WherePredicate::Prefix`（Issue #1778・SQL-54）: 必須フィールド `case_insensitive: bool` を追加した
  （`ILIKE` のとき真）。構築する外部コードと、全フィールドを列挙して分解するコードはコンパイルできなくなる。
  移行: 構築側は `case_insensitive: false` を足し、分解側は `..` を付ける。あわせて
  `declarative_filter::FilterOp` に `ILikeUnbound`、`LikePattern` に非公開フィールドを追加した
  （`FilterOp` を網羅 `match` するコードは腕を足す）。
- `sql::allowlist::ValidatedCopyTo`（Issue #1771・WIRE-22 (5)・SQL-34）: クレート内専用のフィールド `table_form`
  （`pub(crate)`）を追加した。構造体リテラルで生成する外部コードと、全フィールドを列挙して分解するコードは
  コンパイルできなくなる。移行: 構造体を自前で組み立てず、`validate_copy` 等の公開入口が返す検証済み値を使う。
  テーブル形かは `is_table_form()` で判定する。分解側は `..` を付ける。
- `core::CopyPlan::To`（Issue #1765・WIRE-22）: 第 3 要素に解決済みの `sql::copy_options::CopyOptions` を追加した
  （`To(format, result)` → `To(format, result, options)`）。2 要素でパターンマッチするコードはコンパイルできなくなる。
  移行: `To(f, r)` を `To(f, r, _)` に改め、出力にオプションを適用する側は第 3 要素を使う（`To(..)` は変更不要）。
- `COPY` の検証済みオプション（Issue #1764・WIRE-22）: `sql::allowlist::ValidatedCopyFrom`／`ValidatedCopyTo` に
  必須の公開フィールド `options: sql::copy_options::CopyOptions` を追加した。構造体リテラルで生成する外部コードと、
  全フィールドを列挙して分解（パターンマッチ）するコードはコンパイルできなくなる。移行: 構造体を自前で
  組み立てず、`validate_copy_*` 系の入口が返す検証済み値を使う（`options` は検証・既定値適用済みで、HEADER・DELIMITER・NULL・
  QUOTE・ESCAPE を `options` フィールドから参照できる）。分解側は `..` を付けるか `options` を束縛して使う。
- 述語形 `UPDATE`／`DELETE` の恒真述語（Issue #1744・SQL-36・SQL-41）: `WHERE TRUE`・`... OR TRUE`・`WHERE visible()` 単独を受理する。
  `sql::parser::bind_predicate_delete`／`bind_update_form` がこれらに `42601` を返さなくなり、`BoundOrGroup::is_trivially_true` を削除した。
  対象は自テナントの可視行で、影響行数上限は実行段が判定する。NoSQL 表層は不変。
- `WHERE` 省略の `UPDATE`／`DELETE`（Issue #1743・SQL-36）: `DELETE FROM t`・`UPDATE t SET ...` を受理する。
  `validate_delete_statement`／`validate_update_form` が `42601` を返さなくなり、`where_predicates()` が空を返しうる。
  単一行専用の入口（`validate_delete`・`validate_update`）・NoSQL 表層は不変（恒真述語は #1744 で変更）。
- 対象省略の `ON CONFLICT DO NOTHING`（Issue #1736・SQL-49）: `INSERT ... ON CONFLICT DO NOTHING` を受理し、行キー・
  PRIMARY KEY・UNIQUE 制約・UNIQUE 索引（式・部分を含む）のどれかに違反した行だけを飛ばす（`NOT NULL`・`CHECK`・`FOREIGN KEY`
  は従来どおりエラー）。公開 API の変更: `sql::allowlist::OnConflictClause::target` を `Vec<String>` から
  `OnConflictTarget`（`Columns(Vec<String>)`／`Omitted`）へ変更し、`sql::parser::BoundConflictTarget` に `AnyUnique` を追加した。
  対象省略の `DO UPDATE` は `42601`。詳細は [sql-upsert.md](docs/design/sql-upsert.md) の「Issue #1736 追記」。
- `current_schema()`・`current_schemas()`（Issue #1718・TABLE-27 (7)）: FROM なし SELECT で `current_schema()`・
  裸の `current_schema`・`current_schemas(bool)` が `search_path` に従った値（テナントで実在する SCHEMA のみ）を返す。
  互換性の変更: UDF 名 `current_schema`・`current_schemas` を予約名にした（`CREATE FUNCTION` は拒否される）。
  FROM あり SELECT・INSERT・DEFAULT・CHECK・索引・ビュー本文での使用は `0A000`。型は `text`／`text[]`。
  詳細は [schema-tenant-namespace.md](docs/design/schema-tenant-namespace.md) の §11。
- `search_path` の追加（Issue #1715・TABLE-27）: セッション変数 `search_path`（既定 `"$user", public`）の `SET`／`RESET`／`SHOW`
  を受理する。実効 path に `public` が含まれないセッションでは、一時表以外の未修飾のテーブル参照が `42P01`、作成系 DDL の
  作成先が `public` 以外になる文が `3F000`／`0A000` になる（既定の path の挙動は不変）。`"$user"` は handshake の username から
  展開し、自分の username と同名の SCHEMA を持つテナントの未修飾 `CREATE TABLE` は `0A000`（テナント SCHEMA への作成は後続 Issue）。
  公開 API は `Statement::SetSearchPath`・`ShowSearchPath`、`SqlOutcome::SetSearchPath`、`Token::QuotedDollarUser`、
  `SqlSurfaceError::NoSchemaSelected`、`ValidatedCreateTable.public_qualified`（pub フィールド追加）、`sql::search_path` の追加。
  BREAKING CHANGE: `Statement`・`SqlOutcome`・`Token`・`SqlSurfaceError` を網羅的に `match` するコードは variant の追従が必要。
- GIN 相当索引の読み取り（Issue #2208・INDEX-8）: 広域取得で jsonb 列への `@>`・`?`・`?|`・`?&`（`jsonb_ops`）と
  `@>`・`@?`・`@@`（`jsonb_path_ops`）が GIN 相当索引のポスティングで候補を絞るようになった（候補は全走査と同じ可視性判定と
  WHERE 全体の再評価を通るため、結果の行・順序・`LIMIT`／`OFFSET` の数え方は不変）。`EXPLAIN` はこの形のときだけ
  `scalar_plan: index_gin index=<索引名>`・`access_path: gin_index_candidates` を出す（従来は `plain_scan`／`full_scan`）。
  PK 点検索が成り立つ文は従来どおり PK が優先。`ORDER BY`・ウィンドウ・OR・NOT・空集合の問い合わせ・許可形でない jsonpath は不変。
  公開 API は `ScalarPlan::IndexGin` の追加（`#[non_exhaustive]`）。詳細は [gin-jsonb-index.md](docs/design/gin-jsonb-index.md) の §9。
- GIN 相当索引のキー抽出と DML 維持（Issue #2207・INDEX-8）: `CREATE INDEX ... USING gin` の宣言が物理テーブル
  `user_gin/{索引名}` のポスティングを持つようになり、INSERT・UPSERT・UPDATE・DELETE・COPY・TRUNCATE・分割実行 DML・
  FK 参照アクションと同じ write txn で維持する（`CREATE INDEX` は既存行から同期構築）。1 値あたり 65,535 エントリ・
  `jsonb_ops` の 1 エントリ 4,096 バイトを超える jsonb の書き込み・`CREATE INDEX` は `54000`（副作用なし）。
  結果集合は宣言の有無で変わらない（読み取り側の利用は #2208）。公開 API は `TenantWriteError::GinIndexEntryLimitExceeded`・
  `EngineCore::verify_gin_index_read_only`・`GinIndexAudit` の追加。
  BREAKING CHANGE: `TenantWriteError` に variant を追加したため外部の網羅的な `match` は更新が要る。GIN 索引付きの表への
  書き込みが上限超過で `54000` になりうる。
- GIN 相当索引の宣言（Issue #2206・INDEX-8）: `CREATE INDEX ... USING gin (<JSONB 列> [jsonb_ops | jsonb_path_ops])` を
  受理してカタログへ永続化する（従来は `0A000`）。JSONB 以外の列・複数列・式・`UNIQUE`・`WHERE`・`COLLATE` は `0A000`、
  未知の演算子クラスは `42704`。宣言の受理までで、索引の生成・利用は後続（宣言は結果集合に影響しない）。
  公開 API は `IndexKind::Gin(GinOpClass)` と `GinOpClass` の追加。索引カタログの形式は新版 `v8`（`v1`〜`v7` は不変）。
  BREAKING CHANGE: `IndexKind` に variant を追加したため外部の網羅的な `match` は更新が要る。`v8` を含むデータディレクトリは
  旧バイナリでは読めない（未知の版として拒否する）。
- NoSQL の単一行アドレスと insert・scan を PRIMARY KEY 基準へ移行（Issue #2201・TABLE-23・NOSQL-23）: SQL の `CREATE TABLE` で作った表（採番方式）に対して NoSQL の `scan`（`explain` を含む）・`insert`（`id` 不要）・`update`／`delete`（`where` に PRIMARY KEY の列を指定、複合可。PK なし表の単一行形は `42601`・述語形 `filter` は使える）が使える。疑似列 `id` を持つ表の `where: {"id": n}` は互換のまま。NoSQL の `aggregate`／`search` は採番方式の表に対して引き続き `0A000`。`update`／`delete` の `where` の型検査は表のスキーマ解決後（未知キーは従来どおり `42601`）。既存の要求は同じ応答を返すため、既存クライアントへの破壊的変更ではない。
- 既存表の `id` の移行（Issue #2200・TABLE-23）: 既存データディレクトリを開く時点で、従来方式の永続表のカタログを
  移行済みの印（v15 の `rowmode:migrated`。`RowIdentity::MigratedId`）へ一方向に書き換える（表ごとに原子的・再 open で収束。
  行・キー・索引・台帳は書き換えず、既存クエリの結果と wire の型 OID は不変）。壊れたカタログ項目は移行せず残す。
  運用手順（バックアップ）は README を参照。
  BREAKING CHANGE: 移行後のデータディレクトリは旧バイナリでは開けない（`rowmode:migrated` を不正値として拒否する）。
- 暗黙の `id` の廃止（Issue #2199・TABLE-23）: SQL 表層の `CREATE TABLE`・`CREATE TEMP TABLE` で新しく作る表は暗黙の
  `id` 疑似列を持たない（PG と同じ表定義）。`id` は予約名ではなくなり、`id` を通常の列として宣言でき（`tenant_id`・
  `visibility`・`check`・`constraint` は引き続き予約）、PRIMARY KEY を持たない表・`id` 列を持たない表を作れる。
  `INSERT` は `id` を要求せず、`SELECT *`・`RETURNING *` は宣言した列だけを返し、宣言の無い `id` は未定義列（`42703`）になる。
  `INSERT`（`ON CONFLICT`・`COPY` を含む）の行識別子は書き込み段が払い出す（応答・エラーに出ない）。`WHERE id = <n>` は
  新しい表では通常の述語。NoSQL 表層の `create_table`・Rust API の `create_table`・既存の全表は従来方式のまま
  （疑似列 `id` の挙動は不変）。新しい表に対する NoSQL の行 op・Rust API の行 API／検索 API は固定文言の `0A000` で拒否する
  （後続 Issue で対応）。新しい表の親を `id` 列で参照する FK は `0A000`、主キーの無い親への列省略 FK は `42830`。
  公開 API は `validate_create_table_tokens_with_implicit_id`・`ValidatedCreateTable.legacy_row_identity`（pub フィールド追加）・
  `TableLookup::table_is_allocated`（既定実装つきのメソッド追加）・`EngineCore::table_uses_allocated_row_identity` の追加。
  BREAKING CHANGE: SQL の `CREATE TABLE` で作った表の `INSERT ... (id, ...)` は `id` を宣言していなければ `42703` になる。
- 宣言 PRIMARY KEY の点検索（Issue #2198・TABLE-23）: 広域取得で PK の全構成列がリテラル等価のとき、永続一意索引の
  点検索で実行する。`EXPLAIN` はこの形のときだけ `scalar_plan: index_primary_key`・`access_path: primary_key_lookup`
  を出す（従来は `plain_scan`／`full_scan` 固定。PK 未宣言の表・部分等価・範囲比較・`ORDER BY` 付きは不変）。
  公開 API は `ScalarPlan::IndexPrimaryKey` の追加（`#[non_exhaustive]`）。結果は全走査と一致する。
- ビューの `WITH CHECK OPTION`（Issue #1673・TABLE-43 (2)）: `CREATE [OR REPLACE] VIEW ... WITH [CASCADED | LOCAL]
  CHECK OPTION` を受理して永続化し（従来は `42601`）、ビュー経由の `INSERT`／`UPDATE`（UPSERT を含む。`DELETE` は
  検査しない）が書いた行を検査対象の述語（NULL も違反）で検査して、違反を `44000`（固定文言。ビュー名・行の値を
  含めない）で文ごと拒否する。入れ子は PostgreSQL と同じ規則（LOCAL は自身の段、CASCADED は下位すべて、
  下位が自分で宣言していればその段も検査）。自動更新できないビューへの宣言は `0A000`。公開 API は
  `ViewDef.check_option`（pub フィールド追加）・`ViewCheckOption`・`Storage::create_view_with_check_option` の追加と、
  `TenantWriteError::WithCheckOptionViolation`・`SqlSurfaceError::WithCheckOptionViolation`・
  `ErrorClass::WithCheckOptionViolation`（`44000`・HTTP 400）の追加。宣言ありのビュー定義のカタログ形式は新版
  v3（宣言なしの v1／v2 のバイト列は不変。旧バイナリは v3 を読めない）。
- 部分 UNIQUE 索引（Issue #1657・INDEX-11 (4)）: 従来 `0A000` だった `CREATE UNIQUE INDEX ... WHERE <述語>` を受理し、
  述語が TRUE の行どうしの間でだけ一意性を `23505` で強制する（述語外の行どうし・述語外と述語内の重複は許可。
  テナントごと・NULLS DISTINCT）。作成時の既存行検査も述語 TRUE の行だけが対象。UNIQUE 制約へは展開しないため
  `ON CONFLICT` の推論対象にならない。述語付き一意索引のカタログ形式は新版 `v7`（`v1`〜`v6` のバイト列は不変。
  旧バイナリは `v7` を読めない）。永続一意索引（`user_uniq`）の `FORMAT_VERSION` を 3 から 4 へ上げたため、
  既存の全テナントは初回の書き込み時に一意索引を 1 回だけ再構築する（その回のみ O(n)）。`USING hnsw`・`COLLATE`・
  マテリアライズドビューとの組み合わせは引き続き `0A000`。
- 部分索引の格納と DML 維持（Issue #1654・INDEX-11）: 従来 `0A000` だった `CREATE INDEX ... WHERE <述語>` の
  決定的な述語を、書き込みトランザクション内で束縛して正規化テキストを永続化する（SQLSTATE が `0A000` から
  成功、または未知列 `42703`・セッション依存の述語 `42P17`・予約列と `visible()` `42601` 等へ変わる）。
  述語を満たす行だけが索引化され、`INSERT`／`UPDATE`／UPSERT／`COPY` は述語が TRUE の行でだけ式要素を評価する
  （NULL・FALSE の行は索引外。述語の評価エラーは文を拒否し、`CREATE INDEX` は既存行を全件検証する）。
  `UNIQUE`・`USING hnsw`・`COLLATE`・マテリアライズドビューとの組み合わせは引き続き `0A000`。参照列の
  `DROP COLUMN` は部分索引を連鎖削除し、`RENAME COLUMN` は述語を追随させ、述語・要素が参照する列の
  `ALTER COLUMN TYPE` は `0A000`。部分索引のプランナ選択と `EXPLAIN` への表示は #1656（下記）で結線した。
  公開 API は `IndexDef.predicate`（pub フィールド追加）・`IndexPredicateDef`・`IndexDef::with_predicate`・
  `IndexDef::has_predicate` の追加。述語付きの宣言のカタログ形式は新版 `v6`（`v1`〜`v5` のバイト列は不変。旧バイナリは
  `v6` を読めない）。公開の `Storage::create_index` は述語付きの `IndexDef` を `Invalid` で拒否する
  （SQL 表層のみが作成できる）。
- 部分索引のプランナ選択と EXPLAIN 表示（Issue #1656・INDEX-11・INDEX-7）: 起動時 opt-in 済みのエンジンで、
  通常の分類が全走査になる検索 SELECT のうち、`WHERE` が部分索引の述語を含意すると保守的に証明できるものは、
  部分索引の格納行で候補を絞る（残りの述語は必ず再評価する。信頼マスク経路には入れない）。証明できない・
  未構築・選択度超過・カタログ読み取り失敗はこれまでどおり全走査で、結果と RLS の契約は索引の有無で変わらない。
  `EXPLAIN` の `scalar_plan:` 行に新しい語彙 `index_partial` を追加し、含意された部分索引の名前を
  `index=<名前>[,<名前>...]`（昇順）で示す（クライアントから見える出力の語彙が増える）。公開 API は
  `ScalarPlan::IndexPartial` の追加（`#[non_exhaustive]`）。集計・`GROUP BY`・広域取得は部分索引を使わない。
- 式索引の UNIQUE と失効管理（Issue #1651・INDEX-9・INDEX-10）: 従来 `0A000` だった
  `CREATE UNIQUE INDEX ... ((<式>))`（列要素との複合を含む）を受理し、式の値の一意性を全書き込み経路で
  `23505` により強制する（NULL は重複しない・テナントごとに独立・不可視行とも衝突）。作成時に既存行へ
  重複があれば `23505` で拒否する（索引は残らない）。参照列の `DROP COLUMN` は一意の式索引も連鎖削除し、
  `RENAME COLUMN` は式テキストを追随させ、`ALTER COLUMN TYPE` は式を再束縛して既存行を再検証する
  （従来の `0A000` を撤去）。一意の式索引のカタログ形式は新版 `v5`（`v1`〜`v4` のバイト列は不変。旧バイナリは
  `v5` を読めない）。永続一意索引（`user_uniq`）の `FORMAT_VERSION` を 2 から 3 へ上げたため、既存の
  全テナントは初回の書き込み時に一意索引を 1 回だけ再構築する（その回のみ O(n)）。
- 式索引のプランナ選択（Issue #1650・INDEX-10・INDEX-7）: WHERE の `<式> op <リテラル>`（`op` は `=`／`<`／`<=`／
  `>`／`>=`）が宣言済みの式索引と構造一致したときだけ、式キー索引で候補を絞る。一致しない形・宣言なし・
  起動時 opt-in なしは従来どおりの全走査で、結果は変わらない。`EXPLAIN` の `scalar_plan:` 行（検索・集計・
  `USING PLAN`）に `index=<式索引名>` を出す（語彙は増やさない）。公開 API は `sql::explain::ExplainShape` が
  `Copy`・`Eq` でなくなる（式候補を保持するため。`from_filters` のシグネチャは不変）。
- 式索引の DML 維持（Issue #1649・INDEX-10）: 式索引を持つテーブルへの `INSERT`／`UPDATE`／UPSERT／`COPY`
  （参照アクションによる子表の更新を含む）が、式の評価エラー（0 除算 `22012` 等）・非有限値（`22003`）で
  失敗する（副作用ゼロ）。NULL を返す行は許容する。`CREATE INDEX ... ((<式>))` は保存前に既存行を全件検証し、
  失敗行があれば拒否する（応答は固定文言）。公開 API は `TenantWriteError::IndexExpressionEvaluationFailed`
  の追加（`#[non_exhaustive]` ではないため網羅的 `match` の下流は追随が必要）。
- 式索引の格納（Issue #1648・INDEX-10）: 従来 `0A000` だった `CREATE INDEX ... ((<式>))`／`(f(col))` のうち
  決定的な式を、書き込みトランザクション内で束縛して永続化する（未知列 `42703`・型不整合は束縛側の
  SQLSTATE・型で決まる非決定性 `42P17`・索引化できない結果型 `0A000`）。`UNIQUE`・`USING hnsw`・式への
  `COLLATE` との組み合わせは引き続き `0A000`。参照列の `DROP COLUMN` は式索引を連鎖削除し、
  `RENAME COLUMN`／`ALTER COLUMN TYPE` は `0A000`。公開 API は `IndexDef.expressions`（pub フィールド追加）・
  `IndexExpressionDef`・`IndexDef::with_expressions` の追加。式を含む宣言のカタログ形式は新版 `v4`
  （`v1`／`v2`／`v3` のバイト列は不変。旧バイナリは `v4` を読めない）。公開の `Storage::create_index` は
  式付きの `IndexDef` を `Invalid` で拒否する（SQL 表層のみが作成できる）。
- `REFRESH MATERIALIZED VIEW [CONCURRENTLY] <名前> [WITH [NO] DATA]`（Issue #1665・TABLE-42）:
  従来 `42601` だった構文を受理し、全文脈の実体を同じスナップショットから単一トランザクションで作り直す。
  失敗時は旧実体が残る。`WITH NO DATA` は実体を破棄して未実体化（`55000`）にする。タグは固定の
  `REFRESH MATERIALIZED VIEW`。`CONCURRENTLY` は #1666 で本実装済み。公開 API は
  `ParsedSql::RefreshMaterializedView`・`SqlOutcome::RefreshMaterializedView`・
  `ddl::RefreshMaterializedViewOutcome` を追加する。

- `REFRESH MATERIALIZED VIEW CONCURRENTLY`（Issue #1666・TABLE-42 (4)）: 従来の暫定 `0A000` をやめて受理する。
  未実体化は `0A000`、WHERE を持たない列の UNIQUE 索引が無ければ `55000`（固定文言）で、いずれも旧実体が残る。
  前提を満たせば非 CONCURRENTLY と同じ結果になる。`CONCURRENTLY` と `WITH NO DATA` の併用は `42601`。公開 API は
  `SqlSurfaceError::MaterializedViewNotRefreshableConcurrently` を追加する。

- `DROP MATERIALIZED VIEW [IF EXISTS] <名前>[, ...] [CASCADE | RESTRICT]` と matview への索引（Issue #1667・
  TABLE-42 (5)(6)・TABLE-37）: 従来 `42601` だった `DROP MATERIALIZED VIEW` を受理する（複数指定は単一 txn で原子的、
  表・ビュー・索引名の指定は `IF EXISTS` でも `42809`、依存する matview は RESTRICT で `2BP01`、CASCADE で依存する
  側から削除）。表・ビューの `DROP ... CASCADE` は依存 matview を推移的に削除する（従来は #1663 の暫定ガードで
  `2BP01`）。matview への `CREATE [UNIQUE] INDEX` は従来 `42809` だったが宣言を受理する（スカラーのみ。
  `USING hnsw` は `0A000`、暗黙の `id` 列は無く `42703`）。UNIQUE 索引は文脈（テナント）ごとに一意性を強制し、
  重複は `23505`（固定文言）。公開 API は `ParsedSql::DropMaterializedView`・`SqlOutcome::DropMaterializedView`・
  `CascadedObject::MaterializedView` の追加。
- `CREATE MATERIALIZED VIEW [IF NOT EXISTS] <名前> [(<別名>, ...)] AS <本文> [WITH [NO] DATA]`（Issue #1664・TABLE-42）:
  従来 `42601` だった構文を受理し、実体化対象の文脈ごとに本文を評価した結果を単一トランザクションで保存する。
  読み取りは読み手の文脈に一致する実体だけを返し、実体が無ければ新分類 `55000`（HTTP 射影は 409）。
  書き込みは `42809`。実体化対象の文脈は `EngineCore::with_materialization_contexts` で注入する（未設定なら
  作成セッションの文脈のみ）。公開 API は `Statement::MaterializedView`・`ParsedSql::CreateMaterializedView`・
  `SqlOutcome::CreateMaterializedView`・`SqlSurfaceError::MaterializedViewNotPopulated`・
  `ErrorClass::ObjectNotInPrerequisiteState` の追加と、`TableLookup::materialized_view_columns`（既定実装あり）の追加。
  matview が直接参照する表への `DROP COLUMN`（`2BP01`）・`ALTER COLUMN TYPE`／`RENAME COLUMN`（`0A000`）は
  列単位の追随（別 Issue で追跡）まで拒否する。
- `TRUNCATE ... [CASCADE | RESTRICT]`（Issue #1644・SQL-48 (2)）: 従来 `42601` だった `CASCADE`／`RESTRICT` を
  受理する（省略時は RESTRICT）。RESTRICT は指定していない表の FK が参照する表の `TRUNCATE` を、参照元の
  行の有無・他テナントの行に関係なくカタログ宣言だけで `0A000` で拒否する（従来は行を消した後の事後検査で
  自テナントの子の行が残れば `23503`。子が空なら成功していた）。CASCADE は FK の参照元の表を推移的に
  対象へ加え、同じトランザクションで自テナントの行を削除する（参照アクションは発火しない）。公開 API は
  `ValidatedTruncate::behavior` の追加、`TruncateOutcome` の `Copy` 削除と `cascaded()` の追加、
  `TenantWriteError::TruncateReferencedByForeignKey` の追加。
- IDENTITY 列（Issue #1695・TABLE-24）: `CREATE TABLE` の列制約 `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY
  [(<SEQUENCE オプション>)]` を受理する（従来は `42601`）。整数型・`NOT NULL`・列が所有する暗黙 SEQUENCE（型は列型で
  決まり、範囲外のオプションは `22023`）・`DEFAULT` の IDENTITY 表現へ展開し、表と同じ write txn で作る。`ALWAYS` 列への
  明示値・明示 NULL・UPDATE の SET は `428C9`（HTTP 400。型の束縛より前で、SEQUENCE を進めない）、`BY DEFAULT` は
  明示値を受け付ける。COPY は IDENTITY 列へ入力値をそのまま書く。`OVERRIDING`・ALTER の IDENTITY 操作・IDENTITY 列の
  型変更・一時表・生成列は `0A000`、`DEFAULT` との併記は `42601`。公開 API は `ColumnDefault::Identity`・`IdentityKind`・
  `ValidatedCreateTable::identity_columns`（pub フィールド追加）・`IdentityColumn`・`SequenceOptions`・
  `SqlSurfaceError::GeneratedAlways`・`ErrorClass::GeneratedAlways`（`428C9`）の追加。カタログの列 `default` に新タグ
  `a`／`b` を足す（旧バイナリは未知タグを `CorruptSchema` で拒否する）。
- `SERIAL` 系型（Issue #1694・TABLE-24）: `CREATE TABLE` の `SERIAL`／`BIGSERIAL`／`SMALLSERIAL`（別名 `SERIAL4`・
  `SERIAL8`・`SERIAL2`）を受理する（従来は ENUM 型名候補として `42704`）。基底の整数型・`NOT NULL`・暗黙 SEQUENCE
  （`<table>_<col>_seq`・列が所有・MAXVALUE は列型の上限）・`DEFAULT nextval` へ展開し、表と同じ write txn で作る。
  DROP TABLE・DROP COLUMN は所有 SEQUENCE も削除し、RENAME は所有者を追従させる。`DEFAULT` 併記は `42601`、
  `ADD COLUMN`・一時表・配列は `0A000`、列定義の外の型名は `42704`。SERIAL 系の 6 語は列型キーワードとなり、
  制約名・NoSQL の `enum_type` として使えなくなる。公開 API は `ValidatedCreateTable::serial_columns`（pub フィールド追加）・
  `SerialColumn`・`SqlColumnTypeName::Serial`・`SerialKind` の追加。
- `TRUNCATE ... RESTART IDENTITY`／`CONTINUE IDENTITY`（Issue #1645・SQL-48 (3)）: 従来 `42601` だった識別句を
  受理する（省略時は CONTINUE）。RESTART は対象表が所有する SEQUENCE の自テナントの採番を開始値へ戻す。
  所有関係は SEQUENCE 定義 blob の版 3 に格納する（所有者なしは従来の版 2 のまま。旧バイナリは版 3 を
  `CorruptSchema` で拒否する）。SQL から所有者を作る `OWNED BY`／SERIAL は後続 Issue（#1694）のため、それまで
  RESTART は採番に作用しない。明示トランザクション内は所有 SEQUENCE があれば `0A000`。公開 API は
  `ValidatedTruncate::restart_identity` の追加。
- `CREATE VIEW v (a, b) AS ...`（Issue #1659・TABLE-41 (1)）: 従来 `42601` だった列別名リストを受理する。
  別名は出力列に先頭から名前を付け、短いリストの残りは導出名のまま。出力列より長いリストは `42601`、
  リスト内または最終名の重複は `42701`。別名付きビューが読む表への `RENAME COLUMN` は `0A000`、
  `DROP COLUMN`／`ALTER TYPE` は依存ありとして拒否する（保守側）。公開 API は `ViewDef.column_aliases` の追加
  （格納形式 v2。別名なしは従来と同一のバイト列）と `Storage::create_view_with_column_aliases`・
  `ValidatedCreateView::column_aliases` の追加。
- `ALTER COLUMN TYPE` の依存オブジェクト再構築（Issue #1637・TABLE-40 (3)(4)・TASK-278）:
  一般変換が、依存する索引宣言・PK／UNIQUE・CHECK・FK・DEFAULT／補完値を新しい型で再構築・再検証する
  （従来は一律 `0A000`。重複 `23505`・CHECK 違反 `23514`・FK の型照合 `42804`／参照整合性 `23503`・
  DEFAULT の変換不能 `42804`）。再検証は全テナント・全可視性の行で、拒否時は表不変・エラー文言に値を含めない。
  ビューが参照する列（`SELECT *`・連鎖を含む）は PostgreSQL と同じ `0A000` のまま。公開 API の変更なし。
- DROP の CASCADE／RESTRICT（Issue #1641・TABLE-37）: `DROP TABLE|VIEW` が `CASCADE`／`RESTRICT` を受理し、`CASCADE` は依存する FK 制約とビューを連鎖削除する。`ValidatedDropTable` に `pub behavior: DropBehavior` を追加、`DropTablesReport` に `cascaded` を追加、`DropTableOutcome`／`DropViewOutcome` から `Copy` を外し（`cascaded()` を追加）、`Storage::drop_tables` に `DropBehavior` 引数を追加した。`Storage::drop_view_with_behavior` を追加。`DROP INDEX|TYPE ... CASCADE` は `42601` のまま。
- 表の改名でビュー・台帳を追随（Issue #1625・TABLE-38）: `ALTER TABLE ... RENAME TO` が単純形ビューの
  定義を新名へ付け替え、`operation_id` 台帳・`last_op`・分割実行ジョブ記録を新名へ移す（従来は
  ビュー依存の表の改名を `0A000` で拒否し、台帳は削除していた）。評価後射影形ビューが読む表の改名は
  引き続き `0A000`。公開 API の型変更なし。
- 式索引の parser と式の決定性検査（Issue #1647・INDEX-10）: `CREATE INDEX` の列リストで
  `((<式>))` と関数呼び出し形 `(f(col))` を構文上受理し、非決定式を `42P17`、集計を `42803`、
  ウィンドウを `42P20`、部分クエリ・SRF・組み込み以外の関数を `0A000` で拒否する（従来はいずれも
  一律 `0A000`）。決定的な式も後続 Issue（#1648〜#1651）までは `0A000`（式索引は作成できない）。
  `((col))` は通常の列索引として作成できる。公開 API は `SqlSurfaceError`（`InvalidObjectDefinition`・
  `GroupingError`・`WindowingError`）・`ErrorClass`（同名 3 分類）への variant 追加と
  `ValidatedCreateIndex::expression_keys` の追加。
- 部分索引の述語 parser と決定性検査（Issue #1653・INDEX-11）: `CREATE INDEX ... WHERE <述語>` の述語を
  解析し、非決定を `42P17`、集計を `42803`、ウィンドウを `42P20`、部分クエリ・SRF・組み込み以外の
  関数を `0A000`、`visible()` と構文誤りを `42601` で拒否する（従来は `WHERE` を一律 `0A000`）。
  決定的な述語も後続 Issue（#1654）までは `0A000`（部分索引は作成できない）。公開 API は
  `ValidatedCreateIndex::predicate` の追加。
- `DROP TABLE a, b`（Issue #1639・TABLE-37 (1)）: 従来 `42601` だった複数テーブル指定を受理する。
  文全体を 1 つの write txn で原子的に実行し（不在・種類違い・依存のどれか 1 つでもあれば何も削除しない）、
  同じ文に並べた名前どうしの依存は数えない。`CASCADE`／`RESTRICT` は従来どおり `42601`。公開 API は
  `ValidatedDropTable.table_name: String` の `table_names: Vec<String>` への置き換えと
  `Storage::drop_tables`・`DropTablesReport` の追加。
- `ALTER COLUMN TYPE` の全行書き換え（Issue #1636・TABLE-40 (1)(2)(5)・TASK-278）:
  `USING <式>` 付きの変換と、代入変換（SQL-47）のある組の `USING` 省略変換（BIGINT→INTEGER・
  NUMERIC の位取り変更・数値↔文字列等）を受理する。従来の `0A000`（USING 付き）・`42804`（縮小・異種）
  から変わる。全テナント・全可視性の行を 1 つの write txn で書き換え、1 行でも変換に失敗
  （`22P02`／`22003`／`22001`／`23502` 等）すれば表は不変で、エラー文言に値・テナント・行 id を含めない。
  PK／UNIQUE／FK／CHECK／索引宣言／ビューが参照する列と DEFAULT を持つ列への一般変換は、Issue #1637
  で再構築に置き換えた（下記。従来の受理経路は不変）。`USING <対象列>` は省略と同じ扱い。公開 API は
  `Storage::alter_table_alter_column_type_collated` の引数追加（crate 内部）で、外部公開面の変更なし。
- `ALTER TABLE ... ALTER COLUMN ... TYPE ... USING <式>` と `SET DATA TYPE`（Issue #1635・TABLE-40）:
  従来 `42601` だった構文を受理する。USING は旧スキーマで束縛・型検査し（未定義列 `42703`、
  代入変換不可 `42804`、組み込み以外の関数 `42601`）、通ったものは行の書き換え未実装のため
  `0A000` で拒否する。USING 省略で代入変換の無い組は `42804`。公開 API は
  `ValidatedAlterTableAlterColumnType.using` の追加。
- `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` と複数アクション（Issue #1633・TABLE-36・TABLE-39 (5)）:
  従来 `42601` だった構文を受理する。同名の生存列があれば何もせず成功し、`,` 区切りの列単位アクション
  （ADD／DROP COLUMN・SET／DROP DEFAULT・SET／DROP NOT NULL）は 1 つの write txn で原子的に適用する
  （途中失敗で何も適用しない）。RENAME との混在は `42601`、制約・TYPE 変更との混在は `0A000`。
  公開 API は `ValidatedAlterTableAddColumn.if_not_exists`・`ValidatedAlterTable::Multiple`・
  `AlterTableAction::{AddColumnSkipped, Multiple}` の追加（既存の網羅的 `match` はアームの追加が必要）。
- `ALTER COLUMN DROP NOT NULL`（Issue #1632・TABLE-39 (4)）: 従来 `42601` だった構文を受理する。
  主キーの構成列は `42P16`、FK 参照先側の整合違反は `42830`。公開 enum `ValidatedAlterTable` に
  `AlterColumnDropNotNull`、`AlterTableAction` に同名の variant、`CatalogError`・`SqlSurfaceError` に
  `ColumnInPrimaryKey` を追加した（既存の網羅的 `match` はアームの追加が必要）。
- `ALTER COLUMN SET／DROP DEFAULT`（Issue #1628・TABLE-39）: 従来 `42601` だった構文を受理し、
  INSERT 既定値（`default_expr`）だけを変える（既存行の補完値 `fill_value` は変えない）。公開 enum
  `ValidatedAlterTable` に `AlterColumnSetDefault`・`AlterColumnDropDefault`、`AlterTableAction` に
  同名の variant を追加した（既存の網羅的 `match` はアームの追加が必要）。以後 `ColumnDef` の
  `fill_value` と `default_expr` は ALTER で食い違いうる（食い違うスキーマはカタログ v14 で書く）。
- 列定義の分離（Issue #1627・TABLE-39 の前提）: `engine::catalog::ColumnDef` の公開フィールド
  `default` を `default_expr`（`INSERT` 既定値）へ改名し、読み出し時補完値 `fill_value` を追加する。
  `ColumnDef::with_default` は両方を同じ値にする。カタログは補完値が食い違う列を持つ場合のみ
  新形式 v14 で書く（旧バイナリは v14 を拒否する）。

- `DROP TABLE|INDEX|VIEW|TYPE IF EXISTS`（Issue #1614・TABLE-36）: 従来 `42601` だった構文を受理し、
  対象不在は副作用ゼロの no-op で成功する。公開 API は `ValidatedDropTable.if_exists` の追加と
  `Drop{Table,View,Index,Type}Outcome::skipped()` の追加（構造体リテラルでの構築は不可に）。
- `CREATE TABLE`／名前付き `CREATE INDEX` の `IF NOT EXISTS`（Issue #1613・TABLE-36）: 従来 `42601` だった
  句を受理し、同名の表・ビュー・索引があれば何もせず成功する。公開構造体 `ValidatedCreateTable` に
  公開フィールド `if_not_exists` を追加した（構造体リテラルで作る外部コードは追従が必要）。
  `CREATE VIEW`／`CREATE TYPE`／名前なし `CREATE INDEX` の `IF NOT EXISTS` は `42601` のまま。
- 配列型 OID の公告（Issue #1606・TABLE-35）: 配列列と結果型が確定する配列式の `RowDescription` の
  型 OID が `text`（25）から要素型ごとの PostgreSQL 配列型 OID（`int4[]`=1007 など）に変わる。
  `ENUM[]` は 25 のまま。配列のバイナリ形式は従来どおり `0A000`。
- `EXTRACT`／`date_part`／`date_trunc` の INTERVAL 対応と `justify_*`（Issue #1582・TABLE-34）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::BuiltinFn` に `IntervalPart`・`IntervalTrunc`・`JustifyHours`・
  `JustifyDays`・`JustifyInterval` を追加した（既存の網羅的 `match` はアームの追加が必要）。
  従来 `42804` だった `date_part(field, interval)` などが受理に変わる。`justify_hours`／
  `justify_days`／`justify_interval` は組み込み関数名として予約され、同名の UDF 定義は `42723` になる。
- 日付時刻と INTERVAL の加減算・乗除（Issue #1581・TABLE-34）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::Expr` に `IntervalLiteral(Interval)`、`BoundExpr` に
  `Interval`・`IntervalColumnRef`、`ExprType`／`ExprValue` に `Interval` を追加した
  （既存の網羅的 `match` はアームの追加が必要）。従来 `42804` か「列を式で使えない」
  エラーだった `TIMESTAMP ± INTERVAL`・`DATE ± INTERVAL`・`INTERVAL * n`・
  `TIMESTAMP - TIMESTAMP` などの INTERVAL 算術と、INTERVAL 列の式内参照・型付きリテラル
  `INTERVAL '…'` が受理に変わる。INTERVAL が絡む未対応の演算子の組は `42883` で拒否する。
- `AT TIME ZONE`・TIMESTAMPTZ の変換と問い合わせ（Issue #1576・TABLE-33）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::Expr` に `TimestampTzLiteral { micros, session_local }`、
  `BoundExpr` に `TimestampTz`・`TimestampTzColumnRef`、`ExprType`／`ExprValue` に `TimestampTz`、
  `BuiltinFn` に TZ 変換・`DatePartTz`・`DateTruncTz` 系の variant、
  `datetime_fn::DatePartField` に `Timezone`・`TimezoneHour`・`TimezoneMinute`、
  `declarative_filter::TypedLiteral` に `TimestampTz` を追加した（既存の網羅的 `match` は
  アームの追加が必要）。`timezone` を組み込み関数名として予約したため、同名の UDF は
  作れない（`42723`）。TIMESTAMPTZ の述語・集計・二次索引・集合演算は従来の `0A000`／`42883`
  拒否から受理へ変わる。
- セッション TimeZone（Issue #1575・TABLE-33）に伴う公開 API の変更: 公開 enum
  `engine::sql::allowlist::Statement` に `SetTimeZone { value: TimeZoneSetting }`・`ShowTimeZone`、
  公開 enum `engine::sql::SqlOutcome` に `SetTimeZone`・`Show(QueryResult)` を追加した
  （既存の網羅的 `match` はアームの追加が必要）。`engine::datetime::SessionTimeZone` は表示値を
  持つ構造体になり、`SessionState::time_zone`・`SessionTimeZone::parse_setting` を追加した。
  pg wire の起動時 ParameterStatus に `TimeZone`（既定 `UTC`）が増え、StartupMessage の
  `TimeZone` を受理する（不正値は認証後に FATAL `22023`）。
- crate 名を `fandhe-vector-db-engine` / `fandhe-vector-db-wire-server` から
  `fandhe-db-engine` / `fandhe-db-wire-server` へ変更した（プロジェクト名を
  vector-db から fandhe-db へ改めたため）。0.2.0 は新しい crate 名での初公開版となる。
  旧 crate 名の 0.1.0 は crates.io に残すが、以後は更新しない。移行は `Cargo.toml` の
  依存名を新しい crate 名へ置き換える（lib 名 `engine` / `wire_server` は不変のため
  `use` 文の変更は不要）。
- バッチ上限の環境変数を `VECTOR_DB_BATCH_MAX_FILES` / `VECTOR_DB_BATCH_MAX_TOTAL_BYTES` /
  `VECTOR_DB_BATCH_MAX_CHUNKS` から `FANDHE_DB_BATCH_MAX_FILES` /
  `FANDHE_DB_BATCH_MAX_TOTAL_BYTES` / `FANDHE_DB_BATCH_MAX_CHUNKS` へ改名した。旧名は
  読まれず（設定しても既定の上限が適用される）、設定されていれば起動時に stderr へ
  警告が出る。移行は環境変数名を新名へ置き換える。
- `engine::sql::allowlist::ValidatedInsert` の公開フィールド
  `pub values: Vec<InsertLiteral>` を `pub rows: Vec<Vec<InsertLiteral>>` へ
  変更した。単一行 INSERT は `rows.len() == 1` として同じ内容を保持するため、
  移行は `stmt.values` を `stmt.rows[0]`（単一行前提のコードの場合）へ
  読み替えるか、複数行を扱う場合は `rows` を反復する形へ書き換える。
- 書き込み文の現在時刻関数（Issue #2188・SQL-37）に伴う公開 API の変更: 公開 enum
  `engine::sql::allowlist::InsertLiteral` に `CurrentTime(TimeDefault)`、公開 enum
  `engine::catalog::TimeDefault` に `StatementTimestamp`・`ClockTimestamp`・`LocalTime(Option<u8>)`
  を追加した（網羅 `match` は更新が必要）。`engine::sql::parser::BoundInsert::volatile_default_columns`
  の型を `Vec<usize>` から `Vec<VolatileColumn>`（新しい公開 enum）へ変更した。移行は
  `VolatileColumn::Default(i)` を従来の位置 `i` として読み替える。
- `operation_id` 台帳に値フォーマット v3（ハッシュ＋評価に用いたトランザクション開始時刻）を追加した。
  時刻関数を評価した書き込みだけが v3 を書く。v3 を書いた DB を旧バイナリで開くと、該当キーの再送照合が
  `XX000`（fail-closed）になる（ダウングレード不可の既知の制約。移行処理は無い）。
- `engine::sql::parser::BoundInsertForm` に新しい variant `RowBatch(Vec<BoundInsert>)`
  を追加した。この enum を網羅的に `match` していた利用側コードは、新 variant
  への対応を追加しないとコンパイルできない。
- いずれも SQL 表層の複数行 `VALUES (...), (...)` 構文サポート（ビヘイビア ID:
  SQL-16、TASK-190）のための変更であり、単一行 `INSERT` の外部観測可能な挙動
  （wire プロトコル応答・`wire_code`）は不変。
