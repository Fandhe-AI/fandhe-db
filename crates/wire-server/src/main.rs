//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装を持つバイナリ層。
//!
//! 責務境界: クライアント接続の受け付け・wire プロトコルのパース/応答整形を担い、
//! クエリの実処理は `engine` クレート（コアロジック層）へ委譲する（TASK-73 で
//! 簡易クエリプロトコルを `engine::core::EngineCore` へ接続した）。
//!
//! CLI: `wire-server --users <path> --db <path> [--bind <addr:port>]
//! [--surface sql|nosql]
//! [--planner-endpoint <host:port> --planner-model <name>]
//! [--embedder-hashing-dim <N>]
//! [--search-engine default|hnsw|hnsw_f16|hnsw_i8]
//! [--hnsw-full-scan-ratio <num>/<den>]
//! [--hnsw-acorn-max-visible-ratio <num>/<den>]
//! [--hnsw-sparse-visited-max <N>]
//! [--auth-method cleartext|scram-sha-256] [--scram-mock-key-file <path>]
//! [--ddl-allowed-users <user1>[,<user2>...]]
//! [--tls-cert <pem> --tls-key <pem> [--tls-mode require|allow]
//!  [--tls-scram-channel-binding enable|disable]]
//! [--fault-inject post-commit-panic]`
//! （既定 bind: `127.0.0.1:5432`）。`--db` は必須（省略時は fail-closed で
//! 非 0 終了。匿名・揮発 DB の暗黙生成はしない。TASK-73・WIRE-1）。
//!
//! `--surface`（Issue #734・#735・TASK-171／HTTP-1・HTTP-9）: クエリ
//! インターフェースを SQL 表層（現行の PostgreSQL wire プロトコル）／NoSQL
//! 表層（HTTP/1.1 最小サブセット。TASK-172 以降）の 2 択で排他選択する
//! opt-in 注入点。未指定は `sql`（既定・現行経路のままビット同一）。値の
//! 解決は `surface::parse` に一本化し、不正な値・値欠落・2 回目以降の
//! 重複指定はいずれも fail-closed で起動エラー（既定へ黙って読み替えない）。
//! 両表層とも `GuardedBindAddrs::resolve`／`bind()` を共有した**後**に
//! accept ループだけを分岐する（HTTP-9: nosql 選択時も WIRE-7 と同じ bind
//! ガードを通る）。`sql` は `server::accept_loop_with_engine`、`nosql` は
//! `http::listener::accept_loop_with_router`（Issue #743・#747・#752。読み取り
//! 30 秒タイムアウト・同時接続数 64 の共有リミッターを SQL wire と同一契約で
//! 適用したうえで、接続ハンドラ本体〔`http::conn::handle_connection_with`〕
//! を呼ぶ。要求パース・panic 非伝播は Issue #747、ルーティング（`/v1/session`
//! を `http::session::issue::handle` へ、`/v1/session/close` を
//! `http::session::close::handle` へ、`/v1/query` を
//! `http::session::middleware::authenticate` 経由で `http::query::gate::
//! handle` へディスパッチ・他パスは `08P01`）は Issue #752・#753・#754・
//! #758 で実装済み（3 エンドポイント限定・未知パス／非 POST の網羅は
//! #758）。`/v1/query` の op 束縛・実行は Issue #759 以降が追記する）を
//! 呼ぶ。いずれも `match surface` の前に
//! 1 回だけ構築した同一の
//! `limits::ConnectionLimiter` インスタンスを受け取る。選ばれていない
//! 側のリスナーは構造的に bind されない（HTTP-1 の排他方針）。
//! `--fault-inject post-commit-panic`（Issue #705。feature `fault-injection`
//! 有効ビルド限定・**テスト専用**）: `INSERT` の commit 成功直後に自プロセスを
//! 1 回だけ panic させ、TASK-97・RECOVER-6 の緊急応答
//! （`C`=`XX000`・`D`=`state=may_be_committed`。ERR-5）と後続の abort を外部
//! クライアントから観測できるようにする。既定ビルド（feature 無効）では
//! このフラグ自体が存在せず `unknown argument: --fault-inject` で非 0 終了する
//! （fail-closed）。値欠落・不正値・重複指定も同様に起動エラー。詳細は
//! `wire_server::fault_injection` モジュールドキュメント・
//! `docs/design/three-client-e2e-harness.md`「Issue #705」節参照。
//!
//! `--search-engine`（Issue #656）: `--planner-endpoint` 等（TASK-117）と同型の
//! opt-in 注入点。未指定または `default` は現行どおり `EngineCore::open`
//! （既定＝ブルートフォース）をそのまま呼び、`hnsw`／`hnsw_f16`／`hnsw_i8` は
//! `EngineCore::open_with_engine`（Issue #402〜#413・#513・#520 系の HNSW opt-in
//! 経路。索引ノード常駐精度は f32／f16／I8）へ分岐する。値の解決は
//! `search_engine_opt::parse`／`to_engine_kind_with` に一本化し（untrusted な
//! CLI 文字列から `engine::search_engine::SearchEngineKind` へ到達する唯一の
//! 入口）、不正な値・値欠落・2 回目以降の重複指定はいずれも fail-closed で
//! 起動エラー（既定へ黙って読み替えない）。選択結果は `EXPLAIN` の
//! `engine:`／`hnsw_params:` 行（Issue #411）で確認できる。
//!
//! `--hnsw-full-scan-ratio`／`--hnsw-acorn-max-visible-ratio`／
//! `--hnsw-sparse-visited-max`（Issue #657。親 Issue #656「対象外」節で
//! 持ち越された探索パラメータの opt-in 露出）: `--search-engine` が `hnsw`／
//! `hnsw_f16`／`hnsw_i8` のいずれかのときのみ指定できる（`default`／未指定と
//! 同時指定・値欠落・形状不正（`<num>/<den>` 以外・非負整数以外）・意味不正
//! （分母 0・`num > den`・`acorn_max_visible_ratio < full_scan_ratio`）・
//! 重複指定はいずれも fail-closed で起動エラー。パースは
//! `search_engine_opt::parse_ratio`／`parse_sparse_visited_max`、意味検証は
//! `ValidatedHnswParams::with_full_scan_ratio`／`with_acorn_max_visible_ratio`
//! に一本化する）。既定値（`full_scan_ratio`=1/10・`acorn_max_visible_ratio`=
//! none・`sparse_visited_max`=0）は未指定時のまま不変（R2）。`m`／`ef_*` の CLI
//! 露出は引き続き対象外。`full_scan_ratio`／`acorn_max_visible_ratio` は
//! テナント存在情報に繋がるため `EXPLAIN` の `hnsw_params:` 行へは出さない
//! （Issue #411 の方針を維持。`sparse_visited_max=` は Issue #497 で既に
//! 露出済み）。
//!
//! `--durability`（Issue #850。親 Issue #849 が公開した
//! `engine::storage::WriteDurability`・`EngineCore::open_with_durability` へ
//! `--search-engine`（Issue #656）と同型の opt-in CLI から到達する）:
//! `immediate`／`none` の閉じた語彙のみを受理し、未指定は `immediate`
//! （既定・既存挙動とビット同一）のまま不変。不正な値・値欠落・2 回目以降の
//! 重複指定はいずれも fail-closed で起動エラー（既定へ黙って読み替えない）。
//! `none` を明示選択すると commit 成功応答は永続を保証しなくなる
//! （損失ウィンドウの詳細は `docs/design/ingest-write-path.md`「Issue #849
//! 追記」節参照）ため、`none` 選択時のみ起動ログへ英語の警告を 1 行出す
//! （`durability_opt::token_for` で診断用トークンへ変換）。値の解決は
//! `durability_opt::parse` に一本化し、`--search-engine` との組合せは
//! `open_engine_core` の 4 分岐（既定 durability × エンジン有無・非既定
//! durability × エンジン有無）で処理する（`EngineCore::from_storage` は
//! `search_engine_kind()` が構造的に `None` になり `EXPLAIN` の `engine:` 行が
//! divergent するため使わない。`open_engine_core` のドキュメント参照）。
//! `EXPLAIN` への durability 設定の露出は対象外。
//!
//! `--ddl-allowed-users`（Issue #902・SQL-23・TASK-203。`DROP TABLE` の DDL
//! 実行権限ゲート `engine::sql::ddl::require_ddl_permission` へ untrusted な
//! CLI 文字列から到達する唯一の入口）: カンマ区切りの username 列挙を
//! `--users` で読み込んだユーザーストアへ照合し、認証成功後の handshake が
//! それらの username に限り接続の `SessionState::allow_ddl` を呼ぶ（他の
//! opt-in と同じく起動後に変更できない構成値。値欠落・空要素・重複要素・
//! 未知 username・フラグの重複指定はいずれも fail-closed で起動エラー）。
//! 未指定は DDL 実行権限を持つユーザーが 0 人のまま（全 DDL 文が `42501`
//! で拒否される既定）。値の解決は `ddl_permission_opt::parse`・
//! `UserStore::with_ddl_allowed_users` に一本化する。
//!
//! `--tls-cert`／`--tls-key`／`--tls-mode`（Issue #967・親 #941・TASK-228。
//! WIRE-7, WIRE-9 ポインタ）: TLS opt-in の唯一の入口。`--tls-cert`（証明書
//! チェーン PEM）・`--tls-key`（Ed25519 PKCS#8 秘密鍵 PEM）は両方揃って
//! 初めて意味を持つ（`--planner-endpoint`／`--planner-model` と同じ設計。
//! 片方のみの指定は fail-closed で起動エラー）。読み込み・検証は
//! `wire_server::tls_opt::load_server_config`（`crate::tls::server_handshake::
//! TlsServerConfig`）に一本化し、鍵・証明書の内容・長さはエラーメッセージへ
//! 出さない。`--tls-mode`（`require`／`allow`。未指定時の既定は `require`。
//! 安全側）は `--tls-cert`／`--tls-key` を指定したときのみ意味を持ち、単独
//! 指定は組合せ不正として fail-closed 拒否する（`--hnsw-*` が
//! `--search-engine` を要求するのと同じ設計）。`--surface nosql` との併用も
//! 同じ意味で受理する（Issue #968。HTTP リスナーも先頭バイトで TLS／平文を
//! 判定し HTTPS として終端する。`wire_server::http::tls_transport` 参照。
//! `require` の下では平文 HTTP 接続へ一切応答せず切断する）。`bind_guard::
//! TransportSecurity` は TLS 未指定時 `Cleartext`、`require` 選択時
//! `TlsRequired`（非ループバック bind を許可。WIRE-9 を満たす）、`allow` 選択時
//! `TlsOptional`（`Cleartext` と同じくループバック限定。D1: `allow` は平文
//! 接続を受理する以上、非ループバックでは WIRE-9 を満たせないため警告のみで
//! 済ませず起動を拒否する。`docs/design/tls-wire-connection.md` 参照）。
//! TLS 有効時は起動ログへ `TLS enabled (mode=...)` の 1 行のみを出す
//! （鍵・証明書の内容は出さない）。
//!
//! `--tls-scram-channel-binding`（`enable`／`disable`。未指定時の既定は
//! `disable`。Issue #970・#1088・WIRE-9・WIRE-18 ポインタ）: SCRAM-SHA-256-
//! PLUS（`p=tls-server-end-point`）を機構リストへ提示するか
//! （`TlsServerConfig::with_scram_channel_binding`）を選ぶ CLI からの
//! 唯一の入口。`--tls-mode` と同じく `--tls-cert`／`--tls-key` を指定した
//! ときのみ意味を持ち、単独指定は組合せ不正として fail-closed 拒否する。
//! `enable` を選び、かつ葉証明書の署名アルゴリズムに RFC 5929 が定義する
//! ハッシュが無い（Ed25519 など。libpq の既定設定 `channel_binding=prefer`・
//! `channel_binding=require` が TLS 確立後の SCRAM 交換で `could not find
//! digest for NID UNDEF` により失敗しうる。実測結果は `docs/design/
//! tls-channel-binding.md` 参照）場合は、`tls_opt::
//! check_scram_channel_binding` により起動時に非 0 終了で拒否する
//! （黙って PLUS 非提示へ縮退させない。fail-closed）。RSA／ECDSA の CA が
//! 署名した葉証明書であれば `enable` は受理され、起動ログへ運用上の注意を
//! 1 行追加で出す（後述）。**この拒否判定は SQL 表層（`--surface sql`。
//! 既定）に限る**。NoSQL 表層は SASL 往復自体を持たないため、`enable` は
//! 葉証明書の署名アルゴリズムに関わらず無条件で no-op として受理する
//! （H5・Issue #968。Issue #1088 の実装が表層分岐より前にこの判定を
//! 実行しており nosql まで拒否してしまっていた回帰を、codex-review
//! PR #1089 P1 是正で表層限定へ修正した）。
//!
//! `wire-server hash-password` サブコマンドはユーザーストア（`username:tenant_id:phc`）
//! に登録する 1 行を生成する補助コマンド（stdin からパスワードを読み、平文を
//! ログ・引数に残さない）。
//!
//! `--planner-endpoint`／`--planner-model`（TASK-117・PLAN-9）: 両方指定時のみ
//! `engine::query_planner::OllamaClient` を構築して `EngineCore::with_query_planner`
//! へ注入する（未指定が既定＝`CoreError::QueryPlannerUnavailable` で `USING PLAN`
//! を fail-closed 拒否する現行契約を維持。片方だけの指定は起動時エラー）。
//! `--embedder-hashing-dim`（同 TASK-117）は `engine::embedding::HashingEmbedder`
//! （決定的・ネットワーク不要な**検証用参照実装**であり意味的埋め込みではない。
//! 同モジュールドキュメント参照）を opt-in 注入する。いずれも wire 経由での
//! `USING PLAN` 受け入れ検証（PLAN-9 確定化）のための注入点であり、実運用の
//! 埋め込み/プランナー接続先は別途の構成を要する。
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）・
//! TASK-69（対象ビヘイビア WIRE-5, WIRE-6）・TASK-70（対象ビヘイビア WIRE-7）・
//! TASK-99（対象ビヘイビア RECOVER-8。`engine::recovery::fail_fast::install` を
//! 起動時に結線し、panic を経路・スレッド問わずプロセス終了へ統一する）。
//! `--bind` は [`wire_server::bind_guard::GuardedBindAddrs::resolve`] により、通信路の
//! 保護状態（[`wire_server::bind_guard::TransportSecurity`]。TLS 未構成なら
//! `Cleartext`、`--tls-mode require` なら `TlsRequired`、`allow` なら `TlsOptional`。
//! Issue #967）に応じて非ループバックアドレスを起動時に fail-closed で拒否したうえで、
//! 検証済みの数値アドレスへ直接 bind する（TLS 未構成・`allow` 選択時は平文パスワードを
//! 非ループバックへ公開しない。ホスト名の再解決による TOCTOU も作らない。TASK-67
//! review 是正・TASK-70 で移設）。同時接続数上限・認証前後の読み取りタイムアウトは
//! [`wire_server::limits`] の契約値を [`wire_server::server::accept_loop_with_limiter`]／
//! [`wire_server::server::accept_loop_with_tls_mode`] が適用する（TASK-69）。

use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use wire_server::auth::{self, UserStore};
use wire_server::bind_guard::{GuardedBindAddrs, TransportSecurity};
use wire_server::limits;
use wire_server::server;

const DEFAULT_BIND: &str = "127.0.0.1:5432";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("hash-password") {
        return run_hash_password(&args[2..]);
    }

    run_server(&args)
}

/// `wire-server --users <path> --db <path> [--bind <addr:port>]`。
fn run_server(args: &[String]) -> ExitCode {
    // TASK-97（対象ビヘイビア: RECOVER-6・ERR-1）: commit 成功境界を跨いだ panic の
    // 観測可能性側（緊急応答の送出）を有効化する。プロセス全体で 1 回だけ導入し
    // （`engine::recovery::panic_hook::install_panic_hook` は `Once` で冪等）、
    // 起動処理の他のどの失敗経路よりも前に呼ぶことで、後続の初期化中に commit を
    // 伴う処理が万一走っても保護対象から漏れないようにする。engine のライブラリ
    // 初期化（`EngineCore::open` 等）からは呼ばない契約（`panic_hook` モジュール
    // ドキュメント参照。engine 単体のテスト・他バイナリの panic 挙動を変えない）。
    engine::recovery::panic_hook::install_panic_hook();
    // TASK-99（対象ビヘイビア: RECOVER-8）: 内部エラーの 2 系統統一のうち panic 側
    // ―― 経路・スレッドを問わない fail-fast ―― を有効化する。`panic_hook` の
    // **直後**に呼ぶ契約（`engine::recovery::fail_fast` モジュールドキュメント
    // 「導入順序」参照）: `std::panic::set_hook` は 1 プロセスに 1 フックしか
    // 保持できないため、`fail_fast::install` は自分がフックへ差し替わる際に
    // 捕捉した直前のフック（＝ここまでに導入済みの `panic_hook`）を必ず先に
    // 呼んでから abort する。この順序を逆にする（`fail_fast` を先に呼ぶ）と
    // `panic_hook` が緊急応答を送る前段が失われ、TASK-97・RECOVER-6 の緊急応答が
    // 退行する。
    engine::recovery::fail_fast::install();

    let mut users_path: Option<PathBuf> = None;
    let mut db_path: Option<PathBuf> = None;
    let mut bind_addr = DEFAULT_BIND.to_string();
    let mut surface_raw: Option<String> = None;
    let mut planner_endpoint: Option<String> = None;
    let mut planner_model: Option<String> = None;
    let mut embedder_hashing_dim: Option<String> = None;
    let mut search_engine_raw: Option<String> = None;
    let mut full_scan_ratio_raw: Option<String> = None;
    let mut acorn_max_visible_ratio_raw: Option<String> = None;
    let mut sparse_visited_max_raw: Option<String> = None;
    let mut durability_raw: Option<String> = None;
    let mut ddl_allowed_users_raw: Option<String> = None;
    let mut auth_method_raw: Option<String> = None;
    let mut scram_mock_key_file_raw: Option<PathBuf> = None;
    let mut tls_cert_raw: Option<PathBuf> = None;
    let mut tls_key_raw: Option<PathBuf> = None;
    let mut tls_mode_raw: Option<String> = None;
    let mut tls_scram_channel_binding_raw: Option<String> = None;
    // Issue #705（テスト専用・feature `fault-injection` 限定）。feature 無効
    // ビルドではこの変数自体が存在せず、`--fault-inject` は下記 `other =>`
    // 分岐で未知引数として拒否される。
    #[cfg(feature = "fault-injection")]
    let mut fault_inject_raw: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--users" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --users requires a path argument");
                    return ExitCode::FAILURE;
                };
                users_path = Some(PathBuf::from(v));
                i += 2;
            }
            "--db" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --db requires a path argument");
                    return ExitCode::FAILURE;
                };
                db_path = Some(PathBuf::from(v));
                i += 2;
            }
            "--bind" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --bind requires an address argument");
                    return ExitCode::FAILURE;
                };
                bind_addr = v.clone();
                i += 2;
            }
            "--planner-endpoint" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --planner-endpoint requires a host:port argument");
                    return ExitCode::FAILURE;
                };
                planner_endpoint = Some(v.clone());
                i += 2;
            }
            "--planner-model" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --planner-model requires a name argument");
                    return ExitCode::FAILURE;
                };
                planner_model = Some(v.clone());
                i += 2;
            }
            "--embedder-hashing-dim" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --embedder-hashing-dim requires a numeric argument");
                    return ExitCode::FAILURE;
                };
                embedder_hashing_dim = Some(v.clone());
                i += 2;
            }
            wire_server::surface::FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires one of {:?}",
                        wire_server::surface::FLAG,
                        wire_server::surface::TOKENS
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #734: 起動後に変更できない構成値のため、
                // `--search-engine`（D6）と同じ理由で 2 回目以降の指定を
                // fail-closed に拒否する（last-wins にしない）。
                if surface_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::surface::FLAG
                    );
                    return ExitCode::FAILURE;
                }
                surface_raw = Some(v.clone());
                i += 2;
            }
            "--search-engine" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: --search-engine requires one of {:?}",
                        wire_server::search_engine_opt::TOKENS
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #656 D6: 起動後に変更できない構成値のため、typo・
                // スクリプトの二重指定で意図しないエンジンが黙って選ばれる
                // 事故を防ぐ目的で 2 回目以降の指定を fail-closed に拒否する
                // （他フラグの last-wins とは意図的に方針を変える）。
                if search_engine_raw.is_some() {
                    eprintln!("wire-server: --search-engine specified more than once");
                    return ExitCode::FAILURE;
                }
                search_engine_raw = Some(v.clone());
                i += 2;
            }
            wire_server::search_engine_opt::FULL_SCAN_RATIO_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a <num>/<den> argument",
                        wire_server::search_engine_opt::FULL_SCAN_RATIO_FLAG
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #657 D2: `--search-engine` の重複指定拒否（D6 注釈参照）
                // と同じ理由で、起動後に変更できない構成値の 2 回目以降の
                // 指定を fail-closed に拒否する（last-wins にしない）。
                if full_scan_ratio_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::search_engine_opt::FULL_SCAN_RATIO_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                full_scan_ratio_raw = Some(v.clone());
                i += 2;
            }
            wire_server::search_engine_opt::ACORN_MAX_VISIBLE_RATIO_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a <num>/<den> argument",
                        wire_server::search_engine_opt::ACORN_MAX_VISIBLE_RATIO_FLAG
                    );
                    return ExitCode::FAILURE;
                };
                if acorn_max_visible_ratio_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::search_engine_opt::ACORN_MAX_VISIBLE_RATIO_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                acorn_max_visible_ratio_raw = Some(v.clone());
                i += 2;
            }
            wire_server::search_engine_opt::SPARSE_VISITED_MAX_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a non-negative integer argument",
                        wire_server::search_engine_opt::SPARSE_VISITED_MAX_FLAG
                    );
                    return ExitCode::FAILURE;
                };
                if sparse_visited_max_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::search_engine_opt::SPARSE_VISITED_MAX_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                sparse_visited_max_raw = Some(v.clone());
                i += 2;
            }
            wire_server::durability_opt::FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires one of {:?}",
                        wire_server::durability_opt::FLAG,
                        wire_server::durability_opt::TOKENS
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #850: 起動後に変更できない構成値のため、
                // `--search-engine`（D6）と同じ理由で 2 回目以降の指定を
                // fail-closed に拒否する（last-wins にしない）。
                if durability_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::durability_opt::FLAG
                    );
                    return ExitCode::FAILURE;
                }
                durability_raw = Some(v.clone());
                i += 2;
            }
            wire_server::ddl_permission_opt::FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a comma-separated list of usernames",
                        wire_server::ddl_permission_opt::FLAG
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #902: 起動後に変更できない構成値のため、他の閉じた
                // 語彙フラグ（`--search-engine` 等）と同じ理由で 2 回目以降の
                // 指定を fail-closed に拒否する（last-wins にしない）。
                if ddl_allowed_users_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::ddl_permission_opt::FLAG
                    );
                    return ExitCode::FAILURE;
                }
                ddl_allowed_users_raw = Some(v.clone());
                i += 2;
            }
            wire_server::auth_method_opt::FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires one of {:?}",
                        wire_server::auth_method_opt::FLAG,
                        wire_server::auth_method_opt::TOKENS
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #940: 起動後に変更できない構成値のため、他の閉じた
                // 語彙フラグ（`--search-engine` 等）と同じ理由で 2 回目以降の
                // 指定を fail-closed に拒否する（last-wins にしない）。
                if auth_method_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::auth_method_opt::FLAG
                    );
                    return ExitCode::FAILURE;
                }
                auth_method_raw = Some(v.clone());
                i += 2;
            }
            wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a path argument",
                        wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #940 P0 是正: 起動後に変更できない構成値のため、他の
                // 閉じた語彙フラグと同じ理由で 2 回目以降の指定を fail-closed
                // に拒否する（last-wins にしない）。
                if scram_mock_key_file_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                scram_mock_key_file_raw = Some(PathBuf::from(v));
                i += 2;
            }
            wire_server::tls_opt::CERT_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a path argument",
                        wire_server::tls_opt::CERT_FLAG
                    );
                    return ExitCode::FAILURE;
                };
                // Issue #967: 起動後に変更できない構成値のため、他の閉じた
                // 語彙フラグ・パス引数フラグ（`--scram-mock-key-file` 等）と
                // 同じ理由で 2 回目以降の指定を fail-closed に拒否する
                // （last-wins にしない）。
                if tls_cert_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::tls_opt::CERT_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                tls_cert_raw = Some(PathBuf::from(v));
                i += 2;
            }
            wire_server::tls_opt::KEY_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires a path argument",
                        wire_server::tls_opt::KEY_FLAG
                    );
                    return ExitCode::FAILURE;
                };
                if tls_key_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::tls_opt::KEY_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                tls_key_raw = Some(PathBuf::from(v));
                i += 2;
            }
            wire_server::tls_opt::MODE_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires one of {:?}",
                        wire_server::tls_opt::MODE_FLAG,
                        wire_server::tls_opt::MODE_TOKENS
                    );
                    return ExitCode::FAILURE;
                };
                if tls_mode_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::tls_opt::MODE_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                tls_mode_raw = Some(v.clone());
                i += 2;
            }
            wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires one of {:?}",
                        wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG,
                        wire_server::tls_opt::SCRAM_CHANNEL_BINDING_TOKENS
                    );
                    return ExitCode::FAILURE;
                };
                if tls_scram_channel_binding_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG
                    );
                    return ExitCode::FAILURE;
                }
                tls_scram_channel_binding_raw = Some(v.clone());
                i += 2;
            }
            // Issue #705（テスト専用・feature `fault-injection` 限定）。feature
            // 無効ビルドではこのアームごとコンパイルされず、`--fault-inject`
            // は下の `other =>` で未知引数として拒否される（fail-closed）。
            #[cfg(feature = "fault-injection")]
            wire_server::fault_injection::FLAG => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!(
                        "wire-server: {} requires one of [{:?}]",
                        wire_server::fault_injection::FLAG,
                        wire_server::fault_injection::POST_COMMIT_PANIC_TOKEN
                    );
                    return ExitCode::FAILURE;
                };
                // 他の起動後変更不能な構成値（`--search-engine` 等）と同じ理由で
                // last-wins にせず 2 回目以降の指定を fail-closed に拒否する。
                if fault_inject_raw.is_some() {
                    eprintln!(
                        "wire-server: {} specified more than once",
                        wire_server::fault_injection::FLAG
                    );
                    return ExitCode::FAILURE;
                }
                fault_inject_raw = Some(v.clone());
                i += 2;
            }
            other => {
                eprintln!("wire-server: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    // TASK-117（PLAN-9）: `--planner-endpoint`／`--planner-model` は両方揃って
    // 初めて `OllamaClient` を構築できる契約（片方のみは操作ミスの検出漏れを防ぐ
    // ため fail-closed で起動を拒否する。未接続の既定＝`QueryPlannerUnavailable`
    // 拒否を静かに維持したまま片方だけ設定漏れした状態を作らせない）。
    let query_planner = match (planner_endpoint.as_deref(), planner_model.as_deref()) {
        (None, None) => None,
        (Some(_), None) => {
            eprintln!("wire-server: --planner-endpoint requires --planner-model to also be set");
            return ExitCode::FAILURE;
        }
        (None, Some(_)) => {
            eprintln!("wire-server: --planner-model requires --planner-endpoint to also be set");
            return ExitCode::FAILURE;
        }
        (Some(endpoint), Some(model)) => match build_query_planner(endpoint, model) {
            Ok(client) => Some(client),
            Err(e) => {
                eprintln!("wire-server: invalid --planner-endpoint/--planner-model: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let embedder = match embedder_hashing_dim.as_deref() {
        None => None,
        Some(raw_dim) => match build_hashing_embedder(raw_dim) {
            Ok(embedder) => Some(embedder),
            Err(e) => {
                eprintln!("wire-server: invalid --embedder-hashing-dim: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    // Issue #656: 未指定は `resolve_search_engine(None)` が `Ok(None)` を返し、
    // 既存の `EngineCore::open` 経路（下記）をそのまま通す。不正な語彙・
    // `ValidatedHnswParams` 検証失敗はいずれもここで起動エラーとして確定させる
    // （fail-closed。bind・ユーザーストア読込より前に決着させることで、受理
    // 不能な構成のまま listen へ進む経路を作らない）。
    let hnsw_tuning_raw = RawHnswTuning {
        full_scan_ratio: full_scan_ratio_raw.as_deref(),
        acorn_max_visible_ratio: acorn_max_visible_ratio_raw.as_deref(),
        sparse_visited_max: sparse_visited_max_raw.as_deref(),
    };
    let search_engine_kind =
        match resolve_search_engine(search_engine_raw.as_deref(), &hnsw_tuning_raw) {
            Ok(kind) => kind,
            Err(e) => {
                eprintln!("wire-server: invalid search engine configuration: {e}");
                return ExitCode::FAILURE;
            }
        };

    // Issue #850: `--search-engine` と同じく bind・ユーザーストア読込より前に
    // 決着させる（fail-closed。受理不能な構成のまま listen へ進む経路を
    // 作らない）。未指定は `resolve_durability(None)` が既定値
    // （`WriteDurability::Immediate`）を返し、後段の `open_engine_core` が
    // 既存の `EngineCore::open`／`open_with_engine` 経路とビット同一の分岐を
    // 通る。
    let durability = match resolve_durability(durability_raw.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "wire-server: invalid {}: {e}",
                wire_server::durability_opt::FLAG
            );
            return ExitCode::FAILURE;
        }
    };

    // Issue #705（テスト専用・feature `fault-injection` 限定）: `--search-engine`
    // と同じく bind・ユーザーストア読込より前に確定させる（fail-closed。
    // 不正な構成のまま listen へ進む経路を作らない）。`arm` 自体は listen
    // 直前（`guarded.bind()` 成功後）まで遅延する。
    #[cfg(feature = "fault-injection")]
    let fault_kind = match fault_inject_raw.as_deref() {
        None => None,
        Some(raw) => match wire_server::fault_injection::parse(raw) {
            Ok(kind) => Some(kind),
            Err(e) => {
                eprintln!(
                    "wire-server: invalid {}: {e}",
                    wire_server::fault_injection::FLAG
                );
                return ExitCode::FAILURE;
            }
        },
    };

    // Issue #734: `--search-engine` と同じく bind・ユーザーストア読込より前に
    // 決着させる（fail-closed。受理不能な構成のまま listen へ進む経路を
    // 作らない）。
    let surface = match resolve_surface(surface_raw.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: invalid {}: {e}", wire_server::surface::FLAG);
            return ExitCode::FAILURE;
        }
    };

    // Issue #940: `--search-engine`／`--durability` と同じく bind・ユーザー
    // ストア読込より前に決着させる（fail-closed。受理不能な構成のまま
    // listen へ進む経路を作らない）。未指定は `resolve_auth_method(None)` が
    // 既定値（`AuthMethod::Cleartext`）を返し、既存の cleartext フローと
    // ビット同一のまま不変。
    let auth_method = match resolve_auth_method(auth_method_raw.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "wire-server: invalid {}: {e}",
                wire_server::auth_method_opt::FLAG
            );
            return ExitCode::FAILURE;
        }
    };
    // NoSQL 表層（`POST /v1/session`。TASK-174・HTTP-6）は `auth::verify`
    // （Argon2id）を直接呼ぶ別経路であり、SASL のような往復を持たない
    // （HTTP-10 が別方式の新設を禁じているため対応しない）。`scram-sha-256`
    // モードでは SQL 表層の cleartext PasswordMessage 自体を受け付けなくなる
    // ため、この組合せを起動時に fail-closed で拒否する。
    if auth_method == wire_server::auth::AuthMethod::ScramSha256
        && surface == wire_server::surface::Surface::Nosql
    {
        eprintln!(
            "wire-server: {} scram-sha-256 cannot be combined with {} nosql (NoSQL surface has no SASL flow; see HTTP-10)",
            wire_server::auth_method_opt::FLAG,
            wire_server::surface::FLAG
        );
        return ExitCode::FAILURE;
    }

    // Issue #940 P0 是正: `--scram-mock-key-file` は `scram-sha-256` 選択時
    // のみ意味を持つ注入点であり、他フラグ（`--hnsw-*` が `--search-engine`
    // を要求するのと同じ設計）と同様に組合せ不正を fail-closed で拒否する。
    match (
        auth_method == wire_server::auth::AuthMethod::ScramSha256,
        scram_mock_key_file_raw.is_some(),
    ) {
        (true, false) => {
            eprintln!(
                "wire-server: {} <path> is required when {} scram-sha-256 is selected \
                 (fail-closed: a user-store-independent secret prevents the mock salt for \
                 unknown users from acting as a user-existence oracle across store updates)",
                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG,
                wire_server::auth_method_opt::FLAG
            );
            return ExitCode::FAILURE;
        }
        (false, true) => {
            eprintln!(
                "wire-server: {} requires {} scram-sha-256",
                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG,
                wire_server::auth_method_opt::FLAG
            );
            return ExitCode::FAILURE;
        }
        _ => {}
    }

    // Issue #967: `--tls-cert`／`--tls-key`／`--tls-mode` の組合せ検証は
    // `resolve_tls_options` に一本化する（`resolve_search_engine` と同じ
    // 「純関数へ切り出して単体テストできるようにする」流儀）。片方のみの
    // 指定・`--tls-mode` 単独指定・不正な語彙値はいずれも fail-closed で
    // 起動エラー（D3・D4）。
    let tls_options = match resolve_tls_options(
        tls_cert_raw.as_deref(),
        tls_key_raw.as_deref(),
        tls_mode_raw.as_deref(),
        tls_scram_channel_binding_raw.as_deref(),
    ) {
        Ok(opt) => opt,
        Err(e) => {
            eprintln!("wire-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    // 旧 D5（NoSQL 表層は TLS 未結線として起動を拒否）は Issue #968 で置き換え
    // 済み: `--surface nosql` も `--tls-cert`／`--tls-key`／`--tls-mode` を
    // SQL 表層と同じ意味で受理し、HTTP リスナーを HTTPS として終端する
    // （`http::listener::accept_loop_with_router_tls`。詳細は
    // `docs/design/tls-wire-connection.md`「HTTPS 表層（#968）」節）。
    // `--auth-method scram-sha-256 × nosql`（SASL 往復を持たないため別方式の
    // 新設を禁じる HTTP-10 により対応しない）とは独立の判断であり、上の
    // 拒否は変更しない。

    let Some(users_path) = users_path else {
        eprintln!("wire-server: --users <path> is required (fail-closed: no anonymous login)");
        return ExitCode::FAILURE;
    };
    let Some(db_path) = db_path else {
        eprintln!(
            "wire-server: --db <path> is required (fail-closed: no implicit anonymous/volatile database)"
        );
        return ExitCode::FAILURE;
    };

    // Issue #967: 証明書・鍵の読み込みは `--users`／`--db` 必須チェックの後・
    // `GuardedBindAddrs::resolve` の前に行う（受理不能な TLS 構成のまま
    // bind 検証・listen へ進む経路を作らない。fail-closed）。読み込み・検証
    // 失敗時のエラーメッセージは `TlsConfigLoadError` の内容非依存な
    // `Display` に委譲するため、鍵・証明書のバイト列・長さは出力されない。
    let tls_config = match &tls_options {
        None => None,
        Some((cert, key, _mode, scram_channel_binding)) => {
            match wire_server::tls_opt::load_server_config_arc_with_options(
                cert,
                key,
                *scram_channel_binding,
            ) {
                Ok(cfg) => {
                    // Issue #1088: 読み込み・証明書/鍵の整合性検査を通った
                    // 構成でも、`enable` と RFC 5929 が定義するハッシュを
                    // 持たない署名アルゴリズムの葉証明書（Ed25519 など）の
                    // 組合せは libpq と相互運用できないため、bind 検証・
                    // listen へ進む前にここで拒否する（fail-closed）。
                    // ただし本チェックは SQL 表層の SASL（SCRAM-SHA-256-PLUS）
                    // 提示可否に対する落とし穴の是正であり、NoSQL 表層は
                    // SASL 往復自体を持たないため `enable` は無条件で no-op
                    // として受理する契約（H5・Issue #968。
                    // docs/design/implementation-status.md「NoSQL 表層…H5」）。
                    // ここで surface を分岐しないと、Ed25519 葉証明書を使う
                    // 既存の nosql 構成が `enable` 指定だけで起動不能になり、
                    // H5 の契約を破る（codex-review PR #1089 P1 是正）。
                    // fail-closed の既定は「チェックを実行する」側とし、
                    // SASL 往復を持たないと分かっている nosql の場合のみ
                    // 明示的に除外する（`!= Nosql` であって `== Sql` ではない。
                    // 将来 `Surface` に variant が増えても黙って未検査へ
                    // 倒さないため）。
                    if surface != wire_server::surface::Surface::Nosql {
                        if let Err(e) = wire_server::tls_opt::check_scram_channel_binding(
                            &cfg,
                            *scram_channel_binding,
                        ) {
                            eprintln!("wire-server: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                    Some(cfg)
                }
                Err(e) => {
                    eprintln!("wire-server: failed to load TLS configuration: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    // 通信路の保護状態は TLS opt-in の有無・`--tls-mode` によって決まる
    // （Issue #967。TLS 未指定は従来どおり `Cleartext`）。bind の検証を
    // ユーザーストア読込より前に行うことで、ユーザーストアの内容に関わらず
    // bind 先が拒否対象であれば即座に終了できる（fail-closed を早期に
    // 確定させる）。
    let transport_security = match tls_options.as_ref().map(|(_, _, mode, _)| *mode) {
        None => TransportSecurity::Cleartext,
        Some(wire_server::tls_opt::TlsMode::Require) => TransportSecurity::TlsRequired,
        Some(wire_server::tls_opt::TlsMode::Allow) => TransportSecurity::TlsOptional,
        // `TlsMode` は `#[non_exhaustive]`（将来 variant 追加時に下流の
        // exhaustive match を破壊しないため）。現時点で他 variant は存在
        // せず構造的に到達しないが、fail-closed に起動拒否へ倒す
        // （黙って `Cleartext` 等へ読み替えない）。
        Some(_) => {
            eprintln!("wire-server: unsupported TLS mode");
            return ExitCode::FAILURE;
        }
    };
    let guarded = match GuardedBindAddrs::resolve(&bind_addr, transport_security) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("wire-server: {e}");
            // D1: `--tls-mode allow` を選んだ場合のみ、`require` へ切り替える
            // ことで非ループバック bind が受理されうる旨を補足する（`allow`
            // は平文接続を受理する以上、通信路保護の観点で `require` へ
            // 切り替えない限り非ループバックへは出せないため）。
            if transport_security == TransportSecurity::TlsOptional {
                eprintln!(
                    "wire-server: hint: {} allow accepts plaintext connections; use {} require \
                     for non-loopback binds (WIRE-9)",
                    wire_server::tls_opt::MODE_FLAG,
                    wire_server::tls_opt::MODE_FLAG
                );
            }
            return ExitCode::FAILURE;
        }
    };

    let store = match UserStore::load_from_file(&users_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: failed to load user store: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Issue #940: `scram-sha-256` モードでは全レコードが SCRAM 検証子を持つ
    // ことを起動時に要求する（fail-closed。検証子を欠くレコードは Argon2id
    // 照合が使えず恒久的にログイン不能になるだけでなく、モック相当の扱いに
    // なりタイミング対称性が崩れるのを未然に防ぐ）。
    //
    // Issue #940 P0 是正: モック鍵導出用の秘密は `--scram-mock-key-file` から
    // 読む（ユーザーストアの内容から独立させるため。上の組合せ検証により
    // `ScramSha256` のときは必ず `Some` が入っている）。ファイルが短すぎる
    // 場合はエントロピー不足として fail-closed に拒否する。
    let store = if auth_method == wire_server::auth::AuthMethod::ScramSha256 {
        let Some(mock_key_path) = scram_mock_key_file_raw.as_deref() else {
            eprintln!(
                "wire-server: {} <path> is required when {} scram-sha-256 is selected",
                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG,
                wire_server::auth_method_opt::FLAG
            );
            return ExitCode::FAILURE;
        };
        // PR #1006 P1 是正: ファイルサイズを検証してから読む（fail-closed）。
        // メタデータで通常ファイルであることを確認したうえで、
        // `Read::take` により `SCRAM_MOCK_KEY_FILE_MAX_LEN + 1` バイトまで
        // しか読まない。メタデータの `len()` は `/dev/zero` のような
        // 特殊ファイルでは信用できないため、事前チェックに加えて
        // 読み込み自体も固定上限で打ち切る二重の防御とする。
        let mock_key_secret = match std::fs::metadata(mock_key_path) {
            Ok(meta) if !meta.is_file() => {
                eprintln!(
                    "wire-server: {} {mock_key_path:?} is not a regular file",
                    wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                );
                return ExitCode::FAILURE;
            }
            Ok(_) => match std::fs::File::open(mock_key_path) {
                Ok(file) => {
                    let max_len = wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_MAX_LEN;
                    let mut buf = Vec::new();
                    match file
                        .take((max_len as u64).saturating_add(1))
                        .read_to_end(&mut buf)
                    {
                        Ok(_) if buf.len() > max_len => {
                            eprintln!(
                                "wire-server: {} {mock_key_path:?} exceeds the maximum allowed size ({max_len} bytes)",
                                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                            );
                            return ExitCode::FAILURE;
                        }
                        Ok(_) => buf,
                        Err(e) => {
                            eprintln!(
                                "wire-server: failed to read {} {mock_key_path:?}: {e}",
                                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                            );
                            return ExitCode::FAILURE;
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "wire-server: failed to read {} {mock_key_path:?}: {e}",
                        wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                    );
                    return ExitCode::FAILURE;
                }
            },
            Err(e) => {
                eprintln!(
                    "wire-server: failed to read {} {mock_key_path:?}: {e}",
                    wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG
                );
                return ExitCode::FAILURE;
            }
        };
        if mock_key_secret.len() < wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_MIN_LEN {
            eprintln!(
                "wire-server: {} must contain at least {} bytes of secret material (got {})",
                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_FLAG,
                wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_MIN_LEN,
                mock_key_secret.len()
            );
            return ExitCode::FAILURE;
        }
        match store.require_scram(&mock_key_secret) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wire-server: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        store
    };
    // Issue #902（SQL-23・TASK-203）: `--search-engine`／`--durability` と同じく
    // 起動後に変更できない構成値のため、bind・listen より前に確定させる
    // （fail-closed）。未指定は `ddl_allowed_users_raw == None` のままとなり、
    // `UserStore::with_ddl_allowed_users` を呼ばない（既定＝DDL 実行権限を
    // 持つユーザーが 0 人。`sql::ddl::require_ddl_permission` の既定拒否）。
    let store = match ddl_allowed_users_raw.as_deref() {
        None => store,
        Some(raw) => {
            let usernames = match wire_server::ddl_permission_opt::parse(raw) {
                Ok(u) => u,
                Err(e) => {
                    eprintln!(
                        "wire-server: invalid {}: {e}",
                        wire_server::ddl_permission_opt::FLAG
                    );
                    return ExitCode::FAILURE;
                }
            };
            match store.with_ddl_allowed_users(&usernames) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("wire-server: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let store = Arc::new(store);

    // engine（永続化 + SQL 表層）を起動する。ユーザーストア読込に続けて bind 前に
    // 開くことで、DB を開けない状態のまま listen してしまう経路を避ける
    // （fail-closed。TASK-73・WIRE-1）。
    //
    // Issue #656・#850: `search_engine_kind`（`None`＝未指定／`default`）と
    // `durability`（既定＝`WriteDurability::Immediate`）の組合せに応じて
    // 4 分岐で構築する（詳細は [`open_engine_core`] のドキュメント参照）。
    // 両方が既定のときは従来どおり `EngineCore::open` をそのまま呼ぶため、
    // エラー型・メッセージまで既存経路とビット同一に保たれる。
    let mut core = match open_engine_core(&db_path, durability, search_engine_kind) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wire-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    // TASK-117（PLAN-9）: opt-in 注入。未指定（既定）では `query_planner`/
    // `embedder` とも未設定のままとなり、`USING PLAN` は従来どおり
    // `CoreError::QueryPlannerUnavailable`/`EmbedderUnavailable`
    // （wire 応答は `XX000`・固定の一般化メッセージ。`SqlSurfaceError::Internal`
    // 経由）で fail-closed 拒否される。
    if let Some(embedder) = embedder {
        core = core.with_embedder(embedder);
    }
    if let Some(query_planner) = query_planner {
        core = core.with_query_planner(query_planner);
    }
    let core = Arc::new(core);

    // `guarded.bind()` は検証済みの数値アドレスへ直接 bind し、`bind_addr`
    // （文字列）を別途 `TcpListener::bind` へ渡すことはしない（検証時と bind 時で
    // DNS 再解決が起きる TOCTOU を作らないため。TASK-67 review 指摘）。
    let listener = match guarded.bind() {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "wire-server: failed to bind {bind_addr} ({:?}): {e}",
                guarded.addrs()
            );
            return ExitCode::FAILURE;
        }
    };

    // Issue #705（テスト専用・feature `fault-injection` 限定）: bind 成功後・
    // `listening on` 出力より前に arm する（テストが両行の出力順に依存できる
    // ようにするため）。プロセス起動あたり高々 1 回だけ呼ばれる。
    #[cfg(feature = "fault-injection")]
    if let Some(kind) = fault_kind {
        wire_server::fault_injection::arm(kind);
        eprintln!("wire-server: fault injection armed: post-commit-panic (test only)");
    }

    // Issue #850: 非既定 durability（`WriteDurability::None`）を選んだ場合に
    // 限り、commit 成功応答が永続を保証しない旨を起動ログへ明示する。既定
    // （`Immediate`）選択時・未指定時はこの行を一切出さない（既存 stderr を
    // ビット同一のまま保つ。`fault injection armed` → `listening on` の行順序
    // 依存ハーネスと同じ理由で、`listening on` より前・bind 成功後に置く）。
    if durability != engine::storage::WriteDurability::default() {
        eprintln!(
            "wire-server: WARNING: --durability {} selected; commit success responses do not guarantee data survives a process crash or power loss until a later durable commit (see docs/design/ingest-write-path.md, RECOVER-5/RECOVER-6)",
            wire_server::durability_opt::token_for(durability)
        );
    }

    // Issue #735（HTTP-1）: `nosql` 選択時のみ、選ばれた表層を示す 1 行を
    // `listening on` の直前に出す（`listening on` より前に置くのは、
    // `wait_for_listening` 系ヘルパーがその行で読み取りを打ち切るため。
    // 停止後に stderr を drain するテストであれば収集済み行に必ず含まれる）。
    // `sql` では出力しない（既存の E2E ハーネスが `fault injection armed` →
    // `listening on` の行順序に依存しているため、SQL 側の stderr をビット
    // 同一のまま保つ）。
    if surface == wire_server::surface::Surface::Nosql {
        eprintln!(
            "wire-server: surface nosql: HTTP/1.1 listener (30s read timeout, 64 max connections; POST /v1/session, POST /v1/session/close, and POST /v1/query (Bearer-gated) available; POST /v1/query op=search, op=scan, op=aggregate, and op=insert all execute against the engine; other paths and non-POST methods rejected with 08P01)"
        );
    }

    // Issue #967: TLS 有効時に限り、有効であることと `--tls-mode` を起動ログへ
    // 1 行だけ出す（鍵・証明書の内容は出さない）。未指定時はこの行を一切出さず
    // 既存 stderr をビット同一のまま保つ（`durability`／`surface nosql` の行と
    // 同じ方針。`listening on` より前・bind 成功後に置く）。
    if let Some((_, _, mode, scram_channel_binding)) = &tls_options {
        eprintln!("wire-server: TLS enabled (mode={})", mode.token());
        // Issue #970・#1088: `--tls-scram-channel-binding enable` を選んだ
        // 場合のみ注意を 1 行追加する（既定 `disable` では従来どおりこの行を
        // 出さず stderr をビット同一のまま保つ）。SQL 表層でこの行に到達して
        // いる時点で `check_scram_channel_binding` は通過済み（葉証明書は
        // RFC 5929 が定義するハッシュを持つ）なので、Ed25519 特有の失敗を
        // 警告する必要はもう無い。NoSQL 表層は同チェックを適用しない
        // （H5・上の `check_scram_channel_binding` 呼び出し箇所のコメント
        // 参照）ため Ed25519 葉証明書のままここへ到達しうるが、SASL 往復を
        // 持たず本文言の対象外（「only takes effect for scram-sha-256
        // authentication」）なので実害はない。
        if *scram_channel_binding {
            // codex-review PR #1087 P2 是正: この行は `--tls-scram-channel-binding
            // enable` 指定の事実のみを示す（「実際に提示された」という確定的な
            // 主張はしない）。実際の提示は `--auth-method scram-sha-256` の
            // 接続に限る（`--auth-method cleartext` では SASL 自体を送らない
            // ため PLUS は一切提示されない）。
            eprintln!(
                "wire-server: SCRAM-SHA-256-PLUS advertisement enabled ({} enable); only takes \
                 effect for scram-sha-256 authentication (see docs/design/tls-channel-binding.md)",
                wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG
            );
        }
    }

    // 実際に bind されたアドレスを出す（`--bind 127.0.0.1:0` の ephemeral port
    // 割り当て結果を E2E テストハーネスがこの行から取得する前提。TASK-73）。
    match listener.local_addr() {
        Ok(addr) => eprintln!("wire-server: listening on {addr}"),
        Err(_) => eprintln!("wire-server: listening on {bind_addr}"),
    }

    // Issue #735（HTTP-1）: 選択された表層のリスナーだけを 1 本起動する。
    // 両表層とも直前までの `GuardedBindAddrs::resolve`／`bind()` を共有して
    // いるため（HTTP-9）、ここでは accept ループの実装だけが分岐する。
    //
    // Issue #743: 同時接続数リミッターは `match surface` の前に 1 回だけ
    // 構築し、選ばれた表層のループへ渡す（1 プロセス 1 表層のため、同じ
    // 構築箇所・同じ定数・同じ型＝共有リミッターという契約を満たす）。
    let limiter = limits::ConnectionLimiter::new(limits::MAX_CONNECTIONS);
    match surface {
        wire_server::surface::Surface::Sql => {
            // Issue #967: TLS 有効時は `accept_loop_with_tls_mode` へ切り替える
            // （`Surface::Sql` 分岐。`Surface::Nosql` は Issue #968 で TLS 併用が
            // 許可されており独自の accept ループへ分岐する。旧 D5 の
            // 併用拒否は撤廃済み）。無効時は従来どおり `accept_loop_with_engine`
            // を呼び、既存経路とビット同一のまま維持する。
            match (&tls_config, &tls_options) {
                (Some(cfg), Some((_, _, mode, _))) => {
                    server::accept_loop_with_tls_mode(
                        listener,
                        store,
                        Some(core),
                        Some(Arc::clone(cfg)),
                        *mode,
                        limiter,
                        limits::READ_TIMEOUT,
                    );
                }
                _ => {
                    server::accept_loop_with_engine(
                        listener,
                        store,
                        core,
                        limiter,
                        limits::READ_TIMEOUT,
                    );
                }
            }
        }
        wire_server::surface::Surface::Nosql => {
            // Issue #752: `store` はセッション認証（`POST /v1/session`）の
            // ユーザーストアとして `Router` へ渡す。セッションストアは
            // 表層選択のたびに新規構築する（プロセス内で 1 表層のみ起動
            // するため、SQL wire 側の `store`／`limiter` と同じ「1 回だけ
            // 構築」方針）。`core`（クエリ実行）は `search`（Issue #764）・
            // `scan`（TASK-186・NOSQL-3・Issue #766）・`aggregate`
            // （Issue #768）・`insert`（Issue #771・#772）の 4 op すべての
            // 実行に使う。
            let sessions = wire_server::http::session::store::SessionStore::new();
            let router = wire_server::http::router::Router::with_engine(
                Arc::clone(&store),
                sessions,
                Arc::clone(&core),
            );
            // Issue #968: TLS 有効時は `accept_loop_with_router_tls` へ切り替える
            // （SQL 表層の `accept_loop_with_tls_mode` 分岐と同じ流儀。
            // `tls_config`／`tls_options` は D3・D4 の組合せ検証を経て両方
            // `Some` のときのみ意味を持つ）。無効時は従来どおり
            // `accept_loop_with_router` を呼び、既存経路とビット同一のまま
            // 維持する。
            match (&tls_config, &tls_options) {
                (Some(cfg), Some((_, _, mode, _))) => {
                    wire_server::http::listener::accept_loop_with_router_tls(
                        listener,
                        limiter,
                        limits::READ_TIMEOUT,
                        router,
                        Arc::clone(cfg),
                        *mode,
                    );
                }
                _ => {
                    wire_server::http::listener::accept_loop_with_router(
                        listener,
                        limiter,
                        limits::READ_TIMEOUT,
                        router,
                    );
                }
            }
        }
    }
    ExitCode::SUCCESS
}

/// `--planner-endpoint <host:port>`／`--planner-model <name>` から
/// `engine::query_planner::OllamaClient` を構築する（TASK-117・PLAN-9）。
///
/// `host:port` は最後の `:` で分割する（IPv6 リテラルの角括弧表記は本 CLI では
/// 受理しない。検証用途の loopback 接続のみを想定するため、IPv4/ホスト名の
/// 単純な `host:port` 表記に限定して曖昧さを避ける）。ホストの loopback 検証
/// 自体は `OllamaConfig::with_host` が担う（IP リテラルは構築時点で非 loopback
/// を拒否。ホスト名は接続直前の名前解決結果検証へ委譲。同メソッドドキュメント
/// 参照）。モデル名は空文字・制御文字混入・過大な長さを CLI 引数の時点で
/// fail-closed に拒否する（プロンプトへ連結される値のため、明らかに不正な値を
/// 早期に弾く防御的措置。実際の SSRF 対策の主体は `OllamaConfig` 側）。
fn build_query_planner(
    endpoint: &str,
    model: &str,
) -> Result<Box<dyn engine::query_planner::LlmClient>, String> {
    let Some(colon_idx) = endpoint.rfind(':') else {
        return Err(format!("expected host:port, got {endpoint:?}"));
    };
    let (host, port_str) = endpoint.split_at(colon_idx);
    let port_str = &port_str[1..];
    if host.is_empty() {
        return Err(format!("expected host:port, got {endpoint:?}"));
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port in {endpoint:?}"))?;

    if model.is_empty() {
        return Err("model name must not be empty".to_string());
    }
    if model.len() > 256 {
        return Err("model name exceeds 256 bytes".to_string());
    }
    if model.contains(['\0', '\n', '\r']) {
        return Err("model name must not contain control characters".to_string());
    }

    let config = engine::query_planner::OllamaConfig::new(model)
        .with_host(host)
        .map_err(|e| format!("{e:?}"))?
        .with_port(port);
    Ok(Box::new(engine::query_planner::OllamaClient::new(config)))
}

/// `--embedder-hashing-dim <N>` から `engine::embedding::HashingEmbedder`
/// （検証用の決定的参照実装。意味的埋め込みではない。同モジュールドキュメント
/// 参照）を構築する（TASK-117・PLAN-9）。`u32` へのパース失敗・`HashingEmbedder::
/// new` の範囲外拒否をそのまま呼び出し元へ fail-closed で伝える。
fn build_hashing_embedder(raw_dim: &str) -> Result<Box<dyn engine::embedding::Embedder>, String> {
    let dim: u32 = raw_dim
        .parse()
        .map_err(|_| format!("invalid dimension {raw_dim:?}"))?;
    let embedder = engine::embedding::HashingEmbedder::new(dim).map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(embedder))
}

/// `--hnsw-full-scan-ratio`／`--hnsw-acorn-max-visible-ratio`／
/// `--hnsw-sparse-visited-max` の 3 フラグの未パース値（Issue #657）。
/// `resolve_search_engine` へ渡す前段の入れ物で、`std::env::args()` を直接
/// 読まずに単体テストできるようにする（`RawHnswTuning` を経由することで
/// 値をパースする責務を `resolve_search_engine` 側へ寄せ、`run_server` の
/// 引数走査ループには形状検証を持たせない）。
struct RawHnswTuning<'a> {
    full_scan_ratio: Option<&'a str>,
    acorn_max_visible_ratio: Option<&'a str>,
    sparse_visited_max: Option<&'a str>,
}

/// `--search-engine` の値（未指定は `None`）と `--hnsw-*` 探索パラメータの
/// 未パース値（Issue #657）から `EngineCore::open_with_engine` へ渡す
/// `SearchEngineKind` を解決する（Issue #656・#657）。純関数として切り出し、
/// `std::env::args()` を直接読まずに単体テストできるようにする（`--planner-*`
/// 系の `build_query_planner`／`build_hashing_embedder` と同じ流儀）。
///
/// `raw` が `None`（`--search-engine` 未指定）かつ `tuning_raw` が全 `None`
/// の場合は `Ok(None)` を返し、`run_server` は既存の `EngineCore::open` 経路を
/// そのまま通す。`raw` が [`wire_server::search_engine_opt::TOKENS`] のいずれ
/// とも厳密一致しない場合・`tuning_raw` の各値が形状不正（`<num>/<den>`
/// 以外・非負整数以外）の場合・`raw` が `None`／`default` なのに `tuning_raw`
/// が非空の場合・意味検証（`ValidatedHnswParams::with_full_scan_ratio`／
/// `with_acorn_max_visible_ratio`）が失敗する場合はいずれも `Err` で
/// fail-closed（既定へ黙って読み替えない）。
fn resolve_search_engine(
    raw: Option<&str>,
    tuning_raw: &RawHnswTuning<'_>,
) -> Result<Option<engine::search_engine::SearchEngineKind>, String> {
    let choice = match raw {
        None => wire_server::search_engine_opt::SearchEngineChoice::Default,
        Some(raw) => wire_server::search_engine_opt::parse(raw)?,
    };

    let mut tuning = wire_server::search_engine_opt::HnswTuning::default();
    if let Some(raw) = tuning_raw.full_scan_ratio {
        tuning.full_scan_ratio = Some(wire_server::search_engine_opt::parse_ratio(raw).map_err(
            |e| {
                format!(
                    "{}: {e}",
                    wire_server::search_engine_opt::FULL_SCAN_RATIO_FLAG
                )
            },
        )?);
    }
    if let Some(raw) = tuning_raw.acorn_max_visible_ratio {
        tuning.acorn_max_visible_ratio = Some(
            wire_server::search_engine_opt::parse_ratio(raw).map_err(|e| {
                format!(
                    "{}: {e}",
                    wire_server::search_engine_opt::ACORN_MAX_VISIBLE_RATIO_FLAG
                )
            })?,
        );
    }
    if let Some(raw) = tuning_raw.sparse_visited_max {
        tuning.sparse_visited_max = Some(
            wire_server::search_engine_opt::parse_sparse_visited_max(raw).map_err(|e| {
                format!(
                    "{}: {e}",
                    wire_server::search_engine_opt::SPARSE_VISITED_MAX_FLAG
                )
            })?,
        );
    }

    choice.to_engine_kind_with(tuning)
}

/// `--surface` の値（未指定は `None`）から [`wire_server::surface::Surface`]
/// を解決する（Issue #734）。純関数として切り出し、`std::env::args()` を
/// 直接読まずに単体テストできるようにする（`resolve_search_engine` と同じ
/// 流儀）。`raw` が `None` は既定 `Surface::Sql`、[`wire_server::surface::
/// TOKENS`] のいずれとも厳密一致しない場合は `Err`（fail-closed。既定へ
/// 黙って読み替えない）。
fn resolve_surface(raw: Option<&str>) -> Result<wire_server::surface::Surface, String> {
    match raw {
        None => Ok(wire_server::surface::Surface::Sql),
        Some(raw) => wire_server::surface::parse(raw),
    }
}

/// `--durability` の値（未指定は `None`）から
/// [`engine::storage::WriteDurability`] を解決する（Issue #850）。純関数として
/// 切り出し、`std::env::args()` を直接読まずに単体テストできるようにする
/// （`resolve_search_engine`・`resolve_surface` と同じ流儀）。`raw` が `None`
/// は既定 [`engine::storage::WriteDurability::default`]（`Immediate`）、
/// [`wire_server::durability_opt::TOKENS`] のいずれとも厳密一致しない場合は
/// `Err`（fail-closed。既定へ黙って読み替えない）。
fn resolve_durability(raw: Option<&str>) -> Result<engine::storage::WriteDurability, String> {
    match raw {
        None => Ok(engine::storage::WriteDurability::default()),
        Some(raw) => wire_server::durability_opt::parse(raw),
    }
}

/// `--auth-method` の値（未指定は `None`）から `wire_server::auth::AuthMethod`
/// を解決する（Issue #940・WIRE-18・TASK-222）。純関数として切り出し、
/// `std::env::args()` を直接読まずに単体テストできるようにする
/// （`resolve_durability`・`resolve_surface` と同じ流儀）。`raw` が `None` は
/// 既定 [`wire_server::auth::AuthMethod::default`]（`Cleartext`。既存の挙動と
/// ビット同一）、[`wire_server::auth_method_opt::TOKENS`] のいずれとも厳密
/// 一致しない場合は `Err`（fail-closed。既定へ黙って読み替えない）。
fn resolve_auth_method(raw: Option<&str>) -> Result<wire_server::auth::AuthMethod, String> {
    match raw {
        None => Ok(wire_server::auth::AuthMethod::default()),
        Some(raw) => wire_server::auth_method_opt::parse(raw),
    }
}

/// `--tls-cert`／`--tls-key`／`--tls-mode`／`--tls-scram-channel-binding`
/// の未パース値（Issue #967・#970）から `(cert_path, key_path, TlsMode,
/// scram_channel_binding)` を解決する。純関数として切り出し、
/// `std::env::args()` を直接読まずに単体テストできるようにする
/// （`resolve_search_engine`・`resolve_durability` と同じ流儀。
/// `--scram-mock-key-file` の組合せ検証を参考にした設計）。
///
/// - `cert`・`key` がいずれも `None`: TLS 未指定。`mode_raw`・
///   `scram_channel_binding_raw` のいずれかが `Some` でも単独指定として
///   `Err`（D4。`--hnsw-*` が `--search-engine` を要求するのと同じ設計）。
///   それ以外は `Ok(None)`（TLS 無効のまま既存経路を通す）。
/// - `cert`・`key` の片方のみ `Some`: 組合せ不正として `Err`。
/// - `cert`・`key` がいずれも `Some`: `mode_raw` を [`wire_server::tls_opt::
///   parse`] で解決する（`None`＝未指定は既定 `TlsMode::Require`。D3:
///   安全側の既定値）。`scram_channel_binding_raw` は
///   [`wire_server::tls_opt::parse_scram_channel_binding`] で解決する
///   （`None`＝未指定は既定 `false`＝非提示。Ed25519 葉証明書での libpq
///   相互運用実測に基づく安全側の既定値。`docs/design/
///   tls-channel-binding.md` 参照）。不正な語彙値はいずれも `Err`
///   （fail-closed。既定へ黙って読み替えない）。
fn resolve_tls_options(
    cert: Option<&std::path::Path>,
    key: Option<&std::path::Path>,
    mode_raw: Option<&str>,
    scram_channel_binding_raw: Option<&str>,
) -> Result<Option<(PathBuf, PathBuf, wire_server::tls_opt::TlsMode, bool)>, String> {
    match (cert, key) {
        (None, None) => {
            if mode_raw.is_some() {
                return Err(format!(
                    "{} requires {} and {} to also be set",
                    wire_server::tls_opt::MODE_FLAG,
                    wire_server::tls_opt::CERT_FLAG,
                    wire_server::tls_opt::KEY_FLAG
                ));
            }
            if scram_channel_binding_raw.is_some() {
                return Err(format!(
                    "{} requires {} and {} to also be set",
                    wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG,
                    wire_server::tls_opt::CERT_FLAG,
                    wire_server::tls_opt::KEY_FLAG
                ));
            }
            Ok(None)
        }
        (Some(_), None) => Err(format!(
            "{} requires {} to also be set",
            wire_server::tls_opt::CERT_FLAG,
            wire_server::tls_opt::KEY_FLAG
        )),
        (None, Some(_)) => Err(format!(
            "{} requires {} to also be set",
            wire_server::tls_opt::KEY_FLAG,
            wire_server::tls_opt::CERT_FLAG
        )),
        (Some(cert), Some(key)) => {
            let mode = match mode_raw {
                None => wire_server::tls_opt::TlsMode::Require,
                Some(raw) => wire_server::tls_opt::parse(raw)?,
            };
            let scram_channel_binding = match scram_channel_binding_raw {
                None => false,
                Some(raw) => wire_server::tls_opt::parse_scram_channel_binding(raw)?,
            };
            Ok(Some((
                cert.to_path_buf(),
                key.to_path_buf(),
                mode,
                scram_channel_binding,
            )))
        }
    }
}

/// `durability`・`search_engine_kind` の組合せから `EngineCore` を構築する
/// choke point（Issue #850）。4 分岐すべてを 1 箇所へ集約することで、
/// `run_server` からは `match` を持ち出さずに呼べるようにし、`main.rs` 内
/// `#[cfg(test)] mod tests` から直接呼んで検証できるようにする。
///
/// - 既定 durability・既定エンジン: [`engine::core::EngineCore::open`] を
///   従来どおりそのまま呼ぶ（エラー型・メッセージまで既存経路とビット同一に
///   保つ設計判断。`search_engine_opt.rs` モジュールドキュメント参照）。
/// - 既定 durability・ANN opt-in: [`engine::core::EngineCore::open_with_engine`]。
/// - 非既定 durability・既定エンジン: [`engine::core::EngineCore::open_with_durability`]
///   （Issue #849 が公開した durability 版）。
/// - 非既定 durability・ANN opt-in: [`engine::storage::Storage::open_with_durability`]
///   で `Storage` を開いたうえで [`engine::core::EngineCore::from_storage_with_engine`]
///   へ渡す。
///
/// **`EngineCore::from_storage`（`search_engine_kind()` が構造的に `None` に
/// なる）は使わない**: 非既定 durability・既定エンジンのセルでこれを使うと
/// `EXPLAIN` の `engine:` 行が `parallel_brute_force` から `(custom_provider)`
/// へ divergent し、Issue #411 の契約が崩れる。
fn open_engine_core(
    db_path: &std::path::Path,
    durability: engine::storage::WriteDurability,
    search_engine_kind: Option<engine::search_engine::SearchEngineKind>,
) -> Result<engine::core::EngineCore, String> {
    let is_default_durability = durability == engine::storage::WriteDurability::default();
    match (is_default_durability, search_engine_kind) {
        (true, None) => engine::core::EngineCore::open(db_path)
            .map_err(|e| format!("failed to open database at {db_path:?}: {e}")),
        (true, Some(kind)) => engine::core::EngineCore::open_with_engine(db_path, kind)
            .map_err(|e| format!("failed to open database at {db_path:?}: {e}")),
        (false, None) => engine::core::EngineCore::open_with_durability(db_path, durability)
            .map_err(|e| format!("failed to open database at {db_path:?}: {e}")),
        (false, Some(kind)) => {
            let storage = engine::storage::Storage::open_with_durability(db_path, durability)
                .map_err(|e| format!("failed to open database at {db_path:?}: {e}"))?;
            Ok(engine::core::EngineCore::from_storage_with_engine(
                storage, kind,
            ))
        }
    }
}

/// `hash-password` サブコマンド: stdin からパスワードを 1 行読み、新規 salt を
/// 生成して PHC 文字列を stdout へ出力する。パスワードを引数・ログに残さない。
/// `hash-password [--with-scram-sha-256]` サブコマンド。既定は従来どおり PHC
/// のみを出力する。`--with-scram-sha-256`（Issue #940・WIRE-18・TASK-222）を
/// 付けると、ユーザーストアの 4 番目のフィールド（`username:tenant_id:` より
/// 後ろの部分。`phc:scram_verifier`）を出力する。未知の追加引数は fail-closed
/// に拒否する（以前は黙って無視していたが、typo で `--with-scram-sha-256` の
/// つもりが無視される事故を防ぐため）。
fn run_hash_password(args: &[String]) -> ExitCode {
    let mut with_scram = false;
    for arg in args {
        match arg.as_str() {
            "--with-scram-sha-256" => with_scram = true,
            other => {
                eprintln!("wire-server: hash-password: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut password = String::new();
    if let Err(e) = std::io::stdin().read_line(&mut password) {
        eprintln!("wire-server: failed to read password from stdin: {e}");
        return ExitCode::FAILURE;
    }
    let password = password.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        eprintln!("wire-server: empty password is not allowed");
        return ExitCode::FAILURE;
    }

    let salt = match auth::generate_salt() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: failed to read salt from CSPRNG: {e}");
            return ExitCode::FAILURE;
        }
    };

    let phc = match auth::argon2id::encode_phc(password.as_bytes(), &salt, &auth::DEFAULT_PARAMS) {
        Ok(phc) => phc,
        Err(e) => {
            eprintln!("wire-server: failed to compute password hash: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !with_scram {
        println!("{phc}");
        return ExitCode::SUCCESS;
    }

    // SASLprep（RFC 4013）は自作しない。印字可能 ASCII 以外を含むパスワードは
    // 拒否する（`scram::generate_verifier` の制約。README に明記）。
    let scram_salt = match auth::generate_salt() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: failed to read salt from CSPRNG: {e}");
            return ExitCode::FAILURE;
        }
    };
    match auth::scram::generate_verifier(
        password.as_bytes(),
        &scram_salt,
        auth::scram::SCRAM_ITERATIONS,
    ) {
        Ok(verifier) => {
            println!("{phc}:{}", verifier.to_verifier_string());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "wire-server: failed to compute SCRAM verifier: {e:?} \
                 (password must contain only printable ASCII characters, 0x20-0x7e)"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // workspace の雛形が成立していること（wire-server から engine への path 依存が
    // リンクできること）を確認する smoke テスト。対象ビヘイビア ID なし。
    #[test]
    fn engine_is_linked() {
        assert_eq!(engine::ENGINE_NAME, "engine");
    }

    // TASK-117（PLAN-9）: `--planner-endpoint`／`--planner-model`／
    // `--embedder-hashing-dim` の引数パース単体テスト。wire 経由の実行契約
    // （fail-closed 応答の中身）は `tests/wire_using_plan.rs` が担う。
    //
    // `build_query_planner`/`build_hashing_embedder` の `Ok` 側（`Box<dyn
    // LlmClient>`/`Box<dyn Embedder>`）は `Debug` を実装しないため
    // `unwrap_err()` が使えない。`Result::err()` で `Option<String>` へ変換して
    // から展開する（`Err` の中身＝`String` のみを見る、両関数共通のヘルパー）。
    fn expect_err<T>(result: Result<T, String>) -> String {
        result.err().expect("expected an error")
    }

    #[test]
    fn build_query_planner_accepts_loopback_host_and_port() {
        build_query_planner("127.0.0.1:11434", "dummy-model").expect("valid endpoint/model");
    }

    #[test]
    fn build_query_planner_rejects_missing_colon() {
        let err = expect_err(build_query_planner("127.0.0.1", "dummy-model"));
        assert!(err.contains("host:port"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_empty_host() {
        let err = expect_err(build_query_planner(":11434", "dummy-model"));
        assert!(err.contains("host:port"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_non_numeric_port() {
        let err = expect_err(build_query_planner("127.0.0.1:not-a-port", "dummy-model"));
        assert!(err.contains("port"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_non_loopback_ip_host() {
        // `OllamaConfig::with_host` が非 loopback IP リテラルを構築時点で拒否する
        // 既存契約（TASK-72 未実装のうちは平文接続を loopback へ限定する）を、
        // CLI 経由でも維持できていることを確認する。
        let err = expect_err(build_query_planner("10.0.0.5:11434", "dummy-model"));
        assert!(!err.is_empty());
    }

    #[test]
    fn build_query_planner_rejects_empty_model_name() {
        let err = expect_err(build_query_planner("127.0.0.1:11434", ""));
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_model_name_with_control_characters() {
        let err = expect_err(build_query_planner("127.0.0.1:11434", "bad\nmodel"));
        assert!(err.contains("control"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_overlong_model_name() {
        let long_name = "a".repeat(257);
        let err = expect_err(build_query_planner("127.0.0.1:11434", &long_name));
        assert!(err.contains("256"), "unexpected error: {err}");
    }

    #[test]
    fn build_hashing_embedder_accepts_valid_dim() {
        build_hashing_embedder("16").expect("valid dim");
    }

    #[test]
    fn build_hashing_embedder_rejects_zero_dim() {
        expect_err(build_hashing_embedder("0"));
    }

    #[test]
    fn build_hashing_embedder_rejects_non_numeric_dim() {
        let err = expect_err(build_hashing_embedder("not-a-number"));
        assert!(err.contains("invalid dimension"), "unexpected error: {err}");
    }

    #[test]
    fn build_hashing_embedder_rejects_dim_exceeding_max() {
        // `MAX_EMBEDDER_DIM`（= `storage::MAX_EMBEDDING_DIM`）を超える値は
        // `HashingEmbedder::new` が `Result::Err` で拒否する契約
        // （`embedding.rs` モジュールドキュメント参照）。上限値そのものは
        // engine 側の実装既定であり本テストでは転記せず、`u32::MAX` という
        // どの上限設定でも確実に超過する値で契約を確認する。
        expect_err(build_hashing_embedder(&u32::MAX.to_string()));
    }

    // Issue #656・#657: `--search-engine`／`--hnsw-*` の解決結果パーステスト。
    // wire 経由の実行契約（`EXPLAIN` 一致・RLS 非漏えい）は
    // `tests/wire_search_engine_opt.rs`（in-process）・
    // `tests/wire_search_engine_cli.rs`（子プロセス）が担う。

    const EMPTY_TUNING: RawHnswTuning<'static> = RawHnswTuning {
        full_scan_ratio: None,
        acorn_max_visible_ratio: None,
        sparse_visited_max: None,
    };

    #[test]
    fn resolve_search_engine_none_is_default_engine_core_open_path() {
        // R2: 未指定は `EngineCore::open` をそのまま通す契約の入口
        // （`run_server` 側の分岐は `None` を既存経路として扱う）。
        assert_eq!(resolve_search_engine(None, &EMPTY_TUNING), Ok(None));
    }

    #[test]
    fn resolve_search_engine_default_token_is_also_none() {
        assert_eq!(
            resolve_search_engine(Some("default"), &EMPTY_TUNING),
            Ok(None)
        );
    }

    #[test]
    fn resolve_search_engine_accepts_hnsw_variants() {
        for tok in ["hnsw", "hnsw_f16", "hnsw_i8"] {
            let kind = resolve_search_engine(Some(tok), &EMPTY_TUNING)
                .unwrap_or_else(|e| panic!("expected {tok:?} to resolve, got error: {e}"));
            assert!(kind.is_some(), "expected Some(kind) for {tok:?}");
        }
    }

    #[test]
    fn resolve_search_engine_rejects_unknown_value_fail_closed() {
        // R3: 不正な値は既定へ読み替えず起動エラーにする。
        let err = expect_err(resolve_search_engine(Some("bogus"), &EMPTY_TUNING));
        assert!(err.contains("--search-engine"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_search_engine_rejects_case_variant() {
        // 厳密一致のみ受理（`search_engine_opt::parse` の契約）。
        expect_err(resolve_search_engine(Some("HNSW"), &EMPTY_TUNING));
    }

    // Issue #657: `--hnsw-*` 探索パラメータ opt-in の `resolve_search_engine`
    // 結線テスト。フラグ単位のパース・意味検証自体は
    // `search_engine_opt::tests` が担うため、ここでは「未パース raw 文字列 →
    // `SearchEngineKind`」の配線と D1（`Default` との組合せ拒否）を確認する。

    #[test]
    fn resolve_search_engine_accepts_hnsw_with_valid_tuning() {
        let tuning = RawHnswTuning {
            full_scan_ratio: Some("1/4"),
            acorn_max_visible_ratio: Some("1/2"),
            sparse_visited_max: Some("8"),
        };
        let kind = resolve_search_engine(Some("hnsw"), &tuning)
            .expect("valid tuning must resolve")
            .expect("Some for hnsw");
        let engine::search_engine::SearchEngineKind::Hnsw(params) = kind else {
            panic!("expected Hnsw kind");
        };
        assert_eq!(
            params.full_scan_ratio(),
            engine::hnsw::Ratio {
                numerator: 1,
                denominator: 4
            }
        );
        assert_eq!(
            params.acorn_max_visible_ratio(),
            Some(engine::hnsw::Ratio {
                numerator: 1,
                denominator: 2
            })
        );
        assert_eq!(params.sparse_visited_max(), 8);
    }

    #[test]
    fn resolve_search_engine_rejects_tuning_without_opt_in_engine() {
        // D1: `--search-engine` 未指定のまま `--hnsw-*` を指定する構成は
        // fail-closed で拒否する（黙って無視しない）。
        let tuning = RawHnswTuning {
            full_scan_ratio: Some("1/4"),
            acorn_max_visible_ratio: None,
            sparse_visited_max: None,
        };
        let err = expect_err(resolve_search_engine(None, &tuning));
        assert!(err.contains("--search-engine"), "unexpected error: {err}");

        let err = expect_err(resolve_search_engine(Some("default"), &tuning));
        assert!(err.contains("--search-engine"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_search_engine_rejects_malformed_ratio_with_flag_name() {
        let tuning = RawHnswTuning {
            full_scan_ratio: Some("not-a-ratio"),
            acorn_max_visible_ratio: None,
            sparse_visited_max: None,
        };
        let err = expect_err(resolve_search_engine(Some("hnsw"), &tuning));
        assert!(
            err.contains(wire_server::search_engine_opt::FULL_SCAN_RATIO_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_search_engine_rejects_semantically_invalid_ratio_with_flag_name() {
        let tuning = RawHnswTuning {
            full_scan_ratio: Some("1/0"),
            acorn_max_visible_ratio: None,
            sparse_visited_max: None,
        };
        let err = expect_err(resolve_search_engine(Some("hnsw"), &tuning));
        assert!(
            err.contains(wire_server::search_engine_opt::FULL_SCAN_RATIO_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_search_engine_rejects_malformed_sparse_visited_max_with_flag_name() {
        let tuning = RawHnswTuning {
            full_scan_ratio: None,
            acorn_max_visible_ratio: None,
            sparse_visited_max: Some("not-a-number"),
        };
        let err = expect_err(resolve_search_engine(Some("hnsw_i8"), &tuning));
        assert!(
            err.contains(wire_server::search_engine_opt::SPARSE_VISITED_MAX_FLAG),
            "unexpected error: {err}"
        );
    }

    // Issue #734: `--surface` の解決結果パーステスト。wire 経由の実行契約
    // （受理・拒否の外形挙動）は `tests/http1_surface_select.rs`（子プロセス）が
    // 担う。

    #[test]
    fn resolve_surface_none_is_sql() {
        assert_eq!(
            resolve_surface(None),
            Ok(wire_server::surface::Surface::Sql)
        );
    }

    #[test]
    fn resolve_surface_accepts_both_tokens() {
        assert_eq!(
            resolve_surface(Some("sql")),
            Ok(wire_server::surface::Surface::Sql)
        );
        assert_eq!(
            resolve_surface(Some("nosql")),
            Ok(wire_server::surface::Surface::Nosql)
        );
    }

    #[test]
    fn resolve_surface_rejects_unknown_value_fail_closed() {
        let err = expect_err(resolve_surface(Some("bogus")));
        assert!(
            err.contains(wire_server::surface::FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_surface_rejects_case_variant() {
        // 厳密一致のみ受理（`surface::parse` の契約）。
        expect_err(resolve_surface(Some("SQL")));
    }

    // Issue #940: `--auth-method` の解決結果パーステスト。wire 経由の実行契約
    // （SASL 往復・起動時 fail-closed 検証）は `tests/wire18_scram.rs`・
    // `tests/wire_auth_method_cli.rs`（子プロセス）が担う。

    #[test]
    fn resolve_auth_method_none_is_cleartext_default() {
        assert_eq!(
            resolve_auth_method(None),
            Ok(wire_server::auth::AuthMethod::Cleartext)
        );
    }

    #[test]
    fn resolve_auth_method_accepts_both_tokens() {
        assert_eq!(
            resolve_auth_method(Some("cleartext")),
            Ok(wire_server::auth::AuthMethod::Cleartext)
        );
        assert_eq!(
            resolve_auth_method(Some("scram-sha-256")),
            Ok(wire_server::auth::AuthMethod::ScramSha256)
        );
    }

    #[test]
    fn resolve_auth_method_rejects_unknown_value_fail_closed() {
        let err = expect_err(resolve_auth_method(Some("bogus")));
        assert!(
            err.contains(wire_server::auth_method_opt::FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_auth_method_rejects_case_variant() {
        expect_err(resolve_auth_method(Some("SCRAM-SHA-256")));
    }

    // Issue #850: `--durability` の解決・`open_engine_core` 4 分岐の単体テスト。
    // 子プロセス経由の外形的検証（起動受理・拒否・警告出力）は
    // `tests/wire_durability_cli.rs` が担う。

    #[test]
    fn resolve_durability_none_is_immediate_default() {
        assert_eq!(
            resolve_durability(None),
            Ok(engine::storage::WriteDurability::default())
        );
        assert_eq!(
            engine::storage::WriteDurability::default(),
            engine::storage::WriteDurability::Immediate
        );
    }

    #[test]
    fn resolve_durability_accepts_both_tokens() {
        assert_eq!(
            resolve_durability(Some("immediate")),
            Ok(engine::storage::WriteDurability::Immediate)
        );
        assert_eq!(
            resolve_durability(Some("none")),
            Ok(engine::storage::WriteDurability::None)
        );
    }

    #[test]
    fn resolve_durability_rejects_unknown_value_fail_closed() {
        let err = expect_err(resolve_durability(Some("sync")));
        assert!(
            err.contains(wire_server::durability_opt::FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_durability_rejects_case_variant() {
        // 厳密一致のみ受理（`durability_opt::parse` の契約）。
        expect_err(resolve_durability(Some("Immediate")));
    }

    // Issue #967・#970: `--tls-cert`／`--tls-key`／`--tls-mode`／
    // `--tls-scram-channel-binding` の組合せ解決の単体テスト。子プロセス
    // 経由の外形的検証（起動受理・拒否・TLS ハンドシェイク完走・`08P01`
    // 平文拒否）は `tests/wire_tls_cli.rs` が担う。

    #[test]
    fn resolve_tls_options_none_when_all_unset() {
        assert_eq!(resolve_tls_options(None, None, None, None), Ok(None));
    }

    #[test]
    fn resolve_tls_options_mode_alone_is_rejected() {
        // D4: `--tls-mode` 単独指定は組合せ不正（`--hnsw-*` が
        // `--search-engine` を要求するのと同じ設計）。
        let err = expect_err(resolve_tls_options(None, None, Some("require"), None));
        assert!(
            err.contains(wire_server::tls_opt::MODE_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_tls_options_scram_channel_binding_alone_is_rejected() {
        // `--tls-mode` と同じ組合せ不正の設計を `--tls-scram-channel-binding`
        // にも適用する（Issue #970）。
        let err = expect_err(resolve_tls_options(None, None, None, Some("enable")));
        assert!(
            err.contains(wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_tls_options_cert_alone_is_rejected() {
        let err = expect_err(resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            None,
            None,
            None,
        ));
        assert!(
            err.contains(wire_server::tls_opt::KEY_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_tls_options_key_alone_is_rejected() {
        let err = expect_err(resolve_tls_options(
            None,
            Some(std::path::Path::new("key.pem")),
            None,
            None,
        ));
        assert!(
            err.contains(wire_server::tls_opt::CERT_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_tls_options_mode_unset_defaults_to_require() {
        // D3: モード省略時の既定は安全側の `require`。
        let (cert, key, mode, scram_channel_binding) = resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            Some(std::path::Path::new("key.pem")),
            None,
            None,
        )
        .expect("cert+key must be accepted")
        .expect("cert+key must yield Some");
        assert_eq!(cert, std::path::PathBuf::from("cert.pem"));
        assert_eq!(key, std::path::PathBuf::from("key.pem"));
        assert_eq!(mode, wire_server::tls_opt::TlsMode::Require);
        // Issue #970: 未指定時の既定は非提示（`false`）。libpq 相互運用実測
        // に基づく安全側の既定値（`docs/design/tls-channel-binding.md`）。
        assert!(!scram_channel_binding);
    }

    #[test]
    fn resolve_tls_options_accepts_explicit_allow() {
        let (_, _, mode, _) = resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            Some(std::path::Path::new("key.pem")),
            Some("allow"),
            None,
        )
        .expect("cert+key+allow must be accepted")
        .expect("cert+key+allow must yield Some");
        assert_eq!(mode, wire_server::tls_opt::TlsMode::Allow);
    }

    #[test]
    fn resolve_tls_options_rejects_unknown_mode_value() {
        let err = expect_err(resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            Some(std::path::Path::new("key.pem")),
            Some("prefer"),
            None,
        ));
        assert!(
            err.contains(wire_server::tls_opt::MODE_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_tls_options_scram_channel_binding_unset_defaults_to_disabled() {
        // Issue #970: cert+key のみ指定した既存経路が壊れないことも確認する
        // （新フラグ追加が既定挙動を変えない回帰確認）。
        let (_, _, _, scram_channel_binding) = resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            Some(std::path::Path::new("key.pem")),
            None,
            None,
        )
        .expect("cert+key must be accepted")
        .expect("cert+key must yield Some");
        assert!(!scram_channel_binding);
    }

    #[test]
    fn resolve_tls_options_accepts_explicit_enable() {
        let (_, _, _, scram_channel_binding) = resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            Some(std::path::Path::new("key.pem")),
            None,
            Some("enable"),
        )
        .expect("cert+key+enable must be accepted")
        .expect("cert+key+enable must yield Some");
        assert!(scram_channel_binding);
    }

    #[test]
    fn resolve_tls_options_rejects_unknown_scram_channel_binding_value() {
        let err = expect_err(resolve_tls_options(
            Some(std::path::Path::new("cert.pem")),
            Some(std::path::Path::new("key.pem")),
            None,
            Some("true"),
        ));
        assert!(
            err.contains(wire_server::tls_opt::SCRAM_CHANNEL_BINDING_FLAG),
            "unexpected error: {err}"
        );
    }

    /// テストごとに衝突しない一時ディレクトリ（DB ファイルの置き場）を確保し、
    /// `Drop` で確実に削除するガード（`tests/wire_search_engine_cli.rs::
    /// TempFixtureDir` と同型）。`open_engine_core` のテストは redb ファイルを
    /// 実際に作成するため、テスト間の衝突・残留を避ける。
    struct TempDbDir {
        dir: std::path::PathBuf,
    }

    impl TempDbDir {
        fn new(label: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "wire-server-open-engine-core-{label}-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock")
                    .as_nanos(),
                seq
            ));
            std::fs::create_dir(&dir).expect("create unique fixture dir");
            Self { dir }
        }

        fn db_path(&self) -> std::path::PathBuf {
            self.dir.join("db.redb")
        }
    }

    impl Drop for TempDbDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// (true, None) セル: 既定 durability・既定エンジン。`EngineCore::open`
    /// 経路がそのまま通るため `search_engine_kind()` は既定エンジンを表す
    /// `Some(default_kind())` のまま（advisor 指摘 2: `open_engine_core` が
    /// 誤って `EngineCore::from_storage` を使うと構造的に `None` へ化ける
    /// ため、ここでしか検出できない非 vacuous な回帰点）。
    #[test]
    fn open_engine_core_default_durability_no_engine_keeps_default_kind() {
        let fixture = TempDbDir::new("default-default");
        let core = open_engine_core(
            &fixture.db_path(),
            engine::storage::WriteDurability::default(),
            None,
        )
        .expect("open default engine core");
        assert_eq!(
            core.search_engine_kind(),
            Some(engine::search_engine::default_kind())
        );
    }

    /// (false, None) セル: 非既定 durability・既定エンジン。
    /// `EngineCore::open_with_durability` 経由でも `search_engine_kind()` は
    /// 既定エンジンを表す `Some(default_kind())` のまま保たれること
    /// （`EngineCore::from_storage` を誤用していないことの検証。誤用すると
    /// `None` へ化ける）。
    #[test]
    fn open_engine_core_non_default_durability_no_engine_keeps_default_kind() {
        let fixture = TempDbDir::new("none-default");
        let core = open_engine_core(
            &fixture.db_path(),
            engine::storage::WriteDurability::None,
            None,
        )
        .expect("open non-default durability engine core");
        assert_eq!(
            core.search_engine_kind(),
            Some(engine::search_engine::default_kind())
        );
    }

    /// (false, Some(hnsw_kind)) セル: 非既定 durability・ANN opt-in。
    /// `Storage::open_with_durability` + `EngineCore::from_storage_with_engine`
    /// 経由で `search_engine_kind()` が指定した Hnsw kind を保持すること。
    #[test]
    fn open_engine_core_non_default_durability_with_hnsw_engine_sets_hnsw_kind() {
        let fixture = TempDbDir::new("none-hnsw");
        let hnsw_kind = wire_server::search_engine_opt::SearchEngineChoice::Hnsw
            .to_engine_kind()
            .expect("valid hnsw params")
            .expect("Some for hnsw");
        let core = open_engine_core(
            &fixture.db_path(),
            engine::storage::WriteDurability::None,
            Some(hnsw_kind),
        )
        .expect("open non-default durability + hnsw engine core");
        assert!(matches!(
            core.search_engine_kind(),
            Some(engine::search_engine::SearchEngineKind::Hnsw(_))
        ));
    }
}
