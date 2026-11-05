# 条件付き更新の同一応答（存在秘匿）を既存 ErrorClass で表す方式

- ステータス: Accepted（応答方式の記録。実装済み: #2098）
- 対応 Issue: #2097（親 #2096、後続 #2098）
- ビヘイビア ID ポインタ: NOSQL-18（関連: RLS-9・RLS-10・RECOVER-11・NOSQL-12・SQL-19・SQL-32・ERR-2・ERR-4）。
  契約本文は spec を正とし、本書へは転記しない
- 関連コード: `crates/engine/src/error_format.rs`（`ErrorClass`）・`crates/wire-server/src/http/status.rs`（`http_status`）・
  `crates/wire-server/src/http/query/filter.rs`（`bind_filter_where_predicates`）・
  `crates/wire-server/src/http/query/dml_target.rs`（`DmlTargetError`）・
  `crates/engine/src/tenant.rs`（`update_rows_where_unchecked`／`delete_rows_where_unchecked`）
- 関連 doc: [nosql-update-delete-mapping.md](nosql-update-delete-mapping.md)・[update-single-row.md](update-single-row.md)・
  [predicate-dml-exec.md](predicate-dml-exec.md)・[partitioned-dml.md](partitioned-dml.md)・
  [idempotent-resend-comparison.md](idempotent-resend-comparison.md)・
  [error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)・[rls-timing-bench.md](rls-timing-bench.md)

## 背景

NoSQL の `update`／`delete` に期待値の条件 `if` を足し、一致した行だけを更新・削除する（楽観的並行制御。#2096）。
このとき「値が違う」「行が無い」「他テナントの行」「RLS で見えない行」をどう応答するかを先に決めておく。
区別できる応答を返すと、`if` が存在確認の手段（存在オラクル）になるためである。#2098 の実装は本書の決定を前提にする。

## 決定 D1: 新しい ErrorClass を設けない

- 条件の不一致は「エラー」ではなく「対象行 0 件の成功」として表す。応答本文は既存の
  `{"updated":0,...}`／`{"deleted":0,...}`（[nosql-update-delete-mapping.md](nosql-update-delete-mapping.md) の成功応答）と同じバイト列にする
- `ErrorClass` の一覧・`ErrorClass::ALL`・`http_status` の網羅 `match` は変えない。
  `status.rs` の「新しい `wire_code` を追加しない」方針とも一致する
- 採らなかった表現（いずれも存在の秘匿が崩れる、または既存分類の意味を壊す）

| 案 | 採らない理由 |
| -- | ------------ |
| `RowNotFound`（`P0002`・HTTP 404） | 不存在と不一致を区別してしまう |
| 条件不一致専用の新分類（HTTP 412・409 相当） | 「行は在るが値が違う」ことが分かる。区別できる時点で秘匿にならない |
| `SerializationFailure`（`40001`）・`CheckViolation`（`23514`）・`WithCheckOptionViolation`（`44000`） | 意味が違う。`40001` は再試行を促す誤った合図にもなる |

## 場合ごとの応答

| 場合 | 応答 | ErrorClass |
| ---- | ---- | ---------- |
| `if` が一致し、更新・削除した | 200・`n` = 変更した行数 | なし |
| 自テナントの可視行だが `if` が不一致 | 200・`n=0` | なし |
| 行が無い | 200・`n=0` | なし |
| 他テナントの行 | 200・`n=0` | なし |
| 自テナントだが RLS で不可視の行 | 200・`n=0` | なし |
| `if` の形が不正、語彙・型・列の拒否 | `filter` と同じ分類 | 既存の分類をそのまま使う |
| `if` と `mode: partitioned` の併用 | 拒否 | `UnsupportedSqlSyntax`（`42601`） |
| 同じ `operation_id` で同じ内容を再送 | 拒否 | `DuplicateOperationId`（`23505`） |
| 同じ `operation_id` で `if` を変えて再送 | 拒否 | `OperationIdContentMismatch`（`22023`） |
| 影響行数の上限を超えた | 拒否 | `PayloadTooLarge`（`54000`）。従来どおり |

- 「`filter` と同じ分類」の現時点の写しは、型不一致 `42601`・未知列 `42703`・非対応列 `22000`、形・語彙の不正 `42601`。
  単一真実源は `filter.rs` の `bind_filter_where_predicates` で、本表はその時点の写しである
- 空の `if: []` は `42601`（`filter: []` と同じ。#2098 で確定。engine を呼ばず副作用ゼロ）

## 述語形・単一行形との併用

- `if` は対象行を選ぶ条件に AND で加え、SQL 表層の述語形 DML（SQL-19）と同じ実行器・同じ述語評価器で評価する。
  第 2 の実行器は作らない。`filter` と同じ束縛経路を通り、SQL 文字列は組み立てない
- 評価の順序: テナント所有かつ RLS 可視の行を先に絞り、その行にだけ `if` を評価する
  （[predicate-dml-exec.md](predicate-dml-exec.md) §3・§4）。不可視行・他テナントの行に `if` は評価されないため、
  評価エラーを使って他テナントの値を探ることはできない
- 判定と更新は同じ write トランザクションで行う。単一ライタなので、判定から更新までの間に別の書き込みは入らない
- 分割実行（`mode: partitioned`）との併用は `42601`。判定と変更を 1 トランザクションで行えないため

## 台帳（`operation_id`）との相互作用

- [update-single-row.md](update-single-row.md) の判断 B により、0 行の更新・削除でも台帳への記録と commit は必ず行う
- そのため、`operation_id` を付けた条件付き更新が不一致で `n=0` になった後に、行を読み直して**同じ** `operation_id` で
  再試行すると、`if` が同じなら `23505`、`if` を変えていれば `22023` になる
- クライアントへの指針: 試行のたびに新しい `operation_id` を発行する。ES の 409 や HTTP の 412 に慣れた利用者には
  予想しにくい点なので明記する
- `if` は述語（`WherePredicate`）に AND で入るため、内容照合ハッシュ（`for_update_where`／`for_delete_where`）の
  対象に自然に含まれる（RECOVER-11）

## レイテンシの区別不能性

主張は構造上の根拠に限る。**実測はしていない**。

- 述語形: 候補の列挙は、一致するかに関係なく「所有 ∩ 可視」の行をすべて走査する。他テナントのキー範囲には触れない
- 0 行でも台帳の記録と commit（fsync）を行う。不一致・不存在で早く終わる経路は無い
- 単一行形: 他テナントの行は、物理キー `(自テナント, id)` の探索が外れるだけで、他テナントのデータ量や値に依存しない。
  差が出るとすれば「自テナントの行が在って不一致」と「自テナントの行が無い」の差（1 行のデコードと評価）だけで、
  どちらも自テナントの範囲に閉じる
- 計測は [rls-timing-bench.md](rls-timing-bench.md) のハーネスが担う。`if` を計測に含めるかは本書の範囲外

## 他の方式との比較

「同等以上」は書かない。`if` は未実装なので方式の差として並べ、優劣は付けない。
他社の記述は参照節の公式ドキュメントの内容に限り、本作業で再取得して確認できなかった細部は「公式ドキュメントで確認できず」とする。

| 比較軸 | Elasticsearch | PostgREST | HTTP `If-Match` | 楽観ロック（JPA `@Version`・Rails `lock_version`） | fandhe |
| ------ | ------------- | --------- | --------------- | ------------------------------------------------ | ------ |
| 条件の書き方 | `if_seq_no`／`if_primary_term` | 専用キーなし。PATCH／DELETE の水平フィルタで代用 | `If-Match` ヘッダ（ETag） | バージョン列を `WHERE` に含める | 本文の `if`（述語） |
| 不一致時 | 409（`version_conflict_engine_exception`） | 条件に合う行が 0 件の扱い（成功系の空応答） | 412 | 更新件数 0 から例外（`OptimisticLockException`／`StaleObjectError`） | 200・`n=0` |
| 不存在時 | 404 系 | 同上（0 件） | 資源が無ければ 412 または 404 は実装次第 | 更新件数 0（不一致と区別できない） | 200・`n=0` |
| 不一致と不存在を区別できるか | できる | できない（単一オブジェクトを要求した場合は 406 で失敗） | 実装次第 | できない | できない（意図した選択） |
| 削除にも使えるか | 使える | 使える | 使える | 実装による | 使える（#2098） |
| 判定と更新の原子性 | 同一操作内 | 単一文 | サーバー実装次第 | 単一 UPDATE 文 | 同一 write トランザクション |

- 楽観ロックは更新件数 0 からクライアントが判断するため、fandhe の件数 0 と性質が近い
- fandhe がどの場合も 200・`n=0` なのは、存在の秘匿のために意図して選んだ。代償として、クライアントは不一致の原因
  （競合か不存在か）を応答から知れない。必要なら行を読み直して判断する

## 範囲外・申し送り

- `if` の実装・テスト（スキーマ・`filter.rs` への結線、応答バイト列の同一性テスト）は #2098 で完了
- バージョン列の加算（`SET v = v + 1`）は NoSQL の `set` が値だけを受け付けるため、SQL 表層で行う
- `if` を含めたレイテンシの実測は本書の範囲外

## 参照

確認日 2026-11-05（作業日）。本作業では各ページを再取得しておらず、細部は上記の注記に従う。

- Elasticsearch: Optimistic concurrency control（`https://www.elastic.co/guide/en/elasticsearch/reference/current/optimistic-concurrency-control.html`）
- PostgREST: Tables and Views の更新・削除（`https://docs.postgrest.org/en/stable/references/api/tables_views.html`）
- RFC 9110 If-Match（`https://www.rfc-editor.org/rfc/rfc9110#field.if-match`）
- Jakarta Persistence `@Version`・Rails `ActiveRecord::Locking::Optimistic`
