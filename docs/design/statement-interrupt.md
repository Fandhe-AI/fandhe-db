# 文の協調的中断と切断検出（Issue #1798）

## ステータス・対象

- ステータス: 採用（設計記録。Issue #1798 で実装）
- ビヘイビア ID（ポインタのみ）: SQL-34・WIRE-11（関連: WIRE-15・ERR-6・SQL-71）。spec 本文は転記しない
- 対象: engine の協調的中断の仕組み（`engine::interrupt`）と `57014`、wire-server の切断検出（`liveness`）
- 後続: #1950／#1952／#1953／#1954（statement_timeout。#1951 の機構は本文「文単位のデッドライン」節）

## 背景

`LIMIT` 省略の結果は `cap + 1` 件目の確定で `54000` になる（[limit-omitted-result-cap.md](limit-omitted-result-cap.md)）。
ただし「上限内だが走査が長い」文は、クライアントが切断しても走査を続け、接続スレッドと接続スロットを占有し続けていた。
engine には中断の仕組みが無かったため、実行中の文を外から止める共通の入口を作った。

## 設計

### engine の中断スコープ

- `InterruptScope::install(poll)` が thread-local に `poll` を積み、RAII ガードの drop で外す（入れ子可）
- 走査ループは `interrupt::check()?` を呼ぶ。スコープが無ければ何もしない。`CHECK_STRIDE`（1024）回に 1 回、導入後の初回は必ず `poll` を評価する。中断が確定したら以降はラッチして同じ理由で失敗させる
- `poll` が返す `InterruptReason` は `ClientGone`（本件）・`UserCancel`・`StatementTimeout`（#1951 の `DeadlineScope` が発生させる）
- エラーは `SqlSurfaceError::QueryCanceled` から `ErrorClass::QueryCanceled`（`57014`、ラベル `QUERY_CANCELED`）。文言は PostgreSQL と同じ固定の英語で、理由・件数・テナント・行の内容を含めない。HTTP への射影は 503
- 確認点: `sql::scan::with_visible_row`（全走査経路が通る入口。順序なし・経路 (A)・(B) の両パス・PK 点検索・GIN 候補）、`sql::window` の実体化ループ、`sql::group_by`・`sql::aggregate` の行取り込みループ、`sql::relation_snapshot` の入力読み込み、`sql::join::exec` のプローブと一致ペアのループ
- 置かない場所: commit 処理中・台帳（recovery）・commit 成功境界より後。中断は commit 前の `Err` としてしか起きないので、autocommit は副作用ゼロ、明示トランザクションは `Failed`（`25P02`）という既存の契約がそのまま成り立つ
- 分割実行 DML（PARTITIONED）は `interrupt::suppressed` でジョブ全体を包み、この Issue では中断の対象外にする。チャンク commit 後に `57014` を返すと、commit 済みなら `VD001` という契約（SQL-32／SQL-71 (4)）とずれるため。分割実行の中断は #1950 で扱う
- `SessionState` や `PolicyContext` には載せない。前者は値型で `sql/transaction.rs` が clone して巻き戻し、後者は `Eq` を索引の同一性照合に使っているため。engine には既に thread-local の前例がある
- `parallel_search` の `thread::scope` ワーカーからは thread-local が見えない。距離 ORDER BY の `LIMIT` 省略は #2242 の管轄で本件の経路外

### 文単位のデッドライン（Issue #1951）

SQL-71 のポインタ。statement_timeout の engine 側の機構で、同じ `interrupt::check()` に時刻比較を足して実現した（第 2 の仕組みは作らない）。`57014` と `InterruptReason::StatementTimeout` を共有する。

- API: `StatementDeadline`（`disabled`／`after(start, timeout)`／`at(instant)`・`is_expired_at`・`remaining_at`・`check`）、`DeadlineScope::install(deadline)`（RAII の `DeadlineGuard`）、`current_deadline()`。`timeout` が 0 のときと加算が溢れるときは無効（PostgreSQL の `0` = 無効に合わせる）
- 置き場所: poll スタックとは別の thread-local スロット（期限・残り確認回数・ラッチ）。poll スタックは最内スコープしか評価しないため、期限をスタックへ積むと liveness スコープと重ねたときに切断検出か期限のどちらかが評価されなくなる。別スロットなら導入順・入れ子に影響されず、liveness を持たない経路（将来の HTTP 表層）にも期限だけを入れられる。不採用案: `Scope` に期限フィールドを足す方式（最内限定の評価規則を引きずるため）
- 入れ子は厳しい側が勝つ（`min`）。内側で期限を緩められない。ガード drop で直前のスロットへ戻る
- 評価順: `suppressed` 区間なら期限も評価しない → デッドライン → poll スタック
- チェック頻度: `CHECK_STRIDE`（1024）を共有し、カウンタはデッドライン専用に持つ。導入後の初回は必ず時計を読み、以後は 1024 回に 1 回。期限超過を検出したらラッチする。確認点は既に行ループ単位なので、1024 回に 1 回の時計読み出しは行処理コストに対して無視できる
- 無効時のコスト: thread-local の読み出しと分岐 1 つだけで、時計は読まない。時計の読み出しは `clock_now()` に集約し、単体テストが「無効時は 0 回・有効時は stride ごとに 1 回」を決定的に固定する（壁時計ベンチは揺らぐため受け入れの根拠にしない）。既存ベンチでの実測確認は #1950 全体の受け入れで扱う
- 期限後の中断遅れの上限（SQL-71 (3) のポインタ）: 期限到達後、最大 `CHECK_STRIDE` 回分の確認点間隔の処理時間 + 確認点を持たない区間（未結線のソート本体・ベクトル検索カーネル等。#1952／#1953 で結線）+ `Err` の巻き戻し。壁時計の固定値ではなく「最も重い確認点間隔の 1024 倍」として見積もる。重い 1 反復を持つループで間隔を縮める必要が出たら #1952／#1953 で別判断とする
- 既存の資源上限（総走査行数・結果件数・明示 Tx の時間／文数・書き込みロック待ち）には手を入れない。デッドラインはそれらを緩めも置き換えもせず、先に達したもので止まる
- エラー文言: `StatementTimeout` は `canceling statement due to statement timeout`、それ以外は従来の `canceling statement due to user request`。いずれも固定文言で件数・テナント・期限値を含めない
- `parallel_search` のワーカースレッドからは thread-local が見えない（期限の伝搬は対象外）

後続への申し送り: セッション状態・`SET`／`SHOW statement_timeout`・wire の `'Q'`／`'E'` での導入は #1954、走査・結合・集約・ソートの確認点の密度は #1952、書き込み・COPY・ベクトル検索の確認点とロック待ちの期限内計上（`remaining_at`／`current_deadline`）は #1953、分割実行 DML への期限適用は #1950。

### wire の切断検出

- `WireStream::liveness_probe()`（既定は `None`）が、接続ソケットの複製と受信経路と共有する side buffer の組を返す。認証後セッションの受信経路は `SideBufTcp` で包み、TLS 接続でも TLS レコード復号より下層（生バイト列）に置くため順序が保たれる
- `post_auth_loop` が `'Q'`（COPY のサブプロトコルを含む）と `'E'` の処理中だけ、`liveness::install` で切断検出のスコープを入れる。接続ごとに複製を 1 回だけ取り、メッセージごとの dup を避ける
- probe は `PROBE_INTERVAL`（250ms）以上の間隔で、非ブロッキングにして読めるバイトを上限（`SIDE_BUF_CAP` = 64KiB）つきの side buffer へ実際に読み取り、必ず blocking へ戻す。peek は未読バイトが残ると FIN 到達後も同じバイトを返し切断を判別できないため使わない（Execute と Sync を続けて送って閉じる通常の拡張クエリで起きる）。read が `Ok(0)`・`ECONNRESET` 等なら `ClientGone`、`WouldBlock` は継続、blocking へ戻せなければ接続を閉じる側へ倒す。受信経路は次の読み取りでまず side buffer を消費するのでバイトは欠けず順序も崩れない。side buffer が満杯の間は切断判定をせず生存扱い（クライアントが送信中。64KiB を超える未読パイプラインの後に閉じた場合は走査終了まで検出しない）
- 中断後は通常のエラー経路に乗る。ErrorResponse の書き込み失敗で接続が終わり、`SessionTransaction` の drop でライタが解放される
- PostgreSQL 17 との差: PG は既定（`client_connection_check_interval = 0`）では実行中の切断を検出しない。本実装は資源保護のために常に検出する。書き込み側だけ閉じた（半閉じ）クライアントも切断として扱い中断する。CLI での切り替えは足していない

### CancelRequest（今回は受け付けない）

spec の wire-protocol の「制約・宿題」は CancelRequest を未対応としており、WIRE-8 の改訂注記は WIRE-11 の確定後に拒否契約へ縮小改訂するとしている。
SQL-34・WIRE-11 に CancelRequest の受理契約は無いため、認証の無い新しい入口は有効にせず、現行の拒否契約を変えない。
spec 側の判断（CancelRequest の受理契約の新設）が必要で、Issue の受け入れ条件のうち「CancelRequest で止める」は未達として残る。

将来受け付けるときの前提条件:

- BackendKeyData の secret は CSPRNG（`auth::read_urandom`）から作る。現状は連番カウンタで予測でき、pid もプロセス共通のため、そのまま受け付けると他接続（他テナント）の文を推測でキャンセルできてしまう
- pid は接続ごとに一意にし、接続ごとの中断ハンドルをプロセス全体のレジストリ（`pid → (secret, handle)`）に登録する。セッション終了時に RAII で必ず外す
- CancelRequest は長さを厳密に検証し（16 バイト）、secret は定数時間で比べ、一致の有無にかかわらず応答を送らずに閉じる。TLS 前後の経路を同じ関数で扱う
- アイドル中に届いたキャンセルは無視し、文の開始時に中断状態を戻す

## テスト

- engine: `crates/engine/tests/sql1798_interrupt.rs`（毒行の手前で止まること・明示トランザクションの `Failed`・autocommit の副作用ゼロ・stride・ガード drop）と `interrupt.rs` の単体テスト。デッドラインは `interrupt/deadline.rs` の単体テストと `crates/engine/tests/sql_t40_up/sql1951_statement_deadline.rs`（sleep なしの決定的テスト）
- wire: `crates/wire-server/tests/wire1798_limit_omitted_streaming.rs`（simple / extended の上限内完走と上限超過、`max_rows` 分割、切断後のスロット回収）と `liveness.rs` の単体テスト
- 切断後のスロット回収はタイミングに依存するため、「最終的に回復する」ことだけを固定する

## 対象外

- CancelRequest の受け付け（spec 判断待ち）
- 分割実行 DML の中断（#1950）
- デッドラインの wire／CLI 結線（#1954）と各実行ループへの確認点の拡充（#1952・#1953）
- 切断検出の CLI による無効化・間隔変更
