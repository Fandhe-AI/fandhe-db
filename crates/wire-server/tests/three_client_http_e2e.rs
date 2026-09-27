//! NoSQL 表層（`--surface nosql`。HTTP/1.1 自作リスナー・`/v1/session`／
//! `/v1/session/close`／`/v1/query`）の層 B 統合テスト（Issue #776・
//! TASK-183・HTTP-13。ポインタ: `docs/spec/05-tasks.md` TASK-183・
//! `docs/spec/04-behavior/http-transport.md` HTTP-13）。
//!
//! 責務境界: SQL 表層側の `tests/three_client_e2e.rs`（psql／psycopg／pg。
//! `#[ignore]`・`make e2e-three-client`）が担う層 A/層 B 分割方針
//! （`docs/design/three-client-e2e-harness.md`）を NoSQL 表層へ踏襲する。
//! NoSQL 表層自体の契約（`op` 許可リスト・`vector`／`plan`／`mode`／`hybrid`
//! 束縛・`operation_id` 必須化・precision fail-closed・RLS 暗黙適用等）の
//! 回帰保護は既存の層 A（`http4_session_issue.rs`・`http5_query_bearer.rs`・
//! `http8_session_close.rs`・`nosql1_*`〜`nosql11_*`。常時 `make ci`）が担う。
//! 本ファイルはその上澄みとして、無改造の外部 HTTP クライアント
//! （curl／Python 標準ライブラリ `urllib.request`／Node.js 組み込み
//! `fetch`）が実バイナリへ接続して `session → search → close` を実行
//! できることを外形的に確認する導入ハーネスであり、`#[ignore]` とし
//! `make e2e-three-client-http` から明示的に実行する（`ci` には含めない）。
//! 3 クライアントとも `run_session_search_close_scenario` を共有し、
//! シナリオ内容（session 発行→トークン形状検証→search 3 行→close→
//! 失効後再送の `401`／`28000`→stderr 非漏えい検証）はクライアント種別に
//! 依存しない（Issue #777・TASK-183・HTTP-13）。urllib／fetch のクライアント
//! スクリプト本体は `tests/three_client_http/{urllib_client.py,
//! fetch_client.js}` に置き、SQL 表層側の `tests/three_client/
//! {psycopg_client.py, pg_client.js}` と同じ配置・入出力規約
//! （接続情報・要求本文はすべて環境変数経由・`PYTHON_BIN`／`NODE_BIN` で
//! インタプリタを上書き可）を踏襲する。外部パッケージ（pip／npm）には
//! 依存しない（`.claude/rules/dependency-policy.md`）。
//!
//! 起動・ポート取得は `common::SpawnedServer`（`http4_session_issue.rs` の
//! `spawned_binary_accepts_valid_login_over_nosql_surface` と同型）を再利用
//! する。seed（`docs` テーブル・3 テナント × Public 1 行）は
//! `tests/three_client_e2e.rs::seed_three_tenant_db` と同一内容をこのファイル
//! 専用に複製する（`extended_syntax_e2e.rs` の前例と同方針。private 関数の
//! ため import できない）。
//!
//! ツール未検出（curl／python3／node が見つからない）・非 0 終了・応答形状の
//! 不一致はいずれも `panic!` で失敗させ、silent skip はしない
//! （`.claude/rules/coding-rust.md` の実行規約「テストの skip・ignore・
//! アサーション弱体化で CI を通さない」の精神を、明示的に選択実行するこの
//! 導線でも維持する）。
//!
//! 実行記録（Issue #778）: 各テストはシナリオ完走後に `[e2e-record]` 行を
//! stderr へ出力する（`tests/three_client_e2e.rs` の先例と同型）。使用した
//! クライアントバイナリの版（`HttpClient::version`）・各段の観測要点
//! （HTTP ステータス・`row_count`・返却 id 集合等）のみを含み、トークン・
//! ユーザー名・パスワード・テナント id は出力前に機械検証して含めない
//! （`--nocapture` で表示しても安全）。PR 本文への記録様式・再実行手順は
//! `docs/design/three-client-e2e-harness.md`「実行記録の様式と運用手順
//! （Issue #778）」節を参照。
//!
//! ## SQL 経路パリティ（Issue #779）
//!
//! 上記のスモークシナリオとは独立に、同一クエリ意図を SQL 表層（実
//! `wire-server`・無改造 `psql`）と NoSQL 表層（実 `wire-server --surface
//! nosql`・無改造 HTTP クライアント）の双方へ投げ、**列名・型・行集合**が
//! 一致することを固定する（`run_sql_nosql_parity_scenario`。curl／urllib／
//! fetch の 3 テストが共有）。
//!
//! **起動方式（順次 2 プロセス）**: redb は単一ライターのため同一 DB
//! ファイルを 2 プロセスで同時に開けない。よって
//! 「seed 1 回 → SQL 表層起動（`--surface` なし）→ psql で全クエリ採取・
//! 生 wire で型採取 → `stop_and_drain`（SIGKILL）→ 同じ DB ファイルで
//! NoSQL 表層（`--surface nosql`）起動 → HTTP クライアントで全クエリ採取 →
//! 比較」の順に実行する。`stop_and_drain` は SIGKILL のため、続く NoSQL
//! 表層の起動時 stderr に非クリーン終了からの回復（open 時修復）を示す行が
//! 出ることがあるが、本シナリオは読み取り専用のため無害であり欠陥ではない。
//!
//! **型の観測方法（psql では取得不能）**: 拡張クエリプロトコルは `0A000`
//! で未対応（`\gdesc` が使えない）ため、列名・値は psql（`-A`・ヘッダ付き・
//! `-F '|'`・`-P footer=off`）から取り、型は**同じ SQL 表層プロセスへの
//! 生 simple query**（`RowDescription` の型 OID）から取る
//! （`sql_column_types_via_raw_wire`。固定表 OID 1700→`numeric`／25→`text`
//! 以外は fail-closed に `panic!`）。
//!
//! **正規化モデル（表現差と意味差の峻別）**: NoSQL 応答の JSON セルは
//! `json_cell_to_pg_text` で psql のテキスト表現へ正規化してから比較する
//! （`Cell::Integer`/`Cell::Float` は両表層とも Rust `Display` 経由で同じ
//! 10 進テキストになる契約を利用。`VECTOR` 列は `[v1,v2,...]` 形式へ
//! 正規化）。**表現差**（値表現は同じでも型名 `columns[].type` が実列と
//! 式項目〔集計〕を区別せず一律 `"text"` で公告される点、`u64` が
//! `2^53` を超える場合の JS 側丸め可能性）は正規化して通すが、**意味差**
//! （行集合・件数・キー順・列名・型 OID 対応の不一致）は正規化やアサート
//! 弱体化で吸収せず、テストを fail させて報告する（`json_cell_to_pg_text`
//! は本 seed に現れない `Bool`／`Object` セルを防御的に `panic!` する）。
//!
//! **比較順序**: `search`（`ORDER BY` あり）・`aggregate`（`GROUP BY`
//! 既定キー順は SQL テキスト経由と完全一致することを層 A が固定済み）は
//! 順序付き比較、`scan`（SQL-15 は順序保証を持たない契約）は多重集合比較を
//! 行う（`ParityCase::ordered`）。各ケースは固定オラクル（`expected_rows`）
//! とも一致させ、両表層が同じ誤りを返すケースを排除する。
//!
//! **クエリ集合**: `nosql-api.md`「SQL ↔ NoSQL 対応表」に対応する
//! search-1〜5・scan-1・agg-1〜4 の 10 ケース（`PARITY_CASES`）を、alice
//! （tenant-a）・bob（tenant-b）・carol（tenant-c）の 3 テナントそれぞれで
//! 実行する。wire 認証経路の `PolicyContext` は `Public` ＋ 自テナントの
//! `Private` を許可可視性とする（RLS-11・TASK-195。read-your-writes）ため、
//! alice は自身の Private 行（id=11）・bob は自身の Private 行（id=12）を
//! 両表層の応答に一致して含む（`ParityCase::expected_rows_{alice,bob}` の
//! テナント別固定オラクル）。3 テナントいずれも**他テナント**の Private
//! 行 id が両表層のどの応答にも現れないことをあわせて検証する。
//!
//! ## DML パリティ（Issue #877）
//!
//! 上記 2 シナリオ（読み取り専用）とは異なり、`UPDATE`／`DELETE`（SQL-17・
//! SQL-18・SQL-19。単一行 `id` 完全一致形のみ。述語形 `filter` は
//! Issue #1062 で実行結線済みだが、複数プロセス・複数言語クライアントを
//! 要する本ファイルへは拡張せず、層 A（`nosql12_update_delete.rs` の
//! 述語形節）でパリティ・台帳照合・RLS-9 を固定する）が SQL 表層と NoSQL
//! 表層で同一の実行結果（影響行数・
//! エラー `wire_code`・操作後の状態）を返すことを `run_sql_nosql_dml_parity_scenario`
//! で固定する（curl／urllib／fetch の 3 テストが共有）。エラー時の `message`
//! はケースごとに束縛段階が表層間で異なりうるため比較対象に含めない
//! （`DmlExpectation::Error`・`DmlOutcomeSummary` のコメントを参照）。
//!
//! **起動方式（複数 DB・順次プロセス）**: DML は状態を変えるうえ、0 行の
//! `UPDATE`／`DELETE` も台帳に記録される（`docs/design/sql-delete-single-row.md`
//! 「0 行成功への写像と台帳記録」節）。同一 `operation_id` を「他テナント
//! 行向け」と「未存在 id 向け」の両方に同一 DB 内で使うと 2 回目が
//! `23505`／`22023` になり応答比較にならないため、RLS-9 の応答同一性検証
//! だけは他テナント行を含む DB（DB-F）と含まない DB（DB-M）を分けて用意
//! する。それ以外のケース（成功・`operation_id` 必須化・台帳照合・複数列
//! `SET` の宣言順）は、同一内容で複製した 2 つの DB（DB-S を SQL 表層で、
//! DB-N を NoSQL 表層で）へ同じ手順を順に適用して比較する。
//!
//! **書き込み後の SIGKILL について**: `stop_and_drain` は SIGKILL だが、
//! DML の応答を受信し終えてから呼ぶため無害である。durability は起動時
//! 引数を渡さず既定（`immediate`）のままとし、commit 成功応答が返った
//! 時点で永続化が保証される契約（`docs/spec/04-behavior/recovery.md`
//! RECOVER-5 ポインタ）に依拠する。上記の 2 つの読み取り専用シナリオが
//! 述べる「読み取り専用のため無害」という理由づけは本節には当てはまらず、
//! 根拠を durability 契約に差し替える。
//!
//! **DML の採取方法（生 wire）**: SQL 側の `UPDATE`／`DELETE` は psql
//! ではなく生 wire（`run_sql_dml`）で送る。psql は拡張クエリプロトコル
//! 未対応で `CommandComplete` タグ・`ErrorResponse` の SQLSTATE／message を
//! 安定に取り出せないため、このファイル既存の型採取ヘルパー
//! （`sql_column_types_via_raw_wire`）と同じ方針を踏襲する。読み取り専用の
//! 状態確認（操作後の最終状態）は引き続き psql（`run_psql_with_header`）で
//! 行う。
//!
//! **台帳のプロセス・表層横断永続**: DB-S での SQL 実行後にプロセスを
//! SIGKILL し、同じ DB ファイルで NoSQL 表層を起動して同一 `operation_id`
//! を再送すると `23505`／`22023` になること（逆方向も同様）を確認し、台帳が
//! プロセス再起動・表層切替をまたいで永続することの非 vacuous な証跡とする
//! （層 A の `nosql12_update_delete.rs` は同一プロセス内の 2 core 比較に
//! 留まるため、この永続性は本ファイルが固有に検証する）。
//!
//! **スコープ外**: 述語つき `UPDATE ... WHERE`／`DELETE ... WHERE`
//! （NoSQL `filter` は Issue #1062 で実行結線済みだが、本ファイル特有の
//! 複数プロセス・複数言語クライアント e2e への拡張はスコープ外。パリティ・
//! 台帳照合・RLS-9 は層 A `nosql12_update_delete.rs` の述語形節が担う）・
//! `RETURNING`・UPSERT・複数行 `INSERT` は NoSQL 表層が公開していないため
//! パリティが成立せず対象外。ヘッダを含む HTTP 応答全体のバイト同一性は
//! 層 A（`nosql12_update_delete.rs::strip_date` 比較）の担当で、本ファイルは
//! ステータス・本文までの一致に留める。
//!
//! ## パリティ総合検証（Issue #950）
//!
//! 上記 3 シナリオ（#779・#877）に加え、Phase 7（親 #944。#945〜#949・
//! #1061〜#1063）の追加機能・insert・DDL 3 op を含む
//! `wire_server::http::query::op::Op::ALL`（9 語彙）全体へ層 B 到達性を
//! 持たせる。範囲比較・`IN`・`OR`・`sort`・`offset`・複数列 `group_by`は
//! `PARITY_CASES` への追加（`run_sql_nosql_parity_scenario` を再利用）、
//! 拒否ケース（`sort`／`offset` on `search`/`aggregate`・語彙外 `op`・RLS
//! 述語名 `filter`）は `REJECTION_CASES`、insert・DDL 3 op・述語形
//! `delete` は `run_phase7_write_parity_scenario` がそれぞれ担う。
//! `parity_matrix_covers_every_nosql_op`（`#[ignore]` なし・常時
//! `make ci`）が `Op::ALL` 全体の層 B 網羅を機械的に固定する。詳細な
//! スコープ縮小の判断（述語形 DML の RLS-9 応答同一性・3 テナント横断は
//! 対象外にした理由等）は `docs/design/three-client-e2e-harness.md`
//! 「パリティ総合検証（Issue #950）」節に記録する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::collections::BTreeMap;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::json::{parse_json, JsonNumber, JsonValue};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

/// 環境変数 `CURL_BIN` で上書きできるツール解決。未指定時は `PATH` 上の
/// `curl` を使う（`three_client_e2e.rs::resolve_tool` と同型）。ツール自体の
/// 存在確認はしない（`Command::spawn` の失敗として顕在化させ、呼び出し元が
/// 案内メッセージ付きで panic する）。
fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `three_client_e2e.rs::seed_three_tenant_db` と同一内容（`docs` テーブル・
/// 3 テナント × Public 1 行）の一時 DB を用意する。呼び出し元がこの関数を
/// 抜けた時点で `Storage`（redb は単一ライター）は drop 済みのため、直後に
/// 子プロセス（同じ DB ファイルを開く `wire-server`）を安全に起動できる。
fn seed_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-http-e2e-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let tenants: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in tenants {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (path, guard)
}

/// `spawn_sql_server`／`spawn_nosql_server`／それぞれの `_with_ddl_allowed`
/// 変種（Issue #950・DDL パリティ）が共有する起動ロジック。`extra_args` は
/// `--ddl-allowed-users <users>` のような追加フラグ（無ければ空スライス）。
fn spawn_server_with_args(
    users_path: &str,
    db_path: &str,
    surface_nosql: bool,
    extra_args: &[&str],
) -> (common::SpawnedServer, u16) {
    let mut args: Vec<&str> = vec![
        "--users",
        users_path,
        "--db",
        db_path,
        "--bind",
        "127.0.0.1:0",
    ];
    if surface_nosql {
        args.push("--surface");
        args.push("nosql");
    }
    args.extend_from_slice(extra_args);
    let mut server = common::SpawnedServer::spawn(&args);

    let deadline = Instant::now() + Duration::from_secs(10);
    let addr_str = server.wait_for_listening(deadline);
    let addr_str = match addr_str {
        Some(addr) => addr,
        None => {
            let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
            panic!(
                "wire-server (surface_nosql={surface_nosql}) did not report a listening \
                 address within the deadline; stderr so far: {seen:?}"
            );
        }
    };
    let addr: std::net::SocketAddr = addr_str
        .parse()
        .unwrap_or_else(|e| panic!("invalid listening address {addr_str:?}: {e}"));
    (server, addr.port())
}

/// `wire-server --surface nosql` を子プロセスとして起動し、stderr の
/// `listening on` 行から実 bind ポートを取得する（`common::SpawnedServer` を
/// 再利用。取得できなければ蓄積 stderr 行を添えて `panic!`）。
fn spawn_nosql_server(users_path: &str, db_path: &str) -> (common::SpawnedServer, u16) {
    spawn_server_with_args(users_path, db_path, true, &[])
}

/// `wire-server`（SQL 表層。`--surface` を渡さない既定分岐）を子プロセスとして
/// 起動する（Issue #779。`spawn_nosql_server` と同型だが `--surface nosql` を
/// 渡さない点のみが異なる）。
fn spawn_sql_server(users_path: &str, db_path: &str) -> (common::SpawnedServer, u16) {
    spawn_server_with_args(users_path, db_path, false, &[])
}

/// `spawn_nosql_server` に `--ddl-allowed-users <ddl_users>` を追加した変種
/// （Issue #950・DDL パリティ。`ddl_users` はカンマ区切りの username 列）。
fn spawn_nosql_server_with_ddl_allowed(
    users_path: &str,
    db_path: &str,
    ddl_users: &str,
) -> (common::SpawnedServer, u16) {
    spawn_server_with_args(
        users_path,
        db_path,
        true,
        &["--ddl-allowed-users", ddl_users],
    )
}

/// `spawn_sql_server` に `--ddl-allowed-users <ddl_users>` を追加した変種
/// （Issue #950・DDL パリティ）。
fn spawn_sql_server_with_ddl_allowed(
    users_path: &str,
    db_path: &str,
    ddl_users: &str,
) -> (common::SpawnedServer, u16) {
    spawn_server_with_args(
        users_path,
        db_path,
        false,
        &["--ddl-allowed-users", ddl_users],
    )
}

/// SQL 経路パリティ（Issue #779）専用の一時 DB を用意する
/// （`three_client_e2e.rs::seed_aggregate_three_tenant_db` と同一内容の複製。
/// private 関数のため import できないため、`extended_syntax_e2e.rs` の前例と
/// 同方針でこのファイル専用に複製する）。`docs` テーブルに 3 テナント各 1 件の
/// Public 行（id=1/2/3）と、tenant-a・tenant-b それぞれの Private 行
/// （id=11 lang="xx"／id=12 lang="ja"）を投入する。wire 認証経路が導出する
/// `PolicyContext` は `Public` ＋ 自テナントの `Private` を許可可視性とする
/// （RLS-11・TASK-195。read-your-writes）ため、tenant-a（alice）は id=11 を、
/// tenant-b（bob）は id=12 を自身の接続で見る——SQL 経路・NoSQL 経路の
/// いずれの応答でも一致することを `ParityCase::expected_rows_{alice,bob}`
/// で検証する。**他テナント**の接続には両表層のどの応答にも現れないこと
/// （tenant-c／carol はどちらも見えない）をパリティ検証の非漏えい証跡に
/// 使う。
/// `category`（`TEXT`）・`price`（`NUMERIC(10,2)`）は Issue #950（Phase 7
/// パリティ総合検証）向けに追加した列——範囲比較（`gte`）・`IN`・`OR`・
/// `sort`・`offset`・複数列 `group_by`（#945・#946・#947・#949）を NUMERIC
/// 列・複数列で検証するための材料であり、既存 10 ケース（Issue #779）が
/// 参照する列（`id`／`lang`／`embedding`）には影響しない（既存
/// `expected_rows_*` は不変）。`price` は本ファイルのどのケースからも
/// `columns`（投影）へは含めない——NoSQL `columns[].type` の期待値表
/// （`run_sql_nosql_parity_scenario` 内 `expected_nosql_types`）が
/// `id`／`embedding` 以外を一律 `"text"` とみなす簡易表のままのため、
/// `NUMERIC` 列を投影すると型名の対称性チェックが誤って失敗する
/// （`WHERE`／`filter` 条件としてのみ使う分には型名比較を経由しない）。
fn seed_parity_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-http-e2e-parity-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("category", ColumnType::Text, false),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
            ],
        ))
        .expect("create table");
    // clippy::type_complexity 対策のタプル型エイリアス（Issue #950 の
    // `category`／`price` 列追加でタプルの要素数が増えたため）。
    type PublicRowSpec = (
        &'static str,
        u64,
        [f32; 2],
        &'static str,
        &'static str,
        &'static str,
        i128,
    );
    let public_rows: [PublicRowSpec; 3] = [
        (
            "tenant-a",
            1,
            [1.0, 0.0],
            "ja",
            "vector database intro",
            "A",
            350,
        ),
        (
            "tenant-b",
            2,
            [0.0, 1.0],
            "en",
            "query planning notes",
            "B",
            725,
        ),
        (
            "tenant-c",
            3,
            [-1.0, 0.0],
            "ja",
            "unrelated topic",
            "A",
            999,
        ),
    ];
    for (tenant, id, dir, lang, body, category, price_unscaled) in public_rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
                Value::Text(category.to_string()),
                Value::Numeric(
                    engine::numeric::Decimal::from_parts(price_unscaled, 2).expect("valid decimal"),
                ),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    type PrivateRowSpec = (
        &'static str,
        u64,
        [f32; 2],
        &'static str,
        &'static str,
        i128,
    );
    let private_rows: [PrivateRowSpec; 2] = [
        ("tenant-a", 11, [1.0, 0.0], "xx", "A", 100),
        ("tenant-b", 12, [0.0, 1.0], "ja", "B", 800),
    ];
    for (tenant, id, dir, lang, category, price_unscaled) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text("private body".to_string()),
                Value::Text(category.to_string()),
                Value::Numeric(
                    engine::numeric::Decimal::from_parts(price_unscaled, 2).expect("valid decimal"),
                ),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op-private")
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }
    (path, guard)
}

/// NULL セル比較用の番兵文字列（本 seed の値としては現れない固定文字列。
/// `-P null=<sentinel>` と `json_cell_to_pg_text` の双方に渡し NULL と空
/// 文字列を区別する。本 seed は NULL を生まないため防御的措置にとどまり、
/// NULL パリティそのものを検証したとは主張しない）。
const NULL_SENTINEL: &str = "<<NULL>>";

/// psql（無改造。`-A`・ヘッダ付き・`-F '|'`・`-P footer=off`）で 1 文を実行し、
/// ヘッダ行（列名）とデータ行（セルの文字列表現）を返す。拡張クエリ
/// プロトコルは未対応（`0A000`）のため `\gdesc` は使えず、型情報はここでは
/// 取得しない（`sql_column_types_via_raw_wire` が別途担う）。
fn run_psql_with_header(
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
) -> (Vec<String>, Vec<Vec<String>>) {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let args: Vec<String> = vec![
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        port.to_string(),
        "-U".into(),
        user.into(),
        "-d".into(),
        "irrelevant-db-name".into(),
        "-X".into(),
        "-w".into(),
        "-q".into(),
        "-A".into(),
        "-F".into(),
        "|".into(),
        "-P".into(),
        "footer=off".into(),
        "-P".into(),
        format!("null={NULL_SENTINEL}"),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "-c".into(),
        sql.into(),
    ];
    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args(&args)
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {psql} (install libpq-client tools or set PSQL_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "psql exited non-zero for user {user} sql={sql:?}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let header_line = lines
        .next()
        .unwrap_or_else(|| panic!("psql produced no header line for sql={sql:?}"));
    let header: Vec<String> = header_line.split('|').map(str::to_string).collect();
    let rows: Vec<Vec<String>> = lines
        .map(|line| line.split('|').map(str::to_string).collect())
        .collect();
    (header, rows)
}

/// 実際に使う `PSQL_BIN` 解決先の `--version` 出力を取得する（Issue #778 の
/// `HttpClient::version` と同じ様式。`[e2e-record]` 行へ転記する）。
fn psql_version() -> String {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn {psql} for --version: {e}; \
             install libpq-client tools or set PSQL_BIN to an alternative binary"
            )
        });
    if !output.status.success() {
        panic!(
            "{psql} --version exited non-zero (status={:?}): stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw_first_line = stdout.lines().next().unwrap_or("").to_string();
    let sanitized = sanitize_untrusted_first_line(&raw_first_line);
    if sanitized.trim().is_empty() {
        panic!("{psql} --version produced no usable output (stdout={stdout:?})");
    }
    sanitized
}

/// `common::read_row_description`（`common/mod.rs`）は型 OID を読み飛ばすが、
/// SQL 経路パリティの型比較（Issue #779）には OID が要る。共有ヘルパーの
/// シグネチャは変えず（他の全 wire テストへ影響するため）、このファイル
/// 専用のローカル変種として型 OID も保持する版を用意する。実装は
/// `common::read_row_description` と同じレイアウト解釈（列名 nul 終端 →
/// table oid(4) → attnum(2) → type oid(4) → typlen(2) → typmod(4) →
/// format(2)）を踏襲する。
fn read_row_description_with_oids(stream: &mut TcpStream) -> Vec<(String, i32)> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'T', "expected RowDescription");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len_i32 = i32::from_be_bytes(len_buf);
    // 長さフィールドを確保前に検証する（未検証のまま `as usize` すると負値が
    // 巨大な正値へ化ける。coding-rust.md「untrusted 入力の扱い」参照）。
    assert!(
        (4..=(1 << 20)).contains(&len_i32),
        "invalid RowDescription length {len_i32}"
    );
    let len = len_i32 as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let nul = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated column name");
        let name = std::str::from_utf8(&body[pos..pos + nul])
            .expect("utf8 column name")
            .to_string();
        pos += nul + 1;
        pos += 4 + 2; // table oid, attnum
        let oid = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        pos += 2 + 4 + 2; // typlen, typmod, format
        fields.push((name, oid));
    }
    fields
}

/// [`crate::result_encoder::WireType`] の公告と同一の固定表（Issue #779）。
/// このモジュールドキュメントの「型の観測方法」節が述べるとおり、表に無い
/// OID は fail-closed に `panic!` する（新しい型 OID の追加は本テストの
/// 更新を要する変更として顕在化させる）。
fn pg_type_name_of_oid(oid: i32) -> &'static str {
    match oid {
        1700 => "numeric",
        25 => "text",
        23 => "int4",
        20 => "int8",
        // WIRE-13・TASK-200・Issue #895 で追加された型。この固定表は
        // `result_encoder::WireType` とは独立に持つ契約（単一情報源からの
        // 逆引きの妥当性検証）のため、新型を使う fixture が増えたら本表も
        // 更新する。
        16 => "bool",
        700 => "float4",
        701 => "float8",
        1082 => "date",
        1114 => "timestamp",
        17 => "bytea",
        2950 => "uuid",
        114 => "json",
        3802 => "jsonb",
        other => panic!("unexpected type OID {other} (fixed table incomplete)"),
    }
}

/// 認証済み・ReadyForQuery 到達済みの接続を新規に張り、簡易クエリ 1 文を
/// 送って `RowDescription` の列名・型名を採取する（Issue #779）。後続の
/// `DataRow`* → `CommandComplete` → `ReadyForQuery` は読み捨てる
/// （`common::authenticate_to_ready_for_query` と同じ接続を型採取専用として
/// 使い切ったあと `TcpStream` は drop でクローズされる。明示的な
/// Terminate は他の wire テストと同様に送らない）。
fn sql_column_types_via_raw_wire(
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
) -> Vec<(String, &'static str)> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("valid loopback addr");
    let mut stream = common::authenticate_to_ready_for_query(addr, user, password);
    common::send_simple_query(&mut stream, sql);
    let fields = read_row_description_with_oids(&mut stream);

    let mut msg_type = common::read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(
            safety < 10_000,
            "too many messages after RowDescription for sql={sql:?}"
        );
        msg_type = common::read_message_type_discarding_body(&mut stream);
    }

    fields
        .into_iter()
        .map(|(name, oid)| (name, pg_type_name_of_oid(oid)))
        .collect()
}

/// `JsonNumber` の 3 variant を、両表層が共有する Rust `Display` 経由の
/// 10 進テキストへ揃える（`Cell::Integer`/`Cell::Float` の `to_string()`／
/// `{f}` と同じ表現。整数値の浮動小数は小数点無しの表記になる——例えば
/// `AVG(id)` の `2.0` は両表層とも `"2"` になる）。
fn format_json_number(n: &JsonNumber) -> String {
    match n {
        JsonNumber::PosInt(v) => v.to_string(),
        JsonNumber::NegInt(v) => v.to_string(),
        JsonNumber::Float { value, .. } => value.to_string(),
    }
}

/// NoSQL 応答の 1 JSON セルを psql のテキスト表現へ正規化する（Issue #779。
/// モジュールドキュメント「正規化モデル」節参照）。`VECTOR` 列・式項目の
/// 型名がいずれも `"text"` として公告される一方、値自体は native JSON で
/// 届く表現差はここで吸収する（型名の非対称そのものは本関数の対象外で、
/// `columns[].type` の比較で別途検査する）。本 seed に現れない
/// `Bool`／`Object` セルは防御的に `panic!` する。
fn json_cell_to_pg_text(cell: &JsonValue, null_sentinel: &str) -> String {
    match cell {
        JsonValue::Null => null_sentinel.to_string(),
        JsonValue::Number(n) => format_json_number(n),
        JsonValue::String(s) => s.clone(),
        JsonValue::Array(items) => {
            let joined = items
                .iter()
                .map(|item| match item {
                    JsonValue::Number(n) => format_json_number(n),
                    other => panic!("unexpected vector element JSON type: {other:?}"),
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("[{joined}]")
        }
        JsonValue::Bool(_) | JsonValue::Object(_) => {
            panic!("unexpected JSON cell type for this fixture: {cell:?}")
        }
    }
}

/// SQL 経路パリティ（Issue #779）の 1 クエリ意図を表す。`sql` は psql・生
/// wire 型採取の双方に、`json_body` は NoSQL `/v1/query` にそれぞれ渡す
/// リテラル定数（クライアント応答由来の文字列を SQL／JSON へ連結しない）。
/// `ordered` は `scan`（SQL-15。順序保証なし）のみ `false` にする。
///
/// `expected_rows_{alice,bob,carol}` は両表層が同じ誤りを返すケースを排除
/// するための固定オラクル（`docs/spec` 非依存・`seed_parity_db` に対する
/// `EngineCore::execute_sql_in_session` 直接呼び出しで手計算した値）。
/// wire 認証経路の `PolicyContext` は `Public` ＋ 自テナントの `Private` を
/// 許可可視性とする（RLS-11・TASK-195。read-your-writes）ため、tenant-a
/// （alice）は自身の Private 行（id=11）を、tenant-b（bob）は自身の
/// Private 行（id=12）をそれぞれの応答に含む。carol は Private 行を持たない
/// ため元の値のまま不変（3 テナント共通の単一 `expected_rows` だった旧版
/// との差はこの 2 テナントの追加行のみ）。
struct ParityCase {
    label: &'static str,
    sql: &'static str,
    json_body: &'static str,
    ordered: bool,
    expected_rows_alice: &'static [&'static [&'static str]],
    expected_rows_bob: &'static [&'static [&'static str]],
    expected_rows_carol: &'static [&'static [&'static str]],
}

impl ParityCase {
    /// テナント別固定オラクルをユーザー名から選択する（`user` は
    /// `run_sql_nosql_parity_scenario` の `USERS` 定数由来の既知の 3 値のみ）。
    fn expected_rows(&self, user: &str) -> &'static [&'static [&'static str]] {
        match user {
            "alice" => self.expected_rows_alice,
            "bob" => self.expected_rows_bob,
            "carol" => self.expected_rows_carol,
            other => panic!("unknown parity user: {other}"),
        }
    }
}

/// `nosql-api.md`「SQL ↔ NoSQL 対応表」に対応する 10 ケース（Issue #779）。
const PARITY_CASES: &[ParityCase] = &[
    ParityCase {
        label: "search1",
        sql: "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
        json_body: r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"]}"#,
        ordered: true,
        expected_rows_alice: &[&["1"], &["11"], &["2"]],
        expected_rows_bob: &[&["1"], &["2"], &["12"]],
        expected_rows_carol: &[&["1"], &["2"], &["3"]],
    },
    ParityCase {
        label: "search2",
        sql: "SELECT id, lang FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
        json_body: r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        ordered: true,
        // alice の own Private 行（id=11）は lang="xx" のため `lang = 'ja'`
        // に一致せず、この形状は alice でも seed のまま不変。
        expected_rows_alice: &[&["1", "ja"], &["3", "ja"]],
        expected_rows_bob: &[&["1", "ja"], &["12", "ja"], &["3", "ja"]],
        expected_rows_carol: &[&["1", "ja"], &["3", "ja"]],
    },
    ParityCase {
        label: "search3",
        sql: "SELECT id, lang, embedding FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
        json_body: r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id","lang","embedding"]}"#,
        ordered: true,
        expected_rows_alice: &[
            &["1", "ja", "[1,0]"],
            &["11", "xx", "[1,0]"],
            &["2", "en", "[0,1]"],
        ],
        expected_rows_bob: &[
            &["1", "ja", "[1,0]"],
            &["2", "en", "[0,1]"],
            &["12", "ja", "[0,1]"],
        ],
        expected_rows_carol: &[
            &["1", "ja", "[1,0]"],
            &["2", "en", "[0,1]"],
            &["3", "ja", "[-1,0]"],
        ],
    },
    ParityCase {
        label: "search4",
        sql: "SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'zzz-term-absent-from-any-seed-body') LIMIT 3",
        json_body: r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"],"hybrid":{"text":"zzz-term-absent-from-any-seed-body"}}"#,
        ordered: true,
        // 語彙不一致の疎側項は寄与せず密のみ順位（`embedding <=>` 昇順）へ
        // 帰着する。own Private 行はクエリと同一ベクトルのため密側で
        // 最上位に入り、alice/bob それぞれで先頭へ現れる。
        expected_rows_alice: &[&["11"], &["1"], &["2"]],
        expected_rows_bob: &[&["12"], &["1"], &["2"]],
        expected_rows_carol: &[&["1"], &["2"], &["3"]],
    },
    // codex-review 指摘対応（PR #838）: search4 は語彙不一致の疎側項（どの
    // seed body にも出現しない語）を使うため、密のみ結果（search1）と偶然
    // 同一になり、`hybrid.text` を無視して密検索のみへ縮退する退行を本
    // 比較ハーネスが検出できない。search5 は id=3 の body（"unrelated
    // topic"）に実在する語 "unrelated" を疎側項に使い、密のみ順位
    // （id=1,2,3。距離昇順）とは異なる順位（id=3 が繰り上がる）を要求する
    // ことで、疎側チャネルが実際にランキングへ寄与していることを検出可能
    // にする（`expected_rows_*` は `crates/engine/tests/default_preset.rs`
    // と同じ `EngineCore::execute_sql_in_session` 直接呼び出しで実測して
    // 手計算・固定した値であり、`docs/spec` には依存しない）。
    ParityCase {
        label: "search5",
        sql: "SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'unrelated') LIMIT 3",
        json_body: r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"],"hybrid":{"text":"unrelated"}}"#,
        ordered: true,
        // own Private 行の body（"private body"）は疎側項 "unrelated" と
        // 無関係のため疎側で寄与しないが、alice の own Private 行（id=11）
        // は密側で id=1 と同一ベクトル（同一密スコア）のため RRF 融合後の
        // 順位が id=1 と近接し、carol と同じ並びだった 3 件目（id=2）を
        // 押し出して 3 件目に現れる（`EngineCore::execute_sql_in_session`
        // 直接呼び出しで実測して固定）。bob の own Private 行（id=12）は
        // 同水準の近接が生じず carol と同じ並びのまま不変。
        expected_rows_alice: &[&["3"], &["1"], &["11"]],
        expected_rows_bob: &[&["3"], &["1"], &["2"]],
        expected_rows_carol: &[&["3"], &["1"], &["2"]],
    },
    ParityCase {
        label: "scan1",
        sql: "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10",
        json_body: r#"{"op":"scan","table":"docs","limit":10,"columns":["id","lang"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        ordered: false,
        // alice の own Private 行は lang="xx" のため `lang = 'ja'` に一致
        // せず、seed のまま不変。
        expected_rows_alice: &[&["1", "ja"], &["3", "ja"]],
        expected_rows_bob: &[&["1", "ja"], &["12", "ja"], &["3", "ja"]],
        expected_rows_carol: &[&["1", "ja"], &["3", "ja"]],
    },
    ParityCase {
        label: "agg1",
        sql: "SELECT COUNT(*), SUM(id), AVG(id), MIN(lang), MAX(lang) FROM docs",
        json_body: r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"},{"fn":"sum","column":"id"},{"fn":"avg","column":"id"},{"fn":"min","column":"lang"},{"fn":"max","column":"lang"}]}"#,
        ordered: true,
        expected_rows_alice: &[&["4", "17", "4.25", "en", "xx"]],
        expected_rows_bob: &[&["4", "18", "4.5", "en", "ja"]],
        expected_rows_carol: &[&["3", "6", "2", "en", "ja"]],
    },
    ParityCase {
        label: "agg2",
        sql: "SELECT COUNT(*) FROM docs WHERE lang = 'ja'",
        json_body: r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        ordered: true,
        // alice の own Private 行は lang="xx" のため不変。
        expected_rows_alice: &[&["2"]],
        expected_rows_bob: &[&["3"]],
        expected_rows_carol: &[&["2"]],
    },
    ParityCase {
        label: "agg3",
        sql: "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        json_body: r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"]}"#,
        ordered: true,
        expected_rows_alice: &[&["en", "1"], &["ja", "2"], &["xx", "1"]],
        expected_rows_bob: &[&["en", "1"], &["ja", "3"]],
        expected_rows_carol: &[&["en", "1"], &["ja", "2"]],
    },
    ParityCase {
        label: "agg4",
        sql: "SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count >= 2",
        json_body: r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang"],"having":[{"fn":"count","column":"*","op":">=","value":2}]}"#,
        ordered: true,
        expected_rows_alice: &[&["ja", "2"]],
        expected_rows_bob: &[&["ja", "3"]],
        expected_rows_carol: &[&["ja", "2"]],
    },
    // ここから Issue #950（Phase 7 パリティ総合検証）追加分。範囲比較・`IN`・
    // `OR`（#945・NOSQL-14）・`sort`（#946・NOSQL-15）・`offset`（#947・
    // NOSQL-15）・複数列 `group_by`（#949・NOSQL-16）を `seed_parity_db` の
    // `category`／`price` 列で検証する。`ordered: false` にした集計ケース
    // （range-or・multi-group-by）はエンジン内部の既定キー順を仮定せず、
    // 多重集合一致のみを固定オラクルとして要求する（安全側の判断）。
    ParityCase {
        label: "range-price-gte",
        sql: "SELECT id FROM docs WHERE price >= '7.00' LIMIT 10",
        json_body: r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"price","op":"gte","value":7}]}"#,
        ordered: false,
        expected_rows_alice: &[&["2"], &["3"]],
        expected_rows_bob: &[&["2"], &["3"], &["12"]],
        expected_rows_carol: &[&["2"], &["3"]],
    },
    ParityCase {
        label: "in-category",
        sql: "SELECT id FROM docs WHERE category IN ('A') LIMIT 10",
        json_body: r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"column":"category","op":"in","value":["A"]}]}"#,
        ordered: false,
        expected_rows_alice: &[&["1"], &["3"], &["11"]],
        expected_rows_bob: &[&["1"], &["3"]],
        expected_rows_carol: &[&["1"], &["3"]],
    },
    ParityCase {
        label: "or-lang-or-price",
        sql: "SELECT id FROM docs WHERE (lang = 'en' OR price >= '7.00') LIMIT 10",
        json_body: r#"{"op":"scan","table":"docs","limit":10,"columns":["id"],"filter":[{"or":[{"column":"lang","op":"eq","value":"en"},{"column":"price","op":"gte","value":7}]}]}"#,
        ordered: false,
        expected_rows_alice: &[&["2"], &["3"]],
        expected_rows_bob: &[&["2"], &["3"], &["12"]],
        expected_rows_carol: &[&["2"], &["3"]],
    },
    ParityCase {
        label: "sort-category-id",
        sql: "SELECT id, category FROM docs ORDER BY category ASC, id ASC LIMIT 10",
        json_body: r#"{"op":"scan","table":"docs","limit":10,"columns":["id","category"],"sort":[{"column":"category","dir":"asc"},{"column":"id","dir":"asc"}]}"#,
        ordered: true,
        expected_rows_alice: &[&["1", "A"], &["3", "A"], &["11", "A"], &["2", "B"]],
        expected_rows_bob: &[&["1", "A"], &["3", "A"], &["2", "B"], &["12", "B"]],
        expected_rows_carol: &[&["1", "A"], &["3", "A"], &["2", "B"]],
    },
    ParityCase {
        label: "offset-sorted-id",
        sql: "SELECT id FROM docs ORDER BY id ASC LIMIT 10 OFFSET 1",
        json_body: r#"{"op":"scan","table":"docs","limit":10,"offset":1,"columns":["id"],"sort":[{"column":"id","dir":"asc"}]}"#,
        ordered: true,
        expected_rows_alice: &[&["2"], &["3"], &["11"]],
        expected_rows_bob: &[&["2"], &["3"], &["12"]],
        expected_rows_carol: &[&["2"], &["3"]],
    },
    ParityCase {
        label: "group-by-lang-category",
        sql: "SELECT lang, category, COUNT(*) FROM docs GROUP BY lang, category",
        json_body: r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"group_by":["lang","category"]}"#,
        ordered: false,
        expected_rows_alice: &[&["en", "B", "1"], &["ja", "A", "2"], &["xx", "A", "1"]],
        expected_rows_bob: &[&["en", "B", "1"], &["ja", "A", "2"], &["ja", "B", "1"]],
        expected_rows_carol: &[&["en", "B", "1"], &["ja", "A", "2"]],
    },
];

/// Issue #950 §3.6「拒否ケース」: NoSQL 表層が意図的に受理しない形
/// （`nosql-api.md`「対応の無いもの」節）を alice 1 テナントで固定する
/// （`(label, json_body, expected_wire_code)`）。`search`／`aggregate` への
/// `sort`／`offset`（#946・#947 の対象外化）・語彙外 `op`・`filter` への
/// RLS 述語名指定（`.claude/rules/security.md` P0）を含む。
const REJECTION_CASES: &[(&str, &str, &str)] = &[
    (
        "reject-sort-on-search",
        r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"],"sort":[{"column":"id","dir":"asc"}]}"#,
        "42601",
    ),
    (
        "reject-offset-on-aggregate",
        r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],"offset":1}"#,
        "42601",
    ),
    (
        "reject-unknown-op",
        r#"{"op":"begin","table":"docs"}"#,
        "0A000",
    ),
    (
        "reject-rls-predicate-filter",
        r#"{"op":"scan","table":"docs","limit":10,"filter":[{"column":"visible","op":"eq","value":"x"}]}"#,
        "42601",
    ),
];

/// curl 応答の本文上限（untrusted な外部プロセス出力を無制限に読み込まない
/// ための固定上限。本テストが扱う応答は高々数百バイトで十分足りる）。
/// curl 自体にも `--max-filesize` として同じ上限を課し、上限超過時は
/// ディスクへ書き切ってから事後検査するのではなく curl の時点で転送を
/// 中断させる（fail-closed。curl は非 0 終了で応答し既存の非 0 終了検査で
/// `panic!` される）。
const MAX_RESPONSE_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// `curl` を 1 要求 = 1 プロセスで起動し、HTTP ステータスコードと応答本文を
/// 返す（NoSQL 表層は `Connection: close` 固定のため `--next` 連結はしない）。
/// `Command` の引数配列で起動しシェルを介さない。curl の spawn 失敗・非 0
/// 終了はいずれも案内メッセージ付き `panic!`（silent skip しない）。
///
/// トークン（`Authorization` ヘッダ）・パスワードを含む要求本文は、
/// `ps`／プロセス一覧経由で一時的にでも観測されないよう `Command` の
/// 引数へ直接載せず、`out_dir` 配下の一時ファイル経由（ヘッダは `-H @file`・
/// 本文は `--data-binary @file`）で curl へ渡す。
fn curl_post(
    port: u16,
    target: &str,
    bearer: Option<&str>,
    json_body: &str,
    out_dir: &std::path::Path,
    seq: u32,
) -> (u16, String) {
    let out_path = out_dir.join(format!("{seq}.body"));
    let body_path = out_dir.join(format!("{seq}.req.json"));
    let headers_path = out_dir.join(format!("{seq}.headers"));
    let url = format!("http://127.0.0.1:{port}{target}");

    std::fs::write(&body_path, json_body)
        .unwrap_or_else(|e| panic!("failed to write curl request body {body_path:?}: {e}"));

    // 100-continue の余地を消す（本文が小さく即座に送れるため不要）。
    let mut headers = String::from("Content-Type: application/json\nExpect:\n");
    if let Some(token) = bearer {
        headers.push_str(&format!("Authorization: Bearer {token}\n"));
    }
    std::fs::write(&headers_path, &headers)
        .unwrap_or_else(|e| panic!("failed to write curl header file {headers_path:?}: {e}"));

    let args: Vec<String> = vec![
        "-sS".to_string(),
        "--max-time".to_string(),
        "10".to_string(),
        "--max-filesize".to_string(),
        MAX_RESPONSE_BODY_BYTES.to_string(),
        "-H".to_string(),
        format!("@{}", headers_path.to_str().expect("utf-8 headers path")),
        "--data-binary".to_string(),
        format!("@{}", body_path.to_str().expect("utf-8 body path")),
        "-o".to_string(),
        out_path.to_str().expect("utf-8 out path").to_string(),
        "-w".to_string(),
        "%{http_code}".to_string(),
        url,
    ];

    let curl_bin = resolve_tool("CURL_BIN", "curl");
    let output = Command::new(&curl_bin)
        .args(&args)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn curl (bin={curl_bin:?}): {e}; \
             install curl or set CURL_BIN to an alternative binary"
            )
        });
    if !output.status.success() {
        panic!(
            "curl exited non-zero (status={:?}) for target {target}: stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let status: u16 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| {
            panic!(
                "curl did not report a numeric http status for target {target}: {e} \
                 (stdout={:?})",
                String::from_utf8_lossy(&output.stdout)
            )
        });

    let meta = std::fs::metadata(&out_path)
        .unwrap_or_else(|e| panic!("curl response body missing at {out_path:?}: {e}"));
    assert!(
        meta.len() <= MAX_RESPONSE_BODY_BYTES,
        "curl response body for target {target} exceeds the test's size limit: {} bytes",
        meta.len()
    );
    let mut file = std::fs::File::open(&out_path)
        .unwrap_or_else(|e| panic!("failed to open curl response body {out_path:?}: {e}"));
    let mut body = String::new();
    file.read_to_string(&mut body)
        .unwrap_or_else(|e| panic!("curl response body for target {target} is not utf-8: {e}"));

    (status, body)
}

/// curl 応答・要求本文の一時保存先ディレクトリを `Drop` で確実に削除する
/// ガード（`temp_db::CleanupGuard` と同じ idiom。テスト内 `assert!` の
/// panic 経路でも解放されるようにする）。
struct CurlOutDirGuard(PathBuf);

impl Drop for CurlOutDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `tests/three_client_http/` 配下のスクリプトをインタプリタの子プロセスとして
/// 1 要求 = 1 プロセスで起動する共通実装（`three_client_e2e.rs::spawn_psycopg_client`
/// と同型）。接続先・要求本文・bearer はすべて `HTTP_*` 環境変数で渡し
/// （argv・stdin は使わない。security.md P0）、スクリプトの契約
/// （`tests/three_client_http/urllib_client.py`／`fetch_client.js` の
/// モジュール先頭コメント参照）どおり stdout 1 行目のステータス・2 行目以降の
/// 本文を返す。
///
/// インタプリタの spawn 失敗（未インストール・`PYTHON_BIN`／`NODE_BIN` 誤設定）
/// は案内付きで `panic!` する。スクリプト自身の非 0 終了（転送路障害）は
/// 呼び出し元（`HttpClient::post`）が要求ステップの文脈で `panic!` する。
fn spawn_script_client(
    interpreter_env: &str,
    default_bin: &str,
    script_rel_path: &str,
    port: u16,
    target: &str,
    bearer: Option<&str>,
    json_body: &str,
) -> std::process::Output {
    let interpreter = resolve_tool(interpreter_env, default_bin);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(script_rel_path);
    let mut cmd = Command::new(&interpreter);
    cmd.arg(&script)
        .env("HTTP_HOST", "127.0.0.1")
        .env("HTTP_PORT", port.to_string())
        .env("HTTP_TARGET", target)
        .env("HTTP_BODY", json_body);
    if let Some(token) = bearer {
        cmd.env("HTTP_BEARER", token);
    }
    cmd.output().unwrap_or_else(|e| {
        panic!(
            "failed to spawn {default_bin} (bin={interpreter:?}): {e}; \
             install it or set {interpreter_env} to an alternative binary"
        )
    })
}

/// `spawn_script_client` の出力（stdout 1 行目のステータス・2 行目以降の本文）
/// を解析する。非 0 終了・非数値ステータス行はいずれも案内付き `panic!`
/// （untrusted なスクリプト出力を fail-closed に扱う。curl 経路の
/// `curl_post` と同じ検査水準）。
fn parse_script_client_output(
    client_label: &str,
    target: &str,
    output: &std::process::Output,
) -> (u16, String) {
    if !output.status.success() {
        panic!(
            "{client_label} exited non-zero (status={:?}) for target {target}: stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (status_line, rest) = stdout.split_once('\n').unwrap_or_else(|| {
        panic!("{client_label} produced no status line for target {target}: stdout={stdout:?}")
    });
    let status: u16 = status_line.trim().parse().unwrap_or_else(|e| {
        panic!(
            "{client_label} did not report a numeric http status for target {target}: {e} \
             (line={status_line:?})"
        )
    });
    (status, rest.to_string())
}

/// `urllib_client.py`（Python 標準ライブラリ `urllib.request`）で 1 要求を送る。
fn urllib_post(port: u16, target: &str, bearer: Option<&str>, json_body: &str) -> (u16, String) {
    let output = spawn_script_client(
        "PYTHON_BIN",
        "python3",
        "tests/three_client_http/urllib_client.py",
        port,
        target,
        bearer,
        json_body,
    );
    parse_script_client_output("urllib_client.py", target, &output)
}

/// `fetch_client.js`（Node.js 組み込み `fetch`）で 1 要求を送る。
fn fetch_post(port: u16, target: &str, bearer: Option<&str>, json_body: &str) -> (u16, String) {
    let output = spawn_script_client(
        "NODE_BIN",
        "node",
        "tests/three_client_http/fetch_client.js",
        port,
        target,
        bearer,
        json_body,
    );
    parse_script_client_output("fetch_client.js", target, &output)
}

/// 3 クライアント種別のディスパッチ（`run_session_search_close_scenario` が
/// クライアント非依存に書けるようにする薄い抽象化）。curl のみ
/// `out_dir`／`seq`（一時ファイル経由の要求本文・応答本文受け渡し）を使う。
enum HttpClient {
    Curl,
    Urllib,
    Fetch,
}

impl HttpClient {
    fn label(&self) -> &'static str {
        match self {
            HttpClient::Curl => "curl",
            HttpClient::Urllib => "urllib",
            HttpClient::Fetch => "fetch",
        }
    }

    fn post(
        &self,
        port: u16,
        target: &str,
        bearer: Option<&str>,
        json_body: &str,
        out_dir: &std::path::Path,
        seq: u32,
    ) -> (u16, String) {
        match self {
            HttpClient::Curl => curl_post(port, target, bearer, json_body, out_dir, seq),
            HttpClient::Urllib => urllib_post(port, target, bearer, json_body),
            HttpClient::Fetch => fetch_post(port, target, bearer, json_body),
        }
    }

    /// 実際に使うインタプリタ／バイナリの `--version` 出力を取得する
    /// （Issue #778。PR 記録テンプレートへ転記する「クライアント版」欄の
    /// 出所。`resolve_tool` が返す実際の解決先——環境変数上書きを含む——に
    /// 対して問い合わせるため、記録された版は実行に使われたものと一致する）。
    /// spawn 失敗・非 0 終了・空出力はいずれも案内付き `panic!`
    /// （ツール自体が必須のため fail-closed。silent skip・"unknown" 埋めは
    /// しない）。サーバー起動前（シナリオ冒頭）に呼び、ツール不在を早期に
    /// 判明させる想定。
    fn version(&self) -> String {
        let (env_var, default_bin) = match self {
            HttpClient::Curl => ("CURL_BIN", "curl"),
            HttpClient::Urllib => ("PYTHON_BIN", "python3"),
            HttpClient::Fetch => ("NODE_BIN", "node"),
        };
        let bin = resolve_tool(env_var, default_bin);
        let output = Command::new(&bin)
            .arg("--version")
            .output()
            .unwrap_or_else(|e| {
                panic!(
                    "failed to spawn {default_bin} (bin={bin:?}) for --version: {e}; \
                 install it or set {env_var} to an alternative binary"
                )
            });
        if !output.status.success() {
            panic!(
                "{bin} --version exited non-zero (status={:?}): stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // Python の一部ビルドは `--version` を stdout ではなく stderr へ
        // 出す（歴史的経緯。3.4 以降は stdout だが、環境差を吸収するため
        // stdout が空なら stderr にフォールバックする）。
        let stdout = String::from_utf8_lossy(&output.stdout);
        let raw_first_line = if stdout.trim().is_empty() {
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            stdout.lines().next().unwrap_or("").to_string()
        };
        let sanitized = sanitize_untrusted_first_line(&raw_first_line);
        if sanitized.trim().is_empty() {
            panic!(
                "{bin} --version produced no usable output (stdout={stdout:?}, stderr={:?}); \
                 install a version that prints a version string or set {env_var} to an \
                 alternative binary",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        sanitized
    }
}

/// untrusted な外部プロセス出力（`--version` の 1 行目）を、意味解釈せず
/// 印字可能 ASCII のみへ絞り上限 200 バイトへ切り詰める（Issue #778。
/// `[e2e-record]` 行・PR 本文への転記に使うため、制御文字・非 ASCII の
/// 混入を防ぐ）。
fn sanitize_untrusted_first_line(raw: &str) -> String {
    raw.chars()
        .filter(|c| (' '..='~').contains(c))
        .take(200)
        .collect()
}

fn json_object(body: &str) -> std::collections::BTreeMap<String, JsonValue> {
    match parse_json(body)
        .unwrap_or_else(|e| panic!("response body must be valid JSON: {e:?} (body={body:?})"))
    {
        JsonValue::Object(map) => map,
        other => panic!("expected JSON object, got {other:?}"),
    }
}

/// トークン形状（43 文字・base64url アルファベット限定）の検証（untrusted な
/// curl 応答由来の値を `Authorization` ヘッダへ流し込む前のガード。
/// `http4_session.rs` の同種検証と同じ基準）。
fn assert_valid_session_token(token: &str) {
    assert_eq!(token.len(), 43, "token must be 43 chars, got {token:?}");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "token must be base64url alphabet only, got {token:?}"
    );
}

/// 無改造の外部 HTTP クライアント（`client` で切替）で `POST /v1/session`
/// （発行）→ `POST /v1/query`（`op: search`）→ `POST /v1/session/close`
/// （失効）→ 失効後の同一トークン再送が `401`／`28000` で拒否されることまでを
/// 確認するスモークシナリオ（Issue #776 の受け入れ条件 R1〜R3。
/// urllib／fetch への拡張は Issue #777）。curl／urllib／fetch の 3 テストが
/// 本関数へ委譲することでシナリオ内容の重複を避ける。
///
/// 実行記録（`[e2e-record]`）の出力は Issue #778。SQL 経路とのパリティは
/// #779 のスコープ（本関数のスコープ外）。
fn run_session_search_close_scenario(client: HttpClient) {
    // サーバー起動前（シナリオ冒頭）に取得し、ツール不在を早期に判明させる
    // （Issue #778）。
    let client_version = client.version();

    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let db_path_str = db_path.to_str().expect("utf-8 db path").to_string();

    let (server, port) =
        spawn_nosql_server(users_path.to_str().expect("utf-8 users path"), &db_path_str);

    // curl のみ要求・応答本文をファイル経由で受け渡す（`curl_post` 参照）。
    // urllib／fetch は使わないが、`HttpClient::post` のシグネチャ統一のため
    // 引数として受け取る。
    let out_dir = std::env::temp_dir().join(format!(
        "wire-server-three-client-http-e2e-{}-out-{}-{}",
        client.label(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&out_dir).expect("create client output dir");
    let _out_dir_guard = CurlOutDirGuard(out_dir.clone());

    // 1. session 発行。
    let (status, body) = client.post(
        port,
        "/v1/session",
        None,
        r#"{"user":"alice","password":"pw-alice"}"#,
        &out_dir,
        1,
    );
    assert_eq!(status, 200, "session issue failed: {body}");
    let session_obj = json_object(&body);
    let token = match session_obj.get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    assert_valid_session_token(&token);
    let expires_in = match session_obj.get("expires_in") {
        Some(JsonValue::Number(n)) => n.as_f64(),
        other => panic!("expected numeric expires_in field, got {other:?}"),
    };

    // 2. search（alice は tenant-a・全行 Public のため RLS 暗黙適用でも
    //    3 行とも可視。クエリ [1,0] に対する cosine は id1=1.0／id2=0.0／
    //    id3=-1.0 のため id 昇順で [1,2,3] が返る想定。オラクルは engine 側
    //    テストが既に固定済みのため、本テストは「実バイナリ経由でも同じ
    //    行数・行集合が返る」非 vacuous な証跡に限定する）。
    let search_body =
        r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"]}"#;
    let (status, body) = client.post(port, "/v1/query", Some(&token), search_body, &out_dir, 2);
    assert_eq!(status, 200, "search query failed: {body}");
    let result_obj = json_object(&body);
    match result_obj.get("row_count") {
        Some(JsonValue::Number(n)) => assert_eq!(n.as_f64(), 3.0, "row_count: {body}"),
        other => panic!("expected numeric row_count field, got {other:?}"),
    }
    let rows = match result_obj.get("rows") {
        Some(JsonValue::Array(rows)) => rows.clone(),
        other => panic!("expected array rows field, got {other:?}"),
    };
    let mut ids: Vec<f64> = rows
        .iter()
        .map(|row| match row {
            JsonValue::Array(cells) => match cells.first() {
                Some(JsonValue::Number(n)) => n.as_f64(),
                other => panic!("expected numeric id cell, got {other:?}"),
            },
            other => panic!("expected array row, got {other:?}"),
        })
        .collect();
    ids.sort_by(|a, b| a.partial_cmp(b).expect("comparable ids"));
    assert_eq!(ids, vec![1.0, 2.0, 3.0], "unexpected id set: {body}");

    // 3. session close。
    let (close_status, close_body) =
        client.post(port, "/v1/session/close", Some(&token), "{}", &out_dir, 3);
    assert_eq!(close_status, 200, "session close failed: {close_body}");
    assert!(
        close_body.contains("\"closed\":true"),
        "expected closed:true, got: {close_body}"
    );

    // 4. 失効後の同一トークン再送は `401`／`28000` で拒否される（Issue #776
    //    の受け入れ条件 R3。close が実際にトークンを失効させたことの
    //    非 vacuous な証跡。`http8_session_close.rs` の同種検証と同じ
    //    `wire_code` 判定基準）。
    let (revoked_status, revoked_body) =
        client.post(port, "/v1/query", Some(&token), search_body, &out_dir, 4);
    assert_eq!(
        revoked_status, 401,
        "revoked token must be rejected: {revoked_body}"
    );
    assert!(
        revoked_body.contains("\"wire_code\":\"28000\""),
        "expected wire_code 28000 for revoked token, got: {revoked_body}"
    );

    let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let joined = seen.join("");
    // Issue #776 の受け入れ条件 R1: SQL 表層が誤って起動していないことの
    // 非 vacuous な証跡（`main.rs` は `--surface nosql` 選択時のみこの行を
    // `listening on` の直前に出す契約）。
    assert!(
        joined.contains("wire-server: surface nosql:"),
        "expected nosql surface banner in stderr, got: {joined:?}"
    );
    assert!(
        !joined.contains(&token),
        "stderr must not leak the session token"
    );
    assert!(
        !joined.contains("tenant-a"),
        "stderr must not leak tenant id"
    );
    assert!(!joined.contains("alice"), "stderr must not leak username");

    // 実行記録（Issue #778）: PR 本文の Test plan へ転記する 1 行を stderr へ
    // 出力する（`three_client_e2e.rs` の `[e2e-record]` 先例と同型）。
    // トークン・ユーザー名・パスワード・テナント id を一切含めないことを
    // 出力前に機械検証する（`--nocapture` で表示しても安全であることの保証。
    // 上記の stderr 非漏えい assert とは独立に、この行自体に対しても行う）。
    let record = format!(
        "[e2e-record] {label}: client_version={client_version:?} \
         session(status=200,expires_in={expires_in:?}) \
         search(status=200,row_count=3,ids={ids:?}) \
         close(status={close_status},closed=true) \
         revoked(status={revoked_status},wire_code=28000)",
        label = client.label(),
    );
    assert!(
        !record.contains(&token),
        "e2e-record line must not leak the session token"
    );
    assert!(
        !record.contains("alice"),
        "e2e-record line must not leak username"
    );
    assert!(
        !record.contains("tenant-a"),
        "e2e-record line must not leak tenant id"
    );
    assert!(
        !record.contains("pw-alice"),
        "e2e-record line must not leak password"
    );
    eprintln!("{record}");
}

#[test]
#[ignore = "requires curl; run via `make e2e-three-client-http`"]
fn curl_runs_session_search_close_over_nosql_surface() {
    run_session_search_close_scenario(HttpClient::Curl);
}

#[test]
#[ignore = "requires python3; run via `make e2e-three-client-http`"]
fn urllib_runs_session_search_close_over_nosql_surface() {
    run_session_search_close_scenario(HttpClient::Urllib);
}

#[test]
#[ignore = "requires node (>= 18); run via `make e2e-three-client-http`"]
fn fetch_runs_session_search_close_over_nosql_surface() {
    run_session_search_close_scenario(HttpClient::Fetch);
}

/// SQL 表層（psql）で 1 ケースぶんの行・型を採取した結果（Issue #779）。
struct SqlObservation {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    types: Vec<(String, &'static str)>,
}

/// 無改造の外部 HTTP クライアント（`client` で切替）で、`PARITY_CASES`
/// 10 ケースを 3 テナント（alice／bob／carol）それぞれで SQL 表層（psql・
/// 生 wire）と NoSQL 表層（`client`）の双方へ投げ、列名・型・行集合が
/// 一致することを確認するシナリオ（Issue #779。curl／urllib／fetch の
/// 3 テストが本関数へ委譲する）。モジュールドキュメント「SQL 経路
/// パリティ」節の起動方式・正規化モデルに従う。
fn run_sql_nosql_parity_scenario(client: HttpClient) {
    let client_version = client.version();
    let psql_version = psql_version();

    let (db_path, _db_guard) = seed_parity_db();
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let db_path_str = db_path.to_str().expect("utf-8 db path").to_string();
    let users_path_str = users_path.to_str().expect("utf-8 users path").to_string();

    const USERS: [(&str, &str); 3] = [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ];

    // 1. SQL 表層（`--surface` なし）で全ケース・全ユーザーぶんの psql 行・
    //    生 wire 型を採取する。redb は単一ライターのため、この後
    //    `stop_and_drain`（SIGKILL）で確実にプロセスを終了させてから
    //    NoSQL 表層を同じ DB ファイルで起動する。
    let (sql_server, sql_port) = spawn_sql_server(&users_path_str, &db_path_str);
    let mut sql_observations: BTreeMap<(&'static str, &'static str), SqlObservation> =
        BTreeMap::new();
    for (user, pw) in USERS {
        for case in PARITY_CASES {
            let (header, rows) = run_psql_with_header(sql_port, user, pw, case.sql);
            let types = sql_column_types_via_raw_wire(sql_port, user, pw, case.sql);
            sql_observations.insert(
                (user, case.label),
                SqlObservation {
                    header,
                    rows,
                    types,
                },
            );
        }
    }
    let sql_seen = sql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        !sql_seen.iter().any(|line| line.contains("surface nosql")),
        "SQL surface must not print the nosql surface banner: {sql_seen:?}"
    );

    // 2. NoSQL 表層（`--surface nosql`）で同じ DB ファイルを開き、`client` で
    //    全ケース・全ユーザーぶんの応答を採取して SQL 表層の観測値と比較する。
    let (nosql_server, nosql_port) = spawn_nosql_server(&users_path_str, &db_path_str);

    let out_dir = std::env::temp_dir().join(format!(
        "wire-server-three-client-http-e2e-parity-{}-out-{}-{}",
        client.label(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&out_dir).expect("create client output dir");
    let _out_dir_guard = CurlOutDirGuard(out_dir.clone());

    let mut seq: u32 = 0;
    let mut issued_tokens: Vec<String> = Vec::new();
    let mut case_summaries: Vec<String> = Vec::new();

    for (user, pw) in USERS {
        seq += 1;
        let (status, body) = client.post(
            nosql_port,
            "/v1/session",
            None,
            &format!(r#"{{"user":"{user}","password":"{pw}"}}"#),
            &out_dir,
            seq,
        );
        assert_eq!(status, 200, "session issue failed for {user}: {body}");
        let session_obj = json_object(&body);
        let token = match session_obj.get("token") {
            Some(JsonValue::String(s)) => s.clone(),
            other => panic!("expected string token field, got {other:?}"),
        };
        assert_valid_session_token(&token);
        issued_tokens.push(token.clone());

        for case in PARITY_CASES {
            seq += 1;
            let (status, body) = client.post(
                nosql_port,
                "/v1/query",
                Some(&token),
                case.json_body,
                &out_dir,
                seq,
            );
            assert_eq!(
                status, 200,
                "case={} user={user} query failed: {body}",
                case.label
            );
            let result_obj = json_object(&body);

            let columns = match result_obj.get("columns") {
                Some(JsonValue::Array(cols)) => cols.clone(),
                other => panic!(
                    "case={} user={user}: expected array columns field, got {other:?}",
                    case.label
                ),
            };
            let mut nosql_names: Vec<String> = Vec::with_capacity(columns.len());
            let mut nosql_types: Vec<String> = Vec::with_capacity(columns.len());
            for column in &columns {
                let JsonValue::Object(meta) = column else {
                    panic!(
                        "case={} user={user}: expected object column meta, got {column:?}",
                        case.label
                    );
                };
                match meta.get("name") {
                    Some(JsonValue::String(s)) => nosql_names.push(s.clone()),
                    other => panic!(
                        "case={} user={user}: expected string column name, got {other:?}",
                        case.label
                    ),
                }
                match meta.get("type") {
                    Some(JsonValue::String(s)) => nosql_types.push(s.clone()),
                    other => panic!(
                        "case={} user={user}: expected string column type, got {other:?}",
                        case.label
                    ),
                }
            }

            let row_count = match result_obj.get("row_count") {
                Some(JsonValue::Number(n)) => match n {
                    JsonNumber::PosInt(v) => *v,
                    other => panic!(
                        "case={} user={user}: expected non-negative integer row_count, got {other:?}",
                        case.label
                    ),
                },
                other => panic!(
                    "case={} user={user}: expected numeric row_count field, got {other:?}",
                    case.label
                ),
            };
            let rows_json = match result_obj.get("rows") {
                Some(JsonValue::Array(rows)) => rows.clone(),
                other => panic!(
                    "case={} user={user}: expected array rows field, got {other:?}",
                    case.label
                ),
            };
            let nosql_rows: Vec<Vec<String>> = rows_json
                .iter()
                .map(|row| match row {
                    JsonValue::Array(cells) => cells
                        .iter()
                        .map(|c| json_cell_to_pg_text(c, NULL_SENTINEL))
                        .collect(),
                    other => panic!(
                        "case={} user={user}: expected array row, got {other:?}",
                        case.label
                    ),
                })
                .collect();
            assert_eq!(
                row_count as usize,
                nosql_rows.len(),
                "case={} user={user}: row_count does not match rows.len()",
                case.label
            );

            // 非漏えい証跡: 他テナントの Private 行 id が応答に現れない
            // （RLS-11・TASK-195。read-your-writes により alice は自身の
            // id=11・bob は自身の id=12 を legitimate に見るため、ここでは
            // 「自分の id ではない方の Private id」のみを禁止する）。
            let forbidden_private_id = match user {
                "alice" => "12",
                "bob" => "11",
                // carol は Private 行を持たないため両方とも越境になる。
                _ => "11",
            };
            for row in &nosql_rows {
                assert!(
                    !row.iter().any(|cell| cell == forbidden_private_id),
                    "case={} user={user}: another tenant's private row id leaked into NoSQL \
                     response: {row:?}",
                    case.label
                );
                if user == "carol" {
                    assert!(
                        !row.iter().any(|cell| cell == "12"),
                        "case={} user={user}: another tenant's private row id leaked into \
                         NoSQL response: {row:?}",
                        case.label
                    );
                }
            }

            let sql_obs = sql_observations
                .get(&(user, case.label))
                .unwrap_or_else(|| {
                    panic!(
                        "missing SQL observation for case={} user={user}",
                        case.label
                    )
                });

            assert_eq!(
                nosql_names, sql_obs.header,
                "case={} user={user}: column name mismatch (SQL vs NoSQL)",
                case.label
            );
            let sql_type_column_names: Vec<&str> = sql_obs
                .types
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            assert_eq!(
                nosql_names.iter().map(String::as_str).collect::<Vec<_>>(),
                sql_type_column_names,
                "case={} user={user}: column name mismatch between wire RowDescription and \
                 NoSQL columns",
                case.label
            );
            // Issue #896（NOSQL-17）以降、NoSQL `columns[].type` は SQL wire
            // `RowDescription`（Issue #895）の OID 写像から意図的に独立した
            // 対応表を返す。本テストの schema（`embedding: VECTOR`・
            // `lang`/`body`: `TEXT`）では SQL wire 側が `VECTOR` 列を後方
            // 互換のため `text`（OID 25）へ丸める一方、NoSQL 側は
            // `"vector"` を返すため、列名から NoSQL 側の期待値を独立に
            // 導出して比較する（`nosql_type_name`〔`response.rs`〕と同じ
            // 対応。`docs/design/nosql-typed-json-binding.md` 参照）。
            let expected_nosql_types: Vec<&str> = nosql_names
                .iter()
                .map(|name| match name.as_str() {
                    "id" => "numeric",
                    "embedding" => "vector",
                    _ => "text",
                })
                .collect();
            assert_eq!(
                nosql_types, expected_nosql_types,
                "case={} user={user}: NoSQL columns[].type did not match the expected \
                 per-column-type table (Issue #896)",
                case.label
            );

            let expected_rows: Vec<Vec<String>> = case
                .expected_rows(user)
                .iter()
                .map(|row| row.iter().map(|cell| cell.to_string()).collect())
                .collect();
            if case.ordered {
                assert_eq!(
                    nosql_rows, sql_obs.rows,
                    "case={} user={user}: ordered row set mismatch (SQL vs NoSQL)",
                    case.label
                );
                assert_eq!(
                    nosql_rows, expected_rows,
                    "case={} user={user}: ordered row set does not match fixed oracle",
                    case.label
                );
            } else {
                let mut nosql_sorted = nosql_rows.clone();
                nosql_sorted.sort();
                let mut sql_sorted = sql_obs.rows.clone();
                sql_sorted.sort();
                assert_eq!(
                    nosql_sorted, sql_sorted,
                    "case={} user={user}: row multiset mismatch (SQL vs NoSQL)",
                    case.label
                );
                let mut expected_sorted = expected_rows;
                expected_sorted.sort();
                assert_eq!(
                    nosql_sorted, expected_sorted,
                    "case={} user={user}: row multiset does not match fixed oracle",
                    case.label
                );
            }

            case_summaries.push(format!(
                "{label}(rows={n},match=true)",
                label = case.label,
                n = nosql_rows.len()
            ));
        }

        // Issue #950 §3.6「拒否ケース」: SQL 表層には対応する形が無い（または
        // 意味的に別構文になる）ため SQL↔NoSQL 比較の対象外とし、NoSQL 側の
        // 固定 `wire_code` のみを alice 1 テナントで検証する（`nosql-api.md`
        // 「対応の無いもの」節・#945〜#949 の受け入れ条件に列挙された
        // 非対称。psql 側の受理確認は本ファイルのスコープ外——
        // `docs/design/three-client-e2e-harness.md` の Issue #950 節に記録）。
        if user == "alice" {
            for (label, json_body, expected_wire_code) in REJECTION_CASES {
                seq += 1;
                let (status, body) = client.post(
                    nosql_port,
                    "/v1/query",
                    Some(&token),
                    json_body,
                    &out_dir,
                    seq,
                );
                assert_ne!(
                    status, 200,
                    "rejection case {label} unexpectedly succeeded: {body}"
                );
                assert_eq!(
                    nosql_wire_code_of(&body),
                    *expected_wire_code,
                    "rejection case {label}: unexpected wire_code (body={body})"
                );
                for leaked in ["tenant-a", "tenant-b", "tenant-c", "11", "12"] {
                    assert!(
                        !body.contains(leaked),
                        "rejection case {label}: response body leaks {leaked:?}: {body}"
                    );
                }
                case_summaries.push(format!("{label}(rejected={expected_wire_code})"));
            }
        }

        seq += 1;
        let (close_status, close_body) = client.post(
            nosql_port,
            "/v1/session/close",
            Some(&token),
            "{}",
            &out_dir,
            seq,
        );
        assert_eq!(
            close_status, 200,
            "session close failed for {user}: {close_body}"
        );
    }

    let nosql_seen = nosql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let nosql_joined = nosql_seen.join("");
    assert!(
        nosql_joined.contains("wire-server: surface nosql:"),
        "expected nosql surface banner in stderr, got: {nosql_joined:?}"
    );
    for token in &issued_tokens {
        assert!(
            !nosql_joined.contains(token.as_str()),
            "stderr must not leak a session token"
        );
    }
    for (user, _) in USERS {
        assert!(
            !nosql_joined.contains(user),
            "stderr must not leak username {user}"
        );
    }
    for tenant in ["tenant-a", "tenant-b", "tenant-c"] {
        assert!(
            !nosql_joined.contains(tenant),
            "stderr must not leak tenant id {tenant}"
        );
    }

    let record = format!(
        "[e2e-record] parity/{label}: psql_version={psql_version:?} \
         client_version={client_version:?} {summary}",
        label = client.label(),
        summary = case_summaries.join(" "),
    );
    for token in &issued_tokens {
        assert!(
            !record.contains(token.as_str()),
            "e2e-record line must not leak a session token"
        );
    }
    for (user, pw) in USERS {
        assert!(
            !record.contains(user),
            "e2e-record line must not leak username {user}"
        );
        assert!(
            !record.contains(pw),
            "e2e-record line must not leak password for {user}"
        );
    }
    for tenant in ["tenant-a", "tenant-b", "tenant-c"] {
        assert!(
            !record.contains(tenant),
            "e2e-record line must not leak tenant id {tenant}"
        );
    }
    eprintln!("{record}");
}

#[test]
#[ignore = "requires psql and curl; run via `make e2e-three-client-http`"]
fn curl_matches_psql_on_search_scan_aggregate() {
    run_sql_nosql_parity_scenario(HttpClient::Curl);
}

#[test]
#[ignore = "requires psql and python3; run via `make e2e-three-client-http`"]
fn urllib_matches_psql_on_search_scan_aggregate() {
    run_sql_nosql_parity_scenario(HttpClient::Urllib);
}

#[test]
#[ignore = "requires psql and node (>= 18); run via `make e2e-three-client-http`"]
fn fetch_matches_psql_on_search_scan_aggregate() {
    run_sql_nosql_parity_scenario(HttpClient::Fetch);
}

// ---------------------------------------------------------------------------
// DML パリティ（Issue #877）。モジュールドキュメント「DML パリティ」節参照。
// ---------------------------------------------------------------------------

/// `seed_parity_db` と同一内容（`docs` テーブル・3 テナント Public 各 1 件・
/// tenant-a/tenant-b の Private 各 1 件）の一時 DB を、DML シナリオ専用の
/// ラベル付きファイル名で複製する（Issue #877。DB-S／DB-N の 2 つを独立した
/// ファイルとして用意し、同じ手順を SQL 表層・NoSQL 表層それぞれへ適用して
/// 比較できるようにする）。
fn seed_dml_parity_db(label: &str) -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(&format!("three-client-http-e2e-dml-{label}"));
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let public_rows: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in public_rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("dml-seed-public")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    let private_rows: [(&str, u64, [f32; 2], &str); 2] = [
        ("tenant-a", 11, [1.0, 0.0], "xx"),
        ("tenant-b", 12, [0.0, 1.0], "ja"),
    ];
    for (tenant, id, dir, lang) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text("private body".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("dml-seed-private")
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }
    (path, guard)
}

/// 1 行だけを含む最小 `docs` DB を用意する（RLS-9 応答同一性検証の
/// 「未存在 id」対照用。Issue #877）。`owner` が所有する `id`（Public）が
/// 唯一の行になる。
fn seed_single_row_db(label: &str, owner: &str, id: u64) -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(&format!("three-client-http-e2e-dml-{label}"));
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new(owner).expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        id,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0, 1.0]),
            Value::Text("en".to_string()),
            Value::Text("owner body".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("dml-rls9-seed")
            .expect("valid operation_id"),
    )
    .expect("insert owner row");
    (path, guard)
}

/// 空の `docs` テーブルだけを持つ DB を用意する（RLS-9 応答同一性検証の
/// 「未存在 id」対照用。行が 1 件もないため対象 id は常に未存在になる）。
fn seed_empty_docs_db(label: &str) -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(&format!("three-client-http-e2e-dml-{label}"));
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (path, guard)
}

/// SQL 表層への `UPDATE`／`DELETE` 1 文の生 wire 経由の結果（Issue #877。
/// psql は拡張クエリ・SQLSTATE 抽出に不向きなため、`sql_column_types_via_raw_wire`
/// と同じ方針で生 wire を直接読む）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SqlDmlOutcome {
    /// `CommandComplete` タグ（例: `"UPDATE 1"`）。
    Success { tag: String },
    /// `ErrorResponse` の SQLSTATE・message。
    Error { sqlstate: String, message: String },
}

/// `ErrorResponse`（'E'）の本文（`'S'`／`'C'`／`'M'` 等の 1 バイトタグ +
/// nul 終端文字列の繰り返し、終端は 0 バイト 1 個）から SQLSTATE（`'C'`）と
/// message（`'M'`）を取り出す。`common::expect_error_response_with_sqlstate_and_message`
/// は期待値との一致検査に特化しており実際の値を返さないため、このファイル
/// 専用の抽出版を用意する（`crates/wire-server/src/error_response.rs` が
/// 唯一の送出経路であるため、フィールドレイアウトの解釈はそれと一致する）。
fn parse_error_fields(body: &[u8]) -> (String, String) {
    let mut sqlstate: Option<String> = None;
    let mut message: Option<String> = None;
    let mut pos = 0usize;
    while pos < body.len() && body[pos] != 0 {
        let field_type = body[pos];
        pos += 1;
        let nul = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated error field value");
        let value = std::str::from_utf8(&body[pos..pos + nul])
            .expect("utf8 error field value")
            .to_string();
        pos += nul + 1;
        match field_type {
            b'C' => sqlstate = Some(value),
            b'M' => message = Some(value),
            _ => {}
        }
    }
    (
        sqlstate.unwrap_or_else(|| panic!("ErrorResponse missing SQLSTATE field")),
        message.unwrap_or_else(|| panic!("ErrorResponse missing message field")),
    )
}

/// 簡易クエリ送出直後の応答（`CommandComplete` または `ErrorResponse`）を
/// 判別して読み取り、`ReadyForQuery` まで読み切ってから返す（呼び出し元が
/// 同一接続で追加のクエリを送れるようにする）。長さフィールドは
/// `read_row_description_with_oids` と同じ基準で確保前に検証する
/// （coding-rust.md「untrusted 入力の扱い」参照。応答は実バイナリ由来だが
/// 検証コストは軽微なため一貫して適用する）。
fn read_sql_dml_response(stream: &mut TcpStream) -> SqlDmlOutcome {
    let mut kind = [0u8; 1];
    stream.read_exact(&mut kind).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len_i32 = i32::from_be_bytes(len_buf);
    assert!(
        (4..=(1 << 20)).contains(&len_i32),
        "invalid message length {len_i32}"
    );
    let len = len_i32 as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let outcome = match kind[0] {
        b'C' => {
            let end = body.len().saturating_sub(1);
            SqlDmlOutcome::Success {
                tag: String::from_utf8_lossy(&body[..end]).to_string(),
            }
        }
        b'E' => {
            let (sqlstate, message) = parse_error_fields(&body);
            SqlDmlOutcome::Error { sqlstate, message }
        }
        other => panic!("expected CommandComplete or ErrorResponse, got {other:?}"),
    };
    common::read_ready_for_query(stream);
    outcome
}

/// 新規接続を張り 1 文の `UPDATE`／`DELETE` を送って結果を読む
/// （`sql_column_types_via_raw_wire` と同じ接続作法。使い切った
/// `TcpStream` は drop でクローズされる）。
fn run_sql_dml(port: u16, user: &str, password: &str, sql: &str) -> SqlDmlOutcome {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("valid loopback addr");
    let mut stream = common::authenticate_to_ready_for_query(addr, user, password);
    common::send_simple_query(&mut stream, sql);
    read_sql_dml_response(&mut stream)
}

/// psql 接続で `docs` の可視行（`id`,`lang`）を読み戻す（DML パリティの
/// 最終状態比較用）。`ORDER BY` は距離関数（`<=>`／`HYBRID(...)`）専用の
/// 構文でありスカラー列には使えないため（`sql::allowlist`）、SQL-15 の
/// 広域取得（順序保証なし）を使い、呼び出し元でソートしてから比較する
/// （`nosql_read_back_id_lang` の `scan` と同じ方針）。
fn psql_read_back_id_lang(port: u16, user: &str, password: &str) -> Vec<(String, String)> {
    let (_header, rows) =
        run_psql_with_header(port, user, password, "SELECT id, lang FROM docs LIMIT 100");
    rows.into_iter()
        .map(|row| {
            assert_eq!(row.len(), 2, "expected 2 cells, got {row:?}");
            (row[0].clone(), row[1].clone())
        })
        .collect()
}

/// NoSQL 表層の `op: scan` で `docs` の可視行（`id`,`lang`）を読み戻す
/// （DML パリティの最終状態比較用。`scan` は順序保証を持たない契約
/// （SQL-15）のため呼び出し元でソートしてから比較する）。
fn nosql_read_back_id_lang(
    client: &HttpClient,
    port: u16,
    token: &str,
    out_dir: &std::path::Path,
    seq: &mut u32,
) -> Vec<(String, String)> {
    *seq += 1;
    let (status, body) = client.post(
        port,
        "/v1/query",
        Some(token),
        r#"{"op":"scan","table":"docs","limit":100,"columns":["id","lang"]}"#,
        out_dir,
        *seq,
    );
    assert_eq!(status, 200, "scan read-back failed: {body}");
    let result_obj = json_object(&body);
    let rows_json = match result_obj.get("rows") {
        Some(JsonValue::Array(rows)) => rows.clone(),
        other => panic!("expected array rows field, got {other:?}"),
    };
    rows_json
        .iter()
        .map(|row| match row {
            JsonValue::Array(cells) => {
                assert_eq!(cells.len(), 2, "expected 2 cells, got {cells:?}");
                (
                    json_cell_to_pg_text(&cells[0], NULL_SENTINEL),
                    json_cell_to_pg_text(&cells[1], NULL_SENTINEL),
                )
            }
            other => panic!("expected array row, got {other:?}"),
        })
        .collect()
}

/// DML パリティ 1 ステップの期待結果（Issue #877）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DmlExpectation {
    /// `CommandComplete` タグの数値部分・`{"updated"/"deleted":n}` の `n`。
    Affected(u64),
    /// SQLSTATE（`wire_code`）。message はケースごとに束縛段階が異なり
    /// うるため比較対象に含めない（モジュールドキュメント「クエリ集合」節
    /// の方針を踏襲）。
    Error(&'static str),
}

/// SQL 表層（生 wire）・NoSQL 表層（`client`）が共有する 1 DML 操作の期待値
/// （Issue #877。モジュールドキュメント「DML パリティ」節の表に対応）。
/// `sql`／`json_body` はリテラル定数（クライアント応答由来の文字列を
/// SQL／JSON へ連結しない）。
struct DmlStep {
    label: &'static str,
    sql: &'static str,
    json_body: &'static str,
    expect: DmlExpectation,
}

/// alice（tenant-a）が順に実行する DML 手順（Issue #877）。`docs` の初期状態
/// は `seed_dml_parity_db` と同一。各ステップは前のステップが変更した状態を
/// 前提にする（例: `u-resend-same` は `u-own` が記録した `operation_id`
/// `dml-u1` を同一内容で再送する）。
const DML_STEPS: &[DmlStep] = &[
    DmlStep {
        label: "u-own",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'dml-u1'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"dml-u1"}"#,
        expect: DmlExpectation::Affected(1),
    },
    DmlStep {
        label: "u-own-private",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 11 USING OPERATION_ID 'dml-u11'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":11},"operation_id":"dml-u11"}"#,
        expect: DmlExpectation::Affected(1),
    },
    DmlStep {
        label: "u-foreign-visible",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 2 USING OPERATION_ID 'dml-u2'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":2},"operation_id":"dml-u2"}"#,
        expect: DmlExpectation::Affected(0),
    },
    DmlStep {
        label: "u-foreign-private",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 12 USING OPERATION_ID 'dml-u12'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":12},"operation_id":"dml-u12"}"#,
        expect: DmlExpectation::Affected(0),
    },
    DmlStep {
        label: "u-missing",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 999 USING OPERATION_ID 'dml-u999'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":999},"operation_id":"dml-u999"}"#,
        expect: DmlExpectation::Affected(0),
    },
    DmlStep {
        label: "u-resend-same",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'dml-u1'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"dml-u1"}"#,
        expect: DmlExpectation::Error("23505"),
    },
    DmlStep {
        label: "u-resend-diff",
        sql: "UPDATE docs SET lang = 'fr' WHERE id = 1 USING OPERATION_ID 'dml-u1'",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"fr"},"where":{"id":1},"operation_id":"dml-u1"}"#,
        expect: DmlExpectation::Error("22023"),
    },
    DmlStep {
        // SQL は宣言順 `lang, embedding`、JSON は常にアルファベット順
        // `embedding, lang` へ正規化される（`nosql12_update_delete.rs` の
        // J 節と同種の観点）。両者は値・意味が同一のため、`content_hash`
        // 正規化が宣言順に依存しないことも本ステップの成功で確認できる。
        label: "u-multi-col",
        sql: "UPDATE docs SET lang = 'ja', embedding = '[0.4,0.5]' WHERE id = 1 USING OPERATION_ID 'dml-multi'",
        json_body: r#"{"op":"update","table":"docs","set":{"embedding":[0.4,0.5],"lang":"ja"},"where":{"id":1},"operation_id":"dml-multi"}"#,
        expect: DmlExpectation::Affected(1),
    },
    DmlStep {
        label: "d-own",
        sql: "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'dml-d1'",
        json_body: r#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"dml-d1"}"#,
        expect: DmlExpectation::Affected(1),
    },
    DmlStep {
        label: "d-foreign",
        sql: "DELETE FROM docs WHERE id = 3 USING OPERATION_ID 'dml-d3'",
        json_body: r#"{"op":"delete","table":"docs","where":{"id":3},"operation_id":"dml-d3"}"#,
        expect: DmlExpectation::Affected(0),
    },
    DmlStep {
        label: "d-missing",
        sql: "DELETE FROM docs WHERE id = 999 USING OPERATION_ID 'dml-d999'",
        json_body: r#"{"op":"delete","table":"docs","where":{"id":999},"operation_id":"dml-d999"}"#,
        expect: DmlExpectation::Affected(0),
    },
    DmlStep {
        label: "d-resend",
        sql: "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'dml-d1'",
        json_body: r#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"dml-d1"}"#,
        expect: DmlExpectation::Error("23505"),
    },
    DmlStep {
        // `operation_id` 省略。SQL 側は `USING OPERATION_ID` 句そのものを
        // 省く（単一行 `UPDATE` 構文は本節句を必須とするため許可リストが
        // `42601` を返す想定にはならず、`bind_update` の必須化ガードで
        // `23502` になる契約——`docs/design/update-single-row.md` 参照）。
        label: "e-no-opid",
        sql: "UPDATE docs SET lang = 'en' WHERE id = 2",
        json_body: r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":2}}"#,
        expect: DmlExpectation::Error("23502"),
    },
    DmlStep {
        label: "e-undefined-table",
        sql: "UPDATE docs_missing SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'dml-e2'",
        json_body: r#"{"op":"update","table":"docs_missing","set":{"lang":"en"},"where":{"id":1},"operation_id":"dml-e2"}"#,
        expect: DmlExpectation::Error("42P01"),
    },
];

/// bob（tenant-b）が実行する 1 ステップ（Issue #877。alice 所有の Private
/// 行 id=11 は bob には不可視のため `DELETE` は 0 行成功になる。RLS-9・
/// RLS-11 の対照）。
const BOB_STEP: DmlStep = DmlStep {
    label: "b-delete-foreign-private",
    sql: "DELETE FROM docs WHERE id = 11 USING OPERATION_ID 'dml-b1'",
    json_body: r#"{"op":"delete","table":"docs","where":{"id":11},"operation_id":"dml-b1"}"#,
    expect: DmlExpectation::Affected(0),
};

/// `CommandComplete` タグ（`"UPDATE 1"`／`"DELETE 0"` 等）から影響行数を
/// 取り出す。タグ形状が想定と異なる場合は untrusted な実バイナリ応答を
/// fail-closed に扱い `panic!` する。
fn affected_count_from_tag(tag: &str) -> u64 {
    let n = tag
        .rsplit(' ')
        .next()
        .unwrap_or_else(|| panic!("empty CommandComplete tag"));
    n.parse()
        .unwrap_or_else(|e| panic!("CommandComplete tag {tag:?} has no numeric suffix: {e}"))
}

/// NoSQL 応答本文から `updated`／`deleted`／`inserted`（Issue #950・insert
/// パリティで追加）の影響行数を取り出す。
fn affected_count_from_nosql_body(body: &str) -> u64 {
    let obj = json_object(body);
    match obj
        .get("updated")
        .or_else(|| obj.get("deleted"))
        .or_else(|| obj.get("inserted"))
    {
        Some(JsonValue::Number(JsonNumber::PosInt(n))) => *n,
        other => panic!("expected updated/deleted/inserted field, got {other:?} (body={body:?})"),
    }
}

/// NoSQL エラー応答本文（`{"error":{"wire_code":...,"code":...,"message":...}}`。
/// `http_common::wire_code_of` と同じネスト形状。このファイルは HTTP
/// クライアント（curl／urllib／fetch）の生本文しか持たないため専用の抽出版
/// を用意する）から `wire_code` を取り出す。
fn nosql_wire_code_of(body: &str) -> String {
    let obj = json_object(body);
    match obj.get("error") {
        Some(JsonValue::Object(err)) => match err.get("wire_code") {
            Some(JsonValue::String(s)) => s.clone(),
            other => panic!("expected string wire_code field, got {other:?} (body={body:?})"),
        },
        other => panic!("expected object error field, got {other:?} (body={body:?})"),
    }
}

/// `DML_STEPS`／`BOB_STEP` 1 件を SQL 表層（生 wire）へ適用し、`expect` と
/// 一致することを確認する。
fn apply_sql_dml_step(port: u16, user: &str, password: &str, step: &DmlStep) -> SqlDmlOutcome {
    let outcome = run_sql_dml(port, user, password, step.sql);
    match (&step.expect, &outcome) {
        (DmlExpectation::Affected(n), SqlDmlOutcome::Success { tag }) => {
            assert_eq!(
                affected_count_from_tag(tag),
                *n,
                "step={} sql tag={tag:?}",
                step.label
            );
        }
        (DmlExpectation::Error(code), SqlDmlOutcome::Error { sqlstate, message }) => {
            assert_eq!(
                sqlstate, code,
                "step={} sql message={message:?}",
                step.label
            );
        }
        (expect, outcome) => panic!(
            "step={}: sql outcome {outcome:?} does not match expectation {expect:?}",
            step.label
        ),
    }
    outcome
}

/// `SqlDmlOutcome`／NoSQL 応答本文の双方を「影響行数」または「`wire_code`」
/// へ正規化した比較用の値（Issue #877・codex-review 指摘。`message` は
/// `DmlExpectation::Error` のコメントで既述のとおり束縛段階が表層間で
/// 異なりうるため対象外のまま、`wire_code`／影響行数は両表層の実際の
/// 応答同士を直接比較する）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DmlOutcomeSummary {
    Affected(u64),
    Error(String),
}

impl From<&SqlDmlOutcome> for DmlOutcomeSummary {
    fn from(outcome: &SqlDmlOutcome) -> Self {
        match outcome {
            SqlDmlOutcome::Success { tag } => {
                DmlOutcomeSummary::Affected(affected_count_from_tag(tag))
            }
            SqlDmlOutcome::Error { sqlstate, .. } => DmlOutcomeSummary::Error(sqlstate.clone()),
        }
    }
}

/// `DML_STEPS`／`BOB_STEP` 1 件を NoSQL 表層（`client`）へ適用し、同じ
/// `expect` との一致に加え、`sql_outcome`（同じステップを SQL 表層へ適用
/// した実際の結果）と NoSQL 応答を `DmlOutcomeSummary` として直接比較する
/// （codex-review 指摘・PR #994: 従来は両表層を固定 `expect` へ別々に
/// 照合するのみで表層間の応答を直接突き合わせていなかった）。
fn apply_nosql_dml_step(
    client: &HttpClient,
    port: u16,
    token: &str,
    out_dir: &std::path::Path,
    seq: &mut u32,
    step: &DmlStep,
    sql_outcome: &SqlDmlOutcome,
) {
    *seq += 1;
    let (status, body) = client.post(
        port,
        "/v1/query",
        Some(token),
        step.json_body,
        out_dir,
        *seq,
    );
    let nosql_summary = match &step.expect {
        DmlExpectation::Affected(n) => {
            assert_eq!(status, 200, "step={} nosql body={body}", step.label);
            let affected = affected_count_from_nosql_body(&body);
            assert_eq!(affected, *n, "step={} nosql body={body}", step.label);
            DmlOutcomeSummary::Affected(affected)
        }
        DmlExpectation::Error(code) => {
            assert_ne!(status, 200, "step={} nosql body={body}", step.label);
            let wire_code = nosql_wire_code_of(&body);
            assert_eq!(wire_code, *code, "step={} nosql body={body}", step.label);
            DmlOutcomeSummary::Error(wire_code)
        }
    };
    let sql_summary = DmlOutcomeSummary::from(sql_outcome);
    assert_eq!(
        nosql_summary, sql_summary,
        "step={}: sql/nosql outcome mismatch (sql={sql_outcome:?} nosql status={status} \
         body={body})",
        step.label
    );
}

/// `run_sql_nosql_dml_parity_scenario` の実行記録・各サーバー stderr に
/// 機密値（トークン・ユーザー名・パスワード・テナント id）を混ぜないことを
/// 確認する共通アサーション。`[e2e-record]` 行だけでなく、4 回の
/// `stop_and_drain`（`sql_seen`／`nosql_seen`／`db_s_nosql_seen`／
/// `db_n_sql_seen`）が返す生 stderr にもそれぞれ適用する
/// （codex-review 指摘・PR #994）。
fn assert_dml_scenario_no_leak(source: &str, haystack: &str, tokens: &[&str]) {
    // 検査対象の生文字列（`haystack`）には実セッショントークン・実パスワード
    // が含まれ得るため、失敗時のパニックメッセージへは検査対象そのものを
    // 埋め込まない。`source`（呼び出し元が識別する非機密なラベル。例:
    // "sql_seen"）だけを出力し、CI ログへ秘密値が再出力されるのを防ぐ
    // （codex-review P0 指摘・PR #994）。
    for secret in ["alice", "bob", "pw-alice", "pw-bob", "tenant-a", "tenant-b"] {
        assert!(
            !haystack.contains(secret),
            "must not leak a credential/tenant identifier in {source} (value redacted)"
        );
    }
    for token in tokens {
        assert!(
            !haystack.contains(token),
            "must not leak a session token in {source} (value redacted)"
        );
    }
}

/// 無改造の外部 HTTP クライアント（`client` で切替）で、`UPDATE`／`DELETE`
/// （単一行 `id` 完全一致形）が SQL 表層（実 `wire-server`・生 wire）と
/// NoSQL 表層（実 `wire-server --surface nosql`・`client`）で同一の実行
/// 結果（影響行数・エラー `wire_code`・操作後の状態）を返すことを固定する
/// シナリオ（Issue #877。curl／urllib／fetch の 3 テストが本関数へ委譲する）。
/// モジュールドキュメント「DML パリティ」節の起動方式・スコープに従う。
fn run_sql_nosql_dml_parity_scenario(client: HttpClient) {
    let client_version = client.version();
    let psql_version = psql_version();

    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let users_path_str = users_path.to_str().expect("utf-8 users path").to_string();

    let out_dir = std::env::temp_dir().join(format!(
        "wire-server-three-client-http-e2e-dml-{}-out-{}-{}",
        client.label(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&out_dir).expect("create client output dir");
    let _out_dir_guard = CurlOutDirGuard(out_dir.clone());
    let mut seq: u32 = 0;

    // --- Phase 1: DB-S を SQL 表層で駆動し、全ステップを生 wire で適用する。
    let (db_s_path, _db_s_guard) = seed_dml_parity_db("db-s");
    let db_s_path_str = db_s_path.to_str().expect("utf-8 db path").to_string();
    let (sql_server, sql_port) = spawn_sql_server(&users_path_str, &db_s_path_str);
    let mut sql_outcomes: Vec<SqlDmlOutcome> = Vec::with_capacity(DML_STEPS.len());
    for step in DML_STEPS {
        sql_outcomes.push(apply_sql_dml_step(sql_port, "alice", "pw-alice", step));
    }
    let bob_sql_outcome = apply_sql_dml_step(sql_port, "bob", "pw-bob", &BOB_STEP);
    let sql_final_alice = psql_read_back_id_lang(sql_port, "alice", "pw-alice");
    let sql_final_bob = psql_read_back_id_lang(sql_port, "bob", "pw-bob");
    let sql_seen = sql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        !sql_seen.iter().any(|line| line.contains("surface nosql")),
        "SQL surface must not print the nosql surface banner: {sql_seen:?}"
    );
    // codex-review 指摘（PR #994）: `[e2e-record]` だけでなく各 stop_and_drain
    // が返す生 stderr にも機密値の非漏えい検査を適用する。この時点ではまだ
    // セッショントークンを発行していないため tokens は空。
    assert_dml_scenario_no_leak("sql_seen", &sql_seen.join("\n"), &[]);

    // --- Phase 2: 同一内容で複製した DB-N を NoSQL 表層で駆動し、同じ手順を
    //     `client` で適用する。
    let (db_n_path, _db_n_guard) = seed_dml_parity_db("db-n");
    let db_n_path_str = db_n_path.to_str().expect("utf-8 db path").to_string();
    let (nosql_server, nosql_port) = spawn_nosql_server(&users_path_str, &db_n_path_str);

    seq += 1;
    let (status, body) = client.post(
        nosql_port,
        "/v1/session",
        None,
        r#"{"user":"alice","password":"pw-alice"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 200, "alice session issue failed: {body}");
    let alice_token = match json_object(&body).get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    assert_valid_session_token(&alice_token);

    for (step, sql_outcome) in DML_STEPS.iter().zip(sql_outcomes.iter()) {
        apply_nosql_dml_step(
            &client,
            nosql_port,
            &alice_token,
            &out_dir,
            &mut seq,
            step,
            sql_outcome,
        );
    }
    let nosql_final_alice =
        nosql_read_back_id_lang(&client, nosql_port, &alice_token, &out_dir, &mut seq);

    seq += 1;
    let (status, body) = client.post(
        nosql_port,
        "/v1/session",
        None,
        r#"{"user":"bob","password":"pw-bob"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 200, "bob session issue failed: {body}");
    let bob_token = match json_object(&body).get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    assert_valid_session_token(&bob_token);
    apply_nosql_dml_step(
        &client,
        nosql_port,
        &bob_token,
        &out_dir,
        &mut seq,
        &BOB_STEP,
        &bob_sql_outcome,
    );
    let nosql_final_bob =
        nosql_read_back_id_lang(&client, nosql_port, &bob_token, &out_dir, &mut seq);

    // 最終状態の一致（両表層が同じ誤りを返すケースの排除も兼ねる固定
    // オラクル）。alice 視点: id=1 は d-own で削除済み、id=11 は
    // u-own-private で 'en' のまま、id=2/3 は他テナント所有のため不変。
    let expected_alice: Vec<(String, String)> = vec![
        ("2".into(), "en".into()),
        ("3".into(), "ja".into()),
        ("11".into(), "en".into()),
    ];
    let mut sql_final_alice_sorted = sql_final_alice.clone();
    sql_final_alice_sorted.sort();
    let mut nosql_final_alice_sorted = nosql_final_alice.clone();
    nosql_final_alice_sorted.sort();
    let mut expected_alice_sorted = expected_alice.clone();
    expected_alice_sorted.sort();
    assert_eq!(
        sql_final_alice_sorted, nosql_final_alice_sorted,
        "final state (alice) mismatch between SQL and NoSQL surfaces"
    );
    assert_eq!(
        sql_final_alice_sorted, expected_alice_sorted,
        "final state (alice) does not match fixed oracle"
    );

    // bob 視点: id=11（alice の Private 行）は不可視のまま。id=2 は
    // e-no-opid が `23502` で拒否され副作用なしのため 'en' へ書き換わって
    // いない（元の seed 値 'en' のまま——tenant-b 所有の Public 行は元々
    // lang='en' のため見た目の変化はないが、u-foreign-visible の
    // `23502`／拒否経路で書き換わっていないことをこの一致で確認する）。
    let expected_bob: Vec<(String, String)> = vec![
        ("2".into(), "en".into()),
        ("3".into(), "ja".into()),
        ("12".into(), "ja".into()),
    ];
    let mut sql_final_bob_sorted = sql_final_bob.clone();
    sql_final_bob_sorted.sort();
    let mut nosql_final_bob_sorted = nosql_final_bob.clone();
    nosql_final_bob_sorted.sort();
    let mut expected_bob_sorted = expected_bob.clone();
    expected_bob_sorted.sort();
    assert_eq!(
        sql_final_bob_sorted, nosql_final_bob_sorted,
        "final state (bob) mismatch between SQL and NoSQL surfaces"
    );
    assert_eq!(
        sql_final_bob_sorted, expected_bob_sorted,
        "final state (bob) does not match fixed oracle"
    );

    // DB-N を後続フェーズ（SQL 表層での再オープン）に備えていったん停止する
    // （redb は単一ライターのため、同一ファイルを別プロセスで開く前に
    // 確実に終了させる）。
    let nosql_seen = nosql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        nosql_seen.iter().any(|line| line.contains("surface nosql")),
        "expected nosql surface banner in stderr, got: {nosql_seen:?}"
    );
    assert_dml_scenario_no_leak(
        "nosql_seen",
        &nosql_seen.join("\n"),
        &[alice_token.as_str(), bob_token.as_str()],
    );

    // --- Phase 3: 台帳のプロセス・表層横断永続。DB-S（SQL 表層で
    //     `dml-u1`＝`lang='en'` を記録済み）を NoSQL 表層で開き直し、
    //     同一内容の再送は `23505`、異なる内容は `22023` になることを
    //     確認する。
    let (db_s_nosql_server, db_s_nosql_port) = spawn_nosql_server(&users_path_str, &db_s_path_str);
    seq += 1;
    let (status, body) = client.post(
        db_s_nosql_port,
        "/v1/session",
        None,
        r#"{"user":"alice","password":"pw-alice"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 200, "alice session (DB-S/nosql) failed: {body}");
    let db_s_alice_token = match json_object(&body).get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    seq += 1;
    let (status, body) = client.post(
        db_s_nosql_port,
        "/v1/query",
        Some(&db_s_alice_token),
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"dml-u1"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 409, "cross-surface same-content resend: {body}");
    assert_eq!(
        nosql_wire_code_of(&body),
        "23505",
        "cross-surface same-content resend: {body}"
    );
    seq += 1;
    let (status, body) = client.post(
        db_s_nosql_port,
        "/v1/query",
        Some(&db_s_alice_token),
        r#"{"op":"update","table":"docs","set":{"lang":"zz"},"where":{"id":1},"operation_id":"dml-u1"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 400, "cross-surface diff-content resend: {body}");
    assert_eq!(
        nosql_wire_code_of(&body),
        "22023",
        "cross-surface diff-content resend: {body}"
    );
    let db_s_nosql_seen = db_s_nosql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        db_s_nosql_seen
            .iter()
            .any(|line| line.contains("surface nosql")),
        "expected nosql surface banner in stderr, got: {db_s_nosql_seen:?}"
    );
    assert_dml_scenario_no_leak(
        "db_s_nosql_seen",
        &db_s_nosql_seen.join("\n"),
        &[
            alice_token.as_str(),
            bob_token.as_str(),
            db_s_alice_token.as_str(),
        ],
    );

    // 逆方向: DB-N（NoSQL 表層で `dml-u1` を記録済み）を SQL 表層で開き直し、
    // 同一内容の再送が `23505` になることを確認する。
    let (db_n_sql_server, db_n_sql_port) = spawn_sql_server(&users_path_str, &db_n_path_str);
    let outcome = run_sql_dml(
        db_n_sql_port,
        "alice",
        "pw-alice",
        "UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'dml-u1'",
    );
    match outcome {
        SqlDmlOutcome::Error { sqlstate, .. } => {
            assert_eq!(sqlstate, "23505", "cross-surface resend (nosql→sql)")
        }
        other => panic!("expected 23505 error, got {other:?}"),
    }
    let db_n_sql_seen = db_n_sql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        !db_n_sql_seen
            .iter()
            .any(|line| line.contains("surface nosql")),
        "SQL surface must not print the nosql surface banner: {db_n_sql_seen:?}"
    );
    assert_dml_scenario_no_leak(
        "db_n_sql_seen",
        &db_n_sql_seen.join("\n"),
        &[
            alice_token.as_str(),
            bob_token.as_str(),
            db_s_alice_token.as_str(),
        ],
    );

    // --- Phase 4: RLS-9 応答同一性。他テナント行を持つ DB-F・空の DB-M を
    //     用意し、同一 `operation_id` を他テナント所有 id／未存在 id へ
    //     それぞれ送って応答（タグ・NoSQL ステータス＋本文）が一致する
    //     ことを確認する。
    let (db_f_path, _db_f_guard) = seed_single_row_db("db-f", "tenant-b", 100);
    let db_f_path_str = db_f_path.to_str().expect("utf-8 db path").to_string();
    let (db_m_path, _db_m_guard) = seed_empty_docs_db("db-m");
    let db_m_path_str = db_m_path.to_str().expect("utf-8 db path").to_string();

    let (db_f_sql, db_f_sql_port) = spawn_sql_server(&users_path_str, &db_f_path_str);
    let outcome_f = run_sql_dml(
        db_f_sql_port,
        "alice",
        "pw-alice",
        "UPDATE docs SET lang = 'en' WHERE id = 100 USING OPERATION_ID 'dml-rls9-a'",
    );
    let db_f_sql_seen = db_f_sql.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let (db_m_sql, db_m_sql_port) = spawn_sql_server(&users_path_str, &db_m_path_str);
    let outcome_m = run_sql_dml(
        db_m_sql_port,
        "alice",
        "pw-alice",
        "UPDATE docs SET lang = 'en' WHERE id = 999 USING OPERATION_ID 'dml-rls9-a'",
    );
    let db_m_sql_seen = db_m_sql.stop_and_drain(Instant::now() + Duration::from_secs(5));
    // codex-review 指摘（PR #994）: Phase 4 の 4 サーバー（DB-F/DB-M/DB-F2/
    // DB-M2）は従来 stderr を破棄しており非漏えい検査から漏れていた。
    // DB-F／DB-M（SQL 表層）はセッショントークンを発行しないため、この
    // 時点までに発行済みの NoSQL トークンのみを対象に検査する。
    assert_dml_scenario_no_leak(
        "db_f_sql_seen",
        &db_f_sql_seen.join("\n"),
        &[
            alice_token.as_str(),
            bob_token.as_str(),
            db_s_alice_token.as_str(),
        ],
    );
    assert_dml_scenario_no_leak(
        "db_m_sql_seen",
        &db_m_sql_seen.join("\n"),
        &[
            alice_token.as_str(),
            bob_token.as_str(),
            db_s_alice_token.as_str(),
        ],
    );
    assert_eq!(
        outcome_f, outcome_m,
        "RLS-9: foreign-tenant-row and missing-id responses (SQL) must be identical"
    );
    match &outcome_f {
        SqlDmlOutcome::Success { tag } => assert_eq!(affected_count_from_tag(tag), 0),
        other => panic!("expected 0-row success, got {other:?}"),
    }

    let (db_f2_path, _db_f2_guard) = seed_single_row_db("db-f2", "tenant-b", 100);
    let db_f2_path_str = db_f2_path.to_str().expect("utf-8 db path").to_string();
    let (db_m2_path, _db_m2_guard) = seed_empty_docs_db("db-m2");
    let db_m2_path_str = db_m2_path.to_str().expect("utf-8 db path").to_string();

    let (db_f2_nosql, db_f2_port) = spawn_nosql_server(&users_path_str, &db_f2_path_str);
    seq += 1;
    let (status, body) = client.post(
        db_f2_port,
        "/v1/session",
        None,
        r#"{"user":"alice","password":"pw-alice"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 200, "alice session (DB-F2) failed: {body}");
    let db_f2_token = match json_object(&body).get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    seq += 1;
    let (status_f, body_f) = client.post(
        db_f2_port,
        "/v1/query",
        Some(&db_f2_token),
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":100},"operation_id":"dml-rls9-b"}"#,
        &out_dir,
        seq,
    );
    let db_f2_nosql_seen = db_f2_nosql.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert_dml_scenario_no_leak(
        "db_f2_nosql_seen",
        &db_f2_nosql_seen.join("\n"),
        &[
            alice_token.as_str(),
            bob_token.as_str(),
            db_s_alice_token.as_str(),
            db_f2_token.as_str(),
        ],
    );

    let (db_m2_nosql, db_m2_port) = spawn_nosql_server(&users_path_str, &db_m2_path_str);
    seq += 1;
    let (status, body) = client.post(
        db_m2_port,
        "/v1/session",
        None,
        r#"{"user":"alice","password":"pw-alice"}"#,
        &out_dir,
        seq,
    );
    assert_eq!(status, 200, "alice session (DB-M2) failed: {body}");
    let db_m2_token = match json_object(&body).get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    seq += 1;
    let (status_m, body_m) = client.post(
        db_m2_port,
        "/v1/query",
        Some(&db_m2_token),
        r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":999},"operation_id":"dml-rls9-b"}"#,
        &out_dir,
        seq,
    );
    let db_m2_nosql_seen = db_m2_nosql.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert_dml_scenario_no_leak(
        "db_m2_nosql_seen",
        &db_m2_nosql_seen.join("\n"),
        &[
            alice_token.as_str(),
            bob_token.as_str(),
            db_s_alice_token.as_str(),
            db_f2_token.as_str(),
            db_m2_token.as_str(),
        ],
    );

    assert_eq!(status_f, status_m, "RLS-9 (NoSQL): status must match");
    assert_eq!(body_f, body_m, "RLS-9 (NoSQL): body must match");
    assert_eq!(status_f, 200, "resp={body_f}");
    assert_eq!(affected_count_from_nosql_body(&body_f), 0);

    let issued_tokens = [
        alice_token.as_str(),
        bob_token.as_str(),
        db_s_alice_token.as_str(),
        db_f2_token.as_str(),
        db_m2_token.as_str(),
    ];
    let record = format!(
        "[e2e-record] dml-parity/{label}: psql_version={psql_version:?} \
         client_version={client_version:?} steps={n} second_tenant_step=ok \
         cross_surface_resend(sql_to_nosql=23505/22023,nosql_to_sql=23505) \
         rls9(sql_match=true,nosql_match=true) final_state_match=true",
        label = client.label(),
        n = DML_STEPS.len(),
    );
    assert_dml_scenario_no_leak("record", &record, &issued_tokens);
    eprintln!("{record}");
}

#[test]
#[ignore = "requires psql and curl; run via `make e2e-three-client-http`"]
fn curl_matches_psql_on_update_delete_and_rls_boundary() {
    run_sql_nosql_dml_parity_scenario(HttpClient::Curl);
}

#[test]
#[ignore = "requires psql and python3; run via `make e2e-three-client-http`"]
fn urllib_matches_psql_on_update_delete_and_rls_boundary() {
    run_sql_nosql_dml_parity_scenario(HttpClient::Urllib);
}

#[test]
#[ignore = "requires psql and node (>= 18); run via `make e2e-three-client-http`"]
fn fetch_matches_psql_on_update_delete_and_rls_boundary() {
    run_sql_nosql_dml_parity_scenario(HttpClient::Fetch);
}

// ---------------------------------------------------------------------
// insert／DDL／述語形 DML パリティ（Issue #950 §3.3〜§3.5）
// ---------------------------------------------------------------------

/// テーブルを 1 つも持たない空 DB を用意する（Issue #950・DDL パリティの
/// 起点。`seed_empty_docs_db` は `docs` テーブルを事前作成するため別関数と
/// する）。
fn seed_empty_db_no_table(label: &str) -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(&format!("three-client-http-e2e-ddl-{label}"));
    let guard = temp_db::CleanupGuard(path.clone());
    // `Storage::open` だけで DB ファイルを生成させ、直後に drop して
    // 単一ライター制約（redb）を子プロセス起動前に解放する
    // （`seed_parity_db` 等の既存ヘルパーと同じ作法）。
    let _storage = Storage::open(&path).expect("open storage");
    (path, guard)
}

/// SQL 表層（生 wire）・NoSQL 表層（`client`）へ同一の insert／DDL／述語形
/// `DELETE` 手順を順に適用し、影響行数・`wire_code`・最終状態が一致する
/// ことを固定する（Issue #950 §3.3〜§3.5。curl／urllib／fetch の 3 テストが
/// 共有）。`widgets` テーブル自体は `docs` の RLS 検証 seed とは無関係の
/// 独立テーブルであり、DDL がカタログ操作としてテナント非依存であることを
/// 前提に alice（`--ddl-allowed-users`）1 テナントで手順を組む。bob は
/// 非許可ユーザーとして `drop_table` の権限拒否（`42501`）のみ確認する
/// （テナント境界そのものは `run_sql_nosql_parity_scenario`／
/// `run_sql_nosql_dml_parity_scenario` が担う）。
///
/// **スコープ縮小（Issue #950 実装時点の判断）**: 計画（§3.4）が求める
/// RLS-9 応答同一性（他テナント行向け／未存在 id 向けの同一 `operation_id`
/// 応答一致）の述語形版、および 3 テナント × 述語形 DML の全組み合わせは
/// 本関数では扱わない——単一行 `id` 完全一致形の RLS-9 は
/// `run_sql_nosql_dml_parity_scenario` が既に固定しており、述語形の
/// パリティ・RLS 非漏えいは層 A（`nosql12_update_delete.rs` 述語形節）が
/// 固定済みのため、本ファイルでは表層横断の insert／DDL 到達性確認に
/// 絞った（`docs/design/three-client-e2e-harness.md` Issue #950 節に記録）。
fn run_phase7_write_parity_scenario(client: HttpClient) {
    let client_version = client.version();
    let psql_version = psql_version();

    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let users_path_str = users_path.to_str().expect("utf-8 users path").to_string();

    let out_dir = std::env::temp_dir().join(format!(
        "wire-server-three-client-http-e2e-ddl-{}-out-{}-{}",
        client.label(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&out_dir).expect("create client output dir");
    let _out_dir_guard = CurlOutDirGuard(out_dir.clone());
    let mut seq: u32 = 0;

    // --- Phase 1: DB-S を SQL 表層（`--ddl-allowed-users alice`）で駆動する。
    let (db_s_path, _db_s_guard) = seed_empty_db_no_table("db-s");
    let db_s_path_str = db_s_path.to_str().expect("utf-8 db path").to_string();
    let (sql_server, sql_port) =
        spawn_sql_server_with_ddl_allowed(&users_path_str, &db_s_path_str, "alice");

    let create = run_sql_dml(
        sql_port,
        "alice",
        "pw-alice",
        "CREATE TABLE widgets (name TEXT, qty INTEGER)",
    );
    assert_eq!(
        create,
        SqlDmlOutcome::Success {
            tag: "CREATE TABLE".to_string()
        },
        "SQL create_table: {create:?}"
    );

    let bob_drop_attempt = run_sql_dml(sql_port, "bob", "pw-bob", "DROP TABLE widgets");
    match &bob_drop_attempt {
        SqlDmlOutcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "42501"),
        other => panic!("expected 42501 permission error, got {other:?}"),
    }

    let insert1 = run_sql_dml(
        sql_port,
        "alice",
        "pw-alice",
        "INSERT INTO widgets (id, name, qty) VALUES (1, 'gadget', 5) USING OPERATION_ID 'op-insert-1'",
    );
    match &insert1 {
        SqlDmlOutcome::Success { tag } => assert_eq!(affected_count_from_tag(tag), 1),
        other => panic!("expected 1-row insert success, got {other:?}"),
    }
    let insert2 = run_sql_dml(
        sql_port,
        "alice",
        "pw-alice",
        "INSERT INTO widgets (id, name, qty) VALUES (2, 'sprocket', 9) USING OPERATION_ID 'op-insert-2'",
    );
    match &insert2 {
        SqlDmlOutcome::Success { tag } => assert_eq!(affected_count_from_tag(tag), 1),
        other => panic!("expected 1-row insert success, got {other:?}"),
    }

    let alter = run_sql_dml(
        sql_port,
        "alice",
        "pw-alice",
        "ALTER TABLE widgets ADD COLUMN note TEXT",
    );
    assert_eq!(
        alter,
        SqlDmlOutcome::Success {
            tag: "ALTER TABLE".to_string()
        },
        "SQL alter_table: {alter:?}"
    );

    let predicate_delete = run_sql_dml(
        sql_port,
        "alice",
        "pw-alice",
        "DELETE FROM widgets WHERE name = 'sprocket' USING OPERATION_ID 'op-del-1'",
    );
    match &predicate_delete {
        SqlDmlOutcome::Success { tag } => assert_eq!(affected_count_from_tag(tag), 1),
        other => panic!("expected 1-row predicate delete success, got {other:?}"),
    }

    let (sql_header, sql_rows) = run_psql_with_header(
        sql_port,
        "alice",
        "pw-alice",
        "SELECT id, name, qty, note FROM widgets LIMIT 100",
    );
    assert_eq!(sql_header, vec!["id", "name", "qty", "note"]);
    assert_eq!(
        sql_rows,
        vec![vec![
            "1".to_string(),
            "gadget".to_string(),
            "5".to_string(),
            NULL_SENTINEL.to_string(),
        ]],
        "SQL read-back after predicate delete: {sql_rows:?}"
    );

    let drop = run_sql_dml(sql_port, "alice", "pw-alice", "DROP TABLE widgets");
    assert_eq!(
        drop,
        SqlDmlOutcome::Success {
            tag: "DROP TABLE".to_string()
        },
        "SQL drop_table: {drop:?}"
    );

    let post_drop = run_sql_dml(
        sql_port,
        "alice",
        "pw-alice",
        "SELECT id FROM widgets LIMIT 1",
    );
    match &post_drop {
        SqlDmlOutcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "42P01"),
        other => panic!("expected 42P01 undefined-table error, got {other:?}"),
    }

    let sql_seen = sql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        !sql_seen.iter().any(|line| line.contains("surface nosql")),
        "SQL surface must not print the nosql surface banner: {sql_seen:?}"
    );
    assert_dml_scenario_no_leak("sql_seen", &sql_seen.join("\n"), &[]);

    // --- Phase 2: 同一手順を DB-N 上で NoSQL 表層（`client`）へ適用する。
    let (db_n_path, _db_n_guard) = seed_empty_db_no_table("db-n");
    let db_n_path_str = db_n_path.to_str().expect("utf-8 db path").to_string();
    let (nosql_server, nosql_port) =
        spawn_nosql_server_with_ddl_allowed(&users_path_str, &db_n_path_str, "alice");

    let issue_session = |user: &str, pw: &str, seq: &mut u32| -> String {
        *seq += 1;
        let (status, body) = client.post(
            nosql_port,
            "/v1/session",
            None,
            &format!(r#"{{"user":"{user}","password":"{pw}"}}"#),
            &out_dir,
            *seq,
        );
        assert_eq!(status, 200, "session issue failed for {user}: {body}");
        match json_object(&body).get("token") {
            Some(JsonValue::String(s)) => s.clone(),
            other => panic!("expected string token field, got {other:?}"),
        }
    };
    let alice_token = issue_session("alice", "pw-alice", &mut seq);
    assert_valid_session_token(&alice_token);
    let bob_token = issue_session("bob", "pw-bob", &mut seq);
    assert_valid_session_token(&bob_token);

    let query = |token: &str, json_body: &str, seq: &mut u32| -> (u16, String) {
        *seq += 1;
        client.post(
            nosql_port,
            "/v1/query",
            Some(token),
            json_body,
            &out_dir,
            *seq,
        )
    };

    let (status, body) = query(
        &alice_token,
        r#"{"op":"create_table","table":"widgets","columns":[{"name":"name","type":"text"},{"name":"qty","type":"integer"}]}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL create_table: {body}");
    assert_eq!(body.trim(), r#"{"ok":true}"#);

    let (status, body) = query(
        &bob_token,
        r#"{"op":"drop_table","table":"widgets"}"#,
        &mut seq,
    );
    assert_ne!(status, 200, "bob drop_table unexpectedly succeeded: {body}");
    assert_eq!(nosql_wire_code_of(&body), "42501", "bob drop_table: {body}");

    let (status, body) = query(
        &alice_token,
        r#"{"op":"insert","table":"widgets","rows":[{"id":1,"name":"gadget","qty":5}],"operation_id":"op-insert-1"}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL insert 1: {body}");
    assert_eq!(affected_count_from_nosql_body(&body), 1);

    let (status, body) = query(
        &alice_token,
        r#"{"op":"insert","table":"widgets","rows":[{"id":2,"name":"sprocket","qty":9}],"operation_id":"op-insert-2"}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL insert 2: {body}");
    assert_eq!(affected_count_from_nosql_body(&body), 1);

    let (status, body) = query(
        &alice_token,
        r#"{"op":"alter_table","table":"widgets","add_column":{"name":"note","type":"text"}}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL alter_table: {body}");
    assert_eq!(body.trim(), r#"{"ok":true}"#);

    let (status, body) = query(
        &alice_token,
        r#"{"op":"delete","table":"widgets","filter":[{"column":"name","op":"eq","value":"sprocket"}],"operation_id":"op-del-1"}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL predicate delete: {body}");
    assert_eq!(affected_count_from_nosql_body(&body), 1);

    let (status, body) = query(
        &alice_token,
        r#"{"op":"scan","table":"widgets","limit":100,"columns":["id","name","qty","note"]}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL read-back: {body}");
    let result_obj = json_object(&body);
    let rows_json = match result_obj.get("rows") {
        Some(JsonValue::Array(rows)) => rows.clone(),
        other => panic!("expected array rows field, got {other:?}"),
    };
    let nosql_rows: Vec<Vec<String>> = rows_json
        .iter()
        .map(|row| match row {
            JsonValue::Array(cells) => cells
                .iter()
                .map(|c| json_cell_to_pg_text(c, NULL_SENTINEL))
                .collect(),
            other => panic!("expected array row, got {other:?}"),
        })
        .collect();
    assert_eq!(
        nosql_rows,
        vec![vec![
            "1".to_string(),
            "gadget".to_string(),
            "5".to_string(),
            NULL_SENTINEL.to_string(),
        ]],
        "NoSQL read-back after predicate delete: {nosql_rows:?}"
    );
    assert_eq!(
        nosql_rows, sql_rows,
        "read-back after predicate delete must match between SQL and NoSQL surfaces"
    );

    let (status, body) = query(
        &alice_token,
        r#"{"op":"drop_table","table":"widgets"}"#,
        &mut seq,
    );
    assert_eq!(status, 200, "NoSQL drop_table: {body}");
    assert_eq!(body.trim(), r#"{"ok":true}"#);

    let (status, body) = query(
        &alice_token,
        r#"{"op":"scan","table":"widgets","limit":1}"#,
        &mut seq,
    );
    assert_ne!(status, 200, "post-drop scan unexpectedly succeeded: {body}");
    assert_eq!(nosql_wire_code_of(&body), "42P01", "post-drop scan: {body}");

    let nosql_seen = nosql_server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        nosql_seen.iter().any(|line| line.contains("surface nosql")),
        "expected nosql surface banner in stderr, got: {nosql_seen:?}"
    );
    let issued_tokens = [alice_token.as_str(), bob_token.as_str()];
    assert_dml_scenario_no_leak("nosql_seen", &nosql_seen.join("\n"), &issued_tokens);

    let record = format!(
        "[e2e-record] ddl-parity/{label}: psql_version={psql_version:?} \
         client_version={client_version:?} create_table=ok non_ddl_user_drop_denied=42501 \
         insert=ok(2) alter_table=ok predicate_delete=ok(1) read_back_match=true \
         drop_table=ok post_drop=42P01",
        label = client.label(),
    );
    assert_dml_scenario_no_leak("record", &record, &issued_tokens);
    eprintln!("{record}");
}

#[test]
#[ignore = "requires psql and curl; run via `make e2e-three-client-http`"]
fn curl_matches_psql_on_insert_ddl_and_predicate_delete() {
    run_phase7_write_parity_scenario(HttpClient::Curl);
}

#[test]
#[ignore = "requires psql and python3; run via `make e2e-three-client-http`"]
fn urllib_matches_psql_on_insert_ddl_and_predicate_delete() {
    run_phase7_write_parity_scenario(HttpClient::Urllib);
}

#[test]
#[ignore = "requires psql and node (>= 18); run via `make e2e-three-client-http`"]
fn fetch_matches_psql_on_insert_ddl_and_predicate_delete() {
    run_phase7_write_parity_scenario(HttpClient::Fetch);
}

/// 全 [`Op`] 語彙が本ファイル（Issue #950）の層 B パリティケースのいずれかで
/// 最低 1 回使われていることを機械的に固定する（`#[ignore]` を付けない・
/// `make ci` で常時実行。`.claude/rules/coding-rust.md`「テストの skip・
/// ignore・アサーション弱体化で CI を通さない」の精神を、層 B カバレッジの
/// 陳腐化検知としても適用する）。
///
/// レビュー指摘（Issue #950）: 旧実装は手動維持の固定文字列配列
/// （`EXERCISED_BY_PARITY_MATRIX`）と `Op::ALL` の名前集合を突き合わせる
/// だけで、その文字列が実際に `PARITY_CASES`／`REJECTION_CASES`／
/// `run_phase7_write_parity_scenario` の JSON ボディで使われているかまでは
/// 検査していなかった（配列の更新漏れ・誤記が検知できない）。本実装では
/// `include_str!` で本ファイル自身のソーステキストを検査対象にし、各
/// `Op::name()` に対応する `"op":"<name>"` リテラルが実際に出現するかを
/// 直接判定することで、手動維持配列を廃し実使用箇所と直結させる。
#[test]
fn parity_matrix_covers_every_nosql_op() {
    use wire_server::http::query::op::Op;

    // 本ファイル自身のソーステキスト（コンパイル時に埋め込み）。
    // `PARITY_CASES`・`REJECTION_CASES`・`run_phase7_write_parity_scenario`
    // はいずれも `json_body: r#"{"op":"<name>",...}"#` 形の静的文字列で op を
    // 指定するため、そのリテラル出現を直接 grep すれば実使用を検査できる。
    let source = include_str!("three_client_http_e2e.rs");

    for op in Op::ALL {
        let name = op.name();
        let needle = format!("\"op\":\"{name}\"");
        assert!(
            source.contains(needle.as_str()),
            "Op::{name} is not exercised by any Issue #950 layer B parity case in this file \
             (no literal occurrence of {needle:?} found); add a three_client_http_e2e.rs case \
             using it"
        );
    }
}
