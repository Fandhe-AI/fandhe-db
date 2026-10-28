//! TASK-119（対象ビヘイビア: INDEX-3）のチャンク化モジュール公開 API に対する
//! 結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-119・
//! `docs/spec/04-behavior/indexing.md` INDEX-3。
//!
//! `engine::chunking` の公開 API のみを経由して検証する（内部ヘルパの単体テストは
//! `crates/engine/src/chunking.rs` 側に置く）。

use engine::chunking::{
    chunk_file, chunk_generic, chunk_markdown, detect_file_kind, Chunk, ChunkingConfig,
    ChunkingError, FileKind, MAX_INPUT_BYTES, MAX_INPUT_LINES,
};

fn lines_of(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 各チャンクの本文行数が `end_line - start_line + 1` に一致することを検証する
/// 共通アサーション（全経路で「チャンク本文は元テキストの連続する行スライス」
/// であるという不変条件。TASK-120 の増分インデックス結線が行スパンを使う前提）。
fn assert_line_span_matches_text(chunks: &[Chunk]) {
    for chunk in chunks {
        let span = chunk
            .end_line
            .checked_sub(chunk.start_line)
            .and_then(|d| d.checked_add(1))
            .expect("end_line >= start_line");
        assert_eq!(
            chunk.text.lines().count(),
            span,
            "chunk {} text lines must match its line span ({}..={})",
            chunk.index,
            chunk.start_line,
            chunk.end_line
        );
    }
}

#[test]
fn mixed_files_are_chunked_by_kind() {
    let config = ChunkingConfig::default();

    let markdown = "# heading one\nbody a\n\n## heading two\nbody b\n";
    let md_chunks = chunk_file("notes/readme.md", markdown, &config).expect("markdown chunks");
    assert_eq!(md_chunks.len(), 2);

    let generic_120 = lines_of(120);
    let rs_chunks = chunk_file("src/main.rs", &generic_120, &config).expect("generic chunks");
    assert_eq!(rs_chunks.len(), 120usize.div_ceil(60));

    let toml_130 = lines_of(130);
    let toml_chunks = chunk_file("Cargo.toml", &toml_130, &config).expect("toml chunks");
    assert_eq!(toml_chunks.len(), 130usize.div_ceil(60));

    let no_ext = lines_of(1);
    let no_ext_chunks = chunk_file("Makefile", &no_ext, &config).expect("no-ext chunks");
    assert_eq!(no_ext_chunks.len(), 1);
}

#[test]
fn generic_60_line_windows() {
    let config = ChunkingConfig::default();

    let chunks_150 = chunk_generic(&lines_of(150), &config).expect("150 lines");
    assert_eq!(chunks_150.len(), 3);
    assert_eq!((chunks_150[0].start_line, chunks_150[0].end_line), (1, 60));
    assert_eq!(
        (chunks_150[1].start_line, chunks_150[1].end_line),
        (61, 120)
    );
    assert_eq!(
        (chunks_150[2].start_line, chunks_150[2].end_line),
        (121, 150)
    );

    let chunks_60 = chunk_generic(&lines_of(60), &config).expect("60 lines");
    assert_eq!(chunks_60.len(), 1);

    let chunks_61 = chunk_generic(&lines_of(61), &config).expect("61 lines");
    assert_eq!(chunks_61.len(), 2);

    let chunks_empty = chunk_generic("", &config).expect("empty input");
    assert_eq!(chunks_empty.len(), 0);

    let chunks_blank = chunk_generic("   \n\n   \n", &config).expect("blank-only input");
    assert_eq!(chunks_blank.len(), 0);

    assert_line_span_matches_text(&chunks_150);
}

#[test]
fn markdown_heading_boundaries() {
    let config = ChunkingConfig::default();
    let text = "preamble text\n\n# h1\nbody one\n\n## h2\nbody two\n\n### h3\nbody three\n\n#### h4 not a boundary\nstill inside h3 section\n";
    let chunks = chunk_markdown(text, &config).expect("markdown chunks");

    // 前文 + h1 + h2 + h3（#### は境界にならないため h3 節へ吸収される）= 4 節。
    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[0].start_line, 1);
    assert!(chunks[1].text.starts_with("# h1"));
    assert!(chunks[2].text.starts_with("## h2"));
    assert!(chunks[3].text.starts_with("### h3"));
    assert!(chunks[3].text.contains("#### h4 not a boundary"));
    assert_line_span_matches_text(&chunks);
}

#[test]
fn markdown_fence_close_requires_commonmark_conditions() {
    // 閉じフェンスは「開始と同じ文字種・開始以上の長さ・後続が空白のみ」を
    // 満たす行に限る（Review 指摘）。満たさない行で閉じたと誤認すると、
    // コードブロック内の `#` 行が見出し境界になりチャンクが誤分割される。
    let config = ChunkingConfig::default();

    // 短い ``` は ```` で開いたフェンスを閉じない。
    let chunks = chunk_markdown("# a\n````\n```\n# b\n", &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 1);

    // info string 付きの行は閉じフェンスにならない。
    let chunks = chunk_markdown("# a\n```\n```text\n# b\n", &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 1);

    // 逆に条件を満たす行（開始以上の長さ・後続空白のみ）は閉じる。
    let chunks = chunk_markdown("# a\n```\n````  \n# b\n", &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 2);
}

#[test]
fn markdown_fence_open_rejects_backtick_info_string() {
    // backtick 開始フェンスの info string にバッククォートは置けないため、
    // 該当行は fence 開始にならず後続の見出しは通常どおり境界になる。
    let config = ChunkingConfig::default();
    let chunks = chunk_markdown("# a\n```a`b\n# b\n", &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 2);

    // tilde 開始フェンスにはこの制約がない。
    let chunks = chunk_markdown("# a\n~~~a`b\n# b\n", &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 1);
}

#[test]
fn markdown_indented_fence_is_not_a_fence() {
    // 4 スペース以上インデントされた ``` 行はインデントコードブロックの内容で
    // あり fence 開始ではない（is_atx_heading と同じ CommonMark のインデント
    // 基準に揃える。Review 指摘）。誤って fence 開始と見なすと以降の見出しが
    // すべて 1 節へ溶ける。
    let config = ChunkingConfig::default();
    let chunks = chunk_markdown("# a\n    ```\n# b\n", &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].text.starts_with("# a"));
    assert!(chunks[1].text.starts_with("# b"));
}

#[test]
fn markdown_fenced_code_not_heading() {
    let config = ChunkingConfig::default();
    let text = "# real heading\nintro\n\n```text\n# not a heading\nstill code\n```\n\nmore body\n";
    let chunks = chunk_markdown(text, &config).expect("markdown chunks");

    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].text.contains("# not a heading"));
}

#[test]
fn markdown_fence_requires_matching_delimiter_kind() {
    // 開始が ``` の fence の内側に ~~~ 始まりの行が現れても、閉じ扱いに
    // せず fence 継続として扱う（CommonMark: 閉じフェンスは開始と同じ文字種）。
    let config = ChunkingConfig::default();
    let text = "# heading\n```text\n~~~\n# not a heading\n```\n\nafter\n";
    let chunks = chunk_markdown(text, &config).expect("markdown chunks");

    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].text.contains("# not a heading"));
    assert!(chunks[0].text.contains("after"));
}

#[test]
fn markdown_oversized_section_split_by_paragraph() {
    let long_paragraph_a = "a".repeat(400);
    let long_paragraph_b = "b".repeat(400);
    let text = format!("# heading\n{long_paragraph_a}\n\n{long_paragraph_b}\n");

    let split_config = ChunkingConfig {
        max_markdown_section_chars: Some(600),
        ..ChunkingConfig::default()
    };
    let split_chunks = chunk_markdown(&text, &split_config).expect("split chunks");
    assert!(
        split_chunks.len() > 1,
        "oversized section should split at paragraph boundary"
    );

    let unbounded_config = ChunkingConfig {
        max_markdown_section_chars: None,
        ..ChunkingConfig::default()
    };
    assert_line_span_matches_text(&split_chunks);

    let unbounded_chunks = chunk_markdown(&text, &unbounded_config).expect("unbounded chunks");
    assert_eq!(unbounded_chunks.len(), 1);
    assert_line_span_matches_text(&unbounded_chunks);
}

#[test]
fn markdown_oversized_section_preserves_paragraph_blank_lines() {
    // 同一チャンクへ合流した複数段落の間の空行（段落境界）が本文から
    // 落ちないこと。非オーバーサイズ経路（trim_block 直呼び）と整形が
    // 一致していることの回帰保護（Review 指摘）。
    let config = ChunkingConfig {
        max_markdown_section_chars: Some(600),
        ..ChunkingConfig::default()
    };
    let a = "a".repeat(200);
    let b = "b".repeat(200);
    let c = "c".repeat(400);
    let text = format!("# h\n{a}\n\n{b}\n\n{c}\n");

    let chunks = chunk_markdown(&text, &config).expect("markdown chunks");
    assert_eq!(chunks.len(), 2);

    assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 4));
    assert_eq!(chunks[0].text, format!("# h\n{a}\n\n{b}"));
    assert!(chunks[0].text.contains("\n\n"));
    assert_eq!((chunks[1].start_line, chunks[1].end_line), (6, 6));
    assert_eq!(chunks[1].text, c);
    assert_line_span_matches_text(&chunks);
}

#[test]
fn crlf_input_is_normalized() {
    let config = ChunkingConfig::default();
    let crlf = "# heading\r\nbody one\r\nbody two\r\n";
    let lf = "# heading\nbody one\nbody two\n";

    let crlf_chunks = chunk_markdown(crlf, &config).expect("crlf chunks");
    let lf_chunks = chunk_markdown(lf, &config).expect("lf chunks");

    assert_eq!(crlf_chunks.len(), lf_chunks.len());
    for chunk in &crlf_chunks {
        assert!(!chunk.text.contains('\r'));
    }
    assert_eq!(crlf_chunks[0].text, lf_chunks[0].text);
}

#[test]
fn file_kind_detection() {
    assert_eq!(detect_file_kind("a/b/readme.md"), FileKind::Markdown);
    assert_eq!(detect_file_kind("readme.MD"), FileKind::Markdown);
    assert_eq!(detect_file_kind("notes.markdown"), FileKind::Markdown);
    assert_eq!(detect_file_kind("main.rs"), FileKind::Generic);
    assert_eq!(detect_file_kind("data.json"), FileKind::Generic);
    assert_eq!(detect_file_kind("Makefile"), FileKind::Generic);
    assert_eq!(detect_file_kind("trailing."), FileKind::Generic);

    // 拡張子判定は最終パス要素だけを見る（ディレクトリ名のドットを拾わない）。
    assert_eq!(detect_file_kind("docs.md/README"), FileKind::Generic);
    assert_eq!(
        detect_file_kind("archive.markdown/file.txt"),
        FileKind::Generic
    );
    assert_eq!(detect_file_kind("docs.md/"), FileKind::Generic);
    assert_eq!(detect_file_kind("v1.md/notes.md"), FileKind::Markdown);
    assert_eq!(detect_file_kind("dir\\notes.md"), FileKind::Markdown);
    assert_eq!(detect_file_kind("dir.md\\README"), FileKind::Generic);
    // ドットファイル（拡張子ではなく名前の一部）は Generic 扱い。
    assert_eq!(detect_file_kind(".md"), FileKind::Generic);
    assert_eq!(detect_file_kind(""), FileKind::Generic);
}

#[test]
fn limits_fail_closed() {
    let config = ChunkingConfig::default();

    let oversized = "a".repeat(MAX_INPUT_BYTES + 1);
    let result = chunk_generic(&oversized, &config);
    assert!(matches!(result, Err(ChunkingError::InputTooLarge { .. })));

    let zero_lines_per_chunk = ChunkingConfig {
        lines_per_chunk: 0,
        ..ChunkingConfig::default()
    };
    let result = chunk_generic("some text", &zero_lines_per_chunk);
    assert!(matches!(result, Err(ChunkingError::InvalidConfig { .. })));

    // 行数上限は単独で発火すること（バイト長上限に隠れないことの回帰保護）。
    let too_many_lines = "\n".repeat(MAX_INPUT_LINES + 1);
    assert!(too_many_lines.len() <= MAX_INPUT_BYTES);
    let result = chunk_generic(&too_many_lines, &config);
    assert!(matches!(result, Err(ChunkingError::TooManyLines { .. })));

    // チャンク総文字数が入力を超えないこと（有界化の健全性）。
    let text = lines_of(90);
    let chunks = chunk_generic(&text, &config).expect("90 lines");
    let total_chars: usize = chunks.iter().map(|c| c.text.chars().count()).sum();
    assert!(total_chars <= text.chars().count());
}

#[test]
fn chunk_indices_are_dense_and_ordered() {
    let config = ChunkingConfig::default();
    let chunks = chunk_generic(&lines_of(200), &config).expect("200 lines");

    let mut prev_end = 0usize;
    for (expected_index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.index, expected_index);
        assert!(chunk.start_line <= chunk.end_line);
        assert!(chunk.start_line > prev_end);
        prev_end = chunk.end_line;
    }
}
