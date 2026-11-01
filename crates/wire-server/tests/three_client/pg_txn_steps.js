#!/usr/bin/env node
// node `pg`（無改造）で明示トランザクション内の述語形書き込みと 40001 の再試行
// （Issue #2412・SQL-31・SQL-19・ERR-7）を観測するクライアント。
//
// `crates/wire-server/tests/three_client_txn_retry_e2e.rs`（`#[ignore]`）から子プロセスとして
// 起動される層 B のドライバ。`psycopg_txn_steps.py` と同じ環境変数・出力形式で、1 プロセス内で
// 主セッションと他セッション群の `pg.Client` を開き、文の列を逐次に実行する。
// 簡易モードは `values` 無しの `query({text})`（簡易クエリ）、拡張モードは
// `{text, values, queryMode: 'extended'}`（Parse/Bind/Execute）で送る。
// `BEGIN`/`COMMIT`/`ROLLBACK` も同じ経路で文として送る。
//
// 環境変数（すべて必須。語彙外は終了コード 1）:
// - WIRE_HOST / WIRE_PORT: 接続先。
// - WIRE_CREDENTIALS: ユーザー名 -> ダミーパスワードの JSON オブジェクト。
// - WIRE_TXN_MODE: `simple` または `extended` のみ。
// - WIRE_TXN_STEPS: `[{"s","u","sql","p","close"}]` の JSON 配列（psycopg 版と同じ）。
//
// 出力: 文ごとに結果行（SELECT）またはコマンドタグ、続けて `@@<idx> <SQLSTATE>`。失敗時は
// `@@msg <idx> <メッセージ>` も出して続行する。SQL 本文・資格情報は出力しない。

const host = process.env.WIRE_HOST;
const port = process.env.WIRE_PORT;
const mode = process.env.WIRE_TXN_MODE;
const credsRaw = process.env.WIRE_CREDENTIALS;
const stepsRaw = process.env.WIRE_TXN_STEPS;

function fail(msg) {
  process.stderr.write(`pg_txn_steps: ${msg}\n`);
  process.exit(1);
}

if (!host || !port || !credsRaw || !stepsRaw || (mode !== "simple" && mode !== "extended")) {
  fail("missing or invalid WIRE_* environment variables");
}

let creds;
let steps;
try {
  creds = JSON.parse(credsRaw);
  steps = JSON.parse(stepsRaw);
} catch (e) {
  fail("invalid JSON");
}
if (typeof creds !== "object" || creds === null || !Array.isArray(steps)) {
  fail("WIRE_CREDENTIALS must be an object and WIRE_TXN_STEPS an array");
}

let pg;
try {
  pg = require("pg");
} catch (e) {
  fail(`pg module is not installed: ${e}`);
}

const KEYS = ["close", "p", "s", "sql", "u"];

async function main() {
  const clients = new Map();
  try {
    for (let idx = 0; idx < steps.length; idx++) {
      const step = steps[idx];
      if (JSON.stringify(Object.keys(step).sort()) !== JSON.stringify(KEYS)) {
        fail("steps must have exactly s, u, sql, p, close");
      }
      if (!(step.u in creds) || !Array.isArray(step.p)) {
        fail("unknown user or malformed params");
      }
      let client = clients.get(step.s);
      if (!client) {
        client = new pg.Client({
          host,
          port: Number(port),
          user: step.u,
          password: creds[step.u],
          database: "irrelevant-db-name",
          connectionTimeoutMillis: 5000,
        });
        await client.connect();
        clients.set(step.s, client);
      }
      const config = { text: step.sql, rowMode: "array" };
      if (mode === "extended") {
        config.values = step.p;
        config.queryMode = "extended";
      }
      try {
        const res = await client.query(config);
        if (res.command === "SELECT") {
          for (const row of res.rows) {
            process.stdout.write(`${row.map((c) => String(c)).join("|")}\n`);
          }
        } else {
          const tag = res.rowCount === null ? res.command : `${res.command} ${res.rowCount}`;
          process.stdout.write(`${tag}\n`);
        }
        process.stdout.write(`@@${idx} 00000\n`);
      } catch (err) {
        const msg = String(err.message || "").replace(/\n/g, " ");
        process.stdout.write(`@@${idx} ${err.code || "UNKNOWN"}\n@@msg ${idx} ${msg}\n`);
      }
      if (step.close) {
        const c = clients.get(step.s);
        clients.delete(step.s);
        await c.end();
      }
    }
  } finally {
    for (const c of clients.values()) {
      try {
        await c.end();
      } catch (_) {
        // 終了処理の失敗は無視（本体の失敗理由を優先する）
      }
    }
  }
}

main().then(
  () => process.exit(0),
  (e) => fail(`failed: ${e && e.name}`),
);
