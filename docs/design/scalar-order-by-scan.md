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
  で、混在（例: `ORDER BY lang, embedding <=> '...'`）は `42601`。なお Issue #1188 以降は、
  順位付け関数（`HYBRID`／`HYBRID_RRF`）以外の `<ident> '('` 形と `CASE WHEN` 始まりは
  スカラー関数・`CASE` 等の式キーとしてこの経路へ振り分ける
  （`docs/design/order-by-having-expressions.md`）。
- `ASC`/`DESC` は予約語化せず文脈的（大文字小文字非区別）に照合し、省略時は
  昇順。キー数の上限は `MAX_SCALAR_ORDER_KEYS`（8。NOSQL-15 の同種上限と揃えた
  実装既定値）。超過は `54000`。
- `LIMIT <n> [OFFSET <m>]` の後は文末のみを許可する（`USING MODE`／
  `HINT ORDER` は `42601`。`NULLS FIRST|LAST` は各キー直後で Issue #1791 により受理）。取得モード・評価順の
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
| `TEXT` | `Bytes` | バイト列順（C collation。実装既定値。キーの実効照合に従い、現在の許可リストはバイト列順のみ〔Issue #1610〕。ICU 照合は [icu-collator-adoption.md](icu-collator-adoption.md)） |
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

## キーセットモード（Issue #2093。NOSQL-23・SQL-25・TASK-253 ポインタ）

`OFFSET` を使わず「直前ページの最終行より後ろ」を指定する広域取得の engine 側の土台。NoSQL 表層の
`after`／`next_cursor`（Issue #2094）が呼ぶ。SQL テキストには新構文を足さない（`bind_scan` は
`keyset` を設定しない）。

- **順序仕様**: 利用者の `sort` キー ＋ 末尾キー。末尾キーは宣言 PRIMARY KEY の全列（宣言順・昇順）、
  PK を持たない表は物理 rowid（`BoundOrderTarget::Id`。`0A000` で拒否しない）。`BYTEA` の PK 列は
  末尾専用にバイト列順で束縛する（利用者向けの `ORDER BY <bytea 列>` の拒否は不変）。最終同点は
  `tenant_id` のバイト順で、既存の `compare_statement_order`（物理 `id` を同点に使う）とは別の
  `compare_keyset_order` を使う（同一 PK の削除→再挿入で位置が変わらないようにするため）。
- **述語**: 「行が位置より出力順で厳密に後ろ」＝ `compare_nullable` 経由の同一比較器で `Greater`。
  昇降順・複数キー・NULL 位置は既存の `ORDER BY` と構造上一致する。評価位置は可視性再適用＋
  `WHERE` 確定後、`OFFSET` 計数・ヒープ投入・投影より前（経路 (A)・経路 (B) パス 1）。不可視行は
  位置比較にも現れない。
- **次位置**: ヒープ容量（経路 (B)）／打ち切り件数（経路 (A)）を `limit + 1` にし、超過行が存在する
  ときだけ「最後に返した行」の位置を返す（超過行は投影・予算計上しない）。件数が `limit` の倍数
  ちょうどの最終ページは `None`。位置は投影セルではなく順序キーから作るため、採番方式の表の
  rowid（応答では 0）が位置を通じて外へ出ることはない。
- **併用拒否**: `OFFSET`・ウィンドウ・`WITH TIES`・暗黙行数上限とは併用できず `42601`
  （`with_after` 時と実行時の両方で検査）。位置の形・型の不一致は固定文言の `22000`。
- **位置の秘匿**: `KeysetPosition::seal`／`open` が encrypt-then-MAC（既存の HMAC-SHA256 のみ・
  依存追加なし）でペイロード全体を封緘し、rowid・テナントを平文で含めない。MAC は定数時間比較、
  長さは確保前に上限検査、改ざん・切り詰め・別鍵・別 AAD・版違いはすべて同一の固定文言 `22000`。
  鍵の生成・保管・AAD の構成（テナント・認証主体・表・`sort` の束縛）・base64 化は wire-server の責務で、
  Issue #2094 で実装済み（要約は `docs/design/implementation-status.md` の Issue #2094 の記録、
  外部仕様は `crates/wire-server/docs/nosql-api.md` のキーセットページング節）。
- **判断事項**: 従来方式の表で他テナントの `Public` 行が可視なとき、最終行がそのテナントの行だと
  位置に当該 `tenant_id` が（封緘されて）入る。封緘済みでも含めること自体を不可とする場合の代替は、
  その組み合わせでキーセットを固定文言のエラーで拒否することで、オーナー判断を待つ。
- **対象外**: 既存スカラー `ORDER BY` の同点順序の PK 化（既存結果が変わるため）、索引シークによる
  深いページの走査コスト削減（経路 (B) は全走査＋`limit + 1` ヒープで O(limit) メモリ）、
  `EXPLAIN` でのキーセット表示（受理のみ。#2094 の wire も表示は足していない）。

## スコープ外（本 Issue では対応しない）

- 集計文（SQL-13／14）の複数キー `ORDER BY` と NULL 位置の PG 既定化
  （**Issue #1185 で実施**: `docs/design/aggregate-order-by-and-scalar-group-keys.md`）
- `DISTINCT`（#917）・複数列 `GROUP BY`（#918）
- NoSQL 表層の `sort`（NOSQL-15・別 Issue #946・#947）
- ランキング段の二次ソートキーとしての利用
- 3 クライアントでの層 B 実測（spec 確定作業）
