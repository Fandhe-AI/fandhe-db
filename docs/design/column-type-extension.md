# `ColumnType` へスカラー型を追加する基盤整備（Issue #880）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-1, TABLE-2,
  TABLE-6, TABLE-7, TABLE-13・TASK-85, TASK-86, TASK-196

## 背景・目的

`catalog::ColumnType` は `Text`／`Vector(u32)` の 2 値、カタログのテキスト形式は
`v1` のみだった。後続 Issue（#881〜#883 ほか）で `INTEGER`／`BIGINT`／`REAL`／
`DOUBLE`／`BOOLEAN` 等のスカラー型を追加する際、型ごとの処理が個別に手書きで
重複しないよう、型を 1 つ足すときに触る箇所を最小化する基盤を整える。

## 決定

### D1: `#[non_exhaustive]`・ワイルドカード腕を導入しない

`ColumnType`・`row_codec::Value` に `#[non_exhaustive]` を付けず、新設・既存の
match にもワイルドカード腕（`_ =>`）を入れない。variant を追加したとき、
コンパイラが全ディスパッチ地点を列挙することが基盤の価値になる。ワイルドカード
腕は、新型が既存分岐へ黙って流れる fail-open の温床になる。

### D2: 型タグ↔`ColumnType` の往復を関連関数 1 対へ集約

`ColumnType::catalog_fields(&self) -> (&'static str, String)` と
`ColumnType::from_catalog_fields(tag: &str, param: &str) -> Result<ColumnType>`
に集約し、`encode_schema`／`decode_schema_body` はこの 1 対だけを呼ぶ。型追加時に
カタログ側で触る箇所を 1 対に限定する。

### D5: カタログ v2 の文法

`v1` と同じ `name:tag:param:nullable` の 4 フィールドを保つ。`param` は
「パラメータなし型は `-`、それ以外は型ごとの文法」へ汎用化した。許容文字は
`[A-Za-z0-9_,]` に閉じ（`validate_catalog_param`）、encode・decode の両側で
検証する。`v2` で `TEXT`／`VECTOR` の列行バイト列は `v1` と完全に同一で、
変わるのは 1 行目のバージョン識別子のみ（`encode_schema_golden_v2_layout` で
バイト列を固定）。

### D6: `v1` は fail-closed に拒否（マイグレーションなし）

`v1` のカタログ値は未知バージョンと同じ `CatalogError::CorruptSchema` で拒否
する（`ROW_FORMAT_VERSION` と同じ方針）。SQL 表層では既存の `table_lookup_error`
を通り `XX000`／汎用メッセージに丸められるため、`wire_code` の写像・HTTP 射影は
無変更。

### D7: 未知型タグ・余剰行の確保前拒否

`decode_schema_body` は、宣言列数（`cols:`。事前に `MAX_COLUMN_COUNT` 以下と
検証済み）を超えない範囲でのみ `ColumnDef`（内部で `String` を確保する）を
構築する単一走査へ再構成した。旧実装の `lines.collect()` は残り行数が宣言列数と
無関係に無制限へ膨らむ攻撃入力（大量の短い行）に対して行数比例のアロケーションを
先に行っていたが、本実装は列ごとに「識別子検証 → `param` 文字集合検証 →
`ColumnType::from_catalog_fields`（未知タグ拒否）→ nullable 検証 →
`ColumnDef::new`（ここで初めて `String` を確保）」の順で処理し、未知の型タグ・
不正な `param` はその列の `ColumnDef` を構築する前に拒否する。宣言列数を超える
行は「末尾の空行（トレーリング改行）1 行のみ」を許容し、それ以外は余剰行として
拒否する。

### D9: `is_vector()` ヘルパ

`ColumnType::is_vector(&self) -> bool` を追加し、`catalog.rs` 内の
`matches!(ty, ColumnType::Vector(_))` を置き換えた（挙動不変・可読性向上）。
`row_codec.rs` 内の同型の `matches!` は本 Issue の時点では手つかずのまま
残置し、後続の型追加時のチェックリスト対象へ申し送る。

## スコープ縮小の判断

計画時に検討した以下の項目は、本 Issue の時点では見送り、`docs/design` の該当
ポインタへ申し送る（詳細は Issue コメント・PR 本文参照）。

- `row_codec::ScalarRef<'a>` の新設・`scan_scalar_columns` 系の戻り値変更
  （`Option<&'a str>` → `Option<ScalarRef<'a>>`）。`scan_scalar_columns` の
  型別セル encode／decode 集約自体は、#880 時点で既に
  `scalar_text_entry_len`／`SCALAR_TEXT_ENTRY_OVERHEAD` という形で
  `tenant::validate_set_assignments` と共有済みであり（先行 Issue で実装済み）、
  本 Issue で新たに壊す理由が薄いと判断した。#881（`INTEGER`／`BIGINT`）・
  #882（`REAL`／`DOUBLE PRECISION`）がいずれも固定長の非 TEXT スカラー値を
  導入する時点で実際に破壊が必要になったため、`ScalarRef<'a>` を新設し `!`
  を付けて対応済み（`docs/design/column-type-integer.md` D3・
  `docs/design/float-column-types.md` F2 参照）。
- `sql/parser.rs` の `(ColumnType, InsertLiteral)` 束縛 3 重複の
  `bind_literal_for_column` への集約。挙動不変のリファクタリングだが、
  #880 のスコープ（カタログ v2・型タグ往復の集約）から独立して行える
  ため、必要になった時点（#881 以降、型が増えて重複が実害になった時点）で
  別 Issue として着手する。

## 型を 1 つ追加するときのチェックリスト（#881〜#897 向け）

新しいスカラー型を `ColumnType` に追加する際、以下を確認する。

1. `catalog.rs::ColumnType` に variant を追加し、`catalog_fields`／
   `from_catalog_fields` に型タグ・`param` 文法を追加する（D2・D5）。
   `param` の文字集合は `validate_catalog_param` の許容範囲
   （`[A-Za-z0-9_,]`）に収まるよう文法を設計する。
2. `row_codec.rs` の `encode_row`／`decode_row`／`encode_scalar_columns`／
   `merge_encode_scalar_columns`／`scan_scalar_columns_validated` の
   `match column.ty` 分岐（コンパイラが列挙する）に新型のペイロード
   encode／decode を追加する。`Value` enum にも対応する variant を追加する。
3. `sql/parser.rs` の `(ColumnType, InsertLiteral)` 束縛（3 箇所。
   Issue #880 では未集約のまま）にリテラル形式の受理規則を追加する。
4. `tenant.rs::validate_set_assignments` の SET 対象列型ごとの事前検証に
   新型のフレーム長計算を追加する。
5. `recovery/content_hash.rs::push_value` のハッシュ入力タグ（現状
   Null=0／Text=1／Vector=2）に新しいタグ番号を追加する
   （既存タグの意味・並びは変更しない）。
6. wire-server 側: NoSQL 表層の JSON 束縛（`http/query/{insert,update}.rs`）・
   RowDescription の OID 写像（`result_encoder.rs`）に新型を追加する。
7. `sql::scalar_index`／`sql::scan`／`sql::exec` の `ColumnType` に対する
   網羅 match（コンパイラが列挙する）に新型の扱いを追加する。
8. 3 段階デコード tier（`sql::aggregate`／`sql::group_by` の
   `DecodeTier`）が新型を正しく分類することを確認する。
9. `recovery/content_hash.rs::push_value` のタグは、TABLE-13 の列挙順で
   予約割り当て済み（Integer=3、BigInt=4、Real=5、Double=6、Boolean=7）。
   実装するタグ番号を勝手に選ばず、この表に従う（着地後に番号を振り直せない
   ため。Issue #882 で Real=5／Double=6 を実装済み）。
10. `Value`（[`row_codec::Value`]）へ variant を追加する場合、`scan_scalar_columns`
    系が返す `ScalarRef<'a>`（Issue #882 で新設。`Text(&'a str)` に加え
    `Real(f32)`／`Double(f64)` を持つ）にも対応する variant を追加し、
    `ScalarRef::as_text()` の `None` 腕（`TEXT` 専用の既存呼び出し元向け
    後方互換ヘルパー）に含める。

## バイト表現の不変範囲（golden で固定）

以下は Issue #880 の前後で不変（`row_codec.rs`・`catalog.rs` の既存/新設テストで
機械的に固定済み）。

- `ROW_CODEC_FORMAT_VERSION = 1`、presence タグ `0x00`／`0x01`
- `Text` = `u32 LE 長 + 本文`、`Vector` = `u32 LE 次元 + f32 LE`
- `SCALAR_TEXT_ENTRY_OVERHEAD = 5`
- `recovery/content_hash.rs::push_value` のハッシュ入力タグ（Null=0／Text=1／
  Vector=2）
- カタログ `v2` の列行バイト列（`name:tag:param:nullable`）は `v1` と同一。
  変わるのは 1 行目のバージョン識別子のみ（`encode_schema_golden_v2_layout`）

## #883 追記: BOOLEAN 列型

TABLE-13・TASK-196（Issue #883）で `ColumnType::Boolean` を追加した。上記
チェックリストに沿った実装内容は以下のとおり。

- カタログ型タグは `"boolean"`（`param` は `"-"` 固定。TEXT と同型）。
- 行バイト表現: presence タグに続く 1 バイト（`0x00`=false／`0x01`=true）。
  NULL（presence `0x00`）とは常に別のバイト列になる（受け入れ条件「NULL と
  false を区別する」の根拠）。`SCALAR_BOOL_ENTRY_LEN = 2`。
- `row_codec::scan_scalar_columns` 系の戻り値を `Option<&str>` から
  `Option<ScalarRef<'a>>`（`Text`／`Bool` の 2 variant）へ型付き化した
  （上記チェックリストが「#881 以降で必要になれば」と申し送っていた対応。
  `ScalarRef::as_text()`/`as_bool()` で TEXT 専用消費側の型不一致を
  fail-closed に扱う）。
- `content_hash::push_value` のタグは `Bool = 7`（Null=0／Text=1／
  Vector=2 は不変。3〜6 は他型向けに予約）。述語 DML のハッシュ
  （`push_dml_assignments`／`push_dml_where_predicates`）にも
  `InsertLiteral::Bool`＝タグ 3、`WherePredicate::BoolEquality`＝タグ 5、
  `WherePredicate::BoolColumn`＝タグ 6 を追加（`BoolColumn` と
  `BoolEquality{value:true}` は評価結果が同じでも構文が異なるため別ハッシュ
  とする安全側の判断）。
- `WHERE` 述語は宣言的フィルタ経路（`declarative_filter::FilterOp::BoolEquals`）
  でのみ評価する（式評価器はスカラー列を参照できないため）。字句解析器は
  `true`/`false` を `Token::Ident` として出すため、`sql::allowlist::Parser::
  parse_where` が `<col> = true|false` と裸の `WHERE flag`（直後が `AND`・
  `ORDER`・`LIMIT`・文末・`USING`／`HINT`／`RETURNING`／`GROUP`／`HAVING` の
  いずれかの場合に限る）を文脈照合で受理する。`NOT`／`IS [NOT] NULL`／
  `IS TRUE`／`<>`／式中の bool 列参照は対象外（既存の拒否のまま）。
- `sql::scalar_index::ScalarIndex` は BOOLEAN 列を索引化しない
  （`per_column.push(None)`）。`sql::scalar_plan::classify_scalar_plan` は
  `FilterOp::BoolEquals` を含む述語（単独・複合いずれも）を常に `PlainScan`
  に分類する単一情報源とし、Issue #843/#844 の索引被覆多層防御
  （`mask_trusted_defer`／`count_star_only`／`observe_group_count_only`）が
  BOOLEAN 述語を誤って「索引で完全被覆済み」と信頼しないことを保証する。
- 集計: `COUNT(<BOOLEAN 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::BooleanColumn`）。
  `GROUP BY` キー列は引き続き TEXT 限定のまま。
- wire-server: NoSQL `update` op（`http/query/update.rs`）は JSON 真偽値の
  `SET` を受理する。`insert.rs`・RowDescription の OID 拡張・NoSQL `insert`
  op での JSON 真偽値受理は対象外（#895・#896 の担当）。
- 対象外（申し送り）: RowDescription の OID 16 公告（#895）、NoSQL JSON
  束縛の完全対応（#896）、`22P02` の新設、`NOT`/`IS [NOT] NULL`/`IS TRUE`/
  `<>`/式中の bool 列参照、SQL `CREATE TABLE` 構文での `BOOLEAN` 宣言
  （SQL-23 は未実装）。BOOLEAN 列のスカラー二次索引化は値域が 2 値で
  索引化コストに見合わないとして #893 で対象外のまま据え置く判断を
  確定した（`docs/design/scalar-index-prune.md`「Issue #893」節参照）。

## #884 追記: DATE／TIMESTAMP

`DATE`／`TIMESTAMP` 列型（TABLE-13・TASK-197）の設計判断・内部表現・リテラル
文法・移行方針は `docs/design/datetime-column.md` を参照（本ファイルへの
転記はしない）。

## #888 追記: ARRAY 列型（複合型のうち配列部分）

TABLE-14・TASK-198（Issue #888）で `ColumnType::Array(ArrayType)` を追加した。
着手時点で origin/main には TEXT・BOOLEAN のみがマージ済み（INTEGER/BIGINT/
REAL/DOUBLE・DATE/TIMESTAMP は未マージ）だったため、要素型は `ArrayElemType`
（`Text`／`Bool` の 2 値）に限定した。詳細な設計判断・バイトレイアウトは
`docs/design/array-column-type.md` を参照。上記チェックリストとの対応は以下の
とおり。

- カタログ型タグは `"array"`、`param` は `"<elem_tag>,<max_len>"`（例:
  `text,64`）。要素数上限の実装既定値は `MAX_ARRAY_ELEMENTS = 1024`。
- 行バイト表現: presence タグに続き flags(1・`0x00` 固定)＋要素数(`u32 LE`)＋
  ペイロード長(`u32 LE`)＋要素列（TEXT は `u32 LE` 長＋UTF-8、BOOL は 1
  バイト）。TEXT/VECTOR/BOOLEAN の既存バイト表現は不変。
- `row_codec::ScalarRef::Array(ArrayRef<'a>)`・`Value::Array(ArrayValue)` を
  追加。`ArrayRef` は走査時に構造・UTF-8・要素数上限を検証済みの借用結果。
- `recovery::content_hash::push_value` のタグは `Array = 10`（3〜9 は他型
  向けに予約）。
- `sql::parser::parse_array_literal` が `'{v1,v2,...}'` リテラルを状態機械で
  解析する（字句解析器は変更しない）。NULL 要素（引用なしの `NULL`）は
  D-A6 により本版では受理せず `22000`。
- `WHERE`（等価・`IS NULL` を含む）・要素/パス演算子（`tags[1]`・`@>`）は
  対象外のまま `22000`／`42601` で拒否（申し送り）。
- `sql::scalar_index::ScalarIndex` は ARRAY 列を索引化しない
  （`per_column.push(None)`）。
- 集計: `COUNT(<ARRAY 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::ArrayColumn`）。
- wire-server: `result_encoder.rs` は PostgreSQL 配列テキスト形式（`{a,b}`。
  引用・エスケープ規則込み）で描画し、NoSQL `http/query/response.rs` は
  ネイティブ JSON 配列で描画する。NoSQL `insert`／`update` op は ARRAY 列への
  JSON 値を明示的に拒否する（`22000`。JSON 配列束縛は #896・NOSQL-17 へ
  申し送り）。
- 対象外（申し送り）: 配列列への等価（`=`／`IN`）・`IS NULL` 述語、要素/パス
  演算子、NoSQL の JSON 配列束縛（#896）、`22P02` の新設、SQL `CREATE TABLE`
  構文での `<型>[]` 宣言（#899）、数値・日時要素型（兄弟 PR マージ後）、NULL
  要素対応。配列列のスカラー二次索引化は等価・前方一致・範囲いずれの述語も
  持たないとして #893 で対象外のまま据え置く判断を確定した
  （`docs/design/scalar-index-prune.md`「Issue #893」節参照）。

## #886 追記: BYTEA 列型

TABLE-13・TASK-197（Issue #886。関連: WIRE-13・NOSQL-17）で `ColumnType::Bytea`
を追加した。上記チェックリストに沿った実装内容は以下のとおり。

- カタログ型タグは `"bytea"`（`param` は `"-"` 固定。TEXT／BOOLEAN と同型）。
- 行バイト表現は TEXT と完全に同じ枠（presence タグ + `u32 LE` 長 + 本体）
  だが UTF-8 検証を行わない点のみ異なる。長さ上限は新設の
  `bytea::MAX_BYTEA_FIELD_LEN`（`row_codec::MAX_TEXT_FIELD_LEN` と同値。
  `const` アサーションで固定）。フレーム長計算は `scalar_text_entry_len`／
  `SCALAR_TEXT_ENTRY_OVERHEAD` を共有する。
- hex テキスト表現（SQL リテラル・wire のテキスト出力）は新設モジュール
  `engine::bytea`（`parse_hex_text`／`format_hex_text`）に集約し、engine と
  wire-server の双方から共有する。受理する形式は PostgreSQL `\x` 接頭辞形式
  の最小部分集合（`\x`／`\X` ＋ 偶数個の 16 進数字。大小文字混在可・`\x` 単体
  は空バイト列）に限り、接頭辞省略・奇数桁・非 16 進文字・PostgreSQL の
  escape 形式（`\x` 接頭辞なしの `\ooo`）はいずれも拒否する（曖昧さを避ける
  fail-closed な実装既定値）。出力は `\x` ＋ 小文字 16 進（PostgreSQL の
  `bytea_output=hex` 既定と同じ）。
- `content_hash::push_value` のタグは `Bytes = 11`（Null=0／Text=1／
  Vector=2／Bool=7 は不変。3〜6・8〜10 は他型〔INTEGER/BIGINT/REAL/DOUBLE・
  DATE/TIMESTAMP/NUMERIC。並行実装中の別 Issue〕向けに予約）。
- `sql::parser` の 4 つの束縛箇所（INSERT・UPDATE の SET・UPSERT の
  リテラル・UPSERT の `ON CONFLICT` リテラル）が共通ヘルパー
  `bind_bytea_literal` を経由し、`\x` hex リテラルのみを受理する。ファイル形
  `INSERT`（`path`/`body` 列規約専用）は BOOLEAN と同じ理由で `BYTEA` 列も
  対象外として拒否する。
- 集計: `COUNT(<BYTEA 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::ByteaColumn`。`BooleanColumn`
  と同じパターン）。`GROUP BY` キー列は引き続き TEXT 限定のまま。
- `WHERE` 述語・スカラー二次索引・UDF/式評価・hybrid 本文列・`USING PLAN`・
  scoring_boost への `BYTEA` 列の露出はすべて `22000` で拒否する（`declarative_
  filter`・`sql::scalar_index`・`sql::udf_call`・`sql::using_plan`・
  `scoring_boost` の既存拒否パターンを踏襲。索引は `per_column.push(None)`）。
- wire-server: NoSQL 表層の JSON 表現は **標準 base64**（RFC 4648 §4・`=`
  パディング必須・正準形のみ）を採用し、新設 `wire-server::http::query::
  base64_std`（`encode_base64_std`／`decode_base64_std`）に厳格な codec を
  実装した。既存の `http::session::token`（base64url・パディングなし）・
  `auth::argon2id`（standard・パディングなし）はいずれも本仕様と一致しない
  ため流用しなかった。`decode_base64_std` は復号後の長さを入力長・パディング
  数から確保前に算出し、`MAX_BYTEA_FIELD_LEN` 超過を `TooLong` として拒否
  する。
  - `insert.rs`: `blob` フィールドに base64 文字列を受理し、復号結果を
    そのまま `Value::Bytes` として束縛する（`InsertLiteral` へ迂回しない）。
  - `update.rs`: 復号したバイト列を正準形（`\x` ＋ 小文字 hex）の
    `InsertLiteral::String` へ再エンコードしてから既存の hex 解析経路へ渡す
    （B9 判断。`vector_literal_text` が JSON 配列を文字列リテラルへ再エンコード
    する既存パターンと同型。engine 側の束縛経路を hex 解析の 1 本に保ち、
    NoSQL と SQL の小文字 hex リテラルのハッシュを一致させるための設計）。
    `InsertLiteral::Bytes` variant は新設しない（表層をまたぐハッシュが常に
    不一致になる・BREAKING となる enum variant が増えるため不採用）。
  - `response.rs`（`scan`／`search`／`aggregate` の JSON 応答）: `Cell::Bytes`
    を標準 base64 の JSON string として出力する。wire 側の `\x` 16 進テキスト
    表現とは意図的に異なる値表現（型名は wire 側との同一性を、値表現は
    JSON との親和性をそれぞれ優先する既存の非対称方針をそのまま踏襲）。
  - エラー分類（NOSQL-17 と一部異なる暫定判断。`22P02` 未実装〔#897・
    TASK-227〕のための申し送り）: `BYTEA` 列への非文字列 JSON（型不一致）・
    不正な base64（アルファベット外・パディング不正・非正準・長さ不正）は
    いずれも `42601`（`InsertError::InvalidBytea`／`UpdateError::
    InvalidBytea`）。SQL 表層の同種の不一致は `22000` のままであり、意図的に
    異なる（受け入れ基準 4・NOSQL-17 の「型不一致は `42601`」を優先し、
    `22P02` 導入時に再分類する）。長さ超過は表層を問わず `54000`。
- 対象外（申し送り）: RowDescription の OID 拡張（既存の `WireType::Text`
  〔OID 25〕のまま。#895）、NoSQL の既存型統一・`columns[].type`（#896）、
- 対象外（申し送り）: RowDescription の OID 拡張（既存の `WireType::Text`
  〔OID 25〕のまま。#895）、NoSQL の既存型統一・`columns[].type`（#896）、
  `22P02` の新設（#897・TASK-227）、SQL
  `CREATE TABLE` 構文での `BYTEA` 宣言（SQL-23 は未実装。宣言は Rust API の
  `TableSchema` 経由）。`WHERE` 等価・範囲比較述語は #891・TASK-199 で
  対応済み（`declarative_filter::FilterOp::TypedCompare`。詳細は
  `docs/design/scalar-types-predicates.md` 参照）。二次索引（#893）は
  等価・前方一致・範囲いずれの述語も持たない型として索引対象外のまま
  据え置く判断を確定した（`docs/design/scalar-index-prune.md`
  「Issue #893」節参照）。

## #889 追記: JSON / JSONB 列型

TABLE-14・TASK-198（Issue #889。関連: NOSQL-8・NOSQL-17）で `ColumnType::Json`・
`ColumnType::Jsonb` を追加した。上記チェックリストに沿った実装内容・逸脱の
決定は以下のとおり。

- カタログ型タグは `"json"`／`"jsonb"`（`param` は `"-"` 固定。既存型と同型）。
- 値表現は両列型で `row_codec::Value::Json(String)`／`ScalarRef::Json(&str)`
  の単一 variant を共有する。区別は `ColumnType::Json`／`ColumnType::Jsonb`
  にのみ持たせ、`JSON` 列は検証済みの入力テキスト（空白・キー順を含む）を
  そのまま、`JSONB` 列は正規化再シリアライズ済みのテキストを保持する。
- JSON テキストの構文検証は共有パーサー `engine::json::parse_json`（TASK-172・
  NOSQL-8。第 2 のパーサーは作らない）を経由する。`json.rs` に追加した
  `MAX_JSON_FIELD_LEN`（4 MiB。`row_codec::MAX_TEXT_FIELD_LEN` と同値。`const`
  アサーションで固定）・`validate_json_column_text`（`JSON` 列向け。総バイト
  長を `parse_json` を呼ぶ**前**に判定してから構文検証）・
  `canonicalize_jsonb_text`（`JSONB` 列向け。検証後 `write_canonical` で
  正規化し、正規化後の長さも再判定）・`write_canonical`（唯一の正規化
  シリアライザ。オブジェクトのキーは `JsonValue::Object` が `BTreeMap` の
  ため既にキー文字列の昇順で走査される。数値は `JsonNumber::Float` の生
  リテラル文字列をそのまま出力し `f64` を経由した再フォーマットによる
  往復ずれを避ける）を追加した。
- 格納時検証の単一チョークポイントは `row_codec` の encode 系
  （`encode_row`／`encode_scalar_columns`／`merge_encode_scalar_columns`）に
  集約した（`validate_json_column_value` が列型で分岐）。これにより Rust
  API（`tenant::insert_typed_row` 等）経由でも未検証・未正規化の JSON が
  格納されない。束縛層（`sql::parser::bind_json_literal`／NoSQL 表層の
  insert/update）でのユーザー向け分類が先に働くため、encode 層の拒否は
  API 誤用時の内部エラー（`XX000`）として扱う設計判断とした。この単一
  チョークポイントの検証は SET 対象列（クライアントからの新規入力）にのみ
  適用し、`UPDATE` の SET 対象でない既存列（`merge_encode_scalar_columns`
  の `existing` 分岐）は TEXT／BYTEA と同じく再パース・再検証しない
  （decode 契約「書き込み経路の保証に依拠」に揃える。既存値を毎回
  再検証すると SET 対象でない `UPDATE` でも文書サイズに比例した
  再パース・再シリアライズ・文字列比較のコストが掛かるうえ、
  `write_canonical` の出力形式が将来変わった場合に既存の全 `JSONB` 行が
  `XX000` で更新不能になるフォワード互換の結合が生じるため）。
- 行バイト表現は TEXT と同じ枠（presence + `u32 LE` 長 + UTF-8 本体）を
  共有し、フレーム長計算は `scalar_text_entry_len` を共有する。decode 側は
  TEXT と同じく長さ上限・UTF-8 検証のみを行い、再パースしない（書き込み
  経路の保証に依拠。再検証が必要な消費側〔NoSQL 応答〕は自前で行う）。
- `content_hash::push_value` のタグは `Json = 12`（Null=0／Text=1／
  Vector=2／Bool=7／Bytes=11 は不変。3〜6・8〜10 は他型向けに予約）。
- **決定 D1（Issue 本文からの逸脱）**: Issue 本文は「`22032` 相当」の新規
  `wire_code` を想定していたが、spec（SSOT）は ERR-4 の閉じた分類集合を維持し
  新規 `wire_code` の追加を求めていない。よって JSON 受理規則違反は既存分類
  のみで写像する: 構文不正・重複キー・非 RFC 8259 数値・深さ/要素数超過は
  `42601`（`SqlSurfaceError::UnsupportedSyntax`／NoSQL は `InsertError::
  InvalidJson`／`UpdateError::InvalidJson`。NOSQL-8 と同一分類）、総バイト長
  超過は `54000`（`SqlSurfaceError::PayloadTooLarge`／`InsertError::
  JsonTooLarge`／`UpdateError::JsonTooLarge`）、数値・真偽値リテラルの型
  不一致は `22000`（SQL 表層。既存の型不一致パターン）。`22P02` 新設は
  #897・TASK-227 の担当のまま。
  - 注記: SQL 表層の総バイト長超過（`54000`）は、`sql::lexer::MAX_INPUT_LEN`
    （SQL テキスト全体の入力長上限。1 MiB）が `json::MAX_JSON_FIELD_LEN`
    （4 MiB）より小さいため、SQL リテラル経由では構造的に到達できない
    （1 MiB 超の SQL テキストは常に字句解析段で `42601` になる）。総バイト長
    判定が構文検証より前に働くこと自体は `json.rs` の単体テスト
    （`validate_json_column_text_checks_length_before_parsing`／
    `canonicalize_jsonb_text_checks_length_before_parsing`）で固定し、SQL
    テキストを経由しない Rust API（`tenant::insert_typed_row`）経由の到達性は
    `json_column.rs::typed_row_insert_rejects_total_byte_length_over_limit_before_parsing`
    で固定する。また 1 つの文字列リテラル自体には別上限
    `json::MAX_JSON_STRING_CHARS`（1 MiB）が掛かるため、`MAX_JSON_FIELD_LEN`
    ちょうどの JSON を構成する単体テストは複数要素の配列を使う
    （`validate_json_column_text_accepts_at_exact_length_limit`）。
- `sql::parser` の 4 つの束縛箇所（INSERT・UPDATE の SET・UPSERT のリテラル・
  UPSERT の `ON CONFLICT` リテラル）が共通ヘルパー `bind_json_literal` を
  経由する。SQL の文字列リテラルは JSON テキストとして解釈し、トップレベルの
  スカラー JSON（`'1'`・`'"s"'`・`'null'`）も有効な JSON として受理する
  （`'null'` は JSON の `null` であり SQL `NULL` とは別物）。ファイル形
  `INSERT`（`path`/`body` 列規約専用）は BOOLEAN／BYTEA と同じ理由で `JSON`
  列も対象外として拒否する。
- 集計: `COUNT(<JSON/JSONB 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/
  `MIN`/`MAX` は `22000` で拒否する（`AggregateInput::JsonColumn`。
  `ByteaColumn` と同じパターン）。`GROUP BY` キー列は引き続き TEXT 限定の
  まま。
- （`WHERE` の等価・`IN`・`IS [NOT] NULL` は Issue #1193 で受理へ変更した。
  `docs/design/array-column-type.md`「Issue #1193 追記」参照。以下の残りは不変）
  上記以外の `WHERE` 述語（範囲・`LIKE`・パス演算子等）・スカラー二次索引・
  UDF/式評価・hybrid 本文列・`USING PLAN`・
  scoring_boost・バイナリ結果形式（WIRE-14）への `JSON`/`JSONB` 列の露出は
  すべて BYTEA と同じ既存拒否パターンを踏襲し `22000`／`0A000` で拒否する
  （二次索引は等価・前方一致・範囲いずれの述語も持たない型として索引対象外
  のまま据え置く判断を #893 で確定した。`typed_columns[i] = None`。
  `docs/design/scalar-index-prune.md`「Issue #893」節参照）。
- **決定 D5（パス参照の範囲。Issue 本文からの逸脱）**: spec TABLE-14 は
  JSON パス演算子（`->`／`->>`／`@>` 等）による述語を対象外（`42601`）と
  している。式評価器（`sql::udf_call`）は TEXT 列参照すら `22000` で拒否し
  `ExprType` に文字列型が無いため、SQL 関数形の追加も式評価拡張（#891）の
  領域となる。よって本 Issue では `json.rs` に engine の Rust API 限定の
  最小パス参照 API（`JsonPathStep`／`JsonPath`／`extract_path`／
  `extract_path_text`。ステップ数上限は `MAX_JSON_DEPTH` と同値）のみを
  追加し、SQL・NoSQL 表層への構文露出は行わない（`doc -> 'a'` は引き続き
  `42601`）。露出は spec ID 付与後の別 Issue へ申し送る。
  式評価器への写像の設計は [json-expr-type-mapping.md](json-expr-type-mapping.md)（Issue #1595）。
  - Issue #1596（TABLE-25）で `jsonb` 列に限りパス演算子 `->` `->>` `#>` `#>>` を構文段・
    束縛段・評価器に開放した（`json` 列は 0A000、非 jsonb の左辺は 42883）。上記の
    Rust API（`JsonPath`／`extract_path`）は変更していない。
  - Issue #2180（TABLE-25）で `json` 列にもパス演算子と `json_*` 関数を開放した。`json` の
    式結果は入力の**部分テキスト**（空白・数値表記・キー順を保つ）で、`jsonb` の正規化値とは
    別の式型（`ExprType::JsonText`）で運ぶ。詳細は
    [json-expr-type-mapping.md](json-expr-type-mapping.md)。
- wire-server: SQL 表層のテキスト表現は格納テキストをそのまま出力する
  （`JSON` は入力テキスト保持・`JSONB` は正規化済みテキストのため、いずれも
  再シリアライズしない）。NoSQL 表層（`insert.rs`／`update.rs`）は JSON
  オブジェクト／配列を受理し `write_canonical` で正規化テキスト化した
  うえで `Value::Json`（insert）／`InsertLiteral::String`（update。BYTEA の
  B9 判断と同じく束縛経路を 1 本に保つ）へ写像する。JSON `null` は
  nullable 列なら `NULL`。スカラー JSON（文字列・数値・真偽値）は
  NOSQL-17 の束縛表に無いため曖昧さを避けて `42601` で拒否する。
  `response.rs`（`scan`／`search`／`aggregate` の JSON 応答）は `Cell::Json`
  を **native JSON 値**として出力する。格納テキストを共有パーサーで再パース
  し `write_canonical` で再シリアライズしてから埋め込む（格納バイト列を
  生のまま応答本文へ連結しない安全側の設計。再パース失敗は内部エラー
  〔`XX000`〕）。`columns[].type` の型名整備は #896 へ申し送り（現行の一律
  `"text"` のまま）。
- **決定 D4（表層横断の非対称）**: `JSONB` は SQL・NoSQL とも正規化形を
  格納・ハッシュするため表層横断の再送判定（`23505`／`22023`）が一致する。
  一方 `JSON`（非 B）は SQL 表層が入力テキストをそのまま格納・ハッシュする
  のに対し、NoSQL 表層は常に正規化形を格納・ハッシュするため、非正規な
  空白を含む SQL 書き込みを NoSQL から同一 `operation_id` で再送すると
  `22023`（内容不一致）になりうる。これは「`JSON` 型は入力テキスト保持」を
  優先した既知の非対称であり、`JSON` 型の正規化は PostgreSQL `json` の
  意味論から外れるため採らない（層 B テスト
  `wire_json_column.rs::json_operation_id_resend_via_nosql_after_sql_seed_is_content_mismatch_when_whitespace_differs`
  で契約を固定）。
  - **Issue #2180 追記（重複キーと応答の関係）**: `json` 列は重複キーを受理して原文のまま
    格納する（`jsonb` は従来どおり `22P02`）。重複キーの値は最後のものが有効で、SQL 表層の
    `SELECT doc` は原文を返し、`->`・`#>` などの式結果は原文の部分テキスト、NoSQL 表層の
    応答は共有パーサーで後勝ちに再パースして正規形で出力する（重複キーは最後の値へ畳まれる。
    生テキストは連結しない）。等価判定（UNIQUE・`WHERE doc = '...'`・`IN`）も最後の値で行う。
    要素数の上限は重複を含む生のメンバー数で判定する。
- 対象外（申し送り）: RowDescription の OID 拡張（`json` 114／`jsonb` 3802。
  既存の `WireType::Text`〔OID 25〕のまま。#895）、NoSQL の既存型統一・
  `columns[].type`（#896）、`22P02` の新設（#897・TASK-227）、`WHERE` 述語
  （等価・`IS NULL`）対応（#891）、SQL `CREATE TABLE` 構文での
  `JSON`/`JSONB` 宣言（SQL-23 は未実装。宣言は Rust API の `TableSchema`
  経由）、パス参照 API の SQL/NoSQL 表層への構文
  露出（spec ID 付与後の別 Issue）、`JSONB` の真のバイナリ格納表現
  （現状は正規化テキスト表現を実装既定値とする）。

## #890 追記: ENUM 列型

- 対象ビヘイビア（ポインタ表記のみ）: TABLE-14・TASK-198（関連: TABLE-6,
  TABLE-7, TABLE-13, WIRE-13, WIRE-14, NOSQL-17, ERR-2, ERR-4, ERR-6,
  TASK-227）。

### D1: 名前付き型としてカタログに登録する（列ごとのインライン語彙は採らない）

列ごとに語彙をインラインで持つ方式では型削除（`DROP TYPE`）を表現できない
ため、名前付き型を新設テーブル `enum_types`（キー: 型名、値: バージョン付き
blob）へ登録する方式を採用した。DDL は SQL-23（`CREATE TYPE ... AS ENUM`）
は Issue #1194 で SQL 表層へ結線した（`docs/design/enum-type-ddl.md`）。Rust API
は `Storage::{create,get,alter_enum_type_add_value,drop}_enum_type`。

制約: ラベル数 1〜256（`MAX_ENUM_LABELS`）・ラベル長 1〜63 バイト
（`MAX_ENUM_LABEL_LEN`）・制御文字禁止・重複禁止・宣言順保持。型名は識別子
検証に加え組み込み型名（`text`/`vector`/`boolean`/`bytea`/`integer` 等。
大文字小文字を区別しない）との衝突を拒否する（将来 SQL-23 の型名解決での
曖昧さを先に塞ぐ）。登録可能な型数の上限は `MAX_LIST_TABLES` と同値。

blob 形式: `u8` バージョン・`u16` ラベル数（LE）・各ラベル `u8` 長さ＋UTF-8
本体。宣言数の上限をアロケーション前に検証してからデコードする。

`catalog::decode_schema`（列定義のデコード）は ENUM 列の型名解決に txn
経由のリゾルバを要求するため、`decode_schema_with_resolver` へ一般化した。
production の呼び出し口は `get_table_schema_in_txn`（read txn）・
`require_table_schema_write`（write txn）・`Storage::alter_table_add_column`
の 3 箇所のみで、いずれも既に txn を保持しているためリゾルバの追加コストは
小さい。`create_table`／`alter_table_add_column` は、呼び出し元が渡した
`ColumnType::Enum` の `Arc<EnumTypeDef>` を信頼せず、この write txn から
見えるカタログ登録済みの定義の実在のみを検証する（渡された語彙の中身は
使わず、カタログは型名のみを永続化するため）。

### D2: `ColumnType::Enum(Arc<EnumTypeDef>)` とし `Copy` を除去する

`EnumTypeDef { name, labels }` はフィールドを private にし `name()`／
`labels()`／`contains()`／`validate_label()` のみを公開する（engine・
wire-server が語彙検証を委譲する単一情報源）。`ColumnType` から `Copy` を
除去し、既存の呼び出し元は `&column.ty` での参照マッチ／`.clone()` へ移行
した（`!=` 比較・`matches!` は参照を暗黙に取るため無変更で動作する）。

### D3: 行にはラベル文字列を TEXT と同じフレームで格納する（序数は使わない）

行バイト表現は presence タグ＋`u32` LE 長＋UTF-8 本体で、TEXT と完全に
同じ枠（`scalar_text_entry_len`）を共有する。`row_codec::Value::Enum(String)`・
`ScalarRef::Enum(&str)` を新設し、`ScalarRef::as_dictionary_text()`
（`Text`／`Enum` のみ `Some`）を経由してスカラー列二次索引
（`sql::scalar_index::ScalarIndex`）・`declarative_filter` の等価比較が
TEXT と同じ辞書表現を共有する（二重実装にしない。受け入れ基準 3）。

語彙検査は 3 段の多層防御を持つ: (1) 束縛時（`sql::parser::bind_enum_literal`。
書き込みトランザクション開始前に `22P02`）、(2) `row_codec` の encode 時
（`encode_row`／`encode_scalar_columns`／`merge_encode_scalar_columns`。
Rust API から直接渡された `Value::Enum` もここで拒否する）、(3) `row_codec`
の decode 時（`decode_row`／`scan_scalar_columns_validated`。PR #1015
レビュー指摘・codex-review P1: 破損行〔手書き・バグ由来〕が持つ語彙外
ラベルを検査せず通すと、投影・等価フィルタ・二次索引へ任意文字列が
流出しうるため、現行スキーマの `EnumTypeDef::labels`〔書き込み時点の
語彙ではなく decode 時点で解決される最新の語彙〕に含まれないラベルは
`RowCodecError::Invalid` で fail-closed に拒否する）。ラベルは削除されない
契約（D4）のため、この検査は「書込み時点で有効だったか」を後退させる
ものではなく、過去に正当だった値は将来にわたって decode 可能であり続ける。

### D4: ALTER TYPE は「可。ただし末尾への追記（ADD VALUE）のみ」

`BEFORE`/`AFTER` 指定・`RENAME VALUE`・削除・並べ替えはいずれも提供しない。
行はラベル文字列を直接格納するため既存行は不変であり、語彙は単調に増える
だけなので、古いスナップショットで有効だった値は書き込み時点でも常に有効
（削除がないため）。追記は依存テーブルすべての世代を同一 write txn 内で
進行させる（`dependent_tables_in_txn` によるテキスト走査。多層防御）。

### D5: DROP TYPE

依存列（当該型を参照する `ColumnType::Enum` 列）が 1 つでも残っていれば
`CatalogError::DependentObjectsStillExist` で拒否する。SQL 表層の
`DROP TYPE` は Issue #1194 で `2BP01` へ写像する（`ErrorClass` への追加は不要）。

### D6: エラーコード `22P02` の新設

`ErrorClass::InvalidTextRepresentation`（`22P02`。HTTP 400）を新設した
（count 16→17）。SQL の INSERT/UPDATE（単一行 SET・述語形）/UPSERT と
NoSQL の insert/update/filter に送出経路を持つ。エラーメッセージに語彙の
一覧は含めない（型名とクライアント自身の入力値のみ）。`CatalogError` に
`TypeNotFound`／`TypeAlreadyExists`／`DependentObjectsStillExist` を追加し
（非 `non_exhaustive` な公開 enum への破壊的変更）、SQL 経路では既存の
`Internal` へ丸める（DDL の SQL 表層結線が無いため）。型不一致（数値・真偽値
リテラルを ENUM 列へ）は BYTEA の前例に倣い SQL `22000`・NoSQL `42601` の
まま（BOOLEAN／BYTEA の既存分類の再分類はスコープ外・#897・TASK-227）。

### D7: 述語・集計・投影の露出範囲

- `WHERE <enum列> = '<label>'`: `Text` と同じ等価述語を受理し、語彙外は
  `22P02`（PostgreSQL の enum 入力と同じ挙動）。索引の完全被覆を信頼する
  経路（Issue #843/#844）は TEXT と同じ辞書・同じ等価意味論のため無変更で
  安全。`LIKE`（前方一致）は TEXT 限定のまま `22000` で拒否。
- 集計: `COUNT(<enum列>)`（非 NULL 行数）のみ受理し `AggregateInput::
  EnumColumn` を新設。`SUM`/`AVG`/`MIN`/`MAX` は `42883`（Issue #1349）（PostgreSQL の enum
  は宣言順で `MIN`/`MAX` 比較できるが、辞書順で代用すると意味論が食い違う
  ため意図的に受理しない）。`GROUP BY` キー列は TEXT 限定のまま対象外。
- UDF／式評価・hybrid 本文列・`USING PLAN`・scoring_boost への ENUM 列の
  露出は BYTEA と同じく `22000` で拒否する。
- 投影・`RETURNING`: `Value::Enum`／`ScalarRef::Enum` は既存の `Cell::Text`
  へ写像する（表示が TEXT と同一のため wire 側の追加変更を抑える。BYTEA は
  hex 表示が異なるため `Cell::Bytes` が必要だったが ENUM は不要）。
- wire: RowDescription の OID は 25（既存の `WireType::Text` 経由。#895 の
  OID 拡張対象外）。バイナリ形式指定は `BinaryFormatError::UnsupportedType`
  （`0A000`）で拒否する。
- NoSQL 表層: `insert`／`update` の JSON 表現はラベルの生文字列（BYTEA の
  base64 とは異なり、ラベルは人間可読な識別子のため）。語彙外は `22P02`、
  非文字列は `42601`（NOSQL-17 と同じ「型不一致は `42601`」方針）。
- `content_hash::push_value` のタグは `Enum = 12`（Bytes=11 まで使用済み。
  他型と衝突しない新規タグ）。

### 対象外（申し送り）

SQL の `CREATE TYPE ... AS ENUM`／`ALTER TYPE`／`DROP TYPE` 構文と
`CREATE TABLE` での ENUM 列宣言（SQL-23・DDL 権限ゲート前提）、`2BP01` の
`ErrorClass` 化（DROP TYPE の SQL 結線時）、NoSQL の `columns[].type` の
型名整備（#896）、ENUM の宣言順比較・`ORDER BY`・`IN`・`IS [NOT] NULL`・
`GROUP BY` キー、BOOLEAN／BYTEA の型不一致を `22P02` へ再分類する件
（#897・TASK-227）、ENUM 配列（ARRAY #888 との組み合わせ）。

## #885 追記: NUMERIC / DECIMAL 列型

TABLE-13〔検討中〕・TASK-197（Issue #885）で `ColumnType::Numeric { precision,
scale }` を追加した。`DECIMAL` は別名として扱うだけで、カタログの型タグは
`"numeric"` の 1 つに固定する（第 2 のタグは作らない）。外部クレートは使わず
自作実装（オーナー判断・dependency-policy）。

- **内部表現**（`crates/engine/src/numeric.rs::Decimal`）: `unscaled: i128`・
  `scale: u8` の対で `unscaled × 10^-scale` を表す。`10^38 − 1 < i128::MAX`
  をコンパイル時 `assert!` で固定し、`POW10` 参照表を持つ。`unscaled == 0`
  は常に非負へ正規化する（`-0.00` を持たない）。
- **カタログ**: 型タグ `"numeric"`・`param` は `"p,s"`（例: `"10,2"`）。
  `1 <= precision <= 38`・`0 <= scale <= precision` を encode・decode 両側で
  検証する（`validate_numeric_precision_scale`）。decode 側は `parse_numeric_param`
  がカンマ 1 個・両要素とも ASCII 数字のみの `u8`・再 encode 一致（先頭ゼロ等
  の非正規形を拒否）まで厳格に検証する。
- **行バイト表現**: presence タグに続く `unscaled` の `i128` LE 16 バイト固定
  （`SCALAR_NUMERIC_ENTRY_LEN = 17`）。`scale` は行に持たずカタログの列型のみ
  を正とする（BOOLEAN の 1 バイト固定と同じ設計方針の延長）。この設計の帰結
  として、将来 `p` だけを拡大する `ALTER COLUMN TYPE`（#901）は行の書き換え
  なしで実現できるが、`scale` を変える変換は行の書き換えを要する。
- **丸め規則**: 束縛時に列の `scale` へ half away from zero で丸める
  （PostgreSQL numeric と互換。`1.005` → `1.01`、`-1.005` → `-1.01`）。丸め後
  の整数部が `precision - scale` 桁を超えたら `22003`（`999.995` →
  丸め後 `1000.00` は `NUMERIC(5,2)` では桁あふれ）。整数部の先頭ゼロは
  有効桁に数えない（`NUMERIC(38,38)` へ `0.999...9`〔38 桁〕を通すために
  必要）。
- **リテラル文法**: `InsertLiteral::Number`（数値トークン）・
  `InsertLiteral::String`（文字列リテラル）の両方を受理する
  （`crates/engine/src/sql/parser.rs::bind_numeric_literal`。INSERT・UPDATE
  ・UPSERT の 3 束縛経路が共有し、第 2 のパーサーを作らない）。文法は
  `[+-]?(digits)?(\.digits?)?([eE][+-]?digits)?`（整数部・小数部の少なくとも
  一方に 1 桁以上。`.5`・`5.` はいずれも受理。指数部は Issue #1358 で受理し、
  小数点位置のシフトとして扱ってからシフト後の値を丸め・桁検査する）。
  空白・`NaN`/`Infinity`・複数の `.`・桁の無い指数は拒否する。入力長上限（`MAX_LITERAL_LEN = 1024` バイト）を解析前に検査
  する（DoS 防止）。形式不正は `22000`（PostgreSQL の `22P02` 相当だが、
  兄弟 Issue #881/#882 に新設の前例が無かったため既存の `22000` で代替。
  #883 と同じ判断）。
- **負数リテラル**（D6）: 字句解析器は `-1.5` を `Punct('-')` と
  `Number("1.5")` に分けて出すため、`sql::allowlist::Parser::expect_literal`
  へ「`-` の直後に `Number` が来たら符号を連結した `InsertLiteral::Number`」
  の分岐を追加した（`InsertLiteral` の variant は増やさない）。この変更は
  非 NUMERIC 列（`id`・TEXT・VECTOR・BOOLEAN）へ `-5` を与えた場合の
  `wire_code` を `42601`（構文エラー）から `22000`（束縛時の型不一致）へ
  変える（拒否されること自体は変わらない。#881 と同じ意図の差分）。
- **content_hash**（D7）: `push_value` のタグは当初宣言順の `Numeric = 10`
  を想定していたが、origin/main への rebase 取り込みで ARRAY（Issue #888）が
  タグ 10 を先に使用していたため、衝突しない未使用タグ `Numeric = 14`
  （Null=0／Text=1／Vector=2／Bool=7／Array=10／Bytes=11／Json=12／Enum=13 は
  不変。3〜6 は INTEGER/BIGINT/REAL/DOUBLE、8〜9 は DATE/TIMESTAMP 向けに予約）
  へ採番し直した（NUMERIC の content_hash はこの時点まで一度も永続化されて
  いないため後方互換の懸念なし）。入力は `scale`（1 バイト）+ `unscaled`
  （`i128` LE 16 バイト）で、束縛後の正規値に対してハッシュを取るため、
  同一列への `1.10` と `1.1` の再送は同一内容（`23505`）に収束する。
- **出力表現**（D8）: 正規テキストは「符号、整数部、（`scale > 0` のときのみ）
  `.` とちょうど `scale` 桁にゼロ埋めした小数部」（`Decimal` の `Display`）。
  `-0.50`・`-0.01`・`-1.00` 等の負数境界も含めて先頭ゼロ・指数表記・
  trailing `-0` を持たない。wire の DataRow テキスト・HTTP JSON number
  （bare number としてそのまま出力）の両方がこの正規テキストを共有する。
- **集計**: `COUNT(<NUMERIC 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/
  `MIN`/`MAX` は `22000` で拒否する（`AggregateInput::NumericColumn`）。
  `GROUP BY` キー列は既存のとおり TEXT 限定のため NUMERIC 列は構造的に
  拒否される（変更不要）。
- **WHERE・式・二次索引**: `declarative_filter::MetadataFilter::bind` は
  当初 `!matches!(column.ty, ColumnType::Text)`／`Boolean` の否定判定により
  NUMERIC 列も構造的に拒否できる想定だったが、origin/main への rebase 取り込み
  （ARRAY・JSON/JSONB 列型）で当該判定が許可型を列挙する明示的な `match` へ
  変わっていたため、`ColumnType::Numeric { .. }` の明示的な拒否腕を追加した
  （網羅性はコンパイラが強制）。式中の列参照（`sql::udf_call`）・
  `sql::scan`（DecodeTier 分類）・`sql::using_plan`（本文列規約）は
  いずれも明示的な拒否・除外腕を追加した。`sql::scalar_index` は本 Issue
  時点では索引対象外だったが、#893 で `OrderedColumnIndex::I128`
  （列固定 `scale` を第 2 要素として保持）として索引化し、`WHERE` 述語
  （#891・TASK-199 で結線済みの `FilterOp::TypedCompare`）から実際に
  候補削減へ消費される production 結線まで完了した（リテラルが列と
  異なる `scale` を持つ場合は `numeric::rescale_bounds_for_column` が
  `cmp_exact` と同じ「丸めない」比較意味論を保ったまま列 `scale` 側の
  整数境界へ変換する。codex-review 指摘・PR #1032）。`ScalarIndex::build`
  （production の既定入口）は本列を常に構築する。詳細は
  `docs/design/scalar-index-prune.md`「Issue #893」節・「レビュー対応」節
  参照。
- **wire-server**: `result_encoder.rs::cell_to_text`・`http/query/response.rs`
  の JSON 出力に `Cell::Numeric` を追加。NoSQL `update` op の JSON `SET`
  束縛（`http/query/update.rs`）は NUMERIC 列を対象外として明示的に拒否し
  （`insert.rs` の既存ワイルドカード腕による拒否と対称。#883 が残した
  insert/update の非対称を増やさない）、両表層とも fail-closed。
- **RowDescription・HTTP 応答の型メタデータ**（PR #1020 codex-review 指摘）:
  `result_encoder.rs::column_wire_type` が `ColumnMeta::Scalar{ty:
  Numeric{..}}` を他の `Scalar` 列（`text` 固定）より先に判定し、`id` と同じ
  `WireType::Numeric`（OID 1700・`"numeric"`）を公告する（`id` は別経路
  〔`ColumnMeta::Id`〕のため OID 1700 の流用先が衝突することはない）。
  値の実体（`Cell::Numeric` の正規テキスト）とテキスト形式での型公告は
  この時点で一致する。バイナリ形式（format code 1）は引き続き
  `column_binary_support` が fail-closed に非対応とし、対応拡大は #895 の
  担当のまま。
- 対象外（申し送り）: `22P02` の新設、`SUM`/`AVG`/`MIN`/`MAX`（#892）、
  `DecodeTier` の精査（#894）、NUMERIC 列のバイナリ形式対応（#895）、
  NoSQL `insert`/`update` op での JSON 数値の完全な束縛対応（#896）、
  回帰テストの集約（#897）、`ALTER COLUMN TYPE` による `p` の拡大
  （#901）、SQL `CREATE TABLE` 構文での `NUMERIC`/`DECIMAL` 列宣言
  （SQL-23 は未実装）、ファイル形 `INSERT`（`path`/`body` 列規約専用の
  ため NUMERIC 列は明示的に拒否）。`WHERE` 等価・範囲比較述語は
  #891・TASK-199 で対応済み（`declarative_filter::FilterOp::
  TypedCompare`。詳細は `docs/design/scalar-types-predicates.md`
  参照）。スカラー二次索引化（#893。`unscaled` の `i128` をキーとする
  順序索引 `sql::scalar_index::OrderedColumnIndex::I128` として構築し、
  `WHERE` の範囲比較述語（`FilterOp::TypedCompare`）から実際に候補削減へ
  消費される production 結線まで完了した〔codex-review 指摘・PR #1032〕。
  詳細は `docs/design/scalar-index-prune.md`「Issue #893」節・
  「レビュー対応」節参照）。

## #887 追記: UUID 列型

TABLE-13〔検討中〕・TASK-197（Issue #887）で `ColumnType::Uuid`（128bit 識別子
列）を追加した。外部クレートは使わず自作実装（オーナー判断・dependency-policy）。

- **内部表現**（`crates/engine/src/uuid.rs::Uuid`）: `[u8; 16]` を RFC 4122
  ネットワークバイトオーダー（正規テキストの先頭 16 進 2 桁が先頭バイト）で
  保持する。version／variant ビットは検証しない（nil
  `00000000-0000-0000-0000-000000000000`・全 1
  `ffffffff-ffff-ffff-ffff-ffffffffffff` も有効値として受理する）。
- **順序規約**: derive した `Ord` はバイト列の辞書順（`memcmp` 相当）になり、
  これは符号なし 128bit big-endian の大小・正規テキストの辞書順のどちらとも
  一致することを単体テストで機械的に固定した（`uuid.rs::tests::
  byte_order_matches_canonical_text_order`）。WHERE 比較（#891・TASK-199 で
  結線済み。`declarative_filter::MetadataFilter::matches` の `TypedCompare`
  腕がこの `Ord` をそのまま使う）・ORDER BY がこの順序を共有する唯一の
  定義とする。二次索引は #893 で `sql::scalar_index::OrderedColumnIndex::
  U128`〔ネットワークバイトオーダーのバイト列を `u128` ビッグエンディアン
  として扱う。この `Ord` 導出と同じ大小関係になる〕として構築し、`WHERE`
  の範囲比較述語から実際に候補削減へ消費される production 結線まで完了
  した（codex-review 指摘・PR #1032）。詳細は
  `docs/design/scalar-index-prune.md`「Issue #893」節・「レビュー対応」節
  参照）。
- **カタログ**: 型タグ `"uuid"`・`param` は常に `"-"`（他のパラメータなし
  スカラー型と同じ）。
- **行バイト表現**: presence タグに続く 16 バイト生値固定
  （`SCALAR_UUID_ENTRY_LEN = 17`）。16 バイトのどの値も有効なため decode 側の
  値域検証は不要（長さ不足のみ fail-closed に拒否）。
- **入力文法**（U3）: ちょうど 36 バイトの `8-4-4-4-12` 形（ハイフンは位置
  8・13・18・23）のみを受理する。16 進数字は大文字・小文字混在可。波括弧
  （`{...}`）・`urn:uuid:` 接頭辞・ハイフンなし 32 桁・ハイフン位置違い・
  前後の空白・非 ASCII はすべて拒否する。長さは解析前に検査する（DoS
  防止）。
- **出力表現**（U4）: 小文字 16 進の `8-4-4-4-12` 正規テキストへ整形する。
  wire の DataRow テキスト・HTTP JSON 文字列（`Cell::Uuid` を JSON string
  として出力）はこの 1 つの正規テキストを共有する。
- **リテラル束縛**（`sql::parser::bind_uuid_literal`）: INSERT・UPDATE（単一
  行 SET）・UPSERT リテラル・UPSERT `ON CONFLICT` リテラルの 4 束縛箇所が
  共有する。文字列リテラルのみ受理し、数値・真偽値リテラルは `22000`
  （型不一致）。文字列だが厳密文法に反する形式は `22P02`
  （`SqlSurfaceError::invalid_text_representation`。ENUM 列の語彙外ラベル
  〔Issue #890〕と同じ発生経路・同じ分類を共有する）で、書き込みトランザクション
  開始前に拒否する。ファイル形 `INSERT` は `path`/`body` 列規約専用のため
  UUID 列を明示的に拒否する。
- **content_hash**（タグ 15）: `push_value` の既存タグ（Null=0／Text=1／
  Vector=2／3〜6 は INTEGER/BIGINT/REAL/DOUBLE 予約／Bool=7／Date=8／
  Timestamp=9／Array=10／Bytes=11／Json=12／Enum=13／Numeric=14）に続く
  未使用タグ `Uuid = 15` を確保した。入力は 16 バイト生値をそのまま連結する
  （束縛後の正規値でハッシュするため、大文字・小文字だけが違う同一 UUID の
  再送は同一内容〔`23505`〕に収束する）。
- **集計**: `COUNT(<UUID 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::UuidColumn`）。`GROUP BY`
  キー列は既存のとおり TEXT 限定のため UUID 列は構造的に拒否される
  （変更不要）。
- **WHERE・式・二次索引**: `declarative_filter::MetadataFilter::bind`・
  `scoring_boost`・式中の列参照（`sql::udf_call`）・`sql::using_plan`
  （本文列規約）はいずれも明示的な拒否腕を追加した（網羅性はコンパイラが
  強制。ワイルドカード腕は使わない）。`sql::scalar_index` は本 Issue
  時点では索引対象外だったが、#893 で `OrderedColumnIndex::U128` として
  索引化し、`WHERE` 述語（#891・TASK-199 で結線済みの `FilterOp::
  TypedCompare`）から実際に候補削減へ消費される production 結線まで
  完了した（codex-review 指摘・PR #1032）。`ScalarIndex::build`
  （production の既定入口）は本列を常に構築する。
  `sql::scan`（DecodeTier 分類・投影）は UUID 列を他のスカラー型と同じ
  `DimAndScalar` tier で扱う。
- **wire-server**: `result_encoder.rs::cell_to_text`・`http/query/response.rs`
  の JSON 出力に `Cell::Uuid` を追加。`column_wire_type` は他の非 VECTOR
  スカラー列と同じ `WireType::Text`（OID 25）のまま据え置き、OID 専用公告は
  #895 の担当のまま（U10）。`column_binary_support` は `false`（バイナリ
  指定は `0A000`）。NoSQL `update` op の JSON `SET` 束縛（`http/query/
  update.rs`）は UUID 列の JSON 文字列を `InsertLiteral::String` として
  engine の同じ束縛関数（`bind_uuid_literal`）へ渡す（BYTEA・DATE と同型の
  判断で束縛経路を engine 側の 1 本に保つ。`null`（nullable 列）は
  `InsertLiteral::Null`）。`insert` op は既存のワイルドカードによる拒否の
  まま（#896 へ申し送り。insert/update の非対称は BOOLEAN／DATE と同じ
  既知の制約）。
- 対象外（申し送り）: WHERE 等価・範囲比較述語は #891・TASK-199 で対応済み
  （`declarative_filter::FilterOp::TypedCompare`。詳細は
  `docs/design/scalar-types-predicates.md` 参照）。式（算術・関数引数）中の
  UUID 列参照・`SUM`/`AVG`/`MIN`/`MAX`（#892）、`DecodeTier` の精査
  （#894）、UUID 列のバイナリ形式・OID 2950 対応（#895）、SQL `CREATE TABLE`
  構文での `UUID` 列宣言（SQL-23 は未実装）は引き続き対象外。スカラー二次索引化（#893）は
  `OrderedColumnIndex::U128` として構築し、`WHERE` の範囲比較述語から
  実際に候補削減へ消費される production 結線まで完了した
  （codex-review 指摘・PR #1032）。詳細は `docs/design/scalar-index-prune.md`
  「Issue #893」節・「レビュー対応」節参照。NoSQL `insert` op での JSON 束縛
  （#896）は Issue #896（NOSQL-17）で実施済み。

## Issue #896 追記

NoSQL 表層の JSON 束縛（`insert`／`update`／`filter`）の型別対応・`columns[].type` の型名整備は Issue #896（NOSQL-17）で実施済み。詳細は `docs/design/nosql-typed-json-binding.md` 参照。

## Issue #1563 追記: VARCHAR(n)／CHAR(n)

- カタログ（TABLE-31）: `varchar` タグの param は `-`（長さなし。従来のバイト列を維持）または 10 進の n、`bpchar` タグの param は 10 進の n（`-` は不可）。復号は 10 進 ASCII 数字のみ受理し、先頭ゼロ・`+`・空・u32 あふれ・範囲外（`1..=10485760` の外）をすべて拒否する。
- Issue 本文は「新 variant を作らず text＋param」としていたが、#2305 で `varchar` が独立タグになったため、`text` ではなく `varchar` の param 拡張と `bpchar` 新設で表す。CHAR を varchar の種別フラグにしなかったのは、比較・出力の意味論が TEXT と異なる CHAR が varchar の経路へ黙って流れる fail-open を避けるため。
- `"char"`（OID 18 の 1 バイト型）とは別物のため、タグ名に `char` を使わない。
- 詳細な決定と対象外は `docs/design/implementation-status.md` の Issue #1563 の項を参照。

## Issue #1564 追記: 長さ超過の 22001

- 束縛段（INSERT・UPDATE SET・UPSERT・COPY・NoSQL・DEFAULT）で文字数を検証し、n 超過は `22001`（TABLE-31・ERR-6）。超過部分が半角空白のみなら n 文字へ切り詰めて受理する。行 codec の厳格判定は最後の砦として残す。詳細は `docs/design/implementation-status.md` の Issue #1564 の項を参照。

## Issue #1566 追記: CHAR(n) の空白詰めと bpchar 比較

- 保存形は n 文字へ半角空白で詰めた値、比較・GROUP BY・ORDER BY・索引キーは末尾の半角空白を除いた値（TABLE-31）。`is_text_like`／`same_value_domain` は広げず、各 match 腕で Bpchar を個別に扱う（FK・ALTER・PK・UNIQUE は #1567 で解禁。正準キーはタグ 1 を TEXT と共有し FK の型照合は `fk_types_compatible` に集約）。詳細は `docs/design/implementation-status.md` の Issue #1566・#1567 の項を参照。

## Issue #2021 追記: BIT(n) 列と `<~>`・`<%>` の字句（TABLE-48 (1)(4)(5) ポインタ）

- **距離トークンは別型**: `lexer::BitDistanceKind { Hamming, Jaccard }` と `Token::BitDistanceOp` を新設し、f32 専用の `DistanceKind`（opclass 写像・f32 カーネル・`content_hash` のタグへ網羅的に接続済み）は拡張しない。拡張すると偽の写像か到達不能な腕が要り、ワイルドカード禁止方針が防ぐ fail-open の温床になるため。`allowlist` は新トークンを受理せず、ORDER BY・式への接続は距離カーネル側の Issue で扱う。
- **値表現**: `bit_string::BitString`（所有）・`BitRef`（借用）。`Value::Bytes` を流用しない（bytea として比較・整形される取り違えを防ぐ）。ビット順は MSB 先頭、末尾の詰め物ビットは 0。後続の popcount と `binary_quantize` も同じ順序を共有する前提。
- **カタログ**: タグ `bit`・param は `n` の 10 進（先頭ゼロ・符号・`-`・範囲外は復号で拒否）。`bit`・`varbit` は ENUM 名として予約する。`BIT(N)` は VECTOR 列の本数に数えない。
- **行 codec**: 自己記述型（presence ＋ `u32` LE ビット長 ＋ `ceil(len/8)` バイト）。encode でも宣言長との一致を再検証する。`ALTER TABLE ADD COLUMN` の DEFAULT は `TableSchema::bit_default_ref`（遅延キャッシュ）から借用で補完する。
- **エラー分類**: 文字種違反 `22P02`、ビット数不一致 `22026`（`ErrorClass::StringDataLengthMismatch` を新設）、`BIT(0)`・上限超過 `22023`、未対応の型綴り `0A000`。
- **対象外**: `B'1010'` ビット列定数、`'1010'::bit(4)`／CAST、`hamming_distance`・`jaccard_distance`、比較述語・ORDER BY・GROUP BY、wire のバイナリ形式（`bit_send`／`bit_recv`）と宣言 OID 1560 の Parse、COPY BINARY、`bit[]`（OID 1561）、`BIT VARYING`。距離カーネルの SQL 接続と `binary_quantize` は兄弟 Issue の担当。

## Issue #2014 追記: HALFVEC(N) 列（TABLE-46 (1)(4)(5) ポインタ）

- **保存先**: スカラーペイロード（行 codec のスカラー列）に f16 で保存し、embedding スロットは使わない。容量が次元 × 2 バイトになり、`storage` の行フォーマットを変えずに済む。HALFVEC のみの表は VECTOR 列を持たない表と同じ扱い（embedding は空）。
- **別 variant**: `ColumnType::HalfVec(u32)`・`row_codec::Value::HalfVec(Vec<u16>)`・`ScalarRef::HalfVec`・`SqlColumnTypeName::HalfVec`。`Value::Vector` を流用しないのは、UPDATE の割り当てが列型を見ずに `Value::Vector` を embedding として取り込むため。`ColumnType::is_vector()` は VECTOR 限定のまま（HNSW 宣言・DROP 保護・embedding 選択へ流入させない）。本数制約は `is_vector_family()`（VECTOR または HALFVEC）で数え、合わせて 0〜1 本。
- **フレーム**: presence ＋ 次元 `u32` LE ＋ 次元 × `u16` LE（f16 ビット）。encode は次元の一致・上限・有限性、decode は次元の上限と宣言次元の照合を確保前に行い、切り詰め・非有限ビットを `Invalid` で拒否する。カタログタグは `halfvec`（param は正準 10 進）。`content_hash` の値タグは 20。
- **丸めと SQLSTATE**: 入力は VECTOR と同じ検証（次元不一致・非有限は `22000`）の後に f16 へ丸める。有限の入力が ±Inf に飽和する場合は `22003`。判定は `fits_f16`（`|x| <= 65504`）ではなく変換結果で行い pgvector と揃える（65504 超〜65520 未満は 65504 へ丸めて受理、65520 以上は `22003`）。文字列 → f32 → f16 の 2 段丸めも pgvector と同順。
- **投影**: f16 を f32 へ広げた既存の `Cell::Vector` を再利用する（公開 enum `Cell` は増やさない）。結果予算は展開後（次元 × 4）で計上する。wire の型 OID は 25（text）で、バイナリ形式は対象外。
- **fail-closed（対象外）**: `ALTER TABLE ADD COLUMN`（`0A000`）、`DROP COLUMN`（`42601`）、`ALTER COLUMN TYPE`（`42804`）、PK・UNIQUE・FK・索引・CHECK・DEFAULT、距離演算子・距離関数・CAST（`::halfvec`）・比較・ORDER BY・GROUP BY・DISTINCT・集計・集合演算、COPY、CTAS・マテリアライズドビュー・`INSERT ... SELECT`、`halfvec[]`。距離計算・テキスト入出力・CAST は兄弟 #2015、HNSW は #2016 の担当。それ以外は担当 Issue 未定。

## Issue #2015 追記: HALFVEC の距離計算とテキスト入出力（TABLE-46 ポインタ。SQL-72・SQL-33・SQL-74 を併記）

- **計算方式**: 行の f16 を f32 へ広げ（誤差なし）、VECTOR と同じ f32 カーネル（`sql::vector_distance::distance`／`distance_fn`）で計算する。pgvector の halfvec 距離と同じ方式で、f16 のまま演算する系統（`gpu-batch-f16-arith.md`）は採らない（精度がさらに落ち、CPU の stable に f16 算術が無く、pgvector との値一致が崩れるため）。f16 の有限値は `|x| <= 65504` なので次元上限内では f32 の累積が溢れず、非有限になるのはゼロノルムのコサイン（NaN）だけ。
- **問い合わせ側も f16 へ丸める**: 型なしリテラルは halfvec 列の相手として halfvec に解決される（pgvector 同様）。束縛時に f16 へ丸めて f32 へ戻す（`sql::exec::round_to_halfvec_f32`）。次元不一致・非有限は `22000`、範囲外は `22003`。
- **式の束縛**: `BoundExpr::HalfVecColumnRef`（距離関数の引数の位置でだけ、識別子を直接見たときに生成）と `BoundExpr::HalfVecDistance`（距離演算子の式）を追加した。汎用の列参照の腕は HALFVEC を従来どおり拒否する（`VectorRef` を流用すると embedding が空になり黙って NULL が返るため）。裸の HALFVEC 列を算術・比較・`vector_norm`・ORDER BY・GROUP BY・CAST に使う形は従来どおり拒否。リテラル同士は `0A000`。
- **kNN（演算子形 `ORDER BY h <=> '..' LIMIT k`）**: 通常の kNN は arena の embedding スロット（VECTOR 専用・NULL 不可の前提）を読むため、HALFVEC 表の `Select` は束縛の直前に `sql::halfvec_knn::halfvec_select_as_scan` で全走査ソートの scan 経路（関数形 `ORDER BY l2_distance(..)` と同じ実行器）へ写す。RLS・WHERE・LIMIT／OFFSET・DESC は scan 経路が担い、NULL は ASC で末尾・DESC で先頭、ゼロノルムの NaN は NULL を除いた末尾、同点は id 昇順（`tests/sql_named/sql_halfvec_distance.rs` で固定）。変換の付け忘れは従来の `0A000` に戻るだけで開かない。演算子の範囲は VECTOR と同じ `<=>`・`<#>` のみで、`<->`／`<+>` は `0A000`（L2／L1 は関数形 `ORDER BY l2_distance(..)`／`l1_distance(..)` で Top-k を得る）。hybrid・`USING PLAN`・`USING MODE`・`HINT ORDER`・索引・GPU バッチは対象外。
- **テキスト入出力**: `'[..]'::halfvec[(N)]`・`CAST('[..]' AS halfvec[(N)])`・`halfvec '[..]'` は、被演算子が文字列リテラルの場合に限り構文段で畳み込む（`sql::cast::fold_halfvec_literal`）。検証（形式・有限性・長さ上限・修飾子との次元照合）→ f16 丸め → f32 へ戻した最短表記 `[a,b,..]` の文字列リテラルへ置換する。丸め済みの表記は再解析しても同じ値になる（冪等）ので、距離関数・距離演算子のリテラル引数にそのまま使え、投影すれば列の取り出しと同じ表記（`[0.099975586]`）で返る。結果は text（OID 25）。`CastType` は UDF の型・PL/pgSQL でも共有されるため HALFVEC を足していない。列・式の CAST（`h::text`・`h::halfvec`・`vector` との相互変換）・`halfvec[]`・バイナリ形式は `0A000`（または従来の拒否）のまま。
- **既知の差（pgvector）**: (a) VECTOR 列と `'..'::halfvec` の距離は暗黙変換を持たず、リテラル側が文字列になるため VECTOR の距離として動く（エラーにはならない）。(b) 投影の距離列は ORDER BY と同じ形に限る制約（#2007）が残り、ORDER BY の無い `SELECT h <-> '..' FROM t` は従来どおり `42601`（関数形 `l2_distance(h, ..)` は投影できる）。(c) `$n` を使う HALFVEC の距離は未確認（対象外）。
- **精度の実測**（`random_fixture_top_k_matches_f32_reference_within_tolerance`。200 行 × 32 次元、値は `[-1,1]` の線形合同法、問い合わせ 8 本、k = 10。VECTOR 表の f32 を参照とする）:

  | metric | recall@10 | 距離の最大絶対誤差 | 距離の最大相対誤差（参照の絶対値 > 1e-3 の行） |
  | ------ | --------- | ------------------ | ------------------------------------------- |
  | L2 | 1.00（8 本とも） | 5.6e-4 | 1.4e-4 |
  | コサイン | 1.00 | 1.4e-4 | 1.8e-4 |
  | 内積 | 1.00 | 1.5e-3 | 参照が 0 に近い行で最大 1.9e-1（打ち消しによる。絶対誤差で見る） |
  | L1 | 1.00 | 2.8e-3 | 1.4e-4 |

  一致の定義: HALFVEC の Top-k の各行について、f32 参照での距離が `d_k + 2δ` 以下であること。`δ` は要素ごとの f16 丸め誤差の上限（相対 2^-11、サブノーマルで絶対 2^-25）から metric ごとに導く（L2 は `2·√n·ε`、L1 は `2·n·ε`、内積は `n·(2ε+ε²)`、コサインは `2·(√n·ε/最小ノルム + √n·ε/クエリのノルム)`。いずれも実測は上限の内側）。f16 で厳密に表せる値（0.5 刻み）の fixture では、4 metric の関数形・演算子形の距離と Top-k が VECTOR 表とビット一致する。
