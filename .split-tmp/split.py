#!/usr/bin/env python3
"""Mechanical module splitter for the dex structure refactor (scratch)."""
import json
import re
import sys

KEYWORDS = set(
    """as break const continue crate else enum extern false fn for if impl in let
    loop match mod move mut pub ref return self Self static struct super trait true
    type unsafe use where while async await dyn abstract become box do final macro
    override priv typeof unsized virtual yield try union""".split()
)
SKIP_IDENTS = set(
    """vec format println print eprintln panic assert assert_eq assert_ne unreachable
    todo unimplemented dbg write writeln concat env file line column stringify include
    include_str matches cfg option_env""".split()
)

STR_RE = re.compile(r'"(?:[^"\\]|\\.)*"')
FMT_IDENT_RE = re.compile(r"\{([A-Za-z_][A-Za-z0-9_]*)[^}]*\}")

def code_text(ln):
    """Line with // comments removed and string bodies reduced to {idents}."""
    # cut // comment outside of strings
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
    no_comment = "".join(out)
    def keep_fmt(m):
        inner = m.group(0)
        keep = " ".join(FMT_IDENT_RE.findall(inner))
        return '"%s"' % keep
    return STR_RE.sub(keep_fmt, no_comment)

DEF_RE = re.compile(
    r"^(pub(?:\s*\([^)]*\))?\s+)?(?:async\s+|unsafe\s+)?(struct|enum|union|fn|const|static|type|trait|mod)\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)"
)
IDENT_RE = re.compile(r"(?:r#)?([A-Za-z_][A-Za-z0-9_]*)")
BLOCK_OPEN_RE = re.compile(r"^(?:impl\b|mod\b|trait\b|enum\b|struct\b|union\b).*?\{\s*$")


def fail(msg):
    print(f"split.py FATAL: {msg}", file=sys.stderr)
    sys.exit(1)


def expand_use(path):
    path = path.strip()
    if "{" not in path:
        if " as " in path:
            path, alias = [p.strip() for p in path.rsplit(" as ", 1)]
            return [(path, alias)]
        return [(path, path.split("::")[-1])]
    pre, rest = path.split("{", 1)
    depth = 1
    for i, ch in enumerate(rest):
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                inner, post = rest[:i], rest[i + 1 :]
                break
    else:
        fail(f"unbalanced braces in use: {path}")
    out = []
    for part in inner.split(","):
        part = part.strip()
        if not part:
            continue
        if part == "self":
            # `use a::b::{self}` imports name `b` for path `a::b`.
            parent_path = (pre + post).rstrip(":")
            out.append((parent_path, parent_path.split("::")[-1]))
            continue
        for p, n in expand_use(part):
            out.append((pre + p + post, n))
    return out


def parse_drops(lines, drops):
    srcs = []
    for s, e in drops:
        buf, attrs = "", []
        for ln in lines[s : e + 1]:
            t = ln.strip()
            if t.startswith("#["):
                attrs.append(t)
                continue
            buf += " " + t
            if t.endswith(";"):
                m = re.match(r"\s*use\s+(.+);", buf)
                if not m:
                    fail(f"cannot parse use statement: {buf}")
                gated = any("cfg(test)" in a for a in attrs)
                for p, n in expand_use(m.group(1)):
                    srcs.append((p, n, gated))
                buf, attrs = "", []
        if buf.strip():
            fail(f"dangling use statement in drops: {buf}")
    return srcs


def top_defs(lines, s, e):
    """Top-level (col-0, outside impl/mod blocks) defs in [s,e]: {name: (line, vis)}."""
    defs = {}
    in_block = 0
    for i in range(s, e + 1):
        ln = lines[i]
        if in_block > 0:
            in_block += ln.count("{") - ln.count("}")
            continue
        if BLOCK_OPEN_RE.match(ln):
            m0 = DEF_RE.match(ln)
            if m0 and m0.group(2) in ("struct", "enum", "union"):
                defs[m0.group(3)] = (i, (m0.group(1) or "").strip())
            in_block = ln.count("{") - ln.count("}")
            continue
        m = DEF_RE.match(ln)
        if m and m.group(2) != "mod":
            defs[m.group(3)] = (i, (m.group(1) or "").strip())
    return defs


DOT_IDENT_RE = re.compile(r"\.\s*(?:r#)?[A-Za-z_][A-Za-z0-9_]*")

def region_idents(lines, ranges):
    """Idents used in ranges minus col-0 names defined in those same ranges."""
    used, defined = set(), set()
    for s, e in ranges:
        for ln in lines[s : e + 1]:
            ln = DOT_IDENT_RE.sub(".", code_text(ln))
            m = DEF_RE.match(ln)
            if m:
                defined.add(m.group(3))
            for im in IDENT_RE.finditer(ln):
                used.add(im.group(1))
    return used - defined - KEYWORDS - SKIP_IDENTS


def main():
    spec = json.load(open(sys.argv[1]))
    path = spec["file"]
    raw = open(path).read().split("\n")
    if raw and raw[-1] == "":
        raw.pop()
    n = len(raw)
    lines = [""] + raw

    origin_prefix = spec.get("origin_prefix", "super")
    parent_prefix = spec.get("parent_prefix", "super::super")
    origin_file = spec.get("origin_file", path.split("/")[-1].replace(".rs", ""))
    decl = spec.get("decl", "mod")

    covered = [0] * (n + 1)
    for key in ("drops", "head", "keep"):
        for s, e in spec.get(key, []):
            for i in range(s, e + 1):
                covered[i] += 1
    child_ranges = {}
    for ch in spec["children"]:
        cr = []
        for s, e in ch["ranges"] + ch.get("tests", []):
            cr.append((s, e))
            for i in range(s, e + 1):
                covered[i] += 1
        child_ranges[ch["name"]] = cr
    stay_ranges = [st["range"] for st in spec.get("stay", [])]
    for s, e in stay_ranges:
        for i in range(s, e + 1):
            covered[i] += 1
    bad = [i for i in range(1, n + 1) if covered[i] != 1]
    if bad:
        fail(f"{path}: lines not covered exactly once: {bad[:30]}")

    # Top-level defs file-wide (col-0 scan).
    file_defs = top_defs(lines, 1, n)
    home_of = {}
    for ch in spec["children"]:
        for s, e in ch["ranges"] + ch.get("tests", []):
            for nm, (ln, vis) in top_defs(lines, s, e).items():
                if nm in home_of:
                    fail(f"duplicate top-level def {nm}")
                home_of[nm] = ch["name"]

    # Method defs per home (for cross-module method calls).
    method_home = {}
    for ch in spec["children"]:
        for s, e in ch["ranges"]:
            for i in range(s, e + 1):
                m = re.match(
                    r"^\s+(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+|unsafe\s+)?fn\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)",
                    lines[i],
                )
                if m:
                    method_home.setdefault(m.group(1), ch["name"])
    method_callers = {}
    line_home = {}
    for ch in spec["children"]:
        for s, e in ch["ranges"] + ch.get("tests", []):
            for i in range(s, e + 1):
                line_home[i] = ch["name"]
    for s, e in stay_ranges:
        for i in range(s, e + 1):
            line_home[i] = None
    for i in range(1, n + 1):
        for m in re.finditer(r"\.([A-Za-z_][A-Za-z0-9_]*)\s*\(", code_text(lines[i])):
            method_callers.setdefault(m.group(1), set()).add(line_home.get(i))
    method_upgrades = set()
    for mn, callers in method_callers.items():
        home = method_home.get(mn)
        if home is None:
            continue
        if any(c != home for c in callers):
            method_upgrades.add((home, mn))

    # Cross-use: private def referenced outside its home -> pub(crate).
    region_of = {}
    for ch in spec["children"]:
        for s, e in ch["ranges"] + ch.get("tests", []):
            for i in range(s, e + 1):
                region_of[i] = ch["name"]
    for s, e in stay_ranges:
        for i in range(s, e + 1):
            region_of[i] = None
    used_where = {}
    for i in range(1, n + 1):
        if covered[i] == 0:
            continue
        for im in IDENT_RE.finditer(DOT_IDENT_RE.sub(".", code_text(lines[i]))):
            used_where.setdefault(im.group(1), set()).add(region_of.get(i))
    upgraded = set()
    for nm, (ln, vis) in file_defs.items():
        if vis in ("pub", "pub(crate)"):
            continue
        home = home_of.get(nm)
        users = used_where.get(nm, set()) - {home, "SKIP"}
        users.discard(None if home is None else "PARENT_NEVER")
        if home is None:
            users = used_where.get(nm, set()) - {None}
        if users:
            upgraded.add(nm)

    srcs = parse_drops(lines, spec.get("drops", []))
    import_of = {}
    for p, nm, gated in srcs:
        import_of.setdefault(nm, (p, gated))

    siblings = set(spec.get("siblings", []))

    def absolutize_for_child(raw_path):
        segs = raw_path.split("::")
        if segs[0] == "super":
            return parent_prefix + "::" + "::".join(segs[1:])
        if segs[0] in siblings:
            return origin_prefix + "::" + raw_path
        return raw_path

    super_re = re.compile(r"\bsuper::(?!super\b)([A-Za-z_][A-Za-z0-9_]*)")

    def rewrite_super(text, in_tests):
        def rep(m):
            nm = m.group(1)
            base = origin_prefix if nm in file_defs else parent_prefix
            if in_tests:
                base = "super::" + base
            return base + "::" + nm
        return super_re.sub(rep, text)

    outputs = {}
    for ch in spec["children"]:
        name = ch["name"]
        ranges_test = [(s, e, False) for s, e in ch["ranges"]] + [
            (s, e, True) for s, e in ch.get("tests", [])
        ]
        used_all, used_nontest = set(), set()
        for s, e, t in ranges_test:
            ids = region_idents(lines, [(s, e)])
            used_all |= ids
            if not t:
                used_nontest |= ids
        imports, gated_imports = [], []
        for nm in sorted(used_all):
            if nm in import_of:
                p, gated = import_of[nm]
                (gated_imports if (gated or nm not in used_nontest) else imports).append(
                    absolutize_for_child(p)
                )
            elif nm in file_defs and home_of.get(nm) != name:
                if nm in home_of:
                    tgt = "super::" + home_of[nm] + "::" + nm
                else:
                    tgt = origin_prefix + "::" + nm
                (gated_imports if nm not in used_nontest else imports).append(tgt)
        body = []
        for imp in sorted(set(imports)):
            body.append(f"use {imp};")
        for imp in sorted(set(gated_imports)):
            body.append(f"#[cfg(test)]\nuse {imp};")
        if body:
            body.append("")
        for s, e, t in sorted(ranges_test):
            chunk = "\n".join(lines[s : e + 1])
            if t and spec.get("grandparent"):
                chunk = re.sub(
                    r"\bsuper::super::([A-Za-z_][A-Za-z0-9_]*)",
                    "crate::" + spec["grandparent"] + r"::\1",
                    chunk,
                )
            chunk = rewrite_super(chunk, t)
            body.append(chunk)
        text = "\n".join(body).rstrip("\n") + "\n"
        # apply visibility upgrades to defs owned by this child
        out = []
        for ln in text.split("\n"):
            m = DEF_RE.match(ln)
            if m and m.group(3) in upgraded and home_of.get(m.group(3)) == name:
                ln = "pub(crate) " + ln.lstrip()
                out.append(ln)
                continue
            m2 = re.match(
                r"^(\s+)(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+|unsafe\s+)?fn\s+((?:r#)?[A-Za-z_][A-Za-z0-9_]*)",
                ln,
            )
            if m2 and (name, m2.group(2)) in method_upgrades and "pub" not in ln.split("fn")[0]:
                ln = m2.group(1) + "pub(crate) " + ln.lstrip()
            out.append(ln)
        outputs[name] = "\n".join(out)

    parent = []
    for s, e in spec.get("head", []):
        parent.extend(lines[s : e + 1])
    stay_nontest = [st["range"] for st in spec.get("stay", []) if not st.get("test")]
    stay_test = [st["range"] for st in spec.get("stay", []) if st.get("test")]
    used_nt = region_idents(lines, stay_nontest)
    used_t = region_idents(lines, stay_test)
    kept_names = set()
    for s, e in spec.get("keep", []):
        for i in range(s, e + 1):
            if re.match(r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+", lines[i]):
                m2 = re.match(r"\s*use\s+(.+);", lines[i])
                if m2:
                    for _, nm in expand_use(m2.group(1)):
                        kept_names.add(nm)
    p_imports, p_gated = [], []
    for nm in sorted((used_nt | used_t)):
        if nm in kept_names or nm in file_defs:
            continue
        if nm in import_of:
            p, gated = import_of[nm]
            if gated or nm not in used_nt:
                p_gated.append(p)
            else:
                p_imports.append(p)
    for imp in sorted(set(p_imports)):
        parent.append(f"use {imp};")
    for imp in sorted(set(p_gated)):
        parent.append(f"#[cfg(test)]\nuse {imp};")
    if p_imports or p_gated:
        parent.append("")
    for s, e in spec.get("keep", []):
        parent.extend(lines[s : e + 1])
    for ch in spec["children"]:
        parent.append(f"{decl} {ch['name']};")
    for ch in spec["children"]:
        names, pubs = [], []
        for s, e in ch["ranges"]:
            for nm, (ln, vis) in top_defs(lines, s, e).items():
                if vis == "pub" or nm in upgraded:
                    if vis == "pub":
                        pubs.append(nm)
                    else:
                        names.append(nm)
                elif vis == "pub(crate)":
                    names.append(nm)
        if pubs:
            parent.append(f"pub use {ch['name']}::{{{', '.join(pubs)}}};")
        if names:
            parent.append(f"pub(crate) use {ch['name']}::{{{', '.join(names)}}};")
    for st in spec.get("stay", []):
        parent.append("")
        s, e = st["range"]
        parent.extend(lines[s : e + 1])
    parent_text = "\n".join(parent).rstrip("\n") + "\n"
    # upgrade parent-resident private defs used by children
    need_pub = {nm for nm in upgraded if nm not in home_of}
    if need_pub:
        new_parent = []
        for ln in parent_text.split("\n"):
            m = DEF_RE.match(ln)
            if m and m.group(3) in need_pub and not m.group(1):
                ln = "pub(crate) " + ln.lstrip()
            new_parent.append(ln)
        parent_text = "\n".join(new_parent)

    # --- struct field upgrade -------------------------------------------
    # A struct/union named outside its home module: its fields must be
    # visible there too (construction / field access is module-scoped).
    struct_home = {}
    for nm, ln in file_defs.items():
        pass
    for ch in spec["children"]:
        for s, e in ch["ranges"]:
            for nm, (ln, vis) in top_defs(lines, s, e).items():
                pass
    struct_ranges = {}
    for i in range(1, n + 1):
        m = re.match(
            r"^(?:pub(?:\s*\([^)]*\))?\s+)?(struct|union)\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)",
            lines[i],
        )
        if m and i in range(1, n + 1):
            struct_ranges[m.group(2)] = i
    field_upgrade_structs = set()
    for nm, start in struct_ranges.items():
        home = home_of.get(nm)
        users = used_where.get(nm, set())
        if home is None:
            ext = any(u is not None for u in users)
        else:
            ext = any(u != home for u in users)
        if ext:
            field_upgrade_structs.add(nm)

    def upgrade_fields(text):
        out_lines, cur, upgrading = [], None, False
        for ln in text.split("\n"):
            m = re.match(
                r"^(?:pub(?:\s*\([^)]*\))?\s+)?(struct|union)\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)",
                ln,
            )
            if m and m.group(2) in field_upgrade_structs and "{" in ln and ";" not in ln:
                cur, upgrading = m.group(2), True
                out_lines.append(ln)
                continue
            if upgrading:
                if re.match(r"^\}", ln):
                    cur, upgrading = None, False
                    out_lines.append(ln)
                    continue
                fm = re.match(r"^(\s+)(#[^\n]*\s+)?([A-Za-z_][A-Za-z0-9_]*\s*:)", ln)
                if fm and not ln.strip().startswith("pub"):
                    ln = fm.group(1) + "pub(crate) " + ln.lstrip()
                out_lines.append(ln)
                continue
            out_lines.append(ln)
        return "\n".join(out_lines)

    for ch in spec["children"]:
        outputs[ch["name"]] = upgrade_fields(outputs[ch["name"]])

    return path, parent_text, outputs


if __name__ == "__main__":
    import os
    import subprocess

    path, parent_text, outputs = main()
    stem = path.split("/")[-1].replace(".rs", "")
    d = "/".join(path.split("/")[:-1]) + "/" + stem
    os.makedirs(d, exist_ok=True)
    with open(f"{d}/mod.rs", "w") as f:
        f.write(parent_text)
    subprocess.run(["rm", path], check=True)
    for name, text in outputs.items():
        with open(f"{d}/{name}.rs", "w") as f:
            f.write(text)
    print(f"split {path} -> {d}/mod.rs + {sorted(outputs)}")
