# 運用: ストリーミングレプリケーション（構成・監視・再同期・昇格）

- ステータス: 運用手順（Issue #2179）
- 対応: Issue #2179（親 #2171）。ドキュメントのみの変更で、コード・依存・CI は変えない
- 関連ビヘイビア: RECOVER-15。ポインタ: `docs/spec/04-behavior/recovery.md`。本文は転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連 doc: [replication-streaming.md](replication-streaming.md)（方式と実装判断）・[backup-restore-operations.md](backup-restore-operations.md)（ベースの取得・変更ログ）・[README](../../README.md)（フラグの詳細）
- 記載の既定値・終了コード・標準エラーの固定文言は、実装（`crates/wire-server/src/replication*.rs`・`replica*.rs`、engine の `storage::change_log`）の値である

## 1. 対象と前提

- 構成はプライマリ 1 台とレプリカ 1 台以上である。プライマリは `--change-log`（変更ログ）が前提になる
- レプリカは全テナントのデータのコピーを持つ。レプリカの DB・ベース・退避した旧レプリカは、運用者だけが読める場所に置く（[backup-restore-operations.md](backup-restore-operations.md) の 1 と同じ注意）
- レプリカの動かし方は 2 通りある

| 項目 | `wire-server replica`（適用のみ） | `wire-server --replica-primary`（読み取りレプリカ） |
| ---- | -------------------------------- | -------------------------------------------------- |
| 役割 | プライマリに追従して適用するだけ | 適用に加え、テナント向けリスナー（SQL・NoSQL）で読み取りを返す |
| 書き込み | 受け付けるリスナーがない | 書き込み・DDL・`COPY FROM`・明示 `READ WRITE` は `25006` で拒否 |
| 併用できないもの | — | `--change-log`・`--replication-*`・`--backup-socket` |
| ベースでない DB | 起動拒否 | 起動拒否 |
| 適用停止時 | 終了コード 10・11 で終了 | 同じ終了コードでプロセスごと終了（古いデータを配り続けない） |

## 2. 構成

### 2.1 プライマリ

必須の指定は `--change-log`・`--replication-bind`・`--replication-roles`・`--replication-allow` の 4 つである。どれかが欠けると起動エラーになる。

| フラグ | 内容 | 範囲・既定 |
| ------ | ---- | ---------- |
| `--replication-bind <addr>` | レプリカ専用のリスナー（テナント向け `--bind` とは別ポート） | TLS 未構成ならループバックのみ（非ループバックは起動エラー）。TLS を構成すれば非ループバックも可 |
| `--replication-tls-cert <pem>`・`--replication-tls-key <pem>` | レプリカ専用の TLS 1.3 の証明書・秘密鍵（Ed25519。テナント向けの `--tls-*` とは別） | 両方指定か両方なし。構成すると全接続が TLS 必須（平文は何も返さず閉じる） |
| `--replication-roles <file>` | レプリカ専用のロール（`name:phc` の行）。PHC は `wire-server hash-password` で作る | `--users` とは別のファイル |
| `--replication-allow <ip\|cidr>` | 接続を許す送信元（繰り返し可） | 1 件以上が必須。`/0` は拒否 |
| `--replication-max-connections` | 同時接続数 | 既定 8、上限 256 |
| `--replication-max-unacked-bytes` | 送信済みで未確認の量 | 既定 64 MiB、1024〜4 GiB |
| `--replication-ack-timeout-secs` | 最も古い未確認レコードが ack されない時間の上限 | 既定 30、上限 3600 |
| `--replication-max-lag-records` | ack 済みの位置からプライマリ最新までの遅れの上限。超えたレプリカは切断され、保持位置が外れる | 既定 100000、上限 1000000000 |

- `--change-log-retain-records` による刈り込みは、ack 前の区間を消さない。刈り込み済みの位置や遅れの上限を超えた位置からの再接続は `RESYNC_REQUIRED` で拒否される（4 を参照）
- コマンド例（`<...>` は運用者が置き換える。資格情報の実値を履歴・CI ログに残さない）

```text
wire-server --users <users-file> --db <primary.redb> --change-log \
  --replication-bind 127.0.0.1:<port> --replication-roles <roles-file> \
  --replication-allow 127.0.0.1
```

### 2.2 ベースの作成

1. 稼働中のプライマリから、フルバックアップを新規のパスへ取る（`wire-server backup --socket <path> --out <new-path>`。サーバーは `--backup-socket` 付きで起動しておく。詳細は [backup-restore-operations.md](backup-restore-operations.md) の 2）
2. 取得物をレプリカの `--db` にする。ベースは `--change-log` を付けないまま開く（レプリカ側で変更ログを有効にしない）
3. 稼働中の DB ファイルを `cp` でコピーしない（整合した取得にならない）

### 2.3 レプリカの起動

資格情報は `role:password` の 1 行のファイルで渡す。group・other から読める権限だと起動を拒否する。ファイルを置くディレクトリも運用者だけに絞る。

```text
wire-server replica --db <base.redb> --primary 127.0.0.1:<port> --credentials <cred-file>
wire-server --users <users-file> --db <base.redb> \
  --replica-primary 127.0.0.1:<port> --replica-credentials <cred-file>
```

| フラグ（読み取りレプリカでは `--replica-` 接頭辞） | 内容 | 範囲・既定 |
| ---------------------------------------------- | ---- | ---------- |
| `--max-backoff-secs` | 再接続バックオフの上限 | 既定 30、1〜3600 |
| `--batch-records` | 1 回に適用するレコード数 | 既定 256 |
| `--batch-bytes` | 1 回に適用するバイト数 | 既定 16 MiB、64 KiB〜256 MiB |

接続先（`--primary`）は、`--primary-tls-cert`（読み取りレプリカでは `--replica-primary-tls-cert`）を渡さなければループバックのみである。非ループバックのプライマリへは TLS が必須で、ピンなしは起動エラー（終了コード 2）になる。

### 2.3.1 TLS の構成とピンの配布（Issue #2177）

非ループバックでレプリケーションを使うときは TLS を構成する。受信側はプライマリの証明書を**ピン留め**で検証する（CA チェーン・ホスト名の検証はしない）。

1. プライマリ用に Ed25519 の自己署名の証明書と秘密鍵（PEM）を作り、`--replication-tls-cert`・`--replication-tls-key` で渡す。秘密鍵のファイルは運用者だけが読める権限にする
2. **同じ証明書ファイル**（秘密鍵は渡さない）を受信側へ安全な経路で配り、`--primary-tls-cert <pem>` で指定する。提示された証明書がこのファイルと完全一致し、署名と Finished が検証できて初めて、資格情報（Hello）が送られる
3. 証明書を更新したら、プライマリを再起動し、新しい証明書を受信側へ配って受信側も再起動する（再起動なしの更新は未対応）。古いピンの受信側は終了コード 11 で止まる

```text
wire-server --users <users-file> --db <primary.redb> --change-log \
  --replication-bind 0.0.0.0:<port> --replication-roles <roles-file> \
  --replication-allow <replica-ip> \
  --replication-tls-cert <primary-cert.pem> --replication-tls-key <primary-key.pem>
wire-server replica --db <base.redb> --primary <primary-ip>:<port> \
  --primary-tls-cert <primary-cert.pem> --credentials <cred-file>
```

- TLS を構成したリスナーへの平文の接続は、何も返されずに閉じられる（ループバックの送信元も同じ）
- 証明書の検証に失敗した受信側は、資格情報を送らずに終了コード 11 と固定文言（`the primary's TLS handshake was rejected (check --primary-tls-cert)`）で止まる。原因（どの検査か）は出さない
- ピンの有効期間は受信側の起動時にだけ確かめる

### 2.4 同期レプリケーション（オプション）

既定は非同期である。`--replication-sync-standby <role>`（繰り返し可。`--replication-roles` にあるロール）を指定すると、commit の成功応答の前に、指定した全ロールの確認を待つ。

| フラグ | 内容 |
| ------ | ---- |
| `--replication-sync-commit` | 待つ段階。`remote_write`（受信）・`on`（永続化。既定）・`remote_apply`（適用）。実装上 `on` と `remote_apply` は同じ判定 |
| `--replication-sync-timeout-secs` | 待ちの上限（上限 86400）。指定する場合は `--replication-sync-on-timeout` も必須 |
| `--replication-sync-on-timeout` | `reject`（`state=may_be_committed` のエラー。データはプライマリに残る）／`degrade`（成功を返し、縮退中は待たない。レプリカが追いついたら自動で解除） |

- 上限を指定しない既定では無期限に待つ。**レプリカを失うと書き込みの応答が返らず接続が溜まる**。運用では上限と扱いの指定を推奨する
- 同期ロールの接続は 1 本まで。同期の水準はこの CLI だけで決まり、テナントのセッションからは変えられない
- レプリカは版数 2 のプロトコルで接続する。混在させるときはプライマリを先に更新する

## 3. 監視

観測は運用者の標準エラーだけである（テナントのセッション・HTTP からは遅延を参照できない）。資格情報と行の値は出力しない。

### 3.1 レプリカ側の出力

| 出力 | 意味・対応 |
| ---- | ---------- |
| `wire-server: replica: connected (applied lsn=<n>)` | 接続できた。適用位置から再開した |
| `wire-server: replica: applied lsn=<n> primary_latest=<m> lag=<k>` | 進捗。`lag` が増え続けるなら、プライマリの負荷・ネットワーク・適用の遅さを調べる |
| `wire-server: replica: disconnected, will reconnect` | 一時障害（切断・タイムアウトなど）。上限つきの指数バックオフで自動再接続する。続くなら 2 の指定と疎通を確かめる |
| `wire-server: replica: apply halted (<reason>)` | 適用停止（順序違反・分岐・識別子や版数の不一致）。終了コード 10。何も適用していない。再接続して飛ばして進めない。4 の再同期へ |
| `wire-server: replica: the primary rejected the connection` | プライマリの拒否。終了コード 11 |
| `wire-server: replica: the primary belongs to another database` | ベースの識別子が違う。終了コード 11。ベースを取り直す |
| `wire-server: replica: protocol violation by the primary` / `oversized frame from the primary` / `unexpected frame from the primary` / `the primary reported an error` | プロトコル上の異常。繰り返すなら送信側のバージョンと設定を確かめる |

### 3.2 終了コード

| コード | 意味 |
| ------ | ---- |
| 2 | 引数の不備（起動前に拒否。DB は変わらない） |
| 10 | 適用停止（整合性違反） |
| 11 | プライマリの拒否。`FOREIGN`・`RESYNC_REQUIRED`・`AHEAD`・認証失敗・TLS 証明書のピン不一致など |

一時障害（切断・タイムアウト・`BUSY`・`INTERNAL`）は終了せず自動で再接続する。読み取りレプリカは適用停止でプロセスごと終了するため、systemd などで終了コードを監視し、10・11 で自動再起動を繰り返さない設定にする（再起動しても解消しない）。

### 3.3 プライマリ側の出力

| 出力 | 意味 |
| ---- | ---- |
| `wire-server: replication: replica '<role>' connected` | レプリカが接続した |
| `wire-server: replication: replica '<role>' disconnected (<reason>)` | 切断。`<reason>` に `replica lag limit exceeded` などが入る。上限を超えたレプリカは保持位置が外れ、再接続時に `RESYNC_REQUIRED` になりうる |
| `engine: synchronous replication degraded after a timeout` | 同期が縮退した（`degrade` 指定）。待たずに成功を返している間は、同期の保証がない |
| `engine: synchronous replication restored after degradation` | 縮退が解除された |

## 4. 再同期

再同期が要る契機は、`RESYNC_REQUIRED`（刈り込み済み・遅れの上限超過）、終了コード 10 の分岐・順序違反、ベースの識別子の不一致である。

専用の再同期のコマンドは Issue #2176 で追加中である。本書の時点では、次の手順でベースを取り直す。

1. レプリカ（読み取りレプリカを含む）を止める
2. 古いレプリカの DB を退避または削除する。退避したものも全テナントのデータのコピーである
3. プライマリから新しいフルバックアップを取る（2.2）
4. 新しいベースでレプリカを起動する（2.3）

再同期を避ける設定の目安は、`--replication-max-lag-records` と `--change-log-retain-records`（DB 内の change_log の保持）を、レプリカが止まりうる時間の書き込み量より大きくすることである。追従できるかどうかは、この 2 つと DB 内のログ保持だけで決まる。送信側は DB 内の change_log 表だけを読み、アーカイブから刈り込み済みの区間を補わない。アーカイブの保持を長くしても、DB 内の必要なログが刈り込まれれば再同期が必要になる。アーカイブの保持は復元（PITR）用であり、再同期の回避には効かない。

## 5. 昇格（フェイルオーバー）

運用者の明示操作でレプリカをプライマリへ昇格する（Issue #3132。RECOVER-15 (4)(5) がポインタで、本文は転記しない）。**自動フェイルオーバー・自動昇格は対象外**で、どのレプリカを昇格するかは運用者が決める。昇格は停止中の DB に対するオフライン操作である。判断は [replication-streaming.md](replication-streaming.md) の「実装判断（Issue #3132）」。

### 5.1 手順

1. 昇格するレプリカのプロセス（`wire-server replica` または読み取りレプリカ）を止める。旧プライマリが生きていれば止める（書き込みの分岐を避ける）
2. `wire-server replica-promote --db <レプリカ DB>` を実行する。整合検査を通ったときだけ、1 本の保守 commit で変更ログの timeline を分岐させる
3. 昇格した DB を `wire-server --users <f> --db <レプリカ DB> --change-log ...` でプライマリとして起動する。他のレプリカへ配信するなら `--replication-*` も付ける。読み取り専用は起動フラグ（`--replica-primary`）が持つもので、付けずに起動すれば解ける

標準出力（成功時のみ。運用者向けで、テナントのセッションには見せない）:

- `replica-promote done: applied_lsn=<N>`: 昇格時点で確定している適用済みの commit 番号
- `replica-promote: commits after lsn=<N> on the former primary were not received and are lost`: 失われる範囲。非同期では、旧プライマリで確認済みの commit のうち `<N>` より後は、このレプリカにない。レプリカ側からは上限が分からないため、`<N>` より後すべてと報告する。同期モードで ack を受けた commit は含まれない

終了コード:

| コード | 意味 |
| ------ | ---- |
| 0 | 昇格した |
| 1 | 実行時の失敗（別のプロセスが DB を開いている・開けない・保守 commit の失敗）。DB は変わらない |
| 2 | 引数の不備（DB は開かない） |
| 4・5 | 整合検査の矛盾・検査不能。昇格しない（`verify` で詳細を確かめる） |
| 10 | レプリカのベースでない（変更ログのメタが無い）・変更ログが commit 番号と連続しない。DB は変わらない |

### 5.2 昇格後の運用

- 新しいプライマリは、旧プライマリと**別の** `--change-log-archive-dir` を使う。同じ置き場へ書くと timeline が混在し、`change-log-verify`・PITR が拒否する
- 旧プライマリに追従していた他のレプリカは、timeline の不一致（`FOREIGN`。終了コード 11）で止まる。新しいプライマリのフルバックアップから `replica-resync` で作り直す（timeline をまたいだ追従は未対応）
- 旧プライマリが復旧しても、昇格した DB は別の timeline のため、旧プライマリへ `replica` で再接続すると拒否される（取り違えの防止）。旧プライマリを戻すには新しいプライマリから再同期する。退避した旧 DB も全テナントのデータのコピーである
- 昇格したプライマリのフルバックアップから、PITR・増分の新しい起点を取る
- 台帳（operation_id。RECOVER-15 (5)）は通常の表として複製済みで、昇格のための変更はない。非同期で失われた commit は、再送されると新しい書き込みとして受理されうる

### 5.3 既知の制約

- 停止中の**プライマリ**の DB ファイルは、変更ログを付けずに開くとレプリカのベースと区別できない。これに `replica-promote` をかけると timeline が分岐し、そのプライマリのレプリカは `FOREIGN` で拒否されて再同期が要る（データは失われない）。対象のファイルがレプリカのものであることは運用者が確かめる
- `replica-promote` を通さずに、レプリカの DB に `--change-log` を付けて起動してプライマリとして使うことはしない。旧プライマリとの履歴の識別が保証されない。未検証・非対応として扱う
- 整合検査（読み取り専用）と排他オープンの間に窓がある。運用者のファイルに対する検査で、排他オープン後は他のプロセスが書けない
- 分岐後の timeline は時計（マイクロ秒）に基づく。同じマイクロ秒に 2 台を昇格する・時計が戻った場合は衝突しうるため、昇格するのは 1 台だけにする
- プライマリを失ったときのもう一つの手段は、プライマリの DB とアーカイブからの復元と PITR（[backup-restore-operations.md](backup-restore-operations.md) の 4）
- 同期モードで ack を受けた commit が、プライマリの SIGKILL の後もレプリカにあることは、障害注入試験の段 6 で確かめている

## 6. 未対応の範囲

| 項目 | 状況 |
| ---- | ---- |
| 自動フェイルオーバー・自動昇格 | 対象外（RECOVER-15 (4) ポインタ） |
| 運用者の CLI による昇格 | 実装済み（Issue #3132。5 を参照）。稼働中の昇格・timeline をまたぐ追従は未対応 |
| 再同期の専用コマンド | Issue #2176 で追加中。入ったら 4 を更新する |
| 相互 TLS（レプリカのクライアント証明書）・CA チェーンとホスト名の検証・再起動なしの証明書更新 | 未対応（ピン留めのみ。2.3.1 を参照） |
| quorum（`ANY n`／`FIRST n`）・セッション単位の同期の水準・待機の cancel | 未対応 |
| カスケード（レプリカからの再配信）・レプリカからのバックアップ | 未対応 |
| PostgreSQL 互換の観測口（`pg_is_in_recovery()` など） | 未対応 |
| HNSW opt-in との併用 | 未検証 |

## 7. セキュリティ上の注意

- TLS を構成しないリスナーはループバックのみ。port-forward やプロキシで外へ出すと、資格情報とレコードが平文で流れるので、外へ出すときは TLS を構成する（2.3.1）
- TLS を構成したリスナーは全接続が TLS 必須で、非ループバックの bind が使える。受信側はピンと一致する証明書にだけ資格情報を送る
- `--replication-roles` と `--users` は別のファイル・別の名前空間である。テナントのロールではレプリカ用のリスナーに接続できない
- 資格情報ファイルは group・other から読めない権限にする。認証に失敗し続ける送信元は、バックオフの間（最大 60 秒）何も返されずに閉じられる
- 許可リストは 1 件以上が必須で、`/0` は拒否される。許可外の送信元には何も返さず閉じる
- 同期モードでは、commit の待ち時間からレプリカの状態をある程度推測できる（[replication-streaming.md](replication-streaming.md) に記載の既知の制約）

## 8. 障害注入試験による確認範囲

`scripts/crash_test_replication.sh`（`make crash-test-replication`。CI の既存 crash-test ジョブで実行）が確かめている範囲。

| 段 | 内容 |
| -- | ---- |
| 1 | レプリカの SIGKILL（書き込み中）。停止中の DB が verify を通り、適用位置が単調非減少 |
| 2 | 接続断（両プロセス生存）。再起動なしで再接続 |
| 3 | 中継の停滞・SIGSTOP による ack 待ちの上限の発火と再接続 |
| 4 | DDL が書き込み中でも複製される |
| 5 | プライマリの SIGKILL と再起動。ack 済みの commit が失われず、レプリカが先に進んで停止しない |
| 6 | 同期レプリケーション。ack を受けた commit が、プライマリの SIGKILL の後もレプリカにある |

確かめていないもの: 遅れの上限超過と再同期、TLS と非ループバック（結合試験では確認済みで、障害注入の対象外）、電源断・カーネル層の分断、quorum、HNSW opt-in との併用。

## 9. 関連

- [replication-streaming.md](replication-streaming.md)・[backup-restore-operations.md](backup-restore-operations.md)・[README](../../README.md) のレプリケーション各節
- [spec-confidentiality](../../.claude/rules/spec-confidentiality.md)
