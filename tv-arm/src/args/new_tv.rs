//! The local-chart output destination implied by a frozen spec input.

use local_chart_client::DEFAULT_LOCAL_CHART_URL;
use url::Url;

/// An HTTP spec belongs to its server; a file belongs to the default chart.
/// Invalid spec URLs are rejected by the spec reader before replay output.
pub(super) fn inferred_url(spec_url: Option<&str>) -> String {
    spec_url
        .and_then(|raw| Url::parse(raw).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| DEFAULT_LOCAL_CHART_URL.to_string())
}
