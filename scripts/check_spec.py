"""Checks api/openapi.yaml: structure, embedded examples, and the pages pattern."""
import re
import sys

import yaml
from jsonschema import Draft4Validator, RefResolver
from openapi_spec_validator import validate
from openapi_spec_validator.readers import read_from_filename

path = sys.argv[1]
doc = yaml.safe_load(open(path))
spec, _ = read_from_filename(path)
validate(spec)
print("structure: valid OpenAPI", doc["openapi"])
print("paths:", ", ".join(doc["paths"]))
schemas = doc["components"]["schemas"]
print("schemas:", len(schemas))

resolver = RefResolver.from_schema(doc)


def errors(schema_name, instance):
    ref = {"$ref": f"#/components/schemas/{schema_name}"}
    return [e.message for e in Draft4Validator(ref, resolver=resolver).iter_errors(instance)]


failed = False
for name in ("Manifest", "ConvertOptions"):
    errs = errors(name, schemas[name]["example"])
    print(f"example of {name}:", "ok" if not errs else errs)
    failed |= bool(errs)

for rname, resp in doc["components"]["responses"].items():
    content = resp["content"]["application/json"]
    examples = [content["example"]] if "example" in content else [
        v["value"] for v in content.get("examples", {}).values()
    ]
    for ex in examples:
        errs = errors("Error", ex)
        if errs:
            print("bad error example in", rname, errs)
            failed = True
print("error examples: checked")

pattern = re.compile(schemas["ConvertOptions"]["properties"]["pages"]["pattern"])
cases = [("1", True), ("1-5,8,10-", True), ("1-", True), ("-3", False), ("a", False), ("1,,2", False), ("", False)]
for text, want in cases:
    if bool(pattern.match(text)) != want:
        print("pages pattern wrong for", repr(text))
        failed = True
print("pages pattern: checked")

codes = set(schemas["ErrorCode"]["enum"])
mentioned = set(re.findall(r"`([A-Z_]{6,})`", doc["info"]["description"]))
print("codes named in the overview but missing from ErrorCode:", sorted(mentioned - codes) or "none")
failed |= bool(mentioned - codes)

sys.exit(1 if failed else 0)
