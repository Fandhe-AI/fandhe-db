# 衝突有無で応答時間分布を比較するベンチハーネス

Issue #2079（親 #2078。ポインタ: RLS-9・RLS-10）。他テナントにだけ UNIQUE 値・FK 親がある状態と、
どこにも無い状態で、同一の書き込み文の応答時間分布を比較するための計測入口を記録する。
応答内容の不変性は `crates/engine/tests/rls10_write_constraint_paths.rs` の T4 が固定しており、
本ハーネスはその対象外だったレイテンシ分布の計測を担う。

判定基準（分位点差の上限）・実測値の記録・`bench.yml` への配線は Issue #2080 の担当で、
**このファイルには数値・閾値を書かない**（#2080 が追記する）。ハーネスは pass/fail を判定しない。

## 実行方法

```bash
make bench-rls-timing
BENCH_DEDICATED_ENV=1 BENCH_RLS_TIMING_ROUNDS=10 make bench-rls-timing
```

| env | 範囲・既定 | 内容 |
| --- | ---------- | ---- |
| `BENCH_RLS_TIMING_ROUNDS` | 5..=50・5 | ラウンド数。ラウンドごとに形状順・arm 順を輪番させる |
| `BENCH_RLS_TIMING_ITERS` | 200..=20000・1000 | 1 セル（ラウンド × 形状 × arm）あたりの計測回数 |
| `BENCH_RLS_TIMING_SHAPES` | 既定は全形状 | `unique_violation,fk_missing_parent,normal` の部分集合（完全一致・重複不可） |
| `BENCH_RLS_TIMING_OUT_DIR` | `target/bench-rls-timing/<unix-ts>` | 出力先。`raw.json` が既にあれば上書きを拒否する |
| `BENCH_DEDICATED_ENV` | 未設定 | `1` で専有環境の自己申告。未設定の run は共有環境の参考値で、採否の根拠にしない |

不正な値は非 0 終了する。`GITHUB_ACTIONS` 下で `BENCH_DEDICATED_ENV=1` を申告する組み合わせは矛盾として拒否する。

## 形状と arm

| 形状 | 文 | 期待する応答（両 arm 共通） |
| ---- | -- | --------------------------- |
| `unique_violation` | 既存の UNIQUE 値を持つ `INSERT` | `23505` |
| `fk_missing_parent` | 他テナントにだけ存在する親キーを参照する子の `INSERT` | `23503` |
| `normal` | 反復ごとに異なる id・UNIQUE 値の `INSERT` | 成功・影響 1 行 |

- `baseline`: 閲覧テナントの行のみ
- `flooded`: 他 2 テナントが、同一の id・UNIQUE 値・FK 親キー（`normal` では各反復の値）を持つ

seed は `rls10_write_constraint_paths.rs` と同じ方式（従来方式の表 DDL、`insert_typed_row` による投入、
テンプレート DB のファイル複製）。計測前に、各形状が期待どおりの応答であること、2 arm の応答
（`wire_code`・`client_message`・影響行数）が一致すること、flooded に他テナント行が実在し baseline には無いこと
を確かめ、外れれば非 0 終了する。計測中に期待外の応答が 1 件でもあれば非 0 終了する。

## JSON スキーマ（`raw.json`、`schema_version: 1`）

- `bench`・`issue`・`quantile_method`（`nearest_rank`）・`unit`（`ns`）
- `env`: OS・アーキテクチャ・論理コア数・ISA・loadavg・`dedicated_env_attested`・`github_actions`・`git_commit`
- `config`: ラウンド数・warmup・計測回数・シード・形状
- `rounds[]`: `{round, cells[]: {shape, arm, expected_outcome, mismatches, samples_ns[]}}`。生サンプルは計測順
- `summary[]`: `{shape, arm, pooled: {p50_ns, p95_ns, p99_ns, n}, per_round[]}`

`summary` は `samples_ns` から再計算できる。nearest-rank は、昇順に並べた n 件のうち `ceil(q * n)` 番目（1 始まり）を取る。
ローカルの絶対パスは JSON に書かない。

## 既知の交絡（#2080 の判定設計で扱う）

flooded は「衝突」と「他テナントのデータ量」の両方が baseline と異なる。2 arm だけでは両者を分離できないため、
flooded と同じ行数で衝突しない値を持つ対照 arm の要否は、#2080 が判定設計の際に決める。
