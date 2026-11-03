# 他実装ホットパス比較調査（手法×実装×採否候補×ライセンス）

- ステータス: 調査記録（Informational）。production コード無変更。「採否」列は
  2026-09-05 時点の候補判定であり採用決定ではない（決定は各 Phase Issue・
  #508 intrinsics ADR が担う）
- 対応: Issue #470（Phase 1 親 #456・ルート #455）
- 前提: [`docs/design/crossdb-bench.md`](crossdb-bench.md)・
  [`docs/design/hnsw-parallel-build.md`](hnsw-parallel-build.md)・
  [`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)・
  [`docs/design/knn-two-stage-topk.md`](knn-two-stage-topk.md)・
  [`docs/design/hybrid-rrf-latency-breakdown.md`](hybrid-rrf-latency-breakdown.md)・
  [`docs/design/redb-insert-reserve-zero-copy.md`](redb-insert-reserve-zero-copy.md)・
  [`docs/design/hnsw-hybrid-iterative-scan.md`](hnsw-hybrid-iterative-scan.md)・
  [`docs/design/ann-index-adoption.md`](ann-index-adoption.md)

## 背景・目的

本リポ（Rust 製ベクトル DB・`crates/engine`）を「すべてのベンチマークで最速」に
近づけるための材料として、他実装（simsimd／usearch／hnswlib／faiss／qdrant／
pgvector／lance／DiskANN／tantivy）のホットパスを手法単位で比較し、本リポでの
採否候補・理由・ライセンスを整理する。2026-09-05 に実施した調査（Issue #470
コメント）の内容を、再利用しやすい形へ構造化して記録する。本 Issue は docs
専任であり `crates/` は無変更。

## 調査方法と制約

- 各リポジトリを shallow clone し、commit hash で内容を特定した（shallow clone
  のためタグ情報は参照していない）
- **外部実装のコードは転記しない**。出典はリポジトリ URL・commit hash・ファイル
  パスのみを記載する
- 本開発環境（QEMU Virtual CPU 報告・L2 48MiB/L3 16MiB という非現実的な階層）
  で行った速度比較実測は、run-to-run 変動が変更対象の差分と同程度かそれ以上
  あり性能判定に使えないことを確認した（Issue #365・#366 の既往の結論と整合）。
  そのため **本 doc の採否判定は本開発環境の実測値を根拠にしない**。チップ別の
  機械検証事実（`unsafe` 境界など環境非依存の事実）は
  [`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md) を参照

## 採否区分の定義

| 区分 | 意味 |
| ---- | ---- |
| 採用推奨 | 実測ギャップ・構造的理由から優先度が高いと判断した候補 |
| 条件付き | 前提条件（型追加・閾値調整・追加計測など）を満たせば採用候補になる |
| 不採用 | 技術的理由・契約上の理由で採用しない（ライセイン起因ではない） |
| 既採用 | 本リポが既に同型の設計を持つ |
| 既検討・不採用（Issue） | 過去に試作・実測し Rejected とした施策（§10 参照） |
| 既起票（Issue） | 既に別 Issue として起票済みの施策（§9 参照） |

## 調査対象実装とライセンス

調査対象は 9 実装。**GPL 系実装は 1 つも含まない**ため、ライセンス起因で
「参照のみ・採用不可」となる項目は存在しない（不採用判定はすべて技術的・契約上
の理由による）。pgvector の PostgreSQL License は BSD 系であり、依存クレートに
求める MIT OR Apache-2.0 デュアルライセンス配布との両立方針（`AGENTS.md` の
依存ライセンス規則。参照実装へ準用）を満たす。

| 実装 | URL | commit | ライセンス |
| ---- | --- | ------ | ---------- |
| simsimd (numkong) | https://github.com/ashvardanian/SimSIMD | `6de303b` | Apache-2.0 |
| usearch | https://github.com/unum-cloud/usearch | `f91fe5b` | Apache-2.0 |
| hnswlib | https://github.com/nmslib/hnswlib | `d9b3608` | Apache-2.0 |
| faiss | https://github.com/facebookresearch/faiss | `2ed4c10` | MIT |
| qdrant | https://github.com/qdrant/qdrant | `74f3e85` | Apache-2.0 |
| pgvector | https://github.com/pgvector/pgvector | `e48241b` | PostgreSQL License（BSD 系・採用可） |
| lance | https://github.com/lancedb/lance | `7d53161` | Apache-2.0 |
| DiskANN（現行は Rust 実装） | https://github.com/microsoft/DiskANN | `600c2b9` | MIT |
| tantivy | https://github.com/quickwit-oss/tantivy | `21ae09c` | MIT |

候補クレート（simsimd／half／pulp／wide／rayon 等）を本 doc・
[`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md) で言及する
箇所は情報提供のみであり、依存の追加・更新は `.claude/rules/dependency-policy.md`
のユーザー承認制に従う。本 Issue では依存を追加しない。

## 1. 距離カーネル

| 手法 | 実装 | 内容 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| アキュムレータ 1 本 | hnswlib／lance／simsimd(dot) | 1 レジスタ幅のみ | 既採用（`dot_lanes<LANES>` と同型） | Apache-2.0／Apache-2.0／Apache-2.0 |
| アキュムレータ 4 本（独立 FMA 鎖） | qdrant（全メトリック共通）／DiskANN `Strategy4x1` | 独立 FMA 鎖を複数本並走 | 既検討・不採用（Issue #365）の条件付き再訪。cache 常駐では dim>=768 で改善、arena 規模ではストリーミング律速で dim<=384 は中立〜悪化（§9-#10） | Apache-2.0／MIT |
| 次元別カーネル・ディスパッチ表 | faiss `distances_fused` | 次元ごとに専用命令列を持つ | 不採用。可変長 dim を 1 カーネルで扱う本リポ設計から乖離し保守コストが高い | MIT |
| `target_clones` によるコンパイラ任せ ISA 分岐 | pgvector `vector.c` | 自前 cpuid 判定なし | 参照のみ。GCC/Clang 拡張で Rust に対応構文なし | PostgreSQL License |
| ISA 判定を 1 回だけ解決しキャッシュ | lance `SimdSupport` enum／DiskANN（型構築時に 1 回） | 列挙／型で保持 | 既採用（sealed トークン `NeonToken`／`Avx2FmaToken`／`Avx512Token` ＋ `OnceLock`。本リポは検出漏れを型で構造的に排除できる点で優位） | Apache-2.0／MIT |
| 安全性 witness を境界 1 箇所へ集約 | DiskANN（ディスパッチ入口に `#[target_feature]` を集約し型の存在を不変条件として配下の `unsafe` を正当化） | — | 参照のみ。思想は本リポの sealed トークンと同一（本リポが先行採用済み） | MIT |
| スカラー残差 tail ループ | hnswlib／lance／qdrant／pgvector／faiss | — | 既採用（`zip().map().sum()` と同型） | 各種 |
| AVX-512 マスクロード tail | simsimd（skylake/icelake/genoa）／lance（cosine 固定次元） | `_mm512_maskz_loadu_*` で分岐なし端数処理 | 条件付き。マスクロードそのものはポインタ load のため `unsafe` を要するが、tail を零埋め配列へコピーしてから `set` する形なら safe（[`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md) §0.2 参照） | Apache-2.0 |
| SVE 述語による tail 吸収 | simsimd `sve.h` | `svwhilelt_*` | 参照のみ。SVE 対応自体がスコープ外 | Apache-2.0 |
| 次元別静的ディスパッチ（関数ポインタ事前選択） | hnswlib | dim%16/%4 で分岐 | 不採用。次元別関数の増殖で保守コスト増 | Apache-2.0 |
| f16 → f32 都度昇格 | simsimd／usearch（委譲）／pgvector `halfutils.c`／lance（フォールバック）／qdrant（AVX）／DiskANN | ネイティブ fp16 算術は使わず f32 へ昇格 | 条件付き。safe Rust・新規 `unsafe` ゼロ・依存追加ゼロで実装可能（[`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md) §0.2）。既定 brute-force への適用範囲の制約は §4 参照 | 各種 |
| ネイティブ fp16 SIMD 演算 | qdrant（NEON のみ C FFI）／AVX512-FP16 | — | 不採用。環境限定的、かつ C FFI 逃避は依存方針と衝突 | Apache-2.0 |
| f16/bf16 を C 実装へ FFI で逃がす | lance（build.rs で SIMD レベル別 C をコンパイル）／qdrant（NEON） | — | 不採用。自作方針・C 依存を持ち込まない | Apache-2.0 |
| AMX-FP16 タイル命令 | lance | — | 参照のみ・採用不可。ハード/OS 権限が特殊 | Apache-2.0 |
| i8 VNNI（4 要素同時整数内積） | simsimd `icelake.h`／`neonsdot.h`／lance `dot_u8.rs`／DiskANN | — | 条件付き。intrinsic 自体は safe だが、本リポに i8 量子化型が存在しないため量子化採用（§4）が先行条件 | Apache-2.0／MIT |
| VNNI 非対応時の i16 widen フォールバック | lance／qdrant | `_mm256_madd_epi16` | 参照のみ。i8 採用時の実装パターンとして記録 | Apache-2.0 |
| binary popcount（AVX512-VPOPCNTDQ） | simsimd／pgvector `bitutils.c`／lance `hamming.rs` | `_mm512_popcnt_epi64` | 条件付き。intrinsic 自体は safe。前提として binary 型の導入が必要 | 各種 |
| `count_ones()` を既定とし、バッチ経路のみ明示 SIMD | lance `hamming.rs` | LLVM 自動ベクトル化の POPCNT | 条件付き採用候補。本リポの「自動ベクトル化任せ」路線と最も整合。binary 距離追加時の第一候補 | Apache-2.0 |
| cosine を事前正規化して内積に帰着 | hnswlib（運用前提）／qdrant `CosineMetric::preprocess()` | — | 既採用（本リポの現行契約と同一方式） | Apache-2.0 |
| cosine を dot・‖a‖²・‖b‖² の 1 パス同時計算 | simsimd／usearch／pgvector／DiskANN | 3 アキュムレータ並行 | 参照のみ。本リポは正規化済み契約のため契約変更を伴う。変更を推奨する材料なし | 各種 |
| クエリ側のみ事前ノルム・候補側は選択可能な折衷 | lance `cosine_fast`／`cosine_with_norms` | — | 条件付き（優先度低）。本リポは既に両側正規化契約 | Apache-2.0 |

> 追記（Issue #1986）: 「cosine を事前正規化して内積に帰着」の行の「既採用」は、SQL-33 の改訂（`<=>` が pgvector と同じコサイン距離の昇順になった）により上書きされる。実装方式は [cosine-distance-method.md](cosine-distance-method.md)（Proposed）で比較している。上の表の記述は調査時点のまま残す。

出典: simsimd `6de303b` — `include/numkong/dot.h`, `spatial/{haswell,skylake,icelake,genoa,neon,sve}.h`, `neonsdot.h` ／ usearch `f91fe5b` — `include/usearch/index_plugins.hpp` ／ hnswlib `d9b3608` — `hnswlib/space_l2.h`, `space_ip.h` ／ faiss `2ed4c10` — `faiss/utils/distances_fused/{avx512.cpp,simdlib_based.cpp}` ／ pgvector `e48241b` — `src/vector.c`, `halfutils.c`, `bitutils.c` ／ lance `7d53161` — `rust/lance-linalg/src/distance/{dot.rs,cosine.rs,dot_u8.rs,hamming.rs}`, `simd.rs` ／ qdrant `74f3e85` — `lib/segment/src/spaces/{simple.rs,simple_avx.rs,simple_sse.rs,simple_neon.rs,metric_f16/,metric_uint/}` ／ DiskANN `600c2b9` — `diskann-vector/src/distance/simd.rs`, `diskann-wide/src/arch/`

## 2. Top-k 選出

| 手法 | 実装 | 内容 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| bounded max-heap（1-based 配列・swap レス sift） | faiss `Heap.h` | 代入のみで swap しない | 既採用（`BinaryHeap<Reverse<MinHeapItem>>` と同等） | MIT |
| 比較関数に id タイブレークを一体化 | faiss `ordered_key_value.h` | 全ヒープ操作経路で `(score cmp) \|\| (score==score && id cmp)` を保証 | 既採用（`MinHeapItem::cmp` が `total_cmp` → id 昇順を保証） | MIT |
| `BinaryHeap<Reverse<T>>` 固定長キュー | qdrant `fixed_length_priority_queue.rs` | 標準 `BinaryHeap` の薄いラッパー | 既採用（`TopKSelector` と同型・新規性なし） | Apache-2.0 |
| 早期棄却しきい値 | faiss `ResultHandler.h`／`HNSW.cpp` | 距離計算後、ヒープ追加の可否を分岐 | 既採用（`TopKSelector::push` は満杯なら候補 > 現在の k 位を確認してから push） | MIT |
| k=1 専用パス | faiss `Top1BlockResultHandler` | ヒープを使わず単一しきい値更新 | 条件付き（優先度低）。効果は大規模一括検索時のみで本リポは単発クエリ中心 | MIT |
| k と N の規模別切替（heap ↔ reservoir） | faiss `dispatch_knn_ResultHandler` | k に応じて heap／reservoir を切替 | 条件付き（優先度低）。本リポの k は数十〜数百中心。広域取得（#454）で k が大きくなる場合に再検討 | MIT |
| reservoir + bisection 分位点パーティション | faiss `ReservoirTopN`／`partitioning.cpp` | 容量約 2k のバッファ＋二分探索でしきい値を追い込む | 不採用。k が数百〜数千で優位になる手法で本リポの利用規模とミスマッチ。#366（2 段分離撤回）の知見とも整合 | MIT |
| SIMD ヒストグラム分位点 | faiss `simd_histogram_8/16` | AVX でしきい値カウントを高速化 | 不採用（上記 reservoir 不採用に付随） | MIT |
| 2k バッファ + `select_nth_unstable_by` | 本リポ BM25 側 | — | 既採用（疎索引側のみ・Issue #391）。ANN 側 `TopKSelector` はヒープのまま | — |

出典: faiss `2ed4c10` — `faiss/utils/Heap.h`, `ordered_key_value.h`, `partitioning.cpp`, `faiss/impl/ResultHandler.h`, `HNSW.cpp`, `faiss/utils/distances.cpp` ／ qdrant `74f3e85` — `lib/common/common/src/fixed_length_priority_queue.rs`

## 3. HNSW グラフ

| 手法 | 実装 | 内容 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| CSR 型フラット隣接配列 | faiss `impl/HNSW.h` | 全ノード・全レベルの隣接を単一 `Vec<idx>` へ連結し `offsets` から範囲算出。ノードごとの malloc なし | 採用推奨（§9-#5）。本リポの `Node { links: Vec<Vec<u32>> }` はノード×レベル分のヒープ確保が発生。`unsafe` 不要・依存追加不要 | MIT |
| level0 単一連続バッファ（インターリーブ） | hnswlib `data_level0_memory_` | リンク数＋リンク＋ベクトル＋ラベルを 1 要素として連続配置 | 条件付き（優先度中〜低）。prefetch 効果は大きいが本リポは `Arc<[f32]>` を別途一括所有する設計。永続化検討時に再設計する価値あり | Apache-2.0 |
| 可変長 per-node tape（アンアラインドアクセス） | usearch `index.hpp` | 専用アロケータで 1 回確保・ID 圧縮 | 条件付き（優先度低）。アンアラインドアクセスは Rust ではポインタキャスト＝`unsafe` を要し、faiss CSR で同等以上の局所性を安全に得られる | Apache-2.0 |
| ビット詰め圧縮リンク | qdrant `bitpacking_links.rs` | delta 符号化＋可変ビット幅パック | 条件付き（時期尚早）。永続化フォーマットとしては強力だが本リポは HNSW 永続化未実装 | Apache-2.0 |
| 世代カウンタ visited | hnswlib `visited_list_pool.h` | プール管理・巻き戻り時のみ全体初期化 | 既採用（`VisitedScratch`。プール化はスクラッチを都度渡す呼び出し規約で代替済み） | Apache-2.0 |
| ビットマップ visited | hnswlib／qdrant | 1 ノード 1 bit | 既採用（`VisitedBitmap`） | Apache-2.0 |
| サイズ閾値による HashSet／密配列切替 | faiss `impl/VisitedTable.h` | 可視カーディナリティで `HashSet` と versioned byte 配列を切替 | 条件付き採用（§9-#9）。マスク付き探索で可視カーディナリティが索引ノード数に対し極小のとき有効。標準 `HashSet<u32>` で依存追加・`unsafe` とも不要 | MIT |
| ソフトウェアパイプライン prefetch | hnswlib `searchBaseLayerST` | 候補 pop 直後に隣接の visited フラグ・ベクトル・次のリンクリストヘッダを prefetch | 条件付き採用（§9-#7）。`_mm_prefetch` は safe（[`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md) §0.2）。ただし PR #431（codex-review P0 是正）の「非受理ノードのベクトルへ一切触れない」契約（`hnsw.rs::greedy_descend_masked`／`search_layer` doc コメント・[`docs/design/hnsw-rls-cardinality-switch.md`](hnsw-rls-cardinality-switch.md)）により、prefetch は可視判定の後にのみ置ける制約がある | Apache-2.0 |
| visited 全件先読み後の 4-wide バッチ距離計算 | faiss `HNSW.cpp` | prefetch 発行と距離計算の 2 段分離 | 条件付き。prefetch 導入時の設計参考 | MIT |
| ユーザー注入可能な prefetch コールバック | usearch `dummy_prefetch_t` | 既定 no-op | 不採用。注入型パターンの重複でコストに見合わない | Apache-2.0 |
| mmap ファイル単位の非同期プリフェッチ | qdrant `schedule_prefetch` | OS ページ先読み | 不採用（永続化未実装のため対象外） | Apache-2.0 |
| per-(node, level) 独立 RwLock | qdrant `graph_layers_builder.rs` | 隣接ごとに個別ロック | 不採用。本リポは Issue #406 でノード単位 `RwLock` を採用し Recall 同水準・決定性を実測固定済み | Apache-2.0 |
| 要素単位ロック＋エントリポイント更新のみ排他 | pgvector `hnswinsert.c` | 通常挿入は index 全体の共有ロック、レベル超過時のみ排他昇格 | 既採用（Issue #406 が本方式を参考に設計。ページ概念のない本リポでは node 単位 `RwLock` が対応物） | PostgreSQL License |
| 複数エントリポイント保持＋フォールバック | qdrant `entry_points.rs` | 予備エントリポイントを保持 | 条件付き。本リポは Issue #409 の plain-scan フォールバックで同種の問題を吸収している可能性が高く、実測での不足確認が先 | Apache-2.0 |
| Vamana（medoid 起点＋α-RobustPrune・単層グラフ） | DiskANN | 階層構造を持たず接続性を保証 | 参照のみ・採用不可。HNSW（#404〜#413 実装済み・ADR Accepted）の置換は再設計コスト過大 | MIT |
| ef 既定値 | faiss／pgvector／qdrant／hnswlib | 停止条件はいずれも「候補集合の最小距離 > 結果集合の下限」型 | 既採用（本リポ M=16／efC=100／efS=64 は qdrant 寄り。停止条件の式も同型） | 混在（数値のみ参照） |

出典: hnswlib `d9b3608` — `hnswlib/hnswalg.h`, `visited_list_pool.h` ／ faiss `2ed4c10` — `faiss/impl/HNSW.h`, `HNSW.cpp`, `VisitedTable.h` ／ usearch `f91fe5b` — `include/usearch/index.hpp` ／ qdrant `74f3e85` — `lib/segment/src/index/hnsw_index/{graph_layers.rs,graph_layers_builder.rs,entry_points.rs,graph_links/serializer.rs}`, `lib/common/common/src/bitpacking_links.rs`, `lib/segment/src/types.rs` ／ pgvector `e48241b` — `src/hnswinsert.c`, `hnswutils.c`, `hnsw.h` ／ DiskANN `600c2b9` — `diskann/src/graph/start_point.rs`

## 4. 量子化・圧縮

共通前提: 本リポは「返却スコアの f32/f64 ビット一致」を強い契約として持つ
（`tests/hybrid_recall.rs` 層 A 固定値アサーション・`sparse_cache_recall.rs` の
cold/hot 等価性）。量子化を「候補生成のみ・最終スコアは f32 で再計算」に閉じれば
返却スコアのビット一致は保てるが、**候補集合そのものが変わりうる**（近似で真の
Top-k を取りこぼしうる）。したがって量子化が適用できるのは既に近似を受け入れて
いる HNSW opt-in 経路のみであり、既定 brute-force エンジンには適用不可。

| 手法 | 実装 | 既定パラメータ | 本リポでの採否 | ライセンス |
| ---- | ---- | -------------- | -------------- | ---------- |
| f16／bf16 | faiss `QT_fp16`/`QT_bf16`／usearch `f16_k`/`bf16_k`／pgvector `halfvec` | 学習不要（型変換のみ） | 条件付き（§9-#8）。ANN opt-in 経路の常駐形式としてのみ。移動バイト半減。実装は新規 `unsafe` ゼロで可能。GPU 側の f16/f32 常駐比較（Issue #313）は Apple GPU 側が未検証で未決着——CPU 常駐版として仕切り直しが必要 | MIT／Apache-2.0／PostgreSQL License |
| SQ8（対称） | qdrant `encoded_vectors_u8.rs`／faiss `QT_8bit` | 次元ごと or 全次元共有の min/max | 条件付き（f16 より優先度低）。圧縮率は高いが誤差が大きく Recall 実測が前提 | MIT／Apache-2.0 |
| SQ8（非対称・query は f32 のまま） | lance `sq/builder.rs` | グローバル `bounds` | 条件付き。非対称の方が候補選定の質は対称 SQ8 より有利 | Apache-2.0 |
| SQ4／binary(1bit) | faiss `QT_4bit` 系／qdrant `encoded_vectors_binary.rs` | XOR+popcount | 条件付き（低優先）。圧縮率最大だが Recall 劣化リスク大。本リポの候補幅（M=16/ef=64）は小さく候補生成コストが支配的でないため効果限定的 | MIT／Apache-2.0 |
| PQ／OPQ | faiss `ProductQuantizer`／qdrant／lance `pq/builder.rs` | k-means コードブック学習が前提 | 不採用（当面）。コードブック学習・LUT 構築の自作コストが高く、依存追加禁止下で ROI が低い。25k〜100k 規模では圧縮の必要性自体が薄い | MIT／Apache-2.0 |
| PQ4 fast-scan | faiss `impl/fast_scan/` | AVX2/AVX-512 前提の LUT 参照 | 不採用（上記 PQ 不採用と連動） | MIT |
| RaBitQ | faiss `RaBitQuantizer`／lance `bq.rs` | ランダム回転は外部適用前提 | 条件付き（将来）。理論的誤差保証は魅力だがランダム回転等の自作コストが高い | MIT／Apache-2.0 |
| 近似 → 再ランク（rescoring） | faiss `IndexRefine`／qdrant `oversampling`／DiskANN `Quantized` ストラテジ | — | 既採用。本リポの HNSW は「索引ヒットのスコアは常に `kernel::dot` で再計算する」契約を既に持ち、これが rescoring パターンそのもの。量子化追加時もこの契約を候補生成側へ拡張するだけで整合 | MIT／Apache-2.0／MIT |

出典: faiss `2ed4c10` — `faiss/impl/ScalarQuantizer.{h,cpp}`, `ProductQuantizer.h`, `Clustering.h`, `impl/fast_scan/`, `utils/quantize_lut.cpp`, `IndexRefine.{h,cpp}`, `impl/RaBitQuantizer.h` ／ qdrant `74f3e85` — `lib/segment/src/types.rs`, `lib/quantization/src/encoded_vectors_{u8,pq,binary}.rs` ／ usearch `f91fe5b` — `include/usearch/index_plugins.hpp` ／ pgvector `e48241b` — `src/halfvec.h` ／ lance `7d53161` — `rust/lance-index/src/vector/{quantizer.rs,pq/builder.rs,sq/builder.rs,bq.rs,ivf/builder.rs}` ／ DiskANN `600c2b9` — `diskann/src/graph/strategy.rs`, `utils/vector_repr.rs`

## 5. フィルタ付き探索

| 手法 | 実装 | 内容 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| カーディナリティ推定 → plain scan／ANN 切替 | qdrant `full_scan_threshold` | 統計＋サンプリング推定で確定できない曖昧域を扱う | 既採用（`full_scan_ratio` 既定 1/10・Issue #409）。本リポは RLS 事前フィルタで正確な可視カーディナリティが既知のため qdrant 型のサンプリング推定は不要 | Apache-2.0 |
| 探索時グラフ内マスク | qdrant `FilteredScorer` | 不適合ノードをスコア対象から除外 | 既採用（`NodeMask`／`search_masked`・Issue #409） | Apache-2.0 |
| ACORN-1（2-hop 展開） | qdrant `search_on_level_acorn` | 1-hop 不適合ノードのみ 2-hop まで展開。低選択性フィルタでのみ有効化 | 条件付き採用（§9-#3b）。`hnsw_subset` 経路が既定比 37〜45% 悪化する実測（Issue #413）の改善策。#500 で契約整理済み（条件付き成立。ベクトル非参照〔I1・P0〕は維持したまま、リンク非参照〔I2〕のみゲート下で緩和する設計。`docs/design/hnsw-rls-cardinality-switch.md`「Issue #500」節）→ 実装 #501・実測 #502 | Apache-2.0 |
| 破棄候補ヒープ保持型 iterative scan | pgvector `hnswscan.c` | 破棄候補を pairing heap に保持し以後バッチ単位で再開する（ef 倍増による再実行ではない） | 条件付き採用（§9-#3c）。本リポの hybrid 密側再取得ループ（Issue #410）は `dense_fetch_k` 倍増で再実行するためラウンドごとに同じ候補を再評価しており、pgvector 型の継続方式は計算重複を避けられる | PostgreSQL License |
| ビルド時のフィルタ専用追加サブグラフ | qdrant `hnsw/build.rs`（`payload_m`/`payload_m0`） | カーディナリティの大きい payload ブロック専用サブグラフをマージ | 不採用（当面）。本リポの索引は「ctx 可視アリーナのみから構築」という per-テナント前提であり、複数テナント/条件を跨ぐグローバル索引最適化とは前提が異なる | Apache-2.0 |
| `IDSelector` による中間フィルタ | faiss `HNSW.cpp::search_from_candidates_fixVT` | 距離計算は先に行い `is_member` 判定は結果ヒープ追加の可否のみに使う | 不採用（P0 違反）。不適合ノードにも距離計算＝ベクトルアクセスが発生し、PR #431 の「非可視ノードのベクトルへ一切触れない」契約に反する | MIT |
| 事後フィルタ＋oversampling | pgvector README | 低選択性で結果不足が起きる旨を明記し `iterative_scan` を案内 | 不採用（既決・P0）。[`docs/design/ann-index-adoption.md`](ann-index-adoption.md) が「不可視行の存在が候補集合・処理量・応答時間へ影響し、存在情報の副次漏えい経路になる」として既に不採用と判断済み | — |

出典: qdrant `74f3e85` — `lib/segment/src/types.rs`, `index/hnsw_index/hnsw/build.rs`, `index/hnsw_index/graph_layers.rs`, `index/sample_estimation.rs` ／ faiss `2ed4c10` — `faiss/Index.h`, `impl/IDSelector.h`, `impl/HNSW.cpp` ／ pgvector `e48241b` — `src/hnsw.c`, `hnsw.h`, `hnswscan.c`, `README.md`

## 6. 投影・デコードの遅延

| 手法 | 実装 | 構造 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| Top-k 確定後にペイロード／ベクトルを取得 | qdrant（ID+スコア確定後に payload_storage から取得）／lance（`Sort`→`Limit`→`TakeExec`）／faiss（`search()` 後に `reconstruct()`） | 3 実装が共通して「スコアリングは ID+スコアのみ → 確定後に必要な列を取得」の 2 段構造 | 採用推奨（§9-#1）。本リポの Top-k 確定**前**に全可視行のスカラー列をデコードする固定コスト（Issue #453）に直接効く。設計上の注意: `WHERE` 事前フィルタ・`ORDER BY` 式が参照する列とRLS 可視判定に必要な最小フィールドは確定前デコードが必要（投影列とフィルタ列の区別が要る） | Apache-2.0／Apache-2.0／MIT |
| 列指向フォーマットでの列単位遅延デコード | lance `lance-encoding` | 真の列指向・`column_indices` が末端まで伝播 | 条件付き（将来）。本リポの集計経路には既に同発想の `ReferencedColumns`/`DecodeTier`（Issue #350・3 段階デコード）がある。lance 相当への全面移行は `row_codec` 大改修。まず既存 `DecodeTier` の考え方を SELECT/Top-k 経路へ一般化する方が現実的 | Apache-2.0 |
| TID 逐次返却型 heap fetch | pgvector `hnswgettuple` | 索引は TID のみ返し実体は AM 層の外で 1 件ずつ消費 | 不採用（移植困難）。本リポの `VectorArena` は embedding＋メタデータ一体保持で、二層アーキテクチャの前提を持たない | PostgreSQL License |

出典: qdrant `74f3e85` — `lib/segment/src/segment/read_view/search.rs` ／ lance `7d53161` — `rust/lance/src/dataset/scanner.rs`, `io/exec/take.rs`, `dataset/take.rs`, `rust/lance-encoding/src/decoder.rs` ／ pgvector `e48241b` — `src/hnswscan.c` ／ faiss `2ed4c10` — `faiss/Index.{h,cpp}`

## 7. BM25／疎索引

共通制約: 本リポは (1) BM25 スコアの f64 ビット一致、(2) 同点タイブレーク
（score 降順→id 昇順）で k 位と同点の候補すべてに到達する完全性、(3) 可視集合内
で df/avgdl/N を再計算するテナント境界縮約契約、の 3 つを同時に満たす必要がある。

| 手法 | 実装 | 内容 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| ブロック単位 bitpacking posting 圧縮 | tantivy `postings/compression/` | 128 doc/block、doc_id は strictly-sorted delta、term_freq は別ストリーム | 不採用（現状維持）。posting レイアウト全面刷新＋依存追加を要する。Issue #388〜#392 で build/search とも大幅高速化済みで費用対効果が不明 | MIT |
| skip list（ブロックヘッダ） | tantivy `postings/skip.rs` | ブロック内最大 doc_id・doc/tf のビット幅・block-max 用ペア | 不採用。bitpacking ブロック構造の導入が前提 | MIT |
| block-max WAND | tantivy `query/boolean_query/block_wand_union.rs` | pivot doc をブロック最大値の累積和から特定し早期打ち切り | 不採用（契約と非両立）。ブロックに焼き込む構造データは可視集合が変わるたび作り直しが要りテナント境界縮約契約と衝突。加えて pruning は厳密不等号判定で「k 位と同点の候補すべてに到達する」タイブレーク完全性契約と根本的に相容れない | MIT |
| fieldnorm 256 段ロッシー量子化 | tantivy `fieldnorm/code.rs` | 1 byte・256 段の固定テーブル | 既検討・不採用（Issue #391）。ビット一致契約違反そのものが理由。本リポは非ロッシーな厳密クラス表を採用済み | MIT |
| BM25 tf 事前計算テーブル | tantivy `query/bm25.rs::compute_tf_cache` | fieldnorm_id 0..256 についてスコア項を事前計算 | 既採用（Issue #391 の `len_classes`/`doc_len_class`）。本リポは非ロッシーで段数を実際に出現する文書長の個数まで上げた厳密版であり、決定性契約への適合はより厳密 | MIT |
| Top-k 閾値の走査側への伝播 | tantivy `Weight::for_each_pruning` | コールバックが新閾値を返しスキップに使う | 条件付き（優先度低）。等号を含む形なら決定性契約と両立し得るが、本リポは既に 1 パス posting 走査（Issue #392）でスキップ余地が小さい | MIT |
| DAAT（アキュムレータを持たない） | tantivy `term_scorer.rs`／`BufferedUnionScorer` | `DocSet` のみ実装し OR は doc 昇順マージでその場加算。N 長配列の確保・ゼロ初期化が一切ない | 条件付き（§9-#6 の軽量版を Issue #546 で実装済み）。DAAT 完全移行（本手法そのもの）は構造変更コストが大きいため引き続き不採用のまま、`score_by_postings` の `acc: Vec<f64>` を索引の寿命内で再利用するスクラッチプールへ置換し「毎クエリ N 長確保・ゼロ初期化」を解消した（詳細は `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」節参照）。スコアはビット一致のまま | MIT |
| `max_next_weight` による posting 要素単位 pruning | qdrant `sparse/index/search_context.rs` | 後方要素の重み最大値を前方へ伝播し安全な区間だけシーク | 不採用。BM25 ではなく非負重み疎ベクトル（SPLADE 等）前提。block-max WAND と同型の縮約契約懸念を抱え実装コストも高い | Apache-2.0 |
| mmap 圧縮 inverted index | qdrant `InvertedIndexCompressed{ImmutableRam,Mmap}` | — | 不採用。本リポは redb ＋世代キャッシュ方式で完結しており別レイヤ | Apache-2.0 |

出典: tantivy `21ae09c` — `src/postings/compression/mod.rs`, `src/postings/skip.rs`, `src/query/bm25.rs`, `src/query/term_query/term_scorer.rs`, `src/query/boolean_query/block_wand_union.rs`, `src/query/weight.rs`, `src/fieldnorm/code.rs` ／ qdrant `74f3e85` — `lib/sparse/src/index/search_context.rs`, `inverted_index/posting_list.rs`, `posting_list_common.rs`

## 8. バッチ検索・GPU

| 手法 | 実装 | 内容 | 本リポでの採否 | ライセンス |
| ---- | ---- | ---- | -------------- | ---------- |
| BLAS(sgemm) 経路への閾値切替 | faiss `utils/distances.cpp` | クエリ数×次元の閾値で逐次／BLAS を切替 | 不採用。BLAS（OpenBLAS/MKL）の依存追加が必要で依存最小方針と衝突。ただし「規模の閾値でバッチ経路へ切り替える」設計パターン自体は `gpu_batch.rs` のバッチ判定の参考になる（依存非追加の範囲でパターンのみ） | MIT |
| L2 の `‖x‖²+‖y‖²−2⟨x,y⟩` 分解 | faiss `exhaustive_L2sqr_blas_default_impl` | 事前計算ノルム＋sgemm 内積、丸め誤差の微小負値をクランプ | 対象外（本リポは内積のみ・cosine は正規化契約）。負値クランプという数値安定化パターンは将来 L2 系カーネル追加時の参考として記録 | MIT |
| WarpSelect／BlockSelect（GPU 上 Top-k） | faiss `gpu/utils/{WarpSelectKernel,BlockSelectKernel,Select}.cuh` | ウォープ内シャッフルでビトニックマージ | 条件付き（移植困難）。CUDA 専用の warp shuffle に依存し `wgpu`/WGSL へ直接移植不可。WGSL `subgroup` 拡張なら原理的に可能だが `wgpu =30.0.1` 時点の対応状況と環境非依存方針（NVIDIA/AMD/Apple 混在）でハードルが高い。「GPU 上で Top-k まで完結させ全距離を CPU へ転送しない」方針自体は記録に値する。wgpu での設計は [`gpu-batch-topk.md`](gpu-batch-topk.md)（#535） | MIT |
| 距離計算と Top-k のタイル内融合 | faiss `utils/distances_fused/` | 実体は CPU 側 AVX-512 カーネル（GPU ではない）。小次元×Top-1 専用 | 不採用（誤読注意点として記録）。GPU タイル融合ではなく CPU 特殊ケース（小次元・Top-1 専用） | MIT |
| `wgpu` による GPU バッチ検索 | 本リポ `gpu_batch.rs` | — | 既採用（TASK-128〜130・Issue #178。CORE-6/16 のベンチ配線・`GpuF32ContrastBackend` の f16/f32 常駐対照含む） | — |

出典: faiss `2ed4c10` — `faiss/utils/distances.cpp`, `faiss/utils/distances_fused/{distances_fused.h,distances_fused.cpp}`, `faiss/gpu/utils/{Select.cuh,WarpSelectKernel.cuh,BlockSelectKernel.cuh}`

## 9. 実測ギャップに基づく上位施策と既起票 Issue の対応

`docs/design/crossdb-bench.md`（25,000 行・dim128・2026-09-05 時点の記録）では
self は全フェーズで最速ではない。最も健闘するフィルタなし `vector_knn` でも
Qdrant HNSW に劣後する:

| フェーズ | self | 最速の他 DB | 倍率 |
| -------- | ---- | ----------- | ---- |
| `vector_knn`（フィルタなし・self 最善） | 786µs | Qdrant HNSW 559µs | 1.4x 遅い |
| `vector_knn_where`（フィルタ付き） | 2,819µs | Qdrant HNSW 615µs | 4.6x 遅い |
| `agg_count` | 3,547µs | sqlite-vec 616µs | 5.8x 遅い |
| `where_compound_count` | 3,903µs | sqlite-vec 950µs | 4.1x 遅い |
| `rls_isolation` | 3,552µs | sqlite-vec 620µs | 5.7x 遅い |
| `group_by_having` | 3,948µs | sqlite-vec 2,735µs | 1.4x 遅い |
| `hybrid_rrf` | 6,178µs | sqlite-vec 3,508µs | 1.8x 遅い |
| `bulk_knn_k200` | 7,993µs | Qdrant HNSW 3,097µs | 2.6x 遅い |
| `bulk_knn_k1000` | 11,151µs | pgvector 4,989µs | 2.2x 遅い |

負けの主因は投影・フィルタ・集計といった SQL 表層の固定コストに集中しており、
これが以下の順位を決めている。`vector_knn` の 1.4x 差は例外で、ここは距離
カーネル・ANN 側の改善が効きうる唯一のフェーズだが、786µs には wire 往復＋
テキスト応答組み立てが含まれるため（`crossdb-bench.md` 所見）、カーネル／ANN が
1.4x のうち何割を占めるかは未切り分けであり、施策着手前に内訳計測が必要。

| # | 施策 | 期待効果（定性） | 実装難度 | 依存追加 | 新規 `unsafe` | 状態 |
| - | ---- | ---------------- | -------- | -------- | -------------- | ---- |
| 1 | Top-k 確定後の遅延スカラーデコード | 最大。k 非依存の約 9〜10ms 固定コストを除去。`bulk_knn_k200/k1000`・投影列を伴う全 SELECT に効く。集計経路には既に同型の `DecodeTier`（Issue #350）が実装済み | 中 | 不要 | 不要 | 既起票 #453 |
| 2 | スカラー列二次索引 | 大。`vector_knn_where` 4.6x・`where_compound_count` 4.1x の主因である `WHERE` の O(N) 全行走査を除去 | 大 | 不要 | 不要 | 既起票 #359（Proposed）→ #471 |
| 3a | `hnsw_subset` 退行の原因切り分けと `full_scan_ratio` 再調整 | 大。ANN opt-in 時に SCALAR 事前フィルタ付き DISTANCE が 37〜45% 悪化する既知の退行（Issue #413）。#413 は「フィルタの選択性が主要因・行数規模は副次的」と結論しており、切替閾値がずれている可能性がある。定数の再調整が最も安価な仮説で ACORN より先に検証すべき | 小 | 不要 | 不要 | 既起票 #486 |
| 3b | ACORN-1（2-hop 展開）の導入 | 中。3a で閾値調整では解けないと判明した場合の本命 | 大 | 不要 | 不要 | 契約整理・ゲート条件設計は #500 で実施済み（条件付き成立）。実装 #501・実測 #502 |
| 3c | hybrid 密側再取得の破棄候補ヒープ保持化（pgvector 型） | 中。3a/3b とは別経路。SCALAR 事前フィルタ付き DISTANCE 経路には再取得ループ自体が存在しない（Issue #410: `masked_short` は構造的に到達不能）ためこの施策は `hnsw_subset` 退行には効かない | 中 | 不要 | 不要 | 既起票 #503 |
| 4 | `repair_reachability` の並列化 | 中（実測根拠あり）。HNSW 構築の 8→12 スレッド頭打ちの主因（12 スレッド時に total の 39.4% を占める単一スレッド後始末段。usearch に対し 1.24〜1.26x 遅い唯一の敗因。探索は既に usearch より速い：65.5µs vs 76.5µs） | 中 | 不要 | 不要 | 既起票 #446 ツリー（#447〜#450。並列化本体は #449） |
| 5 | HNSW 隣接リストの CSR 化 | 中（実測根拠なし・構造推論）。`Vec<Vec<u32>>` のノード×レベル分のヒープ確保・間接参照を単一 `Vec<u32>`+offsets へ | 中 | 不要 | 不要 | 既起票 #492 |
| 6 | BM25 アキュムレータを可視集合サイズで確保 | 小（N と可視率に依存）。`score_by_postings` の `vec![0.0; N]` を実作業量 O(可視ヒット数) に合わせる。RLS で可視集合が小さいテナントほど効く。スコアはビット一致のまま | 小 | 不要 | 不要 | #546 で実装（索引寿命内の再利用バッファ方式。実測は #547） |
| 7 | `search_layer` への prefetch 導入 | 中。hnswlib 型ソフトウェアパイプライン。真の prefetch 命令は `#[target_feature]` fn の内側限定（通常の fn からは E0133）で発行不可なため、新規 `unsafe` ゼロの制約下では `core::hint::black_box` によるタッチ方式で実装（#490） | 小〜中 | 不要 | 不要 | #490 で実装済み。#491 で 8 規模点（10k／100k・dim 128／768・マスク有無）の前後比較を実測し、8 点中 7 点は悪化方向への一貫したシグナルは無いが、1 点（10k×dim128・可視率50%）で両ノイズ帯を超える一貫した悪化が観測され、静的解析／実アセンブリの裏付けが無いため撤回条件は完全には満たさず保留（production 無変更・専有実機再実測をオーナーへ申し送り）。詳細は [`docs/design/hnsw-search.md`](hnsw-search.md)「Issue #491」節参照 |
| 8 | f16 常駐＋f32 再スコア（ANN opt-in 経路限定） | 中。移動バイト半減。新規 `unsafe` ゼロ。候補集合が変わるため既定 brute-force 経路には適用不可 | 中 | 不要 | 不要 | 既起票 #513 |
| 9 | visited のサイズ閾値切替（密ビットマップ ↔ `HashSet`） | 小〜中。可視カーディナリティが索引ノード数に対し極小のとき全ノード分の確保・走査を回避 | 小 | 不要 | 不要 | #497 で機構（`VisitedSparse`・`search_masked_with`・`sparse_visited_max` opt-in）を実装済み。#498 で可視比率別 dense/sparse 前後比較を実測（rows 10k〜100k・可視率 2〜50%）した結果、全測定点で sparse が dense を上回る速度改善は観測されず、閾値既定値は `0`（常に dense）のまま暫定維持（改善の根拠不足。`benchmark-judgement-policy.md` §5 が求める確定 Rejected の証拠要件は満たさない）。詳細は [`docs/design/hnsw-search.md`](hnsw-search.md)「Issue #498」節参照 |
| 10 | dim>=768 での多アキュムレータディスパッチ | 小〜中（dim=128 の現行ベンチでは効果ゼロ）。[`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md) の arena 表で ACC=4 が dim768/1536 のみ改善。dim=768 のベンチ点追加が前提 | 小 | 不要 | 不要 | 既検討・不採用 #365 の条件付き再訪。#518 で dim 閾値ディスパッチとして実装済み。#519 で本環境（共有 QEMU）の前後比較を実測済み（参考値。production `dot_block4` 経路が 8〜11% 退行する所見あり）。閾値確定・退行対処はオーナー実機〔#530〕へ申し送り |

内訳切り分け・dim=768 規模点追加・生成コード検査ガード・macOS 検出検証・計測規約
は #463／#466／#467／#468／#462 として起票済み。`hybrid_rrf` 最新基線の再計測は
Issue #465 として実施済み（SQL 表層固定コスト・疎側再取得ループがほぼ同水準
〔37〜39%〕で最大区分。`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」
節・Phase 6〔#548〕への引き継ぎ参照）。intrinsics 導入方針 ADR は #508、行間
マイクロカーネル／i8 VNNI／NEON
dotprod／分岐なし tail は #509／#520／#524／#527、GPU 側（マルチクエリ
dispatch／GPU 側 Top-k／SHADER_F16／`dot4I8Packed`）は #531／#534／#538／#541 と
して起票済み。広域取得モード（Top-k の k 規模拡大時の再検討先）は #454。

## 10. 既 Rejected 施策の対応表（再提案防止）

| Issue／PR | 施策 | 記録先 | 再訪条件 |
| --------- | ---- | ------ | -------- |
| #365 | dot カーネルの行内複数アキュムレータ（ACC=2／4） | [`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md) | dim>=768 限定ディスパッチとして #518 で実装済み（#517・#466 が前提）。#519 で本環境の前後比較・閾値候補実測済み（参考値。閾値 768 は据え置き）。最終確定・退行対処はオーナー実機〔#530〕 |
| #366 | 距離計算と Top-k の 2 段分離バッチ化 | [`docs/design/knn-two-stage-topk.md`](knn-two-stage-topk.md) | 専有環境での再実測（#462 規約）で run-to-run 変動を上回る差が出た場合のみ |
| #391 | fieldnorm 256 段ロッシー量子化 | [`docs/design/hybrid-rrf-latency-breakdown.md`](hybrid-rrf-latency-breakdown.md)「Issue #391」節 | ビット一致契約自体の改訂（spec 側判断）なしには再訪しない |
| #400 | redb `insert_reserve` ゼロコピー | [`docs/design/redb-insert-reserve-zero-copy.md`](redb-insert-reserve-zero-copy.md) | redb 側の内部実装変更が確認された場合 |
| #410 Phase B | 再開型スキャン（DISTANCE 経路の ef 倍増ループ） | [`docs/design/hnsw-hybrid-iterative-scan.md`](hnsw-hybrid-iterative-scan.md) | `masked_short` が到達可能になる設計変更時（pgvector 型の継続方式は #503 で hybrid 密側の別経路として扱う。hybrid 密側は #504 で状態保持・決定性・停止性契約を設計済み〔`hnsw-hybrid-iterative-scan.md`「Phase B 再検討（Issue #504）」節〕。実装は #505） |
| PR #451 → PR #452 | 対照エンジン `hnsw_rs`（`=0.3.4`）の追加 | [`docs/design/hnsw-parallel-build.md`](hnsw-parallel-build.md)「hnsw_rs を加えた 3 エンジン比較」節 | 対照は usearch で足りるとのオーナー判断（2026-09-05）。再追加はオーナー承認が前提 |

## 11. 順位から外した候補と理由／未カバーのギャップ／計測カバレッジ

順位から外した主な候補:

- block-max WAND／posting 圧縮（tantivy）: 早期打ち切りがタイブレーク完全性契約
  と構造的に衝突し、インデックス全体統計への依存がテナント境界縮約契約とも
  両立しない
- PQ／PQ4 fast-scan／OPQ: コードブック学習・LUT 構築の自作コストが大きく、
  依存追加禁止下での ROI が低い
- faiss reservoir + SIMD 分位点 Top-k: k が数百〜数千で優位になる手法で本リポの
  利用規模とミスマッチ。#366 の知見とも整合
- hnswlib 型 level0 インターリーブ／qdrant 型リンク圧縮: 永続化前提のレイアウト。
  本リポの HNSW は永続化未実装のため時期尚早
- faiss `IDSelector` 型の中間フィルタ: 不適合ノードにも距離計算が走り、PR #431
  の P0 契約に反するため採用不可（この契約〔ベクトル非参照〕は #500 の ACORN-1
  契約整理でも不変条件 I1 として維持）
- BLAS(sgemm) バッチ経路: BLAS 依存の追加が必要で依存最小方針と衝突

top-10 でカバーできていないギャップ:

- `hybrid_rrf`（6,178µs・sqlite-vec 比 1.8x 遅い・2026-09-05 時点）: 上位 10
  施策のうち直接効くものが無い。疎索引側は #388〜#392 で大幅に最適化済みで、
  最新基線（[`docs/design/hybrid-rrf-latency-breakdown.md`](hybrid-rrf-latency-breakdown.md)
  「Issue #392」節）では `hybrid_search_cached_index` は median 11,020µs→
  5,571µs まで短縮している。それでもなお他 DB に劣後しており、施策化の前に
  `make bench-hybrid-profile` での再計測が要る（既起票 #465）
- `group_by_having`（3,948µs・1.4x）: 施策 #2（二次索引）で改善する見込みだが
  未検証

計測カバレッジ: 上記ギャップ（`hybrid_rrf`・`group_by_having`）の実測元である
[`docs/design/crossdb-bench.md`](crossdb-bench.md) の横断 SQL ベンチ
（self／pgvector／sqlite-vec／Qdrant／LanceDB／MySQL 比較。`scripts/crossdb_bench/`）
は dim=128・25,000 行の単一点に限定されており、この横断比較の範囲では dim 依存の
施策（#8・#10）の採否を判定する材料が無い。これはカーネル単体の計測資産の欠落
ではない ── `crates/engine/benches/dot_kernel_bench.rs` は dim=[100, 128, 384, 768,
1536] を、`crates/engine/benches/batch_bench.rs` も dim=256／768 の経路をそれぞれ
既に計測している（[`docs/design/dot-kernel-multi-accumulator.md`](dot-kernel-multi-accumulator.md)
参照）。不足しているのは横断 SQL ベンチ側の dim 展開であり、`dim=768` のベンチ点
追加（既起票 #466）は crossdb_bench 側の対応として推奨する。

## 12. 未調査の実装

- Milvus/Knowhere（Apache-2.0）: 未調査。時間配分の都合で対象から外した
- Vespa（Apache-2.0）: 未調査。同上
- RaBitQ 論文実装（`gaoj0017/RaBitQ`）: 未調査。ただし faiss に
  `RaBitQuantizer`（MIT）が存在するため、手法の把握と帰属は faiss 経由で
  足りている（§4 参照）

本調査の全対象は MIT／Apache-2.0／PostgreSQL License（BSD 系）であり、GPL 系
ライセンスの実装は 1 つも含まれない。

## 参照

- spec ポインタ（本文非転記）: CORE-9／CORE-10／CORE-16／TASK-132／SEARCH-1／
  SEARCH-3／SEARCH-7／PLAN-4／PLAN-6／PLAN-7（`docs/spec/04-behavior/`）
- [`docs/design/chip-kernel-guidelines.md`](chip-kernel-guidelines.md)
  （チップ別設計指針・Rust stable での intrinsics 可用性）
- [`docs/design/ann-index-adoption.md`](ann-index-adoption.md)（ANN 採否 ADR）
- [`docs/design/scalar-secondary-index.md`](scalar-secondary-index.md)
  （スカラー列二次索引 ADR）
- [`docs/design/hnsw-rls-cardinality-switch.md`](hnsw-rls-cardinality-switch.md)
  （HNSW×RLS 統合・PR #431 の P0 契約の所在。ACORN-1 導入時の契約整理は
  「Issue #500」節参照）
