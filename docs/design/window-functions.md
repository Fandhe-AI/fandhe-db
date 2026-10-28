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
| 文全体のスカラー `ORDER BY [ASC\|DESC][, ...]`（`LIMIT`／`OFFSET` 付き。Issue #1189） | 受理（ウィンドウ値は全母集合で評価し、並べ替え・`LIMIT`／`OFFSET` は出力行にのみ効く） |
| 文全体のベクトル `ORDER BY`（検索 SELECT）・`USING PLAN`・`GROUP BY`／集計 SELECT・`SELECT DISTINCT` との併用 | `42601`（構造的に相互排他） |
| 文全体の `ORDER BY` でのウィンドウ別名参照（実在する同名列がある場合はその列として解釈） | `42601` |
| フレーム句（`ROWS`/`RANGE`/`GROUPS`。Issue #1887・#1889・#1890） | 構文は受理する（形の誤りは `42601`）。`ROWS` は #1889、`RANGE` は #1890 で評価し、`GROUPS` は評価前のゲートで `0A000`（詳細は末尾「Issue #1887 追記」「Issue #1889 追記」「Issue #1890 追記」） |
| 名前付きウィンドウ（`WINDOW`/`OVER w`）・`FILTER (...)`・関数内 `DISTINCT`・複合式の引数・式の中へのウィンドウ呼び出し（`ROW_NUMBER() OVER () + 1` 等）・`*` とウィンドウ項目の混在 | `42601` |
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
   文全体のスカラー `ORDER BY` がある場合（Issue #1189）は、行ごとに
   並べ替えキー値と tenant 識別子（intern 表への添字）も保持し、いずれも state
   予算へ確保前に計上する。キー列はデコード段のマスクへ含める。
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

- ウィンドウ関数の結果による文全体の `ORDER BY`（`ORDER BY rn`）。base scan への委譲方式では評価できない
- `EXPLAIN` とウィンドウ関数の併用（`ORDER BY` の有無を問わず `42601`）
- `GROUP BY`／集計 SELECT とウィンドウの併用
- `GROUPS` フレームの評価（構文は Issue #1887 で受理済み。`ROWS` は #1889、`RANGE` は #1890 で評価）・名前付きウィンドウ・`FILTER (...)`・
  関数内 `DISTINCT`・複合式の引数
- 式の中へのウィンドウ呼び出しの埋め込み
- 検索 SELECT（ベクトル `ORDER BY`・`HYBRID`・`USING PLAN`）へのウィンドウ追加
- NoSQL（HTTP JSON）表層（`BoundScan::new` は常に `windows` を空にするため
  挙動は変わらない）

## 文全体のスカラー ORDER BY との行対応（Issue #1189）

- ウィンドウ値の評価後に、materialize 済み行の出力順（`materialized` への添字の
  順列）を、`sql::order_value::compare_statement_order`（キー → `id` 昇順 →
  `tenant_id` バイト順）による安定ソートで求める。この比較器は base scan
  （`sql::scan` の経路 (B)）の `HeapEntry::order` と同一の関数で、行順が構造上
  ずれない。
- base scan は `windows` を空にした複製に `order_by`・`limit`・`offset` を保ったまま
  委譲する。n 番目の出力行は `materialized[output_order[offset + n]]` に対応し、
  `id` の一致検査は防御として残す。他テナントの Public 行と `id` が衝突しても、
  対応付けは `id` ではなく順列の添字で行う。
- 経路 (A)（先頭キーが `id` で他テナントの Public 行を見ない ctx）は自テナント 1 つの
  範囲走査で、`id` がテナント内で一意なためタイブレークが起きず、上の順序と一致する。
- 順列・キー値・intern 表は `MAX_WINDOW_STATE_BYTES` の予算に計上する。

## Issue #1880 追記: LAG／LEAD の構文受理と値引数の束縛（SQL-64 (1) ポインタ）

親 #1879 の第 1 段。解析・束縛・型検査までを実装し、評価は Issue #1881 が接続する。

- **受理形**: `lag|lead(<値式> [, <オフセット式> [, <既定値式>]]) OVER (...)`。個数の誤り
  （0 個・4 個以上）は `42883`、`DISTINCT`・`*` は `42601`。引数の括弧区間は式パーサの前に
  先読み検査し、入れ子のウィンドウ関数は `42P20`、集計呼び出しは `42803`、`$n` は `42601`
  （Prepared の Describe でダミー値置換と型検査が食い違うのを避ける fail-closed。対応は別途）。
- **種別の分離**: `WindowFunc` に `Lag`／`Lead` を追加し、引数は集計引数（`AggregateArg`）を
  流用せず専用の `WindowOffsetArgs`（構文段）・`BoundWindowOffsetArgs`（束縛段）に持たせる。
- **束縛の型規則**: オフセットは行ごとに評価する式で、整数型か裸の `NULL` のみ（それ以外は
  `42804`）。値と既定値の型統一は `COALESCE` と同じ規則を再利用し
  （`udf_call::bind_value_with_default`）、不一致は `42804`。`VECTOR`・配列・`BYTEA`・
  `json`・`jsonb` の値は `0A000`。結果型は統一後の値の型で、`window_result_type` が公告する。
- **ゲート**: 型検査を終えた `bind_window_item` の末尾の 1 か所で `0A000` を返す。走査・
  Describe・ビューの評価後射影はすべて `bind_window_item` を通るため、行数・テナントの
  存在に依存せず未実装の評価へ到達しない。Issue #1881 でこのゲートを外し、評価を接続した。
- **ビューの列スコープ**: `check_window_columns_within_view` が値・オフセット・既定値の式の
  識別子をすべて検査する。欠くとゲートの `0A000` と `42703` の差から非公開列の存在が漏れる。
- **名前表の副作用**: `lag`／`lead` が窓関数名になるため、`FILTER` 内の `lag(x) OVER ()` は
  `42601` から `42P20` になる（PostgreSQL と同じ分類）。

## Issue #1881 追記: LAG／LEAD の評価（SQL-64 (1) ポインタ）

親 #1879 の第 2 段。`bind_window_item` のゲート（`0A000`）を外し、評価本体を `sql::window` に
実装した。意味論は PostgreSQL 17 の `lag`／`lead` に合わせる。

| 引数 | 評価する行 | 規則 |
| ---- | ---------- | ---- |
| オフセット | 現在行 | 全行で評価する（省略時 1）。NULL なら既定値の有無によらず結果は NULL。負は逆方向、0 は現在行 |
| 値 | 参照先の行 | 参照先がパーティション内ならその行の値。値が NULL なら結果も NULL（既定値へ落とさない） |
| 既定値 | 現在行 | 参照先がパーティション外のときだけ使う（省略時 NULL） |

- **入力の保持**: materialize 段が行ごとに「参照先になったときの値」「現在行のオフセット」
  「現在行の既定値」（`OffsetRowValues`）を集める。抽出は `collect_window_row_values`（走査と
  `CellWindowEvaluator` の共有実装）に統合し、第 2 の抽出器を作らない。式は項目ごとに 1 回だけ
  コンパイルする。保持するセルは `MAX_WINDOW_STATE_BYTES` の状態予算に確保前に計上する。
- **遅延エラー**: 値・既定値式のデータ例外（`22xxx`）は保持し、使用時に初めて返す
  （参照されない行のエラーを出さない。PostgreSQL と同じ）。予算超過・中断・内部エラーは即時。
- **オフセットの範囲**: bigint・smallint も受理するため、`int4` の範囲外は `22003`。
  参照先の位置は `i64` で計算し、`i32::MIN` でもあふれない。
- **評価位置**: `evaluate_partition` が安定ソートの直後に専用の `evaluate_offset_partition`
  へ委譲する（peer・フレームに依存しない）。同点は既存どおり `id`→`seq`。
- **母集合・RLS**: 他のウィンドウ関数と同じ materialize 段（可視性・TABLE-12 整合・`WHERE`・
  可視性の再適用）を通った行だけで構成される（RLS-10 ポインタ）。不可視行は参照先にならない。
- **ビュー経路の修正**: 評価後射影形ビュー（集計／`LIMIT` 本文）の外側が参照する識別子の
  収集が `LAG`／`LEAD` の引数を見ておらず、引数にしか現れない列が NULL になっていた。
  `WindowSelectItem::collect_referenced_idents` へ一本化し、`plan_rows`・
  `outer_non_projection_idents` から使う。
- **共通化**: 裸の列のセル化は `scan::push_scalar_column_cell`（`build_projected_cells` から
  抽出。挙動不変）を共有する。既定値の裸の列は宣言型を併せて運ぶ（`default_column`）。
- **対象外**: フレーム句・名前付きウィンドウ・`FILTER`・`EXPLAIN`（兄弟 Issue）。引数内の
  `$n`（`42601`）。NUMERIC の typmod が異なる値と既定値の公告 typmod の差（#1880 の判断を維持）。

## Issue #1887 追記（フレーム句の構文受理・SQL-64 (2) ポインタ）

`OVER (...)` 内のフレーム句の構文を解析して AST（`WindowSelectItem.frame`）に載せる。評価は
兄弟 Issue（#1889 ROWS・#1890 RANGE・#1891 GROUPS）の担当で、本 Issue はゲートまでを担う。

- **受理する文法**（PostgreSQL 17 の frame_clause）: `<mode> (<bound> | BETWEEN <bound> AND <bound>)
  [EXCLUDE (CURRENT ROW | GROUP | TIES | NO OTHERS)]`。`<mode>` は `ROWS`／`RANGE`／`GROUPS`、
  `<bound>` は `UNBOUNDED PRECEDING`・`<offset> PRECEDING`・`CURRENT ROW`・`<offset> FOLLOWING`・
  `UNBOUNDED FOLLOWING`。位置は `PARTITION BY`／`ORDER BY` の後、`)` の直前。単一境界形の終了は
  `CURRENT ROW` に正規化する。
- **分類と配置段**:

  | SQLSTATE | 内容 | 段 |
  | -------- | ---- | -- |
  | `42601` | 形の誤り（境界の欠落・`UNBOUNDED`／`CURRENT` 単独・未知の語・`EXCLUDE` の不完全・余剰トークン・オフセット内の `$n`） | 構文（`parse_window_item`） |
  | `42P20` | 境界の組み合わせ（`frame start cannot be UNBOUNDED FOLLOWING` 等 6 種）・オフセット内の入れ子ウィンドウ関数 | 構文 |
  | `42803` | オフセット内の集計関数 | 構文（先読み） |
  | `42P20` | `RANGE` オフセットで `ORDER BY` が 1 キーでない・`ORDER BY` の無い `GROUPS` | 束縛（`bind_window_frame`） |
  | `42P10` | オフセット式に識別子がある | 束縛 |
  | `42804` | `ROWS`／`GROUPS` のオフセットが整数型でない | 束縛 |
  | `0A000` | 上記をすべて通った項目（評価前ゲート） | 束縛（`bind_window_item` 末尾） |

- **ゲート**: フレーム句付きの項目は行数・テナントの有無に依存せず `0A000` で止まる。既定
  フレームと同値の明示フレームも対象で、実行しない。解除は #1889〜#1891 がモード単位で行う。
- **オフセットの変数検査**: 識別子が 1 つでもあれば列を解決せず一律 `42P10` とする。実在列・
  未知列・ビューの非公開列で応答が変わらず、存在情報が漏れない（A01）。
  既知の差異: PostgreSQL は未知列を `42703` とする。
- **対象外**（評価 Issue へ引き継ぐ）: 負のオフセット `22013`・NULL のオフセット `22004`
  （PostgreSQL は実行開始時の検査）、`RANGE` オフセット型と `ORDER BY` キー型の照合、順位関数・
  `LAG`／`LEAD` にフレームが付いた場合の扱い（PostgreSQL はフレームを無視して評価する）。
  `ROWS`／`GROUPS` のオフセットが文字列リテラル `'1'` の場合、PostgreSQL は受理するが
  本実装は `42804`（`LAG` と同じ規則）。オフセット内の `$n` は `42601`。
- **公開 API**: `SqlSurfaceError` に `WindowFrameOffsetHasVariables { mode }`（`42P10`）を追加した
  （網羅 enum のため破壊的変更）。

## Issue #1888 追記: パーティション・比較器の整理（SQL-64 ポインタ）

親 #1886（フレーム句）の前提整備。挙動は変えないリファクタで、既存テストの期待値は不変。

- **比較器を 1 本に**: `cmp_order_keys`（ORDER BY キーのみ）と `cmp_partition_rows`
  （キー → `id` → `seq`）に分けた。peer は `cmp_order_keys == Equal` と定義し、旧
  `peer_group_equal`／`window_key_value_eq` を廃止した（NULL 同士のみ peer という意味は同じ）。
- **`SortedPartition`**: 並べ替え済みパーティション。`row(pos)`／`row_index(pos)`／`len()` で
  位置から行を引き、`peer_groups()`／`peer_group_end(start)`／`are_peers(p, q)` で peer 範囲を
  得る。範囲外は panic せず `XX000` で fail-closed。
- **状態予算は不変**: peer の境界は確保せず都度走査で求める。`indices` は
  `evaluate_window_item` が計上済みの Vec をそのまま持つため、`54000` の境目は動かない。
  境界索引が必要になった場合（#1890／#1891）は、確保の前に `try_accumulate_state_budget`
  へ計上すること。
- **#1889〜#1891 の前提**: ROWS は位置アクセス、RANGE／GROUPS は peer 範囲と
  `cmp_order_keys` を土台にする。フレーム固有の処理は本 Issue では入れていない。

## Issue #1889 追記: ROWS フレームの評価（SQL-64 (2) ポインタ）

`bind_window_item` のゲートを「`ROWS` 以外のフレーム」だけに縮め、`ROWS` を PostgreSQL 17 と同じ
結果で評価する。`RANGE`／`GROUPS`（`RANGE UNBOUNDED PRECEDING` のようにオフセットのない形も含む）は
Issue #1890／#1891 まで `0A000` のまま。

- **束縛**: `bind_window_frame` は `ROWS` のとき束縛済みフレーム（`BoundWindowFrame`。オフセットは
  `bind_projection_expr` の束縛済み式、裸の `NULL` は `Null` 境界）を返し、`BoundWindowItem.frame` に載せる。
  検証の順序（`42P20`→`42P10`→`42804`）は #1887 から変えない。Describe は束縛までで、オフセットは評価しない。
- **解決の位置**: `resolve_frames` が実行開始時（行を引く前。ストレージ走査は `materialize_rows` の前、
  評価後射影形ビューは `CellWindowEvaluator::new`）にオフセット式を評価する。PostgreSQL がプラン時・
  WindowAgg 開始時に評価するのと同じ位置で、0 行・`WHERE` 不一致でも検査が走る。
- **関数別の扱い**（PostgreSQL 17.11 で実測）

  | 関数 | フレーム・`EXCLUDE` | 評価エラー（`22012`・`22003` 等） | NULL `22004`／負 `22013` |
  | ---- | ------------------- | ---------------------------------- | ------------------------ |
  | `row_number`／`rank`／`dense_rank` | 無視 | 返る | 検査しない（PG16 以降は最適化でフレームが置換される） |
  | `lag`／`lead` | 無視（`EXCLUDE` も） | 返る | 検査する（空テーブルでも） |
  | `count`／`sum`／`avg`／`min`／`max` | 適用 | 返る | 検査する（開始→終了の順） |

- **オフセットの型**: int8 全域（`i64` へ飽和変換。`LAG` の int4 契約とは別の `frame_offset_from_expr_value`）。
  文言は PostgreSQL と同じ固定語（`frame starting offset must not be null` 等）で、値は含めない。
  `SqlSurfaceError::InvalidPrecedingOrFollowingSize`（`22013`）を追加した（網羅 enum のため破壊的変更）。
- **範囲計算**: `rows_frame_bounds(pos, len, frame)` は位置の半開区間を返すモード非依存の純関数で、
  すべて飽和演算。first_value 等（#1883）と #1890／#1891 が再利用する。
- **集計の評価**: `evaluate_rows_frame_partition` が行ごとにフレームから `EXCLUDE`（`CURRENT ROW`／
  `GROUP`／`TIES`。peer 範囲で判定）を除いた行を**前方順**に観測する。PostgreSQL は逆遷移のない集計
  （float・`MIN`／`MAX` 等）で同じく再集計するため、浮動小数点の加算順まで一致する。整数・`NUMERIC` は
  逆遷移でも正確なので同値。始点が `UNBOUNDED PRECEDING` で `EXCLUDE` なしの形は終点が単調非減少なので、
  累積器へ新規行だけを足す O(n) の経路を取る。それ以外は O(n×幅)（最大 1M 行）。観測ループで
  `interrupt::check()` を呼び、切断で打ち切れる。独自の作業量上限は PostgreSQL にない拒否になるため置かない。
- **状態予算**: 新しい確保は行ごとの結果セルの複製だけで、確保前に `MAX_WINDOW_STATE_BYTES` へ計上する。
  peer 境界の索引は確保しない（#1888 の方針）。
- **RLS**: フレームは materialize 段（可視性判定→整合確認→`WHERE`→再判定）を通った行の並びの上でしか
  位置を数えない。不可視行はフレームに入らない（結合テストで固定）。
- **既知の差異**: 整数リテラルは式層の規則で `2^53` 超が `22003`（例: `9223372036854775807 PRECEDING`。
  `2147483648` と `9007199254740992` は受理）。`'1'` リテラルは `42804`、オフセット内の `$n` は `42601`、
  未知列は `42P10`（#1887 から継続）。評価後射影形ビュー経路では本文のエラーがオフセットのエラーより
  先に出る可能性がある。
- **対象外**: `GROUPS`（#1891）・first_value／last_value／nth_value（#1882／#1883）、
  逆遷移・セグメント木による移動集計の高速化。

## Issue #1890 追記: RANGE フレームの評価（SQL-64 (2) ポインタ）

`bind_window_item` のゲートを `GROUPS` だけに縮め、`RANGE`（値オフセット付き・なしの両方）を
PostgreSQL 17（17.11 で実測）と同じ結果・同じ SQLSTATE で評価する。`GROUPS` は #1891 まで `0A000`。
ORDER BY なし・複数キーの `RANGE UNBOUNDED PRECEDING` 等も評価される（全行が 1 つの peer グループ）。

- **オフセット型の照合**（束縛段 `bind_range_offset`。`ORDER BY` の唯一のキー型 × オフセット型。
  PostgreSQL は不一致を一律 `0A000` にする。`42804` にはならない）

  | キー型 | 受理するオフセット | 型なし文字列リテラル `'..'` の変換先 |
  | ------ | ------------------ | ------------------------------------ |
  | `SMALLINT`／`INTEGER`／`BIGINT`／`id` | 整数（小数・float・interval は `0A000`） | 整数（`'abc'` は `22P02`） |
  | `REAL`／`DOUBLE PRECISION` | 整数・小数・float・numeric（`f64`。interval は `0A000`） | float8 |
  | `NUMERIC` | 整数・小数リテラル・numeric（`Decimal` で正確に加減算。float は `0A000`） | numeric |
  | `DATE`／`TIMESTAMP`／`TIMESTAMPTZ`／`TIME` | `INTERVAL`（整数等は `0A000`） | interval（`'abc'` は `22007`） |
  | `TEXT`／`BOOLEAN`／`UUID` | オフセット付きは常に `0A000`（`NULL` オフセットも） | — |

  小数リテラルは式層では double だが、PostgreSQL は numeric のため、未束縛の `Expr::Number` の形で補正する
  （`1.5` は numeric キーに正確な値で渡り、整数キーには `0A000`）。`NULL` オフセットは照合を通り実行開始時に `22004`。
- **peer と `CURRENT ROW`**: `CURRENT ROW` 境界・`EXCLUDE GROUP`／`TIES` は peer グループ単位
  （`SortedPartition::peer_groups`）。NULL キーは NULL 同士だけが peer。
- **境界の計算**（`range_frame_bounds`）: `UNBOUNDED`／`CURRENT ROW` は位置だけで決まる。値オフセットは、
  現在行のキーが NULL なら NULL の peer グループ、非 NULL なら非 NULL 区間（NULL は先頭か末尾に連続するため
  二分探索で求める）の上で、PostgreSQL の `in_range` 判定を述語にした二分探索で求める。`DESC` は加減と
  大小の向きをともに反転する。NULL 行はオフセット境界を満たさず、`UNBOUNDED` 境界には含まれる
  （`UNBOUNDED PRECEDING AND 1 FOLLOWING` の NULL 行は先頭から自身の peer 末尾まで）。追加の確保は
  しないため状態予算は変わらない。比較対象（`基準値 ± オフセット`）は現在行・境界ごとに 1 回だけ計算する。
- **型別の `in_range`**: 整数は `i128` で正確に比較（PG の桁あふれ分岐と同じ結果）。float は NaN を最大値
  （NaN 同士は等しい）とし、基準値・オフセットがともに無限大で加減算が NaN になる組は常に範囲内とする。
  numeric は `Decimal` の正確な加減算（あふれは `22003`）。`DATE` は日時へ昇格して比較する。
  `TIMESTAMP` は月→日→マイクロ秒の順に加算し、最終値の 0001〜9999 年検査はしない（PG の範囲外だけ
  `22008`。`9999-12-31` の `1 day FOLLOWING` はエラーにならない）。`interval_arith` に範囲検査なしの
  `timestamp_shift_wide`／`timestamptz_shift_wide` を追加した（既存の加減算の挙動は不変）。
  `TIMESTAMPTZ` は月・日をセッション TZ の壁時計で加算する（DST をまたぐ `1 day` は 24 時間と異なる）。
  `TIME` は `INTERVAL` の時間成分だけを日またぎの折り返しなしで加減算する（月・日は無視）。
- **オフセットの検査時点**（PG 17.11 で実測）: `NULL`（`22004`）は実行開始時に、集計と `LAG`／`LEAD` が検査する
  （0 行でも出る）。負・NaN（`22013`）は PG の in_range 関数内の検査に合わせ、**非 NULL キーの現在行で境界を
  求める時点**まで遅延する。0 行・全行 NULL キーのパーティションのみでは出ない。`LAG`／`LEAD` もフレームは
  無視するが、非 NULL キーの行があるパーティションでは同じ `22013` になる。順位関数は検査しない。
  負の判定は整数・numeric・float（NaN 含む）はその値、interval は正規化値（月 = 30 日）、`TIME` キーは時間成分のみ。
- **集計の評価**: `evaluate_range_frame_partition`。`EXCLUDE`・累積経路（始点が `UNBOUNDED PRECEDING` で
  `EXCLUDE` なし。終点が単調でなければ再集計へ倒す）・前方加算順・`interrupt::check()`・結果セルの状態予算計上は
  `ROWS` と同じ。明示の `UNBOUNDED PRECEDING AND CURRENT ROW` と既定フレームは全関数で同値（テストで固定）。
- **RLS**: 母集合は materialize 段を通った行のみ。他テナントの Private 行は境界計算にも現れない（結合テストで固定）。
- **既知の差異**: (1) 識別子を含むオフセットは型照合より先に一律 `42P10`（PG は照合が先。存在情報の秘匿）。
  (2) 式層の整数リテラル `2^53` 超は `22003`（#1889 から継続）。(3) 整数・小数リテラル以外の式
  （算術等）で作った小数は double 扱いのため、numeric キーには `0A000`（fail-closed）。
  (4) `'inf'`／`'NaN'` の float8 への変換は未対応（`0A000`。列にも NaN・無限大は格納できない）ため、
  無限大・NaN の規則は単体テストで固定する。(5) 整数キーに対する型なし文字列リテラルは `SMALLINT` でも
  int4 へ変換する。
- **対象外**: `GROUPS`（#1891）・first_value／last_value／nth_value（#1882／#1883）・interval 列を
  `ORDER BY` キーにする `RANGE`・timetz・移動集計の高速化。
