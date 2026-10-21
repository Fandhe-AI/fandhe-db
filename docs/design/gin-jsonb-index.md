# ADR: jsonb の包含検索用索引（GIN 相当）の格納形式と演算子クラス（Issue #2205）

- ステータス: Accepted（設計記録。後続 #2206・#2207・#2208・#2209 で実装する）
- 対応 Issue: #2205（親 #2204。後続 #2206・#2207・#2208・#2209）
- 関連ポインタ: `docs/spec/04-behavior/indexing.md`（INDEX-8。関連 INDEX-5〜7・INDEX-9〜11）・
  TABLE-25（jsonb 演算子）・TABLE-12・TABLE-16・RLS-9・RLS-10・SQL-6・SQL-27（EXPLAIN）・ERR-6。
  spec 本文は転記しない。契約は spec を正とし、本書では再掲しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/json.rs`（`jsonb_contains`・`top_level_key_set`・
  `scalar_equality_key`／`write_equality_canonical`・`canonicalize_jsonb_text`）・
  `json/jsonpath/`・`constraint/unique_index.rs`・`constraint.rs`
  （`enforce_row_constraints_in_txn`・`forget_unique_index_rows_in_txn`）・`catalog.rs`
  （`IndexKind`・index 定義の版 `v1`〜`v7`・`bump_table_generation_in_txn`）・`sql/pk_point.rs`・
  `sql/scalar_index.rs`・`jsonpath_limits.rs`
- 関連設計: [unique-index.md](unique-index.md)・[primary-key-point-lookup.md](primary-key-point-lookup.md)・
  [index-ddl-declaration.md](index-ddl-declaration.md)・
  [scalar-secondary-index.md](scalar-secondary-index.md)・[jsonpath-limits-cli.md](jsonpath-limits-cli.md)・
  [temp-table.md](temp-table.md)
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

本書の決定は、公開 Issue（#2204〜#2209）・PostgreSQL 17 の公開された挙動・本 ADR 独自の実装判断で構成する。
独自判断には「**自動運転下の判断で、人間の確認が要る**」を付す。

## 1. 背景・目的

現状の索引宣言は `USING hnsw` とスカラー宣言のみで、`USING gin` は `0A000` で拒否される。
jsonb の `@>`・`?`・`?|`・`?&`・`@?`・`@@` は常に全走査で評価される。
親 #2204 は PostgreSQL 17 相当の `CREATE INDEX ... USING gin (col [jsonb_ops | jsonb_path_ops])` を求める。
実装の判断がぶれないよう、先に次を確定する。

1. 永続化モデル（redb 永続か、メモリ常駐の世代整合キャッシュか）
2. 演算子クラスごとのキー抽出規則と PostgreSQL との差
3. ポスティングの物理形式・圧縮の採否・上限
4. 正しさの不変条件・DML 維持の更新コスト
5. テナント境界・crash 耐性・世代との整合
6. 後続 Issue への割り当て

## 2. 前提（公開コードの現状）

| 事実 | 所在 |
| ---- | ---- |
| jsonb は正規化テキスト（キー昇順・空白なし・数値は原文リテラル）で格納される | `canonicalize_jsonb_text` |
| 数値の値等価は `1`≡`1.0`≡`10e-1`、`-0`≡`0`。包含判定はこれを使う | `scalar_equality_key` |
| `?` 系の「キー」はオブジェクトのキー・配列の文字列要素・最上位の文字列スカラー | `top_level_key_set` |
| 包含判定のステップ上限と jsonpath の評価上限（CLI で下げられる） | `MAX_JSONB_CONTAINMENT_STEPS`・[jsonpath-limits-cli.md](jsonpath-limits-cli.md) |
| 索引定義の版は `v1`〜`v7` が使用済み。`IndexKind` は `Scalar`／`Hnsw` | `catalog.rs` |
| 永続一意索引は tenant 先頭キー・マーカー・正引き／逆引き・遅延バックフィル・stale 許容 | [unique-index.md](unique-index.md) |
| 読み取り側は行テーブルからテナントを列挙する | [primary-key-point-lookup.md](primary-key-point-lookup.md) |
| 書き込み検査の唯一のチョークポイントと削除側の後片付け | `enforce_row_constraints_in_txn`・`forget_unique_index_rows_in_txn` |
| 一時表は一時コアで同じ実行器を使う | [temp-table.md](temp-table.md) |

## 3. 決定

### D1. 永続化モデル: redb 永続索引（メモリ常駐キャッシュは不採用）

- 採用: 永続一意索引と同型の redb 永続索引を、行の書き込みと**同一 write txn** で維持する。
- 不採用: `ScalarIndex` 型のメモリ常駐・テーブル世代整合キャッシュ。理由は次のとおり。
  1. 世代が進むたびに全行の jsonb を再パースしてキー抽出する cold 構築が重い。
  2. DML 維持・初回構築・crash テスト（#2207・#2209）の要件と整合しない。
  3. 書き込み時の上限超過 `54000` を副作用ゼロで返すには、結局書き込み経路でキー抽出が要る。
- 構築はテナント単位のマーカー＋遅延バックフィル（[unique-index.md](unique-index.md) と同じ）。
  `CREATE INDEX`／`DROP INDEX` の世代 bump は既存のメモリ常駐キャッシュを失効させるためだけに行い、
  GIN 索引自体の正しさは世代に依存しない。
- 実装 Agent が `ScalarIndex` を類推で流用しないよう、この不採用理由を残す。

### D2. 物理テーブルと命名

- 索引ごとに 1 本の redb テーブル `user_gin/{index_name}`（`/` は識別子に含まれず衝突しない）。
  `DROP INDEX` は `delete_table` 1 回で済み、`ALTER TABLE RENAME` で物理移動が要らない。
- `DROP TABLE`・`DROP COLUMN`（連鎖削除）・CASCADE・TRUNCATE での掃除漏れを防ぐため、
  **表に付随する物理テーブル群を列挙・削除する単一ヘルパー**を #2206／#2207 で設けることを必須とする
  （`catalog.rs` に `user_uniq` の `delete_table` 呼び出しが多数散在している現状が根拠）。
- SCHEMA 修飾下の内部名は index 定義のキーと同じ内部名を使う。実装時に確認する
  （**自動運転下の判断で、人間の確認が要る**）。

### D3. キー形式（1 ポスティング = 1 エントリ）

キーは `(tenant_id, subkey)`。第 1 要素はサーバー側で導出したテナント。`subkey` の先頭 1 バイトで名前空間を分ける。

| 先頭 | 内容 | 値 |
| ---- | ---- | -- |
| `0x00` | マーカー | `[format_version][signature]`（signature は演算子クラス・列名・列型タグの正準エンコード） |
| `0x01` | ポスティング `[0x01][entry key][rowid u64 BE]` | 空 |
| `0x02` | 逆引き `[0x02][rowid]` | その行の entry key 列（衛生用） |

| 案 | 判断 |
| -- | ---- |
| (i) 1 ポスティング = 1 redb エントリ | **採用**。DML は点 insert/delete でキー単位の read-modify-write が要らず、値サイズ上限の設計も不要。rowid 昇順なので `@>`・`?&` は k-way マージ交差、`?\|` は和 |
| (ii) キー → rowid 列のチャンク（delta/varint 圧縮） | 不採用。PG の posting tree 相当だが更新が read-modify-write になり、crash 整合と競合判定が複雑。索引サイズが実測で問題化したら再評価 |

圧縮は採用しない（redb のページ単位で足りるとする）。PG との差として記録する
（**自動運転下の判断で、人間の確認が要る**）。

### D4. 正しさの不変条件

[unique-index.md](unique-index.md) の非対称性を踏襲する。

- マーカーが有効なテナントでは、生存行の抽出される全 entry key にポスティングが**必ず**存在する（欠落不可）。
- 削除済み行・値が変わった行を指す stale なポスティングは**許容**する。読み取り側の再検査で除かれる。
  UPDATE／DELETE の後片付けは衛生措置であり、正しさの依存先にしない。
- 欠落は偽陰性（取りこぼし）になるため、書き込み経路の網羅は D8 のチョークポイントで保証する。

### D5. `jsonb_ops` のキー抽出規則

- 抽出対象: オブジェクトのキー（全階層）と配列の文字列要素を「キー」、スカラー値（全階層）を「値」とし、
  キー／値を区別するタグ 1 バイトを付ける。
- **最上位の文字列スカラー**（jsonb 値そのものが文字列）は、`top_level_key_set` が最上位の文字列もキーとして
  扱うため、値タグのエントリに加えて**キー用タグのエントリも**抽出する。これが無いと D5 の単一キー照会が
  ポスティングを引けず、再検査（recheck）へ到達する前に行が除外される偽陰性になり、D4 の欠落不可に反する。
  最上位以外の文字列スカラー（オブジェクトの値）は値タグのみとし、配列の文字列要素は従来どおりキー扱い。
- `?` 系の意味論は `top_level_key_set` を上位集合として満たす。
- 値の entry key は `scalar_equality_key`／`write_equality_canonical` と**同一の数値正規化**を通す。
  原文テキストを使うと `@> '{"a":1.0}'` が `{"a":1}` を取りこぼす偽陰性になる（必須事項）。
- 問い合わせ側: `@>` は右辺から同じ規則で抽出したキーの AND、`?` は単一キー、`?|` は OR、`?&` は AND。
  **右辺が最上位の文字列スカラー**のときだけ、`@>` は**キー用タグのエントリのみ**で候補抽出し、値タグは AND に含めない
  （例: `'["bar"]'::jsonb @> '"bar"'` は、配列の文字列要素にキー用タグしか付かないため、値タグを要求すると
  ポスティングが無く再検査の前に行が落ちる偽陰性になる。キー用タグだけなら `["bar"]` も最上位文字列 `"bar"` も
  候補に残り、`jsonb_contains` による再検査で正誤が確定する）。これにより D4 の欠落不可は、配列の文字列要素を
  含む全ての格納形に対して成り立つ。
  **抽出キーが 0 個**（`@> '{}'`・`@> '[]'`・`?&` の空配列等）のときは索引を使わず全走査する
  （「索引から全行」を返す経路を作らない）。
- PostgreSQL の `jsonb_gin.c` 相当の細部（長いキーのハッシュ化等）との差は §4 に列挙する。

### D6. `jsonb_path_ops` のキー抽出規則

- 値ごとに「ルートからの経路（キーの列）＋値」をハッシュした entry key を抽出する。
  PG と同じく経路中のキー単体は索引化しないため、`?` 系は非対応で全走査とする。
- ハッシュは固定・決定的で、依存を追加しない（既存の自作実装の流用または自作）。暗号強度は不要
  （衝突は再検査で除かれる）。ハッシュ値は永続形式の一部で、変更時は `format_version` を上げる。
- `@>` は右辺の各葉の経路ハッシュの AND。`@?`／`@@` は**列挙による許可制**とする。
  単純なキーアクセサ連鎖（`$.a.b`）と等価比較のような狭い形だけを対象にし、lax/strict の差・
  ワイルドカード・フィルタ・算術・メソッド等を含む形は全走査にする
  （[scalar-secondary-index.md](scalar-secondary-index.md) の哲学と同じ）。許可形状の確定は #2208 に委ねる。

### D7. 上限と `54000`、CLI

- 1 値あたりの抽出キー数と 1 キーのバイト長に既定上限を置く。超過する値の書き込み・`CREATE INDEX` は
  `54000` で、副作用ゼロ（write txn を commit しない）にする。
- CLI は [jsonpath-limits-cli.md](jsonpath-limits-cli.md) と同型にする。ハード上限＝既定値・下げる方向のみ・
  範囲外／重複指定は起動エラー・範囲検証は engine に一本化・プロセス大域状態を使わず
  `EngineCore::with_*` で注入する。
- フラグ名の候補は `--max-gin-keys-per-value`・`--max-gin-key-bytes`。既定値は #2207 で確定する
  （**自動運転下の判断で、人間の確認が要る**）。
- PG は長いキーをハッシュ化して受理するが、本実装は fail-closed 側で拒否する（`jsonb_ops`）。

### D8. DML 維持とチョークポイント

- 書き込み側は `enforce_row_constraints_in_txn`（書き込まれた rowid を受ける既存の唯一の検査点）から維持を呼ぶ。
  削除側は `forget_unique_index_rows_in_txn` を呼ぶ削除経路に並べて後片付けを呼ぶ。
- #2207 が網羅を確認する経路: 単一／複数行 INSERT・UPSERT・UPDATE（単一行・述語形）・DELETE・COPY・
  TRUNCATE（自テナント範囲の一括削除）・分割実行 DML・ビュー経由 DML・NoSQL insert/update/delete・
  ファイル形 INSERT・`ALTER TABLE`（DROP COLUMN 連鎖削除・ALTER COLUMN TYPE・RENAME COLUMN）・
  DROP TABLE／CASCADE・RENAME TABLE・SET SCHEMA・一時表。
- 更新コストは 1 行あたり O(k・log n)（k = 抽出キー数）。UPDATE は逆引きとの差分だけを書く。
- 初回構築: `CREATE INDEX` は既存行を同一 write txn で走査して上限検証する
  （`CREATE UNIQUE INDEX` の重複走査と同じ流儀）。構築そのものを同期にするか遅延バックフィルに委ねるかは
  #2207 で確定する（推奨: 上限検証のみ同期、構築はテナント単位の遅延バックフィル＋マーカー）。
  INDEX-7 の「行数非依存」との関係は、上限検証のため行数依存になる差として明記する
  （**自動運転下の判断で、人間の確認が要る**）。

### D9. 読み取り側（#2208 の契約）

- テナントは**行テーブルから**列挙する（索引テーブルから列挙するとマーカーのないテナントを見落とす）。
- マーカー有効なテナント: ポスティングの範囲走査を交差／和で rowid 列にし、行を読み戻して全走査と同じ
  可視性判定（TABLE-12 整合）と**元の述語の再評価（recheck）**を必ず通す。
  マーカー無効なテナント: そのテナントの行範囲を全走査する。
- 交差はストリーミングで行い、候補全件のメモリ実体化を避ける。
- 出力順はテナント昇順 → rowid 昇順で、全走査と一致させる。
- 読み取り側は索引を書き換えない（バックフィルは書き込み側だけ）。
- EXPLAIN には既存規則で索引名を表示する。

### D10. テナント境界（RLS）

- 書き込み・バックフィル・TRUNCATE は自テナントのキー範囲に閉じる。
- 候補は可視性再検査で確定するため、他テナントの行は結果に現れない。
- エラー文言は固定で、テナント・値・件数を含めない。上限検証は書き込み対象の値と `CREATE INDEX` に限り、
  索引の有無による観測差が他テナントのデータ由来で生じないようにする。

### D11. crash 耐性・世代との整合

- 索引更新は行の書き込み・台帳記録と同じ write txn・同じ commit 内で行うため、原子性は構造的に保証される。
- GIN は永続索引でテーブル世代に依存しない。`CREATE/DROP INDEX` は世代を bump する（既存キャッシュの失効）。
- #2209 の crash テスト方針: 一意索引の crash ツールと同型の専用ツール＋スクリプト＋CI ジョブ。
  オラクルは「再起動後、全行の抽出キーで索引照会した結果が全走査と一致する（欠落 0）」。
  CI 必須チェックへの登録は運用者作業として申し送る。

### D12. カタログ表現（#2206 の契約）

- `IndexKind::Gin` を追加し、index 定義は新形式 `v8`（種別 `gin`＋演算子クラス `jsonb_ops`／`jsonb_path_ops`）とする。
  `v1`〜`v7` のバイト列は不変。旧バイナリは `v8` を読めず fail-closed になる。
- 対象列は JSONB のみ。`JSON` 列（原文保持・重複キー後勝ち）・複数列・式・部分索引・`UNIQUE`・`COLLATE` との
  組み合わせは `0A000`（**自動運転下の判断で、人間の確認が要る**）。
- 一時表上の GIN は一時コアで同じ実行器を使うため原則動くが、受理範囲は #2206 で確定する
  （未対応にするなら `0A000`）。

## 4. PostgreSQL 17 との差

| 項目 | 本実装 | fail-closed 側か |
| ---- | ------ | ---------------- |
| ポスティングの圧縮・posting tree | なし（1 ポスティング 1 エントリ） | 該当せず（サイズ差のみ） |
| 長いキー | ハッシュせず `54000` で拒否（`jsonb_ops`） | はい |
| 複数列 GIN | `0A000` | はい |
| `JSON` 列 | `0A000` | はい |
| fastupdate／pending list | なし（同期維持） | 該当せず |
| `CREATE INDEX` | 上限検証のため行数依存 | 該当せず |
| `@?`／`@@` の索引化 | 許可形状のみ、他は全走査 | はい（全走査へ倒す） |
| 他の演算子クラス（`array_ops`・`gin_trgm_ops` 等） | 対象外 | はい |

## 5. 後続 Issue への割り当て

| Issue | 担当する決定 |
| ----- | ------------ |
| #2206 | D2（宣言側）・D12 |
| #2207 | D3・D4・D5／D6（抽出）・D7・D8 |
| #2208 | D5／D6 の問い合わせ側抽出・D9・EXPLAIN |
| #2209 | D10・D11 のテスト（越境マトリクス・crash） |

## 6. 対象外

`array_ops`・全文検索 GIN・`gin_trgm_ops`、圧縮、`CONCURRENTLY`、専有環境ベンチ、索引サイズの統計露出。

## 7. Issue #2206 実装記録（宣言とカタログ）

INDEX-8 ポインタ（関連: INDEX-7・TABLE-36・TABLE-37・ERR-6）。D2（宣言側）と D12 を実装した。
本 Issue はポスティング・マーカーを書かず、読みもしない（作成・維持は #2207、プランナ接続は #2208）。
宣言があってもマーカーの無いテナントは全走査になるため、結果集合と RLS は宣言の有無で変わらない。

### 7.1 カタログ表現

- `IndexKind::Gin(GinOpClass)`（`GinOpClass` は `JsonbOps`／`JsonbPathOps`）。演算子クラスを種別に持たせ、
  「演算子クラスの無い GIN」を型で作れなくした。公開 enum への variant 追加のため破壊的変更（CHANGELOG 参照）。
- 永続形は新版 `v8`（`v8／テーブル／gin／列（1 個）／演算子クラス`）。`v1`〜`v7` のバイト列は不変。旧バイナリは
  `v8` を未知の版として拒否する（前方互換なし）。復号は厳格で、kind 不一致・列数不正・未知／非正準の演算子クラス・
  末尾の余分な行・`v1`〜`v7` の `gin` はすべて `CorruptSchema`。
- 物理テーブル名は `user_gin/{索引名}`（キーは `(tenant_id, subkey)`）。本 Issue では作らず、`DROP INDEX`・
  `DROP TABLE [CASCADE]`・`DROP COLUMN` の同一 txn で（存在すれば）削除する。

### 7.2 受理範囲と SQLSTATE

| 形 | 結果 |
| -- | ---- |
| `USING gin (列 [jsonb_ops｜jsonb_path_ops])`（名前は大文字小文字不問。省略時 `jsonb_ops`） | 受理 |
| 未知の演算子クラス | `42704`（暫定） |
| 複数列・式要素・`UNIQUE`・`WHERE`・`COLLATE` | 構文段で `0A000`（`COLLATE` は PG では `42804`。D12 に従い差として記録。暫定） |
| JSONB 以外の列（`JSON` を含む）・疑似列 `id` | カタログ段で `0A000` |
| マテリアライズドビュー | `0A000`（HNSW と同じ） |
| 一時表 | 受理（暫定。D8 の網羅対象のため） |
| `DROP INDEX ... CASCADE` | 全種別で `42601` のまま（GIN 固有の扱いはしない） |
| GIN の付いた列の `ALTER COLUMN TYPE` | 既存の静的検査により `0A000` |

`IF NOT EXISTS` は既存の実行器の読み替えがそのまま効く（定義は比較しない）。構文段はカタログを参照しない。

### 7.3 要確認（人間の判断が必要な暫定事項）

未知の演算子クラスの `42704`、`COLLATE` 付きの `0A000`、一時表での受理、SCHEMA 修飾下の物理名
（索引名の内部キーが変わる PR とのマージ順で追随が要る）。

## 8. Issue #2207 実装記録（キー抽出と DML 維持）

INDEX-8 ポインタ（関連: INDEX-7・TABLE-12・TABLE-16・RLS-9・RLS-10・ERR-6）。D3・D4・D5／D6（格納側の抽出）・D7・D8 を
実装した。読み取り側（プランナ接続・recheck・EXPLAIN）は #2208、crash テストと越境マトリクスは #2209 の担当で、本 Issue は
索引を読まない（診断 API を除く）。実装は `crates/engine/src/gin_index.rs`・`gin_index/extract.rs`。

### 8.1 物理形式（永続形式。`FORMAT_VERSION = 1`）

テーブル `user_gin/{索引名}`、キー `(tenant_id, subkey)`、値 `&[u8]`。

| 名前空間 | subkey | 値 |
| -------- | ------ | -- |
| マーカー | `[0x00]` | `[format_version: u32 BE][signature]` |
| ポスティング | `[0x01][entry_len: u16 BE][entry][rowid: u64 BE]` | 空 |
| 逆引き | `[0x02][rowid: u64 BE]` | `[count: u32 BE]` と `count` 個の `[len: u16 BE][entry]` |

- D3 の `[0x01][entry][rowid]` に**エントリ長の接頭辞を足した**。接頭辞が無いとキー `"a"` の範囲走査にキー `"ab"` の
  ポスティングが混ざる（プレフィックス自由でない）ため、#2208 の範囲走査が正しく動くよう形式に入れた。末尾の rowid は昇順で、
  k-way マージ交差に使える。
- `signature` は `[演算子クラス: u8][列名長: u8][列名][列型タグ: u8]`。D3 に従い列名を含めるため、`RENAME COLUMN` の後は
  マーカーが不一致になり、各テナントの次の書き込みで遅延再構築される（読み取り側は再構築までは全走査へ倒れるので結果は変わらない）。
- エントリ形式やハッシュを変えるときは `FORMAT_VERSION` を上げる（旧版のマーカーは取りこぼしなく無効化される）。

### 8.2 抽出規則

- `jsonb_ops`: キー `[0x01][UTF-8 キー]`、値 `[0x02][型タグ][payload]`。数値の payload は `json::scalar_equality_key`
  （`@>` の等価判定と同じ値としての正規化。`1`・`1.0`・`10e-1` が同一、`-0` は `0`）。オブジェクトのキー（全階層）と配列の文字列要素を
  キー、スカラー（全階層。配列の文字列要素を除く）を値とし、最上位の文字列スカラーだけキーと値の両方を出す（D5）。
- `jsonb_path_ops`: 葉スカラーごとに `[0x03][FNV-1a 64 のハッシュ]`。入力はルートからのキー経路（各キーは長さ接頭辞付き）・
  値の型タグ・値の長さ付き payload。配列は経路に段を足さない（透過。`jsonb_contains` の「配列 ⊇ スカラー」規則と整合）。
  ハッシュは自作で依存を足さない（公開ベクタをユニットテストで固定）。空の容器はエントリを出さない（D6）。
- 1 値の中の重複は除いてから数える。jsonb が NULL の行はエントリ 0 件で、逆引きも書かない。

### 8.3 上限と `54000`（D7）

- 既定上限は 1 値あたり 65,535 エントリ（重複除去後）・`jsonb_ops` の 1 エントリ 4,096 バイト。超過した時点で抽出を打ち切る。
  書き込みは `TenantWriteError::GinIndexEntryLimitExceeded`（`54000`。固定文言でテナント・値・件数・索引名を含めない。commit しない）、
  `CREATE INDEX` は `54000`（宣言も索引も残らない）。PG は長いキーをハッシュ化して受理するが、本実装は fail-closed 側で拒否する。
- **CLI フラグ（`--max-gin-keys-per-value`・`--max-gin-key-bytes`）は本 Issue では入れない。** jsonpath 上限の thread-local
  スコープ（`JsonPathLimitsScope`）は `execute_sql_in_txn`・`describe_*` にしか張られず、NoSQL・COPY・直接 API・一時コアの
  書き込みは通らない。このため起動時の値が経路によって効いたり効かなかったりする。CLI は値を下げる方向にしか使えないので、
  入れなくても安全性は落ちない。運搬方式（`Storage` のフィールドか、tenant 書き込み関数の明示引数か）は後続 Issue で決める。
  抽出器と `CREATE INDEX` の走査は `GinLimits`（`Default` が既定値）を受け取るため、差し込み口は用意済み。

### 8.4 初回構築と維持（D8）

- `CREATE INDEX ... USING gin` は同じ write txn で全テナント・全可視性の行を 1 回だけストリーミング走査し、行ごとの抽出・上限検証・
  ポスティング書き込みを行い、テナントごとにマーカーを書く。**ADR §3 D8 の推奨（上限検証のみ同期、構築は遅延）から変更して構築も同期にした**
  （検証で抽出は済んでおり書き込みを足すだけで済む・書き込みの無いテナントでもすぐ索引を使える・テストが決定的になる）。
  INDEX-7 の「行数非依存」との差（`CREATE INDEX` が行数に依存する）はこの変更でも変わらない。
- 書き込みは `constraint::enforce_row_constraints_impl` が `enforce_unique_keys_with_decls` の後・ビューの `WITH CHECK OPTION` の前に
  維持する（PG の検査順と同じ）。FK の参照アクションが子行を書き換える経路も同じ入口を通る。マーカーが無効なテナント
  （新規・TRUNCATE 後・`RENAME COLUMN` 後・形式版更新後）は書き込み時に自テナントの行だけを再構築する。
- 逆引きとの差分（旧 − 新を削除、新 − 旧を挿入）だけを書く。読み戻せない id は削除扱いで後片付けする。
- 後片付け（衛生措置）: 削除系 3 経路（`delete_row_impl`・`apply_predicate_delete_in_txn`・`replace_typed_rows_by_text_key`）・
  FK `ON DELETE CASCADE`。TRUNCATE は自テナントの範囲を空にしてマーカーを書き直す（空の索引として完全な状態）。
  物理テーブルが無いときは作らない（`list_tables` で確かめる）。
- 生書き込みのテスト専用 API は GIN 付きの表を拒否する（検査点を通らず D4 に反するため）。
- DDL: `DROP INDEX`・`DROP TABLE [CASCADE]`・`DROP COLUMN` は #2206 の掃除経路で物理テーブルを消す。`RENAME TABLE`・
  `SET SCHEMA` は物理名が索引名由来でシグネチャに表名を含めないため移動不要。他の列の追加・削除・型変更は rowid と jsonb の値を
  変えないので索引は有効のまま。

### 8.5 診断 API

`EngineCore::verify_gin_index_read_only(ctx, index_name) -> GinIndexAudit { marker_valid, missing, stale }` は自テナントの
範囲だけを読み取り専用で全行抽出と突き合わせる（再構築も書き込みもしない）。受け入れ条件は「どの DML の後でも
`marker_valid && missing == 0`、後片付けのある経路では `stale == 0`」。#2209 の crash オラクルもこの API を使える。

### 8.6 要確認（自動運転下の判断で、人間の確認が要る事項）

- 既定上限（65,535 エントリ・4,096 バイト）。
- 初回構築を同期にしたこと（D8 の推奨からの変更）。
- シグネチャに列名を含めたことで、`RENAME COLUMN` の後に遅延再構築になること。
- 長いキーをハッシュせず拒否すること（PG との差）。
- SCHEMA 修飾下の物理名: 索引名の内部キーが変わる PR とのマージ順で追随が要る（物理名は `delete_gin_index_table_in_txn` と
  同じ出どころから作ること）。

### 8.7 #2208 への申し送り

- 問い合わせ側の抽出は `gin_index::extract`（`pub(crate)`）を使い、書き込みと同じ規則・同じ数値正規化にする。
- ポスティングの範囲走査は `[0x01][entry_len][entry]` を接頭辞にする。マーカー判定は `gin_index::marker_matches` 相当
  （形式版とシグネチャの完全一致）を読み取り専用で行い、不一致のテナントは行範囲の全走査へ倒す。
- 候補は行を読み戻して可視性・TABLE-12 整合・元の述語の再評価（recheck）を通す（stale・ハッシュ衝突はここで除かれる）。

## 9. Issue #2208 実装記録（読み取り側と EXPLAIN）

ビヘイビア: INDEX-8（関連: INDEX-7・TABLE-12・RLS-9・RLS-10・SQL-6・SQL-27）。D5・D6 の問い合わせ側、D9、EXPLAIN を担当する。
実装は `sql::gin_scan`（静的分類と候補探査。`sql::pk_point` と同じ型）、`gin_index::extract` の `query_*`（問い合わせ側の抽出）、
`gin_index::GinIndexProbe`（読み取り専用のマーカー照合）、`sql::scan` の dispatch、`core.rs::scan_explain_result`。

### 9.1 適用範囲と選択規則

- 対象は順序なしの広域取得（`ORDER BY`・ウィンドウ無し）。経路の優先順位は PK 点検索 → GIN → 全走査で、実行と EXPLAIN が
  同じ分類関数（`classify_gin_scan`）を呼ぶ。
- 索引候補にするのは `expr_filters`（トップレベルの AND 連言）で**最初に条件を満たした 1 述語**だけ。`or_filters`・`NOT` の内側・
  複数述語や複数索引の交差は対象外。
- `@>` は `jsonb_path_ops` を先に、次に `jsonb_ops`（どちらの中でも索引名の昇順）。`?`・`?|`・`?&` は `jsonb_ops` のみ、
  `@?`・`@@` は `jsonb_path_ops` のみ。`<@`・左辺がリテラルの `@>`・右辺がリテラルでない形は全走査。
- `@?`・`@@` は演算子形のみ（`vars` なし・`silent` 定数 true・SQL/JSON 句なし）で、lax モードの次の許可形だけ索引化する。
  `@@` は `$.a.b == <リテラル>`（左右どちらでも可）。`@?` は `$.a.b ? (@.c.d == <リテラル>)`（フィルタは末尾の 1 個。`@` 単独も可）。
  リテラルは文字列・真偽・数値（`null` と符号付き数値は対象外）。ワイルドカード・添字・`.**`・メソッド・変数・算術・
  `!=`／範囲比較・`&&`／`||`・`exists`・`starts with` は全走査。関数形（`jsonb_path_exists(...)`）も全走査。

### 9.2 問い合わせキーと縮退

- `jsonb_ops` の `@>` は右辺に格納側と同じ抽出を適用する（ただし右辺が最上位の文字列スカラーのときはキー用タグのみ。D5）。
  `?` 系はキー用タグ（`?|` は和、それ以外は積）。`jsonb_path_ops` の `@>` は格納側と同じ経路抽出（配列透過）、
  `@?`／`@@` は経路と値から葉エントリ 1 件を作る。葉ハッシュと経路符号化は格納側と**同じ関数**を共有する。
- 次の場合は全走査へ倒す（fail-soft。エラーにしない）: キー集合が空（`@> '{}'`・`?& '{}'`・NULL のみのキー）、抽出が上限を超えた、
  キー数が `MAX_GIN_QUERY_KEYS`（64）を超えた、形を判断できない。
- マーカーが無効（不在・形式版またはシグネチャの不一致）なテナントは、そのテナントの行範囲を走査する。結果は同じ。
- 積は leapfrog 交差（先頭が遠いときは範囲を開き直して飛ぶ）、和は `BinaryHeap` による k-way マージ（重複 rowid は 1 回）。
  メモリは O(キー数)。出力はテナント昇順 → rowid 昇順で全走査の物理順と一致し、`LIMIT`／`OFFSET` の数え方も同じ。

### 9.3 EXPLAIN

`scalar_plan: index_gin index=<索引名>`（`ScalarPlan::IndexGin`）と `access_path: gin_index_candidates`
（`AccessPath::GinIndexCandidates`）。索引名は INDEX-7 と同じ `index=<name>` 接尾辞。EXPLAIN は静的判定だけで `user_gin/*` を開かず、
縮退・候補数・テナントの有無を出さない。索引カタログは GIN になりうる形の述語がある文でだけ読む。

### 9.4 PostgreSQL 17 との差

- `jsonb_ops` では `@?`・`@@` を索引化しない（PostgreSQL は対応）。複数述語の交差（bitmap AND/OR 相当）はしない。
  関数形の `jsonb_path_*` は PostgreSQL も索引化しないため同じ。
- 候補にならない行で述語の評価がエラーになる場合（包含判定のステップ上限等）、全走査と索引経路で表面化のしかたが異なりうる
  （PK 点検索と `LIMIT` の早期打ち切りに同じ前例がある）。正常系の結果集合には影響しない。

### 9.5 要確認（自動運転下の判断で、人間の確認が要る事項）

- EXPLAIN の新語彙（`index_gin`・`gin_index_candidates`）。語彙数は SQL-27 の確定時に spec 側とすり合わせる。
- 問い合わせキー数の上限値（64）。
- `@>` で `jsonb_path_ops` を優先する選択規則。
- jsonpath の許可形から `null` リテラルと符号付き数値・`$` 単独を外したこと。
- SCHEMA 修飾の PR とのマージ順で、物理名（`user_gin_table_name`）と表示名の追随が要ること。

### 9.6 対象外

集計・`GROUP BY`・`ORDER BY` 付き scan・ウィンドウ・検索 SELECT（ハイブリッド）・UPDATE／DELETE の WHERE への適用、
複数索引の交差、`jsonb_ops` での `@?`／`@@`、許可形より広い jsonpath、CLI の上限フラグ、crash テストと越境マトリクス（#2209）。

## 10. Issue #2209 実装記録（越境マトリクスと crash 耐性）

本 Issue はテスト・crash ハーネス・CI 配線だけを追加し、engine 本体と公開 API は変更しない。ポインタ: INDEX-8
（関連: TABLE-12・RLS-9・RLS-10・PERSIST-1・SQL-27）。

### 10.1 越境マトリクス（`crates/engine/tests/rls10_gin_index_paths.rs`）

閲覧テナント 3 つ（tenant-a/b/c を順に閲覧側にする）× 可視性 2 モード（Public のみ／Public＋Private）×
実行経路 2 つ（autocommit／`BEGIN` … `COMMIT`）× 演算子（`@>`・`?`・`?|`・`?&`・`@@`・`@?`・所有者トークンの
`@>`）で、共有索引テーブル経由の越境が無いことを固定する。オラクル側では production の可視性判定を呼ばない。

| 項目 | 内容 |
| ---- | ---- |
| T0 | EXPLAIN が `index_gin`／`gin_index_candidates` を示す（索引経路を実際に通る）。期待集合が空でない組み合わせの下限を固定 |
| T1 | 結果が GIN なしの双子表 `p` とバイト単位で一致し、テスト側の真実値（自テナントの行＋他テナントの Public 行）とも一致 |
| T2 | 他 2 テナントが同じキー・同じ id の Private 行を足しても、閲覧側の応答・EXPLAIN・事後状態・監査結果が不変（flood 不変性） |
| T3 | 閲覧側の INSERT・UPDATE・DELETE・UPSERT・TRUNCATE の前後で、他テナントの行と監査結果が不変 |
| T4 | `RENAME COLUMN` の後に 1 テナントだけ書き込み、マーカーが有効・無効の混在する状態でも結果が `p` と一致（D9 の縮退経路） |
| T5 | 他テナントの UPDATE・DELETE（Public 行を含む）の後に、閲覧側へ旧値が現れない（stale ポスティング） |
| T6 | 自テナントの範囲へ他テナントの行・存在しない行を指す偽装ポスティングを raw redb で差し込んでも、結果は `p` と一致（`user_gin/` 接頭辞でテーブルを列挙し、内部名を決め打ちしない） |
| T7 | 共有 DDL（`CREATE INDEX`）の `54000` が他テナントの行に起因しても、文面は自分の行が原因の場合とバイト単位で同一で、テナント・値を含まない。書き込み時の `54000` は他テナントの状態で応答が変わらない |
| T8 | 試行総数（3×2×2×9）の固定と、捏造した違反を検査器が検出する負の対照 |

Public 行は Rust の行 API でしか作れない（SQL の INSERT は Private 固定）ため、フィクスチャは従来方式の表に
`insert_typed_row` で seed し、索引は後から宣言する（`CREATE INDEX` の初回構築が全テナントを対象にする）。

### 10.2 crash 耐性（`crates/engine/examples/crash_tool_gin_index.rs`・`scripts/crash_test_gin_index.sh`）

D11 の決定どおり、一意索引・採番状態の crash ツールと同型の専用ツール＋スクリプト＋独立 CI ジョブ
（`crash-test-gin-index`）とした。

- `write dml`: 3 テナントを交互に、1 バッチ 1 明示 txn（5 行 INSERT → 先頭行 UPDATE → 2 行目 DELETE → `COMMIT`）で
  `g`（`g_ops`・`g_path` を宣言）と双子表 `p` へ書き続ける。`COMMITTED` を開始同期点とする
- `write ddl`: 3 テナント各 300 行を入れた表で `DROP INDEX` → `CREATE INDEX`（同期構築の 1 txn）を繰り返し、
  構築・破棄の途中の kill を狙う
- `verify`: `g` と `p` の行集合の一致、行を持つ全テナント × 宣言済み全索引の `verify_gin_index_read_only` が
  `marker_valid && missing == 0 && stale == 0`（§8.5 の診断 API を crash オラクルに使う）、演算子ごとの固定クエリの
  `g`／`p` 一致と他テナント行の不在、`ddl` では宣言と物理テーブル `user_gin/<名前>` の原子性
- `self-check`: 小さな 2 テナントの DB で片方のテナントのポスティングを raw redb で全削除し、監査と読み取りの
  オラクルが欠落を検出すること、他テナントの監査が clean のままであること（監査がテナント単位）を確かめる
- スクリプトは奇数セットが `dml`、偶数セットが `ddl`。既定はセット数 4・反復回数 10（手元の実測は約 11 秒）。
  `write.log` の最終 `COMMITTED` の行数を下回れば data loss として失敗する

### 10.3 要確認（自動運転下の判断で、人間の確認が要る事項）

- ruleset の必須チェックへ `crash-test-gin-index` を登録するのはオーナー作業（登録までは必須として扱われない）。
  必須チェック名の表（`.claude/rules/github-actions.md`）には追記済み
- T7 は、`CREATE INDEX` の `54000` が他テナントのデータに起因しうる（1 ビットの観測差）ことを、D10 が許容した
  挙動として固定する。許容の見直しは設計判断
- crash の既定セット数と反復回数（時間予算の見込みから決めた値）

### 10.4 対象外

NoSQL 表層（フィルタ構文に jsonb の包含・キー存在演算子が無く、GIN の読み取り経路に到達しない）、pg wire 経由の
越境（SQL は engine の同じ入口を通るため結合テストで代表させた）、GIN の適用範囲外の経路（§9.6）、CLI の上限フラグ。
