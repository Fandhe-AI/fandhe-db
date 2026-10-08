"""`self_db.py` の `--config hnsw` から使う補助部品（Issue #658）。

`--search-engine hnsw` opt-in（Issue #656・#657。`crates/wire-server/src/main.rs`）を
crossdb ベンチの self（wire-server 経由）構成として選択できるようにするための:

- `PlannerStub`: `EXPLAIN SELECT ... USING PLAN(...)` を通すために wire-server が
  必要とする `--planner-endpoint`/`--planner-model`（TASK-117・PLAN-9）へ渡す、
  loopback 専用の固定応答 HTTP スタブ（実 Ollama 不要）。
- `parse_hnsw_args_env`: `CROSSDB_SELF_HNSW_ARGS` 環境変数から `--hnsw-*`
  探索パラメータ opt-in（Issue #657）の追加起動引数を許可リスト検証つきで取り出す。
- `verify_explain_rows`: `EXPLAIN` の返却行から `engine:`/`hnsw_params:`/`ann_plan:`
  を抽出し、期待どおり ANN（`hnsw`）または既定エンジン（`parallel_brute_force`）を
  指しているかを検証する（`crates/wire-server/tests/wire_search_engine_opt.rs` と
  同じ行構造を wire 経由で確認する。静的判定であり実行時縮退〔brute-force への
  フォールバック・`mask_splits_graph`〕は見えないことに注意——非 vacuous 確認の
  限界は README・docs/design/crossdb-bench.md に記載）。
- `ef_effective`: 探索側の `ef.max(k)`（hnswlib 慣行。`hnsw.rs::search_masked_with`）
  を候補幅記録用に計算する。
- `probe_binary_path`: `crossdb_plan_probe`（`crates/engine/examples/`）example
  バイナリのパスを解決する（`CROSSDB_SELF_BINARY` と同型の絶対パス正規化・
  存在検査。Issue #479 の方針を踏襲）。
"""

from __future__ import annotations

import http.server
import os
import shlex
import threading


# `--search-engine` opt-in を選んだ構成でのみ使う既定探索幅（`hnsw.rs` の
# `ValidatedHnswParams` 既定値。`m=16/ef_construction=100/ef_search=64` は
# pgvector・Qdrant・LanceDB の既定と一致する。README・docs 記録用の定数）。
DEFAULT_EF_SEARCH = 64

# `--search-engine` が受理する ANN 系 4 トークンのうち crossdb ベンチが対象と
# する構成（`hnsw_f16`/`hnsw_i8` は出力ファイル名 `self_hnsw.json` が衝突する
# ため対象外。Issue #658 計画の申し送り）。
SEARCH_ENGINE_TOKEN = "hnsw"

# 固定の LLM 展開応答（Ollama `/api/generate` 応答本体の `response` フィールドに
# 相当する JSON 文字列。`crates/engine/src/query_planner.rs::parse_expansion` が
# 受理する最小形。`EXPLAIN` は検索本体を実行しないため検索語の中身に意味は無い）。
_FIXED_EXPANSION_JSON = '{"search_terms": ["probe"], "path_hint": null, "kind_hint": null}'


class _PlannerStubHandler(http.server.BaseHTTPRequestHandler):
    """`POST /api/generate` に固定応答を返す最小ハンドラ。

    `query_planner.rs` のクライアントはリクエストごとに新規 TCP 接続を張り
    `Connection: close` を送るため（`http_post_json`）、1 リクエスト 1 接続の
    単純な処理で足りる。GET 等の他メソッドは扱わない（未対応は接続を切るのみ
    で足り、wire-server 側の待ち受けは `--planner-endpoint` 未疎通として
    タイムアウト・エラーになる）。
    """

    protocol_version = "HTTP/1.0"

    def log_message(self, format: str, *args) -> None:  # noqa: A002 - BaseHTTPRequestHandler 契約
        # 標準出力へ大量にログを吐かない（計測プロセスの stderr を汚さない）。
        pass

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler 契約
        # リクエスト本文（プロンプト）は読み捨てる（本スタブは中身を見ない
        # 固定応答のみを返す。読み捨てないと後続の keep-alive 判定やソケット
        # 状態が乱れ得るため、Content-Length 分だけ確実に読み切る）。
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length > 0:
            self.rfile.read(length)
        body = ('{"model":"crossdb-stub","response":' + _json_quote(_FIXED_EXPANSION_JSON) + ',"done":true}').encode(
            "utf-8"
        )
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def _json_quote(s: str) -> str:
    """`s`（`_FIXED_EXPANSION_JSON` のような単純な JSON 文字列）を JSON 文字列
    リテラルとしてエスケープする（固定語彙専用の最小実装。`json.dumps` と等価
    だが標準ライブラリの依存を明示するために直接呼ぶ）。"""
    import json

    return json.dumps(s)


class PlannerStub:
    """loopback（127.0.0.1）のエフェメラルポートで daemon スレッド起動する
    固定応答 planner スタブ。`start()`/`stop()` は多重呼び出し可。

    wire-server 側の `--planner-endpoint`/`--planner-model` は「両方指定時のみ
    LLM クライアントを構築する」契約（TASK-117）のため、`endpoint` プロパティ
    を起動引数へそのまま渡せば足りる。
    """

    def __init__(self) -> None:
        self._server: http.server.HTTPServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        if self._server is not None:
            return
        self._server = http.server.HTTPServer(("127.0.0.1", 0), _PlannerStubHandler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def endpoint(self) -> str:
        if self._server is None:
            raise RuntimeError("PlannerStub.start() must be called before endpoint is read")
        host, port = self._server.server_address[:2]
        return f"{host}:{port}"

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
        if self._thread is not None:
            self._thread.join(timeout=5.0)
            self._thread = None


def parse_hnsw_args_env(raw: str | None) -> list[str]:
    """`CROSSDB_SELF_HNSW_ARGS` 環境変数値（`--hnsw-full-scan-ratio 1/2` のような
    shell 風トークン列）を wire-server 起動引数へ追加できる形へ検証・変換する
    （Issue #659 向けの最小対応）。

    許可リスト: フラグ名は `--hnsw-` 接頭辞のみ、各フラグの直後の 1 トークンだけを
    値として許容する。それ以外（`--bind` 等の無関係フラグ・値を伴わない末尾
    フラグ・フラグでないトークンの混入）は `ValueError` で fail-closed に拒否する
    （untrusted な shell 文字列を wire-server の起動引数へ無検証で連結しない。
    coding-rust.md の SQL 文字列組み立てと同じ理由で CLI 引数組み立てにも適用）。
    """
    if raw is None or raw.strip() == "":
        return []
    tokens = shlex.split(raw)
    result: list[str] = []
    i = 0
    while i < len(tokens):
        flag = tokens[i]
        if not flag.startswith("--hnsw-"):
            raise ValueError(
                f"CROSSDB_SELF_HNSW_ARGS: only --hnsw-* flags are allowed, got {flag!r}"
            )
        if i + 1 >= len(tokens):
            raise ValueError(
                f"CROSSDB_SELF_HNSW_ARGS: flag {flag!r} is missing its value token"
            )
        value = tokens[i + 1]
        if value.startswith("--"):
            raise ValueError(
                f"CROSSDB_SELF_HNSW_ARGS: flag {flag!r} value token {value!r} "
                "looks like another flag (missing value?)"
            )
        result.extend([flag, value])
        i += 2
    return result


def ef_effective(k: int, ef_search: int = DEFAULT_EF_SEARCH) -> int:
    """探索側の `ef.max(k)`（`hnsw.rs::search_masked_with` の実装契約）を返す。
    候補幅規約（max(64, k)）を記録するためだけに使う（wire 経由の実測値ではなく
    契約からの導出値）。

    `hybrid_rrf` 経由のクエリ（`ORDER BY hybrid_rrf(...)`）には適用できない
    （`k` に最終 `LIMIT` をそのまま渡すと実際に HNSW 探索へ渡される候補幅と
    一致しない。`hybrid_ef_candidate_fields` を使うこと）。
    """
    return max(ef_search, k)


# hybrid_rrf 経路の密側候補幅（Issue #658 codex-review P2 対応）。
# `sql/exec.rs::DEFAULT_HYBRID_POOL_DEPTH`（200）由来の `pool_depth =
# max(limit, 200)` と、`hybrid.rs::hybrid_search_boosted` の初回 `dense_fetch_k
# = pool_depth * 2` を踏まえた「HNSW 探索へ渡される初回候補幅」の下限値。
# `hybrid.rs::MAX_POOL_DEPTH`（10,000）* 4 が絶対上限（`MAX_FETCH_K`）。
_HYBRID_DEFAULT_POOL_DEPTH = 200
_HYBRID_MAX_FETCH_K = 10_000 * 4


def hybrid_ef_candidate_fields(limit: int, ef_search: int = DEFAULT_EF_SEARCH) -> dict:
    """`hybrid_rrf` クエリの候補幅記録用フィールドを返す。

    `ef_effective`（HNSW 探索が実際に使う候補幅）は境界の同点グループが
    確定できない場合に `hybrid.rs::hybrid_search_boosted` の再取得ループが
    `dense_fetch_k` を倍増（上限 `MAX_FETCH_K`）させながら動的に決めるため、
    `EXPLAIN`／SQL 表層からは静的に取得できない（`sql::hnsw_hybrid` は
    per-query 解決でキャッシュにも載らない）。実効値を偽って記録しないよう
    `ef_effective` は常に `None` とし、代わりに `dense_fetch_k_initial`
    （初回取得幅。実際の探索幅の下限値）と `dense_fetch_k_may_expand`
    （拡張され得る事実。他 DB との候補幅比較で「これで確定」と誤読しない
    ための注記）を記録する。"""
    pool_depth = max(limit, _HYBRID_DEFAULT_POOL_DEPTH)
    dense_fetch_k_initial = min(pool_depth * 2, _HYBRID_MAX_FETCH_K)
    return {
        "ef_search": ef_search,
        "ef_effective": None,
        "dense_fetch_k_initial": dense_fetch_k_initial,
        "dense_fetch_k_may_expand": True,
    }


def probe_binary_path() -> str:
    """`crossdb_plan_probe` example バイナリの絶対パスを解決する。

    `CROSSDB_PLAN_PROBE_BINARY` で上書きでき、指定時は絶対パスへ正規化した
    うえで存在確認する（`SelfServer._default_binary`〔Issue #479〕と同型の
    fail-closed 契約）。未設定時の既定は
    `target/release/examples/crossdb_plan_probe`（リポジトリルート基準）。
    """
    override = os.environ.get("CROSSDB_PLAN_PROBE_BINARY")
    if override:
        override = os.path.abspath(override)
        if not os.path.exists(override):
            raise FileNotFoundError(
                f"CROSSDB_PLAN_PROBE_BINARY points to a nonexistent path: {override}"
            )
        return override
    repo_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
    path = os.path.join(repo_root, "target", "release", "examples", "crossdb_plan_probe")
    if not os.path.exists(path):
        raise FileNotFoundError(
            f"crossdb_plan_probe binary not found: {path}"
            "（`cargo build --release -p fandhe-db-engine --example crossdb_plan_probe` を先に実行）"
        )
    return path


def verify_explain_rows(rows: list[str], expect_hnsw: bool) -> dict:
    """`EXPLAIN` の返却行（1 列ずつのテキスト）から `engine:`/`hnsw_params:`/
    `ann_plan:` 行を抽出し、`expect_hnsw` が期待するエンジンと一致するかを検証
    する（一致しなければ `RuntimeError`。fail-closed——exact 構成の結果を ANN
    経路として誤記録しない）。

    `crates/wire-server/tests/wire_search_engine_opt.rs` が固定する行構造
    （`engine:` の直後に `hnsw` のときのみ `hnsw_params:`、続けて `ann_plan:`）
    をそのまま前提にする。`EXPLAIN` は検索本体を実行しない静的判定のため、
    実行時縮退（構築失敗→brute-force、`mask_splits_graph`→plain scan 等）は
    ここでは確認できない（呼び出し元のコメント・docs に明記する）。
    """
    engine_line = next((r for r in rows if r.startswith("engine: ")), None)
    if engine_line is None:
        raise RuntimeError(f"EXPLAIN output has no 'engine:' line: {rows!r}")
    engine_value = engine_line[len("engine: ") :]

    if expect_hnsw:
        if engine_value != "hnsw":
            raise RuntimeError(
                f"expected EXPLAIN engine: hnsw, got {engine_value!r} (rows={rows!r})"
            )
    else:
        if engine_value == "hnsw":
            raise RuntimeError(
                f"expected non-hnsw EXPLAIN engine, got 'hnsw' (rows={rows!r})"
            )

    hnsw_params_line = next((r for r in rows if r.startswith("hnsw_params: ")), None)
    if expect_hnsw and hnsw_params_line is None:
        raise RuntimeError(f"expected 'hnsw_params:' line when engine is hnsw: {rows!r}")
    if not expect_hnsw and hnsw_params_line is not None:
        raise RuntimeError(f"unexpected 'hnsw_params:' line when engine is not hnsw: {rows!r}")

    ann_plan_line = next((r for r in rows if r.startswith("ann_plan: ")), None)
    if ann_plan_line is None:
        raise RuntimeError(f"EXPLAIN output has no 'ann_plan:' line: {rows!r}")
    ann_plan_value = ann_plan_line[len("ann_plan: ") :]

    return {
        "engine": engine_value,
        "hnsw_params": hnsw_params_line[len("hnsw_params: ") :] if hnsw_params_line else None,
        "ann_plan": ann_plan_value,
        "rows": rows,
    }
