//! `read_web_page` backends: shared request type plus the backend dispatch.
//! [`run_read`] selects the native pinned-fetch reader or the Jina Reader, then
//! applies deterministic objective-excerpting uniformly when the caller passed
//! an `objective` (so both backends share one excerpt path).

mod jina;
mod native;

use tokio_util::sync::CancellationToken;

use super::fetch::Resolver;
use super::{PageResult, ReadBackend, WebToolsConfig};

/// Character budget for objective-based excerpts. Keeps a focused read well
/// under the oversized-output threshold; a full read (no objective) is bounded
/// by the fetch body cap instead and offloaded if still large.
///
/// `pub(super)` so the token-efficiency corpus (`web::corpus`) measures
/// `select_excerpts` with the exact production budget (ADR-0036 rule 5).
pub(super) const EXCERPT_BUDGET_CHARS: usize = 8_000;

/// A parsed read request (from the tool arguments).
#[derive(Debug, Clone)]
pub(super) struct ReadRequest {
    pub(super) url: String,
    pub(super) objective: Option<String>,
}

/// Dispatch a read to the resolved backend, then excerpt if an objective was
/// given. Backends surface actionable errors (SSRF denial, PDF/JS-shell
/// diagnostic, Jina throttle) as `anyhow::Error` or as an honest diagnostic in
/// [`PageResult::content`].
pub(super) async fn run_read(
    backend: ReadBackend,
    config: &WebToolsConfig,
    request: &ReadRequest,
    resolver: &dyn Resolver,
    cancel: &CancellationToken,
) -> anyhow::Result<PageResult> {
    let mut page = match backend {
        ReadBackend::Native => native::read(config, request, resolver, cancel).await?,
        ReadBackend::Jina => {
            jina::read(config, config.jina_key.as_deref(), request, cancel).await?
        }
    };

    if let Some(objective) = request
        .objective
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        let full_len = page.content.chars().count();
        let excerpted =
            super::excerpts::select_excerpts(&page.content, objective, EXCERPT_BUDGET_CHARS);
        if excerpted.chars().count() < full_len {
            page.truncated = true;
        }
        page.content = excerpted;
    }

    Ok(page)
}
