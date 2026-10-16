# ADR: TIMESTAMPTZ の TZ データ源としての jiff 導入（承認資料）

- ステータス: Accepted（2026-10-16 オーナー承認。記録: Issue #1573 のコメント）
  - 推奨案どおりに確定した。#1574〜#1577 は本承認に基づき着手してよい
  - 承認の範囲外の事項（第 4 節の決定事項 5）が必要になった場合は、改めて承認を得る
- 対応: Issue #1573（親 #1572。後続は #1574 → #1575 → #1576、wire は #1577）
- 関連ビヘイビア: TABLE-33（ポインタ: `docs/spec/04-behavior/`。本文は転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 doc: [datetime-column.md](datetime-column.md)（D-4）、[datetime-scalar-functions.md](datetime-scalar-functions.md)（`now()` 系の戻り値型は暫定で TIMESTAMP）、[icu-collator-adoption.md](icu-collator-adoption.md)（承認事項表の先例）
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景・対応範囲

- 現状の日付時刻型は DATE・TIMESTAMP（naive）・TIME のみで、TIMESTAMPTZ は意図的に対象外である（datetime-column.md の D-4）
- 固定オフセット（`+09:00`）は依存なしで実装できる。IANA の名前付き TZ（例: `Asia/Tokyo`、`America/New_York` の DST 境界）の規則評価にだけ、tz データベースが要る
- 依存を使うのは engine だけである。OID 1184 のバイナリ形式は UTC のマイクロ秒（i64）なので、wire-server は依存を追加しない
- 依存の追加は dependency-policy によりユーザー承認制のため、本 ADR で承認事項を整理する

## 2. 候補の比較

| 候補 | 採否の要点 |
| ---- | ---------- |
| jiff | pure Rust。tzdata の同梱と OS 参照を feature で選べ、推奨構成の推移的依存は 2 crate のみ。**採用候補** |
| chrono + chrono-tz | tz 規則をビルド時に生成するため依存とコード生成が多く、naive と aware の型が別系統になる。不採用 |
| time + time-tz | tz 部分のメンテ状況と依存範囲の事実確認が要る（要確認）。現時点では採らない |
| TZif パーサの自作 + OS の tzdata 参照 | 依存は増えないが、POSIX TZ 文字列と遷移規則の評価を自前保守する。ホストごとに tzdata の版が変わり決定性が崩れ、実行時に非信頼ファイルを読む経路もできる。不採用 |
| 自作パーサ + IANA 同梱 | tzdata の更新追従を自前で行う負担が依存最小方針の利点を上回る。不採用 |

## 3. 承認事項

数値は 2026-10-15 の実測値（crates.io API と scratchpad 上の probe）で、再現手順は後述。

| 項目 | 内容 |
| ---- | ---- |
| クレート | `jiff = "=0.2.38"`（`default-features = false`、features は `std`・`tzdb-bundle-always`）。2026-10-06 公開の最新安定版。0.2.36 は yanked |
| 目的 | TZif 規則・DST 遷移・IANA 名の解決を自作せず、名前付き TZ の入力解釈と表示（セッション TimeZone・`AT TIME ZONE`）を提供するため |
| ライセンス | jiff・jiff-core・jiff-tzdb とも `Unlicense OR MIT`。MIT を選べば dependency-policy の MIT 互換を満たす。`deny.toml` の `[licenses] allow` に MIT・Unlicense とも登録済み |
| MSRV | `rust-version = 1.70`。本リポの stable（計画時の実効版 rustc 1.98.1）で充足 |
| メンテ状況 | BurntSushi/jiff。0.2.33〜0.2.38 を 2026-07〜10 に継続公開。累計 DL 約 2.28 億・直近 DL 約 8,574 万（crates.io API） |
| 推移的依存 | 推奨構成で `jiff`・`jiff-core 0.1.1`・`jiff-tzdb 0.1.9` の 3 crate のみ（proc-macro なし） |
| 既存との版違い | #1574 で `cargo tree -d` により確認する（未実施） |
| 内部の unsafe | `jiff` の `src` に `unsafe` を含む行が 79 行、`jiff-core` に 2 行（コメント・属性を含む grep 件数）。自前コードへの unsafe 原則禁止は依存先に及ばない。security-auditor の確認観点とする |
| tz データ | `jiff-tzdb 0.1.9` が IANA tzdata **2026e** を `concatenated-zoneinfo.dat` としてバイナリに埋め込む |
| バイナリ増分 | 最小 probe（x86_64・rustc 1.98.1・`strip = true`）で jiff 未使用 349,984 B → `bundled().get()` と変換を呼ぶ版 885,456 B、**約 +0.54MB**。リポの release profile とは条件が異なる |
| Cargo.lock | probe の lock は 21 エントリ（自 crate と optional 依存の記載を含む）。リポの lock との新規分の差分は #1574 で数える（未実施） |

jiff の既定 features（`tz-system`・`tzdb-zoneinfo`・`tzdb-concatenated`・`tzdb-bundle-platform` など）では、Linux で `/usr/share/zoneinfo`（`TZDIR` で上書き可）と `TZ` 環境変数を実行時に読み、tzdata の同梱は Windows・wasm のときだけになる。この挙動を避けるため、承認した構成は features を明示する。

### 再現手順

1. `mktemp -d` 配下で `cargo new` し、`jiff = { version = "=0.2.38", default-features = false, features = ["std", "tzdb-bundle-always"] }` だけを追加する
2. `cargo tree -e normal,build --prefix none` で依存を列挙する
3. `[profile.release] strip = true` の release build で、jiff 未使用版と `TimeZoneDatabase::bundled().get(...)` を呼ぶ版のサイズを比較する
4. probe の edition（2024）はリポ（2021）と異なる。probe の `Cargo.toml` はそのまま差し込める雛形ではない

## 4. 決定事項（推奨案どおり確定。2026-10-16 オーナー承認）

決定事項 1〜4 は推奨案どおりに確定した。決定事項 5 は承認の範囲外として扱う。

1. **tz データの持ち方（中心論点）**
   - 確定: **バイナリに同梱**する。`tz-system`・`tzdb-zoneinfo`・`tzdb-concatenated`・`tzdb-bundle-platform` は意図的に無効にする
   - engine は `jiff::tz::db()`（グローバル）ではなく `TimeZoneDatabase::bundled()` を明示して使う
   - 理由: ホストごとの tzdata 版の差による非決定性、実行時のファイル I/O 障害点、環境変数（`TZ`・`TZDIR`）による差し替えを作らないため（icu-collator-adoption.md の A05 と同じ立場）。PG もビルド時に同梱した tzdata を既定とする
   - `tz-fat`・`perf-inline` は性能にだけ関わる。採否は #1574 で計測して判断し、承認の範囲内で扱う
2. **データ版の固定**
   - 確定: `jiff-tzdb = "=0.1.9"` を直接 exact pin する（ICU の `icu_collator_data` と同じ扱い）。jiff は jiff-tzdb を `^0.1.9` で引き込むため、`cargo update` で tzdata の版が黙って変わりうる
   - tzdata の版の変更は依存更新（承認制）としてのみ行う
   - TIMESTAMPTZ は UTC で保存するため、版の変更は格納値に影響しない。影響するのは名前付き TZ の入力解釈と表示だけで、索引の再構築は不要と見込む
3. **feature ゲートの要否**
   - 確定: 非 optional（ICU の案 A と同じ）。型の意味論がビルド構成で変わる経路を作らないため。代償は約 +0.54MB
4. **TZ 名の解決と拒否**
   - TZ 名は非信頼入力（SQL・StartupMessage・`SET`）。長さ上限を検証してから `bundled().get()` で引き、`Err` は fail-closed で拒否する。SQLSTATE は #1575 で PG 17 と照合して確定する（PG では `22023` 系の見込み）
   - `get()` は ASCII の大文字小文字を区別せず、`UTC` と `Etc/Unknown` を特別に扱う。`Etc/Unknown` は `Err` を返さず `TimeZone::unknown()` になるため、engine 側で明示的に拒否する
   - 正規名を表示（`SHOW timezone`）するときの扱いは #1575 へ申し送る
   - `TimeZone::system()`・`try_system()` は使わない（`tz-system` 無効では `Err`）。セッション TimeZone の既定値は #1575 で決める
5. **承認の範囲外**
   - `jiff-core` の直接依存化、上記以外の features（`serde`・`static` など）、承認した版（`jiff` `=0.2.38`・`jiff-tzdb` `=0.1.9`）の変更は範囲外とする。必要になった時点で #1574 の実装を止め、改めて承認を得る

## 5. 後続 Issue への申し送り

- **#1574**: 依存の実際の追加、`make deny` の通過、release でのサイズ再確認、`Cargo.lock` の新規分と `cargo tree -d` の確認
- **#1575**: POSIX 形式の TZ 文字列（例: `EST5EDT`、`+09` の符号が逆になる PG の解釈）、略称の扱い、不正な名前の SQLSTATE を PG 17 と照合する
- **#1576**: DST の gap（存在しない時刻）と fold（重複する時刻）の解決を、jiff の既定値に任せず明示オプション（Disambiguation）で選び、PG 17 の挙動と比べてテストで固定する
- **#1577**: wire は依存を追加しない
- **sort-determinism-check の維持**: TIMESTAMPTZ の比較は UTC の i64 で行う

## 6. セキュリティ観点（OWASP Top 10）

| 観点 | 内容 |
| ---- | ---- |
| A01 | TZ はテナント境界の経路に触れない。RLS 相当のフィルタの適用順序は変えない |
| A03 | TZ 名・オフセット文字列は非信頼入力。長さ上限、許可された文法、DB 照合の順に検証し、未知の値は fail-closed。SQL 文字列への連結はしない |
| A04 | 名前の照合コストは名前の長さに比例する。セッションごとの TZ は値として保持し、無制限の確保をしない |
| A05 | 実行時の tzdata ファイル読み込みと、`TZ`・`TZDIR` による差し替えを設けない（features で無効化し `bundled()` を明示する） |
| A06 | `=0.2.38`・`=0.1.9` で exact pin する（承認済み）。#1574 で `make deny` の通過を必須にする。0.2.36 が yanked である事実を踏まえ版を確認する |
| A09 | エラー応答には入力の TZ 名を反映するだけにし、他テナントの情報を含めない |

## 7. 判断記録

- 自動運転モードではオーナー承認を代筆できないため、Proposed で作成した
- 2026-10-16 にオーナーが推奨案を承認し、Accepted に更新した。
  承認記録: [Issue #1573 のコメント](https://github.com/Fandhe-AI/fandhe-db/issues/1573#issuecomment-6206985366)
- 承認の範囲外は、`jiff-core` の直接依存化、上記以外の features、承認した版の変更である
  （`tz-fat`・`perf-inline` は #1574 の計測で判断し、範囲内とする）
