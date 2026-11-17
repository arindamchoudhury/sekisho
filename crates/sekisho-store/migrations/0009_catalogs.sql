-- Sekisho plan 3: catalogs. Git repositories as sources, each sync pinned
-- to a commit; entries with revisions, so what a sync found is kept apart
-- from what environments use (the applied revision); every synced file
-- once by SHA-256; quarantine with its history; registry credentials for
-- resolving container image tags.

CREATE TABLE catalog_sources (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name                  text NOT NULL UNIQUE
                          CHECK (name ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    kind                  text NOT NULL
                          CHECK (kind IN ('catalog', 'skills', 'plugins', 'prompts', 'environments')),
    -- Only Git mode's source has the environments kind and name (D164).
    CHECK ((kind = 'environments') = (name = 'environments')),
    url                   text NOT NULL,
    git_ref               text,
    folder                text NOT NULL DEFAULT '',
    username              text,
    -- Sealed with the install's SecretBox; never returned by the API.
    sealed_token          bytea,
    allow_private_network boolean NOT NULL DEFAULT false,
    -- Users are disabled, never deleted.
    added_by              uuid NOT NULL REFERENCES users (id),
    applied_commit        text,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    next_sync_at          timestamptz NOT NULL DEFAULT now(),
    -- One replica syncs a source at a time, until this passes.
    claimed_until         timestamptz
);
CREATE INDEX catalog_sources_next_sync_at ON catalog_sources (next_sync_at);

CREATE TABLE catalog_syncs (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The order syncs were recorded in; times can tie.
    seq         bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    source_id   uuid NOT NULL REFERENCES catalog_sources (id) ON DELETE CASCADE,
    started_at  timestamptz NOT NULL,
    finished_at timestamptz NOT NULL,
    commit      text,
    outcome     text NOT NULL CHECK (outcome IN ('ok', 'unchanged', 'failed')),
    error       text,
    -- What the sync found: new, updated, unchanged and missing entries,
    -- refusals, warnings, Obot's virtual servers, Git mode's environments.
    report      jsonb NOT NULL DEFAULT '{}',
    applied_at  timestamptz,
    CHECK ((outcome = 'failed') = (error IS NOT NULL)),
    CHECK (outcome <> 'ok' OR commit IS NOT NULL),
    CHECK (applied_at IS NULL OR outcome = 'ok')
);
CREATE INDEX catalog_syncs_source_id ON catalog_syncs (source_id, seq DESC);

CREATE TABLE catalog_entries (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Deleting a source deletes its entries explicitly, once nothing
    -- lists them.
    source_id           uuid NOT NULL REFERENCES catalog_sources (id),
    -- toolkit arrives with plan 3's Task 7b.
    kind                text NOT NULL
                        CHECK (kind IN ('mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt', 'toolkit')),
    name                text NOT NULL
                        CHECK (name ~ '^[a-z0-9]+(-[a-z0-9]+)*$' AND length(name) <= 64),
    -- Where in the source the latest sync found it.
    path                text NOT NULL,
    owner_id            uuid NOT NULL REFERENCES users (id),
    applied_revision_id uuid,
    latest_revision_id  uuid,
    latest_sync_id      uuid REFERENCES catalog_syncs (id) ON DELETE SET NULL,
    removed_at          timestamptz,
    quarantined_at      timestamptz,
    quarantine_reason   text,
    quarantined_by      uuid REFERENCES users (id),
    created_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (source_id, kind, name),
    CHECK ((quarantined_at IS NULL) = (quarantine_reason IS NULL)),
    CHECK ((quarantined_at IS NULL) = (quarantined_by IS NULL))
);
CREATE INDEX catalog_entries_kind_name ON catalog_entries (kind, name);
CREATE INDEX catalog_entries_owner_id ON catalog_entries (owner_id);

CREATE TABLE catalog_revisions (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    entry_id   uuid NOT NULL REFERENCES catalog_entries (id) ON DELETE CASCADE,
    commit     text NOT NULL,
    -- SHA-256 in hex of the canonical spec and files (sekisho-catalog).
    digest     text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    spec       jsonb NOT NULL,
    applied_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (entry_id, digest)
);

ALTER TABLE catalog_entries
    ADD FOREIGN KEY (applied_revision_id) REFERENCES catalog_revisions (id),
    ADD FOREIGN KEY (latest_revision_id) REFERENCES catalog_revisions (id);

CREATE TABLE catalog_blobs (
    sha256 bytea PRIMARY KEY CHECK (length(sha256) = 32),
    bytes  bytea NOT NULL
);

CREATE TABLE catalog_revision_files (
    revision_id uuid NOT NULL REFERENCES catalog_revisions (id) ON DELETE CASCADE,
    path        text NOT NULL,
    sha256      bytea NOT NULL REFERENCES catalog_blobs (sha256),
    size        bigint NOT NULL CHECK (size >= 0),
    executable  boolean NOT NULL DEFAULT false,
    PRIMARY KEY (revision_id, path)
);

-- One row per period in quarantine; plan 6 audits and alerts from it.
CREATE TABLE catalog_quarantines (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    entry_id       uuid NOT NULL REFERENCES catalog_entries (id) ON DELETE CASCADE,
    reason         text NOT NULL,
    quarantined_by uuid NOT NULL REFERENCES users (id),
    quarantined_at timestamptz NOT NULL,
    lifted_by      uuid REFERENCES users (id),
    lifted_at      timestamptz,
    CHECK ((lifted_by IS NULL) = (lifted_at IS NULL))
);
CREATE INDEX catalog_quarantines_entry_id ON catalog_quarantines (entry_id, quarantined_at);

CREATE TABLE registry_credentials (
    -- Lowercase host[:port].
    host            text PRIMARY KEY CHECK (host = lower(host)),
    username        text NOT NULL,
    sealed_password bytea NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT now()
);
