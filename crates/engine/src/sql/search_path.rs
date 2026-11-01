//! セッション変数 `search_path`（未修飾名の解決順と作成先 SCHEMA の決定）。TABLE-27・Issue #1715。
//! 設計判断は ADR `docs/design/schema-tenant-namespace.md` の D5・D6・D11 と §10 のポインタ。
//!
//! # 責務境界
//!
//! - 値の型と構文検証（[`SearchPath`]・要素数と要素長の上限・`$user` と組み込み SCHEMA の正規化）
//! - `SHOW search_path` の表示（PostgreSQL の `quote_identifier` 相当）
//! - 未修飾のテーブル参照が `public` を見失うセッションでの参照ゲート（[`check_unqualified_refs`]）
//! - 作成系 DDL の作成先の判定（[`creation_target`]）
//!
//! 呼び出し元: `sql::allowlist` の `SET`／`RESET`／`SHOW` 構文解析（値の検証）、`core::EngineCore` の
//! 実行腕（セッションへの代入・表示）、`core::EngineCore::parse_tokens_gated` ほか文字列入口
//! （参照ゲート）、永続オブジェクトを作る各実行腕（作成先）、`wire-server` の handshake
//! （[`crate::sql::mode::SessionState::set_session_user`]）。
//!
//! # 現時点で実際に変わる挙動
//!
//! テナント SCHEMA の中にはまだオブジェクトを作れない（#1717）ため、未修飾名の解決結果は
//! 「実効 path に `public` が含まれるか」だけで決まる。含まれなければ一時表以外の未修飾名は
//! `42P01`、作成先が `public` 以外になる文は `3F000`／`0A000`。実効 path に `public` が含まれる
//! 既定のセッションは一切コストを払わず、従来の挙動とバイト列が一致する。
//!
//! # セキュリティ
//!
//! - 要素数・要素長はアロケーション前に上限検査する（`22023`）。入力値はエラーへ反射しない。
//! - 参照ゲートはカタログを引かない（存在オラクルにならない）。作成先の SCHEMA 照会は
//!   `(呼び出し元テナント, 名前)` の 1 回の get だけで、他テナントの SCHEMA は存在しない名前と
//!   同じに扱う。
//! - `$user` は handshake が認証成功後に確定した username だけから展開する（SQL から変える経路なし）。

use crate::catalog::namespace::is_builtin_schema_name;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;

/// `search_path` の要素数の上限（確保前に検査する。untrusted 入力）。
pub const MAX_SEARCH_PATH_ELEMENTS: usize = 64;

/// 要素 1 件の長さの上限（バイト。PostgreSQL の NAMEDATALEN-1）。PostgreSQL は超過分を切り詰めるが、
/// 本実装は拒否する（意図的な差。ADR §10）。
pub const MAX_SEARCH_PATH_ELEMENT_BYTES: usize = 63;

/// 不正な `search_path` 値の固定文言（`22023`。入力を反射しない）。
pub(crate) const INVALID_SEARCH_PATH_DETAIL: &str = "invalid value for parameter \"search_path\"";

/// 既定値と SHOW が使う `public` SCHEMA 名。
const PUBLIC: &str = "public";

/// `search_path` の要素 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchPathElem {
    /// `"$user"`（接続した主体の username と同名の SCHEMA。未設定のセッションでは飛ばす）。
    User,
    /// SCHEMA 名。`public`・`pg_catalog`・`information_schema`・`pg_temp` は小文字に正規化済み、
    /// それ以外は利用者が書いたとおり。
    Name(String),
}

/// `search_path` の値（要素の並び）。既定は `"$user", public`（ADR D5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPath {
    elems: Vec<SearchPathElem>,
}

impl Default for SearchPath {
    fn default() -> Self {
        Self {
            elems: vec![
                SearchPathElem::User,
                SearchPathElem::Name(PUBLIC.to_string()),
            ],
        }
    }
}

/// `SET search_path` の右辺（構文解析の結果）。`RESET`・`TO DEFAULT` は [`Self::Default`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchPathSetting {
    /// 既定値（`"$user", public`）へ戻す。
    Default,
    /// 要素の並び（[`SearchPath::new`] で検証してからセッションへ代入する）。
    List(Vec<SearchPathElem>),
}

fn invalid_value() -> SqlSurfaceError {
    SqlSurfaceError::InvalidParameterValue {
        detail: INVALID_SEARCH_PATH_DETAIL,
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`（文字列リテラル要素の中身に許す形）。
fn is_plain_name(s: &str) -> bool {
    let mut it = s.chars();
    let Some(first) = it.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 特別扱いする SCHEMA 名（`public` と組み込み）を ASCII 大小無視で照合し小文字へ正規化する。
fn normalize_name(name: &str) -> String {
    for special in [PUBLIC, "pg_catalog", "information_schema", "pg_temp"] {
        if name.eq_ignore_ascii_case(special) {
            return special.to_string();
        }
    }
    name.to_string()
}

impl SearchPathElem {
    /// 識別子トークン由来の要素（長さを検査して正規化する）。
    pub(crate) fn from_ident(name: &str) -> Result<Self, SqlSurfaceError> {
        if name.is_empty() || name.len() > MAX_SEARCH_PATH_ELEMENT_BYTES {
            return Err(invalid_value());
        }
        Ok(Self::Name(normalize_name(name)))
    }

    /// 文字列リテラル由来の要素。中身が plain identifier か `$user` のときだけ受理する
    /// （空文字・カンマ入りなどは `22023`。PostgreSQL のようにカンマ分割はしない）。
    pub(crate) fn from_literal(text: &str) -> Result<Self, SqlSurfaceError> {
        if text == "$user" {
            return Ok(Self::User);
        }
        if !is_plain_name(text) {
            return Err(invalid_value());
        }
        Self::from_ident(text)
    }
}

impl SearchPath {
    /// 要素の並びを検証して値にする。要素数の上限超過は `22023`。`pg_temp` を `public` より後ろに
    /// 置く指定は `0A000`（一時表は常に先頭で探すため、`public` を優先する指定を黙って無視しない。
    /// fail-closed）。他テナントの SCHEMA 名・存在しない名前も受理して保持する（ADR D5）。
    #[cfg(test)]
    pub(crate) fn new(elems: Vec<SearchPathElem>) -> Result<Self, SqlSurfaceError> {
        Self::new_for_user(elems, None)
    }

    /// [`Self::new`] に `session_user` を加えた版。`$user` は `session_user` が ASCII 大小無視で
    /// `public` のとき `public` として展開し、`pg_temp` との順序検査に含める（`"$user", pg_temp` が
    /// `public, pg_temp` に化ける指定を拒否する）。
    pub(crate) fn new_for_user(
        elems: Vec<SearchPathElem>,
        session_user: Option<&str>,
    ) -> Result<Self, SqlSurfaceError> {
        if elems.is_empty() || elems.len() > MAX_SEARCH_PATH_ELEMENTS {
            return Err(invalid_value());
        }
        let mut seen_public = false;
        for e in &elems {
            match e {
                SearchPathElem::Name(n) if n == PUBLIC => seen_public = true,
                SearchPathElem::User
                    if session_user.is_some_and(|u| u.eq_ignore_ascii_case(PUBLIC)) =>
                {
                    seen_public = true
                }
                SearchPathElem::Name(n) if n == "pg_temp" && seen_public => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "pg_temp after public in search_path is not supported",
                    ));
                }
                _ => {}
            }
        }
        Ok(Self { elems })
    }

    /// 要素の並び。
    pub fn elems(&self) -> &[SearchPathElem] {
        &self.elems
    }

    /// `SHOW search_path` の表示（要素を `, ` で連結。PostgreSQL の `quote_identifier` 相当）。
    pub fn display(&self) -> String {
        let mut out = String::new();
        for (i, e) in self.elems.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            match e {
                SearchPathElem::User => out.push_str("\"$user\""),
                SearchPathElem::Name(n) => out.push_str(&quote_identifier(n)),
            }
        }
        out
    }

    /// 実効 path に `public` が含まれないか。`$user` は `session_user` が ASCII 大小無視で `public` の
    /// ときだけ public とみなす。真のセッションだけが参照ゲート・作成先判定の対象になる。
    pub(crate) fn hides_public(&self, session_user: Option<&str>) -> bool {
        !self.elems.iter().any(|e| match e {
            SearchPathElem::Name(n) => n == PUBLIC,
            SearchPathElem::User => session_user.is_some_and(|u| u.eq_ignore_ascii_case(PUBLIC)),
        })
    }
}

/// 識別子の表示用引用。`^[a-z_][a-z0-9_]*$` に合い、かつ予約語でない名前だけ素のまま出す。
fn quote_identifier(name: &str) -> String {
    let safe_chars = {
        let mut it = name.chars();
        it.next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && it.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    let plain_token = matches!(
        crate::sql::lexer::tokenize(name).as_deref(),
        Ok([Token::Ident(n)]) if n == name
    );
    if safe_chars && plain_token {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// ゲート対象の未修飾名 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateRef {
    name: String,
    /// `DROP INDEX`／`ALTER INDEX` の対象名か（一時表の索引は一時 DB の索引カタログで照会する）。
    is_index: bool,
}

/// 添字 `idx` の識別子が、表名ではなく構文語として置かれているか。名前の一致だけでは判定せず、
/// 構文位置（直後のトークン・直前の句）で区別する。`"temp"` のように構文語と同名の実テーブルは
/// 参照として残す（fail-closed。除外は構文語だと確定できる形だけ）。
fn is_syntax_keyword_here(tokens: &[Token], idx: usize) -> bool {
    let word = |i: usize| match tokens.get(i) {
        Some(Token::Ident(w)) => Some(w.as_str()),
        _ => None,
    };
    let is =
        |i: Option<usize>, k: &str| i.and_then(word).is_some_and(|w| w.eq_ignore_ascii_case(k));
    let prev = idx.checked_sub(1);
    // `FOR UPDATE <NOWAIT|OF|SKIP>`・`DO UPDATE SET`・`ON UPDATE <CASCADE|SET|NO ..>` は句の続き。
    if is(prev, "UPDATE")
        && (is(idx.checked_sub(2), "FOR")
            || is(idx.checked_sub(2), "DO")
            || is(idx.checked_sub(2), "ON"))
    {
        return true;
    }
    let next_is_name = matches!(
        tokens.get(idx.saturating_add(1)),
        Some(t) if crate::sql::relation_name::is_ident_like(t)
    );
    let Some(w) = word(idx) else {
        return false;
    };
    if w.eq_ignore_ascii_case("IF") {
        return is(Some(idx.saturating_add(1)), "EXISTS") || is(Some(idx.saturating_add(1)), "NOT");
    }
    if w.eq_ignore_ascii_case("ONLY") {
        return next_is_name;
    }
    if w.eq_ignore_ascii_case("TABLE") {
        // `LOCK TABLE <表>` の `TABLE` も構文語（Issue #1958。文頭の `LOCK` の直後だけ）。
        return (is(prev, "TRUNCATE") || (is(prev, "LOCK") && idx == 1)) && next_is_name;
    }
    if w.eq_ignore_ascii_case("LATERAL") {
        let after = tokens.get(idx.saturating_add(1));
        return matches!(after, Some(Token::Punct('(')))
            || (next_is_name
                && matches!(tokens.get(idx.saturating_add(2)), Some(Token::Punct('('))));
    }
    false
}

/// 未修飾のテーブル参照の名前集め（参照ゲートの入力）。`skip` はゲート対象外のトークン添字
/// （準備済み文で `public.` 修飾を外した位置）。トークン列は書き換えない。
fn unqualified_relation_refs(tokens: &[Token], skip: &[usize]) -> Vec<GateRef> {
    let walk = crate::sql::relation_name::relation_positions(tokens);
    let ctes = crate::sql::relation_name::collect_cte_scopes(tokens);
    // `DROP`／`ALTER`／`REFRESH` のオブジェクト名は `object_ddl_names` が（索引かどうかの区別つきで）
    // 数えるため、位置走査の側では二重に数えない。
    let object_ddl = object_ddl_names(tokens);
    let mut out = Vec::new();
    for pos in walk.positions {
        if pos.create_name
            || skip.contains(&pos.index)
            || object_ddl.iter().any(|(i, _)| *i == pos.index)
        {
            continue;
        }
        let Some(name) = tokens
            .get(pos.index)
            .and_then(crate::sql::relation_name::unqualified_ident_name)
        else {
            continue;
        };
        // 名前解決器が `schema.name` の修飾を解決して置き換えた内部キー（`#<id>.<name>`）は修飾済みの
        // 参照なので、`search_path` による未修飾名の判定対象にしない（キーを作れるのは解決器だけ。
        // 字句解析器は `#` を含む識別子を作らない。Issue #1717）。
        if name.starts_with('#') {
            continue;
        }
        // 構文語（`IF`・`ONLY`・`LATERAL` 等）は構文位置で判定する。CTE 名はその参照位置から実際に
        // 可視なものだけを除外する（非再帰 CTE は自身の本文から見えない。`sql::cte` の契約）。
        if is_syntax_keyword_here(tokens, pos.index)
            || ctes
                .iter()
                .any(|c| c.covers(pos.index) && c.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
        // 表関数（`FROM generate_series(..)`・`JOIN f(..)`・`, f(..)`）は関係ではない。`INSERT INTO t (..)`・
        // `REFERENCES t (..)`・`ON t (..)` の `(` は列リストなので除外しない。
        let prev = pos.index.checked_sub(1).and_then(|i| tokens.get(i));
        let table_function_slot = matches!(
            prev,
            Some(Token::Keyword(crate::sql::lexer::Keyword::From) | Token::Punct(','))
        ) || matches!(prev, Some(Token::Ident(w)) if w.eq_ignore_ascii_case("JOIN"));
        if table_function_slot
            && matches!(
                tokens.get(pos.index.saturating_add(1)),
                Some(Token::Punct('('))
            )
        {
            continue;
        }
        out.push(GateRef {
            name: name.to_string(),
            is_index: false,
        });
    }
    // `DROP`／`ALTER`／`REFRESH` の対象名にも位置走査と同じ除外を掛ける。Bind・Describe は書き換え後の
    // トークンでゲートを再実行するため、`public.` 修飾を外した位置（`skip`）と解決済みの内部キー
    // （`#<id>.<name>`）は修飾済みの参照であり、未修飾名として数えない。
    out.extend(
        object_ddl
            .into_iter()
            .filter(|(i, r)| !skip.contains(i) && !r.name.starts_with('#'))
            .map(|(_, r)| r),
    );
    out
}

/// `DROP`／`ALTER`／`REFRESH` の `[MATERIALIZED] VIEW`・`SEQUENCE`・`INDEX`・`TYPE` の対象名。
/// `public` が見えないセッションが共有 `public` のビューなどを取り違えて落とさないよう、
/// 参照として扱う（fail-closed。`IF EXISTS` でも拒否する。ADR §10 の意図的な差）。
fn object_ddl_names(tokens: &[Token]) -> Vec<(usize, GateRef)> {
    let word = |i: usize| match tokens.get(i) {
        Some(Token::Ident(w)) => Some(w.as_str()),
        _ => None,
    };
    let is = |i: usize, k: &str| word(i).is_some_and(|w| w.eq_ignore_ascii_case(k));
    if !(is(0, "DROP") || is(0, "ALTER") || is(0, "REFRESH")) {
        return Vec::new();
    }
    let mut i = 1;
    if is(i, "MATERIALIZED") {
        i += 1;
    }
    if !(is(i, "VIEW") || is(i, "SEQUENCE") || is(i, "INDEX") || is(i, "TYPE")) {
        return Vec::new();
    }
    let is_index = is(i, "INDEX");
    i += 1;
    if is(i, "CONCURRENTLY") {
        i += 1;
    }
    if is(i, "IF") && is(i + 1, "EXISTS") {
        i += 2;
    }
    let mut out = Vec::new();
    while let Some(name) = tokens
        .get(i)
        .and_then(crate::sql::relation_name::unqualified_ident_name)
    {
        out.push((
            i,
            GateRef {
                name: name.to_string(),
                is_index,
            },
        ));
        if matches!(tokens.get(i + 1), Some(Token::Punct(','))) {
            i += 2;
        } else {
            break;
        }
    }
    out
}

/// 文が DDL 実行権限ゲート（`42501`）の対象になりうる先頭か。権限の無いセッションが存在判定より
/// 前に `42501` を受け取る契約を保つため、参照ゲートはこの形を飛ばす。
fn is_ddl_head(tokens: &[Token]) -> bool {
    matches!(
        tokens.first(),
        Some(Token::Ident(w)) if ["CREATE", "DROP", "ALTER", "REFRESH"]
            .iter()
            .any(|k| w.eq_ignore_ascii_case(k))
    )
}

/// 文が `GRANT`／`REVOKE` か（[`check_unqualified_refs`] が現在ロール未束縛のセッションで飛ばす）。
fn is_grant_head(tokens: &[Token]) -> bool {
    matches!(
        tokens.first(),
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("GRANT") || w.eq_ignore_ascii_case("REVOKE")
    )
}

/// `public` が見えない `search_path` のセッションで、未修飾のテーブル参照が一時表以外なら `42P01` に
/// する参照ゲート。実効 path に `public` が含まれる（既定の）セッションは何もせず、トークン走査もしない。
///
/// 現時点でテナント SCHEMA にオブジェクトは作れない（#1717）ため、`public` 以外の path 要素は
/// 参照を解決できない。カタログを引かないので存在オラクルにならない。エラーは利用者が書いた名前だけを
/// 含む。`skip` は準備済み文で `public.` 修飾を外した位置。
pub(crate) fn check_unqualified_refs(
    session: &SessionState,
    tokens: &[Token],
    skip: &[usize],
) -> Result<(), SqlSurfaceError> {
    if !session.search_path_hides_public() {
        return Ok(());
    }
    if is_ddl_head(tokens) && crate::sql::ddl::require_ddl_permission(session).is_err() {
        return Ok(());
    }
    // `GRANT`／`REVOKE`（SQL-40・Issue #1965）は現在ロールが束縛されていないセッションが `42501` を受け取る
    // 契約（`sql::grant_ddl::require_grant_principal`。カタログ照会・名前解決より前）を保つため、
    // その場合は参照ゲートを飛ばす。束縛済みのセッションでは未修飾の表名は他の文と同じく `42P01` になる
    // （カタログを引かない判定のため存在オラクルにならない）。
    if is_grant_head(tokens) && crate::sql::privilege_gate::bound_principal(session).is_err() {
        return Ok(());
    }
    // `SET`／`RESET`／`SHOW` は関係を参照しない。設定値（例 `truncate`）を文種と誤解釈して
    // 関係名として走査すると、同じ設定が現在の path 次第で拒否されるため走査対象から除く。
    if matches!(
        tokens.first(),
        Some(Token::Ident(w)) if ["SET", "RESET", "SHOW"].iter().any(|k| w.eq_ignore_ascii_case(k))
    ) {
        return Ok(());
    }
    let temp = session.temp_tables();
    for r in unqualified_relation_refs(tokens, skip) {
        // 索引名は一時表名の集合ではなく一時 DB の索引カタログで照会する（一時表の索引に対する
        // `DROP INDEX` を `42P01` で先に拒否しない）。照会に失敗したら存在しない側（fail-closed）。
        let temp_hit = if r.is_index {
            temp_index_exists(session, &r.name)
        } else {
            temp.contains(&r.name)
        };
        if temp_hit {
            continue;
        }
        return Err(SqlSurfaceError::undefined_table(r.name));
    }
    Ok(())
}

/// セッションの一時 DB に同名の索引があるか（未生成・照会失敗は `false`）。
fn temp_index_exists(session: &SessionState, name: &str) -> bool {
    session
        .temp_rows_arc()
        .and_then(|store| store.core().ok().map(|c| c.has_index_named(name)))
        .unwrap_or(false)
}

/// 作成系 DDL の作成先（現時点では `public` だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateTarget {
    /// 共有の `public`（従来どおりの作成）。
    Public,
}

/// 作成先を `search_path` の先頭から判定する。`probe` は呼び出し元テナントの SCHEMA 存在照会。
///
/// - `public`: 従来どおり作成する。
/// - `pg_catalog`／`information_schema`／`pg_temp`: `0A000`（`pg_temp` は PostgreSQL では一時表になるため、
///   永続側へ黙って作らない）。
/// - 自テナントの既存 SCHEMA: `0A000`（テナント SCHEMA への作成は #1717）。
/// - 存在しない名前（他テナントの SCHEMA を含む）・`$user` が未設定: 次の要素へ。
/// - どれも無ければ `3F000`（固定文言）。
///
/// 既定の path では `public` へ到達する前に自 username と同名の SCHEMA が見つかれば `0A000` になる
/// （PostgreSQL と同じく `$user` が優先。#1717 まで。ADR D5）。
pub(crate) fn creation_target(
    session: &SessionState,
    probe: &dyn Fn(&str) -> Result<bool, SqlSurfaceError>,
) -> Result<CreateTarget, SqlSurfaceError> {
    for elem in session.search_path().elems() {
        let Some(name) = expand_elem(session, elem) else {
            continue;
        };
        if name == PUBLIC {
            return Ok(CreateTarget::Public);
        }
        if is_builtin_schema_name(&name) || name == "pg_temp" {
            return Err(SqlSurfaceError::feature_not_supported(
                "creating relations in this schema is not supported",
            ));
        }
        if probe(&name)? {
            return Err(SqlSurfaceError::feature_not_supported(
                "creating relations inside a schema is not supported",
            ));
        }
    }
    Err(SqlSurfaceError::NoSchemaSelected)
}

/// path の要素 1 件を正規化した SCHEMA 名へ展開する。`$user` は handshake が確定した username
/// だけから展開し（D6）、未設定なら `None`（飛ばす）。[`creation_target`] と
/// [`resolve_schema_functions`] が共有し、未修飾の作成先と `current_schema()` が別の展開規則で
/// ずれないようにする。
fn expand_elem(session: &SessionState, elem: &SearchPathElem) -> Option<String> {
    match elem {
        SearchPathElem::User => session.session_user().map(normalize_name),
        SearchPathElem::Name(n) => Some(n.clone()),
    }
}

/// `current_schema()`・`current_schemas()` の元になる、セッションのテナントで解決できる SCHEMA の
/// 並び（TABLE-27 (7)・Issue #1718）。内部キー・内部 ID は持たない（利用者が書いた名前を正規化した
/// ものだけ。D2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaFnValues {
    /// 実在する path 要素（重複除去済み。path の順）。
    explicit: Vec<String>,
    /// セッションに一時表があるか（暗黙の一時スキーマを `current_schemas(true)` に含める条件）。
    temp_visible: bool,
}

impl SchemaFnValues {
    /// `current_schema()` の値（明示リストの先頭。無ければ `None`）。
    pub(crate) fn current(&self) -> Option<&str> {
        self.explicit.first().map(String::as_str)
    }

    /// `current_schemas(include_implicit)` の値。`true` は暗黙の要素を PostgreSQL と同じ規則で前置する。
    /// 暗黙の一時スキーマ（一時表があるとき）と `pg_catalog` は、明示リストに無い場合だけ
    /// 一時 → `pg_catalog` の順で先頭へ置き、明示されている場合は利用者が書いた位置を保持する。
    pub(crate) fn schemas(&self, include_implicit: bool) -> Vec<String> {
        let mut out = Vec::with_capacity(self.explicit.len().saturating_add(2));
        if include_implicit {
            let has = |n: &str| self.explicit.iter().any(|e| e == n);
            // 明示リストの `pg_temp` は一時表があるときだけ残る（resolve 側で判定済み）ため、
            // `has("pg_temp")` が真なら暗黙前置は不要（重複させず明示位置を保つ）。
            if self.temp_visible && !has("pg_temp") {
                out.push("pg_temp".to_string());
            }
            if !has("pg_catalog") {
                out.push("pg_catalog".to_string());
            }
        }
        out.extend(self.explicit.iter().cloned());
        out
    }
}

/// path を先頭から見て、セッションのテナントで実在する SCHEMA を集める。`probe` は
/// 呼び出し元テナントの SCHEMA 存在照会（`(テナント, 名前)` の 1 回の get。他テナントの SCHEMA は
/// 不在と同じく飛ばす。D4）。照会回数は path の要素数（上限 [`MAX_SEARCH_PATH_ELEMENTS`]）以下。
pub(crate) fn resolve_schema_functions(
    session: &SessionState,
    probe: &dyn Fn(&str) -> Result<bool, SqlSurfaceError>,
) -> Result<SchemaFnValues, SqlSurfaceError> {
    let temp_visible = !session.temp_tables().is_empty();
    let mut explicit: Vec<String> = Vec::new();
    for elem in session.search_path().elems() {
        let Some(name) = expand_elem(session, elem) else {
            continue;
        };
        if explicit.contains(&name) {
            continue;
        }
        let exists = match name.as_str() {
            PUBLIC | "pg_catalog" | "information_schema" => true,
            "pg_temp" => temp_visible,
            _ => probe(&name)?,
        };
        if exists {
            explicit.push(name);
        }
    }
    Ok(SchemaFnValues {
        explicit,
        temp_visible,
    })
}

/// `current_schema` 系の束縛へ値を渡すモード。
#[derive(Debug, Clone)]
pub(crate) enum SchemaFnMode {
    /// 実行用（probe 済みの値）。
    Values(SchemaFnValues),
    /// Describe 用（型だけ公告する。カタログを引かない）。
    TypesOnly,
}

thread_local! {
    static SCHEMA_FN: std::cell::RefCell<Option<SchemaFnMode>> =
        const { std::cell::RefCell::new(None) };
}

/// `current_schema` 系の値を現スレッドの束縛へ渡す RAII ガード。`core.rs` の FROM なし定数
/// SELECT の 2 つの腕（実行・Describe）だけが張る。スコープの無い文脈（FROM あり SELECT・
/// INSERT・DEFAULT・CHECK・索引・ビュー本文等）では束縛が `0A000` で拒否する（fail-closed）。
/// drop で直前の値へ戻す（panic でも他文脈へ漏れない）。
pub(crate) struct SchemaFnScope {
    prev: Option<SchemaFnMode>,
}

impl SchemaFnScope {
    pub(crate) fn enter(mode: SchemaFnMode) -> Self {
        let prev = SCHEMA_FN.with(|c| c.replace(Some(mode)));
        SchemaFnScope { prev }
    }
}

impl Drop for SchemaFnScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SCHEMA_FN.with(|c| c.replace(prev));
    }
}

/// 現スレッドのスコープのモード（無ければ `None`）。
pub(crate) fn current_schema_fn_mode() -> Option<SchemaFnMode> {
    SCHEMA_FN.with(|c| c.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn names(sql: &str) -> Vec<String> {
        unqualified_relation_refs(&tokenize(sql).expect("lex"), &[])
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn default_value_displays_as_user_then_public() {
        assert_eq!(SearchPath::default().display(), "\"$user\", public");
    }

    #[test]
    fn elements_are_normalized_and_quoted_for_display() {
        let p = SearchPath::new(vec![
            SearchPathElem::from_ident("PUBLIC").unwrap(),
            SearchPathElem::from_ident("MySchema").unwrap(),
            SearchPathElem::from_literal("a_b").unwrap(),
            SearchPathElem::from_literal("$user").unwrap(),
        ])
        .unwrap();
        assert_eq!(p.display(), "public, \"MySchema\", a_b, \"$user\"");
    }

    #[test]
    fn literal_elements_must_be_plain_names() {
        for bad in ["", "a,b", "a b", "1x", "a-b"] {
            assert!(SearchPathElem::from_literal(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn limits_are_enforced_before_allocation() {
        let long = "x".repeat(MAX_SEARCH_PATH_ELEMENT_BYTES + 1);
        assert!(SearchPathElem::from_ident(&long).is_err());
        assert!(SearchPathElem::from_ident(&"x".repeat(MAX_SEARCH_PATH_ELEMENT_BYTES)).is_ok());
        let many = vec![SearchPathElem::Name("a".to_string()); MAX_SEARCH_PATH_ELEMENTS + 1];
        assert!(SearchPath::new(many).is_err());
        assert!(SearchPath::new(Vec::new()).is_err());
    }

    #[test]
    fn pg_temp_after_public_is_rejected_but_before_is_accepted() {
        let temp = SearchPathElem::from_ident("pg_temp").unwrap();
        let public = SearchPathElem::from_ident("public").unwrap();
        assert!(SearchPath::new(vec![temp.clone(), public.clone()]).is_ok());
        assert!(SearchPath::new(vec![public, temp]).is_err());
    }

    #[test]
    fn pg_temp_after_expanded_user_public_is_rejected() {
        let temp = SearchPathElem::from_ident("pg_temp").unwrap();
        let elems = vec![SearchPathElem::User, temp];
        assert!(SearchPath::new_for_user(elems.clone(), Some("public")).is_err());
        assert!(SearchPath::new_for_user(elems.clone(), Some("alice")).is_ok());
        assert!(SearchPath::new_for_user(elems, None).is_ok());
    }

    #[test]
    fn hides_public_depends_on_user_expansion() {
        let p = SearchPath::new(vec![SearchPathElem::User]).unwrap();
        assert!(p.hides_public(None));
        assert!(p.hides_public(Some("alice")));
        assert!(!p.hides_public(Some("Public")));
        assert!(!SearchPath::default().hides_public(None));
    }

    #[test]
    fn refs_cover_relation_positions_and_skip_syntax_words_and_functions() {
        assert_eq!(names("SELECT * FROM t"), vec!["t"]);
        assert_eq!(
            names("SELECT * FROM a JOIN b ON a.id = b.id"),
            vec!["a", "b"]
        );
        assert_eq!(names("DROP TABLE IF EXISTS t"), vec!["t"]);
        assert_eq!(names("INSERT INTO t (a) VALUES (1)"), vec!["t"]);
        assert_eq!(
            names("SELECT * FROM generate_series(1, 3)"),
            Vec::<String>::new()
        );
        assert_eq!(names("SELECT 1"), Vec::<String>::new());
        assert_eq!(
            names("WITH c AS (SELECT * FROM t) SELECT * FROM c"),
            vec!["t"]
        );
        assert_eq!(names("CREATE TABLE u (id INT)"), Vec::<String>::new());
        assert_eq!(names("DROP VIEW IF EXISTS v"), vec!["v"]);
        assert_eq!(names("REFRESH MATERIALIZED VIEW m"), vec!["m"]);
    }

    #[test]
    fn cte_name_hides_only_where_visible() {
        // 非再帰 CTE は自身の本文から見えない（同名の実リレーションを読む）。
        assert_eq!(
            names("WITH t AS (SELECT id FROM t) SELECT id FROM t"),
            vec!["t"]
        );
        // 先行する CTE は後続の本文から見える。後続は先行の本文から見えない。
        assert_eq!(
            names("WITH a AS (SELECT 1 FROM b), b AS (SELECT 1 FROM a) SELECT * FROM b"),
            vec!["b"]
        );
        // CTE の外側（サブクエリの WITH の外）では見えない。
        assert_eq!(
            names("SELECT * FROM (WITH c AS (SELECT 1 FROM x) SELECT * FROM c) s, c"),
            vec!["x", "c"]
        );
    }

    #[test]
    fn syntax_word_named_tables_are_gated_but_keywords_are_not() {
        assert_eq!(names("SELECT * FROM temp"), vec!["temp"]);
        assert_eq!(names("SELECT * FROM \"values\""), vec!["values"]);
        assert_eq!(names("INSERT INTO if (a) VALUES (1)"), vec!["if"]);
        assert_eq!(names("DROP TABLE IF EXISTS t"), vec!["t"]);
        assert_eq!(names("DELETE FROM ONLY t"), vec!["t"]);
        assert_eq!(names("TRUNCATE TABLE ONLY t"), vec!["t"]);
        // `LOCK TABLE` の `TABLE` は構文語（Issue #1958）。
        assert_eq!(names("LOCK TABLE t"), vec!["t"]);
        assert_eq!(names("LOCK TABLE ONLY a, b IN SHARE MODE"), vec!["a", "b"]);
        assert_eq!(names("LOCK t"), vec!["t"]);
        assert_eq!(names("SELECT * FROM a, LATERAL f(a.x)"), vec!["a"]);
        assert_eq!(
            names("INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = 2"),
            vec!["t"]
        );
    }

    fn marked_names(sql: &str) -> Vec<String> {
        let toks = crate::sql::lexer::tokenize_marked(sql).expect("lex");
        unqualified_relation_refs(&toks, &[])
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn quoted_identifier_tokens_are_gated() {
        assert_eq!(marked_names("DROP TABLE \"null\""), vec!["null"]);
        assert_eq!(marked_names("SELECT * FROM \"true\""), vec!["true"]);
        assert_eq!(marked_names("DROP VIEW \"null\""), vec!["null"]);
    }

    #[test]
    fn syntax_word_alias_does_not_open_a_relation_list() {
        assert_eq!(
            names("SELECT 1 AS truncate, id FROM public.t LIMIT 5"),
            Vec::<String>::new()
        );
        assert_eq!(names("SELECT 1 AS truncate, id FROM a, b"), vec!["a", "b"]);
        assert_eq!(names("TRUNCATE a, b"), vec!["a", "b"]);
    }

    #[test]
    fn syntax_word_named_column_does_not_open_a_relation() {
        assert_eq!(
            names("SELECT \"update\" AS x FROM public.t LIMIT 5"),
            Vec::<String>::new()
        );
        assert_eq!(names("SELECT \"join\" AS x, \"only\" y FROM a"), vec!["a"]);
    }

    #[test]
    fn introducer_words_after_quoted_expression_words_still_open_a_relation() {
        // カンマの直後でも表参照リストの中の `ONLY` は構文語（対象表は参照）。
        assert_eq!(names("TRUNCATE a, ONLY b"), vec!["a", "b"]);
        assert_eq!(names("TRUNCATE public.a, ONLY b"), vec!["b"]);
        assert_eq!(names("TRUNCATE a, ONLY b, ONLY c"), vec!["a", "b", "c"]);
        assert_eq!(names("SELECT id FROM a, ONLY b LIMIT 5"), vec!["a", "b"]);
        // 直前が式の語に見える引用付き識別子（表名・索引名・別名）でも、導入語の後の表名は残る。
        assert_eq!(names("CREATE INDEX \"is\" ON t (a)"), vec!["t"]);
        assert_eq!(names("CREATE INDEX \"in\" ON t (a)"), vec!["t"]);
        assert_eq!(
            names("SELECT id FROM \"in\" JOIN b ON x.id = b.id"),
            vec!["in", "b"]
        );
        assert_eq!(
            names("SELECT id FROM a \"is\" JOIN b ON x.id = b.id"),
            vec!["a", "b"]
        );
        // 式の位置の引用付き構文語列名は従来どおり関係を導かない。
        assert_eq!(
            names("SELECT id FROM a WHERE \"join\" IS NULL AND \"only\" IS NULL LIMIT 5"),
            vec!["a"]
        );
        assert_eq!(
            names("SELECT id FROM a ORDER BY b, \"only\" DESC LIMIT 5"),
            vec!["a"]
        );
    }

    #[test]
    fn quoted_cte_names_are_excluded() {
        assert_eq!(
            marked_names("WITH \"null\" AS (SELECT 1) SELECT * FROM \"null\""),
            Vec::<String>::new()
        );
        assert_eq!(
            marked_names("WITH \"true\" AS (SELECT 1) SELECT * FROM \"true\", u"),
            vec!["u"]
        );
    }

    /// SQL-40・Issue #1965: `GRANT`／`REVOKE` の参照ゲートは現在ロール未束縛のセッションでは飛ばす
    /// （`42501` が `42P01` より先）。束縛済みなら表名だけを対象にし、ロール名は走査しない。
    #[test]
    fn grant_revoke_gate_is_skipped_only_for_unbound_sessions() {
        let hidden = || {
            let mut s = SessionState::default();
            s.set_search_path(
                SearchPath::new(vec![SearchPathElem::from_ident("elsewhere").unwrap()]).unwrap(),
            )
            .expect("path");
            s
        };
        let toks = tokenize("GRANT SELECT ON t TO alice").expect("lex");
        let unbound = hidden();
        assert!(check_unqualified_refs(&unbound, &toks, &[]).is_ok());
        let mut bound = hidden();
        bound.bind_role_ids(1, 1);
        let err = check_unqualified_refs(&bound, &toks, &[]).expect_err("hidden public");
        assert_eq!(err.wire_code(), "42P01");
        assert_eq!(
            names("REVOKE SELECT (a) ON t, u FROM alice, bob"),
            vec!["t", "u"]
        );
        assert_eq!(names("GRANT r1 TO r2"), Vec::<String>::new());
    }

    #[test]
    fn index_ddl_names_are_marked_as_index() {
        let toks = tokenize("DROP INDEX i").expect("lex");
        let refs = unqualified_relation_refs(&toks, &[]);
        assert_eq!(
            refs,
            vec![GateRef {
                name: "i".to_string(),
                is_index: true
            }]
        );
        let toks = tokenize("DROP VIEW v").expect("lex");
        assert!(!unqualified_relation_refs(&toks, &[])[0].is_index);
    }
}
