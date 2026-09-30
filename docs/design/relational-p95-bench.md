# 述語・順序・結合の p95 計測ベンチ

- ステータス: Accepted（計測入口の追加と、共有環境での参考値の記録）
- 対応: Issue #1204（親 #1206）
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

| 環境変数 | 意味 | 範囲・既定 |
| -------- | ---- | ---------- |
| `BENCH_RELATIONAL_P95_ROUNDS` | ラウンド数 | 5〜50、既定 5 |
| `BENCH_RELATIONAL_P95_GROUP` | `predicate`／`order_by`／`join`／`all` | 既定 `all` |
| `BENCH_RELATIONAL_P95_ROWS` | 行数（縮小のみ） | 1,000〜100,000、既定 100,000 |
| `BENCH_DEDICATED_ENV` | `1` で専有環境を申告 | 未設定は未申告 |

時間非依存の判定ロジックは `harness/relational_p95.rs` にあり、
`tests/relational_p95_accept.rs` が `make ci` で回帰検証する。

## スコープ外と申し送り

- 専有環境での再測定と spec 閾値の確定判定（オーナー作業）
- `bench.yml` への配線（本ベンチは手動実行専用）
- 3 テーブル以上の結合と、結合でのスカラー `ORDER BY`／`OR`／`IN`
- `OFFSET`・`DISTINCT`・集計文の `ORDER BY` 形の p95
- NoSQL 表層の同等計測
- ハッシュ結合での二次索引の活用
- `pred_or2` の遅さの原因調査
