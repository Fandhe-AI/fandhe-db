# ADR: WASM UDF 実行基盤としての wasmtime クレート導入（承認資料）

- ステータス: Accepted（2026-10-31 オーナー承認。#1911 のコメント。承認範囲は第 8 節）
  - 承認範囲内で #1924 → #1925 → #1926・#1927 → #1928・#1923 の順に着手する（第 7 節）
- 対応: Issue #1912（親 #1911。後続は #1913。承認後は #1924 → #1925 → #1926・#1927 → #1928・#1923）
- 関連ビヘイビア: EXT-9（親として EXT-5・EXT-6。契約層は TASK-149）。ポインタ: `docs/spec/04-behavior/`・`docs/spec/03-poc/udf-mechanism/`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/wasm_udf.rs`（契約層。wasmtime 型を露出しない）、`crates/engine/src/sql/udf_call.rs`（`define_wasm_function`）、`crates/engine/src/sql/mode.rs`（`register_wasm_udf`）、`crates/engine/tests/wasm_udf_contract.rs`
- 関連 doc（承認資料の先例）: [regex-crate-adoption.md](regex-crate-adoption.md)、[icu-collator-adoption.md](icu-collator-adoption.md)、[timestamptz-tz-data-adoption.md](timestamptz-tz-data-adoption.md)
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景・自作しない理由

- WASM UDF は現状、契約層（`WasmUdfBackend` trait・`SandboxLimits`・`WasmUdfError`・`TrapKind`）だけがある。実行バックエンドは依存追加の承認待ちで未実装であり、モジュールを一切実行しない
- WASM の検証・コンパイル（または解釈実行）・メモリ隔離・実行時間の中断を自作すると、保守量と安全性のリスクが依存最小方針の利点を大きく上回る
- 依存を使わない代替として `LANGUAGE sql`（#2215）と PL/pgSQL サブセット（#2222。[plpgsql-subset.md](plpgsql-subset.md)）が先行している。WASM は「SQL で書けない計算」を扱う用途に限る
- 依存追加は dependency-policy によりユーザー承認制のため、本 ADR で承認事項を整理する

## 2. 候補の比較

数値は 2026-10-29 の crates.io API 取得値。`wasmi`・`wasmer` は比較のための公開情報のみで、依存木の probe は行っていない（未検証）。

| 候補 | 版・ライセンス・MSRV | 採否の要点 |
| ---- | -------------------- | ---------- |
| `wasmtime`（Bytecode Alliance） | 50.0.0（2026-10-20）。`Apache-2.0 WITH LLVM-exception`。MSRV 1.97 | JIT（Cranelift）または Winch。epoch interruption と `StoreLimits` を持ち、契約層の設計（EXT-6 の対応）がこれを前提にする。**採用候補** |
| `wasmi` | 2.0.0（2026-09-01）。`MIT/Apache-2.0`。MSRV 1.86 | pure Rust のインタプリタ。依存は小さい見込みだが実行は遅い。時間上限は epoch でなく fuel で実現する。契約層は wasmtime 型を出さないため差し替え可能。**次点**（性能要件が緩ければ有力。依存規模は未検証） |
| `wasmer` | 7.5.0（2026-10-01）。`MIT`。MSRV 1.96 | 複数バックエンドを持ち機能面は同等だが、サンドボックス資源上限の API 契約が wasmtime ほど契約層と一致しない。不採用 |
| 自作 | — | 保守量と安全性のリスクが過大。不採用 |
| WASM を採用しない | — | `LANGUAGE sql`・PL/pgSQL で代替。却下時は親 #1911 の方針どおり #1923〜#1928 を close |

推奨は `wasmtime`。理由は、(1) 契約層が前提とする epoch interruption・`StoreLimits` を標準機能として持つ、(2) 組織管理（Bytecode Alliance）で LTS 版があり、セキュリティ修正のバックポートが保証される（第 3 節）、(3) JIT により UDF の実行性能が見込める、の 3 点。重い点は依存規模とビルド時間（第 3 節・第 4 節 3）。

## 3. 承認事項

数値は 2026-10-29 の実測値（crates.io API 取得日・probe 実施日とも同日）。probe は edition 2024 の新規 bin crate をリポ外の一時ディレクトリで作成した（リポの `Cargo.toml`・`Cargo.lock` は未変更）。

| 項目 | 内容 |
| ---- | ---- |
| クレート・版 | 推奨は `wasmtime = "=48.0.7"`（2026-10-19 公開。**LTS 系列**。承認時に `=48.0.8` へ変更。第 8 節）。最新安定版は 50.0.0（2026-10-20）だが公開から 9 日で、非 LTS（サポートは 2 か月）のため推奨しない。代替は 49.0.4（非 LTS）・36.0.19（旧 LTS）。最終的な版はオーナーが決める |
| LTS 方針 | 公式ドキュメント（`docs/stability-release.md`）による: 毎月 20 日に新メジャー版を出し、12 の倍数の版が LTS（サポート 24 か月）、それ以外は 2 か月。セキュリティ修正は全サポート版へバックポートが保証され、バグ修正は有志ベース。48 は LTS |
| 目的 | WASM UDF（EXT-5・EXT-6）の検証・コンパイル・サンドボックス実行を自作せずに得るため（第 1 節） |
| ライセンス | `Apache-2.0 WITH LLVM-exception`（`deny.toml` の allow に既存）。推移的依存（最小構成 72 crate）のライセンスは `MIT OR Apache-2.0`・`Apache-2.0 WITH LLVM-exception`・`Unlicense OR MIT`・`Zlib`・`MIT`・`(MIT OR Apache-2.0) AND Unicode-3.0`・`Apache-2.0 / MIT` で、**allow list 外のものは無し**。probe に `deny.toml` を複製して `cargo deny --all-features check licenses advisories bans` を実行し、advisories・bans は ok、licenses は probe 自身（ライセンス未記載）以外の違反なし。`deny.toml` の変更は不要の見込み（確認は #1924 の `make deny`） |
| MSRV | `rust-version = 1.95`（48.0.7）。リポの `rust-toolchain.toml`（stable 追従。手元は cargo 1.98.1）で充足 |
| メンテ状況 | bytecodealliance/wasmtime。毎月 20 日前後に新メジャー（43.0.0 は 2026-03-20、…、49.0.0 は 2026-09-21、50.0.0 は 2026-10-20）を出し、複数の系列へパッチ（2026-10-14〜19 に 36.0.19・48.0.6・48.0.7・49.0.3・49.0.4）を継続。累計 DL 約 4,200 万・直近 DL 約 1,200 万（crates.io API） |
| 既知の advisory | RUSTSEC に wasmtime の advisory が 64 件（取得した advisory-db の件数。2026 年公開が 41 件、2025 年公開が 3 件）。2026 年は 4 月・10 月に複数件がまとめて公開されており、**追従頻度が高い**。`=48.0.7` に対する `cargo deny check advisories` は ok（取得日時点）。運用上、更新のたびに再承認が要る点を第 4 節 4 と第 7 節に明記する |
| 推移的依存 | 最小構成（第 4 節 2 の案 A）の `cargo tree -e normal` は 72 crate。リポの `Cargo.lock` に無い**新規は 44 crate**（`cranelift-*`・`wasmtime-*`・`regalloc2`・`gimli`・`object`・`wasmparser`・`rustix`・`sha2`・`postcard` 等）。残り 28 は既存（`serde`・`hashbrown`・`indexmap` 等）。default features では 133 crate・新規 88 crate。`winch` 版（最小）は 74 crate・新規 46 crate。`cargo tree -p fandhe-db-wire-server -e normal` に wasmtime・cranelift 系は**現状 0 件** |
| 既存との版違い | `bitflags`・`hashbrown`・`syn` は lock が複数版を既に持つ。`libc`・`cfg-if`・`either` 等は patch 差が出うるが、`deny.toml` の `multiple-versions = "warn"` の範囲（導入時の lock 上の増減は #1924 で確認） |
| 内部の unsafe と JIT | wasmtime は JIT で実行可能メモリを書き込み、内部で `unsafe` を広く使う。リポの unsafe 原則禁止は自前コードへの規約で依存先には及ばない。security-auditor の確認観点とする |
| バイナリ増分 | strip 済み release の probe で、未使用版 349,984 B に対し最小構成（Cranelift）11,166,528 B（+約 10.8 MB）、`winch` 最小 10,188,112 B（+約 9.8 MB）、default features 13,114,784 B（+約 12.8 MB）。probe は `Engine`・epoch・`StoreLimits`・`Module::validate` を使う小さな main |
| ビルド時間 | 本マシンでの release クリーンビルド（依存取得済み）は最小構成 36.8 s、`winch` 最小 33.2 s、default features 42.1 s。dev プロファイル・CI runner での値は**未検証**（#1924 で計測） |

再現手順:

1. 一時ディレクトリ（リポ外）に `cargo new` し、`wasmtime = { version = "=48.0.7", default-features = false, features = ["runtime", "cranelift", "std"] }` を加える（比較用に `winch`・default features 版も作る）
2. `cargo tree -e normal --prefix none` の結果とリポの `Cargo.lock` の `name =` を突き合わせ、lock 上の新規数を数える
3. リポの `deny.toml` を probe に複製し `cargo deny --all-features check licenses advisories bans` を実行する
4. `[profile.release] strip = true` で未使用版と最小使用版をビルドし、サイズと時間を比べる
5. リポで `cargo tree -p fandhe-db-wire-server -e normal` を実行し、現状 wasmtime 系がリンクされていないことを確認する

feature 名は crates.io API の 48.0.7 の feature 一覧による。最小構成の `runtime`・`cranelift`・`std` で epoch interruption・`Store::limiter`・`StoreLimits` を使うコードがコンパイル・リンクできることを probe で確認した。

## 4. 決定事項（推奨案。オーナー判断事項）

1. **feature ゲート**: `optional = true`、`[features] wasm-udf = ["dep:wasmtime"]`、default には含めない。wire-server でも既定では有効にしない（親 #1911 の前提）。先例は `usearch`／`contrast-bench`、`ort`＋`tokenizers`／`cross-encoder`。#1924 での検証の拠り所は既存の `test-default-build` ジョブと、`cargo tree -p fandhe-db-wire-server -e normal` に wasmtime が出ないこと。feature off で WASM UDF を登録しようとした場合は、実行できる経路を作らず fail-closed で拒否する（SQLSTATE は `0A000` を推奨。最終確定は #1926）。ICU・regex が非 optional だったのと異なり、本件は依存規模が大きく用途も限定的なため optional を推奨する
2. **feature セット**: 案 A `default-features = false` ＋ `runtime`・`cranelift`・`std`（推奨）／案 B `runtime`・`winch`・`std`（依存は 2 crate 多いがバイナリは約 1 MB 小さい。生成コードの最適化は弱い見込み）。`wasi`・`async`・`component-model`・profiling・`cache` は不要として除く。性能要件（EXT-9 ポインタ）が緩ければ案 B も可。判断はオーナー
3. **`--all-features` と CI 時間**: `make lint`・`make test` は `--all-features` で動くため、opt-in でも wasmtime と Cranelift は毎 PR の CI でコンパイルされる。増分は release で約 37 s（本マシン。第 3 節）。推奨は受け入れ。CI 上の実測は #1924 で行い、許容を超えるなら `wasm-udf` を専用ジョブへ分離する案へ切り替える
4. **承認範囲**: `wasmtime = "=X.Y.Z"` の直接依存 1 行に限る。`wasmtime-*` サブクレートの直接依存化や版の変更（セキュリティ更新を含む）は範囲外とし、必要になれば #1924 以降を止めて再承認を得る。advisory の頻度が高いため、更新の依頼を受け付ける運用をオーナーと別途決める

## 5. サンドボックス設計と wasmtime 機能の対応

| 契約層の要素 | wasmtime での実現 |
| ------------ | ----------------- |
| `SandboxLimits::deadline_ticks`・`EPOCH_TICK_INTERVAL_MS`（既定 100 ms × 50 ティック） | epoch interruption（`Config::epoch_interruption`・`Store::set_epoch_deadline`・`Engine::increment_epoch`）。超過は `TrapKind` に写像 |
| `memory_limit_bytes` | `StoreLimitsBuilder::memory_size` ＋ `Store::limiter` |
| import の全拒否・WASI 未リンク | `Linker` を空のまま使い、import を持つモジュールは登録時に拒否。ホスト能力を付与しない |
| `MAX_MODULE_BYTES`（1 MiB） | コンパイル前に長さを上限検証する（現行の契約を維持） |
| `WasmUdfError`・`TrapKind` | 固定の英語文言。wasmtime のエラー文字列（モジュール内容を含みうる）を外へ出さない |

## 6. OWASP Top 10 観点

- A06 脆弱で古いコンポーネント: `=X.Y.Z` で完全固定する。advisory が多いため、LTS の採否・更新時の再承認・#1924 での `make deny` の通過を必須にする
- A04 不安全な設計（DoS）: `CREATE FUNCTION` 時の Cranelift コンパイルは攻撃面になる。`MAX_MODULE_BYTES` でコンパイル前に上限検証し、コンパイル時間・メモリの上限とキャッシュの設計は #1925・#1927 へ申し送る。無限ループは epoch interruption、過大メモリは `StoreLimits` で止める（fail-closed）
- A05 セキュリティ設定ミス: 既定ビルドでは off（opt-in feature）。WASI を使わず、import は 1 件でもあれば登録を拒否する。環境変数・設定で上限やホスト能力を緩める経路を作らない
- A01 アクセス制御: WASM UDF は値を計算するだけで、RLS 相当のフィルタ適用順序・テナント境界を変えない。ストレージ・テナント情報へ触れるホスト関数を渡さない。検証は #1928
- A03 インジェクション: モジュールのバイト列は非信頼入力で、SQL・プラン文字列の組み立てに連結しない
- 情報漏えい: エラーは固定の英語文言に限り、モジュール内容・行値・テナント情報を含めない（既存の契約を維持）
- unsafe／JIT: JIT が実行可能メモリを書き込むこと、依存先の内部 `unsafe` を security-auditor の確認観点とする

## 7. 着手停止

- 2026-10-31 の承認（第 8 節）により、#1923〜#1928 の着手停止は解除した。着手は承認範囲内に限る
- 第 4 節 4 の承認範囲外の依存・版の変更が必要になった場合も停止して再承認を得る

## 8. 判断記録

- 2026-10-29: 自動運転（implement-issue-tree）で作成。オーナー承認は代筆できないためステータスを Proposed とした。**承認は未取得**
- 承認後の手順（当初）: オーナーがコメントで承認 → ステータスを Accepted に変更し、承認日・承認範囲（第 4 節の番号と採用した版・feature セット）・コメントへのリンクを本節に追記する。承認済みと事実以上に書かない
- 2026-10-30: Issue #1913 の時点でオーナー判断（承認・却下）は **未取得**。#97・#1911・#1912・#1913・PR #2778 のコメントに承認・却下の記載が無いことを確認した。ステータスは Proposed のまま、第 7 節の着手停止も継続する
- 記録先の確定: オーナーの承認・却下コメントは **#1911** に付ける。#1911 は open の親 Issue で、受け入れ条件に承認可否と版の記録を持つ。#97 と #1912 は CLOSED のため、#97 は参照に留める
- オーナーのコメントが明示すべき項目（欠けた項目を推奨案で補って記録しない。欠けは「未指定」と記す）
  - 承認か却下か
  - 版（`wasmtime = "=x.y.z"` の完全固定。推奨は第 3 節の LTS 版。範囲指定は承認として扱わない）
  - feature セット（案 A／案 B）
  - 第 4 節の決定事項 1〜4 それぞれの採否
  - 却下の場合は、#1923〜#1928 の close を指示するかどうか
- 承認後の手順の更新: #1911 のコメントを根拠に、本 ADR のステータスと第 7 節・第 8 節を更新する（後続の記録 PR で実施。#1913 はそれまで open のまま残す）
- 2026-10-31: オーナーが #1911 のコメント（https://github.com/Fandhe-AI/fandhe-db/issues/1911#issuecomment-6538261249）で**承認**した。ステータスを Accepted に変更し、第 7 節の着手停止を解除した。承認内容は次のとおり
  - 版: `wasmtime = "=48.0.8"`（LTS 系列。推奨の 48.0.7 から変更。理由は RUSTSEC-2026-0422・0423・0424 の修正版であるため）
  - feature セット: 案 A（`default-features = false` ＋ `runtime`・`cranelift`・`std`）
  - 第 4 節 1（feature ゲート）: 採用。`optional = true`、`wasm-udf = ["dep:wasmtime"]`、default に含めない
  - 第 4 節 2（feature セット）: 案 A を採用
  - 第 4 節 3（`--all-features` と CI 時間）: 受け入れる。増分の実測は #1924 で行い、許容を超える場合は専用ジョブへの分離を検討する
  - 第 4 節 4（承認範囲）: 直接依存 `wasmtime = "=48.0.8"` の 1 行に限る。版の変更（セキュリティ更新を含む）とサブクレートの直接依存化は範囲外で、再承認を要する
- 2026-10-31: Issue #1924 で依存を追加した。版は承認どおり `=48.0.8`（48.0.7 から変更。理由は RUSTSEC-2026-0422・0423・0424 の修正版であるため。承認は #1911 のコメント）。`Cargo.lock` は wasmtime 系の新規エントリのみで既存エントリの版は不変。`make deny` は通過し、既定ビルドの engine・wire-server の依存木に wasmtime・cranelift は出ない
