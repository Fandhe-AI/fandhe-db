# INSERT ... SELECT の parser と束縛（Issue #1732）

親 #1731・ビヘイビア SQL-51 のポインタ（spec 本文は転記しない。関連ポインタ: SQL-50・SQL-47・SQL-34・SQL-32・RLS-10・RECOVER-10/11）。

## 1. 範囲

本 Issue は **構文・内側 SELECT の検証・列数／型の束縛** までを担う。行の実体化・RLS・挿入・台帳は後続 Issue の担当で、
実行腕は束縛を通った形も `0A000` で拒否する（行・台帳・世代に触れない fail-closed）。

| 段 | 担当 | 場所 |
| -- | ---- | ---- |
| 構文（問い合わせ span の切り出しと後続句） | #1732 | `sql::allowlist`（`parse_insert_any_shape_tokens_with_limit`） |
| 内側 SELECT の検証 | #1732（既存の SELECT 検証器を再利用） | `core.rs::parse_insert_select` |
| 列数・型の束縛 | #1732 | `sql::insert_select::bind` |
| 内側 SELECT への RLS・実行・挿入 | #1733 | 未実装（実行腕は `0A000`） |
| 台帳の内容照合 | #1734 | 未実装 |

## 2. 構文

```text
INSERT INTO <表> (<列>[, ...]) <query> [RETURNING ...] [USING OPERATION_ID '<id>'] [;]
```

- `<query>` の先頭は `SELECT`、`(` の直後が `SELECT`、`WITH` のいずれか。それ以外（`VALUES` を含む）は従来どおり
- 問い合わせの範囲は、括弧の深さ 0 で最初に現れる次のいずれかの手前まで
  - `ON CONFLICT`（2 トークン組）・`RETURNING`・`USING OPERATION_ID`（2 トークン組）・`;`・文末
  - JOIN の `ON`・`USING (...)`・`USING PLAN(...)`・`USING MODE` を誤って区切りにしないため、`ON`／`USING` は 2 トークン組で判定する
- 範囲はトークン列のまま既存の検証器へ渡し、SQL 文字列を組み立て直さない（インジェクションを構造的に避ける）
- `ON CONFLICT` との併用は `0A000`（別 Issue）。空の問い合わせ・列リスト省略は `42601`（列リスト省略は #1730 の担当）
- 既存の公開入口（`validate_insert` 等）は内側 SELECT を検証できないため、SELECT 形を受けると `0A000`

## 3. 内側 SELECT の検証

- 検証は SELECT 経路と同じ `validate_sql_tokens_with_subquery_ctx_limits`（一時カタログ・ビューの解決込み）
- 受理する文種は読み取りのみ。`EXPLAIN`・`SET`・`SHOW`・`CREATE FUNCTION` は `42601`、`USING PLAN`（LLM I/O を起こす）は `0A000`
- 挿入先がビューなら `0A000`、表でもビューでもなければ既存の書き込み文と同じ `42P01`／`42809`
- 一時表が関与する文は `is_routable` が偽のため、既存の `0A000` 経路に落ちる
- 拡張クエリで `$n` を含む形は Parse 段で `0A000`

## 4. 束縛の判定順

1. 列リストの解決: 重複（VALUES 形と共有のヘルパー）→ 未知列（`42703`）→ `GENERATED ALWAYS`（`428C9`）
2. 疑似列 `id`: 従来方式は列リストに必須、移行済みの表は省略可（採番は書き込み段）
3. 列数: 内側の出力列と列リストが食い違えば `42601`
4. 型: 出力列の静的型を挿入先へ代入できなければ `42804`。型を持たない式（`Computed { ty: None }`）も `42804`。
   規則表の外にある `VECTOR`・`ENUM` は同じ値ドメインのときだけ受理する
5. `RETURNING` の投影を既存の束縛で検証する

内側の結果列メタは既存の Describe で導出する（列の導出を第 2 の実装にしない）。挿入先スキーマと内側の
スナップショットの一致は本 Issue では要求せず、実行時の再検証は後続 Issue が write txn 内で行う。
エラー文言は英語の固定文言とスキーマ由来の列名だけで、値・他テナントの情報を含めない。

## 5. 後続への引き継ぎ

- #1733: `BoundInsertSelectTarget`（`slots`・`id_omitted`・`omitted`）を実行側で再利用し、内側 SELECT の RLS 適用と挿入を実装する
- #1734: `ValidatedInsertSelect::query_tokens`（送られた内側 SELECT の span）を内容照合ハッシュの材料にする
- #1730: 列リスト省略（`INSERT INTO t SELECT ...`）と `DEFAULT VALUES`
- SQL-34 / #2212: 内側 SELECT の LIMIT 省略の扱い

## 6. 対象外（fail-closed）

`ON CONFLICT` との併用・列リスト省略・実行・content_hash・ビューへの挿入・一時表が関与する文・拡張クエリの `$n`・`OVERRIDING`・
`PARTITIONED` との併用（構文上受理しない）。
