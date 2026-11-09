-- Sekisho plan 1: providers, models, environments, environment keys,
-- budgets, spend and rendered gateway configs.
-- Money is integer micro-USD; prices are micro-USD per 1,000,000 tokens.

CREATE TABLE providers (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name               text NOT NULL UNIQUE
                       CHECK (name ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    kind               text NOT NULL
                       CHECK (kind IN ('anthropic', 'openai', 'gemini', 'openai_compatible')),
    base_url           text,
    -- AES-256-GCM: 12-byte nonce, ciphertext, 16-byte tag.
    api_key_ciphertext bytea NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE models (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name                   text NOT NULL UNIQUE,
    -- No cascade: a provider with models cannot be deleted.
    provider_id            uuid NOT NULL REFERENCES providers (id),
    -- Both NULL: agentgateway's catalog prices the model.
    input_price_micro_usd  bigint CHECK (input_price_micro_usd >= 0),
    output_price_micro_usd bigint CHECK (output_price_micro_usd >= 0),
    created_at             timestamptz NOT NULL DEFAULT now(),
    CHECK ((input_price_micro_usd IS NULL) = (output_price_micro_usd IS NULL))
);
CREATE INDEX models_provider_id ON models (provider_id);

CREATE TABLE environments (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name          text NOT NULL UNIQUE
                  CHECK (name ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    description   text NOT NULL DEFAULT '',
    notice        text NOT NULL DEFAULT '',
    access_groups text[] NOT NULL DEFAULT '{}',
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE environment_models (
    environment_id uuid NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    -- No cascade: a model an environment lists cannot be deleted.
    model_id       uuid NOT NULL REFERENCES models (id),
    -- The model's place in the environment's list.
    position       integer NOT NULL CHECK (position >= 0),
    PRIMARY KEY (environment_id, model_id),
    UNIQUE (environment_id, position)
);
CREATE INDEX environment_models_model_id ON environment_models (model_id);

CREATE TABLE env_keys (
    -- The public part of the token sek_<id>_<secret>.
    id             text PRIMARY KEY CHECK (id ~ '^[a-z2-7]{12}$'),
    environment_id uuid NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    owner          text NOT NULL,
    name           text NOT NULL,
    -- Only the hash of the secret is kept; the token is shown once.
    secret_sha256  bytea NOT NULL CHECK (length(secret_sha256) = 32),
    expires_at     timestamptz,
    suspended      boolean NOT NULL DEFAULT false,
    revoked_at     timestamptz,
    created_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (environment_id, name)
);

CREATE TABLE budgets (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name            text NOT NULL,
    scope           text NOT NULL CHECK (scope IN ('person', 'environment', 'key')),
    -- person: optional (NULL means across all environments);
    -- environment: required; key: unused (the key names its environment).
    environment_id  uuid REFERENCES environments (id) ON DELETE CASCADE,
    key_id          text REFERENCES env_keys (id) ON DELETE CASCADE,
    person          text,
    limit_micro_usd bigint NOT NULL CHECK (limit_micro_usd >= 0),
    window_kind     text NOT NULL CHECK (window_kind IN ('day', 'week', 'month')),
    calendar        boolean NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CHECK (CASE scope
        WHEN 'person'      THEN person IS NOT NULL AND key_id IS NULL
        WHEN 'environment' THEN environment_id IS NOT NULL AND person IS NULL AND key_id IS NULL
        WHEN 'key'         THEN key_id IS NOT NULL AND person IS NULL AND environment_id IS NULL
    END)
);
CREATE INDEX budgets_environment_id ON budgets (environment_id);
CREATE INDEX budgets_key_id ON budgets (key_id);
CREATE INDEX budgets_person ON budgets (person);

-- One row per budget and window; charges add to it atomically.
CREATE TABLE spend (
    budget_id       uuid NOT NULL REFERENCES budgets (id) ON DELETE CASCADE,
    window_start    timestamptz NOT NULL,
    spent_micro_usd bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (budget_id, window_start)
);

-- Every published agentgateway config; the gateway agent polls for newer versions.
CREATE TABLE gateway_config (
    version    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    yaml       text NOT NULL,
    sha256     bytea NOT NULL CHECK (length(sha256) = 32),
    created_at timestamptz NOT NULL DEFAULT now()
);
