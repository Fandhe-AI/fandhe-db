# 衝突有無で応答時間分布を比較するベンチハーネス

Issue #2079（親 #2078。ポインタ: RLS-9・RLS-10）。他テナントにだけ UNIQUE 値・FK 親がある状態と、
どこにも無い状態で、同一の書き込み文の応答時間分布を比較するための計測入口を記録する。
応答内容の不変性は `crates/engine/tests/rls10_write_constraint_paths.rs` の T4 が固定しており、
本ハーネスはその対象外だったレイテンシ分布の計測を担う。

Issue #2080 で、判定基準（下記「判定基準」）・実測値の記録（「実測記録」）・`bench.yml` への配線
（「bench.yml での実行」）を追記した。判定結果は参考値で、ハーネスの終了コードには反映しない。

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
| `BENCH_RLS_TIMING_REJUDGE` | 未設定 | `raw.json` のパス。指定すると計測せず、読み戻した生サンプルから再判定だけ行う（下記） |

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

## 判定基準（Issue #2080）

本リポで決めた公開値であり、spec 由来の閾値ではない（ポインタ: RLS-9・RLS-10）。考え方と先例は
`benchmark-judgement-policy.md` §4（実測帯 = 参照区間の run-to-run 全幅）・Issue #401・Issue #738
（`wire_tenant_row_id_scope.rs` の `judge`）を引き継ぐ。実装は `harness::rls_timing::judge_rows`。

形状ごと・分位点 q（p50・p95・p99。nearest-rank）ごとに次を求める。

| 量 | 定義 |
| -- | ---- |
| `delta(q)` | baseline と flooded の**全ラウンドをプールした**分位点の対称相対差 `abs(a-b) / max(a,b)`（両方 0 なら 0） |
| `aa(q)` | 同一 arm の**ラウンド別**分位点の全幅 `(max-min) / min`。baseline・flooded それぞれで求め大きい方を取る。最小値が 0 なら無限大 |
| 固定相対帯 | p50 = 0.05、p95・p99 = 0.10 |
| A/A 帯上限 | p50 = 0.25、p95・p99 = 0.50 |

分位点ごとの判定（上から順に評価）:

1. `aa(q)` が A/A 帯上限を超える: **判定不能**（同一 arm 内のノイズが大きく、差を判定に使えない）
2. `delta(q)` が `max(固定相対帯, aa(q))` を**厳密に超える**: **区別可能**（= 分位点の相対差の上限を超えた）
3. それ以外: **区別不能**

したがって「分位点の相対差の上限」は `max(固定相対帯, aa(q))` である。形状ごとの結果は
判定不能 > 区別可能 > 区別不能の優先で 1 つにまとめ、**全形状が区別不能のときだけ全体が pass**
になる。判定不能を含めば fail（判定不能）、区別可能を含めば fail。形状が 1 つも無い場合も pass にしない。

既知の制約:

- 判定は 3 分位点だけを見る。分位点の間に現れる部分母集団の差は捉えない（全順序統計量の走査は
  Issue #738 が wire 経路の INSERT 側で別途担っている）
- 2 arm 比較であり、差が出たときの原因（衝突の有無かデータ量か）は切り分けない（次節）
- 共有環境の値は採否の根拠にしない（`benchmark-judgement-policy.md` §5）。ハーネスは常に
  参考値であることを出力に残し、判定結果を終了コードに反映しない

## 既知の交絡（#2080 で 2 arm のまま判定すると決めた）

flooded は「衝突」と「他テナントのデータ量」の両方が baseline と異なる（ポインタ: RLS-10）。
他テナントの行の存在・件数の非観測を確かめる目的では、衝突・データ量のどちらに起因しても
2 arm の差は問題になりうるため、2 arm の比較をそのまま判定対象にする。
同行数・非衝突の対照 arm は、差が出たときの原因切り分け（診断）にしか効かず pass／fail を変えない。
よって本 Issue では対照 arm を追加せず、診断用の後続候補として申し送る。

## 生 JSON からの再判定

`BENCH_RLS_TIMING_REJUDGE=<raw.json のパス>` を指定すると、計測せず `raw.json` から `Round` 列を復元して
要約と判定を出力する（相対パスはリポジトリルート基準。`BENCH_RLS_TIMING_OUT_DIR` と同じ）。
docs の数値は、この再判定の出力から転記する。入力は信頼せず fail-closed で検証し、次のいずれも
非 0 終了する。

- ファイルが 4 MiB を超える（読み込み前に検査。`harness::chip::parse_json` の上限と同じ）、または JSON として不正
- `schema_version` が 1 でない、`quantile_method` が `nearest_rank` でない、必須キーの欠落
- 未知の形状／arm、`config.shapes` の重複、`config` に無い形状のセル
- `samples_ns` が空、または非整数・負数・非数値を含む、`mismatches` が 0 でない
- ラウンド番号の重複、ラウンド内のセルの重複、(形状, arm) のセル欠落

`GITHUB_ACTIONS` 下では拒否する（CI は任意パスの読み込みを持ち込まない）。

## bench.yml での実行

`.github/workflows/bench.yml` の `bench-rls-timing` ジョブが `make bench-rls-timing`（ROUNDS・ITERS は既定の
5・1000）を `schedule`（毎週月曜 03:00 UTC）と `workflow_dispatch` で実行する。

- **既定ゲートに含めない**: 判定結果は終了コードに反映しない。GitHub ホステッド runner は共有環境で
  あり、`BENCH_DEDICATED_ENV` は注入しない。値は Actions ログに判定行として残る
- **正しさの失敗は red**: 事前検査の失敗・2 arm の応答不一致（RLS-10 の応答面の違反）・計測中の期待外応答は
  非 0 終了でジョブが落ちる。これを隠さないため `continue-on-error` は使わない
- **secrets を使わない**ため `environment: bench-gate` は付けない。判定基準は本リポで決めた公開値で、
  実測値の公開は `spec-confidentiality` の公開可の範囲にあたる。`main` 限定の `if` と `ref: main` は維持する
- 実所要時間は main へのマージ後に `workflow_dispatch` で測る（`if` が `main` 限定のため PR 上では動かない）

## 実測記録（Issue #2080）

- 計測日: 2026-11-05。計測コミット: `c7a7c8f42764ce66f2926bfdbe798d26f9855e20`（`rls_timing_bench` の計測経路は
  その後の再判定パス修正と同一）
- 環境: Linux x86_64、13th Gen Intel Core i7-13700K（KVM 仮想化）、論理 10 コア、ISA `Avx2Fma`。
  **共有環境**: 計測前後の loadavg は約 10〜11（他プロセスが同時に CPU を使用）。専有環境ではない
- 設定: ROUNDS = 5、ITERS = 1000（warmup 20）、シード 2079、全 3 形状。1 (形状, arm) あたり 5000 サンプル
- 生 JSON: `docs/design/bench-data/rls-timing-20261105/raw.json`（`BENCH_RLS_TIMING_REJUDGE` で再判定し、
  計測時の出力と一致することを確認済み）

プールした分位点（µs）:

| 形状 | arm | p50 | p95 | p99 |
| ---- | --- | --- | --- | --- |
| `unique_violation` | baseline | 21.1 | 22.5 | 30.4 |
| `unique_violation` | flooded | 26.2 | 28.5 | 35.9 |
| `fk_missing_parent` | baseline | 21.2 | 23.4 | 31.0 |
| `fk_missing_parent` | flooded | 23.8 | 25.7 | 33.6 |
| `normal` | baseline | 51.3 | 59.7 | 67.3 |
| `normal` | flooded | 57.2 | 66.7 | 74.3 |

判定:

| 形状 | q | delta | aa | 上限 `max(固定帯, aa)` | 結果 |
| ---- | - | ----- | -- | ---------------------- | ---- |
| `unique_violation` | p50 | 0.197 | 0.058 | 0.058 | 区別可能 |
| `unique_violation` | p95 | 0.210 | 0.063 | 0.100 | 区別可能 |
| `unique_violation` | p99 | 0.154 | 0.064 | 0.100 | 区別可能 |
| `fk_missing_parent` | p50 | 0.108 | 0.038 | 0.050 | 区別可能 |
| `fk_missing_parent` | p95 | 0.087 | 0.204 | 0.204 | 区別不能 |
| `fk_missing_parent` | p99 | 0.075 | 0.070 | 0.100 | 区別不能 |
| `normal` | p50 | 0.104 | 0.009 | 0.050 | 区別可能 |
| `normal` | p95 | 0.106 | 0.029 | 0.100 | 区別可能 |
| `normal` | p99 | 0.095 | 0.165 | 0.165 | 区別不能 |

**全体: fail**（区別可能が 6 件。判定不能は無い）。判定を緩めて pass にはしていない。

解釈と申し送り:

- 衝突の無い `normal` 形状でも flooded が約 10% 遅く、全形状で flooded が baseline より遅い。この
  計測は 2 arm 比較のため、差が衝突に起因するのかデータ量（flooded は他 2 テナントの行を持つ）に
  起因するのかは分離できていない（前節。対照 arm は診断用の後続候補）
- 共有環境（loadavg 約 10 の 10 コア機）の値であり、**採否の根拠にしない**（`benchmark-judgement-policy.md` §5）。
  この fail は「専有環境での再測定が要る」ことを示す参考値である
- 要対応（オーナー判断）: 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測と、その結果に基づく
  RLS-9・RLS-10 の spec 側の扱いの確認。差の原因切り分け用の対照 arm の追加
