-- Sekisho plan 3, Task 10: Git mode (D175). Environments are kept in one
-- repository, read through the catalog source named `environments` and
-- applied on each new commit; changes are proposed as GitHub pull
-- requests with the token kept here. At most one row: Git mode is on or
-- off for the whole install.

CREATE TABLE git_mode (
    singleton           boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    source_id           uuid NOT NULL UNIQUE
                        REFERENCES catalog_sources (id) ON DELETE CASCADE,
    -- GitHub's REST API: https://api.github.com, or GitHub Enterprise
    -- Server's /api/v3.
    github_api_url      text NOT NULL,
    sealed_github_token bytea NOT NULL,
    -- Owns the environments Git mode creates.
    enabled_by          uuid NOT NULL REFERENCES users (id),
    enabled_at          timestamptz NOT NULL DEFAULT now()
);
