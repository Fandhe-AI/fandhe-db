//! `MERGE INTO` 文の構文解析（SQL-79・Issue #3351。親 #3349、設計記録は `docs/design/sql-merge.md`）。
//!
//! 役割: トークン列から構文木 [`ValidatedMerge`] を作る。`core.rs` の文解析（`parse_tokens_in_impl`）が
//! 先頭語 `MERGE` を見て [`validate_merge_tokens_with_limits`] を呼び、`ParsedSql::Merge` を返す。
//! 実行系（`execute_parsed_in_session`・明示 Tx・Describe）は束縛（#3352。`sql::merge_bind`・
//! `core::merge_exec`）を通して実行する（`WHEN MATCHED` は #3353、`NOT MATCHED [BY TARGET|BY SOURCE]` は #3354）。
//! 対象外の形は固定文言の `0A000` で止める（副作用ゼロ）。台帳・`OpTag`・`EXPLAIN` は #3357、`RETURNING` の
//! `merge_action()` は #3356 が担当する。
//!
//! 構文段の保証範囲: 構文（`42601`）、上限（`54000`）、固定文言の未対応形（`0A000`）、対象・元表の存在
//! （`42P01`）。列解決・型照合は束縛段の責務で、本モジュールは行わない。`$n` の位置規則は
//! `sql::params_merge`（`USING OPERATION_ID $n` と SET 右辺・INSERT 値の単独の `$n`）が担う。
//! 文言は英語の固定文字列で、入力断片・件数・他テナントの情報を含めない。

use super::*;

/// `WHEN` 句の個数の上限（`MAX_CTE_DEFINITIONS`・集合演算の枝数と同値）。`Vec` へ積む前に検査する。
pub(crate) const MAX_MERGE_WHEN_CLAUSES: usize = 16;

/// `WHEN` 句の照合の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMatchKind {
    /// `WHEN MATCHED`。
    Matched,
    /// `WHEN NOT MATCHED [BY TARGET]`。
    NotMatchedByTarget,
    /// `WHEN NOT MATCHED BY SOURCE`。
    NotMatchedBySource,
}

/// `INSERT` 操作の値の与え方。
#[derive(Debug, Clone, PartialEq)]
pub enum MergeInsertValues {
    /// `VALUES (<値>, ...)`（要素は SET 右辺と同じ形。列参照は #3352 が束縛する）。
    Values(Vec<DmlFromSetValue>),
    /// `DEFAULT VALUES`。
    DefaultValues,
}

/// `WHEN` 句の操作。
#[derive(Debug, Clone, PartialEq)]
pub enum MergeAction {
    /// `UPDATE SET <代入>[, ...]`。
    Update {
        /// (列名, 右辺)。宣言順。
        assignments: Vec<(String, DmlFromSetValue)>,
    },
    /// `DELETE`。
    Delete,
    /// `INSERT [(<列>, ...)] { VALUES (...) | DEFAULT VALUES }`。
    Insert {
        /// 列リスト（省略は `None`）。
        columns: Option<Vec<String>>,
        /// 値。
        values: MergeInsertValues,
    },
    /// `DO NOTHING`。
    DoNothing,
}

/// `WHEN` 句 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct MergeWhenClause {
    /// 照合の種類。
    pub(crate) kind: MergeMatchKind,
    /// `AND <条件>`（文法は `ON` と同じ）。
    pub(crate) condition: Option<JoinWhereExpr>,
    /// 操作。
    pub(crate) action: MergeAction,
}

impl MergeWhenClause {
    /// 照合の種類。
    pub fn kind(&self) -> MergeMatchKind {
        self.kind
    }

    /// 操作。
    pub fn action(&self) -> &MergeAction {
        &self.action
    }
}

/// `USING` の元（表または部分クエリ）。
#[derive(Debug, Clone, PartialEq)]
pub enum MergeSource {
    /// 表（実表・単純形ビュー）。
    Table(TableRef),
    /// 部分クエリ（別名必須）。内側は既存の SELECT 検証器を通過済み。
    Subquery {
        /// 内側のトークン列（外側の括弧を除く）。
        query_tokens: Vec<Token>,
        /// 検証済みの内側の問い合わせ。
        query: Box<Statement>,
        /// 別名。
        alias: String,
        /// 列別名（`AS s (a, b)`）。
        column_aliases: Option<Vec<String>>,
    },
}

/// 構文検証を通過した `MERGE` 文（SQL-79）。台帳の文ハッシュ・`OpTag` は持たない（#3357）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ValidatedMerge {
    /// 対象表（別名を含む）。
    pub(crate) target: TableRef,
    /// 元。
    pub(crate) source: MergeSource,
    /// `ON` 条件（文法は結合 DML の `WHERE` と同じ）。
    pub(crate) on: JoinWhereExpr,
    /// `WHEN` 句（宣言順。1..=[`MAX_MERGE_WHEN_CLAUSES`]）。
    pub(crate) when_clauses: Vec<MergeWhenClause>,
    /// `RETURNING`（構造のみ。束縛は #3356）。
    pub(crate) returning: Option<crate::sql::dml_from_returning::DmlFromReturning>,
    /// 文末の `USING OPERATION_ID`。
    pub(crate) operation_id: Option<OperationId>,
}

impl ValidatedMerge {
    /// 対象の表名（別名ではなく実表名）。
    pub fn target_table(&self) -> &str {
        self.target.table()
    }

    /// 検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }

    /// `WHEN` 句（宣言順）。
    pub fn when_clauses(&self) -> &[MergeWhenClause] {
        &self.when_clauses
    }
}

/// 先頭語が `MERGE` か（`core.rs` の文解析が専用の解析器へ振り分ける条件）。`MERGE` の後が `INTO` でない
/// 形もここへ回して `42601` にする（既存の汎用経路の文言へ落とさない）。
pub(crate) fn looks_like_merge(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("MERGE"))
}

/// `MERGE` の別名にできない語（DML の句境界語に `WHEN` を加える）。
fn is_merge_reserved_word(w: &str) -> bool {
    is_dml_target_reserved_word(w) || w.eq_ignore_ascii_case("WHEN")
}

/// 部分クエリの位置情報（検証は構造解析の後に行う）。
enum ParsedSource {
    Table(TableRef),
    Subquery {
        span: (usize, usize),
        alias: String,
        column_aliases: Option<Vec<String>>,
    },
}

/// [`Parser::parse_merge`] の結果（元の部分クエリは未検証）。
struct ParsedMerge {
    target: TableRef,
    source: ParsedSource,
    on: JoinWhereExpr,
    when_clauses: Vec<MergeWhenClause>,
    returning: Option<crate::sql::dml_from_returning::DmlFromReturning>,
    operation_id: Option<OperationId>,
}

impl<'a> Parser<'a> {
    /// `MERGE INTO <対象> [[AS] <別名>] USING <表|(部分クエリ)> [[AS] <別名>] ON <条件> <WHEN 句>+
    /// [RETURNING ...] [USING OPERATION_ID '<id>']` の構造解析（呼び出し元は
    /// [`validate_merge_tokens_with_limits`] のみ）。
    fn parse_merge(&mut self) -> Result<ParsedMerge, SqlSurfaceError> {
        self.expect_contextual_keyword("MERGE")?;
        self.expect_contextual_keyword("INTO")?;
        let target = self.parse_merge_target_relation()?;
        self.expect_contextual_keyword("USING")?;
        let source = self.parse_merge_source(&target)?;
        self.expect_ident_matching("ON")?;
        let on = parse_join_where_dml(self)?;
        let when_clauses = self.parse_merge_when_clauses(&target)?;
        let returning = self.parse_dml_from_returning_clause(target.exposed_name())?;
        self.reject_merge_partitioned()?;
        let operation_id = self.parse_operation_id_clause()?;
        self.reject_merge_partitioned()?;
        Ok(ParsedMerge {
            target,
            source,
            on,
            when_clauses,
            returning,
            operation_id,
        })
    }

    /// 対象表 `<表> [[AS] <別名>]`。別名は `WHEN` を含む句境界語を拒否する（非引用のみ。
    /// 引用識別子は別名にできる）。`parse_dml_target_relation` との差は予約語判定だけ。
    fn parse_merge_target_relation(&mut self) -> Result<TableRef, SqlSurfaceError> {
        let table = self.expect_ident()?;
        crate::catalog::validate_relation_key(&table)
            .map_err(|e| SqlSurfaceError::unsupported(format!("invalid table name: {e}")))?;
        let alias = self.parse_relation_alias(is_merge_reserved_word, "MERGE target")?;
        Ok(match alias {
            Some(a) => TableRef::with_alias(table, a),
            None => TableRef::new(table),
        })
    }

    /// 文末の `PARTITIONED` は固定文言の `0A000`。
    fn reject_merge_partitioned(&self) -> Result<(), SqlSurfaceError> {
        if self.peek_ident_matches("PARTITIONED") {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE ... PARTITIONED is not supported",
            ));
        }
        Ok(())
    }

    /// 元の直後に JOIN 連鎖・カンマ区切りが続く形は固定文言の `0A000`。
    fn reject_merge_multi_source(&self) -> Result<(), SqlSurfaceError> {
        let joinish = ["JOIN", "CROSS", "INNER", "LEFT", "RIGHT", "FULL", "NATURAL"]
            .iter()
            .any(|w| self.peek_ident_matches(w));
        if joinish || matches!(self.peek(), Some(Token::Punct(','))) {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE supports a single table or subquery as the USING source",
            ));
        }
        Ok(())
    }

    /// `USING` の後ろの元。表（別名任意）または `( <問い合わせ> ) [AS] <別名> [(列別名, ...)]`。
    fn parse_merge_source(&mut self, target: &TableRef) -> Result<ParsedSource, SqlSurfaceError> {
        if self.peek_ident_matches("LATERAL") {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE does not support LATERAL in the USING source",
            ));
        }
        let source = if matches!(self.peek(), Some(Token::Punct('('))) {
            let start = self.pos.saturating_add(1);
            let mut depth: usize = 1;
            let mut idx = start;
            let end = loop {
                match self.tokens.get(idx) {
                    None => {
                        return Err(SqlSurfaceError::unsupported(
                            "unterminated subquery in MERGE USING",
                        ))
                    }
                    Some(Token::Punct('(')) => depth = depth.saturating_add(1),
                    Some(Token::Punct(')')) => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break idx;
                        }
                    }
                    Some(_) => {}
                }
                idx = idx.saturating_add(1);
            };
            if end <= start {
                return Err(SqlSurfaceError::unsupported(
                    "empty subquery in MERGE USING",
                ));
            }
            self.pos = end.saturating_add(1);
            // 別名は必須（派生表と同じ形）。省略・予約語は `42601`。
            if self.peek_ident_matches("AS") {
                self.advance();
            } else if !matches!(self.peek(), Some(Token::Ident(w)) if !is_merge_reserved_word(w)) {
                return Err(SqlSurfaceError::unsupported(
                    "subquery in MERGE USING requires an alias",
                ));
            }
            let alias = self.expect_ident()?;
            if is_merge_reserved_word(&alias) {
                return Err(SqlSurfaceError::unsupported(
                    "reserved word cannot be used as a MERGE source alias",
                ));
            }
            crate::catalog::validate_identifier(&alias).map_err(|e| {
                SqlSurfaceError::unsupported(format!("invalid MERGE source alias: {e}"))
            })?;
            let column_aliases = if matches!(self.peek(), Some(Token::Punct('('))) {
                self.advance();
                let mut cols: Vec<String> = Vec::new();
                loop {
                    if cols.len() >= MAX_COLUMN_COUNT {
                        return Err(SqlSurfaceError::payload_too_large(
                            "too many MERGE source column aliases",
                        ));
                    }
                    cols.push(self.expect_ident()?);
                    if matches!(self.peek(), Some(Token::Punct(','))) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect_punct(')')?;
                Some(cols)
            } else {
                None
            };
            if alias == target.exposed_name() {
                return Err(SqlSurfaceError::duplicate_alias(&alias));
            }
            ParsedSource::Subquery {
                span: (start, end),
                alias,
                column_aliases,
            }
        } else {
            let table = self.expect_ident()?;
            crate::catalog::validate_relation_key(&table)
                .map_err(|e| SqlSurfaceError::unsupported(format!("invalid table name: {e}")))?;
            let alias = self.parse_relation_alias(is_merge_reserved_word, "MERGE source")?;
            let rel = match alias {
                Some(a) => TableRef::with_alias(table, a),
                None => TableRef::new(table),
            };
            if rel.exposed_name() == target.exposed_name() {
                return Err(SqlSurfaceError::duplicate_alias(rel.exposed_name()));
            }
            ParsedSource::Table(rel)
        };
        self.reject_merge_multi_source()?;
        Ok(source)
    }

    /// `WHEN` 句の並び（1 個以上）。個数の上限は積む前に検査し、到達できない `WHEN` は `42601`。
    fn parse_merge_when_clauses(
        &mut self,
        target: &TableRef,
    ) -> Result<Vec<MergeWhenClause>, SqlSurfaceError> {
        let mut clauses: Vec<MergeWhenClause> = Vec::new();
        let mut set_total: usize = 0;
        while self.peek_ident_matches("WHEN") {
            if clauses.len() >= MAX_MERGE_WHEN_CLAUSES {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many MERGE WHEN clauses",
                ));
            }
            self.advance();
            let kind = self.parse_merge_match_kind()?;
            let condition = if matches!(self.peek(), Some(Token::Keyword(Keyword::And))) {
                self.advance();
                Some(parse_join_where_dml(self)?)
            } else {
                None
            };
            self.expect_ident_matching("THEN")?;
            let action = self.parse_merge_action(kind, target)?;
            if let MergeAction::Update { assignments } = &action {
                set_total = set_total.saturating_add(assignments.len());
                if set_total > MAX_UPDATE_SET_ASSIGNMENTS {
                    return Err(SqlSurfaceError::unsupported(
                        "too many UPDATE SET assignments",
                    ));
                }
            }
            // 同じ種類の条件なし WHEN の後ろにある同じ種類の WHEN は到達できない。
            if clauses
                .iter()
                .any(|c| c.kind == kind && c.condition.is_none())
            {
                return Err(SqlSurfaceError::unsupported(
                    "unreachable MERGE WHEN clause",
                ));
            }
            clauses.push(MergeWhenClause {
                kind,
                condition,
                action,
            });
        }
        if clauses.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "MERGE requires at least one WHEN clause",
            ));
        }
        Ok(clauses)
    }

    /// `MATCHED`／`NOT MATCHED [BY SOURCE|BY TARGET]`。
    fn parse_merge_match_kind(&mut self) -> Result<MergeMatchKind, SqlSurfaceError> {
        let not = self.peek_ident_matches("NOT");
        if not {
            self.advance();
        }
        self.expect_ident_matching("MATCHED")?;
        let by = matches!(self.peek(), Some(Token::Keyword(Keyword::By)));
        if !by {
            return Ok(if not {
                MergeMatchKind::NotMatchedByTarget
            } else {
                MergeMatchKind::Matched
            });
        }
        // `WHEN MATCHED BY ...` は構文エラー（`NOT MATCHED` のときだけ `BY` を取る）。
        if !not {
            return Err(SqlSurfaceError::unsupported(
                "BY SOURCE/TARGET requires NOT MATCHED",
            ));
        }
        self.advance();
        if self.peek_ident_matches("SOURCE") {
            self.advance();
            Ok(MergeMatchKind::NotMatchedBySource)
        } else if self.peek_ident_matches("TARGET") {
            self.advance();
            Ok(MergeMatchKind::NotMatchedByTarget)
        } else {
            Err(SqlSurfaceError::unsupported(
                "expected SOURCE or TARGET after BY",
            ))
        }
    }

    /// `THEN` の後ろの操作。種類との組み合わせ（`MATCHED`／`BY SOURCE` は UPDATE・DELETE・DO NOTHING、
    /// `BY TARGET` は INSERT・DO NOTHING）が不正なら `42601`。
    fn parse_merge_action(
        &mut self,
        kind: MergeMatchKind,
        target: &TableRef,
    ) -> Result<MergeAction, SqlSurfaceError> {
        let mismatch = || SqlSurfaceError::unsupported("MERGE action is not valid for this WHEN");
        let by_target = kind == MergeMatchKind::NotMatchedByTarget;
        if self.peek_ident_matches("DO") {
            self.advance();
            self.expect_ident_matching("NOTHING")?;
            return Ok(MergeAction::DoNothing);
        }
        if self.peek_ident_matches("DELETE") {
            if by_target {
                return Err(mismatch());
            }
            self.advance();
            return Ok(MergeAction::Delete);
        }
        if self.peek_ident_matches("UPDATE") {
            if by_target {
                return Err(mismatch());
            }
            self.advance();
            self.expect_ident_matching("SET")?;
            let prev = self
                .dml_from_target
                .replace(target.exposed_name().to_string());
            let mut assignments = Vec::new();
            let items = (|| {
                self.parse_update_from_set_item(&mut assignments)?;
                while matches!(self.peek(), Some(Token::Punct(','))) {
                    self.advance();
                    self.parse_update_from_set_item(&mut assignments)?;
                }
                Ok::<(), SqlSurfaceError>(())
            })();
            self.dml_from_target = prev;
            items?;
            return Ok(MergeAction::Update { assignments });
        }
        if self.peek_ident_matches("INSERT") {
            if !by_target {
                return Err(mismatch());
            }
            self.advance();
            return self.parse_merge_insert(target);
        }
        Err(SqlSurfaceError::unsupported(
            "expected UPDATE, DELETE, INSERT, or DO NOTHING after THEN",
        ))
    }

    /// `INSERT` の本体（`INSERT` は消費済み）。`OVERRIDING` は固定文言の `0A000`。
    fn parse_merge_insert(&mut self, target: &TableRef) -> Result<MergeAction, SqlSurfaceError> {
        let mut columns: Option<Vec<String>> = None;
        if matches!(self.peek(), Some(Token::Punct('('))) {
            self.advance();
            let mut cols: Vec<String> = Vec::new();
            loop {
                if cols.len() >= MAX_COLUMN_COUNT {
                    return Err(SqlSurfaceError::unsupported("too many INSERT columns"));
                }
                cols.push(self.expect_ident()?);
                if matches!(self.peek(), Some(Token::Punct(','))) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect_punct(')')?;
            columns = Some(cols);
        }
        if self.peek_ident_matches("OVERRIDING") {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE ... INSERT ... OVERRIDING is not supported",
            ));
        }
        if self.peek_ident_matches("DEFAULT") && !self.quoted_null.contains(&self.pos) {
            self.advance();
            self.expect_ident_matching("VALUES")?;
            return Ok(MergeAction::Insert {
                columns,
                values: MergeInsertValues::DefaultValues,
            });
        }
        self.expect_ident_matching("VALUES")?;
        self.expect_punct('(')?;
        let prev = self
            .dml_from_target
            .replace(target.exposed_name().to_string());
        let mut values: Vec<DmlFromSetValue> = Vec::new();
        let parsed = (|| {
            loop {
                if values.len() >= MAX_COLUMN_COUNT {
                    return Err(SqlSurfaceError::unsupported("too many INSERT values"));
                }
                values.push(self.parse_update_from_set_rhs()?);
                if matches!(self.peek(), Some(Token::Punct(','))) {
                    self.advance();
                } else {
                    break;
                }
            }
            Ok::<(), SqlSurfaceError>(())
        })();
        self.dml_from_target = prev;
        parsed?;
        self.expect_punct(')')?;
        if let Some(cols) = &columns {
            if cols.len() != values.len() {
                return Err(SqlSurfaceError::unsupported(
                    "INSERT has a different number of columns and values",
                ));
            }
        }
        Ok(MergeAction::Insert {
            columns,
            values: MergeInsertValues::Values(values),
        })
    }
}

/// `MERGE` の検証入口（SQL-79・Issue #3351）。検証順序: 構造（`42601`・`54000`・固定文言の `0A000`）→
/// `operation_id` 必須化 → 対象のカタログ確認（未知は `42P01`。ビューは存在すれば受理）→ 元の確認
/// （表・単純形ビュー。部分クエリは既存の SELECT 検証器）。実行は本 Issue の範囲では `0A000`。
pub(crate) fn validate_merge_tokens_with_limits(
    tokens: &[Token],
    mode: LedgerMode,
    limits: crate::nesting_limits::NestingLimits,
    lookup: &impl TableLookup,
) -> Result<ValidatedMerge, SqlSurfaceError> {
    let mut p = Parser::new(tokens).with_nesting_limits(limits);
    let parsed = p.parse_merge()?;
    p.expect_end_of_statement()?;
    mode.require(parsed.operation_id.as_ref())?;

    // 対象: 未知の名前は `resolve_from` が `42P01`。ビューは存在すれば構文段では受理する（#3352・#3355）。
    crate::sql::view::resolve_from(lookup, parsed.target.table())?;

    let source = match parsed.source {
        ParsedSource::Table(rel) => {
            match crate::sql::view::resolve_from(lookup, rel.table())? {
                crate::sql::view::Resolved::Table => {}
                crate::sql::view::Resolved::View {
                    column_map: None, ..
                } => {}
                crate::sql::view::Resolved::View { .. }
                | crate::sql::view::Resolved::Buffered { .. } => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "MERGE supports only simple views without column aliases as the USING source",
                    ));
                }
            }
            MergeSource::Table(rel)
        }
        ParsedSource::Subquery {
            span: (start, end),
            alias,
            column_aliases,
        } => {
            let slice = tokens
                .get(start..end)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
            let query_tokens = normalize_inner_query_tokens(slice.to_vec_registered());
            let query =
                validate_sql_tokens_with_subquery_ctx_limits(&query_tokens, lookup, 0, limits)?;
            require_row_query_statement(&query, "MERGE")?;
            // 距離順（ベクトル順位付け）の問い合わせは元にできない（SQL-28 と同じ扱い）。
            if matches!(query, Statement::Select(_)) {
                return Err(SqlSurfaceError::unsupported(
                    "MERGE source cannot be a vector ranking query",
                ));
            }
            MergeSource::Subquery {
                query_tokens,
                query: Box::new(query),
                alias,
                column_aliases,
            }
        }
    };
    Ok(ValidatedMerge {
        target: parsed.target,
        source,
        on: parsed.on,
        when_clauses: parsed.when_clauses,
        returning: parsed.returning,
        operation_id: parsed.operation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実表 `t`・`s`・`u` だけを持つ最小のカタログ（ビューなし）。
    struct Cat;
    impl TableLookup for Cat {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(["t", "s", "u"].contains(&name))
        }
    }

    fn parse(sql: &str) -> Result<ValidatedMerge, SqlSurfaceError> {
        let tokens = lexer::tokenize(sql).expect("lex");
        validate_merge_tokens_with_limits(
            &tokens,
            LedgerMode::Optional,
            crate::nesting_limits::NestingLimits::default(),
            &Cat,
        )
    }

    fn code(sql: &str) -> &'static str {
        parse(sql).expect_err(sql).wire_code()
    }

    const HEAD: &str = "MERGE INTO t USING s ON t.id = s.id";

    /// SET 右辺の引用付き `"case"` は列参照として受理する（非引用の `CASE WHEN` と区別。Issue #3351）。
    #[test]
    fn accepts_quoted_case_column_in_set_rhs() {
        let tokens = lexer::tokenize_marked(&format!(
            "{HEAD} WHEN MATCHED THEN UPDATE SET v = \"case\" WHEN NOT MATCHED THEN DO NOTHING"
        ))
        .expect("lex");
        validate_merge_tokens_with_limits(
            &tokens,
            LedgerMode::Optional,
            crate::nesting_limits::NestingLimits::default(),
            &Cat,
        )
        .expect("quoted case column");
    }

    #[test]
    fn accepts_the_three_when_kinds_and_actions() {
        let v = parse(&format!(
            "{HEAD} WHEN MATCHED AND s.v = 1 THEN UPDATE SET v = s.v, w = s.w + 1 \
             WHEN MATCHED THEN DELETE \
             WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v) \
             WHEN NOT MATCHED BY SOURCE AND t.v = 2 THEN DO NOTHING \
             WHEN NOT MATCHED BY SOURCE THEN DELETE;"
        ))
        .expect("ok");
        assert_eq!(v.target_table(), "t");
        let kinds: Vec<_> = v.when_clauses().iter().map(|w| w.kind()).collect();
        assert_eq!(
            kinds,
            [
                MergeMatchKind::Matched,
                MergeMatchKind::Matched,
                MergeMatchKind::NotMatchedByTarget,
                MergeMatchKind::NotMatchedBySource,
                MergeMatchKind::NotMatchedBySource,
            ]
        );
    }

    #[test]
    fn accepts_aliases_default_values_returning_and_operation_id() {
        parse("MERGE INTO t AS d USING s AS x ON d.id = x.id WHEN NOT MATCHED BY TARGET THEN INSERT DEFAULT VALUES")
            .expect("alias");
        parse("MERGE INTO t d USING s x ON d.id = x.id WHEN NOT MATCHED THEN INSERT VALUES (x.id, DEFAULT)")
            .expect("implicit alias, no column list");
        let v = parse(&format!(
            "{HEAD} WHEN MATCHED THEN DO NOTHING RETURNING t.id USING OPERATION_ID 'op-1'"
        ))
        .expect("returning + operation_id");
        assert!(v.returning.is_some());
        assert!(v.operation_id().is_some());
    }

    #[test]
    fn accepts_subquery_source_with_column_aliases() {
        let v = parse(
            "MERGE INTO t USING (SELECT id, v FROM u) AS x (id, v) ON t.id = x.id \
             WHEN MATCHED THEN UPDATE SET v = x.v",
        )
        .expect("subquery");
        assert!(matches!(v.source, MergeSource::Subquery { .. }));
    }

    #[test]
    fn syntax_errors_are_42601() {
        for sql in [
            // WHEN 句が無い
            HEAD,
            // THEN 欠落
            &format!("{HEAD} WHEN MATCHED DELETE"),
            // 種類と操作の不整合
            &format!("{HEAD} WHEN MATCHED THEN INSERT DEFAULT VALUES"),
            &format!("{HEAD} WHEN NOT MATCHED THEN DELETE"),
            &format!("{HEAD} WHEN NOT MATCHED BY SOURCE THEN INSERT DEFAULT VALUES"),
            &format!("{HEAD} WHEN NOT MATCHED BY TARGET THEN UPDATE SET v = 1"),
            // MATCHED に BY は付けられない
            &format!("{HEAD} WHEN MATCHED BY SOURCE THEN DELETE"),
            // INSERT の列数と値の数の不一致
            &format!("{HEAD} WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id)"),
            // 到達できない WHEN
            &format!("{HEAD} WHEN MATCHED THEN DELETE WHEN MATCHED THEN DO NOTHING"),
            // INTO 欠落・元の別名なし部分クエリ・入れ子の MERGE
            "MERGE t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            "MERGE INTO t USING (SELECT id FROM u) ON t.id = id WHEN MATCHED THEN DELETE",
            "MERGE INTO t USING (MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE) x ON t.id = x.id WHEN MATCHED THEN DELETE",
            // USING OPERATION_ID は文末のみ
            "MERGE INTO t USING OPERATION_ID 'x' ON t.id = 1 WHEN MATCHED THEN DELETE",
            // 余剰トークン
            &format!("{HEAD} WHEN MATCHED THEN DELETE foo"),
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
    }

    #[test]
    fn unsupported_shapes_are_0a000() {
        for sql in [
            "MERGE INTO t USING s JOIN u ON s.id = u.id ON t.id = s.id WHEN MATCHED THEN DELETE",
            "MERGE INTO t USING s, u ON t.id = s.id WHEN MATCHED THEN DELETE",
            "MERGE INTO t USING LATERAL (SELECT 1) x ON t.id = x.id WHEN MATCHED THEN DELETE",
            &format!("{HEAD} WHEN MATCHED THEN DELETE PARTITIONED"),
            &format!(
                "{HEAD} WHEN NOT MATCHED THEN INSERT (id) OVERRIDING SYSTEM VALUE VALUES (s.id)"
            ),
        ] {
            assert_eq!(code(sql), "0A000", "{sql}");
        }
    }

    #[test]
    fn missing_tables_are_42p01() {
        assert_eq!(
            code("MERGE INTO nope USING s ON nope.id = s.id WHEN MATCHED THEN DELETE"),
            "42P01"
        );
        assert_eq!(
            code("MERGE INTO t USING nope ON t.id = nope.id WHEN MATCHED THEN DELETE"),
            "42P01"
        );
    }

    #[test]
    fn same_exposed_name_is_42712() {
        assert_eq!(
            code("MERGE INTO t USING t ON t.id = t.id WHEN MATCHED THEN DELETE"),
            "42712"
        );
    }

    #[test]
    fn when_clause_count_boundary() {
        // 条件つきの WHEN は同じ種類を何度でも書ける（到達不能にならない）。
        let clause = " WHEN MATCHED AND s.v = 1 THEN DO NOTHING";
        let ok = format!("{HEAD}{}", clause.repeat(MAX_MERGE_WHEN_CLAUSES));
        parse(&ok).expect("16 clauses");
        let too_many = format!("{HEAD}{}", clause.repeat(MAX_MERGE_WHEN_CLAUSES + 1));
        assert_eq!(code(&too_many), "54000");
    }

    #[test]
    fn set_assignment_total_is_bounded_across_when_clauses() {
        let assigns = (0..200)
            .map(|i| format!("c{i} = 1"))
            .collect::<Vec<_>>()
            .join(", ");
        let clause = format!(" WHEN MATCHED AND s.v = 1 THEN UPDATE SET {assigns}");
        parse(&format!("{HEAD}{clause}")).expect("within the cap");
        let err = parse(&format!("{HEAD}{clause}{clause}")).expect_err("over the cap");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn operation_id_is_required_when_the_ledger_demands_it() {
        let tokens = lexer::tokenize(&format!("{HEAD} WHEN MATCHED THEN DELETE")).expect("lex");
        let err = validate_merge_tokens_with_limits(
            &tokens,
            LedgerMode::Ledgered,
            crate::nesting_limits::NestingLimits::default(),
            &Cat,
        )
        .expect_err("required");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn with_clause_and_data_modifying_cte_are_0a000() {
        for sql in [
            "WITH x AS (SELECT id FROM u) MERGE INTO t USING x ON t.id = x.id WHEN MATCHED THEN DELETE",
            "WITH x AS (MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE) SELECT 1",
        ] {
            let tokens = lexer::tokenize(sql).expect("lex");
            let err = validate_sql_tokens_with_subquery_ctx_limits(
                &tokens,
                &Cat,
                0,
                crate::nesting_limits::NestingLimits::default(),
            )
            .expect_err(sql);
            assert_eq!(err.wire_code(), "0A000", "{sql}");
        }
    }
}
