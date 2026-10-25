# 文の協調的中断と切断検出（Issue #1798）

## ステータス・対象

- ステータス: 採用（設計記録。Issue #1798 で実装）
- ビヘイビア ID（ポインタのみ）: SQL-34・WIRE-11（関連: WIRE-15・ERR-6・SQL-71）。spec 本文は転記しない
- 対象: engine の協調的中断の仕組み（`engine::interrupt`）と `57014`、wire-server の切断検出（`liveness`）
- 後続: #1951（文単位のデッドラインと協調キャンセルポイント）、#1950／#1954（statement_timeout）

## 背景

`LIMIT` 省略の結果は `cap + 1` 件目の確定で `54000` になる（[limit-omitted-result-cap.md](limit-omitted-result-cap.md)）。
ただし「上限内だが走査が長い」文は、クライアントが切断しても走査を続け、接続スレッドと接続スロットを占有し続けていた。
engine には中断の仕組みが無かったため、実行中の文を外から止める共通の入口を作った。

## 設計

### engine の中断スコープ

- `InterruptScope::install(poll)` が thread-local に `poll` を積み、RAII ガードの drop で外す（入れ子可）
- 走査ループは `interrupt::check()?` を呼ぶ。スコープが無ければ何もしない。`CHECK_STRIDE`（1024）回に 1 回、導入後の初回は必ず `poll` を評価する。中断が確定したら以降はラッチして同じ理由で失敗させる
- `poll` が返す `InterruptReason` は `ClientGone`（本件）・`UserCancel`・`StatementTimeout`（拡張点。現状は発生しない）
- エラーは `SqlSurfaceError::QueryCanceled` から `ErrorClass::QueryCanceled`（`57014`、ラベル `QUERY_CANCELED`）。文言は PostgreSQL と同じ固定の英語で、理由・件数・テナント・行の内容を含めない。HTTP への射影は 503
- 確認点: `sql::scan::with_visible_row`（全走査経路が通る入口。順序なし・経路 (A)・(B) の両パス・PK 点検索・GIN 候補）、`sql::window` の実体化ループ、`sql::group_by`・`sql::aggregate` の行取り込みループ、`sql::relation_snapshot` の入力読み込み、`sql::join::exec` のプローブと一致ペアのループ
- 置かない場所: commit 処理中・台帳（recovery）・commit 成功境界より後。中断は commit 前の `Err` としてしか起きないので、autocommit は副作用ゼロ、明示トランザクションは `Failed`（`25P02`）という既存の契約がそのまま成り立つ
- 分割実行 DML（PARTITIONED）は `interrupt::suppressed` でジョブ全体を包み、この Issue では中断の対象外にする。チャンク commit 後に `57014` を返すと、commit 済みなら `VD001` という契約（SQL-32／SQL-71 (4)）とずれるため。分割実行の中断は #1950 で扱う
- `SessionState` や `PolicyContext` には載せない。前者は値型で `sql/transaction.rs` が clone して巻き戻し、後者は `Eq` を索引の同一性照合に使っているため。engine には既に thread-local の前例がある
- `parallel_search` の `thread::scope` ワーカーからは thread-local が見えない。距離 ORDER BY の `LIMIT` 省略は #2242 の管轄で本件の経路外

### #1951 との共有の接点

statement_timeout のデッドラインは、同じ `interrupt::check()` の判定にデッドライン比較を足すだけで実装できる。
並行する第 2 の仕組みを作らない。`InterruptReason::StatementTimeout` がそのための拡張点で、`57014` を共有する。

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

- engine: `crates/engine/tests/sql1798_interrupt.rs`（毒行の手前で止まること・明示トランザクションの `Failed`・autocommit の副作用ゼロ・stride・ガード drop）と `interrupt.rs` の単体テスト
- wire: `crates/wire-server/tests/wire1798_limit_omitted_streaming.rs`（simple / extended の上限内完走と上限超過、`max_rows` 分割、切断後のスロット回収）と `liveness.rs` の単体テスト
- 切断後のスロット回収はタイミングに依存するため、「最終的に回復する」ことだけを固定する

## 対象外

- CancelRequest の受け付け（spec 判断待ち）
- 分割実行 DML の中断（#1950）
- 切断検出の CLI による無効化・間隔変更
