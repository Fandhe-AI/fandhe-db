# ALTER TABLE DROP COLUMN／ALTER COLUMN TYPE（engine 層）

- **Issue**: #901（対象ビヘイビア: `docs/spec/04-behavior/data-model.md`
  TABLE-19・TABLE-5・TABLE-7。タスク: `docs/spec/05-tasks.md` TASK-203。
  ポインタのみ・本文非転記）
- **ステータス**: engine 層（Rust API）実装済み。SQL 表層への構文結線は
  Issue #1167 で実施（「SQL 表層結線（Issue #1167）」節参照）

## 背景・目的

既存の DDL（`Storage::create_table`／`alter_table_add_column`／`drop_table`）には
列の削除・型変更の手段が無かった。本 Issue はテーブル定義済みの列を削除する
`ALTER TABLE ... DROP COLUMN`、および列の型を広げる `ALTER COLUMN ... TYPE` を
engine 層（`crates/engine/src/catalog.rs`・`row_codec.rs`）の Rust API として
実装する。

スカラー列の行ペイロード（`row_codec::encode_scalar_columns` の出力 =
`storage::RowInput::metadata`）は `TableSchema::columns` の宣言順に並ぶ**位置
依存**の固定形式であり、`VECTOR` 列はこのペイロードに一切現れない（embedding は
`storage.rs` 側の別スロットが担う）。列を単純に配列から取り除くと、後続の生存列
の物理位置がずれて既存行を誤読する。型を書き換えて幅が変わる場合も同様に既存
行を誤読する。この位置依存性をどう扱うかが本 ADR の核心である。

## D1: DROP COLUMN = 物理スロットの墓標（tombstone）、カタログのみの O(1) 変更

`TableSchema` は論理列と物理配置を分離して持つ。

- `columns: Vec<ColumnDef>`（既存の public フィールド、意味は不変）は**論理列
  （生存列のみ）**を宣言順で持つ。`SELECT *`・投影・`WHERE` 解決・
  `RowDescription` など、行の物理配置を意識する必要のないほぼ全ての呼び出し元
  はこのフィールドだけを見ればよく、既存の呼び出し箇所（数百か所）は無変更の
  まま動く。
- 削除済み列は新設の非公開フィールド `dropped: Vec<DroppedSlot>` に持つ
  （`DroppedSlot { physical_index: u16, name: String, ty: ColumnType }`）。
  アクセサ `TableSchema::dropped_slots()`・`physical_slot_count()`・
  `physical_slots()`（生存列と墓標を物理位置の昇順でマージするイテレータ、
  `PhysicalSlot::Live(logical_index, &ColumnDef)` / `Dropped(&DroppedSlot)`）を
  `pub(crate)` で提供する。
- 物理配置は「生存列（論理順）と墓標を `physical_index` でマージした列」。
  `VECTOR` 列は物理バイトを消費しないため、物理配置の対象にするのは非
  `VECTOR` の生存列と墓標のみ（`row_codec.rs` の走査で自然にそうなる）。
  ADD COLUMN（末尾追記）は「物理末尾への追加」に一致し、既存の追加経路と
  整合する。
- 列数上限 `MAX_COLUMN_COUNT`（256）は**物理スロット総数**
  （`columns.len() + dropped.len()`）に適用する（墓標も物理容量を消費する）。
- 墓標の型は**フレーム等価型へ正規化**して保存する: `ENUM`／`JSON`／`JSONB` は
  行バイト列上 `TEXT` と同一フレーム（presence(1) + u32 長 + 本体）のため
  `TEXT` へ正規化する。これにより、削除済み列が `DROP TYPE`（依存列検査）を
  永久にブロックし続ける結合を断つ。それ以外の型（`INTEGER`／`BIGINT`／
  `REAL`／`DOUBLE`／`BOOLEAN`／`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／
  `BYTEA`／`ARRAY`）は元の型のまま保持する（物理フレーム幅を保つ必要が
  あるため）。`VECTOR` 列は削除不可のため墓標には現れない。
- 墓標スロットの codec 規則:
  - **デコード**（`row_codec::scan_scalar_columns_validated`。生存列・墓標の
    双方が共有する単一の物理走査）: 墓標は元の nullable 宣言に関わらず常に
    nullable 扱いとする（`PRESENCE_VALUE` + 値〔削除前の既存行〕・
    `PRESENCE_NULL`〔削除後に書かれた行〕・末尾打ち切りのいずれも受理する）。
    構造検証（presence タグ・宣言長上限・バッファ境界・UTF-8 等）は生存列と
    同じ経路で一切弱めず行い、値は出力（sink）へは渡さない（論理インデックス
    を持たないため）。
  - **エンコード**（`encode_scalar_columns`／`merge_encode_scalar_columns`）:
    墓標位置には常に `PRESENCE_NULL`（1 バイト）を書く。UPDATE の
    read-merge-write（`merge_encode_scalar_columns`）でも、削除前の既存値
    （`existing`）・SET 対象（`overrides`）のいずれに関わらず常に NULL を
    書く（削除済みの値を引き継がない）。
- 同名列の再追加: 墓標は列名重複検査（`validate_schema`）の対象外。再追加
  された列は新しい物理スロット（末尾）を得るため、既存行では旧値が復活せず
  （新スロットは既存行にとって「打ち切り」＝ NULL）、新規行にのみ値が書かれる。
- 保護列: `id`・`tenant_id`・`visibility`（予約名。テーブル定義上の列として
  存在するかに関わらず名前で判定する）・`VECTOR` 列は削除不可
  （`CatalogError::ProtectedColumn`。判定は列の存在確認より先に行う）。
  削除後に生存列が 0 本になる場合は `validate_schema` の既存契約
  （「列 1 本以上」）により `CatalogError::Invalid` で拒否する。

### カタログ形式 v3（墓標がある場合のみ）

墓標を持たないスキーマは常に v2 で書く（バイト列不変。既存のゴールデン
テスト・v2 の全既存 decode テストは無変更のまま green）。墓標が 1 つでも
あるスキーマだけを v3 で書く。1 行目 `v3`、2 行目 `cols:<物理スロット総数>`、
以降は物理順に 1 行 1 スロットで `name:tag:param:nullable:state`（`state` は
`L`=生存 / `D`=削除済み）。デコードは v2・v3 の両方を受理し、v1 は既存どおり
fail-closed に拒否する。v3 の追加検証:

- 5 フィールド固定・`state` は `L`/`D` のみ
- `D` スロットの型は `VECTOR`／`ENUM`／`JSON`／`JSONB` を禁止
  （フレーム等価型のみ）
- 生存列 ≥ 1、物理スロット総数 ≤ 256
- 生存列の名前重複を禁止（墓標は対象外）
- 生存 `VECTOR` 列 ≤ 1
- v3 なのに墓標 0 件なら拒否（形式の一意性）
- 墓標の `physical_index` は昇順・一意・範囲内であること

`ENUM` 型依存検査の軽量パーサー（`catalog_value_references_enum_type`。
`decode_schema_body` を経由しない独立実装）も v3 の 5 フィールド行を受理する
よう拡張し、生存・削除済みいずれの行も保守的に依存判定の対象に含める
（decode の完全な検証を経由しない軽量パーサーであるため、fail-closed に倒し
依存を見落とさない設計とした）。

## D2: ALTER COLUMN TYPE は拡大変換のみ受理（NUMERIC 精度拡大は Issue #901、INTEGER／REAL の拡大は Issue #1361）

D3（当初計画）では `INTEGER → BIGINT`・`REAL → DOUBLE PRECISION`・
`NUMERIC(p,s) → NUMERIC(p',s)` の 3 種類の拡大変換を受理する設計とした。この
うち `NUMERIC` の精度拡大は行の物理フレーム幅（presence(1) + unscaled
i128(16) = 17 バイト固定）に一切影響しないため、カタログのみを書き換える
O(1) 操作として実装した
（`Storage::alter_table_widen_numeric_precision(table, column, new_precision)`。
`scale` は変更しない）。

`INTEGER → BIGINT`・`REAL → DOUBLE PRECISION` は物理フレーム幅が変わる
（5→9 バイト）ため、単一 write トランザクション内で全テナントの既存行を
読み直し・変換後の型で再エンコードして書き戻す原子的な一括書き換えが必要になる
（redb の `range()` 走査と `insert()` を同一トランザクション内で両立させる
ためのバッチ分割、TABLE-12 のキー/ヘッダ tenant 整合検査の維持、ペイロード
上限超過時の全体中止など、DROP COLUMN 単独より大きい実装・検証面を持つ）。
本 Issue の実装時間内では、この行書き換えを伴う変換は実装を見送り、後続
Issue へ切り出した（Issue #1361 で実装。「行書き換えを伴う拡大変換」節）。

`alter_table_widen_numeric_precision` が受理するのはこの 1 パターン
（`NUMERIC` → より大きい `precision`・同一 `scale`）のみで、それ以外
（縮小変換・異種型変換・同一型への変更を含む）はすべて
`Err(CatalogError::IncompatibleTypeChange { column, from, to })` で拒否する。
判定基準（同一型への変更を拡大変換と見なさない、等）は本リポの実装既定値
である。

## D3: エラー契約

`CatalogError` に 3 variant を追加した（`#[non_exhaustive]` は使わず、既存の
網羅 `match`〔`Display`・`source`・`table_lookup_error`〕を全て更新し、新型が
既存分岐へ黙って流れる fail-open を構造的に防ぐ既存方針を維持）。

- `ColumnNotFound(String)`: 対象列が存在しない
- `ProtectedColumn(String)`: 予約列（`id`/`tenant_id`/`visibility`）または
  `VECTOR` 列の削除・型変更を試みた
- `IncompatibleTypeChange { column, from, to }`: 受理する拡大変換の一覧に
  含まれない型変更

いずれも Rust API 専用のエラーであり、SQL 表層への構文結線を行わないため
（「SQL 表層結線」節）、`error_format.rs::ErrorClass`・
`wire-server/src/error_response.rs`・`err4_http_projection.rs` への追加は
本 Issue のスコープ外のまま据え置く。エラーメッセージには列名・型タグ名
のみを含め、行の値・他テナントの情報は含めない。

## D4: 失効契約

DROP・ALTER TYPE のいずれもテーブル単位世代カウンタ
（`bump_table_generation_in_txn`）を進行させる。これにより
`SqlArenaCache`・`SparseIndexCache`・`HnswIndexCache`・`VisibleBitmapCache`・
`ScalarIndexCache`・`PrefilterCache` 系が既存の ADD COLUMN／DROP TABLE と
同じ仕組みで失効する（drop 専用の新たな失効機構は追加しない）。

## D5: 公開 API 互換性（破壊的変更）

墓標スロット（`dropped: Vec<DroppedSlot>`）を `TableSchema` の非公開
フィールドとして追加したため、従来 `pub name`／`pub columns` のみで
構成されていた `TableSchema` は外部クレートから
`TableSchema { name, columns }` という構造体リテラルで構築できなくなった
（AGENTS.md「公開 API・エラー契約の互換性（P1）」）。**移行方法**:
`TableSchema::new(name, columns)` を使う（本リポ内の呼び出し元は移行済み）。

この変更で `TableSchema` を内部に持つ
`crate::sql::copy::CopyInSession`（`CopyPlan::From` が包む公開型）自体の
サイズも増え、`CopyPlan`（`To` 分岐との enum サイズ差）が
`clippy::large_enum_variant` に抵触した。対応として `CopyPlan::From` の
内包型を `Box<CopyInSession>` へ変更する案を一度採ったが、これは
`CopyPlan::From` 自体の公開 variant 型を変える別の破壊的変更になるため
撤回し、代わりに `CopyInSession` 内部の非公開フィールド `schema` を
`Box<TableSchema>` 化してサイズを抑える設計へ変更した。`CopyPlan::From` の
内包型は `CopyInSession`（Box なし）のまま不変であり、この変更による
外部クレートへの破壊的変更はない。

`CatalogError` へ追加した `ColumnNotFound`／`ProtectedColumn`／
`IncompatibleTypeChange` の 3 variant も、`CatalogError` が
`#[non_exhaustive]` でないため外部クレートの網羅 `match` を破壊する。

これらはいずれも TABLE-19・TASK-203 が定める DROP COLUMN の物理挙動
（削除列を物理位置の墓標として残す）を実装する上で本質的に必要な
データ構造の変更であり、spec のビヘイビア契約自体（テナント境界・
カタログの読み書き挙動）に影響する差分ではない。spec は挙動契約のみを
定め、Rust 型のフィールド可視性・enum 内部表現までは規定しないため、
対応する spec 側定義変更はない（PR 本文・コミット `BREAKING CHANGE:`
に同旨を明記）。

## SQL 表層結線

Issue #901 着手時点（origin/main `be1e760`）で、SQL 表層の DDL 許可リスト・
実行権限ゲート（TASK-202・#899／#900）は未取り込みだった。DDL は全テナント
共有のカタログを変更する操作であり、権限ゲートなしで SQL 経由に露出すると
任意の認証済みセッションがテーブル定義を変更できる fail-open になる
（P0）。そのため本 Issue では **SQL 表層（`sql/allowlist.rs`・`ParsedSql`
variant 追加）・NoSQL 表層（`op` 許可リスト追加）のいずれへの結線も行わない**。
`Storage::alter_table_drop_column`／`alter_table_widen_numeric_precision`
は Rust API（engine クレートの公開関数）としてのみ存在する。

SQL 表層への結線は、TASK-202 の DDL 許可リスト・権限ゲートが取り込まれた
後続の別 Issue で行う。

## SQL 表層結線（Issue #1167）

上記「SQL 表層結線」節で後続扱いだった結線を実施した（ポインタ: TABLE-19・
SQL-23・ERR-6・NOSQL-13）。

- **構文（許可リスト）**: `ALTER TABLE <t> DROP COLUMN <c>`・
  `ALTER TABLE <t> ALTER COLUMN <c> TYPE <型名>` のみ。`COLUMN` 省略・
  `IF EXISTS`・`ONLY`・`CASCADE`／`RESTRICT`・`SET DATA TYPE`・`USING`・
  `COLLATE`・複数アクション・`RETURNING`・`USING OPERATION_ID` はいずれも `42601`。
  予約列名（`id`／`tenant_id`／`visibility`／`check`／`constraint`。大文字小文字
  無視）は構造検証段で `42601`（カタログ非照会のため存在オラクルにならない）。
- **判定順序**: 構文検証 → DDL 権限ゲート（`42501`）→ テーブル存在確認
  （`42P01`／`42809`）→（ALTER TYPE のみ）目標型解決 → engine の単一 write txn。
- **エラー写像**: 列なし `42703`／VECTOR 列・保護列・最後の 1 列・NUMERIC 範囲不正・
  未登録 ENUM `42601`／PK・UNIQUE・FK 参照列（DROP COLUMN のみ）・CHECK 参照列 `2BP01`／非互換な型変更
  `42804`／ロック待機超過 `55P03`。新しい `wire_code`・`ErrorClass` は追加しない。
- **ALTER TYPE の engine 側**: `Storage::alter_table_alter_column_type`（crate 内部）が
  目標の precision/scale を受け取り、単一 write txn 内で判定する（表層で事前に読んだ
  scale との TOCTOU を作らない）。受理は NUMERIC の precision 拡大（同一 scale）のみ。
  既存 `alter_table_widen_numeric_precision` は同じ共通本体へ委譲し挙動不変。
- **NoSQL**: `alter_table.drop_column` を同じ入口へ結線（`0A000` の仮実装と
  `DROP_COLUMN_UNAVAILABLE_MESSAGE` を削除）。ALTER COLUMN TYPE 相当の op 語彙は
  別論点のためスコープ外。
- **既知の差分（記録のみ）**: `INTEGER → BIGINT`・`REAL → DOUBLE PRECISION` は
  Issue #1361 で受理（下記）。同一型への変更は `42804`。ビューが参照する列の DROP は
  DDL 時点で `2BP01` で拒否する（同一 write txn 内で判定）。直接参照するビューに加え、
  連鎖したビュー（`v1 AS SELECT * FROM t`・`v2 AS SELECT c FROM v1`）は各ビューの
  公開列集合を削除後の状態で基底側から導出して検査し、外側ビューが参照する列が
  消える DROP も拒否する。`SELECT *` は公開列集合の導出に使い、それ自体は依存とみなさない。

## 行書き換えを伴う拡大変換（Issue #1361）

`INTEGER → BIGINT`・`REAL → DOUBLE PRECISION` を受理する（ポインタ: TABLE-19・
TABLE-12）。物理フレーム幅が変わる（presence 1 + 4 → presence 1 + 8）ため、
カタログ更新と**同一 write txn 内**で全テナントの既存行を再エンコードする
（`crates/engine/src/column_rewrite.rs`）。

- **方式**: 行バイト列にスキーマ版が無く幅の異なる行が混在できないため、読み出し時の
  読み替え・墓標＋新スロット追加は採らない。行キー `(tenant_id, id)` を件数上限付き
  （1,024 件）のバッチで集め、各行を旧スキーマでデコード → 対象列だけ拡大 → 新スキーマで
  エンコードして同じキーへ書き戻す。tenant・visibility・embedding は保存する。
  所要時間は O(行数) で、その間は書き込みゲートを保持する。
- **原子性・fail-closed**: キーとヘッダの tenant 不整合・デコード／エンコード失敗
  （ペイロード上限超過を含む）は `CorruptSchema`（`XX000`、固定文言。tenant・id・値を含めない）
  で全体を中止し、commit しない（カタログ・行・世代に痕跡なし）。
- **依存検査**: CHECK 参照列の `DROP COLUMN` は従来どおり `2BP01`。`ALTER COLUMN TYPE` は型互換性判定（縮小・非互換は `42804`）の後に CHECK 依存ゲートを置き、行の書き換えを伴う拡大変換（`INTEGER`→`BIGINT`・`REAL`→`DOUBLE PRECISION`）のみ受理する（Issue #1427。評価結果が列型に依存しないことが根拠。同一 txn 内で変更列を参照する CHECK を新スキーマで再コンパイルし、全テナント・全可視性の既存行を再検証する多層防御付き。違反は `23514`、式評価エラーは透過）。NUMERIC 精度拡大は CHECK 参照下で `2BP01` のまま。
  PK／UNIQUE／FOREIGN KEY（参照元・参照先の両方）の構成列も受理する（Issue #1402。TABLE-19・
  TABLE-17）。正準キーは型タグ付きで `INTEGER 5` と `BIGINT 5` が別バイト列になるため、
  同一 write txn 内で永続一意索引 `user_uniq/{table}`（PK／UNIQUE 構成列のとき）と、変更列を
  含む `key_index`（全テナント分。fwd／rev／登録簿）を失効させ、次の検査で新しい型により遅延
  再構築する（DDL 内で全テナント分を一括構築しない）。参照元・参照先の型が食い違う FK は
  [foreign-key.md](./foreign-key.md)「列型が食い違う FOREIGN KEY」のとおり境界でキーを読み替える。
- **DEFAULT の実体化**: 再エンコードで、ADD COLUMN 前に書かれた行の欠落列は既定値で
  実体化される。`REAL DEFAULT 0.1` を DOUBLE へ拡大すると既存行は `f64::from(0.1f32)`、
  拡大後に列を省略した INSERT は `0.1f64` になる（観測可能な差としてテストで固定）。
- **失効**: 既存の世代 bump により索引・アリーナ等のテーブル世代キャッシュが失効する。
- **範囲外のまま `42804`**: 縮小・同一型・`INTEGER → DOUBLE PRECISION`・`REAL → BIGINT` 等。

## スコープ外・後続 Issue

- NoSQL 表層の `ALTER COLUMN ... TYPE` 相当
- `DROP TABLE`（#902）・VIEW/FK/INDEX の依存検査（2BP01・#907〜#909）・
  明示トランザクション内の DDL（#942 系）

## 実装ファイル

- `crates/engine/src/catalog.rs`: `DroppedSlot`・`PhysicalSlot`／
  `PhysicalSlots`・`TableSchema::{from_parts, dropped_slots,
  physical_slot_count, physical_slots}`・カタログ v3 の encode/decode・
  `CatalogError` の 3 variant・
  `Storage::{alter_table_drop_column, alter_table_widen_numeric_precision}`
- `crates/engine/src/row_codec.rs`: `ScalarSlotView`（生存列・墓標を統一的に
  扱う走査ビュー）・`scan_scalar_columns_validated`／`encode_scalar_columns`／
  `merge_encode_scalar_columns` の物理配置対応・`encode_row`／`decode_row`
  （production 未使用の v1 フル行フォーマット）の墓標付きスキーマ拒否
- `crates/engine/src/catalog.rs`（`#[cfg(test)] mod tests`）: DROP COLUMN・
  NUMERIC 精度拡大の単体テスト（バイト不変性・保護列/存在検査・同名再追加・
  DB 再オープン後の永続性・`merge_encode_scalar_columns` の NULL 書き込み）
