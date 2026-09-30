# ADR: 集計経路の embedding 非参照デコードスキップと必要列限定デコード（Issue #350）

- ステータス: Accepted
- 対応: Issue #350
- 関連ポインタ: `docs/spec/05-tasks.md`（TASK-166・TASK-167）・
  `docs/spec/04-behavior/sql-surface.md`（SQL-13・SQL-14）・
  `docs/spec/04-behavior/rls.md`（RLS-7・RLS-8）・`docs/spec/04-behavior/data-model.md`
  （TABLE-12）。spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 変更コード: `crates/engine/src/row_codec.rs`・`crates/engine/src/storage.rs`・
  `crates/engine/src/sql/udf_call.rs`・`crates/engine/src/sql/aggregate.rs`・
  `crates/engine/src/sql/group_by.rs`

## 背景

集計実行経路（`sql::aggregate::execute_aggregate`・`sql::group_by::execute_grouped_aggregate`）
は、可視行に対して常に embedding の完全デコード（`Vec<f32>` 確保）とスキーマ全スカラー
列のパース（毎行 `Vec<Option<&str>>` 確保）を行っていた。`COUNT(*)`・`COUNT(col)`・
`SUM(id)` のように embedding や大半のスカラー列を参照しないクエリでも、行数に比例した
不要なデコード・確保コストが発生していた。

## 設計

### 参照列集合の導出

`sql::aggregate::ReferencedColumns::derive` が、束縛済み集計文
（`items`・`metadata_filters`・`expr_filters`・`GROUP BY` キー列）から

- `scalar_mask`（`schema.columns` と同じ長さのマスク。`row_codec::scan_scalar_columns_masked`
  へそのまま渡す）
- `needs_embedding`（`ScalarExpr` 項目・`WHERE` 式述語が embedding へ到達するか）
- `needs_vector_presence`（`COUNT(<VECTOR 列>)` があるか。embedding 自体は不要で
  dim のみ必要）

を一度だけ導出する。embedding へのアクセスは束縛段で `BoundExpr::VectorRef` に
一元化されているため（`sql::udf_call::eval` 参照）、`udf_call::references_embedding`
による式木の再帰走査（`Builtin`/`Binary`/`WasmCall` は引数へ再帰、非網羅 `match` 禁止）
は過小評価が起きない精密判定になる。

### 3 段階デコード（`DecodeTier`）

可視行 1 件あたりのデコードを、クエリが実際に要求する範囲まで縮小する。

| tier | 内容 | 使用関数 |
| ---- | ---- | -------- |
| `Fast` | ヘッダ（tenant・visibility）＋ TABLE-12 のキー/ヘッダ tenant 整合検査＋ dim・embedding 境界・metadata 境界の構造検証（`Vec<f32>`／`&str` 化は省略） | `storage::decode_row_tenant_and_visibility`・`storage::verify_row_key_tenant`・`storage::decode_row_dim_and_metadata_borrowed` |
| `DimAndScalar` | `Fast` と同じ構造検証に加え、マスク済み metadata 列の `&str` 化まで行う | `storage::decode_row_dim_and_metadata_borrowed`・`row_codec::scan_scalar_columns_masked` |
| `Embedding` | embedding を含む完全デコード | `storage::decode_row_embedding_and_metadata_into`（スクラッチ `Vec<f32>` 再利用） |

`Fast` と `DimAndScalar` はどちらもヒープ確保を伴わない
`storage::decode_row_dim_and_metadata_borrowed`（dim・embedding 境界・metadata 境界・
末尾余剰バイトの構造検証。破損は `XX000`）を通る。両者の違いは metadata 列の
presence・長さ・UTF-8 の構造検証を、`Fast` は結果を一切保持しない
`row_codec::validate_scalar_columns`（`Vec` 確保なし）で行い、`DimAndScalar` は
`&str` 化した結果を `Vec<Option<&str>>` へ保持する `row_codec::scan_scalar_columns_masked`
で行う点のみで、破損検知（fail-closed 契約）に差は無い（両者は走査本体
`row_codec::scan_scalar_columns_validated` を共有し、検証済みの値を `Vec` へ積むか
捨てるかだけを呼び出し元が選ぶ。PR #369 codex-review P1 指摘対応・2 件: (1) `Fast` が
この構造検証自体を丸ごと省略し、破損した可視行を `COUNT(*)`/`SUM(id)` 等の集計結果へ
黙って含めてしまう契約変更になっていた旧実装を修正。(2) その後も `Fast` が
`scan_scalar_columns_masked` を呼び毎行 `Vec<Option<&str>>` を確保しており、`Fast` は
結果 `Vec` 生成を省略するという本 tier 契約に反していた実装バグを修正）。

`Fast` は `sql::aggregate::execute_aggregate`（`GROUP BY` なし）専用で、
`needs_embedding`・`needs_vector_presence`・`scalar_mask` がすべて偽、かつ
`metadata_filters`・`expr_filters` がいずれも空の場合のみ選択する（`COUNT(*)`・
`COUNT(id)`・`SUM`/`AVG`/`MIN`/`MAX(id)` の組み合わせ）。`GROUP BY`
（`sql::group_by::execute_grouped_aggregate`）はグループキー列を必ず読む必要があるため
`Fast` を選ばず、`needs_embedding` の有無で `DimAndScalar`／`Embedding` の 2 段階のみを
切り替える。

可視性判定（ヘッダのみ・デコード前）と TABLE-12 のキー/ヘッダ tenant 整合検査は、
どの tier でも必ず・同じ順序で行う。この 2 つはデコード「範囲」の縮小と独立した
不変条件であり、`ReferencedColumns`・`DecodeTier` はここへ一切手を加えない。

### `scan_scalar_columns_masked` / `validate_scalar_columns`（必要列限定デコード）

`row_codec::scan_scalar_columns_masked(schema, buf, mask)` と
`row_codec::validate_scalar_columns(schema, buf)` は、内部の走査本体
`row_codec::scan_scalar_columns_validated` を共有する。`mask[i] == false` の列も
presence・宣言長（`MAX_TEXT_FIELD_LEN`）・バッファ境界の構造検証に加え UTF-8 妥当性検証
まで常に行い（codex-review P1 指摘・PR #369: 未参照列でも不正 UTF-8 を含む永続行を
fail-closed で拒否する既存のエラー契約（`XX000`）を維持する必要があるため）、
`scan_scalar_columns_masked` は検証済みの値のうち要求列だけを `&str` として
`Vec<Option<&str>>` へ積む一方、`validate_scalar_columns` は検証のみ行い値を一切
保持・返却しない（呼び出し元は `Vec` を確保しない）。untrusted な行バッファに
対する構造検証・内容検証は、マスク外の列・`validate_scalar_columns` 経由のいずれでも
一切弱めない。既存 `scan_scalar_columns` は全列 `true` の薄いラッパーへ変更した
（呼び出し元・テストは無変更で green）。

`DecodeTier::Fast`（`scalar_mask` が全 `false`。`metadata_filters` も必ず空。
`MetadataFilter` は自身の列を必ずマスクへ反映するため）は `validate_scalar_columns`
を使い、走査そのもの（presence・列長・UTF-8・余剰バイトの構造検証）は省略しない一方、
`Vec<Option<&str>>` の確保は行わない（codex-review P1 指摘・PR #369・2 件。(1)
走査自体を省略すると破損した可視行を検出できないまま集計が成功してしまう
fail-open な契約変更になるため走査は必須。(2) 走査を `scan_scalar_columns_masked`
で行うと `Fast` でも毎行 `Vec` を確保してしまい「`Fast` は結果 `Vec` 生成を省略する」
という本 tier 契約に反するため、検証専用 API を新設して置き換えた）。`DimAndScalar`
は引き続き `scan_scalar_columns_masked` を使い、全 `false` マスクで呼び出しても
構造検証・UTF-8 妥当性検証は他マスクと同様に全列へ行われ、省略されるのは検証済み
`&str` の生成・保持だけである。

## 契約変更点（PR #369 codex-review P1 対応で撤回）

初版実装では `DecodeTier::Fast` 到達クエリが可視行の embedding/metadata セクションの
構造破損・次元不一致（従来 `XX000` で query 失敗）を検出しなくなっており、意図的な
契約変更として記述していた。しかし codex-review（PR #369）の指摘により、この契約
変更が spec 側の対応する定義変更を伴わないまま行われていた点が P1 として指摘され、
`Fast` にも `decode_row_dim_and_metadata_borrowed`（ヒープ確保を伴わない構造検証）を
必ず通す実装へ修正した。これにより従来の `XX000` fail-closed 契約は `Fast` を含む
全 tier で維持され、契約変更は生じていない（`Fast`/`DimAndScalar`/`Embedding` の差は
純粋にデコード「範囲」の縮小のみで、検証水準の差ではない）。RLS 可視性判定・
TABLE-12 のキー/ヘッダ tenant 整合検査は元々全 tier で維持していた。この挙動は
`sql::aggregate::tests` の固定テスト（後述）で明示的に固定する。

## `redb::Table::len()` への縮退は不採用

`len()` は不可視行を含む全件数を返すため、「RLS 可視集合＝テーブル全行」が静的に
証明できる場合に限り正しいが、現行の `PolicyContext`（tenant 別可視性・private/public）
にはその証明を engine 側で完結できる状態がなく、適用条件の誤りが即 fail-open（他
テナント行数の漏えい・RLS-7/RLS-8 違反）になる。`DecodeTier::Fast` で行数比例コストの
主要因（embedding 確保・全列パース）は既に除去できているため、縮退は採らない。
tenant プレフィクスの range scan 化等の更なる最適化はスコープ外（オーナー確認後に
別 Issue として起票候補）。

## 固定したテスト

- `crates/engine/src/row_codec.rs`: `scan_scalar_columns_masked` のマスク挙動（マスク
  外列でも構造検証・UTF-8 妥当性検証は維持し `&str` 化・保持のみ省略・不正 UTF-8 は
  マスク内外を問わず `Err`・マスク長不一致は `Err`）
- `crates/engine/src/storage.rs`: `decode_row_dim_and_metadata_borrowed` の完全デコード
  との一致・`decode_row` と同じ破損検知・`verify_row_key_tenant` の受理/拒否
- `crates/engine/src/sql/udf_call.rs`: `references_embedding` の直接参照・`Builtin`/
  `Binary` 経由の間接参照・非参照式
- `crates/engine/src/sql/aggregate.rs`:
  `count_star_fastpath_still_fails_closed_on_corrupted_embedding_section`
  （`DecodeTier::Fast` が embedding セクションの破損を fail-closed（`XX000`）で
  検出することを実証。PR #369 codex-review P1 対応の固定）・
  `count_vector_column_on_same_corrupted_row_still_fails_closed`（`VECTOR` 列参照経路
  〔`DecodeTier::DimAndScalar`〕も同じ破損を検出し、`Fast` との検証水準の差が無いことの
  対照テスト）
- 既存の結合テスト（`crates/engine/tests/sql_aggregate.rs`・`sql_group_by.rs`）は
  無変更のまま green（`WHERE`・`vec_norm` を含む式・`GROUP BY` 等の end-to-end oracle
  一致を維持）
- `crates/engine/tests/sql_aggregate_public_api.rs`（TASK-186・NOSQL-4・NOSQL-5。
  Issue #727）: `bind_aggregate`／`execute_aggregate`／`BoundAggregate` が engine
  クレート外から到達可能であることの固定に加え、`GROUP BY` の有無での振り分け
  （Issue #475 の列挙形フォールバック経路）・索引対応述語ありの `WHERE`・空集合契約
  （`COUNT=0`・`SUM=NULL`）が公開ラッパー越しでも同一であることを確認
- `crates/engine/tests/core_bound_plan_entry.rs`（TASK-186・NOSQL-4・NOSQL-5。
  Issue #728）: 単一 `Storage` 構成で SQL テキスト非経由に束縛済み集計計画を
  実行するセッション対応エントリ `EngineCore::execute_bound_aggregate_in_session`
  が、SQL テキスト経由と同じ `VisibleBitmapCache`（本 Issue のキャッシュ）を
  共有することを固定。詳細は `docs/design/bound-plan-session-entry.md` 参照

## 性能実測について（申し送り）

計画段階では既存の `feature_bench.rs`（`SELECT COUNT(*)` の計測フェーズを含む想定）を
前後比較に使う想定だったが、実装時点で本リポジトリに同名のベンチは存在しなかった
（`crates/engine/benches/` には C1（検索）・GPU バッチ等の既存ベンチはあるが、集計経路
専用のベンチは無い）。本 Issue のスコープでは新規ベンチ整備を行わず、代わりに
`DecodeTier::Fast` が実際にデコードを行わないことを上記の破損注入テストで構造的に
実証する方針とした。行数比例の実測（レイテンシ・スループット差）が必要な場合は、
別途ベンチ整備をオーナー確認のうえ Issue 化する。

## #894 追記: 新スカラー型（TABLE-13・TASK-199）の 3 段階デコード tier 対応

- ステータス: Accepted（精査・回帰テストの固定）
- 対応: Issue #894（`docs/spec/05-tasks.md` TASK-199）
- 変更コード: `crates/engine/src/sql/aggregate.rs`（`select_decode_tier` 抽出のみ）・
  `crates/engine/src/row_codec.rs`・`crates/engine/src/sql/group_by.rs`・
  `crates/engine/src/sql/scan.rs`（いずれもテスト追加のみ）
- 新規結合テスト: `crates/engine/tests/decode_tier_scalar_types.rs`

### 監査結果

INTEGER・BIGINT・REAL・DOUBLE PRECISION・BOOLEAN・DATE・TIMESTAMP・NUMERIC・BYTEA・
UUID・ARRAY・JSON/JSONB・ENUM（Issue #881〜#890）の各追加時点で、本 ADR が定める
3 段階デコード契約（`row_codec::scan_scalar_columns_validated` の全型網羅・`match` に
ワイルドカードを置かない設計、`sql::aggregate::ReferencedColumns::derive`・
`sql::scan::decode_tier_for` の型ごとの分岐を持たない `scalar_mask` 反映）は
production コードとして既に実装されていた（#892 で集計本体が新型へ対応済み）。
本 Issue の実体は次の 3 点である。

1. 網羅性・fail-closed 性の確認（新型ごとに個別の「非要求列でも検証を続ける」実装が
   `row_codec.rs` に揃っていることをコードリーディングで確認済み。ENUM の語彙照合・
   NUMERIC の precision 検証・ARRAY のフレーム解析・非有限浮動小数の拒否・日時の値域・
   UTF-8 検証はいずれもマスクの `wanted` に関わらず実行される）
2. それを固定する回帰テストの追加（新型はほとんど個別追加時のテストが `TEXT` 型を
   前提にした既存テストのコピーに留まり、tier 選択・全型を含むスキーマでのマスク走査・
   TABLE-12 の新型版は未検証だった）
3. `sql::aggregate` の tier 選択インライン `if` 連鎖を `select_decode_tier`
   （`pub(crate) fn select_decode_tier(referenced: &ReferencedColumns, has_expr_filters:
   bool) -> DecodeTier`）へ挙動不変で抽出し、単体テストで直接固定できるようにした
   （production の判定式・分岐順序は抽出前と完全に同一）

### 新型ごとの tier 対応表

| 型 | `COUNT` | `SUM`/`AVG`/`MIN`/`MAX` | 選択される tier（`WHERE`／式なし） |
| --- | --- | --- | --- |
| INTEGER/BIGINT/REAL/DOUBLE | ○ | ○ | `DimAndScalar` |
| BOOLEAN/DATE/TIMESTAMP/ARRAY/BYTEA/JSON/JSONB/ENUM/UUID | ○ | 型により不可（`resolve_aggregate_input` が拒否） | `DimAndScalar` |
| NUMERIC | ○ | ○ | `DimAndScalar` |
| （新型を一切参照しない `COUNT(*)`/`COUNT(id)`/`SUM(id)` 等） | - | - | `Fast`（不変） |
| `VECTOR` 列を参照する式（`ScalarExpr`） | - | - | `Embedding`（不変） |

新型はどれも `Embedding` を要求しない（embedding をデコードしない）契約を維持する。

### 固定したテスト

- `crates/engine/src/sql/aggregate.rs`: `select_decode_tier` の直接構成した
  `ReferencedColumns` による分岐順序の単体テスト、および新型 `AggregateInput`
  （`UuidColumn`/`BooleanColumn`/`NumericColumn`/`DateColumn`/`TimestampColumn`/
  `IntegerColumn`/`BigIntColumn`/`RealColumn`/`DoubleColumn`）を `ReferencedColumns::
  derive` へ通した経路での `DimAndScalar`・`needs_embedding() == false` の固定、
  新型を多数持つスキーマでの `COUNT(*)`（`Fast`）・`COUNT(<VECTOR 列>)`
  （`DimAndScalar`）、TABLE-12（キー/ヘッダ tenant 不一致）が `Fast`／新型列参照の
  `DimAndScalar` いずれでも `XX000` になること、参照されない新型列（REAL）の
  メタデータ破損が `COUNT(*)` でも fail-closed に拒否されること
- `crates/engine/src/sql/group_by.rs`: `TEXT` キー `GROUP BY` ＋新型（UUID）集計での
  TABLE-12 fail-closed・正しい結果（可視行だけのオラクルと一致）
- `crates/engine/src/sql/scan.rs`: `decode_tier_for`（純関数）の新型列投影
  （`DimAndScalar`）・`id` のみ投影（`Fast`）・`VECTOR` 列投影（`Embedding`）
- `crates/engine/src/row_codec.rs`: 可変長・固定長が交互になる全新型スキーマ
  （ENUM を除く。後述）での単一ビットマスク総当たり（対象列だけが値化され他は
  `None`・全 false／全 true マスクの境界）、非要求列でも構造・値域検証を省略しない
  こと（REAL の NaN・BOOLEAN の未知バイト・NUMERIC の precision 超過・INTEGER の
  途中打ち切り・DATE の値域超過・全新型スキーマでの末尾余剰バイト。BYTEA/JSON/
  ARRAY・ENUM の同種契約は個別のバイト単位テストとしては追加していないが、
  上記の単一ビットマスク総当たり・末尾余剰バイトテストは全新型スキーマ〔ENUM を
  除く〕に対して行毎に構造検証を通しており、`scan_scalar_columns_validated` の
  各型別 `match` 分岐はいずれも `wanted` の分岐より前に検証を完了させる実装で
  あることをコードリーディングで確認した）
- `crates/engine/tests/decode_tier_scalar_types.rs`（新設）: `EngineCore::execute_sql`
  経由で `COUNT(*)`（Fast）・`COUNT(u)`/`MIN(dt)`/`SUM(i)`（DimAndScalar）・
  `GROUP BY lang` ＋新型集計・広域取得 `SELECT id, i, u, dt FROM ... LIMIT` が、
  tenant A/B・NULL 行を混ぜた可視行のみのオラクルと一致し他テナント行を混入させない
  ことを固定

ENUM は `Storage::create_enum_type`（redb ファイルを要する）を経由しないと
`EnumTypeDef` を構築できないため、`row_codec.rs` の軽量なユニットテスト（DB ファイル
不要）の対象からは意図的に外し、既存の `tests/enum_column.rs`（マスク付き走査の
語彙照合 fail-closed テストを含む）と新設の `tests/decode_tier_scalar_types.rs`
（`Storage` 経由）で検証する。

### 性能

`select_decode_tier` への抽出はクエリ 1 回あたり 1 回だけ呼ばれる分岐の関数化であり、
可視行 1 件ごとの行走査ループ（`scan_scalar_columns_validated`・各実行経路の行ループ
本体）には一切手を入れていないため、構造的に退行の余地がないと判断し本 Issue の
スコープでは前後比較の実測を必須化しなかった。行ループ自体に変更が入る場合は
`docs/design/benchmark-judgement-policy.md` の規約に従った実測を必須とする。

### 申し送り

- `row_codec::scalar_refs_as_text`（`pub`・呼び出し元ゼロ・fail-open の危険が doc に
  明記済み）の削除または非推奨化は公開 API の破壊的変更になるため別 Issue の候補
- Issue #891（`BoundExpr` への新型スカラー列参照追加）時、`ReferencedColumns::
  derive`（`ScalarExpr` 項目・`expr_filters`）と `sql::scan::decode_tier_for`
  （`Computed` 投影・`expr_filters`）は現状 embedding への到達しか見ておらず、式が
  参照する新型スカラー列も `scalar_mask` へ反映する必要がある（反映しないと
  値があるのに `None` として評価される fail-open になり得る）
- `GROUP BY` キー列の新型拡張（現状 `TEXT` に限定）・二次索引経路の新型対応
  （Issue #893）はいずれも別 Issue の担当

## #1258 追記: 埋め込み非デコードの直接計数テスト

3 段階デコード（`Fast`／`DimAndScalar`／`Embedding`）が埋め込み本体を読まないことを、
結果一致や `select_decode_tier` の選択結果ではなくデコード計数で直接固定する
（TABLE-13・INDEX-5・TASK-199・RLS-7／RLS-8 ポインタ）。

- 計数プローブ: `storage::decode_probe`（`#[cfg(test)]`・thread_local）。
  `decode_row_body_into`・`decode_row` を埋め込みデコード、
  `decode_row_dim_and_metadata_borrowed` を構造検証のみのデコードとして数える。
  production ビルドには含まれない
- テスト: `sql/aggregate_embedding_decode_tests.rs`
  - 層 A: `execute_aggregate` 直呼び出し。新スカラー型 13 種の受理される
    COUNT／SUM／AVG／MIN／MAX と `COUNT(*)`、単一キー GROUP BY で
    埋め込みデコード 0 回・走査行数 > 0 を確認する
  - 層 B: `EngineCore::execute_sql` 経由の WHERE なし単一行集計・複数キー GROUP BY と、
    型不整合の拒否（DATE／TIMESTAMP の SUM／AVG は 42883、その他は 22000）が
    デコード前に失敗すること
  - 陽性対照: `vec_norm(embedding)` 集計のデコード回数が可視行数と一致する
    （RLS で不可視な行が本体デコードに到達しないことも同時に固定）
- 範囲外: `execute_sql` 経由の単一キー・WHERE なし GROUP BY は cold cache で
  スカラー索引／arena スナップショット構築（`capture_scalar_index_snapshot`）が
  可視行全件の埋め込みをデコードする。設計どおりの挙動で 3 段階デコードの契約の外のため
  層 B には含めない
