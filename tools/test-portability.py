#!/usr/bin/env python3
"""Test code that assumes the Mac, so that only Windows CI would find it (tools/check.sh runs this).

Each check below is a failure Windows CI found in 0.1.15 or 0.1.16. A test must expect what this system shows:
- `tilde`: `~/…` (or `~`) expected where the core writes a home-folder path it made. The core writes `~/…` on Unix
  systems and the full path on Windows (trs_platform::dirs::shown); expect each system's form (the ffi chooser
  tests' `at_home`). A `~/…` the test itself fed in (a rule's or a fixture's folder) comes back as it went in.
- `this-mac`: "This Mac", "Show in Finder", "Move to Trash" expected: `computer!()`, `file_manager!()`, `trash!()`.
- `joined`: `.join("a/b")` turned into a string and compared. Path::join writes the system's separator (`a\\b` on
  Windows): join each name on its own, `.join("a").join("b")`.
- `unix-path`: a `/tmp/a/b` or `/Users/me/x` path (two names or more below a root folder) expected as text, which no
  code of the crate gives as it is: the code made it, and a path it joins has `\\` on Windows. Build the expectation
  with Path::join (or compare Paths, which ignore the separator). Also a link whose path has no drive
  (`?path=/Users/…`): it is not absolute on Windows.
- `unix-only-helper`: a test helper that only `#[cfg(unix)]` (or `not(windows)`) code uses, without that cfg itself:
  dead code on Windows, which fails clippy's `-D warnings` there.
- `home-env` (0.1.18; code that is not a test, the mock included): `$HOME` read directly. GitHub's Windows runner and a
  Windows app have none (a mock's `/Users/me` fallback has no drive there, so it is no absolute path): use
  `trs_platform::dirs::home()`, which reads `%USERPROFILE%` on Windows. The platform crate itself is where that is
  decided.

Allowed: code under `#[cfg(unix)]`, `#[cfg(not(windows))]`, `#[cfg(target_os = "macos")]` and the like,
`#[cfg_attr(windows, ignore)]`, an `if cfg!(unix) { … }` block (or the `else` of `if cfg!(windows)`), any line that
names `cfg!(`, and the statement or item after a `// portable: <why>` comment (or a line that ends in one). Only test
code is read: `tests/` folders, `#[cfg(test)]` items and the files of modules declared under `#[cfg(test)]`.

Usage: tools/test-portability.py [folders or files…] (default: crates). Exit 1 and name each place.
"""
import bisect
import functools
import pathlib
import re
import sys

MARK = "// portable:"
ROOTS = "Users|tmp|private|home|Volumes|var|Applications|Library|opt|srv|mnt"
TOKEN = re.compile(
    r"//[^\n]*"
    r"|/\*"
    r"|(?<![\w])b?r(#*)\""
    r"|(?<![\w])b?\"(?:[^\"\\]|\\.)*\""
    r"|'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'",
    re.S,
)


def strip(src):
    """`src` with comments blanked (`code`), and also string contents (`bare`), positions kept; and the string literals
    as (start, end, content)."""
    comments, contents, lits = [], [], []
    i = 0
    while m := TOKEN.search(src, i):
        t = m.group(0)
        if t.startswith("//"):
            comments.append((m.start(), m.end()))
            i = m.end()
        elif t == "/*":
            depth, j = 1, m.end()
            while j < len(src) and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            comments.append((m.start(), j))
            i = j
        elif m.group(1) is not None:
            close = '"' + m.group(1)
            end = src.find(close, m.end())
            end = len(src) if end < 0 else end
            lits.append((m.start(), end + len(close), src[m.end() : end]))
            contents.append((m.end(), end))
            i = end + len(close)
        elif t.startswith(("\"", "b\"")):
            q = t.index('"')
            lits.append((m.start(), m.end(), t[q + 1 : -1]))
            contents.append((m.start() + q + 1, m.end() - 1))
            i = m.end()
        else:
            contents.append((m.start() + 1, m.end() - 1))
            i = m.end()

    def blank(spans):
        out, last = [], 0
        for a, b in sorted(spans):
            out.append(src[last:a])
            out.append(re.sub(r"[^\n]", " ", src[a:b]))
            last = b
        out.append(src[last:])
        return "".join(out)

    return blank(comments), blank(comments + contents), lits


def matching(bare, i):
    """The index after the bracket that closes the one at `i`."""
    depth = 0
    for m in re.compile(r"[()\[\]{}]").finditer(bare, i):
        depth += 1 if m.group(0) in "([{" else -1
        if depth == 0:
            return m.end()
    return len(bare)


def block_end(bare, i):
    """The end of the item or statement that starts at `i`: after its `;` at depth 0, or after the `}` that closes it
    (unless `else` or a method call follows)."""
    depth = 0
    for m in re.compile(r"[()\[\]{};]").finditer(bare, i):
        c = m.group(0)
        if c in "([{":
            depth += 1
        elif c == ";":
            if depth == 0:
                return m.end()
        else:
            depth -= 1
            if depth < 0:
                return m.start()
            if depth == 0 and c == "}" and not re.match(r"\s*(else\b|\.|\?|;|,|\))", bare[m.end() : m.end() + 40]):
                return m.end()
    return len(bare)


def not_windows(expr):
    """A cfg expression that is never true on Windows (unix, macos, not(windows) …)."""
    e = expr.replace(" ", "").replace("\n", "")
    if re.search(r'not\((windows|target_os="windows"|target_family="windows")\)', e):
        return True
    if "windows" in e or re.search(r'not\((any\()?(unix|target_os="macos"|target_vendor="apple")', e):
        return False
    return bool(re.search(r'\bunix\b|"macos"|"apple"|"linux"|"ios"', e))


def item_after(bare, i):
    """The end of the item an attribute that ends at `i` applies to (other attributes skipped)."""
    while m := re.compile(r"\s*#\[").match(bare, i):
        i = matching(bare, m.end() - 1)
    m = re.compile(r"\s*").match(bare, i)
    return block_end(bare, m.end())


class File:
    def __init__(self, path, src):
        self.path, self.src = path, src
        self.code, self.bare, self.lits = strip(src)
        n = len(src)
        self.test, self.guard = bytearray(n), bytearray(n)
        self.asserts = []  # (index of the assert's open paren, end, its arguments before the message)
        self.lines = [m.start() for m in re.finditer("\n", src)]

    def line(self, i):
        return bisect.bisect_left(self.lines, i) + 1

    def scan_cfg(self, file_test, file_guard):
        bare, code, n = self.bare, self.code, len(self.src)
        if file_test:
            self.test[:] = b"\x01" * n
        if file_guard:
            self.guard[:] = b"\x01" * n

        def mark(arr, a, b):
            arr[a:b] = b"\x01" * (b - a)

        for m in re.finditer(r"#\[\s*cfg\s*\(", bare):
            end = matching(bare, m.end() - 1)
            expr = code[m.end() : end - 1]
            b = item_after(bare, bare.find("]", end - 1) + 1)
            if re.search(r"\btest\b", expr) and "not(test" not in expr.replace(" ", ""):
                mark(self.test, m.start(), b)
            if not_windows(expr):
                mark(self.guard, m.start(), b)
        for m in re.finditer(r"#\[\s*cfg_attr\s*\(\s*(windows|target_os\s*=\s*\"windows\")\s*,\s*ignore", bare):
            mark(self.guard, m.start(), item_after(bare, bare.find("]", m.end()) + 1))
        # `if cfg!(unix) { … }`, `if !cfg!(windows) { … }` and the `else` of `if cfg!(windows) { … }`.
        for m in re.finditer(r"\bif\s+(!?)\s*cfg!\s*\(", bare):
            end = matching(bare, m.end() - 1)
            expr = code[m.end() : end - 1]
            windows = re.fullmatch(r'\s*(windows|target_os\s*=\s*"windows")\s*', expr) is not None
            brace = bare.find("{", end)
            if brace < 0:
                continue
            then_end = matching(bare, brace)
            if (m.group(1) == "" and not_windows(expr)) or (m.group(1) == "!" and windows):
                mark(self.guard, m.start(), then_end)
            elif m.group(1) == "" and windows and (e := re.compile(r"\s*else\s*\{").match(bare, then_end)):
                mark(self.guard, e.start(), matching(bare, e.end() - 1))
        # A line that names cfg!( decides per system itself.
        for m in re.finditer(r"cfg!\s*\(", bare):
            a = bare.rfind("\n", 0, m.start()) + 1
            b = bare.find("\n", m.start())
            mark(self.guard, a, n if b < 0 else b)
        # `// portable: <why>`: its own line, or the statement or item after it.
        for m in re.finditer(re.escape(MARK), self.src):
            a = self.src.rfind("\n", 0, m.start()) + 1
            if self.src[a : m.start()].strip():
                mark(self.guard, a, m.start())
                continue
            start = re.compile(r"(\s*//[^\n]*\n)*\s*").match(self.src, m.start()).end()
            mark(self.guard, a, item_after(bare, start) if bare.startswith("#[", start) else block_end(bare, start))
        for m in re.finditer(r"\b(debug_)?assert(_eq|_ne)?!\s*\(", bare):
            self.asserts.append((m.end() - 1, matching(bare, m.end() - 1), 2 if m.group(2) else 1))

    def expected(self, i):
        """The text at `i` is (part of) an expected value: after `==` or `!=`, or an argument of an assert that is not
        inside a call (tuples, `Some(…)`, `format!(…)`, `[…]`, struct literals are looked through). A `Path::new(…)` or
        `PathBuf::from(…)` is not: paths compare by their names, whatever the separator."""
        if re.search(r"[!=]=\s*(Some\(\s*)?$", self.bare[max(0, i - 20) : i]):
            return True
        spans = [(a, args) for a, b, args in self.asserts if a < i < b]
        if not spans:
            return False
        start, args = max(spans)
        stack, commas = [], 0
        for m in re.compile(r"[()\[\]{},]").finditer(self.bare, start + 1, i):
            c = m.group(0)
            if c == ",":
                commas += not stack
                if commas >= args:
                    return False  # the assert's message
            elif c in "([{":
                before = self.bare[max(0, m.start() - 40) : m.start()].rstrip()
                name = re.search(r"([\w:]*\w)?(!?)$", before)
                if c == "{":
                    see = bool(name.group(1)) and not name.group(2)  # a struct literal, not a block
                elif c == "[" or name.group(2):
                    see = True  # an array, vec![…], format!(…), concat!(…)
                elif not name.group(1):
                    see = True  # a tuple
                else:
                    see = name.group(1).split("::")[-1][0].isupper()  # Some(…), a variant or tuple struct; not a call
                stack.append(see)
            elif stack:
                stack.pop()
        return all(stack)

    def live(self, i):
        """Test code that runs on Windows and is not marked."""
        return self.test[i] and not self.guard[i]


TILDE = re.compile(r"(?:^|[\s·(=:,'\"])(~(?:/[^\s,;:\"'()\\]*)?)(?=$|[\s,;:.)'\"\\])")


def tilde_path(s):
    """The first home-folder path written with `~` in `s` (`~/Media/TV`, or `~` alone)."""
    m = TILDE.search(s)
    return m.group(1).rstrip(".") if m else None


def tilde_paths(s):
    """Every home-folder path written with `~` in `s`, up to a template's first `{…}`."""
    return [re.sub(r"\{.*", "", m.group(1).rstrip(".")).rstrip("/") or "~" for m in TILDE.finditer(s)]


def unix_paths(s):
    """Absolute Unix paths in `s` with two or more names below their root folder (`/tmp/a/b`)."""
    found = re.finditer(r"(?:^|[\s=('\"])(/(?:%s)(?:/[^/\s\"'?&#]+){2,})" % ROOTS, s)
    return [m.group(1).rstrip(".,;:") for m in found]


def check(f, ctx):
    hits = []

    def report(i, kind, why):
        if f.live(i):
            hits.append((f.line(i), kind, why))

    own, near, given = ctx.tilde_inputs(f), ctx.near_tilde_inputs(f), ctx.path_inputs(f)
    for a, b, s in f.lits:
        if not ("~" in s or "/" in s or "This Mac" in s or "in Finder" in s or "to Trash" in s) or not f.live(a):
            continue
        expected = f.expected(a)
        if expected and (t := tilde_path(s)):
            # A `~` path the test fed in (or one its fixtures did) comes back as it went in.
            fed = any(t == i or t.startswith(i.rstrip("/") + "/") for i in own)
            fed = fed or any(t == i or t.startswith(i + "/") for i in near if i != "~")
            if not fed:
                report(a, "tilde", f'"{s}": Windows writes a home-folder path in full, without ~ (expect each system\'s form)')
        for word, fix in (("This Mac", 'concat!("This ", computer!())'), ("in Finder", "file_manager!()"), ("to Trash", "trash!()")):
            if word in s and expected:
                report(a, "this-mac", f'"{s}": Windows says it otherwise: use {fix}')
        if re.search(r"[?&]\w+=/(%s)/" % ROOTS, s):
            report(a, "unix-path", f'"{s}": a path without a drive is not absolute on Windows')
        elif expected:
            for p in unix_paths(s):
                # Given as it is somewhere in the crate (or cut from a path given so, by a function of strings), it
                # comes back the same on every system.
                if not any(p in t for t in given):
                    report(a, "unix-path", f'"{p}": no such absolute path on Windows, where a joined path has \\ (build it with Path::join)')
                    break
    for m in re.finditer(r"\.join\(\s*\"([^\"]*\w/[^\"]*|[^\"]*/\w[^\"]*)\"\s*\)", f.code):
        if not f.expected(m.start()):
            continue
        end = f.bare.find(";", m.end())
        if re.search(r"\.display\(\)|to_string_lossy\(\)|\.to_str\(\)", f.code[m.end() : end]):
            report(m.start(), "joined", f'.join("{m.group(1)}") as text: Windows writes \\ between the names (join each name)')
    for m in re.finditer(r"(?m)^[ \t]*((?:pub(?:\([^)]*\))?\s+)?)(?:async\s+)?fn\s+(\w+)", f.bare):
        public, name, at = m.group(1), m.group(2), m.start()
        if not f.live(at):
            continue
        before = f.bare[max(0, at - 400) : at]
        if re.search(r"#\[\s*(tokio::)?test\b", before[max(before.rfind("}"), before.rfind(";")) + 1 :]):
            continue
        end = block_end(f.bare, at)
        uses = [(f, u.start()) for u in re.finditer(r"\b%s\b" % re.escape(name), f.bare) if not at <= u.start() < end]
        if any(not f.guard[u] for _, u in uses):
            continue
        if public:
            uses += ctx.uses_elsewhere(name, f)
        if uses and all(g is not None and g.guard[u] for g, u in uses):
            report(at, "unix-only-helper", f"fn {name}: only cfg(unix) code uses it; give it that cfg too (dead code fails clippy on Windows)")
    return hits


@functools.cache
def crate_of(path):
    for p in [path, *path.parents]:
        if (p / "Cargo.toml").exists():
            return p
    return path.parent


class Tree:
    """The files read, their modules, and what each crate's code feeds its tests."""

    def __init__(self, files):
        self.files = files
        self.texts = {f: f.read_text(errors="replace") for f in files}
        self.flags, self.modpath, self.parent = {}, {}, {}
        self.modules()
        self.parsed = {}
        for f in files:
            if self.flags[f][0] or "cfg(test" in self.texts[f] or "#[test]" in self.texts[f]:
                self.parsed[f] = File(f, self.texts[f])
                self.parsed[f].scan_cfg(*self.flags[f])
        self.crates = {}
        for f in files:
            self.crates.setdefault(crate_of(f), []).append(f)
        self.by_module = {(crate_of(f), m): f for f, m in self.modpath.items()}
        self._inputs = {}

    def modules(self):
        """Per file: [test code, guarded off Windows] and its module path, from where its module is declared."""
        flags = {f: [("tests" in f.parts) or f.name.endswith("_tests.rs"), False] for f in self.files}
        decls = []
        for f in self.files:
            src = self.texts[f]
            if f.name in ("lib.rs", "main.rs"):
                self.modpath[f] = "crate"
            if m := re.match(r"(?:\s*//[^\n]*\n)*\s*#!\[\s*cfg\s*\((.*)\)\s*\]", src):
                flags[f][1] = flags[f][1] or not_windows(m.group(1))
            here = f.parent if f.name in ("lib.rs", "main.rs", "mod.rs") else f.parent / f.stem
            for m in re.finditer(r"(?m)((?:^[ \t]*#\[[^\n]*\]\s*\n)*)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", src):
                attrs, name = m.group(1), m.group(2)
                p = re.search(r'#\[\s*path\s*=\s*"([^"]+)"', attrs)
                cands = [f.parent / p.group(1)] if p else [here / f"{name}.rs", here / name / "mod.rs"]
                child = next((c for c in cands if c in flags), None)
                if child:
                    cfgs = re.findall(r"#\[\s*cfg\s*\((.*)\)\s*\]", attrs)
                    is_test = any(re.search(r"\btest\b", c) and "not(test" not in c for c in cfgs)
                    decls.append((f, child, name, is_test, any(not_windows(c) for c in cfgs)))
                    self.parent[child] = f
        changed = True
        while changed:
            changed = False
            for parent, child, name, is_test, is_guard in decls:
                new = [flags[child][0] or flags[parent][0] or is_test, flags[child][1] or flags[parent][1] or is_guard]
                if new != flags[child]:
                    flags[child], changed = new, True
                if parent in self.modpath and child not in self.modpath:
                    self.modpath[child], changed = f"{self.modpath[parent]}::{name}", True
        self.flags = flags

    def near(self, f):
        """`f`, the file that declares its module, and the files of the modules it names (`crate::chooser::tests`)."""
        out = {f}
        if f in self.parent:
            out.add(self.parent[f])
        c = crate_of(f)
        for m in re.finditer(r"\bcrate((?:::\w+)+)", self.texts[f]):
            names = m.group(1).split("::")[1:]
            for k in range(len(names), 0, -1):
                if g := self.by_module.get((c, "crate::" + "::".join(names[:k]))):
                    out.add(g)
                    break
        return out

    def inputs(self, f):
        """The string literals of `f` that are not expected values (in a file without tests: all of them)."""
        if f not in self._inputs:
            pf = self.parsed.get(f)
            if pf:
                self._inputs[f] = [s for a, _, s in pf.lits if not pf.guard[a] and not pf.expected(a)]
            else:
                self._inputs[f] = re.findall(r'"((?:[^"\\\n]|\\.)*)"', self.texts[f])
        return self._inputs[f]

    def tilde_inputs(self, pf):
        return {t for s in self.inputs(pf.path) for t in tilde_paths(s)}

    def near_tilde_inputs(self, pf):
        return {t for g in self.near(pf.path) for s in self.inputs(g) for t in tilde_paths(s)}

    def path_inputs(self, pf):
        c = crate_of(pf.path)
        if c not in self._inputs:
            self._inputs[c] = [s for g in self.crates[c] for s in self.inputs(g) if "/" in s]
        return self._inputs[c]

    def uses_elsewhere(self, name, here):
        """Uses of `name` in the other files of `here`'s crate: (file, offset); a file without tests counts as live."""
        out = []
        for g in self.crates[crate_of(here.path)]:
            if g == here.path or name not in self.texts[g]:
                continue
            pg = self.parsed.get(g)
            for u in re.finditer(r"\b%s\b" % re.escape(name), pg.bare if pg else self.texts[g]):
                out.append((pg, u.start()))
        return out


HOME_ENV = re.compile(r"\benv::var(?:_os)?\s*\(\s*\"HOME\"\s*\)")


def home_env(tree):
    """`home-env`: `$HOME` read directly by code that is not a test, outside the platform crate."""
    hits = []
    for f in tree.files:
        if crate_of(f).name == "platform" or not HOME_ENV.search(tree.texts[f]):
            continue
        pf = tree.parsed.get(f)
        if pf is None:
            pf = File(f, tree.texts[f])
            pf.scan_cfg(*tree.flags[f])
        for m in HOME_ENV.finditer(pf.code):
            if not pf.test[m.start()] and not pf.guard[m.start()]:
                hits.append((f, pf.line(m.start()), "home-env", "$HOME read directly: Windows has none; use trs_platform::dirs::home()"))
    return hits


def main(args):
    roots = [pathlib.Path(a) for a in args] or [pathlib.Path("crates")]
    files = sorted({p for r in roots for p in ([r] if r.is_file() else r.rglob("*.rs")) if "target" not in p.parts})
    tree = Tree(files)
    found = 0
    for f, pf in tree.parsed.items():
        for line, kind, why in check(pf, tree):
            print(f"{f}:{line}: {kind}: {why}", file=sys.stderr)
            found += 1
    for f, line, kind, why in home_env(tree):
        print(f"{f}:{line}: {kind}: {why}", file=sys.stderr)
        found += 1
    if found:
        print(f"test-portability: {found} place(s) in tests assume the Mac: fix them, guard them with cfg, or mark them"
              f" `{MARK} <why>`", file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
