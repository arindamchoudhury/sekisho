-- Sekisho plan 2: people. Users signed in through Dex (matched by Dex's
-- subject, or a local account by the Dex user ID Sekisho gave it, never by
-- email), Sekisho's own groups, sessions, and owners and persons as users.

CREATE TABLE users (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email             text NOT NULL,
    name              text NOT NULL DEFAULT '',
    dex_subject       text UNIQUE,
    -- The connector of the latest sign-in.
    dex_connector     text,
    local             boolean NOT NULL DEFAULT false,
    -- The user ID of a local account's entry in Dex's password database.
    local_dex_user_id text UNIQUE,
    -- Marked by an admin; admin groups are applied by the server.
    admin             boolean NOT NULL DEFAULT false,
    -- The identity provider's groups at the latest sign-in or refresh.
    idp_groups        text[] NOT NULL DEFAULT '{}',
    disabled_at       timestamptz,
    idp_refused_at    timestamptz,
    idp_checked_at    timestamptz,
    last_sign_in_at   timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (local = (local_dex_user_id IS NOT NULL))
);
-- Emails aren't unique among users, only among local accounts.
CREATE UNIQUE INDEX users_local_email ON users (lower(email)) WHERE local;
CREATE INDEX users_dex_connector ON users (dex_connector);

CREATE TABLE groups (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name       text NOT NULL UNIQUE
               CHECK (name ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE group_members (
    group_id uuid NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    -- Users are disabled, never deleted.
    user_id  uuid NOT NULL REFERENCES users (id),
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX group_members_user_id ON group_members (user_id);

CREATE TABLE sessions (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id                 uuid NOT NULL REFERENCES users (id),
    client_id               text NOT NULL,
    -- Refresh tokens are stored as SHA-256 only.
    refresh_sha256          bytea NOT NULL UNIQUE CHECK (length(refresh_sha256) = 32),
    previous_refresh_sha256 bytea CHECK (length(previous_refresh_sha256) = 32),
    rotated_at              timestamptz,
    idle_expires_at         timestamptz NOT NULL,
    created_at              timestamptz NOT NULL DEFAULT now(),
    last_refreshed_at       timestamptz,
    ended_at                timestamptz,
    end_reason              text,
    CHECK ((ended_at IS NULL) = (end_reason IS NULL))
);
CREATE INDEX sessions_user_id ON sessions (user_id);
CREATE INDEX sessions_previous_refresh_sha256 ON sessions (previous_refresh_sha256);
CREATE INDEX sessions_ended_at ON sessions (ended_at) WHERE ended_at IS NOT NULL;

-- Plan 1's key owners and budget persons were free text. Each distinct one
-- becomes a user with that email and no Dex subject, so nothing signs in as
-- them; an admin offboards them or hands their keys over.
INSERT INTO users (email)
SELECT DISTINCT person FROM (
    SELECT owner AS person FROM env_keys
    UNION
    SELECT person FROM budgets WHERE person IS NOT NULL
) AS people;

ALTER TABLE env_keys ADD COLUMN owner_id uuid REFERENCES users (id);
UPDATE env_keys k SET owner_id = u.id FROM users u WHERE u.email = k.owner;
ALTER TABLE env_keys ALTER COLUMN owner_id SET NOT NULL;
ALTER TABLE env_keys DROP COLUMN owner;
CREATE INDEX env_keys_owner_id ON env_keys (owner_id);

ALTER TABLE budgets ADD COLUMN person_id uuid REFERENCES users (id);
UPDATE budgets b SET person_id = u.id FROM users u WHERE u.email = b.person;
ALTER TABLE budgets DROP CONSTRAINT budgets_check;
DROP INDEX budgets_person;
ALTER TABLE budgets DROP COLUMN person;
ALTER TABLE budgets ADD CONSTRAINT budgets_check CHECK (CASE scope
    WHEN 'person'      THEN person_id IS NOT NULL AND key_id IS NULL
    WHEN 'environment' THEN environment_id IS NOT NULL AND person_id IS NULL AND key_id IS NULL
    WHEN 'key'         THEN key_id IS NOT NULL AND person_id IS NULL AND environment_id IS NULL
END);
CREATE INDEX budgets_person_id ON budgets (person_id);

-- NULL only for environments saved before this plan; the next save sets it.
ALTER TABLE environments ADD COLUMN owner_id uuid REFERENCES users (id);
CREATE INDEX environments_owner_id ON environments (owner_id);
