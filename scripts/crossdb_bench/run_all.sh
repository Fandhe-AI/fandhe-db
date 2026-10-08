#!/bin/bash
# 他 DB 横断ベンチ（scripts/crossdb_bench/run.py）を DB ごとに直列実行する。
# `make bench-crossdb` から呼ばれる。計測対象以外のコンテナは公平性のため停止する。
#
# 必須環境変数:
#   CROSSDB_DIR    fixture ディレクトリ（docs25k.redb / docs25k.jsonl / queries200.jsonl を置く。
#                  results/ と logs/ をこの配下に作る）
#   CROSSDB_PYTHON venv の python（requirements.txt を pip install 済みのもの）
#
# 任意環境変数（Issue #466）:
#   CROSSDB_DIM    十進数字のみ（例 768）。指定時は既定ファイル名を
#                  docs25k-d${CROSSDB_DIM}.{redb,jsonl}／queries200-d${CROSSDB_DIM}.jsonl
#                  へ、結果・ログの出力先を results/d${CROSSDB_DIM}／logs/d${CROSSDB_DIM}
#                  へ切り替える（dim=128 の既存結果を上書きしないため）。
#                  CROSSDB_REDB／CROSSDB_DOCS／CROSSDB_QUERIES の明示指定はこの既定より
#                  常に優先する。未設定なら従来どおり docs25k.*／queries200.jsonl／
#                  results／logs を使う（後方互換）。
#
# 任意環境変数（Issue #848）:
#   CROSSDB_RUN_TAG  英数字・ハイフン・アンダースコアのみ（例 round3）。指定時は
#                    結果・ログの出力先へさらに 1 段のサブディレクトリ
#                    results[/d${DIM}]/${CROSSDB_RUN_TAG}／
#                    logs[/d${DIM}]/${CROSSDB_RUN_TAG} を切る（交互 N ラウンド計測で
#                    ラウンドごとに生データを分離するため。scripts/bench_crossdb_ab.sh
#                    が使う）。未設定なら従来どおり（後方互換）。パス構成要素として
#                    シェル・ファイルシステムへそのまま使うため、上記文字集合以外
#                    （ディレクトリトラバーサル `../`・スラッシュ・シェルメタ文字を含む）
#                    は拒否する（fail-closed）。
set -u
R=$(cd "$(dirname "$0")/../.." && pwd)
B=$R/scripts/crossdb_bench
S=${CROSSDB_DIR:?CROSSDB_DIR is required}
V=${CROSSDB_PYTHON:?CROSSDB_PYTHON is required}
DIM=${CROSSDB_DIM:-}
case "$DIM" in
  '') SUFFIX='' ;;
  *[!0-9]*)
    echo "CROSSDB_DIM must contain only decimal digits (got: $DIM)" >&2
    exit 1
    ;;
  *) SUFFIX="-d${DIM}" ;;
esac
RUN_TAG=${CROSSDB_RUN_TAG:-}
case "$RUN_TAG" in
  '') ;;
  *[!A-Za-z0-9_-]*)
    echo "CROSSDB_RUN_TAG must contain only [A-Za-z0-9_-] (got: $RUN_TAG)" >&2
    exit 1
    ;;
esac
ROWS_REDB=${CROSSDB_REDB:-$S/docs25k${SUFFIX}.redb}
ROWS_JSONL=${CROSSDB_DOCS:-$S/docs25k${SUFFIX}.jsonl}
QUERIES=${CROSSDB_QUERIES:-$S/queries200${SUFFIX}.jsonl}
if [ -n "$DIM" ]; then
  RESULTS_DIR="$S/results/d${DIM}"
  LOGS_DIR="$S/logs/d${DIM}"
else
  RESULTS_DIR="$S/results"
  LOGS_DIR="$S/logs"
fi
if [ -n "$RUN_TAG" ]; then
  RESULTS_DIR="$RESULTS_DIR/$RUN_TAG"
  LOGS_DIR="$LOGS_DIR/$RUN_TAG"
fi
cd "$R"
mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# `CROSSDB_SELF_HNSW_ARGS` は self の `--config hnsw` にのみ意味を持つ opt-in
# （`scripts/crossdb_bench/self_db.py::run` 参照）。この変数を無条件にプロセス
# 環境へ残したまま `self exact` を実行すると、`self_db.run()` が「exact 構成で
# --hnsw-* tuning が設定されている」として ValueError を送出し `run "$ROWS_REDB"
# self exact` が必ず失敗する（対照結果が欠落し一括実行全体も非 0 終了する）。
# `scripts/bench_crossdb_self_hnsw_ab.sh` と同様に値を一度取り出したうえで
# 変数自体を unset し、self hnsw を呼ぶ直前だけ再エクスポートする。
HNSW_ARGS_FOR_SELF_HNSW=${CROSSDB_SELF_HNSW_ARGS:-}
unset CROSSDB_SELF_HNSW_ARGS

# `CROSSDB_SELF_DURABILITY` は `scripts/bench_ingest_durability_ab.sh`（Issue #851）
# 専用の opt-in で、`self_db.py::run` は `--config` を問わず参照する。一括実行
# スコアボードでこの変数が呼び出し元の環境に残っていると、意図せず `none`
# arm（durability を落とした非永続計測）でスコアボードが走ってしまう
# （`CROSSDB_SELF_HNSW_ARGS` と同じ漏れ防止パターン）。
unset CROSSDB_SELF_DURABILITY

# 失敗した DB/構成・コンテナ起動を蓄積し、後片付け後に非 0 で終了する
# （握りつぶすと結果 JSON が欠けても make bench-crossdb が成功に見え、既存の古い
# JSON を今回の結果と誤認しうる）。
FAILED=()

run() {
  echo "== $(date +%T) $2 $3"
  expect_dim_args=()
  if [ -n "$DIM" ]; then
    expect_dim_args=(--expect-dim "$DIM")
  fi
  if ! "$V" "$B/run.py" --rows-file "$1" --queries-file "$QUERIES" --docs-file "$ROWS_JSONL" \
    --out-dir "$RESULTS_DIR" --db "$2" --config "$3" "${expect_dim_args[@]+"${expect_dim_args[@]}"}" \
    > "$LOGS_DIR/$2_$3.log" 2>&1; then
    echo "FAILED $2 $3 (see $LOGS_DIR/$2_$3.log)"
    FAILED+=("$2/$3")
  fi
}

# コンテナ起動に失敗したら当該 DB の計測を飛ばし失敗として記録する
up() {
  if bash "$B/containers.sh" up "$1"; then return 0; fi
  echo "FAILED containers.sh up $1"
  FAILED+=("container:$1")
  return 1
}

# 対象以外のコンテナを止める（本ハーネスが起動したもののみ）
for c in pgvector qdrant mysql mongodb mongodb_plain redis elasticsearch; do bash "$B/containers.sh" down "$c" >/dev/null 2>&1 || true; done

run "$ROWS_REDB" self exact
# self の hnsw 構成（`--search-engine hnsw` opt-in。Issue #656〜#658）は
# `crossdb_plan_probe` example（`cargo build --release -p fandhe-db-engine --example
# crossdb_plan_probe`）のビルドを追加で要求する。未ビルドなら本行は失敗として
# 記録される（`FAILED` に積まれ非 0 終了。ログは `$LOGS_DIR/self_hnsw.log`）。
# `CROSSDB_SELF_HNSW_ARGS` は self hnsw の呼び出しにだけ渡す（上記理由）。
if [ -n "$HNSW_ARGS_FOR_SELF_HNSW" ]; then
  export CROSSDB_SELF_HNSW_ARGS="$HNSW_ARGS_FOR_SELF_HNSW"
fi
run "$ROWS_REDB" self hnsw
unset CROSSDB_SELF_HNSW_ARGS
# self の NoSQL 表層（HTTP `--surface nosql`。self_nosql.py）は exact 構成のみ
# 対応する（HNSW opt-in は self（SQL wire）側の担当のまま）。
run "$ROWS_REDB" self_nosql exact
run "$ROWS_JSONL" sqlite_vec exact
run "$ROWS_JSONL" lancedb exact
run "$ROWS_JSONL" lancedb hnsw
if up pgvector; then
  run "$ROWS_JSONL" pgvector exact
  run "$ROWS_JSONL" pgvector hnsw
fi
bash "$B/containers.sh" down pgvector
if up qdrant; then
  run "$ROWS_JSONL" qdrant exact
  run "$ROWS_JSONL" qdrant hnsw
fi
bash "$B/containers.sh" down qdrant
if up mysql; then
  run "$ROWS_JSONL" mysql exact
fi
bash "$B/containers.sh" down mysql
# NoSQL 対照（2026-09-18 追加）。mongodb（Atlas local・ベクトル検索あり）・redis・
# elasticsearch は exact/hnsw 両構成、mongodb_plain（Community・ベクトル検索なし）は
# exact のみ（hnsw は module 側が ValueError で拒否する）。
if up mongodb; then
  run "$ROWS_JSONL" mongodb exact
  run "$ROWS_JSONL" mongodb hnsw
fi
bash "$B/containers.sh" down mongodb
if up mongodb_plain; then
  run "$ROWS_JSONL" mongodb_plain exact
fi
bash "$B/containers.sh" down mongodb_plain
if up redis; then
  run "$ROWS_JSONL" redis exact
  run "$ROWS_JSONL" redis hnsw
fi
bash "$B/containers.sh" down redis
if up elasticsearch; then
  run "$ROWS_JSONL" elasticsearch exact
  run "$ROWS_JSONL" elasticsearch hnsw
fi
bash "$B/containers.sh" down elasticsearch
if [ "${#FAILED[@]}" -gt 0 ]; then
  echo "== $(date +%T) FAILED (${#FAILED[@]}): ${FAILED[*]}"
  exit 1
fi
echo "== $(date +%T) done: $RESULTS_DIR"
