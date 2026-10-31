//! SQL 標準形の関数本体（`RETURN`・`BEGIN ATOMIC`）の依存辺のカタログ（SQL-67 (2)・SQL-66 (3)・
//! Issue #2219。設計記録: `docs/design/udf-catalog.md`「依存辺」節）。
//!
//! 役割: 標準形の関数は定義時に名前を解決するため、参照先（表・ビュー・マテリアライズドビュー・
//! 別の関数）への依存を「関数 → 参照先」の辺として redb へ保存する。参照先の `DROP`・改名・列変更が
//! 辺を引いて `2BP01` で止める（呼び出し元は `catalog.rs` の各 DROP・ALTER 入口と
//! `catalog::function` の `drop_function_*`）。関数の削除（`remove_function_in_txn`）と置換は辺を
//! 掃除・張り替える。`LANGUAGE sql` の文字列本体と plpgsql は late binding のため辺を持たない。
//!
//! 保存形式（どちらも最初の書き込みまで作らない遅延作成。読み取りは `TableDoesNotExist` を空と扱う
//! ため、標準形を使わない DB のファイル構成は変わらない）:
//! - [`DEPS_BY_DEPENDEE`]: `(kind, 参照先キー, scope, 関数キー, シグネチャ)`。参照先からの全 scope の
//!   範囲走査（`DROP TABLE` の判定）に使う。
//! - [`DEPS_BY_FUNCTION`]: `(scope, 関数キー, シグネチャ, kind, 参照先キー)`。関数側の掃除と
//!   循環検査に使う。2 表は常に同じ write txn で同時に更新する。
//!
//! テナント境界（P0）: 表・ビューは全テナント共通のカタログなので、参照先からの検査は全 scope を
//! 見る。応答に出す名前は削除対象自身の名前だけで、依存元の関数名・scope・件数は含めない。関数への
//! 依存は同じ scope の中だけで解決する。decode の不整合は固定文言の `CorruptSchema`。

use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::{CatalogError, Result};

/// 参照先の種別タグ（表・ビュー・マテリアライズドビュー）。
pub(crate) const KIND_RELATION: u8 = 1;
/// 参照先の種別タグ（別の関数。同じ scope の関数キー）。
pub(crate) const KIND_FUNCTION: u8 = 2;

/// 参照先から引く辺の表。値は持たない（キーだけが情報）。
pub(crate) const DEPS_BY_DEPENDEE: TableDefinition<(u8, &str, &str, &str, &str), ()> =
    TableDefinition::new("function_deps_by_dependee");

/// 関数から引く辺の表。値は持たない。
pub(crate) const DEPS_BY_FUNCTION: TableDefinition<(&str, &str, &str, u8, &str), ()> =
    TableDefinition::new("function_deps_by_function");

/// 関数 1 件あたりの依存先の上限（確保前の検査。超過は `54000` へ写る `ViewLimitExceeded`）。
pub(crate) const MAX_FUNCTION_DEPENDENCIES: usize = 64;

/// 関数同士の依存を辿る探索の訪問数の上限（超過は循環とみなして拒否する。fail-closed）。
const MAX_DEPENDENCY_WALK: usize = 1024;

/// 参照先が `CREATE` と同じ txn で見つからなかったときの `Invalid` の固定文言（`sql::udf_ddl` が `42P01`
/// へ写す。定義時の解決と commit の間に参照先が消えた競合のときだけ到達する）。
pub(crate) const FUNCTION_DEPENDEE_MISSING_MESSAGE: &str = "function dependency target is missing";

/// 参照先の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DependencyKind {
    /// 表・ビュー・マテリアライズドビュー（全テナント共通のカタログのキー）。
    Relation,
    /// 同じ scope の別の関数（関数キー）。
    Function,
}

impl DependencyKind {
    fn tag(self) -> u8 {
        match self {
            Self::Relation => KIND_RELATION,
            Self::Function => KIND_FUNCTION,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            KIND_RELATION => Ok(Self::Relation),
            KIND_FUNCTION => Ok(Self::Function),
            _ => Err(corrupt()),
        }
    }
}

/// 関数 1 件から参照先 1 件への辺。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FunctionDependency {
    pub(crate) kind: DependencyKind,
    /// 参照先のキー（relation キー形、または関数キー。いずれも ASCII 小文字）。
    pub(crate) key: String,
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid function dependency".to_string())
}

fn is_missing(e: &redb::TableError) -> bool {
    matches!(e, redb::TableError::TableDoesNotExist(_))
}

/// `(scope, function_key, signature)` の関数が持つ辺を返す（`DEPS_BY_FUNCTION` の範囲走査。
/// 表が無ければ空）。件数は [`MAX_FUNCTION_DEPENDENCIES`] で打ち切り、超過は `CorruptSchema`。
pub(crate) fn dependencies_of_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    function_key: &str,
    signature: &str,
) -> Result<Vec<FunctionDependency>> {
    let table = match src.open_table(DEPS_BY_FUNCTION) {
        Ok(t) => t,
        Err(e) if is_missing(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((scope, function_key, signature, 0u8, "")..)? {
        let (k, _) = entry?;
        let (s, f, sig, kind, key) = k.value();
        if s != scope || f != function_key || sig != signature {
            break;
        }
        if out.len() >= MAX_FUNCTION_DEPENDENCIES {
            return Err(corrupt());
        }
        out.push(FunctionDependency {
            kind: DependencyKind::from_tag(kind)?,
            key: key.to_string(),
        });
    }
    Ok(out)
}

/// 関数 `(scope, function_key, signature)` の辺をすべて削除する（2 表とも。表が無ければ何もしない）。
/// 関数の削除・置換が呼ぶ。
pub(crate) fn remove_for_function_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    function_key: &str,
    signature: &str,
) -> Result<()> {
    let edges = dependencies_of_in_txn(write_txn, scope, function_key, signature)?;
    if edges.is_empty() {
        return Ok(());
    }
    let mut by_function = write_txn.open_table(DEPS_BY_FUNCTION)?;
    let mut by_dependee = write_txn.open_table(DEPS_BY_DEPENDEE)?;
    for e in &edges {
        by_function.remove((scope, function_key, signature, e.kind.tag(), e.key.as_str()))?;
        by_dependee.remove((e.kind.tag(), e.key.as_str(), scope, function_key, signature))?;
    }
    Ok(())
}

/// 関数 `(scope, function_key, signature)` の辺を `deps` へ置き換える（旧い辺を消してから記録する）。
/// 重複は 1 本にまとめる。件数は [`MAX_FUNCTION_DEPENDENCIES`] を確保前に検査する。`deps` が空で
/// 旧い辺も無ければ表を作らない（遅延作成）。
pub(crate) fn replace_edges_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    function_key: &str,
    signature: &str,
    deps: &[FunctionDependency],
) -> Result<()> {
    let mut unique: Vec<&FunctionDependency> = Vec::new();
    for d in deps {
        if !unique.contains(&d) {
            unique.push(d);
        }
    }
    if unique.len() > MAX_FUNCTION_DEPENDENCIES {
        return Err(CatalogError::ViewLimitExceeded(
            "too many function dependencies".to_string(),
        ));
    }
    remove_for_function_in_txn(write_txn, scope, function_key, signature)?;
    if unique.is_empty() {
        return Ok(());
    }
    let mut by_function = write_txn.open_table(DEPS_BY_FUNCTION)?;
    let mut by_dependee = write_txn.open_table(DEPS_BY_DEPENDEE)?;
    for d in unique {
        by_function.insert(
            (scope, function_key, signature, d.kind.tag(), d.key.as_str()),
            (),
        )?;
        by_dependee.insert(
            (d.kind.tag(), d.key.as_str(), scope, function_key, signature),
            (),
        )?;
    }
    Ok(())
}

/// 関数 `function_key`（任意のシグネチャ）が辺を持つ参照先すべてから、`target` へ到達できるか。
/// `start` の各関数から関数への辺（同じ `scope`）を辿り、`target` に着いたら `true`。訪問数は
/// [`MAX_DEPENDENCY_WALK`] で打ち切り、超過も `true`（循環扱い。fail-closed）。
pub(crate) fn function_edges_reach_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    start: &[FunctionDependency],
    target: &str,
) -> Result<bool> {
    let table = match src.open_table(DEPS_BY_FUNCTION) {
        Ok(t) => t,
        Err(e) if is_missing(&e) => {
            return Ok(start
                .iter()
                .any(|d| d.kind == DependencyKind::Function && d.key == target));
        }
        Err(e) => return Err(e.into()),
    };
    let mut stack: Vec<String> = start
        .iter()
        .filter(|d| d.kind == DependencyKind::Function)
        .map(|d| d.key.clone())
        .collect();
    let mut visited: Vec<String> = Vec::new();
    while let Some(cur) = stack.pop() {
        if cur == target {
            return Ok(true);
        }
        if visited.contains(&cur) {
            continue;
        }
        if visited.len() >= MAX_DEPENDENCY_WALK {
            return Ok(true);
        }
        visited.push(cur.clone());
        for entry in table.range((scope, cur.as_str(), "", 0u8, "")..)? {
            let (k, _) = entry?;
            let (s, f, _sig, kind, key) = k.value();
            if s != scope || f != cur.as_str() {
                break;
            }
            if DependencyKind::from_tag(kind)? == DependencyKind::Function {
                stack.push(key.to_string());
            }
        }
    }
    Ok(false)
}

/// 全 scope を通じて、relation キー `key`（表・ビュー・マテリアライズドビュー）を参照する標準形の
/// 関数が 1 件でもあるか。表が無ければ `false`。`DROP`・改名・列変更の入口が使う。
pub(crate) fn relation_has_dependents_in_txn<R: ReadSource>(src: &R, key: &str) -> Result<bool> {
    let table = match src.open_table(DEPS_BY_DEPENDEE) {
        Ok(t) => t,
        Err(e) if is_missing(&e) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let first = table
        .range((KIND_RELATION, key, "", "", "")..)?
        .next()
        .transpose()?;
    Ok(first.is_some_and(|(k, _)| {
        let (kind, dependee, _, _, _) = k.value();
        kind == KIND_RELATION && dependee == key
    }))
}

/// `scope` の関数 `key` を参照する標準形の関数が 1 件でもあるか（同じ scope の中だけで判定する）。
pub(crate) fn function_has_dependents_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    key: &str,
) -> Result<bool> {
    let table = match src.open_table(DEPS_BY_DEPENDEE) {
        Ok(t) => t,
        Err(e) if is_missing(&e) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let first = table
        .range((KIND_FUNCTION, key, scope, "", "")..)?
        .next()
        .transpose()?;
    Ok(first.is_some_and(|(k, _)| {
        let (kind, dependee, s, _, _) = k.value();
        kind == KIND_FUNCTION && dependee == key && s == scope
    }))
}

/// 参照先が（`Relation` なら表・ビュー・マテリアライズドビューのいずれかとして、`Function` なら
/// `scope` の関数として）write txn の最新カタログに存在するか。
pub(crate) fn dependee_exists_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    dep: &FunctionDependency,
) -> Result<bool> {
    match dep.kind {
        DependencyKind::Relation => {
            if let Ok(t) = ReadSource::open_table(write_txn, super::CATALOG_TABLE) {
                if t.get(dep.key.as_str())?.is_some() {
                    return Ok(true);
                }
            }
            if let Ok(t) = ReadSource::open_table(write_txn, super::VIEWS_TABLE) {
                if t.get(dep.key.as_str())?.is_some() {
                    return Ok(true);
                }
            }
            if let Ok(t) =
                ReadSource::open_table(write_txn, super::materialized::MATERIALIZED_VIEWS_TABLE)
            {
                if t.get(dep.key.as_str())?.is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        DependencyKind::Function => {
            let table = match ReadSource::open_table(write_txn, super::function::FUNCTIONS_TABLE) {
                Ok(t) => t,
                Err(e) if is_missing(&e) => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            let first = table
                .range((scope, dep.key.as_str(), "")..)?
                .next()
                .transpose()?;
            Ok(first.is_some_and(|(k, _)| {
                let (s, f, _) = k.value();
                s == scope && f == dep.key
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    fn fresh(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (Storage::open(&path).expect("open"), guard)
    }

    fn rel(k: &str) -> FunctionDependency {
        FunctionDependency {
            kind: DependencyKind::Relation,
            key: k.to_string(),
        }
    }

    fn func(k: &str) -> FunctionDependency {
        FunctionDependency {
            kind: DependencyKind::Function,
            key: k.to_string(),
        }
    }

    /// 記録・全 scope の範囲走査・関数側の掃除（辺は 2 表で対になる）。
    #[test]
    fn record_scan_and_remove_across_scopes() {
        let (storage, _g) = fresh("fn-dep-scan");
        let txn = storage.begin_write_txn().expect("txn");
        replace_edges_in_txn(&txn, "a", "f", "i", &[rel("t"), rel("t"), func("g")]).expect("a");
        replace_edges_in_txn(&txn, "b", "h", "", &[rel("t")]).expect("b");
        assert!(relation_has_dependents_in_txn(&*txn, "t").expect("t"));
        assert!(!relation_has_dependents_in_txn(&*txn, "u").expect("u"));
        assert!(function_has_dependents_in_txn(&*txn, "a", "g").expect("g"));
        // 他 scope の関数からは見えない（同じ scope の中だけで解決する）。
        assert!(!function_has_dependents_in_txn(&*txn, "b", "g").expect("g-b"));
        assert_eq!(
            dependencies_of_in_txn(&*txn, "a", "f", "i")
                .expect("deps")
                .len(),
            2
        );
        remove_for_function_in_txn(&txn, "a", "f", "i").expect("rm");
        assert!(relation_has_dependents_in_txn(&*txn, "t").expect("still b"));
        remove_for_function_in_txn(&txn, "b", "h", "").expect("rm b");
        assert!(!relation_has_dependents_in_txn(&*txn, "t").expect("none"));
        assert!(!function_has_dependents_in_txn(&*txn, "a", "g").expect("none g"));
    }

    /// 置換は旧い辺を消して新しい辺だけを残す。空の置換で表を作らない（遅延作成）。
    #[test]
    fn replace_swaps_edges_and_empty_does_not_create_tables() {
        let (storage, _g) = fresh("fn-dep-replace");
        let txn = storage.begin_write_txn().expect("txn");
        replace_edges_in_txn(&txn, "a", "f", "", &[]).expect("empty");
        assert!(ReadSource::open_table(&*txn, DEPS_BY_DEPENDEE).is_err());
        replace_edges_in_txn(&txn, "a", "f", "", &[rel("t1")]).expect("first");
        replace_edges_in_txn(&txn, "a", "f", "", &[rel("t2")]).expect("second");
        assert!(!relation_has_dependents_in_txn(&*txn, "t1").expect("t1"));
        assert!(relation_has_dependents_in_txn(&*txn, "t2").expect("t2"));
    }

    /// 関数間の辺の到達判定（循環検査）と、依存数の上限（`54000` へ写る `ViewLimitExceeded`）。
    #[test]
    fn reachability_and_dependency_cap() {
        let (storage, _g) = fresh("fn-dep-reach");
        let txn = storage.begin_write_txn().expect("txn");
        replace_edges_in_txn(&txn, "a", "g", "", &[func("h")]).expect("g");
        replace_edges_in_txn(&txn, "a", "h", "", &[func("f")]).expect("h");
        assert!(function_edges_reach_in_txn(&*txn, "a", &[func("g")], "f").expect("reach"));
        assert!(!function_edges_reach_in_txn(&*txn, "a", &[func("g")], "zzz").expect("no"));
        assert!(!function_edges_reach_in_txn(&*txn, "b", &[func("g")], "f").expect("scope"));
        let many: Vec<FunctionDependency> = (0..=MAX_FUNCTION_DEPENDENCIES)
            .map(|i| rel(&format!("t{i}")))
            .collect();
        assert!(matches!(
            replace_edges_in_txn(&txn, "a", "big", "", &many),
            Err(CatalogError::ViewLimitExceeded(_))
        ));
    }
}
