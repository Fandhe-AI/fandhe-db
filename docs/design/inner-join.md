# INNER JOIN（2 テーブル等価結合。Issue #925・SQL-28・RLS-10・TASK-212）

## ステータス

Accepted・実装済み。

## ポインタ

- spec: `docs/spec/05-tasks.md` TASK-212・`docs/spec/04-behavior/sql-surface.md` SQL-28
  （検討中）・`docs/spec/04-behavior/rls.md` RLS-10・`docs/spec/04-behavior/error-format.md`
  ERR-6（`42702`・`42804`）
- 前提基盤: `docs/design/multi-relation-plan-foundation.md`（Issue #924・`sql::relation`）
- 関連 Issue: #926（`docs/design/outer-join.md`・LEFT/RIGHT/FULL OUTER JOIN・実装済み）・
  #931（JOIN 経路の RLS 境界の網羅検証・対象外）

spec 本文はここへ転記しない（`.claude/rules/spec-confidentiality.md`）。以下は本リポの
実装既定値・設計判断の記録。

## 背景・目的

`sql::allowlist` は「1 文 = 1 テーブル」前提で `JOIN`・複数 FROM を `42601` で拒否してきた
（`rejects_join`・`rejects_multiple_from_tables`）。本 Issue では、2 テーブルの内部結合を
SQL 表層の広域取得形（SQL-15 系の `SELECT ... LIMIT n [OFFSET m]`）として開放する。RLS・
fail-closed・`wire_code` 契約はそのまま維持する。

## 受理文法（実装既定値）

```text
join_select := SELECT join_list FROM rel [INNER] JOIN rel ON cond { AND cond }
               [WHERE conj { AND conj }] LIMIT <n> [OFFSET <m>] [;]
rel         := <table_ident> [[AS] <alias_ident>]
join_list   := '*' | colref { ',' colref }
colref      := <ident> | <qualifier>.<ident>
cond        := colref '=' colref
conj        := colref <op> <literal>        -- op: = < <= > >= / LIKE '<pattern>'
```

relation は常にちょうど 2 個。以下はいずれも `42601`（fail-closed）:

- （`CROSS JOIN` は Issue #1831、`JOIN ... USING (...)` は Issue #1826、`NATURAL JOIN` は Issue #1827 で受理対象に
  なった。`docs/design/join-using.md` 参照。`LEFT`／`RIGHT`／`FULL [OUTER]` は
  Issue #926 で受理対象になった。`docs/design/outer-join.md` 参照）
- 非等価の `ON`（#1835 系で解除予定。[nested-loop-join.md](nested-loop-join.md)）、左右が同じ relation を指す `ON`
- `ORDER BY`・`<=>`・`HYBRID`・`USING PLAN`・`USING MODE`・`HINT ORDER` との併用
- 集計・`GROUP BY`・`DISTINCT`・ウィンドウ関数・式項目・`t.*`
- WHERE の `OR`・括弧・`BETWEEN`・`IN`・`NOT`・式・列同士の比較
- FROM にビューを指定した場合
- `$n` パラメータを含む JOIN 文（後述「対象外」）

別名は `catalog::validate_identifier` で検証し、予約文脈語（`JOIN INNER LEFT RIGHT FULL
OUTER CROSS NATURAL ON USING WHERE LIMIT OFFSET AS`）は大小文字を無視して拒否する
（既存の `FROM t USING PLAN(...)` の `USING` を別名と誤認しない）。公開名（別名があれば
別名、なければテーブル名）が重複したら `42601`（`42712` は採用しない。基盤 ADR の判断を
踏襲）。自己結合は別名が異なれば受け付ける。

## 構文検出（許可リスト）

JOIN の判定はトークン列の先読み（`sql::allowlist::looks_like_join`）で行う。括弧深さ 0 の
最初の `FROM` の直後が `<ident> [[AS] <alias>] (JOIN|INNER JOIN|LEFT|RIGHT|FULL|CROSS|
NATURAL|`,`)` の並びのときだけ真になり、`validate_sql_tokens_impl` の集計・`DISTINCT`・
集合演算の判定より前に呼んで `parse_join_statement` へ分岐する。`CROSS`・`NATURAL`（Issue #1827 で受理対象）・
単独の `OUTER JOIN`（対象外事項）も検出対象に含め、`parse_join_statement` 側で `42601`
に分類させる（構文段の判定順序を `looks_like_join` 側の複雑な除外リストに分散させない）。
`LEFT`／`RIGHT`／`FULL [OUTER]` の受理は Issue #926（`docs/design/outer-join.md`）参照。

`ON`・`WHERE` の列参照は既存の `sql::parser::Parser::parse_where` を token 置換で再利用
せず、`sql::allowlist::ValidatedJoin`（`Vec<(ColumnRef, ColumnRef)>` の `on`・
`Vec<JoinWherePredicate>` の `where_conjuncts`）という専用の構造体・専用の小さな再帰下降
パーサー（`parse_join_on_conditions`・`parse_join_where_conjuncts`）で直接組み立てる設計
とした。理由: 既存の `parse_where` は非修飾 `column: String` を前提にした構造
（`WherePredicate`）を返すため、修飾子つき列参照（`Token::QualifiedIdent`）を通すには
トークン置換と位置対応の復元が必要になり、実装・レビューの複雑度に見合わない。JOIN の
WHERE は「AND のみ・列 vs リテラルの比較のみ」という制限された文法（本 Issue の対象外
事項）のため、専用の小さなパーサーの方が構造的に見通しが良い。束縛時
（`sql::join::build_plan`）に `BindingScope::resolve` で相手側の relation を確定し、
非修飾 `WherePredicate` へ変換してその側の `ValidatedScan::where_predicates` へ
プッシュダウンする。保存側（外部結合の NULL 補完対象にならない側）へのプッシュダウンは
INNER・外部結合いずれでも意味論的に正しく、そのまま再利用する。欠損側（外部結合の
NULL 補完対象になりうる側）は追加の簡約規則が必要（Issue #926・`docs/design/
outer-join.md`「WHERE の意味論」節参照）。

## 束縛（`sql::join::build_plan`）

行を走査する前に次の検証をすべて確定させる:

1. `BindingScope::new` で参照数・公開名の重複を検証する
2. `ON` の各 colref を `BindingScope::resolve` で解決する（`42P01`・`42702`・`22000`）。
   両辺が同じ relation を指していたら `42601`
3. 結合キーの型クラス（`JoinKeyClass`）を判定する。許可するのは疑似列 `id` と
   `TEXT`／`INTEGER`／`BIGINT`／`BOOLEAN`／`DATE`／`TIMESTAMP`／`UUID`／`BYTEA`／`ENUM`。
   整数クラス（`id`・`INTEGER`・`BIGINT`）は相互に結合できる（外部キー設計の
   `a.id = b.fk` 形を成立させるため）。それ以外の型は完全一致が必要（`ENUM` は型名まで
   一致）。`VECTOR`・`REAL`／`DOUBLE`／`NUMERIC`／`JSON`／`JSONB`／`ARRAY` は `42601`。
   クラスが違う・`ENUM` の型名が違う場合は `42804`
4. 投影を解決する。`*` は左の（`id`＋全列）→右の（`id`＋全列）の順。列リストは
   `BindingScope::resolve` で解決する
5. `WHERE` の conjunct を relation へ割り当て、プッシュダウンする
6. 側ごとに合成 `ValidatedScan`（`projection: Columns(結合キー∪その側の投影列、
   重複除去)`・プッシュダウンされた `where_predicates`・`limit: 1`・`offset: 0`）を組み立て、
   `bind_scan_with_dummy_flags(.., &[])` で束縛する（WHERE の束縛・検証を単一テーブル
   経路と完全に共有する）
7. `LIMIT`／`OFFSET` の範囲を検証する（`22000`）

## 実行（`sql::join::execute`）

- 単一スナップショット: `core.rs::EngineCore::read_txn_with_schemas` で `read_txn` を 1 つ
  開き、両辺のスキーマをまとめて解決する
- **両辺の評価は既存の広域取得経路を通す**: 束縛済み `BoundScan::limit` を
  `MAX_JOIN_INPUT_ROWS + 1` に差し替えて `sql::scan::execute_scan_with_budget` を呼び、
  行数が上限を超えたら `54000`（`sql::set_op::eval_branch` と同じ形）。RLS の順序
  （ヘッダ判定 → 可視性 → tenant 整合 → デコード → WHERE → 再判定）と fail-closed の
  エラー写像を、2 つ目の実行器なしに引き継ぐ。両側とも、呼び出したセッションの
  `PolicyContext` で独立に評価する
- **共有バイト予算**: `sql::set_op::SetOpBudget` と同じ形の `JoinBudget`（上限
  `arena::MAX_ARENA_TOTAL_BYTES`）を文全体で 1 つだけ持つ。側スキャン（残り予算を渡す）・
  ハッシュ表のキー・出力行の生成のすべてから消費する
- **ハッシュ結合**: 行数の少ない側をビルド側にする（同数なら右）。キーは正準バイト列
  （型タグ相当をクラスで統一し、長さ接頭辞 `u32` BE＋ペイロード）で、
  `HashMap<Vec<u8>, Vec<u32>>`（容量は側の行数上限で有界）。キーのどれかが `NULL` の行は
  登録も照合もしない
- **カーディナリティの先行判定**: 出力を実体化する前に、一致ペア数を数えながら
  `MAX_JOIN_OUTPUT_ROWS` を超えたら `54000`。この判定は `LIMIT`／`OFFSET` の有無に依存
  させない（`sql::set_op` の PR #1105 の教訓に沿った単純な規則）。判定に使うのは可視行
  だけなので、他テナントの行数は結果に影響しない
- **順序（決定的）**: `(左側走査位置, 右側走査位置)` の安定ソート（`sort_by_key`。
  `sort_unstable*` は使わない。`scripts/check_sort_determinism.sh` ゲート対応）で固定する。
  どちらの側をビルドに選んでも同じ結果順になる
- 次に `OFFSET`／`LIMIT` を適用する。`ResultRow.id` は左行の `id`、`score` は `0.0`
  （既存の広域取得と同じ）

上限（実装既定値。`sql::join` に `pub(crate) const` で公開）:

- `MAX_JOIN_INPUT_ROWS = 100_000`（側ごとの、可視かつ WHERE に一致する行数）
- `MAX_JOIN_OUTPUT_ROWS = 100_000`（`LIMIT` 適用前の結合カーディナリティ）

テストで上限を差し替えられるよう、`pub(crate) struct JoinLimits` を受け取る
`execute_with_limits` を用意する（`sql::scan::execute_scan_with_budget`・
`sql::set_op::execute_with_budget` と同じ設計）。

### 結合キーのエンコーダを `sql::constraint`／`sql::set_op` と共有しない理由

既存の `constraint::push_canonical_component`（`ScalarRef` が対象で、`INTEGER` と
`BIGINT` を別タグにする）や `set_op::row_key`（行全体が対象で、クラスを統一しない）は、
それぞれ一意キー制約・集合演算の行の同値判定という別目的のために設計されており、
JOIN の「整数クラスを跨いで結合できる」という要件（`id`・`INTEGER`・`BIGINT` を
同一クラスとして扱う）と噛み合わない。目的が異なる独立モジュールとして
`sql::join` 内に専用のエンコーダを持つ（バイト予算計上のための行サイズ推定関数
`result_bytes` も同様に `sql::set_op` の private 実装を共有せず独立実装とした——
両モジュールが将来独立に変更されても互いに壊れない設計判断）。

## エラーの優先順位（決定的）

`42601`（構文）→ `42P01`（テーブルの存在）→ 束縛（`42P01` 修飾子・`42702`・`22000`・
`42601` キー型・`42804`）→ `22000`（LIMIT/OFFSET の範囲）→ 実行時の `54000`
（側の行数・カーディナリティ・バイト予算）。Describe（`sql::join::describe_columns`）も、
走査前の検証は Execute と同じものをすべて行う。

## 明示トランザクション

`sql::relation::ensure_relations_not_written` で両辺が書き込み済みでないことを確認して
から実行する（`core.rs::EngineCore::execute_in_active_txn` の `Statement::Join` アーム）。
書き込み済みのテーブルがあれば `0A000`。

## 基盤の申し送り項目の扱い

| 申し送り項目（`docs/design/multi-relation-plan-foundation.md`） | 本 Issue での扱い |
| --- | --- |
| 許可リストの JOIN・修飾列・別名の受理 | 実施 |
| `BindingScope`・`TableRef`・`ColumnRef` の利用 | 実施 |
| `ensure_relations_not_written` の結線 | 実施 |
| 中間結果の行数上限 | 実施（`MAX_JOIN_INPUT_ROWS`・`MAX_JOIN_OUTPUT_ROWS`） |
| `42712` の採否 | 採用しない（`42601` を維持） |
| `MAX_TABLE_REFS = 8` | 据え置く（JOIN は 2 relation に限定） |
| `RelationSnapshot`／`RelationSnapshotCache` の `EngineCore` 結線 | **本 Issue でも結線しない**。理由: 結合には列の値が必要だが、スナップショットは `(tenant_id, id)` しか持たないため、使うと値を取るための 2 回目の点照会走査が必要になる。広域取得経路なら RLS・WHERE・値の取得が 1 パスで済む。キャッシュによる最適化は後続の検討事項とする。基盤は `pub` API として残す |

## セキュリティ考慮（OWASP Top 10）

- **A01 アクセス制御**: 両側とも、サーバーが導出した `PolicyContext` を使って既存の scan
  経路で独立に評価する。不可視行はデコードもカウントもしない。カーディナリティ・行数
  上限・OFFSET の判定は可視行だけで行い、他テナントの存在や件数が観測できないように
  する。明示トランザクションでは書き込み済みのテーブルを `0A000` で拒否する
- **A03 インジェクション**: 識別子は `TableRef`・`ColumnRef` という型で扱い、SQL 文字列を
  組み立てない。別名は `validate_identifier` で検証し、予約語を拒否する
- **A04 不安全な設計（DoS）**: 確保前に上限を検証する（relation 数 2、ON 条件数
  `MAX_JOIN_CONDITIONS`、側の行数、カーディナリティ、文全体で 1 つの共有バイト予算、
  キーの長さ）
- **A05 設定ミス・情報漏えい**: エラー文言には、入力した列名・修飾子だけを含める
- **A08 整合性**: 1 つの `read_txn`（同一スナップショット）で両側を評価する

## 対象外（Issue は起票しない。申し送りのみ）

- （`CROSS JOIN` は Issue #1831、`JOIN ... USING (...)` は Issue #1826、`NATURAL JOIN` は Issue #1827 で受理対象に
  なった。`docs/design/join-using.md` 参照。`LEFT`/`RIGHT`/`FULL [OUTER]` は
  Issue #926 で受理対象になった。`docs/design/outer-join.md` 参照）
- ウィンドウ・`DISTINCT` と JOIN の組み合わせ
- WHERE の `BETWEEN`・サブクエリ

（3 テーブル以上の連鎖・集計・`GROUP BY`・スカラー `ORDER BY`・WHERE の `OR`／`IN`／列同士の
比較は Issue #1190 で受理対象になった。`docs/design/multi-way-join.md` 参照。本書の `where_conjuncts`・
`parse_join_where_conjuncts` は同 Issue で `where_clause`〔論理木〕・`parse_join_where` に置き換わった）

- JOIN を含む VIEW・CTE・集合演算の枝・サブクエリ・`EXPLAIN`・カーソル・`COPY`
- `$n` パラメータ付きの JOIN（`where_equality_literal_is_param` は `Ident '='` の形しか
  数えず、2 つの側へ分かれるプッシュダウンでのフラグ対応付けが保証できないため）
- `REAL`／`DOUBLE`／`NUMERIC`／`JSON`／`ARRAY` の結合キー
- `RelationSnapshotCache` による結合入力のキャッシュ
- JOIN 経路の RLS 境界の網羅検証（#931）
- 性能基準（各 1 万行）の専有環境での正式計測

## Issue #1829 追記（ON 条件の分解。SQL-60 ポインタ）

- ON の文法は `on := leaf { AND leaf }` になった。葉は WHERE と共通の葉（等価・`LIKE`・`IN`・
  比較・列同士の比較・bool 列）で、`OR`・括弧は従来どおり `42601`。上限は 8 個（`54000`）。
- 束縛段 `sql::join::on_split` が各葉を 4 種に分類する: 等価結合キー（一方が既存側、他方が新規
  relation の列の `=`）・左のみ・右のみ・両側を跨ぐ非キー（混在）。列参照の解決エラー
  （`42703`・`42P01` 等）は分類より前に確定する。
- 現状の契約: 等価キーが 1 つも無い ON は `42601`、キーに加えて残余述語（左のみ・右のみ・混在）
  がある ON は `0A000`（fail-closed）。残余の評価は Issue #1830 で行い、そこで拒否を解除する。
  外部結合の ON 残余は WHERE の簡約規則へ流さない。
- Issue #1830 で残余付き ON の `0A000` 拒否を解除した（一致条件として結合段で評価する。意味論は
  [outer-join.md](outer-join.md) の「Issue #1830 追記」）。キー無し ON の `42601` は、#1839 でネステッドループ結合により解除した（設計は [nested-loop-join.md](nested-loop-join.md)）。

## Issue #1831 追記（CROSS JOIN。SQL-60 ポインタ）

- 文法: `rel CROSS JOIN rel`（別名可）。`ON`／`USING (...)` の後置・`NATURAL CROSS`・
  `CROSS OUTER/INNER/LEFT JOIN`・`CROSS` の後に `JOIN` が無い形は `42601`。
- 表現: `JoinCondition::Cross`（kind は常に `Inner`）。束縛段は右 relation の全列を可視列へ足すだけの
  空キー段になり、NATURAL の共通列 0 個と同じ実行経路（1 バケットのハッシュ表による直積列挙）を通る。
  `SELECT *` の列順は左の全列 → 右の全列。
- 上限: 空キーかつ ON 残余なしの段は、ハッシュ表・ペア列の確保前に `左 × 右`（`checked_mul`）を
  出力行数上限と照合して `54000`（オーバーフローも同じ）。最終段では暗黙の結果行数上限（#1797）も事前判定する。
  判定に使うのは可視行（RLS・WHERE 適用後）の件数だけで、`LIMIT`／`OFFSET` の値や他テナントの行数には依存しない。
  従来の逐次判定は後段の安全網として残す。
- 側を跨ぐ WHERE の等価条件は、#1834 で結合キーへ昇格する（後述）。昇格できない形（非等価・`OR` 内部等）は
  結合後の残余として評価するため、上限判定は直積の件数に対して行われる（fail-closed）。
- 対象外: DML の `FROM`／`USING` での CROSS、`CROSS JOIN LATERAL`・派生テーブル。

## Issue #1833 追記（カンマ結合の構文。SQL-60 ポインタ）

- 文法: `rel { ',' rel }`（別名可、2..=8 表）。明示 JOIN 連鎖の後のカンマ（`a JOIN b ON ..., c`）と、
  カンマの後の `CROSS JOIN`（`a, b CROSS JOIN c`）も受理する。いずれも left-deep と行集合・`SELECT *` の列順が一致する。
- 表現: カンマ 1 個を `JoinStep { kind: Inner, condition: Cross }` 1 段にする（#1831 の `CROSS JOIN` と同一）。
  束縛・実行・上限判定（`左 × 右` の確保前判定）・RLS の独立適用は CROSS JOIN の経路をそのまま通る。
- 混在規則: PG では JOIN がカンマより強く結合する。カンマの後に CROSS 以外の明示 JOIN
  （`INNER`／`LEFT`／`RIGHT`／`FULL`／`NATURAL`／`USING`）が続く形は、left-deep の束縛では NULL 補完の範囲・
  ON から見える表・共通列の導出範囲が PG とずれるため `0A000` で拒否する（fail-closed）。
- エラー: 末尾カンマ・カンマ後の派生テーブル／関数・`ON`／`USING` の後置・公開名の重複は `42601`（セッション最上位の SELECT では、FROM／JOIN／カンマ直後の派生テーブル `(SELECT ...)` は構文受理後に固定 `0A000`。[cte.md](cte.md) の「Issue #1865 追記」節）、
  表参照数の超過は確保前に `54000`、ビュー参照は `42601`、存在しない表は `42P01`（構文段が先）。
- 側を跨ぐ WHERE の等価条件は #1834 で結合キーへ昇格する。昇格できない形は結合後の残余として評価し、
  上限判定は直積の件数に対して行う。
- 対象外: カンマの後の明示 JOIN の PG 優先順位どおりの束縛・実行（フォローアップが必要）、
  DML の `FROM`／`USING`、LATERAL・派生テーブル（WHERE の等価条件の昇格は #1834 で対応済み）。

## Issue #1834 追記（WHERE 等価条件の結合キー昇格。SQL-60 ポインタ）

- 目的: `FROM a, b WHERE a.k = b.k` のような側を跨ぐ等価条件を、両列が初めて揃う結合段の結合キー
  （ON 相当）へ移す。空キー段（CROSS／カンマ）はハッシュ結合になり、出力行数上限は直積でなく一致件数で判定される。
- 昇格条件（すべて満たすときだけ。満たさなければ従来の残余・プッシュダウン経路）:
  1. 最上位 AND 直下の葉で、`列 = 列`、2 列が異なる relation `r1 < r2` へ解決される（`OR` 内部・同一 relation・非等価は対象外）
  2. 両列の比較クラスが一致し、両列とも結合キー化可能（REAL／DOUBLE／NUMERIC は残余評価のまま。
     型不一致は残余経路が従来どおり `42804` を返す。昇格は新たなエラーを作らない）
  3. 対象段 `r2 - 1` が WHERE の strict 簡約後に INNER（`preserve_acc`・`preserve_new` とも偽）。
     等価葉は `r1`・`r2` で null-rejecting のため通常は必ず満たし、満たさなければ残余へフォールバックする
  4. 等価の意味論がハッシュ照合と一致する（整数は i128 正規化、TEXT／BYTEA はバイト完全一致、NULL は不一致）
- 同じ段へ複数昇格すれば複合キーになる。昇格した conjunct は残余から外れるため、残余が空になれば
  最終段の早期停止（#1797）が効く場合がある（結果は不変）。
- 実装: 判定は `sql/join/where_promote.rs`、段フラグ検査と `StepKey` 追加は `plan::build_plan` の WHERE ループ。
- 対象外: `UPDATE ... FROM`／`DELETE ... USING`、非等価・式を含む等価の昇格、推移閉包による等価の導出、
  REAL／DOUBLE／NUMERIC の結合キー化、ON と WHERE で重複したキーの除去、結合順序の並べ替え。
