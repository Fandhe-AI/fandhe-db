# 式索引の宣言側（parser と式の決定性検査）

対象: Issue #1647（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7
（spec 本文は転記しない）。#1648（キー計算・エンコード・カタログ永続化）は本書末尾の「Issue #1648」節。
後続: #1649（DML 維持）→（プランナ選択は #1650）→（UNIQUE・失効は #1651）。

## 範囲

`CREATE INDEX ... ((<式>))` と関数呼び出し形 `(f(col))` を構文上受理し、式の決定性
（PostgreSQL の IMMUTABLE 相当）を判定する。**カタログ非参照の構文段だけ**で完結する（#1647 時点では決定的と判定した
式も最後に `0A000`（`expression indexes are not supported yet`）で拒否していた。#1648 で最終ゲートを
縮小し、束縛と永続化は write txn 内へ移した）。#1647 時点で永続化しなかった理由:

- 参照列の `DROP COLUMN`／`RENAME COLUMN`／`ALTER TYPE` への追随（#1651）が無いまま宣言が残る。
- キー計算・維持（#1648・#1649）が無い宣言は効かない索引になり、`UNIQUE` なら一意性の強制漏れ
  （fail-open）になる。
- 式の束縛（`42703`・`42804`・`42883`）と型依存の決定性判定は、TOCTOU を避けるため永続化と同じ
  write txn（`Storage::create_index`）で行うべき（#1648 の担当）。

## 列リスト要素の構文

| 形 | 例 | 扱い |
| -- | -- | ---- |
| 裸の識別子 | `lang`・`lang COLLATE "C"` | 従来どおり列要素 |
| 関数呼び出し形 | `lower(lang)` | 一次式のみ。直後が `,`／`)`／`COLLATE` 以外（`lower(a) + 1`・`lower(a)::text`）は `42601` |
| 括弧式 | `(lang \|\| body)` | 式をノード予算内で解析し、対応する `)` でちょうど終わる必要がある（`(a) + 1` は `42601`） |
| 式が単なる列参照 | `((lang))`・`((lang COLLATE "C"))` | 通常の列要素へ正規化（重複列 `42601` の対象） |
| リテラル・空括弧 | `('a')`・`()` | 従来どおり `0A000` |

## 判定順（全て構文段・カタログ非参照）

1. 字句
2. 要素ごとの先読み検査（`sql::index_expr::precheck_element_tokens`。トークン区間を左端優先で線形走査）
3. 要素の構文解析（`42601`／`54000`）
4. 文全体の構造検査（重複列・`USING hnsw` の列数・`UNIQUE` との併用・要素数上限）
5. 式要素の決定性（`sql::index_expr::check_index_expression_determinism`）
6. 部分索引 `WHERE`（`0A000`）・文末
7. 最終ゲート（`reject_unimplemented_expression_keys`。#1648 以降は `USING hnsw`／`UNIQUE`／`COLLATE` と式の
   組み合わせだけ `0A000`。それ以外は実行段の束縛へ渡す）

## SQLSTATE

| 検出 | SQLSTATE | 固定文言（PostgreSQL 17 の公開文言） |
| ---- | -------- | ------------------------------------ |
| `(` 直後が `SELECT`（`EXISTS`・`IN`・`ARRAY(` を含む） | `0A000` | `cannot use subquery in index expression` |
| 集計・ウィンドウ関数名の呼び出し直後が `OVER` | `42P20` | `window functions are not allowed in index expressions` |
| 集計関数（`OVER` なし） | `42803` | `aggregate functions are not allowed in index expressions` |
| 集合返却関数 | `0A000` | `set-returning functions are not allowed in index expressions` |
| 確定の非決定式 | `42P17` | `functions in index expression must be marked IMMUTABLE` |
| 組み込み以外の関数 | `0A000` | `user-defined functions in index expressions are not supported` |
| 決定的な式（最終ゲート。#1647 時点のみ。#1648 で廃止） | `0A000` | `expression indexes are not supported yet` |

文言は `&'static str` の固定文言（新 variant `InvalidObjectDefinition`・`GroupingError`・
`WindowingError`）で、列名・値・テナント名を載せられない型にしている。

## 決定性判定表（構文段）

`Expr` の全 variant を `_ =>` 無しで網羅する再帰関数。引数の型に依存せず確実に非決定的なものだけを
`42P17` にし、型に依存するものは通して #1648 の束縛後に再判定する。

| 区分 | 対象 | 構文段 |
| ---- | ---- | ------ |
| 非決定的（確定） | 括弧なし SQL 値関数（`CURRENT_TIMESTAMP` 等）、`now`・`transaction_timestamp`・`statement_timestamp`・`clock_timestamp`、1 引数の `timezone`、`concat`・`concat_ws`、`to_json`・`to_jsonb`・`(j)son(b)_build_object/array`、`array_to_string`、本エンジンに無い既知の非決定名（`random`・`nextval` 等の小さな明示リスト） | `42P17` |
| 型依存（#1648 が束縛後に再判定） | `date_part`・`date_trunc`・2 引数の `timezone`、時刻型が絡む `CAST`・算術、`TIMESTAMPTZ` リテラル（セッション TZ 解決のもの）、`COLLATE`、SQL/JSON 問い合わせ | 通す（子は再帰検査） |
| 決定的 | リテラル・列参照・`lower`・`upper`・`length`・算術・`CASE`・`COALESCE`・`NULLIF` など | 通す |
| 組み込み以外 | セッション UDF・未知名 | `0A000` |

非決定名の区分は PostgreSQL 17 の `pg_proc.provolatile` が全オーバーロードで非 IMMUTABLE のものに限る
（偽の `42P17` を避ける）。

## PostgreSQL・spec との差

- 順位関数の `OVER` なし、非窓関数への `OVER` は PostgreSQL では `42809` だが、本実装は fail-closed の
  `42601`（ERR-6 の既存行での扱いは spec 側の確認事項）。
- UDF は `0A000`。`UdfRegistry` はセッション単位で永続化されず、永続する索引がセッション外で消える関数に
  依存すると維持できず fail-open になるため。INDEX-10 の宣言的 UDF の期待は、永続する関数基盤が入るまで
  満たせない（spec 側の確認事項）。
- `42P17` の ERR-6 への収載状況の確認が必要（spec 側の確認事項）。

## 後続 Issue への申し送り

- **#1648**（実施済み。本書末尾の節を参照）: `reject_unimplemented_expression_keys` の呼び出しを外し、`IndexDef` に式要素を追加
  （カタログ形式の新版。式は正規化 SQL テキストで保存し再パースの往復一致を検査）。
  `Storage::create_index` の write txn 内で式を束縛して型依存区分を再判定し、`TimestampTzLiteral`
  （`session_local`）を確定値へ畳み、式から参照列集合を抽出する。
- **#1651**: `UNIQUE` 式索引の強制、参照列の `DROP COLUMN`（索引削除）・`ALTER TYPE`（再構築）・
  `RENAME COLUMN` 追随。
- **#1650**: 正規化構文での式一致判定（#1648 の正規化テキストを共有）。

## Issue #1648（格納側: 束縛・永続化・キー計算）

対象: Issue #1648（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10（spec 本文は転記しない）。
Issue #1647 の最終ゲートを縮小し、決定的な式の索引を `Storage::create_index` の write txn 内で束縛して永続化する。

### 生き残る `0A000`（未実装の組み合わせは fail-closed）

構文段の最終ゲート（`reject_unimplemented_expression_keys`）は次だけを残す。強制・追随が無い宣言を
永続化しないため。

| 組み合わせ | 担当 | 文言 |
| ---------- | ---- | ---- |
| `USING hnsw` ＋式 | — | `USING hnsw does not support expression indexes` |
| `UNIQUE` ＋式 | #1651 | `CREATE UNIQUE INDEX does not support expression indexes yet` |
| 式を含む索引の `COLLATE`（式への指定は要素末尾・式の最上位とも、素の列への指定も。v4 は照合を持たない） | 後続 | `COLLATE on an index expression is not supported yet` |

### 束縛の判定順（`sql::index_expr::bind_index_expression`。write txn 内）

1. 名前衝突 → 対象の種別 → テーブル存在 → 素の列の整合（`validate_index_columns`）
2. 式ごとの束縛
   1. 定義時のセッション TZ で、ゾーン無し `TIMESTAMPTZ` リテラル（`session_local`）を UTC の確定値へ畳む
      （`Cast` 等の内側も再帰。`render_expr` の `+00` 形と一致させる）
   2. 束縛（`udf_call::bind_order_expr`。UDF レジストリは空）。未知列 `42703`・型不整合 `42883`／`42804`
      は束縛側のエラーをそのまま透過する
   3. 型によって決まる決定性の再判定（`42P17`。下表）
   4. 結果型ゲート（`0A000`: `index expression result type is not supported yet`）
   5. 正規化テキストへ描画し（`check_constraint::render_expr` を共有。第 2 のレンダラーは作らない）、
      再解析して元の式と一致することを確認。不一致は保存せず `0A000`（`index expression cannot be
      stored in normalized form`）
   6. 参照列の抽出（走査マスクから算出。VECTOR 列を参照する場合は VECTOR 列も含める）
3. 件数上限 → 保存 → 世代 bump（テーブル世代と索引カタログ世代）

束縛エラーは commit 前に返り副作用ゼロ。権限ゲート通過後にだけ到達するため存在オラクルにならない。

### 型によって決まる決定性の判定表（束縛後）

PostgreSQL 17 で IMMUTABLE でないオーバーロードに束縛されるものを `42P17`（`functions in index expression
must be marked IMMUTABLE`）にする。型を決められない部分式は判定に寄与しない（式全体は束縛に成功している）。

| 対象 | 判定 |
| ---- | ---- |
| `TIMESTAMPTZ` 型の引数を取る関数呼び出し（`date_part`・`date_trunc`・`extract` 等） | `42P17`。`timezone(<zone>, <値>)` の 2 引数形だけは IMMUTABLE のため通す |
| `TIMESTAMPTZ` と `TIMESTAMP`／`DATE` の演算 | `42P17` |
| `TIMESTAMPTZ ± INTERVAL` | `42P17` |
| `CAST`: 変換元または変換先の一方だけが `TIMESTAMPTZ` | `42P17`（文字列リテラルからの変換も評価時の TZ に依存するため） |
| `CAST`: 時刻・間隔型と文字列系（`text`／`varchar`／`bpchar`）の相互変換 | `42P17`（`DateStyle`／`IntervalStyle` 依存） |
| 上記以外（`TIMESTAMP`／`DATE` 引数の `date_part`、`TIMESTAMP ± INTERVAL`、数値の文字列化など） | 通す |

### 索引化できる結果型

| 結果型 | キー |
| ------ | ---- |
| `TEXT` | UTF-8 バイト列の辞書順（`Collation::DEFAULT`） |
| 数値（`f64`） | `-0.0` を `+0.0` へ正規化した有限値の全順序ビット表現（非有限値は不可） |
| `DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` | `i64` |
| 上記以外（`BOOLEAN`・`VECTOR`・`INTERVAL`・`JSON(B)`・`NUMERIC`・`UUID`・`BYTEA`・配列） | `0A000`（固定文言） |

整数の式は評価器が `f64` で返すため `f64` のキーになる（2^53 を超える `BIGINT` は同一キーに丸まり得る。
順序はキー順で一貫する）。

### カタログ形式 `v4`

式要素を 1 つでも含む宣言だけが新版 `v4` で保存される。式を含まない宣言の `v1`／`v2`／`v3` のバイト列は
一切変わらない。**旧バイナリは `v4` を未知の版として拒否する（前方互換なし。`v2` の前例どおり）**。

行構成: `v4`／テーブル／`scalar`／列 CSV（式の位置は空トークン）／照合行（全要素 `-`）／式行（列の位置は
`-`、式の位置は正規化 SQL の UTF-8 を小文字 16 進にしたもの）／参照列行（式ごとに `;` 区切り、式内は
`,` 区切り）。式テキストは `,`・改行・`'` を含み得るため 16 進で格納する。デコードは次を
`CorruptSchema` で fail-closed に拒否する: 未知の版・行数の過不足・末尾の余剰・hex／UTF-8 の不正・
列と式の排他関係の破れ・式位置の照合・式要素が無い `v4`（非正準形）・参照列の識別子不正・長さ超過
（式テキスト 4096 バイト、値全体 `MAX_CATALOG_VALUE_LEN`）。デコード時に構文の再解析はしない
（ホットパスの負荷を避ける。再解析と再束縛は作成時と式キー構築時に行う）。

### 列変更との関係

| 操作 | 挙動 |
| ---- | ---- |
| 参照列の `DROP COLUMN` | 式索引を連鎖削除する（PostgreSQL と同じ。古い宣言を残さない） |
| 参照列の `RENAME COLUMN` | `0A000`、カタログ・世代は不変（追随は #1651） |
| 参照列の `ALTER COLUMN TYPE` | `0A000`、カタログ・世代は不変（再構築は #1651） |
| 参照しない列の変更、テーブル改名 | 影響なし（式は修飾名を持たない） |
| `DROP TABLE` | 式索引ごと消える |

`RENAME`／`ALTER TYPE` の拒否は write txn 内で判定する（`AlterCheckError::Sql` で固定文言を返す。
`CatalogError` の variant は増やさず、`core_api.snapshot` を広げない）。

### 式キー索引（`sql::expression_key_index`）

`ScalarIndex` の構築時に、RLS 適用済みスナップショットの各スロットで式を評価して
`(キー, スロット)` をキー昇順（同値はスロット昇順）で保持する。NULL 結果は `null_slots` に分ける。
順序付き走査 API（`expression_ordered_slots` は NULL を末尾に置く `ASC NULLS LAST`、
`expression_null_slots`、範囲照会）は `pub(crate)` で、現時点の消費者はテストだけ。

- **宣言が構築に効く条件**: 従来どおり起動時 opt-in（`declarations_enabled`）が有効なときだけ。
  無効なら式キーは作らない。
- **fail-soft かつ「狭めるだけ」**: 再束縛の失敗（参照列の変化等）・1 行でも式の評価エラー・非有限の
  `f64`・予算超過（`check_scalar_index_budget`）・確保失敗のいずれでも、その式キーだけを作らない。
  部分的な索引は作らない。既存の列索引・クエリの成否と結果には影響しない。
- **untrusted**: 保存済みテキストは再解析・再束縛し、描き戻して一致しなければ使わない。
- **メモリ**: 確保前に予算へ計上し、`approx_bytes` に確保容量ベースで加算する。
- **列宣言との関係**: 式のみの宣言は列宣言ではなく、`Declared` の絞り込みを起こさない。`explain` の
  宣言一覧にも式要素は出さない（#1650）。

### 暫定の挙動と後続 Issue

| 事項 | 担当 | 本件での挙動 |
| ---- | ---- | ------------ |
| INSERT／UPDATE 時の式評価エラーで DML を失敗させる・crash 整合 | #1649 | DML は従来どおり成功する。構築時に評価エラーがあれば、その式キーは作らない |
| WHERE／ORDER BY の式一致・EXPLAIN 表示 | #1650 | 式キーは構築するがどこからも消費しない（クエリ結果・計画は不変） |
| UNIQUE 式索引・`RENAME`／`ALTER TYPE` の追随 | #1651 | 構文段で `0A000`／write txn 内で `0A000` |
| 式への `COLLATE`（ICU 照合の辞書） | 後続 | `0A000` |
