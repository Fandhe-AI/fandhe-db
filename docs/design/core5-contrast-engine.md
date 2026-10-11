# ADR: CORE-5 対照エンジン接続（TASK-127・Issue #176）

- ステータス: Accepted（2026-08-26 オーナー承認。クレート採用〔`usearch =2.26.1`〕と
  CORE-5 の公開境界の双方を承認済み。spec 側の記録は
  `docs/spec/04-behavior/core-engine.md` CORE-5 の 2026-08-26 追記）
- 対応: TASK-127（`docs/spec/05-tasks.md`）・Issue #176
- 対象ビヘイビア: CORE-5（`docs/spec/04-behavior/core-engine.md`）
- 前提: TASK-127（性能・Recall 受け入れ基準の回帰テスト化。PR #143）で CORE-3/CORE-4
  は稼働済み。CORE-5 は対照エンジンクレート未導入のため `BENCH_CORE5` repo variable
  による opt-in（既定「対象外」）のまま未接続だった
- 関連: `docs/design/hybrid-recall-regression.md`（spec 閾値の Actions secrets
  注入パターンの先例。当初は variables を使っていたが Issue #286 で移行）

## 背景

CORE-5 のビヘイビア詳細（`docs/spec/04-behavior/core-engine.md` CORE-5）が SSOT
だが、対照エンジンクレートの導入が依存追加のユーザー承認を要するため
（`.claude/rules/dependency-policy.md`）、TASK-127 の初回実装（PR #143）では
判定ロジック（`harness/accept.rs::check_contrast_ratio_within_limit`）のみ先行実装し、
実測は `BENCH_CORE5=1` の opt-in でのみ「未接続＝判定不能」を fail-closed とする
運用にしていた。本 Issue #176 は対照エンジンクレートを実際に接続し、opt-in を撤去して
既定ゲートへ復帰させることが目的。

## 対照エンジンの選定

比較対象は総当たり（exact）検索でなければ比率がゲートとして意味を持たない
（ANN〔HNSW 等〕対 総当たりでは被検側〔`ParallelSearchProvider`〕が構造的に不利になる
ため）。crates.io 上の候補を、総当たり API の有無・ライセンス・メンテ状況・推移的
依存規模で比較した。

| 候補 | 総当たり API | ライセンス | 推移的依存 | 判定 |
| --- | --- | --- | --- | --- |
| **usearch** | `Index::exact_search`（公式 API） | Apache-2.0 | 約 25 | **採用** |
| hnsw_rs / instant-distance | なし（HNSW のみ） | MIT/Apache-2.0 系 | 小〜中 | 不可（比較不成立。ただし hnsw_rs は Issue #406 追記で HNSW-vs-HNSW 対照ベンチとして実測済み・撤去済み 2026-09-05） |
| arroy | なし（Annoy 近似）＋ LMDB C ビルド | MIT | 中 | 不可 |
| lancedb | あり | Apache-2.0 | 数百（arrow/datafusion/tokio 等） | 不可（依存最小方針に反する） |
| faiss | あり | MIT/Apache-2.0 | システム libfaiss 必須 | 不可（ホステッド runner で不成立） |

オーナー承認: 2026-08-26 に `usearch = "=2.26.1"`（Apache-2.0・bench 専用 optional
feature）の採用が承認された（`.claude/rules/dependency-policy.md`。承認記録は
Issue #176 のコメント）。

`usearch` を採用した理由:

- `exact_search` を公式 API として持ち、総当たり厳密最近傍の比較が成立する
- Apache-2.0（`deny.toml` の allow license に既存）
- 推移的依存が約 25 と、依存最小方針（`.claude/rules/dependency-policy.md`）の範囲で
  許容できる規模
- `MetricKind::IP`（内積）を指定でき、engine 側の内積スコア（`kernel.rs`）と Top-k
  順序を一致させられる

## 依存の載せ方: feature 限定 optional 依存

`usearch` を通常の `dependencies`（または `dev-dependencies`）にすると、
`make check-cross`（aarch64 クロス `cargo check --all-targets`）が usearch の
`build.rs`（C++ コンパイル）を aarch64 向けに走らせて失敗する（クロス C++
ツールチェーン未導入のため）。よって:

```toml
[dependencies]
usearch = { version = "=2.26.1", optional = true }

[features]
contrast-bench = ["dep:usearch"]

[[bench]]
name = "contrast_bench"
required-features = ["contrast-bench"]
```

とし、`contrast_bench.rs`（新設）にのみ `required-features` を付けた。`src/` 配下は
本 feature で一切 `cfg` 分岐せず、`wire-server` バイナリは usearch をリンクしない
（`cargo tree -p wire-server` に出現しないことを確認済み）。以前 `test-support`
feature を廃止した経緯（`crates/engine/Cargo.toml` コメント参照）はテナント境界
迂回 API の露出が理由だったが、本 feature は bench 専用の依存有効化のみで安全境界に
触れない。

**CI への影響**: CI（と任意の `make ci`）は `cargo clippy`/`cargo test` を `--all-features` で
実行するため、`contrast-bench` feature は PR ごとの CI・`make ci` で常時有効化され
usearch の C++ ビルドが走る（GitHub ホステッド `ubuntu-latest` には g++ が同梱済み。
初回 1〜3 分、以降はビルドキャッシュ次第）。ローカル実行にも C++17 コンパイラが必要
になる（README「回帰ベンチの Environment `bench-gate` secrets」に明記）。`deny.toml` は
`[graph] all-features = true` を追加し、optional 依存も advisories/licenses/sources/
bans の監査対象に含めた（fail-closed）。

## ベンチの構成: 独立バイナリへの分離

CORE-3/CORE-4（`simd_bench.rs`）と CORE-5 を別バイナリ（`contrast_bench.rs`）に
分けた。対照エンジン側（C++ FFI を含む）の障害・ビルド失敗が CORE-3/CORE-4 の
ゲートへ波及しないようにする failure domain 分離が目的で、`.github/workflows/
bench.yml` でも `bench-simd` と `bench-contrast` を別ジョブにしてある。

- 測定: `harness::ab::run_ab`（interleaved A/B）で A＝`ParallelSearchProvider::search`、
  B＝`ContrastIndex::search`（usearch `exact_search`）を同一データ・同一クエリで
  交互実行する（測定条件は `simd_bench.rs` と同一の `ROW_COUNT`/`DIM`/`TOP_K`/
  シード）
- 判定: `harness::accept::p95_ratio`（被検/対照の p95 レイテンシ比率）を
  `harness::accept::check_contrast_ratio_within_limit` で `BENCH_MAX_CONTRAST_RATIO`
  と突き合わせる（`AbMeasurement::median_ratio`〔`ab.rs` の既存契約〕は補助情報として
  標準出力へ併記するのみで判定には使わない）。この算出・判定ヘルパを public 実装として
  置くことはオーナー承認済み（2026-08-26。`docs/spec/04-behavior/core-engine.md`
  CORE-5 の「公開境界」追記が SSOT）。閾値の具体値は public 資産へ書かず Actions
  variable で注入し、bench の標準出力にも出さない
- 閾値注入: `BENCH_MAX_CONTRAST_RATIO` 環境変数（有限・正の浮動小数点）。未設定・
  不正値は fail-closed（`harness::accept::parse_contrast_ratio_limit`）
- 健全性チェック: 同一クエリでの Top-k 一致率（`recall_at_k`）を対照側と算出し標準
  出力へ併記する（配線ミス〔metric 取り違え・key 対応ずれ〕検出用）。**この一致率は
  合否判定に含める**——`contrast_bench.rs` の `MIN_TOPK_OVERLAP` 未満なら比率判定の
  結果によらず不合格とし（`overall_ok = ratio_ok && topk_overlap_ok`）、
  `(0.0, 1.0]` の範囲外も測定不成立として fail-closed とする。比率だけを見ると、
  metric 取り違え等で「速いが誤った結果を返している」計測を合格させてしまうため
  （下限値は本リポジトリ側で定めた実装定数で、spec 由来の数値基準ではない）

### スレッド条件の非対称性（既知の限界）

`ParallelSearchProvider` はスレッド並列＋SIMD カーネルで実行するのに対し、usearch
`exact_search` は 1 クエリを単スレッド SIMD で処理する。両者のスレッド条件を揃える
ことは本 Issue の範囲外とする（対照エンジンをそのまま使うのが CORE-5 の趣旨であり、
条件の対称化は将来オーナー判断で別 Issue とする）。

## `BENCH_CORE5` opt-in の撤去

`simd_bench.rs` から `core5_requested_from_env` と CORE-5 分岐を削除し、
`.github/workflows/bench.yml` から `BENCH_CORE5` 注入を削除、`bench-contrast` ジョブ
（schedule + workflow_dispatch）を追加した。`BENCH_MAX_CONTRAST_RATIO` repo variable
未設定のまま schedule run が実行された場合は fail-closed で red になる（既存の
CORE-3/CORE-4・CORE-6/CORE-16 と同一の「未評価 run が green として埋もれない」方針を
踏襲）。

## スコープ外・申し送り

- 対照エンジンとのスレッド条件の対称化（usearch exact search を複数スレッドで回す等）
- CORE-6/CORE-16（Issue #178）の opt-in は本変更で触っていない
- `BENCH_CORE5` repo variable の削除は管理者作業（残っていても参照されなくなるため
  無害）
- usearch を不採用とする判断があった場合は `harness/contrast.rs` のアダプタ差し替え
  で別クレートへ移行できる構造にしてある
- `BENCH_MAX_CONTRAST_RATIO` の値設定はマージ後の管理者作業（未設定のまま週次 run が
  走ると設計どおり fail-closed で red になる。variable ↔ spec ポインタの対応表・
  設定手順は `docs/design/ci-gate-variables.md` に集約した。Issue #286）
