# ADR: 英語ステミング（Snowball 系）クレート導入（承認資料）

- ステータス: Proposed（**オーナー承認は未取得**）。承認が記録されるまで #2129 に着手しない（第 7 節）
- 対応: Issue #2128（親 #2127。後続は #2129。#2130 の設定ファイルは依存を要さない）
- 関連ビヘイビア: SEARCH-16（関連として SEARCH-5・SEARCH-11。日本語の形態素解析は EXT-8）。ポインタ: `docs/spec/04-behavior/`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/sparse.rs`（`walk_tokens`・`tokenize_with_options`）、`crates/engine/src/sparse/match_span.rs`（`normalize_for_match`。ステミングの拡張点として恒等関数で確保済み）、`crates/wire-server/src/main.rs`（未知の引数は `unknown argument` で非 0 終了）
- 関連 doc（承認資料の先例）: [wasmtime-adoption.md](wasmtime-adoption.md)、[regex-crate-adoption.md](regex-crate-adoption.md)、[icu-collator-adoption.md](icu-collator-adoption.md)。関連: [cjk-stopword-tokenizer.md](cjk-stopword-tokenizer.md)、[fulltext-query-syntax.md](fulltext-query-syntax.md)
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景・自作しない理由

- 疎検索（BM25）のトークナイザは、小文字化・ASCII 単語・CJK のユニグラム／バイグラムで構成され、英語のステミングを持たない
- 親 #2127 が公開している方針は、英語ステミングを opt-in（cargo feature は既定 off）で提供し、実装は Snowball 系の外部クレートに限ることである。自作の簡易ステマー（Porter 系）では進めない
- Snowball の英語アルゴリズム（Porter2）を自作すると、生成元との差分追従と保守のコストが大きい
- 依存追加は dependency-policy によりユーザー承認制のため、本 ADR で承認事項を整理する

## 2. 候補の比較

数値は 2026-11-06 の crates.io API・GitHub API の取得値。依存木・ライセンス・advisory・サイズは同日の probe（第 3 節）による。`rust_version` が空欄の候補は crates.io に MSRV の記載が無い。

| 候補 | 版・ライセンス | 依存・出所・保守 | 採否の要点 |
| ---- | -------------- | ---------------- | ---------- |
| `rust-stemmers` | 1.2.0（2019-11-17）。`MIT/BSD-3-Clause` | 依存は `serde`・`serde_derive`（lock に既存）。owner は個人 `JDemler`、repo は `CurrySoftware/rust-stemmers`（archived ではない。最終 push は 2024-04-27）。累計 DL 約 4,140 万 | 事実上の標準。**最終リリースは約 7 年前**で保守は停滞。出力が長期に不変なので索引の再構築は起きにくい。**推奨（第 4 節）** |
| `qdrant-rust-stemmers` | 1.2.2（2026-02-23）、1.2.3・1.3.0（どちらも 2026-11-04）。`MIT/BSD-3-Clause` | 依存は `serde`・`serde_derive`。owner は個人 `generall`、repo は `qdrant/rust-stemmers`（GitHub 上で fork。最終 push 2026-11-04）。累計 DL 約 16 万 | rust-stemmers の保守されたフォーク。最新 2 版は公開から 2 日で、フォークの差分は**未検証**。版を選ぶなら 1.2.2 だが、差分の確認が済むまで次点 |
| `frostem` | 2.20261016.1（2026-10-17）。`BSD-3-Clause`。MSRV 1.70 | 依存 0。owner は個人 `Xuanwo`、repo は `Xuanwo/frostem`（最終 push 2026-10-17）。Snowball の main に追従し、生成元の commit を同梱の `upstream-pin.toml` に記録。言語ごとの feature を持つ。累計 DL 約 75 万 | 依存が最小で追従も活発。一方、**日付ベースの版（2.20261016.1）で更新が頻繁**であり、出力が変わる更新のたびに再承認と索引の再構築が要る。**次点（代替案）** |
| `waken_snowball` | 0.1.0（2025-07-24）。`BSD-3-Clause` | 依存 0。repo 欄は `snowballstem/snowball` だが、owner は個人 `mc373906408` | **発行者と上流組織が一致しない**。上流が crates.io へ公開した形跡は確認できていない。不採用 |
| `snowball_stemmers_rs` | 1.0.1（2026-03-10）。`MIT` | 依存は `serde`・`serde_derive`。repo は `SeekStorm/snowball-stemmers-rs` | 比較のみ。出所と実装の由来は未検証。不採用 |
| `snowstem` | 0.1.0（2026-08-24）。`MIT AND BSD-3-Clause` | 公開実績が浅い（累計 DL 約 1,400） | 比較のみ。不採用 |
| `tantivy-stemmers` | 0.4.0 | tantivy の API と多数の依存に縛られる | 不採用 |
| `stemmer` | 0.3.2（2015） | C の libstemmer の FFI。保守停止 | 不採用 |
| `porter-stemmer` | 0.1.2。`MPL-2.0`（`deny.toml` の allow 外） | Snowball ではない | 不採用 |
| 上流が生成する Rust コードの vendoring | `snowballstem/snowball` の `rust/` に `Cargo.toml`・`build.rs`・`src` がある。`BSD-3-Clause` | 依存は増えないが、生成コードの取り込みも承認対象になる。ライセンス表示の保持と手作業の更新が要る | 代替案（オーナー判断） |
| ステミングを採用しない | — | 親 #2127 の方針どおり、ステミングは未提供で指定は拒否のまま。ストップワード・同義語（#2130）は依存を要さず影響しない | 却下時の帰結 |

推奨の基準は次の 5 点。(1) 英語の Porter2 を提供し、指定する variant が特定できる（`Algorithm::English`）。(2) 新規の推移的依存が最小。(3) 発行者と上流が一致し、出所が追える。(4) 保守が続いている。(5) ライセンスが `deny.toml` の allow 内。

- `rust-stemmers` は (1)(2)(5) を満たし、(4) が弱い。(3) は owner と repo の組織名が異なる（`JDemler` と `CurrySoftware`）が、広く使われている点を根拠とする
- `frostem` は (1)(2)(4)(5) を満たし、実績が浅い。出力が頻繁に変わりうる点が運用上の負担
- 判断が分かれるのは「保守の停滞」と「更新の頻度」のどちらを重く見るか。最終的な選択はオーナーが行う

## 3. 承認事項

数値は 2026-11-06 の実測値。probe は edition 2024 の新規 bin crate をリポ外の一時ディレクトリで作成した（リポの `Cargo.toml`・`Cargo.lock` は未変更）。

| 項目 | 内容 |
| ---- | ---- |
| クレート・版 | 推奨は `rust-stemmers = "=1.2.0"`。代替は `frostem = "=2.20261016.1"`（`default-features = false` ＋ `english`）。最終的な版はオーナーが決める |
| 目的 | 英語ステミング（Snowball の `english`）を自作せずに得るため（第 1 節） |
| ライセンス | `rust-stemmers` は `MIT/BSD-3-Clause`、`frostem` は `BSD-3-Clause`。どちらも `deny.toml` の allow（`MIT`・`BSD-3-Clause`）の範囲内。probe に `deny.toml` を複製した `cargo deny check licenses advisories bans` は、advisories・bans とも ok。licenses の唯一のエラーは probe 自身がライセンス未記載であることで、候補に起因する違反は無い（`MIT/BSD-3-Clause` の旧表記は許可扱いで受理された）。`deny.toml` の変更は不要の見込み（確認は #2129 の `make deny`） |
| メンテ状況 | `rust-stemmers`: 最終リリース 2019-11-17、repo の最終 push は 2024-04-27 で、**保守は事実上停滞**。advisory は取得日時点で無し。`frostem`: 2026-09〜10 に複数版を公開し、repo の最終 push は 2026-10-17 |
| 推移的依存 | `rust-stemmers`: `cargo tree -e normal` で 11 行・重複除去後 8 crate（`serde`・`serde_core`・`serde_derive`・`proc-macro2`・`quote`・`syn`・`unicode-ident` と本体）。リポの `Cargo.lock` に無い**新規は `rust-stemmers` の 1 crate のみ**。`syn` は probe の解決が 3.0.9 でリポの lock（3.0.5）と patch 差があり、導入時の lock の増減は #2129 で確認する。`qdrant-rust-stemmers` も同じ構成。`frostem`: 依存 0 で新規は 1 crate のみ。`cargo tree -p fandhe-db-wire-server -e normal` にステマー系は**現状 0 件** |
| バイナリ増分 | strip 済み release の probe（基準 353,536 B）。英語を固定指定した最小使用は `rust-stemmers` 372,112 B（+約 18 KB）、`frostem` の `english` のみは 374,640 B（+約 21 KB）。言語名を実行時に指定する使い方（`frostem` の全言語）は 1,255,912 B（+約 0.9 MB）。**英語だけを使い、全言語の選択を公開しない**設計なら増分は小さい |
| ビルド時間 | probe の release ビルドは各 2 秒以内（依存取得済み・キャッシュ状況により変動。正確な値ではない）。dev プロファイルと CI runner での値は**未検証**（#2129 で計測） |
| 動作確認 | `rust-stemmers` の英語で `fruitlessly` を `fruitless` に、`frostem` の英語で `running` を `run` に変換することを probe で確認 |

再現手順:

1. 一時ディレクトリ（リポ外）に `cargo new` し、候補を `=x.y.z` で `cargo add` する
2. `cargo tree -e normal --prefix none` の結果とリポの `Cargo.lock` の `name =` を突き合わせ、lock 上の新規数を数える
3. リポの `deny.toml` を probe に複製し `cargo deny check licenses advisories bans` を実行する
4. `[profile.release] strip = true` で未使用版と最小使用版をビルドし、サイズを比べる
5. リポで `cargo tree -p fandhe-db-wire-server -e normal` を実行し、現状ステマー系がリンクされていないことを確認する

## 4. 決定事項（推奨案。オーナー判断事項）

1. **feature ゲート**: `optional = true`、`[features] english-stemmer = ["dep:<crate>"]`（名前は既存の `wasm-udf`・`cross-encoder` に揃える。最終名は #2129）。default には含めず、wire-server でも既定では有効にしない。既定ビルドが変わらないことは `test-default-build` ジョブと `cargo tree` で確認する
2. **承認前・feature off の挙動**: 親 #2127 が公開済みの方針どおり、ステミングの指定を起動時 CLI では非 0 終了で拒否する。表ごとの指定を設ける場合は `0A000` で拒否する。黙って無視しない。現状は指定の入口が無く、未知の引数として非 0 終了になる
3. **`--all-features` と CI 時間**: `make lint`・`make test` は `--all-features` で動くため、opt-in でも毎 PR でコンパイルされる。増分は第 3 節のとおり小さく、推奨は受け入れ。CI 上の実測は #2129 で行う
4. **承認範囲**: 直接依存 1 行の `=x.y.z` pin に限る。版の変更（セキュリティ更新を含む）と他の候補への切り替えは再承認とする。ステマーの出力が変わる版更新は索引の再構築を伴うため、この点も再承認時に確認する
5. **公開する言語**: 英語のみとし、言語名を実行時に受ける公開 API は作らない（バイナリ増分と攻撃面を小さく保つ）。他言語は別途承認

## 5. 既存コードへの接続点

- `normalize_for_match` を本文側とクエリ側の両方に通す（索引とクエリの対称性。SEARCH-5／SEARCH-16 のポインタ）
- ステミング設定を変えたら疎索引を再構築する（不一致の索引では検索しない）
- CJK トークンにはステミングを適用しない（SEARCH-16 のポインタ）
- 既存の `walk_tokens` は単語長による分割上限を持たない（文書・クエリ全体の上限〔1 MiB〕までの単一トークンが生成され得る）。ステマー入力の最大長（超過時はステミングを行わず原形を通す等）は導入時に別途定め、評価する
- 以上の詳細設計は #2129 で行う

## 6. OWASP Top 10 観点

- A06 脆弱で古いコンポーネント: `=x.y.z` で完全固定する。`rust-stemmers` はリリースが古く保守停止のリスクがある。取得日時点で advisory は無い。版の変更は再承認とする
- A08 ソフトウェアとデータの完全性（サプライチェーン）: 発行者と上流組織が一致するかを確認した。`waken_snowball` は一致しないため不採用。フォーク（`qdrant-rust-stemmers`）は差分の確認が済むまで採用しない
- A04 不安全な設計（DoS）: 既存の上限は文書・クエリ全体のみで単語長の上限は無いため、導入時にステマー入力の最大長を設けて評価し、無制限のアロケーションを増やさない
- A01 アクセス制御・テナント境界: ステミングはサーバー全体の起動時設定とし、テナントのセッションから切り替える経路を作らない。RLS 相当のフィルタ適用順序を変えない
- A05 セキュリティ設定ミス: 既定は off（opt-in feature）。feature off で指定された場合は、黙って無視せず fail-closed で拒否する
- A03 インジェクション: ステミング結果を SQL やプラン文字列の組み立てに連結しない
- spec 機密（P0）: 本 ADR は ID のポインタと、親 #2127 で公開済みの方針だけを記す

## 7. 着手停止

- オーナーの承認が記録されるまで、#2129 に着手しない
- 承認範囲外の依存・版の変更が必要になった場合も停止して再承認を得る
- #2130（ストップワード・同義語の設定）は依存を要さない。ツリー上の依存宣言の扱いはオーナー判断で、本 ADR では変更しない

## 8. 判断記録

- 2026-11-06: 自動運転で作成。オーナー承認は代筆できないためステータスを Proposed とした。**承認は未取得**。そのため現時点ではステミングを提供せず、指定は拒否のままである
- 承認後の手順: オーナーが #2127 または #2128 にコメントで承認する → ステータスを Accepted に変える → 承認日・選んだクレート／版／feature 名・コメントへのリンクを本節に追記する。欠けた項目は推奨案で補わず「未指定」と記す
- オーナーのコメントが明示すべき項目: 承認か却下か、クレートと版（`=x.y.z` の完全固定）、第 4 節 1〜5 それぞれの採否
- 却下の場合は「ステミングは未提供（指定は拒否）のまま」と記録し、#2129 の扱いはオーナーが決める
