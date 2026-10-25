# 正規表現マッチャ（PG ARE → regex クレートの写像）

## ステータス・対象

- ステータス: 採用（Issue #1786・SQL-55 のポインタのみ。spec 本文は転記しない）
- 対象: `engine::regex_match`（`CompiledRegex`・`RegexCaseMode`・`RegexError`）と、その字句変換器 `regex_match/translate.rs`
- 位置づけ: 評価器の部品のみ。SQL 表層への配線は含まない（`~`／`~*` は #1787、`SIMILAR TO` は #1788 で配線済み（末尾の節）、jsonpath の `like_regex` は #2677 で配線済み（末尾の節））。上限の CLI は [regex-limits-cli.md](regex-limits-cli.md)
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
| 対象外 | CHECK（`0A000`）、索引の式・述語（`0A000`）、UDF 本体（`0A000`）、ビュー本体・JOIN・HAVING・サブクエリ・SELECT リスト（`42601`）、列・式のパターン、列・式のパターン（jsonpath の `like_regex` は #2677 で配線済み） |

PG との意図的な差分: PG は評価される行が 0 件なら不正パターンでもエラーにしないが、本実装は束縛時に `2201B`／`0A000`／`54000` で拒否する（`LIKE` の `22025` と同じ fail-closed）。

あわせて、一時表の経路（`core/temp_route.rs`）が `regex_limits` を一時コアへ引き継いでいなかった漏れを直した（運用者が CLI で下げた上限が効かない fail-open の防止）。

## SIMILAR TO への配線（Issue #1788）

ポインタ: SQL-55（正常系）・ERR-2／ERR-6（SQLSTATE 写像）。`[NOT] SIMILAR TO '<pattern>' [ESCAPE '<c>']` を `~` と同じ WHERE の式述語として受理する。PG 自身の 2 段構成（SQL 正規表現 → ARE → 正規表現エンジン）に倣い、変換器 `regex_match/similar.rs` が SQL 正規表現を ARE へ写し、既存のマッチャへ渡す。

| 項目 | 内容 |
| ---- | ---- |
| 字句 | 追加なし。`SIMILAR`・`TO`・`ESCAPE` は文脈照合の識別子で、`similar`／`to` という列名を壊さない |
| 構文 | `parse_expression_predicate` が `[NOT] SIMILAR TO` を検出し、変換済みの ARE を `pattern` に持つ `Expr::RegexMatch { case_insensitive: false }` を作る（専用の AST・束縛・評価の経路を持たず、`~` 系の束縛・評価・索引判定・台帳をそのまま共有する）。パターンは `::` の続かない文字列リテラルか裸の `NULL`。`括弧付き左辺 (…) SIMILAR TO` も値式グループとして読む |
| 変換 | 出力は `^(?:` … `)$`（全体一致）。`%`→`.*`、`_`→`.`、`(`→`(?:`、`.`・`^`・`$`・`\`→リテラル化、`\|`・`*`・`+`・`?`・`{}`・`)` はそのまま、ブラケット式は内容を通す（`\` は二重化。先頭の `]`・`[^]`・`[:alpha:]` の入れ子を扱う）。エスケープ文字＋文字は `\` ＋文字（`\d` 等の ARE エスケープも通る）。エスケープ文字＋`"` は部分抽出用の区切り（1 個目 `){1,1}?(`、2 個目 `){1,1}(?:`）。変換後の ARE は `translate` が再検証する |
| ESCAPE | 句なしは `\`。`ESCAPE ''` はエスケープなし。2 文字以上は `22025`（固定文言）。多バイト 1 文字は可。`ESCAPE NULL`・`ESCAPE $n`・`::` 付きは句として消費せず `42601`。末尾の孤立したエスケープ文字は PG と同じく無視 |
| 束縛・評価 | `~` と同じ。パターンは束縛時に 1 回コンパイル。左辺 NULL またはパターン NULL は NULL（否定の有無に関わらず）。TEXT 以外は `42883`、ICU 照合は `0A000` |
| 上限 | 生パターンのバイト長をハード上限（4096）で変換の前に検査（`54000`）。CLI で下げた上限も変換前の生パターンのバイト長に対して束縛時に検査する（変換後の ARE には別途コンパイラのサイズ制限が掛かる） |
| 拡張クエリ | 受理しない（`sql::params` に対応する形が無く、`$n` は Parse で `42601`） |
| 台帳 | `~` と同じ式タグ 21（変換後の ARE で直列化する。意味が同じなら同じ内容として扱う） |
| 対象外 | CHECK・索引の式・述語・UDF 本体（`0A000`）、ビュー本体・JOIN・HAVING・サブクエリ・SELECT リスト・CASE（`42601`）、列・式のパターン、`substring(… SIMILAR … ESCAPE …)`（SQL-55 の別項目） |

PG 17.11 との一致は `crates/engine/tests/data/similar_to_pg17.tsv`（`postgres:17`、`initdb --locale=C --encoding=UTF8`、各式に `COLLATE "C"`。入力 × パターン × ESCAPE〔句なし・`#`・`''`・`\`・`é`・記号〕× 結果の 26,000 行超）を `crates/engine/tests/similar_to_match.rs` が全行検査する。

意図的な差分:

| 差分 | 理由 |
| ---- | ---- |
| `"` 区切りが 3 個以上のとき `2201B`（PG は `2200C`） | SQLSTATE の写像表に該当行が無く、新しい分類を足さず構文誤りの分類で拒否する（fail-closed）。写像表の追加が必要かは spec 側で判断する |
| ARE として写せない構文（照合要素 `[[.a.]]`・`[[=a=]]`・語境界 `[[:<:]]`・`\u`／`\x` エスケープ等）は `0A000` | `~` と同じ方針（`regex` は線形時間と引き換えに一部の構文を持たない） |
| 評価される行が 0 件でも不正パターンは束縛時に `2201B`／`54000` で拒否 | `~` と同じ fail-closed |
| パターン長の CLI 上限は構文段でなく束縛時に検査する | 構文段は CLI 上限を持たないため、生パターンのバイト長を束縛時に `raw_pattern_len` で渡して検査する（ハード上限は構文段で検査） |

## jsonpath `like_regex` への配線（Issue #2677）

ポインタ: SQL-55・TABLE-25。親 #1784 の残り。jsonpath の `like_regex "<pattern>" [flag "<flags>"]` を、`~` と同じマッチャ（`CompiledRegex`）へ結線した。PG 17.11（`postgres:17-alpine`、`initdb --locale=C --encoding=UTF8`）との一致は `crates/engine/tests/data/jsonpath_like_regex_pg17.tsv`（フラグ × 入力 × パターン、10,707 行）を `crates/engine/tests/jsonpath_like_regex.rs` が全行照合して固定している。

| 項目 | 内容 |
| ---- | ---- |
| コンパイル時期 | 解析時（`parse_jsonpath` の `like_regex` の枝）に 1 回だけ。AST（`PathNode::LikeRegex.matcher`）が保持し、評価器はコンパイルしない。SQL 表層では `bind_path_arg` の束縛時に 1 回（行が 0 件でも `2201B`／`0A000`／`54000` が出る）。行ごとの再コンパイルや無制限のキャッシュは持たない |
| 上限の運搬 | `parse_jsonpath_with_regex_limits` が `RegexLimits` を受ける。`sql::jsonpath_fn` が `current_regex_limits()` を渡す（CLI で下げた上限が効く）。`parse_jsonpath` はハード上限 |
| 評価 | `starts with` と同型。左辺は lax で配列展開し、文字列でない項目は unknown。strict は最初の unknown で unknown、lax は最初の true で true。一致は部分一致。項目ごとにステップと走査バイト数を課金する。入力長の上限超過は抑止不可の `54000`（`RegexInputTooLong`。silent でも返る） |
| エラー | `x` フラグ（`q` なし）は `0A000`。構文誤りは `2201B`、非対応構文は `0A000`、上限超過は `54000`（`JsonPathParseError::Regex`）。文言は固定の英語で、パターンの断片を含めない |
| 件数上限 | 1 つの jsonpath が持てる `like_regex` は `MAX_JSONPATH_LIKE_REGEX`（16）個まで。超過は `54000`（`TooManyRegex`）。入力上限の中に、短くてもコンパイル後サイズが大きいパターンを多数並べる DoS を抑える意図的な差分 |

フラグの写像（`RegexSyntaxOptions` → `translate::TranslateOptions`）:

| フラグ | 写像 |
| ------ | ---- |
| なし | 改行区別（`.` と否定ブラケット `[^…]` が `\n` にマッチしない）。`\D`・`\W`・ブラケット内の否定クラスは `\n` にマッチする（PG の実測）。`^`／`$` は文字列の端だけ |
| `s` | 改行区別を外す（`~` の既定と同じ） |
| `m` | `^`／`$` が改行の直後・直前でもマッチする。`s` が無ければ改行区別も残る。`\A`・`\Z` は影響を受けない |
| `i` | ASCII 英字だけ大文字小文字を区別しない（`~*` と同じ） |
| `q` | パターン全体をリテラルとして扱う（director・埋め込みオプションも解釈しない）。`i` は併用でき、`x`・`s`・`m` は無視される |
| `x` | `q` なしは `0A000`（`q` があれば受理） |

埋め込みオプション `(?s)` はフラグを上書きして改行区別と行アンカーの両方を外す。`(?t)` は効果なし、`(?c)` は `i` を上書きする。

意図的な差分は `~` と同じ（PG が受理する後方参照・lookaround・`(?b)`／`(?e)`／`(?m)`／`(?n)`／`(?p)`／`(?w)`／`(?x)`・照合要素は `0A000`、複雑さは `54000`、非 ASCII の大文字小文字は畳み込まない）。表の照合では `intended_difference` が `q` なしの行に限って列挙する。
