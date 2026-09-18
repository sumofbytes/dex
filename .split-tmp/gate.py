#!/usr/bin/env python3
"""Gate test-only re-exports: gate.py <file> <child> <keep...> ."""
import re, sys
p, child, keep = sys.argv[1], sys.argv[2], set(sys.argv[3:])
lines = open(p).read().split("\n")
for i, l in enumerate(lines):
    m = re.fullmatch(rf"pub\(crate\) use {child}::\{{(.*)\}};", l)
    if m:
        names = sorted(x.strip() for x in m.group(1).split(","))
        gated = sorted(set(names) - keep)
        assert gated, f"nothing to gate in {l}"
        assert set(names) >= keep, f"keep names missing: {keep - set(names)}"
        lines[i] = f"pub(crate) use {child}::{{{', '.join(sorted(keep))}}};"
        lines.insert(i + 1, "#[cfg(test)]")
        lines.insert(i + 2, f"pub(crate) use {child}::{{{', '.join(gated)}}};"
        )
        break
else:
    sys.exit(f"no re-export line for {child}")
open(p, "w").write("\n".join(lines))
print(f"gated {child}: keep={sorted(keep)}")
