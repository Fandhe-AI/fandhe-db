# 他 DB との機能別横断ベンチと GPU（RTX 3060）実機検証

- ステータス: **実施済み**（計測ツール `scripts/crossdb_bench/`・
  `crates/engine/benches/gpu_scaling_bench.rs`。production 変更は
  `crates/wire-server/src/server.rs` の `TCP_NODELAY` 設定のみ）
- 対応: PR #451 後続（hnsw_rs 撤去）・Issue #406 関連（HNSW 構築比較の対照）・
  Issue #636（親 #629。fixture の FS 条件・Qdrant イメージ版・2026-09-08 再計測）・
  Issue #661（比較条件の未記録な非対称・fixture 選択率の記録。production・ハーネス無変更）
- 入口: `make bench-crossdb`（README「他 DB との機能別横断ベンチ」）・
  `make bench-gpu-scaling`・`scripts/crossdb_bench/gpu/README.md`
- 関連: `docs/design/hnsw-parallel-build.md`（自作 HNSW vs usearch）・
  `docs/design/core16-f16-resident-gate.md`（GPU f16 常駐ゲート）・
  `docs/design/hybrid-rrf-latency-breakdown.md`（in-process の段別内訳）・
  `docs/design/benchmark-judgement-policy.md` §5（共有 QEMU 参考値の証拠力区分）

## 目的・範囲

1. 自作 DB（本リポの `wire-server`＋`engine`。以下 self）を **wire 経由の実クライアント
   から**、PostgreSQL（pgvector）・MySQL・SQLite（sqlite-vec）・Qdrant・LanceDB と
   機能別に同一 fixture・同一クエリで比較する。
2. NVIDIA GeForce RTX 3060 で `engine::gpu_batch`（TASK-128〜130）が CPU-SIMD バッチ
   経路に対してどの規模・バッチサイズで高速化するかを実測し、FAISS（GPU）・
   Qdrant（GPU 索引構築）と対照する。

spec の受け入れ基準（閾値）は本ドキュメントでは扱わない。すべて情報提供専用の
実測であり CI には配線しない。

## 計測環境

| 項目 | 値 |
| --- | --- |
| ホスト | dev-box02（Proxmox VM・QEMU 12 vCPU・31 GB。別プロジェクトのコンテナ常駐で loadavg 約 2） |
| GPU | NVIDIA GeForce RTX 3060 12 GB（PCIe パススルー）・driver 595.71.05・CUDA 13.2・nvidia-container-toolkit 1.19.1 |
| self | `wire-server`（release ビルド・`cef02bc` 時点。loopback TCP・簡易クエリプロトコル・psycopg 3.3.5。2026-09-08 再計測は `773a835` 相当・sha256 先頭 `4b950db35d5c`〔下記「2026-09-08 再計測」節参照〕） |
| pgvector | `pgvector/pgvector:pg17`（PostgreSQL 17.11・pgvector 0.8.6。`<#>`。HNSW m=16 / ef_construction=100 / ef_search=64） |
| sqlite-vec | sqlite3 3.46.1・sqlite-vec 0.1.9（in-process・ファイルベース DB・vec0 brute-force・cosine 近似〔内積指標なし〕） |
| Qdrant | `qdrant/qdrant:latest`（gRPC・Distance.DOT・exact / HNSW。2026-09-08 再計測時: サーバ版 1.19.1・digest `sha256:12364fe8…`〔2026-09-03 作成〕。初回計測時のサーバ版・digest は未記録） |
| LanceDB | lancedb 0.38.0（in-process・`metric="dot"`・exact / IVF_HNSW_FLAT m=16 ef_construction=100 ef=64） |
| MySQL | `mysql:9`（9.7.2 Community。`VECTOR` 型は作れるが `DISTANCE()`／`VECTOR_DISTANCE` が無く〔ERROR 1305〕、`CREATE VECTOR INDEX` は構文エラー〔HeatWave 限定〕のため KNN 系は n/a） |
| FAISS | faiss-gpu-cu12 1.14.1（Docker `bench-faiss-gpu`・python 3.12・`OMP_NUM_THREADS=12`・OpenBLAS 1 スレッド・BLAS 経路無効化） |
| Qdrant GPU | `qdrant/qdrant:gpu-nvidia-latest`（`QDRANT__GPU__INDEXING=1`。構築のみ GPU） |
| fixture | `seed_docs seed` 25,000 行・dim 128（tenant-a 23,000 行 public・tenant-b 2,000 行 private）・200 クエリ・k=10。`lang` 列は `en`／`ja` の 2 値（`seed_docs.rs` の `rng.next().is_multiple_of(3)` により `ja` が概ね 1/3）で、`docs25k.jsonl` の実測は `ja` 合計 8,309/25,000（33.2%）・可視行（public）中 7,621/23,000（33.1%）・`where_compound_count`（public ∧ id>100 ∧ ja）7,599 行（Issue #661。下記「公平性の注記」参照。`feature_bench.rs`／`scan_stage_profile_bench.rs` の `lang` 5 値輪番コーパス〔`ja` ≒ 20%〕とは別物） |
| fixture FS | 初回計測は FS 未記録（tmpfs 上の再現値と数値が整合。下記「2026-09-08 再計測」節参照）。2026-09-08 再計測は ext4（`/dev/sda1`）・tmpfs 対照は `/tmp` |
| 反復 | warmup 5・50 反復（`feature_bench.rs` の既定に合わせる）・p50/p95 µs |

計測対象以外のコンテナは停止した。Docker イメージの digest は `scripts/crossdb_bench/README.md`
の手順どおり `docker inspect` で結果 JSON の meta に記録している。2026-09-08 再計測はこの
前提の例外（下記「2026-09-08 再計測」節参照）。

### 公平性の注記

- **接続経路が異なる**: self・pgvector・MySQL は loopback TCP、sqlite-vec・LanceDB は
  in-process、Qdrant は gRPC。in-process の数値は往復コストを含まない。
- **永続化条件**: sqlite-vec は当初 `:memory:`（非永続）で他 DB（永続ストレージ）と
  投入速度の比較条件が揃っていなかったため、`--workdir` 配下のファイルベース DB へ
  変更した（codex-review P2 指摘。PRAGMA は SQLite 既定〔`journal_mode=DELETE`・
  `synchronous=FULL` 相当〕のまま）。本ドキュメントの横断ベンチ表（「横断ベンチ実測」節）の
  sqlite-vec の投入系数値（`ingest_bulk`・`ingest_single_stmt`）はファイルベース化前
  （in-memory）の値。ファイルベース化後の値は 2026-09-08 再計測で取得済み（下記
  「2026-09-08 再計測」節参照）。
- **fixture の FS と fsync**: self（redb・1 文ごとに commit）・pgvector（autocommit）・
  sqlite-vec（ファイル版）はいずれも 1 文（1 行）ごとに commit するため、`ingest_single_stmt`
  のスループットは fixture・DB データファイルの置き場が FS の commit（fsync）コストに
  依存しうる。2026-09-08 再計測では同一 ext4 条件で self 約 2,000 rows/s・pgvector
  1,662 rows/s・sqlite-vec 292 rows/s と self が最速だった一方、self の fixture を
  tmpfs（`/tmp`）へ置くと 7,414〜7,714 rows/s まで上がった（「fsync が支配的」は
  ext4 と tmpfs の差〔約 3.7 倍〕からの推定であり、strace／perf 等での直接の裏付けは
  未取得。詳細は下記「2026-09-08 再計測」節参照）。
- **可視性モデル**: self の wire セッションは現行契約でどのテナントからも
  `visibility = 'public'` の行のみ可視（private は所有テナント自身からも不可視）。
  他 DB は RLS を持たないため `visibility` 列を持たせ毎クエリに
  `WHERE visibility = 'public'` を付け、可視集合を同じ 23,000 行に揃えた
  （テナント列では絞らない。ポリシーエンジンの比較ではない）。
- **exact / ANN**: `exact` は索引なし全件探索、`hnsw` は各 DB の近似索引。
  self は既定エンジン（brute-force）のみ計測した（ANN opt-in は wire から選択できない）。
  Qdrant の `hnsw` は `indexing_threshold` を下げて全セグメントを索引化し、status green
  かつ `indexed_vectors_count` が全件に達したことを確認してから計測する（既定閾値では
  25,000 行・dim 128 は各セグメントが閾値未満のまま HNSW が構築されない）。逆に `exact`
  は `indexing_threshold=0` で自動構築を無効化し、検索側の `exact=True` だけでは止まらない
  バックグラウンド構築の負荷が計測へ混入しないようにする。LanceDB の
  hybrid もベクトル側は `metric="dot"` を明示している。sqlite-vec の hybrid は FTS5 の
  `MATCH` に自然文をそのまま渡すと構文エラーになるため、各語を二重引用符で囲んだ語句
  クエリへ変換している。
- **Recall@10**: 合成 embedding は内積の同点が多いため、主指標 `recall_at_10` は
  同点許容版（`TIE_EPSILON = 1e-4`）。正解集合を境界より厳密に上位の集合
  `strict_above`（スコア > 境界スコア + ε）と境界での同点集合 `tie_boundary`
  （|スコア − 境界スコア| ≤ ε）に分け、分子を
  `|hits ∩ strict_above| + min(|hits ∩ tie_boundary|, k − |strict_above|)`、
  分母を `min(k, 可視件数)` とする（`tie_boundary` からの充当を残り枠で頭打ち
  にし、`strict_above` の欠落を `tie_boundary` の過剰一致で埋め合わせない。
  codex-review 指摘対応）。厳密一致版は `recall_at_10_strict` として併記する。
- **hybrid の疎スコアリング方式・融合条件が揃っていない**（Issue #661。`hybrid_rrf`・
  `bulk_hybrid_k200` に共通）: RRF 定数（`1 / (60 + rank)`）自体は self・pgvector・
  sqlite-vec の 3 構成で一致し、LanceDB も既定 reranker
  `lancedb.rerankers.RRFReranker`（インストール済み 0.38.0 で
  `inspect.signature(RRFReranker.__init__)` を確認すると `K: int = 60`）を
  明示指定せず使っているため定数自体は揃っている。揃っていないのは以下:
  - 疎スコアの方式そのもの: self は自作 BM25 転置索引（`sparse.rs`。小文字化＋CJK
    ユニグラム／バイグラム＋ストップワード除去）、sqlite-vec は FTS5
    `bm25(docs_fts)`（既定トークナイザ）、pgvector は BM25 ではなく
    `ts_rank(body_tsv, plainto_tsquery('english', …))`（TF-IDF 系・`english`
    stemming 辞書）、LanceDB はネイティブ hybrid の内部実装に依存。
  - 候補プール深さ: self は `k_eff.max(DEFAULT_HYBRID_POOL_DEPTH)`
    （`sql/exec.rs` の `DEFAULT_HYBRID_POOL_DEPTH = 200`）で `hybrid_rrf`・
    `bulk_hybrid_k200` とも 200。pgvector・sqlite-vec は `hybrid_rrf` が
    密／疎とも LIMIT 50（`pgvector_db.py`・`sqlite_vec_db.py`）で
    `bulk_hybrid_k200` のみ 200 に揃えている（コメントで意図的な使い分けと明記）。
    LanceDB の候補プール深さはライブラリ内部で非露出。
  - self 固有の同点タイブレーク規約（`TieRank::GroupEnd`・境界同点グループ完全化。
    Issue #310）は他 DB に対応物が無い。
  - 上記いずれも「どちらが正しいスコアリングか」を判定する目的の計測ではなく、
    各 DB の素の hybrid 実装を比較しているため、`hybrid_rrf`／`bulk_hybrid_k200`
    の速度・品質差には方式差そのものの寄与が混入している。
- **距離指標が揃っていない**（Issue #661）: sqlite-vec（vec0）は内積に対応せず
  cosine 近似で計測している（「計測環境」表に既記載）。したがって sqlite-vec の
  KNN 系レイテンシ・`recall_at_10`／`recall_at_10_strict` は他 DB（すべて内積／
  `dot`）と同一の距離指標での比較ではない。
- **可視性モデルの実コストが非対称**（上記「可視性モデル」箇条書きの帰結。
  Issue #661）: self は wire セッションのポリシー（RLS 相当）で
  `visible()` UDF・TABLE-12 検査を全読み取り経路で実際に払うのに対し、他 DB は
  `visibility` 列＋btree 索引への `WHERE visibility = 'public'` 述語（
  `common.py::public_only_where` 相当）を模倣しているだけで、ポリシー強制の
  実コストは払っていない。
- **pgvector の HNSW 索引が使われていない可能性**（Issue #661）: `explain`
  フェーズ（`WHERE visibility='public' ORDER BY <#> LIMIT 10` と同形のクエリ）の
  `sample_output` は `pgvector_exact.json`・`pgvector_hnsw.json`（いずれも
  `docs/design/bench-data/crossdb-20260908/results/`）で完全同一の実行計画
  （`Bitmap Index Scan on docs_visibility_idx` → `Bitmap Heap Scan`
  → `Sort` → `Limit`。推定 `rows=125`）であり、HNSW 構成でも索引スキャンが
  選ばれていないことを直接確認できる（このクエリ形についてのみの直接証拠。
  `vector_knn_where`／`bulk_knn_where_k200` は同型の推定にとどまる）。整合する
  観測として、両構成の p50 がほぼ同値（2026-09-08 実測: `vector_knn`
  3854/3804 µs・`vector_knn_where` 2072/2135 µs・`bulk_knn_where_k200`
  2500/2477 µs）・`recall_at_10_strict` が 0.9865 で完全一致している。
  `pgvector_db.py` に `ANALYZE`／`VACUUM` の呼び出しが無く（`COPY` 直後の
  統計未更新）、`visibility='public'`（実際は 23,000/25,000 行）の推定行数が
  既定選択率由来の 125 行になっていることが一因の仮説（上流 pgvector の
  「フィルタ付きでは planner 判断で索引が使われないことがある」という趣旨の
  記述と同型の事象。手法名・事実のみの帰属で本文は転記しない）。下記「所見」
  節「ANN の効果」の pgvector 行はこの読み替えが必要（規模による効果の無さ
  ではなく索引不使用の可能性）。

## 横断ベンチ実測（25,000 行・dim 128・k=10）

| フェーズ | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `vector_knn` | 786 / 1147 | 3551 / 4033 | 3623 / 4198 | 1971 / 2041 | 729 / 864 | 559 / 627 | 9251 / 13606 | 2407 / 2754 | n/a |
| `vector_knn_where` | 2819 / 3047 | 2159 / 2430 | 2151 / 2173 | 1842 / 1897 | 732 / 808 | 615 / 721 | 8355 / 9656 | 3882 / 5176 | n/a |
| `where_compound_count` | 3903 / 4248 | 1595 / 1620 | 1583 / 1706 | 950 / 958 | 7698 / 10959 | 7573 / 9676 | 970 / 1134 | 1020 / 1294 | 3063 / 3920 |
| `agg_count` | 3547 / 3714 | 1636 / 1700 | 1668 / 1781 | 616 / 626 | 1768 / 2282 | 1414 / 1765 | 685 / 898 | 792 / 872 | 1794 / 1977 |
| `agg_multi` | 3738 / 4079 | 1915 / 1947 | 1950 / 1998 | 1735 / 1761 | n/a | n/a | 7066 / 8810 | 7172 / 8679 | 2516 / 2580 |
| `group_by_having` | 3948 / 4495 | 2887 / 3037 | 2958 / 3118 | 2735 / 2754 | n/a | n/a | 9088 / 11634 | 9474 / 13155 | 16513 / 16710 |
| `hybrid_rrf` | 6178 / 9100 | 4764 / 5206 | 4892 / 5275 | 3508 / 4176 | n/a | n/a | 10328 / 13194 | 3820 / 4419 | n/a |
| `mode_recall` | 726 / 812 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `mode_precision` | 721 / 1096 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `udf_call` | 730 / 1149 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `rls_isolation` | 3552 / 3675 | 1623 / 1644 | 1657 / 1701 | 620 / 634 | 1761 / 2356 | 1406 / 1786 | 662 / 828 | 653 / 833 | 1792 / 1905 |
| `explain` | n/a | 113 / 123 | 122 / 166 | 4 / 4 | n/a | n/a | 1054 / 1503 | 589 / 717 | 133 / 139 |

n/a は当該 DB にその機能が無い（または wire から到達できない）ことを示し、結果 JSON には
`unsupported` と理由を fail-closed で記録している。`mode_recall`／`mode_precision`
（TASK-161）・`udf_call`（TASK-79）は self 固有機能。self の `explain` は `USING PLAN` 文
専用（TASK-78）のため通常 SELECT では n/a。

| 指標 | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `ingest_bulk` | n/a | 0.50 s（49,744 rows/s） | 0.49 s（50,599 rows/s） | 0.27 s（93,138 rows/s） | 2.00 s（12,516 rows/s） | 2.04 s（12,241 rows/s） | 0.20 s（125,093 rows/s） | 0.17 s（151,040 rows/s） | 1.61 s（15,501 rows/s） |
| `ingest_single_stmt` | 0.13 s（7,774 rows/s） | 0.64 s（1,562 rows/s） | 1.27 s（785 rows/s） | 0.09 s（11,604 rows/s） | 0.97 s（1,028 rows/s） | 1.51 s（661 rows/s） | 1.35 s（741 rows/s） | 1.46 s（685 rows/s） | 1.45 s（687 rows/s） |
| `recall_at_10` | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 0.8360 | n/a |
| `recall_at_10_strict` | 0.9995 | 0.9865 | 0.9865 | 0.9740 | 0.7570 | 0.7735 | 0.9990 | 0.6125 | n/a |

self の `ingest_bulk` は wire に COPY 相当が無く（`EngineCore::execute_insert_sql_batch`
は Rust API のみ）n/a。`ingest_single_stmt` は行形 INSERT を `USING OPERATION_ID`
付きで 1,000 行送る（commit 粒度は全 DB とも 1 文ごと〔pgvector は autocommit、MySQL・
sqlite-vec は 1 文ごとに commit、Qdrant は `wait=True`、LanceDB は 1 行ずつ `add`〕。7,473 rows/s。pgvector 1,601・Qdrant 1,081 rows/s より速く、in-process の sqlite-vec 11.7k rows/s には及ばない）。
下表の `7,774 rows/s` と本文の `7,473 rows/s` はいずれも初回計測時の別 run の値
（tmpfs 帯。fixture の置き場は未記録）で、値の混在は初回記録の誤記ではない。
2026-09-08 の tmpfs 対照計測では 7,414〜7,714 rows/s を再現しており、初回計測も
tmpfs 相当の FS 条件だったと推定される（下記「2026-09-08 再計測」節参照）。

sqlite-vec の `ingest_bulk`／`ingest_single_stmt` はファイルベース化前
（`:memory:`）の値（「公平性の注記」節参照）。ファイルベース化後の値は 2026-09-08
再計測で取得済み（下記「2026-09-08 再計測」節参照）。

### 所見

- **フィルタなし KNN（exact）**: self 786 µs（p50）は Qdrant exact 729 µs と同等で、
  pgvector exact 3,551 µs・sqlite-vec 1,971 µs・LanceDB exact 9,251 µs より速い。
  Recall@10（同点許容）は self を含む exact 全構成で 1.0。
- **フィルタ付き KNN・集計・GROUP BY・hybrid**: self は 2.8〜6.1 ms で、pgvector
  （1.6〜4.8 ms）・sqlite-vec（0.6〜3.5 ms）より遅い。in-process の `feature_bench`
  では同種フェーズが 1 ms 台であり、差分の主因は wire 往復＋テキスト応答の組み立て
  と SQL 表層のフィルタ経路（`docs/design/c1-p95-dedicated-env-reverification.md`）。
  MySQL の `GROUP BY`/`HAVING` は 16.5 ms と突出して遅い。
- **投入**: 一括投入は LanceDB（125k〜151k rows/s）＞ sqlite-vec ＞ pgvector ＞ MySQL ＞ Qdrant。
- **ANN の効果**: 25,000 行では pgvector HNSW と exact の差はほぼ無く（3.6 ms 前後）、
  LanceDB HNSW は 9.3→2.4 ms と速くなる代わりに Recall@10 0.836 へ低下した。Qdrant HNSW
  （構築完了確認後）は 729→559 µs で Recall@10 1.0 を維持した。**pgvector の
  「差はほぼ無い」は規模（25,000 行）による効果の乏しさではなく、HNSW 索引が
  そもそも使われていない可能性が高いと判明した**（上記「公平性の注記」
  「pgvector の HNSW 索引が使われていない可能性」参照。Issue #661）。

## 広域取得（LLM へ丸ごと渡す用途）の実測

本 DB の設計思想は「正解を含むデータ群を広く返す」ことにあり、上位数件を精密に
並べ替えるより、必要な文書が漏れなく含まれる集合を安価に返すことを狙っている。
この用途を模した「広域取得」5 フェーズ（`id` と `body` の両方を返す。LLM へ
渡す本文の送出コストを含める）を全 DB に追加して計測した。

**前提の確認（現行実装。Issue #454 で解消済み）**: 本計測の実施時点では、SQL 表層の
許可リスト（`crates/engine/src/sql/allowlist.rs::parse_select_shape`）は行を返す
`SELECT` に `ORDER BY <距離>` か `USING PLAN` を必須とし、ORDER BY なしの
スカラーフィルタのみの行取得は受理しなかった（`scan_where_nosort_k500` は実行して
拒否を捕捉し unsupported とした。下表の self 列 `n/a` はこの時点の計測結果）。
Issue #454 で `SELECT ... [WHERE ...] LIMIT n`（`ORDER BY`／`USING PLAN` を伴わない
広域取得。契約は `docs/design/wide-retrieval-scan.md` 参照）を SQL 表層へ追加した
ため、`scan_where_nosort_k500` は self でも受理される（許可リストが `42601` を
返さなくなった）。self の実測値は 2026-09-08 再計測で取得済み（463／475 µs〔500 行〕。
下記「2026-09-08 再計測」節参照。下表・下記所見の self `n/a` は本計測時点のまま
未更新で残置し、値は新節側にのみ記録する）。`USING MODE 'recall'` は Top-k を固定件数で返す
モードであり、しきい値で件数が可変になる構文は Issue #454 でも実装していない
（spec ID 確定待ちのため見送り。ADR 参照）。

| フェーズ | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `bulk_knn_k200` | 7993 / 10019（200 行） | 4455 / 5143（200 行） | 4443 / 4630（200 行） | 5280 / 11497（200 行） | 3213 / 4272（200 行） | 3097 / 3377（200 行） | 10446 / 13536（200 行） | 3865 / 4500（200 行） | n/a |
| `bulk_knn_k1000` | 11151 / 15399（1000 行） | 4989 / 5281（1000 行） | 5049 / 5143（1000 行） | 28729 / 45359（1000 行） | 12605 / 14087（1000 行） | 12761 / 17248（1000 行） | 12208 / 15042（1000 行） | 6029 / 7254（1000 行） | n/a |
| `bulk_knn_where_k200` | 4661 / 4889（200 行） | 2438 / 2473（200 行） | 2456 / 2476（200 行） | 13613 / 13865（200 行） | 3168 / 5409（200 行） | 3288 / 3682（200 行） | 9460 / 11038（200 行） | 5037 / 6306（200 行） | n/a |
| `bulk_hybrid_k200` | 9370 / 11789（200 行） | 5117 / 5531（200 行） | 5287 / 5900（200 行） | 6425 / 11701（200 行） | n/a | n/a | 11552 / 14278（200 行） | 5285 / 6522（200 行） | n/a |
| `scan_where_nosort_k500` | n/a | 999 / 1012（500 行） | 1003 / 1022（500 行） | 242 / 247（500 行） | 4696 / 8082（500 行） | 4885 / 5838（500 行） | 2395 / 3011（500 行） | 2190 / 2801（500 行） | 893 / 940（500 行） |

ANN 構成は候補幅を 3 DB とも同じ規則 max(64, k) へ引き上げて計測した（pgvector
`hnsw.ef_search`〔終了後 64 へ復帰〕、Qdrant `hnsw_ef`、LanceDB `ef`。hybrid は候補
プール 200 に対して 200）。
`bulk_hybrid_k200` の pgvector・sqlite-vec は候補プールを各 200 へ拡大（既存
`hybrid_rrf` は各 50）。Qdrant の payload には本計測から `body` を含めたため
`ingest_bulk`／`ingest_single_stmt` の値は前回計測と条件が異なる。

### self の投影コスト切り分け（wire 経由・フィルタなし・p50 / p95 µs）

`bulk_knn_k200` の self が `vector_knn`（0.7 ms）の約 11 倍になる原因を、投影列と
k を変えて切り分けた（`scratchpad` 上の ad-hoc 計測・50 反復・同一条件）。

| 投影 | k=10 | k=200 | k=1000 | k=5000 |
| --- | --- | --- | --- | --- |
| `id` | 716 / 1095 | 1423 / 2411 | 4212 / 4951 | 17331 / 18581 |
| `id, lang` | 9804 / 13191 | — | 14850 / 16643 | — |
| `id, body` | 11245 / 12562 | 12564 / 16698 | 15717 / 18068 | 30571 / 32569 |

表は連続する 2 回の ad-hoc 実行（1 回目: `id`／`id, body` × k=10〜5000、2 回目:
`id, lang` を加えた k=10／1000）を合成したもので、両実行の `id` k=10 は 716 と 761
（この幅が run-to-run のノイズ帯）。スカラー列を 1 列でも投影すると、k に依存しない
約 9〜10 ms の固定コストが乗る。
原因は `crates/engine/src/sql/exec.rs` の `on_visible_row`（RLS→SCALAR 段）が
投影列を含む場合に全可視行（25,000 行）へ `row_codec::scan_scalar_columns`
（UTF-8 検証・列複製）を適用してから Top-k を選出する構造にあり、`project_rows`
は Top-k 確定後に構築済みの列を読むだけで LIMIT はこの段のコストに効かない。
`SELECT id` だけは `SqlArenaCache` の恒等写像高速経路（`cache_fast_path_eligible`:
フィルタなし・hybrid でない・投影がスカラー列を参照しない）に乗るため全行デコードを
省略できる。Issue #350 で集計経路に導入した必要列限定デコード（`DecodeTier`）は
`sql/exec.rs` の通常 SELECT 経路には結線されていない。

### 所見（広域取得・フィルタ＋ソート）

- **広域取得は self が「かなり速い」状態にはない（中位）。** `bulk_knn_k200` の self
  8.0 ms は Qdrant 3.1〜3.2 ms・LanceDB HNSW 3.9 ms・pgvector 4.4〜4.5 ms・sqlite-vec 5.3 ms
  より遅い。`bulk_knn_k1000` の self 11.2 ms は pgvector 5.0 ms・LanceDB HNSW 6.0 ms
  より遅く、Qdrant 12.6 ms・LanceDB exact 12.2 ms・sqlite-vec 28.7 ms よりは速い。
- **差の主因は検索本体ではなく投影の全行デコード。** `id` のみなら k=1000 でも
  4.2 ms で pgvector（5.0 ms）と同等以上であり、スカラー列投影を Top-k 確定後の
  k 行に限定できれば `id, body` k=1000 も 5 ms 前後（`id` 4.2 ms ＋ 124 KB の本文
  送出）まで下がる余地がある。この改善は production コード（`sql/exec.rs`）の変更を
  伴うため本 PR の対象外とし、Issue #453 として起票した。
- **フィルタ＋ソート（既存 DB 型の使い方）でも self は中位〜下位。** k=10 の
  `vector_knn_where` は self 2.8 ms に対し Qdrant 0.6〜0.7 ms・sqlite-vec 1.8 ms・
  pgvector 2.2 ms。k=200 の `bulk_knn_where_k200` は self 4.7 ms に対し pgvector 2.4〜2.5 ms・
  Qdrant 3.2〜3.3 ms（sqlite-vec は 13.6 ms と悪化）。フィルタ付きは `SELECT id` でも
  高速経路から外れ、全行の `scan_scalar_columns` を通るため、投影と同じ構造的コストが
  約 2 ms 分乗っている。
- **ソートなしのスカラーフィルタ取得**: 本計測の実施時点（オーナー確認
  2026-09-05）では self に経路が無く、他 DB が 0.24 ms（sqlite-vec）〜4.9 ms
  （Qdrant scroll）で 500 行を返す一方 self は unsupported だった。設計思想どおりの
  「フィルタのみで広く返す」経路は Issue #454 として起票し、SQL 表層
  （`Statement::Scan`。`docs/design/wide-retrieval-scan.md`）へ実装済み。self の
  実測値は 2026-09-08 再計測で取得済み（463／475 µs〔500 行〕。上表の self 列は
  本計測時点の `n/a` のまま残置し、値は下記「2026-09-08 再計測」節に記録する）。
- **ベクトル検索の本体は速い。** `SELECT id` の k=10 0.7 ms・k=1000 4.2 ms は brute-force
  ながら Qdrant exact と同等で、投影・フィルタ経路のオーバーヘッドを取り除けば広域取得
  でも上位に入る見込み。

## wire-server の 40 ms 下限と `TCP_NODELAY` 修正

初回計測では self の全フェーズが p50 42〜48 ms（1 ms 刻みで量子化）に張り付き、
psql でも `SELECT COUNT(*)` が約 45 ms だった（in-process では 1 ms 台）。原因は
`crates/wire-server/src/server.rs` の accept 直後に `TCP_NODELAY` を設定しておらず、
簡易クエリ応答が RowDescription／DataRow／CommandComplete／ReadyForQuery の 4 回の
小さな `write_all` に分かれるため Nagle アルゴリズムと受信側 delayed ACK が相互作用
していたことによる（PostgreSQL 本体・libpq も接続に `TCP_NODELAY` を設定する慣行）。

`accept_loop_inner` の `apply_read_timeout` 直後で `set_nodelay(true)` を設定し（失敗時は
同様に当該接続をスキップする fail-closed）、回帰テスト
`crates/wire-server/tests/wire_nodelay_latency.rs`（20 往復の中央値 < 20 ms）を追加した。
psql の `SELECT COUNT(*)` は 45 ms → 約 3.7 ms。

| フェーズ | 修正前 p50 | 修正後 p50 | 修正後 p95 |
| --- | --- | --- | --- |
| `vector_knn` | 41992 | 786 | 1147 |
| `vector_knn_where` | 44003 | 2819 | 3047 |
| `where_compound_count` | 45009 | 3903 | 4248 |
| `agg_count` | 44966 | 3547 | 3714 |
| `agg_multi` | 44996 | 3738 | 4079 |
| `group_by_having` | 45000 | 3948 | 4495 |
| `hybrid_rrf` | 48001 | 6178 | 9100 |
| `mode_recall` | 41993 | 726 | 812 |
| `mode_precision` | 41995 | 721 | 1096 |
| `udf_call` | 41996 | 730 | 1149 |
| `rls_isolation` | 44960 | 3552 | 3675 |

**（2026-09-06 追記・Issue #481）** 簡易クエリ応答（`RowDescription`/`DataRow`×N/
`CommandComplete`/`ReadyForQuery`）は原則 1 回の `write_all` へ束ねる方式へ変更した
（`crate::response_buffer::ResponseBuffer`。上限 `MAX_RESPONSE_BUFFER_BYTES` 超過時は
フレーム境界で分割送出）。上表の 4 回 `write_all` を前提とした記述はこの変更以前の
挙動。詳細・実測は `docs/design/wire-response-buffering.md` 参照。

## GPU（NVIDIA GeForce RTX 3060）での高速化

### `engine::gpu_batch` vs CPU-SIMD バッチ（`make bench-gpu-scaling`）

`crates/engine/benches/gpu_scaling_bench.rs` は `GpuBatchBackend`（f16 パック常駐・
内積のみ GPU・Top-k は CPU `TopKSelector`・スコアバッファ 32 MiB 分割）・
`GpuF32ContrastBackend`（f32 常駐対照・Issue #234）・`batch_search.rs::BatchEngine`
（CPU-SIMD f16 常駐・12 スレッド）を同一コーパス・同一クエリで比較する。
`mismatch` は GPU と CPU の Top-k 結果の不一致数（全点 0）。

**旧実測表（変更前・2026-09-05 以前）**。dispatch 構造が「1 dispatch = 1
クエリ」だった時点の数値。Issue #532（PR #567）でタイル化方式へ変更したため
下記は履歴として残す（batch≥64 で GPU f16 が CPU-SIMD と同等〜逆転していた
参照点）。

**（2026-09-07 追記・Issue #540）** f16 算術版 S0 シェーダ（Issue #539。
`SHADER_F16` 対応アダプタで読み出し直後に f32 へ拡張して積和する経路）の
前後比較を実施した。クエリが f16 へ厳密往復可能な場合のみシェーダが
切り替わる契約（`select_dot_shader`）のため、opt-in `BENCH_GPU_SCALING_
QUERY_F16_EXACT=1` でクエリを丸めない限り本表が選択する S0 シェーダは
unpack 版のままで不変（`AdaptiveShaderSelector::resolve` は呼び出しごとに
f16 算術版ガードを評価してから縮退するため、性能への影響がないとは断定
しない）。CORE-16 ゲート・規模点診断のクエリ生成も同じ理由で選択される
シェーダが構造的に不変（f16 算術版を経由しない）。詳細・実測値は
`gpu-batch-f16-arith.md`「8. 前後比較実測（Issue #540）」節・
`core16-f16-resident-gate.md`「Issue #540 追記」節参照。

**（2026-09-07 追記・Issue #543）** opt-in 専用の i8 パック常駐経路
（`engine::gpu_batch::packed_i8::GpuI8BatchBackend`。Issue #542。候補生成
のみ・primary 未接続）の前後比較・Recall 影響を実測した。Recall@10 の
劣化は観測されなかった一方、クエリタイル化未実装（1 dispatch = 1 クエリの
まま）が原因と分析される速度低下（既存 f16 常駐経路比で最大 19.4x 遅い）
を確認した。詳細・実測表は `gpu-batch-i8-packed.md`「前後比較実測
（Issue #543）」節参照。

| rows | dim | batch | CPU-SIMD | GPU f16 | GPU f32 | per-query CPU | per-query GPU f16 | speedup f16 (p95) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 1 | 3676 | 180 | 207 | 3676 | 180 | 20.17x |
| 20,000 | 128 | 8 | 5008 | 1448 | 1664 | 626 | 181 | 3.45x |
| 20,000 | 128 | 64 | 16537 | 11672 | 13387 | 258 | 182 | 1.39x |
| 20,000 | 128 | 256 | 57401 | 57250 | 55457 | 224 | 223 | 0.98x |
| 20,000 | 256 | 1 | 6352 | 218 | 349 | 6352 | 218 | 28.76x |
| 20,000 | 256 | 8 | 8499 | 1826 | 2795 | 1062 | 228 | 4.58x |
| 20,000 | 256 | 64 | 27668 | 15515 | 22701 | 432 | 242 | 1.67x |
| 20,000 | 256 | 256 | 92500 | 56726 | 90891 | 361 | 221 | 1.61x |
| 100,000 | 128 | 1 | 18802 | 1351 | 2338 | 18802 | 1351 | 13.77x |
| 100,000 | 128 | 8 | 25594 | 10053 | 18100 | 3199 | 1256 | 2.53x |
| 100,000 | 128 | 64 | 82954 | 80756 | 145908 | 1296 | 1261 | 0.98x |
| 100,000 | 128 | 256 | 282268 | 324617 | 587253 | 1102 | 1268 | 0.86x |
| 100,000 | 256 | 1 | 32083 | 2972 | 6318 | 32083 | 2972 | 10.32x |
| 100,000 | 256 | 8 | 42645 | 17449 | 50092 | 5330 | 2181 | 2.21x |
| 100,000 | 256 | 64 | 138425 | 138479 | 406175 | 2162 | 2163 | 0.99x |
| 100,000 | 256 | 256 | 459848 | 550063 | 1627532 | 1796 | 2148 | 0.83x |
| 500,000 | 128 | 1 | 93955 | 6637 | 9799 | 93955 | 6637 | 13.77x |
| 500,000 | 128 | 8 | 128757 | 53269 | 79643 | 16094 | 6658 | 2.39x |
| 500,000 | 128 | 64 | 413906 | 431620 | 637257 | 6467 | 6744 | 0.94x |
| 500000 | 128 | 256 | skip（rows×batch×dim が MAX_BATCH_WORK 超過） | | | | | |
| 500,000 | 256 | 1 | 187677 | 9548 | 31713 | 187677 | 9548 | 18.83x |
| 500,000 | 256 | 8 | 239812 | 76357 | 253370 | 29976 | 9544 | 3.08x |
| 500,000 | 256 | 64 | 715567 | 613496 | 2037477 | 11180 | 9585 | 1.15x |
| 500000 | 256 | 256 | skip（rows×batch×dim が MAX_BATCH_WORK 超過） | | | | | |

- **batch=1**: GPU f16 は 13.8〜28.8 倍高速（500k×256 で 187.7 ms → 9.5 ms）。
- **batch=8**: 2.2〜4.6 倍。
- **batch≥64**: 0.83〜1.67 倍で CPU-SIMD（12 コア並列）と同等〜逆転。GPU 側の
  クエリあたりコストが規模ごとにほぼ一定（20k で約 180 µs、500k×128 で約 6.6 ms）で
  バッチ化の効果が出ていない。
- f16 常駐は f32 常駐より一貫して速い（500k×256 batch 8 で 76 ms vs 253 ms）。

### #532 クエリタイル化の前後比較（Issue #533）

Issue #532（PR #567・merge commit `59bc7a8`）はバッチ検索 GPU 経路の dispatch
構造を「1 dispatch = 1 クエリ」から「`PolicyContext` 単位にグループ化した
クエリを最大 `GPU_QUERY_TILE_MAX`（実装既定値 16）本まで 1 dispatch へタイル化」
する方式へ変更した。本節は `docs/design/benchmark-judgement-policy.md` の
計測規約（交互 N≥5 ペア・per-run 生データ必須・min-of-N＋median 併記・
固定 ±5% と参照区間実測の 2 種ノイズ帯）に沿って計測した記録である。
初回計測（2026-09-05 前後）は本節の per-run 生データが `.gitignore` 対象
（`_/`）にしか出力されず復元不能だったため codex-review P1 指摘（PR #580・
スレッド `PRRT_kwDOUAKASM6fwR6p`）を受け、`scripts/bench_gpu_scaling_ab.sh`
冒頭コメントの運用注記（tracked 保存先への複製）に従って**再計測した**。
本節は再計測値（`docs/design/bench-data/gpu-scaling-ab/20260907T082358Z-summary.tsv`）
で置き換えたものであり、初回計測の集約値は削除する（per-run 生データを
伴わない集約値は規約 §3 を満たさず再現できないため）。

**前提**:

- before: `b161d5b`（#567 マージの親）／after: `59bc7a8`（#567 マージコミット）。
  `git diff --stat b161d5b 59bc7a8` の変更は `gpu_batch.rs`・
  `tests/gpu_batch.rs`・docs・`CLAUDE.md` のみで、`Cargo.lock`・
  `benches/gpu_scaling_bench.rs`・`benches/harness/*`・`batch_search.rs` は
  不変（同一ハーネス・同一ビルド条件での前後比較が成立する）
- 現 `origin/main` は使わない。Issue #536（workgroup 内部分 Top-k）が #567 の
  後に入っており、readback 方式が交絡するため（#536 後の再計測・readback
  バイト数比較は Issue #537 の担当）
- 1 run = 1 プロセス = 1 (rows, dim, batch) 規模点（規約 §6「複数規模点の
  同一プロセス内逐次比較は不可」に従う）。per-run 値は `gpu_scaling_bench` が
  1 プロセスあたり出す 20 反復（warmup 20 別）の p50／p95
- 参照区間 A = CPU-SIMD 経路（`gpu_batch.rs` を一切通らず #532 の変更を含まない。
  before/after 双方の CPU-SIMD p50 を同一母集団として実測ノイズ帯を算出。
  規模点ごとに before n=5／after n=5 の計 n=10）
- 環境: 本開発環境（QEMU Virtual CPU・12 vCPU・avx2/fma/f16c・NVIDIA GeForce
  RTX 3060・driver 595.71.05・Vulkan backend）。計測全体（5 規模点 × 5 ペア ×
  before/after）を通した各 run 開始時点の loadavg（1 分平均。下記 tracked TSV
  の `loadavg_1m` 列の全 50 run 分布）は 1.57〜8.79（他セッションの同時実行を
  含む共有環境）で `BENCH_DEDICATED_ENV` は未設定 → **参考値・採否根拠に
  しない**（規約 §5）。#532 は Issue #402 系 ADR の対象外の独立変更であり、
  本計測は「効果の記録」であって production コードの採否判定ではない
  （#532 は既にマージ済み）
- rows 3 種 × dim 2 種 × batch 4 種の既定格子 24 点のうち時間予算内で計測できた
  5 点（FAISS 対照点 100,000×128×64 を優先し、batch=1/8/64 の代表点・大規模点を
  選定）を実測。残りは「未計測」（旧値のまま埋めない）
- **per-run 生データ**: `docs/design/bench-data/gpu-scaling-ab/20260907T082358Z-summary.tsv`
  （tracked。50 行＝5 規模点 × 5 ペア × before/after）。各行に
  `cpu_p50`/`cpu_p95`/`f16_p50`/`f16_p95`/`f32_p50`/`f32_p95`/`mismatch` に加え、
  規約 §3「環境記録」・§7.1 チェックリストが求める各 run 時点の `loadavg`（1/5/15分
  平均）を `loadavg_1m`/`loadavg_5m`/`loadavg_15m` 列として保持する（codex-review
  P1 指摘・PR #580・スレッド `PRRT_kwDOUAKASM6f1tz8` で追加。当初は summary の
  数値列のみを tracked 化しコメントで「規約が求めるのは summary の保持のみ」と
  誤って記載していたが、規約は各 run の loadavg も要求するため訂正した）。
  値は `_/bench/gpu-scaling-ab/<point>/<pair>-<side>.log` の
  `# pre-run environment snapshot` 直後の `loadavg:` 行（run 開始直前に採取した
  1 回分のスナップショット）から抽出したもの。生ログ全文（`loadavg` の 5 番目の
  フィールド以降・GPU クロック/温度・`gpu_scaling_bench` 出力全文）は
  `_/bench/gpu-scaling-ab/`（git 管理外）にのみ存在し tracked 化しない（規約が
  求めるのは summary への集約値・loadavg の保持であり、生ログ全文の保持までは
  求めない。詳細は下記「per-run 生データの所在」節参照）

**実測記録表（f16 常駐。単位 µs、N=5 ペア）**

| rows | dim | batch | CPU-A 参照帯 (%) | f16_p95 before (min/med) | f16_p95 after (min/med) | ratio (min-of-N) | ratio (median) | 判定クラス | per-query after (µs) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 8 | 2.40 | 1461 / 1517 | 678 / 789 | 0.464 | 0.520 | Improved | 84.8 |
| 20,000 | 128 | 64 | 3.43 | 11722 / 15082 | 5861 / 8664 | 0.500 | 0.574 | Improved | 91.6 |
| 100,000 | 128 | 1 | 6.64 | 1385 / 1410 | 1415 / 1452 | 1.022 | 1.030 | Neutral | 1415.0 |
| 100,000 | 128 | 64 | 2.55 | 82273 / 82778 | 30070 / 32144 | 0.365 | 0.388 | Improved | 469.8 |
| 500,000 | 128 | 64 | 8.01 | 437659 / 444921 | 137537 / 145789 | 0.314 | 0.328 | Improved | 2149.0 |

- **表の集計対象フィールド**: `f16_p95 before/after (min/med)` 列は
  `summary.tsv` の `f16_p95`（各 run の p95 レイテンシ）を N=5 ペアの
  母集団として min（最小値）・median（中央値）を取ったもの。`CPU-A 参照帯` は
  同一母集団（before n=5 + after n=5 = n=10）の `cpu_p50` から
  `(max - min) / min` で算出した実測ノイズ帯（規約 §4 の相対比率表記）。
  `判定クラス` は固定 ±5% 帯・実測ノイズ帯（CPU-A 参照帯）の両方を
  `ratio (min-of-N)` が超えた場合のみ `Improved`/`Regressed`、いずれかに
  収まる場合は `Neutral` とする（規約 §4）。`per-query after (µs)` は
  `f16_p95 after` 列の min 値を `batch` 数で除した値（例: 100,000×128×64 は
  30070 ÷ 64 ≈ 469.8）で、p95 レイテンシ由来の値をバッチ内 1 クエリあたりに
  換算した参考値であり、per-query 自体の実測ではない
- **全 5 点が規約 §3 の per-run 生データ保持要件を満たす**（初回計測との違い。
  20,000×128×8 も再計測では CPU-A 参照帯が 2.40% と安定し、初回計測で発生した
  外れ値混入（375%）は再現しなかった）
- **batch=1（100,000×128×1）は意図どおり中立**: ratio (min-of-N) ≈ 1.022 で
  固定 ±5% 帯・実測ノイズ帯（6.64%）のいずれも超えない。これは #532 の設計
  （`PolicyContext` 単位にまとめた複数クエリの償却）が単発クエリには効かない
  サニティ確認であり、退行ではない
- **batch≥8（20k・100k・500k の 4 点）は両ノイズ帯を超える改善方向の差分**:
  ratio (min-of-N) 0.314〜0.500（変更前 GPU f16 に対して 2.0〜3.2 倍高速。
  CPU-SIMD との比較は別掲の「speedup vs CPU-SIMD」を参照）。旧実測表
  （batch≥64 で 0.94〜0.99x＝ほぼ同着）とは異なる方向の値であり、#532 が
  狙った「常駐行列の行データ読み込みをタイル幅ぶんのクエリで償却する」効果を
  示す（本環境は共有 QEMU 環境のため規約 §5 により production 変更の採用
  根拠にはできないが、#532 は既にマージ済みのため本節は効果の記録目的に限る）
- **speedup vs CPU-SIMD（after, 中央値ベース）**: 20,000×128×64 で 1.96x、
  100,000×128×64 で 2.61x、500,000×128×64 で 2.85x（after 側 CPU-SIMD p95 の
  中央値 ÷ f16 p95 の中央値）
- **mismatch**: 100,000×128×64／500,000×128×64／100,000×128×1 は全 run
  mismatch=0。20,000×128×8／20,000×128×64 は before/after とも一貫して
  mismatch=1（#532 と無関係な既存の同点許容境界の挙動と考えられ、後続 Issue の
  対象外）
- **未計測点**: rows∈{20000,100000,500000}×dim∈{128,256}×batch∈{1,8,64,256}
  の残り 19 点（dim=256 全点・batch=256 全点・20,000×128×1・500,000×128×8 等）
  は時間予算の都合で未計測。旧実測表の対応値をそのまま代用しない
- **per-run 生データの所在**: `docs/design/bench-data/gpu-scaling-ab/20260907T082358Z-summary.tsv`
  （tracked。上表の集約値・各 run の `loadavg_1m`/`loadavg_5m`/`loadavg_15m` の
  一次記録）。以後の再計測でも同じ命名規則（`<UTC timestamp>-summary.tsv`）で
  `docs/design/bench-data/gpu-scaling-ab/` へコミットし、本ドキュメントの
  実測表からそのパスを参照する運用を継続する（スクリプト側の対応する注記は
  `scripts/bench_gpu_scaling_ab.sh` 冒頭コメント参照）。初回計測（per-run
  生データ不明・集約値のみ）の記録は本節から削除した（codex-review P1 指摘・
  PR #580 の再計測により置き換え）

**再現手順**:

```sh
# before/after のバイナリを退避（それぞれ独立の CARGO_TARGET_DIR で release ビルド）
git archive b161d5b | tar -x -C /path/to/wt-before
git archive 59bc7a8 | tar -x -C /path/to/wt-after
(cd /path/to/wt-before && CARGO_TARGET_DIR=/path/to/target-before \
  cargo bench --bench gpu_scaling_bench -p engine --no-run)
(cd /path/to/wt-after && CARGO_TARGET_DIR=/path/to/target-after \
  cargo bench --bench gpu_scaling_bench -p engine --no-run)
# 交互 N=5 ペア・1 プロセス 1 規模点で実行
BEFORE_BIN=/path/to/target-before/release/deps/gpu_scaling_bench-<hash> \
AFTER_BIN=/path/to/target-after/release/deps/gpu_scaling_bench-<hash> \
  scripts/bench_gpu_scaling_ab.sh 5 100000:128:64 20000:128:8 20000:128:64 \
    100000:128:1 500000:128:64
```

`scripts/bench_gpu_scaling_ab.sh` は規模点ごとに before→after を交互実行し、
per-run 生ログ（`loadavg`・GPU クロック・`gpu_scaling_bench` の出力全文）を
`_/bench/gpu-scaling-ab/<timestamp>/` 配下に保存する薄いシェルドライバ
（引数は数値・パスの正規表現検証のみで `eval` 不使用。production コード
無変更）。

**FAISS GPU との差の更新**: 100,000×128×64 の self GPU f16 after 実測値
min=30,070µs／median=32,144µs は、下表「FAISS（IndexFlatIP）CPU vs GPU」の
同一点（GPU f32 753µs・出典コミット `559b523`）と比べると約 40〜43 倍。
Issue #460（Phase 5 親）本文が引用していた「self GPU f16 80,756µs（旧実測表）→
FAISS 753µs で約 107 倍」から、#532 の効果でこの倍率は約 1/2.7 に縮小した
（それでも FAISS が大きく優位。残る差は #534〜#537 の readback 量削減・
部分 Top-k の担当）。

**申し送り**: 現 `origin/main`（#536 部分 Top-k 込み）での再計測・readback
バイト数比較は Issue #537、残り 19 規模点の計測は必要になった時点で別途
起票する。

**（2026-09-07 追記・Issue #544）** Phase 5（#532・#536・#539・#542）全適用
通し比較・FAISS GPU 対照・Qdrant GPU 構築対照の更新・UMA（Apple Metal）
ゼロコピー静的確認を実施した。self（after・`a40edd7`）の
`100,000×128×64` GPU f16 p95 min-of-5 は 16,355µs（`b161d5b`→`a40edd7`の
通し比較で before 81,704µs 比 0.20 倍・約 5 倍高速化。6 点中 3 点で固定帯・
実測帯とも超過する改善を確認、残り 3 点は共有環境ノイズにより判定不能）。
FAISS GPU（同一セッション再計測）との差は約 21〜26 倍で、Issue #537 時点の
局所値からほぼ横ばい（Phase 5 の f16 算術版・i8 経路は既定クエリでは
非選択のため差の縮小に寄与しない）。Qdrant GPU 索引構築は変わらず優位性
なし。詳細・全 6 点の表・UMA 静的確認・判定根拠は
[`gpu-batch-phase5-before-after.md`](gpu-batch-phase5-before-after.md) 参照
（production コード無変更・テスト専任）。

### FAISS（IndexFlatIP）CPU vs GPU

| rows | dim | batch | CPU | GPU f32 | GPU f16 | GPU f16 / CPU |
| --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 1 | 188 | 112 | 104 | 1.8x |
| 20,000 | 128 | 8 | 416 | 129 | 119 | 3.5x |
| 20,000 | 128 | 64 | 8980 | 172 | 172 | 52.2x |
| 20,000 | 128 | 256 | 14974 | 330 | 418 | 35.8x |
| 20,000 | 256 | 1 | 475 | 148 | 121 | 3.9x |
| 20,000 | 256 | 8 | 782 | 169 | 166 | 4.7x |
| 20,000 | 256 | 64 | 15967 | 220 | 296 | 53.9x |
| 20,000 | 256 | 256 | 31780 | 512 | 662 | 48.0x |
| 100,000 | 128 | 1 | 2283 | 447 | 387 | 5.9x |
| 100,000 | 128 | 8 | 3831 | 525 | 464 | 8.3x |
| 100,000 | 128 | 64 | 37958 | 753 | 833 | 45.6x |
| 100,000 | 128 | 256 | 122907 | 1416 | 1426 | 86.2x |
| 100,000 | 256 | 1 | 5059 | 472 | 455 | 11.1x |
| 100,000 | 256 | 8 | 7567 | 664 | 596 | 12.7x |
| 100,000 | 256 | 64 | 66960 | 964 | 984 | 68.0x |
| 100,000 | 256 | 256 | 249923 | 2273 | 2292 | 109.0x |
| 500,000 | 128 | 1 | 12079 | 2095 | 1788 | 6.8x |
| 500,000 | 128 | 8 | 18727 | 2495 | 2167 | 8.6x |
| 500,000 | 128 | 64 | 176946 | 3268 | 2967 | 59.6x |
| 500,000 | 128 | 256 | 692931 | 6887 | 6782 | 102.2x |
| 500,000 | 256 | 1 | 27458 | 2910 | 2141 | 12.8x |
| 500,000 | 256 | 8 | 37695 | 3485 | 2724 | 13.8x |
| 500,000 | 256 | 64 | 359958 | 4591 | 3693 | 97.5x |
| 500,000 | 256 | 256 | 1457926 | 11188 | 10954 | 133.1x |

FAISS GPU はバッチが大きいほど有利で、500k×128 batch 64 では CPU 176.9 ms → GPU f16
3.0 ms（約 60 倍）。engine の GPU 経路が batch≥64 で頭打ちになるのと対照的であり、
engine 側は Top-k を CPU で行いスコアバッファを読み戻す構造（GPU→CPU 転送量が
rows × batch × 4 バイト）がボトルネックと推定される。GPU 上での Top-k 選出
（分割 Top-k のリダクション）は改善候補だが、本タスクでは profiling を含め対象外。

### Qdrant HNSW 索引構築 CPU vs GPU

| rows | 構成 | upsert | index_build | search p50 / p95 µs | GPU ログ検出 |
| --- | --- | --- | --- | --- | --- |
| 100,000 | cpu | 3.0 s | 3.01 s | 1843 / 2509 | False |
| 500,000 | cpu | 9.2 s | 20.58 s | 955 / 1155 | False |
| 100,000 | gpu | 1.7 s | 5.03 s | 1090 / 1882 | True |
| 500,000 | gpu | 8.6 s | 21.07 s | 855 / 1153 | True |

投入中は索引構築を止め（`indexing_threshold` を行数より大きく設定）、投入完了後に
閾値を下げて構築を開始した時点から green・全件 indexed までを計時する。100k で GPU
5.0 s vs CPU 3.0 s、500k で 21.1 s vs 20.6 s と、この規模・この VM では GPU 索引構築
の利得は無い（GPU 使用はコンテナログの `Create GPU device`〔初期化〕で確認。
`Found GPU device` は列挙のみで証拠にしない）。探索 p50 は同水準（GPU は構築のみ）。

### 結論

GPU は単発〜小バッチの全件内積検索で明確に高速化する（10〜29 倍）。大バッチの
スループットでは現行実装は CPU-SIMD と同等にとどまり、FAISS との差が改善余地を示す。
GPU 経路は in-process API（`engine::gpu_batch`）のみで SQL／wire からは到達できない。

**（2026-09-07 追記・Issue #537）** Issue #536（workgroup 内部分 Top-k）適用後
の再計測: `100,000×128 batch 64` の self GPU f16 min-of-5 p95 は 15,783µs
（本節上表の Issue #532 適用前値 80,756µs から readback 方式変更〔#532 タイル
化＋#536 部分 Top-k〕を経て改善）。FAISS GPU 対照 753µs（`559b523` 時点）との
差はなお約 21 倍に縮小したが解消はしていない。Issue #536／#537 の詳細・
readback バイト数の確定的削減（12.66〜12.79x）・6 規模点の前後比較は
[`gpu-batch-topk.md`](gpu-batch-topk.md)「前後比較実測（Issue #537）」節参照。

## 計測ツール

- `scripts/crossdb_bench/`: Python ハーネス（`run.py --db self|pgvector|sqlite_vec|qdrant|lancedb|mysql --config exact|hnsw`・`containers.sh`・`run_all.sh`）。
  依存は `requirements.txt` で `==` 固定。Cargo 依存は増やしていない。
  `CROSSDB_DIM`（`run_all.sh`。十進数字のみ）・`run.py --expect-dim` で dim 別
  fixture（`docs25k-d<dim>.*`）を計測できる（Issue #466。下記「dim=768 基線」節）。
- `scripts/crossdb_bench/gpu/`: FAISS（`Dockerfile.faiss`・`faiss_batch_bench.py`）・
  Qdrant GPU（`qdrant_gpu_build_bench.py`・`containers_gpu.sh`）。
- `crates/engine/examples/seed_docs.rs`: fixture 生成（`seed`／`export`／`queries`）。dim 引数で
  任意次元数の fixture を生成できる。
- `crates/engine/benches/gpu_scaling_bench.rs`・`benches/harness/gpu_scaling.rs`・
  `tests/gpu_scaling_accept.rs`。
- `crates/engine/examples/feature_bench.rs`・`crates/engine/benches/knn_profile_bench.rs`:
  `BENCH_FEATURE_DIM`／`BENCH_KNN_PROFILE_DIM`（Issue #466。正整数・既定 128・上限 4,096・
  fail-closed）でベクトル次元数を上書きできる（`crates/engine/benches/harness/
  bench_engine.rs::parse_dim`）。

## `vector_knn` 786µs の内訳（Issue #463）

`vector_knn`（self 786/1147µs）を wire／SQL 表層／距離カーネル・Top-k の 4 区分へ
切り分ける実測を `crates/wire-server/benches/knn_wire_profile_bench.rs`
（`make bench-knn-wire-profile`）で実施した。共有開発環境（`BENCH_DEDICATED_ENV`
未設定・参考値）での 1 回の実測（min-of-5 どうし・T3 690.3µs に対する比率。
最新の実測表・計測条件は `docs/design/knn-wire-stage-profile.md`「実測結果」節
が SSOT。当所の数値はそこからの要約であり、両者が食い違う場合は
`knn-wire-stage-profile.md` を正とする）:

| 区分 | 比率 |
| --- | --- |
| SQL 表層 | 55.4% |
| 距離カーネル | 29.6% |
| wire | 11.9%（参照区間帯内） |
| Top-k（単線条件） | 1.8%（参照区間帯内） |

SQL 表層が距離カーネル自体より支配的な区分であり、wire 区分（TCP 送受信・
フレーミング・応答エンコード）の寄与は 11.9% にとどまる。詳細な計測設計・
fail-closed 検証・実測表は `docs/design/knn-wire-stage-profile.md` 参照。

## `hybrid_rrf` 6,178µs の内訳（Issue #465）

`hybrid_rrf`（self 6178/9100µs）の engine 内 hybrid 経路／SQL 表層／wire の
3 区分と、engine 内部の B0s〜B8 段別内訳を `crates/engine/benches/
hybrid_profile_bench.rs`（`make bench-hybrid-profile`）・`crates/wire-server/
benches/hybrid_wire_profile_bench.rs`（`make bench-hybrid-wire-profile`）で
実施した。共有開発環境（`BENCH_DEDICATED_ENV` 未設定・参考値）での実測要約
（詳細・実測表・per-round 生データは `docs/design/hybrid-rrf-latency-breakdown.md`
「最新基線」節が SSOT）:

| 区分（wire 側 T1p〜T3） | 比率 |
| --- | --- |
| engine 内 hybrid（T1p） | ≈27.5% |
| SQL 表層（T2−T1p） | ≈66.0%（参照区間帯超過） |
| wire（T3−T2） | ≈6.4%（5/5 ペアで正値・3/5 参照区間帯超過） |

> 旧値（28.6%／67.1%／4.3%）は T2（`sql_surface_hot`）を fixture 投入後の
> main スレッド上で計測していたハーネス側のアーティファクトの影響を受けて
> おり、sql_surface を過大に・wire を過小（参照区間帯内）に記録していた。
> Issue #634（ハーネス是正・T2 の新規スレッド計測化）・Issue #637（本表の
> 訂正）で是正済み。SSOT は `docs/design/hybrid-rrf-latency-breakdown.md`
> 「訂正版帰属表（Issue #637）」節（#634 after arm・5 ペア平均。分母は
> T3 min 平均）。

engine 内部の B0s〜B8 段別内訳では SQL 表層固定コスト（B1−B4）と疎側再取得
ループ（B5）がほぼ同水準（37〜39%）で並び最大区分であり、融合・境界同点
グループ完全化を含む残差は 19% にとどまることを確認した。Phase 6（Issue #548）
への引き継ぎ内容もあわせて同節に記録。production コード
（`crates/engine/src/`・`crates/wire-server/src/`）は無変更。

Phase 6（#546・#549）を通しで前後比較した crossdb self（wire 経由）の
`hybrid_rrf` 前後表は `docs/design/hybrid-rrf-phase6-before-after.md`
（Issue #550）に記録した（数値は同 doc 参照。本節では転記しない）。

## dim=768 基線（Issue #466）

### 目的

Issue #365（isa.rs dot カーネルの複数アキュムレータ化検討）の実測で dim が
採否の判別変数になることが判明したが、横断 SQL ベンチ（`feature_bench`・
`knn_profile_bench`・`scripts/crossdb_bench/`）はいずれも dim=128 の単一規模点
しか持たず、dim>=768 限定ディスパッチ等の後続施策（例: Issue #517）を横断 SQL
経路で判定する材料が無かった。本節はその計測基盤整備（`BENCH_FEATURE_DIM`／
`BENCH_KNN_PROFILE_DIM`／`CROSSDB_DIM`・`run.py --expect-dim`）と dim=768 の
1 回実測を記録する（性能改善そのものは対象外）。

### 計測条件

- fixture: `seed_docs seed docs25k-d768.redb 25000 768` → `export` →
  `queries 768 200 queries200-d768.jsonl`（tenant-a 23,000 public・tenant-b
  2,000 private・200 クエリ・k=10。dim 128 の既存 fixture と同じ生成方式）
- self コミット: `fa79572`（`origin/main`。本 Issue の production コード変更は
  無し）
- 環境: `lscpu` Model name `QEMU Virtual CPU version 2.5+`、命令セットフラグ
  `avx2 f16c fma`、`nproc` 12、`BENCH_DEDICATED_ENV` 未設定（共有 QEMU 環境。
  `docs/design/benchmark-judgement-policy.md` §5 により**参考値**。1 回実測・
  交互 N≥5 ペアなし）
- 他 DB: pgvector `pg17`（Docker イメージ `pgvector/pgvector:pg17`）・Qdrant
  `1.19.1`（Docker イメージ `qdrant/qdrant:latest`）。索引パラメータは dim=128
  の既存計測と同一（既定値のまま。上記「公平性についての注記」参照）
- Qdrant は self／pgvector とは別セッションの実測（コミット・環境は同一。
  `docs/design/benchmark-judgement-policy.md` §5 により同様に**参考値**）。
  本計測環境に本ハーネス外の `bench-qdrant`（127.0.0.1:16333/16334）コンテナが
  常駐していたため、`containers.sh` へ追加したコンテナ名・ポート上書き
  （`CROSSDB_QDRANT_CONTAINER`／`CROSSDB_QDRANT_HTTP_PORT`／`CROSSDB_QDRANT_GRPC_PORT`。
  Issue #466）を使い、別名・別ポートの `bench-qdrant-466`（127.0.0.1:26333/26334）
  として並存させ、外部コンテナは一度も停止・削除せずに計測した

### `vector_knn`・`bulk_knn_k200`（p50/p95 µs）

| 系列 | vector_knn p50 | vector_knn p95 | bulk_knn_k200 p50 | bulk_knn_k200 p95 | recall_at_10 |
| --- | --- | --- | --- | --- | --- |
| self (wire, exact) | 2222.2 | 2569.0 | 51696.3 | 54974.6 | 1.0000 |
| pgvector (exact) | 33907.3 | 34976.3 | 35131.7 | 35629.0 | 1.0000 |
| pgvector (hnsw) | 34509.7 | 35291.6 | 35097.2 | 36314.8 | 1.0000 |
| Qdrant (exact) | 2195.2 | 2816.7 | 5355.7 | 7316.6 | n/a |
| Qdrant (hnsw) | 1045.9 | 1763.3 | 3707.6 | 4143.1 | n/a |

Qdrant の `recall_at_10` が `n/a` なのは dim=128 の既存計測（本 doc 冒頭の表）と
同じ仕様で、本ハーネスの Qdrant アダプタが recall 計算を実装していないため
（self／pgvector は fixture 内の ground truth 比較で算出）。

dim=128 の同フェーズ実測値（参考。導出方法・環境が異なるため直接比較しない）は
上記「`vector_knn` 786µs の内訳」節・`docs/design/knn-wire-stage-profile.md`
参照。

### `feature_bench` 13 フェーズ（同一バイナリ・同一環境での dim=128 vs dim=768 vs dim=1536。参考値）

| フェーズ | dim=128 p50 (µs) | dim=768 p50 (µs) | 128→768 比 | dim=1536 p50 (µs) | 768→1536 比 |
| --- | --- | --- | --- | --- | --- |
| ingest | 4089 | 14693 | 3.59x | 25450 | 1.73x |
| point_where | 2810 | 4577 | 1.63x | 7163 | 1.57x |
| where_compound | 2917 | 5460 | 1.87x | 5998 | 1.10x |
| agg_count | 2545 | 5105 | 2.01x | 5445 | 1.07x |
| agg_multi | 2745 | 5246 | 1.91x | 5574 | 1.06x |
| group_by_having | 3213 | 5568 | 1.73x | 5825 | 1.05x |
| vector_knn | 8608 | 48681 | 5.66x | 86572 | 1.78x |
| vector_knn_where | 2804 | 4938 | 1.76x | 7292 | 1.48x |
| hybrid_rrf | 11741 | 70275 | 5.99x | 119294 | 1.70x |
| mode_recall | 8713 | 50080 | 5.75x | 86716 | 1.73x |
| mode_precision | 8739 | 49311 | 5.64x | 86274 | 1.75x |
| rls_isolation | 2411 | 4915 | 2.04x | 5341 | 1.09x |
| udf_call | 678 | 2338 | 3.45x | 4460 | 1.91x |

`vector_knn`・`hybrid_rrf`・`mode_recall`／`mode_precision`（いずれも距離計算・
Top-k 選出を経由する）が dim 増加に対して最も敏感（128→768 で 5.6〜6.0 倍、
768→1536 でも 1.7〜1.8 倍と他フェーズより明確に大きい）。`ingest`・`agg_*`
（embedding を主要コストとしない経路）は 128→768 で 2〜3.6 倍、768→1536 では
1.05〜1.1 倍程度まで鈍化する。比は**参考値**（同一プロセス内の単発実測。
`docs/design/benchmark-judgement-policy.md` の交互 N≥5 ペア方式ではない）。
dim=1536 は `feature_bench` の 1 回実測のみで、`knn_profile_bench`・crossdb
横断ベンチでは未実測（下記「申し送り」参照）。

### `knn_profile_bench`（S0〜S5' 段別。dim=768・既定エンジン）

| 段 | median | ns/row |
| --- | --- | --- |
| S4_arena_build | 42.077ms | 1683.1 |
| S5_search_parallel | 1.687ms | 67.5 |
| S5_search_scalar | 2.828ms | 113.1 |
| S5prime_distance_only | 2.594ms | 103.8 |
| S0_cold_sql_e2e | 124.178ms | 4967.1 |
| S0_hot_sql_e2e | 2.278ms | 91.1 |
| S0prime_count_star | 2.966ms | 118.7 |
| S1_redb_scan | 2.014ms | 80.6 |
| S2_header_decode | 2.164ms | 86.6 |
| S3_f32_decode | 11.491ms | 459.6 |

dim=768 では `S4_arena_build`（embedding 込み全件デコード＋arena 構築）が
ns/row 1683.1 と、dim=128 実測（`docs/design/hnsw-index.md` 参照）に比べ
明確に支配的な段になる。`S1`／`S2`（走査・ヘッダデコードのみ）は dim 非依存の
ため ns/row がほぼ変わらない。consistency checks（S1..S3 行数一致・S0 結果
件数・S0' COUNT(*)・S3 vs VectorArena cross-check）はすべて green。

### 再現手順

```bash
cargo build --release -p engine --example seed_docs -p wire-server

S=<CROSSDB_DIR>
cargo run --release -p engine --example seed_docs -- seed "$S/docs25k-d768.redb" 25000 768
cargo run --release -p engine --example seed_docs -- export "$S/docs25k-d768.redb" "$S/docs25k-d768.jsonl"
cargo run --release -p engine --example seed_docs -- queries 768 200 "$S/queries200-d768.jsonl"

python scripts/crossdb_bench/run.py --db self --config exact \
  --rows-file "$S/docs25k-d768.redb" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768

scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config exact \
  --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down pgvector
scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config hnsw \
  --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down pgvector

# Qdrant は本ハーネス外の常駐コンテナ（既定名 bench-qdrant）と衝突しないよう、
# コンテナ名・ポートを上書きして計測する（Issue #466）。
export CROSSDB_QDRANT_CONTAINER=bench-qdrant-466
export CROSSDB_QDRANT_HTTP_PORT=26333 CROSSDB_QDRANT_GRPC_PORT=26334
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config exact   --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl"   --docs-file "$S/docs25k-d768.jsonl" --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down qdrant
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config hnsw   --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl"   --docs-file "$S/docs25k-d768.jsonl" --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down qdrant
unset CROSSDB_QDRANT_CONTAINER CROSSDB_QDRANT_HTTP_PORT CROSSDB_QDRANT_GRPC_PORT

BENCH_FEATURE_DIM=768 cargo run --release -p engine --example feature_bench
BENCH_KNN_PROFILE_DIM=768 make bench-knn-profile
```

per-run 生データは
`$CROSSDB_DIR/results/d768/{self_exact,pgvector_exact,pgvector_hnsw,qdrant_exact,qdrant_hnsw}.json`
（self／pgvector は `run_all.sh` 経由なら `CROSSDB_DIM=768 make bench-crossdb ...`。
Qdrant は上記の別セッション実測のため `run_all.sh` には含まれない）。

### 申し送り

- **dim=1536 は `feature_bench` のみ参考実測**: `BENCH_FEATURE_DIM`／
  `BENCH_KNN_PROFILE_DIM`／`CROSSDB_DIM` はいずれも dim=1536 を受理できるが
  （上限 4,096）、上記「`feature_bench` 13 フェーズ」節の 1 回実測のみで、
  `knn_profile_bench`・crossdb 横断ベンチ（self／pgvector／Qdrant）は
  dim=768 までで完了させた。
- `scan_stage_profile_bench`／`knn_wire_profile_bench`／`hybrid_wire_profile_bench`・
  `batch_bench.rs`・`gpu/faiss_batch_bench.py` の `DIM_GRID` は dim 展開の対象外
  （本 Issue のスコープ外）。
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業として引き続き
  未実施（`docs/design/benchmark-judgement-policy.md` §9 と同方針）。

## 2026-09-08 再計測（Issue #636。ext4 上・共有 QEMU 参考値）

`make bench-crossdb` の再計測（他 DB 側切り分け Issue の一環）で fixture の FS 条件・
Qdrant イメージ版を記録したうえで全 13 フェーズ＋広域取得 5 フェーズを再取得した。
`docs/design/benchmark-judgement-policy.md` §5 の区分に従い、以下はいずれも**共有
QEMU 環境の単発実測（参考値・採否根拠にしない）**である。

### 計測条件

- 日時: 2026-09-08T04:31〜04:33Z（`make bench-crossdb` 実行時間 75 秒）。
- self バイナリ: `wire-server`（release ビルド。sha256 先頭 `4b950db35d5c`）。
  main `773a835`（#627 マージ後）以降・`6ff22dc`（#638 マージ）**前**の状態
  （本 doc の他節にある `hybrid_rrf` の退行分析はこの時点の値であることに注意。
  `hybrid_rrf` 7,092 µs には Issue #569（ScalarIndex 構築）由来の一時的な退行
  約 12% が含まれる。是正の是非・再測定は本 Issue のスコープ外〔#633 の担当〕）。
- fixture 置き場: ext4（`/dev/sda1`。「公平性の注記」節「fixture の FS と fsync」参照）。
- コンテナ名／ポート: `bench-pgvector-0908`（25433）・`bench-qdrant-0908`
  （26333/26334）・`bench-mysql-0908`（43306）で既存の常駐コンテナと衝突しないよう
  分離。
- Qdrant: `qdrant/qdrant:latest` = `qdrant/qdrant@sha256:12364fe851b9f17356fc88189fc06d1b521262e04659ec7345975b00c9246a10`
  （イメージ作成 2026-09-03T13:10Z）・サーバ版 1.19.1（結果 meta より）。
- **逸脱**: ハーネス外の常駐コンテナ `bench-qdrant`（同 digest・アイドル・2026-09-06
  作成）が計測中も running のまま停止されなかった（「計測環境」節の「計測対象以外の
  コンテナは停止した」という前提の例外）。ポート・コンテナ名は計測用と分離されており
  計測対象そのものへの直接的な混入では無いが、ホストリソース（CPU・メモリ）の共有は
  避けられていない。
- loadavg（各結果 JSON の meta より・1 分値）: 0.70〜2.39（構成間でばらつきあり。
  複数 DB のコンテナ・venv を同一ホストで順に起動する構成のため）。
- warmup 5・50 反復（既定と同じ）。

### 結果表（p50 / p95 µs。既存表と同じ列順）

| フェーズ | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `vector_knn` | 677 / 745 | 3854 / 5934 | 3804 / 4019 | 3914 / 4325 | 596 / 774 | 750 / 995 | 8520 / 14130 | 2348 / 2982 | n/a |
| `vector_knn_where` | 1953 / 2044 | 2072 / 2146 | 2135 / 2253 | 3933 / 4242 | 657 / 749 | 675 / 974 | 7698 / 8757 | 3417 / 4269 | n/a |
| `where_compound_count` | 1020 / 1080 | 1547 / 1696 | 1563 / 1614 | 1955 / 2001 | 7833 / 8251 | 7385 / 8036 | 856 / 1177 | 896 / 1073 | 3105 / 3222 |
| `agg_count` | 82 / 87 | 1669 / 1817 | 1638 / 1714 | 1585 / 1623 | 1907 / 2476 | 1385 / 1706 | 603 / 855 | 675 / 818 | 1787 / 1820 |
| `agg_multi` | 322 / 332 | 1941 / 1996 | 2027 / 2100 | 2828 / 2934 | n/a | n/a | 7408 / 9728 | 6599 / 9102 | 2520 / 2548 |
| `group_by_having` | 1651 / 1740 | 2927 / 3050 | 2876 / 3131 | 3704 / 3754 | n/a | n/a | 8345 / 11260 | 8318 / 11597 | 16527 / 17216 |
| `hybrid_rrf` | 7092 / 8720 | 4825 / 5295 | 5011 / 5364 | 5581 / 6357 | n/a | n/a | 9508 / 12272 | 3765 / 4213 | n/a |
| `mode_recall` | 712 / 786 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `mode_precision` | 726 / 819 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `udf_call` | 723 / 1068 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `rls_isolation` | 83 / 89 | 1701 / 1839 | 1642 / 1689 | 1649 / 1697 | 1935 / 2516 | 1433 / 1820 | 608 / 726 | 780 / 878 | 1783 / 1829 |
| `explain` | n/a | 98 / 104 | 100 / 122 | 4 / 4 | n/a | n/a | 1105 / 1605 | 464 / 561 | 167 / 183 |
| `bulk_knn_k200` | 945 / 1022（200 行） | 4663 / 5061（200 行） | 4769 / 5142（200 行） | 7414 / 13628（200 行） | 3088 / 3391（200 行） | 3324 / 5522（200 行） | 9865 / 11554（200 行） | 3401 / 3900（200 行） | n/a |
| `bulk_knn_k1000` | 1698 / 2335（1000 行） | 5131 / 5517（1000 行） | 5261 / 5527（1000 行） | 31636 / 47870（1000 行） | 12748 / 16357（1000 行） | 12546 / 16680（1000 行） | 12718 / 14507（1000 行） | 5310 / 6609（1000 行） | n/a |
| `bulk_knn_where_k200` | 3287 / 3456（200 行） | 2500 / 2778（200 行） | 2477 / 2798（200 行） | 15847 / 16180（200 行） | 3068 / 3483（200 行） | 3113 / 5267（200 行） | 9078 / 10976（200 行） | 4592 / 5615（200 行） | n/a |
| `bulk_hybrid_k200` | 9770 / 10684（200 行） | 5334 / 5863（200 行） | 5459 / 5723（200 行） | 8868 / 13839（200 行） | n/a | n/a | 10784 / 12424（200 行） | 5209 / 6088（200 行） | n/a |
| `scan_where_nosort_k500` | 463 / 475（500 行） | 992 / 1036（500 行） | 924 / 987（500 行） | 247 / 258（500 行） | 5000 / 8632（500 行） | 4607 / 8187（500 行） | 2284 / 2629（500 行） | 2286 / 2652（500 行） | 904 / 1029（500 行） |

`point_where` 行は本表に含めない（`vector_knn_where` と同一値をハーネスが複製して
出力するため。上記「申し送り」節参照。全アダプタで `vector_knn_where` 行と完全一致
することを結果 JSON で確認済み）。

### スループット・品質表

| 指標 | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `ingest_bulk` | n/a | 0.47 s（53,129 rows/s） | 0.49 s（51,360 rows/s） | 0.29 s（87,640 rows/s） | 2.01 s（12,457 rows/s） | 2.07 s（12,083 rows/s） | 0.14 s（176,347 rows/s） | 0.13 s（194,940 rows/s） | 1.58 s（15,827 rows/s） |
| `ingest_single_stmt`（ext4） | 0.49 s（2,025 rows/s） | 0.60 s（1,662 rows/s） | 1.28 s（784 rows/s） | 3.43 s（292 rows/s） | 0.93 s（1,077 rows/s） | 1.68 s（594 rows/s） | 1.47 s（678 rows/s） | 1.48 s（675 rows/s） | 1.42 s（702 rows/s） |
| `recall_at_10` | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 0.8395 | n/a |
| `recall_at_10_strict` | 0.9995 | 0.9865 | 0.9865 | 0.9740 | 0.7560 | 0.7470 | 0.9990 | 0.6100 | n/a |

sqlite-vec の値はファイルベース化後（「公平性の注記」節参照）の初回計測値。
Qdrant HNSW の `index_build` は 0.51 s・status green・25,000 件索引済み
（`indexed_vectors_count` が全件に達したことを確認済み）。

### `ingest_single_stmt` の FS 切り分け（小表。性能判定ではなく計測条件差の確認）

self（`ingest_single_stmt`・rows/s）を ext4 上で ee99db3（before）・HEAD（after）の
交互 3 run、tmpfs 上で HEAD の 2 run 実測した（N<5 のため
`benchmark-judgement-policy.md` の性能判定の対象外。計測条件の差の確認が目的）。

| 条件 | run 1 | run 2 | run 3 | min | median |
| --- | --- | --- | --- | --- | --- |
| ext4・before（ee99db3） | 2,080 | 2,014 | 2,018 | 2,014 | 2,018 |
| ext4・after（HEAD） | 2,007 | 1,993 | 2,019 | 1,993 | 2,007 |
| tmpfs・after（HEAD） | 7,714 | 7,414 | — | 7,414 | — |

ext4 上の before/after は差がなく（1,993〜2,080 rows/s の狭い帯に収まる）、
tmpfs では同一バイナリ（HEAD）が 3.7〜3.8 倍のスループットを示した。この対照は N<5・
参照区間のノイズ帯未算出であり `benchmark-judgement-policy.md` §3〜4 の非退行判定基準を
満たさないため、`ingest_single_stmt` の低下を退行ではないと断定する根拠にはできない。
初回計測（`docs/design/crossdb-bench.md` 初版時点）で FS 条件が記録されておらず、
before 側の FS も未確認であることを踏まえ、本節の対照は「低下は fixture の FS 条件差に
よる可能性が高い」という仮説を支持する参考値にとどめ、退行の有無そのものの判定は
改めて 5 ペア以上・同一 FS 条件での再計測が必要と申し送る。

### 初回計測との差分所見

改善（ハーネス変更・Phase 2〜6 の複数の変更を含み、単一 Issue へは帰属しない）:
`agg_count` 3547→82 µs、`rls_isolation` 3552→83 µs、`where_compound_count`
3903→1020 µs、`agg_multi` 3738→322 µs、`group_by_having` 3948→1651 µs、
`bulk_knn_k200` 7993→945 µs（いずれも p50）。

self が依然劣後するフェーズ:

- `hybrid_rrf` 7092 µs（LanceDB HNSW 3765・pgvector 4825 µs より遅い。上記
  Issue #569 由来の一時的な退行を含む）。
- `bulk_hybrid_k200` 9770 µs（LanceDB HNSW 5209・pgvector 5334 µs より遅い）。
- `vector_knn_where` 1953 µs（Qdrant 657 µs より遅い。pgvector 2072 µs とは同等）。
- `bulk_knn_where_k200` 3287 µs（pgvector 2500 µs より遅い）。
- `scan_where_nosort_k500` 463 µs（sqlite-vec 247 µs のみ self より速い。他 DB は
  すべて self より遅い）。

### 生データ

per-run 生データ・結果表・実行ログは
[`bench-data/crossdb-20260908/`](bench-data/crossdb-20260908/) 配下に保存した:

- `results/*.json`（9 構成の結果 JSON。meta に loadavg・バージョン情報を含む）
- `table.txt`（本節の結果表の元になった生成表）・`run.log`（Qdrant digest 行を含む）・
  `run.sh`（fixture 置き場・コンテナ上書き環境変数の記録）
- `ab-ext4/{before,after}-{1,2,3}-self_exact.json`（ext4 A/B の per-run 生データ）
- `tmpfs/out-{1,2}-self_exact.json`・`run-{1,2}.log`（tmpfs 対照の per-run 生データ）
- `env.txt`（`df -T`・Docker digest・常駐コンテナ状態・self バイナリ sha256 の記録）

## self の `self_hnsw` 構成（Issue #658）

- 対応: Issue #658（親 #659 ツリー）。前提: `--search-engine`（Issue #656・PR #662）・
  `--hnsw-*` 探索パラメータ opt-in（Issue #657・PR #665）
- 目的: self（wire-server 経由）を既定エンジン（`--config exact`）だけでなく
  `--search-engine hnsw` opt-in（`--config hnsw`）でも計測できるようにし、
  (1) ANN 経路の選択を非 vacuous に確認、(2) 他 DB の HNSW 構成
  （m=16/ef_construction=100/ef_search=64）と候補幅規約（`ef.max(k)`）を揃えて
  記録、(3) 既定エンジン対照の Recall@10 と劣化有無を明示する

### 構成

`scripts/crossdb_bench/run.py --db self --config hnsw` は起動前に
`crossdb_plan_probe` example（`crates/engine/examples/crossdb_plan_probe.rs`。
`path`/`body` 列を持つ専用テーブル `plan_probe` を作業コピー redb へ投入）と
固定応答の loopback planner スタブ（`scripts/crossdb_bench/self_hnsw.py::PlannerStub`。
実 Ollama 不要）を用意し、`wire-server` を
`--search-engine hnsw --planner-endpoint <stub> --planner-model crossdb-stub`
で起動する（README「self の `--config hnsw`」節参照）。`crossdb` fixture の
`docs` テーブルには `path` 列が無く `EXPLAIN`（`dictionary_required_columns` が
`path`/`body` 必須列を要求）を直接使えないため、この専用テーブルで代替する。

自作 HNSW の既定パラメータ（`hnsw.rs::ValidatedHnswParams`）は
`m=16/ef_construction=100/ef_search=64` で、pgvector・Qdrant・LanceDB の既定と
一致する。探索側は `ef.max(k)`（`hnsw.rs::search_masked_with`）を内部で自動
適用するため、`bulk_knn_k200`/`bulk_knn_k1000`（k=200/1000）は CLI 無しで
候補幅 max(64, k) になる（`self_hnsw.py::ef_effective`）。

### 非 vacuous 確認（`ann_probe`）と限界

`EXPLAIN SELECT id FROM plan_probe USING PLAN('probe') LIMIT 10` の実測出力
（`ann_probe` フェーズ。5 ペア全 run で完全一致）:

```
engine: hnsw
hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32,sparse_visited_max=0
ann_plan: hnsw_full_visible
```

`EXPLAIN` は検索本体を実行しない静的判定（`sql/explain.rs`）であり、実行時
縮退（索引構築失敗→brute-force、`mask_splits_graph`→plain scan）はこの確認
だけでは検出できない。実行時カウンタ（`hnsw_index_cache_stats()`）による
確認は Rust API のみで wire 非露出であり、Issue #659（選択率 33% での
`ann_masked` 到達実測・既定値判断）へ申し送る。**確定結果**: `default`
は `mask_splits_graph`（連結性検査による plain scan 縮退）へ到達すること
を in-process ハーネスのカウンタで確認済み（`docs/design/
hnsw-rls-cardinality-switch.md`「Issue #659」節参照）。

**Issue #2952 以降の注記**: self の kNN は `<#>`（負の内積）で投げるが、`<#>` は
現行 HNSW の対応 metric 外であり厳密全走査になる（`ann_plan:
plain_scan_opclass_mismatch`）。上記の `ann_probe` はコサイン既定の
`USING PLAN` で到達を確認しているだけで、kNN 本体の ANN 到達は意味しない。
実際の kNN 形の `EXPLAIN` は `ann_probe.knn_operator_plan` に記録し、到達しない
ときは `hnsw_index_warm` を unsupported・`ef_search` を `null` とする。本節と
下記の過去の hnsw 計測値は `<=>` 時点のもので、書き換えていない。

### `hnsw_index_warm`（索引構築の実行証跡）

`sql::hnsw_cache` は同一世代内で索引を再利用する同期構築のため、初回クエリの
所要時間が構築コストの実行証跡になる。5 ペア全 run で 256.3〜262.2 ms
（`first_query_us`）——定常状態の `vector_knn` p50（0.4〜0.7 ms。下記表）と
比べて 3 桁近く大きく、25,000 行（うち可視 23,000 行）の索引構築が初回クエリで
実際に走ったことを示す非 vacuous な証跡になっている。

### 前後比較実測（exact vs hnsw。共有 QEMU 参考値）

`make bench-crossdb-self-hnsw-ab`（`scripts/bench_crossdb_self_hnsw_ab.sh`）で
同一バイナリ（`wire-server` sha256 先頭 `9c9ebad57b5f`・`8225baa` 時点）・同一
fixture（`docs25k.redb`・25,000 行・dim 128・ext4）を交互 5 ペア実行した
（`docs/design/bench-data/crossdb-self-hnsw-ab/20260908T153029Z-*`。計測規約
`docs/design/benchmark-judgement-policy.md` §5 の「共有 QEMU 環境は参考値」
区分に該当。専有環境での再測定は未実施。保存済み `self_hnsw.json` の
`bulk_hybrid_k200.ef_effective` は旧ハーネスの記録誤りであり、訂正内容は
`docs/design/bench-data/crossdb-self-hnsw-ab/NOTE.md` 参照）:

| フェーズ | exact min/median (µs) | hnsw min/median (µs) | ratio (hnsw/exact) |
| --- | --- | --- | --- |
| `vector_knn`（フィルタなし） | 657.4 / 701.0 | 434.2 / 436.5 | 0.66 / 0.62 |
| `vector_knn_where`（`lang='ja'` フィルタ付き） | 1891.2 / 1922.3 | 4140.3 / 4198.9 | 2.19 / 2.18 |
| `hybrid_rrf` | 6121.4 / 6448.0 | 5834.2 / 6050.5 | 0.95 / 0.94 |
| `bulk_knn_k200` | 879.9 / 903.7 | 617.6 / 626.3 | 0.70 / 0.69 |
| `bulk_knn_k1000` | 1627.0 / 1686.0 | 1366.7 / 1371.7 | 0.84 / 0.81 |
| `bulk_knn_where_k200`（フィルタ付き） | 3128.3 / 3152.7 | 5646.8 / 5720.4 | 1.81 / 1.81 |
| `bulk_hybrid_k200`（hybrid 密側が ANN 化） | 9043.6 / 9159.8 | 8691.7 / 8824.3 | 0.96 / 0.96 |

所見（既存の Issue #413・#487 の実測傾向と整合）:

- フィルタなし DISTANCE・hybrid（密側が ANN 化。Issue #410）は hnsw が高速
  （0.62〜0.96 倍）。
- SCALAR 事前フィルタ付き DISTANCE（`hnsw_subset` 経路。`vector_knn_where`・
  `bulk_knn_where_k200`）は hnsw が約 1.8〜2.2 倍遅い。`lang = 'ja'`
  フィルタは可視 23,000 行のうち約 1/3（`en`/`ja` 交互投入）を通すため、
  `full_scan_ratio`（既定 1/10）を超えて `mask_splits_graph` に伴う plain
  scan 縮退（Issue #487 と同型の理由）が有力な要因と推測されるが、
  `hnsw_index_cache_stats()` によるカウンタ確認は Issue #659 の担当のため
  本 Issue では確定していない。**確定結果**: Issue #659 が
  `mask_splits_graph` 縮退であることをカウンタで確認し、`full_scan_ratio`
  を fixture 選択率より高い値（`2/5`）へ引き上げると劣後幅が縮小する
  （3.22x→1.57x・2.59x→1.48x。参考値）ことも確認した（`docs/design/
  hnsw-rls-cardinality-switch.md`「Issue #659」節参照。HEAD には Issue
  #664 が含まれるため本節の比率〔2.19x／1.81x〕とは前提条件が異なり
  単純比較できない点に注意）。

### Recall@10（既定エンジン対照）

同じ 5 ペアで `recall_at_10`（同点許容。`ground truth` は内積 brute-force）を
記録した:

| config | recall_at_10（同点許容） | recall_at_10_strict |
| --- | --- | --- |
| exact | 1.0000（5/5 run で不変） | 0.9995 |
| hnsw | 0.8700〜0.8735（5/5 run で安定） | 0.8665〜0.8710 |

hnsw の Recall@10 は exact 比で約 13 ポイント低い（5/5 run で安定した実測値）。
本 Issue では `docs25k.jsonl`（`seed_docs.rs` がトピック別テンプレート文を
`HashingEmbedder` で埋め込んだ合成コーパス）の埋め込み分布そのものは分析して
いないが、この結果は方向として Issue #405 の既存所見（一様乱数のみの・
クラスタ構造を持たない合成コーパスは ef=64 で Recall@10 が明確に低下する。
informational 参考値。受け入れ判定はクラスタ構造ありフィクスチャの範囲に
限定される契約）と整合する。ただし `docs25k.jsonl` の分布が Issue #405 の
一様乱数フィクスチャとどの程度近いか、Recall 低下の因果自体は本 Issue
では未確定のまま申し送る。`ef_search` 引き上げでの改善余地の検証も本 Issue
の対象外。

### 申し送り（Issue #658）

- `hnsw_f16`/`hnsw_i8` トークン（`--search-engine` が受理する他の ANN 構成）は
  出力ファイル名 `self_hnsw.json` の衝突を避けるため対象外（別構成を追加する
  場合は出力名の設計が必要）。
- 実行時カウンタ（`hnsw_index_cache_stats()`）による縮退有無の確定・選択率
  33% での `ann_masked` 到達実測・既定値判断は Issue #659 の担当。
  実測・既定値判断（据え置き）は `docs/design/hnsw-rls-cardinality-switch.md`
  「Issue #659」節参照。
- wire-server 側の観測性追加（stderr へのエンジン表示等）は production 変更の
  ため本 Issue では行わない。

## 申し送り

- SQL 表層のフィルタ付き経路・集計の wire 越し 2.7〜6 ms は、in-process との差分を
  wire 側（応答組み立て・テキスト化）と SQL 側で切り分ける profiling が未実施
  （`vector_knn`〔フィルタなし〕は上記「`vector_knn` 786µs の内訳」節で実施済み）。
- スカラー列投影時の全行デコード（`sql/exec.rs::on_visible_row` の `scan_scalar_columns`）
  を Top-k 確定後の k 行へ遅延させる改善は Issue #453（上記「self の投影コスト切り分け」節）。
  ORDER BY なしのフィルタのみ行取得（広域取得モード）の SQL 表層追加は Issue #454 で
  実装済み（`docs/design/wide-retrieval-scan.md`）。self の実測値は 2026-09-08
  再計測で取得済み（下記「2026-09-08 再計測」節参照）。
- `point_where` フェーズはハーネスが `vector_knn_where` の統計をそのまま複製して
  出力するため（`scripts/crossdb_bench/self_db.py`・`pgvector_db.py`・
  `sqlite_vec_db.py`・`qdrant_db.py`・`lancedb_db.py`。MySQL は unsupported）、
  結果表に独立フェーズとして掲載する意味が無く重複行になっている。ハーネス側の整理
  （独立クエリ化または出力からの除外）は本 doc・Issue #636 のスコープ外として
  申し送る。
- engine GPU 経路の大バッチ頭打ち（Top-k の GPU 化・転送量削減）は別タスク。
- 本計測は共有 VM（loadavg 約 2）での単発実測であり、専有環境での再測定は未実施。
- Phase 4 通しのチップ別前後比較（Issue #530）では、`vector_knn` self A/C
  再計測は本 doc の既存記録（`vector_knn` 786µs・Qdrant HNSW 目標 559µs）を
  引用するに留め、新規実測は時間予算の都合で見送った。`vector_knn` は
  Phase 2/3/5/6 の変更（遅延デコード・二次索引・CSR・GPU 等）を多く含み
  Phase 4（チップ最適カーネル）単独には帰属できないため、区間帰属の観点でも
  再計測の優先度は `dot_kernel`／`knn_profile` ワークロード（`chip_bench`
  経由）より低いと判断した。詳細は
  [`phase4-chip-before-after.md`](phase4-chip-before-after.md) §6 参照。
  再計測はオーナー実機での手順（同 doc §9）の一部として実施可能。
- `hybrid_rrf`（self）の状態依存退行（`WHERE` 実行後にのみ発現する p50 約 10%
  劣化。`ScalarIndex::build`〔Issue #473〕の長文 `body` 列索引化が原因）と
  その対策（列単位の平均値長ゲート・Issue #632）の crossdb 前後比較実測は
  [`scalar-index-generation-cache.md`「前後比較実測（Issue #633）」節](scalar-index-generation-cache.md)
  参照。
- Issue #661 で記録した比較条件の非対称のうち、以下はハーネス側の変更を伴うため
  本 doc・本 Issue のスコープ外としてオーナー承認後に別 Issue へ切り出す:
  - `pgvector_db.py` への `ANALYZE`（`COPY` 完了・索引構築後）追加と、
    `explain` に限られていた `EXPLAIN` 記録の `vector_knn_where`／
    `bulk_knn_where_k200` 相当への拡張（HNSW 索引使用の直接確認のため）。
  - LanceDB hybrid への `.rerank(RRFReranker(K=60))` 明示指定、および
    pgvector・sqlite-vec の `hybrid_rrf` 候補プール（LIMIT 50）を
    `bulk_hybrid_k200` と同じ 200 へ統一するかどうかの是非。
  - `hnsw-index.md`「Issue #413」節の `point_where` 記述（`lang×topic` 複合条件と
    注記）と `feature_bench.rs` の実クエリ（`lang` 単独条件）の食い違いに
    見える所見の確認。

## Issue #655: 候補 id マスク経路（#654）の前後比較

`vector_knn_where`／`bulk_knn_where_k200`（本 doc が self 最劣後として記録する
フェーズ）の #654 適用前後比較（段別プロファイル・crossdb self 双方）は
[`scalar-index-mask-search.md`「前後比較実測（Issue #655）」節](scalar-index-mask-search.md)
を参照（本 doc の表は再転記しない）。

## 2026-09-18 再計測（Apple M4 Max・NoSQL 対照追加・是正前後の勝敗）

- 環境: MacBook Pro（Apple M4 Max 16 コア・64 GB・macOS 26.6）。共有デスクトップ機（loadavg 2〜6）の参考値。self・sqlite-vec・LanceDB は native、pgvector・Qdrant・MySQL・NoSQL 4 系統は Docker Desktop VM（16 CPU／15.6 GiB・arm64）越し
- 対象: 既存 5 DB に NoSQL 対照 4 系統（Redis 8.10.1／RediSearch・Elasticsearch 9.1.4・MongoDB Atlas local 8.3.11〔`$vectorSearch`／`$rankFusion`〕・MongoDB Community 8.3.11〔ベクトル検索なし〕）と self の HTTP NoSQL 表層（`self_nosql`）を追加。ハーネスは `scripts/crossdb_bench/`（Issue #846）
- HEAD `64cb381`・fixture 25,000 行（可視 23,000）・dim 128・warmup 5／iters 50。生データ: `docs/design/bench-data/crossdb-20260918/`
- 前回（2026-09-08・QEMU x86_64 専有環境）との p50 直接比較は環境差のため行わない。`1deae29..64cb381` の engine に検索経路の性能変更は無い
- Redis `FT.HYBRID` の RRF WINDOW は 50。Elasticsearch の RRF retriever（`retriever.rrf`）は無償ライセンスで 403 のため `hybrid_rrf`／`bulk_hybrid_k200` は unsupported（未計測）として記録。Elasticsearch が最速他 DB となる `agg_multi`・`group_by_having` の 2 フェーズは採用 run が 1 回のみ（exact／hnsw 列で約 30% 乖離）の参考値

### 是正前後の勝敗（p50 µs・対照 DB 越しの順位）

「改善前」は 2026-09-18 単発 run、「改善後」は同一 fixture・同一機で交互 5 ペア計測した after 側（Issue #843・#844 の是正）5 run median を同じ対照値へ当てた再判定（参考値。対照 DB の再取得は Issue #848）。「比」は最速他 DB ÷ self（1 未満が負け）。

| フェーズ | 最速他 DB | self 改善前 | 比 | 勝敗 | self 改善後 | 比 | 勝敗 |
| --- | --- | ---: | ---: | --- | ---: | ---: | --- |
| `vector_knn` | Qdrant 861 | 547 | 1.57 | 1 位 | 594 | 1.45 | 1 位 |
| `vector_knn_where` | Qdrant 669 | 1,007 | 0.66 | 3 位 | 251 | 2.67 | 1 位 |
| `where_compound_count` | LanceDB HNSW 641 | 903 | 0.71 | 4 位 | 93 | 6.88 | 1 位 |
| `agg_count` | LanceDB HNSW 374 | 127 | 2.95 | 1 位 | 106 | 3.51 | 1 位 |
| `agg_multi` | Elasticsearch HNSW 867 | 247 | 3.51 | 1 位 | 245 | 3.54 | 1 位 |
| `group_by_having` | Elasticsearch HNSW 952 | 1,445 | 0.66 | 3 位 | 75 | 12.72 | 1 位 |
| `hybrid_rrf` | Redis FLAT 1,336 | 4,800 | 0.28 | 9 位 | 1,492 | 0.90 | 2 位 |
| `bulk_knn_k200` | LanceDB HNSW 2,579 | 722 | 3.57 | 1 位 | 733 | 3.52 | 1 位 |
| `bulk_knn_k1000` | LanceDB HNSW 3,624 | 1,332 | 2.72 | 1 位 | 1,327 | 2.73 | 1 位 |
| `bulk_knn_where_k200` | pgvector 2,477 | 1,652 | 1.50 | 1 位 | 394 | 6.29 | 1 位 |
| `bulk_hybrid_k200` | Redis FLAT 2,256 | 6,268 | 0.36 | 8 位 | 1,538 | 1.47 | 1 位 |
| `scan_where_nosort_k500` | sqlite-vec 203（in-process） | 427 | 0.48 | 2 位 | 423 | 0.48 | 2 位 |
| `rls_isolation` | LanceDB HNSW 377 | 112 | 3.37 | 1 位 | 107 | 3.51 | 1 位 |

合計: 改善前 7 勝 0 僅差 6 敗 → 改善後 11 勝 0 僅差 2 敗（self 固有の `mode_recall`・`mode_precision`・`udf_call` は対象外）。残る 2 敗は `hybrid_rrf`（self はプール深さ 200・Redis は WINDOW 50 で 0.90 倍）と `scan_where_nosort_k500`（engine 内部 227µs は sqlite-vec 203µs と同水準。残差は loopback＋psycopg）。

### 単文 INSERT の durability 既定値（比較条件の非対称）

`ingest_single_stmt` は self 181 rows/s 対 Redis 3,566 rows/s。self は redb commit ごとの `sync_data` が macOS では `F_FULLFSYNC`（p50 約 4.2 ms。同一ボリュームの `fsync(2)` は約 18 µs）となる一方、対照 DB の既定は次のとおり（MongoDB Atlas local・PostgreSQL を除き）応答時点で永続化を待たない。engine 側の既定は変えず、opt-in は Issue #849〜#851 の担当。

| DB | 単一書き込みの既定 durability |
| --- | --- |
| self（redb） | 毎 commit `Durability::Immediate`（macOS では `F_FULLFSYNC`） |
| Redis（RediSearch） | AOF 既定無効・周期的 RDB のみ。fsync なし |
| LanceDB | fsync なし（OS ページキャッシュ止まり） |
| MongoDB Atlas local（`mongodb_db.py`・単一ノード replica set） | 既定 write concern `w:majority`（`writeConcernMajorityJournalDefault=true`）。応答前に journal の永続化を待つ |
| MongoDB Community（`mongodb_plain_db.py`・standalone） | 既定 write concern `w:1`（`j` 未指定）。journal は 100 ms 間隔で flush し応答時点では永続化を待たない。データページは checkpoint |
| SQLite（sqlite-vec） | `synchronous=FULL`（`fsync(2)`）。`PRAGMA fullfsync` は既定 OFF で `sqlite_vec_db.py` も有効化しないため macOS でも `F_FULLFSYNC` は使わない |
| PostgreSQL（pgvector） | `synchronous_commit=on`（WAL fsync） |

### Issue #851: durability 別実測と比較条件

計測環境: MacBook Pro（Apple M4 Max・共有 GUI 常駐機。`docs/design/
benchmark-judgement-policy.md` §5 の「共有環境」区分。他プロセス実行数
952〜956・専有実測ではない）・macOS 26.6.2・APFS（`Macintosh HD - Data`）。
`wire-server` バイナリ sha256 は既存の crossdb 計測と同型に `meta.version`
で確認する契約だが、本節は engine 側ベンチのみ実施したため wire-server
バイナリは未使用。commit `3d36cf7`（origin/main。#849〔`WriteDurability`
opt-in〕・#850〔`--durability` CLI〕マージ後）。生データ・summary.tsv は
`docs/design/bench-data/ingest-durability-ab/20260918T205301Z/`。

**実施範囲**: `crates/engine/benches/ingest_profile_bench.rs`
（`BENCH_INGEST_PROFILE_MODE=single`。E0/S0/I1〜I8 の内訳を持つ既存
ベンチ。Issue #484）へ `BENCH_INGEST_PROFILE_DURABILITY=immediate|none`
opt-in（Issue #851）を追加し、`scripts/bench_ingest_durability_ab.sh`
から既定 `immediate` と opt-in `none` を同一バイナリ・同一 fixture
（単文数 2,000・dim 128。crossdb の 25,000 より小さい値を N=5 ペア交互で
現実的な時間に収めるため採用）で交互 N=5 ペア実測した。**crossdb ベンチ
本体（`ingest_single_stmt`・wire 経由）の durability A/B は未実施**（本
Issue の作業時間内では fixture〔`docs25k.redb` 等〕・専用 venv の準備が
間に合わなかったため。`scripts/crossdb_bench/self_durability.py`・
`self_db.py::run` への `CROSSDB_SELF_DURABILITY` opt-in 結線は実装済みで、
次回の crossdb 計測時にそのまま使える）。Linux ext4 環境の実測も同様に
未実施。

**A/B 結果（min-of-N・median。N=5）**:

| 指標 | immediate | none | ratio（none/immediate） |
| --- | ---: | ---: | ---: |
| I8（commit）ns/row・median | 4,577,625 ns（≈4.58 ms） | 8,583 ns（≈8.6 µs） | 0.0019（約 533 分の1） |
| E0（typed_row_api）median | 4.565 ms | 0.019 ms | 0.0042 |
| S0（sql_surface）median | 4.629 ms | 0.026 ms | 0.0056 |

**fsync 系原始操作プローブ（`scripts/fsync_probe.py --iters 100`。同一
ボリューム）**:

| 原始操作 | min | p50 | p95 |
| --- | ---: | ---: | ---: |
| `fsync(2)` | 21.5 µs | 23.9 µs | 29.7 µs |
| `F_BARRIERFSYNC` | 188.4 µs | 385.5 µs | 730.4 µs |
| `F_FULLFSYNC` | 3.31 ms | 4.35 ms | 5.32 ms |

**帰属の考察**: `Immediate − None` の per-commit 差（約 4.57 ms）は
`F_FULLFSYNC` の p50（約 4.35 ms）とほぼ同水準であり、Issue 本文の参考値
（4.2 ms／18 µs／350 µs）とも整合する。ただし redb 4.2.0 の
`Durability::None` は `write()` も `sync_data()` も発行せずプロセス内
バッファに留まる契約（`storage.rs::WriteDurability::None` doc。#849）の
ため、この差は「fsync だけ」ではなく commit の I/O 全体（`write()` 発行分を
含む）である点に注意。`fsync(2)` 単体（約 24 µs）との比較では、macOS が
`F_FULLFSYNC` を選ぶことによる追加コストが支配的であることも確認できる。

**整合性検証**: `none` arm でも `ingest_profile_bench.rs` の既存整合性検証
（E0↔レプリカのバイト一致・`table_generation`・台帳 content_hash 照合）は
全 run で通過した。redb `Database::drop`（クリーンクローズ時に pending の
非 durable commit を通常の durability commit へ昇格させる契約）により、
プロセス正常終了後の再オープンでも行が失われないことを実測で確認した。

参照区間（`docs/design/benchmark-judgement-policy.md` §4）は本節では未採取
（`immediate`/`none` の差が 2〜3 桁と極めて大きく、ノイズ帯を跨いだ判定
不能のリスクが無いため）。共有環境の参考値であり、専有環境での再実測・
crossdb 本体経由の実測はオーナー作業として申し送る。

### 原因分析と是正

フェーズ別の原因（self 側機構と最速他 DB の手法）・改善後 5 run の A/B 生データ（`docs/design/bench-data/crossdb-20260918-loss-ab/`）・チップ最適化の考察は `docs/design/crossdb-loss-analysis-20260918.md` を参照（Issue #845・PR #853 で追加。本節の「改善後」列はその生データの after 側 5 run median）。

## 再計測（Issue #848。是正後・Elasticsearch を含む交互 N=5 ラウンド）

2026-09-18 の「是正前後の勝敗」節は、対照 DB 側が単発 run（一部 ES は 1 回のみ）の
まま「改善後」列だけを交互 5 ペアで取り直した参考値だった。是正（#843・#844・
PR #853）が main に取り込まれた後、`docs/design/benchmark-judgement-policy.md`
の規約（交互 N≥5・per-run 生データ必須・min-of-N＋median・run-to-run 幅による
ノイズ帯判定）に従い、**self・ES を含む対照 DB 全系統を同一セッションで N=5
ラウンド再計測**した。

### 環境・実行条件

- HEAD `0ab63b3`（#853 の直後）・wire-server sha256 `627b5c53c6a9…`（`docs/design/
  crossdb-loss-analysis-20260918.md` の after ビルドと同一 sha256）
- MacBook Pro（Apple M4 Max・16 コア・64 GB・macOS 26.6）。fixture は APFS
  （`/tmp` 配下）
- **共有機の参考値**: 本セッションと並行して複数の Issue エージェントが同一機で
  実行されており（ハーネス指示に明記の前提どおり）、`BENCH_DEDICATED_ENV` は
  設定していない。ラウンド間の loadavg 実測: round1 開始 4.93→終了 15.75（他
  ジョブの起動と重なった）、round2 終了 8.12、round3 終了 5.04、round4 終了
  5.98、round5 終了 5.74（1 分間平均）。round2 は全フェーズで一様に p50 が
  他ラウンドより高く（例: self `hybrid_rrf` 2023µs 対 他 4 ラウンド
  1468〜1681µs）、機械全体の輻輳が原因と判断できる——特定フェーズだけが
  悪化していないため、後述の min-of-N 判定はこの影響を受けにくい
- **ドライバ**: `scripts/bench_crossdb_ab.sh`（新設。`run_all.sh` フル一括を
  1 ラウンドとみなし `CROSSDB_RUN_TAG=<ts>-round<N>`（`<ts>` はセッション
  起動時刻）で輪番実行する。cand が DB の数だけある構造のため、
  baseline→cand→baseline→cand... の厳密な 2 arm 輪番からの意図的な
  逸脱——詳細は同スクリプトのコメント参照）
- 全 5 ラウンド × 18 arm（self exact/hnsw/nosql・対照 8 系統 exact/hnsw 混在）
  ＝ 90 run すべて成功（`FAILED` 0 件）。生データ一式（絶対パス）は
  `docs/design/bench-data/crossdb-20260918T142251Z-ab/`（`env.txt`・
  `results/round{1..5}/*.json`・`logs/round{1..5}/*.log`・
  `summarize-output.md`・`redis_window_diag/`）にコミット済み

### スコアボード（min-of-N・自身の実測ノイズ帯を踏まえた勝敗）

「最速他 DB」は 8 対照 DB 系統（exact/hnsw 混在）の中で min-of-N が最小のもの。
「判定」は `scripts/bench_crossdb_ab_summarize.py` の規則——比の乖離が
固定 ±5% 帯**かつ** self・対照双方の run-to-run 幅（5 run の `(max-min)/min`）
の広い方を超える場合のみ確定（win/loss）、それ以外は「僅差」（ノイズ帯内）。

| フェーズ | 最速他 DB | 他 DB min-of-N | self min-of-N | 比（他/self） | 判定 |
| --- | --- | ---: | ---: | ---: | --- |
| `vector_knn` | Qdrant exact | 707.6 | 573.3 | 1.23 | self win |
| `vector_knn_where` | Qdrant exact | 650.2 | 246.2 | 2.64 | self win |
| `where_compound_count` | Elasticsearch hnsw | 530.0 | 87.4 | 6.06 | self win |
| `agg_count` | LanceDB exact | 365.2 | 104.5 | 3.49 | self win |
| `agg_multi` | Elasticsearch hnsw | 637.5 | 249.0 | 2.56 | self win |
| `group_by_having` | Elasticsearch hnsw | 650.8 | 70.6 | 9.22 | self win |
| `hybrid_rrf` | Redis FLAT exact | 1,044.9 | 1,467.9 | 0.71 | **僅差**（run-to-run 幅 self 37.8%・Redis 37.7% が乖離幅 29%を上回るためノイズ帯内） |
| `bulk_knn_k200` | LanceDB hnsw | 2,490.4 | 714.7 | 3.48 | self win |
| `bulk_knn_k1000` | LanceDB hnsw | 3,562.7 | 1,353.9 | 2.63 | self win |
| `bulk_knn_where_k200` | pgvector hnsw | 2,440.0 | 391.7 | 6.23 | self win |
| `bulk_hybrid_k200` | Redis FLAT exact | 1,950.5 | 1,628.3 | 1.20 | 僅差（乖離幅 20% が run-to-run 幅を超えず） |
| `scan_where_nosort_k500` | sqlite-vec（in-process） | 197.0 | 438.3 | 0.45 | **self loss**（測定条件の非対称。§「単文 INSERT の durability 既定値」節・下記参照） |
| `rls_isolation` | LanceDB exact | 370.8 | 110.3 | 3.36 | self win |

単位は p50 µs。全フェーズの生データ（5 run の値・run-to-run 幅・self との
比較表）は `scripts/bench_crossdb_ab_summarize.py` の出力
（`docs/design/bench-data/crossdb-20260918T142251Z-ab/summarize-output.md`
参照。本ファイルは計測当時の集計スクリプト版で生成したもので、
`dir=/tmp/crossdb848` という旧セッションのパス表記のまま残置しており、
後日の堅牢化修正（欠損・非観測の扱い変更）で追加された「非観測」行群は
含まない。ただし「self との比較」win/loss 表——本節が引用する数値・判定——は
旧版・新版の集計スクリプトを同一データへ再実行して出力が一致することを
確認済みで、本節の結論には影響しない）。改善前（7 敗）・改善後単発対照
（2 敗）に対し、**N=5 再計測では
確定的な負けは `scan_where_nosort_k500`（測定条件の非対称。既知・対処
しない）の 1 件のみ**で、`hybrid_rrf`／`bulk_hybrid_k200` は Redis との差が
run-to-run ノイズ帯に収まり確定的な勝敗を付けられない「僅差」へ後退した
（is-loss ではなく not-confirmed-win という区分。数値上は self が遅いが、
本環境のノイズが大きすぎて Redis との実力差と言い切れない）。

**Elasticsearch の run-to-run 変動**: N=5 でも `agg_multi`（run-to-run 幅
171.1%）・`group_by_having`（同 120.1%）と非常に大きく、2026-09-18 時点の
「exact/hnsw 列で ±30% 乖離」という所見は氷山の一角だったと判明した。
ただし self の優位幅（2.6〜9.2 倍）がこのノイズ幅を大きく上回るため、
勝敗判定そのものは揺らがない。

### `hybrid_rrf` が残る「僅差」の理由確認（受け入れ条件 3）

2026-09-18 節の説明「self はプール深さ 200 で候補を取り Redis は WINDOW 50」
（`hybrid.rs::RrfConfig::default` の pool_depth=200 対 `redis_db.py` の
既定 `WINDOW=50`）を実測で確認した。

- pgvector 側の `env.txt` 注記「WINDOW=50（self・pgvector の各プール 50 と
  同条件）」は、pgvector 自身の hybrid 候補プール（`pgvector_db.py::hybrid`
  の `LIMIT 50`。`bulk_hybrid_k200` のみ `LIMIT 200` に拡張）を指しており、
  **self の pool_depth とは無関係**（self は `hybrid_rrf`・`bulk_hybrid_k200`
  いずれも既定 pool_depth=200 のまま不変）。旧記述の「self・pgvector」は
  誤読を招く書き方だったため本節で訂正する
- 診断用 opt-in `CROSSDB_REDIS_HYBRID_WINDOW`（`redis_db.py` へ本 Issue で
  追加。既定 50・本番スコアボードの計測条件は変えない）で Redis 側の
  `WINDOW` を self の pool_depth と同じ 200 に揃え、N=5 で追加計測した:

  | WINDOW | min-of-5 (µs) | median (µs) |
  | --- | ---: | ---: |
  | 50（既定・本番スコアボードと同一条件） | 1,044.9 | 1,272.4 |
  | 200（self の pool_depth と同条件） | 1,044.1 | 1,058.3 |

  **WINDOW を 50→200 へ 4 倍に拡張しても Redis 側の `hybrid_rrf` レイテンシは
  ほぼ不変**（min-of-N の差は 0.1%）。「self がプール深さ 4 倍ぶん余分に
  候補処理をしているから遅い」という従来の説明は、この診断実測では
  **支持されない**——RediSearch の `WINDOW`（各サブクエリの取得件数上限）を
  広げても実測レイテンシが増えないことから、self・Redis の差はプール深さ
  ではなく他の要因（SQL 表層固定コスト・BM25 posting 走査・RRF 融合の実装
  差。`docs/design/crossdb-loss-analysis-20260918.md` §3.1 が既に挙げている
  「Redis は転置索引が書き込み時に確定済みでクエリ時の再構築という概念が
  無い」という構造差が引き続き主因候補）にあると考えられる。ただし
  そもそも上記スコアボードの判定は「僅差」（確定的な負けではない）である
  ため、この理由確認は原因追究というより従来説明の反証記録として申し送る

### 是正前後の比較（旧節との対比）

| フェーズ | 改善前（2026-09-18・単発対照） | 改善後（2026-09-18・単発対照） | 再計測（Issue #848・N=5 対照） |
| --- | --- | --- | --- |
| `hybrid_rrf` | 9 位（0.28 倍） | 2 位（0.90 倍・self loss 扱い） | 僅差（0.71 倍・run-to-run ノイズ帯内で判定不能） |
| `bulk_hybrid_k200` | 8 位（0.36 倍） | 1 位（1.47 倍） | 僅差（1.20 倍・ノイズ帯内） |
| `scan_where_nosort_k500` | 2 位（0.48 倍） | 2 位（0.48 倍） | self loss（0.45 倍。対照 DB を sqlite-vec 固定で再計測しても変わらず） |
| 他 11 フェーズ | 1 位（是正後） | 1 位 | 1 位（N=5 でも維持） |

対照 DB を単発 run から N=5 へ拡充したことで、`hybrid_rrf`／`bulk_hybrid_k200`
の「self loss」「1 位だが僅差」という評価は、より保守的な「判定不能（ノイズ
帯内）」へ変わった。これは self の実装が改善前後で変化した結果ではなく
（本 Issue は production コード無変更）、対照 DB 側の測定条件を初めて
self と同水準（交互 N=5）に揃えたことによる評価の精緻化である。

### 単文 INSERT・`scan_where_nosort_k500` の非対処事項（不変）

`ingest_single_stmt`（self 145〜213 rows/s。`docs/design/crossdb-loss-
analysis-20260918.md` §3.6 の durability 契約差）・`scan_where_nosort_k500`
（同 §3.5 の wire＋psycopg 測定条件差）はいずれも本 Issue のスコープ外の
まま変化なし。N=5 実測でも `scan_where_nosort_k500` の self min-of-N
438.3µs は sqlite-vec（in-process・197.0µs）の約 2.2 倍にとどまり、
2026-09-18 時点の分析（engine 内部コストは sqlite-vec と同水準、差は
wire 経由の測定条件）と整合する。
