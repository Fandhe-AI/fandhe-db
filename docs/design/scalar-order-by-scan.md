# ADR: 広域取得へスカラー列の任意 `ORDER BY` を付与する（Issue #915）

- ステータス: Implemented（spec 側ポインタ: SQL-25〔検討中〕・TASK-209。前提
  タスク TASK-170・TASK-167。ポインタのみで spec 本文は転記しない
  ―[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 対応: Issue #915
- 関連ポインタ: SQL-25・TASK-209・SQL-15（TASK-170。広域取得本体。
  `docs/design/wide-retrieval-scan.md`）・NOSQL-15（NoSQL 表層の `sort`。
  別 Issue #946・#947・本 Issue のスコープ外）・RLS-8（全読み取り経路への
  RLS 一般化）・TABLE-12（キー/ヘッダ tenant 整合検査）
- 検証コード: `crates/engine/src/sql/allowlist.rs`（`ScalarOrderKey`・
  `parse_scalar_order_by`・`ValidatedScan::order_by` の単体テスト）・
  `crates/engine/src/sql/parser.rs`（`BoundOrderKey`・`bind_scalar_order_by`）・
  `crates/engine/src/sql/scan.rs`（`with_visible_row`・`build_projected_cells`・
  `HeapEntry`・経路 (A)／(B) の単体テスト）・
  `crates/engine/tests/sql25_scalar_order_by.rs`（結合テスト）・
  `crates/wire-server/tests/wire_scan_order_by.rs`（層 A）

## 背景

`docs/design/wide-retrieval-scan.md`（SQL-15・TASK-170）が導入した広域取得
（`SELECT ... [WHERE ...] LIMIT n`）は意図的に「順序保証なし」だった。しかし
「スカラー列の値で並べて上位 n 件を取る」という RDBMS の基本操作は、正解を
含むデータ群を広く返す設計思想と両立し、他 DB 横断ベンチでも一般的な形である
ため、広域取得へ明示的な `ORDER BY <スカラー列>` を追加する。

## 設計方針

### 構文（本リポの実装既定値。spec ポインタ: SQL-25・TASK-209）

```text
SELECT <投影> FROM <table | view>
  [WHERE <既存許可述語>]
  ORDER BY <col> [ASC|DESC] [, <col> [ASC|DESC]]*
  LIMIT <n> [OFFSET <m>]
```

- `ORDER BY` 直後の先頭トークンが距離演算子形・関数呼び出し形（`sql::allowlist::
  Parser::parse_order_by` と同じ判定基準）でなければスカラー形として広域取得
  （`ParsedSelect::Scan`）へ振り分ける。ベクトル順位付け形とは構文上相互排他
  で、混在（例: `ORDER BY lang, embedding <=> '...'`）は `42601`。
- `ASC`/`DESC` は予約語化せず文脈的（大文字小文字非区別）に照合し、省略時は
  昇順。キー数の上限は `MAX_SCALAR_ORDER_KEYS`（8。NOSQL-15 の同種上限と揃えた
  実装既定値）。超過は `54000`。
- `LIMIT <n> [OFFSET <m>]` の後は文末のみを許可する（`USING MODE`／
  `HINT ORDER`／`NULLS FIRST|LAST` はいずれも `42601`）。取得モード・評価順の
  余地を持たないという広域取得本体の契約（TASK-161・SQL-12 との関係）をその
  まま引き継ぐ。`OFFSET` は #916（`docs/design/sql-offset-paging.md`）で
  `LIMIT` に後続する任意句として受理されており、本 `ORDER BY` との組み合わせ
  も同様に受理する（下記「スコープ外」節は本 ORDER BY 機能の対象外事項の一覧
  であり、`OFFSET` 自体は含まない）。
- `OrderByForm`（検索 SELECT のランキング段）へはバリアントを追加しない。
  スカラーキーは `ParsedScanShape`／`ValidatedScan` にのみ `order_by:
  Vec<ScalarOrderKey>` として載る。`ScalarOrderKey { column, descending }` は
  新規の公開型（非破壊）。

### 束縛（`sql::parser::bind_scalar_order_by`）

列名の解決は `bind_projection` の `Projection::Columns` 分岐と同じ優先順位
（カタログ上の実カラムを疑似列 `id` より優先して照合する。Issue #56 の方針を
踏襲）。型ごとの比較規約の分類（`OrderKind`）:

| 型 | `OrderKind` | 比較規約 |
| --- | --- | --- |
| `TEXT` | `Bytes` | バイト列順（C collation。実装既定値） |
| `INTEGER`／`BIGINT`／`DATE`／`TIMESTAMP` | `SignedInt` | 符号付き整数の内部表現をそのまま比較 |
| `REAL`／`DOUBLE` | `Float` | NaN はすべての非 NaN より大きく NaN 同士は等しい。`-0.0 == 0.0`（IEEE754 の `PartialOrd` がそのまま満たす。`f64::total_cmp` は符号ビットまで区別するため使わない） |
| `BOOLEAN` | `Bool` | `false < true` |
| `NUMERIC` | `Numeric` | `crate::numeric::cmp_exact`（scale 差を丸めず比較する既存実装をそのまま再利用） |
| `UUID` | `Uuid` | バイト列順（`crate::uuid::Uuid` の派生 `Ord`） |
| `ENUM` | `Enum` | 宣言順のラベル添字（`EnumTypeDef::labels()` の位置。文字列順ではない） |
| 疑似列 `id` | `Id` | `u64` の自然な順序（テナント内で一意） |

`VECTOR`・`ARRAY`・`BYTEA`・`JSON`／`JSONB` は並べ替え不能列として
`SqlSurfaceError::InvalidInput`（`22000`）で拒否する（SQL-25 (c) が `VECTOR`
への `DISTINCT` を `22000` とするのと同じ判断）。未知列も `22000`。

`BoundScan` は `pub(crate) order_by: Vec<BoundOrderKey>` を持つ。
`BoundScan::new`（TASK-186・NOSQL-3 の直接構築入口）は常に空を設定し、既存の
公開 API 契約を変えない（NoSQL 表層の `sort` は NOSQL-15・別 Issue #946・#947
のスコープで、本 Issue では対応しない）。

### 決定的な順序（§受入基準 2）

- 指定キーが同点のときは、指定した昇順・降順によらず**常に `id` 昇順**で
  タイブレークする（`id` はテナント内で一意なため以降のキーは無関係）。
- 他テナントの `Public` 行と `id` が重なる場合は、最終キーとして `tenant_id`
  のバイト列順を使う（可視行同士の相対順序にしか影響せず、`tenant_id` 自体は
  結果へ出力しないため存在情報は漏れない）。
- `NULL` の位置は PostgreSQL 既定（ASC は末尾、DESC は先頭）。

### 実行: 2 経路（`sql/scan.rs`）

`bound.order_by` が空なら既存経路（無変更）。非空の場合は、RLS 判定・
TABLE-12 整合検査・デコード・`WHERE` の適用順序（`with_visible_row` に共通化。
security.md P0）を保ったまま、次のいずれかで決定的な順序を作る。

#### 経路 (A): `id` 早期打ち切り

先頭キーが疑似列 `id` **かつ** `ctx` が他テナントの `Public` 行を可視としない
（`PolicyContext::allows_public() == false`）場合に限る。この条件下では可視行
は必ず自テナント所有行に閉じるため、自テナントのパーティション
`(tenant, 0)..=(tenant, u64::MAX)`（`tenant.rs::enumerate_dml_candidates` と
同型の閉区間）を直接範囲走査し（DESC なら `.rev()`）、`LIMIT` 件で打ち切る。
`id` はテナント内で一意なので後続キーは順序に影響しない。

既定の `PolicyContext::new`（`Public` のみ許可）は他テナントの `Public` 行も
可視にする設計（本 DB の「正解を含むデータ群を広く返す」方針）のため、経路
(A) が選ばれるのは `Private` のみ許可する ctx など限定的なケースになる
（`allows_public() == true` が一般的な既定であることの帰結。narrow だが正しい
最適化として実装する）。

#### 経路 (B): 上位 N 件の 2 パス

- **パス 1**: 既存の走査ループ（RLS → TABLE-12 → デコード → `WHERE` → 可視性
  の再判定）をそのまま通し、可視かつ `WHERE` を満たす行の並べ替えキー
  （`OrderValue`）と `(tenant_id, id)` だけを、容量 `bound.limit` の
  `BinaryHeap<HeapEntry>` に保持する。`HeapEntry` は `Ord` を「昇順ソートする
  と最終的な出力順になる」意味で実装し（`compare_order_key` で ASC/DESC・
  NULL 位置を適用）、`heap.len() == limit` に達した後は「最悪要素
  （`heap.peek()`＝最大）より小さい」候補だけを追い出して差し替える。
  容量は `limit ≤ MAX_SEARCH_K` で有界。
- **予算**: heap に保持するバイト量（TEXT キーの所有バイト長・`tenant_id`・
  構造体サイズ）を `max_result_bytes` に計上し、追い出したら解放する
  （`heap_entry_bytes`。`try_accumulate_budget` と同じ fail-closed 契約）。
- **パス 2**: `BinaryHeap::into_sorted_vec`（`Ord` の昇順。`(tenant_id, id)` が
  一意な全順序のため安定性は問題にならない。`sort_unstable_*` は使わない
  ―`make sort-determinism-check`）で全順序を確定してから、同じ read txn 内で
  勝者のみを `table.get((tenant_id, id))` により再取得し、`build_projected_cells`
  で投影する。再取得時に行が消えている・可視でなくなっている場合は
  `SqlSurfaceError::Internal`（`XX000`）で fail-closed に拒否する（同一スナップ
  ショット内では本来起こらない不変条件）。

#### ボツ案: `ScalarIndex::id_index` の利用

`id` 順の実行に既存の `ScalarCacheAccess`／`ScalarIndex` を使う案も検討したが
採らなかった。`ScalarCacheAccess` は scan 経路に結線されておらず、アリーナの
スナップショットが必要（`VECTOR` 列が必須で、キャッシュミス時に全 embedding
を読み込む）ため、単発の scan では経路 (A) の直接範囲走査より重い
（`docs/design/wide-retrieval-scan.md`「実行本体」節が `VectorArena` を経由
しない理由と同じ判断）。

### ビューとの相互作用

ビュー経由の広域取得（`sql::view::resolve_from`）でも、クエリ自身の
`order_by` 列がビューの公開列集合（`view_columns`）に収まっているかを
`check_columns_within_view` で検査する（隠れた基底列による並べ替えから情報が
漏れないようにする。投影・`WHERE` と同じ扱い）。ビュー本文
（`ParsedViewBody`）自体は構文上 `ORDER BY` を持たないため、連鎖の各段の検査
には影響しない。疑似列 `id` での並べ替えは、ビューの投影が `id` を明示的に
列挙するか `SELECT *` の場合のみ可能（投影・`WHERE` の既存契約と同一）。

## 影響

- `crates/engine/src/sql/allowlist.rs`: `ScalarOrderKey`（新規公開型）・
  `MAX_SCALAR_ORDER_KEYS`・`ParsedScanShape::order_by`・
  `ValidatedScan::order_by`（フィールド追加。非破壊——`ValidatedScan` は
  `pub(crate)` フィールドのみでカプセル化済み）
- `crates/engine/src/sql/view.rs`: `check_columns_within_view` に `order_by`
  引数を追加（`pub(crate)` 関数のため非破壊）
- `crates/engine/src/sql/parser.rs`: `BoundOrderTarget`・`OrderKind`・
  `BoundOrderKey`（いずれも `pub(crate)`）・`BoundScan::order_by`
  （`pub(crate)` フィールド追加。`BoundScan::new` は常に空を設定するため
  TASK-186・NOSQL-3 の公開 API 契約は不変）
- `crates/engine/src/sql/scan.rs`: `with_visible_row`・`build_projected_cells`・
  `HeapEntry`・経路 (A)／(B) の分岐を追加
- `crates/engine/src/policy.rs`: `PolicyContext::allows_public`（`pub(crate)`
  読み取り専用アクセサ。判定ロジック自体は増やさない）

## スコープ外（本 Issue では対応しない）

- 集計文（SQL-13／14）の複数キー `ORDER BY` と NULL 位置の PG 既定化
  （`group_by.rs` は単一キー・NULL 常に末尾の既存規約のまま）
- `DISTINCT`（#917）・複数列 `GROUP BY`（#918）
- NoSQL 表層の `sort`（NOSQL-15・別 Issue #946・#947）
- ランキング段の二次ソートキーとしての利用
- 3 クライアントでの層 B 実測（spec 確定作業）
