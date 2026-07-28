#!/usr/bin/env python3
"""Converts pinned CustomResourceDefinitions into kubeconform JSON schemas.

kubeconform validates built-in Kubernetes kinds from its bundled schema store
but knows nothing about custom resources. Without a schema for each CRD the only
way to make validation pass is `-ignore-missing-schemas`, which turns every
custom resource in the tree into an unchecked blob -- and MOA's most
structurally complex manifests, the Restate cluster and deployment, are exactly
the custom resources that would go unchecked.

Emitted layout matches kubeconform's conventional schema-location template
`{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json`, with the kind
lowercased, so one `-schema-location` covers every vendored CRD.

Run through refresh.sh, which verifies the upstream checksums first.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from typing import Any

import yaml

# Kubernetes structural-schema annotations. They are meaningful to the API
# server and meaningless to a JSON Schema validator, which rejects them as
# unknown keywords under a strict draft.
KUBERNETES_EXTENSIONS = (
    "x-kubernetes-int-or-string",
    "x-kubernetes-preserve-unknown-fields",
    "x-kubernetes-embedded-resource",
    "x-kubernetes-list-type",
    "x-kubernetes-list-map-keys",
    "x-kubernetes-map-type",
    "x-kubernetes-validations",
    "x-kubernetes-patch-strategy",
    "x-kubernetes-patch-merge-key",
)


def convert(node: Any, preserve_unknown: bool = False) -> Any:
    """Rewrites one CRD schema node into a strict JSON Schema node.

    `preserve_unknown` propagates `x-kubernetes-preserve-unknown-fields`, which
    is the API server's way of saying "this subtree is free-form". Forcing
    `additionalProperties: false` onto such a subtree would reject documents the
    cluster accepts, so those nodes stay open.
    """
    if isinstance(node, list):
        return [convert(item) for item in node]
    if not isinstance(node, dict):
        return node

    open_subtree = preserve_unknown or bool(
        node.get("x-kubernetes-preserve-unknown-fields")
    )
    int_or_string = bool(node.get("x-kubernetes-int-or-string"))

    converted: dict[str, Any] = {}
    for key, value in node.items():
        if key in KUBERNETES_EXTENSIONS:
            continue
        if key == "properties" and isinstance(value, dict):
            converted[key] = {
                name: convert(child, open_subtree) for name, child in value.items()
            }
        elif key in ("items", "additionalProperties") and isinstance(value, dict):
            converted[key] = convert(value, open_subtree)
        elif key in ("allOf", "anyOf", "oneOf") and isinstance(value, list):
            converted[key] = [convert(item, open_subtree) for item in value]
        elif key == "not" and isinstance(value, dict):
            converted[key] = convert(value, open_subtree)
        else:
            converted[key] = convert(value, open_subtree)

    if int_or_string:
        # The API server accepts either form here; a schema declaring only one
        # would reject valid manifests.
        converted.pop("type", None)
        converted["oneOf"] = [{"type": "string"}, {"type": "integer"}]
        return converted

    # Strict mode is the entire point of vendoring these: a misspelled field in
    # a RestateDeployment is silently ignored by kustomize and rejected here.
    if (
        converted.get("type") == "object"
        and "properties" in converted
        and "additionalProperties" not in converted
        and not open_subtree
    ):
        converted["additionalProperties"] = False

    return converted


def schema_for_version(crd: dict[str, Any], version: dict[str, Any]) -> dict[str, Any]:
    """Builds the top-level schema document for one served CRD version."""
    body = version.get("schema", {}).get("openAPIV3Schema")
    if body is None:
        raise SystemExit(
            f"CRD {crd['metadata']['name']} version {version['name']} has no schema"
        )
    schema = convert(body)
    schema.setdefault("type", "object")
    properties = schema.setdefault("properties", {})
    # controller-gen omits these on some CRDs; kubeconform needs them present or
    # a manifest naming the wrong apiVersion passes.
    properties.setdefault("apiVersion", {"type": "string"})
    properties.setdefault("kind", {"type": "string"})
    # `metadata` is overridden, not defaulted. controller-gen emits a partial
    # ObjectMeta (often just `name`) as documentation; the API server validates
    # metadata with its own machinery and ignores what the CRD says. Taking the
    # CRD's version literally under strict mode rejects every manifest carrying
    # an ordinary field like `namespace` or `labels`.
    properties["metadata"] = {"type": "object"}
    schema["additionalProperties"] = False
    schema["$schema"] = "http://json-schema.org/draft-07/schema#"
    return schema


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sources", required=True, type=pathlib.Path)
    parser.add_argument("--downloads", required=True, type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    args = parser.parse_args()

    sources = json.loads(args.sources.read_text(encoding="utf-8"))
    written: list[str] = []

    for entry in sources["crds"]:
        path = args.downloads / f"{entry['name']}.yaml"
        documents = [
            document
            for document in yaml.safe_load_all(path.read_text(encoding="utf-8"))
            if document and document.get("kind") == "CustomResourceDefinition"
        ]
        if len(documents) != 1:
            raise SystemExit(
                f"{path} holds {len(documents)} CustomResourceDefinition documents, expected 1"
            )
        crd = documents[0]

        group = crd["spec"]["group"]
        kind = crd["spec"]["names"]["kind"]
        if group != entry["expect_group"] or kind != entry["expect_kind"]:
            raise SystemExit(
                f"{path} declares {group}/{kind}, expected "
                f"{entry['expect_group']}/{entry['expect_kind']}"
            )
        served = [version["name"] for version in crd["spec"]["versions"]]
        if served != entry["expect_versions"]:
            raise SystemExit(
                f"{path} serves versions {served}, expected {entry['expect_versions']}; "
                "the pinned upstream changed its API surface"
            )

        for version in crd["spec"]["versions"]:
            target = args.out / group / f"{kind.lower()}_{version['name']}.json"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(
                json.dumps(schema_for_version(crd, version), indent=2, sort_keys=True)
                + "\n",
                encoding="utf-8",
            )
            written.append(str(target.relative_to(args.out)))

    for name in written:
        print(f"wrote {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
