#!/usr/bin/env python3
"""Is Fusebox behind the clients it speaks as?

Reads .github/client-versions.json, asks each client's public release channel for its
latest version, and keeps one GitHub issue per client that is behind: opened when a
newer release appears, commented on as more follow, closed once Fusebox matches again.

Without GITHUB_TOKEN and GITHUB_REPOSITORY (or with --dry-run) it only prints the
comparison. Exits 1 when a release channel couldn't be read, so a broken source shows
up as a failed run instead of silence.
"""

import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
LABEL = "client-version"
API = "https://api.github.com"


def http(url, method="GET", body=None, token=None):
    headers = {"User-Agent": "fusebox-client-versions", "Accept": "application/json"}
    if token and url.startswith(API):
        headers["Authorization"] = f"Bearer {token}"
        headers["Accept"] = "application/vnd.github+json"
    data = json.dumps(body).encode() if body is not None else None
    if data:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    with urllib.request.urlopen(req, timeout=20) as resp:
        return resp.read().decode()


def fetch(url, token=None):
    """One retry: a release channel hiccup shouldn't fail the run."""
    try:
        return http(url, token=token)
    except (urllib.error.URLError, TimeoutError):
        time.sleep(5)
        return http(url, token=token)


def latest(source, token):
    kind = source["kind"]
    if kind == "npm":
        tags = json.loads(fetch(f"https://registry.npmjs.org/-/package/{source['package']}/dist-tags"))
        version = tags["latest"]
    elif kind == "github-release":
        release = json.loads(fetch(f"{API}/repos/{source['repo']}/releases/latest", token))
        version = release["tag_name"].removeprefix(source.get("strip", ""))
    elif kind == "text":
        version = fetch(source["url"]).strip().splitlines()[0]
    elif kind == "json":
        value = json.loads(fetch(source["url"]))
        for key in source["path"].split("."):
            value = value[key]
        version = str(value)
    elif kind == "yaml-line":
        prefix = source["key"] + ":"
        line = next(l for l in fetch(source["url"]).splitlines() if l.startswith(prefix))
        version = line[len(prefix):].strip().strip("'\"")
    else:
        raise ValueError(f"unknown source kind {kind}")
    if source.get("cut"):
        version = version.split(source["cut"])[0]
    return version.strip().removeprefix("v")


def parse(version):
    """`0.161.0` -> (0, 161, 0); pre-releases (`0.162.0-alpha.2`) are not releases."""
    if not re.fullmatch(r"\d+(\.\d+)*", version):
        return None
    return tuple(int(p) for p in version.split("."))


def issue_body(client, latest_version):
    ours = client["version"]
    lines = [
        f"<!-- {LABEL}: {client['id']} -->",
        f"{client['name']} **{latest_version}** is out. Fusebox matches **{ours}**.",
        "",
        f"- Where Fusebox has it: {client['where']}",
    ]
    if client.get("notes"):
        lines.append(f"- Release notes: {client['notes']}")
    if client.get("compare"):
        lines.append(f"- Changes: {client['compare'].format(**{'from': ours, 'to': latest_version})}")
    lines += [
        "",
        "To catch up, check what the new release sends (headers, body fields, websocket messages),",
        "update the code to match, and bump the version in `.github/client-versions.json` and the",
        "constants above together; `cargo test` checks they agree. This issue closes itself once",
        "Fusebox matches the latest release.",
    ]
    return "\n".join(lines)


class Issues:
    def __init__(self, repo, token):
        self.repo, self.token = repo, token
        self.ensure_label()
        found = json.loads(self.call(f"/repos/{repo}/issues?labels={LABEL}&state=open&per_page=100"))
        self.open = {}
        for issue in found:
            m = re.search(rf"<!-- {LABEL}: ([\w-]+) -->", issue.get("body") or "")
            if m:
                self.open[m.group(1)] = issue

    def call(self, path, method="GET", body=None):
        return http(API + path, method, body, self.token)

    def ensure_label(self):
        try:
            self.call(f"/repos/{self.repo}/labels/{LABEL}")
        except urllib.error.HTTPError as e:
            if e.code != 404:
                raise
            self.call(f"/repos/{self.repo}/labels", "POST",
                      {"name": LABEL, "color": "d4a72c", "description": "A client Fusebox speaks as has a newer release"})

    def behind(self, client, latest_version):
        title = f"{client['name']} {latest_version} is out (Fusebox matches {client['version']})"
        body = issue_body(client, latest_version)
        issue = self.open.get(client["id"])
        if not issue:
            self.call(f"/repos/{self.repo}/issues", "POST", {"title": title, "body": body, "labels": [LABEL]})
            return "opened issue"
        if issue["title"] == title:
            return f"issue #{issue['number']} is current"
        self.call(f"/repos/{self.repo}/issues/{issue['number']}", "PATCH", {"title": title, "body": body})
        # Edits don't notify; a comment does.
        self.call(f"/repos/{self.repo}/issues/{issue['number']}/comments", "POST",
                  {"body": f"{client['name']} {latest_version} is out now."})
        return f"updated issue #{issue['number']}"

    def current(self, client):
        issue = self.open.get(client["id"])
        if not issue:
            return ""
        self.call(f"/repos/{self.repo}/issues/{issue['number']}/comments", "POST",
                  {"body": f"Fusebox matches {client['name']} {client['version']} now."})
        self.call(f"/repos/{self.repo}/issues/{issue['number']}", "PATCH", {"state": "closed", "state_reason": "completed"})
        return f"closed issue #{issue['number']}"


def main():
    dry = "--dry-run" in sys.argv
    token, repo = os.environ.get("GITHUB_TOKEN"), os.environ.get("GITHUB_REPOSITORY")
    with open(os.path.join(ROOT, ".github", "client-versions.json")) as f:
        clients = json.load(f)["clients"]
    issues = None if dry or not (token and repo) else Issues(repo, token)
    rows, failed = [], False
    for client in clients:
        try:
            newest = latest(client["source"], token)
            if parse(newest) is None:
                raise ValueError(f"unexpected version {newest!r}")
        except Exception as e:  # noqa: BLE001 - report every broken channel, then fail
            failed = True
            rows.append((client["name"], client["version"], "?", f"could not check: {e}"))
            continue
        ahead = parse(newest) > parse(client["version"])
        if issues:
            action = issues.behind(client, newest) if ahead else issues.current(client)
        else:
            action = ""
        rows.append((client["name"], client["version"], newest, ("behind" if ahead else "current") + (f", {action}" if action else "")))
    table = ["| Client | Fusebox | Latest | Status |", "|---|---|---|---|"]
    table += [f"| {a} | {b} | {c} | {d} |" for a, b, c, d in rows]
    print("\n".join(table))
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write("\n".join(table) + "\n")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
