# 正規表現マッチャ（PG ARE → regex クレートの写像）

## ステータス・対象

- ステータス: 採用（Issue #1786・SQL-55 のポインタのみ。spec 本文は転記しない）
- 対象: `engine::regex_match`（`CompiledRegex`・`RegexCaseMode`・`RegexError`）と、その字句変換器 `regex_match/translate.rs`
- 位置づけ: 評価器の部品のみ。SQL 表層への配線は含まない（`~`／`~*` は #1787、`SIMILAR TO` は #1788、jsonpath の `like_regex` は #1784）。上限の CLI は [regex-limits-cli.md](regex-limits-cli.md)
- 依存: [regex-crate-adoption.md](regex-crate-adoption.md)（Accepted）

## 方針

- 公開シグネチャに `regex` の型を出さない。エラーは自前の `RegexError` で、`client_message` は固定の英語文言（`regex::Error` の文面はパターンを含むため運ばない）
- パターンは非信頼の入力。自前の字句走査（再帰しない・括弧の深さは明示カウンタ）で解析し、リテラルとブラケット要素を 1 文字ずつエスケープして出力する。未検証の部分文字列を `regex` 構文へ連結しない
- 意味論は **C 照合**に限る。ICU 照合の式を `0A000` で拒否するのは配線側（#1787）の責務
- 大文字小文字を区別しない（`~*`・`(?i)`）場合は、`regex` の `(?i)`（Unicode の simple case folding）を使わず、ASCII 英字だけを展開する（C 照合の PG と同じ。`docs/design/like-wildcard-patterns.md` の ILIKE と同じ方針）
- 文字クラス（`\d \s \w` と POSIX クラス）は ASCII の明示クラスへ写す（`regex` の Unicode 既定の `\d`・`\w` は使わない）

## 処理順序

1. パターン長（生のバイト長）の検査（`54000`）
2. 字句走査・変換（非対応は `0A000`、PG でも構文誤りは `2201B`、入れ子が 64 段超は `54000`）
3. `RegexBuilder` のコンパイル（`size_limit`・`dfa_size_limit`・`nest_limit`。`CompiledTooBig` は `54000`）
4. マッチ時に入力長を検査（`54000`）してから `is_match`（真偽のみを返すので、PG の最長一致規則と `regex` の leftmost-first の差は結果に影響しない）

## 写像・拒否の一覧

| PG 17 の構文 | 本実装 |
| ------------ | ------ |
| リテラル・`.`・`^`・`$`・選択・グループ・`(?:`・量指定（非貪欲を含む。`{m,n}` は 0〜255） | 写像（`.` は改行にもマッチ。`^`／`$` は文字列の端だけ） |
| `[...]`・`[^...]`・範囲・先頭の `]`・`[[:alpha:]]` 等の POSIX クラス・ブラケット内の `\d \s \w \D \S \W` | 写像（ASCII の意味論。`[[:cntrl:]]` は PG の実測どおり U+007F〜U+009F を含む） |
| `\d \s \w \D \S \W`・`\A \Z`・`\m \M \y \Y`・`[[:<:]]`・`[[:>:]]`（ASCII の語境界） | 写像 |
| 文字エスケープ `\a \b \e \f \n \r \t \v \cX \xh+ \uhhhh \Uhhhhhhhh`・英数字以外の `\k`・`\B`（バックスラッシュ 1 個） | 写像 |
| 先頭の director `***:`・`***=`、先頭の埋め込みオプション `(?c)`・`(?i)`・`(?q)`・`(?s)`・`(?t)`（複数文字の並びを含む） | 写像 |
| 後方参照・8 進（`\0`〜`\9` 始まりの全形） | `0A000` |
| 先読み `(?=` `(?!`、後読み `(?<=` `(?<!`、コメント `(?#` | `0A000` |
| 照合要素 `[[.x.]]`・等価クラス `[[=x=]]` | `0A000` |
| 埋め込みオプション `(?b)` `(?e)` `(?m)` `(?n)` `(?p)` `(?w)` `(?x)` | `0A000` |
| NUL・サロゲート・Unicode 範囲外を指す `\x`／`\u`／`\U`（PG は受理するが、UTF-8 のテキストには現れない文字） | `0A000` |
| 括弧の不整合・量指定の対象不在・二重の量指定・回数の範囲外（256 以上や `m>n`）・未終了のブラケット・範囲の逆転・末尾の `\`・未知の英数字エスケープ・未知の POSIX クラス・先頭以外の埋め込みオプション・不正な director | `2201B` |

## PG 17 との一致の検証

`crates/engine/tests/data/regex_pg17.tsv` に、PostgreSQL 17（`postgres:17-alpine`、`initdb --locale=C --encoding=UTF8`、各式に `COLLATE "C"`）で採取した 3,714 行（入力 × パターン × `~`／`~*` と結果）を固定し、`crates/engine/tests/regex_match.rs` が全行の一致を検査する。範囲は量指定・文字クラス（POSIX・`\d\w\s`）・アンカー・選択・グループ・非貪欲・語境界・エスケープ・`~*` の ASCII と非 ASCII（`É`/`é`・KELVIN SIGN・`ß`・`ſ`・`İ`）・改行と `.`／`^`／`$`・空パターン・空入力・ブラケットの特殊文字。PG が受理して本実装が拒否する行は、`intended_difference` が種別を列挙している。

## 意図的な差分

| 差分 | 理由 |
| ---- | ---- |
| 後方参照・先読み・後読み・コメント・照合要素・一部の埋め込みオプション・表現できない文字を `0A000` で拒否する（PG は受理） | `regex` は後方参照と lookaround を持たない代わりに線形時間を保証する。写像できない構文は拒否側に倒す（fail-closed） |
| 複雑すぎるパターンを `54000`（`size_limit` 超過）で拒否する（PG は `2201B`）。PG が拒否する `(a{255}){255}` は、既定の上限内なら受理する | 上限は運用者が CLI で下げられる資源上限であり、構文誤りとは別の分類にする |
| 括弧の入れ子が 64 段を超えると `54000` で拒否する（PG は受理） | 走査の深さを有界にする多重防御 |
| エラー文言は固定の英語（PG は詳細文） | パターン・入力の断片をクライアントへ返さない |

## SQL 表層への配線（Issue #1787）

ポインタ: SQL-55（正常系）・ERR-2（SQLSTATE 写像）。`~` `~*` `!~` `!~*` を WHERE の式述語として受理する。

| 項目 | 内容 |
| ---- | ---- |
| 字句 | `~*`・`~`、`!~*`・`!~`・`!=`・（`!` 単独は拒否）の最長一致。新トークン `RegexMatch`／`RegexMatchCi`／`RegexNotMatch`／`RegexNotMatchCi` |
| 構文 | `parse_expression_predicate` が `Expr::RegexMatch { negated, case_insensitive, lhs, pattern }` を作る。`lhs` は列または式、`pattern` は `::` の続かない文字列リテラルか裸の `NULL` のみ（それ以外は `42601`）。値式の中置演算子ではないため SELECT リスト・CASE・HAVING 等では従来どおり `42601` |
| 否定 | 前置 `NOT` と `!~` は `negated` の反転に畳む。左辺・パターンが NULL なら否定の有無に関わらず NULL なので三値論理でも同値 |
| 束縛 | 左辺は通常の式束縛（ICU 照合は `0A000`）で、TEXT 以外は `42883`。パターンは束縛時に 1 回だけコンパイルし、行ごとには再コンパイルしない。上限は `sql::regex_op::RegexLimitsScope` が `EngineCore::regex_limits` を文の束縛区間へ運ぶ |
| 評価 | 左辺 NULL またはパターン NULL は NULL。それ以外は `is_match` の結果を `negated` で反転。入力長超過は `54000` |
| 拡張クエリ | `<列> (~\|~*\|!~\|!~*) $n`（`sql::params` のパターン 11）。`$n` の型は左辺の列に依らず text。NULL のバインドは裸の `NULL` へ置換。逆向き・両辺 `$n`・式の内側・`$n::text` は `42601` |
| 索引 | 使わない（常に残余述語で RLS 後の可視行だけを評価する） |
| 台帳 | `content_hash` は式タグ 21（`negated`・`case_insensitive`・左辺・パターンの順）、トークンタグ 30〜33 |
| 対象外 | CHECK（`0A000`）、索引の式・述語（`0A000`）、UDF 本体（`0A000`）、ビュー本体・JOIN・HAVING・サブクエリ・SELECT リスト（`42601`）、列・式のパターン、`SIMILAR TO`（#1788）、jsonpath の `like_regex`（#1784） |

PG との意図的な差分: PG は評価される行が 0 件なら不正パターンでもエラーにしないが、本実装は束縛時に `2201B`／`0A000`／`54000` で拒否する（`LIKE` の `22025` と同じ fail-closed）。

あわせて、一時表の経路（`core/temp_route.rs`）が `regex_limits` を一時コアへ引き継いでいなかった漏れを直した（運用者が CLI で下げた上限が効かない fail-open の防止）。
