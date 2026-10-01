# 3 テーブル以上の結合と、結合での集計形・スカラー ORDER BY・OR／IN／列同士の比較（Issue #1190・SQL-28・RLS-10・TASK-212）

## 位置づけ

`inner-join.md`（Issue #925）・`outer-join.md`（Issue #926）が受理していた JOIN は、
「2 テーブル・等価 `ON`・`AND` だけの列 vs リテラル `WHERE`・`LIMIT` 必須の広域取得形」
だけだった。本 Issue で次の形を、PostgreSQL と同じ結果を返す形で受理する。RLS は各テーブル
参照へ独立に適用し、`wire_code` のエラー契約・fail-closed・テナント境界は変えない
（ポインタ: SQL-28・RLS-10・TASK-212・ERR-6）。

- 3 テーブル以上の left-deep 連鎖 JOIN（2〜`MAX_TABLE_REFS`＝8 relation。段ごとに
  `INNER`／`LEFT`／`RIGHT`／`FULL [OUTER]` を混在できる）
- `WHERE` の木構造（`AND`／`OR`／括弧）と、`IN ('<lit>', ...)`・列同士の比較（`= < <= > >=`）
- 非集計形のスカラー列 `ORDER BY`
- 集計形（`COUNT`／`SUM`／`AVG`／`MIN`／`MAX`・`GROUP BY`・`HAVING`・`ORDER BY`・
  `LIMIT`／`OFFSET`）

実装は `crates/engine/src/sql/join/`（責務ごとに `plan`・`exec`・`residual`・`aggregate`・
`values` へ分割）にある。公開 API（`sql::allowlist` の `pub` な型）は変更していない
（新しい情報は `ValidatedJoin` の `pub(crate)` フィールドと `pub(crate)` 型へ追加し、
`pub enum JoinProjection`・`pub enum Statement` は不変）。

## 受理文法（差分）

```text
select      := SELECT ( '*' | item {',' item} ) FROM rel { join_type JOIN rel ON cond }
               [WHERE or_expr] [GROUP BY colref {',' colref}] [HAVING having {AND having}]
               [ORDER BY colref [ASC|DESC] {',' ...}] [LIMIT n] [OFFSET m]
item        := colref [AS alias] | agg
agg         := COUNT '(' '*' ')' | (COUNT|SUM|AVG|MIN|MAX) '(' colref ')'   [AS alias]
or_expr     := and_expr { OR and_expr }
and_expr    := atom { AND atom }
atom        := '(' or_expr ')' | leaf
leaf        := colref ('=' | LIKE | '<' | '<=' | '>' | '>=') ( literal | colref )
             | colref IN '(' literal {',' literal} ')' | colref '=' (true|false) | colref
having      := <項目名> op ['-'] <数値>
```

- k 段目の `ON` は従来どおり「`colref = colref` の `AND` 結合（1〜`MAX_JOIN_CONDITIONS`）」で、
  片側は新しく加わる relation（k+1 番目）、もう片側は既出の relation（0..=k）でなければ
  ならない。両側が既出同士・両側とも新 relation の場合は `42601`
- 段 k の束縛スコープは relation 0..=k+1 だけで構築する。未出現の relation を前方参照
  すると `BindingScope::resolve` が `42P01` を返す（PostgreSQL と同じ分類）
- 非集計形は従来どおり `LIMIT` 必須（広域取得形を維持）。集計形は `LIMIT` を省略できる
- 集計形の判定: SELECT リストに集計項目がある、または `GROUP BY`／`HAVING` がある。
  `AS` 付きの列だけの SELECT リストは非集計形として扱い（`LIMIT` 必須）、別名は出力名に反映する。`GROUP BY` なしで素の列と集計が混在する形・`GROUP BY` に無い素の列は
  `42601`（fail-closed）

### `42601` のまま残す形

ベクトル順位付けとの併用（`ORDER BY ... <=>`・`HYBRID`・`USING PLAN`・`USING MODE`・
`HINT ORDER`）、`CROSS`／`NATURAL` JOIN、`JOIN ... USING (...)`、FROM でのビュー参照、
JOIN の WHERE 内のサブクエリ・`EXISTS`、`$n` パラメータ、`NOT`・`NOT IN`・`IS [NOT] NULL`・
`BETWEEN`・`<>`／`!=`・式、`COUNT(DISTINCT ...)`・集計引数の式・VECTOR 列を引数とする集計・
ウィンドウ関数・`DISTINCT`・`t.*`、JOIN を含む `EXPLAIN`／`DECLARE CURSOR`／`CREATE VIEW`
本体／集合演算の枝／`IN (SELECT ...)` の内側。

## WHERE の意味論（`sql::join::plan`）

1. `WHERE` を最上位の `AND` で conjunct に分解する
2. 各 conjunct について「参照する relation 集合」と「strict な relation 集合」を求める。
   strict とは、その relation が NULL 補完されているとき必ず偽になることを指す。受理する
   葉（`=`・`LIKE`・比較・bool 系・`IN`・列同士の比較）はすべて NULL 入力で偽（または不定）
   になるため、葉の strict 集合は参照集合と一致する。`AND` は和集合、`OR` は共通集合
   （relation を跨ぐ `OR` は、片方の分岐が真になりうるためどの relation についても
   strict とみなさない）。`is_null_rejecting` はワイルドカード無しの網羅的 `match` のまま
   残し、将来 `IS NULL`／`NOT` の葉を足したときにコンパイルエラーで検出させる
3. **外部結合の簡約**: strict な relation r を NULL 補完しうる段の保存側フラグを落とす。
   段 j（新 relation は j+1）では、r = j+1 なら「これまでの結合結果側の未一致行を残す」
   フラグを、r ≤ j なら「新 relation 側の未一致行を残す」フラグを落とす。`LEFT`／`RIGHT`
   は `INNER` へ、`FULL` は一方だけ落ちると `RIGHT`／`LEFT` へ変わる。変換で落ちる行は
   すべて r が NULL の行で、その行は strict な述語で必ず落ちるため結果は変わらない。
   従来の 2 テーブルの簡約規則（欠損側に述語があれば `INNER` 相当）はこの一般規則の
   特殊ケースになる。判定できないとき（relation を跨ぐ `OR` 等）は簡約せず残余評価に倒す
4. **プッシュダウン**: 単一 relation で完結し列同士の比較を含まない conjunct（`OR`・`IN` を
   含む）は、その relation の側スキャン（`ValidatedScan::where_predicates`）へ非修飾の
   `WherePredicate` として渡す。リテラルの型解析・評価は単一テーブル経路
   （`bind_scan_with_dummy_flags`）を完全に共有し、第 2 のリテラル解析器を作らない
5. **残余**: それ以外の conjunct（列同士の比較・relation を跨ぐ `OR`）は結合後に行単位で
   評価する（`sql::join::residual`）。残余の中の単一 relation の部分木は、束縛時に
   `bind_where_predicates` でその relation のスキーマに対して 1 回だけ束縛し（1 分岐の
   `OR` 群として保持するため、TEXT の範囲比較の式レーン振り替えも含めて評価できる）、
   実行時は結合行のセルから組み立てた `scanned`（`Cell` → `ScalarRef` アダプタ）へ当てる。
   その relation が NULL 補完されている行では偽（不定）とする。`NOT` を受理しないため
   `AND`／`OR` は単調で、「NULL → 偽」の 2 値評価で「真の行だけを残す」結果は 3 値論理と
   一致する（`NOT` を対象外にする根拠）
6. **列同士の比較の型規則**: 比較クラス（整数〔疑似列 `id`・`INTEGER`・`BIGINT`〕・浮動・
   `NUMERIC`・`TEXT`・`BOOL`・`DATE`・`TIMESTAMP`・`UUID`・`BYTEA`・`ENUM`〔型名一致〕）が
   一致しないと `42804`。`VECTOR`・`JSON`・`ARRAY` は `42601`。整数クラスは `id`（`u64`）と
   符号付き整数を混在比較できるよう `i128` へ正規化する。NaN・±0 の規約は単一テーブルの
   `ORDER BY` と同じ `order_value::compare_f64` を再利用する

## 実行（`sql::join::exec`）

- 各 relation を、呼び出したセッションの `PolicyContext` で `execute_scan_with_budget` に
  独立に通す（RLS・fail-closed の写像を継承する。3 方向の自己結合でも参照ごとに独立に
  評価する）。予算は文全体で 1 つの `JoinBudget` を共有し、relation ごとの行数上限
  （`MAX_JOIN_INPUT_ROWS`）超過は `54000`
- 中間表現は「タプル＝relation ごとの走査位置（`Option<u32>`。`None` は NULL 補完）」。
  段ごとに新 relation の行でハッシュ表を作り（キー正準化は従来の `encode_key_component`）、
  これまでのタプルをプローブする。NULL キーは登録・照合しない
- **段ごとに**、実体化前のカーディナリティ（一致ペア＋未一致の補完行）が
  `MAX_JOIN_OUTPUT_ROWS` を超えたら `54000`（`LIMIT`／`OFFSET` の値には依存させない）。
  簡約の結果、結合順によって行数上限・カーディナリティ判定のタイミングが旧実装と変わる
  場合があるが、結果は同一
- **決定的順序**: 最終タプルは `(is_none_0, idx_0, is_none_1, idx_1, ...)` の安定ソートで
  固定する（`sort_unstable*` は使わない。`scripts/check_sort_determinism.sh` ゲート対応）。
  2 relation のときは従来の `(l.is_none(), l, r.is_none(), r)` と同一の順序
- 処理順: 結合 → 残余 WHERE →（非集計形）スカラー `ORDER BY` → `OFFSET`／`LIMIT` → 投影、
  （集計形）グループ化・集計 → `HAVING` → `ORDER BY` → `OFFSET`／`LIMIT`

### スカラー ORDER BY（非集計形）

安定ソートで、同値のときは結合直後の決定的順序を保つ。NULL の位置は PostgreSQL 既定
（`ASC` なら末尾、`DESC` なら先頭。`sql::scan` の単一テーブル経路と同じ）。投影していない
列でも指定できる（側スキャンの投影へ追加する）。比較できない型（`VECTOR`・`JSON`・`ARRAY`）は
単一テーブル経路と同じ `22000`。

### 集計形

第 2 の集計エンジンは作らず、`sql::window` が窓集計で採る方式と同じく既存の
`Accumulator`（`sql::aggregate`）を再利用する。項目の束縛は
`BoundAggregateItem::bind`（単一テーブルの `resolve_aggregate_input` と同じ判定。型不整合は
`22000`）を relation のスキーマに対して呼び、結果の型・`22003`（桁あふれ）・`finish` は
既存のまま継承する。

- NULL 補完された relation を引数とする項目は NULL 入力として観測しない
  （`COUNT(*)` だけは常に数える）
- `GROUP BY` キーは結合キーと同じ型クラス（整数・`TEXT`・`BOOL`・`DATE`・`TIMESTAMP`・
  `UUID`・`BYTEA`・`ENUM`）に NULL タグを足して正準化し、NULL 同士は 1 グループにまとめる。
  それ以外の型は `22000`。グループ数の上限は `group_by::MAX_GROUPS`（新グループの確保前に
  判定。超過は `54000`）
- `GROUP BY` なしの集計は、結合結果が 0 行でも 1 行を返す（`COUNT` は 0、他は NULL）。
  `GROUP BY` ありで 0 行なら 0 行
- `HAVING` は `group_by::having_matches` を再利用する。数値として比較できない集計結果
  （`TEXT`・`NUMERIC`・`DATE`・`TIMESTAMP` の `MIN`／`MAX` 等）は単一テーブル経路と同じ
  `22000`
- `ORDER BY` の対象は `GROUP BY` キー（修飾／非修飾／別名）か集計項目名。キーと項目の両方に
  一致する名前（重複した集計名・キー別名の重複を含む）は曖昧として `42702`、どれにも
  一致しなければ `22000`（`HAVING` も重複集計名・キーの出力名と集計項目名の衝突は `42702`。Issue #1270）。NULL の位置は単一テーブル集計
  （`group_by::cmp_cells_pg_nulls`。`ASC` 末尾・`DESC` 先頭）と同じ
- `ORDER BY` が無いときのグループの出力順は、キー昇順（NULL 末尾）の決定的な順序
  （PostgreSQL は順序を保証しない。テストは `ORDER BY` 付きか集合比較で書く）

## エラー契約・テナント境界（RLS-10）

- 新しい `wire_code` は導入しない（`42601`・`42P01`・`42702`・`42804`・`22000`・`22003`・
  `54000`・`XX000` の既存分類だけ）。内部不整合は `Internal`（fail-closed）
- 件数・カーディナリティ・グループ数の判定には可視行（RLS 適用済み）だけを使うため、
  他テナントの存在が `54000` 等の成否や NULL 補完の有無・集計値・グループ数に影響しない。
  3 方向以上・自己結合でも各参照が独立に評価される
- 構文段の上限（relation 数・`ON` 条件数・`WHERE` の葉数・括弧の深さ・`IN` の要素数・
  `ORDER BY`／`GROUP BY` のキー数・集計項目数）と、実行段の確保（ハッシュ表・タプル・
  matched 配列・グループ表・出力行）は、`push`・再帰・確保の前に検査／予算計上する

## テスト方針

CI に PostgreSQL のオラクルは無い（`scripts/crossdb_bench` はベンチ専用）。期待値は
PostgreSQL と同じ意味論に従ってテスト内で手計算した値に固定している。

- `crates/engine/tests/sql28_multi_way_join.rs`: 連鎖・簡約・WHERE 木・列比較・
  `ORDER BY`・集計・上限・Describe・RLS（3 テナント × 3 relation・3 方向自己結合）
- `crates/engine/tests/sql28_inner_join.rs`: 旧「拒否」テスト 3 件を受理テストへ反転
- `crates/engine/tests/rls10_relational_paths.rs`: 形状マトリクスへ 3 方向・`OR`・
  `ORDER BY`・集計形を追加
- `sql::join` の単体テスト: 段ごとのカーディナリティ上限・グループ数上限・2 relation の
  出力順の回帰

## 対象外（申し送りのみ。Issue は起票しない）

- `CROSS`／`NATURAL` JOIN・`USING (...)`・`NOT`／`IS NULL`／`BETWEEN`／`<>`・式・
  `COUNT(DISTINCT ...)`・ウィンドウ・`DISTINCT` と JOIN の組み合わせ
- 整数と浮動小数の列同士の比較（比較クラス不一致として `42804`）
- JOIN を含む VIEW・CTE・サブクエリ・`EXPLAIN`・カーソル・`COPY`・`$n`
- JOIN 経路の性能計測（#1204）
