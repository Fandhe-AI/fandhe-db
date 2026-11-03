# 半径検索（`WHERE <VECTOR 列> <距離演算子> <リテラル> <比較> <数値>`）の実行（Issue #2036）

- ステータス: Implemented（Phase A・B。Phase C は未実装）
- 対応: Issue #2036（親 #2034・兄弟 #2035）・ビヘイビア SQL-74 ((1)(2)(3)(5))
- 関連: [cosine-distance-method.md](cosine-distance-method.md)（コサイン距離の式）・
  [sql-offset-paging.md](sql-offset-paging.md)
- spec 本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 検証コード: `crates/engine/src/sql/vector_distance.rs`（距離関数の単体テスト）・
  `crates/engine/src/sql/udf_call.rs`（評価ノード・NaN 順序の単体テスト）・
  `crates/engine/tests/sql_t40_up/sql74_radius_search.rs`（境界・NaN・論理演算・
  brute-force 照合・テナント境界・DML）・
  `crates/engine/tests/core_misc/sql74_radius_hnsw.rs`（HNSW opt-in）

## 1. 評価の置き場所（Phase A）

- 距離式は `BoundExpr::VectorDistance { kind, query }` として束縛し、`Scalar`（float8 相当）を返す。
  行の embedding が空（VECTOR 列が NULL）の行は NULL で、WHERE では偽として扱う
  （`VectorRef` と同じ契約）。`references_embedding` は真を返すので、embedding を投影しない文脈
  （集計・DML など）でも列がデコードされ、空のスライスで NULL と誤評価されない。
- 述語は既存の式述語（SCALAR 段）として、RLS 段と利用者ポリシー段を通った可視行だけに適用する。
  そのため結果は述語を厳密に評価した場合（brute-force）と一致し、他テナントの行は件数にも
  エラーにも影響しない。
- 距離の値は pgvector と同じ式（f32 で累積し f64 で返す）。コサインは ADR の C1 の手順で、
  ノルム 0 のとき NaN になる。ORDER BY 側のスコア（内積の降順）との統合は #1977・#1987・#1991。
- 比較は PG の float8 の順序にする（NaN 同士は等しく、NaN はどの数より大きい）。片方が NaN の
  ときだけ分岐し、通常値は従来の IEEE 比較のまま。列の値は格納時に非有限値を拒否するので、
  既存の結果は変わらない。`<>` は `<` OR `>`、`BETWEEN` は `>=` AND `<=` に展開されるため、
  `NaN <> NaN` は偽・`NaN BETWEEN 0 AND 2` は偽になる。

## 2. 取りこぼしの防止（Phase B）

`HINT ORDER` で DISTANCE を SCALAR より先にすると、式述語は DISTANCE 段の後の事後フィルタになる。
上位 k 件が述語で間引かれると `LIMIT` に満たず、取りこぼしになる。

- 採用した方式: 距離述語を含む文は、束縛時に評価順序を既定（SCALAR 先行）へ倒す
  （`parser::bind_in_session_with_limits`）。述語を満たす行だけを候補マスクにして検索するので、
  HNSW でも厳密走査でも件数が欠けない。実行・EXPLAIN とも `bound.evaluation_order` だけを
  見るため、両者の経路表示は一致する。
- 検討した代替: k を倍増して取り直すループ（`search_prepared_resumable`）。事後フィルタの
  コストを抱えたまま上限（`MAX_SEARCH_K`）で縮退処理も要るため、事前フィルタへ倒す方が単純で
  確実と判断した。取り直しは SQL-73 の反復走査の設定（未実装）と合わせて別途検討する。
- 距離述語を含まない文の `HINT ORDER` の挙動は変えない。

## 3. 対象外（Phase C と申し送り）

- しきい値を DISTANCE 段へ持ち上げて途中で打ち切り、HNSW を早期終了する最適化は、ORDER BY の
  スコアと WHERE の距離が一致してから行う（#1977・#1987・#1991 待ち。正規化していないデータ
  ではコサインのしきい値が内積の順に対して単調にならない）。
- `ORDER BY` があって `LIMIT` が無い形は #2242。投影・CASE の結果位置の距離値は #2006。
- HAVING・JOIN での距離比較、しきい値側の `$n` は対象外（従来の拒否のまま）。
- NoSQL の filter は対象外。
