# ADR: 複数行変更の operation_id と内容照合ハッシュの対応づけ（Issue #868）

- ステータス: Accepted（2026-09-23 オーナー承認。「判断記録」節参照。spec 側 RECOVER-11 も
  2026-09-23 に確定）
- 対応 Issue: #868（親 #861・ルート #860。後続 #871「述語つき UPDATE 実行結線」・
  #876「NoSQL `update`／`delete` op」。関連 #865「単一行 UPDATE 実行結線」・
  #872「UPSERT」・#942「明示トランザクション内の台帳（RECOVER-12）」）
- 関連ポインタ: `docs/spec/04-behavior/recovery.md`（RECOVER-11・RECOVER-10・
  RECOVER-3・RECOVER-7）・`docs/spec/04-behavior/errors.md`（ERR-2・ERR-3）・
  `docs/spec/04-behavior/sql.md`（SQL-16〜SQL-22）・`docs/spec/04-behavior/nosql.md`
  （NOSQL-12）・`docs/spec/05-tasks.md`（TASK-101・TASK-191・TASK-192）。spec 本文は
  転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/recovery/content_hash.rs`（`OpTag`・
  `HashInputBuilder`・`for_typed_insert_batch`・`for_truncate`・
  `for_typed_insert`・`push_named_scalar_columns`・`push_value`・`push_vector`）・
  `crates/engine/src/recovery/ledger.rs`（`record_in_txn`・`LedgerRecordError`）・
  `crates/engine/src/sql/allowlist.rs`（`ValidatedPredicateUpdate`・
  `ValidatedPredicateDelete`・`WherePredicate`・`InsertLiteral`）・
  `crates/engine/src/sql/parser.rs`（`bind_update_form`・`bind_predicate_delete`・
  `check_dml_affected_rows`・`MAX_DML_AFFECTED_ROWS`・`check_affected_row_count`・
  `DEFAULT_MAX_DML_AFFECTED_ROWS`）・`crates/engine/src/sql/udf_call.rs`（`Expr`・
  `BinOp`・`parse_number_literal`・`MAX_EXPR_NODES`・`UdfRegistry`・
  `UdfDefinition`・`define_function`・`MAX_SESSION_UDFS`）・
  `crates/engine/src/sql/expr_program.rs`（`ExprProgram::compile`）・
  `crates/engine/src/wasm_udf.rs`（`WasmUdfBackend`）・
  `crates/engine/src/tenant.rs`（`delete_row_ledgered_unchecked`・
  `update_row_unchecked` 系・`truncate_table_unchecked`）
- 関連 doc: `docs/design/predicate-dml-where.md`（Issue #869。述語つき `UPDATE` の
  許可リスト・束縛）・`docs/design/delete-predicate-form.md`（Issue #870。述語つき
  `DELETE` の許可リスト・束縛。RECOVER-11 節が本 ADR への窓口）・
  `docs/design/sql-multi-row-insert.md`（TASK-190・SQL-16。複数行 `INSERT` の
  「1 文 = 1 台帳エントリ」先例）・`docs/design/scalar-secondary-index.md`
  （Issue #472。ADR「判断記録」節の書式先例）・
  `docs/design/hnsw-subset-overlay-cache.md`（Issue #678。ADR ヘッダ書式先例）
- 検証コード: なし（docs 専任。production コード〔`crates/engine/src/`〕は本
  Issue の範囲では無変更。ハッシュ関数・実行結線の実装は #871／#865／#876 の担当）

**本 ADR の位置づけ**: 以下 4〜6 節（ハッシュ入力レイアウト・計算位置・実行時契約）は
spec 側 RECOVER-11 が「検討中」の段階でこのリポジトリが提示した提案であり、
2026-09-23 にオーナー承認（「判断記録」節）を経て本リポジトリの実装契約として確定した
（Issue #871 の実装〔`docs/design/predicate-dml-exec.md`〕で記録した意図的な変更点を含む）。
spec 側 RECOVER-11 は 2026-09-23 に確定した（本 ADR・Issue #871 の実装を確定根拠とする）。
今後 spec と本 ADR に差異が生じた場合は spec を正として本 ADR・実装を追随させる。

## 1. 背景・目的

台帳（`recovery::ledger`。TASK-93・RECOVER-2）は `(tenant, table, operation_id)`
という 1 つの複合キーに、内容照合ハッシュ（`recovery::content_hash`。TASK-101・
RECOVER-10）を対応づける。台帳エントリの存在は「その `operation_id` の書き込みが
commit 済みである」ことの確定的な根拠になり、再送時はハッシュが一致すれば
`23505`（同一内容の再送）、不一致なら `22023`（別内容での誤用）として拒否する
（`ledger::record_in_txn`）。

単一行 `INSERT`／`UPDATE`／`DELETE`・ファイル形 `INSERT`・`TRUNCATE` は、
それぞれ対応する `for_*` 関数が「クライアント要求由来の内容のみ」から決定的な
ハッシュを構成する設計で既に揃っている。複数行 `INSERT`（SQL-16・NoSQL
`rows[]`。`for_typed_insert_batch`）と `TRUNCATE`（`for_truncate`。入力なし）も
「1 文 = 1 台帳エントリ」の対応づけを既に満たしている。

一方、述語つき `UPDATE ... WHERE`（Issue #869・`ValidatedPredicateUpdate`／
`BoundPredicateUpdate`）と `DELETE ... WHERE`（Issue #870・
`ValidatedPredicateDelete`／`BoundPredicateDelete`）は束縛まで実装済みだが、
1 つの文が実行時点で確定する複数行へ作用するため、ハッシュ入力の定義が
まだ無い。両 doc（`predicate-dml-where.md`・`delete-predicate-form.md`）は
「正規化の情報源は宣言順そのもの」とだけ書き、具体のレイアウトを本 Issue へ
委ねている。実行結線（Issue #871）はこの入力仕様が確定するまで着手できない。

本 ADR は、複数行変更の再送判定が決定的になるハッシュ入力レイアウトと、
実行時の記録順序・原子性の**提案**をまとめる。承認（「判断記録」節）を経る
までは、#871・#876（NoSQL `update`／`delete` op）・#865（単一行 `UPDATE`
実行結線）にとって確定契約ではなく、作業前提として参照する案に留まる。

## 2. 現状整理（決定の前提）

- 台帳キーは `(tenant, table, operation_id)`、値は v2＝バージョンバイト＋
  SHA-256（32 バイト）。keep-first（先着優先）であり、`record_in_txn` は
  行の書き込みと**同一の `WriteTransaction` 内**で呼ぶ（`update_row_unchecked`
  系・`delete_row_ledgered_unchecked`・`truncate_table_unchecked` の共通順序:
  スキーマ取得 → ハッシュ計算 → `record_in_txn` → 行操作 → 世代カウンタ更新
  → `commit_boundary::commit`）。
- ハッシュ入力の正規化方針（`content_hash.rs` モジュールドキュメント）は
  「クライアント要求由来の内容のみ・DB 状態非依存・長さプレフィクス付き
  可変長フィールド・ドメイン分離タグ・操作種別タグ（`OpTag`）・複数行を
  1 ハッシュへ連結する場合は行境界の曖昧性を排除（PR #823）・列は列名基準
  （位置基準ではない。PR #248）」であり、本 ADR はこの方針をそのまま継承する。
- 複数行 `INSERT`（`for_typed_insert_batch`。`OpTag::InsertBatch`）は既に
  「1 文 = 1 台帳エントリ・件数プレフィクス・行順を保持・行ごとに非 NULL
  列数をプレフィクスして境界を明示」という形で複数行対応を満たしている。
  `TRUNCATE`（`for_truncate`）は入力自体が無いため対応不要。

**本 ADR が新規に設計する対象は、述語形 `UPDATE`・`DELETE` の 2 つに限る。**

## 3. 候補方式と採否

| 候補 | 内容 | 採否 |
| --- | --- | --- |
| A（採用） | 1 文 = 1 台帳エントリ。ハッシュ入力は**検証済み構文形**（テーブル名・`SET` の (列名, リテラル) 宣言順・`WHERE` 述語の宣言順・式中で参照する**セッション UDF の正準定義**）の正準バイト列とし、対象行集合・影響行数・変更前後の行内容は一切含めない | 採用 |
| B | 影響行ごとに `(operation_id, row id)` の台帳エントリを別々に持つ | 不採用: 台帳キーの一意制約と衝突しうる・上限（1,000 行）分のエントリ増幅・一部の行だけ台帳に載った状態が commit 途中で生じうる・再送時にどの行が commit 済みかを判定できない |
| C | 対象行の id 集合、または変更前後の行内容（before/after image）をハッシュへ含める | 不採用: DB 状態依存になる。再送時点でテーブルのスナップショットが変わっていると、クライアントが送った文は同一でも対象行集合が変わり別ハッシュになる。回復手順（RECOVER-7 系の「再送して commit 済みか確認する」手段）が、たまたま状態が変わっただけで `22023`（誤用）と誤判定してしまう |
| D | SQL テキストの生バイト列をそのままハッシュする | 不採用: 空白・大文字小文字・末尾セミコロンの有無で不一致になり同一操作の再送を誤検知する。SQL 表層（テキスト経由）と NoSQL 表層（#876。JSON から直接束縛）で同一操作でも別ハッシュになり、複数行 `INSERT`（`rows[]`）が既に確立している「表層を跨いだキー空間共有」を破る |

## 4. ハッシュ入力レイアウト（採用案 A の詳細）

### 4.1 操作種別タグ

`OpTag` へ新規 2 値を追加する想定（`UpdateWhere`・`DeleteWhere`。既存
`Insert`＝1・`InsertBatch`＝2・`Update`＝3・`Delete`＝4・
`ReplaceByTextKey`＝5・`Truncate`＝6 の続番。単一行 `Update`／`Delete` とは
別のドメインに分離し、部分更新〔単一行〕と述語一括更新〔複数行〕を
取り違えて同一ハッシュ空間へ落とさないようにする）。既存タグの値は
変更しないため、既存台帳エントリのハッシュは不変のまま保たれる。

### 4.2 想定シグネチャ（#871 が実装）

```text
for_update_where(table: &str, assignments: &[(&str, &InsertLiteral)], where_predicates: &[WherePredicate], udf_registry: &UdfRegistry) -> Result<ContentHash, StorageError>
for_delete_where(table: &str, where_predicates: &[WherePredicate], udf_registry: &UdfRegistry) -> Result<ContentHash, StorageError>
```

`udf_registry` は 4.4 節で述べる「参照 UDF 定義の正準化」のために式中の
`Call` 名を解決する読み取り専用の入力であり、呼び出し元（`bind_update_form`／
`bind_predicate_delete`。5.1 節）がその時点の `SessionState` から借用して渡す。
計算位置自体（束縛前・同一セッション内）は変わらないため、ハッシュが
DB 状態非依存であるという契約（2 節）は保たれる——`UdfRegistry` はテーブルの
行データではなく、同一セッションでクライアントが `CREATE FUNCTION` として
送った内容（クライアント要求由来）そのものだからである。

`pub(crate)`（既存 `for_*` 関数と同じ可視性）。`crates/engine/src/recovery/
content_hash.rs` への追加は #871 の担当であり、本 ADR は入力レイアウトの
みを確定する。

### 4.3 連結規則

1. `push_bytes(table_name)`（既存 `for_*` と同じ長さプレフィクス付き）。
2. `SET` 割当（`UPDATE` のみ。`DELETE` には無い）: 件数プレフィクス（u32
   LE・`push_raw`）→ 各要素につき `push_bytes(列名)`・リテラル種別タグ
   1 バイト（`InsertLiteral::String`＝1・`InsertLiteral::Number`＝2）・
   `push_bytes(リテラル生文字列)`。列名は束縛（`bind_set_assignments`）と
   同じ**大文字小文字を区別したまま**連結する（`c.name == name` の厳密
   一致と揃える）。`push_named_scalar_columns`（`Value::Null` を除外する
   実装）はここでは再利用しない——`InsertLiteral` に `Null` 相当の値は
   現状存在しないが、将来 `SET col = NULL` が追加されたときに黙って
   脱落させない前方ガードとして、割当を無条件に列挙する。`InsertLiteral::
   Number` は現状 `bind_set_assignments`（`TEXT`／`VECTOR` いずれの列型も
   `Number` 変種を拒否する）により意味論検証では必ず拒否されるが、
   ハッシュはこれより前の `Validated` 形から計算する（5.1 節）ため
   `Number` タグ自体は構造上到達しうる。ここでは**生文字列のまま**連結し、
   4.4 節の `WHERE` 式の数値（f64 の `to_bits()`）とは正規化方法を意図的に
   分ける——`SET` 側の `String` リテラルは `push_named_scalar_columns`／
   `row_codec::encode_scalar_columns` が永続化する列内容そのもの（テキスト
   表現の違いが行の内容差になりうる）であるのに対し、`WHERE` 側の数値は
   比較演算（`ExprProgram` の数値比較）にのみ使われ、文字列表現ではなく
   数値としての値が候補行確定の意味を持つため。
3. `WHERE` 述語: 件数プレフィクス（u32 LE）→ 各 `WherePredicate` を種別
   タグ 1 バイト＋フィールドで直列化する。
   - `Equality { column, value }`＝タグ 1・`push_bytes(column)`・
     `push_bytes(value)`
   - `Prefix { column, pattern }`＝タグ 2・`push_bytes(column)`・
     `push_bytes(pattern)`（`%` 除去前の生パターン。束縛前の構文形を
     ハッシュする方針〔4.4 節〕と整合させる）
   - `PredicateCall { name }`＝タグ 3・`push_bytes(name を小文字化した
     もの)`（許可リストの述語呼び出し名は大文字小文字を区別しない前提
     と揃える）
   - `Expression(Expr)`＝タグ 4・4.4 節の式直列化
4. 列名・述語は `ValidatedPredicateUpdate::assignments`／
   `where_predicates`・`ValidatedPredicateDelete::where_predicates` が
   保持する**宣言順のまま**連結し、並べ替えない。`WHERE a = 1 AND b = 2`
   と `WHERE b = 2 AND a = 1` は意図的に別内容として扱う（複数行 `INSERT`
   の行順が入力に含まれるのと同じ判断であり、正規化方針「連結曖昧性を
   構造的に排除する」の一貫性を優先する）。

### 4.4 式（`Expr`）の直列化と「構文形をハッシュする」理由

ハッシュ源は `sql::udf_call::Expr`（構文段 AST。`Number`・`Ident`・`Call`・
`Binary` の 4 variant）であり、束縛済みの実行形（`ExprProgram`・
`BoundExpr` 相当）ではない。理由:

1. `MetadataFilter`・`assignments` の束縛済み形は列インデックス基準で
   組み立てられる。`ALTER TABLE ADD COLUMN` を挟むと位置がずれる
   （PR #248 で列名基準へ改めた既存の教訓と同じ性質の問題）。
2. 構文段 AST は表層非依存（SQL テキスト経由でも NoSQL 表層の JSON から
   直接構築した構文形でも同じ木になる）であり、4.3 節冒頭で述べた
   表層横断のキー空間共有（5.2 節）にそのまま使える。

**構文段 AST を直接ハッシュするだけでは不十分**（4.4.1 節）。`Call { name,
args }` を呼び出し先の名前・引数のみで直列化すると、`WHERE f(x) > 1` は
`f` がどう定義されているかに関わらず常に同一のバイト列になる。台帳は
`(tenant, table, operation_id)` にのみ紐づき UDF 定義そのものは保持しない
ため、この設計のままでは「同じ `operation_id` で、実質的に異なる操作
（別定義の `f` を呼ぶ文）を送る」という誤用が `23505`（同一内容の再送）
として通ってしまい、`22023`（内容不一致）を返すべき場面を取り逃がす
（PR #987 codex-review 指摘）。4.4.1 節でこの穴を閉じる。

直列化はタグ付き前置順（prefix order）:

- `Number(raw)`＝タグ 1・`parse_number_literal(raw)` の `f64` を
  `to_bits()` で LE 8 バイトへ変換して連結（`"10"`／`"10.0"`／`"1e1"` を
  同一の数値として扱い同一視する。非有限値〔NaN／無限大〕は束縛段で
  既に拒否されているため到達しない）
- `Ident(name)`＝タグ 2・`push_bytes(name)`（大文字小文字はそのまま）
- `Call { name, args }`＝タグ 3・`push_bytes(name を小文字化したもの)`・
  引数件数プレフィクス（u32 LE）・各引数を再帰的に直列化
- `Binary { op, lhs, rhs }`＝タグ 4・`BinOp` を 1 バイトへ写像（`Add`＝1・
  `Sub`＝2・`Mul`＝3・`Div`＝4・`Gt`＝5・`Lt`＝6・`Ge`＝7・`Le`＝8・
  `Eq`＝9）・lhs を再帰的に直列化・rhs を再帰的に直列化

呼び出し先の名前（`push_bytes(name を小文字化したもの)`）は不変のまま
残す——組み込み関数呼び出しとの区別が不要になるうえ、4.4.1 節の UDF
定義直列化と組み合わせても「同名の異なる定義」と「異名の同一定義」を
どちらも意図どおり別内容として扱える。

**Issue #921（対象ビヘイビア: SQL-26）で追加したタグ**（`Expr` に
`Null`／`Case`／`Coalesce`／`NullIf` の 4 variant を追加。既存タグ 1〜4 は
不変）:

- `Null`＝タグ 5（本体なし）
- `Case { whens, else_result }`＝タグ 6・`whens` の件数（u32 LE）・各
  `(cond, result)` を順に再帰的に直列化・ELSE 有無フラグ（u8。`0`／`1`）・
  `else_result` が `Some` の場合のみその本体を再帰的に直列化（フラグを
  本体の前に置くことで、可変長要素の境界が曖昧にならないようにする）
- `Coalesce(args)`＝タグ 7・引数件数（u32 LE）・各引数を再帰的に直列化
- `NullIf(lhs, rhs)`＝タグ 8・lhs を再帰的に直列化・rhs を再帰的に直列化

入力サイズの有界性は既存の上限（`MAX_CALL_ARGS`・`MAX_EXPR_NODES`。
`sql::udf_call::MAX_CASE_BRANCHES`・`MAX_CASE_NESTING` を新設）の範囲内に
収まり、本節が追加する新たな上限はない。

入力サイズの有界性: `assignments` は `MAX_UPDATE_SET_ASSIGNMENTS`
（256）、述語は `MAX_METADATA_FILTERS`（256）、式のノード数は
`MAX_EXPR_NODES`（1024）で既に上限が掛かっており、`push_bytes` 自体も
u32 長を超えるフィールドを拒否する。本 ADR は新たな上限を追加しない
（4.4.1 節の UDF 定義直列化も `MAX_SESSION_UDFS`・`MAX_UDF_PARAMS`・
`MAX_EXPR_NODES` という既存の上限の範囲内に収まる）。

#### 4.4.1 参照 UDF 定義の正準化（内容照合ハッシュへの組み込み）

**前提の確認**: `sql::udf_call::UdfRegistry` は追記専用であり、
`define_function`（`is_name_taken` による衝突検査）は**同一セッション内**
での同名 UDF の再定義を構造的に拒否する。したがって、1 つの接続（1
セッション）の中で「同じ文を送った後に同名 UDF だけ差し替えて再送する」
という経路は起こらない。codex-review が指摘する脅威は、**接続を跨いだ
再送**（RECOVER-7 が想定する「再送して commit 済みか確認する」回復手順・
クライアントの再接続後の再送を含む）である——`UdfRegistry` はセッション
単位（`sql::mode::SessionState` が保持）であり永続化されないため、
新しい接続で同名だが異なる本体の UDF を定義してから、以前と同じ
`operation_id` を持つ `UPDATE ... WHERE f(x) > 1` を送ることは可能であり、
その場合 `f` が指す操作の意味は変わっている。台帳はこの意味の違いを
`22023` として検出できなければならない。

**設計**: `Call { name, args }` を直列化する際、`name`（小文字化したもの）
を呼び出し時点の `udf_registry`（4.2 節。`bind_update_form`／
`bind_predicate_delete` が保持するセッションの `UdfRegistry`）で引き、
**WASM UDF か・組み込み関数か・セッション宣言的 UDF かを判別したうえで**
（`get_wasm` → `get` の順で確認する。WASM UDF は末尾の段落のとおり文
全体を拒否する）、宣言的 UDF の場合のみ、その定義（`UdfDefinition
{ params, body }`）を同じ直列化スキームで末尾に連結する。

1. `WHERE`（`Expression(Expr)`。`SET` 側の値は `InsertLiteral` のみで
   `Expr` を経由しないため対象外）の式木を走査し、出現する `Call` の
   `name`（小文字化）を集める。
2. 各名前をまず `udf_registry.get_wasm(name)` で引く。`Some`（WASM UDF に
   解決される呼び出し）なら文全体を許可形状外として拒否する（末尾の
   「WASM UDF は本節の対象外」段落を参照。`get_wasm` は `get`（宣言的
   UDF 用の `defs` マップ）とは別マップを引くため、この判定は必ず
   `get(name)` より先に行う——`get(name)` の結果だけで「組み込み関数か
   セッション UDF か」を二分すると、WASM UDF の名前も `get` からは
   `None` が返り「組み込み関数」側へ誤って分類されてしまう。この判定は
   4 の推移閉包でも同じ順序で毎回行う）。`None` なら次へ進む。
3. `udf_registry.get(name)` で引く。`None`（組み込み関数・`catalog`／
   `allowlist` の許可名）なら 4.4 節の既存直列化のまま（呼び出し先の
   変化は名前自体の変化としてのみハッシュに現れる）。`Some(def)`
   （セッション宣言的 UDF）なら**参照 UDF 集合**へ追加する。
4. **推移閉包**: 追加した UDF の `body`（`Expr`）自体にも `Call` が
   含まれうる（UDF 本体が別の登録済み UDF・WASM UDF を呼ぶ多段呼び出し。
   `define_function` の `validate_closed_expr` は宣言的 UDF・WASM UDF の
   いずれの呼び出しも本体に許可する）。この `Call` も 2〜3 と**同じ
   手順**（`get_wasm` を先に確認し `Some` なら拒否、`None` なら
   `get` で解決）で処理する——参照 UDF の本体を「名前だけ」で直列化
   してしまうと、その本体が呼ぶ WASM UDF のバックエンドが差し替わっても
   ハッシュが変化せず、4.4.1 節冒頭で閉じたはずの穴（同一
   `operation_id` で異なる操作を `23505` として通してしまう）が
   参照 UDF の本体経由で再び開く（PR #987 Bugbot 指摘）。`Some(def)`
   （宣言的 UDF）なら参照 UDF 集合へ追加する。`UdfRegistry` は追記専用で
   UDF は**自身より前に登録済みの UDF のみ**を呼べる（`define_function`
   の検証時点で `registry` に無い名前への `Call` は拒否される）ため、
   この走査は有向非巡回（サイクルなし）であり必ず停止する。
5. **決定的な順序**: 参照 UDF 集合は名前（小文字化。`UdfRegistry` の
   キーと同じ）の辞書順にソートしてから直列化する（初出順ではなく
   名前順。複数の呼び出し箇所・複数の式から同じ UDF が参照されても
   重複なく 1 回だけ現れる）。
6. **参照 UDF 定義セクション**の直列化（`WHERE` 述語直列化〔4.3 節〕の
   末尾に追記する新規セクション）: **参照 UDF 集合が空（`Call` が組み込み
   関数のみを指す場合を含む）ならこのセクション自体を一切追加しない**
   （件数プレフィクスの 0 すら書かない）。これにより「UDF を呼ばない文
   （宣言的 UDF を一切参照しない文）は本節導入前とビット同一のハッシュに
   なる」という互換性が成立する——件数プレフィクスとして u32 LE の 0 を
   常に追記する設計だと、参照 UDF が無い文でも本節導入前より 4 バイト
   長いバイト列になり、この互換性主張自体が成立しない（PR #987
   codex-review 指摘）。参照 UDF 集合が 1 件以上のときに限り、件数
   プレフィクス（u32 LE。1 以上の値）→ 各 UDF につき
   `push_bytes(name を小文字化したもの)`・
   パラメータ件数プレフィクス（u32 LE）・各 `push_bytes(param)`（宣言順。
   `define_function` が `normalized_params` として保持する**小文字化済みの
   正規形**をそのまま使う。パラメータ名は SQL 識別子として大文字小文字を
   区別しない扱いに正規化される契約であり〔`define_function` 実装
   コメント参照〕、元の綴りは保存された時点で既に失われている）・
   `body`（`Expr`）を 4.4 節の直列化スキームで再帰的に直列化する。ただし
   **`body` 中の `Ident(name)` がこの UDF のパラメータを指す場合
   （`name.to_ascii_lowercase()` が `normalized_params` のいずれかと一致する
   場合。`bind_expr_in` の `Expr::Ident` 分岐が本体を束縛する際の判定と
   同じ）に限り、4.4 節の「大文字小文字はそのまま」の既定規則を適用せず
   `push_bytes(name.to_ascii_lowercase())` で連結する**（パラメータ参照は
   本体中で `Ident(param_name)` として現れる）。理由: `bind_expr_in` は
   パラメータ参照を大文字小文字を区別せず解決する（`env.params.get(name.
   to_ascii_lowercase().as_str())`）ため、`CREATE FUNCTION f(x) AS x*2` と
   `CREATE FUNCTION f(X) AS X*2` は意味的に同一の UDF だが、4.4 節の既定
   規則で `body` の `Ident` を原綴りのままハッシュすると異なるバイト列に
   なり、再接続・再送信で同一 `operation_id` に対し誤って `22023`
   （内容不一致）を返しうる（PR #987 Bugbot 指摘）。閉じた本体
   （`validate_closed_expr` が列参照を拒否する）ではパラメータ以外の
   自由変数は存在しないため、`Ident` の正規化対象はパラメータ参照に限定
   でき、`WHERE` 直下（列参照は大文字小文字を区別する。4.4 節の既定規則の
   まま）には影響しない。

これにより「同名・同定義」の UDF を呼ぶ再送は常に同一ハッシュ（`23505`）
になり、「同名・異なる定義」（本節が閉じる穴）や「異名・同一定義」
（呼び出し先の名前が直列化に含まれるため）はいずれも異なるハッシュ
（`22023`）になる。定義自体はクライアントが同一セッションへ
`CREATE FUNCTION` として送った内容であり、4 節冒頭の正規化方針
（クライアント要求由来の内容のみ・DB 状態非依存）の例外ではなく、その
まま適用範囲を「呼び出し文だけでなく、呼び出し文が依存する同一セッション
内のクライアント入力（UDF 定義）まで」へ広げたものである。

**却下した代替案**（記録のみ）: 束縛済み実行形（インライン展開後の
`BoundExpr` 相当）をそのままハッシュする案は、4.4 節冒頭の理由 1・2
（列インデックス依存）により不採用のまま据え置く。構文形は維持しつつ
UDF 呼び出しだけを事前にインライン展開してからハッシュする案（`WHERE
f(x) > 1` を `f` の本体で置換してハッシュする）も、置換後の木が別の
リテラル式（例: `WHERE x*2 > 1`）と偶然一致しうる——「関数呼び出しを
使った文」と「同じ計算を式で書いた文」が別の要求であるにもかかわらず
同一ハッシュ（`23505`）に落ちてしまう——ため不採用とし、本節の
「呼び出し先の名前は残したまま定義を追記する」設計を採る。

**WASM UDF（TASK-149・EXT-5, EXT-6）は本節の対象外**とし、predicate 形
`UPDATE`／`DELETE` の `WHERE` から `Call` が WASM UDF（
`UdfRegistry::get_wasm` で解決される名前）に解決される場合は許可形状外
として拒否する（既存の「サポート対象外の構文要素」と同じ `0A000`。
Issue #871 が実装する `bind_update_form`／`bind_predicate_delete` の
検証項目へ追加する）。**この拒否判定は `WHERE` 直下の `Call` に限らず、
上記 2〜4 の走査が可視化するすべての `Call`（`WHERE` 式木・推移閉包で
辿った参照 UDF の本体）へ再帰的に適用する**——宣言的 UDF の本体は
`validate_closed_expr` が WASM UDF 呼び出しを許可するため、`WHERE` 自体は
宣言的 UDF だけを直接呼んでいても、その本体が WASM UDF を呼ぶ経路が
残っていると、4.4.1 節冒頭で閉じたはずの「同一 `operation_id` で異なる
操作が `23505` として通る」穴が参照 UDF 経由で再び開く（PR #987 Bugbot
指摘。2〜4 節の記述を参照）。理由: `WasmUdfBackend` trait
（`crates/engine/src/wasm_udf.rs`）
は `call_vector_scalar` のみを公開し、登録済みモジュールの内容・バージョン
を指し示す安定な識別子（ハッシュ・ダイジェスト等）を一切保持しない
契約層のみの実装（wasmtime バックエンド自体が依存追加のユーザー承認
待ち。Issue #97）であるため、本節の「定義の正準形をハッシュへ含める」
設計を宣言的 UDF と同じ形では今は満たせない。`WasmUdfBackend` へ
安定な定義識別子を返すメソッドを追加し、登録（`define_wasm_function`）
時点でその識別子を記録する設計は、wasmtime バックエンド接続時の
別 Issue（#871 または新規 Issue）へ申し送る。

## 5. 計算位置と表層横断のキー空間共有

### 5.1 計算位置

束縛後の `BoundPredicateUpdate::assignments` は列インデックス基準
（`(usize, Value)`）へ写像済みで列名を失う。したがってハッシュ材料は
**束縛前の `Validated*` 形（`ValidatedPredicateUpdate`／
`ValidatedPredicateDelete`）から**組み立てる。

推奨実装: `bind_update_form`／`bind_predicate_delete` の `Predicate` 腕で
一度だけ `ContentHash` を計算し、対応する束縛済み型（`BoundPredicateUpdate`
／`BoundPredicateDelete`）へ `pub(crate) content_hash: ContentHash`
フィールドとして保持させ、`tenant::*_unchecked` 側へそのまま渡す。
これは `for_insert_encoded` 系が「呼び出し元が 1 回だけ計算した値を
台帳記録と行書き込みの双方へ渡す」設計（Issue #397）と同じ運び方であり、
束縛と実行の間でハッシュ材料を再構築しない。`bind_update_form`／
`bind_predicate_delete` は式の型検査（`bind_where_predicates` の
`Expression` 腕）で既に `SessionState` の `UdfRegistry` を参照しているため、
4.4.1 節の UDF 定義解決に必要な `udf_registry` はこの型検査と同じ借用を
`for_update_where`／`for_delete_where` へ渡すだけでよく、新たな
状態アクセスを追加しない。

### 5.2 SQL⇄NoSQL 表層横断のキー空間共有

NoSQL `update`／`delete` op（#875・#876）は SQL テキストを組み立てず、
JSON リクエストから直接 `Validated*` 相当の検証済み構文形（表層非依存の
正準構造。列名・リテラル・述語の宣言順を保つ）を構築し、同じ
`for_update_where`／`for_delete_where` へ渡すこと。これにより、同一操作を
SQL 表層と NoSQL 表層のどちらから送っても同一ハッシュ空間で再送判定が
働く（複数行 `INSERT`〔`rows[]`〕・`nosql6_insert.rs` の先例と同じ設計）。
`for_update_where`／`for_delete_where` は表層を問わず「呼び出し元の
セッションが保持する `UdfRegistry`」を受け取る契約（4.2 節・4.4.1 節）
であるため、NoSQL 表層が式（`WHERE` 相当の JSON 述語）内で UDF 呼び出しを
許容する場合も、SQL 表層と同じ手順（同一セッションの `UdfRegistry` を
そのまま渡す）で同一ハッシュ空間を維持できる。

`BoundPredicateUpdate::new` を `pub` 化しない方針（PR #985 の是正）とも
整合させ、NoSQL 表層は `bind_update_form` と同じ検証（`SET` 対象列の
拒否・空フィルタの拒否・`operation_id` 必須化）を内部で必ず通したうえで
束縛済み型を返す別の公開 API を経由すること（`predicate-dml-where.md`
「NoSQL 表層・実行結線への申し送り」節が既に同じ方針を記載している）。

## 6. 実行時の順序・原子性・0 行・上限超過（#871 への提案）

1 つの write トランザクション内で、以下の順序を守ること:

1. `begin_write_txn` → スキーマ取得。
2. `record_in_txn`（**候補列挙より先**に呼ぶ。使用済み `operation_id` は
   可視集合を一切走査せずに `23505`／`22023` へ短絡できる。この順序に
   より、応答が対象行の有無・件数に依存しない——RLS-9／RLS-10 が求める
   「他テナントの存在情報を漏らさない」設計と同じ理由で、台帳照合の
   応答も可視行数に依存させない）。
3. テナント所有範囲（`(tenant, 0)..=(tenant, u64::MAX)`）を走査し、
   `is_owner` 判定と述語（`ExprProgram::compile` した式評価を含む）で
   候補行を確定する。`MAX_DML_AFFECTED_ROWS + 1` 件で列挙を打ち切る
   （`check_dml_affected_rows` の入力契約と同じ「打ち切った列挙結果を
   渡す」設計）。
4. `check_dml_affected_rows`（`UPDATE`）／`check_affected_row_count`
   （`DELETE`。`DEFAULT_MAX_DML_AFFECTED_ROWS` を limit として渡す）を
   **変更を開始する前**に呼ぶ。超過時はトランザクションを drop する
   （台帳エントリ・行変更のいずれも残らない。INDEX-4 の `54000` と同じ
   「副作用ゼロで拒否」の扱い）。
5. 候補行すべてへ変更を適用する。
6. 影響行数が 1 件以上のときのみ `bump_table_generation_in_txn` を呼ぶ
   （0 行一致では世代を進めない。キャッシュ〔`SqlArenaCache`・
   `ScalarIndexCache`・`HnswIndexCache` 等〕への不要な失効通知を避ける）。
7. `commit_boundary::commit`。

**原子性**: 全行へ適用するか、全く適用しないかのいずれかとする。commit
前に失敗（エンコードエラー・redb エラー・panic）した場合はトランザクション
が drop され、行・台帳のいずれにも痕跡が残らない。commit 後の panic は
RECOVER-5／RECOVER-6 の既存ガード（`recovery::commit_boundary`・
`recovery::panic_hook`）の管轄であり、本 ADR は変更しない。**台帳エントリの
存在は「当該文が対象とした全行の変更が commit 完了した」ことの確定的な
根拠であり続ける**——複数行変更でもこの同値性を崩さない。

**0 行一致**: 一致する行が 0 件でも、台帳エントリは commit する（対象行
集合をハッシュに含めない設計〔候補 C 不採用〕の直接の帰結: 0 行一致の
再送は、スナップショットが変わっていても常に `23505` へ収束する。世代は
進めない）。

**並行再送**（RECOVER-3）: redb は単一ライターであるため、同一
`operation_id` を持つ 2 つの並行要求は片方が待たされ、同一トランザクション
内での台帳照合により最終的にどちらか一方のみが commit される（既存の
`record_in_txn` の keep-first 契約をそのまま利用する）。

**応答**: `UPDATE n`／`DELETE n`。他テナント行・RLS 不可視行は候補にすら
ならないため、件数・エラー文言・`wire_code` のいずれにも他テナントの
存在情報を含めない（RLS-9・RLS-10）。

**上限 API の並立（申し送り）**: `UPDATE` 側は `MAX_DML_AFFECTED_ROWS`
（`pub const`）＋`check_dml_affected_rows(count)`、`DELETE` 側は
`DEFAULT_MAX_DML_AFFECTED_ROWS`（`pub const`）＋
`check_affected_row_count(count, limit)` という、シグネチャの異なる 2 つの
上限 API が並立している（いずれも `crates/engine/src/sql/parser.rs`）。
両者の統合は本 ADR の対象外とし、#871 実装時の申し送り事項として記録する
に留める。

## 7. 単一行 `UPDATE` 実行結線（Issue #865）への申し送り

既存 `for_update_encoded(id, encoded_row)` は**マージ後の行全体**を
ハッシュする。マージ結果は既存行（更新前の DB 状態）に依存するため、
部分更新である SQL `UPDATE ... SET col = lit WHERE id = n`（単一行・`id`
完全一致形）にそのまま使うと、同一文の再送でも既存行の内容が変化して
いれば異なるハッシュになり、正規化方針（クライアント要求由来の内容のみ・
DB 状態非依存）に反する。

Issue #865 では、本 ADR §4.3 と同型の `(table, assignments 宣言順, id)` という
**構文形**からハッシュすることを推奨として記録する（`OpTag::Update` の
レイアウトを本 ADR の設計に合わせて見直すか、新規タグを割り当てるかは
Issue #865 側の判断とする）。

## 8. 対象外・後続

- **UPSERT**（#872）: `ON CONFLICT` の挙動を含めた別 `OpTag` の設計が
  必要になる見込みだが、本 ADR は方向性のみを示し詳細レイアウトは
  確定しない。
- **明示トランザクション内の台帳**（RECOVER-12・#942）: ポインタのみを
  記載し、本 ADR の範囲では扱わない。
- **`RETURNING`**（#873）: 応答形の違いであり、ハッシュ入力には含めない
  方針のみを記す。
- **`OR`・括弧付き述語**（将来の拡張述語）: `WherePredicate`・`Expr` へ
  種別タグを追加する形で本レイアウトを自然に拡張できる設計であることを
  記す（現行の `AND` 平坦列挙・タグ付き直列化の枠組みを維持したまま
  拡張可能）。

## 9. spec 側へのフィードバック項目（ポインタ表記）

RECOVER-11 の確定（2026-09-23）に向けて、以下をポインタ表記で申し送った（契約
文そのものは転記しない）:

- 数値リテラルの同一視規則（f64 への正準化。RECOVER-11）
- `SET` 割当・`WHERE` 述語の宣言順保持（RECOVER-11・SQL-19）
- 0 行一致でも台帳記録する契約（RECOVER-11・RECOVER-7）
- 上限超過時は台帳未記録（RECOVER-11・INDEX-4 相当）
- SQL・NoSQL 表層で同一ハッシュ空間を共有する契約（RECOVER-11・NOSQL-12）
- ハッシュ源は束縛前の構文形とする契約（RECOVER-11・SQL-19）
- 参照するセッション UDF の定義（推移閉包・名前順）をハッシュ入力へ含める
  契約（RECOVER-11・SQL-9・SQL-19。4.4.1 節）
- 単一行 `UPDATE` のハッシュ源見直し（RECOVER-10・SQL-17・Issue #865）

## 9.5 実装記録（Issue #871）

Issue #871（述語つき UPDATE/DELETE の実行結線）は自動運転モードのため
オーナー承認を待たず本 ADR を作業前提として実行結線し、PR #993 のレビューで
オーナー承認（2026-09-23・「判断記録」節）を得た（詳細・受け入れ条件・
テストは `docs/design/predicate-dml-exec.md` 参照）。ADR からの意図的な変更点
（§4.2 の計算位置・エラー型）も同 doc に記録済み。`OpTag::UpdateWhere = 9`／
`DeleteWhere = 10`（既存 1〜8 の続番。値の変更なし）。

## 10. 判断記録（オーナー記入欄）

| 項目 | 内容 |
| ---- | ---- |
| 判断 | 採用（§4〜6 の提案を本リポジトリの実装契約として確定。#871 で記録した意図的な変更点〔`docs/design/predicate-dml-exec.md`〕を含む） |
| 根拠 | Issue #871 の実装（PR #993）が受け入れ条件（候補列挙の所有スコープ・1 write トランザクション原子性・途中失敗の注入試験・再送判定）を満たし、codex-review 指摘の対応後に整合を確認した |
| 条件 | spec 側 RECOVER-11 は 2026-09-23 に確定（本 PR のマージを条件とする）。今後差異が生じた場合は spec を正として追随する |
| 判断日 | 2026-09-23 |
| 記入者 | オーナー（PR #993 レビュー時の承認。記入は Claude Code が代行） |

## スコープ外

- `crates/` 配下のコード変更・ハッシュ関数の実装そのもの（#871・#865・
  #876 が担当）
- 実行結線（候補行列挙・一括適用・応答生成・キャッシュ失効通知）の実装
  （#871・#870 の実行結線部分）
- 上限既定値（`MAX_DML_AFFECTED_ROWS`・`DEFAULT_MAX_DML_AFFECTED_ROWS`）の
  統合・数値の見直し
- `OpTag::UpdateWhere`／`DeleteWhere` の実コード追加（#871 の担当。本 ADR は
  値の割当方針〔既存 1〜6 の続番〕のみを示す）
- UPSERT（#872）・明示トランザクション台帳（RECOVER-12・#942）の詳細設計
- `WasmUdfBackend`（`crates/engine/src/wasm_udf.rs`）への安定な定義識別子
  メソッドの追加（4.4.1 節。wasmtime バックエンド接続時の別 Issue へ
  申し送り。本 ADR は predicate 形 `UPDATE`／`DELETE` の `WHERE` から
  WASM UDF への `Call` を拒否する提案〔4.4.1 節〕のみを示す）

## 参照

- `docs/spec/04-behavior/recovery.md`（RECOVER-2・RECOVER-3・RECOVER-7・
  RECOVER-10・RECOVER-11）
- `docs/spec/04-behavior/errors.md`（ERR-2・ERR-3）
- `docs/spec/04-behavior/sql.md`（SQL-16〜SQL-22）
- `docs/spec/04-behavior/nosql.md`（NOSQL-12）
- `docs/spec/05-tasks.md`（TASK-93・TASK-101・TASK-190・TASK-191・TASK-192）
- `docs/design/predicate-dml-where.md`（Issue #869）
- `docs/design/delete-predicate-form.md`（Issue #870）
- `docs/design/sql-multi-row-insert.md`（TASK-190・SQL-16）
- `docs/design/scalar-secondary-index.md`（Issue #472。ADR 判断記録の書式先例）
