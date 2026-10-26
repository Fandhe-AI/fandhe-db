# JOIN ... USING（Issue #1826・SQL-60 (1)・親 #1825）

## ステータス

Accepted・実装済み。

## ポインタ

- spec: `docs/spec/04-behavior/sql-surface.md` SQL-60 (1)（期待値は PostgreSQL 17 と同じ）
- 前提: `docs/design/inner-join.md`・`docs/design/outer-join.md`・`docs/design/multi-way-join.md`
- 後続: `NATURAL JOIN`（Issue #1827）は束縛時に導出した列名列を本 Issue の USING 束縛へ流し込む

## 目的

`[INNER|LEFT|RIGHT|FULL [OUTER]] JOIN ... USING (c1, c2, ...)` を受理し、(a) 等価 ON
（ハッシュ結合キー）へ脱糖し、(b) 結合列を 1 列に統合する出力規則を PostgreSQL 17 に揃える。

## 設計

### 構文段（`sql::allowlist`）

`JoinStep` の結合条件を `JoinCondition::On(..) | Using(Vec<String>)` にした。`USING` は直後が `(`
のときだけ結合条件として扱い、それ以外は従来どおり `ON` を要求して `42601`。列リストは
空・修飾名・式を `42601`、`MAX_JOIN_CONDITIONS`（8）超過を `Vec` へ積む前に `54000` とする。
重複・存在・型の判定は PostgreSQL と同じ列ごとの順序にするため束縛段が行う。

### 束縛段（`sql::join::using`・`sql::join::plan`）

- 列ごとに USING リスト順で判定する: 重複 `42701` → 左入力に無い `42703`／2 つ以上 `42702` →
  右に無い `42703` → 型。`key_class` が異なれば ON と同じ `42804`。同クラスでも `ColumnType` が
  完全一致しない組（INTEGER と BIGINT 等）は統合列の型・セル表現を一意に定められないため
  安全側で `0A000`。
- 統合列（`MergedColumn`）は値のソース列の並びを持つ: INNER／LEFT は左、RIGHT は右、FULL は
  `COALESCE(左, 右)`（左ソース ++ 右ソース）。
- 可視列の並び（`SELECT *` の列順かつ非修飾名の候補集合）は「統合列（USING 順）→ 左の残り →
  右の残り」。連鎖では段ごとに再帰適用する。統合に関与した元の列は非修飾の候補から外れ、
  修飾付き `a.x`・`b.x` は元の列を指す。統合されていない同名列が複数見えるときは `42702`。
- 列解決は `JoinScope` に一本化した。USING を 1 度も使わない JOIN では `BindingScope` へ完全
  委譲し、既存の挙動は変わらない。単一ソースの統合列は元の列へ解決されるため、WHERE の
  プッシュダウン・外部結合の簡約は「LEFT は左列・RIGHT は右列」という値規則と一致する。

### 実行段

FULL の統合列だけが複数ソースを持つ。`OutputColumn`／`PlainOrder` に代替ソース
（`fallbacks`）を追加し、先頭ソースが NULL 補完または NULL セルのとき最初の非 NULL セルを採る
（`residual::cell_with_fallbacks`。行は複製せず借用で選ぶため、複製前のバイト見積もり計上の
順序は変わらない）。ハッシュ結合（`exec.rs`）は無変更で、キーは常に単一ソースの列を指す。

## スコープと対象外（fail-closed）

- FULL の統合列は「投影（`*` 展開・明示列）」と「非集計形の `ORDER BY`（列名・序数）」だけが
  値を組み立てられる。WHERE・GROUP BY・集計の引数・後続段の結合キー（後続の USING／ON）からの
  非修飾参照は `0A000`（固定文言。列名・行データを含めない）で拒否する。
- `USING (...) AS <alias>` は未対応（`42601`）。`NATURAL JOIN` は従来どおり `42601`（Issue #1827）。
- 同じ型クラスで型が異なる USING 列の共通型への寄せ、FULL 統合列の WHERE 等での参照、
  `USING ... AS` は後続課題。

## セキュリティ

- 各 relation の側スキャンは既存の `execute_scan_with_budget`（呼び出しセッションの
  `PolicyContext`）を無変更で通すため RLS の暗黙適用を継承する。統合列は可視行のセルだけから
  作られ、他テナント行は結合キー・NULL 補完・統合値のいずれにも現れない（`tests/sql60_join_using.rs`）。
- 拒否判定はスキーマと文だけで決まりデータ件数に依存しない（`tests/rls10_relational_paths.rs`）。
- 列数・relation 数・入出力行数・共有バイト予算の上限は既存のまま維持する。

## テスト

`crates/engine/tests/sql60_join_using.rs`（INNER／LEFT／RIGHT／FULL・複数列・連鎖・ON との混在・
WHERE・集計・疑似列 `id`・拒否形・上限・テナント境界）と `sql::allowlist` の構文段ユニットテスト。
