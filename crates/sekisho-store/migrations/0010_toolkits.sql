-- Sekisho plan 3, Task 7b: toolkits (D181-D186). A toolkit is a catalog
-- entry whose revisions list other entries. Each reference is bound to an
-- entry when a sync records the revision, and the revision keeps that
-- binding (D185): a source added later changes nothing.

CREATE TABLE catalog_toolkit_members (
    revision_id uuid NOT NULL REFERENCES catalog_revisions (id) ON DELETE CASCADE,
    kind        text NOT NULL
                CHECK (kind IN ('mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt')),
    -- The reference's place in its kind's list, as written.
    position    integer NOT NULL CHECK (position >= 0),
    -- The reference as written: name, or name@source.
    written     text NOT NULL,
    entry_id    uuid NOT NULL REFERENCES catalog_entries (id) ON DELETE CASCADE,
    -- An MCP server's allowed tools; NULL allows them all.
    tools       text[],
    CHECK (tools IS NULL OR kind = 'mcp_server'),
    PRIMARY KEY (revision_id, kind, entry_id),
    UNIQUE (revision_id, kind, position)
);

-- Which toolkits list an entry, and whether its source can be deleted.
CREATE INDEX catalog_toolkit_members_entry ON catalog_toolkit_members (entry_id);
