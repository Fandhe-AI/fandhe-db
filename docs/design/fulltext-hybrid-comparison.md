# 全文検索・ハイブリッド検索の他 DB 比較表

- ステータス: Accepted（実装済みの挙動と他 DB の公式ドキュメントの記述を並べた比較記録。本書の追加でコードは変えない）
- 対応 Issue: #2139（親 #2138）。依存: #2111・#2117・#2123・#2127・#2131・#2135
- ビヘイビア ID ポインタ: SEARCH-12〜SEARCH-17（`docs/spec/04-behavior/`）。
  契約本文は spec を正とし、本書へは転記しない
- 他 DB の記述の基準: 下記「参照」の公式ドキュメントを 2026-11-06 に確認した内容だけを書く。
  確認できなかった点は「公式ドキュメントで確認できず」と書き、記憶で補わない
- 関連コード（シンボル名だけ。行番号は書かない）: `crates/engine/src/sparse.rs`・
  `crates/engine/src/sparse/text_query.rs`・`crates/engine/src/sparse/text_eval.rs`・
  `crates/engine/src/sparse/phrase.rs`・`crates/engine/src/sparse/match_span.rs`・
  `crates/engine/src/sparse/analysis`・`crates/engine/src/hybrid.rs`・
  `crates/engine/src/sql/hybrid_tuning.rs`・`crates/engine/src/sql/exec.rs`・
  `crates/engine/src/sql/sparse_cache.rs`・`crates/wire-server/src/http/query/search.rs`
- 関連 doc: [fulltext-query-syntax.md](fulltext-query-syntax.md)・
  [text-query-limits-cli.md](text-query-limits-cli.md)・
  [hybrid-tuning-parameters.md](hybrid-tuning-parameters.md)（第 3 節に小さな比較表あり）・
  [ts-headline-limits-cli.md](ts-headline-limits-cli.md)・
  [stemming-crate-adoption.md](stemming-crate-adoption.md)・
  [stopword-synonym-files.md](stopword-synonym-files.md)・
  [cjk-stopword-tokenizer.md](cjk-stopword-tokenizer.md)・
  [lindera-morphological-tokenizer-adoption.md](lindera-morphological-tokenizer-adoption.md)・
  [sparse-index-cache.md](sparse-index-cache.md)・
  [rrf-tie-break-determinism.md](rrf-tie-break-determinism.md)・
  [hnsw-hybrid-iterative-scan.md](hnsw-hybrid-iterative-scan.md)・
  [row-level-isolation-comparison.md](row-level-isolation-comparison.md)・
  [search-mode-and-reproducibility.md](search-mode-and-reproducibility.md)

## 目的と読み方

fandhe の全文検索（疎検索・BM25）とハイブリッド検索（密＋疎の RRF 融合）を、他 DB の相当機能と同じ軸で並べ、
「同等以上と言える点」「同等の点」「未提供の点」を区別して公開する。過大表示を避けるため、
「同等以上」は fandhe 側にテストで固定した根拠がある項目に限る。fandhe のセルは `crates/` のコードと
`docs/design` の記述だけから作り、他 DB のセルは「参照」の公式ドキュメントだけから作る。

## 前提

- 比較の単位が違う。PostgreSQL・MySQL・SQLite は RDB 内の全文索引、Elasticsearch・Qdrant・Redis は検索エンジンまたはベクトル DB
- 表 1（全文検索）は RDB 内蔵の全文索引と Elasticsearch に絞った。Qdrant・Redis の全文機能は表 2 と「書き分け」で必要な範囲だけ触れる
- 表 2（ハイブリッド）に MySQL・SQLite の列は置かない。参照した MySQL・SQLite の公式ドキュメントに、全文検索とベクトル検索を融合する組み込み機能の記述が見当たらないため
  （SQLite FTS5 のページは「組み込みの融合は記述なし」と確認。MySQL は該当ページを参照しておらず、融合の有無は確認できず）。
  PostgreSQL も参照した範囲に組み込みの融合の記述はなく、利用者が SQL で書く形になる
- fandhe は**既定ビルド**を基準にする。opt-in の feature（`english-stemmer`）や起動時 CLI フラグで変わる部分はセル内で明記する
- Elasticsearch の全文クエリ構文・ステミング・CJK の各ページは参照対象に含めていない。該当セルは「未確認」と書く
  （構文の別の比較は [fulltext-query-syntax.md](fulltext-query-syntax.md) の第 3 節）

## 比較表 1: 全文検索

| 比較軸 | PostgreSQL 17 | MySQL 8.4 | SQLite FTS5 | Elasticsearch | fandhe |
| ------ | ------------- | --------- | ----------- | ------------- | ------ |
| クエリ構文 | `websearch_to_tsquery`: 引用符・`OR`・`-`。構文エラーは出さない | BOOLEAN MODE: `+` `-` `>` `<` `( )` `~` `*` `"`。InnoDB は先頭の `+`／`-` のみ | 裸語・引用符・`*`・`^`・`NEAR`・列フィルタ・`NOT`／`AND`／`OR` | 未確認 | `parse_text_query`: 引用符・`or`・`-`・`*`。括弧なし・構文エラーなし（[D1〜D3](fulltext-query-syntax.md)） |
| 暗黙の結合 | AND | 演算子なしは任意（出現すると上位） | AND（`one OR two three` は `one OR (two AND three)`） | 未確認 | 任意語（積に絞らない）。フレーズ・除外だけが硬いフィルタ（D2） |
| フレーズ一致 | `"..."` は `<->`（語順あり） | `"..."` は同じ語を同じ順で | `"..."` | 未確認 | `"..."`。位置配列で隣接を判定し、CJK はバイグラム列の連続一致。可視集合の中だけで判定（D4。`phrase_matches_within`） |
| 近接 | `<N>` はあるが `websearch_to_tsquery` の入力では認識されない | InnoDB のみ `"w1 w2" @8` | `NEAR(a b, N)`（N 既定 10） | 未確認 | **未提供**。`NEAR(n)`・`<N>` は演算子にせず通常の語として扱う（D10）。拒否コードは無い |
| 前方一致・除外・OR | 前方一致は `websearch_to_tsquery` の入力で認識されない。`-`・`OR` はある | `*`・`-`・（`+` で必須） | `*`・`NOT`・`OR` | 未確認 | `*`・`-`・`or`。展開数・項数は上限付きで、超過は `54000`（`--max-text-query-terms`・`--max-text-query-prefix-expansions`） |
| 重み付け | `setweight`（A〜D）＋ `ts_rank` の重み配列（既定 `{0.1, 0.2, 0.4, 1.0}`） | `>` `<` で語ごとに増減 | `bm25()` に列の重み。`k1`=1.2・`b`=0.75 は固定 | 未確認 | BM25 の `k1`・`b` を `HYBRID(... k1 => , b => )`／NoSQL `hybrid` で指定（既定 1.2／0.75。範囲外は `22000`）。列の重みは**未提供**（疎側は 1 つのテキスト列のみ。専用の拒否コードは無く、構文が存在しない） |
| ハイライト・スニペット | `ts_headline`（8 オプション。出力はエスケープしない） | 公式ドキュメントで確認できず | `highlight()`・`snippet()` | highlighter は `unified`（既定）・`plain`・`fvh`・`semantic`。既定タグ・エスケープは確認できず | `ts_headline(doc, query[, options])` と NoSQL `search` の `highlight`。オプション語彙は PostgreSQL 17 に合わせ、エスケープしない。上限超過は `54000`、未知のオプションは `22023`。JOIN・派生表・CTE の射影は `42601` |
| 辞書・ステミング | Snowball ほか辞書。構成ごとに選ぶ | 公式ドキュメントで確認できず | `porter`（英語向け） | 未確認 | 英語ステミングは feature `english-stemmer` ＋ `--english-stemmer` の opt-in（既定ビルドでは起動時に拒否）。他言語・クエリごとの構成切替は**未提供** |
| ストップワード・同義語 | `stop` ファイル・synonym・thesaurus の各辞書 | ngram は英語の既定リスト。CJK は手動で作る。同義語は確認できず | ストップワードの記述は確認できず | 未確認 | `--stopwords-file`・`--synonyms-file`（起動時のみ。SQL・NoSQL・`SET` からは変更不可）。thesaurus・複数語の同義語は**未提供**（ファイル形式は拒否） |
| CJK のトークン化 | 公式ドキュメント（パーサ）に記述なし | `ngram` パーサ（`ngram_token_size` 既定 2、範囲 1〜10） | `unicode61` は CJK 専用の処理なし。`trigram` は部分文字列一致 | 未確認 | ユニグラム＋バイグラムと CJK ストップワード除去（[cjk-stopword-tokenizer.md](cjk-stopword-tokenizer.md)）。形態素解析は**未提供**（#2131 のオーナー判断で採用を見送り） |
| 書き込みとの整合 | 公式ドキュメントで確認できず | コミット時に索引へ反映。コミット前は検索から見えない | 公式ドキュメントで確認できず | 既定で約 1 秒ごとの refresh。`_refresh` で即時化 | `SparseIndexCache` が表の世代をキーにし、コミット後の次のクエリで再構築（[sparse-index-cache.md](sparse-index-cache.md)）。refresh 間隔に当たる設定は無い |

## 比較表 2: ハイブリッド融合と横断的性質

| 比較軸 | PostgreSQL 17 | Elasticsearch | Qdrant | Redis | fandhe |
| ------ | ------------- | ------------- | ------ | ----- | ------ |
| 融合方式 | 該当なし（利用者が SQL で書く） | `rrf`、`linear`（正規化 `minmax`／`l2_norm`／`none`） | `rrf`、`dbsf` | `FT.HYBRID` の `COMBINE`: RRF（既定）・LINEAR | **RRF のみ**。線形・DBSF は**未提供**。`HYBRID(... name => value)` に未知の名前を渡すと `42601` |
| 調整パラメータと既定値 | 該当なし | `rank_constant`（既定 60、1 以上）・`rank_window_size`（既定は `size`、`size` 以上）。`rrf` に重みなし | `rrf` の `k`（v1.16.0〜）・`weights`（v1.17.0〜、既定は等重み）。`k` の既定値はページ内の記述が揺れ確認できず | `CONSTANT`（既定 60）・`WINDOW`（既定 20）・LINEAR の `ALPHA`／`BETA`・`SCORER` の BM25 `k1`／`b` | `rrf_k`（既定 60）・`window`（既定 `max(LIMIT+OFFSET, 200)`）・`weights`（既定は密・疎とも 1）・`k1`／`b`（1.2／0.75）。SQL と NoSQL の `hybrid` で意味・正常値・範囲検証は共通（範囲外は `22000`）。型・形の違反の分類は異なる（`window` に 1.5・-1・1e3 を指定すると SQL は `22000`、NoSQL は型違反として `42601`。[hybrid-tuning-parameters.md](hybrid-tuning-parameters.md) 第 11 節）。`window` と `USING MODE 'precision'` の併用は `0A000`。EXPLAIN の `hybrid_rrf:`／`hybrid_bm25:` 行に実効値と指定元を出す |
| スコア内訳・系統別順位 | 該当なし | `explain=true` | 公式ドキュメントで確認できず（参照した範囲） | `EXPLAINSCORE`（8.10 以降）で RRF の順位と BM25 の内訳。`YIELD_SCORE_AS` | SQL: `hybrid_dense_rank()`・`hybrid_sparse_rank()`・`hybrid_score()`（検索 SELECT の投影最上位のみ。他位置は `42601`）。NoSQL: `hybrid.explain_score: true` で同名 3 列。その系統の候補に無い順位は NULL |
| フィルタ付きハイブリッド | 該当なし | retrievers のページに `filter` の記述なし | 参照した範囲では確認できず | `FILTER` は密側の事前フィルタ（スコアに影響しない）。`SEARCH` は絞り込みとスコアの両方 | SQL は `WHERE` と `HYBRID` を併用できる（`WHERE` 付きは疎索引キャッシュを経由しない）。密側は件数不足を再取得ラウンドで補う（[hnsw-hybrid-iterative-scan.md](hnsw-hybrid-iterative-scan.md)） |
| RLS（テナント境界）と統計 | 公式ドキュメントで確認できず（参照した範囲） | 公式ドキュメントで確認できず（参照した範囲） | 公式ドキュメントで確認できず（参照した範囲） | 返すのは読み取り権限のある文書の ID とスコアのみ（ACL） | BM25 の N・df・avgdl、前方一致の展開数、フレーズ判定、系統別順位をすべて RLS 適用後の可視集合の中で数える。他テナントの行は統計にも順位にも影響しない |
| 決定性・ページング | 公式ドキュメントで確認できず | 公式ドキュメントで確認できず | 公式ドキュメントで確認できず | `LIMIT offset num` | RRF の同点は id 昇順で決定的（[rrf-tie-break-determinism.md](rrf-tie-break-determinism.md)）。`OFFSET` は [sql-offset-paging.md](sql-offset-paging.md) |

## 書き分け

### 同等以上と言える点

各項目は、確認できた範囲の他 DB の公式ドキュメントに同等の記述が無いことと、fandhe 側のテストによる固定を根拠にする。
他 DB が実際には備えていないという意味ではない。

- 統計を RLS 適用後の可視集合で数える（不可視行の存在が BM25 の値・展開数の上限判定に影響しない）。根拠:
  `sparse.rs` の `search_within_statistics_are_isolated_from_invisible_docs`・`score_within_statistics_are_isolated_from_invisible_docs`、
  `tests/sql_named/search12_hybrid_text_syntax.rs` の `search12_prefix_expansion_counts_only_visible_terms`、
  `tests/core_misc/sparse_phrase.rs` の `phrase_never_returns_invisible_documents`
- ハイライトが RLS・列の権限の暗黙適用の下で動き、不可視行では評価されない（SQL と NoSQL の両方）。根拠:
  `tests/table_rls/rls10_ts_headline_paths.rs` の `ts_headline_rls_matrix`・`ts_headline_errors_are_invariant_and_clean`、
  `wire-server/tests/http_nosql/rls10_nosql_highlight_paths.rs`
- 書き込み後の反映に待ち時間の設定が要らない（コミット後の次のクエリで再構築）。Elasticsearch は既定で約 1 秒ごとの refresh。
  根拠: `tests/core_misc/sparse_cache.rs` の `acceptance2_insert_after_hybrid_query_invalidates_cache_and_reflects_new_row`、
  テナントをまたいだキャッシュ漏えいの否定 `acceptance3_hybrid_cache_does_not_leak_across_tenants`

### 同等の点

- 引用符・除外・OR（PostgreSQL `websearch_to_tsquery` と SQLite FTS5 に相当）。前方一致は SQLite FTS5 と共通で、`websearch_to_tsquery` の入力では認識されない。不正な入力でエラーを出さない点は PostgreSQL と同じ。
  SQL と NoSQL の結果一致は `tests/http_nosql/nosql2_search.rs` の `hybrid_text_syntax_matches_sql_for_each_operator`
- `ts_headline` のオプション語彙・既定値（PostgreSQL 17 に合わせた。エスケープしない点も同じで、表示側の責務）
- BM25 の `k1`・`b` の既定値（SQLite FTS5・Redis の記述と同じ 1.2／0.75）。fandhe は文ごとに指定できる
- RRF の定数 60（Elasticsearch・Redis の既定と同じ）と、重み付き RRF（Qdrant の `weights` に相当）
- 系統別の順位・スコア内訳の返却（Elasticsearch の `explain`、Redis の `EXPLAINSCORE` に相当）。SQL の投影列で返せる点が形の違い。
  根拠は `hybrid.rs` の `rrf_fuse_keeps_per_channel_rank_hand_computed`、`tests/sql_named/search17_hybrid_explain_score.rs`、
  NoSQL との一致 `nosql2_search.rs` の `hybrid_explain_score_matches_sql_projection_and_default_is_unchanged`。優劣は主張しない
- 調整パラメータの SQL／NoSQL パリティ（正常値・範囲検証・コード `22000` の一致。型・形の違反の分類は SQL `22000`／NoSQL `42601` で異なる）: `nosql2_search.rs` の `hybrid_tuning_keys_match_sql_named_args`・`hybrid_tuning_rejections_match_sql_codes`
- 同点が多い入力での決定性: `tests/hnsw_hybrid_refetch.rs` の `tie_inducing_corpus_hybrid_search_terminates_and_is_deterministic`、
  `tests/core_misc/sparse_text_eval.rs` の `ranking_is_deterministic_across_runs`。他 DB の記述は確認できず、比較はしない

### 未提供の点

「拒否する契約がある」ものと「機能が無いだけ」のものを分ける。どちらもテナント境界は緩めない。

| 項目 | 区分 | 挙動 |
| ---- | ---- | ---- |
| 線形・DBSF 等の別の融合方式 | 拒否する契約あり | 未知の名前付き引数は `42601`。融合は RRF のみ |
| `window` と precision モードの併用 | 拒否する契約あり | `0A000`（取得件数が可視件数に依存し、束縛段で大小を決められないため） |
| 検索語の項数・前方一致の展開数・フレーズ長の超過 | 拒否する契約あり | `54000`（一部だけで黙って検索しない）。入口のバイト長・一意語数の超過は `22000` |
| ハイライトの出力長・断片数の超過、JOIN・派生表・CTE の射影 | 拒否する契約あり | 超過は `54000`、射影は `42601` |
| ステミングの指定（既定ビルド） | 拒否する契約あり | `--english-stemmer` は起動時に固定文言で拒否（黙って無視しない） |
| 近接（`NEAR`・`<N>`） | 機能が無いだけ | 構文として認識せず通常の語として扱う（エラーにしない）。PostgreSQL の `websearch_to_tsquery` も `tsquery` 演算子を入力で認識しない点は同じ |
| 列ごとの重み（`setweight`・FTS5 の列重み相当） | 機能が無いだけ | 疎側は 1 つのテキスト列のみ。NoSQL の疎側は `body` 列に固定 |
| 形態素解析（日本語） | 機能が無いだけ | #2131 のオーナー判断で採用を見送り。ユニグラム＋バイグラムのまま |
| thesaurus・複数語の同義語、クエリごとの検索構成 | 機能が無いだけ（ファイル形式の違反は起動時に拒否） | 構成は起動時のみ。他テナントの結果を変えないための設計 |
| 英語以外のステミング | 機能が無いだけ | 英語（Snowball English）のみ。`ts_headline` と字句リランカーにはステミング未適用 |

### 運用上の差

- ストップワード・同義語・ステミングは起動時の設定で、テナント単位・クエリ単位では変えられない。PostgreSQL の構成（クエリごとに指定可）より柔軟性は低く、他テナントへの影響を構造で排す設計である
- 疎索引は表の世代単位で再構築する。書き込みが多い表では再構築が増える（[sparse-index-cache.md](sparse-index-cache.md)）。Elasticsearch の refresh のような間隔の調整手段は無い

## 参照

- PostgreSQL 17: https://www.postgresql.org/docs/17/textsearch-controls.html ・
  https://www.postgresql.org/docs/17/textsearch-dictionaries.html ・
  https://www.postgresql.org/docs/17/textsearch-parsers.html（2026-11-06 確認）
- MySQL 8.4: https://dev.mysql.com/doc/refman/8.4/en/fulltext-search-ngram.html ・
  https://dev.mysql.com/doc/refman/8.4/en/fulltext-boolean.html ・
  https://dev.mysql.com/doc/refman/8.4/en/innodb-fulltext-index.html（2026-11-06 確認）
- SQLite FTS5: https://www.sqlite.org/fts5.html（2026-11-06 確認。ページが長く、取得できたのは前半のみ）
- Elasticsearch: https://www.elastic.co/docs/reference/elasticsearch/rest-apis/reciprocal-rank-fusion ・
  https://www.elastic.co/docs/reference/elasticsearch/rest-apis/retrievers ・
  https://www.elastic.co/docs/reference/elasticsearch/rest-apis/highlighting ・
  https://www.elastic.co/docs/manage-data/data-store/near-real-time-search（2026-11-06 確認）
- Qdrant: https://qdrant.tech/documentation/concepts/hybrid-queries/ ・
  https://qdrant.tech/documentation/concepts/indexing/（2026-11-06 確認。ページが長く、後半は未確認）
- Redis: https://redis.io/docs/latest/commands/ft.hybrid/（2026-11-06 確認）
- fandhe: doc・テスト・コードを関数名とファイル名で参照する（行番号は書かない）
- 他 DB の記述は確認日時点のもので、更新への追従は本書の範囲外
