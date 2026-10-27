use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
}

#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    cookies: Vec<StoredCookie>,
}

impl CookieJar {
    pub fn from_saved(cookies: Vec<StoredCookie>) -> Self {
        Self { cookies }
    }

    pub fn saved(&self) -> Vec<StoredCookie> {
        self.cookies.clone()
    }

    pub fn store_headers(&mut self, url: &Url, headers: &reqwest::header::HeaderMap) {
        let fallback = url.host_str().unwrap_or_default().to_string();
        for value in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(text) = value.to_str() else { continue };
            let (pair, attrs) = text.split_once(';').unwrap_or((text, ""));
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let mut domain = fallback.clone();
            for attr in attrs.split(';') {
                let attr = attr.trim();
                let lower = attr.to_ascii_lowercase();
                if let Some(rest) = lower.strip_prefix("domain=") {
                    domain = rest.trim().trim_start_matches('.').to_string();
                }
            }
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if let Some(existing) = self.cookies.iter_mut().find(|c| c.name == name && c.domain == domain) {
                existing.value = value;
            } else {
                self.cookies.push(StoredCookie { name, value, domain });
            }
        }
    }

    pub fn header_for(&self, url: &Url) -> Option<String> {
        let host = url.host_str().unwrap_or_default();
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|cookie| host_matches(host, &cookie.domain))
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies.iter().rev().find(|c| c.name == name).map(|c| c.value.as_str())
    }
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

    #[test]
    fn cookie_is_sent_only_to_its_domain() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("X-APPLE-WEBAUTH-TOKEN=abc; Domain=icloud.com; Path=/"),
        );
        let mut jar = CookieJar::default();
        jar.store_headers(&Url::parse("https://setup.icloud.com/setup").unwrap(), &headers);
        let setup = Url::parse("https://setup.icloud.com/setup/ws/1/validate").unwrap();
        let other = Url::parse("https://example.com/").unwrap();
        assert!(jar.header_for(&setup).unwrap().contains("X-APPLE-WEBAUTH-TOKEN=abc"));
        assert!(jar.header_for(&other).is_none());
    }
}
