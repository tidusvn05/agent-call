#!/usr/bin/env python3
"""Release helpers for agent-call (the procedure is in docs/releasing.md).

    scripts/release.py version 0.5.0       set the workspace version and every internal dependency
    scripts/release.py order               print the crates in publishing order
    scripts/release.py pending             print the crates whose version is not on crates.io yet
    scripts/release.py publish [--dry-run] publish every crate to crates.io, in order
    scripts/release.py trustpub OWNER/REPO [--workflow F] [--environment E] [--crates a,b] [--only]
                                           set up crates.io Trusted Publishing for the crates

`publish` can be run again after a failure: versions already on crates.io are skipped. When
crates.io rate-limits new crates (HTTP 429, about one new crate every 10 minutes after a short
burst), it waits until the time crates.io gives and retries.

`trustpub` needs a crates.io API token with the `trusted-publishing` scope: the token saved by `cargo login`.
It lets the GitHub workflow publish the crates (by default this workspace's) without any stored
token; `--only` also makes crates.io refuse every other way of publishing them. It works for any
repository: `--crates` names the crates, `--workflow` and `--environment` default to
`release.yml` and `crates-io`. Running it again changes nothing.
"""

import email.utils
import json
import os
import re
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-dev|-rc\.(?:0|[1-9][0-9]*))?")


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def publish_order():
    """Publishable workspace crates, dependencies (dev-dependencies too) before dependents."""
    meta = metadata()
    members = set(meta["workspace_members"])
    packages = {p["name"]: p for p in meta["packages"]
                if p["id"] in members and p.get("publish") != []
                and (p.get("publish") is None or "crates-io" in p["publish"])}
    if not packages:
        sys.exit("no crates.io-publishable workspace crates")
    deps = {
        name: {d["name"] for d in p["dependencies"] if d["name"] in packages and d["name"] != name}
        for name, p in packages.items()
    }
    order, done = [], set()
    while len(order) < len(packages):
        ready = sorted(n for n in packages if n not in done and deps[n] <= done)
        if not ready:
            sys.exit(f"dependency cycle among {sorted(set(packages) - done)}")
        order.extend(ready)
        done.update(ready)
    version = {p["version"] for p in meta["packages"] if p["id"] in members}
    if len(version) != 1:
        sys.exit(f"crates have different versions: {sorted(version)}")
    return order, version.pop()


def set_version(version):
    """Update workspace/package versions and internal dependencies in every member manifest."""
    if not SEMVER.fullmatch(version):
        sys.exit(f"`{version}` is not a version like 0.5.0 or 0.6.0-dev")
    meta = metadata()
    members = [p for p in meta["packages"] if p["id"] in meta["workspace_members"]]
    names = {p["name"] for p in members}
    paths = {ROOT / "Cargo.toml", *(Path(p["manifest_path"]) for p in members)}
    changes = {}
    for path in paths:
        source = path.read_text()
        parsed = tomllib.loads(source)
        headers = list(re.finditer(r"(?m)^\[[^\n]+\]\s*(?:#.*)?$", source))
        edits = []
        for i, header in enumerate(headers):
            if header.group().startswith("[["):
                continue  # [[bin]], [[example]], etc. contain no package/dependency versions.
            # Parse the header itself so quoted aliases and target cfg tables work too.
            section = tomllib.loads(header.group() + "\n__release_marker = true\n")
            keys = []
            while "__release_marker" not in section:
                key, section = next(iter(section.items()))
                keys.append(key)
            data = parsed
            for key in keys:
                data = data[key]
            start = header.end()
            end = headers[i + 1].start() if i + 1 < len(headers) else len(source)
            body = source[start:end]
            if keys in (["workspace", "package"], ["package"]):
                if isinstance(data.get("version"), str):
                    body, count = re.subn(r'(?m)^(version\s*=\s*)[\"\'][^\"\']*[\"\']',
                                          lambda m: m[1] + f'"{version}"', body, count=1)
                    if not count:
                        sys.exit(f"cannot update version in {path}: {keys}")
            elif keys[-1] in ("dependencies", "dev-dependencies", "build-dependencies"):
                entries = list(re.finditer(r"(?m)^([\w-]+|\"[^\"]+\"|'[^']+')\s*=", body))
                for entry in reversed(entries):
                    alias = next(iter(tomllib.loads(entry[1] + " = 0")))
                    dep = data[alias]
                    name = dep.get("package", alias) if isinstance(dep, dict) else alias
                    if name not in names or isinstance(dep, dict) and dep.get("workspace"):
                        continue
                    following = next((e.start() for e in entries if e.start() > entry.start()), len(body))
                    value = body[entry.end():following]
                    if isinstance(dep, str):
                        value = re.sub(r"[\"'][^\"']*[\"']", f'"{version}"', value, count=1)
                    elif isinstance(dep, dict):
                        value = update_dependency(value, dep, version)
                    body = body[:entry.end()] + value + body[following:]
            elif len(keys) >= 2 and keys[-2] in ("dependencies", "dev-dependencies", "build-dependencies"):
                name = data.get("package", keys[-1])
                if name in names and not data.get("workspace"):
                    if "version" in data:
                        body = re.sub(r'(?m)^(version\s*=\s*)[\"\'][^\"\']*[\"\']',
                                      lambda m: m[1] + f'"{version}"', body, count=1)
                    else:
                        body = f'\nversion = "{version}"' + body
            edits.append((start, end, body))
        for start, end, body in reversed(edits):
            source = source[:start] + body + source[end:]
        tomllib.loads(source)  # Validate all manifests before writing any of them.
        changes[path] = source
    for path, source in changes.items():
        path.write_text(source)
    subprocess.run(["cargo", "update", "--workspace", "--quiet"], cwd=ROOT, check=True)
    print(f"version {version} ({len(paths)} manifests)")


def update_dependency(value, dep, version):
    if "version" in dep:
        return re.sub(r"(\bversion\s*=\s*)[\"'][^\"']*[\"']",
                      lambda m: m[1] + f'"{version}"', value, count=1)
    # Inline path dependencies also need a registry version for publishing.
    close = value.rfind("}")
    if close < 0:
        sys.exit("cannot update internal dependency: expected an inline TOML table")
    return value[:close].rstrip().rstrip(",") + f', version = "{version}" ' + value[close:]


def index_versions(name):
    """Versions of `name` on crates.io (from the sparse index), or None for a crate it does not
    have yet."""
    lower = name.lower()
    if len(lower) <= 2:
        prefix = str(len(lower))
    elif len(lower) == 3:
        prefix = f"3/{lower[0]}"
    else:
        prefix = f"{lower[:2]}/{lower[2:4]}"
    url = f"https://index.crates.io/{prefix}/{lower}"
    try:
        with open_retry(urllib.request.Request(url, headers={"User-Agent": USER_AGENT})) as response:
            lines = response.read().decode().splitlines()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    return {json.loads(line).get("vers") for line in lines if line.strip()}


def pending():
    """Prints the crates still to publish. With GITHUB_OUTPUT set, also writes `count` and `new`
    (crates crates.io does not have at all: Trusted Publishing cannot create them)."""
    order, version = publish_order()
    todo, new = [], []
    for name in order:
        versions = index_versions(name)
        if versions is None:
            new.append(name)
        if version not in (versions or set()):
            todo.append(name)
    for name in todo:
        print(f"{name} {version}" + (" (new crate)" if name in new else ""))
    if not todo:
        print(f"every crate is on crates.io at {version}")
    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        with open(github_output, "a") as out:
            out.write(f"count={len(todo)}\nnew={' '.join(new)}\n")


USER_AGENT = "agent-call-release (https://github.com/tidusvn05/agent-call)"


def open_retry(request):
    """Honor Retry-After for API and sparse-index rate limits."""
    while True:
        try:
            return urllib.request.urlopen(request, timeout=30)
        except urllib.error.HTTPError as error:
            if error.code != 429:
                raise
            delay = error.headers.get("Retry-After", "600")
            try:
                wait = max(1, int(delay))
            except ValueError:
                try:
                    when = email.utils.parsedate_to_datetime(delay)
                    wait = max(1, int((when - datetime.now(timezone.utc)).total_seconds()) + 1)
                except (TypeError, ValueError):
                    wait = 600
            error.close()
            print(f"crates.io HTTP 429; retrying in {wait}s", file=sys.stderr, flush=True)
            time.sleep(wait)


def crates_io(method, path, token, body=None):
    request = urllib.request.Request(
        f"https://crates.io/api/v1{path}",
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={
            "Authorization": token,
            "User-Agent": USER_AGENT,
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
    )
    try:
        with open_retry(request) as response:
            return json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        detail = error.read().decode(errors="replace")
        sys.exit(f"crates.io {method} {path}: HTTP {error.code} {detail}")


def cargo_token():
    """The crates.io token saved by `cargo login`, if any."""
    home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    for name in ("credentials.toml", "credentials"):
        path = home / name
        if path.is_file():
            with open(path, "rb") as f:
                return tomllib.load(f).get("registry", {}).get("token", "")
    return ""


def trustpub(args):
    token = cargo_token()
    if not token:
        sys.exit("run `cargo login` with a crates.io token that has the "
                 "`trusted-publishing` scope")
    if not args or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args[0]):
        sys.exit(__doc__)
    owner, repo = args[0].split("/", 1)
    options = {"--workflow": "release.yml", "--environment": "crates-io", "--crates": None}
    only = False
    rest = args[1:]
    while rest:
        flag = rest.pop(0)
        if flag == "--only":
            only = True
        elif flag in options and rest:
            options[flag] = rest.pop(0)
        else:
            sys.exit(__doc__)
    crates = (options["--crates"].split(",") if options["--crates"] else publish_order()[0])
    if not all(re.fullmatch(r"[A-Za-z0-9_-]+", name) for name in crates):
        sys.exit("--crates must be a comma-separated list of crate names")
    environment = options["--environment"] or None
    for name in crates:
        existing = crates_io("GET", f"/trusted_publishing/github_configs?crate={name}", token)
        same = [
            c for c in existing.get("github_configs", [])
            if c["repository_owner"].lower() == owner.lower()
            and c["repository_name"].lower() == repo.lower()
            and c["workflow_filename"] == options["--workflow"]
            and c.get("environment") == environment
        ]
        if same:
            print(f"= {name}: already trusts {owner}/{repo} {options['--workflow']}")
        else:
            crates_io("POST", "/trusted_publishing/github_configs", token, {
                "github_config": {
                    "crate": name,
                    "repository_owner": owner,
                    "repository_name": repo,
                    "workflow_filename": options["--workflow"],
                    "environment": environment,
                }
            })
            print(f"+ {name}: trusts {owner}/{repo} {options['--workflow']}"
                  + (f" (environment {environment})" if environment else ""))
        if only:
            crates_io("PATCH", f"/crates/{name}", token, {"crate": {"trustpub_only": True}})
            print(f"  {name}: Trusted Publishing only")


def retry_after(output):
    """Seconds to wait from a crates.io 429 message ("Please try again after <date>")."""
    match = re.search(r"try again after ([^.]+?(?:GMT|UTC|\+0000))", output)
    if match:
        try:
            when = email.utils.parsedate_to_datetime(match.group(1))
            return max(30, int((when - datetime.now(timezone.utc)).total_seconds()) + 15)
        except (TypeError, ValueError):
            pass
    return 600


def publish(dry_run):
    order, version = publish_order()
    if dry_run:
        # Cargo verifies unpublished workspace dependencies together. Release preparation
        # runs before the version/changelog commit, so allow dirty files only in dry-run mode.
        subprocess.run(["cargo", "publish", "--workspace", "--dry-run", "--locked", "--allow-dirty"],
                       cwd=ROOT, check=True)
        print(f"packaged {len(order)} crates at {version}")
        return
    if "-" in version:
        sys.exit(f"refusing to publish the pre-release version {version}; run `version` first")
    versions = {name: index_versions(name) for name in order}
    new = [name for name in order if versions[name] is None]
    if new:
        sys.exit("New crates: " + ", ".join(new) + ". Trusted Publishing cannot create crates. "
                 "Check name availability and ownership, publish these once by hand with a temporary "
                 "token, then run trustpub OWNER/REPO --crates " + ",".join(new) +
                 " --only and revoke the token (docs/releasing.md).")
    for name in order:
        if version in versions[name]:
            print(f"= {name} {version} is already on crates.io")
            continue
        command = ["cargo", "publish", "--locked", "--registry", "crates-io", "-p", name]
        while True:
            print(f"> {' '.join(command)}", flush=True)
            run = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
            output = run.stdout + run.stderr
            if run.returncode == 0:
                break
            if "429" in output or "Too Many Requests" in output:
                wait = retry_after(output)
                print(f"  rate-limited by crates.io; retrying in {wait // 60} min {wait % 60} s",
                      flush=True)
                time.sleep(wait)
                continue
            if "already exists" in output:
                print(f"= {name} {version} is already on crates.io")
                break
            sys.stderr.write(output)
            sys.exit(f"publishing {name} failed")
    print(f"published {len(order)} crates at {version}")


def main():
    args = sys.argv[1:]
    if args[:1] == ["version"] and len(args) == 2:
        set_version(args[1])
    elif args == ["order"]:
        order, version = publish_order()
        print(f"{version}: " + " ".join(order))
    elif args == ["pending"]:
        pending()
    elif args[:1] == ["publish"] and args[1:] in ([], ["--dry-run"]):
        publish(dry_run=args[1:] == ["--dry-run"])
    elif args[:1] == ["trustpub"]:
        trustpub(args[1:])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(f"command failed ({error.returncode}): {' '.join(error.cmd)}")
    except (urllib.error.URLError, TimeoutError) as error:
        sys.exit(f"registry request failed: {error}")
