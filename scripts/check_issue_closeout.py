#!/usr/bin/env python3
"""親 Issue の close 前に、子 PR 本文の「対象外」節と受け皿 Issue を突合する（標準ライブラリのみ）。

役割: 親 Issue 配下の子孫 Issue をマージで close した PR の本文から「対象外」節を
抽出し、項目ごとの受け皿（`#N` 参照）の有無と開閉、および「相手が結線する」の送り合い
（循環）を報告する。受け皿のない項目や循環が、親の close 後に見過ごされるのを防ぐ
（Issue #3168）。`--check-closed` は、close 済み Issue を指す `tracked:` 注記を
リポ全体から列挙する（ADR `docs/design/rejection-pin-annotation.md` D10。書式は D2）。

呼び出し文脈: `scripts/check_issue_closeout.sh`（薄い入口）から実行される手動の補助
ツール。ネットワーク（`gh`）と認証が要るため CI・make ターゲットには載せない
（`sync_issue_behavior_ids.py` と同じ位置づけ）。

契約:
- 終了コード: 0=検出なし / 1=検出あり（UNTRACKED・CLOSED-ONLY・CYCLE・
  TRACKED-CLOSED・TRACKED-NOT-ISSUE） / 2=使用法の誤り / 3=取得の失敗
- fail-closed: `gh` の失敗・GraphQL エラー（許容する NOT_FOUND を除く）・ページの
  打ち切り・ノード数の上限超過・JSON の不正・`git ls-files` の失敗は終了コード 3。
  いずれも「検出なし」とは扱わない
- 機密保持: PR 本文・Issue タイトル・項目本文は出力に再掲しない。出すのは Issue/PR
  番号、節の見出し（無害化・80 文字で切り詰め）、分類だけ
- subprocess はリスト引数のみ（shell 不使用）。GraphQL の値は変数で渡し、alias 名は
  検証済みの int から組み立てる
- 出力文字列は英語

単体テスト: `scripts/tests/test_check_issue_closeout.py`（`make scripts-test`）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Callable, TextIO

DEFAULT_REPO = "Fandhe-AI/fandhe-db"
REPO_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")

MAX_NUMBER = 10**9
MAX_BODY_BYTES = 256 * 1024
MAX_RANGE_SPAN = 20
MAX_SUBISSUE_PAGES = 50
RESOLVE_CHUNK = 50
MAX_HEADING_CHARS = 80
MAX_SCAN_FILE_BYTES = 4 * 1024 * 1024
GH_TIMEOUT_SECONDS = 60

# ADR docs/design/rejection-pin-annotation.md D2 の `tracked:` 注記書式。
# 書式不正の判定は別 Issue（#3164）の担当なので、ここでは一致した行だけを数える。
TRACKED_LINE = re.compile(
    r"^\s*///?\s*tracked:\s+(#[1-9][0-9]*(?:, #[1-9][0-9]*)*)\s*$"
)
# ADR D4 の走査対象（engine/wire-server の tests と各 crate の src の Rust ファイル）。
SCAN_PATH = re.compile(
    r"^crates/(?:(?:engine|wire-server)/tests/|[^/]+/src/).*\.rs$"
)

HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$")
OUT_OF_SCOPE_RE = re.compile(r"out[- ]of[- ]scope", re.IGNORECASE)
LIST_ITEM_RE = re.compile(r"^(\s*)(?:[-*+]|[0-9]{1,9}[.)])\s+(.*)$")
EMPTY_MARKERS = {"なし", "特になし", "none", "na"}
RANGE_RE = re.compile(
    r"(?<![A-Za-z0-9_/.&#-])#([1-9][0-9]{0,9})\s*[〜~-]\s*#([1-9][0-9]{0,9})(?![0-9])"
)
REF_RE = re.compile(r"(?<![A-Za-z0-9_/.&#-])#([1-9][0-9]{0,9})(?![0-9])")
CONTROL_RE = re.compile(r"[\x00-\x1f\x7f-\x9f]")


class FetchError(Exception):
    """取得の失敗（終了コード 3 へ写像する）。メッセージは種別と番号だけにする。"""


@dataclass
class Section:
    heading: str
    items: list[str] = field(default_factory=list)


@dataclass
class Info:
    kind: str  # "issue" | "pr" | "missing"
    state: str = ""  # "open" | "closed"（PR は merged も closed 扱い）
    body: str = ""


@dataclass
class Finding:
    label: str
    line: str


Runner = Callable[[str, dict], dict]


# ---------------------------------------------------------------- 本文解析


def sanitize_heading(text: str) -> str:
    """端末へ出す見出しから制御文字を除き、引用符をエスケープして切り詰める。"""
    cleaned = CONTROL_RE.sub("", text).replace("\\", "\\\\").replace('"', '\\"')
    return cleaned[:MAX_HEADING_CHARS]


def is_out_of_scope_heading(text: str) -> bool:
    return "対象外" in text or OUT_OF_SCOPE_RE.search(text) is not None


def is_empty_marker(text: str) -> bool:
    return re.sub(r"[\W_]+", "", text.lower()) in EMPTY_MARKERS


def _split_items(lines: list[str]) -> list[str]:
    """節内の行から、最小インデントのリスト項目を 1 件として結合した項目本文を返す。"""
    parsed = []
    for line in lines:
        m = LIST_ITEM_RE.match(line)
        indent = len(line) - len(line.lstrip())
        parsed.append((indent, m.group(2) if m else None, line.strip()))
    bases = [ind for ind, txt, _ in parsed if txt is not None]
    if not bases:
        return []
    base = min(bases)
    items: list[list[str]] = []
    current: list[str] | None = None
    for indent, txt, raw in parsed:
        if txt is not None and indent == base:
            current = [txt]
            items.append(current)
        elif current is not None and indent > base:
            current.append(raw)
        elif txt is None and current is not None and indent <= base:
            current = None
    return [" ".join(parts) for parts in items]


def extract_out_of_scope_sections(body: str) -> list[Section]:
    """PR 本文から「対象外」節（フェンス外の ATX 見出し）を抽出する。untrusted 入力。"""
    text = body.replace("\r\n", "\n").replace("\r", "\n")
    sections: list[Section] = []
    fence = False
    active_level = 0
    active: Section | None = None
    buf: list[str] = []

    def close() -> None:
        nonlocal active, buf
        if active is not None:
            active.items = [i for i in _split_items(buf) if not is_empty_marker(i)]
            sections.append(active)
        active, buf = None, []

    for line in text.split("\n"):
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            fence = not fence
            continue
        if fence:
            continue
        m = HEADING_RE.match(line)
        if m:
            level = len(m.group(1))
            if active is not None and level <= active_level:
                close()
            if active is None and is_out_of_scope_heading(m.group(2)):
                active = Section(heading=sanitize_heading(m.group(2)))
                active_level = level
            continue
        if active is not None and line.strip():
            buf.append(line)
    close()
    return sections


def extract_refs(text: str, repo: str, exclude: set[int] | None = None) -> list[int]:
    """項目本文から同一リポの Issue/PR 参照番号を重複なく抽出する。"""
    text = re.sub(
        r"https://github\.com/" + re.escape(repo) + r"/(?:issues|pull)/([0-9]{1,10})(?![0-9])",
        lambda m: f" #{m.group(1)} ",
        text,
    )

    def expand(m: re.Match) -> str:
        a, b = int(m.group(1)), int(m.group(2))
        if 1 <= b - a <= MAX_RANGE_SPAN:
            return " " + " ".join(f"#{n}" for n in range(a, b + 1)) + " "
        return f" #{a} #{b} "

    text = RANGE_RE.sub(expand, text)
    seen: list[int] = []
    for m in REF_RE.finditer(text):
        n = int(m.group(1))
        if n > MAX_NUMBER or n in seen or (exclude and n in exclude):
            continue
        seen.append(n)
    return seen


def classify_item(refs: list[int], infos: dict[int, Info]) -> str:
    """`untracked` / `ok`（open の Issue を 1 件以上指す） / `closed-only`。"""
    if not refs:
        return "untracked"
    for n in refs:
        info = infos.get(n)
        if info is not None and info.kind == "issue" and info.state == "open":
            return "ok"
    return "closed-only"


def describe_ref(n: int, infos: dict[int, Info]) -> str:
    info = infos.get(n, Info("missing"))
    if info.kind == "missing":
        return f"#{n}(missing)"
    return f"#{n}({info.kind},{info.state})"


def find_cycles(edges: dict[int, list[int]]) -> list[list[int]]:
    """反復版 Tarjan SCC。大きさ 2 以上の強連結成分を昇順・決定的に返す。"""
    index: dict[int, int] = {}
    low: dict[int, int] = {}
    on_stack: set[int] = set()
    stack: list[int] = []
    result: list[list[int]] = []
    counter = 0
    nodes = set(edges)
    for targets in edges.values():
        nodes.update(targets)
    for root in sorted(nodes):
        if root in index:
            continue
        work = [(root, iter(sorted(edges.get(root, []))))]
        index[root] = low[root] = counter
        counter += 1
        stack.append(root)
        on_stack.add(root)
        while work:
            node, it = work[-1]
            advanced = False
            for nxt in it:
                if nxt not in index:
                    index[nxt] = low[nxt] = counter
                    counter += 1
                    stack.append(nxt)
                    on_stack.add(nxt)
                    work.append((nxt, iter(sorted(edges.get(nxt, [])))))
                    advanced = True
                    break
                if nxt in on_stack:
                    low[node] = min(low[node], index[nxt])
            if advanced:
                continue
            work.pop()
            if work:
                parent = work[-1][0]
                low[parent] = min(low[parent], low[node])
            if low[node] == index[node]:
                comp = []
                while True:
                    w = stack.pop()
                    on_stack.discard(w)
                    comp.append(w)
                    if w == node:
                        break
                if len(comp) >= 2:
                    result.append(sorted(comp))
    return sorted(result)


# ---------------------------------------------------------------- 取得（I/O 層）


def _first_line(text: str) -> str:
    lines = text.strip().splitlines()
    return (lines[0] if lines else "")[:200]


def gh_graphql(query: str, variables: dict) -> dict:
    """`gh api graphql` を呼ぶ。NOT_FOUND 等の errors 付き応答も JSON として返す。"""
    cmd = ["gh", "api", "graphql", "-f", f"query={query}"]
    for key, value in variables.items():
        if value is None:
            continue
        cmd += ["-F" if isinstance(value, int) else "-f", f"{key}={value}"]
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=GH_TIMEOUT_SECONDS, check=False
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise FetchError(f"gh invocation failed ({type(exc).__name__})") from None
    try:
        parsed = json.loads(proc.stdout)
    except ValueError:
        parsed = None
    if not isinstance(parsed, dict) or ("data" not in parsed and "errors" not in parsed):
        raise FetchError(f"gh api failed (exit {proc.returncode}): {_first_line(proc.stderr)}")
    if proc.returncode != 0 and "errors" not in parsed:
        raise FetchError(f"gh api failed (exit {proc.returncode})")
    return parsed


NODE_QUERY = """
query($owner:String!,$name:String!,$number:Int!,$cursor:String){
  repository(owner:$owner,name:$name){
    issue(number:$number){
      number state
      subIssues(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{number state}}
      closedByPullRequestsReferences(first:20,includeClosedPrs:true){
        pageInfo{hasNextPage} nodes{number merged body}}
    }
  }
}"""


@dataclass
class Node:
    number: int
    state: str
    children: list[int]
    merged_prs: list[tuple[int, str]]  # (PR 番号, 本文)
    truncated_prs: bool


def fetch_node(runner: Runner, repo: str, number: int) -> Node:
    owner, name = repo.split("/", 1)
    children: list[int] = []
    cursor: str | None = None
    first: dict | None = None
    for _ in range(MAX_SUBISSUE_PAGES):
        resp = runner(NODE_QUERY, {"owner": owner, "name": name, "number": number, "cursor": cursor})
        if resp.get("errors"):
            raise FetchError(f"graphql error while fetching issue #{number}")
        issue = ((resp.get("data") or {}).get("repository") or {}).get("issue")
        if not isinstance(issue, dict):
            raise FetchError(f"issue #{number} not found")
        if first is None:
            first = issue
        subs = issue.get("subIssues") or {}
        for n in subs.get("nodes") or []:
            children.append(int(n["number"]))
        page = subs.get("pageInfo") or {}
        if not page.get("hasNextPage"):
            break
        cursor = page.get("endCursor")
        if not cursor:
            raise FetchError(f"sub-issue paging broken for #{number}")
    else:
        raise FetchError(f"sub-issue pages exceeded for #{number}")
    assert first is not None
    refs = first.get("closedByPullRequestsReferences") or {}
    truncated = bool((refs.get("pageInfo") or {}).get("hasNextPage"))
    prs = []
    for pr in refs.get("nodes") or []:
        if pr.get("merged") is True:
            body = (pr.get("body") or "").encode("utf-8")[:MAX_BODY_BYTES].decode("utf-8", "ignore")
            prs.append((int(pr["number"]), body))
    return Node(number, str(first.get("state", "")).lower(), children, prs, truncated)


def resolve_numbers(
    runner: Runner, repo: str, numbers: list[int], cache: dict[int, Info]
) -> None:
    """番号を Issue/PR/missing に解決して cache へ入れる（50 件ずつ aliases で一括取得）。"""
    owner, name = repo.split("/", 1)
    todo = sorted({n for n in numbers if n not in cache})
    for i in range(0, len(todo), RESOLVE_CHUNK):
        chunk = todo[i : i + RESOLVE_CHUNK]
        fields = "".join(
            "n%d: issueOrPullRequest(number:%d){__typename ... on Issue{state} "
            "... on PullRequest{state merged body}} " % (n, n)
            for n in chunk
        )
        query = (
            "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){"
            + fields
            + "}}"
        )
        resp = runner(query, {"owner": owner, "name": name})
        aliases = {f"n{n}" for n in chunk}
        for err in resp.get("errors") or []:
            path = err.get("path") or []
            if not (err.get("type") == "NOT_FOUND" and path and path[-1] in aliases):
                raise FetchError("graphql error while resolving references")
        repo_data = (resp.get("data") or {}).get("repository")
        if not isinstance(repo_data, dict):
            raise FetchError("graphql response missing repository")
        for n in chunk:
            node = repo_data.get(f"n{n}")
            if not isinstance(node, dict):
                cache[n] = Info("missing")
            elif node.get("__typename") == "PullRequest":
                body = (node.get("body") or "").encode("utf-8")[:MAX_BODY_BYTES].decode("utf-8", "ignore")
                cache[n] = Info("pr", str(node.get("state", "")).lower(), body)
            else:
                cache[n] = Info("issue", str(node.get("state", "")).lower())


@dataclass
class Item:
    issue: int
    pr: int
    heading: str
    index: int
    refs: list[int]


def items_from_prs(
    prs: list[tuple[int, str]], issue: int, repo: str, seen_prs: set[int]
) -> tuple[list[Item], int]:
    items: list[Item] = []
    sections = 0
    for pr_number, body in prs:
        if pr_number in seen_prs:
            continue
        seen_prs.add(pr_number)
        for sec in extract_out_of_scope_sections(body):
            sections += 1
            for idx, text in enumerate(sec.items, 1):
                items.append(
                    Item(issue, pr_number, sec.heading, idx,
                         extract_refs(text, repo, {issue, pr_number}))
                )
    return items, sections


# ---------------------------------------------------------------- 突合


def closeout(
    runner: Runner, repo: str, parent: int, max_hops: int, max_nodes: int,
    verbose: bool, out: TextIO, err: TextIO,
) -> int:
    root = fetch_node(runner, repo, parent)
    descendants: list[Node] = []
    visited = {parent}
    queue = list(root.children)
    while queue:
        n = queue.pop(0)
        if n in visited:
            continue
        visited.add(n)
        if len(visited) > max_nodes:
            raise FetchError("node limit exceeded")
        node = fetch_node(runner, repo, n)
        descendants.append(node)
        queue.extend(node.children)

    all_items: list[Item] = []
    notes: list[str] = []
    sections_total = 0
    merged_total = 0
    seen_prs: set[int] = set()
    edges: dict[int, list[int]] = {}
    for node in descendants:
        if node.truncated_prs:
            raise FetchError(f"closing PR list truncated for issue #{node.number}")
        if node.state != "closed":
            notes.append(f"note: issue #{node.number} is still {node.state or 'unknown'}")
        elif not node.merged_prs:
            notes.append(f"note: issue #{node.number} has no merged closing PR")
        merged_total += len(node.merged_prs)
        items, secs = items_from_prs(node.merged_prs, node.number, repo, seen_prs)
        sections_total += secs
        all_items.extend(items)
        edges.setdefault(node.number, [])
        for it in items:
            edges[node.number].extend(r for r in it.refs if r not in edges[node.number])
            edges.setdefault(it.pr, edges[node.number])

    cache: dict[int, Info] = {}
    in_tree = visited
    referenced = sorted({r for it in all_items for r in it.refs})
    resolve_numbers(runner, repo, referenced, cache)

    # ツリー外の参照先を max_hops 段だけたどり、循環の検出にだけ使う
    frontier = [n for n in referenced if n not in edges and n not in in_tree]
    expanded = 0
    for _ in range(max_hops):
        nxt: list[int] = []
        for n in frontier:
            if n in edges:
                continue
            expanded += 1
            if len(visited) + expanded > max_nodes:
                raise FetchError("node limit exceeded")
            info = cache.get(n, Info("missing"))
            refs: list[int] = []
            if info.kind == "pr":
                prs = [(n, info.body)]
            elif info.kind == "issue" and info.state == "closed":
                node = fetch_node(runner, repo, n)
                if node.truncated_prs:
                    raise FetchError(f"closing PR list truncated for issue #{n}")
                prs = node.merged_prs
            else:
                prs = []
            for it in items_from_prs(prs, n, repo, set())[0]:
                refs.extend(r for r in it.refs if r not in refs)
            edges[n] = refs
            nxt.extend(r for r in refs if r not in edges and r not in in_tree)
        resolve_numbers(runner, repo, nxt, cache)
        frontier = sorted(set(nxt))
        if not frontier:
            break
    cycles = find_cycles(edges)

    counts = {"untracked": 0, "closed-only": 0, "ok": 0}
    lines: list[str] = []
    for it in all_items:
        kind = classify_item(it.refs, cache)
        counts[kind] += 1
        base = f'issue #{it.issue} pr #{it.pr} section "{it.heading}" item {it.index}'
        if kind == "untracked":
            lines.append(f"UNTRACKED   {base}")
        elif kind == "closed-only":
            lines.append(f"CLOSED-ONLY {base} refs " + " ".join(describe_ref(r, cache) for r in it.refs))
        elif verbose:
            lines.append(f"OK          {base} refs " + " ".join(describe_ref(r, cache) for r in it.refs))
    for comp in cycles:
        lines.append("CYCLE       " + " -> ".join(f"#{n}" for n in comp + [comp[0]]))

    print(
        f"parent #{parent}: descendants={len(descendants)} merged_prs={merged_total} "
        f"sections={sections_total} items={len(all_items)}",
        file=out,
    )
    for line in lines + notes:
        print(line, file=out)
    print(
        f"summary: untracked={counts['untracked']} closed-only={counts['closed-only']} "
        f"cycles={len(cycles)} ok={counts['ok']}",
        file=out,
    )
    return 1 if (counts["untracked"] or counts["closed-only"] or cycles) else 0


# ---------------------------------------------------------------- --check-closed


def git_ls_files(root: str) -> list[str]:
    try:
        proc = subprocess.run(
            ["git", "-C", root, "ls-files", "-z", "--", "crates"],
            capture_output=True, text=True, timeout=GH_TIMEOUT_SECONDS, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise FetchError(f"git ls-files failed ({type(exc).__name__})") from None
    if proc.returncode != 0:
        raise FetchError(f"git ls-files failed (exit {proc.returncode})")
    return [p for p in proc.stdout.split("\0") if p]


def check_closed(
    runner: Runner, repo: str, root: str, files: list[str], out: TextIO
) -> int:
    hits: list[tuple[str, int, int]] = []
    for rel in files:
        if not SCAN_PATH.match(rel):
            continue
        path = os.path.join(root, rel)
        if os.path.islink(path) or not os.path.isfile(path):
            continue
        try:
            with open(path, "rb") as fh:
                data = fh.read(MAX_SCAN_FILE_BYTES).decode("utf-8", "replace")
        except OSError:
            raise FetchError(f"cannot read {rel}") from None
        for lineno, line in enumerate(data.split("\n"), 1):
            m = TRACKED_LINE.match(line)
            if m:
                for n in re.findall(r"#([1-9][0-9]*)", m.group(1)):
                    if int(n) <= MAX_NUMBER:
                        hits.append((rel, lineno, int(n)))
    cache: dict[int, Info] = {}
    resolve_numbers(runner, repo, [n for _, _, n in hits], cache)
    problems = 0
    for rel, lineno, n in hits:
        info = cache.get(n, Info("missing"))
        if info.kind == "issue" and info.state == "closed":
            print(f"TRACKED-CLOSED {rel}:{lineno} #{n}", file=out)
            problems += 1
        elif info.kind != "issue":
            print(f"TRACKED-NOT-ISSUE {rel}:{lineno} #{n} ({info.kind})", file=out)
            problems += 1
    print(f"check-closed: annotations={len(hits)} problems={problems}", file=out)
    return 1 if problems else 0


# ---------------------------------------------------------------- エントリ


def _ranged_int(lo: int, hi: int) -> Callable[[str], int]:
    def conv(value: str) -> int:
        n = int(value)
        if not lo <= n <= hi:
            raise argparse.ArgumentTypeError(f"must be between {lo} and {hi}")
        return n

    return conv


def _repo_arg(value: str) -> str:
    if not REPO_PATTERN.match(value):
        raise argparse.ArgumentTypeError("must look like OWNER/NAME")
    return value


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="check_issue_closeout.sh",
        description="Cross-check out-of-scope items in child PR bodies against follow-up issues.",
    )
    p.add_argument("parent", nargs="?", type=_ranged_int(1, MAX_NUMBER), help="parent issue number")
    p.add_argument("--repo", type=_repo_arg, default=DEFAULT_REPO)
    p.add_argument("--max-hops", type=_ranged_int(0, 3), default=1)
    p.add_argument("--max-nodes", type=_ranged_int(1, 5000), default=500)
    p.add_argument("--check-closed", action="store_true")
    p.add_argument("--verbose", action="store_true")
    p.add_argument("--root", default=os.getcwd(), help=argparse.SUPPRESS)
    return p


def run(
    args: argparse.Namespace, runner: Runner, git_runner: Callable[[str], list[str]],
    out: TextIO, err: TextIO,
) -> int:
    code = 0
    try:
        if args.parent is not None:
            code = max(code, closeout(runner, args.repo, args.parent, args.max_hops,
                                      args.max_nodes, args.verbose, out, err))
        if args.check_closed:
            code = max(code, check_closed(runner, args.repo, args.root, git_runner(args.root), out))
    except FetchError as exc:
        print(f"error: {exc}", file=err)
        return 3
    return code


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.parent is None and not args.check_closed:
        parser.error("a parent issue number or --check-closed is required")
    return run(args, gh_graphql, git_ls_files, sys.stdout, sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
