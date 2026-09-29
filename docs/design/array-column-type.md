# ARRAY 列型（複合型のうち配列部分）（Issue #888）

- ステータス: Accepted（実装済み）
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-1, TABLE-6,
  TABLE-7, TABLE-14・TASK-198・WIRE-13・ERR-1・ERR-2・ERR-4

## 背景・目的

`VECTOR(N)` は固定次元の f32 配列専用で、検索カーネル・ANN・hybrid の入力に
なる。可変長の同型配列（タグ列など）を一般の列として保持・往復させる型が
無かった。本 Issue では複合型のうち配列部分（TABLE-14 の `<スカラー型>[]`）を
追加する。

着手時点（origin/main）では要素型候補のうち TEXT・BOOLEAN のみがマージ済み
だったため、要素型は `ArrayElemType`（`Text`／`Bool` の 2 値）に限定した。
数値・日時要素型は兄弟実装のマージ後に追加できるよう `from_catalog_tag`
一箇所へ判定を集約してある。

## 設計判断

### D-A1: 型表現

- `catalog::ColumnType::Array(ArrayType)` を追加。`ColumnType` は非
  `non_exhaustive`・ワイルドカード腕禁止のまま維持し、コンパイラに全
  ディスパッチ地点を列挙させる（`column-type-extension.md` D1 を踏襲）。
- `ArrayType` は `Copy` 構造体・フィールド private。生成は
  `ArrayType::new(elem, max_len) -> Result`（範囲 `1..=MAX_ARRAY_ELEMENTS`
  ＝ 1,024。本リポの実装既定値）。アクセサー `elem()`／`max_len()`。
- `ArrayElemType` は `VECTOR`・`ARRAY`（入れ子・多次元）を構造的に除外する
  （`Text`／`Bool` の 2 値のみ）。1 次元・可変長（0〜`max_len` 要素）限定で
  多次元配列・固定長配列は非対応（固定長数値配列は `VECTOR(N)` の責務）。
- 宣言経路: SQL `CREATE TABLE` 構文（SQL-23・#899）は未実装のため、宣言は
  Rust API（`Catalog::create_table`／`ColumnDef::new(.., ColumnType::Array(..),
  nullable)`）経由のみ（BOOLEAN と同じ扱い）。

### D-A2: カタログ v2 の文法

- 型タグは `"array"`、`param` は `"<elem_tag>,<max_len>"`（例: `text,64`）。
  `validate_catalog_param` の許容文字 `[A-Za-z0-9_,]` に収まる。
- `from_catalog_fields` の検証: カンマ区切りがちょうど 2 要素、elem_tag が
  配列要素として許される型（`vector`／`array`／パラメータつき型を拒否）、
  max_len が 10 進正準形（先頭ゼロ等を `s != n.to_string()` で拒否）、
  max_len が `1..=1024` の範囲。違反はすべて `CatalogError::Invalid`。
- TEXT/VECTOR/BOOLEAN の列行バイト列・v2 ヘッダは不変（golden テストで固定）。

### D-A3: 行バイト表現（スカラーペイロード内）

presence(1) → flags(1・`0x00` 固定。NULL 要素ビットマップ等の将来予約) →
要素数(`u32 LE`) → ペイロード長(`u32 LE`) → 要素列。

- TEXT 要素: `u32 LE` 長 + UTF-8 本文
- BOOL 要素: 1 バイト（`0x00`/`0x01`）

decode の検証順序: 要素数 ≤ `min(スキーマ max_len, MAX_ARRAY_ELEMENTS)` →
ペイロード長 ≤ 残りバッファ長かつ ≤ `MAX_ARRAY_PAYLOAD_LEN`（`MAX_TEXT_FIELD_LEN`
と同値）→ 要素列を構造・UTF-8 検証しながら読む（`row_codec::decode_array_elements`
／`parse_array_frame`。未参照列でも常に全検証。Issue #350／PR #369 と同方針）。

フレーム長計算は `row_codec::scalar_array_entry_len` に集約し、
`encode_scalar_columns`・`merge_encode_scalar_columns`・
`tenant::validate_set_assignments`（UPDATE SET の事前検証）が共有する
（事前検証と実エンコードの乖離によるテナント境界漏えいを防ぐ。既存 TEXT の
`scalar_text_entry_len` と同じ理由）。

### D-A4: 値型・借用型・結果セル

- `row_codec::Value::Array(ArrayValue)`（`ArrayValue::Text(Vec<String>)`／
  `Bool(Vec<bool>)`）。
- `row_codec::ScalarRef::Array(ArrayRef<'a>)`。`ArrayRef` は `Copy`・走査時に
  構造・UTF-8 を検証済みの借用（`bytes` は要素列のみ）を保持し、
  `to_value()` で複製する。`as_text()`／`as_bool()` は `Array` に対し常に
  `None`（fail-closed。TEXT 前提の消費側へ届かない）。
- `sql::exec::Cell::Array(ArrayValue)`。
  - wire（`result_encoder.rs`）: `RowDescription` の OID は既存の text（25）の
    まま変更しない（WIRE-13）。値は PostgreSQL 配列テキスト形式
    （`{a,b}`。空配列は `{}`、BOOL 要素は `t`/`f`）で描画する。TEXT 要素の
    うち空文字列・`,{}"\`／空白を含むもの・大小無視で `NULL` に一致する
    ものは `"..."` で囲み `"`／`\` をバックスラッシュでエスケープする。
  - NoSQL 応答（`http/query/response.rs`）: ネイティブ JSON 配列で描画する。
  - 投影時の確保は既存の `MAX_CANDIDATE_SCALAR_BYTES`／
    `try_alloc_text_for_budget` 系の予算に要素本文を計上する
    （`try_alloc_array_for_budget`／`try_clone_array_for_budget`）。

### D-A5: SQL リテラル（INSERT／UPDATE SET／UPSERT）

字句解析器は変更せず、VECTOR と同じく文字列リテラル `'{...}'` を束縛時に
`sql::parser::parse_array_literal(literal, array_ty)` で解釈する。

1. リテラルのバイト長 ≤ `MAX_ARRAY_LITERAL_BYTES`（4 MiB。`MAX_TEXT_FIELD_LEN`
   と同値の実装既定値）を先に確認（超過は `54000`）。
2. `{`〜`}` で囲まれていることを確認。
3. 1 パスの状態機械で要素を切り出す（引用・バックスラッシュエスケープ・
   引用なし要素の前後空白除去に対応）。要素数が `max_len` を超えた時点で
   打ち切り `54000`。リテラル自体のバイト長上限（1）が総確保量の上限を
   兼ねるため、累計エンコード長の追加チェックは不要（要素は元リテラルの
   部分文字列であり、確保量がリテラル長を超えない）。
4. 要素型ごとに変換（BOOL は `t|f|true|false` を大小無視で受理）。

形式違反（入れ子の `{`、閉じていない引用、末尾カンマ、BOOL の不正語）は
当初は `22000` へ統一していたが、Issue #1187 で形式違反は `22P02` へ変更した（NULL 要素は機能未対応のため `22000` のまま）。

NULL 要素（D-A6）: 引用なしの `NULL`（大小無視）は `22000` で拒否。引用つき
`"NULL"` は TEXT 要素の文字列 `NULL` として往復する。

`(ColumnType::Array(t), InsertLiteral::String(s))` を上記で束縛する
（`Number`/`Bool` リテラルは型不一致として拒否）。対象は
`sql::parser::bind_insert`／`bind_set_assignments`（UPDATE SET・SQL-17/19 共有）／
`bind_upsert_assignments`（`ON CONFLICT ... DO UPDATE SET`）の 3 箇所。
ファイル形 INSERT（`bind_file_insert`）は BOOLEAN と同じく対象外として拒否。

### D-A6: NULL 要素

NULL 要素は受理しない。理由: (1) 等価述語（対象外だが将来追加時）の三値論理を
避ける、(2) エンコードを単純にできる、(3) flags バイトを予約済みのため
将来フォーマット版を上げずに緩和できる。列そのものの NULL（presence
`0x00`）と空配列 `{}` は区別して往復する。

### D-A7: VECTOR・検索経路との境界

`TableSchema::vector_dim()`／`sql::parser::vector_column`／`text_column_index`
はいずれも Array を非該当として除外する。hybrid 本文列規則（TEXT 限定）・
`sql::using_plan::body_column_index` も同様に拒否側へ倒す。
`sql::scalar_index::ScalarIndex` は Array 列を索引化しない
（`per_column.push(None)`）。`sql::scalar_plan`・アリーナ・HNSW・疎索引・
GPU 経路は Array 列を一切読まない（masked 走査で不要列として飛ばすのみ）。

`tests/composite_types.rs::vector_search_is_bit_identical_with_and_without_array_column`
で、Array 列を持つテーブルと持たないテーブルの KNN 結果（id・score のビット
一致）が完全に一致することを固定した。

### D-A8: 述語・集計・その他の表層

- WHERE で Array 列を参照した場合（等価・`IS NULL` を含む）は既存の型不一致
  エラーで fail-closed に拒否（対象外・申し送り）。要素・パス演算子
  （`tags[1]`・`@>`・`->`）は字句解析の時点で `42601`。
- 集計: `COUNT(<Array 列>)`（非 NULL 行数）のみ受理
  （`AggregateInput::ArrayColumn`）。`SUM`/`AVG`/`MIN`/`MAX` は `22000`。
  `GROUP BY` キーは引き続き TEXT 限定。
- `RETURNING`（`sql/returning.rs`）: `Value::Array` から `Cell::Array` へ
  予算つきで複製する（`try_clone_array_for_budget`）。
- NoSQL: `http/query/insert.rs`・`update.rs` は Array 列への JSON 値を明示的に
  拒否する（`22000`）。JSON 配列の束縛は #896（NOSQL-17）へ申し送り。

### D-A9: content_hash（台帳の内容照合）

`recovery/content_hash.rs::push_value` に `Value::Array` のタグ `10` を追加
（0=Null／1=Text／2=Vector／7=Bool は既存、3〜6・8〜9 は他型向け予約）。
ハッシュ入力は「タグ(10)、要素型タグ(1B: Text=0/Bool=1)、要素数(u64)、各要素
（TEXT は長さプレフィックス＋本文、BOOL は 1 バイト）」で単射（`{ab}` と
`{a,b}` が衝突しない）。

## 対象外（申し送り。Issue 起票はユーザー承認後）

- 配列列への等価（`=`・`IN`）・`IS NULL` 述語（TABLE-14）
- NoSQL の JSON 配列束縛と応答パリティ（#896・NOSQL-17）
- `22P02` の新設: Issue #1187 で解消
- SQL `CREATE TABLE` 構文での `<型>[]` 宣言（#899）
- 数値・日時の要素型（兄弟 PR マージ後）
- NULL 要素対応（flags バイトの予約を使う）
- 配列列の二次索引化

## テスト

- `crates/engine/src/row_codec.rs`：encode/decode 往復・空配列・NULL 列との
  区別・要素数超過・型不一致・flags 非 0・不正 UTF-8・不正 bool バイト・
  `scan_scalar_columns`／`merge_encode_scalar_columns` 往復（unit）。
- `crates/engine/src/catalog.rs`：カタログ v2 往復・malformed param・非正準
  max_len・上限超過・入れ子要素型拒否（unit）。
- `crates/engine/src/sql/parser.rs`：`parse_array_literal` の受理・拒否
  パターン一式（unit）。
- `crates/engine/tests/composite_types.rs`：INSERT/SELECT/UPDATE 往復・要素数
  上限・NULL 要素拒否・RLS 分離・KNN 非影響・WHERE 拒否・COUNT/SUM の可否
  （結合）。
- `crates/wire-server/src/result_encoder.rs`：PG 配列テキスト表現・引用
  エスケープ・空配列（unit）。
- `crates/wire-server/tests/wire_array_column.rs`：simple query 経由の
  往復・NULL 列と空配列の区別・要素数上限（`54000`）・形式違反（`22000`）・
  WHERE 拒否（層 A）。

## Issue #896 追記

NoSQL 表層の JSON 束縛（`insert`／`update`／`filter`）の型別対応・`columns[].type` の型名整備は Issue #896（NOSQL-17）で実施済み。詳細は `docs/design/nosql-typed-json-binding.md` 参照。
