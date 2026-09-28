# `FOREIGN KEY` 制約の設計判断

Issue #907・対象ビヘイビア: TABLE-17（TASK-205）。関連ポインタ: TABLE-12
（物理キー `(tenant_id, id)`）・TABLE-15（`DROP TABLE` の依存オブジェクト検査）・
TABLE-16（主キー・UNIQUE・単一検査点）・RLS-9・RLS-10 (c)（他テナントの存在情報の
非漏えい・可視性を問わない判定母集合）・ERR-6（新設 `wire_code` と HTTP 射影）・
SQL-31・TASK-221（明示トランザクション）。

Issue #1077（`MATCH FULL`・`DEFERRABLE`）で D5・D6・「対象外・後続候補」を改訂し、
D13〜D15・「COMMIT 時の遅延検査」節を追加した。この拡張は `docs/spec` の
TABLE-17・TASK-205 が定める範囲を超える実装拡張であり、spec 側に対応する
ビヘイビア ID は無い。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本
ドキュメントは本リポ側の実装判断・設計記録のみを扱う。

## 決定事項

| # | 決定 | 理由 |
| --- | ---- | ---- |
| D1 | 参照先列は、参照先テーブルの `id` 疑似列（物理キー）・宣言済み主キー・UNIQUE 制約のいずれかと**列集合**が一致すること。それ以外は `42830` | 一意性を保証しない列を参照先にすると、同値の参照先行の 1 行を削除しても残りが参照を満たし続ける等、`NO ACTION` の意味論が定まらない |
| D2 | 参照先列の省略（`REFERENCES <t>`）は参照先の主キー、未宣言なら `id` へ解決し、解決済みの列名をカタログへ永続化する | PostgreSQL と同じ規約。解決結果を永続化することで、後から参照先が変わっても宣言の意味が変わらない |
| D3 | 参照元列と参照先列の型は位置ごとに一致すること（型タグ＋パラメータ。ENUM は型名を含む）。`id` 参照の参照元列は `INTEGER`／`BIGINT`。不一致は `42830` | 参照先の照合を一意性検査と同じ型タグ付き正準キーで行うため、型が異なる組は常に違反になる（黙って常に失敗する宣言を受理しない） |
| D4 | SQL 表層 `CREATE TABLE` の列型へ `INTEGER`／`BIGINT` を追加（`NOT NULL`／`DEFAULT <数値>`／`UNIQUE`／`PRIMARY KEY` も受理） | `id` を参照する参照元列を SQL で宣言するための最小限の前提整備 |
| D5 | 参照動作は既定の `NO ACTION`（非遅延のため `RESTRICT` と同値）のみ。`ON DELETE`／`ON UPDATE` には `NO ACTION`／`RESTRICT` だけを受理し、`CASCADE`／`SET NULL`／`SET DEFAULT`・`CONSTRAINT <name>` 前置は `42601`。`MATCH`（D13）・遅延属性（D14）は Issue #1077 で受理するようになった | 対象外の動作を黙って既定動作へ丸めない（fail-closed） |
| D6 | `MATCH SIMPLE`（既定）は NULL を含む値の組を検査しない。`MATCH FULL`（D13）は全 NULL の組のみ検査しない | PostgreSQL の既定 |
| D7 | 検査は文単位・即時。台帳記録・行の書き込みの**後**、テーブル世代 bump・commit の**前**に同一 write トランザクション内で行う | 既存の制約検査（TABLE-16）と同じ位置。`operation_id` の再送判定（`23505`／`22023`）が本検査より優先される |
| D8 | 自己参照を受理する。循環参照は `CREATE TABLE` の時点で参照先が存在する必要があり、`ALTER TABLE ... ADD FOREIGN KEY` を持たないため、自己参照以外の循環は構造的に作れない | 自己参照は参照元＝参照先のスキーマで解決・検査でき、特別な経路を要さない |
| D9 | 参照先名がビュー・索引名なら `42809`、存在しなければ `42P01`。作成対象名の重複（`42P07`）はそれらより先に判定する | テーブル・ビュー・索引は名前空間を共有する（`CREATE TABLE` の既存判定と同じ順序） |
| D10 | 参照先テーブルの `DROP TABLE` は他テーブルから参照されていれば `2BP01`（データの有無を問わずカタログのみで判定）。自己参照は依存に数えない | TABLE-15 |
| D11 | 参照元列の `DROP COLUMN` は `DependentObjectsStillExist` で拒否。参照先側の列は主キー・UNIQUE 構成列（既存の検査で拒否済み）か `id`（予約列）に限られる | `DROP CONSTRAINT` を持たないため、制約を黙って消す暗黙 cascade を作らない |
| D12 | 宣言面は SQL 表層の `CREATE TABLE` のみ（`ALTER TABLE ... ADD COLUMN ... REFERENCES` は `42601`）。Rust API の `TableSchema::with_foreign_keys` は `pub(crate)` | 後付けの宣言は既存の全テナント行の検証を要し別設計になる |
| D13 | `MATCH {SIMPLE\|FULL}`（既定 `SIMPLE`）を受理する。`MATCH FULL` は複合 FK で NULL と非 NULL が混在する組を違反にする（単一列は `SIMPLE` と同じ挙動）。`MATCH PARTIAL` は非対応のまま `42601` | PostgreSQL の 3 値のうち実装コストに見合う 2 値のみを対象にする |
| D14 | `[NOT] DEFERRABLE`／`INITIALLY {DEFERRED\|IMMEDIATE}`（任意順・各グループ高々 1 回）を受理する。`INITIALLY DEFERRED` の宣言だけが、明示トランザクション中の文単位検査を COMMIT まで遅延できる。`SET CONSTRAINTS`（`DEFERRABLE INITIALLY IMMEDIATE` を実行時に遅延へ切り替える機能）は非対応のため、`DEFERRABLE`（`INITIALLY IMMEDIATE` 相当）は文単位検査のまま変わらない | `SET CONSTRAINTS` 抜きでも `INITIALLY DEFERRED` だけで自己参照・相互参照する初期データ投入のユースケースをカバーできる |
| D15 | 遅延は「検査しない」ことを意味しない。autocommit（1 文＝1 トランザクション）は宣言に関わらず必ず文単位で検査する。省略できるのは明示トランザクション中の `INITIALLY DEFERRED` の文単位検査だけで、COMMIT 時にまとめて検査する（下記「COMMIT 時の遅延検査」節） | 「遅延」を「検査省略」と混同すると、autocommit や `SET CONSTRAINTS` 相当の切り替えが無い経路で fail-open になる |

## 構文

```
CREATE TABLE <table> (
  <col> <type> [<列制約>]* [REFERENCES <parent> [(<pcol>[, <pcol>]*)]
                            [MATCH (SIMPLE|FULL)] [<参照動作>]* [<遅延属性>]*]
  | FOREIGN KEY (<col>[, <col>]*) REFERENCES <parent> [(<pcol>[, <pcol>]*)]
                                   [MATCH (SIMPLE|FULL)] [<参照動作>]* [<遅延属性>]*
  [, ...]
) [;]

<参照動作> ::= ON DELETE (NO ACTION | RESTRICT) | ON UPDATE (NO ACTION | RESTRICT)
<遅延属性> ::= [NOT] DEFERRABLE | INITIALLY (DEFERRED | IMMEDIATE)
```

- 列制約 `REFERENCES` は `PRIMARY KEY` の後ろ・`CHECK` の前に高々 1 個置ける。
- 表制約は列リスト中の任意の位置に置ける（列数上限の判定対象外。`PRIMARY KEY`／
  `UNIQUE`／`CHECK` の表制約と同じ位置非依存の判定）。参照元列の実在は列リスト
  全体の構文判定後に判定する（未宣言列・`id` は `42601`）。
- 同一リスト内の列名重複は `42701`、件数・列数の上限（32）超過は `54000`。
- `MATCH` は列リストの直後・`ON` 句の前にのみ置ける（PostgreSQL の句順序）。
  `MATCH PARTIAL`・重複指定・`ON` 句より後ろに置いた `MATCH` は `42601`（D13）。
- 遅延属性は `ON` 句の後ろに任意順・各グループ（`[NOT] DEFERRABLE` と
  `INITIALLY ...`）高々 1 回で置ける。矛盾（`NOT DEFERRABLE INITIALLY DEFERRED`）・
  重複は `42601`（D14）。`REFERENCES` 句の後ろで `NOT` を消費するのは次の識別子が
  `DEFERRABLE` のときだけで、他の `NOT ...`（`NOT NULL` 等）は消費しない。
- `REFERENCES`／`FOREIGN`／`ON`／`NO`／`ACTION`／`RESTRICT`／`MATCH`／`SIMPLE`／
  `FULL`／`DEFERRABLE`／`INITIALLY`／`DEFERRED`／`IMMEDIATE` は `lexer::Keyword` へ
  含めない文脈的キーワード。

## 永続化: カタログ v8／v9（`TableSchema` の拡張）

`TableSchema` に `foreign_keys: Vec<ForeignKeyDef>` を追加し、`FOREIGN KEY` を
1 件以上持つスキーマはカタログ v8 で永続化する。v8 は v7 の上位集合で、`pk:` 行・
6 フィールドの列行・`uniq:` セクション（0 件可）・`checks:` セクション（v8 に限り
0 件可）の後ろに `fks:<n>`（`n >= 1`）と `n` 行の
`fk:<col1,col2,...>:<parent_table>:<pcol1,pcol2,...>` を追記する。`FOREIGN KEY` を
持たないスキーマは従来どおり v2〜v7 のままバイト列を変えない。

`ForeignKeyDef` に `match_type`（`ForeignKeyMatch`）・`deferrability`
（`ForeignKeyDeferrability`）を追加した（Issue #1077）。**既定以外**の値
（`Full`・`NotDeferrable` 以外）を 1 件でも持つスキーマはカタログ v9 で永続化する。
v9 は v8 の上位集合で、`fk:` 行のみ 5 フィールド
（`fk:<cols>:<parent>:<pcols>:<match>:<deferral>`。`match` は `simple`／`full`、
`deferral` は `immediate`〔`NotDeferrable`〕／`deferrable`
〔`DeferrableInitiallyImmediate`〕／`deferred`〔`DeferrableInitiallyDeferred`〕）に
拡張する。全 FK が既定オプションのスキーマは（`FOREIGN KEY` の有無に関わらず）
引き続き v8 のバイト列のまま書く（正規形の一意性。v2〜v9 は互いに排他）。
**旧バイナリは v9 を未知の版として拒否する（前方互換は持たない）。**

- decode は構造（件数・フィールド数・識別子形状・重複・列数一致・`id` 単独・
  `MATCH`／遅延属性トークン）を共有パーサー（`parse_foreign_key_section`）で
  検証し、参照元列の実在・型・自己参照の照合を `validate_schema` で再検証する。
  全 FK が既定オプションの v9（正規形の一意性違反）も同じパーサーが拒否する。
  破損は `CorruptSchema`。
- `DROP TYPE` の依存判定用の軽量パーサー（`catalog_value_references_enum_type`）も
  v8／v9 を認識し、同じ共有パーサーで構造を検証する（decode より緩くならない）。
- 参照先側の逆引き（このテーブルを参照している宣言の列挙。
  `referencing_foreign_keys_in_txn`）はカタログ全走査だが、`FOREIGN KEY` を
  持つスキーマは必ず v8 か v9 のため、値の 1 行目が `v8` または `v9` の
  エントリだけを decode する。**v9 を見落とすと、v9 の子テーブルに対して
  参照先側の検査（`enforce_referencing_rows_in_txn` 経由の DELETE／TRUNCATE／
  キー UPDATE）と `DROP TABLE` の `2BP01` が効かなくなる fail-open 経路になる**
  （codex-review 指摘・回帰テスト
  `table17_foreign_key.rs::v9_catalog_children_are_covered_by_reverse_lookup_checks`）。

以前の試作では専用 redb テーブル `foreign_keys` に分離していたが、主キー・UNIQUE・
`CHECK` がいずれも `TableSchema` とカタログ版で表現されるようになったため、
それらと同じ機構（単一のスキーマ値・版による正規形）へ揃えた。

## 検査の単一実装（`constraint.rs`）

TABLE-16 と同じ単一検査点に置く（表層ごとに検査を持たない）。

- 参照元側: `constraint::enforce_row_constraints_in_txn` の末尾（`CHECK` → 一意性 →
  `FOREIGN KEY` の順）。書き込んだ各行を同一 write トランザクション内で読み戻し
  （UPSERT の `DO UPDATE`・`UPDATE` の SET 適用後の最終値。SET で触れない既存値も
  含む）、値の組が参照先に存在することを確かめる。`id` 参照は物理キーの点照会、
  列参照は永続キー索引（`key_index.rs`、Issue #1071）が登録済みなら索引照会、
  未登録なら参照先のテナント範囲を走査して判定した上で索引を構築する
  （下記「計算量」節参照）。
- 参照先側: `constraint::enforce_referencing_rows_in_txn`。削除・更新・`TRUNCATE`・
  置換の後に、このテーブルを参照先とする各宣言について、参照元の同一テナント
  全行の値の組が変更後の参照先にすべて存在することを確かめる（事後状態の検証）。
  削除前の値を保持する必要がなく、自己参照・複数行の同時削除・置換のいずれにも
  同一の実装で効く。`ALTER TABLE ... ADD FOREIGN KEY` を持たないため各文の開始時点で
  参照整合性は常に成立しており、この検証は `NO ACTION` と等価になる。
- 更新（`UPDATE`・UPSERT の `DO UPDATE`・全列置換）で主キー・UNIQUE 構成列に
  触れない場合は、参照先キーが変わり得ないためカタログの逆引きも行わない
  （`id` は予約列で `SET` できない）。
- 同一文・同一明示トランザクション内で先に書いた行（自己参照で同じ文が書いた行を
  含む）は、redb の write トランザクションが自身の未 commit の書き込みを読める
  ため母集合に含まれる（`BEGIN; INSERT 親; INSERT 子; COMMIT` が成立する）。
- エラーは `TenantWriteError::ForeignKeyViolation` 単一 variant（参照元側・参照先側の
  いずれの原因も区別しない固定文言）。
- `MATCH FULL`（D13）の判定は `constraint::push_required_key` に実装する。構成列の
  NULL 個数を数え、`0 < NULL 数 < 構成列数`（一部だけ NULL）なら即座に違反、
  全 NULL なら検査対象外、全非 NULL なら `MATCH SIMPLE` と同じ照合を行う。
- `INITIALLY DEFERRED` の FK（D14・D15）は `constraint::FkCheckMode`
  （`All`／`ImmediateOnly`）で文単位検査から除外できる。`ImmediateOnly` を
  渡してよいのは `tenant::WriteTarget::InTxn` を受理する 4 経路
  （`insert_row_unchecked`・`insert_rows_unchecked`・`insert_typed_row_unchecked`・
  `truncate_table_unchecked`。`WriteTarget::fk_check_mode()` が導出する）のみで、
  それ以外の呼び出し元（UPDATE／DELETE／UPSERT／複数行 INSERT 等、いずれも
  autocommit 専用）はすべて `FkCheckMode::All` を明示する。詳細は下記
  「COMMIT 時の遅延検査」参照。

### 計算量（Issue #1071 で索引化）

`crates/engine/src/key_index.rs` の永続キー索引により、参照元側（列参照の
存在確認）・参照先側（被参照確認）とも**テナントの保有行数に比例しない**
判定へ切り替えた（`id` 参照は索引導入前から物理キーの点照会で行数に比例
しない）。

- 索引の形: `(テーブル, 索引名)` で識別する 2 本の redb テーブル（順引き
  `(tenant, key, row_id) -> ()`・逆引き `(tenant, row_id) -> key`）。索引名は
  対象列集合から一意に決まり、参照先側（親の被参照列）・参照元側（子の FK
  列）が同じテーブル・同じ列集合を指す場合は 1 本に集約される（自己参照等）。
- 維持点: `constraint::enforce_row_constraints_in_txn`（書き込み直後）が
  登録済み索引を同期する単一箇所（一意性・`CHECK` 検査と同じ検査点）。
  `DELETE`／`TRUNCATE` は `enforce_referencing_rows_in_txn` が自ら同期・
  消去する。
- フォールバック: 索引が未登録（旧 DB・初回参照）の FK・テーブルの組み合わせ
  に限り、索引導入前と同一の全行走査で判定し、成功後に索引を構築・登録して
  以後の文から索引経路に切り替える。この構築（backfill）は該当テーブルの
  **全テナント**を 1 回だけ読むが、結果は応答へ一切影響せず、1 回限りの
  レイテンシだけが観測可能（テナント境界節参照）。
- 走査上限（`tenant::MAX_SCANNED_ROWS`）を継承しない理由は変わらない
  （フォールバック走査に限りテナントの保有行数に比例するため、上限を課すと
  索引未構築のテナントが書き込めなくなる fail-closed 過ぎる制約になる）。
- 対象外: `enforce_referencing_rows_in_txn` が呼ぶ
  `catalog::referencing_foreign_keys_in_txn`（このテーブルを参照する FK の
  逆引き）はカタログの**テーブル数**に比例する走査のままで、行数には比例
  しない（索引化は別課題。§対象外・後続候補参照）。

## テナント境界（RLS-9・RLS-10 (c)）

- 判定の母集合は同一テナントが所有する**全行**（`Public`／`Private` を問わない。
  RLS 可視集合ではない）。可視スナップショット由来の二次索引・世代整合キャッシュは
  流用せず、生の redb 走査で判定する。
- 走査・点照会のキーはサーバー側導出テナント（`ctx.tenant_id()`）の物理キー空間
  `(tenant, 0)..=(tenant, u64::MAX)` のみで組み立て、他テナントのキー空間に触れる
  分岐を持たない。他テナントだけが持つ参照先は「不在」と同じ結果になり、成否・
  `wire_code`・文言のいずれからも区別できない（`table17_foreign_key.rs` の
  `violation_response_does_not_reveal_other_tenant_parent_rows` で固定）。
- 他テナントの参照元行は参照先の削除・`TRUNCATE` を阻止しない
  （`other_tenant_referencing_rows_do_not_block_parent_changes`）。
- 単一行 `DELETE` で対象行が不在・他テナント所有（`0` 行）の場合は参照先側の検査
  自体を行わない（他テナントの行の有無で処理経路が分岐しない）。
- `DROP TABLE` の `2BP01` はカタログ情報のみで判定し、テナントデータを参照しない。

## COMMIT 時の遅延検査（Issue #1077）

明示トランザクション（SQL-31・TASK-221）中は `sql::transaction::ActiveTxn` に
`written_by_tenant: BTreeSet<(tenant_id, table)>` を持たせ、書き込みのたびに
`SessionTransaction::mark_written(tenant_id, table)`（`core.rs` の INSERT・
TRUNCATE の 2 呼び出し口）で記録する。`SessionTransaction::commit` は
`commit_boundary::commit` の**前**に、この記録集合の各要素について
`constraint::enforce_deferred_foreign_keys_in_txn(write_txn, tenant_id, table)` を
呼ぶ——`table` を子とする `INITIALLY DEFERRED` 宣言と、`table` を親とする他
テーブルの `INITIALLY DEFERRED` 宣言（`referencing_foreign_keys_in_txn` 経由。
v9 対応が前提）の両方について、COMMIT 直前の事後状態を全件検証する
（`constraint::enforce_referencing_rows_by_scan_for_fk` を
`enforce_referencing_rows_in_txn` の索引未登録時フォールバック経路と共有し、
ロジックを 2 か所で重複させない。Issue #1071 の索引化スコープ外——事後状態の
全件検証は差分ベースの索引同期に乗らないため全行走査のまま据え置く）。

- 検査対象範囲は `written_by_tenant` だけから導出する（呼び出し元が渡す ctx には
  依存しない）。記録漏れ＝検査漏れ＝fail-open になるため、`mark_written` は
  シグネチャに `tenant_id` を要求し、呼び出し漏れをコンパイル時に検出する。
- 違反時は `write_txn` を drop（abort）し、`SessionState` を `BEGIN` 時点へ復元して
  `Idle` へ戻り `23503` を返す（`session_at_begin` の復元は commit 自体の失敗と
  同じ分岐。COMMIT 後の `ReadyForQuery` は `'I'`）。持続時間上限超過（`54000`）の
  判定は遅延検査より前に行う（既存の順序を維持）。
- 計算量: 子テーブルの当該テナント全行走査（事後状態の全件検証のため索引化
  スコープ外。上記参照）に比例し、COMMIT 中はライタを保持し続ける（親側の
  存在確認は `verify_required_parent_keys` 経由のため索引登録済みなら索引
  照会で済む）。既存の検査と同じく走査上限は設けない（大きなテナントでは
  COMMIT が遅くなるトレードオフとして記録する）。同一トランザクションで複数文が
  同じ `(child, fk)` の組に触れても、`written_by_tenant` はテーブル単位の集合の
  ため検査は高々 1 回で済む——ただし親・子の双方が同一トランザクション内で
  書き込まれた場合、同じ `(child, fk)` の組がそれぞれのループ（子として・親の
  逆引きとして）から 2 回検証されることがある（結果に影響しない冗長な再検証で
  あり、fail-open ではない）。
- `SET CONSTRAINTS` を持たないため、遅延できるのは `INITIALLY DEFERRED` の宣言
  のみ。`DEFERRABLE`（`INITIALLY IMMEDIATE` 相当）は autocommit と同じく文単位で
  検査したままになる（D14）。

## 書き込み経路への結線

参照元側（`enforce_row_constraints_in_txn` 経由。既存の一意性・`CHECK` 検査と同じ
呼び出し点）: `insert_row_unchecked`・`insert_rows_unchecked`・
`insert_typed_row_unchecked`・`insert_typed_rows_unchecked`・
`upsert_typed_rows_unchecked`・`update_row_unchecked`・
`update_row_columns_unchecked`・`update_rows_where_unchecked`・
`replace_typed_rows_by_text_key`（新規チャンク行）。

参照先側（`enforce_referencing_rows_in_txn`）: `delete_row_impl`（単一行 DELETE・
`RETURNING` を包含。実際に削除した場合のみ）・`delete_rows_where_unchecked`・
`truncate_table_unchecked`（明示トランザクション内の `TRUNCATE` を含む）・
`update_row_unchecked`・`update_row_columns_unchecked`・
`update_rows_where_unchecked`・`upsert_typed_rows_unchecked`（`DO UPDATE`）・
`replace_typed_rows_by_text_key`（置換で消える旧行）。

ファイル形 `INSERT` の違反は `sql::exec::map_incremental_error` を経由するため、
行形の `map_write_error` と同じく `23503` への写像アームを持つ（`_` 節の `XX000` へ
丸めない）。

`catalog.rs` の生書き込み API（`#[cfg(test)]` 限定・production では到達不能）は
一意性検査と同じくこの検査点を経由しない（既知のギャップ）。

## エラー・公開 API（BREAKING CHANGE）

- 新設 `wire_code`: `23503`（`FOREIGN_KEY_VIOLATION`。HTTP 409）・`42830`
  （`INVALID_FOREIGN_KEY`。HTTP 400）。`2BP01`・`42809`・`42P01` は既存分類を再利用。
- `ErrorClass::ForeignKeyViolation`・`InvalidForeignKey`（32 → 34 分類）
- `CatalogError::InvalidForeignKey`・`TenantWriteError::ForeignKeyViolation`・
  `SqlSurfaceError::ForeignKeyViolation`／`InvalidForeignKey`
- `ValidatedCreateTable.foreign_keys`（公開フィールド追加）・
  `catalog::ForeignKeyDef`（公開型）・`TableSchema::foreign_keys()`
- SQL 表層 `CREATE TABLE` が `INTEGER`／`BIGINT` 列を受理するようになった

いずれの enum も `#[non_exhaustive]` ではない
（`docs/design/error-enum-non-exhaustive-policy.md`）ため、外部クレートの網羅
`match` を破壊しうる破壊的変更として扱う。

Issue #1077（`MATCH FULL`・`DEFERRABLE`）は上記とは異なり非破壊的変更として扱う:
`catalog::ForeignKeyMatch`・`catalog::ForeignKeyDeferrability` は新規かつ
`#[non_exhaustive]`（将来 `MATCH PARTIAL` 等の追加時に呼び出し元の網羅 `match` を
破壊しない設計）で、`ForeignKeyDef` へのフィールド追加は非公開フィールド＋
`with_options` ビルダー方式（既存の `with_checks` と同じ設計）のため
`ForeignKeyDef::new` の呼び出し元は無変更で動く。`TenantWriteError`・
`SqlSurfaceError`・`ErrorClass` への variant 追加もない（`23503`・`42601` の
既存分類を再利用する）。

## NoSQL（HTTP）表層

`op` 語彙に DDL が無いため `42830` は実要求から到達しない。宣言済みテーブルへの
`insert`／`update`／`delete` op は同じ単一検査点を通るため `23503`／409 は到達する
（`crates/wire-server/docs/nosql-api.md`・`crates/wire-server/tests/
err4_http_projection.rs` の `err4_f_foreign_key_violation_reachable_via_*`）。

## 対象外・後続候補

- `ALTER TABLE ... ADD/DROP CONSTRAINT FOREIGN KEY`（既存行の全テナント検証が必要。
  着手時は `key_index::ensure_index_in_txn`／`drop_indexes_for_table_in_txn` を
  流用できる）
- `ON DELETE CASCADE`／`SET NULL`／`SET DEFAULT`、制約名
  （`CONSTRAINT <name> FOREIGN KEY`）
- `SET CONSTRAINTS { ALL | name } { DEFERRED | IMMEDIATE }`（`DEFERRABLE
  INITIALLY IMMEDIATE` を実行時に遅延へ切り替える機能。Issue #1077 のスコープ外）
- `MATCH PARTIAL`（Issue #1077 のスコープ外。D13）
- `PRIMARY KEY`／`UNIQUE` 制約への `DEFERRABLE`（Issue #1077 のスコープ外）
- `catalog::referencing_foreign_keys_in_txn`（テーブル数比例のカタログ走査）の
  索引化（Issue #1071 の対象外。テーブル数は行数と異なり実運用上小さいため）
- COMMIT 時の遅延検査（`enforce_deferred_foreign_keys_in_txn`。Issue #1077）の
  索引化。事後状態の全件検証という性質上「失われたキー」の差分を持たず、
  索引の増分同期が前提とする差分ベースの判定に乗らないため、索引導入前と
  同じ全行走査のまま据え置く（Issue #1071 の対象外）
- `#[cfg(test)]` の生書き込み API（`catalog.rs`）が索引・制約を迂回する点
  （production では到達不能。Issue #1078）
- 明示トランザクション内の `UPDATE`／`DELETE`／`UPSERT`／複数行 `INSERT`
  （単一行 `INSERT`・`TRUNCATE` のみ対応。`docs/design/explicit-transaction.md`）。
  対応すれば `INITIALLY DEFERRED` の遅延検査の観測可能範囲が広がる
- NoSQL 表層の DDL op での宣言
- `UPDATE ... SET col = NULL`（SQL 表層に `NULL` リテラルの構文が無く、`MATCH FULL`
  を `UPDATE` 経由で NULL 混在にする経路は現状テスト不能）
