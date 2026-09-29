#!/usr/bin/env node
// node `pg`（無改造）で wire-server へ拡張クエリプロトコルの 1 文を送る型付きクライアント。
//
// `crates/wire-server/tests/three_client_extended_e2e.rs`（Issue #1176・WIRE-11／12／13／14、
// `#[ignore]`）から子プロセスとして起動される層 B ハーネス。簡易クエリ経路を固定する
// `pg_client.js` とは責務を分け、こちらは values 付き／`queryMode: 'extended'` で
// Parse／Describe／Bind／Execute を駆動する。
//
// 環境変数（argv／stdin は使わない。語彙外は fail-closed で終了コード 1）:
// WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD / WIRE_SQL: 必須。
// WIRE_PARAMS（任意）: 文字列の JSON 配列（values として束縛）。
// WIRE_BINARY（任意）: "1" のみ受理。結果をバイナリ形式で要求する。node pg は bytea(17)・
//   uuid(2950) のバイナリ parser を持たないため、公開 API `pg.types.setTypeParser` で
//   登録する（ドライバ無改造）。なお node pg はバイナリ DataRow を UTF-8 文字列として
//   読み戻すため、0x80 以上のバイトを含む値は壊れる（ドライバ側の制約。fixture は
//   全バイト < 0x80 の行に限定する。ADR 参照）。
// WIRE_EXTENDED_NOPARAM（任意）: "1" のみ受理。values 無しでも拡張プロトコルを強制する。
//
// 出力: 各行のセルを `<型名>:<正準表現>` にして `|` 連結し改行区切りで stdout へ。
// Buffer は小文字 hex。失敗時は stderr に `[SQLSTATE=<code>]` を含めて終了コード 1。

const host = process.env.WIRE_HOST;
const port = process.env.WIRE_PORT;
const user = process.env.WIRE_USER;
const password = process.env.WIRE_PASSWORD;
const sql = process.env.WIRE_SQL;

function fail(msg) {
  process.stderr.write(`pg_extended: ${msg}\n`);
  process.exit(1);
}

function flag(name) {
  const raw = process.env[name];
  if (raw === undefined || raw === "") return false;
  if (raw !== "1") fail(`${name} must be "1" if set, got ${JSON.stringify(raw)}`);
  return true;
}

if (!host || !port || !user || password === undefined || !sql) {
  fail("missing required WIRE_* environment variables");
}

let values;
if (process.env.WIRE_PARAMS) {
  try {
    values = JSON.parse(process.env.WIRE_PARAMS);
  } catch (e) {
    fail(`WIRE_PARAMS is not valid JSON: ${e}`);
  }
  if (!Array.isArray(values) || !values.every((s) => typeof s === "string")) {
    fail("WIRE_PARAMS must be a JSON array of strings");
  }
}
const binary = flag("WIRE_BINARY");
const noparamExtended = flag("WIRE_EXTENDED_NOPARAM");

let pg;
try {
  pg = require("pg");
} catch (e) {
  fail(`pg module is not installed: ${e}`);
}

if (binary) {
  pg.types.setTypeParser(17, "binary", (buf) => buf);
  pg.types.setTypeParser(2950, "binary", (buf) => {
    const h = Buffer.from(buf).toString("hex");
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  });
}

const client = new pg.Client({
  host,
  port: Number(port),
  user,
  password,
  database: "irrelevant-db-name",
  connectionTimeoutMillis: 5000,
});
client.on("error", (e) => {
  process.stderr.write(`pg_extended: connection error: ${e}\n`);
});

const config = { text: sql, binary };
if (values !== undefined) config.values = values;
if (noparamExtended) config.queryMode = "extended";

function cell(v) {
  if (Buffer.isBuffer(v)) return `Buffer:${v.toString("hex")}`;
  return `${typeof v}:${String(v)}`;
}

client
  .connect()
  .then(() => client.query(config))
  .then((result) => {
    for (const row of result.rows) {
      process.stdout.write(`${Object.values(row).map(cell).join("|")}\n`);
    }
    return client.end();
  })
  .then(() => process.exit(0))
  .catch((err) => {
    const suffix = err && err.code ? ` [SQLSTATE=${err.code}]` : "";
    process.stderr.write(`pg_extended: query failed${suffix}: ${err}\n`);
    process.exit(1);
  });
