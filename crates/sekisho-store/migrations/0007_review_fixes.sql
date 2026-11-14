-- Sekisho plan 2's review: a key an admin suspended holds against its
-- owner, and a reused authorisation code ends the session it issued.

-- Set when an admin suspends a key; only an admin's resume clears it.
ALTER TABLE env_keys ADD COLUMN suspended_by_admin boolean NOT NULL DEFAULT false;

-- The session an authorisation code issued, and when the code was
-- presented again, which ends that session (RFC 6749 section 4.1.2).
ALTER TABLE auth_codes
    ADD COLUMN session_id uuid REFERENCES sessions (id) ON DELETE SET NULL,
    ADD COLUMN reused_at timestamptz;
