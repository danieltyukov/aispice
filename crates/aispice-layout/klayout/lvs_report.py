# Written by aispice. Reads a KLayout LVS database and writes what matched
# and what did not as JSON. Inputs come as -rd variables: `db` (the .lvsdb)
# and `out` (the JSON file to write). Nothing here is built from them.
import json

import pya

STATUS = {
    pya.NetlistCrossReference.Match: "match",
    pya.NetlistCrossReference.MatchWithWarning: "warning",
    pya.NetlistCrossReference.Mismatch: "mismatch",
    pya.NetlistCrossReference.NoMatch: "nomatch",
    pya.NetlistCrossReference.Skipped: "skipped",
}


def name(obj):
    return None if obj is None else obj.expanded_name()


def status(s):
    return STATUS.get(s, "none")


lvs = pya.LayoutVsSchematic()
lvs.read(db)  # noqa: F821 (set by -rd)
xref = lvs.xref()
report = {"circuits": [], "log": []}
if xref is not None:
    for cp in xref.each_circuit_pair():
        a, b = cp.first(), cp.second()
        c = {
            "layout": a.name if a else None,
            "schematic": b.name if b else None,
            "status": status(cp.status()),
            "nets": [],
            "devices": [],
            "pins": [],
        }
        for p in xref.each_net_pair(cp):
            c["nets"].append({"layout": name(p.first()), "schematic": name(p.second()), "status": status(p.status())})
        for p in xref.each_device_pair(cp):
            c["devices"].append({"layout": name(p.first()), "schematic": name(p.second()), "status": status(p.status())})
        for p in xref.each_pin_pair(cp):
            c["pins"].append({"layout": name(p.first()), "schematic": name(p.second()), "status": status(p.status())})
        report["circuits"].append(c)
for entry in lvs.each_log_entry():
    report["log"].append(entry.message)
with open(out, "w") as f:  # noqa: F821 (set by -rd)
    json.dump(report, f)
