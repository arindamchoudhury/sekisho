-- Sekisho plan 4, Task 7b: instructions (D220-D225), text every session of
-- a coding tool loads. A new source kind and entry kind; toolkits and
-- environments list instructions like any entry. Only the checks widen:
-- no data changes.

ALTER TABLE catalog_sources
    DROP CONSTRAINT catalog_sources_kind_check,
    ADD CONSTRAINT catalog_sources_kind_check
        CHECK (kind IN ('catalog', 'skills', 'plugins', 'prompts', 'instructions', 'environments'));

ALTER TABLE catalog_entries
    DROP CONSTRAINT catalog_entries_kind_check,
    ADD CONSTRAINT catalog_entries_kind_check
        CHECK (kind IN ('mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt', 'toolkit',
                        'instruction'));

ALTER TABLE catalog_toolkit_members
    DROP CONSTRAINT catalog_toolkit_members_kind_check,
    ADD CONSTRAINT catalog_toolkit_members_kind_check
        CHECK (kind IN ('mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt', 'instruction'));

ALTER TABLE environment_entries
    DROP CONSTRAINT environment_entries_kind_check,
    ADD CONSTRAINT environment_entries_kind_check
        CHECK (kind IN ('mcp_server', 'a2a_agent', 'skill', 'plugin', 'prompt', 'toolkit',
                        'instruction'));
