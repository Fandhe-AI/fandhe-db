# SELECT DISTINCT ON の構文解析と ORDER BY 先頭一致の検証

- ステータス: 実装記録（Issue #3260・#3261・#3262。親 #3259）
- 対応ビヘイビア: SQL-25 のポインタ（`docs/spec/04-behavior/sql-surface.md`）。spec 本文は転記しない
- 依存・unsafe の変更なし

## 1. 範囲

Issue #3260 は構文解析・個数上限・`ORDER BY` 先頭一致の検証、#3261 は行を返す実行段（ソート後重複排除）を担う。`ORDER BY` なしの決定性の固定・`sort-determinism-check` への登録・RLS 越境の網羅試験は #3262 が担う（§6）。

整合した文は、セッション最上位の入口に限り受理して行を返す（§5）。それ以外の入口と未対応の形は、通常の広域取得として黙って実行せず `0A000` で fail-closed に拒否する（重複排除を黙って落とした結果を返さない）。

## 2. 判定順序

| 順 | SQLSTATE | 内容 |
| -- | -------- | ---- |
| 1 | `42601` | 構文（空リスト・閉じ括弧欠落・末尾カンマ・SELECT リスト欠落） |
| 2 | `54000` | `ON` リストの式数が `MAX_SCALAR_ORDER_KEYS`（8）超。確保前に検査 |
| 3 | `42P01` | FROM の関係が未知（`view::resolve_from`） |
| 4 | `42703`／`42P10` | `ORDER BY` 先頭との不一致。直前に未知列を先に判定する（§4） |
| 5 | `0A000` | 整合しているが実行段へ運べない文脈・形（§5） |
| 6 | `54000` | 実効ソートキー（ON キー＋`ORDER BY`）が `MAX_SCALAR_ORDER_KEYS` 超 |

- 集計・`GROUP BY`・`HAVING`・ウィンドウ・距離順位付け・`USING PLAN`・投影のスカラーサブクエリとの併用は `0A000`
- 検出位置は共有の検証経路 `validate_select_statement`。Parse／Describe・ビュー本文・カーソル等の入口は同じ分類になる
- CTE 本文・WITH 主クエリは `parse_distinct_shape` 冒頭で `0A000`。JOIN・集合演算・派生表・FROM なしは従来どおり拒否のまま（#3259）
- 列名 `on` は `DISTINCT` 直後でも、3 番目のトークンが `(` でなければ従来の経路で解釈する

## 3. 先頭一致の規則

PostgreSQL と同じ。`ORDER BY` キーを先頭から見て、`ON` 集合に含まれるキーは結果列へ加え、含まれないキーを読み飛ばした後に `ON` 集合のキーが現れたら `42P10`。さらに `ON` のキーで結果列に無いものがあり読み飛ばしが起きていれば `42P10`。`ASC`／`DESC`・`NULLS` は影響しない。`ORDER BY` なしは整合（決定性は §6）。

- 比較は式の構造的等価（識別子は書かれたとおりに比較）
- 出力別名・序数は、一意に解決できるときだけ式へ置き換えて比較する。曖昧・位置が確定しない（`*` 含む）・範囲外は推測せず `0A000`
- ビューの別名書き換えより前の、利用者が書いた名前どうしで比較する

## 4. 既知の差と引き継ぎ

- 未知列: PostgreSQL は未知列を `42P10` より先に判定する。本実装は、先頭一致を拒否する直前に限り、実テーブルの列集合で ON 式・`ORDER BY` キーの裸の列参照を照合して `42703` を先に返す（もともとエラーになる経路で SQLSTATE が変わるだけ）。整合した文の未知列は束縛段の `42703`。ビュー越し・式の内側の列参照は先行判定の対象外で、この場合だけ `42P10` が先に出る（受け皿 #3259）
- 実効ソートキー数の上限: ON キーと `ORDER BY` の合計（重複除く）が `MAX_SCALAR_ORDER_KEYS`（8）を超えると `54000`。PostgreSQL にはこの上限がない（受け皿 #3259）
- `INTERVAL` 型の ON キーは `0A000`（グループ同一性の規則が未確定。`GROUP BY`／`DISTINCT` と同じ扱い）（受け皿 #3259）
- `ORDER BY` なしの決定性の固定・`sort-determinism-check` への登録・RLS 越境の網羅試験: §6（Issue #3262 で完了）
- JOIN・集合演算・派生表・CTE・サブクエリ内側・評価後射影形ビュー・カーソル・`COPY`・`INSERT ... SELECT`・CTAS・ビュー本文・`EXPLAIN` 文脈の DISTINCT ON: #3259

## 5. 実行段（Issue #3261）

### 受理する形

セッション最上位の SELECT（簡易・拡張クエリ。`validate_session_sql_tokens`）の、単一表または畳み込み可能ビューへの `SELECT DISTINCT ON (...) <投影> FROM <表> [WHERE] [ORDER BY] [LIMIT/OFFSET]`。`WHERE` の `IN`／`EXISTS` サブクエリは通常の広域取得と同じに解決される。

次は `0A000`: 他の入口（上記）・評価後射影形ビュー・`FETCH ... WITH TIES`・式内のスカラーサブクエリ・`SELECT ... FOR UPDATE／SHARE`・集計／`GROUP BY`／`HAVING`／ウィンドウ・距離順位付け・`USING PLAN`。

### 実効ソートキーとグループ

- PostgreSQL と同じく、`ORDER BY` に無い ON キーは `ASC` で末尾へ暗黙に足す。実効キー列の先頭 `k`（異なり ON キー数）個が ON 集合で、これが等しい行が 1 グループ
- 実効キーは `ValidatedScan::order_keys` に全キーを入れ（`order_by` は空）、通常のソートキーとして束縛段・ビュー畳み込み（公開列検査・別名の物理名書き換え）に乗る。第 2 の名前解決器は持たない。非公開列を ON 経由でグループ化の oracle にはできない
- `ValidatedScan::distinct_on`／`BoundScan::distinct_on` が前置長 `k` を運ぶ。束縛段が `order_by.len() >= k`、ウィンドウ・`WITH TIES` 非併用、`INTERVAL` キー拒否を再確認する（多層防御）

### 実行（`sql::scan` 経路 (B)）

- `distinct_on` を持つ文は必ず経路 (B)（`id` 先頭の範囲走査は使わない）。`BinaryHeap` の代わりに、全順序で整列したグループ代表行の `Vec` を保持する。グループ間の順序は前置 `k` キーだけで決まるため二分探索で位置を引ける
- 同一グループは代表行より全順序で前の行だけが置き換える。新グループは容量（`limit + offset`）まで挿入し、満杯なら最悪グループより前のときだけ挿入して末尾を追い出す。追い出したグループの行は後から来ても復帰しない
- NULL 同士・NaN 同士・`-0.0` と `0.0` は既存の比較器で同順位のため同一グループ（PostgreSQL の `DISTINCT` と一致）。照合は deterministic で、等値はバイト列の等値と一致する
- 重複排除は `OFFSET`／`LIMIT` より前。`LIMIT` 省略・`LIMIT ALL` の上限（`54000`）は入力行数ではなくグループ数に掛かる（可視一致行数による早期中断は使わない）。保持量は `limit + offset` 以下に有界で、挿入は O(n) の移動だが n は `MAX_SEARCH_K` の 2 倍以下
- 予算は既存の不変条件（計上額 ≥ 同時保持量）を保ち、確保前に照合する。置き換え・追い出しは drop 後に解放する
- RLS: 候補判定はすべて `with_visible_row`（RLS → TABLE-12 → デコード → `WHERE` → 可視性の再判定）の内側。不可視行は代表行・グループ数・`54000` の成否に現れない。エラー文言は固定の英語で件数・値・テナントを含めない

### 入口の分類

受理は `validate_select_statement` の `allow_distinct_on`（`AllowMaterialize` かつ深さ 0 の入口だけが真）で制御する。ビュー本文・CTAS・カーソル・サブクエリ内側・`INSERT ... SELECT`・`COPY`・`EXPLAIN` は偽のため `0A000`。`scan_has_unrendered_explain_shape` と `check_row_locking` にも DISTINCT ON の明示拒否を置く。

## 6. ORDER BY なし・同点の決定性（Issue #3262）

PostgreSQL は `ORDER BY` なし（および ON キーだけの `ORDER BY`）で、どの行がグループ代表になるかを定めない。本実装は暗黙のキー順ソートを持つ分岐を採り、PostgreSQL が返しうる結果のうち 1 つを決定的に選ぶ。新しいソートは追加せず、既存の挙動を契約として固定する。

### 契約

- 実効キーは ON キーの `ASC`（NULL は末尾）。グループ順はこれで決まる
- 同点（実効キーが等しい行）は `id` 昇順、続いて `tenant_id` のバイト順で全順序にする。代表はグループ内でこの順序が最小の可視行
- `ORDER BY grp DESC`・`NULLS FIRST` はグループ順だけを変え、代表の選び方は変えない
- 結果は挿入順・走査順・実行回数に依存しない

### 不変条件（sort-determinism-check）

- グループ保持は整列済み `Vec` と二分探索で行い、`sort_unstable_by*`・`select_nth_unstable_by*` を使わない
- `scripts/check_sort_determinism.sh` は `crates/engine/src` を全体走査するため、本経路（`sql/scan.rs`）は既に監視対象で、`sort-determinism: allow` による除外も無い。表の登録先は `docs/design/rrf-tie-break-determinism.md`

### RLS（テナント境界）

代表選択・グループ数・`54000` の成否は、可視性判定を通過した行だけで決まる。他テナントの public 行は可視なので代表になりうる。他テナントの private 行は代表にならず、グループ数・上限判定にも数えられない。エラー文言は件数・値・テナント名を含まず全テナントで同一。

### 固定しているテスト

- `crates/engine/tests/sql_t01_39/sql25_distinct_on_determinism.rs`: 代表の (id, tenant) 最小、ON キーのみの `ORDER BY`、再現性と挿入順非依存、不可視行の非影響（private 行を除いた対照 DB との一致）、`LIMIT`／`OFFSET` の総当たり、`LIMIT` 省略・`LIMIT ALL` の上限、不可視グループが上限に影響しないこと、未対応形の fail-closed
- `crates/engine/tests/sql_t01_39/sql25_distinct.rs`: `ORDER BY` なしの代表 id を順序込みで比較
- `crates/wire-server/tests/wire_sql/wire_sql25_distinct.rs`: 簡易クエリ（末尾 `;` 単独送信を含む）・拡張クエリでの固定行
