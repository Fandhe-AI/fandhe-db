# 論理ダンプのスキーマ側（DDL の書き出し）

- ステータス: スキーマ側は実装済み（Issue #2160）、表データ側は実装済み（Issue #2161。本書 §11）。CLI は実装済み（Issue #2162。本書 §12）、往復試験（#2163）は後続
- 対応: Issue #2160（親 #2159）。関連ビヘイビア: RECOVER-16 (1)。ポインタ: `docs/spec/04-behavior/recovery.md`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/catalog/ddl_export.rs` と配下（`quote.rs`・`table.rs`・`object.rs`・`access.rs`・`data.rs`）、入口は `Storage::export_schema_ddl`・`EngineCore::export_schema_ddl`、表データは `export_table_data`、両者を単一スナップショットで出すのは `export_logical_dump`
- 関連 doc: [backup-snapshot.md](backup-snapshot.md)（一貫性の取得点の方式）

## 1. 役割と API

カタログを読み取りトランザクション 1 本の上で走査し、本リポの SQL 表層がそのまま受理する DDL 文を依存順に 1 文ずつシンクへ渡す。書き込みを止めず、全文が単一の commit 点に対応する。

```rust
Storage::export_schema_ddl<E>(
    &self,
    scope: &SchemaDdlScope,
    sink: impl FnMut(&DdlStatement<'_>) -> Result<(), E>,
) -> Result<SchemaDdlSummary, SchemaDdlError<E>>
```

- `DdlStatement { section, kind, sql }`: `sql` は 1 文（`;` なし）。`section` は `PreData`／`PostData`、`kind` は作る・設定するオブジェクトの種別
- 出力は `PreData` をすべて、続けて `PostData` をすべて渡す。呼び出し側（#2161・#2162）は `PreData` → データ → `PostData` の順に組み合わせる
- `SchemaDdlError`: `Unsupported`（書き出せないものがあった）・`Catalog`（読み取り失敗）・`Sink`（シンクの失敗）。`Unsupported` の文言は固定文字列で、オブジェクト名・テナント名・内部 ID を含めない
- 失敗時にそれまで渡した文は有効だが、呼び出し側は失敗として扱い出力を破棄する（ダンプ全体を失敗させる契約）

## 2. 区分（PreData／PostData）

| 区分 | 内容 |
| ---- | ---- |
| `PreData` | スキーマ・ロール・ENUM・SEQUENCE・表（列・既定値・PRIMARY KEY・UNIQUE・CHECK）・VIEW・関数 |
| `PostData` | 外部キー・索引・SEQUENCE の現在値（`setval`）・トリガ・ポリシー・行セキュリティ |

データを入れる前に制約・トリガ・ポリシーが効くと流し込み自体が阻害されるため、`pg_dump` と同じくそれらはデータの後ろに置く。表の PRIMARY KEY・UNIQUE・CHECK は表の定義に含める。

## 3. テナントの扱い（調査結果）

カタログのキーを調べた結果、オブジェクトは 2 群に分かれる。

| 群 | オブジェクト | 根拠 |
| -- | ------------ | ---- |
| DB 全体で共有 | `public` の表・VIEW・索引・ENUM・SEQUENCE の定義 | キーが裸の名前 |
| テナントごと | SCHEMA・その配下の表・VIEW・索引・SEQUENCE・関数（キー先頭が `tenant_id`、または `#<schema_id>.` でテナント別カウンタ由来）、ロール、トリガ、ポリシー、行セキュリティの状態、SEQUENCE の現在値 | キー先頭が `tenant_id` |

このため `SchemaDdlScope` は「共有分を出すか」と「どのテナント分を出すか」を取る（`shared_only`・`shared_and_tenant`・`tenant_only`）。1 テナントのダンプは `shared_and_tenant`、複数テナントは共有分を 1 回出したうえで各テナントを `tenant_only` で続ける。

- 他テナントの SCHEMA 配下の relation は、存在を示す出力もエラーも作らず黙って対象外にする（内部 ID から所属テナントは引けないため、選択したテナントの SCHEMA 一覧に無い `#` 付きキーは他テナント扱い）
- 共有の表に対するトリガ・ポリシー・行セキュリティもテナントごとに別々に持つため、選択したテナント分だけを出す
- `public` の表はテナント間で共有されるため、テナント A と B を別々の空 DB へ復元すると同名の共有表が重なる。同じ空 DB へ両方を復元する場合は、共有分を 1 回だけ出す（#2162 が扱う）

## 4. 書き出せないものは省かず失敗させる（fail-closed）

DDL へ戻す手段が無い種別・属性があれば `Unsupported` でダンプ全体を失敗させる。

- 未知の redb 表: スナップショット内のすべての表を `classify_redb_table` で「書き出す種別のカタログ表」か「DDL の対象外（行データ・台帳・世代・カウンタ・導出表）」に分類し、どちらでもない表（新しいカタログ表の追加）と multimap 表があれば、最初の文を出す前に失敗させる。単体テストが分類の網羅を担保する
- 列型・既定値・索引種別・関数言語・トリガ/ポリシーの各列挙は `match` を網羅形で書き、ワイルドカード腕を置かない（variant 追加時にコンパイルエラーで対応漏れを検出する）

現時点で `Unsupported` にするもの（理由は SQL 表層側の制約）:

| 対象 | 理由 |
| ---- | ---- |
| 暗黙 `id` を持つ表（旧方式・移行済み方式） | SQL の `CREATE TABLE` は暗黙 `id` を持たない採番方式の表しか作らない |
| マテリアライズド VIEW | 実体（文脈ごとの行）を DDL へ戻す手段が無い |
| メンバーシップ・権限（ACL）・所有者・既定権限・ブートストラップ権限 | `GRANT`／`ALTER ... OWNER TO` は現在ロールの束縛（付与者の記録）が要り、付与者を指定する構文（`GRANTED BY`）を持たない。黙って落とすと復元後に意図しない権限状態になる（ACL の「実体化済み」印も同様） |
| SCHEMA 配下で現在値を持つ SEQUENCE | `setval`／`nextval` は SCHEMA 修飾・引用符付きの名前を受理しない |
| SCHEMA 配下の単独の SEQUENCE | `CREATE SEQUENCE` が修飾名を受理しない |
| 名前が既定の暗黙 SEQUENCE（`<表>_<列>_seq`）と異なる所有 SEQUENCE、既定でない SERIAL の定義 | 再実行で付く名前・定義が変わる |
| `SECURITY DEFINER` の関数・標準形の本体（`RETURN`・`BEGIN ATOMIC`）の関数・宣言的関数 | SQL で再現できない |
| `WHEN` 付きトリガ | 構文を受理しない |
| 字句解析器が読み戻せない識別子（予約語・記号入り・`NULL`／`DEFAULT` など） | 引用しても受理されないか別の意味になる |

## 5. クォートと注入対策

- 識別子は常にダブルクォートで囲み、内部の `"` を二重化する。ただし本リポの字句解析器は「引用符なしでも 1 個の素の識別子になる語」の引用形しか受理しないため、書き出す前にその条件（`check_name`）を検査し、満たさない名前は連結せず `Unsupported` にする。SCHEMA 修飾名（`schema.name`）と `nextval('...')`・`setval('...')` の文字列内の名前は引用符付きを受理しないため素の名前で書く（同じ検査を通した名前だけ）
- 文字列リテラルは `'` を二重化する（字句解析器はバックスラッシュをエスケープとして扱わない）。NUL を含む値は拒否する
- 数値の既定値は数字・符号・小数点・指数の文字だけで構成されることを確かめてから連結する
- 保存済みの正規化 SQL（VIEW 本文・CHECK・式索引・部分索引・ポリシー式）は、書き出し時に文字列リテラルの外にある内部 relation キー（`"#<id>.<name>"`）だけを `schema.name` へ戻す。選択したテナントの SCHEMA に解決できないキーを含む本文は `Unsupported`
- 関数本体はドル引用で囲み、タグは本体に現れない値を選ぶ

## 6. SEQUENCE の扱い

- 所有者のない SEQUENCE は `CREATE SEQUENCE`（START・INCREMENT・MINVALUE・MAXVALUE・CYCLE）で出す。`RESTART` 値が START と異なれば `ALTER SEQUENCE ... RESTART WITH` を足す
- 所有される SEQUENCE（SERIAL・IDENTITY）は表の列定義が作り直すので単独では出さない。SERIAL 系は列型の既定の定義と一致するときだけ `SERIAL` 等で、IDENTITY は SEQUENCE のオプションを明示した `GENERATED ... AS IDENTITY (...)` で出す
- 現在値は選択したテナントの状態行（`last_value`・`is_called`）から `SELECT setval(...)` を `PostData` に出す。状態行が無い SEQUENCE は出さない（まだ採番していない）

## 7. 「同一カタログ」の確かめ方

生成した DDL を空 DB で再実行し、再度書き出した DDL の列が元と一致することを確かめる（`tests/table_rls/logical_dump_schema.rs`）。`TableSchema` の等価比較は使わない。物理スロットの墓標（削除済み列）や読み出し時補完値（`ALTER COLUMN SET DEFAULT` で既定値と食い違うもの）は、データを再投入する論理ダンプでは意味を持たないため再現しない。

## 8. 出力順

出力順は SCHEMA の内部 ID に依存させない。内部 ID はカウンタで払い出され、復元先で付け直されるため、内部キーの並び（`#<id>.<名前>`）で出すと再生成した DDL 列が元と食い違う。表・VIEW（依存を満たす順のうち）・索引・関数・トリガ・ポリシー・行セキュリティは `(SCHEMA 名, 名前)`（共有分は SCHEMA 名が空で先）で並べ、SCHEMA とロールは名前順で出す。

## 9. 運用上の制約（#2162・#2163 への申し送り）

- 所有者の行（`ALTER ... OWNER TO` の相当）は、ロールを束縛したセッションが表・関数・SEQUENCE を作ると必ず記録される。現状は権限系として一括で `Unsupported` にするため、ロール束縛のセッションで作ったオブジェクトを持つ DB は書き出せない。`ALTER ... OWNER TO` は付与者の概念が無く SQL で表現できるので、権限系から切り離して後続で出す余地がある
- ブートストラップ権限（ユーザーファイル由来のロールの初期権限）の行があるテナントも同様に書き出せない
- ダンプにはロールの SCRAM 検証子（`CREATE ROLE ... PASSWORD 'SCRAM-SHA-256$...'`）が含まれる（平文は持たない）。出力ファイルの権限（`0o600` 相当）は CLI（#2162）が守る
- 復元は、ロールを作った後の文を束縛なしの運用者権限で実行できることが前提（現行の `CREATE TABLE`・`CREATE FUNCTION`・`CREATE POLICY` などは束縛なしで通る）

- 未分類の redb 表の検査（`ensure_all_tables_classified`）は、利用者の名前から作られる表名（`user_rows/<名前>` など）が `SCHEMA_CATALOG_TABLES` の完全一致名と衝突しないことを前提にする。利用者の表名は `validate_identifier` が `/` を含ませないため、接頭辞付きの動的表名は固定名と重ならない
- `DdlStatement.sql` には文字列リテラルとして `;` や改行を含みうる（既定値・ラベル・関数本体）。#2162 が `;` と改行で区切って書く場合、復元側は字句解析器を考慮した分割（`sql::statement_splitter` 相当）が必須で、単純な文字列分割は使えない
- `DdlStatement` にはロールの SCRAM 検証子が入るため、ログへ出さない（`Debug` は `sql` を伏せる）

## 10. 残課題

- 権限（GRANT・所有者・既定権限・メンバーシップ）の書き出し: 復元を現在ロールを束縛した主体で実行する設計（#2162・#2163）と、`GRANTED BY` 相当の構文の追加が前提
- 外部キー: 参照先の列名が `id` のものは SQL 表層が暗黙 id への参照として `0A000` で拒否するため、ダンプには現れない。それ以外の外部キーは SQL で作れ、SQL 経由の往復を第 13 節の試験が確かめる
- マテリアライズド VIEW、SCHEMA 配下の単独 SEQUENCE と現在値

## 11. 表データの書き出し（Issue #2161）

RECOVER-16 (1)(2)(3) のデータ側（ポインタ: `docs/spec/04-behavior/recovery.md`）。値のテキスト表現は WIRE-13 に揃える。本節は実装判断の記録で、spec 本文は転記しない。

### 11.1 API

```rust
Storage::export_table_data<E>(
    &self,
    scope: &SchemaDdlScope,
    format: TableDataFormat,            // Copy | Insert
    sink: impl FnMut(&TableDataItem<'_>) -> Result<(), E>,
) -> Result<TableDataSummary, SchemaDdlError<E>>

Storage::export_logical_dump<E>(scope, format, sink: impl FnMut(&LogicalDumpItem<'_>) -> ...)
```

- スコープ・`Summary`・`Error<E>`（`SchemaDdlError` を共用）は §1 と同じ形。`TableDataItem { tenant, part, text }` は 1 断片で、`part` は `CopyHeader`（`COPY <表> (<列>, ...) FROM STDIN`）・`CopyRow`（エスケープ済みの 1 行）・`CopyEnd`（`\.`）・`Insert`（1 文）。`text` に `;` と改行は含まない（書き手が足す）。`Debug` は本文を伏せる
- `export_logical_dump` は `PreData` → データ → `PostData` を読み取り Tx 1 本の上で渡す（`LogicalDumpItem::{Schema, Data}`）。`export_schema_ddl` と `export_table_data` を別々に呼ぶと取得点が 2 つになり、その間の commit で定義とデータが食い違いうるため、CLI（#2162）は `export_logical_dump` を使う
- 全テナントのダンプは §3 と同様、`shared_only`（行は出ない）に続けてテナントごとに `tenant_only` で呼ぶ。`tenant_only` でも共有の表の「自テナントの行」は出る（行はすべてテナント所有で、共有なのは定義だけのため）

### 11.2 走査とストリーミング

- 行ストア `user_rows/<表>` の物理キーは `(tenant_id, id)` で、指定テナントの行は 1 本の連続範囲になる。この範囲だけを redb のカーソルで読み、他テナントの行は読み出しもしない（構造的な絞り込み）。範囲内でも復号した行のテナントがキーと食い違えば失敗する
- 1 行を復号し、使い回す 1 本のバッファへ描画してシンクへ渡す。表全体・結果集合をためない（メモリは 1 行ぶんで、表の大きさに比例しない）。行数の上限は設けない（COPY TO 用の `--max-result-rows` の上限は通らない）
- 表の順は DDL 側と同じ `(SCHEMA 名, 名前)`。行は物理キー昇順（挿入順）。外部キー・索引は `PostData` なので表の順を依存順にする必要はない
- 行を 1 件も持たない表は何も出さない（COPY のヘッダも出さない）

### 11.3 形式（COPY と INSERT の使い分け）

- 既定は `Copy`（text 形式）。COPY が受理しない列型（`HALFVEC`・`INT8VEC`・`SPARSEVEC`）を持つ表だけ、その表を `INSERT` で出す（省かず、形式だけ落とす）
- `Insert` は 1 行 1 文。`GENERATED ALWAYS AS IDENTITY` の列を持つ表は、`INSERT` が `OVERRIDING` を受理せず明示値を書けないため `Unsupported`（COPY は明示値で書ける）
- COPY text のエスケープは本リポの COPY 復号器が受理する集合に限る（`\\`・タブ・LF・CR・`\b`・`\f`・`\v`）。NULL は `\N`。値が文字どおり `\N` の文字列は `\\N` になり NULL と混同しない。復号器が受理しない 8 進・16 進エスケープは使わない
- `INSERT` のリテラルは、整数・浮動小数・NUMERIC が裸、真偽値が `TRUE`／`FALSE`、NULL が `NULL`、それ以外は `'...'`（`'` を二重化。§5 と同じ `push_literal`）。本リポの `INSERT` は数値列に文字列を受理しない
- 識別子は §5 の `Namer`／`push_ident` を共用する。SCHEMA 配下の表は `schema.name` で書く

### 11.4 値の表現（WIRE-13 に揃える）

| 型 | 表現 |
| -- | ---- |
| ベクトル | `[1.5,-2,0.25]`（`f32` の最短表現）。`HALFVEC` は `f32` へ広げて同形、`INT8VEC` は整数、`SPARSEVEC` は `{idx:val,...}/N` |
| 配列 | `ArrayValue::to_pg_text`（`{a,"b c",NULL}`。引用・NULL 要素・多次元は同所の規則） |
| JSON／JSONB | 保存済みテキストをそのまま出す（再整形しない） |
| 浮動小数 | `format_real`／`format_double`（`REAL` は `f32` の最短表現で往復する） |
| 日付時刻 | `format_date`／`format_timestamp`／`format_time`／`format_interval`。`TIMESTAMPTZ` はセッションの TimeZone に依存させず UTC の壁時計と `+00` で出す（読み戻しは offset を解釈するので復元先のセッション設定に依存しない） |
| BYTEA | `\x` ＋小文字 16 進。COPY ではバックスラッシュが `\\x...` に二重化される |
| BIT・UUID・NUMERIC・ENUM | 既存の `format_text`／`Display`／ラベル文字列 |

- `NUL` 文字を含む値は COPY・文字列リテラルのどちらでも運べないため `Unsupported`
- `VECTOR` 列は行の embedding から取り、空は NULL（nullable な VECTOR 列の NULL の保存形）。`ALTER TABLE ADD COLUMN` 前に書いた行の欠落列は、行 codec の読み出し時補完と同じ値で出る
- 値の描画は `Value` の網羅 `match` で、ワイルドカード腕を置かない。型が増えるとコンパイルエラーで対応漏れを検出する

### 11.5 暗号化列の扱い（設計判断）

現行の列型に暗号化列は存在しない（`ColumnType` に該当 variant が無い）。判断は次のとおり。

- 現時点で書き出し対象の暗号化列は無いので、特別な出力経路は作らない
- 将来、暗号化列の型が足された場合は、`value_text` の網羅 `match` がコンパイルエラーで対応を迫る。その時点では **平文を出さない**（既定は `Unsupported` でダンプ全体を失敗させる）。暗号文をそのまま運ぶ場合も、復元先で同じ鍵で読める保証が要るため、鍵管理を含む設計を別途決めてから足す
- 理由: ダンプは平文のファイルとして残るため、暗号化の意図を黙って外す（fail-open）より、書き出せないと明示して止める方が安全

### 11.6 失敗させるもの（fail-closed）

| 対象 | 理由 |
| ---- | ---- |
| 暗黙 `id` の表（旧方式・移行済み方式） | DDL 側と同じ。SQL で再作成できない表の行だけを出さない |
| 可視性が `Private` 以外の行 | SQL の `INSERT`／`COPY` が書く行は常に `Private`。再投入で可視性を黙って変えない（SQL だけで作った表では起きない） |
| `GENERATED ALWAYS` 列を持つ表（`Insert` 形式のみ） | `INSERT` が明示値を書けない |
| `NUL` を含む値 | 運べない |
| 行の復号失敗・キーとテナントの不一致 | 破損として全体を失敗させる（`Catalog`） |

`Unsupported` の文言は固定文字列で、表名・テナント名・行の内容を含まない。

### 11.7 運用上の制約（#2162・#2163 への申し送り）

- 復元側は、断片の `tenant` の文脈で実行する（行の帰属を保って戻すため）。`COPY ... FROM STDIN` は `OPERATION_ID` が要る構成（`Ledgered`）では付けて実行するか、台帳なしの構成で復元する（論理ダンプは台帳を出力せず、復元側でも台帳を作らない）
- `COPY` のデータ行は NULL・タブ・改行を含むため、CLI は `CopyHeader` から `CopyEnd` までを 1 つの COPY として書き、行を再分割・再整形しない
- 途中で失敗したとき、それまでに渡した断片は不完全である。CLI（#2162）は一時ファイルへ書き、成功したときだけ公開する
- `INSERT` 形式でも、TEXT 中の LF・CR は文字列リテラルの中にそのまま出る。行単位で再整形・再分割しない
- 採番列（`SERIAL`・`IDENTITY`）の値は表データの明示値で戻り、次回の採番値は `PostData` の `setval` が戻す（§6）。SCHEMA 配下の SEQUENCE の現在値は §4 のとおり書き出せず、そのスキーマ配下に現在値を持つ表があるとダンプ全体が失敗する

### 11.8 残課題

- 一括 `INSERT`（複数行 `VALUES`）形式、COPY の CSV／バイナリ形式
- 可視性が `Public` の行を持つ表（SQL 経由では作れない）の扱い
- テナント一覧の取得 API（全テナントのダンプを CLI が組み立てるときに行を持つテナントを列挙する手段。現状は呼び出し側がテナントを知っている前提）

## 12. 運用者向け CLI（Issue #2162）

RECOVER-16 (1)(2)(3) の運用入口（ポインタ: `docs/spec/04-behavior/recovery.md`）。本節は実装判断の記録で、spec 本文は転記しない。実装は `crates/wire-server/src/dump_{opt,file,run,restore,tmp}.rs`、試験は `tests/wire_cli/dump_cli.rs`。

### 12.1 サブコマンドと終了コード

```text
wire-server dump --db <停止中の DB> --out <新規ファイル> (--tenant <id> | --shared-only)
                 [--schema-only | --data-only] [--format copy|insert]
wire-server dump-restore --from <ダンプ> --db <新規パス> [--expect-tenant <id>]
```

- 既存のサブコマンド（`backup`・`restore`・`verify`・`backup-incremental`）と名前が重ならない `dump`・`dump-restore` とした。引数の解釈は `*_opt.rs` の流儀（未知の引数・値の欠落・重複・排他違反・必須欠落を DB を開く前に終了コード 2 で拒否。エラー文は固定の英語でパス・テナント名を含まない）
- `dump` は `backup` と同じく**停止中**の DB を開く（稼働中は `backup_opt` と同じ「already open」判定で終了コード 1）。`--schema-only`／`--data-only` は排他、`--format` は `--schema-only` と併用できない
- 終了コード: 0=成功、1=実行時の失敗（DB を開けない・書き出せないオブジェクト・適用中の失敗）、2=引数／パスの拒否（復元先が既存を含む）、3=公開済みだが後始末（一時名の削除・親の fsync）を確かめられなかった、4=ダンプファイルの拒否（`dump-restore` のみ）、5=復元結果の整合検査の失敗（同）。標準出力は成功時の要約 1 行だけ。失敗の報告は標準エラーへ固定の英語で出し、DDL・行の値・オブジェクト名・テナント名は含めない（適用の失敗は枠の通し番号と SQLSTATE のみ）

### 12.2 テナント境界（全テナントのダンプは対象外）

- `--tenant <id>` は `SchemaDdlScope::shared_and_tenant`、`--shared-only` は `SchemaDdlScope::shared_only` にそのまま写す。どちらかの指定が必須で、**全テナントを暗黙に書き出す経路は無い**
- 全テナントのダンプは対象外: engine にテナントを列挙する API が無く（§11.8）、テナントごとに別々のスナップショットで書き出すと定義とデータの取得点が食い違うため。必要になったら engine 側でテナント列挙と単一スナップショットの複数テナント版を足してから CLI を広げる（別 Issue）
- 指定テナントの行だけが読み出され（§11.2）、他テナントの行・SCHEMA・ロールは出力に現れない。共有の定義（`public` の表・VIEW など）は設計上すべてのテナントに共通なので出る。テナント名は `is_header_safe_tenant`（印字可能な ASCII のみ＋engine の長さ検査）を満たすものだけを受理する
- **既知の制約（#2160 の §9 から継続）**: `--users` ファイルから起動したサーバーが作った DB では、初回起動時にそのテナントの利用者のブートストラップ権限が記録されるため、権限を書き出せない現状のダンプは `Unsupported`（終了コード 1）で拒否する。権限の書き出し（§10）が済むまで、CLI で書き出せるのは権限を持たないテナントの DB に限る。結合試験は engine を直接開いて DB を作り、復元結果の読み取りだけサーバー経由で確かめている

### 12.3 ファイル形式（版 2）

SQL スクリプトとして読める形で、先頭に版を含むヘッダを置く。

```text
-- fandhe-db logical dump v2
-- scope: tenant=<id>        （または shared）
-- parts: all                （schema | data）
-- format: copy              （insert）
-- restore-generation: <10 進整数>   （復元元の復元の世代番号。#2451）
-- body
-- @stmt <kind> <バイト数>
<本文>;
-- @copy <バイト数>
COPY <表> (<列>, ...) FROM STDIN;
<COPY text の行>
\.
-- end
```

- **世代行（#2451・RECOVER-17）**: `restore-generation` は必須の 1 行で、書き出し元 DB の復元の世代番号を運ぶ。10 進の数字だけ（符号・空白・先頭ゼロ・空は不可。`0` 単独は可）で、`bigint` の範囲に収まらない値は終了コード 4 で拒否する。**版 1 のファイルは拒否する**（終了コード 4）。版 1 は復元元の世代を持たず、受け入れると「復元先の世代が復元元より大きい」を保証できないため、世代 0 とみなして受け入れる案は採らない。旧ファイルは新しいバイナリで元の DB から取り直す
- 枠の見出しは `--` コメントなので、ファイル全体を `psql` 等へ流しても読める。COPY は `;` の後にデータ行と `\.` が続く標準の形で、`CopyHeader` から `CopyEnd` までを 1 つの COPY として書き、行は再分割・再整形しない（§11.7）
- **復元側は `;` や改行で分割しない**。`@stmt`／`@copy` の見出しの長さに従って本文を切り出す。本文は文字列リテラル・ドル引用・コメントの中に `;`・改行・`--`・`$$` を含みうる（§9）ので、字句を再解析せずに済む長さ付きの枠にした。切り出した本文は復元側が `engine::sql::statement_splitter::split_statements` で 1 文（`Single`）と判定したものだけを実行する。見出しの長さを書き換える改ざんは、1 文でなくなる・次の見出しが壊れる・終端行が無くなるのいずれかで拒否される
- COPY のデータ行だけは長さでなく行で区切る。COPY text 形式は行内の LF・CR・タブをエスケープする（§11.3）ので、生の LF は行境界だけである。終端行（`\.` の 1 行）は値としては出現しない（値 `\.` は `\\.` になる）
- 末尾の `-- end` が無いファイル（枠の境界での切り詰めを含む）は読み切れても拒否する。終端行の後ろに何かが続くファイルも拒否する
- 上限（書き手も同じ上限を課し、復元できないファイルを作らない）: ヘッダ 1 行 512 バイト、見出し 128 バイト、1 文の本文 64 MiB、COPY のデータ 1 行 64 MiB。確保は読み出したバイト数に比例させ、申告した長さだけで確保しない。NUL を含む本文・長さ 0 の本文・UTF-8 でない本文は拒否する
- `kind` は `DdlObjectKind` に 1 対 1（`insert` はデータの `INSERT`）。復元側は種別ごとの許可表（先頭の語句。`CREATE TABLE`・`INSERT INTO`・`SELECT setval(` など。語句の後ろには空白が続く形だけを受け付ける）に合う本文だけを実行する

### 12.4 書き出し

- 一時名（親ディレクトリの `.fandhe-dump-<pid>-<n>.tmp`）を `create_new`・0600 で作り、fd を保持して書く。成功したときだけ fsync して `hard_link` で `--out` へ公開し、一時名を消す（既存の `--out` は上書きしない）。公開前に保持 fd と (dev, ino)・長さを確かめ、食い違えば公開を取り消す。失敗時は一時名を消す（消せなければパスを通知して終了コードは失敗のまま）
- 出力にはロールの SCRAM 検証子が含まれうる（§9）。0600 は一時名の作成時から公開後まで保たれる。受け渡し先の権限は運用者が管理する
- 一時名を作る前に、停止中の DB（排他で開いている）から復元の世代番号を読んでヘッダに書く（`--shared-only`・`--schema-only`・`--data-only` でも同じ）。世代の値が壊れている・欠けている場合は終了コード 1 で、何も作らない（#2451）
- `--schema-only` は `export_schema_ddl`、`--data-only` は `export_table_data`、両方なし（全部）は `export_logical_dump`（単一の読み取り Tx）を呼ぶ。`--shared-only` と `--data-only` の併用は、共有の範囲に行が無いため拒否する

### 12.5 復元

- 復元先は**既定で新規パスだけ**を受け付ける（既存のパスは空の DB を含めて終了コード 2 で拒否。上書きのフラグは設けない）。`dump-restore` は復元先の親に一時名の新規 DB を作り、engine の通常の実行経路（`execute_sql_in_session`・`begin_copy`／`commit_copy_in`。第 2 の書き込み経路は作らない）で枠を順に流し込み、流し込み後に `verify_integrity` で整合を確かめ、通ったときだけ `hard_link` で公開する。**整合検査の後・公開の前に、ヘッダの世代を下限にして復元先の復元の世代番号を進める**（`Storage::advance_restore_generation_from_source`。RECOVER-17・#2451）。進められない場合（上限到達など）は終了コード 1 で何も公開しない。世代は DB 全体で 1 つの値でテナント情報を含まず、エラー文は固定の英語
- **ロールバック**: 途中のどの失敗（ファイルの拒否・文の実行失敗・整合検査の失敗）でも何も公開せず一時名を消すので、復元は全体として成功か不成功かのどちらかになる。公開前の一時名は復元先と別の名前なので、失敗した復元が復元先を占有しない
- **一時 DB の置き場（差し替え対策）**: `Storage::open` は DB をパスで開く（無ければ作る）ため、復元先の親に他者が書き込めると、検査から開くまでの間に一時名を既存 DB へのシンボリックリンクへ差し替えられ、復元 SQL が既存 DB に適用されうる。次の 2 段で塞ぐ
  - **復元先の親は、グループ・その他が書き込めないこと（sticky bit があっても例外にしない。#2167 のアーカイブ先と同じ基準）**。DB を開く・何かを作る前に、親を開いた fd の `fstat` で確かめ、共有ディレクトリは終了コード 2 で拒否する。競合の前提（他者が親の名前空間を変えられること）そのものを無くす。親の所有者は、そこへ作る作業ディレクトリの所有者（＝自分。`geteuid` を使わず、作成直後のディレクトリの uid と比べる）と一致することも求める（所有者は名前を差し替えられるため）
  - 親の中に **0700 の作業ディレクトリを `mkdir`（既存名は使わない）で新規に作り、その中に一時 DB を置く**。作成直後に開いた fd と実パスの (dev, ino)・所有者・権限・親の権限を照合する `check` を、DB を開く前・整合検査で開き直す前・公開前に行う。Linux では `/proc/self/fd/<n>` 経由で fd のディレクトリを指す。`/proc/self/fd` が使えない環境や Linux 以外では実パスで開くが、親の名前空間を変えられるのは親の所有者（自分）だけなので、確認から open までの窓は他者に悪用できない
  - 公開は従来どおり `hard_link` で、作業ディレクトリは復元先の親の中なので同じファイルシステムで完結する。成功・失敗とも作業ディレクトリと一時名を消す。消せなかった残骸は、`/proc` のパスでなく実パス（親 + 作業ディレクトリ名 + 一時名）を標準エラーに出す
  - 以前の「共有の親でも復元できる」という説明は取り下げた。共有ディレクトリ（`/tmp` 直下など）へは復元できない。専用ディレクトリ（0755 や 0700）を使うこと
- **COPY のバッチ分割**: 1 バッチの上限は engine の実際の上限（行数①・チャンク数④〔1 行 1 チャンク〕・合計バイト③）の最小値と 8 MiB で決め、次の行を足すと超える場合は足す前に commit して新しいバッチを始める。1 行だけで上限を超えるダンプは `feed` が拒否し、何も公開しない
- 実行の文脈: ヘッダのテナントの文脈（認証済みセッションと同じ可視性集合）で、文ごとに実行する。`--shared-only` のダンプは固定の内部テナント名で実行する（共有の定義は実行テナントに依存せず、行は無い）。DDL 実行権限はこの一時 DB 上のセッションにだけ与える。`COPY` は engine のバッチ上限（行数）に収まる単位に分けて commit する（一時 DB は公開されるまで誰にも見えないため、分割 commit でも原子性は変わらない）。台帳（`OPERATION_ID`）は使わず、復元後の DB にも作らない（§11.7）
- `--data-only` のダンプは、表が無い新規 DB へは復元できないため終了コード 4 で拒否する（既存 DB への流し込みは §12.7）
- **ダンプファイルは untrusted 入力**: ヘッダは完全一致、枠は上限と長さの検証、本文は 1 文の検証と、種別ごとの形（先頭の語句。`setval` は `SELECT setval('<名前>', <整数>, TRUE|FALSE)`、外部キーは `ADD CONSTRAINT ... FOREIGN KEY`、行セキュリティは `ENABLE`／`FORCE` だけ、COPY は `COPY <表> (<列>, ...) FROM STDIN` だけ）で絞る。
  - **枠の順序を強制する**: 作成系（`Pre`）→ 行の投入（`INSERT`・`COPY`）→ 後段（`Post`: 外部キー・索引・採番値・トリガ・行セキュリティ・ポリシー・権限）の順にしか並べられず、逆行は終了コード 4 で拒否する。トリガなどデータ投入中に効くものは後段でしか受け付けないので、投入中にダンプ由来のトリガ関数は走らない。engine の出力（トリガ・ポリシー・行セキュリティは `PostData`）と一致しており、変更なしで往復が成り立つ（`tests/wire_cli/dump_cli.rs`）。データの枠は DDL 実行権限を持たない別セッションで実行する
  - **ヘッダと枠の整合**: `scope: shared` のファイルにデータ枠、`parts: schema` にデータ枠、`format: insert` に COPY 枠があれば拒否する（`format: copy` でも COPY が受理しない列型の表は `INSERT` で出る〔§11.3〕ので `INSERT` 枠は許す）。ヘッダのテナントは信用される。取り違えを防ぐには `--expect-tenant <id>` を使う（一致しなければ終了コード 4。省略時は従来どおりヘッダのテナントで復元する）
  - **残るリスク**: それでもダンプファイルは書き込み SQL を運ぶ経路で、許可された種別の範囲で任意の `CREATE FUNCTION`・`INSERT`・`CREATE ROLE`（任意のパスワード検証子）・VIEW・CHECK 式・列の既定値の式などを新規 DB に作れる。作成系の段階では DDL 権限つきのセッションで実行され、列の既定値・CHECK 式・外部キーなど表に紐づく式はデータ投入中に評価される（関数を呼ぶ既定値でも、関数本体の SQL はデータ用の DDL 権限なしセッションの権限で走る）。復元結果は運用者が選んだ新規 DB に閉じる（既存の DB・ファイルには触れず、ファイルの読み書きに類する文は許可表に無い）が、**信頼できる出典のダンプだけを復元する**こと。ヘッダの世代を書き換えたダンプは復元先の世代を過大にしうる（上限に近づけて以後の復元を失敗させうる）が、ヘッダのテナント行と同じ信頼モデルの範囲として扱い、新たな緩和は設けない

### 12.6 試験（`tests/wire_cli/dump_cli.rs`）

枠の順序・`setval`／`ALTER TABLE` の形・ヘッダと枠の整合・`--expect-tenant` の拒否、往復（COPY・INSERT。特殊文字・NULL・多バイト文字・バッチ上限を超える行数。復元結果の再ダンプがバイト単位で一致）、全オブジェクト種別の往復、スキーマのみ・データのみ・共有のみ、テナント指定が境界を越えないこと（他テナントの行・値・SCHEMA 名が出力に無い、復元先から他テナントに何も見えない）、既存パスへの復元の拒否、途中で失敗したときの非公開と一時名の不在、改ざん（版・ヘッダ・長さ・種別・終端・追記・切り詰め・許可外の文・複数文）の拒否、出力の 0600、不正な引数。世代の進行（#2451）は `tests/wire_cli/dump_restore_generation_cli.rs`（ダンプ経由の単調増加、時計より先を進む復元元、ヘッダ改ざん・旧版の拒否、壊れた世代のダンプ、範囲指定のダンプの世代行）。往復の再ダンプは世代行以外がバイト一致し、世代は厳密に大きい。ファイル形式の単体試験は `dump_file.rs`（任意位置での切り詰めがすべて拒否されること含む）。

### 12.7 残課題

- 全テナントのダンプ（テナント列挙 API と単一スナップショットの複数テナント版が前提。§12.2）
- 権限（GRANT・所有者・メンバーシップ・ブートストラップ権限）の書き出し（§10）。`--users` 起動のサーバーが作った DB を CLI で書き出すには、これが前提になる
- 既存（空でない）DB へのデータのみの流し込み（`--into` 相当の opt-in）。新規パスだけを受け付ける現行の方針と原子性の方式（一時名＋公開）が合わないため、別途設計する
- ダンプの圧縮・暗号化、`INSERT` の複数行まとめ（§11.8）

## 13. 往復試験（Issue #2163）

定義とデータが書き出しと復元で完全に戻ることを、網羅 fixture 1 つで確かめる（RECOVER-16 ポインタ）。fixture の SQL は `crates/engine/src/test_util/dump_fixture.rs` に置き、engine と wire-server の結合テストが同じ定義を取り込む。

### 13.1 試験の置き場と判定

| 試験 | 場所 | 判定 |
| ---- | ---- | ---- |
| engine の API での往復（COPY・INSERT の両形式） | `crates/engine/tests/table_rls/logical_dump_roundtrip.rs` | `export_logical_dump` の出力を空 DB へ流し、再出力が元とバイト単位で一致すること。全表・VIEW の値が一致すること。往復後の最初の採番（SERIAL・IDENTITY・SEQUENCE）が元と同じ値になること。制約（主キー・UNIQUE・CHECK・外部キー）の違反が元と同じ `wire_code` で拒否されること |
| CLI での往復（COPY・INSERT の両形式） | `crates/wire-server/tests/wire_cli/dump_roundtrip_cli.rs` | `dump` → `dump-restore` → 再 `dump` がバイト一致し、全表・VIEW の値が一致すること |
| 壊れた入力の拒否 | 同上 | 同じ fixture のダンプについて、次のいずれも終了コード 1 または 4 で拒否し、何も公開せず一時名を残さないこと。(a) 各枠の境界と境界の 1 バイト手前での切り詰め (b) ヘッダの各行・枠の見出し・長さ・種別語・本文の終端・COPY の終端・終端行の 1 バイト反転 (c) 枠の入れ替え（索引・VIEW・外部キー・COPY・トリガ・ポリシーを表より前へ、後段をデータより前へ、トリガを関数より前へ） (d) 枠の重複（種別ごとの先頭の枠・COPY／INSERT のデータ・ヘッダ・ダンプ全体の連結）。`sequence-value`（`setval`）と `row-security`（`ENABLE`／`FORCE`）は再実行しても同じ状態になる文なので重複の試験から外している。拒否ごとに終了コード（枠・ヘッダ・終端・順序の違反は 4、文の実行失敗は 1）と標準エラーの識別語・SQLSTATE を 1 組に固定する |

### 13.2 fixture の網羅範囲

- 型: 全列型（文字列・整数・浮動小数・BOOLEAN・日付時刻・INTERVAL・BIT・BYTEA・JSON/JSONB・UUID・NUMERIC・ENUM・配列・VECTOR）の列を持つ表。値は特殊文字（タブ・改行・CR・バックスラッシュ・引用符・COPY の NULL 表記そのもの・制御文字・多バイト文字）・全列 NULL・境界値を含む
- 制約: PRIMARY KEY（単一・複合）、UNIQUE（単一・複合・名前付き）、CHECK、NOT NULL、DEFAULT（リテラル・関数呼び出し・`nextval`）、COLLATE
- 外部キー: 列制約・表制約・`ALTER TABLE ... ADD CONSTRAINT`、複合、自己参照、SCHEMA 配下の表どうし、`ON DELETE`／`ON UPDATE` の各動作、`MATCH FULL`、`DEFERRABLE INITIALLY DEFERRED`
- 索引: 通常・部分・式・複合 UNIQUE・HNSW・GIN、SCHEMA 配下の索引
- VIEW: 表を読むもの、VIEW を読む VIEW、列名指定つき、SCHEMA 配下
- 採番: SEQUENCE（開始値・増分つき）、SERIAL、`GENERATED BY DEFAULT AS IDENTITY`（開始値・増分つき）
- その他: ENUM、SCHEMA、SQL／PL/pgSQL 関数、トリガ、ポリシー（PERMISSIVE・RESTRICTIVE）、行セキュリティ（ENABLE・FORCE）、ロール（SCRAM 検証子）

### 13.3 fixture に含めない（書き出せない）要素と、それを固定する試験

| 要素 | 現状 | 固定する試験 |
| ---- | ---- | ------------ |
| 参照先の列名が `id` の外部キー | SQL 表層が `0A000` で拒否する（列 `id` を明示した表でも同じ）。ダンプには現れない | `foreign_key_to_a_column_named_id_is_rejected_so_it_never_reaches_the_dump` |
| 従来方式（暗黙 id）の表が持つ外部キー | 暗黙 id の表そのものが `Unsupported`（外部キーだけを落として戻すことはしない） | `dump_fails_closed_when_a_legacy_table_carries_a_foreign_key` |
| SCHEMA 配下で現在値を持つ SEQUENCE（SCHEMA 配下の表の SERIAL・IDENTITY を含む） | `Unsupported`（§4） | `logical_dump_schema.rs` の `schema_scoped_sequence_with_a_current_value_fails_closed`。fixture の SCHEMA 配下の表は採番列を持たない |
| `GENERATED ALWAYS` の IDENTITY | INSERT 形式では `Unsupported`、COPY 形式では往復する（§11.6） | `logical_dump_data.rs` の `identity_columns_round_trip_through_copy_and_fail_closed_for_insert`。fixture の IDENTITY は `BY DEFAULT` |
| 権限・メンバーシップ・所有者・マテリアライズド VIEW・暗黙 id の表 | `Unsupported`（§4） | `logical_dump_schema.rs` の既存試験（`role_membership_and_privileges_fail_closed`・`materialized_views_fail_the_whole_dump`・`table_with_implicit_id_row_identity_fails_the_whole_dump`） |

### 13.4 試験で見つかった不具合

- 外部キー（一意キー索引）を持つ表に行があると、逆引き索引の物理表（`key_index_rev/<表>/<名前>`）が「未分類の表」として扱われ、`export_schema_ddl`・`export_logical_dump` が全体を失敗させていた。分類の接頭辞に `key_index_rev/` を加えて直した（分類の単体試験にも追加）。
