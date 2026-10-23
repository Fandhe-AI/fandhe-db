# SQL `LIKE` のワイルドカード拡張（中間一致・後方一致・`_`）

Issue #914・対象ビヘイビア: SQL-24（TASK-208）。関連ポインタ:
EXT-3・TASK-147（前方一致限定の既存実装）。`ILIKE` の畳み込み範囲は Issue #1777
（親 #1776・SQL-54・TABLE-28 ポインタ）で決定し、Issue #1778 で実装した。

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
   - パターン末尾の単独 `\` は `22000`（PostgreSQL は `22025` を返すが、`ErrorClass` に
     相当する分類が無く、追加すると HTTP 射影の変更も必要になるためスコープ外
     とした。ERR-6 ポインタ。8 章参照）。
   - 互換性の根拠: EXT-3 時点では `\` を含むパターンは無条件に `22000` で
     拒否していたため、永続化済みの `CHECK` 制約・`VIEW` 本体に `\` を含む
     `LIKE` は存在しえない。意味を再定義しても後方互換は壊れない。
   - lexer は文字列リテラル内の `\` を加工せずに保持する（`''` のみを
     エスケープとして扱う。`standard_conforming_strings=on` の PostgreSQL と
     同じ）。パターン層で `\` を解釈すればよい。
4. **`ESCAPE '<c>'` 句**: 対応しない。構文上、パターン直後の `ESCAPE`
   識別子は述語の区切りにならず文として受け付けられない（`42601`）。
5. **NULL**: 常に不一致（既存の三値論理の契約を踏襲）。
6. **大文字小文字**: `LIKE` は区別する。`ILIKE` の畳み込み範囲は後述の
   「`ILIKE` の大文字小文字の畳み込み範囲」節で決定済みで、`ILIKE`／`NOT ILIKE` は
   #1778 で受理する（実装は同節末尾の「実装メモ」参照）。
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

## `ILIKE` の大文字小文字の畳み込み範囲（Issue #1777）

ドキュメントのみの決定で、コードは変えない。実装は #1778。根拠は PostgreSQL 17
の公開挙動（`like.c`・`formatting.c` の読解。実測は未実施）と本実装の性質だけ
で説明し、spec 本文は転記しない。

### 決定

- 実効照合（`Collation::effective` の導出結果）が `C`／`POSIX`／照合指定なしの
  場合、**ASCII の英字（`A-Z`↔`a-z`）だけ**を畳み込む。現状の TEXT 列は
  すべてこれに当たる。
- 非 ASCII の文字（`É`・`Σ`・全角 `Ａ`・ケルビン記号 U+212A など）は**拒否せず
  そのまま比べる**。バイトが完全一致するときだけ一致とし、大文字小文字は
  区別する。
- 実効照合が ICU（`und-x-icu`・`ja-x-icu`）の場合は `0A000` で拒否する
  （fail-closed）。黙って ASCII で畳み込まない。照合依存の畳み込みを結線
  するまでの措置で、追跡は既存の対象外項目「照合依存の `LOWER`／`UPPER`／
  `ILIKE`」（implementation-status の #2190 項）に寄せる。
- 照合の衝突は既存の導出規則に従う（明示同士は `42P21`、暗黙同士は `42P22`）。
  新しい `wire_code` は追加しない。

### PostgreSQL 17 の挙動と本実装の採否

| 照合 | PostgreSQL の畳み込み | 本実装 |
| ---- | -------------------- | ------ |
| `C`／`POSIX` | `lower()` が ASCII の `A-Z` だけ変換 | ASCII だけ畳み込む |
| libc ロケール | 1 文字ごとの `towlower`（単純変換） | 対象外（本実装に無い） |
| ICU | `u_strToLower`（ロケール依存のフル変換） | `0A000` |
| 非決定的照合 | `ILIKE` 非対応のエラー（`0A000`） | 同左（導入時） |

### 理由

1. PostgreSQL の `C` 照合下の `ILIKE` と結果が一致する
   （例: `'É' ILIKE 'é'` は偽）。非 ASCII を拒否するとデータの中身で
   クエリがエラーになり互換も壊れる。Unicode で畳み込むと PostgreSQL と
   結果が食い違う。
2. ASCII の畳み込みは 1 文字が 1 文字・1 バイトが 1 バイトに対応する。`_`
   の意味、`MAX_LIKE_PATTERN_LEN`、値の長さが変わらない。Unicode のフル
   変換は文字数・バイト長を変える（`İ`、`ß`→`SS`）。
3. 決定的で、ロケールにも OS にも依存しない。DoS 上限（O(n·m)）も保たれる。
4. 新しい依存が要らない（ロケール依存変換には ICU4X の casemap 系クレートが
   必要で、dependency-policy の承認を要する）。

### #1778 への実装制約

- パターンのリテラル文字はコンパイル時に、値は照合時に ASCII で小文字化し、
  既存の `LikePattern::matches` を再利用する。畳み込みは `%`・`_`・`\` を
  生まず消さないので、エスケープの意味論は変わらない。
- `string_fn::lower` を `ILIKE` の評価に再利用しない（文字数が変わり `_` の
  意味が壊れ、PostgreSQL の `C` とも食い違う）。
- `ILIKE` はワイルドカードなし・純粋な前方一致でも `Equals`／`StartsWith`
  へ振り分けない（スカラー二次索引は大文字小文字を区別する）。
  `classify_scalar_plan` と `ScalarIndex::candidates_for` の両方で
  `PlainScan` へ縮退させ、索引の有無で結果を変えない。
- NULL は不一致、`NOT ILIKE` は否定、パターン長の上限（生パターンのバイト長
  を確保前に検査し超過は `54000`）は `LIKE` と同じ。RLS の適用は変えない。
- NoSQL `filter` には `ilike` を加えない（`42601` のまま）。`ESCAPE` 句
  （#1779）と左辺の式（#1780）は別 Issue。

## パターン長の上限

`MAX_LIKE_PATTERN_LEN = 4096` バイト。判定対象は生パターン（エスケープ解除
前）のバイト長で、パース・確保の**前**に判定し、超過は `54000`。前方一致への
振り分け経路も含め、SQL の `LIKE` すべてに一律で適用する。

## 計算量

`LikePattern::matches` は貪欲法による古典的なワイルドカード照合アルゴリズム
（`%` を跨ぐ再走査は直近の `%` 位置へ戻るだけで、再帰・バックトラックの指数
爆発は起きない）。計算量は最悪 O(n·m)（n = 値の文字数、m = パターンの文字数。
m は `MAX_LIKE_PATTERN_LEN` で定数に抑える）。

### 実装メモ（Issue #1778）

- 構文: `WherePredicate::Prefix` に `case_insensitive` を追加し、`LIKE` と同じ構造葉の
  経路で `ILIKE`／`NOT ILIKE` を受理する。`ILIKE` は文脈的キーワードで、`ilike` という
  名前の列は壊さない。
- 評価: 束縛時にパターンの文字を ASCII 小文字へ畳み、`LikePattern` の `fold_ascii` が
  真のとき値側の 1 文字も畳んで比べる（行ごとの確保なし）。`fold_ascii` は `PartialEq` に
  含まれるため、部分索引の含意判定は `LIKE` と `ILIKE` を同一視しない。常に一般形の
  `FilterOp::Like` に束縛し、`Equals`／`StartsWith` へ振り分けないので、二次索引は使わない。
- 照合: 束縛時に ICU 照合の列を `0A000` で拒否する（新しい `wire_code` は無い）。
- 台帳: 述語つき DML の content_hash は `ILIKE` にタグ 16 を割り当て、`LIKE`（タグ 2）の
  バイト列は変えない。
- 対象外（`42601` のまま）: JOIN の WHERE と `UPDATE … FROM`／`DELETE … USING`（独自文法で
  `NOT LIKE` も受理しない）、NoSQL `filter` の `ilike`。
- 制約（追跡）: 評価後射影形ビュー（本文に集計・`LIMIT` を持つ形）の外側 `WHERE` の `ILIKE` は、
  合成スキーマが元の列の照合を持たないため `0A000` で拒否する。照合の伝搬は後続。
- `CHAR(n)` 列は `LIKE` と同じく `ILIKE` も `22000` で拒否される（既存の挙動との対称）。

## PostgreSQL との差分（既知の簡略化）

- パターン末尾の単独エスケープ文字は PostgreSQL の `22025` ではなく `22000`
  で拒否する（3 章参照。`wire_code` 契約の新設が前提になるためスコープ外）。
- `ESCAPE '<c>'` 句・`SIMILAR TO` は未対応。
- `ILIKE`／`NOT ILIKE` は #1778 で実装済み。方針は次節。
  `C` 系の照合では PostgreSQL と同じ結果になる。ICU 照合の列に対する
  `ILIKE` は `0A000`（PostgreSQL は受理する）。
- `lower(col) LIKE lower('p')` と `col ILIKE 'p'` は、`C` 系の照合でも非 ASCII
  を含む値で結果が異なりうる。`string_fn::lower` は Unicode のフル変換で、
  `ILIKE` は ASCII だけを畳み込むため（PostgreSQL の `C` 照合では `lower()`
  も ASCII だけなので両者は一致する。挙動変更は対象外）。

## スコープ外（Issue 起票はしない。実装 PR の note として記録）

- `22025`（invalid escape sequence）の `wire_code` 追加と HTTP 射影
- `LIKE ... ESCAPE '<c>'` 句・`SIMILAR TO`（`ILIKE` は #1778 で実装済み）
- `NOT LIKE`（別 Issue の `NOT` 対応に委ねる）
- NoSQL `filter` の `like` op（NOSQL-14）
- `LIKE` パターンへの `$n` 束縛
- 後方一致・中間一致を索引で絞る仕組み（逆順索引・n-gram 索引）。今回は
  `PlainScan` への縮退のみ
