# 実行計画の複数テーブル対応基盤（Issue #924・TASK-212・SQL-28・RLS-10）

## ステータス

Accepted（基盤のみ実装済み。許可リストの開放・`EngineCore` への結線は Issue #925 以降に申し送り）。

## ポインタ

- spec: `docs/spec/05-tasks.md` TASK-212・`docs/spec/04-behavior/sql-surface.md` SQL-28（検討中）・
  `docs/spec/04-behavior/rls.md` RLS-10・`docs/spec/04-behavior/error-format.md` ERR-6（`42702`）
- 関連 Issue: #925（INNER JOIN）・#926（OUTER JOIN）・#927〜#930・#931（RLS 境界検証）

spec 本文はここへ転記しない（`.claude/rules/spec-confidentiality.md`）。以下は本リポの
実装既定値・設計判断の記録。

## 背景・目的

既存の SQL 表層は「1 文 = 1 テーブル」前提で閉じている。

- 許可リスト（`sql::allowlist::parse_select_shape` ほか）は `FROM <ident>` の単一テーブルだけを
  受理する。JOIN・カンマ区切り FROM は `42601`
- 束縛（`sql::parser`）は単一の `TableSchema` に対して非修飾の列名を解決する
- 可視スナップショット・世代整合キャッシュ（`sql::arena_cache`・`sql::sparse_cache`・
  `sql::scalar_index`・`sql::visible_cache`・`sql::hnsw_cache`）はどれも `(table, ctx)` と
  テーブル単位の世代をキーにしている

本 Issue では、後続の JOIN 実装が載る**基盤**のみを用意する。

## スコープの線引き

- **SQL テキスト表層は変えない**: 許可リストは JOIN・複数 FROM・修飾列 SELECT を引き続き
  `42601` で拒否する（`rejects_join`・`rejects_multiple_from_tables` は無変更で green）。
  許可リストの開放は #925 の担当
- **既存の単一テーブル経路は触らない**: `parser`／`exec`／`scan`／`aggregate`／`group_by`、
  および既存 5 キャッシュは無変更（`git diff origin/main` で確認）
- **新しい基盤は `pub mod` として公開する**: `EngineCore` への結線（キャッシュフィールドの
  追加・実行経路からの呼び出し）は #925 に回す。本番経路から呼ばれない `pub(crate)` 項目は
  `-D warnings` の dead_code に抵触するため、新型・新関数は `pub` にし、結合テストは
  `crates/engine/tests/`（`sql_scan_public_api.rs` と同じ流儀）に置く
- **FROM 参照先がビューの場合は対象外**: resolver は呼び出し元が解決済みの `TableSchema` を
  受け取る。ビュー展開は #928 以降

## 新設モジュール

| モジュール | 役割 |
| --- | --- |
| `sql::relation`（`pub mod`） | 束縛スコープ: `TableRef`・`ColumnRef`・`ColumnSlot`・`ResolvedColumn`・`BindingScope`・`MAX_TABLE_REFS`、明示トランザクションの書き込み済みテーブル検査ヘルパー `ensure_relations_not_written` |
| `sql::generation_key`（`pub mod`） | 複数テーブル世代整合キー `TableGenerationKey`、汎用の fail-closed 世代整合キャッシュ `GenerationKeyedCache<V>`（`ApproxHeapBytes` trait を実装する値型を保持） |
| `sql::relation_snapshot`（`pub mod`） | テーブル単位の RLS 可視スナップショット `RelationSnapshot`、複数テーブル束 `MultiRelationSnapshot`、`RelationSnapshotCache`、`resolve_relation_snapshots` |

## 設計判断

### `BindingScope` の検証順序（決定的）

1. 参照数を `1..=MAX_TABLE_REFS` で検証する（`Vec` 確保前。超過は `54000`）
2. 公開名（`TableRef::exposed_name`。別名があれば別名、無ければテーブル名）の重複を検出する
3. 通過したらスコープを確定する

列解決:

- 修飾ありは、修飾子が公開名に一致する参照が無ければ `42P01`（カタログを照会しないため
  存在オラクルにならない）。一致した参照内で列（`id` 疑似列を含む）を探し、無ければ `22000`
- 修飾なしは全参照を左から走査し、ヒット 2 件以上で `42702`（`AmbiguousColumn`）、0 件で
  `22000`。`id` は全参照が持つ疑似列のため、参照が 2 つ以上あれば非修飾 `id` は常に `42702`
- 単一参照スコープの非修飾解決は、実カラムを疑似列 `id` より優先する既存の単一テーブル束縛
  （`sql::parser`）と同じ添字を返す（`tests/multi_relation_binding.rs` の等価性テストで固定）

### `MAX_TABLE_REFS = 8`（実装既定値）

本リポ独自の DoS 対策定数（`.claude/rules/security.md`「無制限リソース確保」対応）。
spec に数値基準は無く、後続の JOIN 実装（#925 以降）が要求次第で見直せるよう `pub const` で
公開する。

### `42712`（相関名重複）を採用しない

ERR-6 に相関名重複専用の SQLSTATE 行が無いため、公開名重複は新規分類を増やさず既存の
`UnsupportedSyntax`（`42601`）へ fail-closed に倒す。JOIN 開放時に必要になれば #925 で
再検討する。

### `TableGenerationKey` / `GenerationKeyedCache<V>`

`SqlArenaCache`（Issue #363）の fail-closed 契約（`lookup`/`insert` の非対称・世代不一致時の
破棄条件・容量管理の手順）を、複数テーブルキーへ一般化して一度だけ実装する。単一テーブルなら
鍵の長さが 1 になるだけで、既存キャッシュと同じ意味になる。

`insert` の世代不一致による一括破棄は「挿入キーのテーブルのいずれかを含み、かつそのテーブルの
世代が挿入キーと食い違うエントリ」に限る（`TableGenerationKey::involves`）。無関係なテーブル
のみのエントリを巻き添えにしない。

### `RelationSnapshot` が `id` ではなく `(tenant_id, id)` を保持する理由

物理キーは `(tenant_id, id)`（TABLE-12）で、1 つの `PolicyContext` が複数テナントの `Public`
行を見得るため `id` 単独では行を一意に識別できない。既存の `sql::visible_cache::VisibleSnapshot`
（Issue #478、`id` のみ）は集計（`COUNT`/`SUM` 等、値の突き合わせを要しない）専用のため
再利用せず、複数テーブルの結果を突き合わせる後続タスクに向けて新型として定義する。

### 既存 5 キャッシュを `GenerationKeyedCache` へ移行しない

性能不変（受け入れ条件 4）を優先し、本 Issue では見送る。単一テーブル専用キャッシュは
鍵の長さが常に 1 のため意味論上の差分は無いが、移行によるリグレッションリスクを避ける。
必要になれば別 Issue で扱う。

## セキュリティ考慮（OWASP Top 10 観点）

- **A01 アクセス制御**: `resolve_relation_snapshots` はサーバー導出の `PolicyContext` のみを
  受け取る。ヘッダ判定 → `is_visible` → `verify_row_key_tenant` → dim/metadata 構造検証の順序
  を守り、不可視行は本体をデコードしない。キャッシュは ctx の完全一致でしか共有しない
- **A03 インジェクション**: 識別子は構造化された型（`TableRef`/`ColumnRef`）で扱い、SQL 文字列
  を組み立てない
- **A04 不安全な設計（DoS）**: 参照数（`MAX_TABLE_REFS`）・可視行数（`arena::MAX_ARENA_ROWS`）・
  キャッシュ容量を確保前に検証する
- **A05 情報漏えい**: `42702` の文言には列名のみを含め、候補テーブルを列挙しない。走査・破損
  エラーは固定文言の `XX000`（ストレージ詳細を含めない）

## 対象外（Issue は起票しない。申し送りのみ）

- 許可リストでの JOIN・修飾列・別名の受理、`EngineCore` への `RelationSnapshotCache` 保持と
  実行経路への結線、中間結果行数上限、結合アルゴリズムの ADR → #925／#926
- FROM 参照先のビュー展開 → #928 以降
- `docs/spec/05-tasks.md` の TASK-212 状況更新は spec リポ側の作業
