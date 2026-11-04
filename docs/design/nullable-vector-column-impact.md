# NULL 許容 VECTOR 列の影響範囲の棚卸し（arena／HNSW／GPU／hybrid）

- ステータス: 記録（コードの現状の棚卸し。NULL 許容そのものの経緯は spec 側を参照: `docs/spec/04-behavior/records/pg-parity-owner-decisions-2026-10-10.md`。本書で新たに承認を得たものではない）
- 対応: Issue #2041（親 #2040）。関連: #2042（カタログ・row_codec で NULL を保存）、#2043（kNN・HNSW・GPU・hybrid で NULL 行を除外）
- ビヘイビア: TABLE-26（`docs/spec/04-behavior/data-model.md`）、TABLE-1 の改訂注記、SQL-33 (3)(5)、SQL-74 (1)、CORE-10（`docs/spec/04-behavior/core-engine.md`）（ポインタのみ。期待欄は転記しない）

## 背景

現状、`VECTOR` 列は NOT NULL 固定で、`crates/engine/src/sql/ddl_column_type.rs` の注記が arena 構築・kNN・HNSW 各経路の安全性を根拠に挙げる。本書は NULL を許可した場合に各経路でどこを除外・扱い直す必要があるかを並べ、担当を #2042／#2043 へ振り分ける。**本書はコード・テスト・既存文書を変更せず、最終判断も行わない**（判断材料の提示まで）。

保存表現として「`VECTOR` 列が NULL ＝ `dim == 0`」のモデルは、走査・集計・式評価側にすでにある（`sql/scan.rs` の投影、`sql/aggregate.rs` の `RowVector`、`sql/expr_program.rs` の `PushVector` など）。一方でベクトル検索の経路は `dim == 0` 行を想定していない。

計測基準: `origin/main` の `2704de1ed`、2026-11-04。行番号は計測時点の値で、並行 PR のマージでずれる。

## 要点: 2 つの関所

| 関所 | 場所 | 現状 |
| ---- | ---- | ---- |
| arena 構築 | `crates/engine/src/arena.rs` `build_filtered_with_rows_and_limits_in_txn_capturing`（1218 行〜。次元比較は 1330 行付近） | `found_dim != expected_dim` で `ArenaError::DimMismatch` を返し、構築全体を拒否 |
| RLS ストリーミング走査 | `crates/engine/src/rls.rs` `SearchTimeFilter`（710 行〜。次元比較は 846 行付近） | 同様に `DimMismatch` で拒否 |

- 同じ系統として `rls.rs` の `PrefilterIndex::build` 系（624 行〜。375・459・794 行付近の `KernelError::DimMismatch`）がある
- `sql/exec.rs` は arena 構築失敗を `Internal "arena build failed"` に写像する（608 行付近）
- 今は `dim == 0` 行を書けないため顕在化しない。許可すると、この 2 箇所が **fail-closed のまま kNN 全体を壊す**。この fail-closed は設計ではなく結果的なもので、許可後は NULL 行の扱いを決める関所になる
- HNSW・GPU バッチ・hybrid の密側・各キャッシュは、この決定を引き継ぐ構造である
- 置き換えの方針（セキュリティ上の前提）: `dim == 0` だけを NULL として扱い、`dim != 0` かつ `dim != expected` は従来どおり拒否する。不一致を一律に読み飛ばす fail-open 化は推奨しない

## 経路別の棚卸し表

判定: 影響あり／要判断／影響なし。

| 経路 | 代表パス | 現状の NULL 行の扱い | 許可後に要る対応 | 判定 | 担当 |
| ---- | -------- | -------------------- | ---------------- | ---- | ---- |
| arena 構築 | `arena.rs` `push_visible_row`（412 行）・`SqlArenaCaptureBuilder`（461 行）・`build_from_cached_rls_rows`（941 行）・`build_from_cached_rls_rows_subset`（1014 行）・`filter_cached_rls_rows_subset`（1104 行） | `dim == 0` を想定せず、次元不一致として拒否 | NULL 行の扱い（読み飛ばし／マスク）を決め、各入口で一貫させる | 要判断 | #2043 |
| RLS ストリーミング・事前フィルタ | `rls.rs` `SearchTimeFilter`・`PrefilterIndex`・`PrefilterSnapshot`（239 行）、`core.rs` の `EngineCore::search`（`VectorArena::build_filtered` 経由） | 同上 | arena と同じ決定を適用 | 影響あり | #2043 |
| CPU 厳密走査 | `kernel.rs`・`parallel_search.rs` | 入力は arena のスライスで、`dim == 0` を持たない | arena の決定を引き継ぐ。単独の対応は不要 | 影響あり（間接） | #2043 |
| HNSW | `hnsw.rs`（`dim == 0` は索引単位で `DimMismatch`。4847 行付近）、`hnsw/parallel_build.rs`、`sql/hnsw_cache.rs`、`sq8.rs`、f16 常駐 | 索引は `arena.vectors()` から構築。NULL 行を想定しない | 索引構築は `arena.vectors()` のスライスを前提とするため、`dim == 0` の行を入力から外すか構築側で扱う対応が要る（関連: SQL-33 (3)(5)） | 影響あり | #2043 |
| GPU バッチ | `batch_search.rs` `ResidentMatrix::build_with_metric`（590 行。`vectors.len() == ids.len() * dim` が前提）、`batch_fallback.rs` `FallbackBatchEngine::build`、`gpu_batch.rs`、`gpu_batch/packed_i8.rs` | 行列は NULL を表現できない | 呼び出し元が NULL 行を除いて渡す契約を明記し、テストで固定する。調べた範囲では storage から行を読んで GPU バッチを組む production 経路は `core.rs`・`dispatch.rs`・`wire-server/src` に無く、呼び出し元はベンチとテストのみ | 影響あり（契約の文書化） | #2043 |
| hybrid | `hybrid.rs`、`sql/hnsw_hybrid.rs`、`sql/sparse_cache.rs` | 密・疎の両ヒットを可視集合（arena の id）に限る。疎コーパスの `DocId` は arena のスロット番号 | 下記「hybrid のスロット契約」参照 | **要判断（最重要）** | #2043 |
| 可視カーディナリティ・キャッシュ | `hnsw-rls-cardinality-switch` の `full_scan_ratio`、`visible_bitmap_cache`、`sql/arena_cache.rs` `SqlArenaCache`、`rls_capture` の契約、`sql/scalar_index.rs` の capturing 経路、`sql/hnsw_cache.rs` の subset overlay | 件数・中身は arena の行集合に依存 | NULL 行を arena の内外どちらに置くかで分母とキャッシュの中身が変わる。決定後に整合を取る | 要判断 | #2043 |
| 書き込み側の NULL 拒否点 | `sql/parser.rs`（4844 行付近の「`nullable` に関わらず必須」ほか）、`sql/set_expr.rs`、`tenant.rs`、`catalog.rs`（SET／DROP NOT NULL と ADD COLUMN の検証）、`sql/ddl_column_type.rs`（ADD COLUMN での VECTOR 追加拒否）、NoSQL の `wire-server/src/http/query/insert.rs` `bind_row` | VECTOR への NULL を拒否 | 拒否の緩和と NULL 保存（`dim == 0`）の書き込み | 影響あり | #2042 |
| 旧表の互換 | カタログ（`catalog.rs`）・`sql/ddl.rs` の束縛 | parser が「`nullable` の値に関わらず VECTOR は必須」と扱うため、既存カタログの VECTOR 列が `nullable=true` で永続化されている可能性がある | 改訂前の表を NOT NULL として開くには、カタログ版による判定が要る。これを前提条件として申し送る | 要判断 | #2042 |
| 宣言的フィルタ | `declarative_filter.rs`（1148〜1170 行付近が VECTOR 列への `IS [NOT] NULL` を拒否。テストあり） | 拒否（マスク外の VECTOR 列に常に `None` を積む前提の防御） | NULL を保存する場合、評価側の前提（VECTOR 列は常に `None`）が崩れるため見直しが要る（関連: TABLE-26 (3)） | 要判断 | **担当未定**（#2042・#2043 のどちらにも明示の受け皿が無い。兄弟 Issue の分割に穴がある可能性） |
| 式・述語・投影 | `sql/expr_program.rs`（`PushVector` が空スライスで NULL を返す）、`sql/scan.rs`、`sql/aggregate.rs`（`COUNT(vec)` が NULL 行を数えない）、`sql/where_tree.rs` | 対応済み（SQL-74 (1) のポインタ） | なし | 影響なし | — |
| rerank・precision | `rerank.rs`・`precision.rs` | 融合後の候補を扱い arena を直接読まない見込み | 実装時に確認して判定する | 未確認 | #2043 |
| NoSQL 検索バインド | `wire-server/src/http/query/search.rs` | engine の kNN 経路を引き継ぐ | engine 側の決定に追随 | 影響あり（間接） | #2043 |

## hybrid のスロット契約（要判断）

- `arena.rs` `push_visible_row` は「arena のスロット番号 ＝ `candidate_columns` の添字 ＝ 疎の `DocId` ＝ provider id」の 1 対 1 契約を持つ（`sql/sparse_cache.rs` 冒頭の説明も同じ）
- 現状のコードでは、arena で NULL 行を単純に読み飛ばすと同じスロット番号を使う疎側の候補からも消える。NULL 行を密側と疎側で別扱いにする場合はこの契約との整合が要る（関連: TABLE-26 (3)）
- 選択肢（本書では決めない。#2043 の判断材料）:
  - (A) arena のスロットは保持し、密側の採点・HNSW 構築・GPU 行列からだけマスクで除く。スロット契約を維持できる。費用は密側にマスクを通す改修
  - (B) arena から NULL 行を除き、疎側の `DocId` 空間を arena とは別に組み直す。スロット契約の作り直しが要り、`sql/hnsw_hybrid.rs`・`sql/sparse_cache.rs`・`hybrid.rs` の可視集合検証に波及する

## 変えないもの

- テナント境界: arena の可視性述語を embedding の decode より前に評価する順序、`verify_row_key_tenant`。NULL 行の除外を可視性判定の前に置く案や、hybrid の可視集合検証（可視集合外の id は全体を拒否）を緩める案は採らない
- リソース上限: `check_capacity`（`arena.rs` 216 行）・`MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` の判定対象（可視行の数）が、マスクやスロット保持の案でもずれないことを確認事項とする
- `VECTOR` 列を持たない表の契約: [insert-without-vector-column.md](insert-without-vector-column.md)
- wire_code の分類: 未対応の形は 42601／0A000 などで fail-closed に拒否する

## 兄弟 Issue への申し送り

- #2042: 書き込み側の NULL 拒否点の緩和、旧表の互換（カタログ版判定）
- #2043: arena・RLS・HNSW・GPU・hybrid・キャッシュ。要判断は hybrid のスロット契約（A/B）とキャッシュの分母
- 担当未定: 宣言的フィルタの `IS [NOT] NULL`（VECTOR 列）

関連文書: [hnsw-rls-cardinality-switch.md](hnsw-rls-cardinality-switch.md)、[visible-bitmap-cache.md](visible-bitmap-cache.md)、[sql-arena-generation-cache.md](sql-arena-generation-cache.md)、[cosine-operator-migration.md](cosine-operator-migration.md)（文書形式の参照元）。
