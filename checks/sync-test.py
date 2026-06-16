"""End-to-end coverage of the zhost sync API, driven over HTTP against the
nixosModule-deployed service. Grouped by flow rather than per endpoint, since
each run boots a VM."""

base = "http://localhost:8189"
auth = "-H 'Zotero-API-Key: testtoken' -H 'Zotero-API-Version: 3'"


def http_code(args):
    return machine.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' {args}").strip()


machine.wait_for_unit("postgresql.service")
machine.wait_for_unit("zhost.service")
machine.wait_for_open_port(8189)

with subtest("the module deploys a working service backed by postgres"):
    assert http_code(f"{base}/keys/current {auth}") == "200"

with subtest("login session hands out the configured key"):
    machine.succeed(f"curl -sf -X POST {base}/keys/sessions -d '{{}}' | jq -e .sessionToken")
    machine.succeed(
        f"curl -sf {base}/keys/sessions/zhost-session | jq -e '.status == \"completed\"'"
    )

with subtest("the api key is required off the bootstrap paths"):
    assert http_code(f"{base}/keys/current -H 'Zotero-API-Version: 3'") == "403"

with subtest("an item round-trips through write and read"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: 0' "
        f"-d '[{{\"key\":\"ITEM0001\",\"itemType\":\"book\",\"title\":\"t\"}}]' "
        f"| jq -e '.successful.\"0\".key == \"ITEM0001\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?format=versions&since=0' {auth} | jq -e '.ITEM0001'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ITEM0001&format=json' {auth} "
        f"| jq -e '.[0].data.itemType == \"book\"'"
    )

with subtest("a stale write is rejected with 412"):
    assert (
        http_code(
            f"-X POST {base}/users/1/items {auth} "
            f"-H 'If-Unmodified-Since-Version: 0' "
            f"-d '[{{\"key\":\"ITEMSTAL\",\"itemType\":\"book\"}}]'"
        )
        == "412"
    )

with subtest("attachment data is emitted with linkMode first"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-d '[{{\"key\":\"ATTACH001\",\"itemType\":\"attachment\","
        f"\"linkMode\":\"imported_file\",\"filename\":\"t.pdf\","
        f"\"contentType\":\"application/pdf\"}}]' | jq -e .successful"
    )
    first_key = machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ATTACH001&format=json' {auth} "
        f"| jq -r '.[0].data | keys_unsorted[0]'"
    ).strip()
    assert first_key == "linkMode", first_key

with subtest("an attachment file uploads and downloads"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/ATTACH001/file {auth} "
        f"-H 'If-None-Match: *' "
        f"-d 'md5=abc&filename=t.pdf&filesize=5&mtime=1700000000000' "
        f"| jq -e '.uploadKey == \"ATTACH001\"'"
    )
    machine.succeed(f"printf hello | curl -sf -X POST {base}/uploads/ATTACH001 --data-binary @-")
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/ATTACH001/file {auth} "
        f"-H 'If-None-Match: *' -d 'upload=ATTACH001'"
    )
    assert http_code(f"{base}/users/1/items/ATTACH001/file {auth}") == "302"
    machine.succeed(
        f"curl -sf -D - -o /dev/null {base}/users/1/items/ATTACH001/file {auth} "
        f"| grep -i 'zotero-file-md5: abc'"
    )
    assert machine.succeed(f"curl -sf {base}/files/ATTACH001") == "hello"

with subtest("deletes are recorded in the deletion log"):
    machine.succeed(f"curl -sf -X DELETE '{base}/users/1/items?itemKey=ITEM0001' {auth}")
    machine.succeed(
        f"curl -sf '{base}/users/1/deleted?since=0' {auth} | jq -e '.items | index(\"ITEM0001\")'"
    )
