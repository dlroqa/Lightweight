#!/usr/bin/env python3
"""Fail on a known high or critical advisory in anything the product ships.

`npm audit` alone answers the wrong question. It calls Electron a development
dependency, because `electron-builder` wants it in `devDependencies`, and so
`npm audit --omit=dev` reports nothing while the Electron runtime it
leaves out is the thing every desktop package is built around. Plain
`npm audit` makes the opposite mistake: it puts the packager's own
`undici` next to the panel's React, and a gate that fails on both is a gate
someone eventually turns off.

So this sorts every advisory by what reaches a user:

  shipped     the panel's runtime dependencies (bundled into the page), the
              Electron runtime itself, and every crate the `hermes` binary is
              linked from. A high or critical advisory here fails the check.
  build/test  the packager, the bundler, the test harness and the Rust
              crates that only build or test. Reported with their severity,
              never silently dropped, and never a failure.

An advisory is waived only by an entry in `scripts/advisory-exceptions.json`
naming that advisory and that package, who approved it and until when. An
expired or malformed entry is itself a failure.

Sources: `npm audit` (the npm registry's bulk endpoint, backed by the GitHub
Advisory Database) and OSV (https://osv.dev, which carries RustSec and the
GitHub database) for the crates. Neither needs a token, and neither is the
GitHub REST API, so the check cannot exhaust the workflow's rate limit. If a
source cannot be reached the check fails: a gate that passes when it could
not look has not checked anything.

    python3 scripts/check-advisories.py              # the repository's policy
    python3 scripts/check-advisories.py --self-test  # prove it rejects Electron 43.4.1
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXCEPTIONS = ROOT / "scripts/advisory-exceptions.json"
FAILING = {"high", "critical"}
ORDER = ["critical", "high", "moderate", "low", "info", "unknown"]


@dataclass
class NpmProject:
    path: str
    why: str
    # Whether `dependencies` reach a user. The panel's do (Vite bundles them);
    # the end-to-end harness's never leave CI.
    ships_runtime_deps: bool
    # Shipped although npm files them under `devDependencies`.
    shipped_anyway: tuple[str, ...] = ()


NPM_PROJECTS = [
    NpmProject("frontend", "the panel; its runtime dependencies are bundled into it", True),
    NpmProject(
        "apps/desktop",
        "the desktop shell; electron-builder ships the Electron runtime itself",
        True,
        shipped_anyway=("electron",),
    ),
    NpmProject("e2e", "the end-to-end harness; nothing here ships", False),
]

# The crate whose binaries are released (`hermes`, `lightweight`).
SHIPPED_CRATE = "lightweight-cli"


@dataclass
class Finding:
    ecosystem: str
    project: str
    package: str
    version: str
    advisory: str
    severity: str
    title: str
    shipped: bool
    excepted_by: dict | None = field(default=None)


def run(cmd: list[str], cwd: Path) -> str:
    # Both `npm audit` and `npm ls` exit non-zero when they have something to
    # say; their JSON on stdout is the answer either way.
    out = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if not out.stdout.strip():
        raise RuntimeError(f"`{' '.join(cmd)}` in {cwd} printed nothing:\n{out.stderr.strip()}")
    return out.stdout


def npm(*args: str) -> list[str]:
    # `npm` is `npm.cmd` on Windows, which subprocess will not find by name.
    return [shutil.which("npm") or "npm", *args]


def runtime_names(tree: dict) -> set[str]:
    names: set[str] = set()
    stack = list((tree.get("dependencies") or {}).items())
    while stack:
        name, node = stack.pop()
        if name in names:
            continue
        names.add(name)
        stack.extend((node.get("dependencies") or {}).items())
    return names


def audit_npm(project: NpmProject, directory: Path) -> list[Finding]:
    """Every advisory against the lockfile, each marked shipped or not.

    Both commands read the lockfile only, so nothing has to be installed and
    the answer is about exactly what `npm ci` would install."""
    report = json.loads(run(npm("audit", "--json", "--package-lock-only"), directory))
    if "error" in report:
        raise RuntimeError(f"npm audit failed in {directory}: {report['error']}")

    shipped = set(project.shipped_anyway)
    if project.ships_runtime_deps:
        tree = json.loads(run(npm("ls", "--all", "--omit=dev", "--json", "--package-lock-only"), directory))
        shipped |= runtime_names(tree)

    lock = json.loads((directory / "package-lock.json").read_text())
    findings = []
    seen = set()
    for name, vuln in sorted(report.get("vulnerabilities", {}).items()):
        # A string in `via` names another vulnerable package this one depends
        # on; only the objects are advisories against this package itself.
        for via in vuln.get("via", []):
            if not isinstance(via, dict):
                continue
            url = via.get("url", "")
            advisory = url.rsplit("/", 1)[-1] if url else f"npm-{via.get('source')}"
            # npm repeats an advisory once per affected range of the package.
            if (name, advisory) in seen:
                continue
            seen.add((name, advisory))
            versions = sorted(
                {
                    entry.get("version", "?")
                    for key, entry in lock.get("packages", {}).items()
                    if key == f"node_modules/{name}" or key.endswith(f"/node_modules/{name}")
                }
            )
            findings.append(
                Finding(
                    ecosystem="npm",
                    project=project.path,
                    package=name,
                    version=",".join(versions) or "?",
                    advisory=advisory,
                    severity=(via.get("severity") or "unknown").lower(),
                    title=via.get("title", ""),
                    shipped=name in shipped,
                )
            )
    return findings


def http_json(url: str, body: dict | None = None) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        url, data=data, headers={"content-type": "application/json", "user-agent": "lightweight-check-advisories"}
    )
    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return json.load(response)
        except (urllib.error.URLError, TimeoutError) as error:
            if attempt == 3:
                raise RuntimeError(f"{url}: {error}") from error
            time.sleep(2**attempt * 2)
    raise AssertionError("unreachable")


def cvss3_rating(vector: str) -> str:
    """The qualitative rating of a CVSS 3.x base vector (FIRST's formula)."""
    m = dict(part.split(":") for part in vector.split("/")[1:])
    scope_changed = m["S"] == "C"
    av = {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2}[m["AV"]]
    ac = {"L": 0.77, "H": 0.44}[m["AC"]]
    pr = {"N": 0.85, "L": 0.68 if scope_changed else 0.62, "H": 0.5 if scope_changed else 0.27}[m["PR"]]
    ui = {"N": 0.85, "R": 0.62}[m["UI"]]
    c, i, a = ({"H": 0.56, "L": 0.22, "N": 0}[m[k]] for k in ("C", "I", "A"))
    iss = 1 - (1 - c) * (1 - i) * (1 - a)
    impact = 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02) ** 15 if scope_changed else 6.42 * iss
    exploitability = 8.22 * av * ac * pr * ui
    if impact <= 0:
        return "info"
    raw = (1.08 if scope_changed else 1) * (impact + exploitability)
    score = math.ceil(min(raw, 10) * 10 - 1e-9) / 10
    return "critical" if score >= 9 else "high" if score >= 7 else "moderate" if score >= 4 else "low"


def osv_severity(record: dict) -> str:
    label = (record.get("database_specific") or {}).get("severity")
    if label:
        return label.lower()
    for alias in record.get("aliases", []):
        if alias.startswith("GHSA-"):
            label = (http_json(f"https://api.osv.dev/v1/vulns/{alias}").get("database_specific") or {}).get("severity")
            if label:
                return label.lower()
    for severity in record.get("severity", []):
        if severity.get("type") == "CVSS_V3":
            return cvss3_rating(severity["score"])
    return "unknown"


def cargo_packages(*args: str) -> set[tuple[str, str]]:
    out = subprocess.run(
        ["cargo", "tree", "--locked", "--prefix", "none", "--format", "{p}", *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    packages = set()
    for line in out.splitlines():
        parts = line.split()
        # A workspace crate prints its path after the version; it is ours, not
        # a dependency, and no advisory database lists it.
        local = any(p.startswith("(") and p not in ("(*)", "(proc-macro)") for p in parts[2:])
        if len(parts) >= 2 and parts[1].startswith("v") and not local:
            packages.add((parts[0], parts[1][1:]))
    return packages


def audit_cargo() -> list[Finding]:
    every = cargo_packages("--workspace", "--target", "all", "--edges", "all")
    shipped = cargo_packages("-p", SHIPPED_CRATE, "--target", "all", "--edges", "normal")
    crates = sorted(every)
    batch = http_json(
        "https://api.osv.dev/v1/querybatch",
        {"queries": [{"package": {"name": n, "ecosystem": "crates.io"}, "version": v} for n, v in crates]},
    )
    findings = []
    for (name, version), result in zip(crates, batch.get("results", [])):
        # One advisory is often listed under two ids (RustSec and GitHub's);
        # report it once, under the id that came first.
        seen: set[str] = set()
        for hit in result.get("vulns", []) or []:
            if hit["id"] in seen:
                continue
            record = http_json(f"https://api.osv.dev/v1/vulns/{hit['id']}")
            seen |= {record["id"], *record.get("aliases", [])}
            if record.get("withdrawn"):
                continue
            # RustSec's "unmaintained" and "unsound" notices are advice, not
            # vulnerabilities; they are listed, but as information.
            informational = (record.get("database_specific") or {}).get("informational")
            findings.append(
                Finding(
                    ecosystem="crates.io",
                    project="Cargo.lock",
                    package=name,
                    version=version,
                    advisory=record["id"],
                    severity="info" if informational else osv_severity(record),
                    title=(f"[{informational}] " if informational else "")
                    + record.get("summary", "")
                    + "".join(f" (also {a})" for a in record.get("aliases", []) if a.startswith(("GHSA-", "RUSTSEC-"))),
                    shipped=(name, version) in shipped,
                )
            )
    return findings


def load_exceptions(today: dt.date) -> tuple[list[dict], list[str]]:
    if not EXCEPTIONS.exists():
        return [], []
    entries = json.loads(EXCEPTIONS.read_text()).get("exceptions", [])
    valid, problems = [], []
    required = ("advisory", "package", "reason", "approved_by", "approved_on", "expires")
    for entry in entries:
        missing = [key for key in required if not str(entry.get(key, "")).strip()]
        if missing:
            problems.append(f"exception {entry.get('advisory', '?')} is missing {', '.join(missing)}")
            continue
        try:
            expires = dt.date.fromisoformat(entry["expires"])
            dt.date.fromisoformat(entry["approved_on"])
        except ValueError:
            problems.append(f"exception {entry['advisory']} has a date that is not YYYY-MM-DD")
            continue
        if expires < today:
            problems.append(f"exception {entry['advisory']} ({entry['package']}) expired on {expires}")
            continue
        valid.append(entry)
    return valid, problems


def apply_exceptions(findings: list[Finding], exceptions: list[dict]) -> list[str]:
    used = set()
    for finding in findings:
        for index, entry in enumerate(exceptions):
            if entry["advisory"] == finding.advisory and entry["package"] == finding.package:
                finding.excepted_by = entry
                used.add(index)
    return [
        f"exception {e['advisory']} ({e['package']}) matches nothing; remove it"
        for i, e in enumerate(exceptions)
        if i not in used
    ]


def line(finding: Finding) -> str:
    return (
        f"{finding.severity:<8} {finding.package} {finding.version}  {finding.advisory}  "
        f"[{finding.project}]  {finding.title}"
    )


def report(findings: list[Finding], problems: list[str], notes: list[str], summary: bool = True) -> int:
    rank = {name: i for i, name in enumerate(ORDER)}
    findings.sort(key=lambda f: (rank.get(f.severity, len(ORDER)), f.ecosystem, f.package, f.advisory))
    shipped = [f for f in findings if f.shipped]
    built = [f for f in findings if not f.shipped]
    failing = [f for f in shipped if f.severity in FAILING and not f.excepted_by]

    print("== shipped: what reaches a user ==")
    if not shipped:
        print("  ok    no known advisory")
    for f in shipped:
        if f.excepted_by:
            tag = "note  excepted: "
            suffix = f"  (approved by {f.excepted_by['approved_by']} until {f.excepted_by['expires']}: {f.excepted_by['reason']})"
        else:
            tag = "FAIL  " if f.severity in FAILING else "note  "
            suffix = ""
        print(f"  {tag}{line(f)}{suffix}")

    print("== build and test only: never shipped, reported, not a failure ==")
    if not built:
        print("  ok    no known advisory")
    for f in built:
        print(f"  note  {line(f)}")

    for note in notes:
        print(f"  note  {note}")
    for problem in problems:
        print(f"  FAIL  {problem}")

    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary and summary_path:
        with open(summary_path, "a", encoding="utf-8") as summary:
            summary.write("### Dependency advisories\n\n| scope | severity | package | version | advisory | where |\n|---|---|---|---|---|---|\n")
            for f in shipped + built:
                scope = ("shipped" + (" (excepted)" if f.excepted_by else "")) if f.shipped else "build/test"
                summary.write(
                    f"| {scope} | {f.severity} | {f.package} | {f.version} | {f.advisory} | {f.project} |\n"
                )
            summary.write(f"\n{len(failing)} failing, {len(shipped)} shipped, {len(built)} build/test-only.\n")

    print()
    counts = ", ".join(f"{n} {s}" for s in ORDER if (n := sum(f.severity == s for f in built)))
    print(f"shipped: {len(shipped)} advisories, {len(failing)} failing; build/test only: {counts or 'none'}")
    if failing or problems:
        print("A shipped dependency has a known high or critical advisory. Upgrade it, or record an approved,")
        print("dated exception in scripts/advisory-exceptions.json.")
        return 1
    print("No known high or critical advisory in anything shipped.")
    return 0


def check(projects: list[tuple[NpmProject, Path]], cargo: bool) -> int:
    findings: list[Finding] = []
    try:
        for project, directory in projects:
            findings += audit_npm(project, directory)
        if cargo:
            findings += audit_cargo()
    except (RuntimeError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"  FAIL  could not check advisories: {error}")
        return 1
    exceptions, problems = load_exceptions(dt.date.today())
    notes = apply_exceptions(findings, exceptions)
    return report(findings, problems, notes)


# The lockfile entry the v0.8.0 candidate shipped before this check existed:
# Electron 43.4.1, affected by GHSA-qmv3-fv6v-rmhq (fixed in 43.5.0).
VULNERABLE_ELECTRON = {
    "version": "43.4.1",
    "resolved": "https://registry.npmjs.org/electron/-/electron-43.4.1.tgz",
    "integrity": "sha512-5b+EuiwkgG5iRcsEL34rimgRpkYp15SsfZOa0pC5kXs0Tb82TH4n95rpQzTZa7yRCbA7tm0WoEbuBL6NaAhAcA==",
}


def self_test() -> int:
    """A check that cannot fail proves nothing: put Electron 43.4.1 back into a
    copy of the desktop lockfile and require a failure that names it."""
    desktop = next(p for p in NPM_PROJECTS if p.path == "apps/desktop")
    with tempfile.TemporaryDirectory() as scratch:
        work = Path(scratch)
        package = json.loads((ROOT / "apps/desktop/package.json").read_text())
        lock = json.loads((ROOT / "apps/desktop/package-lock.json").read_text())
        package["devDependencies"]["electron"] = "^43.4.1"
        lock["packages"][""]["devDependencies"]["electron"] = "^43.4.1"
        lock["packages"]["node_modules/electron"].update(VULNERABLE_ELECTRON)
        (work / "package.json").write_text(json.dumps(package, indent=2))
        (work / "package-lock.json").write_text(json.dumps(lock, indent=2))

        findings = audit_npm(desktop, work)
    hit = [f for f in findings if f.package == "electron" and f.advisory == "GHSA-qmv3-fv6v-rmhq"]
    failures = 0
    if hit and hit[0].shipped and hit[0].severity in FAILING:
        print(f"  ok    Electron 43.4.1 is reported as shipped and failing: {line(hit[0])}")
    else:
        print("  FAIL  Electron 43.4.1 was not reported as a shipped high advisory")
        failures += 1
    if all(not f.shipped for f in findings if f.package != "electron"):
        print("  ok    the packager's own advisories are build-only, not shipped")
    else:
        print("  FAIL  a build-tool advisory was classified as shipped")
        failures += 1
    # Not written to the step summary: this lockfile is a fixture, not the product.
    if run_status := report(findings, [], [], summary=False):
        print(f"  ok    the check exits {run_status} on it")
    else:
        print("  FAIL  the check passed a lockfile with Electron 43.4.1")
        failures += 1
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--self-test", action="store_true", help="prove the check rejects Electron 43.4.1")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    return check([(p, ROOT / p.path) for p in NPM_PROJECTS], cargo=True)


if __name__ == "__main__":
    sys.exit(main())
