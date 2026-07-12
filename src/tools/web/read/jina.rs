//! Jina Reader backend (`https://r.jina.ai/<url>`): renders JavaScript and
//! handles PDFs, shifting that rendering risk to Jina. Two guarantees the plan
//! (§3.4, §4) requires:
//!
//! - The target URL passes our own [`policy`](super::super::policy) gate FIRST,
//!   so we never ask Jina to fetch a localhost/private/special-purpose URL on
//!   our behalf.
//! - The request goes through the shared API client (redirects disabled, size
//!   /time capped). The key is optional (keyless = throttled anonymous tier).

use anyhow::{Result, anyhow, bail};
use reqwest::header::AUTHORIZATION;
use tokio_util::sync::CancellationToken;

use super::super::fetch::{self, FetchError};
use super::super::policy;
use super::super::{MAX_BODY_BYTES, PageResult};
use super::ReadRequest;

/// Fetch `request.url` through Jina Reader as Markdown. The key, when present,
/// lifts the anonymous throttle; its absence is a documented degraded tier, not
/// an error.
pub(super) async fn read(
    key: Option<&str>,
    request: &ReadRequest,
    cancel: &CancellationToken,
) -> Result<PageResult> {
    // Validate the TARGET locally before handing it to Jina.
    let validated = policy::validate_external_url(&request.url)
        .map_err(|e| anyhow!("read_web_page (jina): {e}"))?;
    let target = validated.url.to_string();

    let client = fetch::build_api_client().map_err(anyhow::Error::new)?;
    // Jina's documented convention: append the full target URL after the host.
    let endpoint = format!("https://r.jina.ai/{target}");
    let mut req = client
        .get(&endpoint)
        .header("x-respond-with", "markdown")
        .header("x-engine", "auto");
    if let Some(k) = key.map(str::trim).filter(|k| !k.is_empty()) {
        req = req.header(AUTHORIZATION, format!("Bearer {k}"));
    }

    let (status, bytes, truncated) = fetch::send_api(req, MAX_BODY_BYTES, cancel)
        .await
        .map_err(map_fetch_error)?;

    if status != 200 {
        bail!(
            "Jina Reader returned HTTP {status} for {target}. \
             {}Check the readWebPageBackend row in settings.",
            if key.is_none() {
                "No JINA_API_KEY is set (anonymous tier is heavily throttled). "
            } else {
                ""
            }
        );
    }

    let content = String::from_utf8_lossy(&bytes).into_owned();
    if content.trim().is_empty() {
        bail!("Jina Reader returned an empty body for {target}.");
    }

    Ok(PageResult {
        content,
        final_url: target,
        status,
        title: None,
        truncated,
        redirects: 0,
    })
}

/// Map the typed fetch error to an actionable message that names the Jina
/// backend and the cause (plan §4: no silent fallback).
fn map_fetch_error(e: FetchError) -> anyhow::Error {
    anyhow!("Jina Reader request failed: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_private_target_before_contacting_jina() {
        let req = ReadRequest {
            url: "http://169.254.169.254/latest/meta-data".to_string(),
            objective: None,
        };
        let err = read(None, &req, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reserved or private"), "{err}");
    }

    #[tokio::test]
    async fn rejects_non_http_target() {
        let req = ReadRequest {
            url: "file:///etc/passwd".to_string(),
            objective: None,
        };
        assert!(read(None, &req, &CancellationToken::new()).await.is_err());
    }
}
