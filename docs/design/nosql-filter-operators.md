# NoSQL `filter` の演算子拡充（Issue #945）

対象ビヘイビア: NOSQL-14（本ドキュメントの束縛規則の SSOT）。関連: NOSQL-7・EXT-3・SQL-2・SQL-24。ポインタ: `docs/spec/05-tasks.md` TASK-147・`docs/spec/04-behavior/nosql-surface.md` NOSQL-14・`docs/spec/04-behavior/sql-surface.md` SQL-24。

## 背景

Issue #761（NOSQL-7）時点の `http::query::filter` は `eq`／`prefix` の 2 語彙・`AND` 結合のみを受理していた。SQL 表層は TASK-208（Issue #912）で `OR`・括弧グルーピング、`IN`・`BETWEEN`・範囲比較（TABLE-13・Issue #891 系列）を既に持っており、NoSQL 表層との語彙差が広がっていた。本 Issue はその差を、SQL 表層と同一の束縛・評価器を共有したまま埋める。

## 設計方針

- **第 2 の実行器を作らない**: 束縛結果（`declarative_filter::MetadataFilter`・`udf_call::BoundExpr`・`sql::where_tree::BoundOrGroup`）は SQL 表層の `WHERE` 述語束縛（`sql::parser::bind_where_predicates`）が使うものと完全に同じ型で、実行経路（`sql::exec`／`sql::scan`／`sql::aggregate`）も共有する。
- **新規公開 API は engine 側に集約する**: `sql::declarative_predicate::{DeclarativePredicate, BoundWhereFilters, bind_declarative_predicates}` を新設した。wire-server は JSON から `DeclarativePredicate`（`Leaf`／`Expr`／`Or` の 3 variant を持つ小さな AST）を組み立てるだけで、`OR` 群の組み立て（`BoundOrGroup::new`・`BoundConjunction::new`。いずれも `sql::where_tree` が `pub(crate) mod` のため wire-server から直接は呼べない）は engine 側に閉じる。
- **既存の直接構築 API は壊さない**: `BoundStatement::new`／`BoundScan::new`／`BoundAggregate::new`／`new_grouped`／`new_grouped_by_columns` のシグネチャは変えず、`with_where_filters`（`BoundStatement`・`core::PlanSearchBinding`）／`with_or_filters`（`BoundScan`・`BoundAggregate`）という builder を追加する形にした。これらは `#[must_use]` の `pub fn` で、`or_filters` を空のまま構築したい既存の呼び出し元（本 Issue のスコープ外である `update`／`delete` の述語形。`BoundPredicateDelete::new` 等）には一切影響しない。

## 語彙表記のゆらぎ（オーナー判断事項）

Issue #945 の受け入れ条件は `lte`／`gte` を明示するが、対象ビヘイビア NOSQL-14 は別表記を使っており両者が食い違っていた。本実装は **`lt`／`gt` に加えて `le`・`lte`（同義語）・`ge`・`gte`（同義語）を完全一致の許可リストへ入れる**ことでどちらの基準でも受理されるようにし、fail-open にならない安全側の判断を採った。表記をどちらかへ一本化するかは spec 側のオーナー判断事項として申し送る。

## `OR` グループの JSON 形

`filter` 配列の要素は「葉」（`{"column","op","value"}`）または「グループ」（`{"or": [<要素>, ...]}`）のいずれかを取る。

- グループのキーは `or` のみ許可する。他のキーが混在する・`or` の値が配列でない・空配列はいずれも `42601`。
- 各分岐は葉または入れ子のグループ 1 つ。分岐が 1 つだけの `or`（`{"or": [X]}`）は親の `AND` 列へ**平坦化**する（`X` をそのまま親へ挿入する）。これは SQL 表層の `sql::allowlist::Parser::parse_where_or` が同じ状況で行う平坦化と揃えた判断で、`BoundOrGroup` の「分岐は 2 個以上」という内部不変条件と整合する。
- ネスト深さの上限は `udf_call::MAX_EXPR_DEPTH`（32）を共有する。HTTP 経由では JSON 自体の深さ上限（NOSQL-8。16）が先に効くため、32 段の `or` へ実際に到達するのは engine の `sql::declarative_predicate` API を直接呼び出す経路（Rust 経由）に限られる。この制約はドキュメント（`nosql-api.md`）に明記した。

## 列型ごとのレーン

範囲比較（`lt`／`le`／`lte`／`gt`／`ge`／`gte`）と `in` は、`declarative_filter::DeclarativeFilter::compare`／`compare_numeric_literal`／`in_list` を通して `DATE`／`TIMESTAMP`／`UUID`／`NUMERIC`／`BYTEA` 列にだけ束縛する。それ以外の列型（`TEXT`／`ENUM`／`BOOLEAN`／`VECTOR`／`ARRAY`／`JSON`／`JSONB`）は、あえて `DeclarativeFilter` を「値が空」の形で構築して `bind()` へ渡し、engine 側の既存の「範囲比較・IN 非対応列」判定（`22000`）へ委譲する。これは SQL の `WHERE lang > 'x'` が `22000` になるのと同じ結果になり、NoSQL 表層独自の分類を新設しない。

## `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への対応（計画からの縮小）

Issue #945 の実装計画（Plan フェーズ）時点では、これらの数値列への `eq`・範囲比較を式レーン（`udf_call::Expr::Binary` を組み立てて `bind_expr` で束縛する）へ渡す設計だった。実装時に `udf_call::bind_expr` の列参照解決（`Ident` 分岐）を確認したところ、`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列はいずれも式内参照そのものが現時点で `InvalidInput`（`22000`。「cannot be used in an expression (yet)」）として拒否されており、別 Issue #891 が担当する未着手のスコープであることが判明した。

このため、本 Issue では **これらの列型への `eq`・範囲比較を対象外のまま**とし（Issue #761 導入時点からの既存の `eq` 拒否〔`0A000`〕と同じ分類を範囲比較にも揃えた）、`in` は他の非対応列型と同じ経路（`in_list` を空値で構築して engine の `22000` へ委譲）へ倒した。式レーンの拡張自体は Issue #891 の担当であり、その完了後に本 Issue の対象列型を再評価する余地を残す。

## 上限（多層防御）

| 上限 | 値 | 検査タイミング | 超過時 |
| --- | --- | --- | --- |
| 葉（`Leaf`／`Expr`）の総数 | `declarative_filter::MAX_METADATA_FILTERS`（256） | wire-server の JSON 走査時（`Vec` 確保前）・engine `bind_declarative_predicates` の両方 | `54000` |
| `or` のネスト深さ | `udf_call::MAX_EXPR_DEPTH`（32） | 同上 | `54000` |
| `in` の要素数 | `declarative_filter::MAX_IN_LIST_ITEMS`（256。新設の公開定数。`sql::allowlist::MAX_IN_LIST_ITEMS` はこの値を再参照する形へ一本化した） | wire-server の JSON 走査時（値配列を `Vec` へ複製する前） | `54000`。空配列は `42601` |

葉の総数・ネスト深さ・`in` の要素数の検査はいずれも、対応する `Vec` の確保・`String` の複製より前に行う（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応）。

## RLS 境界

`column` が RLS 述語名相当（`visible`／`visible()`。大文字小文字非区別）であることの検査は、`or` グループの内側（ネストを含む）でも再帰的に行う。RLS はサーバー側の `PolicyContext` 経由で暗黙適用されるのみであり、`filter` の形（`AND` か `OR` か）に関わらずクライアントが解除できる経路を作らない。`crates/wire-server/tests/nosql14_filter_operators.rs::or_filter_still_enforces_tenant_boundary_on_scan_execution` が、全テナントの行に一致する `OR` 条件を渡しても自テナントの可視行しか返らないことを実行レベルで固定する。

## 対象外・申し送り

- NOSQL-14 に含まれる、本 Issue の受け入れ条件に無い語彙・グループ形（否定系等）: 未知語彙として `42601` のまま。後続 Issue の候補。
- `update`／`delete` op の `filter`（述語形）への同拡張: #1062 の範囲（本 Issue は `search`／`scan`／`aggregate` の 3 op のみ）。
- `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`・範囲比較: 上記のとおり Issue #891（式レーンの列型拡張）待ち。
- SQL-24 の性能受け入れ基準（`OR` 2 項・`IN` 8 要素での p95）: 本 Issue では測定しない（ベンチ系 Issue の範囲）。
