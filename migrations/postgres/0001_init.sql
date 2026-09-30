-- DatI metadata store, PostgreSQL dialect.
-- Timestamps are ISO-8601 UTC text: sqlx's Any driver cannot bind chrono types, and text
-- sorts chronologically so ORDER BY created_at stays correct on every dialect.

CREATE TABLE users (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64) NOT NULL UNIQUE,
    password VARCHAR(256) NOT NULL,
    display_name VARCHAR(64)
);

CREATE TABLE api_key (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    user_id VARCHAR(36) NOT NULL,
    name VARCHAR(64) NOT NULL,
    key_hash VARCHAR(64) NOT NULL UNIQUE,
    key_mask VARCHAR(32) NOT NULL,
    expires_at VARCHAR(32),
    last_used_at VARCHAR(32)
);

CREATE TABLE data_source (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    connection_url VARCHAR(256),
    type VARCHAR(32),
    user_name VARCHAR(64),
    encrypted_password VARCHAR(512),
    default_schema VARCHAR(256)
);
CREATE INDEX idx_data_source_deleted_created ON data_source (deleted, created_at);

CREATE TABLE table_info (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    data_source_id VARCHAR(36) NOT NULL,
    table_schema VARCHAR(64),
    aliases TEXT
);
CREATE INDEX idx_table_info_datasource ON table_info (data_source_id, deleted);

CREATE TABLE column_info (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    table_id VARCHAR(36) NOT NULL,
    column_type VARCHAR(64),
    aliases TEXT,
    extract_value_enabled BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX idx_column_info_table ON column_info (table_id, deleted);

-- Replaces the Elasticsearch FIELD_VALUE documents the Java version kept column values in.
CREATE TABLE column_value (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    table_id VARCHAR(36) NOT NULL,
    column_name VARCHAR(128) NOT NULL,
    value_text VARCHAR(512) NOT NULL,
    synonyms TEXT,
    sort_no INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_column_value_table_column ON column_value (table_id, column_name);

CREATE TABLE subject (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    datasource_id VARCHAR(64) NOT NULL,
    aliases TEXT
);
CREATE INDEX idx_subject_datasource ON subject (datasource_id, deleted);

CREATE TABLE subject_table (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    subject_id VARCHAR(64) NOT NULL,
    table_id VARCHAR(64) NOT NULL
);
CREATE UNIQUE INDEX uk_subject_table ON subject_table (subject_id, table_id);
CREATE INDEX idx_subject_table_table ON subject_table (table_id);

CREATE TABLE term (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    subject_id VARCHAR(64) NOT NULL,
    aliases TEXT
);
CREATE INDEX idx_term_subject ON term (subject_id, deleted);

CREATE TABLE term_relation (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    term_id VARCHAR(64) NOT NULL,
    entity_type VARCHAR(32) NOT NULL,
    table_id VARCHAR(64) NOT NULL,
    field_name VARCHAR(128)
);
CREATE INDEX idx_term_relation_term ON term_relation (term_id);

CREATE TABLE resource_acl (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    resource_type VARCHAR(32) NOT NULL,
    resource_id VARCHAR(36) NOT NULL,
    principal_type VARCHAR(16) NOT NULL,
    principal_id VARCHAR(64) NOT NULL,
    permission VARCHAR(16) NOT NULL
);
CREATE UNIQUE INDEX uk_resource_acl ON resource_acl (resource_type, resource_id, principal_type, principal_id);

CREATE TABLE mcp_service (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    code VARCHAR(64) NOT NULL UNIQUE,
    status VARCHAR(20),
    active_version_id VARCHAR(36),
    active_version_number INTEGER
);
CREATE INDEX idx_mcp_service_deleted_created ON mcp_service (deleted, created_at);

CREATE TABLE mcp_service_snapshot (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(36) NOT NULL,
    version_number INTEGER NOT NULL,
    release_note VARCHAR(500),
    snapshot_content TEXT NOT NULL
);
CREATE UNIQUE INDEX uk_mcp_snapshot_version ON mcp_service_snapshot (service_id, version_number);

CREATE TABLE mcp_service_data_scope (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(64) NOT NULL,
    scope_type VARCHAR(20) NOT NULL,
    reference_id VARCHAR(64) NOT NULL
);
CREATE INDEX idx_mcp_scope_service ON mcp_service_data_scope (service_id);

CREATE TABLE mcp_prebuilt_tool_config (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    name VARCHAR(64),
    description VARCHAR(1024),
    deleted BOOLEAN NOT NULL DEFAULT false,
    service_id VARCHAR(64) NOT NULL,
    tool_type VARCHAR(30) NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    config TEXT
);
CREATE UNIQUE INDEX uk_mcp_prebuilt_tool ON mcp_prebuilt_tool_config (service_id, tool_type);

CREATE TABLE mcp_custom_tool (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(64) NOT NULL,
    name VARCHAR(128) NOT NULL,
    description TEXT NOT NULL,
    tool_type VARCHAR(30) NOT NULL,
    title VARCHAR(255),
    enabled BOOLEAN NOT NULL DEFAULT true,
    config TEXT
);
CREATE UNIQUE INDEX uk_mcp_custom_tool_name ON mcp_custom_tool (service_id, name);

CREATE TABLE mcp_prompt (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(64) NOT NULL,
    name VARCHAR(128) NOT NULL,
    description TEXT,
    enabled BOOLEAN NOT NULL DEFAULT true,
    content TEXT NOT NULL,
    parameters TEXT
);
CREATE UNIQUE INDEX uk_mcp_prompt_name ON mcp_prompt (service_id, name);

CREATE TABLE mcp_invocation_log (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(64) NOT NULL,
    method VARCHAR(32) NOT NULL,
    tool_name VARCHAR(64),
    caller_id VARCHAR(64),
    caller_name VARCHAR(64),
    client_ip VARCHAR(45),
    success BOOLEAN NOT NULL DEFAULT false,
    duration_ms BIGINT NOT NULL DEFAULT 0,
    error_message VARCHAR(512),
    stat_date VARCHAR(10) NOT NULL
);
CREATE INDEX idx_mcp_log_svc_created ON mcp_invocation_log (service_id, created_at);
CREATE INDEX idx_mcp_log_created ON mcp_invocation_log (created_at);

CREATE TABLE mcp_usage_daily_stat (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(64) NOT NULL,
    tool_name VARCHAR(64) NOT NULL DEFAULT '',
    stat_date VARCHAR(10) NOT NULL,
    total_calls BIGINT NOT NULL DEFAULT 0,
    success_calls BIGINT NOT NULL DEFAULT 0,
    failed_calls BIGINT NOT NULL DEFAULT 0,
    total_duration_ms BIGINT NOT NULL DEFAULT 0,
    max_duration_ms BIGINT NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX uk_mcp_stat_svc_tool_date ON mcp_usage_daily_stat (service_id, tool_name, stat_date);
CREATE INDEX idx_mcp_stat_svc_date ON mcp_usage_daily_stat (service_id, stat_date);

CREATE TABLE mcp_metadata_audit_log (
    id VARCHAR(36) NOT NULL PRIMARY KEY,
    created_by VARCHAR(64),
    created_at VARCHAR(32),
    updated_by VARCHAR(64),
    updated_at VARCHAR(32),
    service_id VARCHAR(64),
    tool_type VARCHAR(30),
    entity_type VARCHAR(20),
    entity_id VARCHAR(36),
    entity_name VARCHAR(255),
    change_type VARCHAR(10),
    old_value TEXT,
    new_value TEXT
);
CREATE INDEX idx_mcp_audit_entity ON mcp_metadata_audit_log (entity_type, entity_id);
