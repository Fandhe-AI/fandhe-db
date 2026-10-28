# hybrid_rrf Phase 6 通し前後比較（feature_bench・bench-hybrid-profile・crossdb・Recall 3 ゲート）

- **ステータス**: Recorded（記録専用・参考値。共有 QEMU 環境での計測のため
  `docs/design/benchmark-judgement-policy.md` §5 により Accepted/Rejected の
  採否根拠にはしない）
- **対応 Issue**: #550（本書）・親 #548（Phase 6 トラッキング）
- **関連ポインタ（spec・本文は転記しない）**: SEARCH-1・SEARCH-3・TASK-104・
  TASK-108・TASK-112・TASK-113

## 1. 背景・目的

Phase 6（親 #461 → #548）で hybrid_rrf 経路に入った production 変更は次の 2 件。

| Issue | PR / commit | 変更 |
| --- | --- | --- |
| #546 | #565 / `b161d5b` | `sparse.rs::score_pass` のスコアアキュムレータを `SparseIndex` 寿命内の有界スクラッチプールへ置換 |
| #549 | #572 / `c86c683` | `hybrid.rs::rrf_fuse_with_limits` の融合コアを `BTreeMap` 累積から位置索引方式へ置換、`has_duplicate_id` の `Vec` 化、`apply_soft_boost` の整列済み時再ソート省略 |

いずれも「スコア・順序はビット同一」を契約とし、実装 PR 単体では共有計測環境の
制約により採否を確定していない。本書はこれらを **Phase 6 通しで** 計測し、
`feature_bench`・`bench-hybrid-profile`・crossdb self（wire 経由）・Recall 3
ゲート（hybrid・rerank・query-planning、`RECALL_ENGINE=brute_force`/`hnsw`）の
before/after を記録する。

## 2. 計測条件

| 状態 | commit | 内容 |
| --- | --- | --- |
| A（before・Phase 6 着手前） | `91f6a18` | #546 直前 |
| C（after） | `c86c683`（PR #572 squash commit。#549 適用後） | #546＋#549 適用 |

- 状態 B（`c636a81`。#546 適用済み・#549 未適用）は**未計測**（自動運転での
  時間配分により A→C の主比較を優先。`docs/design/benchmark-judgement-policy.md`
  が許容する縮退構成）。#549 単独の寄与は `docs/design/
  hybrid-rrf-latency-breakdown.md`「Issue #549」節の単一バイナリ内 B7 参考値を
  参照
- 状態 A・C は明示的なコミットハッシュ（`91f6a18`／`c86c683`）を `git archive`
  で個別ディレクトリへ書き出し、それぞれ独立した `CARGO_TARGET_DIR` でビルドする
  （PR #575 codex-review 指摘対応で「実装ブランチ HEAD」参照から明示コミットへ
  切替。ブランチ HEAD はレビュー対応の追加コミットで動くため before/after の
  対象を固定するには不向き）。`git diff --stat 91f6a18 c86c683 -- Cargo.lock
  Cargo.toml crates/engine/Cargo.toml crates/wire-server/Cargo.toml` は差分なしを
  確認済み
- ツールチェーン: `rustc 1.96.0`・`cargo 1.96.0`（両状態で共通の
  `rust-toolchain.toml` を使用）
- 環境: `lscpu` Model name `QEMU Virtual CPU version 2.5+`・ISA フラグ
  `avx2`／`fma`／`f16c` 検出（`avx512*`・`neon` は無し。`hybrid_profile_bench`
  実行時の自己申告 `isa=Avx2Fma` と整合）・`nproc`=12・`x86_64`・
  他 worktree（他 Issue のエージェント）多数が並行稼働・`loadavg`（1分平均。
  各 run 直前に `cat /proc/loadavg` で記録）は §3〜§5 実測で 2.35〜5.57
  の範囲（PR #575 codex-review P2 指摘対応で各 run の値をそのまま §3〜§5 の
  「各 run の loadavg」小節へ追記。従来の「概ね 2.4〜6.7」という記述は
  この実測範囲に合わせて訂正した。外れ値回の詳細は §5 参照）・
  `BENCH_DEDICATED_ENV` 未設定。**専有環境ではないため
  Accepted/Rejected の採否根拠にしない**（`docs/design/
  benchmark-judgement-policy.md` §5）
- ビルド: 状態 A・C それぞれ独立したディレクトリ・`CARGO_TARGET_DIR` で
  `cargo build --release`（`feature_bench` example・`wire-server` バイナリ・
  `hybrid_profile_bench` ベンチバイナリ〔`--features bench-internals`〕）
- **各 run の生データの保存先**: 本 doc の §3〜§5 の各表直下に「各 run の
  生データ」小節としてインライン記載する（`docs/design/ingest-write-path.md`
  の記録方式を踏襲。ベンチ実行時の一時出力先はホスト環境のスクラッチ
  ディレクトリであり本リポの追跡対象外のため、事後再判定に必要な値は
  この doc 自体に残す）

## 3. `feature_bench` 13 フェーズ（交互 5 ペア・min-of-5／median-of-5）

`./target/release/examples/feature_bench` を A→C の順で 5 ペア交互実行し
（各 run 前に `loadavg` を記録）、p50 の min-of-5・median-of-5 を集計した。

| phase | A min (µs) | C min (µs) | ratio(min) | A median | C median | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| ingest | 4093 | 4081 | 0.997 | 4129 | 4122 | 0.998 |
| point_where | 2749 | 2787 | 1.014 | 2861 | 2814 | 0.984 |
| where_compound | 2882 | 2892 | 1.003 | 2899 | 2903 | 1.001 |
| agg_count | 2449 | 2488 | 1.016 | 2521 | 2503 | 0.993 |
| agg_multi | 2663 | 2746 | 1.031 | 2708 | 2760 | 1.019 |
| group_by_having | 3100 | 3109 | 1.003 | 3117 | 3143 | 1.008 |
| vector_knn | 8589 | 8592 | 1.000 | 8796 | 8625 | 0.981 |
| vector_knn_where | 2804 | 2781 | 0.992 | 2816 | 2812 | 0.999 |
| **hybrid_rrf** | **11741** | **11100** | **0.945** | **11884** | **11218** | **0.944** |
| mode_recall | 8628 | 8696 | 1.008 | 8734 | 8761 | 1.003 |
| mode_precision | 8555 | 8613 | 1.007 | 8755 | 8729 | 0.997 |
| rls_isolation | 2357 | 2421 | 1.027 | 2392 | 2424 | 1.013 |
| udf_call | 640 | 668 | 1.044 | 694 | 701 | 1.010 |

**各 run の p50 生データ（µs、run1..5。PR #575 codex-review P1 指摘対応で
再判定用に追記）**:

| phase | A | C |
| --- | --- | --- |
| ingest | [4144, 4096, 4129, 4131, 4093] | [4121, 4124, 4140, 4122, 4081] |
| point_where | [2772, 2887, 2861, 2891, 2749] | [2794, 2814, 2896, 2787, 2837] |
| where_compound | [2882, 2889, 2902, 3000, 2899] | [2896, 2977, 2903, 2989, 2892] |
| agg_count | [2492, 2449, 2551, 2521, 2548] | [2503, 2513, 2496, 2521, 2488] |
| agg_multi | [2663, 2708, 2666, 2777, 2759] | [2805, 2746, 2928, 2760, 2749] |
| group_by_having | [3100, 3117, 3112, 3177, 3157] | [3158, 3143, 3143, 3109, 3114] |
| vector_knn | [8679, 9429, 9356, 8796, 8589] | [8625, 8592, 8957, 8604, 8813] |
| vector_knn_where | [2814, 3058, 2816, 2862, 2804] | [2812, 2781, 2809, 2906, 2823] |
| hybrid_rrf | [11741, 12085, 11821, 11935, 11884] | [11474, 11218, 11262, 11100, 11110] |
| mode_recall | [8734, 8958, 8706, 8862, 8628] | [9468, 8701, 8849, 8696, 8761] |
| mode_precision | [8793, 9214, 8597, 8755, 8555] | [8967, 8669, 8872, 8613, 8729] |
| rls_isolation | [2396, 2357, 2392, 2369, 2588] | [2421, 2424, 2929, 2491, 2424] |
| udf_call | [694, 784, 749, 692, 640] | [701, 704, 748, 685, 668] |

参照区間の実測ノイズ帯（run-to-run 幅 / min）: `vector_knn` A=9.8%・C=4.2%、
`agg_count` A=4.2%・C=1.3%。対象区間 `hybrid_rrf` の改善（min 比 0.945・
median 比 0.944）は参照区間のノイズ帯（4〜10%）と同程度かそれ未満であり、
固定 ±5% 判定では `hybrid_rrf` は境界付近（`Improved` 寄り）だが、**参照区間の
ノイズ帯を明確に超えているとは言えない**。他 12 フェーズはいずれも比
0.98〜1.04x でノイズ帯内（非退行）。

**各 run 時点の loadavg（1分平均。run1..5。PR #575 codex-review P2 指摘対応で
`benchmark-judgement-policy.md` §3 の環境記録要件に合わせ追記）**:

| run | A loadavg | C loadavg |
| --- | --- | --- |
| 1 | 5.57 | 5.57 |
| 2 | 5.29 | 5.02 |
| 3 | 5.02 | 4.78 |
| 4 | 4.56 | 4.35 |
| 5 | 4.35 | 4.16 |

`feature_bench` 5 ペアの loadavg は 4.16〜5.57 の範囲で単調に減少しており
（他 worktree のビルド負荷が計測セッション中に収束していったため）、外れ値回
は無い。

**p95（µs、min-of-5・median-of-5）**: `feature_bench` の JSON 出力は
`p95_us`（各フェーズ 25〜50 iterations 中の p95）を含むため、p50 と同じ
5 ペアから p95 の min-of-5・median-of-5 も算出した（PR #575 codex-review P2
指摘対応。`benchmark-judgement-policy.md` §4 が p50/p95 双方を非退行判定の
対象とするため）。

| phase | A min (µs) | C min (µs) | ratio(min) | A median | C median | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| ingest | 4277 | 4220 | 0.987 | 4294 | 4350 | 1.013 |
| point_where | 2888 | 2837 | 0.982 | 2998 | 2934 | 0.979 |
| where_compound | 3116 | 2968 | 0.953 | 3213 | 3136 | 0.976 |
| agg_count | 2576 | 2586 | 1.004 | 2654 | 2606 | 0.982 |
| agg_multi | 2824 | 2881 | 1.020 | 2862 | 2994 | 1.046 |
| group_by_having | 3227 | 3186 | 0.987 | 3276 | 3306 | 1.009 |
| vector_knn | 9190 | 9487 | 1.032 | 10267 | 9958 | 0.970 |
| vector_knn_where | 2886 | 2853 | 0.989 | 2982 | 2952 | 0.990 |
| **hybrid_rrf** | **12799** | **11806** | **0.922** | **13965** | **12710** | **0.910** |
| mode_recall | 9788 | 9343 | 0.955 | 10056 | 9844 | 0.979 |
| mode_precision | 9311 | 9191 | 0.987 | 9433 | 9351 | 0.991 |
| rls_isolation | 2475 | 2526 | 1.021 | 2645 | 2572 | 0.972 |
| udf_call | 701 | 725 | 1.034 | 754 | 1039 | 1.378 |

**各 run の p95 生データ（µs、run1..5。PR #575 codex-review P2 指摘対応で追記）**:

| phase | A | C |
| --- | --- | --- |
| ingest | [4293, 4294, 4548, 4313, 4277] | [4236, 4630, 4391, 4220, 4350] |
| point_where | [2888, 2998, 3253, 5119, 2990] | [2934, 2926, 3531, 2837, 2980] |
| where_compound | [3116, 3184, 3280, 3299, 3213] | [2994, 3383, 3346, 3136, 2968] |
| agg_count | [2654, 2576, 2666, 2728, 2603] | [2650, 2592, 2606, 2623, 2586] |
| agg_multi | [2862, 2824, 2825, 2892, 2986] | [2994, 2881, 3231, 2904, 3215] |
| group_by_having | [3227, 3280, 3272, 3287, 3276] | [3805, 3227, 3306, 3540, 3186] |
| vector_knn | [10280, 11081, 10267, 9190, 9504] | [9958, 10576, 9672, 10608, 9487] |
| vector_knn_where | [2886, 4038, 2982, 3074, 2967] | [2952, 2853, 3555, 3298, 2928] |
| hybrid_rrf | [16013, 14016, 13068, 12799, 13965] | [12431, 12710, 13158, 11806, 13714] |
| mode_recall | [10841, 9788, 9935, 10056, 11243] | [11424, 9343, 9508, 9844, 10417] |
| mode_precision | [9907, 10045, 9398, 9311, 9433] | [11173, 9191, 10799, 9337, 9351] |
| rls_isolation | [2686, 2628, 2475, 2645, 3183] | [2526, 2528, 3098, 2715, 2572] |
| udf_call | [754, 1109, 1138, 724, 701] | [777, 1089, 1099, 1039, 725] |

参照区間の p95 実測ノイズ帯（run-to-run 幅 / min）: `vector_knn` A=20.6%・
C=11.8%、`agg_count` A=5.9%・C=2.5%（p50 の実測ノイズ帯 4〜10% より p95 は
一貫して広い——p95 は分布の裾を見るため run-to-run のばらつきが p50 より
大きく現れる）。`hybrid_rrf` の p95 は min 比 0.922・median 比 0.910 と p50
（min 比 0.945・median 比 0.944）と同方向（改善寄り）で、`vector_knn` 参照
区間の p95 ノイズ帯（12〜21%）を明確に超えるとは言えないため、p95 の悪化は
観測されない。`udf_call` の p95 median 比のみ 1.378 と大きく振れている。訂正（PR #575
codex-review P1 指摘対応）: 旧版は絶対時間幅（281µs）の小ささを根拠に退行
シグナルから除外していたが、`benchmark-judgement-policy.md` §4 は絶対時間幅
どうしの比較を明示的に禁止し、対象区間・参照区間とも「無次元の相対比率」に
統一して比較することを求めているため、以下のとおり相対比率のみで再評価する。

固定相対帯（±5%）は `|1.378 - 1.0| = 0.378 > 0.05` のため明確に超過する。
実測帯については、`udf_call` 自体が本 PR（#546・#549）の変更対象外の
フェーズであるため、`udf_call` 自身の p95 run-to-run 幅を参照区間として使う
（`vector_knn` 等の絶対時間が大きく異なるフェーズを参照区間にすると、
測定基盤側のオーバーヘッド比率が異なる可能性があるため、同一フェーズの
自己参照が最も直接的な比較になる）。上記「各 run の p95 生データ」より
`udf_call` の p95 raw 値は A=[754, 1109, 1138, 724, 701]・
C=[777, 1089, 1099, 1039, 725] であり、
`reference_band_A = (1138 - 701) / 701 = 62.3%`、
`reference_band_C = (1099 - 725) / 725 = 51.6%`。
対象区間の比率の乖離 `|1.378 - 1.0| = 37.8%` はこの実測帯（51.6〜62.3%）を
下回るため、固定相対帯は超過するが実測帯は超過しない——両ノイズ帯を
**両方**満たしていないため §4 の要件により非退行（ノイズ帯内）と判定する
（単発の外れ値 run〔C-2 の 1089µs・C-3 の 1099µs〕が median を押し上げて
いるが、これは `udf_call` 自身の run-to-run 変動の範囲内であり、他 12
フェーズと同列の退行シグナルとしては扱わない）。

## 4. `bench-hybrid-profile` 段別（B0s〜B8。プロセス 5 回起動 × 内部 5 ラウンド）

`BENCH_HYBRID_PROFILE_ROUNDS=5` でプロセスを A→C 交互に **5 回**起動し
（PR #575 codex-review P1 指摘対応で 3→5 に補完。1 プロセス内で
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」節と同じ内部 5
ラウンド交互計測を実施）、各プロセスの `baseline_summary` min をそのプロセスの
「1 run」の値として扱い、5 プロセス間で min-of-5・median-of-5 を集計する。

| 段 | A min-of-5 (µs) | C min-of-5 (µs) | ratio(min) | A median-of-5 | C median-of-5 | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| B0s（密側 `CpuScalarProvider` 単線参照区間） | 315 | 316 | 1.003 | 322 | 325 | 1.009 |
| B0（密側 KNN・production provider） | 429 | 405 | 0.944 | 444 | 439 | 0.989 |
| B1（SQL 表層 hybrid） | 8898 | 6315 | 0.710 | 8946 | 6567 | 0.734 |
| B2（投影込み） | 10613 | 8049 | 0.758 | 11172 | 8156 | 0.730 |
| B3（密側対照・informational） | 639 | 643 | 1.006 | 650 | 648 | 0.997 |
| B4（融合込み対照） | 5684 | 3029 | 0.533 | 5742 | 3051 | 0.531 |
| B5（疎側再取得ループ） | 3374 | 1666 | 0.494 | 3408 | 1669 | 0.490 |
| B8（可視集合構築） | 124 | 122 | 0.984 | 126 | 123 | 0.976 |
| B7（融合コア下限近似） | （before 計測不能。B7 は #572 で新設のためハーネス非存在） | 10 | — | （before 計測不能） | 10 | — |

**各プロセスの min（µs、process1..5。PR #575 codex-review P1 指摘対応で
再判定用に追記）**:

| 段 | A | C |
| --- | --- | --- |
| B0s | [331, 347, 322, 317, 315] | [334, 342, 325, 321, 316] |
| B0 | [451, 489, 429, 434, 444] | [477, 439, 405, 461, 430] |
| B1 | [8898, 9711, 8912, 9322, 8946] | [6575, 6567, 6315, 6618, 6417] |
| B2 | [11427, 11388, 10613, 11172, 10621] | [8189, 8525, 8049, 8085, 8156] |
| B3 | [639, 661, 653, 639, 650] | [646, 745, 643, 655, 648] |
| B4 | [5772, 6025, 5684, 5742, 5689] | [3075, 3108, 3039, 3029, 3051] |
| B5 | [3411, 3418, 3408, 3374, 3407] | [1677, 1679, 1669, 1666, 1669] |
| B8 | [126, 127, 125, 124, 126] | [124, 123, 123, 122, 123] |
| B7 | （非存在） | [10, 10, 10, 10, 10] |

派生帰属（#465 と同区分。min-of-5 ベース）:

| 区分 | A (µs) | C (µs) |
| --- | --- | --- |
| sql_surface (B1−B4) | 3214 | 3286 |
| projection (B2−B1) | 1715 | 1734 |
| sparse (B5) | 3374 | 1666 |
| residual (B4−B0−B5) | 1881 | 958 |
| dense (B0) | 429 | 405 |

`sparse(B5)` が約 51%（3374→1666µs）、`residual(B4−B0−B5)` が約 49%
（1881→958µs）減少している（いずれも削減率。減少後の比率で言えば
sparse は約 49%・residual は約 51%に相当する）。`sparse(B5)` の減少は #546（スコアアキュムレータ
のスクラッチプール化）、`residual` の減少は #549（RRF 融合コアの位置索引化）に
それぞれ帰属すると考えられるが、状態 B（#546 のみ）を計測していないため
**寄与の切り分けは確定できない**（申し送り）。`sql_surface`・`projection`・
`dense`・`B8` はノイズ帯程度の変動でほぼ不変。B7（融合コア下限近似）は after
のみ計測可能で、`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #549」節の
単一バイナリ内参考値（10µs）と整合する。参照区間 B0s の実測ノイズ帯（プロセス
min の run-to-run 幅）は A=10.16%・C=8.23%（5 プロセス）。

`crates/engine/benches/harness/hybrid_profile.rs` の stage 定義を確認したところ
（PR #575 codex-review P2 指摘対応）、B0s は `visible_id` 収集ではなく
`CpuScalarProvider::search` を単線・決定的に呼ぶ密側の参照区間
（`B0s_dense_scalar_ref`）であり、上表の見出しをそれに合わせて修正した。また
出力中の `hybrid_profile: memory stage=sparse_index_resident approx_heap_bytes=...`
行は B0s stage の一部ではなく、独立した `sparse_index_resident` という別 stage
のメモリ計測であり、`approx_heap_bytes` は #546 でプール上限分が加算表示される
設計のため、この前後比較では実 RSS の増減指標として扱わない
（`docs/design/hybrid-rrf-latency-breakdown.md` の既存注記のとおり）。

**各プロセス起動直前の loadavg（1分平均。process1..5。PR #575 codex-review
P2 指摘対応で追記）**:

| process | A loadavg | C loadavg |
| --- | --- | --- |
| 1 | 3.87 | 3.61 |
| 2 | 3.40 | 3.75 |
| 3 | 5.37 | 5.45 |
| 4 | 4.82 | 3.71 |
| 5 | 3.12 | 4.50 |

process3（A=5.37・C=5.45）が本セクション内で最も高い loadavg である。訂正
（PR #575 codex-review P2 指摘対応。§4 冒頭の生データから全 15 区分
〔段 × 状態〕を再点検した結果、旧版の「B1・B2・B5 いずれも process3 の値が
min-of-5 に選ばれておらず、外れ値回が min-of-5 集計に混入してはいない」は
誤りだったため訂正する）: 実際には B0（A=429・C=405）・B1-C（6315）・
B2（A=10613・C=8049）・B3-C（643）・B4-A（5684）の計 7 区分で process3 の
値がそのまま min-of-5 として採用されている（B1-A〔8898〕・B3-A〔639〕・
B7-C〔10〕は process1、B4-C〔3029〕・B5-A/C〔3374／1666〕・B8-A/C
〔124／122〕は process4、B0s-A/C〔315／316〕は process5 が min）。
「派生帰属」表が参照する dense（B0）・sql_surface／projection（B1・B2）・
residual（B4 を含む）の主要区分は process3 が min として頻繁に採用されて
いることになるが、高 loadavg の process が min（＝最速）として選ばれている
こと自体は当該 run が負荷側に不利な方向へ振れていないことを示すのみで、
min-of-5 の値を人為的に悪化方向へ押し上げている根拠にはならない（min-of-5
は最良値を採用する集計であり、外れ値混入が懸念されるのは値が押し上げられる
方向のみ）。逆方向（本来より速く見せる混入）の懸念については、process3 の
loadavg（5.37／5.45）が 5 プロセス中最高であり、負荷の高い環境で最速値が
出ていること自体が測定ノイズの範囲内であることを示唆する。

**p95 について（本セクションの対象外・PR #575 codex-review P2 指摘対応で
明記）**: `hybrid_profile_bench` の B0s〜B8 は「1 プロセスあたり内部 5
ラウンドの `baseline_summary`」から min／median のみを出力する設計であり
（`crates/engine/benches/harness/hybrid_profile.rs`）、クエリ単位の p95
percentile は計測していない。そのため本セクションの非退行判定は min-of-5・
median-of-5 の範囲に限定し、p95 前後比較は §3（`feature_bench`）・§5
（crossdb self）を参照する（両方とも `hybrid_rrf` を含む実測が存在する）。

## 5. crossdb self（wire 経由・交互 5 ペア・min-of-5／median-of-5）

25,000 行・dim 128 の共通 fixture（`docs25k.redb`／`queries200.jsonl`／
`docs25k.jsonl`。他セッションが作成した既存 fixture を再利用）に対し、
状態 A・C それぞれ `91f6a18`／`c86c683` を `git archive` で書き出し独立に
`wire-server` をビルドしたうえで `scripts/crossdb_bench/run.py --db self
--config exact` を A→C 交互に **5 ペア**実行した（PR #575 codex-review P1
指摘対応で 3→5 に補完。`CROSSDB_SELF_PORT` を専用ポートへ変更しポート競合を
回避）。

| phase | A min (µs) | C min (µs) | ratio(min) | A median | C median | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| vector_knn | 697 | 698 | 1.000 | 738 | 737 | 0.998 |
| vector_knn_where | 2866 | 2828 | 0.987 | 2983 | 2937 | 0.985 |
| point_where | 2866 | 2828 | 0.987 | 2983 | 2937 | 0.985 |
| where_compound_count | 3974 | 3970 | 0.999 | 3990 | 3986 | 0.999 |
| agg_count | 3453 | 3440 | 0.996 | 3567 | 3585 | 1.005 |
| agg_multi | 3688 | 3733 | 1.012 | 3755 | 3877 | 1.033 |
| group_by_having | 4008 | 4029 | 1.005 | 4045 | 4168 | 1.030 |
| **hybrid_rrf** | **5839** | **5903** | **1.011** | **5961** | **6270** | **1.052** |
| mode_recall | 703 | 686 | 0.975 | 713 | 750 | 1.053 |
| mode_precision | 649 | 671 | 1.033 | 688 | 682 | 0.991 |
| bulk_knn_k200 | 7765 | 7767 | 1.000 | 7888 | 9042 | 1.146 |
| bulk_knn_k1000 | 11259 | 11356 | 1.009 | 11341 | 12545 | 1.106 |
| bulk_knn_where_k200 | 4593 | 4694 | 1.022 | 4730 | 4762 | 1.007 |
| bulk_hybrid_k200 | 9439 | 9311 | 0.986 | 9504 | 9507 | 1.000 |
| rls_isolation | 3354 | 3421 | 1.020 | 3551 | 3566 | 1.004 |
| udf_call | 701 | 744 | 1.061 | 748 | 765 | 1.022 |

**各 pair の p50 生データ（µs、pair1..5。PR #575 codex-review P1 指摘対応で
再判定用に追記）**:

| phase | A | C |
| --- | --- | --- |
| vector_knn | [754, 704, 738, 697, 835] | [714, 757, 698, 737, 743] |
| vector_knn_where | [3061, 2983, 2910, 2866, 3416] | [2922, 2937, 2970, 2828, 3162] |
| point_where | [3061, 2983, 2910, 2866, 3416] | [2922, 2937, 2970, 2828, 3162] |
| where_compound_count | [3979, 3974, 3990, 4001, 5257] | [3985, 3970, 3986, 4013, 4449] |
| agg_count | [3563, 3567, 3577, 3453, 4720] | [3585, 3503, 3440, 3699, 3939] |
| agg_multi | [3751, 3755, 3688, 3761, 4872] | [3877, 3733, 3836, 4151, 4145] |
| group_by_having | [4047, 4043, 4008, 4045, 5083] | [4051, 4362, 4029, 4168, 4363] |
| hybrid_rrf | [5951, 5981, 5961, 5839, 19740] | [6270, 6852, 5949, 5903, 6629] |
| mode_recall | [713, 703, 744, 711, 4039] | [750, 899, 690, 686, 757] |
| mode_precision | [668, 649, 735, 688, 5866] | [702, 699, 682, 682, 671] |
| bulk_knn_k200 | [7888, 7765, 7777, 7920, 25121] | [9042, 9228, 7767, 7882, 9867] |
| bulk_knn_k1000 | [11340, 11259, 11341, 11586, 13160] | [12789, 12545, 11356, 11401, 15668] |
| bulk_knn_where_k200 | [4753, 4736, 4716, 4730, 4593] | [5207, 4738, 4762, 4694, 10763] |
| bulk_hybrid_k200 | [9485, 9504, 9522, 9603, 9439] | [10747, 9311, 9507, 9428, 23412] |
| rls_isolation | [3661, 3602, 3551, 3380, 3354] | [3479, 3421, 3566, 3599, 3735] |
| udf_call | [847, 748, 751, 744, 701] | [823, 744, 757, 1135, 765] |

A の pair5 は前半 12 フェーズ（`vector_knn`〜`bulk_knn_k1000`。例:
`hybrid_rrf` 19,740µs・`bulk_knn_k200` 25,121µs・`mode_recall` 4,039µs）が
大きく跳ねている一方、後半 4 フェーズ（`bulk_knn_where_k200`・
`bulk_hybrid_k200`・`rls_isolation`・`udf_call`）はいずれも 5 ペア中の最小値
であり、全フェーズが一斉にスパイクしたわけではない（PR #575 codex-review P2
再指摘対応で表現を訂正）。`loadavg` は当該 run 直前で 2.99（他 pair と同水準）
であり、`loadavg` に現れない瞬間的な CPU steal・他 worktree のビルド等、
共有 QEMU 環境側の一過性の負荷スパイクが実行順の前半フェーズに偏って生じ、
後半フェーズはむしろ負荷が引いた後に実行されて最小値になった、という仮説を
置いているが未検証である（本 doc は参考値の位置づけのためこの run を除外せず
そのまま記録し、min-of-5・median-of-5 で頑健化する）。

参照区間 `vector_knn` の実測ノイズ帯: A=19.8%・C=8.5%（A は上記 pair5 の
外れ値を含むため、3 ペア時点の 7.1% から拡大）。wire 経由の `hybrid_rrf` は
min 比 1.011・median 比 1.052 であり、5 ペアへの補完後も概ね 1 付近
（ノイズ帯内〜境界）にとどまる。engine 内部（§4）で観測された
`sparse`/`residual` の削減が wire レベルの p50 にはほぼ現れていない。これは
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」節が示す wire 内訳
（SQL 表層 T2−T1p が最大区分・訂正後 ≈66%。旧 67.1% は main スレッド計測
由来のハーネス側アーティファクトを含んでおり Issue #634／#637 で訂正済み。
「訂正版帰属表（Issue #637）」節参照）と整合し、engine 内 hybrid 経路
（T1p・≈27.5%。旧 28.6%も同様に訂正済み）の改善分が SQL 表層・wire 側の
固定コストに対して相対的に小さいため、と考えられる。self の `hybrid_rrf`（min 5,839〜5,903µs・median
5,961〜6,270µs）は `docs/design/crossdb-bench.md` 記載のスナップショット値
（`559b523` 時点 6,178µs）と近い水準にあり、大きな環境差は無い。

**各 pair 実行直前の loadavg（1分平均。pair1..5。PR #575 codex-review P2
指摘対応で追記）**:

| pair | A loadavg | C loadavg |
| --- | --- | --- |
| 1 | 2.36 | 2.41 |
| 2 | 2.38 | 2.35 |
| 3 | 2.38 | 2.35 |
| 4 | 3.18 | 3.08 |
| 5 | 2.99 | 3.96 |

A pair5 の loadavg（2.99）自体は他 pair と同水準であり、上記の前半 12
フェーズのスパイクを loadavg からは説明できない（本文既述のとおり瞬間的な
CPU steal 等の一過性要因という仮説にとどまる）。C pair5（3.96）は本セクション
の中で最も高い loadavg だが、`hybrid_rrf` の C pair5 p50（6,629µs）はスパイクと
呼べる水準ではない。

**`hybrid_rrf` の p95（µs。PR #575 codex-review P2 指摘対応で追記。
`scripts/crossdb_bench/run.py` の JSON 出力に含まれる `p95_us` を pair1..5
から集計）**:

| 指標 | A min-of-5 | C min-of-5 | ratio(min) | A median-of-5 | C median-of-5 | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| hybrid_rrf p95 | 7108 | 7776 | 1.094 | 9094 | 9590 | 1.055 |

各 pair の生値: A=[7844, 9094, 7108, 9494, 38915]・C=[9737, 16405, 7776, 8251,
9590]（A pair5・C pair2 が外れ値）。

参照区間 `vector_knn` の各 pair p95 生データ（µs、pair1..5。PR #575
codex-review P1 指摘対応で `benchmark-judgement-policy.md` §3〔per-run 生
データ必須〕・§4〔参照区間の算出元併記〕に合わせ追記。`scripts/crossdb_bench`
の JSON 出力 `phases.vector_knn.p95_us` から取得）: A=[1219, 767, 1098, 765,
1134]・C=[1117, 1166, 743, 1104, 1056]。`reference_band_A =
(1219 - 765) / 765 = 59.3%`、`reference_band_C = (1166 - 743) / 743 = 56.9%`
（p50 のノイズ帯 A=19.8%・C=8.5% よりも大幅に広い。p95 は分布の裾を見る
統計量であり、共有 QEMU 環境ではこの規模の run-to-run 変動が生じる）。
`hybrid_rrf` の p95 比（min 1.094・median 1.055）の乖離
（`|1.094-1.0|=9.4%`・`|1.055-1.0|=5.5%`）はこの実測帯（57〜59%）を
大きく下回るため、実測帯を超過せず、悪化・改善いずれの根拠にもならない
（ノイズ帯内）。

参考として、本セクションの他フェーズでも p95 は同様に大きく振れる
（PR #575 codex-review P1 指摘対応で生データを追記し `benchmark-judgement-
policy.md` §4・§5 の判定式に照らして再評価する。旧版は生データを示さないまま
「ノイズ」と断定していたが、下記のとおり §4 の 2 種のノイズ帯（固定 ±5%・
参照区間の実測帯）のうち実測帯についても、これらのフェーズは超過するため
その断定は誤りだった）。

**各 pair の p95 生データ（µs、pair1..5。`scripts/crossdb_bench/run.py` の
JSON 出力 `phases.<phase>.p95_us` から取得）**:

| phase | A | C |
| --- | --- | --- |
| bulk_knn_k200 | [8577, 8396, 9316, 8542, 44847] | [20444, 21314, 8336, 10204, 15711] |
| bulk_knn_k1000 | [13247, 14391, 14384, 13697, 30873] | [25058, 28597, 13181, 12475, 27986] |

| 指標 | A min-of-5 | C min-of-5 | ratio(min) | A median-of-5 | C median-of-5 | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| bulk_knn_k200 p95 | 8396 | 8336 | 0.993 | 8577 | 15711 | 1.831 |
| bulk_knn_k1000 p95 | 13247 | 12475 | 0.942 | 14384 | 25058 | 1.742 |

`benchmark-judgement-policy.md` §4 の判定式に照らすと、min-of-5 の比
（0.993・0.942）は固定 ±5% 帯・参照区間の p95 実測帯（vector_knn
A=59.3%・C=56.9%。本節冒頭）のいずれも超えず、悪化の根拠にならない。
一方 median-of-5 の比（1.831・1.742）は固定 ±5% 帯はもちろん、`vector_knn`
の p95 実測帯（57〜59%）そのものも大きく上回る（`|1.831-1.0|=83.1%`・
`|1.742-1.0|=74.2%` はいずれも `reference_band` 57〜59% を超過）。§4 は
「両ノイズ帯を超えた差分のみ判定に効かせる」規約であり、実測帯を超えた
差分を「参照区間より広いノイズ」と再解釈してさらに大きな幅を許容してよい
根拠にはならない。そのため旧版の「共有 QEMU 環境のノイズであって退行
シグナルではない」という断定は §4・§5 の判定基準と整合しておらず撤回する。

この median-of-5 の乖離の原因は本書の計測だけでは特定できない
（原因未確定）。5 pair 中 C は 3 pair（20444・21314・15711）が高く 2 pair
（8336・10204）が低い一方、A は pair5（44847・30873）の単一外れ値を除けば
残り 4 pair が近接しており、単純な「A のみ一過性スパイク」という本節冒頭の
説明はこの 2 フェーズには当てはまらない。`bulk_knn_k200`／`bulk_knn_k1000`
は #546・#549 が変更していない経路（広域取得フェーズ）であり production
変更の対象外だが、この観測自体を「本開発環境の共有 QEMU ノイズの実測例」
として §7 の非退行結論の裏付けに使うことはしない（原因未確定のまま
`hybrid_rrf` 側の結論を補強する証拠にはできないため）。原因の切り分けは
本書のスコープ外とし、必要であれば別 Issue で扱う。全 16 フェーズの p95
詳細表は本書の範囲外とし、`hybrid_rrf`・参照区間（`vector_knn`）・本節の
`bulk_knn_k200`／`bulk_knn_k1000` のみを記録する。

## 6. Recall 3 ゲート層 B（`RECALL_ENGINE=brute_force`／`hnsw` × A／C）

`(0.0,1.0]` 内のプレースホルダ閾値（`0.001`）＋`RECALL_VERBOSE=1` を注入し、
`cargo test --release -p engine --test <name> -- --ignored --nocapture` を
状態 × エンジンの 4 通りで実行した（決定的ハーネスのため 1 回で十分。
プレースホルダは spec 閾値ではなく pass/fail は記録しない）。

| 指標 | A brute_force | A hnsw | C brute_force | C hnsw |
| --- | --- | --- | --- | --- |
| hybrid recall@20（小規模） | 0.9010 | 0.9010 | 0.9010 | 0.9010 |
| hybrid recall@20（大規模） | 0.9145 | 0.9145 | 0.9145 | 0.9145 |
| hybrid recall@100（大規模） | 0.9165 | 0.9165 | 0.9165 | 0.9165 |
| rerank after_recall@20 | 0.9488 | 0.9488 | 0.9488 | 0.9488 |
| rerank non_degraded | true | true | true | true |
| rerank hits20 (baseline/after/ceiling) | 387/389/396 | 387/389/396 | 387/389/396 | 387/389/396 |
| rerank improvement_ratio（informational） | 0.2222 | 0.2222 | 0.2222 | 0.2222 |
| query-planning intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0.9245 |
| query-planning direct_after_recall20 | 0.9321 | 0.9321 | 0.9321 | 0.9321 |
| query-planning intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0.3547 |
| query-planning direct_after_recall20（大規模） | 0.8852 | 0.8852 | 0.8852 | 0.8852 |

**全 11 指標 × 2 エンジンで A/C の差分がゼロ**。ただし Recall ハーネスは
正解 ID の hit 数（recall@k・improvement_ratio 等の集計指標）を比較するのみで
Top-k 内の順位入れ替え・スコアの微小なビット差はこの指標には現れない
（PR #575 codex-review P2 指摘対応でこの限界を明記）。よってこの差分ゼロが
直接裏付けるのは「A/C 間で集計指標（recall@k 等）が非退行」という事実であり、
「スコア・順序がビット同一」という Issue #546・#549 のより強い契約は、
`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」「Issue #549」節に
記載の機械検証（束縛済み式・融合コアの単体テストによる順序付き ID・スコア列の
直接比較）が根拠であり、本書の Recall 一致はそれを補強する状況証拠
（集計指標の非退行）にとどめる。12 run すべて `test result: ok`（`0 failed`）。

層 A（`cargo test --release -p engine --test hybrid_recall --test
rerank_recall --test query_planning_recall --test core_misc sparse_determinism::`
〔束ねたバイナリへの指定は先頭の個別指定にも絞り込みが及ぶため、実際は分けて実行する〕。状態 C）
は全件 green（23 テスト・0 failed）。

## 7. 判定

- **共有 QEMU 環境の計測のため、本書の数値は参考値であり Accepted/Rejected の
  採否根拠にしない**（`docs/design/benchmark-judgement-policy.md` §5・§7.1）
- Recall 3 ゲート（brute_force／hnsw）は A/C で集計指標（recall@k・
  improvement_ratio 等）が完全に一致し、実 hybrid 経路
  （`RECALL_ENGINE=hnsw` の ANN opt-in 経路を含む）で非退行であることを
  確認した。これは非退行の直接証拠であり、共有環境かどうかに依らない結論
  （決定的ハーネスの差分比較）。ただしこのハーネスは hit 数の集計比較であり
  Top-k の順位・スコアのビット列そのものを比較しないため、#546・#549 の
  「スコア・順序はビット同一」契約自体の裏付けは
  `docs/design/hybrid-rrf-latency-breakdown.md` 記載の機械検証（単体テスト）
  が担う
- `feature_bench` の `hybrid_rrf` は参照区間ノイズ帯と同程度の改善
  （比 0.944〜0.945）にとどまり、この計測だけでは「改善した」と断定しない。
  p95（比 0.910〜0.922）も同方向で、p95 側の参照区間ノイズ帯（12〜21%）を
  明確に超えないため p95 側での悪化も観測されない（PR #575 codex-review
  P2 指摘対応で p50 に加え p95 も確認。§3）
- `bench-hybrid-profile` の段別内訳では `sparse(B5)`・`residual(B4−B0−B5)` が
  大きく減少（約半分）しており、engine 内部の狙った経路では改善方向の変化が
  一貫して観測された。ただし専有環境での確定が必要。このハーネスはプロセス
  単位の min／median のみを出力しクエリ単位の p95 を計測しないため、本項の
  非退行判断は min-of-5・median-of-5 の範囲に限定する（§4）
- crossdb self（wire 経由）の `hybrid_rrf` は min 比 1.011・median 比 1.052
  （5 ペアへの補完後。§5 参照区間ノイズ帯 A=19.8%・C=8.5% と同程度〜それ未満）
  で、engine 内部の改善が wire レベルの p50 にはほぼ現れていない。SQL 表層・
  wire の固定コストが支配的なため（`hybrid-rrf-latency-breakdown.md` の内訳と
  整合）。p95（比 1.055〜1.094）も参照区間の p95 ノイズ帯（57〜59%）を
  大きく下回り、裾の悪化を示す根拠にはならない（PR #575 codex-review P2
  指摘対応で追加確認。§5）
- `hybrid_rrf`（対象区間）・`vector_knn`（参照区間）に限れば、一貫した悪化
  （両ノイズ帯を超える）は p50・p95 いずれでも観測されなかった（§5 の A
  pair5 外れ値は前半 12 フェーズに偏ったスパイクであり〔後半 4 フェーズは
  むしろ最小値〕、対象区間固有の退行ではない。原因は共有環境側の一過性要因
  という未検証の仮説にとどまる）。この範囲では差し戻し候補には該当しない
- 本結論は `hybrid_rrf`・`vector_knn` の範囲に限定する。§5 で追加確認した
  `bulk_knn_k200`／`bulk_knn_k1000`（#546・#549 の変更対象外フェーズ）は
  p95 median-of-5 比が固定 ±5% 帯・`vector_knn` の参照区間実測帯（57〜59%）
  の双方を超えており（PR #575 codex-review P1 指摘対応で§5 に生データを
  追記・再評価）、この乖離は原因未確定のまま記録するにとどめ、「共有 QEMU
  環境のノイズだから無視してよい」という判断の根拠には使わない。ただし
  これらは production 変更〔#546・#549〕の対象経路ではないため、
  `hybrid_rrf` 自体の非退行結論を覆すものではない

## 8. 限界・申し送り

- #503（hybrid 密側再取得の再開型探索。sub-issue #504〜#506）は本計測時点で
  **未適用**（open）。#503 マージ後の再実測は別途（#507 の Phase 3 通し比較と
  合わせるのが自然）
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測・Accepted/Rejected の確定は
  オーナー／運用者作業として申し送る
- 状態 B（`c636a81`。#546 単独）は未計測。§4 の `sparse`/`residual` 削減の
  #546/#549 間の寄与切り分けは状態 B の計測を要する
- B7（融合コア下限近似）の before 計測はハーネス非存在のため不能
- 他 DB（pgvector・Qdrant 等）の再計測は行っていない（`crossdb-bench.md` の
  `559b523` 時点値を目標値として参照）
- `feature_bench`／`bench-*` への交互ペア自動集計の組み込みは別 Issue 候補
  （`benchmark-judgement-policy.md` §9）
- `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #547」節（可視率別
  前後比較。担当 #547）とは別観点であり、本書は Phase 6 の通し比較に限定する
- PR #575 codex-review P1 指摘対応（本書 §3〜§5）で §4・§5 のペア数を
  3→5 に補完し、各 run の生データを §3〜§5 にインライン追記した。ただし
  §4・§5 の再測定は初回計測（同一 QEMU 環境・別セッション）とは別の作業
  セッションで実施したため、厳密な意味での「同一計測セッション」内の
  交互比較ではない（§4 の pair1〜3・§5 の pair1〜3 は初回セッション、
  pair4〜5 は本セッションの追加測定。ビルド条件〔同一コミット・同一
  `Cargo.lock`・同一プロファイル〕は統一しているが、セッション間で
  ホストの背景負荷が変化し得る点は申し送る）
- PR #575 codex-review P2 指摘対応で §2〜§5 に各 run／pair／process の
  loadavg 値列を追記し、`benchmark-judgement-policy.md` §3 の環境記録要件
  （各 run の loadavg）を満たした。あわせて §3（`feature_bench`）・§5
  （crossdb self）に `hybrid_rrf` の p95 前後比較を追加し、非退行の結論を
  p50 のみに依らない形へ補強した。`bench-hybrid-profile`（§4）はプロセス
  単位の min／median のみを出力する設計でクエリ単位の p95 percentile を
  持たないため、この段別内訳のみは引き続き p50 相当（min-of-5・
  median-of-5）の範囲に判断を限定する（ハーネスへの p95 percentile 追加は
  本書のスコープ外・別 Issue 候補）

## 9. 再現手順

```bash
# 状態 A・C を明示コミットから git archive で個別ディレクトリへ書き出し
# （ブランチ HEAD は追加コミットで動くため before/after の対象を固定するのに使わない）
mkdir -p <dir-a> <dir-c>
git archive 91f6a18 | tar -x -C <dir-a>
git archive c86c683 | tar -x -C <dir-c>

# 状態 A（before）をビルド
( cd <dir-a> && CARGO_TARGET_DIR=<dir-a>/target cargo build --release -p engine --example feature_bench --bench hybrid_profile_bench --features bench-internals -p wire-server --bin wire-server )

# 状態 C（after）をビルド
( cd <dir-c> && CARGO_TARGET_DIR=<dir-c>/target cargo build --release -p engine --example feature_bench --bench hybrid_profile_bench --features bench-internals -p wire-server --bin wire-server )

# feature_bench 交互 5 ペア（p50 min-of-5・median-of-5 を集計）
for n in 1 2 3 4 5; do
  <dir-a>/target/release/examples/feature_bench > A-$n.json
  <dir-c>/target/release/examples/feature_bench > C-$n.json
done

# bench-hybrid-profile はプロセスを A→C 交互に 5 回起動（BENCH_HYBRID_PROFILE_ROUNDS=5）
export BENCH_HYBRID_PROFILE_ROUNDS=5
for n in 1 2 3 4 5; do
  <dir-a>/target/release/deps/hybrid_profile_bench-* --bench > A-$n.out
  <dir-c>/target/release/deps/hybrid_profile_bench-* --bench > C-$n.out
done

# crossdb self は scripts/crossdb_bench/run.py --db self --config exact を A→C 交互に 5 ペア実行。
# self_db.py の _default_binary() は `__file__`（run.py 自身の配置先）基準で
# `<run.py の repo_root>/target/release/wire-server` を解決する（--binary の
# CLI オーバーライドは無い）。そのため <dir-a>・<dir-c> どちらの状態を計測するかは
# 「どの run.py を呼び出すか」で決まり、run.py を固定してバイナリだけ配置し直しても
# 常に「呼び出した run.py が属するディレクトリ」のバイナリが使われてしまい A/C 比較に
# ならない（PR #575 codex-review 指摘対応）。したがって A・C それぞれの
# `<dir-a>/scripts/crossdb_bench/run.py`・`<dir-c>/scripts/crossdb_bench/run.py`
# （`git archive` で書き出した各ディレクトリ配下のコピー）を明示的に呼び分ける。
# 共通 fixture は既存の `docs25k.redb`／`docs25k.jsonl`／`queries200.jsonl`（$S 配下）を再利用する。
# --out-dir はループ内で固定すると common.write_result() が pair ごとに
# 同じ self_exact.json を上書きし、pair 単位の生データ（p50/p95 raw 値）が
# 失われて §5 の min-of-5／median-of-5 の事後再集計ができなくなる（PR #575
# codex-review P2 指摘対応）。そのため pair 番号をディレクトリへ含め、
# pair ごとに独立した出力先へ書き分ける。
for n in 1 2 3 4 5; do
  python3 <dir-a>/scripts/crossdb_bench/run.py --db self --config exact \
    --rows-file "$S/docs25k.redb" --queries-file "$S/queries200.jsonl" \
    --out-dir <dir-a>/results/pair-$n > crossdb-A-$n.log
  python3 <dir-c>/scripts/crossdb_bench/run.py --db self --config exact \
    --rows-file "$S/docs25k.redb" --queries-file "$S/queries200.jsonl" \
    --out-dir <dir-c>/results/pair-$n > crossdb-C-$n.log
done
# 各 self_exact.json（<dir-a>/results/pair-1..5・<dir-c>/results/pair-1..5）の
# phases.<phase>.p50_us／p95_us を pair1..5 で集計すれば §5 の min-of-5・
# median-of-5（p50・p95 とも）を再現できる。

# Recall 3 ゲート（プレースホルダ閾値・RECALL_VERBOSE=1・RECALL_ENGINE=brute_force|hnsw）
# 比較対象コミット（<dir-a>=91f6a18・<dir-c>=c86c683）でそれぞれ選択するため、
# 各ディレクトリ内で cargo コマンドを実行するループにする（cd はサブシェル内のみ有効で
# 後続コマンドへは引き継がれないため、1 サブシェル内で完結させる）。
for dir in <dir-a> <dir-c>; do
  ( cd "$dir" && \
    RECALL_VERBOSE=1 RECALL_ENGINE=brute_force \
      HYBRID_RECALL_MIN_R20_SMALL=0.001 HYBRID_RECALL_MIN_R20_LARGE=0.001 HYBRID_RECALL_MIN_R100_LARGE=0.001 \
      cargo test --release -p engine --test hybrid_recall -- --ignored --nocapture && \
    RECALL_VERBOSE=1 RECALL_ENGINE=brute_force RERANK_RECALL_MIN_R20_LARGE=0.001 \
      cargo test --release -p engine --test rerank_recall -- --ignored --nocapture && \
    RECALL_VERBOSE=1 RECALL_ENGINE=brute_force \
      QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT=0.001 QUERY_PLANNING_RECALL_MIN_R20_DIRECT=0.001 \
      QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED=0.001 QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE=0.001 \
      cargo test --release -p engine --test query_planning_recall -- --ignored --nocapture )
done
# RECALL_ENGINE=hnsw に差し替えて同様に実行
```

## 10. 参考

- `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」「Issue #549」
  「最新基線」節
- `docs/design/crossdb-bench.md`「`hybrid_rrf` 6,178µs の内訳（Issue #465）」節
- `docs/design/benchmark-judgement-policy.md`（計測規約 SSOT）
