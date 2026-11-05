# NoSQL `create_index`／`drop_index` op の形（NOSQL-19・SQL-23・INDEX-7・INDEX-8）

- Issue: #2107（親 #2106。結線は #2108、スコープの強制は #2101）
- 対象ビヘイビア: NOSQL-19（関連: NOSQL-1・NOSQL-13・SQL-23・SQL-33 (5)・INDEX-7・INDEX-8・HTTP-15）
- 関連 doc: [nosql-ddl-mapping.md](nosql-ddl-mapping.md)・[index-ddl-declaration.md](index-ddl-declaration.md)・
  [nosql-api.md](../../crates/wire-server/docs/nosql-api.md)
- ステータス: Accepted（Issue #2108 で実行結線済み）
- 本書は本リポで確定した設計判断の記録であり、private spec の本文は転記しない（ID はポインタ）

## 背景

NoSQL 表層（`POST /v1/query`）の DDL は `create_table`／`alter_table`／`drop_table` の 3 op のみで、
`create_index`／`drop_index` は語彙外（`0A000`）である。SQL 表層の `CREATE INDEX`／`DROP INDEX` は
`USING btree｜hnsw｜gin`・HNSW の opclass・`WITH (m, ef_construction)`・GIN の
`jsonb_ops`／`jsonb_path_ops` まで実装済みで、#2108 が実装に入る前に NoSQL 側の op 形を固定する。

## 設計方針

- `nosql-ddl-mapping.md` の DDL 3 op と同じく「JSON → 字句トークン列 → SQL 表層と同じ構造検証・実行器」
  とする。**第 2 の実行器・第 2 の権限判定は作らない**
- untrusted な JSON 文字列を `Token::Ident` へ直接流用しない。識別子は `ddl.rs::ident_token`、
  `method`／`opclass` は固定語彙の `match`、数値は検証済み u32 の 10 進化だけで作る
- キー `kind` は採らない（却下案 (a)）。未知キーは `ObjectSchema` の一般則で `42601`

## キー集合

| op | キー | 必須 | 型 | 備考 |
| -- | ---- | ---- | -- | ---- |
| `create_index` | `op` | ○ | string | `"create_index"` |
| | `name` | ○ | string | 索引名。`ident_token` で検証 |
| | `table` | ○ | string | 対象の表。`ident_token` で検証 |
| | `columns` | ○ | string[] | 1 件以上。各要素を `ident_token` で検証。件数上限は `Vec` 確保前に検査（`54000`） |
| | `method` | 任意 | string | `"btree"`／`"hnsw"`／`"gin"`（小文字・完全一致）。省略時は `USING` 句を出さない |
| | `opclass` | 任意 | string | 下記の固定語彙 |
| | `with` | 任意 | object | `{"m"?, "ef_construction"?}`（非負整数）。未知キーは `42601` |
| `drop_index` | `op` | ○ | string | `"drop_index"` |
| | `name` | ○ | string | `ident_token` で検証 |

成功応答は 2 op とも `{"ok":true}`（件数は返さない）。`operation_id` は要求しない（DDL 3 op と同じ）。

## トークン列への写像

```text
create_index → CREATE INDEX <name> ON <table> [USING <method>] ( <col> [<opclass>] [, <col> ...] )
               [WITH ( m = <n> [, ef_construction = <n>] )]
drop_index   → DROP INDEX <name>
```

- `with` の値は `Validated::optional_u32` で取り 10 進テキストの `Token::Number` にする。キーは
  `m` → `ef_construction` の固定順。範囲検査・`ef_construction >= 2*m`・省略キーの補完は engine の
  `catalog::HnswIndexOptions::new` に一本化し（範囲外は `22023`）、wire に上限定数を複製しない。
  `with: {}` は SQL の `WITH ()` と同じく `42601`
- 得たトークン列は SQL 表層と同じ `validate_create_index_tokens`／`validate_drop_index_tokens` →
  `ParsedSql::CreateIndex`／`DropIndex` → `EngineCore::execute_parsed_in_session` へ渡す

## `opclass` の語彙と組み合わせ

- 受理は `vector_cosine_ops`／`vector_l2_ops`／`vector_ip_ops`／`vector_l1_ops`／`jsonb_ops`／
  `jsonb_path_ops` の 6 つのみ（小文字・完全一致）
- 語彙外（大文字混じり・`halfvec_*_ops` を含む）・文字列以外・`columns` が 1 件でないのに指定、は
  NoSQL 側で `42601`（固定文言。入力値は echo しない）
- 組み合わせの判定は engine に任せ、wire で先回りしない（SQLSTATE を SQL 表層と揃えるため）

| 組み合わせ | 判定 | wire_code |
| ---------- | ---- | --------- |
| `method` 省略・`btree` に `opclass` | engine（構文段） | `42601` |
| `hnsw` に `jsonb_*`、`gin` に `vector_*` | engine | `42704` |
| `hnsw` 以外に `with` | engine | `0A000` |
| `vector_*_ops` を `HALFVEC` 列へ（opclass の系列と列型の不一致） | engine | `42804` |
| `hnsw`／`gin` で列が複数 | engine | `0A000` |

## 実行権限

- `principal.ddl_allowed()` が真のときだけ `SessionState::allow_ddl()` を呼ぶ。権限判定は engine の
  `require_ddl_permission`（カタログ照会より前）1 か所のみ。wire に第 2 の判定は置かない
- 権限の無い主体には、表・索引の有無にかかわらず常に同一の `42501`（HTTP 403）。存在のオラクルにしない
- 許可元は `--ddl-allowed-users`（`crates/wire-server/src/ddl_permission_opt.rs`）。権限による判定
  （SQL-40）が確定した後はそちらへ追従する
- 表の世代の更新は engine の `Storage::create_index`／`drop_index` が行い、wire は関与しない

## セッションスコープ（HTTP-15）との関係

強制は #2101／#2108 の担当。本書は期待する挙動のみ固定する。

- `read_only` スコープでは 2 op とも `42501`。`ops` スコープは通常どおり適用
- `tables` スコープ: `create_index` は `table` キーで照合。`drop_index` はカタログを引かないと対象の表を
  決められないため、`tables` 制限のあるセッションでは常に `42501`（fail-closed・存在の秘匿）

## エラー分類

| wire_code | 原因 | 判定層 |
| --------- | ---- | ------ |
| `42501` | DDL 権限なし | engine（カタログ照会より前） |
| `42P07` | 名前が表・ビュー・索引と衝突 | engine |
| `42P01` | 対象の表が不在 | engine |
| `42703` | 列が不在 | engine |
| `42809` | 対象がビュー・索引、または drop にテーブル名 | engine |
| `42704` | `drop_index` の対象が不在、opclass と方式の不一致 | engine |
| `0A000` | 列複数の `hnsw`／`gin`、`hnsw` 以外の `with`、索引方式と列型の非対応 | engine |
| `42804` | opclass の系列と列型の不一致（`vector_*_ops` を `HALFVEC` 列へ指定する等。固定文言） | engine |
| `22023` | `with` の範囲違反、`ef_construction < 2*m` | engine |
| `42601` | 未知キー・型不一致・識別子不正・語彙外・`opclass` で列数が 1 でない・`with: {}` | wire |
| `54000` | 列数上限 | wire（確保前）と engine |

明示トランザクション中の DDL 拒否（`0A000`）は NoSQL では起きないため注記のみ。

## SQL との意図した差分・対象外

- SQL では語彙外の opclass が `42704`、NoSQL は固定語彙の外として `42601`（参照アクションの語彙外を
  `42601` とした前例と同じ）
- `halfvec_*_ops` は受理しない。halfvec 列の HNSW は `opclass` を省略すれば列型の既定系列が使われる
- `with` の負数・小数・u32 超過は、SQL では `22023`、NoSQL では JSON 型不一致の `42601`
- 次はキーを持たないため未知キーとして `42601`: UNIQUE・`IF [NOT] EXISTS`・`COLLATE`・式索引・
  部分索引の述語・`CONCURRENTLY`／`INCLUDE`。`columns` の非文字列要素は型不一致
- `create_view`／`drop_view` は引き続き対象外（`0A000`）

## 却下した代替案

- (a) `kind` キー方式: SQL の `USING` と語が対応せず、`method` に揃えるほうが写像が素直
- (b) `method`／`opclass` を `ident_token` で engine へ素通しする方式: engine のエラー文言が入力値を
  echo し、NoSQL の固定文言方針に反する

## #2108 での対応（実装済み）

- `validate_create_index_tokens`／`validate_drop_index_tokens` を `pub` にした（Issue #910 の DDL 3 種と同じ扱い）
- `CREATE`／`INDEX`／`ON`／`USING`／`WITH` が `Token::Ident`、`=` が `Punct` になることは、
  `ddl.rs` の単体テストが `tokenize("CREATE INDEX ...")` との比較で固定し、engine 側は
  `sql_ddl_tokens_public_api.rs` が `parse_sql` の結果との一致で固定する
- 写像と実行は `http/query/ddl.rs`、層 A の結合テストは `tests/http_nosql/nosql19_index_ddl.rs`
- `drop_index` は `table` を持たないため、スコープ検査（`gate.rs` の `scope_targets`）は対象表を空列で返す。
  `tables` 制限のあるスコープでは常に `42501`（fail-closed）、制限が無ければ ops／`read_only` の判定だけを行う
