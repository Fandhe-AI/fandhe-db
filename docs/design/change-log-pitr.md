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

redb は単一ファイルで、WAL を外部へ出さない。増分バックアップと PITR（RECOVER-14 (3)）
には、commit ごとの変更を取り出せる自前の変更ログが要る。本書は形式・捕捉点・
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
- **prepare への受け渡しは capture session の明示渡しとする（アドレス維持は採らない）。**
  現行の frame 登録簿は `Box<redb::WriteTransaction>` のアドレスをキーにするが、
  `GatedWriteTxn::split_for_commit` は Box 内のトランザクションを値として取り出して移動する
  ため、`commit_and_finish`・`bump_generation_and_commit` の prepare 段で受ける
  `&redb::WriteTransaction` のアドレスは登録時と異なり、`note_*` の登録簿引きは常に外れる。
  アドレスの維持は Box を commit 後まで保持する形に `redb` の `commit(self)` の所有権要件が
  噛み合わず採らない。代わりに capture session（`CommitParts` の `session` と同じ持ち方で、
  frame への `Arc` を直接持つもの）を prepare に渡し、prepare 内ではアドレス引きを使わず
  session 経由でだけ積む・読む。具体的には:
  - `prepare_generation_bump` は `GENERATION_TABLE` の `GENERATION_KEY` を直接書く。この書き込みは
    登録簿に載らないため、capture session の prepare が、世代採番の直後に
    `(GENERATION_TABLE, GENERATION_KEY)` を固定のエントリとして frame へ積んでから after-image を
    読む。D3 の世代カウンタの after-image が必ず入り、次のレコードの
    `prev == storage_generation` 検査（D7）が成立する
  - 同様に prepare 内で追加する他の書き込み（D6 の水位・`change_log` 表）は、記録範囲の例外であるため
    積まない。prepare 内で積む必要があるのは世代カウンタだけである
  - `bump_generation_and_commit` も同じ構造（`split_for_commit` → `prepare_generation_bump` →
    session の prepare → commit → session の finish）で、`g_c` を session へ渡して同じ固定エントリを
    積む。session を持たない経路（一時表ストレージ・テスト fixture）は従来どおり何も積まない
- `commit_and_finish` の `prepare_fn` 内で `prepare_generation_bump` の直後（point of no
  return の前）に、積んだ各キーの現在値を読む。値があれば upsert、無ければ remove として
  レコードを組み、同一 redb トランザクションで `change_log` 表へ insert する。
  `bump_generation_and_commit` も同じ順序にする
- 値ではなく最終内容を prepare で読むため、SAVEPOINT の `ROLLBACK TO` で巻き戻ったキー、
  `insert_reserve` のゼロコピー書き込み、評価済みの値（`now()`・採番結果）がいずれも構造的に
  正しく記録される
- 表のライフサイクル（作成・削除・rename）はキー集合に現れないため、frame はキーとは別に
  表単位のイベントも積む。根拠は catalog が行ストア・一意索引・キー索引・台帳系の表を
  `create`（`open_table` による遅延作成を含む）・`delete_table`・`rename_table` で直接
  増減させていること（`drop_table` は行ストア等を `delete_table` し、`rename_table` は
  行ストアを `rename_table` で付け替える）
  - 積む内容: `Created(表名, shape 番号)`・`Dropped(表名)`・`Renamed(旧名, 新名)`。
    shape 番号は `table_image::SHAPES` の番号で、空表の作成も `Created` だけで再現できる
    （キーが 0 件でも表は存在し、shape も復元される）
  - 書き込み時にイベントを時系列で積み、`ROLLBACK TO` でも巻き戻さない。prepare 段では
    イベント列を単独で再生せず、commit 時点の最終状態へ**正規化**する。具体的には
    「基底（直前 commit）の表集合」と「writer の `list_tables` による最終の表集合」を
    比べ、最終に在って基底に無い表は `created`、基底に在って最終に無い表は `dropped`、
    両方に在る表は通常のキー差分だけとする。作成後に削除された表・`ROLLBACK TO` で取り消された
    作成は両集合に現れず、レコードにも残らない
  - rename は物理表の付け替えで、改名後の表の全行はキー集合に現れない。再生可能な形に
    するため、rename は旧名の `dropped` と新名の `created` + 全行 `upserts` へ**分解**する
    （正規化時に新名の表を全走査して after-image を読む。コストは O(改名表のサイズ) で、
    rename は DDL で頻度が低いため許容する）。`Renamed` イベントは正規化で「新名の表が
    基底の旧名の表と同一の物理表か」を判定する手掛かりに留め、レコードへは出さない
  - **rename をまたぐキーは最終名へ写像せず、全行の after-image で正規化する（採用）。**
    frame のキーは書き込み時点の表名に属するため、A→B と rename して B の行を更新し B→A と
    戻す場合、また新規の空表へ挿入してから rename する場合、積まれたキーは途中の名前の
    もので最終名の更新としては取得できない。表の実体ごとにキー集合を追跡して最終名へ
    写像する案は、`ROLLBACK TO`・DROP 後の同名再作成・物理表を持つ索引まで含めて
    実体の同一性を追う状態が要り、追い損ねが取りこぼし（fail-open）になるため採らない。
    代わりに、当該 commit で `Created` または `Renamed`（旧名・新名のいずれか）のイベントが
    あった表のうち最終の表集合に在るものは、キー差分を使わず**全行の after-image**で記録する
    （基底に同名の表が在れば `dropped` も併載し、`created` + 全行 `upserts` とする）。
    途中の名前のキーは、最終の表集合に無ければ読む対象にせず、在っても全行読みが上位集合として
    包含するため破棄する。したがって A→B→A も A を全行で記録する。コストは
    O(rename または作成を経た表のサイズ) で、DDL は頻度が低いため許容する。
    判定はイベント列と最終の表集合だけから行え、キー集合の写像に依存しない
  - rename は catalog 側で表名キーの論理メタ（`catalog`・`op_ledger`・`last_op`・
    `rowid_state` 等）の付け替えも伴うため、これらはキー集合の通常経路（旧名キーの
    remove・新名キーの upsert）で捕捉される。物理表の付け替え（`user_rows/{旧名}` →
    `user_rows/{新名}`）だけが上記の分解対象である
  - 再生器は `table_image::apply` と同じ順序（`dropped` の削除 → `created` の作成 → 各 shape
    の upsert／remove）で適用する。同一レコード内で同名表が dropped かつ created になる
    場合（DROP 後の同名再作成、および上記の rename を経た同名の表）は、正規化で「基底と別実体」と
    判定できるよう、基底の表を `dropped` に、最終の表を `created` + 全行 `upserts` として
    両方載せる
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

- 書き込みなし commit は世代もレコードも進まない。一時表ストレージは対象外。D6 の保守 commit
  （`change_log` 表と水位の表だけを書く）も世代を進めず、レコードを生まない
- `Storage::open` の移行 commit は、レコードにストレージ形式の版数を持たせ、版数が一致しない
  再生を fail-closed とする
- **#2149／#2154 への要件**: バックアップが記録する取得時点の commit 番号は同じ
  `storage_generation` の系列でなければならない（連鎖検証が成立しないため）。この番号の
  公開範囲は RECOVER-13 (4) のポインタに従う

### D3 レコード形式

**redb の KV レベルの after-image。** 1 レコード = 1 commit。

- ヘッダ: 形式版数・DB 識別子・timeline・LSN（`G_c`）・prev LSN・commit 時刻（壁時計 i64 µs）・
  ストレージ形式の版数・ペイロード長・チェックサム
- 本体: 表ごとの `(表名, shape 番号, upserts, removes)` と作成・削除された表（D1 の正規化後。rename は dropped + created へ分解）。`table_image` の
  差分表現・`SHAPES` に揃え、再生器は `table_image::apply` を再利用する
- エンコードは自前の長さ前置き形式。長さはデコード前に上限検証する。チェックサムは既存の
  自前ハッシュ実装を使い、新規依存を追加しない（dependency-policy）
- 記録範囲: 真の状態（行・ユーザー表・catalog 系・op_ledger・last_op・sequence_state・
  rowid_state・id counter・partitioned_job 等）と派生（索引・世代カウンタ）の**どちらも
  記録する**。再生結果が元とバイト単位で一致し、世代カウンタから再開位置も得られる。
  例外は `change_log` 表自身とアーカイブ水位の表。この 2 表は D6 の保守 commit が世代を進めずに
  書き換えるため、記録しても再生で再現できず、D7 の連続性検査の対象にも入れない
- 台帳: op_ledger・last_op は KV として引き継がれる。復元後の再送の扱いは RECOVER-2、
  評価済みの値の再評価に関する契約は RECOVER-14 (4) のポインタに従う
  （`now-evaluated-value-ledger.md` D11 の申し送りに応える）

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
- 稼働中バックアップの書き出し（`export_snapshot`）は `change_log`・メタ表も他の表と同じく複製する。
  base backup とログを同じ DB 識別子・timeline で結びつけるためで、テストで固定している。複製に
  含まれるログ行の刈り込みは #2167 の範囲とする。savepoint 取得の走査（`table_image::capture`）は、
  ログが commit の prepare 段でのみ書かれ writer の取得中は基底と同一なので、この 2 表を除外する

### D6 アーカイブと保持（#2167）

- セグメント: `change_log` 表のレコードを LSN の連続区間かつバイト上限で区切る（上限値は実装
  既定値であり spec 由来ではない）
- 書き出し: 一時名へ書く → fsync → rename → ディレクトリ fsync。ファイル権限は RECOVER-13 (4) のポインタに従う
- **再試行は収束させる（採用）。** セグメントの rename 後・redb の水位更新前にクラッシュ
  （または水位更新の失敗）すると、再起動後の再試行ではセグメント名（DB 識別子・timeline・
  LSN 範囲から決まる決定的な名前）が既に存在する。既存ファイルへの書き込みを拒否して失敗を
  繰り返すと、未アーカイブ量が減らず容量上限で利用者の書き込みが止まる。そのため、書き出し前に
  同名のセグメントが在れば内容を検証する
  - 一致（ヘッダ・各レコードのチェックサム・`change_log` 表から再構成したバイト列との比較で
    同一と確認できた）: 書き出しは成功扱いとし、書き込みをやり直さずに水位だけを進める
  - 不一致・読めない・途中までしか無い: 上書きも削除もせず fail-closed で止め、エラーを記録して
    アーカイブを進めない。決定的な名前の衝突は破損か別源の混入であり、黙って置換しない。
    一時名の残骸（rename 前のクラッシュ）は、再試行の先頭で一時名の掃除として削除してよい
  - 検証と水位更新は、同じセグメントに対して何度繰り返しても結果が同じ（冪等）にする
- アーカイブ済み水位を redb へ永続化し、`change_log` 表から削除するのは水位以下だけとする
- **水位の更新と水位以下の削除は「保守 commit」で行う（採用）。利用者 commit への相乗りは
  採らない。** 水位も削除も `change_log` 表と水位の表への書き込みで、どちらも D3 の記録範囲の
  例外である。通常の commit 経路（`prepare_generation_bump` → レコード生成）に載せると、
  水位更新・削除の commit が世代を +1 してレコードを生み、そのレコードを再びアーカイブして
  水位を更新すると次の commit が生まれ、書き込みが止まっても収束しない。レコードだけを
  省略すると世代の欠けが生じ、D7 の `prev == storage_generation` 検査が破綻する。相乗りでは
  水位の永続化が利用者 commit に依存し、容量上限（RECOVER-14 (2)）で利用者の書き込みが prepare で
  拒否されている間は相乗り先が現れず、復帰できない。保守 commit はこの 3 点をすべて避ける
  - 保守 commit は `change_log` 表と水位の表だけを書く。`GENERATION_TABLE` を書かず
    （`prepare_generation_bump` を呼ばない）、capture session も持たないため、世代もレコードも
    生まない。`commit_boundary` に専用の入口（名前は `commit` 接頭辞。例: `commit_maintenance`）を
    置き、prepare は何もしない（世代を進めない）形で `commit_and_finish_with` を使う。point of no
    return の保護（`PostCommitPanicGuard`・commit の `Err` は abort）は他の commit と同じにする
  - 既存の不変条件との両立:
    - 世代とキャッシュ: `storage_generation`（`GENERATION_TABLE`）も表世代（`CATALOG_TABLE` の
      `bump_table_generation_in_txn`）も動かない。両表は SQL・catalog から参照されない内部表で、
      利用者に見える表を変えないため、世代で失効するキャッシュ（`visible_cache`・
      `PrefilterIndex`・`USING PLAN` の世代照合）は失効させる必要がなく、`prev == 現在の
      storage_generation` も保たれる。`change_log` 表と水位の表が SQL・catalog から名前解決
      されないことは #2166／#2167 の実装要件とする
    - commit 記録（`commit_log`）: session を持たないので `finish_commit` を呼ばず、`last_generation`
      も床も動かない。後続の利用者 commit の `g_c` は `last_generation + 1` のまま連続する
    - `table_generation_bump_coverage` の許可リスト: 専用入口は `commit` 接頭辞の関数のため走査対象で、
      呼び出し箇所を理由つきで許可リストへ 1 件加える（`create_enum_type` と同じ「`CATALOG_TABLE`・
      `user_rows` に触れない書き込み」の扱い）。接頭辞を外す案は、`redb::WriteTransaction` を値で
      受ける公開関数を commit 接頭辞に限る交差検証に反するため採らない
    - 計装の検査: 層 2（`changed_table_generations_in_write_txn`）は世代管理対象の表だけを見るので
      保守 commit に作用しない。層 3 のオラクルは `change_log` 表と水位の表を比較対象から除く
  - 書き込みゲート: 保守 commit も `begin_write_txn_raw` の前にゲートの permit を取る（`redb` の
    単一ライタへの経路はゲート経由だけ、という `writer_gate` の不変条件を保つ）。permit は
    `acquire_yielding` で取り、通常の書き込みが待機している間は譲る。待機上限超過
    （`WriteLockTimeout`）は保守の失敗ではなく「今回は見送り」とし、次の呼び出しで再試行する
    （水位は進まないだけで、D6 の再試行の収束に沿う）。ただし未アーカイブ量が容量上限に達している
    間は、permit を `acquire` で取り、通常の書き込みと同列に並ぶ。上限の拒否は利用者 commit の
    prepare で起きるので、拒否を受けて再試行する書き込みが `waiting_normal` を押し上げ続けると、
    譲る方式では保守 commit が走れず復帰できないため。明示トランザクションを持つスレッドからは
    呼ばない（`HeldByCurrentThread`）。GatedWriteTxn の `recording` は付けない
  - 容量上限からの復帰: 上限判定による拒否は利用者 commit の prepare だけで行い、保守 commit には
    課さない。利用者の書き込みが拒否・停止していても、アーカイブ書き出し → 保守 commit
    （水位更新と削除）で未アーカイブ量が下がり、上限を下回れば利用者の書き込みが再開する
  - 順序と原子性: セグメントの書き出し（rename・ディレクトリ fsync）が済んでから保守 commit を行う。
    水位更新と削除は同一の保守 commit に入れ、片方だけが永続化されることはない。クラッシュで
    保守 commit が失われても、上記の再試行（同名セグメントの検証 → 水位だけ進める）で収束する
  - 書き込みが止まっていても水位以下のレコードは保守 commit が削除するため、残置は
    1 回の保守 commit の削除上限ぶんに限られる。容量上限は未アーカイブ分だけを数える
  - 1 回の保守 commit で削除する件数・バイト数に上限を設け、ゲート占有時間を抑える（上限値は
    実装既定値で、残りは次の保守 commit が引き継ぐ。水位の更新は削除の完了を待たない）
- 書き出し失敗時の再試行、未アーカイブ容量の上限と書き込み拒否、保持の既定と削除可能範囲は
  RECOVER-14 (2)(7) のポインタに従う。実装上は水位以下だけを削除するため、未アーカイブ分は
  削除されず、上限の判定と拒否は prepare 段で行う
- フラグ名案: `--change-log`・`--change-log-archive-dir` ほか（RECOVER-14 (2)(7) の設定項目）。
  確定は #2166／#2167。パースは既存 `*_opt.rs` と同作法（重複指定・不正値は起動拒否）

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
- 範囲外・欠落・順序違反・改ざんなどの復元拒否の契約は RECOVER-14 (3)(7) のポインタに従う
- 完了時に #2446 のフックで世代番号を 1 回だけ進めてから接続を受け付ける（詳細は #2446）。
  派生状態（HNSW・キャッシュ）は通常の open で再構築する

### D8 有効化の境界と durability

- 有効化の方式と無効時の挙動は RECOVER-14 (7) のポインタに従う。実装上、無効時は frame を
  確保せず、prepare に分岐が 1 つ増えるだけ
- 有効化前のベースから有効化をまたぐ PITR はできない。有効→無効→有効は LSN 欠落として連鎖が
  切れ、新しいフルバックアップが要る（fail-closed）
- `--durability none` との関係は RECOVER-14 (6)・PERSIST-5 のポインタに従う。実装上、
  レコードはデータと同一トランザクションにある

### D9 性能影響の見積り

本書は定性分析のみで、実測値・閾値は書かない。閾値は RECOVER-13・RECOVER-14 のポインタに従う。

- 無効時: 分岐 1 つ・確保なし
- 有効時（1 commit あたり）: キー収集は O(変更キー数)。prepare は変更キーごとの B-tree get、
  エンコード、`change_log` への insert が各 1 回。行データを表とログに二重に書くため書き込み
  バイト数は 2 倍前後（redb の COW ページ単位）。fsync 回数は同一トランザクションなので増えない
- 1 行 commit は fsync 支配で相対影響が小さく、バイト律速のバルク ingest（大きなベクトル行）は
  影響が大きい、という見立て
- 有効時は書き込みトランザクションごとに基底の読み取りトランザクションを 1 本持つ。その期間は
  古いページの回収が止まるため、長い明示トランザクションほど DB ファイルの肥大につながる
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
  含む。実行できる主体と照会の可否は RECOVER-13 (4)(5) のポインタに従う。LSN・アーカイブ
  状態・遅延もセッションへ出さない（量から他テナントの書き込みを推測させない）
- fail-closed: 復元の拒否条件と容量上限到達時の扱いは RECOVER-14 (2)(3)(7) のポインタに従う。
  計装漏れは commit 前拒否とオラクルで検出
- 入力検証: セグメント・バックアップファイルは untrusted。長さ検証後に確保し（無制限
  `with_capacity` 禁止）、`checked_*` 演算を使い、未知の版数は拒否する
- 情報漏えい: エラー・ログに表名・テナント ID・行値・件数・LSN を出さない
  （`commit_boundary` の stderr 方針を踏襲）
- ファイル: 権限と出力先の扱いは RECOVER-13 (4)(7) のポインタに従う。実装は一時名から rename
- 秘密情報: ロールの検証子の扱いは RECOVER-13 (4)(5) のポインタに従う
- 脆弱な依存: チェックサム・エンコードは自前実装で依存を追加しない

## 7. 対象外・未決事項

- コード変更・依存追加はしない。実測値は書かない
- 申し送り（Issue 起票はしていない。要否は人の判断）: #2166 の分割（ラッパ移行の規模）、
  #2149 への commit 番号系列の要件、#2446 との timeline の整合
- #3069 の範囲: 変更ログを有効にした DB をフラグなしで再起動したときの検出（無効のまま書くと LSN の
  欠落になる）。また、エラーを `CommitFootprintUnrecordable` から分離するかの判断と、frame の上限値
  （キー数・バイト数・ペイロード）の見直し
- 未決: 移行 commit の扱いの詳細、保守 commit の削除件数上限の既定値と呼び出し契機、CLI フラグ名の最終形（#2166／#2167）
