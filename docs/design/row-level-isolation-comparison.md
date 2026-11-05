# 行レベル分離の他 DB 比較表

- ステータス: Accepted（実装済みの挙動と他 DB の公式ドキュメントの記述を並べた比較記録。本書の追加でコードは変えない）
- 対応 Issue: #2077（親 #2075）。方針は [fixed-tenant-boundary.md](fixed-tenant-boundary.md)（#2076）
- ビヘイビア ID ポインタ: RLS-6・RLS-8・RLS-9・RLS-10・RLS-12・RLS-13・SQL-40（`docs/spec/04-behavior/`）。
  契約本文は spec を正とし、本書へは転記しない
- 他 DB の記述の基準: 下記「参照」の公式ドキュメントを 2026-11-05 に確認した内容だけを書く。
  確認できなかった点は「公式ドキュメントで確認できず」と書き、記憶で補わない
- 関連コード: `crates/engine/src/policy.rs`・`crates/engine/src/rls.rs`・`crates/engine/src/sql/policy_eval.rs`・
  `crates/engine/src/sql/policy_applicability.rs`・`crates/engine/src/sql/privilege_gate/read.rs`
- 関連 doc: [fixed-tenant-boundary.md](fixed-tenant-boundary.md)・
  [row-security-policy-composition.md](row-security-policy-composition.md)（PostgreSQL 17 との論点別の対比はこちらが詳しい）・
  [hnsw-rls-cardinality-switch.md](hnsw-rls-cardinality-switch.md)・
  [hnsw-hybrid-iterative-scan.md](hnsw-hybrid-iterative-scan.md)・
  [role-privilege-model.md](role-privilege-model.md)

## 目的と読み方

fandhe の行レベル分離を PostgreSQL・Elasticsearch・Qdrant と同じ軸で並べ、
「同等以上と言える点」「同等の点」「未提供の点」を区別して公開する。過大表示を避けるため、
「同等以上」は fandhe 側にテストで固定した根拠がある項目に限る。

## 前提

- fandhe の分離は 2 層である。(a) サーバーが導出するテナント ID と可視性ラベルによる固定のテナント境界（外せない）、
  (b) その内側で AND 合成する利用者ポリシー（見える行・書ける行を狭めるだけ）
- 非対称: 他テナントの `Public` 行は読めるが書けない。境界が秘匿するのは他テナントの `Private` 行と、
  書き込みが自テナントに限られることである（[fixed-tenant-boundary.md](fixed-tenant-boundary.md) の「境界の定義」節）
- 比較の単位が違う。PostgreSQL は表単位のポリシー、Elasticsearch はロールの索引権限ごとの DLS／FLS、
  Qdrant は DB 側の行レベル分離機構ではなくペイロードフィルタ・シャードキーによるデータ配置の設計である

## 比較表

| 比較軸 | PostgreSQL 17 | Elasticsearch | Qdrant | fandhe |
| ------ | ------------- | ------------- | ------ | ------ |
| 分離の適用範囲 | 有効化した表に対する各コマンドへ適用 | ロールの索引権限に紐づく読み取りへ適用 | テナント条件は各リクエストのフィルタ。サーバーが暗黙には付けないと読める（公式ドキュメントに明示はなく、記述から読み取れる範囲） | 検索・読み取り・更新・分割 DML・HTTP・EXPLAIN の全経路に暗黙適用。クライアントから外せない（[経路ごとの単一照合パス](fixed-tenant-boundary.md)） |
| 既定拒否 | 有効化済みでポリシーが無ければ既定拒否 | `query` を省略した索引権限では DLS が無効（全文書が見える） | 公式ドキュメントで確認できず | 空・上限超過のテナント ID は構築時に拒否。利用者ポリシーが適用される表で PERMISSIVE が 0 件なら全行拒否。ただし他テナントの `Public` 行は読める |
| 書き込み時の検査 | `WITH CHECK`（省略時は `USING` を流用） | DLS は書き込み API に適用されない。DLS／FLS は読み取り専用アカウント向けで、更新 API は非対応 | 公式ドキュメントで確認できず | テナント ID はサーバーが導出するため境界側は構造で成立（`SET tenant_id` は `42601`）。利用者ポリシーは `WITH CHECK` を単一の検査点で評価 |
| 他テナントの存在の秘匿 | 一意・主キー・外部キーの検査はポリシーを迂回し、隠れた行の存在が推知されうる | 集計で、見えない文書にだけある語や件数を推知されうる | 公式ドキュメントで確認できず | 一意性・参照整合性・`ON CONFLICT` の判定に他テナントの行を含めず、応答の不変性をテストで固定 |
| テナント内（ポリシーで隠れた行）の存在の秘匿 | 上記のとおり推知されうる | 上記のとおり | 該当なし | 制約はテナント内の全行で判定するため推知されうる。**PostgreSQL と同等であり、それ以上とは言わない**。走査上限・HNSW 走査の時間差という粗い信号もテナント内に限られる |
| 評価順（不可視行で利用者の式が走らないか） | ポリシー式が利用者の条件より先に評価される。ただし `leakproof` 関数は先に評価されうる | 公式ドキュメントで確認できず | 該当なし | テナント境界、利用者ポリシー、利用者の `WHERE` の順。不可視行・隠れた行で UDF・0 除算が評価されないことをカナリアで固定 |
| フィルタ付き ANN の取りこぼし | 公式ドキュメント（行セキュリティ）に記述なし | kNN の `filter` の適用方式は参照したページで確認できず | テナント用ペイロード索引（`is_tenant`）で配置を最適化。取りこぼしの扱いは参照したページで確認できず | HNSW に候補マスクを掛け、可視比が低いか分断を検出したら厳密走査へ倒す。既定の SCALAR 事前フィルタ経路では結果不足が構造的に起きない（事後フィルタ反復は `hnsw.iterative_scan=off` や `max_scan_tuples` 到達時、HNSW 不適格な DISTANCE 先行経路は件数不足になりうる）。ハイブリッド密側は再取得ラウンドで補う |
| `BYPASSRLS` | スーパーユーザーと `BYPASSRLS` ロールは常にポリシーを外れる | 該当なし（`superuser` の挙動は参照ページで確認できず） | 該当なし | 判定機構は実装済みで、外れるのは利用者ポリシーだけ（テナント境界は外れない）。ロール属性 `BYPASSRLS` の付与は **未提供**（`42501`） |
| 所有者・`FORCE` | 所有者は既定でポリシーを外れ、`FORCE ROW LEVEL SECURITY` で適用 | 該当なし | 該当なし | 同じ判定順。外れるのは利用者ポリシーだけ |
| `row_security = off` | 素通りにせず、ポリシーで絞られる文はエラー | 該当なし | 該当なし | 同じく適用されるはずの文を `42501` で拒否 |
| 管理者バイパス | スーパーユーザーは全件を読める | 公式ドキュメントで確認できず | 管理用の API キーの権限範囲は参照ページで確認できず | **無い**。テナント境界を外すロール・接続・API を作らない |
| ロール別・コマンド別の条件 | `TO <ロール>`、コマンド別、PERMISSIVE／RESTRICTIVE | ロールごとに DLS クエリ。複数ロールは OR で合成 | 公式ドキュメントで確認できず | コマンド別（`FOR`）・PERMISSIVE／RESTRICTIVE は提供。ロール別（`TO <ロール>`）は **未提供**（`PUBLIC` のみ。それ以外は `0A000`） |
| 列レベル制御 | 列権限（`GRANT` の列指定）が別にある | FLS（`grant`／`except` による除去） | 公式ドキュメントで確認できず | **未提供**。列単位 `GRANT` はカタログに保存できるが、参照時の列単位の制限は #2081 で未実装。FLS 相当の除去も無い |

## 書き分け

- 同等以上と言える点
  - 全経路への暗黙適用（クライアントが付け忘れられない）。根拠: `table_rls/rls_generalized.rs` の
    `udf_reads_only_reach_visible_rows_across_tables_and_sessions`
  - テナント境界が利用者ポリシー・`BYPASSRLS`・所有者・`row_security` のどれでも外れず、管理者バイパスが無いこと。根拠:
    `table_rls/rls13_row_security.rs` の `tenant_boundary_is_unchanged_by_any_state`、
    `table_rls/rls13_policy_roles_row_security.rs` の `rls13_row_security_off_never_widens_tenant_boundary`
  - 他テナントの存在の秘匿。根拠: `rls10_write_constraint_paths.rs` の
    `t4_constraint_errors_do_not_reveal_foreign_existence`、`table_rls/rls13_policy_tenant_nonleak.rs`
  - フィルタ付き ANN の取りこぼしを厳密走査への切替で塞ぐこと（結果件数の保証は既定の SCALAR 事前フィルタ経路に限る）。根拠: `table_rls/rls13_policy_search.rs`
- 同等の点: テナント内で隠れた行の存在が制約違反から推知されうること、PERMISSIVE／RESTRICTIVE の合成・既定拒否・
  `row_security = off` の拒否、所有者除外と `FORCE`（根拠: `table_rls/rls13_policy_constraint_scope.rs`）
- 未提供の点: 列レベル制御（#2081）、ロール別のポリシー条件（`TO <ロール>`）、
  利用者が付与できる `BYPASSRLS` 属性（[role-privilege-model.md](role-privilege-model.md) の「#2278 段 2」）
  - ロール別のポリシー条件と `BYPASSRLS` 属性の付与は fail-closed（`0A000`／`42501`）で拒否し、
    テナント境界は緩めない
  - 列レベル制御の方針は [column-level-privilege.md](column-level-privilege.md)（Proposed）に置く
  - 列レベル制御は拒否契約ではない。列単位 `GRANT` は受理されカタログに保存されるが、
    参照時の列集合の判定が未実装（#2081）で、保存された列権限はまだ適用されない
- 運用上の差: 管理者バイパスが無いため、全テナントを横断するバックアップ・調査を 1 接続で行う手段は提供しない
  （[運用](fixed-tenant-boundary.md)）

## 参照

他 DB（確認日 2026-11-05。版は各ページの現行版）:

- PostgreSQL 17: <https://www.postgresql.org/docs/17/ddl-rowsecurity.html>
- Elasticsearch: <https://www.elastic.co/docs/deploy-manage/users-roles/cluster-or-deployment-auth/controlling-access-at-document-field-level>、
  <https://www.elastic.co/docs/solutions/search/vector/knn>
- Qdrant: <https://qdrant.tech/documentation/guides/multiple-partitions/>。
  セキュリティ（API キー・JWT）のページは本書の作成時に内容を確認できなかったため、該当セルは「確認できず」とした

fandhe 側は本リポ内の doc・テスト・コードを関数名とファイル名で参照する（行番号は書かない）。
他 DB の記述は上記の確認日時点であり、更新の追従は本書の範囲外である。
