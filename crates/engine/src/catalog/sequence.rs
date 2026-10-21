//! SEQUENCE のカタログ定義と採番状態の格納土台（TABLE-24・TABLE-27 のポインタ・
//! Issue #1688。親: #1686。設計記録: `docs/design/sequence-namespace-and-ledger.md`）。
//!
//! 役割: SEQUENCE の定義を専用カタログ表 [`SEQUENCES_TABLE`] へ永続化し、`CREATE SEQUENCE`／
//! `DROP SEQUENCE`（`sql::ddl` が呼ぶ）の Storage 水準の実体を提供する。SEQUENCE は表・ビュー・
//! 索引・matview と同じ relation 名前空間に属するため、衝突・種別の判定は呼び出し元
//! （`catalog.rs` の各 DDL・[`super::materialized`]）が [`sequence_exists_in_txn`] を併記する。
//!
//! テナント境界（P0）: 定義は表・ビュー・ENUM 型と同じく全テナント共有カタログに置く
//! （#1711 の SCHEMA 導入までは単一の `public`。設計記録 D3）。一方、採番状態は
//! [`SEQUENCE_STATE_TABLE`] のキー先頭を `tenant_id` にして構造的にテナント単位へ分ける
//! （値の払い出し経路は Issue #1690 の [`SequenceAllocator`]。DROP は全テナント分を削除する）。
//! 内部 ID は単調増加で再利用しないため、同名で作り直した SEQUENCE が過去の状態行を拾うことは
//! ない。
//!
//! 採番（TABLE-24・Issue #1690）: [`SequenceAllocator`] がプロセス内の現在値を持ち、書き込み txn の
//! 中で払い出して同じ txn の [`SEQUENCE_STATE_TABLE`] へ現在値を書く。ROLLBACK は永続値だけを
//! 戻しメモリ上の現在値は戻さないため、同一プロセスで同じ値を再び払い出さない（欠番は許容。
//! PostgreSQL と同じ）。明示トランザクションは単一ライタを保持するので、commit 済みの行に入った値は
//! 必ず同じ txn が永続化した現在値で覆われる。ROLLBACK 後のクラッシュでの再払い出しを補強 txn で
//! 防ぐ設計とクラッシュ注入による検証は #1692 の範囲。
//!
//! ALTER SEQUENCE（Issue #1689）: [`Storage::alter_sequence`]／[`Storage::rename_sequence`] が
//! 定義の上書き・RESTART（全テナントの状態行の削除）・RENAME（定義キーの付け替えのみ）を行う。
//! 状態行を持たないテナントの初期値は定義 blob の `restart_value`（版 2 で追加）が担う。
//!
//! fail-closed: 定義 blob の未知の版・未知のタグ・長さ不一致・範囲不変条件の違反は
//! `CorruptSchema` とし、decode で黙って補正しない。

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

mod migrated_identity;

// #2200 の移行 txn（`catalog::legacy_id_migration`）が呼ぶ入口を親モジュールへ公開する。
pub(crate) use migrated_identity::{
    assign_migrated_id_identity_in_txn, migrated_id_sequence_in_txn, tables_missing_id_sequence,
    MigratedIdIdentityOutcome,
};

use crate::storage::read_source::ReadSource;

use super::namespace::RelationKey;
use super::{
    get_table_schema_in_txn, get_table_schema_in_txn_impl, list_tables_in_txn, validate_identifier,
    validate_relation_key, CatalogError, ColumnDefault, Result, Storage, TableSchema,
    MAX_LIST_TABLES,
};

/// SEQUENCE 定義の専用カタログ表。キーは `(schema, name)`。#1711 より前の schema は
/// 常に [`DEFAULT_SEQUENCE_SCHEMA`]（`sequence_key` が唯一の組み立て点）。
pub(crate) const SEQUENCES_TABLE: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("sequences");

/// 採番状態の表。キーは `(tenant_id, sequence_id)`。値は [`encode_state`] の 10 バイト固定
/// （版 1・`last_value` i64 LE・`is_called` 1 バイト。Issue #1690）。
pub(crate) const SEQUENCE_STATE_TABLE: TableDefinition<(&str, u64), &[u8]> =
    TableDefinition::new("sequence_state");

/// 内部 ID カウンタ（固定キー 1 行の単調カウンタ。再利用しない）。
const SEQUENCE_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("sequence_id_counter");

/// [`SEQUENCE_ID_COUNTER_TABLE`] の唯一のキー。
const SEQUENCE_ID_COUNTER_KEY: &str = "next_id";

/// #1711（SCHEMA）より前の SEQUENCE が属する schema 名。
const DEFAULT_SEQUENCE_SCHEMA: &str = "public";

/// 定義 blob の版（所有者なしの書き込み版。版 1 は読み取りのみ受理する）。
const SEQUENCE_DEF_VERSION: u8 = 2;

/// 所有者あり（`owned_by` バイト = 1）の定義 blob の版（Issue #1645）。版 2 の固定部の後ろに
/// 所有する表名・列名を u16 LE 長さ付きで連結する。所有者なしは版 2 のまま書き、既存 blob の
/// バイト列と旧バイナリとの互換を変えない（旧バイナリは版 3 を `CorruptSchema` で拒否する）。
const SEQUENCE_DEF_VERSION_OWNED: u8 = 3;

/// 旧版（`restart_value` を持たない）の版番号。decode は `restart_value = start` として読む。
const SEQUENCE_DEF_VERSION_V1: u8 = 1;

/// 型タグ: bigint（本 Issue ではこれのみ）。
const TYPE_TAG_BIGINT: u8 = 1;

/// 版 1 の定義 blob の固定長（版 1 + id 8 + 型 1 + i64 × 5 + cycle 1 + owned_by 1）。
const SEQUENCE_DEF_LEN_V1: usize = 1 + 8 + 1 + 8 * 5 + 1 + 1;

/// 版 2 の定義 blob の固定長（版 1 の末尾に `restart_value` の i64 を足す）。
const SEQUENCE_DEF_LEN: usize = SEQUENCE_DEF_LEN_V1 + 8;

/// `restart_value` の位置（版 2。owned_by の直後）。
const RESTART_OFFSET: usize = SEQUENCE_DEF_LEN_V1;

/// 登録可能な SEQUENCE の総数上限（[`MAX_LIST_TABLES`] と同じ既定値。DoS 対策）。
pub(crate) const MAX_SEQUENCES: usize = MAX_LIST_TABLES;

/// 採番状態 blob の版（Issue #1690）。
const STATE_VERSION: u8 = 1;

/// 採番状態 blob の固定長（版 1 + `last_value` 8 + `is_called` 1）。
const STATE_LEN: usize = 1 + 8 + 1;

/// プロセス内に保持する `(tenant_id, sequence_id)` 現在値の件数上限（DoS 対策。超過は
/// `54000` で fail-closed。`MAX_SEQUENCES` × テナント数に比例して無制限に増やさない）。
const MAX_CURSORS: usize = 1 << 20;

/// SEQUENCE の定義値（bigint 前提。`CACHE`・`OWNED BY` は後続 Issue のため持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceParams {
    pub(crate) increment: i64,
    pub(crate) min_value: i64,
    pub(crate) max_value: i64,
    pub(crate) start: i64,
    pub(crate) cycle: bool,
}

impl SequenceParams {
    /// 範囲不変条件（`increment != 0`・`min < max`・`min <= start <= max`）を検証する。
    /// SQL 表層（`sql::ddl`）が先に `22023` で拒否するため、ここへ到達するのは
    /// Rust API の直接呼び出しと破損 blob のみ（多層防御）。
    fn validate(&self) -> Result<()> {
        if self.increment == 0
            || self.min_value >= self.max_value
            || self.start < self.min_value
            || self.start > self.max_value
        {
            return Err(CatalogError::Invalid(
                "invalid sequence parameters".to_string(),
            ));
        }
        Ok(())
    }
}

/// 定義 blob の復号結果（`restart_value` を含む完全形）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceDef {
    pub(crate) id: u64,
    pub(crate) params: SequenceParams,
    /// 状態行を持たないテナントの初期 last_value（`is_called = false`）。CREATE 時は `start`、
    /// `ALTER SEQUENCE ... RESTART` で更新される（Issue #1689）。
    pub(crate) restart_value: i64,
}

/// SEQUENCE を所有する表の列（`OWNED BY <table>.<column>`／SERIAL・IDENTITY 列。Issue #1645）。
/// `TRUNCATE ... RESTART IDENTITY` が「どの SEQUENCE を戻すか」を決めるカタログ宣言で、
/// 全テナント共有（テナントごとの状態ではない）。`DEFAULT nextval('s')` で参照されるだけの
/// SEQUENCE は所有ではない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SequenceOwner {
    pub(crate) table: String,
    pub(crate) column: String,
}

/// `MINVALUE`／`MAXVALUE` の変更指定（`ALTER SEQUENCE`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundChange {
    /// 指定なし（現行値を維持）。
    Keep,
    /// `MINVALUE n`／`MAXVALUE n`。
    Set(i64),
    /// `NO MINVALUE`／`NO MAXVALUE`（変更後の INCREMENT の符号から既定値を再導出）。
    Default,
}

/// `ALTER SEQUENCE` の変更要求（構文検証済み。範囲の意味検証は [`resolve_altered_definition`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceAlter {
    /// `None` = RESTART なし。`Some(None)` = 値なしの `RESTART`（値は変更後の START）。
    pub(crate) restart: Option<Option<i64>>,
    pub(crate) start: Option<i64>,
    pub(crate) increment: Option<i64>,
    pub(crate) min_value: BoundChange,
    pub(crate) max_value: BoundChange,
    pub(crate) cycle: Option<bool>,
}

/// 範囲の意味検証違反（SQLSTATE `22023`。文言は値を含まない固定文言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SequenceRangeError {
    IncrementZero,
    MinNotLessThanMax,
    StartBelowMin,
    StartAboveMax,
    RestartBelowMin,
    RestartAboveMax,
    CurrentBelowMin,
    CurrentAboveMax,
}

impl SequenceRangeError {
    /// `22023` の固定 detail（格納値・入力値を含めない）。
    pub(crate) fn detail(&self) -> &'static str {
        match self {
            Self::IncrementZero => "INCREMENT must not be zero",
            Self::MinNotLessThanMax => "MINVALUE must be less than MAXVALUE",
            Self::StartBelowMin => "START value cannot be less than MINVALUE",
            Self::StartAboveMax => "START value cannot be greater than MAXVALUE",
            Self::RestartBelowMin => "RESTART value cannot be less than MINVALUE",
            Self::RestartAboveMax => "RESTART value cannot be greater than MAXVALUE",
            Self::CurrentBelowMin => "current value cannot be less than MINVALUE",
            Self::CurrentAboveMax => "current value cannot be greater than MAXVALUE",
        }
    }
}

/// `alter_sequence` のエラー（範囲違反 `22023` とカタログ系を分ける。`CatalogError` は
/// `core_api` の公開型のため variant を足さない）。
#[derive(Debug)]
pub(crate) enum AlterSequenceError {
    Catalog(CatalogError),
    Range(SequenceRangeError),
}

impl From<CatalogError> for AlterSequenceError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

impl From<redb::TableError> for AlterSequenceError {
    fn from(e: redb::TableError) -> Self {
        Self::Catalog(e.into())
    }
}

impl From<redb::StorageError> for AlterSequenceError {
    fn from(e: redb::StorageError) -> Self {
        Self::Catalog(e.into())
    }
}

/// ALTER 後の定義を PostgreSQL 17 の `init_params` と同じ検査順で解決する純関数。
/// 順序: INCREMENT ≠ 0 → MIN/MAX 決定 → MIN < MAX → START ∈ [MIN, MAX]（未指定でも現行 START を
/// 検査）→ RESTART 値（値なしは START）または現在値 ∈ [MIN, MAX]。
/// 戻り値は `(新しい params, 新しい restart_value)`。`caller_current` は RESTART なしのとき
/// 呼び出し元テナントの現在値（状態行があれば。無ければ定義の `restart_value`）。
/// 定義の `restart_value` は常に新しい範囲内であることを要求する（decode 不変条件との整合）。
pub(crate) fn resolve_altered_definition(
    cur: &SequenceParams,
    cur_restart: i64,
    caller_current: Option<i64>,
    alter: &SequenceAlter,
) -> std::result::Result<(SequenceParams, i64), SequenceRangeError> {
    let increment = alter.increment.unwrap_or(cur.increment);
    if increment == 0 {
        return Err(SequenceRangeError::IncrementZero);
    }
    let ascending = increment > 0;
    let min_value = match alter.min_value {
        BoundChange::Keep => cur.min_value,
        BoundChange::Set(v) => v,
        BoundChange::Default => {
            if ascending {
                1
            } else {
                i64::MIN
            }
        }
    };
    let max_value = match alter.max_value {
        BoundChange::Keep => cur.max_value,
        BoundChange::Set(v) => v,
        BoundChange::Default => {
            if ascending {
                i64::MAX
            } else {
                -1
            }
        }
    };
    if min_value >= max_value {
        return Err(SequenceRangeError::MinNotLessThanMax);
    }
    let start = alter.start.unwrap_or(cur.start);
    if start < min_value {
        return Err(SequenceRangeError::StartBelowMin);
    }
    if start > max_value {
        return Err(SequenceRangeError::StartAboveMax);
    }
    let new_restart = match alter.restart {
        Some(Some(v)) => {
            if v < min_value {
                return Err(SequenceRangeError::RestartBelowMin);
            }
            if v > max_value {
                return Err(SequenceRangeError::RestartAboveMax);
            }
            v
        }
        Some(None) => start,
        None => {
            for current in [caller_current.unwrap_or(cur_restart), cur_restart] {
                if current < min_value {
                    return Err(SequenceRangeError::CurrentBelowMin);
                }
                if current > max_value {
                    return Err(SequenceRangeError::CurrentAboveMax);
                }
            }
            cur_restart
        }
    };
    Ok((
        SequenceParams {
            increment,
            min_value,
            max_value,
            start,
            cycle: alter.cycle.unwrap_or(cur.cycle),
        },
        new_restart,
    ))
}

/// 定義キー `(schema, name)` の唯一の組み立て点（#1711・#1717 での差し替え点）。
///
/// `name` は relation キー（[`super::namespace::RelationKey`]）。`public` は従来のキー
/// `("public", name)`（既存のバイト列を変えない）、テナントの SCHEMA 配下の内部キー
/// `#<id>.<name>` は `("#<id>", name)`（ADR D2。[`super::namespace`] の SCHEMA 内走査と同じ形）。
/// どちらも `name` の部分文字列を借用するだけで確保しない。`#` で始まるが形式が不正な名前は、
/// 呼び出し前の [`validate_relation_key`] が拒否している（ここでは `public` 側へ倒さず
/// 空キーの組にして必ず「無い」ことになる）。
fn sequence_key(name: &str) -> (&str, &str) {
    if name.starts_with('#') {
        return match name.split_once('.') {
            Some((schema, rest)) => (schema, rest),
            None => ("#", name),
        };
    }
    (DEFAULT_SEQUENCE_SCHEMA, name)
}

/// 定義キー `(schema, name)` を relation キー（[`sequence_key`] の逆）へ戻す。`public` は裸の名前、
/// SCHEMA 配下（`schema == "#<id>"`）は `#<id>.<name>`。表を走査して名前を取り出す箇所は、第 2 要素
/// だけを使わず必ず本関数を通す（SCHEMA 配下の SEQUENCE が `public` の同名に化けないため）。
pub(crate) fn sequence_entry_name((schema, name): (&str, &str)) -> String {
    if schema == DEFAULT_SEQUENCE_SCHEMA {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// 定義 blob を符号化する（版バイト先頭）。所有者なしは固定長の版 2、あり（Issue #1645）は
/// 版 3（版 2 の固定部 + u16 LE 長さ付きの表名・列名）。`restart_value` は状態行を持たない
/// テナントの初期 last_value（`is_called = false`）。
fn encode_sequence_def(
    id: u64,
    p: &SequenceParams,
    restart_value: i64,
    owner: Option<&SequenceOwner>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(SEQUENCE_DEF_LEN);
    out.push(if owner.is_some() {
        SEQUENCE_DEF_VERSION_OWNED
    } else {
        SEQUENCE_DEF_VERSION
    });
    out.extend_from_slice(&id.to_le_bytes());
    out.push(TYPE_TAG_BIGINT);
    for v in [p.increment, p.min_value, p.max_value, p.start, 1_i64] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(u8::from(p.cycle));
    out.push(u8::from(owner.is_some())); // owned_by
    out.extend_from_slice(&restart_value.to_le_bytes());
    if let Some(o) = owner {
        for name in [&o.table, &o.column] {
            // 識別子は `validate_identifier` 済み（長さは u16 に収まる）。収まらない値は
            // 書き込み側で弾かれている前提のため、飽和させて decode 側の検証に任せる。
            let len = u16::try_from(name.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
    }
    out
}

/// 版 3 の所有者部（固定部の後ろ）を復号する。長さ不一致・余剰バイト・不正な識別子は
/// `CorruptSchema`（fail-closed）。
fn decode_owner_trailer(trailer: &[u8]) -> Result<SequenceOwner> {
    let corrupt = || CatalogError::CorruptSchema("corrupt sequence definition".to_string());
    let mut rest = trailer;
    let mut names: Vec<String> = Vec::with_capacity(2);
    for is_relation in [true, false] {
        let len_raw: [u8; 2] = rest
            .get(..2)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(corrupt)?;
        let len = usize::from(u16::from_le_bytes(len_raw));
        let name_bytes = rest.get(2..2 + len).ok_or_else(corrupt)?;
        let name = std::str::from_utf8(name_bytes).map_err(|_| corrupt())?;
        // 書き込み側は表名に relation key（修飾名を含む）、列名に厳格な識別子検証を課すため、
        // 復号側も同じ検証で揃える（列名に relation key 形式が入った blob は破損扱い）。
        if is_relation {
            validate_relation_key(name).map_err(|_| corrupt())?;
        } else {
            validate_identifier(name).map_err(|_| corrupt())?;
        }
        names.push(name.to_string());
        rest = rest.get(2 + len..).ok_or_else(corrupt)?;
    }
    if !rest.is_empty() {
        return Err(corrupt());
    }
    let column = names.pop().ok_or_else(corrupt)?;
    let table = names.pop().ok_or_else(corrupt)?;
    Ok(SequenceOwner { table, column })
}

/// 定義 blob を復号する（所有者は読み捨てる）。[`decode_sequence_def_with_owner`] 参照。
fn decode_sequence_def(bytes: &[u8]) -> Result<SequenceDef> {
    decode_sequence_def_with_owner(bytes).map(|(def, _)| def)
}

/// 定義 blob を所有者込みで復号する（版 1・2・3 を受理。版 1 は `restart_value = start`、
/// 版 1・2 の所有者は常に `None`）。未知の版・タグ・長さ不一致・不変条件違反は
/// `CorruptSchema`（fail-closed。detail は固定文言で格納バイト列の断片を含めない）。
fn decode_sequence_def_with_owner(bytes: &[u8]) -> Result<(SequenceDef, Option<SequenceOwner>)> {
    let corrupt = || CatalogError::CorruptSchema("corrupt sequence definition".to_string());
    let version = bytes.first().copied();
    // 版 3 は固定部（版 2 と同じ長さ）の後ろに所有者部が続く可変長。
    let owned = version == Some(SEQUENCE_DEF_VERSION_OWNED);
    let expected_len = match version {
        Some(SEQUENCE_DEF_VERSION) => SEQUENCE_DEF_LEN,
        Some(SEQUENCE_DEF_VERSION_V1) => SEQUENCE_DEF_LEN_V1,
        Some(SEQUENCE_DEF_VERSION_OWNED) => SEQUENCE_DEF_LEN,
        _ => return Err(corrupt()),
    };
    if owned {
        if bytes.len() <= expected_len {
            return Err(corrupt());
        }
    } else if bytes.len() != expected_len {
        return Err(corrupt());
    }
    let read_i64 = |off: usize| -> Result<i64> {
        let raw: [u8; 8] = bytes
            .get(off..off + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(corrupt)?;
        Ok(i64::from_le_bytes(raw))
    };
    let id_raw: [u8; 8] = bytes
        .get(1..9)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(corrupt)?;
    let id = u64::from_le_bytes(id_raw);
    if bytes.get(9) != Some(&TYPE_TAG_BIGINT) {
        return Err(corrupt());
    }
    let params = SequenceParams {
        increment: read_i64(10)?,
        min_value: read_i64(18)?,
        max_value: read_i64(26)?,
        start: read_i64(34)?,
        cycle: match bytes.get(CYCLE_OFFSET) {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(corrupt()),
        },
    };
    // cache は 1 固定。owned_by バイトは版 3 なら 1、版 1・2 なら 0 でなければならない。
    let expected_owned_byte = u8::from(owned);
    if read_i64(42)? != 1 || bytes.get(CYCLE_OFFSET + 1) != Some(&expected_owned_byte) {
        return Err(corrupt());
    }
    params.validate().map_err(|_| corrupt())?;
    let restart_value = if version != Some(SEQUENCE_DEF_VERSION_V1) {
        read_i64(RESTART_OFFSET)?
    } else {
        params.start
    };
    if restart_value < params.min_value || restart_value > params.max_value {
        return Err(corrupt());
    }
    let owner = if owned {
        Some(decode_owner_trailer(
            bytes.get(SEQUENCE_DEF_LEN..).ok_or_else(corrupt)?,
        )?)
    } else {
        None
    };
    Ok((
        SequenceDef {
            id,
            params,
            restart_value,
        },
        owner,
    ))
}

/// cycle バイトの位置（`10 + 8 * 5` = cache の直後。owned_by はその次の 1 バイト）。
const CYCLE_OFFSET: usize = 50;

/// `name` が [`SEQUENCES_TABLE`] に登録済みかを write txn 内で判定する（relation 名前空間の
/// 衝突・種別判定用。表が未作成なら `false` で、表を作らない）。表・ビュー・索引・matview の
/// DDL は `index_name_exists_in_txn` を呼ぶ箇所に必ず本関数を併記する。
pub(crate) fn sequence_exists_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<bool> {
    // 読み取り専用オープン（write txn の `open_table` は欠落表を作ってしまうため）。
    match ReadSource::open_table(write_txn, SEQUENCES_TABLE) {
        Ok(t) => Ok(t.get(sequence_key(name))?.is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_sequence_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(SEQUENCE_ID_COUNTER_TABLE)?;
    let current = counter
        .get(SEQUENCE_ID_COUNTER_KEY)?
        .map(|v| v.value())
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(SEQUENCE_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// 採番の現在値（PostgreSQL の `last_value`／`is_called` と同じ意味）。`is_called` が偽なら
/// 次の払い出しは `last_value`（= START）そのもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeqCursor {
    last_value: i64,
    is_called: bool,
}

/// 採番状態 blob を符号化する（固定長・版バイト先頭）。
fn encode_state(c: &SeqCursor) -> [u8; STATE_LEN] {
    let mut out = [0u8; STATE_LEN];
    out[0] = STATE_VERSION;
    out[1..9].copy_from_slice(&c.last_value.to_le_bytes());
    out[9] = u8::from(c.is_called);
    out
}

/// 採番状態 blob を復号する。未知の版・長さ不一致・`is_called` の不正値は `CorruptSchema`
/// （fail-closed。detail は固定文言で格納バイト列の断片を含めない）。
fn decode_state(bytes: &[u8]) -> Result<SeqCursor> {
    let corrupt = || CatalogError::CorruptSchema("corrupt sequence state".to_string());
    if bytes.len() != STATE_LEN || bytes.first() != Some(&STATE_VERSION) {
        return Err(corrupt());
    }
    let raw: [u8; 8] = bytes
        .get(1..9)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(corrupt)?;
    let is_called = match bytes.get(9) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(corrupt()),
    };
    Ok(SeqCursor {
        last_value: i64::from_le_bytes(raw),
        is_called,
    })
}

/// 払い出しの失敗（呼び出し元の `tenant` が SQL 表層のエラーへ写像する）。
#[derive(Debug)]
pub(crate) enum AllocError {
    /// NO CYCLE の SEQUENCE が上限（`ascending`）または下限に達した（`2200H`）。
    LimitReached { ascending: bool },
    /// 現在値の保持件数が上限に達した（`54000`）。
    TooManyCursors,
    /// 内部ロックの毒化（`XX000`。fail-closed）。
    Poisoned,
    /// カタログ・永続化層のエラー。
    Catalog(CatalogError),
}

impl From<CatalogError> for AllocError {
    fn from(e: CatalogError) -> Self {
        AllocError::Catalog(e)
    }
}

/// 次の払い出し値を `cur` と定義 `p` から計算する純関数（PostgreSQL の `nextval` と同じ規則。
/// 初回は START、以降は `checked_add(increment)`。範囲外は CYCLE なら反対側の端へ折り返し、
/// NO CYCLE なら [`AllocError::LimitReached`]）。オーバーフローは範囲外として扱い、
/// 未定義動作にしない。
fn advance(p: &SequenceParams, cur: &SeqCursor) -> std::result::Result<i64, AllocError> {
    let ascending = p.increment > 0;
    if !cur.is_called {
        return Ok(cur.last_value);
    }
    match cur.last_value.checked_add(p.increment) {
        Some(v) if v >= p.min_value && v <= p.max_value => Ok(v),
        _ => {
            if p.cycle {
                Ok(if ascending { p.min_value } else { p.max_value })
            } else {
                Err(AllocError::LimitReached { ascending })
            }
        }
    }
}

/// プロセス内の SEQUENCE 現在値アロケータ（TABLE-24・Issue #1690。設計記録 D4）。`Storage` が 1 つ
/// 所有し、書き込み経路（`tenant` の INSERT 系）が書き込み txn の中で [`Self::next_value_in_txn`] を
/// 呼ぶ。キーは `(tenant_id, sequence_id)` で、他テナントの値の進み・欠番が自テナントから観測
/// できない（TABLE-24 (2)）。
#[derive(Default)]
pub(crate) struct SequenceAllocator {
    cursors: std::sync::Mutex<std::collections::HashMap<(String, u64), SeqCursor>>,
}

impl SequenceAllocator {
    /// 次の値を 1 つ払い出し、同じ `write_txn` へ現在値を永続化して返す。初回はメモリに無ければ
    /// 永続化済みの状態行（無ければ `START`・未呼び出し）から種を作る。範囲外（NO CYCLE）は
    /// 現在値を変えずに [`AllocError::LimitReached`]。ロックは短時間だけ保持する。
    pub(crate) fn next_value_in_txn(
        &self,
        write_txn: &redb::WriteTransaction,
        tenant: &str,
        sequence_id: u64,
        params: &SequenceParams,
        restart_value: i64,
    ) -> std::result::Result<i64, AllocError> {
        let mut guard = self.cursors.lock().map_err(|_| AllocError::Poisoned)?;
        let key = (tenant.to_string(), sequence_id);
        let cur = match guard.get(&key) {
            Some(c) => *c,
            None => read_state_in_txn(write_txn, tenant, sequence_id)?.unwrap_or(SeqCursor {
                last_value: restart_value,
                is_called: false,
            }),
        };
        let value = advance(params, &cur)?;
        let next = SeqCursor {
            last_value: value,
            is_called: true,
        };
        // 永続化に失敗したら現在値を進めない（失敗した文の値を消費しない）。
        write_state_in_txn(write_txn, tenant, sequence_id, &next)?;
        if !guard.contains_key(&key) && guard.len() >= MAX_CURSORS {
            return Err(AllocError::TooManyCursors);
        }
        guard.insert(key, next);
        Ok(value)
    }

    /// メモリ上の現在値の `last_value` を返す（無ければ `None`。RESTART なしの `ALTER SEQUENCE` の
    /// 範囲検査用。ROLLBACK 済みで永続値より進んだ値も含む）。
    fn peek_last_value(
        &self,
        tenant: &str,
        sequence_id: u64,
    ) -> std::result::Result<Option<i64>, AllocError> {
        let guard = self.cursors.lock().map_err(|_| AllocError::Poisoned)?;
        Ok(guard
            .get(&(tenant.to_string(), sequence_id))
            .map(|c| c.last_value))
    }

    /// メモリ上の現在値があれば `write_txn` へ永続化する（`currval` だけを使う書き込みでも、
    /// ROLLBACK で永続値が戻った状態のまま commit して払い出し済みの値を再払い出ししないため。
    /// 現在値は巻き戻さず進めない。メモリに無ければ何もしない）。
    pub(crate) fn persist_current_in_txn(
        &self,
        write_txn: &redb::WriteTransaction,
        tenant: &str,
        sequence_id: u64,
    ) -> std::result::Result<(), AllocError> {
        let guard = self.cursors.lock().map_err(|_| AllocError::Poisoned)?;
        if let Some(c) = guard.get(&(tenant.to_string(), sequence_id)) {
            write_state_in_txn(write_txn, tenant, sequence_id, c)?;
        }
        Ok(())
    }

    /// `setval` の現在値を `write_txn` へ書く（TABLE-24・Issue #1691。設計記録 D4）。保持件数の上限は
    /// 書き込みの前に検査する（超過は [`AllocError::TooManyCursors`]）。`apply_now` が真なら
    /// 同じ呼び出しでメモリ上の現在値も更新する（明示・暗黙トランザクション用。ROLLBACK しても
    /// メモリは戻らず、後続の `nextval` が `setval` の値から続く。PostgreSQL の `setval` が
    /// 非トランザクション的なのと同じ）。偽のときは更新せず、呼び出し元が commit 成功後に
    /// [`Self::apply_set_value`] を呼ぶ（autocommit 用。commit 前に更新すると、commit 失敗でメモリが
    /// 永続値より後ろへ下がり、払い出し済みの値を再び払い出しうる）。
    pub(crate) fn set_value_in_txn(
        &self,
        write_txn: &redb::WriteTransaction,
        tenant: &str,
        sequence_id: u64,
        cursor_value: (i64, bool),
        apply_now: bool,
    ) -> std::result::Result<(), AllocError> {
        let cursor = SeqCursor {
            last_value: cursor_value.0,
            is_called: cursor_value.1,
        };
        let mut guard = self.cursors.lock().map_err(|_| AllocError::Poisoned)?;
        let key = (tenant.to_string(), sequence_id);
        if !guard.contains_key(&key) && guard.len() >= MAX_CURSORS {
            return Err(AllocError::TooManyCursors);
        }
        write_state_in_txn(write_txn, tenant, sequence_id, &cursor)?;
        if apply_now {
            guard.insert(key, cursor);
        }
        Ok(())
    }

    /// commit 成功後に `setval` の値をメモリ上の現在値へ反映する（[`Self::set_value_in_txn`] を
    /// `apply_now = false` で呼んだ autocommit 経路用。writer permit を保持したまま呼ぶ）。
    /// ロックの毒化は黙って握りつぶさず、次回の払い出しが `Poisoned`（`XX000`）で失敗する。
    pub(crate) fn apply_set_value(
        &self,
        tenant: &str,
        sequence_id: u64,
        cursor_value: (i64, bool),
    ) {
        if let Ok(mut guard) = self.cursors.lock() {
            guard.insert(
                (tenant.to_string(), sequence_id),
                SeqCursor {
                    last_value: cursor_value.0,
                    is_called: cursor_value.1,
                },
            );
        }
    }

    /// `(tenant, sequence_id)` の 1 キーだけメモリ上の現在値を捨てる（`TRUNCATE ... RESTART IDENTITY`
    /// の commit 成功後に writer permit を保持したまま呼ぶ。Issue #1645）。次の払い出しは同じ txn で
    /// 書いた状態行 `{start, is_called = false}` から種を作る。他テナントのキーには触れない。
    /// ロックの毒化は黙って握りつぶさず、次回の払い出しが `Poisoned`（`XX000`）で失敗する。
    pub(crate) fn evict_tenant(&self, tenant: &str, sequence_id: u64) {
        if let Ok(mut guard) = self.cursors.lock() {
            guard.remove(&(tenant.to_string(), sequence_id));
        }
    }

    /// 保持しているカーソル件数（テストで evict の漏れを検出するための観測口）。
    #[cfg(test)]
    pub(crate) fn cursor_count(&self) -> usize {
        self.cursors.lock().map_or(0, |g| g.len())
    }

    /// `sequence_id` の現在値を全テナント分メモリから捨てる（`DROP SEQUENCE`・RESTART の commit 成功後。
    /// ロックの毒化は黙って握りつぶさず、次回払い出しで `Poisoned` になる）。
    pub(crate) fn evict(&self, sequence_id: u64) {
        if let Ok(mut guard) = self.cursors.lock() {
            guard.retain(|(_, id), _| *id != sequence_id);
        }
    }
}

/// 永続化済みの採番状態を write txn 内で読む（状態表が未作成・行なしは `None`。表を作らない）。
fn read_state_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    sequence_id: u64,
) -> Result<Option<SeqCursor>> {
    match ReadSource::open_table(write_txn, SEQUENCE_STATE_TABLE) {
        Ok(t) => match t.get((tenant, sequence_id))? {
            Some(g) => Ok(Some(decode_state(g.value())?)),
            None => Ok(None),
        },
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 採番状態を write txn へ書く（行の書き込みと同じ txn。commit されなければ永続値も戻る）。
fn write_state_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    sequence_id: u64,
    cursor: &SeqCursor,
) -> Result<()> {
    let mut t = write_txn.open_table(SEQUENCE_STATE_TABLE)?;
    t.insert((tenant, sequence_id), encode_state(cursor).as_slice())?;
    Ok(())
}

/// 定義を write txn 内で名前から引く（[`SequenceDef`]。無ければ `None`）。書き込み txn 内で
/// 解決することで、束縛から書き込みまでの間に挟まった `DROP SEQUENCE` の古い ID へ書かない
/// （TOCTOU 回避。`sql::parser::VolatileColumn::Sequence` の採番が呼ぶ）。
pub(crate) fn sequence_definition_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<Option<SequenceDef>> {
    validate_relation_key(name)?;
    match ReadSource::open_table(write_txn, SEQUENCES_TABLE) {
        Ok(t) => match t.get(sequence_key(name))? {
            Some(g) => Ok(Some(decode_sequence_def(g.value())?)),
            None => Ok(None),
        },
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `ColumnDefault` が SEQUENCE `name` を参照するか（`DEFAULT nextval('<name>')` のみ真）。
/// ワイルドカードを置かない網羅 match にして、variant を足すとここがコンパイルエラーになり、
/// `DROP SEQUENCE` の依存判定の書き漏れを防ぐ。
fn column_default_references_sequence(default: &ColumnDefault, name: &str) -> bool {
    match default {
        ColumnDefault::Text(_)
        | ColumnDefault::Number(_)
        | ColumnDefault::Bool(_)
        | ColumnDefault::CurrentTime(_) => false,
        ColumnDefault::Sequence(seq) => seq == name,
        ColumnDefault::Identity { sequence, .. } => sequence == name,
    }
}

/// SEQUENCE `name` を DEFAULT に使う列が 1 つでもあるか（2BP01 判定）。表数は
/// [`MAX_LIST_TABLES`] で打ち切られる（`list_tables_in_txn` の上限）。
fn sequence_has_dependents_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<bool> {
    // 移行済み表（MigratedId）の疑似列 `id` は列 DEFAULT を持たず所有者だけで結ばれるため、
    // 所有者がその疑似列を指す SEQUENCE も依存ありとして扱う（単独 DROP・RENAME を拒否。#1698）。
    let owned = owned_sequence_entries_in_txn(write_txn, &|o| o.column == "id")?;
    for (seq_name, _, owner) in owned {
        if seq_name != name {
            continue;
        }
        if super::table_exists_in_txn(write_txn, &owner.table)? {
            let schema = get_table_schema_in_txn_impl(write_txn, &owner.table, false)?;
            if schema.row_identity() == crate::catalog::RowIdentity::MigratedId {
                return Ok(true);
            }
        }
    }
    for table_name in list_tables_in_txn(write_txn)? {
        // 依存走査はカタログ情報だけを見る。複数アクション `ALTER TABLE` の中間状態（DROP COLUMN
        // 直後で生存列 0 本）の表も読めるよう `require_live = false` で復号する（Issue #1694）。
        let schema = get_table_schema_in_txn_impl(write_txn, &table_name, false)?;
        if schema.columns.iter().any(|c| {
            c.default_expr
                .as_ref()
                .is_some_and(|d| column_default_references_sequence(d, name))
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `(tenant_id, sequence_id)` の状態行から `sequence_id` が一致する行を全テナント分削除する。
/// `retain` で走査削除しキーを無制限に集めない。状態表が未作成なら何もしない（表を作らない）。
fn delete_sequence_state_in_txn(write_txn: &redb::WriteTransaction, id: u64) -> Result<()> {
    match ReadSource::open_table(write_txn, SEQUENCE_STATE_TABLE) {
        // 存在確認のハンドルは書き込みオープンの前に解放する（`TableAlreadyOpen` 回避）。
        Ok(t) => drop(t),
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let mut t = write_txn.open_table(SEQUENCE_STATE_TABLE)?;
    t.retain(|(_, sid), _| sid != id)?;
    Ok(())
}

/// 定義 blob を write txn 内で取得する。無ければ他種別の relation か不在かで
/// `WrongObjectKind`／`TableNotFound` を返す（DROP・ALTER 共通。存在情報はテナント非依存）。
fn load_sequence_bytes_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<Vec<u8>> {
    let existing = match ReadSource::open_table(write_txn, SEQUENCES_TABLE) {
        Ok(t) => t.get(sequence_key(name))?.map(|g| g.value().to_vec()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(bytes) = existing {
        return Ok(bytes);
    }
    if super::table_exists_in_txn(write_txn, name)?
        || super::view_exists_in_txn(write_txn, name)?
        || super::index_name_exists_in_txn(write_txn, name)?
        || super::materialized_view_exists_in_txn(write_txn, name)?
    {
        return Err(CatalogError::WrongObjectKind(name.to_string()));
    }
    Err(CatalogError::TableNotFound(name.to_string()))
}

/// 呼び出し元テナントの現在値を読む（RESTART なしの `ALTER SEQUENCE` の範囲検査用）。
/// メモリ上の現在値（ROLLBACK 済みで永続値より進んだ値を含む）を優先し、無ければ永続化済みの
/// 状態行の `last_value`、どちらも無ければ `None`（定義の `restart_value` を使う）。状態行が
/// 壊れていれば `CorruptSchema` で fail-closed。他テナントの状態は読まない（`22023` の有無から
/// 他テナントの採番の進みを観測させない）。
fn caller_current_value_in_txn(
    allocator: &SequenceAllocator,
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    id: u64,
) -> std::result::Result<Option<i64>, AlterSequenceError> {
    let mem = allocator
        .peek_last_value(tenant_id, id)
        .map_err(|e| match e {
            AllocError::Catalog(c) => AlterSequenceError::Catalog(c),
            _ => AlterSequenceError::Catalog(CatalogError::CorruptSchema(
                "sequence allocator unavailable".to_string(),
            )),
        })?;
    if mem.is_some() {
        return Ok(mem);
    }
    Ok(read_state_in_txn(write_txn, tenant_id, id)?.map(|c| c.last_value))
}

/// `setval` の失敗（`sql::sequence_fn` が SQL 表層のエラーへ写像する）。
#[derive(Debug)]
pub(crate) enum SetvalError {
    /// 値が `[min_value, max_value]` の外（`22003`）。
    OutOfRange,
    /// セッションの `currval` 状態の保持件数が上限に達した（`54000`）。
    SessionFull,
    /// アロケータのエラー（保持件数上限・ロック毒化・永続化層）。
    Alloc(AllocError),
    /// カタログのエラー（不在 `42P01`・他種別 `42809`・破損）。
    Catalog(CatalogError),
}

impl From<CatalogError> for SetvalError {
    fn from(e: CatalogError) -> Self {
        SetvalError::Catalog(e)
    }
}

impl From<AllocError> for SetvalError {
    fn from(e: AllocError) -> Self {
        SetvalError::Alloc(e)
    }
}

/// `setval` の入力（名前・値・`is_called`）。
pub(crate) struct SetvalArgs<'a> {
    pub(crate) name: &'a str,
    pub(crate) value: i64,
    pub(crate) is_called: bool,
}

/// `setval(name, value, is_called)` の本体（TABLE-24・Issue #1691）。同一 `write_txn` 内で名前の
/// 解決（他種別 `WrongObjectKind`・不在 `TableNotFound`。存在情報はテナント非依存）→ 範囲検査
/// （`min_value <= value <= max_value`）→ `admit`（セッション状態の受け入れ可否。偽なら
/// [`SetvalError::SessionFull`]）→ 呼び出し元テナントの状態行の書き込み、を行い内部 ID を返す。
/// 作用するのは `(tenant_id, sequence_id)` の 1 行だけで、他テナントの採番には影響しない
/// （TABLE-24 (2)）。`apply_now` の意味は [`SequenceAllocator::set_value_in_txn`] を参照。
pub(crate) fn setval_in_txn(
    allocator: &SequenceAllocator,
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    args: &SetvalArgs<'_>,
    admit: impl FnOnce(u64) -> bool,
    apply_now: bool,
) -> std::result::Result<u64, SetvalError> {
    validate_relation_key(args.name)?;
    let bytes = load_sequence_bytes_in_txn(write_txn, args.name)?;
    let def = decode_sequence_def(&bytes)?;
    if args.value < def.params.min_value || args.value > def.params.max_value {
        return Err(SetvalError::OutOfRange);
    }
    if !admit(def.id) {
        return Err(SetvalError::SessionFull);
    }
    allocator.set_value_in_txn(
        write_txn,
        tenant_id,
        def.id,
        (args.value, args.is_called),
        apply_now,
    )?;
    Ok(def.id)
}

/// 内部 ID `id` の SEQUENCE が定義として存在するか（`lastval` の存在確認用。TABLE-24・Issue #1691）。
/// 名前ではなく ID で引くため、RENAME 後も真で、DROP 後に同名で作り直しても偽になる。走査は
/// [`MAX_SEQUENCES`] 件で打ち切り、定義 blob の復号失敗は `CorruptSchema`（fail-closed）。
pub(crate) fn sequence_id_exists_in<R: ReadSource>(src: &R, id: u64) -> Result<bool> {
    let table = match src.open_table(SEQUENCES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in table.iter()?.take(MAX_SEQUENCES) {
        let (_, v) = entry?;
        if decode_sequence_def(v.value())?.id == id {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `tables` のいずれかが所有する SEQUENCE の定義を内部 ID 昇順で返す（`TRUNCATE ... RESTART
/// IDENTITY` の対象列挙。SQL-48 (3)、Issue #1645）。`DEFAULT nextval('s')` で参照されるだけの
/// SEQUENCE は含めない。全テナント共有のカタログ宣言だけで決まり、テナントの採番状態には依存
/// しない。表が未作成なら空（表を作らない）。走査は [`MAX_SEQUENCES`] 件で打ち切り、定義 blob の
/// 復号失敗は `CorruptSchema`（スキップせず全体を fail-closed にする）。
pub(crate) fn owned_sequences_for_tables_in_txn(
    write_txn: &redb::WriteTransaction,
    tables: &[String],
) -> Result<Vec<SequenceDef>> {
    let table = match ReadSource::open_table(write_txn, SEQUENCES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()?.take(MAX_SEQUENCES) {
        let (_, v) = entry?;
        let (def, owner) = decode_sequence_def_with_owner(v.value())?;
        if owner.is_some_and(|o| tables.contains(&o.table)) {
            out.push(def);
        }
    }
    out.sort_by_key(|d| d.id);
    Ok(out)
}

/// `(tenant, def.id)` の状態行を `{last_value: START, is_called: false}` で書く（`TRUNCATE ...
/// RESTART IDENTITY` の本体。Issue #1645）。`restart_value`（`ALTER SEQUENCE ... RESTART WITH`
/// でずれうる）ではなく START へ戻す（PostgreSQL の `ResetSequence` と同じ）。状態行を削除せず
/// 明示的に書くのは、行が無いと次の払い出しが `restart_value` にフォールバックするため。他テナントの
/// 行には触れない（TABLE-24 (2)）。メモリ上の現在値の破棄は呼び出し元が commit 後に行う。
pub(crate) fn reset_sequence_state_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    def: &SequenceDef,
) -> Result<()> {
    write_state_in_txn(
        write_txn,
        tenant,
        def.id,
        &SeqCursor {
            last_value: def.params.start,
            is_called: false,
        },
    )
}

/// SEQUENCE `name` の所有者（表・列）を設定または解除する（Issue #1645）。表・列の実在を同一
/// write txn で確認する（表不在 `TableNotFound`・列不在 `Invalid`・SEQUENCE 不在／他種別は
/// `load_sequence_bytes_in_txn` の契約）。`OWNED BY`／SERIAL／IDENTITY の結線（#1694）が呼ぶ入口。
pub(crate) fn set_sequence_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
    owner: Option<SequenceOwner>,
) -> Result<()> {
    validate_relation_key(name)?;
    let bytes = load_sequence_bytes_in_txn(write_txn, name)?;
    let def = decode_sequence_def(&bytes)?;
    if let Some(o) = &owner {
        validate_relation_key(&o.table)?;
        validate_identifier(&o.column)?;
        let schema = get_table_schema_in_txn(write_txn, &o.table)?;
        // 移行済み表（MigratedId）の `id` は列を持たない疑似列なので、所有者としてだけ許す（#1698）。
        let pseudo_id =
            o.column == "id" && schema.row_identity() == crate::catalog::RowIdentity::MigratedId;
        if !pseudo_id && !schema.columns.iter().any(|c| c.name == o.column) {
            return Err(CatalogError::Invalid(
                "sequence owner column does not exist".to_string(),
            ));
        }
    }
    let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
    table.insert(
        sequence_key(name),
        encode_sequence_def(def.id, &def.params, def.restart_value, owner.as_ref()).as_slice(),
    )?;
    Ok(())
}

/// relation 名前空間（表・ビュー・索引・matview・SEQUENCE）で `name` が使用済みか。
/// `CREATE SEQUENCE`・暗黙 SEQUENCE の名前決定が共有する。
fn relation_name_taken_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<bool> {
    Ok(super::table_exists_in_txn(write_txn, name)?
        || super::view_exists_in_txn(write_txn, name)?
        || super::index_name_exists_in_txn(write_txn, name)?
        || super::materialized_view_exists_in_txn(write_txn, name)?
        || sequence_exists_in_txn(write_txn, name)?)
}

/// SEQUENCE を作る write txn 内本体（名前衝突 `TableAlreadyExists` → 件数上限 `ViewLimitExceeded`
/// → 保存。begin・commit を含まない）。[`Storage::create_sequence`] と、表の作成と同じ txn に載せる
/// [`create_table_with_implicit_sequences_in_txn`] が共有する。内部 ID を払い出し返す。
pub(crate) fn create_sequence_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
    params: &SequenceParams,
) -> Result<u64> {
    validate_relation_key(name)?;
    params.validate()?;
    super::namespace::ensure_key_schema_alive_in_txn(write_txn, name)?;
    if relation_name_taken_in_txn(write_txn, name)? {
        return Err(CatalogError::TableAlreadyExists(name.to_string()));
    }
    let id = allocate_sequence_id_in_txn(write_txn)?;
    let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
    if table.len()? >= MAX_SEQUENCES as u64 {
        return Err(CatalogError::ViewLimitExceeded(
            "too many sequences".to_string(),
        ));
    }
    table.insert(
        sequence_key(name),
        encode_sequence_def(id, params, params.start, None).as_slice(),
    )?;
    Ok(id)
}

/// 暗黙 SEQUENCE を持つ列 1 件の宣言（SERIAL 系・IDENTITY 列。TABLE-24・Issue #1694／#1695）。
/// `CREATE TABLE` の列宣言順に並べて渡し、`<table>_<col>_seq` の衝突番号（`seq1`…）が列順に
/// 決まるようにする。`sql::ddl::execute_create_table` が組み立てる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImplicitSequence {
    /// 所有する列名（`schema.columns` に存在する）。
    pub(crate) column: String,
    /// 作成する SEQUENCE の定義（検証済みの値）。
    pub(crate) params: SequenceParams,
    /// `Some` なら IDENTITY 列（種別）、`None` なら SERIAL 系（`DEFAULT nextval`）。
    pub(crate) identity: Option<super::IdentityKind>,
}

/// [`Storage::create_table_with_implicit_sequences`] の write txn 内本体（begin・commit を含まない）。
/// 表の世代 bump は呼び出し側が commit 直前に行う。`Err` の場合は呼び出し側が txn を破棄する（途中まで作った
/// SEQUENCE も残らない）。
pub(crate) fn create_table_with_implicit_sequences_in_txn(
    write_txn: &redb::WriteTransaction,
    schema: &TableSchema,
    serials: &[ImplicitSequence],
) -> Result<Vec<String>> {
    if relation_name_taken_in_txn(write_txn, &schema.name)? {
        return Err(CatalogError::TableAlreadyExists(schema.name.clone()));
    }
    let mut named_schema = schema.clone();
    let mut created: Vec<String> = Vec::with_capacity(serials.len());
    for implicit in serials {
        let column = &implicit.column;
        let seq_name =
            choose_implicit_sequence_name_in_txn(write_txn, &schema.name, column, &created)?;
        create_sequence_in_txn(write_txn, &seq_name, &implicit.params)?;
        let col = named_schema
            .columns
            .iter_mut()
            .find(|c| &c.name == column)
            .ok_or_else(|| {
                CatalogError::Invalid("serial column does not exist in the table".to_string())
            })?;
        col.default_expr = Some(match implicit.identity {
            Some(kind) => ColumnDefault::Identity {
                sequence: seq_name.clone(),
                kind,
            },
            None => ColumnDefault::Sequence(seq_name.clone()),
        });
        created.push(seq_name);
    }
    // 既定値を付けた後のスキーマを、`create_table` と同じ前処理・txn 内本体へ通す。
    let (named_schema, pre_encoded) = Storage::prepare_create_table(&named_schema)?;
    Storage::create_table_in_txn(write_txn, &named_schema, pre_encoded)?;
    for (implicit, seq_name) in serials.iter().zip(&created) {
        set_sequence_owner_in_txn(
            write_txn,
            seq_name,
            Some(SequenceOwner {
                table: schema.name.clone(),
                column: implicit.column.clone(),
            }),
        )?;
    }
    Ok(created)
}

/// PostgreSQL の `makeObjectName` 相当。`<name1>_<name2>_<label>` を識別子長上限に収め、
/// 収まらない場合は長い方の名前から 1 文字ずつ削る（同じ長さなら `name2` を削る）。識別子は
/// ASCII（`validate_identifier` 済み）の前提で、境界で切れない不正入力は切らずにそのまま返し、
/// 呼び出し側の `validate_identifier` に拒否させる。
fn make_object_name(name1: &str, name2: &str, label: &str) -> String {
    let overhead = label.len().saturating_add(2);
    let avail = super::MAX_IDENTIFIER_LEN.saturating_sub(overhead);
    let (mut l1, mut l2) = (name1.len(), name2.len());
    while l1.saturating_add(l2) > avail && (l1 > 0 || l2 > 0) {
        if l1 > l2 {
            l1 -= 1;
        } else {
            l2 -= 1;
        }
    }
    match (name1.get(..l1), name2.get(..l2)) {
        (Some(a), Some(b)) => format!("{a}_{b}_{label}"),
        _ => format!("{name1}_{name2}_{label}"),
    }
}

/// 暗黙 SEQUENCE（SERIAL 列・IDENTITY 列の裏）の名前を決める（TABLE-24・Issue #1694）。
/// PostgreSQL の `ChooseRelationName` と同じく `<table>_<column>_seq`、使用済みなら
/// `<table>_<column>_seq1`、`..._seq2` と数字を足す。`reserved` は同じ文で先に決めた名前
/// （txn にはまだ見えない）、`table` は作成中の表名（カタログには未登録）で、いずれも使用済みとして扱う。
/// 試行は [`MAX_SEQUENCES`]` + 1` 回で打ち切り、尽きたら `ViewLimitExceeded`（無制限ループの防止）。
/// 結果は必ず `validate_identifier` に通す。write txn 内で呼ぶこと（衝突判定と作成の間に別の
/// 作成が割り込まない）。#1698 の `<table>_id_seq` も本関数の規則に揃える。
pub(crate) fn choose_implicit_sequence_name_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
    column: &str,
    reserved: &[String],
) -> Result<String> {
    validate_relation_key(table)?;
    validate_identifier(column)?;
    // SCHEMA 配下の表（`#<id>.<name>`）の暗黙 SEQUENCE は同じ SCHEMA に置く（名前部から
    // 候補を作り、接頭辞 `#<id>.` を付け直す。TABLE-27・Issue #1717）。
    let (prefix, base) = match RelationKey::parse(table)? {
        RelationKey::Public(name) => (String::new(), name),
        RelationKey::InSchema { schema_id, name } => (RelationKey::schema_prefix(schema_id), name),
    };
    for pass in 0..=MAX_SEQUENCES {
        let label = if pass == 0 {
            "seq".to_string()
        } else {
            format!("seq{pass}")
        };
        let candidate = format!("{prefix}{}", make_object_name(&base, column, &label));
        if candidate == table
            || reserved.iter().any(|r| r == &candidate)
            || relation_name_taken_in_txn(write_txn, &candidate)?
        {
            continue;
        }
        validate_relation_key(&candidate)?;
        return Ok(candidate);
    }
    Err(CatalogError::ViewLimitExceeded(
        "too many sequences".to_string(),
    ))
}

/// SEQUENCE `name` を write txn 内で削除する（定義と全テナント分の状態行。[`Storage::drop_sequence`] と
/// `DROP SCHEMA ... CASCADE`〔TABLE-27・Issue #1717〕が共有する本体）。無ければ `TableNotFound`、
/// `DEFAULT nextval` で参照する列が残っていれば `DependentObjectsStillExist`。戻り値の内部 ID は、
/// commit 成功後にメモリ上の現在値を [`SequenceAllocator::evict`] で捨てるために呼び出し側が使う。
pub(crate) fn drop_sequence_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<u64> {
    let bytes = load_sequence_bytes_in_txn(write_txn, name)?;
    let id = decode_sequence_def(&bytes)?.id;
    if sequence_has_dependents_in_txn(write_txn, name)? {
        return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
    }
    {
        let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
        table.remove(sequence_key(name))?;
    }
    delete_sequence_state_in_txn(write_txn, id)?;
    Ok(id)
}

/// 所有者が `matches` を満たす SEQUENCE を同じ write txn で削除する（定義と全テナント分の状態行）。
/// `DROP TABLE`・`ALTER TABLE DROP COLUMN` が所有 SEQUENCE を道連れにするための入口（Issue #1694）。
/// 削除対象の SEQUENCE を `DEFAULT nextval` で参照する列が（呼び出し側が先に消した表・列を除いて）
/// 残っていれば `DependentObjectsStillExist`（`2BP01`。DEFAULT の連鎖削除はしない）。走査は
/// [`MAX_SEQUENCES`] 件で打ち切り、定義の復号失敗は `CorruptSchema`。返り値の内部 ID は、commit 成功後に
/// メモリ上の現在値を [`SequenceAllocator::evict`] で捨てるために呼び出し側へ返す（内部 ID は
/// 再利用しないので、捨て損ねても別 SEQUENCE の値には化けない）。
pub(crate) fn drop_owned_sequences_in_txn(
    write_txn: &redb::WriteTransaction,
    matches: impl Fn(&SequenceOwner) -> bool,
) -> Result<Vec<u64>> {
    let targets = owned_sequence_entries_in_txn(write_txn, &matches)?;
    let mut ids = Vec::with_capacity(targets.len());
    for (name, def, _) in &targets {
        if sequence_has_dependents_in_txn(write_txn, name)? {
            return Err(CatalogError::DependentObjectsStillExist(name.clone()));
        }
        ids.push(def.id);
    }
    for (name, def, _) in &targets {
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            table.remove(sequence_key(name))?;
        }
        delete_sequence_state_in_txn(write_txn, def.id)?;
    }
    Ok(ids)
}

/// 所有者が `matches` を満たす SEQUENCE の `(名前, 定義, 所有者)` を列挙する
/// （[`MAX_SEQUENCES`] 件で打ち切り。読み取りハンドルは返す前に解放する）。
fn owned_sequence_entries_in_txn<S: ReadSource>(
    write_txn: &S,
    matches: &impl Fn(&SequenceOwner) -> bool,
) -> Result<Vec<(String, SequenceDef, SequenceOwner)>> {
    let table = match ReadSource::open_table(write_txn, SEQUENCES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()?.take(MAX_SEQUENCES) {
        let (k, v) = entry?;
        let (def, owner) = decode_sequence_def_with_owner(v.value())?;
        if let Some(owner) = owner {
            if matches(&owner) {
                out.push((sequence_entry_name(k.value()), def, owner));
            }
        }
    }
    Ok(out)
}

/// 所有者が `rewrite` で書き換わる SEQUENCE の所有者を同じ write txn で付け替える
/// （`ALTER TABLE RENAME TO`・`RENAME COLUMN` が所有者を追従させる入口。Issue #1694）。
/// `rewrite` が `None` を返した SEQUENCE は変更しない。古い所有者（存在しない表・列）を
/// 残さないためのもので、定義の他の値（内部 ID・パラメータ・`restart_value`）は変えない。
pub(crate) fn rewrite_sequence_owners_in_txn(
    write_txn: &redb::WriteTransaction,
    rewrite: impl Fn(&SequenceOwner) -> Option<SequenceOwner>,
) -> Result<()> {
    let targets = owned_sequence_entries_in_txn(write_txn, &|o| rewrite(o).is_some())?;
    for (name, def, owner) in targets {
        let Some(new_owner) = rewrite(&owner) else {
            continue;
        };
        let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
        table.insert(
            sequence_key(&name),
            encode_sequence_def(def.id, &def.params, def.restart_value, Some(&new_owner))
                .as_slice(),
        )?;
    }
    Ok(())
}

impl Storage {
    /// [`set_sequence_owner_in_txn`] を単独の write txn で実行する。
    #[cfg_attr(not(test), allow(dead_code))] // 結線は `*_in_txn` 経由。単独版は SQL の `OWNED BY` が入るまでテストのみ。
    pub(crate) fn set_sequence_owner(
        &self,
        name: &str,
        owner: Option<SequenceOwner>,
    ) -> Result<()> {
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        set_sequence_owner_in_txn(&write_txn, name, owner)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// SEQUENCE を作成する。判定順は同一 write txn 内で名前の衝突（`TableAlreadyExists`。
    /// 表・ビュー・索引・matview・SEQUENCE）→ 件数上限（`ViewLimitExceeded`）→ 保存。
    /// `sql::ddl::execute_create_sequence` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn create_sequence(&self, name: &str, params: &SequenceParams) -> Result<()> {
        validate_relation_key(name)?;
        params.validate()?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        create_sequence_in_txn(&write_txn, name, params)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// 表と暗黙 SEQUENCE を単一 write txn で作る（`CREATE TABLE ... (n SERIAL)`。TABLE-24・
    /// Issue #1694）。`serials` の各宣言（SERIAL 系・IDENTITY）について、PostgreSQL の `ChooseRelationName`
    /// 相当で `<table>_<col>_seq` を決め、SEQUENCE の挿入 → 列の `DEFAULT nextval` 付与 → 表の作成 →
    /// 所有者（`OWNED BY <table>.<col>`）設定を行い、1 回の commit で確定する。途中のどの段階で
    /// 失敗しても txn ごと破棄するため、孤児 SEQUENCE も半端な表も残らない（`IF NOT EXISTS` で表が既に
    /// あった場合も同じ）。`sql::ddl::execute_create_table` から呼ばれる（権限ゲート通過後）。
    ///
    /// 判定順は表名の衝突（`TableAlreadyExists`）→ 名前の決定・件数上限（`ViewLimitExceeded`）→
    /// 既存の `create_table` と同じ検証。返り値は作成した SEQUENCE 名（`serials` と同順）。
    pub(crate) fn create_table_with_implicit_sequences(
        &self,
        schema: &TableSchema,
        serials: &[ImplicitSequence],
    ) -> Result<Vec<String>> {
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let created = create_table_with_implicit_sequences_in_txn(&write_txn, schema, serials)?;
        super::bump_table_generation_in_txn(&write_txn, &schema.name)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(created)
    }

    /// SEQUENCE を削除する。名前が他種別なら `WrongObjectKind`、無ければ `TableNotFound`。
    /// 同一 write txn 内で依存 DEFAULT の検査（`DependentObjectsStillExist`）→ 定義の削除 →
    /// 全テナント分の状態行の削除を行う。
    pub(crate) fn drop_sequence(&self, name: &str) -> Result<()> {
        validate_relation_key(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let id = drop_sequence_in_txn(&write_txn, name)?;
        // commit 成功後にだけメモリ上の現在値を捨てる（内部 ID は再利用しないため、失敗時に
        // 残っても害はなく、成功時に残すと全テナント分が滞留する。設計記録 D3）。
        let allocator = self.sequence_allocator();
        crate::recovery::commit_boundary::commit_and_finish(write_txn, (), |()| {
            allocator.evict(id);
            crate::recovery::commit_boundary::PostCommitResult::Ok
        })
        .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// SEQUENCE を変更する（`ALTER SEQUENCE`。Issue #1689）。同一 write txn 内で名前の解決
    /// （他種別 `WrongObjectKind`・不在 `TableNotFound`）→ 範囲の意味検証（[`SequenceRangeError`]）→
    /// 定義の上書き → RESTART なら全テナント分の状態行の削除、を行う。RESTART の作用範囲は全テナント
    /// （結果を他テナントの状態に依存させない。TABLE-24 (2) のポインタ）。operation_id 台帳は開かない。
    /// `sql::ddl::execute_alter_sequence` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn alter_sequence(
        &self,
        name: &str,
        tenant_id: &str,
        alter: &SequenceAlter,
    ) -> std::result::Result<(), AlterSequenceError> {
        validate_relation_key(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let bytes = load_sequence_bytes_in_txn(&write_txn, name)?;
        let (def, owner) = decode_sequence_def_with_owner(&bytes)?;
        let caller_current = if alter.restart.is_none() {
            caller_current_value_in_txn(self.sequence_allocator(), &write_txn, tenant_id, def.id)?
        } else {
            None
        };
        let (params, restart_value) =
            resolve_altered_definition(&def.params, def.restart_value, caller_current, alter)
                .map_err(AlterSequenceError::Range)?;
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            table.insert(
                sequence_key(name),
                encode_sequence_def(def.id, &params, restart_value, owner.as_ref()).as_slice(),
            )?;
        }
        if alter.restart.is_some() {
            delete_sequence_state_in_txn(&write_txn, def.id)?;
        }
        // RESTART のときだけメモリ上の現在値を捨てる（それ以外のオプション変更は現在値を保持し、
        // ROLLBACK 済みの消費済み値の再払い出しを防ぐ）。破棄は commit 直後に writer permit を
        // 保持したまま行い、commit と破棄の間に別の書き込みが古い現在値で払い出せないようにする。
        let restart = alter.restart.is_some();
        let allocator = self.sequence_allocator();
        let id = def.id;
        crate::recovery::commit_boundary::commit_and_finish(write_txn, (), |()| {
            if restart {
                allocator.evict(id);
            }
            crate::recovery::commit_boundary::PostCommitResult::Ok
        })
        .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// SEQUENCE の名前を変更する（`ALTER SEQUENCE ... RENAME TO`。Issue #1689）。定義キーだけを
    /// 付け替え、内部 ID と状態行は変えない（設計記録 D3）。判定順は旧名の解決 → 新名の衝突
    /// （5 種の relation。同名への変更も `TableAlreadyExists`）→ 依存 DEFAULT の有無
    /// （`DependentObjectsStillExist`。名前参照の DEFAULT が壊れるのを防ぐ fail-closed）。
    pub(crate) fn rename_sequence(&self, old: &str, new: &str) -> Result<()> {
        validate_relation_key(old)?;
        validate_relation_key(new)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let bytes = load_sequence_bytes_in_txn(&write_txn, old)?;
        // blob の健全性を確認してから付け替える（破損は CorruptSchema で拒否）。
        decode_sequence_def(&bytes)?;
        if super::table_exists_in_txn(&write_txn, new)?
            || super::view_exists_in_txn(&write_txn, new)?
            || super::index_name_exists_in_txn(&write_txn, new)?
            || super::materialized_view_exists_in_txn(&write_txn, new)?
            || sequence_exists_in_txn(&write_txn, new)?
        {
            return Err(CatalogError::TableAlreadyExists(new.to_string()));
        }
        if sequence_has_dependents_in_txn(&write_txn, old)? {
            return Err(CatalogError::DependentObjectsStillExist(old.to_string()));
        }
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            table.remove(sequence_key(old))?;
            table.insert(sequence_key(new), bytes.as_slice())?;
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// `setval` を autocommit で実行する（TABLE-24・Issue #1691。設計記録 D4）。自前の書き込み txn で
    /// [`setval_in_txn`] を行い、commit 成功後に writer permit を保持したままメモリ上の現在値を
    /// 反映する（commit 失敗時はメモリを変えない）。operation_id 台帳は開かない（冪等で、台帳には
    /// 採番値を保存しない。D1）。`sql::sequence_fn` から呼ばれ、内部 ID を返す。
    pub(crate) fn set_sequence_value(
        &self,
        tenant_id: &str,
        args: &SetvalArgs<'_>,
        admit: impl FnOnce(u64) -> bool,
    ) -> std::result::Result<u64, SetvalError> {
        validate_relation_key(args.name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let allocator = self.sequence_allocator();
        let id = setval_in_txn(allocator, &write_txn, tenant_id, args, admit, false)?;
        let cursor = (args.value, args.is_called);
        crate::recovery::commit_boundary::commit_and_finish(write_txn, (), |()| {
            allocator.apply_set_value(tenant_id, id, cursor);
            crate::recovery::commit_boundary::PostCommitResult::Ok
        })
        .map_err(super::convert_storage_error)?;
        Ok(id)
    }

    /// 内部 ID `id` の SEQUENCE が存在するかをスナップショット読み取りで返す（autocommit の
    /// `lastval` 用。[`sequence_id_exists_in`]）。
    pub(crate) fn sequence_id_exists(&self, id: u64) -> Result<bool> {
        let read_txn = self.db().begin_read()?;
        sequence_id_exists_in(&read_txn, id)
    }

    /// 定義をスナップショット読み取りで取得する（`(id, params)`。無ければ `None`）。
    #[cfg_attr(not(test), allow(dead_code))] // #1690 が使う。
    pub(crate) fn sequence_definition(&self, name: &str) -> Result<Option<(u64, SequenceParams)>> {
        Ok(self
            .sequence_definition_full(name)?
            .map(|d| (d.id, d.params)))
    }

    /// 定義の所有者を取得する（テスト専用。SERIAL 展開・RENAME 追従・DROP の検証に使う）。
    #[cfg(test)]
    pub(crate) fn sequence_owner(&self, name: &str) -> Result<Option<SequenceOwner>> {
        validate_relation_key(name)?;
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(SEQUENCES_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(sequence_key(name))? {
            Some(g) => Ok(decode_sequence_def_with_owner(g.value())?.1),
            None => Ok(None),
        }
    }

    /// 定義を `restart_value` 込みで取得する（スナップショット読み取り。無ければ `None`）。
    #[cfg_attr(not(test), allow(dead_code))] // #1690 が使う。
    pub(crate) fn sequence_definition_full(&self, name: &str) -> Result<Option<SequenceDef>> {
        validate_relation_key(name)?;
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(SEQUENCES_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(sequence_key(name))? {
            Some(g) => Ok(Some(decode_sequence_def(g.value())?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    /// 所有者部の列名は厳格な識別子検証を通す（relation key 形式の列名は破損扱い）。
    #[test]
    fn owner_trailer_rejects_relation_key_column() {
        let enc = |table: &str, column: &str| {
            let mut v = Vec::new();
            for n in [table, column] {
                v.extend_from_slice(&(n.len() as u16).to_le_bytes());
                v.extend_from_slice(n.as_bytes());
            }
            v
        };
        let ok = decode_owner_trailer(&enc("#1.t", "c")).expect("valid owner");
        assert_eq!((ok.table.as_str(), ok.column.as_str()), ("#1.t", "c"));
        assert!(matches!(
            decode_owner_trailer(&enc("t", "#1.c")),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    /// 暗黙 SEQUENCE の名前の切り詰め（PostgreSQL の `makeObjectName` 相当。Issue #1694）。
    #[test]
    fn make_object_name_fits_identifier_limit() {
        assert_eq!(make_object_name("t", "n", "seq"), "t_n_seq");
        assert_eq!(make_object_name("t", "n", "seq12"), "t_n_seq12");
        // 長い方から 1 文字ずつ削る（同じ長さなら列側を削る）。
        let long = make_object_name(&"a".repeat(63), &"b".repeat(63), "seq");
        assert!(long.len() <= 63, "{long}");
        assert!(long.ends_with("_seq"));
        validate_identifier(&long).expect("valid identifier");
        let t = make_object_name(&"t".repeat(40), "n", "seq1");
        assert_eq!(t.len(), 40 + 1 + 1 + 1 + 4);
        let clipped = make_object_name(&"t".repeat(60), "n", "seq1");
        assert!(
            clipped.len() <= 63 && clipped.ends_with("_n_seq1"),
            "{clipped}"
        );
    }

    use super::*;

    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn params() -> SequenceParams {
        SequenceParams {
            increment: 1,
            min_value: 1,
            max_value: i64::MAX,
            start: 1,
            cycle: false,
        }
    }

    /// 状態行をテスト用に直接書く（値の書き込み経路は #1690 が追加する）。
    fn put_state(storage: &Storage, tenant: &str, id: u64) {
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = txn.open_table(SEQUENCE_STATE_TABLE).expect("open");
            t.insert((tenant, id), [0u8].as_slice()).expect("insert");
        }
        crate::recovery::commit_boundary::commit(txn).expect("commit");
    }

    fn state_exists(storage: &Storage, tenant: &str, id: u64) -> bool {
        let rt = storage.db().begin_read().expect("read");
        match rt.open_table(SEQUENCE_STATE_TABLE) {
            Ok(t) => t.get((tenant, id)).expect("get").is_some(),
            Err(_) => false,
        }
    }

    #[test]
    fn blob_roundtrip() {
        let p = SequenceParams {
            increment: -2,
            min_value: i64::MIN,
            max_value: 10,
            start: 10,
            cycle: true,
        };
        let d = decode_sequence_def(&encode_sequence_def(7, &p, 10, None)).expect("decode");
        assert_eq!(
            d,
            SequenceDef {
                id: 7,
                params: p,
                restart_value: 10
            }
        );
    }

    #[test]
    fn v1_blob_reads_restart_value_as_start() {
        let p = SequenceParams {
            start: 5,
            min_value: 1,
            ..params()
        };
        let mut v1 = encode_sequence_def(3, &p, 5, None);
        v1[0] = SEQUENCE_DEF_VERSION_V1;
        v1.truncate(SEQUENCE_DEF_LEN_V1);
        let d = decode_sequence_def(&v1).expect("v1 decode");
        assert_eq!((d.id, d.params, d.restart_value), (3, p, 5));
    }

    #[test]
    fn v2_restart_value_out_of_range_is_corrupt() {
        let good = encode_sequence_def(1, &params(), 1, None);
        let mut bad = good.clone();
        bad[RESTART_OFFSET..RESTART_OFFSET + 8].copy_from_slice(&0_i64.to_le_bytes());
        assert!(matches!(
            decode_sequence_def(&bad),
            Err(CatalogError::CorruptSchema(_))
        ));
        // 版 1 の長さのまま版 2 を名乗る blob も拒否。
        assert!(matches!(
            decode_sequence_def(&good[..SEQUENCE_DEF_LEN_V1]),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn blob_corruption_is_rejected() {
        let good = encode_sequence_def(1, &params(), 1, None);
        let mut bad_version = good.clone();
        bad_version[0] = 9;
        let mut bad_type = good.clone();
        bad_type[9] = 9;
        let mut bad_cycle = good.clone();
        bad_cycle[50] = 2;
        let mut bad_owned = good.clone();
        bad_owned[51] = 1;
        for b in [
            bad_version,
            bad_type,
            bad_cycle,
            bad_owned,
            good[..good.len() - 1].to_vec(),
            Vec::new(),
        ] {
            assert!(matches!(
                decode_sequence_def(&b),
                Err(CatalogError::CorruptSchema(_))
            ));
        }
        // 範囲不変条件違反（increment 0）も破損扱い。
        let mut zero_inc = good;
        zero_inc[10..18].copy_from_slice(&0_i64.to_le_bytes());
        assert!(matches!(
            decode_sequence_def(&zero_inc),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn ids_are_monotonic_and_not_reused() {
        let (s, _g) = tmp_storage("seq_ids");
        s.create_sequence("a", &params()).expect("create");
        let (id1, _) = s.sequence_definition("a").expect("def").expect("some");
        s.drop_sequence("a").expect("drop");
        s.create_sequence("a", &params()).expect("recreate");
        let (id2, _) = s.sequence_definition("a").expect("def").expect("some");
        assert!(id2 > id1);
    }

    #[test]
    fn duplicate_and_missing_names() {
        let (s, _g) = tmp_storage("seq_dup");
        s.create_sequence("a", &params()).expect("create");
        assert!(matches!(
            s.create_sequence("a", &params()),
            Err(CatalogError::TableAlreadyExists(_))
        ));
        assert!(matches!(
            s.drop_sequence("nope"),
            Err(CatalogError::TableNotFound(_))
        ));
    }

    #[test]
    fn drop_removes_state_for_all_tenants_only_for_that_sequence() {
        let (s, _g) = tmp_storage("seq_state");
        s.create_sequence("a", &params()).expect("a");
        s.create_sequence("b", &params()).expect("b");
        let (ida, _) = s.sequence_definition("a").unwrap().unwrap();
        let (idb, _) = s.sequence_definition("b").unwrap().unwrap();
        put_state(&s, "t1", ida);
        put_state(&s, "t2", ida);
        put_state(&s, "t1", idb);
        // テナントのキーが違えば別の行（他テナントの状態は読めない）。
        assert!(!state_exists(&s, "t3", ida));
        s.drop_sequence("a").expect("drop");
        assert!(!state_exists(&s, "t1", ida));
        assert!(!state_exists(&s, "t2", ida));
        assert!(state_exists(&s, "t1", idb));
    }

    #[test]
    fn drop_without_state_table_does_not_create_it() {
        let (s, _g) = tmp_storage("seq_nostate");
        s.create_sequence("a", &params()).expect("a");
        s.drop_sequence("a").expect("drop");
        let rt = s.db().begin_read().expect("read");
        assert!(matches!(
            rt.open_table(SEQUENCE_STATE_TABLE),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
    }

    #[test]
    fn invalid_params_are_rejected() {
        let (s, _g) = tmp_storage("seq_invalid");
        let mut p = params();
        p.increment = 0;
        assert!(matches!(
            s.create_sequence("a", &p),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn state_blob_roundtrip_and_corruption() {
        let c = SeqCursor {
            last_value: -7,
            is_called: true,
        };
        assert_eq!(decode_state(&encode_state(&c)).expect("decode"), c);
        let good = encode_state(&c);
        let mut bad_version = good;
        bad_version[0] = 9;
        let mut bad_flag = good;
        bad_flag[9] = 2;
        for b in [
            bad_version.to_vec(),
            bad_flag.to_vec(),
            good[..good.len() - 1].to_vec(),
            Vec::new(),
        ] {
            assert!(matches!(
                decode_state(&b),
                Err(CatalogError::CorruptSchema(_))
            ));
        }
    }

    fn cursor(last_value: i64, is_called: bool) -> SeqCursor {
        SeqCursor {
            last_value,
            is_called,
        }
    }

    #[test]
    fn advance_follows_postgres_rules() {
        let p = params();
        // 初回は START そのもの。
        assert_eq!(advance(&p, &cursor(1, false)).expect("first"), 1);
        assert_eq!(advance(&p, &cursor(1, true)).expect("next"), 2);
        // NO CYCLE の上限到達とオーバーフロー（checked）は `LimitReached`。
        let capped = SequenceParams { max_value: 5, ..p };
        assert!(matches!(
            advance(&capped, &cursor(5, true)),
            Err(AllocError::LimitReached { ascending: true })
        ));
        assert!(matches!(
            advance(&params(), &cursor(i64::MAX, true)),
            Err(AllocError::LimitReached { ascending: true })
        ));
        // 降順の下限。
        let down = SequenceParams {
            increment: -1,
            min_value: -2,
            max_value: 0,
            start: 0,
            cycle: false,
        };
        assert_eq!(advance(&down, &cursor(-1, true)).expect("next"), -2);
        assert!(matches!(
            advance(&down, &cursor(-2, true)),
            Err(AllocError::LimitReached { ascending: false })
        ));
        // CYCLE は反対側の端へ折り返す。
        let up_cycle = SequenceParams {
            max_value: 5,
            cycle: true,
            ..p
        };
        assert_eq!(advance(&up_cycle, &cursor(5, true)).expect("wrap"), 1);
        let down_cycle = SequenceParams {
            cycle: true,
            ..down
        };
        assert_eq!(advance(&down_cycle, &cursor(-2, true)).expect("wrap"), 0);
        // i64 の端でも未定義動作にならない。
        let wide = SequenceParams {
            increment: i64::MAX,
            min_value: i64::MIN,
            max_value: i64::MAX,
            start: 0,
            cycle: true,
        };
        assert_eq!(
            advance(&wide, &cursor(i64::MAX, true)).expect("wrap"),
            i64::MIN
        );
    }

    #[test]
    fn allocator_persists_state_and_resumes_from_it() {
        let (s, _g) = tmp_storage("seq_alloc");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        {
            let txn = s.begin_write_txn().expect("txn");
            let alloc = s.sequence_allocator();
            assert_eq!(
                alloc
                    .next_value_in_txn(&txn, "t1", id, &p, p.start)
                    .expect("v"),
                1
            );
            assert_eq!(
                alloc
                    .next_value_in_txn(&txn, "t1", id, &p, p.start)
                    .expect("v"),
                2
            );
            // 別テナントは独立した系列。
            assert_eq!(
                alloc
                    .next_value_in_txn(&txn, "t2", id, &p, p.start)
                    .expect("v"),
                1
            );
            crate::recovery::commit_boundary::commit(txn).expect("commit");
        }
        // 永続化された現在値から再開する（メモリが空の新しいアロケータ）。
        let fresh = SequenceAllocator::default();
        let txn = s.begin_write_txn().expect("txn");
        assert_eq!(
            fresh
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            3
        );
        assert_eq!(
            fresh
                .next_value_in_txn(&txn, "t2", id, &p, p.start)
                .expect("v"),
            2
        );
    }

    #[test]
    fn rolled_back_values_are_not_reissued_in_the_same_process() {
        let (s, _g) = tmp_storage("seq_rollback_mem");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        let alloc = s.sequence_allocator();
        {
            let txn = s.begin_write_txn().expect("txn");
            assert_eq!(
                alloc
                    .next_value_in_txn(&txn, "t1", id, &p, p.start)
                    .expect("v"),
                1
            );
            drop(txn); // ROLLBACK 相当（永続値は戻る）。
        }
        let txn = s.begin_write_txn().expect("txn");
        assert_eq!(
            alloc
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            2
        );
    }

    #[test]
    fn alter_without_restart_keeps_unpersisted_current_value() {
        let (s, _g) = tmp_storage("seq_alter_keep");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        let alloc = s.sequence_allocator();
        {
            let txn = s.begin_write_txn().expect("txn");
            assert_eq!(
                alloc
                    .next_value_in_txn(&txn, "t1", id, &p, p.start)
                    .expect("v"),
                1
            );
            drop(txn); // ROLLBACK 相当。
        }
        let a = SequenceAlter {
            restart: None,
            start: None,
            increment: Some(2),
            min_value: BoundChange::Keep,
            max_value: BoundChange::Keep,
            cycle: None,
        };
        s.alter_sequence("a", "t1", &a).expect("alter");
        let (_, p2) = s.sequence_definition("a").unwrap().unwrap();
        let txn = s.begin_write_txn().expect("txn");
        // 消費済みの 1 は再払い出しされず、新しい INCREMENT で進む。
        assert_eq!(
            alloc
                .next_value_in_txn(&txn, "t1", id, &p2, p2.start)
                .expect("v"),
            3
        );
    }

    #[test]
    fn alter_without_restart_succeeds_after_nextval_persisted_state() {
        let (s, _g) = tmp_storage("seq_alter_state");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        {
            let txn = s.begin_write_txn().expect("txn");
            s.sequence_allocator()
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v");
            crate::recovery::commit_boundary::commit(txn).expect("commit");
        }
        let a = SequenceAlter {
            increment: Some(2),
            ..no_alter()
        };
        s.alter_sequence("a", "t1", &a).expect("alter");
        // 現在値（1）が新しい範囲の外なら 22023 相当で拒否される。
        let out_of_range = SequenceAlter {
            min_value: BoundChange::Set(5),
            max_value: BoundChange::Set(10),
            start: Some(5),
            ..no_alter()
        };
        assert!(matches!(
            s.alter_sequence("a", "t1", &out_of_range),
            Err(AlterSequenceError::Range(
                SequenceRangeError::CurrentBelowMin
            ))
        ));
    }

    #[test]
    fn persist_current_saves_memory_cursor_without_rewinding() {
        let (s, _g) = tmp_storage("seq_persist_cur");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        let alloc = s.sequence_allocator();
        {
            let txn = s.begin_write_txn().expect("txn");
            alloc
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v");
            drop(txn); // ROLLBACK 相当（永続値は無い）。
        }
        {
            let txn = s.begin_write_txn().expect("txn");
            alloc
                .persist_current_in_txn(&txn, "t1", id)
                .expect("persist");
            crate::recovery::commit_boundary::commit(txn).expect("commit");
        }
        let fresh = SequenceAllocator::default();
        let txn = s.begin_write_txn().expect("txn");
        assert_eq!(
            fresh
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            2
        );
    }

    #[test]
    fn limit_failure_does_not_advance_the_cursor() {
        let (s, _g) = tmp_storage("seq_limit_mem");
        let capped = SequenceParams {
            max_value: 2,
            ..params()
        };
        s.create_sequence("a", &capped).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        let alloc = s.sequence_allocator();
        let txn = s.begin_write_txn().expect("txn");
        assert_eq!(
            alloc
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            1
        );
        assert_eq!(
            alloc
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            2
        );
        for _ in 0..2 {
            assert!(matches!(
                alloc.next_value_in_txn(&txn, "t1", id, &p, p.start),
                Err(AllocError::LimitReached { ascending: true })
            ));
        }
    }

    #[test]
    fn drop_evicts_cursors_and_recreate_starts_over() {
        let (s, _g) = tmp_storage("seq_evict");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        {
            let txn = s.begin_write_txn().expect("txn");
            s.sequence_allocator()
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v");
            crate::recovery::commit_boundary::commit(txn).expect("commit");
        }
        s.drop_sequence("a").expect("drop");
        s.create_sequence("a", &params()).expect("recreate");
        let (id2, p2) = s.sequence_definition("a").unwrap().unwrap();
        assert!(id2 > id);
        let txn = s.begin_write_txn().expect("txn");
        assert_eq!(
            s.sequence_allocator()
                .next_value_in_txn(&txn, "t1", id2, &p2, p2.start)
                .expect("v"),
            1
        );
    }

    #[test]
    fn column_default_dependency_blocks_drop() {
        let (s, _g) = tmp_storage("seq_dep");
        s.create_sequence("a", &params()).expect("create");
        s.create_table(&crate::catalog::TableSchema::new(
            "tbl",
            vec![
                crate::catalog::ColumnDef::new("n", crate::catalog::ColumnType::BigInt, false)
                    .with_default(ColumnDefault::Sequence("a".to_string())),
            ],
        ))
        .expect("create table");
        assert!(matches!(
            s.drop_sequence("a"),
            Err(CatalogError::DependentObjectsStillExist(_))
        ));
    }

    fn no_alter() -> SequenceAlter {
        SequenceAlter {
            restart: None,
            start: None,
            increment: None,
            min_value: BoundChange::Keep,
            max_value: BoundChange::Keep,
            cycle: None,
        }
    }

    #[test]
    fn resolve_check_order_increment_first() {
        let a = SequenceAlter {
            increment: Some(0),
            min_value: BoundChange::Set(10),
            max_value: BoundChange::Set(5),
            ..no_alter()
        };
        assert_eq!(
            resolve_altered_definition(&params(), 1, None, &a),
            Err(SequenceRangeError::IncrementZero)
        );
        let a = SequenceAlter {
            min_value: BoundChange::Set(10),
            max_value: BoundChange::Set(5),
            ..no_alter()
        };
        assert_eq!(
            resolve_altered_definition(&params(), 1, None, &a),
            Err(SequenceRangeError::MinNotLessThanMax)
        );
    }

    #[test]
    fn resolve_checks_current_start_even_when_unspecified() {
        // CREATE SEQUENCE s; ALTER SEQUENCE s MINVALUE 5（現行 START 1 が範囲外）。
        let a = SequenceAlter {
            min_value: BoundChange::Set(5),
            ..no_alter()
        };
        assert_eq!(
            resolve_altered_definition(&params(), 1, None, &a),
            Err(SequenceRangeError::StartBelowMin)
        );
        // START 5 を併記しても現在値 1 が範囲外。
        let a = SequenceAlter {
            start: Some(5),
            ..a
        };
        assert_eq!(
            resolve_altered_definition(&params(), 1, None, &a),
            Err(SequenceRangeError::CurrentBelowMin)
        );
        // RESTART を併記すれば通り、restart_value は START になる。
        let a = SequenceAlter {
            restart: Some(None),
            ..a
        };
        let (p, r) = resolve_altered_definition(&params(), 1, None, &a).expect("ok");
        assert_eq!((p.min_value, p.start, r), (5, 5, 5));
    }

    #[test]
    fn resolve_no_minmax_rederives_from_new_increment() {
        let a = SequenceAlter {
            increment: Some(-1),
            ..no_alter()
        };
        // 他は維持: min=1・max=i64::MAX・start=1 で有効。
        let (p, _) = resolve_altered_definition(&params(), 1, None, &a).expect("ok");
        assert_eq!((p.increment, p.min_value, p.max_value), (-1, 1, i64::MAX));
        // NO MINVALUE／NO MAXVALUE を併記すると降順の既定（max=-1）で START 1 が範囲外。
        let a = SequenceAlter {
            min_value: BoundChange::Default,
            max_value: BoundChange::Default,
            ..a
        };
        assert_eq!(
            resolve_altered_definition(&params(), 1, None, &a),
            Err(SequenceRangeError::StartAboveMax)
        );
    }

    #[test]
    fn resolve_start_change_keeps_restart_and_restart_uses_start() {
        let p0 = SequenceParams {
            start: 10,
            ..params()
        };
        let a = SequenceAlter {
            start: Some(20),
            ..no_alter()
        };
        let (p, r) = resolve_altered_definition(&p0, 10, None, &a).expect("ok");
        assert_eq!((p.start, r), (20, 10));
        let a = SequenceAlter {
            restart: Some(None),
            ..no_alter()
        };
        let (_, r) = resolve_altered_definition(&p, r, None, &a).expect("ok");
        assert_eq!(r, 20);
        let a = SequenceAlter {
            restart: Some(Some(0)),
            ..no_alter()
        };
        assert_eq!(
            resolve_altered_definition(&p, 10, None, &a),
            Err(SequenceRangeError::RestartBelowMin)
        );
    }

    #[test]
    fn alter_restart_clears_state_of_all_tenants_for_that_sequence_only() {
        let (s, _g) = tmp_storage("seq_alter_restart");
        s.create_sequence("a", &params()).expect("a");
        s.create_sequence("b", &params()).expect("b");
        let ida = s.sequence_definition("a").unwrap().unwrap().0;
        let idb = s.sequence_definition("b").unwrap().unwrap().0;
        put_state(&s, "t1", ida);
        put_state(&s, "t2", ida);
        put_state(&s, "t1", idb);
        let a = SequenceAlter {
            restart: Some(Some(3)),
            ..no_alter()
        };
        s.alter_sequence("a", "t1", &a).expect("alter");
        assert!(!state_exists(&s, "t1", ida));
        assert!(!state_exists(&s, "t2", ida));
        assert!(state_exists(&s, "t1", idb));
        let d = s.sequence_definition_full("a").unwrap().unwrap();
        assert_eq!((d.id, d.restart_value), (ida, 3));
    }

    #[test]
    fn alter_without_state_table_does_not_create_it() {
        let (s, _g) = tmp_storage("seq_alter_nostate");
        s.create_sequence("a", &params()).expect("a");
        let a = SequenceAlter {
            restart: Some(None),
            increment: Some(5),
            ..no_alter()
        };
        s.alter_sequence("a", "t1", &a).expect("alter");
        let rt = s.db().begin_read().expect("read");
        assert!(matches!(
            rt.open_table(SEQUENCE_STATE_TABLE),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
        let d = s.sequence_definition_full("a").unwrap().unwrap();
        assert_eq!((d.params.increment, d.restart_value), (5, 1));
    }

    #[test]
    fn alter_failure_leaves_definition_unchanged() {
        let (s, _g) = tmp_storage("seq_alter_fail");
        s.create_sequence("a", &params()).expect("a");
        let before = s.sequence_definition_full("a").unwrap().unwrap();
        let a = SequenceAlter {
            min_value: BoundChange::Set(5),
            ..no_alter()
        };
        assert!(matches!(
            s.alter_sequence("a", "t1", &a),
            Err(AlterSequenceError::Range(_))
        ));
        assert_eq!(s.sequence_definition_full("a").unwrap().unwrap(), before);
        assert!(matches!(
            s.alter_sequence("nope", "t1", &no_alter()),
            Err(AlterSequenceError::Catalog(CatalogError::TableNotFound(_)))
        ));
    }

    #[test]
    fn alter_without_restart_fails_closed_on_caller_state_only() {
        let (s, _g) = tmp_storage("seq_alter_caller");
        s.create_sequence("a", &params()).expect("a");
        let id = s.sequence_definition("a").unwrap().unwrap().0;
        put_state(&s, "other", id);
        let a = SequenceAlter {
            increment: Some(2),
            ..no_alter()
        };
        // 他テナントの状態行の有無では結果が変わらない。
        s.alter_sequence("a", "caller", &a).expect("alter");
        put_state(&s, "caller", id);
        assert!(matches!(
            s.alter_sequence("a", "caller", &a),
            Err(AlterSequenceError::Catalog(CatalogError::CorruptSchema(_)))
        ));
    }

    #[test]
    fn rename_keeps_id_and_state_and_rejects_conflicts() {
        let (s, _g) = tmp_storage("seq_rename");
        s.create_sequence("a", &params()).expect("a");
        s.create_sequence("b", &params()).expect("b");
        let id = s.sequence_definition("a").unwrap().unwrap().0;
        put_state(&s, "t1", id);
        s.rename_sequence("a", "c").expect("rename");
        assert!(s.sequence_definition("a").unwrap().is_none());
        assert_eq!(s.sequence_definition("c").unwrap().unwrap().0, id);
        assert!(state_exists(&s, "t1", id));
        assert!(matches!(
            s.rename_sequence("a", "d"),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(matches!(
            s.rename_sequence("c", "b"),
            Err(CatalogError::TableAlreadyExists(_))
        ));
        assert!(matches!(
            s.rename_sequence("c", "c"),
            Err(CatalogError::TableAlreadyExists(_))
        ));
    }

    /// DROP の全走査コストの実測（設計記録が #1688 に委ねた判断。結果は設計記録へ数値で残す）。
    /// 閾値アサートは置かず、走査が完走し対象行だけが消えることのみ検証する。
    #[test]
    fn drop_scan_cost_measurement() {
        let (s, _g) = tmp_storage("seq_scan");
        s.create_sequence("a", &params()).expect("a");
        s.create_sequence("b", &params()).expect("b");
        let (ida, _) = s.sequence_definition("a").unwrap().unwrap();
        let (idb, _) = s.sequence_definition("b").unwrap().unwrap();
        let txn = s.begin_write_txn().expect("txn");
        {
            let mut t = txn.open_table(SEQUENCE_STATE_TABLE).expect("open");
            for i in 0..10_000 {
                let tenant = format!("tenant{i}");
                t.insert((tenant.as_str(), ida), [0u8].as_slice()).unwrap();
                t.insert((tenant.as_str(), idb), [0u8].as_slice()).unwrap();
            }
        }
        crate::recovery::commit_boundary::commit(txn).expect("commit");
        let start = std::time::Instant::now();
        s.drop_sequence("a").expect("drop");
        let elapsed = start.elapsed();
        eprintln!("sequence drop scan (10000 tenants x 2 sequences): {elapsed:?}");
        assert!(!state_exists(&s, "tenant0", ida));
        assert!(state_exists(&s, "tenant9999", idb));
    }
}
/// `setval`／`lastval` のカタログ層（TABLE-24・Issue #1691）。
#[cfg(test)]
mod setval_tests {
    use super::*;

    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn params() -> SequenceParams {
        SequenceParams {
            increment: 1,
            min_value: 1,
            max_value: 100,
            start: 1,
            cycle: false,
        }
    }

    fn args(name: &str, value: i64, is_called: bool) -> SetvalArgs<'_> {
        SetvalArgs {
            name,
            value,
            is_called,
        }
    }

    /// 次の払い出し値（永続化まで行う）。
    fn next(s: &Storage, tenant: &str, id: u64, p: &SequenceParams) -> i64 {
        let txn = s.begin_write_txn().expect("txn");
        let v = s
            .sequence_allocator()
            .next_value_in_txn(&txn, tenant, id, p, p.start)
            .expect("next");
        crate::recovery::commit_boundary::commit(txn).expect("commit");
        v
    }

    #[test]
    fn set_sequence_value_then_next_follows_is_called() {
        let (s, _g) = tmp_storage("setval_basic");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        let got = s
            .set_sequence_value("t1", &args("a", 10, true), |_| true)
            .expect("setval");
        assert_eq!(got, id);
        assert_eq!(next(&s, "t1", id, &p), 11);
        s.set_sequence_value("t1", &args("a", 50, false), |_| true)
            .expect("setval");
        assert_eq!(next(&s, "t1", id, &p), 50);
        // 他テナントは影響を受けない。
        assert_eq!(next(&s, "t2", id, &p), 1);
    }

    #[test]
    fn set_sequence_value_checks_range_and_names() {
        let (s, _g) = tmp_storage("setval_range");
        s.create_sequence("a", &params()).expect("create");
        for v in [0, 101, i64::MIN, i64::MAX] {
            assert!(matches!(
                s.set_sequence_value("t1", &args("a", v, true), |_| true),
                Err(SetvalError::OutOfRange)
            ));
        }
        for v in [1, 100] {
            s.set_sequence_value("t1", &args("a", v, true), |_| true)
                .expect("bounds are inclusive");
        }
        assert!(matches!(
            s.set_sequence_value("t1", &args("nope", 1, true), |_| true),
            Err(SetvalError::Catalog(CatalogError::TableNotFound(_)))
        ));
        assert!(matches!(
            s.set_sequence_value("t1", &args("a", 1, true), |_| false),
            Err(SetvalError::SessionFull)
        ));
    }

    #[test]
    fn autocommit_setval_is_persisted_and_survives_a_fresh_allocator() {
        let (s, _g) = tmp_storage("setval_persist");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        s.set_sequence_value("t1", &args("a", 20, true), |_| true)
            .expect("setval");
        let fresh = SequenceAllocator::default();
        let txn = s.begin_write_txn().expect("txn");
        assert_eq!(
            fresh
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            21
        );
    }

    #[test]
    fn txn_setval_applies_memory_immediately_and_rollback_keeps_it() {
        let (s, _g) = tmp_storage("setval_txn");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        let txn = s.begin_write_txn().expect("txn");
        setval_in_txn(
            s.sequence_allocator(),
            &txn,
            "t1",
            &args("a", 30, true),
            |_| true,
            true,
        )
        .expect("setval");
        assert_eq!(
            s.sequence_allocator()
                .next_value_in_txn(&txn, "t1", id, &p, p.start)
                .expect("v"),
            31
        );
        drop(txn); // ROLLBACK 相当
                   // メモリ上の現在値は戻らない（欠番。再払い出しをしない）。
        assert_eq!(next(&s, "t1", id, &p), 32);
    }

    #[test]
    fn deferred_apply_does_not_touch_memory_before_commit() {
        let (s, _g) = tmp_storage("setval_deferred");
        s.create_sequence("a", &params()).expect("create");
        let (id, p) = s.sequence_definition("a").unwrap().unwrap();
        assert_eq!(next(&s, "t1", id, &p), 1);
        let txn = s.begin_write_txn().expect("txn");
        setval_in_txn(
            s.sequence_allocator(),
            &txn,
            "t1",
            &args("a", 50, true),
            |_| true,
            false,
        )
        .expect("setval");
        drop(txn); // commit しない
        assert_eq!(next(&s, "t1", id, &p), 2);
    }

    #[test]
    fn sequence_id_exists_tracks_drop_rename_and_recreate() {
        let (s, _g) = tmp_storage("setval_exists");
        assert!(!s.sequence_id_exists(1).expect("empty"));
        s.create_sequence("a", &params()).expect("create");
        let (id, _) = s.sequence_definition("a").unwrap().unwrap();
        assert!(s.sequence_id_exists(id).expect("exists"));
        s.rename_sequence("a", "b").expect("rename");
        assert!(s.sequence_id_exists(id).expect("renamed"));
        s.drop_sequence("b").expect("drop");
        assert!(!s.sequence_id_exists(id).expect("dropped"));
        s.create_sequence("b", &params()).expect("recreate");
        assert!(!s.sequence_id_exists(id).expect("new id"));
    }

    // --- Issue #1645: 所有者・RESTART IDENTITY 用の土台 ---

    fn owner(table: &str, column: &str) -> SequenceOwner {
        SequenceOwner {
            table: table.to_string(),
            column: column.to_string(),
        }
    }

    fn make_table(s: &Storage, name: &str) {
        s.create_table(&crate::catalog::TableSchema::new(
            name,
            vec![crate::catalog::ColumnDef::new(
                "n",
                crate::catalog::ColumnType::BigInt,
                false,
            )],
        ))
        .expect("create table");
    }

    #[test]
    fn owned_blob_v3_roundtrip_and_unowned_stays_v2() {
        let o = owner("tbl", "n");
        let blob = encode_sequence_def(5, &params(), 1, Some(&o));
        assert_eq!(blob[0], SEQUENCE_DEF_VERSION_OWNED);
        let (d, got) = decode_sequence_def_with_owner(&blob).expect("decode");
        assert_eq!((d.id, d.restart_value), (5, 1));
        assert_eq!(got, Some(o));
        let unowned = encode_sequence_def(5, &params(), 1, None);
        assert_eq!(unowned[0], SEQUENCE_DEF_VERSION);
        assert_eq!(unowned.len(), SEQUENCE_DEF_LEN);
        assert_eq!(decode_sequence_def_with_owner(&unowned).unwrap().1, None);
    }

    #[test]
    fn owned_blob_corruption_is_rejected() {
        let good = encode_sequence_def(1, &params(), 1, Some(&owner("tbl", "n")));
        let mut trailing = good.clone();
        trailing.push(0);
        let mut bad_len = good.clone();
        bad_len[SEQUENCE_DEF_LEN] = 200;
        let mut bad_name = good.clone();
        bad_name[SEQUENCE_DEF_LEN + 2] = b'"';
        let mut owned_byte_zero = good.clone();
        owned_byte_zero[CYCLE_OFFSET + 1] = 0;
        for b in [
            trailing,
            bad_len,
            bad_name,
            owned_byte_zero,
            good[..SEQUENCE_DEF_LEN].to_vec(),
            good[..good.len() - 1].to_vec(),
        ] {
            assert!(matches!(
                decode_sequence_def_with_owner(&b),
                Err(CatalogError::CorruptSchema(_))
            ));
        }
    }

    #[test]
    fn set_owner_validates_table_and_column_and_survives_alter() {
        let (s, _g) = tmp_storage("seq_owner");
        s.create_sequence("a", &params()).expect("create");
        assert!(matches!(
            s.set_sequence_owner("a", Some(owner("nope", "n"))),
            Err(CatalogError::TableNotFound(_))
        ));
        make_table(&s, "tbl");
        assert!(matches!(
            s.set_sequence_owner("a", Some(owner("tbl", "zzz"))),
            Err(CatalogError::Invalid(_))
        ));
        s.set_sequence_owner("a", Some(owner("tbl", "n")))
            .expect("set");
        // ALTER SEQUENCE は所有者を引き継ぐ。
        let alter = SequenceAlter {
            restart: None,
            start: None,
            increment: Some(2),
            min_value: BoundChange::Keep,
            max_value: BoundChange::Keep,
            cycle: None,
        };
        s.alter_sequence("a", "t1", &alter).expect("alter");
        let txn = s.begin_write_txn().expect("txn");
        let got = owned_sequences_for_tables_in_txn(&txn, &["tbl".to_string()]).expect("list");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].params.increment, 2);
        drop(txn);
        // 解除すると列挙されない。
        s.set_sequence_owner("a", None).expect("unset");
        let txn = s.begin_write_txn().expect("txn");
        assert!(
            owned_sequences_for_tables_in_txn(&txn, &["tbl".to_string()])
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn owned_sequences_are_filtered_by_table_and_ordered_by_id() {
        let (s, _g) = tmp_storage("seq_owned_list");
        make_table(&s, "t1");
        make_table(&s, "t2");
        for n in ["a", "b", "c"] {
            s.create_sequence(n, &params()).expect("create");
        }
        s.set_sequence_owner("c", Some(owner("t1", "n"))).unwrap();
        s.set_sequence_owner("a", Some(owner("t1", "n"))).unwrap();
        s.set_sequence_owner("b", Some(owner("t2", "n"))).unwrap();
        let ids = |tables: &[&str]| -> Vec<u64> {
            let txn = s.begin_write_txn().expect("txn");
            let t: Vec<String> = tables.iter().map(|x| x.to_string()).collect();
            owned_sequences_for_tables_in_txn(&txn, &t)
                .expect("list")
                .iter()
                .map(|d| d.id)
                .collect()
        };
        let id = |n: &str| s.sequence_definition(n).unwrap().unwrap().0;
        assert_eq!(ids(&["t1"]), vec![id("a"), id("c")]);
        assert_eq!(ids(&["t2"]), vec![id("b")]);
        assert_eq!(ids(&["t1", "t2"]).len(), 3);
        assert!(ids(&["other"]).is_empty());
    }

    #[test]
    fn reset_writes_only_callers_row_with_start() {
        let (s, _g) = tmp_storage("seq_reset");
        let p = SequenceParams {
            start: 50,
            min_value: 1,
            ..params()
        };
        s.create_sequence("a", &p).expect("create");
        let def = s.sequence_definition_full("a").unwrap().unwrap();
        assert_eq!(next(&s, "t1", def.id, &def.params), 50);
        assert_eq!(next(&s, "t1", def.id, &def.params), 51);
        assert_eq!(next(&s, "t2", def.id, &def.params), 50);
        assert_eq!(next(&s, "t2", def.id, &def.params), 51);
        let txn = s.begin_write_txn().expect("txn");
        reset_sequence_state_in_txn(&txn, "t1", &def).expect("reset");
        crate::recovery::commit_boundary::commit_and_finish(txn, (), |()| {
            s.sequence_allocator().evict_tenant("t1", def.id);
            crate::recovery::commit_boundary::PostCommitResult::Ok
        })
        .expect("commit");
        assert_eq!(next(&s, "t1", def.id, &def.params), 50);
        assert_eq!(next(&s, "t2", def.id, &def.params), 52);
    }
}
