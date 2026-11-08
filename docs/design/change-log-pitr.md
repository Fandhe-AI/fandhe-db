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
- 検証コード: オラクル試験（`storage::change_log::oracle`・`oracle_tests`、feature `change-log-oracle`）、
  クラッシュ試験（`tests/change_log_crash_atomicity.rs`）、計測（`benches/change_log_bench.rs`）。
  Issue #3069 で追加した（D10・D11・7 節）。それ以前の節は設計時点の記録で、変更ログの有効化は D11 に従う

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
     禁止する仕組み（clippy `disallowed-methods` か型）は #2166 で選ぶ（**#3068 で clippy
     `disallowed-methods` を採用**。`clippy.toml`、実装メモは 7 節）。`retain`／
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
- 書き出し: 一時名へ書く → fsync → 公開 → ディレクトリ fsync（公開は `rename` ではなく `hard_link`。理由は下の「D6 の実装判断」）。ファイル権限は RECOVER-13 (4) のポインタに従う
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
  確定は #2166／#2167。パースは既存 `*_opt.rs` と同作法（重複指定・不正値は起動拒否）。**確定内容は下の
  「D6 の実装判断（Issue #2167）」を正とする**

#### D6 の実装判断（Issue #2167）

実装は `storage::change_log::archive`（engine）・`change_log_opt`／`change_log_archive`（wire-server）。D6 の
決定（保守 commit・再試行の収束・水位以下だけを削除）はそのまま実装し、未決だった点と本節の差異を次に確定する。

**CLI（フラグ名・既定値）**

| フラグ | 意味 | 既定・前提 |
| ------ | ---- | ---------- |
| `--change-log-archive-dir <dir>` | アーカイブ先（既存のディレクトリ。`..` 成分・空・非ディレクトリは拒否し、絶対パスに直す） | 無効（指定した起動だけがセグメントを書き出す）。`--change-log` が前提 |
| `--change-log-retain-records <n>` | アーカイブ済みのレコードのうち新しい `n` 件を DB に残して、より古いものを刈り込む（`0` はアーカイブ済みをすべて） | **刈り込まない**（保持の既定は RECOVER-14 (7) のポインタに従う）。`--change-log-archive-dir` が前提 |
| `--change-log-archive-interval-secs <n>` | 書き出しの周期（1〜86400） | 10。`--change-log-archive-dir` が前提 |
| `change-log-verify --archive-dir <dir> [--db <path>]` | アーカイブの検査（サブコマンド。非 0 終了で欠落・改ざんを通知） | `--db` を付けると停止中の DB と突き合わせる |

- 前提を欠く組み合わせ（`--change-log` なしのアーカイブ系、アーカイブ先なしの保持・周期）は DB を開く前に
  起動拒否にする。**アーカイブされていない行を刈り込む設定を作れない**ことを CLI の段階でも保証する。
  エラー文は固定の英語で、パス・値を含まない
- 「保持上限」は **DB の `change_log` 表に残すアーカイブ済みレコードの件数**を指す。RECOVER-14 (7) が定める
  アーカイブ**ファイル**の保持と削除可能範囲は、フルバックアップの commit 番号を引ける仕組み（#2149／#2154／#2168）が無く削除可否を判定できないため
  **実装しない**。セグメントファイルは決して削除しない（fail-closed）。残課題として 7 節に記す
- 削除件数の既定: 1 回の保守 commit で **4096 件または 64 MiB** まで（`DEFAULT_PRUNE_RECORDS`・
  `DEFAULT_PRUNE_BYTES`。実装既定値で spec 由来ではない）。1 回の呼び出しで刈り込み専用の保守 commit を最大
  1024 回まで連ねる。セグメントのバイト上限の既定は 16 MiB（`DEFAULT_SEGMENT_BYTES`。1 件のレコードが
  超えるときは 1 件で 1 セグメント）

**呼び出し契機**

- wire-server の常駐スレッド（`change_log_archive`）が、起動直後に 1 回、以降は設定周期で
  `EngineCore::archive_change_log` を呼ぶ。利用者の commit への相乗りや post-commit フックは採らない
  （セッションのスレッドを止めず、容量上限で書き込みが拒否されている間も復帰できる）
- 保守 commit のゲートは `acquire_yielding`（`Storage::begin_maintenance_txn`）。通常の書き込みが待機している
  間は譲り、待機上限を超えたら**エラーにせず見送り**（`ArchiveReport::skipped`）、次の周期が再試行する
- 失敗は固定文言を stderr へ出し、同じ内容の連続は 1 回だけ出す。1 回の呼び出しで書き出すセグメントは最大
  256 個（残りは次の周期）

**セグメント形式**（#2168／#2169 の再生が読む。little-endian。整数は固定長）

```text
ヘッダ(66B): magic "FCLA"(4) | 形式版数 u16 (=1) | DB 識別子(16) | timeline u64 | 先頭 LSN u64 |
             末尾 LSN u64 | レコード数 u64 | ストレージ形式の版数 u32 | 本体バイト数 u64
ヘッダチェックサム(32B): SHA-256(ヘッダ)
本体: レコードの連結（`change_log` 表の値そのまま = D3 の `[ヘッダ | チェックサム | ペイロード]`。
      レコードごとに自己完結のチェックサムを持ち、LSN の昇順・欠番なし）
トレーラ(32B): SHA-256(ヘッダ + ヘッダチェックサム + 本体)
```

- ファイル名: `fandhe-cl-{DB 識別子 hex}-{timeline hex 16 桁}-{先頭 LSN 20 桁}-{末尾 LSN 20 桁}.seg`。
  名前はヘッダと一致しなければならず（食い違いは拒否）、決定的なので再試行の同名衝突を検出できる
- 本体は表の値を再エンコードせずに連結するため、D6 の「表から再構成したバイト列との比較」は全体チェックサムの
  比較で済む。読み手は本体を `HEADER_LEN`＋チェックサム長の固定長ヘッダ→ペイロード長（上限検証）→ペイロードの
  順に前方から読み、確保の前に長さを検証する（untrusted 入力）
- 空のセグメントは作らない。LSN の幅とレコード数が一致しないヘッダ・先頭 LSN が 0 のヘッダは拒否する

**欠落の検出（受け入れ条件）**

| 事象 | 検出 |
| ---- | ---- |
| セグメント内の LSN の飛び・`prev_lsn` の不連続・レコード数の不足 | `LsnGap`（ファイル単体の検証） |
| ヘッダ・レコード・トレーラのいずれかの改ざん・破損 | `ChecksumMismatch` |
| 途中切れ・余り | `Truncated`／`BadFormat` |
| 別 DB のセグメント・レコードの混入 | `ForeignDatabase` |
| 別 timeline | `ForeignTimeline` |
| セグメント間の飛び・重複 | `LsnGap`（前の末尾 + 1 == 次の先頭） |
| 先頭セグメントの紛失 | `LsnGap`（DB と突き合わせるとき、先頭は有効化時の世代の次） |
| 末尾のセグメントの紛失 | `LsnGap`（DB の水位に届かない。ディレクトリ単体の検証では連鎖が保たれるため検出できない） |
| シンボリックリンクのセグメント | `BadFormat`（通常ファイルに限る） |

- `Storage::verify_change_log_archive`（DB のメタ情報と突き合わせる）と `verify_archive_dir`（ディレクトリ
  単体）を公開し、運用者の `change-log-verify` が呼ぶ。エラーの `Display` は固定文言（パス・LSN・件数を含めない）
- 復元側の拒否（PITR・増分が同じ検証を通ってから適用する）は #2168／#2169 の責務。本形式の検証関数が
  その前段になる

**書き出しの公開（D6 からの差異）**

- 公開は一時名（`.fandhe-seg-{pid}-{seq}.tmp`、`create_new`・mode 0600）への書き出し → fsync → **`hard_link`
  で最終名へ公開** → 一時名の削除 → ディレクトリ fsync。D6 は `rename` と書いていたが、POSIX の `rename` は
  既存の宛先を黙って置換するため、「既存ファイルを上書きしない」を原子的に守れない（存在確認 → `rename` は
  競合する）。`hard_link` は宛先があれば `EEXIST` で失敗する（`export_snapshot` と同じ方式）。`hard_link` を
  提供しないファイルシステムは `Unsupported` で拒否する
- 一時名の残骸（公開前のクラッシュ）は、所有プロセスの識別がつかないため自動では削除しない（無害で、
  名前の接頭辞で運用者が掃除できる）。D6 の「再試行の先頭で掃除してよい」は任意の扱いだった
- **再試行の収束の実装**: 水位 + 1 を先頭 LSN に持つ既存セグメントがあれば、バイト上限から境界を再計算せず、
  そのファイルの範囲を採用する。ファイルを検証し、同じ範囲を表から流した全体チェックサムが一致すれば書き直さず
  水位だけ進める（`ArchiveReport::segments_adopted`）。一致しなければ `Conflict` で止め、上書きも削除もしない。
  セグメントのバイト上限を変えた再起動でも重複区間は生まれない
- 水位は DB のメタ表 `change_log_meta` のキー `archived_lsn`（LSN の u64）。無いときは有効化時の世代
  （`base_generation`）を水位とみなす。保守 commit は更新前に「現在の水位 == 呼び出しが見た水位」を検査し、
  食い違えば `WatermarkMoved` で abort する（並行するアーカイブの二重適用を防ぐ）

**刈り込みと連続性検査（`verify_continuity`）**

- 刈り込みは水位以下の行だけを、**水位更新と同じ保守 commit** で消す。削除対象の上限を `水位 - 保持件数` にして
  いるので、水位より後（未アーカイブ）は構造的に消えない。アーカイブ先が使えない・書き出しに失敗した間は
  水位が進まず、何も消えない
- **刈り込みの覆いの確認（fail-closed）**: 水位は DB 全体の値で、アーカイブ先が差し替わる・欠けることがある。
  保守 commit のたびに、今回消す範囲（先頭の行から最大 `max_prune_records` 件、かつ `水位 - 保持件数` 以下）を
  現在のアーカイブ先の検証済みセグメント（ヘッダ・各レコード・全体のチェックサム、名前との一致）が連続して覆う
  ことを確認し、覆えなければ刈り込まない（`ArchiveIncomplete`。水位の更新は行い、周期の最後にエラーを返す）。
  検証の範囲は消す範囲に掛かるセグメントだけで、費用は 1 回に消すバイト数（上限つき）に比例する。範囲外の
  セグメント（より新しい・より古いもの）はここでは検証しない（全体の連鎖は `change-log-verify`）
- **刈り込みの内容照合**: 覆いの確認は、セグメント自身の検証（チェックサム・LSN の連続）に加えて、消す範囲の
  各レコードを `change_log` 表の同じ LSN の値とバイト列で照合する。バックアップの複製は同じ DB 識別子・
  timeline を持つため、複製から分岐した系列のセグメントは自己整合でも内容が違いうる。不一致は `Conflict` で、
  元のログを消さない。照合は検証と同じ範囲（1 回に消す件数・バイトの上限内）で、メモリは 1 レコード分
- **公開の検証（差し替え対策）**: 一時ファイルの fd を保持し、fsync の後・`hard_link` の直前に一時パスの
  (dev, ino, 長さ) が fd と一致すること、`hard_link` の直後に公開先の (dev, ino, 長さ) が一致することを確認する
  （#3075 の `TempCopy::check_path`・`is_same_file_at` と同じ考え方）。不一致は `TempReplaced` で、公開後なら
  自分が作ったリンクを削除して取り消し（削除に失敗したら固定の英語文を stderr へ出す）、水位を進めない。
  既存ファイルを採用する経路は内容照合済み
- **アーカイブ先ディレクトリの権限**: グループまたはその他が書き込めるディレクトリは、スティッキービットの
  有無に依らず書き出しを拒否する（`InsecureDirectory`。起動時にも同じ検査で拒否）。セグメント名は DB 識別子・
  timeline・LSN 範囲から決まるため、書ける他者が次の範囲の名前のファイルを置く・既存のセグメントを
  `hard_link` するだけで書き出しを `Conflict` で止められる（スティッキービットは削除・改名を防ぐが作成は防がない）。
  fd の検証と合わせた二重の防御。運用者は umask が 002 のシェルで作ったディレクトリ（0775）を 0755 にする必要が
  ある。読み取り（検証）には課さない。**所有者の検査はしない**: 実行ユーザーの euid を標準ライブラリだけでは
  得られず（依存の追加は不可）、他者が所有するディレクトリで自分が書けるのは、所有者がグループ・その他の
  書き込み権限を与えたか ACL による場合で、前者は上の検査で拒否される。ACL は検査しない（運用で担保する）
- **セグメント一覧の共有**: セグメントの一覧は周期の開始時に 1 回だけ取得し、周期内で公開した分を反映して
  計画（`plan_next`）と刈り込みの覆い確認（`prune_cap_for`）で共有する。ファイルを自動削除しないため、毎回の
  列挙・整列は運用開始以来の全ファイル数に比例する。周期の途中で他者が足したファイルは一覧に載らないが
  安全側に倒れる（計画は同名ファイルを `hard_link` の `AlreadyExists` と内容照合で検出し、刈り込みは覆いと
  認めず `ArchiveIncomplete` で止まる）
- **採用経路の永続化**: 公開直後にディレクトリの fsync で失敗した周期の次は採用経路を通る。採用でも、水位を
  進めて刈り込む前に、セグメントファイルと親ディレクトリの fsync の成功を確認する
- **周期の直列化**: 計画から公開・水位更新までを `ChangeLogState` のロックで直列化し、同じ水位から別の末尾を
  計画する並行呼び出し（上限設定の違い・読み取りの間の利用者 commit）が重複区間を残さない。別プロセスからの
  並行は、redb が DB ファイルを単一プロセスで排他的に開くため起きない
- 一時名は `.fandhe-seg-{pid}-{乱数}-{通番}.tmp`（乱数は `RandomState` と時刻のナノ秒）。同じ PID の残骸が
  多数あっても書き出しは進む
- セグメントはストレージ形式の版数が変わる境界で切る（1 セグメントは 1 つの版数。版数が混在する区間で
  アーカイブが止まらない）。
- 保守 commit は世代も表世代も進めず、レコードも生まない。後続の利用者 commit の `prev == storage_generation` は
  保たれる（テストで、刈り込み → 利用者 commit の連鎖とオラクルの検証を固定した）。オラクル（`verify_record`）は
  利用者 commit の prepare でのみ走るので保守 commit とは矛盾しない。再生の通し検証（`verify_log_replay`・
  `oracle_tests` の `finish`）は表の全行を前提にするため、刈り込みと同じ DB では使わない（刈り込みは opt-in）
- **`verify_continuity`（再有効化の連続性検査）は「最後のレコードの LSN、表が空なら水位、水位も無ければ
  `base_generation`」と現在の世代を比べる。** 全件を刈り込んで表が空になっても水位が最後の LSN を保つため、
  再有効化は拒否されない。#3069 の申し送り（最後の LSN をメタ表に残す）は、新しいキーを足さず水位で満たした。
  無効のまま書かれた区間（LSN の欠落）は水位とも一致しないので、従来どおり拒否される（テストで固定）
- `export_snapshot` の複製には `change_log` 行と水位も含まれる（D5）。刈り込みを有効にした DB の複製は
  行が少ない。複製を別のアーカイブ先で起動する運用（復元後の継続）は #2168 の論点で、同じ DB 識別子・timeline の
  別系列を同じディレクトリへ書くと、同名で内容が違うセグメントを `Conflict` で止める（黙って置換しない）

**無効化**: 無効化（有効にした DB をフラグなしで開く手段）は実装しない。D8・D11 のとおり、有効にした DB の
フラグなし起動は LSN の欠落を作れないため拒否する現状を維持する。無効化は LSN の連鎖を意図的に切る操作で、
増分・PITR（#2168／#2169）の連鎖検証の仕様と一体で決めるべきものであり、本 Issue の範囲を超えるため残課題とする。

#### D6 の実装判断（Issue #3091）: 未アーカイブ量の上限

RECOVER-14 (2) のポインタ（上限の超過で書き込みを拒否する）を実装した。実装は
`storage::change_log::backlog`（engine）・`change_log_opt`／`change_log_archive`（wire-server）。

- **測り方**: 件数は LSN が連続し 1 commit が 1 レコードなので `(g_c - 1) - 水位` で求める（状態を持たない。
  prepare を通らない経路の後でも正しい）。バイトはメタ表の `unarchived_bytes`（`(水位, stamp]` の行の値の長さの合計）と
  `unarchived_bytes_lsn`（stamp）で持つ。利用者 commit が行を足す同じトランザクションで加算し、保守 commit が水位を
  進めるときに書き出したセグメントの本体バイト数を引く。stamp が無い・不正・`g_c - 1` と合わないときは
  `(水位, g_c - 1]` を走査して数え直す（復元・レプリカ・上限を設定していなかった期間）。インメモリの
  カウンタは使わない（commit の `Err` は結果不明で、過大に数えたまま戻せないため）。上限を設定していない
  既定の `Storage` は prepare でもメタ表を読み書きしない
- **拒否の条件**: 「この commit を加える**前**」の量が上限以上のとき、prepare の先頭（after-image を組む前）で
  `StorageError::CommitFootprintUnrecordable`（`54000`。D11 のとおり variant は足さず、原因は `Unrecordable::UnarchivedLimit`
  の固定文言を stderr へ）で拒否する。「加えた後」で判定すると上限より大きい 1 レコードが空の backlog でも
  通らなくなるため、超過は最大 1 レコード分（`max_payload_bytes` 以下）まで許す。拒否は commit 前なので世代・
  レコード・データは確定しない。アーカイブが水位を進めれば、次の commit から再起動なしで再開する
- **対象外**: 保守 commit・レプリカの適用・`enable_change_log` 内の識別子の保守 commit と旧形式の表の移行は
  拒否しない。上限は移行が終わった後に設定するので、上限を超えた DB の再起動が移行の拒否で失敗しない
- **飢餓（7 節 ⑤）**: 拒否された書き手の再試行が続くと `acquire_yielding` の保守 commit がゲートを取れない。
  上限に達している（拒否の印・件数・バイトのいずれか）ときは、保守 commit が `acquire`（通常の書き込みと
  同じ並び）でゲートを取る
- **既定値と CLI**: `--change-log-max-unarchived-bytes <n>`・`--change-log-max-unarchived-records <n>`（1 以上。
  重複・0・不正値・`--change-log-archive-dir` なしは起動拒否）。`--change-log-archive-dir` を指定した起動では
  バイト上限に 4 GiB を適用する（実装既定値で spec 由来ではない）。**アーカイブ先なしの `--change-log` には上限を
  適用しない**: 全レコードが未アーカイブになり、到達後は書き込みが永久に止まって回復できず、`--change-log` だけで運用する
  既存の構成（レプリケーションの送信側など）の挙動も変わるため。spec 側が「アーカイブなしの構成にも適用する」と
  読む場合とのずれは、spec 側の判断として残課題にする
- **観測（運用者専用）**: 拒否のたびに stderr へ固定文言（原因ごと・同じ秒に 1 回まで）。常駐スレッドは各周期の後に
  上限の到達・解消が変わったときだけ固定文言を 1 行出す。数値は engine API
  `Storage::change_log_unarchived_status`（`EngineCore` も同名）だけで、SQL・NoSQL の表層・`SHOW`・エラー詳細には
  出さない（他テナントの書き込み量を推測させない）。クライアントへのエラーは他の拒否と同じ `54000` の固定文言

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

#### D7 の実装判断（Issue #2168）

増分バックアップの出力・連鎖検証・適用を実装した（`crates/engine/src/storage/change_log/incremental.rs`）。
PITR の停止条件（#2169）は含まない。

- **形式**: D6 のセグメント形式を**そのまま再利用**する（新しい形式は作らない）。増分バックアップ = ベースの終点
  の次から出力時点の最新までを覆う、**1 ファイルのセグメント**。1 ファイルにするのは、トレーラ（全体チェックサム）
  が「欠けなく書き切れた」ことの証拠になるため（複数ファイルだと末尾側の欠落を受け側が検出できない）。ファイル名は
  任意（運用者が決める）。**アーカイブ先のディレクトリには置かない**（既存のセグメントと LSN 範囲が重なるため、
  `change-log-verify` が `LsnGap` で失敗する）
- **境界**: アーカイブのセグメントはバイト上限で切るため、ベースの終点は既存セグメントの途中に落ちうる。
  そこで既存のセグメントは流用せず、DB の `change_log` 表から**ベースの終点の次ちょうど**で切り直して書く
  （先頭 LSN = ベースの終点 + 1 を厳密に満たす）。書き出しは同じ読み取りトランザクションを 2 周する逐次処理
  （1 周目で連続性・本体バイト数・ストレージ形式を検査、2 周目で書く）で、メモリは 1 レコード分。公開は
  D6 と同じ `publish_with`（一時名 0600 → fsync → 差し替え検出 → `hard_link` → ディレクトリ fsync。既存の宛先は
  上書きしない）で、アーカイブ先と同じく他者が書けるディレクトリへは書かない（`InsecureDirectory`）
- **ベースの終点（`ChainPoint`）**: フルバックアップ（`export_snapshot` の出力。`generation` 表と
  `change_log_meta` を複製済み）は読み取り専用で開いて (DB 識別子, timeline, `storage_generation`)。前回の増分は
  全体を検証して (ヘッダの DB 識別子, timeline, 末尾 LSN)。ファイルの種別は先頭のマジックで判別する。変更ログの
  メタ情報が無い DB は `NotEnabled`
- **連鎖検証（出力時）**: 出力元とベースの DB 識別子・timeline が一致（`ForeignDatabase`／`ForeignTimeline`）、
  ベースの終点が出力元の最新以下（超えれば `ChainBroken`。別系統の複製）、最新と等しければ `EmptyRange`、
  `change_log` 表がベースの終点の次から最新までを欠けなく覆う（刈り込み済み・欠落は `RangeUnavailable`）。
  いずれも何も公開しない。刈り込み済みの区間をアーカイブから補う経路は持たない（残課題）
- **適用**: `apply_incremental_segments(db_path, segments)`。`db_path` は呼び出し側が用意したベースの**複製**
  （元のバックアップは変更しない）。素の `redb::Database` で開き（`Storage::open` は変更ログ有効化済みの DB への
  書き込みを拒否し、移行・修復の副作用もあるため使わない）、次の順で進める
  1. DB に触れる前に全セグメントを `verify_segment_file` で検証し、増分同士の連鎖（同一 DB・timeline・
     前の末尾 + 1 == 次の先頭）を確かめる。**並べ替えはしない**（順序違反は `ChainBroken`）
  2. ベースの DB 識別子・timeline・`storage_generation` + 1 == 先頭 LSN を確かめる
  3. レコードごとに `prev_lsn == 直前に適用したレコードの LSN`（初回はベースの世代。表を読み直さず、適用側で追跡した値）・
     ストレージ形式の版数・DB 識別子・timeline を検査し、
     `dropped` の削除 → `created` の作成 → `tables` の `removes` → `upserts`（`table_image::apply_change`）で適用する。
     表の有無の不整合（無い表の削除・既存の表の作成・無い表の変更）、範囲外の shape、記録範囲外の表
     （`change_log`・`change_log_meta`）への言及は `BadRecord`。チェックサムは破損検出であって認証ではないため、
     検証済みでもメタ表の書き換えは受け付けない
  4. 適用したレコードを**適用先の `change_log` 表へも追記**する（キー = LSN、値 = レコードのバイト列）。記録範囲の
     例外の表は再生では復元されないため、追記しないと再有効化の連続性検査（最後のレコードの LSN == 世代）が
     必ず壊れる。追記により、適用後の DB は最新の DB と同様に `--change-log` で起動できる
  5. 途中の commit は `Durability::None`（公開前の一時コピー。256 件または 8 MiB ごと）、最後に空の Immediate
     commit で fsync し、**このときに初めて** `generation` 表を読んで、世代が末尾 LSN に達していなければ `ChainBroken`（世代の表を含まないレコードの fail-closed）
  6. 検証後・適用中にセグメントが差し替えられていないことを、ヘッダの一致と全体チェックサムの再計算で確かめる
- **キー・値のバイト列の事前検証**: redb の `from_bytes` は不正な長さ・UTF-8 で panic するため、`table_image` の
  shape ごとに検証関数（固定長は長さ一致、`&str` は UTF-8、タプルは redb の符号化〔最後以外の可変長要素の varint
  長を先頭に並べる〕に沿った範囲確認）を持たせ、`apply_change` が適用前に必ず呼ぶ（不正は `BadRecord`）。`shapes!` は型の組に
  検証用 trait の実装を要求するので、新しい型の組は検証なしに登録できない。`catch_unwind` には頼らない
- **信頼境界（認証ではない）**: セグメントのチェックサムは鍵なしの SHA-256 で、破損・不整合の検出に限る。細工して
  計算し直したファイルは検証を通るので、増分はベースの DB ファイルと同じ信頼境界で受け渡す。表名の許可リストを
  カタログ側で持つ案は採らなかった（カタログの表名は DDL で動的に決まり、増分は DDL による作成・削除・rename を含むため、
  静的な許可リストは正当な増分を拒否するか形骸化する）。代わりに記録範囲外の表（`change_log`・`change_log_meta`）への
  言及を拒否し、適用後は通常の復元と同じ整合検査を通す
- **メモリ**: 「メモリは 1 レコード分」は出力側。適用側は 1 レコード（上限 `max_payload_bytes` = 256 MiB）を
  読み、復号して表現を作るため、そのレコードの数倍になりうる
- **分岐の未検出**: 復元した DB は元と同じ DB 識別子・timeline を持つため、別系統の増分を誤って適用しても LSN が
  連続していれば検出できない（#2446 の timeline 分岐待ち）。運用手順書に注意として記載した
- **失敗時**: 適用先の複製は途中まで適用された状態になりうるため呼び出し側が破棄する。wire-server の
  `restore --incremental` は既存の復元と同じ一時名 → 検査 → `hard_link` 公開の手順に適用を挟むので、失敗時は何も
  公開せず一時ファイルも消す。復元後の世代番号の進行（#2446）は未接続で、呼び出し位置は `restore_opt` のコメントに
  ある（適用の成功後・検査の通過後・公開の前）
- **CLI**: `wire-server backup-incremental --db <停止中の DB> --base <直前のベース> --out <新規パス>` は DB を
  読み取り専用で開く。`restore --from <ベース> --db <新規パス> --incremental <増分>...` は `--incremental` を
  古い順に繰り返す。終了コードは `backup` と同じ規則（引数・出力先の拒否は 2、連鎖の切断などの実行時の失敗は 1）。
  エラー文・成功時の出力はレコード数以外に LSN・件数・パスを含めない
- **#2169 との共有**: 再生器は「検証済みセグメント列 → 実表への適用」を 1 関数にまとめてある。PITR は同じ列（ベースの
  終点以降のアーカイブのセグメントと増分）に、コミット境界での停止条件（LSN・時刻）を足せばよい。レコード単位の
  適用（`apply_body`）と連鎖の検査は共有できる

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
  採否根拠にしない）に従う。実測は D11 の「性能の実測」に記録した（Issue #3069）

### D10 テスト方針（#2166〜#2170 への申し送り）

- D1 のオラクル試験
- ラウンドトリップ: ベース + 再生の結果を `table_image::capture` で元と比べ、差分が空であること
- 欠落・順序違反・改ざん・別 DB／別 timeline の拒否
- `scripts/crash_test*.sh` 系への PITR シナリオ追加（#2170。`scripts/crash_test_pitr.sh` を追加し、`crash-test` の前提ターゲットとして既存の CI ジョブで実行する。内容は [backup-restore-operations.md](backup-restore-operations.md) の 7.1 節）
- テストは RECOVER-14 等の ID に紐付ける

### D11 有効化の仕様・オラクル・エラー・性能の実測（Issue #3069）

Issue #2166 の完了条件に当たる実装メモ。D1 の層 3・D8・D9・D10 を満たす。

#### 有効化と再起動の扱い

| 状態 | 起動の扱い |
| ---- | ---------- |
| 一度も有効にしていない DB・フラグなし | 従来と同一（識別子の読み取り 1 回だけが増える） |
| 一度も有効にしていない DB・`--change-log` | DB 識別子・timeline・**有効化時の世代**（`base_generation`）をメタ表へ書く（世代を進めない保守 commit）。以降の commit からレコードを持つ。それ以前の変更はログに無い（D8） |
| 有効にした DB・`--change-log` | 記録の連続性を検査する（最後のレコードの LSN。レコードが無ければ `base_generation` が、現在の `storage_generation` と一致すること）。不一致は LSN の欠落で、起動を拒否する（書き込みも拒否されたまま） |
| 有効にした DB・フラグなし | wire-server は固定文言で起動を拒否する。engine（ライブラリ利用）も書き込みを `54000` で拒否する（`wrap_gated` で書き込みトランザクションの取得時。読み取りと `backup` の書き出しは可）。無効のまま書くとレコードが欠けたまま世代が進むため |

- 有効にした DB をフラグなしで開く経路は、LSN の欠落を**作れない**（起動拒否 + engine 側の書き込み拒否の二重）。
  有効化を戻す手段は現時点で無い。`backup`（`export_snapshot`）の複製も `change_log`・`change_log_meta` を
  含む（D5: ベースバックアップとログを同じ DB 識別子で結ぶため、除外しない）ので、複製も有効扱いで、
  `--change-log` 付きで起動する必要がある。無効化の仕様はアーカイブ・保持（#2167）以降で決める
- 旧形式の表の `id` 移行（`legacy_id_migration`）は `Storage::open` が commit する。有効にした DB では
  その commit が記録されないとレコードに欠落ができるため、open では行わず、`enable_change_log` が
  有効化の直後（記録される状態）で行う。フラグなしの起動は書き込めないので移行を要さない
- `enable_change_log` は公開 API（`&mut self`。`Arc` 化前の構築段階でだけ呼べる）。一時表ストレージは拒否する

#### エラーの分離（判断）

**`StorageError::CommitFootprintUnrecordable`（`wire_code` は `54000`）のまま分けない。** 理由:

1. `StorageError`・`CatalogError` は `#[non_exhaustive]` ではなく、variant の追加は公開 API の
   破壊（`OpenWithEngineError` を別型にした前例と同じ判断）。写像の腕も `sql/ddl.rs` の 16 箇所ほか
   `catalog.rs`・`exec.rs`・`transaction.rs` に及ぶ
2. クライアントへ返す `wire_code` は `54000` に固定すべきで（他テナントの存在・件数を漏らさない）、
   クライアントが原因を区別できる必要はない。区別が要るのは運用者だけである
3. 運用者への区別は、拒否の原因ごとの固定文言を stderr へ 1 行出すことで足りる
   （`storage::change_log::Unrecordable`。frame の上限／multimap 表／捕捉を通らない書き込み／after-image を
   読めない／レコードが上限超過／世代・時計の失敗／有効にしていない起動。表名・件数・LSN は含めず、同じ秒の
   重複は出さない＝利用者の連続した拒否でログを溢れさせない）

#### オラクル試験（D1 の層 3）

- 実装: `storage::change_log::oracle`。`CaptureSession::prepare` がレコードを追記した直後に
  `verify_record` を呼ぶ。検査は、(1) レコードの LSN が基底の世代の次、(2) 作成・削除された表の集合が
  基底と最終の表集合の差と一致、(3) 作成（再作成）された表は全行が書き込みトランザクションの内容と一致、
  (4) `table_image::capture`（基底 → 書き込みトランザクション）の差分の upsert・remove がすべてレコードに載る、
  (5) レコードの値が書き込みトランザクションの現在値と一致、の 5 点。(1)〜(5) は「レコードを基底へ適用すると
  commit 後の状態になる」と同値である（レコードの余分な upsert は値が同じ no-op、余分な remove は基底に無いキー）。
  不一致は panic（テスト専用）
- 有効化の条件: `cfg(test)` では変更ログが有効な commit で常に検証する。結合テストは feature
  `change-log-oracle` と環境変数 `FANDHE_CHANGE_LOG_ORACLE`（`1`＝強制有効化 + 検証、`force`＝強制有効化のみ）
  を両方付けた実行に限り、永続 `Storage` の open で強制的に有効にする。feature だけでは挙動が変わらない
  ため、`--all-features` の CI（`make lint`／`make test`）を壊さない
- 場面集（`oracle_tests`）: DDL 全般・INSERT／UPDATE／DELETE／ON CONFLICT・列の追加と改名・表の rename・
  DROP と同名の再作成・TRUNCATE・索引（B-tree／一意／GIN）の作成と削除・ビュー／マテリアライズドビューと
  REFRESH・シーケンスと `setval`・`TRUNCATE ... RESTART IDENTITY`・トリガ（テスト用の呼び出し口。トリガ内の
  書き込みを含む）・関数・明示トランザクションと SAVEPOINT／ROLLBACK TO／RELEASE・一時表・スキーマ／ロール／
  ポリシー／GRANT／ENUM 型・分割実行・operation_id 台帳と複数テナント・`put`／`put_batch`／`BatchWriteTxn`／
  `WriteTxn`。各場面の終了後に、有効化した時点の全表へ記録した全レコードを LSN 順に適用した結果が最終状態と
  一致すること（再生器 `oracle::replay`。符号化・LSN の連鎖・正規化の通し検証）と、レコード数が世代の進みと
  一致することも確かめる。再生器は #2169 の参照実装を兼ねる
- **検出できないもの**: 記録・突き合わせ・再生は同じ `table_image` の読み取り（shape の符号化）を共有する。
  shape の符号化そのものの誤り（キー・値のバイト列の取り違え）は 3 つとも同じ向きにずれて見逃しうる。
  補うため、行ストアの実体を素の redb で直接読み、キー・値のバイト列が再生結果と一致する場面
  （`replayed_row_store_matches_a_direct_raw_redb_read`）を 1 つ置いた。他の shape は未検証
- オラクル自身の検出力: 層 2（行数の増減）が見逃す「行数が同じ更新を捕捉なしで行う」書き込みを、オラクルが
  不一致として検出することをテストで固定している
- 場面集では**不一致は出なかった**（直した不一致は無い）。一方、強制有効化で見つかった層 3 外の問題が 1 件
  あった: open の旧形式移行 commit が記録されず LSN の欠落になる（上記「有効化と再起動の扱い」で修正）
- 結合テストの強制走行（`FANDHE_CHANGE_LOG_ORACLE=1`、engine の `tests/` の 99 バイナリ・8,127 件）の結果は
  下の「強制走行の結果」

#### 強制走行の結果

- engine の `tests/` を `FANDHE_CHANGE_LOG_ORACLE=1 cargo test -p fandhe-db-engine --features change-log-oracle
  --no-fail-fast --test '*'` で流した（99 バイナリ。テスト実行 3 分 27 秒、10 論理 CPU、ビルド別）。
  **8,106 件が成功、6 件が失敗、15 件が ignore（もとから）**。捕捉漏れ・層 2 の誤拒否・オラクルの不一致は
  0 件だった。失敗 6 件の内訳:
  - `table_rls/table_generation_bump_coverage`（1 件）: 許可リストの行番号の追随だけ（解消済み）
  - `catalog` の `table5_alter_table_add_column_latency_is_independent_of_row_count`、
    `catalog_latency` の `table4_create_table_latency_is_independent_of_row_count`、
    `index7_create_index_latency` の 2 件（4 件）: 「行数が増えても遅延が増えない」を測るテストで、`=1` の
    オラクルは commit ごとに O(DB サイズ) の突き合わせをするため落ちる。**`FANDHE_CHANGE_LOG_ORACLE=force`
    （強制有効化のみ）で流すと 4 件とも成功する**ので、有効時の実コストが DB サイズに依存しないことの確認になっている
  - `persistence` の `persist3_on_disk_row_entry_layout_via_raw_redb`（1 件）: 「redb の表は `rows` と
    `storage_generation` だけ」と固定している。有効化そのものが差になる（`change_log`・`change_log_meta` が増える）
    ため、期待値は変えていない
- 単体テスト（lib）を同じ条件で流すと 6,563 件成功・47 件失敗だった。内訳は、素の redb で書く破損・直書きの
  fixture 26 件（オラクルが「記録されていない変更」として検出するのが正しい）、素の `db().begin_write()` の
  commit で世代だけが進む fixture 6 件（`commit_boundary` の subprocess 試験など。連続性検査が拒否する）、
  有効化そのものが差になる構造テスト 8 件（commit 回数・生成物の有無を固定する）、無効または明示の有効化を前提にした
  変更ログ自身のテスト 7 件。いずれも期待値は変えていない
- 通常実行（feature・環境変数なし）では、lib 6,610 件（オラクルの場面集を含む）と結合テストはすべて成功する
- 強制走行の対象外: wire-server のテスト（`--change-log` を付けた起動は `wire_cli` が外形で見る）。
  COPY・wire プロトコル固有の書き込み経路は強制走行では流していない（engine の同じ書き込み関数を呼ぶ）

#### 原子性

- 構造: レコードの追記（`change_log` 表への insert）は世代採番と同じ redb トランザクションの prepare 段で行い、
  point of no return（`commit`）の前に失敗すれば何も残らない。`commit` が `Err` の場合は従来どおり abort
- 検証: (1) `rejected_prepare_leaves_no_record_no_data_and_no_generation_change` ほか既存の lib テストと、
  `commit_boundary` の subprocess 試験（`commit_fn` の Err／パニックで abort）が有効時も成り立つ、
  (2) `tests/change_log_crash_atomicity.rs`: 変更ログを有効にした子プロセスが DDL・DML・TRUNCATE・rename の
  commit を回し、親が 24 回 SIGKILL する（同じ DB を kill のたびに再有効化）。kill 後に `change_log` の LSN が
  欠落なく連続し、最後の LSN が `storage_generation` に一致し、最初の LSN が `base_generation` の次であること、
  再有効化が連続性検査に拒否されないことを確かめる（`Immediate` の durability。24 回で世代 455 まで進んだ）

#### CI ジョブ `change-log-oracle`

- すでに通常の CI（`rust-ci` の `cargo test --all-features`）に入っているもの: lib のオラクルの場面集（commit ごとの
  突き合わせ込み）、`tests/change_log_crash_atomicity.rs`、`wire_cli` の `--change-log` の試験
- 重い強制走行（99 バイナリ）は `ci.yml` の独立ジョブ `change-log-oracle`（`make change-log-oracle` →
  `scripts/change_log_oracle.sh`）。2 段: ①`FANDHE_CHANGE_LOG_ORACLE=1` で、`persist3_on_disk_row_entry_layout_via_raw_redb`
  （redb の表一覧を固定していて、有効化そのものが差になる）と `*_latency_is_independent_of_row_count` の 4 件
  （O(DB) の突き合わせで落ちる）をテスト名で除外して流す。②`=force`（強制有効化のみ）で遅延 4 件だけを流す。
  除外の理由はスクリプト冒頭とジョブのコメントに書いた。必須チェックへの追加はマージ後に ruleset で行う（ユーザー作業）
- 手元の所要時間は PR 本文に記載する（cold ビルド込み）。手元の実行手順は README「変更ログのオラクル試験」

#### 性能の実測

実測は `benches/change_log_bench.rs`（情報提供専用。公開 API だけを使う）で、無効と有効を**交互に 5 ペア**実行し、
中央値と範囲を併記した（`benchmark-judgement-policy.md`）。環境: Linux x86_64・10 論理 CPU・AVX2。**共有環境の
開発機での値で、採否根拠ではなく規模感の記録**である。DB は実ディスク（ext4）に置いた（`/tmp` は tmpfs で
fsync が無いため。tmpfs の値は CPU コストの上限として併記）。

| 場面 | 無効 | 有効 | 比 |
| ---- | ---- | ---- | -- |
| 1 行 commit の p50（実ディスク、`tenant::insert_rows` 1 行、1,500 commit） | 400 µs [376..1227] | 420 µs [405..426] | 1.05× |
| 同 p95 | 741 µs | 1,037 µs | 1.40× |
| 1 行 commit の p50（tmpfs＝fsync なしの CPU コスト） | 31.9 µs | 59.3 µs | 1.86× |
| 大きな一括 ingest（1,000 行/commit × 20 commit、dim 128、実ディスク）のスループット | 200,613 行/s [178,167..211,852] | 129,086 行/s [121,403..130,876] | 0.64× |
| 同 DB ファイルのサイズ | 16.1 MiB | 48.1 MiB | 2.99× |
| 表が 100 個ある DB の 1 行 commit（中央値） | 0.371 ms | 0.417 ms | 1.12× |
| 表が 1,000 個 | 0.376 ms | 0.534 ms | 1.42× |
| 表が 5,000 個 | 0.392 ms | 0.999 ms | 2.55× |
| 表が 20,000 個 | 0.383 ms | 2.724 ms | 7.11× |

- 1 行 commit は fsync が支配的で、有効時の相対コストは小さい（p50 で 5%、p95 で 1.4 倍）。バイト律速の一括 ingest は
  行データを表とログに二重に書くため、スループットは 0.64 倍、ファイルは約 3 倍になる。ファイルが D9 の見立て
  （2 倍前後）より大きいのは、書き込みトランザクションごとに基底の読み取りトランザクションを持つ間は古いページを
  回収できず、レコード（1 commit = 1 値で数百 KiB）の分も新しいページになるため
- **表数依存（層 2 の `verify_footprint`）**: 世代が変わった表の数ではなく、**世代管理対象の表の総数 T** に比例する
  （`changed_table_generations_in_write_txn` が世代表の全エントリを走査する。`list_tables` の取得は redb 上の
  表が少ないと数 µs で、主要因ではなかった）。当初の実装は走査の各エントリで確定側の世代表を開き直しており、
  T = 5,000 で 2.85 ms（無効の 7.13 倍）だった。確定側の表を 1 回だけ開き、両側をキー順に突き合わせる実装にして
  T = 5,000 で 1.0 ms（2.55 倍）に下げた（結果は同じ。単体テストで点引きと一致を固定）。残りは世代表の走査
  O(T)（約 0.13 µs/表）で、表が数千ある DB の有効時の commit 遅延は 1 ms 前後、2 万表で 2.7 ms。さらに下げるには
  世代表の差分を frame に積んだキーだけで検査する必要があるが、それは層 2 が検出したい「捕捉を通らない世代の bump」
  を検出できなくなる（完全性は層 1・3 が負うが、早期検出の意味が失われる）ため採らない。#2167 以降で問題になれば再検討する
- **frame の上限と一括 ingest**（`limits` モード。dim 128、行ごとの after-image は約 0.55 KiB、実ディスク）:

  | 1 commit の行数 | ペイロード見積 | 有効時 | 最大 RSS（入力データ込み） | 無効時 |
  | --------------- | -------------- | ------ | -------- | ------ |
  | 100,000 | 55 MiB | 成功（0.69 s） | 338 MiB | 成功 |
  | 300,000 | 165 MiB | 成功（2.2 s） | 1,057 MiB | 成功 |
  | 450,000 | 247 MiB | 成功（3.4 s） | 1,453 MiB | 成功 |
  | 600,000 | 329 MiB | **拒否（`54000`）** | 1,218 MiB | 成功（2.1 s） |

  dim 1536 では 20,000 行（118 MiB）は成功（最大 RSS 658 MiB。無効時は 408 MiB）、100,000 行（592 MiB）は拒否される。
  拒否の stderr は `engine: change log: commit refused: after-image unreadable or over the payload limit`。
  ペイロード上限（256 MiB）が先に効き、キー数（4M 個）とキーのバイト数（128 MiB）は dim 128 の行では届かない。
  **既定値（キー 4M 個・128 MiB・ペイロード 256 MiB・イベント 65,536 個）は変えない**。理由: ①最大 RSS の増分
  （ベンチ自身が持つ入力を除く）はペイロードの約 5 倍（after-image・符号化・frame・redb の書き込みバッファ）で、
  上限を上げると 1 commit の一時メモリが比例して増える。書き込みは単一ライタなので同時には 1 commit 分だが、
  247 MiB のペイロードで既に約 1.2 GiB 増える ②拒否は
  commit 前で何も反映されず、クライアントは `54000`（payload too large）で書き込みを分割できる。無効時に通る
  巨大な 1 commit が有効時に拒否されるのは意図した差で、運用では `--max-insert-rows` を下げるか、書き込みを分割する
  （README に記載）
- 無効時の既存ベンチに退行はない: 基準（#3068 直後）と本変更の `ingest_profile_bench` を交互に 5 ペア実行した。
  batch モードの E0（`tenant::insert_rows` 1,000 行/バッチ）は基準 4.915 ms [4.769..5.063]・本変更 4.958 ms
  [4.907..5.008]（+0.9%、基準側のばらつきの範囲内）。single モード（25,000 文）は E0 が 0.392 → 0.390 ms、
  S0（SQL 表層）が 0.414 → 0.412 ms、I8（redb commit）が 0.368 → 0.364 ms で、差はノイズの範囲内（比 0.99〜1.00）。
  無効時に増えたのは `Storage::open` の識別子の読み取り 1 回と `wrap_gated` の分岐 1 つ（確保なし）だけ
- `benches/ingest_profile_bench.rs` の素の redb の呼び出しは、計装レプリカ（I1〜I8＝基準側）の関数だけで書き込む。
  E0 側は `Storage::open`・`create_table`・`tenant::insert_rows`（engine の公開 API）で、素の redb は整合性検証の
  読み取り専用の open だけであることを確認した。したがって有効化しても基準側の測定は変わらない
- 手元の再現: `cargo bench -p fandhe-db-engine --bench change_log_bench -- single n=2000 pairs=5`、
  `-- tables tables=5000`、`-- bulk rows=1000 dim=128 batches=20`、`-- limits rows=450000 dim=128 log=1`。
  実ディスクで測るときは `TMPDIR` を実ディスクの空きに向ける

### D12 PITR 復元（Issue #2169）

指定 LSN／時刻のコミット境界までの復元を実装した（`crates/engine/src/storage/change_log/pitr.rs`。ポインタ: RECOVER-14）。
再生器は #2168 の `replay_segments` を共有し、停止条件とアーカイブの重なり読み飛ばしを足しただけである（二重実装しない）。

- **入力**: ベースの DB ファイルの複製（`restore` が一時名へ作る）、増分バックアップ（古い順。省略可）、変更ログのアーカイブの
  ディレクトリ（省略可）。どちらも無ければ `BadSegmentList`。増分とアーカイブの両方があれば、増分の末尾より後ろをアーカイブから
  補い、重なる区間（`lsn <=` 適用済み）は読み飛ばす。アーカイブのセグメントはバイト上限で切るため、ベースの終点がセグメントの
  途中に落ちても使える
- **停止点**: 1 レコード = 1 commit（D3・D4）なので、停止は commit 境界でしか起こらない。明示 Tx は 1 レコードで、丸ごと適用される
  か丸ごと適用されない
  - LSN 指定 `n`: LSN `n` のレコードを適用した直後で止まる
  - 時刻指定 `T`: レコードのヘッダに記録済みのコミット時刻（UTC マイクロ秒）が `T` を超える**最初の**レコードの直前で止まる。
    `<= T` のレコードを全部適用するのではなく commit 順の接頭辞なので、壁時計が逆行しても結果は決定的（逆行の試験で固定）。
    時刻はレコードに既にあるため、Unsupported にはしない。ヘッダ以外から時刻を作ることはしない。解決は増分 → アーカイブの順に
    レコードのヘッダを走査して停止 LSN に落とし、以降は LSN 指定と同じ経路に一本化する
- **範囲外の拒否（`TargetOutOfRange`）**: 目標が、ベース DB の終点以前（適用するレコードが 0 件）、または
  入力したログ全体（増分とアーカイブ）の最大 LSN より後のとき。復元できる範囲は「ベース DB の終点より後、最大 LSN 以下」で、
  増分の途中や、アーカイブなしの増分だけの入力でも止められる。時刻指定で `T` を超えるレコードが無い（末尾まで行ってしまう）場合も、最初の候補がすでに `T` を
  超えている（停止点がベース以前）場合も同じ。黙って末尾まで適用しない。ベースそのものが欲しいときは通常の `restore` を使う。
  LSN 0 は `InvalidTarget`
- **検証（DB に触れる前に済ませる）**
  - アーカイブのディレクトリ全体を `verify_archive_dir` と同じ基準で検証する（セグメントごとのヘッダ・レコード・全体の
    チェックサム、LSN の飛び・重複・欠落、名前とヘッダの一致、DB 識別子・timeline がベースと一致）。停止点より後ろのセグメントの
    破損・欠落も拒否する。別の timeline のセグメントが混在するディレクトリは拒否する（timeline の分岐は #2446 待ち）
  - ベース（増分の末尾）の次から欠けなく続くセグメントがあること（先頭が後ろにずれていれば `ChainBroken`）
  - 他者が書き込めるアーカイブのディレクトリは拒否する（`InsecureDirectory`。読み取りでも、差し込まれたセグメントを使わないため）
  - 増分は #2168 と同じ検査（同一 DB・timeline・連続・順序。並べ替えない）
  - ストレージ形式の版数が一致すること
- **重なり区間の照合**: アーカイブのレコードのうち適用済みの LSN 以下のもの（ベースや増分と重なる区間）は、LSN だけで読み飛ばさず、
  適用先の `change_log` 表の同じ LSN の行とバイト列で照合する。適用済みの区間（ベース・増分）に完全に収まるセグメントも再生の対象から外さず、読み切って同じ照合と全体チェックサムの
  照合を行う（セグメントの分割位置で判定が変わらない）。再生リストは増分の後ろにアーカイブを付けるため LSN の昇順とは限らない。
  範囲の上限（停止点の範囲検査）は全セグメントの `last_lsn` の最大値を使う（最後の要素からは取らない）。再生が増分の途中で
  止まっても、停止位置以前の区間に重なるアーカイブのセグメントは読み、適用済みの内容と照合する（停止位置より後ろのレコードは
  適用も照合もしない）。これは再生リストを LSN 順に交互に並べる案（増分は 1 ファイルで途中に差し込めず、アーカイブの照合には
  増分の適用が先に要る）ではなく、再生器が停止後も重なるセグメントだけを読み続ける案を採った理由による。事前に一括で
  照合する案も、照合の基準（適用先の `change_log` の行）が適用後でないと揃わないため採らなかった不一致（分岐した履歴の混在）は `ChainBroken`。行が無い
  （刈り込み済みなど）場合は照合の基準がないため、ヘッダの DB 識別子・timeline・版数と `prev_lsn == lsn - 1` だけを確かめる。
  時刻の解決（ヘッダの走査）は重なり区間の形だけを見て、内容の照合は再生側が行う。時刻の解決で読んだセグメントは、停止位置が
  決まった後も最後まで読み切り、全体チェックサムを検証時のトレーラと照合する（検証後にコミット時刻とレコードのチェックサムを
  書き換えられても、書き換え後の時刻で停止 LSN を決めない。停止位置より後ろのセグメントは読まない）
- **再生中の検査**: レコードごとに `prev_lsn == 直前に適用した LSN`、DB 識別子・timeline・版数。セグメントが検証後に差し替えられていない
  こと（ヘッダの一致と、読み切ったときの全体チェックサムの照合）。停止した後も、同じセグメントの残りは読み切ってチェックサムを
  照合する。最終的に世代が停止 LSN に達していなければ `ChainBroken`
- **適用先の `change_log`**: #2168 と同じく、適用したレコードを追記する。復元した DB は `--change-log` で起動でき、連続性検査を通る
- **公開**: engine は一時コピーへ適用するだけで、公開は wire-server の `restore`（既存の方式）が行う。一時名（0600・排他作成）へ
  コピー → PITR 適用 → 整合検査 → fsync → `hard_link`（既存の宛先は上書きしない）。失敗したら何も公開せず、一時ファイルも消す
- **CLI**: `restore --from <base> --db <new> [--incremental <f>]... [--change-log-archive-dir <dir>] (--until-lsn <n> | --until-time <ts>)`。
  `--until-lsn` は 1 以上の 10 進整数。`--until-time` は RFC 3339（`YYYY-MM-DDTHH:MM:SS[.f]` + `Z` または `±hh:mm`、`T` 区切り）で、
  ゾーンの省略・IANA 名・空白区切りは拒否する（時刻を推測しない）。解析は engine の `TIMESTAMPTZ` リテラル解析を使う。
  併用（`--until-lsn` と `--until-time`）、`--change-log-archive-dir` だけ、`--until-*` だけでログの入力（アーカイブも増分も）が
  無い、不正な値、存在しないディレクトリは、DB を開く前に終了コード 2 で拒否する。範囲外・欠落・破損・別 DB・別 timeline は
  実行時の失敗として終了コード 1。エラー文は固定の英語で、LSN・時刻・パス・件数を含めない
- **timeline の分岐は行わない**: 復元した DB は元と同じ DB 識別子・timeline のまま LSN が巻き戻る。元の DB のアーカイブと
  同じディレクトリへ続きを書くと衝突しうるので、復元した DB は別のアーカイブ先で運用する（#2446 で timeline を分岐させる。D5・D7 の
  「分岐の未検出」と同じ扱い）
- **再生中のクラッシュからの再開は持たない**: 一時名から全体を再実行する（D7 の 4 の冪等性は、一時コピーを使うこの方式では不要）
- **増分の内側への停止**: 増分を指定しても、目標がその内側にあれば増分の途中で止まる。ベース（ベースの DB ファイルの世代）の
  終点以前は、入力に依らず範囲外として拒否する（ベースそのものは通常の `restore` を使う）

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
| #2169 | D7 の PITR・目標指定（実装は D12） |
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

- コード変更・依存追加はしない。実測値は書かない（本 ADR 作成時点。後続の実装メモは次項）
- **#3068 の実装メモ（書き込み点の移行。D1 の層 1・層 2）**:
  - 層 1: リポジトリ直下の `clippy.toml` で `redb::WriteTransaction` の `open_table`・`delete_table`・
    `rename_table`・`open_multimap_table`（と multimap の delete／rename）を `disallowed-methods` に
    指定した。型で縛る案は、読み取り側の `ReadSource` 境界・数百の関数シグネチャ・外部クレートの
    結合テストへ波及するため採らない。書き込み点は `storage::change_log::CaptureWriteTxn` の
    `open_table_logged`・`delete_table_logged`・`rename_table_logged`（`CapturingTable::open`・
    `delete_table`・`rename_table` への委譲）へ移した。`CapturingTable` は `ReadableTable` も実装し、
    読み取り境界（`&impl ReadableTable`）へ渡せる。変更系は `insert`・`remove`・`pop_first`・
    `pop_last`・`retain`・`retain_in`・`insert_reserve` で、`extract_if`・`entry`・カーソルは engine に
    利用がないため提供しない（必要になれば削除・更新したキーを積む実装を足してから使う）。
    素の open を許す箇所は `change_log.rs`（捕捉つきハンドルの本体・イベントを積んだ後の委譲先・
    記録範囲の例外である本体表とメタ表）と `storage::read_source`（読み取り専用の境界でだけ
    露出するハンドル）に限る。単体テスト（`#[cfg(test)]`）は素の redb を直接開くことを許し
    （`lib.rs` の `cfg_attr(test, allow(...))`）、`crates/engine/tests/`・`benches/`・`examples/` の
    外部クレート側は破壊・検査用に各ファイル冒頭で許す。製品コードへは漏れない（`--all-targets` の
    lint は `cfg(test)` でない lib ビルドも検査する）。
  - CI での強制: `rust-ci` が呼ぶ外部 workflow（Fandhe-AI/actions の `rust-base-ci.yml`）は
    `cargo clippy --workspace --all-targets --all-features -- -D warnings` を実行する（`make lint` と
    同じ）。wire-server のクレートでも `clippy.toml` は警告なしで通ることを確認した。
  - `ReadSource` の書き込み Tx 実装の戻り値は、変更系を公開しない読み取り専用の newtype
    （`WriteTxnReadTable`。`ReadableTable`・`ReadableTableMetadata` のみ。`Deref` なし）にした。
    具体型で `ReadSource::open_table` を呼んでも、捕捉を経ない書き込みへ到達できない。
  - 層 2: `CaptureSession::prepare` が、表集合の差から導く作成・削除に加えて次を検査し、矛盾は
    `CommitFootprintUnrecordable` で commit 前に拒否する。(1) 基底に在って最終に無い表に
    `Dropped`／`Renamed` のイベントが無い、(2) 世代が変化した論理表（`changed_table_generations_in_
    write_txn`）の世代カウンタ表のキーが積まれていない、(3) 世代が変化した論理表の行ストア
    （論理→物理の対応関数 `physical_row_store`）の行数の増減が積まれたキー数を超える。表の作成
    （`Created`）は #3067 と同じく表集合の差から導き、イベントは積まない。行数は O(1) の読み取りで、
    全行は走査しない。対応が未定義の論理表（行ストアを持たない種類）は(3)を省く。
  - 層 2 は補助的な早期検出で、完全性は層 1 と層 3（#3069 のオラクル）が負う。(3) は行数が同じ
    更新（同数の入れ替え）を検出できない。
  - 層 2 の有効時のコストは、世代が変わった表の数 N と全表数 T に対し、世代カウンタ表の走査
    O(T)・表集合の取得 O(T)・変わった表ごとの行数の読み取り O(N) の和（`list_tables` は表集合の
    取得に一本化済み）。
  - 検証（一時的な試験用改変。コミットしない）: 単体テストの素の redb 直呼びをすべてラッパへ置換し、
    永続 `Storage` の open で変更ログの有効化を強制して engine の lib テスト（約 6,600 件）を走らせた。
    失敗は「無効時の表の有無・commit 回数・カタログのバイト列」を比べる 15 件（有効化そのものが
    差になる）だけで、層 2 の誤拒否と捕捉漏れは出なかった。置換前（`#[cfg(test)]` の専用ヘルパーが
    素の redb で書く状態）では、同じ強制で行ストアの書き込みが層 2 に拒否される失敗が 58 件出た。
- **#2168 の実施状況（実施済み・#2169 へ回したもの・残課題）**:
  - 実施済み: D7 の「D7 の実装判断（Issue #2168）」のとおり、増分の出力・連鎖検証・適用・CLI
  - #2169 へ回したもの: 指定 LSN／時刻での停止（コミット境界）、再生中のクラッシュからの再開（本 Issue は一時名から
    全体を再実行するので不要）、アーカイブのセグメントを入力にする再生
  - 残課題（起票候補。Issue 化は人の判断）: ①稼働中のサーバーからの増分の取得（`--backup-socket` 経由。
    現在は停止中の DB のみ）。②刈り込み済みの区間を、アーカイブのセグメントから補って増分を組む経路（現在は
    `RangeUnavailable` で拒否）。③ベースと出力元が同じ DB 識別子・timeline で履歴が分岐している場合（複製から
    分岐した系列）は、終点の LSN だけでは検出できない。timeline の分岐（#2446）が前提
- 申し送り（Issue 起票はしていない。要否は人の判断）: #2166 の分割（ラッパ移行の規模）、
  #2149 への commit 番号系列の要件、#2446 との timeline の整合
- **#2169 の実施状況（実施済み・残課題）**:
  - 実施済み: D12 のとおり。`restore_to_point`（engine）と `restore` の `--change-log-archive-dir`・`--until-lsn`・`--until-time`
  - 残課題（起票候補。Issue 化は人の判断）: ①timeline の分岐と復元後の世代番号の進行（#2446）。ⓐ`SegmentReader::open` の `symlink_metadata` から `open` までの差し替えの窓（#2168 から続く既存の挙動で、再生時のヘッダ照合とチェックサムが検出する）。②PITR の入力にしたアーカイブの
    刈り込み済み区間をまたぐ復元の手順（ベースの取得点以降のセグメントを保持する運用。#2167 の②保持と削除）
- **#3069 の実施状況（実施済み・残課題・後続）**:
  - 実施済み: ①層 3 のオラクル（D11）。場面集は 11 場面、不一致は出なかった。②強制有効化での結合テスト
    （`tests/` の 99 バイナリ）の走行と内訳（D11 の「強制走行の結果」）。単体テストの `insert_row_into_table`
    系 3 関数は素の redb からラッパへ移した。③層 2 の表数依存の実測と削減（D11 の「性能の実測」。
    `changed_table_generations_in_write_txn` を突き合わせ実装にした）。④`benches/ingest_profile_bench.rs` の素の
    redb の書き込みは計装レプリカ（基準側）の関数に限られ、E0（`tenant::insert_rows`）側は engine の公開 API で、
    素の redb は整合性検証の読み取り専用 open だけであることを確認した。⑤フラグなしの再起動・再有効化の
    連続性・エラー分離の判断・frame の上限の判断（D11）。⑥`--change-log` の結線（`AVAILABLE = true`）
  - 残課題: ①lib の単体テストを強制有効化で流すと 47 件が落ちる（D11）。内訳は、素の redb で書く
    破損・直書きの fixture 26 件（オラクルが検出するのが正しい）、素の `db().begin_write()` の commit で世代だけが
    進む fixture 6 件、有効化そのものが差になる構造テスト 8 件、無効または明示の有効化を前提にした変更ログ自身の
    テスト 7 件。落ちたものの期待値は変えていない。
    必要になれば fixture のうち正常データを書くもの（`sequence` の `put_state`、`partitioned_dml` の `seed` 等）を
    ラッパへ移す。②1 commit の上限（キー 4M 個・128 MiB・ペイロード 256 MiB）を超える一括書き込みは有効時に
    `54000` で拒否される（無効時は通る）。運用では書き込みを分割する（D11）。③重い強制走行を CI へ入れるかは
    所要時間（D11）を見たオーナー判断
  - 後続の Issue への申し送り:
    - #2167（アーカイブ・保持）: `verify_continuity` は最後のレコードの LSN（無ければ `base_generation`）を使う。
      保持・削除で最後のレコードを消す設計にする場合は、削除前に最後の LSN をメタ表へ残すこと。有効化を戻す
      手段（無効化）の仕様もここで決める。D6 の保守 commit はメタ表の書き込みを増やすだけで、`base_generation` は動かさない
    - #2168／#2169（増分・PITR）: `storage::change_log::oracle::{apply_record, replay}` が再生器の参照実装
      （`dropped` の削除 → `created` → upsert／remove、LSN の連鎖検査）。復元の本実装はこれを `table_image` の
      実表への適用にする
    - #2149／#2154（バックアップ）: `Storage::enable_change_log` で `base_generation` を保存済み。取得時点の commit 番号は
      同じ `storage_generation` の系列を使う（D2）
- 未決: 移行 commit の扱いの詳細（保守 commit の削除件数上限の既定値・呼び出し契機・CLI フラグ名は D6 の
  「D6 の実装判断（Issue #2167）」で確定した）
- **#2167 の実施状況（実施済み・残課題）**:
  - 実施済み: セグメント形式・書き出し・水位・刈り込み・検証（ファイル／ディレクトリ／DB 突き合わせ）・
    常駐スレッド・CLI（D6 の実装判断）
  - D6 の記述と実装の既存の差異: D6 は保守 commit に `recording`（commit 記録の `CommitSession`）を付けないと
    書くが、`begin_maintenance_txn` は `wrap_gated` を通るため commit 記録があれば `recording` が付き、
    `commit_maintenance` はそれを `finish` せず破棄する（#3067 の `enable_change_log` 経路から同じ）。
    `CommitSession` の Drop は書き込み省略のスキップ印を戻すだけで、`last_generation`・床は `finish` でしか動かない
    ため、保守 commit は commit 記録を動かさない（5 回の保守 commit と利用者 commit の交互試験で固定）。D6 の
    「付けない」は「記録を進めない」の意味で読む
  - 強制有効化（`FANDHE_CHANGE_LOG_ORACLE=1`）で lib を流すと、#2167 の試験のうち 3 件が落ちる（D11 の 47 件に
    加わる）。いずれも「有効化そのものが差になる」区分で、強制有効化の `Storage::open` が連続性検査・
    有効化済みの判定を先に済ませるため、無効状態・明示の有効化を前提にした試験（`NotEnabled` の確認、
    再オープン後の連続性検査）が成り立たない。刈り込み → 利用者 commit をオラクルが検証する試験を含む
    残りの 23 件は強制有効化下でも通る
  - 残課題（起票候補。Issue 化は人の判断）: ①未アーカイブ容量の上限と `54000` 拒否（RECOVER-14 (2)）は
    #3091 で実施済み（D6 の実装判断（Issue #3091））。アーカイブ先なしの構成には上限を適用していない（要判断）。
    ②アーカイブファイルの保持と削除（RECOVER-14 (7)）。フルバックアップの commit 番号の記録（#2149／#2154）が前提。③無効化の仕様
    （上記）。④一時名の残骸（`.fandhe-seg-*.tmp`）の自動掃除。⑤常時書き込みが続いて保守 commit の見送り（`skipped`）が
    続く飢餓。水位が進まず `change_log` 表が増え続ける。常駐スレッドは見送りが 6 回連続した時点で stderr に
    固定文言の警告を 1 回出すが（実装済み）、ゲートを確実に取る手段（容量上限に達したときの `acquire` への
    切り替え。D6）は #3091 で実施済み。⑥`change-log-verify --db` は `Storage::open` を使うため、
    初期化・修復の書き込みをしうる。読み取り専用の open（#3074）に載せ替える。⑦アーカイブ常駐スレッドの
    panic は `catch_unwind` で周期ごとに握って再試行し、サーバーは止めない（アーカイブは利用者の読み書きの
    正しさに関与せず、未アーカイブの行は DB に残る。停止は stderr の固定文言で分かる）⑧`verify_log_replay` を刈り込み後の DB でも使えるようアーカイブを
    読む形に拡張する（#2168 の再生器と共有）
- **#3091 の実施状況（実施済み・残課題）**:
  - 実施済み: 未アーカイブ量（件数・バイト）の上限、超過時の `54000` 拒否と自動再開、上限時の保守 commit の
    `acquire` 切り替え、CLI 2 フラグと既定値、運用者向けの状態照会と遷移通知（D6 の実装判断（Issue #3091））
  - 残課題（起票候補。Issue 化は人の判断）: ①アーカイブ先なしの `--change-log` への上限の適用可否（spec 側の判断）。
    ②稼働中のサーバーの未アーカイブ量の数値を運用者専用ソケット・サブコマンドで照会する経路。
    ③上限で拒否されている最中の kill の crash-test シナリオ
