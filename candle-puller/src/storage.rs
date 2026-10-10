//! Candle history shares its own database, independently of worker plan state.
use candle_cache::storage::{StorageBackend, postgres::PostgresStorage};
use color_eyre::Result;
use std::{path::Path, sync::Arc};

const DEFAULT_URL: &str = "postgresql://candle_cache:candle_cache@localhost:5432/candle_cache";

pub(super) async fn open(path: &Path) -> Result<Arc<dyn StorageBackend>> {
    // DATABASE_URL belongs to the worker's trade-state database. Never inherit
    // it here: chart and replay use the shared candle_cache database.
    let override_url = std::env::var("CANDLE_CACHE_DATABASE_URL").ok();
    let url = database_url(override_url.as_deref());
    Ok(Arc::new(PostgresStorage::with_url(path, url).await?))
}

fn database_url(override_url: Option<&str>) -> &str {
    override_url.unwrap_or(DEFAULT_URL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_has_its_own_database_and_supports_explicit_override() {
        assert_eq!(database_url(None), DEFAULT_URL);
        assert_eq!(
            database_url(Some("postgresql://history/custom")),
            "postgresql://history/custom"
        );
    }
}
