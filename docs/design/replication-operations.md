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
| `--replication-bind <addr>` | レプリカ専用のリスナー（テナント向け `--bind` とは別ポート） | ループバックのみ。非ループバックは起動エラー |
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

接続先（`--primary`）はループバックのみである。

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
| 11 | プライマリの拒否。`FOREIGN`・`RESYNC_REQUIRED`・`AHEAD`・認証失敗など |

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

- **運用者の CLI による昇格は未実装である**。RECOVER-15 (4) がポインタ（本文は転記しない）。追跡用の sub-issue はまだない
- 代わりの手順は用意しない。とくに、レプリカの DB に `--change-log` を付けて起動してプライマリとして使うことはしない。この経路は昇格として検証されておらず、旧プライマリとの履歴の識別が保証されない。未検証・非対応として扱う
- プライマリを失ったとき、現状でできるのは、プライマリの DB とアーカイブからの復元と PITR である（[backup-restore-operations.md](backup-restore-operations.md) の 4）
- 同期モードで ack を受けた commit が、プライマリの SIGKILL の後もレプリカにあることは、障害注入試験の段 6 で確かめている

## 6. 未対応の範囲

| 項目 | 状況 |
| ---- | ---- |
| 自動フェイルオーバー・自動昇格 | 対象外（RECOVER-15 (4) ポインタ） |
| 運用者の CLI による昇格 | 未実装。追跡 Issue なし |
| 再同期の専用コマンド | Issue #2176 で追加中。入ったら 4 を更新する |
| TLS と非ループバック接続 | Issue #2177。それまで外へ出さない |
| quorum（`ANY n`／`FIRST n`）・セッション単位の同期の水準・待機の cancel | 未対応 |
| カスケード（レプリカからの再配信）・レプリカからのバックアップ | 未対応 |
| PostgreSQL 互換の観測口（`pg_is_in_recovery()` など） | 未対応 |
| HNSW opt-in との併用 | 未検証 |

## 7. セキュリティ上の注意

- リスナーはループバックのみ。port-forward やプロキシで外へ出すと、資格情報とレコードが平文で流れる。TLS（#2177）が入るまで外へ出さない
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

確かめていないもの: 遅れの上限超過と再同期、TLS と非ループバック、電源断・カーネル層の分断、quorum、HNSW opt-in との併用。

## 9. 関連

- [replication-streaming.md](replication-streaming.md)・[backup-restore-operations.md](backup-restore-operations.md)・[README](../../README.md) のレプリケーション各節
- [spec-confidentiality](../../.claude/rules/spec-confidentiality.md)
