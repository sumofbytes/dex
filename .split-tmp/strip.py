#!/usr/bin/env python3
"""Prune unused imports in touched files; repeat until clean."""
import re
import subprocess
import sys

import os

touched = sys.argv[1:]

STR_RE = re.compile(r'"(?:[^"\\]|\\.)*"')
FMT_IDENT_RE = re.compile(r"\{([A-Za-z_][A-Za-z0-9_]*)[^}]*\}")

def code_text(ln):
    out, i, n = [], 0, len(ln)
    in_str, esc = False, False
    while i < n:
        ch = ln[i]
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
            out.append(ch)
        else:
            if ch == '"':
                in_str = True
                out.append(ch)
            elif ch == "/" and i + 1 < n and ln[i + 1] == "/":
                break
            else:
                out.append(ch)
        i += 1
    def keep_fmt(m):
        return '"%s"' % " ".join(FMT_IDENT_RE.findall(m.group(0)))
    return STR_RE.sub(keep_fmt, "".join(out))

def name_used_in_code(fp, name):
    pat = re.compile(r"(?<!::)\b" + re.escape(name) + r"\b")
    with open(fp) as f:
        for ln in f.read().split("\n"):
            t = ln.strip()
            if re.match(r"^(pub(\(crate\))?\s+)?use\s", t):
                continue
            if pat.search(code_text(ln)):
                return True
    return False

for rnd in range(8):
    p = subprocess.run(["cargo", "check", "--all-targets"],
                       capture_output=True, text=True)
    out = p.stdout + p.stderr
    cur_names, targets = None, {}
    for ln in out.split("\n"):
        m = re.match(r"warning: unused import[s]?: (.*)", ln)
        if m:
            cur_names = re.findall(r"`([^`]+)`", m.group(1))
            continue
        m2 = re.match(r"\s*-->\s*([^:]+):(\d+):\d+", ln)
        if m2 and cur_names:
            fp, line = m2.group(1), int(m2.group(2))
            if any(fp.endswith(t) for t in touched):
                targets.setdefault(fp, []).append((line, cur_names))
            cur_names = None
        elif ln.strip() and not ln.startswith((" ", "|", "=", "help")):
            cur_names = None
    if not targets:
        print(f"round {rnd}: no unused-import warnings left")
        break
    deleted = False
    for fp, items in targets.items():
        with open(fp) as f:
            content = f.read().split("\n")
        for ln_no, names in sorted(items, reverse=True):
            idx = ln_no - 1
            if not (0 <= idx < len(content)):
                continue
            line = content[idx]
            if re.match(r"^\s*use\s+[^;{}]*;\s*$", line):
                mname = re.match(r"^\s*use\s+(.+);\s*$", line).group(1)
                base = mname.split("::")[-1].strip()
                if name_used_in_code(fp, base):
                    print(f"  KEEP (used) {fp}:{ln_no}: {line.strip()}")
                    continue
                print(f"  strip {fp}:{ln_no}: {line.strip()}")
                del content[idx]
                if idx > 0 and content[idx - 1].strip() == "#[cfg(test)]":
                    nxt = content[idx].strip() if idx < len(content) else ""
                    if not re.match(r"^(pub(\(crate\))?\s+)?use\s", nxt):
                        print(f"  strip dangling gate {fp}:{ln_no - 1}")
                        del content[idx - 1]
            elif re.match(r"^\s*pub(?:\(crate\))?\s+use\s+\w+::\{[^}]*\};\s*$", line):
                print(f"  MANUAL re-export prune {fp}:{ln_no}: {line.strip()}")
            else:
                print(f"  SKIP {fp}:{ln_no}: {line.strip()}")
        with open(fp, "w") as f:
            f.write("\n".join(content))
        deleted = True
    if not deleted:
        print("no deletions this round; leftovers need manual review")
        break
else:
    print("strip: max rounds reached")
