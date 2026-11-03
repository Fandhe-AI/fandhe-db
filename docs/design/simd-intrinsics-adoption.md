# ADR: intrinsics 導入方針（unsafe 境界・set 構築ロード・ディスパッチ設計・toolchain 1.98・適用経路）

- ステータス: Proposed（オーナー承認待ち。Issue #508 への承認コメントをもって
  Accepted。承認前に #509 以降の production 変更へ着手しない）
- 対応: Issue #508（親 #459・ルート #455。依存 #467・#468・#470・#463）
- 関連ポインタ: `docs/spec/04-behavior/core-engine.md`（CORE-9・CORE-10・CORE-11・
  CORE-12・CORE-14・CORE-16）・`docs/spec/05-tasks.md`（TASK-132・TASK-155・
  TASK-156）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/isa.rs`・`crates/engine/src/dispatch.rs`・
  `crates/engine/src/kernel.rs`（`dot`）・`crates/engine/src/hnsw.rs`
  （`HnswIndex.vectors`）・`crates/engine/src/sql/hnsw_cache.rs`
  （`prepare_full_visible`／`prepare_subset`／`search_prepared`）・
  `crates/engine/src/sql/hnsw_hybrid.rs`（`HnswDenseProvider`）・
  `crates/engine/src/batch_search.rs`（`pack_f16x2`）・
  `crates/engine/tests/isa.rs`・`scripts/check_simd_codegen.sh`（Issue #467）
- 関連 doc: [`chip-kernel-guidelines.md`](chip-kernel-guidelines.md)・
  [`hotpath-implementation-survey.md`](hotpath-implementation-survey.md)・
  `simd-codegen-guard.md`（Issue #467・PR #558 マージ待ち）・
  [`benchmark-judgement-policy.md`](benchmark-judgement-policy.md)・
  [`dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)・
  [`ann-index-adoption.md`](ann-index-adoption.md)・
  [`hnsw-generation-cache.md`](hnsw-generation-cache.md)・
  [`rrf-tie-break-determinism.md`](rrf-tie-break-determinism.md)・
  [`core16-f16-resident-gate.md`](core16-f16-resident-gate.md)・
  [`dot-kernel-branchless-tail.md`](dot-kernel-branchless-tail.md)（Issue #528）
- 本 ADR は Phase 4（親 #459）の実装 8 系統（#509〜#530）が個別に判断を再発明
  しないよう、intrinsics 導入の共通契約を 1 箇所に確定するものであり、
  `crates/` の実装コード変更・`rust-toolchain.toml` の設定変更は含まない
  （[out-of-scope-tracking](../../.claude/rules/out-of-scope-tracking.md)）

## 背景・目的

`crates/engine/src/isa.rs` は現行、`#[target_feature]` 付き safe fn と
`f32::mul_add` の自動ベクトル化に依存しており、`std::arch` の intrinsics
（`unsafe` を要する API）を一切使用していない。`unsafe` はトークン
（`Avx2FmaToken` 等）経由のディスパッチ呼び出し箇所のみに現れる。

[`hotpath-implementation-survey.md`](hotpath-implementation-survey.md) が
挙げる候補（f16 昇格・i8 VNNI・NEON dotprod 等）は、いずれも intrinsics の
**初導入**を伴う。`unsafe` 原則禁止・依存追加承認制（[coding-rust](../../.claude/rules/coding-rust.md)・
[dependency-policy](../../.claude/rules/dependency-policy.md)）のもとでこれを
安全に進めるには、後続の各カーネル実装 Issue が着手する前に、以下 6 点を
1 度確定しておく必要がある。

1. `unsafe` 境界（ポインタ load/store intrinsics 不使用・固定長配列からの
   `set` 構築ロード）
2. 生成コード検査ガード（Issue #467）の必須化
3. sealed トークンの拡張とディスパッチ設計
4. toolchain stable 1.98 系への追従方針
5. 適用経路の制約（既定エンジンとの分離）
6. macOS feature 検出（Issue #468）の結論の反映、AMX／SME／SVE の不採用

**自動運転モードでの位置づけ**: 本 ADR はオーナー承認を代筆できないため、
ステータスを Proposed のまま作成する。Issue #508 へは「ADR を作成した」旨を
コメントするに留め、承認コメントが残った時点でオーナー自身が本 doc の
ステータスを Accepted へ更新する（[`scalar-secondary-index.md`](scalar-secondary-index.md)
の先例と同じ運用）。

## 機械検証した事実

| # | 検証内容 | 環境 | 結果 |
| - | -------- | ---- | ---- |
| 1 | `#[target_feature(enable="avx2,fma,f16c")]` fn 内で `as_chunks` 固定長配列 → `_mm_set_epi16` → `_mm256_cvtph_ps` | rustc 1.98.0（x86_64）・`-O --emit asm` | `E0133` なし（safe）。生成コードは `vcvtph2ps -16(%rdi,%r8), %ymm1` の 1 命令へ畳み込み（`vpinsr*`／`vunpck*` 残留 0） |
| 2 | `#[target_feature(enable="avx2,avxvnni")]` fn 内で `_mm256_set_epi8` → `_mm256_dpbusd_avx_epi32` | 同上 | `E0133` なし。`{vex} vpdpbusd -96(%rdx,%r8), %ymm1, %ymm0` の 1 命令へ畳み込み |
| 3 | `_mm256_set_ps` → `_mm256_fmadd_ps` | 同上 | `E0133` なし。`vinsertps` 残留 0 |
| 4 | `_mm512_loadu_si512`（ポインタ load） | 同上 | `E0133`（unsafe 必須）。ポインタ load/store は本 ADR の禁止対象と整合 |
| 5 | 上記 1〜4 と同一の検証 | rustc 1.96.0（x86_64） | 同一結果（安定した挙動） |
| 6 | `vcombine_f32(vcreate_f32(bits), vcreate_f32(bits))`・`vsetq_lane_f32` 連鎖 | stable 1.96.0・`aarch64-unknown-linux-gnu`・`--emit asm` | `ldp`／`ldr` へ畳み込み（`ins` 残留 0） |
| 7 | `vreinterpret_f16_u16(vcreate_u16(bits))` → `vcvt_f32_f16`（`u16` ビット表現で f16 を扱う） | 同上 | `ldr` + `fcvtl` へ畳み込み（`f16` プリミティブ不要） |
| 8 | `vdotq_s32` | stable 1.96.0 | `E0658: stdarch_neon_dotprod`（rust-lang/rust #117224）でコンパイル不可 |
| 9 | CI（GitHub ホステッド runner）の実効 toolchain | main run 34044452736・PR #558 run 34036078490 | `rustc 1.98.1 (48a229cea 2026-09-01)` で green（`rust-toolchain.toml` の `channel = "stable"` が最新 stable へ解決された結果） |
| 10 | `cargo fmt --all -- --check`（origin/main ツリー） | `RUSTUP_TOOLCHAIN=1.98.0` | exit 0 |

検証 1〜8 の再現手順は `rustc +<version> --edition 2024 --crate-type lib -O
--emit asm`（x86_64）・`rustc +stable --target aarch64-unknown-linux-gnu
--emit asm`（aarch64）。1.98 系での aarch64 側の再検証（検証 8 の解消確認を
含む）は本 ADR の範囲外とし、CI の `cross-check` ジョブでの継続確認に委ねる。

## 決定 1: `unsafe` 境界

- ポインタを取る load/store 系 intrinsics（x86: `_mm*_loadu_*`・
  `_mm*_storeu_*`・`_mm*_maskz_loadu_*`。NEON: `vld1q_*`・`vst1q_*`）は
  **使用禁止**とする。ベクトルレジスタは `as_chunks::<N>()` で得た
  `&[T; N]` から `set` 系 API で構築する:
  - x86: `_mm256_set_ps`／`_mm512_set_ps`（f32）・`_mm_set_epi16`（f16 の
    ビット表現）・`_mm256_set_epi8`（i8）
  - NEON: `vcreate_*` + `vcombine_*` を第一候補、`vsetq_lane_*` 連鎖を
    第二候補
- `unsafe {` はトークンディスパッチ呼び出し箇所のみに限定する（新カーネル
  1 種につき 1 箇所）。直前 10 行以内に `SAFETY:` コメントを付け、
  根拠は「対応する sealed トークンを所持していること」とする。
  `crates/engine/tests/isa.rs` の `unsafe_is_confined_to_isa_module_with_safety_comments`
  が課す個数アサート（現行 3）は、カーネル追加と**同じ PR で**更新する
- カーネル本体（intrinsics を呼ぶロジック）は safe fn として実装し、
  `isa/*.rs` サブモジュールへ分離してよい。ただし `unsafe` は
  `isa.rs`（ファイル名一致）以外に持ち込めない（既存テストのファイル名判定
  を維持）
- `std::mem::transmute`・raw pointer キャストによるロードも禁止する。
  例外は `_mm_prefetch`（`as_ptr()` を渡す safe intrinsic であり、
  ロード/ストアではないため）

## 決定 2: set 構築ロードと生成コード検査ガードの必須化

- 「機械検証した事実」が示す 1 命令への畳み込みは LLVM の最適化挙動であり
  言語仕様上の保証ではない。新カーネルは**すべて**
  `scripts/check_simd_codegen.sh`（Issue #467）の対象カーネル表へ登録を
  必須とする（未登録はカバレッジガードで fail）
- 非 vacuous 検査（「期待命令が 1 件以上出現する」判定）は、カーネル種別
  ごとの期待命令へ一般化する必要がある（現行は「`vfmadd*ps` が 1 件以上」の
  固定判定）。f16 カーネルは「メモリオペランド付き `vcvtph2ps`」、i8
  カーネルは「`vpdpbusd`」を期待命令とする拡張は、#467 の後続タスクまたは
  各カーネル実装 Issue の受け入れ条件として個別に対応する
- aarch64: 新設する NEON カーネル（`fp16`／`dotprod` は baseline NEON
  外のため独立シンボルが残る見込み）には `#[inline(never)]` を付けて
  `--emit asm` からシンボルを検出可能にし、CI の `cross-check` ジョブへ
  禁止命令検査（`ins v.s[i]`／`ld1 {v.s}[i]` の残留チェック）を追加する
  ことを提案する。本 ADR は Issue 起票を代筆せず、「判断記録」節の
  要判断項目として記載するに留める
- tail（次元数がレーン幅の倍数でない端数）処理は、零埋め固定長バッファ
  ＋ `set` 構築で統一する。AVX-512 マスクロード（`_mm512_maskz_loadu_ps`
  等）はポインタ load を伴う unsafe API のため不採用とする

## 決定 3: ディスパッチ設計（sealed トークンの拡張）

| トークン | `enable` 文字列 | 検出条件 |
| -------- | ---------------- | -------- |
| `F16cToken` | `avx2,fma,f16c` | `is_x86_feature_detected!("avx2")` && `("fma")` && `("f16c")` |
| `AvxVnniToken` | `avx2,avxvnni` | `("avx2")` && `("avxvnni")` |
| `Avx512VnniToken` | `avx512f,avx512bw,avx512vnni` | `("avx512f")` && `("avx512bw")` && `("avx512vnni")` |
| `NeonFp16Token` | `neon,fp16` | `is_aarch64_feature_detected!("neon")` && `("fp16")` |
| `NeonDotprodToken` | `neon,dotprod` | `("neon")` && `("dotprod")` |
| `Avx512VpopcntdqToken` | `avx512f,avx512vpopcntdq` | `("avx512f")` && `("avx512vpopcntdq")` |

- いずれのトークンも既存の sealed トークン（`Avx2FmaToken` 等）と同じ
  設計に従う: フィールドは private・構築は `pub(crate) fn try_new() ->
  Option<Self>` のみ・`is_*_feature_detected!` マクロ以外のソースを
  参照しない。環境変数・設定ファイルによる外部上書き機構は持たない
  （`crates/engine/tests/isa.rs::isa_source_has_no_external_override_entry_points`
  が課す禁止事項の対象に含める）
- `dispatch.rs::simd_width_for`・`SimdKernel::dot`（f32 の幅ディスパッチ）
  は**変更しない**。新トークンは「幅」ではなく「機能」の可否を表すため、
  `F16Kernel`／`I8Kernel` 等の別 enum と個別の `OnceLock` ベース
  `current_*()` を並置する形で追加し、`dispatch.rs` の網羅 match・
  `scripts/check_core_api.sh` の公開 API snapshot に影響させない
- フォールバック契約: 対応トークンが取得できない環境では fail-closed で
  スカラー参照実装へ縮退する（f16 は `u16` ビット表現からのソフトウェア
  変換、i8 は i32 累積のスカラー計算）。候補生成スコアの ISA 間ビット
  一致は要求しない（同一プロセス・同一 ISA 内での決定性のみを要求する）。
  索引ヒットの最終スコアは常に `kernel::dot`（f32・既存演算順）で
  再計算するため、既存の決定性契約（[`rrf-tie-break-determinism.md`](rrf-tie-break-determinism.md)）
  は不変のまま維持される
- f16 の常駐ビット表現は `batch_search.rs::pack_f16x2` と同じ IEEE 754
  binary16（RNE 丸め）の `u16` に統一し、GPU 経路（`gpu_batch.rs`）と
  ビット表現を共有できる形にする（`f16` プリミティブは未安定のため
  `half` クレート等の依存追加は不採用）

## 決定 4: toolchain 1.98 系への追従

- `rust-toolchain.toml` は `channel = "stable"` のまま変更しない
- CI（GitHub ホステッド runner）は既に `rustc 1.98.1` で `rust-ci`
  （fmt/clippy/test/deny）が green（run ID は「機械検証した事実」検証 9
  を参照）であり、`vdotq_s32`（決定 3・`NeonDotprodToken` が前提とする
  安定化）は 1.98 で解決される
- ローカル開発環境（1.96.0 系が残っている場合）の `rustup update stable`
  は運用者（オーナー）作業とし、README の環境構築注記への反映可否は
  「判断記録」節の要判断項目とする（本 ADR 側からの先行反映はしない）

## 決定 5: 適用経路の制約

- 低精度常駐表現（f16・SQ8 i8）は `SearchEngineKind::Hnsw` の **opt-in
  経路限定**とする。唯一の適用 seam は `hnsw.rs::HnswIndex.vectors`
  （索引が保持するスナップショット）と `sql::hnsw_cache`
  （`prepare_full_visible`／`prepare_subset`／`search_prepared`）・
  `sql::hnsw_hybrid::HnswDenseProvider` である
- 索引ヒットのスコアは常に `kernel::dot` による f32 再計算（Issue #408 の
  既存契約）を維持する。overlay 差分の brute-force 補完・
  `MIN_INDEXED_ROWS` 未満の縮退・索引構築失敗時の縮退はいずれも f32 の
  ままとし、低精度表現の対象から外す
- 既定の検索エンジン（`ParallelBruteForce`／`CpuScalarBruteForce`）・
  `batch_search`・`rls.rs` の事前フィルタ経路には低精度常駐を適用しない。
  候補集合が変化すると `tests/hybrid_recall.rs` の層 A 固定値・
  `tests/sparse_cache_recall.rs` の cold/hot 等価性・
  `tests/sparse_determinism.rs` の決定性固定を破るため
- テナント境界: 低精度表現も `(table, PolicyContext)` × テーブル単位世代
  キーの索引エントリ内に閉じ、ctx 可視アリーナのみから構築する
  （[`ann-index-adoption.md`](ann-index-adoption.md) の「事後フィルタ
  不採用」判断を維持）。量子化パラメータ（min/max 等）は索引エントリ内に
  保持し、`EXPLAIN` へは静的パラメータ（常駐精度の種別）のみを露出する。
  実行時の縮退結果・可視カーディナリティ等のテナント存在情報に繋がる値は
  非露出のまま維持する（Issue #411 の契約を継承）
- f32 カーネル自体の変更（レーン内の演算順を変えない改善と、演算順が
  変わる改善)を既定エンジンへ適用する条件は以下のいずれかとする:
  1. 現行 `dot_lanes<LANES>` と**全入力に対してビット同一**であることを
     機械検証したうえで適用する
  2. 演算順が変わる場合は、Recall 3 ゲート（hybrid・rerank・
     query-planning）の同一閾値通過・層 A 固定値・cold/hot 等価性の
     再実測結果を添えてオーナー判断を個別に取る

  本条件の該当判断は「判断記録」節の要判断項目として列挙する。

## 不採用（理由付き）

| 候補 | 不採用理由 |
| ---- | ---------- |
| Intel AMX（`x86_amx_intrinsics`） | nightly 限定の feature gate |
| AArch64 SVE／SVE2（`stdarch_aarch64_sve`） | nightly 限定の feature gate |
| SME／SME2 | Rust API 自体が未提供。検出マクロも stable では利用不可 |
| `std::simd`（`portable_simd`） | stable 1.96〜1.98 でも `E0658`（未安定化） |
| 候補クレート simsimd／half／pulp／wide | 依存最小方針。自作トークン＋intrinsics で要件を満たせる（[dependency-policy](../../.claude/rules/dependency-policy.md)） |
| `#[target_feature]` 内でのポインタ load intrinsics | `unsafe` 面積の不要な拡大（決定 1 で禁止） |
| pgvector の `target_clones` 相当機構 | Rust コンパイラが同等機構を提供しない |
| Darwin 向け代替 feature 検出機構 | Issue #468 の結論（下記）により不要と判断 |

## Issue #468（macOS feature 検出）の反映

`aarch64-apple-darwin` では `neon`／`fp16`／`fhm`／`dotprod` がコンパイル時
target_feature として扱われるため `is_aarch64_feature_detected!` は
定数 true を返す。`bf16` のみ sysctl 経由の実行時検出が必要で、
`sme`／`sme2` は stable では検出マクロ自体が利用できない。結論として
本 ADR の決定 3 が定義するトークン（`NeonFp16Token`・`NeonDotprodToken`）
に対する代替検出機構の追加は不要である。詳細は
[`chip-kernel-guidelines.md`](chip-kernel-guidelines.md) §8
（静的解析＋GitHub ホステッド Apple Silicon 実機で確認済み）を参照。

## 実装タスク対応表

| Issue | 内容 | 本 ADR の決定番号 | 必須ガード |
| ----- | ---- | ------------------ | ---------- |
| #509〜#512 | f32 行ブロック化 | 決定 1・決定 2・決定 5（f32 条件） | `check_simd_codegen.sh` 登録・`tests/isa.rs` 個数更新。#510（AVX2+FMA・AVX-512F）・#511（NEON）実装済み（`docs/design/dot-kernel-row-block.md` 参照。決定 1 が明示許容する「新カーネル 1 種につきディスパッチ箇所 1 つの `unsafe`」の範囲で進めた。#512〔前後比較・採否〕は未実施） |
| #513〜#516 | f16 常駐（`F16cToken`／`NeonFp16Token`） | 決定 1・決定 2・決定 3・決定 5（ANN 限定） | 同上 + 期待命令表拡張。#514 実装済み（`docs/design/hnsw-f16-resident.md` 参照。#515〔Recall ゲート〕・#516〔前後比較実測〕は未実施） |
| #517〜#519 | 多アキュムレータ化 | 決定 5（オーナー判断条件） | 同上 + Recall ゲート再実測 |
| #520〜#523 | SQ8・VNNI（`AvxVnniToken`／`Avx512VnniToken`） | 決定 1・決定 2・決定 3・決定 5 | 同上。#521（対称 SQ8 量子化・i8 常駐表現）・#522（VNNI 512bit／256bit・i16 widen フォールバックの整数 i8×i8 dot カーネル。`Avx2FmaToken` を widen 経路に再利用し新規トークンは `AvxVnniToken`／`Avx512VnniToken` の 2 種のみ）実装済み（`docs/design/hnsw-sq8-resident.md`「Issue #522」節参照。#523〔Recall ゲート同一閾値検証・前後比較〕は未実施） |
| #524〜#526 | NEON dotprod（`NeonDotprodToken`） | 決定 3・決定 4（1.98 前提） | #525 実装済み（`docs/design/hnsw-sq8-resident.md`「Issue #525」節参照） |
| #2022 | popcount（`Avx512VpopcntdqToken`。AVX2 は `Avx2FmaToken` を再利用、NEON は `NeonToken`） | 決定 2・決定 3 | 実装済み（親 #2020・TABLE-48 ポインタ）。intrinsics は使わず `count_ones` の自動ベクトル化（AVX2: `vpshufb`＋`vpsadbw`、VPOPCNTDQ: `vpopcntq`、NEON: `cnt`）を生成コード検査で確認する。末尾 8 バイト未満は零埋め固定長バッファで 1 語として数え `vpinsrb` を避ける。AVX-512F 単独版は AVX2 版と同一コードになり検査が空振りするため採用しない |
| #527〜#529 | tail 処理統一 | 決定 2 | 同上 |
| #530 | 前後比較 | — | [`benchmark-judgement-policy.md`](benchmark-judgement-policy.md) §3〜§5 準拠 |

## 判断記録（オーナー記入欄）

| 項目 | 内容 |
| ---- | ---- |
| 判断 | （オーナー記入） |
| 根拠 | （オーナー記入） |
| 条件 | （オーナー記入） |
| 判断日 | Issue #508 の承認コメント日付を転記（本 ADR 単独では確定しない） |
| 記入者 | Issue #508 の承認コメント投稿者を転記（本 ADR 単独では確定しない） |

要判断項目:

- 決定 5 の f32 カーネル適用条件（ビット同一 vs. 演算順変更時の個別判断）
- aarch64 生成コードゲート（`cross-check` への `--emit asm` 追加・
  `#[inline(never)]` 付与）の Issue 起票可否
- README 環境構築節への `rustup update stable`（1.96→1.98 系）注記の
  追加可否

## スコープ外・申し送り

- `crates/` 配下のコード変更・実装そのもの（#509〜#530 が担当）
- `rust-toolchain.toml` の設定変更
- aarch64 生成コードゲートの実装（Issue 起票はユーザー承認制のため本
  タスクでは起票せず、上表の提案に留める）
- Issue #467（PR #558）のマージ後に確定する参照パス
  （`docs/design/simd-codegen-guard.md`）。Issue #468 側
  （`chip-kernel-guidelines.md` §8 の番号整理）は整理済み
- Apple 実機での intrinsics 生成コード実測

## 参照

- `docs/spec/04-behavior/core-engine.md`（CORE-9・CORE-10・CORE-11・
  CORE-12・CORE-14・CORE-16）
- `docs/spec/05-tasks.md`（TASK-132・TASK-155・TASK-156）
- 外部実装の参照・ライセンス帰属は [`chip-kernel-guidelines.md`](chip-kernel-guidelines.md)
  §6 を参照（重複転記を避ける）
