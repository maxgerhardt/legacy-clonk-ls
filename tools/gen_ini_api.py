"""Generate server/src/core/ini_api.json: the keys of DefCore.txt, Scenario.txt and ActMap.txt from the engine source.

Every `mkNamingAdapt(<value>, "Key", <default>)` in the CompileFunc of the classes below is one key; its kind comes
from the adaptor and the default (mkBitfieldAdapt + table -> named flags, mkC4IDAdapt -> id, C4IDList() -> ID list,
C4SVal -> Std,Rnd,Min,Max, true/false -> bool, mkArrayAdapt -> int list, strings, ints). Some string keys get a
better kind by name (Procedure, NextAction, *Call, Material, ...). Materials and Material-Texture pairs come from the
content repo's Material.c4g, the standard pack names from the content folder.
Defaults are written as the value a file would contain (engine constants resolved to their numbers, C4SVal filled
up to Std,Rnd,Min,Max, true/false -> 1/0); "" = empty (no ID, empty list or text).

Usage (in legacy-clonk-ls/): python tools/gen_ini_api.py ../LegacyClonk/src ../content
"""
import json, os, re, sys

SRC = sys.argv[1] if len(sys.argv) > 1 else "../LegacyClonk/src"
CONTENT = sys.argv[2] if len(sys.argv) > 2 else "../content"
OUT = os.path.join(os.path.dirname(__file__), "..", "server", "src", "core", "ini_api.json")

# (file kind, section(s), source file, function)
UNITS = [
    ("DefCore", ["DefCore"], "C4Def.cpp", "C4DefCore::CompileFunc"),
    ("DefCore", ["DefCore"], "C4Shape.cpp", "C4Shape::CompileFunc"),
    ("ActMap", ["Action"], "C4Def.cpp", "C4ActionDef::CompileFunc"),
    ("Scenario", ["Head"], "C4Scenario.cpp", "C4SHead::CompileFunc"),
    ("Scenario", ["Definitions"], "C4Scenario.cpp", "C4SDefinitions::CompileFunc"),
    ("Scenario", ["Game"], "C4Scenario.cpp", "C4SGame::CompileFunc"),
    ("Scenario", ["Player1", "Player2", "Player3", "Player4"], "C4Scenario.cpp", "C4SPlrStart::CompileFunc"),
    ("Scenario", ["Landscape"], "C4Scenario.cpp", "C4SLandscape::CompileFunc"),
    ("Scenario", ["Animals"], "C4Scenario.cpp", "C4SAnimals::CompileFunc"),
    ("Scenario", ["Weather"], "C4Scenario.cpp", "C4SWeather::CompileFunc"),
    ("Scenario", ["Disasters"], "C4Scenario.cpp", "C4SDisasters::CompileFunc"),
    ("Scenario", ["Environment"], "C4Scenario.cpp", "C4SEnvironment::CompileFunc"),
]


def read(name):
    return open(os.path.join(SRC, name), encoding="utf-8", errors="replace").read()


# defaults that depend on something else: written out by hand (C4Scenario.cpp)
DEFAULT_NOTES = {
    "BaseAutoSell": "0 (1 for GOLD)",
    "MaxPlayerLeague": "the value of MaxPlayer",
    "ShadeMaterials": "1 (0 for Version below 4.6.5)",
    "EnableTextureOverlays": "1 (0 for Version below 4.9.5)",
}
_ALL_SRC = None


def constant(name):
    """The number an engine constant (`X = 5`, `#define X 5`, enum member) stands for, or None."""
    global _ALL_SRC
    if _ALL_SRC is None:
        _ALL_SRC = "\n".join(read(f) for f in os.listdir(SRC) if f.endswith((".h", ".cpp")))
    m = re.search(r"\b" + re.escape(name) + r"\s*(?:=\s*|\s+)(0x[0-9a-fA-F]+|-?\d+)\b", _ALL_SRC)
    return str(int(m.group(1), 0)) if m else None


def readable(key, kind, default):
    """The engine default as the text a file would contain."""
    d = default.strip()
    if key in DEFAULT_NOTES:
        return DEFAULT_NOTES[key]
    if re.fullmatch(r"(C4IDList|C4NameList|StdStrBuf|decltype\(\w+\))\(\)|\"\"|C4ID_None", d):
        return ""
    if d.startswith('"'):
        return d.strip('"')
    if d in ("true", "false"):
        return "1" if d == "true" else "0"
    m = re.fullmatch(r"C4SVal\((.*)\)", d)
    if m:
        vals = [v.strip().lstrip("+") for v in m.group(1).split(",") if v.strip()]
        return ",".join(vals + ["0", "0", "0", "100"][len(vals):])
    if d in ("TargetRect0",):
        return "0,0,0,0,0,0"
    if d in ("Rect0",):
        return "0,0,0,0"
    m = re.fullmatch(r"fSection \? \w+ : (-?\d+)", d)  # sections take the main scenario's value
    if m:
        return m.group(1)
    m = re.fullmatch(r"(-?\d+)u?", d)
    if m:
        return m.group(1)
    m = re.fullmatch(r"(?:static_cast<\w+>\()?(?:\w+::)?(\w+)\)?", d)
    if m:
        v = constant(m.group(1))
        if v is not None:
            return "all" if kind == "flags" and v == str(0xffff) else v
    raise SystemExit(f"unreadable default for {key}: {default!r} (add it to DEFAULT_NOTES)")


def body(text, func):
    i = text.index(func + "(")
    i = text.index("{", i)
    depth = 0
    for j in range(i, len(text)):
        if text[j] == "{":
            depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0:
                return text[i:j]
    raise ValueError(func)


def split_args(s):
    """Top-level comma split of an argument list (s = text after the opening parenthesis); returns (args, end)."""
    args, depth, cur, i, in_str = [], 0, "", 0, False
    while i < len(s):
        c = s[i]
        if in_str:
            cur += c
            if c == "\\":
                cur += s[i + 1]
                i += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
            cur += c
        elif c in "([{":
            depth += 1
            cur += c
        elif c in ")]}":
            if depth == 0:
                args.append(cur.strip())
                return args, i
            depth -= 1
            cur += c
        elif c == "," and depth == 0:
            args.append(cur.strip())
            cur = ""
        else:
            cur += c
        i += 1
    return args, i


def tables(text):
    out = {}
    for m in re.finditer(r"Std(?:Bitfield|Enum)Entry<[^>]*>\s+(\w+)\[\]\s*=\s*\{(.*?)\};", text, re.S):
        out[m.group(1)] = re.findall(r'\{\s*"([^"]+)"', m.group(2))
    return out


STRING_KINDS = {
    "Procedure": "procedure", "NextAction": "action", "InLiquidAction": "action", "TurnAction": "action",
    "StartCall": "function", "EndCall": "function", "PhaseCall": "function", "AbortCall": "function",
    "TimerCall": "function", "Material": "material", "Liquid": "material", "Precipitation": "material",
    "Sky": "string",
}


def kind_of(value, key, default, tabs):
    m = re.search(r"mk(?:Bitfield|Enum)Adapt(?:T?<[^>]*>)?\(\s*[^,]+,\s*(\w+)", value)
    if m and m.group(1) in tabs:
        return "flags", tabs[m.group(1)]
    if "mkC4IDAdapt" in value:
        return "id", None
    if "C4IDList" in default or "RequireDef" in value:
        return "idlist", None
    if "C4NameList" in default:
        return "materials", None
    if "C4SVal" in default:
        return "sval", None
    if default in ("true", "false"):
        return "bool", None
    if "mkArrayAdapt" in value or default.startswith(("TargetRect", "Rect0")):
        return "ints", None
    if default.startswith('"') or "mkString" in value or "toC4CStr" in value or "StdStrBuf" in default:
        return STRING_KINDS.get(key, "string"), None
    return "int", None


def main():
    data = {"DefCore": {}, "ActMap": {}, "Scenario": {}}
    for kind, sections, fname, func in UNITS:
        text = read(fname)
        tabs = tables(text)
        b = body(text, func)
        keys = []
        for m in re.finditer(r"mkNamingAdapt\(", b):
            args, _ = split_args(b[m.end():])
            if len(args) < 2 or not re.fullmatch(r'"[^"]*"', args[1]):
                continue
            key = args[1].strip('"')
            default = args[2] if len(args) > 2 else ""
            k, values = kind_of(args[0], key, default, tabs)
            if key == "Procedure":
                values = re.findall(r'"([A-Z]+)"', re.search(r"ProcedureName\[[^\]]*\]\s*=\s*\{(.*?)\};", text, re.S).group(1)) + ["NONE"]
            entry = {"key": key, "kind": k}
            if default:
                entry["default"] = readable(key, k, default)
            if values:
                entry["values"] = values
            if not any(e["key"] == key for e in keys):
                keys.append(entry)
        if func == "C4SDefinitions::CompileFunc":
            for e in keys:
                if e["key"] == "Definitions":
                    e["kind"] = "packs"  # Definitions=Objects.c4d;Knights.c4d
            keys += [{"key": f"Definition{i}", "kind": "pack"} for i in range(1, 11)]
        for s in sections:
            data[kind].setdefault(s, []).extend(keys)
    # Physical: the name table of C4PhysicalInfo
    info = read("C4InfoCore.cpp")
    phys = re.search(r"C4PhysInfoNameMap\[\]\s*=\s*\{(.*?)\};", info, re.S).group(1)
    data["DefCore"]["Physical"] = [{"key": n, "kind": "int", "default": "0"} for n in re.findall(r'\{\s*"(\w+)"', phys)]
    # materials (name and Material-Texture pairs of the TexMap) and the standard packs
    mat_dir = os.path.join(CONTENT, "Material.c4g")
    mats = sorted({re.search(r"^Name=(.+)$", open(os.path.join(mat_dir, f), encoding="cp1252").read(), re.M).group(1).strip()
                   for f in os.listdir(mat_dir) if f.lower().endswith(".c4m")})
    texmap = []
    for line in open(os.path.join(mat_dir, "TEXMAP.TXT"), encoding="cp1252"):
        m = re.match(r"\s*\d+=([\w-]+)", line)
        if m and m.group(1) not in texmap:
            texmap.append(m.group(1))
    packs = sorted(f for f in os.listdir(CONTENT) if f.lower().endswith((".c4d", ".c4f")))
    out = {"files": data, "materials": mats, "texmap": texmap, "packs": packs}
    with open(OUT, "w", encoding="utf-8", newline="\n") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    n = {k: sum(len(v) for v in d.values()) for k, d in data.items()}
    print(f"wrote {OUT}: keys {n}, {len(mats)} materials, {len(texmap)} texmap entries, {len(packs)} packs")


if __name__ == "__main__":
    main()
