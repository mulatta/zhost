-- Persist browser login enrollment across restarts without storing either
-- capability in plaintext. Pending expiry is evaluated against expires_at so
-- approval, cancellation, and polling share one durable state machine.

alter table api_keys
    add constraint api_keys_id_user_id_key unique (id, user_id);

create table login_sessions (
    token_hash   bytea primary key,
    user_id      bigint references users (id) on delete cascade,
    api_key_id   bigint,
    client_type  text        not null,
    status       text        not null default 'pending',
    created_at   timestamptz not null default now(),
    expires_at   timestamptz not null default (now() + interval '15 minutes'),
    completed_at timestamptz,
    constraint login_sessions_sha256_length
        check (octet_length(token_hash) = 32),
    constraint login_sessions_client_type_valid
        check (client_type in ('mac', 'windows', 'linux', 'ios', 'android', 'unknown')),
    constraint login_sessions_status_valid
        check (status in ('pending', 'completed', 'cancelled')),
    constraint login_sessions_expiry_valid
        check (expires_at > created_at),
    constraint login_sessions_completion_valid check (
        (status = 'completed'
            and user_id is not null
            and api_key_id is not null
            and completed_at is not null)
        or
        (status <> 'completed'
            and api_key_id is null
            and completed_at is null)
    ),
    constraint login_sessions_api_key_owner_fk
        foreign key (api_key_id, user_id)
        references api_keys (id, user_id) on delete cascade
);

create index login_sessions_user_id_idx on login_sessions (user_id);
create index login_sessions_expires_at_idx on login_sessions (expires_at);
