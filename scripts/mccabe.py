#!/usr/bin/env python3
"""Approximate McCabe cyclomatic complexity for Rust via token scan.

Used to rank functions by complexity.
Counts if/while/for/loop/match + match arms (`=>`) + `&&`/`||`/`?` per fn body.
Inflates absolute numbers slightly (every `=>` is a point) — use for ranking,
and cross-check with `cargo clippy -- -W clippy::cognitive_complexity`.

    python3 scripts/mccabe.py src            # top 60
    TOP=80 python3 scripts/mccabe.py src     # top 80
"""
import sys, os, re, statistics

DECISION_WORDS = {"if", "while", "for", "loop", "match"}
BINOPS = ("&&", "||")

def strip(src):
    out = []; i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c == '/' and i + 1 < n and src[i+1] == '*':
            depth = 1; i += 2
            while i < n and depth:
                if src[i] == '/' and i+1 < n and src[i+1] == '*': depth += 1; i += 2
                elif src[i] == '*' and i+1 < n and src[i+1] == '/': depth -= 1; i += 2
                else: i += 1
            out.append(' '); continue
        if c == '/' and i + 1 < n and src[i+1] == '/':
            while i < n and src[i] != '\n': i += 1
            continue
        m = re.match(r'b?r(#*)"', src[i:])
        if m:
            close = '"' + m.group(1)
            j = src.find(close, i + len(m.group(0)))
            j = n if j < 0 else j + len(close)
            out.append('""'); i = j; continue
        if c == '"' or (c == 'b' and i+1 < n and src[i+1] == '"'):
            if c == 'b': i += 1
            i += 1
            while i < n and src[i] != '"':
                if src[i] == '\\': i += 2; continue
                i += 1
            i += 1; out.append('""'); continue
        if c == "'":
            lm = re.match(r"'[A-Za-z_][A-Za-z0-9_]*", src[i:])
            if lm and not (i+len(lm.group(0)) < n and src[i+len(lm.group(0))] == "'"):
                out.append("'" + lm.group(0)[1:]); i += len(lm.group(0)); continue
            j = i + 1
            if j < n and src[j] == '\\': j += 2
            else: j += 1
            if j < n and src[j] == "'": j += 1
            out.append("'c'"); i = j; continue
        out.append(c); i += 1
    return ''.join(out)

TOK = re.compile(r'&&|\|\||=>|[?]|[A-Za-z_][A-Za-z0-9_]*|[{}();,.]')
TOKWS = {"fn"}

def analyze(path):
    raw = open(path, encoding='utf-8', errors='replace').read()
    src = strip(raw)
    toks = TOK.findall(src)
    # offset -> line map via finditer on src is expensive; approximate by
    # recomputing positions only for fn starts.
    res = []; i = 0; N = len(toks)
    # build token offset list
    offs = [(m.start(), m.group(0)) for m in TOK.finditer(src)]
    def line_of(off):
        return src.count('\n', 0, off) + 1
    while i < N:
        if toks[i] == 'fn' and i+1 < N:
            name = toks[i+1]; j = i + 2
            while j < N and toks[j] != '{': j += 1
            if j >= N: break
            depth = 0; k = j
            while k < N:
                if toks[k] == '{': depth += 1
                elif toks[k] == '}':
                    depth -= 1
                    if depth == 0: break
                k += 1
            body = toks[j:k+1]; fc = 1
            for t in body:
                if t in DECISION_WORDS or t in BINOPS or t == '?' or t == '=>':
                    fc += 1
            line = line_of(offs[i][0])
            endline = line_of(offs[min(k, len(offs)-1)][0])
            res.append((fc, line, endline, name))
            i = k + 1
        else:
            i += 1
    return res

def gather(roots):
    files = []
    for root in roots:
        if os.path.isdir(root):
            for d, _, fs in os.walk(root):
                for f in fs:
                    if f.endswith('.rs'): files.append(os.path.join(d, f))
        else: files.append(root)
    out = []
    for f in files:
        try:
            for fc, l, e, name in analyze(f):
                out.append((fc, e - l + 1, f, l, name))
        except Exception as ex:
            print('ERR', f, ex, file=sys.stderr)
    return out

def main():
    allf = gather(sys.argv[1:])
    allf.sort(reverse=True)
    vals = [x[0] for x in allf]
    print(f'# functions: {len(allf)}')
    print(f'# median CC {statistics.median(vals)}  mean {statistics.mean(vals):.1f}  p90 {sorted(vals)[int(len(vals)*0.9)]}  max {max(vals)}')
    for fc, ln, f, l, name in allf[:int(os.environ.get('TOP', '60'))]:
        print(f'{fc:4d}  {ln:4d}L  {f}:{l}  {name}')

if __name__ == '__main__':
    main()
