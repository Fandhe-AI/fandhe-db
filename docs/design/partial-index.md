# 部分索引の宣言側（述語 parser と決定性検査）

対象: Issue #1653（親 #1652）。ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11・INDEX-10、
`docs/spec/04-behavior/error-format.md` ERR-6（spec 本文は転記しない）。
兄弟: #1654（格納・DML 維持）、#1655（述語の含意判定）、#1656（プランナ選択・EXPLAIN）、
部分 UNIQUE は #1657。式索引の宣言側（[expression-index.md](expression-index.md)）と同じ形で、
述語は解析・分類までを行い、決定的と判定した述語も最終ゲートで `0A000` にする。

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
| 決定的な述語（#1654 まで） | `0A000` | `partial indexes are not supported yet` |

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
