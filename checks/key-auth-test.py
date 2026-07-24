"""DB-backed API-key contract and populated-v7 migration coverage."""

import hashlib
import json


base = "http://localhost:8189"
recovery = "recoverytoken"
alice = "AliceKey23456789AbCdEfGh"
bob = "BobKeyAB23456789CdEfGhJk"
bob_ro = "BobROAB23456789CdEfGhJKL"
bob_full = "BobFllAB23456789CdEfGhJK"
bob_notes_write = "BobNwrtAB23456789CdEfGh"
bob_group_rw = "BobGrpRW23456789AbCdEfGh"
revoked = "RevokedK23456789AbCdEfGh"
api_headers = "-H 'Zotero-API-Version: 3' -H 'Zotero-Schema-Version: 42'"
oidc_issuer = "https://id.example.test"
oidc_subject = "alice-subject"
oidc_bob_subject = "bob-subject"
oidc = (
    f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}' "
    f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}'"
)
oidc_bob = (
    f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}' "
    f"-H 'X-Zhost-OIDC-Subject: {oidc_bob_subject}'"
)


def psql(sql):
    escaped = sql.replace("'", "'\"'\"'")
    return machine.succeed(
        f"sudo -u zhost psql -v ON_ERROR_STOP=1 -At -d zhost -c '{escaped}'"
    ).strip()


def http_code(path, token=None, method="GET", body=None, version=None):
    auth = f"-H 'Zotero-API-Key: {token}'" if token else ""
    data = ""
    if body is not None:
        data = f"-H 'Content-Type: application/json' --data '{body}'"
    if version is not None:
        data += f" -H 'If-Unmodified-Since-Version: {version}'"
    return machine.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -X {method} "
        f"{api_headers} {auth} {data} {base}{path}"
    ).strip()


def restart_zhost():
    machine.succeed("systemctl reset-failed zhost.service")
    machine.succeed("systemctl restart zhost.service")
    machine.wait_for_unit("zhost.service")
    machine.wait_for_open_port(8189)


def get_json(path, token):
    return json.loads(
        machine.succeed(
            f"curl -sf {api_headers} -H 'Zotero-API-Key: {token}' '{base}{path}'"
        )
    )


def response_header(raw_headers, name):
    prefix = f"{name.lower()}:"
    values = [
        line.split(":", 1)[1].strip()
        for line in raw_headers.splitlines()
        if line.lower().startswith(prefix)
    ]
    assert len(values) == 1, (name, values, raw_headers)
    return values[0]


def assert_group_entry(entry, is_admin):
    assert set(entry) == {"id", "version", "links", "meta", "data"}
    assert entry["id"] == 303 and entry["version"] == 4
    assert entry["links"]["self"] == {
        "href": f"{base}/groups/303",
        "type": "application/json",
    }
    assert entry["links"]["alternate"]["href"].endswith("/groups/303")
    assert entry["links"]["alternate"]["type"] == "text/html"
    assert entry["meta"] == {
        "created": "2024-01-02T03:04:05Z",
        "lastModified": "2024-02-03T04:05:06Z",
        "numItems": 2,
        "isAdmin": is_admin,
    }
    assert entry["data"] == {
        "id": 303,
        "version": 4,
        "name": "Alice Research Group",
        "owner": 101,
        "type": "Private",
        "description": "Private collaboration fixture",
        "url": "https://groups.example.test/alice-research",
        "libraryEditing": "admins",
        "libraryReading": "members",
        "fileEditing": "admins",
        "members": [202],
    }


def sha256_hex(value):
    return hashlib.sha256(value.encode()).hexdigest()


def create_login_session():
    return json.loads(
        machine.succeed(f"curl -sf -X POST {base}/keys/sessions -d '{{}}'")
    )["sessionToken"]


def approve_login_session(token, identity_headers=None):
    headers = oidc if identity_headers is None else identity_headers
    return machine.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
        f"{base}/login {headers} -d 'session={token}'"
    ).strip()


def poll_login_session(token):
    return json.loads(machine.succeed(f"curl -sf {base}/keys/sessions/{token}"))


machine.wait_for_unit("postgresql.service")
machine.wait_for_unit("rustfs.service")
machine.succeed("mc alias set s3 http://localhost:9000 rustfsadmin rustfsadmin")
machine.succeed("mc mb s3/zotero")

# Recreate the exact pre-identity schema and populate every durable data family.
machine.succeed(
    "sudo -u zhost env "
    "'DATABASE_URL=postgresql://zhost@localhost/zhost?host=/run/postgresql' "
    "sqlx migrate run --source /etc/zhost-test/migrations"
)
assert (
    psql("select string_agg(version::text, ',' order by version) from _sqlx_migrations")
    == "1,2,3,4,5,6,7"
)
psql("update library set version = 7 where id = 1")
psql(
    """insert into object (library_id, kind, key, version, data)
       values (1, 'item', 'LEGACY22', 7,
               '{"key":"LEGACY22","version":7,"itemType":"attachment",
                 "linkMode":"imported_file","filename":"legacy.pdf",
                 "contentType":"application/pdf","title":"preserved"}')"""
)
psql(
    """insert into setting (library_id, key, version, value)
       values (1, 'legacySetting', 7, '{"enabled":true}')"""
)
psql(
    """insert into deletion (library_id, kind, key, version)
       values (1, 'item', 'DELETED2', 6)"""
)
psql(
    """insert into file
       (library_id, item_key, md5, filename, filesize, mtime, version)
       values (1, 'LEGACY22', '5d41402abc4b2a76b9719d911017c592',
               'legacy.pdf', 5, 1700000000000, 7)"""
)
psql("insert into library (id, version) values (22, 0)")
psql(
    """insert into object (library_id, kind, key, version, data)
       values (22, 'item', 'OLDLIB22', 0,
               '{"key":"OLDLIB22","version":0,"itemType":"attachment",
                 "linkMode":"imported_file","filename":"old.pdf",
                 "contentType":"application/pdf"}')"""
)
psql(
    """insert into file
       (library_id, item_key, md5, filename, filesize, mtime, version)
       values (22, 'OLDLIB22', '27b525e7d8fdb7db5b821c3a0bf7c60e',
               'old.pdf', 5, 1700000000001, 0)"""
)
machine.succeed("printf old22 | mc pipe s3/zotero/libraries/22/OLDLIB22")
psql(
    """insert into fulltext
       (library_id, item_key, content, indexed_chars, total_chars,
        indexed_pages, total_pages, version)
       values (1, 'LEGACY22', 'legacy text', 11, 11, 1, 1, 7)"""
)

machine.start_job("zhost.service")
machine.wait_for_unit("zhost.service")
machine.wait_for_open_port(8189)

with subtest("populated v7 data survives the identity migration"):
    assert (
        psql("select string_agg(version::text, ',' order by version) from _sqlx_migrations")
        == "1,2,3,4,5,6,7,8,9,10,11,12,13"
    )
    assert psql("select count(*) from pending_uploads") == "0"
    assert psql("select bool_and(not compressed) from file") == "t"
    assert psql("select bool_and(blob_md5 = md5) from file") == "t"
    assert psql("select (id, version) = (1, 7) from library where id = 1") == "t"
    assert (
        psql(
            """select library_id = 1 and kind = 'item' and version = 7
                      and data = '{"key":"LEGACY22","version":7,
                        "itemType":"attachment","linkMode":"imported_file",
                        "filename":"legacy.pdf","contentType":"application/pdf",
                        "title":"preserved"}'::jsonb
               from object where key = 'LEGACY22'"""
        )
        == "t"
    )
    assert (
        psql(
            """select library_id = 1 and version = 7
                      and value = '{"enabled":true}'::jsonb
               from setting where key = 'legacySetting'"""
        )
        == "t"
    )
    assert (
        psql(
            """select library_id = 1 and kind = 'item' and version = 6
               from deletion where key = 'DELETED2'"""
        )
        == "t"
    )
    assert (
        psql(
            """select library_id = 1
                      and md5 = '5d41402abc4b2a76b9719d911017c592'
                      and filename = 'legacy.pdf' and filesize = 5
                      and mtime = 1700000000000 and version = 7
                      and blob_key = 'LEGACY22'
               from file where item_key = 'LEGACY22'"""
        )
        == "t"
    )
    assert (
        psql(
            """select library_id = 1 and content = 'legacy text'
                      and indexed_chars = 11 and total_chars = 11
                      and indexed_pages = 1 and total_pages = 1 and version = 7
               from fulltext where item_key = 'LEGACY22'"""
        )
        == "t"
    )

with subtest("bootstrap identity owns the legacy personal library"):
    assert psql("select username from users where id = 101") == "alice"
    assert psql("select display_name from users where id = 101") == "Alice"
    assert psql("select library_id from personal_libraries where user_id = 101") == "1"
    assert psql("select kind from library where id = 1") == "personal"
    assert (
        psql(
            f"""select user_id = 101
                from external_identities
                where issuer = '{oidc_issuer}' and subject = '{oidc_subject}'"""
        )
        == "t"
    )
    assert (
        psql(
            """select count(*) = 1
               from external_identities
               where user_id = 101"""
        )
        == "t"
    )

with subtest("bootstrap OIDC identity remains unique across restarts"):
    restart_zhost()
    assert (
        psql(
            f"""select count(*) = 1 and min(user_id) = 101
                from external_identities
                where issuer = '{oidc_issuer}' and subject = '{oidc_subject}'"""
        )
        == "t"
    )

# Seed two DB-owned keys. Only SHA-256 digests enter PostgreSQL.
psql(
    """insert into users (id, username, display_name)
       values (202, 'bob', 'Bob')"""
)
psql(
    """insert into users (id, username, display_name, disabled_at)
       values (404, 'disabled-member', 'Disabled Member', now())"""
)
psql("insert into personal_libraries (user_id, library_id) values (202, 22)")
psql(
    f"""insert into external_identities (issuer, subject, user_id)
        values ('{oidc_issuer}', '{oidc_bob_subject}', 202)"""
)
psql("update library set version = 3 where id = 22")
psql("insert into library (id, version, kind) values (33, 17, 'group')")
psql(
    """begin;
       insert into groups
       (id, library_id, owner_user_id, name, description, url, type,
        library_reading, library_editing, file_editing, version,
        created_at, updated_at)
       values
       (303, 33, 101, 'Alice Research Group', 'Private collaboration fixture',
        'https://groups.example.test/alice-research', 'Private',
        'members', 'admins', 'admins', 4,
        '2024-01-02 03:04:05+00', '2024-02-03 04:05:06+00');
       insert into group_memberships (group_id, user_id, role)
       values
       (303, 101, 'owner'),
       (303, 202, 'member'),
       (303, 404, 'member');
       commit"""
)
psql(
    """insert into object (library_id, kind, key, version, data)
       values
       (33, 'item', 'GRPBKK22', 17,
        '{"key":"GRPBKK22","version":17,"itemType":"book",
          "title":"Group fixture item"}'),
       (33, 'item', 'GRPTRS22', 17,
        '{"key":"GRPTRS22","version":17,"itemType":"book",
          "title":"Trashed group fixture item","deleted":true}')"""
)
psql(
    """insert into object (library_id, kind, key, version, data)
       values
       (1, 'item', 'GRPBKK22', 7,
        '{"key":"GRPBKK22","version":7,"itemType":"book",
          "title":"Personal collision fixture"}')"""
)
psql(
    """insert into setting (library_id, key, version, value)
       values (33, 'groupSetting', 17, '"shared"')"""
)
psql(
    """insert into object (library_id, kind, key, version, data)
       values (22, 'item', 'LEGACY22', 3,
               '{"key":"LEGACY22","version":3,"itemType":"book",
                 "title":"Bob private item",
                 "collections":["COLL2222"],
                 "tags":[{"tag":"shared-tag"},{"tag":"book-only"}]}'),
              (22, 'item', 'PRIVNTE2', 3,
               '{"key":"PRIVNTE2","version":3,"itemType":"note",
                 "note":"Bob private note",
                 "collections":["COLL2222"],
                 "tags":[{"tag":"shared-tag"},{"tag":"note-only"}]}'),
              (22, 'item', 'PRIVATCH', 3,
               '{"key":"PRIVATCH","version":3,"itemType":"attachment",
                 "linkMode":"imported_file","filename":"private.pdf",
                 "contentType":"application/pdf","title":"Bob attachment",
                 "collections":["COLL2222"]}'),
              (22, 'item', 'PRIVANN2', 3,
               '{"key":"PRIVANN2","version":3,"itemType":"annotation",
                 "parentItem":"PRIVATCH","annotationType":"highlight",
                 "annotationText":"Visible annotation",
                 "tags":[{"tag":"annotation-only"}]}'),
              (22, 'item', 'TRSHNTE2', 3,
               '{"key":"TRSHNTE2","version":3,"itemType":"note",
                 "note":"Trashed private note","deleted":true}'),
              (22, 'item', 'TRSHANN2', 3,
               '{"key":"TRSHANN2","version":3,"itemType":"annotation",
                 "parentItem":"PRIVATCH","annotationType":"highlight",
                 "annotationText":"Trashed annotation","deleted":true}')"""
)
psql(
    """insert into object (library_id, kind, key, version, data)
       values (22, 'collection', 'COLL2222', 3,
               '{"key":"COLL2222","version":3,"name":"Private collection",
                 "parentCollection":false}')"""
)
psql(
    """insert into api_keys (id, user_id, name, token_hash)
       values
       (1001, 101, 'Alice desktop',
        decode('30946709724859f7e30069553145fcc7b4610e4fa1e42015c02c7546c52730f7', 'hex')),
       (1002, 202, 'Bob reader',
        decode('1601245df15c0f91020aaed3b59109703c77e1254eab802548d921b1e5253774', 'hex')),
       (1006, 202, 'Bob read only',
        decode('3f3f0b28d688250ab4da083a59d4a0b55fcd95d531a9849dd3f5116e98d95e14', 'hex')),
       (1005, 202, 'Bob desktop',
        decode('d777b9d2f600ae973eaea591e10ff1020d0f154c13c18be0b317bce4f652b30d', 'hex')),
       (1007, 202, 'Bob no-notes writer',
        decode('54299e610a6b8388e97182c0280ec5c998c48cab5c6618f782aefbcaa8abfa97', 'hex')),
       (1003, 202, 'Revoked',
        decode('e6dc6348b6d83af6db9a10d7224b6b0da3c340da34b76e66afd7bbf1e0ab30a8', 'hex'))"""
)
psql(
    """insert into api_key_user_permissions
       (api_key_id, library, notes, write, files)
       values
       (1001, true, true, true, true),
       (1002, true, false, false, true),
       (1006, true, true, false, true),
       (1005, true, true, true, true),
       (1007, true, false, true, true),
       (1003, true, true, true, true)"""
)
psql(
    """insert into api_key_all_groups_permissions (api_key_id, library, write)
       values (1001, true, true)"""
)
psql(
    """insert into api_key_group_permissions
       (api_key_id, user_id, group_id, library, write)
       values (1002, 202, 303, true, false)"""
)
psql("update api_keys set revoked_at = now() where id = 1003")

with subtest("non-default legacy attachment path survives the blob-key migration"):
    assert (
        psql("select blob_key from file where library_id = 22 and item_key = 'OLDLIB22'")
        == "libraries/22/OLDLIB22"
    )
    old_location = machine.succeed(
        f"curl -sf -D /tmp/old-library-file -o /dev/null "
        f"{base}/users/202/items/OLDLIB22/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_full}' "
        "&& grep -i '^location:' /tmp/old-library-file | tr -d '\\r' | awk '{print $2}'"
    ).strip()
    machine.succeed(
        "grep -iq 'zotero-file-md5: 27b525e7d8fdb7db5b821c3a0bf7c60e' "
        "/tmp/old-library-file"
    )
    assert machine.succeed(f"curl -sf '{old_location}'").strip() == "old22"
    psql("delete from object where library_id = 22 and key = 'OLDLIB22'")

with subtest("current-key introspection returns the authenticated DB owner"):
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {alice}' {base}/keys/current |
        jq -e '. == {{
          "key":"{alice}",
          "userID":101,
          "username":"alice",
          "displayName":"Alice",
          "access":{{
            "user":{{
              "library":true,"files":true,"notes":true,"write":true
            }},
            "groups":{{"all":{{"library":true,"write":true}}}}
          }}
        }}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' {base}/keys/current |
        jq -e '. == {{
          "key":"{bob}",
          "userID":202,
          "username":"bob",
          "displayName":"Bob",
          "access":{{
            "user":{{"library":true,"files":true}},
            "groups":{{"303":{{"library":true,"write":false}}}}
          }}
        }}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob_full}' {base}/keys/current |
        jq -e '. == {{
          "key":"{bob_full}",
          "userID":202,
          "username":"bob",
          "displayName":"Bob",
          "access":{{"user":{{
            "library":true,"files":true,"notes":true,"write":true
          }}}}
        }}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob_ro}' {base}/keys/current |
        jq -e '. == {{
          "key":"{bob_ro}",
          "userID":202,
          "username":"bob",
          "displayName":"Bob",
          "access":{{"user":{{
            "library":true,"files":true,"notes":true
          }}}}
        }}'"""
    )

with subtest("group listings intersect user membership with key grants"):
    assert (
        psql(
            """select g.library_id = 33 and g.version = 4
                      and l.version = 17 and g.owner_user_id = 101
               from groups g join library l on l.id = g.library_id
               where g.id = 303"""
        )
        == "t"
    )
    for user_id, token, is_admin in (
        (101, alice, True),
        (202, bob, False),
    ):
        headers = machine.succeed(
            f"curl -sf -D - -o /tmp/groups-{user_id}.json "
            f"{api_headers} -H 'Zotero-API-Key: {token}' "
            f"'{base}/users/{user_id}/groups'"
        )
        assert response_header(headers, "Total-Results") == "1"
        listing = json.loads(machine.succeed(f"cat /tmp/groups-{user_id}.json"))
        assert len(listing) == 1
        assert_group_entry(listing[0], is_admin)
        assert get_json(f"/users/{user_id}/groups?format=versions", token) == {
            "303": 4
        }

    assert get_json("/users/202/groups", bob_ro) == []
    assert get_json("/users/202/groups?format=versions", bob_ro) == {}
    assert http_code("/groups/303", bob_ro) == "404"
    alice_cross_listing = get_json("/users/202/groups", alice)
    assert len(alice_cross_listing) == 1
    assert_group_entry(alice_cross_listing[0], True)
    bob_cross_listing = get_json("/users/101/groups", bob)
    assert len(bob_cross_listing) == 1
    assert_group_entry(bob_cross_listing[0], False)
    assert http_code("/users/404/groups", alice) == "404"
    assert http_code("/users/999/groups", alice) == "404"

    list_head = machine.succeed(
        f"curl -sfI {api_headers} -H 'Zotero-API-Key: {alice}' "
        f"{base}/users/101/groups"
    )
    assert response_header(list_head, "Total-Results") == "1"
    assert 'rel="alternate"' in response_header(list_head, "Link")
    assert not any(
        line.lower().startswith("last-modified-version:")
        for line in list_head.splitlines()
    )
    versions_headers = machine.succeed(
        f"curl -sf -D - -o /tmp/group-versions.json "
        f"{api_headers} -H 'Zotero-API-Key: {alice}' "
        f"'{base}/users/101/groups?format=versions'"
    )
    assert response_header(versions_headers, "Total-Results") == "1"
    assert 'rel="alternate"' in response_header(versions_headers, "Link")
    assert not any(
        line.lower().startswith("last-modified-version:")
        for line in versions_headers.splitlines()
    )
    assert json.loads(machine.succeed("cat /tmp/group-versions.json")) == {"303": 4}

with subtest("group metadata and HEAD use upstream wrapper and version"):
    for label, token, is_admin in (
        ("owner", alice, True),
        ("member", bob, False),
    ):
        get_headers = machine.succeed(
            f"curl -sf -D - -o /tmp/group-{label}.json "
            f"{api_headers} -H 'Zotero-API-Key: {token}' "
            f"{base}/groups/303"
        )
        assert response_header(get_headers, "Last-Modified-Version") == "4"
        entry = json.loads(machine.succeed(f"cat /tmp/group-{label}.json"))
        assert_group_entry(entry, is_admin)

        head_headers = machine.succeed(
            f"curl -sfI {api_headers} -H 'Zotero-API-Key: {token}' "
            f"{base}/groups/303"
        )
        assert response_header(head_headers, "Last-Modified-Version") == "4"
        assert not any(
            line.lower().startswith("total-results:")
            for line in head_headers.splitlines()
        )

with subtest("group admins receive isAdmin and admin membership metadata"):
    psql(
        """update group_memberships set role = 'admin'
           where group_id = 303 and user_id = 202"""
    )
    admin_entry = get_json("/groups/303", bob)
    assert admin_entry["meta"]["isAdmin"] is True
    assert admin_entry["data"]["admins"] == [202]
    assert "members" not in admin_entry["data"]
    psql(
        """update group_memberships set role = 'member'
           where group_id = 303 and user_id = 202"""
    )
    assert_group_entry(get_json("/groups/303", bob), False)

with subtest("membership removal immediately revokes explicit group discovery"):
    psql("delete from group_memberships where group_id = 303 and user_id = 202")
    assert (
        psql(
            """select count(*) = 0
               from api_key_group_permissions
               where api_key_id = 1002 and user_id = 202 and group_id = 303"""
        )
        == "t"
    )
    assert get_json("/users/202/groups?format=versions", bob) == {}
    assert http_code("/groups/303", bob) == "404"
    assert http_code("/groups/303/items?format=versions", bob) == "403"
    assert get_json("/users/202/groups?format=versions", alice) == {}
    assert get_json("/users/101/groups?format=versions", bob) == {}
    assert get_json("/users/101/groups?format=versions", alice) == {"303": 4}
    assert http_code("/groups/303", alice) == "200"

    psql(
        """insert into group_memberships (group_id, user_id, role)
           values (303, 202, 'member')"""
    )
    psql(
        """insert into api_key_group_permissions
           (api_key_id, user_id, group_id, library, write)
           values (1002, 202, 303, true, false)"""
    )

with subtest("a disabled private-group owner hides the group from members"):
    psql("update users set disabled_at = now() where id = 101")
    assert get_json("/users/202/groups?format=versions", bob) == {}
    assert http_code("/groups/303", bob) == "404"
    psql("update users set disabled_at = null where id = 101")
    assert get_json("/users/202/groups?format=versions", bob) == {"303": 4}
    assert http_code("/groups/303", bob) == "200"

with subtest("group data routes resolve the authorized group library"):
    expected_group_versions = {
        "GRPBKK22": 17,
        "GRPTRS22": 17,
    }
    assert (
        get_json("/groups/303/items?format=versions&since=0", bob)
        == expected_group_versions
    )
    assert (
        get_json("/groups/303/items?format=versions&since=0", alice)
        == expected_group_versions
    )
    assert (
        get_json("/groups/303/items?format=versions&since=0", recovery)
        == expected_group_versions
    )
    group_items = get_json(
        "/groups/303/items?itemKey=GRPBKK22,GRPTRS22&format=json", bob
    )
    assert {entry["key"] for entry in group_items} == {"GRPBKK22", "GRPTRS22"}
    assert next(
        entry for entry in group_items if entry["key"] == "GRPBKK22"
    )["data"]["title"] == "Group fixture item"
    assert get_json("/groups/303/settings", bob)["groupSetting"]["value"] == "shared"
    assert get_json("/groups/303/collections?format=versions", bob) == {}
    assert get_json("/groups/303/searches?format=versions", bob) == {}
    assert get_json("/groups/303/fulltext?format=versions", bob) == {}
    assert get_json("/groups/303/tags", bob) == []
    deleted = get_json("/groups/303/deleted?since=0", bob)
    assert all(not values for values in deleted.values())
    assert http_code("/groups/303/items?format=versions", bob_full) == "403"
    assert http_code("/groups/999/items?format=versions", alice) == "404"

with subtest("group mutations enforce key grants, role policy and file policy"):
    assert (
        http_code(
            "/groups/303/items",
            bob,
            method="POST",
            body='[{"key":"DENIED22","itemType":"book"}]',
            version=17,
        )
        == "403"
    )
    psql(
        f"""insert into api_keys (id, user_id, name, token_hash)
            values
            (1008, 202, 'Bob explicit group writer',
             decode('{sha256_hex(bob_group_rw)}', 'hex'))"""
    )
    psql(
        """insert into api_key_user_permissions
           (api_key_id, library, notes, write, files)
           values (1008, false, false, false, false)"""
    )
    psql(
        """insert into api_key_group_permissions
           (api_key_id, user_id, group_id, library, write)
           values (1008, 202, 303, true, true)"""
    )
    assert (
        http_code(
            "/groups/303/items",
            bob_group_rw,
            method="POST",
            body='[{"key":"EXPRW222","itemType":"book","title":"explicit write"}]',
            version=17,
        )
        == "200"
    )
    psql(
        """insert into object (library_id, kind, key, version, data)
           values
           (33, 'item', 'XLDATT22', 18,
            '{"key":"XLDATT22","version":18,"itemType":"attachment",
              "linkMode":"imported_file","filename":"existing.pdf"}')"""
    )
    assert (
        http_code(
            "/groups/303/items",
            bob_group_rw,
            method="POST",
            body=(
                '[{"key":"GRPATT22","itemType":"attachment",'
                '"linkMode":"imported_file","filename":"private.pdf"}]'
            ),
            version=18,
        )
        == "403"
    )
    assert (
        http_code(
            "/groups/303/items",
            bob_group_rw,
            method="POST",
            body=(
                '[{"key":"RLLBCK22","itemType":"book"},'
                '{"key":"XLDATT22","itemType":"book"}]'
            ),
            version=18,
        )
        == "403"
    )
    assert (
        get_json("/groups/303/items?itemKey=RLLBCK22&format=json", bob_group_rw)
        == []
    )
    assert (
        http_code(
            "/groups/303/items?itemKey=XLDATT22",
            bob_group_rw,
            method="DELETE",
            version=18,
        )
        == "403"
    )
    assert (
        get_json("/groups/303/items?itemKey=XLDATT22&format=json", bob_group_rw)[0][
            "data"
        ]["itemType"]
        == "attachment"
    )
    assert psql("select version from library where id = 33") == "18"
    assert (
        http_code(
            "/groups/303/settings",
            bob_group_rw,
            method="POST",
            body='{"attachmentRenameTemplate":{"value":"{{ title }}"}}',
            version=18,
        )
        == "403"
    )
    assert (
        http_code(
            "/groups/303/settings",
            bob_group_rw,
            method="POST",
            body='{"memberSetting":{"value":"allowed"}}',
            version=18,
        )
        == "204"
    )
    assert (
        http_code(
            "/groups/303/settings",
            alice,
            method="POST",
            body='{"attachmentRenameTemplate":{"value":"{{ title }}"}}',
            version=19,
        )
        == "204"
    )
    assert (
        http_code(
            "/groups/303/settings?settingKey=attachmentRenameTemplate",
            bob_group_rw,
            method="DELETE",
            version=20,
        )
        == "403"
    )

    psql(
        """insert into api_key_all_groups_permissions (api_key_id, library, write)
           values (1005, true, true)"""
    )
    assert (
        http_code(
            "/groups/303/items",
            bob_full,
            method="POST",
            body='[{"key":"ALLRW222","itemType":"book"}]',
            version=20,
        )
        == "403"
    )
    psql("update groups set library_editing = 'members' where id = 303")
    assert (
        http_code(
            "/groups/303/items",
            bob_full,
            method="POST",
            body='[{"key":"ALLRW222","itemType":"book"}]',
            version=20,
        )
        == "200"
    )
    psql("update groups set library_editing = 'admins' where id = 303")
    psql("delete from api_key_all_groups_permissions where api_key_id = 1005")

with subtest("group files revalidate policy and grants across upload steps"):
    file_form = (
        "md5=5d41402abc4b2a76b9719d911017c592"
        "&filename=group.pdf&filesize=5&mtime=1700000000000"
    )
    psql(
        """insert into object (library_id, kind, key, version, data)
           values
           (33, 'item', 'STALAT22', 21,
            '{"key":"STALAT22","version":21,"itemType":"attachment",
              "linkMode":"imported_file","filename":"stale.pdf"}')"""
    )
    stale_item = machine.succeed(
        f"curl -sf -X POST {base}/groups/303/items/STALAT22/file {api_headers} "
        f"-H 'Zotero-API-Key: {alice}' -H 'If-None-Match: *' "
        f"-d '{file_form}' | jq -r .uploadKey"
    ).strip()
    assert http_code(f"/uploads/{stale_item}", method="POST", body="hello") == "201"
    psql(
        """update object
           set data = data || '{"itemType":"book"}'
           where library_id = 33 and kind = 'item' and key = 'STALAT22'"""
    )
    assert (
        machine.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
            f"{base}/groups/303/items/STALAT22/file {api_headers} "
            f"-H 'Zotero-API-Key: {alice}' -d 'upload={stale_item}'"
        ).strip()
        == "409"
    )
    assert (
        psql(
            """select count(*) from file
               where library_id = 33 and item_key = 'STALAT22'"""
        )
        == "0"
    )
    assert (
        psql(
            """select count(*) from pending_uploads
               where library_id = 33 and item_key = 'STALAT22'"""
        )
        == "0"
    )
    psql(
        """delete from object
           where library_id = 33 and kind = 'item' and key = 'STALAT22'"""
    )
    assert (
        machine.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
            f"{base}/groups/303/items/XLDATT22/file {api_headers} "
            f"-H 'Zotero-API-Key: {bob_group_rw}' -H 'If-None-Match: *' "
            f"-d '{file_form}'"
        ).strip()
        == "403"
    )

    psql("update groups set file_editing = 'members' where id = 303")
    revoked_before_put = machine.succeed(
        f"curl -sf -X POST {base}/groups/303/items/XLDATT22/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_group_rw}' -H 'If-None-Match: *' "
        f"-d '{file_form}' | jq -r .uploadKey"
    ).strip()
    psql("delete from api_key_group_permissions where api_key_id = 1008")
    assert (
        http_code(f"/uploads/{revoked_before_put}", method="POST", body="hello")
        == "403"
    )
    psql(
        """insert into api_key_group_permissions
           (api_key_id, user_id, group_id, library, write)
           values (1008, 202, 303, true, true)"""
    )

    revoked_before_register = machine.succeed(
        f"curl -sf -X POST {base}/groups/303/items/XLDATT22/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_group_rw}' -H 'If-None-Match: *' "
        f"-d '{file_form}' | jq -r .uploadKey"
    ).strip()
    assert (
        http_code(f"/uploads/{revoked_before_register}", method="POST", body="hello")
        == "201"
    )
    restart_zhost()
    psql("delete from api_key_group_permissions where api_key_id = 1008")
    assert (
        machine.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
            f"{base}/groups/303/items/XLDATT22/file {api_headers} "
            f"-H 'Zotero-API-Key: {bob_group_rw}' "
            f"-d 'upload={revoked_before_register}'"
        ).strip()
        == "403"
    )
    assert (
        psql(
            """select count(*) from file
               where library_id = 33 and item_key = 'XLDATT22'"""
        )
        == "0"
    )
    psql(
        """insert into api_key_group_permissions
           (api_key_id, user_id, group_id, library, write)
           values (1008, 202, 303, true, true)"""
    )
    psql("update groups set file_editing = 'admins' where id = 303")

    owner_upload = machine.succeed(
        f"curl -sf -X POST {base}/groups/303/items/XLDATT22/file {api_headers} "
        f"-H 'Zotero-API-Key: {alice}' -H 'If-None-Match: *' "
        f"-d '{file_form}' | jq -r .uploadKey"
    ).strip()
    assert http_code(f"/uploads/{owner_upload}", method="POST", body="hello") == "201"
    assert (
        machine.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
            f"{base}/groups/303/items/XLDATT22/file {api_headers} "
            f"-H 'Zotero-API-Key: {alice}' -d 'upload={owner_upload}'"
        ).strip()
        == "204"
    )
    assert http_code("/groups/303/items/XLDATT22/file", bob) == "302"
    assert psql(
        """select blob_key from file
           where library_id = 33 and item_key = 'XLDATT22'"""
    ).startswith("libraries/33/uploads/")

    group_version = int(psql("select version from library where id = 33"))
    assert (
        http_code(
            "/groups/303/items",
            alice,
            method="POST",
            body='[{"key":"XLDATT22","itemType":"book"}]',
            version=group_version,
        )
        == "200"
    )
    assert (
        psql(
            """select count(*) from file
               where library_id = 33 and item_key = 'XLDATT22'"""
        )
        == "0"
    )
    assert http_code("/groups/303/items/XLDATT22/file", bob) == "404"

    group_version = int(psql("select version from library where id = 33"))
    assert (
        http_code(
            "/groups/303/items",
            alice,
            method="POST",
            body='[{"key":"XLDATT22","itemType":"attachment"}]',
            version=group_version,
        )
        == "200"
    )
    assert http_code("/groups/303/items/XLDATT22/file", bob) == "404"
    replacement_upload = machine.succeed(
        f"curl -sf -X POST {base}/groups/303/items/XLDATT22/file {api_headers} "
        f"-H 'Zotero-API-Key: {alice}' -H 'If-None-Match: *' "
        f"-d '{file_form}' | jq -r .uploadKey"
    ).strip()
    assert (
        http_code(f"/uploads/{replacement_upload}", method="POST", body="hello")
        == "201"
    )
    assert (
        machine.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' -X POST "
            f"{base}/groups/303/items/XLDATT22/file {api_headers} "
            f"-H 'Zotero-API-Key: {alice}' -d 'upload={replacement_upload}'"
        ).strip()
        == "204"
    )

    group_version = int(psql("select version from library where id = 33"))
    assert (
        http_code(
            "/groups/303/items?itemKey=XLDATT22",
            alice,
            method="DELETE",
            version=group_version,
        )
        == "204"
    )
    assert (
        psql(
            """select count(*) from file
               where library_id = 33 and item_key = 'XLDATT22'"""
        )
        == "0"
    )
    assert http_code("/groups/303/items/XLDATT22/file", bob) == "404"
    psql("delete from api_keys where id = 1008")

with subtest("static recovery key remains bound to the bootstrap user"):
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {recovery}' {base}/keys/current |
        jq -e '. == {{
          "key":"{recovery}",
          "userID":101,
          "username":"alice",
          "displayName":"Alice",
          "access":{{
            "user":{{
              "library":true,"files":true,"notes":true,"write":true
            }},
            "groups":{{"all":{{"library":true,"write":true}}}}
          }}
        }}'"""
    )

with subtest("OIDC approval requires the exact bootstrap identity"):
    rejected_session = create_login_session()
    rejected_hash = sha256_hex(rejected_session)
    alice_keys_before_rejection = psql(
        "select count(*) from api_keys where user_id = 101"
    )
    rejected_identities = (
        "",
        f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}'",
        f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}'",
        (
            f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}' "
            f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}' "
            f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}'"
        ),
        (
            f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}' "
            f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}' "
            f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}'"
        ),
        "-H 'X-Auth-Request-Email: owner@mulatta.io'",
        (
            f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}' "
            "-H 'X-Zhost-OIDC-Subject: unknown-subject'"
        ),
        (
            "-H 'X-Zhost-OIDC-Issuer: https://other-id.example.test' "
            f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}'"
        ),
        (
            f"-H 'X-Zhost-OIDC-Issuer: {oidc_issuer}/' "
            f"-H 'X-Zhost-OIDC-Subject: {oidc_subject}'"
        ),
    )
    for identity_headers in rejected_identities:
        assert approve_login_session(rejected_session, identity_headers) == "403"

    rejected_result = poll_login_session(rejected_session)
    assert rejected_result["status"] == "pending"
    assert "apiKey" not in rejected_result or rejected_result["apiKey"] is None
    assert (
        psql("select count(*) from api_keys where user_id = 101")
        == alice_keys_before_rejection
    )
    assert (
        psql(
            f"""select status = 'pending' and api_key_id is null
                from login_sessions
                where token_hash = decode('{rejected_hash}', 'hex')"""
        )
        == "t"
    )

with subtest("browser login mints distinct hashed full-access keys for Alice"):
    initial_alice_keys = int(psql("select count(*) from api_keys where user_id = 101"))
    first_session = create_login_session()
    second_session = create_login_session()
    assert first_session != second_session

    for session in (first_session, second_session):
        session_hash = sha256_hex(session)
        assert (
            psql(
                f"""select status = 'pending'
                           and api_key_id is null
                           and encode(token_hash, 'hex') = '{session_hash}'
                    from login_sessions
                    where token_hash = decode('{session_hash}', 'hex')"""
            )
            == "t"
        )
        assert (
            psql(
                f"""select position('{session}' in row_to_json(login_sessions)::text) = 0
                    from login_sessions
                    where token_hash = decode('{session_hash}', 'hex')"""
            )
            == "t"
        )

    assert approve_login_session(first_session) == "200"
    first_result = poll_login_session(first_session)
    assert first_result["status"] == "completed"
    assert first_result["userID"] == 101 and first_result["username"] == "alice"
    first_key = first_result["apiKey"]
    assert len(first_key) == 24
    assert set(first_key) <= set("23456789ABCDEFGHIJKLMNPQRSTUVWXYZ"), first_key
    assert first_key != recovery
    assert int(psql("select count(*) from api_keys where user_id = 101")) == initial_alice_keys + 1
    first_api_key_id = psql(
        f"""select api_key_id from login_sessions
            where token_hash = decode('{sha256_hex(first_session)}', 'hex')"""
    )
    assert first_api_key_id

    # Approval is a one-way state transition. A replay cannot mint another key.
    assert approve_login_session(first_session) == "409"
    assert int(psql("select count(*) from api_keys where user_id = 101")) == initial_alice_keys + 1
    assert (
        psql(
            f"""select api_key_id from login_sessions
                where token_hash = decode('{sha256_hex(first_session)}', 'hex')"""
        )
        == first_api_key_id
    )

    assert approve_login_session(second_session) == "200"
    second_result = poll_login_session(second_session)
    assert second_result["status"] == "completed"
    assert second_result["userID"] == 101 and second_result["username"] == "alice"
    second_key = second_result["apiKey"]
    assert len(second_key) == 24
    assert set(second_key) <= set("23456789ABCDEFGHIJKLMNPQRSTUVWXYZ"), second_key
    assert second_key not in (first_key, recovery)
    assert int(psql("select count(*) from api_keys where user_id = 101")) == initial_alice_keys + 2

    for session, key in ((first_session, first_key), (second_session, second_key)):
        session_hash = sha256_hex(session)
        key_hash = sha256_hex(key)
        assert (
            psql(
                f"""select ls.status = 'completed'
                           and ls.user_id = 101
                           and ls.api_key_id = k.id
                           and k.user_id = 101
                           and encode(ls.token_hash, 'hex') = '{session_hash}'
                           and encode(k.token_hash, 'hex') = '{key_hash}'
                           and octet_length(ls.token_hash) = 32
                           and octet_length(k.token_hash) = 32
                           and p.library and p.notes and p.write and p.files
                           and gp.library and gp.write
                    from login_sessions ls
                    join api_keys k on k.id = ls.api_key_id
                    join api_key_user_permissions p on p.api_key_id = k.id
                    join api_key_all_groups_permissions gp on gp.api_key_id = k.id
                    where ls.token_hash = decode('{session_hash}', 'hex')"""
            )
            == "t"
        )
        assert (
            psql(
                f"""select position('{session}' in row_to_json(ls)::text) = 0
                           and position('{key}' in row_to_json(ls)::text) = 0
                           and position('{session}' in row_to_json(k)::text) = 0
                           and position('{key}' in row_to_json(k)::text) = 0
                    from login_sessions ls
                    join api_keys k on k.id = ls.api_key_id
                    where ls.token_hash = decode('{session_hash}', 'hex')"""
            )
            == "t"
        )
        machine.succeed(
            f"""curl -sf {api_headers} -H 'Zotero-API-Key: {key}' {base}/keys/current |
            jq -e '.userID == 101
              and .username == "alice"
              and .displayName == "Alice"
              and .access.user == {{
                "library":true,"files":true,"notes":true,"write":true
              }}
              and .access.groups == {{
                "all":{{"library":true,"write":true}}
              }}'"""
        )

    # Recovery remains a configured credential, never a login-minted DB row.
    assert (
        psql(
            f"""select count(*) from api_keys
                where token_hash = decode('{sha256_hex(recovery)}', 'hex')"""
        )
        == "0"
    )
    assert http_code("/keys/current", recovery) == "200"

with subtest("mapped non-bootstrap OIDC identity mints a key for Bob"):
    initial_bob_keys = int(psql("select count(*) from api_keys where user_id = 202"))
    bob_session = create_login_session()
    assert approve_login_session(bob_session, oidc_bob) == "200"
    bob_result = poll_login_session(bob_session)
    assert bob_result["status"] == "completed"
    assert bob_result["userID"] == 202 and bob_result["username"] == "bob"
    bob_login_key = bob_result["apiKey"]
    assert len(bob_login_key) == 24
    assert set(bob_login_key) <= set("23456789ABCDEFGHIJKLMNPQRSTUVWXYZ")
    assert int(psql("select count(*) from api_keys where user_id = 202")) == initial_bob_keys + 1
    assert (
        psql(
            f"""select ls.status = 'completed'
                       and ls.user_id = 202
                       and ls.api_key_id = k.id
                       and k.user_id = 202
                       and encode(k.token_hash, 'hex') = '{sha256_hex(bob_login_key)}'
                       and p.library and p.notes and p.write and p.files
                       and gp.library and gp.write
                from login_sessions ls
                join api_keys k on k.id = ls.api_key_id
                join api_key_user_permissions p on p.api_key_id = k.id
                join api_key_all_groups_permissions gp on gp.api_key_id = k.id
                where ls.token_hash = decode('{sha256_hex(bob_session)}', 'hex')"""
        )
        == "t"
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob_login_key}' {base}/keys/current |
        jq -e '. == {{
          "key":"{bob_login_key}",
          "userID":202,
          "username":"bob",
          "displayName":"Bob",
          "access":{{
            "user":{{
              "library":true,"files":true,"notes":true,"write":true
            }},
            "groups":{{"all":{{"library":true,"write":true}}}}
          }}
        }}'"""
    )

with subtest("expired login sessions are terminal without minting a key"):
    expired_session = create_login_session()
    expired_hash = sha256_hex(expired_session)
    alice_keys_before_expiry = psql("select count(*) from api_keys where user_id = 101")
    psql(
        f"""update login_sessions
            set created_at = now() - interval '1 hour',
                expires_at = now() - interval '1 second'
            where token_hash = decode('{expired_hash}', 'hex')
              and status = 'pending'"""
    )
    assert (
        psql(
            f"""select expires_at < now() and status = 'pending' and api_key_id is null
                from login_sessions
                where token_hash = decode('{expired_hash}', 'hex')"""
        )
        == "t"
    )
    assert http_code(f"/keys/sessions/{expired_session}") == "410"
    assert http_code(f"/keys/sessions/{expired_session}", method="DELETE") == "409"
    assert psql("select count(*) from api_keys where user_id = 101") == alice_keys_before_expiry

with subtest("disabled bootstrap user cannot approve a login session"):
    disabled_session = create_login_session()
    disabled_hash = sha256_hex(disabled_session)
    alice_keys_before_disable = psql("select count(*) from api_keys where user_id = 101")
    psql("update users set disabled_at = now() where id = 101")
    assert approve_login_session(disabled_session) == "403"
    assert psql("select count(*) from api_keys where user_id = 101") == alice_keys_before_disable
    assert (
        psql(
            f"""select status = 'pending' and api_key_id is null
                from login_sessions
                where token_hash = decode('{disabled_hash}', 'hex')"""
        )
        == "t"
    )
    psql("update users set disabled_at = null where id = 101")
    assert http_code("/keys/current", recovery) == "200"

with subtest("unknown and revoked keys fail closed"):
    assert http_code("/keys/current", "unknown-token") == "403"
    assert http_code("/keys/current", revoked) == "403"

with subtest("disabled users fail closed for DB and recovery keys"):
    psql("update users set disabled_at = now() where id = 202")
    assert http_code("/keys/current", bob_ro) == "403"
    psql("update users set disabled_at = null where id = 202")
    psql("update users set disabled_at = now() where id = 101")
    assert http_code("/keys/current", recovery) == "403"
    psql("update users set disabled_at = null where id = 101")

with subtest("DB permissions allow full keys and gate read-only mutations"):
    assert (
        http_code(
            "/users/101/items",
            alice,
            method="POST",
            body='[{"key":"AUTHW222","itemType":"book"}]',
            version=7,
        )
        == "200"
    )
    current = psql("select version from library where id = 22")
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob_ro}' \
        '{base}/users/202/items?itemKey=LEGACY22&format=json' |
        jq -e 'length == 1 and .[0].data.title == "Bob private item"'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {alice}' \
        '{base}/users/101/items?itemKey=LEGACY22&format=json' |
        jq -e 'length == 1 and .[0].data.title == "preserved"'"""
    )
    assert http_code("/users/202/items?format=versions&since=0", alice) == "403"
    assert http_code("/users/101/items?format=versions&since=0", bob_ro) == "403"
    assert http_code("/users/202/items?format=versions&since=0", bob) == "200"
    assert (
        http_code(
            "/users/202/items",
            bob_ro,
            method="POST",
            body='[{"key":"RDNLY222","itemType":"book"}]',
            version=current,
        )
        == "403"
    )

with subtest("notes permission filters notes without hiding library or annotations"):
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob_full}' \
        '{base}/users/202/items?format=versions&since=0' |
        jq -e '. == {{
          "LEGACY22":3,
          "PRIVANN2":3,
          "PRIVATCH":3,
          "PRIVNTE2":3,
          "TRSHANN2":3,
          "TRSHNTE2":3
        }}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items?format=versions&since=0' |
        jq -e '. == {{
          "LEGACY22":3,
          "PRIVANN2":3,
          "PRIVATCH":3,
          "TRSHANN2":3
        }}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items?itemKey=LEGACY22,PRIVNTE2,PRIVATCH,PRIVANN2' |
        jq -e '
          map(.key) | sort
          == ["LEGACY22","PRIVANN2","PRIVATCH"]
        '"""
    )
    machine.succeed(
        f"""curl -sf -D /tmp/note-list-headers {api_headers} \
        -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items?limit=2' |
        jq -e '
          length == 2
          and all(.[]; .data.itemType != "note")
        '"""
    )
    machine.succeed(
        "grep -i '^total-results: 3' /tmp/note-list-headers"
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items?format=keys' |
        sort |
        diff -u - <(printf '%s\\n' LEGACY22 PRIVANN2 PRIVATCH | sort)"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items/top?format=versions&since=0' |
        jq -e '. == {{"LEGACY22":3,"PRIVATCH":3}}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items/top' |
        jq -e '
          map(.key) | sort
          == ["LEGACY22","PRIVATCH"]
        '"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items/top?format=keys' |
        sort |
        diff -u - <(printf '%s\\n' LEGACY22 PRIVATCH | sort)"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items/trash' |
        jq -e 'map(.key) == ["TRSHANN2"]'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/items/trash?format=keys' |
        grep -Fx TRSHANN2"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/collections/COLL2222/items' |
        jq -e '
          map(.key) | sort
          == ["LEGACY22","PRIVATCH"]
        '"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/collections/COLL2222/items?format=keys' |
        sort |
        diff -u - <(printf '%s\\n' LEGACY22 PRIVATCH | sort)"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/collections/COLL2222/items/top' |
        jq -e '
          map(.key) | sort
          == ["LEGACY22","PRIVATCH"]
        '"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/collections/COLL2222/items/top?format=keys' |
        sort |
        diff -u - <(printf '%s\\n' LEGACY22 PRIVATCH | sort)"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' \
        '{base}/users/202/tags' |
        jq -e '. == [
          {{"tag":"annotation-only","numItems":1}},
          {{"tag":"book-only","numItems":1}},
          {{"tag":"note-only","numItems":1}},
          {{"tag":"shared-tag","numItems":2}}
        ]'"""
    )

with subtest("no-notes writers retain upstream batch mutation behavior"):
    before = psql("select version from library where id = 22")
    machine.succeed(
        f"""curl -sf -X POST {api_headers} \
        -H 'Zotero-API-Key: {bob_notes_write}' \
        -H 'Content-Type: application/json' \
        -H 'If-Unmodified-Since-Version: {before}' \
        --data '[
          {{"key":"WRITNTE2","itemType":"note","note":"Writable note"}},
          {{"key":"WRITANN2","itemType":"annotation",
            "parentItem":"PRIVATCH","annotationType":"highlight",
            "annotationText":"Writable annotation"}}
        ]' \
        '{base}/users/202/items' |
        jq -e '
          .successful."0".data.note == "Writable note"
          and .successful."1".data.annotationText == "Writable annotation"
        '"""
    )
    after_create = psql("select version from library where id = 22")
    machine.succeed(
        f"""curl -sf -X PATCH {api_headers} \
        -H 'Zotero-API-Key: {bob_notes_write}' \
        -H 'Content-Type: application/json' \
        -H 'If-Unmodified-Since-Version: {after_create}' \
        --data '[{{"key":"WRITNTE2","note":"Updated hidden note"}}]' \
        '{base}/users/202/items' |
        jq -e '.successful."0".data.note == "Updated hidden note"'"""
    )
    after_patch = psql("select version from library where id = 22")
    assert (
        http_code(
            "/users/202/items?itemKey=WRITNTE2,WRITANN2",
            bob_notes_write,
            method="DELETE",
            version=after_patch,
        )
        == "204"
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob_notes_write}' \
        '{base}/users/202/deleted?since={before}' |
        jq -e '
          (.items | index("WRITNTE2"))
          and (.items | index("WRITANN2"))
        '"""
    )

with subtest("same attachment key stays isolated across personal libraries"):
    psql(
        """insert into object (library_id, kind, key, version, data)
           select id, 'item', 'SHARED22', version,
                  jsonb_build_object(
                    'key', 'SHARED22', 'version', version,
                    'itemType', 'attachment', 'linkMode', 'imported_file',
                    'filename', 'shared.pdf')
           from library where id in (1, 22)"""
    )
    alice_token = machine.succeed(
        f"curl -sf -X POST {base}/users/101/items/SHARED22/file {api_headers} "
        f"-H 'Zotero-API-Key: {alice}' -H 'If-None-Match: *' "
        "-d 'md5=5d41402abc4b2a76b9719d911017c592&filename=alice.pdf&filesize=5&mtime=1' "
        "| jq -r .uploadKey"
    ).strip()
    machine.succeed(f"printf hello | curl -sf -X POST {base}/uploads/{alice_token} --data-binary @-")
    machine.succeed(
        f"curl -sf -X POST {base}/users/101/items/SHARED22/file {api_headers} "
        f"-H 'Zotero-API-Key: {alice}' -H 'If-None-Match: *' -d 'upload={alice_token}'"
    )
    alice_location = machine.succeed(
        f"curl -sf -D /tmp/alice-file -o /dev/null {base}/users/101/items/SHARED22/file "
        f"{api_headers} -H 'Zotero-API-Key: {alice}' "
        "&& grep -i '^location:' /tmp/alice-file | tr -d '\\r' | awk '{print $2}'"
    ).strip()

    bob_token = machine.succeed(
        f"curl -sf -X POST {base}/users/202/items/SHARED22/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_full}' -H 'If-None-Match: *' "
        "-d 'md5=7d793037a0760186574b0282f2f435e7&filename=bob.pdf&filesize=5&mtime=2' "
        "| jq -r .uploadKey"
    ).strip()
    machine.succeed(f"printf world | curl -sf -X POST {base}/uploads/{bob_token} --data-binary @-")
    machine.succeed(
        f"curl -sf -X POST {base}/users/202/items/SHARED22/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_full}' -H 'If-None-Match: *' -d 'upload={bob_token}'"
    )
    bob_blob_key = psql(
        "select blob_key from file where library_id = 22 and item_key = 'SHARED22'"
    )
    assert bob_blob_key.startswith("libraries/22/uploads/"), bob_blob_key
    assert bob_blob_key != psql(
        "select blob_key from file where library_id = 1 and item_key = 'LEGACY22'"
    )
    bob_location = machine.succeed(
        f"curl -sf -D /tmp/bob-file -o /dev/null {base}/users/202/items/SHARED22/file "
        f"{api_headers} -H 'Zotero-API-Key: {bob_full}' "
        "&& grep -i '^location:' /tmp/bob-file | tr -d '\\r' | awk '{print $2}'"
    ).strip()
    assert machine.succeed(f"curl -sf '{alice_location}'").strip() == "hello"
    assert machine.succeed(f"curl -sf '{bob_location}'").strip() == "world"

with subtest("recovery upload cannot cross bootstrap-user reconfiguration"):
    recovery_pending = machine.succeed(
        f"curl -sf -X POST {base}/users/101/items/SHARED22/file {api_headers} "
        f"-H 'Zotero-API-Key: {recovery}' "
        "-H 'If-Match: 5d41402abc4b2a76b9719d911017c592' "
        "-d 'md5=7d793037a0760186574b0282f2f435e7&filename=r.pdf&filesize=5&mtime=4' "
        "| jq -r .uploadKey"
    ).strip()
    psql(
        """update pending_uploads set bootstrap_user_id = 202
           where library_id = 1 and item_key = 'SHARED22'"""
    )
    restart_zhost()
    assert http_code(f"/uploads/{recovery_pending}", method="POST", body="world") == "403"
    psql(
        """update pending_uploads
           set created_at = now() - interval '2 hours',
               expires_at = now() - interval '1 hour'
           where library_id = 1 and item_key = 'SHARED22'"""
    )

with subtest("revoked key cannot finish an authorized upload"):
    psql(
        """insert into object (library_id, kind, key, version, data)
           select id, 'item', 'REVKE222', version,
                  jsonb_build_object(
                    'key', 'REVKE222', 'version', version,
                    'itemType', 'attachment', 'linkMode', 'imported_file',
                    'filename', 'revoked.pdf')
           from library where id = 22"""
    )
    pending_token = machine.succeed(
        f"curl -sf -X POST {base}/users/202/items/REVKE222/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_full}' -H 'If-None-Match: *' "
        "-d 'md5=7d793037a0760186574b0282f2f435e7&filename=r.pdf&filesize=5&mtime=3' "
        "| jq -r .uploadKey"
    ).strip()
    restart_zhost()
    psql("update api_keys set revoked_at = now() where id = 1005")
    assert http_code(f"/uploads/{pending_token}", method="POST", body="world") == "403"

with subtest("revocation takes effect on the next request"):
    assert http_code("/keys/current", alice) == "200"
    psql("update api_keys set revoked_at = now() where id = 1001")
    assert http_code("/keys/current", alice) == "403"

with subtest("database stores only fixed-length key digests"):
    assert psql("select count(*) from api_keys where octet_length(token_hash) = 32") == "9"
    assert psql("select count(*) from api_keys where encode(token_hash, 'escape') like '%token%'") == "0"

with subtest("bootstrap OIDC ownership conflict fails startup closed"):
    machine.succeed("systemctl stop zhost.service")
    psql(
        f"""update external_identities
            set user_id = 202
            where issuer = '{oidc_issuer}' and subject = '{oidc_subject}'"""
    )
    machine.succeed(
        "mkdir -p /run/systemd/system/zhost.service.d && "
        "printf '[Service]\\nRestart=no\\n' "
        "> /run/systemd/system/zhost.service.d/test-no-restart.conf && "
        "systemctl daemon-reload"
    )
    machine.succeed("systemctl start zhost.service >/dev/null 2>&1 || true")
    machine.wait_until_succeeds("systemctl is-failed --quiet zhost.service")
    assert (
        psql(
            f"""select user_id
                from external_identities
                where issuer = '{oidc_issuer}' and subject = '{oidc_subject}'"""
        )
        == "202"
    )

    psql(
        f"""update external_identities
            set user_id = 101
            where issuer = '{oidc_issuer}' and subject = '{oidc_subject}'"""
    )
    machine.succeed(
        "rm /run/systemd/system/zhost.service.d/test-no-restart.conf && "
        "systemctl daemon-reload"
    )
    machine.succeed("systemctl reset-failed zhost.service")
    machine.succeed("systemctl start zhost.service")
    machine.wait_for_unit("zhost.service")
    machine.wait_for_open_port(8189)
    assert http_code("/keys/current", recovery) == "200"
