# `CREATE VIEW` / `DROP VIEW`（非マテリアライズド）の設計判断

Issue #909・対象ビヘイビア: TABLE-18・SQL-23（TASK-205）。関連ポインタ:
RLS-10 (b)（複数のテーブル参照を持つ読み取り経路での暗黙適用）・ERR-6
（`42P07`／`2BP01`／`42809` の新設）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 概要

保存したクエリをビューとして登録し、`FROM` に書いて読めるようにする。
**非マテリアライズド**とし、参照のたびに定義を展開する。ビュー定義は既存の
許可リストパーサーをそのまま通したもの（許可リストの部分集合）に限って保存し、
参照時は保存済みの定義を**同じパーサーで再検証**する。第 2 の SQL パーサー・
実行器は作らない。

## 受理する構文（Phase 1）

```
CREATE VIEW <name> AS SELECT <* | 列名リスト> FROM <table | view> [WHERE <単純述語> [AND ...]]
DROP VIEW <name>
```

- body の `LIMIT`・`ORDER BY ... <=>`・`HYBRID`・`USING PLAN`・集計形・式項目
  （`Projection::Items`）・UDF 呼び出し述語（`WherePredicate::PredicateCall`／
  `Expression`）・`EXPLAIN`・`CREATE OR REPLACE`・`IF NOT EXISTS`・`TEMP`／
  `MATERIALIZED`・列別名リスト・`WITH CHECK OPTION`・末尾の余剰トークンは
  いずれも構造的に受理しない（`42601`）。使える単純述語は
  `Equality`／`Prefix`／`BoolEquality`／`BoolColumn`／`Compare`（列 対
  リテラル）のみ。
- `DROP VIEW` は単一ビュー名の指定のみを受理し、`IF EXISTS`・
  `CASCADE`／`RESTRICT`・複数名指定は `42601`。
- 拡張クエリプロトコルの `$n` を含む `CREATE VIEW`／`DROP VIEW` は Parse 時点で
  拒否する（DDL はバインドパラメータを受け付けない PostgreSQL の慣習に揃える）。

## ビューを参照するクエリ（Phase 1 のスコープ）

広域取得（SQL-15。`Statement::Scan`）のみをビュー展開の対象とする。
**集計（`Statement::Aggregate`）・ベクトル検索（`Statement::Select`）・
`EXPLAIN` はビュー展開の対象外**とし、意図的にスコープを縮小した
（実装時間の制約による判断。集計はいずれ同じ `resolve_from` を再利用できる
設計だが、追加の列スコープ検査コードが必要なため Phase 1 では見送った）。
ベクトル検索・`EXPLAIN` の対象はそもそも spec 上も対象外（ビューは
ランキング段を持てない）。これらの経路は `lookup.table_exists(view_name)` が
そのまま `false` を返すため `42601` ではなく `42P01`（未定義テーブル）に
fail-closed で落ちる——ビューの存在の有無に関わらず同一の応答になるため
情報漏えいにはならないが、PostgreSQL 的な「操作対象が違う」区別
（`42809` 相当）ではない点は既知の簡略化として記録する。

## Phase 2（Issue #1192）: 本文の受理形の拡大と集計からの参照

対象ビヘイビア: TABLE-18（主対象）・SQL-13／SQL-14（集計・`GROUP BY`）・
SQL-15（広域取得）・SQL-25（`ORDER BY`／`OFFSET`／`DISTINCT`）・SQL-28（JOIN）・
RLS-10 (b)・ERR-6。spec 本文は転記しない。

### 本文の 2 系統

| 系統 | 本文 | 参照時の処理 |
| ---- | ---- | ------------ |
| 単純形（Phase 1。変更なし） | 単一 relation への射影＋単純述語（`LIMIT` なし） | 既存のインライン展開（`Resolved::View`） |
| 評価後射影形（Buffered。新設） | 広域取得に `LIMIT`／`OFFSET`／スカラー `ORDER BY` が付いたもの、集計（`GROUP BY`／`HAVING`／`ORDER BY`／`LIMIT`／`SELECT DISTINCT`）、2 テーブル JOIN（`LIMIT` 必須） | 本文を 1 文として、**参照したセッション自身**の `PolicyContext`・同一スナップショットで既存の実行経路に通し、結果へ外側の列射影と `LIMIT`／`OFFSET` の切り出しだけを行う |

- 判定は `sql::allowlist::classify_view_body`: まず単純形（`parse_view_body`）を
  試し、失敗した場合のみ評価後射影形として、通常の SELECT と同じ
  `validate_sql_tokens` を通して文の種別を確定させ、`check_buffered_body_shape`
  で許可形状（`Scan`／`Aggregate`／`Join`）に絞る。第 2 のパーサー・実行器は
  作らない。`CREATE VIEW` 時はカタログを照会しない構文専用 lookup
  （`StructuralOnlyLookup`）を渡し、判定順序（構文 `42601` → DDL 権限 `42501` →
  カタログ判定）を保つ。参照時は実カタログのラッパー（再帰ガード付き）を渡す。
- 永続化する本文は、検証済みトークン列の正規化描画（`sql::lexer::render_tokens`）。
  旧形式（Phase 1）で保存された単純形本文は無変更で再パースでき、カタログの
  フォーマット変更・移行は不要。`ParsedViewBody`／`parse_view_body`／
  `render_view_body` のシグネチャは変更していない（CTE と共有）。
- 本文に書けないもの（`42601`）: ベクトル順位付け・`HYBRID`・`USING PLAN`・
  `EXPLAIN`・CTE・集合演算・サブクエリ・ウィンドウ項目・式項目・式述語・
  UDF 述語・集計引数の式（本文が参照セッションの UDF レジストリに依存しない
  ようにする）。

### 評価後射影形ビューを参照するクエリ

外側クエリは `SELECT <* | 列名リスト> FROM <view> LIMIT n [OFFSET m]` のみ
（`Statement::BufferedView`。破壊的変更: 公開 enum への variant 追加）。外側の
`WHERE`／`ORDER BY`／集計／ウィンドウ項目・式項目は `42601`。列指定が本文の結果列に
無ければ `22000`、本文に同名の結果列が複数あって一意に決まらなければ `42702`
（PostgreSQL は作成時に拒否するが、本実装は参照時に拒否する差異）。後処理では
ソートせず、本文が固定した順序をそのまま保つ。`EXPLAIN`・cursor の `DECLARE`・
サブクエリの内側・CTE・集合演算の枝・JOIN の辺・`validate_statement` からの
参照は `42601`。明示トランザクション内では他の読み取り文と同じく、未 commit の
変更を読む経路で本文を評価する。拡張クエリの Describe は本文の列メタデータを
導出して外側の射影だけを適用する（本文は実行しない）。

### 連鎖・依存関係

- 評価後射影形は連鎖の最外段の 1 段に限る（再帰の深さの上限）。評価後射影形本文が
  別の評価後射影形ビューを指す、または単純形ビューが評価後射影形ビューを指す
  `CREATE VIEW` は `42601`（`Storage::create_view` が write txn 内で判定）。
  参照時も内側に現れた場合は `42601`（`BufferedBodyLookup` が再帰しない）。
- 本文が読む**すべての** relation の存在を作成時に確認する（`42P01`。JOIN の右辺を
  含む）。JOIN 本文の両辺はテーブルに限る（`42601`）。
- `DROP TABLE`／`DROP VIEW` の依存検査は `base_relation` だけでなく本文が読む全
  relation で判定する（JOIN 右辺の `DROP TABLE` は `2BP01`）。本文を再検証できない
  ビューは fail-closed で「依存あり」とする。`ALTER TABLE DROP COLUMN` は、評価後射影形
  ビューが対象テーブルを読んでいれば保守的に拒否し、無関係なテーブルのみを読む
  ビューは妨げない。

### 集計・`SELECT DISTINCT` からの単純形ビュー参照

`validate_select_statement` の集計・DISTINCT 分岐は `lookup.table_exists` の代わりに
`resolve_from` を使い、単純形ビューは基底テーブルへ書き換えてビュー由来の述語を
先頭にマージする（`build_aggregate_from_resolved`。非破壊）。列スコープは
`sql::view::check_aggregate_columns_within_view` で検査する（グループキー・集計
引数の式木・`GROUP BY` 列・`ORDER BY` 対象・`WHERE` の `OR`／`NOT` を再帰。ビューが
公開しない列を集計に使う filter oracle を塞ぐ）。評価後射影形ビューへの集計は
`42601`。`EXPLAIN` の集計も同じ経路を通るため、単純形ビューへの
`EXPLAIN SELECT COUNT(*) ...` を受け付けるようになった（広域取得の `EXPLAIN` が
既にビュー展開を受け付けているのと整合）。

### RLS-10 (b) の維持

`ViewDef` は Phase 2 でも `tenant_id`／`PolicyContext`／作成者の情報を持たない。
評価後射影形の本文は参照セッションの `ctx` で既存の実行経路が評価するため
（JOIN は両辺に独立して適用）、作成者の可視性は構造的に引き継がれない。外側の
後処理（`sql::view_buffered`）は `PolicyContext` を受け取らず、可視性判定に関与
しない。3 テナント対照の結合テスト（`tests/table18_view.rs`）で固定した。

## 展開方式（検証段階での書き換え）

`sql::allowlist::validate_sql_tokens` の `Statement::Scan` 分岐にある
`lookup.table_exists(&shape.table_name)` を `sql::view::resolve_from` へ
置き換えた。

- `TableLookup` トレイトへ既定実装付きの `view_definition` メソッドを追加した
  （既定 `Ok(None)`）。既存の `TableLookup` 実装（テスト用モック等）は無変更の
  ままコンパイルできる。`catalog::Storage` のみが実データを返す。
- `resolve_from` は連鎖（ビューがビューを参照する）を実テーブルへ到達する
  まで辿ったのち、最も内側から外側へ向けて各段の投影・`WHERE` がその段の
  FROM が指す関係の公開列集合に収まっているかを検証しながら畳み込む。
  `WHERE` 述語は `内側のビュー ++ 外側のビュー ++ クエリ自身` の順で合成し、
  クエリへ最終的に公開される列集合は連鎖全体を積み上げた結果（`SELECT *`
  を挟んでも内側の列制限は失われない。外側ビューが内側ビューの非公開列を
  投影・`WHERE` のいずれで参照しても連鎖のどの段でも一様に拒否する）。
- 畳み込み後の文は通常のテーブル参照に対する `ValidatedScan` と完全に同じ形に
  なり、束縛（`sql::parser`）・実行（`sql::scan`）・RLS 適用（既存の暗黙適用
  経路）はすべて無変更のまま通る。

## RLS-10 (b) の不変条件

保存するビュー定義（`catalog::ViewDef`）は `tenant_id`／`PolicyContext`／
作成者の情報を一切含めない。展開後の文は、参照したセッションの `ctx` を使う
既存の実行経路でしか評価されないため、作成者の可視性が参照者へ引き継がれる
ことは構造的に起こらない（`table18_view.rs::
view_read_applies_referencing_session_rls_not_creator_visibility` で
3 テナント対照検証済み）。

## カタログ（redb）

- 新規テーブル `VIEWS_TABLE`（キー: ビュー名、値: `直接の参照先名 + 正規化
  body SQL` の 2 フィールド blob）。
- **body は検証済み AST を正規化して描画した SQL で保存する**
  （`sql::allowlist::render_view_body`）。「描画 → 再トークン化 →
  `parse_view_body`」が元と等価な AST を復元することを round-trip テスト
  （`sql/view.rs` 内）で固定した。
- 名前空間はテーブルと共有する: `Storage::create_view` は同一 write
  トランザクションで `CATALOG_TABLE`／`VIEWS_TABLE` の両方を確認し、衝突は
  `42P07`。`Storage::create_table` にも `VIEWS_TABLE` の確認を追加した。
- 上限（実装既定値）: ビュー総数 10,000（`MAX_LIST_TABLES` と同じ）、body
  64 KiB、ネスト深さ 4（テーブル自身を深さ 0 とする）。超過は `54000`。
- すべての検査（名前衝突・参照先の存在・ネスト深さ・依存オブジェクト）は
  それぞれの操作の write トランザクション内で行う（TOCTOU 回避）。

## ネスト深さと循環

- 循環は正常な経路からは構造的に作れない: `CREATE VIEW` は参照先の存在を
  作成前に要求するため自己参照は `42P01` になり、何も永続化されない。
  他から参照されているビュー・テーブルの `DROP` は `2BP01` になるため、
  「削除して作り直す」ことで循環を組めない。`CREATE OR REPLACE VIEW`・
  `ALTER VIEW` は `42601`。
- それでもカタログ破損に備え、`resolve_from`・深さ計算のいずれも visited
  集合と上限付き反復（ループ。再帰なし）を使う。循環を検出した場合は
  `XX000`（固定文言）、上限超過は `54000`。

## 判定順序（fail-closed）

`DROP TABLE`（Issue #902）と同じ判定順序に揃えた: 構文検証（カタログ非照会）
→ `require_ddl_permission`（`42501`。テーブル・ビューの存在有無を問わず
一律拒否）→ write トランザクション内でのカタログ判定（名前衝突・参照先存在・
深さ・件数上限）→ 保存。

- `CREATE VIEW`: 名前衝突 `42P07` → 参照先不存在 `42P01` → ネスト深さ・
  件数上限 `54000` → 保存。
- `DROP VIEW`: 名前がテーブルなら `42809`、存在しなければ `42P01` →
  依存するビューがあれば `2BP01` → 削除。
- `DROP TABLE <ビュー名>`: `42809`。参照しているビューがあれば `2BP01`。
- **ビューへの書き込み**（`INSERT`／UPSERT／`UPDATE`／`DELETE`／`TRUNCATE`）は
  `42809`。`EngineCore::parse_tokens` の書き込み分岐（`validate_insert_tokens`
  等）が `UndefinedTable` を返し、かつその名前がビューとして存在する場合に
  限り `WrongObjectType` へ読み替える（`EngineCore::
  reclassify_write_to_view_error`。1 か所のみ）。副作用（台帳・世代・行）が
  一切発生しないことをテストで固定した。

## 情報漏えいの抑止

保存済み body の再検証・展開が失敗した場合（カタログ破損・非互換の将来
フォーマット変更）は `XX000` の固定文言（`invalid view definition`）にまとめ、
body のリテラル値・破損理由の詳細をクライアントへ運ばない
（`sql::view::corrupt_view_error`）。列名（`22000 unknown column`）は既存の
束縛エラーと同じ扱い（カタログ情報であり秘匿情報ではない）。

## 対象外・申し送り

- ~~集計形の body、LIMIT／`ORDER BY` 付きの body~~・~~ビューを対象にした集計~~
  は Phase 2（Issue #1192）で対応済み（上記節参照）。ビューを対象にした
  ベクトル検索（`Statement::Select`）は仕様上も対象外。
- Phase 2 の申し送り: 評価後射影形ビューに対する外側の `WHERE`／`ORDER BY`／集計／
  ウィンドウ関数（評価済みセルを評価する仕組みが必要）、評価後射影形の連鎖、本文での
  CTE・集合演算・サブクエリ・ウィンドウ関数・式項目・UDF 述語、3 テーブル以上の
  JOIN 本文と JOIN の辺のビュー、評価後射影形ビューへの cursor `DECLARE`／
  `EXPLAIN`。
- 許可名の述語呼び出し（`WherePredicate::PredicateCall`。空引数の呼び出し形
  で列参照を持たない）のビュー越し列スコープ検査。なお式項目
  （`SelectItem::Expr`）・式述語（`WherePredicate::Expression`）は
  `sql::view::check_columns_within_view` が式木を再帰的に走査して全列参照を
  検査する（PR #1048 レビュー対応で実装済み）ため対象外ではない。いずれも
  RLS 境界には影響しない——書き換え後の述語は基底テーブルへ委譲される既存の
  束縛・実行経路でそのまま評価されるため、この検査はビューが宣言した列
  スコープ契約のためのものであり安全性の境界ではない。
- 更新可能ビュー。
- NoSQL 表層の `scan`／`aggregate` op でビューを対象にすると従来どおり
  `42P01`（fail-closed のまま）。NOSQL-13 の DDL op はスコープ外。
- FOREIGN KEY 由来の `2BP01`（#907）。
- `SELECT *` の展開は参照時点で動的に行う（作成時点の列集合で固定する
  PostgreSQL 互換の方式は採らない）。
- 参照時の展開は Parse 時点で確定する（拡張クエリで Parse から Execute までの
  間にビューが削除されても、Parse 時点の定義で実行される。ビュー自体は権限を
  運ばないため権限昇格にはならない）。
- PR #1041（明示トランザクション。SQL-31）とのトランザクション内 DDL 拒否
  ガードの統合はマージ順に依存する申し送り事項。
