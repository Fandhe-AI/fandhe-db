//! `plpgsql` 構文解析器の単体テスト（Issue #2224・SQL-68）。
//!
//! 受理する構文の網羅（ADR の構文表の「受理」列）、範囲外（`0A000`）、構文誤り（`42601`）、
//! 上限（`54000`／`54001`）、`$n` の範囲外（`42P02`）、エラー文言の非漏えいを固定する。
//! `pub(crate)` モジュールのため crate 内テストとして置く（結合テストからは呼べない）。

use super::ast::*;
use super::embedded::{dummy_token, is_ident_position, validate_embedded, EmbeddedSqlChecker};
use super::lexer::{scan, TokKind};
use super::{parse_body, ParseOptions};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
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
         r RECORD; t users.id%TYPE; w users%ROWTYPE; c varchar(5) COLLATE \"C\"; BEGIN x := x + 1; END outer;",
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
    let b = ok("DECLARE x int; BEGIN x := a + x + $1; x = 3; END");
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
    assert_eq!(expr.var_refs[0], (0, VarRef::Var(a)));
    assert_eq!(expr.var_refs[1], (2, VarRef::Var(x)));
    assert_eq!(expr.var_refs[2], (4, VarRef::Var(a)));
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
    let b = ok("<<o>> DECLARE x int; BEGIN DECLARE x int; BEGIN x := o.x; END; END");
    let Some(Stmt::Block(inner)) = b.block.stmts.first() else {
        panic!("inner block");
    };
    let Some(Stmt::Assign { expr, .. }) = inner.stmts.first() else {
        panic!("assign");
    };
    assert_eq!(expr.var_refs, vec![(0, VarRef::Var(b.block.decls[0]))]);
}

#[test]
fn record_field_reference_and_target() {
    let b = ok("DECLARE r RECORD; n int; BEGIN n := r.f; r.g := 1; END");
    let r = vid(&b, "r");
    let Some(Stmt::Assign { expr, .. }) = b.block.stmts.first() else {
        panic!("assign");
    };
    assert_eq!(expr.var_refs, vec![(0, VarRef::Field(r, "f".to_string()))]);
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
    let b = ok("DECLARE s int := 0; BEGIN \
         <<l1>> LOOP EXIT l1 WHEN s > 3; CONTINUE WHEN s = 1; s := s + 1; END LOOP l1; \
         WHILE s < 10 LOOP s := s + 1; END LOOP; \
         FOR i IN 1..10 LOOP s := s + i; END LOOP; \
         FOR i IN REVERSE 10..1 BY 2 LOOP NULL; END LOOP; \
         <<q>> FOR rec IN SELECT id FROM t WHERE id > a LOOP s := rec.id; CONTINUE q; END LOOP q; \
         FOR rec2 IN WITH c AS (SELECT 1) SELECT * FROM c LOOP EXIT; END LOOP; \
         END");
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
        .any(|(_, r)| *r == VarRef::Var(VarId(0))));
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
    let b = ok("DECLARE n int; r RECORD; BEGIN \
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
         END");
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
    let b = ok("DECLARE x int; BEGIN INSERT INTO t(x) VALUES (x); UPDATE t SET x = x + 1; END");
    let Some(Stmt::Sql(ins)) = b.block.stmts.first() else {
        panic!("insert");
    };
    assert_eq!(ins.var_refs.len(), 1);
    assert_eq!(ins.var_refs[0].0, 8);
    let Some(Stmt::Sql(upd)) = b.block.stmts.get(1) else {
        panic!("update");
    };
    assert_eq!(upd.var_refs.len(), 1);
    assert_eq!(upd.var_refs[0].0, 5);
}

#[test]
fn ident_position_classification() {
    let t = tokenize("SELECT f(x) FROM x AS y").expect("tokenize");
    // f( の f・FROM の x・AS の y は識別子位置、f( の引数 x は違う。
    assert!(is_ident_position(&t, 1));
    assert!(!is_ident_position(&t, 3));
    assert!(is_ident_position(&t, 6));
    assert!(is_ident_position(&t, 8));
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
    ok(&build(64));
    assert_eq!(code(&build(65)), "54000");
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

// ---- 定義時ダミー検証（モック checker） ----

struct Recorder(std::cell::RefCell<Vec<Vec<Token>>>);

impl EmbeddedSqlChecker for Recorder {
    fn check_syntax(&self, tokens: Vec<Token>) -> Result<(), SqlSurfaceError> {
        self.0.borrow_mut().push(tokens);
        Ok(())
    }
}

struct Failing;

impl EmbeddedSqlChecker for Failing {
    fn check_syntax(&self, _tokens: Vec<Token>) -> Result<(), SqlSurfaceError> {
        Err(SqlSurfaceError::unsupported("mock failure"))
    }
}

#[test]
fn dummy_validation_replaces_variables_and_params() {
    let b = ok("DECLARE n int := 1; flag boolean; s text; BEGIN \
         n := n + a; PERFORM flag, s, $2; END");
    let rec = Recorder(Default::default());
    validate_embedded(&b, &rec).expect("validate");
    let calls = rec.0.borrow();
    // 既定値（n := 1）・代入式・PERFORM の 3 件。
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[1],
        vec![
            Token::Keyword(Keyword::Select),
            Token::Number("0".to_string()),
            Token::Punct('+'),
            Token::Number("0".to_string()),
        ]
    );
    assert_eq!(
        calls[2],
        vec![
            Token::Keyword(Keyword::Select),
            Token::Ident("false".to_string()),
            Token::Punct(','),
            Token::StringLiteral("0".to_string()),
            Token::Punct(','),
            Token::StringLiteral("0".to_string()),
        ]
    );
}

#[test]
fn dummy_validation_propagates_checker_errors() {
    let b = ok("BEGIN PERFORM 1; END");
    let e = validate_embedded(&b, &Failing).expect_err("must fail");
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
fn extended_ident_positions() {
    let c = |s: &str, i: usize| is_ident_position(&tokenize(s).expect("tokenize"), i);
    assert!(c("SELECT 1 FROM t x", 4));
    assert!(c("SELECT 1 FROM t JOIN u USING (k)", 8));
    assert!(c("DELETE FROM t USING u", 4));
    assert!(c(
        "INSERT INTO t VALUES (1) ON CONFLICT (k) DO UPDATE SET v = 1",
        10
    ));
    assert!(c(
        "INSERT INTO t VALUES (1) ON CONFLICT (k) DO UPDATE SET v = 1, w = 2",
        19
    ));
    assert!(!c("SELECT 1 FROM t WHERE x = 1", 5));
    let q = tokenize("SELECT 1 FROM r.items").expect("tokenize");
    assert!(is_ident_position(&q, 3));
}

#[test]
fn type_name_and_cte_names_are_not_variable_references() {
    let b = ok("DECLARE text text; BEGIN PERFORM 1::text; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert!(p.var_refs.is_empty());
    let b = ok("DECLARE c int; BEGIN WITH c AS (SELECT 1) SELECT * FROM c INTO STRICT c; END");
    let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
        panic!("sql");
    };
    assert!(s.var_refs.is_empty());
    let b = ok("DECLARE a int; BEGIN WITH c (a) AS (SELECT 1) SELECT 1 FROM c; END");
    let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
        panic!("sql");
    };
    assert!(s.var_refs.is_empty());
}

#[test]
fn with_statement_kind_follows_the_main_statement() {
    let kind = |src: &str| {
        let b = ok(src);
        let Some(Stmt::Sql(s)) = b.block.stmts.first() else {
            panic!("sql");
        };
        (s.kind, s.writes)
    };
    assert_eq!(
        kind("BEGIN WITH c AS (SELECT 1) UPDATE t SET x = 1; END"),
        (EmbeddedKind::Update, true)
    );
    assert_eq!(
        kind("BEGIN WITH c AS (SELECT 1) DELETE FROM t; END"),
        (EmbeddedKind::Delete, true)
    );
    assert_eq!(
        kind("BEGIN WITH c AS (SELECT 1) SELECT 1 FROM c; END"),
        (EmbeddedKind::Select, false)
    );
    assert_eq!(
        kind("BEGIN WITH c AS (DELETE FROM t RETURNING 1) SELECT 1 FROM c; END"),
        (EmbeddedKind::Select, true)
    );
}

#[test]
fn set_column_in_select_list_does_not_hide_variables() {
    let c = |s: &str, i: usize| is_ident_position(&tokenize(s).expect("tokenize"), i);
    // `SELECT set, v = 1`: v は SET 代入先ではない。
    assert!(!c("SELECT set , v = 1", 3));
    assert!(c("UPDATE t SET v = 1 , w = 2", 7));
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
