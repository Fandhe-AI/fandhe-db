# vector-db

Rust 製のローカルファースト・vector 特化クエリ DB の実装リポジトリです。「正解を含むデータ群を広く返す」広域検索（`recall` モード・既定）を設計思想の中心（差別化ポイント）とし、LLM のコンテキストとして渡す用途に最適化します。「正確なデータ 1 件のピンポイント抽出」（`precision` モード）への切り替えも提供します。

## 位置づけ

- **本リポジトリは public** です（rust-ai-library と同一方針）
- **仕様・ビヘイビア定義**: [vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec)（`docs/spec` に submodule 参照。**private リポジトリとして意図的に非公開を維持**する方針であり、アクセス権のない環境からは submodule を解決できません）
- Web API・MCP サーバーはいずれも別プロダクトとして本リポジトリのスコープ外です

## ステータス

実装は未着手です（ロードマップの着手判定待ち）。タスク定義は spec リポの [`05-tasks.md`](https://github.com/Fandhe-AI/vector-db-spec/blob/main/05-tasks.md)（TASK-66〜165・100 件）、マイルストーンは [`06-roadmap.md`](https://github.com/Fandhe-AI/vector-db-spec/blob/main/06-roadmap.md)（MS-1〜6）を参照してください。

## 実装方針（要点）

- **接続プロトコル**: PostgreSQL wire プロトコル v3 互換の**自作実装**（`pgwire` 等の外部ライブラリへ可能な限り依存しない）。psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済み
- **クエリ表層**: 標準クエリカタログ C1〜C5 を MVP とする vector 特化 SQL（C6 集計・C7 結合は拡張扱い）。LLM クエリプランニングは専用構文 `USING PLAN(...)` で SQL に露出
- **検索モード**: `recall`（広域・既定）／`precision`（ピンポイント抽出）の切り替えを提供。切替手段・実行契約の詳細は spec のビヘイビア定義（SQL-12・SEARCH-9/10・PLAN-11・TASK-161〜165）を参照
- **広域取得（ソートなしのフィルタ取得）**: `ORDER BY`／`USING PLAN` を伴わない `SELECT ... [WHERE ...] LIMIT n` を SQL 表層へ追加（`Statement::Scan`。Issue #454）。ランキング段・取得モードを持たず、可視かつ `WHERE` を満たす行を先頭から `LIMIT` 件返す（順序保証なし・早期終了）。契約の詳細（spec 側では SQL-15・TASK-170 として付与済み〔vector-db-spec#12〕。受け入れ確認・確定化は TASK-170 が担う。閾値等は実装既定値）は [`docs/design/wide-retrieval-scan.md`](docs/design/wide-retrieval-scan.md) を参照
- **クレート構成**: `engine`（コアロジック: データロード・検索カーネル・認証・RLS）＋ `wire-server`（バイナリ）の workspace 構成（TASK-66 で雛形を構築済み。各機能の実装は後続タスク）
- **永続化**: `redb` ベース（単一ライタ・スナップショット読み取り。並行書き込み検証は MS-1 の TASK-144）
- **安全性**: RLS 相当のテナント境界・fail-closed のエラー契約（SQLSTATE 風 `wire_code`）
- **検索結果順序**: スコア順 Top-k・RRF 融合結果はいずれもスコア降順・同点は id 昇順で決定的（判断根拠は [`docs/design/rrf-tie-break-determinism.md`](docs/design/rrf-tie-break-determinism.md)）。ただし複数テナントを 1 バッチで扱うバッチ検索経路（`batch_search.rs`）では、同点タイブレークは常駐行列の行スロット昇順であり、行を `(tenant_id, id)` キー順（`Storage` の行キー順）で常駐行列へ渡すという事前条件のもとで `(tenant_id, id)` 昇順になる（単一テナント内では従来どおり id 昇順。CPU 経路・GPU 経路とも同一）
- **依存最小方針**: 依存の追加・更新は必ずユーザー承認を経て行い、`=x.y.z` 完全固定で管理する
- **通信路暗号化（TLS 1.3・opt-in）**: 外部クレートを追加しない自作 TLS 1.3（X25519・`TLS_AES_128_GCM_SHA256`・Ed25519 証明書。TASK-228・WIRE-9・HTTP-10 ポインタ。実装リスクはセキュリティ監査〔Issue #971〕の実施を条件にオーナー判断で受容済み）。`--tls-cert <PEM> --tls-key <Ed25519 PKCS#8 PEM>` で有効化し、未指定時は従来どおり平文・loopback 限定のまま。`--tls-mode`（既定 `require`）で非ループバック bind の可否を制御し、`--surface nosql` では HTTPS 終端として同じ設定を共有する。クライアント側は loopback 限定の開発時は `sslmode=require` で接続できるが、`sslmode=require` は通信路の暗号化のみでサーバー証明書検証を行わないため中間者を正規サーバーと誤認しうる。**非ループバック運用では信頼する CA 証明書を `sslrootcert` に設定した `sslmode=verify-full` を使用する**こと。SCRAM-SHA-256-PLUS の提示は既定無効。詳細は下記「wire-server の起動（TASK-73）」節・[`docs/design/tls-wire-connection.md`](docs/design/tls-wire-connection.md)・[`docs/design/tls-scram-design.md`](docs/design/tls-scram-design.md) を参照
- **バッチ検索の GPU 経路**: 一括インデクシング専用のバッチ検索（TASK-128〜130）は `wgpu`（=30.0.1・依存追加はオーナー承認済み〔2026-08-26〕）による実 GPU バックエンドを持ち、初期化失敗・実行時エラー時は CPU-SIMD 経路へ fail-closed に縮退する（詳細: [`docs/design/gpu-batch-wgpu-enablement.md`](docs/design/gpu-batch-wgpu-enablement.md)）。単発クエリ経路は引き続き CPU-SIMD のみ。GPU 側 workgroup 内部分 Top-k（共有メモリ上の bitonic ソート網＋CPU 側 `TopKSelector` 最終マージ）は [`docs/design/gpu-batch-topk.md`](docs/design/gpu-batch-topk.md)（Issue #535 で設計・#536 で実装済み・#537 で前後比較実測済み。readback バイト数は
12.66〜12.79x 削減を確定的カウンタで確認し、ADR ステータスは Accepted）
- **hybrid 検索の疎索引**: BM25 疎索引（`SparseIndex`）は転置索引（posting list）＋可視ビットマップ 1 パス走査方式で、RLS 可視集合へ統計（df・N・avgdl）自体を縮約する fail-closed 設計（posting へのスコアリング走査のみがコーパス文書数への線形走査から脱却し、可視集合走査 `O(|visible_ids|)`・スコアアキュムレータ初期化 `O(N)` は残る。詳細: [`docs/design/sparse-inverted-index.md`](docs/design/sparse-inverted-index.md)）
- **ANN 索引（opt-in）**: 既定の検索エンジンは厳密最近傍（brute-force）のまま不変。`SearchEngineKind::Hnsw`（自作 HNSW・依存追加なし）を明示的に選択したときのみ opt-in で有効化される（ADR: [`docs/design/ann-index-adoption.md`](docs/design/ann-index-adoption.md) B 案）。適用状況は `EXPLAIN` の `engine:`／`ann_plan:` 行で確認できる。前後比較・opt-in 手順の詳細は下記「ANN（HNSW）opt-in 手順と前後比較（Issue #413）」節を参照。索引ノードの f16 常駐（`ResidentPrecision::F16`。既定 f32・opt-in）と F16C／NEON fp16 デコード付き dot カーネルは [`docs/design/hnsw-f16-resident.md`](docs/design/hnsw-f16-resident.md)（Issue #514）を参照。索引ノードの対称 SQ8（i8）常駐（`ResidentPrecision::I8`。既定 f32・opt-in。次元ごと min/max 由来のスケールで凍結時に 1 回量子化し、索引ヒットの最終スコアは常に f32 アリーナ再計算のまま不変）は [`docs/design/hnsw-sq8-resident.md`](docs/design/hnsw-sq8-resident.md)（Issue #521）を参照。同索引ノードの整数 i8×i8 dot カーネル（VNNI 512bit／256bit・i16 widen フォールバック。ISA 間ビット一致）は同 doc「Issue #522」節を、aarch64 NEON dotprod（`vdotq_s32`）版は同 doc「Issue #525」節を参照
- **他実装比較・チップ別カーネル設計指針**: 他実装のホットパス手法・採否候補・ライセンス帰属は [`docs/design/hotpath-implementation-survey.md`](docs/design/hotpath-implementation-survey.md)、チップ別設計指針と Rust stable での intrinsics 可用性は [`docs/design/chip-kernel-guidelines.md`](docs/design/chip-kernel-guidelines.md)（いずれも調査記録・採用決定は各 Phase Issue）。intrinsics 導入方針 ADR（unsafe 境界・set 構築ロード・ディスパッチ設計・toolchain 1.98・適用経路）は [`docs/design/simd-intrinsics-adoption.md`](docs/design/simd-intrinsics-adoption.md)（Issue #508・ステータス Proposed・オーナー承認待ち）。`isa.rs::dot_lanes` の零埋め固定長バッファによる分岐なし tail（AVX2／AVX-512／NEON。順序保存・既定経路は現行のスカラー tail のまま不変）は [`docs/design/dot-kernel-branchless-tail.md`](docs/design/dot-kernel-branchless-tail.md)（Issue #528。既定切替の採否は Issue #529 で dim 100／129／768 の前後比較実測により Rejected・現状維持確定。`BENCH_DOT_KERNEL_TAIL_AB=1 make bench-dot-kernel` で opt-in の tail A/B 実測を再現可能）。`search_range` の 4 行ブロック（AVX2+FMA／AVX-512F／NEON）カーネルの設計・生成コード検査で判明した SLP 再パック問題と対処は [`docs/design/dot-kernel-row-block.md`](docs/design/dot-kernel-row-block.md)（Issue #510・#511 実装済み。前後比較・採否記録は Issue #512（`BENCH_DOT_KERNEL_BLOCK_AB=1 make bench-dot-kernel`。「参考値・現状維持」。詳細は [`docs/design/dot-kernel-multi-accumulator.md`](docs/design/dot-kernel-multi-accumulator.md)「行間再利用（Issue #512）」節）実施済み。dim 閾値ディスパッチ（Issue #517・#518。既定閾値 768）の前後比較・閾値候補実測は
`scripts/bench_dot_kernel_ab.sh`（`dot_kernel_bench` の before/after 交互 min-of-N 実行ドライバ）で Issue #519 が本環境（共有 QEMU）の参考値を実測済み（詳細:
`docs/design/dot-kernel-multi-accumulator.md`「Issue #519 追記」節）。Phase 4（チップ最適カーネル群 #459）通しの着手前 SHA/適用後 SHA 前後比較・チップ別最速判定は [`docs/design/phase4-chip-before-after.md`](docs/design/phase4-chip-before-after.md)（Issue #530。`scripts/bench_chip_ab.sh`・`make bench-chip-ab` で本開発環境〔共有 QEMU〕参考値を実測済み。Apple M／AMD Zen／Intel 実機での実測はオーナー申し送り）

詳細なビヘイビア（106 件・12 領域）は spec リポの [`04-behavior/`](https://github.com/Fandhe-AI/vector-db-spec/tree/main/04-behavior) を唯一の正（SSOT）とします。

## 開発環境構築

```bash
git clone git@github.com:Fandhe-AI/vector-db.git
cd vector-db
make setup   # サブモジュール → rustup → lefthook（git hooks）を一括構築
```

`docs/spec`（`vector-db-spec`）は private リポジトリのため、アクセス権のない環境では submodule 取得が失敗します（`make setup` は警告のみで継続します）。実装コードのビルド・テストは `docs/spec` 抜きでも成立するよう維持します。

### タスクランナー（Makefile）

| コマンド | 内容 |
| -------- | ---- |
| `make setup` | 開発環境の一括構築（submodule → rustup → lefthook） |
| `make ci` | CI（`.github/workflows/ci.yml`）と同等のチェックをローカル一括実行 |
| `make lint-docs` | ドキュメント／設定ファイル系 lint（markdownlint・yamllint・editorconfig-checker・commitlint） |
| `make fmt` / `make fmt-check` / `make lint` / `make test` / `make deny` | Rust 系チェック（workspace 追加により有効化済み） |
| `make test-default-build` | 既定ビルド（feature `fault-injection` 無効）専用の回帰テスト（`ci` に含む）。`make test` は `--all-features` のため対象外のテストをピンポイント実行 |
| `make docker-build` / `make docker-shell` / `make docker-ci` | Docker による環境非依存の開発・検証（`compose.yaml` 参照） |
| `make bench-simd` / `make bench-c1` / `make recall-regression` / `make precision-regression` | 時間依存・spec 閾値依存の回帰チェック（`ci` には含めない。`.github/workflows/bench.yml`・`recall.yml` から実行。`precision-regression` は目標値未確定のため `recall.yml` へ未接続。詳細は下記「`precision` 評価ハーネス」参照） |
| `make precision-report` | TASK-163 の判断材料レポート・パラメータ感度スイープ（実測値を標準出力へ出すため**ローカル専用**。CI・GitHub Actions からは実行しない） |
| `make e2e-three-client` | TASK-73（WIRE-1）／TASK-165（SQL-12・SEARCH-9）／TASK-168（SQL-13・SQL-14）／TASK-153（ERR-5。commit 後 panic 緊急応答の 3 クライアント detail 到達検証）実 `psql`／`psycopg`／`pg` クライアント統合テスト（`ci` には含めない opt-in。要 `psql`・`python3`+`psycopg`・`node`+`pg`。`PSQL_BIN`/`PYTHON_BIN`/`NODE_BIN` で上書き可） |
| `make e2e-three-client-tls` | TASK-228（WIRE-9・WIRE-1）Issue #969: TLS 1.3 越しの実 `psql`／`psycopg`／`pg` クライアント統合テスト（`ci` には含めない opt-in。要 `psql`・`python3`+`psycopg`・`node`+`pg`。`PSQL_BIN`/`PYTHON_BIN`/`NODE_BIN` で上書き可。証明書・鍵は実行時に生成しリポジトリへは置かない） |
| `make e2e-three-client-http` | TASK-183（HTTP-13）NoSQL 表層（`--surface nosql`）の無改造 HTTP クライアント（curl／urllib／fetch）統合テスト（`ci` には含めない opt-in。要 `curl`・`python3`・`node`（≥18）。`CURL_BIN`／`PYTHON_BIN`／`NODE_BIN` で上書き可）。実行記録（`[e2e-record]`）を stderr へ出力する。PR 記録様式は `docs/design/three-client-e2e-harness.md` 参照 |

ターゲット一覧は `make help` で確認できます。

### wire-server の起動（TASK-73）

```bash
cargo run -p fandhe-vector-db-wire-server -- --users <ユーザーストアのパス> --db <redb ファイルのパス> \
  [--bind 127.0.0.1:5432] [--surface sql|nosql] \
  [--search-engine default|hnsw|hnsw_f16|hnsw_i8] \
  [--hnsw-full-scan-ratio <num>/<den>] \
  [--hnsw-acorn-max-visible-ratio <num>/<den>] \
  [--hnsw-sparse-visited-max <N>] \
  [--durability immediate|none] \
  [--ddl-allowed-users <user1>[,<user2>...]] \
  [--auth-method cleartext|scram-sha-256] \
  [--scram-mock-key-file <path>] \
  [--tls-cert <pem> --tls-key <pem> [--tls-mode require|allow] \
   [--tls-scram-channel-binding enable|disable]]
```

`--users`・`--db` はいずれも必須です（省略時は匿名ログイン・匿名 DB を暗黙生成せず
fail-closed で起動を拒否します）。`--bind` 省略時は `127.0.0.1:5432`。psql・
psycopg・node pg から無改造で cleartext password 認証つき接続できます
（詳細: `docs/design/three-client-e2e-harness.md`）。

`--surface`（Issue #734・#735・TASK-171／HTTP-1・HTTP-9）はクエリ
インターフェースを SQL 表層（現行の PostgreSQL wire プロトコル）／NoSQL
表層（HTTP/1.1 最小サブセット。TASK-172 以降）の 2 択で排他選択する opt-in
CLI 引数です。未指定（または `sql`）は現行どおり SQL wire のまま不変です。
`sql`／`nosql` 以外の値・値欠落・2 回目以降の重複指定はいずれも fail-closed
で起動エラーとなり、既定へ黙って読み替わることはありません。両表層とも
bind アドレスの通信路保護要件検証（TLS 未構成時は loopback 限定。WIRE-7）を
共有した後にリスナーだけが分岐するため、`nosql` 選択時も非ループバック
bind は同じ理由で起動拒否されます。`nosql` を選択すると、選ばれていない
表層（SQL wire）のリスナーは一切 bind されず、代わりに HTTP/1.1 の
リスナーが 1 本だけ bind されます。読み取り 30 秒タイムアウト・同時接続数 64
（超過時は HTTP 503／`wire_code` `53300`）は SQL wire と同一契約で適用済み
です（Issue #743）。要求行・ヘッダ・本文の読み取りと解析、応答の書き込み・
不正フレーム時の有界な読み捨てクローズ、接続ハンドラ本体の `catch_unwind`
による panic 隔離は接続ハンドラ本体（Issue #747）として実装済みです。
`POST /v1/session`（Argon2id 照合・SQL wire と同一の固定遅延・ダミー KDF
対称性・セッション発行。Issue #752・TASK-174・HTTP-4・HTTP-6）・
`POST /v1/session/close`（`Authorization: Bearer` の受信データ経路検証・
ワンタイム失効。Issue #753・TASK-174・HTTP-8）は実装済みです。
`Authorization: Bearer` の欠落・重複・スキーム不一致・トークン不正・
失効済み・二重 close はいずれも `wire_code` `28000`（HTTP 401・
`WWW-Authenticate: Bearer` 付き）へ集約され、区別可能な情報は一切
返しません。`POST /v1/query` は `Authorization: Bearer` 検証（同じ `28000`
集約）を通過した要求のみを受け付け、クライアント自己申告の `tenant_id`
相当（JSON の未知キー・ヘッダ・パス位置のいずれも）は `wire_code` `42601`
で拒否します（テナント文脈はセッションに束縛された `PolicyContext` からの
み導出。Issue #754・TASK-174・HTTP-5・HTTP-6・HTTP-7）。検証を通過した
要求は、`op` フィールドを閉じた語彙 6 値（`search`／`scan`／`aggregate`／
`insert`／`update`／`delete`）の許可リストへ照らし、語彙外（DDL・UDF 呼び出し・
トランザクション制御を含む）は `wire_code` `0A000`（HTTP 501）で
拒否します（Issue #759・TASK-179・NOSQL-1・NOSQL-9。`update`／`delete` の
追加は Issue #875・NOSQL-12。語彙・スキーマ検証のみ実装済みで、束縛・
実行結線は Issue #876 の担当のため現時点では常に `0A000`（HTTP 501）に
留まります）。語彙内の要求は
本文スキーマ検証を経て、`scan` は束縛済み実行計画（`BoundScan`）へ写像し
`EngineCore` で実行して `{"columns":...,"rows":...,"row_count":...}` を
返します（`vector`／`plan`／`mode`／`hybrid` の付与・`explain: true` は
いずれも `wire_code` `42601` で拒否。Issue #766・TASK-176・NOSQL-3）。
`op: aggregate`（`explain: true` を除く単一行集計・`GROUP BY`／`HAVING`
集計の双方）は SQL テキストを組み立てずに `engine::sql::parser::BoundAggregate`
へ直接束縛・実行し、SQL 表層と同一の結果（NULL・オーバーフロー `22003`・
`VECTOR` 列拒否を含む）を返します（Issue #768・TASK-177・NOSQL-4）。
`group_by`（単一列限定）・`having`（`=`／`<`／`<=`／`>`／`>=` の 5 演算子）は
SQL-14 の `GROUP BY` 実行計画（`engine::sql::group_by::execute_grouped_aggregate`）
へ写像し、グループ数上限（`MAX_GROUPS`＝10,000）・グループキー累計バイト・
`HAVING` 述語数上限の超過はいずれも `wire_code` `54000` で拒否します
（Issue #769・TASK-177・NOSQL-5）。`op: search`（`vector`／`plan` 排他・
`hybrid`／`mode`／`columns`）は SQL 表層の検索 `SELECT` と同一形の
`engine::sql::parser::BoundStatement` へ束縛・実行し、`vector` 指定は
`EngineCore::execute_bound_search_in_session`、`plan` 指定は
`EngineCore::execute_bound_plan_search_in_session`（`USING PLAN` と同一の
fail-closed 判定順序・LLM 展開・`precision` 確信度ゲート〔SEARCH-9〕）へ
それぞれ委譲します。`explain: true` は `vector` 指定なら `wire_code`
`42601`、`plan` 指定なら `0A000`（正式な `explain` op 写像は NOSQL-10・
Issue #765 の担当）で拒否します（Issue #764・TASK-186・NOSQL-2）。
それ以外の op（`insert`）は現時点では暫定の `wire_code` `0A000`
（HTTP 501）を返します（束縛・実行計画への写像は Issue #771 の担当）。
`/v1/session`・
`/v1/session/close`・`/v1/query` の 3 エンドポイントは
バイト厳密一致でのみ受理し、それ以外のパス（完全未知パス・クエリ文字列
付き・末尾スラッシュ／余剰セグメント・大文字小文字違い等）はすべて
`08P01`（`unknown request target`。ルータでの判定）で拒否します。
`POST` 以外のメソッドは要求行パーサ（HTTP-2）の段階で先に `08P01`
（`invalid message frame`）として拒否されるため、ルータへは到達しません
（Issue #758・TASK-179・NOSQL-1。`/v1/query` 配下の tenant マーカー判定
〔`42601`。Issue #754〕は本 08P01 判定より優先します）。この `catch_unwind` は
`wire-server` を lib として使う
経路・テストでの防御であり、production バイナリでは起動時に導入する
panic hook（RECOVER-8）が unwind 前に `abort()` するため
`catch_unwind` へは実際には到達せず、1 接続の panic でも他接続を含め
プロセス全体が終了します（詳細: `crates/wire-server/src/http/conn.rs`
モジュール doc「panic 非伝播と RECOVER-8（fail-fast）との関係」節）。

NoSQL 表層の 3 エンドポイント・`op` 6 値それぞれの JSON スキーマ・応答形・
SQL 構文との対応表は `crates/wire-server/docs/nosql-api.md`（Issue #780）に
まとめています（上記段落は Issue #772 以降の `insert` 実装結線を未反映の
まま残しており、最新の契約は同ドキュメントを参照してください）。

`--search-engine`（Issue #656）は検索エンジン選択の opt-in CLI 引数です。
`--planner-endpoint`／`--planner-model`／`--embedder-hashing-dim`
（TASK-117）と同型の「プロセス起動時にのみ明示指定する注入点」で、未指定
（または `default`）は現行どおり brute-force のまま不変です。`hnsw`／
`hnsw_f16`／`hnsw_i8` を指定すると ANN（HNSW。下記「ANN（HNSW）opt-in 手順」
節参照）を索引ノード常駐精度 f32／f16／I8（SQ8）で有効化します。不正な値・
値欠落・2 回目以降の重複指定はいずれも fail-closed で起動エラーとなり、
既定へ黙って読み替わることはありません。選択結果は `EXPLAIN` の
`engine:`／`hnsw_params:` 行（Issue #411）で確認できます。ANN opt-in の性能
評価ベンチ（`RecallEngine`）が使う `brute_force` トークンは本 CLI では受理し
ません（本 CLI の語彙は `default`／`hnsw`／`hnsw_f16`／`hnsw_i8` の 4 値に限定）。

`--hnsw-full-scan-ratio`／`--hnsw-acorn-max-visible-ratio`／
`--hnsw-sparse-visited-max`（Issue #657）はフィルタ付き ANN の探索パラメータ
opt-in CLI 引数です。`--search-engine` が `hnsw`／`hnsw_f16`／`hnsw_i8` の
いずれかのときのみ指定できます（`default`／未指定との組合せ・値欠落・形状
不正（`<num>/<den>` 以外／非負整数以外）・意味不正（分母 0・`num > den`・
`acorn_max_visible_ratio < full_scan_ratio`）・重複指定はいずれも fail-closed
で起動エラー）。未指定時の既定値は下表のとおりで、これまでの実装既定値の
まま不変です（`m`／`ef_*` の CLI 露出は引き続き対象外）。

| CLI フラグ | 意味 | 既定値 | 対応するベンチ env |
| ---------- | ---- | ------ | ------------------ |
| `--hnsw-full-scan-ratio` | 可視カーディナリティ切替の閾値比（Issue #409）。可視候補数 ÷ 索引ノード数がこの比未満なら plain scan | `1/10` | `BENCH_KNN_PROFILE_FULL_SCAN_RATIO` |
| `--hnsw-acorn-max-visible-ratio` | ACORN-1 の 2-hop 展開を有効化する可視比率の上限（Issue #501） | none（無効） | — |
| `--hnsw-sparse-visited-max` | visited 集合の実装切替閾値（Issue #497） | `0`（常に dense） | `BENCH_KNN_PROFILE_SPARSE_VISITED_MAX` |

`full_scan_ratio`／`acorn_max_visible_ratio` はテナント存在情報に繋がるため
`EXPLAIN` の `hnsw_params:` 行へは出しません（Issue #411 の非露出方針を維持。
`sparse_visited_max=` は Issue #497 で既に露出済みのため現状どおり出力され
ます）。閾値の既定値の見直し・実測は Issue #659 の担当です。

`--durability`（Issue #850）は書き込みトランザクションの durability
（`engine::storage::WriteDurability`。Issue #849 が公開した構築時オプション）
を選ぶ opt-in CLI 引数です。`--search-engine` と同型の「プロセス起動時にのみ
明示指定する注入点」で、未指定（または `immediate`）は現行どおり
`WriteDurability::Immediate`（`redb` 自身の既定と同一。commit 完了までに
fsync 相当の同期を伴う）のまま不変です。不正な値・値欠落・2 回目以降の
重複指定はいずれも fail-closed で起動エラーとなり、既定へ黙って読み替わる
ことはありません。`none` を明示選択すると、commit 成功応答はディスクへの
永続を保証しなくなります（`recovery::fail_fast`（RECOVER-8）の
`abort()`・プロセス強制終了・電源断が発生すると、直前の `Immediate` commit
以降の commit が失われ得る損失ウィンドウの詳細は
`docs/design/ingest-write-path.md`「Issue #849 追記」節参照）。この安全上の
含意を運用者が見落とさないよう、`none` を選んだ場合に限り起動ログへ英語の
`WARNING` 行を 1 行出します（`immediate`・未指定では出力されません）。
`EXPLAIN` への durability 設定の露出は対象外です。

`--auth-method`（Issue #940・WIRE-18・TASK-222）は SQL 表層の SASL 認証方式を
選ぶ opt-in CLI 引数です。`--search-engine`／`--durability` と同型の「プロセス
起動時にのみ明示指定する注入点」で、未指定（または `cleartext`）は現行どおり
cleartext password 認証のまま不変です。`scram-sha-256` を指定すると
[RFC 7677](https://www.rfc-editor.org/rfc/rfc7677)（`SCRAM-SHA-256`）による
チャレンジ・レスポンス認証へ切り替わり、平文パスワードが wire 上を流れなく
なります。不正な値・値欠落・2 回目以降の重複指定はいずれも fail-closed で
起動エラーとなり、既定へ黙って読み替わることはありません。認証方式は
サーバー全体で 1 つに固定され、ユーザーごとには切り替えられません。

`scram-sha-256` を選択する場合の制約:

- **全ユーザーが SCRAM 検証子を持っている必要があります**（起動時に
  fail-closed で検証。検証子を欠くレコードが 1 件でもあると起動を拒否します）。
  ユーザーストアへ登録する SCRAM 検証子は `wire-server hash-password
  --with-scram-sha-256` で生成します（stdin からパスワードを 1 行読み、
  `<PHC 文字列>:<SCRAM 検証子文字列>` を標準出力へ出します。既定
  `--with-scram-sha-256` 省略時は従来どおり PHC 文字列のみを出力します）。
- **パスワードは印字可能 ASCII（0x20〜0x7e）に限定されます**。SASLprep
  （[RFC 4013](https://www.rfc-editor.org/rfc/rfc4013)。NFKC 正規化を含む）は
  自作せず、印字可能 ASCII の範囲では SASLprep が恒等変換になることを
  利用してこの制約に置き換えています。範囲外の文字を含むパスワードは
  `hash-password --with-scram-sha-256` の時点で拒否されます（libpq・
  node-postgres 等の一般的なクライアント実装と結果が一致することを保証する
  ための設計判断です）。
- **NoSQL 表層（`--surface nosql`）とは併用できません**（起動時 fail-closed で
  拒否）。`POST /v1/session` は Argon2id 照合を直接呼ぶ別経路であり SASL の
  ような往復を持たないためです（HTTP-10）。
- TLS 自体は `--tls-cert`／`--tls-key`（下記参照）で opt-in できます。SCRAM
  channel binding（`SCRAM-SHA-256-PLUS`・`p=` フラグ）は結線済みですが、
  機構リストへの提示は `--tls-scram-channel-binding enable`（下記参照。
  既定 `disable`）を明示指定した場合のみです（Issue #941・#970）。`enable`
  かつ葉証明書の署名アルゴリズムに RFC 5929 が定義するハッシュが無い
  （Ed25519 など）場合は、SQL 表層（`--surface sql`。既定）に限り起動時に
  拒否されます（Issue #1088）。NoSQL 表層（`--surface nosql`）は本項冒頭の
  とおり SCRAM 自体を併用しないため、この判定は適用されず `enable` は
  無条件で no-op として受理されます（Issue #968）。
- **`--scram-mock-key-file <path>` が必須です**（`scram-sha-256` 選択時のみ。
  未指定・`cleartext` との組合せ・32 バイト未満のファイルはいずれも
  fail-closed で起動拒否）。未知ユーザー向けモック検証子（列挙攻撃対策）の
  salt を導出する秘密で、**ユーザーストアの内容から独立**させる必要が
  あります（P0 review 是正・Issue #940 PR #1006）。運用者は
  `head -c 32 /dev/urandom > <path>` 等で 1 度だけ生成し、以後の再起動・
  ユーザーストア更新をまたいで同じファイルを使い続けてください（毎回
  生成し直す・ユーザーストアと同じ内容から導出する、といった運用は
  未知ユーザーの存在を推測させる情報漏えいに繋がるため避けてください）。

`--ddl-allowed-users`（SQL-23・TASK-202・TASK-203、Issue #899・#902・#909・#908）は
`CREATE TABLE`・`DROP TABLE`・`CREATE VIEW`／`DROP VIEW`（TABLE-18・TASK-205）・
`CREATE INDEX`／`DROP INDEX`（INDEX-7・TASK-206）の DDL 実行権限を持つ
username をカンマ区切りで列挙する opt-in CLI 引数です。
`--search-engine`／`--durability` と同型の「プロセス起動時にのみ明示指定する
注入点」で、未指定は DDL 実行権限を持つユーザーが 0 人のまま（**全ユーザーの
`CREATE TABLE`／`DROP TABLE`／`CREATE VIEW`／`DROP VIEW`／`CREATE INDEX`／
`DROP INDEX` が `42501`（permission denied）で拒否されます**。fail-closed。
DDL は全テナント共有のカタログを変更するため既定で無効化されています）。列挙した username は
`--users` で読み込んだユーザーストアへ実在する必要があり、未知の
username・空要素・重複要素・フラグの重複指定はいずれも fail-closed で
起動エラーになります。認証成功後の接続に対してのみ、対応する username が
一覧に含まれる場合に限り DDL 実行権限が付与されます（テナント境界とは別軸の
権限で、`RLS` の判定は変更しません。詳細な構文・権限判定順序は
`docs/design/sql-create-table.md`・`docs/design/drop-table.md`・
`docs/design/create-view.md`・`docs/design/index-ddl-declaration.md` 参照）。

`--tls-cert`／`--tls-key`／`--tls-mode`（Issue #967・親 #941・TASK-228。
WIRE-7, WIRE-9 ポインタ）は SQL 表層（pg wire）の TLS を有効化する opt-in
CLI 引数です。`--tls-cert`（サーバー証明書チェーン PEM）・`--tls-key`
（Ed25519 PKCS#8 秘密鍵 PEM）は両方揃って初めて意味を持ち、片方のみの
指定は fail-closed で起動エラーになります（`--search-engine`／
`--durability` と同型の「プロセス起動時にのみ明示指定する注入点」）。
証明書・鍵の読み込み・検証（鍵と証明書の公開鍵一致・有効期限）に失敗した
場合も起動を拒否し、鍵・証明書の内容はエラーメッセージへ一切出力されません。

`--tls-mode`（`require`／`allow`。`--tls-cert`／`--tls-key` を指定した
ときのみ意味を持ち、単独指定は fail-closed。未指定時の既定は安全側の
`require`）は `SSLRequest` を経ない平文 StartupMessage の受理ポリシーです。

- `require`（既定）: 平文 StartupMessage を startup パラメータを解釈する
  前に `08P01` で拒否します。TLS を構成済みであれば非ループバックアドレス
  への bind も許可されます（WIRE-9 を満たすため）。
- `allow`: 平文接続も受理します。平文パスワードが漏えいしうるため、
  TLS 未構成時（`Cleartext`）と同じくループバック限定のままです
  （非ループバック bind は起動時に fail-closed で拒否され、`require` へ
  切り替えるよう案内するヒントを表示します）。

TLS 有効時は起動ログへ `TLS enabled (mode=require|allow)` の 1 行のみを
出します（未指定時はこの行を一切出さず、既存の起動ログはビット同一の
まま不変です）。`--surface nosql`（HTTP リスナー）も同じ意味で TLS を
受理し、HTTPS として終端します（Issue #968）。NoSQL 表層は HTTP に
`SSLRequest` のような明示ネゴシエーションが無いため、接続受理直後の
先頭バイトで TLS レコード（`0x16`）か平文 HTTP かを判定します。`require`
の下では平文 HTTP 接続へ要求を解釈せず応答なしで切断し、`allow` の下では
平文・TLS の双方を受理します。TLS 構成時に同時接続数上限を超えた接続は、
`allow` では平文と判定した接続に限り既存の平文 503 応答を維持し、TLS
レコードと判定した接続はハンドシェイクをせず無応答で切断します。`require`
では平文・TLS いずれの接続も要求を解釈せず無応答で切断します。実
クライアント（psql・openssl s_client・curl 等）での接続試験は
Issue #969・#968 の担当です。詳細は
`docs/design/tls-wire-connection.md` を参照してください。

`--tls-scram-channel-binding`（`enable`／`disable`。Issue #970・#1088・
WIRE-9・WIRE-18 ポインタ）は SCRAM-SHA-256-PLUS
（`p=tls-server-end-point`。RFC 5929 §4）を機構リストへ提示するかを選ぶ
opt-in CLI 引数です。`--tls-mode` と同じく `--tls-cert`／`--tls-key` を
指定したときのみ意味を持ち、単独指定は fail-closed で起動エラーに
なります。**未指定時の既定は `disable`**（非提示）です。

`enable` を選び、かつ葉証明書の署名アルゴリズムに RFC 5929 が定義する
ハッシュが無い場合（本サーバーが受理する唯一の葉鍵種別である Ed25519 鍵を、
Ed25519 で自己署名した証明書を含む）は、**SQL 表層（`--surface sql`。既定）
に限り起動時に非 0 終了で拒否されます**（Issue #1088）。NoSQL 表層
（`--surface nosql`）は SASL 往復自体を持たないためこの判定を適用せず、
同じ Ed25519 葉証明書のままでも `enable` は無条件で no-op として起動を
継続します（Issue #968・上記「NoSQL 表層とは併用できません」項参照）。
libpq（psql 18.6・OpenSSL 3.5.5 で実測）の既定設定
`channel_binding=prefer`・`channel_binding=require` はそのような葉証明書に
対し TLS 確立後の SCRAM 交換で失敗する（`could not find digest for NID
UNDEF`。TLS ハンドシェイク自体は成立します）ため、黙って PLUS を非提示へ
縮退させるのではなく起動時に構成ミスとして拒否する判断です。`enable` を
使うには、RSA／ECDSA（署名ハッシュが MD5／SHA-1／SHA-256／SHA-512。
SHA-384 は本サーバーの自作実装が対応していないため同じく拒否されます）の
CA が署名した葉証明書を用意してください（サーバー鍵自体は引き続き
Ed25519 のみです）。受理される構成で `enable` を選んだ場合は起動ログへ
運用上の注意を 1 行追加で出します。実測結果・判断根拠の詳細は
`docs/design/tls-channel-binding.md`（ADR）を参照してください。

### 回帰ベンチの Environment `bench-gate` secrets（TASK-127）

secret ↔ spec ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md`
に集約しています（Issue #286。値の実設定は引き続きマージ後の管理者作業。
`PRECISION_EVAL_*`〔TASK-163〕は目標値未確定のため未設定のままです）。

閾値は repo variables ではなく Environment **`bench-gate`** の **secrets**
（`secrets.*`）から注入します。`run` ステップに渡す `env:` ブロックは GitHub
Actions のログへ値付きでそのまま出力されるため、以前使っていた variables では
閾値の数値が public な Actions ログへ印字されてしまっていました
（`docs/design/ci-gate-variables.md` 参照）。secrets は Actions が一致文字列を
自動的に `***` へマスクするため、同じ経路では漏えいしません。

**repo レベルの secrets ではなく main 限定 Environment に置く理由**:
`workflow_dispatch` は任意の ref を選んで起動でき、選択した ref の workflow
YAML・`run` ステップがそのまま実行されます。write 権限者が別ブランチで `run`
ステップを書き換えて `workflow_dispatch` すれば、repo レベルの secrets を任意の
処理（外部送信を含む）へ渡せてしまいます——ログのマスクは Actions のログ出力
にのみ効くもので、書き換えられた `run` ステップが secrets を別経路へ渡すこと
自体は防ぎません（PR #299 codex-review P0 指摘）。そのため閾値 secrets は
Environment `bench-gate`（deployment branch policy で `main` のみに制限）に
置き、`.github/workflows/bench.yml` の閾値 secrets を使う全 job
（`bench-simd`・`bench-contrast`・`bench-batch`・`bench-c1`）に
`environment: bench-gate` を指定します。main 以外の ref から
`workflow_dispatch` した run は Environment `bench-gate` にアクセスできない
ため、閾値 secrets を取得できません（`.github/workflows/recall.yml` が
`recall-gate` で採用済みの実行境界と同一方針。各 job の `if:
github.ref == 'refs/heads/main'`・`checkout ref: main` は defense-in-depth
として維持しています）。opt-in フラグ（`BENCH_CORE6`・`BENCH_CORE16`・
`BENCH_DEDICATED_ENV`。値は 0/1 や専有環境宣言で非機密）は引き続き repo
variables のままです。

> [!WARNING]
> **workflow を一度でも実行する前に、必ず deployment branch policy（`main` のみ）付きで Environment `bench-gate` を作成してください。** 未作成のまま `environment: bench-gate` を指定した job が走ると、GitHub は branch policy なしの environment を自動作成してしまい、`main` 以外の ref からもアクセスできる状態になります（`recall-gate` と同じ注意点）。**本リポジトリでは Environment `bench-gate` は作成済みです**（branch policy `main` 付き）。

`.github/workflows/bench.yml`（`workflow_dispatch` + 週次 `schedule`。毎週月曜 03:00 UTC）は `BENCH_MAX_P95_MS`（p95 レイテンシ上限・ミリ秒）・`BENCH_MIN_RECALL`（Recall@k 下限）・`BENCH_BATCH_MAX_DEGRADATION_PCT`（バッチ経路の劣化率上限・TASK-130）・`BENCH_MAX_CONTRAST_RATIO`（対照エンジンに対する p95 レイテンシ比率〔被検/対照〕の上限・TASK-127 CORE-5）を Environment `bench-gate` の Actions secrets（`secrets.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。マージ後、リポジトリ管理者が以下を実行して設定してください。

```bash
gh secret set BENCH_MAX_P95_MS --env bench-gate
gh secret set BENCH_MIN_RECALL --env bench-gate
gh secret set BENCH_BATCH_MAX_DEGRADATION_PCT --env bench-gate
gh secret set BENCH_MAX_CONTRAST_RATIO --env bench-gate
```

形式は以下のとおりです（値は上記のとおり本リポジトリには記載しません）。

| secret | 形式 |
| -------- | ---- |
| `BENCH_MAX_P95_MS` | 正の整数（単位: ms） |
| `BENCH_MIN_RECALL` | `(0.0, 1.0]` の浮動小数点 |
| `BENCH_BATCH_MAX_DEGRADATION_PCT` | 0 以上の有限浮動小数点 |
| `BENCH_MAX_CONTRAST_RATIO` | 0 より大きい有限浮動小数点 |

未設定のまま実行すると `crates/engine/benches/simd_bench.rs`／`batch_bench.rs`／`contrast_bench.rs` が fail-closed で判定不能として非ゼロ終了します（デフォルト値は持ちません）。

`simd_bench.rs` の標準出力は各判定（p95_latency・topk_consistency・diagnostic_ab）の pass/fail と非数値の状態のみで、median・p95・recall_min・診断 A/B の a_median/b_median/median_ratio 等の実測値は出力しません（AGENTS.md P0。`contrast_bench.rs`〔CORE-5〕と同方針。Issue #277）。実測値がローカルで必要な場合は `cargo bench --bench simd_bench -p fandhe-vector-db-engine -- --verbose` で出力できますが、`GITHUB_ACTIONS` が設定された環境（CI）では `--verbose` は fail-closed で拒否されます（public ログへの実測値混入防止）。実測値は非公開の記録先へ保存し、public 資産（コード・PR・Issue 等）へ転記しないでください。

CORE-5（対照エンジンとの p95 レイテンシ比較。ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-5）は usearch の総当たり `exact_search`（`contrast-bench` feature 限定の optional 依存。`crates/engine/Cargo.toml`）を対照エンジンとして接続済みです（TASK-127・Issue #176。クレート採用と公開境界はオーナー承認済み〔2026-08-26〕）。`contrast_bench.rs` が被検（`ParallelSearchProvider`）と対照エンジンを同一データ・同一クエリで interleaved A/B 実行し、両者の p95 レイテンシ比率（被検/対照）が `BENCH_MAX_CONTRAST_RATIO` 以下であることを判定します。CORE-3/CORE-4（`simd_bench.rs`）とは独立した bench-contrast ジョブとして既定ゲート実行され、`BENCH_MAX_CONTRAST_RATIO` 未設定・不正値は fail-closed で非ゼロ終了します（旧 `BENCH_CORE5` repo variable による opt-in 方式は撤去済み）。閾値の具体値は spec が SSOT のため本リポジトリには記載せず、bench の標準出力にも出しません。`contrast-bench` feature は `make lint`／`make test`（lefthook pre-push 含む）が `--all-features` で実行するため、`make bench-contrast` に限らずこれらのローカル実行・CI でも usearch の C++ ビルドが走ります。C++17 コンパイラが必要です（GitHub ホステッド `ubuntu-latest` には同梱済み。ローカルに C++17 コンパイラがない環境では `make lint`／`make test`／`make ci` が失敗します）。

CORE-7（動的窓集約を経由する単発クエリ経路の p95 劣化上限）は Issue #302 で測定方式を再整合しました。`batch_bench.rs::run_core7_gate` が CORE-6/CORE-16 と同じ合成データセットを使い、対照・被検とも同一カーネル（`BatchEngine::batch_search`）を経由させたうえで、複数試行の劣化率（%）の中央値を `BENCH_BATCH_MAX_DEGRADATION_PCT` と比較します（設計判断の詳細・原因調査は `docs/design/core7-dynamic-window-gate.md` 参照。数値は書きません）。旧来の「集約器の push/drain 単体を比較するだけ」の測定は、合否に数えない診断（`diagnostic_dynamic_window_push_drain`）として引き続き標準出力に残ります。

同様に CORE-6（GPU vs CPU-SIMD）・CORE-16（f16 常駐 vs f32 常駐）は実 GPU バックエンド（`gpu_batch.rs`）へ接続済みです（Issue #178・#234）。`benches/batch_bench.rs` の A/B 実測ゲートへどちらも配線済みで、CORE-6 は GPU 経路 vs CPU-SIMD 経路、CORE-16 は GPU 側の f16 パック常駐 vs f32 常駐対照経路（`GpuF32ContrastBackend`）を比較します。GitHub ホステッド runner に GPU が無いこと・閾値が spec SSOT であることから `BENCH_CORE6`/`BENCH_CORE16` repo variable による opt-in 方式を維持します（未設定＝既定で対象外。opt-in 時は短縮率下限の Environment `bench-gate` secrets `BENCH_CORE6_MIN_IMPROVEMENT_PCT`/`BENCH_CORE16_MIN_IMPROVEMENT_PCT` も必要で、未設定なら fail-closed）。GPU が初期化できない環境で opt-in された場合はそれぞれ理由とともに `pass=false` を報告します。`schedule` トリガ（週次）は #168 で再追加済みです。secrets 未設定のまま週次 run が実行された場合は fail-closed で red になります（false green にはなりません）。GitHub ホステッド runner には GPU が無いため、CORE-6/16 の実測には GPU 搭載ホストでの手動実行が必要です。

`batch_bench.rs` の標準出力は既定で pass/fail と非数値状態のみを書き、実測値・注入した閾値のどちらも出しません（`contrast_bench.rs` の CORE-5 で採用済みの方針を横展開したもの。Issue #279）。実測値（p95・median）が必要な場合は、ローカルまたは承認済み計測環境で `BENCH_VERBOSE=1 make bench-batch` を実行してください。`BENCH_VERBOSE` は `.github/workflows/bench.yml` の `bench-batch` ジョブへは注入しておらず、`GITHUB_ACTIONS` が設定された実行環境では opt-in 自体を bench 側が fail-closed で拒否します（public な Actions ログへ実測値が漏れないための二重化）。

Apple GPU（Metal）環境での CORE-16 fail 報告（Issue #313）を切り分けるための規模点診断は `BENCH_CORE16_DIAG=1 BENCH_CORE16_DIAG_SCALE_INDEX=<0..5> BENCH_VERBOSE=1 make bench-batch` で実行できます（`BENCH_CORE16_DIAG` 単独では `BENCH_VERBOSE` の設定を促す 1 行のみ出力し、合否には数えません。`BENCH_CORE16_DIAG` opt-in 時は `main` が CORE-7/CORE-6/CORE-16 ゲートを一切測定せず選択規模点の診断のみを直ちに実行するため（PR #326 codex-review 指摘対応: 先行ゲートの GPU バックエンド構築・破棄が同一プロセス内で診断計測へ持ち越されるのを防ぐ）、CORE-7 用の `BENCH_BATCH_MAX_DEGRADATION_PCT` はこの経路では不要です。複数規模点を同一プロセス内で連続測定すると比較不能なノイズが乗ることを ADR で確認済みのため、`BENCH_CORE16_DIAG_SCALE_INDEX` で 1 回の実行につき 1 規模点のみを選び、規模点間の比較はプロセスを分けて複数回実行してください）。切り分け結果・環境別 pass/fail は `docs/design/core16-f16-resident-gate.md` を参照してください。

### C1 p95 専有環境再測定（TASK-83）

`make bench-c1`（`crates/engine/benches/sql_c1_bench.rs`）は SQL 表層（`EngineCore::execute_sql`）経由の C1（純粋 Top-k）p95 を測定します。閾値は SQL-1 専用の `BENCH_SQL_C1_MAX_P95_MS`（正の整数・ms）・`BENCH_SQL_C1_MIN_RECALL`（`(0.0, 1.0]` の浮動小数点）を Environment `bench-gate` の secrets から注入します。上記 TASK-127 の `BENCH_MAX_P95_MS`／`BENCH_MIN_RECALL` は `SearchProvider` 単体（CORE-3・SEARCH-4・CORE-4）の基準であり SQL-1 とは spec 上の出所が異なるため、流用せず別 secret として分離しています（流用すると緩い側で false green・厳しい側で false red になります）。値そのものは本リポジトリには記載しません。

```bash
gh secret set BENCH_SQL_C1_MAX_P95_MS --env bench-gate
gh secret set BENCH_SQL_C1_MIN_RECALL --env bench-gate
```

未設定のまま実行すると `sql_c1_bench.rs` が fail-closed で判定不能として非ゼロ終了します（デフォルト値は持ちません）。`.github/workflows/bench.yml` の `bench-c1` ジョブは `workflow_dispatch` 限定で、`bench-simd`／`bench-batch` と異なり週次 `schedule` には含めません（GitHub ホステッド runner が専有環境ではないため。詳細は `docs/design/c1-p95-dedicated-env-reverification.md` 参照）。

`BENCH_DEDICATED_ENV=1` は Conditional Go 条件7（専有環境での p95 再測定）の判定を有効化する opt-in フラグです。他プロセスと CPU/IO を共有しない専有環境で実行する場合にのみ設定してください（自動検出はできないため運用者の明示宣言に限ります）。未設定（既定）の場合、p95・Recall の pass/fail 自体は出力されますが、条件7 の判定対象からは明示的に除外されます。

```bash
BENCH_SQL_C1_MAX_P95_MS=<spec 値> BENCH_SQL_C1_MIN_RECALL=<spec 値> BENCH_DEDICATED_ENV=1 make bench-c1
```

`sql_c1_bench.rs` の既定の標準出力は、公開済み定数（`rows`/`dim`/`k`/`queries`）と各判定の pass/fail・非数値状態（A/B 診断の可否・条件7 の評価有無）のみです。p95・中央値・Recall・A/B 比率の実測値は出力しません（`contrast_bench.rs`・CORE-5 対応（PR #224）と同一方針。pass/fail と実測値を並べて公開すると spec 由来閾値が逆算可能になるため）。

実測値が必要な場合（ADR の再実行・切り分け）は `BENCH_SQL_C1_VERBOSE=1` を付けて **GitHub Actions 外の環境で** 実行してください。GitHub Actions 下（`GITHUB_ACTIONS` 環境変数が設定されている場合）では verbose 要求を fail-closed で拒否し、データ投入前に非ゼロ終了します。

```bash
BENCH_SQL_C1_MAX_P95_MS=<spec 値> BENCH_SQL_C1_MIN_RECALL=<spec 値> BENCH_DEDICATED_ENV=1 BENCH_SQL_C1_VERBOSE=1 make bench-c1
```

出力された実測値は非公開記録先へ保存し、public な資産（ADR・PR・Issue・コミットメッセージ）へは転記しないでください。

### 性能判定の計測規約（Issue #462）

perf 系 ADR・Issue が個別に定めてきた計測規約（交互実行・統計量・ノイズ帯の
定義）を `docs/design/benchmark-judgement-policy.md` に集約しています。要点:

- before/after を**交互に N ≥ 5 ペア**実行し、per-run 生データを必ず残す
- 統計量は **min-of-N と median の両方**を併記する
- ノイズ帯は固定相対帯（±5%）と参照区間の実測帯の**両方**を超えることを判定に
  効かせる条件とする
- 共有 QEMU 本環境の絶対値は production 変更の採否・絶対閾値ゲートの確定根拠に
  はできない（`BENCH_DEDICATED_ENV=1` は自己申告のみで自動検出はしない）
- 後続 perf Issue がそのまま使える受け入れ条件テンプレート（crossdb フェーズ名の
  固定語彙を含む）を同 doc に収録

### ティア別レイテンシ受け入れ基準の実測手順（TASK-116）

`make bench-tier`（`crates/engine/benches/tier_latency_bench.rs`）は TASK-116（対象ビヘイビア: `docs/spec/04-behavior/query-planning.md` PLAN-4・PLAN-6・PLAN-7。判定内容・測定段階・数値基準は spec 側が SSOT であり本リポジトリには記載しません）の受け入れ基準を実測します。常駐 Ollama への実接続が前提です。

> [!IMPORTANT]
> `.github/workflows/bench.yml` に `bench-tier` ジョブは**置きません**。GitHub ホステッド runner には常駐 Ollama が無く、self-hosted runner の使用は ai-review の codex / review ジョブに限る組織承認済み例外の範囲外（AGENTS.md「CI・ワークフローの改変（P1）」。self-hosted 経路は過去の指摘により撤去済み）のため、CI 上のどの設定（opt-in の有無）でも実測を成功させる経路が存在しません（PR #269 Codex 指摘）。実測は本節の手順により GitHub Actions 外の承認済み計測環境で運用者が直接実行してください。これが TASK-116 受け入れ基準実測の正式な入口です。

常駐 Ollama を持つ環境で `make bench-tier` を実行してください。必要な opt-in・接続・閾値 env の一覧（変数名と用途のみ。値は含みません）は `cargo bench --bench tier_latency_bench -p fandhe-vector-db-engine -- --help` で表示されます。未設定・不正値のまま opt-in（`BENCH_TIER` 設定）した場合は fail-closed で不足している env 名を含む明示エラーとして表示されます。値そのもの・p95 上限は spec 由来のため本リポジトリには記載しません。

常駐 Ollama の応答形式は非決定的で、`PlanError::InvalidResponse`（LLM 不正応答）が試行中に発生することがあります（Issue #316）。本ベンチはこれを除外対象として扱い、規定の有効サンプル数に達するまで追加試行で埋め合わせます（`Timeout`／`Unavailable` 等は従来どおり致命エラーとして即座に非ゼロ終了します）。段ごとの除外数上限は任意 env `BENCH_TIER_MAX_INVALID_RESPONSE_TRIALS`（未設定時は本リポ既定値、固定上限値 1000 を超える値は fail-closed で拒否）で調整でき、上限を超えた場合は該当段名を含む非ゼロ終了メッセージとともに判定未到達のまま終了します。標準出力には各段の試行回数・除外回数（`attempts=… invalid_responses=…`）を記録します（p95 上限は引き続き非出力）。詳細は `docs/design/tier-latency-acceptance.md`「不正応答試行の扱い」節を参照してください。

実測値（p95 の数値）そのものは public な `docs/design/tier-latency-acceptance.md` へ転記せず、非公開記録先へ保存してください。同ドキュメントの「実測状態」節には各判定の「実施済み/未実施」「pass/fail」「routing 一致/不一致」という非数値の状態のみを更新してください。判定ロジック層（時間非依存の純関数）のみ `crates/engine/tests/tier_latency_accept.rs` として `make ci` 対象です。設計判断の記録は `docs/design/tier-latency-acceptance.md` を参照してください。

### `USING PLAN` wire 経由受け入れ基準の実測準備（TASK-117）

`wire-server` バイナリは `--planner-endpoint <host:port> --planner-model <name>`（両方セットで `engine::query_planner::OllamaClient` を注入）・`--embedder-hashing-dim <N>`（`engine::embedding::HashingEmbedder` を注入。決定的・ネットワーク不要な**検証用参照実装**であり意味的埋め込みではありません）を受け付けます。いずれも未指定が既定で、その場合 `USING PLAN` は従来どおり fail-closed に拒否されます（`XX000`・固定の一般化メッセージ）。

wire v3 経由（生バイトクライアント）での `USING PLAN` 実行契約（成功系・fail-closed 系・RLS 不変）は `crates/wire-server/tests/wire_using_plan.rs`（`make ci` 対象）が決定的スタブで検証します。実 Ollama・実クライアント 3 種（psql／psycopg／pg）を使った PLAN-9 数値基準の実測ハーネスは本リポジトリでは未整備です（TASK-116 の `make bench-tier` と同様の運用者実行手順が必要になる見込み。整備は別タスクとして追跡してください）。

**0 行時の切り分け**: `USING PLAN` が SQL エラーなしで 0 行を返す場合、まず `EXPLAIN SELECT ... USING PLAN(...)` の `mode`/`mode_source` を確認してください。`mode: precision` / `mode_source: planner_estimate` であれば確信度ゲート（SEARCH-9）による既知の空集合応答です（`USING MODE 'recall'` 等で明示上書きできます）。詳細な調査結果・再現手順は `docs/design/using-plan-precision-empty-result.md`（Issue #315）を参照してください。`EXPLAIN` は `engine`/`ann_plan`（ANN opt-in 時のみ `hnsw_params` も）行で使用エンジン・索引経路への適用有無（静的判定）も報告します（Issue #411・`docs/design/explain-search-engine-exposure.md` 参照）。`WHERE` の等価・前方一致・`id` 単純比較がスカラー列二次索引の候補削減へ結線されているかどうかは末尾の `scalar_plan` 行（`plain_scan`/`index_equality`/`index_prefix`/`index_id_range`/`index_conjunction`）で確認できます（Issue #474・`docs/design/scalar-index-prune.md` 参照）。

### 境界同点グループ再取得ループのレイテンシ計測（Issue #324）

`make bench-hybrid`（`crates/engine/benches/hybrid_latency_bench.rs`）は、PR #320
が追加した境界同点グループ再取得ループ（`hybrid.rs::hybrid_search_boosted`）の
単発クエリレイテンシへの寄与を、単一ビルド内の A/B（通常コーパス vs 同点誘発
コーパス）で計測します。spec 由来の pass/fail 閾値を持たない情報提供専用のベンチ
のため `.github/workflows/*` へは配線せず、手動実行専用です。`GITHUB_ACTIONS` が
設定された実行環境では起動直後に fail-closed で拒否します。実測結果・設計は
`docs/design/hybrid-refetch-latency.md` を参照してください。

**SQL 表層（hnsw opt-in）計測モード（Issue #506）**: 既定モード（env 未設定）は
`hybrid::hybrid_search` を直接呼ぶため、Issue #505 の実 seam
（`sql::hnsw_hybrid::HnswDenseProvider`）を通りません。`BENCH_HYBRID_LATENCY_ENGINE=
brute_force|hnsw|hnsw_f16` を設定すると、`EngineCore::from_storage_with_engine`
＋ `ORDER BY HYBRID(...)`（SQL 表層。ANN opt-in の唯一の到達経路）を計測する
モードへ切り替わります（既定モードの出力は本追加の前後で不変）。
`BENCH_HYBRID_LATENCY_SCALE=small|large|all`（既定 all）・
`BENCH_HYBRID_LATENCY_CORPUS=no_refetch|tie_refetch|all`（既定 all）・
`BENCH_HYBRID_LATENCY_NUM_DOCS`／`_DIM`／`_VOCAB_SIZE`／`_QUANTIZE_LEVELS`
（既定はスケール別定数を上書き）・`BENCH_HYBRID_LATENCY_EXPECT_RESUMED=1`
（`tie_refetch` の after 側計測にのみ指定。`hybrid_resumed_rounds` が 0 のまま
なら非 0 終了）を指定できます。

前後比較は `scripts/bench_hybrid_latency_ab.sh`（`make bench-hybrid-ab`）で
行います。`BEFORE_BIN`／`AFTER_BIN` に退避済みバイナリの絶対パス、
`BEFORE_COMMIT`／`AFTER_COMMIT` にビルド元コミットの hash を指定し
（`docs/design/benchmark-judgement-policy.md` §3 が要求する追跡可能性のため
必須）、`AB_PAIRS`（既定 5・5 未満は拒否）で交互ペア数を指定して
`ref_bf_large_tie5`・`hnsw_large_uniform`・`hnsw_large_tie5`・
`hnsw_410shape_tie2`（Issue #410 形状）の 4 条件を before→after の順で交互
実行します。`--summarize <dir>` で `hybrid_latency: stage=` 行・環境行を
条件・ペア・before/after の実行順で列挙できます（判定・平均化は行わず、
生ログをそのまま出力）。before バイナリの再現手順（`838c53e` = Issue #505
直前）:

```bash
git archive 838c53e | tar -x -C <scratch>/before
# 本ベンチの差分のみを overlay（production・Cargo.lock は 838c53e のまま）
cp crates/engine/benches/hybrid_latency_bench.rs <scratch>/before/crates/engine/benches/
cp crates/engine/benches/harness/hybrid_latency.rs <scratch>/before/crates/engine/benches/harness/
CARGO_TARGET_DIR=<scratch>/target-before cargo build --release \
  --manifest-path <scratch>/before/crates/engine/Cargo.toml --bench hybrid_latency_bench
```

実測結果・判断は `docs/design/hnsw-hybrid-iterative-scan.md`「前後比較実測
（Issue #506）」節を参照してください。

### hybrid_rrf 段別内訳プロファイルと転置索引化の前後比較（Issue #356・#387・#394）

`make bench-hybrid-profile`（`crates/engine/benches/hybrid_profile_bench.rs`・
`--features bench-internals`）は、hybrid 検索（`sql/exec.rs` の
`Ranking::Hybrid` 分岐）を SQL 実行・`SparseIndex::build`・
`hybrid_search_boosted`（cached index）・疎側再取得ループ・`search_within` の
段別に分解して実測します。spec 由来の pass/fail 閾値を持たない情報提供専用の
ベンチのため `.github/workflows/*` へは配線せず、手動実行専用です。
`GITHUB_ACTIONS` が設定された実行環境では起動直後に fail-closed で拒否します。
`BENCH_HYBRID_PROFILE_ROUNDS`（既定 5・5〜50）で Issue #465 の最新基線ラウンド
計測（SQL 表層・投影・密・疎・残差の帰属表）の交互実行回数、
`BENCH_DEDICATED_ENV=1` で専有環境自己申告を指定できます（`docs/design/
hybrid-rrf-latency-breakdown.md`「最新基線」節参照）。`make
bench-hybrid-wire-profile`（`crates/wire-server/benches/
hybrid_wire_profile_bench.rs`）は同 Issue で `hybrid_rrf` の engine 内 hybrid
経路／SQL 表層／wire の 3 区分を切り分けます。`BENCH_HYBRID_WIRE_ROUNDS`
（既定 5・5〜50）でラウンド数を指定できます。

Issue #547 で行数・可視率を opt-in 可変化しました。
`BENCH_HYBRID_PROFILE_ROWS`（既定 25,000・`1..=100000`）でコーパス行数、
`BENCH_HYBRID_PROFILE_VISIBLE_RATIO`（既定 `1/1`・`1/<1..=1000>` 形式のみ）で
可視率を指定できます。可視率の意味は経路で異なります: SQL 段
（`sql_hybrid`／`sql_dense_knn`／`collect_body_strings`）は RLS の正規経路
（可視率に満たない行を `Visibility::Private` として投入）で索引の文書数
そのものが縮小し、直接 API 段（`hybrid_search_cached_index`・
`sparse_refetch_loop`・`search_within_fetch_k=<k>` 等）は常に全件から
構築した索引へ可視部分集合だけを渡します（「索引 N ≫ 可視集合」条件を
直接検証する経路）。#546（`SparseIndex::score_by_postings` のスコア
アキュムレータ再利用）の前後比較は `scripts/bench_hybrid_profile_ab.sh`
（`make bench-hybrid-profile-ab`）で行います。`BEFORE_BIN`／`AFTER_BIN` に
退避済みバイナリの絶対パス、`BEFORE_COMMIT`／`AFTER_COMMIT` にビルド元コミット
の hash を指定し（`docs/design/benchmark-judgement-policy.md` §3 が要求する
前後比較の追跡可能性のため必須）、`AB_PAIRS`（既定 5・5 未満は拒否）・
`AB_ROUNDS`（既定 5・hybrid_profile_bench 自身の受理範囲 5..=50 の外は拒否）で
交互ペア数・ラウンド数を指定し、N=25,000／100,000 × 可視率 1/1・1/10 の
4 条件を before→after の順で交互実行します。`--summarize <dir>` で
`baseline_round_raw`／`baseline_summary`／`reference_band` 行を条件・ペア・
before/after の実行順に沿ってファイル名付きで一覧表示できます。
前後比較の実測結果は `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #547」
節を参照してください。

`cargo run --release -p fandhe-vector-db-engine --example feature_bench` は SQL 表層・ベクトル
検索・RLS を含む 13 フェーズ（`ingest`・`hybrid_rrf`・`vector_knn` 等）を
横断的に計測し JSON を stdout へ出力します（依存追加なし・std のみ。
`BENCH_FEATURE_ENGINE`／`BENCH_FEATURE_SCALE` による ANN opt-in・規模上書きは
下記「ANN（HNSW）opt-in 手順と前後比較（Issue #413）」節を参照）。

hybrid 疎索引の転置索引化（Issue #386 Phase 1・#388〜#392）の設計判断・
データ構造・維持契約・外部実装参照（tantivy・qdrant）、および上記 2 つの
計測ツールによる Phase 1 導入前後の通し比較は
[`docs/design/sparse-inverted-index.md`](docs/design/sparse-inverted-index.md)
を参照してください。

### ingest 段別内訳プロファイル（Issue #396）

`make bench-ingest-profile`（`crates/engine/benches/ingest_profile_bench.rs`）は、
書き込み経路（`engine::tenant::insert_rows` → `insert_rows_unchecked`）を所有権
検査・`begin_write`・encode（1 回。台帳の内容照合ハッシュと redb 書き込みで共有。
Issue #397）・content_hash・台帳記録・redb insert・世代更新・commit の段別に
分解して実測します。spec 由来の pass/fail 閾値を持たない情報提供
専用のベンチのため `.github/workflows/*` へは配線せず、手動実行専用です。
`GITHUB_ACTIONS` が設定された実行環境では起動直後に fail-closed で拒否します。
`BENCH_INGEST_PROFILE_ROWS`／`BENCH_INGEST_PROFILE_DIM` でバッチ行数・次元を
上書きできます（許容範囲外・非数値は fail-closed に拒否）。`BENCH_INGEST_PROFILE_
INSERT_MODE`（`insert`〔既定〕／`reserve`）で I6 段の redb `insert_reserve` A/B
計測モードを切替できます（Issue #400。`insert`／`reserve` 以外は fail-closed に
拒否）。実測結果・設計は `docs/design/ingest-stage-profile.md`・
`docs/design/redb-insert-reserve-zero-copy.md` を参照してください。
Phase 2（親 Issue #395）を通した前後比較・棄却判断（RECOVER-5／RECOVER-6／
RECOVER-8 ポインタ）・バッチ上限の申し送りは `docs/design/ingest-write-path.md`
（Issue #401）を参照してください。

`BENCH_INGEST_PROFILE_MODE`（`batch`〔既定〕／`single`。Issue #484）で、
上記のバッチ経路とは別に crossdb ベンチが実際に通る**単文** `INSERT` 経路
（wire 簡易クエリ → SQL 表層 → `tenant::insert_typed_row_unchecked`〔1 文 1
write txn〕）の段別内訳（`parse_bind`／`typed_row_api`／`sql_surface` の
3 tier ＋ I1〜I8）を計測できます。`single` モードでは
`BENCH_INGEST_PROFILE_STATEMENTS`（既定 25,000・2,000〜100,000）で単文数を
上書きでき、`BENCH_INGEST_PROFILE_ROWS` は無視されます（`BENCH_INGEST_
PROFILE_INSERT_MODE=reserve` は `batch` 専用機能のため `single` では
fail-closed に拒否）。`make bench-ingest-wire-profile`
（`crates/wire-server/benches/ingest_wire_profile_bench.rs`）は同じ単文
`INSERT` 経路を wire プロトコル経由（in-process ループバック）で計測し、
engine 側 `sql_surface` tier との差分から wire 往復自体の寄与を切り分けます
（`BENCH_INGEST_WIRE_ROWS`〔既定 25,000・5,000〜100,000。`BENCH_INGEST_WIRE_
ROUNDS` で割り切れる値のみ〕・`BENCH_INGEST_WIRE_ROUNDS`〔既定 5・5〜50〕・
`BENCH_DEDICATED_ENV=1` で専有環境自己申告を指定可能）。実測結果は
`docs/design/ingest-stage-profile.md`「Issue #484 追記」節を参照してください。

`single` モードでは `BENCH_INGEST_PROFILE_DURABILITY`（`immediate`〔既定〕／
`none`。Issue #851）で書き込みトランザクションの durability を切り替えられ、
`immediate` 以外は `batch` モードでは fail-closed に拒否されます。
`make bench-ingest-durability-ab`（`scripts/bench_ingest_durability_ab.sh`。
`AB_PAIRS`〔既定 5〕・`STATEMENTS`〔既定 5,000・2,000〜100,000〕）は既定
`immediate` と opt-in `none` を交互 N ペアで実測し、`scripts/fsync_probe.py`
（macOS `F_FULLFSYNC`／`F_BARRIERFSYNC`・`fsync(2)` の原始操作プローブ）も
同時に採取します。`python3 scripts/bench_ingest_durability_ab_summarize.py
<出力ディレクトリ>` で TSV 集約できます。実測結果・比較条件は
`docs/design/crossdb-bench.md`「Issue #851: durability 別実測と比較条件」
節・`docs/design/ingest-write-path.md`「10. Issue #851 追記」節を参照して
ください。

### クロスエンコーダリランカーの実測手順（Issue #333）

`make rerank-cross-encoder-eval`（`crates/engine/tests/rerank_cross_encoder_recall.rs`。
`cross-encoder` feature 限定）は、実 ONNX 推論バックエンド
`OnnxCrossEncoderBackend`（`crates/engine/src/rerank/cross_encoder_onnx.rs`）
による自然言語 fixture（`tests/fixtures/nl_qa.rs`）上の Recall@20 実測を行います。
`bench-tier` と同様に CI には配線せず、運用者が手動実行します。

以下の環境変数が必要です（値そのものは含みません。未設定の場合は明確な
エラーメッセージとともに fail します）:

- `ORT_DYLIB_PATH`: onnxruntime 共有ライブラリ（`libonnxruntime.so` 等）へのパス
- `CROSS_ENCODER_MODEL_PATH`: ONNX 形式のクロスエンコーダモデルファイルへのパス
- `CROSS_ENCODER_TOKENIZER_PATH`: 対応する `tokenizer.json` へのパス

```bash
export ORT_DYLIB_PATH=/path/to/libonnxruntime.so
export CROSS_ENCODER_MODEL_PATH=/path/to/model.onnx
export CROSS_ENCODER_TOKENIZER_PATH=/path/to/tokenizer.json
make rerank-cross-encoder-eval
```

モデル・トークナイザ・onnxruntime 共有ライブラリはいずれもリポジトリへ
コミットしません。運用者が別途取得してください（実測で使用したモデル・
ライセンス・実測値・原因分析は `docs/design/rerank-recall-regression.md`
「Issue #333 追記」節を参照してください）。`GITHUB_ACTIONS` が設定された
実行環境では起動直後に fail-closed で拒否します。

### 他 DB との機能別横断ベンチ（`make bench-crossdb`）

`scripts/crossdb_bench/`（Python ハーネス・Cargo 依存追加なし）を使い、自作 DB（wire-server 経由）と pgvector・sqlite-vec・Qdrant・LanceDB・MySQL の機能別（KNN・フィルタ付き KNN・集計・GROUP BY・hybrid・Recall@10 等）レイテンシを既定 dim 128・25,000 行のデータセット上で比較します（`CROSSDB_DIM` で dim=768 等へ切替可。Issue #466）。計測ツールの Python 依存は `requirements.txt` で `==` 固定し、Cargo 依存は増やしていません。

**前提環境**:

- Docker
- Python venv: `pip install -r scripts/crossdb_bench/requirements.txt`
- `cargo build --release -p fandhe-vector-db-wire-server`
- fixture 生成: `cargo run --release -p fandhe-vector-db-engine --example seed_docs -- seed <out.redb> 25000 128` → `export <db> <docs.jsonl>` → `queries 128 200 <queries.jsonl>`（dim=768 は `docs25k-d768.redb` のように `-d<dim>` 付きファイル名で生成し `seed`／`queries` の第 2 引数〔dim〕を 768 にする。下記「実行」参照）

**実行**:

```bash
export CROSSDB_DIR=<fixture ディレクトリ>
export CROSSDB_PYTHON=<venv の python へのパス>
make bench-crossdb
```

`scripts/crossdb_bench/run_all.sh` を呼び出し、対象外のコンテナは自動停止、結果は `$CROSSDB_DIR/results/<db>_<config>.json` に保存されます。`CROSSDB_DIM=768` を指定すると既定ファイル名を `docs25k-d768.{redb,jsonl}`／`queries200-d768.jsonl`、出力先を `$CROSSDB_DIR/results/d768`／`$CROSSDB_DIR/logs/d768` へ切り替え、`run.py --expect-dim 768` で docs／queries の埋め込み次元が一致することを fail-closed に検証します（`CROSSDB_REDB`／`CROSSDB_DOCS`／`CROSSDB_QUERIES` を明示すればこの既定より優先されます）。

```bash
# dim=768 の fixture を生成してから計測する例（Issue #466）
S=$CROSSDB_DIR
cargo run --release -p fandhe-vector-db-engine --example seed_docs -- seed "$S/docs25k-d768.redb" 25000 768
cargo run --release -p fandhe-vector-db-engine --example seed_docs -- export "$S/docs25k-d768.redb" "$S/docs25k-d768.jsonl"
cargo run --release -p fandhe-vector-db-engine --example seed_docs -- queries 768 200 "$S/queries200-d768.jsonl"
CROSSDB_DIM=768 make bench-crossdb
```

同一ホストで他セッションの対照 DB コンテナ（既定名 `bench-pgvector`／`bench-qdrant`／`bench-mysql`）と並行計測したい場合は `CROSSDB_PG_CONTAINER`／`CROSSDB_QDRANT_CONTAINER`／`CROSSDB_MYSQL_CONTAINER`（Docker コンテナ名文字集合のみ許可・fail-closed）で別名を指定できます（ポートも `CROSSDB_*_PORT` で別値にしないと衝突するため両方指定してください。Issue #466。詳細は `scripts/crossdb_bench/README.md` 参照）。

GPU 対照（FAISS・Qdrant GPU）の詳細は `scripts/crossdb_bench/gpu/README.md` を参照してください。spec 由来の閾値なし、情報提供専用・手動実行・CI 非配線です。計測結果・所見は `docs/design/crossdb-bench.md` を参照してください（dim=768 基線は同ドキュメント参照）。後続 Issue が self との前後比較を行う際の受け入れ条件テンプレート（統計量・ノイズ帯・記入例）は `docs/design/benchmark-judgement-policy.md` を参照してください。

`make hnsw-crossdb-selectivity CROSSDB_DIR=<dir>`（`crates/engine/tests/hnsw_crossdb_selectivity.rs`。層 A は `make ci` 対象、層 B は `#[ignore]`・release 専用）は crossdb fixture（`docs25k.redb`・`lang='ja'` 選択率 33.1%）を対象に、HNSW opt-in の `full_scan_ratio`／`acorn_max_visible_ratio` 別 arm・k（10・200）での `ann_masked`／`mask_splits_graph`／plain scan 到達分類と既定エンジン対照 Recall@k（LIMIT=k に対応する実質 Recall@k。k=10 と k=200 の双方を記録）を記録します（Issue #659。実測結果は `docs/design/hnsw-rls-cardinality-switch.md`「Issue #659」節参照）。

### `vector_knn` の wire／SQL／カーネル内訳プロファイル（Issue #463）

`make bench-knn-wire-profile`（`crates/wire-server/benches/knn_wire_profile_bench.rs`）は、`docs/design/crossdb-bench.md` の `vector_knn` 786µs を wire／SQL 表層／距離カーネル・Top-k の 4 区分へ切り分けます。`BENCH_KNN_WIRE_ROUNDS`（既定 5・5〜50）でラウンド数、`BENCH_DEDICATED_ENV=1` で専有環境自己申告を指定できます。spec 由来の閾値なし・情報提供専用・手動実行・CI 非配線（`GITHUB_ACTIONS` 環境下では起動直後に拒否します）。判定ロジック自体（rounds パース・帰属計算・ノイズ帯判定）は `crates/wire-server/tests/knn_wire_profile_accept.rs` で `make ci` から回帰検証します。実測結果・計測設計の詳細は `docs/design/knn-wire-stage-profile.md` を参照してください。

### 全行走査経路の段別プロファイル（Issue #464）

`make bench-scan-stage-profile`（`crates/engine/benches/scan_stage_profile_bench.rs`）は、`docs/design/crossdb-bench.md` で self が最劣後する `agg_count`／`rls_isolation`／`vector_knn_where` の redb 全行走査・ヘッダデコード・RLS 判定（`PolicyContext::is_visible`＋TABLE-12 キー/ヘッダ整合検査）・dim/metadata デコード・`WHERE` 述語評価・arena 複製の段別内訳を切り分けます。`BENCH_SCAN_PROFILE_ROUNDS`（既定 5・5〜50）でラウンド数、`BENCH_SCAN_PROFILE_SCALE`（既定 1＝25,000 行・4＝100,000 行。1 プロセス = 1 規模点）で規模、`BENCH_SCAN_PROFILE_SELECTIVITY`（既定 `1/5`。`1/<N>`・`N` は 2〜100）で `lang = 'ja'` の選択率、`BENCH_DEDICATED_ENV=1` で専有環境自己申告を指定できます。spec 由来の閾値なし・情報提供専用・手動実行・CI 非配線（`GITHUB_ACTIONS` 環境下では起動直後に拒否します）。判定ロジック自体（rounds/scale/selectivity パース・段間差分・ノイズ帯判定・整合性検証）は `crates/engine/tests/scan_stage_profile_accept.rs` で `make ci` から回帰検証します。実測結果・計測設計・後続 Issue（#477・#471）への帰属分析の詳細は `docs/design/scan-stage-profile.md` を参照してください。`BENCH_SCAN_PROFILE_SELECTIVITY=1/3`（crossdb fixture 相当の選択率 33%）指定時の現行索引経路（Issue #474）内訳・非 vacuous 確認・実測結果は `docs/design/filtered-distance-stage-profile.md`（Issue #653）を参照してください。`BENCH_SCAN_PROFILE_ENGINE=hnsw|hnsw_f16|hnsw_i8`（既定 `brute_force`。未設定時は出力・処理が導入前とビット同一のまま不変）・`BENCH_SCAN_PROFILE_FULL_SCAN_RATIO=<n>/<d>`（`hnsw` 系エンジン限定の opt-in override）で HNSW opt-in 時の `Subset` 形状 plain scan 縮退時委譲（Issue #676）の到達経路（`plain_scan_ratio`／`plain_scan_mask_split`／`plain_scan_masked_short`／`ann_masked`）とレイテンシを追加計測できます（Issue #677。実測結果は `docs/design/hnsw-rls-cardinality-switch.md`「Issue #677」節を参照してください）。

### 候補 id マスク経路の前後比較（Issue #655）

Issue #654（PR #664）は SCALAR 事前フィルタ付き DISTANCE の候補行探索を「新規 `VectorArena` へ複製してから探索する」経路から「候補 id マスクを `SearchProvider::search_subset` へ直接渡す（複製なし）」経路へ置き換えました。この前後比較は 2 トラックで行います。

- **段別プロファイル（Track A）**: `make bench-filtered-distance-ab BEFORE_DIR=<path> AFTER_DIR=<path> BEFORE_COMMIT=8225baa AFTER_COMMIT=db9bd94`（`git archive <commit> | tar -x -C <dir>` で用意した独立ソースツリーを指定。after 側は選択率 opt-in・I 系列・`index_mask_scans_delta` を持つ #663 merge `db9bd94` を指定する。`2488128`（#654 適用直後）は #663 未適用のため after-only 段〔I1〜I3・`index_mask_scans_delta`〕が構造的に出力できない）。`scan_stage_profile_bench`（Issue #464・#653）を before（#654 未適用）/after（適用後）2 コミットで交互 N≥5 ペア実行し、`e2e(vector_knn_where/W0-hot|W0-cold)` の改善幅と参照区間（`e2e(vector_knn/W0-nowhere)`・`R_dot_kernel_distance_only`・`agg_count`・`rls_isolation`）の非退行を確認します。`AB_AFTER_ONLY_SELECTIVITY=1/3`（既定）で crossdb fixture 相当の選択率 33% での after 側単独計測（I1〜I3 内訳・`index_mask_scans_delta`）も併せて行います（before バイナリは選択率 opt-in・I2b/I3 出力を持たないため 1/3 でのペア比較は構造的に不能）。ログは `docs/design/bench-data/filtered-distance-mask-ab/` 配下、`scripts/bench_filtered_distance_ab.sh --summarize <dir> [session_ts]` で TSV 集約します。
- **crossdb（Track B）**: `make bench-scalar-index-crossdb-ab BEFORE_COMMIT=8225baa AFTER_COMMIT=2488128 REF_COMMIT="" CROSSDB_DIR=<dir> CROSSDB_PYTHON=<python>`（`vector_knn_where`・`bulk_knn_where_k200` を含む crossdb self 5 フェーズと参照区間 `vector_knn`・`agg_count`・`mode_recall` を計測。手順・env は既存の「他 DB との機能別横断ベンチ」節を参照）。

実測結果・Qdrant との差の縮小幅は `docs/design/scalar-index-mask-search.md`「前後比較実測（Issue #655）」節を参照してください。

### チップ別カーネルの実測手順（Issue #469）

`make bench-chip`（`crates/engine/benches/chip_bench.rs`）は、`bench-dot-kernel`・`bench-knn-profile`・`feature_bench`（`BENCH_FEATURE_DIM=128`／`768`）の 4 ワークロードを 1 ワークロード = 1 子プロセスとしてラウンドロビン交互計測し、CPU 情報・実行時検出 ISA・per-run 生データ・min/median・参照区間帯を `summary.json` へ出力します。

> [!IMPORTANT]
> `.github/workflows/*` には配線しません。`bench-tier`（TASK-116）と同じ理由（AGENTS.md「CI・ワークフローの改変（P1）」）で、Phase 4（チップ最適カーネル）の採否判定に必要な AVX-512／NEON／実キャッシュ階層は本開発環境（QEMU 仮想 CPU）では実測できず、オーナー実機（Apple M／AMD Zen 4・5／Intel）での手動実行が正式な入口です。Apple M 実機での i8／f16／f32 経路の前後比較専用の手順・記録テンプレートは `docs/design/chip-kernel-guidelines.md` §7.7（Issue #526）を参照してください。

前提: Linux／aarch64 Linux は `/proc/cpuinfo`（追加ツール不要）、macOS は Xcode Command Line Tools（`cargo`）と `sysctl`（標準搭載）のみで動作します。`contrast-bench` feature は使わないため C++17 コンパイラは不要です。

実行例:

- `make bench-chip`（既定 5 ラウンド・全 4 ワークロード）
- `BENCH_CHIP_ROUNDS=1 BENCH_CHIP_WORKLOADS=dot_kernel make bench-chip`（スモーク実行。`BENCH_CHIP_ROUNDS` が `docs/design/benchmark-judgement-policy.md` の最小ペア数〔5〕未満の場合、`summary.json` の `meets_policy_min_rounds` が `false` になり参考値であることを自己ラベルします）
- `BENCH_DEDICATED_ENV=1 make bench-chip`（専有環境自己申告。他の `bench-*` ターゲットと同じ自己申告のみで自動検出はしません）

env 変数（すべて fail-closed パース。不正値は非ゼロ終了）:

| 変数 | 既定 | 内容 |
| ---- | ---- | ---- |
| `BENCH_CHIP_ROUNDS` | 5 | ラウンド数（1〜50） |
| `BENCH_CHIP_WORKLOADS` | 全 4 種 | `dot_kernel,knn_profile,feature_128,feature_768` のカンマ区切り部分集合 |
| `BENCH_CHIP_OUT_DIR` | `target/bench-chip/<unix-ts>` | 出力先（既存の `summary.json` があれば上書き拒否） |
| `BENCH_DEDICATED_ENV` | 未設定 | `1` で専有環境自己申告 |

`BENCH_FEATURE_ENGINE`・`BENCH_KNN_PROFILE_ENGINE` 等、上記以外の `BENCH_*` env は親プロセスの環境をそのまま子プロセスへ継承します（`BENCH_FEATURE_DIM` のみ `feature_128`／`feature_768` ワークロードが上書きします）。

出力は `<BENCH_CHIP_OUT_DIR>/summary.json`（CPU モデル名・関心 ISA フラグ・キャッシュ容量・実行時検出フラグ・build 情報・ラウンドごとの per-run ログパス・メトリクスごとの `values`／`min`／`median`／`max`／`reference_band_pct`）と、各 (round, workload) ごとの `round<N>_<workload>.{stdout,stderr}.log` です。すべて `<BENCH_CHIP_OUT_DIR>` からの相対パスで記録し、絶対パス・ホスト名・ユーザー名は含みません。本開発環境（QEMU・12 vCPU）での既定 5 ラウンド完走は数分程度でした（実測は環境依存）。

**before/after の交互比較手順**（Issue #530。Phase 4 通しの前後比較で追加した専用ドライバ）: `scripts/bench_chip_ab.sh`（`make bench-chip-ab`）が上記の手作業を自動化します。

```sh
# 1. 変更前後のコミットをそれぞれ git archive で独立ディレクトリへ展開し、
#    各ディレクトリで cargo bench --bench {dot_kernel_bench,knn_profile_bench,chip_bench} --manifest-path crates/engine/Cargo.toml --no-run
#    と cargo build --release --manifest-path crates/engine/Cargo.toml --example feature_bench を事前ビルドしておく
BEFORE_DIR=<before のディレクトリ> AFTER_DIR=<after のディレクトリ> AB_PAIRS=5 \
  make bench-chip-ab
# 2. TSV へ集約（env ブロック・workload×metric ごとの before/after min・median・ratio・判定クラス）
scripts/bench_chip_ab.sh --summarize _/bench/chip-ab/<UTC ts>
```

`AB_PAIRS`（既定 5・5 未満は拒否）・`BENCH_CHIP_WORKLOADS`（対象ワークロードの絞り込み）を指定できます。出力は `<OUT_DIR>/pair<N>-{before,after}/summary.json`（`.gitignore` 対象。記録を残す場合は `docs/design/bench-data/phase4-chip-ab/<UTC ts>-*.tsv` へ明示的にコピーしてください）。`GITHUB_ACTIONS` 下・出力先の再利用は fail-closed に拒否します。集約結果は `docs/design/chip-kernel-guidelines.md` §7 のテンプレート・`docs/design/phase4-chip-before-after.md`（本開発環境の実測記録済み）へ転記します。

結果の記録先・公開境界: 実測値そのものは public な docs・Issue へ記録可能です（オーナー判断 2026-08-29・[spec-confidentiality](.claude/rules/spec-confidentiality.md)）。spec 由来の閾値は本リポジトリには記載しません。結果記録テンプレート・チップ別空テンプレート・`summary.json` キー一覧は `docs/design/chip-kernel-guidelines.md` §7 を参照してください。

### macOS 上の feature 検出の実機検証（Issue #468）

`make detect-features`（`crates/engine/examples/detect_features.rs`）は `is_aarch64_feature_detected!`／`is_x86_feature_detected!` マクロの実効性（コンパイル時 `cfg!(target_feature)` 定数化・マクロ実行結果・macOS では `sysctl` 相互検証）を表として出力します。依存追加なし・検出結果の上書き機構なし（`isa.rs` の CORE-12 節と同じ fail-closed 方針）。Apple Silicon 実機での実測は `.github/workflows/detect-features.yml`（全 PR で起動する `detect-changes` ジョブが対象パス〔`isa.rs`・`detect_features.rs`・`tests/isa.rs`・同 workflow〕の変更有無を判定し、変更があった PR と `workflow_dispatch` でのみ `detect-apple` を GitHub ホステッド `macos-latest` runner 上で実行。対象外 PR では skipped の check-run を残します。情報提供専用ですが、自動マージ運用〔G0 ゲート〕の都合で両ジョブを必須チェックへ登録します）で行い、出力を `docs/design/chip-kernel-guidelines.md`「8. macOS 上の `is_aarch64_feature_detected!` 実効性」節へ転記します。GitHub ホステッド runner（macOS 26.5.2・仮想化）での実機確認は完了済みです（同節参照）。オーナー所有実機（M4 等）での追記も同ツールで行えます。

### GPU バッチ検索の規模スイープ（`make bench-gpu-scaling`）

`engine::gpu_batch`（f16 常駐）と CPU-SIMD バッチ経路の規模 × バッチサイズ別比較を行います。`BENCH_GPU_SCALING_ROWS`／`DIMS`／`BATCH`／`TOPK`／`ITERS` で計測条件を上書きできます。GPU 実機必須・手動実行専用ベンチで CI 非配線です。実測結果は `docs/design/crossdb-bench.md`「GPU」節を参照してください。`gpu_scaling:` 結果行に続けて出力される `gpu_scaling_stats:` 行（Issue #537）は f16／f32 各経路の読み戻し統計（`partial_topk_dispatches`・`full_readback_dispatches`・1 呼び出しあたり readback バイト数、Issue #539 追加分の `f16_arith_dispatches`・`f16_arith_guard_fallbacks`）を表示し、`scripts/bench_gpu_scaling_ab.sh` の結果行 grep（`^gpu_scaling: rows=`）とは接頭辞を分離しているため既存 A/B 集計には混入しません。`Features::SHADER_F16` 対応アダプタ（本開発環境の RTX 3060 を含む）でも、f16 経路（読み出し直後に f32 へ拡張してから積和するため算術自体は f32）が自動的に選ばれるのは選択条件（アダプタが `SHADER_F16` に対応し、かつクエリの全成分が f16 として厳密往復可能・オーバーフロー／非正規化アンダーフローも生じないこと。`select_dot_shader`／`docs/design/gpu-batch-f16-arith.md` 参照）を満たす場合に限られ、満たさない場合は unpack 版へ fail-closed に縮退します。CORE-16 ゲート（`crates/engine/benches/batch_bench.rs::build_scaled_gate_dataset`）が生成するクエリは `rng.next_vector` による任意精度の f32 値で f16 丸めを行わないため、往復可能性ガードにより実際には unpack 版へ縮退することがあり、被検側（f16 常駐）が新シェーダを経由するとは限りません。実際にどちらの経路を通ったかは `GpuBatchStats`（`f16_arith_dispatches`／`f16_arith_guard_fallbacks`。`make bench-gpu-scaling` の `gpu_scaling_stats:` 行で確認可能）で確認する必要があります。CORE-16 ゲート本体・規模点診断は `BENCH_VERBOSE=1` 指定時に `verbose(...): f16_arith_available=.. f16_arith_dispatches=.. f16_arith_guard_fallbacks=..` 行（Issue #540。合否には数えない情報提供専用）を追加出力するようになり、本開発環境の実測では常に `dispatches=0 guard_fallbacks=40`（構造的に unpack 版のまま）でした。`gpu_scaling_bench` へは opt-in `BENCH_GPU_SCALING_QUERY_F16_EXACT=1`（クエリを f16 厳密往復可能な値へ丸める。`harness/gpu_scaling.rs::round_to_f16_exact`）を追加し、`scripts/bench_gpu_scaling_ab.sh` へも `QUERY_F16_EXACT=1` としてパススルーできます（summary.tsv 末尾へ `f16_arith_dispatches`／`f16_arith_guard_fallbacks` 列を追加）。前後比較の詳細・実測値は Issue #540・`docs/design/gpu-batch-f16-arith.md`「8. 前後比較実測」節を参照してください。

i8 パック常駐経路（`engine::gpu_batch::packed_i8::GpuI8BatchBackend`。Issue #542。opt-in・候補生成専用）の計測行 `gpu_scaling_i8:`／`gpu_scaling_i8_stats:`（Issue #543）が A/B/C 3 経路の後段に追加で出力されます。`gpu_scaling_i8:` は CPU-SIMD 厳密対照に対する同点許容つき不一致件数（`i8_mismatch`）・平均 Recall@k（`i8_recall_at_k`。確定的指標）・速度比（`speedup_i8_vs_cpu_p95`／`speedup_i8_vs_f16_p95`）を出力し、`gpu_scaling_i8_stats:` は読み戻し・再スコア候補数・GPU backend・`build_ms` を出力します。`GpuI8Options::oversample` は構築時固定のため 1 プロセス = 1 oversample しか計測できません（`BENCH_GPU_SCALING_I8_OVERSAMPLE`。未設定時は既定 `packed_i8::DEFAULT_I8_OVERSAMPLE`＝4）。oversample のスイープは `scripts/bench_gpu_scaling_ab.sh` を `I8_OVERSAMPLE=<値>` 付きで複数回起動して行います（設定時のみ両バイナリへパススルー。before バイナリ〔i8 経路実装前〕は未知の env を読まないため無害）。実測結果・oversample 推奨値は `docs/design/gpu-batch-i8-packed.md`「前後比較実測（Issue #543）」節を参照してください。

Phase 5（#532・#536・#539・#542）の通し前後比較・FAISS GPU 対照・Qdrant GPU 構築対照の更新・Apple UMA ゼロコピー静的確認は `docs/design/gpu-batch-phase5-before-after.md`（Issue #544）を参照してください。

### Recall 回帰ハーネスの repo secrets（TASK-104）

secret ↔ spec ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md`
に集約しています（Issue #286。値の実設定は引き続きマージ後の管理者作業）。

`hybrid_recall.rs` の大規模段層 B（`HYBRID_RECALL_MIN_R20_LARGE`/
`HYBRID_RECALL_MIN_R100_LARGE`）は、TASK-110〜113 の決定的スタブ `LlmClient` に
よるクエリ展開ありの経路で測定します（Issue #306。SEARCH-2 の測定前提に整合。
詳細は `docs/design/hybrid-recall-regression.md`「クエリ展開の結線
（Issue #306）」参照）。secrets 名・注入手順・9 変数の説明は変わりません。

`hybrid_recall.rs`・`rerank_recall.rs`・`query_planning_recall.rs`・
`precision_eval.rs` の層 B 閾値ゲートは既定で対象名と pass/fail のみを標準出力へ
書き、実測値・注入した閾値のどちらも出しません（`batch_bench.rs` の `BENCH_VERBOSE`
方針を横展開したもの。Issue #303）。実測値が必要な場合は、ローカルで
`RECALL_VERBOSE=1 make recall-regression` のように実行してください。
`RECALL_VERBOSE` は `.github/workflows/recall.yml` へは注入しておらず、
`GITHUB_ACTIONS` が設定された実行環境では opt-in 自体をテスト側が fail-closed で
拒否します（public な Actions ログへ実測値が漏れないための二重化）。取得した
実測値は Issue・PR・docs 等の public 資産へ転記しないでください。

閾値は Environment `recall-gate` の variables ではなく **secrets**（`secrets.*`）
から注入します。`run` ステップに渡す `env:` ブロックは GitHub Actions のログへ
値付きでそのまま出力されるため、以前使っていた variables では閾値の数値が
public な Actions ログへ印字されてしまっていました（`docs/design/ci-gate-variables.md`
参照）。secrets は Actions が一致文字列を自動的に `***` へマスクするため、
同じ経路では漏えいしません。

`.github/workflows/recall.yml`（`workflow_dispatch` + 週次 `schedule`。毎週月曜 04:00 UTC。`pull_request` トリガは意図的に持たせていません）は `crates/engine/tests/hybrid_recall.rs` の層 B（`#[ignore]` 付き閾値ゲート）を `make recall-regression` 経由で実行し、`HYBRID_RECALL_MIN_R20_SMALL`（小規模段 Recall@20 下限）・`HYBRID_RECALL_MIN_R20_LARGE`（大規模段 Recall@20 下限）・`HYBRID_RECALL_MIN_R100_LARGE`（大規模段 Recall@100 下限）を GitHub Environment `recall-gate` の Actions secrets（`secrets.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。各下限値は `hits@k / Σmin(k,正解集合サイズ)`（正解集合が k 件を超えるクエリがあっても頭打ちにならない、達成可能な理論上限に対する到達率）というスケールで設定してください。マージ後、リポジトリ管理者が以下を実行して設定してください（`gh api` または Settings > Environments）。`recall-regression` job 内の 3 つの gate step（hybrid・rerank・query-planning）は互いに独立に実行され、job の合否は末尾の `Evaluate recall gates` step による 3 step の AND 判定で決まります（Issue #311・`docs/design/recall-gate-independent-evaluation.md`）。

> [!WARNING]
> **workflow を一度でも実行する前に、必ず deployment branch policy（`main` のみ）付きで Environment `recall-gate` を作成してください。** 未作成のまま `recall-regression` job（`environment: recall-gate` を指定）が走ると、GitHub は branch policy なしの environment を自動作成してしまい、`main` 以外の ref からもアクセスできる状態になります。これは本 workflow が `environment` 指定でブランチ保護（実行境界）を作っている前提を崩し、`HYBRID_RECALL_MIN_*`（spec 由来の非公開閾値）が任意 ref から漏えいしうる状態に戻ってしまいます。**本リポジトリでは Environment `recall-gate` は作成済みです**（下記手順どおり branch policy `main` 付き）。

1. Environment `recall-gate` を作成し、deployment branch policy で `main` のみに制限する（上記警告参照。**workflow の初回実行より前に行うこと**）
2. その environment に閾値 secrets を設定する:

   ```bash
   gh secret set HYBRID_RECALL_MIN_R20_SMALL --env recall-gate
   gh secret set HYBRID_RECALL_MIN_R20_LARGE --env recall-gate
   gh secret set HYBRID_RECALL_MIN_R100_LARGE --env recall-gate
   ```

3. `RERANK_RECALL_MIN_R20_LARGE`／`RERANK_RECALL_MIN_R20_IMPROVEMENT`（下記 TASK-108 参照）・`QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`／`QUERY_PLANNING_RECALL_MIN_R20_DIRECT`／`QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED`／`QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE`（下記 TASK-112 参照）も同じ Environment `recall-gate` に設定する（strict モードは 9 変数すべてを必須とするため）
4. `gh workflow run recall.yml --ref main` で **main を ref に指定して** 本 workflow を手動実行し、`gh run watch` で `recall-regression` job が **skip ではなく実際に実行され**、strict モード（下記）のもとで 9 変数すべてが正しく評価されて green になることを確認する（main 以外の ref を指定すると job が skip されて green に見えるため注意）。ログ中の secrets の値は Actions により自動的に `***` へマスクされる
5. 疎通確認が済めば、`schedule` トリガ（週次・#168 で再追加済み）により以降は自動実行されます

**secrets を設定するとゲートが有効化されます。** ローカルの `make recall-regression`（`HYBRID_RECALL_REQUIRE_THRESHOLDS` を注入しない）で未設定（GitHub Actions では空文字列に解決される repo secret も含む）のまま実行すると、`crates/engine/tests/hybrid_recall.rs` は「ゲート未設定＝明示的に対象外」を出力して成功終了します（fail-closed で塞ぐのは、設定済みの値が非数値・範囲外だった場合のみ）。

**`recall.yml` は strict モードで実行されます**: `recall.yml` は Run step で `HYBRID_RECALL_REQUIRE_THRESHOLDS=1` を常に注入します。この strict モードでは `HYBRID_RECALL_MIN_*` の未設定（environment 作成漏れ・secret 名の誤り・secret の誤削除を含む）も非数値・範囲外と同様に fail-closed でテスト失敗とします。strict モードなしだと「一度も評価していない run」が「基準を満たした run」と同じ green になってしまうため（`crates/engine/tests/hybrid_recall.rs::resolve_gate_threshold` 参照。PR #147 codex-review P1 継続指摘対応）。**`schedule`（週次・#168 で再追加済み）が無人実行で正しく評価されるよう、マージ後は必ず `workflow_dispatch` で strict モードのもとで疎通確認してください**（手順 4 参照）。

**`pull_request` トリガを持たせない理由（spec 機密保持が優先）**: `pull_request` で起動する job は PR 側の untrusted なコード（Makefile・テストコード含む）を checkout して実行するため、もし層 B を PR トリガにすると、PR がコードを書き換えて `HYBRID_RECALL_MIN_*`（spec 由来の非公開閾値）を標準出力へ書き出すだけで public な Actions ログから spec の数値基準を取得できてしまいます（`.claude/rules/spec-confidentiality.md` の P0 違反）。そのため層 B は既定ブランチの trusted なコードのみが走る `workflow_dispatch`・`schedule`（週次。#168 で再追加済み）に限定し、**PR のマージ判定は層 A（spec 数値を含まない public な固定値回帰。`.github/workflows/ci.yml` の `cargo test` で PR ごとに常時実行）が担う**、という役割分担にしています（`docs/design/hybrid-recall-regression.md` 参照）。決定的コーパスでの回帰トラッキング自体（層 A・固定値アサーション）は `make ci`（`cargo test`）に含まれており、こちらは repo secrets 不要です。

**閾値 secrets は repo レベルではなく Environment `recall-gate` に置きます**: `workflow_dispatch` は本来任意の ref を選んで起動でき、選択した ref の workflow YAML がそのまま実行されます。そのため `if: github.ref == 'refs/heads/main'`・`checkout ref: main` のような YAML 内の条件だけでは実行境界になりません——write 権限者が別ブランチでこのガードを外した `recall.yml` を push して `workflow_dispatch` すれば、そのブランチの YAML が実行されてしまうためです。加えて repo レベルの Actions secrets はどのブランチのどの workflow からも参照できるため、YAML 内の条件式では閾値の参照そのものを防げません。そこで閾値は repo レベルではなく Environment `recall-gate`（deployment branch policy で `main` のみに制限）の secrets として設定し、`recall-regression` job に `environment: recall-gate` を指定します。main 以外の ref から起動した run は environment `recall-gate` にアクセスできないため、別ブランチの改変 YAML から `if`／`checkout ref` を外して `workflow_dispatch` したとしても閾値を取得できません。`if: github.ref == 'refs/heads/main'`・`checkout ref: main` は environment 保護に対する defense-in-depth として維持しています。

### リランキング効果測定 Recall 閾値ゲートの repo secrets（TASK-108）

`.github/workflows/recall.yml` の同一 `recall-regression` job は、上記に続けて `crates/engine/tests/rerank_recall.rs` の層 B（`#[ignore]` 付き閾値ゲート）も `make rerank-regression` 経由で実行します。この rerank step は hybrid step とは独立に評価され（Issue #311）、hybrid が fail していても実行されます。`RERANK_RECALL_MIN_R20_LARGE`（リランキング後の最終 Recall@20 の絶対下限）・`RERANK_RECALL_MIN_R20_IMPROVEMENT`（baseline＝リランキングなしからの改善幅の下限）を同じ Environment `recall-gate` の Actions secrets（`secrets.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。設計・実測経緯は `docs/design/rerank-recall-regression.md` を参照してください。Environment `recall-gate` は上記手順ですでに作成済みのため、追加で行うのは secrets の設定のみです。`recall.yml` は `workflow_dispatch` / `schedule` の両方で strict モード（`RERANK_RECALL_REQUIRE_THRESHOLDS=1`）で実行されるため、上記 9 変数がすべて揃っていない場合は fail-closed でテスト失敗になります。

```bash
gh secret set RERANK_RECALL_MIN_R20_LARGE --env recall-gate
gh secret set RERANK_RECALL_MIN_R20_IMPROVEMENT --env recall-gate
```

挙動（opt-in・strict モード・`pull_request` 非対応の理由）は上記「Recall 回帰ハーネスの repo secrets」と同一です。ローカルの `make rerank-regression`（`RERANK_RECALL_REQUIRE_THRESHOLDS` を注入しない）で未設定のまま実行すると「ゲート未設定＝明示的に対象外」を出力して成功終了し、`recall.yml` からの実行（`RERANK_RECALL_REQUIRE_THRESHOLDS=1` を常時注入）では未設定も fail-closed でテスト失敗とします。出力は対象名と pass/fail のみで、実測値は上記と同じ `RECALL_VERBOSE=1` opt-in でのみ確認できます。

### クエリ展開の受け入れ基準 Recall 閾値ゲートの repo secrets（TASK-112）

`.github/workflows/recall.yml` の同一 `recall-regression` job は、上記に続けて `crates/engine/tests/query_planning_recall.rs` の層 B（`#[ignore]` 付き閾値ゲート）も `make query-planning-regression` 経由で実行します。この query-planning step も hybrid・rerank の各 step とは独立に評価され（Issue #311）、上流の step が fail していても実行されます。`QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`（`intent` カテゴリ＝コーパス語彙と重ならない言い換えクエリの、展開なしからの Recall@20 改善幅の下限）・`QUERY_PLANNING_RECALL_MIN_R20_DIRECT`（`direct` カテゴリ＝コーパス語彙と一致するクエリの、展開ありの Recall@20 絶対下限）・`QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED`（`NoisyLlmClient`〔非 oracle・劣化展開品質〕による `intent` カテゴリの Recall@20 改善幅下限。既存 2 変数を非 oracle スタブへ流用すると誤検知することが実測で確認されたため独立に追加した変数。`docs/design/query-planning-recall-regression.md` 参照）・`QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE`（TASK-113・PLAN-3。数万チャンク規模の大規模段における `direct` カテゴリの Recall@20 絶対下限。小規模段の `QUERY_PLANNING_RECALL_MIN_R20_DIRECT` とは別コーパス規模・別変数として独立に評価する）を同じ Environment `recall-gate` の Actions secrets（`secrets.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。設計・実測経緯は `docs/design/query-planning-recall-regression.md` を参照してください。Environment `recall-gate` は上記手順ですでに作成済みのため、追加で行うのは secrets の設定のみです。`recall.yml` は `workflow_dispatch` / `schedule` の両方で strict モード（`QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS=1`）で実行されるため、上記 9 変数がすべて揃っていない場合は fail-closed でテスト失敗になります。

```bash
gh secret set QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT --env recall-gate
gh secret set QUERY_PLANNING_RECALL_MIN_R20_DIRECT --env recall-gate
gh secret set QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED --env recall-gate
gh secret set QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE --env recall-gate
```

挙動（opt-in・strict モード・`pull_request` 非対応の理由）は上記「Recall 回帰ハーネスの repo secrets」と同一です。ローカルの `make query-planning-regression`（`QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS` を注入しない）で未設定のまま実行すると「ゲート未設定＝明示的に対象外」を出力して成功終了し、`recall.yml` からの実行（`QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS=1` を常時注入）では未設定も fail-closed でテスト失敗とします。出力は対象名と pass/fail のみで、実測値は上記と同じ `RECALL_VERBOSE=1` opt-in でのみ確認できます。

### ANN opt-in 時の Recall ゲート実測（Issue #412）

3 つの Recall 閾値ゲート（hybrid・rerank・query-planning）は `RECALL_ENGINE` 環境変数（非機密の opt-in フラグ。値そのものは閾値ではないため secrets ではなく repo variables 相当の扱い）で測定対象の検索エンジンを選べます。`brute_force` は従来どおり `engine::hybrid::hybrid_search` を in-memory 配列に対して直接呼ぶ既存経路で、実測値・固定値アサーションに一切影響しません。`hnsw` を指定すると、SQL 表層（`EngineCore::from_storage_with_engine` ＋ `ORDER BY HYBRID(...)`）経由の ANN opt-in 経路（ADR `docs/design/ann-index-adoption.md` B 案）で同一の閾値を判定します——ANN の実装 seam（`sql::hnsw_cache`／`sql::hnsw_hybrid`）は結合テストから直接は触れない `pub(crate)` のため、SQL 表層を通すのが production API 経由で ANN 経路へ到達する唯一の方法です（`crates/engine/tests/fixtures/recall_engine.rs` 参照）。`hnsw_f16`（Issue #515）を指定すると、同じ ANN opt-in 経路を HNSW 索引ノードの f16 常駐表現（`hnsw::ResidentPrecision::F16`。Issue #514・`docs/design/hnsw-f16-resident.md`）付きで測定します。`hnsw_i8`（Issue #523）を指定すると、同じ経路を I8（SQ8）常駐表現（`hnsw::ResidentPrecision::I8`。Issue #521・#522・`docs/design/hnsw-sq8-resident.md`）付きで測定します。`.github/workflows/recall.yml` は `strategy.matrix.recall_engine: [brute_force, hnsw, hnsw_f16, hnsw_i8]` で 4 エンジンを常に独立 job としてゲートします（`workflow_dispatch`・週次 `schedule` いずれのトリガでも同じ。以前の選択式 `workflow_dispatch` 入力は `schedule` 実行で `hnsw` が測定されない抜け穴になっていたため撤去しました。Issue #412）。

```bash
RECALL_ENGINE=hnsw RECALL_VERBOSE=1 make recall-regression
RECALL_ENGINE=hnsw RECALL_VERBOSE=1 make rerank-regression
RECALL_ENGINE=hnsw RECALL_VERBOSE=1 make query-planning-regression
# f16 常駐 opt-in（Issue #515）を測定する場合は hnsw_f16 を指定します
RECALL_ENGINE=hnsw_f16 RECALL_VERBOSE=1 make recall-regression
# I8（SQ8）常駐 opt-in（Issue #523）を測定する場合は hnsw_i8 を指定します
RECALL_ENGINE=hnsw_i8 RECALL_VERBOSE=1 make recall-regression
# CI から手動実行する場合（brute_force/hnsw/hnsw_f16/hnsw_i8 の 4 matrix job が起動します）
gh workflow run recall.yml --ref main
```

各ゲートのコーパス規模が `MIN_INDEXED_ROWS`（ANN 索引の下限行数。`sql::hnsw_cache.rs` の非公開定数）を下回る段（hybrid の小規模段のみ・400 件）は、`RECALL_ENGINE=hnsw`／`hnsw_f16`／`hnsw_i8` を指定しても構造的に brute-force のまま索引を構築しません（そのようにゲート側が非 vacuous 検証で固定しています）。それ以外の段（hybrid・query-planning の各小規模段は 4,000 件以上、大規模段は 20,000〜40,000 件）は実際に HNSW 索引を構築して測定します。検証設計・実測結果は `docs/design/ann-recall-gate-verification.md` を参照してください（`hnsw_f16` の実測は同 doc「Issue #515 追記」節・`hnsw_i8` の実測は同 doc「Issue #523 追記」節）。

### ANN（HNSW）opt-in 手順と前後比較（Issue #413）

ANN opt-in は Rust API に加え、`wire-server --search-engine`（Issue #656。上記
「wire-server の起動」節参照）でも有効化できます。テーブルカタログ属性による
opt-in 露出は引き続き未実装です。

```rust
let kind = engine::search_engine::hnsw_kind(engine::hnsw::HnswParams::default())?;
let core = engine::core::EngineCore::from_storage_with_engine(storage, kind);
```

`crates/engine/examples/feature_bench.rs`（13 フェーズ通し計測）・`crates/engine/benches/knn_profile_bench.rs`（`make bench-knn-profile`）は、いずれも ANN opt-in・規模スケールを env 変数で切り替えられます。

- `BENCH_FEATURE_ENGINE` / `BENCH_KNN_PROFILE_ENGINE`: 未設定・空・`brute_force`（既定）／`hnsw`（`HnswParams::default()` で opt-in）／`hnsw_f16`（Issue #516。索引ノード f16 常駐 opt-in・`ValidatedHnswParams::with_resident_precision(F16)`。詳細は `docs/design/hnsw-f16-resident.md`）／`hnsw_i8`（Issue #523。索引ノード I8（SQ8）常駐 opt-in・`ValidatedHnswParams::with_resident_precision(I8)`。詳細は `docs/design/hnsw-sq8-resident.md`）。未知値は fail-closed で拒否
- `BENCH_FEATURE_SCALE`（`feature_bench` のみ）: 正整数倍率。既定 1（25,000 行）。`hnsw::MAX_HNSW_NODES` を超えない範囲で bound
- `BENCH_FEATURE_DIM` / `BENCH_KNN_PROFILE_DIM`（Issue #466）: 正整数・既定 128・上限 4,096。dim=768／1536 が Issue #365 で採否の判別変数と判明したため、横断 SQL ベンチ側にも dim を可変にする規模点を用意したもの。未知値・0・上限超過は fail-closed で拒否

```bash
BENCH_FEATURE_ENGINE=hnsw cargo run --release -p fandhe-vector-db-engine --example feature_bench
BENCH_FEATURE_ENGINE=hnsw BENCH_FEATURE_SCALE=4 cargo run --release -p fandhe-vector-db-engine --example feature_bench  # 100,000 行
BENCH_KNN_PROFILE_ENGINE=hnsw make bench-knn-profile
BENCH_FEATURE_DIM=768 cargo run --release -p fandhe-vector-db-engine --example feature_bench
BENCH_KNN_PROFILE_DIM=768 make bench-knn-profile
```

既定エンジン（brute-force）との前後比較・25k/100k の規模スケーリング実測・参照した外部実装（qdrant・pgvector・usearch）の既定値・損益分岐点についての所見は `docs/design/hnsw-index.md` を参照してください。

`knn_profile_bench` にはさらに、可視比率 × 行数の損益分岐点スイープ（Issue #487。`hnsw_subset`〔SCALAR 事前フィルタ付き DISTANCE〕vs plain scan）専用の env があります（設定時は S1〜S5' を伴わない専用モードへ切り替わります）。

- `BENCH_KNN_PROFILE_VISIBLE_RATIO`: `1/<N>`（`N` は正整数・上限 1,000）。未設定（既定）はスイープ無効
- `BENCH_KNN_PROFILE_FULL_SCAN_RATIO`（`BENCH_KNN_PROFILE_ENGINE=hnsw` 限定）: `<num>/<den>`（`den>=1`・`num<=den`）で `ValidatedHnswParams::full_scan_ratio`（既定 1/10）を上書き
- `BENCH_KNN_PROFILE_SCALE`: 正整数倍率。既定 1（25,000 行）。`BENCH_FEATURE_SCALE` と同じ上限方針

```bash
BENCH_KNN_PROFILE_VISIBLE_RATIO=1/4 BENCH_KNN_PROFILE_ENGINE=hnsw make bench-knn-profile
BENCH_KNN_PROFILE_VISIBLE_RATIO=1/20 BENCH_KNN_PROFILE_ENGINE=hnsw BENCH_KNN_PROFILE_SCALE=4 make bench-knn-profile  # 100,000 行
make bench-knn-visible-ratio  # 全比率 × 全行数 × 4 arm を交互 N ペアで実行（SWEEP_PAIRS で回数を上書き）
```

実測結果・判断は `docs/design/hnsw-rls-cardinality-switch.md`「可視比率 × 行数の損益分岐点実測（Issue #487）」を参照してください。

`knn_profile_bench` にはさらに、f16 常駐（`hnsw_f16`）／I8 常駐（`hnsw_i8`）と f32 常駐（`hnsw`）の前後比較・常駐メモリ実測専用の 2 モードがあります（Issue #516・#523。互いに排他、`BENCH_KNN_PROFILE_VISIBLE_RATIO` とも排他）。

- `BENCH_KNN_PROFILE_HOT_ONLY=1`: S0-cold（毎サンプル新規 `EngineCore` 構築）を省き、索引 1 回構築＋ SQL 表層 e2e ホットパス（S0-hot 相当）と参照区間（`COUNT(*)`）のみを測ります。`BENCH_KNN_PROFILE_SCALE`（最大 40 = 1,000,000 行）まで許容するため、500k 行規模のような S0-cold が非現実的な所要時間になる規模点向けです
- `BENCH_KNN_PROFILE_INDEX_MEMORY=1`（`BENCH_KNN_PROFILE_ENGINE=hnsw|hnsw_f16|hnsw_i8` 限定）: redb・SQL 表層（`VectorArena` の 1 GiB 上限）を経由せず、メモリ上のコーパスから `HnswIndex` を 1 回構築して常駐バイト数（`approx_heap_bytes`・VmRSS 前後差・VmHWM）を子プロセス隔離で計測します。500k×768 のように SQL 表層では構造的に到達不能な規模点でも、索引単体としては計測できます

```bash
BENCH_KNN_PROFILE_HOT_ONLY=1 BENCH_KNN_PROFILE_ENGINE=hnsw_f16 BENCH_KNN_PROFILE_SCALE=20 make bench-knn-profile  # 500,000 行・f16 常駐
BENCH_KNN_PROFILE_INDEX_MEMORY=1 BENCH_KNN_PROFILE_ENGINE=hnsw_f16 BENCH_KNN_PROFILE_SCALE=20 BENCH_KNN_PROFILE_DIM=768 make bench-knn-profile
make bench-knn-f16-resident  # 全規模点 × f32/f16 を交互 N≥5 ペア＋索引単体メモリで一括実行（AB_PAIRS・AB_POINTS・AB_MEMORY_POINTS で上書き可）
make bench-knn-i8-resident  # 同じスクリプトの AB_CANDIDATE_ENGINE=hnsw_i8 opt-in（Issue #523。f32/I8 常駐の前後比較）
make bench-knn-precision-resident  # 同じスクリプトの AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8" opt-in（Issue #526。f32/f16/i8 の 3 精度を同一セッションで一括計測。Apple M 実機向け手順・記録テンプレートは docs/design/chip-kernel-guidelines.md §7.7 参照）
```

実測結果・判断は `docs/design/hnsw-f16-resident.md`「Issue #516 追記」節を参照してください。

HNSW 構築の並列化（Issue #406）については `make bench-hnsw-parallel-build`（スレッド数ラダーでの構築時間・8→12 スレッド頭打ちの段別内訳・`repair_reachability` 修復統計〔Issue #447〕）・`make bench-hnsw-compare`（usearch との構築時間・Recall@10・探索レイテンシ比較。L2 正規化コーパス方式を維持）で実測できます。いずれも手動専用ベンチで CI 非配線です。`repair_reachability` の発生抑制（Issue #448）・逐次段の並列化（Issue #449）・100k 点フルラダーでの前後比較実測と Recall 非劣化検証（Issue #450）で `repair` 段が threads=12 で約 12.5 倍短縮したことを確認済みです。詳細・実測値は `docs/design/hnsw-parallel-build.md`・`docs/design/ann-recall-gate-verification.md` を参照してください。

ANN opt-in 経路に積み上がった探索メモリ局所性・構築並列化・フィルタ付き探索の各施策（#486〜#507）を通しで前後比較した実測は `docs/design/hnsw-phase3-before-after.md` を参照してください（Issue #507。before/after を `bench-hnsw-compare`〔N=5 ペア・rows=20,000 縮小構成〕、`bench-knn-profile`〔共有環境のリソース逼迫により N=1 ペア〕、`feature_bench`〔hnsw opt-in の 1 arm のみ N=3 ペア〕、Recall 3 ゲート層 B〔決定的コーパスのためペア測定対象外〕という対象別に異なる実施条件で計測。共有 QEMU 環境の参考値・採否根拠にしない。再現手順は同 doc §9）。

受理判定後 prefetch（Issue #490）の前後比較実測は `make bench-hnsw-search`（`BENCH_HNSW_SEARCH_ROWS`／`BENCH_HNSW_SEARCH_DIM`／`BENCH_HNSW_SEARCH_MASK`〔RLS 事前フィルタ統合の `Subset` 形状を模す可視率〕で 1 規模点を計測し、before/after バイナリを交互起動して比較する手動専用ベンチ）で実施できます。`git archive` で取り出した作業ツリーから before/after バイナリをビルドする再現手順では、ビルド時に `BENCH_HNSW_SEARCH_COMMIT=<sha>` を指定して計測対象コミットをバイナリへ焼き込んでください（未指定時の実行時フォールバックはカレントディレクトリの HEAD を返すため、同一ディレクトリから交互起動する両バイナリに同じ値が記録されます）。CI 非配線・詳細・実測値・採否は `docs/design/hnsw-search.md`「Issue #491」節を参照してください。

visited 集合切替閾値（`sparse_visited_max`。Issue #497）の可視比率別 dense/sparse 前後比較（Issue #498）は、`hnsw_search_bench.rs` へ `--features bench-internals` で `BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX`（`0`＝dense固定 または `18446744073709551615`＝sparse固定の 2 arm のみ。中間値は非 vacuous 検証を単純化できないため拒否）を追加し、`make bench-hnsw-search-visited`（`scripts/bench_hnsw_search_visited_ab.sh`。`AB_PAIRS`／`AB_ROWS`／`AB_MASKS` で規模点・可視率を上書き可）から dense/sparse を交互 N ペアで計測できます。SQL 表層経由の確認は `SWEEP_CANDIDATES=visited SWEEP_SCALES=<n> SWEEP_RATIOS="<n>/<d> ..."  make bench-knn-visible-ratio` で行えます。実測では全測定点で sparse が dense を上回る改善は観測されず、閾値既定値は `0`（既存動作。常に dense）のまま現状維持と判断しました。CI 非配線・詳細・実測値・判断根拠は `docs/design/hnsw-search.md`「Issue #498」節を参照してください。

### `precision` 評価ハーネス（TASK-163）

`crates/engine/tests/precision_eval.rs` は `precision` モード（TASK-162）の
SEARCH-10 の評価指標を、決定的合成コーパス（正解不在クエリを含む）上で実測する
評価ハーネスです。設計判断の記録は `docs/design/precision-eval-regression.md`
を参照してください（指標の定義・実測値・パラメータ感度は spec 側で管理します）。

- 層 A（`cargo test -p fandhe-vector-db-engine --test precision_eval`。`make ci` 対象）: 決定的コーパス
  上で評価を通しで実行し、構造不変条件と測定の決定性のみを検査します（指標の実測値は
  アサートも出力もしません。品質の回帰判定は層 B が担います）。
- 層 B（`make precision-regression`）: 閾値ゲートのみを実行し、指標名と pass/fail
  だけを出力します（閾値の数値も実測値も出力しません。`RECALL_VERBOSE=1`
  〔`GITHUB_ACTIONS` 下では拒否〕opt-in 時のみローカル診断用に実測値を追加出力
  します。Issue #303）。`PRECISION_EVAL_MIN_TOP1_ACC`・
  `PRECISION_EVAL_MIN_MRR10`・`PRECISION_EVAL_MAX_FALSE_RETURN` 環境変数
  と比較して判定します。未設定なら評価は実行しつつ判定をスキップし「ゲート未設定＝
  明示的に対象外」として成功終了、`PRECISION_EVAL_REQUIRE_THRESHOLDS=1`（strict
  モード）では未設定も fail-closed でテスト失敗とします。非数値・範囲外は常に
  fail-closed です。
- 判断材料レポート・感度スイープ（`make precision-report`。**ローカル専用**）:
  hybrid・dense 双方の指標（`precision_eval_report`）と `PrecisionPolicy` の閾値を
  差し替えたパラメータ感度スイープ（`precision_eval_policy_sweep`。hybrid 系列・
  dense 系列）を出力します。実測値を標準出力へ出すため、public runner で動く CI・
  `recall.yml` からは実行しません（`.claude/rules/spec-confidentiality.md`）。
  `GITHUB_ACTIONS` が設定された環境での実行はテスト側が測定前に fail-closed で
  拒否します（Makefile 運用のみに依存しない二重化。Issue #303）。
- **`.github/workflows/recall.yml` への接続は行っていません**: TASK-163 のスコープは
  実測・判断材料の提示までであり目標値の確定は含まないため、上記の
  `PRECISION_EVAL_*` 環境変数は Environment `recall-gate` にまだ設定していません。
  目標値が確定したのち、`RERANK_RECALL_MIN_*` 等と同様に `recall-gate` の Actions
  variables として設定し、`recall.yml` の `recall-regression` job に
  `PRECISION_EVAL_REQUIRE_THRESHOLDS=1` 付きの step を追加してください。

### crates.io への公開（`.github/workflows/release.yml`）

公開名は `fandhe-vector-db-engine`（`crates/engine`）・`fandhe-vector-db-wire-server`
（`crates/wire-server`）。ライブラリ名 `engine`・バイナリ名 `wire-server` は据え置きのため、
ソース内の `use engine::...`・`target/release/wire-server` は不変で、`cargo` の
`-p`/`--package` に渡す名前だけが公開名になる（例: `cargo test -p fandhe-vector-db-engine`）。

公開は `release` workflow の `workflow_dispatch` からのみ行う（タグ push 起点は不採用）。

| 入力 | 値 |
| ---- | -- |
| `crate` | `fandhe-vector-db-engine` / `fandhe-vector-db-wire-server` / `all`（`--workspace` で engine → wire-server の依存順に一括公開。初回公開はこれを使う） |
| `version` | 公開するバージョン。対象クレートの `Cargo.toml` と完全一致が必須 |
| `mode` | `dry-run-only`（既定。ガード群 + `cargo publish --dry-run` のみ） / `publish`（実公開） |

`publish` ジョブは GitHub Environment `crates-io-release` の承認ゲートを通り、secret
`CARGO_REGISTRY_TOKEN` を publish ステップにのみ注入する。Environment の required reviewers と
secret の設定はオーナー作業（初回 run で Environment が自動作成された場合は保護ルールが空の
ため必ず手動で追加する）。`wire-server` 単体の dry-run は engine の公開版が crates.io に
無い間は依存解決で失敗するため、初回は `all` を使う。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。
