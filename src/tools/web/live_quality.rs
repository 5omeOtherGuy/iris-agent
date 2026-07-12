//! Explicitly opt-in live quality probe. Never runs in CI or ordinary ignored tests.

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::super::fetch::SystemResolver;
    use super::super::read::{self, ReadRequest};
    use super::super::{ReadBackend, WebToolsConfig};

    #[tokio::test]
    #[ignore = "live network benchmark; requires IRIS_WEB_LIVE_BENCH=1 and human approval"]
    async fn native_reader_live_quality() {
        assert_eq!(
            std::env::var("IRIS_WEB_LIVE_BENCH").ok().as_deref(),
            Some("1"),
            "set IRIS_WEB_LIVE_BENCH=1 only after explicit human approval"
        );
        let config = WebToolsConfig {
            read_timeout: Duration::from_secs(30),
            max_read_response_bytes: 200 * 1024,
            ..WebToolsConfig::default()
        };
        for url in [
            "https://www.rust-lang.org/learn",
            "https://docs.rs/reqwest/latest/reqwest/",
        ] {
            let page = read::run_read(
                ReadBackend::Native,
                &config,
                &ReadRequest {
                    url: url.to_string(),
                    objective: None,
                },
                &SystemResolver,
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{url}: {error:#}"));
            assert!(page.content.len() >= 200, "{url}: extraction too small");
            println!("{url}\t{} bytes\t{:?}", page.content.len(), page.title);
        }
    }
}
