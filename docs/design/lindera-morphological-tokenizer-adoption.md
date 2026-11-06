# ADR: 形態素解析トークナイザ（lindera・opt-in）の評価と導入承認資料

- ステータス: Proposed（オーナー承認は**未取得**。記録先は第 8 節）
  - 承認が無いことは却下を意味しない。却下された場合の扱いは第 2 節の「採用しない」と第 8 節に書くが、決定としては記録していない
- 対応: Issue #2132（親 #2131。後続は #2133 トークナイザの trait 化、#2134 形態素解析の実装）
- 関連ビヘイビア: EXT-8・SEARCH-11。ポインタ: `docs/spec/04-behavior/`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/sparse.rs`（`tokenize_with_options`・`walk_tokens`）、`crates/engine/tests/core_misc/cjk_tokenizer_impact.rs`（評価方法の元。コーパスと QA の生成部）
- 関連 doc: [cjk-tokenizer-impact-ja-corpus.md](cjk-tokenizer-impact-ja-corpus.md)、[cjk-stopword-tokenizer.md](cjk-stopword-tokenizer.md)、承認資料の先例 [wasmtime-adoption.md](wasmtime-adoption.md)・[regex-crate-adoption.md](regex-crate-adoption.md)・[icu-collator-adoption.md](icu-collator-adoption.md)
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景・自作しない理由

- 現状の CJK 疎検索トークナイザは文字ユニグラムとバイグラム（既定はストップワード除去 ON）だけで、形態素解析は無い
- 親 #2131 の目標は、起動時設定で方式を選べるようにしたうえで、既定はバイグラムのままにすること。形態素解析のクレートと辞書の追加は dependency-policy によりユーザー承認制で、本 ADR がその承認資料になる
- 日本語の辞書と解析器（未知語処理・コスト行列・辞書の更新）を自作する保守量は、依存最小方針の利点を大きく上回る。依存を使わない代替は現行のバイグラム
- 評価は既存の方法（決定的な合成日本語コーパス 5,000 文書・100 クエリ）を再利用した。ただしコーパスの設計が結果を強く制約する（第 4 節の考察）

## 2. 候補の比較

数値は 2026-11-06 の crates.io API・GitHub API・手元の probe による。

| 候補 | 版・ライセンス・MSRV | 採否の要点 |
| ---- | -------------------- | ---------- |
| `lindera`（+ 辞書を同梱する feature） | 6.2.0（2026-09-26）。`MIT`。MSRV 1.88 | 純 Rust の形態素解析。辞書（IPADIC・UniDic 等）をバイナリへ埋め込める。月に複数回リリースされている。**採用候補** |
| 辞書: IPADIC（`embed-ipadic`） | データは NAIST／ICOT 由来の独自の許諾文（第 3 節） | バイナリ増分が約 48 MB。複合語を長めの単位で切る。**推奨の辞書候補**（小ささのため。再現率の差は測れなかった） |
| 辞書: UniDic（`embed-unidic`） | データは UniDic Consortium の BSD-3-Clause 文（第 3 節） | バイナリ増分が約 200 MB。短い単位で切る。比較用 |
| 辞書: neologd・sudachidict 等 | 未検証 | 本 ADR の評価対象外。必要になれば別途評価する |
| 自作 | — | 保守量が過大。不採用 |
| 採用しない（バイグラム継続） | — | オーナーが却下した場合の選択肢。親 #2131 の方針どおり #2133・#2134 を close するかはオーナーが指示する。**本 ADR では決定として記録しない** |

推奨は `lindera`。理由は、(1) 純 Rust で追加の C/C++ ビルドが要らない（ただし build 時に推移的に `ring` が入る。第 3 節）、(2) 辞書を埋め込めて実行時に辞書ファイルを読まない構成が取れる、(3) リリースが活発で、2025-10 以降の 10 か月に 35 版を出している、の 3 点。重い点は辞書のバイナリ増分と、ビルド時のネットワーク取得（第 3 節）。

## 3. 承認事項

数値は 2026-11-06 の実測値（crates.io API 取得日・probe 実施日とも同日）。probe は edition 2021 の新規 bin crate をリポ外の一時ディレクトリで作成した（リポの `Cargo.toml`・`Cargo.lock` は未変更）。

| 項目 | 内容 |
| ---- | ---- |
| クレート・版 | 推奨は `lindera = "=6.2.0"`（2026-09-26 公開の最新安定版）。最終的な版はオーナーが決める。6.1.0（2026-09-25）・6.0.0（2026-08-25）・5.3.0（2026-08-16）が直前の版で、2026-08 に 5 系から 6.0.0 へ上がっており、API の変動が大きい |
| 目的 | 日本語の語単位の分割を、辞書と解析器の自作なしに opt-in で得るため（第 1 節） |
| クレートのライセンス | `lindera`・`lindera-dictionary`・`lindera-ipadic`・`lindera-unidic` はいずれも `MIT`（`deny.toml` の allow に既存） |
| 辞書データのライセンス | クレートの宣言とは別に、辞書データ自体の条項がある。cargo-deny が見るのはクレートのメタデータだけで、**辞書データの条項は検査されない**。IPADIC: 同梱 `NOTICE.txt` の NAIST／ICOT の許諾文（無保証・再配布時に同文を付ける条件。SPDX に無い独自文）。UniDic 2.1.2: 同梱 `NOTICE.txt` が UniDic Consortium の BSD-3-Clause 文（再配布時の著作権表示の条件）。いずれもコピーレフトを選ばずに済むが、バイナリを配布する場合は両 NOTICE の同梱が要る。neologd 等は未検証 |
| MSRV | `rust-version = 1.88`。リポの `rust-toolchain.toml`（stable 追従。手元は cargo 1.98.1）で充足。lindera 系は edition 2024 だが、依存側の edition はリポ（2021）に影響しない |
| メンテ状況 | lindera/lindera。2026-11-06 に push があり、アーカイブされていない。GitHub の star は約 700、open issue は 13。crates.io の累計 DL は約 320 万、直近 DL は約 140 万。2025-10 以降に 35 版（2026 年 4 月に 8 版、8 月に 7 版）。2026-08-25 に 6.0.0、09-26 に 6.2.0 |
| 既知の advisory | `cargo deny check advisories` は ok（6.2.0・取得日時点。lindera 系の advisory は無い）。依存の `memmap2`・`ring`・`rkyv`・`tar` には過去の advisory があるが、解決された版を使っている |
| `cargo deny` の結果 | `advisories ok, bans ok, licenses FAILED`。違反は `webpki-roots 1.0.11` の **`CDLA-Permissive-2.0`** の 1 件のみ（`deny.toml` の allow に無い。`ureq`→`rustls` 経由で、辞書取得用の build 依存）。`deny.toml` に allow を足す必要があり、承認とセットで行う（本 ADR は編集しない）。`CDLA-Permissive-2.0` は permissive なデータライセンスでコピーレフトではない |
| 推移的依存 | `lindera` を直接依存にした probe（`default-features = false`、`embed-ipadic`）で、通常依存（`cargo tree -e normal`）は 84 crate・リポの `Cargo.lock` に無い**新規は 31 crate**（`lindera`・`lindera-dictionary`・`lindera-ipadic`・`rkyv`・`crawdad`・`csv`・`encoding_rs`・`memmap2`・`url`・`idna`・`strum` 等）。build 依存を含めると 117 crate・新規 53 crate で、増えるのは辞書取得用の `ureq`・`rustls`・`ring`・`rustls-webpki`・`webpki-roots`・`tar`・`flate2`・`md5`・`xattr` 等。`ring` は C／アセンブリのビルドを伴う。通常依存は UniDic でも同数（build 依存は未計測） |
| 既存との版違い | 新規ではなく版が違うクレートの比較は行っていない（未検証）。`deny.toml` は `multiple-versions = "warn"` |
| ビルド時の取得 | **辞書は crates.io のクレートに入っておらず、`build.rs` が `https://Lindera.dev/` から tar.gz を取得し、MD5 だけで検証する**（`lindera-ipadic` の `build.rs` に URL と MD5 が固定されている）。IPADIC のアーカイブは 12,907,073 B。キャッシュ用の環境変数 `LINDERA_BUILD_DICTIONARY_CACHE_DIR` にアーカイブを置けば取得しない。実測は次の行 |
| オフラインのビルド | 到達不能な proxy を設定した（実際のネットワーク遮断ではない）`cargo build --offline` で、キャッシュ無しは `build.rs` が `Failed to download dictionary assets` で失敗した（exit 101）。`LINDERA_BUILD_DICTIONARY_CACHE_DIR/6.2.0-fmt2/` にアーカイブを事前配置すると同じ条件で成功した。`cargo vendor` はクレートしか含まないため、辞書の取得は別経路になる |
| 内部の unsafe と mmap | `lindera` と `lindera-ipadic` は `unsafe` 0 箇所。`lindera-dictionary` に 5 箇所（`Mmap::map`、接続コスト行列の `Send`／`Sync` 実装と `from_raw_parts`）。`lindera` の既定 feature `mmap` は `default-features = false` で外せる。埋め込み辞書は実行時にファイルを開かない。security-auditor の確認観点とする |
| バイナリ増分 | `strip = true` の release で、`lindera` を使わない版 352,048 B に対し、IPADIC 埋め込み版 48,448,480 B（+48,096,432 B。約 +45.9 MiB）、UniDic 埋め込み版 200,236,640 B（+199,884,592 B。約 +190.6 MiB） |
| 実行時メモリ | 辞書の読み込みは 0.5 ms（埋め込み。コピーなし）。評価プログラム（5,000 文書の BM25 統計を含む）の最大 RSS は IPADIC 版 51,188 kB・UniDic 版 66,716 kB。これは辞書をページ単位で読むためで、常駐量の上限ではない |
| ビルド時間 | 本マシン（Intel Core i7-13700K・10 スレッド）での release のクリーンビルド（依存取得済み・辞書の取得とビルドを含む）は、lindera を使わない版 12.1 s（依存はコンパイルする）、IPADIC 17.5 s、UniDic 35.3 s。dev プロファイルと CI runner での値は**未検証** |

再現手順:

1. 一時ディレクトリ（リポ外）に `cargo new` し、リポの `Cargo.lock` を複製して、`lindera = { version = "=6.2.0", default-features = false, features = ["embed-ipadic"] }` を加える（UniDic 版は `embed-unidic`。比較用に `lindera` を宣言するが使わない版も作る）
2. `cargo tree -e normal --prefix none`（build 依存を含める場合は `-e normal,build`）の結果とリポの `Cargo.lock` の `name =` を突き合わせ、lock 上の新規数を数える
3. リポの `deny.toml` を probe に複製し、`cargo deny --all-features check licenses advisories bans` を実行する
4. `[profile.release] strip = true` で未使用版と最小使用版をビルドし、サイズと時間を比べる
5. キャッシュ用の環境変数でオフラインのビルドを確かめる。到達不能な proxy を `HTTPS_PROXY` に設定し、`--offline` を付ける
6. 評価（第 4 節）は、`cjk_tokenizer_impact.rs` のコーパスと QA の生成部・BM25 ヘルパを probe へ移植し、`engine::sparse::tokenize_with_options` を path 依存で呼ぶ

feature 名は crates.io API の 6.2.0 の feature 一覧による。評価に使った API は `load_dictionary("embedded://ipadic")`・`Segmenter::new(Mode::Normal, dictionary, None)`・`segment`。

## 4. 評価結果

評価方法は [cjk-tokenizer-impact-ja-corpus.md](cjk-tokenizer-impact-ja-corpus.md) と同じ（シード `0x5EED_C0FF_EE42_1337`、5,000 文書、100 クエリ、BM25 は k1=1.2・b=0.75）。文書とクエリは**同じ変種**でトークン化した。lindera の出力は既存のトークナイザと同じく小文字化し、空白だけのトークンは捨てた。

**基準値の再現（ゲート）**: probe の変種 A（バイグラム・除去 ON）は、総正解数 2,047、Recall@20 の hit 数 933、Recall@100 の hit 数 1,630 を完全に再現した。変種 B（除去 OFF）も既存 ADR の 935・1,635 と一致した。この確認の後に lindera の数値を採った。

変種の定義:

| 記号 | 内容 |
| ---- | ---- |
| A | バイグラム・除去 ON（現行の既定。基準） |
| B | バイグラム・除去 OFF（参考） |
| D | lindera の表層形をすべて使う |
| E | lindera から品詞が助詞・助動詞・記号のものを落とす（除去 ON に相当する構成） |
| F | E とバイグラム A の併用 |

### 4.1 再現率・精度

理論上限は Recall@20 が 938、Recall@100 が 1,644（`Σmin(k, 正解数)`）。Precision@20 は取得した件数（20 件×100 クエリ＝2,000）に対する正解の hit 数。

| 変種 | Recall@20（hit） | Recall@100（hit） | Precision@20 |
| ---- | ---------------- | ----------------- | ------------ |
| A | 0.4558（933/2047） | 0.7963（1630/2047） | 0.4665 |
| B | 0.4568（935/2047） | 0.7987（1635/2047） | 0.4675 |
| D（IPADIC） | 0.4538（929/2047） | 0.7992（1636/2047） | 0.4645 |
| E（IPADIC） | 0.4514（924/2047） | 0.7938（1625/2047） | 0.4620 |
| F（IPADIC） | 0.4558（933/2047） | 0.7997（1637/2047） | 0.4665 |
| D（UniDic） | 0.4548（931/2047） | 0.7987（1635/2047） | 0.4655 |
| E（UniDic） | 0.4553（932/2047） | 0.7997（1637/2047） | 0.4660 |
| F（UniDic） | 0.4563（934/2047） | 0.7997（1637/2047） | 0.4670 |

### 4.2 索引規模・トークン化の時間

| 変種 | 総トークン数 | 異なり語数 | 5,000 文書のトークン化の時間 |
| ---- | ------------ | ---------- | ---------------------------- |
| A | 267,905 | 3,387 | 0.007 s |
| B | 284,232 | 3,396 | 0.006 s |
| D（IPADIC） | 83,629 | 208 | 0.024 s |
| E（IPADIC） | 62,873 | 190 | 0.036 s |
| D（UniDic） | 86,937 | 211 | 0.028 s |
| E（UniDic） | 70,042 | 184 | 0.048 s |

時間は 1 回の実行値で、BM25 統計の構築を含まない。E は品詞の取得（詳細列の分解）を含むため D より遅い。ばらつきは未計測で、桁の目安として読む。

### 4.3 考察

- **差は測れない**。全変種の hit 数は基準 A から Recall@20 で -9〜+1 件、Recall@100 で -5〜+7 件の範囲にあり、いずれも差の向きが一貫しない。基準 A は理論上限の 99.5%（933/938）に達しており、天井効果で差が出ない（既存 ADR と同じ構造）。したがって「lindera が再現率を上げる」「下げる」のどちらも、この評価からは言えない
- **コーパス設計による偏り**: 合成コーパスは辞書にある語（内容語 80 種）を区切りなしで連結し、クエリも内容語 2 語の連結である。語彙の空間が小さいためバイグラムの誤一致が起きにくく、基準 A が天井に達する。一方 lindera の分割は、語彙が辞書語と一致するかどうかに左右され、有利にも不利にもなりうる。lindera の値が低く出ても、コーパスや QA の改変による調整はしていない
- **索引規模**: lindera はトークン総数が A の 23〜33%、異なり語数が 5〜6% で、疎索引は小さくなる。バイグラムの派生トークンが無いため、語単位の検索に近い
- **速度**: トークン化は A の約 3.4〜6.9 倍の時間がかかる（0.007 s に対し 0.024〜0.048 s）。入力長の上限より前に解析器を呼ばない契約を維持する理由になる（第 6 節 A04）
- **辞書の差**: 分割の粒度が違う。IPADIC は「関西国際空港」を 1 語に、UniDic は「関西」「国際」「空港」に切る（下の定性例）。この差は合成コーパスの再現率には現れなかった

### 4.4 定性評価の例（再現率の表には含めない）

手作りの小さな入力による観察で、再現率の根拠にはしない。

| 入力 | A（バイグラム・除去 ON） | D（IPADIC） | D（UniDic） |
| ---- | ------------------------ | ----------- | ----------- |
| 東京都に住んでいる | 東 / 東京 / 京 / 京都 / 都 / 都に / に住 / … | 東京 / 都 / に / 住ん / で / いる | 同左 |
| 京都へ行く | 京 / 京都 / 都 / 都へ / へ行 / 行 / 行く / く | 京都 / へ / 行く | 同左 |
| 関西国際空港限定トートバッグ | 関 / 関西 / 西 / 西国 / … | 関西国際空港 / 限定 / トートバッグ | 関西 / 国際 / 空港 / 限定 / トート / バッグ |

- 「東京都に住んでいる」では、バイグラムは「京都」という別語のトークンを作る。lindera は「東京」「都」に切り、「京都」を作らない。語境界をまたぐ誤一致の抑止は、形態素解析の想定される利点
- 一方、IPADIC の長い単位（関西国際空港）は、部分語（空港）の検索に当たらない可能性がある。UniDic の短い単位はこれを避けるが、バイナリが約 4 倍になる。実コーパスでの再評価が要る

### 4.5 制約

- 合成コーパスによる暫定の測定で、実コーパスでの評価は未了（既存 ADR と同じ）
- 時間は 1 回の実行値・開発機での値で、CI runner の値は未検証
- 辞書は IPADIC と UniDic だけを測った。neologd・sudachidict は未検証

## 5. 決定事項（推奨案。どれもオーナーが判断する）

1. **feature ゲート**: `optional = true`、feature 名の案は `ja-morphology = ["dep:lindera"]`（辞書の feature は `lindera` の宣言側で `default-features = false, features = ["embed-ipadic"]` とする）、default には含めない。wire-server でも既定では有効にしない（親 #2131 の前提）。検証の拠り所は `test-default-build` ジョブと、既定ビルドの依存木に `lindera` が出ないこと
2. **辞書の選択と同梱方式**: IPADIC の埋め込み（推奨。バイナリ増分が小さい）。UniDic は約 200 MB の増分のため推奨しない。ネットワーク取得を避けたい場合の手段は、アーカイブを事前配置して `LINDERA_BUILD_DICTIONARY_CACHE_DIR` を指す方式（第 3 節）。取得元（`Lindera.dev`）と MD5 検証を許容するかはオーナーが判断する
3. **起動時の opt-in**: CLI フラグ名の案は `--sparse-tokenizer <bigram|morphological>`（既定は `bigram`）。feature が無効なのに `morphological` を指定したら非 0 で終了する。表ごとの指定を設ける場合は feature 無効・未許可のとき `0A000` で拒否する。黙ってバイグラムへ戻さない（#2131 の前提。EXT-8 ポインタ）。環境変数と SQL の `SET` では有効化しない
4. **方式の変更と索引**: 方式を変えたら疎索引を再構築し、不一致の索引を検索に使わない（#2131 の受け入れ条件。設計と実装は #2133・#2134）
5. **`--all-features` による CI 時間とネットワーク取得**: `make lint`・`make test` は `--all-features` で動くため、opt-in でも毎 PR の CI で辞書の取得とビルドが走る（増分は第 3 節。クリーンビルドで IPADIC 17.5 s、取得を含む）。推奨は、受け入れるか、`LINDERA_BUILD_DICTIONARY_CACHE_DIR` を CI のキャッシュに置く。許容を超える場合は専用ジョブへ分ける。CI 上の実測は #2134 で行う
6. **承認範囲**: 直接依存 `lindera = "=x.y.z"` の 1 行（`default-features = false` と辞書 feature を含む）に限る。版の変更は再承認とする。`deny.toml` への `CDLA-Permissive-2.0` の追加は承認とセットで行う（第 3 節）。IPADIC の NOTICE の同梱方法は #2134 で決める

## 6. OWASP Top 10 観点

- A06 脆弱で古いコンポーネント: `=X.Y.Z` で完全固定する。リリースが活発で版が頻繁に変わるため、更新時は再承認とする。`make deny` の通過は、`deny.toml` の更新を含めて #2134 で必須にする
- A08 ソフトウェアとデータの完全性: 辞書がビルド時にネットワークから取得され、検証は MD5 のみ（衝突耐性が弱い）。取得元はクレートのレジストリとは別のドメイン。オフラインで再現するには、アーカイブの事前配置が要る。取得物の SHA-256 をリポ側で固定する案は #2134 の検討事項とし、オーナーが方式を判断する
- A04 不安全な設計（DoS）: 解析は A の 3.4〜6.9 倍の時間がかかる（第 4 節）。入力長の上限（`MAX_DOC_BYTES`・`MAX_QUERY_BYTES`）より前に解析器を呼ばない契約を維持する。辞書の読み込みによるメモリ増は第 3 節のとおり
- A05 セキュリティ設定ミス: 既定ビルドでは off（opt-in feature）、起動時にも opt-in を要する。環境変数や SQL の `SET` で黙って有効化する経路を作らない。無効なのに指定された場合は fail-closed（非 0 で終了／`0A000`）で、黙ってバイグラムへ戻さない
- A01 アクセス制御: トークナイザは疎索引の語の分割だけを変え、RLS 相当のテナント境界やフィルタの適用順序を変えない。方式の不一致を検出したら、再構築されるまでその索引を使わない
- A03 インジェクション: 解析結果のトークンを SQL やプラン文字列の組み立てに連結しない
- 情報漏えい: エラー文言は固定の英語にし、入力テキストや辞書のパスを含めない
- ライセンスの適合: 辞書データの条項（非 SPDX を含む）は第 3 節のとおり。cargo-deny では検査されないため、NOTICE の同梱を別途確認する
- unsafe と mmap: `lindera-dictionary` の `unsafe` 5 箇所と、`mmap` feature を外す構成を security-auditor の確認観点とする

## 7. 着手停止

- オーナーの承認が記録されるまで、#2133・#2134 に着手しない（親 #2131 の方針）
- 第 5 節 6 の承認範囲外の依存・版の変更が必要になった場合も停止して再承認を得る

## 8. 判断記録

- 2026-11-06: 自動運転（implement-issue-tree）で作成。オーナー承認は代筆できないためステータスを Proposed とした。**承認は未取得**。評価は probe で実施し、リポの `Cargo.toml`・`Cargo.lock`・`deny.toml`・コードは変更していない
- 記録先: オーナーの承認・却下コメントは open の親 **#2131** に付ける。#2132 は本 ADR の PR のマージで閉じる想定のため、判断の記録先にしない
- オーナーのコメントが明示すべき項目（欠けた項目を推奨案で補って記録しない。欠けは「未指定」と記す）
  - 承認か却下か
  - 版（`lindera = "=x.y.z"` の完全固定。範囲指定は承認として扱わない）
  - 辞書（IPADIC／UniDic 等）と同梱方式
  - feature 名と CLI フラグ名
  - 第 5 節の決定事項 1〜6 それぞれの採否（`deny.toml` の `CDLA-Permissive-2.0` の許可を含む）
  - 却下の場合は、バイグラム継続とする旨と、#2133・#2134 の close を指示するかどうか
- 承認後の手順: #2131 のコメントを根拠に、別の記録 PR で本 ADR のステータスと第 7 節・第 8 節を更新する。承認済みと事実以上に書かない
