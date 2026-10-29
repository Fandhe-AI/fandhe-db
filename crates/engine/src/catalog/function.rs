//! UDF（ユーザー定義関数）カタログのスキーマと保存形式、および Storage 水準の読み書き
//! （SQL-66 のポインタ・TABLE-27・RLS-12・TASK-309 の関連。Issue #1919。親: #1918。設計記録:
//! `docs/design/udf-catalog.md`）。
//!
//! 役割: 現状セッション単位の `sql::udf_call::UdfRegistry` にしか無い関数定義を redb へ永続化する
//! ための土台（テーブル定義・定義 blob の版付き符号化・crate 内部の保存／読み出し API）を担う。
//! `CREATE FUNCTION`／`DROP FUNCTION` からの書き込み（#1920）は `sql::udf_ddl` が呼んで結線済み。
//! セッション開始時のロード（#1921）、上限と権限の設定（#1922）は後続 Issue が結線する（読み出し API の
//! 一部は当面テストからのみ呼ばれるため、該当項目に個別に `dead_code` を許可している）。
//!
//! 既存データの不変: [`FUNCTIONS_TABLE`]／[`FUNCTION_ID_COUNTER_TABLE`] は最初の書き込みまで作らない
//! （遅延作成。読み取りは `TableDoesNotExist` を 0 件と扱う）ため、UDF を使わない DB や本変更より
//! 前の DB のファイル構成・既存カタログの値は変わらない。
//!
//! テナント境界（P0）: キーの先頭を `scope`（本 Issue では検証済みの `tenant_id` のみ）にし、他
//! テナントの定義を構造上引けないようにする。存在しない名前と他テナントにしか無い名前は同じ経路・
//! 同じ結果になる。エラー文言は固定で、blob の断片・tenant_id・本体の原文を含めない。
//!
//! fail-closed: 未知の版・タグ・flag ビット、切断、末尾の余り、上限超過、キーと値のシグネチャ
//! 不一致は `CorruptSchema` とし、decode で黙って補正・既定化しない。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;
use crate::storage::MAX_TENANT_ID_LEN;

use super::{
    validate_catalog_param, validate_identifier, validate_relation_key, CatalogError, ColumnType,
    Result, Storage, MAX_VIEW_BODY_BYTES,
};

/// UDF 定義の専用カタログ表。キーは `(scope, function_key, arg_signature)`、値は定義 blob。
pub(crate) const FUNCTIONS_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("functions");

/// UDF 内部 ID カウンタ（固定キー 1 行の単調カウンタ。1 始まりで再利用しない）。
pub(crate) const FUNCTION_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("function_id_counter");

/// [`FUNCTION_ID_COUNTER_TABLE`] の唯一のキー。
const FUNCTION_ID_COUNTER_KEY: &str = "next_id";

/// 定義 blob の版。形式を変える場合は新しい版を足し、旧版の decode を残す。
pub(crate) const FUNCTION_DEF_VERSION: u8 = 1;

/// 引数数の格納形式上の上限（SQL-66 (5) の引数の数の上限に合わせる）。
pub(crate) const MAX_FUNCTION_PARAMS: usize = 100;

/// 関数本体（原文）の最大バイト数（ビュー本体と同じ上限）。
pub(crate) const MAX_FUNCTION_BODY_BYTES: usize = MAX_VIEW_BODY_BYTES;

/// スコープあたりの関数定義数の既定上限（SQL-66 (5) の既定値）。
pub(crate) const MAX_FUNCTIONS: usize = 64;

/// 同名の定義が既にあるときの `Invalid` の固定文言。`sql::udf_ddl` が 42723 へ写す判定に使う
/// （文言の比較を 1 箇所に集約する。前例: `ROW_IDENTITY_UNSUPPORTED_MESSAGE`）。
pub(crate) const FUNCTION_ALREADY_EXISTS_MESSAGE: &str = "function already exists";

/// 名前指定の削除が複数の多重定義に当たった場合の `Invalid` の固定文言（防御的。42725 へ写す）。
pub(crate) const FUNCTION_AMBIGUOUS_MESSAGE: &str = "function name is not unique";

/// 型タグ・param 文字列の最大バイト数（確保前の上限検査用）。
const MAX_TYPE_FIELD_BYTES: usize = 128;

/// 識別子（関数キー・引数名）文字列の最大バイト数。`#<u64>.` 接頭辞を含めても収まる値。
const MAX_NAME_FIELD_BYTES: usize = 96;

/// 所有者の予約領域。SQL-40（ロール）が入るまでは常に 0。
const OWNER_TAG_NONE: u8 = 0;

/// 型注釈なし（SQL-9 の宣言的な形）を表すシグネチャのトークン。
const UNTYPED_TOKEN: &str = "?";

/// シグネチャの区切り。識別子にも catalog param の文字集合にも現れない。
const SIGNATURE_SEPARATOR: char = ';';

const FLAG_STRICT: u8 = 0b01;
const FLAG_SECURITY_DEFINER: u8 = 0b10;
const FLAG_MASK: u8 = FLAG_STRICT | FLAG_SECURITY_DEFINER;

/// 関数の実装言語。将来の言語（WASM 等）は新しいタグ値で足す（既存値は変えない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FunctionLanguage {
    /// 型注釈なしの宣言的な形（SQL-9）。
    Declarative,
    /// `LANGUAGE sql`。
    Sql,
    /// `LANGUAGE plpgsql`。
    Plpgsql,
}

impl FunctionLanguage {
    fn tag(self) -> u8 {
        match self {
            Self::Declarative => 1,
            Self::Sql => 2,
            Self::Plpgsql => 3,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::Declarative),
            2 => Ok(Self::Sql),
            3 => Ok(Self::Plpgsql),
            _ => Err(corrupt()),
        }
    }
}

/// 揮発性（SQL-67 (3)。既定は `Volatile`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Volatility {
    Immutable,
    Stable,
    Volatile,
}

impl Volatility {
    fn tag(self) -> u8 {
        match self {
            Self::Immutable => 1,
            Self::Stable => 2,
            Self::Volatile => 3,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::Immutable),
            2 => Ok(Self::Stable),
            3 => Ok(Self::Volatile),
            _ => Err(corrupt()),
        }
    }
}

/// `SECURITY INVOKER`（既定）／`SECURITY DEFINER`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Security {
    Invoker,
    Definer,
}

/// `PARALLEL` 属性（受理と保存のみで結果を変えない属性）。既定は `Unsafe`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parallel {
    Unsafe,
    Restricted,
    Safe,
}

impl Parallel {
    fn tag(self) -> u8 {
        match self {
            Self::Unsafe => 1,
            Self::Restricted => 2,
            Self::Safe => 3,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::Unsafe),
            2 => Ok(Self::Restricted),
            3 => Ok(Self::Safe),
            _ => Err(corrupt()),
        }
    }
}

/// 関数の属性句の保存値。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionAttributes {
    pub(crate) volatility: Volatility,
    /// `true` = `STRICT`、`false` = `CALLED ON NULL INPUT`（既定）。
    pub(crate) strict: bool,
    pub(crate) security: Security,
    pub(crate) parallel: Parallel,
    /// `COST`。有限かつ 0 より大きい値に限る。`None` は既定。
    pub(crate) cost: Option<f64>,
    /// `ROWS`。有限かつ 0 より大きい値に限る。`None` は既定。
    pub(crate) rows: Option<f64>,
}

impl Default for FunctionAttributes {
    fn default() -> Self {
        Self {
            volatility: Volatility::Volatile,
            strict: false,
            security: Security::Invoker,
            parallel: Parallel::Unsafe,
            cost: None,
            rows: None,
        }
    }
}

/// 型の保存値。[`ColumnType::catalog_fields`] の `(tag, param)` 1 対をそのまま持つ。ENUM の語彙は
/// `ENUM_TYPES_TABLE` 側が正のため、ここでは `ColumnType` を直接持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FunctionType {
    pub(crate) tag: String,
    pub(crate) param: String,
}

/// ENUM 型名の解決を拒否するコールバック（本 Issue では ENUM を受理しない。encode・decode 共通）。
fn reject_enum(_name: &str) -> Result<std::sync::Arc<super::EnumTypeDef>> {
    Err(CatalogError::Invalid(
        "enum type is not supported in function signatures".to_string(),
    ))
}

impl FunctionType {
    /// `ColumnType` から保存値を作る。ENUM（および要素が ENUM の配列）は拒否する。
    pub(crate) fn from_column_type(ty: &ColumnType) -> Result<Self> {
        let (tag, param) = ty.catalog_fields();
        let out = Self {
            tag: tag.to_string(),
            param,
        };
        out.validate()?;
        Ok(out)
    }

    /// 保存値を `ColumnType` へ復元する（#1921 が登録時に使う入口）。
    pub(crate) fn to_column_type(&self) -> Result<ColumnType> {
        validate_catalog_param(&self.param)?;
        ColumnType::from_catalog_fields(&self.tag, &self.param, &mut reject_enum)
    }

    /// タグ・param の文法を検証する（未知タグ・不正 param・ENUM は `Invalid`）。
    fn validate(&self) -> Result<()> {
        // 復号側の上限（MAX_TYPE_FIELD_BYTES）と同じ長さを保存前に強制し、読み出せない定義を拒否する。
        if self.tag.len() > MAX_TYPE_FIELD_BYTES || self.param.len() > MAX_TYPE_FIELD_BYTES {
            return Err(CatalogError::Invalid(
                "function type field too long".to_string(),
            ));
        }
        self.to_column_type().map(|_| ())
    }

    /// 型の同一性トークン（typmod・次元・最大長を落とした形）。検証済みの値にのみ呼ぶ。
    pub(crate) fn identity_token(&self) -> String {
        if self.tag == "array" {
            let elem = self.param.split(',').next().unwrap_or("");
            format!("array:{elem}")
        } else {
            self.tag.clone()
        }
    }
}

/// 関数の仮引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FunctionParam {
    pub(crate) name: Option<String>,
    /// `None` = 型注釈なし（SQL-9 の宣言的な形）。
    pub(crate) ty: Option<FunctionType>,
}

/// 関数定義の保存値。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionDef {
    /// 内部 ID（1 以上・再利用しない）。[`Storage::create_function`] が払い出す。
    pub(crate) id: u64,
    /// relation キー形（`public` は裸の名前、SCHEMA 配下は `#<schema_id>.<name>`）。ASCII 小文字。
    pub(crate) function_key: String,
    pub(crate) params: Vec<FunctionParam>,
    /// `None` = 型注釈なし。
    pub(crate) return_type: Option<FunctionType>,
    pub(crate) language: FunctionLanguage,
    /// 本体の原文（ドル引用を含めそのまま）。再解析は呼び出し側が許可リストパーサーで行う。
    pub(crate) body_source: String,
    pub(crate) attrs: FunctionAttributes,
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid function definition".to_string())
}

/// 引数型の並びの正準文字列（多重定義の識別子。SQL-66 (2)）。typmod・配列の最大長と次元は
/// 区別せず、型注釈なしは `?`、引数 0 個は空文字列。ENUM・未知の型は `Invalid`。
pub(crate) fn arg_signature(params: &[FunctionParam]) -> Result<String> {
    let mut out = String::new();
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(SIGNATURE_SEPARATOR);
        }
        match &p.ty {
            None => out.push_str(UNTYPED_TOKEN),
            Some(t) => {
                t.validate()?;
                out.push_str(&t.identity_token());
            }
        }
    }
    Ok(out)
}

/// 関数キーを検証して ASCII 小文字へ正規化する。
fn normalize_key(key: &str) -> Result<String> {
    validate_relation_key(key)?;
    Ok(key.to_ascii_lowercase())
}

/// 形状の検証（encode の前段と decode の後段で共有する。ID 以外のすべて）。
fn validate_shape(def: &FunctionDef) -> Result<()> {
    if def.function_key != normalize_key(&def.function_key)? {
        return Err(CatalogError::Invalid(
            "function key is not normalized".to_string(),
        ));
    }
    if def.params.len() > MAX_FUNCTION_PARAMS {
        return Err(CatalogError::Invalid(
            "too many function parameters".to_string(),
        ));
    }
    if def.body_source.len() > MAX_FUNCTION_BODY_BYTES {
        return Err(CatalogError::Invalid("function body too large".to_string()));
    }
    let mut seen: Vec<String> = Vec::new();
    for p in &def.params {
        if let Some(name) = &p.name {
            validate_identifier(name)?;
            let lower = name.to_ascii_lowercase();
            if seen.contains(&lower) {
                return Err(CatalogError::Invalid(
                    "duplicate function parameter name".to_string(),
                ));
            }
            seen.push(lower);
        }
        if let Some(t) = &p.ty {
            t.validate()?;
        }
    }
    if let Some(t) = &def.return_type {
        t.validate()?;
    }
    for v in [def.attrs.cost, def.attrs.rows].into_iter().flatten() {
        if !v.is_finite() || v <= 0.0 {
            return Err(CatalogError::Invalid(
                "function cost/rows must be positive and finite".to_string(),
            ));
        }
    }
    Ok(())
}

fn push_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u32::try_from(s.len())
        .map_err(|_| CatalogError::Invalid("function part too long".to_string()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn push_type(out: &mut Vec<u8>, ty: &Option<FunctionType>) -> Result<()> {
    match ty {
        None => out.push(0),
        Some(t) => {
            out.push(1);
            push_str(out, &t.tag)?;
            push_str(out, &t.param)?;
        }
    }
    Ok(())
}

fn push_opt_f64(out: &mut Vec<u8>, v: Option<f64>) {
    match v {
        None => out.push(0),
        Some(x) => {
            out.push(1);
            out.extend_from_slice(&x.to_bits().to_le_bytes());
        }
    }
}

/// 定義 blob（v1）を符号化する。形状・上限を検証し、違反は `Invalid`。
/// 配置: 版 / id(u64) / owner(予約) / language / volatility / flags / parallel / cost / rows /
/// 引数 / 戻り型 / 本体。文字列は u32 長 + UTF-8、数値はリトルエンディアン。
pub(crate) fn encode_function_def(def: &FunctionDef) -> Result<Vec<u8>> {
    if def.id == 0 {
        return Err(CatalogError::Invalid(
            "function id must be >= 1".to_string(),
        ));
    }
    validate_shape(def)?;
    let mut out = Vec::new();
    out.try_reserve_exact(64 + def.body_source.len())
        .map_err(|_| CatalogError::Invalid("function definition too large".to_string()))?;
    out.push(FUNCTION_DEF_VERSION);
    out.extend_from_slice(&def.id.to_le_bytes());
    out.push(OWNER_TAG_NONE);
    out.push(def.language.tag());
    out.push(def.attrs.volatility.tag());
    let mut flags = 0u8;
    if def.attrs.strict {
        flags |= FLAG_STRICT;
    }
    if def.attrs.security == Security::Definer {
        flags |= FLAG_SECURITY_DEFINER;
    }
    out.push(flags);
    out.push(def.attrs.parallel.tag());
    push_opt_f64(&mut out, def.attrs.cost);
    push_opt_f64(&mut out, def.attrs.rows);
    let n = u32::try_from(def.params.len())
        .map_err(|_| CatalogError::Invalid("too many function parameters".to_string()))?;
    out.extend_from_slice(&n.to_le_bytes());
    for p in &def.params {
        match &p.name {
            None => out.push(0),
            Some(name) => {
                out.push(1);
                push_str(&mut out, name)?;
            }
        }
        push_type(&mut out, &p.ty)?;
    }
    push_type(&mut out, &def.return_type)?;
    push_str(&mut out, &def.body_source)?;
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

    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(corrupt()),
        }
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

    fn opt_f64(&mut self) -> Result<Option<f64>> {
        if self.bool()? {
            Ok(Some(f64::from_bits(self.u64()?)))
        } else {
            Ok(None)
        }
    }

    fn opt_type(&mut self) -> Result<Option<FunctionType>> {
        if !self.bool()? {
            return Ok(None);
        }
        Ok(Some(FunctionType {
            tag: self.string(MAX_TYPE_FIELD_BYTES)?,
            param: self.string(MAX_TYPE_FIELD_BYTES)?,
        }))
    }
}

/// 定義 blob を復号する。`function_key`・`arg_signature` は格納キーの成分で、値から再計算した
/// シグネチャとの一致まで検査する（キーと値のずれによる誤解決を防ぐ）。あらゆる不整合は
/// 固定文言の `CorruptSchema`（格納バイト列の断片を含めない）。
pub(crate) fn decode_function_def(
    function_key: &str,
    arg_signature_key: &str,
    bytes: &[u8],
) -> Result<FunctionDef> {
    decode_inner(function_key, arg_signature_key, bytes).map_err(|_| corrupt())
}

fn decode_inner(function_key: &str, arg_signature_key: &str, bytes: &[u8]) -> Result<FunctionDef> {
    let mut r = Reader { bytes, offset: 0 };
    if r.u8()? != FUNCTION_DEF_VERSION {
        return Err(corrupt());
    }
    let id = r.u64()?;
    if id == 0 || r.u8()? != OWNER_TAG_NONE {
        return Err(corrupt());
    }
    let language = FunctionLanguage::from_tag(r.u8()?)?;
    let volatility = Volatility::from_tag(r.u8()?)?;
    let flags = r.u8()?;
    if flags & !FLAG_MASK != 0 {
        return Err(corrupt());
    }
    let parallel = Parallel::from_tag(r.u8()?)?;
    let cost = r.opt_f64()?;
    let rows = r.opt_f64()?;
    let count = usize::try_from(r.u32()?).map_err(|_| corrupt())?;
    if count > MAX_FUNCTION_PARAMS {
        return Err(corrupt());
    }
    let mut params = Vec::new();
    params.try_reserve_exact(count).map_err(|_| corrupt())?;
    for _ in 0..count {
        let name = if r.bool()? {
            Some(r.string(MAX_NAME_FIELD_BYTES)?)
        } else {
            None
        };
        params.push(FunctionParam {
            name,
            ty: r.opt_type()?,
        });
    }
    let return_type = r.opt_type()?;
    let body_source = r.string(MAX_FUNCTION_BODY_BYTES)?;
    if r.offset != bytes.len() {
        return Err(corrupt());
    }
    let def = FunctionDef {
        id,
        function_key: function_key.to_string(),
        params,
        return_type,
        language,
        body_source,
        attrs: FunctionAttributes {
            volatility,
            strict: flags & FLAG_STRICT != 0,
            security: if flags & FLAG_SECURITY_DEFINER != 0 {
                Security::Definer
            } else {
                Security::Invoker
            },
            parallel,
            cost,
            rows,
        },
    };
    validate_shape(&def)?;
    if arg_signature(&def.params)? != arg_signature_key {
        return Err(corrupt());
    }
    Ok(def)
}

/// `scope`（本 Issue では `tenant_id`）の形状検査。空・長さ超過は拒否する。
fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > usize::from(MAX_TENANT_ID_LEN) {
        return Err(CatalogError::Invalid("invalid function scope".to_string()));
    }
    Ok(())
}

/// 1 件を引く（表が無ければ `None`。`key` は正規化済み）。
pub(crate) fn get_function_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    function_key: &str,
    signature: &str,
) -> Result<Option<FunctionDef>> {
    let table = match src.open_table(FUNCTIONS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((scope, function_key, signature))?;
    match found {
        Some(v) => decode_function_def(function_key, signature, v.value()).map(Some),
        None => Ok(None),
    }
}

/// `scope` の定義を `(function_key, arg_signature)` の昇順で列挙する。`only_key` を指定すると
/// その名前の多重定義だけを返す。件数が [`MAX_FUNCTIONS`] を超えたら `CorruptSchema`
/// （破損時の無制限確保の防止）。
fn list_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    only_key: Option<&str>,
) -> Result<Vec<FunctionDef>> {
    let table = match src.open_table(FUNCTIONS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let start = only_key.unwrap_or("");
    let mut out = Vec::new();
    for entry in table.range((scope, start, "")..)? {
        let (k, v) = entry?;
        let (s, key, sig) = k.value();
        if s != scope || only_key.is_some_and(|want| want != key) {
            break;
        }
        if out.len() >= MAX_FUNCTIONS {
            return Err(corrupt());
        }
        out.push(decode_function_def(key, sig, v.value())?);
    }
    Ok(out)
}

/// `scope` の定義数を `limit` + 1 件で打ち切って数える。
fn count_functions_capped<R: ReadSource>(src: &R, scope: &str, limit: usize) -> Result<usize> {
    let table = match src.open_table(FUNCTIONS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut count = 0usize;
    for entry in table.range((scope, "", "")..)? {
        let (k, _) = entry?;
        if k.value().0 != scope {
            break;
        }
        count += 1;
        if count > limit {
            break;
        }
    }
    Ok(count)
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_function_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(FUNCTION_ID_COUNTER_TABLE)?;
    let current = counter
        .get(FUNCTION_ID_COUNTER_KEY)?
        .map(|v| v.value())
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(FUNCTION_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// 定義の書き込みを集約する単一の挿入点（後続 Issue が世代カウンタ更新などを足す場所）。
fn insert_function_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    def: &FunctionDef,
    signature: &str,
) -> Result<()> {
    let blob = encode_function_def(def)?;
    let mut table = write_txn.open_table(FUNCTIONS_TABLE)?;
    table.insert(
        (scope, def.function_key.as_str(), signature),
        blob.as_slice(),
    )?;
    Ok(())
}

/// 定義の削除を集約する単一の点。存在した場合に `true`（表が無ければ作らず `false`）。
fn remove_function_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    function_key: &str,
    signature: &str,
) -> Result<bool> {
    if get_function_in_txn(write_txn, scope, function_key, signature)?.is_none() {
        return Ok(false);
    }
    let mut table = write_txn.open_table(FUNCTIONS_TABLE)?;
    let removed = table.remove((scope, function_key, signature))?.is_some();
    Ok(removed)
}

/// SCHEMA 接頭辞（`#<id>.`）配下に `scope` の関数が 1 件でもあるか（`DROP SCHEMA ... RESTRICT` の
/// 空判定用。表が無ければ `false`）。
pub(crate) fn functions_in_schema_exist_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    prefix: &str,
) -> Result<bool> {
    let table = match src.open_table(FUNCTIONS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    // 範囲の先頭 1 件だけを見る（scope が変わる・接頭辞が外れた時点で配下に無い）。
    let first = table.range((scope, prefix, "")..)?.next().transpose()?;
    Ok(first.is_some_and(|(k, _)| {
        let (s, key, _) = k.value();
        s == scope && key.starts_with(prefix)
    }))
}

/// SCHEMA 接頭辞配下の `scope` の関数をすべて削除する（`DROP SCHEMA ... CASCADE` 用。削除は単一の
/// [`remove_function_in_txn`] を経由する）。件数は [`MAX_FUNCTIONS`] で打ち切り、超えたら `CorruptSchema`。
pub(crate) fn remove_functions_with_prefix_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    prefix: &str,
) -> Result<()> {
    let mut targets: Vec<(String, String)> = Vec::new();
    {
        let table = match write_txn.open_table(FUNCTIONS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in table.range((scope, prefix, "")..)? {
            let (k, _) = entry?;
            let (s, key, sig) = k.value();
            if s != scope || !key.starts_with(prefix) {
                break;
            }
            if targets.len() >= MAX_FUNCTIONS {
                return Err(corrupt());
            }
            targets.push((key.to_string(), sig.to_string()));
        }
    }
    for (key, sig) in targets {
        remove_function_in_txn(write_txn, scope, &key, &sig)?;
    }
    Ok(())
}

impl Storage {
    /// `scope` に関数定義を作り、払い出した内部 ID を返す。判定順は形状検証 → 同一
    /// `(関数名, 引数シグネチャ)` の既存（`Invalid`。SQLSTATE への写像は #1920 が決める）→ 件数
    /// 上限（`ViewLimitExceeded`）→ ID 払い出しと保存を同一 write txn・1 回の commit で行う。
    /// `def.id` は無視される。#1920 の `CREATE FUNCTION` が呼ぶ予定。
    pub(crate) fn create_function(&self, scope: &str, def: &FunctionDef) -> Result<u64> {
        self.create_function_with_limit(scope, def, MAX_FUNCTIONS)
    }

    /// [`Self::create_function`] の本体（件数上限を引数にしてテストで小さい値を注入する）。
    fn create_function_with_limit(
        &self,
        scope: &str,
        def: &FunctionDef,
        max_functions: usize,
    ) -> Result<u64> {
        validate_scope(scope)?;
        let mut stored = def.clone();
        stored.function_key = normalize_key(&def.function_key)?;
        stored.id = 1;
        // 形状・上限の検証（write txn に入る前に拒否する）。
        encode_function_def(&stored)?;
        let signature = arg_signature(&stored.params)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        // SCHEMA 配下形のキーは、挿入と同じ write txn で当該テナントの SCHEMA の現存を確認する
        // （名前解決後の `DROP SCHEMA` との競合で孤児定義を作らない。Issue #1920）。
        super::namespace::ensure_tenant_key_schema_alive_in_txn(
            &*write_txn,
            scope,
            &stored.function_key,
        )?;
        // 同じ名前の定義が 1 件でもあれば、シグネチャが違っても既存扱いにする（`UdfRegistry` は
        // 名前単位で多重定義を持てず、書けても読めない永続状態を作らないため。設計記録 §書き込み経路）。
        if !list_in_txn(&*write_txn, scope, Some(&stored.function_key))?.is_empty() {
            return Err(CatalogError::Invalid(
                FUNCTION_ALREADY_EXISTS_MESSAGE.to_string(),
            ));
        }
        if count_functions_capped(&*write_txn, scope, max_functions)? >= max_functions {
            return Err(CatalogError::ViewLimitExceeded(
                "too many functions".to_string(),
            ));
        }
        stored.id = allocate_function_id_in_txn(&write_txn)?;
        insert_function_in_txn(&write_txn, scope, &stored, &signature)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        // commit 成功後にだけ世代を進める（他セッションの refresh が検知する。SQL-66・#1921）。
        self.bump_function_generation(scope);
        Ok(stored.id)
    }

    /// `(scope, 関数名, 引数シグネチャ)` の定義を 1 件引く（読み取り専用）。他スコープの定義は
    /// 構造上引けず、不在と同じ `None` になる。
    #[allow(dead_code)] // #1921（セッション開始時のロード）が結線するまではテストからのみ呼ばれる。
    pub(crate) fn get_function(
        &self,
        scope: &str,
        function_key: &str,
        signature: &str,
    ) -> Result<Option<FunctionDef>> {
        validate_scope(scope)?;
        let key = normalize_key(function_key)?;
        let rt = self.db().begin_read()?;
        get_function_in_txn(&rt, scope, &key, signature)
    }

    /// `scope` の全定義を `(関数名, シグネチャ)` の昇順で返す（読み取り専用。#1921 のロード用）。
    pub(crate) fn list_functions(&self, scope: &str) -> Result<Vec<FunctionDef>> {
        validate_scope(scope)?;
        let rt = self.db().begin_read()?;
        list_in_txn(&rt, scope, None)
    }

    /// `scope` の同名の多重定義を返す（読み取り専用）。
    #[cfg(test)]
    pub(crate) fn list_overloads(
        &self,
        scope: &str,
        function_key: &str,
    ) -> Result<Vec<FunctionDef>> {
        validate_scope(scope)?;
        let key = normalize_key(function_key)?;
        let rt = self.db().begin_read()?;
        list_in_txn(&rt, scope, Some(&key))
    }

    /// 定義を削除する。存在して削除した場合に `true`。ID カウンタは巻き戻さない。
    #[cfg(test)]
    pub(crate) fn drop_function(
        &self,
        scope: &str,
        function_key: &str,
        signature: &str,
    ) -> Result<bool> {
        validate_scope(scope)?;
        let key = normalize_key(function_key)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        if !remove_function_in_txn(&write_txn, scope, &key, signature)? {
            return Ok(false);
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        // commit 成功後にだけ世代を進める（他セッションの refresh が検知する。SQL-66・#1921）。
        self.bump_function_generation(scope);
        Ok(true)
    }

    /// 同名の定義がちょうど 1 件で `matcher` が真のとき、同一 write txn で削除して `true`。
    /// 0 件・2 件以上・不一致は `false`（型付き `DROP FUNCTION` 用。列挙・照合・削除を原子的にする）。
    pub(crate) fn drop_function_matching(
        &self,
        scope: &str,
        function_key: &str,
        matcher: impl Fn(&FunctionDef) -> Result<bool>,
    ) -> Result<bool> {
        validate_scope(scope)?;
        let key = normalize_key(function_key)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let found = list_in_txn(&*write_txn, scope, Some(&key))?;
        let [def] = found.as_slice() else {
            return Ok(false);
        };
        if !matcher(def)? {
            return Ok(false);
        }
        let sig = arg_signature(&def.params)?;
        if !remove_function_in_txn(&write_txn, scope, &key, &sig)? {
            return Ok(false);
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        // commit 成功後にだけ世代を進める（他セッションの refresh が検知する。SQL-66・#1921）。
        self.bump_function_generation(scope);
        Ok(true)
    }

    /// 名前（引数の型の並びを省略した形）で定義を削除し、削除した定義のシグネチャを返す。0 件は
    /// `Ok(None)`、2 件以上は `Invalid(FUNCTION_AMBIGUOUS_MESSAGE)`（作成時に同名の多重定義を拒否して
    /// いるため通常は起きない防御）。列挙と削除は同一 write txn・1 回の commit で行う。
    pub(crate) fn drop_function_by_name(
        &self,
        scope: &str,
        function_key: &str,
    ) -> Result<Option<String>> {
        validate_scope(scope)?;
        let key = normalize_key(function_key)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let found = list_in_txn(&*write_txn, scope, Some(&key))?;
        let sig = match found.as_slice() {
            [] => return Ok(None),
            [one] => arg_signature(&one.params)?,
            _ => {
                return Err(CatalogError::Invalid(
                    FUNCTION_AMBIGUOUS_MESSAGE.to_string(),
                ));
            }
        };
        if !remove_function_in_txn(&write_txn, scope, &key, &sig)? {
            return Ok(None);
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        // commit 成功後にだけ世代を進める（他セッションの refresh が検知する。SQL-66・#1921）。
        self.bump_function_generation(scope);
        Ok(Some(sig))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ArrayElemType, ArrayType, ColumnDef, TableSchema};

    fn tmp_path(label: &str) -> (std::path::PathBuf, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (path, guard)
    }

    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let (path, guard) = tmp_path(label);
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn ty(t: &ColumnType) -> Option<FunctionType> {
        Some(FunctionType::from_column_type(t).expect("type"))
    }

    fn param(name: Option<&str>, t: Option<FunctionType>) -> FunctionParam {
        FunctionParam {
            name: name.map(str::to_string),
            ty: t,
        }
    }

    fn def(key: &str, params: Vec<FunctionParam>) -> FunctionDef {
        FunctionDef {
            id: 1,
            function_key: key.to_string(),
            params,
            return_type: ty(&ColumnType::Integer),
            language: FunctionLanguage::Sql,
            body_source: "SELECT 1".to_string(),
            attrs: FunctionAttributes::default(),
        }
    }

    fn array_ty(elem: ArrayElemType, max_len: u32, ndims: u8) -> ColumnType {
        let a = ArrayType::new(elem, max_len).expect("array");
        ColumnType::Array(a.with_ndims(ndims).expect("dims"))
    }

    /// SQL-66 (1)(2): 属性・言語・型の組み合わせが encode -> decode で完全一致する。
    #[test]
    fn blob_roundtrip_covers_languages_attributes_and_types() {
        let types = [
            ColumnType::Integer,
            ColumnType::Text,
            ColumnType::Varchar(Some(10)),
            ColumnType::Numeric {
                precision: 5,
                scale: 2,
            },
            ColumnType::Vector(3),
            ColumnType::Uuid,
            ColumnType::Jsonb,
            array_ty(ArrayElemType::Text, 10, 1),
            array_ty(ArrayElemType::Integer, 4, 2),
        ];
        let mut params: Vec<FunctionParam> = vec![param(None, None), param(Some("untyped"), None)];
        for (i, t) in types.iter().enumerate() {
            let name = if i % 2 == 0 {
                Some(format!("p{i}"))
            } else {
                None
            };
            params.push(param(name.as_deref(), ty(t)));
        }
        let mut n = 0u64;
        for language in [
            FunctionLanguage::Declarative,
            FunctionLanguage::Sql,
            FunctionLanguage::Plpgsql,
        ] {
            for volatility in [
                Volatility::Immutable,
                Volatility::Stable,
                Volatility::Volatile,
            ] {
                for parallel in [Parallel::Unsafe, Parallel::Restricted, Parallel::Safe] {
                    for (strict, security) in [
                        (false, Security::Invoker),
                        (true, Security::Definer),
                        (true, Security::Invoker),
                        (false, Security::Definer),
                    ] {
                        for (cost, rows) in [(None, None), (Some(0.5), Some(1000.0))] {
                            n += 1;
                            let mut d = def("fn_a", params.clone());
                            d.id = n;
                            d.language = language;
                            d.return_type = if n.is_multiple_of(2) {
                                None
                            } else {
                                d.return_type
                            };
                            d.body_source = "$$ 改行\nと日本語 'quote' $$".to_string();
                            d.attrs = FunctionAttributes {
                                volatility,
                                strict,
                                security,
                                parallel,
                                cost,
                                rows,
                            };
                            let sig = arg_signature(&d.params).expect("sig");
                            let bytes = encode_function_def(&d).expect("encode");
                            let back = decode_function_def("fn_a", &sig, &bytes).expect("decode");
                            assert_eq!(back, d);
                        }
                    }
                }
            }
        }
    }

    /// SQL-66 (2): typmod・配列の最大長と次元を無視し、型の違いは区別する。
    #[test]
    fn signature_ignores_typmod_and_distinguishes_types() {
        let sig = |t: ColumnType| arg_signature(&[param(None, ty(&t))]).expect("sig");
        assert_eq!(
            sig(ColumnType::Varchar(Some(10))),
            sig(ColumnType::Varchar(Some(20)))
        );
        assert_eq!(
            sig(ColumnType::Numeric {
                precision: 5,
                scale: 2
            }),
            sig(ColumnType::Numeric {
                precision: 10,
                scale: 0
            })
        );
        assert_eq!(sig(ColumnType::Vector(3)), sig(ColumnType::Vector(8)));
        assert_eq!(
            sig(array_ty(ArrayElemType::Text, 3, 1)),
            sig(array_ty(ArrayElemType::Text, 9, 2))
        );
        assert_ne!(sig(ColumnType::Integer), sig(ColumnType::Text));
        assert_ne!(
            sig(array_ty(ArrayElemType::Text, 3, 1)),
            sig(array_ty(ArrayElemType::Integer, 3, 1))
        );
        assert_eq!(arg_signature(&[]).expect("empty"), "");
        assert_eq!(arg_signature(&[param(None, None)]).expect("untyped"), "?");
        assert_eq!(
            arg_signature(&[param(None, ty(&ColumnType::Integer)), param(None, None)])
                .expect("two"),
            "integer;?"
        );
    }

    /// v1 のゴールデンバイト列を固定する（将来 v2 を作っても v1 を読み続ける回帰防止）。
    #[test]
    fn v1_golden_bytes_decode() {
        let mut b: Vec<u8> = vec![1];
        b.extend_from_slice(&5u64.to_le_bytes());
        b.extend_from_slice(&[0, 2, 2, 1, 3]);
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&[1, 1, 0, 0, 0, b'a']);
        b.push(1);
        b.extend_from_slice(&[7, 0, 0, 0]);
        b.extend_from_slice(b"integer");
        b.extend_from_slice(&[1, 0, 0, 0, b'-']);
        b.push(1);
        b.extend_from_slice(&[4, 0, 0, 0]);
        b.extend_from_slice(b"text");
        b.extend_from_slice(&[1, 0, 0, 0, b'-']);
        b.extend_from_slice(&[1, 0, 0, 0, b'x']);
        let d = decode_function_def("f", "integer", &b).expect("decode v1");
        assert_eq!(d.id, 5);
        assert_eq!(d.language, FunctionLanguage::Sql);
        assert_eq!(d.attrs.volatility, Volatility::Stable);
        assert!(d.attrs.strict);
        assert_eq!(d.attrs.security, Security::Invoker);
        assert_eq!(d.attrs.parallel, Parallel::Safe);
        assert_eq!(d.params, vec![param(Some("a"), ty(&ColumnType::Integer))]);
        assert_eq!(d.return_type, ty(&ColumnType::Text));
        assert_eq!(d.body_source, "x");
        assert_eq!(encode_function_def(&d).expect("re-encode"), b);
    }

    /// 破損・未知値・上限超過はすべて固定文言の `CorruptSchema`。
    #[test]
    fn decode_is_fail_closed() {
        let mut d = def("f", vec![param(Some("a"), ty(&ColumnType::Integer))]);
        d.attrs.cost = Some(2.0);
        let good = encode_function_def(&d).expect("encode");
        let sig = "integer";
        assert!(decode_function_def("f", sig, &good).is_ok());

        let mut bad: Vec<Vec<u8>> = vec![vec![]];
        for cut in 1..good.len() {
            bad.push(good[..cut].to_vec());
        }
        bad.push([good.as_slice(), &[0]].concat());
        let patch = |i: usize, v: u8| {
            let mut x = good.clone();
            x[i] = v;
            x
        };
        // オフセット: 0 版 / 1..9 id / 9 owner / 10 language / 11 volatility / 12 flags / 13 parallel
        for (i, v) in [
            (0, 0),
            (0, 2),
            (0, 255),
            (9, 1),
            (10, 0),
            (10, 4),
            (11, 0),
            (11, 4),
            (12, 4),
            (12, 255),
            (13, 0),
            (13, 4),
            (14, 2),
        ] {
            bad.push(patch(i, v));
        }
        let mut zero_id = good.clone();
        zero_id[1..9].copy_from_slice(&0u64.to_le_bytes());
        bad.push(zero_id);
        // cost: 14 が present、15..23 が値。NaN・無限大・0・負数。
        for v in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let mut x = good.clone();
            x[15..23].copy_from_slice(&v.to_bits().to_le_bytes());
            bad.push(x);
        }
        for b in &bad {
            let err = decode_function_def("f", sig, b).expect_err("must fail");
            assert!(
                matches!(&err, CatalogError::CorruptSchema(m) if m == "invalid function definition"),
                "{err:?}"
            );
        }
        // キーとシグネチャの不一致・不正キー・大文字キー。
        for (k, s) in [
            ("f", "text"),
            ("f", ""),
            ("g h", sig),
            ("F", sig),
            ("#0.f", sig),
        ] {
            assert!(matches!(
                decode_function_def(k, s, &good),
                Err(CatalogError::CorruptSchema(_))
            ));
        }
    }

    /// 型・名前・件数・長さの破損（手組み blob）も `CorruptSchema` になる。
    #[test]
    fn decode_rejects_bad_fields_and_oversize() {
        let mut d = def("f", vec![param(Some("a"), ty(&ColumnType::Integer))]);
        let good = encode_function_def(&d).expect("encode");
        let replace = |from: &[u8], to: &[u8]| -> Vec<u8> {
            let pos = good
                .windows(from.len())
                .position(|w| w == from)
                .expect("pattern");
            let mut x = good[..pos].to_vec();
            x.extend_from_slice(to);
            x.extend_from_slice(&good[pos + from.len()..]);
            x
        };
        let mut cases: Vec<Vec<u8>> = vec![
            // 引数名が不正な識別子
            replace(&[1, 0, 0, 0, b'a'], &[1, 0, 0, 0, b'1']),
            // 不正 UTF-8 の引数名
            replace(&[1, 0, 0, 0, b'a'], &[1, 0, 0, 0, 0xff]),
            // 未知の型タグ
            replace(b"integer", b"intxxxx"),
        ];
        // 引数数が上限 + 1
        let count_pos = 1 + 8 + 1 + 4 + 1 + 1 + 1;
        let mut over = good.clone();
        over[count_pos..count_pos + 4]
            .copy_from_slice(&((MAX_FUNCTION_PARAMS as u32) + 1).to_le_bytes());
        cases.push(over);
        // 本体長が上限 + 1（実体なし）
        let mut huge = good[..good.len() - 8].to_vec();
        huge.extend_from_slice(&((MAX_FUNCTION_BODY_BYTES as u32) + 1).to_le_bytes());
        cases.push(huge);
        // 先頭の bool が 2
        let mut badbool = good.clone();
        badbool[count_pos + 4] = 2;
        cases.push(badbool);
        for (i, c) in cases.iter().enumerate() {
            assert!(
                matches!(
                    decode_function_def("f", "integer", c),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "case {i}"
            );
        }
        // ENUM 型は encode でも拒否する。
        d.params = vec![FunctionParam {
            name: None,
            ty: Some(FunctionType {
                tag: "enum".to_string(),
                param: "mood".to_string(),
            }),
        }];
        assert!(encode_function_def(&d).is_err());
    }

    /// encode 側の上限と形状検証。
    #[test]
    fn encode_rejects_invalid_shapes() {
        let ok = def("f", vec![]);
        let mut d = ok.clone();
        d.params = (0..=MAX_FUNCTION_PARAMS)
            .map(|_| param(None, None))
            .collect();
        assert!(matches!(
            encode_function_def(&d),
            Err(CatalogError::Invalid(_))
        ));
        let mut d = ok.clone();
        d.body_source = "x".repeat(MAX_FUNCTION_BODY_BYTES + 1);
        assert!(matches!(
            encode_function_def(&d),
            Err(CatalogError::Invalid(_))
        ));
        let mut d = ok.clone();
        d.body_source = "x".repeat(MAX_FUNCTION_BODY_BYTES);
        assert!(encode_function_def(&d).is_ok());
        let mut d = ok.clone();
        d.id = 0;
        assert!(encode_function_def(&d).is_err());
        let mut d = ok.clone();
        d.function_key = "Upper".to_string();
        assert!(encode_function_def(&d).is_err());
        let mut d = ok.clone();
        d.function_key = "a b".to_string();
        assert!(encode_function_def(&d).is_err());
        let mut d = ok.clone();
        d.params = vec![param(Some("x"), None), param(Some("X"), None)];
        assert!(encode_function_def(&d).is_err());
        for v in [f64::NAN, f64::INFINITY, 0.0, -2.0] {
            let mut d = ok.clone();
            d.attrs.rows = Some(v);
            assert!(encode_function_def(&d).is_err());
        }
        let mut d = ok;
        d.params = vec![param(
            None,
            Some(FunctionType {
                tag: "bogus".to_string(),
                param: "-".to_string(),
            }),
        )];
        assert!(encode_function_def(&d).is_err());
    }

    /// SQL-66 (1): Storage 経由の往復と、再オープン後の永続性。
    #[test]
    fn storage_roundtrip_and_persists_across_reopen() {
        let (path, _g) = tmp_path("fn-reopen");
        let id;
        {
            let s = Storage::open(&path).expect("open");
            let mut d = def("Fn_Reopen", vec![param(Some("a"), ty(&ColumnType::Text))]);
            d.attrs.strict = true;
            id = s.create_function("alice", &d).expect("create");
            let got = s
                .get_function("alice", "fn_reopen", "text")
                .expect("get")
                .expect("some");
            assert_eq!(got.id, id);
            assert_eq!(got.function_key, "fn_reopen");
            assert!(got.attrs.strict);
            assert_eq!(s.list_functions("alice").expect("list"), vec![got]);
        }
        let s = Storage::open(&path).expect("reopen");
        let got = s
            .get_function("alice", "fn_reopen", "text")
            .expect("get")
            .expect("persisted");
        assert_eq!(got.id, id);
        assert_eq!(got.body_source, "SELECT 1");
        assert_eq!(
            got.params[0]
                .ty
                .as_ref()
                .expect("ty")
                .to_column_type()
                .expect("col"),
            ColumnType::Text
        );
    }

    /// 型フィールドの長さ上限（128 受理・129 拒否）と、保存後の get/list/drop の往復。
    #[test]
    fn type_field_length_limit_is_enforced_before_store() {
        let (s, _g) = tmp_storage("fn-typelen");
        let vec_ty = |n: usize| FunctionType {
            tag: "vector".to_string(),
            param: format!("{}1", "0".repeat(n - 1)),
        };
        // 129 バイトは引数・戻り値どちらでも保存前に拒否する。
        let long = Some(vec_ty(129));
        let err = s
            .create_function("t", &def("f_long", vec![param(None, long.clone())]))
            .expect_err("param too long");
        assert!(matches!(err, CatalogError::Invalid(_)));
        let mut d = def("f_long_ret", vec![]);
        d.return_type = long;
        assert!(matches!(
            s.create_function("t", &d),
            Err(CatalogError::Invalid(_))
        ));
        // 128 バイトは受理され、get/list/drop が読み出せる。
        let ok = Some(vec_ty(128));
        let d = def("f_edge", vec![param(None, ok)]);
        s.create_function("t", &d).expect("128 accepted");
        let listed = s.list_functions("t").expect("list");
        assert_eq!(listed.len(), 1);
        let key = listed[0].params[0]
            .ty
            .as_ref()
            .expect("ty")
            .identity_token();
        assert!(s.get_function("t", "f_edge", &key).expect("get").is_some());
        assert!(s.drop_function("t", "f_edge", &key).expect("drop"));
    }

    /// 保存層の検査を迂回して定義を直接書く（多重定義の状態を作るテスト専用）。
    fn insert_raw(s: &Storage, scope: &str, d: &FunctionDef) {
        let w = s.begin_write_txn().expect("w");
        let mut stored = d.clone();
        stored.id = allocate_function_id_in_txn(&w).expect("id");
        let sig = arg_signature(&stored.params).expect("sig");
        insert_function_in_txn(&w, scope, &stored, &sig).expect("insert");
        crate::recovery::commit_boundary::commit(w).expect("commit");
    }

    /// SQL-66 (2)・#1920: 同じ名前の定義があればシグネチャが違っても既存扱いで拒否する
    /// （名前単位の多重定義は `UdfRegistry` が持てないため）。typmod 違いも同様。
    #[test]
    fn same_name_is_rejected_regardless_of_signature() {
        let (s, _g) = tmp_storage("fn-overload");
        s.create_function("t", &def("f", vec![param(None, ty(&ColumnType::Integer))]))
            .expect("int");
        for p in [
            param(None, ty(&ColumnType::Text)),
            param(None, ty(&ColumnType::Integer)),
        ] {
            let err = s
                .create_function("t", &def("F", vec![p]))
                .expect_err("same name");
            assert!(
                matches!(&err, CatalogError::Invalid(m) if m == FUNCTION_ALREADY_EXISTS_MESSAGE)
            );
        }
        s.create_function("t", &def("g", vec![])).expect("g");
        assert_eq!(s.list_overloads("t", "f").expect("ov").len(), 1);
        assert_eq!(s.list_functions("t").expect("all").len(), 2);
        assert!(s.drop_function("t", "f", "integer").expect("drop"));
        assert!(!s.drop_function("t", "f", "integer").expect("again"));
        s.create_function("t", &def("f", vec![param(None, ty(&ColumnType::Text))]))
            .expect("name is free after drop");
    }

    /// #1920: 型付き削除は照合と削除が同一 txn。不一致・0 件・多重定義は削除しない。
    #[test]
    fn drop_matching_only_when_single_and_matcher_true() {
        let (s, _g) = tmp_storage("fn-dropmatch");
        assert!(!s
            .drop_function_matching("t", "f", |_| Ok(true))
            .expect("zero"));
        s.create_function("t", &def("f", vec![param(None, ty(&ColumnType::Integer))]))
            .expect("f");
        assert!(!s
            .drop_function_matching("t", "f", |_| Ok(false))
            .expect("no match"));
        assert_eq!(s.list_overloads("t", "f").expect("ov").len(), 1);
        assert!(s
            .drop_function_matching("t", "f", |_| Ok(true))
            .expect("drop"));
        assert!(s.list_overloads("t", "f").expect("ov").is_empty());
    }

    /// #1920: 名前での削除。0 件は `None`・1 件は削除・2 件（直接作った防御的な状態）は拒否。
    #[test]
    fn drop_by_name_handles_zero_one_and_many() {
        let (s, _g) = tmp_storage("fn-dropname");
        assert_eq!(s.drop_function_by_name("t", "nope").expect("zero"), None);
        s.create_function("t", &def("f", vec![param(None, ty(&ColumnType::Integer))]))
            .expect("f");
        assert_eq!(
            s.drop_function_by_name("t", "F").expect("one"),
            Some("integer".to_string())
        );
        assert!(s.list_functions("t").expect("list").is_empty());
        insert_raw(
            &s,
            "t",
            &def("g", vec![param(None, ty(&ColumnType::Integer))]),
        );
        insert_raw(&s, "t", &def("g", vec![param(None, ty(&ColumnType::Text))]));
        let err = s.drop_function_by_name("t", "g").expect_err("many");
        assert!(matches!(&err, CatalogError::Invalid(m) if m == FUNCTION_AMBIGUOUS_MESSAGE));
        assert_eq!(s.list_overloads("t", "g").expect("ov").len(), 2);
        assert_eq!(
            s.drop_function_by_name("u", "g").expect("other tenant"),
            None
        );
    }

    /// #1920: 削除は再オープン後も保たれ、ID は再利用されない。
    #[test]
    fn create_and_drop_persist_across_reopen() {
        let (path, _g) = tmp_path("fn-reopen-drop");
        let first;
        {
            let s = Storage::open(&path).expect("open");
            first = s.create_function("t", &def("f", vec![])).expect("f");
            s.create_function("t", &def("g", vec![])).expect("g");
            assert!(s.drop_function("t", "f", "").expect("drop"));
        }
        let s = Storage::open(&path).expect("reopen");
        assert!(s.get_function("t", "f", "").expect("get").is_none());
        assert!(s.get_function("t", "g", "").expect("get").is_some());
        let again = s.create_function("t", &def("f", vec![])).expect("again");
        assert!(again > first);
    }

    /// #1920: SCHEMA 接頭辞の存在判定と一括削除は自テナント・自接頭辞に限る。
    #[test]
    fn schema_prefix_helpers_are_scoped() {
        let (s, _g) = tmp_storage("fn-prefix");
        // 関数は生存する SCHEMA 配下にしか作れない（Issue #1920）ため実 SCHEMA を作る。
        s.create_schema("t", "a", false).expect("a");
        s.create_schema("t", "b", false).expect("b");
        s.create_schema("u", "a", false).expect("u a");
        let id = |t: &str, n: &str| s.schema_id(t, n).expect("id").expect("exists");
        let (ta, tb, ua) = (id("t", "a"), id("t", "b"), id("u", "a"));
        let key = |i: u64| format!("#{i}.f");
        let prefix = |i: u64| format!("#{i}.");
        s.create_function("t", &def(&key(ta), vec![]))
            .expect("in schema");
        s.create_function("t", &def(&key(tb), vec![]))
            .expect("other schema");
        s.create_function("t", &def("plain", vec![]))
            .expect("public");
        s.create_function("u", &def(&key(ua), vec![]))
            .expect("other tenant");
        {
            let rt = s.db().begin_read().expect("read");
            assert!(functions_in_schema_exist_in_txn(&rt, "t", &prefix(ta)).expect("exist"));
            assert!(!functions_in_schema_exist_in_txn(&rt, "t", "#999.").expect("none"));
            assert!(!functions_in_schema_exist_in_txn(&rt, "v", &prefix(ta)).expect("tenant"));
        }
        let w = s.begin_write_txn().expect("w");
        remove_functions_with_prefix_in_txn(&w, "t", &prefix(ta)).expect("remove");
        crate::recovery::commit_boundary::commit(w).expect("commit");
        let mut keys: Vec<String> = s
            .list_functions("t")
            .expect("list")
            .into_iter()
            .map(|d| d.function_key)
            .collect();
        keys.sort();
        let mut want = vec![key(tb), "plain".to_string()];
        want.sort();
        assert_eq!(keys, want);
        assert_eq!(s.list_functions("u").expect("u").len(), 1);
    }

    /// RLS-12（P0）: 他テナントの定義は不可視で、同名の独立作成も互いに干渉しない。
    #[test]
    fn tenants_are_isolated() {
        let (s, _g) = tmp_storage("fn-tenant");
        let mut a = def("shared", vec![]);
        a.body_source = "SELECT 'a'".to_string();
        s.create_function("alice", &a).expect("alice");
        assert!(s.get_function("bob", "shared", "").expect("get").is_none());
        assert!(s.list_functions("bob").expect("list").is_empty());
        assert!(s.list_overloads("bob", "shared").expect("ov").is_empty());
        assert!(!s.drop_function("bob", "shared", "").expect("drop"));
        let mut b = def("shared", vec![]);
        b.body_source = "SELECT 'b'".to_string();
        s.create_function("bob", &b).expect("bob");
        let got_a = s
            .get_function("alice", "shared", "")
            .expect("a")
            .expect("some");
        assert_eq!(got_a.body_source, "SELECT 'a'");
        assert!(s.drop_function("bob", "shared", "").expect("drop b"));
        assert!(s.get_function("alice", "shared", "").expect("a").is_some());
        for bad in ["", &"t".repeat(usize::from(MAX_TENANT_ID_LEN) + 1)] {
            assert!(s.create_function(bad, &a).is_err());
            assert!(s.get_function(bad, "shared", "").is_err());
            assert!(s.list_functions(bad).is_err());
        }
    }

    /// 旧データ互換: 関数を使わない DB は読み取りで表を作らず、書き込みも既存カタログを変えない。
    #[test]
    fn legacy_db_is_unchanged_by_function_catalog() {
        let (s, _g) = tmp_storage("fn-legacy");
        s.create_schema("alice", "sch", false).expect("schema");
        s.create_table(&TableSchema::new(
            "t1",
            vec![ColumnDef::new("c", ColumnType::Text, true)],
        ))
        .expect("table");
        s.create_view("v1", "t1", "SELECT c FROM t1").expect("view");
        assert!(s.list_functions("alice").expect("list").is_empty());
        assert!(s.get_function("alice", "f", "").expect("get").is_none());
        {
            let rt = s.db().begin_read().expect("read");
            assert!(matches!(
                rt.open_table(FUNCTIONS_TABLE),
                Err(redb::TableError::TableDoesNotExist(_))
            ));
            assert!(matches!(
                rt.open_table(FUNCTION_ID_COUNTER_TABLE),
                Err(redb::TableError::TableDoesNotExist(_))
            ));
        }
        assert!(!s.drop_function("alice", "f", "").expect("drop"));
        {
            let rt = s.db().begin_read().expect("read");
            assert!(rt.open_table(FUNCTIONS_TABLE).is_err());
        }
        let dump = |s: &Storage| -> Vec<(String, Vec<u8>)> {
            let rt = s.db().begin_read().expect("read");
            let t = rt.open_table(super::super::CATALOG_TABLE).expect("catalog");
            t.iter()
                .expect("iter")
                .map(|e| {
                    let (k, v) = e.expect("entry");
                    (k.value().to_string(), v.value().to_vec())
                })
                .collect()
        };
        let before = dump(&s);
        assert!(!before.is_empty());
        s.create_function("alice", &def("f", vec![]))
            .expect("create");
        assert_eq!(dump(&s), before);
    }

    /// SQL-66 (5): 件数上限はスコープ単位。上限超過は `ViewLimitExceeded`。
    #[test]
    fn function_count_limit_is_per_scope() {
        let (s, _g) = tmp_storage("fn-limit");
        s.create_function_with_limit("a", &def("f1", vec![]), 2)
            .expect("1");
        s.create_function_with_limit("a", &def("f2", vec![]), 2)
            .expect("2");
        assert!(matches!(
            s.create_function_with_limit("a", &def("f3", vec![]), 2),
            Err(CatalogError::ViewLimitExceeded(_))
        ));
        s.create_function_with_limit("b", &def("f1", vec![]), 2)
            .expect("other scope is unaffected");
        assert!(s.drop_function("a", "f1", "").expect("drop"));
        s.create_function_with_limit("a", &def("f3", vec![]), 2)
            .expect("slot freed");
    }

    /// 内部 ID は単調増加で、drop 後に作り直しても再利用されない。
    #[test]
    fn ids_are_monotonic_and_never_reused() {
        let (s, _g) = tmp_storage("fn-id");
        let a = s.create_function("t", &def("f", vec![])).expect("a");
        let b = s.create_function("t", &def("g", vec![])).expect("b");
        assert!(a >= 1 && b > a);
        assert!(s.drop_function("t", "g", "").expect("drop"));
        let c = s.create_function("t", &def("g", vec![])).expect("c");
        assert!(c > b);
        let d = s.create_function("u", &def("g", vec![])).expect("d");
        assert!(d > c);
    }

    /// ENUM 型は本 Issue では保存対象外（encode・シグネチャ計算とも拒否）。
    #[test]
    fn enum_types_are_rejected() {
        let bad = FunctionType {
            tag: "array".to_string(),
            param: "enum,mood,4".to_string(),
        };
        assert!(bad.to_column_type().is_err());
        assert!(arg_signature(&[param(None, Some(bad))]).is_err());
    }

    /// 削除済み SCHEMA・他テナントの SCHEMA の ID 配下には関数を作れず、生存する自テナントの
    /// SCHEMA 配下には作れる（名前解決後の `DROP SCHEMA` との競合・準備済み CREATE の再実行。Issue #1920）。
    #[test]
    fn create_function_requires_live_tenant_schema() {
        use crate::catalog::namespace::RelationKey;
        let (s, _g) = tmp_storage("fn-schema-alive");
        s.create_schema("alice", "a", false).expect("schema");
        let id = s.schema_id("alice", "a").expect("id").expect("exists");
        let key = RelationKey::InSchema {
            schema_id: id,
            name: "f".to_string(),
        }
        .encode();
        // 他テナントからは拒否（存在情報を区別しない）。
        assert!(matches!(
            s.create_function("bob", &def(&key, vec![])),
            Err(CatalogError::TableNotFound(_))
        ));
        s.create_function("alice", &def(&key, vec![]))
            .expect("live schema accepts");
        s.drop_function_by_name("alice", &key).expect("drop fn");
        s.drop_schema("alice", "a", false).expect("drop schema");
        // DROP SCHEMA 後の CREATE（競合・準備済み文の再実行）は拒否され、何も残らない。
        assert!(matches!(
            s.create_function("alice", &def(&key, vec![])),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(s.list_functions("alice").expect("list").is_empty());
    }
}
