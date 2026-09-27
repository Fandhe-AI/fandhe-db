# NoSQL `explain` フィールドの写像（NOSQL-10）

- Issue: #765
- 対象タスク: TASK-175・TASK-186
- 対象ビヘイビア: NOSQL-10（関連: SQL-6・SQL-5・PLAN-11・NOSQL-2）
- ステータス: Implemented

## 背景

`POST /v1/query`（`op: search`）の `explain: true` は、SQL 表層
`EXPLAIN SELECT ... USING PLAN(...)`（SQL-6）と同一内容を検索本体を実行せず
返す契約。Issue #763 の `bind_search` は `explain` フィールドの値を保持する
のみで判定には使っていなかった。`scan`（Issue #766）は `explain: true` を
`42601` で拒否、`aggregate`（Issue #768）は本 Issue に委ねる暫定 `0A000` を
返していた。

## 採用した設計

### engine 側: SQL `EXPLAIN` アームと NoSQL エントリの共有ヘルパー

- `sql::explain::ExplainShape`（`pub`・`#[non_exhaustive]`）: `WHERE` 述語の
  構造のみから決まる形状情報（`filters_empty`・`scalar_plan`）を運ぶ型。
  `sql::using_plan::pre_check_bindable`（`pub(crate)`）の戻り値をこの型へ
  差し替えた（旧 `PreCheckShape` を撤去）。構築は `ExplainShape::from_filters
  (metadata_filters, expr_filters)` の 1 本のみ（`scalar_prefilter: true`
  固定。`USING PLAN` は `HINT ORDER` を受理しないため SCALAR 段は常に
  DISTANCE 段より先に評価される契約に基づく）。
- `core.rs::EngineCore::run_explain_plan`（private）: 既存 `Statement::
  Explain` アームの本体（テーブル世代の事前記録 → 束縛検証（`bind`。`plan`
  欠落判定を含む。LLM I/O より前）→ `question`（`USING PLAN` 本文）の
  空文字・長さ上限検証（`bind` より後。codex-review P1 指摘対応・PR #828）→
  辞書必須列検証（`question` 検証より後。同 PR）→ `mode_literal` 解析
  （辞書必須列検証より後）→ LLM クエリ展開・モード解決 → 世代の事後照合 →
  辞書必須列の再検証 → 使用エンジン・ANN／SCALAR 静的判定 → `QUERY PLAN`
  整形）を抽出した私的ヘルパー。binder closure は `&TableSchema`／
  `&UdfRegistry` を受け取り `Result<ExplainShape, E>`
  （`E: From<SqlSurfaceError>`）を返す。
  `Statement::Explain` アームはこのヘルパーを呼ぶだけの薄いラッパーへ縮約
  した（挙動不変。手順順序・エラー分類はビット同一）。`mode_literal`
  （`Option<&str>`。`USING MODE` 相当の生リテラル）はテーブル解決（最初の
  `read_txn_with_schema`）・`bind` 呼び出しより後で初めて解析する
  （Cursor Bugbot 指摘対応・PR #828 レビュー継続。未知テーブル〔`42P01`〕・
  `bind` が返す `42601` 系エラー（NoSQL 経路の `plan` 欠落を含む）が
  いずれも `mode` 値不正〔`22000`〕より優先される順序を、`Statement::
  Select` アームの `USING PLAN` 経路〔`run_using_plan_select`。PR #827〕が
  `pre_check`（束縛検証）を `mode_literal` 解析より先に呼ぶのと同一に保つ。
  当初の実装〔Issue #765〕では呼び出し元がテーブル解決前に
  `SearchMode::parse_literal` を済ませてから `run_explain_plan` へ渡して
  いたため、未知テーブル＋ `mode` 値不正の要求で `22000` が `42P01` より
  先に確定する回帰があった。その修正〔PR #828〕でも `mode_literal` 解析を
  `bind` 呼び出しより先に置いたままだったため、既存テーブル＋ NoSQL の
  `plan` 欠落＋ `mode` 値不正が同時に揃う要求で `22000` が `42601` より
  先に確定する回帰が残っていた（Cursor Bugbot 指摘・Issue #765 継続対応。
  `bind` を `mode_literal` 解析より先に呼ぶ現在の順序で解消し、
  `run_using_plan_select` と整合させた）。`question` の空文字・長さ上限
  検証（`sql::allowlist::validate_using_plan_question`）は NoSQL 表層側
  （`http/query/explain.rs::execute`）で既に行っているが、engine 公開 API
  （`explain_bound_plan_in_session`）を NoSQL 表層の事前検証を経由せず直接
  呼ぶ経路の多層防御として `run_explain_plan` 内でも独立に検証する
  （codex-review P1 指摘対応・PR #828 追加分。`execute_bound_plan_search_
  in_session`〔通常の `plan` 検索〕が engine 側で無条件に同じ検証を行うのと
  エラー契約を揃える）。`bind`（`plan` 欠落判定）成功後にのみ検証すること
  で、NoSQL 表層が `plan` 未指定時に渡すプレースホルダの空文字列
  （`question.unwrap_or("")`）を実在する `plan` と誤認せず、既存の
  「`plan` 欠落（`42601`）が優先される」順序を壊さない。SQL `EXPLAIN`
  経路は字句解析時点（`Parser::parse_using_plan_clause`）で既に同じ検証を
  通過済みの `question` を渡すため、この呼び出しは常に成功し挙動を変えない。
- `core.rs::EngineCore::explain_bound_plan_in_session`（`pub`）:
  `run_explain_plan` をそのまま公開する薄いラッパー。`execute_bound_scan_
  in_session`・`execute_bound_aggregate_in_session`（Issue #728）と同型の
  セッション対応エントリだが、エラー型 `E` が `From<SqlSurfaceError>` の
  ジェネリック境界のみを課す点が異なる——呼び出し元（wire-server）の binder
  が SQL 表層の束縛ヘルパーを経由して独自のエラー型（`search::SearchError`
  等）を返すため。検索本体（`hnsw_state` の `lookup`／`prepare_*`・
  `SearchProvider::search`）はどちらの呼び出し元でも呼ばれない（`EXPLAIN`
  は検索を実行しない契約）。

### wire-server 側: `http/query/explain.rs`

- `ExplainError`: `SearchError`（[`super::search::bind_search`] の全エラーを
  透過）・`ExplainRequiresPlan`（`vector` 指定または `plan` 未指定。`42601`。
  SQL-6 の「`EXPLAIN` は `USING PLAN` 付き検索 `SELECT` 専用」契約の写像）・
  `Engine(SqlSurfaceError)`・`Encode(ResponseEncodeError)` の 4 種。
- `execute`: `table`（識別子形状検査）→ `vector` 指定拒否（`42601`。`plan`
  の有無によらずテーブル解決を要さない構造的違反のため最優先。同時指定も
  この分岐で拒否）→ `plan` が指定されている場合のみ長さ検証
  （`validate_using_plan_question`。`54000`）→ `limit` の型・範囲検証
  （`validate_search_limit`。`22000`）→ `mode`（識別子形状検査のみ。語彙
  検証は行わない）を読み取ってから `EngineCore::explain_bound_plan_in_
  session` を（`plan` 未指定の場合もプレースホルダの空文字列を渡して）
  呼ぶ。binder closure はテーブル解決後に初めて `plan` 欠落を判定してから
  `bind_search`（Issue #763）の完全な束縛結果から `BoundSearch::Plan` の
  フィルタのみを取り出して `ExplainShape::from_filters` を組み立てる
  （`BoundSearch::Vector` への到達は構造上ないが、多層防御として
  `ExplainRequiresPlan` へ拒否する）。`table`／`plan`／`mode` の検証は
  `bind_search` 内でも再度行われる（二重検査。`EngineCore::
  explain_bound_plan_in_session` のドキュメント参照）。第 2 の実行器は
  作らない。
  - `mode` の語彙検証（`SearchMode::parse_literal`）をここで先に行うと、
    テーブル未存在＋ mode 値不正の要求で `22000` が `42P01` より先に確定
    してしまう。`explain_bound_plan_in_session`（`run_explain_plan`）が
    テーブル解決・`bind`（`plan` 欠落判定を含む）を終えた後で初めて解析
    することで、未知テーブル（`42P01`）・`plan` 欠落（`42601`）のいずれも
    `mode` 値不正（`22000`）より優先される fail-closed 順序を保つ
    （Cursor Bugbot 指摘対応・PR #828 レビュー継続。`vector`／`plan` の
    排他判定を `mode` 解析より先に行う `search::bind_search`〔`vector`
    指定検索〕と同一の優先順位）。
  - `limit` の検証をテーブル解決後（binder closure 内）まで遅延させると、
    未知テーブル＋ `limit` 範囲外の要求で `42P01` が `22000` より先に確定
    してしまう。SQL `EXPLAIN` 経路〔`core.rs` の `Statement::Explain` アーム〕
    が `run_explain_plan` 呼び出し前に `validate_search_limit` を呼ぶのと
    同一の優先順位を、通常の `plan` 検索（`search.rs::execute`）と同様に
    ここでも保つ（codex-review P1 指摘対応・PR #828）。
  - `plan` 欠落判定をテーブル解決前に行うと、未知テーブル＋ `plan` 欠落の
    要求で `42601` が `42P01` より先に確定してしまう。`super::search::
    execute` が `vector`／`plan` 両方欠落の判定を `bind_search` 自身の
    テーブル解決後へ委ねるのと同じ優先順位を保つため、判定を binder
    closure（テーブル解決後）まで遅延させる（codex-review P1 指摘対応・
    PR #828）。
- `response::encode_explain`: `QueryResult` が `Computed { name: "QUERY
  PLAN" }` 1 列・各行 `Cell::Text` 1 個であることを検証してから
  `{"explain":["<行>", ...]}`（キー固定・空白なし）へ写像する。逸脱時は
  best-effort に描画せず `Err`（`InternalError`／`XX000`）で fail-closed。
- `gate.rs`: 手順 5 の `match (op, engine)` に `(Op::Search, Some(engine))
  if explain_requested(&validated)` を、通常の `search` 実行アーム（#764 が
  追加する見込み）より必ず先に置く。`explain: true` は match の腕の順序
  自体により構造的に実行経路へ落ちない（`aggregate.rs::reject_explain` と
  同じ fail-open 防止の思想）。
- `aggregate.rs`: `AggregateError::NotYetSupported`（`0A000`・
  `EXPLAIN_NOT_YET_SUPPORTED_MESSAGE`）を `ExplainNotSupported`（`42601`・
  `EXPLAIN_NOT_SUPPORTED_MESSAGE`）へ変更した（`reject_not_yet_supported`
  → `reject_explain` に改名）。SQL-6 が集計 `SELECT` への `EXPLAIN` 前置を
  `42601` で拒否する契約の写像であり、`scan.rs::ScanError::
  ExplainNotSupported` と同型。

## テスト

- `crates/engine/tests/core_explain_plan_entry.rs`（新設）: SQL `EXPLAIN` と
  `explain_bound_plan_in_session` の行単位完全一致（フィルタなし・等価
  フィルタあり）・HNSW opt-in 時の `hnsw_params:` 行一致と索引キャッシュ
  非タッチ（`lookup`／`prepare_*` 不呼び出しの非 vacuous 証跡）・未定義
  テーブルの binder 呼び出し前拒否・未定義テーブル＋ `mode_literal` 値不正
  で `42P01` が `22000` より優先されること（PR #828 レビュー対応）・binder
  エラーの伝播と LLM 呼び出しスキップ・`bind` 成功後の空文字列・64 KiB 超
  `question` がそれぞれ `22000`／`54000` で LLM 呼び出し前に拒否されること
  （engine 公開 API 単独での多層防御。codex-review P1 指摘対応・PR #828
  追加分）・プランナー未注入時の `XX000`・辞書必須列欠如の `22000`・他
  テナント行内容の非漏えいを固定。
- `crates/wire-server/tests/nosql10_explain.rs`（新設。production ルータ
  経由の層 A）: SQL `EXPLAIN` との行単位一致（フィルタなし・フィルタ
  あり・`mode` あり）・HNSW opt-in 時の `hnsw_params:` 行・`vector` 指定
  拒否・`vector`＋`plan` 併存拒否・未知テーブル＋ `mode` 値不正で `42P01`
  が `22000` より優先されること（PR #828 レビュー対応）・未知テーブル＋
  `plan` 欠落で `42P01` が `42601` より優先されること・未知テーブル＋
  `limit` 範囲外で `22000` が `42P01` より優先されること（いずれも
  codex-review P1 指摘対応・PR #828）・既存テーブル＋ `plan` 欠落＋
  `mode` 値不正が同時に揃う要求で `42601` が `22000` より優先されること
  （Cursor Bugbot 指摘・Issue #765 継続対応）・プランナー未注入時の
  `XX000`・`aggregate`／`scan` の `explain: true` が引き続き `42601`・
  他テナント行内容の非漏えいを検証。
- `crates/wire-server/tests/nosql4_aggregate.rs`: `explain_true_rejects_
  with_0a000_and_does_not_execute` を `explain_true_rejects_with_42601_
  and_does_not_execute` へ改名・期待値を `42601` へ更新。

レイテンシ（`explain` 応答時間 vs 同一 core での通常 `USING PLAN` 検索
時間）は run-to-run 変動で flaky になるためアサーションには使わず、
Issue 本文の受け入れ条件からは対象外とした（`docs/design/
benchmark-judgement-policy.md` の計測規約と同じ判断）。

## spec 側への申し送り事項（Issue #948 で解消）

NOSQL-10 は `search`／`scan`／`aggregate` を対象に挙げるが、当時の SQL-6
の `EXPLAIN` は `USING PLAN` 付き検索 `SELECT` 専用（`scan`／`aggregate`／
`vector` 形は `42601`）だったため、本実装（Issue #765）は SQL-6 と同一の
拒否契約を写像していた。その後 SQL 表層が Issue #922・SQL-27 で `USING
PLAN` なし検索 `SELECT`・集計・広域取得への `EXPLAIN` を受理するよう拡大
したため、NoSQL 表層の `vector` 指定 `search`・`scan`・`aggregate` への
`explain: true` も Issue #948・NOSQL-16 (a) で同様に受理するよう拡大した
（`crates/wire-server/docs/nosql-api.md` の `explain` 節・
`crates/wire-server/tests/nosql16_explain_targets.rs` 参照）。

## 対象外（後続 Issue の担当）

- `search`（`explain` なし）の実行結線（#764）
- `insert` の `gate.rs` 結線・成功応答（別 Issue）
- 3 クライアント統合テスト（TASK-183）・確定化判定（TASK-185）
- NoSQL 表層の `group_by` 配列形（複数列）受理（NOSQL-16 (b)。Issue #949）
