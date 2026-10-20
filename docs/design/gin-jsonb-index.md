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
