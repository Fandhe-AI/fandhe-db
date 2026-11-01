//! ロール・メンバーシップ・ログイン名対応表のカタログのスキーマと保存形式、および Storage 水準の読み書き
//! （SQL-39 のポインタ、Issue #1961。設計記録: `docs/design/role-privilege-model.md` の D1・D5・D12）。
//!
//! 役割: 後続の `CREATE ROLE`（#1962）・`GRANT`（#1965）・権限検査ゲート（#1966）・認証（#2233・#1971）・
//! ブートストラップ（#2238）が読み書きする、ロール関連の永続化層だけを担う。本ファイルは認証・認可の判定を
//! しない（wire-server は無変更で、`--users` だけで認証が決まる従来の挙動は変わらない）。ロールの依存検査・
//! メンバーシップの循環検出・DROP ROLE の掃除は呼び出し側の責務で、本ファイルの書き込みは生の書き込みである。
//!
//! 表（すべて最初の書き込みまで作らない遅延作成。読み取りの `TableDoesNotExist` は 0 件と扱う。旧 DB は
//! 表が無いことがそのまま「空のカタログ」になる。ADR D12）:
//! - [`ROLES_TABLE`]: `(tenant_id, role_name)` → ロール blob
//! - [`ROLE_ID_COUNTER_TABLE`]: ロール内部 ID の採番カウンタ（1 始まり・再利用しない）
//! - [`ROLE_MEMBERSHIPS_TABLE`]: `(tenant_id, member_id, granted_role_id)` → オプション blob
//! - [`ROLE_LOGIN_NAMES_TABLE`]: 符号化済みログイン識別子 → ログイン対応 blob（認証前にテナントを解決する
//!   ためのサーバー全体の表。ADR D1。ADR の「新しい表 3 つ」の記述とは別に 4 表目として定義する）
//!
//! テナント境界: ログイン名対応表以外の表はキーの先頭が `tenant_id` で、他テナントの行は構造上引けない。
//! 不在と「他テナントにしか無い」は同じ結果（`Ok(None)` または同じ固定文言）にする（ADR D10）。
//!
//! ID 0 の扱い: ロール blob の内部 ID・メンバーシップのキーの ID では 0 を破損として拒否する。0 を PUBLIC の
//! 予約 ID として受理するのは ACL の `grantee_id`（`catalog::privilege`）だけである。
//!
//! fail-closed: 未知の版・タグ、予約ビットが 0 以外、切断、末尾の余り、上限超過、キーと値の不整合は固定文言の
//! `CorruptSchema` とし、検証子・tenant_id・blob の断片をエラーに含めない。一覧・有無の判定でも破損した行を
//! 読み飛ばさず全体を `Err` にする（破損を「空」と取り違えて許可側へ倒さない）。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;
use crate::storage::MAX_TENANT_ID_LEN;

use super::{
    validate_identifier, CatalogError, Result, Storage, MAX_IDENTIFIER_LEN, MAX_LIST_TABLES,
};

/// ロール定義の専用カタログ表。キーは `(tenant_id, role_name)`、値はロール blob。
pub(crate) const ROLES_TABLE: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("roles");

/// ロール内部 ID の採番カウンタ表（1 行だけ。再利用しない）。
pub(crate) const ROLE_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("role_id_counter");

/// [`ROLE_ID_COUNTER_TABLE`] の唯一のキー。
const ROLE_ID_COUNTER_KEY: &str = "next_id";

/// メンバーシップ表。キーは `(tenant_id, member_id, granted_role_id)`、値はオプション blob。
pub(crate) const ROLE_MEMBERSHIPS_TABLE: TableDefinition<(&str, u64, u64), &[u8]> =
    TableDefinition::new("role_memberships");

/// ログイン名対応表。キーは [`encode_login_identifier`] の結果、値はログイン対応 blob。
pub(crate) const ROLE_LOGIN_NAMES_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("role_login_names");

/// blob の版（ロール・メンバーシップ・ログイン対応で共通の初版）。
pub(crate) const ROLE_BLOB_VERSION: u8 = 1;

/// 1 テナントあたりのロール件数の上限（`MAX_LIST_TABLES` と同じ既定値）。
pub(crate) const MAX_ROLES_PER_TENANT: usize = MAX_LIST_TABLES;

/// 検証子の領域 1 つの最大バイト数（確保の前に検査する実装既定値）。
pub(crate) const MAX_ROLE_VERIFIER_BYTES: usize = 512;

/// ログイン識別子の修飾子の最大バイト数。
const MAX_LOGIN_QUALIFIER_BYTES: usize = 64;

/// 同名のロールが既にあるときの `Invalid` 文言。#1962 が `42710` へ写す。
pub(crate) const ROLE_ALREADY_EXISTS_MESSAGE: &str = "role already exists";

/// 同じログイン識別子が既に登録済みのときの `Invalid` 文言。
pub(crate) const LOGIN_ALREADY_EXISTS_MESSAGE: &str = "login name already exists";

/// `ALTER ROLE` の対象ロールが無いときの `Invalid` 文言。`sql::role_ddl` が `42704` へ写す（Issue #1963）。
pub(crate) const ROLE_NOT_FOUND_MESSAGE: &str = "role does not exist";

/// `ALTER ROLE ... RENAME` の対象がセッションの現在・セッションユーザーのときの `Invalid` 文言
/// （PostgreSQL と同じく改名できない。`sql::role_ddl` が `0A000` へ写す。Issue #1963）。
pub(crate) const ROLE_IN_SESSION_USE_MESSAGE: &str = "role is in use by the session";

/// 件数上限超過の `ViewLimitExceeded` 文言（`54000`）。
pub(crate) const ROLE_LIMIT_MESSAGE: &str = "too many roles";

/// `DROP ROLE` の対象に依存オブジェクトがあるときの固定文言（[`DropRolesError::HasDependents`] へ写され、
/// `sql::role_ddl` が `2BP01` にする。依存元は含めない）。
pub(crate) const ROLE_HAS_DEPENDENTS_MESSAGE: &str = "role has dependent objects";

/// メンバーシップ表の 1 回の走査で許容する行数の上限（破損・暴走の検出用。超過は `CorruptSchema`）。
const MAX_MEMBERSHIP_SCAN: usize = 1_000_000;

/// ロール属性のビット（予約ビットは 0 でなければ破損。SUPERUSER 相当はテナント境界を越えるためビットを
/// 作らない。RLS-12 のポインタ）。
const ATTR_LOGIN: u16 = 1 << 0;
const ATTR_INHERIT: u16 = 1 << 1;
const ATTR_CREATEROLE: u16 = 1 << 2;
const ATTR_CREATEDB: u16 = 1 << 3;
const ATTR_ALL: u16 = ATTR_LOGIN | ATTR_INHERIT | ATTR_CREATEROLE | ATTR_CREATEDB;

/// メンバーシップのオプションビット。
const MEMBER_ADMIN: u8 = 1 << 0;
const MEMBER_INHERIT: u8 = 1 << 1;
const MEMBER_SET: u8 = 1 << 2;
const MEMBER_ALL: u8 = MEMBER_ADMIN | MEMBER_INHERIT | MEMBER_SET;

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid role catalog entry".to_string())
}

fn invalid(msg: &str) -> CatalogError {
    CatalogError::Invalid(msg.to_string())
}

/// `tenant_id` の形状検査（`catalog::policy` の `validate_scope` と同じ基準）。
pub(crate) fn validate_tenant(tenant: &str) -> Result<()> {
    if tenant.is_empty() || tenant.len() > usize::from(MAX_TENANT_ID_LEN) {
        return Err(invalid("invalid role scope"));
    }
    Ok(())
}

/// ロール属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RoleAttributes {
    pub(crate) login: bool,
    pub(crate) inherit: bool,
    pub(crate) createrole: bool,
    pub(crate) createdb: bool,
}

impl RoleAttributes {
    fn to_bits(self) -> u16 {
        let mut bits = 0;
        for (flag, bit) in [
            (self.login, ATTR_LOGIN),
            (self.inherit, ATTR_INHERIT),
            (self.createrole, ATTR_CREATEROLE),
            (self.createdb, ATTR_CREATEDB),
        ] {
            if flag {
                bits |= bit;
            }
        }
        bits
    }

    fn from_bits(bits: u16) -> Result<Self> {
        if bits & !ATTR_ALL != 0 {
            return Err(corrupt());
        }
        Ok(Self {
            login: bits & ATTR_LOGIN != 0,
            inherit: bits & ATTR_INHERIT != 0,
            createrole: bits & ATTR_CREATEROLE != 0,
            createdb: bits & ATTR_CREATEDB != 0,
        })
    }
}

/// ロール定義 1 件（テナントは格納キーの成分で、本構造体には持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleDef {
    /// 内部 ID（作成時に払い出す。呼び出し側の値は無視される。0 は破損）。
    pub(crate) id: u64,
    /// ロール名（識別子。テナントごとに一意）。
    pub(crate) name: String,
    pub(crate) attributes: RoleAttributes,
    /// 接続数の上限（-1 は無制限。-1 未満は破損）。
    pub(crate) connection_limit: i32,
    /// 有効期限（UTC マイクロ秒。`TIMESTAMPTZ` の内部表現と同じ単位）。
    pub(crate) valid_until: Option<i64>,
    /// SCRAM 検証子（不透明な ASCII 印字可能バイト列。構文検証は wire 側。空は未設定。ADR D3）。
    pub(crate) scram_verifier: Vec<u8>,
    /// 移行期間用の旧形式ハッシュ（不透明な ASCII 印字可能バイト列。空は未設定。ADR D3）。
    pub(crate) legacy_phc: Vec<u8>,
}

fn validate_verifier(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_ROLE_VERIFIER_BYTES && bytes.iter().all(|b| (0x20..=0x7e).contains(b))
}

/// 形状の検証（encode の前段と decode の後段で共有する。ID 以外のすべて）。
fn validate_role_shape(def: &RoleDef) -> Result<()> {
    validate_identifier(&def.name)?;
    if def.connection_limit < -1 {
        return Err(invalid("invalid role connection limit"));
    }
    if !validate_verifier(&def.scram_verifier) || !validate_verifier(&def.legacy_phc) {
        return Err(invalid("invalid role verifier"));
    }
    Ok(())
}

fn push_bytes16(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let len = u16::try_from(bytes.len()).map_err(|_| invalid("role field is too long"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// ロール blob へ符号化する（版 1。整数はリトルエンディアン）。
/// 並び: `version / id / attrs(u16) / connection_limit(i32) / valid_until(tag[+i64]) /
/// scram(u16 長+bytes) / legacy_phc(u16 長+bytes)`。
pub(crate) fn encode_role_def(def: &RoleDef) -> Result<Vec<u8>> {
    validate_role_shape(def)?;
    if def.id == 0 {
        return Err(invalid("invalid role id"));
    }
    let mut out = vec![ROLE_BLOB_VERSION];
    out.extend_from_slice(&def.id.to_le_bytes());
    out.extend_from_slice(&def.attributes.to_bits().to_le_bytes());
    out.extend_from_slice(&def.connection_limit.to_le_bytes());
    match def.valid_until {
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_le_bytes());
        }
        None => out.push(0),
    }
    push_bytes16(&mut out, &def.scram_verifier)?;
    push_bytes16(&mut out, &def.legacy_phc)?;
    Ok(out)
}

/// blob のカーソル。範囲外・長さ超過はすべて `CorruptSchema`。
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(n).ok_or_else(corrupt)?;
        let slice = self.bytes.get(self.offset..end).ok_or_else(corrupt)?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        self.take(1)?.first().copied().ok_or_else(corrupt)
    }

    fn u16(&mut self) -> Result<u16> {
        let raw: [u8; 2] = self.take(2)?.try_into().map_err(|_| corrupt())?;
        Ok(u16::from_le_bytes(raw))
    }

    fn i32(&mut self) -> Result<i32> {
        let raw: [u8; 4] = self.take(4)?.try_into().map_err(|_| corrupt())?;
        Ok(i32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64> {
        let raw: [u8; 8] = self.take(8)?.try_into().map_err(|_| corrupt())?;
        Ok(u64::from_le_bytes(raw))
    }

    fn i64(&mut self) -> Result<i64> {
        let raw: [u8; 8] = self.take(8)?.try_into().map_err(|_| corrupt())?;
        Ok(i64::from_le_bytes(raw))
    }

    /// `u16` の長さ接頭辞つきバイト列。長さを上限検証してから領域を取る。
    fn bytes16(&mut self, max: usize) -> Result<&'a [u8]> {
        let len = usize::from(self.u16()?);
        if len > max {
            return Err(corrupt());
        }
        self.take(len)
    }

    fn finish(&self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(corrupt())
        }
    }
}

/// ロール blob を復号する。`name` は格納キーの成分で値の検証（識別子形状）に使う。あらゆる不整合は固定文言の
/// `CorruptSchema`。
pub(crate) fn decode_role_def(name: &str, bytes: &[u8]) -> Result<RoleDef> {
    let mut r = Reader::new(bytes);
    if r.u8()? != ROLE_BLOB_VERSION {
        return Err(corrupt());
    }
    let id = r.u64()?;
    if id == 0 {
        return Err(corrupt());
    }
    let attributes = RoleAttributes::from_bits(r.u16()?)?;
    let connection_limit = r.i32()?;
    let valid_until = match r.u8()? {
        0 => None,
        1 => Some(r.i64()?),
        _ => return Err(corrupt()),
    };
    let scram_verifier = r.bytes16(MAX_ROLE_VERIFIER_BYTES)?.to_vec();
    let legacy_phc = r.bytes16(MAX_ROLE_VERIFIER_BYTES)?.to_vec();
    r.finish()?;
    let def = RoleDef {
        id,
        name: name.to_string(),
        attributes,
        connection_limit,
        valid_until,
        scram_verifier,
        legacy_phc,
    };
    validate_role_shape(&def).map_err(|_| corrupt())?;
    Ok(def)
}

/// メンバーシップのオプション（付与者は持たない。追跡が必要になれば #1965 が版 2 で足す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MembershipOptions {
    pub(crate) admin: bool,
    pub(crate) inherit: bool,
    pub(crate) set: bool,
}

/// メンバーシップ blob へ符号化する（版 1 + オプションビット）。
pub(crate) fn encode_membership(opts: MembershipOptions) -> Vec<u8> {
    let mut bits = 0;
    for (flag, bit) in [
        (opts.admin, MEMBER_ADMIN),
        (opts.inherit, MEMBER_INHERIT),
        (opts.set, MEMBER_SET),
    ] {
        if flag {
            bits |= bit;
        }
    }
    vec![ROLE_BLOB_VERSION, bits]
}

/// メンバーシップ blob を復号する（予約ビット・末尾の余りは破損）。
pub(crate) fn decode_membership(bytes: &[u8]) -> Result<MembershipOptions> {
    let mut r = Reader::new(bytes);
    if r.u8()? != ROLE_BLOB_VERSION {
        return Err(corrupt());
    }
    let bits = r.u8()?;
    r.finish()?;
    if bits & !MEMBER_ALL != 0 {
        return Err(corrupt());
    }
    Ok(MembershipOptions {
        admin: bits & MEMBER_ADMIN != 0,
        inherit: bits & MEMBER_INHERIT != 0,
        set: bits & MEMBER_SET != 0,
    })
}

/// ログイン識別子を単射に符号化する（ADR D1）。形は `<修飾子のバイト長（10 進）>:<修飾子><ロール名>`。
/// 修飾子は ASCII 英数字のみ（空は拒否）、ロール名は識別子検証を通す。長さ接頭辞により、区切り文字を含む
/// 組合せでも異なる `(修飾子, ロール名)` が同じ文字列にならない。
pub(crate) fn encode_login_identifier(qualifier: &str, role_name: &str) -> Result<String> {
    if qualifier.is_empty()
        || qualifier.len() > MAX_LOGIN_QUALIFIER_BYTES
        || !qualifier.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(invalid("invalid login qualifier"));
    }
    validate_identifier(role_name)?;
    Ok(format!("{}:{qualifier}{role_name}", qualifier.len()))
}

/// [`encode_login_identifier`] の逆。先頭ゼロ・長さの不一致・`:` の欠落・不正な成分は拒否する。
pub(crate) fn decode_login_identifier(identifier: &str) -> Result<(&str, &str)> {
    let bad = || invalid("invalid login identifier");
    // 全体の長さを先に抑える（受信由来の入力に備える）。
    if identifier.len() > MAX_LOGIN_QUALIFIER_BYTES + MAX_IDENTIFIER_LEN + 4 {
        return Err(bad());
    }
    let (digits, rest) = identifier.split_once(':').ok_or_else(bad)?;
    if digits.is_empty()
        || digits.len() > 3
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return Err(bad());
    }
    let qlen: usize = digits.parse().map_err(|_| bad())?;
    if qlen == 0 || qlen > MAX_LOGIN_QUALIFIER_BYTES {
        return Err(bad());
    }
    let qualifier = rest.get(..qlen).ok_or_else(bad)?;
    let role_name = rest.get(qlen..).ok_or_else(bad)?;
    // 再符号化して一致することで、正規形だけを受理する。
    if encode_login_identifier(qualifier, role_name)? != identifier {
        return Err(bad());
    }
    Ok((qualifier, role_name))
}

fn encode_login_blob(tenant: &str, role_id: u64) -> Result<Vec<u8>> {
    validate_tenant(tenant)?;
    if role_id == 0 {
        return Err(invalid("invalid role id"));
    }
    let mut out = vec![ROLE_BLOB_VERSION];
    push_bytes16(&mut out, tenant.as_bytes())?;
    out.extend_from_slice(&role_id.to_le_bytes());
    Ok(out)
}

fn decode_login_blob(bytes: &[u8]) -> Result<(String, u64)> {
    let mut r = Reader::new(bytes);
    if r.u8()? != ROLE_BLOB_VERSION {
        return Err(corrupt());
    }
    let tenant = std::str::from_utf8(r.bytes16(usize::from(MAX_TENANT_ID_LEN))?)
        .map_err(|_| corrupt())?
        .to_string();
    let role_id = r.u64()?;
    r.finish()?;
    if tenant.is_empty() || role_id == 0 {
        return Err(corrupt());
    }
    Ok((tenant, role_id))
}

/// ロール 1 件を引く（表・ロールが無ければ `None`）。
pub(crate) fn get_role_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    name: &str,
) -> Result<Option<RoleDef>> {
    validate_tenant(tenant)?;
    validate_identifier(name)?;
    let table = match src.open_table(ROLES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((tenant, name))?;
    match found {
        Some(v) => decode_role_def(name, v.value()).map(Some),
        None => Ok(None),
    }
}

/// テナントのロールを名前の昇順で列挙する。破損が 1 件でもあれば全体を `Err` にし、読み飛ばさない。件数が
/// `max` を超えたら `CorruptSchema`（破損時の無制限確保の防止）。
fn list_roles_limited<R: ReadSource>(src: &R, tenant: &str, max: usize) -> Result<Vec<RoleDef>> {
    validate_tenant(tenant)?;
    let table = match src.open_table(ROLES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((tenant, "")..)? {
        let (k, v) = entry?;
        let (t, name) = k.value();
        if t != tenant {
            break;
        }
        if out.len() >= max {
            return Err(corrupt());
        }
        out.push(decode_role_def(name, v.value())?);
    }
    Ok(out)
}

/// テナントのロールを名前の昇順で列挙する。
pub(crate) fn list_roles_in_txn<R: ReadSource>(src: &R, tenant: &str) -> Result<Vec<RoleDef>> {
    list_roles_limited(src, tenant, MAX_ROLES_PER_TENANT)
}

/// 内部 ID でロールを引く（テナントの範囲を上限付きで走査する。同じ ID の重複は破損）。
pub(crate) fn get_role_by_id_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    id: u64,
) -> Result<Option<RoleDef>> {
    let mut found: Option<RoleDef> = None;
    for def in list_roles_in_txn(src, tenant)? {
        if def.id == id {
            if found.is_some() {
                return Err(corrupt());
            }
            found = Some(def);
        }
    }
    Ok(found)
}

/// テナントにロールが 1 件でもあるか。表が無ければ `Ok(false)`、先頭の行が破損していれば `Err`
/// （ブートストラップ判定が「破損」を「空」と取り違えないため。#2238）。
pub(crate) fn tenant_has_roles_in_txn<R: ReadSource>(src: &R, tenant: &str) -> Result<bool> {
    validate_tenant(tenant)?;
    let table = match src.open_table(ROLES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let first = table.range((tenant, "")..)?.next();
    match first {
        Some(entry) => {
            let (k, v) = entry?;
            let (t, name) = k.value();
            if t != tenant {
                return Ok(false);
            }
            decode_role_def(name, v.value())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_role_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(ROLE_ID_COUNTER_TABLE)?;
    let stored = counter.get(ROLE_ID_COUNTER_KEY)?.map(|v| v.value());
    let current = match stored {
        Some(v) => {
            // 保存済みカウンタが 0、または既存ロールの最大 ID 以下なら巻き戻っている。書き込む前に
            // fail-closed で拒否する（設計 D1「ID は再利用しない」）。全行を保持せず順に走査する。
            if v == 0 {
                return Err(corrupt());
            }
            let roles = write_txn.open_table(ROLES_TABLE)?;
            for entry in roles.iter()? {
                let (k, val) = entry?;
                let (_, name) = k.value();
                let existing = decode_role_def(name, val.value())?;
                if existing.id >= v {
                    return Err(corrupt());
                }
            }
            v
        }
        None => {
            // カウンタ欠損は「未使用カタログ（全テナントでロール行 0 件）」のときだけ 1 から始める。
            // 既存ロールがあるのに欠損していれば ID 重複（ACL・メンバーシップの別名ロール化）を
            // 招くため fail-closed で拒否する（設計 D1「ID は再利用しない」）。
            let roles = write_txn.open_table(ROLES_TABLE)?;
            if roles.iter()?.next().is_some() {
                return Err(corrupt());
            }
            1
        }
    };
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(ROLE_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// ロールを作り、内部 ID を返す。同名があれば `Invalid(ROLE_ALREADY_EXISTS_MESSAGE)`、件数上限超過は
/// `ViewLimitExceeded(ROLE_LIMIT_MESSAGE)`。メンバーシップ・ログイン対応は呼び出し側が別途書く。
pub(crate) fn create_role_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    def: &RoleDef,
) -> Result<u64> {
    create_role_limited(write_txn, tenant, def, MAX_ROLES_PER_TENANT)
}

fn create_role_limited(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    def: &RoleDef,
    max_roles: usize,
) -> Result<u64> {
    validate_tenant(tenant)?;
    validate_role_shape(def)?;
    if get_role_in_txn(write_txn, tenant, &def.name)?.is_some() {
        return Err(invalid(ROLE_ALREADY_EXISTS_MESSAGE));
    }
    if list_roles_limited(write_txn, tenant, max_roles)?.len() >= max_roles {
        return Err(CatalogError::ViewLimitExceeded(
            ROLE_LIMIT_MESSAGE.to_string(),
        ));
    }
    let mut stored = def.clone();
    stored.id = allocate_role_id_in_txn(write_txn)?;
    let blob = encode_role_def(&stored)?;
    let mut table = write_txn.open_table(ROLES_TABLE)?;
    table.insert((tenant, stored.name.as_str()), blob.as_slice())?;
    Ok(stored.id)
}

/// [`Storage::drop_roles`] の失敗。`sql::role_ddl` が SQLSTATE（`42704`／`2BP01`／その他）へ写す。
#[derive(Debug)]
pub(crate) enum DropRolesError {
    /// 対象が無い（他テナントにしか無い場合を含む）。値は利用者が書いた名前。
    NotFound(String),
    /// 対象に依存オブジェクトがある。値は利用者が書いた名前（依存元は運ばない）。
    HasDependents(String),
    Catalog(CatalogError),
}

impl From<CatalogError> for DropRolesError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

/// ロールを生の削除で消し、存在して消したら `true`。依存検査・メンバーシップ掃除は [`drop_role_in_txn`] の担当。
pub(crate) fn remove_role_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    name: &str,
) -> Result<bool> {
    validate_tenant(tenant)?;
    validate_identifier(name)?;
    if get_role_in_txn(write_txn, tenant, name)?.is_none() {
        return Ok(false);
    }
    let mut table = write_txn.open_table(ROLES_TABLE)?;
    table.remove((tenant, name))?;
    Ok(true)
}

/// `role_id` が member 側にも granted 側にも現れるメンバーシップ行を同じ txn で削除する（`DROP ROLE` は PostgreSQL と
/// 同じくメンバーシップを自動で取り消し、依存扱いにしない。Issue #1962）。他テナントの行は構造上走査しない。
/// 走査行数が [`MAX_MEMBERSHIP_SCAN`] を超える・行が破損している場合は `Err`（fail-closed）。
pub(crate) fn remove_memberships_of_role_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
) -> Result<usize> {
    validate_tenant(tenant)?;
    if role_id == 0 {
        return Err(invalid("invalid role membership"));
    }
    let mut doomed: Vec<(u64, u64)> = Vec::new();
    match ReadSource::open_table(write_txn, ROLE_MEMBERSHIPS_TABLE) {
        Ok(table) => {
            let mut seen = 0usize;
            for entry in table.range((tenant, 0u64, 0u64)..)? {
                let (k, v) = entry?;
                let (t, member, granted) = k.value();
                if t != tenant {
                    break;
                }
                seen += 1;
                if seen > MAX_MEMBERSHIP_SCAN || member == 0 || granted == 0 || member == granted {
                    return Err(corrupt());
                }
                decode_membership(v.value())?;
                if member == role_id || granted == role_id {
                    doomed.push((member, granted));
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    }
    if doomed.is_empty() {
        return Ok(0);
    }
    let mut table = write_txn.open_table(ROLE_MEMBERSHIPS_TABLE)?;
    for (member, granted) in &doomed {
        table.remove((tenant, *member, *granted))?;
    }
    Ok(doomed.len())
}

/// ロール `role_id` に依存するオブジェクトがあるか（`DROP ROLE` の `2BP01` 判定。Issue #1962）。現在カタログに
/// あるのは ACL（付与先・付与者）だけ。所有者（#2236）・ポリシーの `TO <ロール>`（#2278 段 2）の検査は、この関数へ
/// 足す拡張点とする。
pub(crate) fn role_has_dependents_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
) -> Result<bool> {
    super::privilege::acl_references_role_in_txn(src, tenant, role_id)
}

/// `DROP ROLE` の 1 件分: 解決 → 依存検査 → メンバーシップ掃除 → 削除（呼び出し側が単一 write txn で束ねる）。
/// 対象が無ければ `Ok(false)`（`IF EXISTS` の判断は呼び出し側）。依存があれば
/// `Invalid(ROLE_HAS_DEPENDENTS_MESSAGE)`。
pub(crate) fn drop_role_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    name: &str,
) -> Result<bool> {
    validate_tenant(tenant)?;
    validate_identifier(name)?;
    let Some(role) = get_role_in_txn(write_txn, tenant, name)? else {
        return Ok(false);
    };
    if role_has_dependents_in_txn(write_txn, tenant, role.id)? {
        return Err(invalid(ROLE_HAS_DEPENDENTS_MESSAGE));
    }
    remove_memberships_of_role_in_txn(write_txn, tenant, role.id)?;
    remove_role_in_txn(write_txn, tenant, name)
}

fn check_membership_ids(member_id: u64, granted_role_id: u64) -> Result<()> {
    if member_id == 0 || granted_role_id == 0 || member_id == granted_role_id {
        return Err(invalid("invalid role membership"));
    }
    Ok(())
}

/// メンバーシップを書く（生の書き込み。ロールの存在・循環は検査しない。#1963・#2232 の担当）。自分自身への
/// 付与と ID 0 は拒否する。
pub(crate) fn put_membership_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    member_id: u64,
    granted_role_id: u64,
    opts: MembershipOptions,
) -> Result<()> {
    validate_tenant(tenant)?;
    check_membership_ids(member_id, granted_role_id)?;
    let is_new = get_membership(write_txn, tenant, member_id, granted_role_id)?.is_none();
    if is_new
        && list_memberships_limited(write_txn, tenant, member_id, MAX_ROLES_PER_TENANT)?.len()
            >= MAX_ROLES_PER_TENANT
    {
        return Err(CatalogError::ViewLimitExceeded(
            ROLE_LIMIT_MESSAGE.to_string(),
        ));
    }
    let mut table = write_txn.open_table(ROLE_MEMBERSHIPS_TABLE)?;
    table.insert(
        (tenant, member_id, granted_role_id),
        encode_membership(opts).as_slice(),
    )?;
    Ok(())
}

/// メンバーシップを削除し、存在して消したら `true`。
pub(crate) fn remove_membership_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    member_id: u64,
    granted_role_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    check_membership_ids(member_id, granted_role_id)?;
    if get_membership(write_txn, tenant, member_id, granted_role_id)?.is_none() {
        return Ok(false);
    }
    let mut table = write_txn.open_table(ROLE_MEMBERSHIPS_TABLE)?;
    table.remove((tenant, member_id, granted_role_id))?;
    Ok(true)
}

fn get_membership<R: ReadSource>(
    src: &R,
    tenant: &str,
    member_id: u64,
    granted_role_id: u64,
) -> Result<Option<MembershipOptions>> {
    let table = match src.open_table(ROLE_MEMBERSHIPS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((tenant, member_id, granted_role_id))?;
    match found {
        Some(v) => decode_membership(v.value()).map(Some),
        None => Ok(None),
    }
}

fn list_memberships_limited<R: ReadSource>(
    src: &R,
    tenant: &str,
    member_id: u64,
    max: usize,
) -> Result<Vec<(u64, MembershipOptions)>> {
    let table = match src.open_table(ROLE_MEMBERSHIPS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((tenant, member_id, 0u64)..)? {
        let (k, v) = entry?;
        let (t, m, granted) = k.value();
        if t != tenant || m != member_id {
            break;
        }
        if out.len() >= max || m == 0 || granted == 0 || m == granted {
            return Err(corrupt());
        }
        out.push((granted, decode_membership(v.value())?));
    }
    Ok(out)
}

/// `member_id` が直接属するロール（付与先 ID の昇順）とそのオプションを返す。破損は `Err`。
pub(crate) fn list_memberships_of_member_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    member_id: u64,
) -> Result<Vec<(u64, MembershipOptions)>> {
    validate_tenant(tenant)?;
    list_memberships_limited(src, tenant, member_id, MAX_ROLES_PER_TENANT)
}

/// ロールの属性・検証子を `mutate` で書き換えて同じキーへ保存する（`ALTER ROLE ... 属性`。Issue #1963）。
/// 内部 ID・名前は変えない（`mutate` が変えても ID と名前は元の値へ戻す）。対象が無ければ
/// `Invalid(ROLE_NOT_FOUND_MESSAGE)`（他テナントにしか無い場合も同じ）。形状が不正なら保存せず `Invalid`。
pub(crate) fn alter_role_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    name: &str,
    mutate: impl FnOnce(&mut RoleDef),
) -> Result<()> {
    validate_tenant(tenant)?;
    let Some(current) = get_role_in_txn(write_txn, tenant, name)? else {
        return Err(invalid(ROLE_NOT_FOUND_MESSAGE));
    };
    let mut next = current.clone();
    mutate(&mut next);
    next.id = current.id;
    next.name = current.name;
    let blob = encode_role_def(&next)?;
    let mut table = write_txn.open_table(ROLES_TABLE)?;
    table.insert((tenant, name), blob.as_slice())?;
    Ok(())
}

/// ロールの名前を変える（`ALTER ROLE ... RENAME TO`。Issue #1963）。blob に名前は無く ACL・メンバーシップは
/// ID キーのため、格納キー `(tenant, name)` を移すだけで ID・属性・検証子はそのまま保つ。旧名が無ければ
/// `Invalid(ROLE_NOT_FOUND_MESSAGE)`、新名が既にあれば `Invalid(ROLE_ALREADY_EXISTS_MESSAGE)`、対象が
/// `protected_ids`（セッションの現在・セッションユーザー）に含まれれば `Invalid(ROLE_IN_SESSION_USE_MESSAGE)`。
pub(crate) fn rename_role_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    old_name: &str,
    new_name: &str,
    protected_ids: &[u64],
) -> Result<()> {
    validate_tenant(tenant)?;
    validate_identifier(new_name)?;
    let Some(def) = get_role_in_txn(write_txn, tenant, old_name)? else {
        return Err(invalid(ROLE_NOT_FOUND_MESSAGE));
    };
    if protected_ids.contains(&def.id) {
        return Err(invalid(ROLE_IN_SESSION_USE_MESSAGE));
    }
    if get_role_in_txn(write_txn, tenant, new_name)?.is_some() {
        return Err(invalid(ROLE_ALREADY_EXISTS_MESSAGE));
    }
    let mut renamed = def;
    renamed.name = new_name.to_string();
    let blob = encode_role_def(&renamed)?;
    let mut table = write_txn.open_table(ROLES_TABLE)?;
    table.remove((tenant, old_name))?;
    table.insert((tenant, new_name), blob.as_slice())?;
    Ok(())
}

/// `SET ROLE` の名前解決・認可の結果（[`resolve_settable_role_in_txn`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetRoleResolution {
    /// 自テナントに該当するロールが無い（他テナントにしか無い場合を含む）。
    NotFound,
    /// ロールはあるが、セッションのロールから `SET` 真のメンバーシップの経路で到達できない。
    Denied,
    /// 到達できる。値は対象ロールの内部 ID。
    Allowed(u64),
}

/// `from_id` から `SET` 真のメンバーシップだけを辿って `target_id` に到達できるか（PostgreSQL の
/// `member_can_set_role` 相当。ADR D4。Issue #1963）。`from_id == target_id` は常に真。
/// 循環は訪問済み集合で除く。訪問ロール数が [`MAX_ROLES_PER_TENANT`]、走査する辺の総数が
/// [`MAX_MEMBERSHIP_SCAN`] を超えたら到達不能（`false`）として拒否側へ倒す（fail-closed）。
/// 行の破損は `Err`。他テナントのメンバーシップはキーの先頭が `tenant_id` のため辿れない。
pub(crate) fn role_can_set_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    from_id: u64,
    target_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    if from_id == target_id {
        return Ok(true);
    }
    let mut visited = std::collections::HashSet::new();
    visited.insert(from_id);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(from_id);
    let mut scanned = 0usize;
    while let Some(member) = queue.pop_front() {
        for (granted, opts) in list_memberships_of_member_in_txn(src, tenant, member)? {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_MEMBERSHIP_SCAN {
                return Ok(false);
            }
            if !opts.set {
                continue;
            }
            if granted == target_id {
                return Ok(true);
            }
            if visited.insert(granted) {
                if visited.len() > MAX_ROLES_PER_TENANT {
                    return Ok(false);
                }
                queue.push_back(granted);
            }
        }
    }
    Ok(false)
}

/// `SET ROLE <name>` の名前解決と認可を 1 つの読み取りで行う（Issue #1963）。名前は自テナントのカタログだけで
/// 引き、不在と他テナントにしか無い場合は同じ [`SetRoleResolution::NotFound`]（ADR D10）。
pub(crate) fn resolve_settable_role_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    session_role_id: u64,
    name: &str,
) -> Result<SetRoleResolution> {
    let Some(target) = get_role_in_txn(src, tenant, name)? else {
        return Ok(SetRoleResolution::NotFound);
    };
    if role_can_set_in_txn(src, tenant, session_role_id, target.id)? {
        Ok(SetRoleResolution::Allowed(target.id))
    } else {
        Ok(SetRoleResolution::Denied)
    }
}

/// ログイン識別子を登録する。重複は `Invalid(LOGIN_ALREADY_EXISTS_MESSAGE)`。識別子は正規形のみ受理する。
pub(crate) fn register_login_in_txn(
    write_txn: &redb::WriteTransaction,
    identifier: &str,
    tenant: &str,
    role_id: u64,
) -> Result<()> {
    decode_login_identifier(identifier)?;
    let blob = encode_login_blob(tenant, role_id)?;
    let exists = match ReadSource::open_table(write_txn, ROLE_LOGIN_NAMES_TABLE) {
        Ok(t) => t.get(identifier)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(e.into()),
    };
    if exists {
        return Err(invalid(LOGIN_ALREADY_EXISTS_MESSAGE));
    }
    let mut table = write_txn.open_table(ROLE_LOGIN_NAMES_TABLE)?;
    table.insert(identifier, blob.as_slice())?;
    Ok(())
}

/// ログイン識別子の登録を解除し、存在して消したら `true`。
pub(crate) fn remove_login_in_txn(
    write_txn: &redb::WriteTransaction,
    identifier: &str,
) -> Result<bool> {
    decode_login_identifier(identifier)?;
    let exists = match ReadSource::open_table(write_txn, ROLE_LOGIN_NAMES_TABLE) {
        Ok(t) => t.get(identifier)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(e.into()),
    };
    if !exists {
        return Ok(false);
    }
    let mut table = write_txn.open_table(ROLE_LOGIN_NAMES_TABLE)?;
    table.remove(identifier)?;
    Ok(true)
}

/// ログイン識別子から `(tenant_id, role_id)` を解決する。未登録は `Ok(None)`。参照先のロールが存在し、LOGIN
/// 属性を持ち、ID と名前が識別子に一致することを確かめ、一致しなければ `CorruptSchema`（fail-closed。
/// 認証前にテナントを解決する入口のため、不整合を許可側へ倒さない）。
pub(crate) fn resolve_login_in_txn<R: ReadSource>(
    src: &R,
    identifier: &str,
) -> Result<Option<(String, u64)>> {
    let (_, role_name) = decode_login_identifier(identifier)?;
    let blob = {
        let table = match src.open_table(ROLE_LOGIN_NAMES_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let found = table.get(identifier)?;
        match found {
            Some(v) => v.value().to_vec(),
            None => return Ok(None),
        }
    };
    let (tenant, role_id) = decode_login_blob(&blob)?;
    let role = get_role_by_id_in_txn(src, &tenant, role_id)?.ok_or_else(corrupt)?;
    if role.name != role_name || !role.attributes.login {
        return Err(corrupt());
    }
    Ok(Some((tenant, role_id)))
}

impl Storage {
    /// `tenant` にロールを作り、内部 ID を返す（単一 write txn・1 回の commit。表の世代は進めない）。
    pub(crate) fn create_role(&self, tenant: &str, def: &RoleDef) -> Result<u64> {
        validate_tenant(tenant)?;
        validate_role_shape(def)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let id = create_role_in_txn(&write_txn, tenant, def)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(id)
    }

    /// `tenant` のロール `names` を単一 write txn・1 回の commit で順に削除し、実際に消した件数を返す
    /// （`DROP ROLE a, b`。Issue #1962）。対象が無い名前は `if_exists` なら skip、そうでなければ
    /// [`DropRolesError::NotFound`]。依存があれば [`DropRolesError::HasDependents`]。途中で失敗したら
    /// commit せず全件取り消す（原子的）。エラーは利用者が書いた名前だけを運ぶ。
    pub(crate) fn drop_roles(
        &self,
        tenant: &str,
        names: &[String],
        if_exists: bool,
    ) -> std::result::Result<usize, DropRolesError> {
        validate_tenant(tenant)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let mut removed = 0usize;
        for name in names {
            match drop_role_in_txn(&write_txn, tenant, name) {
                Ok(true) => removed += 1,
                Ok(false) if if_exists => {}
                Ok(false) => return Err(DropRolesError::NotFound(name.clone())),
                Err(CatalogError::Invalid(m)) if m == ROLE_HAS_DEPENDENTS_MESSAGE => {
                    return Err(DropRolesError::HasDependents(name.clone()));
                }
                Err(e) => return Err(e.into()),
            }
        }
        if removed > 0 {
            crate::recovery::commit_boundary::commit(write_txn)
                .map_err(super::convert_storage_error)?;
        }
        Ok(removed)
    }

    /// `tenant` のロール `name` の属性・検証子を単一 write txn・1 回の commit で書き換える
    /// （`ALTER ROLE`。Issue #1963。表の世代は進めない）。対象が無ければ `Invalid(ROLE_NOT_FOUND_MESSAGE)`。
    pub(crate) fn alter_role(
        &self,
        tenant: &str,
        name: &str,
        mutate: impl FnOnce(&mut RoleDef),
    ) -> Result<()> {
        validate_tenant(tenant)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        alter_role_in_txn(&write_txn, tenant, name, mutate)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// `tenant` のロールを単一 write txn・1 回の commit で改名する（`ALTER ROLE ... RENAME TO`。
    /// Issue #1963）。エラーは [`rename_role_in_txn`] のとおり。
    pub(crate) fn rename_role(
        &self,
        tenant: &str,
        old_name: &str,
        new_name: &str,
        protected_ids: &[u64],
    ) -> Result<()> {
        validate_tenant(tenant)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        rename_role_in_txn(&write_txn, tenant, old_name, new_name, protected_ids)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// `SET ROLE` の名前解決・認可（読み取り専用。[`resolve_settable_role_in_txn`]）。
    pub(crate) fn resolve_settable_role(
        &self,
        tenant: &str,
        session_role_id: u64,
        name: &str,
    ) -> Result<SetRoleResolution> {
        let rt = self.db().begin_read()?;
        resolve_settable_role_in_txn(&rt, tenant, session_role_id, name)
    }

    /// `tenant` のロール `name` を引く（読み取り専用。他テナントのロールは構造上引けず、不在と同じ `None`）。
    pub(crate) fn get_role(&self, tenant: &str, name: &str) -> Result<Option<RoleDef>> {
        let rt = self.db().begin_read()?;
        get_role_in_txn(&rt, tenant, name)
    }

    /// `tenant` のロールを名前の昇順で返す（破損があれば `Err`）。
    pub(crate) fn list_roles(&self, tenant: &str) -> Result<Vec<RoleDef>> {
        let rt = self.db().begin_read()?;
        list_roles_in_txn(&rt, tenant)
    }

    /// `tenant` にロールが 1 件でもあるか（表が無ければ `false`。破損は `Err`）。
    pub(crate) fn tenant_has_roles(&self, tenant: &str) -> Result<bool> {
        let rt = self.db().begin_read()?;
        tenant_has_roles_in_txn(&rt, tenant)
    }

    /// ログイン識別子を解決する（[`resolve_login_in_txn`]）。
    pub(crate) fn resolve_login(&self, identifier: &str) -> Result<Option<(String, u64)>> {
        let rt = self.db().begin_read()?;
        resolve_login_in_txn(&rt, identifier)
    }
}

#[cfg(test)]
mod tests;
