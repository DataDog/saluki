//! POSTing an oracle's parameters to the intake.
//!
//! The intake owns the point store, so it runs both comparisons and makes the SDK calls itself. A
//! check sends parameters and reads a status. There is no result body to interpret.

use std::time::Duration;

use antithesis_sdk::prelude::*;
use anyhow::Context;
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::json;

/// How long a check waits on the intake before giving up.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Build the blocking client every check uses.
///
/// # Errors
///
/// Errors when the client cannot be constructed.
pub fn client() -> anyhow::Result<Client> {
    Client::builder().timeout(TIMEOUT).build().context("build HTTP client")
}

/// POST `params` to an oracle and return whether the intake ran it.
///
/// No fault is injected while `eventually_` and `finally_` checks run, so a transport failure here is
/// a real defect rather than an artifact and the caller does not swallow it. A 4xx means the check
/// built parameters the intake rejected, which the check itself is responsible for, so it fires
/// `assert_unreachable!`.
///
/// # Errors
///
/// Errors when the request cannot be sent or the intake answers 5xx.
pub fn post<P: Serialize>(client: &Client, intake_addr: &str, path: &str, params: &P) -> anyhow::Result<()> {
    let url = format!("http://{intake_addr}/antithesis/metrics/{path}");
    let response = client
        .post(&url)
        .json(params)
        .send()
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;

    if response.status().is_client_error() {
        assert_unreachable!(
            "differential.oracle_rejected_parameters",
            &json!({ "path": path, "status": response.status().as_u16() })
        );
    }
    response
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("POST {url} returned an error status: {e}"))?;
    Ok(())
}
