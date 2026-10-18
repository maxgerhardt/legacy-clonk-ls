"""Generate server/src/core/engine_api.json: the engine's script functions and constants.

Sources:
  - LegacyClonk engine source (C4Script.cpp): authoritative function names, parameter count/types, constants
  - lcdocs (sdk/script/fn/*.xml): parameter names, descriptions (German) ...
  - ... and lcdocs/en.po: English translations of those descriptions

Usage: python tools/gen_engine_api.py <path to LegacyClonk/src> [--lcdocs lcdocs] [--out server/src/core/engine_api.json]
"""
import argparse
import copy
import difflib
import glob
import json
import os
import re
import xml.etree.ElementTree as ET

TYPE_MAP = {
    "C4ValueInt": "int", "std::optional<C4ValueInt>": "int", "int32_t": "int", "long": "int", "int": "int",
    "bool": "bool", "C4ID": "id", "C4Object *": "object", "C4String *": "string",
    "C4ValueArray *": "array", "C4ValueHash *": "map", "C4Value": "any", "C4Value *": "&any",
}
RTYPE_MAP = {**TYPE_MAP, "C4Value": "any", "void": "void", "C4AulDefFunc": "any"}


def parse_engine(src_dir):
    s = open(os.path.join(src_dir, "C4Script.cpp"), encoding="cp1252").read()
    # C++ function definitions: "[static] <rtype> FnName(<params>) [// comment]\n{"
    defs = {}
    for m in re.finditer(r'^(?:static\s+)?([\w:<>\*& ]+?)\s*\b(Fn\w+)\s*\(([^)]*)\)\s*(?://[^\n]*)?\s*\{', s, re.M):
        defs[m.group(2)] = (m.group(1).strip(), m.group(3))
    funcs = {}
    for name, cname in re.findall(r'AddFunc\(pEngine,\s*"(\w+)",\s*(\w+)', s):
        if cname not in defs:
            funcs[name] = {"params": None, "rtype": "any"}  # unknown signature: no arity check
            continue
        rtype, plist = defs[cname]
        params = []
        for p in [x.strip() for x in plist.split(",") if x.strip()]:
            p = re.sub(r'/\*.*?\*/', '', p).strip()
            m = re.match(r'(.*?)\s*\b(\w+)$', p)
            ctype, pname = (m.group(1).strip(), m.group(2)) if m else (p, "")
            if ctype.startswith("C4AulContext"):
                continue
            params.append({"name": pname, "type": TYPE_MAP.get(ctype, "any")})
        funcs[name] = {"params": params, "rtype": RTYPE_MAP.get(rtype, "any")}
    # Functions registered with a fixed array of 10 criteria parameters
    # (the template's C4V_ type is the return type: FindObject2 -> object)
    vtypes = {"C4Object": "object", "Int": "int", "Bool": "bool", "Array": "array", "C4ID": "id", "String": "string"}
    for vt, name in re.findall(r'new C4AulEngineFuncParArray<\d+, C4V_(\w+)>\s*\{pEngine, "(\w+)"', s):
        funcs[name] = {"params": [{"name": f"criteria{i + 1}", "type": "array"} for i in range(10)], "rtype": vtypes.get(vt, "any")}
    # Cast functions: CastInt(x), ScoreboardCol(id), ...
    for to_type, name in re.findall(r'new C4AulDefCastFunc<\w+, C4V_(\w+)>\{pEngine, "(\w+)"\}', s):
        funcs[name] = {"params": [{"name": "value", "type": "any"}],
                       "rtype": {"Int": "int", "Bool": "bool", "C4ID": "id"}.get(to_type, "any")}
    consts = {}
    block = s[s.index("C4ScriptConstMap[]"):]
    block = block[:block.index("};")]
    for name, vtype, value in re.findall(r'\{\s*"(\w+)",\s*(C4V_\w+),\s*([^}]+?)\s*\}', block):
        consts[name] = {"type": {"C4V_Int": "int", "C4V_Bool": "bool"}.get(vtype, "any"), "value": value.strip()}
    return funcs, consts


def parse_po(path):
    """Parse a gettext .po file (UTF-8).

    Returns (msgid -> msgstr, {source file -> [(element tag, msgid, msgstr)]}). The second map comes from the
    "#: sdk/script/fn/X.xml:16(desc)" reference comments, so entries can be found by where they come from.
    """
    out, refs = {}, {}
    state = {"refs": [], "id": None, "str": None, "cur": None}

    def unq(x):
        return json.loads(x) if x.startswith('"') else x

    def flush():
        if state["id"] and state["str"]:
            out[state["id"]] = state["str"]
            for r in state["refs"]:
                m = re.match(r"(.+?):\d+\((\w+)\)", r)
                if m:
                    refs.setdefault(m.group(1), []).append((m.group(2), state["id"], state["str"]))
        state.update(refs=[], id=None, str=None, cur=None)

    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("#:"):
            if state["cur"] == "str":
                flush()
            state["refs"].extend(line[2:].split())
        elif line.startswith("msgid "):
            if state["cur"] == "str":
                flush()
            state.update(id=unq(line[6:]), str="", cur="id")
        elif line.startswith("msgstr "):
            state.update(str=unq(line[7:]), cur="str")
        elif line.startswith('"'):
            if state["cur"] in ("id", "str"):
                state[state["cur"]] += unq(line)
    flush()
    return out, refs


def norm(x):
    """Whitespace collapsed, self-closing tags written like xml2po does (<br/>, not <br />)."""
    return re.sub(r"\s*/>", "/>", re.sub(r"\s+", " ", x)).strip()


def without_tail(el):
    """Shallow copy without the text following the element (ET.tostring would include it, XML-escaped)."""
    c = copy.copy(el)
    c.tail = None
    return c


def inner_xml(el):
    """Element content as xml2po sees it: text (XML-escaped, like &gt;) + child tags."""
    xml = ET.tostring(without_tail(el), encoding="unicode")
    if xml.endswith("/>"):
        return ""
    xml = xml[xml.index(">") + 1: xml.rindex("</")]
    return norm(xml)


def plain(x):
    """Text only (no tags/placeholders, entities decoded), for fuzzy matching."""
    x = re.sub(r"<[^>]+>", " ", x)
    x = x.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
    return re.sub(r"\s+", " ", x).strip()


def to_markdown(x):
    x = re.sub(r"<funclink>(.*?)</funclink>", r"`\1`", x)
    x = re.sub(r"<code>(.*?)</code>", r"`\1`", x)
    x = re.sub(r"<emlink[^>]*>(.*?)</emlink>", r"\1", x)
    x = re.sub(r"<br\s*/>\s*", "\n", x)
    x = re.sub(r"<[^>]+>", "", x)
    x = x.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
    return re.sub(r"[ \t]+\n", "\n", x).strip()


def parse_docs(lcdocs):
    po, refs = parse_po(os.path.join(lcdocs, "en.po"))
    stats = {"exact": 0, "fuzzy": 0, "german": 0}

    def table_md(el, relfile):
        rows = []
        for r in el:
            cells = [tr(c, relfile).replace("\n", " ").replace("|", "/") for c in r]
            rows.append("| " + " | ".join(cells) + " |")
            if r.tag == "rowh":
                rows.append("|" + "---|" * len(cells))
        # escape markdown as text so to_markdown() keeps it (it only strips tags)
        return "\n" + "\n".join(rows).replace("<", "&lt;").replace(">", "&gt;") + "\n"

    def child_xml(c, relfile):
        if c.tag == "table":
            return table_md(c, relfile)
        return ET.tostring(without_tail(c), encoding="unicode")

    def tr(el, relfile):
        if el is None:
            return ""
        x = inner_xml(el)
        if not x:
            return ""
        if x in po:
            stats["exact"] += 1
            return to_markdown(po[x])
        # all child elements as <placeholder-N/> (xml2po does this e.g. for funclink)
        parts = [el.text or ""]
        for i, c in enumerate(el, 1):
            parts.append(f"<placeholder-{i}/>")
            parts.append(c.tail or "")
        key = norm("".join(parts).replace("&", "&amp;").replace("<placeholder", "\0").replace("<", "&lt;")
                   .replace(">", "&gt;").replace("\0", "<placeholder").replace("/&gt;", "/>"))
        if key in po:
            mstr = po[key]
            for i, c in enumerate(el, 1):
                mstr = mstr.replace(f"<placeholder-{i}/>", child_xml(c, relfile))
            stats["exact"] += 1
            return to_markdown(mstr)
        # xml2po replaces some child elements (funclink, table, ...) by <placeholder-N/>: take the entry of the
        # same file and element type whose text is most similar, then put the children back in order
        # compare without tables: their cells are separate entries and would dominate the similarity
        skeleton = plain(re.sub(r"<table>.*?</table>", " ", x, flags=re.S))
        best, best_ratio = None, 0.0
        for tag, mid, mstr in refs.get(relfile, []):
            if tag != el.tag:
                continue
            ratio = difflib.SequenceMatcher(None, plain(mid), skeleton).ratio()
            if ratio > best_ratio:
                best, best_ratio = (mid, mstr), ratio
        if best and best_ratio >= 0.75:
            mid, mstr = best
            inline = set(re.findall(r"<(\w+)", mid))
            ph_children = [c for c in el if c.tag not in inline]
            for i, c in enumerate(ph_children, 1):
                mstr = mstr.replace(f"<placeholder-{i}/>", child_xml(c, relfile))
            stats["fuzzy"] += 1
            return to_markdown(mstr)
        stats["german"] += 1
        return to_markdown(x)

    docs = {}
    for f in glob.glob(os.path.join(lcdocs, "sdk", "script", "fn", "*.xml")):
        # all lcdocs XML files are ISO-8859-1 (declared in their header)
        raw = open(f, "rb").read().decode("latin-1")
        raw = re.sub(r"<!DOCTYPE[^>]*>", "", raw).replace('encoding="ISO-8859-1"', "")
        raw = re.sub(r"&(?!(lt|gt|amp|quot|apos);)\w+;", " ", raw)  # DTD entities like &nbsp;
        try:
            root = ET.fromstring(raw)
        except ET.ParseError as e:
            print("skip", f, e)
            continue
        relfile = os.path.relpath(f, lcdocs).replace("\\", "/")
        for fn in root.iter("func"):
            title = (fn.findtext("title") or "").strip()
            syn = fn.find("syntax")
            params = []
            if syn is not None:
                for p in syn.iter("param"):
                    params.append({"name": (p.findtext("name") or "").strip(),
                                   "type": (p.findtext("type") or "any").strip(),
                                   "desc": tr(p.find("desc"), relfile),
                                   "optional": p.find("optional") is not None})
            docs[title] = {
                "rtype": (syn.findtext("rtype") if syn is not None else None) or "",
                "params": params,
                "desc": tr(fn.find("desc"), relfile),
                "remark": tr(fn.find("remark"), relfile),
                "version": (fn.findtext("version") or "").strip(),
                "category": (fn.findtext("category") or "").strip(),
            }
    print(f"translations: {stats['exact']} exact, {stats['fuzzy']} matched by source reference, "
          f"{stats['german']} without English text")
    return docs


GERMAN_HINT = re.compile(r"\b(der|die|das|und|wird|eine?|des|nicht|wenn|oder|siehe|für|für)\b", re.I)


def apply_overrides(docs, path):
    """English texts for entries en.po has no (current) translation for; warn when an override became unnecessary."""
    overrides = json.load(open(path, encoding="utf-8"))
    for name, o in overrides.items():
        if name.startswith("_") or name not in docs:
            continue
        d = docs[name]
        for field in ("desc", "remark"):
            if field in o:
                if not GERMAN_HINT.search(d[field]):
                    print(f"note: {name}.{field} is translated in en.po now; override in translations_en.json can go")
                d[field] = o[field]
        for pname, text in o.get("params", {}).items():
            for p in d["params"]:
                if p["name"] == pname:
                    if not GERMAN_HINT.search(p["desc"]):
                        print(f"note: {name}({pname}) is translated in en.po now; override can go")
                    p["desc"] = text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("engine_src")
    ap.add_argument("--lcdocs", default="lcdocs")
    ap.add_argument("--out", default=os.path.join("server", "src", "core", "engine_api.json"))
    ap.add_argument("--system-out", default=os.path.join("server", "src", "core", "system_scripts.json"),
                    help="where to write the System.c4g scripts embedded as fallback (from <engine_src>/../planet/System.c4g)")
    a = ap.parse_args()

    funcs, consts = parse_engine(a.engine_src)
    docs = parse_docs(a.lcdocs)
    apply_overrides(docs, os.path.join(os.path.dirname(os.path.abspath(__file__)), "translations_en.json"))
    out_funcs = {}
    for name, f in sorted(funcs.items()):
        d = docs.get(name, {})
        params = f["params"]
        if params is not None and d.get("params"):
            # engine gives count/types; docs give nicer names and descriptions
            for i, p in enumerate(params):
                p["etype"] = p["type"]  # the type the engine converts to (C++ signature); "type" is the documented one
                if i < len(d["params"]):
                    dp = d["params"][i]
                    p["name"] = dp["name"] or p["name"]
                    p["desc"] = dp["desc"]
                    if dp["type"]:
                        p["type"] = dp["type"]
        elif params is None and d.get("params"):
            params = [{"name": p["name"], "type": p["type"], "desc": p["desc"]} for p in d["params"]]
        out_funcs[name] = {
            "params": params,
            "rtype": d.get("rtype") or f["rtype"],
            "ertype": f["rtype"],
            "desc": d.get("desc", ""),
            "remark": d.get("remark", ""),
            "version": d.get("version", ""),
            "documented": name in docs,
            "arity_known": f["params"] is not None,
        }
    data = {"functions": out_funcs, "constants": consts,
            "documented_only": sorted(n for n in docs if n not in funcs)}
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    with open(a.out, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(data, fh, ensure_ascii=False, indent=0, sort_keys=True)
    # System.c4g is always loaded by the engine: embed its scripts so the server knows its global functions
    sys_dir = os.path.join(a.engine_src, "..", "planet", "System.c4g")
    scripts = {}
    for f in sorted(glob.glob(os.path.join(sys_dir, "*.c"))):
        raw = open(f, "rb").read()
        try:
            scripts[os.path.basename(f)] = raw.decode("utf-8")
        except UnicodeDecodeError:
            scripts[os.path.basename(f)] = raw.decode("cp1252")
    with open(a.system_out, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(scripts, fh, ensure_ascii=False, indent=0, sort_keys=True)
    print(f"{len(scripts)} System.c4g scripts -> {a.system_out}")
    print(f"{len(out_funcs)} engine functions ({sum(f['documented'] for f in out_funcs.values())} documented), "
          f"{len(consts)} constants, {len(data['documented_only'])} documented but not engine functions -> {a.out}")


if __name__ == "__main__":
    main()
