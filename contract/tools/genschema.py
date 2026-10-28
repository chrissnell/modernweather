#!/usr/bin/env python3
"""Derive the machine-readable legacy contract from the Go server source, the Swift client
source, and the captured fixtures. Nothing here is hand-transcribed."""
import json, os, re, subprocess, sys, glob
import endpoints as EP

# Both repositories are read-only reference checkouts. Override with RW_GO_REPO / RW_IOS_REPO.
GO_REPO = os.environ.get("RW_GO_REPO", "../remoteweather")
IOS_REPO = os.environ.get("RW_IOS_REPO", "../RemoteWeather.app")
GO = os.path.join(GO_REPO, "internal/controllers/restserver")
IOS = os.path.join(IOS_REPO, "RemoteWeather/Sources/RemoteWeather")


def git_head(repo):
    try:
        return subprocess.check_output(["git", "-C", repo, "rev-parse", "HEAD"], text=True).strip()
    except Exception as e:
        sys.exit(f"cannot read HEAD of {repo}: {e}")

GO_JSON_TO_SCHEMA = {
    "float32": "number", "float64": "number", "int": "number", "int16": "number",
    "int64": "number", "*int": "number", "string": "string", "*string": "string",
    "bool": "boolean", "time.Time": "string", "*time.Time": "string",
}


def parse_go_structs(path):
    src = open(path).read()
    out = {}
    for m in re.finditer(r"\ntype (\w+) struct \{(.*?)\n\}", src, re.S):
        name, body = m.group(1), m.group(2)
        fields = []
        for line in body.splitlines():
            line = line.split("//")[0].rstrip()
            fm = re.match(r"\s*(\w+)\s+([\w\.\*\[\]]+)\s+`.*?json:\"([^\"]+)\"", line)
            if not fm:
                continue
            gofield, gotype, tag = fm.groups()
            parts = tag.split(",")
            jname = parts[0]
            fields.append({
                "json": jname,
                "go_field": gofield,
                "go_type": gotype,
                "json_type": GO_JSON_TO_SCHEMA.get(gotype, "object"),
                "omitempty": "omitempty" in parts[1:],
                "nullable": gotype.startswith("*"),
            })
        if fields:
            out[name] = fields
    return out


def parse_swift_structs(path):
    src = open(path).read()
    out = {}
    for m in re.finditer(r"(?:public )?struct (\w+)\s*:[^\{]*\{(.*?)\n\}", src, re.S):
        name, body = m.group(1), m.group(2)
        coding = dict(re.findall(r"case (\w+)\s*=\s*\"([^\"]+)\"", body))
        props = []
        for pm in re.finditer(r"^\s*(?:public )?let (\w+)\s*:\s*([^\n/]+)", body, re.M):
            prop, ty = pm.group(1), pm.group(2).strip()
            props.append({
                "swift_property": prop,
                "json": coding.get(prop, prop),
                "swift_type": ty,
                "optional": ty.endswith("?"),
            })
        if props:
            out[name] = props
    return out


go = {}
go.update(parse_go_structs(os.path.join(GO, "types.go")))
go.update(parse_go_structs(os.path.join(GO, "handlers.go")))
sw = {}
sw.update(parse_swift_structs(os.path.join(IOS, "Data/REST/WeatherResponse.swift")))
sw.update(parse_swift_structs(os.path.join(IOS, "Domain/Entities/StationInfo.swift")))

# -- Hazard table: {Go omitempty} INTERSECT {Swift non-optional}, per response type.
PAIRS = [
    ("WeatherReading", "WeatherResponse", ["/latest", "/span/{span}"]),
    ("SnowReading", "SnowResponse", ["/snow"]),
    ("StationInfoResponse", "StationInfo", ["/stationinfo"]),
    ("StationInfoItem", "Station", ["/stationinfo"]),
]
hazards, cross = [], []
for gname, sname, endpoints in PAIRS:
    gf = {f["json"]: f for f in go.get(gname, [])}
    sp = {p["json"]: p for p in sw.get(sname, [])}
    for key, f in gf.items():
        if key not in sp:
            continue
        p = sp[key]
        row = {
            "json": key, "endpoints": endpoints,
            "go_type": gname + "." + f["go_field"] + " " + f["go_type"],
            "go_omitempty": f["omitempty"],
            "swift_type": sname + "." + p["swift_property"] + ": " + p["swift_type"],
            "swift_optional": p["optional"],
        }
        cross.append(row)
        if f["omitempty"] and not p["optional"]:
            row["effect"] = ("Key is absent whenever the Go value is the zero value, and the Swift "
                             "property is non-optional, so JSONDecoder throws keyNotFound and the "
                             "ENTIRE %s decode fails." % sname)
            hazards.append(row)

counts = {
    "go_json_tags_types_go": sum(1 for n, f in go.items() for _ in f
                                 if n in ("AerisWeatherForecastRecord", "AerisWeatherAlertRecord",
                                          "WeatherReading", "SnowReading", "SnowSeasonReading",
                                          "StationData", "StationWebsiteData", "StationInfoItem",
                                          "StationInfoResponse", "Alert")),
    "go_json_tags_all": sum(len(f) for f in go.values()),
    "go_omitempty_all": sum(1 for f in go.values() for x in f if x["omitempty"]),
    "swift_properties": sum(len(p) for p in sw.values()),
    "swift_non_optional": sum(1 for p in sw.values() for x in p if not x["optional"]),
}

FIXTURE_DIR = os.environ.get("RW_FIXTURE_DIR", "fixtures")
fixtures = []
for f in sorted(glob.glob(os.path.join(FIXTURE_DIR, "*.json"))):
    d = json.load(open(f))
    fixtures.append({
        "name": d["name"],
        "file": "fixtures/" + os.path.basename(f),
        "request": d["request"],
        "status": d["response"]["status"],
        "body_kind": d["response"]["body_kind"],
        "description": d["description"],
    })

doc = {
    "contract": "remoteweather-legacy-rest",
    "version": "legacy-v1",
    "frozen": True,
    "captured_from": {
        "repository": "git@github.com:chrissnell/remoteweather.git",
        "commit": git_head(GO_REPO),
        "server_version": os.environ.get("RW_SERVER_VERSION", "unknown"),
        "database": os.environ.get("RW_DATABASE_VERSION", "unknown"),
    },
    "clients": {
        "ios": {
            "repository": "git@github.com:chrissnell/RemoteWeather.app.git",
            "commit": git_head(IOS_REPO),
            "decoder": "Foundation.JSONDecoder, default key strategy (match by member name)",
            "endpoints": ["/latest", "/span/{hours}h", "/snow", "/stationinfo", "/forecast/{hours}"],
        },
        "website": {
            "source": "internal/controllers/restserver/assets/js/ in the Go repository",
            "decoder": "Response.json(), structurally tolerant (every read is guarded)",
            "endpoints": ["/latest", "/span/{hours}h", "/snow", "/stationinfo", "/forecast/{hours}",
                          "/almanac", "/api/stations", "/", "/portal", "/privacy", "/support",
                          "/js/", "/css/", "/fonts/", "/images/"],
        },
    },
    "transport": EP.COMMON,
    "endpoints": EP.ENDPOINTS,
    "quirks": EP.QUIRKS,
    "counts": counts,
    "go_response_types": go,
    "go_response_types_not_package_level": EP.EXTRA_TYPES,
    "swift_response_types": sw,
    "client_field_crosswalk": cross,
    "hazards": hazards,
    "fixtures": fixtures,
}
json.dump(doc, open(sys.argv[1], "w"), indent=2)
open(sys.argv[1], "a").write("\n")
print("go structs:", ", ".join(sorted(go)))
print("swift structs:", ", ".join(sorted(sw)))
print("counts:", json.dumps(counts))
print("hazards:", json.dumps([h["json"] for h in hazards]))
print("crosswalk rows:", len(cross))
