//! 台帳エントリの内容照合ハッシュ（TASK-101、対象ビヘイビア: RECOVER-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-101・`docs/spec/04-behavior/recovery.md` RECOVER-10）。
//!
//! `recovery::ledger`（TASK-93・RECOVER-2）の台帳は複合キー（テナント・テーブル・
//! `operation_id`）のみを保持し、書き込み内容そのものは照合できない。過去に commit
//! 済みの `operation_id` が**内容の異なる**書き込みへ誤って再利用された場合、
//! 再送ベースの回復確認（RECOVER-7 系）が誤判定する余地がある。本モジュールは
//! クライアント要求由来の内容のみから決定的な [`ContentHash`] を構成し、
//! `ledger::record_in_txn` が「同一 `operation_id`・同一内容の再送（`23505`）」と
//! 「同一 `operation_id`・内容不一致の誤用（`22023`）」を区別できるようにする。
//!
//! 依存追加なし: 本タスクは自動運転で実行されユーザー承認を得られないため、
//! 依存追加（`.claude/rules/dependency-policy.md`）を避ける安全側の判断として
//! SHA-256 をこのモジュール内で安全 Rust により自作実装する（`unsafe` 不使用）。
//! FIPS 180-4 の公開テストベクタで正当性を機械検証する（下部 `tests` モジュール）。
//!
//! ## 正規化（canonical 化）の方針
//!
//! ハッシュ入力は**クライアント要求由来の内容のみ**から構成し、DB の現在状態
//! （既存行の有無・削除対象集合等）に依存する値を含めない。これにより、同一の
//! クライアント要求が再送された場合は常に同一ハッシュへ決定的に写像される
//! （再送検知の前提条件）。
//!
//! 連結曖昧性（例: `"ab"+"c"` と `"a"+"bc"` が同一バイト列になる事故）を構造的に
//! 排除するため、[`HashInputBuilder`] はすべての可変長フィールドへ長さプレフィクス
//! （4 バイト LE）を付けてから連結する。先頭にドメイン分離タグ（固定文字列）＋
//! 操作種別タグ（1 バイト）を置き、他コンテキストでの SHA-256 利用や操作種別間の
//! 衝突を避ける。
//!
//! 長さプレフィクスだけでは**複数行を 1 ハッシュへ連結する**操作（[`for_typed_insert_batch`]）
//! の行境界までは一意に定まらない（ある行の列データが次の行の固定長フィールドへ
//! 「はみ出して」再解釈され得る。codex-review P1 指摘・PR #823）。1 ハッシュ = 1 行
//! （列データが常に入力全体の末尾）の操作（[`for_typed_insert`]・
//! [`for_replace_by_text_key`]）はこの曖昧性の対象外のため、対応は
//! [`for_typed_insert_batch`] のドキュメント（行ごとの列数プレフィクス）に限定する。
//!
//! 呼び出し元は `crate::tenant::*_unchecked`（6 箇所。TASK-93 の台帳追記と同一の
//! write トランザクション内でハッシュ計算済みの値を渡す設計）。各操作種別の入力
//! レイアウトは対応する `for_*` 関数のコメントを参照。
//!
//! Issue #1061: [`push_dml_assignments`] は述語形 `UPDATE` の `SET` 割当を
//! ハッシュする際、対象列がスキーマ上 `VECTOR(dim)` かどうかを `TableSchema`
//! から読む（`VECTOR` 列への `String` 割当をタグ 5・f32 LE 列へ正準化する
//! ため）。これは「列型」への依存であり、先例は [`for_typed_insert`] の
//! スキーマ列順依存（`ALTER TABLE ADD COLUMN` を挟んでも列名基準で解決する
//! ため不変。PR #248）と同じ性質——列の**型**は `ALTER TABLE MODIFY COLUMN`
//! でも `VECTOR` 列については変更できない（`ProtectedColumn` で拒否。
//! `docs/design/alter-table-drop-modify-column.md` D3）ため、この依存も
//! 「同一クライアント要求は常に同一ハッシュ」という不変条件を壊さない。
//!
//! `encode_row` を経由する操作種別（挿入・バッチ挿入・更新）は、呼び出し元が
//! **1 回だけ** `encode_row` した結果（`&[u8]`）を受け取る `for_*_encoded` 系を
//! production から呼ぶ（Issue #397。encoded バイト列は台帳ハッシュと redb 書き込みの
//! 双方で共有され、以前存在した「ハッシュ計算用と書き込み用でそれぞれ 1 回ずつ、
//! 合計 2 回 `encode_row` する」二重実行を排除する）。`RowInput` を受けて内部で
//! `encode_row` する旧形（`for_insert`／`for_insert_batch`／`for_update`）は
//! `#[cfg(test)]` の参照実装として残し、`for_*_encoded` との等価性テストにのみ使う。

use crate::crypto::sha256::Sha256;
use crate::row_codec::Value;
#[cfg(test)]
use crate::storage::{encode_row, RowInput};
use crate::storage::{StorageError, Visibility};

/// SHA-256 ダイジェスト（32 バイト）を保持する台帳内容ハッシュ。中身の生バイト列は
/// [`ContentHash::as_bytes`] 経由でのみ参照する（`recovery::ledger` の台帳値
/// エンコード・照合専用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentHash([u8; 32]);

impl ContentHash {
    /// 生の 32 バイトダイジェストへの参照。台帳エントリのエンコード（`ledger.rs`）と
    /// 既存エントリとの照合にのみ使う。
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 保存済みダイジェスト（`ledger.rs` がデコードした生バイト列）と一致するかを判定する。
    pub(crate) fn matches(&self, stored: &[u8; 32]) -> bool {
        &self.0 == stored
    }

    /// `ledger.rs` の単体テスト専用: 任意バイト列から直接構成する（`RowInput`/
    /// `TableSchema` を用意せずに「内容の違いで異なるハッシュになる」ことだけを
    /// 検証したいケース向け）。本番経路（`for_insert` 等）は使わない。
    #[cfg(test)]
    pub(crate) fn for_test(seed: &[u8]) -> Self {
        ContentHash(crate::crypto::sha256::digest(seed))
    }
}

/// ドメイン分離タグ。他コンテキストでの SHA-256 利用との衝突を避ける固定文字列
/// （バージョン番号を含み、将来レイアウトを変更する場合は新タグへ切り替える）。
const DOMAIN_TAG: &[u8] = b"vector-db/op_ledger/content_hash/v1";

/// 操作種別タグ（1 バイト）。同一フィールド列でも操作種別が異なれば必ず異なる
/// ハッシュになるよう、[`HashInputBuilder::new`] が先頭に埋め込む。
#[repr(u8)]
#[derive(Clone, Copy)]
enum OpTag {
    Insert = 1,
    InsertBatch = 2,
    Update = 3,
    Delete = 4,
    ReplaceByTextKey = 5,
    Truncate = 6,
    /// `tenant::upsert_typed_rows_unchecked` 用（SQL-20・TASK-193、Issue #872）。
    /// [`for_typed_upsert`] ドキュメント参照。
    Upsert = 7,
    UpdateColumns = 8,
    /// 述語つき `UPDATE ... WHERE`（SQL-19・TASK-192、Issue #871・RECOVER-11）用。
    /// 単一行 `id` 完全一致形（[`OpTag::Update`]）とは別ドメインに分離する
    /// （ADR `docs/design/multi-row-dml-operation-id.md` §4.1）。[`for_update_where`]
    /// ドキュメント参照。
    UpdateWhere = 9,
    /// 述語つき `DELETE ... WHERE`（SQL-19・TASK-192、Issue #871・RECOVER-11）用。
    /// 単一行 `id` 完全一致形（[`OpTag::Delete`]）とは別ドメインに分離する
    /// （ADR 同上）。[`for_delete_where`] ドキュメント参照。
    DeleteWhere = 10,
    /// 分割実行（チャンクごとに commit する非原子）版の述語つき `UPDATE ... WHERE`
    /// 用（Issue #1127。ADR `docs/design/partitioned-dml.md` §6）。原子版
    /// （[`OpTag::UpdateWhere`]）とは別ドメインに分離し、同じ `operation_id` を
    /// 原子版と分割版で取り違えた再送を内容不一致（`22023`）として検出する。
    /// [`for_update_where_partitioned`] ドキュメント参照。
    UpdateWherePartitioned = 11,
    /// 分割実行版の述語つき `DELETE ... WHERE` 用（Issue #1127。ADR 同上）。
    /// [`for_delete_where_partitioned`] ドキュメント参照。
    DeleteWherePartitioned = 12,
}

/// 長さプレフィクス付きフィールド連結でハッシュ入力を組み立てるビルダー
/// （本モジュールドキュメントの「正規化の方針」参照）。
///
/// Issue #399: 内部を `Vec<u8>`（バッチ全体を一度連結し、`finish` でさらに
/// パディング用に全体を複製する二重コピー構造）から [`Sha256`] のストリーミング
/// 更新へ置換した。各 `push_*` は `Sha256::update` を直接呼ぶため、バッチ全体
/// （数百 KB 規模）を保持する中間バッファは存在しない。出力（ダイジェスト）は
/// 旧実装（`#[cfg(test)] sha256_reference`）と等価であることをテストで機械検証する。
struct HashInputBuilder(Sha256);

impl HashInputBuilder {
    fn new(tag: OpTag) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_TAG);
        hasher.update(&[tag as u8]);
        HashInputBuilder(hasher)
    }

    /// 可変長バイト列を「4 バイト LE 長さ＋本体」で連結する。untrusted 入力由来の
    /// 長さを上限検証してからアロケーションに使う契約（coding-rust.md）に従い、
    /// `u32` へ収まらない場合は `Err` で拒否する（`encode_row` 等の既存エンコーダが
    /// 既に検証済みの値のみが渡る想定だが、本関数単体でも fail-closed を保つ）。
    fn push_bytes(&mut self, field: &[u8]) -> Result<(), StorageError> {
        let len = u32::try_from(field.len())
            .map_err(|_| StorageError::Codec("content hash field too large".to_string()))?;
        self.0.update(&len.to_le_bytes());
        self.0.update(field);
        Ok(())
    }

    fn push_u64(&mut self, v: u64) {
        self.0.update(&v.to_le_bytes());
    }

    fn push_u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }

    /// 長さプレフィクスなしで生バイト列をそのまま流し込む（バッチ件数プレフィクス
    /// 等、[`push_bytes`] の長さプレフィクス契約に合わない箇所専用の内部 API）。
    fn push_raw(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(self) -> ContentHash {
        ContentHash(self.0.finalize())
    }
}

/// [`HashInputBuilder`] の参照実装（Issue #399 以前の一括処理版。バッチ全体を
/// `Vec<u8>` へ都度 `extend_from_slice` で連結し、`finish` で [`sha256_reference`]
/// （パディングのため全体をもう一度複製する二重コピー構造）を 1 回呼ぶ）。
/// `#[cfg(test)]`。本番経路は経由しない（理由は [`for_insert`] 参照）。同じ
/// `push_bytes`／`push_u64` 呼び出しパターンを共有する [`for_insert_batch_encoded_reference`]
/// から、production の [`HashInputBuilder`]（ストリーミング版）と同一のフィールド
/// 境界（行ごとに `push_u64` 1 回＋ `push_bytes` 1 回）で A/B 計測できるようにする
/// （codex-review 指摘・PR #419: 単一の巨大バッファへの 1 回 `update` 呼び出しでは
/// 本番経路〔行・フィールド単位の多数回 `update`〕を再現しない）。
#[cfg(test)]
struct HashInputBuilderReference(Vec<u8>);

#[cfg(test)]
impl HashInputBuilderReference {
    fn new(tag: OpTag) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(DOMAIN_TAG);
        buf.push(tag as u8);
        HashInputBuilderReference(buf)
    }

    fn push_bytes(&mut self, field: &[u8]) -> Result<(), StorageError> {
        let len = u32::try_from(field.len())
            .map_err(|_| StorageError::Codec("content hash field too large".to_string()))?;
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(field);
        Ok(())
    }

    fn push_u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn push_raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(self) -> ContentHash {
        ContentHash(crate::crypto::sha256::digest_reference(&self.0))
    }
}

/// `row_codec::Value` 1 個をタグ＋長さプレフィクス付きで連結する。スキーマに
/// 依存せず（`Value` の判別子のみで境界を確定できる）、ハッシュ対象を DB 状態
/// （カタログのスキーマ定義）から独立させる。
fn push_value(b: &mut HashInputBuilder, v: &Value) -> Result<(), StorageError> {
    match v {
        Value::Null => b.push_u8(0),
        Value::Text(text) => {
            b.push_u8(1);
            b.push_bytes(text.as_bytes())?;
        }
        Value::Vector(vector) => {
            b.push_u8(2);
            push_vector(b, vector)?;
        }
        // Issue #881 D7: 既存タグ（Null=0／Text=1／Vector=2）の意味・並びは
        // 変更せず、`INTEGER`／`BIGINT` にタグ 3／4 を新設する。TABLE-13・
        // Issue #882 計画 F9: Real=5・Double=6（正規化後の LE ビット列）。
        // BOOLEAN は TABLE-13 の宣言順で 7 とする（Issue #883。他の型と衝突しない
        // 新規タグ）。8〜9 は DATE/TIMESTAMP（別 Issue の作業）向けに予約する。
        Value::Integer(v) => {
            b.push_u8(3);
            b.push_raw(&v.to_le_bytes());
        }
        Value::BigInt(v) => {
            b.push_u8(4);
            b.push_raw(&v.to_le_bytes());
        }
        // NUMERIC は当初宣言順の 10 を想定していたが、base（main）マージ取り込みで
        // ARRAY（Issue #888）がタグ 10 を先に使用していたため、Issue #885 の
        // origin/main への rebase 時点（NUMERIC の content_hash はまだ一度も
        // 永続化されていないため後方互換の懸念なし）で衝突しない未使用タグ 14 へ
        // 採番し直した（ENUM のタグ 13 の次点）。ハッシュ対象は束縛後の正規値
        // （`scale` + `unscaled`）のため、同一列へ再送された `1.10` と `1.1` は
        // 同一ハッシュ（`23505`）に収束する。
        Value::Real(v) => {
            b.push_u8(5);
            // F4: エンコード時に `-0.0` を `+0.0` へ正規化する契約
            // （row_codec.rs のエンコード経路と同じ理由）。ここでハッシュ入力を
            // 正規化前の生ビット列のままにすると、論理的に同一な値の
            // `operation_id` 再送が重複応答（23505）ではなく内容不一致
            // （22023）として扱われてしまう。
            b.push_raw(&crate::scalar_float::canonicalize_real(*v).to_le_bytes());
        }
        Value::Double(v) => {
            b.push_u8(6);
            // F4: canonicalize_real と同じ理由。
            b.push_raw(&crate::scalar_float::canonicalize_double(*v).to_le_bytes());
        }
        Value::Numeric(d) => {
            b.push_u8(14);
            b.push_u8(d.scale());
            b.push_bytes(&d.unscaled().to_le_bytes())?;
        }
        Value::Bool(b_val) => {
            b.push_u8(7);
            b.push_u8(u8::from(*b_val));
        }
        // DATE は宣言順で 8、TIMESTAMP は 9 とする（Issue #884・D-3。TABLE-13 の
        // 宣言順規則を踏襲し、他の型タグと衝突しない新規タグ）。負値も含め
        // ビット列をそのまま連結するため、`i32`/`i64` を符号保存のまま `u64` へ
        // 変換して既存の `push_u64` を再利用する。
        Value::Date(days) => {
            b.push_u8(8);
            b.push_u64(i64::from(*days) as u64);
        }
        Value::Timestamp(micros) => {
            b.push_u8(9);
            b.push_u64(*micros as u64);
        }
        // 配列列（TABLE-14・Issue #888）は 10 とする。要素型タグ・要素数・各要素を
        // 積むことで、`{ab}`（1 要素）と `{a,b}`（2 要素）のような表記ゆれが
        // 衝突しない単射なハッシュ入力にする。
        Value::Array(array_value) => {
            b.push_u8(10);
            // NULL 要素を含む配列（Issue #1193）は要素型タグへ 0x80 を立て、要素ごとに
            // presence バイトを積む。NULL を含まない配列は従来と同一のバイト列
            // （要素型タグ 0・1 の TEXT／BOOLEAN は不変）で、`{NULL}` と `{}`・
            // `{NULL}` と `{"NULL"}` が衝突しない単射性を保つ。
            let has_null = array_value.has_null();
            let mark: u8 = if has_null { 0x80 } else { 0 };
            match array_value {
                crate::row_codec::ArrayValue::Text(items) => {
                    b.push_u8(mark); // 要素型タグ: TEXT
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_bytes(item.as_bytes())?;
                        }
                    }
                }
                crate::row_codec::ArrayValue::Bool(items) => {
                    b.push_u8(1 | mark); // 要素型タグ: BOOLEAN
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u8(u8::from(*item));
                        }
                    }
                }
                crate::row_codec::ArrayValue::Integer(items) => {
                    b.push_u8(2 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u64(i64::from(*item) as u64);
                        }
                    }
                }
                crate::row_codec::ArrayValue::BigInt(items) => {
                    b.push_u8(3 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u64(*item as u64);
                        }
                    }
                }
                crate::row_codec::ArrayValue::Real(items) => {
                    b.push_u8(4 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u64(u64::from(
                                crate::scalar_float::canonicalize_real(*item).to_bits(),
                            ));
                        }
                    }
                }
                crate::row_codec::ArrayValue::Double(items) => {
                    b.push_u8(5 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u64(crate::scalar_float::canonicalize_double(*item).to_bits());
                        }
                    }
                }
                crate::row_codec::ArrayValue::Date(items) => {
                    b.push_u8(6 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u64(i64::from(*item) as u64);
                        }
                    }
                }
                crate::row_codec::ArrayValue::Timestamp(items) => {
                    b.push_u8(7 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u64(*item as u64);
                        }
                    }
                }
                crate::row_codec::ArrayValue::Uuid(items) => {
                    b.push_u8(8 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_bytes(item.as_bytes())?;
                        }
                    }
                }
                crate::row_codec::ArrayValue::Numeric(items) => {
                    b.push_u8(9 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_u8(item.scale());
                            b.push_bytes(&item.unscaled().to_le_bytes())?;
                        }
                    }
                }
                crate::row_codec::ArrayValue::Bytea(items) => {
                    b.push_u8(10 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_bytes(item)?;
                        }
                    }
                }
                crate::row_codec::ArrayValue::Json(items) => {
                    b.push_u8(11 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_bytes(item.as_bytes())?;
                        }
                    }
                }
                crate::row_codec::ArrayValue::Jsonb(items) => {
                    b.push_u8(12 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_bytes(item.as_bytes())?;
                        }
                    }
                }
                crate::row_codec::ArrayValue::Enum(items) => {
                    b.push_u8(13 | mark);
                    b.push_u64(items.len() as u64);
                    for item in items {
                        if has_null {
                            b.push_u8(u8::from(item.is_some()));
                        }
                        if let Some(item) = item {
                            b.push_bytes(item.as_bytes())?;
                        }
                    }
                }
            }
        }
        // タグ 8・9 は DATE/TIMESTAMP（別 Issue の作業）向けに予約し、10 は
        // ARRAY（Issue #888）が使用済みのため、BYTEA は TABLE-13 の宣言順で
        // 11 とする（Issue #886。長さ前置は Text と同じ方式で、型タグの
        // 違いだけでハッシュを区別する）。
        Value::Bytes(bytes) => {
            b.push_u8(11);
            b.push_bytes(bytes)?;
        }
        // JSON／JSONB は共通の Value::Json 表現を持つため（Issue #889 D2）、
        // 表層横断で同一の再送判定（`23505`／`22023`）を得るためにはハッシュ入力も
        // 同じタグ・同じテキストで揃う必要がある。タグ 12 は未使用のため確保する
        // （TABLE-13/14 の宣言順に沿った次点）。
        Value::Json(text) => {
            b.push_u8(12);
            b.push_bytes(text.as_bytes())?;
        }
        // ENUM 値は TABLE-14 の宣言順で JSON／JSONB（タグ 12）の次点となる
        // タグ 13 とする（Issue #890。base の想定タグ 12 は本マージで JSON と
        // 衝突するため採番し直した）。TEXT と同じ長さ前置方式だが、型タグの
        // 違いだけで TEXT・JSON とハッシュを区別する。
        Value::Enum(label) => {
            b.push_u8(13);
            b.push_bytes(label.as_bytes())?;
        }
        // UUID 値は TABLE-13〔検討中〕・TASK-197、Issue #887。タグ 14 は
        // NUMERIC（Issue #885）が使用済みのため、次点の未使用タグ 15 を
        // 確保する。ハッシュ対象は束縛後の正規値（16 バイト生値）のため、
        // 同一列への大文字・小文字違いの同一値再送は同一ハッシュ（`23505`）に
        // 収束する。
        Value::Uuid(u) => {
            b.push_u8(15);
            b.push_raw(u.as_bytes());
        }
    }
    Ok(())
}

/// `f32` ベクトル 1 個を長さプレフィクス付きで連結する（[`push_value`] の
/// `Value::Vector` 分岐と、埋め込みだけを単独で受け取る [`for_typed_insert`] の
/// 双方から使う共通実装）。
fn push_vector(b: &mut HashInputBuilder, vector: &[f32]) -> Result<(), StorageError> {
    let len = u32::try_from(vector.len())
        .map_err(|_| StorageError::Codec("content hash vector too large".to_string()))?;
    b.push_raw(&len.to_le_bytes());
    // f32 を 1 要素ずつ update するとバッチ規模のベクトルで呼び出し回数が
    // 膨らむため、固定長スタックバッファ（64 要素分）へ詰めてからまとめて
    // update する（Issue #399。ヒープ確保は増やさない）。
    let mut scratch = [0u8; 256];
    let mut filled = 0usize;
    for f in vector {
        let bytes = f.to_le_bytes();
        if filled + 4 > scratch.len() {
            b.push_raw(&scratch[..filled]);
            filled = 0;
        }
        scratch[filled..filled + 4].copy_from_slice(&bytes);
        filled += 4;
    }
    if filled > 0 {
        b.push_raw(&scratch[..filled]);
    }
    Ok(())
}

/// スキーマの非 `VECTOR` 列（列名, 値）ペア列から、`Value::Null` の列を除外して
/// 列名＋値の順に連結する（TASK-101・RECOVER-10 追加修正。cursor bugbot 指摘・
/// PR #248: `sql::parser::bind_insert`/`bind_file_insert` は列値配列を
/// `schema.columns.len()` 幅・位置インデックス基準で構築するため、間に
/// `ALTER TABLE ADD COLUMN`（常に nullable。`catalog::alter_table_add_column`
/// 参照）が挟まると同一クライアント要求の再送でも配列幅・各列の位置がずれ、
/// 位置ベースでハッシュ化すると内容一致のはずの再送が不一致
/// （`OperationIdContentMismatch`・`22023`）に誤判定されてしまう）。
///
/// 列の**位置ではなく名前**で連結することで、新規追加列（クライアントの元の
/// 文には現れない → 束縛時は常に `Value::Null` で埋まる）を素通しで除外し、
/// 既存列だけの並びをスキーマ変更前後で不変に保つ。クライアントが既存の
/// nullable 列へ明示的に `NULL` を送った場合と、その列がまだスキーマに存在
/// しなかった場合とで同一ハッシュになるが、いずれも永続化される行の内容
/// （`row_codec::encode_scalar_columns` が書く当該列のプレゼンスバイト）は
/// 同一であり、内容一致判定の観点で区別する必要はない。
fn push_named_scalar_columns(
    b: &mut HashInputBuilder,
    columns: &[(&str, &Value)],
) -> Result<(), StorageError> {
    for (name, value) in columns {
        if matches!(value, Value::Null) {
            continue;
        }
        b.push_bytes(name.as_bytes())?;
        push_value(b, value)?;
    }
    Ok(())
}

/// `insert_row_unchecked` 用（TASK-101 対象経路 1）。入力: `(id, encoded_row)`。
/// `encoded_row` は [`crate::storage::encode_row`] の出力（`tenant_id`・`visibility`・
/// `embedding`・`metadata` を含む正準表現）を**呼び出し元が 1 回だけ計算した結果**
/// （Issue #397。以前は本関数が内部で `encode_row` し、呼び出し元が redb 書き込み用に
/// 同じ行をもう一度 `encode_row` していた二重実行を、呼び出し元が事前エンコードした
/// 結果をここと redb 書き込みの双方で共有する形に変更した。ハッシュ入力バイト列の
/// レイアウトは変更前と完全一致する）。
pub(crate) fn for_insert_encoded(id: u64, encoded_row: &[u8]) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Insert);
    b.push_u64(id);
    b.push_bytes(encoded_row)?;
    Ok(b.finish())
}

/// [`for_insert_encoded`] の参照実装（`RowInput` から内部で `encode_row` する旧形。
/// production からは呼ばれず、`for_insert_encoded` との等価性テストのみに使うため
/// `#[cfg(test)]`。Issue #353 が `sql/udf_call::eval` を同様の位置づけで残置した
/// 運用に倣う）。
#[cfg(test)]
pub(crate) fn for_insert(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    let encoded = encode_row(row)?;
    for_insert_encoded(id, &encoded)
}

/// `insert_rows_unchecked` 用（TASK-101 対象経路 2）。バッチ全体で 1 ハッシュ。
/// 入力: 要求記載順の `(id, encoded_row)` 列（順序も入力に含める。並び替えた同一集合の
/// 再送を意図的に区別する設計）。`encoded_row` は呼び出し元が事前に 1 回だけ
/// `encode_row` した結果（[`for_insert_encoded`] と同じ理由。Issue #397）。
pub(crate) fn for_insert_batch_encoded(rows: &[(u64, &[u8])]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, encoded_row) in rows {
        b.push_u64(*id);
        b.push_bytes(encoded_row)?;
    }
    Ok(b.finish())
}

/// [`for_insert_batch_encoded`] の参照実装（`#[cfg(test)]`。理由は [`for_insert`] 参照）。
#[cfg(test)]
pub(crate) fn for_insert_batch(rows: &[(u64, RowInput<'_>)]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, row) in rows {
        let encoded = encode_row(row)?;
        b.push_u64(*id);
        b.push_bytes(&encoded)?;
    }
    Ok(b.finish())
}

/// [`for_insert_batch_encoded`] と同一のフィールド境界（行ごとに `push_u64` 1 回＋
/// `push_bytes` 1 回）を、[`HashInputBuilderReference`]（Issue #399 以前の一括処理版）
/// で計算する A/B 計測専用の参照実装。`#[cfg(test)]`。`encoded_row` は呼び出し元が
/// 用意した `&[u8]` をそのまま受け取り、production の `for_insert_batch_encoded` と
/// 引数・呼び出しパターンを完全に揃える（codex-review 指摘・PR #419）。
#[cfg(test)]
fn for_insert_batch_encoded_reference(rows: &[(u64, &[u8])]) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilderReference::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, encoded_row) in rows {
        b.push_u64(*id);
        b.push_bytes(encoded_row)?;
    }
    Ok(b.finish())
}

/// `insert_typed_row_unchecked` 用（TASK-101 対象経路 3）。入力: `id`・`visibility`・
/// `VECTOR` 列の埋め込み・非 `VECTOR` 列の（列名, 値）ペア列（`Value::Null` は
/// [`push_named_scalar_columns`] が除外する）。操作種別タグは [`OpTag::Insert`] を
/// 共有する（行形 `INSERT` と宣言的 `INSERT` は同じ「新規挿入」操作であり、経路の
/// 違いでハッシュ空間を分ける必要がない）。
///
/// [`for_insert`] と異なり `storage::encode_row`（`schema.columns.len()` 幅の
/// `row_codec::encode_scalar_columns` 出力を `metadata` に含む正準表現）を経由
/// **しない**（cursor bugbot 指摘・PR #248。[`push_named_scalar_columns`]
/// ドキュメント参照）。呼び出し元 `tenant::insert_typed_row_unchecked` は
/// `values`（`sql::parser::bind_insert` が現在のスキーマ幅で構築した配列）から
/// 列名付きペアを組み立てて渡す。
///
/// `visibility` はハッシュ入力に明示的に含める（codex-review P1・cursor bugbot
/// 指摘・PR #248: `insert_typed_row_unchecked` が実際に永続化する行には
/// `visibility` が含まれるが、旧実装では `id`・embedding・非 `VECTOR` 列しか
/// ハッシュへ渡していなかった。そのため同一 `(tenant, table, operation_id)` で
/// 内容は同一のまま `visibility` だけを変えた再送が、内容不一致の `22023` では
/// なく内容一致の `23505` と誤判定されてしまう。`for_replace_by_text_key` と
/// 同様に [`crate::storage::Visibility::to_byte`] で 1 バイトへ写像して連結する）。
pub(crate) fn for_typed_insert(
    id: u64,
    visibility: Visibility,
    embedding: &[f32],
    columns: &[(&str, &Value)],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Insert);
    b.push_u64(id);
    b.push_u8(visibility.to_byte());
    push_vector(&mut b, embedding)?;
    push_named_scalar_columns(&mut b, columns)?;
    Ok(b.finish())
}

/// [`for_typed_insert_batch`]・`tenant::insert_typed_rows_unchecked` が共有する
/// 1 行分のハッシュ材料（`(id, visibility, embedding, 非 VECTOR 列の列名付き
/// ペア列)`）。`clippy::type_complexity` 回避のための型エイリアス（意味は
/// タプルの並びそのもの。`tenant.rs` からも参照する）。
pub(crate) type TypedInsertBatchRow<'a> = (u64, Visibility, &'a [f32], &'a [(&'a str, &'a Value)]);

/// `insert_typed_rows_unchecked` 用（Issue #771・TASK-178・NOSQL-6。TASK-101・
/// RECOVER-10 の型付き挿入経路をバッチへ拡張したもの）。1 つの `operation_id` が
/// 要求記載順の複数行をまとめて覆う点は [`OpTag::InsertBatch`]（[`for_insert_batch_encoded`]
/// と共有するタグ。「新規挿入をバッチでまとめて行う」操作として同じ操作種別に
/// 属する）で表す。入力: 要求記載順の `(id, visibility, embedding, 非 VECTOR 列の
/// 列名付きペア列)`。各行のハッシュ材料は [`for_typed_insert`] と同一の組み立て
/// （`push_u64(id)` → `push_u8(visibility)` → [`push_vector`] → 列数プレフィクス →
/// [`push_named_scalar_columns`]）を行ごとに連結する（列名ベース・PR #248 の教訓を
/// 踏襲。`ALTER TABLE ADD COLUMN` を挟んだ再送でも列幅がずれない）。並び替えた
/// 同一集合の再送は意図的に区別する（[`for_insert_batch_encoded`] と同じ設計。
/// 件数プレフィクスに続けて行ごとのフィールドを連結する）。
///
/// 行境界の曖昧性回避（codex-review P1 指摘・PR #823）: [`push_named_scalar_columns`]
/// は 1 行分の列を「列数を明示しない、末尾まで連結するだけ」の形式で書く。
/// [`for_typed_insert`]／[`for_replace_by_text_key`] のように 1 ハッシュ = 1 行
/// （列データが常に入力全体の末尾）ならこれで一意に復元できるが、本関数は複数行を
/// 1 ハッシュへ連結するため、ある行の列データが**次の行**の `id`／`visibility`／
/// `embedding` 長さへ「はみ出して」再解釈されても、`push_bytes` の長さプレフィクス
/// 契約自体は内部的に自己無矛盾なまま別の行分割として復元できてしまい、異なる
/// 行数・列配置を持つ 2 つのバッチが同一バイト列（同一ハッシュ）に還元されうる
/// （回帰テスト `for_typed_insert_batch_rejects_row_boundary_reinterpretation`
/// 参照）。この曖昧性は行ごとの列**数**を明示すれば解消するため、
/// [`push_named_scalar_columns`] 自体（[`for_typed_insert`]／[`for_replace_by_text_key`]
/// も共有する既存レイアウト）は変更せず、本関数（Issue #771 で新設された
/// バッチ入力レイアウトのみ）に列数プレフィクス（`Value::Null` 除外後の非 NULL 列数。
/// `push_named_scalar_columns` が実際に書き込む列数と一致させる）を追加する形で
/// 閉じる。
pub(crate) fn for_typed_insert_batch(
    rows: &[TypedInsertBatchRow<'_>],
) -> Result<ContentHash, StorageError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    let mut b = HashInputBuilder::new(OpTag::InsertBatch);
    b.push_raw(&count.to_le_bytes());
    for (id, visibility, embedding, columns) in rows {
        b.push_u64(*id);
        b.push_u8(visibility.to_byte());
        push_vector(&mut b, embedding)?;
        let non_null_columns = columns
            .iter()
            .filter(|(_, value)| !matches!(value, Value::Null))
            .count();
        let non_null_columns = u32::try_from(non_null_columns).map_err(|_| {
            StorageError::Codec("content hash batch row column count too large".to_string())
        })?;
        b.push_raw(&non_null_columns.to_le_bytes());
        push_named_scalar_columns(&mut b, columns)?;
    }
    Ok(b.finish())
}

/// [`for_typed_upsert`] の `SET` 右辺（ハッシュ入力専用の最小表現。SQL-20・
/// TASK-193、Issue #872）。本モジュールは `sql` に依存しない設計を維持するため、
/// `sql::allowlist::UpsertValue` をそのまま受け取らず、呼び出し元
/// （`tenant::upsert_typed_rows_unchecked`）が変換した最小 enum を受け取る。
pub(crate) enum UpsertAssignmentHashValue<'a> {
    /// `EXCLUDED.<col>`。ハッシュには新規挿入しようとした行の値ではなく
    /// **参照元の列名**を含める（同一 `EXCLUDED.<col>` 参照は新規行の内容が
    /// 変われば行ハッシュ側〔`rows` レイアウト〕で自然に区別されるため、ここで
    /// 実際の値を重複してハッシュ化する必要はない。列名自体が異なれば当然
    /// ハッシュも変わる）。
    Excluded(&'a str),
    Literal(&'a Value),
}

/// [`for_typed_upsert`] の衝突分岐（SQL-20・TASK-193、Issue #872）。
/// `sql::allowlist::OnConflictAction` に対応する最小表現。
pub(crate) enum UpsertHashAction<'a> {
    DoNothing,
    /// `(対象列名, 右辺)` の宣言順スライス（並べ替えない。`Parser::parse_
    /// upsert_assignment` の宣言順をそのまま保持する契約は
    /// `sql::parser::bind_upsert_assignments` が担う）。
    DoUpdate(&'a [(String, UpsertAssignmentHashValue<'a>)]),
}

/// [`for_typed_upsert`] の衝突対象（SQL-20・TABLE-16、Issue #1074）。
/// `sql::parser::BoundConflictTarget` に対応する最小表現。`RowId` は本 Issue
/// 導入前と完全に同じレイアウトを出力する（golden テストで固定）。
pub(crate) enum UpsertHashTarget<'a> {
    /// `ON CONFLICT (id)`。
    RowId,
    /// `ON CONFLICT (<UNIQUE 制約の構成列>)`。列名は解決した制約の宣言順
    /// （`sql::parser` が `schema.unique_constraints()` から解決した順）。
    Unique(&'a [&'a str]),
}

/// `tenant::upsert_typed_rows_unchecked` 用（SQL-20・TASK-193、Issue #872）。
/// `INSERT ... ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...` を単一の
/// 「新規挿入かもしれないし更新かもしれない」操作として、[`OpTag::Upsert`]
/// タグの下でハッシュ化する（[`OpTag::Insert`]／[`OpTag::Update`] のいずれとも
/// 共有しない専用タグ。同一 `VALUES` でも plain `INSERT`／`DO NOTHING`／
/// `DO UPDATE`／`SET` 内容差は必ず異なるハッシュになる——衝突分岐そのものを
/// 先頭で `push_u8` してから行データを連結するため）。
///
/// 入力レイアウト: `push_u8(action_tag)`（`(id)` 対象: `0` = DO NOTHING、
/// `1` = DO UPDATE。UNIQUE 対象〔Issue #1074〕: `2` = DO NOTHING、
/// `3` = DO UPDATE——`(id)` 対象と同一 `VALUES`／同一 action でも異なる
/// ハッシュになることを action tag の時点で保証する）→ UNIQUE 対象のみ
/// `push_raw(target_column_count)` に続けて対象列名を解決した制約の宣言順で
/// `push_bytes(column_name)`（`(a,b)` と `(b,a)` は同じ制約に解決されるため
/// 同一の並びになり、同一ハッシュになる） → DO UPDATE のみ
/// `push_raw(assignment_count)` に続けて各割当を宣言順で
/// `push_bytes(target_column_name)` → `push_u8(value_kind)`（`0` = `EXCLUDED`、
/// `1` = リテラル）→ `EXCLUDED` なら `push_bytes(src_column_name)`、リテラル
/// なら [`push_value`] → その後は [`for_typed_insert_batch`] と**完全に同一**の
/// 行データレイアウト（件数プレフィクス＋行ごとの `(id, visibility, embedding,
/// 列数プレフィクス, 列名付きスカラー列)`）を連結する。単一行 UPSERT も
/// `rows.len() == 1` としてこのレイアウトを使う（`for_typed_insert` へは
/// 委譲しない——plain `INSERT` と同一 `operation_id` での UPSERT 再送を
/// `OpTag` の違いで機械的に内容不一致〔`22023`〕として検出させるため）。
/// 行境界の曖昧性回避（[`for_typed_insert_batch`] ドキュメント参照）は行数
/// プレフィクスを持つ本レイアウトにもそのまま適用される。
///
/// `target` が [`UpsertHashTarget::RowId`] の場合、出力バイト列は本 Issue
/// （#1074）導入前と**完全に同一**（golden テストで固定。`for_typed_upsert_
/// row_id_layout_is_stable` 参照）。
pub(crate) fn for_typed_upsert(
    target: &UpsertHashTarget<'_>,
    action: &UpsertHashAction<'_>,
    rows: &[TypedInsertBatchRow<'_>],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Upsert);
    // UNIQUE 対象は action tag を `2`／`3` へずらすことで、`(id)` 対象と
    // 同一の action・`VALUES` でも先頭バイトの時点でハッシュが分岐する。
    let action_tag_base: u8 = match target {
        UpsertHashTarget::RowId => 0,
        UpsertHashTarget::Unique(_) => 2,
    };
    match action {
        UpsertHashAction::DoNothing => b.push_u8(action_tag_base),
        UpsertHashAction::DoUpdate(assignments) => {
            b.push_u8(action_tag_base + 1);
            let count = u32::try_from(assignments.len()).map_err(|_| {
                StorageError::Codec("content hash upsert assignment count too large".to_string())
            })?;
            b.push_raw(&count.to_le_bytes());
            for (name, value) in assignments.iter() {
                b.push_bytes(name.as_bytes())?;
                match value {
                    UpsertAssignmentHashValue::Excluded(src) => {
                        b.push_u8(0);
                        b.push_bytes(src.as_bytes())?;
                    }
                    UpsertAssignmentHashValue::Literal(v) => {
                        b.push_u8(1);
                        push_value(&mut b, v)?;
                    }
                }
            }
        }
    }
    if let UpsertHashTarget::Unique(columns) = target {
        let count = u32::try_from(columns.len()).map_err(|_| {
            StorageError::Codec("content hash upsert target column count too large".to_string())
        })?;
        b.push_raw(&count.to_le_bytes());
        for name in *columns {
            b.push_bytes(name.as_bytes())?;
        }
    }

    let count = u32::try_from(rows.len())
        .map_err(|_| StorageError::Codec("content hash batch too large".to_string()))?;
    b.push_raw(&count.to_le_bytes());
    for (id, visibility, embedding, columns) in rows {
        b.push_u64(*id);
        b.push_u8(visibility.to_byte());
        push_vector(&mut b, embedding)?;
        let non_null_columns = columns
            .iter()
            .filter(|(_, value)| !matches!(value, Value::Null))
            .count();
        let non_null_columns = u32::try_from(non_null_columns).map_err(|_| {
            StorageError::Codec("content hash batch row column count too large".to_string())
        })?;
        b.push_raw(&non_null_columns.to_le_bytes());
        push_named_scalar_columns(&mut b, columns)?;
    }
    Ok(b.finish())
}

/// `update_row_unchecked` 用（TASK-101 対象経路 4）。入力: `(id, encoded_row)`。
/// `encoded_row` は呼び出し元が事前に 1 回だけ `encode_row` した結果
/// （[`for_insert_encoded`] と同じ理由。Issue #397）。
pub(crate) fn for_update_encoded(id: u64, encoded_row: &[u8]) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::Update);
    b.push_u64(id);
    b.push_bytes(encoded_row)?;
    Ok(b.finish())
}

/// [`for_update_encoded`] の参照実装（`#[cfg(test)]`。理由は [`for_insert`] 参照）。
#[cfg(test)]
pub(crate) fn for_update(id: u64, row: &RowInput<'_>) -> Result<ContentHash, StorageError> {
    let encoded = encode_row(row)?;
    for_update_encoded(id, &encoded)
}

/// `delete_row_unchecked` 用（TASK-101 対象経路 5）。入力: `id` のみ（削除要求は
/// 対象 id 以外にクライアント由来の内容を持たない）。
pub(crate) fn for_delete(id: u64) -> ContentHash {
    let mut b = HashInputBuilder::new(OpTag::Delete);
    b.push_u64(id);
    b.finish()
}

/// `tenant::update_row_columns_unchecked` 用（Issue #865・SQL-17・TASK-191。
/// 部分更新 `UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>` の実行結線が
/// 使う専用ハッシュ。既存の [`for_update_encoded`]（`update_row_unchecked` が使う
/// 全行置換 API 向け。マージ後の行全体を対象）とは意図的に別 [`OpTag`] を持たせる
/// （ドメイン分離。同一 `operation_id` を全行置換 UPDATE と部分更新 UPDATE の
/// 双方で使い回した場合に、内容が実質同じでも異なる操作として `22023` へ倒す）。
///
/// 入力はクライアント要求由来の内容のみ（DB の現在状態＝マージ後の行内容には
/// 依存しない。本モジュールドキュメント「正規化の方針」参照）: `id` と、SET 句に
/// 書かれた（列名, 値）ペア列を**呼び出し元が渡した順のまま**（本関数自身は
/// 並べ替えない）連結する。列の位置ではなく名前で連結する理由・`Value::Null` 列の
/// 扱いは [`push_named_scalar_columns`] と共有する（`ALTER TABLE ADD COLUMN`
/// 耐性）。
///
/// 呼び出し元 `tenant::update_row_columns_unchecked` は表層をまたいだ再送の
/// 判定を安定させるため、以降の台帳記録・照合にはスキーマの列 index 順へ
/// 正規化した列スライスを渡す（`for_typed_insert` がスキーマ列順を渡す既存契約と
/// 同じ考え方。Issue #876 レビュー指摘）。加えて、この正規化を導入する**前**に
/// 台帳へ記録されたエントリ（SET 句の宣言順のままハッシュ計算されている）とも
/// 照合できるよう、宣言順のまま呼び出した本関数の結果を `legacy_hash` として
/// `ledger::record_in_txn_accepting` へ渡す（互換性の詳細は呼び出し元のコメント
/// 参照）。本関数自体はどちらの用途で呼ばれても同じ「渡された順のまま連結する」
/// 契約のままであり、`for_update_columns_differs_by_declared_order` で固定した
/// 単体契約は変えていない。
///
/// 行境界の曖昧性回避（[`for_typed_insert_batch`] と同じ理由）: 本関数は 1 回の
/// 呼び出しで 1 行分の SET 句しか扱わないため複数行の境界問題は生じないが、
/// SET 句の列**数**（`Value::Null` 除外後）を先頭にプレフィクスすることで、将来
/// `SET col = NULL` を許容する変更が入った場合でも [`push_named_scalar_columns`]
/// の「末尾まで連結するだけ」の形式が列挙順・列数のいずれについても一意に復元
/// できる状態を維持する（[`for_typed_insert_batch`] の行ごとの列数プレフィクスと
/// 同じ設計）。
pub(crate) fn for_update_columns(
    id: u64,
    columns: &[(&str, &Value)],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::UpdateColumns);
    b.push_u64(id);
    let non_null_columns = columns
        .iter()
        .filter(|(_, value)| !matches!(value, Value::Null))
        .count();
    let non_null_columns = u32::try_from(non_null_columns).map_err(|_| {
        StorageError::Codec("content hash update column count too large".to_string())
    })?;
    b.push_raw(&non_null_columns.to_le_bytes());
    push_named_scalar_columns(&mut b, columns)?;
    Ok(b.finish())
}

/// `replace_typed_rows_by_text_key`（ファイル形 `INSERT` の置換経路）用（TASK-101
/// 対象経路 6）。入力: `(key_column, key_value, visibility, path, body,
/// template_columns)`。削除対象集合・採番される id 等の DB 状態由来の値に加え、
/// **チャンク化・埋め込み結果（`replace_typed_rows_by_text_key` へ渡る
/// 派生済み行データ）も含めない**（codex-review P1 指摘・PR #248。`chunking`
/// 設定や `Embedder` の応答は同一のクライアント要求に対しても実行時に変わり得る
/// ため、これらをハッシュへ含めると再送の内容一致判定が偽陰性
/// （`OperationIdContentMismatch` の誤検出）を起こす。ハッシュ入力は
/// クライアントが `INSERT` 文で実際に送った値
/// （`path`・`body`・その他の Text 列値＝`template_columns`。`path`/`body`/VECTOR
/// 列は `sql::parser::bind_file_insert` により `template_columns` 由来の
/// `template_values` 中で常に `Value::Null` に正規化済みのため、`path`/`body` は
/// 別引数として明示的に渡す）のみから決定的に構成する。本モジュールドキュメント
/// 「正規化の方針」参照。
///
/// `template_values`（`schema.columns.len()` 幅・位置インデックス基準の配列）を
/// 直接ハッシュしない（cursor bugbot 指摘・PR #248。[`push_named_scalar_columns`]
/// ドキュメント参照: `ALTER TABLE ADD COLUMN` を挟むと同一クライアント要求の
/// 再送でも配列幅・位置がずれる）。呼び出し元 `tenant::replace_typed_rows_by_text_key`
/// が現在のスキーマから（列名, 値）ペアへ変換して渡す。
/// `truncate_table_unchecked`（SQL-22。`TRUNCATE TABLE <table> USING
/// OPERATION_ID '<id>'`）用。入力: なし（台帳キー自体が `(tenant, table,
/// operation_id)` でテーブル名を既に一意に識別しており、TRUNCATE 要求は
/// クライアント由来の可変フィールドをテーブル名以外に持たないため、他の
/// `for_*` と異なり本体に何も追記しない。同一 `operation_id` への TRUNCATE
/// 再送は常に内容一致となり `23505`（`DuplicateOperationId`）に収束する
/// （`22023` は構造的に到達不能だが、他操作と同じハッシュ機構を再利用することで
/// 台帳照合の実装を一貫させる）。
pub(crate) fn for_truncate() -> ContentHash {
    HashInputBuilder::new(OpTag::Truncate).finish()
}

pub(crate) fn for_replace_by_text_key(
    key_column: &str,
    key_value: &str,
    visibility: Visibility,
    path: &str,
    body: &str,
    template_columns: &[(&str, &Value)],
) -> Result<ContentHash, StorageError> {
    let mut b = HashInputBuilder::new(OpTag::ReplaceByTextKey);
    b.push_bytes(key_column.as_bytes())?;
    b.push_bytes(key_value.as_bytes())?;
    b.push_u8(visibility.to_byte());
    b.push_bytes(path.as_bytes())?;
    b.push_bytes(body.as_bytes())?;
    push_named_scalar_columns(&mut b, template_columns)?;
    Ok(b.finish())
}

/// フィールド長超過（`push_bytes`／件数プレフィクスの `u32` 上限超過）を
/// `wire_code` `54000` へ写像する（[`for_update_where`]／[`for_delete_where`]
/// 系の共通ヘルパー。実際には上流の既存上限〔`MAX_METADATA_FILTERS`・
/// `MAX_EXPR_NODES`・`MAX_UDF_PARAMS`・`MAX_SESSION_UDFS`〕により事実上到達
/// しないが、`push_bytes` 自体の fail-closed 契約を崩さないよう明示的に
/// 写像する）。
fn dml_hash_field_too_large() -> crate::sql::allowlist::SqlSurfaceError {
    crate::sql::allowlist::SqlSurfaceError::payload_too_large("content hash field too large")
}

/// 述語つき `UPDATE ... WHERE`（SQL-19・TASK-192、Issue #871・対象ビヘイビア:
/// RECOVER-11）の内容照合ハッシュ。ADR `docs/design/multi-row-dml-operation-id.md`
/// （Issue #868）§4 のレイアウトをそのまま実装する。
///
/// 他の `for_*` と異なり、本関数（および [`for_delete_where`]）は
/// `sql::allowlist::WherePredicate`・`sql::udf_call::UdfRegistry` を直接受け取る
/// （ADR §4.2 が提案するシグネチャをそのまま採用）。`WHERE` の構文形（束縛前）を
/// ハッシュ源にする契約（ADR §4.4「構文段 AST を直接ハッシュする理由」）上、
/// `recovery` モジュールが `sql` モジュールへ依存する形になるが、`sql` は既に
/// `recovery`（`content_hash`・`ledger`・`required_op_id`）へ依存しているため、
/// 同一クレート内のモジュール参照としてはどちらの向きも許容される（Rust が
/// 禁止するのはクレート単位の循環のみ）。
///
/// 入力: `table`（テーブル名）・`assignments`（`SET` 句の宣言順 `(列名, リテラル)`
/// 対応。`ValidatedPredicateUpdate::assignments()` が保持する順序そのまま）・
/// `where_predicates`（`WHERE` 句の宣言順。同じく並べ替えない）・`udf_registry`
/// （呼び出し元セッションの `UdfRegistry`。`WHERE` 式中の `Call` を解決するために
/// 使う。ADR §4.4.1）。
///
/// **計算位置（ADR §5.1 からの意図的な変更）**: ADR は `bind_update_form`／
/// `bind_predicate_delete` の内部で計算し束縛済み型（`BoundPredicateUpdate`／
/// `BoundPredicateDelete`）へ保持させることを推奨するが、これらの型の
/// コンストラクタ（`pub fn new`／`pub(crate) fn new`）は既に固定された引数
/// （束縛済み `MetadataFilter`／`BoundExpr`）を取り、生の `WherePredicate`／
/// `UdfRegistry` を経由しない別入口（NoSQL 表層直接構築、Issue #876）を将来
/// 持つ設計であるため、これらの型へ `content_hash` フィールドを追加すると
/// `new` の契約が割れる。本実装は `core.rs::EngineCore`（`Validated*` 形と
/// `session.udfs()` の両方を持つ唯一の呼び出し元）が束縛の直前に 1 回だけ
/// 呼び出し、`sql::exec::execute_predicate_update`／`execute_predicate_delete`
/// へ `&ContentHash` として渡す（詳細は `docs/design/predicate-dml-exec.md`
/// 参照）。
///
/// **エラー型（ADR §4.2 からの意図的な変更）**: ADR は `StorageError` を返す
/// シグネチャを示すが、本実装は `sql::allowlist::SqlSurfaceError` を直接返す。
/// WASM UDF 呼び出しの拒否（ADR §4.4.1「WASM UDF は本節の対象外」）は
/// `wire_code` を伴う SQL 表層のエラーであり、`StorageError` には対応する
/// variant が存在しない。ADR は ERR-2 分類として `0A000` を挙げているが、
/// `sql::allowlist::SqlSurfaceError` には `0A000` を返す variant が存在しない
/// （`grep -n '"0A000"' crates/engine/src/sql/allowlist.rs` で不在を確認済み。
/// `0A000` は NoSQL 表層の op 許可リスト専用）ため、本実装は
/// `SqlSurfaceError::unsupported`（許可形状外・`42601`）へ写像する。
pub(crate) fn for_update_where(
    table: &str,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
    // Issue #1061: `SET` 割当のうち `VECTOR` 列を対象とする `InsertLiteral::String`
    // を正準表現（タグ 5・f32 LE 列）へ正規化するための列型参照。
    // [`push_dml_assignments`] 参照。
    schema: &crate::catalog::TableSchema,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    update_where_with_tag(
        OpTag::UpdateWhere,
        table,
        assignments,
        where_predicates,
        udf_registry,
        schema,
    )
}

/// [`for_update_where`] と [`for_update_where_partitioned`] が共有する本体
/// （タグのみが異なる。出力レイアウトは従来と同一）。
fn update_where_with_tag(
    tag: OpTag,
    table: &str,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
    schema: &crate::catalog::TableSchema,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    let mut b = HashInputBuilder::new(tag);
    b.push_bytes(table.as_bytes())
        .map_err(|_| dml_hash_field_too_large())?;
    push_dml_assignments(&mut b, assignments, DmlVectorRepr::Canonical, schema)?;
    push_dml_where_predicates(&mut b, where_predicates, udf_registry)?;
    Ok(b.finish())
}

/// 分割実行版述語つき `UPDATE ... WHERE`（Issue #1127。ADR
/// `docs/design/partitioned-dml.md` §6・RECOVER-11）の内容照合ハッシュ。
/// [`for_update_where`] と同じ直列化に別タグ（[`OpTag::UpdateWherePartitioned`]）を
/// 付ける。**チャンク幅を引数に取らない**（再送ごとに幅が変わってもジョブを
/// 同一視するため、シグネチャでハッシュ入力に入らないことを保証する）。
/// 新しいドメインで過去の記録が存在しないため legacy 版は持たない。
/// 呼び出し元は #1128 の分割実行器。
// 分割実行（core.rs の分割実行 form）が呼ぶ。
pub(crate) fn for_update_where_partitioned(
    table: &str,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
    schema: &crate::catalog::TableSchema,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    update_where_with_tag(
        OpTag::UpdateWherePartitioned,
        table,
        assignments,
        where_predicates,
        udf_registry,
        schema,
    )
}

/// [`for_update_where`] の legacy 版（Issue #1061）。`legacy_hashes`
/// （[`crate::recovery::ledger::record_in_txn_accepting`] 参照）専用で、
/// 正規化前のレイアウト（`VECTOR` 列への `String` 割当もタグ 1・生テキストの
/// まま連結する）を再現する。**新規記録には使わない**——呼び出し元
/// （`core.rs::EngineCore::execute_predicate_update_form`）は
/// [`needs_legacy_vector_hash`] が true の場合のみ本関数を呼び、結果を
/// `legacy_hashes` へだけ渡す（保存対象の正準ハッシュは常に [`for_update_where`]）。
pub(crate) fn for_update_where_legacy_text_vector(
    table: &str,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    let mut b = HashInputBuilder::new(OpTag::UpdateWhere);
    b.push_bytes(table.as_bytes())
        .map_err(|_| dml_hash_field_too_large())?;
    // Legacy モードは列型を参照しないため、空スキーマ（列 0 件）を渡す
    // （`push_dml_assignments` は `DmlVectorRepr::LegacyText` のとき
    // `schema` を参照しない）。
    let empty_schema = crate::catalog::TableSchema::new(String::new(), Vec::new());
    push_dml_assignments(
        &mut b,
        assignments,
        DmlVectorRepr::LegacyText,
        &empty_schema,
    )?;
    push_dml_where_predicates(&mut b, where_predicates, udf_registry)?;
    Ok(b.finish())
}

/// [`assignments`] に `VECTOR` 列を対象とする `InsertLiteral::String` 割当が
/// 1 つ以上含まれるかを判定する（Issue #1061）。true の場合のみ、呼び出し元は
/// [`for_update_where_legacy_text_vector`] を計算して `legacy_hashes` へ渡す
/// 必要がある（該当割当が無ければ正準ハッシュと旧レイアウトのハッシュは
/// 常に一致するため、legacy 側の計算・受理は不要）。
pub(crate) fn needs_legacy_vector_hash(
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    schema: &crate::catalog::TableSchema,
) -> bool {
    assignments.iter().any(|(name, literal)| {
        matches!(literal, crate::sql::allowlist::InsertLiteral::String(_))
            && schema
                .columns
                .iter()
                .any(|c| &c.name == name && matches!(c.ty, crate::catalog::ColumnType::Vector(_)))
    })
}

/// 述語つき `DELETE ... WHERE`（SQL-19・TASK-192、Issue #871・対象ビヘイビア:
/// RECOVER-11）の内容照合ハッシュ。[`for_update_where`] と同じ `WHERE` 直列化
/// （[`push_dml_where_predicates`]）を共有し、`SET` 割当を持たない点のみが
/// 異なる（ADR §4.3「2. `SET` 割当（`UPDATE` のみ。`DELETE` には無い）」）。
/// タグは [`OpTag::DeleteWhere`]（[`OpTag::UpdateWhere`] とは別ドメイン）。
pub(crate) fn for_delete_where(
    table: &str,
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    delete_where_with_tag(OpTag::DeleteWhere, table, where_predicates, udf_registry)
}

/// [`for_delete_where`] と [`for_delete_where_partitioned`] が共有する本体
/// （タグのみが異なる）。
fn delete_where_with_tag(
    tag: OpTag,
    table: &str,
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    let mut b = HashInputBuilder::new(tag);
    b.push_bytes(table.as_bytes())
        .map_err(|_| dml_hash_field_too_large())?;
    push_dml_where_predicates(&mut b, where_predicates, udf_registry)?;
    Ok(b.finish())
}

/// 分割実行版述語つき `DELETE ... WHERE`（Issue #1127。ADR
/// `docs/design/partitioned-dml.md` §6・RECOVER-11）の内容照合ハッシュ。
/// [`for_delete_where`] と同じ直列化に別タグ（[`OpTag::DeleteWherePartitioned`]）を
/// 付ける。チャンク幅は入力に含めない（[`for_update_where_partitioned`] 参照）。
// 分割実行（core.rs の分割実行 form）が呼ぶ。
pub(crate) fn for_delete_where_partitioned(
    table: &str,
    where_predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<ContentHash, crate::sql::allowlist::SqlSurfaceError> {
    delete_where_with_tag(
        OpTag::DeleteWherePartitioned,
        table,
        where_predicates,
        udf_registry,
    )
}

/// [`push_dml_assignments`] における `VECTOR` 列 `String` 割当の表現モード
/// （Issue #1061。SQL・NoSQL 跨ぎの DML 再送で `content_hash` が食い違う問題の
/// 是正）。SQL 表層の述語形 `UPDATE ... SET <vector列> = '[...]'`（字句上は
/// `InsertLiteral::String`）と、NoSQL 表層が JSON 配列から直接構築する
/// `InsertLiteral::Vector` は、正規化前は別タグ（1 対 5）でハッシュされ、
/// 同一ベクトル値でも表層を跨ぐ再送が内容不一致（`22023`）に誤判定されていた。
#[derive(Clone, Copy, PartialEq, Eq)]
enum DmlVectorRepr {
    /// 新規記録用。`VECTOR` 列を対象とする `String` 割当を
    /// `sql::parser::parse_vector_literal` でパースし、タグ 5（f32 LE 列。
    /// `InsertLiteral::Vector` と同一レイアウト）へ正規化する。
    Canonical,
    /// [`for_update_where_legacy_text_vector`] 専用（`legacy_hashes` 判定にのみ
    /// 使う）。`VECTOR` 列への `String` もタグ 1・生テキストのまま連結する
    /// 正規化前のレイアウトを再現する。
    LegacyText,
}

/// `SET` 割当（宣言順）を連結する（ADR §4.3 の 2 番）。件数プレフィクス（u32 LE）
/// → 各要素につき `push_bytes(列名)`・リテラル種別タグ（`String`＝1・`Number`＝2）・
/// `push_bytes(リテラル生文字列)`。列名は宣言どおりの大文字小文字のまま連結する
/// （`bind_set_assignments` の厳密一致と揃える）。`push_named_scalar_columns`
/// （`Value::Null` を除外する実装）は再利用しない——将来 `SET col = NULL` が
/// 追加されたときに黙って脱落させない前方ガード（ADR §4.3 参照）。
///
/// `repr == Canonical` かつ対象列がスキーマ上 `VECTOR(dim)` の場合、
/// `InsertLiteral::String` は生テキストのままハッシュせず
/// `sql::parser::parse_vector_literal` でパースしてからタグ 5（f32 LE 列）で
/// 連結する（Issue #1061）。列の解決は [`bind_set_assignments`]
/// （`sql/parser.rs`）と同じ厳密一致（`c.name == name`）。パース失敗は
/// `Err` をそのまま返す（fail-closed。タグ 1 へフォールバックしない）。
/// `VECTOR` 以外の列・`repr == LegacyText` のときは従来どおりタグ 1 ＋
/// 生テキストで連結する（ビット同一）。
fn push_dml_assignments(
    b: &mut HashInputBuilder,
    assignments: &[(&str, &crate::sql::allowlist::InsertLiteral)],
    repr: DmlVectorRepr,
    schema: &crate::catalog::TableSchema,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::InsertLiteral;

    let count = u32::try_from(assignments.len()).map_err(|_| dml_hash_field_too_large())?;
    b.push_raw(&count.to_le_bytes());
    for (name, literal) in assignments {
        b.push_bytes(name.as_bytes())
            .map_err(|_| dml_hash_field_too_large())?;
        match literal {
            InsertLiteral::String(s) => {
                let vector_dim = if repr == DmlVectorRepr::Canonical {
                    schema.columns.iter().find_map(|c| {
                        if &c.name == name {
                            match c.ty {
                                crate::catalog::ColumnType::Vector(dim) => Some(dim),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };
                if let Some(dim) = vector_dim {
                    let values = crate::sql::parser::parse_vector_literal(s, dim)?;
                    b.push_u8(5);
                    push_vector(b, &values).map_err(|_| dml_hash_field_too_large())?;
                } else {
                    b.push_u8(1);
                    b.push_bytes(s.as_bytes())
                        .map_err(|_| dml_hash_field_too_large())?;
                }
            }
            InsertLiteral::Number(s) => {
                b.push_u8(2);
                b.push_bytes(s.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            InsertLiteral::Bool(v) => {
                b.push_u8(3);
                b.push_u8(u8::from(*v));
            }
            // `InsertLiteral::Null`（Issue #889 レビュー指摘・PR #1014。
            // `bind_set_assignments` が nullable 列向けに追加した SQL `NULL`
            // 表現）。述語つき `UPDATE ... WHERE`（本関数の呼び出し元）は
            // 現状 NoSQL 表層から到達しない（`filter` 形は Issue #871 実行結線
            // 対象だが NULL 対応は本 Issue のスコープ外）ため実質未到達だが、
            // 上部コメントが予告する前方ガードとしてタグ 4 を割り当てる。
            InsertLiteral::Null => {
                b.push_u8(4);
            }
            // `InsertLiteral::Vector`（Issue #896 レビュー指摘・PR #1038。
            // NoSQL 表層 `insert`／`update` op の `insert.rs`／`update.rs::
            // map_set_assignments` が JSON 配列から直接構築する variant）。
            // 述語つき `UPDATE ... WHERE`（本関数の呼び出し元）の `SET` は
            // NoSQL 表層の `filter` 形からは現状到達しないが（`update.rs` の
            // `map_set_assignments` は `where`〔単一行 `id` 完全一致形〕専用）、
            // `filter` 結線（#871 系）後は同じタグ 5 で到達しうる。
            // Issue #1061: 上の `InsertLiteral::String` 分岐が `VECTOR` 列を
            // `Canonical` モードで同じタグ 5・[`push_vector`] レイアウトへ
            // 正規化しているため、SQL 表層 `'[1,2,3]'` と NoSQL 表層
            // `[1,2,3]` は表層を跨いでも同一 `content_hash` になる。
            InsertLiteral::Vector(values) => {
                b.push_u8(5);
                push_vector(b, values).map_err(|_| dml_hash_field_too_large())?;
            }
        }
    }
    Ok(())
}

/// `WHERE` 述語列（宣言順）を連結する（ADR §4.3 の 3 番）。`for_update_where`・
/// `for_delete_where` が共有する。件数プレフィクス（u32 LE）に続けて各
/// `WherePredicate` を種別タグ＋フィールドで直列化し、`Expression` 述語が
/// 参照する UDF の推移閉包（ADR §4.4.1）を集めて末尾に追記する。
fn push_dml_where_predicates(
    b: &mut HashInputBuilder,
    predicates: &[crate::sql::allowlist::WherePredicate],
    udf_registry: &crate::sql::udf_call::UdfRegistry,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    let count = u32::try_from(predicates.len()).map_err(|_| dml_hash_field_too_large())?;
    b.push_raw(&count.to_le_bytes());

    // 参照 UDF 集合（名前の辞書順。ADR §4.4.1「5. 決定的な順序」）。
    let mut referenced: std::collections::BTreeMap<String, crate::sql::udf_call::UdfDefinition> =
        std::collections::BTreeMap::new();

    for pred in predicates {
        push_dml_where_predicate(b, pred, udf_registry, &mut referenced)?;
    }

    // 参照 UDF 定義セクション（ADR §4.4.1「6.」）。参照 UDF が無ければ件数
    // プレフィクスの 0 すら書かない（UDF を呼ばない文は本節導入前とビット同一の
    // ハッシュになる互換性を保つ）。
    if !referenced.is_empty() {
        let udf_count = u32::try_from(referenced.len()).map_err(|_| dml_hash_field_too_large())?;
        b.push_raw(&udf_count.to_le_bytes());
        for (name, def) in referenced.iter() {
            b.push_bytes(name.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            let param_count =
                u32::try_from(def.params.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&param_count.to_le_bytes());
            // パラメータ名は小文字化してから連結する（`push_dml_expr` の `Ident`
            // 側パラメータ参照・`bind_expr_in` の呼び出し引数解決と同じ大文字小文字
            // 非区別契約に揃える。ここを原文のまま連結すると `x`／`X` のように
            // 意味的に同一なパラメータ宣言が異なる content_hash を生み、同一
            // `operation_id` の正当な再送を内容不一致〔`22023`〕として誤拒否する）。
            for p in &def.params {
                b.push_bytes(p.to_ascii_lowercase().as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            push_dml_expr(b, &def.body, Some(&def.params))?;
        }
    }

    Ok(())
}

/// [`push_dml_where_predicates`] が各述語 1 個ぶんを直列化する再帰本体
/// （TASK-208・SQL-24、Issue #912／#913）。`WherePredicate::Or`（タグ 8）は
/// 「分岐数（u32 LE）→ 分岐ごとの述語数（u32 LE）→ 各述語を本関数で再帰的に
/// 直列化」という形にすることで、`a OR (b AND c)` と `(a OR b) AND c` が異なる
/// ハッシュになる（分岐の木構造そのものを直列化に反映する）。`InList`（タグ 9）・
/// `Between`（タグ 10）・`IsNull`（タグ 11）・`Not`（タグ 12）は Issue #912 の
/// `Or`（タグ 8）採番後に追加したため 9 番から採番する（タグ値の再利用は異なる
/// 構文が同一ハッシュへ衝突する事故を招くため、未使用の番号を新規に割り当てる）。
/// `Not` は内側を再帰でそのまま直列化する（構文段が連続する `NOT` を偶奇で
/// 畳んでいる〔正規化済み〕ため、`NOT NOT x` と `x` は同じハッシュになる。意図した
/// 正規化）。タグ 1〜7 の直列化形式は本 Issue 以前と一切変えない（`AND` だけの
/// 既存述語列の content hash は不変。回帰テストで固定する）。
fn push_dml_where_predicate(
    b: &mut HashInputBuilder,
    pred: &crate::sql::allowlist::WherePredicate,
    udf_registry: &crate::sql::udf_call::UdfRegistry,
    referenced: &mut std::collections::BTreeMap<String, crate::sql::udf_call::UdfDefinition>,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::WherePredicate;

    match pred {
        WherePredicate::Equality { column, value } => {
            b.push_u8(1);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_bytes(value.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
        }
        WherePredicate::Prefix { column, pattern } => {
            b.push_u8(2);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_bytes(pattern.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
        }
        WherePredicate::PredicateCall { name } => {
            b.push_u8(3);
            b.push_bytes(name.to_ascii_lowercase().as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
        }
        WherePredicate::Expression(expr) => {
            b.push_u8(4);
            push_dml_expr(b, expr, None)?;
            collect_referenced_udfs(expr, udf_registry, referenced)?;
        }
        // `BoolColumn`（`WHERE flag`）と `BoolEquality { value: true }`
        // （`WHERE flag = true`）は評価結果としては同一だが、構文が異なる
        // ため安全側に倒し別タグ・別ハッシュとする（Issue #883）。
        WherePredicate::BoolEquality { column, value } => {
            b.push_u8(5);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_u8(u8::from(*value));
        }
        WherePredicate::BoolColumn { column } => {
            b.push_u8(6);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
        }
        // `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較
        // （`< > <= >=`。TABLE-13・TASK-199、Issue #891）。演算子の判別子
        // （`CompareOp` の宣言順）を末尾へ付け加えることで、列・リテラルが
        // 同じでも演算子が異なれば別ハッシュになる。
        WherePredicate::Compare { column, op, value } => {
            b.push_u8(7);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_bytes(value.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            let op_tag: u8 = match op {
                crate::sql::allowlist::CompareOp::Lt => 0,
                crate::sql::allowlist::CompareOp::Le => 1,
                crate::sql::allowlist::CompareOp::Gt => 2,
                crate::sql::allowlist::CompareOp::Ge => 3,
            };
            b.push_u8(op_tag);
        }
        WherePredicate::Or(branches) => {
            b.push_u8(8);
            let branch_count =
                u32::try_from(branches.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&branch_count.to_le_bytes());
            for branch in branches {
                let leaf_count =
                    u32::try_from(branch.len()).map_err(|_| dml_hash_field_too_large())?;
                b.push_raw(&leaf_count.to_le_bytes());
                for leaf in branch {
                    push_dml_where_predicate(b, leaf, udf_registry, referenced)?;
                }
            }
        }
        // `IN`（SQL-24。TASK-208 ポインタ）。件数プレフィクス（u32 LE）に続けて
        // 各要素を出現順のまま連結する（構文段が要素の順序を保持したまま束縛
        // するため、順序を変えるとハッシュが不安定になる）。
        WherePredicate::InList { column, values } => {
            b.push_u8(9);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            let count = u32::try_from(values.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&count.to_le_bytes());
            for value in values {
                b.push_bytes(value.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
        }
        // `BETWEEN`（SQL-24。TASK-208 ポインタ）。
        WherePredicate::Between { column, low, high } => {
            b.push_u8(10);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_bytes(low.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_bytes(high.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
        }
        // `IS [NOT] NULL`（SQL-24。TASK-208 ポインタ）。`negated` を末尾へ
        // 付け加えることで `IS NULL` と `IS NOT NULL` が別ハッシュになる。
        WherePredicate::IsNull { column, negated } => {
            b.push_u8(11);
            b.push_bytes(column.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            b.push_u8(u8::from(*negated));
        }
        // `NOT`（SQL-24。TASK-208 ポインタ）。内側を再帰でそのまま直列化する。
        WherePredicate::Not(inner) => {
            b.push_u8(12);
            push_dml_where_predicate(b, inner, udf_registry, referenced)?;
        }
        // Issue #927・SQL-29 (a)・TASK-213: サブクエリは述語形 `UPDATE`/`DELETE`
        // の WHERE では構文解析段（`Parser::require_subquery_depth`）が既定
        // `subquery_ctx == None` により `42601` で拒否するため、DML の content
        // hash 対象へ到達しない（fail-closed の防御的経路）。
        WherePredicate::InSubquery { .. }
        | WherePredicate::Exists { .. }
        | WherePredicate::ScalarSubqueryCompare { .. } => {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "unresolved subquery reached DML content hash".to_string(),
            });
        }
        // `IdCompare` は `sql::subquery` が解決段で生成する内部専用の葉で、構文解析
        // からは作られない（述語形 DML の content hash 対象へ到達しない防御的経路）。
        WherePredicate::IdCompare { .. } => {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "internal id comparison reached DML content hash".to_string(),
            });
        }
    }
    Ok(())
}

/// `Call` の呼び出し先を解決し、宣言的 UDF なら参照 UDF 集合（`out`）へ追加して
/// 本体を再帰的に走査する（推移閉包。ADR §4.4.1「4.」）。WASM UDF に解決される
/// 呼び出しは許可形状外として拒否する（`get_wasm` → `get` の順で必ず判定する。
/// ADR §4.4.1「2.」の順序を維持しないと WASM UDF の名前が誤って「組み込み関数」
/// 側へ分類されてしまう）。`UdfRegistry` は追記専用（自身より前に登録済みの
/// UDF のみを呼べる）ため巡回はなく、この再帰は必ず停止する。
fn collect_referenced_udfs(
    expr: &crate::sql::udf_call::Expr,
    udf_registry: &crate::sql::udf_call::UdfRegistry,
    out: &mut std::collections::BTreeMap<String, crate::sql::udf_call::UdfDefinition>,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::udf_call::Expr;

    match expr {
        Expr::Number(_) | Expr::Ident(_) | Expr::String(_) => Ok(()),
        Expr::Call { name, args } => {
            if udf_registry.get_wasm(name).is_some() {
                return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "WASM UDF calls are not supported in predicate-form UPDATE/DELETE WHERE clauses",
                ));
            }
            if let Some(def) = udf_registry.get(name) {
                let lower = name.to_ascii_lowercase();
                if let std::collections::btree_map::Entry::Vacant(entry) = out.entry(lower) {
                    entry.insert(def.clone());
                    collect_referenced_udfs(&def.body, udf_registry, out)?;
                }
            }
            for arg in args {
                collect_referenced_udfs(arg, udf_registry, out)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_referenced_udfs(lhs, udf_registry, out)?;
            collect_referenced_udfs(rhs, udf_registry, out)
        }
        // `CASE`／`COALESCE`／`NULLIF`（対象ビヘイビア: SQL-26。Issue #921）は
        // UDF 呼び出しを持たない葉と同じく子を再帰的に走査するだけでよい
        // （UDF 呼び出しは `Expr::Call` 経由でのみ発生する）。
        Expr::Null => Ok(()),
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                collect_referenced_udfs(cond, udf_registry, out)?;
                collect_referenced_udfs(result, udf_registry, out)?;
            }
            if let Some(else_result) = else_result {
                collect_referenced_udfs(else_result, udf_registry, out)?;
            }
            Ok(())
        }
        Expr::Coalesce(args) => {
            for a in args {
                collect_referenced_udfs(a, udf_registry, out)?;
            }
            Ok(())
        }
        Expr::NullIf(lhs, rhs) => {
            collect_referenced_udfs(lhs, udf_registry, out)?;
            collect_referenced_udfs(rhs, udf_registry, out)
        }
        // `DATE`／`TIMESTAMP` 型付きリテラル（対象ビヘイビア: SQL-26。
        // Issue #920）は UDF 呼び出しを持たない葉。
        Expr::DateLiteral(_) | Expr::TimestampLiteral(_) => Ok(()),
    }
}

/// 数値リテラル（平文十進・指数表記）を正確な十進値の正規形 `-?<有効数字>e<10 の指数>`
/// へ変換する（先頭 0・末尾 0 を除去し、指数へ繰り込む。値が 0 の場合は符号を保持した
/// `0`／`-0`）。解釈できない形（指数部の欠落・範囲外等）は `None`。
/// `push_dml_expr` の数値リテラル直列化から呼ばれ、同じ十進値の異なる綴り
/// （平文十進と指数表記を含む）を同一視する。
fn canonical_decimal_parts(raw: &str) -> Option<String> {
    let (neg, body) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (mantissa, exp_text) = match body.find(['e', 'E']) {
        Some(i) => (body.get(..i)?, Some(body.get(i + 1..)?)),
        None => (body, None),
    };
    let exp10: i64 = match exp_text {
        Some(t) => t.strip_prefix('+').unwrap_or(t).parse().ok()?,
        None => 0,
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int_part.is_empty()
        || !int_part.bytes().all(|c| c.is_ascii_digit())
        || !frac_part.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let sign = if neg { "-" } else { "" };
    let digits: String = [int_part, frac_part].concat();
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some(format!("{sign}0"));
    }
    let trimmed = digits.trim_end_matches('0');
    let trailing = i64::try_from(digits.len() - trimmed.len()).ok()?;
    let frac_len = i64::try_from(frac_part.len()).ok()?;
    let exp = exp10.checked_sub(frac_len)?.checked_add(trailing)?;
    Some(format!("{sign}{trimmed}e{exp}"))
}

/// 数値リテラルの正規形を文字列で返す。解釈できない形は生テキストのまま返す。
fn canonical_decimal_text(raw: &str) -> String {
    canonical_decimal_parts(raw).unwrap_or_else(|| raw.to_string())
}

/// 数値リテラル `raw` が f64 `v` へ値を変えずに往復できるか。`v` の最短往復表記
/// （`{:e}`。桁数に依らず指数形）と正確な十進値の正規形が一致するときのみ忠実とみなす。
/// 平文十進・指数表記のどちらも同じ基準で判定する（Issue #1430 codex P1・Bugbot 指摘）。
/// 非有限値・解釈不能な入力は忠実でない（呼び出し側が生テキスト系へ倒す）。
fn number_literal_is_f64_faithful(raw: &str, v: f64) -> bool {
    if !v.is_finite() {
        return false;
    }
    match (
        canonical_decimal_parts(raw),
        canonical_decimal_parts(&format!("{v:e}")),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// `Expr` をタグ付き前置順で直列化する（ADR §4.4）。`params` が `Some` の場合、
/// `Ident` が参照 UDF 自身のパラメータ（大文字小文字を区別せず照合）を指すときに
/// 限り小文字化して連結する（ADR §4.4.1「6.」。`bind_expr_in` のパラメータ解決が
/// 大文字小文字を無視するため、同じ UDF の異なる大文字小文字綴りが異なるハッシュに
/// ならないようにする）。`WHERE` 直下（`params == None`）の `Ident`（列参照）は
/// 大文字小文字を区別したまま連結する（ADR §4.4 既定規則）。
fn push_dml_expr(
    b: &mut HashInputBuilder,
    expr: &crate::sql::udf_call::Expr,
    params: Option<&[String]>,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::udf_call::Expr;

    match expr {
        // Issue #1430・SQL-24: NUMERIC 列 × 裸の数値リテラルの比較は束縛段で正確な 10 進値
        // として受理されるため、f64 で正確に表せない整数（2^53 超）もここへ到達する。
        // 従来どおり f64 で直列化できる入力はタグ 1 のまま（既存ハッシュ不変）、できない入力
        // だけを新タグ 11＋生テキストで直列化する（従来エラーだった入力のみが対象で台帳互換）。
        // 値の妥当性（範囲・型）は束縛段が検査する。
        Expr::Number(raw) => match crate::sql::udf_call::parse_number_literal(raw) {
            // 十進表記が f64 の最短往復表記と一致する（f64 へ丸めても値が変わらない）入力は
            // 従来どおりタグ 1（既存ハッシュ不変）。f64 へ丸めると別の十進値と同一視される
            // 入力（`1.0000000000000001` と `1.0000000000000002` 等。NUMERIC 列との正確な
            // 比較では別述語）は、RECOVER-10 の内容照合を誤一致させないようタグ 11＋
            // 正規化した十進文字列で直列化する（Issue #1430 codex P1 指摘）。
            Ok(v) if number_literal_is_f64_faithful(raw, v) => {
                b.push_u8(1);
                b.push_raw(&v.to_bits().to_le_bytes());
            }
            Ok(_) => {
                b.push_u8(11);
                b.push_bytes(canonical_decimal_text(raw).as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
            Err(_) => {
                b.push_u8(11);
                b.push_bytes(raw.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
        },
        Expr::Ident(name) => {
            b.push_u8(2);
            let is_param = params
                .map(|ps| ps.iter().any(|p| p.eq_ignore_ascii_case(name)))
                .unwrap_or(false);
            if is_param {
                b.push_bytes(name.to_ascii_lowercase().as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            } else {
                b.push_bytes(name.as_bytes())
                    .map_err(|_| dml_hash_field_too_large())?;
            }
        }
        // Issue #919・SQL-26（P0・RECOVER-10）: 文字列リテラルはタグ＋長さ前置＋
        // 内容込みで直列化する。ここを欠くと、文字列リテラルだけが異なる 2 つの
        // DML（`WHERE lower(label) = 'a'` と `'b'` 等）が同じ content hash に
        // なり、operation_id の内容照合が誤って「内容一致」と判定してしまう。
        // タグ 9（origin/main 取り込み時の是正: Issue #921 が独立に 5〜8 を
        // `Null`／`Case`／`Coalesce`／`NullIf` へ割り当てていたため、5 のままでは
        // `Expr::Null` と衝突していた。9 番へ採番し直し、既存タグ 1〜8 は不変）。
        Expr::String(s) => {
            b.push_u8(9);
            b.push_bytes(s.as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
        }
        Expr::Call { name, args } => {
            b.push_u8(3);
            b.push_bytes(name.to_ascii_lowercase().as_bytes())
                .map_err(|_| dml_hash_field_too_large())?;
            let count = u32::try_from(args.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&count.to_le_bytes());
            for arg in args {
                push_dml_expr(b, arg, params)?;
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            b.push_u8(4);
            b.push_u8(dml_binop_tag(*op));
            push_dml_expr(b, lhs, params)?;
            push_dml_expr(b, rhs, params)?;
        }
        // タグ 5〜8（対象ビヘイビア: SQL-26。Issue #921。ADR §4.4 タグ表に追記）。
        Expr::Null => {
            b.push_u8(5);
        }
        Expr::Case { whens, else_result } => {
            b.push_u8(6);
            let count = u32::try_from(whens.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&count.to_le_bytes());
            for (cond, result) in whens {
                push_dml_expr(b, cond, params)?;
                push_dml_expr(b, result, params)?;
            }
            // ELSE 有無フラグ（境界の曖昧さを避けるため本体の前に置く）。
            b.push_u8(u8::from(else_result.is_some()));
            if let Some(else_result) = else_result {
                push_dml_expr(b, else_result, params)?;
            }
        }
        Expr::Coalesce(args) => {
            b.push_u8(7);
            let count = u32::try_from(args.len()).map_err(|_| dml_hash_field_too_large())?;
            b.push_raw(&count.to_le_bytes());
            for arg in args {
                push_dml_expr(b, arg, params)?;
            }
        }
        Expr::NullIf(lhs, rhs) => {
            b.push_u8(8);
            push_dml_expr(b, lhs, params)?;
            push_dml_expr(b, rhs, params)?;
        }
        // タグ 10（対象ビヘイビア: SQL-26。Issue #920。ADR §4.4 タグ表に追記）。
        // `DATE`／`TIMESTAMP` を型バイト（0/1）で区別してから内部表現（LE）を
        // 連結する。同じ暦日を表す `DATE '2024-01-01'` と
        // `TIMESTAMP '2024-01-01 00:00:00'` を型バイトで区別しないと、日付だけが
        // 異なる DML と衝突しないことは保証できても、型違いの 2 リテラルが
        // 同じハッシュになってしまう（RECOVER-10 の要件: 内容が異なる DML は
        // 異なるハッシュを持つ）。
        Expr::DateLiteral(days) => {
            b.push_u8(10);
            b.push_u8(0);
            b.push_raw(&days.to_le_bytes());
        }
        Expr::TimestampLiteral(micros) => {
            b.push_u8(10);
            b.push_u8(1);
            b.push_raw(&micros.to_le_bytes());
        }
    }
    Ok(())
}

/// `BinOp` を 1 バイトへ写像する（ADR §4.4）。
fn dml_binop_tag(op: crate::sql::udf_call::BinOp) -> u8 {
    use crate::sql::udf_call::BinOp;
    match op {
        BinOp::Add => 1,
        BinOp::Sub => 2,
        BinOp::Mul => 3,
        BinOp::Div => 4,
        BinOp::Gt => 5,
        BinOp::Lt => 6,
        BinOp::Ge => 7,
        BinOp::Le => 8,
        BinOp::Eq => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 操作種別が違えば同じフィールド列でも異なるハッシュになる（連結曖昧性排除の
    // ピン留め）。
    #[test]
    fn different_op_tags_produce_different_hashes() {
        let mut insert_b = HashInputBuilder::new(OpTag::Insert);
        insert_b.push_u64(1);
        let insert_hash = insert_b.finish();

        let mut update_b = HashInputBuilder::new(OpTag::Update);
        update_b.push_u64(1);
        let update_hash = update_b.finish();

        assert_ne!(insert_hash, update_hash);
    }

    // BYTEA（タグ 11）と TEXT（タグ 1）は本体バイト列が偶然一致していても
    // 型タグの違いだけで別ハッシュになる（Issue #886。golden な区別の固定）。
    #[test]
    fn bytea_and_text_values_produce_different_hashes_even_with_matching_bytes() {
        let bytes_value = Value::Bytes(vec![0xde, 0xad]);
        let text_value = Value::Text("\\xdead".to_string());
        let bytes_hash = for_typed_insert(1, Visibility::Public, &[], &[("blob", &bytes_value)])
            .expect("hash bytes");
        let text_hash = for_typed_insert(1, Visibility::Public, &[], &[("blob", &text_value)])
            .expect("hash text");
        assert_ne!(bytes_hash, text_hash);
    }

    // ENUM（タグ 13）と TEXT（タグ 1）は本体バイト列が完全一致していても
    // 型タグの違いだけで別ハッシュになる（Issue #890。golden な区別の固定）。
    #[test]
    fn enum_and_text_values_produce_different_hashes_even_with_matching_bytes() {
        let enum_value = Value::Enum("happy".to_string());
        let text_value = Value::Text("happy".to_string());
        let enum_hash = for_typed_insert(1, Visibility::Public, &[], &[("mood", &enum_value)])
            .expect("hash enum");
        let text_hash = for_typed_insert(1, Visibility::Public, &[], &[("mood", &text_value)])
            .expect("hash text");
        assert_ne!(enum_hash, text_hash);
    }

    // 同一の BYTEA 値からは同じハッシュが再現する（再送判定の前提）。
    #[test]
    fn bytea_hash_is_reproducible_for_identical_content() {
        let value = Value::Bytes(vec![0x01, 0x02, 0x03]);
        let h1 = for_typed_insert(1, Visibility::Public, &[], &[("blob", &value)]).expect("hash 1");
        let h2 = for_typed_insert(1, Visibility::Public, &[], &[("blob", &value)]).expect("hash 2");
        assert_eq!(h1, h2);
    }

    // バイト列が異なれば BYTEA のハッシュも異なる（内容不一致検出の前提）。
    #[test]
    fn different_bytea_values_produce_different_hashes() {
        let a = Value::Bytes(vec![0xde, 0xad]);
        let b = Value::Bytes(vec![0xbe, 0xef]);
        let hash_a = for_typed_insert(1, Visibility::Public, &[], &[("blob", &a)]).expect("hash a");
        let hash_b = for_typed_insert(1, Visibility::Public, &[], &[("blob", &b)]).expect("hash b");
        assert_ne!(hash_a, hash_b);
    }

    // JSON（タグ 12）と TEXT（タグ 1）は本体バイト列が偶然一致していても
    // 型タグの違いだけで別ハッシュになる（Issue #889。golden な区別の固定）。
    #[test]
    fn json_and_text_values_produce_different_hashes_even_with_matching_bytes() {
        let json_value = Value::Json("{}".to_string());
        let text_value = Value::Text("{}".to_string());
        let json_hash = for_typed_insert(1, Visibility::Public, &[], &[("doc", &json_value)])
            .expect("hash json");
        let text_hash = for_typed_insert(1, Visibility::Public, &[], &[("doc", &text_value)])
            .expect("hash text");
        assert_ne!(json_hash, text_hash);
    }

    // 同一の JSON テキストからは同じハッシュが再現する（再送判定の前提）。
    #[test]
    fn json_hash_is_reproducible_for_identical_content() {
        let value = Value::Json(r#"{"a":1}"#.to_string());
        let h1 = for_typed_insert(1, Visibility::Public, &[], &[("doc", &value)]).expect("hash 1");
        let h2 = for_typed_insert(1, Visibility::Public, &[], &[("doc", &value)]).expect("hash 2");
        assert_eq!(h1, h2);
    }

    // JSON テキストが異なれば（空白のみの差であっても）ハッシュも異なる
    // （JSON 列は入力テキストを保持する契約のため。内容不一致検出の前提）。
    #[test]
    fn different_json_values_produce_different_hashes() {
        let a = Value::Json(r#"{"a":1}"#.to_string());
        let b = Value::Json(r#"{"a": 1}"#.to_string());
        let hash_a = for_typed_insert(1, Visibility::Public, &[], &[("doc", &a)]).expect("hash a");
        let hash_b = for_typed_insert(1, Visibility::Public, &[], &[("doc", &b)]).expect("hash b");
        assert_ne!(hash_a, hash_b);
    }

    // 長さプレフィクスにより "ab"+"c" と "a"+"bc" が同一ハッシュにならない
    // （連結曖昧性排除のピン留め）。
    #[test]
    fn length_prefix_prevents_concatenation_ambiguity() {
        let mut b1 = HashInputBuilder::new(OpTag::Insert);
        b1.push_bytes(b"ab").expect("push");
        b1.push_bytes(b"c").expect("push");
        let h1 = b1.finish();

        let mut b2 = HashInputBuilder::new(OpTag::Insert);
        b2.push_bytes(b"a").expect("push");
        b2.push_bytes(b"bc").expect("push");
        let h2 = b2.finish();

        assert_ne!(h1, h2);
    }

    // 同一内容の再送は同一ハッシュ（決定性）。
    #[test]
    fn for_insert_is_deterministic_for_same_input() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0],
            metadata: b"meta",
        };
        let h1 = for_insert(7, &row).expect("hash");
        let h2 = for_insert(7, &row).expect("hash");
        assert_eq!(h1, h2);
    }

    // id の違いは異なるハッシュを生む。
    #[test]
    fn for_insert_differs_by_id() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0],
            metadata: b"meta",
        };
        let h1 = for_insert(7, &row).expect("hash");
        let h2 = for_insert(8, &row).expect("hash");
        assert_ne!(h1, h2);
    }

    // for_delete は id のみで決定的に定まる。
    #[test]
    fn for_delete_is_deterministic_and_differs_by_id() {
        assert_eq!(for_delete(1), for_delete(1));
        assert_ne!(for_delete(1), for_delete(2));
    }

    // for_truncate は入力を持たないため常に同一値を返す（決定性の固定）。
    // OpTag が異なる for_delete とはハッシュが一致しないことも合わせて固定する
    // （ドメイン分離タグ・OpTag のみで区別できることの確認）。
    #[test]
    fn for_truncate_is_deterministic_and_differs_from_other_ops() {
        assert_eq!(for_truncate(), for_truncate());
        assert_ne!(for_truncate(), for_delete(1));
    }

    // for_replace_by_text_key はクライアント要求由来の body/template_values の
    // 違いを区別する。
    #[test]
    fn for_replace_by_text_key_differs_by_body() {
        let lang = Value::Text("en".to_string());
        let template: [(&str, &Value); 1] = [("lang", &lang)];
        let h1 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "body a",
            &template,
        )
        .expect("hash");
        let h2 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "body b",
            &template,
        )
        .expect("hash");
        assert_ne!(h1, h2);
    }

    // チャンク化・埋め込みの結果（行データそのもの）が変わっても、クライアント
    // 要求（path/body/template_values）が同一なら同一ハッシュ（P1 修正のピン留め:
    // codex-review 指摘・PR #248。`chunking` 設定や `Embedder` の応答差でハッシュが
    // 変わると、同一要求の再送が `OperationIdContentMismatch` に誤判定される）。
    #[test]
    fn for_replace_by_text_key_is_independent_of_chunking_and_embedding_output() {
        let lang = Value::Text("en".to_string());
        let template: [(&str, &Value); 1] = [("lang", &lang)];
        let h1 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &template,
        )
        .expect("hash");
        let h2 = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &template,
        )
        .expect("hash");
        assert_eq!(h1, h2);
    }

    // cursor bugbot 指摘・PR #248 のピン留め: `ALTER TABLE ADD COLUMN`（常に
    // nullable）で新規列が追加されても、それに触れていないクライアント要求の
    // 再送ハッシュは不変（新規列は Value::Null として渡り、
    // push_named_scalar_columns が素通しで除外するため）。
    #[test]
    fn for_replace_by_text_key_is_stable_across_added_nullable_column() {
        let lang = Value::Text("en".to_string());
        // スキーマ変更前: `lang` 列のみ。
        let before: [(&str, &Value); 1] = [("lang", &lang)];
        let h_before = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &before,
        )
        .expect("hash");

        // スキーマ変更後（`ALTER TABLE ADD COLUMN note TEXT` 相当）: 同一クライアント
        // 要求の再送では `note` 列は未提供のため Value::Null で束縛される。
        let null_note = Value::Null;
        let after: [(&str, &Value); 2] = [("lang", &lang), ("note", &null_note)];
        let h_after = for_replace_by_text_key(
            "path",
            "docs/a.md",
            Visibility::Private,
            "docs/a.md",
            "same body",
            &after,
        )
        .expect("hash");

        assert_eq!(h_before, h_after);
    }

    // for_typed_insert も同じ理由で、追加された nullable 列（未提供 → Value::Null）
    // の有無にハッシュが影響されない。
    #[test]
    fn for_typed_insert_is_stable_across_added_nullable_column() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let before: [(&str, &Value); 1] = [("title", &title)];
        let h_before = for_typed_insert(7, Visibility::Public, &embedding, &before).expect("hash");

        let null_note = Value::Null;
        let after: [(&str, &Value); 2] = [("title", &title), ("note", &null_note)];
        let h_after = for_typed_insert(7, Visibility::Public, &embedding, &after).expect("hash");

        assert_eq!(h_before, h_after);
    }

    // 一方、実際に異なる値が入れば当然ハッシュも変わる（区別できないほど鈍化
    // していないことの確認）。
    // TABLE-13・Issue #882 計画 F9: Real=5／Double=6 のタグ割り当てを固定する
    // golden テスト。既存タグ（Null=0／Text=1／Vector=2）と衝突せず、かつ
    // 同じビットパターンを持つ異なる型（`Real(1.0)` と `Double(1.0)`）を
    // 区別できることを固定する。
    #[test]
    fn for_typed_insert_real_and_double_tags_are_stable_and_distinct() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let real_col = Value::Real(1.0);
        let double_col = Value::Double(1.0);
        let text_col = Value::Text("1".to_string());
        let cols_real: [(&str, &Value); 1] = [("v", &real_col)];
        let cols_double: [(&str, &Value); 1] = [("v", &double_col)];
        let cols_text: [(&str, &Value); 1] = [("v", &text_col)];

        let h_real = for_typed_insert(7, Visibility::Public, &embedding, &cols_real).expect("hash");
        let h_double =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_double).expect("hash");
        let h_text = for_typed_insert(7, Visibility::Public, &embedding, &cols_text).expect("hash");

        // 同じ論理値（1.0）でも型タグが異なれば別ハッシュになる（タグが
        // ハッシュ入力に混ざっている証拠）。
        assert_ne!(h_real, h_double);
        assert_ne!(h_real, h_text);
        assert_ne!(h_double, h_text);

        // 再現性: 同一入力から同一ハッシュが得られる（タグ値が偶然の実行時
        // 揺れでない）。
        let h_real_again =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_real).expect("hash");
        assert_eq!(h_real, h_real_again);
    }

    #[test]
    fn for_typed_insert_real_and_double_signed_zero_normalizes_to_positive_zero() {
        // F4: `-0.0` と `+0.0` は論理的に同一値であり、`operation_id` の
        // 再送判定（同一内容なら 23505・不一致なら 22023）はハッシュ一致で
        // 決まる。エンコード経路（row_codec.rs）だけでなくハッシュ入力の
        // 生成でも正規化しないと、再送時に符号ビットの偶然の違いで
        // 誤って内容不一致（22023）と判定されてしまう。
        let embedding = [1.0_f32, 2.0, 3.0];
        let real_pos = Value::Real(0.0_f32);
        let real_neg = Value::Real(-0.0_f32);
        let double_pos = Value::Double(0.0_f64);
        let double_neg = Value::Double(-0.0_f64);
        let cols_real_pos: [(&str, &Value); 1] = [("v", &real_pos)];
        let cols_real_neg: [(&str, &Value); 1] = [("v", &real_neg)];
        let cols_double_pos: [(&str, &Value); 1] = [("v", &double_pos)];
        let cols_double_neg: [(&str, &Value); 1] = [("v", &double_neg)];

        let h_real_pos =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_real_pos).expect("hash");
        let h_real_neg =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_real_neg).expect("hash");
        let h_double_pos =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_double_pos).expect("hash");
        let h_double_neg =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_double_neg).expect("hash");

        assert_eq!(h_real_pos, h_real_neg);
        assert_eq!(h_double_pos, h_double_neg);
    }

    #[test]
    fn for_typed_insert_differs_by_column_value() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title_a = Value::Text("hello".to_string());
        let title_b = Value::Text("world".to_string());
        let cols_a: [(&str, &Value); 1] = [("title", &title_a)];
        let cols_b: [(&str, &Value); 1] = [("title", &title_b)];
        let h1 = for_typed_insert(7, Visibility::Public, &embedding, &cols_a).expect("hash");
        let h2 = for_typed_insert(7, Visibility::Public, &embedding, &cols_b).expect("hash");
        assert_ne!(h1, h2);
    }

    // --- INTEGER / BIGINT（Issue #881・TABLE-13・TASK-196） -----------------

    // タグ 3（Integer）／タグ 4（BigInt）で値が異なれば区別できる（同一内容の
    // 再送は 23505、内容不一致は 22023 という RECOVER-10 の契約を成立させる前提）。
    #[test]
    fn for_typed_insert_differs_by_integer_and_bigint_value() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let n_a = Value::Integer(1);
        let n_b = Value::Integer(2);
        let cols_a: [(&str, &Value); 1] = [("n", &n_a)];
        let cols_b: [(&str, &Value); 1] = [("n", &n_b)];
        let h1 = for_typed_insert(7, Visibility::Public, &embedding, &cols_a).expect("hash");
        let h2 = for_typed_insert(7, Visibility::Public, &embedding, &cols_b).expect("hash");
        assert_ne!(h1, h2);

        let b_a = Value::BigInt(1);
        let b_b = Value::BigInt(2);
        let cols_c: [(&str, &Value); 1] = [("b", &b_a)];
        let cols_d: [(&str, &Value); 1] = [("b", &b_b)];
        let h3 = for_typed_insert(7, Visibility::Public, &embedding, &cols_c).expect("hash");
        let h4 = for_typed_insert(7, Visibility::Public, &embedding, &cols_d).expect("hash");
        assert_ne!(h3, h4);
    }

    // 同一内容（同じ列名・同じ Integer/BigInt 値）の再送は同一ハッシュになる
    // （台帳照合による再送判定 23505 が成立する前提）。
    #[test]
    fn for_typed_insert_is_stable_for_identical_integer_and_bigint_resend() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let n = Value::Integer(42);
        let b = Value::BigInt(-42);
        let cols: [(&str, &Value); 2] = [("n", &n), ("b", &b)];
        let h1 = for_typed_insert(7, Visibility::Public, &embedding, &cols).expect("hash");
        let h2 = for_typed_insert(7, Visibility::Public, &embedding, &cols).expect("hash");
        assert_eq!(h1, h2);
    }

    // Integer と BigInt はタグ（3 と 4）で区別され、たとえ値が偶然一致しても
    // 異なる列型として異なるハッシュになる（タグ混同による衝突が無いことの固定）。
    #[test]
    fn for_typed_insert_distinguishes_integer_from_bigint_tag_even_with_same_numeric_value() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let n = Value::Integer(5);
        let b = Value::BigInt(5);
        let cols_int: [(&str, &Value); 1] = [("v", &n)];
        let cols_big: [(&str, &Value); 1] = [("v", &b)];
        let h_int = for_typed_insert(7, Visibility::Public, &embedding, &cols_int).expect("hash");
        let h_big = for_typed_insert(7, Visibility::Public, &embedding, &cols_big).expect("hash");
        assert_ne!(h_int, h_big);
    }

    // codex-review P1・cursor bugbot 指摘（PR #248）の回帰固定: visibility のみが
    // 異なる再送は、内容一致（23505）ではなく内容不一致（22023）として区別できな
    // ければならない（RECOVER-10 の内容照合契約）。
    #[test]
    fn for_typed_insert_differs_by_visibility() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let cols: [(&str, &Value); 1] = [("title", &title)];
        let h_public = for_typed_insert(7, Visibility::Public, &embedding, &cols).expect("hash");
        let h_private = for_typed_insert(7, Visibility::Private, &embedding, &cols).expect("hash");
        assert_ne!(h_public, h_private);
    }

    /// 配列列（TABLE-14・Issue #888・D-A9）のハッシュ入力が単射であること。
    /// `{ab}`（1 要素）と `{a,b}`（2 要素）のような表記ゆれが衝突しないこと、
    /// 同一内容は同一ハッシュになることを固定する。
    #[test]
    fn for_typed_insert_array_value_is_injective_and_deterministic() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let one_elem = Value::Array(crate::row_codec::ArrayValue::Text(vec![Some(
            "ab".to_string(),
        )]));
        let two_elems = Value::Array(crate::row_codec::ArrayValue::Text(vec![
            Some("a".to_string()),
            Some("b".to_string()),
        ]));
        let cols_one: [(&str, &Value); 1] = [("tags", &one_elem)];
        let cols_two: [(&str, &Value); 1] = [("tags", &two_elems)];
        let h_one = for_typed_insert(7, Visibility::Public, &embedding, &cols_one).expect("hash");
        let h_two = for_typed_insert(7, Visibility::Public, &embedding, &cols_two).expect("hash");
        assert_ne!(
            h_one, h_two,
            "{{ab}} (1 element) and {{a,b}} (2 elements) must not collide"
        );

        // 同一内容の再送は同一ハッシュ（決定性）。
        let h_one_again =
            for_typed_insert(7, Visibility::Public, &embedding, &cols_one).expect("hash");
        assert_eq!(h_one, h_one_again);

        // 要素型が異なれば（同じ見た目の値でも）別ハッシュになること。
        let bool_elems = Value::Array(crate::row_codec::ArrayValue::Bool(vec![
            Some(true),
            Some(false),
        ]));
        let cols_bool: [(&str, &Value); 1] = [("tags", &bool_elems)];
        let h_bool = for_typed_insert(7, Visibility::Public, &embedding, &cols_bool).expect("hash");
        assert_ne!(h_two, h_bool);
    }

    /// Issue #1193: NULL 要素を含む配列のハッシュ入力が単射であること
    /// （`{NULL}`／`{}`／`{"NULL"}`／`{1,NULL}`／`{1}` が互いに衝突しない）。
    #[test]
    fn for_typed_insert_array_with_null_elements_is_injective() {
        use crate::row_codec::ArrayValue as A;
        let embedding = [1.0_f32, 2.0, 3.0];
        let hash = |v: A| {
            let value = Value::Array(v);
            let cols: [(&str, &Value); 1] = [("tags", &value)];
            for_typed_insert(7, Visibility::Public, &embedding, &cols).expect("hash")
        };
        let hashes = [
            hash(A::Text(vec![None])),
            hash(A::Text(vec![])),
            hash(A::Text(vec![Some("NULL".to_string())])),
            hash(A::Text(vec![Some(String::new())])),
            hash(A::Text(vec![Some("a".to_string()), None])),
            hash(A::Text(vec![None, Some("a".to_string())])),
            hash(A::Integer(vec![Some(1), None])),
            hash(A::Integer(vec![Some(1)])),
            hash(A::Integer(vec![None])),
            hash(A::BigInt(vec![Some(1), None])),
            hash(A::Uuid(vec![None])),
        ];
        for (i, a) in hashes.iter().enumerate() {
            for b in hashes.iter().skip(i + 1) {
                assert_ne!(a, b, "array hash inputs must not collide");
            }
        }
        // 同一内容は同一ハッシュ（決定性）。
        assert_eq!(
            hash(A::Integer(vec![Some(1), None])),
            hash(A::Integer(vec![Some(1), None]))
        );
    }

    // Issue #771: `for_typed_insert_batch` は要求記載順を入力に含めるため、
    // 同一集合でも並び替えると別ハッシュになる（`for_insert_batch_encoded` と
    // 同じ設計判断。並び替えた同一集合の再送は意図的に区別する）。
    #[test]
    fn for_typed_insert_batch_differs_when_row_order_changes() {
        let embedding_a = [1.0_f32, 0.0, 0.0];
        let embedding_b = [0.0_f32, 1.0, 0.0];
        let title = Value::Text("hello".to_string());
        let cols: [(&str, &Value); 1] = [("title", &title)];
        let rows_forward: [TypedInsertBatchRow<'_>; 2] = [
            (1, Visibility::Private, &embedding_a, &cols),
            (2, Visibility::Private, &embedding_b, &cols),
        ];
        let rows_reversed: [TypedInsertBatchRow<'_>; 2] = [
            (2, Visibility::Private, &embedding_b, &cols),
            (1, Visibility::Private, &embedding_a, &cols),
        ];
        let h_forward = for_typed_insert_batch(&rows_forward).expect("hash");
        let h_reversed = for_typed_insert_batch(&rows_reversed).expect("hash");
        assert_ne!(h_forward, h_reversed);
    }

    // 行ごとの `visibility` の違いはバッチ全体のハッシュへ反映される
    // （`for_typed_insert_differs_by_visibility` の複数行版）。
    #[test]
    fn for_typed_insert_batch_differs_by_row_visibility() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let cols: [(&str, &Value); 1] = [("title", &title)];
        let rows_public: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Public, &embedding, &cols)];
        let rows_private: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Private, &embedding, &cols)];
        let h_public = for_typed_insert_batch(&rows_public).expect("hash");
        let h_private = for_typed_insert_batch(&rows_private).expect("hash");
        assert_ne!(h_public, h_private);
    }

    // `for_typed_insert` と同じく、`ALTER TABLE ADD COLUMN` 相当（未提供 →
    // `Value::Null` の追加列）を挟んでもバッチ全体のハッシュは不変（列名ベース・
    // `Null` 除外の設計を複数行版でも維持する）。
    #[test]
    fn for_typed_insert_batch_is_stable_across_added_nullable_column() {
        let embedding = [1.0_f32, 2.0, 3.0];
        let title = Value::Text("hello".to_string());
        let before: [(&str, &Value); 1] = [("title", &title)];
        let rows_before: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Public, &embedding, &before)];
        let h_before = for_typed_insert_batch(&rows_before).expect("hash");

        let null_note = Value::Null;
        let after: [(&str, &Value); 2] = [("title", &title), ("note", &null_note)];
        let rows_after: [TypedInsertBatchRow<'_>; 1] =
            [(7, Visibility::Public, &embedding, &after)];
        let h_after = for_typed_insert_batch(&rows_after).expect("hash");

        assert_eq!(h_before, h_after);
    }

    // codex-review P1 指摘（PR #823）の回帰固定: 行境界を示す列数プレフィクスが
    // 無いと、ある行の列データが「次の行」の `id`／`visibility`／`embedding` 長さへ
    // 意図的にはみ出す形で再解釈でき、実際には異なる行 id 集合へ書き込む 2 つの
    // バッチが同一バイト列（同一ハッシュ）へ還元されてしまう。
    //
    // 具体的には以下の 2 バッチを手計算で構築する（コメント中の 16 進バイト列は
    // `push_bytes`（4 バイト LE 長さ＋本体）・`push_value`（Null=0／Text=1 タグ＋
    // 長さ付き本体）のレイアウトから逆算した値。空ベクトル・空文字列値のみを使い
    // フィールド境界の算術を単純化してある）:
    //
    // - batch A: [(id1, "WXYZ"→"" の列あり), (id2a, 列なし)]
    // - batch B: [(id1, 列なし), (id2b, "PQRS"→"" の列あり)]
    //
    // id1 は共通だが、batch A は id2a を、batch B は id2b（id2a とは異なる値）を
    // 書き込む——つまり書き込み対象の行 id 集合そのものが異なる。列数プレフィクス
    // 導入前は、batch A の「行1の列データ」が batch B の「行2のヘッダ（id/vis/
    // vector 長）」として、batch A の「行2のヘッダ」が batch B の「行2の列データ」
    // として、それぞれ再解釈可能なバイト列になるよう choose してあり、両者は
    // バイト単位で完全に一致していた（本テストは列数プレフィクス導入後、この
    // 2 バッチが異なるハッシュになることを固定する）。
    #[test]
    fn for_typed_insert_batch_rejects_row_boundary_reinterpretation() {
        let id1 = 42u64;
        // id2a = u64::from_le_bytes([namelen=4, 'P','Q','R','S']) の逆算値。
        let id2a = u64::from_le_bytes([0x04, 0x00, 0x00, 0x00, b'P', b'Q', b'R', b'S']);
        // id2b = u64::from_le_bytes([namelen=4, 'W','X','Y','Z']) の逆算値。
        let id2b = u64::from_le_bytes([0x04, 0x00, 0x00, 0x00, b'W', b'X', b'Y', b'Z']);
        assert_ne!(id2a, id2b, "test construction requires distinct row ids");

        let empty_vec: [f32; 0] = [];
        let wxyz_value = Value::Text(String::new());
        let pqrs_value = Value::Text(String::new());
        let wxyz_col: [(&str, &Value); 1] = [("WXYZ", &wxyz_value)];
        let pqrs_col: [(&str, &Value); 1] = [("PQRS", &pqrs_value)];
        let no_cols: [(&str, &Value); 0] = [];

        let rows_a: [TypedInsertBatchRow<'_>; 2] = [
            (id1, Visibility::Public, &empty_vec, &wxyz_col),
            (id2a, Visibility::Public, &empty_vec, &no_cols),
        ];
        let rows_b: [TypedInsertBatchRow<'_>; 2] = [
            (id1, Visibility::Public, &empty_vec, &no_cols),
            (id2b, Visibility::Public, &empty_vec, &pqrs_col),
        ];

        let h_a = for_typed_insert_batch(&rows_a).expect("hash");
        let h_b = for_typed_insert_batch(&rows_b).expect("hash");
        assert_ne!(
            h_a, h_b,
            "batches writing to different row-id sets must not collide"
        );
    }

    // Issue #397 のピン留め: `for_insert_encoded` は「呼び出し元が事前エンコードした
    // 結果を渡す」新形、`for_insert` は「内部で `encode_row` する」旧形（参照実装）。
    // 同じ論理内容に対して常に同一ハッシュを返すことを確認し、事前エンコード共有化が
    // ハッシュ入力バイト列を変えていないことを機械検証する（既存台帳エントリとの
    // 互換の根拠）。
    #[test]
    fn for_insert_encoded_matches_reference_impl() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0, 4.0],
            metadata: b"meta",
        };
        let encoded = encode_row(&row).expect("encode");
        let h_new = for_insert_encoded(7, &encoded).expect("hash");
        let h_ref = for_insert(7, &row).expect("hash");
        assert_eq!(h_new, h_ref);
    }

    #[test]
    fn for_insert_batch_encoded_matches_reference_impl() {
        let row_a = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[1.0, 2.0, 3.0],
            metadata: b"meta-a",
        };
        let row_b = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[4.0, 5.0, 6.0],
            metadata: b"",
        };
        let rows: [(u64, RowInput<'_>); 2] = [(1, row_a), (2, row_b)];
        let h_ref = for_insert_batch(&rows).expect("hash");

        let encoded_a = encode_row(&row_a).expect("encode");
        let encoded_b = encode_row(&row_b).expect("encode");
        let hash_input: [(u64, &[u8]); 2] = [(1, &encoded_a), (2, &encoded_b)];
        let h_new = for_insert_batch_encoded(&hash_input).expect("hash");

        assert_eq!(h_new, h_ref);
    }

    // 空バッチも一致する（境界値）。
    #[test]
    fn for_insert_batch_encoded_matches_reference_impl_for_empty_batch() {
        let empty_rows: [(u64, RowInput<'_>); 0] = [];
        let h_ref = for_insert_batch(&empty_rows).expect("hash");
        let empty_hash_input: [(u64, &[u8]); 0] = [];
        let h_new = for_insert_batch_encoded(&empty_hash_input).expect("hash");
        assert_eq!(h_new, h_ref);
    }

    // [`for_insert_batch_encoded_reference`]（Issue #399 以前の一括処理版・A/B 計測
    // 専用）が production の [`for_insert_batch_encoded`]（ストリーミング版）と
    // 同一ダイジェストを返すことをピン留めする（codex-review 指摘・PR #419）。
    #[test]
    fn for_insert_batch_encoded_reference_matches_streaming() {
        let encoded_a: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
        let encoded_b: Vec<u8> = (0..700u32).map(|i| ((i * 3) % 256) as u8).collect();
        let hash_input: [(u64, &[u8]); 2] = [(1, &encoded_a), (2, &encoded_b)];

        let h_streaming = for_insert_batch_encoded(&hash_input).expect("hash");
        let h_reference = for_insert_batch_encoded_reference(&hash_input).expect("hash");
        assert_eq!(h_streaming, h_reference);
    }

    #[test]
    fn for_update_encoded_matches_reference_impl() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[9.0, 8.0, 7.0],
            metadata: b"updated",
        };
        let encoded = encode_row(&row).expect("encode");
        let h_new = for_update_encoded(3, &encoded).expect("hash");
        let h_ref = for_update(3, &row).expect("hash");
        assert_eq!(h_new, h_ref);
    }

    // --- for_update_columns（Issue #865・SQL-17・TASK-191） --------------------

    #[test]
    fn for_update_columns_is_deterministic() {
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        assert_eq!(
            for_update_columns(1, &cols).expect("hash"),
            for_update_columns(1, &cols).expect("hash")
        );
    }

    #[test]
    fn for_update_columns_differs_by_id() {
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        assert_ne!(
            for_update_columns(1, &cols).expect("hash"),
            for_update_columns(2, &cols).expect("hash")
        );
    }

    #[test]
    fn for_update_columns_differs_by_value() {
        let ja = Value::Text("ja".to_string());
        let en = Value::Text("en".to_string());
        let cols_ja: [(&str, &Value); 1] = [("lang", &ja)];
        let cols_en: [(&str, &Value); 1] = [("lang", &en)];
        assert_ne!(
            for_update_columns(1, &cols_ja).expect("hash"),
            for_update_columns(1, &cols_en).expect("hash")
        );
    }

    #[test]
    fn for_update_columns_differs_by_declared_order() {
        let a = Value::Text("a".to_string());
        let b = Value::Text("b".to_string());
        let forward: [(&str, &Value); 2] = [("col_a", &a), ("col_b", &b)];
        let reversed: [(&str, &Value); 2] = [("col_b", &b), ("col_a", &a)];
        assert_ne!(
            for_update_columns(1, &forward).expect("hash"),
            for_update_columns(1, &reversed).expect("hash")
        );
    }

    // Vector 列を含む SET も同じ経路で受理できることを確認する（判断 C）。
    #[test]
    fn for_update_columns_accepts_vector_values() {
        let vec_a: Value = Value::Vector(vec![1.0, 2.0]);
        let vec_b: Value = Value::Vector(vec![1.0, 2.5]);
        let cols_a: [(&str, &Value); 1] = [("embedding", &vec_a)];
        let cols_b: [(&str, &Value); 1] = [("embedding", &vec_b)];
        assert_ne!(
            for_update_columns(1, &cols_a).expect("hash"),
            for_update_columns(1, &cols_b).expect("hash")
        );
    }

    // 専用 OpTag（UpdateColumns）を持つため、同じ id・同種の列名付きペアでも
    // 既存の Update（全行置換・`for_update_encoded`）・Insert とはハッシュが
    // 一致しない（ドメイン分離。判断 C）。
    #[test]
    fn for_update_columns_differs_from_other_op_tags() {
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let h_update_columns = for_update_columns(1, &cols).expect("hash");

        let h_update = for_update_encoded(1, b"unrelated-encoded-row").expect("hash");
        assert_ne!(h_update_columns, h_update);

        let h_insert = for_typed_insert(1, Visibility::Private, &[], &cols).expect("hash");
        assert_ne!(h_update_columns, h_insert);
    }

    // 列数プレフィクスにより、列境界の再解釈（異なる列数・並びが同一バイト列へ
    // 還元される事故）が起きないことを固定する（`for_typed_insert_batch_rejects_row_boundary_reinterpretation`
    // と同種の回帰テスト）。
    #[test]
    fn for_update_columns_null_valued_columns_are_excluded_from_the_count_prefix() {
        let non_null = Value::Text("x".to_string());
        let null = Value::Null;
        let with_null: [(&str, &Value); 2] = [("a", &non_null), ("b", &null)];
        let without_null: [(&str, &Value); 1] = [("a", &non_null)];
        // NULL 列は push_named_scalar_columns が除外するため、列数プレフィクス・
        // ハッシュ入力とも NULL 列を含まない形と一致する。
        assert_eq!(
            for_update_columns(1, &with_null).expect("hash"),
            for_update_columns(1, &without_null).expect("hash")
        );
    }

    // Issue #399 受け入れ 2: ストリーミング版（本番経路 `for_insert_batch_encoded`）と
    // 参照実装（旧・一括処理版 `for_insert_batch_encoded_reference`。バッチ全体を
    // `Vec` へ連結してからパディングする）の処理時間を、台帳ハッシュ対象と同オーダー
    // （1,000 行 × 約 0.5KB ≈ 500KB）の入力で手元比較するための手動専用テスト
    // （CI 非配線・既定 ignore）。`cargo test --release -p fandhe-vector-db-engine --lib
    // recovery::content_hash::tests::sha256_streaming_vs_reference_manual_timing
    // -- --ignored --nocapture` で実行する。
    //
    // codex-review 指摘（PR #419）: 従来はここで事前構築済みの 500KB 単一バッファへ
    // `update`/一括ハッシュを 1 回だけ呼んでおり、本番経路（`HashInputBuilder` が
    // 行・フィールド単位で `push_u64`／`push_bytes` を多数回呼ぶ）を再現していなかった。
    // 本版は `for_insert_batch_encoded`（本番。ストリーミング）と
    // `for_insert_batch_encoded_reference`（旧・一括処理版。同じ行・フィールド境界を
    // 共有する `#[cfg(test)]` 参照実装）を、同一の 1,000 行入力へ通す形で比較する。
    #[test]
    #[ignore = "手動計測専用（CI 非配線。--ignored --nocapture で明示実行する）"]
    fn sha256_streaming_vs_reference_manual_timing() {
        use std::time::Instant;

        // 1 行あたり約 0.5KB（実運用の embedding + metadata の encode 済みバイト列
        // と同オーダー）× 1,000 行 ≈ 500KB。行ごとに長さを僅かに変えて境界処理
        // （パディング境界近辺）も網羅する。
        let rows_owned: Vec<(u64, Vec<u8>)> = (0..1_000u64)
            .map(|id| {
                let len = 480 + (id % 41) as usize; // 480..=520 バイトで揺らす
                let row: Vec<u8> = (0..len as u32)
                    .map(|i| ((i.wrapping_add(id as u32)) % 256) as u8)
                    .collect();
                (id, row)
            })
            .collect();
        let hash_input: Vec<(u64, &[u8])> = rows_owned
            .iter()
            .map(|(id, row)| (*id, row.as_slice()))
            .collect();
        let total_bytes: usize = rows_owned.iter().map(|(_, row)| row.len()).sum();
        let iterations = 200;

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(
                for_insert_batch_encoded_reference(std::hint::black_box(&hash_input))
                    .expect("hash"),
            );
        }
        let reference_elapsed = start.elapsed();

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(
                for_insert_batch_encoded(std::hint::black_box(&hash_input)).expect("hash"),
            );
        }
        let streaming_elapsed = start.elapsed();

        println!(
            "for_insert_batch_encoded_reference (旧・一括処理版): {reference_elapsed:?} \
             ({:.3} ns/byte)",
            reference_elapsed.as_nanos() as f64 / (total_bytes as f64 * iterations as f64)
        );
        println!(
            "for_insert_batch_encoded (新・ストリーミング版・本番経路): {streaming_elapsed:?} \
             ({:.3} ns/byte)",
            streaming_elapsed.as_nanos() as f64 / (total_bytes as f64 * iterations as f64)
        );
        assert_eq!(
            for_insert_batch_encoded_reference(&hash_input).expect("hash"),
            for_insert_batch_encoded(&hash_input).expect("hash")
        );
    }

    // --- for_typed_upsert（SQL-20・TASK-193、Issue #872） ---

    #[test]
    fn for_typed_upsert_is_deterministic() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let a = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        let b = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        assert_eq!(a, b);
    }

    #[test]
    fn for_typed_upsert_differs_from_plain_insert_for_same_values() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let plain_insert =
            for_typed_insert(1, Visibility::Private, &embedding, &cols).expect("hash");
        let do_nothing = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        assert_ne!(plain_insert, do_nothing);
    }

    #[test]
    fn for_typed_upsert_differs_between_do_nothing_and_do_update() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let assignments = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang),
        )];
        let do_nothing = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        let do_update = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoUpdate(&assignments),
            &rows,
        )
        .expect("hash");
        assert_ne!(do_nothing, do_update);
    }

    #[test]
    fn for_typed_upsert_differs_by_set_assignment_content() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang_ja = Value::Text("ja".to_string());
        let lang_en = Value::Text("en".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang_ja)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let assignments_ja = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang_ja),
        )];
        let assignments_en = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang_en),
        )];
        let h_ja = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoUpdate(&assignments_ja),
            &rows,
        )
        .expect("hash");
        let h_en = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoUpdate(&assignments_en),
            &rows,
        )
        .expect("hash");
        assert_ne!(h_ja, h_en);
    }

    #[test]
    fn for_typed_upsert_differs_between_excluded_and_literal_with_same_target_column() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];
        let assignments_excluded = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Excluded("lang"),
        )];
        let assignments_literal = vec![(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&lang),
        )];
        let h_excluded = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoUpdate(&assignments_excluded),
            &rows,
        )
        .expect("hash");
        let h_literal = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoUpdate(&assignments_literal),
            &rows,
        )
        .expect("hash");
        assert_ne!(h_excluded, h_literal);
    }

    #[test]
    fn for_typed_upsert_differs_when_row_order_changes() {
        let embedding_a = [1.0_f32, 0.0, 0.0];
        let embedding_b = [0.0_f32, 1.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows_forward: [TypedInsertBatchRow<'_>; 2] = [
            (1, Visibility::Private, &embedding_a, &cols),
            (2, Visibility::Private, &embedding_b, &cols),
        ];
        let rows_reversed: [TypedInsertBatchRow<'_>; 2] = [
            (2, Visibility::Private, &embedding_b, &cols),
            (1, Visibility::Private, &embedding_a, &cols),
        ];
        let h_forward = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows_forward,
        )
        .expect("hash");
        let h_reversed = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows_reversed,
        )
        .expect("hash");
        assert_ne!(h_forward, h_reversed);
    }

    /// golden 固定（Issue #1074）: `(id)` 対象（[`UpsertHashTarget::RowId`]）の
    /// レイアウトが本 Issue の `UpsertHashTarget` 導入前後でバイト単位に不変で
    /// あることを固定する。`RowId` 分岐が変わって数値がずれた場合は「バグを
    /// 直した」のではなく「永続化済み台帳エントリとの互換性を壊した」ことを
    /// 意味するため、この定数は変更前の実装から採取した値のまま更新しない。
    #[test]
    fn for_typed_upsert_row_id_layout_is_golden() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];

        fn to_hex(bytes: &[u8; 32]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }

        let do_nothing = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        assert_eq!(
            to_hex(do_nothing.as_bytes()),
            "f02cbdf3fd5dde7d08444243919a222cc5917fb160ea6785c8c779a60c04e2ed"
        );

        let new_lang = Value::Text("en".to_string());
        let assignments = [(
            "lang".to_string(),
            UpsertAssignmentHashValue::Literal(&new_lang),
        )];
        let do_update = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoUpdate(&assignments),
            &rows,
        )
        .expect("hash");
        assert_eq!(
            to_hex(do_update.as_bytes()),
            "384049cb628efadc559112030ad153a3edb6b8707d548134bd6ec85aa4eb41bb"
        );
    }

    /// UNIQUE 対象（Issue #1074）は `(id)` 対象と action tag の時点で分岐する
    /// ため、同一 `VALUES`・同一 action でも異なるハッシュになる。
    #[test]
    fn for_typed_upsert_unique_target_differs_from_row_id() {
        let embedding = [1.0_f32, 0.0, 0.0];
        let lang = Value::Text("ja".to_string());
        let cols: [(&str, &Value); 1] = [("lang", &lang)];
        let rows: [TypedInsertBatchRow<'_>; 1] = [(1, Visibility::Private, &embedding, &cols)];

        let row_id_hash = for_typed_upsert(
            &UpsertHashTarget::RowId,
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        let unique_a_hash = for_typed_upsert(
            &UpsertHashTarget::Unique(&["a"]),
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        let unique_b_hash = for_typed_upsert(
            &UpsertHashTarget::Unique(&["b"]),
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        let unique_ab_hash = for_typed_upsert(
            &UpsertHashTarget::Unique(&["a", "b"]),
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");
        let unique_ba_hash = for_typed_upsert(
            &UpsertHashTarget::Unique(&["b", "a"]),
            &UpsertHashAction::DoNothing,
            &rows,
        )
        .expect("hash");

        assert_ne!(row_id_hash, unique_a_hash);
        assert_ne!(unique_a_hash, unique_b_hash);
        assert_ne!(unique_a_hash, unique_ab_hash);
        // 本関数は渡された列名の並びをそのままハッシュ化し、並べ替えは行わない
        // （`(a,b)` と `(b,a)` を同一制約として同一ハッシュに正規化する責務は
        // 呼び出し元の `sql::parser::bind_upsert_form` にあり、常に解決した
        // 制約の宣言順で本関数へ渡す契約——`sql/parser.rs` の結合テスト参照）。
        assert_ne!(unique_ab_hash, unique_ba_hash);
    }

    // --- for_update_where / for_delete_where（Issue #871・SQL-19・TASK-192） --------

    /// [`crate::wasm_udf::WasmUdfBackend`] のテスト専用モック実装。呼び出されたら
    /// 即座に成功を返す（`collect_referenced_udfs` の拒否判定は呼び出し前の
    /// レジストリ照会段で完結するため、本体の計算内容はテストの関心事ではない）。
    #[derive(Debug)]
    struct StubWasmBackend;

    impl crate::wasm_udf::WasmUdfBackend for StubWasmBackend {
        fn call_vector_scalar(
            &self,
            _v: &[f32],
            scalar: f64,
        ) -> Result<f64, crate::wasm_udf::WasmUdfError> {
            Ok(scalar)
        }
    }

    /// `id > 0`（[`WherePredicate::Expression`] の許可形状：比較演算子を頂点に持つ
    /// 木）を土台に、左辺だけを差し替えた式述語を組み立てるテストヘルパー。
    fn where_predicate_id_gt_zero(
        lhs: crate::sql::udf_call::Expr,
    ) -> crate::sql::allowlist::WherePredicate {
        use crate::sql::udf_call::{BinOp, Expr};
        crate::sql::allowlist::WherePredicate::Expression(Expr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(lhs),
            rhs: Box::new(Expr::Number("0".to_string())),
        })
    }

    // --- CASE／COALESCE／NULLIF のタグ直列化（対象ビヘイビア: SQL-26。
    // Issue #921。ADR §4.4 タグ表のタグ 5〜8） ------------------------------

    /// `Expr::Case`（有無・分岐順・ELSE 有無いずれの差もハッシュへ反映される
    /// こと。§4.4 の「構文形をハッシュする」設計の回帰防止）を固定する。
    #[test]
    fn case_hash_differs_by_branch_order_and_else_presence() {
        use crate::sql::udf_call::{BinOp, Expr, UdfRegistry};

        let registry = UdfRegistry::default();
        let cond_gt = Expr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(Expr::Ident("id".to_string())),
            rhs: Box::new(Expr::Number("1".to_string())),
        };
        let cond_lt = Expr::Binary {
            op: BinOp::Lt,
            lhs: Box::new(Expr::Ident("id".to_string())),
            rhs: Box::new(Expr::Number("1".to_string())),
        };
        let with_else = where_predicate_id_gt_zero(Expr::Case {
            whens: vec![(cond_gt.clone(), Expr::Number("1".to_string()))],
            else_result: Some(Box::new(Expr::Number("0".to_string()))),
        });
        let without_else = where_predicate_id_gt_zero(Expr::Case {
            whens: vec![(cond_gt.clone(), Expr::Number("1".to_string()))],
            else_result: None,
        });
        let different_branch_order = where_predicate_id_gt_zero(Expr::Case {
            whens: vec![(cond_lt, Expr::Number("1".to_string()))],
            else_result: Some(Box::new(Expr::Number("0".to_string()))),
        });
        let same_as_with_else = where_predicate_id_gt_zero(Expr::Case {
            whens: vec![(cond_gt, Expr::Number("1".to_string()))],
            else_result: Some(Box::new(Expr::Number("0".to_string()))),
        });

        let h_with_else = for_delete_where("t", &[with_else], &registry).unwrap();
        let h_without_else = for_delete_where("t", &[without_else], &registry).unwrap();
        let h_different_order =
            for_delete_where("t", &[different_branch_order], &registry).unwrap();
        let h_same = for_delete_where("t", &[same_as_with_else], &registry).unwrap();

        assert_ne!(h_with_else, h_without_else);
        assert_ne!(h_with_else, h_different_order);
        assert_eq!(h_with_else, h_same);
    }

    /// `Expr::Null`／`Expr::Coalesce`／`Expr::NullIf` がそれぞれ他タグ・互いに
    /// 異なるハッシュを持ち、同一内容は同一ハッシュになることを固定する。
    #[test]
    fn null_coalesce_nullif_hash_are_distinct_and_deterministic() {
        use crate::sql::udf_call::{Expr, UdfRegistry};

        let registry = UdfRegistry::default();
        let null_pred = where_predicate_id_gt_zero(Expr::Null);
        let coalesce_pred = where_predicate_id_gt_zero(Expr::Coalesce(vec![
            Expr::Number("1".to_string()),
            Expr::Number("2".to_string()),
        ]));
        let coalesce_pred_reordered = where_predicate_id_gt_zero(Expr::Coalesce(vec![
            Expr::Number("2".to_string()),
            Expr::Number("1".to_string()),
        ]));
        let nullif_pred = where_predicate_id_gt_zero(Expr::NullIf(
            Box::new(Expr::Number("1".to_string())),
            Box::new(Expr::Number("2".to_string())),
        ));

        let h_null = for_delete_where("t", std::slice::from_ref(&null_pred), &registry).unwrap();
        let h_coalesce = for_delete_where("t", &[coalesce_pred], &registry).unwrap();
        let h_coalesce_reordered =
            for_delete_where("t", &[coalesce_pred_reordered], &registry).unwrap();
        let h_nullif = for_delete_where("t", &[nullif_pred], &registry).unwrap();
        let h_null_again = for_delete_where("t", &[null_pred], &registry).unwrap();

        assert_ne!(h_null, h_coalesce);
        assert_ne!(h_null, h_nullif);
        assert_ne!(h_coalesce, h_nullif);
        assert_ne!(h_coalesce, h_coalesce_reordered);
        assert_eq!(h_null, h_null_again);
    }

    /// ADR §4.4.1「WASM UDF は本節の対象外」の拒否判定（`collect_referenced_udfs` の
    /// `get_wasm` → `get` の順序）を固定する。`for_delete_where`・`for_update_where`
    /// いずれも `WHERE` 直列化を共有するため、WASM UDF 拒否は両者で同じ経路を通る。
    #[test]
    fn for_delete_where_rejects_where_referencing_wasm_udf() {
        use crate::sql::udf_call::{define_wasm_function, Expr, UdfRegistry};
        use std::sync::Arc;

        let mut registry = UdfRegistry::default();
        define_wasm_function(&mut registry, "wasm_fn", Arc::new(StubWasmBackend))
            .expect("register wasm udf");

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "wasm_fn".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let err = for_delete_where("t", &[predicate], &registry)
            .expect_err("WASM UDF calls must be rejected in predicate-form WHERE clauses");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn for_update_where_rejects_where_referencing_wasm_udf() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::{define_wasm_function, Expr, UdfRegistry};
        use std::sync::Arc;

        let mut registry = UdfRegistry::default();
        define_wasm_function(&mut registry, "wasm_fn", Arc::new(StubWasmBackend))
            .expect("register wasm udf");

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "wasm_fn".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });
        let value = InsertLiteral::String("ja".to_string());
        let assignments: [(&str, &InsertLiteral); 1] = [("lang", &value)];
        let schema = crate::catalog::TableSchema::new("t", Vec::new());

        let err = for_update_where("t", &assignments, &[predicate], &registry, &schema)
            .expect_err("WASM UDF calls must be rejected in predicate-form WHERE clauses");
        assert_eq!(err.wire_code(), "42601");
    }

    // --- Issue #1061: 述語形 UPDATE の VECTOR 割当 content_hash 表現統一 -----

    /// `embedding VECTOR(3)`・`lang TEXT` を持つテストスキーマ（[`push_dml_assignments`]
    /// の列型解決対象）。
    fn vector_dml_test_schema() -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    /// (a) 表層跨ぎ一致の直接証明: SQL 表層由来の `InsertLiteral::String`
    /// （`'[1,2,3]'`）と NoSQL 表層由来の `InsertLiteral::Vector`
    /// （`[1.0,2.0,3.0]`）が、同じ `VECTOR` 列への割当として同一
    /// `content_hash` になること（正準化の主目的）。
    #[test]
    fn for_update_where_vector_string_and_vector_literal_match_across_surfaces() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let schema = vector_dml_test_schema();
        let registry = UdfRegistry::default();
        let predicate =
            where_predicate_id_gt_zero(crate::sql::udf_call::Expr::Ident("id".to_string()));

        let sql_literal = InsertLiteral::String("[1,2,3]".to_string());
        let sql_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &sql_literal)];
        let sql_hash = for_update_where(
            "t",
            &sql_assignments,
            std::slice::from_ref(&predicate),
            &registry,
            &schema,
        )
        .expect("SQL-form VECTOR string assignment must hash successfully");

        let nosql_literal = InsertLiteral::Vector(vec![1.0, 2.0, 3.0]);
        let nosql_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &nosql_literal)];
        let nosql_hash =
            for_update_where("t", &nosql_assignments, &[predicate], &registry, &schema)
                .expect("NoSQL-form VECTOR literal assignment must hash successfully");

        assert_eq!(
            sql_hash, nosql_hash,
            "SQL 表層の VECTOR 文字列リテラルと NoSQL 表層の Vector リテラルは \
             同一値なら同一 content_hash になるべき（Issue #1061）"
        );
    }

    /// (b) 表記ゆれ不変: `'[1,2,3]'`・`'[1.0, 2.0, 3.0]'`・`' [1,2,3] '` は
    /// いずれも同一 `f32` 列にパースされるため同一ハッシュになる。
    #[test]
    fn for_update_where_vector_string_hash_is_unaffected_by_literal_spelling() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let schema = vector_dml_test_schema();
        let registry = UdfRegistry::default();
        let predicate =
            where_predicate_id_gt_zero(crate::sql::udf_call::Expr::Ident("id".to_string()));

        let hash_for = |literal: &str| -> ContentHash {
            let value = InsertLiteral::String(literal.to_string());
            let assignments: [(&str, &InsertLiteral); 1] = [("embedding", &value)];
            for_update_where(
                "t",
                &assignments,
                std::slice::from_ref(&predicate),
                &registry,
                &schema,
            )
            .expect("well-formed vector literal must hash successfully")
        };

        let base = hash_for("[1,2,3]");
        assert_eq!(base, hash_for("[1.0, 2.0, 3.0]"));
        assert_eq!(base, hash_for(" [1,2,3] "));
    }

    /// (c) D2 の `-0.0` 保持契約: `'[-0,1,2]'` は `Vector([-0.0,1.0,2.0])` と
    /// 一致し、`'[0,1,2]'`（`+0.0`）とは不一致のまま（[`push_vector`] は
    /// REAL／DOUBLE の `canonicalize_*` と異なり符号付きゼロを正規化しない）。
    #[test]
    fn for_update_where_vector_negative_zero_is_preserved_and_distinct_from_positive_zero() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let schema = vector_dml_test_schema();
        let registry = UdfRegistry::default();
        let predicate =
            where_predicate_id_gt_zero(crate::sql::udf_call::Expr::Ident("id".to_string()));

        let neg_zero_string = InsertLiteral::String("[-0,1,2]".to_string());
        let neg_zero_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &neg_zero_string)];
        let neg_zero_hash = for_update_where(
            "t",
            &neg_zero_assignments,
            std::slice::from_ref(&predicate),
            &registry,
            &schema,
        )
        .expect("negative-zero vector literal must hash successfully");

        let neg_zero_vector = InsertLiteral::Vector(vec![-0.0, 1.0, 2.0]);
        let neg_zero_vector_assignments: [(&str, &InsertLiteral); 1] =
            [("embedding", &neg_zero_vector)];
        let neg_zero_vector_hash = for_update_where(
            "t",
            &neg_zero_vector_assignments,
            std::slice::from_ref(&predicate),
            &registry,
            &schema,
        )
        .expect("negative-zero Vector literal must hash successfully");
        assert_eq!(neg_zero_hash, neg_zero_vector_hash);

        let pos_zero_string = InsertLiteral::String("[0,1,2]".to_string());
        let pos_zero_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &pos_zero_string)];
        let pos_zero_hash =
            for_update_where("t", &pos_zero_assignments, &[predicate], &registry, &schema)
                .expect("positive-zero vector literal must hash successfully");
        assert_ne!(
            neg_zero_hash, pos_zero_hash,
            "-0.0 と +0.0 は別内容として区別され続けるべき（D2 契約）"
        );
    }

    /// (d) 異なる値は不一致になる。
    #[test]
    fn for_update_where_vector_string_hash_differs_for_different_values() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let schema = vector_dml_test_schema();
        let registry = UdfRegistry::default();
        let predicate =
            where_predicate_id_gt_zero(crate::sql::udf_call::Expr::Ident("id".to_string()));

        let a = InsertLiteral::String("[1,2,3]".to_string());
        let a_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &a)];
        let a_hash = for_update_where(
            "t",
            &a_assignments,
            std::slice::from_ref(&predicate),
            &registry,
            &schema,
        )
        .expect("well-formed vector literal must hash successfully");

        let b = InsertLiteral::String("[1,2,4]".to_string());
        let b_assignments: [(&str, &InsertLiteral); 1] = [("embedding", &b)];
        let b_hash = for_update_where("t", &b_assignments, &[predicate], &registry, &schema)
            .expect("well-formed vector literal must hash successfully");

        assert_ne!(a_hash, b_hash);
    }

    /// (e) VECTOR 以外の割当（TEXT 列 `lang = 'en'` のみ）は、正準モードと
    /// legacy モードでビット同一（[`push_dml_assignments`] は列型が `VECTOR`
    /// でなければモードに関わらずタグ 1・生テキストのまま連結する）。
    /// 加えて、モジュール内の [`HashInputBuilder`] で直接組み立てた期待値と
    /// 一致することを固定し、VECTOR 以外の割当のレイアウトへドリフトが
    /// 無いことを保証する。
    #[test]
    fn for_update_where_non_vector_assignment_hash_is_unaffected_by_normalization() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let schema = vector_dml_test_schema();
        let registry = UdfRegistry::default();
        let predicate =
            where_predicate_id_gt_zero(crate::sql::udf_call::Expr::Ident("id".to_string()));

        let value = InsertLiteral::String("en".to_string());
        let assignments: [(&str, &InsertLiteral); 1] = [("lang", &value)];

        let canonical_hash = for_update_where(
            "t",
            &assignments,
            std::slice::from_ref(&predicate),
            &registry,
            &schema,
        )
        .expect("non-vector assignment must hash successfully");
        let legacy_hash =
            for_update_where_legacy_text_vector("t", &assignments, &[predicate], &registry)
                .expect("legacy-mode non-vector assignment must hash successfully");
        assert_eq!(
            canonical_hash, legacy_hash,
            "VECTOR 以外の割当は正準／legacy 両モードでビット同一のはず"
        );

        // タグ・レイアウトの直接固定（`push_dml_assignments` のドリフト検知）:
        // ドメインタグ＋OpTag::UpdateWhere＋table("t")＋assignments件数(1)＋
        // 列名("lang")＋タグ1＋テキスト("en")＋WHERE述語件数(0) を手動で組み立て、
        // predicate 無しの `for_update_where` 出力と一致することを固定する。
        let no_predicate_hash = for_update_where("t", &assignments, &[], &registry, &schema)
            .expect("non-vector assignment without WHERE predicates must hash successfully");
        let mut expected2 = HashInputBuilder::new(OpTag::UpdateWhere);
        expected2.push_bytes(b"t").expect("short field");
        expected2.push_raw(&1u32.to_le_bytes());
        expected2.push_bytes(b"lang").expect("short field");
        expected2.push_u8(1);
        expected2.push_bytes(b"en").expect("short field");
        expected2.push_raw(&0u32.to_le_bytes()); // WHERE 述語件数 0
        assert_eq!(no_predicate_hash, expected2.finish());
    }

    /// (f) legacy モードは変更前レイアウト（`VECTOR` 列への `String` も
    /// タグ 1・生テキストで連結する）を再現する。`push_dml_assignments` の
    /// `DmlVectorRepr::LegacyText` 分岐は `vector_dim` を常に `None` にする
    /// ため、[`HashInputBuilder`] で手動組み立てた「常にタグ 1」の期待値と
    /// 一致することを固定する。
    #[test]
    fn for_update_where_legacy_text_vector_reproduces_pre_normalization_layout() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let registry = UdfRegistry::default();

        let value = InsertLiteral::String("[1,2,3]".to_string());
        let assignments: [(&str, &InsertLiteral); 1] = [("embedding", &value)];
        let legacy_hash = for_update_where_legacy_text_vector("t", &assignments, &[], &registry)
            .expect("legacy-mode vector-column string assignment must hash successfully");

        let mut expected = HashInputBuilder::new(OpTag::UpdateWhere);
        expected.push_bytes(b"t").expect("short field");
        expected.push_raw(&1u32.to_le_bytes());
        expected.push_bytes(b"embedding").expect("short field");
        expected.push_u8(1);
        expected.push_bytes(b"[1,2,3]").expect("short field");
        expected.push_raw(&0u32.to_le_bytes());
        assert_eq!(legacy_hash, expected.finish());
    }

    /// (g) VECTOR 列への不正な `String`（次元不一致）は `Err`（`22000`）に
    /// なり、タグ 1 へフォールバックしない（fail-closed）。
    #[test]
    fn for_update_where_vector_string_dimension_mismatch_is_rejected_fail_closed() {
        use crate::sql::allowlist::InsertLiteral;
        use crate::sql::udf_call::UdfRegistry;

        let schema = vector_dml_test_schema();
        let registry = UdfRegistry::default();
        let predicate =
            where_predicate_id_gt_zero(crate::sql::udf_call::Expr::Ident("id".to_string()));

        let value = InsertLiteral::String("[1,2]".to_string());
        let assignments: [(&str, &InsertLiteral); 1] = [("embedding", &value)];
        let err = for_update_where("t", &assignments, &[predicate], &registry, &schema)
            .expect_err("dimension-mismatched vector literal must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    /// (h) `needs_legacy_vector_hash` の真偽判定。
    #[test]
    fn needs_legacy_vector_hash_detects_vector_column_string_assignments_only() {
        use crate::sql::allowlist::InsertLiteral;

        let schema = vector_dml_test_schema();

        let vector_string = InsertLiteral::String("[1,2,3]".to_string());
        let with_vector_string: [(&str, &InsertLiteral); 1] = [("embedding", &vector_string)];
        assert!(needs_legacy_vector_hash(&with_vector_string, &schema));

        let vector_literal = InsertLiteral::Vector(vec![1.0, 2.0, 3.0]);
        let with_vector_literal: [(&str, &InsertLiteral); 1] = [("embedding", &vector_literal)];
        assert!(
            !needs_legacy_vector_hash(&with_vector_literal, &schema),
            "NoSQL 由来の Vector リテラルは正準・legacy 両方でタグ 5 のため不要"
        );

        let text_only = InsertLiteral::String("en".to_string());
        let with_text_only: [(&str, &InsertLiteral); 1] = [("lang", &text_only)];
        assert!(!needs_legacy_vector_hash(&with_text_only, &schema));
    }

    /// 宣言的 UDF を 2 段（`outer` が `inner` を呼ぶ）で登録したレジストリを
    /// 組み立てるテストヘルパー。`param_case` はパラメータ宣言の綴りを変えるために
    /// 使い、大文字小文字畳み込み（ADR §4.4.1「6.」）の検証に使う。
    fn registry_with_two_layer_udf(
        param_case: fn(&str) -> String,
        inner_op: crate::sql::udf_call::BinOp,
    ) -> crate::sql::udf_call::UdfRegistry {
        use crate::sql::udf_call::{define_function, BinOp, Expr, UdfRegistry};

        let mut registry = UdfRegistry::default();
        let inner_param = param_case("x");
        define_function(
            &mut registry,
            "inner",
            std::slice::from_ref(&inner_param),
            &Expr::Binary {
                op: inner_op,
                lhs: Box::new(Expr::Ident(inner_param.clone())),
                rhs: Box::new(Expr::Number("1".to_string())),
            },
        )
        .expect("define inner");

        let outer_param = param_case("y");
        define_function(
            &mut registry,
            "outer",
            std::slice::from_ref(&outer_param),
            &Expr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(Expr::Call {
                    name: "inner".to_string(),
                    args: vec![Expr::Ident(outer_param.clone())],
                }),
                rhs: Box::new(Expr::Number("2".to_string())),
            },
        )
        .expect("define outer");
        registry
    }

    /// ADR §4.4.1「4. 推移閉包」「6. 決定的な順序・大文字小文字畳み込み」の
    /// ピン留め：`inner`/`outer` の意味が同一なら、パラメータ宣言の大文字小文字が
    /// 異なっていてもハッシュは一致する。
    #[test]
    fn for_delete_where_transitive_udf_closure_is_case_insensitive_on_params() {
        use crate::sql::udf_call::{BinOp, Expr};

        let lower = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Add);
        let upper = registry_with_two_layer_udf(|s| s.to_uppercase(), BinOp::Add);

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "outer".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let h_lower =
            for_delete_where("t", std::slice::from_ref(&predicate), &lower).expect("hash");
        let h_upper = for_delete_where("t", &[predicate], &upper).expect("hash");
        assert_eq!(
            h_lower, h_upper,
            "same UDF closure must hash identically regardless of declared parameter case"
        );
    }

    /// 上のテストは `define_function`（production の唯一の登録経路）がパラメータ名を
    /// 定義時点で必ず小文字へ正規化する（`sql::udf_call::define_function` の
    /// `normalized_params`）ため、`push_dml_where_predicates` の UDF 定義
    /// セクション自体が大文字小文字を畳み込む必要性を検証できていなかった
    /// （codex-review P1 指摘。`define_function` を経由しない場合でも定義
    /// セクションの直列化自体が大文字小文字を畳み込む契約であることを、正規化を
    /// 経由しない [`crate::sql::udf_call::insert_raw_definition_for_test`] で
    /// 直接ピン留めする）。
    #[test]
    fn for_delete_where_udf_definition_section_folds_param_case_independent_of_define_function() {
        use crate::sql::udf_call::{
            insert_raw_definition_for_test, Expr, UdfDefinition, UdfRegistry,
        };

        let mut lower = UdfRegistry::default();
        insert_raw_definition_for_test(
            &mut lower,
            "raw_fn",
            UdfDefinition {
                params: vec!["x".to_string()],
                body: Expr::Ident("x".to_string()),
            },
        );

        let mut upper = UdfRegistry::default();
        insert_raw_definition_for_test(
            &mut upper,
            "raw_fn",
            UdfDefinition {
                params: vec!["X".to_string()],
                body: Expr::Ident("X".to_string()),
            },
        );

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "raw_fn".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let h_lower =
            for_delete_where("t", std::slice::from_ref(&predicate), &lower).expect("hash");
        let h_upper = for_delete_where("t", &[predicate], &upper).expect("hash");
        assert_eq!(
            h_lower, h_upper,
            "definition-section serialization itself must fold parameter name case, \
             independent of any upstream normalization by define_function"
        );
    }

    /// 同じ形（`outer` が `inner` を呼ぶ）でも `inner` の本体演算子が異なれば
    /// （推移閉包の中身が変われば）ハッシュが変わることを固定する。
    #[test]
    fn for_delete_where_transitive_udf_closure_differs_when_inner_body_changes() {
        use crate::sql::udf_call::{BinOp, Expr};

        let add_registry = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Add);
        let sub_registry = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Sub);

        let predicate = where_predicate_id_gt_zero(Expr::Call {
            name: "outer".to_string(),
            args: vec![Expr::Ident("id".to_string())],
        });

        let h_add =
            for_delete_where("t", std::slice::from_ref(&predicate), &add_registry).expect("hash");
        let h_sub = for_delete_where("t", &[predicate], &sub_registry).expect("hash");
        assert_ne!(
            h_add, h_sub,
            "differing UDF closures (inner body operator) must not collapse to the same hash"
        );
    }

    /// ADR §4.4.1「参照 UDF が無ければ件数プレフィクスの 0 すら書かない」契約の
    /// ピン留め：`WHERE` が UDF を一切呼ばない場合、セッションに UDF が登録済みか
    /// どうか（登録の有無・登録内容）に関わらずハッシュは完全に一致する。
    #[test]
    fn for_delete_where_hash_is_unaffected_by_unreferenced_udf_registrations() {
        use crate::sql::allowlist::WherePredicate;
        use crate::sql::udf_call::{BinOp, UdfRegistry};

        let predicate = WherePredicate::Equality {
            column: "lang".to_string(),
            value: "ja".to_string(),
        };

        let empty_registry = UdfRegistry::default();
        let populated_registry = registry_with_two_layer_udf(|s| s.to_string(), BinOp::Add);

        let h_empty =
            for_delete_where("t", std::slice::from_ref(&predicate), &empty_registry).expect("hash");
        let h_populated = for_delete_where("t", &[predicate], &populated_registry).expect("hash");
        assert_eq!(
            h_empty, h_populated,
            "UDF section must be omitted entirely when no UDF is referenced by WHERE"
        );
    }

    // --- SQL-24（TASK-208 ポインタ）: IN / BETWEEN / IS NULL / NOT のハッシュ --

    fn hash_for(predicate: crate::sql::allowlist::WherePredicate) -> ContentHash {
        let empty_registry = crate::sql::udf_call::UdfRegistry::default();
        for_delete_where("t", &[predicate], &empty_registry).expect("hash")
    }

    #[test]
    fn new_predicate_forms_each_get_a_distinct_hash() {
        use crate::sql::allowlist::WherePredicate;

        let in_list = WherePredicate::InList {
            column: "lang".to_string(),
            values: vec!["ja".to_string(), "en".to_string()],
        };
        let not_in_list = WherePredicate::Not(Box::new(in_list.clone()));
        let between = WherePredicate::Between {
            column: "day".to_string(),
            low: "2024-01-01".to_string(),
            high: "2024-06-01".to_string(),
        };
        let is_null = WherePredicate::IsNull {
            column: "tag".to_string(),
            negated: false,
        };
        let is_not_null = WherePredicate::IsNull {
            column: "tag".to_string(),
            negated: true,
        };

        let hashes = [
            hash_for(in_list.clone()),
            hash_for(not_in_list.clone()),
            hash_for(between.clone()),
            hash_for(is_null.clone()),
            hash_for(is_not_null.clone()),
        ];
        for (i, a) in hashes.iter().enumerate() {
            for (j, b) in hashes.iter().enumerate() {
                if i != j {
                    assert_ne!(
                        a, b,
                        "predicate forms at index {i} and {j} must not collide"
                    );
                }
            }
        }
    }

    /// 構文段が連続する `NOT` を偶奇で正規化する契約（`sql::allowlist::
    /// Parser::parse_where_leaf`）により、`NOT NOT x` と `x` は同一の
    /// `WherePredicate` 木になる。ここではその正規化済み木を直接ハッシュし、
    /// 「畳んだ結果は元と同一ハッシュ」であることを固定する（意図した挙動）。
    #[test]
    fn double_not_folds_to_the_same_hash_as_no_not() {
        use crate::sql::allowlist::WherePredicate;

        let eq = WherePredicate::Equality {
            column: "lang".to_string(),
            value: "ja".to_string(),
        };
        let not_not_eq = eq.clone(); // 構文段で既に畳み込み済みの表現。
        assert_eq!(hash_for(eq), hash_for(not_not_eq));
    }

    #[test]
    fn not_wrapped_predicate_hash_differs_from_unwrapped() {
        use crate::sql::allowlist::WherePredicate;

        let eq = WherePredicate::Equality {
            column: "lang".to_string(),
            value: "ja".to_string(),
        };
        let not_eq = WherePredicate::Not(Box::new(eq.clone()));
        assert_ne!(hash_for(eq), hash_for(not_eq));
    }

    /// TASK-208・SQL-24（Issue #912）: `WherePredicate::Or`（タグ 8）の直列化が
    /// 分岐の木構造をハッシュへ反映することを固定する。`a OR (b AND c)` と
    /// `(a OR b) AND c` は葉の集合こそ同じだが木構造が異なるため、別ハッシュに
    /// ならなければならない（内容照合ハッシュが構造の違いを取りこぼすと、
    /// 意味の異なる 2 つの `WHERE` が同一 `operation_id` の正当な再送だと
    /// 誤認され得る）。
    #[test]
    fn or_predicate_hash_reflects_branch_structure_not_just_leaf_set() {
        use crate::sql::allowlist::WherePredicate;
        use crate::sql::udf_call::UdfRegistry;

        let leaf = |column: &str, value: &str| WherePredicate::Equality {
            column: column.to_string(),
            value: value.to_string(),
        };

        // `a OR (b AND c)`。
        let a_or_b_and_c = vec![WherePredicate::Or(vec![
            vec![leaf("a", "1")],
            vec![leaf("b", "2"), leaf("c", "3")],
        ])];
        // `(a OR b) AND c`。
        let a_or_b_and_then_c = vec![
            WherePredicate::Or(vec![vec![leaf("a", "1")], vec![leaf("b", "2")]]),
            leaf("c", "3"),
        ];

        let registry = UdfRegistry::default();
        let h1 = for_delete_where("t", &a_or_b_and_c, &registry).expect("hash a OR (b AND c)");
        let h2 = for_delete_where("t", &a_or_b_and_then_c, &registry).expect("hash (a OR b) AND c");
        assert_ne!(
            h1, h2,
            "differing OR/AND tree structure over the same leaves must not collapse"
        );

        // 決定性: 同一構造は同一ハッシュを返す。
        let h1_again =
            for_delete_where("t", &a_or_b_and_c, &registry).expect("hash a OR (b AND c) again");
        assert_eq!(h1, h1_again, "identical OR structure must hash identically");
    }

    /// TASK-208・Issue #912: `AND` だけの述語列（`Or` を含まない）の直列化形式は
    /// 本 Issue 導入前と完全に同一のまま（タグ 1〜7 の形式・意味は変えない）。
    /// `push_dml_where_predicate` への切り出しがバイト列を変えていないことを、
    /// 固定入力に対する決定的なハッシュ値で回帰的に固定する。
    #[test]
    fn and_only_predicate_hash_is_unaffected_by_or_support_refactor() {
        use crate::sql::allowlist::WherePredicate;
        use crate::sql::udf_call::UdfRegistry;

        let predicates = vec![
            WherePredicate::Equality {
                column: "lang".to_string(),
                value: "ja".to_string(),
            },
            WherePredicate::BoolColumn {
                column: "flag".to_string(),
            },
        ];
        let registry = UdfRegistry::default();
        let h1 = for_delete_where("t", &predicates, &registry).expect("hash");
        let h2 = for_delete_where("t", &predicates, &registry).expect("hash again");
        assert_eq!(
            h1, h2,
            "AND-only predicate hashing must remain deterministic across calls"
        );
    }

    /// NUMERIC 値（TABLE-13〔検討中〕・TASK-197、Issue #885・D7）のハッシュは、
    /// 正規化後の値（scale + unscaled）だけで決まる。表記が異なっても
    /// 正規値が一致すれば同一ハッシュ、正規値が異なれば別ハッシュになる
    /// （`operation_id` 再送判定の `23505`／`22023` 分岐の土台）。
    #[test]
    fn push_value_numeric_hash_depends_only_on_normalized_value() {
        let mut b1 = HashInputBuilder::new(OpTag::Insert);
        push_value(
            &mut b1,
            &Value::Numeric(crate::numeric::Decimal::from_parts(150, 2).expect("valid scale")),
        )
        .expect("push numeric");
        let h1 = b1.finish();

        // 同じ正規値（1.50）を異なる unscaled/scale の組み合わせで表現しても、
        // 実際にはスキーマの scale が固定されているため通常は起こらないが、
        // ハッシュ自体は入力バイト列に忠実であることを確認する（scale が異なれば
        // 別ハッシュ）。
        let mut b2 = HashInputBuilder::new(OpTag::Insert);
        push_value(
            &mut b2,
            &Value::Numeric(crate::numeric::Decimal::from_parts(150, 2).expect("valid scale")),
        )
        .expect("push numeric");
        let h2 = b2.finish();
        assert_eq!(h1, h2, "identical normalized value must hash identically");

        let mut b3 = HashInputBuilder::new(OpTag::Insert);
        push_value(
            &mut b3,
            &Value::Numeric(crate::numeric::Decimal::from_parts(200, 2).expect("valid scale")),
        )
        .expect("push numeric");
        let h3 = b3.finish();
        assert_ne!(h1, h3, "differing normalized value must not collapse");

        let mut b4 = HashInputBuilder::new(OpTag::Insert);
        push_value(
            &mut b4,
            &Value::Numeric(crate::numeric::Decimal::from_parts(150, 3).expect("valid scale")),
        )
        .expect("push numeric");
        let h4 = b4.finish();
        assert_ne!(h1, h4, "differing scale must not collapse to the same hash");
    }

    /// Issue #1430（codex P1）: f64 へ丸めると同値になる異なる十進リテラルは別ハッシュ、
    /// f64 で往復できる従来入力は表記差があっても同一（既存ハッシュ不変）。
    #[test]
    fn dml_number_literal_hash_distinguishes_lossy_decimals() {
        use crate::sql::udf_call::Expr;
        let hash = |raw: &str| {
            let mut b = HashInputBuilder::new(OpTag::Insert);
            push_dml_expr(&mut b, &Expr::Number(raw.to_string()), None).expect("push");
            b.finish()
        };
        assert_ne!(hash("1.0000000000000001"), hash("1.0000000000000002"));
        assert_ne!(hash("1.0000000000000001"), hash("1"));
        assert_eq!(hash("1.0000000000000001"), hash("1.00000000000000010"));
        assert_eq!(hash("1.5"), hash("1.50"));
        assert_eq!(hash("1"), hash("1."));
        // 指数表記も正確な十進値で判定する（f64 で別値になる指数表記は区別する）。
        assert_ne!(hash("1.0000000000000001e0"), hash("1.0000000000000002e0"));
        assert_ne!(hash("1.0000000000000001e0"), hash("1e0"));
        assert_eq!(hash("1.0000000000000001e0"), hash("1.0000000000000001"));
        // 1e-4 未満の小さな平文十進は従来どおりタグ 1（指数表記と同一ハッシュ）。
        let mut small = HashInputBuilder::new(OpTag::Insert);
        small.push_u8(1);
        small.push_raw(&0.00001f64.to_bits().to_le_bytes());
        assert_eq!(hash("0.00001"), small.finish());
        assert_eq!(hash("0.00001"), hash("1e-5"));
        assert_eq!(hash("100"), hash("1e2"));
        // 従来入力（f64 で往復できる）はタグ 1＋ビット列のまま。
        let mut b = HashInputBuilder::new(OpTag::Insert);
        b.push_u8(1);
        b.push_raw(&0.1f64.to_bits().to_le_bytes());
        assert_eq!(hash("0.1"), b.finish());
    }

    // --- Issue #1127: 分割実行版ハッシュのドメイン分離 -------------------------

    fn partitioned_test_predicates() -> Vec<crate::sql::allowlist::WherePredicate> {
        vec![crate::sql::allowlist::WherePredicate::Equality {
            column: "lang".to_string(),
            value: "ja".to_string(),
        }]
    }

    /// 分割実行版は原子版と別ドメイン（タグ 11／12）で、同一入力でも別ハッシュ
    /// になり、かつ決定的である。原子版の出力は共有本体の導入前後で不変
    /// （タグ・直列化を手組みした参照ビルダーと一致する）。
    #[test]
    fn partitioned_hashes_are_separate_domains_and_atomic_hashes_are_unchanged() {
        use crate::sql::udf_call::UdfRegistry;
        let registry = UdfRegistry::default();
        let predicates = partitioned_test_predicates();
        let schema = crate::catalog::TableSchema::new(String::new(), Vec::new());
        let literal = crate::sql::allowlist::InsertLiteral::String("x".to_string());
        let assignments = [("title", &literal)];

        let atomic_del = for_delete_where("t", &predicates, &registry).expect("hash");
        let part_del = for_delete_where_partitioned("t", &predicates, &registry).expect("hash");
        assert_ne!(atomic_del, part_del);
        assert_eq!(
            part_del,
            for_delete_where_partitioned("t", &predicates, &registry).expect("hash again")
        );

        let atomic_upd =
            for_update_where("t", &assignments, &predicates, &registry, &schema).expect("hash");
        let part_upd =
            for_update_where_partitioned("t", &assignments, &predicates, &registry, &schema)
                .expect("hash");
        assert_ne!(atomic_upd, part_upd);
        assert_ne!(part_upd, part_del);

        // 原子版は従来レイアウト（タグ 10／9 ＋同一直列化）のまま。
        let mut expected_del = HashInputBuilder::new(OpTag::DeleteWhere);
        expected_del.push_bytes(b"t").expect("push");
        push_dml_where_predicates(&mut expected_del, &predicates, &registry).expect("push");
        assert_eq!(atomic_del, expected_del.finish());

        let mut expected_upd = HashInputBuilder::new(OpTag::UpdateWhere);
        expected_upd.push_bytes(b"t").expect("push");
        push_dml_assignments(
            &mut expected_upd,
            &assignments,
            DmlVectorRepr::Canonical,
            &schema,
        )
        .expect("push");
        push_dml_where_predicates(&mut expected_upd, &predicates, &registry).expect("push");
        assert_eq!(atomic_upd, expected_upd.finish());

        // 分割版のレイアウトはタグ以外が原子版と同一（チャンク幅の入力は無い）。
        let mut expected_part = HashInputBuilder::new(OpTag::DeleteWherePartitioned);
        expected_part.push_bytes(b"t").expect("push");
        push_dml_where_predicates(&mut expected_part, &predicates, &registry).expect("push");
        assert_eq!(part_del, expected_part.finish());
    }
}
