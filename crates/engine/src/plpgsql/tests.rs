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
fn with_insert_and_cte_insert_column_lists_are_identifier_positions() {
    for src in [
        "DECLARE x int; BEGIN WITH c AS (SELECT 1) INSERT INTO t(x) VALUES (x); END",
        "DECLARE x int; BEGIN WITH c AS (INSERT INTO t(x) VALUES (x) RETURNING 1) SELECT 1; END",
    ] {
        let b = ok(src);
        let Some(Stmt::Sql(q)) = b.block.stmts.first() else {
            panic!("sql");
        };
        assert_eq!(q.var_refs.len(), 1, "for {src:?}");
        assert!(q.writes);
    }
}

#[test]
fn return_query_and_for_query_classify_write_main_statements() {
    let o = opts_with(ReturnShape::SetOf);
    let b = ok_with("BEGIN RETURN QUERY UPDATE t SET x = 1 RETURNING x; END", &o);
    let Some(Stmt::ReturnQuery(q)) = b.block.stmts.first() else {
        panic!("rq");
    };
    assert!(q.writes);
    assert_eq!(q.kind, EmbeddedKind::Update);
    let b = ok_with(
        "BEGIN FOR r IN WITH c AS (SELECT 1) UPDATE t SET x = 1 RETURNING x LOOP NULL; END LOOP; END",
        &opts(),
    );
    let Some(Stmt::ForQuery { query, .. }) = b.block.stmts.first() else {
        panic!("for");
    };
    assert!(query.writes);
    let b = ok_with("BEGIN RETURN QUERY SELECT 1; END", &o);
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
#[test]
fn expression_from_does_not_hide_variables() {
    let b = ok("DECLARE v int; BEGIN PERFORM 1 IS DISTINCT FROM v; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert_eq!(p.var_refs.len(), 1);
    let b = ok("DECLARE v int; BEGIN PERFORM substring('abc' FROM v); END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert_eq!(p.var_refs.len(), 1);
    let b = ok("DECLARE v int; BEGIN PERFORM 1 FROM t WHERE x IS NOT DISTINCT FROM v; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert_eq!(p.var_refs.len(), 1);
}

#[test]
fn from_list_tables_and_aliases_are_not_variables() {
    let c = |s: &str, i: usize| is_ident_position(&tokenize(s).expect("tokenize"), i);
    // `SELECT 1 FROM t , u` の u、`FROM t a , u b` の b、JOIN ON 後のカンマ以降の表。
    assert!(c("SELECT 1 FROM t , u", 5));
    assert!(c("SELECT 1 FROM t a , u b", 7));
    assert!(c("SELECT 1 FROM t JOIN s ON a = b , u", 11));
    // ON 条件式・WHERE の語は変数でありうる。
    assert!(!c("SELECT 1 FROM t JOIN s ON a = v", 9));
    assert!(!c("SELECT 1 FROM t , u WHERE v = 1", 8));
    let b = ok("DECLARE u int; BEGIN PERFORM 1 FROM t , u; END");
    let Some(Stmt::Perform(p)) = b.block.stmts.first() else {
        panic!("perform");
    };
    assert!(p.var_refs.is_empty());
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
fn relation_slots_do_not_swallow_value_positions() {
    // ON 式内の left(...) 関数呼び出しは JOIN 語ではなく、後続の値位置の語は関係位置にならない。
    let s = "SELECT 1 FROM a JOIN b ON a.x = left(b.y, 2) AND a.z = v";
    let t = tokenize(s).expect("tokenize");
    assert!(!is_ident_position(&t, t.len() - 1));
}

#[test]
fn delegated_type_errors_do_not_echo_fragments() {
    let e =
        parse_body("DECLARE x float(secret_xyz); BEGIN NULL; END", &opts()).expect_err("bad type");
    assert_eq!(e.wire_code(), "42601");
    assert!(!e.to_string().contains("secret"), "leaked: {e}");
}

#[test]
fn extract_field_is_ident_position() {
    // `EXTRACT(year FROM ts)` の field は変数として置換しない。
    let t = tokenize("SELECT EXTRACT(year FROM ts)").expect("tokenize");
    assert!(is_ident_position(&t, 3));
}

#[test]
fn sql_keyword_named_variable_is_rejected_at_declaration() {
    // `limit` 等は式中で `Token::Keyword` になり参照を解決できないため、宣言時に拒否する。
    assert_eq!(
        code("DECLARE limit int := 1; BEGIN RETURN limit; END"),
        "42601"
    );
}

#[test]
fn classification_is_linear_for_many_references() {
    // 多数の変数参照を持つ式でも二乗時間にならない（事前計算した括弧情報を使う）。
    let expr = vec!["a"; 3_000].join(" + ");
    let t = tokenize(&format!("SELECT {expr}")).expect("tokenize");
    let start = std::time::Instant::now();
    let ctx = super::embedded::IdentCtx::new(&t, &vec![false; t.len()]);
    for i in 0..t.len() {
        let _ = super::embedded::is_ident_position_with(i, &ctx);
    }
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
fn ambiguous_names_are_rejected_at_declaration_with_0a000() {
    // 構造語・リテラル語・文の語は、未引用でも引用付きでも変数名にできない。
    for name in [
        "set",
        "into",
        "using",
        "values",
        "insert",
        "update",
        "delete",
        "with",
        "get",
        "first",
        "next",
        "strict",
        "reverse",
        "exception",
        "operation_id",
        "group",
        "returning",
        "offset",
        "true",
        "false",
        "null_",
        "default",
        "array",
        "current_date",
        "current_timestamp",
    ] {
        // `null_` は許可される名前（下で受理を確認する）。
        if name == "null_" {
            ok(&format!("DECLARE {name} int; BEGIN NULL; END"));
            continue;
        }
        for decl in [name.to_string(), format!("\"{name}\"")] {
            assert_eq!(
                code(&format!("DECLARE {decl} int; BEGIN NULL; END")),
                "0A000",
                "for {decl}"
            );
        }
    }
    // 引用付きの "null" は予約語経路（42601）か 0A000 のどちらでも拒否される。
    assert_ne!(code("DECLARE \"null\" int; BEGIN NULL; END"), "");
    // ループ変数・ラベル・パラメータ名も同じ。
    assert_eq!(
        code("BEGIN FOR set IN 1..3 LOOP NULL; END LOOP; END"),
        "0A000"
    );
    assert_eq!(
        code("BEGIN FOR \"into\" IN SELECT 1 LOOP NULL; END LOOP; END"),
        "0A000"
    );
    // ラベルは修飾子としてだけ使われ識別子位置に現れないので、構造語と同名でも宣言できる。
    ok("<<values>> BEGIN NULL; END");
    let o = ParseOptions::new(
        vec![ParamDecl {
            name: Some("Set".to_string()),
            ty: SqlColumnTypeName::Integer,
        }],
        ReturnShape::Scalar,
    );
    assert_eq!(code_with("BEGIN RETURN 1; END", &o), "0A000");
    // エラー文言は名前を含めない。
    let e = parse_body("DECLARE set int; BEGIN NULL; END", &opts()).expect_err("reject");
    assert!(!e.to_string().to_lowercase().contains("set"), "leaked: {e}");
}

#[test]
fn rejected_names_keep_literals_and_clauses_literal() {
    // 同名の変数を宣言できないので、リテラル・構造語は変数参照にならない。
    let b = ok("BEGIN IF true THEN NULL; END IF; END");
    let Some(Stmt::If { branches, .. }) = b.block.stmts.first() else {
        panic!("if");
    };
    assert!(branches[0].0.var_refs.is_empty());
    let b = ok("BEGIN INSERT INTO t(x) VALUES (default); END");
    assert_eq!(first_sql_refs(&b), 0);
    let b = ok("BEGIN PERFORM ARRAY[1, 2]; END");
    assert_eq!(first_sql_refs(&b), 0);
    let b = ok("DECLARE v int; BEGIN UPDATE t SET x = v; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = ok("DECLARE n int; BEGIN PERFORM 1 FROM t LIMIT 1 FETCH FIRST n ROWS ONLY; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = ok("DECLARE n int; BEGIN PERFORM 1 FROM t GROUP BY x HAVING count(*) > n; END");
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
fn insert_assignment_is_not_declarable_so_statements_stay_sql() {
    // `insert := 1` は変数が宣言できない（0A000）ので SQL 文として読まれることはない。
    assert_eq!(code("DECLARE insert int; BEGIN insert := 1; END"), "0A000");
    let b = ok("BEGIN insert := 1; END");
    assert!(matches!(b.block.stmts.first(), Some(Stmt::Sql(_))));
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
    let b = ok("<<blk>> BEGIN RETURN blk.a; END");
    let Some(Stmt::Return(Some(e))) = b.block.stmts.first() else {
        panic!("return");
    };
    assert_eq!(e.var_refs.len(), 1);
    let b = ok("<<blk>> BEGIN PERFORM blk.found; END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = ok("<<blk>> BEGIN blk.a := 2; RETURN 1; END");
    assert!(matches!(b.block.stmts.first(), Some(Stmt::Assign { .. })));
    assert_eq!(
        code("<<blk>> BEGIN blk.found := true; RETURN 1; END"),
        "42601"
    );
    // 内側ブロックのラベルではパラメータを修飾参照しない（最外ブロックのみ）。
    let b = ok("BEGIN <<inner>> BEGIN PERFORM inner.a; END; END");
    let Some(Stmt::Block(inner)) = b.block.stmts.first() else {
        panic!("block");
    };
    let Some(Stmt::Perform(p)) = inner.stmts.first() else {
        panic!("perform");
    };
    assert!(p.var_refs.is_empty());
}

#[test]
fn with_classification_skips_cte_definitions() {
    let kind = |src: &str| {
        let b = ok(&format!("BEGIN {src}; END"));
        match b.block.stmts.first() {
            Some(Stmt::Sql(q)) => (q.kind, q.writes),
            other => panic!("not sql: {other:?}"),
        }
    };
    // CTE 名が主文の語と同名でも、CTE 定義の後の主文で分類する。
    assert_eq!(
        kind("WITH \"values\" AS (SELECT 1) UPDATE t SET x = 1"),
        (EmbeddedKind::Update, true)
    );
    assert_eq!(
        kind("WITH \"update\" AS (SELECT 1) SELECT 1 FROM \"update\""),
        (EmbeddedKind::Select, false)
    );
    assert_eq!(
        kind("WITH \"delete\" (a, b) AS NOT MATERIALIZED (SELECT 1, 2), c AS (SELECT 2) DELETE FROM t"),
        (EmbeddedKind::Delete, true)
    );
    assert_eq!(
        kind("WITH RECURSIVE r AS (SELECT 1) INSERT INTO t SELECT * FROM r"),
        (EmbeddedKind::Insert, true)
    );
    // 引用付きの "recursive" は CTE 名（キーワードと取り違えない）。
    assert_eq!(
        kind("WITH \"recursive\" AS (SELECT 1) UPDATE t SET x = 1"),
        (EmbeddedKind::Update, true)
    );
    assert_eq!(
        kind("WITH \"recursive\"(a) AS (SELECT 1) SELECT a FROM \"recursive\""),
        (EmbeddedKind::Select, false)
    );
    assert_eq!(
        kind("WITH recursive \"recursive\" AS (SELECT 1) DELETE FROM t"),
        (EmbeddedKind::Delete, true)
    );
    // データ変更 CTE を持つ SELECT は書き込み扱い。
    assert_eq!(
        kind("WITH c AS (DELETE FROM t RETURNING 1) SELECT 1 FROM c"),
        (EmbeddedKind::Select, true)
    );
    // 形を読み解けない WITH は fail-closed で拒否する。
    assert_eq!(code("BEGIN WITH c AS SELECT 1; END"), "42601");
    assert_eq!(
        code("BEGIN RETURN QUERY WITH c AS (SELECT 1) TABLE t; END"),
        "42601"
    );
}

#[test]
fn declaration_default_resolves_in_the_scope_before_the_declaration() {
    // PostgreSQL 17 の `decl_statement`（pl_gram.y）は既定値を読んだ後に変数を名前空間へ追加するため、
    // 内側 `DECLARE x int := x` の既定値の `x` は外側の x に解決される。
    let b = ok("DECLARE x int := 10; BEGIN DECLARE x int := x; BEGIN RETURN x; END; END");
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
    assert_eq!(default.var_refs[0].1, VarRef::Var(outer));
    // 内側の宣言後の参照は内側の x（外側を隠す）。
    let Some(Stmt::Return(Some(e))) = inner.stmts.first() else {
        panic!("return");
    };
    assert_eq!(e.var_refs[0].1, VarRef::Var(inner_id));
}

#[test]
fn type_specifications_are_not_variable_references() {
    // 型名の語（`precision`・`varying`・`zone` 等）は変数名にできない（宣言時に拒否）。
    for w in [
        "precision",
        "varying",
        "zone",
        "time",
        "timestamp",
        "numeric",
    ] {
        assert_eq!(
            code(&format!("DECLARE {w} int; BEGIN NULL; END")),
            "0A000",
            "for {w}"
        );
    }
    // 型指定の範囲（複数語・修飾子・`[]`）は `::` と `CAST` の両方で識別子位置になり、その後に続く
    // 変数参照だけが置換される。
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
            assert_eq!(first_sql_refs(&ok(&src)), 0, "for {src:?}");
            let src = format!("DECLARE v int; BEGIN PERFORM {expr} + v; END");
            assert_eq!(first_sql_refs(&ok(&src)), 1, "for {src:?}");
        }
    }
    // 型の外の `AS` 別名は従来どおり識別子位置、`CAST` の式部分の変数は参照になる。
    let b = ok("DECLARE v int; BEGIN PERFORM CAST(v AS int); END");
    assert_eq!(first_sql_refs(&b), 1);
    let b = ok("DECLARE v int; BEGIN RETURN 1::double precision; END");
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
fn quoted_words_in_with_and_query_classification() {
    let kind = |src: &str| {
        let b = ok(&format!("BEGIN {src}; END"));
        match b.block.stmts.first() {
            Some(Stmt::Sql(q)) => (q.kind, q.writes),
            other => panic!("not sql: {other:?}"),
        }
    };
    assert_eq!(
        kind("WITH \"recursive\" AS (SELECT 1) SELECT 1 FROM \"recursive\""),
        (EmbeddedKind::Select, false)
    );
    assert_eq!(
        kind("WITH \"recursive\" AS (SELECT 1) DELETE FROM t"),
        (EmbeddedKind::Delete, true)
    );
    assert_eq!(
        kind("WITH RECURSIVE r AS (SELECT 1) SELECT 1 FROM r"),
        (EmbeddedKind::Select, false)
    );
    // 引用付きの "delete" CTE 名・"update" 括弧始まりの語は書き込み扱いにならない。
    assert_eq!(
        kind("WITH c AS (SELECT 1 AS \"delete\") SELECT 1 FROM c"),
        (EmbeddedKind::Select, false)
    );
}

#[test]
fn derived_table_column_aliases_are_identifier_positions() {
    let refs = |src: &str| {
        let b = ok(&format!("DECLARE v int; BEGIN {src}; END"));
        first_sql_refs(&b)
    };
    assert_eq!(refs("PERFORM 1 FROM (SELECT 1) AS d(v)"), 0);
    assert_eq!(refs("PERFORM 1 FROM (SELECT 1) d(v)"), 0);
    assert_eq!(refs("PERFORM 1 FROM t AS a(v)"), 0);
    assert_eq!(refs("PERFORM 1 FROM t a(v)"), 0);
    assert_eq!(refs("PERFORM 1 FROM t, (SELECT 1, 2) AS d(v, w)"), 0);
    // 後続の値の位置の変数は参照のまま。
    assert_eq!(refs("PERFORM 1 FROM (SELECT 1) AS d(x) WHERE d.x = v"), 1);
    // 関数呼び出しの引数は列別名リストではない（変数は参照のまま）。
    assert_eq!(refs("PERFORM 1 FROM generate_series(1, v)"), 1);
    assert_eq!(
        refs("PERFORM 1 FROM t JOIN generate_series(1, v) ON true"),
        1
    );
    assert_eq!(refs("PERFORM 1 FROM t, generate_series(1, v)"), 1);
    assert_eq!(refs("PERFORM 1 FROM ONLY t WHERE x = v"), 1);
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

// ---- 拒否語の網羅性（SQL 側が照合する語の漏れ検出） ----

/// 照合語として扱う関数・メソッド名（引数の文字列リテラルを語として集める）。
const MATCH_HELPERS: &[&str] = &[
    "peek_ident_matches",
    "peek_ident_matches_at",
    "expect_ident_matching",
    "expect_contextual_keyword",
    "peek_contextual_keyword",
    "ident_is",
    "is_kw",
    "eat_kw",
    "expect_kw",
    "eat_word",
    "is_word",
    "is_ident",
    "ident_eq_ignore_case",
    "eq_ignore_ascii_case",
    "kw",
    "word",
    "ident_at",
    "depth0_words",
    "prev_is",
    "next_is",
    "expect_contextual_ident",
    "peek_truncate_keyword",
    "expr_until",
    "parse_relation_alias",
];

/// 拒否語としない語（理由付き）。関数名は直後が `(` なら識別子位置として扱われ、変数参照にならない。
const EXCLUDED_FUNCTIONS: &[&str] = &[
    "array_agg",
    "avg",
    "bool_and",
    "bool_or",
    "coalesce",
    "concat",
    "count",
    "cume_dist",
    "currval",
    "date_part",
    "date_trunc",
    "dense_rank",
    "every",
    "extract",
    "first_value",
    "greatest",
    "json_array_length",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "lag",
    "last_value",
    "lastval",
    "lead",
    "least",
    "log",
    "lpad",
    "max",
    "min",
    "nextval",
    "nth_value",
    "ntile",
    "nullif",
    "overlay",
    "percent_rank",
    "position",
    "rank",
    "replace",
    "round",
    "row_number",
    "rpad",
    "set_config",
    "setval",
    "split_part",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "string_agg",
    "substr",
    "substring",
    "sum",
    "timezone",
    "trim",
    "trunc",
    "var_pop",
    "var_samp",
    "variance",
    "current_schema",
    "current_schemas",
    "visible",
];

/// DDL・DCL・ユーティリティ・トランザクション・カーソル・COPY の語。埋め込み文の文頭の語は
/// `OUT_OF_SCOPE_STARTS` で `0A000` になり、DML・式の文法には現れない。
const EXCLUDED_UTILITY: &[&str] = &[
    "abort",
    "action",
    "add",
    "alter",
    "always",
    "analyze",
    "assert",
    "authorization",
    "binary",
    "btree",
    "cache",
    "call",
    "cancel",
    "cascade",
    "cascaded",
    "check",
    "chunk",
    "close",
    "cluster",
    "column",
    "comment",
    "commit",
    "concurrently",
    "copy",
    "create",
    "csv",
    "data",
    "deallocate",
    "deferrable",
    "deferred",
    "delimiter",
    "discard",
    "dml",
    "drop",
    "enum",
    "explain",
    "force",
    "foreach",
    "foreign",
    "forward",
    "freeze",
    "function",
    "generated",
    "gin",
    "global",
    "grant",
    "header",
    "hnsw",
    "identity",
    "import",
    "increment",
    "index",
    "initially",
    "immediate",
    "key",
    "listen",
    "local",
    "lock",
    "match",
    "maxvalue",
    "merge",
    "minvalue",
    "move",
    "notify",
    "nowait",
    "open",
    "option",
    "owned",
    "owner",
    "prepare",
    "preserve",
    "primary",
    "raise",
    "references",
    "refresh",
    "reindex",
    "release",
    "rename",
    "reset",
    "restart",
    "restrict",
    "revoke",
    "rollback",
    "savepoint",
    "schema",
    "sequence",
    "session",
    "share",
    "show",
    "simple",
    "start",
    "stdin",
    "stdout",
    "temp",
    "temporary",
    "transaction",
    "truncate",
    "type",
    "unique",
    "unlogged",
    "vacuum",
    "view",
    "work",
];

/// 構文上のキーワードではなく、文字列値・内部の列名・スキーマ名・単位名の照合に使われる語。
const EXCLUDED_VALUES: &[&str] = &[
    "day",
    "hour",
    "minute",
    "month",
    "second",
    "year",
    "f",
    "t",
    "inf",
    "infinity",
    "nan",
    "id",
    "tenant_id",
    "visibility",
    "pg_catalog",
    "pg_temp",
    "public",
    "information_schema",
    "search_path",
];

/// 宣言型（`DECLARE x anyelement`）の判定でだけ照合する多相型名（式には現れない）。
const EXCLUDED_POLYMORPHIC_TYPES: &[&str] = &[
    "anyarray",
    "anyelement",
    "anyenum",
    "anymultirange",
    "anynonarray",
    "anyrange",
];

fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if name != "tests" {
                rust_sources(&p, out);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs") {
            out.push(p);
        }
    }
}

/// `b[i]` が `"` のとき、文字列リテラルの内容と閉じ引用符の次の位置を返す。
fn string_literal(b: &[u8], i: usize) -> Option<(&str, usize)> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return std::str::from_utf8(&b[i + 1..j]).ok().map(|s| (s, j + 1)),
            _ => j += 1,
        }
    }
    None
}

fn is_word_literal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphabetic() || c == b'_')
}

/// 呼び出しの括弧の中（`i` は `(` の次）から、文字列リテラルを集める。
fn call_literals(b: &[u8], mut i: usize) -> Vec<&str> {
    let mut depth = 1usize;
    let mut out = Vec::new();
    while i < b.len() && depth > 0 {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'"' => match string_literal(b, i) {
                Some((s, next)) => {
                    out.push(s);
                    i = next;
                    continue;
                }
                None => break,
            },
            _ => {}
        }
        i += 1;
    }
    out
}

/// 1 ファイルの本体（`#[cfg(test)] mod` より前）から、SQL・PL/pgSQL が照合する語を集める。
/// 規則: (1) `MATCH_HELPERS` の呼び出し引数の英字リテラル、(2) 任意の呼び出しの引数にある
/// 大文字だけの英字リテラル（2 文字以上。照合ヘルパーの名前変更・追加の取りこぼし対策）、
/// (3) `[ "A", "B" ].iter()`／`.contains(` の配列、(4) plpgsql 側の `&[&str]` 定数。
fn matched_words(src: &str, is_plpgsql: bool, out: &mut std::collections::BTreeSet<String>) {
    let src = src
        .find("\n#[cfg(test)]\nmod ")
        .map_or(src, |k| src.get(..k).unwrap_or(src));
    let b = src.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let name = &src[start..i];
            let mut k = i;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if b.get(k) == Some(&b'(') {
                let helper = MATCH_HELPERS.contains(&name);
                for lit in call_literals(b, k + 1) {
                    let upper =
                        lit.len() >= 2 && lit.bytes().all(|c| c.is_ascii_uppercase() || c == b'_');
                    if (helper && is_word_literal(lit)) || upper {
                        out.insert(lit.to_ascii_lowercase());
                    }
                }
            }
            continue;
        }
        if c == b'"' {
            // 通常の文字列は読み飛ばす（中の `[` や識別子風の語を拾わない）。
            match string_literal(b, i) {
                Some((_, next)) => i = next,
                None => break,
            }
            continue;
        }
        if c == b'[' {
            let mut j = i + 1;
            let mut words = Vec::new();
            let mut ok = false;
            loop {
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    break;
                }
                let Some((s, next)) = string_literal(b, j) else {
                    break;
                };
                words.push(s);
                j = next;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                match b.get(j) {
                    Some(b',') => j += 1,
                    Some(b']') => {
                        ok = true;
                        break;
                    }
                    _ => break,
                }
                // 末尾カンマの後の `]`。
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) == Some(&b']') {
                    ok = true;
                    break;
                }
            }
            if ok {
                let rest = src.get(j + 1..).unwrap_or("").trim_start();
                if rest.starts_with(".iter()") || rest.starts_with(".contains(") {
                    for w in words.iter().filter(|w| is_word_literal(w)) {
                        out.insert(w.to_ascii_lowercase());
                    }
                }
            }
        }
        i += 1;
    }
    if is_plpgsql {
        let mut from = 0usize;
        while let Some(k) = src.get(from..).and_then(|r| r.find("&[&str] = &[")) {
            let at = from + k;
            let head = src.get(at.saturating_sub(60)..at).unwrap_or("");
            let end = src
                .get(at..)
                .and_then(|r| r.find("];"))
                .map_or(src.len(), |e| at + e);
            // 拒否語の定義そのものは検査対象にしない。
            if !head.contains("UNSUPPORTED_NAMES") {
                let body = src.get(at..end).unwrap_or("");
                let bb = body.as_bytes();
                let mut p = 0usize;
                while p < bb.len() {
                    if bb[p] == b'"' {
                        match string_literal(bb, p) {
                            Some((s, next)) => {
                                if is_word_literal(s) {
                                    out.insert(s.to_ascii_lowercase());
                                }
                                p = next;
                                continue;
                            }
                            None => break,
                        }
                    }
                    p += 1;
                }
            }
            from = end.max(at + 1);
        }
    }
}

#[test]
fn every_matched_sql_word_is_rejected_or_excluded() {
    use super::parser::{OUT_OF_SCOPE_STARTS, RESERVED, SQL_KEYWORD_NAMES, UNSUPPORTED_NAMES};
    use std::collections::BTreeSet;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root.join("sql"), &mut files);
    rust_sources(&root.join("plpgsql"), &mut files);
    assert!(files.len() > 20, "source discovery failed: {}", files.len());
    let mut words = BTreeSet::new();
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read source");
        let is_plpgsql = f.components().any(|c| c.as_os_str() == "plpgsql");
        matched_words(&src, is_plpgsql, &mut words);
    }
    // SQL 字句解析が Keyword にする語（`"SELECT" => Some(Keyword::Select)` の腕）。
    let lexer = std::fs::read_to_string(root.join("sql").join("lexer.rs")).expect("lexer");
    let mut keywords = BTreeSet::new();
    let mut rest = lexer.as_str();
    while let Some(k) = rest.find("\" => Some(Keyword::") {
        let head = rest.get(..k).unwrap_or("");
        if let Some(q) = head.rfind('"') {
            keywords.insert(head[q + 1..].to_ascii_lowercase());
        }
        rest = rest.get(k + 1..).unwrap_or("");
    }
    assert!(
        keywords.len() >= 7,
        "keyword extraction failed: {keywords:?}"
    );
    words.extend(keywords.iter().cloned());
    // 抽出が空振りしていないこと（現状 350 語前後。大きく減ったら抽出規則の退行）。
    assert!(words.len() > 250, "too few matched words: {}", words.len());

    let rejected: BTreeSet<String> = UNSUPPORTED_NAMES.iter().map(|w| w.to_string()).collect();
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for list in [
        EXCLUDED_FUNCTIONS,
        EXCLUDED_UTILITY,
        EXCLUDED_VALUES,
        EXCLUDED_POLYMORPHIC_TYPES,
        OUT_OF_SCOPE_STARTS,
    ] {
        excluded.extend(list.iter().map(|w| w.to_ascii_lowercase()));
    }
    let known: BTreeSet<String> = RESERVED
        .iter()
        .chain(SQL_KEYWORD_NAMES)
        .map(|w| w.to_string())
        .collect();
    // 拒否語と除外語は排他（除外すべき語を拒否にも入れて矛盾させない）。
    let both: Vec<&String> = rejected
        .iter()
        .filter(|w| {
            excluded.contains(*w)
                && !OUT_OF_SCOPE_STARTS
                    .iter()
                    .any(|o| o.eq_ignore_ascii_case(w))
        })
        .collect();
    assert!(
        both.is_empty(),
        "words are both rejected and excluded: {both:?}"
    );
    // 字句解析の Keyword は必ず変数名にできない。
    for k in &keywords {
        assert!(
            rejected.contains(k) || known.contains(k),
            "keyword {k} is not rejected"
        );
    }
    let missing: Vec<&String> = words
        .iter()
        .filter(|w| !rejected.contains(*w) && !excluded.contains(*w) && !known.contains(*w))
        .collect();
    assert!(
        missing.is_empty(),
        "SQL words matched by the parsers but neither rejected nor excluded \
         (add to UNSUPPORTED_NAMES, or to an EXCLUDED_* list with a reason): {missing:?}"
    );
}

#[test]
fn operator_and_window_words_cannot_name_variables() {
    for w in [
        "is",
        "like",
        "ilike",
        "between",
        "filter",
        "over",
        "similar",
        "escape",
        "in",
        "and",
        "or",
        "not",
        "collate",
        "at",
        "zone",
        "partition",
        "range",
        "groups",
        "following",
        "preceding",
        "unbounded",
        "exclude",
        "ties",
    ] {
        for decl in [w.to_string(), format!("\"{w}\"")] {
            let c = code(&format!("DECLARE {decl} int; BEGIN NULL; END"));
            assert!(c == "0A000" || c == "42601", "for {decl}: {c}");
        }
    }
    for w in ["is", "like", "between", "filter", "over"] {
        assert_eq!(
            code(&format!("DECLARE \"{w}\" int := 1; BEGIN NULL; END")),
            "0A000"
        );
    }
}

#[test]
fn operators_stay_operators_when_variables_exist() {
    let b = ok("DECLARE v int; BEGIN IF 1 IS NULL THEN NULL; END IF; END");
    let Some(Stmt::If { branches, .. }) = b.block.stmts.first() else {
        panic!("if");
    };
    assert!(branches[0].0.var_refs.is_empty());
    for (src, n) in [
        ("PERFORM 'a' LIKE 'a%'", 0),
        ("PERFORM v LIKE 'a%'", 1),
        ("PERFORM v BETWEEN 1 AND 2", 1),
        ("PERFORM 1 BETWEEN v AND 2", 1),
        ("PERFORM v IS NOT NULL", 1),
        ("PERFORM v ILIKE 'x' ESCAPE '\\'", 1),
        ("PERFORM count(*) FILTER (WHERE v > 0) FROM t", 1),
        ("PERFORM sum(v) OVER (PARTITION BY x) FROM t", 1),
    ] {
        let b = ok(&format!("DECLARE v int; BEGIN {src}; END"));
        assert_eq!(first_sql_refs(&b), n, "for {src:?}");
    }
}
