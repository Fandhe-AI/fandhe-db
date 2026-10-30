//! `plpgsql` 構文解析器の単体テスト（Issue #2224・SQL-68）。
//!
//! 受理する構文の網羅（ADR の構文表の「受理」列）、範囲外（`0A000`）、構文誤り（`42601`）、
//! 上限（`54000`／`54001`）、`$n` の範囲外（`42P02`）、エラー文言の非漏えいを固定する。
//! `pub(crate)` モジュールのため crate 内テストとして置く（結合テストからは呼べない）。

use super::ast::*;
use super::embedded::{dummy_token, validate_embedded, EmbeddedSqlChecker, StructuralChecker};
use super::lexer::{scan, TokKind};
use super::{parse_body, ParseOptions};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::embedded_unit::UnitSummary;
use crate::sql::lexer::{tokenize, Keyword, Token};

fn opts_with(returns: ReturnShape) -> ParseOptions {
    ParseOptions::new(
        vec![
            ParamDecl {
                name: Some("a".to_string()),
                ty: SqlColumnTypeName::Integer,
            },
            ParamDecl {
                name: None,
                ty: SqlColumnTypeName::Text,
            },
        ],
        returns,
    )
}

fn opts() -> ParseOptions {
    opts_with(ReturnShape::Scalar)
}

fn ok_with(src: &str, o: &ParseOptions) -> Body {
    match parse_body(src, o) {
        Ok(b) => b,
        Err(e) => panic!("expected accept for {src:?}, got {} ({e})", e.wire_code()),
    }
}

fn ok(src: &str) -> Body {
    ok_with(src, &opts())
}

fn code_with(src: &str, o: &ParseOptions) -> &'static str {
    match parse_body(src, o) {
        Ok(_) => panic!("expected reject for {src:?}"),
        Err(e) => e.wire_code(),
    }
}

fn code(src: &str) -> &'static str {
    code_with(src, &opts())
}

/// 解析して、SQL 構文解析器の記録で定義時分類まで行う（`var_refs`・文種・書き込みの有無が確定する）。
fn classified_with(src: &str, o: &ParseOptions) -> Body {
    let mut b = ok_with(src, o);
    if let Err(e) = validate_embedded(&mut b, &StructuralChecker) {
        panic!("expected classify for {src:?}, got {} ({e})", e.wire_code());
    }
    b
}

fn classified(src: &str) -> Body {
    classified_with(src, &opts())
}

/// 解析は通り、定義時分類で拒否されるときの `wire_code`。
fn classify_code_with(src: &str, o: &ParseOptions) -> &'static str {
    let mut b = ok_with(src, o);
    match validate_embedded(&mut b, &StructuralChecker) {
        Ok(()) => panic!("expected classify reject for {src:?}"),
        Err(e) => e.wire_code(),
    }
}

fn classify_code(src: &str) -> &'static str {
    classify_code_with(src, &opts())
}

fn vid(b: &Body, name: &str) -> VarId {
    let i = b
        .vars
        .iter()
        .position(|v| v.name == name)
        .unwrap_or_else(|| panic!("no var {name}"));
    VarId(i as u32)
}

// ---- 受理 ----

#[test]
fn accepts_blocks_and_declarations() {
    let srcs = [
        "BEGIN NULL; END",
        "BEGIN NULL; END;",
        "<<outer>> DECLARE x int := 1; y CONSTANT text DEFAULT 'a'; z numeric(10,2) NOT NULL = 0; \
         r RECORD; t users.id%TYPE; w users%ROWTYPE; c varchar(5); BEGIN x := x + 1; END outer;",
        "DECLARE x int; BEGIN DECLARE x int; BEGIN x := 2; END; x := 1; END",
        "<<l>> DECLARE x int; BEGIN <<inner>> BEGIN l.x := 1; END inner; END",
        "DECLARE xs int[]; ts timestamptz; BEGIN NULL; END",
    ];
    for s in srcs {
        ok(s);
    }
}

#[test]
fn accepts_assignment_and_resolves_variables() {
    let b = classified("DECLARE x int; BEGIN x := a + x + $1; x = 3; END");
    let x = vid(&b, "x");
    let a = vid(&b, "a");
    assert_eq!(a, VarId(0));
    assert!(matches!(b.vars[a.0 as usize].kind, VarKind::Param(1)));
    let Some(Stmt::Assign { target, expr }) = b.block.stmts.first() else {
        panic!("assign expected");
    };
    assert_eq!(*target, AssignTarget::Var(x));
    // `a`・`x`・`$1` の 3 参照（`$1` は引数 1 番）。
    assert_eq!(expr.var_refs.len(), 3);
    assert_eq!(
        (expr.var_refs[0].pos, &expr.var_refs[0].var),
        (0, &VarRef::Var(a))
    );
    assert_eq!(
        (expr.var_refs[1].pos, &expr.var_refs[1].var),
        (2, &VarRef::Var(x))
    );
    assert_eq!(
        (expr.var_refs[2].pos, &expr.var_refs[2].var),
        (4, &VarRef::Var(a))
    );
    assert_eq!(b.block.stmts.len(), 2);
}

#[test]
fn inner_declaration_hides_outer() {
    let b = ok("DECLARE x int; BEGIN DECLARE x text; BEGIN x := 'v'; END; END");
    let Some(Stmt::Block(inner)) = b.block.stmts.first() else {
        panic!("inner block");
    };
    let Some(Stmt::Assign { target, .. }) = inner.stmts.first() else {
        panic!("assign");
    };
    let outer_x = b.block.decls[0];
    let inner_x = inner.decls[0];
    assert_ne!(outer_x, inner_x);
    assert_eq!(*target, AssignTarget::Var(inner_x));
    assert_eq!(b.vars[inner_x.0 as usize].scope_depth, 2);
}

#[test]
fn label_qualified_reference_resolves_outer_variable() {
    let b = classified("<<o>> DECLARE x int; BEGIN DECLARE x int; BEGIN x := o.x; END; END");
    let Some(Stmt::Block(inner)) = b.block.stmts.first() else {
        panic!("inner block");
    };
    let Some(Stmt::Assign { expr, .. }) = inner.stmts.first() else {
        panic!("assign");
    };
    assert_eq!(expr.var_refs.len(), 1);
    assert_eq!(
        (expr.var_refs[0].pos, &expr.var_refs[0].var),
        (0, &VarRef::Var(b.block.decls[0]))
    );
}

#[test]
fn record_field_reference_and_target() {
    let b = classified("DECLARE r RECORD; n int; BEGIN n := r.f; r.g := 1; END");
    let r = vid(&b, "r");
    let Some(Stmt::Assign { expr, .. }) = b.block.stmts.first() else {
        panic!("assign");
    };
    assert_eq!(expr.var_refs.len(), 1);
    assert_eq!(
        (expr.var_refs[0].pos, &expr.var_refs[0].var),
        (0, &VarRef::Field(r, "f".to_string()))
    );
    let Some(Stmt::Assign { target, .. }) = b.block.stmts.get(1) else {
        panic!("assign2");
    };
    assert_eq!(*target, AssignTarget::Field(r, "g".to_string()));
}

#[test]
fn accepts_declaration_types() {
    let b =
        ok("DECLARE t users.id%TYPE; w users%ROWTYPE; r RECORD; n numeric(10,2); BEGIN NULL; END");
    assert!(matches!(
        &b.vars[vid(&b, "t").0 as usize].ty,
        DeclType::PercentType { path } if path == &["users", "id"]
    ));
    assert!(matches!(
        &b.vars[vid(&b, "w").0 as usize].ty,
        DeclType::PercentRowType { path } if path == &["users"]
    ));
    assert_eq!(b.vars[vid(&b, "r").0 as usize].ty, DeclType::Record);
    assert!(matches!(
        b.vars[vid(&b, "n").0 as usize].ty,
        DeclType::Named(SqlColumnTypeName::Numeric { .. })
    ));
}

#[test]
fn accepts_if_and_case() {
    let b = ok(
        "BEGIN IF a > 1 THEN NULL; ELSIF a = 1 THEN NULL; ELSEIF a = 0 THEN NULL; ELSE NULL; END IF; \
         CASE a WHEN 1, 2 THEN NULL; WHEN 3 THEN NULL; ELSE NULL; END CASE; \
         CASE WHEN a = 1 THEN NULL; WHEN CASE WHEN a = 2 THEN true ELSE false END THEN NULL; END CASE; END",
    );
    let Some(Stmt::If { branches, else_ }) = b.block.stmts.first() else {
        panic!("if");
    };
    assert_eq!(branches.len(), 3);
    assert!(else_.is_some());
    let Some(Stmt::Case {
        selector, whens, ..
    }) = b.block.stmts.get(1)
    else {
        panic!("case");
    };
    assert!(selector.is_some());
    assert_eq!(whens[0].0.len(), 2);
    let Some(Stmt::Case {
        selector,
        whens,
        else_,
    }) = b.block.stmts.get(2)
    else {
        panic!("searched case");
    };
    assert!(selector.is_none() && else_.is_none());
    assert_eq!(whens.len(), 2);
}

#[test]
fn accepts_loops() {
    let b = classified(
        "DECLARE s int := 0; BEGIN \
         <<l1>> LOOP EXIT l1 WHEN s > 3; CONTINUE WHEN s = 1; s := s + 1; END LOOP l1; \
         WHILE s < 10 LOOP s := s + 1; END LOOP; \
         FOR i IN 1..10 LOOP s := s + i; END LOOP; \
         FOR i IN REVERSE 10..1 BY 2 LOOP NULL; END LOOP; \
         <<q>> FOR rec IN SELECT id FROM t WHERE id > a LOOP s := rec.id; CONTINUE q; END LOOP q; \
         FOR rec2 IN WITH c AS (SELECT id FROM t) SELECT id FROM c LOOP EXIT; END LOOP; \
         END",
    );
    assert_eq!(b.block.stmts.len(), 6);
    let Some(Stmt::ForInt {
        var, reverse, step, ..
    }) = b.block.stmts.get(3)
    else {
        panic!("for int");
    };
    assert!(*reverse && step.is_some());
    assert_eq!(b.vars[var.0 as usize].kind, VarKind::LoopInt);
    let Some(Stmt::ForQuery { targets, query, .. }) = b.block.stmts.get(4) else {
        panic!("for query");
    };
    assert_eq!(targets.len(), 1);
    // クエリ中の `a` は引数参照になる。
    assert!(query
        .var_refs
        .iter()
        .any(|r| r.var == VarRef::Var(VarId(0))));
    let Some(Stmt::ForInt { lower, upper, .. }) = b.block.stmts.get(2) else {
        panic!("for int 1..10");
    };
    assert_eq!(lower.tokens, vec![Token::Number("1".to_string())]);
    assert_eq!(upper.tokens, vec![Token::Number("10".to_string())]);
}

#[test]
fn accepts_exit_continue_with_block_label() {
    ok("BEGIN <<b>> BEGIN EXIT b; END; END");
    ok("BEGIN <<l>> LOOP <<b>> BEGIN EXIT l; END; END LOOP; END");
    ok("BEGIN LOOP <<b>> BEGIN CONTINUE; END; END LOOP; END");
}

#[test]
fn accepts_return_forms() {
    ok("BEGIN RETURN a + 1; END");
    let o = opts_with(ReturnShape::SetOf);
    ok_with(
        "BEGIN RETURN NEXT a; RETURN QUERY SELECT 1; RETURN; END",
        &o,
    );
    ok_with("BEGIN RETURN NEW; END", &opts_with(ReturnShape::Trigger));
    ok_with("BEGIN RETURN; END", &opts_with(ReturnShape::Void));
}

#[test]
fn accepts_sql_statements() {
    let b = classified(
        "DECLARE n int; r RECORD; BEGIN \
         SELECT count(*) INTO STRICT n FROM t; \
         SELECT id, lang INTO r FROM t LIMIT 1; \
         PERFORM 1; PERFORM f(a); \
         INSERT INTO t (x) VALUES (a) RETURNING id INTO n; \
         UPDATE t SET x = a WHERE id = n; \
         DELETE FROM t WHERE id = a; \
         EXECUTE 'select 1' INTO n USING a, 2; \
         EXECUTE 'select $1' USING a INTO n; \
         GET DIAGNOSTICS n = ROW_COUNT; \
         GET CURRENT DIAGNOSTICS n := ROW_COUNT; \
         IF FOUND THEN NULL; END IF; \
         END",
    );
    let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
        panic!("select into");
    };
    let into = s.into.as_ref().expect("into");
    assert!(into.strict);
    assert_eq!(into.targets, vec![AssignTarget::Var(vid(&b, "n"))]);
    // INTO は除去され、FROM 以降が残る。
    assert!(!s
        .tokens
        .iter()
        .any(|t| matches!(t, Token::Ident(i) if i.eq_ignore_ascii_case("INTO"))));
    let Some(Stmt::Perform(p)) = b.block.stmts.get(2) else {
        panic!("perform");
    };
    assert_eq!(p.tokens.first(), Some(&Token::Keyword(Keyword::Select)));
    // INSERT INTO の INTO は残り、RETURNING ... INTO は除去される。
    let Some(Stmt::Sql(ins)) = b.block.stmts.get(4) else {
        panic!("insert");
    };
    assert!(ins
        .tokens
        .iter()
        .any(|t| matches!(t, Token::Ident(i) if i.eq_ignore_ascii_case("INTO"))));
    assert!(ins.into.is_some());
    let Some(Stmt::Execute { using, into, .. }) = b.block.stmts.get(7) else {
        panic!("execute");
    };
    assert_eq!(using.len(), 2);
    assert!(into.is_some());
    assert!(matches!(
        b.block.stmts.get(9),
        Some(Stmt::GetDiagnostics { .. })
    ));
}

#[test]
fn insert_column_list_and_update_set_target_are_not_variable_references() {
    let b =
        classified("DECLARE x int; BEGIN INSERT INTO t(x) VALUES (x); UPDATE t SET x = x + 1; END");
    let Some(Stmt::Sql(ins)) = b.block.stmts.first() else {
        panic!("insert");
    };
    assert_eq!(ins.var_refs.len(), 1);
    assert_eq!(ins.var_refs[0].pos, 8);
    let Some(Stmt::Sql(upd)) = b.block.stmts.get(1) else {
        panic!("update");
    };
    assert_eq!(upd.var_refs.len(), 1);
    assert_eq!(upd.var_refs[0].pos, 5);
}

#[test]
fn data_modifying_cte_needs_the_catalog_aware_checker() {
    // データ変更 CTE の実行形はカタログ付きの解析でしか判定できない（構造専用の解析では 0A000。
    // core の実装での分類は `core::plpgsql_validate_tests` が確かめる）。
    assert_eq!(
        classify_code(
            "BEGIN WITH c AS (DELETE FROM t WHERE q = 1 RETURNING q) SELECT q FROM c; END"
        ),
        "0A000"
    );
}

#[test]
fn return_query_and_for_query_classify_write_main_statements() {
    let o = opts_with(ReturnShape::SetOf);
    let b = classified_with("BEGIN RETURN QUERY UPDATE t SET x = 1 RETURNING x; END", &o);
    let Some(Stmt::ReturnQuery(q)) = b.block.stmts.first() else {
        panic!("rq");
    };
    assert!(q.writes);
    assert_eq!(q.kind, EmbeddedKind::Update);
    let b = classified_with(
        "BEGIN FOR r IN UPDATE t SET x = 1 RETURNING x LOOP NULL; END LOOP; END",
        &opts(),
    );
    let Some(Stmt::ForQuery { query, .. }) = b.block.stmts.first() else {
        panic!("for");
    };
    assert!(query.writes);
    let b = classified_with("BEGIN RETURN QUERY SELECT 1; END", &o);
    let Some(Stmt::ReturnQuery(q)) = b.block.stmts.first() else {
        panic!("rq");
    };
    assert!(!q.writes);
    assert_eq!(
        code_with("BEGIN RETURN QUERY CREATE TABLE t(a int); END", &o),
        "42601"
    );
}

#[test]
fn string_and_comment_semicolons_do_not_split_statements() {
    ok("DECLARE n text; BEGIN n := 'a;b'; -- c; d\n /* e; /* nested; */ f; */ n := \"q\"; END");
}

#[test]
fn case_expression_inside_condition_does_not_cut_at_inner_then() {
    ok("BEGIN IF CASE WHEN a = 1 THEN true ELSE false END THEN NULL; END IF; END");
}

// ---- 範囲外（0A000） ----

#[test]
fn rejects_out_of_scope_with_0a000() {
    let srcs = [
        "BEGIN RAISE NOTICE 'x'; END",
        "BEGIN ASSERT true; END",
        "BEGIN OPEN c; END",
        "BEGIN FETCH c INTO a; END",
        "BEGIN MOVE c; END",
        "BEGIN CLOSE c; END",
        "BEGIN FOREACH x IN ARRAY a LOOP NULL; END LOOP; END",
        "BEGIN CREATE TABLE t (a int); END",
        "BEGIN ALTER TABLE t ADD COLUMN b int; END",
        "BEGIN DROP TABLE t; END",
        "BEGIN TRUNCATE t; END",
        "BEGIN COMMIT; END",
        "BEGIN ROLLBACK; END",
        "BEGIN SET x TO 1; END",
        "BEGIN RESET x; END",
        "BEGIN CALL f(); END",
        "BEGIN COPY t FROM stdin; END",
        "BEGIN EXPLAIN SELECT 1; END",
        "BEGIN NULL; EXCEPTION WHEN others THEN NULL; END",
        "DECLARE c CURSOR FOR SELECT 1; BEGIN NULL; END",
        "DECLARE c NO SCROLL CURSOR FOR SELECT 1; BEGIN NULL; END",
        "DECLARE c refcursor; BEGIN NULL; END",
        "DECLARE x anyelement; BEGIN NULL; END",
        "DECLARE x anycompatiblearray; BEGIN NULL; END",
        "DECLARE x ALIAS FOR $1; BEGIN NULL; END",
        "#variable_conflict use_column\nBEGIN NULL; END",
        "BEGIN FOR r IN EXECUTE 'select 1' LOOP NULL; END LOOP; END",
        "BEGIN FOR r IN cur LOOP NULL; END LOOP; END",
        "DECLARE n int; BEGIN GET DIAGNOSTICS n = PG_CONTEXT; END",
        "DECLARE n int; BEGIN GET STACKED DIAGNOSTICS n = MESSAGE_TEXT; END",
        "BEGIN EXECUTE 'x' USING OPERATION_ID 'abc'; END",
        "BEGIN UPDATE t SET x = 1 USING OPERATION_ID 'abc'; END",
        "BEGIN UPDATE t SET x = 1 PARTITIONED; END",
    ];
    for s in srcs {
        assert_eq!(code(s), "0A000", "for {s:?}");
    }
    assert_eq!(
        code_with(
            "BEGIN RETURN QUERY EXECUTE 'select 1'; END",
            &opts_with(ReturnShape::SetOf)
        ),
        "0A000"
    );
}

// ---- 構文誤り（42601） ----

#[test]
fn rejects_syntax_errors_with_42601() {
    let srcs = [
        "",
        "NULL;",
        "BEGIN NULL; END x",
        "<<a>> BEGIN NULL; END b;",
        "BEGIN NULL; END; BEGIN",
        "DECLARE x int; x int; BEGIN NULL; END",
        "BEGIN y := 1; END",
        "DECLARE x CONSTANT int := 1; BEGIN x := 2; END",
        "BEGIN found := true; END",
        "BEGIN EXIT; END",
        "BEGIN CONTINUE; END",
        "BEGIN LOOP EXIT nolabel; END LOOP; END",
        "BEGIN <<b>> BEGIN LOOP CONTINUE b; END LOOP; END; END",
        "<<b>> BEGIN LOOP CONTINUE b; END LOOP; END",
        "DECLARE n int; BEGIN SELECT 1 INTO n INTO n; END",
        "DECLARE n int; BEGIN PERFORM 1 INTO n; END",
        "BEGIN y := 'abc; END",
        "BEGIN EXECUTE $q$select 1$q$; END",
        "DECLARE x int NOT NULL; BEGIN NULL; END",
        "DECLARE begin int; BEGIN NULL; END",
        "BEGIN FOO; END",
        "BEGIN IF a THEN NULL; END",
        "BEGIN IF a THEN NULL; END IF END",
        "BEGIN NULL END",
        "BEGIN LOOP NULL; END",
        "BEGIN FOR i IN 1..3 LOOP NULL; END LOOP wrong; END",
        "BEGIN FOR i, j IN 1..3 LOOP NULL; END LOOP; END",
        "BEGIN CASE END CASE; END",
        "BEGIN CASE WHEN 1, 2 THEN NULL; END CASE; END",
        "BEGIN EXECUTE 'x' USING a USING a; END",
        "BEGIN <<l>> NULL; END",
        "DECLARE x notatype ( ; BEGIN NULL; END",
        "BEGIN RETURN NEXT 1; END",
        "BEGIN RETURN QUERY SELECT 1; END",
        "BEGIN SELECT (1; END",
    ];
    for s in srcs {
        assert_eq!(code(s), "42601", "for {s:?}");
    }
}

#[test]
fn rejects_return_value_shapes_with_42804() {
    assert_eq!(
        code_with("BEGIN RETURN 1; END", &opts_with(ReturnShape::Void)),
        "42804"
    );
    assert_eq!(
        code_with("BEGIN RETURN 1; END", &opts_with(ReturnShape::SetOf)),
        "42804"
    );
}

#[test]
fn rejects_undefined_parameter_with_42p02() {
    assert_eq!(code("BEGIN PERFORM $3; END"), "42P02");
    assert_eq!(code("DECLARE x int := $9; BEGIN NULL; END"), "42P02");
    ok("BEGIN PERFORM $2; END");
}

// ---- 上限 ----

#[test]
fn body_size_limit_is_checked_before_scanning() {
    let src = "BEGIN NULL; END";
    let o = opts().with_limits(32, src.len());
    ok_with(src, &o);
    let o = opts().with_limits(32, src.len() - 1);
    assert_eq!(code_with(src, &o), "54000");
    // 巨大な入力は（字句エラーになりうる内容でも）長さで先に拒否する。
    let huge = "'".repeat(crate::plpgsql_limits::HARD_MAX_PLPGSQL_BODY_BYTES + 1);
    assert_eq!(code(&huge), "54000");
}

#[test]
fn limits_saturate_at_hard_caps() {
    let o = opts().with_limits(usize::MAX, usize::MAX);
    assert_eq!(
        o.max_nesting_depth,
        crate::plpgsql_limits::HARD_MAX_PLPGSQL_NESTING_DEPTH
    );
    assert_eq!(
        o.max_body_bytes,
        crate::plpgsql_limits::HARD_MAX_PLPGSQL_BODY_BYTES
    );
}

fn nested_blocks(n: usize) -> String {
    format!("{}NULL; {}", "BEGIN ".repeat(n), "END; ".repeat(n))
}

fn nested_ifs(n: usize) -> String {
    format!(
        "BEGIN {}NULL; {}END",
        "IF true THEN ".repeat(n),
        "END IF; ".repeat(n)
    )
}

#[test]
fn nesting_depth_limit() {
    ok(&nested_blocks(32));
    assert_eq!(code(&nested_blocks(33)), "54001");
    // ブロック 1 + IF 31 = 32 段は通り、32 個目の IF で 33 段になり拒否される。
    ok(&nested_ifs(31));
    assert_eq!(code(&nested_ifs(32)), "54001");
    // ループ・CASE の混在も同じカウンタで数える。
    let mixed = format!(
        "BEGIN {}NULL; {}END",
        "LOOP CASE WHEN true THEN ".repeat(16),
        "END CASE; END LOOP; ".repeat(16)
    );
    assert_eq!(code(&mixed), "54001");
    // 下げた上限が効く。
    let o = opts().with_limits(2, 1 << 20);
    ok_with(&nested_blocks(2), &o);
    assert_eq!(code_with(&nested_blocks(3), &o), "54001");
}

#[test]
fn variables_per_statement_limit() {
    let build = |n: usize| {
        let decls: String = (0..n).map(|i| format!("v{i} int; ")).collect();
        let sum: Vec<String> = (0..n).map(|i| format!("v{i}")).collect();
        format!("DECLARE {decls}BEGIN PERFORM {}; END", sum.join(" + "))
    };
    classified(&build(64));
    assert_eq!(classify_code(&build(65)), "54000");
}

#[test]
fn scanner_token_limit() {
    let at = "(".repeat(crate::plpgsql_limits::HARD_MAX_PLPGSQL_TOKENS);
    assert!(scan(&at).is_ok());
    let over = "(".repeat(crate::plpgsql_limits::HARD_MAX_PLPGSQL_TOKENS + 1);
    assert_eq!(scan(&over).expect_err("over").wire_code(), "54000");
}

// ---- スキャナ ----

#[test]
fn scanner_recognizes_plpgsql_punctuation() {
    let toks =
        scan("x := 1..10 <<l>> %TYPE $1 'a;b' \"Q\"\"x\" -- c;\n /* /* n */ ; */ $t$ ; $t$ ;")
            .expect("scan");
    let kinds: Vec<TokKind> = toks.into_iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        vec![
            TokKind::Word("x".to_string()),
            TokKind::Assign,
            TokKind::Number,
            TokKind::DotDot,
            TokKind::Number,
            TokKind::LabelOpen,
            TokKind::Word("l".to_string()),
            TokKind::LabelClose,
            TokKind::Punct('%'),
            TokKind::Word("TYPE".to_string()),
            TokKind::Param(1),
            TokKind::Str,
            TokKind::Quoted("Q\"x".to_string()),
            TokKind::Dollar,
            TokKind::Punct(';'),
        ]
    );
}

#[test]
fn scanner_rejects_unterminated_input() {
    for s in ["'abc", "\"abc", "/* abc", "$t$ abc"] {
        assert_eq!(scan(s).expect_err(s).wire_code(), "42601", "for {s:?}");
    }
}

// ---- 非漏えい ----

#[test]
fn error_messages_do_not_echo_body_fragments() {
    for s in [
        "BEGIN secret_var_xyz := 1; END",
        "BEGIN secret_stmt_xyz; END",
        "DECLARE secret_ty_xyz secret_ty_xyz; BEGIN NULL; END",
        "BEGIN PERFORM secret_col_xyz FROM secret_tbl_xyz WHERE ; END",
        "BEGIN EXIT secret_lbl_xyz; END",
    ] {
        if let Err(e) = parse_body(s, &opts()) {
            let msg = e.to_string();
            assert!(!msg.contains("xyz"), "leaked fragment in {msg:?}");
        }
    }
}

// ---- 定義時分類（モック checker で受け渡しを、実体の構造解析で分類を確かめる） ----

type Call = (usize, bool, Vec<Token>);

struct Recorder(std::cell::RefCell<Vec<Call>>);

impl EmbeddedSqlChecker for Recorder {
    fn check_unit(
        &self,
        tokens: &[Token],
        depth: usize,
        is_root: bool,
    ) -> Result<UnitSummary, SqlSurfaceError> {
        self.0.borrow_mut().push((depth, is_root, tokens.to_vec()));
        Ok(UnitSummary {
            kind: crate::sql::embedded_unit::UnitKind::Select,
            has_write_cte: false,
        })
    }
}

struct Failing;

impl EmbeddedSqlChecker for Failing {
    fn check_unit(
        &self,
        _tokens: &[Token],
        _depth: usize,
        _is_root: bool,
    ) -> Result<UnitSummary, SqlSurfaceError> {
        Err(SqlSurfaceError::unsupported("mock failure"))
    }
}

#[test]
fn validation_hands_every_expression_and_statement_to_the_checker() {
    let mut b = ok("DECLARE n int := 1; flag boolean; BEGIN \
         n := n + a; PERFORM flag; END");
    let rec = Recorder(Default::default());
    validate_embedded(&mut b, &rec).expect("validate");
    let calls = rec.0.borrow();
    // 既定値（n := 1）・代入式・PERFORM の 3 件。変数は置換せず、ルート（深さ 0）として渡す。
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[1],
        (
            0,
            true,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Ident("n".to_string()),
                Token::Punct('+'),
                Token::Ident("a".to_string()),
            ]
        )
    );
    assert_eq!(
        calls[2].2,
        vec![
            Token::Keyword(Keyword::Select),
            Token::Ident("flag".to_string()),
        ]
    );
}

#[test]
fn validation_propagates_checker_errors() {
    let mut b = ok("BEGIN PERFORM 1; END");
    let e = validate_embedded(&mut b, &Failing).expect_err("must fail");
    assert_eq!(e.wire_code(), "42601");
}

#[test]
fn dummy_token_by_type() {
    assert_eq!(
        dummy_token(&DeclType::Named(SqlColumnTypeName::BigInt)),
        Token::Number("0".to_string())
    );
    assert_eq!(
        dummy_token(&DeclType::Named(SqlColumnTypeName::Boolean)),
        Token::Ident("false".to_string())
    );
    assert_eq!(
        dummy_token(&DeclType::Record),
        Token::StringLiteral("0".to_string())
    );
}

#[test]
fn return_requires_value_for_scalar_trigger_and_return_next() {
    for shape in [ReturnShape::Scalar, ReturnShape::Trigger] {
        assert_eq!(code_with("BEGIN RETURN; END", &opts_with(shape)), "42601");
    }
    assert_eq!(
        code_with("BEGIN RETURN NEXT; END", &opts_with(ReturnShape::SetOf)),
        "42601"
    );
}

#[test]
fn labeled_for_loop_variable_resolves_via_label() {
    ok("<<l>> BEGIN <<m>> FOR i IN 1..3 LOOP PERFORM m.i; END LOOP; END");
    ok("BEGIN <<l>> FOR i IN 1..3 LOOP PERFORM l.i; END LOOP; END");
    ok("BEGIN <<l>> FOR r IN SELECT 1 LOOP PERFORM l.r; END LOOP; END");
}

#[test]
fn parenthesized_query_for_loop_is_accepted() {
    ok("BEGIN FOR r IN (SELECT 1) LOOP NULL; END LOOP; END");
}

#[test]
fn type_name_and_cte_names_are_not_variable_references() {
    let b = classified("DECLARE text text; BEGIN PERFORM 1::text; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert!(p.var_refs.is_empty());
    let b = classified(
        "DECLARE c int; BEGIN WITH c AS (SELECT q FROM u) SELECT q FROM c INTO STRICT c; END",
    );
    let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
        panic!("sql");
    };
    assert!(s.var_refs.is_empty());
    assert!(s.into.is_some());
}

#[test]
fn with_statement_kind_follows_the_syntax_tree() {
    let b = classified("BEGIN WITH c AS (SELECT q FROM u) SELECT q FROM c; END");
    let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
        panic!("sql");
    };
    assert_eq!((s.kind, s.writes), (EmbeddedKind::Select, false));
    // 許可リストが受理しない主文（`WITH ... UPDATE`・`WITH ... INSERT`）は定義時に 42601。
    for src in [
        "BEGIN WITH c AS (SELECT q FROM u) UPDATE t SET x = 1; END",
        "BEGIN WITH c AS (SELECT q FROM u) INSERT INTO t SELECT q FROM c; END",
    ] {
        assert_eq!(classify_code(src), "42601", "for {src:?}");
    }
}

#[test]
fn parenthesized_for_query_passes_validation_shape() {
    let b = ok("BEGIN FOR r IN (SELECT 1) LOOP NULL; END LOOP; END");
    let Some(Stmt::ForQuery { query, .. }) = b.block.stmts.first() else {
        panic!("for");
    };
    assert!(matches!(
        query.tokens.first(),
        Some(Token::Keyword(Keyword::Select))
    ));
}
#[test]
fn expression_from_does_not_hide_variables() {
    let b = classified("DECLARE v int; BEGIN PERFORM substring('abc' FROM v); END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert_eq!(p.var_refs.len(), 1);
    let b = classified("DECLARE v int; BEGIN PERFORM 1 FROM t WHERE x IS NOT DISTINCT FROM v; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert_eq!(p.var_refs.len(), 1);
}

#[test]
fn from_list_tables_and_aliases_are_not_variables() {
    // 表名・別名の位置は解析器が列参照と確定しないので、同名の変数があっても変数にならない。
    for src in [
        "PERFORM 1 FROM u",
        "PERFORM 1 FROM t AS u",
        "PERFORM 1 FROM t u WHERE u.q = 1",
        "PERFORM t.q FROM t JOIN u ON t.k = u.k",
    ] {
        let b = classified(&format!("DECLARE u int; BEGIN {src}; END"));
        assert_eq!(first_sql_refs(&b), 0, "for {src:?}");
    }
    // 値の位置（WHERE）は変数でありうる。
    let b = classified(
        "DECLARE v int; BEGIN PERFORM t.q FROM t JOIN s ON t.k = s.k WHERE t.q = v; END",
    );
    assert_eq!(first_sql_refs(&b), 1);
    let b = classified("DECLARE v int; BEGIN PERFORM 1 FROM t WHERE v = 1; END");
    assert_eq!(first_sql_refs(&b), 1);
}

#[test]
fn for_query_accepts_returning_dml_and_rejects_collate() {
    ok("BEGIN FOR r IN UPDATE t SET x = 1 RETURNING x LOOP NULL; END LOOP; END");
    ok("BEGIN FOR r IN DELETE FROM t RETURNING x LOOP NULL; END LOOP; END");
    assert_eq!(
        code("DECLARE s text COLLATE \"C\"; BEGIN NULL; END"),
        "0A000"
    );
}

#[test]
fn delegated_type_errors_do_not_echo_fragments() {
    let e =
        parse_body("DECLARE x float(secret_xyz); BEGIN NULL; END", &opts()).expect_err("bad type");
    assert_eq!(e.wire_code(), "42601");
    assert!(!e.to_string().contains("secret"), "leaked: {e}");
}

#[test]
fn sql_keyword_named_variable_is_rejected_at_declaration() {
    // `limit` 等は字句解析で `Token::Keyword` になり変数参照として記録できないため、宣言時に拒否する。
    assert_eq!(
        code("DECLARE limit int := 1; BEGIN RETURN limit; END"),
        "42601"
    );
}

#[test]
fn classification_is_linear_for_many_references() {
    // 多数の変数参照を持つ式でも解析回数は参照数に比例しない（記録は 1 回の解析で得る）。
    let expr = vec!["a"; 200].join(" + ");
    let start = std::time::Instant::now();
    let b = classified(&format!("BEGIN RETURN {expr}; END"));
    let Some(Stmt::Return(Some(e))) = b.block.stmts.first() else {
        panic!("return");
    };
    assert_eq!(e.var_refs.len(), 200);
    assert!(start.elapsed().as_secs() < 5);
}

// ---- PR #2779 レビュー指摘の回帰（曖昧な名前の宣言拒否・RETURN QUERY・ラベル解決・CTE 分類） ----

fn first_sql_refs(b: &Body) -> usize {
    match b.block.stmts.first() {
        Some(Stmt::Sql(q)) | Some(Stmt::Perform(q)) | Some(Stmt::ReturnQuery(q)) => {
            q.var_refs.len()
        }
        other => panic!("not a SQL statement: {other:?}"),
    }
}

#[test]
fn reserved_names_are_rejected_at_declaration() {
    // PL/pgSQL の予約語は 42601、SQL の予約語（`true`・`false`・`null` を含む）は 0A000。引用付きでも同じ。
    for name in [
        "select", "from", "where", "and", "order", "by", "limit", "begin", "loop",
    ] {
        assert_eq!(
            code(&format!("DECLARE {name} int; BEGIN NULL; END")),
            "42601",
            "for {name}"
        );
    }
    for name in [
        "true",
        "false",
        "default",
        "array",
        "current_date",
        "current_timestamp",
        "user",
        "table",
        "all",
        "group",
        "returning",
        "offset",
        "using",
        "into",
        "with",
        "window",
        "variadic",
    ] {
        for decl in [name.to_string(), format!("\"{name}\"")] {
            assert_eq!(
                code(&format!("DECLARE {decl} int; BEGIN NULL; END")),
                "0A000",
                "for {decl}"
            );
        }
    }
    // ループ変数・パラメータ名も同じ。ラベルは修飾子としてだけ使うので対象外。
    assert_eq!(
        code("BEGIN FOR \"table\" IN 1..3 LOOP NULL; END LOOP; END"),
        "0A000"
    );
    ok("<<values>> BEGIN NULL; END");
    let o = ParseOptions::new(
        vec![ParamDecl {
            name: Some("User".to_string()),
            ty: SqlColumnTypeName::Integer,
        }],
        ReturnShape::Scalar,
    );
    assert_eq!(code_with("BEGIN RETURN 1; END", &o), "0A000");
    // エラー文言は名前を含めない。
    let e = parse_body("DECLARE \"user\" int; BEGIN NULL; END", &opts()).expect_err("reject");
    assert!(
        !e.to_string().to_lowercase().contains("user"),
        "leaked: {e}"
    );
}

#[test]
fn keyword_like_names_are_accepted_and_do_not_capture_syntax() {
    // SQL 側の文脈キーワード・文の語と同名の変数も宣言できる。構文上の語は解析器が列参照と確定しない
    // ので、変数参照にならない（PostgreSQL の予約語だけが宣言できない）。
    let names = [
        "set",
        "values",
        "insert",
        "update",
        "delete",
        "is",
        "like",
        "ilike",
        "between",
        "filter",
        "over",
        "similar",
        "escape",
        "partition",
        "range",
        "rows",
        "zone",
        "time",
        "timestamp",
        "interval",
        "first",
        "next",
        "strict",
        "reverse",
        "exception",
        "operation_id",
        "out",
        "inout",
        "local",
        "exists",
        "at",
        "nulls",
        "conflict",
        "nothing",
        "plan",
        "mode",
        "hybrid",
        "partitioned",
        "recursive",
        "materialized",
        "ties",
        "preceding",
        "following",
        "unbounded",
        "exclude",
        "cube",
        "rollup",
        "grouping",
        "sets",
        "search",
        "cycle",
        "left",
        "right",
        "inner",
        "outer",
        "join",
        "natural",
        "cross",
        "full",
        "key",
    ];
    let decls: String = names.iter().map(|n| format!("{n} int; ")).collect();
    let refs = |stmt: &str| {
        let b = classified(&format!("DECLARE {decls} BEGIN {stmt}; END"));
        first_sql_refs(&b)
    };
    assert_eq!(refs("UPDATE t SET x = 1 WHERE y IS NULL"), 0);
    assert_eq!(refs("UPDATE t SET x = 1, y = 2 WHERE z = 'a'"), 0);
    assert_eq!(refs("PERFORM 1 FROM t WHERE y BETWEEN 1 AND 2"), 0);
    assert_eq!(
        refs("PERFORM 1 FROM t WHERE y LIKE 'a%' AND z ILIKE 'b'"),
        0
    );
    assert_eq!(refs("PERFORM t.q FROM t LEFT JOIN u ON t.k = u.k"), 0);
    assert_eq!(
        refs("PERFORM sum(y) OVER (PARTITION BY z ORDER BY w) FROM t"),
        0
    );
    assert_eq!(
        refs("PERFORM 1 FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = 1)"),
        0
    );
    assert_eq!(
        refs("INSERT INTO t(x) VALUES (1) ON CONFLICT (x) DO UPDATE SET x = 2"),
        0
    );
    assert_eq!(
        refs("INSERT INTO t(x) VALUES (1) ON CONFLICT (x) DO NOTHING"),
        0
    );
    assert_eq!(refs("PERFORM 1::timestamp"), 0);
    assert_eq!(refs("PERFORM q FROM t ORDER BY y NULLS FIRST"), 0);
    assert_eq!(refs("PERFORM q FROM t UNION SELECT q FROM u"), 0);
    // 同名の変数は値の位置では参照になる。
    assert_eq!(refs("PERFORM 1 FROM t WHERE y = set"), 1);
    assert_eq!(refs("UPDATE t SET x = values + insert"), 2);
}

#[test]
fn identifier_roles_are_not_variable_references_even_with_same_named_variables() {
    // 列リスト・別名・型名・CTE 名・SET の代入先・関数名・表名は、同名の変数があっても変数にならない。
    let refs = |stmt: &str| {
        let b = classified(&format!(
            "DECLARE x int; text int; n int; BEGIN {stmt}; END"
        ));
        first_sql_refs(&b)
    };
    assert_eq!(refs("INSERT INTO t(x) VALUES (1)"), 0);
    assert_eq!(refs("INSERT INTO t(n, x) VALUES (1, 2)"), 0);
    assert_eq!(refs("UPDATE t SET x = 1"), 0);
    assert_eq!(refs("UPDATE t SET (n, x) = (1, 2)"), 0);
    assert_eq!(refs("UPDATE t SET n = 1, x = 2 WHERE n = 3"), 1);
    assert_eq!(refs("PERFORM 1 AS x"), 0);
    assert_eq!(refs("PERFORM 1 FROM t AS x"), 0);
    assert_eq!(refs("PERFORM 1 FROM t x"), 0);
    assert_eq!(refs("PERFORM 1 FROM x"), 0);
    assert_eq!(refs("PERFORM 1::text"), 0);
    assert_eq!(refs("PERFORM x(1) FROM t"), 0);
    assert_eq!(refs("WITH x AS (SELECT q FROM u) SELECT q FROM x"), 0);
    // `INSERT INTO ONLY t (x)`・`INSERT INTO t AS a (x)` は SQL 構文解析器が受理しない形（42601）で、
    // 列リストの `x` が変数として読まれることはない。
    for stmt in [
        "INSERT INTO ONLY t(x) VALUES (1)",
        "INSERT INTO t AS r(x) VALUES (1)",
    ] {
        assert_eq!(
            classify_code(&format!("DECLARE x int; BEGIN {stmt}; END")),
            "42601",
            "for {stmt:?}"
        );
    }
}

#[test]
fn expression_left_hand_sides_resolve_variables() {
    // 左辺が式（括弧付き・演算）の `IS [NOT] NULL`・`BETWEEN`・`IN` の変数も参照になる。
    let refs = |cond: &str| {
        let b = classified(&format!(
            "DECLARE v int; w int; BEGIN IF {cond} THEN NULL; END IF; END"
        ));
        let Some(Stmt::If { branches, .. }) = b.block.stmts.first() else {
            panic!("if");
        };
        branches[0].0.var_refs.len()
    };
    assert_eq!(refs("v IS NULL"), 1);
    assert_eq!(refs("(v) IS NOT NULL"), 1);
    assert_eq!(refs("(v + 1) IS NULL"), 1);
    assert_eq!(refs("v + w IS NOT NULL"), 2);
    assert_eq!(refs("v BETWEEN 1 AND 5"), 1);
    assert_eq!(refs("(v) NOT BETWEEN w AND 5"), 2);
    assert_eq!(refs("v IN (1, 2, 3)"), 1);
    assert_eq!(refs("(v + 1) NOT IN (1, w)"), 2);
    assert_eq!(refs("1 IS NULL"), 0);
    assert_eq!(refs("v > 0 AND w IS NULL"), 2);
}

#[test]
fn column_only_positions_reject_variables_with_0a000() {
    // 値（リテラル）を置いても構文として成立しない位置（列名がそのまま構造になる位置）。
    for stmt in [
        "PERFORM q FROM t WHERE v IN (SELECT q FROM u)",
        "PERFORM DISTINCT v FROM t",
        "PERFORM sum(1) OVER (PARTITION BY v) FROM t",
        "PERFORM sum(1) OVER (ORDER BY v) FROM t",
        "PERFORM q FROM t UNION SELECT v FROM u",
        "DELETE FROM t WHERE q = 1 RETURNING v",
        "UPDATE t SET x = 1 RETURNING v",
        "INSERT INTO t(x) VALUES (1) RETURNING v",
        "PERFORM count(DISTINCT v) FROM t",
    ] {
        assert_eq!(
            classify_code(&format!("DECLARE v int; BEGIN {stmt}; END")),
            "0A000",
            "for {stmt:?}"
        );
    }
    // 同じ形で変数でない列は受理される（列専用位置そのものは拒否しない）。
    classified("BEGIN PERFORM DISTINCT q FROM t; END");
}

#[test]
fn variables_in_unsupported_literal_positions_are_rejected_with_0a000() {
    // 解析器がリテラルしか受理しない位置のうち、変数を受理しない形（ESCAPE・型付きリテラル・SIMILAR TO）。
    for stmt in [
        "PERFORM q FROM t WHERE lower(q) LIKE 'x%' ESCAPE v",
        "PERFORM q FROM t WHERE ts > TIMESTAMP v",
        "PERFORM q FROM t WHERE q SIMILAR TO v",
        "PERFORM 1 FROM t WHERE y = 1 AND q SIMILAR TO v",
    ] {
        assert_eq!(
            classify_code(&format!("DECLARE v text; BEGIN {stmt}; END")),
            "0A000",
            "for {stmt:?}"
        );
    }
    // 変数を含まない同じ形は受理される。
    classified("BEGIN PERFORM q FROM t WHERE q SIMILAR TO 'x%'; END");
}

#[test]
fn unclassifiable_parameter_positions_are_rejected_with_0a000() {
    // `$n` が列参照として記録されない位置（表名など）は置換できないので 0A000（fail-closed）。
    assert_eq!(classify_code("BEGIN PERFORM q FROM $1; END"), "0A000");
    // 値の位置の `$n` は参照になり、リテラル専用位置（LIMIT）では LiteralSlot になる。
    let b = classified("BEGIN PERFORM q FROM t WHERE q = $1; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = classified("BEGIN PERFORM q FROM t LIMIT $1; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert_eq!(p.var_refs[0].role, VarRole::LiteralSlot);
    // 範囲外の `$n` は解析時点で 42P02。
    assert_eq!(code("BEGIN PERFORM q FROM t WHERE q = $9; END"), "42P02");
}

#[test]
fn subquery_and_cte_bodies_are_classified_recursively() {
    let refs = |stmt: &str| {
        let b = classified(&format!("DECLARE v int; BEGIN {stmt}; END"));
        first_sql_refs(&b)
    };
    assert_eq!(
        refs("PERFORM q FROM t WHERE k IN (SELECT k FROM u WHERE z = v)"),
        1
    );
    assert_eq!(
        refs("PERFORM q FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.z = v)"),
        1
    );
    assert_eq!(
        refs("PERFORM q FROM t WHERE k IN (SELECT k FROM u WHERE z IN (SELECT z FROM w WHERE r = v))"),
        1
    );
    // CTE 本体の変数: リテラル専用位置（`LIMIT`・`LIKE` のパターン）は LiteralSlot として分類される。
    let b =
        classified("DECLARE v int; BEGIN WITH c AS (SELECT q FROM u LIMIT v) SELECT q FROM c; END");
    assert_eq!(first_sql_refs(&b), 1);
    let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
        panic!("sql");
    };
    assert_eq!(s.var_refs[0].role, VarRole::LiteralSlot);
    let b = classified(
        "DECLARE v text; BEGIN WITH c AS (SELECT q FROM u WHERE z LIKE v) SELECT q FROM c; END",
    );
    assert_eq!(first_sql_refs(&b), 1);
    // CTE 本体の WHERE は許可リストが `列 = '文字列'`・`LIKE` 等の形しか受理しない（ビュー本体と同じ制限）。
    // `列 = 変数` は解析器が式述語として読み、CTE 本体では 42601 になる。リテラルを置けば通る形なので、
    // 定義時は変数の置き場所が非対応として 0A000（fail-closed。既知の制限）。
    assert_eq!(
        classify_code(
            "DECLARE v text; BEGIN WITH c AS (SELECT q FROM u WHERE z = v) SELECT q FROM c; END"
        ),
        "0A000"
    );
    // 外側の表を参照する相関サブクエリ（範囲の再解析で外側の別名が解決できない形）は fail-closed。
    assert_eq!(
        classify_code(
            "DECLARE v int; BEGIN PERFORM q FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.k AND u.z = v); END"
        ),
        "0A000"
    );
}

#[test]
fn row_count_clause_and_named_window_are_normalized_before_classification() {
    let b = classified(
        "DECLARE n int; BEGIN PERFORM 1 FROM t LIMIT 1 OFFSET 0; \
         PERFORM 1 FROM t FETCH FIRST n ROWS ONLY; \
         PERFORM sum(x) OVER w FROM t WINDOW w AS (PARTITION BY y); END",
    );
    let Some(Stmt::Perform(p)) = b.block.stmts.get(1) else {
        panic!("perform");
    };
    assert_eq!(p.var_refs.len(), 1);
    assert_eq!(p.var_refs[0].role, VarRole::LiteralSlot);
    // 保存されるトークン列は正規化後（`FETCH FIRST n ROWS ONLY` は `LIMIT n`）で、位置は保存側の列に対応する。
    assert!(matches!(
        p.tokens.get(p.var_refs[0].pos),
        Some(Token::Ident(w)) if w == "n"
    ));
    assert!(!p
        .tokens
        .iter()
        .any(|t| matches!(t, Token::Ident(w) if w.eq_ignore_ascii_case("FETCH"))));
    let Some(Stmt::Perform(w)) = b.block.stmts.get(2) else {
        panic!("perform");
    };
    assert!(!w
        .tokens
        .iter()
        .any(|t| matches!(t, Token::Ident(x) if x.eq_ignore_ascii_case("WINDOW"))));
}

#[test]
fn literal_words_are_not_variable_references() {
    let b = classified("BEGIN IF true THEN NULL; END IF; END");
    let Some(Stmt::If { branches, .. }) = b.block.stmts.first() else {
        panic!("if");
    };
    assert!(branches[0].0.var_refs.is_empty());
    let b = classified("BEGIN INSERT INTO t(x) VALUES (default); END");
    assert_eq!(first_sql_refs(&b), 0);
    let b = classified("BEGIN PERFORM ARRAY[1, 2]; END");
    assert_eq!(first_sql_refs(&b), 0);
    let b = classified("DECLARE v int; BEGIN UPDATE t SET x = v; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = classified("DECLARE n int; BEGIN PERFORM 1 FROM t FETCH FIRST n ROWS ONLY; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = classified("DECLARE n int; BEGIN PERFORM 1 FROM t GROUP BY x HAVING count(*) > n; END");
    assert_eq!(first_sql_refs(&b), 1);
}

#[test]
fn execute_requires_a_command_expression() {
    // 句としての先頭 INTO／USING は空のコマンド文字列（42601）。
    assert_eq!(code("DECLARE x int; BEGIN EXECUTE INTO x; END"), "42601");
    assert_eq!(code("DECLARE a int; BEGIN EXECUTE USING a; END"), "42601");
    assert_eq!(code("BEGIN EXECUTE ; END"), "42601");
    ok("DECLARE x int; BEGIN EXECUTE 'select 1' INTO x; END");
}

#[test]
fn statement_words_stay_sql_even_when_a_variable_has_the_same_name() {
    // `insert` は宣言できるが、`insert := 1` は先頭の語から SQL 文として読まれ、定義時分類で 42601 になる
    // （代入として黙って読み替えない）。
    let b = ok("DECLARE insert int; BEGIN insert := 1; END");
    assert!(matches!(b.block.stmts.first(), Some(Stmt::Sql(_))));
    assert_eq!(
        classify_code("DECLARE insert int; BEGIN insert := 1; END"),
        "42601"
    );
    ok("BEGIN INSERT INTO t VALUES (1); END");
}

#[test]
fn return_query_accepts_parenthesized_query() {
    let o = opts_with(ReturnShape::SetOf);
    let b = ok_with("BEGIN RETURN QUERY (SELECT 1); END", &o);
    let Some(Stmt::ReturnQuery(q)) = b.block.stmts.first() else {
        panic!("return query");
    };
    assert!(matches!(
        q.tokens.first(),
        Some(Token::Keyword(Keyword::Select))
    ));
    ok_with("BEGIN RETURN QUERY ((SELECT 1)); END", &o);
}

#[test]
fn outermost_block_label_qualifies_params_and_found() {
    let b = classified("<<blk>> BEGIN RETURN blk.a; END");
    let Some(Stmt::Return(Some(e))) = b.block.stmts.first() else {
        panic!("return");
    };
    assert_eq!(e.var_refs.len(), 1);
    let b = classified("<<blk>> BEGIN PERFORM blk.found; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = ok("<<blk>> BEGIN blk.a := 2; RETURN 1; END");
    assert!(matches!(b.block.stmts.first(), Some(Stmt::Assign { .. })));
    assert_eq!(
        code("<<blk>> BEGIN blk.found := true; RETURN 1; END"),
        "42601"
    );
    // 内側ブロックのラベルではパラメータを修飾参照しない（最外ブロックのみ）。
    let b = classified("BEGIN <<inner>> BEGIN PERFORM inner.a; END; END");
    let Some(Stmt::Block(inner)) = b.block.stmts.first() else {
        panic!("block");
    };
    let Some(Stmt::Perform(p)) = inner.stmts.first() else {
        panic!("perform");
    };
    assert!(p.var_refs.is_empty());
}

#[test]
fn with_classification_reads_cte_definitions_from_the_syntax_tree() {
    let kind = |src: &str| {
        let b = classified(&format!("BEGIN {src}; END"));
        match b.block.stmts.first() {
            Some(Stmt::Sql(q)) => (q.kind, q.writes),
            other => panic!("not sql: {other:?}"),
        }
    };
    // CTE 名が主文の語と同名でも、解析木から文種を取る。
    assert_eq!(
        kind("WITH \"update\" AS (SELECT q FROM u) SELECT q FROM \"update\""),
        (EmbeddedKind::Select, false)
    );
    assert_eq!(
        kind("WITH c AS (SELECT q FROM u) SELECT q FROM c"),
        (EmbeddedKind::Select, false)
    );
    // 形を読み解けない WITH は定義時に 42601（fail-closed）。
    assert_eq!(classify_code("BEGIN WITH c AS SELECT 1; END"), "42601");
    assert_eq!(
        classify_code_with(
            "BEGIN RETURN QUERY WITH c AS (SELECT 1) TABLE t; END",
            &opts_with(ReturnShape::SetOf)
        ),
        "42601"
    );
    assert_eq!(
        code_with(
            "BEGIN RETURN QUERY CREATE TABLE t(a int); END",
            &opts_with(ReturnShape::SetOf)
        ),
        "42601"
    );
}

#[test]
fn declaration_default_resolves_in_the_scope_before_the_declaration() {
    // PostgreSQL 17 の `decl_statement`（pl_gram.y）は既定値を読んだ後に変数を名前空間へ追加するため、
    // 内側 `DECLARE x int := x` の既定値の `x` は外側の x に解決される。
    let b = classified("DECLARE x int := 10; BEGIN DECLARE x int := x; BEGIN RETURN x; END; END");
    let outer = b.block.decls[0];
    let Some(Stmt::Block(inner)) = b.block.stmts.first() else {
        panic!("block");
    };
    let inner_id = inner.decls[0];
    assert_ne!(outer, inner_id);
    let default = b.vars[inner_id.0 as usize]
        .default
        .as_ref()
        .expect("default");
    assert_eq!(default.var_refs.len(), 1);
    assert_eq!(default.var_refs[0].var, VarRef::Var(outer));
    // 内側の宣言後の参照は内側の x（外側を隠す）。
    let Some(Stmt::Return(Some(e))) = inner.stmts.first() else {
        panic!("return");
    };
    assert_eq!(e.var_refs[0].var, VarRef::Var(inner_id));
}

#[test]
fn type_specifications_are_not_variable_references() {
    // 型名の語（`precision`・`varying`・`zone` 等）は予約語でないので宣言でき、型指定の位置では変数にならない。
    for w in [
        "precision",
        "varying",
        "zone",
        "time",
        "timestamp",
        "numeric",
    ] {
        ok(&format!("DECLARE {w} int; BEGIN NULL; END"));
    }
    // 型指定の範囲（複数語・修飾子・`[]`）は `::` と `CAST` の両方で列参照にならず、その後に続く
    // 変数参照だけが記録される。
    let types = [
        "double precision",
        "character varying(10)",
        "timestamp without time zone",
        "timestamp with time zone",
        "int[]",
        "numeric(10,2)",
    ];
    for ty in types {
        for expr in [format!("1::{ty}"), format!("CAST(1 AS {ty})")] {
            let src = format!("DECLARE v int; BEGIN PERFORM {expr}; END");
            assert_eq!(first_sql_refs(&classified(&src)), 0, "for {src:?}");
            let src = format!("DECLARE v int; BEGIN PERFORM {expr} + v; END");
            assert_eq!(first_sql_refs(&classified(&src)), 1, "for {src:?}");
        }
    }
    // `CAST` の式部分の変数は参照になる。
    let b = classified("DECLARE v int; BEGIN PERFORM CAST(v AS int); END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = classified("DECLARE v int; BEGIN RETURN 1::double precision; END");
    let Some(Stmt::Return(Some(e))) = b.block.stmts.first() else {
        panic!("return");
    };
    assert!(e.var_refs.is_empty());
}

// ---- 引用付き識別子・派生表の列別名・括弧剥がしの線形性（PR #2779 5 往復目） ----

#[test]
fn quoted_identifiers_named_like_clause_words_are_not_clauses() {
    // `"into"` は別名であり INTO 句ではない。実際の INTO 句は 1 個だけ抽出される。
    let b = ok("DECLARE n int; BEGIN SELECT 1 AS \"into\" INTO n; END");
    let Some(Stmt::Sql(q)) = b.block.stmts.first() else {
        panic!("sql");
    };
    assert_eq!(q.into.as_ref().map(|i| i.targets.len()), Some(1));
    // 引用付きの "into" 自体を INTO 対象の名前にもできる（名前は元のトークンから取る）。
    ok("DECLARE n int; BEGIN SELECT 1 AS \"x\" INTO n; END");
    // 禁止句の語も引用付きなら識別子。
    ok("BEGIN PERFORM 1 AS \"partitioned\"; END");
    ok("BEGIN PERFORM 1 AS \"operation_id\"; END");
    ok("DECLARE v int; BEGIN EXECUTE 'x' USING \"operation_id\"; END");
    // 句としての語は従来どおり拒否される。
    assert_eq!(code("BEGIN PERFORM 1 PARTITIONED BY x; END"), "0A000");
    assert_eq!(
        code("BEGIN EXECUTE 'x' USING OPERATION_ID 'k'; END"),
        "0A000"
    );
    assert_eq!(
        code("DECLARE n int; BEGIN SELECT 1 INTO n INTO n; END"),
        "42601"
    );
    // INSERT INTO の INTO は句ではなく、引用付きの "insert" は文の語ではない。
    ok("BEGIN INSERT INTO t(x) VALUES (1); END");
}

#[test]
fn strip_outer_parens_is_linear_and_correct() {
    use super::parser::strip_outer_parens;
    let q = |n: usize| {
        let mut t = vec![Token::Punct('('); n];
        t.push(Token::Number("1".to_string()));
        t.extend(vec![Token::Punct(')'); n]);
        let b = vec![false; t.len()];
        (t, b)
    };
    // 10 万組でも一度の走査で全て剥がれる。
    let (t, b) = strip_outer_parens(q(100_000));
    assert_eq!(t, vec![Token::Number("1".to_string())]);
    assert_eq!(b.len(), 1);
    // 外側全体を囲まない括弧は残る。
    let (t, _) = strip_outer_parens((
        vec![
            Token::Punct('('),
            Token::Number("1".to_string()),
            Token::Punct(')'),
            Token::Punct('+'),
            Token::Punct('('),
            Token::Number("2".to_string()),
            Token::Punct(')'),
        ],
        vec![false; 7],
    ));
    assert_eq!(t.len(), 7);
    // `((a) + (b))` は 1 組だけ剥がす。
    let (t, b) = strip_outer_parens((tokenize("((1) + (2))").expect("tokenize"), vec![false; 9]));
    assert_eq!(t.len(), 7);
    assert_eq!(b.len(), 7);
}

#[test]
fn deeply_nested_parentheses_are_accepted_or_rejected_without_blowup() {
    let n = 5_000;
    let src = format!(
        "BEGIN RETURN QUERY {}SELECT 1{}; END",
        "(".repeat(n),
        ")".repeat(n)
    );
    let o = opts_with(ReturnShape::SetOf);
    // 受理でも上限超過の拒否でもよい。重要なのは線形時間で終わること。
    let _ = parse_body(&src, &o);
    let big = format!(
        "BEGIN RETURN QUERY {}SELECT 1{}; END",
        "(".repeat(30_000),
        ")".repeat(30_000)
    );
    let _ = parse_body(&big, &o);
}

#[test]
fn many_declarations_resolve_in_linear_time() {
    // 変数名の表は連想配列なので、宣言数・代入数に対して線形で終わる。
    let decls: String = (0..1_500).map(|i| format!("v{i} int; ")).collect();
    let stmts: String = (0..1_500).map(|i| format!("v{i} := {i}; ")).collect();
    let start = std::time::Instant::now();
    ok(&format!("DECLARE {decls} BEGIN {stmts} END"));
    assert!(start.elapsed().as_secs() < 5);
}

#[test]
fn quoted_flags_align_with_tokens() {
    use crate::sql::lexer::tokenize_with_params_quoted;
    let (t, q) =
        tokenize_with_params_quoted("SELECT \"into\" , into , \"true\" , \"x\"").expect("tokenize");
    assert_eq!(t.len(), q.len());
    let flagged: Vec<&Token> = t
        .iter()
        .zip(&q)
        .filter(|(_, f)| **f)
        .map(|(t, _)| t)
        .collect();
    assert_eq!(flagged.len(), 3);
    assert!(q
        .iter()
        .zip(&t)
        .all(|(f, t)| !*f || !matches!(t, Token::Keyword(_))));
}

#[test]
fn array_commas_keep_variable_references() {
    // `ARRAY[1, v]` のカンマの後でも、`v` は参照のまま。
    let b = classified("DECLARE v int; BEGIN PERFORM ARRAY[1, v, 3]; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = classified("DECLARE v int; BEGIN PERFORM ARRAY[v, v]; END");
    assert_eq!(first_sql_refs(&b), 2);
}

#[test]
fn argument_mode_words_can_name_variables() {
    // 引数モードの語・`AT LOCAL` の語は予約語ではないので宣言でき、構文上の語は変数にならない。
    for w in ["out", "inout", "variadic_", "local"] {
        ok(&format!("DECLARE {w} int; BEGIN NULL; END"));
    }
    let b = classified("DECLARE local int; BEGIN PERFORM now() AT TIME ZONE 'UTC'; END");
    assert_eq!(first_sql_refs(&b), 0);
}

#[test]
fn into_after_insert_word_follows_postgresql_make_execsql_stmt() {
    // PostgreSQL 17 `make_execsql_stmt`（pl_gram.y）は、深さを問わず `K_INTO` の直前が `K_INSERT` なら
    // 「INSERT INTO」とみなして代入先として切り出さない（`MERGE INTO`・先頭が `IMPORT` の `INTO` も同様）。
    // 引用なしの `insert` は文脈によらず `K_INSERT` なので、列名の `insert` の直後の `INTO` も同じ扱いで、
    // 切り出さない（本実装の `split_into` と一致）。引用付きの `"insert"` は識別子なので代入先として切り出す。
    let into_of = |src: &str| {
        let b = ok(&format!("DECLARE n int; BEGIN {src}; END"));
        match b.block.stmts.first() {
            Some(Stmt::Sql(q)) => q.into.is_some(),
            other => panic!("not sql: {other:?}"),
        }
    };
    assert!(!into_of("SELECT insert INTO n FROM t"));
    assert!(!into_of("SELECT x AS insert INTO n FROM t"));
    assert!(into_of("SELECT \"insert\" INTO n FROM t"));
    assert!(into_of("SELECT x AS \"insert\" INTO n FROM t"));
    // 通常の `INSERT INTO`（切り出さない）と `RETURNING ... INTO`（切り出す）。
    assert!(!into_of("INSERT INTO t(x) VALUES (1)"));
    assert!(into_of("INSERT INTO t(x) VALUES (1) RETURNING x INTO n"));
    // MERGE・IMPORT は本実装では範囲外（0A000）。
    assert_eq!(
        code("BEGIN MERGE INTO t USING u ON true WHEN MATCHED THEN DELETE; END"),
        "0A000"
    );
}
