//! 読み取り経路の入力源の抽象 [`ReadSource`]（SQL-31・TASK-221、Issue #1179）。
//!
//! 明示トランザクション内の読み取りが自トランザクションの未 commit 変更を見られる
//! ようにするための最小の抽象。redb の確定済みスナップショット
//! （[`redb::ReadTransaction`]）と、明示トランザクションが保持する共有書き込み
//! トランザクション（[`redb::WriteTransaction`]。自身の未 commit 書き込みを読める）
//! の双方を、`sql::scan`・`sql::aggregate`・`sql::join`・`arena`・`rls`・`catalog` の
//! 読み取り本体が同じコードで扱えるようにする。呼び出し元は
//! `core::EngineCore::execute_in_active_txn`（dirty テーブルがあるとき書き込み
//! トランザクション、なければ従来どおり確定済みスナップショットを渡す）と、
//! 従来の autocommit 読み取り経路（常に確定済みスナップショット。挙動不変）。
//!
//! ## キャッシュの構造的ゲート（P0）
//!
//! テーブル世代をキーにしたキャッシュ（`arena_cache`・`hnsw_cache`・`sparse_cache`・
//! `visible_cache`・`scalar_index`）は、未 commit の内容から作ったエントリを
//! 共有してはならない。ROLLBACK したトランザクションのテーブル世代は、次に
//! commit される別の書き込みで再利用されるため、未 commit の行から作った
//! エントリが後で確定済みデータとして別セッション（別テナントを含む）へ返る
//! 恐れがあるためである。そこで各キャッシュ経路は具体型 `&redb::ReadTransaction`
//! のまま残し、[`ReadSource::snapshot`] が `Some`（＝確定済みスナップショット）の
//! ときだけ呼べる構造にする。`WriteTransaction` では `None` を返し、読み取り本体は
//! キャッシュなしの brute-force 経路へ落ちる。
//!
//! ## 同一テーブルの二重オープン
//!
//! 書き込みトランザクションでは、同一テーブルへ同時に 2 つ目のハンドルを開くと
//! `TableAlreadyOpen` になる（確定済みスナップショットでは起きない）。読み取り本体は
//! ハンドルを重ねない構造になっている: サブクエリは外側の走査より前に解決し終える、
//! 自己 JOIN は `relation_snapshot` がテーブル名で 1 つのスナップショットを共有する、
//! 書き込みトランザクション由来では投影の遅延（`DeferredScalars::Redb` による行
//! テーブルの再オープン）を使わない。万一重なった場合は `Internal`（fail-closed）に
//! 写像され、トランザクションは `Failed` になる。
//!
//! ## 副作用の禁止
//!
//! redb の `WriteTransaction::open_table` は存在しないテーブルを**作成**する。読み取りの
//! つもりで空の索引・台帳テーブルが作られ COMMIT で永続化されるのを避けるため、
//! `WriteTransaction` 実装は `list_tables` で存在を確認してから開き、無ければ
//! `TableError::TableDoesNotExist` を返す（`ReadTransaction` と同じ契約）。

use redb::{Key, ReadableTable, TableDefinition, TableError, Value};

/// 読み取り本体が行テーブル・カタログ・索引テーブルを開くための入力源。
/// 実装は [`redb::ReadTransaction`]（確定済みスナップショット）と
/// [`redb::WriteTransaction`]（自トランザクションの未 commit 書き込みを含む）。
///
/// 公開関数（`sql::scan::execute_scan` 等）の引数境界に現れるが、本モジュールが
/// クレート内限定（`pub(crate) mod`）のためクレート外からは名前を指定できず、
/// 実装を追加することもできない（実質的に封印された trait。従来どおり
/// `&redb::ReadTransaction` をそのまま渡せる）。
pub trait ReadSource {
    /// 開いたテーブルの型（読み取り専用のハンドルとして [`ReadableTable`] のみを使う）。
    type Table<'s, K: Key + 'static, V: Value + 'static>: ReadableTable<K, V>
    where
        Self: 's;

    /// `def` のテーブルを読み取り専用で開く。存在しなければ
    /// [`TableError::TableDoesNotExist`]（書き込みトランザクションでも作成しない）。
    fn open_table<'s, K: Key + 'static, V: Value + 'static>(
        &'s self,
        def: TableDefinition<'_, K, V>,
    ) -> Result<Self::Table<'s, K, V>, TableError>;

    /// 確定済みスナップショットなら `Some`。キャッシュ（テーブル世代キー）を使って
    /// よいのは `Some` のときだけ（モジュールドキュメント「キャッシュの構造的ゲート」）。
    fn snapshot(&self) -> Option<&redb::ReadTransaction>;
}

impl ReadSource for redb::ReadTransaction {
    type Table<'s, K: Key + 'static, V: Value + 'static>
        = redb::ReadOnlyTable<K, V>
    where
        Self: 's;

    fn open_table<'s, K: Key + 'static, V: Value + 'static>(
        &'s self,
        def: TableDefinition<'_, K, V>,
    ) -> Result<Self::Table<'s, K, V>, TableError> {
        redb::ReadTransaction::open_table(self, def)
    }

    fn snapshot(&self) -> Option<&redb::ReadTransaction> {
        Some(self)
    }
}

impl ReadSource for redb::WriteTransaction {
    type Table<'s, K: Key + 'static, V: Value + 'static>
        = redb::Table<'s, K, V>
    where
        Self: 's;

    fn open_table<'s, K: Key + 'static, V: Value + 'static>(
        &'s self,
        def: TableDefinition<'_, K, V>,
    ) -> Result<Self::Table<'s, K, V>, TableError> {
        use redb::TableHandle;
        // 存在しないテーブルを `open_table` が作成してしまう副作用を避ける
        // （モジュールドキュメント「副作用の禁止」）。
        let exists = self
            .list_tables()
            .map_err(TableError::Storage)?
            .any(|handle| handle.name() == def.name());
        if !exists {
            return Err(TableError::TableDoesNotExist(def.name().to_string()));
        }
        redb::WriteTransaction::open_table(self, def)
    }

    fn snapshot(&self) -> Option<&redb::ReadTransaction> {
        None
    }
}

/// キャッシュ経路のように確定済みスナップショットを必須とする箇所向けの取得口。
/// 書き込みトランザクション由来（[`ReadSource::snapshot`] が `None`）では、呼び出し元が
/// キャッシュ群をすべて `None` に落とすため本関数は呼ばれない。万一呼ばれた場合は
/// 未 commit 変更由来のデータをキャッシュへ載せないよう fail-closed に `Internal` を
/// 返す（モジュールドキュメント「キャッシュの構造的ゲート」）。
pub(crate) fn require_snapshot<R: ReadSource + ?Sized>(
    source: &R,
) -> Result<&redb::ReadTransaction, crate::sql::allowlist::SqlSurfaceError> {
    source
        .snapshot()
        .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    const PROBE: TableDefinition<&str, u64> = TableDefinition::new("read_source_probe");
    const OTHER: TableDefinition<&str, u64> = TableDefinition::new("read_source_other");

    fn open_db(label: &str) -> (redb::Database, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        (redb::Database::create(&path).expect("create db"), guard)
    }

    fn read_value<R: ReadSource>(source: &R, key: &str) -> Option<u64> {
        let table = source.open_table(PROBE).expect("open probe");
        let value = table.get(key).expect("get").map(|g| g.value());
        value
    }

    /// 書き込みトランザクションは自身の未 commit 書き込みを読め、確定済み
    /// スナップショットには見えない。
    #[test]
    fn write_txn_source_sees_uncommitted_rows_and_snapshot_does_not() {
        let (db, _guard) = open_db("read-source-visibility");
        {
            let txn = db.begin_write().expect("begin");
            {
                let mut t = txn.open_table(PROBE).expect("open");
                t.insert("a", 1).expect("insert");
            }
            txn.commit().expect("commit");
        }
        let write_txn = db.begin_write().expect("begin write");
        {
            let mut t = write_txn.open_table(PROBE).expect("open");
            t.insert("b", 2).expect("insert");
        }
        assert_eq!(read_value(&write_txn, "a"), Some(1));
        assert_eq!(read_value(&write_txn, "b"), Some(2));
        assert!(ReadSource::snapshot(&write_txn).is_none());

        let read_txn = db.begin_read().expect("begin read");
        assert_eq!(read_value(&read_txn, "a"), Some(1));
        assert_eq!(read_value(&read_txn, "b"), None);
        assert!(ReadSource::snapshot(&read_txn).is_some());
    }

    /// 書き込みトランザクション経由の読み取りは、存在しないテーブルを作成しない
    /// （`TableDoesNotExist`）。COMMIT 後もテーブルは存在しない。
    #[test]
    fn write_txn_source_does_not_create_missing_tables() {
        let (db, _guard) = open_db("read-source-no-create");
        let write_txn = db.begin_write().expect("begin write");
        match ReadSource::open_table(&write_txn, OTHER) {
            Err(TableError::TableDoesNotExist(name)) => assert_eq!(name, "read_source_other"),
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("a missing table must not be opened"),
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        assert!(matches!(
            read_txn.open_table(OTHER),
            Err(TableError::TableDoesNotExist(_))
        ));
    }
}
