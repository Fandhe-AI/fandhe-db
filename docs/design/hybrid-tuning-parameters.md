# ADR: ハイブリッド調整パラメータの公開方式

- ステータス: Proposed（ポインタ表記のみの設計記録。具体的な公開契約は spec 側を正とし、本書では確定しない）
- 対応 Issue: #2118（親 #2117。後続は #2119・#2120・#2121・#2122）
- ビヘイビア ID ポインタ: SEARCH-13（`docs/spec/04-behavior/search.md`）。関連として SQL-4・
  NOSQL-2・SQL-12・SQL-34・SEARCH-17 をポインタで挙げる。契約の本文は spec を正とし、
  本書へは転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード（行番号は書かずシンボル名で示す）:
  - `crates/engine/src/hybrid.rs`（`RrfConfig::new`・`MAX_POOL_DEPTH`）
  - `crates/engine/src/sparse.rs`（`DEFAULT_K1`・`DEFAULT_B`・`with_params`）
  - `crates/engine/src/sql/allowlist.rs`（`parse_order_by_function_args`・`FunctionArg`）
  - `crates/engine/src/sql/parser.rs`（`Ranking::Hybrid`・束縛）
  - `crates/engine/src/sql/exec.rs`（`DEFAULT_HYBRID_POOL_DEPTH`、`RrfConfig::new(60.0…)` の
    箇所、precision 正規化の `RrfConfig::default()` の箇所、`map_hybrid_error`）
  - `crates/engine/src/precision.rs`（`rrf_normalized`）
  - `crates/engine/src/sql/params.rs`（HYBRID の引数位置の `$n` 拒否）
  - `crates/engine/src/sql/explain.rs`（`hnsw_params:` 行）
  - `crates/wire-server/src/http/query/schema.rs`（`HYBRID_SCHEMA`・`SchemaError` → 42601）
- 関連 doc: `hybrid-recall-regression.md`・`rrf-tie-break-determinism.md`・
  `hybrid-rrf-phase6-before-after.md`・`result-limits-cli.md`・
  `explain-search-engine-exposure.md`・`nosql-explain-mapping.md`・`sparse-index-cache.md`
- 検証コード: なし（設計記録のみ。コード変更は後続 Issue）

## 1. 背景・目的

RRF の定数・密と疎の重み・プール深さと、BM25 の `k1`・`b` は固定値で、SQL・NoSQL の
どちらからも指定できない。公開方式の契約（構文・キー・範囲・既定値・拒否条件・エラー契約）は
SEARCH-13 を正とし、本書へは転記しない。本 ADR は公開コードの現状と、本リポ側の実装方針
（検証の層分け・クエリ時適用・文ごとの指定に限る方針）をポインタ表記で記録する。
spec の具体契約の公開は、オーナー承認を得るまで行わない。

作業要件:

1. `USING PLAN`／`USING MODE` の流儀と整合させる。セッション変数・テーブル宣言での指定は設けない
2. プラン文字列や SQL へ未検証の値を連結しない構文であることを確認する
3. 具体的な構文・キー名・範囲・既定値は SEARCH-13 を参照する（本書では確定しない）

## 2. 公開コードの現状

| 項目 | 現状 |
| ---- | ---- |
| RRF 定数・重み | `RrfConfig::new(60.0, 1.0, 1.0, pool_depth)` を実行器が固定で生成 |
| プール深さ | `max(limit + offset, 200)`（`DEFAULT_HYBRID_POOL_DEPTH`） |
| BM25 | `k1 = 1.2`・`b = 0.75`（`DEFAULT_K1`・`DEFAULT_B`） |
| `FunctionArg` | Ident と StringLiteral のみ。数値・名前付き引数を表せない |
| 字句 | `=>` トークンがない |
| `RrfConfig::new` | 重み 0 を拒否する |
| NoSQL スキーマ | `SchemaError` はすべて 42601 に写る |
| precision の正規化 | 既定の `RrfConfig::default()` を固定で使う |

## 3. 他 DB の比較（公開ドキュメントの範囲）

| 製品 | 該当する設定 | 本 ADR との違い |
| ---- | ------------ | ---------------- |
| Elasticsearch | `rank_constant`・`rank_window_size` | 窓が size 未満だとエラー。本リポの扱いは SEARCH-13 を参照 |
| Qdrant | 融合の重み | 重みは検索ごとの指定 |
| SQLite FTS5 | `bm25()` の列重み | 関数引数で指定。文ごとの指定という点で同じ流儀 |
| PostgreSQL | 名前付き引数 `name => value` | 本リポの採否は SEARCH-13 を参照 |

## 4. 決定事項（方針のみ）

具体的な値・構文・エラー契約は SEARCH-13（`docs/spec/04-behavior/search.md`）を参照する。

- 方針 A（検証の層）: SQL の束縛と NoSQL の写像は engine の共通検証を 1 つ通し、同じ意味・
  同じエラーにする。`RrfConfig::new` と `SparseIndex` の不正値は Internal にせず入力不正として写す。
  型名・分担は #2119・#2122 で決める
- 方針 B（クエリ時適用）: BM25 の `k1`・`b` は索引・キャッシュキーに含めず、クエリ時に作用させる
  （#2121）。既定値の経路の計算結果は改訂前と一致させる
- 方針 C（文ごとの指定）: セッション変数・テーブル宣言・起動時 CLI による既定値の上書きは設けない。
  `USING PLAN` の LLM 出力から調整値へ値が流れる経路を作らない
- 方針 D（既定の同一性）: 調整値を省略した結果は改訂前と一致させ、既存の
  `hybrid-rrf-*`・`rrf-tie-break-determinism` の回帰を変えない
- 方針 E（プール深さ）: 実効のプール深さは LIMIT 件数を窓で削らない形にし、上限は
  `--max-result-rows` に結び付けクランプしない（詳細は SEARCH-13）
- 方針 F（融合の契約）: `RrfConfig::new` の検証の緩和が要るかは #2120 で SEARCH-13 に照らして
  決める。融合スコア・precision 正規化の分母が非有限・0 にならないよう束縛時に検査する
- 方針 G（`$n`）: 引数位置の `$n` は `params.rs` の既存拒否を維持する
- 方針 H（互換性）: 既存の 2 引数形・4 引数形の受理・拒否は変えない。公開 enum への
  variant 追加が破壊的変更になる点は #2119 で `!` と `BREAKING CHANGE:` の要否を判断する

## 5. 後続 Issue 対応表

| 方針 | #2119 構文・束縛 | #2120 実行器 | #2121 BM25 | #2122 NoSQL | 未割当 |
| ---- | ---------------- | ------------ | ---------- | ----------- | ------ |
| A 検証の層 | ○ | | | ○ | |
| B クエリ時適用 | | | ○ | | |
| C 文ごとの指定 | ○ | ○ | | ○ | |
| D 既定の同一性 | ○ | ○ | ○ | ○ | |
| E プール深さ | 束縛での検査 | ○ | | ○ | |
| F 融合の契約 | 束縛の検査 | ○ | | | |
| G `$n` | 拒否の維持 | | | | |
| H 互換性 | ○ | | | | |
| EXPLAIN 表示 | | | | | 未割当（要起票） |

## 6. セキュリティ

- インジェクション: 調整値は許可リストの構文段で数値としてだけ受理し、識別子・式は拒否する。
  値は SQL テキストやプラン文字列へ再連結しない。`USING PLAN` の LLM 出力から調整値への経路を作らない
- テナント境界: 調整値は RLS 暗黙適用後の可視集合の中での順位付けにだけ作用する。
  他テナントの行・件数を観測させない。`hybrid.rs` の可視集合外 id の拒否は変えない
- DoS・fail-closed: プール深さの上限を `--max-result-rows` に結び付け、非有限入力・
  オーバーフロー・非有限スコアは入力不正として拒否する。上限は撤廃しない
- エラー経由の漏えい: エラー文は固定の英語文とし、値・表・テナントの情報を含めない
- 設定ミス: セッション変数・テーブル宣言を設けないので、他セッションや表から既定値が汚染される経路はない

## 7. 対象外・未決

- EXPLAIN への調整値表示を担う Issue が #2117 配下にない。起票が必要
- 片側ごとの窓など spec の解釈確認が要る点は spec 側（SEARCH-13）で扱う
- `$n` 対応は別 Issue
- spec 由来の具体契約を本書へ記載するにはオーナー承認が必要（未取得）
