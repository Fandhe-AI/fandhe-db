# SQL `LIKE` のワイルドカード拡張（中間一致・後方一致・`_`）

Issue #914・対象ビヘイビア: SQL-24（TASK-208）。関連ポインタ:
EXT-3・TASK-147（前方一致限定の既存実装）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 背景

EXT-3（TASK-147）時点の SQL 表層 `WHERE <col> LIKE '<pattern>'` は「末尾に `%`
が 1 つだけ付いた非空の前方一致」しか受け付けず、それ以外の形
（`%` 単独・先頭/中間の `%`・`_`・`\`・ワイルドカードなし）はすべて `22000` で
拒否していた（`declarative_filter::parse_prefix_pattern`）。SQL-24 は LIKE の
ワイルドカード意味論を PostgreSQL 互換へ拡張することを求めており、本 Issue で
中間一致・後方一致・`_`（1 文字ワイルドカード）を受理対象へ加える。

## 意味論の契約

1. **`%`**: 空列を含む任意の文字列に一致する。連続する `%%` は 1 つの `%` と
   同じ意味に正規化する。
2. **`_`**: ちょうど 1 **文字**（Unicode scalar。`str::chars()` の 1 要素）に
   一致する。1 バイトではない。
3. **エスケープ**: 既定のエスケープ文字は `\` のみ。
   - `\%`・`\_`・`\\` は、それぞれリテラルの `%`・`_`・`\` として扱う。
   - `\<その他の文字>` はリテラル `<その他の文字>` として扱う（PostgreSQL の
     挙動に倣う）。
   - パターン末尾の単独 `\` は `22000`（PostgreSQL は `22025` を返すが、この
     値は本リポの `wire_code` 契約〔ERR-6・HTTP 射影〕に無く、新設すると
     HTTP 射影の変更も必要になるためスコープ外とした。8 章参照）。
   - 互換性の根拠: EXT-3 時点では `\` を含むパターンは無条件に `22000` で
     拒否していたため、永続化済みの `CHECK` 制約・`VIEW` 本体に `\` を含む
     `LIKE` は存在しえない。意味を再定義しても後方互換は壊れない。
   - lexer は文字列リテラル内の `\` を加工せずに保持する（`''` のみを
     エスケープとして扱う。`standard_conforming_strings=on` の PostgreSQL と
     同じ）。パターン層で `\` を解釈すればよい。
4. **`ESCAPE '<c>'` 句**: 対応しない。構文上、パターン直後の `ESCAPE`
   識別子は述語の区切りにならず文として受け付けられない（`42601`）。
5. **NULL**: 常に不一致（既存の三値論理の契約を踏襲）。
6. **大文字小文字**: 区別する（`ILIKE` は引き続き `42601`）。
7. **`'%'` 単独**: 非 NULL の全行に一致する（PostgreSQL 互換）。EXT-3 時代の
   「空 prefix は無意味なので拒否する」という規約を SQL の `LIKE` に限って
   置き換える。Rust API の `DeclarativeFilter::starts_with("")` は従来どおり
   `22000` で拒否する（Rust API 直接呼び出しの契約は変えない）。

## 振り分け（索引利用を最大化する）

`declarative_filter::parse_like_pattern` がパターンをコンパイルし、次の 3 通り
へ振り分ける（`CompiledLike`）:

- ワイルドカードを含まないリテラル → `FilterOp::Equals`。索引経路
  （`index_equality`）を維持する。
- 末尾がちょうど 1 つの `%` で、それ以外に `%`・`_` を含まない（エスケープ
  解除後）→ `FilterOp::StartsWith`。索引経路（`index_prefix`）を維持する。
- 上記以外（中間一致・後方一致・`_` を含む一般形）→ `FilterOp::Like`。

## 索引縮退（`PlainScan` への統一）

`FilterOp::Like` の一般形は二次索引（`sql::scalar_index::ScalarIndex`）が
対応する照会手段を持たない。索引の有無で結果（可視行のみ・述語一致行のみ）が
変わらないようにするため、次の 2 か所で「索引非対応」として扱う:

- `sql::scalar_plan::classify_scalar_plan`: `FilterOp::Like` を含む場合、単独
  でも複合述語の一部でも常に `ScalarPlan::PlainScan` を返す（`BoolEquals`・
  `TypedCompare(Bytes)` と同じ単一情報源での保証。`mask_trusted_defer`・
  `count_star_only` 等が誤って「索引で完全被覆済み」と信頼しない）。
- `sql::scalar_index::ScalarIndex::candidates_for`: `FilterOp::Like` に対し
  `None` を返す（「一致 0 件」`Some(vec![])` と区別する。呼び出し元は全行
  走査へ縮退する）。

## パターン長の上限

`MAX_LIKE_PATTERN_LEN = 4096` バイト。判定対象は生パターン（エスケープ解除
前）のバイト長で、パース・確保の**前**に判定し、超過は `54000`。前方一致への
振り分け経路も含め、SQL の `LIKE` すべてに一律で適用する。

## 計算量

`LikePattern::matches` は貪欲法による古典的なワイルドカード照合アルゴリズム
（`%` を跨ぐ再走査は直近の `%` 位置へ戻るだけで、再帰・バックトラックの指数
爆発は起きない）。計算量は最悪 O(n·m)（n = 値の文字数、m = パターンの文字数。
m は `MAX_LIKE_PATTERN_LEN` で定数に抑える）。

## PostgreSQL との差分（既知の簡略化）

- パターン末尾の単独エスケープ文字は PostgreSQL の `22025` ではなく `22000`
  で拒否する（3 章参照。`wire_code` 契約の新設が前提になるためスコープ外）。
- `ESCAPE '<c>'` 句・`ILIKE`・`SIMILAR TO` は未対応。

## スコープ外（Issue 起票はしない。実装 PR の note として記録）

- `22025`（invalid escape sequence）の `wire_code` 追加と HTTP 射影
- `LIKE ... ESCAPE '<c>'` 句・`ILIKE`・`SIMILAR TO`
- `NOT LIKE`（別 Issue の `NOT` 対応に委ねる）
- NoSQL `filter` の `like` op（NOSQL-14）
- `LIKE` パターンへの `$n` 束縛
- 後方一致・中間一致を索引で絞る仕組み（逆順索引・n-gram 索引）。今回は
  `PlainScan` への縮退のみ
