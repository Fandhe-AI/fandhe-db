//! ユーザーストア・Argon2id 照合・`engine::policy::PolicyContext` へのテナント導出を担う。
//!
//! `handshake.rs` の認証フローから [`verify`] が呼ばれ、成功時は `engine` クレートの
//! `PolicyContext`（テナント境界・可視性判定の唯一の入力経路）を返す。テナントは
//! このユーザーストアのフィールドからのみ導出し、クライアント自己申告値は参照しない
//! （fail-closed の設計判断）。
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-2, WIRE-3）。

pub mod argon2id;
pub mod base64_std;
pub mod blake2b;
pub mod hmac_sha256;
pub mod scram;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use argon2id::Params;
use engine::storage::Visibility;

/// 認証成功時に導出する `PolicyContext` が持つ可視性集合を一箇所に定める
/// ヘルパー（RLS-11・TASK-195。ポインタ: `docs/spec/04-behavior/*.md`）。
/// `UserStore::load_from_file` の起動時検証（tenant_id の妥当性のみを確認）と
/// [`verify`] の実行時導出が同一コンストラクタを使うことで、両者の意味的な
/// 乖離（起動時は通るが実行時の可視性集合が異なる、といった食い違い）を防ぐ。
///
/// 可視性は `Public` ＋ 自テナントの `Private`（read-your-writes。書いた本人が
/// 同一テナントの別セッションも含めて commit 済みの自分の行を読み戻せる）。
/// 他テナントの `Private` 行は `engine::policy::PolicyContext::is_visible` の
/// テナント一致判定により不可視のまま（このヘルパーはあくまで「自テナントに
/// 何を許可するか」の集合であり、テナント境界そのものは engine 側の判定が担う）。
///
/// engine 側 `crate::policy::PolicyContext::new`（既定 = `Public` のみ）は
/// 変更しない。可視性の拡張はこの wire 認証導出点に閉じる。
pub(crate) fn session_policy_context(
    tenant_id: &str,
) -> Result<engine::policy::PolicyContext, engine::policy::PolicyError> {
    engine::policy::PolicyContext::with_visibilities(
        tenant_id,
        [Visibility::Public, Visibility::Private],
    )
}

/// 認証失敗時に課す固定遅延。ポインタ: TASK-67・WIRE-3（`docs/spec/04-behavior/wire-protocol.md`）。
///
/// `pub`（Issue #752）: NoSQL 表層の `POST /v1/session`
/// （`http::session::issue`。TASK-174・HTTP-6）も [`verify`] を直接呼び、この
/// 固定遅延・ダミー KDF による対称性を SQL wire と共有する。結合テストが
/// 「応答時間が最低この値以上である」ことを検証する際の単一情報源として
/// 参照できるよう公開する（値そのものはこの公開より前から変わらない）。
pub const AUTH_FAILURE_DELAY: Duration = Duration::from_millis(200);

/// wire プロトコルの認証失敗応答が用いる SQLSTATE（invalid_password）。値は
/// `engine::error_format::ErrorClass`（SSOT。TASK-152・ERR-2）を単一の真実源とし、
/// 本モジュールで文字列リテラルを独自に持たない（TASK-153・ERR-1 の分散定数
/// SSOT 化。`const fn` のため const コンテキストでもそのまま使える）。
pub const SQLSTATE_INVALID_PASSWORD: &str =
    engine::error_format::ErrorClass::AuthInvalid.wire_code();

/// サーバー全体で一意に選ぶ認証方式（Issue #940・WIRE-18・TASK-222）。
/// ユーザーごとに切り替えない（`Authentication*` の種類自体からユーザーの
/// 存在・方式が判別できてしまうのを避けるため）。`--auth-method` CLI
/// （`auth_method_opt.rs`）が唯一の入口。未指定時は `Cleartext` で、これは
/// 本 Issue 以前の挙動とビット同一のまま不変。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMethod {
    #[default]
    Cleartext,
    ScramSha256,
}

/// ユーザーストア 1 行分のレコード（`username:tenant_id:phc[:scram_verifier]` 形式）。
struct UserRecord {
    tenant_id: String,
    phc: String,
    scram_verifier: Option<scram::ScramVerifier>,
}

/// サーバー起動時に `--users` で読み込むユーザーストア。認証主体からテナントを
/// 一意に導出できる唯一の情報源であり、全レコードが必ず `tenant_id` を持つ
/// （テナント無所属の特権ログイン主体を構造的に作らない。WIRE-2）。
///
/// `auth_method`（既定 [`AuthMethod::Cleartext`]）は [`UserStore::require_scram`]
/// による opt-in でのみ [`AuthMethod::ScramSha256`] へ切り替わる。
pub struct UserStore {
    users: HashMap<String, UserRecord>,
    auth_method: AuthMethod,
    /// 未知ユーザー向けモック検証子（[`scram::mock_verifier`]）の鍵。
    ///
    /// P0 review 指摘（Issue #940 PR #1006）: 以前はロード済み全レコードの
    /// ServerKey をファイル行順に連結して導出していたが、ユーザーストアの
    /// 追加・削除・並べ替えや検証子更新の前後でこの値が変化してしまい、
    /// 更新前後の `server-first` の salt を観測できるクライアントに
    /// 未知ユーザーの登録有無を推測させる存在オラクルになっていた
    /// （テナント境界: エラー時の存在情報漏えい違反）。修正後は
    /// `UserStore::require_scram` の呼び出し元（`main.rs` の
    /// `--scram-mock-key-file`）が渡す、ユーザーレコード集合から独立した
    /// 外部秘密（プロセス再起動・ストア更新をまたいで運用者が同じ値を
    /// 与え続ける限り不変）からのみ導出する。`Cleartext` モードでは
    /// `None`（このフィールドを参照する `authenticate_scram` は
    /// `ScramSha256` のときにしか呼ばれない）。
    scram_mock_key: Option<[u8; scram::KEY_LEN]>,
    /// DDL（`CREATE TABLE`・`DROP TABLE` 等）実行権限を持つ username 集合
    /// （SQL-23・TASK-202・TASK-203、Issue #899・#902。`--ddl-allowed-users`
    /// からのみ設定する）。`users` に実在しない username は
    /// [`UserStore::with_ddl_allowed_users`] が起動時エラーとして拒否するため、
    /// 本フィールドの要素は必ず `users` のキーの部分集合になる。既定は空集合
    /// （全 DDL 文が `42501` で拒否される。`sql::ddl::require_ddl_permission`
    /// ドキュメント参照）。
    ddl_allowed: HashSet<String>,
}

/// [`UserStore::load_from_file`] のロード時検証エラー（fail-closed。起動を中断する）。
#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    /// `line`: 1 起算の行番号。util フィールド値そのもの（平文パスワード等）は
    /// 含まれないため、起動時診断としてそのまま表示してよい。
    MalformedLine {
        line: usize,
    },
    DuplicateUsername {
        line: usize,
    },
    EmptyUsername {
        line: usize,
    },
    EmptyTenantId {
        line: usize,
    },
    InvalidPhc {
        line: usize,
    },
    /// `tenant_id` が `engine::policy::PolicyContext` の構築時検証（長さ上限等）を
    /// 満たさない。
    InvalidTenantId {
        line: usize,
    },
    /// 4 番目のフィールド（SCRAM 検証子）が構文的に不正、または
    /// [`scram::SCRAM_ITERATIONS`]／[`scram::SALT_LEN`] への完全一致を満たさない
    /// （タイミング側チャネル対策。PHC の `RECOMMENDED_PARAMS` 完全一致検証と
    /// 同じ設計判断）。
    InvalidScramVerifier {
        line: usize,
    },
    /// [`UserStore::require_scram`] が検証子を持たないレコードを検出した
    /// （`ScramSha256` モードでは全レコードが検証子を持つ必要がある）。
    MissingScramVerifier {
        username: String,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "user store: I/O error: {e}"),
            LoadError::MalformedLine { line } => {
                write!(
                    f,
                    "user store: malformed line {line} (expected username:tenant_id:phc)"
                )
            }
            LoadError::DuplicateUsername { line } => {
                write!(f, "user store: duplicate username at line {line}")
            }
            LoadError::EmptyUsername { line } => {
                write!(f, "user store: empty username at line {line}")
            }
            LoadError::EmptyTenantId { line } => {
                write!(f, "user store: empty tenant_id at line {line}")
            }
            LoadError::InvalidPhc { line } => {
                write!(f, "user store: invalid PHC hash at line {line}")
            }
            LoadError::InvalidTenantId { line } => {
                write!(
                    f,
                    "user store: tenant_id rejected by policy context at line {line}"
                )
            }
            LoadError::InvalidScramVerifier { line } => {
                write!(f, "user store: invalid SCRAM verifier at line {line}")
            }
            LoadError::MissingScramVerifier { username } => {
                write!(
                    f,
                    "user store: user '{username}' has no SCRAM verifier (required in scram-sha-256 auth mode)"
                )
            }
        }
    }
}

impl std::error::Error for LoadError {}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

impl UserStore {
    /// `username:tenant_id:phc` 形式のファイルをロードする。重複 username・空
    /// username/tenant_id・不正 PHC・`PolicyContext` が拒否する tenant_id はすべて
    /// 起動失敗として扱う（fail-closed。実行時に初めて発覚させない）。
    ///
    /// PHC の Argon2id パラメータは [`argon2id::RECOMMENDED_PARAMS`]（[`dummy_phc`] が
    /// 使う値）への完全一致のみを受理する（P0 review 指摘: 個々のレコードが
    /// 「許容範囲内」の異なる m/t/p を持てると、レコードごとに実際の KDF 計算
    /// コストが変わり、未知ユーザー用のダミー PHC とのコスト差が
    /// `AUTH_FAILURE_DELAY` の固定遅延で吸収しきれない場合、応答時間からユーザーの
    /// 存在・レコードのコスト特性が列挙されうる。「認証エラー時に存在情報を
    /// 漏らさない」テナント境界基準に反するため、範囲受理をやめ単一の既定値へ
    /// 完全一致させることで既知・未知ユーザーの KDF コストを常に一致させる）。
    pub fn load_from_file(path: &Path) -> Result<Self, LoadError> {
        let content = std::fs::read_to_string(path)?;
        let mut users = HashMap::new();

        for (idx, raw_line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            // 4 番目のフィールド（SCRAM 検証子。`splitn` により内部の `:` は
            // 保持される）は opt-in。3 フィールドの行は従来どおり読み込める。
            // username・tenant_id に `:` が混入した行は 4 要素以上になり、かつ
            // 4 番目のフィールドが `SCRAM-SHA-256$` で始まらないため
            // `MalformedLine` として自動的に拒否される（余分なロジックなしで
            // 既存契約を維持する）。
            let parts: Vec<&str> = line.splitn(4, ':').collect();
            if parts.len() != 3 && parts.len() != 4 {
                return Err(LoadError::MalformedLine { line: line_no });
            }
            let username = parts
                .first()
                .copied()
                .ok_or(LoadError::MalformedLine { line: line_no })?;
            let tenant_id = parts
                .get(1)
                .copied()
                .ok_or(LoadError::MalformedLine { line: line_no })?;
            let phc = parts
                .get(2)
                .copied()
                .ok_or(LoadError::MalformedLine { line: line_no })?;
            let scram_field = parts.get(3).copied();
            // 4 番目のフィールドがあるのに SCRAM 検証子の接頭辞で始まらない
            // 行は、そもそも username・tenant_id に `:` が混入した 3 フィールド
            // 行の誤解釈である可能性が高いため `MalformedLine` として拒否する
            // （review 指摘の再現ケース: `ali:ce:tenant-a:{phc}` のような行を
            // 「tenant_id=ce, phc=tenant-a, scram_verifier=不正な検証子」と
            // 誤って受理しないための既存契約〔`:` 混入行の拒否〕を維持する）。
            if let Some(field) = scram_field {
                if !field.starts_with("SCRAM-SHA-256$") {
                    return Err(LoadError::MalformedLine { line: line_no });
                }
            }

            if username.is_empty() {
                return Err(LoadError::EmptyUsername { line: line_no });
            }
            if tenant_id.is_empty() {
                return Err(LoadError::EmptyTenantId { line: line_no });
            }
            // 構文検証（parse_phc）に加え、Argon2id パラメータが
            // `argon2id::RECOMMENDED_PARAMS` と完全一致することを検証する
            // （fail-closed。実行時に初めて発覚させない）。この完全一致検証は
            // 以下の両方を兼ねる:
            // (1) OOM 対策: m_cost_kib 等が極端な範囲の構文的に正しい PHC が
            //     `hash_raw` 呼び出し時まで検出されずプロセス全体のクラッシュに
            //     波及することを防ぐ（`RECOMMENDED_PARAMS` が `validate_params`
            //     の運用上限内であることは
            //     `argon2id::tests::validate_params_accepts_recommended_params`
            //     で回帰確認済みのため、完全一致は自動的に上限内を意味する）
            // (2) タイミング側チャネル対策（P0 review 指摘）: 個々のレコードが
            //     異なる m/t/p を持てると、既知ユーザーの KDF コストが未知
            //     ユーザー用ダミー PHC（常に `RECOMMENDED_PARAMS`）と食い違い、
            //     `AUTH_FAILURE_DELAY` の固定遅延で吸収しきれない差が応答時間に
            //     現れてユーザーの存在・レコードのコスト特性を列挙されうる
            let (params, _, _) =
                argon2id::parse_phc(phc).map_err(|_| LoadError::InvalidPhc { line: line_no })?;
            if params != argon2id::RECOMMENDED_PARAMS {
                return Err(LoadError::InvalidPhc { line: line_no });
            }
            session_policy_context(tenant_id)
                .map_err(|_| LoadError::InvalidTenantId { line: line_no })?;

            let scram_verifier = match scram_field {
                None => None,
                Some(field) => {
                    let verifier = scram::ScramVerifier::parse(field)
                        .map_err(|_| LoadError::InvalidScramVerifier { line: line_no })?;
                    // 反復回数・salt 長はレコードごとに異ならせない（タイミング
                    // 側チャネル対策。PHC の `RECOMMENDED_PARAMS` 完全一致検証と
                    // 同じ設計判断。鍵長は `ScramVerifier::parse` が型
                    // （`[u8; KEY_LEN]`）で既に保証する）。
                    if verifier.iterations != scram::SCRAM_ITERATIONS
                        || verifier.salt.len() != scram::SALT_LEN
                    {
                        return Err(LoadError::InvalidScramVerifier { line: line_no });
                    }
                    Some(verifier)
                }
            };

            if users
                .insert(
                    username.to_string(),
                    UserRecord {
                        tenant_id: tenant_id.to_string(),
                        phc: phc.to_string(),
                        scram_verifier,
                    },
                )
                .is_some()
            {
                return Err(LoadError::DuplicateUsername { line: line_no });
            }
        }

        Ok(Self {
            users,
            auth_method: AuthMethod::default(),
            scram_mock_key: None,
            ddl_allowed: HashSet::new(),
        })
    }

    /// 認証方式を [`AuthMethod::ScramSha256`] へ切り替える opt-in（`--auth-method
    /// scram-sha-256` からのみ呼ばれる）。全レコードが SCRAM 検証子を持つことを
    /// 要求し、欠けていれば起動失敗とする（fail-closed。SCRAM モードでは
    /// Argon2id の PHC を一切照合に使わないため、検証子を欠くレコードは
    /// 恒久的にログイン不能になる、というだけでなく、そのレコードだけ
    /// モック相当の扱いになりタイミング対称性が崩れるのを未然に防ぐ）。
    ///
    /// `mock_key_secret`（P0 review 指摘・Issue #940 PR #1006）: 未知
    /// ユーザー向けモック検証子の鍵をここから導出する外部秘密。ユーザー
    /// レコード集合から独立しており、`--scram-mock-key-file`（`main.rs`）が
    /// 読んだファイルの生バイト列をそのまま渡す契約。運用者が同じ秘密
    /// ファイルを使い続ける限り、ユーザーストアの追加・削除・並べ替えや
    /// 検証子更新の前後でモック salt は変化しない（存在オラクル対策）。
    pub fn require_scram(mut self, mock_key_secret: &[u8]) -> Result<Self, LoadError> {
        let mut usernames: Vec<&String> = self.users.keys().collect();
        usernames.sort();
        for username in usernames {
            let has_verifier = self
                .users
                .get(username)
                .map(|r| r.scram_verifier.is_some())
                .unwrap_or(false);
            if !has_verifier {
                return Err(LoadError::MissingScramVerifier {
                    username: username.clone(),
                });
            }
        }
        self.auth_method = AuthMethod::ScramSha256;
        self.scram_mock_key = Some(derive_scram_mock_key(mock_key_secret));
        Ok(self)
    }

    /// このストアが選択している認証方式。`handshake.rs` が `Authentication*`
    /// の種類を分岐する唯一の情報源。
    pub fn auth_method(&self) -> AuthMethod {
        self.auth_method
    }

    /// モック検証子の鍵（[`scram::mock_verifier`] へ渡す）。`Cleartext`
    /// モードや（構造的に到達しないはずだが）`require_scram` 未経由の
    /// `ScramSha256` では `None`。
    pub fn scram_mock_key(&self) -> Option<&[u8; scram::KEY_LEN]> {
        self.scram_mock_key.as_ref()
    }

    /// SCRAM 認証向けの照合情報を返す。ユーザーが存在しない、または
    /// （通常到達しないはずだが）検証子を欠く場合は `None`（呼び出し元の
    /// `handshake.rs` がモック検証子へフォールバックする）。
    pub fn scram_lookup(&self, username: &str) -> Option<(&str, &scram::ScramVerifier)> {
        let record = self.users.get(username)?;
        let verifier = record.scram_verifier.as_ref()?;
        Some((record.tenant_id.as_str(), verifier))
    }

    /// ロード済みレコード数。ユーザー名・テナント ID 等の存在情報は含まない
    /// （wire 経路からは呼ばない。テストのフィクスチャ診断専用。Issue #172）。
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// レコードが 1 件も無いか（`len() == 0`）。`clippy::len_without_is_empty` 対応。
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// DDL 実行権限を持つ username 集合を設定する opt-in（`--ddl-allowed-users`
    /// からのみ呼ばれる。Issue #902・SQL-23・TASK-203）。`--search-engine` 等の
    /// 他の起動時 opt-in と同じく、起動後に変更できない構成値として一度だけ
    /// 適用する想定（`main.rs` が重複指定自体は CLI 引数側で拒否する）。
    ///
    /// `usernames` に `self.users`（`--users` で読み込んだユーザーストア）へ
    /// 実在しない username が含まれる場合は起動失敗として扱う（fail-closed。
    /// typo で意図した username への権限付与が黙って無効になる事故を防ぐ。
    /// `require_scram` の「欠落レコードは起動失敗」と同じ設計判断）。
    pub fn with_ddl_allowed_users(mut self, usernames: &[String]) -> Result<Self, String> {
        for username in usernames {
            if !self.users.contains_key(username) {
                return Err(format!(
                    "{FLAG} lists unknown username {username:?} (not present in --users)",
                    FLAG = crate::ddl_permission_opt::FLAG
                ));
            }
        }
        self.ddl_allowed = usernames.iter().cloned().collect();
        Ok(self)
    }

    /// `username` が DDL 実行権限を持つか（Issue #902）。`handshake.rs` が
    /// 認証成功直後に 1 回だけ参照し、真の場合に限り
    /// `engine::sql::mode::SessionState::allow_ddl` を呼ぶ。
    pub fn is_ddl_allowed(&self, username: &str) -> bool {
        self.ddl_allowed.contains(username)
    }

    #[cfg(test)]
    fn from_records(records: Vec<(&str, &str, &str)>) -> Self {
        let mut users = HashMap::new();
        for (username, tenant_id, phc) in records {
            users.insert(
                username.to_string(),
                UserRecord {
                    tenant_id: tenant_id.to_string(),
                    phc: phc.to_string(),
                    scram_verifier: None,
                },
            );
        }
        Self {
            users,
            auth_method: AuthMethod::default(),
            scram_mock_key: None,
            ddl_allowed: HashSet::new(),
        }
    }
}

/// [`UserStore::scram_mock_key`] の導出（P0 review 指摘・Issue #940 PR #1006 で
/// 再設計）。ユーザーレコード集合から独立した外部秘密（`--scram-mock-key-file`
/// の生バイト列）にドメインタグを付けて SHA-256 するのみで、ユーザーストアの
/// 内容（ServerKey・行順）を一切参照しない。同じ秘密ファイルを使い続ける限り
/// ストア更新・プロセス再起動をまたいで安定する（存在オラクル対策）。
fn derive_scram_mock_key(secret: &[u8]) -> [u8; scram::KEY_LEN] {
    let mut input = Vec::with_capacity(32 + secret.len());
    input.extend_from_slice(b"vector-db/scram/mock-key/v2");
    input.extend_from_slice(secret);
    engine::crypto::sha256::digest(&input)
}

/// cleartext password 認証の失敗を表す（ポインタ: TASK-67・WIRE-3）。
#[derive(Debug)]
pub struct AuthFailure;

impl AuthFailure {
    /// クライアントへ返す ErrorResponse 本文（英語固定。プログラム出力文字列は
    /// 英語とする規約 [japanese-style.md] に従う）。
    pub const MESSAGE: &'static str = "password authentication failed";
}

/// 未知ユーザーに対しても実ユーザーと同一コストで照合を走らせるためのダミー PHC
/// レコード（ポインタ: TASK-67・WIRE-2）。salt・hash は固定値でよい（値自体は
/// 秘密ではなく、計算コストを揃えることだけが目的のため）。プロセス内で一度だけ
/// 生成して再利用する。
///
/// [`argon2id::synthesize_phc_without_hashing`] で組み立て、`encode_phc` は使わない
/// （`encode_phc` は構築時に実際に Argon2id を計算してしまい、`verify()` 側の
/// `verify_phc` 呼び出しと合わせて計算コストが二重になるため。review 指摘）。
fn dummy_phc() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        argon2id::synthesize_phc_without_hashing(
            &argon2id::RECOMMENDED_PARAMS,
            b"0000000000000000",
            &[0u8; 32],
        )
    })
}

/// cleartext password 認証を照合し、成功時は `engine::policy::PolicyContext` を返す。
/// 返す `PolicyContext` の可視性は `Public` ＋ 自テナントの `Private`
/// （RLS-11・TASK-195。[`session_policy_context`] 参照）。
/// `handshake.rs` の接続ハンドラから呼ばれる（ポインタ: TASK-67・WIRE-3）。
/// `http::session::issue`（`POST /v1/session`・TASK-174・HTTP-6。Issue #752）
/// からも同じ関数がそのまま呼ばれ、固定遅延・ダミー KDF による対称性・
/// `MAX_CONCURRENT_ARGON2_KDF` のブロッキングセマフォは SQL wire・NoSQL 表層の
/// 両方で共有される（HTTP 側で `sleep` を追加する等の再実装はしない）。
///
/// 失敗時は未知ユーザー・誤パスワードのいずれも [`dummy_phc`] を用いて実ユーザーと
/// 同一コストの Argon2id 計算を必ず実行してから固定遅延 [`AUTH_FAILURE_DELAY`] 込みで
/// `Err` を返し、早期 return によるタイミング差を作らない（列挙攻撃対策。ポインタ:
/// WIRE-2, WIRE-3）。
///
/// `argon2id::MAX_CONCURRENT_ARGON2_KDF` による同時実行数の制限（review 指摘）は
/// `hash_raw` 内部でブロッキング待機するが、既知ユーザー・未知ユーザーいずれも同一の
/// `verify_phc` 経路（＝同一のセマフォ）を通るため、待機時間もタイミング対称性を
/// 崩さない。
///
/// この対称性は、既知ユーザーの実レコードと未知ユーザー用 [`dummy_phc`] とで
/// Argon2id パラメータが常に一致していることに依存する。`UserStore::load_from_file`
/// が `argon2id::RECOMMENDED_PARAMS` への完全一致以外のレコードを起動時に拒否する
/// ことでこれを保証する（P0 review 指摘: レコードごとのパラメータ差がタイミング
/// 側チャネルになり得た）。
pub fn verify(
    store: &UserStore,
    username: &str,
    password: &[u8],
) -> Result<engine::policy::PolicyContext, AuthFailure> {
    let start = Instant::now();

    let record = store.users.get(username);
    let phc_to_check: &str = match record {
        Some(r) => &r.phc,
        None => dummy_phc(),
    };
    // `verify_phc` の失敗（KDF 実行エラー・構文エラー）は起動時検証済みの PHC では
    // 原理上起きないが、万一に備えて fail-closed（不一致扱い）にする。応答・遅延
    // 契約（`AUTH_FAILURE_DELAY` 起点は `start` のまま）は変えず、原因追跡のために
    // stderr へのみ記録する（username・password・PHC は出力しない。Issue #172:
    // 高負荷時に稀発する非再現フレークの原因切り分け用診断）。
    let password_matches = match argon2id::verify_phc(phc_to_check, password) {
        Ok(matches) => matches,
        Err(e) => {
            engine::log_stderr!(
                "wire-server: password verification could not run the KDF ({e:?}); treating as authentication failure"
            );
            false
        }
    };

    let outcome = match (record, password_matches) {
        (Some(r), true) => Ok(r.tenant_id.clone()),
        _ => Err(()),
    };

    let elapsed = start.elapsed();
    if outcome.is_err() && elapsed < AUTH_FAILURE_DELAY {
        std::thread::sleep(AUTH_FAILURE_DELAY - elapsed);
    }

    match outcome {
        Ok(tenant_id) => {
            // tenant_id はロード時に `session_policy_context` で検証済みだが、
            // 契約変更に備えて再検証する（fail-closed。ここでの失敗も認証失敗
            // として扱う）。
            //
            // `Public` ＋ 自テナントの `Private` を許可する（RLS-11・TASK-195。
            // read-your-writes: 書いた本人が同一テナントの別セッションも含めて
            // commit 済みの自分の行を読み戻せる）。他テナントの `Private` 行は
            // `PolicyContext::is_visible` のテナント一致判定により不可視のまま
            // （テナント境界そのものは変更しない）。engine 側
            // `crate::policy::PolicyContext::new`（既定 = `Public` のみ）は
            // 変更せず、可視性の拡張はこの導出点に閉じる。
            session_policy_context(&tenant_id).map_err(|_| AuthFailure)
        }
        Err(()) => Err(AuthFailure),
    }
}

/// salt 生成用の CSPRNG。新規依存を追加せず `/dev/urandom` から読む（Unix 前提。
/// 読めない場合はエラー終了の fail-closed。WIRE-2: salt はユーザーごとにランダム
/// 生成する）。`main.rs` の `hash-password` サブコマンドから呼ばれる。
/// [`crate::http::session::token::SessionToken::generate`]（NoSQL 表層の
/// セッショントークン生成。TASK-174・HTTP-4。Issue #750）からも同じ乱数源
/// として再利用される。
pub fn read_urandom(len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// [`read_urandom`] を既定の salt 長（16 バイト）で呼ぶ補助関数。
pub fn generate_salt() -> std::io::Result<Vec<u8>> {
    read_urandom(16)
}

/// `hash-password` サブコマンドの既定パラメータ（[`argon2id::RECOMMENDED_PARAMS`]
/// の再エクスポート）。
pub const DEFAULT_PARAMS: Params = argon2id::RECOMMENDED_PARAMS;

#[cfg(test)]
mod tests {
    /// テストごとに一意な一時ディレクトリを作り、`Drop`（panic 時の unwinding 含む）で
    /// 削除するガード（Issue #1303）。従来は pid 単位の共有ディレクトリを作りっぱなしに
    /// していた。`Deref<Target = Path>` なので `dir.join(..)` をそのまま使える。
    /// 削除失敗はテストを失敗させず stderr へ出すのみ。
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        /// `label` はパス区切りを含まないテストコード内リテラルを渡すこと。
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            assert!(
                !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "invalid temp dir label"
            );
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "wire-server-auth-test-{label}-{}-{}-{seq}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock must not be before UNIX_EPOCH")
                    .as_nanos()
            ));
            // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる。
            std::fs::create_dir(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl std::ops::Deref for TestDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "warning: failed to remove temp dir {}: {e}",
                        self.0.display()
                    );
                }
            }
        }
    }

    /// `TestDir` 配下のファイルパス。ディレクトリのガードを保持する。
    struct TestFile {
        _dir: TestDir,
        path: std::path::PathBuf,
    }

    impl std::ops::Deref for TestFile {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl AsRef<std::path::Path> for TestFile {
        fn as_ref(&self) -> &std::path::Path {
            &self.path
        }
    }

    use super::*;

    /// テスト実行時間短縮のための軽量パラメータ（`m` は許容最小値 `8*p`）。
    /// 本番既定値は [`argon2id::RECOMMENDED_PARAMS`]（`hash-password` サブコマンド）。
    const TEST_PARAMS: Params = Params {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };

    fn store_with_user(username: &str, tenant_id: &str, password: &[u8]) -> UserStore {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(password, salt, &TEST_PARAMS).expect("valid phc");
        UserStore::from_records(vec![(username, tenant_id, Box::leak(phc.into_boxed_str()))])
    }

    /// `dummy_phc()` が `synthesize_phc_without_hashing`（実ハッシュ計算なし）で
    /// 組み立てられた後も、構文的に有効な PHC として `parse_phc`/`verify_phc` を
    /// 通り、かつ任意のパスワードに対して必ず不一致になること。前者が崩れると
    /// `verify()` は `unwrap_or(false)` で不一致扱いにはなるものの
    /// `verify_phc` 自体を通らず、dummy 経路が担うコスト整合性（review 指摘）が
    /// 失われる。後者が崩れると固定の合成 hash が偶然一致し得る形になっていないかを
    /// 確認する回帰チェックになる。
    #[test]
    fn dummy_phc_parses_and_never_matches_any_password() {
        assert!(
            !argon2id::verify_phc(dummy_phc(), b"anything").expect("dummy phc must parse"),
            "dummy phc must never match a real password"
        );
    }

    /// 正しいユーザー名・パスワードでの照合成功と、tenant_id がユーザーストア由来で
    /// あること（WIRE-2 の中核契約）。
    #[test]
    fn verify_succeeds_and_derives_tenant_from_store() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let ctx = verify(&store, "alice", b"correct-horse").expect("valid credentials");
        assert_eq!(ctx.tenant_id(), "tenant-a");
    }

    /// RLS-11・TASK-195: `verify()` が返す `PolicyContext` は自テナントの
    /// `Private` 行を可視とし（read-your-writes）、他テナントの `Private` 行は
    /// 不可視のまま。`Public` 行は引き続き全テナントから可視
    /// （RLS-6/RLS-7 の既存契約が壊れていないことの直接確認）。
    #[test]
    fn verify_grants_own_tenant_private_visibility_but_not_other_tenant() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let ctx = verify(&store, "alice", b"correct-horse").expect("valid credentials");

        assert!(
            ctx.is_visible("tenant-a", Visibility::Private),
            "own tenant's Private rows must be visible (read-your-writes)"
        );
        assert!(
            !ctx.is_visible("tenant-b", Visibility::Private),
            "other tenant's Private rows must remain invisible"
        );
        assert!(
            ctx.is_visible("tenant-b", Visibility::Public),
            "Public rows must remain visible across tenants"
        );
    }

    /// 誤りパスワードは拒否され、固定遅延が実際に課されること（下限のみ確認し
    /// フレークを避ける）。
    #[test]
    fn verify_rejects_wrong_password_with_min_delay() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let start = Instant::now();
        let result = verify(&store, "alice", b"wrong-password");
        assert!(result.is_err());
        assert!(start.elapsed() >= AUTH_FAILURE_DELAY);
    }

    /// 未知ユーザーも同一の失敗応答・固定遅延であること（列挙対策）。
    #[test]
    fn verify_rejects_unknown_user_with_min_delay() {
        let store = store_with_user("alice", "tenant-a", b"correct-horse");
        let start = Instant::now();
        let result = verify(&store, "bob", b"anything");
        assert!(result.is_err());
        assert!(start.elapsed() >= AUTH_FAILURE_DELAY);
    }

    /// レビュー指摘（Argon2id KDF の同時実行数上限）の再確認: `hash_raw` 内部の
    /// セマフォで多数の `verify` 呼び出しが同時に待機しても、各呼び出しの
    /// `elapsed` が `AUTH_FAILURE_DELAY` を下回ることはないこと（下限保証は
    /// セマフォの待機時間が加算される方向にしか作用しないため、構造的に破れない
    /// はずだが、実際の並行実行下で回帰確認する）。
    #[test]
    fn verify_min_delay_holds_under_concurrent_kdf_contention() {
        use std::sync::Arc;

        let store = Arc::new(store_with_user("alice", "tenant-a", b"correct-horse"));
        // `argon2id::MAX_CONCURRENT_ARGON2_KDF`（8）を上回るスレッド数で
        // セマフォを実際に競合させる。
        let thread_count = 16;

        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let start = Instant::now();
                    let result = verify(&store, "bob", b"anything");
                    assert!(result.is_err());
                    start.elapsed()
                })
            })
            .collect();

        for h in handles {
            let elapsed = h.join().expect("verify thread must not panic");
            assert!(
                elapsed >= AUTH_FAILURE_DELAY,
                "elapsed {elapsed:?} must not fall below the fixed failure delay even under \
                 KDF semaphore contention"
            );
        }
    }

    /// ユーザーストアロード時、重複 username を拒否すること。
    #[test]
    fn load_from_file_rejects_duplicate_username() {
        let dir = TestDir::new("users");
        let path = dir.join("users_dup.txt");
        let phc = dummy_phc();
        std::fs::write(
            &path,
            format!("alice:tenant-a:{phc}\nalice:tenant-b:{phc}\n"),
        )
        .expect("write user store fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(
            result,
            Err(LoadError::DuplicateUsername { line: 2 })
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// `:` を含む username を含む行はフィールド数不一致として拒否されること。
    #[test]
    fn load_from_file_rejects_colon_in_username() {
        let dir = TestDir::new("users");
        let path = dir.join("users_colon.txt");
        let phc = dummy_phc();
        std::fs::write(&path, format!("ali:ce:tenant-a:{phc}\n")).expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::MalformedLine { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 空 tenant_id を含む行を拒否すること。
    #[test]
    fn load_from_file_rejects_empty_tenant_id() {
        let dir = TestDir::new("users");
        let path = dir.join("users_empty_tenant.txt");
        let phc = dummy_phc();
        std::fs::write(&path, format!("alice::{phc}\n")).expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::EmptyTenantId { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 不正な PHC ハッシュを含む行を拒否すること。
    #[test]
    fn load_from_file_rejects_invalid_phc() {
        let dir = TestDir::new("users");
        let path = dir.join("users_bad_phc.txt");
        std::fs::write(&path, "alice:tenant-a:not-a-valid-phc\n").expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 構文的には正しいが `m_cost_kib` が `RECOMMENDED_PARAMS` と一致しない PHC を
    /// 起動時に拒否すること（レビュー指摘: `hash_raw` 呼び出し時まで検出されないと
    /// OOM でプロセス全体が落ちるため、`load_from_file` の時点で fail-closed に
    /// する。現在は完全一致検証がこの範囲外検出も自動的に兼ねる）。
    #[test]
    fn load_from_file_rejects_oversized_m_cost() {
        let dir = TestDir::new("users");
        let path = dir.join("users_oversized_m_cost.txt");
        // salt は下限（8 バイト）以上の有効値にし、m_cost の検証だけを分離する。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=999999999,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘: `MAX_M_COST_KIB` を 2 GiB から 256 MiB へ引き下げた回帰確認。
    /// 新上限をわずかに超える PHC も起動時に拒否されること（`RECOMMENDED_PARAMS`
    /// への完全一致検証により、範囲外はすべて自動的に拒否される）。salt は下限
    /// （8 バイト）以上の有効値にし、m_cost の検証だけを分離する。
    #[test]
    fn load_from_file_rejects_m_cost_above_reduced_ceiling() {
        let dir = TestDir::new("users");
        let path = dir.join("users_reduced_m_cost_ceiling.txt");
        let over_ceiling = argon2id::MAX_M_COST_KIB as u64 + 1;
        std::fs::write(
            &path,
            format!(
                "alice:tenant-a:$argon2id$v=19$m={over_ceiling},t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$aGFzaA\n"
            ),
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 構文的には正しいが decode 後の hash フィールドが 4 バイト未満（`hash_raw` が
    /// 要求する `out_len >= 4` の下限を満たさない）の PHC を起動時に拒否すること
    /// （レビュー指摘: この検証がないと load は通過し、`verify()` が常に
    /// `hash_raw` の `InvalidParam` で不一致扱いになり続け、恒久的なログイン不能に
    /// なるのを起動時に前倒しで検出する）。
    #[test]
    fn load_from_file_rejects_hash_shorter_than_4_bytes() {
        let dir = TestDir::new("users");
        let path = dir.join("users_short_hash.txt");
        // "AAAAAAAAAAAAAAAAAAAAAA" は 16 バイト（下限 8 バイト以上）の有効な salt。
        // "AA" は 1 バイトにしか decode されない hash フィールド
        // （`hash_raw` の下限 4 バイト未満）。salt は有効値にして hash 長の検証だけを
        // 分離する。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$AA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// 空ファイルは「レコード 0 件のストア」として受理される現行挙動を明示する
    /// 回帰テスト（Issue #172）。この挙動自体の是非は本 Issue のスコープ外だが、
    /// 結合テスト側フィクスチャが誤って空ファイルを読んだ場合に「既知ユーザー
    /// なし → 常に `28P01`」という非再現フレークの原因になり得るため、前提を
    /// 固定して診断（`UserStore::is_empty`）の土台にする。
    #[test]
    fn load_from_file_accepts_empty_file_as_empty_store() {
        let dir = TestDir::new("empty-store");
        let path = dir.join("users_empty.txt");
        std::fs::write(&path, "").expect("write empty fixture");
        let store = UserStore::load_from_file(&path).expect("empty file must load as empty store");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: RFC 9106 の salt 下限（8 バイト以上）未満の salt を
    /// 含む PHC を起動時に拒否すること（短 hash と同じ fail-closed の起動時検証）。
    #[test]
    fn load_from_file_rejects_salt_shorter_than_8_bytes() {
        let dir = TestDir::new("users");
        let path = dir.join("users_short_salt.txt");
        // "AAAAAAAAAA" は 7 バイト（下限 8 バイト未満）に decode される salt。
        // "aGFzaA" は "hash"（4 バイト、下限以上）に decode される有効な hash
        // フィールド。hash は有効値にして salt 長の検証だけを分離する。
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: hash フィールドが decode 後
    /// `argon2id::MAX_HASH_LEN`（64 バイト）を超える PHC を起動時に拒否すること
    /// （上限がないと、認証試行のたびに大きな出力バッファを確保し続ける経路になる）。
    #[test]
    fn load_from_file_rejects_hash_longer_than_max_hash_len() {
        let dir = TestDir::new("users");
        let path = dir.join("users_oversized_hash.txt");
        // "AAAAAAAAAAAAAAAAAAAAAA" は 16 バイト（下限 8 バイト以上）の有効な salt。
        // 87 文字の 'A' は 65 バイト（上限 64 バイトを 1 バイト超える）に decode
        // される hash フィールド。salt は有効値にして hash 上限の検証だけを分離する。
        let oversized_hash = "A".repeat(87);
        std::fs::write(
            &path,
            format!(
                "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA${oversized_hash}\n"
            ),
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: salt フィールドが decode 後
    /// `argon2id::MAX_SALT_LEN`（64 バイト）を超える PHC を起動時に拒否すること
    /// （hash 上限と同じ形のリソース消費経路。上限がないと `compute_h0` が認証試行の
    /// たびに大きな `Vec` を確保し続ける）。
    #[test]
    fn load_from_file_rejects_salt_longer_than_max_salt_len() {
        let dir = TestDir::new("users");
        let path = dir.join("users_oversized_salt.txt");
        // 87 文字の 'A' は 65 バイト（上限 64 バイトを 1 バイト超える）に decode
        // される salt フィールド。"aGFzaA" は "hash"（4 バイト、下限以上上限以下）に
        // decode される有効な hash。hash は有効値にして salt 上限の検証だけを
        // 分離する。
        let oversized_salt = "A".repeat(87);
        std::fs::write(
            &path,
            format!("alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1${oversized_salt}$aGFzaA\n"),
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// P0 review 指摘の再現ケース: 旧仕様では「運用上限内」なら受理していた
    /// m/t/p（ここでは m=8,t=1,p=1。`RECOMMENDED_PARAMS` とは異なる値）を、
    /// `RECOMMENDED_PARAMS` への完全一致要求により起動時に拒否すること。個々の
    /// レコードが異なる KDF コストを持てると、未知ユーザー用ダミー PHC との
    /// コスト差からユーザーの存在・レコードのコスト特性が列挙されうるため
    /// （タイミング側チャネル対策）。
    #[test]
    fn load_from_file_rejects_in_range_params_that_are_not_canonical() {
        let dir = TestDir::new("users");
        let path = dir.join("users_noncanonical_params.txt");
        std::fs::write(
            &path,
            "alice:tenant-a:$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$aGFzaA\n",
        )
        .expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(matches!(result, Err(LoadError::InvalidPhc { line: 1 })));
        let _ = std::fs::remove_file(&path);
    }

    /// PHC パラメータが `RECOMMENDED_PARAMS` に完全一致するレコードは受理される
    /// こと（拒否側だけでなく許容側の境界も明示する）。
    #[test]
    fn load_from_file_accepts_params_equal_to_recommended_params() {
        let dir = TestDir::new("users");
        let path = dir.join("users_canonical_accept.txt");
        let phc = dummy_phc();
        std::fs::write(&path, format!("alice:tenant-a:{phc}\n")).expect("write fixture");
        let result = UserStore::load_from_file(&path);
        assert!(
            result.is_ok(),
            "RECOMMENDED_PARAMS-equal PHC must be accepted"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// P0 review 指摘の中核となる回帰確認: `load_from_file` を通過した実ユーザー
    /// レコードの Argon2id パラメータが、未知ユーザー用ダミー PHC（[`dummy_phc`]）の
    /// パラメータと常に一致すること。実時間の統計比較ではなく、両者が経由する
    /// PHC のパラメータそのものが一致することを検証することで、タイミング側
    /// チャネルの原因（レコードごとの KDF コスト差）を構造的に排除したことを
    /// 確認する。
    #[test]
    fn load_from_file_enforces_same_kdf_params_as_dummy_phc() {
        let dir = TestDir::new("users");
        let path = dir.join("users_matches_dummy_params.txt");
        let salt = b"0123456789abcdef";
        let real_phc = argon2id::encode_phc(b"correct-horse", salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid recommended params");
        std::fs::write(&path, format!("alice:tenant-a:{real_phc}\n")).expect("write fixture");

        let store = UserStore::load_from_file(&path).expect("recommended params must be accepted");
        let real_record_phc = &store.users.get("alice").expect("alice loaded").phc;

        let (real_params, _, _) = argon2id::parse_phc(real_record_phc).expect("valid phc");
        let (dummy_params, _, _) = argon2id::parse_phc(dummy_phc()).expect("valid phc");
        assert_eq!(
            real_params, dummy_params,
            "known-user record params must equal the dummy PHC params (P0: no per-record KDF \
             cost variance that could leak user existence via timing)"
        );

        let _ = std::fs::remove_file(&path);
    }

    fn write_users_file_with_content(name_hint: &str, content: &str) -> TestFile {
        let dir = TestDir::new(name_hint);
        let path = dir.join("users.txt");
        std::fs::write(&path, content).expect("write fixture");
        TestFile { _dir: dir, path }
    }

    fn sample_scram_line(username: &str, tenant_id: &str, password: &[u8]) -> String {
        let salt = [3u8; scram::SALT_LEN];
        let verifier =
            scram::generate_verifier(password, &salt, scram::SCRAM_ITERATIONS).expect("verifier");
        format!(
            "{username}:{tenant_id}:{}:{}",
            dummy_phc(),
            verifier.to_verifier_string()
        )
    }

    /// 3 フィールドの既存行と 4 フィールド（SCRAM 検証子付き）の行が同じ
    /// ファイル内で共存でき、それぞれ正しくロードされること。
    #[test]
    fn load_from_file_accepts_mixed_three_and_four_field_lines() {
        let cleartext_line = format!("bob:tenant-b:{}\n", dummy_phc());
        let scram_line = sample_scram_line("alice", "tenant-a", b"correct-horse");
        let path =
            write_users_file_with_content("mixed", &format!("{cleartext_line}{scram_line}\n"));

        let store = UserStore::load_from_file(&path).expect("mixed file must load");
        assert_eq!(store.len(), 2);
        assert!(store.scram_lookup("bob").is_none());
        let (tenant, _verifier) = store.scram_lookup("alice").expect("alice has verifier");
        assert_eq!(tenant, "tenant-a");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_file_rejects_scram_verifier_with_wrong_iterations() {
        let salt = [3u8; scram::SALT_LEN];
        let verifier = scram::generate_verifier(b"pw", &salt, 1).expect("verifier");
        let line = format!(
            "alice:tenant-a:{}:{}\n",
            dummy_phc(),
            verifier.to_verifier_string()
        );
        let path = write_users_file_with_content("bad-iter", &line);
        let result = UserStore::load_from_file(&path);
        assert!(matches!(
            result,
            Err(LoadError::InvalidScramVerifier { line: 1 })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn require_scram_rejects_store_with_missing_verifier() {
        let path = write_users_file_with_content(
            "missing-verifier",
            &format!("alice:tenant-a:{}\n", dummy_phc()),
        );
        let store = UserStore::load_from_file(&path).expect("load");
        let result = store.require_scram(b"test-secret-at-least-32-bytes!!");
        assert!(matches!(
            result,
            Err(LoadError::MissingScramVerifier { username }) if username == "alice"
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn require_scram_accepts_store_where_all_records_have_verifiers() {
        let line = sample_scram_line("alice", "tenant-a", b"correct-horse");
        let path = write_users_file_with_content("all-verified", &format!("{line}\n"));
        let store = UserStore::load_from_file(&path).expect("load");
        let store = store
            .require_scram(b"test-secret-at-least-32-bytes!!")
            .expect("all records have verifiers");
        assert_eq!(store.auth_method(), AuthMethod::ScramSha256);
        let _ = std::fs::remove_file(&path);
    }

    /// 未知ユーザー向けモック鍵から導出する salt は、同一ファイルの
    /// 再ロードをまたいでも安定する（RFC 5802 の salt が再接続で変わらない
    /// 性質と対称）。
    #[test]
    fn scram_mock_key_is_stable_across_reloads_of_same_file() {
        let line = sample_scram_line("alice", "tenant-a", b"correct-horse");
        let path = write_users_file_with_content("mock-stable", &format!("{line}\n"));
        let secret = b"test-secret-at-least-32-bytes!!";
        let store1 = UserStore::load_from_file(&path)
            .expect("load 1")
            .require_scram(secret)
            .expect("require_scram 1");
        let store2 = UserStore::load_from_file(&path)
            .expect("load 2")
            .require_scram(secret)
            .expect("require_scram 2");
        assert_eq!(store1.scram_mock_key(), store2.scram_mock_key());
        let _ = std::fs::remove_file(&path);
    }

    /// P0 review 指摘の再発防止（Issue #940 PR #1006）: モック鍵はユーザー
    /// レコード集合から独立しているため、同じ秘密ファイルを使う限り
    /// ユーザーストアの内容（行の追加・削除・並べ替え・検証子の更新）が
    /// 変わってもモック鍵は不変であること。
    #[test]
    fn scram_mock_key_is_independent_of_user_store_contents() {
        let secret = b"test-secret-at-least-32-bytes!!";

        let line_a = sample_scram_line("alice", "tenant-a", b"correct-horse");
        let path_a = write_users_file_with_content("mock-indep-a", &format!("{line_a}\n"));
        let store_a = UserStore::load_from_file(&path_a)
            .expect("load a")
            .require_scram(secret)
            .expect("require_scram a");

        // 別のユーザー（レコード数・ServerKey・行順いずれも異なる）を持つ
        // 別ファイルでも、同じ `mock_key_secret` を渡せば同じモック鍵になる。
        let line_b1 = sample_scram_line("alice", "tenant-a", b"correct-horse");
        let line_b2 = sample_scram_line("bob", "tenant-b", b"another-password");
        let path_b =
            write_users_file_with_content("mock-indep-b", &format!("{line_b1}\n{line_b2}\n"));
        let store_b = UserStore::load_from_file(&path_b)
            .expect("load b")
            .require_scram(secret)
            .expect("require_scram b");

        assert_eq!(store_a.scram_mock_key(), store_b.scram_mock_key());

        // 秘密が異なれば当然モック鍵も異なる（弁別可能でなければ列挙耐性の
        // 検証自体ができない）。
        let store_a_other_secret = UserStore::load_from_file(&path_a)
            .expect("load a (reload)")
            .require_scram(b"different-secret-at-least-32-b!")
            .expect("require_scram a (other secret)");
        assert_ne!(
            store_a.scram_mock_key(),
            store_a_other_secret.scram_mock_key()
        );

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
