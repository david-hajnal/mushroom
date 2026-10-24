#!/usr/bin/env python3
"""Equivalent syntax check for the deployment artifacts in this repo.

`systemd-analyze verify` is not available on the development host (macOS), so
this script provides an equivalent *syntax* check for:

  * systemd unit files (``*.service``) — validates section structure,
    ``Key=Value`` syntax, required keys, and that ``ExecStart`` is an absolute
    path (the things ``systemd-analyze verify`` would flag syntactically).
  * freedesktop ``.desktop`` autostart files — validates the ``[Desktop Entry]``
    section and the required keys (``Type``, ``Name``, ``Exec``).

Usage:
    python3 deploy/verify_unit.py <file> [<file> ...]

With no arguments it checks every ``*.service`` and ``*.desktop`` file under
``deploy/``. Exits 0 if all files pass, 1 otherwise, printing one line per
problem (or a per-file PASS).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# systemd section names we accept in this repo's units.
VALID_UNIT_SECTIONS = {"Unit", "Service", "Install"}
# A systemd directive key: letters, digits, underscore, hyphen.
KEY_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_-]*$")
# Directives that legitimately accept an empty value in systemd
# (e.g. `CapabilityBoundingSet=` clears the capability set).
EMPTY_VALUE_OK = {"CapabilityBoundingSet", "Environment"}
# A .desktop key: letters, digits, underscore (per the freedesktop spec).
DESKTOP_KEY_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_]*$")


def _is_comment_or_blank(line: str) -> bool:
    stripped = line.strip()
    return stripped == "" or stripped.startswith("#") or stripped.startswith(";")


def _logical_lines(text: str) -> list[tuple[int, str]]:
    """Split unit text into logical lines, honoring systemd continuations.

    systemd unit files support line continuation: a line ending in a
    backslash is joined with the following line (whitespace around the join
    is collapsed). Returns ``(starting_lineno, logical_line)`` pairs so
    diagnostics point at the first physical line of the logical line.
    """
    logical: list[tuple[int, str]] = []
    buf = ""
    start = 0
    for lineno, raw in enumerate(text.splitlines(), start=1):
        stripped = raw.strip()
        if buf:
            # Continuation of a previous logical line.
            if stripped.endswith("\\"):
                buf += " " + stripped[:-1].rstrip()
            else:
                buf += " " + stripped
                logical.append((start, buf))
                buf = ""
        else:
            if _is_comment_or_blank(stripped):
                continue
            if stripped.endswith("\\"):
                buf = stripped[:-1].rstrip()
                start = lineno
            else:
                logical.append((lineno, stripped))
    if buf:
        # Dangling continuation at EOF: keep it so the parser can flag it.
        logical.append((start, buf))
    return logical


def _split_key_value(line: str) -> tuple[str, str] | None:
    """Split a ``Key=Value`` line. Returns (key, value) or None if malformed."""
    if "=" not in line:
        return None
    key, _, value = line.partition("=")
    key = key.strip()
    value = value.strip()
    if not key:
        return None
    return key, value


def check_systemd_unit(path: Path) -> list[str]:
    """Return a list of problems (empty if the unit file is syntactically valid)."""
    problems: list[str] = []
    text = path.read_text(encoding="utf-8")

    sections: dict[str, list[tuple[str, str]]] = {}
    current: str | None = None
    seen_sections: set[str] = set()

    for lineno, line in _logical_lines(text):
        if line.startswith("[") and line.endswith("]"):
            name = line[1:-1].strip()
            if not name:
                problems.append(f"{path}:{lineno}: empty section header")
                continue
            if name not in VALID_UNIT_SECTIONS:
                problems.append(
                    f"{path}:{lineno}: unknown section [{name}] "
                    f"(expected one of {sorted(VALID_UNIT_SECTIONS)})"
                )
                continue
            if name in seen_sections:
                problems.append(f"{path}:{lineno}: duplicate section [{name}]")
                continue
            seen_sections.add(name)
            sections.setdefault(name, [])
            current = name
            continue

        if current is None:
            problems.append(f"{path}:{lineno}: directive before any section: {line!r}")
            continue

        kv = _split_key_value(line)
        if kv is None:
            problems.append(f"{path}:{lineno}: not a Key=Value directive: {line!r}")
            continue
        key, value = kv
        if not KEY_RE.match(key):
            problems.append(f"{path}:{lineno}: invalid key {key!r}")
            continue
        if not value:
            # systemd accepts empty values for some directives (e.g.
            # `CapabilityBoundingSet=` clears the capability set, and
            # `Environment=` resets the environment). Allow the ones that are
            # legitimately empty; flag the rest as suspicious.
            if key not in EMPTY_VALUE_OK:
                problems.append(f"{path}:{lineno}: empty value for {key!r}")
                continue
        sections[current].append((key, value))

    # Required structure.
    if "Service" not in sections:
        problems.append(f"{path}: missing [Service] section")
    else:
        keys = {k for k, _ in sections["Service"]}
        if "ExecStart" not in keys:
            problems.append(f"{path}: [Service] missing required key ExecStart")
        else:
            for k, v in sections["Service"]:
                if k == "ExecStart":
                    # The command (first token, after any prefix) must be an
                    # absolute path. Strip systemd exec prefixes (-, @, +, !, :).
                    cmd = v.split()[0] if v.split() else ""
                    cmd = cmd.lstrip("-@+!:")
                    if not cmd.startswith("/"):
                        problems.append(
                            f"{path}: ExecStart command {cmd!r} is not an absolute path"
                        )
                    break

    if "Install" not in sections:
        problems.append(f"{path}: missing [Install] section")
    else:
        keys = {k for k, _ in sections["Install"]}
        if not ({"WantedBy", "RequiredBy", "Also"} & keys):
            problems.append(
                f"{path}: [Install] missing WantedBy/RequiredBy/Also"
            )

    if "Unit" not in sections:
        problems.append(f"{path}: missing [Unit] section")

    return problems


def check_desktop(path: Path) -> list[str]:
    """Return a list of problems (empty if the .desktop file is valid)."""
    problems: list[str] = []
    text = path.read_text(encoding="utf-8")

    entries: dict[str, list[tuple[str, str]]] = {}
    current: str | None = None

    for lineno, raw in enumerate(text.splitlines(), start=1):
        if _is_comment_or_blank(raw):
            continue
        line = raw.strip()
        if line.startswith("[") and line.endswith("]"):
            name = line[1:-1].strip()
            if not name:
                problems.append(f"{path}:{lineno}: empty section header")
                continue
            entries.setdefault(name, [])
            current = name
            continue
        if current is None:
            problems.append(f"{path}:{lineno}: directive before any section: {line!r}")
            continue
        kv = _split_key_value(line)
        if kv is None:
            problems.append(f"{path}:{lineno}: not a Key=Value directive: {line!r}")
            continue
        key, value = kv
        if not DESKTOP_KEY_RE.match(key):
            problems.append(f"{path}:{lineno}: invalid key {key!r}")
            continue
        entries[current].append((key, value))

    if "Desktop Entry" not in entries:
        problems.append(f"{path}: missing [Desktop Entry] section")
        return problems

    keys = {k for k, _ in entries["Desktop Entry"]}
    for required in ("Type", "Name", "Exec"):
        if required not in keys:
            problems.append(f"{path}: [Desktop Entry] missing required key {required}")

    # Exec must name a command (non-empty).
    for k, v in entries["Desktop Entry"]:
        if k == "Exec" and not v.strip():
            problems.append(f"{path}: Exec is empty")

    return problems


def check_file(path: Path) -> list[str]:
    if not path.exists():
        return [f"{path}: file not found"]
    if path.suffix == ".desktop":
        return check_desktop(path)
    # Default to systemd unit validation for .service and anything else.
    return check_systemd_unit(path)


def default_files() -> list[Path]:
    deploy = Path(__file__).resolve().parent
    files = sorted(deploy.glob("*.service")) + sorted(deploy.glob("*.desktop"))
    return files


def main(argv: list[str]) -> int:
    if len(argv) > 1:
        files = [Path(a) for a in argv[1:]]
    else:
        files = default_files()

    if not files:
        print("verify_unit: no files to check", file=sys.stderr)
        return 1

    all_ok = True
    for path in files:
        problems = check_file(path)
        if problems:
            all_ok = False
            print(f"FAIL {path}")
            for p in problems:
                print(f"  - {p}")
        else:
            print(f"PASS {path}")

    if all_ok:
        print(f"verify_unit: {len(files)} file(s) OK")
        return 0
    print("verify_unit: FAILED", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
