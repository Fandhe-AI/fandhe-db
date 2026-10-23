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
   - パターン末尾の単独 `\` は `22025`（PostgreSQL と同じ。Issue #1779 で `ErrorClass` に
     `InvalidEscapeSequence` を追加した。従来は `22000`。ERR-6 ポインタ）。
   - 互換性の根拠: EXT-3 時点では `\` を含むパターンは無条件に `22000` で
     拒否していたため、永続化済みの `CHECK` 制約・`VIEW` 本体に `\` を含む
     `LIKE` は存在しえない。意味を再定義しても後方互換は壊れない。
   - lexer は文字列リテラル内の `\` を加工せずに保持する（`''` のみを
     エスケープとして扱う。`standard_conforming_strings=on` の PostgreSQL と
     同じ）。パターン層で `\` を解釈すればよい。
4. **`ESCAPE '<c>'` 句**（Issue #1779）: `LIKE`／`NOT LIKE`／`ILIKE`／`NOT ILIKE` の
   パターンリテラル直後に書ける。エスケープ文字の直後の文字（`%`・`_`・エスケープ文字自身・
   その他）はリテラルになる。`ESCAPE ''` はエスケープの無効化で、`\` もただの文字になる。
   `ESCAPE '\'` は ESCAPE を書かない場合と同じ。値が 2 文字以上は `22025`。値が単純な
   文字列リテラルでない形（識別子・`NULL`・`$n`・数値・`::` キャスト付き）は `42601` のまま。
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
  は #1779 で実装済み（次々節）。左辺の式（#1780）は別 Issue。

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

### 実装メモ（Issue #1779）

- 方針: PostgreSQL の `like_escape` と同じく、構文層で `ESCAPE` を既定の `\` エスケープ形へ
  正規化する（`declarative_filter::apply_like_escape`）。`WherePredicate::Prefix` にフィールドは
  足さず、正規化後のパターンを `pattern` に格納する。以降は ESCAPE 無しの経路と同一で、
  `'a!%' ESCAPE '!'`（正規化後 `a\%`）は等価の索引経路、`'ab!%c%' ESCAPE '!'` は前方一致に
  振り分けられる。`ILIKE` は従来どおり索引を使わない。
- 正規化の順序: ESCAPE の正規化は `ILIKE` の畳み込みより前。エスケープ文字の比較は畳み込まない。
- 変換規則: ESCAPE が空なら `\` を `\\` に、独自の 1 文字 `e` なら `e` → `\`（直後の文字は
  リテラル）、素の `\` → `\\`。末尾に `e` が残ると変換後が単独 `\` で終わり、トークン化段が
  `22025` で拒否する。
- 台帳（RECOVER-10）: content_hash のバイト列は変えない（タグの追加なし）。`LIKE 'a!%' ESCAPE '!'` と
  `LIKE 'a\%'` は同じ述語として同じハッシュ、ESCAPE を落とした `LIKE 'a!%'` は別ハッシュ（再送は
  `22023`）。ESCAPE は従来 `42601` で台帳に記録済みの例が無く、再送互換は壊れない。
- CHECK 制約・ビュー: 格納形が `\` エスケープ形のため往復が成立する。再表示は ESCAPE を含まない
  正規化形になる。
- `22025`: 末尾の孤立エスケープは従来の `22000` から `22025` へ変えた（ESCAPE 無しの `LIKE 'src\'`・
  `ILIKE`・NoSQL `like` も同じ。HTTP は 400 のまま）。`ESCAPE` の値が 2 文字以上でも `22025`。
- `ESCAPE` は文脈的キーワード（`Keyword` に追加しない）。`escape` という名前の列・表は壊れない。
- 対象外（`42601` のまま）: JOIN の WHERE と `UPDATE … FROM`／`DELETE … USING`、NoSQL `filter` の
  `like` への escape 指定、`ESCAPE` の値が式・`$n`・`NULL` の形。

### 実装メモ（Issue #1781）

- `WherePredicate::Prefix` の左辺を `column: String` から `lhs: Expr` に変えた（挙動不変の refactor。SQL-54 ポインタ）。
  構文層が作る左辺は引き続き裸の列（`Expr::Ident`）だけ。
- 裸の列かどうかは `Expr::as_bare_column()` に集約する。宣言的フィルタ（`MetadataFilter`）への束縛と
  スカラー索引の候補抽出は、左辺が裸の列のときだけ従来経路に乗る。
- 台帳の content_hash は裸の列なら従来のバイト列（タグ 2／16）のまま。式左辺には新タグを割り当てない。
- 式左辺は束縛段・content_hash・NoSQL の update/delete ゲート・ビュー本体・CHECK で `42601`（fail-closed）。
  列スコープ検査・決定性検査・ON CONFLICT の式走査は式左辺を式として辿る。
- 式左辺の受理・評価・タグ割り当ては #1782 で実装した（次節）。

### 実装メモ（Issue #1782）

- 構文層は `<式> [NOT] LIKE|ILIKE '<lit>' [ESCAPE '<c>']` を受理する（SQL-54 (3)）。左辺は式パーサで読み、
  関数呼び出し・`concat`・算術・`CASE`・括弧付き左辺を許す。後置・前置 `NOT` は `Not(Prefix)` で表し、
  二重否定は畳む。パターンは文字列リテラルのみで、列・式・`$n` は `42601` のまま。
- 束縛段は `BoundExpr::Like { lhs, pattern, negated }`（BREAKING: `BoundExpr` の variant 追加）にして
  式レーンの残余述語にする。宣言的フィルタ・スカラー索引の候補にしないため、前方一致形でも常に全行評価
  （EXPLAIN は `plain_scan`。SQL-54 (5)）。左辺が裸の列の既存経路（`index_prefix`／`index_equality`）は不変。
- 左辺が NULL の行は否定の有無に関わらず `NULL`（三値論理）。WHERE は真の行だけ通すので
  `NOT LIKE`・`NOT (... LIKE ...)` でもその行は除外される。
- 左辺が文字列型でなければ `42883`（`42804` ではない）。文言は固定の英語で値・列名を含まない。
- 照合: 式の内側の ICU 列・非バイト列順の `COLLATE` は `0A000`（`C`／`POSIX` は受理）。
- 台帳の content_hash は述語タグ 17（ILIKE 有無 1 バイト・左辺式・長さ前置のパターン）。
  列左辺のタグ 2／16 は不変。
- fail-closed のまま: CHECK 本体・部分索引の述語（`0A000`）、ビュー本体の WHERE・集計／LIMIT 本体ビュー
  の外側 WHERE・NoSQL update/delete ゲート・JOIN の WHERE と `UPDATE … FROM`／`DELETE … USING`（`42601` 等）。
- 対象外: パターン右辺の式・`$n`、式索引による候補絞り込み、SELECT リスト・HAVING・JOIN ON での式左辺 LIKE、
  左辺が列の非テキスト型 LIKE の `22000`（SQL-54 (3) の `42883` との差。別 Issue の候補）。

## PostgreSQL との差分（既知の簡略化）

- `ESCAPE` 付きのパターンは、正規化後（`\` を多く含むと最大 2 倍）の長さが 4096 バイトを超えると
  `54000` になる。PostgreSQL は受理する（fail-closed の簡略化）。
- `SIMILAR TO` は未対応。
- `ILIKE`／`NOT ILIKE` は #1778 で実装済み。方針は次節。
  `C` 系の照合では PostgreSQL と同じ結果になる。ICU 照合の列に対する
  `ILIKE` は `0A000`（PostgreSQL は受理する）。
- `lower(col) LIKE lower('p')` と `col ILIKE 'p'` は、`C` 系の照合でも非 ASCII
  を含む値で結果が異なりうる。`string_fn::lower` は Unicode のフル変換で、
  `ILIKE` は ASCII だけを畳み込むため（PostgreSQL の `C` 照合では `lower()`
  も ASCII だけなので両者は一致する。挙動変更は対象外）。

## スコープ外（Issue 起票はしない。実装 PR の note として記録）

- `SIMILAR TO`（`ILIKE` は #1778、`LIKE ... ESCAPE '<c>'` と `22025` は #1779 で実装済み）
- `NOT LIKE`（別 Issue の `NOT` 対応に委ねる）
- NoSQL `filter` の `like` op（NOSQL-14）
- `LIKE` パターンへの `$n` 束縛
- 後方一致・中間一致を索引で絞る仕組み（逆順索引・n-gram 索引）。今回は
  `PlainScan` への縮退のみ
