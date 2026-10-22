# INSERT ... SELECT（SQL-51。parser・束縛 #1732／実行 #1733）

本書は 2 部構成。第 I 部は構文・束縛（Issue #1732）、第 II 部は実行（Issue #1733）。

親 #1731・ビヘイビア SQL-51 のポインタ（spec 本文は転記しない。関連ポインタ: SQL-50・SQL-47・SQL-34・SQL-32・RLS-10・RECOVER-10/11）。

## 第 I 部 構文・束縛（#1732）

### 1. 範囲

本 Issue は **構文・内側 SELECT の検証・列数／型の束縛** を担う。行の実体化・RLS・挿入・台帳は #1733・#1734 の担当で、
実行腕は束縛の後に第 II 部の実行本体へ渡す（結線済み）。

| 段 | 担当 | 場所 |
| -- | ---- | ---- |
| 構文（問い合わせ span の切り出しと後続句） | #1732 | `sql::allowlist`（`parse_insert_any_shape_tokens_with_limit`） |
| 内側 SELECT の検証 | #1732（既存の SELECT 検証器を再利用） | `core.rs::parse_insert_select` |
| 列数・型の束縛 | #1732 | `sql::insert_select::bind` |
| 内側 SELECT への RLS・実行・挿入 | #1733 | `core/insert_select_exec.rs`（結線済み） |
| 台帳の内容照合 | #1734 | `recovery::content_hash::for_insert_select`（結線済み） |

### 2. 構文

```text
INSERT INTO <表> (<列>[, ...]) <query> [RETURNING ...] [USING OPERATION_ID '<id>'] [;]
```

- `<query>` の先頭は `SELECT`、`(` の直後が `SELECT`、`WITH` のいずれか。それ以外（`VALUES` を含む）は従来どおり
- 問い合わせの範囲は、括弧の深さ 0 で最初に現れる次のいずれかの手前まで
  - `ON CONFLICT`（2 トークン組）・`RETURNING`・`USING OPERATION_ID`（2 トークン組）・`;`・文末
  - JOIN の `ON`・`USING (...)`・`USING PLAN(...)`・`USING MODE` を誤って区切りにしないため、`ON`／`USING` は 2 トークン組で判定する
- 範囲はトークン列のまま既存の検証器へ渡し、SQL 文字列を組み立て直さない（インジェクションを構造的に避ける）
- `ON CONFLICT` との併用は `0A000`（別 Issue）。空の問い合わせ・`DEFAULT VALUES` の後ろの問い合わせは `42601`（列リスト省略は第 III 部〔Issue #2613〕で受理）
- 既存の公開入口（`validate_insert` 等）は内側 SELECT を検証できないため、SELECT 形を受けると `0A000`

### 3. 内側 SELECT の検証

- 検証は SELECT 経路と同じ `validate_sql_tokens_with_subquery_ctx_limits`（一時カタログ・ビューの解決込み）
- 受理する文種は読み取りのみ。`EXPLAIN`・`SET`・`SHOW`・`CREATE FUNCTION` は `42601`、`USING PLAN`（LLM I/O を起こす）は `0A000`
- 挿入先がビューなら `0A000`、表でもビューでもなければ既存の書き込み文と同じ `42P01`／`42809`
- 一時表が関与する文は `is_routable` が偽のため、既存の `0A000` 経路に落ちる
- 拡張クエリで `$n` を含む形は Parse 段で `0A000`

### 4. 束縛の判定順

1. 列リストの解決: 重複（VALUES 形と共有のヘルパー）→ 未知列（`42703`）→ `GENERATED ALWAYS`（`428C9`）
2. 疑似列 `id`: 従来方式は列リストに必須、移行済みの表は省略可（採番は書き込み段）
3. 列数: 内側の出力列と列リストが食い違えば `42601`
4. 型: 出力列の静的型を挿入先へ代入できなければ `42804`。型を持たない式（`Computed { ty: None }`）も `42804`。
   規則表の外にある `VECTOR`・`ENUM` は同じ値ドメインのときだけ受理する
5. `RETURNING` の投影を既存の束縛で検証する

内側の結果列メタは既存の Describe で導出する（列の導出を第 2 の実装にしない）。挿入先スキーマと内側の
スナップショットの一致は本 Issue では要求せず、実行時の再検証は #1733 の実行本体が write txn 内で行う。
エラー文言は英語の固定文言とスキーマ由来の列名だけで、値・他テナントの情報を含めない。

### 5. 後続への引き継ぎ

- #1733・#1734: 結線済み（第 II 部。`BoundInsertSelectTarget` の再利用と `query_tokens` による台帳ハッシュ）
- #1730: `DEFAULT VALUES`（列リスト省略の `INSERT ... SELECT` は第 III 部で受理）
- SQL-34 / #2212: 内側 SELECT の LIMIT 省略の扱い

### 6. 対象外（fail-closed）

`ON CONFLICT` との併用・ビューへの挿入・一時表が関与する文・拡張クエリの `$n`・`OVERRIDING`・
`PARTITIONED` との併用（構文上受理しない）。

## 第 II 部 実行（SQL-51・Issue #1733）

親: #1731。構文・束縛は #1732、内容照合ハッシュの文正規化は #1734 の担当。本書は実行部分（内側 SELECT の
評価から挿入まで）の設計判断を記録する。ビヘイビアはポインタ表記のみ（SQL-51・SQL-34・SQL-21・INDEX-4・
RLS-7・RLS-10・RECOVER-10・RECOVER-11）。

### 実行経路

1. `ON CONFLICT` 併用は `0A000`（別 Issue。SQL-49）。
2. 対象表のスキーマ取得（`42P01`。それ以外の失敗は固定文言の `XX000`）。RETURNING は評価より前に束縛する。
3. 内側 SELECT を **書き込むセッションの `PolicyContext`** で既存の読み取り経路
   （`execute_read_statement_with_limits`）へ通して実体化する。独自の走査を持たないため RLS が暗黙に
   適用され、他テナントの Private 行・不可視行は材料にならない。
4. 1 段目の上限: 結果行数が `--max-result-rows`（matview と同じ `cap`）を超えたら `54000`。
5. 結果行を `Vec<BoundInsert>` へ変換する（`sql/insert_select.rs`）。省略列は DEFAULT 補完、
   `DEFAULT nextval` 系の印は COPY と同じく `0A000`。
6. 2 段目の上限: `--max-insert-rows` と INDEX-4 のバッチ上限を挿入前に判定する（`54000`）。
7. 書き込みは複数行 `VALUES`・COPY と同じ `sql::exec::execute_insert_batch_with_schema_in`
   （RETURNING は `execute_insert_returning_in`）。1 write txn・1 台帳エントリで、失敗は txn の abort で
   全体が拒否される。0 行は行の書き込みをせず、台帳だけを記録する（`record_insert_select_ledger_only`）。

### スナップショットの判断

- autocommit: 内側 SELECT は確定済みスナップショットで評価し、挿入は `WriteTarget::Autocommit`
  （既存の INSERT と同じ。DEFERRED の FK 検査を含む）で行う。全行を実体化してから書くため、自己参照
  （`INSERT INTO t SELECT ... FROM t`）でも挿入行を同じ文が読まない。
- 明示トランザクション: 共有 write txn を読み取り源にも書き込み先にも使う（自トランザクションの未 commit
  変更が見え、ROLLBACK で破棄される）。
- 採らなかった案: autocommit で自前の write txn を開き `WriteTarget::InTxn` を渡す方式。`InTxn` は
  DEFERRED の FK 検査を COMMIT へ先送りするため、autocommit で検査が抜ける（fail-open）。writer gate を
  保持したまま評価する方式（文単位の呼び出し側所有 txn で FK は全検査）は `tenant.rs` の P0 経路の変更を
  伴うため本 Issue では採らない。要判断: spec の操作欄が「書き込み txn のスナップショットで評価」と
  読めるため、人間の確認が要る。

### 結線（#1732 のマージに伴い完了）

`ParsedSql::InsertSelect` の実行腕は autocommit・明示トランザクションとも
`EngineCore::execute_parsed_insert_select`（`core/insert_select_exec.rs`）を呼ぶ。入口は先に
`bind_insert_select`（第 I 部の束縛。列数 `42601`・型 `42804` を挿入より前に拒否）を通し、検証済みの
文を `InsertSelectRequest` へ詰めて `execute_insert_select` へ渡す。

- autocommit: `InsertSelectTxn::Autocommit`。内側 SELECT は確定済みスナップショット、挿入は `WriteTarget::Autocommit`。
- 明示トランザクション: 共有 write txn を `InsertSelectTxn::InTxn` で渡し、成功後に `mark_written`（台帳・世代の単位は挿入先の表）。
- RETURNING: VALUES 形と同じ経路（`execute_insert_returning_in`）。Describe は束縛結果の投影から列メタを返す。
- 台帳ハッシュ: `ValidatedInsertSelect::statement_hash` を parse 時に
  `for_insert_select(strip_operation_id_clause(送られた文のトークン列))` で算出する（結果行は材料にしない）。
- `ON CONFLICT` 併用は parser が `0A000` で拒否する。列リスト省略（#1730）・ビュー・一時表・拡張クエリの `$n`・`USING PLAN` も
  従来どおり `0A000`／`42601`。
- 列の解決は束縛（`bind`）が先に行い、実行側の `resolve_target_columns` は同じ列リストを `TargetColumn` へ写す。
  書き込み先を表す型は `TargetColumn` に一本化した（旧 `InsertSelectSlot` は廃止）。
- e2e: `tests/sql51_insert_select_exec.rs`（行の挿入・RLS・再送・0 行・明示トランザクション・ON CONFLICT）。

### 台帳（#1734 への引き継ぎ）

内容照合ハッシュは結果行ではなく、呼び出し側から渡される `statement_hash`（文の正規化ハッシュ）を使う。
書き込み段は `InsertSelectLedgerScope` で行の値ベースのハッシュを文ハッシュへ差し替える。同じ
`operation_id` での再送は、内側の結果が変わった後でも同一文なら台帳キーで照合され `23505` になり、
二重挿入しない。文が異なる再送は内容不一致として拒否される（拒否側。fail-open ではない）。

### 値の変換（代入変換）

VARCHAR/CHAR/ENUM/JSON は VALUES 形と同じ束縛関数で再検証し、整数幅は範囲検査（`22003`）、NUMERIC は
`apply_numeric_typmod`、整数 → 浮動小数は拡大のみ受理する（この直接変換は第 III 部でも不変）。
それ以外の組（浮動小数 → 整数・各種 → text・配列等）は第 III 部の代入変換が担う。静的な型照合（`42804`）は #1732 の担当。

## 第 III 部 列リスト省略・DEFAULT 補完・代入変換（SQL-51・Issue #2613）

### 列リスト省略

- `INSERT INTO t SELECT ...` と `INSERT INTO t (SELECT ...)` を受理する。構文段は省略印（`columns_omitted`）だけを持ち、
  展開は束縛（`bind`）が行う。`(` の直後が `SELECT` のときは列リストでなく括弧付き問い合わせとして扱い、
  問い合わせ全体を囲む 1 組の括弧は外して既存の SELECT 検証器へ渡す。`( WITH ...` は従来どおり `42601`。
- 展開は VALUES 形（#1730）と共有のヘルパー（`expand_omitted_insert_columns`）で、表の宣言順（疑似 `id` が先頭）の
  先頭から内側の出力列数だけを当てる。出力列が多ければ `42601`、少なければ残りは省略列として DEFAULT 補完へ回る。
  `id` が 2 回現れる表は `0A000`。展開後の列名は `BoundInsertSelectTarget.columns` で実行側へ渡り、
  実行側が実行時のスキーマで解決し直す（明示形と同じ TOCTOU 特性）。
- 台帳の内容照合ハッシュは送られた文のトークン列から算出するため、省略形と明示形は別の文として扱われる（既存挙動を変えない）。

### DEFAULT 補完

列リスト指定時の省略列は VALUES 形・COPY と共有の `fill_omitted_columns` で補う（リテラル・`now()` は文の時刻・
NULL 許容は NULL・DEFAULT の無い NOT NULL は `23502`）。`DEFAULT nextval`・IDENTITY の省略列は従来どおり `0A000`
（台帳ハッシュ差し替えとの相互作用と明示トランザクションの採番スコープに追加設計が要るため。fail-closed）。

### 代入変換

- 文ごとに 1 回、実行時の結果列メタから変換計画（`ConversionPlan`）を組む。各列は次のどちらか。
  - `Direct`: 従来の `cell_to_insert_value`（同一型・文字列系・JSON・VECTOR・整数幅・整数 → numeric/float・numeric → numeric）。値とエラーは不変。CTAS は全列 `Direct`。
  - `Assign`: それ以外の組。`ALTER COLUMN TYPE` の代入変換（SQL-47。`bind_alter_conversion`）と同じ束縛・評価器を使う。
    行ごとに変換元セルを作業用スキーマへ 1 回だけ encode して評価する。浮動小数 → 整数は偶数丸め、numeric → 整数は 0 から遠い側への四捨五入で、範囲外は `22003`。
- 変換の失敗は書き込みの前に文全体を拒否する（部分挿入なし）。評価・変換エラーは SQLSTATE を保った固定文言へ写し、
  行の値・他テナントの情報を載せない（`redact_row_conversion_error`）。
- 既知差分: 内部で整数を f64 経由で運ぶ変換（`cast_value`）のため、|v| > 2^53 の BIGINT は `22003` で拒否される（黙って丸めず拒否側）。
- 代替案（不採用）: セルから `ExprValue` へのアダプタを作り `CastFn` を直接適用する方式。既存ヘルパーが無く第 2 の変換表になるため採らない。

### 対象外（追跡候補）

省略列の `DEFAULT nextval`／IDENTITY の採番（COPY と共通）・`INSERT INTO t (WITH ... SELECT ...)`・ビューへの挿入・`ON CONFLICT` との併用。
