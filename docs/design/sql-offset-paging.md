# ADR: 広域取得・`GROUP BY` 集計の `LIMIT` へ `OFFSET` を追加する（Issue #916）

- ステータス: Implemented（spec 側ビヘイビア ID は SQL-25 (b)・TASK-209 として
  付与済み〔`docs/spec/05-tasks.md`〕。ID 全体〔SQL-25〕は本 ADR 執筆時点で
  「検討中」——(a) スカラー `ORDER BY`・(c) `DISTINCT`・(d) `GROUP BY` 複数列は
  別 Issue（#915・#917・#918）の管轄で未実装のまま。本 ADR は (b) `OFFSET` の
  部分実装のみを対象とする。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 対応: Issue #916・spec 側 SQL-25 (b)・TASK-209
- 関連ポインタ: SQL-15・SQL-25 (b)・TASK-209・WIRE-15（カーソル）・RLS-7・RLS-8
  （全読み取り経路への RLS 一般化）・NOSQL-15・TASK-224（NoSQL 表層の `offset`
  写像。Issue #947、本 ADR の対象外）
- 検証コード: `crates/engine/src/sql/allowlist.rs`（`OFFSET` の構文受理・拒否の
  単体テスト）・`crates/engine/src/sql/parser.rs`（`validate_search_offset`
  単体テスト）・`crates/engine/src/sql/scan.rs`（`execute_scan_with_budget` の
  スキップ処理の単体テスト。可視行のみを対象とした計数）・
  `crates/engine/src/sql/group_by.rs`（ソート後・`LIMIT` 適用前の `drain` の
  単体テスト）・`crates/engine/tests/sql25_offset.rs`（ページング一貫性・RLS
  非漏えい・範囲検証・`GROUP BY`・カーソル・ビュー経由の結合テスト）・
  `crates/wire-server/tests/wire_scan_offset.rs`（層 A）

## 背景

広域取得（SQL-15。`docs/design/wide-retrieval-scan.md`）は `LIMIT n` を受理する
一方、`OFFSET` は許可リスト（`sql/allowlist.rs::parse_select_shape`）が構造上
拒否していた（`42601`）。結果の先頭からしか取得できず、ページングができない。

## 設計方針

### 受理範囲

| 形 | 扱い |
| -- | ---- |
| 広域取得 `SELECT <投影> FROM t [WHERE ...] LIMIT n OFFSET m` | 受理 |
| 集計 `SELECT ... GROUP BY c [HAVING ...] [ORDER BY ...] LIMIT n OFFSET m` | 受理（グループ単位でスキップ） |
| 検索 SELECT（`ORDER BY <距離>`／`HYBRID`／`USING PLAN`。SQL-1〜4） | 引き続き `42601`（本 ADR の対象外。spec 上の改訂注記なし） |
| 単一行集計（`GROUP BY` なし） | 従来どおり `42601`（`LIMIT` 自体を受理しない形） |
| `LIMIT` を伴わない `OFFSET` 単独・逆順（`OFFSET m LIMIT n`）・`OFFSET m ROWS`・`FETCH FIRST ...`・負値・小数 | `42601` |
| `OFFSET` の後の余剰トークン | `42601` |
| 拡張クエリの `OFFSET $n` | `LIMIT $n` と同じく拒否（`sql/params.rs` の位置検証。本 ADR での変更なし） |

`OFFSET` は字句解析段階のキーワード（`lexer::Keyword`）に追加しない。`USING`・
`SET` と同じ理由（既存の列名・テーブル名としての `offset` を壊さないため）で、
`LIMIT` の数値直後という文脈限定で識別子として判定する
（`allowlist::Parser::parse_optional_offset`）。

### 値検証

`LIMIT` と同じ 2 段構え（構文段は `u32` パースのみ、束縛段で範囲検証）を踏襲する。
束縛段の新設 `sql::parser::validate_search_offset` は `0..=MAX_SEARCH_K`
（実装既定 10,000。`LIMIT` と同一の上限）を受理し、範囲外は `22000`。`LIMIT` と
異なり `0`（no-op）を受理する。`GROUP BY` の `LIMIT` は別軸の上限
（`group_by::MAX_GROUPS`）で検証するが、`OFFSET` は `MAX_SEARCH_K` を用いる
——`OFFSET` が制限するのは「可視かつ `WHERE` 一致の行数」であり、`MAX_GROUPS`
（グループ数上限）とは意味論的に別の量であるため。

`validate_search_offset` は `pub` とし、NoSQL 表層の `offset` 写像
（TASK-224・NOSQL-15、Issue #947）から再利用できるようにした（第 2 の実装を
作らない方針。`validate_search_limit` と同じ判断）。

### 実行段

- **広域取得**（`sql::scan::execute_scan_with_budget`）: 可視性再適用（2 回目の
  `ctx.is_visible`）・`WHERE` 評価（`metadata_filters`／`expr_filters`）が確定した
  後にのみ読み飛ばし数を加算する。投影・`cells` 確保・byte 予算計上より前に
  読み飛ばすため、スキップされた行はメモリを消費しない（深いページングでも
  結果セットは O(`limit`) のまま）。
- **`GROUP BY` 集計**（`sql::group_by::execute_grouped_aggregate`）: ソート確定後・
  `truncate(limit)` の前に `finished.drain(..offset.min(finished.len()))` を適用する
  （添字アクセスを使わない）。グループは可視行のみから構成済みのため RLS 契約は
  不変。
- **`DECLARE CURSOR`**: `bind_scan`／`bind_group_by_clause` をそのまま経由するため
  追加実装なしに `OFFSET` が効く。

### 公開 API 互換

`BoundScan::new`（wire-server・engine 結合テストが使用）・
`BoundAggregate::new_grouped` のシグネチャは変更しない。`BoundScan` に
`offset: usize`（既定 0）・`offset()`・`with_offset(usize)` を追加し、`BoundGroupBy`
（`pub(crate)`）にも `offset: usize` を追加した。

## RLS・不可視行の非漏えい（RLS-7・RLS-8）

`OFFSET` の計数は「可視かつ `WHERE` 一致」の行のみを対象にする。不可視行は
デコード前・再適用いずれの `ctx.is_visible` 判定でも除外されており、読み飛ばし数
にも結果にも一切現れない。したがって、ページ境界の出方（先頭ページの内容・
空ページになる `OFFSET` の値）から他テナント行の存在・件数を推測できない
（`crates/engine/src/sql/scan.rs::tests::offset_counts_only_visible_rows_not_other_tenant_rows`・
`crates/engine/tests/sql25_offset.rs::offset_does_not_count_or_leak_other_tenant_rows`
で固定）。

## `ORDER BY` なし `OFFSET` の意味論

広域取得は順序保証を持たない（`docs/design/wide-retrieval-scan.md`「順序」節）。
`OFFSET` はこの前提の上に乗る——順序は同一スナップショット内の物理走査順
（`(tenant_id, id)` 昇順）で決定的だが、意味的な順序（値による並び）ではない。
別の文でページを取得する間に書き込み（挿入・削除）があれば、他の RDBMS の
`OFFSET`（例: PostgreSQL）と同様に行の重複・欠落が起こりうる。安定したページング
が必要な場合は明示トランザクション内のカーソル（`DECLARE`/`FETCH`。WIRE-15）を
推奨する。スカラー `ORDER BY`（SQL-25 (a)、Issue #915）実装後は「ソート確定後に
`OFFSET` を適用する」契約（本 ADR の `GROUP BY` 経路と同型）を広域取得側にも
適用する必要があり、#915 の統合事項として申し送る。

## 方式比較: `OFFSET` とカーソル・keyset

| 方式 | 特性 |
| --- | --- |
| `OFFSET`（本 ADR） | ステートレス（サーバー側に状態を持たない）・実装が単純・深いページングは走査コストが線形に増える |
| カーソル（`DECLARE`/`FETCH`。WIRE-15、既存） | `DECLARE` 時に一度だけ実行して確定するため書き込み挟み込みの影響を受けない。ただしトランザクション・セッション byte 予算（`MAX_CURSOR_BYTES_PER_SESSION`）に縛られる |
| keyset（`WHERE id > <last>`） | 広域取得の物理走査順はテナント主順で `id` 単調ではないため、スカラー `ORDER BY`（`ORDER BY id` 前提。SQL-25 (a) 実装後）でのみ正しく機能する。現時点では一般には適用できない |

本 Issue では `OFFSET` を採用し、安定性が必要な場合はカーソルを既存経路として
併用可能とした。ステートレスな任意ページアクセス（`OFFSET`）と、書き込み挟み
込みに強い順次アクセス（カーソル）は互いに補完する用途であり、一方を他方の
代替として強制しない。

## 深いページングのコスト特性

広域取得は「可視かつ `WHERE` 一致の `m + n` 件目」まで走査し、その間の不可視行・
`WHERE` 不一致行もすべて読む。時間計算量は概ね O(`m + n` + 走査中に読み飛ばした
不可視・不一致行数)、`WHERE` 評価回数は最大 `m + n` 回。結果セットの
メモリ・byte 予算は O(`n`)（スキップ行は投影・確保しない）。`m` は
`MAX_SEARCH_K` で上限化しているため、最悪でも `m + n` は `2 * MAX_SEARCH_K`
（20,000）件の一致行を上限とする。`GROUP BY` 集計は全グループを構築・ソート
した後にスキップするため、`OFFSET` によるコスト削減はなく、既存の
O(可視行数 + `G log G`)（`G` はグループ数）のまま変わらない。

## エラー写像

- 構文外（受理範囲外の形）: `42601`
- 範囲外（`0..=MAX_SEARCH_K` を超える値）: `22000`（ERR-2）
- 新規の SQLSTATE は追加しない

## スコープ外（Issue 起票はせず記録のみ）

- 検索 SELECT（SQL-1〜4）への `OFFSET`: spec 上の改訂注記が無く `42601` を維持。
- NoSQL `scan`/`aggregate` の `offset`: #947（NOSQL-15・TASK-224）。本 Issue は
  `validate_search_offset`・`BoundScan::with_offset` を再利用可能な形で用意する
  のみ。
- スカラー `ORDER BY` との組み合わせ（ソート後スキップの統合）: #915 の統合事項
  （上記「`ORDER BY` なし `OFFSET` の意味論」節参照）。
