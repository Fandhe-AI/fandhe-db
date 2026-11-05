# 固定テナント方式（テナント境界を外せない設計）の方針

- ステータス: Accepted（実装で確定済みの判断を方針として記録する。本書の追加でコードは変えない）
- 対応 Issue: #2076（親 #2075）。他 DB との比較表は #2077 が担当し、[row-level-isolation-comparison.md](row-level-isolation-comparison.md) に置く
- ビヘイビア ID ポインタ: RLS-6・RLS-8・RLS-10・RLS-12・RLS-13（`docs/spec/04-behavior/rls.md`）。
  契約本文は spec を正とし、本書へは転記しない
- 関連コード: `crates/engine/src/policy.rs`・`crates/engine/src/rls.rs`・`crates/engine/src/tenant.rs`・
  `crates/engine/src/tenant/partitioned_dml.rs`・`crates/engine/src/sql/row_policy.rs`・
  `crates/engine/src/sql/policy_eval.rs`・`crates/engine/src/sql/policy_applicability.rs`・
  `crates/wire-server/src/auth.rs`・`crates/wire-server/src/http/query/gate.rs`・
  `crates/wire-server/src/http/session/middleware.rs`
- 関連 doc: [row-security-policy-composition.md](row-security-policy-composition.md)・
  [row-level-isolation-comparison.md](row-level-isolation-comparison.md)・
  [rls-generalized-read-paths.md](rls-generalized-read-paths.md)・
  [role-privilege-model.md](role-privilege-model.md)・
  [privilege-check-paths.md](privilege-check-paths.md)・
  [hnsw-rls-cardinality-switch.md](hnsw-rls-cardinality-switch.md)・
  [partitioned-dml.md](partitioned-dml.md)・
  [explain-search-engine-exposure.md](explain-search-engine-exposure.md)・
  [schema-tenant-namespace.md](schema-tenant-namespace.md)

## 決定

- テナント境界は、サーバーが導出したテナント ID と可視性ラベルで決まる。全経路に暗黙に適用され、クライアントからは外せない
- テナント境界を外す管理者バイパス（`SUPERUSER` 相当のロール、テナント文脈を持たない接続、境界判定を飛ばす API）は採用しない
- 利用者定義ポリシー（`CREATE POLICY`・`PERMISSIVE`／`RESTRICTIVE`・`FOR` によるコマンド別ポリシー）は非採用ではない。
  テナント境界の内側で AND 合成する追加の制限として実装済み（親 #2272・RLS-13）で、見える行・書ける行を狭めることしかできない

## 境界の定義

- 読み取りは `PolicyContext::is_visible`、書き込みは `PolicyContext::is_owner` の単一照合パスで判定する。
  呼び出し側は独自にテナント文字列を比較しない（`policy.rs` のモジュール doc が根拠）
- 非対称に注意: 他テナントの `Public` 行は読めるが、書けない。`is_visible` は「許可された可視性集合に含まれる」かつ
  「`Public`、または行のテナントが自テナントと一致」で判定し、`is_owner` はテナント一致だけを見る。
  境界が守るのは「他テナントの `Private` 行は見えない」と「書き込みは自テナントだけ」の 2 点
- `ImplicitRlsHook` は `is_visible` へ委譲するだけで、第 2 の判定系を持たない
  （`crates/engine/src/rls.rs` の単体テスト `implicit_hook_delegates_to_policy_context_is_visible`）

## テナント ID はサーバーが導出する

- wire: 認証成功時に `auth::session_policy_context` で導出する。導出点はここ 1 か所で、`Public` と自テナントの `Private` を許す
- HTTP: テナントを指定するヘッダは `http/session/middleware.rs` の `reject_tenant_headers` が検出し、
  `http/query/gate.rs` が `42601` で拒否する。本文の `tenant_id` も未知のキーとして `42601` になる
- SQL: `UPDATE ... SET tenant_id`／`SET visibility` は束縛段で `42601`
  （`tests/sql_named/sql_update_predicate_bind.rs` の `bound_predicate_update_never_exposes_tenant_or_visibility_assignment`）。
  `SET id` も `42601`（`tests/sql_named/sql_update_single_row.rs` の `set_id_is_rejected_as_unsupported_syntax`）
- INSERT で行のテナントを指定する経路は無く、書き込みは接続のテナント文脈の `is_owner` を通る（`tenant.rs`）
- 結論: 自テナント以外の行を書けないこと（`WITH CHECK` 相当）は、ポリシー式に頼らず構造で成り立つ

## default-deny（fail-closed）

- 空のテナント ID（`EmptyTenantId`）と上限超過（`MAX_TENANT_ID_LEN` 超）は、`PolicyContext` の構築時に `PolicyError` で拒否する
- `PolicyContext::new` が許す可視性は既定で `Public` だけ。`Private` は `with_visibilities` で明示したときだけ許し、黙って昇格させない
- 利用者ポリシーが適用される表で、適用される PERMISSIVE が 0 件なら全行を拒否する
  （詳細は `row-security-policy-composition.md` を参照し、本書では重複させない）

## 経路ごとの単一照合パス

行番号は書かず、関数名・ファイル名で参照する（テストの束ね替えで古くならないように）。
テストのパスは `crates/engine/tests/` 配下（`wire-server` は明記）。

| 経路 | 境界判定の位置 | 根拠テスト |
| ---- | -------------- | ---------- |
| 検索（kNN 厳密・HNSW・ハイブリッド） | `rls::ImplicitRlsHook`・`PrefilterIndex`・`SearchTimeFilter`・`RlsSafetyNet` が `is_visible` へ集約 | `table_rls/rls_implicit.rs`・`rls_prefilter.rs`・`table_rls/rls_safety_net.rs`・`table_rls/rls_security.rs` |
| 読み取り全般（任意形状 SELECT・集計・PK 点取得） | `sql/exec.rs`・`sql/scan.rs`・`sql/aggregate.rs` の `is_visible` 呼び出し | `table_rls/rls_generalized.rs`・`table_rls/rls10_*_paths.rs`・`rls10_primary_key_paths.rs` |
| 更新・削除（id 形・述語形・データ変更 CTE） | `tenant.rs` の `is_owner`（と `is_visible`） | `table_rls/tenant_breach.rs`・`rls10_data_modifying_cte_paths.rs`・`rls10_write_constraint_paths.rs` |
| 分割実行 DML（`PARTITIONED`） | `tenant/partitioned_dml.rs` が各チャンクで原子経路と同じ判定を通す | `table_rls/partitioned_dml_sql.rs`・`wire-server/tests/http_nosql/nosql12_partitioned_dml.rs` |
| HTTP（NoSQL） | `http/query/gate.rs` が束縛済みの計画を SQL と同じ engine の実行器へ渡す（第 2 の判定系を作らない） | `wire-server/tests/http_nosql/nosql6_tenant_row_id_scope.rs`・`rls13_nosql_policy_nonleak.rs` |
| EXPLAIN | 出力がテナントに依存しない（除外前の件数を露出しない） | `sql_t40_up/sql75_explain_dml.rs` の `output_is_tenant_independent`・`sql_t01_39/sql27_explain_targets.rs` の `*_identical_across_tenants_*`・`wire-server/tests/http_nosql/nosql10_explain.rs` |
| 生の `Storage` API | 認可なしの内部 API で wire からは到達できない（[rls-generalized-read-paths.md](rls-generalized-read-paths.md) 参照） | — |

## 利用者ポリシーの合成点（RLS-13）

- 評価順は、テナント境界（`is_visible`）、利用者ポリシー（`sql::row_policy::BoundRowPolicy::passes`。書き込みは `sql::policy_eval`）、
  利用者の `WHERE` の順。ポリシーは `is_visible` の内側で AND されるだけで、外側へ広げる合成は無い
- `sql::policy_applicability::decide` の `Skip`（`BYPASSRLS`・所有者・`DISABLE`）で外れるのは利用者ポリシーだけ。
  `row_security = off` は、ポリシーが適用される文を `42501` で拒否する
- 根拠テスト: `table_rls/rls13_row_security.rs` の `tenant_boundary_is_unchanged_by_any_state`、
  `rls13_policy_roles_row_security.rs` の `rls13_row_security_off_never_widens_tenant_boundary`、
  `rls13_policy_tenant_nonleak.rs` の `bypassrls_grant_is_denied_identically`
- 詳細は [row-security-policy-composition.md](row-security-policy-composition.md) を参照し、本書では重複させない

## 管理者バイパスを採用しない理由と運用

- 理由: バイパス経路が 1 本でもあると、fail-closed の不変条件が経路単位で崩れる。誤設定や資格情報の漏えいが、そのまま全テナントの漏えいになる
- 現状: `SUPERUSER` 相当のロール属性は作らない。ロール属性 `BYPASSRLS` は現在 `42501` で拒否される
  （[role-privilege-model.md](role-privilege-model.md)）。将来解禁しても、外れるのは利用者ポリシーだけで、テナント境界は外れない
- 運用: 調査や保守もテナント文脈を持つ接続で行う。テナント境界を越えて読む専用の経路は提供しない

## 未対応の形

未対応の構文・経路は従来どおり fail-closed（`42601`・`0A000` 等）で拒否し、テナント境界を緩めない。
例: ポリシーを反映できない経路は、ポリシーが適用される表に限り `0A000` で拒否する（`row_policy::reject_if_policies_apply`）。

## 対象外

- 他 DB（PostgreSQL・Elasticsearch・Qdrant）との比較表: #2077（[row-level-isolation-comparison.md](row-level-isolation-comparison.md) で追加済み）
- 列レベルの制御: 未提供
- ロール属性 `BYPASSRLS` の解禁: #2278 の段 2
