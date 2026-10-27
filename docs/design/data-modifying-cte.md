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
