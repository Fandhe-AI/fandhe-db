# MERGE の実行方式

Issue #3350（親 #3349）・対象ビヘイビア: SQL-79。関連ポインタ: SQL-53・TABLE-43・TABLE-45・TABLE-50・TASK-352・RLS-10・RECOVER-10・RECOVER-11・SQL-21・SQL-28・SQL-32・SQL-46。

spec 本文は転記せず、ID のポインタのみを記す（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは本リポ側の実装判断・設計記録だけを扱う。

## ステータス

方式は決定済み・実装は進行中（#3351〜#3358）。**#3351 で構文解析を実装済み**（`sql::allowlist::merge`・`ParsedSql::Merge`）。解析を通った MERGE は Execute・Describe・明示 Tx のいずれも固定文言の `0A000`（`MERGE is not supported yet`）で止まり、行・台帳・writer・世代に触れない（`USING OPERATION_ID` も台帳に参加させない）。従来 `42601` だった形のうち、構文を通るものが `0A000` に変わる。束縛は #3352 で入った（下の §8）。実行・`RETURNING`・台帳・`EXPLAIN` は後続（#3353〜#3357）が入れる。

**#3352 で束縛を実装済み**（`sql::merge_bind`・`core::merge_exec`。下の「#3352 の束縛」節）。束縛エラー（`42703`・`42702`・`42P01`・`42804`・`22000`・`55000` 等）は書き込みより前に返り、束縛を通った Execute・明示 Tx は固定文言の `0A000` のまま、Describe は `RETURNING` なしなら結果列なしで成功する（`RETURNING` つきは `0A000`）。

`$n` の受理範囲（#3352）: `USING OPERATION_ID $n` に加え、`WHEN ... THEN UPDATE SET <列> = $n` の右辺と `INSERT ... VALUES (...)` の要素の単独の `$n`（括弧で包んだ形を含む）を受理する。`ON`・`WHEN ... AND` の条件内、式の内側（`SET x = $1 + 1`）、`RETURNING` 内、元が部分クエリの形の `$n` は引き続き Parse 段で `42601`（UPDATE FROM の `$n`〔#3297〕と同じ段階的な制限）。

各決定は「決定・理由・アンカー」で書く。アンカーは行番号でなく関数名・モジュール名で指す。

## 1. 実行方式

### D1: 構文の範囲

- 決定: 次を受理する
  - `MERGE INTO <対象> [[AS] <別名>] USING <元> [[AS] <別名>] ON <条件> <WHEN 句>+ [RETURNING ...] [USING OPERATION_ID '<id>' | NULL]`
  - `WHEN MATCHED [AND c] THEN {UPDATE SET ... | DELETE | DO NOTHING}`
  - `WHEN NOT MATCHED [BY TARGET] [AND c] THEN {INSERT [(cols)] {VALUES (...) | DEFAULT VALUES} | DO NOTHING}`
  - `WHEN NOT MATCHED BY SOURCE [AND c] THEN {UPDATE SET ... | DELETE | DO NOTHING}`
- `USING` は 2 回現れる。元を示す `USING` は対象（と別名）の直後に 1 回だけ置く。`USING OPERATION_ID` は文末（`RETURNING` の後）だけで受理し、他の DML と同じ位置にする。構文段はキーワードの並び（`USING` `OPERATION_ID`）で区別する。`strip_operation_id_clause` は末尾の句だけを対象にする
- 条件なしの WHEN の後に同じ種類の WHEN が来たら、到達できない句として `42601`（PG17 と同じ。#3358 で実機確認）
- `UPDATE SET` は単一表 UPDATE と同じ規則（非修飾の列名・行値代入・`DEFAULT`・生成列への代入の拒否）に従う。`INSERT` は単独 INSERT と同じ規則（列リスト・`DEFAULT`・IDENTITY・`OVERRIDING`）に従う。単独 INSERT で受理しない形は MERGE でも受理しない
- `$n`（拡張クエリ）は条件・値・元の中で許す（#3351）。型推論は結合 DML の Parse 段の機構（#3472）に揃える。#3352 は値位置（SET 右辺・INSERT 値）の単独の `$n` だけを受理し、条件・式の内側・元の部分クエリの中は UPDATE FROM と同じく Parse 段で `42601`（§8）
- 理由: 既存の DML と同じ入口・規則を再利用し、MERGE 固有の構文を WHEN 句に閉じる

### D2: 元の形

- 決定: 受理する元は 3 種類
  - 単一の表
  - 単純形のビュー（`dml_from_exec::apply_view_source` の前例。列別名ビューと評価後射影形は `0A000`）
  - 部分クエリ `( SELECT ... ) [AS] s[(cols)]`
- 部分クエリは、書き込むセッションの `PolicyContext` で既存の読み取り経路（`execute_read_statement_with_limits`。`core::insert_select_exec` の前例）に通して実体化し、build 側の行源にする。上限は可視行数（`cap+1` 超過の検出）と結合の共有バイト予算。列の型は結果の列記述から取り、疑似列 `id` は持たない
- 元が対象を参照する形（相関・LATERAL 相当）は `42P01`（PG17 と同じ。#3358 で確認）
- 元にベクトル順位付けを含む形は `42601`（SQL-28 と同じ扱い）
- `USING` に JOIN の連鎖・カンマ区切りの複数表を書く形は `0A000`。部分クエリで書ける。畳み込み source の残余が未対応なため
- 理由: 元の評価を既存の読み取り経路に寄せると、RLS・上限・型付けを再実装せずに済む

### D3: 結合の形（実行方式の中心）

- 決定: 対象を probe 側、元を build 側とする既存のハッシュ結合を使う。第 2 の結合実行器は作らない（`sql::join::dml`・`execute_joined_dml_in`・`core::dml_from_exec`）
- build 側の各行に「一致した」フラグを持たせる
  - MATCHED の組: probe が真の組
  - NOT MATCHED BY TARGET: 一致フラグが立たなかった元の行
  - NOT MATCHED BY SOURCE: probe で一致が 0 件だった対象行
- 外部結合の種類は WHEN 句の構成で決まる。BY TARGET があれば元を保持し、BY SOURCE があれば対象を保持し、両方あれば完全外部結合相当にする。実行器は構成に関わらず同じ走査で列挙する
- ON の振り分けは UPDATE FROM の変換器（`sql::join::dml_plan`）と同じく、列同士の等値を結合キー、それ以外を残余（`sql::join::dml_residual`）に分ける。ただし片側だけを参照する conjunct の扱いは UPDATE FROM と意図的に異なる
  - 対象だけの conjunct は、BY SOURCE の句が無いときに限って候補の絞り込みへ押し下げてよい。ある場合は一致判定の残余にする
  - 元だけの conjunct は、BY TARGET の句が無いときに限って元の走査へ押し下げてよい。ある場合は残余にする
  - 理由: 一致しない行が INSERT・BY SOURCE の操作を受けるため、絞り込むと書き込みの範囲が変わる
- 等値キーが 0 本の形は、既存と同じく全行を 1 バケットに入れ、一致ペアの上限で抑える

### D4: 1 行ごとの WHEN 選択

- 決定: 各組について、該当する種類の WHEN を宣言順に評価し、`AND` 条件が真になった最初の句を採る。真になる句が無ければ何もしない
- `AND` 条件の束縛は残余と同じく、合成スキーマ上の `bind_where_predicates` と `BoundConjunction::matches` で行い、NULL の三値論理・型強制・照合を通常の WHERE と一致させる
- 参照できる側は句の種類で決まる
  - MATCHED: 両側
  - BY TARGET の条件と INSERT の値: 元だけ
  - BY SOURCE の条件と SET: 対象だけ
  - 反対側を参照したら `42P01`（PG17 と同じ。#3358 で確認）

### D5: 段階と `21000`（多重一致）

- 決定: 3 段で実行する
  1. 全組の操作を決めて操作列を作る
  2. `21000` と各上限を判定する
  3. 1 つの書き込みトランザクションに適用する
- 第 1・第 2 段は行を書かない
- `21000` の条件は、同じ対象行について UPDATE か DELETE を選んだ組が 2 つ以上あること
  - DO NOTHING を選んだ組と、どの WHEN も真にならなかった組は数えない
  - 対象行が元の複数行と一致しても、変更する組が 1 つ以下なら受理する
  - PG17 の挙動（2 回目の変更を試みたときに拒否する）に揃える。先が DO NOTHING で後が UPDATE の境界は #3358 の突き合わせ表に必ず入れる
- 既存の「一致位置の先頭の source 行を採る」一意化は MERGE では使わない。全組を評価して操作列を作る。`54000` の判定は UPDATE FROM と同じく、一意化の前の一致ペアの累計で行う
- 判定はすべて第 3 段より前に終える。`21000`・`54000` は副作用ゼロ（write txn を commit しない）で、台帳も消費しない

### D6: 適用の順序

- 決定: 操作列の順は、対象候補の走査順（MATCHED と BY SOURCE）の後に、元の走査順（BY TARGET の INSERT）とする。決定的にする
- 第 3 段は操作列を先頭から読み、同じ種類が続く区間ごとに既存の書き込み経路へ渡す
  - UPDATE: `tenant::update_rows_joined_unchecked`
  - DELETE: `delete_rows_where_unchecked`
  - INSERT: `sql::insert_select::rows_to_bound_inserts` から `sql::exec::execute_insert_*_in`
  - 第 2 の書き込み経路は作らない
- 区間ごとの書き込み経路は、台帳の記録を行わない形で呼ぶ（記録の抑止を引数で受け、既存の単独 DML の挙動は変えない）。台帳の事前照合と確定記録は MERGE 全体で 1 回だけ行う（D11）。区間ごとに記録すると、複数の action 種別を持つ MERGE に `USING OPERATION_ID` を付けたとき 2 つ目の区間が `23505`・`22023` になるため
- UNIQUE などの即時検査が途中の状態で衝突するかは適用順に依存する。PG17 でも計画に依存して不定なので、突き合わせ（#3358）は順序に依存しない入力で行う
- 第 3 段の挿入行・更新行は、同じ文の一致判定には現れない（文の開始時点のスナップショット）

## 2. WHEN 句の範囲と上限

### D7: WHEN 句の数と資源の上限

- WHEN 句の数の上限 `MAX_MERGE_WHEN_CLAUSES = 16`。値は `MAX_CTE_DEFINITIONS`・`MAX_SET_OP_BRANCHES` と揃える。構文段で積む前に検査し、超過は `54000`（#3351）
- 全 WHEN の `UPDATE SET` の代入数は、累計で既存の `MAX_UPDATE_SET_ASSIGNMENTS` を上限にする
- 結合の上限は UPDATE FROM と共有する（いずれも超過は `54000`・副作用ゼロ）
  - リレーション数、元の可視行数、一致ペアの累計、NLJ の比較回数、共有バイト予算
  - 対象側の走査行数（BY SOURCE があるときは対象の全候補を走査しうるため、この上限で数える）
  - 影響行数（操作列の UPDATE・DELETE・INSERT の合計）。既存の DML 上限と INSERT のバッチ上限（INDEX-4）
- 内側のループで協調的中断（`57014`）に応じる

## 3. 台帳（content_hash）

### D11: 操作種別と材料

- 決定: `OpTag` に MERGE の操作種別を 1 つ足す。番号は base 時点の次の空き番号で、7c8be8464 時点は 30（`DataModifyingCte = 29` の続番）。#3357 の着手時に open PR と照合し直す。既存タグのバイト列は変えない
- 材料は `UpdateFrom` と同じく、送られた文のトークン列から `USING OPERATION_ID` 句・最上位の `RETURNING` 句・末尾の `;` を除いたもの（`for_merge`）。`RETURNING` は応答の投影だけを決めるため、有無や投影を変えた再送も同じ文として扱う（D13 と同じ）。結合結果・操作列・行の値は含めない
- 算出位置は構文検証の入口（全検証の後）。事前照合（`precheck_in_write_txn`）は、元の走査・部分クエリの実体化より前に行う
  - 同じ内容の再送は `23505`、別の文への再利用は `22023`
  - 確定記録は書き込み本体と同じ txn で行う
  - 台帳エントリは MERGE 1 文につき 1 件。D6 の各区間（UPDATE・DELETE・INSERT）は台帳を記録せず、全区間の書き込みの後、commit の前に 1 回だけ記録する。区間ごとのキー導出は採らない（再送の判定が文単位でなくなるため）
- 束縛・`21000`・`54000` のエラーは txn を commit しないので台帳を消費しない。省略時は台帳に記録しない（RECOVER-1 のポインタ）
- 残余リスク（UDF 定義が材料に入らない）は UPDATE FROM と同じ（`multi-row-dml-operation-id.md` の Issue #1757 追記）

## 4. RLS・権限・実行文脈

### D8: RLS とテナント境界（P0）

- 一致に使う対象行は、DML の候補列挙（`tenant::enumerate_dml_candidates_with`）が返す「所有かつ可視」の行だけ。他テナントの行は一致・BY SOURCE のどちらの判定にも入らないため、UPDATE・DELETE・BY SOURCE の操作を受けない
- 元は書き込むセッションの `PolicyContext` で独立に走査する。可視性は SELECT の JOIN と同じで、UPDATE FROM と同じ非対称性を引き継ぐ。部分クエリの中の表も同じ読み取り経路で暗黙適用を受ける
- 一致相手が他テナントの行、または自テナントの不可視行だけのときは「一致なし」として扱う。BY TARGET の INSERT は単独 INSERT と同じ経路と検査を通すので、観測できる結果（`23505` 等を含む）は単独 INSERT と同じになり、新たな存在情報の経路を作らない（前例: `sql-upsert.md` の衝突検出スコープ、`sql-returning.md` の UPSERT の衝突先が不可視の場合）
- INSERT 行の所有はセッションのテナントにする（書き込み段が `ctx` で固定する）
- エラー文言は固定文言と入力された名前だけにする。行の値・件数・他テナントの存在を含めない

### D9: 明示 Tx・writer・Describe

- 明示 Tx では共有 write txn 上で実行し、対象表とトリガが書いた表へ `mark_written` する
- writer は最初の書き込み文として `ensure_writer_with` で取得する
- `READ ONLY` Tx は `25006`
- `parsed_operation_id` は実値を返す。同じ Tx 内での再利用は既存 DML と同じく拒否する
- 暗黙 Tx を通らない経路（`execute_parsed_statement` のネスト実行等）は固定文言の `0A000`
- Describe は Execute と同じ準備段（構文・束縛・変換）を通して SQLSTATE をそろえる。`RETURNING` があればその列を、無ければ結果列なしを返す

### D10: 権限

- `classify_write` に MERGE の腕を足す（網羅 match）
- 対象に課すビットは、WHEN 句に現れた操作の種類の和（`PRIV_INSERT`・`PRIV_UPDATE`・`PRIV_DELETE`）に `PRIV_SELECT` を加えたもの。実際に実行されたかどうかは問わない。`PRIV_SELECT` は ON・条件で対象を読むため
- 列単位の精密化はせず、表単位で全列を要求する（UPDATE FROM と同じ。`column-level-privilege.md` の対象外の項と対応）
- 元の SELECT はトークン走査の読み取りゲートで判定する。対象がビューのときはビュー自身の権限を判定する
- 違反は固定文言の `42501`。UPDATE 権限だけのロールが MERGE を経由して INSERT・DELETE できる経路を作らない

## 5. 周辺規則

### D12: トリガ・生成列・制約・ビュー

- 行トリガは、各操作が既存の書き込み経路を通ることで、操作ごとの種類のトリガとして発火する
- 文トリガは、MERGE の入口が WHEN 句に現れた操作の種類ごとに 1 回ずつ発火し、各区間の書き込み呼び出しでは抑止する
  - BEFORE は INSERT → UPDATE → DELETE、AFTER は UPDATE → DELETE → INSERT（PG17 の順。#3358 で実機確認）
  - #3355 の本文は「実行した操作の種類ごと」と読めるが、PG17 は実行の有無を問わないため、本記録では PG17 側を決定とし、#3355 で確認する
- 結合形 DML の `RETURNING` とトリガの併用は、`trigger-update-delete.md` の現行の契約（`0A000`）に従う
- 対象ビューの INSTEAD OF トリガを経由する MERGE は `0A000`（結線しない形）
- 生成列は、#3326（TABLE-50）の設計記録が定める「符号化直前の単一計算点」を各書き込み経路が通る。MERGE で別の計算点は作らない（担当 #3355）
- CHECK・UNIQUE・FK（`23514`・`23505`・`23503`）は単独 DML と同じ検査点を通る
- 対象が自動更新可能なビューのときは、ビュー DML（`create-view.md`）と同じく基底表とビュー述語へ書き換え、単独のビュー書き込みと同じ規則と SQLSTATE に従う（#3352）
- 一時表の関与と更新不可のビューは D14（第 6 節の表）に従う

### D13: RETURNING・タグ・EXPLAIN

- `RETURNING` は変更した行を返す。INSERT・UPDATE は新しい値、DELETE は削除した行を返す。元の列も参照でき、BY SOURCE の行では元の列が NULL になる
- `merge_action()` は `'INSERT'`／`'UPDATE'`／`'DELETE'` を返す。MERGE の `RETURNING` の外での使用は PG17 と同じ SQLSTATE（#3356・#3358 で確認）
- `RETURNING` は content_hash に影響しない
- コマンドタグは `MERGE n`。n は UPDATE・DELETE・INSERT の行数の合計で、DO NOTHING は含めない。wire 側のタグ写像の要否は #3356 で確認する
- EXPLAIN は既存の結合 DML の EXPLAIN（`update-from-delete-using.md` の EXPLAIN 節）と同じ契約に従う（#3357）
  - 表示するのは、結合の方式（ハッシュかネストループか、外部保持の向き）と WHEN の構成（種類・操作・条件の有無）
  - 見積もり・行数・表名・値は載せない（テナントの存在情報を漏らさないため）
  - ANALYZE は実際に実行する。実装までは `0A000`

## 6. 扱わない形（D14）

文言は英語の固定文字列とする（例: `MERGE in a data-modifying CTE is not supported`）。「受け皿なし」は受け皿の Issue が未起票であることを示す（起票はユーザー承認が要る）。

| 形 | SQLSTATE | 受け皿・理由 |
| -- | -------- | ------------ |
| データ変更 CTE の中の MERGE | `0A000` | 対応しない。`data-modifying-cte.md` の経路が単独 DML 前提のため |
| MERGE の前に置く WITH | `0A000` | 受け皿なし |
| 入れ子の MERGE（部分クエリ・元の中） | `42601` | 構文上、MERGE は問い合わせではないため |
| 分割実行（`PARTITIONED`） | `0A000` | SQL-32 の対象外（SQL-79 のポインタ） |
| NoSQL 表層 | op を追加しない（`0A000`） | 対象外（SQL-79 のポインタ） |
| 一時表が対象・元のどちらかに関与する形 | `0A000` | 受け皿なし。`temp_dispatch` に MERGE が無い |
| 元が JOIN の連鎖・複数表 | `0A000` | 部分クエリで代替できる。受け皿なし |
| 元の LATERAL・対象への相関参照 | `0A000`／`42P01` | lateral-derived-table（#3320）は DML を対象外にしている |
| 元のベクトル順位付け | `42601` | SQL-28 と同じ |
| 対象ビューの INSTEAD OF 経由 | `0A000` | 受け皿なし（#3355 で再判断） |
| PL/pgSQL 本体・DO の中の MERGE | `0A000` | 受け皿なし |
| 暗黙 Tx を通らない経路 | `0A000` | fail-closed |
| 結合 DML の RETURNING とトリガの併用 | `0A000` | `trigger-update-delete.md` の現行の契約 |
| WHEN 句の数の超過 | `54000` | D7 |
| 到達できない WHEN 句 | `42601` | D1 |

## 7. 担当の割り当て

| 決定 | 担当 |
| ---- | ---- |
| D1・D7 の構文段 | #3351 |
| D2・D4 の束縛と参照できる側・ビュー対象・`$n` | #3352 |
| D3・D5・D6 の MATCHED と `21000` | #3353 |
| D3・D6 の BY TARGET の INSERT と BY SOURCE | #3354 |
| D8・D10・D12 | #3355 |
| D13 の RETURNING・`merge_action()`・タグ | #3356 |
| D9・D11・D13 の EXPLAIN・crash 耐性の段 | #3357 |
| PG17 の突き合わせ（D1・D4・D5・D12・D13 の要確認点）・越境・psql の結合試験 | #3358 |
| D14 の扱わない形（第 6 節の表） | 各形に近い担当（構文段は #3351、それ以外は #3355・#3357） |

## 8. #3352 の束縛

束縛は `sql::merge_bind::bind_merge`（純関数。スキーマだけを読み、行・台帳・writer・世代・RLS に触れない）で、入口は `core::merge_exec::bind_merge_stmt`（同一の読み取りスナップショットから対象・元のスキーマを取る）。権限ゲート（`42501`）は束縛より前なので、権限の無い主体には列の存在が見えない。

- 束縛結果の relation 添字は **0 = 対象、1 = 元**（#3353・#3354 はこの規約で消費する）
- 参照できる側（D4）: `ON`・MATCHED は両方、NOT MATCHED [BY TARGET] は元だけ、NOT MATCHED BY SOURCE は対象だけ。見えない側への修飾参照は `42P01`、見えない側にしか無い非修飾の列は `42703`、両側にある非修飾の列は `42702`。解決した参照は公開名で修飾し直してから型を照合する（狭いスコープで一意だった名前が両側スコープで曖昧にならない）
- `ON`・`WHEN ... AND` の条件: 最上位の `AND` の conjunct ごとに、対象と元をまたぐ列同士の等値は型クラスを照合して結合キー候補にし（比較できない型は `42804`。`t.id = s.id` も受理）、それ以外は「対象 ++ 参照される元の列」の合成スキーマで通常の WHERE と同じ束縛器に通す。元の疑似列 `id` を残余で参照する形は固定文言の `0A000`。押し下げの決定は #3353
- `UPDATE SET`: 左辺は常に対象の列（重複 `22000`・疑似列 `42601`・未知列 `42703`・`GENERATED ALWAYS`）。右辺の列参照は `assignable` で照合し（`42804`）、元の列を含む式は合成スキーマで `bind_set_expr` に通す
- `INSERT`: 単独の INSERT と同じ規則。列リストの重複 `22000`・未知列 `42703`・従来方式の表で疑似列 `id` を省くと `22000`（採番方式は省略可）・個数不一致 `42601`・型不一致 `42804`。`DEFAULT VALUES` に列リストを付けると `42601`。値は元だけを参照できる
- 部分クエリの元: 内側を Describe して列記述だけを得る（**実行しない**）。疑似列 `id` なしの採番方式の合成スキーマにする。列別名が出力列より多ければ `42P10`、静的な型を持てない列・不正な列名は固定文言の `0A000`、出力名の重複は `42701`（PG は参照時に `42702` とするが、本実装は合成の時点で拒否する。fail-closed）。内側から対象を相関参照すると構文段が `42P01`。内側の表の RLS は実行時（#3353 以降）に書き込むセッションの `PolicyContext` で読み取り経路が適用する
- 対象がビュー（TABLE-43）: 単独のビュー書き込みと同じ規則（`view_dml::resolve_merge_target`）。自動更新できなければ `55000`（文言 `cannot merge into view`）、公開していない基底列は `42703`。列別名つきのビュー・利用者列を `id` として公開するビューは固定文言の `0A000`。Issue 本文は「`0A000`／PG17 と同じ SQLSTATE」と書くが、既存のビュー書き込み（D12）に揃えて `55000` にした。PG17 との突き合わせは #3358 の確認項目に加える（`55000` の可否・`42P01`／`42703` の側の規則・ParameterDescription の OID）。INSTEAD OF トリガを持つビューの判定は #3355
- 判定順（決定的）: 構文 → 権限 → スキーマ取得（`42P01`）→ ビュー対象の形 → スコープ構築（`42712`・`54000`）→ `ON`（列解決の後に型）→ WHEN 句を宣言順に（条件の後に操作）
- `$n`: `sql::params_merge`（`looks_like_merge` で分岐する専用の走査）。型は SET が対象の列、INSERT が句の列リスト（省略時は対象表の `*` 展開）の序数の列。Describe 用ダミーフラグの並びは「WHEN 句ごとに SET の代入／INSERT の値の宣言順」で、個数が合わなければ全て検証側へ倒す

対象外（受け皿）: 条件内・式の内側の `$n`、元が部分クエリの形の `$n`（対応しない（現行の制限として固定する。要望が出た時点で別途起票する）。#3472 と同じ制限）、列別名つきのビューを対象にする形（対応しない（現行の制限として固定する。要望が出た時点で別途起票する））、`RETURNING` の束縛（#3356）、実行・押し下げ（#3353・#3354）。

## セキュリティ

- アクセス制御: D8 の「所有かつ可視」の一致集合で、他テナントの行に BY SOURCE の DELETE・UPDATE が及ぶ経路を構造的に塞ぐ。D10 で WHEN に現れた操作の種類ごとの権限を課し、MERGE 経由の権限昇格を防ぐ
- インジェクション: 構文段はトークン列だけを扱い、未検証の入力を文字列連結で SQL・計画へ組み立てない。台帳の材料もトークン列だけにする
- 不安全な設計: 判定をすべて書き込みの前に行う 3 段構成で、`21000`・`54000` を副作用ゼロにする。WHEN 句の数・代入数・結合の上限で無制限の資源確保を防ぐ。扱わない形は fail-closed
- ログ・エラー: 固定文言と入力された名前だけにし、件数・他テナントの存在を出さない。EXPLAIN に見積もり・値を載せない
- 依存の変更なし

## テスト方針

各子 Issue の結合テストは `crates/engine/tests/sql_t40_up/` の束ね方に従う（`coding-rust.md` のテスト節・README「engine の結合テストの束ね方と個別実行」）。PG17 との突き合わせは #3358 が担う。
