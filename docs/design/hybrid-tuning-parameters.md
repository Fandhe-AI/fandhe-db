# ADR: ハイブリッド調整パラメータの公開方式

- ステータス: Proposed（自動運転で作成。オーナーの承認は未取得で、承認済みとは扱わない）
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
どちらからも指定できない。公開方式の契約は SEARCH-13 をポインタで参照し、本書へ転記しない。
本 ADR は本リポ側の実装詳細（字句・束縛・エラーの写像・既定値との同一性・EXPLAIN）の
設計案を記録する。公開契約を超える開示にあたる部分は、オーナー承認を得るまで案として扱う。

作業要件:

1. `USING PLAN`／`USING MODE` の流儀と整合させる。セッション変数・テーブル宣言での指定は設けない
2. プラン文字列や SQL へ未検証の値を連結しない構文であることを確認する
3. 構文・キー名・範囲・既定値（＝現行値）を確定事項として記録する

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
| Elasticsearch | `rank_constant`・`rank_window_size` | 窓が size 未満だとエラー。本書は窓を `limit + offset` で下支えしエラーにしない |
| Qdrant | 融合の重み | 重みは検索ごとの指定 |
| SQLite FTS5 | `bm25()` の列重み | 関数引数で指定。文ごとの指定という点で同じ流儀 |
| PostgreSQL | 名前付き引数 `name => value` | 本書はこの記法を採る（`:=` は受理しない） |

## 4. 決定事項

### D1 パラメータ名・型・範囲・既定値（SQL と NoSQL 共通）

| 名前 | 意味 | 型 | 範囲（違反は 22000） | 既定 |
| ---- | ---- | -- | -------------------- | ---- |
| `k1` | BM25 の項の飽和度 | 有限の数値 | 0 以上 | 1.2 |
| `b` | BM25 の文書長の正規化 | 有限の数値 | 0〜1（両端を含む） | 0.75 |
| `rrf_k` | RRF の順位減衰の定数 | 有限の数値 | 0 より大きい | 60 |
| `window` | 密・疎それぞれから融合に取り込む候補数（1 つの値を両側に適用） | 整数 | 1〜`--max-result-rows`（実行時の `ResultLimits`） | 200（実効値は D4） |
| `weights` | 密・疎の重み | 2 要素（dense, sparse） | 各 0 以上の有限値。両方 0 は不可。和が有限、かつ `rrf_k` との組で分母が正（D5） | (1.0, 1.0) |

- 指定しない項目は現行値のまま。全項目を省略した結果は改訂前とバイト単位で一致する
  （`hybrid-rrf-*`・`rrf-tie-break-determinism` の回帰が変わらない）
- `window` は 1 つの値を密・疎の両方に適用する（`RrfConfig` の `pool_depth` が 1 つである
  現構造に合わせる）。片側ごとの窓は設けない（7 節）

### D2 SQL 構文（名前付き引数）

- 形: `HYBRID(<vec列>, '<vec>', <text列>, '<query>' [, <名前> => <値> ...])`。
  `hybrid_rrf(...)` も同じ形で同じ結果を返す（SQL-4 の両構文一致を維持）
- 名前付き引数は位置引数の後ろにだけ置ける。次はすべて構文段で 42601:
  - 位置引数が名前付き引数より後ろにある
  - 同じ名前が 2 回出る
  - 名前が未知
  - `:=` 記法
- 名前は大文字小文字を区別しない非引用の識別子。引用識別子は 42601
- 字句: `=>` を新しい多文字トークンとして字句化する（`->` と同じ扱い。この位置以外では
  構文段が 42601 で拒否する＝fail-closed）。`FunctionArg` に数値と名前付き引数を表す
  variant を追加する。型の具体設計は #2119 に任せる
- 2 引数形に名前付き引数を付けた場合: 構文は受理し、束縛で従来どおり「2 引数形は実行
  不能」の 22000 を返す。名前の検査（42601）は構文段なので先に決まる
- 値の位置で受理する形:
  - 数値リテラル（整数・小数・`.5`・指数表記）に省略可能な単項 `+`／`-` を付けた形
  - `weights` のみ `ARRAY[<数値>, <数値>]`（順序は dense, sparse。位置引数の
    「ベクトル列 → テキスト列」と同じ順）
  - 文字列リテラル・`NULL`・要素数が 2 でない `ARRAY` は束縛で 22000（型不一致。暗黙変換なし）
  - 識別子・式・副問い合わせ・`$n` は構文段で 42601
- 負の数は構文として受理し、範囲検査で 22000 にする
- `weights` の綴りの代替案（不採用）:
  - `dense_weight =>`／`sparse_weight =>` に分ける案は、NoSQL と名前の集合が変わり未知名の
    判定がずれるため採らない
  - JSON 文字列リテラル案は、値を文字列から再解析する経路を SQL に作るため採らない

### D3 NoSQL（`search` op の `hybrid` オブジェクト）

- キー: `text`（既存・必須）に加え、任意キー `k1`・`b`・`rrf_k`・`window`・
  `weights`（`{"dense": <数>, "sparse": <数>}`）
- `hybrid` 内の未知キーは既存スキーマの UnknownKey で 42601。`weights` 内の未知キーも 42601
- 型の不一致: 調整キーの型不一致は 22000 とするが（契約は SEARCH-13 を参照）、既存の
  `SchemaError::TypeMismatch` は 42601 になる。次のように解消する:
  - 調整キーは `HYBRID_SCHEMA` 上で緩い型（Any・nullable）として宣言し、未知キーの判定だけを
    スキーマ層に残す
  - 型・範囲は engine 側の共通検証（D6）で 22000 にする
  - `null`・文字列・真偽値・配列は 22000。`window` の非整数（`200.5`・`2e2` など整数表記でない数）も 22000
  - `weights` の `dense`／`sparse` の欠落も 22000（部分指定は受理しない。SQL の ARRAY が
    2 要素必須であることと揃える）
- 既存の `limit`（非整数は 42601）とは扱いが異なる。調整キーに限った例外である
- `plan` と `hybrid` の併用はこれまでどおり 42601。`metric` の併用規則も変えない

### D4 `window` の実効値

- 実効のプール深さは `max(window, limit + offset)`。指定なしは `window = 200` と同じ式になり、
  現行の `max(fetch_count, 200)` とバイト単位で一致する
- `window < limit + offset` はエラーにしない。LIMIT 件数を窓で削らない（「正解を含む
  データ群を広く返す」方針）
- 上限: 明示値は `1..=--max-result-rows`。超過はクランプせず 22000（#2120）。
  `max-result-rows` ≤ `MAX_RESULT_ROWS_HARD_CAP` ≤ `hybrid::MAX_POOL_DEPTH`（既存のコンパイル時
  assert）なので `RrfConfig::new` の上限を必ず通る
- `--max-result-rows` が 200 未満の構成: 省略時の既定 200 は明示値の範囲検査の対象外とし、
  現行の挙動を保つ。明示の `window => 200` は 22000 になり得る。この非対称は意図したものである

### D5 重み 0 と融合の契約変更

- 片側の重みが 0 であることを認める（両方 0 は 22000）。`RrfConfig::new` の検証を「各重みは
  有限かつ 0 以上、両方 0 は拒否」に改める（#2120）。負値・非有限の拒否は維持する
- 重み 0 の側の候補もプールには入るが寄与は 0。同点は既存の規則（id 昇順・
  `TieRank::GroupEnd`）で決まり決定性を保つ。結果が同一なら計算の省略は実装の任意
- オーバーフロー防止: 寄与は `重み / (rrf_k + 順位)`。順位は 1 以上、`rrf_k` は正なので寄与は
  重み以下。束縛時に `dense + sparse` の有限性を検査すれば、融合スコアの非有限
  （現状は `NonFiniteScore` → Internal）は利用者入力から起こらない。和が非有限なら束縛で 22000
- `precision::rrf_normalized` の分母 `(dense + sparse) / (rrf_k + 1)` は、両方 0 の拒否だけでは
  f64 の除算でアンダーフローして 0 になり得る（例: dense=1e-300, sparse=0, rrf_k=1e300）。
  このため束縛時に「`(dense + sparse) / (rrf_k + 1)` が正の有限値」であることも検査し、
  満たさない組み合わせは 22000 で拒否する（D1・D5 の個別検査を通っても拒否される）。
  これにより D8 結線後の precision 正規化の分母は常に正で、検索が空集合になる経路を作らない

### D6 検証の層とエラーの優先順位

- engine 側に共通の検証を 1 つ置く（調整値の束を表す型と、`Option` 入力から検証して構築する
  関数。型名は #2119 で決める）。SQL の束縛と NoSQL の写像は必ずこれを通し、同じ意味・同じエラーにする
- `RrfConfig::new` の `InvalidConfig` と `SparseIndex` の `InvalidParams` は 22000
  （`invalid_input`）に写す。Internal（XX000）にはしない
- 優先順位:
  1. 構文段の 42601（未知の名前・重複・順序・値の形）
  2. 束縛段の既存判定（表 42P01、列、ベクトルリテラル、2 引数形の 22000）
  3. 調整値の型・範囲の 22000
- 3 を最後に置くのは、`USING MODE` リテラルを表の解決より後で解析する既存の先例
  （`nosql-explain-mapping.md`）に揃えるため
- エラー文は固定の英語文でパラメータ名だけを含める。値・表・テナントの情報は含めない

### D7 BM25 の k1・b はクエリ時に作用する

- 文書統計（平均文書長・df）と疎索引・`sql::sparse_cache` のキャッシュキーに k1・b を含めない。
  索引を再構築しない（#2121）
- 計算結果が非有限になる場合（極端な `k1` と tf の組み合わせ）は、順位付けへ NaN／Inf を
  流さず 22000（fail-closed）。既定値の経路の浮動小数の計算式は変えず（既定結果のバイト単位
  一致のため）、計算後の有限性検査で対処する
- SEARCH-12（全文検索の構文）の語・フレーズ・前方一致の BM25 にも同じ k1・b を適用する

### D8 文ごとの指定に限る（`USING MODE`／`USING PLAN` との整合）

- セッション変数（`SET`）・テーブル宣言・起動時 CLI による既定値の上書きは設けない。結果は
  文のテキスト（NoSQL では要求本文）だけで決まり再現できる。NoSQL には `SET` の手段がなく、
  表層の非対称も生まない
- `USING MODE` や `hnsw.*` と流儀が違う理由: これらはモードや探索幅の運用上の既定を切り替える
  のに対し、調整値は融合の意味そのものを変えるので文に閉じる
- `USING PLAN(...)`: `ORDER BY` がなく、LLM 出力（`search_terms`・`expanded_query_text`）から
  調整値へ値が流れる経路は作らない。plan の経路は常に既定値。NoSQL の `plan`＋`hybrid` は既存どおり 42601
- `USING MODE 'precision'` とは独立。ただし precision ゲートの正規化（exec.rs の
  `RrfConfig::default()`）は、DISTANCE 段と同じ実効 cfg（`rrf_k`・重み）を使うよう #2120 で改める。
  既定 cfg のままだと正規化スコアの尺度が実際の融合とずれる
- SQL-73 の `hnsw.*` は hybrid の結果に作用しない（既存契約）。`window` は HNSW の密側にも同じ値で効く

### D9 プレースホルダ `$n`

名前付き引数の値位置での `$n` は今回受理しない（`params.rs` の既存拒否どおり 42601）。
必要になれば別 Issue で扱う。

### D10 EXPLAIN の表示

- 行の形: `hybrid_params: rrf_k=<v>,window=<実効値>,dense_weight=<v>,sparse_weight=<v>,k1=<v>,b=<v>`
  （`hnsw_params:` の k=v 形式。数値は Rust の f64 の `Display`、window は整数）
- 置き場所は `hnsw_params` の直後、`ann_plan` の前。ハイブリッドの文に限り、既定値でも常に出す
- 静的な行で、検索を実行せず表の内容に依存しない。SQL の `EXPLAIN` と NoSQL の `explain` は同じ行を返す
- #2119〜#2122 のいずれにも EXPLAIN 対応が含まれていない。後続 Issue が必要（7 節）

### D11 SEARCH-17（将来）との関係

`hybrid_score()` 等は実効の `rrf_k`・重みで計算する（実装は TASK-327 側）。

### D12 互換性

- 既存の 2 引数形と 4 引数形の受理・拒否は変えない
- `FunctionArg` は `#[non_exhaustive]` のない公開 enum で、variant 追加は網羅的 `match` を書く
  下流にとって破壊的変更になる。#2119 で `feat(engine)!` と `BREAKING CHANGE:` の要否を判断する
  （既存の `OrderByForm::UsingPlan` 追加時の運用に倣う）

## 5. 後続 Issue 対応表

| 決定 | #2119 構文・束縛 | #2120 実行器 | #2121 BM25 | #2122 NoSQL | 未割当 |
| ---- | ---------------- | ------------ | ---------- | ----------- | ------ |
| D1 名前・範囲・既定 | ○ | ○ | ○ | ○ | |
| D2 `=>`・FunctionArg・ARRAY | ○ | | | | |
| D3 NoSQL キーと 22000 | | | | ○ | |
| D4 window の実効値・上限 | 束縛での範囲検査 | `max(window, fetch)` | | ○ | |
| D5 重み 0・和の有限性 | 束縛の検査 | `RrfConfig::new` の緩和 | | | |
| D6 共通検証・優先順位 | ○ | | | ○ | |
| D7 k1・b をクエリ時に適用 | | | ○ | | |
| D8 precision の正規化 cfg | | ○ | | | |
| D9 `$n` | 拒否の維持 | | | | |
| D10 EXPLAIN | | | | | 未割当（要起票） |

## 6. セキュリティ

- インジェクション: 調整値は許可リストの構文段で数値リテラル（と `ARRAY[数値, 数値]`）としてだけ受理する。
  識別子・式・`$n` は 42601。値は SQL テキストやプラン文字列へ再連結しない。`USING PLAN` の
  LLM 出力から調整値への経路を作らない（D8）
- テナント境界: 調整値は RLS 暗黙適用後の可視集合の中での順位付けにだけ作用する。window は
  可視候補から取る数で、他テナントの行・件数を観測させない。`hybrid.rs` の可視集合外 id の
  拒否（`ProviderResultRejected`）は変えない
- DoS・fail-closed: window の上限を `--max-result-rows` に結び付け、クランプしない。非有限入力・
  和のオーバーフロー・BM25 の非有限スコアは 22000 で拒否する。上限は撤廃しない
- エラー経由の漏えい: エラー文は固定の英語文でパラメータ名だけを含める
- 設定ミス: セッション変数・テーブル宣言を設けないので、他セッションや表から既定値が汚染される経路はない

## 7. 対象外・未決

- EXPLAIN の `hybrid_params:` 行（D10）を担う Issue が #2117 配下にない。起票が必要
- 片側ごとの window: 1 つの `window` を両側に適用すると解釈した。片側ごとの値が意図なら
  spec 側（SEARCH-13）での確認が要る
- `$n` 対応（D9）は別 Issue
- `--max-result-rows` < 200 での既定値と明示値の非対称（D4）
- NoSQL の型不一致を 22000 とする扱い（D3）は調整キーに限った例外であり、他キーの 42601 は変えない
