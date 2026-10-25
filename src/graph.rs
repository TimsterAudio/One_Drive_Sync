//! [`RemoteDrive`] over Microsoft Graph, for a personal OneDrive.

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::json;
use ureq::http::Response;
use ureq::Body;

use crate::remote::{
    Delta, Precondition, RemoteChange, RemoteDrive, RemoteError, RemoteItem, RemoteResult,
    SIMPLE_UPLOAD_LIMIT,
};

const GRAPH: &str = "https://graph.microsoft.com/v1.0/";

/// How much of OneDrive the app can reach, which also sets where pair
/// folders are found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveScope {
    /// Only `Apps/<app name>/` (`Files.ReadWrite.AppFolder`); pair folders
    /// are inside it.
    #[default]
    AppFolder,
    /// The whole drive (`Files.ReadWrite`); pair folders are from its top.
    FullDrive,
}

impl DriveScope {
    /// The Graph scopes to ask for, `offline_access` included.
    pub fn scopes(self) -> &'static str {
        match self {
            DriveScope::AppFolder => "Files.ReadWrite.AppFolder offline_access",
            DriveScope::FullDrive => "Files.ReadWrite offline_access",
        }
    }

    fn base(self) -> &'static str {
        match self {
            DriveScope::AppFolder => "me/drive/special/approot",
            DriveScope::FullDrive => "me/drive/root",
        }
    }
}

/// Supplies access tokens; [`Auth`](crate::auth::Auth) is the real one.
pub trait TokenSource: Send + Sync {
    fn access_token(&self) -> RemoteResult<String>;
    /// The last token was refused; get a new one next time.
    fn invalidate(&self);
}

/// Characters left as they are in a path segment.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct DriveItem {
    id: String,
    name: Option<String>,
    e_tag: Option<String>,
    size: Option<u64>,
    last_modified_date_time: Option<String>,
    file_system_info: Option<FileSystemInfo>,
    parent_reference: Option<ParentReference>,
    file: Option<FileFacet>,
    folder: Option<serde_json::Value>,
    deleted: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct FileSystemInfo {
    last_modified_date_time: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ParentReference {
    id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileFacet {
    hashes: Option<Hashes>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Hashes {
    quick_xor_hash: Option<String>,
}

#[derive(Deserialize)]
struct DeltaPage {
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta: Option<String>,
}

fn parse_ms(s: &str) -> i64 {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        .unwrap_or(0)
}

impl DriveItem {
    fn into_item(self) -> RemoteItem {
        let modified = self
            .file_system_info
            .and_then(|f| f.last_modified_date_time)
            .or(self.last_modified_date_time);
        RemoteItem {
            id: self.id,
            parent_id: self.parent_reference.and_then(|p| p.id),
            name: self.name.unwrap_or_default(),
            is_folder: self.folder.is_some(),
            etag: self.e_tag.unwrap_or_default(),
            size: self.size.unwrap_or(0),
            mtime_ms: modified.as_deref().map(parse_ms).unwrap_or(0),
            hash: self
                .file
                .and_then(|f| f.hashes)
                .and_then(|h| h.quick_xor_hash),
        }
    }
}

/// A personal OneDrive reached over Microsoft Graph.
pub struct GraphDrive {
    agent: ureq::Agent,
    tokens: Arc<dyn TokenSource>,
    scope: DriveScope,
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent("one_drive_sync")
        .build()
        .new_agent()
}

pub(crate) fn transport_error(e: ureq::Error) -> RemoteError {
    use ureq::Error as E;
    match e {
        E::Io(_)
        | E::Timeout(_)
        | E::HostNotFound
        | E::ConnectionFailed
        | E::ConnectProxyFailed(_) => RemoteError::Offline(e.to_string()),
        e => RemoteError::Other(e.to_string()),
    }
}

/// Turns an HTTP status into the matching error, if it's one.
fn check(mut resp: Response<Body>) -> RemoteResult<Response<Body>> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let message = resp
        .body_mut()
        .read_to_string()
        .ok()
        .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_default();
    Err(match status {
        401 => RemoteError::Auth(format!("OneDrive refused the sign-in {message}")),
        404 => RemoteError::NotFound,
        409 | 412 => RemoteError::Conflict,
        410 => RemoteError::ResyncRequired,
        413 => RemoteError::TooLarge(0),
        429 | 503 | 509 => RemoteError::Throttled(Duration::from_secs(retry_after.unwrap_or(30))),
        _ => RemoteError::Other(format!("OneDrive answered {status} {message}")),
    })
}

enum Payload<'a> {
    None,
    Bytes(&'a [u8]),
    Json(serde_json::Value),
}

impl GraphDrive {
    pub fn new(tokens: Arc<dyn TokenSource>, scope: DriveScope) -> Self {
        Self {
            agent: agent(),
            tokens,
            scope,
        }
    }

    fn item_url(&self, path: &str) -> String {
        let path = encode_path(path);
        if path.is_empty() {
            format!("{GRAPH}{}", self.scope.base())
        } else {
            format!("{GRAPH}{}:/{path}:", self.scope.base())
        }
    }

    /// Sends a request, getting a fresh token and trying once more if the
    /// token was refused.
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Payload,
    ) -> RemoteResult<Response<Body>> {
        let mut retried = false;
        loop {
            let token = self.tokens.access_token()?;
            let auth = format!("Bearer {token}");
            let result = match (method, &body) {
                ("GET", _) => {
                    let mut r = self.agent.get(url).header("Authorization", &auth);
                    for (k, v) in headers {
                        r = r.header(*k, *v);
                    }
                    r.call()
                }
                ("DELETE", _) => {
                    let mut r = self.agent.delete(url).header("Authorization", &auth);
                    for (k, v) in headers {
                        r = r.header(*k, *v);
                    }
                    r.call()
                }
                (m, b) => {
                    let mut r = if m == "PUT" {
                        self.agent.put(url)
                    } else {
                        self.agent.post(url)
                    }
                    .header("Authorization", &auth);
                    for (k, v) in headers {
                        r = r.header(*k, *v);
                    }
                    match b {
                        Payload::None => r.send_empty(),
                        Payload::Bytes(d) => r
                            .header("Content-Type", "application/octet-stream")
                            .send(*d),
                        Payload::Json(v) => r.send_json(v),
                    }
                }
            };
            let resp = result.map_err(transport_error)?;
            if resp.status().as_u16() == 401 && !retried {
                self.tokens.invalidate();
                retried = true;
                continue;
            }
            return check(resp);
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> RemoteResult<T> {
        let mut resp = self.send("GET", url, &[], Payload::None)?;
        resp.body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_json()
            .map_err(transport_error)
    }

    fn item_at(&self, path: &str) -> RemoteResult<RemoteItem> {
        Ok(self
            .get_json::<DriveItem>(&self.item_url(path))?
            .into_item())
    }

    /// The name of the account the drive belongs to.
    pub fn account_name(&self) -> RemoteResult<String> {
        let v: serde_json::Value = self.get_json(&format!("{GRAPH}me/drive"))?;
        Ok(v["owner"]["user"]["displayName"]
            .as_str()
            .unwrap_or("OneDrive")
            .to_string())
    }
}

impl RemoteDrive for GraphDrive {
    fn ensure_folder(&mut self, path: &str) -> RemoteResult<RemoteItem> {
        match self.item_at(path) {
            Err(RemoteError::NotFound) if !path.trim_matches('/').is_empty() => {}
            r => return r,
        }
        let path = path.trim_matches('/');
        let (parent, name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let parent = self.ensure_folder(parent)?;
        let url = format!("{GRAPH}me/drive/items/{}/children", parent.id);
        let body = json!({
            "name": name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "fail",
        });
        match self.send("POST", &url, &[], Payload::Json(body)) {
            Ok(mut resp) => Ok(resp
                .body_mut()
                .read_json::<DriveItem>()
                .map_err(transport_error)?
                .into_item()),
            // Made meanwhile by someone else.
            Err(RemoteError::Conflict) => self.item_at(path),
            Err(e) => Err(e),
        }
    }

    fn delta(&mut self, folder_id: &str, cursor: Option<&str>) -> RemoteResult<Delta> {
        let mut url = match cursor {
            Some(c) => c.to_string(),
            None => format!("{GRAPH}me/drive/items/{folder_id}/delta"),
        };
        let mut changes = Vec::new();
        loop {
            let page: DeltaPage = self.get_json(&url)?;
            for item in page.value {
                if item.deleted.is_some() {
                    changes.push(RemoteChange::Deleted { id: item.id });
                } else {
                    changes.push(RemoteChange::Upsert(item.into_item()));
                }
            }
            match (page.next, page.delta) {
                (Some(next), _) => url = next,
                (None, Some(delta)) => {
                    return Ok(Delta {
                        changes,
                        cursor: delta,
                        complete: false,
                    })
                }
                (None, None) => {
                    return Err(RemoteError::Other(
                        "OneDrive's change list ended without a link".into(),
                    ))
                }
            }
        }
    }

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
        let url = format!("{GRAPH}me/drive/items/{id}/content");
        let mut resp = self.send("GET", &url, &[], Payload::None)?;
        resp.body_mut()
            .with_config()
            .limit(1024 * 1024 * 1024)
            .read_to_vec()
            .map_err(transport_error)
    }

    fn upload(&mut self, path: &str, data: &[u8], cond: Precondition) -> RemoteResult<RemoteItem> {
        if data.len() as u64 > SIMPLE_UPLOAD_LIMIT {
            return Err(RemoteError::TooLarge(data.len() as u64));
        }
        let base = format!("{}/content", self.item_url(path));
        let (url, headers): (String, Vec<(&str, &str)>) = match &cond {
            Precondition::New => (
                format!("{base}?@microsoft.graph.conflictBehavior=fail"),
                vec![],
            ),
            Precondition::Match(tag) => (base, vec![("If-Match", tag.as_str())]),
            Precondition::Overwrite => (
                format!("{base}?@microsoft.graph.conflictBehavior=replace"),
                vec![],
            ),
        };
        let result = self.send("PUT", &url, &headers, Payload::Bytes(data));
        let mut resp = match result {
            // If-Match on a file that's gone.
            Err(RemoteError::NotFound) if matches!(cond, Precondition::Match(_)) => {
                return Err(RemoteError::Conflict)
            }
            r => r?,
        };
        Ok(resp
            .body_mut()
            .read_json::<DriveItem>()
            .map_err(transport_error)?
            .into_item())
    }

    fn delete(&mut self, id: &str, etag: Option<&str>) -> RemoteResult<()> {
        let url = format!("{GRAPH}me/drive/items/{id}");
        let headers: Vec<(&str, &str)> = etag.map(|t| ("If-Match", t)).into_iter().collect();
        self.send("DELETE", &url, &headers, Payload::None)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_encoded_by_segment() {
        assert_eq!(
            encode_path("/Trips/Lake District (2026).trip.json"),
            "Trips/Lake%20District%20%282026%29.trip.json"
        );
        assert_eq!(encode_path(""), "");
    }

    #[test]
    fn delta_items_parse() {
        let page: DeltaPage = serde_json::from_str(
            r#"{"value":[
                {"id":"A","name":"x.trip.json","eTag":"\"{A},3\"","size":12,
                 "lastModifiedDateTime":"2026-10-25T10:00:00Z",
                 "parentReference":{"id":"P"},
                 "file":{"hashes":{"quickXorHash":"abc="}}},
                {"id":"B","deleted":{"state":"deleted"}}
             ],
             "@odata.deltaLink":"https://graph/delta?token=1"}"#,
        )
        .unwrap();
        let mut it = page.value.into_iter();
        let a = it.next().unwrap().into_item();
        assert_eq!(a.parent_id.as_deref(), Some("P"));
        assert_eq!(a.hash.as_deref(), Some("abc="));
        assert_eq!(a.mtime_ms, 1_792_922_400_000);
        assert!(!a.is_folder);
        assert!(it.next().unwrap().deleted.is_some());
        assert_eq!(page.delta.as_deref(), Some("https://graph/delta?token=1"));
    }
}
