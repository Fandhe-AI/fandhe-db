# 述語つき `UPDATE ... WHERE` の許可リスト・束縛

Issue #869・対象ビヘイビア: SQL-19（TASK-192）。関連ポインタ: SQL-17（TASK-191。
単一行・id 指定形 `UPDATE`）・EXT-3（TASK-147。前方一致述語）・SQL-9（TASK-79。式述語）・
RLS-7・RLS-10（RLS 暗黙適用）・RECOVER-1（`operation_id` 必須化）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

> **実行結線は実装済み**（Issue #871）。詳細は `docs/design/predicate-dml-exec.md`
> 参照。以下「実行結線は別 Issue（#871）の担当」等の記述は本 Issue（#869）着手時点
> の申し送りとして残置する。

## スコープ

本 Issue は述語つき `UPDATE ... WHERE` の **許可リスト構造検証・束縛** のみを
対象とする。実行結線（候補行の列挙・可視集合との突き合わせ・一括適用・原子性・
`operation_id` 内容照合）は別 Issue（#871。実装済み）の担当。述語つき
`DELETE ... WHERE` は別 Issue（#870）が本 Issue の型・振り分け規則をそのまま
再利用する想定。

## 構文

```
UPDATE <table> SET <col> = <lit>[, <col> = <lit>]* WHERE <where_form>
  USING OPERATION_ID '<id>' [;]
```

`<where_form>` は次の 2 形のいずれかへ決定的に振り分けられる:

- **単一行・id 指定形**（SQL-17）: `id = <n>` で、直後が文末・`;`・`USING`（文脈的
  キーワード）のいずれか。
- **述語形**（SQL-19、本 Issue）: 上記に一致しないすべて。`SELECT`・集計
  `SELECT`・広域取得 `SELECT` と同一の `WHERE` 述語文法（等価・前方一致・
  `visible()`・式比較の `AND` 結合。`sql::allowlist::WherePredicate`）を再利用する。

`WHERE` 句そのものの省略は SQL-36（Issue #1743）で述語形（空の述語列）として受理する。

## 追加型 API（破壊的変更を避ける）

`bind_insert`（単一行契約）／`bind_insert_form`（`BoundInsertForm` へディスパッチ）と
同じ「既存 API は不変のまま、新しい形を追加型として提供する」設計を踏襲した。

| 層 | 既存（無変更） | 新設（本 Issue） |
| --- | --- | --- |
| allowlist | `ValidatedUpdate`・`validate_update`・`validate_update_tokens`（述語形は引き続き `42601`） | `ValidatedPredicateUpdate`・`ValidatedUpdateForm { Single, Predicate }`・`validate_update_form`・`validate_update_form_tokens` |
| parser | `BoundUpdate`・`bind_update` | `BoundPredicateUpdate`・`BoundUpdateForm { Single, Predicate }`・`bind_update_form`・`MAX_DML_AFFECTED_ROWS`・`check_dml_affected_rows` |

`sql::allowlist::Parser::parse_update` 自体は内部表現 `UpdateWhereForm { Id, Predicates }`
を返すよう書き換えたが、`validate_update`（既存公開 API）は `Predicates` を
`42601` で拒否することで挙動・シグネチャとも完全に不変のまま維持する。

## 振り分け規則の決定性

`Parser::parse_update_where`（`WHERE` キーワード消費済みの位置から呼ばれる）は
直後の 3 トークンを覗き見て判定する:

1. `Ident("id")`・`Punct('=')`・`Token::Number` の 3 トークンに一致し、
2. かつ 4 番目のトークンが「文末（`None`）」「`Punct(';')`」「文脈的キーワード
   `USING`」のいずれかである場合 **のみ** id 指定形。
3. それ以外は位置を巻き戻し、`Parser::parse_where`（`SELECT` と共有する既存実装）で
   述語形として解析する。

この結果、`WHERE id = 5 AND lang = 'ja'`・`WHERE id = 'x'`（数値以外の id 比較）・
`WHERE lang = 'ja'` はいずれも述語形へ流れる。特に `id = 5 AND ...` は 3 トークン
一致自体はするが 4 番目の条件（`AND` は該当しない）に外れるため述語形になる。

### エラーコードの差

同じ入力 `WHERE id = 'x'` でも、到達するエントリポイントによって `wire_code` が
異なる（意図した仕様。どちらも fail-closed だが判定層が異なる）:

- `validate_update`（既存・id 指定形専用）経由: 述語形 `Predicates` へ振り分けられ
  `42601`（許可リスト外の形として拒否）。
- `validate_update_form` → `bind_update_form`（本 Issue の新エントリポイント）経由:
  述語形として受理されたうえで `declarative_filter::bind_all` が `id` を
  未知列として `22000`（`SELECT` の `WHERE id = 'x'` と同一の失敗点・同一コード）。

## `WHERE` の意味論的束縛: 二重実装の禁止

`bind_update_form` の `Predicate` 腕は SET 束縛（`bind_set_assignments`。
`bind_update` から抽出し `Single`／`Predicate` 双方が共有）の後、`WHERE` 述語列を
`sql::parser::bind_where_predicates`（`SELECT`・集計 `SELECT`・広域取得 `SELECT`
（`bind_scan`）と完全に同一の実装。TASK-166・SQL-13 で切り出し済み）へそのまま
渡す。結合テスト
`crates/engine/tests/sql_update_predicate_bind.rs::predicate_update_where_binds_to_the_same_representation_as_select_where`
が、同一 `WHERE lang = 'ja' AND path LIKE 'src/%' AND id > 10` を `SELECT` 経由
（`bind_scan`）と述語つき `UPDATE` 経由（`bind_update_form`）の両方で束縛し
`metadata_filters()`／`expr_filters()` が完全一致することを機械的に固定する。

## `visible()` 単独・恒等述語の扱い

`WHERE visible()` のみ（`metadata_filters`・`expr_filters` がともに空）は構造上
`WHERE` 句を持つが実質的に全行更新であるため、`bind_update_form` の `Predicate`
腕が束縛時に `42601` で拒否する（`WHERE` 自体の省略は SQL-36・Issue #1743 で受理する。
空の述語列は束縛段ガードの対象外。`visible()` のみという形は引き続きここで拒否する）。
`visible()` を他述語と `AND` 併用する形（例: `WHERE visible() AND lang = 'ja'`）は
`metadata_filters`／`expr_filters` のいずれかが非空になるため受理される
（RLS は述語の有無に関わらず暗黙適用のため、`visible()` の有無自体は実行結線の
可視性判定を変えない。RLS-7）。

`1 = 1` のような恒真式は静的に一般除外できないため束縛では受理する。実行時の
防御は「`WHERE` 明示要求」と次節の影響行数上限が担う。

## 影響行数上限（`54000`）

束縛段階では対象行数が確定しない（RLS 可視集合・SCALAR フィルタの適用は実行時）
ため、本 Issue では上限値と判定関数のみを提供する:

```rust
pub const MAX_DML_AFFECTED_ROWS: usize = 1_000;
pub fn check_dml_affected_rows(count: usize) -> Result<(), SqlSurfaceError>; // 超過は 54000
```

既定値 1,000 は本リポの実装既定値（spec 由来の数値ではない）。`SQL-16`・
`TASK-190` の `MAX_INSERT_ROWS_PER_STATEMENT`（1 文あたり 1,000 行）と桁を揃えた。
実行結線（Issue #871・述語つき `UPDATE`。Issue #870・述語つき `DELETE`）が
**変更を開始する前**に呼ぶ契約（実行前・副作用ゼロの段階で拒否する fail-closed
設計）。`check_dml_affected_rows` は超過の有無だけを判定するため、呼び出し元は
対象行集合を全件列挙する必要はなく、広い述語（例: 全行に一致する `WHERE`）による
無制限列挙を避けるため `MAX_DML_AFFECTED_ROWS + 1` 件に達した時点で列挙を打ち切り
その件数を渡してよい（早期終了。未検証入力によるリソース増幅の回避）。

## `BoundPredicateUpdate`: `expr_filter_programs` を保持しない

`BoundScan`／`BoundStatement`（Issue #353・ステップ列コンパイル化）が保持する
`expr_filter_programs`（`ExprProgram` へのコンパイル済み実行形）を
`BoundPredicateUpdate` は本 Issue の時点では持たない。理由は 2 つ:

1. `sql::expr_program` が `pub(crate) mod` のためクレート外に型を出せない
   （`BoundScan::expr_filter_programs` と同じ制約）。
2. 束縛時点の `BoundPredicateUpdate` 自体は行ループを持たないため、コンパイル
   結果を保持しても本 Issue の範囲に読み手が存在しない（`unsafe` を増やさず
   dead code を作らない設計判断）。

実行結線（#871）が候補行確定後に `ExprProgram::compile` を都度呼ぶ契約とし、
その旨をドキュメンテーションコメントへ明記した。

## 構文上の余剰トークンの扱い

`HINT ORDER(...)`・`USING MODE '...'`・`ORDER BY ...`・`LIMIT n`・複数テーブル
指定・`SET` 右辺のサブクエリはいずれも `parse_update` の文法に存在しないため（`WHERE` 内のサブクエリは Issue #1750・SQL-53 で構文段が受理し、実行は #1751 の解決経路へ渡る）、
`expect_end_of_statement` が余剰トークンとして構造的に `42601` へ落とす（述語形
でも id 指定形でも同じ経路。個別の拒否コードを持たない）。結合テストで両形状に
対して固定している。

`RETURNING <投影>`（Issue #873・SQL-21）は `parse_update` の文法上は
`USING OPERATION_ID` 句の直前に受理できるが、`UPDATE` の実行結線（#865）が
未着手のため `validate_update_tokens`／`validate_update_form_tokens` が
`WHERE` 形状の判定より前に単一のチョークポイントで一律 `42601` 拒否する
（`sql::allowlist` のドキュメントコメント参照。余剰トークンによる拒否とは
判定経路が異なるが、応答の `wire_code`（`42601`）は不変）。詳細は
`docs/design/sql-returning.md` 参照。

## RLS・テナント境界

`SET` 対象列に `id`（疑似列）・`tenant_id`／`visibility`（RLS 内部列）を指定する
経路は `bind_set_assignments`（`bind_update`／`bind_update_form` の共有実装）が
`42601` で拒否する。束縛結果（`BoundPredicateUpdate`）にはテナント・可視性を
表すフィールドが一切存在しない。RLS 可視集合の適用・テナント境界の強制は
サーバー側の `PolicyContext` から導出される（実行結線 #871 の担当。述語の有無で
緩まない。RLS-7・RLS-10）。

## NoSQL 表層・実行結線への申し送り

- **#871**（述語つき `UPDATE` 実行結線）: `BoundPredicateUpdate::metadata_filters()`／
  `expr_filters()` を RLS 可視行の SCALAR 段（`matches_all` ＋ `ExprProgram`
  コンパイル）に適用して候補集合を確定し、`check_dml_affected_rows(count)` を
  変更開始前に呼ぶ。`operation_id` 内容照合ハッシュの入力仕様・記録順序の
  提案は `docs/design/multi-row-dml-operation-id.md`（Issue #868・ステータス
  Proposed）を参照。オーナー承認・spec 側 RECOVER-11 の確定前は実装の
  確定契約ではなく作業前提の案に留まる。
- **#870**（述語つき `DELETE`）: `UpdateWhereForm` の振り分け規則・`visible()` のみ
  拒否・`MAX_DML_AFFECTED_ROWS` をそのまま再利用する想定。
- **#876**（NoSQL `update` op）: `BoundPredicateUpdate::new` は `pub(crate)`
  （codex-review 指摘・PR #985 是正）。生フィールドをそのまま受け取れる公開
  constructor は `bind_set_assignments`（`id`／`tenant_id`／`visibility` 列への
  SET 拒否）・`metadata_filters`／`expr_filters` 両方空＝無条件更新の拒否・
  `LedgerMode::Ledgered` 下の `operation_id` 必須化（`validate_update` が
  `ValidatedUpdateForm` 構築時点で強制）を丸ごと迂回できてしまうため、
  #876 は `bind_update_form` と同じ検証を内部で必ず通したうえで
  `BoundPredicateUpdate` を返す**別の**公開 API を新設すること（`new` 自体は
  pub 化しない）。
- **#865**（`core.rs` の `UPDATE` ディスパッチ）: `validate_update`（単一行）を
  引き続き使ってよい。述語形へ拡張する際は `validate_update_form` の `Single`
  腕が既存と同一の `ValidatedUpdate` を返すことを利用できる。
- 上限既定値（1,000）の確定・設定値化はオーナー判断事項として申し送る。
