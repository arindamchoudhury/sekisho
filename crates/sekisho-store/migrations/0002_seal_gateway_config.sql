-- Rendered gateway configs hold decrypted provider keys, so they are sealed
-- at rest like the keys themselves (AES-256-GCM: 12-byte nonce, ciphertext,
-- 16-byte tag), and the store keeps only recent versions.
--
-- Plaintext versions written before this migration are deleted rather than
-- kept; the server publishes a fresh, sealed version when it starts. Version
-- numbers keep counting up, so gateway agents simply get the next one.
DELETE FROM gateway_config;

ALTER TABLE gateway_config
    DROP COLUMN yaml,
    DROP COLUMN sha256,
    ADD COLUMN yaml_ciphertext bytea NOT NULL;
