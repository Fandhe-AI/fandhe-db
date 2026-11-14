//! トリガ本体が行を書くための狭い窓口（TABLE-45・Issue #1724。実行モデルの D5・D9・D10）。
//!
//! 発火側（`tenant/trigger_insert.rs`）が、外側の文と同じ write txn・同じ呼び出しテナントの
//! `PolicyContext` を借用して作り、[`TriggerCall`](super::call::TriggerCall) に載せてトリガ関数の実体へ
//! 渡す。実体は raw な `redb::WriteTransaction` を持たず、この窓口の [`TriggerWriteHandle::insert_row`]・
//! `run_insert`・`run_delete`（Issue #3187）だけで書き、`with_read_source`（同 Issue）の読み取り専用の源だけで読む。
//!
//! 契約（fail-closed）:
//! - 書き込みは SQL `INSERT` と同じ `tenant::insert_typed_row_unchecked` を通る（第 2 の書き込み経路を
//!   作らない）。呼び出し元の `PolicyContext` を差し替える手段は無く、行キーは常に呼び出しテナントの
//!   `(tenant_id, rowid)` になる（他テナントの行は構造上書けない）
//! - 台帳は `Disabled`（外側の文の 1 エントリだけが台帳に載る。D10。PL/pgSQL 本体の実行
//!   〔`core::plpgsql_host`。Issue #2226〕との共通化は #2228 の担当）
//! - 書き込み先の表に INSERT トリガがあれば入れ子で発火する。深さは `trigger::depth` が数え、上限超過は
//!   `54001`（Issue #1727）。通過証の無い（未結線の）経路では `0A000`
//! - 表名は裸の識別子だけ受理し、SCHEMA 配下の内部キー（`#<id>.<name>`）は不在と同じ応答で拒否する
//!   （内部 ID から他テナントの SCHEMA の表を指せない。本番の SCHEMA 修飾の解決は #2228）
//! - 書いた表名を記録し、発火側が遅延 FK の検査対象にする
//!
//! 本番の結線（トリガ関数の本体の SQL をこの窓口経由で実行する経路）は Issue #3184 の
//! `core::plpgsql_trigger_host`（`run_insert` で `INSERT ... VALUES` を、Issue #3186 で `run_write` から
//! `UPDATE` も同じ契約で、`run_delete` で `DELETE` を、`with_read_source` で表を読む `SELECT` を、
//! `function_def`・`table_schema` で同じ txn のカタログを読む。外側の行ループが進行中の表への `DELETE` は
//! `trigger::row_loop` の暫定ガードで拒否する。通常の関数・`DO` の本体を同じ Tx で実行するホストは
//! Issue #2226 の `core::plpgsql_host`）。

use std::cell::RefCell;

use crate::catalog::TableSchema;
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::storage::read_source::ReadSource;
use crate::tenant::{TenantWriteError, WriteTarget};

/// トリガ本体の読み取り専用の読み取り源（外側の write txn の薄い包み。Issue #3187）。
/// [`ReadSource`] だけを公開し、書き込みハンドルへ到達できない（変更ログの捕捉を経ない裏口にならない）。
/// 確定済みスナップショットではないため `snapshot()` は `None` で、キャッシュ経路へは落ちない。
pub(crate) struct TriggerReadSource<'a>(&'a redb::WriteTransaction);

impl ReadSource for TriggerReadSource<'_> {
    type Table<'s, K: redb::Key + 'static, V: redb::Value + 'static>
        = <redb::WriteTransaction as ReadSource>::Table<'s, K, V>
    where
        Self: 's;

    fn open_table<'s, K: redb::Key + 'static, V: redb::Value + 'static>(
        &'s self,
        def: redb::TableDefinition<'_, K, V>,
    ) -> Result<Self::Table<'s, K, V>, redb::TableError> {
        ReadSource::open_table(self.0, def)
    }

    fn snapshot(&self) -> Option<&redb::ReadTransaction> {
        None
    }
}

/// トリガ内書き込みの窓口。フィールドはすべて非公開で、構築は発火側（`tenant`）だけが行う。
pub(crate) struct TriggerWriteHandle<'a> {
    write_txn: &'a redb::WriteTransaction,
    ctx: &'a PolicyContext,
    /// 入れ子の発火に使う通過証。`None`（未結線の経路）なら入れ子は従来どおり `0A000`。
    runtime: Option<&'a super::TriggerRuntime<'a>>,
    written_tables: RefCell<Vec<String>>,
}

impl<'a> TriggerWriteHandle<'a> {
    /// 外側の文の write txn と呼び出しテナントの文脈を借用して作る。`runtime` は外側の文が
    /// 使った通過証で、トリガ本体の書き込みが別のトリガを発火させるときに引き継ぐ（Issue #1727）。
    pub(crate) fn new(
        write_txn: &'a redb::WriteTransaction,
        ctx: &'a PolicyContext,
        runtime: Option<&'a super::TriggerRuntime<'a>>,
    ) -> Self {
        Self {
            write_txn,
            ctx,
            runtime,
            written_tables: RefCell::new(Vec::new()),
        }
    }

    /// 表 `table`（裸の識別子）へ 1 行（`values` は `schema.columns` と同じ幅・順）を、呼び出し
    /// テナントの行として書く。`id` は従来方式の表の利用者指定 id（採番方式・移行済みの表では
    /// `None` で払い出す）。制約検査・索引同期・世代の更新は SQL `INSERT` と同じ経路で行う。
    // 本番のホスト（Issue #3184・#3185）は本文の INSERT を SQL 経路で実行するため、この窓口の呼び出し元は
    // 他表への書き込みを注入するテストだけ。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert_row(
        &self,
        table: &str,
        id: Option<u64>,
        values: &[Value],
    ) -> Result<(), SqlSurfaceError> {
        if table.starts_with('#') {
            // 内部キーは窓口から受けない（不在と同じ応答）。
            return Err(SqlSurfaceError::UndefinedTable {
                name: table.to_string(),
            });
        }
        let schema: TableSchema =
            crate::catalog::require_table_schema_write_any_identity(self.write_txn, table)
                .map_err(|e| crate::sql::exec::map_insert_write_error(TenantWriteError::from(e)))?;
        let (row_id, id_omitted) = match (id, schema.is_migrated_id(), schema.is_allocated()) {
            // 採番方式は利用者指定の id を持たない（呼び出し元の id は仮値）。
            (_, _, true) => (0, false),
            (Some(id), _, false) => (id, false),
            (None, true, false) => (0, true),
            (None, false, false) => {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: "row id is required for this table".to_string(),
                })
            }
        };
        crate::tenant::insert_typed_row_unchecked(
            WriteTarget::InTxn(self.write_txn),
            table,
            self.ctx,
            row_id,
            crate::storage::Visibility::Private,
            values,
            crate::recovery::ledger::LedgerWrite::Disabled,
            None,
            &[],
            id_omitted,
            // 入れ子で発火する。深さは `trigger::depth` が数え、超過は `54001`。通過証が無い
            // （未結線の）経路では書き込み先に INSERT トリガがあれば `0A000`（fail-closed）。
            self.runtime,
        )
        .map_err(crate::sql::exec::map_insert_write_error)?;
        let mut written = self.written_tables.borrow_mut();
        if !written.iter().any(|t| t == table) {
            written.push(table.to_string());
        }
        Ok(())
    }

    /// 呼び出しテナントの文脈（読み取り専用の借用。差し替える手段は無い）。トリガ用ホストが、主体の写しの
    /// 解決テナントと照合するのと、`CorePlHost` の `ctx` に使う（Issue #3184）。
    pub(crate) fn policy_context(&self) -> &'a PolicyContext {
        self.ctx
    }

    /// 呼び出しテナントの scope から、同じ write txn 内で関数定義を 1 件引く（引数なしの関数。トリガ関数は
    /// 引数を持たない）。不在・他テナントの関数は区別のない `None`（scope が構造上テナントに固定される）。
    /// カタログの I/O 失敗は固定文言の `Internal` に丸める（Issue #3184）。
    pub(crate) fn function_def(
        &self,
        function_key: &str,
    ) -> Result<Option<crate::catalog::function::FunctionDef>, SqlSurfaceError> {
        crate::catalog::function::get_function_in_txn(
            self.write_txn,
            self.ctx.tenant_id(),
            function_key,
            "",
        )
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "failed to load trigger function".to_string(),
        })
    }

    /// 表 `table` のスキーマを、外側と同じ write txn から読む（同じ Tx 内の DDL を取り込む。`insert_row` と同じ
    /// 読み方）。トリガ本体の `INSERT ... VALUES` の束縛に使う（Issue #3184）。
    pub(crate) fn table_schema(&self, table: &str) -> Result<TableSchema, SqlSurfaceError> {
        crate::catalog::require_table_schema_write_any_identity(self.write_txn, table)
            .map_err(|e| crate::sql::exec::map_insert_write_error(TenantWriteError::from(e)))
    }

    /// 外側の write txn を書き込み先にして `f` を実行し、成功したら `table`（書き込みの対象表）を書いた表として
    /// 記録する（Issue #3184・#3186）。トリガ本体の `INSERT ... VALUES`・`UPDATE` が SQL の同じ文と同じ実行本体
    /// （`sql::exec` の `execute_*_in`）を通すための口で、raw な write txn は `f` の中の `WriteTarget` としてだけ
    /// 見える。記録は遅延 FK の確定対象・世代の更新・明示 Tx の書き込み印に使われる（漏れると fail-open）。
    /// 台帳は呼び出し側が `CompareOnlyWithoutLedger` を明示して記録させない（D10）。
    pub(crate) fn run_write<T>(
        &self,
        table: &str,
        f: impl FnOnce(WriteTarget<'_>) -> Result<T, SqlSurfaceError>,
    ) -> Result<T, SqlSurfaceError> {
        let out = f(WriteTarget::InTxn(self.write_txn))?;
        self.note_written(table);
        Ok(out)
    }

    /// [`Self::run_insert`] の `DELETE` 版（Issue #3187）。契約は同じで、成功時に `table`（`DELETE` の対象表）を
    /// 書いた表として記録する。親行の削除で子が宙に浮く遅延 FK を COMMIT 時（Autocommit は文の終わり）に
    /// 検査させるための記録で、漏れると fail-open になる。
    pub(crate) fn run_delete<T>(
        &self,
        table: &str,
        f: impl FnOnce(WriteTarget<'_>) -> Result<T, SqlSurfaceError>,
    ) -> Result<T, SqlSurfaceError> {
        let out = f(WriteTarget::InTxn(self.write_txn))?;
        self.note_written(table);
        Ok(out)
    }

    /// 外側と同じ write txn を読み取り源として `f` を実行する（トリガ本体の表を読む `SELECT`。Issue #3187）。
    /// 自 txn の未 commit の変更（外側の文が先に書いた行を含む）が見える。`f` へ渡すのは読み取り専用の
    /// [`TriggerReadSource`] だけで、raw な `WriteTransaction` は出さない。
    pub(crate) fn with_read_source<T>(
        &self,
        f: impl FnOnce(&TriggerReadSource<'_>) -> Result<T, SqlSurfaceError>,
    ) -> Result<T, SqlSurfaceError> {
        f(&TriggerReadSource(self.write_txn))
    }

    /// `table` の行の `DELETE` が参照アクション（`CASCADE`／`SET NULL`／`SET DEFAULT`）で波及しうる表のうち、
    /// `is_blocked` が真になるものがあるか（行ループ進行中の表の検出に使う。Issue #3187 の D6 ガードの拡張）。
    /// 参照元 FK を外側と同じ write txn のカタログから推移的に辿る（`NoAction` の FK は波及しない）。循環は
    /// 訪問済み集合で止める。カタログを読めない場合は検出できないので `Err`（呼び出し側は fail-closed で拒否する）。
    pub(crate) fn delete_reaches_table(
        &self,
        table: &str,
        is_blocked: impl Fn(&str) -> bool,
    ) -> Result<bool, SqlSurfaceError> {
        let mut seen: Vec<String> = vec![table.to_ascii_lowercase()];
        let mut stack: Vec<String> = vec![table.to_string()];
        while let Some(parent) = stack.pop() {
            let referencing =
                crate::catalog::referencing_foreign_keys_in_txn(self.write_txn, &parent).map_err(
                    |e| crate::sql::exec::map_insert_write_error(TenantWriteError::from(e)),
                )?;
            for (child, fk) in referencing {
                if matches!(fk.on_delete(), crate::catalog::ReferentialAction::NoAction) {
                    continue;
                }
                if is_blocked(&child.name) {
                    return Ok(true);
                }
                let key = child.name.to_ascii_lowercase();
                if !seen.contains(&key) {
                    seen.push(key);
                    stack.push(child.name.clone());
                }
            }
        }
        Ok(false)
    }

    /// 書いた表として記録する（重複なし。書いた順）。
    fn note_written(&self, table: &str) {
        let mut written = self.written_tables.borrow_mut();
        if !written.iter().any(|t| t == table) {
            written.push(table.to_string());
        }
    }

    /// [`Self::run_write`] の `INSERT` 向けの別名（Issue #3184 からの呼び出し元を保つ）。
    pub(crate) fn run_insert<T>(
        &self,
        table: &str,
        f: impl FnOnce(WriteTarget<'_>) -> Result<T, SqlSurfaceError>,
    ) -> Result<T, SqlSurfaceError> {
        self.run_write(table, f)
    }

    /// これまでにトリガが書いた表名（重複なし。書いた順）。
    pub(crate) fn written_tables(&self) -> Vec<String> {
        self.written_tables.borrow().clone()
    }
}
