# `TRUNCATE TABLE` の設計判断

Issue #874・対象ビヘイビア: SQL-22（TASK-195）。関連ポインタ: TABLE-4（テーブル定義は
残る）・RLS-7（RLS 暗黙適用）・RLS-9（他テナント存在情報の非漏えい）・
RECOVER-1〜3・RECOVER-10（`operation_id` 必須化・台帳照合による再送判定）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
TRUNCATE TABLE <table> USING OPERATION_ID '<id>'
```

- `INSERT`（TASK-80・SQL-10）と同じ「文末専用句 `USING OPERATION_ID '<id>'`」規範を
  踏襲する。省略・明示 `NULL` はいずれも `LedgerMode::Ledgered`（既定）で `23502`。
- 複数テーブル指定・`CASCADE`／`RESTART IDENTITY` 等の PostgreSQL 拡張句は構造的に
  受理しない（許可リスト外・`42601`）。
- `EXPLAIN TRUNCATE ...` は許可形状に存在しないため、先頭トークンが `EXPLAIN` の
  場合は既存の `EXPLAIN` 分岐（次トークンが `SELECT` であることを要求）へ流れて
  自然に `42601` になる（`INSERT` と同じ経路）。
- `FOREIGN KEY` の参照アクション（`CASCADE`／`SET NULL`／`SET DEFAULT`。TABLE-17・
  TASK-205、Issue #1076）は発火しない（`TRUNCATE ... CASCADE` は上記のとおり
  許可リスト外で未対応のまま）。PostgreSQL の `TRUNCATE` も `ON DELETE` アクションを
  発火させない挙動に合わせた設計判断で、`constraint::ReferencedRowsChange::Truncated`
  （`Removed` と区別する専用 variant）を渡すことで事後検証（`NO ACTION` 相当。
  自テナントの参照元行が残れば `23503`）のみを行う。詳細は
  `docs/design/foreign-key.md`「D15」節参照。

## 削除スコープ: 「RLS 可視集合」ではなく「テナント所有」

`TRUNCATE` はセッションのテナントが**所有する**全行（`Visibility::Public`／
`Private` を問わない）を削除する。これは通常の読み取り経路が適用する
「RLS 可視集合」（`PolicyContext::is_visible`。他テナントの `Public` 行も条件次第で
可視になる）とは異なる、書き込み認可の判定基準（`PolicyContext::is_owner` と同型の
「テナント一致のみ」判定）である。

実装上は `(tenant_id, id)` の物理キー名前空間（TABLE-12）そのものを削除範囲とする。
`redb::Table::retain_in` の範囲引数 `(tenant_id, 0)..=(tenant_id, u64::MAX)` が
唯一の境界であり、述語内で行ヘッダをデコードして所有権を再チェックしない
（`retain_in` の述語は `bool` を返すのみでエラーを伝播できず、フォールブルな
デコード失敗を `Err` として扱う手段がない——`panic` はトランザクション破損に
つながるため許容できない。既存の `replace_typed_rows_by_text_key` が同じ理由で
テナント名前空間の走査中にヘッダ再チェックを行っていないのと同じ設計判断）。

## 応答: 件数を返さない

`TruncateOutcome`（`sql::exec`）はフィールドを持たない空構造体で、削除件数を
一切含まない。自テナント内の削除件数であっても、これを返すと再送検知や
タイミング以外の経路で「対象テナントがどれだけ行を持っていたか」の推測材料に
なり得るため、意図的に構造で禁止する。wire 応答（simple query プロトコル）も
`CommandComplete` タグを PostgreSQL の `TRUNCATE TABLE` に合わせた固定文字列
（件数なし）にする。

## RLS-9: 他テナント存在情報の非漏えい

削除対象・応答内容のいずれも他テナントの `Public`／`Private` 行の有無に依存しない
（`crates/engine/tests/truncate_table.rs::truncate_response_is_identical_regardless_of_other_tenant_row_count`
で固定）。同ファイルの `truncate_removes_only_own_tenant_rows_public_and_private` は、
`Visibility::Public` がグローバルに可視（`policy.rs::is_visible` の契約）である点を
踏まえ、以下 3 種の観測点を組み合わせて「alice の Public・Private 行だけが消え、
bob の行には触れない」ことを検証する:

1. `ctx_private_only(tenant)`（`Private` のみ許可）による、各テナント自身の
   `Private` 行数（cross-tenant `Public` 混入なしに数えられる）
2. 第三者（行を一切所有しないテナント。`Public` のみ許可）による、テーブル全体で
   現在グローバルに可視な `Public` 行数
3. `alice` 自身の視点（`Public`＋`Private` 許可）: 自テナント分がすべて消えても、
   bob の残存 `Public` 行はそのまま見え続ける（cross-tenant `Public` 可視の契約が
   `TRUNCATE` によって変わらないことの確認）

## RECOVER-1・RECOVER-10: `operation_id` 必須化・再送判定

`content_hash::for_truncate()` は入力を一切持たない（`OpTag::Truncate` と
ドメイン分離タグのみ）。`TRUNCATE` 要求はテーブル名以外にクライアント由来の
可変フィールドを持たず、台帳キー自体が `(tenant, table, operation_id)` で
テーブル名を既に一意に識別しているため、同一 `operation_id` への `TRUNCATE`
再送は常に内容一致（`23505`）に収束する（`22023` は構造的に到達不能だが、
他操作と同じ台帳照合機構を再利用することで実装の一貫性を保つ）。

台帳記録は行削除より**先**に行う（`delete_row_unchecked` と同じ理由: 再送時の
判定を正しく検出するため）。削除対象が 0 件でも台帳記録・commit は必ず発生する
ため、常にテーブル世代を進める（`insert_rows` の空バッチショートカットとは
意図的に非対称。`truncate_on_empty_table_still_records_the_ledger_entry` が
0 行 TRUNCATE でも再送検知が機能することを vacuous pass にならない形で固定する）。

## TABLE-4: DDL ではない

`TRUNCATE` はテーブル定義（カタログ）を変更しない。`Storage::drop_table` との
対比として、`truncate_keeps_table_definition_and_does_not_affect_other_tables`
テストが「`TRUNCATE` 後も同一テーブルへ `INSERT` できる」ことで確認する
（`drop_table` ならテーブル不存在で `42P01` になる）。他テーブルの行・カタログも
無変更のまま。

## キャッシュ失効

`TRUNCATE` は `bump_table_generation_in_txn` を経由するため、既存のテーブル単位
世代整合キャッシュ（`SqlArenaCache`・`VisibleBitmapCache`・`SparseIndexCache`・
`HnswIndexCache`・`ScalarIndexCache` 等）はすべて自然に失効する。
`truncate_invalidates_arena_and_visible_bitmap_caches` テストが `SqlArenaCache`
（DISTANCE クエリ）・`VisibleBitmapCache`（`COUNT(*)`）を TRUNCATE 前に温めてから
TRUNCATE し、削除済み行が一切ヒットしないことを固定する（cold のみのテストでは
世代失効を検証したことにならないため、必ず warm→truncate→re-query の順で確認する）。

## NoSQL 表層は対象外

`op` 許可リスト（`search`／`scan`／`aggregate`／`insert` の閉じた語彙。NOSQL-1・
NOSQL-9）に `truncate` は含まれない。Issue #874・本ドキュメントのスコープは
SQL 表層限定であり、NoSQL 側への `truncate` op 追加は対象外（spec 側にも
TRUNCATE 専用の NoSQL 対応 ID は存在しない）。

## 影響行数上限（SQL-19）の対象外

SQL-19（述語形 UPDATE/DELETE）が定める 1 文あたり影響行数上限は `TRUNCATE` には
適用しない（全行削除が目的の操作のため）。`retain_in` は候補 id を `Vec` へ
materialize しないため、`MAX_SCANNED_ROWS`／`MAX_VISIBLE_ROWS` のような DoS 上限も
構造的に不要である。
