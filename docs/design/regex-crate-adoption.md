# ADR: 正規表現エンジンとしての regex クレート導入（承認資料）

- ステータス: Accepted（2026-10-23 オーナー承認。範囲は第 9 節）
  - 承認前は #1786・#1787・#1788 および #1784 の `like_regex` 結線に着手しない運用だった（第 8 節）
- 対応: Issue #1785（親 #1784。後続は #1786 → #1787 → #1788）
- 関連ビヘイビア: SQL-55（親は TABLE-25 も。エスケープ不正は ERR-6）。ポインタ: `docs/spec/04-behavior/`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/json/jsonpath/parser.rs`・`eval.rs`（`like_regex` を構文解析後に `0A000` で拒否）、`crates/engine/src/error_format.rs`（`2201B` の `ErrorClass` は未定義）
- 関連 doc: [like-wildcard-patterns.md](like-wildcard-patterns.md)、[json-expr-type-mapping.md](json-expr-type-mapping.md)、[icu-collator-adoption.md](icu-collator-adoption.md)（承認資料の先例）、[jsonpath-limits-cli.md](jsonpath-limits-cli.md)（上限を CLI で下げる流儀）
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景・対応範囲

- 現状、`~`・`~*`・`!~`・`!~*` は字句段で拒否、`SIMILAR TO` は `42601`、jsonpath の `like_regex` は `0A000` で拒否している
- PG 17 の正規表現は ARE（バックトラックを含む実装）である。本 DB は**線形時間で終了する評価器**に限定し、扱えない構文は明示的に拒否する（親 #1784 の方針）
- 自作は Unicode クラス・大文字小文字畳み込み・最適化の保守量が大きく、依存最小方針の利点を上回る。依存追加は dependency-policy によりユーザー承認制のため、本 ADR で承認事項を整理する

## 2. 候補の比較

| 候補 | 採否の要点 |
| ---- | ---------- |
| `regex`（rust-lang） | 有限オートマトンで最悪 O(m·n) の線形時間を保証、Unicode 対応、pure Rust。**採用候補** |
| `fancy-regex` | 後方参照・先読みを持つがバックトラックで指数時間になりうる。ReDoS 対策と矛盾するため不採用 |
| `onig`（Oniguruma FFI。`tokenizers` の feature 経由で lock に既存） | C ライブラリかつバックトラック型で線形時間保証なし。`cross-encoder` 専用経路から既定ビルドへ広げない。不採用 |
| PCRE2 系 FFI | C 依存・バックトラック型。不採用 |
| 自作（Thompson NFA 等） | 保守量が過大。不採用 |

## 3. 承認事項

数値は 2026-10-23 の実測値（crates.io API 取得日も同日）。

| 項目 | 内容 |
| ---- | ---- |
| クレート | `regex = "=1.13.1"`。2026-07-15 公開。計画時点の crates.io 最新安定版で、`Cargo.lock` に既存の版と同一 |
| 目的 | 線形時間の正規表現評価を自作せずに得るため（第 2 節） |
| ライセンス | `MIT OR Apache-2.0`（本リポと同一）。推移の `regex-automata` 0.4.18・`regex-syntax` 0.8.11 も `MIT OR Apache-2.0`、`memchr` 2.8.3・`aho-corasick` 1.1.5 は `Unlicense OR MIT`。いずれも OR 式で MIT を選べるため `deny.toml` の変更は不要の見込み（確認は #1786 で `make deny` により行う） |
| MSRV | `rust-version = 1.65`。stable 追従で充足 |
| メンテ状況 | rust-lang/regex（rust-lang 組織）。1.12.2（2025-10）→ 1.12.3（2026-02）→ 1.12.4（2026-06）→ 1.13.0（2026-07）→ 1.13.1（2026-07）と継続リリース。累計 DL 約 12.9 億・直近 DL 約 2.9 億（crates.io API） |
| 推移的依存 | `regex-automata`・`regex-syntax`、default features の `perf` 経由で `aho-corasick`・`memchr`。`Cargo.lock` 上の**新規 crate は 0**（`tokenizers` 経由で既存）。ただし wire-server の既定ビルド（`cargo tree -p fandhe-db-wire-server -e normal`）には現状**いずれもリンクされておらず**、導入で新規リンクされるのは default features で 5 crate、`default-features = false` ＋ `std`・`unicode` で 3 crate（下記の probe） |
| 既存との版違い | `=1.13.1` は lock の既存版と同一のため、新たな重複版は生じない |
| 内部の unsafe | `regex-automata`・`memchr`・`aho-corasick` は性能のため内部で `unsafe` を使う。リポの unsafe 原則禁止は自前コードへの規約で依存先には及ばない。security-auditor の確認観点とする |
| バイナリ増分 | strip 済み release の probe で、未使用版 352,192 B に対し default features 版 2,106,576 B（+約 1.75 MB）、最小 features 版（`std`・`unicode`）1,530,536 B（+約 1.18 MB） |

再現手順（数値は probe の実測。probe は edition 2024 の新規 bin crate）:

1. 一時ディレクトリ（リポ外）に `cargo new` し、`regex = "=1.13.1"` だけを依存に加える
2. `cargo tree -e normal --prefix none` の結果とリポの `Cargo.lock` の `name =` を突き合わせ、lock 上の新規数を数える
3. リポで `cargo tree -p fandhe-db-wire-server -e normal` を実行し、regex 系の crate が現状リンクされていないことを確認する
4. `[profile.release] strip = true` で、Regex 未使用版・`Regex::new(..).is_match(..)` 使用版（default features／最小 features）をビルドしサイズを比較する

## 4. 決定事項（推奨案。オーナー判断事項）

1. **feature ゲートの要否**: 案 A 非 optional ／ 案 B optional ＋ `regex` feature（無効時 `0A000`）。**推奨 A**。SQL の意味論がビルド構成で変わる経路を作らないため（ICU と同じ理由）。案 B では `test-default-build` との整合が要る
2. **feature セット**: 案 A default features ／ 案 B `default-features = false` ＋ `std`・`unicode`。推奨は A（`perf` による性能と Unicode クラス）。サイズ差は第 3 節の実測（約 0.57 MB）で判断する
3. **非対応構文と構文誤りの判別方法**: `regex::Error` は構文エラーの種別を型で細分しない（**要確認（#1786）**: 1.13.1 の docs.rs で確認してから確定する）。案 a: `regex-syntax` を直接依存にして種別を検査（追加承認が要る）／案 b: #1786 の PG→regex 変換層で非対応構文を先に字句走査して `0A000` で拒否し、残りのコンパイル失敗を `2201B` にする。**推奨 b**（承認面を増やさず fail-closed）。案 a を後で選ぶ場合は再承認
4. **計算量・資源の上限の写像**: 親が求める「ステップ上限」は線形時間保証で代替し、資源は (i) パターン長上限、(ii) 入力長上限（既存の TEXT 長上限が外枠）、(iii) `RegexBuilder::size_limit`、(iv) `dfa_size_limit`、(v) `nest_limit` で抑える。超過は `54000` で fail-closed（最終確定と既定値・CLI フラグは #1786 で [jsonpath-limits-cli.md](jsonpath-limits-cli.md) に倣う）
5. **直接依存の範囲**: 承認対象は `regex = "=1.13.1"` の 1 行のみ。`regex-syntax`・`regex-automata` の直接依存化や版変更は承認範囲外（必要なら #1786 を止めて再承認）

## 5. PG 17（ARE）との構文差と拒否方針

PG の構文に関する記述は PostgreSQL 公式ドキュメント（POSIX Regular Expressions の節。公開情報）に基づく。細部は **要確認（#1786）** とし、実測による固定は #1786 の受入基準へ申し送る。

| 区分 | 例 | SQLSTATE |
| ---- | -- | -------- |
| 後方参照 | `\1`〜`\9` | `0A000`（PG 17 は受理。本 DB の意図的な差分） |
| 先読み | `(?=…)`・`(?!…)` | `0A000` |
| 後読み | `(?<=…)`・`(?<!…)` | `0A000` |
| 線形評価器へ写像しない ARE 固有構文（`***:`・`***=` の director 接頭辞、写像しない埋め込みオプション、`[[.x.]]` 照合要素、`[[=x=]]` 等価クラス） | — | `0A000`（写像するか拒否するかの最終判断は #1786。拒否が既定＝fail-closed） |
| パターンの構文の誤り | 括弧の不整合・不正な量指定 | `2201B`（PG 17 と同じ。`ErrorClass` への追加は #1786／#1787） |
| `SIMILAR TO` のエスケープ不正 | 2 文字以上のエスケープ等 | `22025`（ERR-6 ポインタ。[like-wildcard-patterns.md](like-wildcard-patterns.md) と整合） |
| 資源上限超過 | 長大パターン・`CompiledTooBig` | `54000`（第 4 節 4） |
| 承認前（現状） | 上記すべてと `like_regex` | 一律 `0A000`（現行挙動を維持） |

変換層（#1786）で写像が必要な差（ここでは列挙のみ）:

- `.` の改行マッチ: PG の既定は改行にもマッチ。`regex` 側は `(?s)` が要る
- 語境界: PG の `\m`・`\M`・`\y`・`\Y` と `regex` の `\b` 系は意味が異なる。`\B` の意味差に注意
- アンカー・エスケープ: `\A`・`\Z`・`\e`・`\cX`・`\uXXXX`・8 進などの扱い差
- 量指定の上限: PG の `{m,n}` の上限と `regex` の繰り返し上限の差
- 大文字小文字非区別（`~*`）と文字クラス: C 照合での PG の振る舞いと、`regex` の Unicode 既定（`\w`・`\d`・`(?i)` の畳み込み）の差。照合（C／ICU）ごとにどちらへ合わせるかは #1786 で PG 17 の実測により決め、合わせられない組み合わせは `0A000`
- マッチ意味論: PG は最長一致規則、`regex` は leftmost-first。真偽だけを返す `~` 系と `SIMILAR TO`（全体一致）では結果に影響しない。部分文字列抽出は親の対象外（`0A000`）のため顕在化しない

本節は差分の一覧であり、PG 17 とのパリティの証明ではない。同結果とする範囲（量指定・文字クラス・アンカー・選択・グループ）の確定と固定は #1786 に申し送る。

## 6. 線形時間保証・Unicode 対応

- `regex` は後方参照と lookaround を持たない代わりに、パターン長 m・入力長 n に対し最悪 O(m·n) で終了する（公式ドキュメント記載の保証）。入れ子の量指定（`(a*)*b` 等）でも指数時間にならない。#1786 の「病的パターンが上限内で終了する」テストの根拠とする
- `&str` を UTF-8 のコードポイント単位でマッチし、Unicode クラスと simple case folding を持つ（第 5 節の照合差の論点へ接続）
- コンパイルコスト: 定数パターンは文ごとに 1 回、列値パターン（行ごとに変わる）は有界キャッシュ等で DoS を防ぐ設計を #1787 へ申し送る

## 7. OWASP Top 10 観点

- A03 インジェクション: パターンは非信頼の SQL 入力。変換層は文字列連結でなく字句走査で構築し、未検証のまま別構文へ埋め込まない。非対応構文は `0A000` で fail-closed
- A04 不安全な設計（ReDoS・DoS）: 線形時間保証に加え、パターン長・入力長・`size_limit`・`dfa_size_limit`・`nest_limit` の上限を置く。確保は上限検証後に行う
- A05 設定ミス: 上限は CLI で下げる方向のみとし、環境変数での差し替え経路を作らない
- A06 脆弱な依存: `=1.13.1` の exact pin、rust-lang 管理で継続リリース。`make deny` の通過を #1786 で必須とする
- A01 アクセス制御: 正規表現は述語評価の意味論だけを変え、RLS 相当のフィルタ適用順序・テナント境界に触れない。エラー文にパターン断片や他テナントのデータを含めない（固定の英語文）

## 8. 着手停止

- 承認が記録されるまで #1786・#1787・#1788 と #1784 の `like_regex` 結線は着手しない。承認前の挙動（`~` 系の字句拒否・`SIMILAR TO` の `42601`・`like_regex` の `0A000`）は維持する
- 第 4 節 5 の承認範囲外の依存・版の変更が必要になった場合も停止して再承認を得る

## 9. 判断記録

- 2026-10-23: 自動運転（implement-issue-tree）で作成。オーナー承認は代筆できないためステータスを Proposed とした。**承認は未取得**
- 承認後の手順: オーナーが Issue #1785 にコメントで承認 → ステータスを Accepted に変更し、承認日・承認範囲（第 4 節の決定事項の番号）・コメントへのリンクを本節に追記する。承認済みと事実以上に書かない
- 2026-10-23: オーナーが [Issue #1785 のコメント](https://github.com/Fandhe-AI/fandhe-db/issues/1785#issuecomment-6366408726)で承認し、ステータスを Accepted とした。承認範囲は `regex = "=1.13.1"` の直接依存 1 行と、第 4 節の決定事項 1〜5 の推奨案（1: 非 optional、2: default features、3: 案 b、4: 上限超過は `54000`、5: 範囲外の変更は再承認）

## 10. #1786 での確定

- `regex::Error` は `#[non_exhaustive]` で `Syntax(String)` と `CompiledTooBig(usize)` だけを持つ。`CompiledTooBig` は `54000`、それ以外（未知の variant を含む）は拒否側（`2201B`）に倒した。`RegexBuilder` の `size_limit`・`dfa_size_limit`・`nest_limit` を使う
- 字句走査で非対応構文（`0A000`）と PG でも構文誤りになるもの（`2201B`）を先に決める第 4 節の案 b を採用した。走査が正しければ `regex` の `Syntax` は起きない（PG 17 の採取表の全行で確認済み）
- 第 5 節の差分の確定先は [regex-matcher.md](regex-matcher.md)。照合は C に限り、ICU 照合の式の拒否は #1787 の責務として申し送る
- `make deny` は通過（advisories・bans・licenses・sources）。`wire-server` の既定ビルドで新たにリンクされる crate は `regex`・`regex-automata`・`regex-syntax`・`aho-corasick`・`memchr` の 5 件（見込みどおり。`Cargo.lock` の差分は engine の依存リストへの `regex` 追加 1 行だけ）
- 上限の CLI は [regex-limits-cli.md](regex-limits-cli.md)
