#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["openapi-spec-validator==0.7.2"]
# ///
"""Check that crates/straum-http/openapi.json is a valid OpenAPI 3.1
document. Whether the replies match it is straum-http's tests' job; this
checks the document itself, so tools that read it accept it."""

import json
import pathlib
import sys

from openapi_spec_validator import OpenAPIV31SpecValidator
from referencing.exceptions import Unresolvable

ROOT = pathlib.Path(__file__).resolve().parents[1]
DOCUMENT = ROOT / "crates" / "straum-http" / "openapi.json"


def errors(document: dict) -> list[str]:
    """Each way the document breaks OpenAPI 3.1, a line each."""
    found = []
    # A $ref that points at nothing ends the walk with an exception
    # rather than an error in the list.
    try:
        for error in OpenAPIV31SpecValidator(document).iter_errors():
            path = "/".join(str(part) for part in error.absolute_path)
            found.append(f"{path or '(root)'}: {error.message}")
    except Unresolvable as error:
        found.append(f"a $ref points at nothing: {error}")
    return found


def main() -> int:
    found = errors(json.loads(DOCUMENT.read_text()))
    for line in found:
        print(f"check-openapi: {line}", file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
