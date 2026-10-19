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

use crate::storage::read_source::ReadSource;

use super::{
    get_table_schema_in_txn, list_tables_in_txn, validate_identifier, CatalogError, ColumnDefault,
    Result, Storage, MAX_LIST_TABLES,
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

/// 定義 blob の版（現行の書き込み版。版 1 は読み取りのみ受理する）。
const SEQUENCE_DEF_VERSION: u8 = 2;

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
fn sequence_key(name: &str) -> (&str, &str) {
    (DEFAULT_SEQUENCE_SCHEMA, name)
}

/// 定義 blob を符号化する（固定長・版バイト先頭。常に版 2 で書く）。`restart_value` は
/// 状態行を持たないテナントの初期 last_value（`is_called = false`）。
fn encode_sequence_def(id: u64, p: &SequenceParams, restart_value: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(SEQUENCE_DEF_LEN);
    out.push(SEQUENCE_DEF_VERSION);
    out.extend_from_slice(&id.to_le_bytes());
    out.push(TYPE_TAG_BIGINT);
    for v in [p.increment, p.min_value, p.max_value, p.start, 1_i64] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(u8::from(p.cycle));
    out.push(0); // owned_by: なし
    out.extend_from_slice(&restart_value.to_le_bytes());
    out
}

/// 定義 blob を復号する（版 1・版 2 を受理。版 1 は `restart_value = start`）。未知の版・タグ・
/// 長さ不一致・不変条件違反は `CorruptSchema`（fail-closed。detail は固定文言で格納バイト列の
/// 断片を含めない）。
fn decode_sequence_def(bytes: &[u8]) -> Result<SequenceDef> {
    let corrupt = || CatalogError::CorruptSchema("corrupt sequence definition".to_string());
    let version = bytes.first().copied();
    let expected_len = match version {
        Some(SEQUENCE_DEF_VERSION) => SEQUENCE_DEF_LEN,
        Some(SEQUENCE_DEF_VERSION_V1) => SEQUENCE_DEF_LEN_V1,
        _ => return Err(corrupt()),
    };
    if bytes.len() != expected_len {
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
    // cache は 1 固定、owned_by はなし固定（現時点では他の値を書かない）。
    if read_i64(42)? != 1 || bytes.get(CYCLE_OFFSET + 1) != Some(&0) {
        return Err(corrupt());
    }
    params.validate().map_err(|_| corrupt())?;
    let restart_value = if version == Some(SEQUENCE_DEF_VERSION) {
        read_i64(RESTART_OFFSET)?
    } else {
        params.start
    };
    if restart_value < params.min_value || restart_value > params.max_value {
        return Err(corrupt());
    }
    Ok(SequenceDef {
        id,
        params,
        restart_value,
    })
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

    /// `sequence_id` の現在値を全テナント分メモリから捨てる（`DROP SEQUENCE` の commit 成功後。
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
    validate_identifier(name)?;
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
    }
}

/// SEQUENCE `name` を DEFAULT に使う列が 1 つでもあるか（2BP01 判定）。表数は
/// [`MAX_LIST_TABLES`] で打ち切られる（`list_tables_in_txn` の上限）。
fn sequence_has_dependents_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<bool> {
    for table_name in list_tables_in_txn(write_txn)? {
        let schema = get_table_schema_in_txn(write_txn, &table_name)?;
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

/// 呼び出し元テナントの現在値を読むフック（RESTART なしの `ALTER SEQUENCE` の範囲検査用）。
/// 状態行が無ければ `None`（定義の `restart_value` を使う）。状態行の値の形式は #1690 が決めるため、
/// 行が存在する場合は形式が確定するまで `CorruptSchema` で fail-closed にする（#1690 の結線点）。
/// 他テナントの状態行は読まない（`22023` の有無から他テナントの採番の進みを観測させない）。
fn caller_current_value_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    id: u64,
) -> Result<Option<i64>> {
    match ReadSource::open_table(write_txn, SEQUENCE_STATE_TABLE) {
        Ok(t) => {
            if t.get((tenant_id, id))?.is_some() {
                return Err(CatalogError::CorruptSchema(
                    "sequence state format is not yet supported".to_string(),
                ));
            }
            Ok(None)
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Storage {
    /// SEQUENCE を作成する。判定順は同一 write txn 内で名前の衝突（`TableAlreadyExists`。
    /// 表・ビュー・索引・matview・SEQUENCE）→ 件数上限（`ViewLimitExceeded`）→ 保存。
    /// `sql::ddl::execute_create_sequence` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn create_sequence(&self, name: &str, params: &SequenceParams) -> Result<()> {
        validate_identifier(name)?;
        params.validate()?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        if super::table_exists_in_txn(&write_txn, name)?
            || super::view_exists_in_txn(&write_txn, name)?
            || super::index_name_exists_in_txn(&write_txn, name)?
            || super::materialized_view_exists_in_txn(&write_txn, name)?
            || sequence_exists_in_txn(&write_txn, name)?
        {
            return Err(CatalogError::TableAlreadyExists(name.to_string()));
        }
        let id = allocate_sequence_id_in_txn(&write_txn)?;
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            if table.len()? >= MAX_SEQUENCES as u64 {
                return Err(CatalogError::ViewLimitExceeded(
                    "too many sequences".to_string(),
                ));
            }
            table.insert(
                sequence_key(name),
                encode_sequence_def(id, params, params.start).as_slice(),
            )?;
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// SEQUENCE を削除する。名前が他種別なら `WrongObjectKind`、無ければ `TableNotFound`。
    /// 同一 write txn 内で依存 DEFAULT の検査（`DependentObjectsStillExist`）→ 定義の削除 →
    /// 全テナント分の状態行の削除を行う。
    pub(crate) fn drop_sequence(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let bytes = load_sequence_bytes_in_txn(&write_txn, name)?;
        let id = decode_sequence_def(&bytes)?.id;
        if sequence_has_dependents_in_txn(&write_txn, name)? {
            return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
        }
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            table.remove(sequence_key(name))?;
        }
        delete_sequence_state_in_txn(&write_txn, id)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        // commit 成功後にだけメモリ上の現在値を捨てる（内部 ID は再利用しないため、失敗時に
        // 残っても害はなく、成功時に残すと全テナント分が滞留する。設計記録 D3）。
        self.sequence_allocator().evict(id);
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
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let bytes = load_sequence_bytes_in_txn(&write_txn, name)?;
        let def = decode_sequence_def(&bytes)?;
        let caller_current = if alter.restart.is_none() {
            caller_current_value_in_txn(&write_txn, tenant_id, def.id)?
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
                encode_sequence_def(def.id, &params, restart_value).as_slice(),
            )?;
        }
        if alter.restart.is_some() {
            delete_sequence_state_in_txn(&write_txn, def.id)?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        self.sequence_allocator().evict(def.id);
        Ok(())
    }

    /// SEQUENCE の名前を変更する（`ALTER SEQUENCE ... RENAME TO`。Issue #1689）。定義キーだけを
    /// 付け替え、内部 ID と状態行は変えない（設計記録 D3）。判定順は旧名の解決 → 新名の衝突
    /// （5 種の relation。同名への変更も `TableAlreadyExists`）→ 依存 DEFAULT の有無
    /// （`DependentObjectsStillExist`。名前参照の DEFAULT が壊れるのを防ぐ fail-closed）。
    pub(crate) fn rename_sequence(&self, old: &str, new: &str) -> Result<()> {
        validate_identifier(old)?;
        validate_identifier(new)?;
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

    /// 定義をスナップショット読み取りで取得する（`(id, params)`。無ければ `None`）。
    #[cfg_attr(not(test), allow(dead_code))] // #1690 が使う。
    pub(crate) fn sequence_definition(&self, name: &str) -> Result<Option<(u64, SequenceParams)>> {
        Ok(self
            .sequence_definition_full(name)?
            .map(|d| (d.id, d.params)))
    }

    /// 定義を `restart_value` 込みで取得する（スナップショット読み取り。無ければ `None`）。
    #[cfg_attr(not(test), allow(dead_code))] // #1690 が使う。
    pub(crate) fn sequence_definition_full(&self, name: &str) -> Result<Option<SequenceDef>> {
        validate_identifier(name)?;
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
        let d = decode_sequence_def(&encode_sequence_def(7, &p, 10)).expect("decode");
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
        let mut v1 = encode_sequence_def(3, &p, 5);
        v1[0] = SEQUENCE_DEF_VERSION_V1;
        v1.truncate(SEQUENCE_DEF_LEN_V1);
        let d = decode_sequence_def(&v1).expect("v1 decode");
        assert_eq!((d.id, d.params, d.restart_value), (3, p, 5));
    }

    #[test]
    fn v2_restart_value_out_of_range_is_corrupt() {
        let good = encode_sequence_def(1, &params(), 1);
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
        let good = encode_sequence_def(1, &params(), 1);
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
