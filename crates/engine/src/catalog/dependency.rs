//! DROP の依存オブジェクト列挙層（TABLE-37 のポインタ・Issue #1640。親: #1638）。
//!
//! 役割: DROP 文の対象（[`DropTarget`]。複数指定可）に依存するオブジェクトを、名前つきで・
//! 推移的に・決定的な順序で列挙する（[`enumerate_drop_dependents_in_txn`]）。従来の依存検査は
//! 種別ごと（`Storage::drop_table`・`drop_view`・`drop_index` の一意索引分岐・`drop_enum_type`）
//! に散在し「1 件でも依存があれば `2BP01`」という真偽しか返せなかった。本層は #1641
//! （CASCADE／RESTRICT の適用と NOTICE）と #1639（複数指定の原子実行）が共通に使う前段で、
//! **列挙のみ**を担う。削除の適用・`2BP01` の判定切り替え・NOTICE 送出・SQL 構文は持たない
//! （既存の DROP／ALTER の挙動は変えない）。
//!
//! 呼び出し元の想定: `Storage::drop_*` と同じ write txn 内で呼び（TOCTOU 回避）、読み取りのみ
//! 行う。カタログ表（`CATALOG_TABLE`／`VIEWS_TABLE`／`INDEX_CATALOG_TABLE`／`ENUM_TYPES_TABLE`）
//! だけを読み、`user_rows/*` など行ストアには触れないため、他テナントの行の有無は結果に
//! 影響しない。返す名前は内部判断用で、エラー文言や wire へ出すかどうかは呼び出し元（#1641）が決める。
//!
//! 依存に数えないもの: 表に付随するオブジェクト（その表自身の索引・PK・UNIQUE・CHECK・
//! 自己参照を含む自分が持つ FK）は表と一緒に消える付随物であり、依存ではない
//! （PostgreSQL の挙動に合わせる。本層は付随する索引名を別途返さない。必要なら呼び出し元が
//! `INDEX_CATALOG_TABLE` から表名で引く）。
//!
//! 根集合の除外: 依存先が同じ文で DROP する対象（根集合）に含まれるなら、列挙結果に入れない
//! （参照元の表ごと消える FK、同時に消えるビュー、同時に消える表の列）。ただし辺
//! （`(依存される側, Target(根))`）は残す。残るオブジェクトが根に依存される関係を失うと、
//! 削除順の入力として使う `edges` が不完全になるため。
//!
//! 走査: 再帰せず明示スタックと訪問済み集合で反復する（深い連鎖でスタックを消費しない）。
//! 子は名前順、出力は帰りがけ順で「依存する側が先」。ビューの循環は構造上作れない
//! （`create_view` が拒否する）ため、検出した場合はカタログ破損として
//! [`CatalogError::CorruptSchema`]。走査量は上限で頭打ちにし、超過・decode 失敗は `Err`
//! （fail-closed）。判定を列単位で正確にできない要素（本文を再検証できないビュー、評価後射影形
//! ビュー）は依存に加えたうえで [`DependencyClosure::conservative`] を立てる（黙って省かない）。
//!
//! UDF 参照: 現状は辺が構造的に存在しない。UDF はセッション単位で永続化されず
//! （`sql::udf_call::UdfRegistry`）、永続オブジェクト（ビュー本文・CHECK・索引）は作成時に
//! 非組み込み呼び出しを拒否するため、永続オブジェクトが UDF に依存することがない。永続関数
//! （SQL-66〜68 のポインタ）を導入する際に、ここへ辺を追加する。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use redb::ReadableTable;

use super::{
    decode_index_def, decode_view_def, dependent_tables_in_txn, exposed_columns_of_view,
    foreign_keys_relying_on_unique_columns_in_txn, referencing_foreign_keys_in_txn,
    require_table_schema_write, view_body_shape, view_def_dependence_on, CatalogError, ColumnType,
    ForeignKeyDef, IndexDef, Result, TableSchema, ViewDef, ViewDependence, ViewExposure,
    CATALOG_TABLE, ENUM_TYPES_TABLE, INDEX_CATALOG_TABLE, MAX_LIST_TABLES, MAX_VIEWS,
    MAX_VIEW_CHAIN_WALK, VIEWS_TABLE,
};

/// 走査で訪問するオブジェクト数・記録する辺数の上限（本リポの実装既定値。DoS 対策として
/// 無制限の `Vec`／集合を確保しないための頭打ち）。ちょうど上限までは通す。
const MAX_DEPENDENCY_WALK: usize = MAX_LIST_TABLES * 2;

/// DROP 文が指定する根（TABLE-37）。複数指定はスライスで渡す。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DropTarget {
    Table(String),
    View(String),
    Index(String),
    EnumType(String),
}

/// 根に依存するオブジェクト。`Ord` は決定的な整列に使う。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DependentObject {
    /// ビュー全体が依存する。
    View { name: String },
    /// 他表の FK 制約。参照元の表は残り、制約だけが依存する。
    ForeignKey { table: String, constraint: String },
    /// ENUM 型を使う列（表は残り、列が依存する）。
    Column { table: String, column: String },
}

/// 辺の端点（向きは [`DependencyClosure::edges`] 参照）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DependencyNode {
    Target(DropTarget),
    Object(DependentObject),
}

/// 列挙結果（TABLE-37）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DependencyClosure {
    /// 依存オブジェクト。削除順（依存する側が先）で、決定的に並ぶ。重複しない。
    pub objects: Vec<DependentObject>,
    /// `(依存される側, 依存する側)` の辺。NOTICE の文面と順序決定に使う。
    pub edges: BTreeSet<(DependencyNode, DependencyNode)>,
    /// 列単位で正確に判定できず、保守的に「依存あり」とした要素があるか
    /// （本文を再検証できないビュー・評価後射影形ビュー）。呼び出し元は CASCADE を拒否するなど
    /// fail-closed に扱える。
    pub conservative: bool,
}

/// 子の候補。`soft` は「本文を再検証できないため依存とみなした」辺で、循環に見えても
/// 破損とは扱わず黙って打ち切る（保守フラグは別途立っている）。
#[derive(Debug, Clone)]
struct Child {
    obj: DependentObject,
    soft: bool,
    /// 依存側が同じ文の根集合の要素（そのビュー・表）である場合の根。`objects` には入れず
    /// 辺（`(依存される側, Target(根))`）だけを出す（削除順の入力として、残るオブジェクトが
    /// まだ根に依存されている関係を `edges` から失わないため）。
    root: Option<DropTarget>,
}

/// 走査中の 1 オブジェクト分の状態（明示スタックの要素）。
struct Frame {
    node: DependencyNode,
    children: Vec<Child>,
    next: usize,
}

/// `targets` の依存を推移的に列挙する。読み取りのみで何も書かない。根が存在しなければ
/// 種類ごとの `*NotFound`（種類不一致の `42809` は既存 DROP 経路の判定に委ねる）。
pub(crate) fn enumerate_drop_dependents_in_txn(
    write_txn: &redb::WriteTransaction,
    targets: &[DropTarget],
) -> Result<DependencyClosure> {
    let roots: BTreeSet<DropTarget> = targets.iter().cloned().collect();
    for root in &roots {
        require_root_exists(write_txn, root)?;
    }
    let mut walker = Walker {
        write_txn,
        views: load_views(write_txn)?,
        root_tables: roots
            .iter()
            .filter_map(|t| match t {
                DropTarget::Table(n) => Some(n.clone()),
                _ => None,
            })
            .collect(),
        root_views: roots
            .iter()
            .filter_map(|t| match t {
                DropTarget::View(n) => Some(n.clone()),
                _ => None,
            })
            .collect(),
        conservative: false,
    };

    let mut closure = DependencyClosure::default();
    let mut visited: BTreeSet<DependentObject> = BTreeSet::new();
    let mut on_stack: BTreeSet<DependentObject> = BTreeSet::new();
    for root in roots {
        let node = DependencyNode::Target(root);
        let children = walker.children(&node)?;
        let mut stack = vec![Frame {
            node,
            children,
            next: 0,
        }];
        while let Some(top) = stack.last_mut() {
            if let Some(child) = top.children.get(top.next).cloned() {
                top.next = top.next.saturating_add(1);
                let parent = top.node.clone();
                if let Some(root) = child.root {
                    // 依存側が根: オブジェクトには入れず辺だけ残す（根自体は最上位ループが走査する）。
                    if closure.edges.len() >= MAX_DEPENDENCY_WALK {
                        return Err(walk_limit_error());
                    }
                    closure.edges.insert((parent, DependencyNode::Target(root)));
                    continue;
                }
                if on_stack.contains(&child.obj) {
                    if child.soft {
                        continue;
                    }
                    return Err(CatalogError::CorruptSchema(
                        "dependency cycle detected".to_string(),
                    ));
                }
                if closure.edges.len() >= MAX_DEPENDENCY_WALK {
                    return Err(walk_limit_error());
                }
                let child_node = DependencyNode::Object(child.obj.clone());
                closure.edges.insert((parent, child_node.clone()));
                if visited.contains(&child.obj) {
                    continue;
                }
                if visited.len() >= MAX_DEPENDENCY_WALK {
                    return Err(walk_limit_error());
                }
                visited.insert(child.obj.clone());
                on_stack.insert(child.obj);
                let grandchildren = walker.children(&child_node)?;
                stack.push(Frame {
                    node: child_node,
                    children: grandchildren,
                    next: 0,
                });
            } else if let Some(done) = stack.pop() {
                if let DependencyNode::Object(obj) = done.node {
                    on_stack.remove(&obj);
                    closure.objects.push(obj);
                }
            }
        }
    }
    closure.conservative = walker.conservative;
    Ok(closure)
}

fn walk_limit_error() -> CatalogError {
    CatalogError::ViewLimitExceeded("too many dependent objects".to_string())
}

/// テーブルを読み取り専用で開く。欠落なら `None`（書き込みトランザクションでも作成せず、
/// 読み取り列挙が空テーブルを COMMIT で永続化しないようにする。列挙の read-only 契約）。
fn open_existing<'t, K, V>(
    write_txn: &'t redb::WriteTransaction,
    def: redb::TableDefinition<'_, K, V>,
) -> Result<
    Option<<redb::WriteTransaction as crate::storage::read_source::ReadSource>::Table<'t, K, V>>,
>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    match crate::storage::read_source::ReadSource::open_table(write_txn, def) {
        Ok(t) => Ok(Some(t)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 根の存在を確認する（無ければ種類ごとの `*NotFound`。`Ok(空)` へ丸めない）。
fn require_root_exists(write_txn: &redb::WriteTransaction, root: &DropTarget) -> Result<()> {
    let (def, name, missing) = match root {
        DropTarget::Table(n) => (CATALOG_TABLE, n, CatalogError::TableNotFound(n.clone())),
        DropTarget::View(n) => (VIEWS_TABLE, n, CatalogError::ViewNotFound(n.clone())),
        DropTarget::Index(n) => (
            INDEX_CATALOG_TABLE,
            n,
            CatalogError::IndexNotFound(n.clone()),
        ),
        DropTarget::EnumType(n) => (ENUM_TYPES_TABLE, n, CatalogError::TypeNotFound(n.clone())),
    };
    // `WriteTransaction::open_table` は欠落テーブルを作成するため、作成しない
    // `ReadSource` 経由で開く（欠落は「根なし」＝ `*NotFound`）。
    let Some(table) = open_existing(write_txn, def)? else {
        return Err(missing);
    };
    if table.get(name.as_str())?.is_none() {
        return Err(missing);
    }
    Ok(())
}

/// 全ビュー定義を名前順に 1 回だけ読む（以降の走査で表ハンドルを保持しないため。
/// 件数は [`MAX_VIEWS`] で頭打ちにし、超過は `Err`）。
fn load_views(write_txn: &redb::WriteTransaction) -> Result<Vec<(String, ViewDef)>> {
    // VIEWS_TABLE 欠落は「ビュー無し」（既存 DROP 経路と同じ扱い。空テーブルを作らない）。
    let Some(table) = open_existing(write_txn, VIEWS_TABLE)? else {
        return Ok(Vec::new());
    };
    let mut views = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        if views.len() >= MAX_VIEWS {
            return Err(CatalogError::ViewLimitExceeded(
                "too many views".to_string(),
            ));
        }
        views.push((key.value().to_string(), decode_view_def(value.value())?));
    }
    Ok(views)
}

struct Walker<'a> {
    write_txn: &'a redb::WriteTransaction,
    views: Vec<(String, ViewDef)>,
    root_tables: BTreeSet<String>,
    root_views: BTreeSet<String>,
    conservative: bool,
}

fn fk_object(child: &TableSchema, fk: &ForeignKeyDef, root: Option<DropTarget>) -> Child {
    Child {
        obj: DependentObject::ForeignKey {
            table: child.name.clone(),
            constraint: fk.name().to_string(),
        },
        soft: false,
        root,
    }
}

impl Walker<'_> {
    /// `node` に直接依存するオブジェクトを名前順で返す（根集合に含まれるものは `Child::root` を付けて返し、
    /// 走査側が辺だけを記録する）。
    fn children(&mut self, node: &DependencyNode) -> Result<Vec<Child>> {
        let mut out = match node {
            DependencyNode::Target(DropTarget::Table(name)) => self.table_children(name)?,
            DependencyNode::Target(DropTarget::View(name))
            | DependencyNode::Object(DependentObject::View { name }) => self.views_reading(name),
            DependencyNode::Target(DropTarget::Index(name)) => self.index_children(name)?,
            DependencyNode::Target(DropTarget::EnumType(name)) => self.enum_children(name)?,
            DependencyNode::Object(DependentObject::Column { table, column }) => {
                self.column_children(table, column)?
            }
            // FK 制約は葉（制約だけが消え、そこから先に依存するものはない）。
            DependencyNode::Object(DependentObject::ForeignKey { .. }) => Vec::new(),
        };
        out.sort_by(|a, b| a.obj.cmp(&b.obj));
        out.dedup_by(|a, b| a.obj == b.obj);
        Ok(out)
    }

    /// 根集合の表なら対応する根（`Child::root` 用）。
    fn root_for_table(&self, name: &str) -> Option<DropTarget> {
        self.root_tables
            .contains(name)
            .then(|| DropTarget::Table(name.to_string()))
    }

    /// 根集合のビューなら対応する根（`Child::root` 用）。
    fn root_for_view(&self, name: &str) -> Option<DropTarget> {
        self.root_views
            .contains(name)
            .then(|| DropTarget::View(name.to_string()))
    }

    /// 他表の FK（自己参照は除く。参照元が根集合の表でも返し、`fk_object` が根を付ける）。
    fn foreign_keys_into(&self, table: &str) -> Result<Vec<(TableSchema, ForeignKeyDef)>> {
        Ok(referencing_foreign_keys_in_txn(self.write_txn, table)?
            .into_iter()
            .filter(|(child, _)| child.name != table)
            .collect())
    }

    fn table_children(&mut self, table: &str) -> Result<Vec<Child>> {
        let mut out: Vec<Child> = self
            .foreign_keys_into(table)?
            .iter()
            .map(|(child, fk)| fk_object(child, fk, self.root_for_table(&child.name)))
            .collect();
        out.extend(self.views_reading(table));
        Ok(out)
    }

    /// relation `relation` を読むビュー（根集合のビューは `root` 付きで返す）。
    fn views_reading(&mut self, relation: &str) -> Vec<Child> {
        let mut out = Vec::new();
        for (name, def) in &self.views {
            let soft = match view_def_dependence_on(def, relation) {
                ViewDependence::No => continue,
                ViewDependence::Direct => false,
                ViewDependence::Unverifiable => true,
            };
            if soft {
                self.conservative = true;
            }
            out.push(Child {
                obj: DependentObject::View { name: name.clone() },
                soft,
                root: self.root_for_view(name),
            });
        }
        out
    }

    /// 一意索引なら、その列集合を参照先とする FK（索引は自己参照 FK も依存に数える。
    /// 索引だけを消しても表は残るため）。非 unique 索引に依存はない。
    fn index_children(&mut self, index: &str) -> Result<Vec<Child>> {
        let def: IndexDef = {
            let table = open_existing(self.write_txn, INDEX_CATALOG_TABLE)?
                .ok_or_else(|| CatalogError::IndexNotFound(index.to_string()))?;
            let guard = table
                .get(index)?
                .ok_or_else(|| CatalogError::IndexNotFound(index.to_string()))?;
            decode_index_def(index, guard.value())?
        };
        if !def.unique {
            return Ok(Vec::new());
        }
        let corrupt = || {
            CatalogError::CorruptSchema(format!(
                "unique index {} has no matching unique constraint",
                def.name
            ))
        };
        let schema = match require_table_schema_write(self.write_txn, &def.table) {
            Ok(s) => s,
            Err(CatalogError::TableNotFound(_)) => return Err(corrupt()),
            Err(e) => return Err(e),
        };
        let target_set: std::collections::HashSet<&str> = schema
            .unique_constraints
            .iter()
            .find(|u| u.name() == def.name)
            .ok_or_else(corrupt)?
            .columns()
            .iter()
            .map(String::as_str)
            .collect();
        Ok(
            foreign_keys_relying_on_unique_columns_in_txn(self.write_txn, &def.table, &target_set)?
                .iter()
                .map(|(child, fk)| fk_object(child, fk, self.root_for_table(&child.name)))
                .collect(),
        )
    }

    /// ENUM 型 `type_name` を（直接または配列要素として）使う列。
    fn enum_children(&mut self, type_name: &str) -> Result<Vec<Child>> {
        let mut out = Vec::new();
        for table in dependent_tables_in_txn(self.write_txn, type_name)? {
            let root = self.root_for_table(&table);
            let schema = require_table_schema_write(self.write_txn, &table)?;
            for column in &schema.columns {
                let uses = match &column.ty {
                    ColumnType::Enum(def) => def.name() == type_name,
                    ColumnType::Array(array) => {
                        array.enum_def().is_some_and(|def| def.name() == type_name)
                    }
                    _ => false,
                };
                if uses {
                    out.push(Child {
                        obj: DependentObject::Column {
                            table: table.clone(),
                            column: column.name.clone(),
                        },
                        soft: false,
                        root: root.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// 列 `column` に依存する他表の FK と、その列を参照・公開するビュー。
    /// 同じ表の PK・UNIQUE・CHECK・自分の FK・その列を含む索引は列に付随する（数えない）。
    fn column_children(&mut self, table: &str, column: &str) -> Result<Vec<Child>> {
        let mut out: Vec<Child> = self
            .foreign_keys_into(table)?
            .iter()
            .filter(|(_, fk)| fk.parent_columns().iter().any(|c| c == column))
            .map(|(child, fk)| fk_object(child, fk, self.root_for_table(&child.name)))
            .collect();
        let schema = require_table_schema_write(self.write_txn, table)?;
        out.extend(self.views_affected_by_column(&schema, column)?);
        Ok(out)
    }

    /// `schema` の表から `column` が無くなると壊れる、または `column` を公開するビュー
    /// （`alter_table_drop_column` の依存ビュー検査と同じ判定部品を使う）。評価後射影形ビューが
    /// 表へ到達する場合と本文を再検証できない場合は列単位で判定できないため、依存に加えて
    /// 保守フラグを立てる。
    fn views_affected_by_column(
        &mut self,
        schema: &TableSchema,
        column: &str,
    ) -> Result<Vec<Child>> {
        let table = schema.name.as_str();
        let reserved = ["id", "tenant_id", "visibility"].map(String::from);
        let remaining: Vec<String> = reserved
            .iter()
            .cloned()
            .chain(
                schema
                    .columns
                    .iter()
                    .filter(|c| c.name != column)
                    .map(|c| c.name.clone()),
            )
            .collect();
        let full: Vec<String> = reserved
            .iter()
            .cloned()
            .chain(schema.columns.iter().map(|c| c.name.clone()))
            .collect();

        // 値は「列単位で判定できず保守的に依存とした」か。
        let mut affected: BTreeMap<String, bool> = BTreeMap::new();
        let mut simple: HashMap<String, ViewDef> = HashMap::new();
        let mut buffered: Vec<(String, Vec<String>)> = Vec::new();
        for (name, def) in &self.views {
            match view_body_shape(&def.body_sql) {
                Ok(shape) if shape.buffered => buffered.push((name.clone(), shape.relations)),
                Ok(_) => {
                    simple.insert(name.clone(), def.clone());
                }
                Err(_) => {
                    affected.insert(name.clone(), true);
                }
            }
        }
        // 評価後射影形ビューが（直接、または単純形ビューの連鎖経由で）表へ到達するなら保守的に依存。
        // 連鎖が過大な場合も打ち切って依存とする（fail-closed）。
        for (name, relations) in &buffered {
            for rel in relations {
                let mut cur: &str = rel;
                let mut steps = 0usize;
                loop {
                    if cur == table {
                        affected.insert(name.clone(), true);
                        break;
                    }
                    steps = steps.saturating_add(1);
                    if steps > MAX_VIEW_CHAIN_WALK {
                        affected.insert(name.clone(), true);
                        break;
                    }
                    match simple.get(cur) {
                        Some(d) => cur = d.base_relation.as_str(),
                        None => break,
                    }
                }
            }
        }
        // 単純形ビュー: 削除後の公開列で壊れるもの、または対象列を公開するもの。
        let mut names: Vec<&String> = simple.keys().collect();
        names.sort();
        let mut memo_remaining = HashMap::new();
        let mut memo_full = HashMap::new();
        for name in names {
            let broken =
                exposed_columns_of_view(&simple, &mut memo_remaining, name, table, &remaining)?
                    .is_broken();
            let exposes =
                match exposed_columns_of_view(&simple, &mut memo_full, name, table, &full)? {
                    ViewExposure::Broken => true,
                    ViewExposure::Columns(cols) => cols.iter().any(|c| c == column),
                    ViewExposure::Unrelated => false,
                };
            if broken || exposes {
                affected.entry(name.clone()).or_insert(false);
            }
        }
        let mut out = Vec::new();
        for (name, conservative) in affected {
            if conservative {
                self.conservative = true;
            }
            let root = self.root_for_view(&name);
            out.push(Child {
                obj: DependentObject::View { name },
                soft: conservative,
                root,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{encode_view_def, ColumnDef, IndexKind, ReferentialAction};
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn cols() -> Vec<ColumnDef> {
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("a", ColumnType::Text, false),
            ColumnDef::new("b", ColumnType::Text, true),
        ]
    }

    fn fk(columns: &[&str], parent: &str, parent_columns: &[&str]) -> ForeignKeyDef {
        ForeignKeyDef::new(
            columns.iter().map(|c| c.to_string()).collect(),
            parent.to_string(),
            parent_columns.iter().map(|c| c.to_string()).collect(),
            ReferentialAction::NoAction,
            ReferentialAction::NoAction,
        )
    }

    fn parent_table(name: &str) -> TableSchema {
        TableSchema::new(name, cols()).with_primary_key(vec!["a".to_string()])
    }

    fn child_table(name: &str, parent: &str) -> TableSchema {
        TableSchema::new(name, cols()).with_foreign_keys(vec![fk(&["b"], parent, &["a"])])
    }

    fn open(label: &str) -> (Storage, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn run(storage: &Storage, targets: &[DropTarget]) -> Result<DependencyClosure> {
        let txn = storage.begin_write_txn().expect("begin write");
        let result = enumerate_drop_dependents_in_txn(&txn, targets);
        drop(txn);
        result
    }

    fn view(name: &str) -> DependentObject {
        DependentObject::View {
            name: name.to_string(),
        }
    }

    fn table(name: &str) -> DropTarget {
        DropTarget::Table(name.to_string())
    }

    fn is_fk_of(obj: Option<&DependentObject>, child: &str) -> bool {
        matches!(obj, Some(DependentObject::ForeignKey { table, .. }) if table == child)
    }

    /// 他表の FK は `ForeignKey` として列挙され、自己参照 FK だけの表は依存なし（TABLE-37）。
    #[test]
    fn t1_foreign_key_dependents_exclude_self_reference() {
        let (storage, _g) = open("dep-t1");
        storage.create_table(&parent_table("p")).expect("p");
        storage.create_table(&child_table("c", "p")).expect("c");
        let selfref = TableSchema::new("s", cols())
            .with_primary_key(vec!["a".to_string()])
            .with_foreign_keys(vec![fk(&["b"], "s", &["a"])]);
        storage.create_table(&selfref).expect("s");

        let closure = run(&storage, &[table("p")]).expect("enumerate");
        assert_eq!(closure.objects.len(), 1);
        assert!(is_fk_of(closure.objects.first(), "c"));
        assert!(!closure.conservative);
        let selfref_closure = run(&storage, &[table("s")]).expect("enumerate");
        assert!(selfref_closure.objects.is_empty());
        assert!(selfref_closure.edges.is_empty());
    }

    /// 同じ文で DROP する対象どうしの依存は数えない（根集合の除外。TABLE-37）。
    #[test]
    fn t2_root_set_members_are_excluded() {
        let (storage, _g) = open("dep-t2");
        storage.create_table(&parent_table("p")).expect("p");
        storage.create_table(&child_table("c", "p")).expect("c");
        let both = run(&storage, &[table("p"), table("c")]).expect("enumerate");
        assert!(both.objects.is_empty());

        storage
            .create_view("v1", "p", "SELECT a FROM p")
            .expect("v1");
        storage
            .create_view("v2", "v1", "SELECT a FROM v1")
            .expect("v2");
        let views = run(
            &storage,
            &[
                DropTarget::View("v1".to_string()),
                DropTarget::View("v2".to_string()),
            ],
        )
        .expect("enumerate");
        assert!(views.objects.is_empty());
        let only_v1 = run(&storage, &[DropTarget::View("v1".to_string())]).expect("enumerate");
        assert_eq!(only_v1.objects, vec![view("v2")]);
    }

    /// 根のビューに依存される非根ビューの辺は残る（`DROP TABLE t` + `DROP VIEW v2`、v2→v1→t）。
    #[test]
    fn t2b_edge_to_root_dependent_is_kept() {
        let (storage, _g) = open("dep-t2b");
        storage.create_table(&parent_table("t")).expect("t");
        storage
            .create_view("v1", "t", "SELECT a FROM t")
            .expect("v1");
        storage
            .create_view("v2", "v1", "SELECT a FROM v1")
            .expect("v2");
        let closure =
            run(&storage, &[table("t"), DropTarget::View("v2".to_string())]).expect("enumerate");
        assert_eq!(closure.objects, vec![view("v1")]);
        assert!(closure.edges.contains(&(
            DependencyNode::Object(view("v1")),
            DependencyNode::Target(DropTarget::View("v2".to_string()))
        )));
        assert!(closure.edges.contains(&(
            DependencyNode::Target(table("t")),
            DependencyNode::Object(view("v1"))
        )));
    }

    /// ビューの連鎖は推移的に辿り、依存する側が先に並ぶ。複数の枝でも重複しない（TABLE-37）。
    #[test]
    fn t3_view_chain_is_transitive_ordered_and_deduplicated() {
        let (storage, _g) = open("dep-t3");
        storage.create_table(&parent_table("t")).expect("t");
        storage
            .create_view("v1", "t", "SELECT a FROM t")
            .expect("v1");
        storage
            .create_view("v2", "v1", "SELECT a FROM v1")
            .expect("v2");
        let closure = run(&storage, &[table("t")]).expect("enumerate");
        assert_eq!(closure.objects, vec![view("v2"), view("v1")]);

        storage
            .create_view("w1", "t", "SELECT a FROM t")
            .expect("w1");
        storage
            .create_view("w2", "w1", "SELECT a FROM w1")
            .expect("w2");
        let closure = run(&storage, &[table("t")]).expect("enumerate");
        let unique: BTreeSet<_> = closure.objects.iter().collect();
        assert_eq!(unique.len(), closure.objects.len());
        assert_eq!(closure.objects.len(), 4);
        let pos = |n: &str| closure.objects.iter().position(|o| *o == view(n));
        assert!(pos("v2") < pos("v1"));
        assert!(pos("w2") < pos("w1"));
    }

    /// 評価後射影形ビュー（表を読む）も依存として列挙される（TABLE-37）。
    #[test]
    fn t4_buffered_view_is_listed() {
        let (storage, _g) = open("dep-t4");
        storage.create_table(&parent_table("t")).expect("t");
        storage
            .create_view("vb", "t", "SELECT a FROM t ORDER BY a LIMIT 5")
            .expect("vb");
        let closure = run(&storage, &[table("t")]).expect("enumerate");
        assert_eq!(closure.objects, vec![view("vb")]);
        assert!(!closure.conservative);
    }

    /// 一意索引の依存は、列集合が一致する FK だけ。非 unique 索引は依存なし（TABLE-37）。
    #[test]
    fn t5_unique_index_depends_only_on_matching_foreign_keys() {
        let (storage, _g) = open("dep-t5");
        storage
            .create_table(&TableSchema::new("p", cols()))
            .expect("p");
        let mk = |name: &str, col: &str, unique: bool| {
            IndexDef::new(
                name.to_string(),
                "p".to_string(),
                IndexKind::Scalar,
                vec![col.to_string()],
            )
            .with_unique(unique)
        };
        storage.create_index(&mk("u_a", "a", true)).expect("u_a");
        storage.create_index(&mk("u_b", "b", true)).expect("u_b");
        storage.create_table(&child_table("c", "p")).expect("c");

        let on_a = run(&storage, &[DropTarget::Index("u_a".to_string())]).expect("enumerate");
        assert_eq!(on_a.objects.len(), 1);
        assert!(is_fk_of(on_a.objects.first(), "c"));
        let on_b = run(&storage, &[DropTarget::Index("u_b".to_string())]).expect("enumerate");
        assert!(on_b.objects.is_empty());
    }

    /// 非 unique 索引は依存を持たない（TABLE-37）。
    #[test]
    fn t5b_plain_index_has_no_dependents() {
        let (storage, _g) = open("dep-t5b");
        storage
            .create_table(&TableSchema::new("p", cols()))
            .expect("p");
        storage
            .create_index(&IndexDef::new(
                "plain".to_string(),
                "p".to_string(),
                IndexKind::Scalar,
                vec!["a".to_string()],
            ))
            .expect("plain");
        let closure = run(&storage, &[DropTarget::Index("plain".to_string())]).expect("enumerate");
        assert!(closure.objects.is_empty());
    }

    /// ENUM 型は使う列へ、列はそれを参照するビューへ推移的に辿る。参照しない列のビューは含まない。
    #[test]
    fn t6_enum_type_reaches_columns_and_views() {
        let (storage, _g) = open("dep-t6");
        let def = storage
            .create_enum_type("mood", vec!["x".to_string(), "y".to_string()])
            .expect("enum");
        storage
            .create_table(&TableSchema::new(
                "t",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("m", ColumnType::Enum(def), true),
                    ColumnDef::new("a", ColumnType::Text, true),
                ],
            ))
            .expect("t");
        storage
            .create_view("vm", "t", "SELECT m FROM t")
            .expect("vm");
        storage
            .create_view("vm2", "vm", "SELECT m FROM vm")
            .expect("vm2");
        storage
            .create_view("va", "t", "SELECT a FROM t")
            .expect("va");
        let closure = run(&storage, &[DropTarget::EnumType("mood".to_string())]).expect("enum");
        assert_eq!(
            closure.objects,
            vec![
                view("vm2"),
                view("vm"),
                DependentObject::Column {
                    table: "t".to_string(),
                    column: "m".to_string()
                }
            ]
        );
        assert!(!closure.objects.contains(&view("va")));
    }

    /// 同じ DB の再列挙と、作成順を入れ替えた DB で結果が完全に一致する（決定性）。
    #[test]
    fn t7_enumeration_is_deterministic() {
        let build = |label: &str, reverse: bool| {
            let (storage, g) = open(label);
            storage.create_table(&parent_table("p")).expect("p");
            let mut names = vec!["c1", "c2", "c3"];
            if reverse {
                names.reverse();
            }
            for n in names {
                storage.create_table(&child_table(n, "p")).expect("child");
            }
            let mut vs = vec!["va", "vb"];
            if reverse {
                vs.reverse();
            }
            for v in vs {
                storage
                    .create_view(v, "p", "SELECT a FROM p")
                    .expect("view");
            }
            (storage, g)
        };
        let (s1, _g1) = build("dep-t7a", false);
        let (s2, _g2) = build("dep-t7b", true);
        let a = run(&s1, &[table("p")]).expect("a");
        let a2 = run(&s1, &[table("p")]).expect("a2");
        let b = run(&s2, &[table("p")]).expect("b");
        assert_eq!(a, a2);
        assert_eq!(a.objects, b.objects);
        assert_eq!(a.edges, b.edges);
        assert_eq!(a.objects.len(), 5);
    }

    /// ビュー数がちょうど上限なら通り、上限を超える破損カタログは `Err`（fail-closed）。
    #[test]
    fn t8_view_scan_limit_boundary() {
        let (storage, _g) = open("dep-t8");
        storage.create_table(&parent_table("t")).expect("t");
        let encoded = encode_view_def(&ViewDef {
            base_relation: "other".to_string(),
            body_sql: "SELECT a FROM other".to_string(),
        })
        .expect("encode");
        let write_txn = storage.db().begin_write().expect("begin");
        {
            let mut views = write_txn.open_table(VIEWS_TABLE).expect("views");
            for i in 0..MAX_VIEWS {
                views
                    .insert(format!("v{i:05}").as_str(), encoded.as_slice())
                    .expect("insert");
            }
        }
        let ok = enumerate_drop_dependents_in_txn(&write_txn, &[table("t")]);
        assert!(matches!(ok, Ok(ref c) if c.objects.is_empty()), "{ok:?}");
        {
            let mut views = write_txn.open_table(VIEWS_TABLE).expect("views");
            views.insert("v_over", encoded.as_slice()).expect("insert");
        }
        let over = enumerate_drop_dependents_in_txn(&write_txn, &[table("t")]);
        assert!(matches!(over, Err(CatalogError::ViewLimitExceeded(_))));
        write_txn.abort().expect("abort");
    }

    /// 本文を再検証できないビューは依存ありかつ保守フラグ付き。循環した定義は `CorruptSchema`。
    #[test]
    fn t9_unverifiable_view_is_conservative_and_cycle_is_corrupt() {
        let (storage, _g) = open("dep-t9");
        storage.create_table(&parent_table("t")).expect("t");
        let inject = |name: &str, base: &str, body: &str| {
            let encoded = encode_view_def(&ViewDef {
                base_relation: base.to_string(),
                body_sql: body.to_string(),
            })
            .expect("encode");
            let txn = storage.db().begin_write().expect("begin");
            {
                let mut views = txn.open_table(VIEWS_TABLE).expect("views");
                views.insert(name, encoded.as_slice()).expect("insert");
            }
            txn.commit().expect("commit");
        };
        inject("broken", "other", "THIS IS NOT SQL");
        let closure = run(&storage, &[table("t")]).expect("enumerate");
        assert_eq!(closure.objects, vec![view("broken")]);
        assert!(closure.conservative);

        // c4 は基底が t、本文が c1 を読む（食い違う定義を直接注入）。c1 は c4 を読むので、
        // t -> c4 -> c1 -> c4 の循環になる。
        inject("c4", "t", "SELECT a FROM c1");
        inject("c1", "c4", "SELECT a FROM c4");
        let err = run(&storage, &[table("t")]).expect_err("cycle");
        assert!(matches!(err, CatalogError::CorruptSchema(_)), "{err:?}");
    }

    /// テナントの行データは結果に影響しない（カタログのみを読む。TABLE-37）。
    #[test]
    fn t10_enumeration_ignores_row_data() {
        let (storage, _g) = open("dep-t10");
        storage
            .create_table(&TableSchema::new("t", cols()))
            .expect("t");
        storage.create_view("v", "t", "SELECT a FROM t").expect("v");
        let before = run(&storage, &[table("t")]).expect("before");
        for (id, tenant) in [(1u64, "tenant-a"), (2, "tenant-b")] {
            storage
                .insert_row_into_table(
                    "t",
                    id,
                    &RowInput {
                        tenant_id: tenant,
                        visibility: Visibility::Public,
                        embedding: &[0.1, 0.2],
                        metadata: &[],
                    },
                )
                .expect("insert row");
        }
        let after = run(&storage, &[table("t")]).expect("after");
        assert_eq!(before, after);
        assert_eq!(after.objects, vec![view("v")]);
    }

    /// 根が存在しなければ種類ごとの `*NotFound`（`Ok(空)` へ丸めない）。
    #[test]
    fn t11_missing_roots_are_not_found() {
        let (storage, _g) = open("dep-t11");
        assert!(matches!(
            run(&storage, &[table("nope")]),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(matches!(
            run(&storage, &[DropTarget::View("nope".to_string())]),
            Err(CatalogError::ViewNotFound(_))
        ));
        assert!(matches!(
            run(&storage, &[DropTarget::Index("nope".to_string())]),
            Err(CatalogError::IndexNotFound(_))
        ));
        assert!(matches!(
            run(&storage, &[DropTarget::EnumType("nope".to_string())]),
            Err(CatalogError::TypeNotFound(_))
        ));
    }

    /// 列挙は欠落したカタログ表を作成しない（空の VIEWS／INDEX／ENUM 表を COMMIT で永続化しない）。
    #[test]
    fn t11b_enumeration_does_not_create_missing_tables() {
        use redb::TableHandle;
        let (storage, _g) = open("dep-t11b");
        storage.create_table(&parent_table("t")).expect("t");
        let guarded = [
            VIEWS_TABLE.name().to_string(),
            INDEX_CATALOG_TABLE.name().to_string(),
            ENUM_TYPES_TABLE.name().to_string(),
        ];
        let existing = |txn: &redb::WriteTransaction| -> Vec<String> {
            txn.list_tables()
                .expect("list")
                .map(|h| h.name().to_string())
                .filter(|n| guarded.contains(n))
                .collect()
        };
        let write_txn = storage.db().begin_write().expect("begin");
        let before = existing(&write_txn);
        let closure = enumerate_drop_dependents_in_txn(&write_txn, &[table("t")]).expect("enum");
        assert!(closure.objects.is_empty());
        for missing in [
            DropTarget::View("nope".to_string()),
            DropTarget::Index("nope".to_string()),
            DropTarget::EnumType("nope".to_string()),
        ] {
            assert!(enumerate_drop_dependents_in_txn(&write_txn, &[missing]).is_err());
        }
        assert_eq!(existing(&write_txn), before);
        write_txn.abort().expect("abort");
    }

    /// ENUM 型だけがある新規 DB でも、列挙は空のカタログ表を作らない（PR #2521 指摘）。
    #[test]
    fn t11c_enum_enumeration_does_not_create_catalog_table() {
        use redb::TableHandle;
        let (storage, _g) = open("dep-t11c");
        storage
            .create_enum_type("mood", vec!["x".to_string(), "y".to_string()])
            .expect("enum");
        let write_txn = storage.db().begin_write().expect("begin");
        let has_catalog = |txn: &redb::WriteTransaction| {
            txn.list_tables()
                .expect("list")
                .any(|h| h.name() == CATALOG_TABLE.name())
        };
        assert!(!has_catalog(&write_txn));
        let closure = enumerate_drop_dependents_in_txn(
            &write_txn,
            &[DropTarget::EnumType("mood".to_string())],
        )
        .expect("enum");
        assert!(closure.objects.is_empty());
        assert!(!has_catalog(&write_txn));
        write_txn.abort().expect("abort");
    }

    /// 列挙は何も書かない: 列挙後も既存の DROP の判定結果が変わらない。
    #[test]
    fn t12_enumeration_does_not_change_existing_drop_behavior() {
        let (storage, _g) = open("dep-t12");
        storage.create_table(&parent_table("p")).expect("p");
        storage.create_table(&child_table("c", "p")).expect("c");
        storage.create_view("v", "c", "SELECT a FROM c").expect("v");
        run(&storage, &[table("p"), DropTarget::View("v".to_string())]).expect("enumerate");
        assert!(matches!(
            storage.drop_table("p"),
            Err(CatalogError::DependentObjectsStillExist(_))
        ));
        assert!(matches!(
            storage.drop_table("c"),
            Err(CatalogError::DependentViewsExist(_))
        ));
        storage.drop_view("v").expect("drop view");
        storage.drop_table("c").expect("drop child");
        storage.drop_table("p").expect("drop parent");
    }
}
