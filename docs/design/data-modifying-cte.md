# データ変更 CTE（`WITH x AS (INSERT/UPDATE/DELETE ...)`）の設計記録

ポインタ: SQL-63（`docs/spec/04-behavior/sql-surface.md`）・SQL-29 (b)・SQL-62・RLS-10 (b)・
RECOVER-1。親 Issue #1874 配下の 4 Issue（#1875 構文 → #1876 書き込み経路での実行 →
Issue #1877 1 文内の可視性・原子性 → #1878 RLS・権限境界テスト）の設計記録を本ファイルに集約する。

## Issue #1875: 構文受理と列解決（実行なし）

CTE 本文に単一表の `INSERT ... VALUES`・`UPDATE`・`DELETE`（各 `RETURNING` 任意）を書ける構文を
受理し、`RETURNING` が外側へ公開する列（名前と並び）を確定する。**書き込みは一切発生させない**。
検証をすべて通過した文も固定文言の `0A000`（`data-modifying CTE is not supported yet`）で止める。
公開 API（`Statement` の variant）は変えない。実行結線は #1876。

### 構成

| 箇所 | 役割 |
| ---- | ---- |
| `sql/allowlist.rs` `parse_data_modifying_cte_body` | 本文先頭が DML なら既存の DML 許可パーサーで解析する（第 2 のパーサーなし） |
| `sql/cte.rs` `CteBody` / `DataModifyingCteBody` | 検証済みの DML 型をそのまま保持（#1876 の実行器が再構築なしで受け取る） |
| `sql/cte.rs` `validate_data_modifying_cte` | 対象表の確認と `RETURNING` 公開列の確定（列名リストの改名を含む） |
| `sql/allowlist.rs` `WITH` 分岐 | 定義順に検証し、主クエリの列スコープを既存検査へ通し、最後に `0A000` |
| `sql/statement_splitter.rs` | データ変更 CTE を含む `WITH` を `Write` に分類（fail-closed） |

### 拒否コード

| 入力 | コード |
| ---- | ------ |
| 本文の構文誤り、副文の `USING OPERATION_ID`・`PARTITIONED`、本文内の `;` | `42601` |
| 主クエリが DML（`WITH ... INSERT ...`） | `42601`（従来どおり） |
| 対象表なし | `42P01` |
| `RETURNING` に存在しない列 | `42703` |
| 主クエリが `RETURNING` に無い列を参照 | `42703`（既存 CTE と同じ） |
| 列名リストが `RETURNING` の列数より長い | `42P10` |
| 定義数・連鎖の上限超過（データ変更 CTE も数える） | `54000` |
| 検証を通過した文・`RETURNING` なしの参照・`INSERT ... SELECT`・`UPDATE ... FROM`・`DELETE ... USING`・ビュー／CTE が対象・SELECT 本文 CTE からの参照・列情報を返せない lookup | `0A000` |

### 不変条件

- `resolve_relation` はデータ変更 CTE の名前に一致したとき、表・ビューへ展開せず固定 `0A000` を返す
  （同名の実表を RLS の暗黙適用を迂回して読む経路を作らない）
- `CteDef` はテナント・`PolicyContext` を保持しない（RLS-10 (b)）。エラーはテナント間で同一
- `operation_id` の必須化は文単位の判定で、副文ごとには呼ばない（副文は `LedgerMode::Optional` で解析）

### 後続への引き継ぎ

- 実行・`RETURNING` 行の具体化・型付き列メタデータ・Describe（#1876）、可視性・原子性（#1877）、境界テスト（#1878）
- `INSERT ... SELECT`・`UPDATE ... FROM`・`DELETE ... USING` 本文、ビューや一時表が対象の本文、
  SELECT 本文 CTE からの参照、主クエリが DML の形は未対応
- 副文の `INSERT` の行数上限（`max_insert_rows`）は検証段では適用されない。実行結線時に必ず適用する
- `WITH RECURSIVE` 内のデータ変更 CTE の扱いは再帰 CTE の実装後に確認する

## Issue #1876（書き込み経路での実行）

ポインタ: SQL-63 (1)〜(6)・RECOVER-1・RECOVER-11・RLS-9・RLS-10 (b)。親 #1874、後続 #1877（文内の可視性・
同一行の多重変更・失敗時動作）・#1878（RLS・権限の境界テスト）。

### 経路

| 入口 | 役割 |
| ---- | ---- |
| `sql::allowlist::validate_session_with_tokens` | 最上位の文だけが呼ぶ。`parse_with_clause` の結果でデータ変更 CTE を検出し、実行計画 `ValidatedDataModifyingCte` を返す（第 2 の検出器を作らない）。読み取り入口（`validate_sql_tokens`・cursor・COPY・CREATE VIEW・CTAS・EXPLAIN・サブクエリ内側）は従来どおり検証後に固定 `0A000` |
| `ParsedSql::DataModifyingCte` | `ParsedSql::Statement`（読み取り専用）に載せない。wire 側が非書き込みと誤判定して commit 後の失敗を包み損ねないため |
| `core/dm_cte_exec.rs` | 事前照合 → 副文を定義順に実行 → 台帳記録 → 主クエリの外側処理。副文は単独 DML と同じ `execute_*_returning_form` を通す |
| `view_buffered::plan_outer_parts` | 評価後射影形ビューと同じ外側処理（Describe と Execute が同じ入口） |

### 実行する形（ゲート）

検証段（書き込みトランザクションを開く前）で判定し、それ以外は固定 `0A000`（表は変わらない）。

| 形 | 扱い |
| ---- | ---- |
| データ変更 CTE が 1 つで、主クエリの FROM がその CTE（`RETURNING` あり） | 実行 |
| データ変更 CTE が 2 つ以上 | `0A000`（可視性と多重変更の順序は #1877） |
| 主クエリの FROM が実表・ビュー・SELECT 本文 CTE | `0A000`（主クエリの読み取りスナップショットは #1877） |
| `RETURNING` なしの副文を参照 | `0A000` |
| 一時表が対象 | `0A000`（`temp_route` が振り分け対象外として拒否。一時コアへ流さない） |

### トランザクション

- autocommit は `execute_subquery_dml_autocommit` と同じ文単位の暗黙トランザクションで実行する。副文ごとの
  autocommit では原子的にならず、`WriteTarget::InTxn` を直接使うと遅延 `FOREIGN KEY` 検査が抜けるため、
  `commit_implicit` を通す。明示・暗黙トランザクションは共有 write txn 上で実行し、書いた全表へ `mark_written` する
- 失敗（副文の制約違反・主クエリのエラー・行数上限超過）は txn の abort で全体が破棄される

### 台帳（文単位 1 エントリ）

- 文末の `USING OPERATION_ID '<id>'` を主クエリから切り離して受理する（副文内の指定は従来どおり `42601`）。
  `--require-operation-id` 構成での省略・`NULL` は検証段で `23502`
- 文ハッシュは `OpTag::DataModifyingCte`（= 29）で、送られた文のトークン列だけが材料（結果行の値は含めない）。
  再送は最初の副文を実行する前の事前照合で `23505`、別内容は `22023`
- 副文は `operation_id` を持たないため、副文の実行中だけ台帳モードを「省略可」へ切り替える
  （`SubstatementLedgerScope`。`Ledgered` のままだと副文が `23502` になる）。副文は台帳に触れない
- 台帳・世代の単位は「定義順で最初のデータ変更 CTE の対象表」に決定的に固定する。同じ `operation_id` を同表へ
  直接 DML で再送した場合は別ドメインのハッシュで `22023` になる

### 対象外（後続）

- 文開始時点スナップショットの可視性、同一行を複数の副文が変更する場合の順序、複数のデータ変更 CTE、
  主クエリが実表・ビュー・SELECT 本文 CTE を読む形（#1877）
- RLS・権限の境界テスト（#1878）。DML 単位の権限検査（SQL-40 相当）は現状実装がなく、本 Issue では副文が
  単独 DML と同じ経路を通ることだけを保証する
- 主クエリが DML の形、`INSERT ... SELECT`／`UPDATE ... FROM`／`DELETE ... USING` 本文、ビュー・一時表が対象の本文、
  `WITH RECURSIVE` 内のデータ変更、拡張クエリの `$n`、NoSQL 表層（従来どおり拒否）
