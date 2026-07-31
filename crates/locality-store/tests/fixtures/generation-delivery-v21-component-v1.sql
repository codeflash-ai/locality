PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

DROP TABLE generation_inode_evidence;

ALTER TABLE generation_paths RENAME TO generation_paths_current;
CREATE TABLE generation_paths (
    mount_id TEXT NOT NULL,
    projection_id TEXT NOT NULL,
    logical_path TEXT NOT NULL,
    base_generation_id TEXT NOT NULL,
    base_identity_json TEXT,
    state TEXT NOT NULL CHECK (state IN ('clean', 'dirty', 'conflicted')),
    incoming_identity_json TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (mount_id, projection_id),
    UNIQUE (mount_id, logical_path),
    FOREIGN KEY (mount_id) REFERENCES mounts(mount_id) ON DELETE CASCADE
);
INSERT INTO generation_paths
SELECT mount_id, projection_id, logical_path, base_generation_id,
       base_identity_json, state, incoming_identity_json, updated_at
FROM generation_paths_current;
DROP TABLE generation_paths_current;

ALTER TABLE generation_apply_outcomes RENAME TO generation_apply_outcomes_current;
ALTER TABLE generation_apply_journals RENAME TO generation_apply_journals_current;
DROP INDEX generation_apply_one_active_per_source;
CREATE TABLE generation_apply_journals (
    delta_id TEXT PRIMARY KEY,
    source_connection_id TEXT NOT NULL,
    base_generation_id TEXT NOT NULL,
    target_generation_id TEXT NOT NULL,
    delta_json TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    receipt_sha256 TEXT NOT NULL,
    stage_root TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('staged', 'applying', 'completed')),
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    completed_at TEXT,
    CHECK (
        (active = 0 AND status = 'completed' AND completed_at IS NOT NULL)
        OR (active = 1 AND status IN ('staged', 'applying') AND completed_at IS NULL)
    )
);
INSERT INTO generation_apply_journals
SELECT delta_id, source_connection_id, base_generation_id, target_generation_id,
       delta_json, receipt_json, receipt_sha256, stage_root, status, active,
       created_at, updated_at, completed_at
FROM generation_apply_journals_current;
CREATE UNIQUE INDEX generation_apply_one_active_per_source
ON generation_apply_journals(source_connection_id) WHERE active = 1;
CREATE TABLE generation_apply_outcomes (
    delta_id TEXT NOT NULL,
    entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
    outcome_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (delta_id, entry_index),
    FOREIGN KEY (delta_id) REFERENCES generation_apply_journals(delta_id) ON DELETE CASCADE
);
INSERT INTO generation_apply_outcomes
SELECT delta_id, entry_index, outcome_json, updated_at
FROM generation_apply_outcomes_current;
DROP TABLE generation_apply_outcomes_current;
DROP TABLE generation_apply_journals_current;

UPDATE state_components
SET version = 21
WHERE component_id = 'core:schema';
UPDATE state_components
SET version = 1, min_reader_version = 1
WHERE component_id = 'durable:generation_delivery';
PRAGMA user_version = 21;
COMMIT;
PRAGMA foreign_keys = ON;
