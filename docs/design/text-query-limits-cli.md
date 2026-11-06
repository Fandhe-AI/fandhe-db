# 全文検索クエリの項数・前方一致展開数上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #2115・SEARCH-12／RLS-8 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-text-query-terms`・`--max-text-query-prefix-expansions`
- 先行: [regex-limits-cli.md](regex-limits-cli.md)（Issue #1786）と同型。構文と評価の設計は [fulltext-query-syntax.md](fulltext-query-syntax.md)（D2・D5・D6・D7）

## 背景

除外語・前方一致・`or` を可視集合の BM25 へ結線すると、1 つの検索語から走査する posting の量が「項の数」と「前方一致の展開後の語数」に比例して増える。長さと一意語数の既存上限（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS`）だけでは、`a or a or a ...` のような重複グループや、重なった語幹の繰り返しで走査量を増幅できる。新しい 2 つの上限を持ち、運用者が起動時に下げられるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限がそのまま既定値。項数 `1,024`（`MAX_QUERY_TERMS` と同値）・展開数 `1,024`。設定できるのは**下げる方向だけ**（範囲は `1..=1024`）。引き上げは対象外 |
| D2 | 項数は「グループごとの一意な語（通常語・語幹・フレーズ／除外の構成トークン）の総和」。演算子を含まない入力は 1 グループで一意語数が `MAX_QUERY_TERMS` 以下に収まるため、既定値では拒否されない（D8 の後方互換）。グループごとに数えるのは、重複グループによる走査量の増幅を上限に含めるため |
| D3 | 展開数は「グループごとの一意な語幹（正・除外とも）の展開語数の総和」で、展開語は**可視集合で df > 0** の語だけを数える。不可視テナントの語を数えないので、上限超過の有無から他テナントの語の存在を推測できない（RLS-8）。時間差のサイドチャネルは既知の制約として残る |
| D4 | 超過は `54000`（`TextSearchError::TooManyTerms`／`TooManyPrefixExpansions`／`PhraseTooLong`）。既存の長さ・一意語数の超過は `22000` のまま。検査順は 既存上限（22000）→ 項数 → フレーズ長 → 展開数で、両方を超える入力は常に 22000 になる。一部の語だけで黙って検索しない |
| D5 | 値の運搬は `regex_limits` と同型。`engine::text_query_limits::TextQueryLimits`（`Copy`・フィールド非公開・ハード上限で飽和）を `EngineCore::with_text_query_limits` で起動時に 1 回注入する。範囲検証は engine の `validate_*` に一本化し、wire-server は CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけ。環境変数で差し替える経路は作らない |
| D6 | 範囲外・非数値・値欠落は起動エラー。2 回目以降の指定は fail-closed（last-wins にしない）。解決は bind・ユーザーストア読込より前 |
| D7 | フレーズの必須トークン数の上限 `MAX_PHRASE_REQUIRED_TOKENS`（64）は CLI 化しない。位置照合のコストを抑えるデータ非依存の定数で、項数（クエリ全体の幅）とは別の軸（フレーズ 1 つの長さ）であり、64 は通常のフレーズを十分に覆うため。超過は `PhraseError::PatternTooLong` を `54000` へ写す |

## 対象外

- ハード上限の引き上げ・セッション単位の `SET` による変更
- SQL／NoSQL 表層への配線（Issue #2116 で実装済み。`EngineCore::run_select_plan` が `sql::exec` へ引数で渡し、`54000` は `map_hybrid_text_error` が固定の英語文へ写す）
