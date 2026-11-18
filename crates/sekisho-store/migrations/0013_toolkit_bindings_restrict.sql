-- Sekisho plan 3, after its independent review: a toolkit's binding to an
-- entry no longer disappears with the entry. Deleting a source checks that
-- no toolkit elsewhere lists its entries, but a sync could bind one in the
-- meantime; with a cascade that binding vanished silently, and now the
-- delete fails instead. Old toolkit versions' bindings are removed by the
-- delete itself; a source's own toolkits go with their revisions.

ALTER TABLE catalog_toolkit_members
    DROP CONSTRAINT catalog_toolkit_members_entry_id_fkey,
    ADD CONSTRAINT catalog_toolkit_members_entry_id_fkey
        FOREIGN KEY (entry_id) REFERENCES catalog_entries (id);
