# ADR: スカラー列二次索引の設計検討（Issue #359）

- ステータス: Accepted 提案（オーナー承認待ち。Issue #472 へのオーナー承認コメントを
  もって Accepted 確定。承認コメントが残るまで #473 以降は着手しない。判断記録は
  「判断記録（オーナー記入欄）」節参照）
- 対応: Issue #359（データモデル・一貫性・RLS 境界の確定は Issue #472。
  親 #471〔スカラー列二次索引・Phase 2〕→ 親 #457〔Phase 2〕→ ルート #455。
  #472 → #473 → #474 → #475 → #476 の直列依存）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-12）・
  `docs/spec/04-behavior/rls.md`・`docs/spec/05-tasks.md`（TASK-75・TASK-89/133 系・
  SQL-6・SQL-13・SQL-14・TASK-147・TASK-162）。
  spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/catalog.rs`（`ColumnType`・テーブルスコープ行ストア
  `user_rows_table_def(user_rows_table_name(...))`・`TABLE_GENERATION_TABLE`・
  `table_generation_in_txn`）・`crates/engine/src/arena.rs`（`build_filtered_with_rows`）・
  `crates/engine/src/declarative_filter.rs`・`crates/engine/src/sql/allowlist.rs`
  （`WherePredicate`）・`crates/engine/src/sql/udf_call.rs`（`BoundExpr`）・
  `crates/engine/src/sql/plan.rs`（`ExecutionPlan`）・
  `crates/engine/src/policy.rs`（`PolicyContext::is_visible`）・
  `crates/engine/src/sql/arena_cache.rs`（`SqlArenaCache`・Issue #363）・
  `crates/engine/src/sql/sparse_cache.rs`（`SparseIndexCache`・Issue #357）・
  `crates/engine/src/sql/hnsw_cache.rs`（`HnswIndexCache`・`classify_ann_plan`・
  Issue #408・#411）・`crates/engine/src/hnsw.rs`（`Ratio`・`full_scan_ratio`・
  Issue #409）
- 関連 Issue: #360（パースキャッシュ検討・兄弟）・#363（VectorArena 世代整合キャッシュ）・
  #367（ANN 索引採否検討）・#464（段別プロファイル実測）・#477（可視ビットマップ
  世代整合キャッシュ・兄弟 Phase 2 Issue）
- 関連 doc: `docs/design/scan-stage-profile.md`（Issue #464。本 ADR の実測根拠）・
  `docs/design/sql-arena-generation-cache.md`（#363）・
  `docs/design/sparse-index-cache.md`（#357）・
  `docs/design/hnsw-generation-cache.md`（#408）・
  `docs/design/hnsw-rls-cardinality-switch.md`（#409）・
  `docs/design/explain-search-engine-exposure.md`（#411）・
  `docs/design/benchmark-judgement-policy.md`（§5・§7.1・§7.2）・
  `docs/design/hotpath-implementation-survey.md`（§5・§9-#2）・
  `docs/design/table-generation-rejection-granularity.md`（Issue #285）
- 本 ADR は**設計検討のみ**であり、実装コード（`crates/`）は含まない。実装タスクの
  起票は Issue #473〜#476 として既に完了しており（親 #471 配下）、本 ADR はそれらが
  従う契約を確定する
  （[out-of-scope-tracking](../../.claude/rules/out-of-scope-tracking.md)）

## 背景

現状、SQL 表層の `WHERE` スカラー条件（等価・前方一致）は、
`sql/exec.rs::execute_statement` から `arena.rs::VectorArena::build_filtered_with_rows`
（および txn 内版 `_in_txn`）を経由し、RLS 段（`predicate`）→ SCALAR 段
（`on_visible_row`）という固定順序の**全行走査（O(N)）＋インライン比較**で評価される。
等価・前方一致条件は `declarative_filter.rs`（`DeclarativeFilter::equals` /
`starts_with` → `MetadataFilter`）としてバインドされ、`sql/plan.rs::ExecutionPlan`
の `scalar_prefilter`（SCALAR 段を DISTANCE 段より先に評価するか）によって適用順序が
決まる。

Phase 1（#345 配下）で行ったデコード段の最適化（`storage::decode_row_embedding_and_metadata_into`
のスクラッチバッファ再利用等、Issue #314）は行あたりコストの定数倍削減であり、**走査
する行数そのものを減らす技法ではない**。走査行数を減らすには二次索引が必要になるが、
索引は書き込み経路・RLS 境界・キャッシュ世代機構（`catalog.rs::TABLE_GENERATION_TABLE`・
`core.rs::PrefilterCache`）に新しい不変条件を持ち込むため、実装に先行して設計を整理
する。

参照実装として PostgreSQL の B-tree 索引選択（`access/nbtree/`）・SQLite のプランナ
（`where.c` / `wherecode.c`）、および Issue #359 のコメント（オーナー member）で補足
された Qdrant のスカラー列索引（`lib/segment/src/index/field_index/`）とカーディナリ
ティ推定（`query_estimator.rs` 等）を検討対象とする。これらはいずれも公開 OSS
（PostgreSQL: PostgreSQL License、SQLite: Public Domain、Qdrant: Apache-2.0）であり、
本 ADR では設計概念（選択度推定に基づく索引経路 / 全走査経路の切替）の要約のみを用い、
コードの転記は行わない。

### 許可リスト述語の事実整理（本 ADR 確定時点での制約）

`sql/allowlist.rs::WherePredicate` が受理する形状は `Equality`（`col = '<lit>'`）・
`Prefix`（`col LIKE '<p>%'`）・`Expression`（比較演算子 `> < >= <= =` を頂点に持つ
`Expr::Binary` のみ）・`PredicateCall`（`visible()` 等、空引数）の 4 種であり、
**`IN`・`BETWEEN` 構文は現行許可リストに存在しない**。`sql/udf_call.rs::BoundExpr`
（`Expression` 側の束縛先）は `Number`・`IdRef`（疑似列 `id`）・`VectorRef`・
`Builtin`・`Binary`・`WasmCall` を取り得る（`Text` 列参照は含まれない）。

**訂正（Issue #472 レビュー指摘）**: 本 ADR の初版は「`Expression` 形状での範囲比較は
疑似列 `id` に限られる」と記していたが、これは誤り——`crates/engine/tests/sql_udf_call.rs`
の `equality_predicate_and_expr_predicate_combine_in_the_same_where_clause`
（`WHERE lang = 'ja' AND vec_norm(embedding) > 2.0 ORDER BY ...`、同ファイル
373 行目付近）が示すとおり、`Expression` は `VectorRef`・
`Builtin`（`vec_norm` 等）を頂点に持つ比較も受理し、**現行許可リストで実際に成立する
`Expression` は疑似列 `id` の比較に限られない**。加えて `catalog.rs::ColumnType` は
`Text` と `Vector(u32)` の 2 種のみで、**等価・前方一致索引の対象になり得るスカラー
列は `Text` 列に限られる**——`Expression` 形状の比較は列型を問わず幅広く成立しうる
一方、それを二次索引で高速化できるのは疑似列 `id` の単純比較（後述「索引対応述語の
狭い定義」）だけであり、「`Expression` の構文上の広さ」と「索引が対応できる範囲の
狭さ」は別の話である。この区別を「採用案（候補 B）の確定仕様」節の索引対応述語の
定義（狭義の限定列挙）に反映する。

この事実は本 ADR が定める索引の対応述語（「採用案（候補 B）の確定仕様」節）を
「等価・前方一致・（`id` 疑似列限定の狭義比較のみ）」に限定する根拠であり、`IN`／
`BETWEEN` を含む索引対応の拡張は許可リスト・構文自体の spec 側確定を前提とする
（「spec 側への申し送り」節）。**索引対応述語として明示的に判定できない `Expression`
（`Builtin`・`WasmCall`・`VectorRef` を含む式、`IdRef` と数値リテラルの単純比較の
形に一致しない式のすべて）は、候補削減を一切行わず既存評価器（`sql/expr_program.rs`）
へそのまま渡す残余述語として扱う——列挙による許可制（明示的に索引対応と判定できた
ものだけを索引対応とし、それ以外はすべて残余）であり、除外による拒否制（残余の形状を
列挙し、それ以外を索引対応とみなす）ではない。この規則は「索引対応述語の狭い定義」
節・「述語適用規則」節で確定する。

### 実クエリ形状（Issue #464 実測が使うクエリ・本 ADR のコスト見積りの根拠）

`scripts/crossdb_bench/self_db.py` の各フェーズは次の SQL 文字列を固定して計測して
いる（本 ADR が「索引対応述語 AND 残余述語」規則を検討する直接の入力）:

| フェーズ | クエリ |
| -------- | ------ |
| `vector_knn_where` | `SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '<vec>' LIMIT 10` |
| `where_compound_count` | `SELECT COUNT(*) FROM docs WHERE visible() AND id > 100 AND lang = 'ja'` |
| `group_by_having` | `SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 5` |
| `agg_count` | `SELECT COUNT(*) FROM docs`（`WHERE` を持たない） |

`where_compound_count` は `visible()`（`PredicateCall`。索引対象外・残余述語）・
`id > 100`（`Expression`。疑似列 `id` の比較のみ索引対応し得る）・`lang = 'ja'`
（`Equality`。索引対応）の AND 結合であり、「索引対応述語で候補スロット集合を
得て、残余述語は候補のみに評価する」規則の実例になる。

`agg_count`・`rls_isolation`（`SELECT COUNT(*) FROM docs`。`WHERE` を持たない）は
`sql/aggregate.rs`・`sql/group_by.rs` の `on_visible_row` を通らないため、本 ADR
（`WHERE` スカラー条件の索引化）の対象**外**である——両フェーズの改善は兄弟 Issue #477
（可視ビットマップ世代整合キャッシュ）が対象にする（「Issue #464 実測の反映」節参照）。

`sql/aggregate.rs`・`sql/group_by.rs` は現状 `sql::arena_cache::SqlArenaCache` を
経由せず毎クエリ redb を全行走査する。「採用案（候補 B）の確定仕様」節の集計・
`GROUP BY` 経路がこの構造を前提に索引と `VisibleBitmapCache`（#477）の層分担を
定義する。

## 索引データモデル（redb 上の設計比較）

### 候補比較（候補 A: redb 永続二次テーブル vs 候補 B: メモリ常駐世代整合キャッシュ）

Issue #472 の調査で、後続実装 Issue（#473〜#476）が前提とする方式（`(table,
PolicyContext)` × テーブル単位世代キーで `sql::arena_cache::SqlArenaCache`
〔#363〕と同型の fail-closed 契約を持つ索引キャッシュを可視アリーナから構築する
＝メモリ常駐方式）と、本 ADR の初版（Proposed 時点）が前提としていた redb 永続
二次テーブル方式が乖離していることが判明した。両者を候補として比較し採用案を
確定する。

| 観点 | 候補 A: redb 永続二次テーブル（Index-T/Index-P 二層。後述） | 候補 B: メモリ常駐・テーブル世代整合キャッシュ |
| --- | --- | --- |
| データモデル | `(tenant_id, value_key, id)`／`(value_key, tenant_id, id)` の 2 テーブル | `sql::arena_cache::SqlArenaSnapshot`（RLS 可視行のみ）の metadata から `row_codec::scan_scalar_columns` で構築する派生構造。等価: `HashMap<Text, Vec<slot>>`（slot 昇順）、前方一致（＋`id` 比較の任意拡張）: `(value, slot)` の整列配列を二分探索でレンジ走査 |
| 一貫性 | 行書き込みと同一 `write_txn`・DDL 8 項目の同期契約（後述「候補 A を採る場合の契約」節）・`table_generation_bump_coverage.rs` 対象拡大が必要 | 書き込み経路は**無変更**。`catalog::table_generation_in_txn` の世代が進めば失効し次クエリで再構築（`SqlArenaCache`・`sql::sparse_cache::SparseIndexCache`・`sql::hnsw_cache::HnswIndexCache` と同型） |
| RLS 境界 | 索引ヒットは候補であり `is_visible` 再適用必須。Index-P 由来統計のみ横断保持 | 索引は ctx 可視行のみから構築されるため候補 ⊆ 可視集合が構造的に成立。統計は per-ctx のみで、他テナント `Private` 行の存在・分布が候補集合・統計値へ混入することは無い（`sql-arena-generation-cache.md`「安全性」節と同じ論法）。**ただし処理時間の独立性は保証しない**——採用するテーブル単位世代キャッシュでは他テナントの `Private` 行の書き込みでも同じテーブルの世代が進み `SqlArenaCache`／本索引ともにミスして再構築が必要になり、cold 時の構築は不可視行を含む全行走査であるため、他テナントの書き込み頻度・データ量は本索引の cold/hot 発生率・構築コストという処理時間の観点には影響しうる |
| write amplification | 列数 ×（1〜2）倍の追加書き込み | 0（読み取り側の初回構築コスト O(N) のみ） |
| メモリ | redb ページ（永続） | 索引バイト量を容量上限に計上（既存 `SqlArenaCache` 容量上限と同桁の独立上限、または共有上限。「採用案の確定仕様」節） |
| 対応可能な述語 | 等価・前方一致 | 等価・前方一致・`id` 比較（現行許可リストで索引可能な形の全部） |
| Issue #464 実測との整合 | W1〜W3（後述）を省けるが A1〜A5（redb 走査。`agg_count`／`rls_isolation` 側）は残る | 同左。加えて `W0-cold`（11.9ms 相当）→`W0-hot`（1.1ms 相当）差が示すとおりスナップショット常駐が前提のため候補 B は既存 `SqlArenaCache` と自然に同居する |
| 採否 | **見送り**（Rejected ではなく再評価条件付き。「見送りの再評価条件」参照） | **採用案** |

**採用理由**:

1. production 書き込み経路・DDL 契約・`operation_id` 台帳（TASK-101・RECOVER-10）に
   新しい不変条件を持ち込まない
2. `sql::arena_cache::SqlArenaCache`（#363）・`sql::sparse_cache::SparseIndexCache`
   （#357）・`sql::hnsw_cache::HnswIndexCache`（#408）で確立済みの `(table, ctx)` ×
   テーブル単位世代の fail-closed 契約をそのまま再利用でき、RLS 境界の論証が既存
   3 件の doc と同一構造になる（新しい安全性論証を 1 から組み立てる必要がない）
3. Issue #464 の段別帰属表で本 Issue（#471）が省き得る段は W1〜W3 のみであり
   （「Issue #464 実測の反映」節）、候補 B で十分到達できる。候補 A が追加で
   省ける段（A1〜A5・`agg_count`／`rls_isolation` 経路）は本 ADR の対象クエリ
   （`WHERE` を持つもの）には現れない

**見送りの再評価条件**: 可視行集合がメモリ上限を超える規模、または cold クエリ
（`SqlArenaCache` が毎回無効化される書き込み頻発ワークロード）が支配的になり
スナップショット常駐が成立しなくなった場合に候補 A を再検討する。

以降「索引データモデル」「一貫性」「RLS 境界」の既存節は**候補 A を採る場合の
契約（見送り時点の設計記録として保存）**として残し、採用案（候補 B）の確定仕様は
次節にまとめる。

## 採用案（候補 B: メモリ常駐・テーブル世代整合キャッシュ）の確定仕様

Issue #473〜#476 はこの節の契約に従う。

- **キー**: `(table, PolicyContext)` 完全一致 × `catalog::table_generation_in_txn`
  の世代。`PolicyContext` の `Eq` がテナント ID・許可可視性集合の値比較であること
  （`policy.rs`）に依拠する——既存 `SqlArenaCache`・`SparseIndexCache`・
  `HnswIndexCache` と同一のキー形状であり、ctx が 1 bit でも異なれば別エントリに
  なる（security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）
- **構築元**: `SqlArenaSnapshot` の metadata（RLS 段適用済み）。索引はスナップ
  ショットの**派生データ**であり、スナップショットと同じ世代で生成・失効する。
  NULL 値はエントリを作らない（索引は「値が存在する行」のみを列挙し、NULL 述語は
  従来どおり全走査にフォールバックする）。`Text` 値は無加工（`Equality` は完全
  一致、`Prefix` はバイト列前方一致）で `declarative_filter::MetadataFilter::matches`
  と同一判定になることを不変条件にする（`row_codec::scan_scalar_columns` の借用
  結果からの複製量は容量上限で検証する）
- **`insert` 契約（`SqlArenaCache` との意図的な非対称）**: 世代競合時は `None` で
  拒否する（`core.rs::PrefilterCache` と同じ fail-closed 契約・Issue #280）。
  `SqlArenaCache::insert` が世代競合時も呼び出し元へ常に構築済み `Arc` を返すのは
  「呼び出し元が自分の `read_txn` の結果をそのまま使う」ためだが、索引は候補集合
  の**正しさ**が世代に結び付く派生データであり、競合時は索引経路を使わず全走査へ
  縮退するのが fail-closed 側であるため、この非対称を意図的に選ぶ
- **索引対応述語の狭い定義**（Issue #472 レビュー指摘 (1) を反映。「許可リスト述語の
  事実整理」節の訂正の帰結）: 索引対応述語は次の 2 形のみとし、列挙による許可制で
  判定する——(i) `Equality`（`Text` 列 `=` リテラル）・`Prefix`（`Text` 列
  `LIKE '<p>%'`）、(ii) `Expression` のうち `BoundExpr::Binary` の直下がちょうど
  `IdRef` 1 個と `Number` リテラル 1 個（左右どちらの位置でも可）からなる単純比較
  （`id > 100`・`100 < id` 等）**のみ**。この 2 形に構文的に一致しない `Expression`
  はすべて——`Builtin`（`vec_norm(embedding) > 2.0` 等）・`WasmCall`・`VectorRef` を
  含む式、`IdRef`/`Number` 以外の項を持つ `Binary`、ネストした算術式——例外なく
  残余述語として扱う。索引対応かどうかを「残余の形状を列挙し、それ以外を索引対応と
  みなす」除外制では判定しない（誤って新しい `BoundExpr` variant や複雑な式を
  索引対応と誤判定する経路を作らないため）。SQL-24・TASK-208（Issue #914）で
  `LIKE` の中間一致・後方一致・`_` を受理した際も、この狭い定義は変えていない
  ——`Prefix`（末尾 `%` のみの純粋な前方一致）はそのまま索引対応（`Equality`
  相当は `FilterOp::Equals` へ振り分け）だが、それ以外の一般形
  （`FilterOp::Like`）は索引対応述語に含めず常に残余（`PlainScan`）として扱う
  （詳細: `docs/design/like-wildcard-patterns.md`）。
- **述語適用規則**: 索引対応述語（上記の狭い定義に一致するもの）ごとに候補スロット
  集合（昇順）を得て AND は交差（マージ）、残余述語（`PredicateCall`、および上記の
  狭い定義に一致しない `Expression` すべて）は候補のみに `sql/expr_program.rs`
  （Issue #353）で評価する。`where_compound_count`
  （`visible() AND id > 100 AND lang = 'ja'`）はこの規則の実例——`lang = 'ja'`
  （`Equality`）と `id > 100`（`Expression`・`IdRef` と `Number` の単純比較）の
  交差を候補集合とし、`visible()` は候補のみへ適用する。`sql/plan.rs::ExecutionPlan`
  の `scalar_prefilter`（SCALAR 段を DISTANCE 段より先に適用するか）の意味・
  `precision` モードの契約（TASK-162）は不変。DISTANCE 先行（`!scalar_prefilter`）
  の事後フィルタ経路は索引対象外（候補集合が可視全集合のため索引を引く意味が無い）
- **エラー契約の維持（Issue #472 レビュー指摘 (2)）**: 現行の逐次評価
  （`sql/expr_program.rs` の「評価順序の保存」節が定める、`WHERE` の複数述語を
  `Vec<BoundExpr>` として宣言順に AND 短絡評価する契約）では、エラーを起こしうる
  述語（0 除算・非有限値化・`id_as_finite_scalar` の `id > 2^53` 拒否等）が宣言順で
  先に来れば、後続の索引対応述語の真偽に関わらず評価されエラーになりうる
  （例: `WHERE 1/0 > 0 AND id < 0` は `id < 0` が u64 の `id` に対し常に偽でも、
  宣言順で先に評価される `1/0 > 0` が可視行 1 件につき必ず `22000` を返す。
  `crates/engine/tests/sql_udf_call.rs` の `constant_subexpression_error_still_fails_a_query_with_a_visible_row`
  参照）。索引による AND 候補削減は「索引対応述語をすべて先に評価して候補を絞り、
  残余述語は候補のみに評価する」規則であり、宣言順を無視して索引対応述語を先に
  評価する——これは残余述語がエラーを起こしうる場合、宣言順評価では発生していた
  はずのエラーを索引経路が黙って握りつぶし得ることを意味し、fail-closed の趣旨に
  反する。この非等価を避けるため、索引による候補削減（AND 交差・COUNT 直接返却の
  いずれも）は**残余述語がすべてエラーを起こし得ないと構造的に保証できる場合に
  限り**適用する。現行許可リストで残余述語になり得るのは `PredicateCall`
  （`visible()` など。空引数でエラーパスを持たない）と、上記「索引対応述語の狭い
  定義」に一致しない `Expression`（`Builtin`・`WasmCall`・算術式等、0 除算・非有限
  値化・WASM 実行時エラー等のエラーパスを持ちうる）の 2 種であり、**残余述語に
  `PredicateCall` 以外（エラーを起こしうる `Expression`）が 1 つでも含まれる場合は
  索引による候補削減を行わず、全走査＋既存の宣言順逐次評価（`sql/expr_program.rs`）
  へ縮退する**。残余述語が `PredicateCall` のみ（`where_compound_count` の
  `visible()` はこの形）の場合に限り、索引対応述語の交差で得た候補へ `PredicateCall`
  を適用する規則を適用してよい。この判定（残余述語の集合からエラー起因の
  `Expression` の有無を静的に確認する）は #474／#475 の実装契約とする。
  加えて、索引対応述語のうち `id` の単純比較（`IdRef`/`Number`）を索引化する際は、
  各候補行の `id` に対し `sql/udf_call.rs::id_as_finite_scalar`（`id > 2^53` を
  `22000` で拒否）と同一の検査を索引構築・候補判定の経路でも必ず適用する——`u64` の
  `id` を直接比較する高速経路（f64 変換を経ない比較）で `id_as_finite_scalar` の
  検査を省略すると、`id > 2^53` の行が索引経由では黙って通過し得るため、現行の
  逐次評価（`eval` が毎行 `IdRef` を `id_as_finite_scalar` 経由で評価する）が
  与える `22000` の fail-closed 契約を破る。集計の `COUNT(*)` fast path
  （候補件数を直接返す経路。下記「集計・`GROUP BY` 経路」参照）は、この
  `id_as_finite_scalar` 相当の検査を経ていない候補（索引構築時に検査済みでない、
  または索引構築後にテーブルへ `id > 2^53` の行が追加された等）を含む可能性がある
  世代では使わず、通常の全走査＋逐次評価へ縮退する。
- **選択度切替**: 索引ヒット件数 ÷ 可視行数の比が閾値（`hnsw.rs::Ratio`／
  `full_scan_ratio` と同型の整数比。既定 1/10 の先例あり・Issue #409）を超える
  場合は全走査へフォールバックする。既定値は #474 で仮置きし #476 で実測確定する。
  統計は per-ctx 索引自身のカーディナリティのみ（他テナントの `Private` 行の
  存在・分布が統計値へ混入することは無い設計を維持する。ただし「RLS 境界」節の
  注記のとおり、テーブル単位世代キャッシュである以上、他テナントの `Private` 行
  書き込みによる世代進行は cold/hot 発生率という処理時間の観点には影響しうる）
- **集計・`GROUP BY` 経路（#475）**: `sql/aggregate.rs`・`sql/group_by.rs` は現状
  redb 直走査のため、兄弟 Issue #477 の `VisibleBitmapCache`（RLS 可視性の権威）を
  下層、本索引（可視スナップショットの派生）を上層とする層分担を定義する。
  `COUNT(*)` は索引対応述語のみで `WHERE` が構成される場合（`where_compound_count`
  の `id > 100 AND lang = 'ja'` 部分）に候補件数を直接返す fast path を許容する。
  `GROUP BY` キー列が索引列なら等価索引のキー列挙でグループ列挙できる
  （`group_by_having` の `GROUP BY lang` はこの形に該当する）。ただし本索引は
  NULL 値のエントリを作らない設計（前掲）のため、等価索引のキー列挙だけでは
  `lang IS NULL` に相当する NULL グループを列挙できない——既存
  `group_by.rs` は NULL キーを `null_group` として単一グループに集計し
  `HAVING` 判定（例: `HAVING COUNT(*) > 1`）の対象に含めるため、索引経路の
  キー列挙をそのままグループ集合として使うと NULL グループが結果から欠落
  しうる。この fast path は**索引が保持する非 NULL キーの列挙に加え、
  「NULL 値の可視行が 1 件以上存在するか」を索引と同一世代で判定できる
  場合に限り `null_group` を候補集合の末尾へ補って**成立させる（候補
  補完不能な場合——索引が非 NULL 専用でありこの判定手段を持たない構築
  時点では——`GROUP BY` fast path 自体を不採用とし、`sql/group_by.rs` の
  既存の全走査（`VisibleBitmapCache` 下層のみを経由する経路）へ縮退する。
  索引経路を使うかどうかの選択自体が fail-closed 側に倒れ、NULL グループを
  欠落させたまま返す経路は作らない。この判定手段の具体化（索引メタデータへ
  の NULL 件数フィールド追加か、`VisibleBitmapCache` 側からの補完か）は
  #475 の実装時に確定する。TABLE-12 のキー／ヘッダ tenant 整合検査は索引
  構築時に全件実施し省略しない

  **#475 実装確定**: NULL 補完は `ScalarIndex::slots_without_value`（索引済み
  全値のスロットを `row_count` 長のビットマップで被覆し、被覆されなかった
  スロットを NULL とみなす差分計算）として実装した——`ScalarIndex::build` が
  当該列の非 `NULL` 値を**すべて**索引化するか索引全体の構築自体を諦めるかの
  いずれかである契約（本 ADR「データモデル」節）により、この差分は常に正確な
  NULL 集合になる。NULL 件数フィールドの追加は不採用。`COUNT(*)` の候補件数
  直接返却 fast path は、レビューで再検証省略が問われうる（#473/#474 の判定
  同一性テストのみを根拠にする）リスクを避けるため**不採用**とし、他の集計と
  同じ候補走査＋`matches_all`／式述語の再検証を経る設計へ統一した（コストは
  候補数に比例するのみで実質的な差は小さい）。詳細・前後比較は
  `docs/design/scalar-index-aggregate.md` 参照。
- **`EXPLAIN`（#474）**: `sql::hnsw_cache::classify_ann_plan`（Issue #411）と同型の
  純粋関数 `classify_scalar_plan` を単一情報源にし `scalar_plan:`（例:
  `plain_scan`／`index_equality`／`index_prefix`／`index_conjunction`）を静的判定
  として追記する。件数・カーディナリティ・閾値比較結果は非露出のまま
  （`explain-search-engine-exposure.md` の非露出方針を踏襲する）
- **容量・DoS**: エントリ数上限・総バイト上限・LRU・stale 一括破棄を
  `SqlArenaCache` と同手順で持つ。untrusted 由来の値長は既存の行エンコード上限で
  既に検証済みである
- **失敗時**: 構築失敗・容量超過・世代競合はいずれも「索引不使用（全走査）」へ
  縮退する。stale な索引で応答する経路を作らない

## 候補 A を採る場合の契約（見送り。設計記録として保存）

以下は候補 A（redb 永続二次テーブル）の物理設計・一貫性・RLS 境界の詳細である。
「見送りの再評価条件」に該当し候補 A へ切り替える場合、この節を実装契約の
出発点とする。

### 物理キー案（旧「A 案／B 案」を A-1／A-2 へ改名）

| 案 | 構造 | 特徴 |
| -- | ---- | ---- |
| A-1 | 列ごとの `MultimapTable<(tenant_id, value_key), id>` | 等価条件のヒット列挙に直接対応。前方一致には別途レンジスキャン可能な構造が要る |
| A-2 | 複合キー `TableDefinition<(tenant_id, value_key, id), ()>` | redb のタプルキーは辞書式全順序を持つため、`(tenant_id, prefix..)` によるレンジスキャンで等価・前方一致の双方に対応できる |

A-2 は前方一致（`declarative_filter.rs::starts_with` / `parse_prefix_pattern`）への
拡張性で A-1 に優位なため、**推奨は A-2** とする。ただし最終選定は実装フェーズの
プロトタイプ計測で確定する。

**索引は単一テーブルではなく二層構成とする**（RLS 境界レビューで判明した不整合の
是正）:

| 層 | キー | 収録対象 | 用途 |
| -- | ---- | -------- | ---- |
| 自テナント索引（Index-T） | `(tenant_id, value_key, id)` | そのテナントが所有する全行（`Public`・`Private` 問わず） | `is_owner` と同じテナント境界でレンジスキャンを閉じる。A-2 どおり |
| Public 横断索引（Index-P） | `(value_key, tenant_id, id)` | `visibility = Public` の行のみ（全テナント） | `policy.rs::PolicyContext::is_visible` が定義するとおり `Public` 行は元々全テナントから可視のため、この層への収録・横断スキャンは新たな漏えいを生まない |

`WHERE` 条件の索引経路は Index-T（自テナント prefix scan）と Index-P（value_key
prefix scan）の**和集合**を候補として返す。行ストア（`catalog.rs::user_rows_table_def
(user_rows_table_name(...))`）の物理キーが `(tenant_id, id)` の複合キーであるとおり
`id` はテナント内でのみ一意な識別子であるため、和集合の重複排除は `id` 単独ではなく
`(tenant_id, id)` の組で行う（`id` 単独で dedup すると異なるテナントの別行を同一視
しうる）。両層とも索引ヒット後の可視性再判定（`is_visible` の再適用）は必須で変わ
らない。`Private` 行のエントリは Index-T にのみ存在し、Index-P には一切現れない
——他テナントの `Private` 行の存在・分布は索引のどの層からも観測できない設計とする。

設計上の留意点:

- 値エンコーディング: `Text` 値は memcmp 順序を保つ正規化（バイト列比較で辞書式順序が
  意味的順序と一致する形）が必要。キー長には上限を設け、untrusted 由来の値をそのまま
  無制限にキーへ連結しない（`coding-rust.md` の untrusted 入力規約——長さ上限検証後に
  アロケーションする）
- NULL 値は索引エントリを作らない
- `visibility` が `Private` → `Public` へ更新される場合（更新経路が存在する場合）は
  Index-P への新規挿入を、`Public` → `Private` の場合は Index-P からの削除を、行の
  値更新と同一 write txn 内で行う。Index-T のエントリは可視性変更の影響を受けない
- 索引対象列の宣言方式は、全 `Text` 列を自動索引化する案と、`CREATE INDEX` 相当の
  宣言的構文を SQL 表層へ追加する案がある。将来構文を追加する場合も**許可リストへの
  追加**として設計し、未検証入力を SQL 文字列へ連結する経路は作らない

### 一貫性（DML 反映・世代整合）

索引エントリの更新は、対応する行の `user_rows_table_def(user_rows_table_name(...))`
への書き込みと**同一の `redb::WriteTransaction` 内**でコミットする。redb の write txn は
単一トランザクション内の複数テーブル書き込みをアトミックにコミットするため、この設計
により「行は書けたが索引は書けていない」という不整合状態を構造的に排除する。

具体的なハザードと対応方針:

1. **同一パス置換の索引残留**: `incremental.rs::index_file` / `index_file_batch`
   はファイル形 `INSERT` を同一パスで置換書き込みする。旧行を置換する際、旧索引
   エントリを**先に削除してから**新索引エントリを挿入しないと、古いキーで索引を
   引いた際に既に置換済みの行を指す stale-positive エントリが残る。これは
   Index-T・Index-P（旧行が `Public` だった場合）の両方に適用される
2. **世代バンプの網羅漏れ**: `catalog.rs::bump_table_generation_in_txn` を呼ぶべき
   書き込み系関数の一覧は `crates/engine/tests/table_generation_bump_coverage.rs`
   がソース走査で網羅性を検証している。索引専用の新しい書き込みパスもこのカバレッジ
   検査対象へ含める
3. **既存データからの索引ビルドと途中失敗**: `crates/engine/tests/index_failure_injection.rs`
   の方針（commit 前失敗・再構築処理そのものの途中失敗）に倣い、途中失敗時にコミット
   済み行が壊れない fail-closed 契約を注入試験で固定する
4. **索引の鮮度判定と既存キャッシュ機構**: `core.rs::PrefilterCache` の fail-closed
   契約（Issue #280）へ相乗りし、索引専用の別系統世代カウンタを新設しない
5. **`operation_id` 台帳との独立性**: 索引更新は `operation_id` 再送契約（TASK-101・
   RECOVER-10 系）と独立であり、索引の存在・不在は重複判定（`23505` / `22023`）に
   影響を与えない
6. **DDL（`DROP TABLE`）時の索引破棄**: 行テーブル・Index-T・Index-P・カーディナリ
   ティ統計を同一 `write_txn`・単一 `commit_boundary::commit` で破棄する。分離される
   と、`drop_table` 後に同名テーブルが再作成された場合、旧 `(tenant_id, id)` を指す
   stale-positive エントリが新テーブルの索引経路へ紛れ込み、誤った検索結果に加え
   旧テナントデータの存在情報漏えい（RLS 境界の実質的な迂回）につながる
7. **DDL（`ALTER TABLE ADD COLUMN`）時の索引同期**: 列追加時点では当該列の索引
   エントリは存在しない（既存行はその列を持たないため false-negative を生まない）。
   列追加後の `INSERT` が新列へ値を書く際は当該列の索引エントリを同一 `write_txn`
   で作成する
8. **列削除・列名変更 DDL**: 現行未実装。導入時は (a) 列削除で当該列の Index-T・
   Index-P 全エントリとカーディナリティ統計を同一 `write_txn` で破棄する、(b) 列名
   変更で索引カタログを新列名へ同一 txn 内で書き換える契約を適用する

### RLS 境界

`policy.rs::PolicyContext::is_visible` は許可可視性集合による絞り込み → 「同一
テナントの行」または「`Public` 行（テナント問わず）」判定の二段である。索引データ
モデルの Index-T／Index-P 二層分離により、索引経路は両層の和集合を候補として返す
ことで全走査経路と同じ範囲を候補として網羅する。`allowed_visibilities` による絞り
込みは索引側では行わないため、和集合は可視集合そのものではなく上位集合（候補）で
あり、候補に対しては**必ず** `is_visible` を再適用する。カーディナリティ統計も
Index-T／Index-P の分離をそのまま踏襲し、`Private` 行の分布を横断集計する統計・
「非権威的ヒント」であっても `Private` 行に由来するテナント横断統計を経路選択へ
用いる設計は採用しない。

## Issue #464 実測の反映（コスト見積り節の書き換え）

`docs/design/scan-stage-profile.md`（25k／100k・共有 QEMU・参考値）が `vector_knn_where`
を W1（`scalar_scan`）⊆ W2（`predicate`）⊆ W3（`arena_copy`）の累積段に分解した:

| 段 | 内容 | 25k（ns/row・累積） | 100k（ns/row・累積） |
| -- | ---- | -------------------- | ---------------------- |
| W1 `scalar_scan` | 可視行の metadata へ `row_codec::scan_scalar_columns` | 13.8 | 13.8 |
| W2 `predicate` | W1 ＋ `lang = 'ja'` 判定（`declarative_filter::matches_all`） | 17.8 | 19.7 |
| W3 `arena_copy` | W2 一致行の embedding を連続 `Vec<f32>` へ複製 | 21.3 | 35.3 |

`agg_count`／`rls_isolation`（`WHERE` を持たない）は `on_visible_row` を通らず、
これらの改善は #477 側の段（A1〜A5。実測では特に A1〜A3）に帰属する——**本 ADR
（#471）が `agg_count` を対象に挙げていた旧記述は誤りであり、対象フェーズを
`vector_knn_where`・`where_compound_count`・`group_by_having` へ訂正する**。

`crossdb-bench.md`（25,000 行・dim 128・wire 経由・p50）の該当フェーズ実測値
（self、参考値）: `vector_knn_where` 2,819µs・`where_compound_count` 3,903µs・
`group_by_having` 3,948µs。

**#471 の効果上限の見積り**: 候補集合を揃えた W2（W1 の `scan_scalar_columns` を
累積で含んだうえで述語判定まで終えた値。dense 探索の候補集合サイズに依存しない）
の累積値——25k: 17.8 ns/row・100k: 19.7 ns/row——を可視行数に乗じた値を、索引化に
よって省略し得るコストの上限とする（`W1` を別途加算すると `W1` 分のコストを二重
計上するため `W2` を単独の累積値として用いる）。`W3`（arena 複製）は候補集合
縮小分のみが索引化の恩恵になり、複製処理自体は残る（「候補 A を採る場合の契約」
節と対比した表の W3 行「一部」を参照）。

**候補 B の損益**: 初回構築 O(N)（W1 相当）を同一世代内のクエリ回数で償却する。
世代進行のたびに再構築されるため「書き込み頻度 ≫ 読み取り頻度」のワークロードでは
無効化（全走査縮退）が妥当——`SqlArenaCache` と同じ性質である。`W0-cold`（`SqlArenaCache`
を毎回空の状態から測る）が `W0-hot` の約 9〜11 倍というスナップショット常駐の効果
（`scan-stage-profile.md`）は、候補 B が `SqlArenaCache` と同居することで索引側にも
及ぶ。

**数値の位置づけ**: 上記はいずれも共有 QEMU 環境での「帰属・上限見積り」であり、
採否根拠ではない（`benchmark-judgement-policy.md` §5「共有計測環境の数値は帰属分析
には使えるが perf 動機の production 変更の採用根拠には使えない」）。before/after の
採否判定・選択度閾値 s\* の確定は #474／#476 で同 policy §7.1／§7.2 テンプレート
（min-of-N・N≥5・参照区間・固定帯 ±5%）に従い専有環境で行う。

## 代替案の比較

| 代替案 | 概要 | 本 ADR の結論 |
| ------ | ---- | -------------- |
| 現状維持 | 索引を導入せず全走査のみ | 行数が増えた場合の性能天井を左右するため、設計整理自体は先行して行う価値がある |
| #363（VectorArena 世代整合キャッシュ）との統合 | `VectorArena` 構築結果自体をキャッシュする方向 | 目的が異なる（索引は走査行数の削減、#363 はアリーナ再構築の削減）。採用案（候補 B）は #363 の `SqlArenaSnapshot` を構築元として直接利用するため、排他ではなく積極的に併用する設計とした |
| zone map / bloom filter 等の軽量代替 | 列値の範囲・存在有無のみを粗く記録し、行単位索引より軽量に走査対象を絞る | 等価条件の完全な絞り込みはできない（偽陽性を許容し全走査は残る）。採用案（候補 B）の等価索引で高選択性条件を完全に絞り込めるため、補助的な最適化候補としての優先度は下げる |

## 実装タスクの対応表

| 旧タスク（Proposed 版） | 対応 Issue | 備考 |
| ---------------------- | ---------- | ---- |
| (1) 物理設計確定・プロトタイプ | #472（本 ADR） | 候補 B 採用により redb 物理設計（候補 A）は不要になった |
| (2) DML 同期・(3) 既存データからの索引ビルド | #473 | 候補 B では「世代整合キャッシュの構築」に集約される。DML 同期・途中失敗注入は構造的に消滅する（構築失敗は全走査縮退で済む） |
| (4) `ExecutionPlan` 統合・選択度切替 | #474（`WHERE` 経路）・#475（集計・`GROUP BY` 経路） | `classify_scalar_plan`・`EXPLAIN` 露出を含む |
| (5) RLS 不変テスト | #476（`tests/scalar_index_rls.rs` 相当。統計縮約オラクル・Issue #393 型） | `DROP TABLE` 再作成・`ADD COLUMN` 後 `INSERT` のケースは候補 B では世代バンプで自動失効するが回帰テストとしては維持する |
| (6) 損益分岐実測 | #476 | `benchmark-judgement-policy.md` §7 テンプレートに従う |

spec 側への申し送り（ポインタのみ）: `IN`／`BETWEEN` 構文の要否・ビヘイビア ID、
`EXPLAIN` の `scalar_plan:` 行の契約化（SQL-6 系）、索引対象列の宣言方式
（`CREATE INDEX` 相当）は本 ADR で決めない。

## 判断記録（オーナー記入欄）

| 項目 | 内容 |
| ---- | ---- |
| 判断 | 候補 B（メモリ常駐・テーブル世代整合キャッシュ）を採用案として確定する |
| 根拠 | 「候補比較」節の採用理由 1〜3（production 契約への非干渉・既存 fail-closed 契約の再利用・Issue #464 実測が示す効果上限との整合） |
| 条件 | 「見送りの再評価条件」（可視行集合がメモリ上限超過、または cold クエリ支配的） |
| 判断日 | Issue #472 の承認コメント日付を転記（本 ADR 単独では確定しない） |
| 記入者 | Issue #472 の承認コメント投稿者を転記（本 ADR 単独では確定しない） |

## スコープ外

- `crates/` 配下のコード変更・索引の実装そのもの（#473〜#476 が担当）
- オーナー承認コメントの取得（オーナー作業。承認前は #473 をブロックする）
- `IN`／`BETWEEN` 構文・索引宣言構文（`CREATE INDEX` 相当）・`EXPLAIN` の
  `scalar_plan:` 契約の spec 側確定
- 選択度閾値（`full_scan_ratio` 型の比）の具体的な数値の確定（#474／#476 での
  専有環境実測が必要）
- `core.rs::PrefilterCache` のテーブル単位世代への統一（#363 由来の申し送りを継承。
  本 Issue は `sql::arena_cache::SqlArenaCache` 経路の追加のみを対象とする）
- #360（パースキャッシュ）・#367（ANN 索引採否検討）・#477（可視ビットマップ
  世代整合キャッシュ）そのものの設計（関係の整理・層分担の言及に留める）

## 参照

- `docs/spec/04-behavior/data-model.md`（TABLE-12）
- `docs/spec/04-behavior/rls.md`
- `docs/spec/05-tasks.md`（TASK-75・TASK-89/133 系・SQL-6・SQL-13・SQL-14・TASK-147・TASK-162）
- PostgreSQL `access/nbtree/`（PostgreSQL License）
- SQLite `where.c` / `wherecode.c`（Public Domain）
- Qdrant `lib/segment/src/index/field_index/`・`query_estimator.rs`（Apache-2.0）
- `docs/design/table-generation-rejection-granularity.md`（Issue #285）
- `docs/design/plan-rls-boost-interaction.md`（TASK-139）
- `docs/design/scan-stage-profile.md`（Issue #464）
- `docs/design/sql-arena-generation-cache.md`（Issue #363）
- `docs/design/sparse-index-cache.md`（Issue #357）
- `docs/design/hnsw-generation-cache.md`（Issue #408）
- `docs/design/hnsw-rls-cardinality-switch.md`（Issue #409）
- `docs/design/explain-search-engine-exposure.md`（Issue #411）
- `docs/design/benchmark-judgement-policy.md`
- `docs/design/hotpath-implementation-survey.md`
