# ADR: ハイブリッド調整パラメータの公開方式

- ステータス: Proposed（ポインタ表記のみの記録。方針・契約は spec 側を正とし、本書では確定しない）
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
どちらからも指定できない（現状は次節）。公開方式の契約は SEARCH-13 を正とし、本書へは
転記しない。本書は公開コードの現状と、SEARCH-13・後続 Issue へのポインタだけを記録する。
spec 由来の具体的な方針・契約の公開は、オーナー承認を得るまで行わない。

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

## 4. 決定事項

本書では設計方針を確定しない。公開方式の方針・契約は SEARCH-13（`docs/spec/04-behavior/search.md`）
を正とし、実装の分担は後続 Issue（#2119 構文・束縛、#2120 実行器、#2121 BM25、#2122 NoSQL）で扱う。

## 5. 後続 Issue

| Issue | 範囲（ポインタのみ） |
| ----- | -------------------- |
| #2119 | 構文・束縛 |
| #2120 | 実行器 |
| #2121 | BM25 |
| #2122 | NoSQL |
| 未割当 | EXPLAIN への調整値表示（要起票） |

## 6. セキュリティ

セキュリティ上の要件（インジェクション・テナント境界・DoS・エラー経由の漏えい）は
SEARCH-13 と [security](../../.claude/rules/security.md) を正とし、後続 Issue の実装・レビューで検査する。
本書では方針を確定しない。

## 7. 対象外・未決

- EXPLAIN への調整値表示を担う Issue が #2117 配下にない。起票が必要
- 片側ごとの窓など spec の解釈確認が要る点は spec 側（SEARCH-13）で扱う
- `$n` 対応は別 Issue
- spec 由来の具体的な方針・契約を本書へ記載するかは、オーナー判断を待つ
