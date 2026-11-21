# WITH HOLD カーソルの寿命・メモリ上限・可視性・解放（Issue #3338）

- ステータス: **承認待ち（判断事項は決定済み）**。判断事項 Q1〜Q6 はオーナーが 2026-11-21 に決定した（「決定（オーナー、
  2026-11-21）」節）。security-auditor の監査の指摘は反映済み。本 ADR を含む PR のマージをもって Accepted とし、それまで
  実装 Issue（#3339 の子 #3340・#3341）は着手しない
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
`Failed` の中の `COMMIT` を PG17 が `ROLLBACK` として扱う点も、同じく実機で確かめる。

## 決定

決定は本 DB の実装の構造に沿って D1〜D8 に分ける（所有の構造 → Tx の終わり方 → 文の振り分け → 行と読み手の束縛 →
課金 → 失敗した取得 → wire 層 → エラー契約）。

### D1. 所有の構造（2 層の登録簿）

- **Tx 登録簿**は従来どおり `ActiveTxn::cursors` に置く。`WITH HOLD` の有無に関わらず、宣言したカーソルはまずここへ入る。
  `OpenCursor` に `hold: bool` と、D4 の読み手の束縛（宣言時のロール）を加える
- **held 登録簿**（持ち越したカーソルの置き場）は `SessionTransaction` のフィールドに置く。`SessionState` には置かない。
  `SessionState` は `BEGIN` で複製され `ROLLBACK` で巻き戻るため、置くと行集合（最大 16 MiB）を Tx ごとに複製し、
  `ROLLBACK` で位置の移動や `CLOSE` まで巻き戻してしまう（PG17 はどちらも巻き戻さない）
- 持ち越しは、commit 成功の直後に Tx 登録簿から `hold` のエントリを取り出し、held 登録簿へ **移動**する
  （`QueryResult` を複製しない。失敗し得ない操作にする）。残りのエントリは `ActiveTxn` と一緒に drop される
- この形にすると、新しく書く解放のコードは「commit 成功時の移動」1 か所だけになる。失敗・abort・期限切れ・接続断の
  経路は従来どおり drop に任せるので、移動を書き忘れた経路はカーソルを失うだけで外へは漏らさない（fail-closed）
- **名前空間は 1 つ**: Tx 登録簿と held 登録簿を通して名前は一意にする。held 登録簿にある名前での再 `DECLARE` は、
  既存の重複と同じ `22000`
- **個体識別子は接続単位で単調増加**にする。`next_id` を `CursorRegistry` から `SessionTransaction` へ移し、
  宣言のたびに接続全体で払い出す。現行は Tx ごとに 0 から払い出すため、`Idle` の portal 束縛（D7）では世代で区別できず、
  前の Tx から持ち越したカーソルと、後の Tx で `CLOSE` の後に同名で再宣言したカーソルの id が衝突しうる
  （savepoint の高水位は Tx 登録簿にしか作用しないので、held 登録簿には影響しない）
- held 登録簿のカーソルは確定済みの `QueryResult` だけを持ち、redb の `ReadTransaction`・write txn・writer の permit を持たない
  （持ち越した後は、MVCC のページ解放を妨げず、単一ライタも塞がない）
- `WITHOUT HOLD`（指定なしを含む）のカーソルは、従来どおり `ActiveTxn` と一緒に消える（WIRE-15 の契約を変えない）

### D2. Tx の終わり方ごとの扱い

- 行集合は `DECLARE` の時点で確定済みであり、**COMMIT は行集合を再評価せず、そのまま引き継ぐだけ**とする。
  PG17 は commit 時に残りの行を実体化するが、本 DB の行集合はもともと `DECLARE` の時点で確定している
  （INSENSITIVE）ため、返す行の集合は同じになる
- held 登録簿へ移すのは、`commit_active` が成功を返した後に限る（明示 Tx の `SessionTransaction::commit`。
  暗黙 Tx〔複数文メッセージ〕の中で `DECLARE` が実行される経路がある場合は `commit_implicit` も同じ。経路の有無は #3340 で確かめる）。
  write txn から読んだ行集合は未 commit の変更を含むため、commit が確定していない行を外へ出さないための規則である
- 持ち越した後は、トランザクションの持続時間の上限（`max_duration`）は掛からない（PG17 と同じ。時間の扱いは D5 と Q2）

| Tx の終わり方・出来事 | その Tx で宣言した `WITH HOLD` | 前の Tx から持ち越したカーソル |
| --------------------- | ------------------------------ | ------------------------------ |
| `COMMIT` 成功（明示・暗黙） | held 登録簿へ移す | 残る |
| `COMMIT` 成功の後、同期確認の失敗などで `state=may_be_committed` のエラーを返し、接続は続く場合 | 移したまま残す（commit は確定済み。PG17 と同じ） | 残る（暗黙 Tx の最後の文の `FETCH`／`MOVE` で動かした個体は、応答が届かないため D6 で閉じる） |
| `COMMIT` 成功の後、応答の書き込みに失敗して接続が終わる場合 | 接続の終了の行と同じく `SessionTransaction` の drop で消える | 同左 |
| `COMMIT` 失敗（遅延 `FOREIGN KEY` の `23503`・commit 自体の失敗・持続時間の上限超過の `54000`） | 移さず `ActiveTxn` と一緒に破棄 | 残る |
| `Failed` の中の `COMMIT` | 現行どおりエラーを返して状態を変えない（`Failed`／`FailedRecoverable` のまま）。後の `ROLLBACK` で破棄。`FailedRecoverable` から `ROLLBACK TO` で `Active` に戻り commit に成功すれば、その時点で残っているものを持ち越す（PG17 との差。下の表） | 残る |
| `ROLLBACK`（`Active`／`Failed`） | 破棄（PG17 と同じ） | 残る。位置の移動と `CLOSE` は巻き戻さない |
| `ROLLBACK TO SAVEPOINT` | savepoint より後に宣言したものは閉じる（既存の `close_declared_since`） | 残る。位置は巻き戻さない |
| 持続時間の上限超過（`release_if_expired`） | その場で `ActiveTxn` ごと破棄（`Failed` へ遷移し、`FailedRecoverable` からの遷移でも破棄） | 残る |
| 復帰できない文のエラー（`fail` で `Failed`。巻き戻せる savepoint が無い・内部エラー・直列化失敗） | その場で `ActiveTxn` ごと破棄 | 残る |
| 復帰できる文のエラー（`fail` で `FailedRecoverable`） | 保持する。以後は `ROLLBACK`（破棄）・`ROLLBACK TO`（savepoint より後のものだけ閉じる）の行に従う | 残る |
| 暗黙 Tx の失敗（`abort_if_implicit`） | その場で破棄 | 残る（取得結果を返せなかったものは D6 で閉じる） |
| `CLOSE <name>` | 閉じる | 閉じる（その場で held 登録簿から外す。後の `ROLLBACK` で戻らない） |
| `CLOSE ALL` | 閉じる | 閉じる（D3。#3340 で `WITH HOLD` と同時に入れる） |
| `DISCARD ALL`（Tx の外だけ） | — | 閉じる（Q5・#3545。実装されるまでは `CLOSE ALL` で代える） |
| 接続の終了（切断・`READ_TIMEOUT`・サーバーの終了） | `SessionTransaction` の drop で消える（D5） | 同左 |

### D3. 文の振り分け（どの登録簿を引くか）

- `Idle`（明示 Tx の外）の `FETCH`／`MOVE`／`CLOSE` は held 登録簿を引く。現行の「Tx 文脈の無い入口では一律 `34000`」は、
  held 登録簿に名前が無い場合の応答として残す。読み取り源には触れないので、スナップショットや writer は要らない
- 明示 Tx の `Active` の中（暗黙 Tx の中でカーソルの文が実行される経路があればそこも）では、Tx 登録簿の次に held 登録簿を引く
- `Failed` の中では、持ち越したカーソルへの文も既存の parse 前判定で `25P02` にする（PG17 と同じ）
- `CLOSE ALL` を受理し、Tx 登録簿と held 登録簿の全カーソルを閉じる。D4 のロールの束縛に関わらず全て閉じ、タグは
  `CLOSE CURSOR` で件数を返さない。`ALL` はカーソル名として既に拒否済み（`validate_cursor_name`）なので曖昧さは生じない。
  **#3340 の受け入れ条件に含め、`WITH HOLD` の受理と同時に出す**。`DISCARD ALL` が無い間、接続を使い回すプーラーが
  前の利用者のカーソルを消す手段は `CLOSE ALL` だけになるため
- **プーラーの利用条件**: `Failed` の中の `CLOSE ALL` は `25P02` で拒否され、`ROLLBACK` の後も前の Tx から持ち越したカーソルは
  残る。このため接続を次の利用者へ渡す前に、(1) Tx の中なら `ROLLBACK` を先に完了し、(2) `CLOSE ALL` の成功（`CLOSE CURSOR`）を
  確認する。どちらかが失敗した接続は再利用せずに破棄する。清掃に失敗した接続が同じロールの次の利用者へ渡ると、D4 のロールの
  照合を通って前の利用者の行が返るため、この手順を運用の前提として利用者向けの文書に書く（#3340）
- `WITHOUT HOLD`（指定なしを含む）の Tx 外の `DECLARE` は、従来どおり `25P01`
- `WITH HOLD` の Tx 外の `DECLARE` は受理する（Q1 で決定。PG17 の文書は `WITH HOLD` なしを Tx ブロックの外で
  エラーにすると明記しており、`WITH HOLD` は受理されると読めるが、直接の明記は無いため要実測）。1 文だけの
  暗黙の読み取りとして確定状態（`TxnReadSource::Committed` に相当）を読み、その場で held 登録簿へ入れる。
  課金（D5）・RLS と読み手の束縛（D4）・権限検査（SQL-40）は Tx 内と同じ判定点を通す
- 文数・持続時間の上限（`check_and_register_statement`）は、Tx の中で発行した文だけに掛かる（Tx 外の `FETCH` は数えない）

### D4. 行集合の確定と読み手の束縛

#### テナント

- 返す行は、**宣言したセッションのテナントの文脈で `DECLARE` の時点に確定した行だけ**とする。RLS は `DECLARE` の内側 SELECT で
  参照セッション自身の `PolicyContext` により暗黙適用済みで（RLS-7）、COMMIT も `FETCH` も行を足さない・入れ替えない
- テナントは接続の認証時に固定され、SQL から変えられない（[fixed-tenant-boundary.md](fixed-tenant-boundary.md)）。
  held 登録簿は接続単位の値なので、他の接続から名前を引く経路が無い。他セッションのカーソル名は不在と同じ `34000` で、
  文言にカーソル名や所有の有無を含めない（[sql-cursor.md](sql-cursor.md) §3 の契約を held 登録簿にも適用する）
- 持ち越した後に元の行の `UPDATE`・`DELETE`・`TRUNCATE` があっても、返す行は宣言時の行集合のまま（再実行しない。INSENSITIVE）。
  これらは行の内容の変化で、他テナントの行を足す方向には働かない
- 権限・可視性の前提が変わったことは、下の「権限と可視性の再検査」で検知してカーソルを閉じる（行を残したまま読ませ続けない）

#### 権限と可視性の再検査

持ち越したカーソルへの `FETCH`／`MOVE`／Describe／中断 portal の再開のたびに、宣言時の認可と可視性の前提がまだ成り立つかを
検査する。ロールの照合（下の節）を通った後に行う。

- **権限の再検査**: 宣言時に SQL-40 の検査（`sql::privilege_gate` の `read` による参照先の収集と `check`）へ渡した要求
  （relation の内部 ID・要求した列・必要な権限）をカーソルに記録しておき、同じ判定をもう一度行う。判定は宣言時と同じ関数を
  通すので、ロールのメンバーシップによる継承、スキーマの `USAGE`、列単位の付与、所有者の変更も宣言時と同じ規則で反映される。
  relation ごとの変更回数のような別の仕組みは作らない（権限に効く依存を漏れなく数え上げる必要が無くなる）
- **可視性の再検査**: 宣言時に内側 SELECT へ適用した行セキュリティの前提（参照した relation ごとの、適用した利用者ポリシーの
  集合とその定義、行セキュリティの有効・無効、`row_security` の値）の指紋を記録し、同じ指紋を計算し直して比べる。
  行を絞り直すことはしない（INSENSITIVE を保つ）
- **照合の基準**: どちらも呼び出し元の Tx のスナップショットではなく、**照合の時点の確定済みカタログ**で行う
  （`BEGIN` 時点のスナップショットを読む明示 Tx の中でも、他の接続が確定させた `REVOKE` を見逃さない）。呼び出し元の Tx が
  未 commit の権限・ポリシーの変更を持つ場合は、その Tx の見え方でも判定し、両方を満たすときだけ通す（厳しい側に倒す）
- **テナントの範囲**: 判定は宣言したセッションのテナントの文脈で行う。ACL・利用者ポリシー・所有者はテナントごとに持つため、
  他テナントの `GRANT`／`REVOKE`・ポリシーの変更は判定の結果を変えない。他テナントの DDL の有無は、この経路から推せない
- **参照先が無い**: 記録した relation が `DROP` などで解決できない場合は、前提が崩れたものとして扱う（「変化なし」に倒さない）
- **結果**: 権限の再検査が通らなければ固定文言の `42501`、可視性の指紋が一致しない・参照先が解決できなければ固定文言の
  `55000`（object_not_in_prerequisite_state）を返し、**どちらもそのカーソルを閉じる**（以後その名前は `34000`）。後退走査の
  拒否の `55000`（`cursor can only scan forward`）とは文言で区別し、そちらはカーソルを残す（D6）
- **判定順**: 名前とロールの照合（`34000`。閉じない）→ 権限の再検査（`42501`。閉じる）→ 可視性の再検査（`55000`。閉じる）→
  既存の判定（後退走査の `55000`・`ALL` の `54000` など。閉じない）
- Tx 登録簿のカーソル（宣言した Tx の中）には掛けない（既存の契約。寿命が Tx の持続時間の上限以下のため）。COMMIT の時点では
  検査せず、持ち越した後の操作のたびに検査する
- 権限の変化が広げる方向（`GRANT` の追加など）なら、権限の再検査は通る。可視性の指紋はポリシーの追加・削除のどちらでも変わり、
  向きを区別せずに閉じる（fail-closed）

#### ロール（Q3 で案 A に決定）

- **各カーソルに宣言時の `current_role_id` と `session_role_id`（`SessionState` のアクセサ）を記録する**。持ち越したカーソルへの
  `FETCH`／`MOVE`／`CLOSE <name>`／Describe の時点で、現在の値がどちらか一方でも記録と一致しなければ、**不在と同じ `34000`
  の固定文言**で拒否する（`42501` にしないのは、カーソルの存在を知らせないため）。位置は変えず、カーソルは残す（中断 portal の再開を拒む場合だけは、保持行を捨てるため個体を閉じる。D6・D7）。元のロールへ
  戻れば（`RESET ROLE` など）再び使える。`CLOSE ALL` はロールに関わらず全て閉じる（D3）
- `SET SESSION AUTHORIZATION` は現状受理する経路が無く、`session_role_id` は認証層だけが束縛する。記録に含めておくのは、
  将来受理したときに照合を足し忘れないため
- 照合は held 登録簿のカーソルに掛ける。宣言した Tx の中のカーソル（Tx 登録簿）は、既存の契約どおり `DECLARE` 後のロール変更の
  影響を受けない（[role-privilege-model.md](role-privilege-model.md) §12。SQL-40）。宣言した Tx の中で `SET ROLE` してから
  COMMIT したカーソルは、宣言時のロールで記録しているため、持ち越した後は宣言時のロールに戻るまで `34000` になる
- 理由: Tx 内のカーソルが `DECLARE` 後のロール変更の影響を受けないのは、寿命が Tx の持続時間の上限（既定 20 秒）以下だからで、
  寿命に上限の無い持ち越しカーソルには同じ理屈が成り立たない。照合しないと、高いロールで確定した行を、`SET ROLE` で権限を
  落とした後も同じ接続で読み続けられる（ロールを利用者ごとに切り替えて 1 つの接続を共有する使い方で問題になる）
- 残る観測: 照合で隠れたカーソルと同じ名前の `DECLARE` は `22000`、隠れたカーソルの課金で上限に達すれば `54000` になり、
  名前の存在や保持量の多寡は同じ接続から推せる。行の中身は出ない。これも無くすなら下の案 B

| 案 | 権限を落とした後の読み取り | 隠れたカーソルの存在の観測 | 元のロールへ戻った後 | PG17 との互換 | 実装 |
| -- | -------------------------- | -------------------------- | -------------------- | ------------- | ---- |
| **A（採用。Q3）**: 宣言時のロールを記録し、不一致は `34000` | できない | 名前の衝突（`22000`）・上限（`54000`）から推せる | 使える | 差（PG17 は返す） | 記録 2 値と照合 1 か所 |
| B: `SET ROLE`／`RESET ROLE`（将来は `SET SESSION AUTHORIZATION` も）でロールが実際に変わったら held 登録簿を全て閉じる | できない | 推せない（閉じているため） | 使えない（閉じている） | 差（PG17 は残す） | ロール変更の経路ごとに閉じる処理。閉じるのは巻き戻らないため、後で `ROLLBACK` された `SET ROLE` でも閉じたまま |
| C: 見え方を変えない（PG17 どおり） | できる | — | 使える | 一致 | 追加なし |

### D5. 課金（本数・概算バイト）と常駐の見積り

- 1 本あたりの行数は、従来どおり `DECLARE` の時点で結果行数の上限の中に収まる（[limit-omitted-result-cap.md](limit-omitted-result-cap.md) D5）。
  持ち越しで行数は増えない
- **上限は Tx 登録簿と held 登録簿の合算**で、既存の 16 本・16 MiB をそのまま使う（新しい定数を作らない。
  合算にしないと、持ち越すたびに上限が実質 2 倍・3 倍になる）。`ensure_capacity_for_declare`（本数）と
  `remaining_bytes`（生成予算）は両方の登録簿の合計で判定する
- **超過は `DECLARE` の時点の `54000`** とし、判定点と文言は既存と同じにする。行は `DECLARE` の時点で確定し課金済みなので、
  COMMIT 時に新しく課金するものは無い。このため **上限超過で COMMIT が失敗することも、COMMIT でカーソルが黙って閉じることも無い**
- **16 MiB は実メモリではない**: 課金は `estimate_result_bytes`／`estimate_cell_bytes` による値の長さの概算で、`Vec`・`String`
  の確保単位、行ごとの入れ物、列メタデータ、アロケータの余白を含まない。実メモリは概算より大きい。**#3340 の受け入れ条件で、
  代表的な列型の組み合わせについて概算と実メモリの係数を実測して記録する**
- 概算での最悪の常駐量は、接続あたり 16 MiB × 同時接続数（既定 64・ハード上限 256）で最大 4 GiB。実メモリはこれに上の係数を
  掛けた値になる。中断 portal の上限（`MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`）とは別枠のまま。一時表
  （[temp-table.md](temp-table.md) の総バイト数の上限）と同じく接続単位の上限で抑え、プロセス全体の上限は当面設けない。
  係数の実測後に Q2 で再判断する
- **時間**: 無通信の接続は `limits::READ_TIMEOUT`（既定 30 秒）で閉じられる。この値は `--read-timeout-secs` で最大 600 秒
  （`limits::MAX_READ_TIMEOUT_SECS`）まで延ばせるため、無通信のまま保持できる時間は設定次第で最大 10 分になる。
  要求を送り続ける接続では、カーソルをいつまでも保持できる（PG17 と同じ。Q2）
- **切断時の破棄（コードで確認）**: 接続ごとに `server::accept_loop_with_limiter`（`crates/wire-server/src/server.rs`）が
  `std::thread::spawn` でスレッドを起こし、`handshake::handle_connection_with_session_limits` を呼ぶ。`SessionState`
  （`let mut session`）と `SessionTransaction`（`let mut txn = engine.map(|e| e.new_session_transaction())`）は、この接続ハンドラ
  （`crates/wire-server/src/handshake.rs`）のローカル変数で、読み取りタイムアウトや切断で `Err` が返るとハンドラを抜けて drop される
  （同所のコメントは、未 commit の write txn の abort もこの drop に依ると明記している）。held 登録簿を `SessionTransaction` に
  置けば同じ drop で消える。コードを読んでの確認であり、#3340 の層 B テストで切断・タイムアウト後の解放を固定する

### D6. 失敗した取得の扱い

持ち越したカーソルの `FETCH`／`MOVE` の失敗と、応答を届けられない場合を次のように扱う。

- **位置を動かす前の拒否**（`34000`・後退走査の `55000`・`ALL` の `54000`・構文と件数の `42601`／`22000`）: 位置は変わらないので、
  カーソルは使える状態で残す。Tx 内カーソルの `ROLLBACK TO` 後の扱い（[sql-cursor.md](sql-cursor.md) 「PG17 との意図した差」の
  最終行）と同じ既存の契約に揃えたもので、PG17 との差として扱う（要実測）
- **再検査による閉鎖**（D4 の `42501` と、前提の崩れの `55000`）: 位置を動かす前に判定するが、カーソルを閉じる。同じ `55000` でも
  後退走査の拒否とは文言と扱いが違う
- **応答を届けられない場合は、その応答を作った個体を閉じる**: 現行の `CursorRegistry::fetch` は送出の確定を待たずに位置を進め、
  応答が届かなければ Tx が `Failed` になるか接続が切れて登録簿ごと消えることを前提にしている。持ち越したカーソルは Tx と一緒に
  消えないため、位置だけ進んで応答（行・`MOVE n`）が届かないと、次の `FETCH` が未受信の行を飛ばす。そこで、位置を進めた操作の
  応答を送らずに捨てる経路では、**束縛した `cursor_id` の個体だけ**を閉じる（以後 `34000`。名前ではなく id で閉じ、同名の別個体に
  影響させない）。位置を戻す案は、エンジンの位置の巻き戻しを wire 層から呼ぶ経路を新たに作るため採らない。対象の経路は次のとおり
  - wire 層の応答の生成・送出の失敗（中断 portal のバイト上限の `54000` など）
  - 暗黙 Tx の最後の文（`simple_query::run_statement_as` の `ImplicitFinal`）が持ち越したカーソルへの `FETCH`／`MOVE` で、その後の
    `commit_implicit` が失敗した場合。`ImplicitFinal` は文の実行の後、結果を返す前に commit する
  - 同じく `ImplicitFinal` の commit は成功したが、同期確認の失敗などで成功の応答が `state=may_be_committed` のエラーへ差し替わった場合
  - 中断（`Suspended`）した `FETCH` portal の保持行を送らずに捨てる場合（再開時の照合の失敗で portal を `Failed` にする、`Idle` の
    Sync での破棄、portal の Close。D7）
- **応答を届け終えた操作は対象外**: 暗黙 Tx の途中の文（`ImplicitIntermediate`）は後続の文や commit より先に結果を送るため、
  その後に commit が失敗しても、その `FETCH`／`MOVE` で動いたカーソルは閉じない（D2 のとおり残る）。明示 Tx の中の `FETCH` も
  文ごとに応答を返してから `COMMIT` を受けるため、`COMMIT` の失敗では閉じない
- 接続が切れる場合は従来どおり登録簿ごと消える

### D7. wire 層（拡張クエリ・Describe・状態バイト）

- **portal 束縛**: 持ち越したカーソルの `FETCH` から作った portal も、中断（`Suspended`）からの再開時に照合する。
  `CursorBinding` の世代を `Option<u64>` として束縛時の `active_generation()` をそのまま記録し、再開時は
  「世代が束縛時と一致し、かつ `cursor_id` が一致する」ときだけ送出する。`Idle` で作った portal は世代が `None` 同士で
  一致するため、`CLOSE`・同名の再宣言は `cursor_id`（D1 で接続単位に一意）で弾く。現行は `Idle` では束縛を作らない
  （`active_generation()` が `None` なので束縛が `None` になり、照合の対象外になる）ため、#3340 でこの穴を塞ぐ。
  D4 のロールの照合と再検査も再開時に行う。再開を拒む場合は保持行を送らずに捨てるので、ロールの不一致（`34000`）でも、
  束縛した個体を閉じる（D6。保持行を捨てたまま残すと、`RESET ROLE` の後の `FETCH` が未送信の行を飛ばすため）
- portal 自体の寿命は変えない。portal は PG17 と同じく保持の対象ではなく、Tx の終了・Sync で既存の規則どおり破棄する
  （`purge_expired_portals`）。持ち越すのはカーソルの行集合と位置だけ。持ち越したカーソルに束縛した中断 portal を、保持行を
  残したまま破棄する場合（Sync・Close・Tx の終了）は、束縛した個体を閉じる（D6）
- **応答の失敗**: D6 の「応答を届けられない場合」の各経路で、wire 層は束縛した `cursor_id` のカーソルを閉じるよう engine へ求める
  （名前ではなく id で閉じ、同名の別個体を閉じない）。簡易クエリ・拡張クエリの両方の経路が対象
- **Describe**: `describe_parsed_in_txn` の `FETCH` の結果列は、Tx 登録簿の次に held 登録簿を引く。`Idle` でも返す（D4 の照合を通す）
- **状態バイト**: 持ち越したカーソルがあっても、Tx の外の `ReadyForQuery` は `I`（PG17 と同じ）

### D8. エラー契約（追加・変更分）

| 事象 | `wire_code` | 備考 |
| ---- | ----------- | ---- |
| `WITH HOLD` の受理 | 成功（`DECLARE CURSOR`） | 現行の `0A000` を外す。`BINARY` の `0A000` は変えない |
| `CLOSE ALL` | 成功（`CLOSE CURSOR`） | 現行の拒否を外す（D3） |
| 合算の本数・バイト数の超過 | `54000` | `DECLARE` の時点だけで判定する。COMMIT は上限で失敗しない |
| held 登録簿と同名の `DECLARE` | `22000` | 既存の重複と同じ |
| 名前が無い・宣言時のロールと不一致 | `34000` | 既存と同じ固定文言。両者を区別しない（D4） |
| 権限の再検査が通らない | `42501` | 固定文言。カーソルを閉じる（D4） |
| 可視性の前提が崩れた・参照先が解決できない | `55000` | 後退走査の拒否とは別の固定文言。カーソルを閉じる（D4） |
| `Failed` の中の持ち越しカーソルへの文 | `25P02` | 既存の parse 前判定 |
| `WITHOUT HOLD` の Tx 外の `DECLARE` | `25P01` | 変更なし |
| `WITH HOLD` の Tx 外の `DECLARE` | 成功（Q1） | 1 文だけの暗黙の読み取りで確定する（D3） |
| `WITH HOLD` と行ロック句 | `42601` | カーソルの行ロック句は既に `42601`（`sql38_row_lock_clause.rs`）。PG17 の SQLSTATE は要実測 |
| `DISCARD ALL`（未実装の間） | 現行どおり拒否 | Q5。実装は #3545 |

- 文言は英語の固定文言で、カーソル名・件数・他の状態を含めない（既存の方針）

## 代替案（却下）

| 案 | 却下の理由 |
| -- | ---------- |
| 登録簿を 1 つにまとめて `SessionTransaction` へ移し、各経路で `hold` 以外を明示的に消す | 解放が「全ての終了経路で消すコードを書く」形になり、1 経路の書き忘れで `WITHOUT HOLD` のカーソルが Tx の後に残る（fail-open）。D1 の 2 層なら新しい経路は commit 成功時の移動だけで、書き忘れはカーソルを失う側に倒れる |
| held 登録簿を `SessionState` に置く | `BEGIN` ごとに行集合を複製し、`ROLLBACK` で位置と `CLOSE` まで巻き戻す（PG17 と食い違う）。D1 参照 |
| COMMIT 時に上限を判定し、超過したら COMMIT を失敗させる | カーソルの都合で書き込みまで取り消すことになる。行は `DECLARE` で課金済みなので、判定点を増やす必要が無い |
| COMMIT 時に上限を超えた分のカーソルを黙って閉じる | 利用者に知らされずにカーソルが消え、後の `FETCH` が `34000` になる。原因が分からない失敗を作る |
| held 用に別枠の上限（例: 持ち越し専用の 16 MiB）を設ける | 接続あたりの常駐量の上限が実質 2 倍になる。既存の「接続あたり 16 MiB」の説明が成り立たなくなる |
| COMMIT 時に行集合を取り直す（PG17 の実体化の時点に合わせる） | `DECLARE` の時点で確定する契約（WIRE-15）を崩し、同じ Tx の後続の書き込みが混ざる |
| `FETCH` のたびに権限・ポリシーを再評価し、外れた行を除く | 再実行無しでは行を絞り直せない（INSENSITIVE を崩す）。読み手の制約は D4 のロールの照合（行ではなくカーソル単位）で掛ける |
| ロールの不一致を `42501` で拒否する | 隠したいカーソルの存在を知らせる。`34000`（不在と同じ）にする（D4） |
| 権限・ポリシーの変更があっても返し続ける（PG17 どおり） | 寿命に上限の無い持ち越しカーソルでは、`REVOKE` の後も同じロールで行を読み続けられる（D4） |
| relation ごとの変更回数（世代）を新設し、進んでいたら閉じる | 権限に効く依存（ロールのメンバーシップ・スキーマの `USAGE`・列単位の付与）を漏れなく数え上げる必要があり、漏れが fail-open になる。テナントごとの ACL を 1 つの回数で数えると他テナントの DDL で閉じ、その有無も漏れる。宣言時と同じ判定関数で再検査する（D4） |
| 権限の検査だけ行い、利用者ポリシーの変更は見ない | ポリシーの変更は行を絞り直さないと反映できない。可視性の指紋で検知して閉じる |
| 再検査を呼び出し元の Tx のスナップショットで行う | `BEGIN` 時点のスナップショットを読む Tx では、他の接続が確定させた `REVOKE` を見逃す。確定済みカタログで行う |
| `DISCARD ALL` を「カーソルを閉じるだけ」で先に受理する | 他のセッション状態（ロール・`search_path`・一時表・採番状態など）が残り、接続を使い回すプーラーで前の利用者の状態を次の利用者へ持ち越す。部分実装は受理しない（Q5） |

## 影響

- engine: `CursorRegistry`（`hold`・宣言時のロール・id の払い出し元）、`SessionTransaction`（held 登録簿・id カウンタ・commit 成功時の
  移動・Tx 外の取得口・`CLOSE ALL`）、`core.rs`（Tx 外の `FETCH`／`MOVE`／`CLOSE` の分岐、`WITH HOLD` の `0A000` の撤去、
  ロールの照合、権限と可視性の再検査、Describe）、`sql::privilege_gate`（記録した要求での再検査の入口）、
  `simple_query`・`extended_query`（応答を届けられない場合の持ち越しカーソルの閉鎖）
- wire-server: `extended_query::CursorBinding` の世代の持ち方と照合、応答失敗時のカーソルの閉鎖
- 既存の契約: `WITHOUT HOLD` の寿命、上限の値、`34000`・`22000`・`25P01`・`25P02` の判定点は変えない。
  [sql-cursor.md](sql-cursor.md) の「カーソルの寿命はトランザクションの寿命以下」は `WITHOUT HOLD` に限った記述になり、
  `CLOSE ALL` の拒否の記述は受理へ変わる（実装の段で同ファイルを更新する）
- 拒否を固定している既存テスト（`cursor.rs` の単体テストの `WITH HOLD`／`CLOSE ALL` の拒否、SQL 経由の同種のテスト）は、
  #3340 で受理後の挙動へ書き換える（期待値は弱めない）
- spec 側（WIRE-27 (6) のポインタ先）の更新は spec リポ側の作業として、承認後にオーナーへ申し送る

## 実装 Issue の分け方（案）

既存の #3339 の子 2 件を使い、範囲を次のように確定する（Q1〜Q6 の決定を反映済み）。

### #3340: 宣言と COMMIT 後の持ち越し

- 範囲: `WITH HOLD` の受理、D1 の 2 層の登録簿・接続単位の id、D2 の commit 成功時の移動、D3 の振り分けと **`CLOSE ALL`
  （`WITH HOLD` と同時に出す）**、D4 のロールの記録と照合（Q3 の案 A）、D5 の合算上限、D7 の portal 束縛と Describe、
  Tx 外の `DECLARE ... WITH HOLD`（Q1）
- 受け入れ条件に足すもの: `CLOSE ALL` が持ち越したカーソルを含めて全て閉じること。概算バイトと実メモリの係数の実測値の記録（D5）
- テスト観点（層 A: engine）
  - COMMIT 後に `FETCH`・`MOVE`（`SCROLL` の逆方向を含む）が宣言時と同じ行・位置で動く
  - `WITHOUT HOLD` と指定なしは COMMIT で消える（以後 `34000`）
  - held 登録簿の本数・バイト数を含めて 17 本目・16 MiB 超が `DECLARE` の時点で `54000` になり、COMMIT は成功する
  - held 登録簿と同名の `DECLARE` が `22000`
  - 前の Tx から持ち越したカーソルが、後の Tx の savepoint の `ROLLBACK TO` で閉じない
  - writer 取得後に宣言したカーソルの行集合（未 commit の変更を含む）が、COMMIT 成功後にだけ持ち越される
  - `SET ROLE` の後の `FETCH`／`MOVE`／`CLOSE <name>` が不在と同じ `34000` になり、`RESET ROLE` の後に同じ位置から再開できる。
    `CLOSE ALL` はロールに関わらず全て閉じる
  - 別セッションが参照先の `REVOKE`（表単位・列単位・ロールのメンバーシップ・スキーマの `USAGE`）を commit した後の
    `FETCH`／`MOVE` が `42501` になり、カーソルが閉じる（以後 `34000`）。`BEGIN` 時点のスナップショットを読む明示 Tx の中でも同じ
  - 利用者ポリシーの変更・行セキュリティの切り替え・`DROP` の後は `55000` で閉じる。行の `DELETE` だけでは閉じない
  - 他テナントの `GRANT`／`REVOKE`・ポリシーの変更では閉じない
  - 後退走査の拒否の `55000` ではカーソルが残る
  - 暗黙 Tx の最後の文の `FETCH` の後に commit が失敗した場合と、commit 後に成功の応答がエラーへ差し替わった場合、その個体が閉じ、
    未受信の行を飛ばして再開しない。途中の文の `FETCH`（応答済み）で動いたカーソルは閉じない
- テスト観点（層 B: wire-server）
  - psql と同じ形（末尾 `;` 付きの単独送信）で `BEGIN` → `DECLARE ... WITH HOLD` → `COMMIT` → `FETCH` が通る。`CLOSE ALL` も同じ形で通る
  - 拡張クエリの `max_rows` で中断した `FETCH` portal の再開を、`Idle` で `CLOSE`・同名の再宣言・`SET ROLE` を挟んだ場合は `34000` で拒否する
  - Describe（Statement／Portal）が `Idle` の `FETCH` で列を返す。状態バイトは `I`
  - `Idle` で持ち越しカーソルの `FETCH` の応答生成が失敗した場合（中断 portal のバイト上限の超過など）、そのカーソルが閉じ、
    次の `FETCH` が受け取っていない行を飛ばして再開しない
  - `Failed`（`FailedRecoverable`）の中の `COMMIT` の後、`ROLLBACK TO` で復帰して commit に成功した場合に、savepoint より前に
    宣言した `WITH HOLD` だけが持ち越される
  - 切断と `READ_TIMEOUT` による切断の後、held 登録簿が破棄される（D5 のコード上の確認を固定する）
  - 中断した `FETCH` portal の再開を、別セッションの `REVOKE` の commit の後に行うと `42501` で、行を送らず個体を閉じる
  - 中断した `FETCH` portal を、`SET ROLE` を挟んだ再開・`Idle` の Sync・portal の Close で捨てると、束縛した個体が閉じ、
    同名で再宣言した別の個体は閉じない
  - プーラーの手順（`ROLLBACK` → `CLOSE ALL` の成功の確認）の後、同じロールで接続し直した利用者から前のカーソルが見えない

### #3341: 解放と越境試験

- 範囲: D2 の各出来事での解放と、その後に登録簿が空になることの確認。D4 の越境試験。PG17 の実測による「要実測」項目の確定
- テスト観点（層 A）
  - `ROLLBACK`・COMMIT の失敗（`23503`・持続時間の上限超過）・`Failed` 後の `ROLLBACK` で、その Tx の `WITH HOLD` が破棄され、
    前の Tx から持ち越したカーソルは位置を保って残る
  - COMMIT 成功の後、`state=may_be_committed` のエラーを返して接続が続く場合は持ち越したカーソルが残り、応答の書き込みに
    失敗して接続が終わる場合は破棄される
  - 持続時間の上限超過と復帰できない文のエラーでは、その Tx の `WITH HOLD` がその場で破棄される
  - 後の Tx の中の `CLOSE` が `ROLLBACK` で戻らない
  - `SessionTransaction` の drop（切断）で held 登録簿が空になる
- テスト観点（層 B）
  - 3 テナントの越境: 各テナントの接続で宣言・持ち越したカーソルに他テナントの行が現れない。他テナントの行を増減させても
    持ち越したカーソルの応答が変わらない（WIRE-27 (4) のポインタ先）。持ち越した後の元の行の `DELETE` の後も同じ。
    利用者ポリシーの変更・`DROP TABLE` の後は、他テナントの行を返さずに `55000` で閉じる。他テナント側の権限・ポリシーの
    変更では閉じない
  - 他の接続から同じ名前を `FETCH`／`CLOSE` すると `34000`（不在と区別できない）
  - 再接続した接続から前の接続のカーソルが見えない
- PG17 の実測（`crates/engine/tests/data/` の既存の突き合わせの形に揃える）: 下の「PG17 との差」の要実測の項目

### #3545: `DISCARD ALL`

- `DISCARD ALL` の実装（Q5）。セッションの全状態の初期化と、Tx 内の拒否、held 登録簿の解放をまとめて扱う

### 新規の候補（未起票。起票はオーナーの承認を経る）

- プロセス全体の常駐量の上限・保持時間の上限（係数の実測後に Q2 で必要と判断した場合）

## PG17 との差（意図した差・要実測）

| 形 | PG17 | 本 ADR | 扱い |
| -- | ---- | ------ | ---- |
| 実体化の時点 | commit 時 | `DECLARE` 時（COMMIT は引き継ぐだけ） | 意図した差。返す行の集合は同じ（D2） |
| ロールを変えた後の持ち越しカーソル | 見え方は変わらない（実体化した行を返す） | 不在と同じ `34000`（案 A） | Q3 の決定による意図した差 |
| 参照先の権限・ポリシーの変更の後の持ち越しカーソル | 実体化した行を返し続ける | 権限なら `42501`、可視性の前提なら `55000` で閉じる（D4） | 意図した差（寿命に上限が無いため） |
| 中断した `FETCH` portal の保持行を捨てた後 | 要実測 | 束縛した個体を閉じる（D6） | 未受信の行を飛ばさないための差 |
| `Failed` の中の `COMMIT` | Tx を `ROLLBACK` として終わらせる（その Tx の `WITH HOLD` は破棄。要実測で確認） | エラーを返して `Failed` のまま。`ROLLBACK TO` で復帰すれば持ち越せる | 既存の Tx の状態機械（[explicit-transaction.md](explicit-transaction.md)）の契約を維持した結果の差 |
| 持ち越したカーソルの `FETCH` が失敗した後 | 要実測（abort させたカーソルは以後使えない、という savepoint の記述がある） | 後退走査の `55000` など位置を動かす前の拒否なら残す。再検査の `42501`／`55000` と、応答を届けられない場合は閉じる | 既存の契約に揃える（D6）。実測で差を確定する |
| 後の Tx の `ROLLBACK` 後の位置・`CLOSE` | 要実測（savepoint については巻き戻さないと明記） | 巻き戻さない | 実測で一致を確認する |
| `DISCARD ALL` の Tx 内での発行 | 実行できない（SQLSTATE は要実測） | Q5 | 実装する場合は実測の SQLSTATE に揃える |
| `WITH HOLD` と行ロック句 | 併用不可（SQLSTATE は要実測） | `42601`（カーソルの行ロック句は既に拒否） | 実測後に差として記録する |
| 指定なしの逆方向・`42P11`・`42P03` | — | 既存の差のまま | [sql-cursor.md](sql-cursor.md) の表のとおり |

## 決定（オーナー、2026-11-21）

判断事項 Q1〜Q6 は、オーナーがすべて提案どおりに決定した。

- **Q1（Tx の外の `DECLARE ... WITH HOLD`）**: 受理する（D3）。PG17 の挙動は #3341 で実測し、差があれば「PG17 との差」に記録する
- **Q2（保持時間・プロセス全体の常駐量の上限）**: 当面は設けない。#3340 で概算バイトと実メモリの係数を実測した後に再判断する
  （D5。概算 16 MiB は実メモリの下限でしかないため）。設ける場合の候補は、持ち越したカーソルの無操作の時間の上限（超過で閉じる）と、
  プロセス全体の合計の上限（超過は `DECLARE` の `54000`）
- **Q3（ロールを変えた後の読み取り）**: **案 A** を採る。宣言時の `current_role_id`・`session_role_id` を記録し、持ち越した後に
  一致しなければ不在と同じ `34000` で拒否する（D4）。名前の衝突・上限から隠れたカーソルの存在を推せる余地は残る（D4「残る観測」）
- **Q4（`CLOSE ALL`）**: 受理し、#3340 に含めて `WITH HOLD` と同時に出す（D3）
- **Q5（`DISCARD ALL`）**: セッションの全状態の初期化を伴う別 Issue（#3545）で扱い、それまでは現行どおり拒否する。#3341 の解放の契機からは
  外す（部分実装は却下。「代替案」節）。**`DISCARD ALL` が入るまでは、プーラーを使う構成では `CLOSE ALL` による明示的な消去が必要**
  （プーラーのリセット文に `CLOSE ALL` を設定する運用になる。`ROLLBACK` を先に完了し、`CLOSE ALL` の成功を確かめてから再利用し、
  失敗した接続は破棄する。D3「プーラーの利用条件」）
- **Q6（上限の値）**: 16 本・16 MiB を据え置く。[sql-cursor.md](sql-cursor.md) の対象外の「上限の既定値の確定」と、Q2 の係数の
  実測と合わせて見直す

## 判断記録

| 項目 | 内容 |
| ---- | ---- |
| 判断 | Q1〜Q6 はすべて提案どおり（Q3 は案 A）。D1〜D8 は上の決定を前提とする |
| security-auditor の監査 | P1 3 件（プーラーでの持ち越しカーソルの消去手段・ロールを落とした後の読み取り・COMMIT 成功後の応答失敗）と P2 の指摘を本 ADR に反映済み |
| PR レビューでの追加 | PR #3544 のレビュー指摘で、D4「権限と可視性の再検査」（参照先の権限・ポリシーの変更で持ち越しカーソルを閉じる）、D3「プーラーの利用条件」、D6 の「応答を届けられない場合は閉じる」規則、D2 の表の細分化を加えた。D4 の再検査の規則は Q1〜Q6 の後に加えた規則のため、マージ前にオーナーの確認を受ける |
| 判断日 | 2026-11-21（Q1〜Q6） |
| 承認 | 本 ADR を含む PR のマージをもって Accepted とする |

## 対象外

- NoSQL 表層のカーソル（カーソルを持たない。[sql-cursor.md](sql-cursor.md) の対象外のまま）
- portal 自体の保持（PG17 の保持の対象もカーソルだけ）
- 持ち越したカーソルの行集合の永続化（再起動・crash を越えて残すこと。PG17 もセッションの終了で消える）
- `pg_cursors` 相当のカタログ（システムカタログの ADR 側で扱う）
