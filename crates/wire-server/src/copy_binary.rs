//! `COPY ... WITH (FORMAT binary)` のフレーム層（WIRE-23 (1)(2)・Issue #1767・親 #1766）。
//!
//! 役割: PostgreSQL の binary COPY ストリーム（署名・フラグ・ヘッダ拡張・タプル・トレーラ。
//! 形式は PostgreSQL 公式ドキュメント「COPY — Binary Format」）を、**列の型を知らずに**
//! フィールド単位のバイト列へ分解する [`BinaryCopyReader`] と、同じ形式を書き出す
//! `encode_*_into` を提供する。型ごとの変換は #1768、`run_copy_from` への結線は #1769、
//! `run_copy_to` への結線は #1770 が担う（本モジュールの呼び出し元の予定）。
//!
//! engine の text/csv 用 `RecordSplitter` と違い wire-server に置くのは、binary フィールドを
//! 型コーデック（`binary_param`・`result_encoder::binary` と同じ層。[`crate::copy_binary_types`]）で変換してから engine の
//! 束縛へ渡す流れにし、型情報の無いバイト列を engine へ渡さないため。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを使わず、長さは `try_from`／`checked_*`
//! で扱い、宣言長を確保量に使わない（fail-closed）。入力全体の量は #1769 が INDEX-4 の
//! 生 CopyData 累計上限で有界にする（ヘッダ拡張はメモリ O(1) で読み飛ばす）。
//! エラー文言は固定文字列で、受信値・他テナントの情報を含めない。
#![cfg_attr(not(test), allow(dead_code))] // 本番の呼び出し元は #1768〜#1770 が結線する。

/// 署名（11 バイト）。
const SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";
/// flags の OID 有無ビット（bit 16）。
const FLAG_OIDS: u32 = 1 << 16;
/// flags の critical 領域（bit 17〜31）。
const FLAGS_CRITICAL_MASK: u32 = 0xFFFE_0000;

/// binary COPY フレーム層のエラー。`sqlstate`／`client_message` で wire への写像を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyBinaryError {
    /// 署名・flags・ヘッダ拡張長の不正（22P04）。
    InvalidHeader,
    /// タプルのフィールド数が期待列数と不一致（22P04）。
    FieldCountMismatch,
    /// フィールド長が不正（`-1` 未満。22P04）。
    InvalidFieldLength,
    /// データが途中で終わった（22P04）。
    UnexpectedEof,
    /// トレーラの後にデータが続いた（22P04）。
    DataAfterTrailer,
    /// フィールド長またはタプル合計が上限超過（54000）。
    LimitExceeded,
    /// writer の変換オーバーフロー・parser の構成不正（XX000。内部不整合）。
    EncodeOverflow,
}

impl CopyBinaryError {
    /// SQLSTATE（`ErrorClass` への写像は 22P04 の variant 追加を伴うため #1769 で行う）。
    pub(crate) fn sqlstate(&self) -> &'static str {
        match self {
            Self::InvalidHeader
            | Self::FieldCountMismatch
            | Self::InvalidFieldLength
            | Self::UnexpectedEof
            | Self::DataAfterTrailer => "22P04",
            Self::LimitExceeded => "54000",
            Self::EncodeOverflow => "XX000",
        }
    }

    /// クライアントへ返す固定の英語文言（受信値を含めない）。
    pub(crate) fn client_message(&self) -> &'static str {
        match self {
            Self::InvalidHeader => "invalid COPY file header",
            Self::FieldCountMismatch
            | Self::InvalidFieldLength
            | Self::UnexpectedEof
            | Self::DataAfterTrailer => "invalid COPY data",
            Self::LimitExceeded => "COPY binary field exceeds the size limit",
            Self::EncodeOverflow => "internal error in COPY binary framing",
        }
    }
}

/// parser のメモリ上限（確保前に検証する）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct BinaryFrameLimits {
    /// 1 フィールドの最大バイト数。
    pub max_field_bytes: usize,
    /// 1 タプルのフィールド本文合計の最大バイト数。
    pub max_tuple_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Signature { matched: usize },
    Flags { buf: [u8; 4], filled: usize },
    ExtLen { buf: [u8; 4], filled: usize },
    ExtSkip { remaining: u32 },
    TupleStart { buf: [u8; 2], filled: usize },
    FieldLen { buf: [u8; 4], filled: usize },
    FieldBody { remaining: usize },
    AfterTrailer,
    Failed,
}

/// チャンク境界をまたぐストリーミング parser。CopyData の分割位置に依存しない。
#[derive(Debug)]
pub(crate) struct BinaryCopyReader {
    state: State,
    expected_fields: usize,
    limits: BinaryFrameLimits,
    current: Vec<Option<Vec<u8>>>,
    body: Vec<u8>,
    tuple_bytes: usize,
}

/// `buf[*filled..]` を `chunk[*pos..]` から埋め、満了したら true。
fn fill(buf: &mut [u8], filled: &mut usize, chunk: &[u8], pos: &mut usize) -> bool {
    let rest = chunk.get(*pos..).unwrap_or(&[]);
    let need = buf.len().saturating_sub(*filled);
    let n = rest.len().min(need);
    let end = filled.saturating_add(n);
    if let (Some(dst), Some(src)) = (buf.get_mut(*filled..end), rest.get(..n)) {
        dst.copy_from_slice(src);
        *filled = end;
        *pos = pos.saturating_add(n);
    }
    *filled >= buf.len()
}

impl BinaryCopyReader {
    /// `expected_fields` は 1..=`i16::MAX`（範囲外は構成不正として `EncodeOverflow`）。
    pub(crate) fn new(
        expected_fields: usize,
        limits: BinaryFrameLimits,
    ) -> Result<Self, CopyBinaryError> {
        if expected_fields == 0 || i16::try_from(expected_fields).is_err() {
            return Err(CopyBinaryError::EncodeOverflow);
        }
        Ok(Self {
            state: State::Signature { matched: 0 },
            expected_fields,
            limits,
            current: Vec::new(),
            body: Vec::new(),
            tuple_bytes: 0,
        })
    }

    /// CopyData の payload を投入し、完成したタプルごとに `on_tuple` を呼ぶ。
    /// 一度エラーになった後は以降の入力をすべて拒否する（poisoned）。
    pub(crate) fn feed<E: From<CopyBinaryError>>(
        &mut self,
        chunk: &[u8],
        mut on_tuple: impl FnMut(Vec<Option<Vec<u8>>>) -> Result<(), E>,
    ) -> Result<(), E> {
        let result = self.feed_inner(chunk, &mut on_tuple);
        if result.is_err() {
            self.state = State::Failed;
        }
        result
    }

    fn feed_inner<E: From<CopyBinaryError>>(
        &mut self,
        chunk: &[u8],
        on_tuple: &mut impl FnMut(Vec<Option<Vec<u8>>>) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut pos = 0usize;
        while pos < chunk.len() {
            match self.state {
                State::Failed => return Err(CopyBinaryError::UnexpectedEof.into()),
                State::AfterTrailer => return Err(CopyBinaryError::DataAfterTrailer.into()),
                State::Signature { matched } => {
                    let rest = chunk.get(pos..).unwrap_or(&[]);
                    let want = SIGNATURE.get(matched..).unwrap_or(&[]);
                    let n = rest.len().min(want.len());
                    if rest.get(..n) != want.get(..n) {
                        return Err(CopyBinaryError::InvalidHeader.into());
                    }
                    pos = pos.saturating_add(n);
                    let matched = matched.saturating_add(n);
                    self.state = if matched >= SIGNATURE.len() {
                        State::Flags {
                            buf: [0; 4],
                            filled: 0,
                        }
                    } else {
                        State::Signature { matched }
                    };
                }
                State::Flags {
                    mut buf,
                    mut filled,
                } => {
                    if fill(&mut buf, &mut filled, chunk, &mut pos) {
                        let flags = u32::from_be_bytes(buf);
                        // 下位 16 bit は無視。OID あり・未知の critical flag は拒否。
                        if flags & (FLAG_OIDS | FLAGS_CRITICAL_MASK) != 0 {
                            return Err(CopyBinaryError::InvalidHeader.into());
                        }
                        self.state = State::ExtLen {
                            buf: [0; 4],
                            filled: 0,
                        };
                    } else {
                        self.state = State::Flags { buf, filled };
                    }
                }
                State::ExtLen {
                    mut buf,
                    mut filled,
                } => {
                    if fill(&mut buf, &mut filled, chunk, &mut pos) {
                        let len = i32::from_be_bytes(buf);
                        let len = u32::try_from(len).map_err(|_| CopyBinaryError::InvalidHeader)?;
                        self.state = Self::tuple_start_or_skip(len);
                    } else {
                        self.state = State::ExtLen { buf, filled };
                    }
                }
                State::ExtSkip { remaining } => {
                    let avail = chunk.len().saturating_sub(pos);
                    let n = u32::try_from(avail).unwrap_or(u32::MAX).min(remaining);
                    pos = pos.saturating_add(usize::try_from(n).unwrap_or(avail));
                    self.state = Self::tuple_start_or_skip(remaining.saturating_sub(n));
                }
                State::TupleStart {
                    mut buf,
                    mut filled,
                } => {
                    if fill(&mut buf, &mut filled, chunk, &mut pos) {
                        let count = i16::from_be_bytes(buf);
                        if count == -1 {
                            self.state = State::AfterTrailer;
                        } else if usize::try_from(count) == Ok(self.expected_fields) {
                            self.current = Vec::new();
                            self.tuple_bytes = 0;
                            self.state = State::FieldLen {
                                buf: [0; 4],
                                filled: 0,
                            };
                        } else {
                            return Err(CopyBinaryError::FieldCountMismatch.into());
                        }
                    } else {
                        self.state = State::TupleStart { buf, filled };
                    }
                }
                State::FieldLen {
                    mut buf,
                    mut filled,
                } => {
                    if fill(&mut buf, &mut filled, chunk, &mut pos) {
                        let len = i32::from_be_bytes(buf);
                        self.begin_field(len, on_tuple)?;
                    } else {
                        self.state = State::FieldLen { buf, filled };
                    }
                }
                State::FieldBody { remaining } => {
                    let rest = chunk.get(pos..).unwrap_or(&[]);
                    let n = rest.len().min(remaining);
                    if let Some(src) = rest.get(..n) {
                        self.body.extend_from_slice(src);
                    }
                    pos = pos.saturating_add(n);
                    let remaining = remaining.saturating_sub(n);
                    if remaining == 0 {
                        self.end_field(on_tuple)?;
                    } else {
                        self.state = State::FieldBody { remaining };
                    }
                }
            }
        }
        Ok(())
    }

    fn tuple_start_or_skip(remaining: u32) -> State {
        if remaining == 0 {
            State::TupleStart {
                buf: [0; 2],
                filled: 0,
            }
        } else {
            State::ExtSkip { remaining }
        }
    }

    /// フィールド長を検証（確保前）し、本文読み込みまたは NULL 確定へ進む。
    fn begin_field<E: From<CopyBinaryError>>(
        &mut self,
        len: i32,
        on_tuple: &mut impl FnMut(Vec<Option<Vec<u8>>>) -> Result<(), E>,
    ) -> Result<(), E> {
        if len == -1 {
            self.current.push(None);
            return self.after_field(on_tuple);
        }
        let len = usize::try_from(len).map_err(|_| CopyBinaryError::InvalidFieldLength)?;
        if len > self.limits.max_field_bytes {
            return Err(CopyBinaryError::LimitExceeded.into());
        }
        self.tuple_bytes = match self.tuple_bytes.checked_add(len) {
            Some(t) if t <= self.limits.max_tuple_bytes => t,
            _ => return Err(CopyBinaryError::LimitExceeded.into()),
        };
        self.body = Vec::new();
        if len == 0 {
            return self.end_field(on_tuple);
        }
        self.state = State::FieldBody { remaining: len };
        Ok(())
    }

    fn end_field<E: From<CopyBinaryError>>(
        &mut self,
        on_tuple: &mut impl FnMut(Vec<Option<Vec<u8>>>) -> Result<(), E>,
    ) -> Result<(), E> {
        let body = std::mem::take(&mut self.body);
        self.current.push(Some(body));
        self.after_field(on_tuple)
    }

    fn after_field<E: From<CopyBinaryError>>(
        &mut self,
        on_tuple: &mut impl FnMut(Vec<Option<Vec<u8>>>) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.current.len() < self.expected_fields {
            self.state = State::FieldLen {
                buf: [0; 4],
                filled: 0,
            };
            return Ok(());
        }
        self.state = State::TupleStart {
            buf: [0; 2],
            filled: 0,
        };
        on_tuple(std::mem::take(&mut self.current))
    }

    /// CopyDone 受信時に呼ぶ。トレーラ済み、またはタプル境界での終端のみ成功。
    /// 1 バイトの端数・長さ／本文／ヘッダの途中は fail-closed で拒否する。
    pub(crate) fn finish(self) -> Result<(), CopyBinaryError> {
        match self.state {
            State::AfterTrailer | State::TupleStart { filled: 0, .. } => Ok(()),
            _ => Err(CopyBinaryError::UnexpectedEof),
        }
    }
}

/// ヘッダ（署名 + flags 0 + 拡張長 0 の 19 バイト）を追記する。
pub(crate) fn encode_header_into(out: &mut Vec<u8>) {
    out.extend_from_slice(SIGNATURE);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
}

/// 1 タプルを追記する（`None` は NULL）。失敗時は `out` を呼び出し前の長さへ戻す。
pub(crate) fn encode_tuple_into(
    fields: &[Option<&[u8]>],
    out: &mut Vec<u8>,
) -> Result<(), CopyBinaryError> {
    let start = out.len();
    let result = encode_tuple_inner(fields, out);
    if result.is_err() {
        out.truncate(start);
    }
    result
}

fn encode_tuple_inner(fields: &[Option<&[u8]>], out: &mut Vec<u8>) -> Result<(), CopyBinaryError> {
    let count = i16::try_from(fields.len()).map_err(|_| CopyBinaryError::EncodeOverflow)?;
    out.extend_from_slice(&count.to_be_bytes());
    for field in fields {
        match field {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                let len =
                    i32::try_from(bytes.len()).map_err(|_| CopyBinaryError::EncodeOverflow)?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    Ok(())
}

/// トレーラ（`-1` の i16）を追記する。
pub(crate) fn encode_trailer_into(out: &mut Vec<u8>) {
    out.extend_from_slice(&(-1i16).to_be_bytes());
}

#[cfg(test)]
mod tests {
    //! WIRE-23 (1)(2) のフレーム境界テスト（結合テストは #1769 以降）。
    use super::*;

    type Tuple = Vec<Option<Vec<u8>>>;

    fn limits() -> BinaryFrameLimits {
        BinaryFrameLimits {
            max_field_bytes: 64,
            max_tuple_bytes: 128,
        }
    }

    fn stream(tuples: &[Vec<Option<&[u8]>>], trailer: bool) -> Vec<u8> {
        let mut out = Vec::new();
        encode_header_into(&mut out);
        for t in tuples {
            encode_tuple_into(t, &mut out).unwrap();
        }
        if trailer {
            encode_trailer_into(&mut out);
        }
        out
    }

    /// 分割して投入し、全タプルと finish 結果を返す。
    fn run(
        cols: usize,
        lim: BinaryFrameLimits,
        chunks: &[&[u8]],
    ) -> Result<Vec<Tuple>, CopyBinaryError> {
        let mut r = BinaryCopyReader::new(cols, lim).unwrap();
        let mut got = Vec::new();
        for c in chunks {
            r.feed::<CopyBinaryError>(c, |t| {
                got.push(t);
                Ok(())
            })?;
        }
        r.finish()?;
        Ok(got)
    }

    fn sample() -> (Vec<u8>, Vec<Tuple>) {
        let a: &[u8] = b"abc";
        let e: &[u8] = b"";
        let tuples = vec![
            vec![Some(a), None],
            vec![Some(e), Some(a)],
            vec![None, None],
        ];
        let want = vec![
            vec![Some(a.to_vec()), None],
            vec![Some(vec![]), Some(a.to_vec())],
            vec![None, None],
        ];
        (stream(&tuples, true), want)
    }

    #[test]
    fn roundtrip_whole_bytewise_and_every_split() {
        let (s, want) = sample();
        assert_eq!(run(2, limits(), &[&s]).unwrap(), want);
        let bytes: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(2, limits(), &bytes).unwrap(), want);
        for i in 0..=s.len() {
            let (a, b) = s.split_at(i);
            assert_eq!(run(2, limits(), &[a, b]).unwrap(), want, "split {i}");
        }
    }

    #[test]
    fn header_extension_is_skipped_and_low_flags_accepted() {
        let mut s = Vec::new();
        s.extend_from_slice(SIGNATURE);
        s.extend_from_slice(&0x0000_ABCDu32.to_be_bytes());
        s.extend_from_slice(&5u32.to_be_bytes());
        s.extend_from_slice(&[9; 5]);
        encode_tuple_into(&[Some(b"x")], &mut s).unwrap();
        encode_trailer_into(&mut s);
        let want = vec![vec![Some(b"x".to_vec())]];
        assert_eq!(run(1, limits(), &[&s]).unwrap(), want);
        let bytes: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(1, limits(), &bytes).unwrap(), want);
    }

    #[test]
    fn zero_tuples_and_missing_trailer_at_tuple_boundary() {
        assert!(run(1, limits(), &[&stream(&[], true)]).unwrap().is_empty());
        let s = stream(&[vec![Some(b"a")]], false);
        assert_eq!(run(1, limits(), &[&s]).unwrap().len(), 1);
    }

    #[test]
    fn header_bytes_and_signature_mismatch() {
        let mut h = Vec::new();
        encode_header_into(&mut h);
        assert_eq!(h.len(), 19);
        assert_eq!(h.get(..11), Some(&b"PGCOPY\n\xff\r\n\0"[..]));
        assert!(h.iter().skip(11).all(|b| *b == 0));
        for i in 0..11 {
            let mut s = stream(&[], true);
            s[i] ^= 0x01;
            let e = run(1, limits(), &[&s]).unwrap_err();
            assert_eq!(e.sqlstate(), "22P04", "byte {i}");
        }
    }

    #[test]
    fn truncated_at_every_point_is_22p04_except_boundaries() {
        let s = stream(&[vec![Some(b"abc"), None]], true);
        for n in 0..s.len() {
            let r = run(2, limits(), &[&s[..n]]);
            // 完全なタプル境界（ヘッダ直後とタプル直後）のみ成功。
            let boundary = n == 19 || n == s.len() - 2;
            if boundary {
                assert!(r.is_ok(), "n={n}");
            } else {
                assert_eq!(r.unwrap_err().sqlstate(), "22P04", "n={n}");
            }
        }
    }

    fn header_with(flags: u32, ext: i32) -> Vec<u8> {
        let mut s = SIGNATURE.to_vec();
        s.extend_from_slice(&flags.to_be_bytes());
        s.extend_from_slice(&ext.to_be_bytes());
        s
    }

    #[test]
    fn bad_flags_and_extension_length() {
        let mut flags = vec![FLAG_OIDS];
        flags.extend((17..32).map(|b| 1u32 << b));
        for f in flags {
            let e = run(1, limits(), &[&header_with(f, 0)]).unwrap_err();
            assert_eq!(e, CopyBinaryError::InvalidHeader, "flags {f:#x}");
        }
        for ext in [-1, i32::MIN] {
            let e = run(1, limits(), &[&header_with(0, ext)]).unwrap_err();
            assert_eq!(e, CopyBinaryError::InvalidHeader);
        }
        let mut s = header_with(0, 4);
        s.extend_from_slice(&[0, 0]);
        assert_eq!(run(1, limits(), &[&s]).unwrap_err().sqlstate(), "22P04");
    }

    #[test]
    fn field_count_mismatch() {
        for cols in [1usize, 3] {
            let s = stream(&[vec![Some(b"a"), None]], true);
            let e = run(cols, limits(), &[&s]).unwrap_err();
            assert_eq!(e, CopyBinaryError::FieldCountMismatch);
        }
        let mut s = header_with(0, 0);
        s.extend_from_slice(&0i16.to_be_bytes());
        assert_eq!(
            run(1, limits(), &[&s]).unwrap_err(),
            CopyBinaryError::FieldCountMismatch
        );
    }

    #[test]
    fn invalid_and_oversized_field_length() {
        for len in [-2i32, i32::MIN] {
            let mut s = header_with(0, 0);
            s.extend_from_slice(&1i16.to_be_bytes());
            s.extend_from_slice(&len.to_be_bytes());
            assert_eq!(
                run(1, limits(), &[&s]).unwrap_err(),
                CopyBinaryError::InvalidFieldLength
            );
        }
        // 上限超過は本文が届く前に拒否され、確保も行われない。
        let mut s = header_with(0, 0);
        s.extend_from_slice(&1i16.to_be_bytes());
        s.extend_from_slice(&65i32.to_be_bytes());
        let mut r = BinaryCopyReader::new(1, limits()).unwrap();
        let e = r.feed::<CopyBinaryError>(&s, |_| Ok(())).unwrap_err();
        assert_eq!(e.sqlstate(), "54000");
        assert_eq!(r.body.capacity(), 0);
        let mut s2 = header_with(0, 0);
        s2.extend_from_slice(&1i16.to_be_bytes());
        s2.extend_from_slice(&i32::MAX.to_be_bytes());
        assert_eq!(
            run(1, limits(), &[&s2]).unwrap_err(),
            CopyBinaryError::LimitExceeded
        );
    }

    #[test]
    fn tuple_total_limit() {
        let lim = BinaryFrameLimits {
            max_field_bytes: 64,
            max_tuple_bytes: 10,
        };
        let a = [0u8; 6];
        let s = stream(&[vec![Some(&a[..]), Some(&a[..])]], true);
        assert_eq!(
            run(2, lim, &[&s]).unwrap_err(),
            CopyBinaryError::LimitExceeded
        );
        let b = [0u8; 5];
        let ok = stream(&[vec![Some(&b[..]), Some(&b[..])]], true);
        assert!(run(2, lim, &[&ok]).is_ok());
    }

    #[test]
    fn data_after_trailer_and_poisoning() {
        let s = stream(&[], true);
        let mut extra = s.clone();
        extra.push(0);
        assert_eq!(
            run(1, limits(), &[&extra]).unwrap_err(),
            CopyBinaryError::DataAfterTrailer
        );
        assert_eq!(
            run(1, limits(), &[&s, &[0]]).unwrap_err(),
            CopyBinaryError::DataAfterTrailer
        );

        let mut r = BinaryCopyReader::new(1, limits()).unwrap();
        assert!(r.feed::<CopyBinaryError>(b"XXXX", |_| Ok(())).is_err());
        assert!(r
            .feed::<CopyBinaryError>(&stream(&[], true), |_| Ok(()))
            .is_err());
        assert!(r.finish().is_err());
    }

    #[test]
    fn callback_error_propagates_and_poisons() {
        #[derive(Debug, PartialEq)]
        enum E {
            Cb,
            Frame,
        }
        impl From<CopyBinaryError> for E {
            fn from(_: CopyBinaryError) -> Self {
                E::Frame
            }
        }
        let s = stream(&[vec![Some(b"a")], vec![Some(b"b")]], true);
        let mut r = BinaryCopyReader::new(1, limits()).unwrap();
        assert_eq!(r.feed(&s, |_| Err(E::Cb)), Err(E::Cb));
        assert!(r.finish().is_err());
    }

    #[test]
    fn client_messages_are_fixed_strings() {
        assert_eq!(
            CopyBinaryError::InvalidHeader.client_message(),
            "invalid COPY file header"
        );
        assert_eq!(
            CopyBinaryError::UnexpectedEof.client_message(),
            "invalid COPY data"
        );
    }

    #[test]
    fn writer_overflow_restores_length() {
        let fields: Vec<Option<&[u8]>> = vec![None; i16::MAX as usize + 1];
        let mut out = vec![1, 2, 3];
        assert_eq!(
            encode_tuple_into(&fields, &mut out),
            Err(CopyBinaryError::EncodeOverflow)
        );
        assert_eq!(out, vec![1, 2, 3]);
        assert!(BinaryCopyReader::new(0, limits()).is_err());
        assert!(BinaryCopyReader::new(i16::MAX as usize + 1, limits()).is_err());
    }

    #[test]
    fn random_input_never_panics() {
        let mut x: u64 = 0x1234_5678_9abc_def1;
        let mut next = move || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as usize
        };
        let (valid, _) = sample();
        for round in 0..300 {
            let mut data = if round % 2 == 0 {
                valid.clone()
            } else {
                Vec::new()
            };
            let extra = next() % 80;
            for _ in 0..extra {
                data.push(next() as u8);
            }
            if round % 3 == 0 && !data.is_empty() {
                let i = next() % data.len();
                data[i] = next() as u8;
            }
            let mut r = BinaryCopyReader::new(2, limits()).unwrap();
            let mut rest: &[u8] = &data;
            while !rest.is_empty() {
                let n = (next() % 7 + 1).min(rest.len());
                let (a, b) = rest.split_at(n);
                let _ = r.feed::<CopyBinaryError>(a, |_| Ok(()));
                rest = b;
            }
            let _ = r.finish();
        }
    }
}
