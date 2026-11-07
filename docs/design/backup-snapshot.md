# ADR: 稼働中バックアップの方式（読み取りトランザクションからの書き出し）

- ステータス: Accepted（方式）。運用者専用経路の細部は #2152 で確定する（第 9 節）
- 対応: Issue #2150（親 #2149。後続は #2151 engine の書き出し実装・#2152 バックアップ用 CLI・#2153 一貫性と並行書き込みのテスト）
- 関連ビヘイビア: RECOVER-13・PERSIST-4・PERSIST-5・TABLE-3。ポインタ: `docs/spec/04-behavior/recovery.md`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/storage.rs`（`Storage::open_with_durability`・`Storage::db`・`writer_gate`）、`crates/wire-server/src/main.rs`（`hash-password` サブコマンドの分岐）
- 関連 doc: [wasmtime-adoption.md](wasmtime-adoption.md)（リポ外 probe の先例）
- 本 ADR は docs のみの変更であり、`Cargo.toml`・`Cargo.lock`・`deny.toml`・コードを変更しない

## 1. 背景

- 永続化は redb 単一ファイルで、バックアップ機能は未実装である。親 #2149 では「書き込み中のファイルコピーが整合するか」が redb の保証範囲として未確認だった
- 本 ADR は redb 4.2.0 の保証範囲を公開 API とソースから確認し、小規模プロトタイプで実測し、方式を決める

## 2. redb 4.2.0 の保証範囲の結論

引用元は redb 4.2.0 のソース（`src/` 以下の行番号）と同梱 `docs/design.md`。

| # | 事実 | 根拠 |
| - | ---- | ---- |
| 1 | `Database` は排他ロック、`ReadOnlyDatabase` は共有ロックを取る。競合は `DatabaseAlreadyOpen`。`ReadOnlyDatabase` は `Database` と同時に開けない | `tree_store/page_store/file_backend/optimized.rs:29-39`、`db.rs:390`・`db.rs:521` |
| 2 | 読み取りトランザクションは、開始前に commit 済みのデータだけが見える固定のスナップショットで、書き込みと並行に存在できる | `db.rs:370-376`（`ReadableDatabase::begin_read` の doc） |
| 3 | commit は super-header の 2 つの slot と god byte で切り替える。既定の 1PC+C は 1 回の fsync とチェックサム検証で、失敗時は前の slot へ戻る。ページは CoW で、エポック経過後に再利用される | `docs/design.md` の 1PC+C 節・MVCC 節 |
| 4 | 稼働中のファイルをコピーする方法・オンラインバックアップ API の記述は、ドキュメントにも CHANGELOG にも無い | `grep -i backup` で該当なし |
| 5 | `Durability::None` の commit は、プロセス内に留まりファイルに出ないことがある | `storage.rs` の `WriteDurability::None`（PERSIST-5） |
| 6 | 型なしの表は名前と件数・統計しか取れず、key/value のバイト列を反復できない。`experimental-api-5` も追加しない | `transactions.rs:1605`（`list_tables`）、`table.rs:807-840`（`ReadOnlyUntypedTable`）、`Cargo.toml:34-35` |
| 7 | `Builder::create_file(File)` は事前に開いた `File` を受け取る | `db.rs:1544` |

結論は次のとおり。

| 項目 | 結論 |
| ---- | ---- |
| 読み取りトランザクションからの読み出し | 単一 commit 点の一貫した内容。**保証あり**（公開 API の契約） |
| 稼働中ファイルの素朴なコピー | **保証なし**。header・ページ・書き込みバッファの取得時刻がずれうる（事実 3）。open 時の repair は前 slot へ戻すか失敗するだけで、ある commit 点との一致は保証しない |
| 稼働中の別プロセスからの open | **不可**（事実 1。PERSIST-4 と整合） |
| 型なしの全表コピー | **不可**。engine 側で表ごとの型付きコピー関数が要る（事実 6） |
| `Durability::None` 下のファイルコピー | 成功応答済みの commit が欠ける（事実 5。第 3 節で実測） |

engine の表は `TableDefinition<` を含む定義が約 65 行（テスト除く）あり、(K,V) の形が複数で、表名が動的なもの（`catalog/materialized.rs`・`key_index.rs` 等）もある。`MultimapTableDefinition` は未使用。

## 3. プロトタイプによる比較

- 環境: Linux 7.0.0（x86_64・tmpfs）、Intel Core i7-13700K（nproc=10）、rustc 1.90.0、redb `=4.2.0`
- 配置: リポ外の一時 crate で実行し、コミットしない
- データモデル: 表 `meta`（`"commit"` → 通番 k）と表 `rows`（u64 → 512 バイト）。commit k ごとに行を 1 件削除・1 件挿入し、`rows` の内容を k の決定的な関数にする（キー範囲 `k..k+200` ちょうどで値も決定的）。ページ再利用が起きるよう writer スレッドが commit を繰り返す
- 判定: 出力を別に open し、(open 失敗 / `check_integrity` 偽 / 不変条件違反 / 開始前に commit 済みの k より古い) を数える。案 D は出力の k が `begin_read` 時点の k と一致することも見る
- 試行: 条件ごとに 200 回

| 条件 | 方式 | 正常 | open 失敗 | 開始前 commit より古い | 備考 |
| ---- | ---- | ---- | --------- | ---------------------- | ---- |
| `Immediate` | 案 A 高速コピー（`std::fs::copy`） | 164 | 36 | 0 | 失敗は全て「All roots are corrupted」 |
| `Immediate` | 案 A 低速コピー（64KiB ごとに 200us 待つ） | 0 | 200 | 0 | 同上 |
| `Immediate` | 案 D 読み取り Tx 書き出し | 200 | 0 | 0 | 全回で出力の k が `begin_read` 時点と一致。全回で取得中にライブは先へ進んでいた |
| `None` | 案 A 高速コピー | 200 | 0 | 200 | 正常に開くが全回で古い状態（コピー開始前に commit 済みの k が無い） |
| `None` | 案 A 低速コピー | 200 | 0 | 200 | 同上 |
| `None` | 案 D 読み取り Tx 書き出し | 200 | 0 | 0 | 全回で一致 |

- ライブ保持中に `Database::open`・`ReadOnlyDatabase::open` を呼ぶと、どちらも `DatabaseAlreadyOpen` だった
- writer の commit 数は `Immediate` で約 3.1 万、`None` で約 16 万。ライブファイルは約 1.8MB と約 2.1MB まで伸びた
- 注意: 失敗 0 件の観測は保証の根拠にならない。結論は第 2 節の構造分析を主とし、実測は補強である。素朴なコピーの失敗率は負荷・ファイルサイズ・FS に依存する
- 再現の要旨: writer スレッドで `begin_write` → `set_durability` → 行の `remove`/`insert` と `meta` 更新 → `commit` を繰り返す。案 D は `begin_read` → `File::create_new` → `Builder::create_file` → 両表を `iter()` で新ファイルへ `insert` → `commit` → 出力を open して検証する

## 4. 方式の比較と採否

| 案 | 内容 | 採否 |
| -- | ---- | ---- |
| A | 稼働中ファイルの素朴なコピー | **不採用**。保証が無く、実測でも open 不能（`Immediate`）または欠落（`None`） |
| B | 読み取り Tx を握ったうえでのファイルコピー | **不採用**。header は commit ごとに上書きされ、読み取り Tx はファイルの一貫性を固定しない。redb の保証外 |
| C | サーバー停止中のファイルコピー | 補助としてのみ許容。稼働中の読み書きを止めない要件を満たさない |
| D | 読み取り Tx 上で全表を型付きで新ファイルへ書き出す | **採用** |
| E | `ephemeral_savepoint`／`persistent_savepoint` | 不採用。書き込み Tx が要り、復元用でファイル書き出しにならない |

## 5. 採用方式の設計（#2151 への入力）

- engine にプロセス内の書き出し入口を置く（`Storage` 上でスナップショットをパスへ書き出す。名前は #2151 で確定）
- `Storage::db()` の `begin_read` を 1 回だけ取り、全表を読む。`writer_gate`（TABLE-3 の単一ライタの入口）は通さないため、書き込みを止めない
- 表名・接頭辞から型付きコピー関数への対応表を engine に集約する。`list_tables()` の全表が対応表で解決できなければ、書き出しを中止して非 0 にする（黙って欠落させない。fail-closed）。網羅性テストは #2153 に置く
- 出力は一時名に `create_new` と mode `0o600` を同時に指定して作り、`Builder::create_file` へ渡す。書き出し → `Immediate` で commit → fsync → 最終名への公開 → 一時名の `unlink` → 親ディレクトリ fsync の順にする。最終名への公開は上書きを原子的に拒否する操作にする。Unix の通常の `rename` は既存の宛先を置換するため使わず、一時ファイルから最終名への `link`（hard link）を作り、`EEXIST` なら失敗として非 0 にする。事前の存在確認だけでは確認後の競合が残るため使わない。Linux の `renameat2(RENAME_NOREPLACE)` 相当でもよい。可搬性と制約は、`link` は同一ファイルシステム内に限られ hard link 非対応の FS では使えず、`RENAME_NOREPLACE` は Linux 固有で FS 対応が必要で非対応時は `link` へ退避する。出力先の扱いと中断時の挙動の規則は RECOVER-13 (5)(7) に従う
- 出力は `rename` 前は未公開のため、大規模 DB では出力側を複数の書き込み Tx に分けてよい
- 出力は単独で `Storage::open` でき、`check_integrity` で事後検証できること（#2153）
- トレードオフ: 長い読み取り Tx はページ回収を止めるため、取得中はライブファイルが伸びる。同時取得数の上限は #2151・#2152 で検討する

## 6. API 形（運用者の入口）

- 別プロセスは稼働中の DB を開けない（第 2 節の事実 1）ため、稼働中の取得はサーバープロセス内で実行する
- 入口の範囲は RECOVER-13 (4) に従う
- 方向性: `wire-server` のサブコマンド（`hash-password` と同じ引数分岐）を CLI とする。(i) 停止中の DB には CLI プロセス自身が開いて同じ engine API で書き出す。(ii) 稼働中のサーバーには、テナント向けリスナーと分離した運用者専用のローカル経路で、サーバー内の書き出しを起動する
- 稼働中に (i) を実行した場合は `DatabaseAlreadyOpen` で fail-closed とする（非 0。稼働中のプロセスとデータに影響しない）
- 経路 (ii) の具体策（ローカルソケットかシグナルか・認可・終了コードの返し方）は #2152 で決める

## 7. バックアップの範囲

- redb 内に保存される範囲の扱いは RECOVER-13 (1) に従う
- redb の外にあるものは含まれない: `--users` の別ファイル（ロールと SCRAM 検証子）、セッション専用の一時表、プロセス内のみの状態（関数の世代など）
- 認証情報の保存形式は RECOVER-13 (4) に従う。`--users` ファイルの扱いは #2151・#2152 で確認する

## 8. セキュリティ要件と承認事項

- アクセス制御・成果物の権限: RECOVER-13 (4) に従う。認可できない場合は拒否する（fail-closed）
- 機密: 作成時に権限を指定し、作成後の chmod による競合を作らない。エラー・ログに行の値を含めない
- 上書きと不完全ファイル: `create_new` でシンボリックリンク経由の上書きを防ぎ、一時名から最終名への `link`（`EEXIST` で失敗、または `RENAME_NOREPLACE` 相当）による公開で、既存バックアップ・ライブ DB の上書きと不完全な最終ファイルを防ぐ
- インジェクション: 出力パスは CLI 引数でのみ受け取り、SQL・プラン文字列に連結しない
- 承認事項: **新規依存・feature の追加は不要**。標準ライブラリ（`OpenOptions`・`os::unix::fs::OpenOptionsExt`）と既存の redb `=4.2.0` の公開 API で足りる。将来 redb が型なし反復を提供しても、使うには版更新の承認が要る

## 9. 残課題

- 運用者専用経路 (ii) の方式は #2152 で確定した: opt-in の `--backup-socket` による Unix ドメインソケット。認可はファイルシステム権限（親ディレクトリ 0700 相当・ソケット 0600）、プロトコルは出力先 1 行の要求と結果コード 1 行の応答（README の `backup` の節）
- `--users` ファイルは #2152 で対象外とした（README の `backup` の節に明記）
- `--users` ファイルの扱い（#2151・#2152）
- 増分バックアップ・整合検査 CLI・世代管理は本 ADR の範囲外
- ファイルロックに対応しない環境（redb は警告のみで続行する）での挙動は未確認
- 素朴なコピーの失敗率は環境依存であり、本 ADR の実測は 1 環境のみ
