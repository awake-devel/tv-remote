//! DIAL app launching (`http://<host>:8080/ws/apps/<App>`): YouTube and Netflix answer.

use std::time::Duration;

use crate::dlna::field;
use crate::error::{Error, Result};
use crate::http::{lan_agent, status_and_body};

/// DIAL port.
pub const DIAL_PORT: u16 = 8080;

/// Extract a YouTube video id from an id, a `watch?v=` URL, a `youtu.be/` link,
/// or a `/shorts/`, `/embed/` or `/live/` URL. Pure.
pub fn parse_youtube_id(input: &str) -> Option<String> {
    let s = input.trim();
    let valid = |id: &str| {
        id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if valid(s) {
        return Some(s.to_string());
    }
    let take_id = |rest: &str| -> Option<String> {
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        valid(&id).then_some(id)
    };
    if let Some(i) = s.find("v=") {
        // Make sure it is a query parameter (`?v=` or `&v=`).
        if i > 0 && matches!(s.as_bytes()[i - 1], b'?' | b'&') {
            if let Some(id) = take_id(&s[i + 2..]) {
                return Some(id);
            }
        }
    }
    for marker in ["youtu.be/", "/shorts/", "/embed/", "/live/"] {
        if let Some(i) = s.find(marker) {
            if let Some(id) = take_id(&s[i + marker.len()..]) {
                return Some(id);
            }
        }
    }
    None
}

/// DIAL client for one TV.
#[derive(Clone)]
pub struct Dial {
    host: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Dial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dial").field("host", &self.host).finish()
    }
}

impl Dial {
    /// New client (no traffic until used).
    pub fn new(host: &str) -> Dial {
        Dial {
            host: host.to_string(),
            agent: lan_agent(Duration::from_secs(8)),
        }
    }

    fn url(&self, app: &str) -> String {
        format!("http://{}:{DIAL_PORT}/ws/apps/{app}", self.host)
    }

    /// `POST /ws/apps/<app>` with a `text/plain` body (may be empty).
    pub fn launch(&self, app: &str, body: &str) -> Result<()> {
        let r = self
            .agent
            .post(&self.url(app))
            .set("Content-Type", "text/plain; charset=utf-8")
            .send_string(body);
        let (code, _) = status_and_body(r)?;
        if (200..300).contains(&code) {
            Ok(())
        } else {
            Err(Error::Http(format!("DIAL launch {app}: HTTP {code}")))
        }
    }

    /// `GET /ws/apps/<app>` → the `<state>` value (`running`, `stopped`, ...).
    pub fn state(&self, app: &str) -> Result<String> {
        let (code, body) = status_and_body(self.agent.get(&self.url(app)).call())?;
        if code >= 400 {
            return Err(Error::Http(format!("DIAL state {app}: HTTP {code}")));
        }
        Ok(field(&body, "state"))
    }

    /// Open a YouTube video by id or URL.
    pub fn youtube(&self, id_or_url: &str) -> Result<()> {
        let id = parse_youtube_id(id_or_url)
            .ok_or_else(|| Error::parse(format!("not a YouTube video id or URL: {id_or_url}")))?;
        self.launch("YouTube", &format!("v={id}"))
    }

    /// Launch Netflix.
    pub fn netflix(&self) -> Result<()> {
        self.launch("Netflix", "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_ids() {
        let id = Some("dQw4w9WgXcQ".to_string());
        assert_eq!(parse_youtube_id("dQw4w9WgXcQ"), id);
        assert_eq!(parse_youtube_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"), id);
        assert_eq!(parse_youtube_id("https://youtu.be/dQw4w9WgXcQ?si=x"), id);
        assert_eq!(parse_youtube_id("https://youtube.com/shorts/dQw4w9WgXcQ"), id);
        assert_eq!(parse_youtube_id("https://m.youtube.com/watch?feature=x&v=dQw4w9WgXcQ"), id);
        assert_eq!(parse_youtube_id("hello"), None);
    }
}
