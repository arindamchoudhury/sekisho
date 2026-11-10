-- Sekisho plan 1b: Bedrock and Azure providers. A provider now carries the
-- API format it serves, its kind's settings, one of four credential shapes,
-- and whether its base URL may be on a private network. Plan 1's rows are
-- upgraded in place: each keeps its key and gets its kind's only format.

ALTER TABLE providers DROP CONSTRAINT providers_kind_check;
ALTER TABLE providers ADD CONSTRAINT providers_kind_check
    CHECK (kind IN ('anthropic', 'openai', 'gemini', 'openai_compatible', 'bedrock', 'azure'));

ALTER TABLE providers ADD COLUMN api_format text;
UPDATE providers SET api_format = CASE kind
    WHEN 'anthropic' THEN 'anthropic'
    WHEN 'gemini'    THEN 'gemini'
    ELSE 'openai'
END;
ALTER TABLE providers ALTER COLUMN api_format SET NOT NULL;
ALTER TABLE providers ADD CONSTRAINT providers_api_format_check
    CHECK (api_format IN ('anthropic', 'openai', 'gemini'));
-- Single-format kinds serve their only format; Bedrock and Azure choose.
ALTER TABLE providers ADD CONSTRAINT providers_kind_format_check CHECK (CASE kind
    WHEN 'anthropic' THEN api_format = 'anthropic'
    WHEN 'gemini'    THEN api_format = 'gemini'
    WHEN 'openai'    THEN api_format = 'openai'
    WHEN 'openai_compatible' THEN api_format = 'openai'
    ELSE api_format IN ('anthropic', 'openai')
END);

-- {"region": ...} for Bedrock, {"endpoint": ...} for Azure, {} otherwise.
ALTER TABLE providers
    ADD COLUMN options jsonb NOT NULL DEFAULT '{}'
        CONSTRAINT providers_options_check CHECK (jsonb_typeof(options) = 'object'),
    ADD COLUMN allow_private_network boolean NOT NULL DEFAULT false;

-- AES-256-GCM as before: an API key's bytes, or AWS access keys or an Entra
-- service principal as JSON. The gateway's cloud identity stores nothing.
ALTER TABLE providers RENAME COLUMN api_key_ciphertext TO credential_ciphertext;
ALTER TABLE providers ALTER COLUMN credential_ciphertext DROP NOT NULL;
ALTER TABLE providers
    ADD COLUMN credential_kind text NOT NULL DEFAULT 'key'
        CONSTRAINT providers_credential_kind_check
        CHECK (credential_kind IN ('key', 'aws_access_keys', 'azure_client_secret', 'cloud_identity')),
    ADD CONSTRAINT providers_credential_check
        CHECK ((credential_kind = 'cloud_identity') = (credential_ciphertext IS NULL));
-- The default only upgraded plan 1's rows; every insert names its shape.
ALTER TABLE providers ALTER COLUMN credential_kind DROP DEFAULT;
