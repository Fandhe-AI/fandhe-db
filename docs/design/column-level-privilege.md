# 列レベル制御の方針（PostgreSQL の列権限の意味に揃える）

- ステータス: Proposed（自動運転のため、オーナーの承認は得ていない。承認済みとは扱わない）
- 対応 Issue: #2082（親 #2081。後続 #2083・#2084）。前提の Phase 6 は #1964・#1965・#1967・#1968・#1969
- ビヘイビア ID ポインタ: SQL-40（`docs/spec/04-behavior/sql-surface.md`）・RLS-12（`docs/spec/04-behavior/rls.md`）。
  契約本文は spec を正とし、本書へは転記しない
- 関連コード: `crates/engine/src/sql/privilege_gate.rs`・`crates/engine/src/sql/privilege_gate/{read,write,copy}.rs`・
  `crates/engine/src/catalog/privilege.rs`（`relation_grant_bits_in_txn`）・
  `crates/engine/src/sql/allowlist/grant.rs`（`COLUMN_PRIVILEGES`）・`crates/engine/src/sql/allowlist.rs`（`UndefinedColumn`）・
  `crates/engine/src/sql/{parser,relation}.rs`・`crates/engine/src/core.rs`（`parse_tokens_gated`）・
  `crates/engine/src/core/temp_route.rs`・`crates/wire-server/src/http/query/{scan,search,aggregate,explain,filter}.rs`
- 関連 doc: [role-privilege-model.md](role-privilege-model.md)（D5・D8・D9・D10・§8 J10・§12・§17・§18）・
  [privilege-check-paths.md](privilege-check-paths.md)・
  [row-level-isolation-comparison.md](row-level-isolation-comparison.md)・
  [fixed-tenant-boundary.md](fixed-tenant-boundary.md)・
  [rls-generalized-read-paths.md](rls-generalized-read-paths.md)
- 検証コード: なし（設計記録のみ。本書の追加でコードは変えない）

## 1. 背景・目的

- Phase 6 で列単位の `GRANT`／`REVOKE` は構文とカタログ（`acl` 表の列成分）まで実装済みだが、参照時の列集合の判定は未実装である
- 現在の暫定規則は「列単位の付与だけでは表単位の要求を満たさない」で、PostgreSQL より厳しい側に倒している
- [role-privilege-model.md](role-privilege-model.md) の D9 は「判定は共通ゲートの単一関数で行い、別機構を作らない」と決めるだけで、
  列権限の意味・結線点・応答契約が未決だった。これを #2083（束縛段への判定の追加）と #2084（全経路の越境マトリクス）の前提として固定する
- 根拠は、公開 Issue の記述、PostgreSQL 17 の公開ドキュメント・公開挙動、本 ADR 独自の決定（C1〜C6。#2082 で確定）に限る

## 2. 公開コードの現状

| 項目 | 現状 |
| ---- | ---- |
| ACL のキー | 列成分を持ち、列単位の付与を保存できる（#1961・#1965） |
| 列に付けられる権限 | SELECT／INSERT／UPDATE／REFERENCES（`COLUMN_PRIVILEGES`）。DELETE／TRUNCATE は表単位のみ |
| 読み取り・書き込み・COPY のゲート | トークン走査で relation 集合を集め、表単位のビットだけを判定する |
| 暫定規則 | 列単位の付与だけでは表単位の要求を満たさない（PostgreSQL より厳しい） |
| ビュー・matview | 列リスト付きの `GRANT` は `0A000`（role-privilege-model.md §8 J10） |
| 列の改名・削除 | `rename_column_acl_in_txn`・`remove_column_acl_in_txn` で列の ACL が追随する |

## 3. 決定

### C1. 列権限の意味（PostgreSQL 17 に揃える）

- 参照する列ごとに「表単位のビット OR その列のビット」で判定する。数える付与は、表単位の付与・その列への付与・
  所有者の暗黙権限で、INHERIT 閉包と PUBLIC の和を含む（`relation_grant_bits_in_txn` と同じ範囲）
- 表単位の権限は全列に及ぶ。表単位で要求を満たしていれば列ごとの照会は不要（高速経路）
- `SELECT *`・`t.*`・行全体の参照・`RETURNING *` は、対象の全列への要求に展開する。許可された列へ黙って絞らない
- 列を 1 つも参照しない読み取り（`count(*)`・`SELECT 1 FROM t` 等）には「少なくとも 1 列に SELECT」を要求する。
  これは PostgreSQL の権限検査（`ExecCheckOneRelPerms` の「いずれかの列」判定）の挙動であり、「全列を要求する」規則では誤る
- 判別できない形は、従来の方針どおり厳しい側（全列への要求）に倒す

### C2. 文種ごとの要求列

| 文種 | 要求 |
| ---- | ---- |
| SELECT | 投影・WHERE・JOIN ON／USING・GROUP BY・HAVING・ORDER BY・集計／窓関数の引数・サブクエリ／CTE／集合演算の内側が参照する列に SELECT。SQL 内の kNN（距離演算子の対象のベクトル列）・全文検索・ハイブリッド検索の対象列にも SELECT |
| INSERT | 値を与える対象列（列リストがあればその列、省略時は値の個数で決まる先頭の列）に INSERT。RETURNING の列に SELECT |
| UPDATE | SET の対象列に UPDATE。WHERE・RETURNING・SET の右辺の式で読む列に SELECT |
| DELETE | 表単位の DELETE。WHERE・RETURNING で読む列に SELECT |
| ON CONFLICT | 衝突対象の列と `DO UPDATE` の SET・条件で読む列に、上記と同じ組み合わせを要求する |
| COPY | 列リストがあればその列、省略時は全列。TO は SELECT、FROM は INSERT |
| ビュー・matview | 利用者にはビュー自身の表単位 SELECT だけを判定する。基底表はビュー所有者の権限で読む扱い（§12 と同じ）。ビューへの列 `GRANT` は J10 の `0A000` のまま |
| §17 の PostgreSQL より厳しい箇所 | 式の SET・`id` 一致形などで SELECT を要求する箇所は、#2083 で列単位へ精密化するか判断する。未定の間は厳しい側（全列）とする |

### C3. 統合方式（第 2 の判定系を作らない）

- 判定点は `privilege_gate::check` の 1 本のまま。要求の型を「relation ＋ 表単位のビット ＋ 列ごとのビット」へ一般化する
- ACL の照会は `relation_grant_bits_in_txn` を拡張し、同じスナップショットで列ごとのビットも返す。別の関数・別の閉包計算は作らない（D8）
- 列集合はトークン走査ではなく束縛の結果（`sql/parser.rs`・`sql/relation.rs` の列解決）から一括で得る。
  トークン走査では識別子が列か別名か区別できないため。束縛の結果を得られない経路は全列への要求に倒す
- 暫定規則は C1 の規則に置き換える。置き換えで fail-open にしない（表単位も列単位も満たさなければ拒否）。
  `privilege_gate.rs` の冒頭契約と §12・§17 の記述の更新は #2083 が担う
- 移行期の契約は変えない（未束縛のセッションは素通し・運用者権限と初期ロールの全権限は所有者相当・`Some(0)` は拒否）
- 列集合の件数に上限を置き、ACL を照会する前に検査する（untrusted 入力）。上限値は #2083 で決める
- 結線する入口（新しい判定は増やさず、既存のゲート呼び出しに列集合を渡す）
  - `core.rs` の `parse_tokens_gated`（簡易クエリ・明示 Tx・PL/pgSQL の埋め込み SQL）
  - `core/temp_route.rs` の Parse・Bind・Describe
  - カーソルの `DECLARE`（`FETCH` では再検査しない）と各 `EXPLAIN` 形
  - 分割実行 DML のチャンクごとの再判定（`RelationPrivilegeRecheck`）
  - COPY
  - HTTP の bound 読み取り（scan・search・plan search・aggregate・各 explain）と bound 書き込み

### C4. NoSQL（HTTP）表層への写像

- `columns` を省略した要求は全列の要求とみなす。許可列に絞らず `42501` で拒否する
- `filter`・`sort`・検索の対象列（ベクトル・全文）・aggregate のグループ化キーと集計の対象列は SELECT を要求する
- 応答に暗黙に含まれる項目は、どの列への要求かを決める。スコアは計算元の対象列への要求とし、
  行の識別子が実在する列ならその列への要求とする

### C5. 応答契約と存在情報の秘匿

- (a) `42501` は固定文言にする。列名・他の列名・列数・表の構造・ロール ID・テナントを含めない。
  `SELECT *` の拒否でも、どの列が不許可かを示さない
- (b) テナント境界の外: 解決の順序は「構文 → relation の解決（`42P01`／`3F000`）→ 列の解決 → 権限」。
  他テナントの relation は relation の解決の段で不在と同じ応答になり、列の解決と列権限の分岐に入らない（D10・RLS-12）
- (c) テナント境界の内: PostgreSQL と同じく、存在しない列は `42703`、存在するが権限の無い列は `42501` とする。
  名前を引ける範囲では PostgreSQL も両者を区別し、テナント外の秘匿とは両立する。
  `42703` の文言・ヒントに他の列の候補を含めない（現状 `UndefinedColumn { name }` は入力された名前だけを持つ。この性質を不変条件とする）
- (d) 拒否は実行前で、副作用はゼロ。行の値に依存しない。`Describe`（RowDescription）・`EXPLAIN` の計画・HTTP の explain は、
  列のメタデータを返す前に拒否する（§12 のパターン）
- (e) 許可された列だけへ黙って絞る方式（Elasticsearch の FLS 型）は採らない
  - RowDescription や `columns` の形が付与状態で変わり、許可状態の推知経路になる
  - 欠落を「列が存在しない」と誤読させ、LLM へ渡すコンテキストの正確さを損なう
  - fail-closed の明示的な拒否のほうが PostgreSQL と整合する

### C6. テナント境界との関係

- 列権限は RLS の暗黙適用より前段に加わる追加の制限である。列権限で RLS を緩めたり、テナント境界を広げたりはできない（RLS-12 のポインタ）
- 列 ACL はテナントごとに独立している（D5）。PUBLIC は同じテナントのロール全体を指す

## 4. 他方式との比較

| 項目 | PostgreSQL 17（列 `GRANT`） | Elasticsearch `field_security`（`grant`／`except`） | fandhe（本 ADR） |
| ---- | ---- | ---- | ---- |
| 不許可の列（フィールド）の参照 | `42501` で拒否 | 文書から除去され、存在しないフィールドとして扱われる | PostgreSQL と同じく `42501` |
| 全列の取得（`SELECT *`／`_source` 全体） | 全列に権限が無ければ拒否 | 許可フィールドだけに絞る | 拒否（絞らない） |
| 不許可の列を使った検索・並べ替え | 拒否 | 該当なし扱い | 拒否 |
| 判定の単位 | ロール（継承・PUBLIC） | ロールの索引権限 | ロール（テナント内の継承・PUBLIC） |

- 他 DB 全体の評価は [row-level-isolation-comparison.md](row-level-isolation-comparison.md) に置き、ここでは重複させない
- 出典: PostgreSQL 17 の `GRANT` のドキュメント、Elasticsearch の field level security の公開ドキュメント

## 5. Issue と決定の対応

| Issue | 担当 |
| ----- | ---- |
| #2083 | C1〜C5 の実装（束縛段の列集合・ゲートの一般化・HTTP の写像・暫定規則の置き換え・上限値） |
| #2084 | C5 を含む全経路の越境マトリクス試験 |

## 6. 対象外・未決事項

- ビュー・matview への列 `GRANT`（J10 の `0A000` を維持）
- DDL での `REFERENCES` 列権限の検査・MERGE・`GRANTED BY`
- 列のマスキング・除去、行の値に応じた列の可視性
- 列集合の件数の上限値（#2083 で決める）
- 親 ADR（role-privilege-model.md）は Proposed のままである。#1960 は close 済みで、Phase 6 の後続は D9 を前提に進んだ。
  本書の Proposed も同様に、後続の着手可否を許可として読まない

## 7. OWASP Top 10 の観点

| 観点 | 本 ADR での担保 |
| ---- | --------------- |
| アクセス制御の不備 | 判定点を `privilege_gate::check` の 1 本に保つ。判別できない形は全列への要求で fail-closed。列権限で RLS・テナント境界を緩めない（C3・C6） |
| 情報漏えい（存在オラクル） | `42501` は固定文言。他テナントの relation は列の解決に入らない。`42703` に候補を出さない。Describe／EXPLAIN は列メタデータの前に拒否（C5） |
| 不安全な設計（DoS） | 列集合の件数の上限を ACL 照会の前に検査する（C3） |
| インジェクション | ACL のキーには束縛済みの識別子だけを使い、未検証の文字列連結でキーを作らない |
| セキュリティ設定ミス | 未束縛セッションの素通し条件を変えない。暫定規則の置き換えで fail-open にしない（C3） |

## 8. Issue #2083 実装記録（列許可の判定を共通ゲートへ統合）

ステータスは Proposed のまま（オーナー承認は未取得）。以下は ADR の C1〜C5 を実装した結果の記録で、承認済みを意味しない。

### 実装した範囲

| 決定 | 実装 |
| ---- | ---- |
| C1 | `privilege_gate::columns`。`ColumnSpec`（`TableOnly`／`All`／`Any`／`Names`／`Scan`）を `ColumnDemand` へ解決し、`cover_bits` が列単位の付与で満たせるビットを返す。列で満たせるのは `SELECT`／`INSERT`／`UPDATE`／`REFERENCES` だけ。`DELETE`・`TRUNCATE` は常に表単位 |
| C3 | 判定点は `check` の 1 本のまま。`PrivilegeTarget::RelationColumns { required_bits, granted_bits, column_cover_bits }` を足した。ACL の照会点は `read::evaluate_in_txn` に一本化（読み取り・書き込み・COPY・分割実行のチャンク再判定が共有） |
| C3（ACL） | `catalog::privilege::relation_grants_in_txn` が表単位のビットと列単位のビット（relation ごとに 1 回の範囲走査）を同じスナップショット・同じ INHERIT 閉包・同じ PUBLIC 規則で返す。形（`RelationShape`）は列単位の判定が要るときだけ引く |
| C5 | 付与が 1 つも無いロールは列の解決結果によらず `42501`。付与を持つロールの存在しない列は従来どおり束縛段の `42703`。`42501` は既存の固定文言の variant だけを使う |
| 暫定規則の置き換え | 「列単位の付与だけでは表単位の要求を満たさない」を廃止。置き換えたテスト: `column_level_grant_satisfies_only_the_granted_columns`（select）・`column_level_grant_satisfies_only_the_listed_columns_of_copy`（copy） |

### 列集合の求め方（C3 からの逸脱）

ゲートは束縛の前に走るため、束縛の結果そのものは使えない。ADR は「トークン走査ではなく束縛の結果」としたが、これは精度
（列と別名を区別できない）が理由であり、上位近似として使う限り安全である。そこで、列集合は名前解決済みトークン列の識別子と
relation の列名（大文字小文字を区別しない）の照合による上位近似とした。同名の別名・関数名・他表の列まで要求に入ることは
あっても、取りこぼし（fail-open）は起きない。

- 精密化するのは、単一 relation の `SELECT`／集約／広域取得（`ReadRequirement::SelectPrecise`）と、`INSERT ... VALUES`・述語形／`id` 指定形の
  `UPDATE`・`DELETE`・`COPY` の列リストだけ。
- 実在の relation が複数になる文は、`Scan` を精密化せず `All` にする。JOIN・サブクエリ・CTE・集合演算・EXPLAIN・DECLARE・
  行ロック句・`INSERT ... SELECT`・`UPDATE ... FROM`・`DELETE ... USING`・データ変更 CTE・`ON CONFLICT` の `UPDATE`・`PARTITIONED` は全列。
- `*`・`t.*`（`count(*)` を除く）・`NATURAL`・`USING PLAN` は全列。列を参照しない形（`count(*)`・`SELECT 1 FROM t`）は「いずれかの列」。
- `UPDATE` の `SET` 左辺と `INSERT` の列リストは、書き込みの列として `UPDATE`／`INSERT` で判定し、読みの走査からは除く。
  右辺・`WHERE`・`RETURNING` が名指しする列は `SELECT` で判定する。位置を確定できない形は除かない（厳しい側）。

### 上限値

| 名前 | 値 | 導出 |
| ---- | -- | ---- |
| `MAX_COLUMN_DEMANDS` | 2048 | `relation::MAX_TABLE_REFS`（8）× `catalog::MAX_COLUMN_COUNT`（256）。1 文の列名の数（重複除去後） |

構文木が確定した列名（`INSERT` の列リスト等）の数は ACL の照会より前に検査し、`Scan` の集計は集計の途中で検査する。
超過は `54000`（固定文言 `too many column references`）。`All`／`Any`／`TableOnly` は列を数え上げないので数えない。

### PG より厳しい箇所

- 疑似列 `id`（従来方式の表の行キー）の参照は表単位でしか満たせない（`GRANT ... (id)` は `42703` で作れない）。
  `SELECT *`・`COPY ... FROM` の列リスト（`id` を必ず含む）も従来方式の表では表単位が要る。緩めるか（「いずれかの列」にする等）は
  オーナー判断の後続とする。
- ビュー・matview は relation 自身の表単位の権限だけで判定する（C2・J10。列付与は `0A000` のまま）。
- HTTP 表層の bound 読み取り・書き込みは、束縛前の入口で列を特定できないため、列で満たせるビットを全列への要求として判定する。
  束縛済み計画（`BoundScan` 等）から参照列を集める精密化は後続とする。
- 分割実行（`PARTITIONED`）は入口もチャンクごとの再判定も全列を要求する（途中の剥奪を含め同じ結論にする）。
- 自動コミットでは、ゲートの読み取りスナップショットと実行のスナップショットが別になる。ゲートと実行の間の同時
  `ALTER TABLE ADD COLUMN` で `*` の展開が変わりうる。既存の表単位ゲートの `REVOKE` 競合と同じ種類の制約で、明示 Tx は
  `BEGIN` 時のスナップショットを共有するので起きない。

### 結線した経路（#2084 の越境マトリクスが狙う対象）

通常 SQL（`parse_tokens_gated`）・拡張クエリ（Parse・Bind・Describe）・明示 Tx（`BEGIN` 時のスナップショット）・COPY（`begin_copy*`）・
HTTP の bound 読み取り／書き込み・分割実行 DML のチャンク再判定。いずれも既存の呼び出し点のまま、ACL の照会点で列を評価する。
