# 実装・テスト・レビュー面の残課題の棚卸し（2026-10）

Issue #1331（親 #1328・ルート #1323）の成果物。実装・テスト・レビューで残っている課題と、ドキュメントの書き漏れを集めた記録。
調査日は 2026-10-06、基準は `origin/main`（`1b3d517e`）。本記録は調べた結果の整理であり、CI 配線・テスト属性・挙動の変更は行わない。
spec の内容は書かず、ビヘイビア ID と Issue 番号のポインタのみを使う（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。

## 1. CI と定期ワークフローの状況

| 対象 | 状況 |
| ---- | ---- |
| `ci.yml`（main） | 直近の完了 run はすべて success。cancelled の 2 件は concurrency による取り消し |
| `bench.yml` の週次 schedule | 2026-09-07 から 2026-10-05 まですべて success |
| `recall.yml` の週次 schedule | 2026-09-07 から 2026-10-05 まですべて success |

失敗中の CI・定期ワークフローは無い。同じツリー（#1323）の docs 系 PR（#1453・#1454・#1455・#1457）はいずれも未マージで、`docs/design/implementation-status.md` の末尾に追記するため、マージ順によっては末尾で競合する（両方の節を残して解消する）。

## 2. 実施されていないテスト

| 項目 | 状況 |
| ---- | ---- |
| `scripts/crossdb_bench/tests/` | make ターゲットにも CI にも接続されておらず、どこからも自動実行されない |
| `scripts/tests/test_fsync_probe.py` | 同上 |
| `scripts/eval/tests/` | `make test-eval` で実行できるが、CI からは呼ばれない |
| 裸の `#[ignore]`（Rust） | 15 件（理由文字列なし）。`#[ignore = "..."]` は 56 件。裸の 15 件は周辺の doc コメントか make ターゲットで手動実行用と説明されている |
| 3 クライアント e2e（`make e2e-three-client*`） | opt-in で CI では動かない。設計どおり |

後続候補は 5 章に記す。

## 3. 対象外のまま残っている項目

範囲は `implementation-status.md` の `## Issue #1347` から `#1438`・`#1432` までの節にある「対象外」の箇条。実装記録の範囲で突き合わせた。

### 3.1 後の節で解消済み

| 出典の節 | 項目 | 解消先 |
| -------- | ---- | ------ |
| #1348 | NoSQL `create_table` の型拡張 | #1409 |
| #1348 | 配列の要素型 `NUMERIC`・`BYTEA`・`ENUM`・`JSON` | #1357 |
| #1354 | 先頭文が 0 行 `DELETE` の場合の再送判定 | #1403（wire・RETURNING・暗黙トランザクションは #1433） |
| #1356 | `ARRAY`／`JSON`／`JSONB` 列への DML の述語 | #1410 |
| #1357 | NoSQL filter の `in` | #1429 |
| #1359 | 数値 4 型の `CREATE INDEX` 宣言（INDEX-7） | #1413 |
| #1360 | 外側の集計・`DISTINCT`・ウィンドウ・式 `ORDER BY`・評価後射影形の連鎖 | #1411 |
| #1404 | 入れ子 WHERE スカラーサブクエリの 21000 | #1432 |
| #1411 | 連鎖本文の WHERE の式述語 | #1436 |
| #1412 | 名前付き UNIQUE・列制約の名前付き REFERENCES | #1428 |
| #1412 | NoSQL の名前付き主キー | #1437 |
| #1406 ほか | 大きな絶対値の浮動小数との比較（1e21 等） | #1438 |

### 3.2 実装記録上は未解消

| 出典の節 | 項目の要約 | 状態 |
| -------- | ---------- | ---- |
| #1347 | 拡張クエリ・明示トランザクション内の `RETURNING`、NoSQL・ベクトル列投影の層 B | 未解消 |
| #1349 | 集計・ウィンドウ関数の arity（構文層）、ARRAY・ENUM の `MIN`/`MAX` | 未解消 |
| #1351 | NoSQL `aggregate` の `sort` と `explain` の併用、wire 経由の層 B | 未解消 |
| #1353 | NoSQL バッチのトランザクション対応 | 未解消 |
| #1354 | `--fault-inject` を `COMMIT` へ広げること、明示トランザクション内の UPDATE／UPSERT／複数行 INSERT | 未解消 |
| #1355 | `INITIALLY DEFERRED` の FK、ROLLBACK 後の全テナント物理不変の専用検査 | 未解消 |
| #1358 | `NaN`／`Infinity`、scale 38 超の範囲比較リテラル | 未解消 |
| #1359・#1362・#1413 | 数値・`BYTEA` の `IN` の索引化、列同士・算術式の索引化 | 未解消 |
| #1406・#1407 | `NUMERIC`／`DATE`／`TIMESTAMP` ほかのバイナリ受信・表現 | 未解消 |
| #1409・#1427・#1428・#1437 | NoSQL の `constraints[].name`・列型変更・CHECK 参照列の精度拡大 | 未解消 |
| #1430・#1431 | CHECK 本体での NUMERIC／BOOLEAN の比較形、`<>` 周りの残り | 未解消 |
| #1434・#1435 | REAL／DOUBLE PRECISION の FK 列化 | 未解消（オーナー判断事項） |

各行は、対象節以降に解消の記録が見つからなかったという意味である。open Issue での追跡有無までは確認していない。

## 4. ドキュメントの書き漏れと是正

| 箇所 | 内容 | 本 Issue での扱い |
| ---- | ---- | ----------------- |
| CLAUDE.md | `detect-features.yml`・`update-external.yml` が構成ツリーにない | 是正 |
| CLAUDE.md | `ci.yml` の説明に `simd-codegen-check` がない | 是正 |
| CLAUDE.md | `scripts/` の説明に `crash_test_unique_index.sh`・`check_core_api.sh`・`check_simd_codegen.sh` がない | 是正 |
| CLAUDE.md | ルートの `.agents/skills/`・`CHANGELOG.md`・`LICENSE-MIT`／`LICENSE-APACHE` がない | 是正 |
| README | タスクランナー表に `make ci` のガード系・`make check-cross`・`make simd-codegen-check-cross`・`make test-eval` がない | 是正（要約行を追加） |
| CLAUDE.md・README | #1454 の `scripts/check_worktree_lag.sh` と `make worktree-lag-check` | #1454 が未マージのため反映せず。マージ後に追記する |

## 5. 後続の候補（Issue は起票していない）

- Python のユニットテスト（`scripts/crossdb_bench/tests`・`scripts/tests`・`scripts/eval/tests`）を make・CI に接続する。CI 変更を伴うため別 PR で扱う
- 裸の `#[ignore]` 15 件に理由文字列を付ける
- #1454 のマージ後に CLAUDE.md の構成ツリーと README のタスクランナー表へ反映する
- 3.2 の未解消項目のうち、オーナー判断が要るもの（REAL／DOUBLE PRECISION の FK 列化など）の扱いを決める
