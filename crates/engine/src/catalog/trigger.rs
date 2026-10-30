//! TRIGGER カタログのスキーマと保存形式、および Storage 水準の読み書き（TABLE-45・TASK ポインタ、
//! Issue #1722。設計記録: `docs/design/trigger-catalog.md`、実行モデル: `docs/design/trigger-execution-model.md`
//! の D8・D12）。
//!
//! 役割: `CREATE [OR REPLACE] TRIGGER`／`DROP TRIGGER`（`sql::trigger_ddl`）が読み書きする定義の
//! 永続化層を担う。発火側（#1723 以降）はここの `list_triggers` で対象表の定義を引く想定で、本ファイル
//! 自体は発火しない。表・関数・SCHEMA のライフサイクル操作（`catalog.rs`・`catalog/function.rs`・
//! `catalog/namespace.rs`）は同じ write txn の中で本ファイルの `*_in_txn` を呼び、孤児定義を作らない。
//!
//! キー設計: `(table_key, scope, trigger_name)`。`scope` は検証済みの `tenant_id`。表キーを先頭に
//! 置くのは、表の DROP・RENAME が全テナント分の定義を前方一致の範囲走査だけで処理するため（共有 `public`
//! 表には複数テナントのトリガがありうる）。テナント境界は全 API が `scope` を必須引数に取り、範囲走査も
//! `scope` を必ず突き合わせることで守る。トリガ関数は同じ `scope` の関数に限る（UDF カタログがテナント単位
//! のため。`docs/design/trigger-catalog.md`）。
//!
//! 既存データの不変: [`TRIGGERS_TABLE`]／[`TRIGGER_ID_COUNTER_TABLE`] は最初の書き込みまで作らない
//! （遅延作成。読み取りは `TableDoesNotExist` を 0 件と扱い、ライフサイクル操作も存在しなければ何もしない）。
//!
//! fail-closed: 未知の版・タグ・ビット、切断、末尾の余り、上限超過、キーと値の不整合は固定文言の
//! `CorruptSchema` とし、blob の断片・tenant_id・本文をエラーに含めない。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;
use crate::storage::MAX_TENANT_ID_LEN;

use super::function::{arg_signature, get_function_in_txn, FunctionDef};
use super::{
    bump_table_generation_in_txn, index_name_exists_in_txn, materialized_view_exists_in_txn,
    require_table_schema_write_any_identity, validate_identifier, validate_relation_key,
    CatalogError, Result, Storage, CATALOG_TABLE, MAX_COLUMN_COUNT, MAX_VIEW_BODY_BYTES,
    VIEWS_TABLE,
};

/// TRIGGER 定義の専用カタログ表。キーは `(table_key, scope, trigger_name)`、値は定義 blob。
pub(crate) const TRIGGERS_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("triggers");

/// 内部 ID の採番カウンタ表（1 行だけ。再利用しない）。
pub(crate) const TRIGGER_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("trigger_id_counter");

/// [`TRIGGER_ID_COUNTER_TABLE`] の唯一のキー。
const TRIGGER_ID_COUNTER_KEY: &str = "next_id";

/// 定義 blob の版。
pub(crate) const TRIGGER_DEF_VERSION: u8 = 1;

/// 1 表・1 テナントあたりのトリガ件数の上限（実装既定値）。
pub(crate) const MAX_TRIGGERS_PER_TABLE: usize = 64;

/// `EXECUTE FUNCTION f(<引数>, ...)` の引数（`TG_ARGV`）の個数の上限。
pub(crate) const MAX_TRIGGER_ARGS: usize = 100;

/// `TG_ARGV` の 1 要素の最大バイト数。
pub(crate) const MAX_TRIGGER_ARG_BYTES: usize = 1024;

/// `WHEN` の正規化テキストの最大バイト数（ビュー本体と同じ上限）。
pub(crate) const MAX_TRIGGER_WHEN_BYTES: usize = MAX_VIEW_BODY_BYTES;

/// 関数キー（`#<id>.<name>` 形を含む）の最大バイト数。
const MAX_FUNCTION_KEY_BYTES: usize = 256;

/// 同名のトリガが既にある（`OR REPLACE` なし）ときの `Invalid` 文言。`sql::trigger_ddl` が `42710` へ写す。
pub(crate) const TRIGGER_ALREADY_EXISTS_MESSAGE: &str = "trigger already exists";

/// `EXECUTE FUNCTION` の関数が自 scope に無いときの `Invalid` 文言（`42883`）。
pub(crate) const TRIGGER_FUNCTION_NOT_FOUND_MESSAGE: &str = "trigger function does not exist";

/// 関数はあるが `RETURNS trigger` ではないときの `Invalid` 文言（`42P17`）。
pub(crate) const TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE: &str = "function is not a trigger function";

/// 件数上限超過の `ViewLimitExceeded` 文言（`54000`）。
pub(crate) const TRIGGER_LIMIT_MESSAGE: &str = "too many triggers";

/// 所有者の予約値（ロール機構が入るまで常に 0）。
const OWNER_TAG_NONE: u8 = 0;

/// 実行タイミング。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerTiming {
    Before,
    After,
}

impl TriggerTiming {
    fn tag(self) -> u8 {
        match self {
            Self::Before => 0,
            Self::After => 1,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Before),
            1 => Ok(Self::After),
            _ => Err(corrupt()),
        }
    }
}

/// 起動粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerLevel {
    Row,
    Statement,
}

impl TriggerLevel {
    fn tag(self) -> u8 {
        match self {
            Self::Row => 0,
            Self::Statement => 1,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Row),
            1 => Ok(Self::Statement),
            _ => Err(corrupt()),
        }
    }
}

/// イベントの集合（ビットマスク）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TriggerEvents(u8);

impl TriggerEvents {
    pub(crate) const INSERT: u8 = 0b0001;
    pub(crate) const UPDATE: u8 = 0b0010;
    pub(crate) const DELETE: u8 = 0b0100;
    pub(crate) const TRUNCATE: u8 = 0b1000;
    const ALL_BITS: u8 = Self::INSERT | Self::UPDATE | Self::DELETE | Self::TRUNCATE;

    /// ビットマスクから作る。0・未知のビットは `Invalid`。
    pub(crate) fn from_bits(bits: u8) -> Result<Self> {
        if bits == 0 || bits & !Self::ALL_BITS != 0 {
            return Err(CatalogError::Invalid("invalid trigger events".to_string()));
        }
        Ok(Self(bits))
    }

    pub(crate) fn bits(self) -> u8 {
        self.0
    }

    pub(crate) fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// TRIGGER の定義 1 件（対象表は格納キーの成分で、本構造体には持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerDef {
    /// 内部 ID（`create_trigger` が払い出す。呼び出し側の値は無視される）。
    pub(crate) id: u64,
    /// トリガ名（識別子。SCHEMA 修飾なし。表ごとに一意）。
    pub(crate) name: String,
    pub(crate) timing: TriggerTiming,
    pub(crate) events: TriggerEvents,
    pub(crate) level: TriggerLevel,
    /// `UPDATE OF <列>, ...` の列（空は列指定なし）。`UPDATE` を含むイベントでだけ非空にできる。
    pub(crate) update_of: Vec<String>,
    /// 実行する関数の正規化キー（引数なしで宣言された関数。UDF カタログの scope と同じテナントの定義）。
    pub(crate) function_key: String,
    /// `EXECUTE FUNCTION f('a', 1)` の引数（`TG_ARGV`。値として保存し SQL へ連結しない）。
    pub(crate) tg_args: Vec<String>,
    /// `WHEN` 条件の正規化テキスト。現状の SQL 表層は `WHEN` を受理しない（`0A000`）ため常に `None`
    /// だが、保存形式は将来のために持つ。
    pub(crate) when_sql: Option<String>,
}

/// 関数が `RETURNS trigger` を宣言しているか。トリガ関数の保存形式（#2228）が入るまで、どの関数も
/// トリガ関数ではない（`CREATE TRIGGER` は `42P17` で拒否される。fail-closed）。#2228 がトリガ関数の
/// 表現を `FunctionDef` に足したら、ここだけを結線する。
pub(crate) fn returns_trigger(_def: &FunctionDef) -> bool {
    false
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid trigger catalog entry".to_string())
}

fn invalid(msg: &str) -> CatalogError {
    CatalogError::Invalid(msg.to_string())
}

/// `scope`（`tenant_id`）の形状検査。
fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > usize::from(MAX_TENANT_ID_LEN) {
        return Err(invalid("invalid trigger scope"));
    }
    Ok(())
}

/// 関数キーの検証と ASCII 小文字化（UDF カタログの `normalize_key` と同じ規則）。
fn normalize_function_key(key: &str) -> Result<String> {
    validate_relation_key(key)?;
    if key.len() > MAX_FUNCTION_KEY_BYTES {
        return Err(invalid("invalid trigger function"));
    }
    Ok(key.to_ascii_lowercase())
}

/// 形状の検証（encode の前段と decode の後段で共有する。ID 以外のすべて）。
fn validate_shape(def: &TriggerDef) -> Result<()> {
    validate_identifier(&def.name)?;
    if def.function_key != normalize_function_key(&def.function_key)? {
        return Err(invalid("trigger function key is not normalized"));
    }
    if def.level == TriggerLevel::Row && def.events.contains(TriggerEvents::TRUNCATE) {
        return Err(invalid("TRUNCATE triggers must be statement-level"));
    }
    if !def.update_of.is_empty() {
        if !def.events.contains(TriggerEvents::UPDATE) {
            return Err(invalid("UPDATE OF requires an UPDATE event"));
        }
        if def.update_of.len() > MAX_COLUMN_COUNT {
            return Err(invalid("too many UPDATE OF columns"));
        }
        for (i, col) in def.update_of.iter().enumerate() {
            validate_identifier(col)?;
            if def.update_of.iter().take(i).any(|c| c == col) {
                return Err(invalid("duplicate UPDATE OF column"));
            }
        }
    }
    if def.tg_args.len() > MAX_TRIGGER_ARGS {
        return Err(invalid("too many trigger arguments"));
    }
    if def.tg_args.iter().any(|a| a.len() > MAX_TRIGGER_ARG_BYTES) {
        return Err(invalid("trigger argument is too long"));
    }
    if def
        .when_sql
        .as_ref()
        .is_some_and(|w| w.is_empty() || w.len() > MAX_TRIGGER_WHEN_BYTES)
    {
        return Err(invalid("invalid trigger WHEN condition"));
    }
    Ok(())
}

fn push_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u32::try_from(s.len()).map_err(|_| invalid("trigger field is too long"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn push_list(out: &mut Vec<u8>, items: &[String]) -> Result<()> {
    let len = u32::try_from(items.len()).map_err(|_| invalid("trigger list is too long"))?;
    out.extend_from_slice(&len.to_le_bytes());
    for item in items {
        push_str(out, item)?;
    }
    Ok(())
}

/// 定義を blob へ符号化する（版 1。整数はリトルエンディアン、文字列は `u32` 長 + UTF-8）。
/// 並び: `version / id / owner / timing / events / level / update_of / function_key / tg_args / when`。
pub(crate) fn encode_trigger_def(def: &TriggerDef) -> Result<Vec<u8>> {
    validate_shape(def)?;
    let mut out = vec![TRIGGER_DEF_VERSION];
    out.extend_from_slice(&def.id.to_le_bytes());
    out.push(OWNER_TAG_NONE);
    out.push(def.timing.tag());
    out.push(def.events.bits());
    out.push(def.level.tag());
    push_list(&mut out, &def.update_of)?;
    push_str(&mut out, &def.function_key)?;
    push_list(&mut out, &def.tg_args)?;
    match &def.when_sql {
        Some(w) => {
            out.push(1);
            push_str(&mut out, w)?;
        }
        None => out.push(0),
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

    /// 件数を上限検証してから読む（確保の前に検査する）。
    fn list(&mut self, max_items: usize, max_bytes: usize) -> Result<Vec<String>> {
        let n = usize::try_from(self.u32()?).map_err(|_| corrupt())?;
        if n > max_items {
            return Err(corrupt());
        }
        let mut out = Vec::with_capacity(n.min(16));
        for _ in 0..n {
            out.push(self.string(max_bytes)?);
        }
        Ok(out)
    }
}

/// 定義 blob を復号する。`name` は格納キーの成分で、値の検証（識別子形状）に使う。あらゆる不整合は固定
/// 文言の `CorruptSchema`（格納バイト列の断片を含めない）。
pub(crate) fn decode_trigger_def(name: &str, bytes: &[u8]) -> Result<TriggerDef> {
    let mut r = Reader { bytes, offset: 0 };
    if r.u8()? != TRIGGER_DEF_VERSION {
        return Err(corrupt());
    }
    let id = r.u64()?;
    if r.u8()? != OWNER_TAG_NONE {
        return Err(corrupt());
    }
    let timing = TriggerTiming::from_tag(r.u8()?)?;
    let events = TriggerEvents::from_bits(r.u8()?).map_err(|_| corrupt())?;
    let level = TriggerLevel::from_tag(r.u8()?)?;
    let update_of = r.list(MAX_COLUMN_COUNT, MAX_NAME_FIELD_BYTES)?;
    let function_key = r.string(MAX_FUNCTION_KEY_BYTES)?;
    let tg_args = r.list(MAX_TRIGGER_ARGS, MAX_TRIGGER_ARG_BYTES)?;
    let when_sql = match r.u8()? {
        0 => None,
        1 => Some(r.string(MAX_TRIGGER_WHEN_BYTES)?),
        _ => return Err(corrupt()),
    };
    if r.offset != bytes.len() {
        return Err(corrupt());
    }
    let def = TriggerDef {
        id,
        name: name.to_string(),
        timing,
        events,
        level,
        update_of,
        function_key,
        tg_args,
        when_sql,
    };
    validate_shape(&def).map_err(|_| corrupt())?;
    Ok(def)
}

/// 識別子 1 つの最大バイト数（列名・トリガ名）。
const MAX_NAME_FIELD_BYTES: usize = 128;

/// 1 件を引く（表が無ければ `None`）。
fn get_in_txn<R: ReadSource>(
    src: &R,
    table_key: &str,
    scope: &str,
    name: &str,
) -> Result<Option<TriggerDef>> {
    let table = match src.open_table(TRIGGERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((table_key, scope, name))?;
    match found {
        Some(v) => decode_trigger_def(name, v.value()).map(Some),
        None => Ok(None),
    }
}

/// `(table_key, scope)` のトリガを名前の昇順で列挙する。件数が [`MAX_TRIGGERS_PER_TABLE`] を超えたら
/// `CorruptSchema`（破損時の無制限確保の防止）。
fn list_in_txn<R: ReadSource>(src: &R, table_key: &str, scope: &str) -> Result<Vec<TriggerDef>> {
    let table = match src.open_table(TRIGGERS_TABLE) {
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
        if out.len() >= MAX_TRIGGERS_PER_TABLE {
            return Err(corrupt());
        }
        out.push(decode_trigger_def(name, v.value())?);
    }
    Ok(out)
}

/// 書き込み txn の中で `scope` の表 `table_key` のトリガを名前の昇順で引く（発火側〔Issue #1723〕が
/// 文の開始時に同じ txn から読む入口。存在しないテーブルは作成せず空を返す。他テナントの定義は構造上引けない）。
pub(crate) fn list_triggers_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    table_key: &str,
) -> Result<Vec<TriggerDef>> {
    list_in_txn(write_txn, table_key, scope)
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(TRIGGER_ID_COUNTER_TABLE)?;
    let current = counter
        .get(TRIGGER_ID_COUNTER_KEY)?
        .map(|v| v.value())
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(TRIGGER_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// 全テナントの `table_key` 配下のキーを最大 `limit` 件集める（バッチ走査。無制限確保をしない）。
fn collect_table_keys<R: ReadSource>(
    src: &R,
    table_key: &str,
    limit: usize,
) -> Result<Vec<(String, String, Vec<u8>)>> {
    let table = match src.open_table(TRIGGERS_TABLE) {
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

/// 表 `table_key` の全テナント分のトリガを削除する（`DROP TABLE`・`DROP SCHEMA ... CASCADE` の表削除
/// から同じ write txn で呼ぶ。同名の表を再作成しても旧定義が復活しないようにする）。表が無ければ何もしない。
pub(crate) fn remove_triggers_for_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table_key: &str,
) -> Result<()> {
    loop {
        let batch = collect_table_keys(write_txn, table_key, SWEEP_BATCH)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(TRIGGERS_TABLE)?;
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
    remove_triggers_for_table_in_txn(write_txn, new_key)?;
    loop {
        let batch = collect_table_keys(write_txn, old_key, SWEEP_BATCH)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(TRIGGERS_TABLE)?;
        for (scope, name, blob) in &batch {
            table.remove((old_key, scope.as_str(), name.as_str()))?;
            table.insert((new_key, scope.as_str(), name.as_str()), blob.as_slice())?;
        }
    }
}

/// `scope` の関数 `function_key` を実行するトリガが 1 件でもあるか（`DROP FUNCTION` の依存検査用。
/// 全体を 1 回ストリーム走査する。表が無ければ `false`）。
pub(crate) fn triggers_referencing_function_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    function_key: &str,
) -> Result<bool> {
    let table = match src.open_table(TRIGGERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in table.iter()? {
        let (k, v) = entry?;
        let (_, s, name) = k.value();
        if s != scope {
            continue;
        }
        if decode_trigger_def(name, v.value())?.function_key == function_key {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `scope` の関数のうちキーが `prefix`（`#<id>.`）で始まるものを実行するトリガをすべて削除する
/// （`DROP SCHEMA ... CASCADE` が配下の関数を消す直前に呼ぶ。`public` 表のトリガが消えた関数を参照する
/// 孤児を作らない）。
pub(crate) fn remove_triggers_with_function_prefix_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    prefix: &str,
) -> Result<()> {
    loop {
        let mut targets: Vec<(String, String)> = Vec::new();
        {
            let table = match ReadSource::open_table(write_txn, TRIGGERS_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            for entry in table.iter()? {
                let (k, v) = entry?;
                let (t, s, name) = k.value();
                if s != scope {
                    continue;
                }
                if decode_trigger_def(name, v.value())?
                    .function_key
                    .starts_with(prefix)
                {
                    targets.push((t.to_string(), name.to_string()));
                    if targets.len() >= SWEEP_BATCH {
                        break;
                    }
                }
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(TRIGGERS_TABLE)?;
        for (t, name) in &targets {
            table.remove((t.as_str(), scope, name.as_str()))?;
        }
    }
}

/// `scope` の関数 `function_key` を実行するトリガをすべて削除する（`DROP FUNCTION ... CASCADE`
/// が関数の削除と同じ write txn で呼ぶ。SQL-66 (3)・Issue #1917）。キーの完全一致・同じ `scope` の
/// トリガだけが対象で、他テナントのトリガには触れない。影響した表の世代は `drop_trigger` と同様に進める
/// （`USING PLAN` の世代照合がトリガの消失を見逃さないため）。表が無ければ何もしない。
pub(crate) fn remove_triggers_referencing_function_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    function_key: &str,
) -> Result<()> {
    let mut touched_tables: Vec<String> = Vec::new();
    loop {
        let mut targets: Vec<(String, String)> = Vec::new();
        {
            let table = match ReadSource::open_table(write_txn, TRIGGERS_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            for entry in table.iter()? {
                let (k, v) = entry?;
                let (t, s, name) = k.value();
                if s != scope {
                    continue;
                }
                if decode_trigger_def(name, v.value())?.function_key == function_key {
                    targets.push((t.to_string(), name.to_string()));
                    if targets.len() >= SWEEP_BATCH {
                        break;
                    }
                }
            }
        }
        if targets.is_empty() {
            break;
        }
        let mut table = write_txn.open_table(TRIGGERS_TABLE)?;
        for (t, name) in &targets {
            table.remove((t.as_str(), scope, name.as_str()))?;
            if !touched_tables.iter().any(|x| x == t) {
                touched_tables.push(t.clone());
            }
        }
    }
    for t in &touched_tables {
        bump_table_generation_in_txn(write_txn, t)?;
    }
    Ok(())
}

/// 表 `table_key` のいずれかのテナントのトリガが `UPDATE OF` で列 `column` を参照しているか
/// （`RENAME COLUMN`・`DROP COLUMN`・`ALTER COLUMN TYPE` の暫定ガード用。表が無ければ `false`）。
pub(crate) fn triggers_referencing_column_in_txn<R: ReadSource>(
    src: &R,
    table_key: &str,
    column: &str,
) -> Result<bool> {
    let table = match src.open_table(TRIGGERS_TABLE) {
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
        if decode_trigger_def(name, v.value())?
            .update_of
            .iter()
            .any(|c| c == column)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Storage {
    /// `scope` のテナントとして、表 `table_key` にトリガを作り、内部 ID を返す。単一 write txn・1 回の
    /// commit で行い、判定順は次のとおり（すべて commit 前に返るため副作用ゼロ）。
    ///
    /// 形状 → SCHEMA の現存 → 対象の種別（ビュー・索引・SEQUENCE・マテリアライズドビューは
    /// `WrongObjectKind`）→ 表の存在（`TableNotFound`）→ `UPDATE OF` の列（`ColumnNotFound`）→ 関数
    /// （`Invalid(TRIGGER_FUNCTION_NOT_FOUND_MESSAGE)`／`Invalid(TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE)`）→
    /// 同名の既存（`or_replace` でなければ `Invalid(TRIGGER_ALREADY_EXISTS_MESSAGE)`）→ 件数上限
    /// （`ViewLimitExceeded`）。`or_replace` の置き換えは既存の内部 ID を引き継ぐ。成功時は対象表の世代を進める。
    /// `def.id` は無視される。呼び出し元は `sql::trigger_ddl`。
    pub(crate) fn create_trigger(
        &self,
        scope: &str,
        table_key: &str,
        def: &TriggerDef,
        or_replace: bool,
    ) -> Result<u64> {
        self.create_trigger_inner(
            scope,
            table_key,
            def,
            or_replace,
            MAX_TRIGGERS_PER_TABLE,
            returns_trigger,
        )
    }

    /// [`Self::create_trigger`] の本体。件数上限とトリガ関数の判定を引数にし、テストが小さい上限と
    /// 「`RETURNS trigger` の関数がある」状況（保存形式は #2228）を注入できるようにする。
    fn create_trigger_inner(
        &self,
        scope: &str,
        table_key: &str,
        def: &TriggerDef,
        or_replace: bool,
        max_triggers: usize,
        is_trigger_fn: fn(&FunctionDef) -> bool,
    ) -> Result<u64> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        let mut stored = def.clone();
        stored.function_key = normalize_function_key(&def.function_key)?;
        stored.id = 1;
        validate_shape(&stored)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        // 名前解決後に `DROP SCHEMA` が完了しても、削除済み ID 配下の表へ定義を残さない。
        super::namespace::ensure_tenant_key_schema_alive_in_txn(&*write_txn, scope, table_key)?;
        let in_views = match ReadSource::open_table(&*write_txn, VIEWS_TABLE) {
            Ok(t) => t.get(table_key)?.is_some(),
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(e) => return Err(e.into()),
        };
        if in_views
            || index_name_exists_in_txn(&write_txn, table_key)?
            || materialized_view_exists_in_txn(&write_txn, table_key)?
            || super::sequence::sequence_exists_in_txn(&write_txn, table_key)?
        {
            return Err(CatalogError::WrongObjectKind(table_key.to_string()));
        }
        let table_exists = match ReadSource::open_table(&*write_txn, CATALOG_TABLE) {
            Ok(t) => t.get(table_key)?.is_some(),
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(e) => return Err(e.into()),
        };
        if !table_exists {
            return Err(CatalogError::TableNotFound(table_key.to_string()));
        }
        if !stored.update_of.is_empty() {
            let schema = require_table_schema_write_any_identity(&write_txn, table_key)?;
            for col in &stored.update_of {
                if !schema.columns.iter().any(|c| &c.name == col) {
                    return Err(CatalogError::ColumnNotFound(col.clone()));
                }
            }
        }
        // トリガ関数は同じ scope の引数なし関数に限る（設計記録 §関数の解決）。
        let signature = arg_signature(&[])?;
        match get_function_in_txn(&*write_txn, scope, &stored.function_key, &signature)? {
            None => return Err(invalid(TRIGGER_FUNCTION_NOT_FOUND_MESSAGE)),
            Some(f) if !is_trigger_fn(&f) => {
                return Err(invalid(TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE));
            }
            Some(_) => {}
        }
        let existing = get_in_txn(&*write_txn, table_key, scope, &stored.name)?;
        match existing {
            Some(prev) if or_replace => stored.id = prev.id,
            Some(_) => return Err(invalid(TRIGGER_ALREADY_EXISTS_MESSAGE)),
            None => {
                if list_in_txn(&*write_txn, table_key, scope)?.len() >= max_triggers {
                    return Err(CatalogError::ViewLimitExceeded(
                        TRIGGER_LIMIT_MESSAGE.to_string(),
                    ));
                }
                stored.id = allocate_id_in_txn(&write_txn)?;
            }
        }
        let blob = encode_trigger_def(&stored)?;
        {
            let mut table = write_txn.open_table(TRIGGERS_TABLE)?;
            table.insert((table_key, scope, stored.name.as_str()), blob.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_key)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(stored.id)
    }

    /// テスト専用: 「どの関数もトリガ関数」と見なして定義を作る（トリガ関数の保存形式〔#2228〕が未整備で、
    /// SQL の `CREATE TRIGGER` が `42P17` で止まるため、発火側のテストが定義を注入する入口）。
    #[cfg(test)]
    #[allow(dead_code)] // 発火側のテスト（`core/trigger_insert_tests.rs`）が使う。
    pub(crate) fn create_trigger_for_test(
        &self,
        scope: &str,
        table_key: &str,
        def: &TriggerDef,
    ) -> Result<u64> {
        self.create_trigger_inner(scope, table_key, def, false, MAX_TRIGGERS_PER_TABLE, |_| {
            true
        })
    }

    /// `scope` のトリガ `name` を削除し、存在して削除したら `true`。表が無ければ `TableNotFound`、トリガが
    /// 無ければ（他テナントにしか無い場合を含む）`Ok(false)`。削除と対象表の世代更新は同一 write txn。
    pub(crate) fn drop_trigger(&self, scope: &str, table_key: &str, name: &str) -> Result<bool> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let table_exists = match ReadSource::open_table(&*write_txn, CATALOG_TABLE) {
            Ok(t) => t.get(table_key)?.is_some(),
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(e) => return Err(e.into()),
        };
        if !table_exists {
            return Err(CatalogError::TableNotFound(table_key.to_string()));
        }
        if get_in_txn(&*write_txn, table_key, scope, name)?.is_none() {
            return Ok(false);
        }
        {
            let mut table = write_txn.open_table(TRIGGERS_TABLE)?;
            table.remove((table_key, scope, name))?;
        }
        bump_table_generation_in_txn(&write_txn, table_key)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(true)
    }

    /// `scope` の表 `table_key` のトリガを名前の昇順で返す（読み取り専用。発火側〔#1723〕が引く入口）。
    /// 他テナントの定義は構造上引けない。
    pub(crate) fn list_triggers(&self, scope: &str, table_key: &str) -> Result<Vec<TriggerDef>> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        let rt = self.db().begin_read()?;
        list_in_txn(&rt, table_key, scope)
    }
}

#[cfg(test)]
mod tests;
