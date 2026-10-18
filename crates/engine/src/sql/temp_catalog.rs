//! セッション単位の一時カタログと、一時表を永続表より先に解決する名前解決層
//! （TABLE-44・Issue #1681。親 #1680 の TEMP TABLE 系列の土台）。
//!
//! # 責務境界
//!
//! - `SessionTempCatalog`: 接続（セッション）ごとの一時表のスキーマ台帳。
//!   [`crate::sql::mode::SessionState`] が保持するため、他接続・他テナントへ漏れる経路は
//!   構造上存在しない。永続化しない。行データは持たない（行は `temp_rows` の行ストアが持つ）。
//! - `ResolvedRelation`・`SessionRelationLookup`: 修飾なしの名前を「一時カタログ →
//!   永続カタログ」の順に解決する。`sql::allowlist::TableLookup` を包むラッパーで、trait
//!   自体は変更しない。呼び出し元は `core.rs` の `EngineCore::parse_tokens_in`。
//! - `tokens_reference_temp`: トークン列が一時表の名前を参照しうるかの保守的な判定。
//!   カタログを引かない文（DROP・ALTER・CREATE INDEX 等）や永続ビュー本文の再検証も覆う。
//!
//! # 暫定ゲート（#1684 で置換）
//!
//! 一時表への DML／SELECT の実行器はまだ無い。実行すると同名の**永続表**へ作用してしまうため、
//! 一時カタログが空でないセッションで一時表を参照する文は `core.rs` の入口が `0A000` で
//! 拒否する（fail-closed）。一時カタログが空なら（一時表を作っていないセッション）ゲートは
//! 何もせず、既存の挙動と同一である。`CREATE [TEMP] TABLE`・`DROP TABLE` だけは一時表の行を
//! 読み書きせず、実行段が自セッションの一時カタログで名前を解決するため例外として通す
//! （`docs/design/temp-table.md`「Issue #1683」）。
//!
//! # 後続 Issue への申し送り
//!
//! 行ストアと切断時の破棄は `temp_rows`（#1682 で実装済み）、`CREATE TEMP TABLE` と
//! `ON COMMIT`（`SessionState::apply_temp_on_commit`）・一時表の `DROP` は #1683 で実装済み、
//! 一時表への DML・索引・制約とゲートの解除は #1684、テナント境界・crash 耐性の検査は #1685。
//! 修飾名（`pg_temp.<名前>`）はスキーマ・`search_path`（TABLE-27）の実装へ申し送る。

// 名前解決（`resolve_unqualified`）・`from_raw` 等は一時表への DML／SELECT の実行腕（#1684）が
// 使う。本 Issue（#1683）の時点ではテストからのみ到達するため dead_code 警告を抑止する。
#![allow(dead_code)]

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
pub(crate) const DEFAULT_MAX_TEMP_TABLES_PER_SESSION: usize = 64;

/// 一時表のプロセス内で一意な識別子。永続化しない（再起動で採番し直す）。
/// `temp_rows` の行領域のキーに使い、同名の再作成と区別するために登録ごとに採番する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TempRelationId(u64);

impl TempRelationId {
    pub(crate) fn get(self) -> u64 {
        self.0
    }

    /// 行ストア（`temp_rows`）が行領域のテーブル名から復元した値を戻すための生成口。
    /// 採番は `register` だけが行う（ここで作った値をカタログへ登録する経路は無い）。
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

static NEXT_TEMP_RELATION_ID: AtomicU64 = AtomicU64::new(1);

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
}

impl SessionTempCatalog {
    pub(crate) fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.tables.len()
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
        self.tables.remove(name).map(|e| e.id)
    }
}

/// 修飾なしの名前の解決結果。`written_tables` 等が裸の `String` を使うため、
/// 同名の一時表と永続表を取り違えないための型境界とする（#1684 が使う）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResolvedRelation<'a> {
    Temp(&'a TempTableEntry),
    Persistent(&'a str),
}

impl SessionTempCatalog {
    /// 修飾なしの名前を解決する。一時カタログに有れば `Temp`、無ければ `Persistent`
    /// （永続側の存否判定は従来の経路に任せる）。
    pub(crate) fn resolve_unqualified<'a>(&'a self, name: &'a str) -> ResolvedRelation<'a> {
        match self.tables.get(name) {
            Some(entry) => ResolvedRelation::Temp(entry),
            None => ResolvedRelation::Persistent(name),
        }
    }
}

/// `TableLookup` を一時カタログ優先で包むラッパー。一時表の名前は永続の表・ビューを
/// 隠す。一時カタログが空なら完全な素通し。一時表に当たったことを `hit` に記録する。
pub(crate) struct SessionRelationLookup<'a, L: TableLookup + ?Sized> {
    inner: &'a L,
    temp: &'a SessionTempCatalog,
    hit: &'a Cell<bool>,
}

impl<'a, L: TableLookup + ?Sized> SessionRelationLookup<'a, L> {
    pub(crate) fn new(inner: &'a L, temp: &'a SessionTempCatalog, hit: &'a Cell<bool>) -> Self {
        Self { inner, temp, hit }
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
        self.inner.table_exists(name)
    }

    fn view_definition(
        &self,
        name: &str,
    ) -> Result<Option<crate::catalog::ViewDef>, SqlSurfaceError> {
        if self.temp_entry(name).is_some() {
            // 一時表は同名の永続ビューを隠す。
            return Ok(None);
        }
        self.inner.view_definition(name)
    }

    fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        if let Some(entry) = self.temp_entry(name) {
            return Ok(Some(crate::catalog::lookup_columns_of(&entry.schema)));
        }
        self.inner.table_columns(name)
    }

    fn table_star_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        if let Some(entry) = self.temp_entry(name) {
            return Ok(Some(crate::catalog::lookup_star_columns_of(&entry.schema)));
        }
        self.inner.table_star_columns(name)
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

/// トークン列に、一時表の名前と ASCII 大小無視で一致する識別子があるか。一致しすぎても
/// 拒否側（fail-closed）に倒れるだけである。一時カタログが空なら常に `false`。
pub(crate) fn tokens_reference_temp(tokens: &[Token], catalog: &SessionTempCatalog) -> bool {
    if catalog.is_empty() {
        return false;
    }
    tokens.iter().any(|t| match t {
        Token::Ident(s) | Token::QuotedBoolWord(s) | Token::QuotedNull(s) => {
            catalog.names().any(|n| n.eq_ignore_ascii_case(s))
        }
        _ => false,
    })
}

/// 暫定ゲートが返す固定の拒否（`0A000`）。#1684 で一時表の実行器ができるまで使う。
pub(crate) fn temp_table_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("temporary tables are not supported yet")
}
