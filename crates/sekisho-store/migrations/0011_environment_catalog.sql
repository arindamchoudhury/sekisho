-- Sekisho plan 3, Task 8: environments list catalog entries and toolkits,
-- name default models per API format and a small model, and carry labels.
-- Environments saved before this keep empty lists and no defaults: the
-- first model each format reaches is its default (design 9.2).

ALTER TABLE environments
    ADD COLUMN labels jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(labels) = 'object'),
    ADD COLUMN default_anthropic_model_id uuid REFERENCES models (id) ON DELETE RESTRICT,
    ADD COLUMN default_openai_model_id    uuid REFERENCES models (id) ON DELETE RESTRICT,
    ADD COLUMN default_gemini_model_id    uuid REFERENCES models (id) ON DELETE RESTRICT,
    ADD COLUMN small_model_id             uuid REFERENCES models (id) ON DELETE RESTRICT,
    -- Git mode (Task 10): the file the environment was read from.
    ADD COLUMN git_path text;

-- The entries and toolkits an environment lists. Each reference is bound
-- to an entry by ID when it is first saved, and stays bound while it is
-- saved the same way (D167).
CREATE TABLE environment_entries (
    environment_id uuid NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    entry_id       uuid NOT NULL REFERENCES catalog_entries (id) ON DELETE RESTRICT,
    kind           text NOT NULL
                   CHECK (kind IN ('mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt',
                                   'toolkit')),
    -- The reference's place in its kind's list.
    position       integer NOT NULL CHECK (position >= 0),
    -- The reference as saved: name, or name@source.
    written        text NOT NULL,
    -- An MCP server's allowed tools; NULL allows them all.
    tools          text[],
    CHECK (tools IS NULL OR kind = 'mcp_server'),
    PRIMARY KEY (environment_id, entry_id),
    UNIQUE (environment_id, kind, position)
);

-- Which environments list an entry, and whether its source can be deleted.
CREATE INDEX environment_entries_entry ON environment_entries (entry_id);
