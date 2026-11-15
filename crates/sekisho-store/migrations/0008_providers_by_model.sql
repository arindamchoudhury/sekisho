-- Sekisho plan 1d: each environment chooses a provider by model, so a
-- provider no longer serves one route format. One Bedrock or Azure
-- provider serves every tool. An OpenAI-compatible provider says which APIs
-- its host serves and, optionally, the key agentgateway's model catalog
-- files the host's prices under. Plan 1b's rows keep everything else; an
-- OpenAI-compatible one gets Chat Completions, all it served before.

ALTER TABLE providers DROP CONSTRAINT providers_kind_format_check;
ALTER TABLE providers DROP CONSTRAINT providers_api_format_check;
ALTER TABLE providers DROP COLUMN api_format;

-- [{"api": "chat_completions"}, {"api": "messages", "path": "/anthropic/v1/messages"}]:
-- OpenAI-compatible providers only. The rules for the list are the
-- server's (Chat Completions always, each API once, paths checked).
ALTER TABLE providers ADD COLUMN apis jsonb;
UPDATE providers SET apis = '[{"api": "chat_completions"}]' WHERE kind = 'openai_compatible';
ALTER TABLE providers ADD CONSTRAINT providers_apis_check CHECK (
    (kind = 'openai_compatible') = (apis IS NOT NULL)
    AND (apis IS NULL OR jsonb_typeof(apis) = 'array')
);

-- NULL passes for every kind: `NULL AND false` is false, not unknown.
ALTER TABLE providers ADD COLUMN catalog_key text
    CONSTRAINT providers_catalog_key_check CHECK (
        catalog_key IS NULL
        OR (catalog_key ~ '^[a-z0-9][a-z0-9._-]{0,63}$' AND kind = 'openai_compatible')
    );
