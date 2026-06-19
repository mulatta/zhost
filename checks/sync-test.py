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
machine.wait_for_unit("rustfs.service")
machine.wait_for_open_port(9000)
machine.wait_for_unit("zhost.service")
machine.wait_for_open_port(8189)

# Create the attachment bucket in the S3 store before any upload.
machine.succeed("mc alias set s3 http://localhost:9000 rustfsadmin rustfsadmin")
machine.succeed("mc mb s3/zotero")

with subtest("the module deploys a working service backed by postgres"):
    assert http_code(f"{base}/keys/current {auth}") == "200"

# The front SSO proxy (oauth2-proxy/kanidm) forwards this header; loginAuthorizedUser
# is set to it, so the consent POST must carry it to authorize.
sso = "-H 'X-Auth-Request-Email: owner@mulatta.io'"

with subtest("login session hands out the key only after authorization"):
    machine.succeed(f"curl -sf -X POST {base}/keys/sessions -d '{{}}' > /tmp/sess")
    token = machine.succeed("jq -r .sessionToken /tmp/sess").strip()
    login_url = machine.succeed("jq -r .loginURL /tmp/sess").strip()
    assert token and f"session={token}" in login_url, login_url
    # GET /login renders a consent page only — it does NOT authorize, so a
    # prefetch or cross-site GET can't silently grant the key.
    machine.succeed(f"curl -sf '{login_url}' | grep -qi 'Authorize'")
    machine.succeed(f"curl -sf {base}/keys/sessions/{token} | jq -e '.status == \"pending\"'")
    machine.succeed(f"curl -sf {base}/keys/sessions/{token} | jq -e '.apiKey == null'")
    # A cross-site form POST (foreign Origin) is refused even with a valid identity.
    assert (
        http_code(f"-X POST {base}/login -H 'Origin: https://evil.example' {sso} -d 'session={token}'")
        == "403"
    )
    # Without the SSO identity the front proxy forwards, the POST is refused.
    assert http_code(f"-X POST {base}/login -d 'session={token}'") == "403"
    # A non-authorized identity is refused.
    assert (
        http_code(f"-X POST {base}/login -H 'X-Auth-Request-Email: someone@else' -d 'session={token}'")
        == "403"
    )
    # The consent POST (Approve) with the authorized identity completes the flow.
    machine.succeed(f"curl -sf -X POST {base}/login {sso} -d 'session={token}'")
    machine.succeed(f"curl -sf {base}/keys/sessions/{token} | jq -e '.status == \"completed\"'")
    machine.succeed(f"curl -sf {base}/keys/sessions/{token} | jq -e '.apiKey == \"testtoken\"'")
    # A canceled session no longer hands out the key.
    machine.succeed(f"curl -sf -X DELETE {base}/keys/sessions/{token}")
    machine.succeed(f"curl -sf {base}/keys/sessions/{token} | jq -e '.status == \"pending\"'")

with subtest("the login consent endpoint validates the session token"):
    assert http_code(f"{base}/login") == "400"  # missing ?session
    assert http_code(f"{base}/login?session=nope") == "404"  # unknown session
    assert http_code(f"-X POST {base}/login {sso} -d 'session=nope'") == "404"

with subtest("groups endpoint returns an empty set (single personal library)"):
    machine.succeed(f"curl -sf {base}/users/1/groups {auth} | jq -e '. == {{}}'")

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
            f"-d '[{{\"key\":\"RDNLY222\",\"itemType\":\"book\"}}]'"
        )
        == "403"
    )
    assert http_code(f"-X DELETE '{base}/users/1/items?itemKey=ITEM2223' {readonly}") == "403"

with subtest("an item round-trips through write and read"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: 0' "
        f"-d '[{{\"key\":\"ITEM2223\",\"itemType\":\"book\",\"title\":\"t\"}}]' "
        f"| jq -e '.successful.\"0\".key == \"ITEM2223\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?format=versions&since=0' {auth} | jq -e '.ITEM2223'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ITEM2223&format=json' {auth} "
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

with subtest("a write without If-Unmodified-Since-Version is rejected with 428"):
    # Without the precondition the version guard would be bypassed.
    assert (
        http_code(
            f"-X POST {base}/users/1/items {auth} "
            f"-d '[{{\"key\":\"NPRECN22\",\"itemType\":\"book\"}}]'"
        )
        == "428"
    )

with subtest("a malformed object key is rejected with 400"):
    # The client refuses keys outside the 8-char base32 alphabet (no 0/1/O),
    # queuing them with "key is not valid"; reject them at the API instead.
    for bad in ["HASOABC2", "HAS0ABC2", "SHORTKY", "TOOLONGKY", "lowercs2"]:
        v = library_version()
        assert (
            http_code(
                f"-X POST {base}/users/1/items {auth} "
                f"-H 'If-Unmodified-Since-Version: {v}' "
                f'-d \'[{{"key":"{bad}","itemType":"book"}}]\''
            )
            == "400"
        ), bad
    # The version did not move (nothing was written).
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=HASOABC2&format=json' {auth} | jq -e '. == []'"
    )

with subtest("an empty batch does not bump the library version"):
    before = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {before}' -d '[]' | jq -e .successful"
    )
    assert library_version() == before, library_version()

with subtest("attachment data is emitted with linkMode first"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f"-d '[{{\"key\":\"ATTACH22\",\"itemType\":\"attachment\","
        f"\"linkMode\":\"imported_file\",\"filename\":\"t.pdf\","
        f"\"contentType\":\"application/pdf\"}}]' | jq -e .successful"
    )
    first_key = machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ATTACH22&format=json' {auth} "
        f"| jq -r '.[0].data | keys_unsorted[0]'"
    ).strip()
    assert first_key == "linkMode", first_key

with subtest("an attachment file uploads to S3 and downloads via a presigned URL"):
    # md5 of "hello"; the upload endpoint verifies the bytes against it.
    md5 = "5d41402abc4b2a76b9719d911017c592"
    # Authorization returns an unguessable upload token (not the item key).
    token = machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/ATTACH22/file {auth} "
        f"-H 'If-None-Match: *' "
        f"-d 'md5={md5}&filename=t.pdf&filesize=5&mtime=1700000000000' "
        f"| jq -r .uploadKey"
    ).strip()
    assert token and token != "ATTACH22", token
    # Bytes are POSTed to the token URL (zhost verifies them and PUTs to S3);
    # registration then commits via the token.
    machine.succeed(f"printf hello | curl -sf -X POST {base}/uploads/{token} --data-binary @-")
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/ATTACH22/file {auth} "
        f"-H 'If-None-Match: *' -d 'upload={token}'"
    )
    assert http_code(f"{base}/users/1/items/ATTACH22/file {auth}") == "302"
    # The 302 carries the md5/mtime headers and a Location that is a pre-signed
    # GET URL pointing straight at the S3 store; follow it (no API key) to read
    # the bytes, the way the stock client downloads from a presigned URL.
    location = machine.succeed(
        f"curl -sf -D /tmp/dlhdr -o /dev/null {base}/users/1/items/ATTACH22/file {auth} "
        f"&& grep -i '^location:' /tmp/dlhdr | tr -d '\\r' | awk '{{print $2}}'"
    ).strip()
    machine.succeed(f"grep -iq 'zotero-file-md5: {md5}' /tmp/dlhdr")
    # The redirect targets the object store directly, not this server.
    assert ":9000" in location and "8189" not in location, location
    assert machine.succeed(f"curl -sf '{location}'").strip() == "hello"

with subtest("re-authorizing the same file returns exists:1 (dedup)"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/ATTACH22/file {auth} "
        f"-H 'If-None-Match: *' "
        f"-d 'md5=5d41402abc4b2a76b9719d911017c592&filename=t.pdf&filesize=5&mtime=1700000000000' "
        f"| jq -e '.exists == 1'"
    )

with subtest("a file authorization without a precondition header is 428"):
    assert (
        http_code(
            f"-X POST {base}/users/1/items/ATTACH22/file {auth} "
            f"-d 'md5=deadbeef&filename=t.pdf&filesize=5&mtime=1'"
        )
        == "428"
    )

with subtest("uploading bytes that do not match the declared md5 is rejected"):
    # Verification happens at the upload step now (the bytes go straight to S3),
    # so a mismatch is refused there rather than at registration.
    bad = machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/BADHASH2/file {auth} "
        f"-H 'If-None-Match: *' -d 'md5=00000000000000000000000000000000&filename=b&filesize=5&mtime=1' "
        f"| jq -r .uploadKey"
    ).strip()
    code = machine.succeed(
        f"printf hello | curl -s -o /dev/null -w '%{{http_code}}' "
        f"-X POST {base}/uploads/{bad} --data-binary @-"
    ).strip()
    assert code == "400", code

with subtest("registering without a prior upload is rejected"):
    # Authorize but never PUT the bytes, then try to register: the object was
    # never stored, so registration must refuse rather than commit metadata.
    tok = machine.succeed(
        f"curl -sf -X POST {base}/users/1/items/NQUPLQAD/file {auth} "
        f"-H 'If-None-Match: *' -d 'md5=5d41402abc4b2a76b9719d911017c592&filename=n&filesize=5&mtime=1' "
        f"| jq -r .uploadKey"
    ).strip()
    assert (
        http_code(
            f"-X POST {base}/users/1/items/NQUPLQAD/file {auth} "
            f"-H 'If-None-Match: *' -d 'upload={tok}'"
        )
        == "400"
    )

with subtest("non-alphanumeric keys are rejected from the file endpoint"):
    # The item key becomes an object key in the bucket and a path component in
    # URLs, so a key with '.'/'/' (path traversal) must be refused.
    assert (
        http_code(
            f"-X POST {base}/users/1/items/bad..key/file {auth} "
            f"-H 'If-None-Match: *' -d 'md5=x&filename=t&filesize=1&mtime=1'"
        )
        == "400"
    )

with subtest("annotations round-trip as ordinary items"):
    # Highlights/notes are items (itemType annotation/note), so they sync through
    # the same opaque object path rather than any dedicated endpoint.
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f"-d '[{{\"key\":\"ANNQT223\",\"itemType\":\"annotation\","
        f"\"annotationType\":\"highlight\",\"annotationText\":\"marked\","
        f"\"parentItem\":\"ATTACH22\"}}]' "
        f"| jq -e '.successful.\"0\".data.annotationText == \"marked\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ANNQT223&format=json' {auth} "
        f"| jq -e '.[0].data.annotationType == \"highlight\"'"
    )
    # Like linkMode, annotationType must be emitted first: the client's fromJSON
    # rejects an annotation ("annotationType must be set before other annotation
    # properties") if it sees other annotation* fields first.
    first_key = machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=ANNQT223&format=json' {auth} "
        f"| jq -r '.[0].data | keys_unsorted[0]'"
    ).strip()
    assert first_key == "annotationType", first_key

with subtest("full-text content uploads, lists versions, and downloads"):
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/fulltext {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f"-d '[{{\"key\":\"ATTACH22\",\"content\":\"hello world\","
        f"\"indexedChars\":11,\"totalChars\":11,\"indexedPages\":1,\"totalPages\":1}}]' "
        f"| jq -e '.successful.\"0\".key == \"ATTACH22\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/fulltext?format=versions&since=0' {auth} | jq -e '.ATTACH22'"
    )
    # The per-item version must equal the value in the versions map, else the
    # client re-downloads content it already holds every sync.
    item_v = machine.succeed(
        f"curl -sf -D - -o /dev/null '{base}/users/1/items/ATTACH22/fulltext' {auth} "
        f"| grep -i last-modified-version | tr -d '\\r' | cut -d' ' -f2"
    ).strip()
    list_v = machine.succeed(
        f"curl -sf '{base}/users/1/fulltext?format=versions&since=0' {auth} | jq -r '.ATTACH22'"
    ).strip()
    assert item_v == list_v, f"{item_v} != {list_v}"
    machine.succeed(
        f"curl -sf '{base}/users/1/items/ATTACH22/fulltext' {auth} "
        f"| jq -e '.content == \"hello world\" and .totalPages == 1'"
    )

with subtest("a stale full-text write is rejected with 412"):
    assert (
        http_code(
            f"-X POST {base}/users/1/fulltext {auth} "
            f"-H 'If-Unmodified-Since-Version: 0' "
            f"-d '[{{\"key\":\"ATTACH22\",\"content\":\"x\"}}]'"
        )
        == "412"
    )

with subtest("the query API filters, searches, sorts and paginates items"):
    # Seed a couple of distinct items to query over.
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"QBQQK223","itemType":"book","title":"Borrow Checker",'
        f'"creators":[{{"lastName":"Klabnik","firstName":"Steve"}}],'
        f'"tags":[{{"tag":"rustlang"}},{{"tag":"systems"}}]}},'
        f'{{"key":"QART2223","itemType":"journalArticle","title":"Unrelated",'
        f'"tags":[{{"tag":"rustlang"}}]}}]\' | jq -e .successful'
    )

    def keys(qs):
        return machine.succeed(
            f"curl -sf '{base}/users/1/items?{qs}' {auth} | jq -r '[.[].key]|sort|join(\",\")'"
        ).strip()

    # Title and creator search (titleCreatorYear, the default qmode).
    assert keys("q=borrow") == "QBQQK223", keys("q=borrow")
    assert keys("q=klabnik") == "QBQQK223", keys("q=klabnik")
    # Type filter, including negation.
    assert "QBQQK223" in keys("itemType=book")
    assert "QART2223" not in keys("itemType=book")
    assert "QART2223" in keys("itemType=-book")
    # Tags: repeated key is AND, only the book carries both.
    assert keys("tag=rustlang&tag=systems") == "QBQQK223"
    assert keys("tag=rustlang") == "QART2223,QBQQK223"
    # Full-text only matches under qmode=everything (ATTACH22 holds "hello world").
    assert keys("q=world") == ""
    assert keys("q=world&qmode=everything") == "ATTACH22"

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
        f'-d \'[{{"key":"TQPITEM3","itemType":"book","title":"Shelf Book",'
        f'"collections":["CQLLXXXX"],"tags":[{{"tag":"shelf"}}]}},'
        f'{{"key":"CHILDNQ3","itemType":"note","note":"child","parentItem":"TQPITEM3"}},'
        f'{{"key":"TRASHED3","itemType":"book","title":"Gone","deleted":1}}]\' '
        f"| jq -e .successful"
    )

    def path_keys(p):
        return machine.succeed(
            f"curl -sf '{base}{p}' {auth} | jq -r '[.[].key]|sort|join(\",\")'"
        ).strip()

    # Top excludes children (have parentItem) and trashed items.
    top = path_keys("/users/1/items/top")
    assert "TQPITEM3" in top, top
    assert "CHILDNQ3" not in top, top
    assert "TRASHED3" not in top, top
    # Trash returns only trashed items.
    trash = path_keys("/users/1/items/trash")
    assert trash == "TRASHED3", trash
    # Collection membership.
    assert path_keys("/users/1/collections/CQLLXXXX/items") == "TQPITEM3"
    # Tags lists distinct tags with counts; the trashed item's tag is excluded.
    machine.succeed(
        f"curl -sf {base}/users/1/tags {auth} "
        f"| jq -e '.[] | select(.tag == \"shelf\") | .numItems == 1'"
    )

with subtest("top filtering covers parentItem:false and the versions view"):
    # A top-level item may carry parentItem:false rather than omitting it; the
    # /top versions view (the client's parent-first phase) must be top-filtered.
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"TQPFALS3","itemType":"book","title":"pf",'
        f'"parentItem":false}}]\' | jq -e .successful'
    )
    # parentItem:false is treated as top-level.
    top = machine.succeed(
        f"curl -sf '{base}/users/1/items/top' {auth} | jq -r '[.[].key]|join(\"\\n\")'"
    )
    assert "TQPFALS3" in top, top
    # /items/top?format=versions returns only top-level keys; the child CHILDNQ3
    # (from the previous subtest) appears in the full versions map but not here.
    top_v = machine.succeed(
        f"curl -sf '{base}/users/1/items/top?format=versions&since=0' {auth} | jq -e 'has(\"CHILDNQ3\")|not'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?format=versions&since=0' {auth} | jq -e '.CHILDNQ3'"
    )

with subtest("malformed (non-array) item data does not 500 the query API"):
    # data is stored opaquely, so an item can carry a scalar where an array is
    # expected; jsonb_array_elements must not crash the listing/tag endpoints.
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"MALFQRM3","itemType":"book","title":"weird",'
        f'"creators":"notarray","tags":"x","collections":"y"}}]\' | jq -e .successful'
    )
    assert http_code(f"{base}/users/1/tags {auth}") == "200"
    # q=zzz forces the creators sub-select to run on the malformed row.
    assert http_code(f"'{base}/users/1/items?q=zzz' {auth}") == "200"
    assert http_code(f"'{base}/users/1/items?q=zzz&qmode=everything' {auth}") == "200"
    assert http_code(f"'{base}/users/1/items?tag=zzz' {auth}") == "200"
    assert http_code(f"{base}/users/1/collections/y/items {auth}") == "200"
    # A tags array whose elements aren't {tag:...} objects must not put a NULL
    # into tag_names and crash /tags (migration 0007 filters them).
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"BADTAGS3","itemType":"book","title":"bt",'
        f'"tags":[{{"tag":"good"}},123,"junk"]}}]\' | jq -e .successful'
    )
    machine.succeed(
        f"curl -sf {base}/users/1/tags {auth} "
        f"| jq -e '.[] | select(.tag == \"good\") | .numItems == 1'"
    )

with subtest("Total-Results stays correct past the last page"):
    def total_results(qs):
        return machine.succeed(
            f"curl -sf -D - -o /dev/null '{base}/users/1/items?{qs}' {auth} "
            f"| grep -i total-results | tr -d '\\r' | cut -d' ' -f2"
        ).strip()

    total = total_results("limit=1")
    assert int(total) > 0, total
    # A page past the end still reports the true total, not zero.
    assert total_results("limit=1&start=100000") == total, total

with subtest("query API: OR filters, descending sort and includeTrashed"):
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"CQVRA223","itemType":"book","title":"Aaa","tags":[{{"tag":"aa"}}]}},'
        f'{{"key":"CQVRB223","itemType":"journalArticle","title":"Bbb","tags":[{{"tag":"bb"}}]}},'
        f'{{"key":"CQVRT223","itemType":"book","title":"Ccc","deleted":1,'
        f'"tags":[{{"tag":"aa"}}]}}]\' | jq -e .successful'
    )

    def first(qs):
        return machine.succeed(
            f"curl -sf '{base}/users/1/items?{qs}' {auth} | jq -r '.[0].key'"
        ).strip()

    def kset(qs):
        return machine.succeed(
            f"curl -sf '{base}/users/1/items?{qs}' {auth} | jq -r '[.[].key]|sort|join(\",\")'"
        ).strip()

    # `||` is OR for both tag and itemType.
    both = kset("tag=aa||bb&itemType=book||journalArticle")
    assert "CQVRA223" in both and "CQVRB223" in both, both
    # Descending title sort: Bbb before Aaa, trashed Ccc excluded by default.
    assert first("tag=aa||bb&sort=title&direction=desc") == "CQVRB223"
    # includeTrashed surfaces the trashed item (Ccc sorts first descending).
    assert first("tag=aa&sort=title&direction=desc&includeTrashed=1") == "CQVRT223"
    assert "CQVRT223" not in kset("tag=aa")

with subtest("sort=date orders by extracted year, not raw freeform text"):
    version = library_version()
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {version}' "
        f'-d \'[{{"key":"DATE2QLD","itemType":"book","title":"old","date":"circa 1850",'
        f'"tags":[{{"tag":"era"}}]}},'
        f'{{"key":"DATE2MID","itemType":"book","title":"mid","date":"January 1960",'
        f'"tags":[{{"tag":"era"}}]}},'
        f'{{"key":"DATE2NEW","itemType":"book","title":"new","date":"2020-05-01",'
        f'"tags":[{{"tag":"era"}}]}}]\' | jq -e .successful'
    )
    # Ascending by year: 1850 < 1960 < 2020, despite the leading-character order
    # ("2020" < "January" < "circa") a naive text sort would give.
    order = machine.succeed(
        f"curl -sf '{base}/users/1/items?tag=era&sort=date&direction=asc' {auth} "
        f"| jq -r '[.[].key]|join(\",\")'"
    ).strip()
    assert order == "DATE2QLD,DATE2MID,DATE2NEW", order

with subtest("collection top items are served as a plain-text key list"):
    # The sync client fetches collections/<key>/items/top?format=keys when
    # restoring a deleted collection, parsing the body as newline-split keys.
    # TQPITEM3 (top, in CQLLXXXX) was created by the convenience-listings subtest.
    body = machine.succeed(
        f"curl -sf '{base}/users/1/collections/CQLLXXXX/items/top?format=keys' {auth}"
    ).strip()
    assert body == "TQPITEM3", repr(body)
    headers = machine.succeed(
        f"curl -sf -D - -o /dev/null '{base}/users/1/collections/CQLLXXXX/items/top?format=keys' {auth}"
    ).lower()
    # Plain text (not JSON) plus a version header for the client's 304 handling.
    assert "content-type: text/plain" in headers, headers
    assert "last-modified-version:" in headers, headers

with subtest("a versions read with nothing newer returns 200 + empty map, not 304"):
    v = library_version()
    # format=versions reads must NOT 304: the client's getVersions sends no
    # If-Modified-Since-Version header and treats a 304 as "no data", which then
    # mismatches its library-version check and loops the sync forever. Return 200
    # with an empty {} map plus the version header instead.
    for path in [
        f"items?format=versions&since={v}",
        f"items/top?format=versions&since={v}",
        f"collections?format=versions&since={v}",
        f"searches?format=versions&since={v}",
        f"fulltext?format=versions&since={v}",
    ]:
        assert http_code(f"'{base}/users/1/{path}' {auth}") == "200", path
        machine.succeed(f"curl -sf '{base}/users/1/{path}' {auth} | jq -e '. == {{}}'")
    # The version header is still present so the client keeps tracking the library version.
    machine.succeed(
        f"curl -sf -D - -o /dev/null '{base}/users/1/items?format=versions&since={v}' {auth} "
        f"| grep -iq 'last-modified-version: {v}'"
    )
    # settings still 304s — getSettings sends the If-Modified-Since-Version header
    # and handles a 304 itself.
    assert (
        http_code(f"{base}/users/1/settings -H 'If-Modified-Since-Version: {v}' {auth}") == "304"
    )

with subtest("collections round-trip through write, read, versions and delete"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/collections {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"CQLL2223","name":"Papers"}}]\' '
        f"| jq -e '.successful.\"0\".key == \"CQLL2223\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/collections?collectionKey=CQLL2223&format=json' {auth} "
        f"| jq -e '.[0].data.name == \"Papers\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/collections?format=versions&since=0' {auth} | jq -e '.CQLL2223'"
    )
    machine.succeed(f"curl -sf -X DELETE '{base}/users/1/collections?collectionKey=CQLL2223' {auth} "
                    f"-H 'If-Unmodified-Since-Version: {library_version()}'")
    machine.succeed(
        f"curl -sf '{base}/users/1/deleted?since=0' {auth} | jq -e '.collections | index(\"CQLL2223\")'"
    )

with subtest("saved searches round-trip through write, read and delete"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/searches {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"SRCH2223","name":"Recent","conditions":[]}}]\' '
        f"| jq -e '.successful.\"0\".key == \"SRCH2223\"'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/searches?searchKey=SRCH2223&format=json' {auth} "
        f"| jq -e '.[0].data.name == \"Recent\"'"
    )
    machine.succeed(f"curl -sf -X DELETE '{base}/users/1/searches?searchKey=SRCH2223' {auth} "
                    f"-H 'If-Unmodified-Since-Version: {library_version()}'")
    machine.succeed(
        f"curl -sf '{base}/users/1/deleted?since=0' {auth} | jq -e '.searches | index(\"SRCH2223\")'"
    )

with subtest("settings round-trip through write, read and delete"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/settings {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'{{"tagColors":{{"value":[{{"name":"x","color":"#fff"}}]}}}}\' '
    )
    machine.succeed(
        f"curl -sf {base}/users/1/settings {auth} "
        f"| jq -e '.tagColors.value[0].name == \"x\" and (.tagColors.version | type == \"number\")'"
    )
    # A stale settings write is a 412.
    assert (
        http_code(
            f"-X POST {base}/users/1/settings {auth} "
            f"-H 'If-Unmodified-Since-Version: 0' -d '{{\"k\":{{\"value\":1}}}}'"
        )
        == "412"
    )
    # DELETE actually removes the setting (regression: it used to be a no-op).
    machine.succeed(
        f"curl -sf -X DELETE '{base}/users/1/settings?settingKey=tagColors' {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}'"
    )
    machine.succeed(f"curl -sf {base}/users/1/settings {auth} | jq -e '.tagColors == null'")
    machine.succeed(
        f"curl -sf '{base}/users/1/deleted?since=0' {auth} | jq -e '.settings | index(\"tagColors\")'"
    )

with subtest("a gzip-compressed write body is decoded"):
    # The real client gzips write bodies; exercise the decompression middleware.
    machine.succeed(
        f"printf '%s' '[{{\"key\":\"GZIP2223\",\"itemType\":\"book\",\"title\":\"zipped\"}}]' "
        f"| gzip | curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f"-H 'Content-Encoding: gzip' --data-binary @- | jq -e .successful"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=GZIP2223&format=json' {auth} "
        f"| jq -e '.[0].data.title == \"zipped\"'"
    )

with subtest("PATCH updates items and is gated by the read-only key"):
    machine.succeed(
        f"curl -sf -X PATCH {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"GZIP2223","itemType":"book","title":"patched"}}]\' '
        f"| jq -e '.successful.\"0\".data.title == \"patched\"'"
    )
    assert (
        http_code(
            f"-X PATCH {base}/users/1/items {readonly} "
            f"-H 'If-Unmodified-Since-Version: 0' -d '[{{\"key\":\"X\",\"itemType\":\"book\"}}]'"
        )
        == "403"
    )

with subtest("PATCH merges: a partial update keeps unspecified fields"):
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"MERGEIT2","itemType":"book","title":"orig","tags":[{{"tag":"keep"}}]}}]\' '
        f"| jq -e .successful"
    )
    # PATCH only the title; itemType and tags must survive.
    machine.succeed(
        f"curl -sf -X PATCH {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"MERGEIT2","title":"new"}}]\''
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=MERGEIT2&format=json' {auth} "
        f"| jq -e '.[0].data.title==\"new\" and .[0].data.itemType==\"book\" "
        f"and .[0].data.tags[0].tag==\"keep\"'"
    )

with subtest("POST also merges: a partial upload keeps unspecified fields"):
    # The sync client uploads only an existing object's changed fields via POST
    # (e.g. just lastRead after opening an attachment). A full replace would drop
    # itemType/linkMode and corrupt the attachment, so POST must merge too.
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"PMERGEAT","itemType":"attachment","linkMode":"imported_file",'
        f'"filename":"f.pdf","contentType":"application/pdf"}}]\' | jq -e .successful'
    )
    # POST a partial object: only key + a changed field (mimics lastRead sync).
    machine.succeed(
        f"curl -sf -X POST {base}/users/1/items {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}' "
        f'-d \'[{{"key":"PMERGEAT","lastRead":1700000000,"tags":[]}}]\''
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/items?itemKey=PMERGEAT&format=json' {auth} "
        f"| jq -e '.[0].data.itemType==\"attachment\" and .[0].data.linkMode==\"imported_file\" "
        f"and .[0].data.filename==\"f.pdf\" and .[0].data.lastRead==1700000000'"
    )

with subtest("deletes are recorded in the deletion log"):
    machine.succeed(
        f"curl -sf -X DELETE '{base}/users/1/items?itemKey=ITEM2223' {auth} "
        f"-H 'If-Unmodified-Since-Version: {library_version()}'"
    )
    machine.succeed(
        f"curl -sf '{base}/users/1/deleted?since=0' {auth} | jq -e '.items | index(\"ITEM2223\")'"
    )
