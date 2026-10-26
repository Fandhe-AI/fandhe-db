//! 数値・文字列・BOOLEAN 間の `CAST` の値変換（対象ビヘイビア: SQL-47。Issue #1591。
//! 規則表は `docs/design/cast-conversion-rules.md`）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価）を
//! production 経路として、規則表の 10 型 × 10 型（int4・int8・int2・float4・float8・numeric・
//! bool・text・varchar・bpchar）の 100 セルを `<変換元>-><変換先>` のセルキーで検証する。
//! あわせて、丸め・範囲外（`22003`）・入力形式不正（`22P02`）・結果列の型・NULL 伝播・
//! テナント境界・numeric の消費位置の fail-closed・型修飾子の範囲を固定する。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

fn select_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    // 検索 SELECT は LIMIT が必須（FROM なしの SELECT は不要）。
    let sql = if sql.contains(" FROM ") && !sql.contains("LIMIT") {
        format!("{sql} LIMIT 10")
    } else {
        sql.to_string()
    };
    core.execute_sql(&ctx(tenant), &sql)
}

const CREATE: &str = "CREATE TABLE c (embedding VECTOR(2) NOT NULL, s2 SMALLINT, i4 INTEGER, \
    i8 BIGINT, r REAL, d DOUBLE PRECISION, n NUMERIC(10,3), b BOOLEAN, t TEXT, v VARCHAR(10), \
    ch CHAR(5))";

/// alice: id=1 は全型に値を持つ行、id=3 は全列 NULL の行。bob: id=2 は不正な文字列を持つ行。
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(&core, "alice", CREATE);
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, s2, i4, i8, r, d, n, b, t, v, ch) VALUES \
         (1, '[0.1,0.2]', 7, 42, 1234, 1.5, 2.5, 3.145, true, '1', '1', '1') \
         USING OPERATION_ID 'seed-alice-1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding) VALUES (3, '[0.3,0.4]') USING OPERATION_ID 'seed-alice-3'",
    );
    ok(
        &core,
        "bob",
        "INSERT INTO c (id, embedding, i4, t, v) VALUES (2, '[0.5,0.6]', 5, 'abc', 'zzz') \
         USING OPERATION_ID 'seed-bob-2'",
    );
    (core, guard)
}

fn query(core: &EngineCore, expr: &str, id: u64) -> Result<(Cell, ColumnMeta), SqlSurfaceError> {
    let sql = format!("SELECT {expr} FROM c WHERE id = {id} LIMIT 1");
    let r = select_as(core, "alice", &sql)?;
    assert_eq!(r.rows.len(), 1, "{sql}");
    Ok((r.rows[0].cells[0].clone(), r.columns[0].clone()))
}

fn first(core: &EngineCore, expr: &str) -> (Cell, ColumnMeta) {
    query(core, expr, 1).unwrap_or_else(|e| panic!("{expr} failed: {e:?}"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    match select_as(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn meta_type(meta: &ColumnMeta) -> Option<ColumnType> {
    match meta {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
        ColumnMeta::Id | ColumnMeta::AliasedId { .. } => None,
    }
}

/// セルの期待値。
#[derive(Debug, Clone)]
enum E {
    I(i64),
    F(f64),
    N(&'static str),
    B(bool),
    T(String),
}

fn check_cell(key: &str, got: &Cell, want: &E) {
    match (got, want) {
        (Cell::SignedInteger(g), E::I(w)) => assert_eq!(g, w, "cell {key}"),
        (Cell::Float(g), E::F(w)) => assert_eq!(g, w, "cell {key}"),
        (Cell::Numeric(g), E::N(w)) => assert_eq!(g.to_string(), *w, "cell {key}"),
        (Cell::Bool(g), E::B(w)) => assert_eq!(g, w, "cell {key}"),
        (Cell::Text(g), E::T(w)) => assert_eq!(g, w, "cell {key}"),
        other => panic!("cell {key}: unexpected cell/expectation pair {other:?}"),
    }
}

/// 変換元の型の値の見え方（変換先ごとの期待値を作る元）。
struct Profile {
    /// 整数への変換結果（`None` は `42846`）。
    int: Option<i64>,
    /// 浮動小数への変換結果（`None` は `42846`）。
    float: Option<f64>,
    /// numeric(12,3) への変換結果。
    numeric: Option<&'static str>,
    /// boolean への変換結果。
    boolean: Option<bool>,
    /// 文字列表現。
    text: &'static str,
}

fn pad(s: &str, n: usize) -> String {
    format!("{s:<n$}")
}

/// 規則表の 10 型。(セルキーの名前, 列式, 値の見え方, bool→int4 だけ許されるなどの個別規則)
struct Src {
    name: &'static str,
    expr: &'static str,
    profile: Profile,
    /// 整数のうち int4 への変換だけが許される（bool）。
    int4_only: bool,
}

fn sources() -> Vec<Src> {
    let ints =
        |name: &'static str, expr: &'static str, v: i64, num: &'static str, t: &'static str| Src {
            name,
            expr,
            profile: Profile {
                int: Some(v),
                float: Some(v as f64),
                numeric: Some(num),
                boolean: None, // int4 のみ個別に上書きする
                text: t,
            },
            int4_only: false,
        };
    let mut int4 = ints("int4", "i4", 42, "42.000", "42");
    int4.profile.boolean = Some(true);
    vec![
        int4,
        ints("int8", "i8", 1234, "1234.000", "1234"),
        ints("int2", "s2", 7, "7.000", "7"),
        Src {
            name: "float4",
            expr: "r",
            profile: Profile {
                int: Some(2), // 1.5 は偶数丸めで 2
                float: Some(1.5),
                numeric: Some("1.500"),
                boolean: None,
                text: "1.5",
            },
            int4_only: false,
        },
        Src {
            name: "float8",
            expr: "d",
            profile: Profile {
                int: Some(2), // 2.5 は偶数丸めで 2
                float: Some(2.5),
                numeric: Some("2.500"),
                boolean: None,
                text: "2.5",
            },
            int4_only: false,
        },
        Src {
            name: "numeric",
            expr: "n",
            profile: Profile {
                int: Some(3),
                float: Some(3.145),
                numeric: Some("3.145"),
                boolean: None,
                text: "3.145",
            },
            int4_only: false,
        },
        Src {
            name: "bool",
            expr: "b",
            profile: Profile {
                int: Some(1),
                float: None,
                numeric: None,
                boolean: Some(true),
                text: "true",
            },
            int4_only: true,
        },
        text_src("text", "t"),
        text_src("varchar", "v"),
        text_src("bpchar", "ch"),
    ]
}

fn text_src(name: &'static str, expr: &'static str) -> Src {
    Src {
        name,
        expr,
        profile: Profile {
            int: Some(1),
            float: Some(1.0),
            numeric: Some("1.000"),
            boolean: Some(true),
            text: "1",
        },
        int4_only: false,
    }
}

struct Dst {
    name: &'static str,
    sql: &'static str,
    ty: ColumnType,
}

fn destinations() -> Vec<Dst> {
    vec![
        Dst {
            name: "int4",
            sql: "integer",
            ty: ColumnType::Integer,
        },
        Dst {
            name: "int8",
            sql: "bigint",
            ty: ColumnType::BigInt,
        },
        Dst {
            name: "int2",
            sql: "smallint",
            ty: ColumnType::SmallInt,
        },
        Dst {
            name: "float4",
            sql: "real",
            ty: ColumnType::Real,
        },
        Dst {
            name: "float8",
            sql: "double precision",
            ty: ColumnType::Double,
        },
        Dst {
            name: "numeric",
            sql: "numeric(12,3)",
            ty: ColumnType::Numeric {
                precision: 12,
                scale: 3,
            },
        },
        Dst {
            name: "bool",
            sql: "boolean",
            ty: ColumnType::Boolean,
        },
        Dst {
            name: "text",
            sql: "text",
            ty: ColumnType::Text,
        },
        Dst {
            name: "varchar",
            sql: "varchar(20)",
            ty: ColumnType::Varchar(Some(20)),
        },
        Dst {
            name: "bpchar",
            sql: "char(8)",
            ty: ColumnType::Bpchar(8),
        },
    ]
}

/// 100 セルの期待値。`None` は規則表で `×`（`42846`）。
fn expected(src: &Src, dst: &Dst) -> Option<E> {
    let p = &src.profile;
    match dst.name {
        "int4" | "int8" | "int2" => {
            if src.int4_only && dst.name != "int4" {
                return None;
            }
            p.int.map(E::I)
        }
        "float4" => p.float.map(|f| E::F(f64::from(f as f32))),
        "float8" => p.float.map(E::F),
        "numeric" => p.numeric.map(E::N),
        "bool" => p.boolean.map(E::B),
        "text" | "varchar" => Some(E::T(p.text.to_string())),
        "bpchar" => Some(E::T(pad(p.text, 8))),
        other => panic!("unknown destination {other}"),
    }
}

#[test]
fn all_100_cells_of_the_numeric_text_bool_block() {
    let (core, _g) = setup("cast-cells100");
    let mut checked = 0;
    for src in sources() {
        for dst in destinations() {
            let key = format!("{}->{}", src.name, dst.name);
            let expr = format!("CAST({} AS {})", src.expr, dst.sql);
            match expected(&src, &dst) {
                None => {
                    // `×` のセルは値が NULL の行でも `42846`。
                    for id in [1, 3] {
                        let e = query(&core, &expr, id).expect_err(&key);
                        assert_eq!(e.wire_code(), "42846", "cell {key} (id {id})");
                    }
                }
                Some(want) => {
                    let (cell, meta) = query(&core, &expr, 1)
                        .unwrap_or_else(|e| panic!("cell {key}: {expr} failed: {e:?}"));
                    check_cell(&key, &cell, &want);
                    assert_eq!(
                        meta_type(&meta),
                        Some(dst.ty.clone()),
                        "cell {key} column type"
                    );
                    // NULL を入れると NULL になり、結果列の型は変わらない。
                    let (null_cell, null_meta) = query(&core, &expr, 3)
                        .unwrap_or_else(|e| panic!("cell {key} (NULL): {e:?}"));
                    assert_eq!(null_cell, Cell::Null, "cell {key} NULL propagation");
                    assert_eq!(
                        meta_type(&null_meta),
                        Some(dst.ty.clone()),
                        "cell {key} NULL type"
                    );
                }
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 100);
}

#[test]
fn numeric_typmod_identity_cells_apply_only_the_modifier() {
    let (core, _g) = setup("cast-typmod");
    // numeric(10,3) 列 -> numeric(4,1): 0 から遠い側への四捨五入
    let (cell, meta) = first(&core, "CAST(n AS numeric(4,1))");
    assert!(
        matches!(&cell, Cell::Numeric(d) if d.to_string() == "3.1"),
        "{cell:?}"
    );
    assert_eq!(
        meta_type(&meta),
        Some(ColumnType::Numeric {
            precision: 4,
            scale: 1
        })
    );
    // numeric(p,s) の桁あふれは 22003
    assert_eq!(
        code_of(&core, "SELECT CAST(n AS numeric(2,2)) FROM c WHERE id = 1"),
        "22003"
    );
    assert_eq!(
        first(&core, "CAST(v AS varchar(1))").0,
        Cell::Text("1".into())
    );
}

#[test]
fn char_modifiers_truncate_silently_and_pad() {
    let (core, _g) = setup("cast-char");
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, t, v, ch) VALUES (5, '[0.1,0.2]', 'abcdefgh', 'abcdefghij', 'abcde') \
         USING OPERATION_ID 'seed-alice-5'",
    );
    let q = |expr: &str| query(&core, expr, 5).expect(expr).0;
    assert_eq!(q("CAST(t AS varchar(3))"), Cell::Text("abc".into()));
    assert_eq!(q("CAST(v AS varchar(3))"), Cell::Text("abc".into()));
    assert_eq!(q("CAST(ch AS varchar(3))"), Cell::Text("abc".into()));
    assert_eq!(q("CAST(t AS char(2))"), Cell::Text("ab".into()));
    assert_eq!(q("CAST(ch AS char(2))"), Cell::Text("ab".into()));
    assert_eq!(q("CAST(ch AS char(7))"), Cell::Text("abcde  ".into()));
    // CHAR(n) の値を別の型へ変換するときは末尾の空白を除く（bpchar -> text）。
    assert_eq!(
        q("CAST(CAST(t AS char(12)) AS text)"),
        Cell::Text("abcdefgh".into())
    );
    // COLLATE は型を変えないため、挟んでも同じ（末尾空白を除く）。
    assert_eq!(
        q("CAST((CAST(t AS char(12)) COLLATE \"C\") AS text)"),
        Cell::Text("abcdefgh".into())
    );
}

#[test]
fn float_to_int_rounds_to_even_and_numeric_to_int_rounds_away_from_zero() {
    let (core, _g) = setup("cast-round");
    for (expr, want) in [
        ("CAST(CAST('2.5' AS double precision) AS integer)", 2),
        ("CAST(CAST('3.5' AS double precision) AS integer)", 4),
        ("CAST(CAST('-2.5' AS double precision) AS integer)", -2),
        ("CAST(CAST('2.5' AS real) AS smallint)", 2),
        ("CAST(2.5 AS integer)", 3),
        ("CAST(-2.5 AS integer)", -3),
        ("CAST(2.4 AS bigint)", 2),
        ("CAST(CAST('2.5' AS numeric) AS integer)", 3),
    ] {
        let (cell, _) = query(&core, expr, 1).unwrap_or_else(|e| panic!("{expr}: {e:?}"));
        assert_eq!(cell, Cell::SignedInteger(want), "{expr}");
    }
}

#[test]
fn range_errors_are_22003_at_the_documented_boundaries() {
    let (core, _g) = setup("cast-range");
    let sel = |expr: &str| format!("SELECT {expr} FROM c WHERE id = 1");
    // int2 / int4 の境界
    assert_eq!(
        first(&core, "CAST('32767' AS smallint)").0,
        Cell::SignedInteger(32767)
    );
    assert_eq!(code_of(&core, &sel("CAST('32768' AS smallint)")), "22003");
    assert_eq!(
        first(&core, "CAST('-32768' AS smallint)").0,
        Cell::SignedInteger(-32768)
    );
    assert_eq!(
        code_of(&core, &sel("CAST('2147483648' AS integer)")),
        "22003"
    );
    assert_eq!(
        first(&core, "CAST('-2147483648' AS integer)").0,
        Cell::SignedInteger(-2_147_483_648)
    );
    // 縮小変換
    assert_eq!(
        code_of(&core, &sel("CAST(CAST('40000' AS integer) AS smallint)")),
        "22003"
    );
    assert_eq!(
        code_of(&core, &sel("CAST(CAST('3000000000' AS bigint) AS integer)")),
        "22003"
    );
    // float -> int / numeric -> int の範囲外
    assert_eq!(
        code_of(
            &core,
            &sel("CAST(CAST('1e10' AS double precision) AS integer)")
        ),
        "22003"
    );
    assert_eq!(
        code_of(&core, &sel("CAST(12345678901.5 AS integer)")),
        "22003"
    );
    // float8 -> float4 のオーバーフロー / アンダーフロー
    assert_eq!(
        code_of(
            &core,
            &sel("CAST(CAST('1e300' AS double precision) AS real)")
        ),
        "22003"
    );
    assert_eq!(
        code_of(
            &core,
            &sel("CAST(CAST('1e-300' AS double precision) AS real)")
        ),
        "22003"
    );
    // 2^53 を超える int8 は丸めずに拒否する（既知の差分）
    assert_eq!(
        first(&core, "CAST('9007199254740992' AS bigint)").0,
        Cell::SignedInteger(9_007_199_254_740_992)
    );
    assert_eq!(
        code_of(&core, &sel("CAST('9007199254740993' AS bigint)")),
        "22003"
    );
    assert_eq!(
        code_of(&core, &sel("CAST(9007199254740993 AS bigint)")),
        "22003"
    );
    // numeric(p,s) のあふれ（丸め後の整数部で判定する）
    assert_eq!(
        code_of(&core, &sel("CAST('99.95' AS numeric(3,1))")),
        "22003"
    );
    assert!(matches!(
        first(&core, "CAST('1.25' AS numeric(3,1))").0,
        Cell::Numeric(d) if d.to_string() == "1.3"
    ));
}

#[test]
fn input_functions_share_one_grammar_between_literals_and_columns() {
    let (core, _g) = setup("cast-input");
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, t) VALUES (6, '[0.1,0.2]', ' 0x1A ') USING OPERATION_ID 'seed-alice-6'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, t) VALUES (7, '[0.1,0.2]', '1.5') USING OPERATION_ID 'seed-alice-7'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, t) VALUES (8, '[0.1,0.2]', '1_000') USING OPERATION_ID 'seed-alice-8'",
    );
    // 列経由と型なしリテラルは同じ結果になる
    for (id, lit, want) in [
        (6, " 0x1A ", Some(26)),
        (8, "1_000", Some(1000)),
        (7, "1.5", None),
    ] {
        let col = query(&core, "CAST(t AS integer)", id);
        let lit = select_as(
            &core,
            "alice",
            &format!("SELECT CAST('{lit}' AS integer) FROM c WHERE id = 1"),
        );
        match want {
            Some(v) => {
                assert_eq!(col.expect("column").0, Cell::SignedInteger(v));
                assert_eq!(
                    lit.expect("literal").rows[0].cells[0],
                    Cell::SignedInteger(v)
                );
            }
            None => {
                assert_eq!(col.expect_err("column").wire_code(), "22P02");
                assert_eq!(lit.expect_err("literal").wire_code(), "22P02");
            }
        }
    }
    // 形式不正の文言は PostgreSQL と同じ形で、入力値を 1 度だけ含む
    let e = select_as(
        &core,
        "alice",
        "SELECT CAST('abc' AS integer) FROM c WHERE id = 1",
    )
    .unwrap_err();
    assert_eq!(e.wire_code(), "22P02");
    assert!(
        e.to_string()
            .ends_with("invalid input syntax for type integer: \"abc\""),
        "{e}"
    );
    // float の文法（`+1`・`.5`・`1.`・指数）と boolean の文法
    assert_eq!(
        first(&core, "CAST('+1.5' AS double precision)").0,
        Cell::Float(1.5)
    );
    assert_eq!(
        first(&core, "CAST('.5' AS double precision)").0,
        Cell::Float(0.5)
    );
    assert_eq!(
        first(&core, "CAST('1.' AS double precision)").0,
        Cell::Float(1.0)
    );
    assert_eq!(
        first(&core, "CAST(' 1e3 ' AS double precision)").0,
        Cell::Float(1000.0)
    );
    assert_eq!(first(&core, "CAST('yes' AS boolean)").0, Cell::Bool(true));
    assert_eq!(first(&core, "CAST('off' AS boolean)").0, Cell::Bool(false));
    assert_eq!(
        code_of(&core, "SELECT CAST('maybe' AS boolean) FROM c WHERE id = 1"),
        "22P02"
    );
    assert_eq!(
        code_of(&core, "SELECT CAST('' AS integer) FROM c WHERE id = 1"),
        "22P02"
    );
    // `NaN`／`Infinity` は本エンジンが値として持たないため 0A000（22P02 にはしない）
    for lit in ["NaN", "Infinity", "-inf"] {
        for ty in ["double precision", "real", "numeric"] {
            assert_eq!(
                code_of(
                    &core,
                    &format!("SELECT CAST('{lit}' AS {ty}) FROM c WHERE id = 1")
                ),
                "0A000",
                "{lit} -> {ty}"
            );
        }
    }
    // 型付きリテラルは CAST と同じ入力関数
    let r = select_as(
        &core,
        "alice",
        "SELECT integer '12', numeric '1.50', boolean 'true', varchar(2) 'abc'",
    )
    .expect("typed literals");
    assert_eq!(r.rows[0].cells[0], Cell::SignedInteger(12));
    assert!(matches!(&r.rows[0].cells[1], Cell::Numeric(d) if d.to_string() == "1.50"));
    assert_eq!(r.rows[0].cells[2], Cell::Bool(true));
    assert_eq!(r.rows[0].cells[3], Cell::Text("ab".into()));
}

#[test]
fn float_to_numeric_uses_pg_significant_digits() {
    let (core, _g) = setup("cast-f2n");
    for (expr, want) in [
        ("CAST(d AS numeric)", "2.5"),
        ("CAST(r AS numeric)", "1.5"),
        ("CAST(CAST('0.1' AS double precision) AS numeric)", "0.1"),
        ("CAST(CAST('100' AS double precision) AS numeric)", "100"),
        ("CAST(CAST('0.1' AS real) AS numeric)", "0.1"),
    ] {
        let (cell, meta) = first(&core, expr);
        assert!(
            matches!(&cell, Cell::Numeric(d) if d.to_string() == want),
            "{expr}: {cell:?}"
        );
        assert!(
            matches!(meta_type(&meta), Some(ColumnType::Numeric { .. })),
            "{expr}"
        );
    }
    assert_eq!(
        code_of(
            &core,
            "SELECT CAST(CAST('1e300' AS double precision) AS numeric) FROM c WHERE id = 1"
        ),
        "22003"
    );
}

#[test]
fn projection_types_stay_consistent_with_plain_columns() {
    let (core, _g) = setup("cast-proj");
    let (cell, meta) = first(&core, "CAST(i4 AS smallint)");
    assert_eq!(cell, Cell::SignedInteger(42));
    assert_eq!(meta_type(&meta), Some(ColumnType::SmallInt));
    let (cell, meta) = first(&core, "CAST(d AS real)");
    assert_eq!(cell, Cell::Float(2.5));
    assert_eq!(meta_type(&meta), Some(ColumnType::Real));
    let (_, meta) = first(&core, "CAST(i4 AS numeric)");
    assert!(matches!(meta_type(&meta), Some(ColumnType::Numeric { .. })));
    let (cell, _) = first(&core, "CAST(i4 AS numeric)");
    assert!(
        matches!(&cell, Cell::Numeric(d) if d.to_string() == "42"),
        "{cell:?}"
    );
    // 値を変えない変換でも、公告する型は変換先（内側の型付き算術の型ではない）
    let (cell, meta) = first(&core, "CAST(i4 + 1 AS double precision)");
    assert_eq!(cell, Cell::Float(43.0));
    assert_eq!(meta_type(&meta), Some(ColumnType::Double));
    let (cell, meta) = first(&core, "CAST(i4 + 1 AS bigint)");
    assert_eq!(cell, Cell::SignedInteger(43));
    assert_eq!(meta_type(&meta), Some(ColumnType::BigInt));
    let r = select_as(&core, "alice", "SELECT CAST(1 + 1 AS bigint)").expect("from-less");
    assert_eq!(meta_type(&r.columns[0]), Some(ColumnType::BigInt));
    assert_eq!(r.rows[0].cells[0], Cell::SignedInteger(2));
    // WHERE の述語でも NUMERIC／BOOLEAN 列の参照が行ごとにデコードされる（マスク漏れは 0 件になる）
    for pred in ["CAST(n AS integer) = 3", "CAST(b AS integer) = 1"] {
        let r = select_as(&core, "alice", &format!("SELECT id FROM c WHERE {pred}")).expect(pred);
        assert_eq!(r.rows.len(), 1, "{pred}");
    }
    // 投影の型付き算術の結果も変換できる
    assert_eq!(
        first(&core, "CAST(i4 + 1 AS text)").0,
        Cell::Text("43".into())
    );
}

#[test]
fn numeric_values_are_only_consumed_by_projection_and_cast() {
    let (core, _g) = setup("cast-numeric-pos");
    for sql in [
        "SELECT id FROM c WHERE CAST(i4 AS numeric(5,2)) > 1",
        "SELECT id FROM c WHERE 1 < CAST(i4 AS numeric)",
        "SELECT CAST(i4 AS numeric) + 1 FROM c",
        "SELECT abs(CAST(i4 AS numeric)) FROM c",
        "SELECT id FROM c ORDER BY CAST(i4 AS numeric(5,2))",
        "SELECT COALESCE(CAST(i4 AS numeric), CAST(i8 AS numeric)) FROM c",
        "SELECT CASE WHEN i4 > 1 THEN CAST(i4 AS numeric) ELSE CAST(i8 AS numeric) END FROM c",
    ] {
        assert_eq!(code_of(&core, sql), "0A000", "{sql}");
    }
    // NUMERIC / BOOLEAN 列は CAST の被演算子以外では従来どおり式の中で拒否する
    for sql in [
        "SELECT id FROM c WHERE n + 1 > 1",
        "SELECT id FROM c WHERE abs(n) > 1",
    ] {
        let e = select_as(&core, "alice", sql).unwrap_err();
        assert_eq!(e.wire_code(), "22000", "{sql}");
    }
    // CAST の入れ子は numeric を経由して変換できる
    assert_eq!(
        first(&core, "CAST(CAST(i4 AS numeric(5,2)) AS text)").0,
        Cell::Text("42.00".into())
    );
}

#[test]
fn unknown_numeric_sources_stay_unimplemented() {
    let (core, _g) = setup("cast-unknown-numeric");
    for expr in [
        "CAST(abs(i4) AS integer)",
        "CAST(abs(i4) AS text)",
        "CAST(id AS bigint)",
        "CAST(id AS text)",
    ] {
        let sql = format!("SELECT {expr} FROM c WHERE id = 1");
        assert_eq!(code_of(&core, &sql), "0A000", "{expr}");
    }
}

#[test]
fn numeric_type_modifier_ranges() {
    let (core, _g) = setup("cast-numeric-range");
    for ty in [
        "numeric(0,0)",
        "numeric(0)",
        "numeric(1001,0)",
        "numeric(5000,1)",
        "numeric(5,-1001)",
    ] {
        assert_eq!(
            code_of(
                &core,
                &format!("SELECT CAST(i4 AS {ty}) FROM c WHERE id = 1")
            ),
            "22023",
            "{ty}"
        );
    }
    for ty in [
        "numeric(50,2)",
        "numeric(3,5)",
        "numeric(39)",
        "numeric(5,-1)",
        "numeric(5,-1000)",
    ] {
        assert_eq!(
            code_of(
                &core,
                &format!("SELECT CAST(i4 AS {ty}) FROM c WHERE id = 1")
            ),
            "0A000",
            "{ty}"
        );
    }
    // 修飾子なしと numeric(p) は CAST の型名に限って受理する
    for ty in ["numeric", "decimal", "numeric(5)"] {
        assert!(
            select_as(
                &core,
                "alice",
                &format!("SELECT CAST(i4 AS {ty}) FROM c WHERE id = 1")
            )
            .is_ok(),
            "{ty}"
        );
    }
    // DDL の型名は従来どおり修飾子なしの numeric を受け付けない
    let e = run(
        &core,
        "alice",
        "CREATE TABLE bare (embedding VECTOR(2) NOT NULL, a NUMERIC)",
    )
    .unwrap_err();
    assert_eq!(e.wire_code(), "42601");
}

#[test]
fn tenant_boundary_holds_for_value_conversions() {
    let (core, _g) = setup("cast-tenant");
    // bob の行には変換できない文字列 'abc' がある。alice の可視行だけが評価される。
    let r = select_as(
        &core,
        "alice",
        "SELECT id FROM c WHERE CAST(t AS integer) = 1 LIMIT 10",
    )
    .expect("invisible rows must not be evaluated");
    assert_eq!(r.rows.len(), 1);
    // bob からは自分の行の値で失敗し、エラー文に alice の値は出ない。
    let e = select_as(&core, "bob", "SELECT CAST(t AS integer) FROM c LIMIT 10").unwrap_err();
    assert_eq!(e.wire_code(), "22P02");
    assert!(e.to_string().contains("abc"), "{e}");
    assert!(!e.to_string().contains("alice"), "{e}");
    // alice が失敗しても bob の値（'zzz'・'abc'）は出ない
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, t) VALUES (9, '[0.1,0.2]', 'mine') USING OPERATION_ID 'seed-alice-9'",
    );
    let e = select_as(&core, "alice", "SELECT CAST(t AS integer) FROM c LIMIT 10").unwrap_err();
    assert_eq!(e.wire_code(), "22P02");
    let msg = e.to_string();
    assert!(msg.contains("mine"), "{msg}");
    assert!(!msg.contains("abc") && !msg.contains("zzz"), "{msg}");
}

#[test]
fn value_conversions_never_reach_all_tenant_validation_paths() {
    let (core, _g) = setup("cast-ddl-paths");
    for sql in [
        "CREATE TABLE ck (embedding VECTOR(2) NOT NULL, a INTEGER, CHECK (CAST(a AS bigint) > 0))",
        "CREATE TABLE ck2 (embedding VECTOR(2) NOT NULL, a TEXT, CHECK (CAST(a AS integer) > 0))",
        "CREATE TABLE df (embedding VECTOR(2) NOT NULL, a INTEGER DEFAULT CAST('1' AS integer))",
    ] {
        let e = run(&core, "alice", sql).unwrap_err();
        assert_eq!(e.wire_code(), "0A000", "{sql}");
    }
    let e = run(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, i4) VALUES (11, '[0.1,0.2]', CAST('1' AS integer))",
    )
    .unwrap_err();
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn numeric_cast_in_aggregate_select_never_fails_internally() {
    // 集計 SELECT（GROUP BY あり・なし）で numeric 変換値を射影しようとしても、内部エラー
    // （XX000）ではなく構文・機能未対応として fail-closed に拒否される。
    let (core, _g) = setup("cast-numeric-agg");
    for sql in [
        "SELECT CAST(i4 AS numeric) FROM c GROUP BY i4",
        "SELECT CAST(i4 AS numeric), count(*) FROM c GROUP BY i4",
        "SELECT count(*) + CAST(1 AS numeric) FROM c",
    ] {
        let e = select_as(&core, "alice", sql).unwrap_err();
        assert_ne!(e.wire_code(), "XX000", "{sql}: {e:?}");
    }
    // Issue #1812: bigint と numeric の集計式項目は numeric 算術として受理される（PostgreSQL と同じ）。
    select_as(
        &core,
        "alice",
        "SELECT count(*) + CAST(i4 AS numeric) FROM c GROUP BY i4",
    )
    .expect("bigint + numeric is numeric arithmetic");
}
