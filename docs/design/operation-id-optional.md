# OPERATION_ID の任意化の方針（ポインタ）

## ステータス・対象

- ステータス: 設計段階（実装は後続 Issue）。詳細な契約は公開境界の確認が済むまで本メモに記載せず、ポインタ表記に留める（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 親: #1701（operation_id を既定で省略可能にし、従来の必須契約は opt-in の CLI フラグ `--require-operation-id` で選べる方針）
- ビヘイビア ID: RECOVER-1（`docs/spec/04-behavior/recovery.md`）・HTTP-14（NoSQL 表層）。補助として RECOVER-2・RECOVER-7・SQL-10・ERR-2
- 関連: [operation-id-required-guard.md](operation-id-required-guard.md)・[operation-id-ledger.md](operation-id-ledger.md)・[partitioned-dml.md](partitioned-dml.md)・[error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)

## 背景

現状は書き込み文と NoSQL の書き込み op で operation_id が必須であり、省略すると `23502` で拒否される（`crates/engine/src/recovery/required_op_id.rs`）。親 #1701 の方針に沿って任意化を進める。

## 後続 Issue の担当

| Issue | 担当 |
| ----- | ---- |
| #1703 | `--require-operation-id` フラグ（wire-server） |
| #1704 | engine 側の任意モードと SQL 経路 |
| #1705 | 回帰テスト |
| #2203 | NoSQL（HTTP）経路 |

任意モードの既定値・例外・照会契約・設定制約・経路別の扱いなどの詳細は、公開境界の確認後に各 Issue で確定し、必要なら本メモを追記する。

## Issue #1703 追記

- `--require-operation-id` は値を取らない起動時スイッチ。解決は `crates/wire-server/src/require_operation_id_opt.rs` の `resolve` に一本化し、`EngineCore::with_ledger_mode` へ 1 回だけ注入する（セッション・SQL・HTTP からは変更できない）
- 指定時は `LedgerMode::Ledgered`（現行の必須契約）。未指定時は `LedgerMode::default()` を注入するだけで、現時点の挙動は変わらない。任意モードの variant 追加と既定の移行は #1704 の担当
- 未指定を `CompareOnlyWithoutLedger` へ写像しない（台帳書き込みが無効になり再送の冪等性が失われるため）
- 重複指定・`=` 連結形・後続の値は起動エラー（fail-closed）。指定時のみ起動ログに英語 1 行を出す
- 受け入れ条件のうち「未指定時に省略を受理する」実挙動は #1704 で成立した（下記）

## Issue #1704 追記

- `LedgerMode::Optional` を追加し、engine の既定を `Ledgered` から `Optional` へ移した。`--require-operation-id`（`Ledgered`）は従来の必須契約のまま。wire-server の `resolve(false)` は `LedgerMode::default()` を返すためソース変更は不要
- 省略した書き込み（句の欠落・明示 `NULL`）は `LedgerWrite::Omitted` に写像し、台帳にもジョブ表にも触れない通常の書き込みとして実行する（再送の検出はしない）。指定した場合の記録・重複拒否（`23505`）・内容不一致（`22023`）・照会は不変
- `LedgerWrite::Disabled`（台帳テーブルを持たない構成）は流用しない。意味が異なり、分割実行器が `op_id()==None` を `0A000` に写すため、`23502` という契約とずれるため
- 分割実行 DML は構成に関わらず operation_id 必須（`require_partitioned`）。SQL 検証（述語形 UPDATE／DELETE。`EXPLAIN ... PARTITIONED` を含む）・NoSQL から呼ばれる engine 入口・共通本体の 3 箇所で検査し、実行器の `0A000` は最後の防御として残す。構成が `CompareOnlyWithoutLedger` でも `23502` になる
- 省略した書き込みは台帳に触れないため `last_op`（RECOVER-7 の `last_operation_id` 照会）を更新しない。照会の対象は指定付き書き込みのみで、省略書き込みの後も直前の指定付き operation_id を返す（最終書き込みとは限らない）
- 切り替えは起動時のフラグのみ。セッション・SQL からは変更できない
- 既定に依存して省略の拒否を検証していた既存テストは `LedgerMode::Ledgered`（またはバイナリ起動時の `--require-operation-id`）を明示して固定した（アサーションは変更していない）
- 保護が必要な運用（冪等な再送を前提とするクライアント）は `--require-operation-id` を使う
- 対象外: NoSQL／HTTP 経路の wire 層検査（#2203）、既定と必須の行列回帰（#1705）

## Issue #1705 追記

- 実装は変えず、「`operation_id` 省略／指定」×「既定（任意）／`--require-operation-id`（必須）」の行列を回帰テストで固定した（RECOVER-1 ポインタ）
- engine 層: `crates/engine/tests/recovery_ledger_mode_matrix.rs`。省略形（INSERT 単一・複数行・UPSERT・UPDATE／DELETE の id 形と述語形・TRUNCATE・明示 `NULL`）のモード別結果、指定時の契約（記録・再送 `23505`・内容不一致 `22023`）がモード間で一致すること、分割実行 DML（SQL・束縛済み入口）が両モードで `23502` であること、SQL の `SET`／`RESET`／`ALTER SYSTEM` で両方向とも切り替えられない（`42601`）ことを固定する
- wire 層: `crates/wire-server/tests/wire_require_operation_id_matrix.rs`。簡易クエリ・拡張クエリ・COPY の行列に加え、wire 越しの `SET`、StartupMessage の追加パラメータ（`options`・素のキー）、HTTP のクエリ文字列でモードが変わらないことを固定する。モードは CLI の解決関数経由で決める
- 追跡事項: 拡張クエリの `USING OPERATION_ID $n` に `NULL` を束縛すると、既定でも bind 段で `23502` になる（リテラルの `NULL` は既定で受理）。任意化との整合は別途扱うため、本 Issue では固定していない
- 対象外: HTTP の書き込み op の省略時の挙動（#2203）

## Issue #2203 追記

- NoSQL（HTTP）の `insert`／`update`／`delete` が wire 層で行っていた前段拒否（欠落・`null`・空文字を `parse("")` で `23502` にする処理）を撤去した。3 op とも `operation_id` を `Option<OperationId>` として解釈し、欠落・`null`・空文字（完全一致のみ。trim 等はしない）は `None`、それ以外は従来どおり `OperationId::parse`（長さ超過・制御文字は `22000`）を通す（RECOVER-1・NOSQL-6・NOSQL-12 ポインタ）
- 省略の可否の判定は engine の 7 つの束縛済み入口（insert／update／delete／述語形 update・delete／分割実行 update・delete）の `LedgerMode`（および分割実行の `require_partitioned`）に一本化した。wire 層にモードの getter や独自の分岐は置かない。既定（`Optional`）では台帳に記録しない通常の書き込みとして成功し、`--require-operation-id`（`Ledgered`）では `23502`。判定はスキーマ取得より前にあるため、必須構成で欠落の `23502` が `42P01` より先に返る順序は不変
- 省略した書き込みの成功応答本文は `operation_id` キーを含まない（`{"inserted":n}`／`{"updated":n}`／`{"deleted":n}`。TASK-240 ポインタ）。指定時は従来どおりキーを echo する。クライアントから見える契約の変更のため BREAKING CHANGE
- 分割実行（`"mode":"partitioned"`）の update／delete は既定でも欠落を `23502`。`show_partitioned_dml`／`cancel_partitioned_dml` は書き込みではなくジョブの識別子なので、両モードで必須のまま
- `Idempotency-Key` ヘッダ（HTTP-14 ポインタ・Phase 8・未実装）の前提: 本文の `operation_id` と同じ台帳・同じ意味で扱い、必須構成ではヘッダか本文のどちらかで要件を満たす。ヘッダ自体の実装は本 Issue の対象外
- 追跡事項（別 Issue）: SQL 表層の `USING OPERATION_ID ''` は既定でも `23502` のままで、NoSQL の空文字＝省略と食い違う。拡張クエリの `USING OPERATION_ID $n` への `NULL` 束縛も既定で `23502`（#1705 の追跡事項）
- 既定に依存して欠落の `23502` を検証していた既存テスト（`err4_http_projection.rs`・`nosql12_update_delete.rs`・`nosql1_op_vocabulary.rs` と `insert.rs` の単体テスト）は `LedgerMode::Ledgered` を明示して固定した（アサーションは変更していない）
- テスト: `crates/wire-server/tests/wire_require_operation_id_matrix.rs` の M7（M7a 省略 × 両モード、M7b 台帳に触れないこと、M7c 指定時の再送契約が両モードで同一、M7d 分割実行は両モードで `23502`、M7e show／cancel は両モードで `23502`）と、各 op の `encode_success_body` 単体テスト

## スコープ外

- コード変更（後続 Issue で実施）
- `docs/spec` の変更
