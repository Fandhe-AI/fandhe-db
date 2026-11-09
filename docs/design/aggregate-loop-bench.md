# 集計の行ループ計測ベンチ（Issue #1490）

## ステータス

実装済み（Issue #1490）。

## 対応する Issue・ポインタ

- 本 Issue: #1490。親 #1486、関連 #1204・#1319（関係演算 p95 ベンチ）・#1491（退行の原因と是正前後の設計記録）
- 是正は兄弟 Issue #1487・#1488・#1489 が担当する。本ベンチは是正を含まず、検知だけを担う
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-25（集計表層）。spec 本文は転記しない

## 目的

2026-10 の crossdb 再計測で集計系フェーズ（agg_multi・agg_count・rls_isolation）だけが前回より遅くなったが、
engine 単体の計測が無く、約 3 週間後の再計測まで気づけなかった。engine 単体で集計の行ループのコストを測り、
週次で推移を見られるようにして、同種の退行を早く見つける。

## 構成

| 項目 | 内容 |
| ---- | ---- |
| 計測本体 | `crates/engine/benches/aggregate_loop_bench.rs`（`harness = false`・`test = false`） |
| 時間非依存ロジック | `crates/engine/benches/harness/aggregate_loop.rs`（回帰は `tests/relational_p95_accept.rs` に追記。テストバイナリは増やさない） |
| 実行入口 | `make bench-aggregate-loop`（`ci` には含めない） |
| CI 配線 | `.github/workflows/bench.yml` の `bench-aggregate-loop`（workflow_dispatch・週次 schedule。verdict 非ゲート・secrets と environment は不要） |

### 既存の `relational_p95_bench` を拡張しなかった理由

`relational_p95_bench` は GitHub Actions 上の起動を拒否する契約で、fixture も 100,000 行 × 768 次元と重い。
その契約は緩めず、集計専用の別バイナリにして、計測プロトコル（`harness::protocol`）・統計
（`harness::relational_p95::summarize_rounds`／`round_p95`／`rotate_arms`）・出力行の並びを共有した。
行頭のプレフィックスだけ `aggregate_loop:` とし、`group=aggregate` を付ける。

## arm と fixture

| arm | 文 | 集計項目数 | 期待する `access_path` |
| --- | -- | ---------- | ---------------------- |
| `agg_count` | `COUNT(*)` | 1 | `visible_bitmap_cache` |
| `agg_multi` | `COUNT(*)`・`SUM(id)`・`AVG(id)`・`MIN(id)`・`MAX(id)` | 5 | `visible_bitmap_cache` |
| `agg_count_distinct` | `COUNT(DISTINCT lang)` | 1 | `full_scan` |

- 文は crossdb の agg_count・agg_multi と同形
- 自テナントの Public 行 25,000（id 連番・`lang` は 16 種・次元 128）に、他テナントの Private 行（`lang` は語彙外の定数）を混ぜる
- 経路の期待値は静的な `EXPLAIN` の `access_path:` で自己検査する。経路が変わると計測の意味が変わるため、不一致は非 0 終了にする

## 計測プロトコル

- `MeasurementConfig::new(20, 200, 1)`（warmup 20・計測 200）× N ラウンド（既定 5、5〜50）。arm の順序はラウンドごとの輪番
- 戻り値の解放は計測区間の外へ出す
- 出力: ラウンド行・要約行（`min_of_n`・`median`・`max`・`run_to_run_band`）・導出行（`ns_per_cell_min`・`ns_per_cell_median` = 時間 ÷ (行数 × 集計項目数)）・閾値行
- 閾値行は常に `not evaluated`。性能の良し悪しは終了コードに反映しない

## 事前検査（fail-closed）

1. 自テナント文脈の結果が、fixture の規則から独立に導いた期待値（COUNT・SUM・AVG・MIN・MAX・DISTINCT）と一致する
2. 他テナント文脈でも同様に一致する（越境があれば見える fixture であることの対照）
3. `access_path` が期待どおり

違反は非 0 終了する。出力にテナント ID・行の値・SQL 全文は含めない。
GitHub Actions 下で `BENCH_DEDICATED_ENV=1` が渡された場合は拒否する（hosted runner は共有環境）。

## 再現手順

```bash
make bench-aggregate-loop
BENCH_AGGREGATE_LOOP_ROUNDS=10 make bench-aggregate-loop
BENCH_AGGREGATE_LOOP_ROWS=1000 make bench-aggregate-loop   # scale=reduced（記録には使わない）
```

| env | 既定 | 意味 |
| --- | ---- | ---- |
| `BENCH_AGGREGATE_LOOP_ROUNDS` | 5 | ラウンド数（5〜50） |
| `BENCH_AGGREGATE_LOOP_ROWS` | 25000 | 自テナント行数（1000〜25000。縮小は `scale=reduced`） |
| `BENCH_DEDICATED_ENV` | 未設定 | 専有環境の自己申告（GitHub Actions 下では拒否） |

## 退行の検知力の確認（AC1）

実施時点では是正 Issue（#1487〜#1489）が main に未マージだったため、「呼び出し層を足すと計測が動くか」を確かめた。
ad07bfd 相当の変更を模すため、`sql/aggregate.rs` の `observe_with_budgets` を `#[inline(never)]` の中継関数で包み、
内側の呼び出しの前後で `text_len()`／`distinct_footprint()` をもう 1 回ずつ読む一時パッチを当てた（コミットしていない）。

- A: 本ブランチ（パッチなし）。B: A に上記の一時パッチを当てたバイナリ
- 環境: Linux x86_64・10 論理 CPU・AVX2+FMA。他プロセスが走る共有機で、ラン間の幅（`run_to_run_band`）は外れ値ラウンドで大きく広がった。判定は外れ値に強い `min_of_n` で行う
- A,B,A,B… の順に 5 ペア実行（各 5 ラウンド）

| ペア | agg_multi A (ms) | agg_multi B (ms) | 差 (ms) | agg_count A / B (ms) | distinct A / B (ms) |
| ---- | ---------------- | ---------------- | ------- | -------------------- | ------------------- |
| 1 | 1.942 | 2.175 | +0.233 | 0.443 / 0.498 | 4.089 / 8.362 |
| 2 | 1.875 | 2.057 | +0.182 | 0.438 / 0.501 | 4.688 / 4.207 |
| 3 | 1.853 | 2.203 | +0.350 | 0.425 / 0.483 | 4.043 / 4.273 |
| 4 | 1.848 | 2.061 | +0.213 | 0.423 / 0.476 | 4.068 / 4.407 |
| 5 | 1.891 | 2.146 | +0.255 | 0.427 / 0.493 | 4.093 / 4.819 |

- agg_multi は 5 ペアすべてで B が遅い（+10〜+19%）。A の `min_of_n` のペア間の散らばりは 1.848〜1.942 ms（約 5%）で、差はそれより大きい
- セルあたり ns は A が約 14.8〜15.5、B が約 16.5〜17.6 で、約 +1.2〜+2.8 ns/セル。親 Issue の見積もり（約 2.5 ns/セル）と同じ桁
- agg_count も 5 ペアすべてで B が +0.05 ms 前後（約 +12%）遅い。agg_count_distinct はペア 1・5 の外れ値を除くと +0.2〜0.4 ms で、共有機のばらつきが大きく向きの一貫性は弱い
- 判定: 呼び出し層の追加で agg_multi・agg_count の `min_of_n` が明確に動く。ラン間の幅は外れ値ラウンドで広がるため、幅との比較ではなく、ペア間の向きの一貫性と A 側の散らばりとの比較で判定した

c94dc5d と ad07bfd に新しいベンチを重ねて比べる案は採らなかった。両コミットには EXPLAIN の `access_path:` が無く、
`OperationId` のモジュール位置も異なり、COUNT(DISTINCT) は c94dc5d に無い。差分を吸収する shim が必要になり、
HEAD の計測と同じものを測っている保証が弱くなるため。原因の構造分析は #1491 が担当する。
是正が main に入った後は、是正前後のバイナリで同じ手順を再実施できる。

## 必須 CI の所要時間（AC2）

`ci.yml`・`make ci` は変更しない。ベンチは `test = false` で `cargo test` ではビルドされない。
回帰テストは既存の `relational_p95_accept` に追記しただけで、テストバイナリは増えない。

## スコープ外

- 是正そのもの（#1487〜#1489）と原因・是正前後の設計記録（#1491）
- `relational_p95_bench` の GitHub Actions 配線と拒否の緩和
- 性能の閾値による自動判定（基準が無く、共有 runner の値は確定判定に使えない）
- `GROUP BY` 経路・索引経路の集計・NoSQL の aggregate の同等計測
