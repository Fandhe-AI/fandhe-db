# ADR: 広域取得へウィンドウ関数を追加する（Issue #930）

- ステータス: Implemented（実装既定値。spec 側ビヘイビア ID は SQL-30・TASK-214
  として付与済み〔`docs/spec/05-tasks.md`・`docs/spec/04-behavior/sql-surface.md`〕。
  RLS 側は `docs/spec/04-behavior/rls.md` RLS-10 (b) の管轄。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)））
- 対応: Issue #930・spec 側 SQL-30・TASK-214
- 関連ポインタ: SQL-15（広域取得）・SQL-13/SQL-14（集計・`GROUP BY`）・SQL-25
  （スカラー `ORDER BY`／`DISTINCT`。本 Issue の対象外の根拠）・RLS-7・RLS-8・
  RLS-10 (b)・ERR-1/2/4（`wire_code` 契約）
- 検証コード: `crates/engine/src/sql/allowlist.rs`（構文受理・拒否の単体テスト）・
  `crates/engine/src/sql/parser.rs`（束縛・WHERE 別名参照拒否の単体テスト）・
  `crates/engine/tests/sql30_window.rs`（受理・値・決定性・拒否マトリクス・RLS
  不変性・cursor の結合テスト）

## 背景

広域取得（SQL-15。`docs/design/wide-retrieval-scan.md`）は 1 行ずつの結果しか
返せず、パーティション内の順位・累積集計を付けることができなかった。既存の
集計経路（`GROUP BY`。SQL-13・SQL-14）は複数行を 1 グループへ畳み込むため、
「元の行を保ったまま順位・累積値を添える」用途には使えない。

## 設計方針

### 受理範囲

対象は広域取得（`SELECT <投影> FROM t [WHERE ...] LIMIT n [OFFSET m]`。SQL-15
形）の投影に置くウィンドウ項目のみ。

| 形 | 扱い |
| -- | ---- |
| `ROW_NUMBER()`・`RANK()`・`DENSE_RANK()`（引数なし） | 受理 |
| `COUNT(*)`・`COUNT(<列>)`・`SUM`/`AVG`/`MIN`/`MAX(<列>)`（引数は裸の列名・`id`・`*`〔`COUNT` のみ〕に限定） | 受理 |
| `OVER ( [PARTITION BY <列>[, ...]] [ORDER BY <列> [ASC\|DESC][, ...]] )`（空 `OVER ()` も可） | 受理（列数は各 8 個まで） |
| ウィンドウ項目と通常列の混在（位置を保持） | 受理 |
| 文全体のスカラー `ORDER BY`・ベクトル `ORDER BY`（検索 SELECT）・`USING PLAN`・`GROUP BY`／集計 SELECT・`SELECT DISTINCT` との併用 | `42601`（構造的に相互排他） |
| フレーム句（`ROWS`/`RANGE`/`GROUPS`）・`NULLS FIRST/LAST`・名前付きウィンドウ（`WINDOW`/`OVER w`）・`FILTER (...)`・関数内 `DISTINCT`・複合式の引数・式の中へのウィンドウ呼び出し（`ROW_NUMBER() OVER () + 1` 等）・`*` とウィンドウ項目の混在 | `42601` |
| `WHERE` でのウィンドウ別名参照（実在する同名列がある場合はその列として解釈） | `42601` |
| `CREATE VIEW` 本文への `OVER` 混入 | `42601`（`parse_view_body` が構造上受理しない） |

### 公開 API への影響（非破壊）

`Statement`・`ProjectedColumn`・`SelectItem`・`Projection` は網羅的 `pub enum` の
ため variant を追加すると破壊的変更になる。本実装は variant を追加せず、
`ValidatedScan::window_items`（`Vec<WindowSelectItem>`）・`BoundScan::windows`
（`Vec<BoundWindowItem>`）という side-table を追加するだけに留めた（いずれも
`pub(crate)`。既存の公開 API・`BoundScan::new`〔NoSQL 直接構築〕は不変で
`windows` は常に空）。`Projection` にはウィンドウ以外の項目だけを残し、各
ウィンドウ項目は SELECT リスト全体での出現位置（`position`）を保持することで、
実行時に元の並び順へ合流する。

### 実行方式（2 段階）

`sql::scan::execute_scan_with_budget` の先頭で `bound.windows()` が非空なら
`sql::window::execute_window_scan` へ dispatch する（`sql::scan` 本体への変更を
この 1 行に留め、他 PR との衝突を最小化する）。

1. **materialize 段**: 対象テーブルを 1 回、`LIMIT` による早期終了なしで走査し、
   可視かつ `WHERE` を満たす行**全体**についてウィンドウ項目ごとの
   `PARTITION BY`／`ORDER BY` キー・集計引数の値を owned 化して集める。RLS
   適用順序（ヘッダのみで可視性判定 → TABLE-12 のキー/ヘッダ tenant 整合検査 →
   必要範囲のみのデコード → SCALAR 段〔`WHERE`〕→ 可視性の再適用）は
   `sql::scan`／`sql::aggregate` の走査ループと同一の規約を踏襲する。
2. **投影段**: ウィンドウ項目ごとに独立してパーティション分割（正準バイト列
   キーによる `HashMap`）・安定ソート（ORDER BY キー → `id` 昇順 → 走査順
   `seq` 昇順のタイブレーク。`sort_by` のみを使い `sort_unstable*` は使わない
   ——`scripts/check_sort_determinism.sh` が CI で検知する）・peer グループ
   評価（`RANK`/`DENSE_RANK`/累積集計）を行う。集計本体は
   `sql::aggregate::Accumulator`（`Clone` を追加）を再利用し、NULL 契約・
   桁あふれ検査・NUMERIC の scale 保持を集計 SELECT と共有する——owned 化した
   値をその場で最小限の借用形（`scanned: &[Option<ScalarRef>]`・
   `RowVector`）へ組み立て直して 1 回ずつ `observe` する。ウィンドウ以外の
   投影・`LIMIT`／`OFFSET` の適用は、同じ `WHERE`・`limit`・`offset` を持つ
   `windows` 空の `BoundScan` 複製を `sql::scan::execute_scan_with_budget` へ
   そのまま渡すことで、既存の実行器（早期終了・結果バイト予算・RLS 適用順序
   いずれも同一）を再利用する（同じ `read_txn` で呼ぶため物理走査順は
   materialize 段と一致する）。ウィンドウ値は
   `LIMIT`／`OFFSET` 適用**前**の全体から計算し、出力対象行の選定にのみ
   `LIMIT`／`OFFSET` を使う。

### 上限（受入基準 2）

| 定数 | 値 | 意図 |
| ---- | -- | ---- |
| `MAX_WINDOW_PARTITIONS` | 10,000（`sql::group_by::MAX_GROUPS` と同値） | パーティション数の上限（`54000`） |
| `MAX_WINDOW_ROWS` | 1,000,000 | materialize 行数の合計上限（`54000`） |
| `MAX_WINDOW_FRAME_ROWS` | 1,000,000 | 1 パーティションの行数上限（`54000`） |
| `MAX_WINDOW_STATE_BYTES` | 64 MiB | キー値・引数値の累計バイト数上限（`54000`） |
| `MAX_WINDOW_KEYS` | 8 | `PARTITION BY`／`ORDER BY` の列数上限（構文段、`54000`） |

いずれも本リポの実装既定値であり、確定化（spec 側の層 B 検証）は別途 spec リポ
側の管轄。

### 同順位（peer）の決定性（受入基準 3）

- パーティションキーは NULL 同士を同値とする（`GROUP BY` と同じ規則）。
- ORDER BY キーが等しい行が 1 つの peer グループを構成する（NULL の位置は
  PostgreSQL 既定: ASC は末尾・DESC は先頭）。`id`／`seq` のタイブレークは
  ソート順にのみ影響し、peer 判定には含めない。
- `ROW_NUMBER` はソート後の 1 始まり連番、`RANK` は「peer グループ先頭の行位置
  ＋1」、`DENSE_RANK` は peer グループの序数。集計（ORDER BY あり）は先頭から
  現在の peer グループ最後の行までの累積値をグループ内の全行へ同じ値で付ける。
  ORDER BY なしはパーティション全体が 1 つの peer グループになる。
- FLOAT の比較は `total_cmp`（`group_by::cmp_cell_values` と同じ規則）。

## 実装上の簡略化（既知の制約・対象外）

- `SUM`/`AVG`/`MIN`/`MAX` の集計引数は裸の列名・`*`（`COUNT` のみ）に限定し、
  複合式（UDF・演算）は受理しない（構文段で `42601`）。この制約により、束縛結果
  （`AggregateInput`）に `ScalarExpr` が現れないことが保証され、`Accumulator`
  の観測をスカラー式評価（`ExprProgram`）を経由せず直接行える。
- ウィンドウ集計の集計本体は `sql::aggregate::Accumulator` を再利用しており、
  型ごとの桁あふれ検査・NULL 契約は集計 SELECT と同一。
- 性能: materialize 段は `LIMIT` を適用できないため、`WHERE` に一致する全行を
  スキャンする（2 パス構成）。既存の広域取得（1 パス）と比べ最大 2 倍のスキャン
  コストが掛かるが、早期終了なしの単純な線形走査に留まる（受入ゲートは設けない。
  10 万行規模の参考計測は今後のベンチ整備時に追加検討）。

## 対象外（本 Issue の外。利用者への影響が出た時点で別途判断し、必要なら Issue を起票する）

- スカラー `ORDER BY`（TASK-209・#915・PR #1096）とウィンドウの併用
- `GROUP BY`／集計 SELECT とウィンドウの併用
- フレーム指定・`NULLS FIRST/LAST`・名前付きウィンドウ・`FILTER (...)`・
  関数内 `DISTINCT`・複合式の引数
- 式の中へのウィンドウ呼び出しの埋め込み
- 検索 SELECT（ベクトル `ORDER BY`・`HYBRID`・`USING PLAN`）へのウィンドウ追加
- NoSQL（HTTP JSON）表層（`BoundScan::new` は常に `windows` を空にするため
  挙動は変わらない）
