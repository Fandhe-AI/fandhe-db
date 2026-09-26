# ADR: SCRAM チャネルバインディング用 `tls-server-end-point` の実装既定値

- ステータス: Accepted（署名アルゴリズム別ハッシュ選択・TLS 未確立時 `None`
  の 2 点は実装・テストで確定済み。`PLUS` 提示既定 `false` は下記実測に
  基づき、オーナー判断（2026-09-26）で最終確定した。Issue #971 で本 ADR を
  Accepted へ更新）
- 対応: TASK-228・WIRE-9・WIRE-18・HTTP-10 ポインタ（Issue #970・#1088・親 #941）
- 関連: `docs/design/tls-server-handshake.md`・`docs/design/tls-scram-design.md`

## 背景

SCRAM-SHA-256-PLUS（RFC 5802 の `-PLUS` 機構・[WIRE-18] ポインタ）は
`gs2-cbind-flag` に `p=<cb-name>` を伴い、クライアント・サーバー双方が
同一のチャネルバインディング値を算出できることを要求する。本サーバーは
`tls-server-end-point`（RFC 5929 §4）のみを対象とし、算出は
[`crates/wire-server/src/tls/channel_binding.rs`] の
`tls_server_end_point` が担う。

## 決定

### 1. ハッシュ関数の選択（署名アルゴリズム別）

RFC 5929 §4.1 は「証明書の署名に使われたハッシュ関数（MD5・SHA-1 は
SHA-256 へ格上げ）で証明書 DER 全体をハッシュする」ことを要求する。
本実装は `signatureAlgorithm`（`Certificate` 外側 `SEQUENCE` の 2 番目
フィールド）の OID から表引きし、表に無い OID は `None` へ縮退させる
（推測で別のハッシュを当てない fail-closed 判断）。

Ed25519（OID `1.3.101.112`。RFC 8410 §3）は RFC 5929 が定義した当時には
存在せず単一の「署名ハッシュ」を持たない署名方式だが、本サーバーが受理
する唯一の葉鍵種別であるため、SHA-256 を割り当てる決定を本リポの
実装既定値として行った（`crates/wire-server/tests/tls_channel_binding.rs`
の RFC 8410 §10.2 公開テストベクタで、OpenSSL `x509 -fingerprint -sha256`
の独立算出値との一致を固定）。

### 2. TLS 未確立時は `None`

`crate::wire_stream::WireStream::tls_server_end_point` は TLS 未確立の
平文接続では `None` を返す。チャネルバインディングを要求しない
SCRAM-SHA-256（無印）はこの値を一切参照しない。

### 3. SCRAM-SHA-256-PLUS の機構提示は既定で無効

`TlsServerConfig::with_scram_channel_binding`（既定 `false`）を新設し、
`TlsServerConfig::new` はチャネルバインディング値の算出のみ起動時に
1 回行い、機構リストへの `PLUS` 提示可否は別フラグとして分離した。

**根拠（実測）**: `crates/wire-server/tests/wire_scram_plus_psql_interop.rs`
（手動専用・`#[ignore]`。CI 非配線）で psql 18.6・OpenSSL 3.5.5 を用い、
`sslmode=require` かつ `channel_binding` を `disable`／`prefer`／`require`
と変えた 3 通りを、`PLUS` 提示（`advertise_scram_channel_binding`）の
有効・無効それぞれについて本サーバー（Ed25519 葉証明書）に対して実測
した。いずれの組み合わせでも TLS ハンドシェイク自体は成立する
（本フラグは TLS 確立後の SASL 機構リストのみを変えるため）。差は
その後の SCRAM 交換（tls-server-end-point の算出）に現れる:

| `PLUS` 提示 | `disable` | `prefer` | `require` |
| ----------- | --------- | -------- | --------- |
| 無効（既定） | 成功 | 成功 | libpq がクライアント側で拒否（"channel binding is required, but server did not offer ..."）。サーバーが `PLUS` を提示しないため |
| 有効（opt-in） | 成功 | 失敗（`could not find digest for NID UNDEF`） | 失敗（`could not find digest for NID UNDEF`） |

`PLUS` 提示が有効な場合の `prefer`／`require` 失敗は、libpq が
SCRAM-SHA-256-PLUS を選び tls-server-end-point 用のダイジェストを
算出しようとした際に、本サーバーが受理する唯一の葉鍵種別である
Ed25519 の署名アルゴリズムに対応するダイジェストを libpq 側が解決
できないために起きる（TLS ハンドシェイクの失敗ではない）。libpq
側の具体的な実装箇所の特定は本 Issue の対象外（下記「スコープ外」）
とする。

この実測結果に基づき、`PLUS` 機構を既定で提示しないことで、libpq の
既定設定（`channel_binding=prefer`）を使う一般的なクライアントが
（`disable`・`prefer` いずれでも）認証成功する状態を維持する。`PLUS`
を使いたい運用は `TlsServerConfig::with_scram_channel_binding(true)` の
opt-in で有効化できる。CLI からの結線は
`--tls-scram-channel-binding enable|disable`（既定 `disable`。
`crates/wire-server/src/tls_opt.rs`）が担うが、下記§4 のとおり CLI 経由の
`enable` は葉証明書の署名アルゴリズムを条件に起動時拒否されうる。

### 4. CLI の `enable` は、RFC 5929 が定義するハッシュを持たない署名アルゴリズムの葉証明書を起動時に拒否する（オーナー判断 2026-09-26・Issue #1088）

上記§3 の実測結果が示すとおり、`PLUS` 提示を有効化した状態で本サーバーの
Ed25519 葉証明書（署名アルゴリズムが Ed25519＝RFC 5929 が単一ハッシュを
定義しない方式）を使うと、libpq の `channel_binding=prefer`／`require`
は既定の接続すら失敗させる（`disable` のみ成功）。これは「フラグを
有効化すると一般的なクライアントの既定接続が壊れる」という落とし穴で
あり、`--tls-scram-channel-binding enable` を選んだ運用者が気づかずに
踏みうる。

オーナー判断（2026-09-26）により、`--tls-scram-channel-binding enable`
と、葉証明書の署名アルゴリズムに RFC 5929 が定義するハッシュが無い
構成（現時点では Ed25519 のみ。
[`channel_binding::has_rfc5929_defined_hash`]）の組合せは、§3 のように
黙って `PLUS` 非提示へ縮退させるのではなく、**起動時に fail-closed で
拒否する**。判定は `tls_opt::check_scram_channel_binding` が担い、
`main.rs` が TLS 設定読み込み直後・bind 検証より前に 1 回呼ぶ。判定は
`--auth-method` に依存させない（`cleartext` との組合せでも同じく拒否し、
「フラグが効かないまま受理される」経路を作らない）。

判定基準は葉証明書の **鍵種別（SPKI）ではなく `signatureAlgorithm`** で
ある。`x509.rs` は SPKI が Ed25519 であることは強制するが証明書の署名
アルゴリズムまでは強制しないため、Ed25519 鍵の葉を RSA／ECDSA の CA が
署名した構成（例: `ecdsa-with-SHA256`）は RFC 5929 上のハッシュが決まり
libpq も解決できるので、この構成では `enable` を許可する。運用者が
`enable` を使うには、RSA／ECDSA（署名ハッシュが MD5／SHA-1／SHA-256／
SHA-512。SHA-384 は本サーバーの自作実装が未対応のため同じく拒否される）
の CA が署名した葉証明書を用意すればよい（サーバー鍵自体は引き続き
Ed25519 のみ）。

ライブラリ API（`TlsServerConfig::with_scram_channel_binding`）はこの
拒否を行わない。自前クライアントでの `PLUS` 検証テスト
（`tests/wire_scram_plus_tls.rs`・`tests/wire_scram_plus_psql_interop.rs`）
が、起動時拒否の対象になりうる Ed25519 葉証明書のままテストを続行できる
必要があるため（`crates/wire-server/tests/common/tls_client.rs::
test_config_with_scram_channel_binding`）。

`main.rs` のこの起動時拒否は **SQL 表層（`--surface sql`。既定）限定**
である。NoSQL 表層は後述の「スコープ外」節のとおりチャネルバインディング
自体を適用しない（SASL 往復を持たないため `enable` は実際には何も提示
しない no-op。H5・Issue #968）ので、`--surface nosql` では葉証明書の
署名アルゴリズムに関わらず `enable` を無条件で受理する（codex-review
PR #1089 P1 是正: 本判定が表層分岐より前に実行されており、nosql でも
拒否されてしまう回帰が入っていた）。

## 受け入れ基準への対応

- 署名アルゴリズム別のハッシュ選択（SHA-256／SHA-512。非対応は `None`）:
  `channel_binding.rs::classify_signature_oid`・単体テストで固定
- TLS 未確立なら `None`（`p=tls-server-end-point` 非提示・無印 SCRAM の
  みへ縮退）: `wire_stream.rs`・`tls/stream.rs` 経由で固定
- 既知の固定値に対する単体テスト: `tls_channel_binding.rs`
  （合成証明書・RFC 8410 §10.2 公開テストベクタ双方）

## スコープ外

- HTTPS 表層（NoSQL・HTTP-10）へのチャネルバインディング適用
- libpq 以外のクライアント（psycopg・node-postgres 等）での相互運用実測
- 自作 SHA-384 の実装による非対応署名アルゴリズム（sha384WithRSA 等）の
  解消
- libpq 側の失敗箇所の特定（`fe-secure-openssl.c` 等の追跡）そのもの

## 参照

- RFC 5929 §4（`tls-server-end-point`）
- RFC 5802（SCRAM）・RFC 8410 §3（Ed25519 OID）
- `crates/wire-server/src/tls/channel_binding.rs`
- `crates/wire-server/src/tls/server_handshake.rs`
  （`TlsServerConfig::with_scram_channel_binding`）
- `crates/wire-server/src/tls_opt.rs`（`--tls-scram-channel-binding` CLI 結線）
- `crates/wire-server/tests/tls_channel_binding.rs`
- `crates/wire-server/tests/wire_scram_plus_tls.rs`
- `crates/wire-server/tests/wire_scram_plus_psql_interop.rs`（手動専用）
- `crates/wire-server/tests/wire_tls_cli.rs`（R8: `--tls-scram-channel-binding`
  の CLI 結線・機構リスト反映の結合テスト。
  `tls_scram_channel_binding_enable_with_ed25519_signed_leaf_is_rejected`・
  `tls_scram_channel_binding_enable_with_ed25519_signed_leaf_is_rejected_
  under_cleartext_auth`・
  `tls_scram_channel_binding_explicit_disable_does_not_advertise_plus_
  mechanism`・
  `tls_scram_channel_binding_enable_with_ecdsa_sha256_signed_leaf_
  advertises_plus_mechanism` が Issue #1088 の起動時拒否／許可を固定）
