"""DB-backed API-key contract and populated-v7 migration coverage."""

base = "http://localhost:8189"
recovery = "recoverytoken"
alice = "AliceKey23456789AbCdEfGh"
bob = "BobKeyAB23456789CdEfGhJk"
bob_ro = "BobROAB23456789CdEfGhJKL"
bob_full = "BobFllAB23456789CdEfGhJK"
revoked = "RevokedK23456789AbCdEfGh"
api_headers = "-H 'Zotero-API-Version: 3' -H 'Zotero-Schema-Version: 42'"


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

# Seed two DB-owned keys. Only SHA-256 digests enter PostgreSQL.
psql(
    """insert into users (id, username, display_name)
       values (202, 'bob', 'Bob')"""
)
psql("insert into library (id, kind, version) values (22, 'personal', 0)")
psql("insert into personal_libraries (user_id, library_id) values (202, 22)")
psql("update library set version = 3 where id = 22")
psql(
    """insert into object (library_id, kind, key, version, data)
       values (22, 'item', 'LEGACY22', 3,
               '{"key":"LEGACY22","version":3,"itemType":"book",
                 "title":"Bob private item"}')"""
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
       (1003, true, true, true, true)"""
)
psql("update api_keys set revoked_at = now() where id = 1003")

with subtest("current-key introspection returns the authenticated DB owner"):
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {alice}' {base}/keys/current |
        jq -e '. == {{
          "key":"{alice}",
          "userID":101,
          "username":"alice",
          "displayName":"Alice",
          "access":{{"user":{{
            "library":true,"files":true,"notes":true,"write":true
          }}}}
        }}'"""
    )
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {bob}' {base}/keys/current |
        jq -e '. == {{
          "key":"{bob}",
          "userID":202,
          "username":"bob",
          "displayName":"Bob",
          "access":{{"user":{{"library":true,"files":true}}}}
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

with subtest("static recovery key remains bound to the bootstrap user"):
    machine.succeed(
        f"""curl -sf {api_headers} -H 'Zotero-API-Key: {recovery}' {base}/keys/current |
        jq -e '. == {{
          "key":"{recovery}",
          "userID":101,
          "username":"alice",
          "displayName":"Alice",
          "access":{{"user":{{
            "library":true,"files":true,"notes":true,"write":true
          }}}}
        }}'"""
    )

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
    assert http_code("/users/202/items?format=versions&since=0", bob) == "403"
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

with subtest("same attachment key stays isolated across personal libraries"):
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
    bob_location = machine.succeed(
        f"curl -sf -D /tmp/bob-file -o /dev/null {base}/users/202/items/SHARED22/file "
        f"{api_headers} -H 'Zotero-API-Key: {bob_full}' "
        "&& grep -i '^location:' /tmp/bob-file | tr -d '\\r' | awk '{print $2}'"
    ).strip()
    assert machine.succeed(f"curl -sf '{alice_location}'").strip() == "hello"
    assert machine.succeed(f"curl -sf '{bob_location}'").strip() == "world"

with subtest("revoked key cannot finish an authorized upload"):
    pending_token = machine.succeed(
        f"curl -sf -X POST {base}/users/202/items/REVOKE22/file {api_headers} "
        f"-H 'Zotero-API-Key: {bob_full}' -H 'If-None-Match: *' "
        "-d 'md5=7d793037a0760186574b0282f2f435e7&filename=r.pdf&filesize=5&mtime=3' "
        "| jq -r .uploadKey"
    ).strip()
    psql("update api_keys set revoked_at = now() where id = 1005")
    assert http_code(f"/uploads/{pending_token}", method="POST", body="world") == "403"

with subtest("revocation takes effect on the next request"):
    assert http_code("/keys/current", alice) == "200"
    psql("update api_keys set revoked_at = now() where id = 1001")
    assert http_code("/keys/current", alice) == "403"

with subtest("database stores only fixed-length key digests"):
    assert psql("select count(*) from api_keys where octet_length(token_hash) = 32") == "5"
    assert psql("select count(*) from api_keys where encode(token_hash, 'escape') like '%token%'") == "0"
