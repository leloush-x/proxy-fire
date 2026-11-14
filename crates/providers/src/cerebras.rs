//! Provider: Cerebras
//! Docs: https://inference-docs.cerebras.ai/support/rate-limits
//! Free tier: all models, RPS limits (not RPM). Headers on every response.
//! x-ratelimit-reset-tokens is fractional float seconds (see core::headers).
//! Pre-emptive spacing/slowdown lives in the proxy fallback chain.

use crate::{generic, Provider, ProviderError};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;

pub struct Cerebras;

impl Provider for Cerebras {
    fn id(&self) -> &'static str {
        "cerebras"
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move { generic::DEFAULT.scan(client, cfg).await })
    }
}
