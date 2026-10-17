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
- `<literal>` は共通のリテラルパーサー
  （`sql::allowlist::Parser::expect_literal`）をそのまま再利用する（第 2 の
  リテラル文法を作らない）。`expect_literal` は大小無視の `NULL` を
  `InsertLiteral::Null` として受理する単一の入口で、`INSERT ... VALUES` の要素
  （Issue #1523）・`UPDATE ... SET`・`ON CONFLICT ... DO UPDATE SET`
  （Issue #1524）・`DEFAULT` が共有する（SQL-41）。`DEFAULT NULL` は既定値の宣言
  なしと同義とし、`parse_column_constraints` が `default: None` へ正規化する
  （重複 `DEFAULT` の検出は専用フラグで行うため `DEFAULT NULL DEFAULT 'x'` も
  `42601`）。したがって `NOT NULL DEFAULT NULL` の列は省略 INSERT で `23502`、
  `VECTOR(N) DEFAULT NULL` は省略時と同じ扱い（non-nullable のまま）になる。
  `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT NULL` は既定値なしの `NOT NULL` と
  同じなので従来どおり `42601`（fail-closed）。`-NULL`／`+NULL` は `42601`。
- `UPDATE ... SET col = NULL`（単一行形・述語形・分割実行）と
  `DO UPDATE SET col = NULL` は束縛（`bind_set_assignments`／
  `bind_upsert_assignments`）が nullable 列なら `Value::Null`、そうでなければ
  `23502` に解決する。束縛はスキーマだけを見る判定で対象行の探索・RLS 可視性より
  前に終わるため、`23502` は対象行の有無で分かれない。PostgreSQL は NOT NULL 検査を
  行ごとに実行時に行い一致行 0 件なら `UPDATE 0` になるが、本実装は束縛時に拒否する
  （NoSQL 表層と同じ既存契約。既知の差異）。分割実行も束縛が分割実行器より前の
  ため、拒否時は chunk の部分 commit もジョブ登録も起きない。
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

## エラー契約（`wire_code` の共有。ERR-6 ポインタ）

NOT NULL 違反は新設の `ErrorClass::NotNullViolation`（`code` ラベル
`NOT_NULL_VIOLATION`）へ写像する。`wire_code` は `23502` を
`ErrorClass::MissingOperationId`（`USING OPERATION_ID` 句の省略）と**共有**する
——本リポでは PostgreSQL の `not_null_violation` 相当コードを先に別用途へ
割り当て済みだったため、`wire_code` を共有し `code` ラベルで区別する形で
新設した（ERR-6 ポインタ）。

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
- `ALTER COLUMN SET/DROP DEFAULT`・`DROP NOT NULL`。（`SET NOT NULL` は Issue #1631 で実装済み）
- （Issue #1523 で実装済み）SQL `VALUES` 内の `NULL` リテラル。
- （Issue #1524 で実装済み）`UPDATE ... SET`・`DO UPDATE SET`・`DEFAULT` の `NULL`。
  VECTOR 列への NULL（`SET` は `22000`・`DO UPDATE SET` は `23502` で不揃い）の整理と
  nullable 化は TABLE-26（TASK-252）の範囲。
- （Issue #1525 で実装済み）NULL を含む単一行 UPDATE の `content_hash`（NULL 列を
  列数・本体に含める）。詳細は [update-single-row.md](update-single-row.md)。
- SQL `VALUES` 内の `DEFAULT` キーワード（`INSERT INTO t (...) VALUES
  (DEFAULT, ...)` 形）。
- NoSQL 表層の DDL op（`PRIMARY KEY`・`UNIQUE`・`CHECK`・`FOREIGN KEY` は
  Issue #903・#905・#906・#907 で実装済み）。
- `CREATE TABLE` の型拡張（`TEXT`／`VECTOR` 以外。TABLE-13／14）。
- Rust API（`tenant::insert_typed_row` 等）の NOT NULL 拒否の分類変更（現状
  の `row_codec` 由来の分類を維持）と、Rust API での `DEFAULT` 適用（Rust
  API は列挙位置指定の `Value` を直接受け取るため「省略」の概念が無く、
  `DEFAULT` は適用しない）。
- `23505` の `UNIQUE_VIOLATION`／`DUPLICATE_OPERATION_ID` ラベル分離は Issue #1180 で
  実装済み（ERR-6。`SHARED_WIRE_CODES` の仕組みを再利用）。

## Issue #1631 追記: `ALTER COLUMN SET NOT NULL`

ポインタ: TABLE-39 (3)・TABLE-22 (e)・TABLE-26・ERR-6、TASK-277。

- **構文**: `ALTER TABLE <表> ALTER COLUMN <列> SET NOT NULL [;]` のみ受理する。`COLUMN` 省略形、
  `SET DEFAULT`／`DROP DEFAULT`／`DROP NOT NULL`／`SET DATA TYPE`、カンマ区切りの複数アクションは
  従来どおり `42601`。予約列名は `42601`。
- **検証**: 単一の write トランザクション（書き込みロック保持中）で、全テナント・全可視性の既存行を
  走査し、対象列に NULL が 1 行でもあれば `23502`。行数上限は設けない。**変更前スキーマ**で走査する
  （`ADD COLUMN`〔DEFAULT なし〕より前の行は列バイトが無く、変更後スキーマだと decode エラーになる
  ため）。拒否時はカタログ・世代・行とも不変。応答には列名のみを含める。
- **no-op**: 既に NOT NULL の列（PK 構成列・VECTOR 列を含む）は commit・世代 bump なしで成功する。
  NULL 許容の VECTOR 列に到達した場合は検査手段が無いため `42601` で fail-closed とし、TABLE-26 の
  後続 Issue で埋め込みの有無を見る検査へ置き換える。
- **FK との整合**: 変更後スキーマで FK 参照アクションの宣言時検査を再実行し、`ON DELETE／UPDATE
  SET NULL`（または DEFAULT なしの `SET DEFAULT`）を持つ FK 構成列は `42830` で拒否する。
  `ON UPDATE CASCADE` の子列を NOT NULL にして親列が nullable になる組も `42830`。
- **エラー写像**: 表なし `42P01`／ビュー等 `42809`／列なし `42703`／書き込みロック待機超過 `55P03`／
  NULL 行あり `23502`／FK 不整合 `42830`／内部矛盾 `XX000`。
- **PostgreSQL との既知差分**: (1) `23502` の文言は既存の NOT NULL 違反と同じ
  （`null value in column "c" violates not-null constraint`）。(2) FK 参照アクションとの不整合を
  DDL 時点で `42830` として拒否する（PostgreSQL は実行時に `23502`）。(3) 引用符付きの `"NULL"` も
  キーワードとして受理される（ALTER TABLE の構文段が引用符付き識別子を区別しないため）。
- **範囲外**: `DROP NOT NULL`（#1632）、`SET／DROP DEFAULT`、`COLUMN` 省略形、複数アクション、
  NoSQL 表層。

## Issue #1628 追記: `ALTER COLUMN SET／DROP DEFAULT`

ポインタ: TABLE-39 (1)(2)(6)・TABLE-5・TABLE-16・TABLE-20・ERR-6、Issue #1627（補完値の分離）。

- **構文**: `ALTER TABLE <表> ALTER COLUMN <列> SET DEFAULT <式>`／`... DROP DEFAULT`。式の許可範囲は
  `CREATE TABLE` の列 DEFAULT と同じ（定数リテラル・定数式・時刻値関数・`NULL`）。`SET DEFAULT NULL`
  は `DROP DEFAULT` と同じ扱い（SQL-41 の正規化）。`COLUMN` 省略形・`DROP NOT NULL`・カンマ区切りの
  複数アクションは `42601` のまま。
- **変えるもの／変えないもの**: 変えるのは以後の書き込みで使う INSERT 既定値（`default_expr`）だけで、
  ADD COLUMN 時に決めた既存行の読み出し時補完値（`fill_value`）は変えない。食い違うスキーマは
  カタログ v14 で永続化される。変更時はテーブル単位の世代を進める。
- **判定順序**: 表の存在（`42P01`／`42809`）→ 列の存在（`42703`）→ 既定値の生成（型不一致 `42804`）
  → TIMESTAMPTZ の正規化（`default_expr` のみ）→ 値の束縛検証（`22003`／`22P02` 等）→ 時刻既定値と
  FK `SET DEFAULT` の組（`0A000`）→ カタログ更新（単一 write txn 内で列型一致・再検証・FK 宣言整合
  `42830`）。拒否はすべて commit 前で、カタログ・世代・行に痕跡を残さない。
- **D1 型不一致は SET DEFAULT でだけ `42804`**: `ADD COLUMN`／`CREATE TABLE` の種別不一致 `42601` は
  既存契約（TABLE-16、SQL-45 の確定待ち）のため据え置く。変換規則は共通化し、不一致エラーの生成だけ
  を経路ごとに差し替える。DEFAULT を付けられない型（VECTOR 等）の `0A000`、長さ超過 `54000`、
  VARCHAR(n) 超過 `22001`、書式不正 `22P02`／`22007`／`22008`、範囲外 `22003` は両経路で同一。
- **D3 時刻値関数のミラー**: `CREATE TABLE ... DEFAULT now()` の列は `fill_value == default_expr ==
  CurrentTime`（ミラー）。`fill_value` は定数に限る不変条件のため、SET／DROP で `default_expr` を変える
  ときは `CurrentTime` の補完値を `None` に落とす。`ADD COLUMN` は時刻値関数の DEFAULT を拒否するので
  `CurrentTime` の補完値は CREATE TABLE 由来に限られ、その列の全行は物理的に値を持つため補完値は
  参照されない。定数のミラーの `fill_value` は変えない。
- **D4 冪等**: 変更後の `default_expr` が現在値と等しければ commit・世代 bump なしで成功する
  （既定値の無い列への DROP を含む。PostgreSQL と同じ）。
- **D5 FK 参照アクション**: 変更後スキーマで宣言時検査を再実行する。`ON DELETE／UPDATE SET DEFAULT`
  で参照される NOT NULL 列への DROP DEFAULT は `42830`、同じ列への時刻値関数の SET DEFAULT は
  SQL 表層で `0A000`（カタログ側は多層防御で `42830`）。
- **D6 並行 DDL**: 既定値は write txn の外で読んだ列型で作るため、カタログ API は期待する列型を受け取り、
  write txn 内の列型と一致しなければ `Invalid`（`42601`）で拒否する。
- **PostgreSQL との既知差分**: (1) ADD COLUMN の型不一致は `42601`、SET DEFAULT は `42804`。
  (2) PostgreSQL は DROP DEFAULT を受理し FK の `SET DEFAULT` 実行時に `23502` を返すが、本 DB は宣言時
  に `42830` で拒否する。(3) DEFAULT 式の列参照の `42P10`・部分クエリの `0A000` など SQL-45 の範囲は
  CREATE TABLE と同じ現状のまま。
- **範囲外**: 各書き込み経路（INSERT／UPSERT／COPY／NoSQL／`UPDATE ... SET c = DEFAULT`）が新しい
  既定値を使うことの網羅的検証と content_hash／台帳の整合（#1629）、`DROP NOT NULL`（#1632）、
  `COLUMN` 省略形・複数アクション、NoSQL 表層、wire-server 側の結合テスト。
