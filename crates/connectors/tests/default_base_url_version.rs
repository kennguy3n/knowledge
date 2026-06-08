//! Regression guard for the doubled-API-version URL bug.
//!
//! Several connectors bake the provider's API version into
//! `DEFAULT_API_BASE_URL` (e.g. `https://global-api.afterpay.com/v2`).
//! The request paths must therefore be version-less, otherwise the
//! built URL carries the version twice (`.../v2/v1/orders`) and 404s in
//! production.
//!
//! Every other unit test overrides `api_base_url` with a version-less
//! test host, so the production default is never exercised — which is
//! exactly how the doubled-version defect shipped undetected. These
//! tests deliberately drive each connector with **no** `api_base_url`
//! override so the real `DEFAULT_API_BASE_URL` constant is used, and
//! assert the resulting request URL matches the single-version target.

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorInstanceId, ConnectorKind, HttpMethod,
    HttpTransport, MockHttpTransport, MockResponse, OAuth2CodeExchange, OAuth2Token, Result,
};
use evidence_store::ScopeId;

struct FixedOAuth;
impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "unused",
            "unused",
            Utc::now() + Duration::hours(1),
            "scope",
        ))
    }
}

fn oauth() -> Arc<dyn OAuth2CodeExchange> {
    Arc::new(FixedOAuth)
}

/// Builds each connector with its production `DEFAULT_API_BASE_URL`
/// (no `api_base_url` override), serves a single-record page at the
/// expected single-version URL, and asserts the connector actually
/// requested that URL. A doubled-version regression would request an
/// unregistered URL (mock returns 404 → `initial_sync` errors) and
/// fail the `expect`, and would also trip the exact-URL assertion.
macro_rules! default_base_url_case {
    ($test:ident, $conn:ty, $base:expr, $kind:expr, $expected:expr) => {
        #[test]
        fn $test() {
            let transport = Arc::new(MockHttpTransport::new());
            transport.expect(
                HttpMethod::Get,
                $expected,
                MockResponse::ok_json(
                    serde_json::to_vec(&serde_json::json!({
                        "data": [ { "id": "rec-1", "updated_at": "2024-01-01T00:00:00Z" } ]
                    }))
                    .unwrap(),
                ),
            );
            let t: Arc<dyn HttpTransport> = transport.clone();
            let connector =
                <$conn>::new(ConnectorInstanceId::new_v4(), t, oauth()).with_page_size(2);
            let config = ConnectorConfig::new($kind, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "api_key": "test-key" }));

            let token = connector.authenticate(&config).expect("authenticate");
            let res = connector.initial_sync(&config, &token).expect(
                "initial_sync should hit the default base URL with a single API-version segment",
            );
            assert_eq!(res.events.len(), 1, "one record served");

            let recorded = transport.recorded();
            assert_eq!(
                recorded[0].url, $expected,
                "request must use the versioned default base URL exactly once"
            );
            assert!(
                recorded[0].url.starts_with($base),
                "request URL must extend DEFAULT_API_BASE_URL ({})",
                $base
            );
        }
    };
}

default_base_url_case!(
    afterpay_default_base_url_has_single_version,
    connectors::afterpay::AfterpayConnector,
    connectors::afterpay::DEFAULT_API_BASE_URL,
    ConnectorKind::Afterpay,
    "https://global-api.afterpay.com/v2/orders?limit=2&offset=0"
);

default_base_url_case!(
    deputy_default_base_url_has_single_version,
    connectors::deputy::DeputyConnector,
    connectors::deputy::DEFAULT_API_BASE_URL,
    ConnectorKind::Deputy,
    "https://api.deputy.com/api/v1/timesheets?limit=2&offset=0"
);

default_base_url_case!(
    employment_hero_default_base_url_has_single_version,
    connectors::employment_hero::EmploymentHeroConnector,
    connectors::employment_hero::DEFAULT_API_BASE_URL,
    ConnectorKind::EmploymentHero,
    "https://api.employmenthero.com/api/v1/employees?limit=2&offset=0"
);

default_base_url_case!(
    campaign_monitor_default_base_url_has_single_version,
    connectors::campaign_monitor::CampaignMonitorConnector,
    connectors::campaign_monitor::DEFAULT_API_BASE_URL,
    ConnectorKind::CampaignMonitor,
    "https://api.createsend.com/api/v3.3/subscribers?limit=2&offset=0"
);

// Connectors outside the Australia batch that carried the same
// versioned-base + `/v1/`-path defect.

default_base_url_case!(
    freeagent_default_base_url_has_single_version,
    connectors::freeagent::FreeAgentConnector,
    connectors::freeagent::DEFAULT_API_BASE_URL,
    ConnectorKind::FreeAgent,
    "https://api.freeagent.com/v2/invoices?limit=2&offset=0"
);

default_base_url_case!(
    mangopay_default_base_url_has_single_version,
    connectors::mangopay::MangoPayConnector,
    connectors::mangopay::DEFAULT_API_BASE_URL,
    ConnectorKind::MangoPay,
    "https://api.mangopay.com/v2.01/payins?limit=2&offset=0"
);

default_base_url_case!(
    ovh_cloud_default_base_url_has_single_version,
    connectors::ovh_cloud::OvhCloudConnector,
    connectors::ovh_cloud::DEFAULT_API_BASE_URL,
    ConnectorKind::OvhCloud,
    "https://eu.api.ovh.com/1.0/services?limit=2&offset=0"
);

default_base_url_case!(
    pennylane_default_base_url_has_single_version,
    connectors::pennylane::PennylaneConnector,
    connectors::pennylane::DEFAULT_API_BASE_URL,
    ConnectorKind::Pennylane,
    "https://app.pennylane.com/api/external/v1/invoices?limit=2&offset=0"
);

default_base_url_case!(
    qonto_default_base_url_has_single_version,
    connectors::qonto::QontoConnector,
    connectors::qonto::DEFAULT_API_BASE_URL,
    ConnectorKind::Qonto,
    "https://thirdparty.qonto.com/v2/transactions?limit=2&offset=0"
);

default_base_url_case!(
    revolut_business_default_base_url_has_single_version,
    connectors::revolut_business::RevolutBusinessConnector,
    connectors::revolut_business::DEFAULT_API_BASE_URL,
    ConnectorKind::RevolutBusiness,
    "https://b2b.revolut.com/api/1.0/transactions?limit=2&offset=0"
);

default_base_url_case!(
    sendinblue_default_base_url_has_single_version,
    connectors::sendinblue::SendinblueConnector,
    connectors::sendinblue::DEFAULT_API_BASE_URL,
    ConnectorKind::Sendinblue,
    "https://api.brevo.com/v3/contacts?limit=2&offset=0"
);

default_base_url_case!(
    shopee_regional_default_base_url_has_single_version,
    connectors::shopee_regional::ShopeeRegionalConnector,
    connectors::shopee_regional::DEFAULT_API_BASE_URL,
    ConnectorKind::ShopeeRegional,
    "https://partner.shopeemobile.com/api/v2/orders?limit=2&offset=0"
);

default_base_url_case!(
    starling_default_base_url_has_single_version,
    connectors::starling::StarlingConnector,
    connectors::starling::DEFAULT_API_BASE_URL,
    ConnectorKind::Starling,
    "https://api.starlingbank.com/api/v2/transactions?limit=2&offset=0"
);
