# ADR: 変更ログ（増分バックアップ・PITR）の設計

- ステータス: Accepted（本リポの実装判断。振る舞い契約の SSOT は spec の RECOVER-14 で、
  本書へは転記しない）
- 対応 Issue: #2165（親 #2164・ルート #2148。後続は #2166〜#2170。関連は #2149・#2154・#2446）
- ビヘイビア ID ポインタ: RECOVER-14。関連として RECOVER-13・RECOVER-15・RECOVER-17・
  RECOVER-2・PERSIST-1・PERSIST-5・SQL-37・TABLE-24・TASK-257 をポインタで挙げる
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード（行番号は書かずシンボル名で示す）:
  - `recovery::commit_boundary::{commit_and_finish, commit_and_finish_with}`
  - `storage::{prepare_generation_bump, commit_prepared_write_txn, bump_generation_and_commit,
    GatedWriteTxn, GENERATION_TABLE}`、`Storage::begin_write_txn`
  - `storage::commit_log`（SQL-38 用のインメモリ commit 記録。本 ADR の永続変更ログとは
    別物。名前衝突を避け、永続側はモジュール `storage::change_log`・redb 表 `change_log`
    と呼ぶ）
  - `storage::table_image::{capture, apply, SHAPES}`
  - `recovery::ledger::{OP_LEDGER_TABLE, LAST_OP_TABLE}`、`recovery::partitioned_job`
  - `catalog::changed_table_generations_in_write_txn`
- 関連 doc: `explicit-transaction.md`・`now-evaluated-value-ledger.md`・
  `sequence-namespace-and-ledger.md`・`operation-id-ledger.md`・`resend-semantics.md`・
  `partitioned-dml.md`・`crash-tolerance-reverification.md`・`benchmark-judgement-policy.md`・
  `ingest-write-path.md`・`temp-table.md`
- 検証コード: なし（設計記録のみ。コード変更は後続 Issue）

## 1. 背景

redb は単一ファイルで、WAL を外部へ出さない。増分バックアップと PITR（指定 LSN／時刻
までの復元）には、commit ごとの変更を取り出せる自前の変更ログが要る。本書は形式・捕捉点・
commit 番号・アーカイブと保持・復元手順・有効化境界・性能影響の見積りを確定し、
後続 #2166〜#2170 の実装前提を固める。バックアップ本体（#2149・#2154）は未実装で、DB の同一性を
表す識別子も現状は無い。

## 2. 公開コードの現状

| 事実 | 設計への含意 |
| ---- | ------------ |
| 実書き込みを伴う commit は `commit_and_finish`（`prepare_fn`→`commit_fn` の 2 段）と `bump_generation_and_commit` の 2 経路に一本化されている | 捕捉点は 2 箇所で足りる |
| `prepare_generation_bump` が実書き込みのある commit ごとに `storage_generation` を同一トランザクション内で `checked_add` により +1 する。書き込みなしの経路は abort し世代を進めない | 世代を commit 番号に流用できる |
| writer は writer gate で排他。commit は直列で世代順と commit 順が一致する | 連続性検査が成立する |
| `table_image` は表単位の差分（upsert／remove／drop）を取り再適用もできるが、全表を走査する | 再生器に再利用できるが commit ごとの捕捉には使えない |
| 明示 Tx の `ROLLBACK TO` は基底へ復元して差分を再適用する方式 | 書き込み時のログは巻き戻しと整合しない。commit 時の最終内容を読む必要がある |
| HNSW グラフ等はメモリ上の派生状態。一時表は commit 記録を持たない別ストレージ | 記録対象は主 `Storage` の redb 内容のみ |

## 3. 決定事項

### D1 捕捉方式

**書き込んだキーの集合を計装で積み、prepare 段で after-image を読む。**

- 書き込み時は、`GatedWriteTxn` の寿命に結び付けた capture frame（`commit_log` の frame と
  同じ置き場）へ `(redb 表名, キーのバイト列)` だけを積む。キー集合は上位集合であれば正しい
- `commit_and_finish` の `prepare_fn` 内で `prepare_generation_bump` の直後（point of no
  return の前）に、積んだ各キーの現在値を読む。値があれば upsert、無ければ remove として
  レコードを組み、同一 redb トランザクションで `change_log` 表へ insert する。
  `bump_generation_and_commit` も同じ順序にする
- 値ではなく最終内容を prepare で読むため、SAVEPOINT の `ROLLBACK TO` で巻き戻ったキー、
  `insert_reserve` のゼロコピー書き込み、評価済みの値（`now()`・採番結果）がいずれも構造的に
  正しく記録される
- 計装漏れへの fail-closed 3 層:
  1. 構造: キーを積むのは writer 側の表ハンドルのラッパ（例: `CapturingTable`）に一本化する。
     redb の `open_table`・`delete_table`・`rename_table`・`open_multimap_table` の直呼びを
     禁止する仕組み（clippy `disallowed-methods` か型）は #2166 で選ぶ。`retain`／
     `extract_if`／`drain` 系は削除したキーを積む実装に限る
  2. 本番の検査: `changed_table_generations_in_write_txn` で変更が確定した表に frame の
     エントリが無ければ commit 前に拒否する（`commit_log` と同じ契約）。ただし次の 2 点を
     明記する（後続実装がこの検査を完全性保証として扱わないため）
     - 名前の対応: 同関数が返すのは論理表名（`table_generation` 登録名）で、frame のキーは
       redb 物理表名（行ストアは `user_rows_table_name` による `user_rows/{table_name}`
       等の別名）である。照合は、論理表名から物理表名の集合を導く 1 本の対応関数（行ストア・
       索引・メタ表ごとの写像）を経由して行い、物理名を直接比較しない。対応が未定義の論理表は
       拒否ではなく実装時のテストで検出する（正常捕捉分を誤って拒否しない）
     - 検査範囲: 同関数は世代管理対象の表だけが対象で、views・`op_ledger` 等を含む全 redb 表の
       変更検出器ではない。世代管理対象外の表は本検査では漏れを検出できないため、層 1 の
       ラッパ一本化（直呼び禁止）と層 3 のオラクル（`table_image::capture` による全表の実差分
       照合）で担保する。したがって層 2 は補助的な早期検出であり、完全性は層 1 と 3 が負う
  3. テストのオラクル: 変更ログ有効の構成で、各 commit のレコードと `table_image::capture`
     （committed と writer の実差分）を突き合わせ、不一致なら失敗とする
- 規模: engine の `open_table` 呼び出しは数百箇所に及ぶ。#2166 は「ラッパと frame」
  「書き込み点の移行」「オラクル」に分割することを推奨する（起票は 7 節の申し送り）

### D2 commit 番号（LSN 相当）

**`storage_generation` の値 `G_c` を流用する。** 実書き込みのある commit で必ず 1 ずつ、
同一トランザクション内で増え、writer 排他のため順序が一意になる。別カウンタは 2 本の整合
という新たな不変条件を生むため採らない。

- 書き込みなし commit は世代もレコードも進まない。一時表ストレージは対象外
- `Storage::open` の移行 commit は、レコードにストレージ形式の版数を持たせ、版数が一致しない
  再生を fail-closed とする
- **#2149／#2154 への要件**: バックアップが記録する取得時点の commit 番号は同じ
  `storage_generation` の系列でなければならない（連鎖検証が成立しないため）。この番号は
  テナントのセッションから照会できない（RECOVER-13 のポインタ）。現状 SQL／NoSQL 表層へ
  出しておらず、今後も出さない

### D3 レコード形式

**redb の KV レベルの after-image。** 1 レコード = 1 commit。

- ヘッダ: 形式版数・DB 識別子・timeline・LSN（`G_c`）・prev LSN・commit 時刻（壁時計 i64 µs）・
  ストレージ形式の版数・ペイロード長・チェックサム
- 本体: 表ごとの `(表名, shape 番号, upserts, removes)` と作成・削除された表。`table_image` の
  差分表現・`SHAPES` に揃え、再生器は `table_image::apply` を再利用する
- エンコードは自前の長さ前置き形式。長さはデコード前に上限検証する。チェックサムは既存の
  自前ハッシュ実装を使い、新規依存を追加しない（dependency-policy）
- 記録範囲: 真の状態（行・ユーザー表・catalog 系・op_ledger・last_op・sequence_state・
  rowid_state・id counter・partitioned_job 等）と派生（索引・世代カウンタ）の**どちらも
  記録する**。再生結果が元とバイト単位で一致し、世代カウンタから再開位置も得られる。
  例外は `change_log` 表自身とアーカイブ水位の表
- 台帳: op_ledger・last_op は KV として引き継がれる。PITR 目標より後に commit された
  `operation_id` の再送は新規書き込みとして受理されうる。これは運用上の注意であり、契約は
  RECOVER-2 のポインタに従う。評価済みの値は再評価されない（SQL-37・TABLE-24。
  `now-evaluated-value-ledger.md` D11 の申し送りに応える）

### D4 明示 Tx・SAVEPOINT・分割実行

- 明示 Tx は 1 commit = 1 レコード。`ROLLBACK TO` は D1 の最終内容読みで整合し、frame の巻き戻しは
  不要（余分なキーは no-op になるだけ）
- 分割実行（`partitioned-dml.md`）はチャンク commit ごとに 1 レコード。PITR の停止は commit 境界
  だけで起こる

### D5 DB 同一性と timeline

- DB 作成時に乱数の DB 識別子を生成して redb へ永続化する。既存 DB は変更ログを初めて有効にした
  ときに生成する
- timeline は PITR・復元のたびに分岐する番号。#2446（RECOVER-17）の復元世代番号の流用を
  推奨し、決め方は #2446 と整合させる
- セグメント名とヘッダに (DB 識別子, timeline, LSN 範囲) を入れ、別 DB・別 timeline のログの
  適用を拒否する

### D6 アーカイブと保持（#2167）

- セグメント: `change_log` 表のレコードを LSN の連続区間かつバイト上限で区切る（上限値は実装
  既定値であり spec 由来ではない）
- 書き出し: 一時名へ書く → fsync → rename → ディレクトリ fsync。ファイル権限は `0600`
- アーカイブ済み水位を redb へ永続化し、`change_log` 表から削除するのは水位以下だけとする。
  削除 commit も世代を進める（キャッシュ過剰失効の回避策として、後続書き込みの prepare への
  相乗りと専用トランザクションを #2167 で比較して選ぶ）
- 書き出し失敗は（テナント情報なしで）ログに残して再試行し、未アーカイブ分は削除しない。
  未アーカイブ容量の上限（既定値 + CLI）到達時は新しい書き込みを prepare 段で拒否する
  （SQLSTATE は RECOVER-14 のポインタに従う）
- 保持期間・容量は CLI で設定し、既定は削除しない。削除可能範囲は RECOVER-14 のポインタに従う
- フラグ名案: `--change-log`・`--change-log-archive-dir`・`--change-log-max-unarchived-bytes`・
  保持系。確定は #2166／#2167。パースは既存 `*_opt.rs` と同作法（重複指定・不正値は起動拒否）

### D7 増分バックアップと PITR の復元手順（#2168／#2169）

- 増分バックアップ = 直前バックアップの LSN より後のセグメント区間。連鎖検証は LSN の連続性・
  DB 識別子・timeline・チェックサムで行う
- PITR:
  1. ベースを新しいパスへ復元する（明示フラグなしに既存 DB を上書きしない）
  2. 接続を受け付けない回復モードで LSN 順に再生し、各レコードで `prev == 現在の
     storage_generation` を検査する
  3. 適用は 1 つ以上のレコードを 1 つの redb トランザクションで行い、停止は commit 境界だけ
  4. 再生中のクラッシュは DB 内の `storage_generation` から再開できる（冪等）
- 目標指定: LSN 指定はその LSN で停止。時刻指定は「commit 時刻が T を超える最初のレコードの
  直前」で停止する（壁時計が逆行しても commit 順の接頭辞になり決定的）
- 範囲外・欠落・順序違反・改ざん・識別子不一致は非 0 終了で復元を拒否し、黙って飛ばさない
- 完了時に #2446 のフックで世代番号を 1 回だけ進めてから接続を受け付ける（詳細は #2446）。
  派生状態（HNSW・キャッシュ）は通常の open で再構築する

### D8 有効化の境界と durability

- 起動時 opt-in で既定は無効。無効時は frame を確保せず、prepare に分岐が 1 つ増えるだけで
  既存経路の挙動・性能は不変
- 有効化前のベースから有効化をまたぐ PITR はできない。有効→無効→有効は LSN 欠落として連鎖が
  切れ、新しいフルバックアップが要る（fail-closed）
- `--durability none` 併用時の喪失範囲は、レコードがデータと同一トランザクションにあるため
  PERSIST-5 と同じ（ポインタのみ）

### D9 性能影響の見積り

本書は定性分析のみで、実測値・閾値は書かない。閾値は RECOVER-13・RECOVER-14 のポインタに従う。

- 無効時: 分岐 1 つ・確保なし
- 有効時（1 commit あたり）: キー収集は O(変更キー数)。prepare は変更キーごとの B-tree get、
  エンコード、`change_log` への insert が各 1 回。行データを表とログに二重に書くため書き込み
  バイト数は 2 倍前後（redb の COW ページ単位）。fsync 回数は同一トランザクションなので増えない
- 1 行 commit は fsync 支配で相対影響が小さく、バイト律速のバルク ingest（大きなベクトル行）は
  影響が大きい、という見立て
- 計測は `benchmark-judgement-policy.md`（交互実行・N ≥ 5 ペア・ノイズ帯併記・共有環境の数値を
  採否根拠にしない）に従い、`make bench-ingest-profile` 等で #2166 が行う

### D10 テスト方針（#2166〜#2170 への申し送り）

- D1 のオラクル試験
- ラウンドトリップ: ベース + 再生の結果を `table_image::capture` で元と比べ、差分が空であること
- 欠落・順序違反・改ざん・別 DB／別 timeline の拒否
- `scripts/crash_test*.sh` 系への PITR シナリオ追加（#2170。CI ジョブ追加時は `github-actions.md`）
- テストは RECOVER-14 等の ID に紐付ける

## 4. 却下した代替案

| 案 | 却下理由 |
| -- | -------- |
| (a) commit ごとに `table_image::capture` で全差分を取る | 計装漏れは無いが O(DB サイズ) / commit でホットパスに使えない（テストのオラクルとして採用） |
| (b) 行レベルの論理レコードを出して再生で engine を再実行 | カタログ・台帳・採番・索引保守の再現器が要り、`now()` 等の再評価混入リスクがある |
| (c) 表レベルの計装と触った表の全差分 | 大きな表で O(表サイズ) |
| 別カウンタの LSN | `storage_generation` との整合が新たな不変条件になる |

## 5. 後続 sub-issue との対応

| sub-issue | 担当 |
| --------- | ---- |
| #2166 | D1・D2・D3・D5（識別子）・D8・D9 の計測 |
| #2167 | D6 |
| #2168 | D7 の増分 |
| #2169 | D7 の PITR・目標指定 |
| #2170 | D10 の crash |
| #2149／#2154 | D2 の要件（commit 番号の系列・識別子をバックアップに含める） |
| #2446 | D5 の timeline・完了時のフック |

## 6. セキュリティ（OWASP）

- アクセス制御・テナント境界（P0）: 変更ログ・セグメント・バックアップは全テナントのデータを
  含む。扱えるのは運用者の CLI だけで、SQL／NoSQL セッションから実行も照会もできない。LSN・
  アーカイブ状態・遅延もセッションへ出さない（量から他テナントの書き込みを推測させない）
- fail-closed: 欠落・順序違反・チェックサム／識別子／形式版数の不一致は復元拒否（非 0 終了）。
  計装漏れは commit 前拒否とオラクルで検出。容量上限到達は黙って捨てず拒否する
- 入力検証: セグメント・バックアップファイルは untrusted。長さ検証後に確保し（無制限
  `with_capacity` 禁止）、`checked_*` 演算を使い、未知の版数は拒否する
- 情報漏えい: エラー・ログに表名・テナント ID・行値・件数・LSN を出さない
  （`commit_boundary` の stderr 方針を踏襲）
- ファイル: 出力は `0600`、一時名から rename、既存ファイルや空でないディレクトリへは書かない
- 秘密情報: ロールの検証子は保存形式のまま含め、平文にしない（RECOVER-13 のポインタ）
- 脆弱な依存: チェックサム・エンコードは自前実装で依存を追加しない

## 7. 対象外・未決事項

- コード変更・依存追加はしない。実測値は書かない
- 申し送り（Issue 起票はしていない。要否は人の判断）: #2166 の分割（ラッパ移行の規模）、
  #2149 への commit 番号系列の要件、#2446 との timeline の整合
- 未決: 移行 commit の扱いの詳細、削除 commit の相乗り方式、CLI フラグ名の最終形（#2166／#2167）
