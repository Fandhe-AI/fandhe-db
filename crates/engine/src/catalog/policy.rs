//! 行セキュリティポリシーのカタログのスキーマと保存形式、および Storage 水準の読み書き（RLS-13・
//! Issue #2274。設計記録: `docs/design/row-security-policy-composition.md`）。
//!
//! 役割: `CREATE POLICY`／`ALTER POLICY`／`DROP POLICY`（`sql::policy_ddl`）が読み書きする定義の永続化層を
//! 担う。ポリシーを読み書きの経路へ適用する側（USING・WITH CHECK の評価。後続 Issue）は本ファイルの
//! `list_policies` で対象表の定義を引く想定で、本ファイル自体は行の可視性に影響しない。表のライフサイクル
//! 操作（`catalog.rs`）は同じ write txn の中で本ファイルの `*_in_txn` を呼び、孤児定義を作らない。
//!
//! キー設計: `(table_key, scope, policy_name)`。`scope` は検証済みの `tenant_id`。表キーを先頭に置くのは、
//! 表の DROP・RENAME が全テナント分の定義を前方一致の範囲走査だけで処理するため（共有 `public` 表には複数
//! テナントのポリシーがありうる）。テナント境界は全 API が `scope` を必須引数に取り、範囲走査も `scope` を
//! 必ず突き合わせることで守る（`catalog::trigger` と同じ型）。
//!
//! 所有者: ロール・所有者の機構が未実装の間、DDL を実行できる主体の判定は `sql::ddl::require_ddl_permission`
//! に一本化する。blob には所有者の予約バイト（常に 0）だけを置き、判定は持たない。
//!
//! 既存データの不変: [`POLICIES_TABLE`]／[`POLICY_ID_COUNTER_TABLE`] は最初の書き込みまで作らない（遅延作成。
//! 読み取りは `TableDoesNotExist` を 0 件と扱う）。
//!
//! fail-closed: 未知の版・タグ、予約バイトが 0 以外、切断、末尾の余り、上限超過、キーと値の不整合は固定文言の
//! `CorruptSchema` とし、blob の断片・tenant_id・式本文をエラーに含めない。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;
use crate::storage::MAX_TENANT_ID_LEN;

use super::{
    bump_table_generation_in_txn, index_name_exists_in_txn, materialized_view_exists_in_txn,
    require_table_schema_write_any_identity, validate_identifier, validate_relation_key,
    CatalogError, Result, Storage, TableSchema, CATALOG_TABLE, MAX_COLUMN_COUNT, VIEWS_TABLE,
};

/// ポリシー定義の専用カタログ表。キーは `(table_key, scope, policy_name)`、値は定義 blob。
pub(crate) const POLICIES_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("policies");

/// 内部 ID の採番カウンタ表（1 行だけ。再利用しない）。
pub(crate) const POLICY_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("policy_id_counter");

/// [`POLICY_ID_COUNTER_TABLE`] の唯一のキー。
const POLICY_ID_COUNTER_KEY: &str = "next_id";

/// 定義 blob の版。
pub(crate) const POLICY_DEF_VERSION: u8 = 1;

/// 1 表・1 テナントあたりのポリシー件数の上限（実装既定値）。
pub(crate) const MAX_POLICIES_PER_TABLE: usize = 64;

/// USING／WITH CHECK の正規化テキストの最大バイト数。
pub(crate) const MAX_POLICY_EXPR_BYTES: usize = 4096;

/// `TO` に書けるロールの最大件数（現状は `PUBLIC` のみ受理。保存形式の上限）。
const MAX_POLICY_ROLES: usize = 16;

/// 識別子 1 つの最大バイト数（列名）。
const MAX_NAME_FIELD_BYTES: usize = 128;

/// 同名のポリシーが既にある（作成・改名先）ときの `Invalid` 文言。`sql::policy_ddl` が `42710` へ写す。
pub(crate) const POLICY_ALREADY_EXISTS_MESSAGE: &str = "policy already exists";

/// 対象のポリシーが無い（他テナントにしか無い場合を含む）ときの `Invalid` 文言（`42704`）。
pub(crate) const POLICY_NOT_FOUND_MESSAGE: &str = "policy does not exist";

/// 件数上限超過の `ViewLimitExceeded` 文言（`54000`）。
pub(crate) const POLICY_LIMIT_MESSAGE: &str = "too many policies";

/// 所有者の予約値（ロール機構が入るまで常に 0）。
const OWNER_TAG_NONE: u8 = 0;

/// ポリシーが適用されるコマンド（`FOR` 句）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyCommand {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCommand {
    fn tag(self) -> u8 {
        match self {
            Self::All => 0,
            Self::Select => 1,
            Self::Insert => 2,
            Self::Update => 3,
            Self::Delete => 4,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::All),
            1 => Ok(Self::Select),
            2 => Ok(Self::Insert),
            3 => Ok(Self::Update),
            4 => Ok(Self::Delete),
            _ => Err(corrupt()),
        }
    }

    /// このコマンドが `USING` を持てるか（`INSERT` は `WITH CHECK` のみ）。
    pub(crate) fn allows_using(self) -> bool {
        !matches!(self, Self::Insert)
    }

    /// このコマンドが `WITH CHECK` を持てるか（`SELECT`・`DELETE` は `USING` のみ）。
    pub(crate) fn allows_check(self) -> bool {
        !matches!(self, Self::Select | Self::Delete)
    }
}

/// `TO` 句の対象。名前付きロールはロール機構（後続 Issue）が入るまで受理しない（存在を確かめられない名前を
/// 保存すると fail-open になるため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyRole {
    Public,
}

impl PolicyRole {
    fn tag(self) -> u8 {
        match self {
            Self::Public => 0,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Public),
            _ => Err(corrupt()),
        }
    }
}

/// ポリシー定義 1 件（対象表は格納キーの成分で、本構造体には持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicyDef {
    /// 内部 ID（`create_policy` が払い出す。呼び出し側の値は無視される）。
    pub(crate) id: u64,
    /// ポリシー名（識別子。SCHEMA 修飾なし。表・テナントごとに一意）。
    pub(crate) name: String,
    /// `AS PERMISSIVE`（既定）なら真、`AS RESTRICTIVE` なら偽。
    pub(crate) permissive: bool,
    pub(crate) command: PolicyCommand,
    pub(crate) roles: Vec<PolicyRole>,
    /// `USING` 式の正規化テキスト（束縛・往復一致を確認済みの値）。
    pub(crate) using_sql: Option<String>,
    /// `WITH CHECK` 式の正規化テキスト。
    pub(crate) check_sql: Option<String>,
    /// 両式が参照するスキーマ列（重複なし）。`DROP COLUMN` 等の依存検査に使う。
    pub(crate) referenced_columns: Vec<String>,
}

/// 束縛段（`sql::policy_expr`）が確定した式の保存値。`create_policy` の束縛クロージャが返す。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PolicyExprs {
    pub(crate) using_sql: Option<String>,
    pub(crate) check_sql: Option<String>,
    pub(crate) referenced_columns: Vec<String>,
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid policy catalog entry".to_string())
}

fn invalid(msg: &str) -> CatalogError {
    CatalogError::Invalid(msg.to_string())
}

/// `scope`（`tenant_id`）の形状検査。
fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > usize::from(MAX_TENANT_ID_LEN) {
        return Err(invalid("invalid policy scope"));
    }
    Ok(())
}

/// 形状の検証（encode の前段と decode の後段で共有する。ID 以外のすべて）。
fn validate_shape(def: &PolicyDef) -> Result<()> {
    validate_identifier(&def.name)?;
    if def.roles.len() > MAX_POLICY_ROLES {
        return Err(invalid("too many policy roles"));
    }
    for (expr, allowed) in [
        (&def.using_sql, def.command.allows_using()),
        (&def.check_sql, def.command.allows_check()),
    ] {
        if let Some(sql) = expr {
            if !allowed {
                return Err(invalid("expression is not allowed for this policy command"));
            }
            if sql.is_empty() || sql.len() > MAX_POLICY_EXPR_BYTES {
                return Err(invalid("invalid policy expression"));
            }
        }
    }
    if def.referenced_columns.len() > MAX_COLUMN_COUNT {
        return Err(invalid("too many policy columns"));
    }
    for (i, col) in def.referenced_columns.iter().enumerate() {
        validate_identifier(col)?;
        if def.referenced_columns.iter().take(i).any(|c| c == col) {
            return Err(invalid("duplicate policy column"));
        }
    }
    Ok(())
}

fn push_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u32::try_from(s.len()).map_err(|_| invalid("policy field is too long"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn push_opt(out: &mut Vec<u8>, s: &Option<String>) -> Result<()> {
    match s {
        Some(v) => {
            out.push(1);
            push_str(out, v)
        }
        None => {
            out.push(0);
            Ok(())
        }
    }
}

/// 定義を blob へ符号化する（版 1。整数はリトルエンディアン、文字列は `u32` 長 + UTF-8）。
/// 並び: `version / id / owner(予約) / permissive / command / roles(件数+タグ) / using / check / columns`。
pub(crate) fn encode_policy_def(def: &PolicyDef) -> Result<Vec<u8>> {
    validate_shape(def)?;
    let mut out = vec![POLICY_DEF_VERSION];
    out.extend_from_slice(&def.id.to_le_bytes());
    out.push(OWNER_TAG_NONE);
    out.push(u8::from(def.permissive));
    out.push(def.command.tag());
    let nroles = u32::try_from(def.roles.len()).map_err(|_| invalid("too many policy roles"))?;
    out.extend_from_slice(&nroles.to_le_bytes());
    for r in &def.roles {
        out.push(r.tag());
    }
    push_opt(&mut out, &def.using_sql)?;
    push_opt(&mut out, &def.check_sql)?;
    let ncols = u32::try_from(def.referenced_columns.len())
        .map_err(|_| invalid("too many policy columns"))?;
    out.extend_from_slice(&ncols.to_le_bytes());
    for c in &def.referenced_columns {
        push_str(&mut out, c)?;
    }
    Ok(out)
}

/// 定義 blob のカーソル。範囲外・長さ超過・不正 UTF-8 はすべて `CorruptSchema`。
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(n).ok_or_else(corrupt)?;
        let slice = self.bytes.get(self.offset..end).ok_or_else(corrupt)?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        self.take(1)?.first().copied().ok_or_else(corrupt)
    }

    fn u32(&mut self) -> Result<u32> {
        let raw: [u8; 4] = self.take(4)?.try_into().map_err(|_| corrupt())?;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64> {
        let raw: [u8; 8] = self.take(8)?.try_into().map_err(|_| corrupt())?;
        Ok(u64::from_le_bytes(raw))
    }

    fn string(&mut self, max: usize) -> Result<String> {
        let len = usize::try_from(self.u32()?).map_err(|_| corrupt())?;
        if len > max {
            return Err(corrupt());
        }
        std::str::from_utf8(self.take(len)?)
            .map(str::to_string)
            .map_err(|_| corrupt())
    }

    fn opt_string(&mut self, max: usize) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.string(max).map(Some),
            _ => Err(corrupt()),
        }
    }

    /// 件数を上限検証してから読む（確保の前に検査する）。
    fn count(&mut self, max_items: usize) -> Result<usize> {
        let n = usize::try_from(self.u32()?).map_err(|_| corrupt())?;
        if n > max_items {
            return Err(corrupt());
        }
        Ok(n)
    }
}

/// 定義 blob を復号する。`name` は格納キーの成分で、値の検証（識別子形状）に使う。あらゆる不整合は固定
/// 文言の `CorruptSchema`（格納バイト列の断片を含めない）。
pub(crate) fn decode_policy_def(name: &str, bytes: &[u8]) -> Result<PolicyDef> {
    let mut r = Reader { bytes, offset: 0 };
    if r.u8()? != POLICY_DEF_VERSION {
        return Err(corrupt());
    }
    let id = r.u64()?;
    if r.u8()? != OWNER_TAG_NONE {
        return Err(corrupt());
    }
    let permissive = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(corrupt()),
    };
    let command = PolicyCommand::from_tag(r.u8()?)?;
    let nroles = r.count(MAX_POLICY_ROLES)?;
    let mut roles = Vec::with_capacity(nroles);
    for _ in 0..nroles {
        roles.push(PolicyRole::from_tag(r.u8()?)?);
    }
    let using_sql = r.opt_string(MAX_POLICY_EXPR_BYTES)?;
    let check_sql = r.opt_string(MAX_POLICY_EXPR_BYTES)?;
    let ncols = r.count(MAX_COLUMN_COUNT)?;
    let mut referenced_columns = Vec::with_capacity(ncols.min(16));
    for _ in 0..ncols {
        referenced_columns.push(r.string(MAX_NAME_FIELD_BYTES)?);
    }
    if r.offset != bytes.len() {
        return Err(corrupt());
    }
    let def = PolicyDef {
        id,
        name: name.to_string(),
        permissive,
        command,
        roles,
        using_sql,
        check_sql,
        referenced_columns,
    };
    validate_shape(&def).map_err(|_| corrupt())?;
    Ok(def)
}

/// 1 件を引く（表が無ければ `None`）。
fn get_in_txn<R: ReadSource>(
    src: &R,
    table_key: &str,
    scope: &str,
    name: &str,
) -> Result<Option<PolicyDef>> {
    let table = match src.open_table(POLICIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((table_key, scope, name))?;
    match found {
        Some(v) => decode_policy_def(name, v.value()).map(Some),
        None => Ok(None),
    }
}

/// `(table_key, scope)` のポリシーを名前の昇順で列挙する。件数が [`MAX_POLICIES_PER_TABLE`] を超えたら
/// `CorruptSchema`（破損時の無制限確保の防止）。
fn list_in_txn<R: ReadSource>(src: &R, table_key: &str, scope: &str) -> Result<Vec<PolicyDef>> {
    let table = match src.open_table(POLICIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((table_key, scope, "")..)? {
        let (k, v) = entry?;
        let (t, s, name) = k.value();
        if t != table_key || s != scope {
            break;
        }
        if out.len() >= MAX_POLICIES_PER_TABLE {
            return Err(corrupt());
        }
        out.push(decode_policy_def(name, v.value())?);
    }
    Ok(out)
}

/// 書き込み txn の中で `scope` の表 `table_key` のポリシーを名前の昇順で引く（適用側が文の開始時に同じ
/// txn から読む入口。存在しないテーブルは作成せず空を返す。他テナントの定義は構造上引けない）。
pub(crate) fn list_policies_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    table_key: &str,
) -> Result<Vec<PolicyDef>> {
    list_in_txn(write_txn, table_key, scope)
}

/// `ReadSource`（読み取り txn・書き込み txn・明示トランザクションの読み取り窓口）から `scope` の表 `table_key`
/// のポリシーを名前の昇順で引く（USING の適用側 `sql::row_policy` が、行を読むのと同じスナップショットから
/// 解決する入口。存在しないテーブルは作成せず空を返す。他テナントの定義は構造上引けない）。
pub(crate) fn list_policies_in_source<R: ReadSource>(
    src: &R,
    scope: &str,
    table_key: &str,
) -> Result<Vec<PolicyDef>> {
    list_in_txn(src, table_key, scope)
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(POLICY_ID_COUNTER_TABLE)?;
    let current = counter
        .get(POLICY_ID_COUNTER_KEY)?
        .map(|v| v.value())
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(POLICY_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// 全テナントの `table_key` 配下のキーを最大 `limit` 件集める（バッチ走査。無制限確保をしない）。
fn collect_table_keys<R: ReadSource>(
    src: &R,
    table_key: &str,
    limit: usize,
) -> Result<Vec<(String, String, Vec<u8>)>> {
    let table = match src.open_table(POLICIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((table_key, "", "")..)? {
        let (k, v) = entry?;
        let (t, s, n) = k.value();
        if t != table_key || out.len() >= limit {
            break;
        }
        out.push((s.to_string(), n.to_string(), v.value().to_vec()));
    }
    Ok(out)
}

/// 1 バッチの件数。
const SWEEP_BATCH: usize = 256;

/// 表 `table_key` の全テナント分のポリシーを削除する（`DROP TABLE`・`DROP SCHEMA ... CASCADE` の表削除
/// から同じ write txn で呼ぶ。同名の表を再作成しても旧定義が復活しないようにする）。表が無ければ何もしない。
pub(crate) fn remove_policies_for_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table_key: &str,
) -> Result<()> {
    loop {
        let batch = collect_table_keys(write_txn, table_key, SWEEP_BATCH)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(POLICIES_TABLE)?;
        for (scope, name, _) in &batch {
            table.remove((table_key, scope.as_str(), name.as_str()))?;
        }
    }
}

/// 表の改名に追随して全テナント分の定義を新しい表キーへ移す（`ALTER TABLE RENAME` から同じ write txn で
/// 呼ぶ）。改名先に残っていた定義（通常は無い孤児）は先に掃除する。
pub(crate) fn rename_table_in_txn(
    write_txn: &redb::WriteTransaction,
    old_key: &str,
    new_key: &str,
) -> Result<()> {
    remove_policies_for_table_in_txn(write_txn, new_key)?;
    loop {
        let batch = collect_table_keys(write_txn, old_key, SWEEP_BATCH)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(POLICIES_TABLE)?;
        for (scope, name, blob) in &batch {
            table.remove((old_key, scope.as_str(), name.as_str()))?;
            table.insert((new_key, scope.as_str(), name.as_str()), blob.as_slice())?;
        }
    }
}

/// 表 `table_key` のいずれかのテナントのポリシーが列 `column` を参照しているか（`RENAME COLUMN`・
/// `DROP COLUMN`・`ALTER COLUMN TYPE` のガード用。表が無ければ `false`）。
pub(crate) fn policies_referencing_column_in_txn<R: ReadSource>(
    src: &R,
    table_key: &str,
    column: &str,
) -> Result<bool> {
    let table = match src.open_table(POLICIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in table.range((table_key, "", "")..)? {
        let (k, v) = entry?;
        let (t, _, name) = k.value();
        if t != table_key {
            break;
        }
        if decode_policy_def(name, v.value())?
            .referenced_columns
            .iter()
            .any(|c| c == column)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 対象表の前提検査（SCHEMA の現存 → 種別 → 表の存在）。ビュー・索引・SEQUENCE・マテリアライズドビューは
/// `WrongObjectKind`、表が無ければ `TableNotFound`。すべて commit 前に返るため副作用ゼロ。
fn require_policy_target_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    table_key: &str,
) -> Result<()> {
    // 名前解決後に `DROP SCHEMA` が完了しても、削除済み ID 配下の表へ定義を残さない。
    super::namespace::ensure_tenant_key_schema_alive_in_txn(write_txn, scope, table_key)?;
    let in_views = match ReadSource::open_table(write_txn, VIEWS_TABLE) {
        Ok(t) => t.get(table_key)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(e.into()),
    };
    if in_views
        || index_name_exists_in_txn(write_txn, table_key)?
        || materialized_view_exists_in_txn(write_txn, table_key)?
        || super::sequence::sequence_exists_in_txn(write_txn, table_key)?
    {
        return Err(CatalogError::WrongObjectKind(table_key.to_string()));
    }
    let table_exists = match ReadSource::open_table(write_txn, CATALOG_TABLE) {
        Ok(t) => t.get(table_key)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(e.into()),
    };
    if !table_exists {
        return Err(CatalogError::TableNotFound(table_key.to_string()));
    }
    Ok(())
}

impl Storage {
    /// `scope` のテナントとして、表 `table_key` にポリシーを作り、内部 ID を返す。単一 write txn・1 回の
    /// commit で行い、判定順は次のとおり（すべて commit 前に返るため副作用ゼロ）。
    ///
    /// 形状 → SCHEMA の現存 → 対象の種別（`WrongObjectKind`）→ 表の存在（`TableNotFound`）→ 式の束縛
    /// （`bind`。同じ txn で読んだスキーマに対して行い TOCTOU を作らない。失敗は `E` のまま返る）→ 同名の
    /// 既存（`Invalid(POLICY_ALREADY_EXISTS_MESSAGE)`）→ 件数上限（`ViewLimitExceeded`）。`def` の式・参照列・
    /// ID は無視され、`bind` の返す [`PolicyExprs`] と払い出した ID が保存される。成功時は対象表の世代を進める。
    /// 呼び出し元は `sql::policy_ddl`。
    pub(crate) fn create_policy<E, B>(
        &self,
        scope: &str,
        table_key: &str,
        def: &PolicyDef,
        bind: B,
    ) -> std::result::Result<u64, E>
    where
        E: From<CatalogError>,
        B: FnOnce(&TableSchema) -> std::result::Result<PolicyExprs, E>,
    {
        self.create_policy_inner(scope, table_key, def, bind, MAX_POLICIES_PER_TABLE)
    }

    /// [`Self::create_policy`] の本体。件数上限を引数にし、テストが小さい上限を注入できるようにする。
    fn create_policy_inner<E, B>(
        &self,
        scope: &str,
        table_key: &str,
        def: &PolicyDef,
        bind: B,
        max_policies: usize,
    ) -> std::result::Result<u64, E>
    where
        E: From<CatalogError>,
        B: FnOnce(&TableSchema) -> std::result::Result<PolicyExprs, E>,
    {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        let mut stored = def.clone();
        stored.id = 1;
        stored.using_sql = None;
        stored.check_sql = None;
        stored.referenced_columns = Vec::new();
        validate_shape(&stored)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        require_policy_target_in_txn(&write_txn, scope, table_key)?;
        let schema = require_table_schema_write_any_identity(&write_txn, table_key)?;
        let exprs = bind(&schema)?;
        stored.using_sql = exprs.using_sql;
        stored.check_sql = exprs.check_sql;
        stored.referenced_columns = exprs.referenced_columns;
        validate_shape(&stored)?;
        if get_in_txn(&*write_txn, table_key, scope, &stored.name)?.is_some() {
            return Err(invalid(POLICY_ALREADY_EXISTS_MESSAGE).into());
        }
        if list_in_txn(&*write_txn, table_key, scope)?.len() >= max_policies {
            return Err(CatalogError::ViewLimitExceeded(POLICY_LIMIT_MESSAGE.to_string()).into());
        }
        stored.id = allocate_id_in_txn(&write_txn)?;
        let blob = encode_policy_def(&stored)?;
        {
            let mut table = write_txn
                .open_table(POLICIES_TABLE)
                .map_err(CatalogError::from)?;
            table
                .insert((table_key, scope, stored.name.as_str()), blob.as_slice())
                .map_err(CatalogError::from)?;
        }
        bump_table_generation_in_txn(&write_txn, table_key)?;
        // commit 記録（SQL-38・Issue #2404）: ポリシー DDL は表世代の更新と同じ txn で DDL として記録する。
        crate::storage::commit_log::note_ddl(&write_txn, table_key);
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(stored.id)
    }

    /// `scope` のポリシー `name` を `modify` の返す定義へ置き換える（`ALTER POLICY ... [TO] [USING] [WITH CHECK]`）。
    /// 判定順: 形状 → SCHEMA の現存 → 種別 → 表の存在 → 既存の取得（無ければ `Invalid(POLICY_NOT_FOUND_MESSAGE)`。
    /// 他テナントにしか無い場合も同じ）→ `modify`（現行の定義と同じ txn のスキーマを受け取り、新しい定義を返す。
    /// 名前は変えられず、ID は引き継がれる）。成功時は対象表の世代を進める。
    pub(crate) fn alter_policy<E, M>(
        &self,
        scope: &str,
        table_key: &str,
        name: &str,
        modify: M,
    ) -> std::result::Result<(), E>
    where
        E: From<CatalogError>,
        M: FnOnce(&TableSchema, &PolicyDef) -> std::result::Result<PolicyDef, E>,
    {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        require_policy_target_in_txn(&write_txn, scope, table_key)?;
        let Some(current) = get_in_txn(&*write_txn, table_key, scope, name)? else {
            return Err(invalid(POLICY_NOT_FOUND_MESSAGE).into());
        };
        let schema = require_table_schema_write_any_identity(&write_txn, table_key)?;
        let mut next = modify(&schema, &current)?;
        next.id = current.id;
        next.name = current.name.clone();
        let blob = encode_policy_def(&next)?;
        {
            let mut table = write_txn
                .open_table(POLICIES_TABLE)
                .map_err(CatalogError::from)?;
            table
                .insert((table_key, scope, name), blob.as_slice())
                .map_err(CatalogError::from)?;
        }
        bump_table_generation_in_txn(&write_txn, table_key)?;
        // commit 記録（SQL-38・Issue #2404）: ポリシー DDL は表世代の更新と同じ txn で DDL として記録する。
        crate::storage::commit_log::note_ddl(&write_txn, table_key);
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// `scope` のポリシー `name` を `new_name` へ改名する（`ALTER POLICY ... RENAME TO`）。無ければ
    /// `Invalid(POLICY_NOT_FOUND_MESSAGE)`、改名先が既にあれば `Invalid(POLICY_ALREADY_EXISTS_MESSAGE)`。
    /// 内部 ID は引き継ぐ。削除と挿入と世代更新は同一 write txn。
    pub(crate) fn rename_policy(
        &self,
        scope: &str,
        table_key: &str,
        name: &str,
        new_name: &str,
    ) -> Result<()> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        validate_identifier(name)?;
        validate_identifier(new_name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        require_policy_target_in_txn(&write_txn, scope, table_key)?;
        let Some(mut current) = get_in_txn(&*write_txn, table_key, scope, name)? else {
            return Err(invalid(POLICY_NOT_FOUND_MESSAGE));
        };
        if name != new_name && get_in_txn(&*write_txn, table_key, scope, new_name)?.is_some() {
            return Err(invalid(POLICY_ALREADY_EXISTS_MESSAGE));
        }
        current.name = new_name.to_string();
        let blob = encode_policy_def(&current)?;
        {
            let mut table = write_txn.open_table(POLICIES_TABLE)?;
            table.remove((table_key, scope, name))?;
            table.insert((table_key, scope, new_name), blob.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_key)?;
        // commit 記録（SQL-38・Issue #2404）: ポリシー DDL は表世代の更新と同じ txn で DDL として記録する。
        crate::storage::commit_log::note_ddl(&write_txn, table_key);
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// `scope` のポリシー `name` を削除し、存在して削除したら `true`。対象が表でなければ `WrongObjectKind`、表が無ければ `TableNotFound`、ポリシーが
    /// 無ければ（他テナントにしか無い場合を含む）`Ok(false)`。削除と対象表の世代更新は同一 write txn。
    pub(crate) fn drop_policy(&self, scope: &str, table_key: &str, name: &str) -> Result<bool> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        // CREATE／ALTER と同じ対象検査（ビュー・索引・SEQUENCE 等は `WrongObjectKind`、無ければ `TableNotFound`）。
        require_policy_target_in_txn(&write_txn, scope, table_key)?;
        if get_in_txn(&*write_txn, table_key, scope, name)?.is_none() {
            return Ok(false);
        }
        {
            let mut table = write_txn.open_table(POLICIES_TABLE)?;
            table.remove((table_key, scope, name))?;
        }
        bump_table_generation_in_txn(&write_txn, table_key)?;
        // commit 記録（SQL-38・Issue #2404）: ポリシー DDL は表世代の更新と同じ txn で DDL として記録する。
        crate::storage::commit_log::note_ddl(&write_txn, table_key);
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(true)
    }

    /// `scope` の表 `table_key` のポリシーを名前の昇順で返す（読み取り専用。適用側が引く入口）。
    /// 他テナントの定義は構造上引けない。
    #[allow(dead_code)] // 適用側（後続 Issue）とテストが使う読み取り入口。
    pub(crate) fn list_policies(&self, scope: &str, table_key: &str) -> Result<Vec<PolicyDef>> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        let rt = self.db().begin_read()?;
        list_in_txn(&rt, table_key, scope)
    }
}

#[cfg(test)]
mod tests;
