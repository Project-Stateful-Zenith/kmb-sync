use anyhow::{Result, anyhow};
use kmb_store::Store;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Clone)]
pub struct StoreHandle(Arc<Mutex<Store>>);

impl StoreHandle {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self(Arc::new(Mutex::new(store)))
    }

    pub async fn call<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Store) -> kmb_store::Result<R> + Send + 'static,
    {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = inner.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut store)
        })
        .await
        .map_err(|e| anyhow!("store task failed: {e}"))?
        .map_err(Into::into)
    }
}
