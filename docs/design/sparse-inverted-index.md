# ADR: hybrid 疎索引の転置索引化（Issue #386 Phase 1・記録 Issue #394）

- ステータス: Accepted（実装済み。#388〜#392。本ドキュメントは記録専任）
- 対応: Issue #394（親 Issue #386・ルート Issue #385）
- 関連ポインタ（本文非転記）: SEARCH-1・SEARCH-3・TASK-104
  （`docs/spec/04-behavior/search.md`・`docs/spec/05-tasks.md`）
- 関連 ADR: `sparse-index-cache.md`（Issue #357）・`hybrid-rrf-latency-breakdown.md`
  （Issue #356/#387〜#392。段別内訳の実測はこちらが一次資料）・
  `rrf-tie-break-determinism.md`（TASK-84）・
  `sparse-inverted-index-recall-verification.md`（Issue #393。Recall 非劣化・
  cold/hot 等価性・決定性の検証）
- 検証コード: `crates/engine/tests/sparse_cache_recall.rs`・
  `crates/engine/tests/sparse_determinism.rs`・
  `crates/engine/tests/hybrid_profile_accept.rs`・`sparse.rs` 内 `#[cfg(test)]`
- production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
  本 Issue の範囲では無変更（docs 専任）

## 背景

親 Issue #385/#386 の調査により、hybrid 検索（`sql/exec.rs` の `Ranking::Hybrid`
分岐）が呼ぶ `SparseIndex`（`crates/engine/src/sparse.rs`）が、旧実装では
クエリ毎に全文書を線形走査してスコアを計算していたことが分かった
（`feature_bench` 実測（下記「前後比較」節）で `hybrid_rrf` は密 KNN 単体
（`vector_knn`）の before 10.63x。Phase 1 後は 1.38x へ縮小しており、実測値・
測定条件の詳細は下記「前後比較」節の比較表を単一の出典とする）。Phase 1
（#388〜#392）はこの疎索引を転置索引（posting list）＋可視ビットマップ 1
パス走査方式へ再構成し、コーパス文書数 `N` に対する posting へのスコアリング
走査を排除した（呼び出しごとのスコアアキュムレータ確保・ゼロ初期化は `O(N)`
のまま残る。詳細は下記「データ構造」節・「スコープ外・申し送り」節参照）。

## 設計判断（サブ Issue 順）

各判断は「採用・不採用」を明記する。データ構造の詳細な不変条件は
`sparse.rs` のモジュール／構造体ドキュメンテーションコメントが単一真実源であり、
本節はその要点のみを転記する。

### #388: term インターニング（`TermId`・`TermDictionary`）

クエリ語・コーパス語をキー文字列のまま扱う `BTreeMap<String, _>` を撤去し、
`TermId(u32)` への intern／lookup（`HashMap` ベース）へ置換した。クエリ語は
構築済み辞書への lookup 1 回で未知語（辞書に存在しない語）を除外できるため、
後続のスコアリングがクエリ語数 `Q` に対してのみ線形になる基盤を作った。

**採用**。文字列キーでの走査コストを構造的に排除する前提となる変更であり、
不採用の選択肢は検討していない。

### #389: posting list・`doc_len`／`doc_ids` 配列

`postings: Vec<Vec<(u32 doc_idx, u32 tf)>>`（`TermId` 添字）・`doc_len: Vec<u32>`・
`doc_ids: Vec<DocId>`・`id_index: HashMap<DocId, usize>` を追加した。`postings[t]`
は `TermId(t)` の出現文書を `doc_idx` 昇順・重複なしで保持する（ソートは行わず、
構築順序自体〔文書を `doc_idx` 昇順に処理する〕がこの不変条件を満たす）。

**採用（非圧縮 `Vec` 表現）**。不変条件は
`postings.len() == doc_freq.len() == terms.len()`・
`postings[t].len() == doc_freq[t] as usize`（構築時に保証）。

### #390: 可視ビットマップ＋posting 走査 1 パス化

`VisibleBitmap { words: Vec<u64>, n, total_len }` を新設し、`search`／
`search_within` の共通コア `score_by_postings` が、可視集合を `doc_idx` 空間の
ビットマップへ変換したうえで、クエリ語ごとに posting list だけを辿ってスコアを
積む 1 パス走査へ再実装した（旧 `docs: Vec<DocEntry>` によるコーパス全体の線形
走査を撤去）。可視集合限定の統計（可視文書数 `n`・可視文書の文書長合計
`total_len`）もこの 1 回の走査で確定させ、インデックス全体の `doc_count`／
`avg_doc_len`／`doc_freq` を一切参照しない構成を維持した（テナント境界の
維持契約。詳細は次節）。

**採用**。時間計算量は `O(|visible_ids| + Σ_{t∈Q}|postings(t)| + M log k)`
（`|visible_ids|`: 可視集合を `VisibleBitmap` へ変換する走査コスト、
`Σ|postings(t)|`: クエリ語の出現延べ件数（posting へのスコアリング走査コスト）、
`M`: スコア `> 0` の一致文書数）であり、**posting へのスコアリング走査のみ**が
コーパス全体の線形走査（旧 `docs: Vec<DocEntry>` 方式）から脱却した。可視集合
走査 `O(|visible_ids|)`、およびスコアアキュムレータの確保・ゼロ初期化
（呼び出しごとに `O(N)`。`N`: コーパス文書数）は引き続き残る（「スコープ外・
申し送り」節参照）。

### #391: 文書長クラス表＋`select_nth_unstable_by` 型 Top-k

`len_classes: Vec<u32>`（コーパス中に現れる文書長の相異なる値を昇順・重複なしで
並べたクラス表）・`doc_len_class: Vec<u32>`（`doc_idx` → クラス添字）を追加し、
`TopKSelector`（2k バッファ＋`select_nth_unstable_by` による Top-k 選出。tantivy
の `TopNComputer` を参考にした決定的タイブレーク付き構成）を導入した。

本リポの疎スコアは f64 ビット一致を production の契約として維持しており
（`docs/design/rrf-tie-break-determinism.md`）、段数をコーパス中の相異なる
文書長数まで上げた厳密なクラス表を採用した。

### #392: 疎側再取得ループの再スコアリング回避

`score_within(...) -> SparseScored`・`SparseScored::top(k)`（前方一致な Top-k
拡張。`top(k1)` は `top(k2)`（`k1 ≤ k2`）の前方一致になる）を新設し、
`hybrid.rs::sparse_refetch_loop` が境界同点グループ拡張のたびに `search_within`
を呼び直していた構造（各ラウンドでゼロからスコアリング）を、「1 回スコア
計算 → `top(fetch_k)` で k を伸ばす」へ置換した。密側の再取得ループ・
`complete_boundary_tie_group`（Issue #310/#320）は無変更。

## データ構造

`SparseIndex`（`crates/engine/src/sparse.rs`）の主要フィールド:

| フィールド | 型 | 役割 |
| ---------- | -- | ---- |
| `terms` | `TermDictionary` | クエリ語・コーパス語 ⇔ `TermId` の intern／lookup |
| `doc_freq` | `Vec<u32>`（`TermId` 添字） | 出現文書数（`df`）。`len() == terms.len()` |
| `postings` | `Vec<Vec<(u32 doc_idx, u32 tf)>>`（`TermId` 添字） | 転置索引。`doc_idx` 昇順・重複なし |
| `positions` | `Vec<Vec<u32>>`（`TermId` 添字） | 出現位置（Issue #2114）。`postings[t]` の各要素の `tf` 個ずつを posting 順に連結。フレーズ一致だけが読む |
| `doc_len` | `Vec<u32>`（`doc_idx` 添字） | 文書長 |
| `doc_ids` | `Vec<DocId>`（`doc_idx` 添字） | `doc_idx` → 呼び出し元 `DocId` |
| `id_index` | `HashMap<DocId, usize>` | `DocId` → `doc_idx`（`O(1)` 逆引き） |
| `len_classes` | `Vec<u32>` | 相異なる文書長の昇順・重複なし表 |
| `doc_len_class` | `Vec<u32>`（`doc_idx` 添字） | `doc_idx` → `len_classes` の添字 |

`VisibleBitmap { words: Vec<u64>, n, total_len }`（`search_within` 呼び出し
1 回ごとに構築）は可視集合を `doc_idx` 空間のビットマップへ変換したもので、
確保量（`words` の長さ）はインデックス側の文書数（`MAX_CORPUS_DOCS` 以下で
有界）のみで決まり、untrusted な `visible_ids` の大きさで増幅しない。

計算量: `O(|visible_ids| + Σ_{t∈Q}|postings(t)| + M log k)`
（`|visible_ids|`: 可視集合走査コスト、`Σ|postings(t)|`: posting へのスコア
リング走査コスト、`M`: スコア `> 0` の一致文書数。`sparse.rs::search_within`
のドキュメンテーションコメントと一致）。この式に加え、呼び出しごとのスコア
アキュムレータ確保・ゼロ初期化は別途 `O(N)`（`N`: コーパス文書数）かかり、
式には含まれない（前掲「採用」節・「スコープ外・申し送り」節参照）。
メモリ: `approx_heap_bytes()`（下記「前後比較」節の `bench-hybrid-profile`
実測値を参照）。

## 位置付き posting（Issue #2114・SEARCH-12 ポインタ）

フレーズ一致（隣接した語の並び）の判定のため、`postings` とは別に並列の位置配列 `positions` を持つ。
位置モデルは [fulltext-query-syntax.md](fulltext-query-syntax.md) の D4（ASCII 単語 1 つまたは CJK 1 文字を 1 単位とし、
バイグラムは先頭文字の位置、ストップワード文字は位置を消費し、区切りは消費しない）で、
分割規則の正である `walk_tokens_positioned` が数える。

- レイアウト: `positions[t]` は `postings[t]` の要素 `i` について、直前までの `tf` の累積から `tf_i` 個の位置を持つ（オフセット配列なし）。
  不変条件は `positions.len() == postings.len()`・`positions[t].len() == Σ tf`・各文書の区間内で位置は厳密に昇順
- `postings`・スコア経路（`score_by_postings` ほか）と構築・世代キャッシュの無効化経路は変えない。位置は `with_params` の中で同時に作る
- 採用しなかった案: `(doc_idx, tf, pos_start)` へ広げる案（スコアの走査の配置が変わる）、CSR への平坦化（構築が 2 パスになる。将来の最適化）
- メモリの上界: `4 B × MAX_CORPUS_TOKENS`（8,000,000）= 32 MiB と、語彙ごとの `Vec` ヘッダ（24 B × 語彙数）。`approx_heap_bytes` に計上済み（`SparseIndexCache` の容量判定が過少計上されない）

### メモリ増加の実測

決定的な合成コーパス（xorshift64\*）で、位置配列の追加ぶん（`approx_position_heap_bytes`）を測った。
値は容量ベースの概算（`approx_heap_bytes` と同じ算出方式で、スクラッチプール上限を含む）。

| コーパス | 文書数 | 総トークン数 | `approx_heap_bytes` | うち位置 | 位置なし | 増加率 |
| -------- | ------ | ------------ | ------------------- | -------- | -------- | ------ |
| ASCII 主体（語彙 5,000・1 文書 100 語） | 2,000 | 200,000 | 3,807,630 | 996,608 | 2,811,022 | +35.5% |
| 日本語主体（1 文書 100 文字） | 2,000 | 357,399 | 3,907,840 | 1,454,172 | 2,453,668 | +59.3% |

位置は 1 トークンあたり 4 B（`u32`）で、語彙ごとの `Vec` ヘッダが加わる。増加率は語彙の重なりが小さい（posting が短い）コーパスほど
`postings` の 8 B/件に対する割合が定数に近づき、トークンの多い日本語主体で大きい。
`bench-hybrid-profile` の RSS 実測は今回の環境では未実施（未検証）。再現は `hybrid_profile_bench` の
`memory stage=sparse_index_positions` 行、または下記のテストで得る。

```bash
cargo test -p fandhe-db-engine --test core_misc sparse_phrase -- --nocapture
```

## 維持した契約

- **RLS 縮約統計の fail-closed**: `search_within` は `VisibleBitmap` 構築と
  同じ 1 回の走査で可視集合限定の統計（`n`・文書長合計）を確定させ、
  インデックス全体の `doc_count`／`avg_doc_len`／`doc_freq` を一切参照しない
  （テナント境界を越えた統計漏えいの防止。Issue #36 codex-review P0 指摘に
  由来する既存契約を Phase 1 でも維持）。`VisibleBitmap` の確保量は
  `MAX_CORPUS_DOCS` で有界であり、untrusted `visible_ids` で増幅しない。
- **決定性**: `TieRank::GroupEnd`（既定）・境界同点グループ完全化
  （`complete_boundary_tie_group`）は無変更。#391・#392 が導入した
  `select_nth_unstable_by` 等の不安定ソート API は `Candidate` の全順序を
  根拠に許容される例外（`// sort-determinism: allow` マーカー付き。
  `scripts/check_sort_determinism.sh` は green のまま）。詳細は
  `docs/design/rrf-tie-break-determinism.md` および
  `sparse-inverted-index-recall-verification.md`（Issue #393）の疎側決定性
  固定テストを参照。
- **スコアの f64 ビット一致**: 旧実装との等価性は参照実装比較テストで固定
  （`sparse-inverted-index-recall-verification.md` 参照）。
- **`SparseIndexCache` の世代整合・fail-closed**（Issue #357）: 転置索引化は
  キャッシュ対象のデータ構造の内部変更であり、`sql/sparse_cache.rs` の
  キャッシュ契約（テーブル世代不一致時の fail-closed 拒否）はそのまま維持。
- **untrusted 入力上限**: `MAX_QUERY_TERMS`（1024）・`MAX_QUERY_BYTES`
  （16 KiB）・`MAX_DOC_BYTES`（1 MiB）・`MAX_CORPUS_DOCS`（100,000）・
  `MAX_CORPUS_BYTES`（64 MiB）・`MAX_CORPUS_TOKENS`（8,000,000）はいずれも
  無変更。
- **公開 API シグネチャ**: `SparseIndex::build`／`with_params`／`search`／
  `search_within` の公開シグネチャは無変更。新設 API は `score_within`・
  `SparseScored`（#392）のみで、既存呼び出し元（`hybrid.rs`）への破壊的変更
  はない。

## 参照した外部実装（手法名・ライセンス・採否のみ。コード転記なし）

| 実装 | ライセンス | 参考にした手法 | 採否 |
| ---- | ---------- | --------------- | ---- |
| tantivy | MIT | posting list（転置索引）構造 | 採用（非圧縮 `Vec`） |
| tantivy | MIT | `TopNComputer`（2k バッファ＋`select_nth`型 Top-k） | 採用（決定的タイブレーク付き。#391） |

（`docs/design/ann-index-adoption.md`・`scalar-secondary-index.md` が qdrant を
Apache-2.0 表記で参照している既存の先例と表記を揃えた）

## 前後比較（`feature_bench` 13 フェーズ）

### 測定条件

- **before**: `8bfaaa4`（origin/main。#387〔Issue #416〕まで込み・#388 直前と
  production コード同一）を `git worktree add --detach` でチェックアウトし、
  別 `CARGO_TARGET_DIR` で `cargo build --release -p engine --example
  feature_bench` を実行
- **after**: 本ブランチ（`origin/main` `6cdf2e7`〔#428 まで。Phase 1 の
  production 変更 #388〜#392 = PR #422/#424/#425/#426/#427 を含む〕+
  本 Issue の docs 変更）で同一コマンドを実行
- 各 3 回連続実行し、フェーズごとの p50/p95/`rss_kb_after` の**中央値**を採用値
  とした（本開発環境は非専有・並行エージェントあり。他 doc と同様、参考値として
  扱う）
- Issue #385 が記録する参考基線（`hybrid_rrf` p50 288.6ms・95.6ms 等。
  コミット未特定・`feature_bench` 自体が長期未追跡だった経緯は
  `hybrid-rrf-latency-breakdown.md`「実測値の比較可能性についての重要な注意」
  参照）とは実行環境・コミット双方が異なるため直接比較しない。本節の
  before/after はいずれも本ドキュメント作成時に同一環境で実測した値のみを扱う

### 13 フェーズ表（p50/p95 中央値・3 回実測）

| フェーズ | before p50 | after p50 | 比 after/before | before p95 | after p95 | before rss_kb | after rss_kb |
| -------- | ---------: | --------: | ---------------: | ---------: | --------: | -------------: | -------------: |
| ingest | 4,113us | 4,130us | 1.00x | 4,320us | 4,305us | 33,140 | 33,248 |
| point_where | 2,727us | 2,728us | 1.00x | 2,931us | 2,863us | 60,580 | 60,644 |
| where_compound | 3,290us | 3,290us | 1.00x | 3,366us | 3,497us | 60,580 | 60,644 |
| agg_count | 2,838us | 2,866us | 1.01x | 3,077us | 3,115us | 60,580 | 60,644 |
| agg_multi | 3,073us | 3,100us | 1.01x | 3,277us | 3,278us | 60,580 | 60,644 |
| group_by_having | 3,581us | 3,566us | 1.00x | 3,659us | 3,674us | 60,580 | 60,644 |
| vector_knn | 8,539us | 8,348us | 0.98x | 9,723us | 9,324us | 86,736 | 86,792 |
| vector_knn_where | 2,766us | 2,763us | 1.00x | 2,915us | 2,981us | 86,736 | 86,792 |
| **hybrid_rrf** | **90,800us** | **11,507us** | **0.13x** | **98,682us** | **12,948us** | 167,916 | 108,572 |
| mode_recall | 8,709us | 8,605us | 0.99x | 10,131us | 11,342us | 167,916 | 108,572 |
| mode_precision | 8,633us | 8,560us | 0.99x | 9,635us | 9,274us | 167,916 | 108,572 |
| rls_isolation | 2,720us | 2,793us | 1.03x | 2,839us | 2,944us | 167,916 | 108,572 |
| udf_call | 659us | 685us | 1.04x | 743us | 1,014us | 167,916 | 108,572 |

`hybrid_rrf` の生値（3 回実測。中央値算出前）: before `[90,800, 90,262,
96,691]us`、after `[11,463, 11,507, 11,953]us`。

### `hybrid_rrf`／`vector_knn` 比率

| | before | after |
| - | -----: | ----: |
| `hybrid_rrf` p50 | 90,800us | 11,507us |
| `vector_knn` p50 | 8,539us | 8,348us |
| 比率（`hybrid_rrf` ÷ `vector_knn`） | **10.63x** | **1.38x** |

### 判定

- `hybrid_rrf` は約 7.9 倍改善（p50 90,800us → 11,507us）。`vector_knn`
  との比率も 10.63x → 1.38x へ縮小し、親 Issue #355「密検索と同オーダー」に
  実質到達したと判断できる水準（同じ桁）
- `hybrid_rrf` 以外の 12 フェーズは **p50** 比 0.98x〜1.04x の差にとどまる。
  ただし中央値算出前の生値を記録しているのは `hybrid_rrf` のみであり、他
  フェーズの run-to-run 差（分散特性）は `hybrid_rrf` と異なりうるため、
  `hybrid_rrf` の run-to-run 幅（before 生値 90,262〜96,691us）をそのまま
  他フェーズのノイズ帯として転用した非退行判定はしない。他フェーズ自身の
  生値による判定は本 Issue の測定条件（3 回実測・中央値のみ記録）では行えず、
  0.98x〜1.04x という差が実変化かノイズかは**未確定**として扱う（下記
  「スコープ外・申し送り」節へ申し送る）。転置索引化は `SparseIndex`／
  `hybrid.rs` 経路のみを変更しており、他フェーズ
  が通る経路（集計・`GROUP BY`・純ベクトル KNN・RLS・UDF）は構造的に変更
  対象外であることと整合する
- p95 は `mode_recall`（10,131us→11,342us・約 1.12x）・`udf_call`
  （743us→1,014us・約 1.36x）のように p50 より大きい変化を示すフェーズが
  ある。この 2 フェーズは本 Issue の測定条件（3 回実測・中央値採用）では
  中央値算出前の生値を記録しておらず、`hybrid_rrf` で確認した run-to-run 差
  との比較による非退行判定はできない。上記の非退行判定は **p50 に限定**し、
  p95 の非退行確認は本 Issue のスコープ外として次項「スコープ外・申し送り」
  へ申し送る
- `rss_kb_after` は `hybrid_rrf` 以降のフェーズで before 167,916 → after
  108,572（約 35% 減）と改善している。これは疎索引がクエリ毎の全文書線形走査
  用ワーキングセットを持たなくなったことによる副次効果と考えられるが、本 Issue
  では定量的な要因分解は行わない（参考情報として記録するに留める）

## 前後比較（`bench-hybrid-profile` 段別）

### 測定条件

`make bench-hybrid-profile`（`cargo bench --bench hybrid_profile_bench -p
engine --features bench-internals`）を before（`8bfaaa4` worktree）・
after（本ブランチ）でそれぞれ 1 回実行した（25,000 行・dim 128・単一テナント。
warmup 20・計測 30。`GITHUB_ACTIONS` 下は fail-closed 拒否のため未設定環境で
実行）。

**測定方法の違いの注記**: `8bfaaa4` 側のベンチには #392 で追加された
`score_within_once`／`sparse_refetch_loop` 段が無い。疎側再取得ループの累積
コストは before では `sparse_refetch_summary` の
`estimated_cumulative_mixed_median_us`（各ラウンドの `search_within` 単体
実測 median を実スケジュールに沿って合算した推定値）、after では
`sparse_refetch_loop`（`sparse_refetch_observed` による、疎側再取得ループ
本体そのものの実測値）である（`hybrid-rrf-latency-breakdown.md` の
「Issue #392」節と同方針の注記）。

### 通し表

| 段 | before（`8bfaaa4`） | after（本ブランチ） | 比 | 備考 |
| -- | -------------------: | --------------------: | --: | ---- |
| `sql_hybrid` median | 100,936us | 11,826us | 0.12x | SQL 表層 hybrid クエリ全体 |
| `sql_hybrid` p95 | 114,385us | 12,774us | 0.11x | |
| `sql_dense_knn` median | 6,798us | 6,053us | 0.89x | 対照（密 KNN 単体。変更対象外） |
| `collect_body_strings` median | 1,217us | 1,124us | 0.92x | 変更対象外 |
| `sparse_build_total` median | 212,516us | 46,817us | 0.22x | `SparseIndex::build` |
| `tokenize_only` median | 21,280us | 23,193us | 1.09x | build 内訳（変更対象外の tokenize 自体） |
| `tokenize_term_freq` median | 80,832us | 86,606us | 1.07x | build 内訳 |
| `tokenize_term_doc_freq` median | 161,130us | 155,963us | 0.97x | build 内訳 |
| `hybrid_search_cached_index` median | 114,618us | 5,872us | **0.05x** | 実 hybrid 検索経路（cached index） |
| `hybrid_search_cached_index` p95 | 131,896us | 7,915us | 0.06x | |
| `score_within_once` median | — | 800us | — | after のみ（#392 で新設） |
| `sparse_refetch_loop` median | 116,266us（推定） | 3,413us（実測） | — | 測定方法が異なるため比率は参考値扱い |
| `search_within_fetch_k=25000` median | 17,387us | 1,184us | 0.07x | |
| `search_within_subset_only` median | 767us | 822us | 1.07x | k 非依存部分（変更対象外に近い） |
| `search_within_subset_df` median | 8,360us | 9,457us | 1.13x | k 非依存部分 |
| `approx_heap_bytes()`（`sparse_index_resident`） | — | 12,253,568 bytes | — | after のみ計測（before 側は本段の計測コードが無い） |

### 判定

- `sparse_build_total`（`SparseIndex::build` 全体）は約 4.5 倍改善
  （212,516us → 46,817us）
- `hybrid_search_cached_index`（実際に production が通る hybrid 検索経路
  そのもの）は約 19.5 倍改善（114,618us → 5,872us）。`feature_bench` の
  `hybrid_rrf`（約 7.9 倍改善）より改善率が大きいのは、`feature_bench` の
  `hybrid_rrf` フェーズが `SparseIndexCache`（Issue #357）のキャッシュヒット
  込みの SQL 表層経由測定であるのに対し、本表の `hybrid_search_cached_index`
  は `SparseIndex::build` 自体を含まないキャッシュ済みインデックス前提の
  hybrid 検索コア（`hybrid.rs::hybrid_search_boosted`）単体測定であるため
  （測定範囲の違い。`hybrid-rrf-latency-breakdown.md`「実測値の比較可能性
  についての重要な注意」と同様の理由）
- 疎側再取得ループの累積コストは、旧実装の推定値（116,266us。各ラウンドで
  `search_within` を呼び直す想定コストの合算）に対し、新実装の実測値
  （3,413us）は約 34 分の 1。#392 が「1 回スコア計算 → `top(fetch_k)`」へ
  再構成した効果がここに現れている（測定方法の違いにより厳密な倍率としては
  参考値だが、桁が 2 桁変わっていることは実装変更の方向と整合する）
- `tokenize_*`・`search_within_subset_only`／`_subset_df`（いずれも
  Phase 1 の変更対象外、または k に依存しない部分）は比 0.97x〜1.13x で
  ノイズ帯内

## スコープ外・申し送り

- 位置付き posting は Issue #2114 で実装した（上記）。前方一致用の順序付き語彙は [fulltext-query-syntax.md](fulltext-query-syntax.md)（Issue #2112）で設計し #2115 で実装する
- 位置配列の CSR 化（構築の 2 パス化によるメモリ・ヘッダの削減）は未着手
- posting list の CSR 化・圧縮（可変長整数・delta encoding）・枝刈り高速化は未着手
- `VisibleBitmap` はクエリ（`search_within` 呼び出し）ごとに再構築しており、
  ループ間での再利用はしていない
- クエリ呼び出しごとのスコアアキュムレータ確保（`O(N)`）は残っている
  （`&self`〔非 `&mut self`〕シグネチャのため呼び出し間バッファ再利用には
  シグネチャ変更を要する）
- 専有環境での再実測: 本ドキュメントの数値はすべて非専有環境（本開発
  コンテナ・並行エージェントあり）での参考値
- `feature_bench`／`bench-hybrid-profile` とも各 1〜3 回実測であり、統計的
  厳密性（多数回のブートストラップ等）は担保しない
- `feature_bench` 13 フェーズ表の p95 非退行判定はスコープ外: 上記「判定」
  節の非退行判定は p50（3 回実測の run-to-run 差との比較が可能）に限定して
  おり、`mode_recall`（約 1.12x）・`udf_call`（約 1.36x）のように p50 より
  大きく変化する p95 が存在する。これらの中央値算出前の生値は記録しておらず、
  多数回反復測定による p95 の非退行確認は別途フォローアップとして申し送る
- `feature_bench` 13 フェーズ表の `hybrid_rrf` 以外 12 フェーズの p50 非退行
  判定も未確定: 中央値算出前の生値を記録しているのは `hybrid_rrf` のみで、
  他フェーズは分散特性が異なりうるため `hybrid_rrf` の run-to-run 幅を転用
  できない。12 フェーズそれぞれの生値取得・run-to-run 差との比較による非退行
  確認は別途フォローアップとして申し送る
- 親 Issue #386 の close 判断: 本 Issue が Phase 1 の最終タスクだが、close
  判断はオーナー／ユーザー作業として申し送る

## 再現方法

```sh
# feature_bench（release ビルド。before は該当コミットの worktree + 別 CARGO_TARGET_DIR）
cargo build --release -p engine --example feature_bench
cargo run --release -p engine --example feature_bench

# bench-hybrid-profile（手動専用。GITHUB_ACTIONS 下は fail-closed 拒否）
make bench-hybrid-profile
```

## 参照

- SEARCH-1・SEARCH-3・TASK-104（ポインタ: `docs/spec/04-behavior/search.md`・
  `docs/spec/05-tasks.md`。本文非転記）
- `docs/design/hybrid-rrf-latency-breakdown.md`（#356/#387〜#392 の段別
  内訳・サブ Issue 単位の前後比較。本ドキュメントの前後比較は Phase 1 全体
  〔`8bfaaa4` → 現在〕の通し比較）
- `docs/design/sparse-index-cache.md`（Issue #357。`SparseIndexCache` の
  テーブル世代整合キャッシュ設計）
- `docs/design/sparse-inverted-index-recall-verification.md`（Issue #393。
  Recall 非劣化・cold/hot 等価性・疎側決定性の検証）
- `docs/design/rrf-tie-break-determinism.md`（TASK-84。同点タイブレーク契約）
- `docs/design/ann-index-adoption.md`・`docs/design/scalar-secondary-index.md`
  （qdrant 参照の既存表記の先例）
