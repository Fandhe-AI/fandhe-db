#!/usr/bin/env python3
"""拒否固定テストの注記検査の中核（Issue #3164。check_rejection_pins.sh から呼ばれる）。

ADR docs/design/rejection-pin-annotation.md の D2〜D9 を実装する。
`git diff <base>...HEAD` の差分から、変更に触れた `#[test]` 関数のうち拒否（0A000・
not supported・unsupported）を固定するものを選び、`tracked:` / `intentional:` 注記の
欠落・書式不正・根拠ファイルの不在を違反として報告する。

- 判定本体（parse_diff / find_tests / select_targets / check）は差分テキストと読み取り関数を
  受け取る純関数で、単体テスト（scripts/tests/test_check_rejection_pins.py）が直接呼ぶ
- git を叩くのは main() だけ（配列引数・shell=False。オフライン）
- fail-closed: 差分の解析不能・テスト範囲の終端なし・git の失敗はすべて終了コード 1
- 終了コード: 0 = 違反なし / 1 = 違反あり・検査不能 / 2 = 引数の誤り
- 出力文字列は英語（運用ツール連携のため）
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Set, Tuple

# ---------------------------------------------------------------------------
# 定数・正規表現（ASCII 意味論に固定し、全角空白などを \s が受けないようにする）
# ---------------------------------------------------------------------------

_A = re.ASCII

# D4: 対象パス
_TARGET_RE = re.compile(r"^crates/(?:engine|wire-server)/tests/.+\.rs$|^crates/[^/]+/src/.+\.rs$", _A)
PATHSPECS = [
    ":(glob)crates/*/src/**/*.rs",
    ":(glob)crates/engine/tests/**/*.rs",
    ":(glob)crates/wire-server/tests/**/*.rs",
]

# D2: 注記らしい行（緩い判定）と厳密な文法
_LOOSE_RE = re.compile(r"^\s*///?\s*(tracked|intentional)", _A | re.IGNORECASE)
_TRACKED_RE = re.compile(r"^\s*///?\s*tracked:\s+(#[1-9][0-9]*(?:, #[1-9][0-9]*)*)\s*$", _A)
_INTENTIONAL_RE = re.compile(
    r"^\s*///?\s*intentional:\s+"
    r"(docs/design/[A-Za-z0-9._-]+\.md(?:, docs/design/[A-Za-z0-9._-]+\.md)*)\s*$",
    _A,
)

# D6: 拒否の語彙
_VOCAB_RE = re.compile(r"0a000|not[ _]?supported|unsupported", _A | re.IGNORECASE)

# D9: 根拠にできない設計記録
_DISALLOWED_DOCS = {"docs/design/implementation-status.md"}

_FN_RE = re.compile(
    r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r'(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)',
    _A,
)
_TEST_ATTR_RE = re.compile(r"^\s*#\[test\]\s*(?://.*)?$", _A)
_ONE_LINE_FN_RE = re.compile(r"\{\s*\}\s*$", _A)

_DIFF_HEADER_RE = re.compile(r"^diff --git a/(.+) b/(.+)$")
_HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
_SHA_RE = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")


class DiffParseError(Exception):
    """差分を解釈できない（fail-closed で検査失敗にする）。"""


@dataclass
class FileDiff:
    """1 ファイル分の差分。行番号は新側（added）と旧側（deleted）。"""

    old_path: Optional[str] = None
    new_path: Optional[str] = None
    added: Set[int] = field(default_factory=set)
    deleted: Set[int] = field(default_factory=set)


@dataclass
class TestRange:
    """`#[test]` 関数 1 件の範囲（D3・D4）。end が None なら終端が見つからない。"""

    __test__ = False  # unittest/pytest に収集させない

    name: str
    fn_line: int
    start: int
    end: Optional[int]
    block: List[Tuple[int, str]]
    lines: List[Tuple[int, str]]


@dataclass
class Violation:
    path: str
    line: int
    test: Optional[str]
    kind: str
    message: str

    def render(self) -> str:
        who = f"test '{self.test}': " if self.test else ""
        return f"{self.path}:{self.line}: {who}{self.message} [{self.kind}]"


def is_target_path(path: Optional[str]) -> bool:
    return bool(path) and bool(_TARGET_RE.match(path or ""))


# ---------------------------------------------------------------------------
# 差分の解析
# ---------------------------------------------------------------------------


def _strip_prefix(p: str, prefix: str) -> str:
    p = p.split("\t", 1)[0]
    return p[len(prefix):] if p.startswith(prefix) else p


def parse_diff(text: str) -> List[FileDiff]:
    """unified diff（-U0 を想定。context 行付きも可）を FileDiff の列へ解釈する。"""
    files: List[FileDiff] = []
    cur: Optional[FileDiff] = None
    old_rem = new_rem = 0
    old_ln = new_ln = 0
    for raw in text.split("\n"):
        if old_rem > 0 or new_rem > 0:
            # hunk 内は先頭 1 文字で種別を決める（本文が `+++` や `---` で始まっても本文）
            tag = raw[:1]
            if cur is None:
                raise DiffParseError("hunk without file header")
            if tag == "+":
                cur.added.add(new_ln)
                new_ln += 1
                new_rem -= 1
            elif tag == "-":
                cur.deleted.add(old_ln)
                old_ln += 1
                old_rem -= 1
            elif tag == " " or raw == "":
                old_ln += 1
                new_ln += 1
                old_rem -= 1
                new_rem -= 1
            elif tag == "\\":
                pass
            else:
                raise DiffParseError(f"unexpected line inside hunk: {raw[:40]!r}")
            continue
        if raw.startswith("diff --git "):
            if raw.startswith('diff --git "') or raw.startswith('diff --git a/"'):
                raise DiffParseError("quoted path in diff header")
            m = _DIFF_HEADER_RE.match(raw)
            if not m:
                raise DiffParseError(f"unparsable diff header: {raw[:80]!r}")
            cur = FileDiff(old_path=m.group(1), new_path=m.group(2))
            files.append(cur)
        elif cur is None:
            continue
        elif raw.startswith("rename from "):
            cur.old_path = raw[len("rename from "):]
        elif raw.startswith("rename to "):
            cur.new_path = raw[len("rename to "):]
        elif raw.startswith("new file mode"):
            cur.old_path = None
        elif raw.startswith("deleted file mode"):
            cur.new_path = None
        elif raw.startswith("--- "):
            rest = raw[4:]
            if rest.startswith('"'):
                raise DiffParseError("quoted path in diff")
            cur.old_path = None if rest.startswith("/dev/null") else _strip_prefix(rest, "a/")
        elif raw.startswith("+++ "):
            rest = raw[4:]
            if rest.startswith('"'):
                raise DiffParseError("quoted path in diff")
            cur.new_path = None if rest.startswith("/dev/null") else _strip_prefix(rest, "b/")
        elif raw.startswith("@@"):
            m = _HUNK_RE.match(raw)
            if not m:
                raise DiffParseError(f"unparsable hunk header: {raw[:80]!r}")
            old_ln = int(m.group(1))
            old_rem = int(m.group(2)) if m.group(2) is not None else 1
            new_ln = int(m.group(3))
            new_rem = int(m.group(4)) if m.group(4) is not None else 1
        elif raw.startswith("Binary files") or raw.startswith("GIT binary patch"):
            raise DiffParseError("binary diff for a checked path")
    return files


# ---------------------------------------------------------------------------
# テスト範囲の抽出（D3・D4）
# ---------------------------------------------------------------------------


def _scan_attribute(lines: List[str], start: int) -> int:
    """`#[` で始まる属性の終端行（0 始まり）を返す。

    文字列・raw 文字列・文字リテラル・行/ブロックコメント内の角括弧は深さに数えない。
    終端を解釈できない場合は DiffParseError（fail-closed で検査失敗）。
    """
    depth = 0
    seen_open = False
    in_str = False
    raw_hashes: Optional[int] = None  # raw 文字列内のとき `#` の個数
    block_cmt = 0
    i = start
    n = len(lines)
    while i < n:
        s = lines[i]
        j = 0
        m = len(s)
        while j < m:
            c = s[j]
            if block_cmt:
                if s.startswith("*/", j):
                    block_cmt -= 1
                    j += 2
                elif s.startswith("/*", j):
                    block_cmt += 1
                    j += 2
                else:
                    j += 1
                continue
            if raw_hashes is not None:
                if c == '"' and s.startswith("#" * raw_hashes, j + 1):
                    j += 1 + raw_hashes
                    raw_hashes = None
                else:
                    j += 1
                continue
            if in_str:
                if c == "\\":
                    j += 2
                elif c == '"':
                    in_str = False
                    j += 1
                else:
                    j += 1
                continue
            if s.startswith("//", j):
                break
            if s.startswith("/*", j):
                block_cmt += 1
                j += 2
                continue
            rm = re.compile(r'b?r(#*)"', _A).match(s, j)
            if rm and (j == 0 or not (s[j - 1].isalnum() or s[j - 1] == "_")):
                raw_hashes = len(rm.group(1))
                j = rm.end()
                continue
            if c == '"':
                in_str = True
                j += 1
                continue
            if c == "'":
                cm = re.compile(r"'(?:\\.[^']*|[^'\\])'", _A).match(s, j)
                j = cm.end() if cm else j + 1  # 一致しなければライフタイム等
                continue
            if c == "[":
                depth += 1
                seen_open = True
            elif c == "]":
                depth -= 1
                if seen_open and depth <= 0:
                    return i
            j += 1
        i += 1
    raise DiffParseError("cannot find the end of an attribute (unterminated string/bracket)")


def find_tests(text: str) -> List[TestRange]:
    """ファイル内容から `#[test]` 付き fn の範囲を抽出する。

    連続ブロック（コメント行と属性行のみ・空行なし）を `fn` 行の直前で拾い、`#[test]` を
    含めばテストとして登録する。終端は「fn 行と同じインデントの `}` だけの行」で決める
    （波括弧の数え上げはしない）。
    """
    lines = text.split("\n")
    tests: List[TestRange] = []
    block: List[Tuple[int, str]] = []
    has_test = False
    i = 0
    n = len(lines)
    while i < n:
        ln = i + 1
        line = lines[i]
        stripped = line.strip()
        if stripped.startswith("//"):
            block.append((ln, line))
            i += 1
            continue
        if stripped.startswith("#[") or stripped.startswith("#!["):
            end_i = _scan_attribute(lines, i)
            for k in range(i, end_i + 1):
                block.append((k + 1, lines[k]))
                if _TEST_ATTR_RE.match(lines[k]):
                    has_test = True
            i = end_i + 1
            continue
        m = _FN_RE.match(line)
        if m and has_test:
            indent, name = m.group(1), m.group(2)
            end: Optional[int] = None
            if _ONE_LINE_FN_RE.search(line):
                end = ln
            else:
                closer = indent + "}"
                for j in range(i + 1, n):
                    if lines[j].rstrip() == closer:
                        end = j + 1
                        break
            start = block[0][0] if block else ln
            last = end if end is not None else n
            tests.append(
                TestRange(
                    name=name,
                    fn_line=ln,
                    start=start,
                    end=end,
                    block=list(block),
                    lines=[(k + 1, lines[k]) for k in range(start - 1, last)],
                )
            )
        block = []
        has_test = False
        i += 1
    return tests


# ---------------------------------------------------------------------------
# 対象テストの選択（D5）
# ---------------------------------------------------------------------------

ReadFn = Callable[[str], Optional[str]]


def select_targets(
    file_diffs: List[FileDiff], read_head: ReadFn, read_base: ReadFn
) -> List[Tuple[str, TestRange]]:
    """追加・変更行、または削除行（base 側の範囲）に触れたテストを HEAD 側で選ぶ。"""
    out: Dict[Tuple[str, int], Tuple[str, TestRange]] = {}
    for fd in file_diffs:
        if fd.new_path is None or not is_target_path(fd.new_path):
            continue
        head_text = read_head(fd.new_path)
        if head_text is None:
            continue
        head_tests = find_tests(head_text)
        for t in head_tests:
            last = t.end if t.end is not None else t.fn_line
            if any(t.start <= a <= last for a in fd.added):
                out[(fd.new_path, t.fn_line)] = (fd.new_path, t)
        if fd.deleted and fd.old_path is not None:
            base_text = read_base(fd.old_path)
            if base_text is not None:
                names = set()
                for bt in find_tests(base_text):
                    last = bt.end if bt.end is not None else bt.fn_line
                    if any(bt.start <= d <= last for d in fd.deleted):
                        names.add(bt.name)
                for t in head_tests:
                    if t.name in names:
                        out[(fd.new_path, t.fn_line)] = (fd.new_path, t)
    return sorted(out.values(), key=lambda x: (x[0], x[1].fn_line))


# ---------------------------------------------------------------------------
# 判定（D2・D6・D7・D9）
# ---------------------------------------------------------------------------


def is_rejection_pin(t: TestRange) -> bool:
    """D6: 行頭 `//` のコメント行を除いた範囲に語彙が現れれば拒否固定。"""
    for _, line in t.lines:
        if line.lstrip().startswith("//"):
            continue
        if _VOCAB_RE.search(line):
            return True
    return False


def _check_annotation_line(
    path: str,
    ln: int,
    line: str,
    test: Optional[str],
    head_exists: Callable[[str], bool],
) -> Tuple[List[Violation], Optional[str]]:
    """注記らしい 1 行を検査する。戻り値は (違反, 有効な種類 'tracked'|'intentional'|None)。"""
    vs: List[Violation] = []
    if _TRACKED_RE.match(line):
        return vs, "tracked"
    m = _INTENTIONAL_RE.match(line)
    if m:
        for doc in m.group(1).split(", "):
            if doc in _DISALLOWED_DOCS:
                vs.append(Violation(path, ln, test, "disallowed-design-doc",
                                    f"{doc} cannot be the basis of an intentional: annotation"))
            elif not head_exists(doc):
                vs.append(Violation(path, ln, test, "missing-design-doc",
                                    f"intentional: annotation points to a missing file {doc}"))
        return vs, "intentional"
    vs.append(Violation(path, ln, test, "malformed-annotation",
                        "malformed tracked:/intentional: annotation"))
    return vs, None


def check(
    file_diffs: List[FileDiff],
    read_head: ReadFn,
    read_base: ReadFn,
    head_exists: Callable[[str], bool],
) -> Tuple[List[Violation], int, int]:
    """違反の一覧と (対象テスト数, 拒否固定テスト数) を返す。"""
    viol: Dict[Tuple[str, int, str], Violation] = {}

    def add(v: Violation) -> None:
        viol.setdefault((v.path, v.line, v.kind), v)

    targets = select_targets(file_diffs, read_head, read_base)
    pins = 0
    for path, t in targets:
        if t.end is None:
            add(Violation(path, t.fn_line, t.name, "unterminated-test",
                          "cannot find the closing brace of the test (rustfmt layout assumed)"))
            continue
        kinds: List[str] = []
        for ln, line in t.block:
            if _LOOSE_RE.match(line):
                vs, kind = _check_annotation_line(path, ln, line, t.name, head_exists)
                for v in vs:
                    add(v)
                if kind:
                    kinds.append(kind)
        for k in ("tracked", "intentional"):
            if kinds.count(k) > 1:
                add(Violation(path, t.fn_line, t.name, "duplicate-annotation",
                              f"multiple {k}: annotation lines (merge them into one line)"))
        if is_rejection_pin(t):
            pins += 1
            if not kinds:
                add(Violation(path, t.fn_line, t.name, "missing-annotation",
                              "rejection-pinning test has no tracked:/intentional: annotation"))

    # D7: 検出の有無にかかわらず、追加された注記らしい行すべてに書式・値の検査をかける
    for fd in file_diffs:
        if fd.new_path is None or not is_target_path(fd.new_path) or not fd.added:
            continue
        head_text = read_head(fd.new_path)
        if head_text is None:
            continue
        head_lines = head_text.split("\n")
        for ln in sorted(fd.added):
            if 1 <= ln <= len(head_lines) and _LOOSE_RE.match(head_lines[ln - 1]):
                vs, _ = _check_annotation_line(fd.new_path, ln, head_lines[ln - 1], None, head_exists)
                for v in vs:
                    add(v)
    return sorted(viol.values(), key=lambda v: (v.path, v.line, v.kind)), len(targets), pins


# ---------------------------------------------------------------------------
# git 呼び出しと main
# ---------------------------------------------------------------------------


class GitError(Exception):
    """git の実行失敗（fail-closed で検査失敗にする）。"""


def _git(repo: str, *args: str, ok_codes: Tuple[int, ...] = (0,)) -> "subprocess.CompletedProcess[str]":
    proc = subprocess.run(
        ["git", "-C", repo, *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        shell=False,
    )
    if proc.returncode not in ok_codes:
        raise GitError(f"git failed: {proc.stderr.strip()}")
    return proc


def main(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser(prog="check_rejection_pins.py")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--base", required=True)
    try:
        args = ap.parse_args(argv)
    except SystemExit as e:
        return 2 if e.code not in (0, None) else 0
    if not _SHA_RE.match(args.base):
        print("base must be a full commit SHA", file=sys.stderr)
        return 2
    repo = args.repo
    try:
        try:
            mb = _git(repo, "merge-base", args.base, "HEAD").stdout.strip()
        except GitError as e:
            raise GitError(f"merge-base failed (shallow clone? fetch full history): {e}")
        diff = _git(
            repo, "-c", "core.quotePath=false", "-c", "diff.relative=false",
            "diff", "--no-color", "--no-ext-diff", "--no-textconv", "--find-renames", "-U0",
            "--src-prefix=a/", "--dst-prefix=b/", f"{args.base}...HEAD", "--", *PATHSPECS,
        ).stdout

        def show(rev: str, path: str) -> Optional[str]:
            p = _git(repo, "show", f"{rev}:{path}", ok_codes=(0, 128))
            return p.stdout if p.returncode == 0 else None

        def head_exists(path: str) -> bool:
            p = _git(repo, "cat-file", "-t", f"HEAD:{path}", ok_codes=(0, 128))
            return p.returncode == 0 and p.stdout.strip() == "blob"

        files = parse_diff(diff)
        viol, n_tests, n_pins = check(
            files, lambda p: show("HEAD", p), lambda p: show(mb, p), head_exists
        )
    except (GitError, DiffParseError) as e:
        print(f"check_rejection_pins: error: {e}", file=sys.stderr)
        return 1
    for v in viol:
        print(v.render())
    if viol:
        print(f"check_rejection_pins: {len(viol)} violation(s)")
        return 1
    print(f"check_rejection_pins: ok ({n_tests} changed test(s), {n_pins} rejection pin(s))")
    return 0


if __name__ == "__main__":
    sys.exit(main())
