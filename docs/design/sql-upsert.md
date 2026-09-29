# UPSERT（`INSERT ... ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`）の設計判断

Issue #872・対象ビヘイビア: SQL-20（TASK-193 の UPSERT 部）。関連ポインタ: TABLE-12
（テナント名前空間キー）・RLS-9・RLS-10（他テナント存在情報の非漏えい）・
RECOVER-1〜3（`operation_id` 必須化・台帳）・RECOVER-10（台帳照合による再送判定）・
RECOVER-11(a)（複数行・UPSERT のハッシュ入力）・RECOVER-12（`23505` の `code` ラベル
区別。本 Issue の対象外）・SQL-16（複数行 `VALUES`）・SQL-17（`SET` の禁止列・次元検証）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
INSERT INTO <table> (<col>[, <col>]*) VALUES (<lit>[, <lit>]*)[, (<lit>[, <lit>]*)]*
ON CONFLICT (id) DO NOTHING
USING OPERATION_ID '<id>'
```

```
INSERT INTO <table> (<col>[, <col>]*) VALUES (<lit>[, <lit>]*)[, (<lit>[, <lit>]*)]*
ON CONFLICT (id) DO UPDATE SET <col> = (EXCLUDED.<col> | <lit>)[, ...]
USING OPERATION_ID '<id>'
```

- 対象列リストは `(id)`、または宣言済み `UNIQUE` 制約（単一列・複数列）の構成列
  集合と一致する列名の並びを受理する（TABLE-16、Issue #1074。「ON CONFLICT 対象の
  UNIQUE 制約列への拡張」節参照）。`ON CONSTRAINT` 形・PRIMARY KEY 宣言列・部分一意
  制約はいずれも対象外のまま `42601`。
- `ON CONFLICT` は複数行 `VALUES`（SQL-16、TASK-190）と併用でき、全行が同じ衝突分岐を
  共有する。
- `ON CONFLICT` はファイル形 `INSERT`（`path`/`body` 列指定。TASK-120）とは併用できない
  （`42601`）。
- `DO UPDATE SET` の右辺は `EXCLUDED.<col>`（新規挿入しようとした行の束縛済み値。
  スキーマ列名で参照する）またはリテラル（`UPDATE ... SET` と同じ表現）のみを受理する。
  式・関数呼び出し・他列参照は許可リスト外（`42601`）。
- `USING OPERATION_ID '<id>'` は `ON CONFLICT ...` の**後**に置く（前に置いた形は
  `42601`）。句の重複・`DO UPDATE SET` の後ろへの `WHERE` の付加もいずれも `42601`
  （`Parser::expect_end_of_statement` の余剰トークン判定に自然に落ちる）。
- `EXPLAIN INSERT ... ON CONFLICT ...` は既存の `EXPLAIN`（次トークンが `SELECT` で
  あることを要求する分岐）へ流れて `42601` になる（`INSERT`・`TRUNCATE`・`DELETE` と
  同じ経路。専用の判定は持たない）。

## 字句解析: `EXCLUDED.<col>` の `Token::QualifiedIdent`

`EXCLUDED.<col>` を表現するため、`sql::lexer::Token` へ `QualifiedIdent { qualifier,
name }` を追加した（**BREAKING CHANGE**）。空白を挟まず「識別子＋`.`＋識別子開始文字
（英字・`_`）」が続く場合のみ 2 語 1 トークンへまとめ、既存の数値小数点判定（`1.`・
`.5`・`1..2`）とは独立に扱う。`a.`（末尾 `.`）・`a. b`（空白を挟む）・`a.b.c`（3 段）は
いずれも従来どおり `LexError`（許可リスト外の `.` として拒否）のまま変わらない。予約語
（`Keyword`）は修飾子として使わせない（`EXCLUDED` はそもそも `Keyword` 化されていない
語であり、予約語直後の `.` を新たに受理対象へ広げる必要がないため）。

`sql::allowlist::Parser` は `qualifier` を `EXCLUDED` と大小無視で照合し、`ON CONFLICT
... SET` の右辺以外の位置に `QualifiedIdent` が現れた場合は `expect_ident` 系ヘルパーが
受理せず `42601` へ落とす（構文上その位置以外に出現できない）。

## 衝突判定スコープ: 物理キー `(tenant_id, id)` の所有（可視性ではない）

既存 DML 実行器（`tenant::update_row_unchecked`・`delete_row_impl`・
`truncate_table_unchecked`）はいずれも `(ctx.tenant_id(), id)` キー取得＋
`ctx.is_owner` の二重防御で判定しており、RLS 可視性（`PolicyContext::is_visible`）では
ない。`tenant::upsert_typed_rows_unchecked` も同じ規約に揃える。

- 可視集合で判定すると「自テナント所有だが ctx に不可視な行」が非衝突扱いになり、
  plain INSERT 経路で行 `id` 衝突の `23505`（`TenantWriteError::IdConflict`）になる
  ——「本構文では行 `id` 衝突の `23505` を出さない」という契約（下記「行制約由来
  `23505` が構造的に発生しないこと」節）と矛盾する。所有スコープなら構造的に
  `23505` が出ない。
- 物理キーは `(ctx.tenant_id(), id)` で既にテナント名前空間化されているため
  （TABLE-12）、他テナントの同一 `id` はキーが異なり**取得すらされず**、常に
  「非衝突＝新規挿入」として扱われる。他テナント行の存在で分岐するコードを一切
  持たない。
- RLS-11（read-your-writes。TASK-195）導入後の認証セッション `PolicyContext` は
  自テナント `Private` 行を可視とするため、wire／HTTP 経由では「可視集合 ＝ 所有集合」
  となり実質一致する。

## ON CONFLICT 対象の UNIQUE 制約列への拡張（TABLE-16、Issue #1074）

PR #1053（Issue #905・UNIQUE 制約）が対象外とした「UPSERT の衝突対象を UNIQUE 列に
すること」を実装する。関連ポインタ: TABLE-16・RECOVER-10〜12。

- **構文段階**（`sql::allowlist::parse_on_conflict_clause`）は対象列リストを `id` に
  限らず任意の識別子の並びとして受理する（列数上限は UNIQUE 制約と同じ
  `MAX_UNIQUE_CONSTRAINT_COLUMNS`（32）、重複列名は `42601`）。実在性・意味論的妥当性は
  検証しない。
- **束縛段階**（`sql::parser::resolve_conflict_target`）でスキーマと照合する:
  対象が `(id)` のみなら `BoundConflictTarget::RowId`（本 Issue 導入前と完全に同じ
  挙動）。それ以外は、対象列の**集合**が宣言済み `UNIQUE` 制約（`schema.
  unique_constraints()`）のいずれかの構成列集合と一致する場合にのみ
  `BoundConflictTarget::Unique(indices)` へ解決する。`indices` は**対象リストに
  書いた順ではなく解決した制約の宣言順**になる（`(a,b)` と `(b,a)` は同じ制約として
  同じ並びに解決される）。次はいずれも `42601`: `id` と他列の混在、未知列、大小文字
  違い、`PRIMARY KEY` 宣言列（`schema.primary_key()` は対象に含めない）、どの
  `UNIQUE` 制約とも一致しない列集合。
- **衝突判定**（`tenant::upsert_typed_rows_unchecked`）: `UpsertTarget::Unique` は
  書き込み**前**に対象テナントの既存行を 1 回だけ走査し（`constraint::
  scan_tenant_rows_by_unique_key`。`enforce_unique_keys_in_txn` と同じ
  `(tenant_id, 0)..=(tenant_id, u64::MAX)` の閉区間・可視性を問わない全行が母集合。
  RLS-9・RLS-10 (c)）、対象キー→既存行 id の対応表を作る。各 `VALUES` 行はこの表と
  照合し、一致すればその既存行 id に対して `DO NOTHING`／`DO UPDATE`（read-merge-
  write は `(id)` 対象と共有）を行い、一致しなければ `VALUES` の `id` で新規挿入する
  （同じ `id` を持つ既存行があれば通常どおり `23505`）。対象キーの正準化
  （`constraint::unique_key_from_values`）は書き込み時検査点（`key_bytes`／
  `decode_key_columns`）と同じ表現を共有し、NULL を含むキーは NULLS DISTINCT として
  常に非衝突（新規挿入）。
- **バッチ内対象キー重複**: `(id)` 対象のバッチ内 `id` 重複と同じ理由で、束縛時
  （write トランザクション開始前・決定的）に `22000` で拒否する（NULL を含むキーは
  対象外）。`tenant::upsert_typed_rows_unchecked` 側にも同じ判定の防御的二重検査を
  持つ。
- **ハッシュ（RECOVER-11(a)）**: `content_hash::for_typed_upsert` の action tag を
  UNIQUE 対象では `2`／`3`（`(id)` 対象の `0`／`1` とは別）にずらし、続けて対象列数・
  列名（解決した制約の宣言順）を連結する。`(id)` 対象のレイアウトはバイト単位で不変
  （golden テスト `for_typed_upsert_row_id_layout_is_golden` で固定）。

## 判定順序

1. 字句・構文（許可リスト外は `42601`）。
2. `operation_id` 必須化ガード（`23502`。カタログ照会・write トランザクション開始より
   前。既存 `validate_insert_tokens` の順序を維持）。
3. テーブル存在確認（`42P01`）。
4. 束縛: 禁止列（`id`/`tenant_id`/`visibility` の SET 対象化・`EXCLUDED` の参照元）→
   `42601`／未知列・型不一致・`EXCLUDED` 型不一致・非 nullable 列への NULL・SET 内
   重複・バッチ内 `id` 重複 → `22000`。
5. INDEX-4 上限（`54000`。複数行時に `RowBatch` 分岐と同一の判定を適用）。
6. write トランザクション内: 束縛時スキーマとの不一致（TOCTOU・`22000`）→ 次元検証。
7. **台帳照合（RECOVER-10）**: 同一内容再送 `23505`（`DuplicateOperationId`）／内容
   不一致 `22023`（`OperationIdContentMismatch`）。行の衝突分岐より**前**（
   `update_row_unchecked`・`delete_row_impl` と同じ先行契約。commit 済み操作の再送が
   行状態の変化に左右されず検出される）。
8. 行ごとの衝突分岐（`DO NOTHING`／`DO UPDATE`／新規 INSERT）。
9. 変更行があればテーブル世代 bump → commit（台帳エントリは変更ゼロでも commit）。

## 行制約由来 `23505` が構造的に発生しないこと

本構文で `23505` が返るのは**台帳由来（`DuplicateOperationId`）のみ**。行制約由来
（`TenantWriteError::IdConflict`）は上記の衝突判定スコープにより構造的に到達しない。
両者は `SqlSurfaceError` の**variant**で区別する（`error_format.rs` は Issue #1180 で
`23505` の `code` ラベルを分離済み。台帳由来は `ErrorClass::DuplicateOperationId`
（`DUPLICATE_OPERATION_ID`）、行制約由来は `UniqueViolation`（`UNIQUE_VIOLATION`）。
ERR-6・RECOVER-12・TASK-227）。

`crates/engine/tests/sql_upsert.rs::same_tenant_conflict_never_returns_row_constraint_id_conflict`
が、同一衝突が通常 `INSERT` では `23505`（行制約由来）、UPSERT（`DO NOTHING`）では
`INSERT 0 0`（成功）へ吸収されることを対照確認する。

## バッチ内 `id` 重複

`tenant::insert_typed_rows_unchecked` はバッチ内 `id` 重複を `TenantWriteError::
IdConflict`（`23505`）で拒否するが、UPSERT ではこの経路を使えない（上記のとおり
`23505` は台帳由来のみに限定する契約のため）。そのため**束縛時（write トランザクション
開始前・決定的）に `22000` で拒否**する（`sql::parser::bind_upsert_form`）。2 行目を
「1 行目への更新」と解釈しない（PostgreSQL も同一文内の二重変更を拒否する）。

`tenant::upsert_typed_rows_unchecked` 自体もバッチ内 `id` 重複を検出し
`TenantWriteError::IdConflict` を返す防御を持つが、これは呼び出し元（SQL 表層の
`bind_upsert_form`）が既に拒否済みのため通常は到達しない（`insert_typed_rows_
unchecked` と同じ「関数単体でも fail-closed を保つ」設計方針）。

## `DO UPDATE` の値の扱い（read-merge-write）

- 既存行を `storage::decode_row_for_key(ctx.tenant_id(), id, raw)`（キー↔ヘッダ tenant
  の整合検査〔TABLE-12〕を内包。不一致は fail-closed に拒否）＋
  `row_codec::decode_scalar_columns(&schema, &metadata)` で復元し、SET 対象列のみを
  新しい値で上書きする。
- SET で触れなかった列（`VECTOR` 列を含む）は既存値を保持する。既存行の `visibility`
  も保持する（`visibility` は SET の禁止列でもある。新規挿入行のみ `Visibility::
  Private` 固定）。
- 右辺は `EXCLUDED.<src>`（新規行の束縛済み値列インデックス参照。`src` の型が対象列と
  一致しない場合は束縛時に `22000`）と、リテラル（`UPDATE` の `bind_set_assignments`
  と同じ型検査）。`EXCLUDED.<src>` が対象行の列リストに含まれていなければ `Value::
  Null`（`src` が nullable なら許容、非 nullable な対象列に代入しようとすると束縛時
  `22000`）。

## テーブル世代

`DO UPDATE`／新規挿入が 1 行でもあれば `catalog::bump_table_generation_in_txn`
（`SqlArenaCache`・`VisibleBitmapCache`・`ScalarIndexCache`・`HnswIndexCache` の失効
源泉）。全行 `DO NOTHING` で変更ゼロの場合は**bump しない**（`DeleteRowOutcome::
NotFound` と同じ設計。内容が変わらないためキャッシュは有効のまま。台帳エントリのみ
commit する）。`TRUNCATE`（0 件でも常に bump）とは意図的に異なる。

## 内容照合ハッシュ（RECOVER-11(a)）

`recovery::content_hash::for_typed_upsert`（`OpTag::Upsert = 7`。既存 1〜6 とは独立の
専用タグ）: `push_u8(action)`（`0` = DO NOTHING、`1` = DO UPDATE）→ DO UPDATE のみ
割当数プレフィクス＋各割当（対象列名・種別 `0` = EXCLUDED（参照元の列名）／`1` =
リテラル値）→ `for_typed_insert_batch` と同一の行データレイアウト（件数プレフィクス＋
行ごとの `(id, visibility, embedding, 列数プレフィクス, 列名付きスカラー列)`）。

同一 `VALUES` でも plain `INSERT`／`DO NOTHING`／`DO UPDATE`／`SET` 内容差は必ず
異なるハッシュになる。単一行 UPSERT も `rows.len() == 1` としてこの複数行レイアウトを
使う（`for_typed_insert` へは委譲しない）——これにより、plain `INSERT` と同一
`operation_id` での UPSERT 再送は `OpTag` の違いにより機械的に内容不一致
（`22023`）として検出される。

## 応答

`sql::exec::InsertOutcome { rows_affected: inserted + updated, incremental: None }` を
`SqlOutcome::Insert` で返す（既存 `INSERT` と同じ型。wire-server の `CommandComplete`
タグ写像 `INSERT 0 <rows_affected>` は無変更）。`DO NOTHING` で全行衝突なら
`INSERT 0 0`。`rows_affected` は自テナント内の結果のみを表し、他テナント推定材料には
ならない（`DELETE` の `rows_affected` と同じ判断）。

## RLS-9 の検証方法

`crates/engine/tests/sql_upsert.rs`・`crates/wire-server/tests/wire_upsert.rs`
（`wire_upsert_response_is_byte_identical_for_other_tenant_row_and_nonexistent_id`）で、
他テナント保持 `id` への UPSERT（`DO NOTHING`）と未存在 `id` への UPSERT の応答
（`CommandComplete` の生バイト列）が完全に一致することを固定する。両者はいずれも
「非衝突＝新規挿入」として `INSERT 0 1` になる（`wire_delete_single_row.rs` と同じ
検証方式。`DELETE` の 0 行成功〔`DELETE 0`〕とは異なる値だが、同じ「存在情報を応答から
区別できない」という性質を UPSERT の文脈で確認する）。

## NoSQL 表層は対象外

`insert` op の未知キー拒否（`SchemaError::UnknownKey` → `UnsupportedSqlSyntax`）は
本 Issue では変更しない。`on_conflict` キーを NoSQL `insert` op へ規範化する対応は
spec 側「別途」の扱いのまま（コードでの固定テストも追加しない）。

## 対象外・申し送り

- `RETURNING` 句（別 Issue の担当）。
- `UNIQUE` 制約を衝突対象にする拡張（TABLE-16）は Issue #1074 で実装済み
  （「ON CONFLICT 対象の UNIQUE 制約列への拡張」節参照）。`PRIMARY KEY` 宣言列を
  対象にすること・`ON CONFLICT ON CONSTRAINT <name>`・部分一意制約の
  `ON CONFLICT` は引き続き対象外（`42601`）。
- `23505` の `code` ラベル分離（`DUPLICATE_OPERATION_ID` vs `UNIQUE_VIOLATION`）は
  Issue #1180 で実装済み。
- NoSQL 表層の UPSERT（`on_conflict` キーの規範化）。
- 3 クライアント統合ハーネス（層 B）への UPSERT ケース追加（層 A で契約を固定済み。
  層 B への追加は別 Issue の担当範囲）。
