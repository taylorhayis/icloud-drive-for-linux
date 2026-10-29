use std::path::Path;
use std::sync::Mutex;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::cookies::{CookieJar, StoredCookie};
use crate::parse::{parse_container, parse_created, parse_download_url, parse_file_count, parse_folder, RemoteNode};
use crate::srp_auth::{finish_srp, start_srp};
use crate::{derive_password, ProtoError, Result, CONNECT_TIMEOUT, READ_TIMEOUT};

const CLIENT_ID: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStep {
    SignedIn,
    NeedsCode,
    NeedsTerms,
    NeedsDeviceApproval,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeChannel {
    TrustedDevice,
    Sms,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub apple_id: String,
    pub client_id: String,
    pub cookies: Vec<StoredCookie>,
    pub session_token: Option<String>,
    pub trust_token: Option<String>,
    pub scnt: Option<String>,
    pub session_id: Option<String>,
    pub account_country: Option<String>,
    pub setup_base: String,
    pub dsid: Option<String>,
    pub drivews: Option<String>,
    pub docws: Option<String>,
}

#[derive(Debug)]
struct AuthMemory {
    apple_id: Option<String>,
    client_id: String,
    session_token: Option<String>,
    trust_token: Option<String>,
    scnt: Option<String>,
    session_id: Option<String>,
    account_country: Option<String>,
    setup_base: String,
    dsid: Option<String>,
    drivews: Option<String>,
    docws: Option<String>,
    signed_in: bool,
    code_channel: CodeChannel,
    phone_id: u32,
    terms_body: Option<Value>,
}

pub struct IcloudClient {
    http: Client,
    jar: Mutex<CookieJar>,
    auth: Mutex<AuthMemory>,
}

#[derive(Debug, Clone)]
pub struct FolderListing {
    pub docwsid: Option<String>,
    /// Files in this folder and everything under it. On the drive root this is the library total.
    pub file_count: Option<u64>,
    pub children: Vec<RemoteNode>,
}

#[derive(Debug, Clone)]
pub struct CreatedFolder {
    pub drivewsid: Option<String>,
    pub docwsid: Option<String>,
    pub etag: String,
}

impl IcloudClient {
    pub fn new(china: bool) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        let setup_base = if china {
            "https://setup.icloud.com.cn/setup/ws/1".to_string()
        } else {
            "https://setup.icloud.com/setup/ws/1".to_string()
        };
        Ok(Self {
            http,
            jar: Mutex::new(CookieJar::default()),
            auth: Mutex::new(AuthMemory {
                apple_id: None,
                client_id: format!("auth-{}", uuid::Uuid::new_v4()),
                session_token: None,
                trust_token: None,
                scnt: None,
                session_id: None,
                account_country: None,
                setup_base,
                dsid: None,
                drivews: None,
                docws: None,
                signed_in: false,
                code_channel: CodeChannel::TrustedDevice,
                phone_id: 1,
                terms_body: None,
            }),
        })
    }

    pub fn from_saved(saved: SavedSession) -> Result<Self> {
        let china = saved.setup_base.contains("icloud.com.cn");
        let client = Self::new(china)?;
        *client.jar.lock().expect("cookie jar") = CookieJar::from_saved(saved.cookies);
        let mut auth = client.auth.lock().expect("auth");
        auth.apple_id = Some(saved.apple_id);
        auth.client_id = saved.client_id;
        auth.session_token = saved.session_token;
        auth.trust_token = saved.trust_token;
        auth.scnt = saved.scnt;
        auth.session_id = saved.session_id;
        auth.account_country = saved.account_country;
        auth.setup_base = saved.setup_base;
        auth.dsid = saved.dsid;
        auth.drivews = saved.drivews;
        auth.docws = saved.docws;
        auth.signed_in = auth.session_token.is_some() && auth.drivews.is_some();
        drop(auth);
        Ok(client)
    }

    pub fn export(&self) -> SavedSession {
        let auth = self.auth.lock().expect("auth");
        let jar = self.jar.lock().expect("cookie jar");
        SavedSession {
            apple_id: auth.apple_id.clone().unwrap_or_default(),
            client_id: auth.client_id.clone(),
            cookies: jar.saved(),
            session_token: auth.session_token.clone(),
            trust_token: auth.trust_token.clone(),
            scnt: auth.scnt.clone(),
            session_id: auth.session_id.clone(),
            account_country: auth.account_country.clone(),
            setup_base: auth.setup_base.clone(),
            dsid: auth.dsid.clone(),
            drivews: auth.drivews.clone(),
            docws: auth.docws.clone(),
        }
    }

    pub fn is_signed_in(&self) -> bool {
        self.auth.lock().expect("auth").signed_in
    }

    pub async fn validate(&self) -> Result<bool> {
        let url = format!("{}/validate", self.setup_base());
        let (status, _, _) = self.send("POST", &url, Some("null".into()), false).await?;
        Ok(status.is_success())
    }

    pub async fn sign_in(&self, apple_id: &str, password: &str) -> Result<AuthStep> {
        let apple_id = apple_id.trim().to_lowercase();
        {
            let mut auth = self.auth.lock().expect("auth");
            auth.apple_id = Some(apple_id.clone());
            auth.signed_in = false;
        }
        self.detect_partition().await;
        let secrets = start_srp();
        let init = json!({
            "a": STANDARD.encode(&secrets.a_pub),
            "accountName": apple_id,
            "protocols": ["s2k", "s2k_fo"],
        });
        let (status, _, body) = self.auth_post("/signin/init", init).await?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            tracing::warn!(status = status.as_u16(), "sign-in init was rejected");
            return Ok(AuthStep::Failed(format!(
                "Apple did not start sign-in. Check the Apple ID. {}",
                self.failure_text(status, &body)
            )));
        }
        if !status.is_success() {
            return Ok(AuthStep::Failed(format!(
                "Apple sign-in did not start ({})",
                status.as_u16()
            )));
        }
        let init: InitReply = serde_json::from_slice(&body)
            .map_err(|err| ProtoError::Message(format!("unexpected sign-in init reply: {err}")))?;
        let salt = STANDARD
            .decode(init.salt)
            .map_err(|err| ProtoError::Message(format!("salt: {err}")))?;
        let b_pub = STANDARD
            .decode(init.b)
            .map_err(|err| ProtoError::Message(format!("server key: {err}")))?;
        let derived = derive_password(password, &salt, init.iteration, &init.protocol);
        let proof = finish_srp(&apple_id, &derived, &secrets.a_priv, &salt, &b_pub)?;
        let complete = json!({
            "accountName": apple_id,
            "c": init.c,
            "m1": STANDARD.encode(proof.m1),
            "m2": STANDARD.encode(proof.m2),
            "rememberMe": true,
            "trustTokens": self.trust_tokens(),
        });
        let (status, _, body) = self
            .auth_post("/signin/complete?isRememberMeEnabled=true", complete)
            .await?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            tracing::warn!(
                status = status.as_u16(),
                protocol = %init.protocol,
                "sign-in proof was rejected"
            );
            let detail = self.failure_text(status, &body);
            let message = if detail.starts_with("iCloud returned") {
                "Apple did not accept that password. Use the Apple ID password. An app-specific password will not work here.".into()
            } else {
                format!("Apple did not accept that password. {detail}")
            };
            return Ok(AuthStep::Failed(message));
        }
        if status == StatusCode::CONFLICT {
            self.note_auth_body(&body);
            return Ok(AuthStep::NeedsCode);
        }
        if !status.is_success() {
            return Ok(AuthStep::Failed(self.failure_text(status, &body)));
        }
        self.finish_account_login().await
    }

    pub async fn submit_code(&self, code: &str) -> Result<AuthStep> {
        let channel = self.auth.lock().expect("auth").code_channel;
        let path = match channel {
            CodeChannel::TrustedDevice => "/verify/trusteddevice/securitycode".to_string(),
            CodeChannel::Sms => "/verify/phone/securitycode".to_string(),
        };
        let body = if channel == CodeChannel::Sms {
            let phone_id = self.auth.lock().expect("auth").phone_id;
            json!({
                "securityCode": { "code": code },
                "phoneNumber": { "id": phone_id },
                "mode": "sms",
            })
        } else {
            json!({ "securityCode": { "code": code } })
        };
        let (status, _, resp) = self.auth_post(&path, body).await?;
        if status == StatusCode::UNAUTHORIZED || status.as_u16() == 400 {
            return Ok(AuthStep::Failed("That code was not accepted".into()));
        }
        if !status.is_success() {
            return Ok(AuthStep::Failed(self.failure_text(status, &resp)));
        }
        let _ = self.auth_get("/2sv/trust").await;
        self.finish_account_login().await
    }

    pub async fn request_sms(&self) -> Result<AuthStep> {
        let phone_id = self.auth.lock().expect("auth").phone_id;
        let body = json!({ "phoneNumber": { "id": phone_id }, "mode": "sms" });
        let (status, _, resp) = self.auth_put("/verify/phone", body).await?;
        if status.is_success() {
            self.auth.lock().expect("auth").code_channel = CodeChannel::Sms;
            return Ok(AuthStep::NeedsCode);
        }
        Ok(AuthStep::Failed(self.failure_text(status, &resp)))
    }

    pub async fn accept_terms(&self) -> Result<AuthStep> {
        self.finish_account_login().await
    }

    /// Ask a trusted Apple device to release Drive keys. Used when Advanced Data Protection is on.
    pub async fn request_device_approval(&self) -> Result<AuthStep> {
        let setup = self.setup_base();
        let _ = self
            .send("POST", &format!("{setup}/enableDeviceConsentForPCS"), Some(json!({}).to_string()), false)
            .await;
        let (status, _, body) = self
            .send(
                "POST",
                &format!("{setup}/requestPCS"),
                Some(json!({ "services": ["iclouddrive"] }).to_string()),
                false,
            )
            .await?;
        if status.is_success() {
            self.auth.lock().expect("auth").signed_in = self.drive_roots().is_ok();
            return Ok(AuthStep::SignedIn);
        }
        let text = String::from_utf8_lossy(&body);
        if text.to_ascii_lowercase().contains("consent") || status == StatusCode::CONFLICT {
            return Ok(AuthStep::NeedsDeviceApproval);
        }
        Ok(AuthStep::Failed(self.failure_text(status, &body)))
    }

    pub async fn list_folder(&self, drivewsid: &str) -> Result<FolderListing> {
        let root = self.drive_root("drivews")?;
        let url = format!("{root}/retrieveItemDetailsInFolders");
        let body = json!([{ "drivewsid": drivewsid, "partialData": false }]);
        let (status, _, bytes) = self.send("POST", &url, Some(body.to_string()), false).await?;
        if status == StatusCode::LOCKED {
            return Err(ProtoError::Api {
                status: status.as_u16(),
                message: "Advanced Data Protection approval is required".into(),
            });
        }
        self.ensure_ok(status, &bytes)?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|err| ProtoError::Message(format!("Drive listing was not JSON: {err}")))?;
        Ok(FolderListing {
            docwsid: parse_container(&value).map(|(_, doc)| doc),
            file_count: parse_file_count(&value),
            children: parse_folder(&value).map_err(ProtoError::Message)?,
        })
    }

    pub async fn download_to(
        &self,
        docwsid: &str,
        dest: &Path,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        if cancel.is_cancelled() {
            return Err(ProtoError::Cancelled);
        }
        let doc = self.drive_root("docws")?;
        let mut url = format!("{doc}/ws/com.apple.CloudDocs/download/by_id");
        url.push_str(&self.query(&[("document_id", docwsid)]));
        let (status, _, bytes) = self.send("GET", &url, None, false).await?;
        self.ensure_ok(status, &bytes)?;
        let meta: Value = serde_json::from_slice(&bytes)
            .map_err(|err| ProtoError::Message(format!("download ticket was not JSON: {err}")))?;
        let file_url = parse_download_url(&meta).map_err(ProtoError::Message)?;
        let response = self.http_get(&file_url).await?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.unwrap_or_default();
            return self.ensure_ok(status, &bytes).map(|_| 0);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|err| ProtoError::Message(err.to_string()))?;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                return Err(ProtoError::Cancelled);
            }
            let chunk = chunk?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|err| ProtoError::Message(err.to_string()))?;
            written += chunk.len() as u64;
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|err| ProtoError::Message(err.to_string()))?;
        file.sync_all()
            .await
            .map_err(|err| ProtoError::Message(err.to_string()))?;
        Ok(written)
    }

    pub async fn upload_new(
        &self,
        folder_docwsid: &str,
        filename: &str,
        src: &Path,
        existing_docwsid: Option<&str>,
    ) -> Result<()> {
        let size = tokio::fs::metadata(src)
            .await
            .map_err(|err| ProtoError::Message(err.to_string()))?
            .len();
        let doc = self.drive_root("docws")?;
        let token = self.webauth_token();
        let mut url = format!("{doc}/ws/com.apple.CloudDocs/upload/web");
        url.push_str(&self.query(&[("token", token.as_deref().unwrap_or(""))]));
        let ticket = json!({
            "filename": filename,
            "type": "FILE",
            "content_type": content_type_for(filename),
            "size": size,
        });
        // CloudDocs rejects application/json with "Unexpected input".
        let (status, _, bytes) = self.post_plain(&url, ticket.to_string()).await?;
        self.ensure_ok(status, &bytes).map_err(|err| annotate("reserving the upload", err))?;
        let tickets: Value = serde_json::from_slice(&bytes)
            .map_err(|err| ProtoError::Message(format!("upload ticket was not JSON: {err}")))?;
        let first = tickets
            .as_array()
            .and_then(|items| items.first())
            .ok_or_else(|| ProtoError::Message("upload ticket was empty".into()))?;
        let document_id = first
            .get("document_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProtoError::Message("upload ticket had no document id".into()))?
            .to_string();
        let content_url = first
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProtoError::Message("upload ticket had no URL".into()))?
            .to_string();
        let file = tokio::fs::File::open(src)
            .await
            .map_err(|err| ProtoError::Message(err.to_string()))?;
        let part = reqwest::multipart::Part::stream_with_length(file, size).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new().part(filename.to_string(), part);
        let cookie = self
            .jar
            .lock()
            .expect("jar")
            .header_for(&Url::parse(&content_url).unwrap_or_else(|_| Url::parse("https://icloud.com").unwrap()))
            .unwrap_or_default();
        let response = self
            .http
            .post(&content_url)
            .header(reqwest::header::COOKIE, cookie)
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        self.remember_headers(status, &content_url, &HeaderMap::new());
        self.ensure_ok(status, &bytes).map_err(|err| annotate("sending the file", err))?;
        let uploaded: Value = serde_json::from_slice(&bytes)
            .map_err(|err| ProtoError::Message(format!("upload response was not JSON: {err}")))?;
        let single = uploaded
            .get("singleFile")
            .cloned()
            .ok_or_else(|| ProtoError::Message("upload response had no file receipt".into()))?;
        let signature = single.get("fileChecksum").cloned().unwrap_or(Value::Null);
        let wrapping_key = single.get("wrappingKey").cloned().unwrap_or(Value::Null);
        let reference_signature = single.get("referenceChecksum").cloned().unwrap_or(Value::Null);
        let file_size = single.get("size").cloned().unwrap_or(json!(size));
        let receipt = single.get("receipt").cloned();
        let now = chrono::Utc::now().timestamp_millis();
        let mut data = json!({
            "signature": signature,
            "wrapping_key": wrapping_key,
            "reference_signature": reference_signature,
            "size": file_size,
        });
        if let Some(receipt) = receipt {
            data["receipt"] = receipt;
        }
        // A new document id is only for a file that does not exist yet. Replacing one
        // reuses its document id, and allow_conflict must stay off or iCloud saves a copy.
        let (command_id, allow_conflict, short_guid) = if let Some(existing) = existing_docwsid.filter(|id| !id.is_empty()) {
            (existing.to_string(), false, false)
        } else {
            (document_id, true, true)
        };
        let update = json!({
            "data": data,
            "command": "add_file",
            "create_short_guid": short_guid,
            "document_id": command_id,
            "path": { "starting_document_id": folder_docwsid, "path": filename },
            "allow_conflict": allow_conflict,
            "file_flags": { "is_writable": true, "is_executable": false, "is_hidden": false },
            "mtime": now,
            "btime": now,
        });
        let update_url = format!("{doc}/ws/com.apple.CloudDocs/update/documents");
        let (status, _, bytes) = self.post_plain(&update_url, update.to_string()).await?;
        self.ensure_ok(status, &bytes).map_err(|err| annotate("saving the file on iCloud", err))?;
        Ok(())
    }

    pub async fn rename_item(&self, drivewsid: &str, etag: &str, name: &str) -> Result<()> {
        let root = self.drive_root("drivews")?;
        let body = json!({
            "items": [{ "drivewsid": drivewsid, "etag": etag, "name": name }]
        });
        let (status, _, bytes) = self
            .send("POST", &format!("{root}/renameItems"), Some(body.to_string()), false)
            .await?;
        self.ensure_ok(status, &bytes)
    }

    pub async fn trash_item(&self, drivewsid: &str, etag: &str) -> Result<()> {
        let root = self.drive_root("drivews")?;
        let client_id = self.auth.lock().expect("auth").client_id.clone();
        let body = json!({
            "items": [{ "drivewsid": drivewsid, "etag": etag, "clientId": client_id }]
        });
        let (status, _, bytes) = self
            .send("POST", &format!("{root}/moveItemsToTrash"), Some(body.to_string()), false)
            .await?;
        self.ensure_ok(status, &bytes)
    }

    pub async fn create_folder(&self, parent_drivewsid: &str, name: &str) -> Result<CreatedFolder> {
        let root = self.drive_root("drivews")?;
        let client_id = self.auth.lock().expect("auth").client_id.clone();
        let body = json!({
            "destinationDrivewsId": parent_drivewsid,
            "folders": [{ "clientId": client_id, "name": name }]
        });
        let (status, _, bytes) = self
            .post_plain(&format!("{root}/createFolders"), body.to_string())
            .await?;
        self.ensure_ok(status, &bytes)?;
        let (drivewsid, docwsid, etag) = parse_created(&bytes);
        Ok(CreatedFolder { drivewsid, docwsid, etag })
    }

    pub async fn move_item(&self, drivewsid: &str, etag: &str, destination: &str) -> Result<()> {
        let root = self.drive_root("drivews")?;
        let client_id = self.auth.lock().expect("auth").client_id.clone();
        let body = json!({
            "destinationDrivewsId": destination,
            "items": [{ "drivewsid": drivewsid, "etag": etag, "clientId": client_id }]
        });
        let (status, _, bytes) = self
            .send("POST", &format!("{root}/moveItems"), Some(body.to_string()), false)
            .await?;
        self.ensure_ok(status, &bytes)
    }

    async fn finish_account_login(&self) -> Result<AuthStep> {
        let (token, trust, country, setup) = {
            let auth = self.auth.lock().expect("auth");
            (
                auth.session_token.clone().unwrap_or_default(),
                auth.trust_token.clone().unwrap_or_default(),
                auth.account_country.clone().unwrap_or_default(),
                auth.setup_base.clone(),
            )
        };
        if token.is_empty() {
            return Ok(AuthStep::Failed(
                "Apple did not return a session. Try the code again.".into(),
            ));
        }
        let mut body = json!({
            "accountCountryCode": country,
            "dsWebAuthToken": token,
            "extended_login": true,
            "trustToken": trust,
        });
        let (status, _, bytes) = self
            .send("POST", &format!("{setup}/accountLogin"), Some(body.to_string()), false)
            .await?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes).to_lowercase();
            if text.contains("terms") {
                self.auth.lock().expect("auth").terms_body = serde_json::from_slice(&bytes).ok();
                return Ok(AuthStep::NeedsTerms);
            }
            return Ok(AuthStep::Failed(self.failure_text(status, &bytes)));
        }
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if value.get("termsUpdateNeeded").and_then(|v| v.as_bool()) == Some(true)
            || value_mentions_terms(&value)
        {
            body["dsWebAuthToken"] = json!(token);
            let accepted = json!({
                "accountCountryCode": country,
                "dsWebAuthToken": token,
                "extended_login": true,
                "trustToken": trust,
                "termsAccepted": true,
            });
            let (status, _, bytes) = self
                .send(
                    "POST",
                    &format!("{setup}/accountLogin"),
                    Some(accepted.to_string()),
                    false,
                )
                .await?;
            if !status.is_success() && self.auth.lock().expect("auth").terms_body.is_some() {
                return Ok(AuthStep::NeedsTerms);
            }
            if status.is_success() {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    self.apply_account(&value);
                }
            }
        } else {
            self.apply_account(&value);
        }
        let auth = self.auth.lock().expect("auth");
        if value_needs_2fa(&value) {
            return Ok(AuthStep::NeedsCode);
        }
        if auth.drivews.is_some() {
            return Ok(AuthStep::SignedIn);
        }
        Ok(AuthStep::Failed(
            "Signed in, but iCloud Drive is not available on this account.".into(),
        ))
    }

    fn apply_account(&self, value: &Value) {
        let mut auth = self.auth.lock().expect("auth");
        if let Some(dsid) = value
            .pointer("/dsInfo/dsid")
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())))
        {
            auth.dsid = Some(dsid);
        }
        if let Some(url) = value.pointer("/webservices/drivews/url").and_then(|v| v.as_str()) {
            auth.drivews = Some(url.trim_end_matches('/').to_string());
        }
        if let Some(url) = value.pointer("/webservices/docws/url").and_then(|v| v.as_str()) {
            auth.docws = Some(url.trim_end_matches('/').to_string());
        }
        auth.signed_in = auth.drivews.is_some();
    }

    async fn detect_partition(&self) {
        let url = format!("{}/validate", self.setup_base());
        let Ok((status, headers, _)) = self.send("POST", &url, Some("null".into()), false).await else {
            return;
        };
        if status == StatusCode::MISDIRECTED_REQUEST || status.as_u16() == 421 {
            if let Some(host) = headers
                .get("x-apple-mme-host")
                .and_then(|v| v.to_str().ok())
            {
                let mut auth = self.auth.lock().expect("auth");
                auth.setup_base = format!("https://{host}/setup/ws/1");
            }
        }
    }

    fn note_auth_body(&self, body: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        if let Some(id) = value
            .pointer("/phoneNumberVerification/trustedPhoneNumbers/0/id")
            .and_then(|v| v.as_u64())
        {
            self.auth.lock().expect("auth").phone_id = id as u32;
        }
    }

    async fn auth_post(&self, path: &str, body: Value) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let url = self.auth_url(path);
        self.send("POST", &url, Some(body.to_string()), true).await
    }

    async fn auth_put(&self, path: &str, body: Value) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let url = self.auth_url(path);
        self.send("PUT", &url, Some(body.to_string()), true).await
    }

    async fn auth_get(&self, path: &str) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let url = self.auth_url(path);
        self.send("GET", &url, None, true).await
    }

    fn auth_url(&self, path: &str) -> String {
        let base = if self.setup_base().contains(".cn") {
            "https://idmsa.apple.com.cn/appleauth/auth"
        } else {
            "https://idmsa.apple.com/appleauth/auth"
        };
        format!("{base}{path}")
    }

    async fn post_plain(&self, url: &str, body: String) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        self.dispatch("POST", url, Some(body), false, "text/plain").await
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<String>,
        idmsa: bool,
    ) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let content_type = if body.is_some() { "application/json" } else { "" };
        self.dispatch(method, url, body, idmsa, content_type).await
    }

    async fn dispatch(
        &self,
        method: &str,
        url: &str,
        body: Option<String>,
        idmsa: bool,
        content_type: &str,
    ) -> Result<(StatusCode, HeaderMap, Vec<u8>)> {
        let parsed = Url::parse(url).map_err(|err| ProtoError::Message(err.to_string()))?;
        let mut request = self.http.request(method.parse().expect("method"), url);
        if let Some(cookie) = self.jar.lock().expect("jar").header_for(&parsed) {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        request = request.header("Origin", "https://www.icloud.com");
        request = request.header("Referer", "https://www.icloud.com/");
        if idmsa {
            for (name, value) in self.idmsa_headers() {
                request = request.header(name, value);
            }
        }
        if let Some(body) = body {
            if !content_type.is_empty() {
                request = request.header(CONTENT_TYPE, content_type);
            }
            request = request.body(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        self.jar.lock().expect("jar").store_headers(&parsed, &headers);
        self.capture_auth_headers(&headers);
        if let Some(host) = headers.get("x-apple-mme-host").and_then(|v| v.to_str().ok()) {
            if status.as_u16() == 421 {
                self.auth.lock().expect("auth").setup_base = format!("https://{host}/setup/ws/1");
            }
        }
        let bytes = response.bytes().await?.to_vec();
        tracing::debug!(%method, %url, status = status.as_u16(), "icloud");
        Ok((status, headers, bytes))
    }

    fn remember_headers(&self, _status: StatusCode, url: &str, headers: &HeaderMap) {
        if let Ok(parsed) = Url::parse(url) {
            self.jar.lock().expect("jar").store_headers(&parsed, headers);
        }
    }

    async fn http_get(&self, url: &str) -> Result<reqwest::Response> {
        let parsed = Url::parse(url).map_err(|err| ProtoError::Message(err.to_string()))?;
        let mut request = self.http.get(url);
        if let Some(cookie) = self.jar.lock().expect("jar").header_for(&parsed) {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        Ok(request.send().await?)
    }

    fn idmsa_headers(&self) -> Vec<(String, String)> {
        let auth = self.auth.lock().expect("auth");
        let mut headers = vec![
            ("Accept".into(), "application/json".into()),
            ("X-Apple-OAuth-Client-Id".into(), CLIENT_ID.into()),
            ("X-Apple-OAuth-Client-Type".into(), "firstPartyAuth".into()),
            ("X-Apple-OAuth-Redirect-URI".into(), "https://www.icloud.com".into()),
            ("X-Apple-OAuth-Require-Grant-Code".into(), "true".into()),
            ("X-Apple-OAuth-Response-Mode".into(), "web_message".into()),
            ("X-Apple-OAuth-Response-Type".into(), "code".into()),
            ("X-Apple-OAuth-State".into(), auth.client_id.clone()),
            ("X-Apple-Widget-Key".into(), CLIENT_ID.into()),
            ("X-Apple-Frame-Id".into(), auth.client_id.clone()),
        ];
        if let Some(scnt) = &auth.scnt {
            headers.push(("scnt".into(), scnt.clone()));
        }
        if let Some(session_id) = &auth.session_id {
            headers.push(("X-Apple-ID-Session-Id".into(), session_id.clone()));
        }
        headers
    }

    fn capture_auth_headers(&self, headers: &HeaderMap) {
        let mut auth = self.auth.lock().expect("auth");
        capture(&mut auth.session_id, headers, "x-apple-id-session-id");
        capture(&mut auth.session_token, headers, "x-apple-session-token");
        capture(&mut auth.trust_token, headers, "x-apple-twosv-trust-token");
        capture(&mut auth.scnt, headers, "scnt");
        capture(&mut auth.account_country, headers, "x-apple-id-account-country");
    }

    fn setup_base(&self) -> String {
        self.auth.lock().expect("auth").setup_base.clone()
    }

    fn trust_tokens(&self) -> Vec<String> {
        self.auth
            .lock()
            .expect("auth")
            .trust_token
            .clone()
            .into_iter()
            .collect()
    }

    fn drive_roots(&self) -> std::result::Result<(String, String), ()> {
        let auth = self.auth.lock().expect("auth");
        Ok((auth.drivews.clone().ok_or(())?, auth.docws.clone().ok_or(())?))
    }

    fn drive_root(&self, which: &str) -> Result<String> {
        let (drive, doc) = self.drive_roots().map_err(|_| {
            ProtoError::Api {
                status: 401,
                message: "Sign in again".into(),
            }
        })?;
        Ok(if which == "docws" { doc } else { drive })
    }

    fn query(&self, pairs: &[(&str, &str)]) -> String {
        let dsid = self.auth.lock().expect("auth").dsid.clone().unwrap_or_default();
        let mut parts: Vec<String> = pairs
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| format!("{key}={}", urlencoding(value)))
            .collect();
        if !dsid.is_empty() {
            parts.push(format!("dsid={dsid}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }

    fn webauth_token(&self) -> Option<String> {
        let jar = self.jar.lock().expect("jar");
        let value = jar.get("X-APPLE-WEBAUTH-VALIDATE")?;
        value.split(['&', ':']).find_map(|part| part.strip_prefix("t=")).map(str::to_string).or_else(|| {
            value.split("t=").nth(1).map(|rest| rest.split([':', '&']).next().unwrap_or(rest).to_string())
        })
    }

    fn ensure_ok(&self, status: StatusCode, body: &[u8]) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        if status.as_u16() == 423 || status == StatusCode::LOCKED {
            return Err(ProtoError::Api {
                status: status.as_u16(),
                message: "Advanced Data Protection approval is required".into(),
            });
        }
        Err(ProtoError::Api {
            status: status.as_u16(),
            message: self.failure_text(status, body),
        })
    }

    fn failure_text(&self, status: StatusCode, body: &[u8]) -> String {
        let text = String::from_utf8_lossy(body);
        if let Ok(value) = serde_json::from_slice::<Value>(body) {
            if let Some(reason) = value
                .get("errorMessage")
                .or_else(|| value.get("serviceErrors"))
                .or_else(|| value.get("errorReason"))
                .and_then(|v| v.as_str())
            {
                return reason.to_string();
            }
        }
        if text.trim().is_empty() {
            format!("iCloud returned {}", status.as_u16())
        } else {
            format!("iCloud returned {}: {}", status.as_u16(), text.chars().take(240).collect::<String>())
        }
    }
}

#[derive(Deserialize)]
struct InitReply {
    iteration: u32,
    salt: String,
    protocol: String,
    b: String,
    c: String,
}

fn capture(slot: &mut Option<String>, headers: &HeaderMap, name: &str) {
    if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
        if !value.is_empty() {
            *slot = Some(value.to_string());
        }
    }
}

fn value_needs_2fa(value: &Value) -> bool {
    let version = value.pointer("/dsInfo/hsaVersion").and_then(|v| v.as_i64()).unwrap_or(0);
    let challenge = value.get("hsaChallengeRequired").and_then(|v| v.as_bool()).unwrap_or(false);
    let trusted = value.get("hsaTrustedBrowser").and_then(|v| v.as_bool()).unwrap_or(false);
    version >= 2 && (challenge || !trusted)
}

fn value_mentions_terms(value: &Value) -> bool {
    value
        .to_string()
        .to_ascii_lowercase()
        .contains("terms of service")
}

fn content_type_for(filename: &str) -> &'static str {
    match filename.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).as_deref() {
        Some("txt" | "md") => "text/plain",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("mp4") => "video/mp4",
        Some("zip") => "application/zip",
        _ => "",
    }
}

fn annotate(step: &str, err: ProtoError) -> ProtoError {
    match err {
        ProtoError::Api { status, message } => ProtoError::Api { status, message: format!("{step}: {message}") },
        ProtoError::Message(message) => ProtoError::Message(format!("{step}: {message}")),
        other => other,
    }
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

