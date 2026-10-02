# NoSQL `update`／`delete` op の写像（NOSQL-6・NOSQL-12）

- Issue: #876（親 #861・ルート未指定。前提 #864〜#867・#869・#870・#875）
- 対象タスク: TASK-178・TASK-186
- 対象ビヘイビア: NOSQL-6・NOSQL-12（関連: SQL-17・SQL-18・RECOVER-1・RECOVER-10・TABLE-12・RLS-9）
- ステータス: Implemented（`where` 単一行形・`filter` 述語形の両方。述語形は Issue #1062 で実行結線）

## 背景

`POST /v1/query` の `op: update`／`op: delete` は Issue #875 で許可リスト
（`op.rs::Op`）と JSON スキーマ（`schema.rs::UPDATE_SCHEMA`／`DELETE_SCHEMA`／
`WHERE_ID_SCHEMA`）まで実装済みだったが、`gate.rs::handle` は engine 接続の
有無を問わず暫定応答（`0A000`／501）へ落としていた。本 Issue はこの 2 op を、
SQL 表層の `UPDATE ... WHERE id = <n> USING OPERATION_ID`（SQL-17）／
`DELETE FROM ... WHERE id = <n> USING OPERATION_ID`（SQL-18）と**同一の実行器**
（`engine::sql::exec::execute_update_with_schema`／`execute_delete` →
`tenant::update_row_columns_unchecked`／`delete_row_ledgered_unchecked`）へ、
SQL テキストを組み立てずに束縛済み計画で到達させる。

## 採用した設計

### D1: `filter`（述語形）は SQL 表層の述語形 DML と同一の実行器へ結線する（Issue #1062）

`where` を伴わず `filter` のみを持つ要求は、`filter: []`（空配列）の場合のみ
`dml_target.rs::DmlTargetError::EmptyFilter`（`42601`）で engine を一切呼ばず
拒否する（SQL の `WHERE` 句省略が構文エラーであることとのパリティ）。`filter`
が非空の場合は `TargetForm::Predicate` として、SQL 表層の述語形
`UPDATE`／`DELETE ... WHERE <述語> USING OPERATION_ID`（SQL-19・TASK-192）と
**同一の実行器**（`engine::core::EngineCore::
execute_bound_predicate_update_in_session`／
`execute_bound_predicate_delete_in_session`。`docs/design/
predicate-dml-exec.md`「NoSQL 表層からの到達経路」節）へ結線する。`where` と
`filter` の両方指定、および双方欠落はいずれも `42601`（対象指定なし）。

判定は `dml_target.rs::bind_target_form` に集約し、`update.rs`・`delete.rs`
がこの単一実装を共有する（第 2 の判定を作らない）。

`filter`（述語形）から `engine::sql::allowlist::WherePredicate` への写像は
`filter.rs::bind_filter_where_predicates` が担う。**不変条件**: 各要素は
まず `declare_leaf(column, op, value, schema)?` の `Leaf` を
`DeclarativeFilter::bind`（scan／search／aggregate の `filter` の葉と共通の
検証経路）に通してから構文形へ変換する——検証を経ない値は変換しない。
述語形 DML の `content_hash`（`for_update_where`／
`for_delete_where`）は束縛前の構文形 `WherePredicate` をハッシュ源にする
ため、SQL 表層の等価な文（`WHERE <col> = '<v>'`・`WHERE <col> LIKE
'<prefix>%'`）と同一の `WherePredicate` を生成することが SQL⇄NoSQL 台帳照合
（`23505`／`22023`）成立の必須条件になる。

既知の非対称:

- `prefix` は LIKE のメタ文字（`\`・`%`・`_`）を `filter.rs::like_escape`
  でエスケープしてから `%` を付与するため、パターン長が最大約 2 倍になる。
  `MAX_LIKE_PATTERN_LEN` 超過（`54000`）が scan／search の `filter` より
  短い入力長で起きうる。
- `eq` × `NUMERIC` は JSON 数値を文字列リテラル形へ変換した
  `WherePredicate::Equality` になる（SQL の `WHERE col = '1.5'` と同一
  ハッシュ）。SQL の裸の数値形 `WHERE col = 1.5`（式レーン）とは別ハッシュ
  になる（既知の差）。
- `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq` は
  scan／search／aggregate の `filter` と同じ理由（式レーンの入口が無い）で
  `0A000`（Issue #945）。
- PR #1118（filter 演算子の拡充。scan／search／aggregate 向け）が導入した
  `lt`／`le`／`gt`／`ge`／`in`／`or` は、述語形 DML では対象外のまま据え置く
  （PR #1118 が明示的に対象外とした範囲。`filter.rs::
  map_predicate_dml_items` が `eq`／`prefix` の 2 語彙のみを独立に検証し、
  `bind_filter`／`FilterNode` とは別経路を使う）。

### D2: `set` の JSON → `InsertLiteral` 写像は engine の `bind_update` を再利用

`update.rs::map_set_assignments` は `set`（JSON オブジェクト）を列型で分岐
しながら `Vec<(String, engine::sql::allowlist::InsertLiteral)>` へ写像する
（`insert.rs::bind_row` と同じ分岐設計）。禁止列（`id`／`tenant_id`／
`visibility`）・未知列の判定はここでは行わず、`schema.columns` に無い列名は
プレースホルダー値のまま `engine::sql::parser::bind_update`
（内部で `bind_set_assignments` を呼ぶ）へそのまま渡し、その判定結果
（禁止列 `42601`・未知列 `22000`）を透過する。これにより「禁止列・未知列の
判定基準」の単一情報源を engine 側に保つ（wire 側で二重実装しない）。

`set` の各キーは engine へ渡す前に `ident::check_identifier`（`42601`）を
通す（`table` に対する既存の同判断・PR #823 と同じ理由。untrusted な列名が
engine 側のエラー文言〔`unknown column: {name}`〕へ埋め込まれて `XX000` へ
縮退する経路を塞ぐ）。`set` が空オブジェクトの場合は `UpdateError::EmptySet`
として `42601`（SQL 文法は SET 項目 ≥ 1 を要求するため）。

`VECTOR` 列の値は **JSON 配列形のみ**を受理する（`insert` op と同じ
「文字列形のベクトルリテラルは受理しない」非対称）。**訂正（Issue #1061。
PR #1038〔Issue #896 レビュー指摘〕以降の実装に合わせた記述の是正）**:
各要素は `typed_json::vector_literal_values`（内部で
`engine::json::JsonNumber::as_f32` を呼ぶ。`insert.rs::bind_row` と同一の
単一丸め経路）で有限性を検証したうえで `Vec<f32>` を直接組み立て、
`InsertLiteral::Vector`（テキストリテラルの 64 KiB 上限を経由しない
`f32` 列の直接構築。SQL ベクトルリテラル文字列への直列化は経由しない）
として `bind_update`（内部で `engine::sql::parser::bind_vector_literal_values`
を呼ぶ。`sql::parser::parse_vector_literal` は経由しない）へ渡す。
`-0.0`（JSON `-0`）は `JsonNumber::as_f32` がビット列のまま `f32` へ
変換するため、SQL 表層の `-0.0` 保持契約（`content_hash` 一致）と自然に
揃う——文字列直列化を経由しないため、丸めによる符号情報の欠落は起こらない。

### D3: engine 側に NoSQL 向けセッション入口を 2 つ新設

`crates/engine/src/core.rs::EngineCore`（`execute_bound_insert_in_session`
の直後）に以下を追加した:

- `execute_bound_update_in_session<F>(ctx, table, operation_id, bind: F)`:
  `execute_bound_insert_in_session` と同型の binder closure 方式。判定順序
  （fail-closed。契約の一部）:
  1. `operation_id` 必須化ガード（`23502`。スキーマ取得より前）
  2. `read_txn_with_schema(table)` でスキーマ取得（テーブル不存在は
     `42P01`）
  3. `bind(&schema)` で `BoundUpdate` を得る（`read_txn` が開いている間に
     呼ぶ）。戻り値の `table`／`operation_id` が引数と一致することを検証
     （不一致は `22000`。早期ガードと実書き込みが異なる `operation_id` を
     使う経路を閉じる。PR #823 Bugbot 指摘と同種の多層防御）
  4. `read_txn` を drop してから `sql::exec::execute_update_with_schema`
     （束縛時点のスキーマ `Some(&schema)` を渡す）を呼ぶ
- `execute_bound_delete_in_session(ctx, bound: &BoundDelete)`:
  `sql::exec::execute_delete` への薄い委譲。`DELETE` は `id` 疑似列以外の
  列を参照しないため、`insert`／`update` のようなスキーマ突き合わせを伴う
  `bind` closure は不要（`core.rs::execute_delete_form` と同じ設計）。

`VectorCore` trait・`lib.rs` の公開シグネチャは変更しない
（`make core-api-check` で無変更を確認済み）。

### D4: 成功応答

`update`: `{"updated":<n>,"operation_id":"<escaped>"}`、`delete`:
`{"deleted":<n>,"operation_id":"<escaped>"}`（キー順固定・空白なし。
`insert::encode_success_body` と同型）。`n` は `0` または `1`。他テナント
所有 id・未存在 id はいずれも `200`・`n=0` で応答バイト列が完全一致する
（RLS-9。`execute_update_with_schema`／`execute_delete` の既存契約——
「対象行が不存在」と「対象行が存在するが他テナント所有」を区別しない——を
そのまま透過する）。

### D5: wire 側 `execute` の判定順序（契約）

`table` `required_str` → `ident::check_identifier`（`42601`） →
`where`/`filter` の形判定（両方 `42601`／どちらも無し `42601`／`filter` のみ
`0A000`） → 分割実行修飾（`mode`・`chunk`。Issue #1130。`mode` の値・`chunk` の値・
`mode`＋`where` は `42601`／`22000`。`chunk` のみは `42601`） → [`update` のみ] `set` 非空＋各キー `check_identifier`（`42601`）
→ JSON→`InsertLiteral` 写像（`22000`） → `where.id` は
`JsonNumber::as_exact_u64` のみ受理し、非受理時は variant で分岐する
（`dml_target.rs::DmlTargetError`）→ `operation_id`（`optional_str` の
`None` を `""` に読み替え `OperationId::parse` → `23502`） → engine 入口
（`23502` 再ガード → `42P01` → bind `42601`/`22000` → 実行
`23505`/`22023`/`XX000`）。

**`where.id` 非受理時の分岐は SQL 表層の字句解析・束縛とパリティを取る**
（実測で確認済み。`crates/wire-server/tests/nosql12_update_delete.rs::
update_rejects_non_integer_where_id_matching_sql_lexer_parity`）:

- `JsonNumber::Float{..}`（小数。`1.5` 等）→ `InvalidWhereId`（`22000`）。
  SQL 表層は `1.5` を単一の `Number` トークンとして読み `WHERE id = <n>`
  の単一行形へ振り分けるが、`sql::parser::bind_update`／`bind_delete` の
  `id_literal.parse::<u64>()` が失敗し `22000` になる
- `JsonNumber::NegInt(_)`（負の整数。`-0` を含む）→ `NegativeWhereId`
  （`42601`）。SQL 表層の字句解析は `-` を数値リテラルに含めず独立した
  `Punct('-')` として読むため `WHERE id = -1` は単一行形パターンに一致
  せず述語形へ振り分けられ、単一行 id 指定形専用エントリポイントが
  `UnsupportedSyntax`（`42601`）で拒否する

計画段階では両者とも `22000`（`insert.rs::bind_row` の `id` 疑似列判定と
同一と想定）としていたが、実測（`execute_update_sql` を直接呼ぶ一時
プローブテスト）で SQL 表層が負数と小数を異なる経路（字句解析段の
振り分け vs 束縛段のパース失敗）で拒否することを確認し、上記のとおり
修正した。

## 複数列 `set` の宣言順と `content_hash`（Issue #876 レビュー指摘の是正）

`engine::json` は JSON オブジェクトを `BTreeMap<String, JsonValue>`（キーの
アルファベット順）へパースする。`update.rs::map_set_assignments` はこの
`BTreeMap` をそのまま反復して `Vec<(String, InsertLiteral)>` を組み立てる
ため、NoSQL 表層の複数列 `set` は常にアルファベット順の
`BoundUpdate::assignments` を生成する。一方 SQL 表層の `SET col1 = ..,
col2 = ..` はクライアントが記述した宣言順をそのまま保持する
（`sql::parser::bind_update` のドキュメント参照）。

`recovery::content_hash::for_update_columns` 自体は渡された列スライスの
**宣言順**に依存してハッシュを計算する契約（`for_update_columns_differs_
by_declared_order` で固定済みの、この関数単体の低レベル契約）のまま
変更していない。代わりに、その唯一の呼び出し元である
`tenant::update_row_columns_unchecked` が `for_update_columns` へ渡す
直前に列を**スキーマの列 index（宣言順ではなく固定の列定義順）**へ
安定ソートするよう変更した。SET 対象の列は index 基準で適用されるため
並び順自体は書き込み結果に一切影響せず、ハッシュ入力のみを正規化できる
（`for_typed_insert` が挿入時に常にスキーマ列順の `named_columns` を渡す
既存契約と同じ考え方）。

この結果、SQL 表層がアルファベット順**でない**宣言順（例: `SET lang =
'en', embedding = '[...]'`）で書いた `UPDATE` と、同じ値を持つ NoSQL
表層の `update`（常にアルファベット順 `embedding, lang` へ正規化される）
は、同一 `operation_id` への再送であれば列の記述順に関わらず
**同一内容の再送（`23505`）として正しく判定される**。

回帰テストで固定済み（`crates/engine/tests/sql_update_single_row.rs::
resending_same_operation_id_with_different_set_clause_order_is_23505`・
`crates/wire-server/tests/nosql12_update_delete.rs::
cross_surface_multi_column_set_declared_out_of_alphabetical_order_is_
treated_as_duplicate`）。

### 正規化導入前の台帳エントリとの互換性（PR #992 レビュー指摘の是正）

上記の正規化（宣言順 → スキーマ列順）を導入する**前**に記録された台帳
エントリは、宣言順のままハッシュ計算されている。正規化後のコードが
それを常に「内容不一致」（`22023`）へ倒すと、アップグレード前に記録
済みの `operation_id` を同一 SQL で再送しただけの正当な操作が誤って
拒否されてしまう。

`tenant::update_row_columns_unchecked` は宣言順のまま計算した
`legacy_hash` も保持し、`ledger::record_in_txn_accepting`
（`ledger::record_in_txn` の一般化版）が正準ハッシュ（スキーマ列順）に
加えてこの宣言順ハッシュとも照合する。新規記録・以降の照合には常に
正準ハッシュのみを使う（keep-first 契約は変えない）。同一
`operation_id` だが内容が異なる再送は、正準ハッシュ・宣言順ハッシュの
いずれとも一致しないため引き続き `22023` になる（`22023` 契約は
弱めていない）。

回帰テストで固定済み（`crates/engine/src/tenant.rs::tests::
update_row_columns_resend_matches_pre_normalization_declared_order_
ledger_entry`）。

### 述語形 VECTOR 割当の表現統一と既存台帳エントリの互換性（Issue #1061）

述語つき `UPDATE ... SET <vector列> = '文字列リテラル'`（単一行形〔`WHERE
id = n`〕ではなく `filter` 相当・SQL 表層は `WHERE <述語>`）は、対象列が
`VECTOR` の場合、SQL 表層由来の `InsertLiteral::String`（タグ 1・生
テキスト）と NoSQL 表層由来の `InsertLiteral::Vector`（タグ 5・f32 LE
列。上記 D2 訂正の PR #1038）とで別ドメインのハッシュになっていた。この
ため、`filter` 結線（#871 系）後に同一 `operation_id` を表層を跨いで
再送すると、本来 `23505`（同一内容の再送）であるべきものが `22023`
（内容不一致）へ誤判定される余地があった。

- **影響範囲**: 変わるのは述語形 `UPDATE` で `VECTOR` 列を `SET` した
  `InsertLiteral::String` 割当を含むハッシュだけ。単一行形（`WHERE
  id = n`。束縛後の `Value::Vector` を `for_update_columns` でハッシュ
  するため、正規化前から表層横断で一致済み）・`DELETE`・`INSERT`・
  `UPSERT`・`VECTOR` 以外の列を対象とする述語形 `UPDATE` はビット不変。
- **正準化**: `VECTOR` 列を対象とする `String` 割当を
  `sql::parser::parse_vector_literal` でパースし、`InsertLiteral::Vector`
  と同一のタグ 5・f32 LE 列レイアウトへ揃える
  （`recovery::content_hash::push_dml_assignments`）。パース失敗は
  `Err`（fail-closed。タグ 1 へのフォールバックなし）。
- **legacy 受理の仕組み**: 正規化前に記録されえたハッシュ
  （`VECTOR` 列への `String` もタグ 1 のまま連結したもの）を
  `for_update_where_legacy_text_vector` で計算し、`content_hash::
  needs_legacy_vector_hash` が true の場合のみ `ledger::
  record_in_txn_accepting` の `legacy_hashes` へ渡す。新規記録・以降の
  照合には常に正準ハッシュのみを使う（keep-first 契約は変えない）。
  同一 `operation_id` だが内容が異なる再送は、正準ハッシュ・legacy
  ハッシュのいずれとも一致しないため引き続き `22023`（`22023` 契約は
  弱めていない）。
- **回帰テスト**: `crates/engine/src/recovery/content_hash.rs::tests::
  for_update_where_vector_string_and_vector_literal_match_across_surfaces`
  （表層跨ぎ一致の直接証明）・`for_update_where_legacy_text_vector_
  reproduces_pre_normalization_layout`（legacy 受理）・
  `crates/engine/src/tenant.rs::tests::update_rows_where_unchecked_
  resend_matches_pre_normalization_vector_string_ledger_entry`
  （legacy 台帳エントリとの互換）・`crates/engine/tests/
  sql_predicate_dml_exec.rs::predicate_update_set_vector_column_resend_
  content_hash_matches_spelling_variants`（表記ゆれ・`-0.0` 保持を
  production 経路で固定）。単一行形の表層跨ぎは
  `crates/wire-server/tests/nosql12_update_delete.rs::
  cross_surface_vector_value_*` で層 A 固定済み。述語形（`filter`。`WHERE`
  対象列は `VECTOR` 以外——`filter` の `eq` は `VECTOR` 列を対象にできない
  が、`SET` 側に `VECTOR` 列を含めることは述語形でも単一行形と同様に可能）の
  跨表層一致・台帳照合は Issue #1062・`crates/wire-server/tests/
  nosql12_update_delete.rs::
  cross_surface_predicate_update_with_vector_set_resend_with_same_content_is_duplicate`
  で固定する。

## 対象外・申し送り

- fault-injection（Issue #829）の update/delete 版
  `maybe_panic_after_http_*_commit`: `FaultKind` 語彙拡張を伴うため本 Issue
  では追加しない
- `three_client_http_e2e.rs` の update/delete パリティケース追加（述語形を
  含む）: Issue #877 の担当。述語形（`filter`）は複数プロセス・複数言語
  クライアントを要する同ファイルへは拡張せず、層 A
  （`nosql12_update_delete.rs`）でパリティ・台帳照合・RLS-9 を固定する
  （Issue #1062）
- RETURNING（Issue #873・PR #991）との統合: 本 Issue では `rows_affected`
  のみ
- `filter` の `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`・
  PR #1118（未マージ）が導入する演算子（`lt`／`le`／`gt`／`ge`／`in`／`or`）
  の述語形 DML への対応: Issue #945 の担当
- `bind_predicate_delete` 自体に「`visible()` 単独の述語」を拒否する
  ガードが無い点（SQL 表層側の既存事項）: NoSQL の入口
  （`reject_unsupported_predicate_dml_forms`）は `PredicateCall` を拒否する
  ため防御済みだが、SQL 表層自体のガード追加は別途申し送る

## 検証

- `crates/engine/tests/sql_update_delete_session_public_api.rs`:
  `execute_bound_update_in_session`／`execute_bound_delete_in_session` の
  到達性・判定順序・SQL 表層との台帳キー空間共有（同一 `operation_id` の
  表層を跨いだ再送が `23505`／`22023` になること）を固定
- `crates/wire-server/src/http/query/{dml_target,update,delete}.rs` 内
  unit tests: `bind_target_form`・`map_set_assignments` の境界
- `crates/wire-server/tests/nosql12_update_delete.rs`: production ルータ
  経由（生バイトクライアント）での契約全体（成功・`operation_id` 必須化・
  未存在テーブル・`42601`／`22000` 各ケース・台帳照合の表層横断パリティ・
  RLS-9 応答バイト同一性・`filter` のみの `0A000`・`explain: true` 拒否）
- `crates/wire-server/tests/nosql9_op_allowlist.rs`・`nosql1_op_vocabulary.rs`:
  既存の placeholder 前提テストを実行到達テストへ更新
