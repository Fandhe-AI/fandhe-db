# ADR: TRIGGER の実行モデル（トリガ関数・NEW／OLD・WHEN・再帰上限・台帳整合）（Issue #1721）

- ステータス: Accepted（設計記録。実装は後続 #1722〜#1728 で行う）
- 対応 Issue: #1721（親 #1720。後続 #1722・#1723・#1724・#1725・#1726・#1727・#1728）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-45。関連: TABLE-24・TABLE-37・
  TABLE-43）・SQL-32・SQL-46・SQL-66・SQL-67・SQL-68・RECOVER-1・RECOVER-11・RLS-10。
  spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 前提: #2222 配下の PL/pgSQL（#2223 の設計 ADR は PR #2776 で `plpgsql-subset.md` として
  追加予定。未マージの間は D 番号が移動していないか実装時に読み直す）・#2228
  （`RETURNS trigger` 関数と内部の呼び出し口）・PR #2775（UDF カタログ）
- 関連 ADR: `plpgsql-subset.md`（PR #2776 で追加予定）・
  [explicit-transaction.md](explicit-transaction.md)・
  [operation-id-ledger.md](operation-id-ledger.md)・
  [multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)・
  [data-modifying-cte.md](data-modifying-cte.md)・
  [sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md)・
  [foreign-key.md](foreign-key.md)・[partitioned-dml.md](partitioned-dml.md)・
  [statement-interrupt.md](statement-interrupt.md)・
  [nesting-limits-cli.md](nesting-limits-cli.md)・[recursive-cte.md](recursive-cte.md)・
  [create-view.md](create-view.md)・[temp-table.md](temp-table.md)・
  [error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)・
  [schema-tenant-namespace.md](schema-tenant-namespace.md)
- 出典: 発火の意味は PostgreSQL 17 公式ドキュメント（`CREATE TRIGGER`・トリガの動作の
  概要・PL/pgSQL のトリガ関数）、fandhe 固有の制約は public な Issue 本文（#1720・#1725〜
  #1728）と既存コード・既存 ADR の不変条件に限る。上記以外の判断は「本 ADR の決定」として
  理由を添える
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

## 背景

- 現状 TRIGGER は無い。UDF は宣言的な式だけ（`crates/engine/src/sql/udf_call.rs`）で、
  手続き言語は #2222 で追加中
- #2223 の ADR は関数側の契約（`TriggerCall`／`TriggerOutcome` の内部 API）だけを定め、
  発火の規則・`WHEN`・発火順序・再帰上限・台帳整合は本 ADR に委ねている
- 本書はトリガ本体を `RETURNS trigger` の PL/pgSQL 関数として `EXECUTE FUNCTION` で結線する
  前提で、発火側の実行モデルを確定し、#1722〜#1728 の分解の前提を列挙する

## 公開コードの現状（設計の前提）

| 事実 | 所在 |
| ---- | ---- |
| 行を書き換える全経路は `tenant.rs` の `*_unchecked` 系を通る。制約の単一検査点は `enforce_row_constraints_in_txn`。結線の一覧は `foreign-key.md` の「書き込み経路への結線」 | `tenant.rs` |
| 台帳のキーは `(tenant_id, table_name, operation_id)`。行と同じ txn に記録し keep-first。再送は `23505`、内容の不一致は `22023` | `recovery/ledger.rs`・`recovery/content_hash.rs` |
| データ変更 CTE は「事前照合 → 副文 → 台帳記録」の順。副文は `SubstatementLedgerScope` で台帳モードを Optional に切り替え、台帳に触れない | `core/dm_cte_exec.rs` |
| 文内変更集合 `StatementWritesScope` が文の中で書いた `(表, id)` を追跡する | `tenant/statement_writes.rs` |
| 参照アクションは走査（読み取りハンドル）→ 適用（書き込みハンドル）→ 再帰の順に分け、`TableAlreadyOpen` を避ける | `constraint.rs` の `propagate_referential_actions` |
| 明示 Tx は共有の `WriteTransaction` を 1 本持ち、`mark_written`・`dirty_tables`（世代）で書いた表を追跡する | `sql/transaction.rs` |
| 上限は `*_limits.rs` と `*_opt.rs` の型（既定値＝ハード上限、下げる方向だけ、`EngineCore::with_*` で 1 回注入） | `recursive_cte_limits.rs`・`recursive_cte_limits_opt.rs` |
| 必要な SQLSTATE（`42P17`・`42883`・`42710`・`42809`・`2BP01`・`0A000`・`54001`・`54000`・`42804`・`23505`・`22023`・`23502`・`57014`）は既存の `ErrorClass` にある。`27000` だけが無い（D6） | `error_format.rs` |

## 決定

### D1. トリガ関数の結線と呼び出し口

- トリガ本体は #2228 の内部 API（`TriggerCall`／`TriggerOutcome`）だけで呼ぶ。発火側
  （新モジュール。配置案は `crates/engine/src/trigger/` の `fire`／`queue`／`guard`）は
  PL/pgSQL インタプリタを直接触らない
- `TriggerCall` に渡す値は NEW／OLD・`TG_OP`・`TG_WHEN`・`TG_LEVEL`・`TG_NAME`・
  `TG_TABLE_NAME`・`TG_TABLE_SCHEMA`・`TG_NARGS`・`TG_ARGV`（`CREATE TRIGGER` に書いた
  引数の文字列）。文トリガでは NEW／OLD を NULL にする（PG と同じ）

### D2. NEW／OLD の内容とテナント境界

- NEW／OLD に含めるのは利用者に見える列だけ。テナントのキー・物理 rowid・可視性ラベル
  （`storage::Visibility`）は含めず、書き換えた NEW を返してもこれらは変えられない。
  トリガ内の INSERT で作る行の可視性は、セッションの通常の INSERT と同じ規則で決める
- DEFAULT・IDENTITY・SERIAL の値は BEFORE ROW より前に確定し、NEW に入れて渡す（PG と同じ）
- PL/pgSQL の RECORD が保持できない列型を持つ表へのトリガ定義は `0A000`。この検査は
  `CREATE TRIGGER` の時点に加え、発火時にも再度行う（`ALTER TABLE` で列が増えた場合の
  fail-closed）

### D3. BEFORE ROW の戻り値

> Issue #1723 の実装記録: [trigger-before-insert.md](trigger-before-insert.md)（INSERT の BEFORE ROW 経路・
> 経路ごとのガード・PostgreSQL との差分）。

- 同じ行の BEFORE トリガを名前順に連鎖させ、各トリガが受け取る NEW は前のトリガが返した行
- NULL が返った場合、その行の操作を行わず後続のトリガを呼ばない。影響行数・`RETURNING`・
  AFTER のキューにも入れない
- 返した行が対象表の構造と合わなければ `42804`（#2223 ADR 参照）
- DELETE の BEFORE は、戻り値が NULL でなければ続行する（返した値は使わない。PG と同じ）
- 行の書き換えは BEFORE だけが行える。AFTER と文トリガの戻り値は無視する

### D4. 発火の時点と順序（1 文のライフサイクル）

> Issue #1723 の実装記録: [trigger-before-insert.md](trigger-before-insert.md)（INSERT の BEFORE ROW 経路・
> 経路ごとのガード・PostgreSQL との差分）。
>
> Issue #1724 の実装記録: [trigger-after-insert.md](trigger-after-insert.md)（INSERT の AFTER ROW 経路・
> 段 6 のキュー消化・トリガ内書き込みの窓口・遅延 FK）。

1. 台帳の事前照合（D10）。再送ならここで `23505` を返し、トリガは 1 つも呼ばない
2. BEFORE STATEMENT（対象 0 行でも発火する）
3. 対象の確定（D5）
4. 行ごとの処理: `WHEN` の評価と BEFORE ROW の呼び出し → その行の書き込みと行単位の
   制約検査（単一検査点を 1 行分呼ぶ）→ AFTER ROW のイベントをキューに積む
5. 文単位の制約処理（FOREIGN KEY の検査・参照アクション）
6. AFTER ROW のキューを、変更の順 → トリガ名の順に消化する
7. AFTER STATEMENT
8. 台帳の記録
9. commit（autocommit の場合）

- 同じ表・時点・イベント・粒度のトリガは、正規化した名前のバイト順で呼ぶ（PG の名前順。
  C 照合に相当）
- AFTER のキューは文ごとに持つ。トリガ内の文が作った AFTER イベントは、その内側の文の
  終わりで消化する（PG と同じ入れ子）

### D5. 行単位の処理と再入の不変条件

> Issue #1723 の実装記録: [trigger-before-insert.md](trigger-before-insert.md)（INSERT の BEFORE ROW 経路・
> 経路ごとのガード・PostgreSQL との差分）。

- 表に該当イベントのトリガがある文だけ「トリガ経路」で実行する。トリガ経路は行単位で、
  対象行を先に実体化（UPDATE／DELETE の対象、INSERT ... SELECT の入力行、VALUES の行と
  DEFAULT 適用後の値）し、行ごとに redb の表ハンドルを開いていない状態で BEFORE を呼び、
  その後でハンドルを開いて 1 行を書き込み、閉じる（#2223 ADR の再入の不変条件を満たす）
- 対象は D4 の段 3（BEFORE STATEMENT の実行後）に確定する。段 3 以降の行トリガ
  （BEFORE／AFTER ROW）が同じ表へ書いた行は外側の文の対象に入らない（PG の command id に
  よる可視性に相当）。行 1 の書き込みは行 2 の BEFORE から見える
- 除外の保証は行トリガに限る。BEFORE STATEMENT が同じ表へ書いた行は段 3 より前の書き込み
  なので、条件に合えば外側の UPDATE／DELETE の対象になる（PG と同じ。段 2 の書き込みは
  対象確定の時点で可視）。AFTER STATEMENT の書き込みは対象確定の後なので対象に入らない
- トリガが無い文は既存のバッチ経路のまま（性能回帰を避ける）
- 経路の選択は、文の開始時に同じ write txn 内で読んだカタログから決める。トリガの集合は
  文の間は固定する（PL/pgSQL の本体では DDL が範囲外のため文の途中で変わらない）

### D6. 自己変更の衝突

- 外側の文がこれから書き換える・削除する行を、同じ文のトリガが既に変更・削除していた場合は
  `27000`（triggered_data_change_violation。PG と同じ）を返し、文全体を取り消す
- 検出には `StatementWritesScope` を一般化した文内の変更集合を使う（データ変更 CTE 専用の
  スコープとの入れ子も扱う）
- `ErrorClass` に新 variant `TriggeredDataChangeViolation`（`27000`）を追加する。公開 enum の
  変更なので、実装する #1725 は `feat(engine)!:` と `BREAKING CHANGE:` で出す

### D7. `WHEN` と `UPDATE OF`

- 評価の時点（PG と同じ）: BEFORE ROW は呼び出し直前にその時点の NEW（前のトリガが書き換えた
  後）で評価。AFTER ROW は行の変更の時点で評価し、偽ならキューに積まない
- 式の受理範囲は CHECK 制約と同じ宣言的な式に限る。サブクエリは `0A000`、集計は `42803`、
  窓は `42P20`。ユーザー定義の PL/pgSQL 関数と揮発性関数は初版では `0A000`
- 定義時の参照検査は PG に合わせ `42P17`: INSERT トリガの `WHEN` で OLD を参照、DELETE
  トリガの `WHEN` で NEW を参照、文トリガの `WHEN` が列を参照
- `UPDATE OF <列>` は列が `SET` の対象として書かれているときだけ発火する。BEFORE トリガに
  よる書き換えは判定に含めない。UPSERT では `DO UPDATE SET` の列で判定する

### D8. 経路ごとの扱いと「未結線の経路」ガード

> Issue #1723 の実装記録: [trigger-before-insert.md](trigger-before-insert.md)（INSERT の BEFORE ROW 経路・
> 経路ごとのガード・PostgreSQL との差分）。
>
> Issue #1725 の実装記録: [trigger-update-delete.md](trigger-update-delete.md)（UPDATE／DELETE の行トリガ・
> `UPDATE OF`・分割実行と未結線経路のガード）。UPSERT の発火・`WHEN` の評価・D6（`27000`）は今回見送り、
> UPSERT は INSERT・UPDATE のどちらかのトリガがあれば `0A000` のまま。ビュー経由・データ変更 CTE の副文は
> 共有のフォーム関数を通るため INSERT と同様に発火する。
>
> Issue #1726 の実装記録: [trigger-statement-truncate.md](trigger-statement-truncate.md)（段 2・段 7 の文トリガ・
> 対象 0 行での発火・TRUNCATE トリガの順序と PostgreSQL との差分）。

- 中核の不変条件: **トリガが黙って発火しない経路を作らない**
- `tenant.rs` の `*_unchecked` 系の書き込み関数（`foreign-key.md` の結線一覧と同じ集合）は、
  発火側が発行した通過証（型）を持たない呼び出しを拒否する。対象表に該当イベントのトリガが
  ある場合、その呼び出しは `0A000`。判定はカタログだけで行い、テナントのデータ（他テナントの
  行の有無）に依存しない

| 経路 | 初版 |
| ---- | ---- |
| `INSERT ... VALUES`／`INSERT ... SELECT` | 結線（#1723・#1724） |
| 単一行・述語形の `UPDATE`／`DELETE`、`UPDATE ... FROM`／`DELETE ... USING`、サブクエリ形 | 結線（#1725） |
| UPSERT | 結線（#1725） |
| `TRUNCATE` | 文トリガだけ（#1726）。行トリガは定義時に `0A000`、DELETE トリガは発火しない（PG と同じ）。複数表・`CASCADE` では対象表ごとに発火 |
| 分割実行 DML（SQL-32 ポインタ） | 対象表に該当イベントのトリガが 1 つでもあれば（行・文を問わず）`0A000`。文の途中で commit するため文トリガも原子性を保てず、行トリガだけを拒否するより厳しい側に倒す |
| FK の参照アクションが、該当イベントのトリガを持つ子表に及ぶ形 | 文の開始時にカタログから静的に判定して `0A000`（PG は子表のトリガを発火させる。差分として記録） |
| `COPY ... FROM`・NoSQL 表層の書き込み・ビュー経由の書き込み・データ変更 CTE の副文 | 初版はガードにより `0A000`。結線は追跡候補 |
| `ALTER TABLE` による行の書き換え・CTAS・マテリアライズドビューの更新 | 発火しない（PG と同じ。ガード対象外と明示） |

- UPSERT の順序（PG と同じ）: INSERT の BEFORE ROW を呼んでから衝突を判定し、衝突すれば
  UPDATE の BEFORE／AFTER ROW、衝突しなければ INSERT の AFTER ROW を呼ぶ。文トリガの
  発火の細部は PG 17 の挙動と突き合わせて #1725 で確定する
- 定義対象の制限: ビューの行トリガとマテリアライズドビューへの定義は `42809`（PG と同じ）、
  ビューの文トリガは `0A000`（PG との差分）、INSTEAD OF・REFERENCING・CONSTRAINT TRIGGER・
  DEFERRABLE は `0A000`、一時表へのトリガは初版では `0A000`、NoSQL 表層からは定義できない

### D9. Tx・テナント文脈（RLS）

> Issue #1724 の実装記録: [trigger-after-insert.md](trigger-after-insert.md)（トリガ内の書き込みは
> `TriggerWriteHandle` が呼び出しテナントの `PolicyContext` を借用して行い、差し替えられない）。

- トリガ関数は、書き込みを発行したセッションの `PolicyContext`（テナント）で、同じ write txn
  の中で実行する。作成者の文脈へは切り替えない。`SECURITY DEFINER` は #2223 ADR のとおり
  `0A000`
- 共有の `public` 表のトリガは全テナントの書き込みで発火し、それぞれ書き込んだテナントの
  文脈で実行する
- トリガ内でエラーが起きた場合、autocommit では文全体を取り消し、txn 内の永続的な変更
  （行・索引・台帳）の副作用はゼロにする。明示 Tx では `Failed`／`25P02` になる。
  例外として、SEQUENCE のカウンタ状態・採番の欠番・セッション単位のカウンタ状態は取り消し
  対象に含めない（トリガ内の `nextval`（D10）は、エラー後もカウンタが進んだままになる。
  `sequence-namespace-and-ledger.md` §8.2 の ROLLBACK の扱いに従う）。エラー文言は固定の英語で、値・行・他テナントの存在を含めない
- トリガが書いた表は、明示 Tx の `mark_written` と世代の更新の対象にする（遅延 FK 検査が
  漏れないように）
- キャンセル: 行ごと・トリガ呼び出しごとに `interrupt::check()` を呼ぶ（`57014`）。確認点は
  台帳の記録・commit の後ろに置かない

### D10. 台帳・content_hash との整合

- 台帳のエントリは外側の文の対象表に 1 つだけ作る。トリガ内の文は台帳に記録しない
  （`SubstatementLedgerScope` を共通化したスコープの中で実行し、台帳モードは Optional。
  #2226 の host と共有）。本体内の `USING OPERATION_ID`・`PARTITIONED` は `0A000`
- 事前照合は BEFORE STATEMENT より前に行う（BEFORE トリガ自体が副作用を持つため）。台帳の
  記録はすべてのトリガの副作用の後・commit の前に同じ txn で行う。再送は `23505` を返し、
  文もトリガも再実行しない。クラッシュしても、文の書き込みとトリガの副作用は redb の単一
  commit でともに残るかともに残らない
- content_hash の材料は外側の要求（BEFORE で書き換える前の値・構文形）だけとする。トリガに
  よる NEW の書き換え・トリガ内の `nextval`・トリガの定義は含めない
  （`sequence-namespace-and-ledger.md` と同じ立場）。既知の性質として、再送までの間に
  トリガが置換・削除されても、記録済みなら `23505` のまま変わらない
- トリガ内の `nextval` が進めたカウンタ状態は、文が失敗・ROLLBACK しても戻らない
  （欠番は許容。行・索引・台帳の取り消しとは別扱いで、D9 のエラー時の例外と同じ）
- `--require-operation-id` 構成での `operation_id` 省略は、外側の文の段階で従来どおり `23502`

### D11. 再帰（入れ子）の上限

- トリガの入れ子の深さは専用のカウンタで数える。既定値＋起動時 CLI `--max-trigger-depth`
  （`engine::trigger_limits`／`wire-server::trigger_limits_opt`。下げる方向だけ）。超過時は
  `54001` で文全体を拒否する。既定値は**実装既定値であり spec 由来ではない**（案: 16。
  #1727 で確定）
- トリガ関数の呼び出しは、関数呼び出し深さ（#2223 ADR の `--max-function-call-depth`）にも
  1 として数える
- 文数・ループ回数の予算（#2223 ADR）は、外側の文の行が直接発火させた 1 回の呼び出しを
  「トップレベル」とみなし、その中の入れ子のトリガは予算に合算する。行ごとに予算を
  リセットするため、総仕事量は「外側の入力の行数 × 1 回分の予算」であり、**有限の上限が
  保証されるのは外側の入力に上限を設けた構成だけ**である。DML の対象行数は既定で上限なし
  （`multi-row-dml-operation-id.md`「上限 API の統合」）のため、既定構成では処理量が対象行数に
  比例して増える。その制御は行ごとのキャンセル（`57014`）で行う。外側の文全体で合算する
  方式は、大量行の書き込みで常に `54000` になるため採らない

### D12. カタログ・DDL の契約（レイアウトは #1722 が決める）

> Issue #1722 の実装記録: カタログのレイアウトと判定順は [trigger-catalog.md](trigger-catalog.md)。
> UDF カタログがテナント単位のため、トリガ関数は同じテナントの関数に限り、D9 の共有 `public` 表の
> 全テナント発火は現状のカタログでは表現できない（オーナー確認事項。同ファイルの「キー設計」節）。

- トリガは表に属し、表の削除とともに削除する。名前は表ごとに一意で重複は `42710`。
  `OR REPLACE` は原子的に置き換える
- 関数が無ければ `42883`、`RETURNS trigger` でなければ `42P17`
- トリガ関数への依存を `catalog/dependency.rs` に記録し、`DROP FUNCTION` は `2BP01`
  （`CASCADE` ならトリガを削除する）。`DROP TRIGGER [IF EXISTS] ... [CASCADE|RESTRICT]` を受理
- テナントのスキーマの表のトリガは他テナントから存在も見えない（`42P01`／`42704` で区別
  しない）。DDL は既存の DDL 権限ゲート（`--ddl-allowed-users`）を通す

### D13. エラー分類のまとめ

| SQLSTATE | 用途 | 状態 |
| -------- | ---- | ---- |
| `0A000` | 未対応の経路・定義対象・`WHEN` の式・型（D2・D7・D8） | 既存 |
| `27000` | 自己変更の衝突（D6） | **新規**（#1725） |
| `54001` | トリガの入れ子の上限超過（D11） | 既存 |
| `54000` | 文数・ループ回数の予算超過（D11） | 既存 |
| `42804` | BEFORE の戻り行が表の構造と不一致（D3） | 既存 |
| `42P17`・`42883`・`42710`・`42809`・`2BP01` | DDL の検査（D7・D8・D12） | 既存 |
| `23505`・`22023`・`23502` | 台帳（D10） | 既存 |
| `57014` | キャンセル（D9） | 既存 |

## PostgreSQL との差分（fail-closed 側に倒したもの）

| 差分 | 理由・追跡先 |
| ---- | ------------ |
| FK の検査と参照アクションを AFTER ROW のユーザートリガより前にまとめて行う | 単一検査点の維持（#1724・#1725） |
| 参照アクションが及ぶ子表のトリガは `0A000` | 結線が未定のため。追跡候補 |
| COPY・NoSQL・ビュー経由・データ変更 CTE の経路は `0A000` | 黙って発火しない経路を作らない（D8）。追跡候補 |
| 分割実行 DML では文トリガも `0A000` | 文の途中 commit で原子性を保てない |
| ビューの文トリガ・一時表へのトリガは `0A000` | 初版の範囲外。追跡候補 |
| `WHEN` でユーザー関数・揮発性関数を受理しない | 宣言的な式に限定。追跡候補 |

## セキュリティ観点（OWASP Top 10）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御（テナント境界） | 書き込んだセッションの `PolicyContext` と同じ Tx で実行し、作成者の文脈へ切り替えない。NEW／OLD は自テナントの利用者列だけで、キー・rowid・可視性は出さず書き換えもできない |
| 不安全な設計（fail-open） | 未結線の経路ガード（D8）でカタログだけから静的に `0A000`。分割実行 DML・参照アクションの子表も拒否 |
| 不安全な設計（DoS） | トリガ深さ（`54001`）・関数呼び出し深さ・文数／ループ予算（`54000`）・行ごとのキャンセル（`57014`）。上限は下げる方向だけ |
| インジェクション | `WHEN` は束縛済みの式で評価し、`TG_ARGV` は値として渡して SQL テキストへ連結しない |
| 整合性（台帳・crash） | 事前照合を BEFORE STATEMENT より前に置き、再送では発火しない。単一 redb commit |
| 情報漏えい | エラー文言は固定の英語で、値・行・他テナントの存在・件数を含めない |
| セキュリティ設定ミス | DDL は既存の DDL 権限ゲートを通し、既定で拒否 |
| 脆弱な依存 | 外部クレートを追加しない（dependency-policy） |
| spec 漏えい（P0） | 出典は PG 17 公式ドキュメントと public 資産に限り、spec はポインタ表記のみ |

## 既知の制限・追跡候補（起票は要承認）

- COPY／NoSQL／ビュー経由の経路の結線
- データ変更 CTE の副文での発火
- FK の参照アクションによる子表のトリガの発火
- 一時表・ビューの文トリガ
- `WHEN` での関数呼び出しの拡大
- トリガ経由の 3 テナント越境マトリクス（親 #1720 の受け入れ条件。#1728 と #2229 の分担を確認）

## 後続 Issue への引き継ぎ表

| Issue | 前提とする決定 |
| ----- | -------------- |
| #2228 | D1（`TriggerCall` に渡す変数）・D2（NEW／OLD に出さない列）・D3（戻り値の意味） |
| #2226 | D9・D10（トリガ内の文は台帳に記録しない。台帳モードのスコープ共通化） |
| #1722 | D8 の定義対象の制限・D12・D2 の型検査・D7 の `WHEN` の定義時検査（#2228・PR #2775 に依存） |
| #1723 | D4 の段 1〜4・D3・D5・D8 のガードの土台（INSERT）・D2 の DEFAULT の順序 |
| #1724 | D4 の段 6・D7（AFTER の `WHEN`）・D9 |
| #1725 | UPDATE／DELETE 系と UPSERT（D8）・`UPDATE OF`（D7）・D6（`feat(engine)!`）・分割実行 DML と参照アクションの子表の `0A000` |
| #1726 | 段 2・7（対象 0 行でも発火）・TRUNCATE（D8）・REFERENCING／INSTEAD OF／CONSTRAINT TRIGGER の `0A000` |
| #1727 | D11 |
| #1728 | D10（事前照合を BEFORE STATEMENT より前に行う・単一 txn・content_hash の材料）・crash-test（既存の `crash-test-*` の流儀）・越境の回帰テスト |
