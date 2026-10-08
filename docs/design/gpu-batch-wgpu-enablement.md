# wgpu 導入と GPU バッチ経路の有効化

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 依存追加はオーナー承認済み（2026-08-26。承認記録は Issue #178 のコメント。
  `.claude/rules/dependency-policy.md`。承認事項は
  `crates/engine/Cargo.toml` のコメント・本文 §1 参照）
- 対応: TASK-128〜130（Issue #178。ポインタ: `docs/spec/05-tasks.md`）
- 関連ビヘイビア: CORE-6, CORE-8, CORE-16, EXT-2（ポインタ:
  `docs/spec/04-behavior/core-engine.md`・`extensions.md`）
- 前提: TASK-129（`batch_fallback.rs`。PR #148/#152/#154 でマージ済み。GPU 経路を
  CPU 上の参照実装 `GpuReferenceBackend` で実装していた）

## 1. 依存追加の承認事項

| 項目 | 内容 |
| ---- | ---- |
| クレート | `wgpu = { version = "=30.0.1", default-features = false, features = ["std", "vulkan", "metal", "dx12", "wgsl"] }`（`crates/engine/Cargo.toml`） |
| 目的 | Vulkan/Metal/DX12 の compute を単一 API で扱うため。各 GPU API を自作 FFI で叩くと依存最小方針に反して肥大化する |
| ライセンス | MIT OR Apache-2.0（本リポと同一） |
| メンテ状況 | gfx-rs/wgpu。crates.io の 30.0.1 系。`rust-version = 1.87.0`（本リポ toolchain は stable。充足） |
| 推移的依存 | 上記 feature 構成で新規 100 crate（`Cargo.lock` diff の `name =` 追加行で実測。`git diff <merge-base>..<この PR> -- Cargo.lock` で再現可能）。主要: `wgpu-core`/`wgpu-hal`/`wgpu-types`/`naga`/`ash`/`gpu-allocator`/`libloading`/`bitflags`/`thiserror`/`hashbrown`/`indexmap`/`smallvec`/`log`/`bytemuck`（`gpu-allocator` 経由の推移的依存。本体は `bytemuck` を使わず `to_ne_bytes`/`from_ne_bytes` でバイト変換する）/`parking_lot`・`parking_lot_core`（`wgpu-core` が間接的に要求し、features 指定では除外できない） |
| cargo-deny | `make deny` で `advisories ok, bans ok, licenses ok, sources ok`（warn: `hashbrown`/`syn` の重複バージョンのみ。`multiple-versions = "warn"` 方針どおり） |
| ビルド | `cargo check`／`cargo clippy --all-targets -- -D warnings` green。Vulkan ローダは `libloading` による実行時動的ロードのため、リンク時要件・CI runner への追加パッケージは不要 |

feature はゲート化しない（CORE-12: 経路を外部から上書きする機構を設けない方針。GPU
の可否は実行時の初期化結果のみで決まる）。

## 2. 設計方針

### 2.1 責務配置

```text
FallbackBatchEngine（batch_fallback.rs・CORE-8）
  ├─ primary: Box<dyn BatchBackend>  ← gpu_batch.rs::GpuBatchBackend
  │     ├─ 初期化失敗 → PrimarySlot::Unavailable（既存。FallbackEvent{Init}）
  │     ├─ 実行時エラー → runtime_latched + CPU 縮退（既存。FallbackEvent{Runtime}）
  │     └─ Ok(hits) → revalidate_primary_hits（既存 P0 防御。GPU 出力を信頼しない）
  └─ cpu: CpuFallbackMatrix（f32 原本）→ run_batch_search（既存・CORE-3 同一カーネル）
```

`FallbackBatchEngine::build_with_gpu`（新設）が `GpuBatchBackend::try_new` を
`backend_factory` として渡す。`GpuReferenceBackend`（CPU 参照実装）はテスト・
ベンチの配線疎通用に残し、`build_with_gpu_reference` も互換維持する。

### 2.2 GPU バックエンド（`gpu_batch.rs`）の契約

- プロセス共有の `GpuContext`（`Instance`/`Adapter`/`Device`/`Queue`/
  `ComputePipeline`）を `OnceLock` で 1 回だけ初期化する（初期化 `Err` も
  キャッシュし、以降の `try_new` は即 `InitFailed`）
- `InstanceDescriptor::new_without_display_handle()` を使い、`WGPU_*` 環境変数を
  読む `from_env` 系は使わない（CORE-12 と整合）
- `request_adapter(HighPerformance, force_fallback_adapter=false)`。
  `AdapterInfo.device_type == Cpu`（lavapipe 等）は `InitFailed` 扱い（「GPU
  経路」の capability として偽陽性にしない）
- `request_device` の `required_limits` は adapter の実測 limits をそのまま
  要求する。常駐行列（`packed()`）のバイト数が
  `max_storage_buffer_binding_size` を超える場合は `InitFailed`（CPU 縮退へ）
- `push_error_scope`/`pop`（LIFO）で Validation/OutOfMemory を捕捉する。scope は
  バッファ・bind group の生成前（dispatch）／シェーダ・パイプライン生成前
  （初期化）から張り、生成失敗も scope 内へ収める。加えて
  `device.on_uncaptured_error` で scope 外エラーをラッチし（既定ハンドラの panic を
  回避）、検知後の `batch_search` は backend エラーを返して CPU 縮退へ倒す。
  `set_device_lost_callback` でデバイスロストもラッチする
- `GpuContext`（`wgpu::Device`）はプロセス共有だが `GpuBatchBackend` は複数
  インスタンス存在しうるため、共有 Device への dispatch・バッファアップロードは
  **プロセス単位**のグローバルロックで直列化する（インスタンス単位の Mutex では
  別インスタンスからの並行操作を防げない）
- error scope の `pop()` は `device.poll` を駆動しながら待つ
  （`block_on_with_device_poll`）。自己ポーリングのみだとデバイスのポーリング待ちで
  無限スピンしうるため
- GPU の待機はすべて有限 deadline（`GPU_POLL_DEADLINE`）を持つ。初期化の
  `request_adapter`/`request_device` も同じ deadline で打ち切り、超過は
  `InitFailed`（＝CPU 縮退）へ写像する。readback は
  `PollType::wait_indefinitely()` ではなく有限タイムアウトの `Wait` を deadline まで
  繰り返し、`PollError::Timeout` は継続・deadline 超過は `DeviceLost` として返す
  （完了通知が停止しても CPU 縮退〔CORE-8〕へ移れるようにするため）。error scope の
  待機ループにも同じ deadline を共有する
- ステージング用のバイト列・readback の `f32` 列は `try_reserve_exact` で
  フォールブルに確保する（`Vec::with_capacity` の abort-on-OOM を避け、確保失敗も
  CPU 縮退可能な backend エラーとして返す）
- 計算量ガードは dispatch 前の `check_reachable_batch_work` が単独で担い、
  クエリごとの実到達行数（`is_visible` を満たす行数）× dim を合算して照合する。
  `gather_reachable_rows` 内には後段チェックを置かない: 全行ベースで課金すると
  CPU 経路では予算内の要求まで `Input` エラー（＝縮退対象外）で恒久的に拒否して
  しまううえ、`MAX_BATCH_ROWS × MAX_BATCH_DIM` < `MAX_BATCH_WORK` のため 1 クエリ分の
  課金がこの上限を超えることは構造的にありえず到達不能だったため
- WGSL は `const` 文字列で埋め込み（外部ファイル読み込みなし）。既定は
  `unpack2x16float`（コア機能。`shader-f16` 拡張不要）で
  `batch_search.rs::pack_f16x2` と同じビット解釈の f16 → f32 復元を行い、
  積和は f32 で行う（unpack 版）。`Features::SHADER_F16` 対応アダプタでは
  `select_dot_shader` が f16 パック常駐版（`enable f16;`。読み出した
  `vec2<f16>` を直後に `vec2<f32>` へ拡張してから積和するため算術自体は
  同じく f32・クエリが f16 へ厳密往復できない場合は unpack 版へ
  fail-closed に縮退）を自動選択する（詳細は Issue #539・
  `docs/design/gpu-batch-f16-arith.md` 参照）
- テナント境界・可視性判定は `policy.rs::PolicyContext::is_visible` の単一
  照合パスを CPU 側で使う（`gather_reachable_rows`）。GPU では積和のみを行い、
  テナント境界判定を GPU に持ち込まない
- 計算量 DoS 対策は `batch_search.rs::MAX_BATCH_WORK` と同じ定数を dispatch
  前に適用する（§3 の設計方針縮小により、`run_batch_search` のテナント別
  精緻化ではなく `rows × queries × dim` の粗い見積もりを使う。安全側）
- 非同期 API（`request_adapter`/`request_device`/`map_async`/
  `pop_error_scope`）は std-only の `block_on`（`Waker::noop()` +
  ポーリングループ）で同期化する（`pollster` 依存を追加しない）
- `unsafe`・`bytemuck` は使わない。バイト列変換は `to_ne_bytes`/
  `from_ne_bytes` のみ。バッファサイズは `checked_*`/`saturating_*` で導出する

workgroup 内部分 Top-k（readback 量削減。段階的 fail-closed 縮退を含む）は
[`gpu-batch-topk.md`](gpu-batch-topk.md) を参照（Issue #536 で実装）。

### 2.3 スコープ縮小事項（当初計画からの差分）

実装時間の制約により、以下は当初計画（`BatchPlan` によるテナント別グループ
dispatch・チャンク分割）から縮小した。挙動の正しさ・
安全性には影響しないが、性能特性・網羅性が計画より狭い:

- `batch_search.rs::run_batch_search` の内部（テナント別精緻化された work
  budget・行外側ループ）は変更していない。`GpuBatchBackend` は独自に
  `gather_reachable_rows`（クエリ単位で `PolicyContext::is_visible` を
  1 行ずつ判定）を実装しており、共通化（`BatchPlan`/
  `push_visible_candidate`/`finalize_selection` の抽出）は行っていない
- ~~dispatch はクエリ単位（バッチ内の複数クエリを 1 回の dispatch にまとめる
  最適化は行わない）。~~ **（2026-09-06 追記・Issue #532）** クエリを
  `PolicyContext` 単位にグループ化したうえで最大 `GPU_QUERY_TILE_MAX`
  （実装既定値 16）本を 1 dispatch へタイル化する方式へ変更した。各スレッド
  （行 1 つを担当）はレジスタ配列でタイル内の全クエリ分を同時に積算するため、
  常駐行列の行データはタイル幅ぶんのクエリで 1 回読みを償却する（`workgroup`
  共有メモリは使わない設計上の簡略化。詳細は `docs/design/hnsw-*.md` と同様の
  ADR 形式ではなく `gpu_batch.rs::DOT_SHADER_WGSL` のドキュメンテーション
  コメントに記載）。行数が `GPU_SCORE_BUFFER_BUDGET_BYTES`（32 MiB）を超える
  場合はタイル内で行チャンクへ分割する点は変わらない
- GPU バッファ（スコア/リードバック/クエリ）の呼び出し間再利用（CORE-15 の
  プール方針を GPU ステージングへ拡張）は行っておらず、呼び出しごとに
  確保・解放する
- `FANDHE_DB_TEST_REQUIRE_GPU` strict モード・`make test-gpu` ターゲットは
  未実装。結合テスト（`tests/gpu_batch.rs`）は GPU の有無を実行時に判定し、
  利用不能な環境では該当分岐を `eprintln!` で報告して早期 return する

### 2.4 選出結果の解決（`(tenant_id, id)` 契約への追随）

`BatchHit.hits` は PR #205/#228 で `CandidateHit`（行 id 単独）から `SearchHit`
（`(tenant_id, id)` で行を一意に解決できる型）へ変更されている。GPU 経路も
CPU 経路（`batch_search.rs::run_batch_search` の「選出後の独立再検証」）と同じ
`resolve_batch_slot` + `PolicyContext::is_visible` を通し、`finalize_gpu_hits`
（`gpu_batch.rs`）で解決する。

重要な帰結として、`TopKSelector` へ push する候補識別子は**行 id ではなく常駐
行列のスロット番号**でなければならない。`TopKSelector` の同点タイブレークは
候補識別子の昇順であり、`FallbackBatchEngine::revalidate_primary_hits` の順序
検証は「スコア降順・同点はスロット昇順」を契約とするため、行 id を使うと
`(tenant_id, id)` 順に並んだ通常のマルチテナント常駐行列で同点時に順序契約違反
となり、正当な結果まで `PrimaryResultRejected` で拒否される（再検証違反は CPU
縮退させず `Err` を返す設計のため、GPU 搭載環境で恒久的に失敗する）。

`finalize_gpu_hits` は GPU デバイスに触れないため、GPU 非搭載環境（CI）でも
順序・解決・fail-closed 挙動を単体テストできる（スロット昇順の確認・解決不能
スロットの拒否・不可視行の拒否・テナント跨ぎ同一 `id` の区別、および出力が
`revalidate_primary_hits` を通ること／行 id 順の出力が拒否されることの回帰）。

### 2.5 ベンチ A/B 実測配線（CORE-6/CORE-16 opt-in ゲート。Issue #178・#234）

`benches/batch_bench.rs` の `BENCH_CORE6`/`BENCH_CORE16` opt-in フラグは、
「実 GPU 未実装のため常に `pass=false`」という案内から実測経路へ置き換えた:

- CORE-6: 対照 = `BatchEngine::batch_search`（CPU-SIMD・f16 常駐）、被検 =
  `FallbackBatchEngine::build_with_gpu`。GPU が初期化できない環境・計測中に
  CPU 縮退（CORE-8）が発生した場合は「測定不能（`pass=false`）」とし、CPU 同士の
  比較値を GPU 実測の代替として計上しない
- CORE-16（**Issue #234 で対照経路追加済み**。Issue #178 は CORE-6 の充足で
  close 済み）: GPU 常駐コピーの f16 パックと f32 常駐の比較（ポインタ:
  `docs/spec/04-behavior/core-engine.md` CORE-16）。対照 A = `gpu_batch.rs::
  GpuF32ContrastBackend`（bench/テスト専用の対照経路。GPU 側に元の f32
  ベクトル列をそのまま常駐させ `unpack2x16float` を経由しない内積を計算する）、
  被検 B = 本番の f16 パック常駐 `GpuBatchBackend`。dispatch 本体
  （`dispatch_dot_products`）は両常駐形式で共通化し、行データの読み方
  （u32 パック解凍 vs f32 直接読み）だけが異なる WGSL シェーダを差し替える。
  どちらかの GPU 初期化が失敗した環境では「測定不能（`pass=false`）」とし、
  CPU 経路同士の比較値を GPU 実測の代替として計上しない（CORE-6 と同方針）
- CORE-6/CORE-16 の短縮率下限はそれぞれ `BENCH_CORE6_MIN_IMPROVEMENT_PCT` /
  `BENCH_CORE16_MIN_IMPROVEMENT_PCT`（Environment `bench-gate` secrets。
  Issue #286 で variables から repo secrets へ、その後 codex-review P0 指摘
  〔PR #299〕で Environment `bench-gate` へ移行）から注入し、未設定・非正値は
  fail-closed（値は spec が SSOT のため本リポにデフォルトを持たない）
- Apple GPU（Metal）環境での CORE-16 fail 報告（Issue #313）の切り分け・
  規模スイープ診断（`BENCH_CORE16_DIAG`）は
  `docs/design/core16-f16-resident-gate.md` 参照

## 3. ローカル実測（開発環境）

開発コンテナ内で GPU（NVIDIA、Vulkan backend）が実際に利用可能であることを
確認し、以下を実測した（値そのものは性能基準ではないため、pass/fail の記録
のみ。spec 由来の閾値は転記しない）:

- `GpuBatchBackend::try_new` → adapter/device 初期化成功（`DeviceType` が
  `Cpu` でないことを確認）
- 手計算した内積（4 次元・複数行）との一致を GPU dispatch 経由で確認
  （`crates/engine/src/gpu_batch.rs` の `gpu_batch_search_matches_hand_computed_dot_products_when_available`）
- 複数テナント混在バッチでのテナント混入 0 件・奇数次元でのパディング
  正しさ・CPU オラクル（`kernel.rs::CpuScalarProvider`）とのスコア一致
  （相対誤差 1e-3 以内）を確認
  （`crates/engine/tests/gpu_batch.rs`）
- `make bench-batch` の CORE-6 ゲート（`BENCH_CORE6` opt-in）が GPU 経路で
  完走し、CPU-SIMD 経路との A/B p95 を実測できることを確認した（縮退イベント
  0 件。閾値・判定値そのものは Actions secrets 経由で注入されるため本文には
  記録しない）
- GitHub ホステッド runner（CI）には GPU が無いため、CI では常に
  「初期化失敗 → CPU-SIMD 縮退」分岐のみが実走する。開発環境（GPU あり）で
  両分岐（成功・初期化失敗）を実際に確認する仕組みは持たない
  （`GpuBatchBackend::try_new` を強制的に失敗させる注入経路を作ると
  CORE-12 の「経路を外部から上書きする機構を設けない」方針に反するため）。
  初期化失敗分岐は `tests/batch_fallback.rs`（モック `BatchBackend`
  実装によるエラー注入）が別途カバーする

## 4. セキュリティ考慮（OWASP Top 10 + AGENTS.md P0）

| 観点 | 対応 |
| ---- | ---- |
| A03 インジェクション | WGSL は `const` 埋め込み。外部文字列からシェーダ・パラメータを組み立てない |
| A01 アクセス制御（テナント境界 P0） | GPU は積和のみ。可視性判定は CPU 側の `PolicyContext::is_visible` 単一照合パス。`FallbackBatchEngine::revalidate_primary_hits` が GPU 出力を独立再検証する（既存 P0 防御を維持） |
| A04 不安全な設計 / DoS | バッファサイズは `checked_*`・定数予算（`GPU_SCORE_BUFFER_BUDGET_BYTES`）で制限。計算量は `MAX_BATCH_WORK` を dispatch 前に適用。**（2026-09-06 追記・Issue #532）** クエリタイル幅は `GPU_QUERY_TILE_MAX`（シェーダ側 `const` と一致することをテストで固定）でホスト・シェーダ双方が `min` クランプする二重ガード |
| fail-closed / panic 禁止 | `unwrap`/`expect`/添字アクセスは使わない。wgpu のエラー・デバイスロスト・ポーリング失敗はすべて `Result` で伝播する |
| 情報漏えい | エラー/イベント文字列にテナント ID・クエリ・adapter 製品名を含めない |
| A05 設定ミス / CORE-12 | GPU 経路を強制・無効化する環境変数・feature flag を `src/` に設けない（`InstanceDescriptor::new_without_display_handle()`） |
| A06/A08 依存・サプライチェーン | `=30.0.1` 完全固定、`Cargo.lock` コミット、`cargo deny` green。オーナー承認済み（2026-08-26。承認記録は Issue #178 のコメント） |
| `unsafe`（P1） | 追加しない。`tests/isa.rs` の走査テストで機械的に担保（`unsafe` トークンが `gpu_batch.rs` に存在しないことを含む） |
