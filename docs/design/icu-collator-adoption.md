# ADR: ICU 照合データとしての icu_collator 導入（承認資料）

- ステータス: Accepted（2026-10-15 オーナー承認。記録: Issue #1608 のコメント）
  - 承認の範囲は第 4 節の決定事項 1〜4（推奨案）。決定事項 5 と版の変更は承認の範囲外（第 4 節末尾）
  - #2189・#2190・#2191 は本承認に基づいて着手してよい
- 対応: Issue #1608（親 #1607。後続は #2189 → #2190 → #2191）
- 関連ビヘイビア: TABLE-28（ポインタ: `docs/spec/04-behavior/`。本文は転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/sql/parser.rs`（TEXT の C collation 既定）、`crates/engine/src/sql/allowlist.rs`（`COLLATE` を `42601` で拒否）
- 関連 doc: [scalar-order-by-scan.md](scalar-order-by-scan.md)、[alter-table-drop-modify-column.md](alter-table-drop-modify-column.md)、[gpu-batch-wgpu-enablement.md](gpu-batch-wgpu-enablement.md)（承認事項表の先例）
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景・対応範囲

- 現状、TEXT は常にバイト列順（C collation）で比較され、`COLLATE` は構文段で `42601` として拒否されていた（Issue #1609 で `C`／`POSIX` の解析・保存に対応済み）
- 既定照合 `C`（バイト順・決定的）と `POSIX`（`C` と同義）は依存なしで実装できる
- ICU 照合（`und-x-icu`・`ja-x-icu` など）だけが照合アルゴリズムと CLDR 照合データを必要とする。依存の追加は dependency-policy によりユーザー承認制のため、本 ADR で承認事項を整理し、2026-10-15 にオーナーの承認を得た

## 2. 候補の比較

| 候補 | 採否の要点 |
| ---- | ---------- |
| ICU4X `icu_collator` | pure Rust。外部 C/C++ ライブラリへのリンク不要。CLDR データを baked で同梱できる。**採用候補** |
| ICU4C への FFI（`rust_icu` 系） | システムの libicu と C/C++ ビルドに依存し、環境ごとに ICU の版が変わって決定性・再現性が崩れる。不採用 |
| UCA と CLDR tailoring の自作 | DUCET と言語別 tailoring のデータ量・保守量が依存最小方針の利点を上回る。不採用 |
| その他の pure Rust UCA 実装（例: feruca） | CLDR の言語別 tailoring（ja など）の対応範囲は未確認。#2189 着手前に crates.io・README で事実確認する（要確認） |

## 3. 承認事項

数値は計画立案時（2026-10-15）の実測値で、再現手順は後述。

| 項目 | 内容 |
| ---- | ---- |
| クレート | `icu_collator = "=2.3.1"`（default features は `compiled_data`）。2026-08-20 公開。計画時点の crates.io 最新安定版 |
| 目的 | UCA と CLDR tailoring の自作を避け、PG の ICU 照合（`und-x-icu`・`ja-x-icu` など）に相当する比較を提供するため |
| ライセンス | Unicode-3.0。`deny.toml` の `[licenses] allow` に登録済み。dependency-policy の「MIT / Apache-2.0 互換」との関係は**オーナー判断事項**（Unicode-3.0 は OSI 承認済みの permissive ライセンス） |
| MSRV | `rust-version = 1.88`。本リポは stable 追従（計画時の実効版 rustc 1.98.1）で充足 |
| メンテ状況 | unicode-org/icu4x（Unicode Consortium）。2.0（2025-05）→ 2.1（2025-10）→ 2.2（2026-04）→ 2.3（2026-08）と継続リリース。累計 DL 約 1,044 万・直近 DL 約 484 万（crates.io API） |
| 推移的依存 | 新規 約 32 crate（下記）。proc-macro: `displaydoc`・`yoke-derive`・`zerofrom-derive`・`zerovec-derive`（+ `synstructure`） |
| 既存との版違い | `smallvec`・`syn`・`unicode-ident`（`multiple-versions = "warn"` の対象。単一化は #2189 で再確認する） |
| 内部の unsafe | `yoke`・`zerovec` は zero-copy のため内部で `unsafe` を使う。リポの unsafe 原則禁止は自前コードへの規約で依存先には及ばない。security-auditor の確認観点とする |
| 照合データ | `compiled_data` で `icu_collator_data` 等がバイナリに baked される。出典は CLDR 48.2.1・ICU release-78.1rc（`icu_collator_data` の README） |
| バイナリ増分 | 最小 probe（x86_64・rustc 1.98.1・`strip = true`・`und`／`ja` の Collator 構築と比較のみ）で 354,288 B → 1,610,248 B、**約 +1.26MB**。リポの release profile とは条件が異なる |

新規 crate（主要）: `icu_collator`、`icu_collator_data`、`icu_collections`、`icu_locale_core`、`icu_locale_fallback`、`icu_locale_fallback_data`、`icu_normalizer`、`icu_normalizer_data`、`icu_properties`、`icu_provider`、`litemap`、`potential_utf`、`stable_deref_trait`、`synstructure`、`tinystr`、`utf16_iter`、`utf8_iter`、`write16`、`writeable`、`yoke`、`yoke-derive`、`zerofrom`、`zerofrom-derive`、`zerotrie`、`zerovec`、`zerovec-derive`、`displaydoc` ほか。

### 再現手順

1. `mktemp -d` 配下で `cargo new` し、`icu_collator = "=2.3.1"` だけを追加する
2. `cargo tree -e normal,build --prefix none` の結果を、リポの `Cargo.lock` の `name =` と突き合わせて新規分を数える
3. サイズは `[profile.release] strip = true` の release build で、Collator 未使用版と使用版を比較する
4. probe の edition（2024）はリポ（2021）と異なる。probe の `Cargo.toml` はそのまま差し込める雛形ではない

## 4. 決定事項（2026-10-15 に決定事項 1〜4 は推奨案で確定）

決定事項 1〜4 は、いずれも推奨案どおりにオーナーが承認した（案 A の非 optional、`icu_collator_data = "=2.3.0"` の直接 pin、許可リストでの照合名解決、`compiled_data` での開始）。

1. **feature ゲートの要否**
   - 案 A: 非 optional（wgpu と同じ）。案 B: `optional` にして `icu-collation` feature の背後に置く（feature 無しで ICU 照合名を指定したら `0A000` で fail-closed。CI の `test-default-build` との整合も要る）
   - **確定: 案 A（非 optional）**。理由: 照合は SQL の意味論の一部で、ビルド構成により結果が変わる経路を作らないため。代償は約 +1.26MB
2. **照合データ版の固定**
   - `icu_collator_data` は `icu_collator` から `~2.3.0` で推移的に入る。`cargo update` でデータの版が変わると、永続化したソートキーの順序（#2191 の索引）が黙って変わりうる
   - **確定: `icu_collator_data = "=2.3.0"` の直接 exact pin**（`icu_normalizer_data` も同様に扱うかを #2189 で判断する）
   - データ版の変更は依存更新（承認制）としてのみ行う。索引の再構築要否と照合版のカタログ記録（PG の collversion 相当）は #2191 で設計する
3. **照合名の解決は engine 側の許可リストで行う**
   - 計画時の probe で、`xx`・`zz-Qaaa` などの未知ロケールでも `Collator::try_new` が Err を返さず root 照合へ黙って縮退すること、`C` がロケールとしてパースできない（`InvalidLanguage`）ことを確認した
   - **確定**: 照合名は固定の許可リスト（`C`・`POSIX`・`und-x-icu`・`ja-x-icu` …）で解決し、未知の名前は `42704`（#2190）で拒否する。`try_new` の失敗を拒否の根拠にしない
4. **feature セット**
   - **確定**: `default-features`（`compiled_data`）で開始する。`default-features = false` と `icu4x-datagen` による `und`＋`ja` 限定データへの縮小は、サイズが問題になったときの後続候補とする
   - ビルド時のネットワーク取得や、実行時にデータ blob ファイルを読む方式（`icu_provider_blob`）は採らない（I/O 障害点と非信頼ファイル読込みを作らないため）
5. **直接依存の範囲（承認の範囲外）**
   - ロケール構築に `icu_locale_core`（`=2.3.0`）を直接依存とするかは、`icu_collator` の再エクスポートの有無による
   - `icu_locale_core` など、決定事項 1〜4 に含まれない直接依存の追加と、承認済みの版（`icu_collator` `=2.3.1`・`icu_collator_data` `=2.3.0`）の変更は、今回の承認の範囲外である。必要になった時点で #2189 の実装を止め、改めてオーナーの承認を得る

## 5. 後続 Issue への申し送り

- **PG 17 との順序一致（#2190 の検証範囲）**: PG は ICU4C、本リポは ICU4X を使うため、CLDR／Unicode の版や tailoring の細部で順序が異なりうる。probe の実測では `ja` の既定強度で「あ」と「ア」が Equal、`und` では Less だった。一致検証は #2190 で行い、本 ADR では解決しない
- **決定性**: 照合が Equal と判定した文字列はバイト順で二次比較して全順序にする（PG の deterministic collation と同じ考え方）。`sort-determinism-check` の green を維持する
- **索引**: データ版変更時の再構築要否は #2191 で扱う。照合不一致で索引を使わず走査へ縮退する場合も、RLS 相当のフィルタの適用順序は変えない

## 6. セキュリティ観点（OWASP Top 10）

- A03: 照合名は非信頼の SQL 入力。固定の許可リストで照合し、未知の名前は `42704` で fail-closed
- A04: 比較・ソートキー生成のコストは入力長に線形で、既存の TEXT 長上限が境界。ソートキー長の上限と確保は #2190・#2191 で checked 演算と `try_reserve` を用いる。`compiled_data` のため照合器は I/O を行わない
- A05: 実行時のデータファイル読込みや環境変数による照合データの差し替えを設けない。feature ゲートを採る場合も無効時は `0A000` の fail-closed
- A06: `=2.3.1` で exact pin し、データ crate も直接 pin する（承認済み）。#2189 では `make deny` の通過を必須にする
- A01: 照合は比較の意味論だけを変え、テナント境界の経路には触れない

## 7. 判断記録

- 自動運転モードではオーナー承認を代筆できないため、Proposed で作成した
- 2026-10-15: オーナーが推奨案（決定事項 1〜4）を承認し、本 ADR を Accepted とした。記録: [Issue #1608 のコメント](https://github.com/Fandhe-AI/fandhe-db/issues/1608#issuecomment-6199758104)
  - 承認内容: `icu_collator = "=2.3.1"` の追加、`icu_collator_data = "=2.3.0"` の直接 exact pin、ライセンス Unicode-3.0 の受け入れ、feature ゲートなし（案 A）、許可リストでの解決、`compiled_data` での開始
  - 決定事項 5 とそれ以外の直接依存・版の変更は範囲外。#2189 で必要と判明した時点で止めて再承認を得る

## 付録: Issue #2189 での実施結果

- 追加した依存: `crates/engine/Cargo.toml` に `icu_collator = "=2.3.1"` と `icu_collator_data = "=2.3.0"` の 2 行（非 optional・default features）。依存の実追加は #2189 で行った
- `Cargo.lock` は新規 crate の追加のみで、既存 crate の版は変わらない
- 版違いの重複（`cargo tree -d`）: `hashbrown`（0.16.1 / 0.17.1）、`syn`（2.0.119 / 3.0.5）。`multiple-versions = "warn"` のため fail しない
- `icu_normalizer_data` は直接 pin せず、`Cargo.lock`（2.3.0）と CI の `--locked` で固定する。直接 pin は承認範囲外のため、必要ならオーナーの判断を仰ぐ
- ビルド再現性の前提: `icu_collator_data` の `build.rs` はビルド時に環境変数 `ICU4X_DATA_DIR` があると別データへ差し替える。CI・release・開発環境で設定しないこと
- `ja` などのロケール指定 Collator の構築には `icu_locale_core` の直接依存が要るため、#2190 で決定事項 5 の再承認が必要になる見込み
- 導入確認として `crates/engine/tests/icu_collator_smoke.rs` を追加した（root 照合の構築と自明な順序のみ。意味論は #2190）

## 付録: Issue #2190 での実施結果

- 決定事項 5（`icu_locale_core` の直接依存）は**再承認が不要になった**。`icu_collator` が公開する `CollatorPreferences::from_locale_strict(&Locale)` へ `"ja".parse()` を型推論で渡せるため、`icu_locale_core` を直接依存にせず `ja-x-icu` の Collator を構築できる。`Cargo.toml`・`Cargo.lock`・`deny.toml` は変更していない
- 照合名は固定の許可リストの完全一致で解決する（`C`・`POSIX`・`und-x-icu`・`ja-x-icu`）。他の ICU 名は `42704`。構文では、ハイフンを含む引用識別子のうち「先頭が英字・単一の `-` で区切った英数字語・63 バイト以下」の形だけを専用トークン（`Token::QuotedCollationName`）にし、`COLLATE` 直後でだけ受理する（他の位置では従来どおり引用識別子として使えず `42601`）
- 比較は `compare_utf8` に、`Equal` のときのバイト列順の二次比較を重ねた全順序（deterministic）。ORDER BY・GROUP BY・DISTINCT・MIN/MAX は比較器方式で、ソートキーは作らない（ヒープの候補は数回しか比較されず、ICU4X の注意書きどおりキー生成の償却が効かないため）。`Collation::sort_key`（ICU キー ‖ `0x00` ‖ 元バイト列）は #2191 が使う API として提供し、性質テストで `compare` と一致することを固定した（ICU キーが `0x00` を含まないことも検査）
- ICU4X の `CollationKeySink` は `unstable` feature の背後にあり名前を出せない（feature の追加は依存変更になる）ため、標準実装の `Vec<u8>` へ `try_reserve` で確保し、生成後に入力長の定数倍の上限と入力長の上限を検査する
- PG 17.11（ICU 有効ビルド・ICU4C）との順序一致を、基本的な 31 値のプールで `und-x-icu`・`ja-x-icu` とも確認した（`crates/engine/tests/sql_collate_icu.rs`）。漢字の順序は `und` と `ja` で異なるが（`字` と `漢` の前後）、ICU4X でも PG と同じだった。ICU4C／ICU4X の版差は残りうるため、プールは基本の文字を中心にしている
- 照合データの差し替え経路（環境変数 `ICU4X_DATA_DIR`・実行時のデータファイル）は作っていない。CI・開発環境で `ICU4X_DATA_DIR` を設定しない前提は #2189 のとおり
