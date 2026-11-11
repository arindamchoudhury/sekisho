-- Sekisho plan 1c: a model's prices have four parts. Cache read and cache
-- write prices, in micro-USD per 1,000,000 tokens like the input and output
-- prices, are optional and only set together with input and output prices;
-- when unset, the provider's usual ratios to the input price apply.

ALTER TABLE models
    ADD COLUMN cache_read_price_micro_usd bigint
        CONSTRAINT models_cache_read_price_check CHECK (cache_read_price_micro_usd >= 0),
    ADD COLUMN cache_write_price_micro_usd bigint
        CONSTRAINT models_cache_write_price_check CHECK (cache_write_price_micro_usd >= 0),
    ADD CONSTRAINT models_cache_prices_need_prices_check CHECK (
        input_price_micro_usd IS NOT NULL
        OR (cache_read_price_micro_usd IS NULL AND cache_write_price_micro_usd IS NULL)
    );
