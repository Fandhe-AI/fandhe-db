# 行セキュリティポリシーとテナント境界の合成設計

- ステータス: Proposed（設計記録。後続 sub-issue の実装で Accepted 化する）
- 対応 Issue: #2273（親 #2272・Phase 6）。後続の実装は #2274〜#2281
- ビヘイビア ID ポインタ: RLS-13・RLS-12・RLS-1〜4・RLS-6〜10（`docs/spec/04-behavior/rls.md`）、
  ERR-4（`docs/spec/04-behavior/error-format.md`）、TASK-331（`docs/spec/05-tasks.md`）。
  契約本文は spec を正とし、本書へは転記しない
- 構成方針: 本書は公開 Issue #2272 の記述・PostgreSQL 17 の公開ドキュメントの挙動・本書独自の実装上の決定だけで構成する。
  spec が定める細目はポインタ表記に留め、独自の決定は「本書で確定」と明記する
- 関連コード: `crates/engine/src/policy.rs`（`PolicyContext`）・`crates/engine/src/rls.rs`
  （`ImplicitRlsHook`・`PrefilterIndex`・`SearchTimeFilter`・`RlsSafetyNet`）・`crates/engine/src/tenant.rs`
  （`insert_row`／`update_row`／`delete_row`・`upsert_typed_rows_impl`・`ConflictTargetNotVisible`）・
  `crates/engine/src/sql/{exec,scan,aggregate,pk_point,scalar_index,hnsw_cache,hnsw_hybrid,upsert_where,copy,explain,view_insert}.rs`・
  `crates/wire-server/src/http/query/{gate,op,scan,search,aggregate,insert,update,delete}.rs`
- 関連 doc: [rls-generalized-read-paths.md](rls-generalized-read-paths.md)・
  [privilege-check-paths.md](privilege-check-paths.md)・
  [hnsw-rls-cardinality-switch.md](hnsw-rls-cardinality-switch.md)・
  [hnsw-hybrid-iterative-scan.md](hnsw-hybrid-iterative-scan.md)・
  [scalar-index-mask-search.md](scalar-index-mask-search.md)・
  [visible-bitmap-cache.md](visible-bitmap-cache.md)・[sql-upsert.md](sql-upsert.md)・
  [wire-copy-protocol.md](wire-copy-protocol.md)・
  [explain-search-engine-exposure.md](explain-search-engine-exposure.md)。
  ロール・権限モデルの ADR は PR #2828（#1960。未マージのため番号で参照）

## 背景・目的・範囲

親 #2272 は、利用者が定義する行セキュリティポリシー（`CREATE／ALTER／DROP POLICY`、
`ENABLE／DISABLE／FORCE／NO FORCE ROW LEVEL SECURITY`、`PERMISSIVE`／`RESTRICTIVE`、`FOR`・`TO`・`USING`・`WITH CHECK`、
`BYPASSRLS`、`row_security`）を PostgreSQL 17 と同等に受け付けるツリーである。
現状は `CREATE POLICY` 等が未対応文として拒否され、行の制御は fandhe のテナント境界だけが担う。

後続 sub-issue が経路ごとに実装する前に、「テナント境界の内側で AND 合成し、狭めることしかできない」という
親の不変条件を、差し込み位置・キャッシュ・索引走査・評価順まで具体化して判断のぶれと漏えい経路の見落としを防ぐ。
本書は設計記録のみでコードを変えない。ポリシーのカタログ形（#2274）・個別 SQLSTATE の細目・試験網羅（#2281）は
担当 sub-issue へ委ねる。

## 論点 1: 評価順と差し込み位置

### 評価順（本書で確定）

テナント境界は 2 段に現れる。#2828 の「名前解決 → 権限ゲート → RLS 暗黙適用」と親の「テナント境界 → 権限検査 → ポリシー」を
次の順で統合する。

1. カタログ段のテナント境界: 名前解決は自テナントのカタログだけを対象にする。他テナントのポリシーは解決対象にならない
2. 権限検査: ロール・GRANT の共通ゲート（未実装の間は現行の `require_ddl_permission` 等）
3. 行段のテナント境界: `ImplicitRlsHook`／`PolicyContext::is_visible`。常に適用され、解除できない
4. 利用者ポリシー: 3 の内側で AND 合成する
5. 呼び出し元の `WHERE`・投影・ランキング

### PG 17 との対比

| 観点 | PG 17 | fandhe の合成規則 | 担当 |
| ---- | ----- | ----------------- | ---- |
| 行の可視性の基準 | ポリシーのみ（テナントの概念なし） | テナント境界が常に先。ポリシーは内側で AND され狭めるだけ | #2276・#2277 |
| 権限検査との順序 | 表権限を先に確認し、通った文にポリシーを適用 | 同じ。名前解決（自テナント）→ 権限 → 行段 | #2278 |
| 適用の起点 | 表に RLS が有効なとき文種ごとにポリシーを選ぶ | 同じ。ただし有効化状態とポリシーはテナントごとに独立 | #2274・#2275 |
| 実行器の数 | 単一 | SQL・NoSQL・COPY は同じ実行器・束縛済み計画へ到達し、第 2 の判定系を作らない | #2280 |

### 差し込み位置インベントリ

`rls-generalized-read-paths.md` の R1〜R6 を土台とする。symbol は実装時に origin/main と再照合する。

| 区分 | 経路（実装の位置） | ポリシーの適用 | 担当 |
| ---- | ------------------ | -------------- | ---- |
| 読み取り | 任意形状 SELECT（`sql/exec.rs`）・広域取得（`sql/scan.rs`）・PK 点取得（`sql/pk_point.rs`） | SELECT の USING | #2276 |
| 読み取り | 集計（`sql/aggregate.rs`。`VisibleBitmapCache` の高速経路を含む） | 適用表では高速経路の結果をそのまま返さない（論点 5） | #2276・#2279 |
| 読み取り | 結合・部分クエリ・CTE・ビュー・カーソル | 参照する表ごとにその表のポリシーを適用 | #2276 |
| 読み取り | `EXPLAIN`（`sql/explain.rs`） | 除外前の件数・候補数を露出しない | #2276・#2279 |
| 書き込み | `INSERT`（`tenant::insert_row*`） | WITH CHECK。`RETURNING` があれば新行に SELECT の USING | #2277 |
| 書き込み | `UPDATE`／`DELETE`（`tenant::update_row`／`delete_row`。`UPDATE … FROM`・`DELETE … USING`・データ変更 CTE・分割実行 DML を含む） | 対象行の選択に USING、更新後の行に WITH CHECK | #2276・#2277 |
| 書き込み | `INSERT … SELECT`・ビュー経由（`sql/view_insert.rs`）・トリガ経由の書き込み | 最終的に書く表のポリシーを適用 | #2277 |
| 書き込み | `ON CONFLICT`（`sql/upsert_where.rs`・`tenant::upsert_typed_rows_impl`） | 後述「経路別の書き込み規則」 | #2277 |
| COPY | `COPY … FROM`（`commit_copy_in`）／`COPY … TO`（`run_scan_plan`） | FROM は INSERT と同じ WITH CHECK、TO は SELECT と同じ USING | #2280 |
| 検索 | kNN 厳密走査（`PrefilterIndex`・`SearchTimeFilter`） | 論点 5 のマスク合成 | #2279 |
| 検索 | HNSW（`sql/hnsw_cache.rs`）・ハイブリッド（`sql/hnsw_hybrid.rs`）・二次索引（`sql/scalar_index.rs`）・`RlsSafetyNet` | 論点 5 のマスク合成 | #2279 |
| NoSQL | `http/query/{scan,search,aggregate,insert,update,delete}.rs`（upsert を含む） | SQL と同じ実行器へ束縛済み計画で渡す既存設計を維持 | #2280 |

## 論点 2: PERMISSIVE・RESTRICTIVE・既定拒否

| 観点 | PG 17 | fandhe の合成規則 | 担当 |
| ---- | ----- | ----------------- | ---- |
| 適用ポリシーの選択 | 文種（`FOR`）と現在のロール（`TO`）で選ぶ | 同じ | #2276・#2278 |
| 合成 | PERMISSIVE は OR、RESTRICTIVE は AND | `テナント境界 AND (PERMISSIVE_1 OR … OR PERMISSIVE_n) AND RESTRICTIVE_1 AND …` | #2276 |
| 既定拒否 | 有効化済みで適用される PERMISSIVE が無ければ全行拒否 | 同じ。読み取り・`UPDATE`・`DELETE` は 0 行、`INSERT` は `42501`（親 #2272 の非対称をそのまま記録） | #2276・#2277 |
| NULL の扱い | 式が NULL なら偽扱い | 同じ | #2276 |
| 有効化・定義の範囲 | クラスタ内の表ごと | テナントごとに独立。共有カタログの表でも A の定義は B に作用しない | #2274・#2275 |

## 論点 3: BYPASSRLS・所有者・FORCE・row_security が外す範囲

いずれも外れるのは利用者ポリシーだけで、テナント境界は外れない（親 #2272 の不変条件）。

| 条件 | PG 17 | fandhe（外れるもの） | 担当 |
| ---- | ----- | -------------------- | ---- |
| `BYPASSRLS` ロール | ポリシーを適用しない | 利用者ポリシーのみ。テナント境界は適用 | #2278 |
| 所有者（`FORCE` なし） | ポリシーを適用しない | 利用者ポリシーのみ | #2278 |
| `FORCE ROW LEVEL SECURITY` | 所有者にもポリシーを適用 | 所有者にも利用者ポリシーを適用 | #2275 |
| `NO FORCE` | 所有者除外を戻す | 同じ | #2275 |
| `DISABLE` | ポリシーが残っていても適用しない | 利用者ポリシーのみ停止 | #2275 |
| `row_security = off` | 適用されるはずの文をエラーで拒否（黙って素通りにしない） | 同じく `42501` で拒否。利用者ポリシーの素通りにも使えない | #2278 |

- `BYPASSRLS` の指定・変更の規則と初期ロールの保持は親 #2272 のとおり（RLS-12・RLS-13 はポインタ）。`SUPERUSER` 相当は作らない
- 現在ロール・所有者・`row_security` の判定材料は `SessionState` 側（#2828 D4）から取り、`PolicyContext` には入れない（論点 5 の判断 A）

## 論点 4: 評価コンテキストと存在秘匿

| 観点 | PG 17 | fandhe の合成規則 | 担当 |
| ---- | ----- | ----------------- | ---- |
| `current_user` | 現在のロール（`SET ROLE` 後はその値） | 同じ。値は認証済み `SessionState` から取る | #2278 |
| 式内の部分クエリ・関数 | 呼び出し元の権限で評価。参照先表にもその表のポリシーを適用 | 呼び出し元のテナント境界内で評価。`SECURITY DEFINER` でもテナントは切り替わらない（現状は `SECURITY DEFINER` 自体を拒否） | #2276 |
| 評価の順序 | security barrier と leakproof により、ポリシー述語を呼び出し元の述語より先に評価 | 本書で確定: テナント境界・ポリシー述語を、エラーを起こしうる呼び出し元の式（CAST・UDF 等）より先に評価する。索引による候補削減は先行してよいが、最終判定はポリシー通過後 | #2276 |
| ポリシーの再帰 | 検出してエラー | 同じく検出して拒否し、評価深さに上限を置く（DoS 対策。#2276 で「入れ子なし（深さ 1）」と確定） | #2276 |
| 除外行の見え方 | 存在しない行と同じ | 同じ。0 行・0 件・`RETURNING` に出ない・`UPDATE … SET` の参照に出ない | #2276・#2277 |
| `WITH CHECK` 違反 | `42501` | `42501`。文言は表・ポリシー名のみで行の値を含めない | #2277 |
| 一意性・参照整合性・`ON CONFLICT` 判定 | テナント（PG ではクラスタ）内の全行で判定。ポリシーで隠れた行の存在が推知されうる | テナント内の全行で判定（同じ）。他テナントの行は従来どおり判定に含めない | #2277 |
| `EXPLAIN`・統計 | 実行計画に推定件数が出る | 除外前の件数・候補数を露出しない（`explain-search-engine-exposure.md` の非露出方針に合わせる） | #2279 |

## 論点 5: 索引走査との合成

| 観点 | PG 17 | fandhe の合成規則 | 担当 |
| ---- | ----- | ----------------- | ---- |
| ポリシー述語の位置 | 述語を表の制約としてプランに組み込む。索引走査は述語で絞られる | 索引の候補にテナント可視スナップショットとポリシーの 2 段のマスクを掛け、ランキングの前に適用 | #2279 |
| 件数・順位の漏えい | LIMIT は述語通過後の行で数える | 除外行が上位 k の枠を消費しない。件数・順位・スコアから存在を推測できない | #2279 |

### 判断 A（本書で確定）: `PolicyContext` はテナント・可視性だけに保つ

`PrefilterIndex`・`SqlArenaCache`・`VisibleBitmapCache`・`HnswIndexCache` は、`PolicyContext` の等値とテーブル世代を
キャッシュ照合の契約に使っている。ロール・ポリシー版・`row_security` を混ぜるとこの契約が崩れるため、
`PolicyContext` には入れない（#2828 D4 と同じ理由）。

### 判断 B（本書で確定）: 利用者ポリシーは第 2 のマスクとして合成する

既存の id マスク機構（`SearchProvider::search_subset`・HNSW の `Subset`／`NodeMask`・
[scalar-index-mask-search.md](scalar-index-mask-search.md)・
[hnsw-rls-cardinality-switch.md](hnsw-rls-cardinality-switch.md)）を再利用し、ランキング（top-k）より前に適用する。
第 2 の判定系は作らない。

### 判断 C（本書で確定）: マスクは初期実装では問い合わせごとに作る

キャッシュする場合のキーは「ポリシーカタログ世代・有効ロール・`row_security`・除外判定結果」を含め、
式が決定的（部分クエリ・`current_user`・揮発関数を含まない）な場合に限る。キー不一致・判定不能は再計算する（fail-closed）。
キャッシュ化は #2279 以降の任意課題とする。

### 個別規則

- 集計の高速経路（`VisibleBitmapCache` の COUNT 等）・スカラー索引集計・PK 点取得は、ポリシー適用表では
  高速経路の結果をそのまま返さず、ポリシーマスク後の値を返す。適用表かの判定は束縛時に行う
- ハイブリッド検索の密側の再取得ループと HNSW の縮退判定は、ポリシーマスク後の候補数で行う。
  除外行数が縮退・ラウンド数に現れないようにし、これらは従来どおり非露出とする
- ハイブリッド検索の疎側（`SparseIndex`）は、候補へのマスクだけでは存在秘匿を保証できない。
  `SparseIndex` は可視集合から文書数・df・平均文書長を算出するため、`PolicyContext` を変えずに候補へ
  第 2 マスクを掛けると、ポリシーで隠れた文書が BM25 統計に残り、可視文書のスコア・順位が変わる。
  そこで #2279 の設計に `SparseIndex::search_within` を含め、候補だけでなく BM25 統計
  （文書数・df・平均文書長）もテナント可視集合とポリシーマスクの積集合から算出する。
  統計は top-k の前に確定させ、除外行がスコア・順位に影響しない（スコアから存在を推測できない）ことを保つ。
  積集合を作れない・判定不能な場合は疎側の結果を返さず拒否する（fail-closed）
- `RlsSafetyNet` は第 2 層の防御としてテナント境界の再判定を維持する。ポリシーの再判定を足すかは #2279 で決める
  （足す場合も `is_visible` の単一照合パスは変えない）

## 経路別の書き込み規則

| 文 | PG 17 | fandhe |
| -- | ----- | ------ |
| `UPDATE` | 対象行の選択に USING（`WHERE`／`RETURNING` が列を読めば SELECT のポリシーも）、更新後の行に WITH CHECK（無ければ USING） | 同じ。テナント境界の判定が先 |
| `DELETE` | DELETE の USING（と必要なら SELECT のポリシー） | 同じ |
| `INSERT` | WITH CHECK。`RETURNING` があれば新行に SELECT のポリシー | 同じ |
| `ON CONFLICT DO UPDATE` | 既存行が UPDATE の USING（と SELECT のポリシー）を満たさなければエラー。更新後の行は WITH CHECK | 2 層を区別する。テナント境界外・不可視の衝突は現行の `ConflictTargetNotVisible`（`42501`・存在情報を出さない既存契約）を維持し、テナント内でポリシーにより隠れた衝突行は PG 17 の挙動に揃える。判定順はテナント境界 → ポリシー |
| BEFORE トリガとの順序 | BEFORE トリガ後の行に WITH CHECK | 同じ |
| 失敗時の原子性 | 文全体が失敗し何も書かない | 同じ。ただし利用者が明示する非原子の分割実行 DML（`PARTITIONED`）はチャンク単位で、違反したチャンクは何も書かずにジョブを止め、確定済みのチャンクは残る（`CHECK` 違反と同じ契約。#2277） |

## エラー契約と HTTP 射影

- 新しい `wire_code` は設けない。`42501` は既存の分類で表し、HTTP 射影は `http/status.rs` の既存写像に従う
- NoSQL（HTTP）からのポリシー管理は `http/query/gate.rs`・`op.rs` の `classify_op` 許可リストの外として扱い、
  既存経路どおり `0A000` で拒否する
- DDL の SQLSTATE（`42501`・`42710`・`42704`・`42703`・`42804`）は PG 17 と同じとし、詳細は ERR-4・RLS-13 のポインタに委ねる

## 親 #2272 の合成規則との対応

| 親の箇条（要約） | 本書の節 |
| ---------------- | -------- |
| テナント境界が先で、ポリシーは内側の AND。狭めるだけ | 論点 1（評価順）・論点 2 |
| 初期ロールは `BYPASSRLS`。指定・変更できるのは同一テナントの権限を持つロールだけ | 論点 3 |
| `BYPASSRLS`・所有者除外・`NO FORCE`・`row_security = off` が外すのは利用者ポリシーだけ | 論点 3 |
| 除外行は存在しない扱いで、件数・`RETURNING`・検索順位から漏れない | 論点 4・論点 5 |
| 一意性・参照整合性・`ON CONFLICT` はテナント内の全行で判定。他テナントは含めない | 論点 4・経路別の書き込み規則 |
| `WITH CHECK` 違反は `42501`。エラーは書こうとした行の情報だけ | 論点 4・エラー契約 |
| 式内の部分クエリ・関数も呼び出し元のテナント境界内で評価。`SECURITY DEFINER` でも切り替わらない | 論点 4 |

## 有効化状態（#2275）

`ALTER TABLE ... ENABLE／DISABLE／FORCE／NO FORCE ROW LEVEL SECURITY`（RLS-13 のポインタ）の状態の保存形と判定式。
本 Issue が入れるのは構文・DDL の実行・永続化・ライフサイクル追随・判定 API だけで、行の見え方への適用は #2276 以降。

- **保存形**: redb 表 `table_row_security`。キーは `(表キー, tenant_id)`、値は `[版, flags]` の 2 バイト（bit0 = enabled、
  bit1 = forced、他は 0 必須）。最初の書き込みまで表を作らず、両ビットとも false なら行を消す（行が無い＝既定）。
  decode は長さ・版・予約ビットを検証し、不正は固定文言の `CorruptSchema`（`XX000`）で断片を出さない
- **2 ビットの独立**: PG の `relrowsecurity`・`relforcerowsecurity` と同じ。各アクションは自分のビットだけを変える
  （`ENABLE`／`DISABLE` は enabled、`FORCE`／`NO FORCE` は forced）。FORCE だけで ENABLE していなければ誰にも適用しない
- **判定式**: `RowSecurityState::user_policies_apply(is_owner) = enabled && (!is_owner || forced)`。`BYPASSRLS`・
  `row_security` は #2278 が前段に足す。`is_owner` は引数で受け、所有者の根拠は #2278 が決める
- **テナント独立**: 状態はテナントごと。全 API が `tenant_id` を必須に取り、範囲走査でも突き合わせる。
  テナント境界（`PolicyContext`・`ImplicitRlsHook`）には触れない（判断 A）
- **再指定**: 状態が変わらない再指定は成功の no-op（カタログも表の世代も変えない）。変わったときは世代を進める
- **ライフサイクル**: `DROP TABLE`（`DROP SCHEMA ... CASCADE` 経由を含む）で全テナント分を削除し、`RENAME TO` で
  新しい表キーへ移す。`SET SCHEMA` は表キーが変わらないので追随不要
- **所有者の代替**: DDL を実行できる主体は #2274 と同じく DDL 実行権限ゲート（カタログ照会より前）で判定する。
  権限の無い主体には対象の有無を問わず `42501`
- **PG との差**: (1) `ENABLE ..., FORCE ...` のように RLS アクションを含む複数アクションの `ALTER TABLE` は `0A000`
  （PG は受理）。(2) 明示トランザクション内の DDL は既存どおり `0A000`。(3) 一時表が同名の永続表を隠しているとき、
  永続表の状態を黙って変えず `0A000`（一時表の行セキュリティは未対応）。(4) NoSQL の `alter_table` op は閉じた
  JSON スキーマで本 DDL に到達しない（#2280）
- **暫定状態**: #2276 で行の見え方へ適用された（下の「USING の適用（#2276）」節）。所有者の扱いは #2278 が入るまで暫定で、
  常に「所有者ではない」として適用する（ENABLE 済みならすべてのセッションにポリシーが適用され、FORCE の有無で結果は
  変わらない）。行の見え方の PG 17 照合（4 状態 x 所有者/非所有者）のうち所有者側は #2278 へ申し送る

## sub-issue への申し送りと未決事項

| Issue | 本書で担当する節・判断 |
| ----- | ---------------------- |
| #2274 | カタログとテナントごとの独立（論点 2）・名前解決（論点 1 の評価順 1） |
| #2275 | 有効化・`FORCE`・`DISABLE`（論点 3） |
| #2276 | USING・合成・評価順序・再帰上限（論点 1・2・4） |
| #2277 | WITH CHECK・`ON CONFLICT`（経路別の書き込み規則） |
| #2278 | ロール・`BYPASSRLS`・`row_security`（論点 3）・判断 A |
| #2279 | 索引走査・HNSW・ハイブリッド（論点 5）・判断 B・C |
| #2280 | NoSQL・COPY（論点 1 のインベントリ・エラー契約） |
| #2281 | 非漏えい試験（論点 4・5 の各規則） |

未決事項: マスクのキャッシュ化（判断 C）、`RlsSafetyNet` へのポリシー再判定の追加、PR #2828 のマージ後の相互リンクの追従。
（ポリシー再帰の深さ上限は #2276 で「入れ子なし（深さ 1）」と確定した。下の「USING の適用（#2276）」節の判断 6）

## カタログ形（#2274）

RLS-13 のポインタ。ポリシーの定義だけを持ち、読み書きの経路への適用（USING・WITH CHECK の評価と合成）は
Issue #2276・#2277 の担当のため、本節の導入後も行の可視性は変わらない（テナント境界は従来どおり）。

### 保存とキー

- 専用表 `policies`（キー `(table_key, tenant_id, policy_name)`、値は版付き blob）と採番表 `policy_id_counter`。どちらも
  最初の書き込みまで作らない（遅延作成）。`catalog::trigger` と同じ型で、表キーを先頭に置くので表の DROP・RENAME は
  全テナント分を前方一致の範囲走査だけで処理する
- 全 API が `tenant_id` を必須引数に取り、範囲走査でも突き合わせる。他テナントの定義は構造上引けない。他テナントにしか
  無い名前への `ALTER`／`DROP` は本当に無い場合と同じ `42704`（`IF EXISTS` は成功の no-op）
- blob は版 1。並びは `version / id / owner(予約・常に 0) / permissive / command / roles / using / check / 参照列`。
  未知の版・タグ・予約バイトが 0 以外・切断・末尾の余り・上限超過・キーと値の不整合は固定文言の `CorruptSchema`
  （断片・tenant_id・式本文を含めない。応答は `XX000`）
- 保存するのは束縛・往復一致を確認した正規化テキストだけ。読み出し時は untrusted として再解析する
- 上限: 1 表・1 テナントあたり 64 件（超過は `54000`）、式は 4096 バイト、参照列は列数上限まで。確保の前に検証する

### 判断

| 項目 | 内容 |
| ---- | ---- |
| 所有者の判定 | ロール・所有者は未実装のため、既存の DDL 権限ゲート `require_ddl_permission`（`42501`）で代替する。権限ゲートはカタログ照会より前に置き、対象の有無を問わず同じ応答にする（[privilege-check-paths.md](privilege-check-paths.md)）。blob には予約バイトだけを置き、判定は持たない |
| `TO` 句 | `PUBLIC`（既定）だけを受理する。名前付きロール・`CURRENT_USER`・`SESSION_USER` は #2278 が入るまで `0A000`（存在を確かめられない名前を保存すると fail-open になるため） |
| 式の受理範囲 | `CHECK` 本体と同じ文法・`WHERE` と同じ束縛。存在しない列は `42703`、真偽値でない式（非真偽値列・リテラル・算術）は `42804`、`tenant_id`・`visibility`・`visible()` は `42601`。決定性（`42P17`）は要求しない |
| `FOR` と句 | `SELECT`／`DELETE` の `WITH CHECK` と `INSERT` の `USING` は `42601`。`ALTER POLICY` で既存の `FOR` と矛盾する句を足す形は実行段で同じ `42601` |
| `ALTER POLICY` | `RENAME TO` は他の句と併用できない（`42601`）。句が 1 つも無い形は PG 17 と同じく受理する（対象の存在だけ確かめる）。指定しない句は保持し、式は同じ txn のスキーマで再束縛して参照列を求め直す |
| 句の順序 | PG 17 と同じ `AS`→`FOR`→`TO`→`USING`→`WITH CHECK`。順序違い・重複は `42601` |

### ライフサイクル追随（同じ write txn）

- `DROP TABLE`・`DROP SCHEMA ... CASCADE` の表削除は全テナント分のポリシーを消す（同名再作成で復活しない）。
  `ALTER TABLE RENAME` は新しい表キーへ移す。`SET SCHEMA` は表キーを変えないため追随は不要
- `DROP COLUMN` は、いずれかのテナントのポリシーが参照する列なら `2BP01`（RESTRICT・CASCADE とも。ポリシー名・テナントを
  応答に含めない）。`RENAME COLUMN`・`ALTER COLUMN TYPE` は暫定で `0A000`（trigger の `UPDATE OF` と同じガード）
- 対象表の世代（`bump_table_generation_in_txn`）を作成・変更・削除のたびに進める

### PG 17 との差

| 項目 | PG 17 | 本実装 |
| ---- | ----- | ------ |
| `DROP POLICY IF EXISTS` の NOTICE | 出る | 出さない（wire に NOTICE の仕組みが無い。`DROP TRIGGER IF EXISTS` と同じ） |
| 明示トランザクション内の DDL | 受理 | `0A000`（権限なしは `42501`）。ポリシー DDL は独立した write txn で commit され ROLLBACK で取り消せない |
| 式中の部分クエリ・`current_user` | 受理 | 拒否（評価側の再帰・認証情報の合成は #2276・#2278 の担当） |
| `RENAME COLUMN` の追随 | ポリシーの式を自動で書き換える | 暫定ガードで拒否（自動追随は後続の課題） |
| `DROP COLUMN CASCADE` | 依存ポリシーを連鎖削除 | `2BP01` で拒否 |

## USING の適用（#2276）

RLS-13 のポインタ。読み取り（SELECT）と UPDATE・DELETE の対象行選択へ、利用者ポリシーの `USING` を適用する。
実装は `sql::row_policy`（束縛・合成・評価段）と、各実行器・`tenant` の DML 列挙段への差し込み。
`WITH CHECK`（#2277）・ロール／所有者／`row_security`（#2278）・索引走査／検索（#2279）・NoSQL／COPY の網羅（#2280）は担当外。

### 判断

1. **所有者の扱い（暫定）**: `RowSecurityState::user_policies_apply(is_owner)` に常に `is_owner = false` を渡す。ENABLE 済みの
   表はすべてのセッションにポリシーが適用され、現時点では FORCE の有無で結果は変わらない。`allow_ddl()` 済みのセッションを
   所有者とみなさない（照合できる範囲が狭まり、PG 17 との比較が成り立たなくなる）。狭める側の判断なので fail-closed。
   ロールは `TO PUBLIC` だけが受理されるため、ロールによる選別は「全ポリシーが対象」と同じ。#2278 がこの前段へ
   `BYPASSRLS`・所有者除外・`row_security` を足す
2. **ポリシーは文の `WHERE` の AST へ混ぜず、独立した評価段として持つ**: `BoundConjunction` は
   `metadata_filters → expr_filters → or_groups` の順で評価し宣言順の保証がないため、混ぜると利用者の式（CAST・UDF・0 除算）が
   ポリシーで隠れた行に先に走り、エラーの有無から存在が分かる（論点 4 違反）。台帳の内容ハッシュ（RECOVER-11）は文の AST から
   出すため、混ぜるとカタログ状態に依存してしまう。各走査は `テナント境界 → キーとヘッダのテナント整合 → 行の復号 →
   ポリシー段 → 利用者の WHERE` の順で評価する。式の評価は `sql::where_tree` の既存評価器を再利用する
3. **合成規則**: PG 17 と同じ。文ごとにコマンドの集合を選ぶ（SELECT は `FOR SELECT`＋`FOR ALL`、UPDATE は `FOR UPDATE`＋`FOR ALL`、
   DELETE は `FOR DELETE`＋`FOR ALL`）。UPDATE・DELETE は既存行を読む（`WHERE` が列を参照する・`RETURNING` がある・UPDATE の
   `SET` の右辺が式）ときに限り、SELECT の集合も別の層として AND する。各層は `(PERMISSIVE の OR) AND (RESTRICTIVE の AND)`、
   適用される PERMISSIVE が無ければ全行拒否、式の NULL は偽。式を省略した PERMISSIVE は TRUE の分岐として OR に加わる（書き込み側 `policy_eval` と同じ。RESTRICTIVE の TRUE は AND に何も足さない）
4. **解決は行を読むのと同じスナップショットで行う**: `resolve_row_policy` が有効化状態と定義を `ReadSource`（読み取り txn・
   write txn・明示トランザクションの窓口）から引き、保存テキストを untrusted として再束縛する（`bind_where_predicates`）。
   書き込みは列挙と同じ write txn・同じスキーマの中で解決する（TOCTOU を作らない）。再束縛の失敗・破損は固定文言の `XX000`
   （式本文・名前を含めない）で拒否し、ポリシーを無視して素通しにしない。一時表・導出表・CTE の作業表は対象外
5. **適用できない経路は適用表に限り `0A000` で拒否する（固定文言・表名やポリシー名を含めない）**:
   ランキング付き検索（距離順・ハイブリッド・`USING PLAN`）、`VectorCore::search`・辞書構築、`rls.rs` の
   `PrefilterSnapshot::build`・`SearchTimeFilter::search`、`tenant::visible_rows`、生の全列置換・id 指定削除（Rust API の
   `EngineCore::update_row`・`delete_row`）。これらは `PolicyContext` をキーにしたアリーナ・HNSW・スカラー索引・疎索引・
   各キャッシュに依存し、ポリシーを反映できない（#2279）。集計は適用表に限り、可視 id ビットマップ・アリーナ・スカラー索引の
   高速経路を使わずポリシー段を通る通常の走査へ倒す。点取得（`get_row`）は取得した 1 行へポリシーを評価し、通らなければ
   不在と同じ `NotFound`。適用表がこれらの経路を黙って通ることは無い。分類は構造テスト
   `tests/table_rls/rls13_policy_read_path_coverage.rs` が固定する
6. **ポリシーの再帰上限は「入れ子なし（深さ 1）」と確定する**: #2274 の段階でポリシー式中の部分クエリ・UDF・`current_user` は
   DDL で拒否されるため、ポリシー展開が他の関係を参照して再帰することは構造上起きない。再束縛で部分クエリ系の葉や
   `visible()` が出たら破損として固定文言の `XX000` にする。再帰の検出機構は作らず、部分クエリを受理する将来の Issue で
   実体化する

### 経路の分類

| 経路 | ポリシー | 備考 |
| ---- | -------- | ---- |
| 広域取得（順序なし・経路 A・B・PK 点取得・GIN 候補） | 適用 | `sql::scan::with_visible_row` が全経路で共有。復号マスク・段階をポリシーの参照列へ引き上げる |
| 集計・`GROUP BY`・ウィンドウ関数 | 適用 | `ReferencedColumns::include_policy`。適用表は高速経路・索引・アリーナを使わない |
| 結合・部分クエリ・CTE・ビュー・カーソル・集合演算・`COPY TO` | 適用 | 参照する表ごとに広域取得・集計の実行器を通るため自動で継承する |
| 述語形 UPDATE／DELETE・結合形・分割実行 | 適用 | `tenant::enumerate_dml_candidates_with` と `partitioned_dml::scan_chunk`（所有かつ可視の内側・利用者の述語より前）。総走査数は従来どおりテナント所有行で数える |
| id 形 UPDATE／DELETE（`RETURNING` 版・トリガ表を含む） | 適用 | 所有・可視と確認した行だけ本体を復号して評価し、通らなければ 0 件（不在と同じ） |
| ランキング付き検索・Rust API の検索・辞書構築 | 拒否（`0A000`） | #2279 |
| `INSERT`・`ON CONFLICT`・`INSERT ... SELECT` の書き込み側 | 対象外 | #2277。`ON CONFLICT DO UPDATE` はポリシーで隠れた行も従来どおり更新する |
| 制約検査（一意性・参照整合性・CHECK）・索引保守・DDL の書き換え・採番 | 適用しない | テナント内の全行で判定する（親 #2272 の規則） |

### 存在の非漏えい

- 除かれた行は 0 件・`RETURNING` に出ない・エラーにならない。ポリシー段が利用者の式より先に走るため、隠れた行にだけ 0 除算を
  起こす値を置いても利用者の式はその行を評価しない（`rls13_policy_using.rs` の `n = 0` カナリアで固定）
- 件数の漏えい: `max_affected_rows` はポリシーを通過した行だけを数える。走査上限（`MAX_SCANNED_ROWS`）はテナント所有行で数え、
  ポリシーで落ちた行も数える（他テナントの行数には依存しない）。テナント内で粗い行数の信号が出る点は、一意制約と同じ扱い
- 拒否・破損の文言は固定で、表名・ポリシー名・式本文を含めない

### PG 17 との差・既知の制約

- `reads_existing` の判定は、`WHERE` に述語（宣言的フィルタ・式・OR 群）がある・`RETURNING` がある・`SET` に式がある、のいずれかで真。
  `WHERE true` のような列を参照しない定数述語も「読む」側に倒す（PG 17 は読まない側。狭める方向なので fail-closed）。
  id 形・結合形・分割実行は常に「読む」
- `UPDATE ... SET` の後の行に対する `WITH CHECK`／SELECT の `USING`（`42501`）は #2277
- `SELECT ... FOR UPDATE／SHARE`（行ロック付き SELECT）の実行は現状 `0A000`（#2230）。実装時は SELECT と UPDATE の両方の USING を使う
- `get_row` は行・スキーマ・有効化状態・ポリシーを同一の read txn（同一スナップショット）から取得して評価する（行の取得とポリシー評価の間に別の変更が割り込まない）
- Rust API の拒否は `CoreError::Catalog(Invalid)`／`RlsError::Arena(Catalog(Invalid))`／`TenantError::Catalog(Invalid)` で表す
  （`CoreError` 等に `0A000` 相当の variant が無いため。SQL 表層では `0A000`）
- 一括検索（`batch_search`・`batch_fallback`・`gpu_batch`）は呼び出し元が渡す配列を扱いテーブルを読まないため対象外。
  表と接続する時点で #2279 が分類する
- `tenant::replace_typed_rows_by_text_key`（再取り込み）・`update_row_unchecked` の内部経路は #2277 の書き込み側の担当

## セキュリティ観点（OWASP Top 10）

| 観点 | 本設計での扱い |
| ---- | -------------- |
| アクセス制御の不備 | テナント境界は常に先に適用し、ポリシー・`BYPASSRLS`・所有者・`FORCE`・`row_security` のいずれも外すのは利用者ポリシーだけ。`PolicyContext` を緩める経路を作らない |
| 情報漏えい（存在オラクル） | 除外行は件数・`RETURNING`・順位・スコア・集計の高速経路・`EXPLAIN`・HNSW の縮退判定・エラー文言から判別できない。他テナントのポリシーは名前解決の対象外 |
| 不安全な設計 | キャッシュ照合契約を壊さない（判断 A）。キャッシュ化は決定的な式に限り不一致は再計算（fail-closed）。再帰の検出と深さ上限で DoS を防ぐ |
| インジェクション | ポリシー式は既存の式束縛器・評価器で扱い、SQL 文字列を組み立てない。NoSQL は束縛済み計画で同じ実行器へ渡す |
| 認証の不備 | `current_user` 等は認証済み `SessionState` だけから取り、クライアント申告のテナント・ロールを使わない |
| 秘密情報・設定 | 実ユーザー名・資格情報を載せない。private spec の本文は転記せずポインタのみ |
| 脆弱な依存 | 依存追加なし |

## WITH CHECK の適用（#2277）

`INSERT`・`UPDATE`・`INSERT ... ON CONFLICT DO UPDATE` で書いた行に利用者定義ポリシーの `WITH CHECK` を適用する
（RLS-13 のポインタ）。実装は `sql::policy_eval`（選択・合成・評価）と、書き込みの単一の検査点
`constraint::enforce_row_constraints_in_txn` の新しい段。

- **種別は明示引数**: 検査点は `RowPolicyWrite`（`Insert`・`Update`・`Upsert { updated_ids }`・`Internal`）を必須引数に取る。
  thread-local にしないので、指定の付け忘れはコンパイルエラーになり「検査なし」（fail-open）の経路ができない
- **合成**: `(PERMISSIVE の OR) AND (RESTRICTIVE の AND)`。式が NULL なら偽、該当する PERMISSIVE が無ければ既定拒否。
  `WITH CHECK` が無ければ `USING` で代用する。代用後も式が無い適用ポリシーは TRUE として合成する（PERMISSIVE は OR に TRUE が入る。「適用ポリシーなし」のときだけ既定拒否）。
  違反の報告は PERMISSIVE の合成を先に、続けて RESTRICTIVE を名前順に個別に評価する
- **段の順序**: 索引同期の前、つまり `CHECK`・UNIQUE・ビューの `WITH CHECK OPTION`・`FOREIGN KEY` より前に評価する
  （PG の RLS の検査が制約・索引挿入より前であることに合わせた。PG 17 のソースでの照合は未実施で、要確認）
- **ON CONFLICT**: (1) 提案行に INSERT 側の `WITH CHECK` を、衝突判定より前に全行へ評価する。(2) 既存行に UPDATE 側と
  SELECT 側の `USING` を、テナント境界（所有 ∩ 可視。外れは従来の `ConflictTargetNotVisible`）の後・`DO UPDATE ... WHERE` の
  前に評価する。(3) 更新後の行に UPDATE 側の `WITH CHECK` を検査点で評価する。(1) の PG 17 との順序は要確認
- **エラー**: `TenantWriteError::RowSecurityPolicyViolation`（`42501`。新しい `wire_code` は作らず HTTP 射影も既存の
  `42501` のまま）。文言は PG と同形で、含めるのは自テナントの表名とポリシー名（RESTRICTIVE 違反のときだけ）に限る。
  行の値・id・テナントは含めない
- **原子性**: 検査点で失敗すると write txn は commit されず、台帳・行・索引とも残らない。明示トランザクションでは
  既存の文失敗の扱いに乗る
- **暫定（#2278 まで）**: 所有者・`BYPASSRLS` の機構が無いので、全セッションを非所有者として扱う
  （`policy_eval::user_policies_apply_for_write` の 1 か所）。そのため `ENABLE` だけで INSERT ポリシーの無い表は
  全員の INSERT が `42501` になり、`FORCE`／`NO FORCE` の違いはまだ観測できない
- **連鎖書き込み**: `FOREIGN KEY` の参照アクションが子表へ書く行（`Internal`）には適用しない（暫定。PG の RI は
  ポリシーを実質的に掛けない。#2278 で再検討）
- **一時表**: 一時表への `ENABLE` は `0A000`（#2275）のため、一時表への書き込みは検査対象にならない
- **未対応（後続）**: `RETURNING` の新しい行への SELECT ポリシー、対象行の選択への `USING`（#2276）
- **PG との差**: NOT NULL・行キー衝突（`23505`）は検査点より前に検出されるため、RLS 違反と同時に起きたときのエラーが
  PG と異なりうる（他テナントへの漏えいはない）
