-- Sekisho plan 2: what signing in needs to keep. Secrets are stored as
-- SHA-256 (state values, codes, device codes, setup tokens) or sealed with
-- the install's SecretBox (signing keys' seeds, Dex refresh tokens, Dex PKCE
-- verifiers, Sekisho's Dex client secret).

-- Values made once per install, such as Sekisho's client secret at Dex.
CREATE TABLE install_settings (
    name       text PRIMARY KEY,
    value      bytea NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Ed25519 keys for session tokens. A key is published from its creation and
-- signs from not_before; a retired key stays published for 65 minutes.
CREATE TABLE signing_keys (
    kid             text PRIMARY KEY CHECK (kid ~ '^[0-9a-f]{16}$'),
    seed_ciphertext bytea NOT NULL,
    public_x        bytea NOT NULL CHECK (length(public_x) = 32),
    not_before      timestamptz NOT NULL,
    retired_at      timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- One Dex refresh token per user, shared by all their sessions.
CREATE TABLE dex_tokens (
    user_id            uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    refresh_ciphertext bytea NOT NULL,
    updated_at         timestamptz NOT NULL DEFAULT now()
);

-- RFC 8628 device codes. user_code is the 8 letters without the dash.
CREATE TABLE device_codes (
    device_sha256  bytea PRIMARY KEY CHECK (length(device_sha256) = 32),
    user_code      text NOT NULL UNIQUE CHECK (user_code ~ '^[BCDFGHJKLMNPQRSTVWXZ]{8}$'),
    client_id      text NOT NULL,
    status         text NOT NULL DEFAULT 'pending'
                   CHECK (status IN ('pending', 'approved', 'denied', 'issued')),
    user_id        uuid REFERENCES users (id) ON DELETE CASCADE,
    interval_s     integer NOT NULL CHECK (interval_s > 0),
    last_polled_at timestamptz,
    expires_at     timestamptz NOT NULL,
    CHECK ((status IN ('approved', 'issued')) = (user_id IS NOT NULL))
);

-- A sign-in waiting for Dex: for the code flow, what the client asked for;
-- for the device flow, the device it approves.
CREATE TABLE auth_requests (
    state_sha256        bytea PRIMARY KEY CHECK (length(state_sha256) = 32),
    kind                text NOT NULL CHECK (kind IN ('code', 'device')),
    client_id           text NOT NULL,
    redirect_uri        text,
    client_state        text,
    code_challenge      text,
    device_sha256       bytea REFERENCES device_codes (device_sha256) ON DELETE CASCADE,
    -- Sekisho's PKCE verifier towards Dex.
    verifier_ciphertext bytea NOT NULL,
    nonce               text NOT NULL,
    expires_at          timestamptz NOT NULL,
    CHECK (CASE kind
        WHEN 'code'   THEN redirect_uri IS NOT NULL AND code_challenge IS NOT NULL
                           AND device_sha256 IS NULL
        WHEN 'device' THEN device_sha256 IS NOT NULL AND redirect_uri IS NULL
                           AND client_state IS NULL AND code_challenge IS NULL
    END)
);

-- Authorisation codes Sekisho gives its clients: single use, 60 seconds.
CREATE TABLE auth_codes (
    code_sha256    bytea PRIMARY KEY CHECK (length(code_sha256) = 32),
    client_id      text NOT NULL,
    redirect_uri   text NOT NULL,
    code_challenge text NOT NULL,
    user_id        uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at     timestamptz NOT NULL,
    used_at        timestamptz
);

-- One-time links for setting a local account's password: 7 days.
CREATE TABLE setup_links (
    token_sha256 bytea PRIMARY KEY CHECK (length(token_sha256) = 32),
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    make_admin   boolean NOT NULL DEFAULT false,
    expires_at   timestamptz NOT NULL,
    used_at      timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX setup_links_user_id ON setup_links (user_id);
