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
