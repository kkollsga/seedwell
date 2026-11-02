#!/usr/bin/env python3
"""Names in the FFI that the Windows app's C# bindings cannot compile (uniffi-bindgen-cs).

bindgen-cs makes each variant of an enum with data a record nested in the enum's class, so a variant's field whose
PascalCase name is the name of a variant of the same enum (or of the enum itself) clashes with it (CS8866). The Mac's
Swift bindings don't mind, so without this the clash is found only by Windows CI. Exit 1 and name each clash.
Usage: tools/ffi-names.py [crates/ffi/src]
"""
import pathlib
import re
import sys


def pascal(name):
    return "".join(p.capitalize() for p in name.split("_"))


def enums(src):
    for m in re.finditer(r"pub enum (\w+)\s*\{", src):
        i, depth = m.end(), 1
        j = i
        while depth and j < len(src):
            depth += {"{": 1, "}": -1}.get(src[j], 0)
            j += 1
        yield m.group(1), src[i : j - 1]


def clashes(root):
    for path in sorted(pathlib.Path(root).glob("*.rs")):
        for enum, body in enums(path.read_text()):
            variants, fields, depth, current = set(), [], 0, None
            for line in body.split("\n"):
                s = line.strip()
                if depth == 0 and (v := re.match(r"([A-Z]\w*)\s*(\{|\(|,|$)", s)):
                    current = v.group(1)
                    variants.add(current)
                if depth == 1 and (f := re.match(r"(?:pub\s+)?([a-z_][a-z0-9_]*)\s*:", s)):
                    fields.append((current, f.group(1)))
                depth += line.count("{") - line.count("}")
            for variant, field in fields:
                if pascal(field) in variants or pascal(field) == enum:
                    yield f"{path}: {enum}::{variant} has a field `{field}`, named like a variant: C# will not compile it"


found = list(clashes(sys.argv[1] if len(sys.argv) > 1 else "crates/ffi/src"))
for line in found:
    print(line, file=sys.stderr)
sys.exit(1 if found else 0)
