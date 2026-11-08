//! 論理ダンプファイルの形式（版 1）の書き手と読み手（Issue #2162・RECOVER-16 ポインタ。
//! 方式は `docs/design/logical-dump.md` の第 12 節）。
//!
//! 責務境界: ダンプファイルのバイト列と、`engine` が渡す DDL／データ断片の対応づけだけを担う。
//! 文の実行は [`crate::dump_restore`]、書き出しの駆動は [`crate::dump_run`] が行う。
//!
//! 形式: 先頭のヘッダ（`-- ` で始まるコメント行 5 本）に続き、文ごとに「長さ付きの枠」を並べる。
//! 枠の見出しも `--` コメントなので、ファイル全体は SQL スクリプトとしても読める（`psql` 等に流せる。
//! COPY は `;` の後ろにデータ行と `\.` が続く標準の形）。復元側は **見出しの長さに従って本文を
//! 切り出す**（`;` や改行で分割しない。文字列リテラル・ドル引用・コメントの中の `;`・改行を
//! 壊さないため）。切り出した本文は復元側が字句解析器（`split_statements`）で 1 文であることを
//! 検証するので、長さを書き換えた改ざんは 1 文でなくなって拒否される。
//!
//! 受信（ダンプファイル）は untrusted として扱う: 行長・文長に上限を設け、`unwrap`／`expect`／添字
//! アクセスを使わず、長さは検証してから確保に使う（確保は読み出したバイト数に比例させる）。

use std::io::{BufRead, Read, Write};

use engine::catalog::ddl_export::DdlObjectKind;

use crate::dump_opt::{DumpFormat, DumpParts, DumpScope};

/// 先頭行（版を含む）。完全一致しなければ拒否する。
pub const MAGIC: &str = "-- fandhe-db logical dump v1";
/// ヘッダの終端行。
pub const BODY_MARK: &str = "-- body";
/// 末尾の終端行。これが無いファイル（枠の境界での切り詰めを含む）は読み切れても拒否する。
pub const END_MARK: &str = "-- end";
/// ヘッダ 1 行の最大バイト数（改行を除く）。テナント名の上限より十分に大きい。
pub const MAX_HEADER_LINE: usize = 512;
/// 1 文（DDL・`INSERT`・`COPY` の見出し文）の本文の最大バイト数。書き手も同じ上限を課し、
/// 復元できないファイルを作らない。
pub const MAX_STATEMENT_BYTES: usize = 64 * 1024 * 1024;
/// COPY のデータ 1 行の最大バイト数（改行を除く）。書き手も同じ上限を課す。
pub const MAX_COPY_LINE: usize = 64 * 1024 * 1024;
/// 枠の見出し行の最大バイト数。
const MAX_MARKER_LINE: usize = 128;

/// ダンプファイルの読み書きの失敗（種別だけを持ち、文の内容・名前・パスは含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileError {
    /// ヘッダが版 1 の形式と一致しない。
    BadHeader,
    /// 枠の見出し・長さ・終端が形式と一致しない（改ざん・破損）。
    BadFrame,
    /// 長さが上限を超える。
    TooLarge,
    /// 途中でファイルが終わっている。
    Truncated,
    /// UTF-8 でない・NUL を含むなど、文として扱えない本文。
    BadText,
    /// 読み書きの I/O 失敗。
    Io(std::io::ErrorKind),
}

impl From<std::io::Error> for FileError {
    fn from(e: std::io::Error) -> Self {
        FileError::Io(e.kind())
    }
}

impl FileError {
    /// 固定の英語の説明（内容を含まない）。
    pub fn message(self) -> &'static str {
        match self {
            FileError::BadHeader => "dump file header is invalid or unsupported",
            FileError::BadFrame => "dump file is malformed",
            FileError::TooLarge => "dump file contains an item larger than the supported limit",
            FileError::Truncated => "dump file is truncated",
            FileError::BadText => "dump file contains text that is not valid",
            FileError::Io(_) => "failed to read or write the dump file",
        }
    }
}

/// 枠の種別。`DdlObjectKind` と対応し、見出しに載る固定の語で表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtKind {
    /// `CREATE SCHEMA`。
    Schema,
    /// `CREATE ROLE`。
    Role,
    /// ロールのメンバーシップ（`GRANT`）。
    RoleMembership,
    /// `CREATE TYPE ... AS ENUM`。
    EnumType,
    /// `CREATE SEQUENCE`／`ALTER SEQUENCE`。
    Sequence,
    /// `SELECT setval(...)`。
    SequenceValue,
    /// `CREATE TABLE`。
    Table,
    /// 外部キー制約。
    ForeignKey,
    /// `CREATE [UNIQUE] INDEX`。
    Index,
    /// `CREATE VIEW`。
    View,
    /// `CREATE MATERIALIZED VIEW ... WITH NO DATA`（実体は運ばない。Issue #3097）。
    MaterializedView,
    /// `REFRESH MATERIALIZED VIEW`（実体の再計算。論理ダンプ（`parts: all`）の末尾だけに出る。Issue #3097）。
    MaterializedViewRefresh,
    /// `CREATE FUNCTION`。
    Function,
    /// `CREATE TRIGGER`。
    Trigger,
    /// 行セキュリティの有効化。
    RowSecurity,
    /// `CREATE POLICY`。
    Policy,
    /// `GRANT`。
    Privilege,
    /// データの `INSERT` 1 文。
    Insert,
}

impl StmtKind {
    /// `DdlObjectKind` から対応する種別へ。網羅 `match` で、型が増えたらコンパイルエラーになる。
    pub fn from_ddl(kind: DdlObjectKind) -> Self {
        match kind {
            DdlObjectKind::Schema => StmtKind::Schema,
            DdlObjectKind::Role => StmtKind::Role,
            DdlObjectKind::RoleMembership => StmtKind::RoleMembership,
            DdlObjectKind::EnumType => StmtKind::EnumType,
            DdlObjectKind::Sequence => StmtKind::Sequence,
            DdlObjectKind::SequenceValue => StmtKind::SequenceValue,
            DdlObjectKind::Table => StmtKind::Table,
            DdlObjectKind::ForeignKey => StmtKind::ForeignKey,
            DdlObjectKind::Index => StmtKind::Index,
            DdlObjectKind::View => StmtKind::View,
            DdlObjectKind::MaterializedView => StmtKind::MaterializedView,
            DdlObjectKind::MaterializedViewRefresh => StmtKind::MaterializedViewRefresh,
            DdlObjectKind::Function => StmtKind::Function,
            DdlObjectKind::Trigger => StmtKind::Trigger,
            DdlObjectKind::RowSecurity => StmtKind::RowSecurity,
            DdlObjectKind::Policy => StmtKind::Policy,
            DdlObjectKind::Privilege => StmtKind::Privilege,
        }
    }

    /// 見出しに載せる語。
    pub fn token(self) -> &'static str {
        match self {
            StmtKind::Schema => "schema",
            StmtKind::Role => "role",
            StmtKind::RoleMembership => "role-membership",
            StmtKind::EnumType => "enum-type",
            StmtKind::Sequence => "sequence",
            StmtKind::SequenceValue => "sequence-value",
            StmtKind::Table => "table",
            StmtKind::ForeignKey => "foreign-key",
            StmtKind::Index => "index",
            StmtKind::View => "view",
            StmtKind::MaterializedView => "materialized-view",
            StmtKind::MaterializedViewRefresh => "materialized-view-refresh",
            StmtKind::Function => "function",
            StmtKind::Trigger => "trigger",
            StmtKind::RowSecurity => "row-security",
            StmtKind::Policy => "policy",
            StmtKind::Privilege => "privilege",
            StmtKind::Insert => "insert",
        }
    }

    fn from_token(t: &str) -> Option<Self> {
        const ALL: [StmtKind; 18] = [
            StmtKind::Schema,
            StmtKind::Role,
            StmtKind::RoleMembership,
            StmtKind::EnumType,
            StmtKind::Sequence,
            StmtKind::SequenceValue,
            StmtKind::Table,
            StmtKind::ForeignKey,
            StmtKind::Index,
            StmtKind::View,
            StmtKind::MaterializedView,
            StmtKind::MaterializedViewRefresh,
            StmtKind::Function,
            StmtKind::Trigger,
            StmtKind::RowSecurity,
            StmtKind::Policy,
            StmtKind::Privilege,
            StmtKind::Insert,
        ];
        ALL.into_iter().find(|k| k.token() == t)
    }

    /// 本文が取りうる先頭の語句（書き手の出力形に一致する大文字の接頭辞）。復元側はこの許可表に
    /// 合わない文を実行しない（ダンプファイルが任意の SQL を運ぶ経路を狭める）。
    pub fn allowed_prefixes(self) -> &'static [&'static str] {
        match self {
            StmtKind::Schema => &["CREATE SCHEMA "],
            StmtKind::Role => &["CREATE ROLE "],
            StmtKind::RoleMembership | StmtKind::Privilege => &["GRANT "],
            StmtKind::EnumType => &["CREATE TYPE "],
            StmtKind::Sequence => &["CREATE SEQUENCE ", "ALTER SEQUENCE "],
            StmtKind::SequenceValue => &["SELECT setval("],
            StmtKind::Table => &["CREATE TABLE "],
            StmtKind::ForeignKey | StmtKind::RowSecurity => &["ALTER TABLE "],
            StmtKind::Index => &["CREATE INDEX ", "CREATE UNIQUE INDEX "],
            StmtKind::View => &["CREATE VIEW "],
            StmtKind::MaterializedView => &["CREATE MATERIALIZED VIEW "],
            StmtKind::MaterializedViewRefresh => &["REFRESH MATERIALIZED VIEW "],
            StmtKind::Function => &["CREATE FUNCTION "],
            StmtKind::Trigger => &["CREATE TRIGGER "],
            StmtKind::Policy => &["CREATE POLICY "],
            StmtKind::Insert => &["INSERT INTO "],
        }
    }

    /// 復元時に実行してよい段階。`engine` の `PreData`／`PostData` の区分と一致させ、データの投入中に
    /// 効いてしまうもの（外部キー・索引・トリガ・ポリシー・行セキュリティ・権限・採番の現在値）は
    /// 後段でしか受け付けない。
    pub fn phase(self) -> Phase {
        match self {
            StmtKind::Schema
            | StmtKind::Role
            | StmtKind::RoleMembership
            | StmtKind::EnumType
            | StmtKind::Sequence
            | StmtKind::Table
            | StmtKind::View
            | StmtKind::MaterializedView
            | StmtKind::Function => Phase::Pre,
            StmtKind::Insert => Phase::Data,
            StmtKind::SequenceValue
            | StmtKind::ForeignKey
            | StmtKind::Index
            | StmtKind::Trigger
            | StmtKind::RowSecurity
            | StmtKind::Policy
            | StmtKind::Privilege
            | StmtKind::MaterializedViewRefresh => Phase::Post,
        }
    }

    /// 本文が種別ごとの期待する形か。多くは許可表の接頭辞だが、`setval`・`ALTER TABLE` の 2 種別は
    /// 接頭辞だけで通すと別の式・別の操作（`DISABLE ROW LEVEL SECURITY` など）が通るため、
    /// 構造で厳密に照合する。
    pub fn accepts(self, sql: &str) -> bool {
        match self {
            StmtKind::SequenceValue => is_setval_call(sql),
            StmtKind::ForeignKey => is_add_foreign_key(sql),
            StmtKind::RowSecurity => is_enable_or_force_rls(sql),
            StmtKind::MaterializedView => is_create_matview_no_data(sql),
            StmtKind::MaterializedViewRefresh => is_refresh_matview(sql),
            _ => self.allowed_prefixes().iter().any(|p| sql.starts_with(p)),
        }
    }
}

/// 復元の段階。枠はこの順（`Pre` → `Data` → `Post`）にしか並べられない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// オブジェクトの作成。
    Pre,
    /// 行の投入（`INSERT`・`COPY`）。
    Data,
    /// 制約・索引・トリガ・ポリシー・採番の現在値。
    Post,
}

/// `SELECT setval('<名前>', <整数>, TRUE|FALSE)` の厳密な形か（他の式・引数を許さない）。
fn is_setval_call(sql: &str) -> bool {
    let Some(rest) = sql.strip_prefix("SELECT setval('") else {
        return false;
    };
    let Some((name, rest)) = rest.split_once("', ") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(')') else {
        return false;
    };
    let Some((num, flag)) = rest.split_once(", ") else {
        return false;
    };
    let digits = num.strip_prefix('-').unwrap_or(num);
    !name.is_empty()
        && is_setval_name(name)
        && !digits.is_empty()
        && digits.len() <= 19
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(flag, "TRUE" | "FALSE")
}

/// `setval` の名前引数。素の識別子、または SCHEMA 修飾名 `schema.name`（`.` は 1 個まで。両側は空でない。
/// 各部は英数字と `_` だけ。Issue #3097）。
fn is_setval_name(name: &str) -> bool {
    let part_ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_');
    match name.split_once('.') {
        Some((schema, rel)) => part_ok(schema) && part_ok(rel),
        None => part_ok(name),
    }
}

/// `CREATE MATERIALIZED VIEW ... WITH NO DATA` の形か。実体を作る `WITH DATA`（または句なし）は不可
/// （`Pre` の段で本文を評価させない。再計算は `Post` の `REFRESH` 枠だけが行う）。
fn is_create_matview_no_data(sql: &str) -> bool {
    // 末尾の `WITH NO DATA` をコメント（`-- WITH NO DATA` 等）で偽装できないよう、引用符の外に
    // コメント開始記号があれば拒否する（engine の lexer はコメントを除去するため、実際は `WITH DATA`
    // 既定となり `Pre` で本文が評価される）。ダンプの書き手は SQL を再描画し、コメントを出さない。
    !has_comment_outside_quotes(sql)
        && sql.starts_with("CREATE MATERIALIZED VIEW ")
        && sql.ends_with(" WITH NO DATA")
        && sql.len() > "CREATE MATERIALIZED VIEW  WITH NO DATA".len()
}

/// 単引用符の文字列・二重引用符の識別子の外に、行コメント `--` またはブロックコメント `/*` があるか。
/// 引用符は閉じるまで読み飛ばす（`''` / `""` の連続は閉じて直ちに開き直す扱いで同じ結果になる）。
fn has_comment_outside_quotes(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while let Some(&c) = b.get(i) {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' => quote = Some(c),
                b'-' if b.get(i + 1) == Some(&b'-') => return true,
                b'/' if b.get(i + 1) == Some(&b'*') => return true,
                _ => {}
            },
        }
        i += 1;
    }
    false
}

/// `REFRESH MATERIALIZED VIEW <名前>` の厳密な形か。`CONCURRENTLY`・`WITH [NO] DATA`・複数名・後続の語は
/// 不可。名前は書き手の出力形（共有は `"name"`、SCHEMA 配下は引用符なしの `schema.name`）に限る。
fn is_refresh_matview(sql: &str) -> bool {
    let Some(name) = sql.strip_prefix("REFRESH MATERIALIZED VIEW ") else {
        return false;
    };
    let part_ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_');
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(inner) => part_ok(inner),
        None => match name.split_once('.') {
            Some((schema, rel)) => part_ok(schema) && part_ok(rel),
            None => part_ok(name),
        },
    }
}

/// `ALTER TABLE <表> ADD CONSTRAINT <名> FOREIGN KEY ...` の形か。
fn is_add_foreign_key(sql: &str) -> bool {
    let t: Vec<&str> = sql.split_whitespace().take(9).collect();
    matches!(
        (t.first(), t.get(1), t.get(2), t.get(3), t.get(4), t.get(5)),
        (
            Some(&"ALTER"),
            Some(&"TABLE"),
            Some(_),
            Some(&"ADD"),
            Some(&"CONSTRAINT"),
            Some(_)
        )
    ) && t.get(6) == Some(&"FOREIGN")
        && t.get(7).is_some_and(|k| k.starts_with("KEY"))
}

/// `ALTER TABLE <表> ENABLE|FORCE ROW LEVEL SECURITY` の形か（`DISABLE`・`NO FORCE` は不可）。
fn is_enable_or_force_rls(sql: &str) -> bool {
    let t: Vec<&str> = sql.split_whitespace().collect();
    t.len() == 7
        && t.first() == Some(&"ALTER")
        && t.get(1) == Some(&"TABLE")
        && matches!(t.get(3), Some(&"ENABLE") | Some(&"FORCE"))
        && t.get(4) == Some(&"ROW")
        && t.get(5) == Some(&"LEVEL")
        && t.get(6) == Some(&"SECURITY")
}

/// `COPY <表> (<列>, ...) FROM STDIN` の形か（オプション句・副問い合わせ・`TO` を許さない）。
/// 表・列は引用符付きまたは素の識別子（英数字・`_`・`.`）だけで構成される。
pub fn is_copy_from_stdin(sql: &str) -> bool {
    let Some(rest) = sql.strip_prefix("COPY ") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(" FROM STDIN") else {
        return false;
    };
    let Some((table, cols)) = rest.split_once(" (") else {
        return false;
    };
    let Some(cols) = cols.strip_suffix(')') else {
        return false;
    };
    let ident_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '"'))
    };
    ident_ok(table) && !cols.is_empty() && cols.split(", ").all(ident_ok)
}

/// ダンプファイルのヘッダ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// 対象範囲。
    pub scope: DumpScope,
    /// 含む内容。
    pub parts: DumpParts,
    /// データの書式。
    pub format: DumpFormat,
}

fn parts_token(p: DumpParts) -> &'static str {
    match p {
        DumpParts::All => "all",
        DumpParts::SchemaOnly => "schema",
        DumpParts::DataOnly => "data",
    }
}

fn format_token(f: DumpFormat) -> &'static str {
    match f {
        DumpFormat::Copy => "copy",
        DumpFormat::Insert => "insert",
    }
}

/// ダンプファイルの書き手。出力先（`Write`）へ枠を順に書く。
pub struct FileWriter<W: Write> {
    out: W,
}

impl<W: Write> FileWriter<W> {
    /// ヘッダを書いて書き手を作る。
    pub fn begin(mut out: W, header: &Header) -> Result<Self, FileError> {
        let scope = match &header.scope {
            DumpScope::Shared => "shared".to_string(),
            DumpScope::Tenant(t) => format!("tenant={t}"),
        };
        write!(
            out,
            "{MAGIC}\n-- scope: {scope}\n-- parts: {}\n-- format: {}\n{BODY_MARK}\n",
            parts_token(header.parts),
            format_token(header.format),
        )?;
        Ok(Self { out })
    }

    /// 1 文の枠（見出し・本文・`;` と改行）を書く。`sql` は `;` を含まない 1 文。
    pub fn statement(&mut self, kind: StmtKind, sql: &str) -> Result<(), FileError> {
        check_text(sql)?;
        writeln!(self.out, "-- @stmt {} {}", kind.token(), sql.len())?;
        self.out.write_all(sql.as_bytes())?;
        self.out.write_all(b";\n")?;
        Ok(())
    }

    /// `COPY` の見出し文の枠を書く。続けて [`Self::copy_row`]・[`Self::copy_end`] を呼ぶ。
    pub fn copy_header(&mut self, sql: &str) -> Result<(), FileError> {
        check_text(sql)?;
        writeln!(self.out, "-- @copy {}", sql.len())?;
        self.out.write_all(sql.as_bytes())?;
        self.out.write_all(b";\n")?;
        Ok(())
    }

    /// COPY のデータ 1 行（エスケープ済み・改行なし）を書く。生の改行・CR を含む行は、行境界を
    /// 壊すので拒否する（エンジンはこれらを `\n`・`\r` にエスケープする契約）。
    pub fn copy_row(&mut self, row: &str) -> Result<(), FileError> {
        if row.len() > MAX_COPY_LINE {
            return Err(FileError::TooLarge);
        }
        // 終端行と同じ文字列の行は読み戻しで COPY を閉じてしまう（エンジンは値 `\.` を `\\.` に
        // エスケープする契約）ので、書かずに拒否する。
        if row == "\\." || row.contains(['\n', '\r', '\0']) {
            return Err(FileError::BadText);
        }
        self.out.write_all(row.as_bytes())?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    /// COPY の終端行（`\.`）を書く。
    pub fn copy_end(&mut self) -> Result<(), FileError> {
        self.out.write_all(b"\\.\n")?;
        Ok(())
    }

    /// 終端行を書き、バッファを流して出力先を返す。
    pub fn finish(mut self) -> Result<W, FileError> {
        writeln!(self.out, "{END_MARK}")?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// 本文の検査（上限と NUL）。長さ 0 の本文は文ではないので拒否する。
fn check_text(sql: &str) -> Result<(), FileError> {
    if sql.len() > MAX_STATEMENT_BYTES {
        return Err(FileError::TooLarge);
    }
    if sql.is_empty() || sql.contains('\0') {
        return Err(FileError::BadText);
    }
    Ok(())
}

/// 読み出した枠。
#[derive(Debug, PartialEq, Eq)]
pub enum Record {
    /// 1 文。
    Statement {
        /// 種別（許可表の照合に使う）。
        kind: StmtKind,
        /// 本文（`;` なし）。
        sql: String,
    },
    /// `COPY` の見出し文。続けて [`FileReader::next_copy_line`] で行を読み切ること。
    CopyHeader {
        /// 見出し文の本文（`;` なし）。
        sql: String,
    },
}

/// [`FileReader::next_copy_line`] の結果。
#[derive(Debug, PartialEq, Eq)]
pub enum CopyLine<'a> {
    /// データ 1 行（末尾の改行つき。読み手の内部バッファを借用する）。
    Row(&'a [u8]),
    /// 終端行（`\.`）。
    End,
}

/// ダンプファイルの読み手。
pub struct FileReader<R: BufRead> {
    inner: R,
    in_copy: bool,
    line: Vec<u8>,
}

impl<R: BufRead> FileReader<R> {
    /// ヘッダを読み、検証して返す。版・順序・語彙が完全一致しなければ拒否する。
    pub fn open(inner: R) -> Result<(Self, Header), FileError> {
        let mut r = Self {
            inner,
            in_copy: false,
            line: Vec::new(),
        };
        let magic = r.header_line()?;
        if magic != MAGIC {
            return Err(FileError::BadHeader);
        }
        let scope_line = r.header_line()?;
        let scope = match scope_line.strip_prefix("-- scope: ") {
            Some("shared") => DumpScope::Shared,
            Some(v) => match v.strip_prefix("tenant=") {
                Some(t) if crate::dump_opt::is_header_safe_tenant(t) => {
                    DumpScope::Tenant(t.to_string())
                }
                _ => return Err(FileError::BadHeader),
            },
            None => return Err(FileError::BadHeader),
        };
        let parts = match r.header_line()?.as_str() {
            "-- parts: all" => DumpParts::All,
            "-- parts: schema" => DumpParts::SchemaOnly,
            "-- parts: data" => DumpParts::DataOnly,
            _ => return Err(FileError::BadHeader),
        };
        let format = match r.header_line()?.as_str() {
            "-- format: copy" => DumpFormat::Copy,
            "-- format: insert" => DumpFormat::Insert,
            _ => return Err(FileError::BadHeader),
        };
        if r.header_line()? != BODY_MARK {
            return Err(FileError::BadHeader);
        }
        Ok((
            r,
            Header {
                scope,
                parts,
                format,
            },
        ))
    }

    /// 上限つきで 1 行（改行を除く）を `self.line` に読む。行が無ければ `Ok(false)`。
    /// 上限を超える行・改行なしで終わる行は拒否する。
    fn read_bounded_line(&mut self, max: usize, keep_newline: bool) -> Result<bool, FileError> {
        self.line.clear();
        let limit = (max as u64).saturating_add(1);
        let n = (&mut self.inner)
            .take(limit)
            .read_until(b'\n', &mut self.line)?;
        if n == 0 {
            return Ok(false);
        }
        if self.line.last() != Some(&b'\n') {
            // 上限に達したか、改行の前でファイルが終わった。
            return if n as u64 >= limit {
                Err(FileError::TooLarge)
            } else {
                Err(FileError::Truncated)
            };
        }
        if !keep_newline {
            self.line.pop();
        }
        Ok(true)
    }

    fn header_line(&mut self) -> Result<String, FileError> {
        match self.read_bounded_line(MAX_HEADER_LINE, false) {
            Ok(true) => {}
            Ok(false) | Err(FileError::Truncated) => return Err(FileError::BadHeader),
            Err(FileError::TooLarge) => return Err(FileError::BadHeader),
            Err(e) => return Err(e),
        }
        String::from_utf8(self.line.clone()).map_err(|_| FileError::BadHeader)
    }

    /// 次の枠を読む。終端行（直後がファイルの終わり）なら `Ok(None)`。終端行が無いまま終わったら
    /// `Truncated`。直前の COPY を読み切っていなければ拒否する。
    pub fn next_record(&mut self) -> Result<Option<Record>, FileError> {
        if self.in_copy {
            return Err(FileError::BadFrame);
        }
        // 見出し行の長さ超過・途中終了・終端行の欠落は枠の破損（切り詰め）として扱う。
        match self.read_bounded_line(MAX_MARKER_LINE, false) {
            Ok(true) => {}
            Ok(false) => return Err(FileError::Truncated),
            Err(FileError::TooLarge) => return Err(FileError::BadFrame),
            Err(e) => return Err(e),
        }
        if self.line == END_MARK.as_bytes() {
            // 終端行の後ろには何も続かない（連結・追記された内容を黙って無視しない）。
            return if self.inner.fill_buf()?.is_empty() {
                Ok(None)
            } else {
                Err(FileError::BadFrame)
            };
        }
        let marker = std::str::from_utf8(&self.line).map_err(|_| FileError::BadFrame)?;
        let mut parts = marker.split(' ');
        let (Some("--"), Some(tag)) = (parts.next(), parts.next()) else {
            return Err(FileError::BadFrame);
        };
        match tag {
            "@stmt" => {
                let kind = parts
                    .next()
                    .and_then(StmtKind::from_token)
                    .ok_or(FileError::BadFrame)?;
                let len = parse_len(parts.next())?;
                if parts.next().is_some() {
                    return Err(FileError::BadFrame);
                }
                let sql = self.read_body(len)?;
                Ok(Some(Record::Statement { kind, sql }))
            }
            "@copy" => {
                let len = parse_len(parts.next())?;
                if parts.next().is_some() {
                    return Err(FileError::BadFrame);
                }
                let sql = self.read_body(len)?;
                self.in_copy = true;
                Ok(Some(Record::CopyHeader { sql }))
            }
            _ => Err(FileError::BadFrame),
        }
    }

    /// 長さ `len` の本文と、続く `;` と改行を読む。確保は実際に読めたバイト数に比例させる
    /// （長さの申告だけで確保しない）。
    fn read_body(&mut self, len: usize) -> Result<String, FileError> {
        let mut body = Vec::new();
        let got = (&mut self.inner).take(len as u64).read_to_end(&mut body)?;
        if got != len {
            return Err(FileError::Truncated);
        }
        let mut tail = [0u8; 2];
        self.inner.read_exact(&mut tail).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                FileError::Truncated
            } else {
                FileError::Io(e.kind())
            }
        })?;
        if &tail != b";\n" {
            return Err(FileError::BadFrame);
        }
        let sql = String::from_utf8(body).map_err(|_| FileError::BadText)?;
        check_text(&sql)?;
        Ok(sql)
    }

    /// `COPY` のデータを 1 行読んで借用で返す（末尾の改行つき。行を二重に保持しない）。終端行（`\.`）なら
    /// [`CopyLine::End`] を返して COPY を閉じる。行は改行でのみ区切る（COPY text の行内の改行・CR は
    /// エスケープされており、生の改行は行境界だけである）。
    pub fn next_copy_line(&mut self) -> Result<CopyLine<'_>, FileError> {
        if !self.in_copy {
            return Err(FileError::BadFrame);
        }
        // 行（改行を含む）が上限を超えるか、終端行の前でファイルが終わったら拒否する。
        match self.read_bounded_line(MAX_COPY_LINE, true) {
            Ok(true) => {}
            Ok(false) => return Err(FileError::Truncated),
            Err(e) => return Err(e),
        }
        if self.line == b"\\.\n" {
            self.in_copy = false;
            return Ok(CopyLine::End);
        }
        if self.line.contains(&0) {
            return Err(FileError::BadText);
        }
        Ok(CopyLine::Row(&self.line))
    }
}

fn parse_len(tok: Option<&str>) -> Result<usize, FileError> {
    let tok = tok.ok_or(FileError::BadFrame)?;
    // 先頭 0・符号・空白を許さない 10 進のみ。
    if tok.is_empty()
        || !tok.bytes().all(|b| b.is_ascii_digit())
        || (tok.len() > 1 && tok.starts_with('0'))
    {
        return Err(FileError::BadFrame);
    }
    let n: usize = tok.parse().map_err(|_| FileError::BadFrame)?;
    if n == 0 {
        return Err(FileError::BadFrame);
    }
    if n > MAX_STATEMENT_BYTES {
        return Err(FileError::TooLarge);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            scope: DumpScope::Tenant("t1".into()),
            parts: DumpParts::All,
            format: DumpFormat::Copy,
        }
    }

    fn sample() -> Vec<u8> {
        let mut w = FileWriter::begin(Vec::new(), &header()).expect("begin");
        w.statement(StmtKind::Table, "CREATE TABLE t (a TEXT)")
            .expect("stmt");
        w.statement(StmtKind::Insert, "INSERT INTO t (a) VALUES ('x;\ny -- $$')")
            .expect("stmt");
        w.copy_header("COPY t (a) FROM STDIN").expect("copy");
        w.copy_row("a;b\\nc -- $$").expect("row");
        w.copy_row("\\\\.").expect("row");
        w.copy_end().expect("end");
        w.finish().expect("finish")
    }

    #[test]
    fn matview_no_data_check_rejects_comment_disguise() {
        assert!(is_create_matview_no_data(
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t WITH NO DATA"
        ));
        // 文字列・識別子内の `--` は許可する。
        assert!(is_create_matview_no_data(
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT '--x' FROM t WITH NO DATA"
        ));
        for sql in [
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t -- WITH NO DATA",
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t /* x */ WITH NO DATA",
            "CREATE MATERIALIZED VIEW \"mv\" AS SELECT id FROM t --\n WITH NO DATA",
        ] {
            assert!(!is_create_matview_no_data(sql), "{sql}");
        }
    }

    #[test]
    fn round_trips_statements_and_copy_rows() {
        let bytes = sample();
        let (mut r, h) = FileReader::open(&bytes[..]).expect("open");
        assert_eq!(h, header());
        assert_eq!(
            r.next_record().expect("rec"),
            Some(Record::Statement {
                kind: StmtKind::Table,
                sql: "CREATE TABLE t (a TEXT)".into()
            })
        );
        assert_eq!(
            r.next_record().expect("rec"),
            Some(Record::Statement {
                kind: StmtKind::Insert,
                sql: "INSERT INTO t (a) VALUES ('x;\ny -- $$')".into()
            })
        );
        assert_eq!(
            r.next_record().expect("rec"),
            Some(Record::CopyHeader {
                sql: "COPY t (a) FROM STDIN".into()
            })
        );
        assert_eq!(
            r.next_copy_line().expect("row"),
            CopyLine::Row(b"a;b\\nc -- $$\n")
        );
        assert_eq!(r.next_copy_line().expect("row"), CopyLine::Row(b"\\\\.\n"));
        assert_eq!(r.next_copy_line().expect("end"), CopyLine::End);
        assert_eq!(r.next_record().expect("eof"), None);
    }

    #[test]
    fn rejects_tampered_headers() {
        let bytes = sample();
        let text = String::from_utf8(bytes).expect("utf-8");
        for (from, to) in [
            ("v1", "v2"),
            ("scope: tenant=t1", "scope: tenant=t 1"),
            ("scope: tenant=t1", "scope: all"),
            ("parts: all", "parts: everything"),
            ("format: copy", "format: csv"),
            ("-- body", "-- bodyx"),
        ] {
            let bad = text.replacen(from, to, 1);
            assert!(
                FileReader::open(bad.as_bytes()).is_err(),
                "header edit {from:?} must be rejected"
            );
        }
        assert!(FileReader::open(&b""[..]).is_err());
        assert!(FileReader::open(&b"-- fandhe-db logical dump v1"[..]).is_err());
        let long = format!("{}{}\n", MAGIC, " ".repeat(MAX_HEADER_LINE));
        assert!(FileReader::open(long.as_bytes()).is_err());
    }

    fn read_all(bytes: &[u8]) -> Result<usize, FileError> {
        let (mut r, _) = FileReader::open(bytes)?;
        let mut n = 0;
        while let Some(rec) = r.next_record()? {
            n += 1;
            if matches!(rec, Record::CopyHeader { .. }) {
                while matches!(r.next_copy_line()?, CopyLine::Row(_)) {}
            }
        }
        Ok(n)
    }

    #[test]
    fn rejects_tampered_frames() {
        let good = sample();
        assert_eq!(read_all(&good).expect("good"), 3);
        let text = String::from_utf8(good.clone()).expect("utf-8");
        for (from, to) in [
            ("@stmt table 23", "@stmt table 22"),
            ("@stmt table 23", "@stmt table 24"),
            ("@stmt table 23", "@stmt table 023"),
            ("@stmt table 23", "@stmt table -23"),
            ("@stmt table 23", "@stmt table 0"),
            ("@stmt table 23", "@stmt table 99999999999999999999999"),
            ("@stmt table 23", "@stmt nope 23"),
            ("@stmt table 23", "@stmt table 23 extra"),
            ("@stmt table", "@bogus table"),
            ("@copy 21", "@copy 22"),
        ] {
            let bad = text.replacen(from, to, 1);
            assert!(
                read_all(bad.as_bytes()).is_err(),
                "edit {from:?} must be rejected"
            );
        }
        // 途中で切れたファイルは、どの位置で切っても成功しない（COPY の終端行の前で切れる場合を含む）。
        for cut in 0..good.len() {
            let r = read_all(good.get(..cut).unwrap_or(&[]));
            assert!(r.is_err(), "truncation at {cut} must not be accepted");
        }
    }

    #[test]
    fn reader_rejects_oversized_lengths_without_allocating() {
        let huge = format!(
            "{}\n-- scope: shared\n-- parts: all\n-- format: copy\n-- body\n-- @stmt table {}\nx",
            MAGIC,
            MAX_STATEMENT_BYTES + 1
        );
        assert_eq!(read_all(huge.as_bytes()), Err(FileError::TooLarge));
        // 上限以内の申告でも、実体が短ければ Truncated（申告分を先に確保しない）。
        let lie = format!(
            "{}\n-- scope: shared\n-- parts: all\n-- format: copy\n-- body\n-- @stmt table {}\nx",
            MAGIC, MAX_STATEMENT_BYTES
        );
        assert_eq!(read_all(lie.as_bytes()), Err(FileError::Truncated));
    }

    #[test]
    fn writer_rejects_unframeable_text() {
        let mut w = FileWriter::begin(Vec::new(), &header()).expect("begin");
        assert!(w.statement(StmtKind::Table, "").is_err());
        assert!(w.statement(StmtKind::Table, "a\0b").is_err());
        assert!(w.copy_row("a\nb").is_err());
        assert!(w.copy_row("a\rb").is_err());
        assert!(w.copy_row("\\.").is_err());
    }

    #[test]
    fn kind_prefix_allowlist_is_enforced() {
        assert!(StmtKind::Table.accepts("CREATE TABLE t (a INT)"));
        assert!(!StmtKind::Table.accepts("DROP TABLE t"));
        assert!(!StmtKind::Table.accepts("create table t (a INT)"));
        assert!(!StmtKind::Insert.accepts("DELETE FROM t"));
        assert!(!StmtKind::SequenceValue.accepts("SELECT 1"));
        assert!(StmtKind::SequenceValue.accepts("SELECT setval('s_seq', 12, TRUE)"));
        assert!(StmtKind::SequenceValue.accepts("SELECT setval('s_seq', -3, FALSE)"));
        for bad in [
            "SELECT setval('s', 1, TRUE), f()",
            "SELECT setval('s', 1, TRUE) FROM t",
            "SELECT setval('s', f(), TRUE)",
            "SELECT setval('s', 1)",
            "SELECT setval('s''; x', 1, TRUE)",
            "SELECT setval('s', 1, maybe)",
        ] {
            assert!(!StmtKind::SequenceValue.accepts(bad), "{bad}");
        }
        assert!(StmtKind::RowSecurity.accepts("ALTER TABLE t ENABLE ROW LEVEL SECURITY"));
        assert!(StmtKind::RowSecurity.accepts("ALTER TABLE t FORCE ROW LEVEL SECURITY"));
        for bad in [
            "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
            "ALTER TABLE t NO FORCE ROW LEVEL SECURITY",
            "ALTER TABLE t DROP COLUMN a",
        ] {
            assert!(!StmtKind::RowSecurity.accepts(bad), "{bad}");
        }
        assert!(StmtKind::ForeignKey.accepts(
            "ALTER TABLE a.t ADD CONSTRAINT \"fk\" FOREIGN KEY (\"p\") REFERENCES u (id)"
        ));
        assert!(!StmtKind::ForeignKey.accepts("ALTER TABLE t DISABLE ROW LEVEL SECURITY"));
        assert!(!StmtKind::ForeignKey.accepts("ALTER TABLE t DROP COLUMN a"));
        assert!(StmtKind::Trigger.phase() > StmtKind::Insert.phase());
        assert!(StmtKind::Function.phase() < StmtKind::Insert.phase());
    }

    #[test]
    fn copy_header_shape_is_structural() {
        assert!(is_copy_from_stdin("COPY \"t\" (\"a\", \"b\") FROM STDIN"));
        assert!(is_copy_from_stdin("COPY app.t (a) FROM STDIN"));
        for bad in [
            "COPY (SELECT 1) TO STDOUT",
            "COPY t (a) FROM STDIN WITH (FORMAT csv)",
            "COPY t FROM STDIN",
            "COPY t (a; DROP TABLE t) FROM STDIN",
            "COPY t (a) TO STDOUT",
            "COPY t (a)) FROM STDIN",
        ] {
            assert!(!is_copy_from_stdin(bad), "{bad}");
        }
    }
}
