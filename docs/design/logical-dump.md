# 論理ダンプのスキーマ側（DDL の書き出し）

- ステータス: 実装済み（Issue #2160）。データの書き出し（#2161）・CLI（#2162）・往復試験（#2163）は後続
- 対応: Issue #2160（親 #2159）。関連ビヘイビア: RECOVER-16 (1)。ポインタ: `docs/spec/04-behavior/recovery.md`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/catalog/ddl_export.rs` と配下（`quote.rs`・`table.rs`・`object.rs`・`access.rs`）、入口は `Storage::export_schema_ddl`・`EngineCore::export_schema_ddl`
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
- 外部キー: 現行の SQL 表層は採番方式の表への外部キーを受理しない（`0A000`）ため、書き出し側は文の組み立てを単体試験で確かめるにとどまり、SQL 経由の往復は未検証
- マテリアライズド VIEW、SCHEMA 配下の単独 SEQUENCE と現在値
