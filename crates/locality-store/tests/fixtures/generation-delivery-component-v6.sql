PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

ALTER TABLE generation_paths RENAME TO generation_paths_v7;
ALTER TABLE observed_generations RENAME TO observed_generations_v7;

CREATE TABLE observed_generations (
    mount_id TEXT PRIMARY KEY,
    source_connection_id TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    inventory_sha256 TEXT NOT NULL,
    workspace_layout_version INTEGER NOT NULL CHECK (workspace_layout_version > 0),
    workspace_layout_digest TEXT NOT NULL,
    last_receipt_sha256 TEXT,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
);
INSERT INTO observed_generations
SELECT mount_id, source_connection_id, generation_id, inventory_sha256,
       workspace_layout_version, workspace_layout_digest,
       last_receipt_sha256, updated_at
FROM observed_generations_v7;

CREATE TABLE generation_paths (
    mount_id TEXT NOT NULL,
    projection_id TEXT NOT NULL,
    logical_path TEXT NOT NULL,
    local_logical_path TEXT NOT NULL,
    base_generation_id TEXT NOT NULL,
    base_identity_json TEXT,
    base_payload_delta_id TEXT,
    base_payload_entry_index INTEGER CHECK (base_payload_entry_index >= 0),
    conflict_payload_delta_id TEXT,
    conflict_payload_entry_index INTEGER CHECK (conflict_payload_entry_index >= 0),
    state TEXT NOT NULL CHECK (state IN ('clean', 'dirty', 'conflicted')),
    incoming_identity_json TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (mount_id, projection_id),
    UNIQUE (mount_id, logical_path),
    FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
);
INSERT INTO generation_paths
SELECT mount_id, projection_id, logical_path, local_logical_path,
       base_generation_id, base_identity_json, base_payload_delta_id,
       base_payload_entry_index, conflict_payload_delta_id,
       conflict_payload_entry_index, state, incoming_identity_json, updated_at
FROM generation_paths_v7;

DROP TABLE generation_paths_v7;
DROP TABLE observed_generations_v7;

UPDATE state_components
SET version = 6, min_reader_version = 6
WHERE component_id = 'durable:generation_delivery';
COMMIT;
PRAGMA foreign_keys = ON;
