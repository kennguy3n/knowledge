-- Initialise the knowledge database and enable pgvector.
-- This runs once when the postgres container is first started.

CREATE EXTENSION IF NOT EXISTS vector;

-- Placeholder tables for gateway-level stores (tenants, audit, etc.)
-- are created by the Go server's auto-migration on first boot. This
-- script only ensures the pgvector extension is available.
