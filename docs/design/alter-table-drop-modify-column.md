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
  `ALTER TABLE <t> ALTER COLUMN <c> TYPE <型名> [COLLATE <名前>]` のみ（`COLLATE` は
  TABLE-28・Issue #1609 で追加。未知の照合名は `42704`、照合を持てない型は `42804`）。`COLUMN` 省略・
  `IF EXISTS`・`ONLY`・`CASCADE`／`RESTRICT`・`SET DATA TYPE`・`USING`・
  `RETURNING`・`USING OPERATION_ID` はいずれも `42601`。複数アクションは Issue #1633 以降、
  DROP COLUMN は受理し（[alter-table-multi-action.md](alter-table-multi-action.md)）、ALTER COLUMN TYPE との混在は `0A000`。
  予約列名（`id`／`tenant_id`／`visibility`／`check`／`constraint`。大文字小文字
  無視）は構造検証段で `42601`（カタログ非照会のため存在オラクルにならない）。
- **判定順序**: 構文検証 → DDL 権限ゲート（`42501`）→ テーブル存在確認
  （`42P01`／`42809`）→（ALTER TYPE のみ）目標型解決 → engine の単一 write txn。
- **エラー写像**: 列なし `42703`／VECTOR 列・保護列・最後の 1 列・NUMERIC 範囲不正は
  既存どおり `42601`（構造エラー群）、未登録 ENUM 型名のみ `42704`（Issue #2306）／PK・UNIQUE・FK 参照列（DROP COLUMN のみ）・CHECK 参照列 `2BP01`／非互換な型変更
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
- **範囲外のまま `42804`**: `BIGINT → INTEGER` などの縮小（TABLE-40 の担当）・同一型・`INTEGER → DOUBLE PRECISION`・`REAL → BIGINT` 等。

### SMALLINT の変換（Issue #1560）

ポインタ: TABLE-30・TABLE-19・TABLE-40。変換計画（`catalog::AlterPlan`）は次の 4 種。

| 変換 | 計画 | 行 | `user_uniq`／`key_index` |
| ---- | ---- | -- | ------------------------ |
| `SMALLINT → INTEGER` | 再タグ付け（カタログのみ） | 不変（presence + `i32 LE` が同一） | 失効しない（タグ 2 を共有） |
| `SMALLINT → BIGINT` | 全行を再エンコード | 書き換え | 失効させる（タグ 2 → 3） |
| `INTEGER → SMALLINT` | 範囲検証のみ（全行を走査） | 不変 | 失効しない |
| `BIGINT → SMALLINT` | 検証しつつ再エンコード | 書き換え | 失効させる（タグ 3 → 2） |

- **縮小の範囲**: SMALLINT への縮小に限って受理する。それ以外の縮小は従来どおり `42804`。
- **範囲外は `22003`**: 全テナント・全可視性の行を対象に検証する（可視集合に絞ると不可視行の
  範囲外値を見逃し、読み出し時の decode 失敗を招く）。1 行でも範囲外なら固定文言
  （`smallint out of range`。値・テナント・行 id を含めない）で失敗し、commit しないので
  カタログ・行・索引・世代のいずれにも痕跡が残らない。破損（`XX000`）とは別の結果として扱う。
- **列 DEFAULT**: 縮小時は DEFAULT も `i16` の範囲内であること（`ADD COLUMN ... NOT NULL DEFAULT`
  の列は既存行にバイトが無く、行の走査だけでは見逃すため）。
- **CHECK 依存**: 値を変えない整数型の変換として受理し、再コンパイルと全行再検証を通す。

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

## Issue #1635 追記（ALTER COLUMN TYPE の USING 句の構文と式の束縛。TABLE-40 ポインタ）

`ALTER TABLE <t> ALTER COLUMN <c> [SET DATA] TYPE <型> [COLLATE <名前>] [USING <式>]` のうち、
構文解析と式の束縛・型検査までを実装した。行の書き換えは #1636、依存オブジェクトの再構築は
Issue #1637 の担当で、本 Issue では型検査に通った USING 付きの文を `0A000` で拒否する（式を
無視して従来経路へ流さない fail-closed）。

- **構文**: `SET DATA TYPE` を `TYPE` と同義で受理する。`USING` は `COLLATE` の後ろにだけ置け、
  式は `UPDATE ... SET` 右辺と同じ構文木・ノード予算で解析する。`ALTER <c>`（`COLUMN` 省略）は
  従来どおり `42601`。複数アクション内の `TYPE ... USING` は従来どおり `0A000`
- **実行段の順序**: DDL 権限ゲート → 表の存在確認 → 目標型の解決 → 照合の検査 →
  事前判定（`sql::ddl::precheck_alter_type_conversion`）→ カタログ。束縛エラーが権限の無い主体への
  存在オラクルにならない
- **USING あり**（`sql::alter_using::check_alter_using`）: 組み込み以外の関数・現在時刻関数は
  `42601`、VECTOR は変換元・先とも `42804`、ENUM 変換先と型を決められない式は `0A000`、
  未定義列は `42703`、結果型を代入変換できなければ `42804`。成功時は `0A000`（固定文言）
- **USING 省略**: 旧型から新型への代入変換が SQL-47 の規則表に無い組を `42804`（USING の指定を
  促す固定文言）で拒否する。代入変換はあるが未実装の縮小などは、TABLE-19 の契約どおりカタログの
  `42804` のまま（`catalog::decide_alter_type` は変更しない）
- **PG との差分**: 集計・ウィンドウ関数は `42601`（PG は `42803`・`42P20`）、部分クエリは
  `42601`（PG は `0A000`）、USING 内の `now()` 等は `42601`（PG は受理）
- **#1636 への引き継ぎ**: txn 内での再束縛と世代照合、代入変換ノードの生成、`USING c`（同じ列
  だけの参照）を省略と同値に扱う最適化。`check_alter_using` は現在 `Result<(), _>` で、束縛結果を
  返す形へ広げる
- **公開 API（BREAKING）**: `ValidatedAlterTableAlterColumnType.using: Option<Expr>` の追加

## Issue #1636 追記（ALTER COLUMN TYPE の全行書き換え変換エンジン。TABLE-40 (1)(2)(5)・TASK-278 ポインタ）

`USING <式>` 付きの変換と、代入変換（SQL-47）のある組の `USING` 省略変換を受理し、全行を書き換える。

- **判定順（カタログの write txn 内。`alter_table_alter_column_type_collated`）**:
  1. 型が同じで照合だけが変わる（`USING` なしまたは `USING <対象列>`）→ 従来の `CatalogOnly`
  2. `USING` なしまたは `USING <対象列>` で `decide_alter_type` が受理する組 → 従来の計画（不変）
  3. `USING` なしまたは `USING <対象列>` で同一型・規則表の外（VECTOR・ENUM）・代入変換が無い組 → 従来の
     `IncompatibleTypeChange`（`42804`。表層の事前判定と同じ判定を txn 内でもう一度行う）
  4. 上記以外 → `AlterPlan::RewriteExpr`
- **`RewriteExpr` の処理**（`catalog::alter_column_type_rewrite_expr_in_txn`）: カタログのハンドルを
  手放した後で、依存ゲート → txn 内の旧スキーマへの束縛し直し（事前判定との TOCTOU を作らない）→
  全行の評価・代入変換・再エンコード（`column_rewrite::rewrite_rows_with_conversion_in_txn`）→
  派生索引の失効（多層防御）→ スキーマの書き戻し。世代 bump と commit は既存の唯一の commit 点
- **代入変換**: `sql::cast::bind_scalar_assignment` が `sql::alter_using::bind_alter_conversion` から
  変換ノードを作る。`VARCHAR(n)`／`CHAR(n)` の長さは変換ノードで切り詰めず、評価後の列値化
  （`sql::set_expr::expr_value_to_column_value`。`UPDATE SET` と共有の単一の変換器）で `22001`／空白詰め
- **範囲**: 全テナント・全可視性の行を行数の上限なしで走査する（不可視な行の違反を見逃すと
  読み出し時の decode 失敗を招くため `PolicyContext` で絞らない）。単一 write txn のため書き込み
  ロックを保持し続ける（仕様上の前提）
- **失敗**: 評価・変換エラーは commit せず表は不変。`redact_row_conversion_error`（全 variant を
  ワイルドカード無しで列挙する網羅 match）が SQLSTATE を保ったまま固定文言へ置き換え、値・テナント・行 id を
  応答へ載せない
- **依存ゲート（暫定）**: PK／UNIQUE／この表の FK／他表の FK 参照先／CHECK／索引宣言／ビューが参照する列と、
  DEFAULT・補完値を持つ列への一般変換は `0A000`。型付きの正準キー・旧型の DEFAULT・古い索引が新しい値と
  食い違う fail-open を避けるため。**Issue #1637 で解除した**（下の「Issue #1637 追記」）。既存の受理経路
  （INTEGER→BIGINT 等）は依存があっても不変
- **`USING <対象列>`**: 省略と同じ扱い（既存の拡大経路を通る）
- **従来どおりの拒否**: 複数アクション内の `TYPE`（`0A000`）・明示トランザクション内の DDL（`0A000`）・
  VECTOR（`42804`）・ENUM が変換元／先になる組（USING あり `0A000`／省略 `42804`）
- **PG との差分**: 2^53 を超える BIGINT への変換は式評価が f64 で運ぶため `22003`。PG は受理する。
  ENUM→TEXT などの代入変換は未対応
- **セッション TZ**: TIMESTAMP↔TIMESTAMPTZ の変換は評価時にセッション TimeZone を読む。ALTER TABLE の
  実行は `core.rs` の `session_time_zone_scope` の内側で行われる

## Issue #1637 追記（ALTER COLUMN TYPE の依存オブジェクト再構築。TABLE-40 (3)(4)・TASK-278 ポインタ）

Issue #1636 の暫定ゲート（依存を持つ列を一律 `0A000`）を外し、一般変換（`AlterPlan::RewriteExpr`）が依存する
索引・制約・DEFAULT を新しい型で再構築・再検証する。ビューが参照する列だけは PostgreSQL と同じ `0A000`
（固定文言 `cannot alter type of a column used by a view or rule`）で拒否する。

- **処理順（単一 write txn。`catalog::alter_column_type_rewrite_expr_in_txn`）**:
  1. ビューのゲート（`column_referenced_by_views_in_txn`。直接参照・`SELECT *` 展開・連鎖を検出し、導出不能は
     fail-closed。データを読まない）
  2. 旧スキーマへの束縛し直し → 新スキーマの組み立て
  3. 静的検証（`validate_dependents_statically_in_txn`。行を読まない）: 変更列を含む索引宣言へ
     `validate_index_columns` を再適用（索引化未対応の型 `0A000`・照合を持てない型への照合 `42804`）、
     PK／UNIQUE の許可型（`0A000`）、FK 参照元の許可型（`42804`）、変更列を参照する CHECK の再束縛
     （失敗はその束縛エラーのまま。`XX000` に丸めない）
  4. DEFAULT・補完値の変換（下記）
  5. 全行の書き換え → `user_uniq` と `key_index` の失効 → 変更列を参照する CHECK の全行再検証（`23514`）→
     スキーマの書き戻し
  6. 呼び出し元が `revalidate_keys_after_type_change` で PK／UNIQUE の重複（`23505`）・FK の型照合（`42804`）・
     参照整合性（`23503`。自表・他表・自己参照）を再検証し、世代 bump と commit（唯一の commit 点）
- **再検証の範囲**: 全テナント・全可視性の行（`PolicyContext` で絞らない）。拒否時は commit せず表は不変。
  エラー文言は固定で、値・テナント・行 id を含めない
- **DEFAULT**: 旧型から新型への**代入変換**（SQL-47。`USING` 式は使わない。PG と同じ）で変換し、新しい型の
  正準形へ戻して再束縛した値が一致することを確かめる（`sql::alter_using::convert_column_default_for_alter`）。
  変換不能・往復不一致は値を含まない固定文言の `42804`。例: `TEXT DEFAULT '5'` を `INTEGER` へは代入変換が
  無いため `42804`（PG の "default for column cannot be cast automatically" と同じ）。`BIGINT DEFAULT 300`
  を `INTEGER` へは受理。時刻値関数の DEFAULT は新しい型と整合するときだけ保持し、それ以外は `42804`
- **補完値（`fill_value`）**: 全行の再エンコードで各行へ実体化される（読み出し時の補完は旧スキーマで適用済み）。
  変換後の `fill_value` は `default_expr` とミラーなら同じ値、食い違うときは変換できなければ破棄する
- **対象外・PG との差分**: 拡大経路（INTEGER→BIGINT 等）はビュー依存があっても従来どおり受理する
  （変更すると破壊的になるため別途）。BOOLEAN・JSONB など PG では索引・キーにできる型への変換は `0A000`。
  時刻 DEFAULT の非互換型への変換は `42804`（PG は受理）。式索引・部分索引（INDEX-10）・マテリアライズド
  ビュー（TABLE-42）は未実装のため対象外。複数アクション内の `TYPE` は `0A000` のまま。`VECTOR(N)` の次元変更は
  TABLE-19 の `42804` のまま
- テスト: `crates/engine/tests/sql_alter_column_type_dependents.rs`（新規）・既存の `*_rewrite`／`*_check_columns`／
  `*_key_columns`／`sql_ddl_drop_alter_column`・wire は `wire_ddl_drop_alter_column.rs`
