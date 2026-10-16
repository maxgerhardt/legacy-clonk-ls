"""Generate server/src/core/engine_api.json: the engine's script functions and constants.

Sources:
  - LegacyClonk engine source (C4Script.cpp): authoritative function names, parameter count/types, constants
  - lcdocs (sdk/script/fn/*.xml): parameter names, descriptions (German) ...
  - ... and lcdocs/en.po: English translations of those descriptions

Usage: python tools/gen_engine_api.py <path to LegacyClonk/src> [--lcdocs lcdocs] [--out server/src/core/engine_api.json]
"""
import argparse
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
    for name in re.findall(r'new C4AulEngineFuncParArray<\d+, \w+>\s*\{pEngine, "(\w+)"', s):
        funcs[name] = {"params": [{"name": f"criteria{i + 1}", "type": "array"} for i in range(10)], "rtype": "any"}
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
    """msgid -> msgstr from a gettext .po file (multi-line strings joined)."""
    out, cur, key = {}, None, None
    msgid = msgstr = None

    def unq(x):
        return json.loads(x) if x.startswith('"') else x

    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("msgid "):
            if msgid is not None and msgstr:
                out[msgid] = msgstr
            msgid, msgstr, cur = unq(line[6:]), "", "id"
        elif line.startswith("msgstr "):
            msgstr, cur = unq(line[7:]), "str"
        elif line.startswith('"'):
            if cur == "id":
                msgid += unq(line)
            elif cur == "str":
                msgstr += unq(line)
    if msgid is not None and msgstr:
        out[msgid] = msgstr
    return out


def inner_xml(el):
    """Element content as xml2po sees it: text + child tags, whitespace collapsed."""
    parts = [el.text or ""]
    for c in el:
        parts.append(ET.tostring(c, encoding="unicode"))
    return re.sub(r"\s+", " ", "".join(parts)).strip()


def to_markdown(x):
    x = re.sub(r"<funclink>(.*?)</funclink>", r"`\1`", x)
    x = re.sub(r"<code>(.*?)</code>", r"`\1`", x)
    x = re.sub(r"<emlink[^>]*>(.*?)</emlink>", r"\1", x)
    x = re.sub(r"<br\s*/>", "\n", x)
    x = re.sub(r"<[^>]+>", "", x)
    return x.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&").strip()


def parse_docs(lcdocs):
    po = parse_po(os.path.join(lcdocs, "en.po"))

    def tr(el):
        if el is None:
            return ""
        x = inner_xml(el)
        if x in po:
            return to_markdown(po[x])
        # xml2po replaces inline child elements by <placeholder-N/> in the msgid
        parts = [el.text or ""]
        for i, c in enumerate(el, 1):
            parts.append(f"<placeholder-{i}/>")
            parts.append(c.tail or "")
        key = re.sub(r"\s+", " ", "".join(parts)).strip()
        if key in po:
            out = po[key]
            for i, c in enumerate(el, 1):
                c_xml = ET.tostring(c, encoding="unicode")
                if c.tail:
                    c_xml = c_xml[: len(c_xml) - len(c.tail)]
                out = out.replace(f"<placeholder-{i}/>", c_xml)
            return to_markdown(out)
        return to_markdown(x)

    docs = {}
    for f in glob.glob(os.path.join(lcdocs, "sdk", "script", "fn", "*.xml")):
        raw = open(f, "rb").read().decode("latin-1")
        raw = re.sub(r"<!DOCTYPE[^>]*>", "", raw).replace('encoding="ISO-8859-1"', "")
        raw = re.sub(r"&(?!(lt|gt|amp|quot|apos);)\w+;", " ", raw)  # DTD entities like &nbsp;
        try:
            root = ET.fromstring(raw)
        except ET.ParseError as e:
            print("skip", f, e)
            continue
        for fn in root.iter("func"):
            title = (fn.findtext("title") or "").strip()
            syn = fn.find("syntax")
            params = []
            if syn is not None:
                for p in syn.iter("param"):
                    params.append({"name": (p.findtext("name") or "").strip(),
                                   "type": (p.findtext("type") or "any").strip(),
                                   "desc": tr(p.find("desc")),
                                   "optional": p.find("optional") is not None})
            docs[title] = {
                "rtype": (syn.findtext("rtype") if syn is not None else None) or "",
                "params": params,
                "desc": tr(fn.find("desc")),
                "remark": tr(fn.find("remark")),
                "version": (fn.findtext("version") or "").strip(),
                "category": (fn.findtext("category") or "").strip(),
            }
    return docs


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
    out_funcs = {}
    for name, f in sorted(funcs.items()):
        d = docs.get(name, {})
        params = f["params"]
        if params is not None and d.get("params"):
            # engine gives count/types; docs give nicer names and descriptions
            for i, p in enumerate(params):
                if i < len(d["params"]):
                    dp = d["params"][i]
                    p["name"] = dp["name"] or p["name"]
                    p["desc"] = dp["desc"]
                    if dp["type"]:
                        p["type"] = dp["type"]
        elif params is None and d.get("params"):
            params = [{"name": p["name"], "type": p["type"], "desc": p["desc"]} for p in d["params"]]
            f["params_from_docs_only"] = True
        out_funcs[name] = {
            "params": params,
            "rtype": d.get("rtype") or f["rtype"],
            "desc": d.get("desc", ""),
            "remark": d.get("remark", ""),
            "version": d.get("version", ""),
            "documented": name in docs,
            "arity_known": f["params"] is not None,
        }
    data = {"functions": out_funcs, "constants": consts,
            "documented_only": sorted(n for n in docs if n not in funcs)}
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    with open(a.out, "w", encoding="utf-8") as fh:
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
    with open(a.system_out, "w", encoding="utf-8") as fh:
        json.dump(scripts, fh, ensure_ascii=False, indent=0, sort_keys=True)
    print(f"{len(scripts)} System.c4g scripts -> {a.system_out}")
    print(f"{len(out_funcs)} engine functions ({sum(f['documented'] for f in out_funcs.values())} documented), "
          f"{len(consts)} constants, {len(data['documented_only'])} documented but not engine functions -> {a.out}")


if __name__ == "__main__":
    main()
