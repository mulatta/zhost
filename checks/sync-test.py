"""End-to-end coverage of the zhost sync API, driven over HTTP against the
nixosModule-deployed service. Grouped by flow rather than per endpoint, since
each run boots a VM."""

base = "http://localhost:8189"
auth = "-H 'Zotero-API-Key: testtoken' -H 'Zotero-API-Version: 3'"
readonly = "-H 'Zotero-API-Key: readonlytoken' -H 'Zotero-API-Version: 3'"


def http_code(args):
    return machine.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' {args}").strip()


def library_version():
    return machine.succeed(
        f"curl -sf -D - -o /dev/null '{base}/users/1/items?format=versions&since=0' {auth} "
        f"| grep -i last-modified-version | tr -d '\\r' | cut -d' ' -f2"
    ).strip()


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

with subtest("a read-only key reads but cannot write"):
    assert http_code(f"'{base}/users/1/items?format=versions&since=0' {readonly}") == "200"
    machine.succeed(f"curl -sf {base}/keys/current {readonly} | jq -e '.access.user.write == false'")
    machine.succeed(f"curl -sf {base}/keys/current {auth} | jq -e '.access.user.write == true'")
    assert (
        http_code(
            f"-X POST {base}/users/1/items {readonly} "
            f"-H 'If-Unmodified-Since-Version: 0' "
            f"-d '[{{\"key\":\"RONLY001\",\"itemType\":\"book\"}}]'"
        )
        == "403"
    )
    assert http_code(f"-X DELETE '{base}/users/1/items?itemKey=ITEM0001' {readonly}") == "403"

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

with subtest("annotations round-trip as ordinary items"):
    # Highlights/notes are items (itemType annotation/note), so they sync through
    # the same opaque object path rather than any dedicated endpoint.
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-d '[{{\"key\":\"ANNOT001\",\"itemType\":\"annotation\","
        f"\"annotationType\":\"highlight\",\"annotationText\":\"marked\","
        f"\"parentItem\":\"ATTACH001\"}}]' "
        f"| jq -e '.successful.\"0\".data.annotationText == \"marked\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ANNOT001&format=json' {auth} "
        f"| jq -e '.[0].data.annotationType == \"highlight\"'"
    )

with subtest("full-text content uploads, lists versions, and downloads"):
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/fulltext {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f"-d '[{{\"key\":\"ATTACH001\",\"content\":\"hello world\","
        f"\"indexedChars\":11,\"totalChars\":11,\"indexedPages\":1,\"totalPages\":1}}]' "
        f"| jq -e '.successful.\"0\".key == \"ATTACH001\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/fulltext?format=versions&since=0' {auth} | jq -e '.ATTACH001'"
    )
    # The per-item version must equal the value in the versions map, else the
    # client re-downloads content it already holds every sync.
    item_v = machine.succeed(
        f"curl -sf -D - -o /dev/null '{base}/users/1/items/ATTACH001/fulltext' {auth} "
        f"| grep -i last-modified-version | tr -d '\\r' | cut -d' ' -f2"
    ).strip()
    list_v = machine.succeed(
        f"curl -sf '{base}/users/1/fulltext?format=versions&since=0' {auth} | jq -r '.ATTACH001'"
    ).strip()
    assert item_v == list_v, f"{item_v} != {list_v}"
    machine.succeed(
        f"curl -sf '{base}/users/1/items/ATTACH001/fulltext' {auth} "
        f"| jq -e '.content == \"hello world\" and .totalPages == 1'"
    )

with subtest("a stale full-text write is rejected with 412"):
    assert (
        http_code(
            f"-X POST {base}/users/1/fulltext {auth} "
            f"-H 'If-Unmodified-Since-Version: 0' "
            f"-d '[{{\"key\":\"ATTACH001\",\"content\":\"x\"}}]'"
        )
        == "412"
    )

with subtest("the query API filters, searches, sorts and paginates items"):
    # Seed a couple of distinct items to query over.
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"QBOOK001","itemType":"book","title":"Borrow Checker",'
        f'"creators":[{{"lastName":"Klabnik","firstName":"Steve"}}],'
        f'"tags":[{{"tag":"rustlang"}},{{"tag":"systems"}}]}},'
        f'{{"key":"QART0001","itemType":"journalArticle","title":"Unrelated",'
        f'"tags":[{{"tag":"rustlang"}}]}}]\' | jq -e .successful'
    )

    def keys(qs):
        return machine.succeed(
            f"curl -sf '{base}/users/1/items?{qs}' {auth} | jq -r '[.[].key]|sort|join(\",\")'"
        ).strip()

    # Title and creator search (titleCreatorYear, the default qmode).
    assert keys("q=borrow") == "QBOOK001", keys("q=borrow")
    assert keys("q=klabnik") == "QBOOK001", keys("q=klabnik")
    # Type filter, including negation.
    assert "QBOOK001" in keys("itemType=book")
    assert "QART0001" not in keys("itemType=book")
    assert "QART0001" in keys("itemType=-book")
    # Tags: repeated key is AND, only the book carries both.
    assert keys("tag=rustlang&tag=systems") == "QBOOK001"
    assert keys("tag=rustlang") == "QART0001,QBOOK001"
    # Full-text only matches under qmode=everything (ATTACH001 holds "hello world").
    assert keys("q=world") == ""
    assert keys("q=world&qmode=everything") == "ATTACH001"

with subtest("the query API reports Total-Results and a next-page Link"):
    # Header names come back lower-cased over HTTP/1.1, so match case-insensitively.
    headers = machine.succeed(
        f"curl -sf -D - -o /dev/null '{base}/users/1/items?limit=1&sort=title&direction=asc' {auth}"
    ).lower()
    assert "total-results:" in headers, headers
    assert 'rel="next"' in headers, headers
    # The Link points at the public URL, not the internal bind address.
    assert "localhost:8189" in headers, headers

with subtest("a read-only key can drive the query API"):
    assert (
        http_code(f"'{base}/users/1/items?q=borrow' {readonly}") == "200"
    )

with subtest("convenience listings: top, trash, collection items and tags"):
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"TOPITEM1","itemType":"book","title":"Shelf Book",'
        f'"collections":["COLLXXXX"],"tags":[{{"tag":"shelf"}}]}},'
        f'{{"key":"CHILDNO1","itemType":"note","note":"child","parentItem":"TOPITEM1"}},'
        f'{{"key":"TRASHED1","itemType":"book","title":"Gone","deleted":1}}]\' '
        f"| jq -e .successful"
    )

    def path_keys(p):
        return machine.succeed(
            f"curl -sf '{base}{p}' {auth} | jq -r '[.[].key]|sort|join(\",\")'"
        ).strip()

    # Top excludes children (have parentItem) and trashed items.
    top = path_keys("/users/1/items/top")
    assert "TOPITEM1" in top, top
    assert "CHILDNO1" not in top, top
    assert "TRASHED1" not in top, top
    # Trash returns only trashed items.
    trash = path_keys("/users/1/items/trash")
    assert trash == "TRASHED1", trash
    # Collection membership.
    assert path_keys("/users/1/collections/COLLXXXX/items") == "TOPITEM1"
    # Tags lists distinct tags with counts; the trashed item's tag is excluded.
    machine.succeed(
        f"curl -sf {base}/users/1/tags {auth} "
        f"| jq -e '.[] | select(.tag == \"shelf\") | .numItems == 1'"
    )

with subtest("deletes are recorded in the deletion log"):
    machine.succeed(f"curl -sf -X DELETE '{base}/users/1/items?itemKey=ITEM0001' {auth}")
    machine.succeed(
        f"curl -sf '{base}/users/1/deleted?since=0' {auth} | jq -e '.items | index(\"ITEM0001\")'"
    )
