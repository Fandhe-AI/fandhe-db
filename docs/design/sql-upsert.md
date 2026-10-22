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

- 対象列リストは `(id)`、または宣言済み `UNIQUE` 制約・`PRIMARY KEY`（単一列・
  複数列）の構成列集合と一致する列名の並びを受理する（TABLE-16、Issue #1074・
  SQL-49、Issue #1707。「ON CONFLICT 対象の UNIQUE 制約列への拡張」節・
  「Issue #1707 追記」節参照）。どれとも一致しない対象は `42P10`。`ON CONSTRAINT` 形は Issue #1737 で対応（末尾の追記参照）。
  式索引・部分索引の推論形は対象外のまま `42601`。対象を省略した `ON CONFLICT DO NOTHING`
  は Issue #1736 で受理する（省略した `DO UPDATE` は `42601`。「Issue #1736 追記」節参照）。
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

## 衝突検出スコープ: 物理キー `(tenant_id, id)` の所有（書き込み対象は所有 ∩ 可視）

> **改訂注記（Issue #1251・#1254）**: 衝突の**検出**（どの既存行と衝突するか）は
> 所有スコープのまま（`23505` を構造的に出さない契約の維持）。検出した既存行が可視集合外
> の場合の分岐（`DO NOTHING` はスキップ・`DO UPDATE` は `42501`）は
> `docs/design/sql-returning.md`「UPSERT の衝突先が不可視の場合」節で確定し、#1254 で
> `tenant::upsert_typed_rows_impl` へ実装済みである。

**契約**: 衝突の**検出**は所有スコープ（`(ctx.tenant_id(), id)` キー取得または UNIQUE
事前走査）、**書き込み対象**は所有 ∩ 可視。衝突先の判定はヘッダ専用デコード
（`decode_row_tenant_and_visibility`）で行い、`Absent`（新規挿入）／`Visible`／`Invisible`
の 3 状態に分ける。ヘッダのデコード失敗・ヘッダ tenant 不一致は `Invisible` とみなし、
検出済みの衝突を新規挿入へ落とさない（`23505` を出さない）。`DO NOTHING` は可視・不可視とも
本体を読まずスキップし、`DO UPDATE` は可視のときだけ本体をデコードして read-merge-write
する。不可視の衝突先を更新してはならない。

他の DML 実行器の対象選定は経路ごとに異なる。SQL 経路の `UPDATE`／`DELETE`
（`delete_row_impl` の `DeleteScope::OwnedVisible` を含む）は #1253 で「所有かつ可視」
（`ctx.is_owner && ctx.is_visible`）へ揃え済みである。Rust の行 API
（`tenant::update_row_unchecked`・`delete_row_unchecked`）と `truncate_tables_unchecked` は
従来どおり `(ctx.tenant_id(), id)` キー取得＋`ctx.is_owner` の二重防御のみで判定し、
可視性は問わない（Rust API の契約は変えない）。

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
  同じ並びに解決される）。Issue #1707 以降は `PRIMARY KEY` 宣言列も対象に含め、次は
  いずれも `42P10`: `id` と他列の混在、未知列、大小文字違い、どの `UNIQUE` 制約・
  `PRIMARY KEY` とも一致しない列集合（「Issue #1707 追記」節参照）。
- **衝突判定**（`tenant::upsert_typed_rows_unchecked`）: `UpsertTarget::Unique` は
  書き込み**前**に対象テナントの既存行を 1 回だけ走査し（`constraint::
  scan_tenant_rows_by_unique_key`。`enforce_unique_keys_in_txn` と同じ
  `(tenant_id, 0)..=(tenant_id, u64::MAX)` の閉区間・可視性を問わない全行が母集合。
  RLS-9・RLS-10 (c)）、対象キー→既存行 id の対応表を作る（この事前走査の母集合は
  所有行全体のまま不変。衝突検出に必要なキー列のみを可視性を問わず読み、可視行のキー列
  破損は fail-closed の `XX000`、不可視行のキー列破損は永続索引の逆引きからキーを補完して衝突先に含め（件数上限なし）、補完できない場合は対象キーに依らず一様に `XX000` で拒否する。本体は読まない。契約は `sql-returning.md`「書き込み対象の選定スコープ」節。一致した既存行の可視性で `DO NOTHING`／`DO UPDATE` の分岐が
  変わる点は `sql-returning.md`「UPSERT の衝突先が不可視の場合」節）。各 `VALUES` 行はこの表と
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

本構文で arbiter（行 `id` の `TenantWriteError::IdConflict`）由来の `23505` は、上記の
衝突判定スコープにより構造的に到達しない。arbiter 以外の PK／UNIQUE 制約の違反
（`DO UPDATE` の結果によるもの、Issue #1708 参照）は行制約由来の `UniqueViolation` として
`23505` になる。台帳由来（`DuplicateOperationId`）と合わせ、`23505` は 2 分類となる。
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

- `RETURNING` 句は Issue #1182 で結線済み（`docs/design/sql-returning.md` 参照）。
- `UNIQUE` 制約を衝突対象にする拡張（TABLE-16）は Issue #1074 で実装済み
  （「ON CONFLICT 対象の UNIQUE 制約列への拡張」節参照）。`PRIMARY KEY` 宣言列を
  対象にすることは Issue #1707 で実装済み（「Issue #1707 追記」節参照）。
  部分一意制約の `ON CONFLICT` は引き続き対象外
  （`42601`）。対象省略の `DO NOTHING` は Issue #1736 で実装済み（「Issue #1736 追記」節参照）。
- `23505` の `code` ラベル分離（`DUPLICATE_OPERATION_ID` vs `UNIQUE_VIOLATION`）は
  Issue #1180 で実装済み。
- NoSQL 表層の UPSERT（`on_conflict` キーの規範化）。
- 3 クライアント統合ハーネス（層 B）への UPSERT ケース追加（層 A で契約を固定済み。
  層 B への追加は別 Issue の担当範囲）。

## Issue #1707 追記: PRIMARY KEY 列を衝突対象にする（SQL-49 ポインタ）

- **解決**: `resolve_conflict_target` は `UNIQUE` 制約を宣言順に先に照合し、一致しなければ
  `schema.primary_key()` の列集合と照合して `BoundConflictTarget::Unique(indices)` を返す。
  新しい variant は作らない（実行時の機構は UNIQUE 対象とバイト単位で同じ）。複合 PK は
  列順を問わず集合が一致すれば解決する。
- **UNIQUE 優先の理由**: 既に受理していた文の解決結果と `content_hash::for_typed_upsert`
  （解決した列名の並びを含む）を変えないため。
- **`indices` は PK の宣言順**: 永続索引の序数照合
  （`constraint::unique_index::recover_forward_keys_for_rows`）が `key_specs` の宣言順
  `indices` との完全一致で序数を探すため、書いた順やソート順にすると不可視の破損行の補完が
  効かなくなる。
- **SQLSTATE の変更（破壊的）**: 束縛段の「一致なし」は `42601` から `42P10`
  （`SqlSurfaceError::InvalidColumnReference`、`ErrorClass::InvalidColumnReference`。
  PostgreSQL と同じコード。固定文言で列名・値を含めない）へ変更した。対象は、どの制約とも
  一致しない列集合（部分集合・上位集合を含む）、`id` と他列の混在、未知列・大小文字違い。
  未知列は PostgreSQL では `42703` だが、本 Issue は「一致なし」の `42P10` のみを定めるため
  別分岐は設けていない（差異として記録。寄せるかは別途判断）。
- **`42601` のまま**: 構文段（`parse_on_conflict_clause`）で拒否する形——式索引推論
  `((<式>))`、部分索引推論 `(<列>) WHERE ...`、空リスト、
  重複列、列数超過、ファイル形 `INSERT` との併用。
- **NULL**: PK 構成列は NOT NULL。`VALUES` の NULL は衝突せず新規挿入分岐へ進み `23502`
  になる（結合テストで固定）。既存行側の PK 列は NOT NULL のため事前走査の
  `NullPolicy::Skip` は実害がない。
- **テナント境界**: 事前走査は自テナント所有の全行が母集合（既存の UNIQUE 対象と同一）。
  他テナントの同じ PK 値は非衝突、自テナントの不可視行との衝突は `DO NOTHING` でスキップ。
- **#1708 との境界**: PK 対象の `DO UPDATE` は既存の Unique 用経路をそのまま通る。
  `EXCLUDED` の網羅、PK 列自体の更新と `23505`、再送不変性は #1708 で網羅済み（次節）。

## Issue #1708 追記: PK 列指定の DO UPDATE（SQL-49 (3)(5) ポインタ）

実装変更は無く、既存経路（#1707 の解決結果 → Unique 用経路）の振る舞いを
`crates/engine/tests/table16_primary_key.rs` の結合テストで固定した。

- **`EXCLUDED` の反映**: 単一・複合 PK とも衝突した既存行の `id` のまま更新され、`VALUES` 側の
  `id` の行は作られない。複合 PK は対象列の書き順に依らない。
- **PK 列自体の SET**: リテラル・`EXCLUDED.<PK 列>` とも受理する（束縛が拒否するのは
  `id`／`tenant_id`／`visibility` のみ）。自分自身の値への SET は自己衝突にならない。
  変更後は永続キー索引の旧キーが解放され、新キーが以後の衝突対象になる。
- **`23505` の分類**: 更新結果が他行の PK と衝突（複数行が同じ値へ収束する場合を含む）すると
  行制約由来の `UniqueViolation` で拒否され、行・台帳とも副作用は残らない。台帳由来の
  `DuplicateOperationId` とは `error_class` で区別して検証している。
- **再送の不変性**: 台帳照合は行ごとの衝突分岐より先に行うため、PK を変更した後の同一
  `operation_id` 再送は事前走査が衝突を見つけなくても台帳重複（`23505`／
  `DuplicateOperationId`）になり、行は変わらない。content_hash は解決した PK の宣言順の
  列名を連結するので、複合 PK の対象列順を変えた再送も同じ扱いになる。SET の内容だけを
  変えた再送は `22023`。
- **テナント境界**: 不可視な自テナント行への衝突は `42501` で文全体を拒否し台帳にも残さない。
  他テナントの同じ PK 値は衝突しない。
- **PostgreSQL との差異（記録のみ・挙動は変更しない）**:
  (a) 対象キー→既存行の対応表は文の書き込み前に 1 回だけ作るため、先行行が PK を `'a'→'b'`
  に変えたあと後続行が `'b'` を持つ場合、PostgreSQL は再衝突として `21000` だが、本実装は
  新規挿入し文末の検査で `23505` になる（`21000` 化は SQL-49 (4) の範囲）。
  (b) 一意性は文末にまとめて検査する（既存 `UPDATE` 経路と同じ）ため、複数行の `DO UPDATE` で
  PK 値を入れ替える文は本実装では成功する（PostgreSQL は行ごとに検査し `23505`）。
  いずれも PostgreSQL と異なる挙動を固めないため回帰テストでは固定しない。
- **対象外**: バッチ内の対象キー重複の `21000` 化、`ON CONFLICT ON CONSTRAINT`（Issue #1737 で対応済み。下記追記）、
  `DO UPDATE ... WHERE`、SET 右辺の式、NoSQL 表層の `upsert`（対象省略の `DO NOTHING` は
  Issue #1736 で対応。次節）。

## Issue #1736 追記: 対象省略の ON CONFLICT DO NOTHING（SQL-49 ポインタ）

- **構文**: `ON CONFLICT DO NOTHING` を受理する。AST は `OnConflictClause.target` を
  `ConflictTargetSpec::{Columns, Constraint, Omitted}` にし、省略と空リスト `()` を区別する（空リスト `()` は従来どおり `42601`。
  `ON CONSTRAINT` は Issue #1737 で受理する）。省略した `DO UPDATE` は構文段と束縛段の両方で `42601`
  （多層防御。実行段も内部エラーで拒否する）。
- **束縛**: `BoundConflictTarget::AnyUnique`。文内の `id`・鍵の重複は拒否しない（PostgreSQL と
  同じく先の行を挿入し後の重複を飛ばす）。対象を指定した形の文内重複 `22000` は変えない
  （この非対称は意図的。`21000` 化は SQL-49 (4) の別 Issue）。id 省略・SEQUENCE 呼び出しの拒否は維持する。
- **実行**: `constraint::AnyUniqueProbe` が行ごとに、行キー（従来方式の表）・主キー・UNIQUE 制約・
  UNIQUE 索引（式・部分を含む）のいずれかの違反を事前に判定し、違反した行だけを飛ばす。
  判定は書き込み後の一意性検査と同じ鍵計算（`UniqueKeyPlan::row_keys`）と同じ永続索引
  （`user_uniq/{table}`）で行い、鍵集合がずれないようにしている（ずれると飛ばすべき行が `23505`
  になる）。索引エントリが stale の場合は所有者の行を読み戻して鍵を再計算し、一致したときだけ
  衝突とする。同じ文で先に挿入した行の鍵はメモリ上で追跡する。挿入した行は従来どおり
  `CHECK`・`NOT NULL`・`FOREIGN KEY`・一意性を検査する（一意性検査は事前判定が漏れたときの最後の砦）。
- **テナント境界**: 判定の母集合は自テナントの物理キー範囲だけ。他テナントの鍵は衝突しない（RLS-9）。
  自テナントの不可視行との衝突も飛ばし、不可視行の破損状態は応答に出さない（RLS-10）。
- **RETURNING・件数**: 飛ばした行は返さず、`INSERT 0 <挿入数>`。全行を飛ばしてもテーブル世代は
  進めず、台帳は記録する（既存の `DO NOTHING` と同じ）。
- **content_hash**: `UpsertHashTarget::AnyUnique`（action tag `4`、列名部なし）。既存の tag `0`〜`3` の
  レイアウトは不変。
- **PostgreSQL との差異（記録のみ）**: 飛ばす行には `CHECK` 制約を評価しない（`NOT NULL` はハッシュ材料の
  組み立て時に全行で検査する）。対象を指定した既存の `DO NOTHING` も同様。
- **対象外**: `DO UPDATE` の式・`WHERE`、`INSERT ... SELECT`
  との併用、NoSQL 表層の `upsert`。

> Issue #2199 注記: 採番方式の表（SQL 表層で新しく作る表）には行キー指定の `ON CONFLICT (id)` が無く、`id` は宣言した通常の列として
> PRIMARY KEY／UNIQUE 制約との照合（`Unique` 対象）で解決する。新規挿入行の rowid は書き込み段が払い出す。詳細は
> [rowid-primary-key-separation.md](rowid-primary-key-separation.md) の §11。

## Issue #1737 追記: `ON CONFLICT ON CONSTRAINT <名前>`

SQL-49 (1)・(5)・INDEX-9 (3) のポインタ。構文は `ON CONFLICT ON CONSTRAINT <名前>
{DO NOTHING | DO UPDATE SET ...}`。`OnConflictClause::target` は `ConflictTargetSpec`
（`Columns`／`Constraint`／`Omitted` の 3 variant）になった（BREAKING CHANGE。`Omitted` は
Issue #1736 の対象省略形）。

- 名前解決は束縛時（`sql::parser::resolve_conflict_target`）。`DROP CONSTRAINT` と同じ探索順で
  UNIQUE 制約 → PRIMARY KEY の実効名を引き、列リスト形と同じ `BoundConflictTarget`（制約の宣言順）へ
  解決する。このため同じ制約の列リスト形と衝突判定・`RETURNING`・内容照合ハッシュが一致し、
  両形の再送は台帳重複になる。
- SQLSTATE: 不在 `42704`、CHECK・FOREIGN KEY の名前 `42809`、`CREATE UNIQUE INDEX` 由来の名前
  `42704`（制約として扱わない。`DROP CONSTRAINT <索引名>` と対称）、大文字小文字は区別する。
- 索引由来の判定は索引カタログが要るため、純関数の束縛ではなく `core` が束縛前に
  `Storage::is_unique_index_of_table` で行う（スキーマ取得と別 txn。競合してもエラーコードが
  入れ替わるだけで書き込みは既存の世代検査で守られる）。
- PRIMARY KEY 未宣言の従来方式の表の導出名 `<表>_pkey` は不在（`42704`）。行キー衝突は `(id)` 形で書く。
- ビュー経由は制約を持たないため `42704`。`INSERT ... SELECT` との併用は従来どおり `0A000`。
  NoSQL 表層の `upsert` は対象外のまま。

## Issue #2425 追記: `DO UPDATE SET` 右辺の数値加算（SQL-20・SQL-49・RECOVER-11 ポインタ）

親 #2424（TASK-334）。`SET n = n + 1` のような「既存行の値への加算」を受理して評価する。

### 受理形

- `<列> = <同じ列> + <数値リテラル>`（符号付き可）
- `<列> = <同じ列> + EXCLUDED.<列>`
- 上記以外は従来どおり拒否する（一般の式・別列参照・`WHERE`・衝突対象の一般化は #1738）。
  `n = m + 1`・`n = 1 + n`・`n = n - 1`・`n = n + 1 + 2`・`n = (n + 1)`・`n = n + NULL`・
  `n = n + 'x'`・`n = EXCLUDED.n + 1` は `42601`。
- `$n` の受理と ParameterDescription は #2427（下記追記）、並行加算・不可視衝突先 `42501` の検証は #2428。

### 型と分類

| 場面 | 結果 |
| ---- | ---- |
| 代入先が SMALLINT・INTEGER・BIGINT | 受理。整数同士は幅が違ってもよい（i128 で計算し、代入先の幅へ戻す） |
| 代入先が REAL・DOUBLE PRECISION | 受理。オペランドは整数・浮動小数どちらも可 |
| 代入先が NUMERIC | `0A000` |
| 代入先または `EXCLUDED` 側が数値以外 | `42883` |
| 整数の代入先への浮動小数オペランド・小数／指数表記のリテラル | `0A000`（PG では丸めて受理されるが fail-closed とする） |
| i64 を超える整数リテラル | `22003`（PG では numeric 計算になる差分） |
| 結果が列型の範囲外（REAL の overflow・underflow を含む） | `22003`。文全体を abort し、行・台帳とも副作用を残さない |
| 既存値または `EXCLUDED` が NULL | nullable 列は NULL、NOT NULL 列は `23502`（衝突しなければ評価しない） |

### 評価の場所

- 衝突した行が自テナントの所有かつ可視と確定した後の Visible 分岐（同じ write トランザクション内）で、
  デコード直後の既存値（pre-image）に加算する。全代入が更新前の行を見る形（PG と同じ）にするため、
  pre-image を別に保持する。
- 不可視の衝突先は評価前に `42501` のまま。他テナントの行は物理キーが違うため衝突せず、
  `22003`・`23502` が他テナントの値の存在を示すことはない。
- `sql::set_expr` は f64 を通し 2^53 超の BIGINT を扱えないため使わず、専用の `sql::upsert_add` で厳密に計算する。

### 内容照合ハッシュ

- 代入の value kind `2`（加算）を追加した。演算子タグ `+`、オペランド種別（`0` = EXCLUDED 列名、`1` = リテラル値）を
  含める。kind `0`／`1` のバイト列は不変。評価結果の値は含めない（RECOVER-11）。
- 同一 `operation_id`・同一文の再送は二重加算されず、リテラルや形が違えば内容不一致（`22023`）になる。

## Issue #2427 追記: 加算式の `$n` の型を代入先の列から推論する（SQL-49・WIRE-12 ポインタ）

- 拡張クエリ（Parse／Bind）で `DO UPDATE SET <列> = <同じ列> + $n` の `$n` を受理する。
  位置判定は `sql::params` のパターン 8、型推論は `core.rs` の `infer_param_types`。
- `$n` の型は INSERT 対象表の代入先列の型。ParameterDescription は整数列で int2／int4／int8、
  REAL／DOUBLE 列で float4／float8。数値でない列は Parse で `42883`（NUMERIC は `0A000`）。
- Parse の宣言型が合わない場合は fail-closed（text／bool は `42883`、整数列への
  float4／float8／numeric は `0A000`）。Bind 値の不正は `22P02`、範囲外は Execute で `22003`。
- Bind 後の文はリテラル SQL と同一のため、parser・束縛・評価・台帳ハッシュは変更しない。
- 対象外: `SET a = $n`（`$n` 単独）と一般式中の `$n`（#1738）、並行加算・不可視衝突先 `42501`（#2428）。
  NULL の Bind はリテラル `NULL` と同じ `42601`。
