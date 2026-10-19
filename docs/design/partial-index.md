# 部分索引の宣言側（述語 parser と決定性検査）

対象: Issue #1653（親 #1652）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11・INDEX-10、
`docs/spec/04-behavior/error-format.md` ERR-6（spec 本文は転記しない）。
兄弟: #1654（格納・DML 維持）、#1655（述語の含意判定）、#1656（プランナ選択・EXPLAIN）、
部分 UNIQUE は #1657（実装済み。「Issue #1657」節）。式索引の宣言側（[expression-index.md](expression-index.md)）と同じ形で、
述語は解析・分類までを行い、決定的と判定した述語も最終ゲートで `0A000` にした（Issue #1654 で格納・DML
維持を実装し、最終ゲートは未対応の組み合わせだけに縮小した。「Issue #1654」節を参照）。

## 範囲

`CREATE [UNIQUE] INDEX ... (...) WHERE <述語>` の述語を、**カタログ非参照の構文段だけ**で解析・分類する。
格納・維持が無いまま宣言が残ると効かない索引になり、`UNIQUE` なら強制漏れ（fail-open）になるため、
決定的な述語は `partial indexes are not supported yet`（`0A000`）で拒否する。同じ拒否を
`sql::ddl::execute_create_index` にも防御ゲートとして置く（述語を黙って落とした列だけの索引を保存しない。
両方とも Issue #1654 で撤去する）。

## 述語の文法

既存の `Parser::parse_where`（`WherePredicate` の木。`AND`・`OR`・`NOT`・括弧・`IN`・`BETWEEN`・
`IS [NOT] NULL`・式比較・`LIKE`）を既定状態（部分クエリ文脈なし・CHECK 文脈なし）で使い、第 2 の
パーサを作らない。`OR`・`NOT` は宣言時には受理する（含意判定〔#1655〕は部分集合だけを扱えばよい）。
式・葉数・深さは既存の上限（`54000`）の内側でだけ解析する。

## 判定順（全て構文段・カタログ非参照）

1. 要素（キー式）の先読み・構文・構造検査・式要素の決定性（式索引と同じ。要素の `42P17` が述語より先）
2. 述語の先読み（`precheck_index_tokens(.., Predicate)`。集計・ウィンドウ・部分クエリ・SRF を分類）
3. 述語の解析（`parse_where`。空・構文誤りは `42601`、上限超過は `54000`）
4. 述語の決定性（`check_index_predicate_determinism`。`WherePredicate` の全 variant を網羅）
5. 文末（`WHERE flag LIMIT 1` 等は `42601`）
6. 最終ゲート（式索引の未対応の組み合わせ → 部分索引は `0A000`）

## SQLSTATE と固定文言

| 区分 | SQLSTATE | 文言（固定。列名・値・テナント名を載せない） |
| ---- | -------- | -------------------------------------------- |
| 非決定的な式（`now()`・`CURRENT_DATE`・`random()` 等） | `42P17` | `functions in index predicate must be marked IMMUTABLE` |
| 集計 | `42803` | `aggregate functions are not allowed in index predicates` |
| ウィンドウ | `42P20` | `window functions are not allowed in index predicates` |
| 部分クエリ | `0A000` | `cannot use subquery in index predicate` |
| SRF | `0A000` | `set-returning functions are not allowed in index predicates` |
| 組み込み以外の関数 | `0A000` | `user-defined functions in index predicates are not supported` |
| `visible()`・構文誤り・文末の余剰 | `42601` | 既存の構文エラー文言 |
| 決定的な述語（#1654 まで。#1654 で縮小） | `0A000` | `partial indexes are not supported yet`（#1654 で撤去。現在の `0A000` は `partial indexes are not supported for this index kind yet`・`COLLATE on a partial index is not supported yet`・`partial index on a materialized view is not supported`） |

決定性の基準は式索引（INDEX-10）と同じ表を共有する（`sql::index_expr::IndexExprSite` が文言だけ切り替える）。
型に依存する判定（時刻型の `CAST`・`TIMESTAMPTZ` 列とリテラルの比較等）は構文段では通し、束縛段（#1654）で
再判定する。

## セキュリティ上の判断

- `visible()`（RLS 述語呼び出し）はセッション主体に依存するため、永続する索引へ埋め込ませず `42601`。
- 構文段の判定はカタログを参照しない。権限の無い主体・存在しない表でも同じ SQLSTATE を返す。
- `WherePredicate` の全 variant を `_ =>` なしで網羅し、variant 追加時の検査漏れをコンパイルエラーにする。

## PostgreSQL との差

- 式要素と述語がともに不正な複合エラーでは、PostgreSQL は述語を先に検査するが、本実装は要素を先に報告する
  （報告される SQLSTATE が異なりうる。どちらも拒否）。
- 修飾列参照（`docs.lang`）・`$n` は `42601`（PostgreSQL は修飾列を受理する。fail-closed）。
- 式索引の既存の差は expression-index.md を参照。

## 後続 Issue への申し送り

- #1654: 述語の永続化（正規化テキストと往復一致）、束縛（`42703`・`42804`・型依存の決定性の再判定）、
  予約列（`tenant_id`・`visibility`）の扱い、DML 維持、両最終ゲートの撤去。
- #1655: 含意判定は部分集合のみ（判定できなければ全走査）。#1656: プランナ選択と EXPLAIN。
  #1657: 部分 UNIQUE。`ON CONFLICT (...) WHERE` は SQL-49 ポインタで対象外のまま `42601`。

## Issue #1654（格納・DML 維持）

対象: Issue #1654（親 #1652）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11 (2)（述語を満たす行だけを
索引化する・UPDATE による入退場を書き込みと同じ txn で反映する・クラッシュ後も行と一致する）。
spec 本文は転記しない。実装は `sql::index_predicate`（束縛・評価）・`catalog`（`v6`・振り分け・列 DDL）・
`constraint`（DML の検査点）・`sql::scalar_index`（派生キー）。

### 判断

| # | 判断 | 理由 |
| - | ---- | ---- |
| D1 | 永続の部分索引表は作らない。派生キーは式キー索引と同じく、コミット済みの行からテーブル世代ごとに作り直す | 第 2 の複製が無いので、クラッシュ後に行と食い違う状態が構造的に生じない。DML は全経路で同じ write txn 内に世代を bump する |
| D2 | 守る不変条件は「部分索引があれば、全行で述語が評価でき、述語が TRUE の行では式要素も評価・索引化できる」。述語が NULL・FALSE の行は索引外で、式要素は評価しない | `((100 / n)) WHERE n <> 0` の `n = 0` 行を過剰に拒否しない。述語の評価エラーは文を拒否する |
| D3 | 部分索引の宣言は列宣言（`scalar_columns`）・式キー（`expression_keys`）・一意宣言のどれにも入れず、専用の一覧（`partial_indexes`／`partials`）へ振り分ける。EXPLAIN の列被覆の名前にも入れない（#1656 で `index_partial` として別経路に結線） | 含意判定（#1655）なしでプランナが使うと述語外の行が結果から落ちる（fail-open）。プランナ選択は #1656 |
| D4 | 述語は WHERE と同じ束縛（`bind_where_predicates`）・評価（`BoundOrGroup::matches`）を使い、第 2 の評価器を作らない。CHECK 用の束縛は使わない | 索引に入る行の集合と「`WHERE <述語>` が選ぶ行の集合」の一致が #1655・#1656 の前提 |
| D5 | セッションに依存する述語は束縛段で `42P17`（固定文言）: 式葉の `check_bound_index_expression_determinism`・`TIMESTAMPTZ` 列と文字列リテラルの宣言的比較・日時の特殊値。`'now'` 等は束縛が `22007` で受理しない | 保存テキストは書き込みのたびに再束縛するため、セッション TZ・評価時刻で結果が変わる述語は決定的でない |
| D6 | 予約列（`tenant_id`・`visibility`）・`visible()` を参照する述語は `42601`。疑似列 `id` は許可する | 共有カタログの永続宣言へテナント識別や可視性を埋め込ませない |
| D7 | カタログ形式に新版 `v6` を追加する（`v1`〜`v5` のバイト列は不変。旧バイナリは `v6` を読めない）。非一意に限る | 部分 UNIQUE の強制は #1657 で `v7` として追加した（「Issue #1657」節） |
| D8 | 派生構造 `PartialIndexEntries`（述語 TRUE のスロット `members` と、式要素の式キーを `members` だけで構築したもの）を `ScalarIndex` が持つ。起動時 opt-in 有効時のみ構築し、fail-soft（再束縛失敗・述語の評価エラー・予算超過・確保失敗のどれでも丸ごと作らない）。素の列要素のキー構造は作らない | #1648 の式キーの前例。消費者は #1656 の `resolve_partial_candidates` とテスト |
| D9 | `0A000` のまま残す組み合わせ（`UNIQUE`＋`WHERE` は #1657 で受理に変更）: `USING hnsw`＋`WHERE`・`COLLATE`＋`WHERE`・matview への `WHERE`。最終ゲートと防御ゲートは「撤去」ではなく「縮小」した | 強制や照合の永続形が無いまま宣言を残すと fail-open になる |
| D10 | 列 DDL: `DROP COLUMN` は `referenced_columns()` が述語の参照列を含むため連鎖削除。`RENAME COLUMN` は保存テキストを再解析し AST 上の識別子を写像して再束縛し、参照列集合の一致を確認してから保存（失敗は `0A000`・txn 破棄）。述語・要素が参照する列への `ALTER COLUMN TYPE` は `0A000`（無変化） | 放置すると次の書き込みで保存テキストが再束縛できず `CorruptSchema` になる |

### `v6` の行構成（`\n` 区切り）

`v6`／テーブル／`scalar`／列 CSV（式の位置は空トークン）／照合行（全要素 `-`）／式行（列は `-`、式は
正規化 SQL の小文字 16 進）／式の参照列行（式ごとに `;` 区切り）／述語の正規化 SQL（小文字 16 進）／述語の
参照列 CSV（空可）。デコードは行の過不足・余剰・hex／UTF-8 の不正・空や 4096 バイト超の述語・参照列の
識別子不正・列と式の排他関係の破れ・照合の混入・種別 `hnsw`・値全体の長さ超過を `CorruptSchema` として
拒否する。構文の再解析はデコードでは行わない（作成時・DML ゲート・派生キー構築時に行う）。

### 判定順（`create_index_bound`、同一 write txn）

名前衝突 → 対象の種別（matview＋述語は `0A000`）→ テーブル存在 → 素の列の整合 → 式ごとの束縛 → 述語の束縛 →
既存行の検証（全テナント・全可視性。評価エラーは SQLSTATE を保った固定文言）→ 件数上限 → 保存（`v6`）→
テーブル世代・索引カタログ世代の bump。DML の検査順は 索引同期 → CHECK → 式評価 → 部分索引 → UNIQUE → FK。

### SQLSTATE

| 区分 | SQLSTATE |
| ---- | -------- |
| 未知列 | `42703` |
| 型不整合・日時の特殊値 | 束縛側の SQLSTATE（`22007` 等） |
| セッション依存の述語 | `42P17` |
| 予約列・`visible()` | `42601` |
| 述語の評価エラー（DML・作成時の既存行） | 評価エラー自身（0 除算 `22012` 等）。作成時は固定文言 |
| hnsw・COLLATE・matview・述語列の `ALTER TYPE`（部分 UNIQUE は #1657 で受理） | `0A000` |
| 述語テキスト 4096 バイト超 | `54000` |

### テナント境界

- DML の検査点はサーバー側で導出したテナントの物理キーで `written_ids` だけを読み戻す。他テナントの行は読まず、
  存在オラクルにならない。
- `CREATE INDEX` の全件検証は DDL 権限ゲート（`42501`）の後の運用者権限の操作で、不可視行の失敗も検出する。
  評価エラーは固定文言に置換し、テナント・行 id・値・件数を応答へ出さない。
- 派生キーの構築元は RLS 適用済みのスナップショットだけ。部分索引のプランナへの結線は #1656（下記）。

### PostgreSQL との差

- `TIMESTAMPTZ` 列とゾーン無しリテラルの比較を `42P17` にする（PostgreSQL は定数畳み込みで受理する）。
- 述語・式要素が参照する列の `ALTER COLUMN TYPE` を拒否する。

### 検証

`crates/engine/tests/sql_partial_index.rs`（宣言側）・`sql_partial_index_dml.rs`（作成・永続・入退場・既存行検証・
テナント境界・プランナ不可視・列 DDL）、`crates/wire-server/tests/wire_index_ddl.rs`、`index_predicate`／`catalog`／
`scalar_index` の単体テスト（`v6` の往復と破損・述語の往復・`members` のオラクル一致・fail-soft・再オープン後の
再構築）。クラッシュ整合は `make crash-test-unique-index`（`docs_pidx` 表。入退場の UPDATE を含めて SIGKILL）。

### 対象外（後続）

含意判定 #1655・プランナ選択と EXPLAIN #1656・部分 UNIQUE の強制 #1657（実装済み）・素の列要素のキー構造・部分索引への
`COLLATE`／matview／`USING hnsw`・述語が参照する列の `ALTER COLUMN TYPE` の再束縛。`ON CONFLICT (...) WHERE` は
SQL-49 ポインタのとおり `42601` のまま。

## Issue #1655（述語の含意判定）

INDEX-11 ポインタ。`sql::index_implication::query_implies_partial_predicate` は「クエリの `WHERE`（Q）が TRUE の
行は索引述語（P）も TRUE」を保守的に証明する純関数で、プランナへの結線は #1656（`implied_partial_index_names` が呼ぶ）。SQL 表層・SQLSTATE・
EXPLAIN・結果は変えない。公開 API の追加なし（`pub(crate)`）。

| 項目 | 判断 |
| ---- | ---- |
| 判定の層 | 束縛済み表現（`MetadataFilter`・`BoundExpr`・`BoundOrGroup`）。両辺が同じ `bind_where_predicates` を通るため列添字・型付きリテラル・書き換えが一致し、評価器との意味論のずれが無い |
| 既定値 | P の未認識の項は「含意しない」。Q の未認識の項と Q の `OR` 群は無視（条件を落とすと Q は弱くなるだけ）。P の `OR`・`NOT`・`LIKE`・式は Q に構造的に同一の項があるときだけ含意 |
| 認識する原子 | 等価・範囲・`IN`・`BETWEEN`・`IS [NOT] NULL`（`CHAR(n)` 列を除く）、整数列と有限の数値定数の比較（逆向き可）、疑似列 `id` の比較。Q の原子は列ごとに下限・上限・等価集合・所属集合・NULL 状態へ畳み、P の各項を照合する |
| 推論に使わない | 整数性（`n > 4` は `n >= 5` を含意しない）、浮動小数列、`CHAR(n)`、TEXT の範囲比較、照合依存の比較。結果は常に正しい（索引を使わないだけ） |
| NULL | 等価・範囲・所属は非 NULL を含意する（P の `IS NOT NULL` を満たす）。P の `IS NULL` は Q の `IS NULL` でのみ満たす |
| 値の比較 | 評価器と同じ比較（`NUMERIC` は `numeric::cmp_exact`。`1.0` と `1.00` は同値）。種別違い・比較不能は含意しない |
| 作業予算 | 比較 1 回を 1 単位として上限（既定 2^18）を設け、超過は `false`（未信頼入力による CPU 増幅の防止） |
| 検証 | 値グリッド上のオラクルテスト（含意と判定した組は、実評価器で Q が TRUE の行すべてで P も TRUE）と明示の正例・負例 |

申し送り（Issue #1656）: 両辺を同じスキーマ世代で束縛する（索引側は `rebind_stored_index_predicate` の結果の
`BoundIndexPredicate::conjunction`）。プリペアド文は具体値で束縛した後に毎回判定し、describe 用ダミー値での判定や
判定結果の使い回しをしない。`false` のときは従来の経路へ。浮動小数・`CHAR(n)`・TEXT 範囲・`OR`/`NOT` の推論拡張は、
オラクルテストを伴う別 Issue で扱う。

## Issue #1656（プランナ選択と EXPLAIN）

INDEX-11 (3)・INDEX-7 ポインタ。含意判定（#1655）と格納（#1654）を検索 SELECT の読み取り経路へ結線する。
SQLSTATE は変えない。公開 API は `ScalarPlan::IndexPartial` の追加（`#[non_exhaustive]`）。クライアントから見える
EXPLAIN の語彙として `index_partial` が増えるため CHANGELOG の Breaking Changes に載せる。

| # | 判断 | 理由 |
| - | ---- | ---- |
| P1 | Q が P を含意するとき、部分索引の格納行は Q に一致する行の**上位集合**にすぎない。候補は必ず `on_visible_row` を再適用する経路（`filter_cached_rls_rows_subset`／`build_from_cached_rls_rows_subset`）へ流し、索引信頼マスク（`mask_trusted_defer`）には入れない | 信頼マスクの前提は「WHERE が索引で完全に被覆される」こと。部分索引は満たさず、入れると述語外の行が混ざる（fail-open） |
| P2 | 参加条件: opt-in 済み・`scalar_prefilter`・WHERE あり・**通常の分類が `PlainScan`**（式索引の一致を含む。宣言による補正の前の値）。判定に使う量は exec と EXPLAIN で揃える | 宣言補正後の値で判定すると exec と EXPLAIN が食い違う。通常の索引経路には触れず、カタログを読むのは全走査になるクエリだけ |
| P3 | 選択関数は `index_implication::implied_partial_index_names` の 1 本だけ。宣言ごとに再束縛・往復一致・単一分岐を確認し、含意される全索引の名前を昇順・重複なしで返す。失敗はすべて「含意しない」 | EXPLAIN は静的な判定でなければならずデータ（行数・格納行数）に依存できない |
| P4 | 実行時の候補は、含意された索引のうち構築済みの格納行を昇順で交差して作る（未構築は飛ばす。1 本もなければ `FallbackNoIndex`）。既存と同じ選択度ゲートを通す | 交差しても上位集合のまま健全。EXPLAIN は実行時の縮退を観測しない既存の契約を維持 |
| P5 | EXPLAIN は `scalar_plan: index_partial index=<名前>[,<名前>...]`（縮退理由なし）。`ScalarPlan::IndexPartial` は分類関数からは返さず、exec と EXPLAIN が付与する | `plain_scan` に `index=` を付けない既存規則と、部分索引を使うのに `plain_scan` と出す誤りの回避 |
| P6 | 部分索引の名前は列被覆の判定を通さず選択関数の結果をそのまま使う。`explain_index_names_in_txn` が部分索引を列被覆から除外する動作（#1654 D3）は維持する | 列被覆に含めると含意しないクエリにも名前が出る |
| P7 | 集計・`GROUP BY`・広域取得（`sql::scan`）は変更しない | 集計の信頼経路は完全被覆が前提で P1 と衝突する |
| P8 | 素の列要素の専用キー・部分索引の式キーでの絞り込み・通常の索引候補との組み合わせはしない | 範囲を最小にする。性能改善の後続候補で、正しさには影響しない |
| P9 | 判定はクエリの実行ごとに具体値で束縛した Q に対して行い、結果は使い回さない。カタログを読めなければ空（全走査）に倒す | プリペアド文をダミー値で判定しない（#1655 からの申し送り）。fail-closed |

テナント境界: 候補は RLS 適用済みスナップショットのスロットの部分集合（同一性ガード通過分）で、`on_visible_row`
を必ず再適用するため述語外の行・不可視行は混入しない。EXPLAIN が出すのは検証済み識別子の索引名だけで、件数・値・
テナント情報は出さない。既知のコスト: 通常の分類が `PlainScan` のクエリは、opt-in 時に宣言の読み取りと再束縛を
行う（全走査になるクエリだけ。世代キーのキャッシュは #1650 と同じく後続候補）。

検証: `tests/partial_index_planner.rs`（含意する形の結果が全走査オラクルと一致・索引経路の発火と信頼マスク
不使用・含意しない形・opt-in なし・テナント境界・UPDATE による入退場・集計・`USING PLAN` の EXPLAIN）と
`index_implication`・`scalar_index`・`explain` の単体テスト。対象外: 集計・広域取得での使用、通常の索引候補との
組み合わせ、部分索引の式キーでの絞り込み、世代キーのキャッシュ、部分 UNIQUE（#1657）、含意判定の拡張。

## Issue #1657（部分 UNIQUE 索引）

INDEX-11 (4)・INDEX-9 ポインタ。`CREATE UNIQUE INDEX ... WHERE <述語>` を受理し、述語が TRUE の行どうしの間
でだけ一意性を強制する（テナントごと・NULLS DISTINCT・`23505`）。述語外（NULL・FALSE）の行どうし、および
述語外と述語内の行の間の重複は許可する。作成時の既存行の重複検査も述語 TRUE の行だけが対象。

### 判断

| # | 判断 | 理由 |
| - | ---- | ---- |
| E1 | 永続形は新版 `v7`（`v6` の行構成のうち、参照列行と述語行の間に固定行 `unique` を挟む）。`v1`〜`v6` のバイト列は不変。旧バイナリは `v7` を読めない | `v4`→`v5` の前例と同じ。`unique` 行の欠落・別値・行順違い・`v6` への混入は `CorruptSchema` |
| E2 | 部分 UNIQUE は UNIQUE 制約へ展開せず、索引カタログだけが持つ（式一意索引と同じ） | 展開すると述語外の行まで強制される。`ON CONFLICT` の推論対象・FK の参照先にもさせない |
| E3 | 強制は既存の永続一意索引（`user_uniq/{table}`）に序数を足して行う。序数は 主キー → UNIQUE 制約 → 式一意索引 → 部分一意索引（各索引名の昇順） | 既存キーの序数・バイト列を変えない |
| E4 | `UniqueKeyPlan::row_keys` は、部分一意索引の序数について述語が TRUE でなければキーなし（NULL を含む行と同じ概念）を返す | 登録・衝突検査・逆引きの後片付けに新しい概念が要らない。UPDATE の入場は新キーの衝突検査、退場は stale 判定で処理される |
| E5 | 部分一意索引の述語・式要素は `UniqueKeyPlan::build` 内で保存テキストを再解析・再束縛して自前でコンパイルする（`sql::index_predicate::compile_stored_predicate` を DML ゲートと共有）。漂流は `CorruptSchema` | 部分索引の式要素は全行評価の対象外（`decls.keys` に入らない）ため。第 2 の束縛経路を作らない |
| E6 | `unique_index::FORMAT_VERSION` を 3 から 4 へ上げ、`schema_signature` に索引名・述語の正規化 SQL・要素の署名を足す | bump しないと既存テナントのマーカーが構築済みと誤認され、新序数が未バックフィルのまま強制漏れ（fail-open）になる。代償は各テナント・表で初回書き込み時に 1 回だけの再構築 |
| E7 | 作成時は「既存行の述語・式要素の検証 → 述語 TRUE の行だけのテナントごとの重複走査」の順。重複は `23505`（固定文言） | 述語の評価エラーが重複より先に報告される（DML の検査順と一致）。キーは `row_keys` の 1 経路だけで計算する |
| E8 | 前提ゲートは式一意索引と同じ `check_unique_index_gate`（素の列の型ゲート・制約名前空間の衝突） | INDEX-9 と同じ規則 |
| E9 | `DeclaredPartialIndex` に `unique` を追加。`partials` には一意・非一意の両方を入れ続ける | DML ゲートと派生キーは一意性に関係なく必要 |
| E10 | `DROP INDEX`・`DROP COLUMN`・`RENAME COLUMN` は制約を介さず `user_uniq` を失効させる分岐へ。`catalog::dependency` の `index_children` は述語付き一意索引を FK の参照先にしない | 対応する UNIQUE 制約が無いため（無いと `CorruptSchema`） |

### `v7` の行構成（`\n` 区切り）

`v7` / テーブル / `scalar` / 列 CSV / 照合行（全 `-`）/ 式行 / 参照列行 / `unique` / 述語 hex / 述語の参照列 CSV。

### SQLSTATE

| 区分 | SQLSTATE |
| ---- | -------- |
| 述語内の重複（DML・作成時の既存行） | `23505`（固定文言。値・テナント・行 id・件数を含まない） |
| 述語・式要素の評価エラー | 評価エラー自身（作成時は重複より先に報告） |
| 素の列の型ゲート・制約名の衝突 | 式一意索引と同じ |
| `ON CONFLICT (列)` が部分一意索引しか無い列を指す | `42P10`（推論対象外） |
| `ON CONFLICT (...) WHERE` | `42601`（SQL-49 ポインタ。従来どおり） |
| hnsw・COLLATE・matview・述語列の `ALTER TYPE` | `0A000`（従来どおり） |

### テナント境界

- 永続一意索引のキーの第 1 要素は常にサーバー側導出のテナント。DML の検査は自テナント行だけを読み戻す。
- 作成時の走査は DDL 権限ゲート（`42501`）の後の運用者操作で、テナントが変わるたびに集合をリセットする
  （他テナントの同値を重複としない）。応答は固定文言で、他テナントの存在を示さない。

### PostgreSQL との差

部分 UNIQUE 索引は `ON CONFLICT (列) WHERE <述語>` の推論対象にならない（対象外のまま `42601`）。述語なしの
`ON CONFLICT (列)` が部分一意索引を仲裁に使わない点は PostgreSQL と同じ。

### 検証

`crates/engine/tests/sql_partial_unique_index.rs`（述語内外の重複・入退場・NULL・作成時検査・評価エラーの順序・
式要素・テナント・再オープン・列 DDL・`ON CONFLICT`）、`catalog` 単体テスト（`v7` の往復と破損）、
`wire_index_ddl.rs`（wire 経由の `23505`）、クラッシュ整合は `make crash-test-unique-index`（`docs_pidx` 表）。

### 対象外（後続）

プランナ選択と EXPLAIN（#1656）・述語参照列の `ALTER COLUMN TYPE` の再束縛・`USING hnsw`／`COLLATE`／matview との
組み合わせ。
