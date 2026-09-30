# NoSQL 表層の型付き JSON 束縛（Issue #896）

対象ビヘイビア: NOSQL-17（本ドキュメントの束縛表の SSOT）。関連: NOSQL-2〜NOSQL-6・NOSQL-7・NOSQL-11。ポインタ: `docs/spec/05-tasks.md` TASK-175〜TASK-178・`docs/spec/04-behavior/nosql-surface.md` NOSQL-17。

## 背景

列型は #880〜#890 で INTEGER/BIGINT/REAL/DOUBLE/NUMERIC/BOOLEAN/DATE/TIMESTAMP/UUID/ARRAY まで拡張済みだったが、NoSQL 表層（`POST /v1/query` の `insert`／`update`／`filter`）の JSON 束縛は TEXT/VECTOR/ENUM/BYTEA/JSON/JSONB にしか対応していなかった。本 Issue でこの JSON ⇄ 列型の束縛規則を型ごとに閉じた形で固定し、SQL 表層と同じ engine 束縛経路を共有したまま新型の insert・update・応答を通す。

## 設計方針

- **第 2 の実行器を作らない**: JSON 値の解析・範囲検証（整数のオーバーフロー・`NUMERIC` の桁あふれ・`DATE`／`TIMESTAMP` の暦上妥当性・`UUID` の文法等）は一切 wire-server 側で再実装せず、SQL 表層と共有する `engine::sql::parser::bind_insert`／`bind_update`（`pub fn`）へ委譲する。
- **insert の束縛経路**: `insert` op は JSON 行 1 件ごとに `engine::sql::allowlist::ValidatedInsert`（`rows: vec![単一行]`）を組み立て、`bind_insert(&validated, schema)` を呼ぶ（`crates/wire-server/src/http/query/insert.rs::bind_row`）。`bind_insert` は単一行専用の公開関数であり、複数行 `VALUES`・`ON CONFLICT` を明示的に拒否するため、`insert` op のバッチ（`rows` 配列）は行ごとに独立して呼び出す。
- **update の束縛経路**: `update` op は既存どおり `engine::sql::allowlist::InsertLiteral` 列（`Vec<(String, InsertLiteral)>`）を組み立てて `bind_update(&stmt, schema)` を呼ぶ（変更前から同じ形）。新型は `map_set_assignments` の型別分岐を追加するだけで足りた。
- **共有モジュール**: `crates/wire-server/src/http/query/typed_json.rs` を新設し、「JSON 値 → `InsertLiteral`」の型別写像（`map_json_to_literal`）と wire 固有の符号化（BYTEA の base64 ⇄ hex テキスト、`ARRAY` 列の JSON 配列 ⇄ `{...}` テキスト、`JSON`／`JSONB` の正規化）を集約する。値の解析（範囲・形式）はしない。

## 束縛表（本リポの実装既定値。SSOT は NOSQL-17）

| 列型 | 受理する JSON | 型不一致 | 備考 |
| --- | --- | --- | --- |
| INTEGER / BIGINT | `PosInt`/`NegInt`（整数リテラル）。`u64`/`i64` に収まらない整数リテラル（`JsonNumber::Float` へフォールバックした、小数点・指数部を含まないもの）も整数リテラルとして扱う | `42601` | 小数・指数表記・非数値は拒否。範囲外（`u64`/`i64` を超える整数リテラルを含む）は `bind_insert`/`bind_update` が `22003` |
| REAL / DOUBLE PRECISION | 数値 | `42601` | 範囲外は `22003`。指数表記は受理（Issue #1187） |
| NUMERIC(p,s) | 数値、または数値文字列 | `42601` | 桁あふれは `22003`、形式不正は `22P02`（Issue #1187） |
| BOOLEAN | 真偽値 | `42601` | |
| DATE / TIMESTAMP | 文字列 | `42601` | 書式違反 `22007`、範囲外 `22008`（Issue #1187） |
| UUID | 文字列 | `42601` | 形式不正 `22P02` |
| ARRAY（text[] / boolean[]） | 配列 | `42601` | 要素種別不一致・入れ子・配列以外は `42601`。要素数超過は `54000`（テキスト組み立て前に検査）。JSON `null` 要素は unquoted `NULL` として出力し engine の `22000` に任せる |
| BYTEA（既存） | 標準 base64 文字列 | `42601` | 長すぎは `54000`（既存契約） |
| JSON / JSONB（既存） | オブジェクト・配列 | `42601` | 長すぎは `54000`（既存契約） |
| ENUM（既存） | 文字列 | `22P02`（語彙外） | 非文字列は `42601` |
| TEXT / VECTOR（旧来型） | 文字列 / 数値配列 | **`22000`** | 既存コードを据え置く（`nosql-api.md` が `22000` と明記済みのため。新型との非対称はオーナー確認事項） |
| 全型共通（TEXT／ENUM を除く） | `null` | — | insert: 列を省略（nullable 列は `NULL`、非 nullable 列は「値が提供されていない」として `22000`）。update: `InsertLiteral::Null` をそのまま渡し `bind_update` の nullable 判定へ委譲 |
| TEXT／ENUM（`update` op） | `null` | TEXT: `22000`／ENUM: `42601` | Issue #896 以前の契約を維持し `nullable` 属性に関わらず一律拒否する（下記「null の扱い」節参照） |

数値は `f64` を経由しない（`JsonNumber::PosInt`/`NegInt` は `to_string()`、`Float` は保持済みの生テキストをそのまま使う）。SQL 表層のリテラルパーサーと同一の「テキストから直接解釈する」経路に載せることで、表層を跨いだ `content_hash`（TASK-101・RECOVER-10）の一致を保つ（`update.rs::vector_literal_text` が確立していた設計を `typed_json::number_literal_text`／`vector_literal_text` へ一般化した）。

## null の扱い（挙動変化）

Issue #896 導入前は `update` op の JSON/UUID 列 `null` 分岐を wire 層が個別に判定しており（nullable 列は `InsertLiteral::Null`、非 nullable 列は `42601`）、他の型は「非対応」として拒否していた。本 Issue で `map_json_to_literal` は **`null` を列型を問わず一律 `InsertLiteral::Null` へ写像**し、nullable 判定は engine 側の単一情報源（`bind_set_assignments` の `(_, InsertLiteral::Null) if column.nullable => Value::Null` / それ以外は `22000`）に委譲するよう一般化した。

その結果、**非 nullable 列への `null` の拒否コードが `42601` から `22000` へ変わった**（`wire_json_column.rs::nosql_update_set_json_null_on_non_nullable_column_is_rejected` で固定。旧テスト名 `..._is_rejected_with_42601` から改名）。拒否されること自体・fail-closed であることは不変。

**解消済み（PR #1038 レビュー指摘）**: 一般化の副作用として、当初これまで `update` op が明示的に拒否していた `nullable` な `TEXT`／`ENUM` 列への JSON `null` が成功するように変わってしまっていた（`map_json_to_literal` が `null` を列型を問わず `InsertLiteral::Null` へ写像し、nullable 判定を `bind_update` へ委譲するため）。契約変更にはオーナー承認・spec 改訂・BREAKING CHANGE 告知が別途必要であり、本 PR の時点ではそれらが揃っていないため、安全側として `TEXT`／`ENUM` 列は `map_json_to_literal` の冒頭で `null` を列型固有のエラー（TEXT: `LegacyMismatch`＝`22000`、ENUM: `TypeMismatch`＝`42601`）として拒否する特例を追加し、`nullable` 属性に関わらず Issue #896 以前の拒否契約を維持するよう修正した。`{"set":{"lang":null}}`（`lang` が nullable な `TEXT`）は導入前・導入後を通じて一貫して拒否される（`typed_json.rs::tests::rejects_null_for_text_column_even_when_nullable`／`rejects_null_for_enum_column_even_when_nullable`・`update.rs::tests::map_set_assignments_rejects_null_for_nullable_text_column` で固定）。`TEXT`／`ENUM` 以外の型（`bind_update` へ nullable 判定を委譲する一般化そのもの）は変更していない。

`insert` op は `bind_insert_row` が明示 `NULL` リテラルを列型を問わず一律拒否する契約（SQL テキストの `INSERT` 構文からは `NULL` リテラルが構築されない到達不能パスのため）を踏まえ、JSON `null` の列は **`ValidatedInsert.columns` から省略**する（値を丸ごと省略した場合と同じ扱いに統一。nullable 列は `Value::Null` で埋まり、非 nullable 列は「値が提供されていない」で `22000`）。`VECTOR` 列は `nullable` の値に関わらず常に必須という既存契約（PR #823）を維持する。

**未確定事項ではないが挙動変化**: `insert` op の `bind_row` は行の各 JSON キー（`id` 除く）に `ident::check_identifier`（NUL・制御文字・63 文字上限等の識別子形状検査）を先に適用してから列名解決するよう変更した（`update.rs::execute` の `set` キー検査・`table` フィールドの既存検査と同じ判断に揃えた）。本 Issue導入前はこの検査が無く、NUL を含む列名は engine 側の「未知列」判定（`22000`）まで素通りしていた。導入後は `42601` で先に拒否する（`insert.rs::tests::bind_rows_rejects_column_key_containing_nul_as_invalid_identifier` で固定）。

## columns[].type（応答の型名。BREAKING CHANGE）

`crate::result_encoder::column_wire_type`／`WireType`（SQL wire `RowDescription` の OID 写像。Issue #895 の担当。**本 Issue では変更していない**）は、後方互換のため多くの新型を `text`（OID 25）へ丸める。NoSQL の JSON API は型情報をそのまま伝える方が有用なため、`crates/wire-server/src/http/query/response.rs::nosql_type_name` を **SQL wire とは独立の対応表**として新設した:

| 列型 | `columns[].type` |
| --- | --- |
| `id`（疑似列） | `"numeric"`（SQL wire と一致。`column_wire_type(meta).pg_type_name()` へ委譲し二重管理を避ける） |
| 式項目・集計結果（`Computed`） | `"text"` 固定（NOSQL-11。SQL wire の `RowDescription` は Issue #1173 で型付き OID を公告するが、本表層は意図的に据え置く） |
| TEXT | `"text"` |
| VECTOR | `"vector"`（旧: `"text"`） |
| INTEGER | `"integer"`（旧: `"int4"` 相当だったが SQL wire は既に `pg_type_name()` で `"int4"` を返しており、NoSQL 側は本 Issue で独立の `"integer"` へ変更） |
| BIGINT | `"bigint"` |
| REAL | `"real"` |
| DOUBLE PRECISION | `"double precision"` |
| BOOLEAN | `"boolean"` |
| DATE | `"date"` |
| TIMESTAMP | `"timestamp"` |
| NUMERIC | `"numeric"` |
| UUID | `"uuid"`（旧: `"text"`） |
| BYTEA | `"bytea"`（旧: `"text"`） |
| JSON / JSONB | `"json"` / `"jsonb"`（旧: `"text"`） |
| ENUM | `"enum"`（旧: `"text"`。語彙一覧は含めない） |
| ARRAY | `"text[]"` / `"boolean[]"`（旧: `"text"`） |

`BIGINT` の投影値（`Cell::SignedInteger`）は `±(2^53-1)`（`Number.MAX_SAFE_INTEGER`）を超える場合のみ JSON 文字列で送出するよう変更した（JS 系クライアントの `JSON.parse` による精度誤解を防ぐ）。`Cell::Integer(u64)`（`id`／`COUNT`）は TASK-185 の担当のまま変更していない。

## filter（`eq` の型別レーン）

`filter[].value` のスキーマを `FieldType::Scalar`（文字列・数値・真偽値。`crates/wire-server/src/http/query/schema.rs`）へ広げ、`crates/wire-server/src/http/query/filter.rs::bind_filter` が対象列の型に応じて `eq` を以下のレーンへ振り分ける（`prefix` は従来どおり `TEXT` 列限定のまま変更なし）:

| 列型 | `eq` の写像 | 値・型不一致 |
| --- | --- | --- |
| TEXT（旧来型） | `DeclarativeFilter::equals` | `42601`（`TypeMismatch`。本 Issue 導入前からの filter 既存契約をそのまま維持——insert/update の「TEXT は旧来型 = `22000`（`LegacyMismatch`）」非対称は filter には適用しない） |
| ENUM | `DeclarativeFilter::equals`（語彙照合は engine 側 `bind` が `22P02` で行う） | `42601` |
| BOOLEAN | `DeclarativeFilter::bool_equals` | `42601` |
| DATE / TIMESTAMP / UUID | `DeclarativeFilter::compare`（`CompareOp::Eq`。形式・範囲検証は engine 側へ委譲） | `42601` |
| BYTEA | base64 → hex（`typed_json::bytea_literal_text`）→ `compare` | `42601`／`54000`（下記「BYTEA の実効長（PR #1038 是正）」参照） |
| NUMERIC | 数値または数値文字列 → `compare_numeric_literal`／`compare` | `42601` |
| INTEGER / BIGINT / REAL / DOUBLE PRECISION | 対象外（`0A000`） | — |
| VECTOR / ARRAY / JSON / JSONB | 従来どおり engine 側「`TEXT` 列でない」判定へ委譲 | `22000` |
| 未知列 | 列名だけで完結する判定のため値に関わらず engine 側「unknown column」へ委譲 | `22000` |

`scan.rs` はこれまで `filter` を schema 到達前に未束縛の `DeclarativeFilter` として宣言し、schema 到達後に `declarative_filter::bind_all` で束縛する二段構成だったが、列型別レーンの振り分けに `schema` が必須なため、`search.rs`／`aggregate.rs` と同じ「schema 到達後に `bind_filter` を単一段で呼ぶ」構成へ揃えた。

`engine::sql::allowlist::SqlSurfaceError` へ `FeatureNotSupported { detail }`（`0A000`）variant を追加し（レビュー指摘対応。以前はこの variant が無く、`scan`／`search`／`aggregate` の束縛 closure〔`Result<_, SqlSurfaceError>` 契約〕を通る際に `FilterError::NumericFilterNotSupported` が `42601`〔`UnsupportedSyntax`〕へ縮退していた）、`bind_filter` を直接呼ぶ層 A テストと HTTP 経由の実応答のいずれも `0A000` を観測する。数値列への `eq` を式レーン（`udf_call::bind_expr`）経由で扱う対応自体は、`BoundStatement`／`PlanSearchBinding` に `expr_filters` を渡す入口が無いため引き続き Issue #945 へ申し送る。

**BYTEA の実効長（PR #1038 是正）**: `bytea_literal_text` 自身は復号後のバイト列を `insert`／`update` と同じ [`engine::bytea::MAX_BYTEA_FIELD_LEN`]（4 MiB）まで許容するが、当初 `declarative_filter::bind_typed_compare_literal` は `DeclarativeFilter::compare` の `Bytea` 分岐で `bind_bytea_literal` 呼び出しの前に `check_literal_len` を掛けており、再エンコード後の hex テキスト（`\x` 接頭辞＋2 バイト/オクテット）の長さを `MAX_TEXT_FIELD_LEN`（復号後基準の `MAX_BYTEA_FIELD_LEN` と同値）でそのまま検査してしまうため、復号後 約 2 MiB を超える値は `54000` になっていた（PR #1038 レビュー指摘）。`bind_bytea_literal`（`bytea::parse_hex_text`）自身が確保前に**復号後バイト長**で `MAX_BYTEA_FIELD_LEN` 超過を判定するため、`Bytea` 分岐の `check_literal_len` 事前検査は不要かつ有害と判断し撤去した。是正後は insert／update／`WHERE bytea_col = '\x...'`（SQL・NoSQL いずれも同じ `declarative_filter::bind` 経路）の全経路で復号後 4 MiB を実効上限として揃えている。

## テスト

- 層 A: `crates/wire-server/src/http/query/typed_json.rs`（単体）・`insert.rs`／`update.rs` の `mod tests`（新型の bind・end-to-end 実行・`operation_id` 契約）・`crates/wire-server/src/http/query/filter.rs`（型別レーン単体）・`crates/wire-server/src/http/query/response.rs`（`nosql_type_name`・`BIGINT` の閾値）。
- 層 A（Issue #1203）: `crates/wire-server/tests/nosql17_typed_json_parity.rs`（NUMERIC/BOOLEAN/DATE/TIMESTAMP/UUID/ARRAY〔text・boolean・integer・bigint・double precision・date・timestamp・uuid 要素〕の HTTP `insert`→`scan` 往復と `aggregate`〔COUNT/SUM/AVG/MIN/MAX・`group_by`・`having`・`filter`〕が SQL 表層とバイト一致することを固定。RLS 境界・拒否契約〔`wire_code`・副作用ゼロ・エラー本文へ値を echo しない〕を含む）。
- 層 A（既存の反転）: `wire_integer_bigint_column.rs`・`wire_float_columns.rs`（旧 `22000` 拒否テストを成功＋SQL 表層との読み戻しパリティへ反転）・`wire_json_column.rs`（非 nullable 列 `null` の拒否コード変更を反映）・`nosql11_response_schema.rs`（SQL wire OID と NoSQL 型名の意図的な乖離を固定）・`three_client_http_e2e.rs`（SQL/NoSQL パリティの型名比較を列名ベースの独立表へ変更。層 B・opt-in）。
- 層 B: `crates/wire-server/tests/nosql7_filter_mapping.rs`（`search`／`scan`／`aggregate` が同一の `bind_filter` 束縛結果を共有すること・SQL 表層との一致を固定。既存）。

## 対象外・申し送り

- `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `filter` `eq`（式レーンの入口が無いため）→ Issue #945。
- `22P02`／`22007` への統一（形式エラーの分類統一）: Issue #1187 で解消（INTEGER／BIGINT の小数、BYTEA の不正 base64 も `22P02`）。
- TIMESTAMP 応答の区切り文字（空白／`T`）・NUMERIC の指数表記が `22P02` になる点は（REAL/DOUBLE の指数表記は Issue #1187 で受理）、SQL 表層と同じ既知の制約のまま。
- `RowDescription` の OID 写像（`result_encoder.rs`）は Issue #895 の担当のまま変更していない。
- nullable な `TEXT`／`ENUM` 列への `update` op の JSON `null` 受理拡大は PR #1038 レビュー指摘により見送り、Issue #896 以前の拒否契約を維持する形へ修正済み（上記「null の扱い」節）。
- クロスサーフェス `23505`（再送同一性）テストは INTEGER/REAL/NUMERIC/ARRAY/DATE では未追加（TEXT/VECTOR のみ既存）。

## レビュー対応（PR #1038）

`VECTOR` 列を `insert`／`update` op の JSON 配列から束縛する際、上記「テキストから
直接解釈する」設計（`typed_json::vector_literal_text` → `InsertLiteral::String` →
`engine::sql::parser::parse_vector_literal`）が SQL 表層のテキストリテラル長上限
（64 KiB。`MAX_VECTOR_LITERAL_BYTES`）を経由してしまい、宣言次元が大きく JSON 配列
としては妥当でもテキスト表現が 64 KiB を超えるベクトルを `54000` で誤って拒否する
退行があった（cursor Bugbot 指摘）。`typed_json::vector_literal_values` が JSON 配列
から `f32` 列を直接構築し、新設した `engine::sql::allowlist::InsertLiteral::Vector`
（NoSQL 表層専用 variant。SQL テキスト・COPY・ファイル形 `INSERT` からは構築されない）
としてテキストリテラルを経由せず束縛するよう変更した。次元一致・各要素の有限性は
engine 側（`sql::parser::bind_vector_literal_values`）で再検証し、`content_hash` の
`push_dml_assignments`（述語つき `UPDATE ... WHERE` 用。本 variant は現状 NoSQL
表層からは到達しない）にも前方ガードのタグを追加した。
