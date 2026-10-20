# 宣言 PRIMARY KEY の点検索（広域取得）の設計記録

Issue #2198（親 #2197・上位 #1710）。対象ビヘイビア: TABLE-23・TABLE-16・TABLE-12、
RLS-9・RLS-10、SQL-27（EXPLAIN の語彙）・SQL-15（広域取得）、ERR-6。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断のみを記録する。

## 背景

宣言した PRIMARY KEY は、永続一意索引 `user_uniq/{table}`（序数 0）に格納され、書き込み時の
一意性検査（`23505`）にだけ使われていた（[unique-index.md](unique-index.md)・
[sql-primary-key.md](sql-primary-key.md)）。読み取りは索引を使わず、
`SELECT ... WHERE <pk 列> = <リテラル> LIMIT n`（広域取得 `sql::scan`）は常に全行を走査し、
EXPLAIN も `scalar_plan: plain_scan`／`access_path: full_scan` 固定だった。本 Issue は、PK の
全構成列への等価述語を PK 索引の点検索として計画・実行し、EXPLAIN に表示する。結果は索引なしの
走査と完全に一致させる。

## 設計

### 静的分類（実行と EXPLAIN の単一情報源）

`sql::pk_point::classify_pk_point(schema, bound)` が、スキーマと束縛結果だけから
`Point { canonical_key }` か `NotApplicable` を返す。`PolicyContext`・テナントごとの索引状態・
データには依存しない。`sql::scan` の経路選択と `core.rs::scan_explain_result`（SQL の
`EXPLAIN SELECT ... LIMIT n` と NoSQL の `explain: true` の両入口が共有）が同じ関数を呼ぶので、
2 行は実経路と矛盾しない。

`Point` になる条件（すべて満たす場合だけ）:

1. 宣言 PK がある（許可型は `ColumnType::is_primary_key_allowed` のまま。本 Issue では広げない）。
2. ウィンドウ項目も `ORDER BY` も無い（順序なしの経路だけを対象にする）。
3. PK の各構成列に、トップレベルの AND 連言（`metadata_filters`・`expr_filters`）としてリテラル等価
   がある。`or_filters` は分類に使わない（取り出した行で再評価される）。
4. リテラルを列の値へ完全に変換できる。

| 列型 | 等価述語の束縛形 | 変換上の注意 |
| ---- | ---------------- | ------------ |
| TEXT／VARCHAR／ENUM | `FilterOp::Equals` | 照合は deterministic で、等価はバイト列の等価と一致する |
| CHAR(n) | `FilterOp::Equals`（右側空白は束縛時に除去済み） | 格納形は n 文字へ空白で詰めた値。リテラルを同じ長さへ詰める。n 超過は `NotApplicable` |
| SMALLINT／INTEGER／BIGINT | `expr_filters` の `<列> = <数値>` | 整数でない値・型の値域外・\|v\| >= 2^53 は `NotApplicable`（下記） |
| BOOLEAN | `FilterOp::BoolEquals` | — |
| DATE／TIMESTAMP／UUID／BYTEA | `FilterOp::TypedCompare { op: Eq }` | — |

**整数の 2^53 境界**: 整数列の等価は `f64` 比較で評価される（`BoundExpr::ColumnRef`）。行側が
2^53 を超える BIGINT は `f64` 比較の時点で `22003` になる既存の契約があり、リテラルが 2^53 以上だと
「`f64` 上で等しい行」と「整数として等しい行」が食い違いうる。そのため 2^53 未満の整数リテラルだけを
点検索にし、それ以外は全走査に倒す。点検索は他のキーの行を読まないので、範囲外の値を持つ別キーの行が
あっても `22003` を出さない（全走査が無関係な破損・範囲外の行で失敗するのと同じ種類の差で、
結果の契約には含めない）。

### 正準キー

書き込み側の符号化器 `constraint::push_canonical_component` を PK の宣言順に連結して作る
（第 2 の符号化器を持たない）。不変条件: 「行 R が WHERE の PK 等価述語を満たす ⇒ R の PK の正準キー
== 変換したリテラルの正準キー」。偽陽性は WHERE の再評価で除かれ、偽陰性はこの不変条件と、判断に
迷う型・値を必ず `NotApplicable` に倒す規則で防ぐ。

### 実行（テナントごとの探査）

`sql::scan::execute_scan_with_budget` の順序なし経路で、`Point` のときだけ行の反復元を
`pk_point::for_each_candidate` に差し替える。行ごとの処理（`with_visible_row` ＋投影＋
OFFSET／LIMIT／予算）は全走査と共有する。

1. 探査対象のテナントは、`ctx` が他テナントの `Public` 行を可視としないなら自テナントだけ
   （経路 (A) と同じ判断）。それ以外は行テーブルから `(t, u64::MAX)` の次へ seek して昇順に列挙する
   （索引テーブルから列挙すると、行はあるがマーカーが無いテナントを見落とす）。
2. テナントごとにマーカー `(t, [0x00])` を読み取り専用で検証する（`FORMAT_VERSION` と
   `schema_signature`。書き込み側と同じ `marker_matches`）。
   - 一致: 正引き `(t, [0x01][0u16 BE][canonical])` を点照会し、ヒットなら locator から行を読み戻して
     `with_visible_row` に渡す。ミスは「このテナントに一致する生存行は無い」（マーカーが有効な間は
     生存行の各キーが正引きを必ず持つ。[unique-index.md](unique-index.md) の不変条件）。stale な
     正引き（D4）は WHERE の再評価で除かれる。
   - 索引テーブルなし・マーカーなし・不一致: そのテナントの `(t, 0)..=(t, u64::MAX)` を範囲走査する
     （`ALTER TABLE ADD PRIMARY KEY`・`ADD UNIQUE` などで無効化された後、まだ書き込みが無いテナント）。
3. `LIMIT` 件に達したら打ち切る。索引や宣言が壊れている場合は固定文言の `XX000`
   （値・id・テナントを含めない）。

性質: 出力（行・順序・OFFSET の数え方）は順序なしの全走査と同一。コストは O(T・log n)（T は探査対象の
テナント数）で、メモリはテナント数に比例しない。`ReadSource` 越しに動くので、確定済みスナップ
ショット・明示トランザクションの書き込み txn・一時表・カーソル・ビュー展開後の scan に同じ実装が効く。

### locator の扱い

正引きの値は現状、行の物理キー `(tenant, id)` の `id`（u64）で、`pk_point::row_key_for_locator` の
1 か所に閉じ込めた。索引の行参照を rowid へ移す #2194
（[rowid-primary-key-separation.md](rowid-primary-key-separation.md)）がマージされたら、この関数を
追随させる。`user_uniq` のレイアウトと `FORMAT_VERSION` は本 Issue では変更していない。

### テナント境界

読み取り側の候補探査は、他テナントの索引パーティション（マーカーと正引き）を**読み取り専用**で
参照する。書き込み側の一意性判定（自テナントのパーティションだけを見る。unique-index.md D5）は
変えない。他テナントの `Public` 行は全走査でも読まれて `is_visible` で選別されており、それを前例とする。
可視性の判定は `PolicyContext::is_visible` だけが行い、新しいテナント比較の経路は作らない。
他テナントの `Private` 行は結果にも OFFSET の数にも現れず、応答は「該当行なし」と同一になる
（`tests/sql_scan_pk_point.rs`）。

### EXPLAIN

`ScalarPlan::IndexPrimaryKey`（`index_primary_key`）と `AccessPath::PrimaryKeyLookup`
（`primary_key_lookup`）を追加した。`Point` のときだけ両方を出し、それ以外は従来どおり
`plain_scan`／`full_scan`（既存の出力はバイト単位で不変）。テナント単位のマーカー欠落による範囲走査への
縮退は実行時の縮退であり、可視カーディナリティ・テナントの存在情報に繋がるため EXPLAIN には出さない。
索引名の注記（`index=`）も付けない。新しい語彙は SQL-27 の確定時に spec 側とすり合わせる事項
（[explain-search-engine-exposure.md](explain-search-engine-exposure.md)）。

### ALTER TABLE ADD／DROP PRIMARY KEY

分類は常に現在のスキーマから導く。ADD の直後は索引が無効化されているため、実行時はテナント単位で
範囲走査へ縮退し、そのテナントの次の書き込みで再構築されて索引経路へ移る。DROP の後は PK が無いので
全走査になる。いずれも結果は全走査と一致する（[alter-table-primary-key.md](alter-table-primary-key.md)）。

## 検証

- `crates/engine/tests/sql_scan_pk_point.rs`: 結果の一致（単一・複合・全許可型）、EXPLAIN の語彙、
  可視性文脈ごとの一致（Private の不可視）、stale な正引き（DELETE／UPDATE／再 INSERT）、
  ADD／DROP PRIMARY KEY、明示トランザクション・一時表・ビュー・カーソル、23505／23502。
- `crates/engine/src/sql/pk_point.rs` の単体テスト: 計測フックで探査回数・縮退回数を確認し、
  単一テナントでは行数（10 件と 400 件）によらず探査 1 回・範囲走査 0 回であること、マーカー欠落の
  テナントだけが縮退し次の書き込みで索引経路へ移ること、自テナントだけを探査する場合を固定した。

## 対象外・申し送り

- PK 列型の許可範囲は広げない（Issue 本文は「索引可能な全スカラー型」だが、許可型の範囲は TABLE-16 の
  改訂注記・TABLE-23 のポインタに従う）。FK の参照元列が許可リストを共有している
  （foreign-key.md D3）ことと、TIME／TIMESTAMPTZ／INTERVAL は正準タグが番兵 0 で新しいタグの
  永続化も要ることが実装上の制約。REAL・DOUBLE・NUMERIC・TIME・TIMESTAMPTZ・INTERVAL への拡張は、
  spec の改訂を確認したうえで FK 用の許可リストを分離する別 Issue とする。
- `UPDATE`／`DELETE ... WHERE <pk> = ...`、検索 SELECT の事前フィルタ、集計、`ORDER BY` 付き scan への
  適用。`pk_point` の分類と探査を再利用できる（#2197 配下の後続で扱う）。
- `id` 列（PK 未宣言の従来方式の表の暗黙主キー）への点読み（#2200 の後に見直す。#2199 で新しく作る表は
  暗黙の `id` を持たず、宣言した `id` は通常の列として PK／UNIQUE の点検索の対象になる）。
- 無効化した索引を DDL の txn 内で全テナント分即時に再構築すること（現状は各テナントの次の書き込みで
  遅延して再構築する）。
- 読み取り経路の `UniqueKeyPlan`・シグネチャのキャッシュ（問い合わせごとの組み立て費用の削減）。
