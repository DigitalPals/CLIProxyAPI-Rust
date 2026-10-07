//! Durable, metadata-only usage accounting. Provider allowance stays in quota.rs.
pub mod api;
pub mod capture;
pub mod collector;
pub mod imports;
pub mod pricing;
pub mod store;
pub mod types;

pub fn database_path(cfg: &crate::config::Config, config_path: &std::path::Path) -> std::path::PathBuf {
    cfg.usage
        .database
        .as_deref()
        .map(crate::config::expand_home)
        .unwrap_or_else(|| config_path.with_file_name("usage.sqlite3"))
}

#[cfg(test)]
mod integration_tests;
