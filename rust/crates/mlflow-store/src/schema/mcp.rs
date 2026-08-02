//! MCP registry table names. The physical schema is owned by Alembic revision
//! `a8b9c0d1e2f3` (chain head `6f8d9c3b2a1e`); Rust only consumes its net
//! schema. In particular, `mcp_server_versions` has `connect_options` after
//! `source` and no version-level `display_name` column.

pub const MCP_SERVERS: &str = "mcp_servers";
pub const MCP_SERVER_VERSIONS: &str = "mcp_server_versions";
pub const MCP_SERVER_TAGS: &str = "mcp_server_tags";
pub const MCP_SERVER_VERSION_TAGS: &str = "mcp_server_version_tags";
pub const MCP_SERVER_ALIASES: &str = "mcp_server_aliases";
pub const MCP_ACCESS_ENDPOINTS: &str = "mcp_access_endpoints";
