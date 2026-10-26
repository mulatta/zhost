"""Pinned wire invariants that cannot be established by coverage-copy agreement."""

import re

from models import Contract, Fixture
from pydantic import JsonValue

OBJECT_KEY = re.compile(r"[23456789ABCDEFGHIJKLMNPQRSTUVWXYZ]{8}")


class ContractError(Exception):
    """A cross-record invariant failed."""


def require(condition: bool, detail: str) -> None:
    if not condition:
        raise ContractError(detail)


def check_key(value: JsonValue, identity: str) -> None:
    require(
        isinstance(value, str) and OBJECT_KEY.fullmatch(value) is not None,
        f"{identity}: invalid object key",
    )


def check_references(value: JsonValue, identity: str) -> None:
    if isinstance(value, str):
        for key in re.findall(r"/(?:items|collections|searches)/([^/?#]+)", value):
            if key not in {"top", "trash", "tags", "new"}:
                check_key(key, identity)
    if isinstance(value, list):
        for entry in value:
            check_references(entry, identity)
    if isinstance(value, dict):
        for entry in value.values():
            check_references(entry, identity)


def check_data(value: JsonValue, identity: str) -> None:
    if isinstance(value, list):
        for entry in value:
            check_data(entry, identity)
    if isinstance(value, dict):
        for field, entry in value.items():
            if field in {"key", "parentItem", "parentCollection"}:
                if entry is not None and entry is not False:
                    check_key(entry, identity)
            elif field == "collections":
                if not isinstance(entry, list):
                    raise ContractError(f"{identity}: collection references")
                for key in entry:
                    check_key(key, identity)
            elif field in {"relations", "links"}:
                check_references(entry, identity)
            else:
                check_data(entry, identity)


def check_write_report(fixture: Fixture) -> None:
    """Bind every successful report index to its submitted object and changes."""
    identity = fixture.contract
    submitted = fixture.request.body
    report = fixture.response.body
    if not isinstance(submitted, list) or not isinstance(report, dict):
        raise ContractError(f"{identity}: bulk write shape")
    successful, success = report.get("successful"), report.get("success")
    unchanged, failed = report.get("unchanged"), report.get("failed")
    if not isinstance(successful, dict) or not isinstance(success, dict):
        raise ContractError(f"{identity}: successful maps")
    if not isinstance(unchanged, dict) or not isinstance(failed, dict):
        raise ContractError(f"{identity}: terminal maps")
    for key in unchanged.values():
        check_key(key, identity)
    require(success.keys() == successful.keys(), f"{identity}: successful index maps")
    require(
        not (
            successful.keys() & unchanged.keys()
            or successful.keys() & failed.keys()
            or unchanged.keys() & failed.keys()
        ),
        f"{identity}: overlapping write outcomes",
    )
    for index in successful.keys() | unchanged.keys() | failed.keys():
        require(
            index.isdecimal()
            and index == str(int(index))
            and int(index) < len(submitted),
            f"{identity}: submitted index",
        )
    header = fixture.response.headers.get("Last-Modified-Version", "")
    require(header.isdecimal(), f"{identity}: write version header")
    versions = []
    for index, wrapper in successful.items():
        item = submitted[int(index)]
        if not isinstance(item, dict) or not isinstance(wrapper, dict):
            raise ContractError(f"{identity}: successful object")
        data = wrapper.get("data")
        if not isinstance(data, dict):
            raise ContractError(f"{identity}: successful canonical data")
        key = wrapper.get("key")
        check_key(key, identity)
        require(
            success[index] == key == data.get("key"), f"{identity}: successful identity"
        )
        if "key" in item:
            require(item["key"] == key, f"{identity}: submitted identity")
        version = wrapper.get("version")
        if not isinstance(version, int) or isinstance(version, bool):
            raise ContractError(f"{identity}: successful version")
        require(
            version == data.get("version") and 0 < version <= int(header),
            f"{identity}: canonical version",
        )
        old_version = item.get("version", 0)
        require(
            isinstance(old_version, int) and version > old_version,
            f"{identity}: changed object version",
        )
        for field, value in item.items():
            if field != "version":
                require(
                    field in data and data[field] == value,
                    f"{identity}: submitted field {field}",
                )
        versions.append(version)
    if versions:
        require(max(versions) == int(header), f"{identity}: final write version")


def validate_wire(contract: Contract, fixture: Fixture) -> None:
    """Check concrete keys, not arbitrary user prose or API credential tokens."""
    identity = contract.id
    request, response = fixture.request, fixture.response
    if contract.format == "keys" and 200 <= response.status < 300:
        mime = (
            response.headers.get("Content-Type", "").partition(";")[0].strip().lower()
        )
        require(
            contract.envelope == "keys" and mime == "text/plain",
            f"{identity}: keys wire representation",
        )
    check_references(request.path, identity)
    for field in ("itemKey", "collectionKey", "searchKey"):
        if field in request.query:
            value = request.query[field]
            values = value.split(",") if isinstance(value, str) else value
            if not isinstance(values, list):
                raise ContractError(f"{identity}: object selection")
            for key in values:
                check_key(key, identity)
    path = request.path.removeprefix("/xotero/web/api")
    library_data = re.match(
        r"/(?:users|groups)/\d+/(?:items|collections|searches|fulltext|deleted)(?:/|$)",
        path,
    )
    if library_data:
        check_data(request.body, identity)
        check_data(response.body, identity)
        if contract.envelope == "versions" and isinstance(response.body, dict):
            for key in response.body:
                check_key(key, identity)
        if contract.envelope == "keys" and isinstance(response.body, str):
            for key in response.body.splitlines():
                check_key(key, identity)
        if contract.envelope == "deleted" and isinstance(response.body, dict):
            for field in ("items", "collections", "searches"):
                deleted_keys = response.body.get(field)
                if not isinstance(deleted_keys, list):
                    raise ContractError(f"{identity}: deleted object keys")
                for key in deleted_keys:
                    check_key(key, identity)
    if contract.envelope == "write-report":
        check_write_report(fixture)
