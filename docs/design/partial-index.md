# 部分索引の宣言側（述語 parser と決定性検査）

対象: Issue #1653（親 #1652）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11・INDEX-10、
`docs/spec/04-behavior/error-format.md` ERR-6（spec 本文は転記しない）。
兄弟: #1654（格納・DML 維持）、#1655（述語の含意判定）、#1656（プランナ選択・EXPLAIN）、
部分 UNIQUE は #1657。式索引の宣言側（[expression-index.md](expression-index.md)）と同じ形で、
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
| 決定的な述語（#1654 まで。#1654 で縮小） | `0A000` | `partial indexes are not supported yet`（#1654 で撤去。現在の `0A000` は `partial unique indexes are not supported yet`・`partial indexes are not supported for this index kind yet`・`COLLATE on a partial index is not supported yet`・`partial index on a materialized view is not supported`） |

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
| D3 | 部分索引の宣言は列宣言（`scalar_columns`）・式キー（`expression_keys`）・一意宣言のどれにも入れず、専用の一覧（`partial_indexes`／`partials`）へ振り分ける。EXPLAIN にも名前を出さない | 含意判定（#1655）なしでプランナが使うと述語外の行が結果から落ちる（fail-open）。プランナ選択は #1656 |
| D4 | 述語は WHERE と同じ束縛（`bind_where_predicates`）・評価（`BoundOrGroup::matches`）を使い、第 2 の評価器を作らない。CHECK 用の束縛は使わない | 索引に入る行の集合と「`WHERE <述語>` が選ぶ行の集合」の一致が #1655・#1656 の前提 |
| D5 | セッションに依存する述語は束縛段で `42P17`（固定文言）: 式葉の `check_bound_index_expression_determinism`・`TIMESTAMPTZ` 列と文字列リテラルの宣言的比較・日時の特殊値。`'now'` 等は束縛が `22007` で受理しない | 保存テキストは書き込みのたびに再束縛するため、セッション TZ・評価時刻で結果が変わる述語は決定的でない |
| D6 | 予約列（`tenant_id`・`visibility`）・`visible()` を参照する述語は `42601`。疑似列 `id` は許可する | 共有カタログの永続宣言へテナント識別や可視性を埋め込ませない |
| D7 | カタログ形式に新版 `v6` を追加する（`v1`〜`v5` のバイト列は不変。旧バイナリは `v6` を読めない）。非一意に限る | 部分 UNIQUE の強制は #1657 |
| D8 | 派生構造 `PartialIndexEntries`（述語 TRUE のスロット `members` と、式要素の式キーを `members` だけで構築したもの）を `ScalarIndex` が持つ。起動時 opt-in 有効時のみ構築し、fail-soft（再束縛失敗・述語の評価エラー・予算超過・確保失敗のどれでも丸ごと作らない）。素の列要素のキー構造は作らない | #1648 の式キーの前例。消費者は #1656 まではテストだけ |
| D9 | `0A000` のまま残す組み合わせ: `UNIQUE`＋`WHERE`・`USING hnsw`＋`WHERE`・`COLLATE`＋`WHERE`・matview への `WHERE`。最終ゲートと防御ゲートは「撤去」ではなく「縮小」した | 強制や照合の永続形が無いまま宣言を残すと fail-open になる |
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
| 部分 UNIQUE・hnsw・COLLATE・matview・述語列の `ALTER TYPE` | `0A000` |
| 述語テキスト 4096 バイト超 | `54000` |

### テナント境界

- DML の検査点はサーバー側で導出したテナントの物理キーで `written_ids` だけを読み戻す。他テナントの行は読まず、
  存在オラクルにならない。
- `CREATE INDEX` の全件検証は DDL 権限ゲート（`42501`）の後の運用者権限の操作で、不可視行の失敗も検出する。
  評価エラーは固定文言に置換し、テナント・行 id・値・件数を応答へ出さない。
- 派生キーの構築元は RLS 適用済みのスナップショットだけ。部分索引は #1656 までプランナに結線しない。

### PostgreSQL との差

- `TIMESTAMPTZ` 列とゾーン無しリテラルの比較を `42P17` にする（PostgreSQL は定数畳み込みで受理する）。
- 述語・式要素が参照する列の `ALTER COLUMN TYPE` を拒否する。

### 検証

`crates/engine/tests/sql_partial_index.rs`（宣言側）・`sql_partial_index_dml.rs`（作成・永続・入退場・既存行検証・
テナント境界・プランナ不可視・列 DDL）、`crates/wire-server/tests/wire_index_ddl.rs`、`index_predicate`／`catalog`／
`scalar_index` の単体テスト（`v6` の往復と破損・述語の往復・`members` のオラクル一致・fail-soft・再オープン後の
再構築）。クラッシュ整合は `make crash-test-unique-index`（`docs_pidx` 表。入退場の UPDATE を含めて SIGKILL）。

### 対象外（後続）

含意判定 #1655・プランナ選択と EXPLAIN #1656・部分 UNIQUE の強制 #1657・素の列要素のキー構造・部分索引への
`COLLATE`／matview／`USING hnsw`・述語が参照する列の `ALTER COLUMN TYPE` の再束縛。`ON CONFLICT (...) WHERE` は
SQL-49 ポインタのとおり `42601` のまま。
