//! 要素つき `CREATE SCHEMA <名> [AUTHORIZATION <ロール>] CREATE TABLE ... CREATE VIEW ...` の実行本体
//! （TABLE-27・TABLE-44・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10 のポインタ、Issue #3286。設計記録:
//! `docs/design/schema-tenant-namespace.md`「Issue #3286 実装記録」）。
//!
//! 責務境界: 構文検証・要素の分割は `sql::allowlist::schema_elements`（カタログを照会しない）が担い、本モジュールは
//! `core.rs` の `execute_parsed_in_session_inner` の `CreateSchema` 腕（DDL 権限ゲート通過後。要素が空でない
//! ときだけ）から呼ばれ、SCHEMA の作成と全要素の作成を **1 つの write txn・1 回の commit** に束ねる。要素の作成
//! 本体は単独の DDL と共有する write txn 内本体（`sql::ddl::create_*_element_in_txn`）で、第 2 の作成経路を
//! 作らない。途中のどこで失敗しても commit 前に txn を破棄するため、SCHEMA も作成済みの要素も残らない。
//! クラッシュ時は redb の単一 commit が「全部ある」か「全部無い」かのどちらかだけを見せる。
//!
//! 名前解決（設計記録 D3・D8）: 文全体の汎用解決（`relation_name`・`search_path`）は要素つき `CREATE SCHEMA` を
//! 素通りするため、要素ごとにここで解決する。作成中の SCHEMA は内部 ID が払い出されるまで分からないため、
//! 「新しい SCHEMA を指す位置」を先に集め（`PreparedElement::new_positions`）、write txn の中で ID が決まった
//! 後に内部キー `#<id>.<名前>` へ 1 対 1 で置換してから、単独の DDL と同じ検証器を再実行する。それ以外の
//! 参照は、他の SCHEMA・`public` の修飾を `relation_name`、未修飾名を `search_path` の既存の解決器で解く。
//!
//! テナント境界（P0）: SCHEMA の照会・払い出しは `(ctx.tenant_id, 名前)` だけで、他テナントの同名 SCHEMA は
//! 存在しない名前と同じに扱う。要素ごとに単独の DDL と同じ `require_ddl` をカタログ照会より前に課す。
//! 他人を所有者にする `AUTHORIZATION` と要素の併用は昇格経路になるため `0A000` で拒否する。

use super::EngineCore;
use crate::catalog::namespace::RelationKey;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{
    SchemaElement, SchemaElementKind, SqlSurfaceError, ValidatedCreateSchema,
};
use crate::sql::ddl::CreateSchemaOutcome;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;
use crate::sql::privilege_gate::DdlStatementKind;

/// 名前解決を済ませた要素 1 件（write txn を開く前の準備結果）。
struct PreparedElement {
    kind: SchemaElementKind,
    /// 作成する名前（修飾を除いた名前部。以降の要素の未修飾の参照を新しい SCHEMA へ解決するために使う）。
    create_name: Option<String>,
    /// 他の SCHEMA・`public`・`search_path` の解決を済ませたトークン列。新しい SCHEMA を指す位置は
    /// 裸の名前のまま（[`Self::new_positions`] が内部キーへの置換位置を持つ）。
    tokens: Vec<Token>,
    /// 新しい SCHEMA を指す位置（トークン添字と名前）。write txn の中で ID が決まった後に
    /// `#<id>.<名前>` へ置換する。
    new_positions: Vec<(usize, String)>,
}

/// `nextval`／`currval`／`setval` の文字列引数（regclass 相当）の位置と値。
fn sequence_literal_args(tokens: &[Token]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for i in 0..tokens.len() {
        let is_call = matches!(tokens.get(i), Some(Token::Ident(w))
            if ["nextval", "currval", "setval"].iter().any(|k| w.eq_ignore_ascii_case(k)))
            && matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('(')));
        if !is_call {
            continue;
        }
        let at = i.saturating_add(2);
        if let Some(Token::StringLiteral(lit)) = tokens.get(at) {
            out.push((at, lit.clone()));
        }
    }
    out
}

/// 構文語と衝突する名前を内部キーの名前部に使わない（`relation_name::rewrite_relation_names_full` と同じ判定）。
fn reject_syntax_word_collision(name: &str) -> Result<(), SqlSurfaceError> {
    if crate::sql::relation_name::collides_with_syntax_word(name) {
        return Err(SqlSurfaceError::unsupported(
            "qualified relation name collides with a syntax word",
        ));
    }
    Ok(())
}

impl EngineCore {
    /// 要素つき `CREATE SCHEMA` の実行本体（呼び出し元の腕が `require_ddl(CreateSchema)` を通過済み）。
    ///
    /// 判定順: 要素ごとの `require_ddl`（カタログを引く前に `42501`）→ 所有者の解決と `AUTHORIZATION` の検査
    /// → 要素の名前解決（他の SCHEMA・`search_path`。カタログは確定済みスナップショット）→ write txn の中で
    /// SCHEMA 作成（`42P06`・`42939`・`54000`）→ 要素の作成（記述順ではなく PostgreSQL 17 と同じ
    /// SEQUENCE → TABLE → VIEW の順）→ 世代 bump と commit。
    pub(crate) fn execute_create_schema_with_elements(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedCreateSchema,
    ) -> Result<CreateSchemaOutcome, SqlSurfaceError> {
        // 実行順（種別順・同種は記述順。安定ソート）。
        let mut elements: Vec<&SchemaElement> = stmt.elements.iter().collect();
        elements.sort_by_key(|e| e.kind);

        // 要素ごとの DDL 権限ゲート。単独の `CREATE TABLE`／`VIEW`／`SEQUENCE` と同じ判定で、運用者相当だけが
        // 通る。カタログを引く前に置き、権限の無い主体へ SCHEMA・要素の有無を観測させない。
        let mut gated: Vec<SchemaElementKind> = Vec::new();
        for e in &elements {
            if gated.contains(&e.kind) {
                continue;
            }
            gated.push(e.kind);
            crate::sql::privilege_gate::require_ddl(
                session,
                match e.kind {
                    SchemaElementKind::Sequence => DdlStatementKind::CreateSequence,
                    SchemaElementKind::Table => DdlStatementKind::CreateTable,
                    SchemaElementKind::View => DdlStatementKind::CreateView,
                },
            )?;
        }

        // 所有者（`AUTHORIZATION` の指定、省略時は作成者＝束縛中の現在ロール）。要素は SCHEMA 所有者ではなく
        // 現在ロールの所有で作られるため、現在ロール以外を所有者に指定した要素つき作成は昇格経路になり得る。
        // 黙って別の所有者で作らず `0A000` で拒否する（fail-closed。設計記録 D2）。
        let owner = crate::sql::owner_ddl::resolve_schema_owner(
            &self.storage,
            ctx.tenant_id(),
            session,
            stmt.authorization.as_ref(),
        )?;
        if owner != session.current_role_id().filter(|id| *id != 0) {
            return Err(SqlSurfaceError::feature_not_supported(
                "CREATE SCHEMA with schema elements requires the schema owner to be the current role",
            ));
        }

        // 要素の名前解決（write txn を開く前。確定済みカタログだけを読む）。
        let mut prepared: Vec<PreparedElement> = Vec::with_capacity(elements.len());
        let mut known: Vec<String> = Vec::new();
        for e in &elements {
            let p = self.prepare_schema_element(ctx, session, &stmt.name, e, &known)?;
            if let Some(n) = &p.create_name {
                known.push(n.clone());
            }
            prepared.push(p);
        }

        let map_catalog = |e| crate::sql::ddl::map_schema_error(e, "CREATE SCHEMA failed");
        let write_txn = self
            .storage
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_catalog)?;
        let schema_id = crate::catalog::namespace::create_schema_in_txn(
            &write_txn,
            ctx.tenant_id(),
            &stmt.name,
            crate::catalog::namespace::MAX_SCHEMAS_PER_TENANT,
            owner,
        )
        .map_err(map_catalog)?;

        let mut bumped_tables: Vec<String> = Vec::new();
        for p in prepared {
            if let Some(table) =
                self.create_schema_element(ctx, session, &write_txn, schema_id, p)?
            {
                bumped_tables.push(table);
            }
        }
        Self::commit_schema_with_elements(write_txn, &bumped_tables)?;
        Ok(CreateSchemaOutcome {})
    }

    /// 作成した表の世代を進めてから commit する（`commit_boundary::commit` の悉皆検査の契約。表の作成の
    /// 世代 bump は commit の直前に行う）。commit は 1 回だけ。
    fn commit_schema_with_elements(
        write_txn: crate::storage::GatedWriteTxn,
        tables: &[String],
    ) -> Result<(), SqlSurfaceError> {
        let map_catalog = |e| crate::sql::ddl::map_schema_error(e, "CREATE SCHEMA failed");
        for table in tables {
            crate::catalog::bump_table_generation_in_txn(&write_txn, table).map_err(map_catalog)?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_catalog)
    }

    /// 要素 1 件の名前解決（write txn の外）。新しい SCHEMA を指す位置を集めて裸の名前にし、残りを既存の
    /// 解決器で解く。`known` は実行順で先に作られる要素の名前（未修飾の参照は新しい SCHEMA を path の先頭に
    /// 置いた扱いで、まずここへ解決する。PostgreSQL 17 と同じ。設計記録 D3 (d)）。
    fn prepare_schema_element(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        schema_name: &str,
        element: &SchemaElement,
        known: &[String],
    ) -> Result<PreparedElement, SqlSurfaceError> {
        let mut tokens = element.tokens.clone();
        let mut new_positions: Vec<(usize, String)> = Vec::new();
        let create_index = crate::sql::allowlist::schema_element_create_name_index(&tokens);

        // (1) 作成中の SCHEMA で修飾された関係位置（作成名・参照）を裸の名前へ落として位置を記録する。
        //     比較は SCHEMA の照会と同じく大文字小文字を区別する。
        for pos in crate::sql::relation_name::relation_positions(&tokens).positions {
            if let Some(Token::QualifiedIdent { qualifier, name }) = tokens.get(pos.index) {
                if qualifier == schema_name {
                    let name = name.clone();
                    new_positions.push((pos.index, name.clone()));
                    if let Some(slot) = tokens.get_mut(pos.index) {
                        *slot = Token::Ident(name);
                    }
                }
            }
        }

        // (2) 未修飾の作成名は新しい SCHEMA に作る。
        if let Some(i) = create_index {
            if let Some(Token::Ident(name)) = tokens.get(i) {
                if !new_positions.iter().any(|(p, _)| *p == i) {
                    new_positions.push((i, name.clone()));
                }
            }
        }
        let own_name = create_index.and_then(|i| match tokens.get(i) {
            Some(Token::Ident(n)) => Some(n.clone()),
            _ => None,
        });

        // (3) 未修飾の参照。一時表と同名の参照は PostgreSQL では pg_temp が暗黙に先頭のため、永続オブジェクトが
        //     一時表へ依存する形になる。黙って新しい SCHEMA の同名へ束縛せず `0A000`（設計記録 D2）。
        //     同じ文で先に作られる名前（表の自己参照を含む）は新しい SCHEMA を指す。
        let skip: Vec<usize> = new_positions.iter().map(|(i, _)| *i).collect();
        for (index, name) in crate::sql::search_path::element_relation_refs(&tokens, &skip) {
            let is_self_table =
                element.kind == SchemaElementKind::Table && own_name.as_deref() == Some(&name);
            // 作成中の表自身への参照は一時表との同名でも新しい SCHEMA の表を指すため、拒否の対象から外す。
            if !is_self_table && session.temp_tables().contains(&name) {
                return Err(SqlSurfaceError::feature_not_supported(
                    "CREATE SCHEMA elements cannot reference temporary tables",
                ));
            }
            if is_self_table || known.contains(&name) {
                new_positions.push((index, name));
            }
        }

        // (4) `nextval('q')` 等の文字列引数。新しい SCHEMA の修飾（`'s.q'`）と、同じ文で作られる名前は内部キーの
        //     `Ident` へ置き換える対象にする（文字列のまま残して利用者が内部キーを直接書けないようにする）。
        for (index, lit) in sequence_literal_args(&tokens) {
            let target = match lit.split_once('.') {
                Some((q, n))
                    if q == schema_name && crate::catalog::validate_identifier(n).is_ok() =>
                {
                    Some(n.to_string())
                }
                Some(_) => None,
                None if known.contains(&lit) => Some(lit.clone()),
                None => None,
            };
            if let Some(name) = target {
                new_positions.push((index, name.clone()));
                if let Some(slot) = tokens.get_mut(index) {
                    *slot = Token::Ident(name);
                }
            }
        }
        for (_, name) in &new_positions {
            reject_syntax_word_collision(name)?;
        }

        // (5) 他の SCHEMA・`public` の修飾を既存の解決器で解く（USAGE の検査つき。不在・他テナントは `3F000`）。
        let resolver = self.schema_resolver_for(ctx, session);
        let info =
            crate::sql::relation_name::rewrite_relation_names_full(&mut tokens, Some(&resolver))?;

        // (6) 残りの未修飾名を `search_path`（自テナントの SCHEMA・`public`）で解く。解決済みの位置は除く。
        let mut skip: Vec<usize> = new_positions.iter().map(|(i, _)| *i).collect();
        skip.extend(info.positions.iter().copied());
        let resolution =
            self.with_search_path_lookup(ctx.tenant_id(), session, None, |lookup| {
                crate::sql::search_path::resolve_path_refs(session, &tokens, &skip, lookup)
            })?;
        if let Some(e) = resolution.deferred_error {
            return Err(e);
        }
        crate::sql::search_path::apply_rewrites(&mut tokens, resolution.rewrites);
        // `public` が隠れた path で、どこにも解決されない未修飾名は固定文言の `42P01`。
        crate::sql::search_path::check_unqualified_refs(session, &tokens, &skip)?;

        Ok(PreparedElement {
            kind: element.kind,
            create_name: own_name,
            tokens,
            new_positions,
        })
    }

    /// 要素 1 件を、SCHEMA を作った write txn の中で作る。新しい SCHEMA を指す位置を内部キーへ置換し、
    /// 単独の DDL と同じ検証器を再実行してから、共有の txn 内本体で作る。作った表の名前（世代 bump 対象）を返す。
    fn create_schema_element(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        write_txn: &redb::WriteTransaction,
        schema_id: u64,
        prepared: PreparedElement,
    ) -> Result<Option<String>, SqlSurfaceError> {
        let PreparedElement {
            kind,
            create_name: _,
            mut tokens,
            new_positions,
        } = prepared;
        for (index, name) in new_positions {
            let key = RelationKey::InSchema { schema_id, name }.encode();
            if let Some(slot) = tokens.get_mut(index) {
                *slot = Token::Ident(key);
            }
        }
        match kind {
            SchemaElementKind::Table => {
                let validated = crate::sql::allowlist::validate_create_table_tokens(&tokens)?;
                let _owner = self.arm_created_relation_owner(ctx, session, &validated.table_name);
                crate::sql::ddl::create_table_element_in_txn(
                    &self.storage,
                    write_txn,
                    &validated,
                    &self.schema_limits,
                )?;
                Ok(Some(validated.table_name))
            }
            SchemaElementKind::View => {
                let validated = crate::sql::allowlist::validate_create_view_tokens(&tokens)?;
                let _owner = self.arm_created_relation_owner(ctx, session, validated.name());
                crate::sql::ddl::create_view_element_in_txn(write_txn, &validated)?;
                Ok(None)
            }
            SchemaElementKind::Sequence => {
                let validated = crate::sql::allowlist::validate_create_sequence_tokens(&tokens)?;
                // SEQUENCE は所有者行を持たないが、作成者の `SEQUENCES` の既定権限を初期 ACL へ適用するため
                // 作成の直前に作成者を予約する（単独の `CREATE SEQUENCE` と同じ）。
                let _owner = self.arm_created_relation_owner(ctx, session, validated.name());
                crate::sql::ddl::create_sequence_element_in_txn(
                    &self.storage,
                    write_txn,
                    &validated,
                )?;
                Ok(None)
            }
        }
    }
}
