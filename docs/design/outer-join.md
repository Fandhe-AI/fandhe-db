# LEFT / RIGHT / FULL OUTER JOIN（Issue #926・SQL-28・RLS-10・TASK-212）

## ステータス

Accepted・実装済み。

## ポインタ

- spec: `docs/spec/05-tasks.md` TASK-212・`docs/spec/04-behavior/sql-surface.md` SQL-28
  （検討中）・`docs/spec/04-behavior/rls.md` RLS-10・`docs/spec/04-behavior/error-format.md`
  ERR-6（`42702`・`42804`）
- 前提: `docs/design/inner-join.md`（Issue #925・INNER JOIN）
- 関連 Issue: #931（JOIN 経路の RLS 境界の網羅検証・対象外）

spec 本文はここへ転記しない（`.claude/rules/spec-confidentiality.md`）。以下は本リポの
実装既定値・設計判断の記録。

## 背景・目的

`sql::join`（Issue #925）は `[INNER] JOIN` のみを受理し、`LEFT`／`RIGHT`／`FULL [OUTER]`
JOIN は `parse_join_statement` が `42601` で拒否していた。本 Issue ではこれらを開放し、
相手側に一致する行が無い保存側の行も NULL 補完して出力に含める。RLS・fail-closed・
`wire_code` 契約は INNER と同様に維持する。

## 受理文法（差分）

```text
join_type := [INNER] JOIN | LEFT [OUTER] JOIN | RIGHT [OUTER] JOIN | FULL [OUTER] JOIN
```

`inner-join.md` の文法をこの `join_type` の拡張だけ差し替える。それ以外（`ON`・`WHERE`・
`LIMIT`／`OFFSET`・別名・対象外事項）は INNER と共通。引き続き `42601`（fail-closed）:
`OUTER JOIN` 単独（`LEFT`／`RIGHT`／`FULL` を伴わない形）、`CROSS`／`NATURAL` JOIN、
`JOIN ... USING (...)`、非等価の `ON`、ベクトル順位付けとの併用、`$n` パラメータ、ビュー参照
（3 テーブル以上の連鎖・スカラー `ORDER BY`・集計形・`OR`／`IN`／列同士の比較は Issue #1190 で
受理対象になった。`multi-way-join.md` 参照。本書の 2 テーブルの WHERE 簡約規則は、そこでの
一般規則〔strict な relation を NULL 補完しうる段の保存側フラグを落とす〕の特殊ケース）。

構文検出（`sql::allowlist::looks_like_join`）は変更していない——`LEFT`／`RIGHT`／`FULL`
はすでに Issue #925 時点で検出対象だったため（当時は `parse_join_statement` 側で一律
`42601` に分類していた）。`sql::allowlist::JoinKind`（`Inner`／`Left`／`Right`／`Full`）を
新設し、`ValidatedJoin::kind` として保持する。

## WHERE の意味論（設計判断）

保存側（`LEFT` の左、`RIGHT` の右、`FULL` の両側）の述語は、INNER と同様にその側の
スキャンへプッシュダウンする。保存側の行が一致するかどうかは相手側だけで決まるため、
プッシュダウンしても意味は変わらない。

**欠損側**の述語は、PostgreSQL 意味論（NULL 補完行は WHERE より後に評価され、strict な
述語は必ず偽になるため実質 INNER に縮退する）を次の簡約規則で再現する:

- `preserve_left  = kind ∈ {Left, Full}  && 右側への WHERE 述語が 0 個`
- `preserve_right = kind ∈ {Right, Full} && 左側への WHERE 述語が 0 個`

この簡約が正しいのは述語が strict（NULL に対して必ず偽）な場合に限る。現行で受理する
全 `JoinWherePredicate` variant（`=`・`LIKE`・比較・bool 等価・bool 列）はいずれも strict
なため、`sql::join::is_null_rejecting` は常に真を返す。この関数はワイルドカード無しの
網羅的 `match` で実装し、将来 `IS NULL` 等の非 strict な variant を追加した際にコンパイル
エラーで検出させる（簡約規則が黙って壊れるのを防ぐ。`build_plan` は `kind ∈ {Left, Right,
Full}` の場合に限り、non-strict な述語を検出したら簡約せず `42601` で拒否する fail-closed
分岐を持つが、現行は到達しない。この防御は NULL 補完を行う外部結合にのみ必要であり、
`Inner` は簡約規則自体を使わないため対象外とする）。

`preserve_left`・`preserve_right` は `sql::join::JoinPlan` に保持し、`Inner` は両方偽の
ままで INNER JOIN と完全に同じ挙動になる。

## 実行（`sql::join::execute_with_limits` の拡張）

1. 両辺の評価・ハッシュ結合本体（ビルド側選択・キー正準化・`NULL` キーの除外）は
   `inner-join.md` と同じ
2. ハッシュ結合の一致ペアを数える過程で、保存側ごとに未一致フラグ配列
   （`matched_left: Vec<bool>`／`matched_right: Vec<bool>`。`preserve_*` が真の側だけ
   確保し、確保前に `JoinBudget` へ計上する）を更新する。**結合キーが NULL の保存側の
   行**は最初から `false` のまま残り、NULL 補完行として出力される（INNER との差分の
   要点）
3. カーディナリティ判定: `total = pairs.len() + (preserve_left ? 左の未一致数 : 0) +
   (preserve_right ? 右の未一致数 : 0)` を `checked_add` で数え、`MAX_JOIN_OUTPUT_ROWS`
   を超えたら実体化前に `54000`（`LIMIT`／`OFFSET` の値には依存させない。INNER と同じ
   規則）。判定に使うのは可視行だけ（RLS 適用済み）
4. 出力の中間表現は `Vec<(Option<u32>, Option<u32>)>`。安定ソート `sort_by_key`（
   `sort_unstable*` は使わない。`scripts/check_sort_determinism.sh` ゲート対応）のキーは
   `(l.is_none(), l.unwrap_or(0), r.is_none(), r.unwrap_or(0))`——左側を持つ行はその左
   走査位置・右走査位置の順に並び、左の未一致行はその左位置の直後に 1 行だけ出る。
   右だけの行は末尾に右の走査位置の順で並ぶ。`Inner`（`Option` が常に `Some`）では
   このキーは INNER の `(l, r)` ソートに潰れ、既存の出力順序と完全に一致する。ビルド側
   の選び方は順序に影響しない
5. `OFFSET`／`LIMIT` を適用したあとに行を実体化する。`OutputColumn::Left(pos, _)` で
   左行が無い場合は `Cell::Null`（`resolve_output_cell` ヘルパーで解決）。行の予算見積もり
   （複製前に借用のまま算出する既存方式）は NULL セルを 0 バイトとして扱う
6. `ResultRow.id` は左行があれば左の `id`、無ければ右の `id`（wire・HTTP のクエリ出力
   ではいずれも `row.id` を使っていないため観測上の影響は無い）

列メタデータ（`ColumnMeta`）は INNER と同じで NULL 許容かどうかを持たないため、Describe
（`sql::join::describe_columns`）の結果は結合種別に関わらず不変。

## wire・HTTP・エラー契約

`Cell::Null` は `ColumnMeta::Id` を含むどの列型でも、SQL wire では長さ `-1`
（`crates/wire-server/src/result_encoder.rs`）、NoSQL（HTTP）では `null`
（`crates/wire-server/src/http/query/response.rs`）として出力される既存の汎用経路を
そのまま使う。**新しい `wire_code` は追加しない**（`42601`・`42P01`・`42702`・`42804`・
`22000`・`54000`・`0A000` の既存契約で足りる）ため、`crates/wire-server/src/http/status.rs`
と `crates/wire-server/docs/nosql-api.md` は変更していない。エラーの優先順位は
`inner-join.md` と同じ（`42601` → `42P01` → 束縛 → `22000` → 実行時の `54000`）。

## テナント境界（RLS-10 (b)）

未一致の判定は、RLS 適用済みの相手側の可視行に対してだけ行う。他テナントの `Private`
行にしか一致しない保存側の行は、一致する行がまったく無い場合と**同じ結果**（NULL
補完）になり、他テナント行の存在は観測できない。RIGHT／FULL の末尾に並ぶ未一致行も、
当該セッションの可視行だけから生成されるため、他テナントの `Private` 行は混入しない
（`Public` 行は既存の可視性規則どおり可視になる）。明示トランザクションでは従来どおり
`ensure_relations_not_written` を通す（`core.rs` の `Statement::Join` アームは変更不要）。

## セキュリティ考慮（OWASP Top 10）

- **A01 アクセス制御**: 両辺とも、サーバーが導出した `PolicyContext` で既存の scan 経路を
  通して独立に評価する（第 2 の実行器を作らない）。一致・未一致の判定、NULL 補完行の
  生成、カーディナリティ・`OFFSET` の判定は可視行だけで行う
- **A03 インジェクション**: 識別子は `TableRef`／`ColumnRef` の型で扱い、SQL 文字列を
  組み立てない。結合種別は固定の予約語との照合だけで決める
- **A04 不安全な設計（DoS）**: 未一致行を含めた合計カーディナリティを実体化前に上限と
  照合する。`matched_left`／`matched_right` は入力行数上限で有界で、確保前に共有バイト
  予算へ計上する。整数演算は `checked_*`／`saturating_*` を使う
- **A04 fail-closed**: 非 strict な述語が欠損側にあれば `42601`（網羅的 `match` で将来の
  追加を強制検出する）
- **A05 情報漏えい**: エラー文言には入力された列名・修飾子と固定の英語文言だけを含める。
  新しい `wire_code` や HTTP の状態コード写像は追加しない
- **A08 整合性**: 1 つの `read_txn`（同一スナップショット）で両辺を評価する（既存）

## 対象外（申し送りのみ。Issue は起票しない）

- `CROSS` JOIN・`NATURAL` JOIN・`USING (...)`
- 外部結合とウィンドウ・`DISTINCT` の組み合わせ（3 テーブル以上の連鎖・集計・`GROUP BY`・
  スカラー `ORDER BY`・WHERE の `OR`／`IN` は Issue #1190〔`multi-way-join.md`〕で受理対象に
  なった）
- WHERE の `IS NULL`／`NOT`（導入する場合は `is_null_rejecting` と簡約規則の再設計が必要）
- ON 句の中に書くリテラル条件（`ON a.x = b.y AND b.z = 'v'`）
- JOIN を含む VIEW・CTE・サブクエリ・`EXPLAIN`・カーソル・`COPY`・`$n`
- JOIN 経路の RLS 境界の網羅検証（#931）
- 専有環境での性能計測
