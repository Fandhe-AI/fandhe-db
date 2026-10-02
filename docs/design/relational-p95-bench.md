# 述語・順序・結合の p95 計測ベンチ

- ステータス: Accepted（計測入口の追加と、共有環境での参考値の記録）
- 対応: Issue #1204（親 #1206）、専有環境での再測定手順は Issue #1320
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・SQL-25・SQL-28、RLS-10（spec 本文は転記しない）

## 目的

`WHERE` の `OR`／`IN` 述語（SQL-24）、スカラー列による `ORDER BY`（SQL-25）、2 テーブル結合（SQL-28）は
SQL 表層の関係演算経路を通るが、p95 を測る回帰ベンチが無かった。
`crates/engine/benches/relational_p95_bench.rs` を追加し、各形の p95 を
[benchmark-judgement-policy](./benchmark-judgement-policy.md) に沿って計測・記録する。
本 Issue は計測と記録までを範囲とし、性能改善と production コードの変更は行わない。

## 計測対象 arm と fixture

| グループ | arm | 文の形（要旨） | 規模 |
| -------- | --- | -------------- | ---- |
| predicate | `pred_eq`（参照） | `WHERE lang = 'l0' ORDER BY embedding <=> '<vec>' LIMIT 10` | 100,000 行 x 768 次元 |
| predicate | `pred_or2` | `WHERE lang = 'l0' OR lang = 'l1' ...`（OR 2 項） | 同上 |
| predicate | `pred_in2` | `WHERE lang IN ('l0','l1') ...`（IN 2 要素。診断用。Issue #1275） | 同上 |
| predicate | `pred_or_same` | `WHERE lang = 'l0' OR lang = 'l0' ...`（同一リテラルの OR。診断用。Issue #1275） | 同上 |
| predicate | `pred_in8` | `WHERE lang IN ('l0',...,'l7') ...`（IN 8 要素） | 同上 |
| order_by | `order_single` | `SELECT id, qty ... ORDER BY qty LIMIT 100` | 100,000 行 |
| order_by | `order_multi` | `ORDER BY lang ASC, qty DESC LIMIT 100` | 同上 |
| join | `join_inner` | `documents JOIN authors ON documents.author_id = authors.id LIMIT 100` | 各 10,000 行 |

- 規模は SQL-24／25／28 の数値基準の計測規模に合わせた（定数は `harness/relational_p95.rs`）
- `lang` は `id % 16`。選択率は `pred_eq` が約 6.25%、`pred_or2` が約 12.5%、`pred_in8` が約 50%
- `qty` は id 順と無相関の決定的な値（ソートが自明にならない）
- 結合キー `author_id` は右表 id の範囲に全件が一致する
- すべての fixture に他テナントの Private 行を混ぜる。計測前に各 arm を 1 回実行し、
  受理される・非空・他テナント行が混入しない・述語／順序／結合キーが期待どおり、を検査し、
  違反時は非 0 で終了する（RLS を外した状態や空振りを測らないための fail-closed）
- 所見: 現行のハッシュ結合（`sql/join.rs`）は結合キーのスカラー二次索引を参照しない

## 計測プロトコル

- N=5 ラウンド。ラウンド内の arm 順序は輪番（開始位置をラウンドごとにずらす）
- 1 ラウンドは warmup 20 回、計測 200 回。ラウンドごとの p95 と median を出力する
- arm ごとに min-of-N・median・max・ラン間幅を併記する。述語グループは `pred_eq` との min-of-N 比も出す
- 出力にテナント ID・行の値・SQL 全文は含めない。GitHub Actions 上では起動を拒否する

## 環境記録

- 計測対象コミット: `a355e607dbf16a6f84a5f77bfaf1ae72d15c546a`（本表はこのコミットで再測定した値。production コードは変更なし）
- CPU: 13th Gen Intel(R) Core(TM) i7-13700K（KVM 仮想化。共有環境）、論理 10 コア、ISA は Avx2Fma
- `BENCH_DEDICATED_ENV` は未申告（専有環境ではない）
- 同時実行プロセス: あり。共有開発機で他の Claude セッション等が稼働しており、専有していない（計測直前の
  `ps` 上位に高負荷プロセスは無かったが、他ジョブの実行は排除していない）
- ラウンド開始時の 1 分 loadavg は述語 2.78〜3.32、順序 1.98〜2.96、結合 1.64〜1.76

## 実測表（参考値・共有環境）

共有環境のため spec 閾値の確定判定には使わない（policy §5・§6）。ベンチも閾値行を
`not evaluated` と出力する（専有環境の申告があっても、規定の行数でない縮小規模は
`not evaluated (reduced scale ...)` とする）。`pred_eq` 比は候補の min を出したペアで直前に測った
参照 arm の p95 で割り、分母を `ref_paired` として併記する。単位は ms。

| arm | 行数 | round1 | round2 | round3 | round4 | round5 | min-of-N | median | ラン間幅 | `pred_eq` 比 |
| --- | ---- | ------ | ------ | ------ | ------ | ------ | -------- | ------ | -------- | ------------ |
| `pred_eq` | 100,000 | 0.871 | 0.783 | 0.759 | 0.723 | 0.778 | 0.723 | 0.778 | 20.4% | 1.0 |
| `pred_or2` | 100,000 | 36.094 | 27.685 | 35.392 | 27.176 | 34.609 | 27.176 | 34.609 | 32.8% | 37.6 |
| `pred_in8` | 100,000 | 6.403 | 6.530 | 6.580 | 6.479 | 6.545 | 6.403 | 6.530 | 2.8% | 8.9 |
| `order_single` | 100,000 | 21.979 | 22.717 | 27.604 | 35.972 | 39.017 | 21.979 | 27.604 | 77.5% | - |
| `order_multi` | 100,000 | 23.316 | 38.303 | 22.743 | 39.042 | 46.102 | 22.743 | 38.303 | 102.7% | - |
| `join_inner` | 10,000 x 10,000 | 8.987 | 8.964 | 9.376 | 9.589 | 9.749 | 8.964 | 9.376 | 8.8% | - |

- 順序グループはラウンド間で 2 倍近く変動した（ラン間幅が大きい）。共有環境のノイズと解釈し、
  min-of-N（`order_single` 21.979 ms・`order_multi` 22.743 ms）を参考値とする
- 所見: `pred_or2` は同じ選択率帯の `pred_in8` より約 4 倍遅く、`pred_eq` の約 38 倍である。
  OR 述語の評価経路に改善余地がある可能性があるが、本 Issue では調査・改善しない
- production コードは本ベンチ追加の前後で変更していない

## 再現手順

```bash
make bench-relational-p95
# グループ別（1 プロセス 1 グループ。policy §5）
BENCH_RELATIONAL_P95_GROUP=predicate make bench-relational-p95
# スモーク（縮小。scale=reduced と自己ラベルされ記録には使わない）
BENCH_RELATIONAL_P95_ROWS=5000 make bench-relational-p95
```

専有環境での本規模の再測定と閾値判定は「専有環境での再測定（Issue #1320）」節を参照する。

| 環境変数 | 意味 | 範囲・既定 |
| -------- | ---- | ---------- |
| `BENCH_RELATIONAL_P95_ROUNDS` | ラウンド数 | 5〜50、既定 5 |
| `BENCH_RELATIONAL_P95_GROUP` | `predicate`／`order_by`／`join`／`all` | 既定 `all` |
| `BENCH_RELATIONAL_P95_ROWS` | 行数（縮小のみ） | 1,000〜100,000、既定 100,000 |
| `BENCH_DEDICATED_ENV` | `1` で専有環境を申告 | 未設定は未申告 |

時間非依存の判定ロジックは `harness/relational_p95.rs` にあり、
`tests/relational_p95_accept.rs` が `make ci` で回帰検証する。

## スコープ外と申し送り

- 専有環境での再測定と spec 閾値の確定判定（オーナー作業。手順と記録の雛形は「専有環境での再測定（Issue #1320）」節に用意済み。実行自体は引き続きオーナー作業）
- `bench.yml` への配線（本ベンチは手動実行専用）
- 3 テーブル以上の結合と、結合でのスカラー `ORDER BY`／`OR`／`IN`
- `OFFSET`・`DISTINCT`・集計文の `ORDER BY` 形の p95
- NoSQL 表層の同等計測
- ハッシュ結合での二次索引の活用
- `pred_or2` の遅さの原因調査 → 実施済み（Issue #1275。下記「`pred_or2` の遅さの原因調査」）。改善の実装は別 Issue。F と c の分離は「述語経路の段別内訳（Issue #1319）」で実施済み

## `pred_or2` の遅さの原因調査

Issue #1275。SQL-24・SQL-2・RLS-10 ポインタ。production コード（`crates/engine/src/`）は変更せず、
診断用 arm 2 本と経路の自己検査をベンチへ足して原因を切り分けた。

### 方法

- 選択率と経路を直交させる診断用 arm を追加した。`pred_in2`（`IN` 2 要素。`pred_or2` と同じ選択率 12.5% で索引経路）と
  `pred_or_same`（同一リテラルの OR。`pred_eq` と同じ選択率 6.25% で OR 群の経路）である
- 計測前に全 arm を 1 回ずつ実行してキャッシュを温め、arm ごとに `EXPLAIN` の `scalar_plan:` トークンと
  カウンタ差分（`scalar_index_cache_stats`・`sql_arena_cache_stats`）を取って期待と照合する。
  期待と異なれば非 0 で終了する（出力は arm ラベル・トークン・カウンタ差分だけ）
- 再現: `BENCH_RELATIONAL_P95_GROUP=predicate make bench-relational-p95`（縮小は `BENCH_RELATIONAL_P95_ROWS=20000` を併用）

### 経路の確認

| arm | `scalar_plan` | `index_scans` | `index_trusted_mask_scans` | `full_rebuild_copies` |
| --- | ------------- | ------------- | -------------------------- | --------------------- |
| `pred_eq` | `index_equality` | +1 | +1 | +0 |
| `pred_in2` | `index_in_list` | +1 | +1 | +0 |
| `pred_in8` | `index_in_list` | +1 | +1 | +0 |
| `pred_or2` | `plain_scan` | +0 | +0 | +1 |
| `pred_or_same` | `plain_scan` | +0 | +0 | +1 |

差分はキャッシュを温めた後の 1 回の実行あたり。縮小規模（2 万行）と本規模（10 万行）で同じ結果だった。
`pred_in8`（選択率 50%）も索引経路のままで、選択度による切替は起きない。

Issue #1305（案 A の実装）以降は、`pred_or2` と `pred_or_same` も束縛時に `IN` へ畳まれ、次の経路になる
（縮小規模 2 万行で確認。経路自己検査は通る）。上の表は #1275 時点の値で、履歴として残す。

| arm | `scalar_plan` | `index_scans` | `index_trusted_mask_scans` | `full_rebuild_copies` |
| --- | ------------- | ------------- | -------------------------- | --------------------- |
| `pred_or2`（#1305 後） | `index_in_list` | +1 | +1 | +0 |
| `pred_or_same`（#1305 後） | `index_in_list` | +1 | +1 | +0 |

縮小規模（2 万行・共有環境・負荷平均約 12）の参考値: `pred_or2` の p95 は約 0.31 ms で `pred_in2`（約 0.30 ms）と同等になった。
専有環境の再測定ではないため、閾値判定には使わない。

### 実測

本規模（100,000 行 x 768 次元・N=5）。単位は ms。round 行の p95 を並べた。共有環境（10 論理 CPU・loadavg 約 9〜10。
専有環境の申告なし）のため参考値で、ラウンド間のばらつきが大きい。`perf_event_paranoid` は 4 で、
非 root の `perf record` は使えなかったため、経路の根拠は上記のカウンタと A/B arm とした。

| arm | round1 | round2 | round3 | round4 | round5 | min-of-N | median | ラン間幅 |
| --- | ------ | ------ | ------ | ------ | ------ | -------- | ------ | -------- |
| `pred_eq` | - | - | - | - | - | 0.637 | 0.788 | 926.0% |
| `pred_in2` | 1.588 | 1.561 | 2.227 | 3.133 | 5.251 | 1.561 | 2.227 | 236.5% |
| `pred_in8` | 6.651 | 7.280 | 7.926 | 9.339 | 6.883 | 6.651 | 7.280 | 40.4% |
| `pred_or_same` | 13.360 | 12.444 | 14.300 | 26.309 | 14.194 | 12.444 | 14.194 | 111.4% |
| `pred_or2` | 34.618 | 51.872 | 69.247 | 119.371 | 100.254 | 34.618 | 69.247 | 244.8% |

縮小規模（2 万行）でも同じ順序だった（min-of-N は `pred_eq` 0.166・`pred_in2` 0.306・`pred_in8` 1.030・
`pred_or_same` 1.168・`pred_or2` 1.779）。`pred_eq` の round 行は輪番で複数回現れるため、表では要約値だけを載せた。

### 分解

- 同じ選択率での経路の差: `pred_or2` と `pred_in2`（12.5%）は 34.6 ms と 1.56 ms で約 22 倍、
  `pred_or_same` と `pred_eq`（6.25%）は 12.4 ms と 0.64 ms で約 19 倍。選択率が同じでも OR 群の経路だけで
  1 桁以上遅く、遅さの主因は述語の意味や選択率ではなく経路である
- 索引経路の一致 1 行あたりの費用: `(pred_in8 - pred_in2) / 37,500 行` は約 0.14 us。
  OR 群の経路は `(pred_or2 - pred_or_same) / 6,250 行` が約 3.5 us で、約 25 倍になる
- `PlainScan` を「全件評価の固定費 F ＋ 一致 1 行あたりの費用 c」の線形モデルに当てはめると F が負になった
  （`2 x pred_or_same - pred_or2` が約 -9.7 ms）。費用は選択率に対して線形ではなく超線形で、
  この環境のノイズ（ラン間幅 100% 超）では F と c の比率を分離できない。固定費と複製費の内訳は
  専有環境の再計測か、段別のプロファイル（`filtered-distance-stage-profile.md` 参照）で確かめる必要がある

### 原因

`classify_scalar_plan` は OR 群（`or_filters` が空でない）を一律に `PlainScan` へ縮退させる
（`sql/scalar_plan.rs`。索引経路が OR 群の和集合に未対応のため）。このため `pred_or2` は次の 2 点で
`IN`・等価述語と異なる経路を通る。

1. 索引候補もスロットマスクも使わず、可視行の全件に対して行ごとの SCALAR 評価（マスク付きデコードと OR 分岐ごとの照合）を行う
2. 一致行の embedding を owned の `VectorArena` へ複製してから距離を計算する
   （`full_rebuild_copies` が +1。約 12,500 行 x 768 次元 x 4 B で約 38 MB）。索引経路はスナップショットの
   `VectorArena` を借用したまま探索するため複製しない

根拠は経路の確認の表（カウンタ・`EXPLAIN`）と、選択率を揃えた A/B arm の差である。
OR の各分岐は束縛段で再帰的に束縛される（`parser.rs` の `bind_where_predicates_recursive`）。
1 と 2 のどちらがどれだけ占めるかは上記のとおり未分離である。

### 判断

SQL-24 の数値基準（10 万本 x 768 次元で p95 100 ms 以下）に対し、`pred_or2` の min-of-N は今回 34.6 ms で
基準内である（前回記録は 27.2 ms）。ただしラウンド別の p95 は共有環境の負荷下で最大 119 ms まで上がった
ため、閾値判定は専有環境の再測定（オーナー作業）で確定する。現時点の結論は
「基準は共有環境の min-of-N では満たす。OR 群の経路は索引経路より約 20 倍遅く、改善は任意の最適化」とする。

### 改善案（案 A は Issue #1305 で実装済み。詳細は [sql-or-to-in-rewrite.md](./sql-or-to-in-rewrite.md)）

| 案 | 内容 | 効果の範囲 | 見積・リスク |
| -- | ---- | ---------- | ------------ |
| A（実装済み: Issue #1305） | 束縛時に、全分岐が同じ TEXT／ENUM 列の等価（または IN）1 件だけの OR 群を、1 本の `IN` フィルタへ書き換える（AND の連言に足すだけで意味は同じ。NULL は両形とも不一致） | `pred_or2` 型（同じ列の OR） | `EXPLAIN` の `scalar_plan` が `plain_scan` から `index_in_list` に変わる。`HINT ORDER` の経路・NoSQL の `or`・型付き等価の扱いを決める必要があり、2h を超える見込み |
| B | `ScalarIndex::resolve_candidates` に OR 群の分岐ごとの候補の和集合を足し、`ScalarPlan` に新しい variant を足す | 異なる列にまたがる OR、Issue #1165 のチャンク化した `IN (SELECT ...)` | 分類・`EXPLAIN` トークン・信頼マスクの不変条件の再証明が要り、規模が大きい |
| C | `PlainScan` でも hybrid でない距離順位付けは、複製しないマスク経路（`filter_cached_rls_rows_subset` と同じ形）へ載せる | 残余述語を持つすべての `PlainScan` クエリ（一致 1 行あたりの複製費） | 全件の SCALAR 評価は残る。中程度 |

A と B は全件評価と複製の両方を、C は複製だけを削る。いずれも 2h に収まらない見込みのため、
別 Issue として提案する（起票はオーナーの承認待ち）。着手前に、専有環境で F と c の内訳を再測定して案の優先度を決める。
F と c の分離手段は次節の段別計測で用意した。

## 述語経路の段別内訳（Issue #1319）

SQL-24・SQL-2 ポインタ。`perf record` が使えない環境（`perf_event_paranoid=4`）でも、プロセス内のタイマーと件数カウンタだけで
述語経路の段ごとの時間・件数を出す。述語グループの p95 行の後に、arm ごとの内訳を別ループで測って出力する
（既存の p95 行の意味・値は変えない）。

### 方式

- engine 本体（`crates/engine/src/`）は変更しない。段は pub API（`VectorArena::build_filtered_with_rows`・
  `row_codec::scan_scalar_columns_masked`・`declarative_filter::matches_all`・`ParallelSearchProvider::{search, search_subset}`）で
  ベンチ内に再実装する。`filtered-distance-stage-profile.md` の I 系列・W 系列と同じ方式で、段本体は `#[inline(never)]` に分離する。
  計測コードはベンチバイナリ（`make ci` の対象外）にだけあり、既定ビルドの engine の挙動・性能は構造的に変わらない
- engine のホットループへタイマーを入れる案（feature ゲート）は採らなかった。ホットパスへのコード混入と、
  結果・順序・エラーが変わらないことの別途証明が要り、リスクに見合わないため
- 毎ラウンド、計測前に各段の出力を fixture 規則（`lang` は `id % 16`）から独立に導いた期待と照合する。
  索引候補・PlainScan の一致 id 集合は期待 id と一致、Top-k は参照実装の距離順の期待と一致（同値境界は許容）すること。
  対象テナントの可視行スナップショットは RLS を通る pub API で捕捉し、行数が自テナントの行数と一致することも確かめる
  （他テナントの Private 行が混入しない）。違反は値を出さず非 0 で終了する。出力は全検査・全計測の完了後にまとめて行う
- 段ごとに 20 回の warmup と 20 回の計測を行い、全ラウンドのサンプルをプールして median・Q1・Q3（nearest-rank）・min を出す。
  `stage_round` 行は生データ（ラウンド別の中央値）、`stage_summary` 行がプール要約、`stage_diff` 行が median 同士の差である。
  差が逆転した場合は `n/a` とする。出力に tenant ID・行の値・SQL 全文は含めない

### 段の定義

2 系列を出す。`plain_scan_ref` は現 HEAD の述語 arm の実経路ではない。

| path | 段 | 内容 | 件数 |
| ---- | -- | ---- | ---- |
| `current` | `idx_candidate_resolve` | 値 → 値索引 → 候補スロットの辞書（計測外で構築）の lookup。単一値は複製のみ（整列なし。`ScalarIndex::candidates_for` の等価腕と同じ）、複数値（IN・OR 形）は連結 → 整列 → 重複除去（OR 形の実経路は分岐ごとの候補和集合のため、IN 形と同じ処理での近似） | 候補件数 |
| `current` | `idx_search_subset` | 候補をマスクにして借用した arena 上で `search_subset`（距離計算＋Top-k。複製なし） | 走査行数・k |
| `current` | `e2e` | 同ラウンドで測った同 arm の SQL 全体。`residual_median = e2e - 候補解決 - search_subset`（SQL 表層の固定費） | k |
| `plain_scan_ref` | `plain_scalar_eval`（F） | 可視行全件への `lang` 列だけのマスク付きデコードと述語評価（OR 形は分岐の any、IN 形は `in_list`、等価は 1 本） | 評価行数・一致件数 |
| `plain_scan_ref` | `plain_copy`（F＋c） | F と同じ評価に加え、一致行の embedding・id・tenant_id・visibility を owned バッファへ複製（`Vec::new()` から amortized 成長）。`c_median = plain_copy - plain_scalar_eval` | 一致件数・複製バイト数 |
| `plain_scan_ref` | `plain_search` | 複製済みバッファへ `search`（距離計算＋Top-k） | 一致件数・k |

`plain_scan_ref` は #1275 時点の PlainScan 経路（現在も、異なる列にまたがる OR は PlainScan に乗る）を反実仮想として再現したもので、
F と c を分離するための系列である。現 HEAD の `pred_or2` などは #1305 以降 `index_in_list` に乗るため、この系列に対応する e2e は無い
（`residual` は `current` 系列にだけ出す）。`vec_norm(embedding) > 0` で PlainScan を強制する方法は、余分なベクトル演算が混入するため使わない。

### 再現

```text
BENCH_RELATIONAL_P95_GROUP=predicate BENCH_RELATIONAL_P95_ROWS=20000 make bench-relational-p95
```

本規模（100,000 行 x 768 次元）では、スナップショット arena が engine 側のキャッシュとは別に約 307 MB 増える。
縮小規模（5,000 行）では `pred_in8` が PlainScan fallback を取り、経路自己検査で止まる（観測した事実のみ。原因は未調査）。2 万行以上で実行する。

### 参考値（縮小規模・共有環境）

2 万行 x 768 次元・N=5・各段 5 ラウンド x 20 回（サンプル 100 件）をプールした値。単位は us。
環境は i7-13700K（論理 10 コア）・loadavg 約 3.5・専有環境の申告なし・コミット `bf9888e6` 基準。
縮小規模かつ共有環境のため参考値であり、**閾値判定には使わない**。

`current` 系列（median。Q1〜Q3 は括弧内。`idx_candidate_resolve` の 2 値のみ実経路の手順へ合わせた再測定値で、他の列は初回測定のまま。`residual` の差は 1 us 未満）:

| arm | 候補件数 | `idx_candidate_resolve` | `idx_search_subset` | `e2e` | `residual` |
| --- | -------- | ----------------------- | ------------------- | ----- | ---------- |
| `pred_eq` | 1,250 | 0.09（0.09〜0.10） | 102.9（102.3〜107.1） | 158.5（155.1〜162.6） | 54.9 |
| `pred_or_same` | 1,250 | 0.10（0.09〜0.18） | 171.4（166.4〜175.1） | 161.0（156.7〜278.1） | n/a |
| `pred_in2` | 2,500 | 15.4（15.3〜22.3） | 212.4（183.9〜234.2） | 292.6（241.3〜381.3） | 64.9 |
| `pred_or2` | 2,500 | 15.1（15.0〜15.4） | 143.8（134.1〜193.2） | 274.5（224.4〜302.5） | 115.7 |
| `pred_in8` | 10,000 | 59.6（59.1〜62.5） | 384.7（355.2〜438.6） | 716.8（643.0〜768.4） | 272.5 |

`plain_scan_ref` 系列（median。Q1〜Q3 は括弧内）:

| arm | 一致件数 | F: `plain_scalar_eval` | F＋c: `plain_copy` | c（差） | `plain_search` | 複製バイト数 |
| --- | -------- | --------------------- | ------------------ | ------- | -------------- | ------------ |
| `pred_eq` | 1,250 | 542.5（536.9〜552.7） | 778.0（772.4〜783.6） | 235.5 | 72.2（71.5〜75.7） | 3,868,750 |
| `pred_or_same` | 1,250 | 624.3（619.3〜637.3） | 859.1（854.3〜866.3） | 234.8 | 73.7（73.0〜78.4） | 3,868,750 |
| `pred_in2` | 2,500 | 618.2（590.5〜1001.1） | 1,428.9（1,409.7〜1,683.7） | 810.7 | 155.2（138.1〜182.8） | 7,737,500 |
| `pred_or2` | 2,500 | 628.9（624.8〜637.5） | 1,442.6（1,425.8〜1,480.3） | 813.7 | 163.3（145.4〜192.2） | 7,737,500 |
| `pred_in8` | 10,000 | 748.1（742.4〜761.2） | 13,007.3（12,982.6〜13,049.8） | 12,259.2 | 371.2（336.4〜458.8） | 30,950,000 |

`pred_or_same` の `residual` は、e2e の median が候補解決と `search_subset` の合計を下回ったため `n/a` になった
（`search_subset` の再実装は SQL 経路より遅い側にぶれる。固定費が小さい arm では残差が見えない）。

### 所見

- F（全件の SCALAR 評価）は一致件数にほぼよらず 0.54〜0.75 ms で、一致行が増えても数十 % しか増えない。
  対して c（一致行の複製）は一致件数に対して超線形に増える（1,250 行で約 0.24 ms、2,500 行で約 0.81 ms、10,000 行で約 12.3 ms）。
  #1275 の線形モデルで F が負になった理由は、c が超線形で、2 点からの当てはめが成り立たなかったためと読める
  （amortized 成長に伴う再確保・ページ確保の費用が選択率とともに増える）
- 選択率が低い（6.25%）arm では F が全体の主役だが、選択率が高い arm（`pred_in8` の 50%）では c が F の約 16 倍になり支配的になる。
  改善案 C（複製しない経路）は一致件数が多い述語ほど効く。改善案 B（索引の和集合）は F と c の両方を削る。
  異なる列にまたがる OR のように PlainScan が残る形では F が残るため、C 単独の効果は c の分に限られる。優先度の確定は専有環境の再測定（オーナー作業）で行う
- 現 HEAD の索引経路（`current`）では、`idx_search_subset`（距離計算）と SQL 表層の固定費（`residual`）が大半を占め、候補解決は `pred_in8` でも約 60 us に収まる
- 上記の差・比率は共有環境の median 同士で、Q1〜Q3 の幅が広い段（`pred_in2` の F など）はノイズを含む。専有環境の再測定までは傾向としてのみ扱う

## 専有環境での再測定（Issue #1320）

SQL-24・SQL-25・SQL-28・SQL-2 の数値基準（ポインタ）の確定判定に使う、専有環境での再測定手順と記録の雛形である。
実行と判定はオーナーが行う（本節は手順書であり、本規模の実測値は含まない）。
絶対閾値の確定判定が専有環境でのみ可能である理由は [benchmark-judgement-policy](./benchmark-judgement-policy.md) §5 を参照する。
前例は [c1-p95-dedicated-env-reverification](./c1-p95-dedicated-env-reverification.md)。

### 1. 環境の条件と事前確認

- 専有の申告は `BENCH_DEDICATED_ENV=1`。値は trim 後に完全一致で `1` のときだけ申告として扱われる（`true` などは未申告）。
  自己申告なので、後述の記録欄の環境情報で専有性を裏づける
- 同時実行プロセスが無いことを、計測前と各グループの前に確認する

  | OS | 負荷の確認 |
  | -- | ---------- |
  | Linux | `uptime`、`ps -eo pcpu,comm --sort=-pcpu \| head` |
  | macOS | `sysctl -n vm.loadavg`、`ps -Ao pcpu,comm -r \| head` |

- CPU・周波数・電源を記録する

  | OS | 記録するもの |
  | -- | ------------ |
  | Linux | `lscpu`（Model name と Flags の avx2／fma／avx512 の有無）、`nproc`、`cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor \| sort \| uniq -c`、`/sys/devices/system/cpu/intel_pstate/no_turbo` または `/sys/devices/system/cpu/cpufreq/boost`（存在するほう）、ノート機は AC 給電（`/sys/class/power_supply/*/online`） |
  | macOS | `sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize`、`pmset -g`（`lowpowermode 0` と AC Power）、`sw_vers` |

  governor を変える場合はオーナーが一時的に変更し、終了後に元へ戻す。変更した事実は記録欄に書く。恒久的な設定変更は行わない
- macOS では `/proc/loadavg` が無いため、ベンチ出力の loadavg は `unavailable`（`env:` 行）／`n/a`（ラウンド行）になる。
  各グループの開始前と終了後に `sysctl -n vm.loadavg` を手で記録する
- ツールチェーンとコミット: `rustc --version`、`git rev-parse HEAD`、`git status --short`（`docs/spec` 以外が clean であること）
- メモリと所要時間: 本規模では 100,000 行 x 768 次元の embedding だけで約 307 MB あり、スナップショット arena が engine 側のキャッシュとは別に約 307 MB 増える。
  空きメモリに余裕のある機で実行する。所要時間の目安は後述「手順の完走確認」の値（縮小規模）から見積もる（本規模はより長い）

### 2. 本規模での実行

1 プロセスで 1 グループを実行する（policy §5。順序は predicate → order_by → join）。
`BENCH_RELATIONAL_P95_ROWS` は**設定しない**（既定の 100,000 が本規模。設定すると縮小規模になる）。
ラウンド数は既定の 5（policy §3 の N>=5）。増やす場合は `BENCH_RELATIONAL_P95_ROUNDS=<5..50>` を指定し、その値を記録する。

```bash
set -o pipefail
out="/path/to/logdir"  # 保存先ディレクトリの絶対パスへ書き換える（引用符は残す）
ts=$(date -u +%Y%m%dT%H%M%SZ)
for g in predicate order_by join; do
  BENCH_DEDICATED_ENV=1 BENCH_RELATIONAL_P95_GROUP=$g make bench-relational-p95 2>&1 | tee "$out/${ts}-${g}.log" || { echo "FAILED: $g"; break; }
done
```

- 終了コードが 0 以外のグループの値は記録に使わない（fail-closed）。原因は `relational_p95_bench: ...` 行（stderr）に出る。
  `GITHUB_ACTIONS` が設定された環境では起動を拒否する（実測値を public ログへ出さないため。CI へは配線しない）。環境変数が不正な値のときも終了コード 1 になる
- 生ログをリポジトリへ置く場合は `docs/design/bench-data/relational-p95-dedicated/` を使う。出力にテナント ID・行の値・SQL 全文は含まれないが、
  絶対パス・ホスト名・ユーザー名を足さない。末尾改行あり・行末の空白なしにする（editorconfig-checker の対象）

### 3. 段別内訳の採り方

段別内訳（Issue #1319）は predicate グループのプロセスで自動的に出力され、追加の環境変数は不要である。
`stage_summary` は全ラウンドをプールした median・Q1・Q3・min、`stage_diff` は `residual_median`（`current`）と `c_median`（`plain_scan_ref`。逆転時は `n/a`）、`stage_round` は生データである。
path と段の定義は「述語経路の段別内訳」節を参照する。抽出例（`<log>` は各ログのパス）:

```bash
grep -E '^relational_p95: group=.* min_of_n=' <log>        # arm 要約
grep -E '^relational_p95: group=.* round=' <log>           # ラウンド別 p95（生データ）
grep -E '^relational_p95: stage_(summary|diff) ' <log>     # 段別内訳
grep -E '^(env:|relational_p95_bench:|threshold_judgement:|path arm=)' <log>
```

### 4. 閾値判定の読み方

まず各ログの最終行 `threshold_judgement:` を確認する。判定に使えるのは、3 つのプロセスすべてで
`dedicated environment attested; compare min_of_n against the spec criteria manually` が出た場合だけである。

| 閾値行・終了状態 | 条件 | 扱い |
| ---------------- | ---- | ---- |
| `not evaluated (shared environment; reference values only)` | `BENCH_DEDICATED_ENV` が未設定、または `1` 以外 | 判定に使わない（申告を直して再実行） |
| `not evaluated (reduced scale; reference values only)` | 申告はあるが、predicate／order_by で行数が 100,000 でない、または join で 10,000 未満 | 判定に使わない（`ROWS` を設定していないか確認） |
| 閾値行なしで非 0 終了 | `GITHUB_ACTIONS` 設定、環境変数の不正、`path arm=` の経路自己検査失敗、RLS・fixture・段の期待照合の失敗 | 記録しない（原因を直して全体を再実行） |

arm と判定基準の対応（ID ポインタと数値基準のみ。条件の本文は spec を参照）:

| arm | 対応 ID | 判定基準（p95） | 規模 |
| --- | ------- | --------------- | ---- |
| `pred_or2`・`pred_in8` | SQL-24 | 100 ms 以下 | 100,000 行 x 768 次元 |
| `pred_eq` | SQL-2（参照 arm） | 100 ms 以下 | 同上 |
| `pred_in2`・`pred_or_same` | 診断用（#1275） | 判定対象外（参考として記録） | 同上 |
| `order_single`・`order_multi` | SQL-25 | 100 ms 以下 | 100,000 行 |
| `join_inner` | SQL-28 | 100 ms 以下 | 各 10,000 行 |

- 統計量は policy §3 を本手順で次のように読む。主統計量は `min_of_n`（ベンチ出力の案内どおり）、交差確認に `median` を使い、`max`・`run_to_run_band` も記録する
- 判定区分（新しい閾値は作らない）
  - **pass**: `min_of_n` と `median` がどちらも基準以内
  - **fail**: `min_of_n` が基準を超える
  - **要再測定**: `min_of_n` は基準以内だが `median` が基準を超える。環境ノイズを疑い、事前確認からやり直す
- 本規模・専有環境の値は確定判定に使える。専有性は自己申告であるため、記録欄（§6）の環境情報で裏づける

### 5. 任意: perf による補助プロファイル（Linux のみ）

p95 を記録する実行とは**別のセッション**で行う（perf の負荷が p95 を歪める）。perf の値は閾値判定に使わず参考扱いとする。
`perf_event_paranoid` の変更はオーナーが一時的に行い、自動化しない。

1. 現在値を控える: `cat /proc/sys/kernel/perf_event_paranoid`
2. 一時的に緩める: `sudo sysctl kernel.perf_event_paranoid=1`
3. シンボル付きでビルドする（Cargo.toml は変更せず環境変数で上書き）:
   `CARGO_PROFILE_BENCH_DEBUG=line-tables-only cargo bench --bench relational_p95_bench -p fandhe-vector-db-engine --no-run`。
   出力される `target/release/deps/relational_p95_bench-<hash>` を使う
4. 計測: `BENCH_RELATIONAL_P95_GROUP=predicate perf record -F 999 -g -o <出力先>/perf.data -- <バイナリ>`、続けて `perf report --stdio -i <出力先>/perf.data`
5. 終了後、控えた値へ必ず戻す: `sudo sysctl kernel.perf_event_paranoid=<元の値>`

macOS では perf を使えない。段別内訳（§3）で代用する。

### 6. 記録の雛形

計測 1 回につき以下をコピーして埋める。ホスト名・ユーザー名・絶対パス・資格情報は書かない。

環境記録:

| 項目 | 値 |
| ---- | -- |
| 計測日（UTC） | （未記入） |
| 機種・CPU（brand） | （未記入） |
| OS・arch | （未記入） |
| `logical_cpus`・検出 ISA（`env:` 行） | （未記入） |
| 周波数設定（governor・turbo／boost。macOS は lowpowermode） | （未記入） |
| 電源（AC） | （未記入） |
| メモリ | （未記入） |
| rustc | （未記入） |
| 計測対象コミット | （未記入） |
| `dedicated_env_attested`・rounds・rows | （未記入） |
| 各グループの loadavg（開始と終了） | （未記入） |
| 同時実行プロセスの確認結果 | （未記入） |
| 設定を一時変更した場合の内容と復元の確認 | （未記入） |

arm 別 p95（単位 ms）。`BENCH_RELATIONAL_P95_ROUNDS` を 5 より大きくした場合は、round6 以降の全値を「round6 以降」列へ `r6=…, r7=…` の形で漏れなく記入する（policy §3 の per-run 生データ）:

| arm | グループ | round1 | round2 | round3 | round4 | round5 | round6 以降 | `min_of_n` | `median` | `max` | `run_to_run_band` | `ratio_vs_pred_eq`・`ref_paired` |
| --- | -------- | ------ | ------ | ------ | ------ | ------ | ------------ | ---------- | -------- | ----- | ----------------- | -------------------------------- |
| `pred_eq` | predicate | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | - |
| `pred_or2` | predicate | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_in2` | predicate | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_or_same` | predicate | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_in8` | predicate | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `order_single` | order_by | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | - |
| `order_multi` | order_by | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | - |
| `join_inner` | join | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | - |

段別内訳（単位 us。median。Q1〜Q3 は括弧内）。`current` 系列:

| arm | 候補件数 | `idx_candidate_resolve` | `idx_search_subset` | `e2e` | `residual` |
| --- | -------- | ----------------------- | ------------------- | ----- | ---------- |
| `pred_eq` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_or_same` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_in2` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_or2` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_in8` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |

`plain_scan_ref` 系列:

| arm | 一致件数 | F: `plain_scalar_eval` | F＋c: `plain_copy` | c（差） | `plain_search` | 複製バイト数 |
| --- | -------- | ---------------------- | ------------------ | ------- | -------------- | ------------ |
| `pred_eq` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_or_same` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_in2` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_or2` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |
| `pred_in8` | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） | （未計測） |

判定（各グループの `threshold_judgement:` 行: predicate =（未記入）／order_by =（未記入）／join =（未記入））:

| arm | 対応 ID | 基準 | `min_of_n` | `median` | 判定（pass・fail・要再測定・対象外） | 備考 |
| --- | ------- | ---- | ---------- | -------- | ------------------------------------ | ---- |
| `pred_or2` | SQL-24 | 100 ms 以下 | （未計測） | （未計測） | （未判定） | （未記入） |
| `pred_in8` | SQL-24 | 100 ms 以下 | （未計測） | （未計測） | （未判定） | （未記入） |
| `pred_eq` | SQL-2 | 100 ms 以下 | （未計測） | （未計測） | （未判定） | （未記入） |
| `order_single` | SQL-25 | 100 ms 以下 | （未計測） | （未計測） | （未判定） | （未記入） |
| `order_multi` | SQL-25 | 100 ms 以下 | （未計測） | （未計測） | （未判定） | （未記入） |
| `join_inner` | SQL-28 | 100 ms 以下 | （未計測） | （未計測） | （未判定） | （未記入） |

判定後の申し送り: 改善案 B／C の優先度（F と c の比率から）の記録先と担当をここに書く。Issue の起票はオーナー判断とする。

### 7. 手順の完走確認（縮小規模・共有環境・参考）

本手順を、縮小規模（`BENCH_RELATIONAL_P95_ROWS=20000`）・共有環境・`BENCH_DEDICATED_ENV` 未設定で、
1 プロセス 1 グループの順（predicate → order_by → join）に実行して確かめた。専有の申告は共有機で行うと虚偽になるため付けていない。
数値は判定と混同しないよう雛形へは書かない。

確認できたこと（Linux・共有環境・コミット `1ab7726d` 基準）:

- 3 グループとも終了コード 0 で完走した。所要時間は predicate 約 45 秒、order_by 約 14 秒、join 約 6 秒（初回ビルド込みを含みうる。本規模はより長い）
- 各ログの最終行は `threshold_judgement: not evaluated (shared environment; reference values only)` だった
- §3 の抽出コマンドで、arm 要約行（predicate 5・order_by 2・join 1）、ラウンド別行、段別行（predicate のみ。`stage_summary`／`stage_diff`）、`env:` 行、`path arm=` 行が取れた
- 専有の申告ありで縮小規模の分岐（`reduced scale`）は、`tests/relational_p95_accept.rs::threshold_line_states_not_evaluated_when_shared` が `make ci` で検証している
- `GROUP=join` では `ROWS` が 10,000 以上なら `scale=full` と表示される（join の規定規模が各 10,000 行のため。仕様どおり）

確認できていないこと: macOS 経路（`sysctl`／`pmset` の手順と loadavg の表示）、perf 手順（§5）、本規模での所要時間・メモリ使用量。
