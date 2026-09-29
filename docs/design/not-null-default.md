# NOT NULL / DEFAULT 宣言構文の設計判断

Issue #904・対象ビヘイビア: TABLE-16（検討中）・TASK-204。関連ポインタ:
TABLE-5（`ALTER TABLE ADD COLUMN`）・SQL-23（`CREATE TABLE`。`docs/design/
sql-create-table.md`）・ERR-2／ERR-4／ERR-6（`23502` の共有と `code` ラベル
による区別）・RECOVER-10（`operation_id` 内容照合ハッシュ）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
CREATE TABLE <table> (<col> TEXT [NOT NULL] [DEFAULT <literal>] | <col> VECTOR(<dim>) [NOT NULL], ...) [;]
```

- `NOT NULL`・`DEFAULT <literal>` は列型の直後に、順序自由・各々最大 1 回まで
  受理する。重複指定（`NOT NULL NOT NULL`・`DEFAULT` 2 回）は `42601`。
- `<literal>` は既存の `INSERT ... VALUES` 要素パーサー
  （`sql::allowlist::Parser::expect_literal`）をそのまま再利用する（第 2 の
  リテラル文法を作らない）。`NULL` キーワードはこのパーサーが受理しないため、
  `DEFAULT NULL` は構造的に `42601` になる（TABLE-16: 明示 `NULL` は
  `DEFAULT` の対象外という契約と整合）。
- `TEXT` 列: `DEFAULT` は文字列リテラルのみ受理する（数値・真偽値リテラルは
  型不一致として `42601`）。
- `VECTOR` 列: `NOT NULL` の明示指定は受理するが冗長（元から
  `nullable = false`）。`DEFAULT` は禁止（`42601`）——`VECTOR` は増分索引
  反映（TASK-120）・ファイル形 `INSERT` がサーバー側で埋める値であり、
  クライアントが指定する既定値という概念と噛み合わないため。
- `NOT NULL`／`DEFAULT` は `lexer::Keyword` へ追加しない（`TEXT`／`VECTOR` と
  同方針。`Parser::peek_ident_matches`／`expect_contextual_keyword` で文脈的に
  照合し、同名の列名・テーブル名を壊さない）。

CREATE TABLE が受理する型は現状 `TEXT`／`VECTOR(N)` のみ（`docs/design/
sql-create-table.md` 参照）のため、`DEFAULT` が実質的に使えるのは `TEXT` 列
だけである。カタログ層（`catalog::ColumnDefault`）は `Number`／`Bool` も型と
して保持できるが、これは Rust API 直接構築や将来の型拡張（`INTEGER`・
`BOOLEAN` 等が `CREATE TABLE` へ露出した場合）に備えた設計であり、本 Issue の
時点では SQL 構文から到達しない。

## カタログ表現と永続化

- `catalog::ColumnDefault`（`Text(String)`／`Number(String)`／`Bool(bool)`）を
  新設し、`ColumnDef.default: Option<ColumnDefault>` として保持する。
  `row_codec::Value` を直接持たない（`Value::Real`/`Double` が `f64`/`f32` を
  持ち `Eq` を実装できないため、`ColumnDef` の `#[derive(Eq)]` を壊さずに
  済ませるための軽量な独自表現）。
- **破壊的変更**: `ColumnDef` への公開フィールド `default` の追加により、
  外部クレートが `ColumnDef { name, ty, nullable }` という完全な構造体
  リテラルで構築するコードはコンパイル不能になる。移行先は
  `ColumnDef::new(name, ty, nullable)`（引き続き `default: None` で構築。
  約 1,200 箇所の既存呼び出しは無変更のまま動く）と、新設の
  `ColumnDef::with_default(default)`。
- カタログのテキスト形式に **v5** を追加する（`v2`・`v3`〔TABLE-19・
  Issue #901〕・`v4`〔`PRIMARY KEY` 宣言専用フォーマット、TASK-204・
  Issue #903〕は無変更）。`DEFAULT` を 1 つでも持つスキーマは（`PRIMARY KEY`・
  `DROP COLUMN` の墓標の有無を問わず）v5 で書き、`cols:` 行の直後に `pk:`
  行を必ず 1 行持つ（主キー宣言が無ければ空のまま書き、decode 側はこれを
  「主キーなし」と解釈する。v4 の `pk:` 行は非空必須のまま変えない）うえで
  6 フィールド `name:tag:param:nullable:state:default` を 1 列 1 行に持つ
  （`state` は v3／v4 と同じ `L`／`D`。墓標行の `default` は常に `-`）。
  `DEFAULT` を持たないスキーマは従来どおり v2／v3／v4 のままバイト列を
  変えない（既存ゴールデンテストへの影響なし）。
- `default` フィールドの符号化: `-`（なし）／`t`・`f`（真偽値）／`s<hex>`・
  `n<hex>`（文字列・数値リテラルの本体を 16 進化）。`TEXT` の既定値は `:`・
  改行を含み得るため、カタログの区切り文字（`:`・改行）と衝突しないよう
  生のまま連結せず 16 進化してから連結する（`TABLE-6` の `param` フィールド
  検証と同じ「区切り文字注入の防止」の考え方）。16 進の符号化・復号は自作
  （依存追加なし。`.claude/rules/dependency-policy.md`）。
- 長さ上限 `catalog::MAX_COLUMN_DEFAULT_LEN`（1024 バイト）を構文検証・
  カタログ decode の両方でアロケーション前に検証する。16 進化後の長さ
  （最大 2 倍）× 列数上限（`MAX_COLUMN_COUNT` = 256）がカタログ値全体の上限
  （`MAX_CATALOG_VALUE_LEN` = 1 MiB）に十分収まることを `const _: () =
  assert!(...)` でコンパイル時に固定する。
- decode 側は v5 の一意性契約（`DEFAULT` を 1 つも持たない v5 値は不正。
  v3 の「墓標 0 件の v3 値は不正」〔TABLE-19 D2〕と同じ設計判断）・
  未知の符号化タグ・不正な 16 進・不正な UTF-8・墓標行への `DEFAULT` 混入を
  すべて fail-closed（`CatalogError::CorruptSchema`）に拒否する。
- `catalog::validate_schema` が `ColumnDefault::compatible_with` で列型の
  大分類（`TEXT` ↔ `Text`／数値型 ↔ `Number`／`BOOLEAN` ↔ `Bool`）との整合を
  検証する（`VECTOR` は常に不可）。SQL 表層の構文検証と Rust API 直接構築の
  両方から通る唯一の検査点であり、`sql` 層に依存しない（catalog は sql の
  下位レイヤであるという既存の依存方向を維持する）。

## 単一の適用点（`sql::parser::fill_omitted_columns`）

`INSERT` で列が省略された場合の `DEFAULT` 補完・NOT NULL 検査を、
`sql::parser::fill_omitted_columns`（`pub(crate)`）へ集約した。

- `INSERT`（SQL テキスト・NoSQL `insert` op が共有する `bind_insert_row`）・
  `COPY`（`sql::copy::bind_copy_record`）が本関数を直接呼ぶ。
- ファイル形 `INSERT`（`bind_file_insert`）は `VECTOR` 列（サーバー側が
  埋め込み結果で後から埋めるため、省略が正常系）を除外する専用ループを
  保ちつつ、同じ `DEFAULT` 補完・NOT NULL 検査ロジックを共有する。
- `UPSERT`（`bind_upsert_form`）は挿入側の各行を `bind_insert_row` へ委譲
  するため、追加の変更なしに同じ契約を継承する。
- `DEFAULT` を持たない省略列が非 nullable の場合は
  `SqlSurfaceError::not_null_violation`（`23502`）で拒否する。

`ColumnDefault` から実際の `row_codec::Value` への束縛（`bind_column_default`）
は、数値の精度・範囲検証を含め既存の `INSERT` リテラル束縛ヘルパー
（`bind_integer_literal`・`bind_real_literal`・`bind_numeric_literal` 等）へ
委譲し、第 2 の実装を作らない。

## 明示的な `NULL` と `DEFAULT` の非対称性（TABLE-16 の確定契約）

省略（列を指定しない）と明示的な `NULL` 指定は区別する。

| 入力 | nullable 列 | 非 nullable 列 |
| ---- | ----------- | --------------- |
| 省略・`DEFAULT` あり | 既定値を適用 | 既定値を適用（`23502` にならない） |
| 省略・`DEFAULT` なし | `NULL` | `23502` |
| 明示 `NULL` | `NULL`（`DEFAULT` は適用しない） | `23502` |

この非対称性は次の各経路で統一して実装する:

- `sql::parser::bind_insert_row`・`bind_upsert_assignments`・
  `bind_set_assignments`（UPDATE）: `(_, InsertLiteral::Null)` の分岐を
  「`column.nullable` なら `Value::Null`、そうでなければ
  `not_null_violation`」へ統一した（従来は列型を問わず一律拒否、または
  `invalid_input`〔`22000`〕としていた箇所を含む）。
- `sql::copy::bind_copy_record`: COPY テキスト形式の明示 NULL マーカー
  （`\N`）は列を省略した場合とは別経路のまま、`DEFAULT` を適用せずに
  同じ判定を行う。
- NoSQL 表層 `insert` op（`wire-server::http::query::insert::bind_row`）:
  従来は JSON `null` を一律「列の省略」として扱っていたが、`VECTOR` 列
  （`tenant::insert_typed_rows_unchecked` の既存契約により nullable の値に
  関わらず常に必須。`DEFAULT` は適用対象外のまま）を除き、
  `InsertLiteral::Null` として `bind_insert` へそのまま渡すよう変更した。
  `VECTOR` 列専用の必須判定ループ自体は `fill_omitted_columns` を経由しない
  ため、`wire_code` は列の `nullable` で分岐させている: 非 nullable な
  `VECTOR` 列の省略は SQL 表層（`fill_omitted_columns`）と同じ `23502`
  （`NotNullViolation`）、nullable な `VECTOR` 列の省略（PR #823 が導入した
  NoSQL 表層固有の「VECTOR は nullable でも常に必須」という別ルール。NOT
  NULL 違反ではない）は従来どおり `22000` のまま維持する。

`operation_id` の内容照合ハッシュ（RECOVER-10。`content_hash::
for_typed_insert`／`for_typed_insert_batch`）は束縛後（`DEFAULT` 適用後）の
値をハッシュするため、本変更後も同一文の再送判定は決定的なまま変わらない。

## エラー契約（ERR-6: `wire_code` の共有）

NOT NULL 違反は新設の `ErrorClass::NotNullViolation`（`code` ラベル
`NOT_NULL_VIOLATION`）へ写像する。`wire_code` は `23502` を
`ErrorClass::MissingOperationId`（`USING OPERATION_ID` 句の省略）と**共有**する
——本リポでは PostgreSQL の `not_null_violation` 相当コードを先に別用途へ
割り当て済みだったため、ERR-6 が認める「`wire_code` の共有と `code` ラベルに
よる区別」の枠組みで新設した。

- `error_format.rs::SHARED_WIRE_CODES`（`#[cfg(test)]` 専用。現状
  `{"23502"}` のみ）が、既存の「`wire_code` は分類ごとに一意」という
  単体テストの不変条件を「`SHARED_WIRE_CODES` に載る組み合わせを除き一意」
  へ緩和する唯一の許可リスト。偶発的な重複（`SHARED_WIRE_CODES` に載って
  いない `wire_code` の重複）は引き続き検出する。
- `ErrorClass::from_wire_code("23502")` は宣言順で最初の分類
  （`MissingOperationId`）を返す契約とし、doc に明記した。この関数は
  HTTP ステータス射影の往復確認にのみ使われ、応答本文の `code` ラベルは
  各エラー型の `error_class()` が直接返す分類（`wire_code` の逆引きを
  経由しない）から得るため、共有コードの逆引きが `code` ラベルの取り違えを
  起こすことはない（`wire-server::http::error_body::encode_inner`・
  `wire-server::error_response::encode` のいずれも `ErrorClass` を直接
  受け取る設計）。
- HTTP ステータス射影（`wire-server::http::status::http_status`）は
  `NotNullViolation → 400`（他の 4xx 系クライアントエラーと同じ）。

## `ALTER TABLE ADD COLUMN` との不整合の解消

`Storage::alter_table_add_column`（TABLE-5）は当初（Issue #904）、既存行へ読み出し時に
`DEFAULT` を補完する仕組みが無いため `nullable == false` と `default.is_some()` を
fail-closed で拒否していた。Issue #1169 で読み出し時補完（行は書き換えない）を実装し、
`NOT NULL`／`DEFAULT` を受理するようにした（詳細は
`docs/design/sql-alter-table-add-column.md`）。

`DEFAULT` が到達可能な列型は、本 Issue の時点では `TEXT` のみだったが、現在は
`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION`／`NUMERIC(p,s)`／`BOOLEAN` にも
対応している（`CREATE TABLE`／`ALTER TABLE ADD COLUMN` とも列型ごとの束縛は
`row_codec::column_default_scalar` に一本化）。

`ALTER COLUMN ... TYPE`（Issue #901）で型が変わる列に既存の `DEFAULT` が付いて
いた場合、`DEFAULT` は列定義に保持されたまま型だけが変わる。型変更後の
`DEFAULT` は、カタログのデコード時再検証（`validate_schema` の型と `DEFAULT` の
整合検証。`ColumnDefault::compatible_with`）が既存経路のまま効き、型と整合
しない場合は fail-closed で拒否する。`NUMERIC` の精度拡大のように値を変えない
変更では `DEFAULT` は整合したまま読める。読み出し時補完（`ADD COLUMN` 後の
既存行）も同じ `default_scalar` を通すため、不正な `DEFAULT` は参照列の選択に
よらず検出される。

## スコープ外・後続 Issue

- （Issue #1169 で実装済み）`ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT ...` の受理と、
  既存行への読み出し時の `DEFAULT` 補完（TABLE-5・TABLE-16）。
- `ALTER COLUMN SET/DROP DEFAULT`・`SET/DROP NOT NULL`。
- SQL `VALUES` 内の `NULL` リテラルと `DEFAULT` キーワード（`INSERT INTO t
  (...) VALUES (DEFAULT, ...)` 形）。
- NoSQL 表層の DDL op（`PRIMARY KEY`・`UNIQUE`・`CHECK`・`FOREIGN KEY` は
  Issue #903・#905・#906・#907 で実装済み）。
- `CREATE TABLE` の型拡張（`TEXT`／`VECTOR` 以外。TABLE-13／14）。
- Rust API（`tenant::insert_typed_row` 等）の NOT NULL 拒否の分類変更（現状
  の `row_codec` 由来の分類を維持）と、Rust API での `DEFAULT` 適用（Rust
  API は列挙位置指定の `Value` を直接受け取るため「省略」の概念が無く、
  `DEFAULT` は適用しない）。
- `23505` の `UNIQUE_VIOLATION`／`DUPLICATE_OPERATION_ID` ラベル分離
  （ERR-6。本 Issue の `SHARED_WIRE_CODES` の仕組みを再利用できる）。
