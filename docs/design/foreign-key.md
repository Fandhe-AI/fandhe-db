# `FOREIGN KEY` 制約の設計判断

Issue #907・対象ビヘイビア: TABLE-17（TASK-205）。関連ポインタ: TABLE-12
（物理キー `(tenant_id, id)`）・TABLE-15（`DROP TABLE` の依存オブジェクト検査）・
TABLE-16（主キー・UNIQUE・単一検査点）・RLS-9・RLS-10 (c)（他テナントの存在情報の
非漏えい・可視性を問わない判定母集合）・ERR-6（新設 `wire_code` と HTTP 射影）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本
ドキュメントは本リポ側の実装判断・設計記録のみを扱う。

## 決定事項

| # | 決定 | 理由 |
| --- | ---- | ---- |
| D1 | 参照先列は、参照先テーブルの `id` 疑似列（物理キー）・宣言済み主キー・UNIQUE 制約のいずれかと**列集合**が一致すること。それ以外は `42830` | 一意性を保証しない列を参照先にすると、同値の参照先行の 1 行を削除しても残りが参照を満たし続ける等、`NO ACTION` の意味論が定まらない |
| D2 | 参照先列の省略（`REFERENCES <t>`）は参照先の主キー、未宣言なら `id` へ解決し、解決済みの列名をカタログへ永続化する | PostgreSQL と同じ規約。解決結果を永続化することで、後から参照先が変わっても宣言の意味が変わらない |
| D3 | 参照元列と参照先列の型は位置ごとに一致すること（型タグ＋パラメータ。ENUM は型名を含む）。`id` 参照の参照元列は `INTEGER`／`BIGINT`。不一致は `42830` | 参照先の照合を一意性検査と同じ型タグ付き正準キーで行うため、型が異なる組は常に違反になる（黙って常に失敗する宣言を受理しない） |
| D4 | SQL 表層 `CREATE TABLE` の列型へ `INTEGER`／`BIGINT` を追加（`NOT NULL`／`DEFAULT <数値>`／`UNIQUE`／`PRIMARY KEY` も受理） | `id` を参照する参照元列を SQL で宣言するための最小限の前提整備 |
| D5 | 参照動作は既定の `NO ACTION`（非遅延のため `RESTRICT` と同値）のみ。`ON DELETE`／`ON UPDATE` には `NO ACTION`／`RESTRICT` だけを受理し、`CASCADE`／`SET NULL`／`SET DEFAULT`・`MATCH`・`DEFERRABLE`・`CONSTRAINT <name>` 前置は `42601` | 対象外の動作を黙って既定動作へ丸めない（fail-closed） |
| D6 | NULL を含む値の組は検査しない（MATCH SIMPLE） | PostgreSQL の既定 |
| D7 | 検査は文単位・即時。台帳記録・行の書き込みの**後**、テーブル世代 bump・commit の**前**に同一 write トランザクション内で行う | 既存の制約検査（TABLE-16）と同じ位置。`operation_id` の再送判定（`23505`／`22023`）が本検査より優先される |
| D8 | 自己参照を受理する。循環参照は `CREATE TABLE` の時点で参照先が存在する必要があり、`ALTER TABLE ... ADD FOREIGN KEY` を持たないため、自己参照以外の循環は構造的に作れない | 自己参照は参照元＝参照先のスキーマで解決・検査でき、特別な経路を要さない |
| D9 | 参照先名がビュー・索引名なら `42809`、存在しなければ `42P01`。作成対象名の重複（`42P07`）はそれらより先に判定する | テーブル・ビュー・索引は名前空間を共有する（`CREATE TABLE` の既存判定と同じ順序） |
| D10 | 参照先テーブルの `DROP TABLE` は他テーブルから参照されていれば `2BP01`（データの有無を問わずカタログのみで判定）。自己参照は依存に数えない | TABLE-15 |
| D11 | 参照元列の `DROP COLUMN` は `DependentObjectsStillExist` で拒否。参照先側の列は主キー・UNIQUE 構成列（既存の検査で拒否済み）か `id`（予約列）に限られる | 制約を黙って消す暗黙 cascade を作らない（`DROP COLUMN` の話。UNIQUE 制約自体の明示 `DROP CONSTRAINT` は Issue #1067 で追加済みで、参照されている UNIQUE の DROP は `2BP01` で拒否する。詳細は [alter-table-unique-constraint.md](./alter-table-unique-constraint.md) D7 参照） |
| D12 | 宣言面は SQL 表層の `CREATE TABLE` のみ（`ALTER TABLE ... ADD COLUMN ... REFERENCES` は `42601`）。Rust API の `TableSchema::with_foreign_keys` は `pub(crate)` | 後付けの宣言は既存の全テナント行の検証を要し別設計になる |

## 構文

```
CREATE TABLE <table> (
  <col> <type> [<列制約>]* [REFERENCES <parent> [(<pcol>[, <pcol>]*)] [<参照動作>]*]
  | FOREIGN KEY (<col>[, <col>]*) REFERENCES <parent> [(<pcol>[, <pcol>]*)] [<参照動作>]*
  [, ...]
) [;]

<参照動作> ::= ON DELETE (NO ACTION | RESTRICT) | ON UPDATE (NO ACTION | RESTRICT)
```

- 列制約 `REFERENCES` は `PRIMARY KEY` の後ろ・`CHECK` の前に高々 1 個置ける。
- 表制約は列リスト中の任意の位置に置ける（列数上限の判定対象外。`PRIMARY KEY`／
  `UNIQUE`／`CHECK` の表制約と同じ位置非依存の判定）。参照元列の実在は列リスト
  全体の構文判定後に判定する（未宣言列・`id` は `42601`）。
- 同一リスト内の列名重複は `42701`、件数・列数の上限（32）超過は `54000`。
- `REFERENCES`／`FOREIGN`／`ON`／`NO`／`ACTION`／`RESTRICT` は `lexer::Keyword` へ
  含めない文脈的キーワード。

## 永続化: カタログ v8（`TableSchema` の拡張）

`TableSchema` に `foreign_keys: Vec<ForeignKeyDef>` を追加し、`FOREIGN KEY` を
1 件以上持つスキーマはカタログ v8 で永続化する。v8 は v7 の上位集合で、`pk:` 行・
6 フィールドの列行・`uniq:` セクション（0 件可）・`checks:` セクション（v8 に限り
0 件可）の後ろに `fks:<n>`（`n >= 1`）と `n` 行の
`fk:<col1,col2,...>:<parent_table>:<pcol1,pcol2,...>` を追記する。`FOREIGN KEY` を
持たないスキーマは従来どおり v2〜v7 のままバイト列を変えない（v2〜v8 は互いに
排他な正規形）。

- decode は構造（件数・フィールド数・識別子形状・重複・列数一致・`id` 単独）を
  共有パーサー（`parse_foreign_key_section`）で検証し、参照元列の実在・型・
  自己参照の照合を `validate_schema` で再検証する。破損は `CorruptSchema`。
- `DROP TYPE` の依存判定用の軽量パーサー（`catalog_value_references_enum_type`）も
  v8 を認識し、同じ共有パーサーで構造を検証する（decode より緩くならない）。
- 参照先側の逆引き（このテーブルを参照している宣言の列挙。
  `referencing_foreign_keys_in_txn`）はカタログ全走査だが、v8 は `FOREIGN KEY` を
  持つスキーマ専用の版のため、値の 1 行目が `v8` のエントリだけを decode する。

以前の試作では専用 redb テーブル `foreign_keys` に分離していたが、主キー・UNIQUE・
`CHECK` がいずれも `TableSchema` とカタログ版で表現されるようになったため、
それらと同じ機構（単一のスキーマ値・版による正規形）へ揃えた。

## 検査の単一実装（`constraint.rs`）

TABLE-16 と同じ単一検査点に置く（表層ごとに検査を持たない）。

- 参照元側: `constraint::enforce_row_constraints_in_txn` の末尾（`CHECK` → 一意性 →
  `FOREIGN KEY` の順）。書き込んだ各行を同一 write トランザクション内で読み戻し
  （UPSERT の `DO UPDATE`・`UPDATE` の SET 適用後の最終値。SET で触れない既存値も
  含む）、値の組が参照先に存在することを確かめる。`id` 参照は物理キーの点照会、
  列参照は参照先のテナント範囲を走査し、必要な値の組がすべて見つかった時点で
  打ち切る。
- 参照先側: `constraint::enforce_referencing_rows_in_txn`。削除・更新・`TRUNCATE`・
  置換の後に、このテーブルを参照先とする各宣言について、参照元の同一テナント
  全行の値の組が変更後の参照先にすべて存在することを確かめる（事後状態の検証）。
  削除前の値を保持する必要がなく、自己参照・複数行の同時削除・置換のいずれにも
  同一の実装で効く。`ALTER TABLE ... ADD FOREIGN KEY` を持たないため各文の開始時点で
  参照整合性は常に成立しており、この検証は `NO ACTION` と等価になる。
- 更新（`UPDATE`・UPSERT の `DO UPDATE`・全列置換）で主キー・UNIQUE 構成列に
  触れない場合は、参照先キーが変わり得ないためカタログの逆引きも行わない
  （`id` は予約列で `SET` できない）。
- 同一文・同一明示トランザクション内で先に書いた行（自己参照で同じ文が書いた行を
  含む）は、redb の write トランザクションが自身の未 commit の書き込みを読める
  ため母集合に含まれる（`BEGIN; INSERT 親; INSERT 子; COMMIT` が成立する）。
- エラーは `TenantWriteError::ForeignKeyViolation` 単一 variant（参照元側・参照先側の
  いずれの原因も区別しない固定文言）。

### 計算量（既知の制約）

永続索引は導入しない。参照先側の検査は参照元の同一テナントの行数に比例し、
列参照の参照元側の検査は参照先の同一テナントの行数に比例する（一意性検査
〔`docs/design/unique-constraint.md`〕と同じ位置づけ。永続索引化は将来の別課題）。
走査上限（`tenant::MAX_SCANNED_ROWS`）は一意性検査と同じ理由で継承しない
（上限を超える行数を保有するテナントが一切書き込めなくなる fail-closed 過ぎる
制約になるため）。

## テナント境界（RLS-9・RLS-10 (c)）

- 判定の母集合は同一テナントが所有する**全行**（`Public`／`Private` を問わない。
  RLS 可視集合ではない）。可視スナップショット由来の二次索引・世代整合キャッシュは
  流用せず、生の redb 走査で判定する。
- 走査・点照会のキーはサーバー側導出テナント（`ctx.tenant_id()`）の物理キー空間
  `(tenant, 0)..=(tenant, u64::MAX)` のみで組み立て、他テナントのキー空間に触れる
  分岐を持たない。他テナントだけが持つ参照先は「不在」と同じ結果になり、成否・
  `wire_code`・文言のいずれからも区別できない（`table17_foreign_key.rs` の
  `violation_response_does_not_reveal_other_tenant_parent_rows` で固定）。
- 他テナントの参照元行は参照先の削除・`TRUNCATE` を阻止しない
  （`other_tenant_referencing_rows_do_not_block_parent_changes`）。
- 単一行 `DELETE` で対象行が不在・他テナント所有（`0` 行）の場合は参照先側の検査
  自体を行わない（他テナントの行の有無で処理経路が分岐しない）。
- `DROP TABLE` の `2BP01` はカタログ情報のみで判定し、テナントデータを参照しない。

## 書き込み経路への結線

参照元側（`enforce_row_constraints_in_txn` 経由。既存の一意性・`CHECK` 検査と同じ
呼び出し点）: `insert_row_unchecked`・`insert_rows_unchecked`・
`insert_typed_row_unchecked`・`insert_typed_rows_unchecked`・
`upsert_typed_rows_unchecked`・`update_row_unchecked`・
`update_row_columns_unchecked`・`update_rows_where_unchecked`・
`replace_typed_rows_by_text_key`（新規チャンク行）。

参照先側（`enforce_referencing_rows_in_txn`）: `delete_row_impl`（単一行 DELETE・
`RETURNING` を包含。実際に削除した場合のみ）・`delete_rows_where_unchecked`・
`truncate_table_unchecked`（明示トランザクション内の `TRUNCATE` を含む）・
`update_row_unchecked`・`update_row_columns_unchecked`・
`update_rows_where_unchecked`・`upsert_typed_rows_unchecked`（`DO UPDATE`）・
`replace_typed_rows_by_text_key`（置換で消える旧行）。

ファイル形 `INSERT` の違反は `sql::exec::map_incremental_error` を経由するため、
行形の `map_write_error` と同じく `23503` への写像アームを持つ（`_` 節の `XX000` へ
丸めない）。

`catalog.rs` の生書き込み API（`#[cfg(test)]` 限定・production では到達不能）は
一意性検査と同じくこの検査点を経由しない（既知のギャップ）。

## エラー・公開 API（BREAKING CHANGE）

- 新設 `wire_code`: `23503`（`FOREIGN_KEY_VIOLATION`。HTTP 409）・`42830`
  （`INVALID_FOREIGN_KEY`。HTTP 400）。`2BP01`・`42809`・`42P01` は既存分類を再利用。
- `ErrorClass::ForeignKeyViolation`・`InvalidForeignKey`（32 → 34 分類）
- `CatalogError::InvalidForeignKey`・`TenantWriteError::ForeignKeyViolation`・
  `SqlSurfaceError::ForeignKeyViolation`／`InvalidForeignKey`
- `ValidatedCreateTable.foreign_keys`（公開フィールド追加）・
  `catalog::ForeignKeyDef`（公開型）・`TableSchema::foreign_keys()`
- SQL 表層 `CREATE TABLE` が `INTEGER`／`BIGINT` 列を受理するようになった

いずれの enum も `#[non_exhaustive]` ではない
（`docs/design/error-enum-non-exhaustive-policy.md`）ため、外部クレートの網羅
`match` を破壊しうる破壊的変更として扱う。

## NoSQL（HTTP）表層

`op` 語彙に DDL が無いため `42830` は実要求から到達しない。宣言済みテーブルへの
`insert`／`update`／`delete` op は同じ単一検査点を通るため `23503`／409 は到達する
（`crates/wire-server/docs/nosql-api.md`・`crates/wire-server/tests/
err4_http_projection.rs` の `err4_f_foreign_key_violation_reachable_via_*`）。

## 対象外・後続候補

- `ALTER TABLE ... ADD/DROP CONSTRAINT FOREIGN KEY`（既存行の全テナント検証が必要）
- `ON DELETE CASCADE`／`SET NULL`／`SET DEFAULT`、遅延制約（`DEFERRABLE`）、
  `MATCH FULL`、制約名（`CONSTRAINT <name> FOREIGN KEY`）
- 参照先側・列参照の検査の索引化（現状はテナント範囲の線形走査）
- 明示トランザクション内の `UPDATE`／`DELETE`（明示トランザクション自体が未対応。
  `docs/design/explicit-transaction.md`）
- NoSQL 表層の DDL op での宣言
