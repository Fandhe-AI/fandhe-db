# 集計の行ループ性能の退行の原因と是正前後（Issue #1491）

## ステータス

記録（production コード・Makefile・scripts・依存の変更なし）。計測は Issue #1491 で実施した。

## 対応する Issue・ポインタ

- 本 Issue: #1491。親 #1486。是正は兄弟 Issue #1487（PR #3145）・#1488（PR #3148）・#1489（PR #3149）、計測ベンチは #1490（PR #3147）
- 退行を入れたとされる変更: #1098（DISTINCT と COUNT(DISTINCT)。コミット ad07bfd）
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-25（集計表層）。spec 本文は転記しない
- 各 Issue の実装記録は [implementation-status.md](implementation-status.md) の Issue #1487〜#1491 の項、
  計測ベンチは [aggregate-loop-bench.md](aggregate-loop-bench.md)

## 結論（先に）

| 問い | 答え |
| ---- | ---- |
| 退行を入れたコミットは | 本機の bisect で first bad = ad07bfd（#1098）、last good = c94dc5d。#1486 の結論と一致した |
| 是正は退行幅を回収したか | 是正前後（f9f3e1dc8 対 2e94de153）の交互 N=5 で、agg_multi は 232.01 us（回収率 75.94%）、agg_count は 52.59 us（107.91%）、rls_isolation は 57.65 us（103.01%）回収した（参考値。後述の限界を参照） |
| crossdb の「集計系の退行」所見は解消したか | **未解消の水準**。最新の 2e94de153 は last good の c94dc5d に対し agg_multi が 4.6032 倍（min-of-N）。プラン規則の判定は「部分解消」だが、c94dc5d 比では解消していない |
| なぜか | 2026-10-09（cb73a8e）以降に、別の集計経路の増加が重なっている（8e68162 → dc9261e → ac4bbae で agg_multi が 611.79 → 843.69 → 1643.07 us）。#1486 の範囲外で、本 Issue では原因を調べていない |

数値はすべて、コミットした生 JSON を [summarize.py](bench-data/aggregate-regression-bisect/summarize.py) で集計した出力
（各 `summarize-output.md`）から写した。手で丸めた値や「約」の値は使っていない。

## 背景

2026-10-09 の crossdb 再計測で、self exact の集計系フェーズだけが 2026-09-18 より遅くなった（Apple M4 Max・共有機の参考値）。
既存の生データを `summarize.py` で再集計した値（min-of-N、n=5）は次のとおり。

| phase | 0ab63b3（2026-09-18） | cb73a8e（2026-10-09） |
| ----- | --------------------- | --------------------- |
| agg_multi | 248.96 us | 564.04 us |
| agg_count | 104.50 us | 146.08 us |
| rls_isolation | 110.29 us | 166.08 us |
| vector_knn | 573.33 us | 537.04 us |
| group_by_having | 70.58 us | 74.58 us |

生データ: `bench-data/crossdb-20260918T142251Z-ab/` と `bench-data/crossdb-20261009T140039Z-ab/`。
再集計の出力: [summarize-output.md](bench-data/aggregate-regression-bisect/summarize-output.md) の「背景」節。

crossdb のスコアボード（対照 DB との勝敗）は、退行の前も後も agg 系で self の勝ちである。
ここでいう「集計系の退行」所見は、勝敗ではなく self の前回比の悪化を指す。

## bisect

### 条件

- 範囲: good = 0ab63b3、bad = cb73a8e（316 コミット。`git rev-list --merges` は 0 件で履歴は線形）
- 環境: 本機（Linux x86_64・i7-13700K・10 論理 CPU）。#1486 の M4 Max とは別の環境のため、絶対値は比べず、同じ機械の中の比で判定した
  （[env.txt](bench-data/aggregate-regression-bisect/env.txt)）
- 方法: `git bisect` は HEAD を動かすため使わず、`git rev-list --bisect` で中点を選ぶ手動 bisect
- 計測: ステップごとに `self exact` を 3 回計測し、`agg_multi` の p50 の min-of-N で判定
- **判定規則（計測の前に決めた）**: 端点 3 回ずつの min-of-N の中点 T を求め、T 以上なら bad、未満なら good。
  T の ±15% 帯に入った場合は追加計測する規則だったが、全ステップを 3 回計測にしたため追加は発生していない
- ビルド: ステップごとに `git archive` で展開し `cargo build --release`。共有 `CARGO_TARGET_DIR` を使う

### fixture をステップごとに作り直した理由

redb の形式やカタログはコミットによって異なり、別コミットで作った fixture は開けない、または取り違えると誤計測になる。
そのためステップごとに、そのコミット自身の `seed_docs`（25,000 行・128 次元。可視 23,000 行）で fixture を作り直した。
ハーネス（`scripts/crossdb_bench/`）もそのコミットのものを使った。新しい phase を古いバイナリに流して
計測全体が止まるのを避けるためである。古いバイナリに別コミットの fixture を渡したときのエラーの内容は、確かめていない（未確認）。

### 計測の落とし穴: git archive の mtime

最初の試行では、共有 `CARGO_TARGET_DIR` に新しいコミットをビルドした後に古いコミットを展開すると、cargo が
「成果物より古いソース」を変更なしと見なして別コミットの成果物を再利用した。ed2effa で wire-server が
`ArrayValue` の網羅漏れでコンパイルできなくなって気づいた。この試行のデータはすべて破棄して取り直した。
ドライバは展開直後に全ファイルの mtime を現在時刻に更新する（`run_bisect_step.sh`）。

### 全ステップ

端点: good（0ab63b3）の agg_multi は min-of-N 288.90 us、bad（cb73a8e）は 684.88 us、T は 486.89 us。

| step | commit | 件名 | agg_multi p50 (us) の各回 | min-of-N | agg_count min-of-N | 判定 |
| ---- | ------ | ---- | ------------------------- | -------- | ------------------ | ---- |
| 00 | 0ab63b3 | test(bench): crossdb へ NoSQL 対照を追加（#852） | 291.57, 288.90, 292.13 | 288.90 | 74.85 | 端点（good） |
| 00 | cb73a8e | chore: エージェントスキルを最新に更新（#1484） | 684.88, 1186.41, 1218.35 | 684.88 | 150.13 | 端点（bad） |
| 01 | a2195a2 | feat(engine)!: ALTER TABLE DROP COLUMN／ALTER COLUMN TYPE（#1216） | 1085.29, 653.96, 676.84 | 653.96 | 157.47 | bad |
| 02 | a10e115 | feat(engine)!: CREATE VIEW / DROP VIEW（#1048） | 343.91, 344.69, 345.72 | 343.91 | 70.56 | good |
| 03 | 9dece10 | refactor(engine)!: DML 影響行数上限 API の統合（#1116） | 593.77, 621.57, 585.66 | 585.66 | 123.89 | bad |
| 04 | 2459b22 | feat(engine): OFFSET とページング（#1095） | 526.63, 350.39, 403.76 | 350.39 | 77.03 | good |
| 05 | f0d5a19 | feat(engine)!: UNION / UNION ALL / INTERSECT / EXCEPT（#1105） | 659.33, 607.67, 582.99 | 582.99 | 125.49 | bad |
| 06 | 72cb975 | feat(engine)!: EXPLAIN の対象文の拡大（#1102） | 1165.50, 636.57, 614.26 | 614.26 | 141.92 | bad |
| 07 | ad07bfd | feat(engine): DISTINCT と COUNT(DISTINCT)（#1098） | 677.43, 644.52, 668.68 | 644.52 | 123.86 | bad |
| 08 | c94dc5d | docs(wire): TLS 自作実装のセキュリティ監査と設計 ADR（#1097） | 345.34, 339.00, 346.42 | 339.00 | 75.13 | good |

結論: **first bad = ad07bfd（#1098）、last good = c94dc5d（#1097）**。隣り合う 2 コミットで 339.00 us から 644.52 us に跳ねた。
M4 Max の #1486 の結論と同じコミットに収束した。

所見:

- 外れ値が出る。同じコミットでも step 00 の bad（684.88 / 1186.41 / 1218.35）や step 01、step 06 のように 1 回だけ大きく外れる値があり、
  単発の計測では判定を誤りうる。このため全ステップを 3 回計測し min-of-N で判定した
- 0ab63b3 の 288.90 us から a10e115 の 343.91 us、c94dc5d の 339.00 us へは、退行前に小さな段差がある。
  ad07bfd の跳ねとは別の変化で、原因は調べていない
- 集計を通らない group_by_having は c94dc5d の 38.43 us と ad07bfd の 39.96 us でほぼ変わらない
  （値は [summarize-output.md](bench-data/aggregate-regression-bisect/summarize-output.md) の last-good-c94dc5d と first-bad-ad07bfd のグループ集計）。
  vector_knn は外れ値の幅が大きく（幅は同ファイル参照）、前後の差は読み取れない

## 原因の構造分析

cb73a8e 時点のコード（`crates/engine/src/sql/aggregate.rs`。行番号は cb73a8e のもの）を読んだ結果。

1. **集計セルごとの予算判定**: 全走査・Fast 経路・索引経路の 3 箇所は `observe_with_budgets`（158 行付近）を通る。
   この関数は 1 セルごとに `text_len()` と `distinct_footprint()` を観測の前後で読み、2 種類の予算（TEXT の累計と COUNT(DISTINCT) の累計）を判定する。
   COUNT(DISTINCT) を含まない通常の集計（COUNT・SUM・AVG・MIN・MAX）でも、この呼び出しが行ごと・集計ごとに走る
2. **`Accumulator::observe` の先頭分岐**: 828 行付近の先頭で `CountDistinct` かどうかを判定し、該当すれば `observe_distinct`（1080 行付近）へ分岐する。
   通常の集計もこの判定を毎回通る
3. **enum サイズ**: `CountDistinct` の追加で `size_of::<Accumulator>()` が 48 から 64 バイトになった（#1489 の記録による。
   [implementation-status.md](implementation-status.md) の Issue #1489 の項）。集計状態のコピーと配置が大きくなる
4. **別経路への影響なし**: `group_by_having` は `group_by.rs` の別経路で、`where_compound_count` は集計を通らない。
   group_by_having の c94dc5d と ad07bfd の値（上記）から、退行は集計の行ループに閉じていると見ている（`GROUP BY` 経路そのものの計測ではない）

1 セルあたりのコスト（`agg_multi` は 5 集計 × 可視 23,000 行、`agg_count` は 1 集計 × 23,000 行で割った値。本機）:

| 地点 | agg_multi | agg_count |
| ---- | --------- | --------- |
| last good（c94dc5d） | 2.948 ns | 3.266 ns |
| first bad（ad07bfd） | 5.605 ns | 5.385 ns |
| cb73a8e | 5.955 ns | 6.527 ns |

ad07bfd で 1 セルあたり 2〜3 ns 程度が加わっており、#1486 が M4 Max で見た増分と同じ桁である。
ただし、内訳（予算判定・分岐・enum サイズのどれがどれだけか）は本 Issue では分離していない（個別の是正の効果は #1487〜#1489 の記録を参照）。

## 是正内容

意味論は変えていない（NULL の除外、新しいキーだけを計上、予算超過は `54000`、`observe` に来た `CountDistinct` を `XX000` で拒否する fail-closed のガード）。

- **#1487（PR #3145）**: `observe_with_budgets` に `#[inline(always)]` を付けた
- **#1488（PR #3148）**: COUNT(DISTINCT) の予算計上を通常集計の行ループから分離した。`Accumulator::observe` 先頭の分岐を外し、
  DISTINCT 系を含まない集計は `distinct_footprint` を呼ばない軽量経路（`observe_light`）で処理する
- **#1489（PR #3149）**: `CountDistinct` の状態を `Box` で包み、`size_of::<Accumulator>()` を 64 から 48 バイトに戻した

詳細は [implementation-status.md](implementation-status.md) の各 Issue の項。

## 是正前後の A/B（交互 N=5）

- before = f9f3e1dc8（#1487 の直前）、after = 2e94de153（#1489 の直後。本ブランチの base）
- 5 ペアを before → after の順で交互に実行（ペア内の順番は固定）。各計測でそのコミット自身のソース・fixture・ハーネスを使う
- ペア開始時の loadavg: [loadavg.log](bench-data/aggregate-regression-ab/loadavg.log)

| 区間 | before min | before median | after min | after median | ratio (min-of-N) | 自区間の幅 | 判定クラス |
| ---- | ---------- | ------------- | --------- | ------------ | ---------------- | ---------- | ---------- |
| agg_multi | 1808.98 | 1855.24 | 1576.97 | 1589.24 | 0.8717 | 2.72% | 帯内 |
| agg_count | 438.53 | 439.60 | 385.93 | 396.90 | 0.8801 | 6.05% | 帯内 |
| rls_isolation | 471.51 | 474.75 | 413.85 | 416.23 | 0.8777 | 8.97% | 帯内 |
| vector_knn | 645.63 | 660.60 | 638.98 | 674.60 | 0.9897 | 9.71% | 帯内 |
| vector_knn_where | 273.23 | 289.24 | 269.21 | 313.86 | 0.9853 | 27.03% | 帯内 |
| where_compound_count | 109.35 | 111.52 | 121.17 | 125.15 | 1.1080 | 69.77% | 帯内 |
| group_by_having | 50.92 | 52.35 | 50.66 | 51.03 | 0.9949 | 31.84% | 帯内 |

単位は us。「判定クラス」は benchmark-judgement-policy §4 の方式（固定 ±5% と、参照区間の実測 run-to-run 幅の両方を超えた差だけを有効とする）。
参照区間の最大幅が 69.77%（where_compound_count の外れ値）と大きいため、この方式ではどの区間も「帯内」になる。
一方、集計 3 区間の自区間の幅は 2.72%〜8.97% と小さく、before と after の min-of-N も median も分離している（agg_multi の after の最大は before の最小より小さい）。
そのため、集計 3 区間の ratio（min-of-N）は 0.8717〜0.8801 で、共有機の参考値としては改善と読めるが、ポリシー上の有効な差とは言い切れない。

1 セルあたり: agg_multi は before 15.730 ns・after 13.713 ns、agg_count は before 19.066 ns・after 16.780 ns。
この 1 セルあたりの値は、ad07bfd 時点（5.605 ns / 5.385 ns）より大きい。後述の 10-09 以降の増加による。

## last good との比較とスコアボード所見の判定

### 判定基準（計測の前に決めた）

同じ機械の上で、after の agg_multi・agg_count の min-of-N を c94dc5d と比べる。

- (a) 両方のノイズ帯（固定 ±5% と参照区間の実測幅）の内側なら「解消」
- (b) 外側だが、退行幅（ad07bfd − c94dc5d。bisect の生 JSON から算出）の半分以上を回収していれば「部分解消」
- (c) それ未満なら「未解消」

rls_isolation も同じ基準で併記する。いずれも共有機の参考値（`BENCH_DEDICATED_ENV` 未設定）で、専有環境での再測定ではない。

### 結果

c94dc5d（ref）と 2e94de153（after）の交互 N=5（ペア内は ref → after 固定）:

| 区間 | ref min | after min | ratio (min-of-N) | 自区間の幅 | 判定クラス |
| ---- | ------- | --------- | ---------------- | ---------- | ---------- |
| agg_multi | 340.00 | 1565.09 | 4.6032 | 6.72% | 悪化 |
| agg_count | 75.78 | 386.72 | 5.1031 | 31.04% | 悪化 |
| rls_isolation | 78.28 | 408.65 | 5.2205 | 6.78% | 悪化 |
| vector_knn | 602.87 | 656.17 | 1.0884 | 58.19% | 帯内 |
| group_by_having | 39.12 | 52.21 | 1.3346 | 区間の幅は summarize-output.md を参照 | 帯内 |

退行幅に対する回収（上の A/B と bisect の生 JSON から。単位 us）:

| phase | 退行幅（ad07bfd − c94dc5d） | 是正の回収量（before − after） | 回収率 | after − c94dc5d |
| ----- | ---------------------------- | ------------------------------ | ------ | --------------- |
| agg_multi | 305.52 | 232.01 | 75.94% | 1237.97 |
| agg_count | 48.74 | 52.59 | 107.91% | 310.81 |
| rls_isolation | 55.97 | 57.65 | 103.01% | 335.46 |

### 判定

- 基準 (a) は満たさない（c94dc5d の 4 倍超）
- 基準 (b) は、退行幅に対する回収量で見ると 3 区間とも満たす。プラン規則どおりなら **部分解消**
- ただし (b) は、10-09 以降の別の増加を含んだ絶対値（us）の差同士を比べており、参考にとどめる。
  **「集計系が c94dc5d の水準に戻った」とは言えず、crossdb の「集計系の退行」所見は 2e94de153 でも未解消の水準**と記録する
- 10-09 以降の増加は ad07bfd とは別のもので、本機の追加計測（各 n=3、min-of-N の agg_multi）は次のとおり。
  cb73a8e は 684.88 us、8e68162（#2529）は 611.79 us、dc9261e（#2724）は 843.69 us、ac4bbae（#2921）は 1643.07 us、
  f9f3e1dc8（A/B の before）は 1808.98 us。dc9261e と ac4bbae の間に大きな段差がある
  （[summarize-output.md](bench-data/aggregate-regression-bisect/summarize-output.md) の later-* グループ）

## 再現手順

パスはプレースホルダ。`<tgt>` は共有の `CARGO_TARGET_DIR` 用の一時ディレクトリ。

```bash
# 1 コミット分の計測（ビルド・fixture 作り直し・self exact）
BENCH_TARGET=<tgt> bench-data/aggregate-regression-bisect/run_bisect_step.sh <sha> <out_dir> <port>

# 是正前後（ペア内は A -> B 固定）
BENCH_TARGET=<tgt> bench-data/aggregate-regression-ab/run_pairs.sh \
  f9f3e1dc8 before 2e94de153 after <out_dir> 5

# last good との比較
BENCH_TARGET=<tgt> bench-data/aggregate-regression-ab/run_pairs.sh \
  c94dc5d ref 2e94de153 after <out_dir> 5

# 集計（summarize-output.md の再生成）
bash bench-data/aggregate-regression-bisect/make_summaries.sh
```

bisect 本体は `git rev-list --bisect <bad> ^<good>` で中点を選び、`run_bisect_step.sh` を 3 回呼んで判定する手作業の繰り返しである。

## 生データ

- [bench-data/aggregate-regression-bisect/](bench-data/aggregate-regression-bisect/NOTES.md): bisect の全ステップ、端点、以降のコミットの `self_exact.json`
- [bench-data/aggregate-regression-ab/](bench-data/aggregate-regression-ab/NOTES.md): 是正前後の交互 N=5
- [bench-data/aggregate-regression-ref-ab/](bench-data/aggregate-regression-ref-ab/NOTES.md): c94dc5d と 2e94de153 の交互 N=5

## 限界

- 環境が #1486 と違う（本機は x86_64 の Linux、#1486 は Apple M4 Max）。絶対値は比べず、同じ機械の中の比で判定した
- 共有機。他のエージェントのビルド等が並走しており、loadavg は 3〜9 の帯だった。外れ値が出るため min-of-N を主統計量にした
- ハーネスと fixture がコミットごとに違う。クエリの形（`agg_multi` は `COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id)`）は c94dc5d と 2e94de153 の間で変わっていないが、
  fixture の生成物はコミットごとに作り直している
- 2e94de153 は c94dc5d より DISTINCT 系を含む多くの機能を持つため、同じ計測でも経路が同一ではない
- 回収率は絶対値（us）の差の比で、10-09 以降の増加が混ざる
- 規則上の追加計測（T の ±15% 帯に min-of-N が入った場合）は、全ステップを 3 回計測にしたため発動していない。帯に入ったステップも無かった
- 内訳（予算判定・分岐・enum サイズ）は分離していない

## スコープ外

- 10-09 以降の追加の増加（dc9261e → ac4bbae の段差を含む）の原因調査と是正
- `GROUP BY`・索引経路・NoSQL の同等計測
- CI ゲート化（`aggregate_loop_bench` の週次計測は #1490 が担当）

## #2888 の是正（Issue #3208）

- 原因: #2888 で集計の行ループに行ごとの `interrupt::check()` が入った。`check()` は呼ぶたびに thread-local を 3 つ触る（抑止カウンタ・デッドラインのスロット・poll スタック）ため、間引きカウンタ自体が thread-local 側にあっても行ごとに数 ns かかる
- 変更: `interrupt::RowCheck`（ループ局所のカウンタ）を新設し、集計の 3 つの行ループ（Fast 経路の可視 id・全走査・索引候補スロット）の `check()?` を `row_check.tick()?` に置き換えた。行ごとのコストは局所カウンタの減算と分岐 1 つだけになる
- 二重の間引きを避ける: `check()` は内部で既に `CHECK_STRIDE` 回に 1 回しか評価しない。そのまま N 行に 1 回呼ぶと実効間隔が N × `CHECK_STRIDE` 行になる。そこで `RowCheck` は内部の間引きを飛ばす即時評価（`check_reason_inner(force = true)`）を呼ぶ。抑止（`suppressed`）・ラッチ・`fire_after` プローブ・再入無視の意味は `check()` と同一
- 間隔 `ROW_CHECK_INTERVAL` は `CHECK_STRIDE` と同じ 1024。最初の `tick` は必ず評価する（先頭行での確認に依存する既存契約の維持）。置き場所は従来どおりループ先頭で、全走査では RLS の `continue` より前（物理行ごとに進めるので、不可視行だけの走査でも中断できる）
- 間隔の根拠: 評価 1 回（数十 ns）を 1024 行で割ると 1 行あたり 0.1 ns 未満で、4096 にしても差は計測誤差に埋もれる。増分の本体は行ごとの thread-local アクセスで、N の大小ではない。一方で 4096 にすると遅延の上限が 4 倍になり、`statement-interrupt.md` の契約の改訂が要る
- 中断までの最大遅延: 期限到達または切断の検出から、最大 `ROW_CHECK_INTERVAL` 行分の処理（最も重い 1 反復 × 1024）＋確認点を持たない区間＋`Err` の巻き戻し。従来の `check()` と同じ上限で、変わらない。liveness の probe は wire 側の `PROBE_INTERVAL`（250ms）で抑えているため、評価の回数が変わっても probe の頻度は変わらない
- 固定した検査: 確認回数が行数ではなく ⌈行数/1024⌉ の水準になること、後方の確認点・2 回目の poll でも `57014` になること、結果が（NULL・空集合を含め）スコープの有無で一致すること、RLS で他テナントの行が続いても自テナント分だけになること（`sql3208_aggregate_row_check.rs`）
- A/B（同一ホストの壁時計）: 未検証。本変更の作成環境では実施していない。受け入れ条件のうち「#2888 の増分が消える」は、決定的な根拠（行ごとの thread-local アクセスの除去）までで、実測の回収率は後続の計測で確認する

## #2684 の是正（Issue #3211）

- 原因（コード差分からの推論）: #2684 で `Accumulator::observe` が「`eval_aggregate_input` が行ごとに `AggregateRowValue` を作る → `observe_value` が読んで振り分ける」の 2 段になった。式を含まない入力（`COUNT(*)`・id・列参照）でも行ごとに中間値の生成と 2 回目の `match` が入る。前後を分けた実験は未実施（下記）
- 変更: `Accumulator::observe` の先頭ガード（`CountDistinct`・`Grouping`・`StringAgg`・`ArrayAgg`）の後に、式を含まない入力を中間値なしで直接観測する分岐を足した。対象は `AllVisible`・`GroupingKeys`・`VectorColumnPresence`・`IdU64` と各列参照（TEXT・整数系・REAL・DOUBLE・NUMERIC・日付時刻系・BOOLEAN・BYTEA・ENUM・UUID・ARRAY・JSON）
- 従来経路へ戻る条件: 式（`ScalarExpr`・`BoolExpr`・`TypedExpr`・`VectorExpr`）・`VectorColumn`・今後足される variant は `_` の腕で従来の 2 段経路へ戻る（fail-closed）。`StringAgg`／`ArrayAgg` の入力は先頭ガードのまま
- 意味の基準: 現行の「`eval_aggregate_input` → `observe_value`」の合成。NULL は種別確認より前に `Ok(())`、型不一致は同じ `accumulator_bug`（XX000）の文言。列の取り出しと型検査は `aggregate_input.rs` の `cell_*` に集約し、2 経路で共有する
- 変えないもの: `observe_value`・`observe_distinct`・`window.rs` の直接呼び出し・`observe_with_budgets`・FILTER・RLS／TABLE-12 の検査位置。実行時の切替は入れていない
- 固定した検査: 単体の差分テスト `direct_observe_matches_two_stage_path`（入力 × 関数 × 値で、`Ok`／`Err` の文言・`finish`・`text_len` が 2 経路で一致）、結合テスト `sql3211_aggregate_direct_observe.rs`（結果型・値・NULL・空集合・GROUP BY・FILTER・window・テナント境界）
- A/B（同一ホストの壁時計）: 未検証。本変更の作成環境では実施していない。受け入れ条件のうち「#2684 の増分が消える」は、中間値の生成と 2 回目の `match` の除去までが根拠で、実測の回収率は後続の計測で確認する。因果を前後で分ける実験（#2684 以前の直接 match の手移植との比較）も未実施
- スコープ外: `observe_value` のインライン化の復元（#3209）、agg_multi の残差（#3210）、`VectorColumn` の 1 段化
