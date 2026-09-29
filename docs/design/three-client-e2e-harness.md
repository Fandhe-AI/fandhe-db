# ADR: 3 クライアント統合検証ハーネスの層 A/層 B 分割（WIRE-1）

- ステータス: Accepted
- 対応: TASK-73（WIRE-1）、TASK-165（SQL-12・SEARCH-9）、TASK-168（SQL-13・SQL-14）、
  TASK-82（SQL-5〜7・9・10）、TASK-195（RLS-11。Issue #878 判断記録）
- 関連: TASK-67・TASK-68・TASK-69・TASK-70・TASK-71（wire プロトコル層）、
  TASK-74・TASK-75・TASK-80・TASK-161・TASK-162・TASK-166・TASK-167（SQL 表層）、
  TASK-137（RLS 暗黙適用）

## 背景

WIRE-1 は「無改造の実クライアント（psql・Python psycopg・Node.js pg）から
C1〜C4 を実行できる」ことを製品コードで統合検証するビヘイビアである。しかし
本リポジトリのローカル開発環境・Docker 開発コンテナ（`Dockerfile`）のいずれにも
`psql`／`psycopg`／`pg` は導入されておらず、`make ci` の必須経路へそのまま
組み込むと開発者のローカル検証（`make ci`）が壊れる。

## 決定

検証を 2 層に分割する。

- **層 A**（`crates/wire-server/tests/wire1_simple_query.rs`）: 生バイトの wire
  クライアント（`tests/common`）による常時回帰テスト。`make ci`・`cargo test`
  の通常経路で毎回実行される。3 クライアントが実際に送信するのと同一の簡易
  クエリバイト列を検証するため、層 B が実行できない環境でも WIRE-1 の中核
  契約（結果セット整形・エラー応答・接続維持）を回帰保護する。
- **層 B**（`crates/wire-server/tests/three_client_e2e.rs`）: 実 `psql`・
  Python `psycopg`・Node.js `pg` を子プロセスとして駆動する統合テスト。
  `#[ignore]` とし、`make e2e-three-client` から明示的に実行する。ツール
  未導入・クライアントスクリプトの失敗は silent skip せず `panic!` で
  失敗させる（実行された場合に「全クライアントが実際に成功した」ことを
  保証するため）。C1〜C4（TASK-73／ビヘイビア WIRE-1。定義は private spec
  参照）それぞれを 3 クライアント全てで実行し、各ドライバでの挙動が独立
  オラクルと一致することを検証する（codex-review P2 指摘・PR #210）。

層 B は `.github/workflows/ci.yml` の必須チェックには含めない（このリポジトリで
psql・psycopg・pg の導入自動化を確定させるには、pip/npm の実バージョン確認が
必要であり、依存最小・完全固定方針（[dependency-policy](../../.claude/rules/dependency-policy.md)）
の下でユーザー承認を経て別途整備する）。

### TASK-165: セッション複数文の対応と検証範囲

`USING MODE` 句・`SET search_mode`（SQL-12）・`precision` の確信度ゲート
（SEARCH-9）の wire 経由検証も同じ層分割に従う。

- **層 A**（`crates/wire-server/tests/wire_search_mode.rs`）: 主たる回帰保護。
  クエリ句／セッション変数の優先順位、未知モード値・`$n` 形式の拒否、低確信度
  `precision` が空集合の通常応答（エラーではない）になること、テナント境界を
  越えて確信度ゲートへ他テナントの `Private` 行が混入しないことを、生バイトの
  wire クライアントで常時（`make ci`）検証する。
- **層 B**（`crates/wire-server/tests/three_client_e2e.rs`）: `SET
  search_mode = ...` を先行実行してから本体の `SELECT` を送る、同一接続内の
  セッション複数文を 3 クライアントで検証するため、既存の単文実行
  （`WIRE_SQL`）に加えて任意の `WIRE_SQL_PRELUDE`（実行順を保った SQL 文の
  JSON 配列）を追加した。`tests/three_client/{psycopg_client.py,pg_client.js}`
  は `WIRE_SQL_PRELUDE` を同一接続で逐次実行してから `WIRE_SQL` を実行し、
  psql 側は `run_psql_session`（複数 `-c` を同一セッションで送る）で同じ契約を
  実現する。拒否経路の検証では、各クライアントの失敗出力に SQLSTATE
  （`[SQLSTATE=<code>]`）を含めるようにし、`run_*_session_expect_sqlstate`
  ヘルパーで「非 0 終了 かつ期待 SQLSTATE を含む」ことを assert する。
  代表ケース（クエリ句 precision の Top-1 応答／`SET` 経由の precision 適用／
  未知モード値の拒否）のみを検証し、層 A で確定済みの全閾値パターンを層 B へ
  複製しない（層 B は無改造クライアント経由の受信確認が目的であり、閾値の
  網羅は層 A の責務）。

### TASK-168: 集計クエリ（SQL-13／SQL-14）の検証範囲

集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`。SQL-13）・`GROUP BY`/`HAVING`
集計（SQL-14）の wire 経由検証も同じ層分割に従う。

- **層 A**（`crates/wire-server/tests/wire_aggregate.rs`）: 主たる回帰保護。
  単一行集計・`GROUP BY` の既定順（キー昇順）・`HAVING`・`ORDER BY`／`LIMIT`、
  空集合契約（`COUNT`=0、他は NULL）、数値オーバーフロー（`22003`）・
  型不整合／許可形状外（`22000`／`42601`）の拒否経路、テナント境界（他
  テナントの `Private` 行を大量追加しても `COUNT`／`GROUP BY` の結果が
  不変であること）を、生バイトの wire クライアントで常時（`make ci`）検証
  する。
- **層 B**（`crates/wire-server/tests/three_client_e2e.rs`）: `docs` テーブルに
  Private 行（他テナントにのみ存在するグループ値を含む）を追加した専用 seed
  （`seed_aggregate_three_tenant_db`）に対し、単一行集計・`GROUP BY`/`HAVING`・
  RLS 不変（Private 専用グループが現れない）・拒否経路 2 種（型不整合・
  許可形状外）の代表ケースのみを 3 クライアントで検証する。Node `pg` の
  `Object.values(row)` 出力仕様（同名列の衝突）を避けるためすべての SELECT に
  一意の `AS` 別名を付け、NULL 描画の描画差異（psql／psycopg／pg で表現が
  異なる）を避けるため NULL を返す SQL は使わない（NULL 契約は層 A の
  DataRow -1 長検証に閉じる）。

### TASK-82: 拡張構文（SQL-5〜7・9・10）の検証範囲・`extended_syntax_e2e.rs` の採用理由

`USING PLAN`（SQL-5）・`EXPLAIN`（SQL-6）・`HINT ORDER`（SQL-7）・宣言的 UDF
呼び出し（SQL-9）・`USING OPERATION_ID` 付き `INSERT`（SQL-10）の wire 経由
検証も同じ層分割に従う。spec（TASK-82）の定義に基づき、層 B は
`three_client_e2e.rs` とは別ファイル（`extended_syntax_e2e.rs`）に分離した。
層 A はそれぞれ既存の流儀（`wire_using_plan.rs`（TASK-117・PLAN-9 で
先行実装済み）・`wire_explain.rs`・新規 `wire_hint_order.rs`・
`wire_udf_call.rs`・`wire_insert_operation_id.rs`）に従う。

- **層 A**: 各構文の規則自体は engine 側 in-process 結合テスト（`sql_using_plan.rs`
  等）が確定オラクルとして検証済みのため、同じ規則が wire フレーミング越しに
  観測できることの確認に徹する。常時（`make ci`）実行される。
- **層 B**（`crates/wire-server/tests/extended_syntax_e2e.rs`）: `USING PLAN`・
  `EXPLAIN` は `wire-server` バイナリへの `--planner-endpoint`／
  `--planner-model`／`--embedder-hashing-dim`（TASK-117）注入を要するため、
  子プロセス起動前にプロセス内 HTTP スタブ（Ollama `/api/generate` 互換の
  最小応答。`engine::query_planner` の単体テスト `spawn_stub_server` と同型）を
  立て、`--planner-endpoint 127.0.0.1:<stub port>` として渡す。`HINT ORDER`・
  `CREATE FUNCTION`・`INSERT` は追加注入を要しないため素の `wire-server` 起動で
  足りる。各構文 1 本の代表ケースのみを 3 クライアントで確認する（層 A で
  確定済みの拒否経路・順列網羅を層 B へ複製しない方針は TASK-165・TASK-168 と
  同じ）。

**INSERT の wire 受理（判断の記録）**: TASK-82（SQL-10）の定義に基づき、本タスク
で wire 経由の `INSERT` 受理へ切り替えた（下記「スコープ外」の旧項目を参照。
判断の詳細は `simple_query.rs` モジュールコメント）。読み取り可視性の既定
（`Public` のみ）は当時拡大しなかった。RLS-11・TASK-195（Issue #973・
PR #977）で wire 認証導出の `PolicyContext` が `Public` ＋ 自テナントの
`Private` を許可可視性とするよう改訂され、この非対称は解消済み
（詳細は後述「Issue #878: wire セッションの可視性非対称と DML の相互作用
（判断記録）」節参照）。

### Issue #454: 広域取得（ソートなしのフィルタ取得）の検証範囲

`ORDER BY`／`USING PLAN` を伴わない `SELECT ... [WHERE ...] LIMIT n`（広域取得。
契約の詳細は `docs/design/wide-retrieval-scan.md`）の wire 経由検証も同じ層分割に
従う。

- **層 A**（`crates/wire-server/tests/wire_scan.rs`）: 実行契約自体は engine 側
  in-process 結合テスト（`sql_scan.rs`）が確定オラクルとして検証済みのため、
  同じ規則（複数行の返却・`WHERE` 絞り込み・RLS 非漏えい・`USING MODE`／
  `EXPLAIN` 前置の拒否・取得モードからの独立性）が wire フレーミング越しに
  観測できることの確認に徹する。常時（`make ci`）実行される。
- **層 B**（`crates/wire-server/tests/extended_syntax_e2e.rs::
  three_clients_run_scan_where_nosort`）: 追加注入を要しないため素の
  `wire-server` 起動で足りる。`seed_plain_docs`（単一テナント）に対する
  代表ケース 1 本（bare `LIMIT` と、可視総数を超える `LIMIT` の 2 パターン）を
  3 クライアントで確認する（層 A で確定済みの拒否経路・順列網羅を層 B へ
  複製しない方針は TASK-165・TASK-168・TASK-82 と同じ）。

### Issue #705: テスト専用 commit 後 panic 注入フラグ（`fault-injection` feature）

**目的**: commit 成功境界を跨いだ panic → 緊急応答の同期送出 → abort
（TASK-97・RECOVER-6。応答本体は `D`=`state=may_be_committed`。ERR-5）は、
これまで engine のプロセス内テストと wire-server 層 A
（`tests/wire_emergency_response.rs`。テストバイナリ自身が登録・commit・
panic を再現する）でしか観測できず、`wire-server` バイナリを外部クライアント
から commit 後 panic させる手段が無かった。#706（3 クライアント e2e で `D`
到達を確認する）の前提として、テスト専用・feature gate 付きの注入フラグを
バイナリへ追加した。

**feature gate と CLI 契約**: `crates/wire-server/Cargo.toml` の
`fault-injection` feature（default に含めない・依存追加なし）を有効化した
ビルドでのみ `--fault-inject post-commit-panic` を受理する。既定ビルドでは
このフラグ自体が存在せず `main.rs` の `other =>` 分岐で未知引数として拒否
される。値欠落・不正値（閉じた語彙 `post-commit-panic` の厳密一致のみ受理）・
2 回目以降の重複指定はいずれも fail-closed で起動エラー（`--search-engine`
等・Issue #656 と同じ判断枠組み）。

**注入点の位置と理由**: 実際の panic 注入は
`crate::simple_query::execute_and_respond` の「登録ブロック」
（`_emergency_registration` が生存する区間。同関数のコメント参照）の**内側**、
`engine.execute_sql_in_session` の呼び出し直後に置く
（`crate::fault_injection::maybe_panic_after_commit`）。この区間の外
（ブロック終端後の `match outcome { .. }` 側）で panic しても、登録は既に
drop 済みで緊急応答は送られず接続断（RECOVER-5 の abort バックストップ）に
なるだけのため、注入点をここより後ろへ移動してはならない。登録ブロックの
境界自体は変更していない。

**arm-once と発火条件**: `main.rs::run_server` が bind 成功後・`listening on`
出力前に `fault_injection::arm` を高々 1 回呼ぶ（テストが両行の出力順に
依存できるようにするため）。発火は `Ok(SqlOutcome::Insert(_))`（commit 成功
を意味する）のときだけ arm を消費する（`compare_exchange` による take-once）。
`Err(_)` や他の読み取り専用 variant（`SELECT`・`EXPLAIN` 等）では arm は
一切触らず据え置くため、失敗した INSERT・SELECT を挟んでも後続の成功
INSERT で確実に発火する。

**安全性の判断**: default features に含めないため既定ビルド・crates.io
公開の既定構成にはシンボルもフラグも存在しない。feature を有効化しても
露出するのは「CLI を握る者が自プロセスを 1 回だけ commit 後 panic で終了
させる」能力のみで、テナント境界・RLS・認証・fail-closed 経路を迂回する
API は一切露出しない（engine の `bench-internals` feature と同じ判断
枠組み）。発火経路自体は production の RECOVER-6 経路そのもので、追加する
のはトリガーだけ。`make lint`／`make test` は `--all-features` のため CI
でも常にコンパイル・実行され、feature コードの腐敗を防ぐ。既定ビルド
（feature 無効）側の `--fault-inject` 拒否契約は `make test-default-build`
（`make ci` に含む）・`.github/workflows/ci.yml` の `test-default-build`
ジョブが担う（Issue #715・#716）。

**検証層**: 単体テスト（`crates/wire-server/src/fault_injection.rs`）は
`is_committed_insert`／take-once の判定純関数のみを検証する（実発火は
`ResponseBoundaryGuard` の `Drop` と `fail_fast::install` により必ず
SIGABRT に至るためプロセス内テスト不可）。結合テスト
（`crates/wire-server/tests/wire_fault_injection_cli.rs`）は
`CARGO_BIN_EXE_wire-server` の実子プロセスとして起動し、CLI 引数の拒否・
未 arm 時の通常応答・実際の発火（緊急応答の `S`/`C`/`M`/`D` フィールド・
abort・再オープン後の可視性）・arm 維持（SELECT・拒否された INSERT を
挟んでも消費されない）を検証する。e2e 層（#706・`three_client_e2e.rs`）は
本フラグを使って 3 クライアント経由の `D` 到達を確認する担当。

**既定ビルド側の検査経路**: `make test`／`rust-ci` は `--all-features` の
ため `cfg(not(feature = "fault-injection"))` の既定ビルド拒否テスト
（`default_build_rejects_fault_inject_flag_as_unknown_argument`）はこの経路
には入らない。この既定ビルド側の拒否契約は `make test-default-build`
（`make ci` に含む）・`.github/workflows/ci.yml` の独立ジョブ
`test-default-build` が常時実行して検査する（Issue #715・#716・PR #718）。

### Issue #706: 3 クライアントでの緊急応答 detail 到達検証

**目的**: ERR-5（`docs/spec/04-behavior/error-format.md`）の期待欄が定める
「無改造クライアントから `detail` へ到達できる」ことを、Issue #705 の注入
フラグを使って実クライアント経由で機械検証する。Issue #701 で `D` フィールド
自体のバイト列契約は層 A で固定済みだったが、psql・psycopg・node `pg` それぞれ
の**ドライバ API から**その値が実際に読めるかは未検証のまま残っていた。

**到達フィールド（実測で確認）**:

| クライアント | 到達フィールド | stderr 出力例 |
| --- | --- | --- |
| psql | `DETAIL:` 行（`-v VERBOSITY=verbose` 必須） | `DETAIL:  state=may_be_committed` |
| psycopg | `e.diag.message_detail` | `[DETAIL=state=may_be_committed]` |
| node `pg` | `err.detail` | `[DETAIL=state=may_be_committed]` |

**1 クライアント 1 サーバーの理由**: `--fault-inject post-commit-panic`
（Issue #705）は 1 プロセスにつき高々 1 回しか発火しない take-once 契約
（同プロセスへ 2 回目の成功 INSERT を送っても発火しない）。3 クライアントを
同一サーバーへ順に接続すると 2 クライアント目以降が発火を観測できないため、
`three_clients_receive_emergency_response_detail_after_post_commit_panic`
はクライアント（psql→alice、psycopg→bob、pg→carol）ごとに独立した
`ServerGuard`・一時 DB・テナントを用意する。

**psql の終了コードと `LC_ALL`**: psql は `ErrorResponse` 受信直後の接続断を
「connection to server was lost」として終了コード 2 で報告する（通常の拒否
経路——`ReadyForQuery` まで到達してからのエラー——の終了コード 1 とは異なる）
ため、検証は `!status.success()` のみで行う（`run_psql_session_expect_sqlstate`
と同じ判定方針）。`DETAIL:` ラベル自体は libpq の gettext 翻訳対象になり
得るため `LC_ALL=C` を渡すが、`state=may_be_committed` という生値そのものは
翻訳対象ではないためこのガードで十分。

**psycopg の接続断吸収挙動**: psycopg 3.x の内部ジェネレータは
`ErrorResponse` を受け取った直後の接続断由来の例外を、既に受け取った
FATAL エラー結果の陰に隠して送出する。結果として `cur.execute()` は素直に
`XX000` の `InternalError`（`e.diag.message_detail` 込み）を送出し、
呼び出し側で接続断由来の二重例外を個別にハンドルする必要はない。

**node `pg` の追加防御**: `client.query()` が reject した後に接続断由来の
後追い `error` イベントを `pg.Client` が emit することがあり、リスナー未登録
だと Node プロセス全体が uncaught 例外で異常終了する。`client.on("error",
...)` を登録し stderr へログするだけに留め、成否判定は既存の `.catch()` に
一本化した（`Issue #706` で追加。`WIRE_SQL_PRELUDE` を使わない通常の拒否
経路テストの挙動・出力形式は変えていない）。

**`pg` の一時導入**: 本リポは `pg` を `package.json` として常設していないため
（Node 依存の管理方針は本 ADR のスコープ外）、実行確認はリポ外のスクラッチ
ディレクトリへ `npm install pg@8.23.0` した上で `NODE_PATH` 環境変数経由で
`pg_client.js` に解決させた（PR #708 と同じ一時導入方法）。

**実行記録**: `make e2e-three-client`（`three_client_e2e` 5 件・
`extended_syntax_e2e` 6 件、計 11 件）で本テストを含め全件成功することを
確認した。個別実行時の観測 `[e2e-record]` 出力（実行環境固有の文言・
接続情報は含まない）は PR 本文の Test plan に転記した。

### TASK-183／HTTP-13: NoSQL 表層の 3 クライアント統合ハーネス（Issue #776）

NoSQL 表層（`--surface nosql`。HTTP/1.1 自作リスナー・`/v1/session`／
`/v1/session/close`／`/v1/query`）は Issue #734〜#772 で production 結線まで
実装済みだが、無改造の外部 HTTP クライアントから実バイナリへ接続する層 B
統合テストは未整備だった。SQL 表層側の層 A/層 B 分割方針（本 ADR 冒頭「決定」
節）をそのまま踏襲し、以下の形で追加した。

- **層 A**（既存。`tests/http4_session_issue.rs`・`tests/http5_query_bearer.rs`・
  `tests/http8_session_close.rs`・`tests/nosql1_*`〜`tests/nosql11_*`）:
  in-process サーバースレッド・生 HTTP/1.1 バイトの自作クライアントによる
  常時回帰テスト。NoSQL 表層自体の契約（`op` 許可リスト・束縛規則・
  `operation_id` 必須化・precision fail-closed・RLS 暗黙適用等）はこちらが
  主として担う。
- **層 B**（新規。`tests/three_client_http_e2e.rs`）: 実 `wire-server --surface
  nosql` を子プロセスとして起動し、無改造の `curl` から `POST /v1/session`
  （発行）→ `POST /v1/query`（`op: search`）→ `POST /v1/session/close`
  （失効）→ 失効後の同一トークン再送が `401`／`wire_code 28000` で
  拒否されることまでを確認するスモークテスト。あわせて stderr に
  `--surface nosql` 起動時の告知行が含まれること（SQL 表層が誤って
  起動していないことの非 vacuous な証跡）も検証する。`#[ignore]` とし
  `make e2e-three-client-http` から明示的に実行する（`ci` には含めない）。

**起動・ポート取得**: `tests/common/mod.rs::SpawnedServer`（`Drop` ガード付き。
`tests/http4_session_issue.rs::spawned_binary_accepts_valid_login_over_nosql_surface`
の前例と同型）をそのまま再利用した。SQL 表層側の `three_client_e2e.rs` が持つ
`spawn_wire_server`／`ServerGuard` の第 3 コピーは作らない判断とした。

**seed の複製**: `three_client_e2e.rs::seed_three_tenant_db`（`docs` テーブル・
3 テナント × Public 1 行）は private 関数のため import できず、
`extended_syntax_e2e.rs` の前例と同じ方式で `three_client_http_e2e.rs` 専用に
複製した。この seed は Public 3 行のみで Private 行を持たないため、
wire 認証が導出する `PolicyContext` の許可可視性が RLS-11・TASK-195
（Issue #973）後に `Public` ＋ 自テナントの `Private` へ広がった後も、
alice（tenant-a）から全 3 行が可視という既存オラクルはそのまま成立する
（自テナント `Private` 行が存在しないため RLS-11 の拡張が可視結果へ
影響しない）。

**curl 起動の設計**: `Command` の引数配列で `curl` を起動しシェルを介さない。
要求 JSON は固定 `const`／リテラルで、未検証の文字列連結は行わない。
`Content-Type: application/json` を明示し、`Expect:` を空値で送って
100-continue の余地を消した。NoSQL 表層は `Connection: close` 固定のため
1 要求 = 1 curl プロセスとし `--next` 連結はしない。取得したセッション
トークンは形状検証（43 文字・base64url アルファベット限定）を経てから
`Authorization` ヘッダへ載せる（untrusted な外部プロセス応答をそのまま
信頼しない）。curl 未検出・非 0 終了・応答形状不一致はいずれも `panic!` で
失敗させ、silent skip はしない。

**本 Issue のスコープ**: ハーネス＋curl ランナー＋curl での
session→search→close→失効後再送（`401`／`28000`）スモーク 1 本＋
`make e2e-three-client-http` に限定した。urllib／fetch ランナー・
3 クライアント一連手順の実行記録の整備・psql（SQL 経路）との結果一致比較は
後続 Issue（#777〜#779）へ申し送る。

### urllib／fetch ランナー（Issue #777）

Issue #776 で curl のみだった `three_client_http_e2e.rs` へ、Python 標準
ライブラリ `urllib.request`・Node.js 組み込み `fetch` の 2 クライアントを
追加した。SQL 表層側の `tests/three_client/{psycopg_client.py,pg_client.js}`
と同じ配置・起動方式（`tests/three_client_http/{urllib_client.py,
fetch_client.js}`・`PYTHON_BIN`／`NODE_BIN` 環境変数でインタプリタを解決）を
踏襲し、外部パッケージ（pip／npm）には一切依存しない
（`.claude/rules/dependency-policy.md`）。

**入出力契約**: 3 クライアント共通で接続先・要求本文・bearer トークンは
すべて `HTTP_HOST`／`HTTP_PORT`／`HTTP_TARGET`／`HTTP_BODY`／`HTTP_BEARER`
環境変数経由（argv・stdin は使わない。security.md P0）。成功時は stdout
1 行目に HTTP ステータス（10 進数字のみ）、2 行目以降に応答本文をそのまま
出力し終了コード 0（4xx／5xx も「応答を受信できた」として扱う。失効後
トークン再送で `401` を確認するステップに必要な契約）。転送路・プロトコル
障害（接続不能・タイムアウト・応答本文の上限超過・UTF-8 デコード不正・
必須環境変数の欠落）はいずれも終了コード 1 とし、stderr には障害種別のみを
書く（要求本文・bearer 値・env の値は echo しない）。応答本文の上限は
curl 経路と同じ `2 MiB`（拒否閾値として）、タイムアウトは 10 秒に揃えた。
`urllib_client.py` は `read(LIMIT + 1)` で受信量そのものを上限に抑えるが、
`fetch_client.js` は `res.arrayBuffer()` で先に全量を読み切ってから長さを
検査する（Node 組み込み `fetch` の標準 API では読み取り量の事前制限が
できないため）。本テストの対象はいずれも自前サーバー（本文は高々数百
バイト）で上限超過は想定しない経路のため、ストリーミング読み取りの
複雑化は見送った。

**curl との差分**: `fetch` は既定でリダイレクトを追従するため
`redirect: "error"` を明示し、サーバーが 3xx を返さない契約
（`crates/wire-server/docs/nosql-api.md`）から外れた場合は fail-closed に
倒す（curl 側は追従しても到達しない想定でそのまま）。要求ヘッダは両者とも
`Content-Type: application/json` を明示する（urllib の既定
`application/x-www-form-urlencoded`・Node fetch の文字列本文既定
`text/plain;charset=UTF-8` のままだと `08P01` になるため）。

**Node.js ≥18 前提**: `fetch` は Node の組み込みグローバル
（`require` 不要）だが Node 18 未満には存在しない。`typeof fetch !==
"function"` を検査し、非搭載環境では案内メッセージ付きで終了コード 1 にする
（ローカル実行環境は Node v24 系で確認済み）。

**Rust ランナーの共有化**: `curl_post` と同じシグネチャの
`urllib_post`／`fetch_post` を追加し、共通の `spawn_script_client`
（`three_client_e2e.rs::spawn_psycopg_client` と同型。`Command` 引数配列で
起動しシェル非経由）へ委譲した。テスト本体は
`run_session_search_close_scenario(client: HttpClient)` へ抽出し、
`curl_runs_session_search_close_over_nosql_surface`（既存名を維持）・
`urllib_runs_session_search_close_over_nosql_surface`・
`fetch_runs_session_search_close_over_nosql_surface` の 3 本の
`#[test] #[ignore]` から呼ぶ。シナリオ内容（session 発行→トークン形状検証→
search 3 行→close→失効後再送→stderr 非漏えい検証）はクライアント種別に
依存しない。`make e2e-three-client-http` は 3 テストとも一括実行する。

production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
無変更（テスト・スクリプト専任）。

### 実行記録の様式と運用手順（Issue #778）

**目的**: HTTP-13 の確定化判定（TASK-185）は製品コードでの実測を前提とする
ため、`make e2e-three-client-http` の実行結果を「いつ・どのコミット・どの
クライアント版で・何件 pass・何を観測したか」の形で再現可能に記録できる
導線が要る。SQL 表層側の `three_client_e2e.rs`（Issue #706・PR #708）が
採った `[e2e-record]` 出力・PR 本文への転記という様式をそのまま踏襲する。

**ハーネス側の変更**: `three_client_http_e2e.rs` の `HttpClient::version`
（実際に使う `CURL_BIN`／`PYTHON_BIN`／`NODE_BIN` 解決先の `--version` 出力を
取得。印字可能 ASCII・200 バイト上限へサニタイズ）と、シナリオ完走時の
`[e2e-record]` 行（クライアント種別・版・session／search／close／失効後
再送の各段の観測要点）を追加した。記録行はトークン・ユーザー名・
パスワード・テナント id を含まないことを出力前に `assert!` で機械検証する
（`--nocapture` で表示しても安全であることの保証。既存の stderr 非漏えい
assert とは独立に行う）。

**`Makefile` の変更**: `e2e-three-client-http` を
`-- --ignored --nocapture --test-threads=1` へ変更した。受け入れ条件が
「この make ターゲットを実行して記録する」ことに結びつくため、記録行が
make 実行そのものの出力に現れる必要がある。記録行は上記の assert で
秘密情報非含有が保証されるため `--nocapture` は安全と判断した。
`--test-threads=1` は 3 テストの `[e2e-record]` 行（および失敗時の診断
出力）が交錯して読み取れなくなることを防ぐ。SQL 表層側の
`e2e-three-client` は変更しない（先例どおり個別実行での記録取得のまま。
統一は対象外と判断）。

**記録テンプレート**（PR #708 の Test plan と同型。PR 本文の Test plan へ
以下を転記する）:

- 実行日（UTC）
- 対象コミット SHA（PR head）
- ツールチェーン（`rustc --version`）
- クライアント版（`[e2e-record]` の `client_version` を 3 クライアント分
  転記）
- テスト名 × 結果の表（3 行: curl／urllib／fetch）
- pass 件数
- `[e2e-record]` 3 行の転記

**秘匿規則**: トークン・ユーザー名・パスワード・テナント id・実行環境固有の
接続情報は記録しない。ハーネス側の `assert!`（上記）で機械的に保証する。

**再実行手順**:

```sh
make e2e-three-client-http 2>&1 | tee <scratch>/e2e-http.log
grep '\[e2e-record\]' <scratch>/e2e-http.log
```

出力された `[e2e-record]` 行と `test result:` 行を記録テンプレートへ転記
する。

**#779（SQL 経路パリティ）との分担**: 本節が扱うのは実行記録の様式・運用
手順のみであり、NoSQL 表層と SQL 表層の結果一致比較（search／scan／
aggregate）は次節が担う。production コード
（`crates/engine/src/`・`crates/wire-server/src/`）は無変更。

### SQL 経路パリティ（Issue #779）

**目的**: `run_session_search_close_scenario`（#776〜#778）は search 1 本の
外形確認にとどまり、SQL 表層との結果一致は明示的にスコープ外としていた
（上記「#779（SQL 経路パリティ）との分担」節）。本節はその後続として、
同一クエリ意図を SQL 表層（実 `wire-server`・無改造 `psql`）と NoSQL 表層
（実 `wire-server --surface nosql`・無改造 HTTP クライアント）の双方へ
投げ、**列名・型・行集合**が一致することを層 B へ組み込む。

**起動方式（順次 2 プロセス）**: redb は単一ライターのため同一 DB
ファイルを 2 プロセス同時には開けない。`seed_parity_db` で 1 回だけ seed
した DB ファイルに対し、SQL 表層（`--surface` なし）を起動して psql・
生 wire で全クエリを採取したのち `stop_and_drain`（SIGKILL）で終了させ、
同じ DB ファイルで NoSQL 表層（`--surface nosql`）を起動して HTTP
クライアントで全クエリを採取する。SIGKILL 後の再オープンは非クリーン
終了からの回復（open 時修復）を伴いうるが、本シナリオは読み取り専用の
ため無害であり欠陥ではない。

**型の観測方法（psql では取得不能）**: 拡張クエリプロトコルは `0A000` で
未対応のため `\gdesc` は使えない。列名・値は psql（`-A`・ヘッダ付き・
`-F '|'`・`-P footer=off`）から取り、型は同じ SQL 表層プロセスへの生
simple query（`RowDescription` の型 OID）から取る
（`sql_column_types_via_raw_wire`。固定表 OID 1700→`numeric`／25→`text`
以外は fail-closed に `panic!`。`crate::result_encoder::WireType` の
公告表と単一情報源）。

**正規化モデル**: NoSQL 応答の JSON セルは `json_cell_to_pg_text` で
psql のテキスト表現へ正規化してから比較する（`Cell::Integer`/
`Cell::Float` は両表層とも Rust `Display` 経由で同じ 10 進テキストになる
契約を利用）。正規化して通す**表現差**は、`VECTOR` 列・式項目（集計）の
型名がいずれも一律 `"text"` として公告される一方値は native JSON で
届く非対称、および `u64` が `2^53` を超える場合の JS 側丸め可能性。
**意味差**（行集合・件数・キー順・列名・型 OID 対応の不一致）は正規化や
アサート弱体化で吸収せず、テストを fail させる契約とした。

**比較順序**: `search`（`ORDER BY` あり）・`aggregate`（`GROUP BY` 既定
キー順）は順序付き比較、`scan`（SQL-15。順序保証なし）は多重集合比較を
行う。各ケースは固定オラクルとも一致させ、両表層が同じ誤りを返す
ケースを排除する。

**クエリ集合**: `crates/wire-server/docs/nosql-api.md`「SQL ↔ NoSQL 対応
表」に対応する search-1〜5・scan-1・agg-1〜4 の 10 ケース（`PARITY_CASES`）
を、alice（tenant-a）・bob（tenant-b）・carol（tenant-c）の 3 テナント
それぞれで実行する。seed には alice の Private 行（id=11）・bob の
Private 行（id=12）が含まれ、RLS-11・TASK-195（read-your-writes）後の
許可可視性（`Public` ＋ 自テナントの `Private`）により alice は自身の
id=11 を、bob は自身の id=12 をそれぞれの応答に legitimate に含む。
非漏えい検証は「自分の id ではない方の Private id」のみを禁止する形
（alice には id=12、bob には id=11、Private 行を持たない carol には
id=11／12 の両方を禁止）とし、3 テナントいずれも**他テナント**の
Private 行が両表層のどの応答にも現れないことをあわせて検証する。
search5 は codex-review 指摘（PR #838）対応で追加したケースで、search4
（どの seed body にも出現しない語を疎側項に使う）だけでは `hybrid.text`
を無視して密検索のみへ縮退する退行を検出できないため、id=3 の body に
実在する語（"unrelated"）を疎側項に使い密のみ順位とは異なる順位（id=3
が繰り上がる）を期待値に固定して疎側チャネルの寄与を検出可能にした。
own Private 行（alice の id=11・bob の id=12）は密側でクエリベクトルと
同一のため search3／search4 で上位へ現れ、search5 では RRF 融合後の
近接により alice の順位のみ変化する（`ParityCase` の doc コメント参照）。

**実測結果**: 3 クライアント（curl／urllib／fetch）× 3 テナント × 10
ケース＝ 90 組すべてで列名・型・行集合が RLS-11 後の期待値（own Private
行を含む）と一致することを確認済み（本開発
環境。`make e2e-three-client-http` の `[e2e-record] parity/<client>: ...`
行で `match=true` を確認できる）。実測で発見した意味差は無かった。

production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
無変更（テスト・docs 専任）。

### DML パリティ（Issue #877）

**目的**: SQL 経路パリティ（#779）は読み取り専用の 3 シナリオに限られて
おり、`UPDATE`／`DELETE`（単一行 `id` 完全一致形。SQL-17・SQL-18・
SQL-19）が SQL 表層と NoSQL 表層で同一の実行結果（影響行数・エラー
`wire_code`・操作後の状態）を返すことは検証していなかった。層 A
（`nosql12_update_delete.rs`）は同一プロセス内の 2 core 比較に留まる
ため、実バイナリ `wire-server`・無改造クライアント（psql／curl／
urllib／fetch）経由での検証を `run_sql_nosql_dml_parity_scenario` として
追加した（curl／urllib／fetch の 3 テストが共有）。

**起動方式（複数 DB・順次プロセス）**: DML は状態を変えるうえ、0 行の
`UPDATE`／`DELETE` も台帳に記録される
（`docs/design/sql-delete-single-row.md`「0 行成功への写像と台帳記録」節）。
同一 `operation_id` を「他テナント行向け」と「未存在 id 向け」の両方に
同一 DB 内で使うと 2 回目が `23505`／`22023` になり応答比較にならないため、
RLS-9 の応答同一性検証だけは他テナント行を含む DB（DB-F）と含まない
DB（DB-M）を分けて用意する。それ以外のケース（成功・`operation_id`
必須化・台帳照合・複数列 `SET` の宣言順）は、同一内容で複製した 2 つの
DB（DB-S を SQL 表層で、DB-N を NoSQL 表層で）へ同じ手順を順に適用して
比較する。

**書き込み後の SIGKILL について**: SQL 経路パリティ（#779）の「読み取り
専用のため無害」という理由づけは DML には当てはまらない。本節では DML
の応答を受信し終えてから `stop_and_drain` を呼び、durability は起動時
引数を渡さず既定（`immediate`）のままとし、commit 成功応答が返った時点で
永続化が保証される契約（`docs/spec/04-behavior/recovery.md` RECOVER-5
ポインタ）に依拠する。

**DML の採取方法（生 wire）**: SQL 側の `UPDATE`／`DELETE` は psql では
なく生 wire（`run_sql_dml`）で送る。psql は拡張クエリプロトコル未対応で
`CommandComplete` タグ・`ErrorResponse` の SQLSTATE／message を安定に
取り出せないため、`sql_column_types_via_raw_wire`（#779）と同じ方針を
踏襲する。読み取り専用の状態確認（操作後の最終状態）は引き続き psql
（`run_psql_with_header`）で行う——`ORDER BY` は距離関数（`<=>`／
`HYBRID(...)`）専用の構文でスカラー列には使えないため、SQL-15 の広域
取得（順序保証なし。`SELECT id, lang FROM docs LIMIT 100`）を使い呼び出し
元でソートしてから NoSQL 側の `op: scan` 結果と比較する。

**ケース集合（`DML_STEPS`）**: alice（tenant-a）が順に実行する 14
ステップ（成功〔own 行・own Private 行〕・RLS-9 対照〔他テナント可視
Public 行・他テナント Private 行・未存在 id〕・台帳照合〔同一内容再送
`23505`・内容不一致再送 `22023`〕・複数列 `SET` の宣言順パリティ・
`operation_id` 必須化 `23502`・未存在テーブル `42P01`）と、bob
（tenant-b）が実行する 1 ステップ（alice 所有の Private 行 id=11 への
`DELETE` が 0 行成功になることの RLS-9・RLS-11 対照）で構成する。
SQL 側・NoSQL 側それぞれに同じ手順を適用し、各ステップの影響行数
（`CommandComplete` タグの数値部分・`{"updated"/"deleted":n}` の `n`）
または `wire_code` が一致することを確認したうえで、両表層の最終状態
（`id`,`lang` の多重集合）が一致し、かつ手計算した固定オラクルとも
一致することを確認する（両表層が同じ誤りを返すケースの排除）。

**台帳のプロセス・表層横断永続**: DB-S での SQL 実行後にプロセスを
SIGKILL し、同じ DB ファイルで NoSQL 表層を起動して同一 `operation_id`
（`dml-u1`）を再送すると `23505`（同一内容）／`22023`（内容不一致）に
なること、逆方向（DB-N を NoSQL 表層で記録 → SIGKILL → 同じ DB ファイルで
SQL 表層を起動 → 再送）でも `23505` になることを確認した。台帳が
プロセス再起動・表層切替をまたいで永続することの非 vacuous な証跡になる
（層 A は同一プロセス内の 2 core 比較に留まるため、この永続性は本節が
固有に検証する）。

**RLS-9 応答同一性**: 他テナント（tenant-b）所有の Public 行 id=100 のみを
含む DB-F と、空の `docs` テーブルのみを持つ DB-M を用意し、同一
`operation_id` で id=100（DB-F）／id=999（DB-M）へ `UPDATE` を送ると、
SQL 側は `CommandComplete` タグが一致（`"UPDATE 0"`）、NoSQL 側は
ステータス・本文が完全一致することを確認した。

**実測結果**: 3 クライアント（curl／urllib／fetch）すべてで 14 ステップ・
bob ステップ・台帳の表層横断永続・RLS-9 応答同一性・最終状態一致の
いずれも green（本開発環境。`make e2e-three-client-http` の
`[e2e-record] dml-parity/<client>: ...` 行を参照）。実測で発見した意味差
は無かった。

**スコープ外**: 述語つき `UPDATE ... WHERE`／`DELETE ... WHERE`
（NoSQL `filter` は `0A000`／501 のまま未接続）・`RETURNING`・
UPSERT・複数行 `INSERT` は NoSQL 表層が公開していないためパリティが
成立せず対象外。ヘッダを含む HTTP 応答全体のバイト同一性は層 A
（`nosql12_update_delete.rs::strip_date` 比較）の担当で、本節はステータス・
本文までの一致に留める。

production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
無変更（テスト・docs 専任）。

### パリティ総合検証（Issue #950）

**目的**: Phase 7（親 #944）で NoSQL 表層に追加された機能——`filter` の
範囲比較・`IN`・`OR`（#945・NOSQL-14）、`sort`（#946・NOSQL-15）、
`offset`（#947・NOSQL-15）、`explain` 対象拡大（#948）、複数列
`group_by`（#949・NOSQL-16）、DDL 3 op（#1063・NOSQL-13）——は個別の
層 A テストで固定済みだが、実バイナリ・無改造クライアント越しに SQL 表層
と NoSQL 表層が同じ結果を返すことを横断的に固定する層 B 検証は #779
（読み取り 10 ケース）・#877（単一行 DML）の範囲に留まっていた。本 Issue
は既存 3 クライアントハーネスを拡張し、Phase 7 機能・insert・DDL 3 op を
含めた `Op::ALL`（9 語彙）すべてに層 B 到達性を持たせた。

**読み取りパリティの拡張**: `seed_parity_db` に `category`（`TEXT`）・
`price`（`NUMERIC(10,2)`）列を追加し（既存 10 ケースが参照する
`id`／`lang`／`embedding`・`expected_rows_*` は不変）、`PARITY_CASES` へ
6 ケース（`range-price-gte`・`in-category`・`or-lang-or-price`・
`sort-category-id`・`offset-sorted-id`・`group-by-lang-category`）を
追加した。`run_sql_nosql_parity_scenario`（#779 と共有）がそのまま
SQL↔NoSQL↔固定オラクルの三者一致を検証する。`price` はどのケースからも
投影列（`columns`）へは含めない——NoSQL `columns[].type` の期待値表
（同関数内 `expected_nosql_types`）が `id`／`embedding` 以外を一律
`"text"` とみなす簡易表のままのため、`NUMERIC` 列を投影すると型名比較が
誤って失敗する。**SQL 側の構文上の制約（実装時に判明）**: SQL `WHERE`
句で `NUMERIC` 列を裸の数値リテラルと比較すると
`udf_call::bind_expr`（式評価経路）が `NUMERIC` 列を未対応として拒否する
ため（`crates/wire-server/src/http/query/filter.rs` モジュール doc の
「`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`・範囲比較は
対象外」と同種の非対称）、`price >= '7.00'`（数値文字列リテラル）の形で
宣言的比較経路へ渡す必要がある——`engine/tests/scalar_types_predicates.rs`
の既存パターンに倣った。NoSQL 側の `filter` 値は数値・数値文字列いずれも
受理するため JSON 側は素の数値のままでよい。

**拒否ケース（§3.6 相当・スコープ縮小）**: `search`／`aggregate` への
`sort`／`offset`・語彙外 `op`（`begin`）・`filter` への RLS 述語名
（`visible`）指定が NoSQL 側で固定 `wire_code`（`42601`／`0A000`）を
返すことを alice 1 テナントで確認する（`REJECTION_CASES`）。psql 側の
受理確認（SQL 表層にはそもそも対応する構文形が無いか意味が異なる）は
本ファイルのスコープ外とした——`nosql-api.md`「対応の無いもの」節が
非対称の一次情報源であり、二重管理を避けた。

**insert／DDL／述語形 DML パリティ（§3.3〜§3.5 相当・スコープ縮小）**:
`run_phase7_write_parity_scenario` が、DDL 非依存の独立テーブル
（`widgets`）に対して create_table → 非許可ユーザーの drop_table 拒否
（`42501`）→ insert 2 行 → alter_table（`ADD COLUMN`）→ 述語形
`delete`（`filter: [{"eq"}]`）→ 読み戻し一致 → drop_table → post-drop
`42P01` を SQL 表層（生 wire）・NoSQL 表層（curl／urllib／fetch）の両方で
適用し、影響行数・`wire_code`・最終状態の一致を固定する。**計画時点からの
スコープ縮小**: RLS-9 応答同一性の述語形版（他テナント行向け／未存在 id
向けの同一 `operation_id` 応答一致）と 3 テナント横断の述語形 DML は
本関数では扱わない——単一行 `id` 完全一致形の RLS-9 は
`run_sql_nosql_dml_parity_scenario`（#877）が固定済みであり、述語形の
テナント境界パリティ自体は層 A（`nosql12_update_delete.rs` 述語形節）が
固定済みのため、本節は表層横断の insert／DDL 到達性確認に絞った。

**カバレッジガード**: `parity_matrix_covers_every_nosql_op`
（`#[ignore]` を付けない・常時 `make ci` で実行）が
`wire_server::http::query::op::Op::ALL`（9 語彙）のすべてが本ファイルの
いずれかの**成功系**層 B ケースで使われていることを確認する。判定は
`include_str!` によるソース全文の文字列出現検査ではなく、
`PARITY_CASES`／`DML_STEPS`／`BOB_STEP`／`PHASE7_WRITE_CASES`——各シナリオ
関数が実際に反復・送信するケース定義の値そのもの——から `op` 値を収集し、
その集合に対して `Op::ALL` の網羅を判定する方式に実装している（レビュー
指摘により、手動維持の固定文字列配列と `Op::ALL` を突き合わせるだけの
旧実装、およびその後のソース全文 grep 方式から変更。コメント等への
リテラル残存や実行されないコード上の文字列一致では検知できない抜けを
防ぐ）。`REJECTION_CASES`（拒否ケース）の `op` はこの網羅判定に加えない
——拒否ケースは非成功応答の `wire_code` のみを固定し、SQL/NoSQL 両表層の
成功パリティを検証していないため、拒否ケースにしか現れない `op` で
本ガードを見かけ上通過させない（PR #1135 レビュー指摘）。10 個目の
`Op` が追加され、対応する成功系ケースが本ファイルに存在しない場合は
このテストが赤くなり、層 B ケースの追加を強制する。

**実測結果**: 3 クライアント（curl／urllib／fetch）すべてで新規 6 読み取り
ケース・拒否 4 ケース・insert/DDL/述語形 delete シナリオが 3 テナント
（読み取り）・alice/bob（DDL）の両方で green（本開発環境。
`make e2e-three-client-http` の `[e2e-record] parity/<client>: ...`・
`[e2e-record] ddl-parity/<client>: ...` 行を参照）。既存 #776〜#779・#877
テストも回帰なし。実測で発見した意味差は無かった。

**ドキュメント追随**: `nosql-api.md`「`aggregate`」節の `group_by` が
「ちょうど 1 要素」と誤って記載されていた（#949 で複数列対応済み。実上限は
`engine::sql::allowlist::MAX_GROUP_BY_COLUMNS`）ため、実装に合わせて
1〜8 要素の記述へ更新した（レビュー指摘。PR #1135）。

**スコープ外**（PR 本文にも記載）: NoSQL 述語形 DML の
拡張語彙（範囲・`in`・`or`）非対応・`aggregate` への `sort`／`offset`・
`LIKE` 中間／後方一致・`create_index` 等の既知非対称の深掘り、`explain`
の SQL `EXPLAIN` 出力とのプラン行テキスト完全一致比較（層 A
`nosql16_explain_targets.rs`・`sql27_explain_targets.rs` がそれぞれの
表層内で固定済みであり、プランナ出力形式の差異を正規化する追加調査は
本 Issue のスコープ外とした）、DATE／TIMESTAMP／UUID／BYTEA 列での範囲
比較（`NUMERIC` のみを検証材料に採用）、層 B を CI 必須チェックへ組み
込むこと（既存 ADR の方針どおり別途ユーザー承認事項）。

production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
無変更（テスト・docs 専任）。

### Issue #878: wire セッションの可視性非対称と DML の相互作用（判断記録）

Phase 0（#972）の最終 Issue として、旧「スコープ外」項の可視性非対称を
RLS-11・TASK-195 の確定に合わせて棚卸しした判断記録。private spec
（`docs/spec/04-behavior/records/rdbms-parity-decision-2026-09-22.md`。
ポインタのみ・本文は転記しない）の詳細議論は転記せず、本リポの公開情報
（旧 doc・`auth.rs`／`simple_query.rs` のモジュールコメント・各テストの
doc コメント）の範囲で自分の言葉として整理する。

**1. 非対称の内容と影響範囲**

- 書き込み: engine `sql::exec::execute_insert` は行を常に
  `Visibility::Private` で書き込む固定仕様（NoSQL `insert` 写像も同一。
  `docs/design/nosql-insert-mapping.md:70`）。
- 読み取り（旧）: wire／HTTP 認証が導出する `PolicyContext`
  （`wire-server/src/auth.rs::session_policy_context`）は `Public` のみを
  許可可視性としていた。
- 影響 (a) 同一セッション内: `INSERT` 直後の `SELECT`（Dense・Hybrid・
  scan・aggregate のいずれも）で自分が書いた行が見えない。
- 影響 (b) テナント内の別セッション: 同一テナントの別接続からも見えない
  （セッション単位ではなくテナント単位の非対称）。
- 影響 (c) DML 経路の前提: Phase 1 以降の `UPDATE`／`DELETE`
  （SQL-17〔#864 で許可リスト・束縛、#865 で実行結線まで実装済み〕・
  SQL-18〔#975・#976 は許可リスト・束縛のみで実行結線は別 Issue の担当〕）・
  NoSQL `update`／`delete` 写像は、書いた行を読み戻し・
  対象として特定できることを前提にする。非対称を残したままでは
  「同一セッション内で `INSERT` した行を `UPDATE`／`DELETE` する」操作が
  組めず、現行動作の記述ではなく実行結線そのものの前提条件として問題に
  なる。

**2. 採用方針**

RLS-11・TASK-195（`docs/spec/04-behavior/*.md`。ポインタのみ）に従い、
wire 認証導出点（`auth::session_policy_context`）の許可可視性を
`Public` ＋ 自テナントの `Private` へ拡張した。テナント境界は
`engine::policy::PolicyContext::is_visible` のテナント一致判定が
引き続き担うため、他テナントの `Private` 行は拡張後も不可視のまま
（`session_policy_context` のドキュメンテーションコメント参照）。
可視性は **テナント単位であってセッション単位ではない**点に注意——
同一テナントの別セッションからも自分（同テナントの別ユーザーを含む）が
書いた `Private` 行が見える。拡張は wire 認証導出点
（`auth::session_policy_context`）に閉じ、engine 側
`crate::policy::PolicyContext::new`（既定 = `Public` のみ）は不変。
検討した選択肢・得失比較・却下理由の詳細は private spec 側の記録
（`docs/spec/04-behavior/records/rdbms-parity-decision-2026-09-22.md`。
ポインタのみ）を参照。

**3. 判断待ち事項**: なし（2026-09-22 オーナー判断で解消済み）。spec 側で
RLS-11・TASK-195 を新設し、RLS-7・RLS-9 は改訂注記付きで残置（RLS-11
確定より前の記述は従来契約が正、という関係を保つ）。

**4. ポインタ**: RLS-11・TASK-195・RLS-7・RLS-9・TASK-82・TASK-183
（`docs/spec/04-behavior/*.md`・`05-tasks.md`）。Issue #973（PR #977）・
Issue #974（PR #980）・親 Issue #972（Phase 0）・ルート Issue #860。
private spec 記録:
`docs/spec/04-behavior/records/rdbms-parity-decision-2026-09-22.md`
（ポインタのみ）。spec 側リビジョンは vector-db-spec #21・本リポの
submodule 追随は PR #951。

**5. 検証の所在**: `crates/engine/tests/rls11_read_your_writes.rs`
（`rls11_own_private_row_is_visible_in_same_and_other_session_of_same_tenant_after_cache_warm`・
`rls11_other_tenant_never_observes_private_row_across_all_read_shapes`・
`engine_default_policy_context_still_hides_own_private_rows`）・
`crates/wire-server/tests/rls11_read_your_writes.rs`（wire／HTTP／表層
横断 matrix）。層 B（`three_client_e2e.rs`・`extended_syntax_e2e.rs`・
`three_client_http_e2e.rs`）の更新箇所は上記「影響」節参照。

**6. 申し送り（本 Issue では編集しない）**:
`docs/design/scan-stage-profile.md`・
`visible-bitmap-cache-verification.md`・`knn-wire-stage-profile.md`・
`crossdb-bench.md`「可視性モデル」にある「wire セッションは Public
のみ可視」という記述は、いずれも**計測時点の条件記述**であり書き換えると
計測記録の意味が変わるため本 Issue では編集しない。RLS-11 下での再計測・
条件再記載は別 Issue 候補（PR #980 でも同旨を申し送り済み）。Issue
起票はオーナー承認事項のため自動運転では起票せず、対応 PR の「対象外」
節に記載する。`docs/design/nosql-insert-mapping.md:70`（書き込みは
`Private` 固定）は現在も正しい記述のため無変更。

## トランザクション状態遷移（Issue #943・WIRE-19）

`ReadyForQuery`（'Z'）の状態バイト（`'I'`／`'T'`／`'E'`。SQL-31・TASK-221・
WIRE-19。production の中核は Issue #942（PR #1041）で実装済み——
`docs/design/explicit-transaction.md`「`#943` との分担」節参照）が、
無改造の実クライアント 3 種から観測できることを
`three_client_e2e.rs::three_clients_observe_transaction_status_transitions`
（`#[ignore]`）として検証する。層 A（`wire19_ready_for_query_status.rs`・
`wire942_extended_transaction.rs`）が生バイトの wire クライアントで固定
する契約と同じものを、各ドライバ自身の API を通じて追加確認する。

**観測経路（実装前にツールのソースを確認して選定。推測で書かない）**:

- **psycopg**（`three_client/psycopg_txn_status.py`）: `psycopg.pq.
  TransactionStatus`（`IDLE`/`INTRANS`/`INERROR`）と `conn.info.
  transaction_status` が公開 API として存在する。`autocommit=False`
  （既定）で接続すると、psycopg 自身が（受信した `ReadyForQuery` の状態
  バイトから）この状態を追跡し、`IDLE` のときだけ次の文の前に暗黙の
  `BEGIN` を送る（libpq に autocommit の概念はなく、この判断・送出は
  psycopg 自身が行う）。本番の
  `ReadyForQuery` が常に `'I'` のまま（不具合を仮定した）だと、2 文目の
  前にも `BEGIN` が再送されて「入れ子の `BEGIN`」（`25001`）が観測される
  はずであり、これが本スクリプトの検出対象。
- **node pg**（`three_client/pg_txn_status.js`）: `pg` は psycopg のような
  公開の transaction status API を持たない。`pg.Client` が内部で保持する
  `Connection`（`pg-protocol` の `ReadyForQueryMessage` を emit する
  `EventEmitter`）の `readyForQuery` イベントを購読し、受信した状態バイト
  （`msg.status`。1 文字の文字列 `'I'`/`'T'`/`'E'`）をそのまま記録する。
  `pg`／`pg-protocol` のソース自体は変更しない（既存の public プロパティを
  読むだけ）。トランザクション制御自体は `BEGIN`/`COMMIT`/`ROLLBACK` を
  明示的に送る（psycopg と異なり pg は autocommit の自動切替を持たない）。
- **psql**: プロンプト文字列（`%x` → `=`/`*`/`!`）は対話端末専用の
  エスケープであり、非対話実行（`-c` の並び）では表示されないため
  観測できない。pty ラッパー（`script` コマンド）での対話プロンプト検証は
  flaky になりやすいため採らない。代わりに `\set AUTOCOMMIT off` の
  **挙動**で間接的に確認する: この設定下では psql が libpq の
  `PQtransactionStatus()`（`ReadyForQuery` の状態バイトから libpq が
  導出する）を見て、`IDLE` のときだけ暗黙の `BEGIN` を送る。
  `ReadyForQuery` が常に `'I'`
  のまま返る不具合があれば、`INSERT` の後の `SELECT` の前にも `BEGIN` が
  再送されて「入れ子の `BEGIN`」（`25001`）に倒れ、続く `COMMIT` も
  `25P02` で拒否されて非 0 終了する。正しく `'T'` を反映していれば
  `BEGIN` は 1 回だけ送られ、全体が正常終了する
  （`assert_psql_autocommit_off_reflects_transaction_status`）。

**engine 側の制約への対応**: 明示トランザクション内では「直前に同じ
トランザクションで書き込んだテーブル自身を読めない」制約
（`docs/design/explicit-transaction.md` 参照）があるため、seed
（`seed_txn_status_db`）は書き込み対象の `documents` と、
トランザクション内 `SELECT` 用の未書き込みテーブル `notes` を分けて
用意する。

**非 vacuous 性の確認**: 実装時に `encode_ready_for_query` を一時的に
常時 `'I'` を返すよう書き換え、層 A（`wire19_ready_for_query_status.rs`）
15 件中 8 件・層 B（3 クライアントいずれも）が失敗することを確認した
うえで元に戻した（コミットには含めない）。

**子プロセス stderr の読み続け契約（ハーネス不具合の是正）**: 本テストの
追加で `make e2e-three-client` の並列度が上がった結果、高負荷下（load
average 約 20）で既存テスト（集計・取得モード切替）が 1〜2 件ずつ psql の
"server closed the connection unexpectedly" で失敗する事象が出た。各テスト
は独立したサーバープロセス・一時 DB を使っており、新テストからの状態漏れ
ではない。原因は既存ハーネスの `spawn_wire_server`（`three_client_e2e.rs`・
`extended_syntax_e2e.rs`）で、listen 行の取得後に受信側チャネルが破棄される
と stderr 読み取りスレッドが終了してパイプの読み口を閉じていた点にある。
以後サーバーが `wire-server: connection error: Connection reset by peer`
等をログすると `EPIPE` で `eprintln!` が panic し、panic フック
（TASK-97・RECOVER-6／TASK-99・RECOVER-8）経由で SIGABRT 終了していた
（失敗時のサーバー終了状態 134 で確認。未読データを残した接続 close による
RST で決定的に再現する）。是正として読み取りスレッドは子プロセスの終了
（EOF）まで読み続け、`three_client_e2e.rs` は listen 後の行を直近 256 行まで
保持してテストが panic した場合に限り `[e2e-diag]` 行（サーバーの終了状態・
listen 後の stderr）を出力する。回帰テスト
`server_guard_keeps_draining_stderr_so_logged_connection_errors_do_not_abort_server`
（外部クライアント不要のため `#[ignore]` なし・`make ci` で常時実行）が、
接続エラーのログ後もサーバーが生存し新規接続へ認証要求を返すことを固定する。
production コードは変更していない（stderr の消費側が閉じた場合に
サーバーが abort する挙動の扱いは、本 Issue のスコープ外として別途判断する）。
**追記（Issue #1081）**: production コード側の扱いは Issue #1081 で決着した
（`engine::log_stderr!` へ一本化し、診断ログの書き込み失敗は abort させない。
判断の記録: `docs/design/stderr-log-write-failure.md`）。本ハーネスの読み続け
契約自体は失敗時診断のため変更なく維持する。

## TLS 上の 3 クライアント検証（Issue #969・WIRE-9）

TASK-228（WIRE-9・WIRE-1、親 #941）のうち、自作 TLS 1.3 スタック
（Issue #952〜#967・#970）の wire 接続への結線が完了した後段として、
「無改造の実クライアントが TLS 越しに C1〜C4・RLS 分離を完走できるか」
「TLS 1.2 のみの提示・改ざんレコード・切り詰めハンドシェイクが
fail-closed で切断されるか」を検証する（新設ファイル
`crates/wire-server/tests/wire9_tls.rs`）。

- **層 A（常時 `make ci`）**: 内製の TLS 1.3 最小クライアント
  （`tests/common/tls_client.rs`）で、TLS 越しの C1〜C4 一致・RLS 3 テナント
  分離（他テナント Private 行の混入 0 件）・`--tls-mode require`（既定）の
  平文拒否を回帰保護する。加えて、以下 7 種の負のテストがいずれも
  fail-closed で切断され、かつ同一サーバープロセスが後続の正規接続を
  完走できる（1 接続の失敗がプロセスを落とさない）ことを固定する。
  実測した alert の description（RFC 8446 の値）は次のとおり:
  - TLS 1.2 のみを提示する ClientHello（`supported_versions` 拡張なし／
    TLS 1.2 のみを含む拡張の 2 変種）→ `protocol_version`（70）
  - ハンドシェイク完了後の application data レコードを 1 バイト改ざん
    → `bad_record_mac`（20）
  - レコード長が `MAX_CIPHERTEXT_LEN`（2^14+256）を超えるヘッダのみ送信
    （本体は待たずに拒否）→ `record_overflow`（22）
  - ClientHello レコードの宣言長より短い本体で半クローズ／レコードヘッダ
    自体を切り詰めて半クローズ → `RecordError::Truncated`／`Io` は
    `alert_description()` が `None`（レコード層コメント参照）を返すため、
    サーバーは alert を一切送らず無応答のまま接続を閉じる（実測: 受信
    バイト列が空のまま EOF）ことを固定する
  - server flight 受信後・client Finished 送出前に半クローズ → 同じく
    `RecordError::Truncated` 経路のため、pg wire バイトはおろか暗号化
    alert すら一切届かずに無応答のまま切断される（実測: 受信バイト列は空）
- **層 B（`#[ignore]`。`make e2e-three-client-tls` から明示実行）**: 無改造の
  `psql`（`sslmode=require`）・Python `psycopg`（`sslmode=require`）・
  Node.js `pg`（`ssl: { rejectUnauthorized: false }`）が `--tls-mode require`
  越しに C1〜C4・RLS 分離を完走すること、`psql`
  `ssl_max_protocol_version=TLSv1.2` が TLS 1.3 専用サーバーに拒否される
  ことを検証する。`tests/three_client/{psycopg_client.py,pg_client.js}` に
  任意の `WIRE_SSLMODE`／`WIRE_SSL` 環境変数を追加した（未指定なら従来どおり
  TLS 指定を渡さない。`three_client_e2e.rs` はいずれの変数も渡さないため
  無改造のまま挙動不変）。

証明書・鍵は RFC 8032 §7.1 TEST 1 の公開テストベクタ
（`tls_client::RFC8032_TEST1_SEED`）からテスト実行時に一時ディレクトリへ
生成し、秘密鍵 PEM をリポジトリへは置かない（security.md P0）。

`three_client_e2e.rs`（平文の層 B）とはヘルパーを意図的に重複させている
（`tls_client.rs` 冒頭コメントと同じ方針）。共有モジュール化は本 Issue の
対象外（別 Issue 候補）。

## FOREIGN KEY 参照アクションの SQL/NoSQL パリティ（Issue #1148）

`run_phase7_write_parity_scenario`（DDL パリティ）と同じ 2 db 構成
（`seed_empty_db_no_table`・`spawn_{sql,nosql}_server_with_ddl_allowed`）で、
`run_fk_referential_action_parity_scenario` を新設した。`ON DELETE
CASCADE`・`ON UPDATE SET NULL` を宣言した `parents`／`children` を SQL 表層
（生 wire）・NoSQL 表層（`client`）それぞれ独立の DB へ作り、同型の
insert（4 行）→ update（`SET NULL` の連鎖）→ delete（`CASCADE` の連鎖）→
読み戻しに加え、拒否ケース 2 種（語彙外の参照アクション＝`42601`・`NOT
NULL` 列への `SET NULL`＝宣言時検査 `42830`）を適用し、影響行数・
`wire_code`・読み戻し行集合が一致することを固定する。3 クライアント
（curl／urllib／fetch）とも
`[e2e-record] fk-referential-action-parity/<client>: ...` 行で green を
確認済み（本開発環境）。`parity_matrix_covers_every_nosql_op` の網羅集合
（`PARITY_CASES`／`DML_STEPS`／`BOB_STEP`／`PHASE7_WRITE_CASES`）には
本シナリオの `create_table`／`insert`／`update`／`delete` は追加していない
（いずれも既存ケースで既に網羅済みのため）。

## SCRAM・COPY・複数行 INSERT の 3 クライアント検証（Issue #1177・WIRE-17・WIRE-18・SQL-16）

`three_client_e2e.rs` に次を追加した。クライアント用スクリプト
（`tests/three_client/*.py|js`）は変更していない（無改造接続の証跡）。

- **SCRAM-SHA-256（WIRE-18）**: 4 フィールド形式の users ファイルと
  `--scram-mock-key-file`（テスト専用ダミー秘密）で `--auth-method
  scram-sha-256` のサーバーを起動し、3 テナント × 3 クライアントで C1 が
  オラクルと一致すること、誤りパスワードが 3 クライアントで拒否されること、
  未知ユーザーの拒否文言が誤りパスワードと（ユーザー名を除き）同一であること、
  psql の `PGREQUIREAUTH=scram-sha-256` で接続でき `password` では拒否される
  こと（libpq が実際に SCRAM をネゴシエートした証跡）を確認する。非空性は
  常時実行テスト（`#[ignore]` なし）が AuthenticationSASL の機構一覧
  （`SCRAM-SHA-256` を含み TLS なしでは `-PLUS` を含まない）で担保する。
  SCRAM-SHA-256-PLUS（TLS・チャネルバインディング）は対象外
  （`wire_scram_plus_psql_interop.rs`・`tls-channel-binding.md` が担当）。
- **psql `\copy`（WIRE-17）**: text・csv の FROM、TO、別 DB への再投入と
  再 TO の多重集合一致、他テナントからの非可視（RLS）、`operation_id` 無しの
  `23502` 拒否を確認する。psql は `\copy` を `COPY  docs ( id, ... ) FROM
  STDIN <残りの入力そのまま>` のように再構成して送るため（二重空白・括弧内の
  空白・小文字のオプション語）、その形状を層 A（`wire17_copy.rs`）にも固定した。
  比較は `f32` の `Display` による表現差を避けるため、embedding を `f32` へ
  パースして行う。実測: psql 18 が生成する形は production の許可リストが
  そのまま受理した（許可リストの変更なし）。
- **複数行 INSERT と NoSQL `rows[]`（SQL-16）**: 同一の 3 行を、NoSQL
  `rows[]`（1 回）と 3 クライアントの複数行 `INSERT ... VALUES` へ投入し、
  読み戻し行集合が固定オラクルと相互に一致すること、bob からは見えないこと
  （書き込みは Private 固定）を確認する。NoSQL の HTTP 呼び出しは、
  `http_common` が `temp_db` を二重宣言し `clippy::duplicate_mod` となるため、
  ファイル内の最小の生 HTTP/1.1 ヘルパー（受信上限付き）で行う。

実行結果（本開発環境）: psql・psycopg・node `pg`（scratchpad へ一時導入し
`NODE_PATH` で参照。リポへは持ち込まない）の 3 クライアントとも green。

## 影響

- `crates/wire-server/src/{simple_query,result_encoder}.rs`（新規）・
  `handshake.rs`／`server.rs`／`main.rs`（拡張）により、簡易クエリが
  `engine::core::EngineCore` の SQL 表層へ到達する（TASK-73 本体）。
- `wire-server --db <path>` が必須化された（省略時は fail-closed で
  起動拒否。匿名・揮発 DB の暗黙生成はしない）。
- `Makefile` に `e2e-three-client`（opt-in・`ci` には含めない）を追加した。
- `Makefile` に `test-default-build`（`ci` に含む）・
  `.github/workflows/ci.yml` に同名の独立ジョブを追加し、既定ビルド拒否
  テストを常時検査する経路を整備した（Issue #715・#716・PR #718）。
- `crates/wire-server/Cargo.toml` に `fault-injection` feature（default 外）
  を追加し、`e2e-three-client` の `three_client_e2e` 行はこの feature 付きで
  ビルドするよう変更した（Issue #705）。
- `three_client_e2e.rs::spawn_wire_server` が `extra_args: &[String]` を
  受け取れるよう拡張され（`extended_syntax_e2e.rs` と同型）、`ServerGuard`
  が起動時 stderr の全行と `wait_for_exit` を保持するようになった。
  `tests/three_client/{psycopg_client.py,pg_client.js}` は失敗時に
  `[DETAIL=<detail>]` を stderr へ追記する（いずれも Issue #706）。
- RLS-11・TASK-195（Issue #973・PR #977）で wire 認証導出の
  `PolicyContext` が `Public` ＋ 自テナントの `Private` へ拡張されたことに
  伴い、層 B の seed／期待値（`three_client_e2e.rs`・
  `extended_syntax_e2e.rs`・`three_client_http_e2e.rs`）が「own Private
  行を含む」前提へ更新された（Issue #974・PR #980）。詳細は前述
  「Issue #878: wire セッションの可視性非対称と DML の相互作用
  （判断記録）」節参照。
- Issue #1177: `three_client_e2e.rs` に SCRAM・psql `\copy`・複数行 INSERT の
  層 B と常時実行の SASL 広告テストを、`wire17_copy.rs` に psql `\copy` 生成
  形状の層 A 固定テストを追加した（production コードの変更なし）。

## スコープ外

- `psql`・`psycopg`・`pg` の CI 自動導入ジョブ（バージョン確認・pin の確定は
  別途ユーザー承認を要する）
- Docker 開発コンテナへの `psql`／`psycopg` 追加
- **解消済み**: SQL `INSERT` が書き込む行の可視性（`Visibility::Private`
  固定）と wire 認証経由の `PolicyContext`（当時は `Public` のみ許可）の
  非対称。TASK-82（SQL-10）で `INSERT` 自体は wire 経由で受理するよう
  切り替えたが（旧: 当面 `INSERT` 自体を公開しない方針だった。
  codex-review P1・PR #210 指摘の検討過程の判断）、`Private` 許可を wire
  認証側へ広げる案は当時
  `wire1_three_tenant_visibility_public_shared_private_hidden`
  （自テナント自身の `Private` 行も含め wire 越しには不可視、という
  旧最小権限境界）を壊すため不採用とし、非対称（書いた
  本人も同一セッションでは読み戻せない）を残していた。オーナー判断
  （2026-09-22）により RLS-11・TASK-195 として read-your-writes を既定化
  し、Issue #973（PR #977）・Issue #974（PR #980）で実装・テスト更新済み
  （旧テストは
  `wire1_three_tenant_visibility_public_shared_own_private_visible`
  へ改名。契約固定テストは
  `wire1_insert_is_accepted_and_row_is_visible_over_wire_select_to_own_tenant`
  も参照）。詳細は前述「Issue #878: wire セッションの可視性非対称と
  DML の相互作用（判断記録）」節参照
- `EXPLAIN` 応答での実効モード・指定元の可視化（SQL-12 が SQL-6 と併せて
  期待する項目）: engine に `EXPLAIN` 自体が未実装のため対象外（SQL-6 の
  確定化で扱う）
- 拡張クエリプロトコル経由の `USING MODE $n`: WIRE-11（Issue #933）で
  Parse／Describe は受理する経路へ切り替わり、その後 Bind／Execute（#934）・
  `$n` パラメータ束縛（WIRE-12・#1171）も実装された。ただし `USING MODE $n` は
  `$n` の受理位置外のため引き続き `42601` で拒否され、MVP は簡易クエリの
  `42601` 拒否のみを検証する（層 B での `$n` 検証は後続）
