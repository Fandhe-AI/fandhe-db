# `<=>` の意味変更に伴う影響範囲の棚卸しと移行方針

- ステータス: 記録（意味変更そのものはオーナー判断 2026-10-10 として spec 側に記録済み。ポインタ: `docs/spec/04-behavior/records/pgvector-distance-operators-decision-2026-10-10.md`、`docs/spec/04-behavior/records/pg-parity-owner-decisions-2026-10-10.md`。本書で新たに承認を得たものではない）
- 対応: Issue #1990（親 #1989）。関連: #1991〜#1995（移行ガイドは #1995）、#2240（ゼロノルムの NaN）、#1986・#2911（実装方式の ADR）、#1996・#1999（HNSW の opclass 別処理）
- ビヘイビア: SQL-33、TASK-237／238、MS-10（ポインタのみ）

## 背景

本書は `<=>` の意味変更（SQL-33 のオーナー判断。意味の詳細は上記ポインタ先を参照し、本書へは転記しない）が及ぶ箇所を一覧にして担当 Issue を割り当てる。本書はコード・テスト・既存文書を変更しない。

距離演算子 `<->`・`<#>`・`<+>` の字句化は #1976 で済み、構文の受理点は現時点で `<=>` のみ（`crates/engine/src/sql/allowlist.rs` の `parse_order_by` と `crates/engine/src/sql/params.rs`）。新演算子の受理拡張は #1977、実行の切替は #1991。

## 棚卸し表（計測値）

- 計測コマンド: `git grep -c -F '<=>' -- . ':!docs/spec'`
- 計測基準: `origin/main` の `44c9e7140`、2026-11-03。並行 PR のマージで件数は変わるため、計測時点の値として扱う
- 合計: 244 ファイル・1056 行（本書の追加前）

| 領域 | ファイル数 | 行数 | 件数上位 |
| ---- | ---------- | ---- | -------- |
| `crates/engine/src` | 13 | 238 | `sql/allowlist.rs` 172、`sql/parser.rs` 20、`sql/params.rs` 18、`sql/lexer.rs` 13 |
| `crates/engine/tests` | 143 | 607 | `core_misc/scalar_index_mask_search.rs` 31、`core_misc/scalar_index_prune.rs` 24、`sql_udf_call.rs` 22 |
| `crates/engine/benches` | 6 | 16 | `harness/sql_c1.rs` 4 |
| `crates/engine/examples` | 1 | 8 | `feature_bench.rs` 8 |
| `crates/wire-server/tests` | 44 | 132 | `wire9_tls.rs` 12、`http_nosql/nosql2_search_binding.rs` 8、`wire_sql/wire_hint_order.rs` 8 |
| `crates/wire-server/docs` | 1 | 2 | `nosql-api.md` 2 |
| `scripts/crossdb_bench` | 11 | 23 | `self_db.py` 9、`hybrid_after_where.py` 5 |
| `docs/design` | 25 | 30 | `wire-extended-query-param-binding.md` 4 |
| 合計 | 244 | 1056 | |

`docs/design/bench-data/` に出現は無い。`.github/workflows/recall.yml` にも出現は無い。

## 棚卸し表（担当割当）

| 領域・代表パス | 影響の種類 | 担当 |
| -------------- | ---------- | ---- |
| `crates/engine/src/sql/allowlist.rs`・`parser.rs`・`params.rs`・`lexer.rs` | 構文受理（新演算子の受理、`<=>` の束縛） | #1977、#1991 |
| `crates/engine/src/sql/exec.rs`・`core.rs` と検索本体（`kernel.rs`・`hybrid.rs`） | 実行・スコアの向き | #1991 |
| `crates/engine/src` 内の単体テスト（`sql/colref_sink_tests.rs`・`core/select_privilege_tests.rs`） | テスト期待値 | #1992 |
| `crates/engine/tests/` | テスト期待値 | #1992 |
| `crates/wire-server/tests/`（`http_nosql/nosql2_search*.rs` を含む） | テスト期待値・NoSQL 検索バインド | #1993 |
| `crates/engine/benches/`・`crates/engine/examples/feature_bench.rs`・`scripts/crossdb_bench/` | ベンチ・ハーネス | #1994 |
| `crates/wire-server/docs/nosql-api.md`・`docs/design/*.md`（既存 25 ファイル）・README・CHANGELOG | 文書 | #1995 |
| `docs/design/bench-data/` | 履歴データ。更新しない | なし |

## 波及先の整理

| 波及先 | 判定 | 根拠のコードパス | 担当 |
| ------ | ---- | ---------------- | ---- |
| HYBRID の密側スコア | 影響あり | `crates/engine/src/hybrid.rs` の候補はスコア降順・同点 id 昇順を入力契約とする | #1991 |
| `USING MODE`・`USING PLAN` | 要判断 | `crates/engine/src/sql/mode.rs`。距離の向きで挙動が変わるかを #1991 が確認する | #1991 |
| HNSW | 要判断 | `crates/engine/src/hnsw.rs`・`sql/hnsw_cache.rs`・`sql/hnsw_hybrid.rs`。正規化前提の内積で動く。opclass 別の扱いは #1996・#1999 | #1996、#1999 |
| GPU バッチ | 影響あり | `crates/engine/src/gpu_batch/packed_i8.rs` は近似内積の降順で候補を選ぶ | #1991 |
| 総当たりカーネル | 影響あり | `crates/engine/src/kernel.rs`（スコア降順・同点 id 昇順の選出規約） | #1991 |
| Recall ゲート | 要判断 | `crates/engine/tests/hybrid_recall.rs` ほか。順位で評価し距離値の符号には依存しない見込み。期待値の書換えは #1992 | #1992 |
| NoSQL(HTTP) の検索バインド | 影響あり | `crates/wire-server/src/http/query/search.rs` は SQL 文字列を作らず `Ranking::Distance` を持つ `BoundStatement` を直接構築する。意味は #1991 の実行切替に従う | #1991、#1993 |
| EXPLAIN の表示 | 影響なし | `crates/engine/src/sql/explain.rs`。計画行の `score` は実在行を持たない固定値で、距離値は出ない | なし |
| 内部のスコア降順表現 | 要判断 | 「内部は類似度降順のまま SQL 境界で反転する」か「距離昇順へ揃える」かで影響範囲が変わる | #1991 |

## 変えないもの

| 対象 | 理由 |
| ---- | ---- |
| `crates/engine/src/recovery/content_hash.rs` のタグ 24 | 意味ではなく `<=>` のトークンで決まる。触らない |
| `CHANGELOG.md` の既存行、`implementation-status.md` の既存段落 | 当時の意味で書かれた履歴 |
| `docs/design/bench-data/` | 実測の履歴データ |

## `ORDER BY` の方向指定

本節は #1990 時点の記録。当時は距離形が `ASC`／`DESC` を受けず、射影にも現れなかったため、観測できるのは順位だけだった。その後 `ASC`／`DESC` が #2009、投影の距離列が #2007 で入り、値が見える経路ができた。符号と大きさの変化は「移行ガイド（#1995）」節の 4 点目を参照。

## 移行手順（要点。詳細は「移行ガイド（#1995）」節）

- 移行方針（互換性の扱い・書き換え手順・ゼロノルムの扱い）は spec のポインタ（SQL-33、`docs/spec/04-behavior/records/pgvector-distance-operators-decision-2026-10-10.md`、#2240）に従う。本書へは転記しない。利用者向けの詳細は末尾の [移行ガイド（#1995）](#移行ガイド1995) 節
- 未対応の形は従来どおり fail-closed（`42601`／`0A000`）で、テナント境界（RLS 相当）は変えない
- 例示は `ORDER BY embedding <#> '[0.1,0.2,0.3]'` のようにリテラルか `$n` バインドで書き、SQL 文字列の連結で組み立てない

## #1991 での判断（`<=>` の実行切替と `<#>` の接続）

`<=>` の実行を本物のコサイン距離（`1 - cos`、昇順）へ切り替え、`<#>` を負の内積の昇順として実行経路へ接続した。上の「要判断」行は次のとおり確定する（ポインタ: SQL-33）。

| 波及先 | 確定 | 内容 |
| ------ | ---- | ---- |
| 内部のスコア降順表現 | 距離昇順へ統一 | DISTANCE 段の出力 `(slot, f64)` は演算子の距離値（昇順）。`<=>` は `1 - cos`、`<#>` は `-dot`。`ResultRow.score` もこの値になる。HYBRID は従来どおり融合スコア（降順） |
| 総当たりカーネル | `SearchProvider` に既定実装付きの `search_cosine`／`search_cosine_subset` を追加 | 内積用の `search`／`search_subset` は変えない（`<#>` と Rust API の `VectorCore::search` が使う）。既定実装は `kernel::cosine_top_k`／`cosine_top_k_slots`。差し替え provider は上書きで観測・注入できる |
| `<#>` の経路 | 既存の内積経路 | 同じ選出器・同じ同点規約なので、順位は旧 `<=>` と同じになる。出力時に符号だけ反転する |
| `<->`／`<+>` | 未接続のまま `0A000` | 実行入口の関門を L2／L1 だけに狭めた。接続は別 Issue |
| HYBRID の密側 | コサイン | HNSW を使わない密側は crate 内アダプタが `search_cosine` を呼び、NaN 行を除いて `1 - d` のスコア降順へ写す（`rrf_fuse` は非有限スコアを拒否するため）。HNSW の密側（`HnswDenseProvider`）は内積のまま残る |
| HNSW（opt-in） | `<=>` は継続利用し返った候補を再採点 | グラフは従来どおり内積で、正規化済み入力の契約（[hnsw-search.md](hnsw-search.md)）に従う。exec が返った候補を厳密なコサイン距離で再採点・再整列する。取りこぼしは補わない（索引を使わない縮退〔全走査・候補マスク〕も内積の上位 k 件を再採点するだけなので、未正規化の入力では厳密なコサインの上位 k 件と一致しないことがある）。コサインの正確化は #1998、キャッシュキー・永続化への metric 反映は #2000 |
| `USING MODE 'precision'` | `<=>` は許可、`<#>`（と L2／L1）との併用は `0A000` | 確信度ゲートはコサイン類似度で判定するため、内積で順位付けした結果とは意味がずれる。束縛段で拒否し、Execute・EXPLAIN・Describe で同じ判定にする |
| `USING PLAN`・NoSQL `search` | 変更なし | どちらも `distance_kind = Cosine` の `BoundStatement` を作り、実行器の切替にそのまま従う |
| `DESC`・`LIMIT` 省略の厳密全走査 | 両演算子に対応 | `<=>` は NaN 行を除外せず、昇順で末尾・`DESC` で先頭に置く。`<#>` は非有限スコアの行を除く |
| GPU バッチ | 対象外 | SQL 経路に無い。#2002 |
| 並列化・ノルム² 常駐（C2） | 対象外 | `<=>` の総当たりは単一スレッドの C1（ノルム²をその場で計算）で接続した。`ParallelSearchProvider` の上書きと C2 は性能だけの変更で、後続 Issue の候補 |

テストへの波及: 内積順位を前提にしていた `<=>` のテストは、`<#>` へ置き換える（仕組みの検証）か、コサインで期待値を導出し直した（一般の kNN）。HNSW と総当たりを比べるテストは、入力を L2 正規化した。意味が古いだけで落ちないテストの掃除は #1992・#1993。

## #2241: ゼロノルム行と HNSW

コサイン用の HNSW 構築は、正規化できない行（ゼロノルム、およびノルム²がアンダーフロー／オーバーフローする行）を索引に入れず、構築を成功させる（INDEX-7・SQL-33 のポインタ）。判定は全走査の `cosine_distance_from_parts` が行側の理由で NaN を返す条件と同じで、除外する行の集合と全走査で NaN になる行の集合が一致する。

| 経路 | ゼロノルム行の扱い |
| --- | --- |
| 全走査（`kernel::cosine_top_k`。#2240） | 距離 NaN で昇順の末尾（`DESC` では先頭）。`k` を消費する |
| HNSW（コサイン） | 索引に入らず、結果に現れない。構築は成功する |

HNSW 経由では、ゼロノルム行は結果から消える。この差は仕様どおり（索引は近傍を返すための構造で、距離が定義できない行は近傍になり得ない）。

- 除外した行もノード番号は入力行番号のまま保つ（詰めない）。入辺を持たず、entry にならず、修復の対象にもならない。常駐ベクトルは 0 埋め。f16・SQ8 常駐でも同じ規則を適用する
- 挿入・更新への追随: 索引済みの行が正規化できなくなったら失効（delta に入れない）、除外された行が正規化できるようになったら delta で拾う。除外のまま変わらない行は churn に数えない
- 現状の SQL の HNSW opt-in は内積グラフと再採点のままで、本 Issue の規則が SQL から観測できるのは `HnswMetric::Cosine` への結線（#2000）以降。それまでは索引が除外を持たないため Overlay の規則は有効にならず、挙動は変わらない
- 申し送り（#2000）: キャッシュ内部で全走査へ縮退する経路（`full_scan_with_arena`）が、ゼロノルム行を NaN で返すか索引経路と同じく落とすかは、結線時に決める。`HnswError::NonNormalizableVector` は公開 enum のため残置した（構築経路は返さない）。クエリ側の正規化不可（`NonNormalizableQuery`）は従来どおり全走査へ縮退する

## 移行ガイド（#1995）

SQL の `<=>` は、内積の降順から pgvector と同じコサイン距離 `1 - cos` の昇順へ変わった。`<#>` は負の内積 `-dot` の昇順として実行できる（#1991・SQL-33 ポインタ）。破壊的変更で、CHANGELOG の 0.2.0 に記録している。

### 必ず押さえる 4 点

1. **互換スイッチは無い**。旧い意味へ戻す起動フラグ・設定・セッション変数は無い。書き換えで対応する。
2. **旧 `<=>` と同じ順位は `ORDER BY e <#> q`（昇順。`ASC` は省略可）で得られる**。同じ選出器・同じ同点規約（同点は行の順序キー昇順）を通る。
3. **正規化済み（L2 ノルム 1）のベクトルなら、新しい `<=>` でも同じ順位になる**。ただし旧経路は f32 の内積、新経路はノルム²をその場で計算して f64 で除算するため、丸めが違う。ほぼ同点の境界では順位が入れ替わることがある（ビット一致は保証しない）。
4. **返る値の符号と大きさが変わる**。旧い値は内積で、大きいほど近い。新しい値は `<=>` が `1 - cos`（0〜2、小さいほど近い）、`<#>` が `-dot`（負になりうる、小さいほど近い）。値が見える経路は投影の距離列（`embedding <=> q AS d`。#2007）と、`execute_sql` 経由の Rust API の `ResultRow.score`。値でしきい値を切っている場合は書き換えが要る。正の内積が欲しい場合は `inner_product(embedding, q)`（`<#>` の符号を反転した値。#2011）を使う。

### 書き換え例

クエリベクトルはリテラルか `$n` バインドで書く（SQL 文字列の連結で組み立てない）。

| 目的 | 旧 | 新 |
| ---- | -- | -- |
| 旧い順位を保つ | `SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 10` | `SELECT id FROM docs ORDER BY embedding <#> '[0.1,0.2,0.3]' LIMIT 10` |
| 拡張問い合わせ | `ORDER BY embedding <=> $1` | `ORDER BY embedding <#> $1` |
| pgvector と同じコサイン距離にする | （旧い意味では不可） | 書き換え不要。上の 4 点目の値の変化に注意 |

### 移行時の注意

- `USING MODE 'precision'` と `<#>` の併用は `0A000`。precision を使う問い合わせは `<=>` のまま使う
- HNSW（opt-in）の物理グラフが対応する距離種別はコサインだけ（`catalog::hnsw_supported_metric`）。`<#>` に書き換えると opclass が合わず、厳密全走査へ倒れる（`ann_plan: plain_scan_opclass_mismatch`）。結果は正しいが性能が変わる。`<=>` の HNSW は内積のグラフで候補を出し、厳密なコサインで再採点する（入力が L2 正規化済みであることが契約。取りこぼしは補わない）
- ゼロノルムの行は、`<=>` の全走査では距離 NaN で昇順の末尾（`DESC` では先頭）に並び、`k` の枠を消費する。HNSW 経由では結果に出ない（#2240・#2241）
- `<->`／`<+>` は受理されるが、SQL 実行は未接続で `0A000` のまま。書く前に現状を確かめる
- NoSQL(HTTP) の `search` は新しい `<=>` と同じ意味（`distance_kind = Cosine`）。`<#>` に当たる指定は #2243 で入る予定で、それまで NoSQL から旧い順位を得る手段は無い
- Rust API の `VectorCore::search`／`SearchProvider::search` は内積スコアの降順のまま変わらない。変わったのは SQL・NoSQL の経路と `ResultRow.score` の値。`SearchProvider` には既定実装付きの `search_cosine`／`search_cosine_subset` が増えた。差し替え provider を実装している場合は、上書きするかを判断する
- テナント境界（RLS 相当）と fail-closed のエラー契約は変わらない
