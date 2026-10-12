//! Internal HTTP helpers around `ureq` 2.x.

use std::time::Duration;

use crate::error::Result;

/// Agent for plain-HTTP calls on the LAN (REST :8001, DLNA :9197, DIAL :8080).
pub(crate) fn lan_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(timeout)
        .build()
}

/// Agent for HTTPS calls with normal certificate validation (SmartThings).
pub(crate) fn tls_agent(timeout: Duration) -> Result<ureq::Agent> {
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(timeout);
    Ok(crate::tls::verified(builder)?.build())
}

/// Map a ureq result to (status, body) without treating 4xx/5xx as transport errors.
pub(crate) fn status_and_body(
    r: std::result::Result<ureq::Response, ureq::Error>,
) -> Result<(u16, String)> {
    match r {
        Ok(resp) => {
            let code = resp.status();
            Ok((code, resp.into_string()?))
        }
        Err(ureq::Error::Status(code, resp)) => Ok((code, resp.into_string().unwrap_or_default())),
        Err(e) => Err(e.into()),
    }
}
