#!/usr/bin/env node
// node `pg`（無改造）で wire-server へ簡易クエリを 1 文送るクライアント。
//
// `crates/wire-server/tests/three_client_e2e.rs`（TASK-73・WIRE-1、`#[ignore]`）から
// 子プロセスとして起動される。接続情報・SQL は環境変数で受け取り（コマンドライン
// 引数・ソース中にダミー資格情報以外の秘密情報を書かない。security.md P0）、
// `client.query(text)`（values を渡さない = 簡易クエリプロトコルで送信される）を
// 使う。
//
// 環境変数: WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD / WIRE_SQL。
// WIRE_SQL_PRELUDE（任意・TASK-165）: 同一接続で WIRE_SQL より先に順次実行する
// SQL 文の配列を表す JSON 文字列（例 ["SET search_mode = 'precision'"]）。
// `SET search_mode` を先行実行してから本体の SELECT を送る、セッション複数文の
// 検証（SQL-12）に使う。配列でない・要素が文字列でない場合は fail-closed で
// エラー終了する（stdin／argv を使わない現行方針を維持。security.md P0）。
// WIRE_SSL（任意・Issue #969・WIRE-9）: `no-verify` のときのみ
// `ssl: { rejectUnauthorized: false }` を pg.Client へ渡す（TLS 越しの接続
// 検証用。テスト証明書は検証対象外＝chain 検証は行わない構成のため）。
// 語彙外（`no-verify` 以外の非空値）は fail-closed でエラー終了する。未指定
// なら従来どおり ssl オプションを渡さない（挙動不変）。
// WIRE_PRINT_COMMAND_TAG（任意・Issue #1339・SQL-32）: "1" のときのみ、本体 SQL の
// 実行後に `${result.command} ${result.rowCount}`（CommandComplete のコマンドタグ。
// 例 "UPDATE 2"）を stdout へ 1 行出力する（分割実行 DML の件数がドライバ API へ
// 届くことの観測用。Issue #1347・SQL-21: `RETURNING` の行とタグを 1 回の実行で
// 観測するため、INSERT は `INSERT ${result.oid} ${result.rowCount}` で出力する）。語彙は "1" のみで、それ以外の値は fail-closed でエラー終了
// する。未指定なら従来どおり何も出さない（挙動不変）。
// 成功時は結果セットの各行を `|` 区切りで結合した文字列を改行区切りで stdout へ
// 出力し終了コード 0（複数列を返す SQL でも列構成・型変換を検証できるよう全列を
// 出力する。`crates/wire-server/tests/three_client_e2e.rs` の `run_psql`／
// `run_psycopg` と同じ区切り規約）。
// 失敗時はエラーを stderr へ出力し終了コード 1（silent skip はしない）。SQLSTATE
// を伴う失敗（拒否経路の検証。TASK-165）は `[SQLSTATE=<code>]` を stderr
// メッセージに含める。commit 成功境界を跨いだ panic 時の緊急応答（TASK-97・
// TASK-153・ERR-5・Issue #706）が持つ detail フィールド（state=may_be_committed）
// は node pg の err.detail として読めるため、値がある場合は [DETAIL=<detail>]
// を続けて stderr メッセージに含める。接続断由来の後追い error イベント
// （ErrorResponse 受信後の切断）で無リスナーの uncaught 例外にならないよう
// client.on("error", ...) を登録する。

const host = process.env.WIRE_HOST;
const port = process.env.WIRE_PORT;
const user = process.env.WIRE_USER;
const password = process.env.WIRE_PASSWORD;
const sql = process.env.WIRE_SQL;

if (!host || !port || !user || password === undefined || !sql) {
  process.stderr.write("pg_client: missing required WIRE_* environment variables\n");
  process.exit(1);
}

let prelude = [];
const preludeRaw = process.env.WIRE_SQL_PRELUDE;
if (preludeRaw) {
  let parsed;
  try {
    parsed = JSON.parse(preludeRaw);
  } catch (e) {
    process.stderr.write(`pg_client: WIRE_SQL_PRELUDE is not valid JSON: ${e}\n`);
    process.exit(1);
  }
  if (!Array.isArray(parsed) || !parsed.every((s) => typeof s === "string")) {
    process.stderr.write("pg_client: WIRE_SQL_PRELUDE must be a JSON array of strings\n");
    process.exit(1);
  }
  prelude = parsed;
}

const wireSsl = process.env.WIRE_SSL;
if (wireSsl !== undefined && wireSsl !== "no-verify") {
  process.stderr.write(`pg_client: WIRE_SSL must be "no-verify" if set, got ${JSON.stringify(wireSsl)}\n`);
  process.exit(1);
}

const printTag = process.env.WIRE_PRINT_COMMAND_TAG;
if (printTag !== undefined && printTag !== "1") {
  process.stderr.write(`pg_client: WIRE_PRINT_COMMAND_TAG must be "1" if set, got ${JSON.stringify(printTag)}\n`);
  process.exit(1);
}

let pg;
try {
  pg = require("pg");
} catch (e) {
  process.stderr.write(`pg_client: pg module is not installed: ${e}\n`);
  process.exit(1);
}

const clientOptions = {
  host,
  port: Number(port),
  user,
  password,
  database: "irrelevant-db-name",
  connectionTimeoutMillis: 5000,
};
if (wireSsl === "no-verify") {
  clientOptions.ssl = { rejectUnauthorized: false };
}
const client = new pg.Client(clientOptions);

// ErrorResponse 受信直後の接続断（緊急応答経路。Issue #706）で pg が
// 後追いの "error" イベントを emit することがあり、リスナー未登録だと
// プロセス全体が uncaught 例外で異常終了してしまう。stderr へログだけ
// 残し、終了コード・メッセージの決定は下の .catch() に一本化する。
client.on("error", (e) => {
  process.stderr.write(`pg_client: connection error: ${e}\n`);
});

client
  .connect()
  .then(async () => {
    for (const stmt of prelude) {
      await client.query(stmt);
    }
    return client.query(sql);
  })
  .then((result) => {
    for (const row of result.rows) {
      process.stdout.write(`${Object.values(row).join("|")}\n`);
    }
    if (printTag === "1" && result.command) {
      // INSERT のタグは `INSERT <oid> <rows>`。pg は oid を result.oid へ保持するため、
      // 整数のときのみ 3 語形式で出力し 3 クライアントで完全一致比較できるようにする。
      // 整数でなければ従来の 2 語形式のまま出し、期待値不一致で fail-closed にする。
      if (result.command === "INSERT" && Number.isInteger(result.oid)) {
        process.stdout.write(`${result.command} ${result.oid} ${result.rowCount}\n`);
      } else {
        process.stdout.write(`${result.command} ${result.rowCount}\n`);
      }
    }
    return client.end();
  })
  .then(() => process.exit(0))
  .catch((err) => {
    let suffix = err && err.code ? ` [SQLSTATE=${err.code}]` : "";
    suffix += err && typeof err.detail === "string" ? ` [DETAIL=${err.detail}]` : "";
    process.stderr.write(`pg_client: query failed${suffix}: ${err}\n`);
    process.exit(1);
  });
