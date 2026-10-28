//! 旧バイナリが書いたデータディレクトリ（フィクスチャ）を現行バイナリで開いたときの `id` 移行の
//! 結合テスト（Issue #2202。ポインタ: TABLE-23・TABLE-12・TABLE-16・TABLE-17、RLS-9・RLS-10、
//! RECOVER-10〜12、PERSIST-1。設計記録: `docs/design/rowid-primary-key-separation.md` §14）。
//!
//! 役割: `table23_legacy_id_migration.rs`（Issue #2200）は現行バイナリが作った従来方式の表を閉じて
//! 開き直すだけで、「旧バイナリが書いたファイル」からの移行を検証していない。本ファイルは、
//! 物理キーを非公開の rowid へ切り替える前のコミット（`c42f7735`。直後の `7af39885` が切り替え）の
//! バイナリで生成した `tests/fixtures/table23_legacy_pre_rowid.redb`（約 190 KiB。コミット済みの
//! バイナリ）をコピーして開き、移行の前後で次が変わらないことを固定する。
//!
//! 固定する内容: 両テナント（同一 `id` を持つ）の `SELECT`・`WHERE id = n`・集約・ENUM／ベクトル／
//! DEFAULT／ビュー／SEQUENCE／墓標列の読み取り、`id` 重複（`23505`）・FK 違反（`23503`）・UNIQUE 重複・
//! `operation_id` の再送の応答、移行後の書き込み（`ON CONFLICT (id)`・`ON CONFLICT (<宣言 PK>)`・
//! `id` 指定の UPDATE／DELETE・参照アクション・SEQUENCE の継続）と他テナントの行の不変。
//! **期待値は現行バイナリの出力ではなく、同じ問い合わせを旧バイナリで実行して記録した結果**
//! （下の `EXPECTED_*`）で、現行側の変更で期待値が一緒に動いて空振りすることを避ける。
//!
//! フィクスチャの再現手順（成果物には含めない生成作業）:
//! 1. `git worktree add --detach <dir> c42f7735`（物理キー切り替え前のコミット）
//! 2. 旧バイナリの `EngineCore::open`＋`execute_sql_in_session` で下の `FIXTURE_SQL` を順に実行する
//!    （DDL は DDL 許可セッション。SQL `INSERT` は可視性 Private 固定のため、Public 行
//!    `plain` の `id` 10・11 は `engine::tenant::insert_typed_row` で別途 1 行ずつ追加した）
//! 3. 閉じたあと `redb::Database::compact()` で詰めてコミットする
//!
//! カタログ版 v2〜v14 を網羅していること、移行後に全表が移行済みでスキーマ本体が不変なことは
//! `crates/engine/src/catalog/legacy_id_migration.rs` の単体テストが担当する。
//!
//! 対象外: 旧バイナリからの移行中に open 自体を中断する故障注入（同単体テストの
//! `interrupted_migration_converges_without_partial_state` が担当）、NoSQL 表層。

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Visibility;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/table23_legacy_pre_rowid.redb"
);

/// フィクスチャを生成した旧バイナリで実行した（テナント, SQL）の列（再現手順 2）。
const FIXTURE_SQL: &[(&str, &str)] = &[
    // ---- 表の作成（カタログ版を v2〜v14 まで網羅するための形） ----
    ("tenant-a", "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')"),
    ("tenant-a", "CREATE TABLE plain (name TEXT)"),
    ("tenant-a", "CREATE TABLE vec (embedding VECTOR(3), body TEXT)"),
    ("tenant-a", "CREATE TABLE dflt (a BIGINT DEFAULT 5 CHECK (a > 0), b TEXT, c TEXT)"),
    ("tenant-a", "CREATE TABLE uniq (code TEXT UNIQUE, a TEXT, b TEXT, UNIQUE (a, b))"),
    ("tenant-a", "CREATE TABLE keyed (code TEXT PRIMARY KEY, n BIGINT)"),
    ("tenant-a", "CREATE TABLE parents (name TEXT)"),
    ("tenant-a", "CREATE TABLE kids (parent_id BIGINT REFERENCES parents, v TEXT)"),
    ("tenant-a", "CREATE TABLE parents_c (name TEXT)"),
    ("tenant-a", "CREATE TABLE kids_c (parent_id BIGINT REFERENCES parents_c ON DELETE CASCADE, v TEXT)"),
    ("tenant-a", "CREATE TABLE kids_s (parent_id BIGINT REFERENCES parents_c ON DELETE SET NULL, v TEXT)"),
    ("tenant-a", "CREATE TABLE named_u (code TEXT, CONSTRAINT uq_named UNIQUE (code))"),
    ("tenant-a", "CREATE TABLE named_k (parent_id BIGINT, CONSTRAINT fk_named FOREIGN KEY (parent_id) REFERENCES parents ON DELETE CASCADE)"),
    ("tenant-a", "CREATE TABLE named_pk (code TEXT, n BIGINT, CONSTRAINT pk_named PRIMARY KEY (code))"),
    ("tenant-a", "CREATE TABLE moody (who TEXT, m mood)"),
    ("tenant-a", "CREATE TABLE tl (v TEXT)"),
    ("tenant-a", "ALTER TABLE tl ADD COLUMN extra TEXT DEFAULT 'x'"),
    ("tenant-a", "ALTER TABLE tl ALTER COLUMN extra SET DEFAULT 'y'"),
    ("tenant-a", "CREATE SEQUENCE seq1"),
    ("tenant-a", "CREATE TABLE seqd (n BIGINT DEFAULT nextval('seq1'), v TEXT)"),
    ("tenant-a", "CREATE TABLE idx (code TEXT, n INTEGER)"),
    ("tenant-a", "CREATE UNIQUE INDEX idx_code ON idx (code)"),
    ("tenant-a", "CREATE INDEX idx_expr ON idx ((lower(code)))"),
    ("tenant-a", "CREATE INDEX idx_part ON idx (code) WHERE n > 0"),
    ("tenant-a", "CREATE TABLE dropped (a TEXT, b TEXT)"),
    ("tenant-a", "ALTER TABLE dropped DROP COLUMN b"),
    ("tenant-a", "CREATE TABLE named_ud (code TEXT, parent_id BIGINT, CONSTRAINT uq_ud UNIQUE (code), FOREIGN KEY (parent_id) REFERENCES parents DEFERRABLE INITIALLY IMMEDIATE)"),
    ("tenant-a", "CREATE VIEW plain_v AS SELECT id, name FROM plain ORDER BY id LIMIT 100"),
    // ---- 行（両テナントが同じ id を持つ） ----
    ("tenant-a", "INSERT INTO plain (id, name) VALUES (1, 'a1') USING OPERATION_ID 'op-plain-a1'"),
    ("tenant-a", "INSERT INTO plain (id, name) VALUES (2, 'a2') USING OPERATION_ID 'op-plain-a2'"),
    ("tenant-a", "INSERT INTO plain (id, name) VALUES (3, 'hidden') USING OPERATION_ID 'op-plain-a3'"),
    ("tenant-b", "INSERT INTO plain (id, name) VALUES (1, 'b1') USING OPERATION_ID 'op-plain-b1'"),
    ("tenant-a", "INSERT INTO vec (id, embedding, body) VALUES (1, '[1,0,0]', 'v-a1') USING OPERATION_ID 'op-vec-a1'"),
    ("tenant-b", "INSERT INTO vec (id, embedding, body) VALUES (1, '[0,1,0]', 'v-b1') USING OPERATION_ID 'op-vec-b1'"),
    ("tenant-a", "INSERT INTO dflt (id, b, c) VALUES (1, 'b', 'c') USING OPERATION_ID 'op-dflt-a1'"),
    ("tenant-a", "INSERT INTO uniq (id, code, a, b) VALUES (1, 'u1', 'x', 'y') USING OPERATION_ID 'op-uniq-a1'"),
    ("tenant-b", "INSERT INTO uniq (id, code, a, b) VALUES (1, 'u1', 'x', 'y') USING OPERATION_ID 'op-uniq-b1'"),
    ("tenant-a", "INSERT INTO keyed (id, code, n) VALUES (5, 'k1', 1) USING OPERATION_ID 'op-keyed-a5'"),
    ("tenant-b", "INSERT INTO keyed (id, code, n) VALUES (5, 'k1', 9) USING OPERATION_ID 'op-keyed-b5'"),
    ("tenant-a", "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-parents-a1'"),
    ("tenant-a", "INSERT INTO parents (id, name) VALUES (2, 'p2') USING OPERATION_ID 'op-parents-a2'"),
    ("tenant-b", "INSERT INTO parents (id, name) VALUES (1, 'bp1') USING OPERATION_ID 'op-parents-b1'"),
    ("tenant-a", "INSERT INTO kids (id, parent_id, v) VALUES (10, 1, 'k') USING OPERATION_ID 'op-kids-a10'"),
    ("tenant-a", "INSERT INTO parents_c (id, name) VALUES (1, 'pc1') USING OPERATION_ID 'op-parentsc-a1'"),
    ("tenant-a", "INSERT INTO kids_c (id, parent_id, v) VALUES (1, 1, 'kc') USING OPERATION_ID 'op-kidsc-a1'"),
    ("tenant-a", "INSERT INTO kids_s (id, parent_id, v) VALUES (1, 1, 'ks') USING OPERATION_ID 'op-kidss-a1'"),
    ("tenant-a", "INSERT INTO named_u (id, code) VALUES (1, 'n1') USING OPERATION_ID 'op-namedu-a1'"),
    ("tenant-a", "INSERT INTO named_k (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-namedk-a1'"),
    ("tenant-a", "INSERT INTO named_pk (id, code, n) VALUES (1, 'np1', 1) USING OPERATION_ID 'op-namedpk-a1'"),
    ("tenant-a", "INSERT INTO moody (id, who, m) VALUES (1, 'w', 'ok') USING OPERATION_ID 'op-moody-a1'"),
    ("tenant-a", "INSERT INTO tl (id, v) VALUES (1, 'old') USING OPERATION_ID 'op-tl-a1'"),
    ("tenant-a", "INSERT INTO seqd (id, v) VALUES (1, 's1') USING OPERATION_ID 'op-seqd-a1'"),
    ("tenant-a", "INSERT INTO seqd (id, v) VALUES (2, 's2') USING OPERATION_ID 'op-seqd-a2'"),
    ("tenant-a", "INSERT INTO dropped (id, a) VALUES (1, 'd1') USING OPERATION_ID 'op-dropped-a1'"),
    ("tenant-a", "INSERT INTO named_ud (id, code, parent_id) VALUES (1, 'ud1', 1) USING OPERATION_ID 'op-namedud-a1'"),
    ("tenant-a", "INSERT INTO idx (id, code, n) VALUES (1, 'Ix', 1) USING OPERATION_ID 'op-idx-a1'"),
];

/// 移行前後で結果が一致しなければならない状態不変の問い合わせ: （テナント, SQL, 旧バイナリでの結果）。
const EXPECTED_PRE: &[(&str, &str, &str)] = &[
    (
        r#"tenant-a"#,
        r#"SELECT id, name FROM plain ORDER BY id LIMIT 100"#,
        r#"rows:[[Integer(1), Text("a1")], [Integer(2), Text("a2")], [Integer(3), Text("hidden")], [Integer(10), Text("a-public10")], [Integer(11), Text("b-public11")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, name FROM plain WHERE id = 2 ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(2), Text("a2")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT COUNT(id), SUM(id), MIN(id), MAX(id) FROM plain"#,
        r#"rows:[[Integer(5), Integer(27), Integer(1), Integer(11)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, body FROM vec ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("v-a1")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, a, b, c FROM dflt ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), SignedInteger(5), Text("b"), Text("c")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code, a, b FROM uniq ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("u1"), Text("x"), Text("y")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code, n FROM keyed WHERE code = 'k1' ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(5), Text("k1"), SignedInteger(1)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, name FROM parents ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("p1")], [Integer(2), Text("p2")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, parent_id, v FROM kids ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(10), SignedInteger(1), Text("k")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, parent_id, v FROM kids_c ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), SignedInteger(1), Text("kc")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, parent_id, v FROM kids_s ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), SignedInteger(1), Text("ks")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code FROM named_u ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("n1")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, parent_id FROM named_k ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), SignedInteger(1)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code, n FROM named_pk ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("np1"), SignedInteger(1)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, who, m FROM moody ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("w"), Text("ok")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, v, extra FROM tl ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("old"), Text("y")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, n, v FROM seqd ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), SignedInteger(1), Text("s1")], [Integer(2), SignedInteger(2), Text("s2")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code, n FROM idx ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("Ix"), SignedInteger(1)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, a FROM dropped ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("d1")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code, parent_id FROM named_ud ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("ud1"), SignedInteger(1)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, name FROM plain_v ORDER BY id LIMIT 100"#,
        r#"rows:[[Integer(1), Text("a1")], [Integer(2), Text("a2")], [Integer(3), Text("hidden")], [Integer(10), Text("a-public10")], [Integer(11), Text("b-public11")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, name FROM plain ORDER BY id LIMIT 100"#,
        r#"rows:[[Integer(1), Text("b1")], [Integer(10), Text("a-public10")], [Integer(11), Text("b-public11")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, name FROM plain WHERE id = 2 ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT COUNT(id), SUM(id), MIN(id), MAX(id) FROM plain"#,
        r#"rows:[[Integer(3), Integer(22), Integer(1), Integer(11)]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, body FROM vec ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("v-b1")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, a, b, c FROM dflt ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code, a, b FROM uniq ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("u1"), Text("x"), Text("y")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code, n FROM keyed WHERE code = 'k1' ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(5), Text("k1"), SignedInteger(9)]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, name FROM parents ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("bp1")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, parent_id, v FROM kids ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, parent_id, v FROM kids_c ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, parent_id, v FROM kids_s ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code FROM named_u ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, parent_id FROM named_k ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code, n FROM named_pk ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, who, m FROM moody ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, v, extra FROM tl ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, n, v FROM seqd ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code, n FROM idx ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, a FROM dropped ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code, parent_id FROM named_ud ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, name FROM plain_v ORDER BY id LIMIT 100"#,
        r#"rows:[[Integer(1), Text("b1")], [Integer(10), Text("a-public10")], [Integer(11), Text("b-public11")]]"#,
    ),
];

/// 状態を変えずに必ず失敗する書き込み（`tenant-a`）の旧バイナリでの結果。
const EXPECTED_ERRORS: &[(&str, &str, &str)] = &[
    (
        r#"tenant-a"#,
        r#"INSERT INTO plain (id, name) VALUES (1, 'dup') USING OPERATION_ID 'x1'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO kids (id, parent_id, v) VALUES (50, 999, 'x') USING OPERATION_ID 'x2'"#,
        r#"err:23503"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO keyed (id, code, n) VALUES (9, 'k1', 1) USING OPERATION_ID 'x3'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO uniq (id, code, a, b) VALUES (9, 'u1', 'q', 'r') USING OPERATION_ID 'x4'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO uniq (id, code, a, b) VALUES (9, 'u9', 'x', 'y') USING OPERATION_ID 'x5'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO idx (id, code, n) VALUES (9, 'Ix', 1) USING OPERATION_ID 'x6'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO named_pk (id, code, n) VALUES (9, 'np1', 1) USING OPERATION_ID 'x7'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO plain (id, name) VALUES (1, 'a1') USING OPERATION_ID 'op-plain-a1'"#,
        r#"err:23505"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO plain (id, name) VALUES (1, 'other') USING OPERATION_ID 'op-plain-a1'"#,
        r#"err:22023"#,
    ),
    (
        r#"tenant-a"#,
        r#"DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'x8'"#,
        r#"err:23503"#,
    ),
];

/// 移行後に行う書き込み（`tenant-a`）の旧バイナリでの結果。
const EXPECTED_WRITES: &[(&str, &str, &str)] = &[
    (
        r#"tenant-a"#,
        r#"INSERT INTO plain (id, name) VALUES (2, 'a2x') ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name USING OPERATION_ID 'y1'"#,
        r#"ok:Insert(InsertOutcome { rows_affected: 1, incremental: None })"#,
    ),
    (
        r#"tenant-a"#,
        r#"UPDATE plain SET name = 'a1x' WHERE id = 1 USING OPERATION_ID 'y2'"#,
        r#"ok:Update(UpdateOutcome { rows_affected: 1 })"#,
    ),
    (
        r#"tenant-a"#,
        r#"DELETE FROM plain WHERE id = 3 USING OPERATION_ID 'y3'"#,
        r#"ok:Delete(DeleteOutcome { rows_affected: 1 })"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO plain (id, name) VALUES (20, 'new') USING OPERATION_ID 'y4'"#,
        r#"ok:Insert(InsertOutcome { rows_affected: 1, incremental: None })"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO keyed (id, code, n) VALUES (5, 'k1', 7) ON CONFLICT (code) DO UPDATE SET n = EXCLUDED.n USING OPERATION_ID 'y5'"#,
        r#"ok:Insert(InsertOutcome { rows_affected: 1, incremental: None })"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO seqd (id, v) VALUES (3, 's3') USING OPERATION_ID 'y6'"#,
        r#"ok:Insert(InsertOutcome { rows_affected: 1, incremental: None })"#,
    ),
    (
        r#"tenant-a"#,
        r#"DELETE FROM parents_c WHERE id = 1 USING OPERATION_ID 'y7'"#,
        r#"ok:Delete(DeleteOutcome { rows_affected: 1 })"#,
    ),
    (
        r#"tenant-a"#,
        r#"INSERT INTO moody (id, who, m) VALUES (2, 'x', 'happy') USING OPERATION_ID 'y8'"#,
        r#"ok:Insert(InsertOutcome { rows_affected: 1, incremental: None })"#,
    ),
];

/// 書き込み後の読み戻し（両テナント）の旧バイナリでの結果。
const EXPECTED_POST: &[(&str, &str, &str)] = &[
    (
        r#"tenant-a"#,
        r#"SELECT id, name FROM plain ORDER BY id LIMIT 100"#,
        r#"rows:[[Integer(1), Text("a1x")], [Integer(2), Text("a2x")], [Integer(10), Text("a-public10")], [Integer(11), Text("b-public11")], [Integer(20), Text("new")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, code, n FROM keyed ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(5), Text("k1"), SignedInteger(7)]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, n, v FROM seqd ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), SignedInteger(1), Text("s1")], [Integer(2), SignedInteger(2), Text("s2")], [Integer(3), SignedInteger(3), Text("s3")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, parent_id, v FROM kids_c ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, parent_id, v FROM kids_s ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Null, Text("ks")]]"#,
    ),
    (
        r#"tenant-a"#,
        r#"SELECT id, who, m FROM moody ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(1), Text("w"), Text("ok")], [Integer(2), Text("x"), Text("happy")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, name FROM plain ORDER BY id LIMIT 100"#,
        r#"rows:[[Integer(1), Text("b1")], [Integer(10), Text("a-public10")], [Integer(11), Text("b-public11")]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, code, n FROM keyed ORDER BY id LIMIT 10"#,
        r#"rows:[[Integer(5), Text("k1"), SignedInteger(9)]]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, n, v FROM seqd ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, parent_id, v FROM kids_c ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, parent_id, v FROM kids_s ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
    (
        r#"tenant-b"#,
        r#"SELECT id, who, m FROM moody ORDER BY id LIMIT 10"#,
        r#"rows:[]"#,
    ),
];

fn ctx(t: &str) -> PolicyContext {
    PolicyContext::with_visibilities(t, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 旧バイナリでの記録（`i2202_record`）と同じ形式の結果文字列: 読み取りは行のセル列、成功は応答の
/// Debug、失敗は `wire_code`。
fn outcome(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let mut s = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut s, sql) {
        Ok(SqlOutcome::Query(q)) => {
            let cells: Vec<_> = q.rows.iter().map(|r| r.cells.clone()).collect();
            format!("rows:{cells:?}")
        }
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => format!("err:{}", e.wire_code()),
    }
}

fn check(core: &EngineCore, expected: &[(&str, &str, &str)], phase: &str) {
    assert!(!expected.is_empty());
    for (tenant, sql, want) in expected {
        let got = outcome(core, tenant, sql);
        assert_eq!(&got, want, "{phase}: {tenant}: {sql}");
    }
}

/// フィクスチャのコピーを開く（元ファイルは変更しない）。
fn open_fixture_copy(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    std::fs::copy(FIXTURE, &path).expect("copy fixture");
    (path, guard)
}

/// 非空虚ゲート: 期待値が実際に行を含み、両テナントの境界・Public 行・エラー応答を含む。
#[test]
fn recorded_expectations_are_not_vacuous() {
    let rows_nonempty = EXPECTED_PRE
        .iter()
        .filter(|(_, _, r)| r.starts_with("rows:") && *r != "rows:[]")
        .count();
    let rows_empty = EXPECTED_PRE
        .iter()
        .filter(|(_, _, r)| *r == "rows:[]")
        .count();
    assert!(rows_nonempty >= 25, "{rows_nonempty}");
    // 他テナントの行が見えない（空の結果）が実際に含まれる。
    assert!(rows_empty >= 10, "{rows_empty}");
    // 他テナントの Public 行が読み取りで見える（RLS-7）ことが記録されている。
    assert!(EXPECTED_PRE
        .iter()
        .any(|(t, _, r)| *t == "tenant-b" && r.contains("a-public10")));
    // 3 種の違反応答（重複・FK・operation_id の内容違い）が含まれる。
    for code in ["err:23505", "err:23503", "err:22023"] {
        assert!(EXPECTED_ERRORS.iter().any(|(_, _, r)| *r == code), "{code}");
    }
    assert!(EXPECTED_WRITES.iter().all(|(_, _, r)| r.starts_with("ok:")));
    assert!(EXPECTED_POST.len() >= 12);
}

/// フィクスチャの再現手順（`FIXTURE_SQL`）の表がすべてフィクスチャに存在する。
#[test]
fn fixture_contains_every_table_of_the_recorded_recipe() {
    let (path, _g) = open_fixture_copy("table23-fixture-recipe");
    let core = EngineCore::open(&path).expect("open fixture");
    let mut tables = 0;
    for (_, sql) in FIXTURE_SQL {
        let Some(rest) = sql.strip_prefix("CREATE TABLE ") else {
            continue;
        };
        let name = rest.split_whitespace().next().expect("table name");
        let probe = format!("SELECT id FROM {name} ORDER BY id LIMIT 1");
        let out = outcome(&core, "tenant-a", &probe);
        assert!(out.starts_with("rows:"), "{name}: {out}");
        tables += 1;
    }
    assert!(tables >= 15, "{tables}");
}

/// 受け入れ基準: 旧バイナリが書いたデータディレクトリを開く（＝移行する）と、既存クエリの結果が
/// 旧バイナリでの結果と一致し、開き直しを繰り返しても変わらない。
#[test]
fn queries_match_the_old_binary_after_migration_and_stay_stable_on_reopen() {
    let (path, _g) = open_fixture_copy("table23-fixture-read");
    {
        let core = EngineCore::open(&path).expect("open migrates the fixture");
        check(&core, EXPECTED_PRE, "after migration");
        check(&core, EXPECTED_ERRORS, "errors after migration");
        // エラー応答は状態を変えない（読み取りがそのまま一致し続ける）。
        check(&core, EXPECTED_PRE, "after error probes");
    }
    let core = EngineCore::open(&path).expect("reopen");
    check(&core, EXPECTED_PRE, "after reopen");
    drop(core);
    let core = EngineCore::open(&path).expect("reopen again");
    check(&core, EXPECTED_PRE, "after second reopen");
    check(&core, EXPECTED_ERRORS, "errors after second reopen");
}

/// 移行後の書き込み（`ON CONFLICT (id)`・`ON CONFLICT (<宣言 PK>)`・`id` 指定の UPDATE／DELETE・
/// 参照アクション・SEQUENCE の継続）が旧バイナリと同じ結果になり、他テナントの行を変えない。
#[test]
fn writes_after_migration_match_the_old_binary_and_keep_other_tenants_intact() {
    let (path, _g) = open_fixture_copy("table23-fixture-write");
    let core = EngineCore::open(&path).expect("open migrates the fixture");
    check(&core, EXPECTED_PRE, "before writes");
    check(&core, EXPECTED_WRITES, "writes");
    check(&core, EXPECTED_POST, "after writes");
    // 書き込みは永続化されている（開き直しても同じ）。
    drop(core);
    let core = EngineCore::open(&path).expect("reopen");
    check(&core, EXPECTED_POST, "after reopen");
}

/// 新しく SQL で作った採番方式の表が、移行済みの表の宣言キーを参照する FK は、自テナントの親だけを
/// 見つける（他テナントだけが持つ親キーはどこにも無い場合と同じ応答）。`id` 参照 FK は対象外。
#[test]
fn allocated_table_can_reference_declared_key_of_a_migrated_table() {
    let (path, _g) = open_fixture_copy("table23-fixture-fk");
    let core = EngineCore::open(&path).expect("open migrates the fixture");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut s,
        "CREATE TABLE refs (kcode TEXT REFERENCES keyed (code), note TEXT)",
    )
    .expect("create allocated child of a migrated parent");
    let ins = |tenant: &str, code: &str, op: &str| {
        outcome(
            &core,
            tenant,
            &format!(
                "INSERT INTO refs (kcode, note) VALUES ('{code}', 'n') USING OPERATION_ID '{op}'"
            ),
        )
    };
    // どちらのテナントも宣言キー `k1` を（別の行として）持つ。
    assert!(ins("tenant-a", "k1", "fk-a1").starts_with("ok:"));
    assert!(ins("tenant-b", "k1", "fk-b1").starts_with("ok:"));
    // どこにも無いキーは拒否される。
    assert_eq!(ins("tenant-a", "zz", "fk-a2"), "err:23503");
    // tenant-b だけが持つキーを tenant-a から参照しても、どこにも無い場合と同じ応答。
    core.execute_sql_in_session(
        &ctx("tenant-b"),
        &mut SessionState::default(),
        "INSERT INTO keyed (id, code, n) VALUES (6, 'only-b', 1) USING OPERATION_ID 'fk-b-parent'",
    )
    .expect("tenant-b parent");
    assert_eq!(
        ins("tenant-a", "only-b", "fk-a3"),
        ins("tenant-a", "zz", "fk-a4")
    );
    assert!(ins("tenant-b", "only-b", "fk-b2").starts_with("ok:"));
}
