# 明示トランザクション BEGIN / COMMIT / ROLLBACK（Issue #942）

- ステータス: **Accepted（実装既定値）**
- 対応: Issue #942・TASK-221
- 改訂: Issue #2396・TASK-333（決定 1 を改訂。writer の遅延取得と `40001` の判定は
  下記 1a。実装は #2399〜#2405・#2408 で進行中。#2399・#2400・#2401・#2404・#2405・#2412 は実施済み。設計記録は #2397）
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-31・
  `docs/spec/04-behavior/persistence.md` RECOVER-12・
  `docs/spec/04-behavior/error-format.md` ERR-6
- 関連ポインタ: SQL-38・SQL-19・ERR-7・PERSIST-4・WIRE-15・RLS-7・TASK-335・RECOVER-5・RECOVER-6・RECOVER-8・TASK-96・TASK-97・TASK-99・
  TABLE-3・SQL-18（SQL 表層の 0 行 DELETE も台帳記録。#983）・WIRE-16（複数文実行。
  `wire-multi-statement.md`）・WIRE-19（`ReadyForQuery` 状態バイト。PR #1041
  レビュー指摘対応で #943 の担当分を本 PR へ吸収し実装済み）

## 背景・目的

各書き込み文（`INSERT`／`UPDATE`／`DELETE`／`TRUNCATE`／UPSERT）は、`tenant.rs`
の `*_unchecked` 関数の中で `Storage::begin_write_txn()` → 書き込み →
`commit_boundary::commit` まで自己完結していた（autocommit）。`BEGIN` などは
許可リストに無いため `42601` で拒否され、複数文メッセージ（Issue #938・
`wire-multi-statement.md`）では書き込み文は最後の 1 文に限られていた。

本 Issue は、簡易クエリ・拡張クエリの両経路で `BEGIN`〜`COMMIT`／`ROLLBACK`
を受理し、複数の書き込み文を 1 つの redb 書き込みトランザクションにまとめて
原子的に commit／破棄できるようにする。

## 設計の要点（判断記録）

### 1. `BEGIN` の時点で redb の write txn を取得して保持する（改訂前の判断）

> 2026-10 の改訂（Issue #2396）で、下記 1a に置き換わる（Explicit のみ。
> Implicit は従来どおり開始時に取得する）。以下は経緯として残す改訂前の判断。

`redb::WriteTransaction`（`=4.2.0`）はライフタイム引数を持たず、全フィールドが
`Arc`／`Mutex`／`Atomic` なので `Send`。`Drop` は abort 相当（panicking 中を
除く）。単一ライタ（TABLE-3）なので、`BEGIN` で write txn を取得すれば
`COMMIT`／`ROLLBACK` までの間に他者は commit できない。台帳
（`recovery::ledger::record_in_txn`）も同じ write txn 内で書かれるため、
「台帳エントリが `COMMIT` と同じトランザクションで永続化され、`ROLLBACK`・
接続断で消える」（RECOVER-12）は追加コードなしで成立する。

代償: 読み取り専用のトランザクションでも単一ライタを占有する。既知のトレード
オフとして記録する（#2396 で解消予定。1a 参照）。

### 1a. writer の遅延取得と `40001` の判定（Issue #2396・TASK-333）

決定 1 の代償（読み取りだけの明示トランザクションも、他セッションの書き込みを
待機上限まで待たせる）を解消するため、writer の取得を最初の書き込み文へ遅らせる。
取得前の読み取りが古くなった場合は `40001`（serialization_failure）として判定する
（1.0 前の BREAKING CHANGE。ポインタ: SQL-31・SQL-38・SQL-19）。本節は後続の実装
Issue（#2399〜#2405・#2408〜#2413）が拠り所にする設計記録で、実装前の判断を固定する。

#### 1a-1. writer の取得時点

- `BEGIN` では writer を取らない。`BEGIN` 時点の確定済み `redb::ReadTransaction`
  （読み取りスナップショット）だけを開く。
- writer は最初の書き込み文で取得する。書き込み文は DML（INSERT・UPDATE・DELETE・
  TRUNCATE・UPSERT・ファイル形 INSERT・COPY FROM）、ロック句、`LOCK TABLE`。
- トランザクション内の DDL は、これまでどおり writer を取得する前に `0A000` で拒否する。
- 読み取りだけのトランザクションは writer を一度も取得しないため、他セッションの
  書き込みを待たせない。
- 暗黙のトランザクション（WIRE-16）は対象外。従来どおり開始時に取得する（#2401）。

#### 1a-2. スナップショットの切り替え（SQL-19）

- writer を取得する前の読み取り: `BEGIN` 時点のスナップショット。
- writer を取得した後の、書き込み文の述語・対象行の評価とそれ以降の読み取り:
  取得時点の最新の確定状態＋自トランザクションの未 commit 変更（#2403）。

#### 1a-3. 判定の時点と順序

最初の書き込み文で、次の順に処理する。

1. writer gate を取得する（待機上限を超えたら `55P03`）。
2. permit を保持したまま `40001` を判定する。
3. 該当すれば write txn を書き込み前に drop（abort）し、permit を解放して `Failed`
   へ遷移し、`40001` を返す。
4. 該当しなければ、取得時点のスナップショットで文を実行する。

- 行・台帳（RECOVER-12）・カタログ・世代に一切書く前に判定するため、副作用はゼロ。
  以降の文は `25P02` を返し、受け付けるのは `ROLLBACK` だけ（ERR-7。クライアントは
  `ROLLBACK` の後に再試行する）。
- permit を保持して判定するのは、取得時点までのすべての commit が記録済みである
  ことを保証するため（1a-5 の順序不変条件）。
- writer 取得前に読み取りが無い（読み取り集合が空）場合は判定せず、`40001` にならない。
- `40001` のエラーメッセージにテナント ID・表名・世代を含めない（既存の stderr 方針と同じ）。

#### 1a-4. 読み取り集合（#2404）

- 持ち方: `ActiveTxn` が保持する「読み取った基底表の名前の集合」。
- 記録する期間: writer を取得する前の読み取りだけ。取得後は記録しない。
- 含める読み取り: 結合・部分クエリ・集合演算の各表、ビューを展開した先の基底表、
  `USING PLAN` の辞書構築と検索本体、`EXPLAIN`、`COPY (...) TO STDOUT`、
  カーソル（`DECLARE`）の内側の SELECT。
- 除外する読み取り: 一時表（TABLE-44。セッションローカルで他セッションの commit の
  影響を受けない）。
- fail-closed: 文の種別ごとに参照先を確定できない場合（カタログの参照など）は
  「全表を読んだ」というワイルドカードとして扱う。読み取り側を過大に見積もっても、
  1a-6 の可視性フィルタは書き込み側に掛かるため、他テナントの存在情報は漏れず、
  偽陽性が増えるだけで済む。

#### 1a-5. commit 記録（書き込み側の足跡。#2404）

**置き場所**: `Storage` が持つインメモリの有界な記録とする（redb には永続化しない）。
明示トランザクションはプロセスの再起動をまたがないため永続化は要らず、redb に置くと
刈り込みのたびに durable な書き込みが発生してファイルが育つ。

**キー**: 既存のストレージ全体の書き込み世代（`GENERATION_TABLE`）。
`commit_boundary::commit` を通るすべての commit は、`prepare_generation_bump` で世代を
必ず 1 つ進めてから commit する（既存の契約）。commit 記録にはその commit が書き込んだ
世代の値 `G_c` を付ける。読み取りスナップショットの世代 `G_s` は、`BEGIN` で開いた
`ReadTransaction` 自身から `current_generation_in_txn` で読む（対応が正確になる）。

**順序不変条件（偽陰性を防ぐ要）**:

- 記録の追加は、redb の commit が成功した後、`WriterPermit` を解放する前に行う。
- writer を取得したセッションから見ると、取得時点の世代 `G_now` までのすべての
  commit が記録済みになる。判定では `G_s < G_c ≤ G_now` の範囲の記録をすべて照合する。
- この範囲に記録が欠けた世代があれば、fail-closed で衝突として扱う（構造上の多層防御）。
  欠けが通常の経路で起きないことは構造テストで固定する。

**内容**: commit ごとに、書き込んだ表それぞれについて次の 2 つを持つ。

- 行の足跡: その commit が書き換えた行の、書き込み前の行と書き込み後の行の
  (`tenant_id`, `visibility`) の組の集合。INSERT は後の行だけ、DELETE は前の行だけ、
  UPDATE と UPSERT は前後の両方。
- DDL フラグ: その表のスキーマやカタログを変えたかどうか。

**収集する箇所**:

- 既存の契約「行を書き換えるすべての経路は、commit 前に `bump_table_generation_in_txn`
  を呼ぶ」（`tests/table_generation_bump_coverage.rs` が構造的に固定）を起点にし、
  同じ箇所で足跡を積む。
- 対象は autocommit・暗黙のトランザクション・明示トランザクションの COMMIT・DDL・
  参照アクションの連鎖で書き換わった子表・ファイル形 INSERT・NoSQL 表層の書き込み。
- 行の (`tenant_id`, `visibility`) が分からない経路は、「その表の全組」として記録しない。
  全組の記録は、他テナントの Private 行だけを変えた commit でもその表を読んだセッションに
  `40001` を返すため、1a-6 の可視性契約を破り、他テナントの書き込みを観測できる側チャネルに
  なる。足跡を安全に記録できない書き込みは、redb の commit より前に fail-closed で拒否する
  （1a-7 の上限到達時と同じ拒否。`54000`）。通常の経路が拒否に落ちないことを構造テストで固定する。
  DDL は足跡ではなく DDL フラグで表すため、この拒否の対象外とする。

#### 1a-6. 判定規則（テナントと表の組の粒度。SQL-31・SQL-38 (2)）

読み取り集合の表 T について、`G_s` より後の記録が次のいずれかに当たれば `40001`。

- DDL フラグが立っている。
- 行の足跡に、判定するセッションから可視な組が含まれる。可視かどうかは、読み取り時の
  RLS 暗黙適用（RLS-7 の `PolicyContext`）と同じ判定関数で評価する（判定ロジックを
  二重に実装しない）。

**他テナントの Private 行を除外する根拠（側チャネルを作らない）**:

- 他テナントが Private 行だけを書き換えた commit の足跡には、判定するセッションから
  可視な組が含まれない。その commit は `40001` の原因にならない。
- このセッションの読み取りはもともと RLS の暗黙適用で可視の行に限られるため、不可視の
  行への書き込みを除外しても取りこぼし（偽陰性）は生じない。
- その結果、`40001` が起きるかどうかと起きる時点から、他テナントの不可視の行への書き込みの
  有無も件数も推測できない。
- 既存の表単位・全テナント共通の `table_generation` を `40001` の判定に流用しない。
  流用すると他テナントの不可視の行への書き込みでも世代が進み、側チャネルになる
  （`table-generation-rejection-granularity.md` の判断は `USING PLAN` の照合に限った話で、
  本判定には適用しない）。

**粒度**: 行や述語ではなく表の粒度で判定する。偽陽性の `40001` はあり得るが、偽陰性は許さない。

#### 1a-7. 保持範囲と上限

**保持範囲**:

- 「生存中の明示トランザクションのうち、writer 未取得のもの」の `G_s` の最小値より古い
  記録は捨てる。読み取り集合が空かどうかは問わず、`BEGIN` の時点から保持対象にする。
  `BEGIN` 後・最初の SELECT 前に起きた commit も、その後の SELECT が `BEGIN` の
  スナップショットを読むため、衝突判定に必要になる。読み取り集合が空の間に限って記録を
  捨てると、後から読み取りが入ったとき記録欠落（fail-closed の衝突扱い）になり、無関係な表や
  他テナントの Private 行だけの更新でも `40001` になって 1a-6 の可視性契約を保てない。
- 生存中トランザクションの `G_s` は、`Storage` 上の登録簿に RAII のハンドルで登録する。
  登録の順序: 登録簿のロックを取り、そのロックを保持したまま `ReadTransaction` を開いて
  `G_s` を読み、登録してからロックを放す。刈り込みも同じロックの下で行う。これにより、
  スナップショット取得と登録の間に `G_s` より新しい記録が刈り込まれることはない。
  `COMMIT`・`ROLLBACK`・`fail`・期限切れ・writer の取得・drop のどの経路でも登録を外す。
- 生存中のものが無ければ、記録は保持しない。
- `max_duration`（既定 20 秒、CLI のハード上限 60 秒）で生存期間に上限があるため、
  通常の保持範囲は直近の数十秒分に限られる。

**上限（無制限に増えない）**:

- 記録の総量に固定の上限を設ける。総量は「表ごとの記録の総数」と、各記録が持つ行の足跡
  （(`tenant_id`, `visibility`) の組の集合）の要素数の合計の両方で数える。件数だけでは
  足跡が大きい記録でメモリ使用量を制限できないため。具体値は実装既定値として
  #2404 で決める（決定 5 と同じく spec 由来の数値ではない）。
- 単一 commit の収集途中に足跡の要素数が 1 commit あたりの上限へ達したときは、「その表の
  全組」に畳まない。全組への畳み込みは、他テナントの Private 行だけを書き換えた commit でも
  その表を読んだセッションに `40001` を返し、1a-6 の可視性契約とテナント境界に反するため。
  代わりに可視性境界ごとに要約する:
  - 判定するどのセッションからも可視な種別の行（全テナント共通で見える行）は、テナントを
    区別せず「全セッションから可視な組」の 1 要素へ畳む。どのセッションにも可視なので、
    不可視の組を可視扱いすることにならない。
  - テナント固有に可視な行（Private）は (`tenant_id`, `visibility`) の組を畳まない。それでも
    上限へ達する commit は、redb の commit より前に abort して fail-closed で拒否する（`54000`）。
    不可視の組を可視扱いして記録するより、書き込みを断る方を選ぶ。
  - 偽陰性は作らない。拒否は commit 前なので、記録できない書き込みが確定することはない。
  上限の大きさと、通常の負荷で拒否に落ちないことは構造テストで固定する（「通常負荷で
  起きない」ことを安全性の根拠にはしない。根拠は上記の拒否と要約の可視性境界である）。
- 上限を超えたら最古の記録から捨て、捨てた最大の世代を「床」として保持する。
- `G_s` が床より古いトランザクションは、writer の取得時に fail-closed で `40001` とする
  （偽陽性は許容）。
- 上限に達したことで出る `40001` から観測できるのは全体の commit 量だけ。これは writer の
  保持と待機が既に観測されうるのと同程度の情報で、行や件数の情報は含まない。この残余
  リスクを許容し、上限は通常の負荷で到達しない大きさに取る。
- 上限と刈り込みを検証するテストは #2404 の担当。

#### 1a-8. カーソルとの関係

- writer の取得前に宣言したカーソルは `BEGIN` 時点のスナップショット、取得後に宣言した
  カーソルは取得時点のスナップショットを読む（WIRE-15）。
- 取得前に宣言したカーソルの内側の SELECT は読み取り集合に入る。
- 取得前のカーソルが残っている間は、`BEGIN` 時点の `ReadTransaction` を保持する。
- 実装は #2403、検証は #2411（engine・wire）。

#### 1a-9. 本節で決めないこと（申し送り）

- `55P03` が出る箇所・上限の起点の検証は #2409 で実施済み。状態バイトの検証は #2410、3 クライアントの層 B は #2412 で実施済み（`docs/design/three-client-e2e-harness.md` の #2412 の節）
- READ ONLY の読み取りトランザクション・上限の適用 → #1933 で実装済み（「READ ONLY の読み取りトランザクション」節）。並行のスナップショット一貫性・テナント境界の検証 → #1935 で検証済み（`tests/sql_t01_39/sql38_read_only_concurrency.rs`・`core/deferred_writer_tests.rs`）、書き込み拒否 → #1934、行ロック句・`LOCK TABLE` → 「行ロック句・`LOCK TABLE` の扱い」節（#2230・#2231・#1957・#1958）
- CHANGELOG と `implementation-status.md` は #2413 で記載済み
- 早期の事前判定（gate を待つ前に明らかな衝突を返す最適化）は任意とし、#2405 の判断に委ねる。

#### 1a-10. 実装上の決定（#2404）

読み取り集合（1a-4）と commit 記録（1a-5・1a-7）の実装で固定した決定。判定（`40001`）は #2405。

**読み取り集合**（`sql/read_set.rs`）:

- AST から参照表を集めない。実行時に必ず通る 2 点で記録する: `catalog::user_rows_table_name`
  （行テーブル名の生成点）と、関係の解決点 `catalog::get_table_schema_in_txn_impl`（行を読まない
  `EXPLAIN` とビュー名を拾う）。マテビューの実体名の生成点も同様。カタログ全体の列挙
  （`list_tables_in_txn`）はワイルドカード。どれも読み取りを過大に見積もる側に倒れる。
- 記録は thread-local の RAII 区間。`core.rs` の `execute_in_active_txn` と `begin_copy_active`
  が、明示トランザクションかつ writer 未取得のときだけ張る。writer を取得した時点
  （`SessionTransaction::ensure_writer`）で区間の内容を `ActiveTxn::read_set` へ移して以降を止める。
  同じ文の取得後の読み取り（`INSERT ... SELECT` の元表）は入らない。
- 一時表の文は `core/temp_route.rs` の振り分けで記録を止める。表数の上限 `MAX_READ_SET_TABLES`
  （1024）を超えたらワイルドカードへ畳む。
- `information_schema`・`pg_catalog` は関係として未対応（`0A000`）のため、それらを読む経路は存在せず、
  記録の対象外（対応する場合は列挙点でワイルドカードにすること）。
- 既知の限界: 読み取りが別スレッドで表を開く経路は記録されない（表の解決は文の実行スレッド上で
  行われる前提。並列化されるのは行の採点だけ）。

**commit 記録**（`storage/commit_log.rs`）:

- `Storage` が `Option<Arc<CommitLog>>` を持つ（一時表用ストレージは `None`）。`Storage::open` の
  最後（移行 commit の後）に arm し、それ以前の commit は記録対象外。
- 登録は `Storage::open_registered_snapshot`（ログのロックを保持して `ReadTransaction` を開き `G_s` を
  読んで登録）。`ActiveTxn::registration` が持ち、writer を `active.writer` へ格納した後に外す
  （#2405 の判定の差し込み位置。判定中に必要な記録を刈り込ませない）。
- 追記は `commit_and_finish` の準備段階（世代の採番と検査。durable な書き込みの前）と、commit 成功後・
  permit 解放前（post-commit 区間の先頭）の 2 段。追記は panic・失敗させない（mutex の汚染は回収して
  `tainted` を立て、照会側が判定不能として扱う）。
- 書き込まれた表は表世代の差分（`catalog::changed_table_generations_in_write_txn`）で確定する。
  開始時点の確定状態は、記録を始めた永続ストレージで `begin_write` の直後に開いた `ReadTransaction` を
  使う。差分を取るのは commit の準備段階で登録が存在するときだけで、登録が無ければ差分も取らず記録も
  保持しない。準備段階の後に登録が現れた場合は、その commit の記録が無いので床をその世代まで上げる
  （fail-closed。`BEGIN` が進行中の autocommit と重なると、その `BEGIN` は最初の書き込みで `40001`
  になりうる。偽陽性側で、窓は準備段階から追記までに限られる）。
- 行の足跡は書き込み点が `note_row`・`note_row_bytes`（行のヘッダから `(tenant_id, visibility)` を読む）・
  `note_table`（0 行でも世代を進める経路）・`note_ddl` で、`GatedWriteTxn`（`Box` でアドレス固定）に結び付く
  frame へ積む。frame は process 共有の登録簿にアドレスをキーとして置く（逆参照しない）。
  書き込まれた表のうち足跡も DDL も積まれていないものがあれば、redb の commit より前に拒否する
  （`StorageError::CommitFootprintUnrecordable`。`54000` の固定文言 `commit footprint limit exceeded`。
  テナント ID・表名・件数は含めない）。書き込み点の結線漏れは取りこぼしではなく拒否として表れる。
  クレート内テストでは常に検査する（`cfg(test)` で記録要否を常に真にしている）ため、全既存テストが
  結線漏れの検出器になる。
- `Public` 行は表ごとに 1 要素へ畳む。`PolicyContext::is_visible` は `Public` 行のテナントを見ないため
  判定は変わらない（`commit_log::tests::public_collapse_matches_policy_is_visible` が `is_visible` に対して
  固定する）。`Private` は `(tenant_id, Private)` のまま畳まない。更新で可視性が変わらない経路は更新後の
  行 1 つで前後を表し、削除・置換・TRUNCATE は消える前の行のヘッダから積む。
- DDL（スキーマ・カタログ・マテビュー実体・トリガ・移行）は `note_ddl`。マテビューの更新を DDL として
  扱うのは偽陽性側の近似（読んだセッションに `40001` が出る側）で、行の足跡を持たない実体の更新を
  表単位で表す。

| 定数（実装既定値。spec 由来の数値ではない） | 値 | 超えたときの扱い |
| --- | --- | --- |
| `MAX_COMMIT_RECORDS` | 16384 | 最古から追い出し、床を上げる |
| `MAX_FOOTPRINT_ELEMENTS`（全記録の合計） | 262144 | 同上 |
| `MAX_FOOTPRINT_ELEMENTS_PER_COMMIT` | 1024 | commit 前に `54000` で拒否 |
| `MAX_READ_SET_TABLES` | 1024 | ワイルドカードへ畳む |

1 commit の上限は全体の上限より小さい。RLS 下の書き込みは所有者の行に限られるため、通常の 1 commit が持つのは
表ごとに「自テナントの Private」と「Public」の 2 要素までで、1 commit の上限は複数テナントにまたがる管理系
経路のためのもの。

照会 API は `CommitLog::records_between(g_s, g_now)`（床より古い・世代の欠け・汚染は `Unverifiable`）、
`SessionTransaction::read_set()`・`snapshot_generation()`。`ensure_writer_with` が #2405 で結線した（1a-11）。

**足跡の取り方（経路ごと）**:

| 経路 | 足跡 |
| --- | --- |
| 挿入（単一・複数・型付き・トリガ・UPSERT の挿入側） | `insert_unique_row` が書いた行のヘッダから |
| 更新（単一行・述語形・結合形・トリガ・UPSERT の更新側・FK の連鎖） | 更新後の行（可視性は不変）。UPSERT の更新側は更新前の行も |
| 削除（単一行・述語形・置換・FK の連鎖・トリガ） | 消える前の行のヘッダ |
| TRUNCATE | `retain_in` で消える各行のヘッダ（0 行でも `note_table`） |
| DDL・マテビュー・トリガ定義・採番・CTAS・移行 | `note_ddl` |
| テスト専用の生の挿入 | `note_row` |

#### 1a-11. 実装上の決定（#2405）

writer 取得時の `40001` 判定（1a-3・1a-6・1a-7）の実装で固定した決定。

- **挿入位置と順序**: `SessionTransaction::ensure_writer_with` の中で、gate 取得 → 読み取り集合の確定 →
  判定 → 衝突なしなら登録を外して終了、の順。判定は行・台帳・カタログ・世代へ何も書く前に、permit を
  保持したまま行う。衝突なら `ActiveTxn` を drop（write txn の abort → permit 解放）して `Failed` へ遷移し、
  副作用は残らない。
- **`PolicyContext` の受け渡し**: `ensure_writer`／`ensure_writer_with` は書き込み文を実行する接続の
  `PolicyContext` を必須引数で受ける（判定を飛ばす経路を作らない）。判定用の文脈を新たに作らない。
- **判定の入力**: 読み取り集合が空なら判定しない。`g_now` は取得時点の確定世代（permit 保持中は他の commit が
  起きないため安定）。`g_now` が `G_s` 以下なら衝突なし。
- **判定不能は衝突**: commit 記録を持たないストレージ・記録の床より古い・世代の欠け・mutex 汚染は
  `40001` に倒す（fail-closed。新しい `wire_code` は作らない）。世代の読み取り失敗は `XX000`。
- **述語**: `storage::commit_log::conflicts_with`。読み取り集合に含まれる表（ワイルドカードは全表）への
  記録のうち、DDL、または `PolicyContext::is_visible` で可視な足跡（自テナントの Private・Public）があれば衝突。
  可視性は必ず `is_visible` を通し、判定ロジックを二重に持たない。他テナントの Private だけの commit は
  原因にならない。
- **取得前の存在確認**: `LOCK TABLE` の取得前の存在確認は読み取り集合へ入らない（読まずに書く形のため。
  テストで固定）。
- **採らないこと**: 早期の事前判定（gate を待つ前の衝突返却）は行わず、判定経路を 1 本に保つ。
- テスト: `crates/engine/src/core/serialization_failure_tests.rs`・
  `crates/engine/src/storage/commit_log.rs` の述語テスト・
  `crates/engine/tests/sql_t01_39/sql31_serialization_failure.rs`（SQL-31・SQL-38）。

### 2. writer gate（待機上限付き。ロック待ちで `55P03`）

`crates/engine/src/storage/writer_gate.rs`。`Mutex<GateState>` と `Condvar` で
構成し、`GateState` は `{ held_by: Option<ThreadId> }`。

- **改訂（#2396・1a）**: `begin_explicit_write_txn` の呼び出しが `BEGIN` から最初の書き込み文へ
  移り、`55P03` が出る箇所も最初の書き込み文へ移る。待機上限と gate の構造は変えない。
- **autocommit 経路**（`Storage::begin_write_txn`）と**明示トランザクション
  経路**（`Storage::begin_explicit_write_txn`）は同じ保持方針をとる。どちらも
  ゲートを待機上限つきで取得し、`redb` の書き込みトランザクションが commit／abort
  されるまで `WriterPermit`（RAII）を保持する。`storage::GatedWriteTxn` が
  `redb::WriteTransaction` と permit を同じ寿命で束ねる。超過時は
  `StorageError::WriteLockTimeout`（`55P03`）。
- 当初の実装では、autocommit が `redb` のライタを得た直後にゲートを手放して
  いた。このため autocommit の書き込みがライタを持っている間に別セッションが
  ゲートを取得すると、`redb::Database::begin_write` の中で待機上限なしに
  止まっていた（PR #1041 レビュー指摘）。現在は「ゲートを保持していること」と
  「`redb` のライタを保持していること」が常に一致するため、ライタ待ちはすべて
  ゲートの待機上限で打ち切られる。
- **デッドロックが起きない理由**: ゲートは 1 段しかなく、`redb` のライタへの
  経路はゲート経由だけ。待機はすべてゲートの待機上限で打ち切られる。
- **自己デッドロックの防御**: 書き込みトランザクションを保持しているスレッド自身が
  再度ゲートへ到達した場合は待たずに即座に `WriteTxnHeldByCurrentSession`
  エラーを返す（`55P03` へ写像。書き込み経路の配線漏れに対する多層防御）。

### 3. `tenant.rs` の書き込み本体を `WriteTarget` で共有する

`pub(crate) enum WriteTarget<'a> { Autocommit(&'a Storage), InTxn(&'a redb::WriteTransaction) }`
と `WriteTarget::with_txn`（`f: impl FnOnce(&WriteTransaction) -> Result<(T, TxnEffect), E>`
を受け取り、`Autocommit` は `begin_write_txn` → `f` → `commit`（`f` が
`TxnEffect::NoOp` を返したときは commit せず abort）、`InTxn` は `f` の結果を
そのまま返す。`E` は `TenantWriteError` から変換できる型なら何でもよく、述語形
DML は `PredicateDmlError<E>` を使う）を導入した。Issue #942 では次の 4 関数のみを
対象にし、Issue #1179 で行を書き換える全書き込み関数へ拡大した:

- `insert_row_unchecked`（Rust API の行形 INSERT）
- `insert_rows_unchecked`（同・バッチ）
- `insert_typed_row_unchecked`（SQL 表層の単一行 typed INSERT）
- `truncate_tables_unchecked`（`TRUNCATE TABLE`）
- （Issue #1179）`insert_typed_rows_unchecked`（複数行 `VALUES`・COPY）・
  `upsert_typed_rows_unchecked`（`ON CONFLICT`）・`update_row_unchecked`・
  `update_row_columns_unchecked`（単一行 `UPDATE`）・`delete_row_impl`
  （単一行 `DELETE`。`RETURNING` を含む）・`delete_rows_where_unchecked`・
  `update_rows_where_unchecked`（述語形 `DELETE`／`UPDATE`）
- （Issue #1353）`replace_typed_rows_by_text_key`（ファイル形 `INSERT` のチャンク置換）。
  `incremental::index_file`・`sql::exec::execute_file_insert` が `WriteTarget` を受け取り、
  埋め込み I/O は BEGIN 時点で既に保持している単一ライタの下で `max_duration` の範囲内に行う
  （文の前にしか期限を見ないため、埋め込み中に期限を超えても次の要求の期限解放と COMMIT 時の
  期限検査が abort させる。正しさは崩れない）。`INITIALLY DEFERRED` の FK は他の書き込み経路と同じく
  COMMIT 時の検査へ回す。NoSQL バッチ（`index_file_batch`）は従来どおり `Autocommit` のみ

`sql::exec` は各実行関数の `_in` 版（`WriteTarget` を受け取る本体）を持ち、既存の
関数は `Autocommit` を渡す薄いラッパーとして挙動を変えない。`core.rs` の
`execute_in_active_txn` が `Insert`（単一行・複数行・UPSERT）・`Delete`・`Update` を
受理し、成功後に `mark_written` する。`RETURNING` は UPSERT を除く全 DML
（`INSERT`・`UPDATE`・`DELETE`。Issue #1272 で `UPDATE`・述語形 `DELETE` を追加）で
受け付け、UPSERT だけ下記の `0A000` で拒否する。
`parsed_operation_id` は `Delete`・`Update` の `operation_id` も返し、同一
トランザクション内での再利用（`25000`）は全 DML に効く。

**明示トランザクション内で拒否（`0A000`）を維持するもの**: DDL。ファイル形 `INSERT`
と、`USING PLAN` を伴う検索 SELECT・`EXPLAIN` は Issue #1353 で受理に変わった
（「6. トランザクション内の読み取り」参照）。

**明示トランザクション内の `RETURNING` で拒否（`0A000`）するもの（Issue #1182・#1272・#1273 の
経緯・SQL-31・SQL-21）**: なし（全 DML の `RETURNING` を受理する）。UPSERT
（`INSERT ... ON CONFLICT ... RETURNING`）は Issue #1273 で受理に変わり、
`execute_insert_returning_form` の `Upsert` 腕が autocommit と同じ
`execute_upsert_returning_in` へ `WriteTarget::InTxn` を渡す（衝突判定・不可視な衝突先の
扱い・台帳・制約検査は `tenant::upsert_typed_rows_impl` が共有）。
単一行・述語形の `UPDATE ... RETURNING` と述語形の `DELETE ... RETURNING` は Issue #1272
で受理に変わり、`execute_in_active_txn` が autocommit と同じ
`execute_update_returning_form`・`execute_predicate_delete_returning_form` へ
`WriteTarget::InTxn` を渡す（返却行数＝影響行数・可視集合での対象選定・RLS は共通）。
`RETURNING` を黙って落とす fail-open にはしない。fail-closed の契約: autocommit では
投影の失敗（`54000` 等）がその文自身の write トランザクションを abort するが、`InTxn` では
文ごとの abort は無く、`execute_parsed_in_txn` がエラーを受けて `txn.fail()` を呼び共有の
`write_txn` を drop（abort）する。明示トランザクションは `Failed` へ遷移して部分書き込みは
永続化されず（COMMIT は `25P02`、受け付けるのは ROLLBACK だけ）、暗黙トランザクション
（WIRE-16・Issue #1175。文の実行ディスパッチは起源を区別しない）は abort して `Idle` に
戻る。

**commit 後の副作用の監査（Issue #1179）**: 上記の書き込み関数と呼び出し元
（`sql::exec`・`core.rs`）に、`commit_boundary::commit` の後で共有インメモリ状態
（キャッシュの差分更新・索引・統計）を変更するコードは無い（キャッシュはテーブル世代
キーで失効する方式）。したがって `InTxn` でも本当の COMMIT より前に走ってしまう
副作用は無い。

**空バッチ**: `insert_rows_unchecked` の空バッチ（`rows.is_empty()`）は、main と
同じく commit せずに `write_txn` を abort する（`TxnEffect::NoOp`）。当初の実装は
空の `write_txn` を `commit_boundary::commit` で commit していたため、グローバル
世代が進み、世代で失効するキャッシュを無駄に捨てていた（PR #1041 レビュー指摘）。
他の関数は基本的に常に書き込む（`truncate_tables_unchecked` は 0 行でも台帳記録と
テーブル世代の進行を行う既存契約）ため `TxnEffect::Wrote` を返す（述語形 DML の
件数上限超過は `TxnEffect::NoOp` で、呼び出し元が `54000` を返し `Failed` へ遷移する）。

### 4. 状態機械（`crates/engine/src/sql/transaction.rs`）

`SessionTransaction<'e>`（`enum TxnState<'e> { Idle, Active(Box<ActiveTxn<'e>>),
Failed { session_at_begin: SessionState, expired: bool } }`）。`ActiveTxn` は
`writer: Option<GatedWriteTxn>`（permit を内包）・`read_snapshot: ReadTransaction`・`started_at`・
`statements`・`seen_operation_ids`・`written_tables`・`written_by_tenant`・`has_writes`・
`session_at_begin` と、dirty テーブル判定（確定済みの世代の読み取り）に使う
`&'e Storage` を保持する。

- **改訂（#2396・1a。#2399 で状態の形を実施済み）**: `ActiveTxn` は `writer: Option<GatedWriteTxn>`
  （未取得／取得済み）と `BEGIN` 時点の確定済み `read_snapshot: ReadTransaction` を持つ。
  - #2399 は挙動不変のリファクタ（SQL-31・PERSIST-4・TASK-333）。#2400 で明示トランザクションの
    writer を `BEGIN` ではなく**最初の書き込み文**で取得する形へ変えた（BREAKING。実施済み）。
    `BEGIN` は読み取りスナップショットだけを開き、writer を待たない（`55P03` も出さない）。
  - 取得の入口は `SessionTransaction::ensure_writer`（冪等）。`core.rs` の書き込み腕
    （INSERT・ビュー DML・DELETE・UPDATE・TRUNCATE・データ変更 CTE・INSERT…SELECT・`setval` 系）が
    `write_txn()` を取る直前に呼ぶ。`COPY FROM` は確定処理（CopyDone 後）で取得するため、
    データ受信中は持たない。`lastval` も同じ腕のため writer を取る（粒度は粗いが安全側）。
    DDL（`0A000`）・一時表・読み取り・カーソルは取得しない。ロック句・`LOCK TABLE` は今も
    `42601` で拒否されるので届かない（#2230・#1958 は同じ入口を呼ぶ）。
  - `55P03`（待機上限超過）の発生点は `BEGIN` から最初の書き込み文へ移る。受けた Tx は `Failed`
    になり、以降の文は `25P02`、受理するのは `ROLLBACK` だけ。取得前は何も書いていない。
  - `ensure_writer` は 1a-3 の「gate 取得 → permit 保持のまま判定 → 格納」の挿入点で、`40001` 判定は
    #2405 で入った（1a-11）。
  - 暗黙トランザクション（WIRE-16）は従来どおり開始時に writer を取る（スナップショットは取得後に開く。#2401）。
  - 取得の前後の回帰（read-your-writes・COMMIT までの隔離・ROLLBACK 後のキャッシュ非汚染・ファイル形
    `INSERT`・`USING PLAN`・`EXPLAIN`・`COPY FROM`・`setval`）は #2402 のテストで固定済み（SQL-19・SQL-31）。
  - **読み取りの意味（#2403 で結線済み）**: 読み取り源は `SessionTransaction::read_source` の三分岐で選ぶ。
    writer 取得前は `read_snapshot`（`BEGIN` 時点）、取得後で dirty ありは共有 write txn（取得時点の
    最新＋自分の変更）、取得後で変更なしは文ごとの確定状態（permit 保持中は取得時点と一致）。
    取得後の述語形 `UPDATE`／`DELETE` の述語評価は、取得時点の write txn で行うため、`BEGIN` 後・取得前に
    他セッションが commit した行も対象に入る。`dirty_tables` の比較基準は最新の確定状態のままにする
    （`read_snapshot` 基準にすると取得前に他セッションが書いた表まで dirty になり、キャッシュ回避と
    遅延 FK 検査の範囲が不要に広がるため）。
  - **既知の残り**: 取得前の `USING PLAN` の SELECT・`EXPLAIN` は自前で確定スナップショットを開く
    既存経路のまま文ごとの確定状態を見る（`BEGIN` 時点への一般化は辞書スナップショットの構築変更を伴い
    #2402 と競合するため見送り）。読みが `BEGIN` 時点より新しい側へずれるだけで、`40001` 判定には
    偽陽性側にしか倒れず偽陰性は生まない。
  - **検証のカタログ引き先（#2403 レビュー対応）**: 明示 Tx 内の `DECLARE` と `COPY TO` の検証は、実行と同じ
    読み取り源（`BEGIN` 時点のスナップショット／共有 write txn／確定状態）でカタログを引く。
    `COPY FROM` は書き込みのため最新のカタログで検証する。文の字句・構文解析の入口（`parse_tokens_gated`
    系。Parse メッセージ等 Tx 状態を持たない入口と共有）は今も最新の確定カタログで解決し、実行段が
    スナップショットのスキーマで再束縛する。食い違いは `42P01` 等の fail-closed になる（誤読はしない）。
  - DoS 面: `BEGIN` だけで黙る接続が単一ライタを占有できなくなる。一方、読み取りだけの Tx は
    `ReadTransaction` を最大 `max_duration` 保持し、その間は redb がページを回収できない（有界。
    期限切れ処理がスナップショットを解放する）。
  - writer が `None` で `has_writes` が真の状態は不変条件違反で、`COMMIT` は `XX000`
    （`session` を開始時点へ復元）、`dirty_tables` は `Err` で fail-closed に倒す。
  - writer を取得しないまま `COMMIT` した場合は読み取りトランザクションを drop するだけで、
    commit も世代の更新もしない（一時側の確定・`ON COMMIT` は従来どおり実行する）。
  以下の記述は改訂前（`BEGIN` で取得）のもの。
- **`BEGIN`**: `Idle` → `Active`（`Storage::begin_explicit_write_txn` 経由。改訂後は
  `BEGIN` で writer を取得しない。取得は最初の書き込み文）。
  `Active` 中の再 `BEGIN` は `25001`（`ActiveSqlTransaction`）で `Failed` へ
  遷移。`Failed` 中は `25P02`。
- **`COMMIT`**: `Idle` は `25P01`（`NoActiveSqlTransaction`）。`Active` は
  `has_writes` なら `commit_boundary::commit`、無ければ `drop`（abort）して
  `Idle` へ。`Failed` は `25P02` のまま（`ROLLBACK` のみ受理し続ける）。
  持続時間の上限を過ぎた `Active` の `COMMIT` は確定させず、abort して
  `Failed` へ遷移し `54000` を返す（文実行時の上限超過と同じ契約）。commit
  自体が失敗した場合は、PostgreSQL と同じくロールバック扱いとし、`BEGIN`
  時点の `SessionState` を復元してから `Idle` へ戻る（PR #1041 レビュー指摘）。
- **`ROLLBACK`**: `Active`／`Failed` いずれからも `Idle` へ戻り、`BEGIN` 時点の
  `SessionState`（`SET search_mode`・`CREATE FUNCTION` 等）を復元する
  （PostgreSQL の挙動に準拠する実装判断。spec は沈黙）。`Idle` からの
  `ROLLBACK` は `25P01`。
- **通常の文**: `Idle` は既存 autocommit（`execute_parsed_in_session`）を
  そのまま通す（挙動は不変）。`Failed` は実行せず `25P02`。`Active` は
  上限検査 → `operation_id` 再利用検査 → 種別判定 → 実行、の順。
- **`Active` 中のどの文でエラーが起きても**、その場で `write_txn` を abort
  し permit を解放して `Failed`（ロックは保持しない）へ遷移し、元のエラーを
  そのまま返す。Failed 状態から `COMMIT` はできないため、失敗した文が残した
  部分書き込みが永続化されることはない。**redb の savepoint は不要**。
  構文・許可リスト検証のエラー（`execute_sql_in_txn` の `parse_sql` 失敗）、
  簡易クエリの複数文分割・位置検証のエラー、拡張クエリプロトコルの
  Parse／Bind／Describe／Execute のエラー応答も同じく `Failed` へ遷移させる
  （PostgreSQL と同じく、エラーの種類を問わない。PR #1041 レビュー指摘）。
  `SessionTransaction::fail` は `Active` 以外では状態を変えない（冪等）ため、
  wire 層はエラー応答のたびに状態を問わず呼べる。
- **同一トランザクション内での `operation_id` の再利用**: 台帳照合より前に
  `seen_operation_ids` と照合し、一致すれば `25000`
  （`InvalidTransactionState`）で `Failed` へ遷移する。台帳は自トランザクションの
  未 commit エントリを見て `23505` を返してしまうため、それより先に検査する。
  テーブルを問わず同じ ID を拒否する（保守的な側に倒す）。

### 5. 上限（実装既定値。spec 由来の数値ではない）

- `max_duration`（`BEGIN` からの経過時間の上限）: **20 秒**
- `max_statements`（トランザクション内の文数の上限）: **1,000**
- **改訂（#2396・1a）**: `max_duration`・`max_statements` はこれまでどおり `BEGIN` から数える
  （不変）。`lock_wait` が効き始めるのは最初の書き込み文から。下記の「`Active` の間だけ
  読み取りタイムアウトを切り詰める」規則には「`Active` かつ writer 未取得」の場合が新たに
  生じる。**決定（#2409）**: 切り詰めと期限での解放は writer の有無に関係なく `Active` 全体に
  効く（起点は `BEGIN`）。writer 未取得の場合の目的は、`BEGIN` 時点の読み取りスナップショットを
  解放して redb がページを回収できるようにすること。engine 側は #2409 のテスト
  （`tests/write_lock_wait.rs` T5〜T10・`core_misc/transaction_limits.rs`）で固定し、wire 側の
  確認は #2410 に委ねる。
- 超過時は `54000`（`PayloadTooLarge`）で `Failed` へ遷移する。
- 起動時 CLI（`--max-transaction-duration-ms`・`--max-transaction-statements`。Issue #1553）で
  設定できる。範囲・ハード上限は [transaction-limits-cli.md](transaction-limits-cli.md) 参照。
- `max_duration` は文の実行時・`COMMIT` 時に検査するほか、wire 層
  （`handshake::post_auth_loop`）が要求を受け取るたびに
  `SessionTransaction::release_if_expired` で検査する。期限を過ぎていれば、
  要求の種類（Sync・Flush 等の SQL を伴わない要求を含む）を問わず
  共有書き込みトランザクションを abort してライタを解放し、`Failed` へ遷移する。
  その後の最初の文／`COMMIT` には `54000` を 1 回だけ返し、以降は `25P02`。
- 要求が 1 件も届かない無通信の間も `max_duration` で解放する（PR #1041
  レビュー指摘）。`Active` の間だけ、wire 層（`handshake::read_next_frame_header`）
  が次の要求の型バイトを待つ読み取りタイムアウトを「上限までの残り時間」と
  接続の読み取りタイムアウト（`limits::READ_TIMEOUT`＝30 秒。WIRE-5）の小さい方へ
  切り詰める。期限到達でタイムアウトしたら `release_if_expired` で abort して
  ライタを解放し、応答は送らずに接続を維持したまま受信待ちを続ける（最初の文／
  `COMMIT` に `54000` が返る点は受信時の検査と同じ）。
  - 型バイトの受信待ちで期限に達した場合は接続を維持する（フレームの境界のため）。
    型バイトの受信後、長さ・本文の受信全体にも同じ期限を課し（`framing::
    FrameDeadlineGuard`。読み取り 1 回あたりの待機は 100 ms で区切る）、途中で
    期限に達したら応答を送らずに接続を閉じてライタを解放する（本文を少しずつ
    送り続けて上限後もライタを保持させる経路を塞ぐ。PR #1041 レビュー指摘）。
  - 無通信の上限（WIRE-5）は不変: 期限到達後の再待機は、受信待ちを始めた時点
    からの経過を差し引いた読み取りタイムアウトの残りで待ち、尽きたら従来どおり
    応答なしで切断する。
  - `Failed`・`Idle` はライタを保持しないため切り詰めない。
- `lock_wait`（他セッションが writer gate を待つ上限）: **30 秒**
  （`Storage::DEFAULT_WRITE_LOCK_WAIT`。既存 `READ_TIMEOUT` と同じ値）。
  起動時 CLI `--write-lock-wait-ms`（`100`〜`60000` ms）で変更できる（Issue #1554。
  [write-lock-wait-cli.md](write-lock-wait-cli.md)）。
  超過時は `55P03`。

### 6. トランザクション内の読み取り（未 commit 変更の反映。Issue #1179）

読み取りの入力源を `storage::read_source::ReadSource`（確定済みスナップショットの
`redb::ReadTransaction` と、明示トランザクションが保持する共有
`redb::WriteTransaction` の双方を実装）へ抽象化し、`sql::scan`・`aggregate`・
`group_by`・`window`・`subquery`・`set_op`・`join`・`relation_snapshot`・`exec`・
`arena`・`catalog` の読み取り本体を同じコードで動かす。`EngineCore::read_only_in_active_txn`
が次のとおり経路を選ぶ。

> 改訂（#2396・1a）: 以下の「単一ライタにより BEGIN 時点と厳密に一致する」「BEGIN 以降カタログと
> 世代は誰にも変えられない」という根拠は、writer を保持している間（取得後）にしか成り立たない。
> writer 取得前は「`BEGIN` で開いた確定済み `ReadTransaction` に固定する」、取得後は「取得時点の
> 最新状態＋自トランザクションの未 commit 変更を読む」（#2403 で実装済み。三分岐は 4 節）に差し替わった。`USING PLAN` の
> 世代の事前・事後照合も同じ考え方で書き直す。

- 自トランザクションが未 commit の変更を持たない（`SessionTransaction::dirty_tables`
  が空）間: 従来どおり BEGIN 時点の確定済みスナップショットで読む（キャッシュ利用可。
  単一ライタにより BEGIN 時点と厳密に一致する）。
- 変更を持つ間: 共有書き込みトランザクションを読み取り源にして、autocommit と同じ
  実行本体（`EngineCore::execute_read_statement`。スキーマ取得・サブクエリ解決・
  RLS 適用・実行）を呼ぶ。対象は `Select`（`USING PLAN` なし）・`Aggregate`・`Scan`
  （ウィンドウ関数を含む）・`SetOperation`・`Join`・`DECLARE` の内側 SELECT・
  `COPY (...) TO STDOUT`。サブクエリ・JOIN・集合演算が別テーブルの未 commit 変更を
  読む場合も、文全体が同じ読み取り源を使うため反映される。RLS の判定ロジックには
  手を入れない（読み取り源だけが変わる）。
- **dirty テーブル**は、文が直接書き込んだテーブル（`mark_written`）と、書き込み
  トランザクション内のテーブル世代が確定済みの世代から変化したテーブルの和。
  行を書き換える全経路は commit 前に `bump_table_generation_in_txn` を呼ぶ契約
  （`tests/table_generation_bump_coverage.rs` が構造的に固定）で、参照アクションの
  連鎖で書き込まれた子テーブルも bump するため、`mark_written` の記録漏れに依存しない。
  取得に失敗した場合は fail-closed に dirty として扱う。

**キャッシュの構造的ゲート（P0）**: テーブル世代キーのキャッシュ（`arena_cache`・
`hnsw_cache`・`sparse_cache`・`visible_cache`・`scalar_index`・`relation_snapshot`）は
具体型 `&redb::ReadTransaction` のまま残し、`ReadSource::snapshot()` が `Some`
（確定済みスナップショット）のときだけ使える構造にした。書き込みトランザクションでは
`None` を返し、読み取り本体はキャッシュなしの brute-force 経路へ落ちる（ベクトル検索の
投影遅延も使わない）。理由: ROLLBACK したトランザクションのテーブル世代は次に commit
される別の書き込みで再利用されるため、未 commit の行から作ったキャッシュエントリが
確定済みデータとして別セッション（別テナントを含む）へ返る恐れがある。
`WriteTransaction` を読み取り源にするときは、存在しないテーブルを `open_table` が作成
してしまう副作用を避けるため、`list_tables` で存在を確認してから開く。

**`USING PLAN`・`EXPLAIN` の扱い（Issue #1353）**: dirty のとき、カタログ・スキーマ・
テーブル世代・索引宣言は確定済みスナップショットから読み、辞書構築（LLM 展開の入力）と
`USING PLAN` の検索本体の行の読みだけを共有書き込みトランザクションへ切り替える
（`EngineCore::execute_validated_with_dict`・`dictionary_from_write_txn`）。トランザクション
内は DDL 不可で単一ライタを保持するため、BEGIN 以降カタログと確定済みの世代は誰にも変えられない。
世代の事前・事後照合は同じ確定済み側から読むので一致し、書き込みトランザクション側で読むと
dirty では必ず不一致になるため使わない。辞書は `DictionaryCache` に lookup も insert もしない
（未 commit の行から作った辞書を世代キーのキャッシュに載せると、ROLLBACK 後の世代再利用で
別セッション・別テナントへ漏れる。P0）。行を読まない `EXPLAIN`（`USING PLAN` なしの
Search・Aggregate・Scan）は確定済みのまま出力が決まる。想定外の文種別は `Internal` で拒否する。
書き込み後の読み取りはキャッシュ・HNSW を使わない brute-force になる
（性能上のトレードオフ。テーブル単位のキャッシュ再利用は後続課題）。

## 分離レベルの受理方針（Issue #1937）

明示トランザクションの分離レベル指定の受理方針は spec 側が定義する。
本節はポインタ表記のみとし、受理範囲・表示・修飾子の契約は本書へ記述しない。
ポインタは `docs/spec/04-behavior/sql-surface.md` SQL-38・SQL-31、
`docs/spec/04-behavior/data-model.md` TABLE-3、`docs/spec/04-behavior/persistence.md` PERSIST-4。
構文の実装は #1938（`ISOLATION LEVEL`）・#1939（`SET TRANSACTION`／`SET SESSION CHARACTERISTICS`）・
Issue #1931（`READ ONLY`）、挙動テストは #1940、`40001` は #2396 が担う。
`BEGIN ISOLATION LEVEL ...` の構文解釈は Issue #1938 で実装した（`validate_transaction_control_tokens`）。
受理範囲・エラー契約は上記ポインタ先に従い、本書には記述しない。
`START TRANSACTION`／`END`／`ABORT` の別名は Issue #1930 で受理した（下記「別名」節はポインタのみ）。

- **engine の宣言型**: `crates/engine/src/txn.rs` の `IsolationLevel` は単一バリアントのまま維持する。
  #1938 は要求されたレベルを別の `sql::transaction::RequestedIsolationLevel` として
  `ValidatedTxnControl`（`ParsedSql::Transaction` の payload）から `ActiveTxn` へ記録するだけで、
  実行は変えない（`SessionTransaction::isolation_level()` で読める。暗黙・未指定は `None`）。
  引用識別子の `"ISOLATION"` 等が受理される既存の性質は変えていない。
- **`SET TRANSACTION` 系の実装（Issue #1939）**: モード列の語彙は `BEGIN` と同じ関数
  （`parse_txn_mode_list`）で解釈し、`ParsedSql::SetTransaction` へ運ぶ。契約は SQL-38 (7)(a)・SQL-31 の
  ポインタ先に従い、本書には契約の詳細（受理時期・SQLSTATE・失敗状態への遷移・セッション既定の適用範囲）を書かない
  （下記「公開境界」）。実装の入口は `SessionTransaction::set_transaction_modes` と `SessionState` の既定の保持。
- **fail-closed・テナント境界**: 未対応・不正な形は従来どおり `42601`／`0A000` で拒否する。
  分離レベルの指定は RLS 相当のテナント境界にも可視集合にも影響しない。
- **未割り当ての後続**: `SHOW` による分離レベル等の表示対応を担う sub-issue は #1936 配下にまだ無い。担当は #1936 で決める。
- **公開境界**: 受理範囲などの契約詳細を本書へ記述する場合は、AGENTS.md の公開境界拡張手順（管理者承認）が必要。
  現時点では承認を得ておらず、記述していない。

## 行ロック句・`LOCK TABLE` の扱い（Issue #1956）

行ロック句（`SELECT ... FOR UPDATE／NO KEY UPDATE／SHARE／KEY SHARE [OF ...] [NOWAIT|SKIP LOCKED]`）と
`LOCK TABLE` の受理・結果の契約は spec 側が定義する。本節は実装側の写像と担当の分担だけを記録し、
各形の SQLSTATE・`SKIP LOCKED` の結果・トランザクション外の扱い・併用可否は本書へ記述しない。
ポインタは `docs/spec/04-behavior/sql-surface.md` SQL-38（(3)(4)(5)(7)）・SQL-31、
`docs/spec/04-behavior/data-model.md` TABLE-3、`docs/spec/04-behavior/persistence.md` PERSIST-4、
関連は RLS-12・RLS-7。現状の main では行ロック句・`LOCK TABLE` はいずれも `42601` で拒否される（#1955 の現状欄）。

- **判断（実装側の写像）**: 行ロック句と `LOCK TABLE`（8 モード）は、単一ライタの writer 取得に写す。
  行・表・ロックモードの粒度は区別しない。根拠は 1a-1（書き込み文の一覧にロック句・`LOCK TABLE` が含まれ、
  writer 取得の契機になる）と #1955 本文。行単位の細粒度ロックと複数ライタ（MVCC）は対象外（#1955 本文）。
- **取得経路（実装側の判断）**: 明示トランザクション内では #2396 の遅延取得経路
  （`Storage::begin_explicit_write_txn`、`crates/engine/src/storage/writer_gate.rs` の `acquire`）を使い、
  取得時の `40001` 判定も同じ経路に載せる（1a-3・1a-6）。待機上限は 2 節の書き込みロック待機上限
  （`StorageError::WriteLockTimeout` から `55P03` への写像）に従う。
  `NOWAIT` は「即時取得を試みる経路」で実現する。既存の `acquire` ／ `acquire_yielding` の流用か
  新しい API にするかは #2230 で決める。

| 形 | spec ポインタ | 担当 | 現状 |
| -- | ------------- | ---- | ---- |
| 行ロック句 4 種・`OF`・`NOWAIT`・`SKIP LOCKED` の構文 | SQL-38 (7) | #1957 | 構文解釈済み（`ParsedSql::LockingSelect`）。実行は #2230 まで `0A000` の暫定ゲート |
| 行ロック句によるロック取得・`NOWAIT` | SQL-38 (4) | #2230 | `42601` |
| `SKIP LOCKED` の実行 | SQL-38 (4) | #2231 | `42601` |
| `LOCK [TABLE]` の 8 モードの構文・取得・表名の解決 | SQL-38 (4)(7) | #1958 | 実装済み（#1958） |
| READ ONLY トランザクション内での扱い | SQL-38 (3) | #1934 | 実装済み（#1934。書き込み系の文は `25006`） |
| writer 取得時の直列化判定 | SQL-38 (2)・SQL-31 | #2396 系 | 1a 節の記録のとおり |

- **READ ONLY トランザクション内**: ロック句つき SELECT・`LOCK TABLE` は `25006` で拒否する
  （Issue #1934。「READ ONLY での書き込み拒否」節）。8 モードすべてを拒否する。
- **要確認（担当 Issue が確定する。本節では決めない）**:
  - `SKIP LOCKED` の結果規則（単一ライタの下でどの行を飛ばすか）→ #2231（SQL-38 (4)）
  - トランザクション外の `LOCK TABLE` のエラー契約 → #1958（#1958 本文でも要確認）
  - autocommit の文に付いたロック句が writer を保持する期間 → #2230
- **テナント境界（fail-closed）**: ロック句の結果と `LOCK TABLE` の表名解決を、他テナントの行・表の
  存在や件数のオラクルにしない。1a-6 の「可視性判定関数を共有し、側チャネルを作らない」を再利用する
  （SQL-38 (5)・RLS-12・RLS-7）。未対応の形は従来どおり `42601`／`0A000` で拒否する。
- **公開境界**: 受理範囲・SQLSTATE などの契約詳細を本書へ記述する場合は、AGENTS.md の公開境界拡張手順
  （管理者承認）が必要。現時点では承認を得ておらず、記述していない。

## `#943` との分担

`ReadyForQuery` の状態バイト（`'I'`／`'T'`／`'E'`。WIRE-19）は当初 `#943` が
担当する計画だったが、PR #1041 レビュー指摘（codex P1: 簡易・拡張クエリ両
プロトコルとも `SessionTransaction` 導入後も常に `'I'` を送出しており、
`BEGIN` 後もクライアントからトランザクションが終了したように見える不整合）
への対応として本 PR へ吸収し実装済み（`wire-server::result_encoder::
encode_ready_for_query`／`simple_query.rs`／`extended_query::handle_sync`）。

`#943`（WIRE-19）は上記の中核実装を前提に、残差の検証（単一文の異常遷移・
複数文（WIRE-16）・空クエリ・COPY・拡張クエリの Execute 段エラーの状態
バイト、および無改造の実クライアント 3 種による観測）を層 A
（`crates/wire-server/tests/wire19_ready_for_query_status.rs`・
`wire942_extended_transaction.rs` の失効テストへの状態アサーション追加）・
層 B（`crates/wire-server/tests/three_client_e2e.rs::
three_clients_observe_transaction_status_transitions`）として実施した。
production コード（`crates/wire-server/src/`）は無変更・テスト専任。

## wire-server への結線

- `handshake.rs::post_auth_loop` が接続単位の `Option<SessionTransaction<'e>>`
  を `session` と並べて保持する（`engine.map(|e| e.new_session_transaction())`）。
  未 commit のまま接続が切れた場合、共有 `write_txn` は commit されずに abort
  され、`WriterPermit` も解放される。
- `simple_query.rs::execute_and_respond`／`run_statement` は
  `engine.execute_sql_in_session` の代わりに `engine.execute_sql_in_txn` を
  呼ぶ（`txn` が `Idle` の間はビット同一の挙動）。
- `statement_splitter::check_write_placement(stmts, initially_in_txn)` が
  `TransactionControl` 遷移を模擬し、`BEGIN` を含む複数文メッセージでは書き込み
  文の位置制約を緩和する（詳細は `wire-multi-statement.md` 参照）。
- 明示トランザクション中の `COPY`（Issue #1179）は `crate::copy::run` へ委譲する。
  `COPY FROM STDIN` は `EngineCore::commit_copy_in_txn`（`check_and_register_statement`
  → 共有 `write_txn` への複数行 INSERT → `mark_written`。既存の `commit_copy_in` と
  実行本体を共有）、`COPY (...) TO STDOUT` は `EngineCore::begin_copy_in_txn`（未 commit
  変更を反映）を呼ぶ。COPY 中のあらゆるエラー（CopyFail・デコード失敗・上限超過・
  実行エラー）はトランザクションを `Failed`（ReadyForQuery `'E'`）へ遷移させ、成功時の
  ReadyForQuery は `txn.status()` から導出する。`Failed` 中の `COPY` は autocommit として
  実行せず `25P02` で拒否する（PR #1041 レビュー指摘）。受信期限（DoS 対策）は
  ハンドシェイク層のフレーム受信期限（`framing::FrameDeadlineGuard`。`Active` の間は
  `min(残り持続時間, 読み取りタイムアウト)`）が COPY サブプロトコルの受信にも及び、
  期限超過で接続を閉じて `SessionTransaction` の drop でライタを解放する（CopyData を
  少しずつ送り続けても単一ライタを無期限に保持できない）。

## エラー時の遷移と `Failed` 中の拒否（入口別の網羅表）

`Active` 中のエラーは種類を問わず `Failed` へ遷移させ、`Failed` 中は `ROLLBACK`
以外を `25P02` で拒否する（持続時間の上限で解放した直後の最初の 1 回だけは
`54000`。`SessionTransaction::take_failed_error`）。PR #1041 のレビューを受けて、
入口ごとに次のとおり確認した。

| 入口 | `Active` 中のエラー → `Failed` | `Failed` 中の拒否 |
| ---- | ---- | ---- |
| 簡易クエリ: 複数文の分割・位置検証（`simple_query::respond_splitter_error`） | `txn.fail()` | `25P02` |
| 簡易クエリ: 各文（`EngineCore::execute_sql_in_txn`） | 字句・構文・許可リスト検証のエラーは `fail()`、上限超過・`operation_id` 再利用・実行エラーは `execute_parsed_in_txn` が `fail()` | parse より前に先頭トークンで判定し、`ROLLBACK` 以外は `25P02` |
| 最初の書き込み文での `40001`（writer 取得時の判定。#2396・1a） | `execute_parsed_in_txn` が `fail()`（write txn は書き込み前に abort） | `25P02` |
| 簡易クエリ: `BEGIN` | 入れ子は `25001` で `fail()` | `25P02` |
| 簡易クエリ: `COMMIT` | 期限切れは abort して `Failed`・`54000`。commit 失敗はロールバック扱いで `Idle` | `25P02` |
| 簡易クエリ: `COPY`（`handshake::post_auth_loop`） | 実行エラー・CopyFail は `copy::run` が `fail()`（Issue #1179。正常終了は `'T'` のまま） | `25P02`（`crate::copy::run` へ委譲しない） |
| 簡易クエリ: 空文字列 | 対象外（エラーにならない） | EmptyQueryResponse（副作用なし） |
| 簡易クエリ: 応答のエンコード失敗（`RowDescription`・`DataRow`・`CommandComplete`） | 文の実行後でも `txn.fail()` | — |
| 拡張: Parse | エラー応答は `respond_error_and_await_sync` を通り、`post_auth_loop` が `ignore_till_sync` を見て `fail()` | `ROLLBACK`・空文字列以外は、本体の構造検証（`08P01`）の後、パラメータ型 OID 指定の検査（`0A000`）・parse より前に `25P02` |
| 拡張: Bind | 同上 | `ROLLBACK`・空文字列以外のステートメントは、本体の構造検証の後、format code の検証・ステートメントの存在確認より前に `25P02`（未登録・`Failed` 前に Parse 済みのものを含む） |
| 拡張: Describe | 同上 | 受理（副作用なし。実行は Execute で拒否される） |
| 拡張: Execute | 実行エラーは `execute_parsed_in_txn` が `fail()`。後処理のエラー応答は `ignore_till_sync` 経由で `fail()` | 未実行の `ROLLBACK`・空文字列以外の portal は、本体の構造検証の後、portal の存在確認・実行状態より前に `25P02`（期限切れの未報告分は `54000`）で拒否し、portal を終端状態 `Failed` にする（実行を開始済みの `Suspended`・`Done` も残り行の送出・タグの再送をしない） |
| 拡張: Close・Sync・Flush | 対象外（エラー応答は `ignore_till_sync` 経由で `fail()`） | 受理（副作用なし） |
| フレーミング・プロトコル違反 | 接続を切断し、`SessionTransaction` の drop で abort | 同左 |
| 全メッセージ共通（受信直後） | 期限切れなら `release_if_expired` で `Failed` にしてライタを解放 | — |
| 無通信（受信待ち） | 期限到達で受信待ちを打ち切り、`release_if_expired` で `Failed` にしてライタを解放 | — |

## 検証

- 予告（#2398）: NoSQL 表層から到達できない分類に `40001` を加える。

- `crates/engine/tests/sql31_transaction.rs`: BEGIN/INSERT/INSERT/COMMIT の
  可視性・ROLLBACK の完全巻き戻し・入れ子 BEGIN の `25001`・トランザクション外
  COMMIT/ROLLBACK の `25P01`・同一トランザクション内 `operation_id` 再利用の
  `25000`・対応外文（DDL）の `0A000`・文数上限の `54000`・TRUNCATE と INSERT の原子的
  commit・接続断相当（drop）での完全ロールバックを固定。
- `crates/engine/tests/sql31_txn_dml.rs`（Issue #1179）: 複数行 INSERT・UPSERT・
  UPDATE・DELETE の COMMIT／ROLLBACK／drop、`UPDATE`・述語形 `DELETE` の `RETURNING`
  （Issue #1272。read-your-writes・ROLLBACK で行・索引・台帳に痕跡なし・テナント境界・
  未知列での fail-closed）、全 DML への
  `operation_id` 再利用検査、遅延 FK の COMMIT 時検査（連鎖で書き換わったテーブルの
  下位の遅延 FK を含む）、未 commit 変更の読み取り（Scan・Aggregate・GROUP BY・JOIN・
  自己 JOIN・サブクエリ・集合演算・カーソル）、キャッシュ汚染の回帰（ROLLBACK 後に
  世代が再利用されても未 commit 行が現れない）、RLS を固定。
- `crates/engine/tests/sql_returning.rs`
  （`upsert_returning_inside_explicit_transaction_returns_changed_rows`）: 明示トランザクション内の
  UPSERT の `RETURNING` が受理され返却行数＝影響行数になることを固定（Issue #1273）。
  `sql31_txn_dml.rs` の `upsert_returning_*` は read-your-writes・ROLLBACK で行／索引／台帳に
  痕跡なし・DO NOTHING 衝突行を返さない・テナント境界・不可視な衝突先の `42501` と
  `Failed` 遷移を固定。
  `sql21_returning_row_count_parity.rs` は UPSERT・`UPDATE`・述語形 `DELETE` の `txn: true` セルで
  返却行数・影響行数・物理差分の一致を両可視性モードで固定。暗黙トランザクション側の
  対応外文の例は `DROP TABLE`（`crates/engine/tests/wire16_implicit_txn.rs`・
  `crates/wire-server/tests/wire16_multi_statement.rs`・`wire16_implicit_transaction.rs`）で
  固定し、`UPDATE ... RETURNING`・UPSERT の `RETURNING` の受理は
  `wire16_implicit_transaction.rs` が固定。
- `crates/wire-server/tests/wire19_ready_for_query_status.rs`・
  `wire942_extended_transaction.rs`（Issue #1179）: トランザクション内 COPY の
  ReadyForQuery 状態・拡張クエリ経路の UPDATE／DELETE／UPSERT・停滞した COPY の
  受信期限を固定。
- `crates/wire-server/tests/`: 既存の複数文・COPY・エラー射影テストが
  `check_write_placement` のシグネチャ変更・`SqlOutcome` の新 variant 追加後も
  無変更のまま green（回帰なし）。
- `crates/wire-server/tests/err4_http_projection.rs`・
  `crates/wire-server/docs/nosql-api.md`（`nosql_api_doc.rs`）: 新規 5 分類
  （`55P03`／`25000`／`25001`／`25P01`／`25P02`）を NoSQL 表層からの到達不能
  分類として追加し、production の応答エンコーダ経由で射影のみを固定する
  （NoSQL 表層の `op` 許可リストにトランザクション制御が無いため）。
- RECOVER-12 の再送手順の検証（Issue #1354。実装済み）: engine の
  `crates/engine/tests/recover12_explicit_txn_resend.rs`（行制約を持つ表での台帳照合の
  順序）、wire の `crates/wire-server/tests/recover12_explicit_txn_resend.rs`（層 A）、
  `three_client_e2e.rs` の層 B（psql／psycopg／node pg の 3 クライアント。
  [`three-client-e2e-harness.md`](./three-client-e2e-harness.md)）。
- 暗黙トランザクションが従来どおり開始時に writer を取ることの固定（Issue #2401。SQL-31・
  WIRE-16。本番コードの変更は無い）: 取得は開始時の 1 回だけで最初の書き込み文では取り直さない
  （`core/deferred_writer_tests.rs`。明示 Tx との対照つき）、先頭の読み取りを跨いで保持する・
  開始自体が `55P03` で `Idle` のまま 1 文も実行されない（`tests/core_misc/wire16_implicit_txn.rs`）、
  待機上限での `55P03`（`tests/write_lock_wait.rs`）、wire 越しの複数文メッセージ
  （`crates/wire-server/tests/write_lock_wait_parity.rs`）。
- wire 越しの開始時取得と状態バイトの固定（Issue #2410。SQL-31・WIRE-16・WIRE-19。本番コードの
  変更は無い）: 先頭が読み取りの暗黙 Tx は先頭文の `RowDescription` より前に `55P03` になり、
  書き込みが最後の複数文メッセージ（順次実行）との対照で取得時点を判別する。待機中は応答が届かず、
  取得後にスナップショットを開く（`write_lock_wait_parity.rs`）。遅延取得後の `ReadyForQuery` の
  状態バイト（writer を持たない `Active`・最初の書き込みの `55P03`／`40001`・`BEGIN` を含む複数文・
  拡張クエリ・期限切れ）は `crates/wire-server/tests/wire19_deferred_writer_status.rs`。
- 先頭文（または唯一の文）が 0 行 DELETE の場合の再送判定（Issue #1403。実装済み）:
  明示トランザクション内の DELETE は autocommit と同じフォーム関数へ `WriteTarget::InTxn`
  を渡し、0 行でも台帳を共有 write txn へ記録して `mark_written` する。このため従来の
  既知の制約は解消済みで、engine の `recover12_explicit_txn_resend.rs` が単一行形・
  述語形の両方で固定する（commit 後の再送は台帳由来の `23505`、COMMIT 前の接続断では
  台帳が残らない対照、後から INSERT された行を再送が消さないこと、テナント単位の台帳）。
  本番コードの変更は不要だった。
  wire 層 A（`crates/wire-server/tests/recover12_explicit_txn_resend.rs`）にも同じ
  シナリオを射影した（Issue #1433）: 単一行形・述語形 × `RETURNING` 有無（0 行のため
  `DataRow` は無く、再送は `RowDescription` より前に `ErrorResponse`）・暗黙トランザクション
  （複数文メッセージ）。本番コードの変更は無い。

## 起源（Explicit／Implicit）ごとの遷移差分（Issue #1175）

`ActiveTxn` は起源 `TxnOrigin`（`Explicit`／`Implicit`）を持つ。起源の違いは状態遷移
だけで、文の実行ディスパッチ（`core.rs::execute_parsed_in_txn`）は区別しない。

| 事象 | Explicit | Implicit |
| ---- | -------- | -------- |
| writer の取得時点（#2396・1a） | 最初の書き込み文（取得前の読み取りは `40001` の判定対象） | 開始時（従来どおり。#2401・WIRE-16。取得前に読み取りが無いため `40001` の経路を持たない） |
| 開始 | `BEGIN`（`begin`） | メッセージ先頭で wire 層が `begin_implicit`（`Idle` からのみ。それ以外は `XX000`） |
| 文のエラー・上限超過・`operation_id` 再利用（`fail`） | `Failed`（`ROLLBACK` 待ち） | `Idle`（write txn を abort・ライタ解放） |
| 確定 | `COMMIT`（上限超過は `Failed` へ遷移し `54000`） | 最後の文の成功後に `commit_implicit`（上限超過・commit 失敗のどちらも `Idle`） |
| 期限切れ（`release_if_expired`） | `Failed`（次の文で `54000`） | `Idle`（メッセージをまたがないため通常は到達しない） |
| `BEGIN`／`COMMIT`／`ROLLBACK` が届いた場合 | 通常の遷移 | 不変条件違反として abort して `Idle`、`XX000` |
| `ReadyForQuery` の状態バイト | `'T'`／`'E'` | メッセージ内の途中は送らず、終了時は常に `'I'` |

## 対象外・申し送り

- #2396 関連の申し送り: #2408（`55P03`・上限の起点・カーソル・状態バイトの整合と層 B）、
  #2411（カーソルの分割の検証。実装は #2403）、#2413（CHANGELOG・実装記録。記載済み）、#1933（READ ONLY）、
  #2230（行ロック句・`LOCK TABLE` は専用節を参照）。
- 待ちの途中で期限を越えた最初の書き込み文は、writer を取って成功する（取得後に期限を再検査しない）。
  次の文か `COMMIT` が `54000` で拒否し永続化しない（fail-closed。#2409 の T10 で現行契約を固定）。
  取得後の再検査が要るなら別 Issue で扱う。
- 書き込み後の読み取りでのキャッシュ・HNSW の再利用（テーブル単位の最適化。現状は
  brute-force）。
- `DUPLICATE_OPERATION_ID` と `UNIQUE_VIOLATION` の `code` ラベル区別は Issue #1180 で
  engine の `ErrorClass` と HTTP `code` に実装済み（ERR-6）。pg wire の `ErrorResponse` は
  ERR-1 の既存形式（`S`/`C`/`M`）のままで、固定文言の違いで区別する。
- 暗黙トランザクション（WIRE-16）による複数文の書き込み位置制約の撤廃は
  **Issue #1175 で実装済み（制約付き）**。`BEGIN` を含まないメッセージのうち書き込みが
  最後以外にある形は、メッセージ全体を 1 つの暗黙トランザクションで原子的に実行する
  （詳細は [`wire-multi-statement.md`](./wire-multi-statement.md)「原子性」節）。
  使える文は明示トランザクションと同じ許可リスト（Issue #1179 で拡大済み。`RETURNING`
  は UPSERT のみ未対応）。`BEGIN` より前の書き込みを
  明示ブロックへ昇格させる PostgreSQL の意味論（`INSERT; BEGIN; ...`）は対象外。
- アクセスモード修飾子（SQL-38・Issue #1932）: `BEGIN [WORK|TRANSACTION] [READ ONLY|READ WRITE]` を
  構文解釈し、`ActiveTxn` が `TxnAccessMode` を持つ。モードなしの `BEGIN` と `READ WRITE` は
  従来どおりの読み書き Tx。`READ ONLY` の暫定ゲート（`0A000`）は Issue #1934 で除去し、
  `READ ONLY` の Tx を開始できる（「READ ONLY での書き込み拒否」節）。PostgreSQL との差異として、アクセスモードの重複・矛盾（`READ ONLY READ WRITE`）
  は `42601`（fail-closed）。引用識別子 `"READ"` も受理される（既存の `"WORK"` と同じ性質）。
- savepoint、`COMMIT AND CHAIN`・`PREPARE TRANSACTION`。分離レベルの指定は「分離レベルの受理方針」節のポインタ先の契約に従う（`BEGIN ISOLATION LEVEL` は #1938 で実装済み、`SET TRANSACTION` 系は #1939 で実装済み）。
  別名 `START TRANSACTION`／`END`／`ABORT` は Issue #1930 で受理した（下記「別名」節）。
- NoSQL 表層のトランザクション（設計上 `0A000`。`op` 許可リストに追加しない）。
- 上限の既定値（20 秒・1,000 件・30 秒）と CLI のハード上限（60 秒・10,000 件。Issue #1553 で暫定採用）の確定 → オーナー判断。

## カーソルとの関係

`DECLARE`／`FETCH`／`CLOSE`（Issue #937・WIRE-15・TASK-218）は本トランザクション
機構の上に実装されており、カーソルは `sql::transaction::ActiveTxn` に埋め込
まれるため、`COMMIT`／`ROLLBACK`／`fail`／期限切れ／接続断のいずれでも自動的に
クローズされる。詳細は `docs/design/sql-cursor.md` 参照。

改訂（#2396・1a-8）: writer の取得前に宣言したカーソルは `BEGIN` 時点のスナップショット、
取得後に宣言したカーソルは取得時点のスナップショットを読む（WIRE-15。実装は #2403・検証は #2411）。
取得前に宣言したカーソルが残っている間は、`BEGIN` 時点の `ReadTransaction` を保持し続ける。

## 別名（Issue #1930・SQL-69 (1) ポインタ）

本節はポインタ表記のみとし、受理構文・エラー契約の詳細は本書へ記述しない
（AGENTS.md の公開境界拡張手順〔管理者承認〕が必要なため。現時点では承認を得ていない）。

- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-69・SQL-31。
- 実装箇所: `sql::transaction::validate_transaction_control_tokens`・`is_transaction_control_head`・
  `is_rollback_statement`、複数文メッセージの分割器（制御文の分類）。
- テスト: `crates/engine/tests/sql_t40_up/sql69_txn_aliases.rs`・
  `crates/wire-server/tests/wire_proto/sql69_txn_aliases.rs`・`wire942_extended_transaction.rs`。

## READ ONLY での書き込み拒否（SQL-38 (3)・ERR-6・Issue #1934）

`BEGIN READ ONLY` の Tx は writer を一度も取らず、書き込み系の文を `25006`
（`read_only_sql_transaction`）で拒否して `Failed` へ遷移させる。契約の詳細は spec のポインタ先に従い、
本節は実装の構造だけを記す。

- **判定は許可リスト方式**: `core.rs` の `read_only_permits` が読み取り・セッション設定・カーソル等だけを許し、
  DML・DDL・`COPY FROM`・ロック・`USING OPERATION_ID` 付きの文と未知の variant は拒否する（fail-closed）。
  文の種類だけで判定し、表名・テナントは参照しない。拒否は writer・台帳・採番・カタログへ触れる前に起きる。
- **経路**: 主判定は `execute_in_active_txn_inner` の先頭（PL/pgSQL 本体・`DO` の入れ子の文も通る）。
  `COPY FROM` は `begin_copy_active_inner` と一時表向けの入口で CopyInResponse の前に拒否する。
  `SessionTransaction::ensure_writer_with` と `LOCK` 実行腕の `25006` は取りこぼしに備えた保険。
- **優先順位**: `PARTITIONED` 付き書き込みは既存の `25001` が先。DDL・ポリシー・`CREATE FUNCTION` は
  権限判定（`42501`）より先に `25006`。PL/pgSQL 本体の書き込みは、既存の非 VOLATILE 拒否（`0A000`）・
  台帳必須構成の `23502` が先に動く（VOLATILE かつ台帳任意のときだけ `25006`）。
- **PostgreSQL との既知の差**: 一時表への書き込みと `COPY FROM`、弱いモードの `LOCK TABLE` も拒否する
  （fail-closed）。緩和が必要なら別 Issue で扱う。
- HTTP の射影は 403。NoSQL にはトランザクション制御が無く到達しない。

## READ ONLY の読み取りトランザクション（SQL-38・SQL-31・SQL-46・Issue #1933）

`BEGIN READ ONLY` は writer を取らず、commit 記録の登録簿にも載らない素の redb 読み取りトランザクションで開く。

- **登録しない理由**: READ ONLY は `ensure_writer_with` で必ず `25006` になり `40001` の判定に到達しない。
  登録があると他セッションの commit が足跡を計算し、記録を `G_s` より新しい分だけ保持させられるため、
  その負荷を READ ONLY が他セッションへ掛けないようにする。`BEGIN READ ONLY` が他セッションの commit 中に
  待たされることもない。
- **読み取り集合**: `read_set_scope` は READ ONLY で `None`。読み取り集合は空のまま。
- **世代**: `snapshot_generation` は未登録の 0（暗黙 Tx と同じ扱いで、判定には使わない）。
- **EXPLAIN**: writer を取らないことだけを保証する。検証は従来どおり最新の確定カタログ（#2403 の既存契約）。
- **上限**: 持続時間・文数は `BEGIN` から数え、アクセスモードに関係なく効く。
- 並行のスナップショット一貫性・テナント境界は #1935 で検証済み（`tests/sql_t01_39/sql38_read_only_concurrency.rs`・`core/deferred_writer_tests.rs`）。`USING PLAN` 付き SELECT と `EXPLAIN` は文ごとに最新の確定状態を読む既知の残りのため、一貫性の対象外（writer を取らないことだけを確かめる）。
