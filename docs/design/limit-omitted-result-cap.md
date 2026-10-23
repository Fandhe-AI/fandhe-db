# LIMIT 省略時の結果行数上限（判定位置・SQLSTATE・カーソルとの関係）

## ステータス・対象

- ステータス: 採用（設計記録。#1796 で構文・計画への接続を実装済み。#1797〜#1798 は未着手）
- ポインタ: SQL-34・SQL-15・WIRE-15・ERR-6（spec 本文は転記しない）
- 対象: `LIMIT` 省略・`LIMIT ALL` の SELECT を「全件実体化 + 結果行数上限の超過でエラー」で受理するときの、上限判定の位置・超過時の SQLSTATE・カーソルとの関係・広域取得 Scan の走査上限との関係
- 先行: [result-limits-cli.md](result-limits-cli.md)（Issue #1555。フラグ `--max-result-rows`・`ResultLimits` を本 ADR が再利用する）
- 後続: #1796（パース・計画への接続）・#1797（上限付き実体化・走査停止）・#1798（wire 分割送出・キャンセル）

## 背景

`--max-result-rows`（既定 10,000・ハード上限 100,000）は、これまで明示 `LIMIT n` の範囲検査にだけ使われていた。`LIMIT` 省略・`LIMIT ALL` を受理するには、結果を実体化する段で行数を数えて上限を守る必要がある。実装 Issue が迷わないよう、判定の位置と失敗の形をここで固定する。

## 決定

### D1 判定位置（実体化段・`cap + 1` 方式）

- 判定は結果の実体化段で行う。`cap` を `ResultLimits::max_result_rows()` とし、省略時は内部の取得上限を `cap + 1` にして実行する。`rows.len() > cap` なら拒否する
- `cap` ちょうどで打ち切ると、超過を黙って切り詰める fail-open になる。必ず `cap + 1` 件まで読む
- 先例（同じ方式の既存実装）
  - `EngineCore::run_copy_to_scan`（COPY のテーブル形）
  - `INSERT ... SELECT` の `LIMIT cap + 1` 注入（[insert-select.md](insert-select.md)）
  - CTAS／MATERIALIZED VIEW（`core/ctas_exec.rs`・`core/matview_exec.rs`）
- 計数は、最終結果件数の下限が確定した時点（`cap + 1` 件そろった時点）で走査・結合を中断する。ただし中断してよいのは、後続の処理で件数が減らない経路に限る（次項）。上限を超える行は保持しない。走査停止の実装と境界テストは #1797 が担う
- `cap` の算出法は経路間で揃っていない（COPY は設定値そのまま、`INSERT ... SELECT`・matview は `min(cap, HARD_CAP - 1)`）。LIMIT 省略の読み取り経路は **COPY 方式（設定値 `cap` そのもの、`cap + 1` で取得）** に揃える。書き込み系の統一は #2212 に委ねる
  - 申し送り: `cap = 100,000` のとき `cap + 1` が下流の固定上限（`hybrid::MAX_POOL_DEPTH`・`MAX_SET_OP_ROWS`・`storage::MAX_SCAN_TOTAL_ROWS`・JOIN の行数上限）にかからないか、経路ごとに確認する
- ORDER BY（スカラー）・JOIN・ウィンドウは、全行を見ないと順序は確定しないが、行数は後続処理で減らない。「可視かつ一致する結果行が `cap + 1` 件に達した時点で超過は確定する」ので、ソートの完了を待たず中断してよい
- 早期中断（超過確定）は、最終結果件数の下限が確定する経路だけに限る。走査途中の暫定件数が、後続行の到着で減りうる経路では、暫定件数で超過を確定してはならない
  - HAVING 付き GROUP BY: HAVING は確定済みのグループに対して評価する契約（[order-by-having-expressions.md](order-by-having-expressions.md)）。走査途中で条件を満たすグループが `cap + 1` 個に達しても、後続行で集約値が変わり最終的に条件を外れうる（例: `HAVING COUNT(*) = 1`）。このため集約を最後まで行い、HAVING 評価後・OFFSET 適用後の件数で判定する。HAVING 適用前のグループ数では超過を確定しない（上限以内の結果を `54000` で誤って拒否しないため）
  - HAVING なしの GROUP BY・集約: グループは一度現れたら消えないので、グループ数が `cap + 1` に達した時点で確定してよい。ただし `MAX_GROUPS` など既存の上限は別軸として残す
  - DISTINCT・集合演算（重複除去を伴う UNION、INTERSECT、EXCEPT）など、後続行で行が消えうる・相殺されうる経路も同様に、重複除去・集合演算の完了後の件数で判定する。早期中断を許す経路は、実装（#1797）が下限確定を示せるものに限り、判断に迷う場合は完了後判定（安全側）に倒す
- `OFFSET m` との併用: 計数の対象は OFFSET 適用後の結果行。内部の取得上限は `m + cap + 1`（`checked_add`。溢れたら内部エラーへ fail-closed）。`OFFSET` の範囲は既存どおり `0..=10000`

### D2 束縛済み計画の多層防御との関係（#1796 への必須申し送り）

- 束縛済み計画の入口（`execute_bound_*_in_session`・`explain_bound_*`）は `check_bound_limit` で `bound.limit <= 設定値` を再検査する（[result-limits-cli.md](result-limits-cli.md) D5）。`limit = cap + 1` を運ぶ計画はそのままでは誤って `22000` になる
- 決定: 暗黙上限（省略・`ALL` 由来）には、明示 `LIMIT` と区別できる印を構文段・束縛段に持たせる。先例は `ValidatedInsertSelect::implicit_limit` と `ValidatedCopyTo::is_table_form`。印付きの計画は、束縛後に `bound.limit = cap + 1` へ差し替える（検証は迂回しない）
- D5 の再検査は、明示 `LIMIT` に対してだけ `22000` で拒否する。暗黙上限の計画には「`cap + 1` を超える取得上限を受け入れない」検査を適用する
- 印のフィールドは `pub(crate)` とし、検証経路だけが生成する。クレート外から偽装できる経路を作らない
- Describe でも暗黙上限を `22000` にしない（`bind_insert_select` と同じ扱い）

### D3 SQLSTATE の切り分け

| 事象 | SQLSTATE | 備考 |
| ---- | -------- | ---- |
| `LIMIT` 省略・`LIMIT ALL` で、可視結果が `cap` を超えた | `54000`（`SqlSurfaceError::payload_too_large`） | 文全体を拒否する。固定の英語文言（例: `result exceeds the row limit`）で、行の内容・テナント・件数を含めない |
| 明示 `LIMIT n` の範囲外（現状） | `22000` | #1796 で改訂するまで現行契約を維持する |
| 明示 `LIMIT n`（n > cap）で、ベクトル順位付けを持たない文（目標挙動） | 実結果が `cap` 以内なら受理、超えれば `54000` | 移行の判断と実装は #1796 に委ねる。それまでは `22000` |
| ベクトル順位付け（kNN・hybrid）の `LIMIT k` | `22000`（範囲 `..=cap`）を維持 | 距離 ORDER BY の省略は #2242 |
| `LIMIT 0`・負の `LIMIT`／`OFFSET` | 本 ADR では決めない | #1796 で扱う（SQL-34・ERR-6 ポインタ） |

### D4 部分結果を送らない

- engine は `QueryResult` を全件確定してから返す。超過は確定前に `Err` として返るので、結果は部分的にも存在しない
- simple query は確定後にまとめてエンコードする（`simple_query.rs` → `result_encoder`）。extended は Execute の初回で確定した結果を `max_rows` で分割送出する（`extended_query.rs` の `PortalState::Suspended`）
- よって判定は常に `max_rows` 分割より上流にあり、wire 層を変えずに「DataRow を 1 行も送らない」が成り立つ（WIRE-15 ポインタ）
- extended で Describe 済みの RowDescription が先に届くことは許容する（PostgreSQL と同じ）。送らないと保証するのは DataRow
- #1798 での確認事項: Execute の `max_rows` 分割が判定より上流に入り込んでいないこと

### D5 カーソル（DECLARE／FETCH）

- `DECLARE` の内側 SELECT で `LIMIT` 省略・`ALL` を受理し、`DECLARE` の時点で全行を数える。超過なら `54000` で `DECLARE` を拒否し、カーソルは登録しない。Active 中のエラーなので、既存規則どおり txn は `Failed` へ遷移する
- `FETCH` の途中で上限超過を検出して打ち切ることはない（[sql-cursor.md](sql-cursor.md) §1 の「`DECLARE` 時点で確定」と整合）
- 内側の実行は既存の `execute_cursor_inner_query` を通し、D1 と同じ `cap + 1` 経路を使う（第 2 の実行器を作らない）
- 判定順
  1. 同時カーソル数の上限（実行前）
  2. 結果行数上限（実行中に下限が確定した `cap + 1` 件で中断。HAVING 付き集約などは完了後の件数で判定）
  3. バイト予算 `MAX_CURSOR_BYTES_PER_SESSION`（16 MiB。生成中・登録時）

  2 と 3 は独立した上限で、どちらも `54000`。先に達した方が拒否する
- `FETCH n` の範囲は `validate_search_limit_with` で設定値に追随済み。`FETCH 0` の受理は #1796 の範囲（SQL-34 ポインタ）

### D6 走査上限との関係（3 つの軸）

| 軸 | 内容 |
| -- | ---- |
| 結果行数上限 `--max-result-rows` | 可視かつ一致する結果行だけを数える |
| バイト予算 | 広域取得の `MAX_SCAN_RESULT_BYTES`（1 GiB）・カーソルの 16 MiB・集計の `MAX_AGGREGATE_RESULT_BYTES`。超過はいずれも `54000` |
| 走査行数上限 `--max-scanned-rows` | DML 専用。[scan-limits-cli.md](scan-limits-cli.md) の「対象外」が SELECT を除いている |

- SELECT の処理量を抑えるのは「`cap + 1` での早期終了」とバイト予算。選択性の高い WHERE を持つ LIMIT 省略は、表全体を走査しうる。これは既存の明示 `LIMIT` と同じ性質で、本 ADR では変えない
- `storage::MAX_SCAN_TOTAL_ROWS`（100,000）は `Storage::scan` の一括確保上限で、広域取得の逐次走査ループ（[wide-retrieval-scan.md](wide-retrieval-scan.md)）とは別物

### D7 RLS（RLS-7・RLS-8・RLS-10 ポインタ）

- 不可視行はデコード前の `is_visible` 判定で計数から外れる。`54000` が出るかどうかから、他テナントの行の存在や件数は観測できない
- [wide-retrieval-scan.md](wide-retrieval-scan.md) の「RLS・早期終了の相互作用」の議論を、結果行数上限にもそのまま当てはめたもの

## 効く経路（#1796 で受理する範囲）

| 経路 | 扱い |
| ---- | ---- |
| 広域取得・スカラー ORDER BY・GROUP BY／DISTINCT・JOIN・集合演算全体・ウィンドウ・ビュー参照 | 効く（#1796） |
| `DECLARE CURSOR` の内側 SELECT | 効く（D5） |
| NoSQL（HTTP）`scan` の `limit` | 必須のまま（上限値の共有のみ） |
| 書き込み系入力（`INSERT ... SELECT`・CTAS・matview・COPY） | 既存の `cap + 1` 実装。統一は #2212 |
| 部分クエリ内側の `LIMIT` 省略 | #1852・#1853 |
| 距離 ORDER BY の省略 | #2242 |

## 対象外

- キャンセル・切断時の走査停止の仕組み（#1797・#1798。engine に現状キャンセルフラグは無く、仕組みは約束しない）
- 距離 ORDER BY の省略・内側 `LIMIT` の省略・CLI の統一
- `LIMIT 0`・負値の扱い
- SELECT への走査行数上限の適用

## 実装メモ（Issue #1796）

- 構文段: `LIMIT {n | ALL}` と `OFFSET` 単独を `Parser::parse_optional_limit` が受理する（`ALL` と省略は同義の `None`）。広域取得は `ValidatedScan::limit_implicit`（番兵 `limit = u32::MAX`）、評価後射影形ビューの外側は `BufferedRows::limit_implicit` で印を持つ。印を見落とした経路は番兵値が `22000` で落ちる（fail-closed）
- 束縛段: `bind_scan_with_dummy_flags` が印付きの文を取得上限 `cap + 1`・`BoundScan::result_cap = Some(cap)` に差し替える。`EngineCore::check_bound_scan_limit` は印付きの計画を「`cap` が設定値と一致し取得上限が `cap + 1` 以下」のときだけ通す。NoSQL 表層の `BoundScan::new` は印を持たず契約は不変
- 実行段: 広域取得は `sql::scan::execute_scan_with_budget` の入口 1 か所で `rows.len() > cap` を `54000` にする（Scan・ウィンドウ・cursor の内側・COPY・INSERT…SELECT・CTE・ビュー畳み込みが共有）。JOIN・`GROUP BY`／DISTINCT（HAVING と OFFSET の適用後）・集合演算全体・評価後射影形ビューの外側は各経路の結果に同じ判定をかける。文言は固定で件数・テナントを含めない
- `MAX_SET_OP_ROWS`（固定 10,000）は別軸として残る。cap を 10,000 より上げても集合演算は 10,000 で `54000` になる
- 従来どおり `42601`: 距離 ORDER BY・`USING PLAN` の省略（#2242）、サブクエリ内側・集合演算の括弧付き枝・ビュー／CTAS／matview の本文の省略（#1852／#1853／#2212）
- 現行契約を維持（本 Issue では見送り）: `LIMIT 0`・明示 `LIMIT n (n > cap)` は `22000`（SQL-34・ERR-6 ポインタ）。`validate_search_limit_with` は kNN と `FETCH n` でも共有されるため影響範囲が広い
- #1797 へ: `cap + 1` 件到達時の走査・結合・集約の早期中断、メモリ頭打ちのテスト、キャンセル。#1798 へ: wire の分割送出
