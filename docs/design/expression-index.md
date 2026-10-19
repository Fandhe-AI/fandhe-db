# 式索引の宣言側（parser と式の決定性検査）

対象: Issue #1647（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7
（spec 本文は転記しない）。#1648（キー計算・エンコード・カタログ永続化）は本書末尾の「Issue #1648」節。
後続: #1649（DML 維持）→ #1650（プランナ選択）→ #1651（UNIQUE・失効管理）。いずれも実施済み。

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
2. 要素ごとの先読み検査（`sql::index_expr::precheck_index_tokens`。トークン区間を左端優先で線形走査）
3. 要素の構文解析（`42601`／`54000`）
4. 文全体の構造検査（重複列・`USING hnsw` の列数・`UNIQUE` との併用・要素数上限）
5. 式要素の決定性（`sql::index_expr::check_index_expression_determinism`）
6. 部分索引の述語（[partial-index.md](partial-index.md) 参照）・文末
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
- **#1651**（実施済み。本書末尾の「Issue #1651」節）: `UNIQUE` 式索引の強制、参照列の `DROP COLUMN`（索引削除）・
  `ALTER TYPE`（再構築）・`RENAME COLUMN` 追随。
- **#1650**（実施済み。本書末尾の「Issue #1650」節）: 正規化構文での式一致判定（#1648 の正規化テキストを共有）。

## Issue #1648（格納側: 束縛・永続化・キー計算）

対象: Issue #1648（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10（spec 本文は転記しない）。
Issue #1647 の最終ゲートを縮小し、決定的な式の索引を `Storage::create_index` の write txn 内で束縛して永続化する。

### 生き残る `0A000`（未実装の組み合わせは fail-closed）

構文段の最終ゲート（`reject_unimplemented_expression_keys`）は次だけを残す。強制・追随が無い宣言を
永続化しないため。

| 組み合わせ | 担当 | 文言 |
| ---------- | ---- | ---- |
| `USING hnsw` ＋式 | — | `USING hnsw does not support expression indexes` |
| `UNIQUE` ＋式 | #1651（実施済み） | 受理に変更（本書末尾「Issue #1651」節） |
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
| 参照列の `RENAME COLUMN` | #1651 で式テキストを追随させる（本書末尾「Issue #1651」節。#1648 時点の `0A000` は撤去） |
| 参照列の `ALTER COLUMN TYPE` | #1651 で式を再束縛して再検証する（同上。#1648 時点の `0A000` は撤去） |
| 参照しない列の変更、テーブル改名 | 影響なし（式は修飾名を持たない） |
| `DROP TABLE` | 式索引ごと消える |

（#1648 時点の `RENAME`／`ALTER TYPE` の拒否は、#1651 で追随・再束縛に置き換えた。`CatalogError` の variant は
増やさず、`core_api.snapshot` を広げない方針は変わらない。）

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
| INSERT／UPDATE 時の式評価エラーで DML を失敗させる・crash 整合 | #1649（実施済み） | 下記「Issue #1649」節。全書き込み経路が commit 前に式を評価し、評価エラーを拒否する |
| WHERE の式一致・EXPLAIN 表示 | #1650（実施済み） | 下記「Issue #1650」節。ORDER BY の式一致は未対応 |
| UNIQUE 式索引・`RENAME`／`ALTER TYPE` の追随 | #1651（実施済み） | 下記「Issue #1651」節 |
| 式への `COLLATE`（ICU 照合の辞書） | 後続 | `0A000` |

## Issue #1649（DML 維持・構築時検証）

ポインタ: INDEX-10（`docs/spec/04-behavior/indexing.md`）。式の評価エラーで文を拒否する・NULL も
索引に含める・書き込みと同じトランザクションで維持する・クラッシュ後も行と一致する、の 4 点を満たす。

### 判断

| # | 判断 | 理由 |
| - | ---- | ---- |
| D1 | 永続の式索引表は作らない。式キー索引は従来どおりコミット済みの行からテーブル世代ごとに作り直す派生物のままにする | 読み手は #1650 が消費するメモリ上の `ScalarIndex` だけで、一意性は #1651 が `user_uniq` の拡張で扱える。第 2 の複製が無いので、クラッシュ後に行と食い違う状態が構造的に存在しない。DML は既に全経路で同じ write txn 内に世代を bump している |
| D1' | 守る不変条件は「式索引が存在するなら、テーブルの全行で式が評価エラーなく索引化できる値（NULL を含む）を返す」 | 旧キーは再構築で消え、新キーは不変条件の強制によって必ず再構築に入る。#1648 の fail-soft（`Unbuildable`）は多層防御として残す |
| D2 | DML ゲートは単一検査点 `constraint::enforce_row_constraints_in_txn` に式評価段を足す（呼び出し元の変更なし） | 検査順は 索引同期 → CHECK → **式評価** → UNIQUE → FK。参照アクションで書き換えた子表・COPY・一時表も同じ関数を通る |
| D3 | `CREATE INDEX` は保存前に既存行を全件評価する（全テナント・全可視性） | 失敗行を抱えた索引が恒久的に作れない状態で残るのを防ぐ。`ALTER TABLE ... ADD CHECK` と同じ設計判断 |
| D4 | `TenantWriteError::IndexExpressionEvaluationFailed(SqlSurfaceError)` を追加する | `CheckEvaluationFailed` の意味は CHECK に限られる。内側の `wire_code` を透過し、`Display`／`Debug` は値を含めない |
| D5 | crash-test は既存の `crash_tool_unique_index` に 3 つ目の表 `docs_xidx` を足して行い、新しい CI ジョブは作らない | required check の登録というリポジトリ設定の変更を要さない |

### 判定と SQLSTATE

| 事象 | 結果 |
| ---- | ---- |
| 式が NULL を返す | 可（索引に含める） |
| 式の評価エラー（0 除算 `22012` 等） | 内側の SQLSTATE を透過して文を拒否 |
| 数値が非有限（キーを表せない） | `22003` で拒否 |
| 静的な結果型と値の型の不一致 | `XX000` |
| 保存済みの式が再解析・再束縛・往復一致に失敗（漂流） | `XX000`（`CorruptSchema`。詳細は渡さない） |

- 起動時 opt-in（`declarations_enabled`）に**依存せず**、カタログに宣言があれば常に発火する。
- 値の分類は `sql::expression_key_index::classify_expression_value` を構築側と共有する。
- 式索引を持たない表のコストは索引カタログを 1 回走査するだけ（対象外エントリはテーブル名の照合だけで読み飛ばす）。
- 拒否は commit 前に返すため、行・台帳・世代のいずれにも痕跡を残さない。

### テナント境界

- DML ゲートはサーバー側で導出したテナントの物理キーで `written_ids` だけを読み戻して評価する。
  他テナントの行に触れず、存在オラクルにならない。
- `CREATE INDEX` の全件検証は運用者権限の DDL（`42501` ゲートの後）に限る。評価エラーは SQLSTATE を保った
  固定文言へ置換し、テナント・行 id・値を応答へ出さない。

### PostgreSQL との差

- PostgreSQL の `float8` は Infinity を索引化できるが、本実装はキーのエンコードが非有限を表せないため
  `22003` で拒否する。

### 検証

- 層 A: `crates/engine/tests/sql_expression_index.rs`（INSERT・複数行・UPSERT・UPDATE・DELETE・TRUNCATE・
  `CREATE INDEX` の既存行検証・明示トランザクション）、`table44_temp_table_dml.rs`（一時表）、
  `crates/wire-server/tests/err4_http_projection.rs`（NoSQL insert の `22012` 射影）。
- crash: `make crash-test-unique-index` が `docs_xidx` で、再起動後も式の評価失敗が `22012` で拒否され、
  コミット済みの全行が式を評価できることを確かめる。

### 対象外

- 分割実行 DML（`partitioned-dml.md`）での評価エラーは、既存の CHECK エラーと同じチャンク原子性の契約に従う。
- 本件より前の #1648 ビルドで作った式索引の既存失敗行は遡って検証しない（構築側の fail-soft で無害化される）。

## Issue #1650（プランナ選択・EXPLAIN 表示）

対象: Issue #1650（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7
（spec 本文は転記しない）。#1648 が構築した式キー索引を、WHERE の式述語の選択と EXPLAIN の索引名注記で
消費する。

### 判断

| # | 判断 | 理由 |
| - | ---- | ---- |
| D1 | 宣言を分類へ渡す。式述語は、宣言済みの式索引と構造一致したものだけを索引対応にする。公開の `classify_scalar_plan`・`ScalarShapeInput` は変えず、`pub(crate)` の `classify_scalar_plan_with` が一致判定関数（`finder`）を受ける | 宣言を見ずに形だけで分類する案は、式索引が無い表・opt-in なしでも `scalar_plan_kind != PlainScan` になり、索引の構築・統計加算が走って「opt-in なしの挙動を変えない」契約を破る。宣言を渡せば、宣言なし・不一致・opt-in なしの分類は従来とビット単位で同じ（`PlainScan`） |
| D2 | 一致判定は束縛済み式木（`BoundExpr`）の構造一致。保存済みの式テキストを再解析・再束縛（往復一致つき）した木と、WHERE の比較の非リテラル側を比べる。数値リテラルは `-0.0` と `0.0` を区別する（`PartialEq` に加えてデバッグ表現の一致を要求） | `BoundExpr` はレジストリやセッションを参照せず単独で評価できる純粋な木で、索引に入る式は決定性検査を通過済みなので、木が等しければ評価結果も等しい。関数名の大小文字や余分な括弧の差は束縛後に消える。`BoundStatement` に束縛前の WHERE を持ち回る改修は採らない |
| D3 | 対応する形は `<式> op <リテラル>`（`op` は `=`／`<`／`<=`／`>`／`>=`。左右逆は演算子を反転）。リテラルは式の結果型と同じ種類の定数だけ（TEXT・有限の数値・DATE／TIMESTAMP／TIME／TIMESTAMPTZ）。`BETWEEN` は 2 本の比較に脱糖されて交差になる | TEXT は `Collation::DEFAULT` のバイト列順と byte 等価、数値は `f64` 比較（`±0` は同値でキーは正規化済み）、時間型は同種 `i64` の比較で、評価器と同値。種類が違えば単位が違うので一致させない |
| D4 | 候補集合は「厳密一致」か `FallbackNoIndex` のどちらか。式述語の収集口は `collect_expr_index_predicates_with` の 1 つだけで、`filter_map` 形で述語を落とさない | `mask_trusted_defer`・`count_star_only`・`observe_group_count_only` は候補を WHERE の厳密一致集合とみなして再評価を省く。#1649 の不変条件（式索引があるなら全行で式が評価できる）と、式キーは 1 行でも評価に失敗すれば作らない構築規則により、再評価を省いても全走査と結果が変わらない |
| D5 | EXPLAIN の語彙は増やさない。1 件だけの式述語は、TEXT 結果の `=` なら `index_equality`、それ以外は `index_typed_range`、2 件以上は `index_conjunction`。式索引名は `index=<名前>` の注記で出す（INDEX-7 と同じ規則） | 数値列のレーン（#1359）が語彙を増やさなかった前例に合わせる。`ScalarPlan::IndexExpression` の新設は代替案として残す |
| D6 | opt-in なし（`declarations_enabled == false`）では式述語は常に全走査。式キーの構築も opt-in 時だけ（#1648）で、EXPLAIN の索引名も opt-in 時だけ | #1648 から持ち越された論点への回答 |

### 一致しない形（対象外。残余述語＝全走査のまま）

- `TextCompare`（ICU 照合の比較。順序がバイト列順のキーと合わない）、`IS NULL`、`IN`、`LIKE`、`<>`、OR 群。
  式に対する `BETWEEN`・`IN`・`IS NULL` は現状の構文段が受理しない形もある。
- 式同士の比較、リテラルの種類が式の結果型と食い違う比較（`DATE` 式と `TIMESTAMP` リテラル等。束縛器が
  昇格ノードで包むため一致しない）。
- 宣言した式と構文が異なる式（`lower(x)` に対する `upper(x)`、`n + 1` に対する `n + 2`）。

### 構成

- `sql::expression_key_index::ExpressionIndexMatcher`: 宣言を再束縛した式木の集合。`find(expr, ty)` が分類・実行・
  EXPLAIN の共通の一致判定。再束縛・往復一致・識別子検証に失敗した宣言は黙って外す（fail-soft）。
- `sql::scalar_index::resolve_expression_matcher_in_txn`: クエリと同一の `read_txn` から解決する唯一の入口。
  opt-in なし・式候補なし（構造フィルタ）・カタログ読み取り失敗は空のマッチャ。
- 実行時: `sql::exec`・`sql::aggregate`・`sql::group_by` が、分類と候補収集の双方へ同じ `finder` を渡す。
- EXPLAIN: 検索・集計は束縛済みの WHERE から直接、`USING PLAN` は `ExplainShape` が保留した式候補を、世代照合を
  通過した `post_check_txn` の宣言で `resolve_expressions` が確定する。式索引名は列被覆名と合流させる
  （列述語がある場合は全列が被覆されているときだけ）。名前は `ExplainIndexNames::new` の識別子検証を通る。
- `ExplainShape` は式候補を保持するため `Copy`・`Eq` ではなくなった（BREAKING。`from_filters` のシグネチャは不変）。

### 既知のコストと後続

- `declared_index_targets_in_txn` には世代キーのキャッシュが無い。式候補を含むクエリは毎回カタログ走査と
  宣言の再束縛を行う（宣言数は `MAX_INDEX_COUNT` が上限で、DDL 権限者しか増やせない）。キャッシュ化は後続候補。
- ORDER BY の式一致による索引順走査（`ExpressionKeyIndex::ordered_slots` の消費）は本件の対象外。

## Issue #1651（UNIQUE と失効管理）

対象: Issue #1651（親 #1646）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-9
（spec 本文は転記しない）。#1648〜#1650 で残っていた 3 つの暫定 `0A000`（`CREATE UNIQUE INDEX ... ((式))`・
参照列の `RENAME COLUMN`・参照列の `ALTER COLUMN TYPE`）を撤去し、式一意性の強制と列変更への追随を実装する。

### 判断

| # | 判断 | 理由 |
| - | ---- | ---- |
| D1 | 一意の式索引は `TableSchema::unique_constraints` へ展開しない。索引カタログ（`IndexDef`）だけに持ち、強制は永続一意索引 `user_uniq/{table}` の拡張で行う | 列だけの一意索引は UNIQUE 制約へ展開する方式だが、式には列リストが無い。制約に入れると `ON CONFLICT` の推論・FK の参照先・`information_schema` へ波及する。INDEX-9 の「索引は制約として扱わない」とも整合し、#1649 の D1 が予告した `user_uniq` の拡張に従う |
| D2 | キーの仕様は `constraint::UniqueKeyPlan`。序数は主キー → UNIQUE 制約 → 一意の式索引（索引名の昇順）の順。要素は素の列または式で、キーは要素ごとの正準成分の連結。1 要素でも NULL ならキーを持たない（NULLS DISTINCT）。混在形 `((lower(a)), b)` も同じ合成で扱う | 列要素は既存の `push_canonical_component`、式要素は `classify_expression_value` の分類を共有し、タグ（`0xE1`〜`0xE3`）は列成分のタグ（0〜14）と重ならない。書き込み検査・バックフィル・読み戻し・作成時の重複走査が `UniqueKeyPlan::row_keys` の 1 関数を通り、判定が 2 系統にならない。列だけのキーのバイト列は従来と同一 |
| D3 | マーカーのシグネチャへ一意の式索引の仕様（索引名・要素の種別・列名と型タグ、または式の正規化 SQL・結果型・参照列の名前と型の指紋）を追記し、`FORMAT_VERSION` を 2 → 3 に上げる | 作成・削除・式の変更・参照列の改名や型変更のどれでもマーカーが不一致になり、古い正引きエントリを流用せず再構築する（fail-closed）。上げた版は全テナントで初回の書き込み時に 1 回だけ再構築を起こす |
| D4 | 式索引の宣言の読み込みと式のコンパイルは 1 文につき 1 回。`enforce_row_constraints_in_txn` が索引カタログを 1 回だけ走査し、式の評価段（#1649）と一意検査段へ同じ `CompiledIndexExpressions` を渡す | 式索引の無い表のコストを「索引カタログの 1 回走査」のまま保つ。TRUNCATE・DELETE の後片付けは、`user_uniq` の物理的な存在確認だけで読み飛ばす（PK・UNIQUE が無い表でも式一意索引の永続索引があり得るため、スキーマ側の有無では判定しない） |
| D5 | カタログ形式に新版 `v5`（`v4` と同じ行構成の末尾に固定行 `unique`）を追加する。`v1`〜`v4` のバイト列は変えない | 旧バイナリは `v5` を未知の版として拒否する（前方互換なし。BREAKING） |
| D6 | `RENAME COLUMN` は再解析 → AST 上の識別子写像（`map_expr_idents`。1 回の走査で一括適用） → 改名後のスキーマで再束縛 → 写像後の参照列集合の一致確認 → 正規化テキストと参照列を保存。どの段の失敗も `0A000` で txn を破棄する | 文字列置換を使わない。`CHECK` の改名追随と同型で、列の入れ替え（`a→tmp, b→a, tmp→b`）でも連鎖置換が起きない |
| D7 | `ALTER COLUMN TYPE` は行の書き換え・キー再検証の後、世代 bump の前に、変更列を参照する式索引を新スキーマで再束縛し、全式索引を再コンパイルして全テナント・全可視性の既存行を再検証し、一意の式索引はテナントごとの重複を再走査し、`user_uniq` を失効させる | 新しい値・新しい型で検査する必要がある。メモリ上の式キー索引は世代ごとに作り直す派生物なので、「再構築」は世代 bump で成立する |

### SQLSTATE と挙動

| 操作 | 結果 |
| ---- | ---- |
| `CREATE UNIQUE INDEX ... ((式))` | 受理。式・列要素の束縛は #1648 と同じ。素の列要素は一意キー許可型だけ（`id` 等は `0A000`）。制約名前空間との衝突は `42P07` |
| 作成時に既存行（全テナント・全可視性）へ重複 | `23505`。テナントごとに独立して判定。索引・世代は変わらず、応答にテナント・行・値・件数を含めない |
| INSERT／UPDATE／UPSERT／COPY／NoSQL の重複 | `23505`（全書き込み経路が `enforce_row_constraints_in_txn` を通る）。不可視行とも衝突し、別テナントとは衝突しない |
| `DROP INDEX` | 強制が外れる。`user_uniq` を失効させ、再作成ではマーカーを作り直す |
| 参照列の `DROP COLUMN` | 一意・非一意とも索引を連鎖削除（PostgreSQL と同じ。`CASCADE` 不要）。一意の式索引を消したら `user_uniq` も失効 |
| 参照列の `RENAME COLUMN` | 式テキスト・参照列・素の列要素を追随。追随できなければ `0A000`（無変化） |
| 参照列の `ALTER COLUMN TYPE` | 再束縛の失敗はその SQLSTATE（`42883`／`42804`／`42P17`／`0A000`）、既存行の評価エラーはその SQLSTATE（固定文言）、一意の重複は `23505`。いずれも表・索引は無変化 |

### PostgreSQL との差

- 整数の式は `f64` キーになるため、2^53 を超える `BIGINT` の値は同じキーに丸まり、PostgreSQL では一意な値が
  `23505` になり得る（拒否が増える側の差で、fail-open ではない）。
- 非有限の数値は `22003` で拒否する（#1649 から継続）。TEXT の一意性はバイト等価（決定的照合と同じ）。

### 検証

- `crates/engine/tests/sql_expression_unique_index.rs`: 全書き込み経路・NULLS DISTINCT・テナント独立・不可視行・
  複合形・ロールバック・作成時の重複・`IF NOT EXISTS`・名前衝突・再オープン・`DROP`／`RENAME`／`ALTER TYPE`。
- `make crash-test-unique-index`: `docs_xidx` の一意の式索引について、`verify_unique_index_read_only` の成功と、
  再起動後の大文字小文字違いの再挿入が `23505` になることを確かめる（新しい CI ジョブは作らない）。

### 対象外

- `ON CONFLICT ((式))` の推論（SQL-49。推論は `unique_constraints` だけを見るので挙動は不変）。
- 一意の式索引を FK の参照先にすること（列リストが無いため不可。PostgreSQL も同様）。
- 式要素への `COLLATE`・matview への式索引（従来どおり `0A000`）、ORDER BY の式一致による索引順走査。
