#!/usr/bin/env python3
"""issue 本文の「ビヘイビア ID」節を spec の対応表から同期する（標準ライブラリのみ）。

役割: private spec（`docs/spec` submodule）の
`04-behavior/records/pg-parity-issue-behavior-ids.json`（issue 番号 → ビヘイビア ID の
対応表）を読み、本リポ（public）の open issue 本文のうちマーカー
`<!-- behavior-ids:start -->` 〜 `<!-- behavior-ids:end -->` で囲んだ節だけを upsert する。
マーカー外の本文は 1 バイトも変えない。節に書くのは ID と spec ディレクトリへの
ポインタのみで、spec 本文・タイトルは書かない（.claude/rules/spec-confidentiality.md）。

呼び出し文脈: `.github/workflows/sync-issue-behavior-ids.yml`（main への docs/spec
gitlink 更新 push・workflow_dispatch）から実行される。ローカルでも
`GH_TOKEN=$(gh auth token) python3 scripts/sync_issue_behavior_ids.py --map <json>
--repo Fandhe-AI/fandhe-db --dry-run` で差分件数を確認できる。

契約:
- 対応表は schema==1・issue 番号は正整数・ID は `^[A-Z]+-[0-9]+$` のみ受理し、
  不一致は fail-closed（API を 1 回も呼ばずに非 0 終了）
- open issue のみ対象（closed・PR・対応表に無い issue は触らない）。内容が同じなら
  更新しない（冪等）。マーカーが壊れている issue は書き換えずエラーとして数え、
  最後に非 0 終了する
- 書き込み間隔（`--write-interval`）と 403/429/5xx のバックオフ再試行で二次レート
  制限に配慮する。待機は必ず「最新本文の GET」より前に置き、GET → upsert → PATCH の
  間には挟まない。PATCH は同じ payload を再送せず、再試行時は最新本文・状態を
  取り直して upsert をやり直す（`sync_issue` 参照）。
- 受容済み残留リスク: GitHub の issue 更新 API には条件付き更新（ETag/If-Match に
  よる楽観ロック）が無いため、GET 直後〜PATCH までの 1 往復分の間にマーカー外が
  手動編集されると、その編集は上書きされうる（窓は待機を含まない往復時間のみ）
- 1 回の実行の書き込み上限（`--max-writes`）に達したら残件数を出して
  正常終了する（冪等なので次回実行が続きを処理する）
- ログには issue 番号・ID・件数・HTTP ステータスのみを出す（応答本文・spec の他の
  内容は出さない）

単体テスト: `scripts/tests/test_sync_issue_behavior_ids.py`（`make scripts-test`）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from typing import Callable

START_MARKER = "<!-- behavior-ids:start -->"
END_MARKER = "<!-- behavior-ids:end -->"
SUPPORTED_SCHEMA = 1
ID_PATTERN = re.compile(r"[A-Z]+-[0-9]+")
ISSUE_NUMBER_PATTERN = re.compile(r"[1-9][0-9]*")
REPO_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
# 節内のポインタ表記。spec のディレクトリを指すのみで本文には触れない
SPEC_POINTER = "docs/spec/04-behavior/"
MAP_POINTER = "docs/spec/04-behavior/records/pg-parity-issue-behavior-ids.json"
# 対応表ファイルの上限（untrusted ではないが、誤ったファイルを丸読みしない防御）
MAX_MAP_BYTES = 4 * 1024 * 1024
API_BASE = "https://api.github.com"
USER_AGENT = "fandhe-db-sync-issue-behavior-ids"
# retry_delay が参照するレート制限ヘッダ（ApiError に載せて呼び出し側へ渡す対象）
RETRY_HEADER_KEYS = ("retry-after", "x-ratelimit-remaining", "x-ratelimit-reset")
# open issue 一覧のページ数上限（per_page=100。無限ページングの防御）
MAX_LIST_PAGES = 100


class MapError(ValueError):
    """対応表の形式不正（fail-closed で全体を中止する）。"""


class MarkerError(ValueError):
    """issue 本文のマーカー不整合（その issue だけ書き換えずに失敗扱いとする）。"""


class ApiError(RuntimeError):
    """GitHub API 呼び出しの失敗（ステータスのみ保持し、応答本文は保持しない）。"""

    def __init__(
        self,
        status: int | None,
        what: str,
        retryable: bool = False,
        retry_headers: dict[str, str] | None = None,
    ) -> None:
        super().__init__(f"{what}: status={status}")
        self.status = status
        # retry=False で呼んだ request が失敗したとき、呼び出し側が再試行するための情報。
        # 待ち秒数は呼び出し側が自分の試行回数で retry_delay を計算する（指数バックオフ
        # を効かせるため）。保持するのは待機計算に使うレート制限ヘッダのみ
        self.retryable = retryable
        self.retry_headers = retry_headers or {}


# --------------------------------------------------
# 純関数（単体テスト対象）
# --------------------------------------------------


def validate_map(data: object) -> dict[int, list[str]]:
    """対応表 JSON を検証し、issue 番号 → ID 列の dict を返す。不正なら MapError。"""
    if not isinstance(data, dict):
        raise MapError("top-level value must be an object")
    schema = data.get("schema")
    # bool は int のサブクラスのため明示的に除外する
    if isinstance(schema, bool) or schema != SUPPORTED_SCHEMA:
        raise MapError(f"unsupported schema (expected {SUPPORTED_SCHEMA})")
    raw_map = data.get("map")
    if not isinstance(raw_map, dict) or not raw_map:
        raise MapError("map must be a non-empty object")
    result: dict[int, list[str]] = {}
    for key, ids in raw_map.items():
        if not isinstance(key, str) or not ISSUE_NUMBER_PATTERN.fullmatch(key):
            raise MapError("issue number must be a positive integer")
        if not isinstance(ids, list) or not ids:
            raise MapError(f"#{key}: ids must be a non-empty array")
        seen: set[str] = set()
        for behavior_id in ids:
            if not isinstance(behavior_id, str) or not ID_PATTERN.fullmatch(behavior_id):
                raise MapError(f"#{key}: invalid behavior id")
            if behavior_id in seen:
                raise MapError(f"#{key}: duplicate behavior id {behavior_id}")
            seen.add(behavior_id)
        number = int(key)
        if number in result:
            # "01" 等の別表記は ISSUE_NUMBER_PATTERN で弾くため通常は到達しない
            raise MapError(f"#{key}: duplicate issue number")
        result[number] = list(ids)
    return result


def load_map(path: str) -> dict[int, list[str]]:
    """対応表ファイルを読み込んで検証する。重複キーも MapError とする。"""
    try:
        size = os.path.getsize(path)
    except OSError as exc:
        raise MapError(f"cannot stat map file: {exc.__class__.__name__}") from None
    if size > MAX_MAP_BYTES:
        raise MapError("map file too large")

    def _no_duplicate_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
        obj: dict[str, object] = {}
        for k, v in pairs:
            if k in obj:
                raise MapError("duplicate key in map file")
            obj[k] = v
        return obj

    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f, object_pairs_hook=_no_duplicate_keys)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise MapError(f"cannot parse map file: {exc.__class__.__name__}") from None
    return validate_map(data)


def render_section(ids: list[str], newline: str = "\n") -> str:
    """マーカーを含む「ビヘイビア ID」節を生成する（ID のポインタのみ）。"""
    lines = [
        START_MARKER,
        "## ビヘイビア ID",
        "",
        f"<!-- {MAP_POINTER} から自動同期（sync-issue-behavior-ids workflow）。"
        "この節は手で編集しない -->",
        "",
    ]
    lines.extend(f"- {behavior_id}（`{SPEC_POINTER}`）" for behavior_id in ids)
    lines.append(END_MARKER)
    return newline.join(lines)


def upsert_section(body: str | None, ids: list[str]) -> str:
    """本文のマーカー節を ids の内容へ置換（無ければ末尾に追加）した本文を返す。

    マーカー外は変更しない。既存節が改行コードの差を除いて同一なら body をそのまま
    返す（呼び出し側は `new != body` で更新要否を判定する）。マーカーが片方だけ・
    複数・逆順の場合は MarkerError。
    """
    text = body or ""
    newline = "\r\n" if "\r\n" in text else "\n"
    section = render_section(ids, newline)
    starts = text.count(START_MARKER)
    ends = text.count(END_MARKER)
    if starts == 0 and ends == 0:
        if not text:
            return section + newline
        if text.endswith(newline * 2):
            sep = ""
        elif text.endswith(newline):
            sep = newline
        else:
            sep = newline * 2
        return text + sep + section + newline
    if starts != 1 or ends != 1:
        raise MarkerError("markers must appear exactly once each")
    start = text.index(START_MARKER)
    end = text.index(END_MARKER)
    if end < start:
        raise MarkerError("end marker precedes start marker")
    end += len(END_MARKER)
    current = text[start:end]
    if current.replace("\r\n", "\n") == section.replace("\r\n", "\n"):
        return text
    return text[:start] + section + text[end:]


def parse_next_link(link_header: str | None) -> str | None:
    """Link ヘッダから rel="next" の URL を取り出す（無ければ None）。"""
    if not link_header:
        return None
    for part in link_header.split(","):
        segments = part.strip().split(";")
        if len(segments) < 2:
            continue
        url = segments[0].strip()
        if not (url.startswith("<") and url.endswith(">")):
            continue
        if any(s.strip() == 'rel="next"' for s in segments[1:]):
            return url[1:-1]
    return None


def is_retryable(status: int | None, headers: dict[str, str], body_hint: str) -> bool:
    """再試行対象（429・5xx・レート制限起因の 403・ネットワーク失敗）かを判定する。"""
    if status is None or status == 429 or 500 <= status <= 599:
        return True
    if status == 403:
        if "retry-after" in headers or headers.get("x-ratelimit-remaining") == "0":
            return True
        return "rate limit" in body_hint.lower()
    return False


def retry_delay(attempt: int, headers: dict[str, str], now: float, cap: float) -> float:
    """再試行までの待ち秒数（Retry-After → x-ratelimit-reset → 指数バックオフ）。"""
    retry_after = headers.get("retry-after", "")
    if retry_after.isdigit():
        return min(float(retry_after), cap)
    reset = headers.get("x-ratelimit-reset", "")
    if headers.get("x-ratelimit-remaining") == "0" and reset.isdigit():
        return min(max(float(reset) - now, 1.0), cap)
    # 二次レート制限は「最低 1 分待つ」が GitHub の推奨のため 60 秒から倍化する
    return min(60.0 * (2 ** attempt), cap)


# --------------------------------------------------
# GitHub API（urllib）
# --------------------------------------------------


class GitHubClient:
    """GitHub REST API の最小クライアント。応答本文はログへ出さない。"""

    def __init__(
        self,
        token: str,
        max_attempts: int = 5,
        backoff_cap: float = 600.0,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self._token = token
        self._max_attempts = max_attempts
        self._backoff_cap = backoff_cap
        self._sleep = sleep

    def request(
        self,
        method: str,
        url: str,
        payload: dict[str, object] | None = None,
        retry: bool = True,
    ) -> tuple[object, dict[str, str]]:
        """JSON を送受信する。

        retry=True（GET 等の冪等な読み取り向け）は再試行可能な失敗をバックオフして
        再試行する。retry=False（本文 PATCH 向け）は 1 回だけ送り、失敗時は
        ApiError の retryable／retry_headers に再試行可否と待機計算用ヘッダを載せて
        呼び出し側へ返す（古い本文から作った payload を待機後に再送しないため）。
        """
        data = None if payload is None else json.dumps(payload).encode("utf-8")
        attempts = self._max_attempts if retry else 1
        for attempt in range(attempts):
            req = urllib.request.Request(url, data=data, method=method)
            req.add_header("Accept", "application/vnd.github+json")
            req.add_header("Authorization", f"Bearer {self._token}")
            req.add_header("X-GitHub-Api-Version", "2022-11-28")
            req.add_header("User-Agent", USER_AGENT)
            if data is not None:
                req.add_header("Content-Type", "application/json")
            status: int | None
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    headers = {k.lower(): v for k, v in resp.headers.items()}
                    raw = resp.read()
                return (json.loads(raw) if raw else None), headers
            except urllib.error.HTTPError as exc:
                status = exc.code
                headers = {k.lower(): v for k, v in (exc.headers or {}).items()}
                try:
                    # 判定用に先頭だけ読む（ログへは出さない）
                    hint = exc.read(2048).decode("utf-8", "replace")
                except OSError:
                    hint = ""
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                status, headers, hint = None, {}, ""
            retryable = is_retryable(status, headers, hint)
            if not retry:
                kept = {k: v for k, v in headers.items() if k in RETRY_HEADER_KEYS}
                raise ApiError(status, f"{method} failed", retryable, kept)
            if not retryable or attempt + 1 >= attempts:
                raise ApiError(status, f"{method} failed")
            delay = retry_delay(attempt, headers, time.time(), self._backoff_cap)
            print(f"retry: {method} status={status} wait={delay:.0f}s", flush=True)
            self._sleep(delay)
        raise ApiError(None, f"{method} failed")  # 到達しない（ループ内で return/raise）


def list_open_issue_bodies(client: GitHubClient, repo: str) -> dict[int, str | None]:
    """open issue（PR を除く）の番号 → 本文を一括取得する（ページング）。"""
    url: str | None = f"{API_BASE}/repos/{repo}/issues?state=open&per_page=100"
    result: dict[int, str | None] = {}
    pages = 0
    while url:
        pages += 1
        if pages > MAX_LIST_PAGES:
            raise ApiError(None, "too many pages")
        # 次ページ URL は API 応答由来のため、同一ホスト以外へは送らない（トークン漏えい防止）
        if not url.startswith(f"{API_BASE}/"):
            raise ApiError(None, "unexpected pagination host")
        items, headers = client.request("GET", url)
        if not isinstance(items, list):
            raise ApiError(None, "unexpected list response")
        for item in items:
            if not isinstance(item, dict) or "pull_request" in item:
                continue
            number = item.get("number")
            body = item.get("body")
            if isinstance(number, int) and (body is None or isinstance(body, str)):
                result[number] = body
        url = parse_next_link(headers.get("link"))
    return result


def sync_issue(
    client: GitHubClient,
    url: str,
    ids: list[str],
    sleep: Callable[[float], None] = time.sleep,
    max_attempts: int = 5,
    backoff_cap: float = 600.0,
    now: Callable[[], float] = time.time,
) -> str:
    """1 issue の節を同期する。戻り値は "updated" / "unchanged" / "not-open"。

    各試行で最新本文・状態を GET し、その本文から upsert した payload を直ちに PATCH
    する（GET と PATCH の間に待機を挟まない）。PATCH が再試行可能な理由で失敗した
    場合は同じ payload を再送せず、待機後に GET からやり直す（待機中の手動編集・
    close を取り込む）。待機は本関数の試行回数で retry_delay を計算し、Retry-After 等が
    無い連続失敗では 60/120/240… 秒と指数的に伸ばす。残る GET→PATCH 間の競合は
    モジュール docstring の受容済み残留リスクを参照。MarkerError・ApiError は呼び出し側へ送出する。
    """
    for attempt in range(max_attempts):
        issue, _ = client.request("GET", url)
        if not isinstance(issue, dict) or issue.get("state") != "open":
            return "not-open"
        body = issue.get("body")
        if body is not None and not isinstance(body, str):
            raise ApiError(None, "unexpected issue body")
        new_body = upsert_section(body, ids)
        if new_body == (body or ""):
            return "unchanged"
        try:
            client.request("PATCH", url, {"body": new_body}, retry=False)
        except ApiError as exc:
            if not exc.retryable or attempt + 1 >= max_attempts:
                raise
            delay = retry_delay(attempt, exc.retry_headers, now(), backoff_cap)
            print(f"retry: PATCH status={exc.status} wait={delay:.0f}s (refetch)",
                  flush=True)
            sleep(delay)
            continue
        return "updated"
    raise ApiError(None, "PATCH failed")  # 到達しない（ループ内で return/raise）


# --------------------------------------------------
# エントリポイント
# --------------------------------------------------


def run(
    args: argparse.Namespace,
    client: GitHubClient,
    sleep: Callable[[float], None] = time.sleep,
) -> int:
    """同期本体。戻り値は終了コード（0: 成功・1: 一部失敗）。"""
    mapping = load_map(args.map)
    print(f"map: {len(mapping)} issues", flush=True)
    open_bodies = list_open_issue_bodies(client, args.repo)
    targets = sorted(n for n in mapping if n in open_bodies)
    skipped = len(mapping) - len(targets)
    print(f"open targets: {len(targets)} (skipped not-open: {skipped})", flush=True)

    pending: list[int] = []
    errors = 0
    for number in targets:
        try:
            new_body = upsert_section(open_bodies[number], mapping[number])
        except MarkerError:
            print(f"error: #{number} malformed markers (left unchanged)", flush=True)
            errors += 1
            continue
        if new_body != (open_bodies[number] or ""):
            pending.append(number)
    print(f"to update: {len(pending)} (unchanged: {len(targets) - len(pending) - errors})",
          flush=True)
    if args.dry_run:
        for number in pending:
            print(f"would update: #{number} ids={','.join(mapping[number])}", flush=True)
        print(f"dry-run: {len(pending)} issues would be updated, {errors} errors", flush=True)
        return 1 if errors else 0

    updated = 0
    for index, number in enumerate(pending):
        if updated >= args.max_writes:
            print(f"write cap reached: remaining {len(pending) - index} (rerun to continue)",
                  flush=True)
            break
        url = f"{API_BASE}/repos/{args.repo}/issues/{number}"
        # 書き込み間隔の待機は最新本文の GET より前に置く（GET→PATCH 間に挟むと、
        # 待機中の手動編集を古い本文で上書きしうるため）
        if updated:
            sleep(args.write_interval)
        try:
            # 一覧取得から時間が経つため、sync_issue が書き込み直前に最新本文・状態を
            # 取り直す
            outcome = sync_issue(client, url, mapping[number], sleep)
            if outcome == "not-open":
                print(f"skip: #{number} no longer open", flush=True)
                continue
            if outcome == "unchanged":
                print(f"skip: #{number} already up to date", flush=True)
                continue
        except MarkerError:
            print(f"error: #{number} malformed markers (left unchanged)", flush=True)
            errors += 1
            continue
        except ApiError as exc:
            print(f"error: #{number} status={exc.status}", flush=True)
            errors += 1
            continue
        updated += 1
        print(f"updated: #{number} ids={','.join(mapping[number])}", flush=True)
    print(f"done: {updated} updated, {errors} errors", flush=True)
    return 1 if errors else 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--map", required=True, help="issue→behavior ID map JSON path")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""),
                        help="owner/name (default: $GITHUB_REPOSITORY)")
    parser.add_argument("--dry-run", action="store_true", help="report counts only")
    parser.add_argument("--max-writes", type=int, default=300,
                        help="max issue updates per run (default: 300)")
    parser.add_argument("--write-interval", type=float, default=2.5,
                        help="seconds between writes (default: 2.5)")
    args = parser.parse_args(argv)
    if not REPO_PATTERN.fullmatch(args.repo):
        parser.error("--repo must be owner/name")
    if args.max_writes < 1:
        parser.error("--max-writes must be >= 1")
    if args.write_interval < 0:
        parser.error("--write-interval must be >= 0")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN") or ""
    try:
        # API 呼び出し前に対応表を検証し、不正なら何もせず止める（fail-closed）
        load_map(args.map)
        if not token:
            print("error: GITHUB_TOKEN (or GH_TOKEN) is not set", file=sys.stderr)
            return 2
        return run(args, GitHubClient(token))
    except MapError as exc:
        print(f"error: invalid map: {exc}", file=sys.stderr)
        return 2
    except ApiError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
