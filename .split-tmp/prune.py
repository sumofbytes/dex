import re, subprocess, collections
p = "src/llm/config/mod.rs"
out = subprocess.run(["cargo","check","--all-targets"], capture_output=True, text=True)
txt = out.stdout + out.stderr
targets = collections.defaultdict(set)  # line -> names
cur = None
for ln in txt.split("\n"):
    m = re.match(r"warning: unused import[s]?: (.*)", ln)
    if m:
        cur = re.findall(r"`([^`]+)`", m.group(1)); continue
    m2 = re.match(r"\s*-->\s*([^:]+):(\d+):\d+", ln)
    if m2 and cur:
        if m2.group(1).endswith("llm/config/mod.rs"):
            targets[int(m2.group(2))] |= set(cur)
        cur = None
if not targets:
    print("nothing to prune"); raise SystemExit
lines = open(p).read().split("\n")
for no in sorted(targets, reverse=True):
    i = no - 1
    line = lines[i]
    if not re.match(r"^pub\(crate\) use ", line):
        print(f"skip non-use {no}: {line[:60]}"); continue
    names = targets[no]
    for nm in names:
        # remove `nm` or `nm, ` occurrences from the brace list
        line2 = re.sub(r"(?<![A-Za-z0-9_])" + re.escape(nm) + r",\s*", "", line)
        if line2 == line:
            line2 = re.sub(r",\s*" + re.escape(nm) + r"(?![A-Za-z0-9_])", "", line)
        line = line2
    if re.search(r"use \w+::\{\s*\};", line) or re.search(r"use \w+::\{\};", line):
        print(f"drop line {no}")
        del lines[i]
    else:
        if line != lines[i]:
            lines[i] = line
            print(f"pruned {no}: removed {sorted(names)}")
        else:
            print(f"NO-OP {no}: {sorted(names)} — manual: {line[:80]}")
open(p, "w").write("\n".join(lines))
