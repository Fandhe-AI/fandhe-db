# crossdb_bench

自作ベクトル DB（本リポジトリの `wire-server`＋`engine`。以下 "self"）と他 DB
（pgvector・sqlite-vec・Qdrant・LanceDB・MySQL・MongoDB・Redis・Elasticsearch）の
機能別ベンチマークを行う Python ハーネス。

private spec（`docs/spec`）の内容はここでは参照・転記しない。数値基準（warmup・
反復回数）は本ハーネス独自の実装既定値（`crates/engine/examples/feature_bench.rs`
の WARMUP=5・ITERS=50 に合わせてある）。Cargo 依存は増やしていない
（`cargo build --release -p fandhe-db-wire-server` の既存バイナリをそのまま子プロセス
起動するだけ）。

## 前提

- Docker（`pgvector/pgvector:pg17`・`qdrant/qdrant:latest`・`mysql:9`・
  `mongodb/mongodb-atlas-local:latest`・`mongo:8`・`redis:8`・
  `docker.elastic.co/elasticsearch/elasticsearch:9.1.4` を取得済みであること。
  `docker inspect` で digest／実バージョンを確認してから計測結果の meta に記録する）
  2026-09-18 計測（`docs/design/bench-data/crossdb-20260918/`）で解決されたバージョン:
  `redis:8` = Redis 8.10.1（RediSearch 8.10.0）・
  `docker.elastic.co/elasticsearch/elasticsearch:9.1.4` = 9.1.4・
  `mongodb/mongodb-atlas-local:latest` = mongod 8.3.11・`mongo:8` = 8.3.11（mongot 非搭載）。
  `latest`／メジャータグは計測ごとに `docker image ls --digests` で digest を記録する
- Python venv（`requirements.txt` を `pip install -r` 済み。`pymongo==4.18.1`・
  `redis==8.1.0` が必要。Elasticsearch は標準ライブラリのみ）
- `target/release/wire-server`（`cargo build --release -p fandhe-db-wire-server` でビルド済み）
- `crates/engine/examples/seed_docs.rs`（別 agent 生成）等で作成した
  `docs25k.redb`・`docs25k.jsonl`・`queries200.jsonl`

## セットアップ

```bash
python -m venv /path/to/venv
source /path/to/venv/bin/activate
pip install -r scripts/crossdb_bench/requirements.txt

cargo build --release -p fandhe-db-wire-server
```

## コンテナの起動・停止

```bash
scripts/crossdb_bench/containers.sh up pgvector
scripts/crossdb_bench/containers.sh up qdrant
scripts/crossdb_bench/containers.sh up mysql
scripts/crossdb_bench/containers.sh up mongodb
scripts/crossdb_bench/containers.sh up mongodb_plain
scripts/crossdb_bench/containers.sh up redis
scripts/crossdb_bench/containers.sh up elasticsearch
scripts/crossdb_bench/containers.sh down pgvector
scripts/crossdb_bench/containers.sh down qdrant
scripts/crossdb_bench/containers.sh down mysql
scripts/crossdb_bench/containers.sh down mongodb
scripts/crossdb_bench/containers.sh down mongodb_plain
scripts/crossdb_bench/containers.sh down redis
scripts/crossdb_bench/containers.sh down elasticsearch
```

`up mysql` は `mysql:9` イメージからコンテナ名 `bench-mysql`
（127.0.0.1:33306 既定、`MYSQL_ROOT_PASSWORD=bench`・`MYSQL_DATABASE=bench`）を
`docker rm -f` で毎回冪等に作り直し、`mysqladmin ping` で起動を待つ
（pgvector・Qdrant と同じ up/down 管理。`down mysql` も `docker rm -f` で
コンテナごと破棄する）。MongoDB・Redis・Elasticsearch も同様に `docker rm -f` で
毎回冪等に管理される。

sqlite-vec・LanceDB は in-process（コンテナ不要）。sqlite-vec は他 DB（永続ストレージ）
と投入速度の比較条件を揃えるため `--workdir` 配下のファイルベース DB を使う
（`:memory:` ではない。非永続だった旧実装の数値との比較は不可）。self は `run.py` が
`wire-server` を子プロセスとして起動・停止する。

**計測中は対象 DB 以外のコンテナを止めること**（公平な比較のため。CPU・メモリの
競合を避ける）。

### 環境変数（接続先ポート）

各 DB のポートは以下の環境変数で上書きできる。起動側（`containers.sh`／
`gpu/containers_gpu.sh`）と接続側（各 `*_db.py`・`gpu/qdrant_gpu_build_bench.py`。
`common.env_port`）が同じ変数名・同じ既定値を読む。Python 側は 1〜65535 の整数
以外（空文字は未設定扱い）を `ValueError` で拒否する（fail-closed）。

| 環境変数 | 既定値 | 対象 |
| -------- | ------ | ---- |
| `CROSSDB_SELF_PORT` | 15432 | self（`run.py` が起動する `wire-server` の bind ポート） |
| `CROSSDB_PG_PORT` | 15433 | pgvector（`containers.sh up pgvector`・`pgvector_db.py`） |
| `CROSSDB_QDRANT_HTTP_PORT` | 16333（gpu/ は 17333） | Qdrant HTTP（`containers.sh up qdrant`・`qdrant_db.py`。`gpu/containers_gpu.sh`・`gpu/qdrant_gpu_build_bench.py` も同じ変数名を読むが既定値は 17333） |
| `CROSSDB_QDRANT_GRPC_PORT` | 16334（gpu/ は 17334） | Qdrant gRPC（同上。gpu/ の既定値は 17334） |
| `CROSSDB_MYSQL_PORT` | 33306 | MySQL（`containers.sh up mysql`・`mysql_db.py`） |
| `CROSSDB_MONGODB_PORT` | 37017 | MongoDB Atlas local（`containers.sh up mongodb`・`mongodb_db.py`） |
| `CROSSDB_MONGODB_PLAIN_PORT` | 37018 | MongoDB Community（`containers.sh up mongodb_plain`・`mongodb_plain_db.py`） |
| `CROSSDB_REDIS_PORT` | 36379 | Redis（`containers.sh up redis`・`redis_db.py`） |
| `CROSSDB_ES_PORT` | 39200 | Elasticsearch（`containers.sh up elasticsearch`・`elasticsearch_db.py`） |

```bash
CROSSDB_PG_PORT=25433 scripts/crossdb_bench/containers.sh up pgvector
CROSSDB_PG_PORT=25433 python scripts/crossdb_bench/run.py --db pgvector --config exact ...
```

### 環境変数（self が起動するバイナリの上書き）

`--db self` は既定で `target/release/wire-server`（リポジトリルート基準）を
子プロセスとして起動する。環境変数 `CROSSDB_SELF_BINARY` を設定すると、
別の場所にビルドしたバイナリを起動できる（別コミット・別 `CARGO_TARGET_DIR`
の before/after バイナリを交互起動する前後比較計測向け。Issue #479）。
未設定時は既定パスのまま（後方互換）。設定した場合に限り、指定パスが
存在しなければ起動前に `FileNotFoundError` で拒否する（fail-closed）。

```bash
CROSSDB_SELF_BINARY=/tmp/wt-before/target-before/release/wire-server \
  python scripts/crossdb_bench/run.py --db self --config exact ...
```

### self の `--config hnsw`（ANN opt-in。Issue #656〜#658）

`--search-engine`（Issue #656）・`--hnsw-*` 探索パラメータ opt-in（Issue #657）が
wire-server の CLI へ入ったことで、self（wire-server 経由）でも `--config hnsw`
（`--search-engine hnsw`）を計測できる。**事前に `crossdb_plan_probe` example の
ビルドが必要**（`cargo build --release -p fandhe-db-engine --example crossdb_plan_probe`）。
`--search-engine` を持たない旧バイナリを `CROSSDB_SELF_BINARY` で指定すると
未知引数として起動失敗するが、これは意図した fail-closed の挙動である
（exact 構成の結果を ANN 経路と誤認させないため、緩めない）。

```bash
cargo build --release -p fandhe-db-wire-server
cargo build --release -p fandhe-db-engine --example crossdb_plan_probe

python scripts/crossdb_bench/run.py --db self --config hnsw \
  --rows-file "$S/docs25k.redb" --queries-file "$S/queries200.jsonl"
```

`--config hnsw` は起動前に次の追加処理を行う（`self_hnsw.py`。`--config exact`
の起動引数・フェーズ・結果キーはビット同一のまま不変）:

1. `crossdb_plan_probe <作業コピー redb> <dim>` を実行し、`EXPLAIN` の前段
   `dictionary_required_columns` が要求する非 nullable TEXT の `path`/`body`
   列を持つ専用テーブル `plan_probe` を投入する（`docs` テーブルには `path`
   列が無いため使えない）。
2. loopback（`127.0.0.1`）のエフェメラルポートで固定応答の planner スタブ
   （`self_hnsw.PlannerStub`。実 Ollama 不要）を起動し、
   `--planner-endpoint`/`--planner-model` として wire-server へ渡す。
3. `EXPLAIN SELECT id FROM plan_probe USING PLAN('probe') LIMIT 10` を 1 回
   実行し、`engine: hnsw` を確認する（結果は `ann_probe` フェーズへ記録。
   不一致・欠落は例外で計測全体を失敗させる fail-closed）。

`EXPLAIN` は検索本体を実行しない静的判定（`sql/explain.rs`）のため、実行時
縮退（索引構築失敗→brute-force、`mask_splits_graph`→plain scan）はこの確認
だけでは検出できない。実行時カウンタ（`hnsw_index_cache_stats()`）による
確認は Rust API のみで wire 非露出であり、Issue #659 の担当として申し送る。

追加フェーズ・meta:

| キー | 内容 |
| --- | --- |
| `phases.ann_probe` | 上記 3. の `EXPLAIN` 検証結果（`engine`/`hnsw_params`/`ann_plan`/生の行） |
| `phases.hnsw_index_warm` | 索引構築を含む初回 `vector_knn` 相当クエリの単発計時（`first_query_us`。informational） |
| `phases.bulk_knn_k*`/`bulk_knn_where_k200` | `ef_search`/`ef_effective`（`ef.max(k)`。`hnsw.rs::search_masked_with` の契約からの導出値） |
| `phases.bulk_hybrid_k200` | `ef_search`/`dense_fetch_k_initial`/`dense_fetch_k_may_expand`（直後の段落参照。`ef_effective` は常に `null`） |
| `meta.index` | `hnsw(m=16,ef_construction=100,ef_search=64,resident=f32)` または `exact (no index)` |
| `meta.search_engine` | `hnsw` または `default` |
| `meta.hnsw_args` | `CROSSDB_SELF_HNSW_ARGS` から渡した追加起動引数（無ければ `[]`） |

候補幅は自作 HNSW の既定 `m=16/ef_construction=100/ef_search=64` を使う
（pgvector・Qdrant・LanceDB の既定と一致）。探索側は `ef.max(k)` を内部で
自動適用するため、`bulk_knn_k200`/`bulk_knn_k1000`（k=200/1000）・
`bulk_knn_where_k200` は CLI 無しで候補幅 max(64, k) になる（`k` は SQL の
`LIMIT` そのもの）。

**hybrid（`bulk_hybrid_k200`）の候補幅は上記と異なる**: `ORDER BY
hybrid_rrf(...)` は `sql/hnsw_hybrid.rs::HnswDenseProvider` 経由で HNSW 密側
探索へ結線されるが、渡される候補幅は SQL の `LIMIT`（200）ではなく
`hybrid.rs::hybrid_search_boosted` が内部で管理する `dense_fetch_k` であり、
初回は `pool_depth * 2`（既定 `pool_depth = max(limit, 200)` のため
`dense_fetch_k_initial = 400`）から始まり、境界の同点グループが確定
できない場合は密側再取得ループが `MAX_FETCH_K`（`10,000 * 4`）を上限に
動的に倍増させる。この最終的な実効候補幅は `EXPLAIN`／SQL 表層からは
静的に取得できない（per-query 解決でキャッシュにも載らない）ため、
`self_hnsw.hybrid_ef_candidate_fields` は `ef_effective` を偽って計算せず
常に `None` とし、代わりに下限値 `dense_fetch_k_initial` と拡張され得る
事実を示す `dense_fetch_k_may_expand: true` を記録する。

環境変数:

| 環境変数 | 既定 | 内容 |
| -------- | ---- | ---- |
| `CROSSDB_PLAN_PROBE_BINARY` | `target/release/examples/crossdb_plan_probe` | probe example バイナリのパス上書き（絶対パス正規化・存在検査は fail-closed） |
| `CROSSDB_SELF_HNSW_ARGS` | 未設定（空） | `--config hnsw` の起動引数へ追加する `--hnsw-*` 探索パラメータ opt-in（例: `--hnsw-full-scan-ratio 1/2`）。`shlex.split` 後、フラグ名が `--hnsw-` 接頭辞であること・各フラグの直後に値トークンが 1 個続くことを検証し、不適合は `ValueError` で拒否する（許可リスト方式）。`--config exact` で設定されていた場合も拒否する |
| `CROSSDB_SELF_ANN_PROBE` | 未設定（`0`） | `1` を指定すると `--config exact` でも `ann_probe`（`engine: parallel_brute_force` を期待）を実行する（既定 off。対照値の記録用） |
| `CROSSDB_SELF_DURABILITY` | 未設定（既定 `immediate`） | wire-server の `--durability` opt-in（Issue #851。`self_durability.py`）。`immediate`／`none` の厳密一致のみ受理し、`--config` を問わず適用される。未指定は起動引数を追加せず既定経路のまま不変。`none` 選択時は起動ログの `WARNING` 行有無を検証し arm 取り違えを fail-closed に検出する（`self_durability.verify_arm_identity`） |

`hnsw_f16`/`hnsw_i8` トークン（`--search-engine` が受理する他の ANN 構成）は
出力ファイル名 `self_hnsw.json` の衝突を避けるため本ハーネスの対象外
（別構成を追加する場合は出力名の設計が必要。Issue #658 計画の申し送り）。

前後比較（exact vs hnsw）は `scripts/bench_crossdb_self_hnsw_ab.sh`
（`make bench-crossdb-self-hnsw-ab`）を使う:

```bash
CROSSDB_DIR="$S" CROSSDB_PYTHON=/path/to/venv/bin/python \
  scripts/bench_crossdb_self_hnsw_ab.sh
scripts/bench_crossdb_self_hnsw_ab.sh --summarize docs/design/bench-data/crossdb-self-hnsw-ab
```

### self_nosql（NoSQL 表層経由。`--db self_nosql`）

`self_db.py`（PostgreSQL wire・SQL 表層）と同じ `wire-server` バイナリ・同じ
fixture（redb）を使いつつ、`--surface nosql`（TASK-171・HTTP-1・HTTP-9）で起動し
HTTP/1.1 最小サブセット（`POST /v1/session`・`/v1/session/close`・`/v1/query`）
経由で計測する。JSON 契約の単一情報源は `crates/wire-server/docs/nosql-api.md`
（本 README では要点のみ記載する）。`--config exact` のみ対応（HNSW opt-in は
`self`（SQL wire）側の担当のまま）。Cargo・pip 依存は追加していない
（Python 標準ライブラリ `http.client`／`json` のみ）。

```bash
python scripts/crossdb_bench/run.py --db self_nosql --config exact \
  --rows-file "$S/docs25k.redb" --queries-file "$S/queries200.jsonl"
```

**keep-alive 非対応**: NoSQL 表層は 1 要求ごとに応答へ `Connection: close` を
付けて接続を閉じる契約（`crates/wire-server/src/http/conn.rs` のモジュール
doc・`crates/wire-server/docs/nosql-api.md`「転送路の共通規則」節）。そのため
`self_nosql.py` は毎 HTTP 要求ごとに新規 TCP 接続を張る（`meta.connection` に
明記）。

`self_db.py` の 20 フェーズのうち、NoSQL の `op` 語彙（`search`／`scan`／
`aggregate`／`insert` の 4 値のみ）で表現できるものはそのまま計測し、次の
5 フェーズは構造的に `unsupported` として記録する（HTTP 要求を送らず理由の
みを記録。実行障害〔非 2xx 応答〕はいずれも unsupported へ丸めず例外として
計測全体を止める fail-closed 方針は `self_db.py` と同じ）:

| フェーズ | unsupported の理由 |
| -------- | ------------------- |
| `where_compound_count` | `filter[].op` が `eq`／`prefix` のみで範囲比較演算子が無く `id > 100` を表現できない |
| `udf_call` | 宣言的 UDF 呼び出しに対応する `op` が許可リスト（4 値）に無い |
| `explain` | `vector` 指定検索への `explain:true` は `42601` で拒否される契約（`plan` 検索の `EXPLAIN` は LLM プランナー未接続のため対象外） |
| `ingest_bulk` | COPY 相当のストリーミング一括投入 op が無い。`op: insert` の `rows` 配列は複数行 INSERT 相当（1 要求あたり既定上限 64 行・INDEX-4 ①・要求ごとに commit）で、SQL 表層の COPY 計測と同形の分割投入は未実装 |

`group_by_having` は NoSQL の `aggregate`（`group_by`／`having`）に `ORDER BY`／
`LIMIT` 相当のフィールドが無いため、`self_db.py` の `... ORDER BY n DESC
LIMIT 5` とは全件取得の点で異なる（集計・`HAVING` 条件自体は同一。結果 JSON
の `note` フィールドに明記）。

### 環境変数（コンテナ名）

`containers.sh` が起動する各コンテナの名前は既定で `bench-<db>` 固定だが、
同一ホストで別セッションが同名コンテナを既に使っている場合、冪等化のための
`docker rm -f` がその別セッションの状態を破壊してしまう（Issue #466。他 DB
横断ベンチの実測中に外部 `bench-qdrant` を誤って削除しかけた事故の再発防止）。
以下の環境変数で別名を指定できる（Docker のコンテナ名文字集合 `^[a-zA-Z0-9][a-zA-Z0-9_.-]+$`
に適合しない値は `containers.sh` が fail-closed で拒否する）。名前だけ変えても
既定ポートのままでは外部コンテナと衝突するため、上記のポート変数も必ず
あわせて別値を指定すること。

| 環境変数 | 既定値 | 対象 |
| -------- | ------ | ---- |
| `CROSSDB_PG_CONTAINER` | `bench-pgvector` | pgvector コンテナ名 |
| `CROSSDB_QDRANT_CONTAINER` | `bench-qdrant` | Qdrant コンテナ名 |
| `CROSSDB_MYSQL_CONTAINER` | `bench-mysql` | MySQL コンテナ名 |
| `CROSSDB_MONGODB_CONTAINER` | `bench-mongodb` | MongoDB Atlas local コンテナ名 |
| `CROSSDB_MONGODB_PLAIN_CONTAINER` | `bench-mongodb-plain` | MongoDB Community コンテナ名 |
| `CROSSDB_REDIS_CONTAINER` | `bench-redis` | Redis コンテナ名 |
| `CROSSDB_ES_CONTAINER` | `bench-elasticsearch` | Elasticsearch コンテナ名 |

```bash
# 外部 bench-qdrant（既定ポート）と衝突しない別名・別ポートで計測する例
export CROSSDB_QDRANT_CONTAINER=bench-qdrant-466
export CROSSDB_QDRANT_HTTP_PORT=26333 CROSSDB_QDRANT_GRPC_PORT=26334
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config exact ...
scripts/crossdb_bench/containers.sh down qdrant
```

self の `run.py --db self` は認証ファイル `users.txt` を `--workdir`
（既定: `--rows-file` の親ディレクトリ）直下ではなく、その配下に毎回新規作成する
一意なサブディレクトリ `self_bench_auth_<pid>_*/` へ生成し、停止時に削除する
（`--workdir` 直下に既存の `users.txt` があっても上書きしない）。

## 実行

```bash
S=/path/to/scratch/crossdb  # docs25k.redb / docs25k.jsonl / queries200.jsonl のあるディレクトリ

# self（wire-server 経由）
python scripts/crossdb_bench/run.py --db self --config exact \
  --rows-file "$S/docs25k.redb" --queries-file "$S/queries200.jsonl"

# pgvector（exact / hnsw の 2 構成を別々に計測。コンテナは毎回作り直す想定）
scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
scripts/crossdb_bench/containers.sh down pgvector
scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# sqlite-vec（exact 固定。--config は受け付けるが無視して meta に exact と記録する。
#   ファイルベース DB を --workdir 配下の専用一時ディレクトリに作り終了時に削除する）
python scripts/crossdb_bench/run.py --db sqlite_vec --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# Qdrant
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# LanceDB（in-process。--workdir 配下に lancedb ディレクトリを作る）
python scripts/crossdb_bench/run.py --db lancedb --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# MySQL（KNN 系は構造的に unsupported）
scripts/crossdb_bench/containers.sh up mysql
python scripts/crossdb_bench/run.py --db mysql --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# MongoDB Atlas local（exact は vectorSearch 索引ありで `exact: true`、
#   hnsw は numCandidates=max(64,k)）
scripts/crossdb_bench/containers.sh up mongodb
python scripts/crossdb_bench/run.py --db mongodb --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
scripts/crossdb_bench/containers.sh down mongodb
scripts/crossdb_bench/containers.sh up mongodb
python scripts/crossdb_bench/run.py --db mongodb --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# MongoDB Community（ベクトル検索なし。exact のみ。
#   vector_knn_pipeline_bruteforce は aggregation パイプラインの汎用演算であり、
#   ベクトル検索機能ではない）
scripts/crossdb_bench/containers.sh up mongodb_plain
python scripts/crossdb_bench/run.py --db mongodb_plain --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# Redis（FLAT=exact / HNSW、FT.HYBRID で hybrid）
scripts/crossdb_bench/containers.sh up redis
python scripts/crossdb_bench/run.py --db redis --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
scripts/crossdb_bench/containers.sh down redis
scripts/crossdb_bench/containers.sh up redis
python scripts/crossdb_bench/run.py --db redis --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# Elasticsearch（exact は index:false の script_score、hnsw は knn。
#   RRF は無償ライセンスで 403 のため unsupported）
scripts/crossdb_bench/containers.sh up elasticsearch
python scripts/crossdb_bench/run.py --db elasticsearch --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
scripts/crossdb_bench/containers.sh down elasticsearch
scripts/crossdb_bench/containers.sh up elasticsearch
python scripts/crossdb_bench/run.py --db elasticsearch --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
```

結果は `<queries-file のディレクトリ>/results/<db>_<config>.json`
（`--out-dir` で上書き可）に書き出される。

### dim 別 fixture（Issue #466）

dim=768／1536 が Issue #365（dot カーネル多アキュムレータ化検討）で採否の
判別変数と判明したため、`docs25k.*`／`queries200.jsonl` 固定名とは別に
`-d<dim>` サフィックス付き fixture を生成して計測できる。

```bash
cargo run --release -p fandhe-db-engine --example seed_docs -- seed "$S/docs25k-d768.redb" 25000 768
cargo run --release -p fandhe-db-engine --example seed_docs -- export "$S/docs25k-d768.redb" "$S/docs25k-d768.jsonl"
cargo run --release -p fandhe-db-engine --example seed_docs -- queries 768 200 "$S/queries200-d768.jsonl"

python scripts/crossdb_bench/run.py --db self --config exact \
  --rows-file "$S/docs25k-d768.redb" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768
```

`--docs-file` を省略した場合、self は `--rows-file` と同じ basename（拡張子
のみ `.jsonl`）を自動探索する（`docs25k-d768.redb` → `docs25k-d768.jsonl`）。
`--expect-dim` を指定すると docs／queries の埋め込み長がその値と一致することを
fail-closed に検証する（不一致・fixture の取り違えは非 0 終了で拒否し、
`unsupported` へ丸めない）。`make bench-crossdb` から一括実行する場合は
`CROSSDB_DIM=768`（十進数字のみ）を指定すると上記のファイル名・出力先・
`--expect-dim` を自動で組み立てる（Makefile「bench-crossdb」参照）。

### スモークテスト（2,000 行サブセット）

25,000 行の本計測前に、まず小規模で全フェーズが動く／`unsupported` が正しく
記録されることを確認する。

```bash
head -n 2000 "$S/docs25k.jsonl" > "$S/docs2k.jsonl"
python scripts/crossdb_bench/run.py --db pgvector --config exact \
  --rows-file "$S/docs2k.jsonl" --queries-file "$S/queries200.jsonl"
```

self はサブセット用の別 redb を用意する必要がある（`docs25k.redb` は
25,000 行固定で書き出し済みのため、2,000 行のスモークは他 DB のみで行う）。
sqlite-vec・pgvector 等のスモークでは public/private 双方を含む区間
（例: `sed -n '8501,10500p'`）を切り出すこと。`docs25k.jsonl` は
tenant-a=public・tenant-b=private が連続した区間にまとまっているため、
単純な `head -n 2000` では全行が public になり可視性フィルタの効果を
確認できない。

## フェーズ一覧

`crates/engine/examples/feature_bench.rs` の 13 フェーズに対応させたうえで、
`recall_at_10`（正解集合との突き合わせ）と広域取得 5 種を追加した 20 種類。DB がその機能を
持たない場合は `{"unsupported": true, "reason": "..."}` を記録する
（fail-closed。黙って欠落させない）。

| フェーズ | 内容 |
| -------- | ---- |
| `ingest_bulk` | 一括投入（COPY 相当。self は `COPY ... FROM STDIN`〔CSV〕を専用テーブル `docs_bulk` へ 64 行ずつの複数 COPY で投入し COPY ごとに commit する。tenant／visibility は接続テナントの tenant-a／private 固定で、pgvector が含める索引作成に相当する処理は計測区間に無い。`self_copy.py` 参照） |
| `ingest_single_stmt` | 単文 INSERT ループでの投入（self は行形 INSERT + `USING OPERATION_ID`） |
| `vector_knn` | フィルタなし KNN（Top-10） |
| `vector_knn_where` / `point_where` | `lang = 'ja'` フィルタ付き KNN（同形のため統合） |
| `where_compound_count` | 複合 WHERE の COUNT |
| `agg_count` | `COUNT(*)` |
| `agg_multi` | `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` |
| `group_by_having` | `GROUP BY`/`HAVING` |
| `hybrid_rrf` | ベクトル + 全文検索の RRF 融合 |
| `mode_recall` / `mode_precision` | 取得モード切替（self のみ） |
| `rls_isolation` | tenant-b 接続（self）／同一 public-only フィルタの再実行（他 DB）での `COUNT(*)`（RLS 相当の境界確認） |
| `udf_call` | 宣言的 UDF 呼び出し（self のみ） |
| `explain` | `EXPLAIN` |
| `recall_at_10` | `docs25k.jsonl` の public 行から numpy で計算した内積正解との突き合わせ（同点許容。下記「Recall@10 の算出方法」参照） |
| `bulk_knn_k200` / `bulk_knn_k1000` | 広域取得: フィルタなし Top-200／Top-1000 を `id, body` 込みで返す（LLM へ丸ごと渡す用途。ANN 構成は候補幅を max(64, k) へ引き上げて計測し `ef_search`／`hnsw_ef`／`ef` を記録） |
| `bulk_knn_where_k200` | 広域取得: `lang = 'ja'` フィルタ付き Top-200（`id, body`） |
| `bulk_hybrid_k200` | 広域取得: hybrid RRF の Top-200（`id, body`。pgvector・sqlite-vec は候補プールを 200 へ拡大し `candidate_pool` を記録） |
| `scan_where_nosort_k500` | ORDER BY なしのスカラーフィルタのみ LIMIT 500（`id, body`）。Issue #454 で self の SQL 表層へ広域取得（`Statement::Scan`。`docs/design/wide-retrieval-scan.md`）を追加したため受理される。実行して受理／拒否を都度確認したうえで記録する |
| `ann_probe` | self の `--config hnsw`（または `CROSSDB_SELF_ANN_PROBE=1`）でのみ記録。`EXPLAIN` による選択エンジンの非 vacuous 確認（上記「self の `--config hnsw`」参照。静的判定であり実行時縮退は検出できない） |
| `hnsw_index_warm` | self の `--config hnsw` でのみ記録。索引構築を含む初回クエリの単発計時（informational） |

## 公平性についての注記

- **接続経路の違い**: self・pgvector・MySQL は loopback TCP、sqlite-vec・
  LanceDB は in-process（sqlite-vec はファイルベース DB。`:memory:` ではない）、
  Qdrant は gRPC。経路の違い自体もレイテンシに寄与する
  ため、`meta.connection` に必ず記録し、横並び比較の際はこの違いを踏まえて
  読む。
- **exact / ANN の区別**: `--config exact` は索引なし全件探索、`--config hnsw`
  は各 DB の近似最近傍索引を使う。両者は精度・速度のトレードオフが異なるため
  同じ表で比較する際は必ず `config` を添えて読む。Qdrant の `exact` は
  `indexing_threshold=0` で HNSW の自動構築を無効化する（検索側 `exact=True` は
  構築を止めないため、既定のままだとバックグラウンド構築の負荷が混入し得る）。
- **既定設定のまま**: 各 DB は README に明記した索引パラメータ（pgvector
  m=16/ef_construction=100/ef_search=64、Qdrant 同様、LanceDB
  IVF_HNSW_FLAT m=16/ef_construction=100/ef=64）以外は既定値のまま変更していない
  （postgresql.conf のメモリ・WAL 設定、Qdrant の WAL・optimizer 設定等は
  デフォルト）。
- **fsync 等の永続化設定は未変更**: 公平性のためではなく、単に「素の Docker
  イメージ・デフォルト設定」を比較対象にする方針のため。本番運用のチューニング
  差は本ベンチの対象外。
- **距離指標**: 自作 DB の `<=>` は内積（`crates/engine/src/kernel.rs`
  「スコアは内積」参照）。pgvector は `<#>`（負の内積）、Qdrant は
  `Distance.DOT`、LanceDB は `metric="dot"` に揃えている。sqlite-vec の
  vec0 は内積相当の `distance_metric` を持たない（実機確認: `l2`/`l1`/`cosine`
  のみ受理）ため、コサイン距離で近似し `meta` に注記する。
- **RLS 相当（可視性モデル）**: 実機確認済みの自作 DB の現行契約は「**どの
  テナントの wire セッションからも `visibility = 'public'` の行のみが可視**」
  であり、private 行は所有テナント自身のセッションからも不可視（`docs25k.jsonl`
  の `visibility` フィールド: tenant-a 行 = public・tenant-b 行 = private。
  tenant-a・tenant-b いずれの wire セッションで `SELECT COUNT(*) FROM docs`
  しても public 行数〔25,000 行構成で 23,000〕に一致することを実機で確認済み）。
  他 DB は本物の RLS を持たないため、`visibility` 列（Qdrant は payload、
  LanceDB は `where` 句）を追加したうえで毎クエリに `WHERE visibility =
  'public'` を付ける素朴な模倣であり、`tenant` 列そのものでは絞り込まない。
  ポリシーエンジンとしての比較ではない。self 以外の DB にはテナント別
  セッションの概念が無いため、`rls_isolation` フェーズは同じ public-only
  フィルタを再実行して `agg_count` と同値になることの一致確認として扱う。

## Recall@10 の算出方法

正解集合は `visibility == "public"` な行のみから、200 クエリそれぞれについて
numpy で内積を計算して作る（`recall.py::build_ground_truth`）。

合成 embedding は疎で単純な値（0・±0.2・±0.4 等）の組み合わせが多く、内積が
完全に一致する文書グループができやすい。そのため「厳密な上位 10 件」を
決定的な順序（例: id 昇順）で切ると、その境界がどの 10 件を含むかは同点内の
順序規約に依存してしまい、exact 構成（索引なし全件探索）で比較しても DB 間
で完全一致しない——同じ意味で「正しい」10 件の組み合わせが複数存在するため。

そこで `recall_at_10`（主指標）は正解側を「境界（10 位）より厳密に上位の集合
`strict_above`（スコア > 境界スコア + ε）」と「境界での同点集合 `tie_boundary`」
に分け、分子を `|hits ∩ strict_above| + min(|hits ∩ tie_boundary|, k − |strict_above|)`、
分母を `min(k, 可視件数)` として返す（`recall_at_k_tie_tolerant`）。同点集合
からの充当は残り枠で頭打ちにし、厳密上位の欠落を同点の過剰一致で埋め合わせ
ない（例: 厳密上位 1 件を落として同点 10 件を返すと 0.9）。同点判定には
`TIE_EPSILON = 1e-4` の許容誤差を設けている——数学的には同一の内積値でも、
DB ごとに float32 の総和順序が異なると計算結果が最終桁でずれるため（実機
確認: pgvector と numpy で同一の意味的な同点グループのうち 1 件だけ
`0.4850712716579437` と `0.48507124185562134` のように 1e-7 台で食い違う
ケースを確認）。この許容誤差の導入により、self・pgvector(exact)・sqlite-vec
いずれも exact 構成で `recall_at_10 == 1.0` になることをスモークテストで
確認済み。

従来の厳密一致版（同点は決定的だが恣意的な順序で 10 件に切ったうえで完全一致
を要求する）は `recall_at_10_strict` として引き続き記録する。exact 構成でも
同点境界の順序差により 1.0 未満になり得る（sqlite-vec は距離指標がコサイン
近似のため、同点構造自体が自作 DB〔内積〕と異なりさらに低くなり得る）。

## 既知の unsupported 項目（スモークテストで確認済み）

- **MySQL**: `vector_knn`・`vector_knn_where`・`point_where`・`hybrid_rrf`。
  `VECTOR(128)` 列自体は作成でき `STRING_TO_VECTOR()`/`VECTOR_TO_STRING()` で
  入出力できるが、KNN 用の `DISTANCE()` 関数が存在しない
  （実機確認: `ERROR 1305 (42000): FUNCTION bench.DISTANCE does not exist`）。
  `CREATE VECTOR INDEX ...` も構文エラー
  （実機確認: `ERROR 1064 (42000)`）。
- **sqlite-vec**: `distance_metric=dot`/`ip`/`inner_product` はいずれも
  vec0 constructor error（実機確認）のため `cosine` で近似。vec0 の
  auxiliary 列（`+col`）は KNN の WHERE 句に使えない
  （実機確認: `An illegal WHERE constraint was provided on a vec0 auxiliary
  column in a KNN query.`）ため、`visibility` 列は `partition key` として
  宣言する（`tenant`・`lang` は素の列）。
- **Qdrant**: `agg_multi`・`group_by_having`（SUM/AVG/MIN/MAX・GROUP BY 相当の
  API が無い）・`hybrid_rrf`（疎ベクトルを自前生成すると自作 DB の BM25 系
  hybrid と非等価）・`mode_recall`/`mode_precision`・`udf_call`・`explain`。
- **pgvector・LanceDB**: `mode_recall`/`mode_precision`・`udf_call`
  （取得モード切替・宣言的 UDF 呼び出しの概念自体が無い）。
- **MongoDB Community（`mongodb_plain`）**: `vector_knn`・`vector_knn_where`・
  `point_where`・`bulk_knn_k200`・`bulk_knn_k1000`・`bulk_knn_where_k200`・
  `hybrid_rrf`（ベクトル検索 API そのものが無い。`vector_knn_pipeline_bruteforce`
  のみ aggregation パイプラインの汎用演算で実装）。
- **Elasticsearch**: `hybrid_rrf`・`bulk_hybrid_k200`（RRF retriever が無償
  ライセンスで `403 security_exception` として拒否される）・`mode_recall`/
  `mode_precision`・`udf_call`。
- **Redis**: `mode_recall`/`mode_precision`・`udf_call`（集計は `FT.AGGREGATE`、
  hybrid は `FT.HYBRID`、EXPLAIN は `FT.EXPLAIN` で計測する）。
- **MongoDB（Atlas local）**: `mode_recall`/`mode_precision`・`udf_call`。
- **self**: `ingest_bulk` は SQL 表層では `COPY ... FROM STDIN`（TASK-220・WIRE-17）で
  計測する（`self_copy.py`）。docs jsonl が見つからない場合のみ unsupported を記録する。

## ファイル構成

| ファイル | 役割 |
| -------- | ---- |
| `common.py` | フィクスチャ読み込み・レイテンシ統計・meta 構築・計測ループの共通部品 |
| `recall.py` | numpy による内積正解集合の計算（同点許容込み）・Recall@10 突き合わせ |
| `self_db.py` | self（wire-server 経由・SQL wire 表層）の全フェーズ実装 |
| `self_nosql.py` | self（wire-server `--surface nosql`・HTTP NoSQL 表層）の全フェーズ実装 |
| `pgvector_db.py` | pgvector の全フェーズ実装 |
| `sqlite_vec_db.py` | sqlite-vec の全フェーズ実装 |
| `qdrant_db.py` | Qdrant の全フェーズ実装 |
| `lancedb_db.py` | LanceDB の全フェーズ実装 |
| `mysql_db.py` | MySQL の全フェーズ実装 |
| `mongodb_db.py` | MongoDB Atlas local の全フェーズ実装 |
| `mongodb_plain_db.py` | MongoDB Community の全フェーズ実装 |
| `redis_db.py` | Redis の全フェーズ実装 |
| `elasticsearch_db.py` | Elasticsearch の全フェーズ実装 |
| `run.py` | CLI エントリポイント（フィクスチャ読み込み → 各 db モジュール呼び出し → recall 計算 → JSON 書き出し） |
| `containers.sh` | pgvector・Qdrant・MySQL・MongoDB・Redis・Elasticsearch コンテナの起動・停止（`docker rm -f` で毎回冪等に作り直す） |

## 前後比較用ハーネス（Issue #633）

- `hybrid_after_where.py`: `run.py` の全フェーズ実行とは別に、「WHERE クエリを
  何本か実行してスカラー索引を暖機した後の `hybrid_rrf` レイテンシ・
  wire-server RSS」だけを単独で計測する最小ハーネス。3 モード
  （`hybrid`／`warm_where_then_hybrid`／`body_predicate`）は本ファイル冒頭の
  ドキュメンテーションコメント参照。単体では使わず
  `scripts/bench_scalar_index_crossdb_ab.sh`（前後比較ドライバ。リポジトリ
  ルートの Makefile `bench-scalar-index-crossdb-ab` から起動）から呼ぶ想定。
- 前後比較ドライバは `CROSSDB_SELF_PORT`（既定 15437。`CROSSDB_SELF_PORT_BASE`
  環境変数で上書き）で `self_db.py` の既定ポート（15432）以外を使う。
  他ジョブが同時に crossdb_bench を実行していてもポート衝突で計測が
  止まらないようにするため。

## 交互 N ラウンド再計測ハーネス（Issue #848）

`run_all.sh` フル一括（self exact/hnsw/nosql・対照 DB 全系統。Elasticsearch を
含む）を N≥5 ラウンド輪番実行し、self との勝敗を
`docs/design/benchmark-judgement-policy.md` 規約（min-of-N＋median・run-to-run
幅）で集計するドライバ。`bench_crossdb_self_hnsw_ab.sh` 等の 2 arm 厳密輪番
とは異なり、`run_all.sh` 自体が 1 回で十数 arm を実行する構造のため、
そのフル一括を 1 ラウンドとみなしてラウンドロビンする（cand が DB の数だけ
あり baseline→cand→baseline→cand... の 2 arm 輪番を適用できない構造的な
理由による意図的な逸脱）。

```bash
CROSSDB_DIR="$S" CROSSDB_PYTHON="$V" scripts/bench_crossdb_ab.sh
# 標準出力の最後の行が実際に使うべき --summarize コマンド
# （例: scripts/bench_crossdb_ab.sh --summarize "$S" 5 20260919T000000Z-round）
```

- `run_all.sh` は任意環境変数 `CROSSDB_RUN_TAG`（英数字・ハイフン・
  アンダースコアのみ）を受け付け、指定時は `results`／`logs` の下へさらに
  1 段のサブディレクトリを切る。`bench_crossdb_ab.sh` はセッション起動時刻
  `<ts>` を含む `<ts>-round<N>` を使う（同じ `CROSSDB_DIR` へ複数セッション
  を実行しても前回セッションの JSON が残留・混在しないようにするため）。
  未設定時は `run_all.sh` 自体は完全に既存動作のまま（後方互換）。
- 生データ: `$S/results/<ts>-round<N>/<db>_<config>.json`・
  `$S/logs/<ts>-round<N>/<db>_<config>.log`。環境記録:
  `docs/design/bench-data/crossdb-<ts>-ab/env.txt`。
- `--summarize <dir> <rounds> [<round_dir_prefix>]`（`round_dir_prefix` 省略時は
  既定 `round`。既存コミット済み生データ `docs/design/bench-data/
  crossdb-20260918T142251Z-ab/` はこの旧セッション形式〔`results/round<N>`〕
  のため省略形のまま集計できる。`bench_crossdb_ab.sh` 自身の実行結果を集計
  するときは、その標準出力が示す `<ts>-round` を第 3 引数に渡す）。
- `hybrid_rrf` フェーズ診断用に `redis_db.py` が任意環境変数
  `CROSSDB_REDIS_HYBRID_WINDOW`（正の 10 進整数のみ・既定 50）を受け付ける。
  self の `hybrid_rrf` は既定でプール深さ 200（`hybrid.rs::RrfConfig::
  default`）を使う一方、Redis 側の既定 `WINDOW` は 50、pgvector 側の
  hybrid 候補プールは各 50（`bulk_hybrid_k200` のみ 200 に揃えてある）と
  条件が異なる。`CROSSDB_REDIS_HYBRID_WINDOW=200` で Redis 側を self の
  pool_depth に揃えた informational な追加計測ができる（本番スコアボードの
  既定計測条件は変えない。`redis_db.py` 側の既定値は 50 のまま不変）。
