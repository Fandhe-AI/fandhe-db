# WITH HOLD カーソルの寿命・メモリ上限・可視性・解放（Issue #3338）

- ステータス: **提案（オーナー承認待ち）**。security-auditor の監査結果と、Issue #3338 へのオーナーの
  承認コメントをもって Accepted とする。承認まで実装 Issue（#3339 の子 #3340・#3341）は着手しない。
  判断は末尾の「判断記録（オーナー記入欄）」節に残す
- 対応: Issue #3338（親 #3306）。実装: #3339（子 #3340・#3341）
- ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-27 (6)・WIRE-15、`docs/spec/05-tasks.md` TASK-347・TASK-348。
  関連: SQL-31（明示トランザクション）・SQL-69 (5)（savepoint とカーソル）・RLS-7（暗黙適用）・SQL-40（権限検査）・
  ERR-6（エラー分類）。spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 前提: [sql-cursor.md](sql-cursor.md)（カーソルの全体設計）・[explicit-transaction.md](explicit-transaction.md)
  （トランザクションの状態機械）
- 本記録は設計判断だけを扱い、コードは変えない。コードはシンボル名で指し、行番号は書かない

## 背景

`DECLARE <name> CURSOR WITH HOLD FOR ...` は構文としては読めるが、固定文言の `0A000` で拒否している
（`sql::cursor::validate_declare_tokens`。判定順は [sql-cursor.md](sql-cursor.md) §4）。カーソルの実体
（`sql::cursor::CursorRegistry`）は `sql::transaction::ActiveTxn` のフィールドで、トランザクションが終わると
`ActiveTxn` ごと drop される。この構造のおかげで、次の 3 点が追加コードなしで成り立っている。

1. **解放の漏れが無い**: `COMMIT`・`ROLLBACK`・`fail`・期限切れ・接続断のどれでも、カーソルは構造上残らない
2. **保持時間に上限がある**: カーソルの寿命はトランザクションの持続時間の上限（`TransactionLimits::max_duration`。
   既定 20 秒）以下になる
3. **古さに上限がある**: 返す行は、宣言してから最長でもトランザクションの持続時間の上限までの間に確定した状態に限られる

`WITH HOLD` はカーソルをトランザクションの外へ持ち越すため、この 3 点を崩す。実装の前に、寿命・メモリ・
可視性・解放の規則を決めておく必要がある。

## 現状（設計の根拠）

| 事項 | 現状 |
| ---- | ---- |
| 構文 | `validate_declare_tokens` が `parse_declare_hold` で `WITH HOLD`／`WITHOUT HOLD` を読み、内側 SELECT の構造検証の後で `WITH HOLD` を `0A000` にする。`WITHOUT HOLD` は受理して何もしない |
| 登録簿 | `CursorRegistry`（名前 → `OpenCursor`。確定済み `QueryResult`・位置・`SCROLL` 指定・概算バイト数・個体識別子 `id`）。`ActiveTxn::cursors` に埋め込まれる |
| 個体識別子 | `CursorRegistry::next_id` は `ActiveTxn` ごとに 0 から始まる。savepoint は `next_id` を高水位として記録し、`ROLLBACK TO` は `close_declared_since` で高水位以上の id のカーソルを閉じる（SQL-69 (5)） |
| 上限 | `MAX_CURSORS_PER_SESSION`（16 本）と `MAX_CURSOR_BYTES_PER_SESSION`（16 MiB）。超過はどちらも `54000`。本数は内側 SELECT の実行前に、バイト数は生成中（`remaining_bytes` を生成予算に渡す）と登録時に判定する |
| 行集合の確定 | `DECLARE` の時点で内側 SELECT を 1 回だけ実行し、参照セッション自身の `PolicyContext` で RLS を暗黙適用した行を確定する（`EngineCore::execute_cursor_in_active_txn`）。`FETCH`／`MOVE` は読み取り源に触れない |
| 読み取り源 | `SessionTransaction::read_source` の三分岐（`BEGIN` 時点のスナップショット／共有 write txn／確定状態）。write txn から読んだ場合、行集合は自トランザクションの未 commit の変更を含む |
| Tx 外の文 | トランザクション文脈を持たない入口では、`DECLARE` は `25P01`、`FETCH`／`MOVE`／`CLOSE` は `34000` に固定している（`core.rs` の `ParsedSql::Cursor` の分岐） |
| `CLOSE ALL` | 拒否（`validate_close_tokens`）。`ALL` はカーソル名としても拒否済み（`validate_cursor_name`） |
| `DISCARD` | 文として未対応（どの文種別にも当たらず拒否される） |
| wire の portal 束縛 | `wire-server::extended_query::CursorBinding`（Tx 世代と `cursor_id`）。中断（`Suspended`）portal の再開時に `active_generation()` と `cursor_id` の両方が一致しなければ `34000`。`active_generation()` は `Active` の間だけ `Some` |
| セッションの状態 | `sql::mode::SessionState` は `BEGIN`／`SAVEPOINT` で複製され、`ROLLBACK`／`ROLLBACK TO` で巻き戻る。`SessionTransaction` の `generation`・`reported_time_zone` は巻き戻らない接続単位の値 |
| 接続の無通信 | `limits::READ_TIMEOUT`（既定 30 秒。`--read-timeout-secs` で 1〜600 秒）を超えて要求が来なければ接続を閉じる（WIRE-5） |

## PostgreSQL 17 の挙動（一次情報）

出典は PostgreSQL 17 の文書（[DECLARE](https://www.postgresql.org/docs/17/sql-declare.html)・
[CLOSE](https://www.postgresql.org/docs/17/sql-close.html)・[DISCARD](https://www.postgresql.org/docs/17/sql-discard.html)・
[ROLLBACK TO SAVEPOINT](https://www.postgresql.org/docs/17/sql-rollback-to.html)）と、REL_17_STABLE の
`src/backend/utils/mmgr/portalmem.c` のコメント（`PreCommit_Portals`・`AtAbort_Portals`・`AtCleanup_Portals`・
`AtSubCleanup_Portals`・`PortalHashTableDeleteAll`）。

| 事項 | PG17 |
| ---- | ---- |
| COMMIT 後 | 宣言したトランザクションが commit に成功すれば、同じセッションの後続のトランザクションでも使える。行は commit 時に一時領域へ実体化される |
| 宣言した Tx の abort | カーソルは破棄される（`ROLLBACK` での暗黙のクローズ） |
| 前の Tx から持ち越したカーソル | 後の Tx が abort しても何もしない（残る） |
| 解放 | 明示の `CLOSE`、またはセッションの終了まで残る |
| `CLOSE ALL` | 開いている全カーソルを閉じる（PG の拡張） |
| `DISCARD ALL` | `CLOSE ALL` を含むセッションの全状態の初期化（ロール・設定・準備済み文・一時表・採番状態ほか）。トランザクションブロックの中では実行できない |
| Tx ブロックの外の `DECLARE` | `WITH HOLD` なしはエラー（明記）。`WITH HOLD` は受理されると読める（直接の明記は無く要実測） |
| savepoint | savepoint の中で開いたカーソルは `ROLLBACK TO` で閉じる。`FETCH`／`MOVE` による位置の移動と `CLOSE` は巻き戻らない。実行が Tx を abort させたカーソルは以後使えない |
| 行ロック句 | `WITH HOLD` は `FOR UPDATE`／`FOR SHARE` と併用できない |

上の表の外の挙動（`DISCARD ALL` を Tx ブロック内で発行したときの SQLSTATE、`WITH HOLD` と行ロック句の併用の
SQLSTATE、後の Tx の `ROLLBACK` の後の持ち越しカーソルの位置・`CLOSE`、持ち越しカーソルの `FETCH` が失敗した後の扱い）
は文書に明記が無いため、**要実測**として #3341 で PG17 の実機と突き合わせる（下の「PG17 との差」節）。

## 決定

### H1. 寿命と持ち越し先

- `WITH HOLD` のカーソルは、宣言したトランザクションが **commit に成功したときだけ** 持ち越す。持ち越し先は
  `SessionTransaction` が持つ接続単位の「保持登録簿」（以下 held 登録簿）とする
- 持ち越した後は、`CLOSE`・`CLOSE ALL`（H5）・`DISCARD ALL`（H5）・接続の終了のいずれかまで残る。
  トランザクションの持続時間の上限（`max_duration`）は持ち越した後のカーソルには掛からない（PG17 と同じ。
  保持時間の上限の扱いは H3 と未決事項 Q2）
- 行集合は `DECLARE` の時点で確定済みであり、**COMMIT は行集合を再評価せず、そのまま引き継ぐだけ**とする。
  PG17 は commit 時に残りの行を実体化するが、本 DB の行集合はもともと `DECLARE` の時点で確定している
  （INSENSITIVE）ため、返す行の集合は同じになる
- held 登録簿へ移すのは、`commit_active` が成功を返した後に限る（明示 Tx の `SessionTransaction::commit`。
  暗黙 Tx〔複数文メッセージ〕の中で `DECLARE` が実行される経路がある場合は `commit_implicit` も同じ。経路の有無は #3340 で確かめる）。
  commit が失敗する経路（遅延 `FOREIGN KEY` の `23503`、commit 自体の失敗、持続時間の上限超過の `54000`）では移さず、
  `ActiveTxn` と一緒に破棄する。write txn から読んだ行集合は未 commit の変更を含むため、commit が確定していない行を
  外へ出さないための規則である
- `Failed` の中の `COMMIT` は、現行どおりエラーを返して状態を変えない（`Failed`／`FailedRecoverable` のまま）。
  `FailedRecoverable` は `ActiveTxn` をカーソルごと保持しているので、後の `ROLLBACK TO` で `Active` に戻った Tx が
  commit に成功すれば、その時点で残っている `WITH HOLD` のカーソルを持ち越す（savepoint より後に宣言したものは
  `ROLLBACK TO` で既に閉じている）。`ROLLBACK` で終われば破棄する
- `WITHOUT HOLD`（指定なしを含む）のカーソルは、従来どおり `ActiveTxn` と一緒に消える（WIRE-15 の契約を変えない）

### H2. 登録簿の構造（2 層）

- **Tx 登録簿**は従来どおり `ActiveTxn::cursors` に置く。`WITH HOLD` の有無に関わらず、宣言したカーソルはまずここへ入る。
  `OpenCursor` に `hold: bool` を加える
- **held 登録簿**は `SessionTransaction` のフィールドに置く。`SessionState` には置かない。`SessionState` は `BEGIN` で
  複製され `ROLLBACK` で巻き戻るため、置くと行集合（最大 16 MiB）を Tx ごとに複製し、`ROLLBACK` で位置の移動や
  `CLOSE` まで巻き戻してしまう（PG17 はどちらも巻き戻さない）
- 持ち越しは、commit 成功の直後に Tx 登録簿から `hold` のエントリを取り出し、held 登録簿へ **移動**する
  （`QueryResult` を複製しない。失敗し得ない操作にする）。残りのエントリは `ActiveTxn` と一緒に drop される
- この形にすると、新しく書く解放のコードは「commit 成功時の移動」1 か所だけになる。失敗・abort・期限切れ・接続断の
  経路は従来どおり drop に任せるので、移動を書き忘れた経路はカーソルを失うだけで外へは漏らさない（fail-closed）
- **名前空間は 1 つ**: Tx 登録簿と held 登録簿を通して名前は一意にする。held 登録簿にある名前での再 `DECLARE` は、
  既存の重複と同じ `22000`
- **個体識別子は接続単位で単調増加**にする。`next_id` を `CursorRegistry` から `SessionTransaction` へ移し、
  宣言のたびに接続全体で払い出す。現行は Tx ごとに 0 から払い出すため、`Idle` の portal 束縛（H7）では世代で区別できず、
  前の Tx から持ち越したカーソルと、後の Tx で `CLOSE` の後に同名で再宣言したカーソルの id が衝突しうる
  （savepoint の高水位は Tx 登録簿にしか作用しないので、held 登録簿には影響しない）

### H3. メモリ上限と超過時の挙動

- 1 本あたりの行数は、従来どおり `DECLARE` の時点で結果行数の上限の中に収まる（[limit-omitted-result-cap.md](limit-omitted-result-cap.md) D5）。
  持ち越しで行数は増えない
- **上限は Tx 登録簿と held 登録簿の合算**で、既存の 16 本・16 MiB をそのまま使う（新しい定数を作らない。
  合算にしないと、持ち越すたびに上限が実質 2 倍・3 倍になる）。`ensure_capacity_for_declare`（本数）と
  `remaining_bytes`（生成予算）は両方の登録簿の合計で判定する
- **超過は `DECLARE` の時点の `54000`** とし、判定点と文言は既存と同じにする。行は `DECLARE` の時点で確定し課金済みなので、
  COMMIT 時に新しく課金するものは無い。このため **上限超過で COMMIT が失敗することも、COMMIT でカーソルが黙って閉じることも無い**
- held 登録簿のカーソルは確定済みの `QueryResult` だけを持ち、redb の `ReadTransaction`・write txn・writer の permit を持たない
  （持ち越した後は、MVCC のページ解放を妨げず、単一ライタも塞がない）
- 最悪の常駐量は、接続あたり 16 MiB × 同時接続数（既定 64・ハード上限 256）で最大 4 GiB。中断 portal の上限
  （`MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`）とは別枠のまま。一時表（[temp-table.md](temp-table.md) の総バイト数の上限）と
  同じく接続単位の上限で抑え、プロセス全体の上限は設けない（未決事項 Q2）
- 時間の上限: 無通信の接続は `READ_TIMEOUT` で閉じられ、カーソルもそこで消える。要求を送り続ける接続では、カーソルを
  いつまでも保持できる（PG17 と同じ。未決事項 Q2）

### H4. COMMIT 後の RLS・権限上の可視性

- 返す行は、**宣言したセッションのテナントの文脈で `DECLARE` の時点に確定した行だけ**とする。RLS は `DECLARE` の内側 SELECT で
  参照セッション自身の `PolicyContext` により暗黙適用済みで（RLS-7）、COMMIT も `FETCH` も行を足さない・入れ替えない
- テナント境界: テナントは接続の認証時に固定され、SQL から変えられない（[fixed-tenant-boundary.md](fixed-tenant-boundary.md)）。
  held 登録簿は接続単位の値なので、他の接続から名前を引く経路が無い。他セッションのカーソル名は不在と同じ `34000` で、
  文言にカーソル名や所有の有無を含めない（[sql-cursor.md](sql-cursor.md) §3 の契約を held 登録簿にも適用する）
- **COMMIT 後の変化は見え方を変えない**: 持ち越した後に次のことが起きても、`FETCH`／`MOVE` の結果は宣言時に確定した行集合だけで決まる
  - `SET ROLE`／`RESET ROLE`、`row_security` の変更
  - `REVOKE` などの権限の変更、利用者ポリシー（`CREATE POLICY`／`ALTER POLICY`／`DROP POLICY`）の変更
  - 元の行の `UPDATE`・`DELETE`、表の `TRUNCATE`・`DROP TABLE`
- 理由: (1) 行集合は確定済みで、条件を変えて絞り直すには再実行が要る（INSENSITIVE を崩す）。(2) 現行の Tx 内カーソルも、
  `DECLARE` 後の `REVOKE`・ロール変更の影響を受けない（[role-privilege-model.md](role-privilege-model.md) §12。SQL-40）。
  (3) PG17 も実体化した行をそのまま返す。テナント境界は (1) の確定時点で既に効いているので、COMMIT 後の変化で
  他テナントの行や存在情報が見えるようになる経路は無い
- 受け入れる残留リスク: 宣言時に読めたロール・ポリシーの行を、権限を外した後も同じ接続から読み続けられる。範囲は同じ
  接続・同じテナント・宣言時に既に読めた行に限られる。保持時間を限りたい場合の対策は未決事項 Q2・Q3

### H5. 解放の契機と ROLLBACK での扱い

| 契機 | Tx 内で宣言した `WITH HOLD` | 前の Tx から持ち越したカーソル |
| ---- | --------------------------- | ------------------------------ |
| `COMMIT` 成功（明示・暗黙） | held 登録簿へ移す（H1） | 残る |
| `COMMIT` 失敗（`23503`・commit の失敗・期限切れ） | 破棄 | 残る |
| `Failed` の中の `COMMIT` | 状態を変えない（後の `ROLLBACK` で破棄。`ROLLBACK TO` で復帰して commit に成功すれば持ち越す） | 残る |
| `ROLLBACK`（`Active`／`Failed`） | 破棄（PG17 と同じ） | 残る。位置の移動と `CLOSE` は巻き戻さない |
| `ROLLBACK TO SAVEPOINT` | savepoint より後に宣言したものは閉じる（既存の `close_declared_since`） | 残る。位置は巻き戻さない |
| 文のエラーで `Failed`・期限切れ（`release_if_expired`） | 後の `ROLLBACK` で破棄（`FailedRecoverable` からの `ROLLBACK TO` は上の行） | 残る |
| `CLOSE <name>` | 閉じる | 閉じる（その場で held 登録簿から外す。後の `ROLLBACK` で戻らない） |
| `CLOSE ALL` | 閉じる | 閉じる（提案。未決事項 Q4） |
| `DISCARD ALL`（Tx の外だけ） | — | 閉じる（提案。未決事項 Q5） |
| 接続の終了（切断・`READ_TIMEOUT`・サーバーの終了） | `SessionTransaction` の drop で消える | 同左 |

- `CLOSE` は Tx 登録簿を先に、無ければ held 登録簿を引く（名前は H2 で一意）。どちらにも無ければ既存どおり `34000`
- 持ち越したカーソルの `FETCH`／`MOVE` の失敗は 2 種類に分けて扱う
  - **位置を動かす前の拒否**（`34000`・`55000`・`ALL` の `54000`・構文と件数の `42601`／`22000`）: 位置は変わらないので、
    カーソルは使える状態で残す。Tx 内カーソルの `ROLLBACK TO` 後の扱い（[sql-cursor.md](sql-cursor.md) 「PG17 との意図した差」の
    最終行）と同じ既存の契約に揃えたもので、PG17 との差として扱う（要実測）
  - **位置を動かした後の失敗**（エンジンの実行は成功したが、wire 層の応答の生成・送出に失敗した場合。中断 portal の
    バイト上限の `54000` など）: 現行の `CursorRegistry::fetch` は、この場合に Tx が `Failed` になるか接続が切れて
    登録簿ごと消えることを前提に、送出の確定を待たずに位置を進めている。持ち越したカーソルは Tx と一緒に消えず、`Idle` では
    失敗させる Tx も無いため、そのままだと次の `FETCH` がクライアントの受け取っていない行を飛ばして再開する。
    このため **その持ち越しカーソルを閉じる**（以後 `34000`）。位置を戻す案は、エンジンの位置の巻き戻しを wire 層から呼ぶ
    経路を新たに作るため採らない。接続が切れる場合は従来どおり登録簿ごと消える
- `CLOSE ALL` を受理する場合は、`ALL` がカーソル名として既に拒否済み（`validate_cursor_name`）なので曖昧さは生じない

### H6. Tx の外での `DECLARE`／`FETCH`／`MOVE`／`CLOSE`

- `Idle`（明示 Tx の外）の `FETCH`／`MOVE`／`CLOSE` は held 登録簿を引く。現行の「Tx 文脈の無い入口では一律 `34000`」は、
  held 登録簿に名前が無い場合の応答として残す。読み取り源には触れないので、スナップショットや writer は要らない
- 明示 Tx の `Active` の中（暗黙 Tx の中でカーソルの文が実行される経路があればそこも）では、Tx 登録簿の次に held 登録簿を引く
- `Failed` の中では、持ち越したカーソルへの文も既存の parse 前判定で `25P02` にする（PG17 と同じ）
- `WITHOUT HOLD`（指定なしを含む）の Tx 外の `DECLARE` は、従来どおり `25P01`
- `WITH HOLD` の Tx 外の `DECLARE` は、受理することを提案する（未決事項 Q1。PG17 の文書は `WITH HOLD` なしを Tx ブロックの外で
  エラーにすると明記しており、`WITH HOLD` は受理されると読めるが、直接の明記は無いため要実測）。受理する場合は、1 文だけの
  暗黙の読み取りとして確定状態（`TxnReadSource::Committed` に相当）を読み、その場で held 登録簿へ入れる。
  上限（H3）・RLS（H4）・権限検査（SQL-40）は Tx 内と同じ判定点を通す
- 文数・持続時間の上限（`check_and_register_statement`）は、Tx の中で発行した文だけに掛かる（Tx 外の `FETCH` は数えない）

### H7. wire 層（拡張クエリ・Describe・状態バイト）

- **portal 束縛**: 持ち越したカーソルの `FETCH` から作った portal も、中断（`Suspended`）からの再開時に照合する。
  `CursorBinding` の世代を `Option<u64>` として束縛時の `active_generation()` をそのまま記録し、再開時は
  「世代が束縛時と一致し、かつ `cursor_id` が一致する」ときだけ送出する。`Idle` で作った portal は世代が `None` 同士で
  一致するため、`CLOSE`・同名の再宣言は `cursor_id`（H2 で接続単位に一意）で弾く。現行は `Idle` では束縛を作らない
  （`active_generation()` が `None` なので束縛が `None` になり、照合の対象外になる）ため、#3340 でこの穴を塞ぐ
- portal 自体の寿命は変えない。portal は PG17 と同じく保持の対象ではなく、Tx の終了・Sync で既存の規則どおり破棄する
  （`purge_expired_portals`）。持ち越すのはカーソルの行集合と位置だけ
- **応答の失敗**: H5 の「位置を動かした後の失敗」の場合、wire 層は束縛した `cursor_id` のカーソルを閉じるよう engine へ求める
  （名前ではなく id で閉じ、同名の別インスタンスを閉じない）。簡易クエリ・拡張クエリの両方の応答失敗経路が対象
- **Describe**: `describe_parsed_in_txn` の `FETCH` の結果列は、Tx 登録簿の次に held 登録簿を引く。`Idle` でも返す
- **状態バイト**: 持ち越したカーソルがあっても、Tx の外の `ReadyForQuery` は `I`（PG17 と同じ）

### H8. エラー契約（追加・変更分）

| 事象 | `wire_code` | 備考 |
| ---- | ----------- | ---- |
| `WITH HOLD` の受理 | 成功（`DECLARE CURSOR`） | 現行の `0A000` を外す。`BINARY` の `0A000` は変えない |
| 合算の本数・バイト数の超過 | `54000` | `DECLARE` の時点だけで判定する。COMMIT は上限で失敗しない |
| held 登録簿と同名の `DECLARE` | `22000` | 既存の重複と同じ |
| Tx 外の `FETCH`／`MOVE`／`CLOSE` で名前が無い | `34000` | 既存と同じ固定文言 |
| `Failed` の中の持ち越しカーソルへの文 | `25P02` | 既存の parse 前判定 |
| `WITHOUT HOLD` の Tx 外の `DECLARE` | `25P01` | 変更なし |
| `WITH HOLD` の Tx 外の `DECLARE` | 成功（提案。Q1） | 不採用なら `25P01` のまま |
| `WITH HOLD` と行ロック句 | `42601` | カーソルの行ロック句は既に `42601`（`sql38_row_lock_clause.rs`）。PG17 の SQLSTATE は要実測 |
| `DISCARD ALL`（未実装の間） | 現行どおり拒否 | Q5 |

- 文言は英語の固定文言で、カーソル名・件数・他の状態を含めない（既存の方針）

## 代替案（却下）

| 案 | 却下の理由 |
| -- | ---------- |
| 登録簿を 1 つにまとめて `SessionTransaction` へ移し、各経路で `hold` 以外を明示的に消す | 解放が「全ての終了経路で消すコードを書く」形になり、1 経路の書き忘れで `WITHOUT HOLD` のカーソルが Tx の後に残る（fail-open）。H2 の 2 層なら新しい経路は commit 成功時の移動だけで、書き忘れはカーソルを失う側に倒れる |
| held 登録簿を `SessionState` に置く | `BEGIN` ごとに行集合を複製し、`ROLLBACK` で位置と `CLOSE` まで巻き戻す（PG17 と食い違う）。H2 参照 |
| COMMIT 時に上限を判定し、超過したら COMMIT を失敗させる | カーソルの都合で書き込みまで取り消すことになる。行は `DECLARE` で課金済みなので、判定点を増やす必要が無い |
| COMMIT 時に上限を超えた分のカーソルを黙って閉じる | 利用者に知らされずにカーソルが消え、後の `FETCH` が `34000` になる。原因が分からない失敗を作る |
| held 用に別枠の上限（例: 持ち越し専用の 16 MiB）を設ける | 接続あたりの常駐量の上限が実質 2 倍になる。既存の「接続あたり 16 MiB」の説明が成り立たなくなる |
| COMMIT 時に行集合を取り直す（PG17 の実体化の時点に合わせる） | `DECLARE` の時点で確定する契約（WIRE-15）を崩し、同じ Tx の後続の書き込みが混ざる |
| `FETCH` のたびに権限・ポリシーを再検査し、外れた行を除く | 再実行無しでは行を絞り直せない。Tx 内カーソルの既存の契約（SQL-40）・PG17 とも食い違う |
| `DISCARD ALL` を「カーソルを閉じるだけ」で先に受理する | 他のセッション状態（ロール・`search_path`・一時表・採番状態など）が残り、接続を使い回すプーラーで前の利用者の状態を次の利用者へ持ち越す。部分実装は受理しない（Q5） |

## 影響

- engine: `CursorRegistry`（`hold`・id の払い出し元）、`SessionTransaction`（held 登録簿・id カウンタ・commit 成功時の移動・
  Tx 外の取得口）、`core.rs`（Tx 外の `FETCH`／`MOVE`／`CLOSE` の分岐、`WITH HOLD` の `0A000` の撤去、Describe）
- wire-server: `extended_query::CursorBinding` の世代の持ち方と照合
- 既存の契約: `WITHOUT HOLD` の寿命、上限の値、`34000`・`22000`・`25P01`・`25P02` の判定点は変えない。
  [sql-cursor.md](sql-cursor.md) の「カーソルの寿命はトランザクションの寿命以下」は `WITHOUT HOLD` に限った記述になる
  （実装の段で同ファイルを更新する）
- 拒否を固定している既存テスト（`cursor.rs` の単体テストの `WITH HOLD` の `0A000`、SQL 経由の同種のテスト）は、
  #3340 で受理後の挙動へ書き換える（期待値は弱めない）
- spec 側（WIRE-27 (6) のポインタ先）の更新は spec リポ側の作業として、承認後にオーナーへ申し送る

## 実装 Issue の分け方（案）

既存の #3339 の子 2 件を使い、範囲を次のように確定することを提案する。

### #3340: 宣言と COMMIT 後の持ち越し

- 範囲: `WITH HOLD` の受理、H2 の 2 層の登録簿・接続単位の id、H1 の commit 成功時の移動（明示・暗黙）、H3 の合算上限、
  H6 の Tx 外の `FETCH`／`MOVE`／`CLOSE`、H7 の portal 束縛と Describe。Q1 が採用なら Tx 外の `DECLARE ... WITH HOLD` も含む
- テスト観点（層 A: engine）
  - COMMIT 後に `FETCH`・`MOVE`（`SCROLL` の逆方向を含む）が宣言時と同じ行・位置で動く
  - `WITHOUT HOLD` と指定なしは COMMIT で消える（以後 `34000`）
  - held 登録簿の本数・バイト数を含めて 17 本目・16 MiB 超が `DECLARE` の時点で `54000` になり、COMMIT は成功する
  - held 登録簿と同名の `DECLARE` が `22000`
  - 前の Tx から持ち越したカーソルが、後の Tx の savepoint の `ROLLBACK TO` で閉じない（id の高水位の回帰）
  - writer 取得後に宣言したカーソルの行集合（未 commit の変更を含む）が、COMMIT 成功後にだけ持ち越される
- テスト観点（層 B: wire-server）
  - psql と同じ形（末尾 `;` 付きの単独送信）で `BEGIN` → `DECLARE ... WITH HOLD` → `COMMIT` → `FETCH` が通る
  - 拡張クエリの `max_rows` で中断した `FETCH` portal の再開を、`Idle` で `CLOSE`・同名の再宣言を挟んだ場合は `34000` で拒否する
  - Describe（Statement／Portal）が `Idle` の `FETCH` で列を返す。状態バイトは `I`
  - `Idle` で持ち越しカーソルの `FETCH` の応答生成が失敗した場合（中断 portal のバイト上限の超過など）、そのカーソルが閉じ、
    次の `FETCH` が受け取っていない行を飛ばして再開しない
  - `Failed`（`FailedRecoverable`）の中の `COMMIT` の後、`ROLLBACK TO` で復帰して commit に成功した場合に、savepoint より前に
    宣言した `WITH HOLD` だけが持ち越される

### #3341: 解放と越境試験

- 範囲: H5 の各契機の解放と、その後に登録簿が空になることの確認。Q4 が採用なら `CLOSE ALL`。H4 の越境試験。
  PG17 の実測による「要実測」項目の確定
- テスト観点（層 A）
  - `ROLLBACK`・COMMIT の失敗（`23503`・持続時間の上限超過）・`Failed` 後の `ROLLBACK` で、その Tx の `WITH HOLD` が破棄され、
    前の Tx から持ち越したカーソルは位置を保って残る
  - 後の Tx の中の `CLOSE` が `ROLLBACK` で戻らない
  - `SessionTransaction` の drop（切断）で held 登録簿が空になる
- テスト観点（層 B）
  - 3 テナントの越境: 各テナントの接続で宣言・持ち越したカーソルに他テナントの行が現れない。他テナントの行を増減させても
    持ち越したカーソルの応答が変わらない（WIRE-27 (4) のポインタ先）。持ち越した後の `SET ROLE`、利用者ポリシーの変更、
    元の行の `DELETE`、`DROP TABLE` の後も同じ
  - 他の接続から同じ名前を `FETCH`／`CLOSE` すると `34000`（不在と区別できない）
  - 切断・`READ_TIMEOUT` 後の再接続で前のカーソルが見えない
- PG17 の実測（`crates/engine/tests/data/` の既存の突き合わせの形に揃える）: 下の「PG17 との差」の要実測の項目

### 新規の候補（未起票。起票はオーナーの承認を経る）

- `DISCARD ALL` の実装（Q5 が「別 Issue」の場合）。セッションの全状態の初期化と、Tx 内の拒否、held 登録簿の解放をまとめて扱う
- プロセス全体の常駐量の上限・保持時間の上限（Q2 で必要と判断した場合）

## PG17 との差（意図した差・要実測）

| 形 | PG17 | 本提案 | 扱い |
| -- | ---- | ------ | ---- |
| 実体化の時点 | commit 時 | `DECLARE` 時（COMMIT は引き継ぐだけ） | 意図した差。返す行の集合は同じ（H1） |
| 持ち越したカーソルの `FETCH` が失敗した後 | 要実測（abort させたカーソルは以後使えない、という savepoint の記述がある） | 使える状態で残す | 既存の契約に揃える（H5）。実測で差を確定する |
| 後の Tx の `ROLLBACK` 後の位置・`CLOSE` | 要実測（savepoint については巻き戻さないと明記） | 巻き戻さない | 実測で一致を確認する |
| `DISCARD ALL` の Tx 内での発行 | 実行できない（SQLSTATE は要実測） | Q5 | 実装する場合は実測の SQLSTATE に揃える |
| `WITH HOLD` と行ロック句 | 併用不可（SQLSTATE は要実測） | `42601`（カーソルの行ロック句は既に拒否） | 実測後に差として記録する |
| 指定なしの逆方向・`42P11`・`42P03` | — | 既存の差のまま | [sql-cursor.md](sql-cursor.md) の表のとおり |

## 未決事項（オーナー判断）

- **Q1**: Tx の外の `DECLARE ... WITH HOLD` を受理するか。提案は受理（PG17 の挙動は要実測。psql の自動 commit で単独送信する形）。
  不採用なら `25P01` のまま残し、PG17 との差として記録する
- **Q2**: 保持時間・プロセス全体の常駐量に上限を設けるか。提案は設けない（接続あたり 16 MiB と `READ_TIMEOUT` で抑え、PG17 と同じ）。
  設ける場合は、持ち越したカーソルの無操作の時間の上限（超過で閉じる）か、プロセス全体の合計の上限（超過は `DECLARE` の `54000`）の
  どちらにするか
- **Q3**: H4 の残留リスク（宣言時に読めた行を、持ち越した後に権限やポリシーが変わっても同じ接続で読み続けられること）を
  受け入れるか。提案は受け入れる（PG17 と、既存の Tx 内カーソルの契約と同じ）
- **Q4**: `CLOSE ALL` を #3341 で受理するか。提案は受理（Tx 登録簿と held 登録簿の全カーソルを閉じる。`Failed` の中は `25P02`）
- **Q5**: `DISCARD ALL` をどう扱うか。提案は、セッションの全状態の初期化を伴う別 Issue として起票し、それまでは現行どおり拒否する。
  #3341 の解放の契機からは外す（部分実装は却下。「代替案」節）
- **Q6**: 上限の値（16 本・16 MiB）を持ち越しの導入後も同じにするか。提案は同じ（[sql-cursor.md](sql-cursor.md) の対象外の
  「上限の既定値の確定」と合わせて判断する）

## 判断記録（オーナー記入欄）

| 項目 | 内容 |
| ---- | ---- |
| 判断 | （未記入）H1〜H8 の採否と Q1〜Q6 への回答 |
| security-auditor の監査 | （未記入）監査の結果と指摘への対応を Issue #3338 に記録する |
| 判断日 | Issue #3338 の承認コメントの日付を転記する（本 ADR 単独では確定しない） |
| 記入者 | Issue #3338 の承認コメントの投稿者を転記する |

## 対象外

- NoSQL 表層のカーソル（カーソルを持たない。[sql-cursor.md](sql-cursor.md) の対象外のまま）
- portal 自体の保持（PG17 の保持の対象もカーソルだけ）
- 持ち越したカーソルの行集合の永続化（再起動・crash を越えて残すこと。PG17 もセッションの終了で消える）
- `pg_cursors` 相当のカタログ（システムカタログの ADR 側で扱う）
