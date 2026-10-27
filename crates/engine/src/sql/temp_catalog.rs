//! セッション単位の一時カタログと、一時表を永続表より先に解決する名前解決層
//! （TABLE-44・Issue #1681。親 #1680 の TEMP TABLE 系列の土台）。
//!
//! # 責務境界
//!
//! - `SessionTempCatalog`: 接続（セッション）ごとの一時表のスキーマ台帳。
//!   [`crate::sql::mode::SessionState`] が保持するため、他接続・他テナントへ漏れる経路は
//!   構造上存在しない。永続化しない。行データは持たない（行は `temp_rows` の行ストアが持つ）。
//! - `SessionRelationLookup`: 修飾なしの名前を「一時カタログ →
//!   永続カタログ」の順に解決する。`sql::allowlist::TableLookup` を包むラッパーで、trait
//!   自体は変更しない。呼び出し元は `core.rs` の `EngineCore::parse_tokens_in`。
//! - `tokens_reference_temp`: トークン列が一時表の名前を参照しうるかの保守的な判定。
//!   カタログを引かない文（DROP・ALTER・CREATE INDEX 等）や永続ビュー本文の再検証も覆う。
//!
//! # 振り分けと残る拒否（#1684）
//!
//! 一時表だけを参照する文（`INSERT`／`UPDATE`／`DELETE`／`TRUNCATE`／読み取り系／索引 DDL）は
//! 一時コア（`sql::temp_rows` のモジュールドキュメント）で実行する。一時表と永続の関係
//! （表・ビュー）を 1 文で混ぜる文、および振り分け対象外の文種で一時表の名前が現れる文は
//! `0A000` で拒否する（fail-closed。同名の**永続表**へ黙って作用しないため）。一時カタログが
//! 空なら（一時表を作っていないセッション）何もせず、既存の挙動と同一である。
//! 判定は `core/temp_route.rs` が行う。
//!
//! # 後続 Issue への申し送り
//!
//! 実体ストアと切断時の破棄は `temp_rows`（#1682）、`CREATE TEMP TABLE` と
//! `ON COMMIT`（`SessionState::apply_temp_on_commit`）・一時表の `DROP` は #1683、
//! 一時表への DML・索引・制約は #1684、テナント境界・crash 耐性の検査は #1685。
//! 修飾名（`pg_temp.<名前>`）の結線は別 Issue へ申し送る（`search_path`〔TABLE-27・Issue #1715〕は実装済みで、
//! 一時表は path に関わらず先頭で解決する）。

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::catalog::TableSchema;
use crate::sql::allowlist::{SqlSurfaceError, TableLookup, TempOnCommit};
use crate::sql::lexer::Token;

/// 1 セッションに登録できる一時表の数の既定上限（TABLE-44 (6)）。スキーマ 1 件は小さいが、
/// `SessionState` は BEGIN／メッセージ単位で `clone` されるため無制限にはしない。
/// 通常の利用（作業用の数表）には十分で、暴走したクライアントによる確保を抑える値とする。
#[cfg(test)]
pub(crate) const DEFAULT_MAX_TEMP_TABLES_PER_SESSION: usize = 64;

/// 一時表のプロセス内で一意な識別子。永続化しない（再起動で採番し直す）。
/// 同名の再作成を区別するために登録ごとに採番する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TempRelationId(u64);

static NEXT_TEMP_RELATION_ID: AtomicU64 = AtomicU64::new(1);

/// 一時カタログの変更ごとに進むプロセス全体で単調なカタログ世代
/// （[`SessionTempCatalog::epoch`]）の採番元。0 は採番されない（「未設定」の予約値）。
static NEXT_TEMP_CATALOG_EPOCH: AtomicU64 = AtomicU64::new(1);

/// 次のカタログ世代を採番する。`u64` が尽きた場合は `None`。
fn next_catalog_epoch() -> Option<u64> {
    NEXT_TEMP_CATALOG_EPOCH
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        .ok()
}

/// 次の識別子を採番する。`u64` が尽きた場合は `Internal`（fail-closed。到達しない想定）。
fn next_temp_relation_id() -> Result<TempRelationId, SqlSurfaceError> {
    let id = NEXT_TEMP_RELATION_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "temporary relation id space exhausted".to_string(),
        })?;
    Ok(TempRelationId(id))
}

/// 一時カタログの 1 エントリ。スキーマは `Arc` 共有（`SessionState` の `clone` を安く保つ）。
#[derive(Debug, Clone)]
pub(crate) struct TempTableEntry {
    pub(crate) id: TempRelationId,
    pub(crate) schema: Arc<TableSchema>,
    /// `ON COMMIT` の動作（TABLE-44・Issue #1683）。commit フック
    /// （`SessionState::apply_temp_on_commit`）が読む。
    pub(crate) on_commit: TempOnCommit,
}

/// セッション単位の一時カタログ（モジュールドキュメント参照）。
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionTempCatalog {
    tables: BTreeMap<String, TempTableEntry>,
    /// 変更（登録・削除・索引 DDL）のたびにプロセス全体で単調に増える採番から新しい値を取る。
    /// ROLLBACK によるカタログの復元では値も一緒に戻るため、値が等しいことはカタログの状態が
    /// 同じであることを意味する。準備済み文の陳腐化検出（`ParsedSql::SessionRouted`）が使う。
    epoch: u64,
}

impl SessionTempCatalog {
    pub(crate) fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.tables.len()
    }

    /// 現在のカタログ世代（構造体ドキュメント参照）。
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// カタログの内容は変えずに世代だけを進める（一時表の索引 DDL など、既存の準備済み文の
    /// 前提が変わる変更の後に呼ぶ）。
    pub(crate) fn bump_epoch(&mut self) -> Result<(), SqlSurfaceError> {
        self.epoch = next_catalog_epoch().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "temporary catalog epoch space exhausted".to_string(),
        })?;
        Ok(())
    }

    pub(crate) fn get(&self, name: &str) -> Option<&TempTableEntry> {
        self.tables.get(name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    /// 登録名を決定的な（辞書）順で返す。
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    /// 一時表を登録する。識別子は永続の `CREATE TABLE` と同じ検証（`validate_identifier`）、
    /// 同名の重複は `42P07`、上限超過は `54000`（確保の前に拒否する）。
    pub(crate) fn register(
        &mut self,
        schema: TableSchema,
        on_commit: TempOnCommit,
        max_tables: usize,
    ) -> Result<TempRelationId, SqlSurfaceError> {
        crate::catalog::validate_identifier(&schema.name)
            .map_err(crate::catalog::table_lookup_error)?;
        if self.tables.contains_key(&schema.name) {
            return Err(SqlSurfaceError::duplicate_table(schema.name.clone()));
        }
        if self.tables.len() >= max_tables {
            return Err(SqlSurfaceError::payload_too_large(
                "too many temporary tables in this session",
            ));
        }
        let id = next_temp_relation_id()?;
        self.bump_epoch()?;
        let name = schema.name.clone();
        self.tables.insert(
            name,
            TempTableEntry {
                id,
                schema: Arc::new(schema),
                on_commit,
            },
        );
        Ok(id)
    }

    /// 一時表を取り除き、その識別子を返す（無ければ `None`）。呼び出し元（#1683 の DROP）は
    /// 返った識別子で行ストアの `drop_relation` を呼び、行領域を解放する。
    pub(crate) fn remove(&mut self, name: &str) -> Option<TempRelationId> {
        let removed = self.tables.remove(name).map(|e| e.id);
        if removed.is_some() {
            // 世代が尽きた場合は 0 にする（0 は採番されないため、どの準備済み文の世代とも
            // 一致せず陳腐化として拒否される側＝fail-closed）。
            self.epoch = next_catalog_epoch().unwrap_or(0);
        }
        removed
    }
}

/// parse 中に引かれた関係の種類の記録（[`SessionRelationLookup`] の記録先）。一時表に当たったか、
/// 永続の関係（表・ビュー）に当たったかを別々に持つ。
#[derive(Debug, Default)]
pub(crate) struct RelationHits {
    temp: Cell<bool>,
    persistent: Cell<bool>,
}

impl RelationHits {
    pub(crate) fn temp_hit(&self) -> bool {
        self.temp.get()
    }

    pub(crate) fn persistent_hit(&self) -> bool {
        self.persistent.get()
    }

    pub(crate) fn temp_cell(&self) -> &Cell<bool> {
        &self.temp
    }

    pub(crate) fn persistent_cell(&self) -> &Cell<bool> {
        &self.persistent
    }
}

/// `TableLookup` を一時カタログ優先で包むラッパー。一時表の名前は永続の表・ビューを
/// 隠す。一時カタログが空なら完全な素通し。一時表に当たったことを `hit` に、永続の関係
/// （表・ビュー）に当たったことを `persistent_hit`（[`Self::with_persistent_hit`] で指定した
/// 場合）に記録する。両方が立った文は一時表と永続の関係を混ぜており、`0A000` で拒否する
/// （2 つのストアを 1 つの読み取り元で扱えないため）。
pub(crate) struct SessionRelationLookup<'a, L: TableLookup + ?Sized> {
    inner: &'a L,
    temp: &'a SessionTempCatalog,
    hit: &'a Cell<bool>,
    persistent_hit: Option<&'a Cell<bool>>,
}

impl<'a, L: TableLookup + ?Sized> SessionRelationLookup<'a, L> {
    pub(crate) fn new(inner: &'a L, temp: &'a SessionTempCatalog, hit: &'a Cell<bool>) -> Self {
        Self {
            inner,
            temp,
            hit,
            persistent_hit: None,
        }
    }

    /// 永続の関係に当たったことの記録先を指定する。
    #[must_use]
    pub(crate) fn with_persistent_hit(mut self, persistent_hit: &'a Cell<bool>) -> Self {
        self.persistent_hit = Some(persistent_hit);
        self
    }

    fn note_persistent(&self) {
        if let Some(cell) = self.persistent_hit {
            cell.set(true);
        }
    }

    fn temp_entry(&self, name: &str) -> Option<&'a TempTableEntry> {
        let entry = self.temp.get(name)?;
        self.hit.set(true);
        Some(entry)
    }
}

impl<L: TableLookup + ?Sized> TableLookup for SessionRelationLookup<'_, L> {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        if self.temp_entry(name).is_some() {
            return Ok(true);
        }
        let exists = self.inner.table_exists(name)?;
        if exists {
            self.note_persistent();
        }
        Ok(exists)
    }

    fn view_definition(
        &self,
        name: &str,
    ) -> Result<Option<crate::catalog::ViewDef>, SqlSurfaceError> {
        if self.temp_entry(name).is_some() {
            // 一時表は同名の永続ビューを隠す。
            return Ok(None);
        }
        let view = self.inner.view_definition(name)?;
        if view.is_some() {
            // 永続ビューの本文は、この一時カタログ優先の lookup で再検証される。本文が一時表と
            // 同名の永続表を指していると一時表に解決されうるため、ビューに当たった時点で
            // 「永続の関係に当たった」と記録し、本文が一時表を引いた場合は混在として拒否させる
            // （永続ビューが一時表を読む fail-open を作らない）。
            self.note_persistent();
        }
        Ok(view)
    }

    fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        if let Some(entry) = self.temp_entry(name) {
            return Ok(Some(crate::catalog::lookup_columns_of(&entry.schema)));
        }
        let columns = self.inner.table_columns(name)?;
        if columns.is_some() {
            self.note_persistent();
        }
        Ok(columns)
    }

    fn table_star_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        if let Some(entry) = self.temp_entry(name) {
            return Ok(Some(crate::catalog::lookup_star_columns_of(&entry.schema)));
        }
        let columns = self.inner.table_star_columns(name)?;
        if columns.is_some() {
            self.note_persistent();
        }
        Ok(columns)
    }

    fn table_is_allocated(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        if let Some(entry) = self.temp_entry(name) {
            return Ok(entry.schema.is_allocated());
        }
        let allocated = self.inner.table_is_allocated(name)?;
        if allocated {
            self.note_persistent();
        }
        Ok(allocated)
    }

    fn table_has_non_byte_collation(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        if let Some(entry) = self.temp_entry(name) {
            return Ok(crate::catalog::schema_has_non_byte_collation(&entry.schema));
        }
        self.inner.table_has_non_byte_collation(name)
    }

    fn buffered_depth(&self) -> usize {
        self.inner.buffered_depth()
    }

    fn materialized_view_columns(
        &self,
        name: &str,
    ) -> Result<Option<Vec<crate::catalog::ColumnDef>>, SqlSurfaceError> {
        if self.temp_entry(name).is_some() {
            // 一時表は同名の永続 matview を隠す。
            return Ok(None);
        }
        self.inner.materialized_view_columns(name)
    }
}

/// `SET`／`RESET`／`SHOW` の対象が `search_path` か（先頭 3 トークンの構造だけで判定する）。
fn is_search_path_statement(tokens: &[Token]) -> bool {
    let word = |i: usize, k: &str| matches!(tokens.get(i), Some(Token::Ident(w)) if w.eq_ignore_ascii_case(k));
    (word(0, "SET") || word(0, "RESET") || word(0, "SHOW"))
        && (word(1, "search_path")
            || ((word(1, "SESSION") || word(1, "LOCAL")) && word(2, "search_path")))
}

/// トークン列に、一時表の名前と ASCII 大小無視で一致する識別子があるか。一致しすぎても
/// 拒否側（fail-closed）に倒れるだけである。一時カタログが空なら常に `false`。
pub(crate) fn tokens_reference_temp(tokens: &[Token], catalog: &SessionTempCatalog) -> bool {
    if catalog.is_empty() {
        return false;
    }
    // `SET`／`RESET`／`SHOW [SESSION|LOCAL] search_path` の値は SCHEMA 名で関係名ではない。
    // 一時表と同名の要素（や `set`・`to`）で誤って一時表参照と判定しない（TABLE-27・Issue #1715）。
    if is_search_path_statement(tokens) {
        return false;
    }
    tokens.iter().any(|t| match t {
        Token::Ident(s) | Token::QuotedBoolWord(s) | Token::QuotedNull(s) => {
            catalog.names().any(|n| n.eq_ignore_ascii_case(s))
        }
        _ => false,
    })
}

/// `public.<一時表の名前>` を含む文を拒否する（TABLE-27・Issue #1714）。`public.` で修飾した名前は
/// 永続側の関係を指すが、解決後は未修飾名と同一になり一時表（検索順で優先）に当たってしまう。
/// `DROP TABLE public.t` が一時表を落とす等の取り違えを避けるため、解決より前に固定の `0A000`
/// （[`temp_table_unsupported`]）で拒否する（fail-closed）。一時カタログが空なら常に `Ok`。
pub(crate) fn reject_public_qualified_temp(
    tokens: &[Token],
    catalog: &SessionTempCatalog,
) -> Result<(), SqlSurfaceError> {
    if catalog.is_empty() {
        return Ok(());
    }
    // 列修飾（`public.name` を別名 `public` の列参照として使う等）を誤って拒否しないよう、
    // 名前解決と同じ表参照位置の判定（`rewrite_relation_names`）で `public.<表名>` と確認できた
    // ものだけを照合する。解決エラーは後続の parse が同じ入力で返すため、ここでは拒否しない。
    let has_public = tokens.iter().any(|t| {
        matches!(t, Token::QualifiedIdent { qualifier, .. } if qualifier.eq_ignore_ascii_case("public"))
    });
    if !has_public {
        return Ok(());
    }
    let mut resolved = tokens.to_vec();
    let Ok(rewritten) = crate::sql::relation_name::rewrite_relation_names(&mut resolved, None)
    else {
        return Ok(());
    };
    let public_names = rewritten.public_names;
    let hit = public_names
        .iter()
        .any(|p| catalog.names().any(|n| n.eq_ignore_ascii_case(p)));
    if hit {
        return Err(temp_table_unsupported());
    }
    Ok(())
}

/// Parse 時に `public.` 修飾を解決済みの表名（[`crate::sql::relation_name::rewrite_relation_names`]
/// の戻り値）が、現在の一時表と衝突するなら固定の `0A000` で拒否する。準備済み文は Parse 後に
/// 同名の一時表が作られうるため、Bind・Describe の段で再検査する（fail-closed）。
pub(crate) fn reject_public_names_temp(
    public_names: &[String],
    catalog: &SessionTempCatalog,
) -> Result<(), SqlSurfaceError> {
    if public_names.is_empty() || catalog.is_empty() {
        return Ok(());
    }
    let hit = public_names
        .iter()
        .any(|p| catalog.names().any(|n| n.eq_ignore_ascii_case(p)));
    if hit {
        return Err(temp_table_unsupported());
    }
    Ok(())
}

/// 一時表を扱えない文（混在・対象外の文種）に返す固定の拒否（`0A000`）。永続側の表の存否を
/// 含まない固定文言で、存在オラクルにならない。
pub(crate) fn temp_table_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("this statement is not supported on temporary tables")
}

/// 陳腐化した準備済み文（一時カタログが変わった後に実行された）に返す固定の拒否（`0A000`）。
pub(crate) fn temp_statement_stale() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "the temporary table definitions changed since this statement was prepared",
    )
}
