# 回帰ベンチ・Recall ゲートの閾値注入: 設計判断記録

- ステータス: Proposed（本コミットは secrets ↔ spec ポインタの対応整理・設定手順の
  確立のみ。実際の `gh secret set`・`workflow_dispatch` による疎通確認はマージ後の
  リポジトリ管理者作業。実施後、別コミットで Accepted に更新する）
- 対応: Issue #286（`ci: 回帰ベンチ・Recall ゲートの閾値注入を variables から secrets へ移行`）。
  PR #299 の codex-review P0 指摘を受け、`bench.yml` の閾値 secrets は repo レベルから
  Environment `bench-gate`（`recall-gate` と同じく main 限定 deployment branch policy 付き）
  へ再移設した（下記「Environment `bench-gate` secrets」参照）
- 前提: TASK-127・TASK-130・TASK-83（`.github/workflows/bench.yml`）、
  TASK-104・TASK-108・TASK-112・TASK-113（`.github/workflows/recall.yml`）。既存 ADR
  （`hybrid-recall-regression.md`・`rerank-recall-regression.md`・
  `query-planning-recall-regression.md`・`core5-contrast-engine.md`）はいずれも
  「Actions variables／secrets の実値設定はマージ後のリポジトリ管理者作業」と明記して
  おり、本 ADR はその作業の実行手順・対応表を一箇所に集約するもの

## 目的

`bench.yml`・`recall.yml` は spec 由来の数値基準を GitHub Actions の repo
レベル／Environment `recall-gate` の値付き変数から注入する設計で、未設定時は
fail-closed で red になる。

当初は Actions variables（`vars.*`）から注入していたが、2026-08-29 の
`workflow_dispatch` 実行で、`run` ステップに渡す `env:` ブロックが GitHub Actions
のログへ**値付きでそのまま出力される**ことを確認した。これにより repo/environment
variables に設定した閾値（spec 由来の非公開数値）が public な Actions ログへ
そのまま印字されてしまう（`.claude/rules/spec-confidentiality.md` P0 違反）。
当該 run のログは削除済み。対策として、値が自動的に `***` へマスクされる
**secrets**（repo secrets／Environment `recall-gate` の secrets）へ参照を切り替えた
（本コミット）。

本 ADR は secrets 名と spec ポインタ・受理形式（本リポの Rust 実装が検証する
範囲）の対応を一箇所にまとめ、値そのもの（spec 由来の数値）を一切含まない形で
設定手順を確立する。**実際の値の設定・`workflow_dispatch` による疎通確認は
本コミットのスコープ外**（下記「申し送り」参照。GitHub 設定変更は他の並列作業と
共有される repository-wide な状態変更であり、レビュー未了の変更として実行しない）。

**追記（PR #299 codex-review P0 指摘）**: `bench.yml` を repo レベルの secrets へ
移行した直後、`workflow_dispatch` は任意の ref を選んで起動でき、選択した ref の
`run` ステップがそのまま実行される点を突かれると、write 権限者が別ブランチで
`run` ステップを書き換えて `workflow_dispatch` した場合に repo レベルの secrets を
任意の処理（外部送信を含む）へ渡せてしまう、との指摘を受けた。secrets のログ
マスクは Actions のログ出力にのみ効くもので、書き換えられた `run` ステップが
secrets を別経路へ渡すこと自体は防がない。`recall.yml` は元々 Environment
`recall-gate`（main 限定 deployment branch policy）でこの経路を塞いでいたため、
`bench.yml` も同じ形へ揃えた: Environment `bench-gate`（main 限定）を新設し、
`bench.yml` の閾値 secrets を使う全 job（`bench-simd`・`bench-contrast`・
`bench-batch`・`bench-c1`）に `environment: bench-gate` を指定した。main 以外の
ref から `workflow_dispatch` した run は Environment `bench-gate` にアクセス
できないため、閾値 secrets を取得できない。

## secret ↔ spec ポインタ対応（値は書かない）

値そのもの（spec 由来の数値基準）は `.claude/rules/spec-confidentiality.md`
（本リポ public・spec は private）により本 ADR を含むいかなる public 資産にも
記載しない。設定時は `docs/spec` の該当ビヘイビア ID の記述を直接参照すること。

### Environment `bench-gate` secrets（`.github/workflows/bench.yml`）

repo レベルの secrets ではなく Environment `bench-gate`（deployment branch
policy で `main` のみに制限）に置く（上記「追記」参照。`.github/workflows/
recall.yml` が `recall-gate` で採用済みの実行境界と同一方針）。

| secret | spec ポインタ | 受理形式（実装の検証規則） |
| -------- | -------------- | -------------------------- |
| `BENCH_MAX_P95_MS` | `docs/spec/04-behavior/core-engine.md` CORE-3・`search.md` SEARCH-4（TASK-127） | 正の整数（ms） |
| `BENCH_MIN_RECALL` | `core-engine.md` CORE-4（TASK-127） | `(0.0, 1.0]` |
| `BENCH_MAX_CONTRAST_RATIO` | `core-engine.md` CORE-5（TASK-127。`harness/accept.rs::parse_contrast_ratio_limit`） | 有限・正の浮動小数点 |
| `BENCH_BATCH_MAX_DEGRADATION_PCT` | `core-engine.md` CORE-7 改訂＋`05-tasks.md` TASK-130（動的窓の単発 p95 劣化上限） | 0 以上の有限浮動小数点（%） |
| `BENCH_SQL_C1_MAX_P95_MS` | `sql-surface.md` SQL-1（TASK-83） | 正の整数（ms） |
| `BENCH_SQL_C1_MIN_RECALL` | `sql-surface.md` SQL-1（TASK-83。参照実装との Top-20 一致率） | `(0.0, 1.0]` |
| `BENCH_CORE6_MIN_IMPROVEMENT_PCT`（任意） | `core-engine.md` CORE-6（TASK-130） | 正の浮動小数点（%）。`BENCH_CORE6` opt-in 時のみ読まれる |
| `BENCH_CORE16_MIN_IMPROVEMENT_PCT`（任意） | `core-engine.md` CORE-16（TASK-130） | 正の浮動小数点（%）。`BENCH_CORE16` opt-in 時のみ読まれる |

上記のうち `bench-simd`・`bench-contrast`・`bench-batch` の 3 job は
schedule／workflow_dispatch の両方で実行され、`BENCH_MAX_P95_MS`・
`BENCH_MIN_RECALL`・`BENCH_MAX_CONTRAST_RATIO`・`BENCH_BATCH_MAX_DEGRADATION_PCT`
の 4 secrets が揃わない限り fail-closed で red のままとなる（週次 schedule を
green にするための必須 secrets はこの 4 つ）。

一方 `bench-c1` job は `workflow_dispatch` 限定（`if: github.event_name ==
'workflow_dispatch' && github.ref == 'refs/heads/main'`）で **schedule には
含まれない**。`BENCH_SQL_C1_MAX_P95_MS`・
`BENCH_SQL_C1_MIN_RECALL` の 2 secrets は `workflow_dispatch` で `bench-c1` を
明示的に起動したときにのみ必要であり、週次 schedule の red 状態には無関係
（未設定でも schedule は red のまま変化しない）。

`BENCH_CORE6`・`BENCH_CORE16`・`BENCH_DEDICATED_ENV`（opt-in フラグ・専有環境
フラグ）は数値基準ではなく非機密の 0/1 フラグのため、secrets 化の対象外とし
repo variables（`vars.*`）のまま維持する（`.github/workflows/bench.yml` 参照）。
`BENCH_CORE16_DIAG`（Issue #313・CORE-16 の Apple GPU〔Metal〕fail 切り分け用
規模スイープ診断の opt-in フラグ。`docs/design/core16-f16-resident-gate.md`
参照）も同様に非機密であり、`bench.yml` へは注入しない（`BENCH_VERBOSE` と
同じく手動実行〔`make bench-batch`〕専用の opt-in とし CI では設定しない）。

`RECALL_ENGINE`（Issue #412・ADR `docs/design/ann-index-adoption.md` B 案の
受け入れ条件 3。`crates/engine/tests/fixtures/recall_engine.rs::RecallEngine`）
も同様に非機密の opt-in フラグ（値は `brute_force`／`hnsw`／`hnsw_f16`
〔Issue #515。HNSW 索引ノードの f16 常駐 opt-in・Issue #514〕／`hnsw_i8`
〔Issue #523。HNSW 索引ノードの I8（SQ8）常駐 opt-in・Issue #521・#522〕の
4 択で数値基準を含まない）であり、`BENCH_CORE6`／`BENCH_CORE16` と同じ
扱いで secrets 化しない。`recall.yml` は `strategy.matrix.recall_engine`
（`[brute_force, hnsw, hnsw_f16, hnsw_i8]`）で job を 4 系列に固定し、
各 job が `matrix.recall_engine` を 3 gate step の `env:` へそのまま渡す。
`workflow_dispatch`・`schedule` いずれのトリガでも全系列の job が毎回実行
される（旧来の `workflow_dispatch.inputs.recall_engine` は撤去済み。
Issue #412 PR #438）。

### Environment `recall-gate` secrets（`.github/workflows/recall.yml`）

| secret | spec ポインタ | 受理形式 |
| -------- | -------------- | -------- |
| `HYBRID_RECALL_MIN_R20_SMALL` | `search.md` SEARCH-1（TASK-104） | `(0.0, 1.0]` |
| `HYBRID_RECALL_MIN_R20_LARGE` | `search.md` SEARCH-2（TASK-104。クエリ展開あり・決定的スタブ。Issue #306） | `(0.0, 1.0]` |
| `HYBRID_RECALL_MIN_R100_LARGE` | `search.md` SEARCH-2（TASK-104。クエリ展開あり・決定的スタブ。Issue #306） | `(0.0, 1.0]` |
| `RERANK_RECALL_MIN_R20_LARGE` | `search.md` SEARCH-7（TASK-108。絶対下限。非劣化 `after_hits20 >= baseline_hits20` とあわせてブロッキング条件） | `(0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT` | `query-planning.md` PLAN-1（TASK-112。改善幅。pt → 小数換算） | `[0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_R20_DIRECT` | `query-planning.md` PLAN-2（TASK-112） | `(0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE` | `query-planning.md` PLAN-3 → `search.md` SEARCH-2 のスケール条件付き基準（TASK-113） | `(0.0, 1.0]` |
| `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED` | spec に対応値なし（本リポ独自の劣化展開シナリオ用回帰基準。`docs/design/query-planning-recall-regression.md` 参照） | `[0.0, 1.0]` |

`DEGRADED` を含む全 8 secrets が揃わない限り `recall-regression` job は
strict モード（`*_REQUIRE_THRESHOLDS=1`）により fail-closed で red のままとなる
（`crates/engine/tests/query_planning_recall.rs::resolve_gate_threshold_with` は
strict モード時、`DEGRADED` を含め未設定の変数を検出した時点で `panic!` する）。
そのため **オーナーは `DEGRADED` の採用可否によらず、strict モードの必須条件
として全 8 secrets の値を確定・設定する**（下記「設定手順」参照）。`DEGRADED` の
副検査を他 7 secrets の回帰検知から独立させたい場合（別 job・非 strict 経路への
分離等）は本 ADR のスコープ外とし、別 Issue で扱う。`RERANK_RECALL_MIN_R20_
IMPROVEMENT` は SEARCH-7 改訂（2026-08-31・fandhe-db-spec#8）で判定から除外
された（`recall.yml` は不読・`rerank_recall.rs` は同名変数を参照しない）ため
この一覧・strict モードの必須条件からは対象外である。過去に設定していた場合は
リポジトリ管理者が `gh secret delete RERANK_RECALL_MIN_R20_IMPROVEMENT --env
recall-gate` で削除してよい（削除しなくても未読のため red 化はしない）。

## 意図的に据え置く事項

- **`PRECISION_EVAL_MIN_TOP1_ACC` / `PRECISION_EVAL_MIN_MRR10` /
  `PRECISION_EVAL_MAX_FALSE_RETURN`（TASK-163・SEARCH-10）は設定しない**。
  spec（SEARCH-10）は目標値未確定の間はこれらを必達基準に含めない方針であり、
  確定はユーザー確認を要するとしている。`docs/design/precision-eval-regression.md`・
  README も同じ申し送りを持つ。仮置き値を週次 strict ゲートへ入れることは
  spec の申し送りと矛盾するため、
  安全側（設定しない・`recall.yml` へ未接続のまま）に倒す。
- **TASK-116（ティア別レイテンシ・PLAN-4/6/7）は対象外**。常駐 Ollama が必要で
  GitHub ホステッド runner に CI 経路が無く、repo variables／secrets でもない
  （`BENCH_TIER_*` は `make bench-tier` の実行時 env）。
  `docs/design/tier-latency-acceptance.md` の目標値確定はオーナーが承認済み計測環境で実施する。
- **`BENCH_CORE6` / `BENCH_CORE16` の opt-in フラグは有効化しない**。GitHub
  ホステッド runner に GPU が無く、有効化すると必ず `pass=false` で red になる。
  下限値の 2 secrets（`BENCH_CORE6_MIN_IMPROVEMENT_PCT` / `BENCH_CORE16_MIN_IMPROVEMENT_PCT`）
  は opt-in しない限り読まれないため、先行設定するかは管理者判断に委ねる。
- **`bench-c1` の Conditional Go 条件7 判定（`BENCH_DEDICATED_ENV=1`）は有効化
  しない**。GitHub ホステッド runner は専有環境の自己申告に該当せず、
  `docs/design/c1-p95-dedicated-env-reverification.md` の既定方針どおり運用者が
  専有環境で直接実行する。

## 設定手順（マージ後・リポジトリ管理者作業）

値をコマンドライン引数・シェル履歴・ファイル（リポジトリ配下）に残さないため、
対話シェルの `read -rs`（非表示入力・シェル履歴に残らない）で値を変数へ読み込み、
その変数を stdin 経由で `gh secret set` に渡す（`gh secret set --help` の
「標準入力から読む」形。`printf "$value" | gh secret set NAME` は
`value` が展開された時点でコマンドライン全体が一時的にシェル履歴・`ps` に
現れうるため使わない）。1 secret ずつ次を実行する（`NAME` を対象の secret 名に
置き換える）。

```bash
# Environment bench-gate secrets（bench.yml）: --env bench-gate を付ける
read -rs value && printf '%s' "$value" | gh secret set NAME --env bench-gate && unset value

# Environment recall-gate secrets（recall.yml）: --env recall-gate を付ける
# DEGRADED を含む全 8 secrets を、DEGRADED の採用可否によらず設定する
read -rs value && printf '%s' "$value" | gh secret set NAME --env recall-gate && unset value

gh secret list --env bench-gate
gh secret list --env recall-gate
```

対象の secret 名（environment `bench-gate` secrets）:

- `BENCH_MAX_P95_MS`
- `BENCH_MIN_RECALL`
- `BENCH_MAX_CONTRAST_RATIO`
- `BENCH_BATCH_MAX_DEGRADATION_PCT`
- `BENCH_SQL_C1_MAX_P95_MS`
- `BENCH_SQL_C1_MIN_RECALL`
- `BENCH_CORE6_MIN_IMPROVEMENT_PCT`（任意）
- `BENCH_CORE16_MIN_IMPROVEMENT_PCT`（任意）

対象の secret 名（environment `recall-gate` secrets）:

- `HYBRID_RECALL_MIN_R20_SMALL`
- `HYBRID_RECALL_MIN_R20_LARGE`
- `HYBRID_RECALL_MIN_R100_LARGE`
- `RERANK_RECALL_MIN_R20_LARGE`
- `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`
- `QUERY_PLANNING_RECALL_MIN_R20_DIRECT`
- `QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE`
- `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED`（strict モードの必須条件として設定する）

既存の repo secrets（`secrets.BENCH_MAX_P95_MS` 等。本 PR の前段で repo レベルに
設定していたもの）・repo variables（`vars.BENCH_MAX_P95_MS` 等）・Environment
`recall-gate` の variables（`vars.HYBRID_RECALL_MIN_*` 等）を過去に設定していた
場合は、上記の同名 Environment secrets への設定完了後に `gh secret delete NAME`
（repo secrets）・`gh variable delete NAME`（repo variables。environment 側は
`--env recall-gate` を付ける）で削除し、二重管理・古い値の混在を避ける。特に
repo レベルの `BENCH_*` secrets は本 ADR の前段で一時的に設定したものであり、
Environment `bench-gate` への移行後は速やかに削除する（repo レベルに残したまま
だと codex-review P0 指摘の書き換え経路が再び成立するため）。

設定後は必ず以下を確認する:

1. `gh api repos/Fandhe-AI/fandhe-db/environments/bench-gate/deployment-branch-policies`
   / `gh api repos/Fandhe-AI/fandhe-db/environments/recall-gate/deployment-branch-policies`
   で branch policy が `main` のみに制限されたままであること
   （`hybrid-recall-regression.md` の実行境界設計を参照）
2. `gh workflow run bench.yml --ref main` / `gh workflow run recall.yml --ref main`
   を実行し、`gh run view <id>` で各 job・step が **skip ではなく実行**され、
   strict モード（`*_REQUIRE_THRESHOLDS=1`。recall.yml のみ）で pass したことを
   `pass=`/`pass_*=` 行（非数値の判定結果のみ）で確認する。閾値未達（red）の
   場合は spec 値を変更せず、pass/fail の状態のみを記録する（fail-closed を
   維持する）。ログ中の secrets の値は GitHub Actions により自動的に `***` へ
   マスクされる（`env:` ブロックが値付きで印字されていた repo variables 時代の
   問題はこの副作用として解消される）。`recall.yml` は 3 つの gate step
   （`Run recall-regression` / `Run rerank-regression` /
   `Run query-planning-regression`）を独立に実行し、job の最終判定は末尾の
   `Evaluate recall gates` step が 3 step の `outcome` を AND 集約して行う
   （Issue #311・`docs/design/recall-gate-independent-evaluation.md`）。
   `Evaluate recall gates` step の `gate=<name> outcome=<...>` 行と最終
   `pass=true|false` 行で、3 ゲートそれぞれの pass/fail を（数値なしで）
   確認し、#301 に記録する
3. 別ブランチ（`main` 以外）から `bench.yml`/`recall.yml` を `workflow_dispatch`
   した場合、`environment: bench-gate`/`environment: recall-gate` を指定した
   job が Environment 保護により実行拒否される（またはスキップされる）ことを
   確認する（codex-review P0 指摘で問題になった経路が塞がれていることの確認）
4. ログ全文（`gh run view --log`）は保存・転記しない。閾値・実測値を public
   資産（PR・commit・docs・Issue）へ書かない

## 申し送り（本コミットのスコープ外）

- Environment `bench-gate` secrets（6 件必須＋2 件任意）の実値設定
- Environment `recall-gate` secrets（`DEGRADED` を含む全 8 件）の実値設定
- 旧 repo secrets（`BENCH_*`。Environment `bench-gate` 移行前に設定していた場合）・
  旧 repo variables／Environment `recall-gate` variables に実値が設定済みだった
  場合の削除（上記「設定手順」参照）
- `workflow_dispatch` による strict モード疎通確認と run の記録（main 以外の ref
  からの `workflow_dispatch` が Environment 保護で拒否されることの確認を含む）
- `PRECISION_EVAL_*`（TASK-163・SEARCH-10）目標値確定と `recall.yml` への接続
- TASK-116（PLAN-4/6/7）の `make bench-tier` 実測と ADR の Accepted 化
- `BENCH_CORE6` / `BENCH_CORE16` の GPU 搭載ホストでの opt-in 有効化
- `bench-c1`（TASK-83 条件7）の専有環境での最終判定
- Issue #311（`recall.yml` の 3 ゲート独立評価・AND 集約）のマージ後
  `workflow_dispatch` 疎通確認と、3 ゲートそれぞれの pass/fail の #301 への記録
  （`docs/design/recall-gate-independent-evaluation.md` 参照）
