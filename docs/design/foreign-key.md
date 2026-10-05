# `FOREIGN KEY` 制約の設計判断

Issue #907・対象ビヘイビア: TABLE-17（TASK-205）・TABLE-20（参照アクション）・
TABLE-21（`MATCH`・遅延属性。いずれも TASK-232）。関連ポインタ: TABLE-12
（物理キー `(tenant_id, id)`）・TABLE-15（`DROP TABLE` の依存オブジェクト検査）・
TABLE-16（主キー・UNIQUE・単一検査点）・RLS-9・RLS-10 (c)（他テナントの存在情報の
非漏えい・可視性を問わない判定母集合）・ERR-6（新設 `wire_code` と HTTP 射影）・
SQL-31・TASK-221（明示トランザクション）。

Issue #1077（`MATCH FULL`・`DEFERRABLE`）で D5・D6・「対象外・後続候補」を改訂し、
D13〜D15・「COMMIT 時の遅延検査」節を追加した。対応ビヘイビア: TABLE-21（2026-09-28
新設・spec 側判断記録は `04-behavior/records/phase8-constraint-index-followups-2026-09-28.md`
ポインタ。本文は転記しない）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本
ドキュメントは本リポ側の実装判断・設計記録のみを扱う。

## 決定事項

| # | 決定 | 理由 |
| --- | ---- | ---- |
| D1 | 参照先列は、参照先テーブルの `id` 疑似列（物理キー）・宣言済み主キー・UNIQUE 制約のいずれかと**列集合**が一致すること。それ以外は `42830` | 一意性を保証しない列を参照先にすると、同値の参照先行の 1 行を削除しても残りが参照を満たし続ける等、`NO ACTION` の意味論が定まらない |
| D2 | 参照先列の省略（`REFERENCES <t>`）は参照先の主キー、未宣言なら `id` へ解決し、解決済みの列名をカタログへ永続化する | PostgreSQL と同じ規約。解決結果を永続化することで、後から参照先が変わっても宣言の意味が変わらない |
| D3 | 参照元列と参照先列の型は位置ごとに一致すること（型タグ＋パラメータ。ENUM は型名を含む）。`id` 参照の参照元列は `INTEGER`／`BIGINT`。不一致は `42830` | 参照先の照合を一意性検査と同じ型タグ付き正準キーで行うため、型が異なる組は常に違反になる（黙って常に失敗する宣言を受理しない） |
| D4 | SQL 表層 `CREATE TABLE` の列型へ `INTEGER`／`BIGINT` を追加（`NOT NULL`／`DEFAULT <数値>`／`UNIQUE`／`PRIMARY KEY` も受理） | `id` を参照する参照元列を SQL で宣言するための最小限の前提整備 |
| D5 | ~~参照動作は既定の `NO ACTION` のみ~~（Issue #1076 で改訂。D16〜参照）。`MATCH`（D13）・遅延属性（D14）は Issue #1077 で受理するようになった。表制約 `CONSTRAINT <name> FOREIGN KEY (...)` 前置は Issue #1069 で受理するようになった（列制約 `<col> ... CONSTRAINT <n> REFERENCES` は引き続き `42601`。詳細は [alter-table-foreign-key-constraint.md](./alter-table-foreign-key-constraint.md) F6 参照） | 対象外の動作を黙って既定動作へ丸めない（fail-closed） |
| D6 | `MATCH SIMPLE`（既定）は NULL を含む値の組を検査しない。`MATCH FULL`（D13）は全 NULL の組のみ検査しない | PostgreSQL の既定 |
| D7 | 検査は文単位・即時。台帳記録・行の書き込みの**後**、テーブル世代 bump・commit の**前**に同一 write トランザクション内で行う | 既存の制約検査（TABLE-16）と同じ位置。`operation_id` の再送判定（`23505`／`22023`）が本検査より優先される |
| D8 | 自己参照を受理する。~~循環参照は `CREATE TABLE` の時点で参照先が存在する必要があり、`ALTER TABLE ... ADD FOREIGN KEY` を持たないため、自己参照以外の循環は構造的に作れない~~（Issue #1069 で `ALTER TABLE ... ADD FOREIGN KEY` を追加したため、自己参照以外の循環〔A↔B〕も後から作成できるようになった。循環した両テーブルの `DROP TABLE` はどちらも `2BP01` になり、先に `DROP CONSTRAINT` が必要。詳細は [alter-table-foreign-key-constraint.md](./alter-table-foreign-key-constraint.md) F9 参照） | 自己参照は参照元＝参照先のスキーマで解決・検査でき、特別な経路を要さない |
| D9 | 参照先名がビュー・索引名なら `42809`、存在しなければ `42P01`。作成対象名の重複（`42P07`）はそれらより先に判定する | テーブル・ビュー・索引は名前空間を共有する（`CREATE TABLE` の既存判定と同じ順序） |
| D10 | 参照先テーブルの `DROP TABLE` は他テーブルから参照されていれば `2BP01`（データの有無を問わずカタログのみで判定）。自己参照は依存に数えない | TABLE-15 |
| D11 | 参照元列の `DROP COLUMN` は `DependentObjectsStillExist` で拒否。参照先側の列は主キー・UNIQUE 構成列（既存の検査で拒否済み）か `id`（予約列）に限られる | 制約を黙って消す暗黙 cascade を作らない（`DROP COLUMN` の話。UNIQUE 制約自体の明示 `DROP CONSTRAINT` は Issue #1067 で追加済みで、参照されている UNIQUE の DROP は `2BP01` で拒否する。詳細は [alter-table-unique-constraint.md](./alter-table-unique-constraint.md) D7 参照） |
| D12 | 宣言面は SQL 表層の `CREATE TABLE` に加え、Issue #1069 で `ALTER TABLE ... ADD [CONSTRAINT <name>] FOREIGN KEY` を追加した（既存行の全テナント検証を伴う。詳細は [alter-table-foreign-key-constraint.md](./alter-table-foreign-key-constraint.md) F7 参照）。`ALTER TABLE ... ADD COLUMN ... REFERENCES` は引き続き `42601`。Rust API の `TableSchema::with_foreign_keys` は `pub(crate)`。新設 `Storage::alter_table_add_foreign_key` も `pub(crate)`（宣言面は SQL 表層のみ） | 後付けの宣言は既存の全テナント行の検証を要するため、別途 Issue #1069 で設計した |
| D13 | `MATCH {SIMPLE\|FULL}`（既定 `SIMPLE`）を受理する。`MATCH FULL` は複合 FK で NULL と非 NULL が混在する組を違反にする（単一列は `SIMPLE` と同じ挙動）。`MATCH PARTIAL` は非対応のまま `42601` | PostgreSQL の 3 値のうち実装コストに見合う 2 値のみを対象にする |
| D14 | `[NOT] DEFERRABLE`／`INITIALLY {DEFERRED\|IMMEDIATE}`（任意順・各グループ高々 1 回）を受理する。`INITIALLY DEFERRED` の宣言だけが、明示トランザクション中の文単位検査を COMMIT まで遅延できる。`SET CONSTRAINTS`（`DEFERRABLE INITIALLY IMMEDIATE` を実行時に遅延へ切り替える機能）は非対応のため、`DEFERRABLE`（`INITIALLY IMMEDIATE` 相当）は文単位検査のまま変わらない | `SET CONSTRAINTS` 抜きでも `INITIALLY DEFERRED` だけで自己参照・相互参照する初期データ投入のユースケースをカバーできる |
| D15 | 遅延は「検査しない」ことを意味しない。autocommit（1 文＝1 トランザクション）は宣言に関わらず必ず文単位で検査する。省略できるのは明示トランザクション中の `INITIALLY DEFERRED` の文単位検査だけで、COMMIT 時にまとめて検査する（下記「COMMIT 時の遅延検査」節） | 「遅延」を「検査省略」と混同すると、autocommit や `SET CONSTRAINTS` 相当の切り替えが無い経路で fail-open になる |

### 参照アクション（Issue #1076・TASK-205 拡張・対応ビヘイビア TABLE-20）

TABLE-17 が定めていた「`ON DELETE CASCADE`／`SET NULL` は `42601`」の拒否契約を、
TABLE-20（2026-09-28 新設。spec 側判断記録は
`04-behavior/records/phase8-constraint-index-followups-2026-09-28.md` ポインタ。
本文は転記しない）の確定をもって受理形へ改訂する対応で実装した。

| # | 決定 | 理由 |
| --- | ---- | ---- |
| D16 | `ON DELETE`／`ON UPDATE` は `NO ACTION`・`RESTRICT`（`NoAction` へ正規化）・`CASCADE`・`SET NULL`・`SET DEFAULT` を受理する。列リスト形の `SET NULL (col, ...)`／`SET DEFAULT (col, ...)`・`CONSTRAINT <name>` は引き続き `42601` | PostgreSQL の基本形に揃えつつ、対応しない形は fail-closed に拒否する |
| D17 | 実行順序: (1) 参照アクションを再帰的にすべて適用する → (2) 元の文の対象テーブルと連鎖で変更した各テーブルについて、それを参照する全 FK（アクションを問わない）の事後状態検証（既存の `NO ACTION` 検証）を行う。`RESTRICT` は `NO ACTION` と区別して永続化せず、両方とも (2) の文末検証に統一する（PG は `RESTRICT` を即時検査するため、本実装は PG が拒否する一部の文を受理しうる既知の差分がある） | PG の `NO ACTION` が文末に検査される意味論に合わせつつ、アクション実装の不具合があっても最終状態の参照整合性を fail-closed な最終防御として保証する |
| D18 | `TRUNCATE` は参照アクションを発火させない（`ReferencedRowsChange::Truncated`。事後検証のみ行う） | PostgreSQL の `TRUNCATE` も `ON DELETE` アクションを発火させない（`TRUNCATE ... CASCADE` は別構文で未対応のまま） |
| D19 | 宣言時検査（`42830`）: (a) `SET NULL` で参照元列に `NOT NULL` の列がある、(b) `SET DEFAULT` で参照元列に「`DEFAULT` 無し・`NOT NULL`」の列がある、(c) `ON UPDATE CASCADE` で参照元列が `NOT NULL` なのに参照先列が nullable、のいずれも拒否する | `ALTER` で FK 列の nullability・`DEFAULT` を変える経路が無いため、宣言時検査が恒久的に有効であり続ける |
| D20 | 連鎖の深さ・1 文あたりの対象行数に実装既定の上限（`constraint::MAX_REFERENTIAL_ACTION_DEPTH`＝16・`MAX_REFERENTIAL_ACTION_ROWS`＝10,000。spec 由来ではない）を設け、超過は `TenantWriteError::ReferentialActionLimitExceeded`（`54000`）で副作用ゼロに拒否する | 永続索引を持たないテナント範囲走査の再帰であり、無制限だと DoS になり得る（coding-rust.md「不安全な設計」） |
| D21 | FK の重複判定は構造（`columns`・`parent_table`・`parent_columns`）のみで行い、アクション・`MATCH`・遅延属性の違いは無視する（`ForeignKeyDef::shares_reference_shape`） | 同じ列の組に矛盾するアクション・オプションを 2 つ宣言できる抜け穴を塞ぐ |
| D22 | カタログ v8 の `fk:` 行は、参照アクション・`MATCH`・遅延属性のすべてが既定値の場合は従来の 3 フィールド形のままバイト列を変えず、いずれか 1 つでも既定値以外の場合のみ 7 フィールド形（D13・D14 の `MATCH`・遅延属性を含む。「永続化」節参照）でカタログ v9 として永続化する | 既存 v8 ゴールデンテスト・カタログ後方互換を保ちつつ、アクション・`MATCH`・遅延属性を単一のフォーマット拡張として素直に表現できる |
| D23 | 同じ親の変更（`ON DELETE`／`ON UPDATE` 1 回分）に対する全 `FOREIGN KEY` の対象特定は、いずれの FK もまだアクションを適用していない子テーブルの状態から先にまとめて行う（Pass 1）。適用（Pass 2）は `referencing`（カタログ走査順＝概ね宣言順）ではなく、子テーブル名・参照元列・参照先テーブル・参照先列で決まる正準キーの昇順で行い、`CREATE TABLE` での FK 宣言順に一切依存しない。`CASCADE`（削除）は行そのものを消すため他アクションとどちらの順で交差しても最終状態は削除に収束するが（`SET NULL`／`SET DEFAULT` は削除済み行を素通りし、`CASCADE` は `id` で読み直すため既に書き換えられた行も削除できる）、同じ列に `SET NULL` と `SET DEFAULT` が競合する退化ケース（同じ子列を異なる `UNIQUE` 経由で参照する複数 FK）だけは適用順で最終値が変わり得るため、この正準キーで固定する——正準順で**後**に適用される FK（キーが辞書順で大きい方。`parent_columns` が異なる同一子テーブル・同一参照先列数の FK では概ね `parent_columns` の辞書順が支配的）の SET 結果が最終値として残る。キーの大小関係自体に PostgreSQL 由来の意味はない。PostgreSQL は参照整合性トリガーを行キューの順で逐次発火し、後続のトリガーは先行トリガーの効果を可視のまま参照するため、本実装（対象を確定してから適用する 2 パス方式）は PG よりこの種の交差に厳格という既知の差分がある | codex-review 指摘（PR #1138）: FK ごとに対象を収集し即座に適用すると、先に適用した FK の書き込みが後続 FK の対象特定に影響し、宣言順で最終結果が変わってしまう |

## 構文

```
CREATE TABLE <table> (
  <col> <type> [<列制約>]* [REFERENCES <parent> [(<pcol>[, <pcol>]*)]
                            [MATCH (SIMPLE|FULL)] [<参照動作>]* [<遅延属性>]*]
  | FOREIGN KEY (<col>[, <col>]*) REFERENCES <parent> [(<pcol>[, <pcol>]*)]
                                   [MATCH (SIMPLE|FULL)] [<参照動作>]* [<遅延属性>]*
  [, ...]
) [;]

<参照動作> ::= ON DELETE <アクション> | ON UPDATE <アクション>
<アクション> ::= NO ACTION | RESTRICT | CASCADE | SET NULL | SET DEFAULT
<遅延属性> ::= [NOT] DEFERRABLE | INITIALLY (DEFERRED | IMMEDIATE)
```

`SET NULL`／`SET DEFAULT` の列リスト形（`SET NULL (col, ...)`）は未実装のまま
`42601`（D16）。表制約 `CONSTRAINT <name> FOREIGN KEY (...)` 前置は Issue #1069
で受理するようになった（D5 参照）。

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
`fk:<col1,col2,...>:<parent_table>:<pcol1,pcol2,...>`（すべてのオプションが既定値
の場合）を追記する。`FOREIGN KEY` を持たないスキーマは従来どおり v2〜v7 のまま
バイト列を変えない（v2〜v8 は互いに排他な正規形）。

`ForeignKeyDef` に `on_delete`／`on_update`（`ReferentialAction`。Issue #1076）と
`match_type`（`ForeignKeyMatch`）・`deferrability`（`ForeignKeyDeferrability`。
Issue #1077）を追加した。**既定以外**の値（参照アクションが `NoAction` 以外、
または `Full`・`NotDeferrable` 以外）を 1 件でも持つスキーマはカタログ v9 で
永続化する。v9 は v8 の上位集合で、`fk:` 行のみ 7 フィールド
（`fk:<cols>:<parent>:<pcols>:<on_delete>:<on_update>:<match>:<deferral>`。
`on_delete`／`on_update` は `noaction`／`cascade`／`setnull`／`setdefault`、
`match` は `simple`／`full`、`deferral` は `immediate`〔`NotDeferrable`〕／
`deferrable`〔`DeferrableInitiallyImmediate`〕／`deferred`
〔`DeferrableInitiallyDeferred`〕）に拡張する。全 FK が既定オプションのスキーマは
（`FOREIGN KEY` の有無に関わらず）引き続き v8 のバイト列のまま書く（正規形の
一意性。v2〜v9 は互いに排他）。**旧バイナリは v9 を未知の版として拒否する
（前方互換は持たない）。**

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
  渡してよいのは `tenant::WriteTarget::InTxn` 経由の書き込み（`WriteTarget::
  fk_check_mode()` が導出する。Issue #1179 で INSERT・複数行 INSERT・UPSERT・
  UPDATE・DELETE・TRUNCATE の全書き込み関数が `WriteTarget` を受け取る形になった。
  ファイル形 INSERT の `replace_typed_rows_by_text_key` のみ autocommit 専用で
  `FkCheckMode::All` を明示する）だけで、`Autocommit` は常に `All`。詳細は下記
  「COMMIT 時の遅延検査」参照。

### 参照アクションの適用（Issue #1076）

`enforce_referencing_rows_in_txn` の内部で、事後検証（上記）の**前**に
`constraint::propagate_referential_actions` が連鎖を適用する（D17）。

- 対象特定（Pass 1。D23）: このテーブルを参照する全 FK について、いずれの FK も
  まだアクションを適用していない子テーブルの状態から対象を確定する。`ON DELETE`
  は事後状態（削除済みの参照先）に存在しなくなった参照元キーを持つ子行、
  `ON UPDATE` は呼び出し元が書き込み前に捕捉した `constraint::UpdatedKeyPreImages`
  （更新前の全列値。`record` は同じ id への 2 回目以降の呼び出しを無視し最初の
  値を保持する契約——同じ id を同一文内で複数回記録する呼び出し元は現状無い）と
  現在の参照先行を突き合わせて「旧キー→新キー」が変わった子行を特定する。`id`
  参照は `ON UPDATE` で発火しない（`id` 疑似列は不変）。子テーブルの走査
  （`constraint::scan_child_fk_rows_for_keys`）は、削除・更新で失われた旧キーの
  集合（呼び出し元が先に確定する）に一致する行だけを保持し、一致件数が
  `MAX_REFERENTIAL_ACTION_ROWS` の残り枠を超えた時点で走査を打ち切って `54000`
  を返す（無関係な子行を保持せず、上限判定も収集完了を待たない）。
- 適用（Pass 2。D23）: 対象特定を終えた全 FK を、子テーブル名・参照元列・参照先
  テーブル・参照先列で決まる正準キーの昇順に並べ替えてから適用する（宣言順は
  一切参照しない）。`CASCADE`（`ON DELETE` は子行を削除、`ON UPDATE` は子の FK 列を
  新キー値へ書き換え）・`SET NULL`（FK 列を `NULL` に）・`SET DEFAULT`（FK 列を
  それぞれの列 `DEFAULT`、無ければ `NULL` に）。孫段への連鎖対象特定に使う
  pre-image は必ず Pass 1 の時点のスナップショット（`snapshot_child_rows_
  before_pass2`）から作り、Pass 2 で正準順が先の別 FK が既に書き換えた中間状態は
  使わない（Cursor Bugbot 指摘・PR #1138: 同じ子行を複数の FK が対象にし
  `SET NULL`／`SET DEFAULT` が `CASCADE` より先に適用される場合、中間状態を
  pre-image にすると孫段が本来の旧キーを見失い `23503` になる）。書き込み自体
  （read-merge-write のマージ元）は引き続き「現在の行」を使い、他 FK が既に
  適用した書き換えを正しく引き継ぐ。同じ子テーブルへ複数 FK が action を
  適用していても、そのテーブルの全 FK 適用が終わってから
  `enforce_row_constraints_in_txn`（`CHECK` → UNIQUE → 子自身の FK 参照元側）で
  1 回だけ再検証し、テーブル世代も bump する（TABLE-16 の単一検査点を再利用）。
- 再帰: 子テーブル自身がさらに親であれば、Pass 1・Pass 2 とも完了した後に同じ
  経路で孫段へ連鎖する（`depth` を 1 段ずつ進め、上限は D20）。自己参照では親役・
  子役で同じ行ストアハンドルを同時に持たないよう、走査（読み取り専用ハンドル）→
  適用（書き込みハンドル）→ 次段の再帰、の順に厳密に分離する（redb の
  `TableAlreadyOpen` 回避）。
- `TRUNCATE` は連鎖を起こさない（D18）。

### 計算量（Issue #1071 で索引化）

`crates/engine/src/key_index.rs` の永続キー索引により、参照元側（列参照の
存在確認）・参照先側（被参照確認）とも**テナントの保有行数に比例しない**
判定へ切り替えた（`id` 参照は索引導入前から物理キーの点照会で行数に比例
しない）。

- 索引の形: `(テーブル, 索引名)` で識別する 2 本の redb テーブル（順引き
  `(tenant, key, row_id) -> ()`・逆引き `(tenant, row_id) -> key`。テナントを
  跨いで共有）。索引名は対象列集合から一意に決まり、参照先側（親の被参照列）・
  参照元側（子の FK 列）が同じテーブル・同じ列集合を指す場合は 1 本に集約
  される（自己参照等）。登録簿（`key_index_registry`）は**テナント単位**
  `(table, name, tenant) -> ()`（Issue #1071 レビュー指摘 P0: テーブル単位の
  登録だと 1 テナントの backfill が他の全テナントの索引済み状態まで確定させ
  てしまい fail-open になり得るため）。
- 維持点: `constraint::enforce_row_constraints_in_txn`（書き込み直後）が
  登録済み索引を同期する単一箇所（一意性・`CHECK` 検査と同じ検査点）。
  `DELETE`／`TRUNCATE` は `enforce_referencing_rows_in_txn` が自ら同期・
  消去する。`ON DELETE CASCADE`（Issue #1076）による子行の物理削除は
  `constraint::apply_referential_action` が直接行うため、削除した id を
  同関数から `key_index::sync_rows_in_txn` へ渡して同期する（この同期を
  怠ると、削除済み行の索引エントリが残留し、孫段の列参照 FK による
  `NO ACTION` 事後検証が索引照会で「参照先はまだ存在する」と誤判定して
  違反を見逃す）。
- フォールバック: 索引がそのテナントで未登録（旧 DB・初回参照）の FK・
  テーブルの組み合わせに限り、索引導入前と同一の全行走査で判定し、成功後に
  索引を構築・登録して以後の文から索引経路に切り替える。この構築
  （backfill）は**要求元テナントの行だけ**を 1 回読み、他テナントの行数・
  破損状態には一切触れない（テナント境界節参照）。
- 走査上限（`tenant::MAX_SCANNED_ROWS`）を継承しない理由は変わらない
  （フォールバック走査に限りテナントの保有行数に比例するため、上限を課すと
  索引未構築のテナントが書き込めなくなる fail-closed 過ぎる制約になる）。
- 破損検知: 登録簿にエントリがあるのに順引きテーブルが実在しない状態
  （`ensure_index_in_txn` が両者を同一 write_txn で作成する不変条件が破れた
  破損 DB）は、`redb::WriteTransaction::open_table` が get-or-create で
  `TableDoesNotExist` を返さないため `list_tables` によるテーブル名の明示
  確認で検出し、`CorruptSchema` として fail-closed に拒否する（Issue #1071
  レビュー指摘 P0。黙って空テーブル扱いすると「参照なし」の誤判定で
  `DELETE`／`TRUNCATE` を許してしまう）。
- 同一 write_txn 内で「このテーブルの既存行を削除してから新規行を挿入する」
  呼び出し元（`tenant::replace_typed_rows_by_text_key` のファイル形置換等）は、
  この削除より前に `constraint::prepare_referenced_key_indexes_in_txn` で
  参照先列の索引を backfill しておく契約（cursor bugbot 指摘: 自己参照 FK で
  この事前 backfill を省くと、索引の初回構築が削除後の状態から行われ、
  削除された旧行の旧キーが逆引き索引の pre-image に一度も現れず、参照先側
  検査の `lost` 差分に載らないまま検査をすり抜ける。同関数ドキュメント参照）。
- 対象外: `enforce_referencing_rows_in_txn` が呼ぶ
  `catalog::referencing_foreign_keys_in_txn`（このテーブルを参照する FK の
  逆引き）はカタログの**テーブル数**に比例する走査のままで、行数には比例
  しない（索引化は別課題。§対象外・後続候補参照）。
- 参照アクションの連鎖（Issue #1076）は索引化のスコープ外: `propagate_
  referential_actions`（対象特定〔Pass 1〕の `scan_child_fk_rows_for_keys`）は
  索引を使わず、引き続き参照元の同一テナント全行数に比例する時間がかかる
  走査で対象を特定する（保持するメモリは一致した行数〔高々
  `MAX_REFERENTIAL_ACTION_ROWS`〕に有界化されている。無関係な行は一致判定の
  直後に捨てる）。同様に、連鎖で変更した各テーブルの事後検証
  （`verify_no_action_backstop_by_scan`）も、テーブルごとの索引差分を追跡
  していないため索引導入前と同じ全行走査で行う（索引化は元の直接変更テーブル
  の検査に限る。連鎖テーブルの索引化は後続課題）。

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

- 検査対象範囲は「`written_by_tenant` に現れるテナント × dirty テーブル」の直積
  （呼び出し元が渡す ctx には依存しない。テナント集合は上位集合＝fail-closed）。
  dirty テーブルは `mark_written` で記録した集合と、テーブル世代が確定済みの世代から
  変化したテーブル（`SessionTransaction::dirty_tables`）の和にする（Issue #1179）。
  参照アクション（`CASCADE`・`SET NULL`・`SET DEFAULT`）の連鎖で書き換わった子
  テーブルは文が直接対象にしたテーブルではなく `mark_written` されないが、その子を
  親とする別の `INITIALLY DEFERRED` FK は文単位検査から外れているため、記録だけに
  頼ると COMMIT で違反を見逃す（fail-open）。世代は行を書き換える全経路が bump する
  契約で、連鎖先も bump するため、記録漏れに依存せず検査対象に入る。`mark_written` は
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
一意性検査と同じくこの検査点を経由しない。Issue #1078 で、これら 3 API 自身が
`FOREIGN KEY` を含む制約付きテーブルへの書き込みを fail-closed に拒否するガードを
追加したため、検査点を経由しなくても違反状態は作れない。参照先（親テーブル）側は、
PK／UNIQUE で参照される場合は親テーブル自体が制約付きとしてガードに拒否され、
`id` 疑似列（D1）で参照される制約なしの親でも、生 API は `(tenant_id, id)` キーへの
追加・置換のみで既存の `id` を削除しないため参照元が孤児化することはない。

## エラー・公開 API（BREAKING CHANGE）

- 新設 `wire_code`: `23503`（`FOREIGN_KEY_VIOLATION`。HTTP 409）・`42830`
  （`INVALID_FOREIGN_KEY`。HTTP 400）。`2BP01`・`42809`・`42P01` は既存分類を再利用。
- `ErrorClass::ForeignKeyViolation`・`InvalidForeignKey`（32 → 34 分類）
- `CatalogError::InvalidForeignKey`・`TenantWriteError::ForeignKeyViolation`・
  `SqlSurfaceError::ForeignKeyViolation`／`InvalidForeignKey`
- `ValidatedCreateTable.foreign_keys`（公開フィールド追加）・
  `catalog::ForeignKeyDef`（公開型）・`TableSchema::foreign_keys()`
- SQL 表層 `CREATE TABLE` が `INTEGER`／`BIGINT` 列を受理するようになった
- **Issue #1076 の破壊的変更**: `TenantWriteError::ReferentialActionLimitExceeded`
  （新 variant。`54000`）の追加、`catalog::ReferentialAction`（公開型）・
  `ForeignKeyDef::on_delete()`／`on_update()`（公開メソッド）の追加、`ON DELETE`／
  `ON UPDATE` に `CASCADE`／`SET NULL`／`SET DEFAULT` を宣言できるようになったこと
  （従来 `42601` だった宣言が受理される）、カタログ v9 `fk:` 行の 7 フィールド形は
  旧バイナリでは読めない（decode 時に `CorruptSchema` として拒否される）

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

NoSQL `create_table.constraints[kind=foreign_key].references.on_delete`／
`on_update`（Issue #1148）は、SQL 表層と同じ固定語彙
（`no_action`／`restrict`／`cascade`／`set_null`／`set_default`）から
`ON DELETE`／`ON UPDATE` の固定トークン列へ写像し、同一の実行器
（`validate_create_table_tokens`・`EngineCore::execute_parsed_in_session`）へ
渡す（`crates/wire-server/src/http/query/ddl.rs::referential_action_tokens`）。
そのため宣言時検査の `42830`・連鎖適用後の参照違反 `23503`／409・連鎖上限
超過の `54000`／413 のいずれも NoSQL 表層の実要求から到達する
（`crates/wire-server/docs/nosql-api.md`・`crates/wire-server/tests/
nosql13_ddl.rs`・`crates/wire-server/tests/err4_http_projection.rs` の
`err4_f_foreign_key_violation_reachable_via_*`・
`err4_f_invalid_foreign_key_reachable_via_nosql_create_table`・
`err4_f_referential_action_limit_reachable_via_nosql_delete`）。SQL・NoSQL
いずれの宣言面でも、連鎖適用は engine 側の単一検査点（`constraint` モジュール）
だけが担う（第 2 の実行経路を作らない）。

## 列型が食い違う FOREIGN KEY（Issue #1402・TABLE-19・TABLE-17）

`ALTER COLUMN TYPE` の `INTEGER → BIGINT` が参照元・参照先の片側だけに適用されると、列型が
食い違う FK が永続スキーマ上に生じる。正準キーは型タグ付きのため、境界で読み替えないと
参照先側の検査が素通りする（fail-open）か参照元側で誤検知する。

- **型照合の規則**: `CREATE TABLE`・`ADD FOREIGN KEY` の宣言は従来どおり型の完全一致
  （食い違いは `42830`）。永続スキーマの再検証（decode・`encode_schema`）に限り、
  `INTEGER`／`BIGINT` の食い違いを許す（自己参照 FK の PK 拡大を表現するため）。
- **読み替え**: `constraint::recode_key_for_types` が正準キーを値を保って相手側の型へ
  再エンコードする。参照元の型で作った必須キー（`verify_required_parent_keys`）、参照先の
  失われたキー（`enforce_referencing_rows_in_txn`）、参照アクションの対象特定
  （`collect_action_targets`）の 3 境界で適用し、型が全位置で等しければ恒等（挙動不変）。
  相手側の列に収まらない値は存在し得ないため、参照元側は違反（`23503`）、参照先側は
  対象なしとして扱う。長さ・成分数・タグの不整合は内部矛盾として fail-closed。
- **ON UPDATE CASCADE**: 新しい親キー値を子列の型へ合わせる。子列に収まらない場合は
  副作用なしで `23503` により拒否する（専用の `22003` variant は公開 enum の破壊的変更に
  なるためスコープ外）。
- `id` 疑似列を参照する FK は子列の型で処理済みのため変更なし。

## 対象外・後続候補

- ~~`ALTER TABLE ... ADD/DROP CONSTRAINT FOREIGN KEY`・制約名
  （`CONSTRAINT <name> FOREIGN KEY`）~~（Issue #1069 で実装済み。詳細は
  [alter-table-foreign-key-constraint.md](./alter-table-foreign-key-constraint.md)
  参照）
- `SET NULL (col, ...)`／`SET DEFAULT (col, ...)`（列リスト形。Issue #1076）
- `TRUNCATE ... CASCADE`（`TRUNCATE` 自体は参照アクションを発火させない。D18）
- `RESTRICT` を `NO ACTION` と区別して永続化し即時検査すること（現状は両方とも
  文末の事後検証に統一。既知の差分として D17 の理由欄に記録）
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
- NoSQL `references` への `MATCH {SIMPLE|FULL}`・`[NOT] DEFERRABLE`・
  `INITIALLY {DEFERRED|IMMEDIATE}` の露出（Issue #1148 のスコープ外。
  `on_delete`／`on_update` の宣言自体は解消済み）
- `UPDATE ... SET col = NULL`（SQL 表層に `NULL` リテラルの構文が無く、`MATCH FULL`
  を `UPDATE` 経由で NULL 混在にする経路は現状テスト不能）
