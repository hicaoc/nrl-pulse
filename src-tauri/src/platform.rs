use std::net::IpAddr;
use std::str::FromStr;

use reqwest::{multipart, Client};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

const PLATFORM_SERVERS_URL: &str = "https://nrlptt.com/api/platform-servers";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformServer {
    pub id: Option<i32>,
    pub name: String,
    pub host: String,
    pub port: String,
    pub online: i32,
    pub total: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUser {
    pub id: i32,
    pub name: String,
    pub callsign: String,
    pub nickname: Option<String>,
    pub avatar: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformGroup {
    pub id: i32,
    pub name: String,
    #[serde(rename(deserialize = "type", serialize = "groupType"))]
    pub group_type: i32,
    #[serde(
        rename(deserialize = "online_dev_number", serialize = "onlineDevNumber"),
        alias = "onlineDevNumber"
    )]
    pub online_dev_number: i32,
    #[serde(
        rename(deserialize = "total_dev_number", serialize = "totalDevNumber"),
        alias = "totalDevNumber"
    )]
    pub total_dev_number: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformDevice {
    pub id: i32,
    pub name: String,
    pub callsign: String,
    pub ssid: u8,
    #[serde(
        rename(deserialize = "group_id", serialize = "groupId"),
        alias = "groupId"
    )]
    pub group_id: i32,
    #[serde(
        rename(deserialize = "dev_model", serialize = "devModel"),
        alias = "devModel"
    )]
    pub dev_model: Option<u8>,
    pub qth: Option<String>,
    pub note: Option<String>,
    #[serde(
        rename(deserialize = "is_online", serialize = "isOnline"),
        alias = "isOnline",
        default
    )]
    pub is_online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginBootstrap {
    pub api_base: String,
    pub token: String,
    pub user: PlatformUser,
    pub groups: Vec<PlatformGroup>,
    pub current_group_id: i32,
    pub devices: Vec<PlatformDevice>,
    pub server: PlatformServer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupSnapshot {
    pub groups: Vec<PlatformGroup>,
    pub current_group_id: i32,
    pub devices: Vec<PlatformDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformRegisterPayload {
    pub callsign: String,
    pub name: String,
    pub phone: String,
    pub password: String,
    pub address: String,
    pub mail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformRegisterResult {
    pub code: i32,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginEnvelope {
    code: i32,
    data: Option<LoginData>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginData {
    token: String,
}

#[derive(Debug, Deserialize)]
struct DataEnvelope<T> {
    code: i32,
    data: T,
}

#[derive(Debug, Deserialize)]
struct ItemsEnvelope<T> {
    code: i32,
    data: ItemsData<T>,
}

#[derive(Debug, Deserialize)]
struct ItemsData<T> {
    items: T,
}

pub async fn fetch_platform_servers() -> Result<Vec<PlatformServer>, String> {
    let client = http_client()?;
    let response = client
        .get(PLATFORM_SERVERS_URL)
        .send()
        .await
        .map_err(|err| format!("fetch platform servers failed: {err}"))?;
    let body = response
        .text()
        .await
        .map_err(|err| format!("read platform server list failed: {err}"))?;
    decode_platform_servers(&body)
}

pub async fn login(
    server: PlatformServer,
    username: String,
    password: String,
) -> Result<LoginBootstrap, String> {
    let client = http_client()?;
    let candidates = base_candidates(&server.host);
    let login_body = json!({
        "username": username,
        "password": password,
    });

    let (api_base, login_envelope): (String, LoginEnvelope) =
        post_json_candidates(&client, &candidates, "/user/login", None, &login_body).await?;
    if login_envelope.code != 20000 {
        return Err(login_envelope
            .message
            .unwrap_or_else(|| "login failed".into()));
    }
    let token = login_envelope
        .data
        .ok_or_else(|| "login response missing token".to_string())?
        .token;

    restore_session_with_client(&client, api_base, token, server, 0).await
}

pub async fn restore_session(
    api_base: String,
    token: String,
    server: PlatformServer,
    current_group_id: i32,
) -> Result<LoginBootstrap, String> {
    let client = http_client()?;
    restore_session_with_client(&client, api_base, token, server, current_group_id).await
}

pub async fn register(
    host: String,
    payload: PlatformRegisterPayload,
    license_filename: String,
    license_bytes: Vec<u8>,
) -> Result<PlatformRegisterResult, String> {
    let client = http_client()?;
    let candidates = base_candidates(&host);
    let (_api_base, value): (String, Value) = post_multipart_candidates(
        &client,
        &candidates,
        "/user/reg/create",
        &payload,
        &license_filename,
        &license_bytes,
    )
    .await?;
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_default() as i32;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(PlatformRegisterResult { code, message })
}

pub async fn fetch_groups(
    api_base: String,
    token: String,
    current_group_id: i32,
) -> Result<GroupSnapshot, String> {
    let client = http_client()?;
    let groups = fetch_group_list_with_client(&client, &api_base, &token).await?;
    let target_group_id = resolve_group_id(&groups, current_group_id);
    let devices = if groups.iter().any(|group| group.id == target_group_id) {
        fetch_group_devices_with_client(&client, &api_base, &token, target_group_id).await?
    } else {
        Vec::new()
    };
    Ok(GroupSnapshot {
        groups,
        current_group_id: target_group_id,
        devices,
    })
}

pub async fn fetch_group_devices(
    api_base: String,
    token: String,
    group_id: i32,
) -> Result<Vec<PlatformDevice>, String> {
    let client = http_client()?;
    fetch_group_devices_with_client(&client, &api_base, &token, group_id).await
}

pub async fn switch_group(
    api_base: String,
    token: String,
    callsign: String,
    ssid: u8,
    group_id: i32,
) -> Result<GroupSnapshot, String> {
    let client = http_client()?;
    let body = json!({
        "callsign": callsign,
        "ssid": ssid,
        "group_id": group_id,
    });
    let response: Value = post_json_exact(
        &client,
        &api_base,
        "/device/changegroupnrl",
        Some(&token),
        &body,
    )
    .await?;
    let code = response
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    if code != 20000 {
        return Err(response
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("switch group failed")
            .to_string());
    }
    let message = response
        .get("data")
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !message.contains("成功") {
        return Err(if message.is_empty() {
            format!("switch group failed: {response}")
        } else {
            message.to_string()
        });
    }
    fetch_groups(api_base, token, group_id).await
}

async fn fetch_group_devices_with_client(
    client: &Client,
    api_base: &str,
    token: &str,
    group_id: i32,
) -> Result<Vec<PlatformDevice>, String> {
    post_items(
        client,
        api_base,
        "/group/device/list",
        Some(token),
        &json!({ "group_id": group_id.to_string() }),
    )
    .await
}

async fn restore_session_with_client(
    client: &Client,
    api_base: String,
    token: String,
    server: PlatformServer,
    current_group_id: i32,
) -> Result<LoginBootstrap, String> {
    let user: PlatformUser = get_data(client, &api_base, "/user/info", Some(&token)).await?;
    let groups = fetch_group_list_with_client(client, &api_base, &token).await?;
    let selected_group_id = resolve_group_id(&groups, current_group_id);
    let devices = if groups.iter().any(|group| group.id == selected_group_id) {
        fetch_group_devices_with_client(client, &api_base, &token, selected_group_id).await?
    } else {
        Vec::new()
    };
    Ok(LoginBootstrap {
        api_base,
        token,
        user,
        groups,
        current_group_id: selected_group_id,
        devices,
        server,
    })
}

async fn fetch_group_list_with_client(
    client: &Client,
    api_base: &str,
    token: &str,
) -> Result<Vec<PlatformGroup>, String> {
    let value: Value = post_json_exact(
        client,
        api_base,
        "/group/list/mini",
        Some(token),
        &json!({}),
    )
    .await?;
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    if code != 20000 {
        return Err(format!("api returned code {}", code));
    }
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("unexpected group list response: {value}"))?;
    data.iter().map(parse_platform_group).collect()
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .user_agent("NRL Pulse/0.1.0")
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|err| format!("build http client failed: {err}"))
}

fn base_candidates(host: &str) -> Vec<String> {
    let trimmed = host.trim().trim_end_matches('/').to_string();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return vec![trimmed];
    }
    let is_ip = IpAddr::from_str(&trimmed).is_ok() || trimmed.eq_ignore_ascii_case("localhost");
    if is_ip {
        vec![format!("http://{trimmed}"), format!("https://{trimmed}")]
    } else {
        vec![format!("https://{trimmed}"), format!("http://{trimmed}")]
    }
}

fn build_register_form(
    payload: PlatformRegisterPayload,
    license_filename: String,
    license_bytes: Vec<u8>,
) -> Result<multipart::Form, String> {
    let mime = if license_filename.ends_with(".png") {
        "image/png"
    } else if license_filename.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    let license_part = multipart::Part::bytes(license_bytes)
        .file_name(license_filename)
        .mime_str(mime)
        .map_err(|err| format!("build upload part failed: {err}"))?;
    Ok(multipart::Form::new()
        .text("callsign", payload.callsign)
        .text("name", payload.name)
        .text("phone", payload.phone)
        .text("password", payload.password)
        .text("address", payload.address)
        .text("mail", payload.mail)
        .part("license", license_part))
}

async fn post_json_candidates<T: DeserializeOwned>(
    client: &Client,
    candidates: &[String],
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> Result<(String, T), String> {
    let mut last_error = String::new();
    for base in candidates {
        match post_json_exact(client, base, path, token, body).await {
            Ok(value) => return Ok((base.clone(), value)),
            Err(err) => last_error = err,
        }
    }
    Err(last_error)
}

async fn post_json_exact<T: DeserializeOwned>(
    client: &Client,
    api_base: &str,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> Result<T, String> {
    let url = format!("{}{}", api_base.trim_end_matches('/'), path);
    let mut request = client.post(url).json(body);
    if let Some(token) = token {
        request = request.header("x-token", token);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("read response failed: {err}"))?;
    if !status.is_success() {
        return Err(format!("http {} {}", status.as_u16(), text));
    }
    serde_json::from_str::<T>(&text)
        .map_err(|err| format!("decode response failed: {err}; body={text}"))
}

async fn post_multipart_candidates<T: DeserializeOwned>(
    client: &Client,
    candidates: &[String],
    path: &str,
    payload: &PlatformRegisterPayload,
    license_filename: &str,
    license_bytes: &[u8],
) -> Result<(String, T), String> {
    let mut last_error = String::new();
    for base in candidates {
        let current_form = build_register_form(
            payload.clone(),
            license_filename.to_string(),
            license_bytes.to_vec(),
        )?;
        match post_multipart_exact(client, base, path, current_form).await {
            Ok(value) => return Ok((base.clone(), value)),
            Err(err) => last_error = err,
        }
    }
    Err(last_error)
}

async fn post_multipart_exact<T: DeserializeOwned>(
    client: &Client,
    api_base: &str,
    path: &str,
    form: multipart::Form,
) -> Result<T, String> {
    let url = format!("{}{}", api_base.trim_end_matches('/'), path);
    let response = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("read response failed: {err}"))?;
    if !status.is_success() {
        return Err(format!("http {} {}", status.as_u16(), text));
    }
    serde_json::from_str::<T>(&text)
        .map_err(|err| format!("decode response failed: {err}; body={text}"))
}

async fn get_data<T: DeserializeOwned>(
    client: &Client,
    api_base: &str,
    path: &str,
    token: Option<&str>,
) -> Result<T, String> {
    let url = format!("{}{}", api_base.trim_end_matches('/'), path);
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.header("x-token", token);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("read response failed: {err}"))?;
    if !status.is_success() {
        return Err(format!("http {} {}", status.as_u16(), text));
    }
    let envelope = serde_json::from_str::<DataEnvelope<T>>(&text)
        .map_err(|err| format!("decode response failed: {err}; body={text}"))?;
    if envelope.code != 20000 {
        return Err(format!("api returned code {}", envelope.code));
    }
    Ok(envelope.data)
}

async fn post_items<T: DeserializeOwned>(
    client: &Client,
    api_base: &str,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> Result<T, String> {
    let value: Value = post_json_exact(client, api_base, path, token, body).await?;
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    if code != 20000 {
        return Err(format!("api returned code {}", code));
    }
    if let Some(data) = value.get("data") {
        if let Ok(items) = serde_json::from_value::<ItemsData<T>>(data.clone()) {
            return Ok(items.items);
        }
        if let Ok(list) = serde_json::from_value::<T>(data.clone()) {
            return Ok(list);
        }
    }
    Err(format!("unexpected items response: {value}"))
}

fn decode_platform_servers(body: &str) -> Result<Vec<PlatformServer>, String> {
    if let Ok(list) = serde_json::from_str::<Vec<PlatformServer>>(body) {
        return Ok(list);
    }
    if let Ok(envelope) = serde_json::from_str::<ItemsEnvelope<Vec<PlatformServer>>>(body) {
        if envelope.code == 20000 {
            return Ok(envelope.data.items);
        }
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(data) = value.get("data") {
            if let Ok(list) = serde_json::from_value::<Vec<PlatformServer>>(data.clone()) {
                return Ok(list);
            }
        }
    }
    Err(format!("unexpected platform server response: {body}"))
}

fn parse_platform_group(value: &Value) -> Result<PlatformGroup, String> {
    Ok(PlatformGroup {
        id: value
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("group missing id: {value}"))? as i32,
        name: value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("group missing name: {value}"))?
            .to_string(),
        group_type: value
            .get("type")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("group missing type: {value}"))? as i32,
        online_dev_number: value
            .get("online_dev_number")
            .or_else(|| value.get("onlineDevNumber"))
            .and_then(Value::as_i64)
            .unwrap_or_default() as i32,
        total_dev_number: value
            .get("total_dev_number")
            .or_else(|| value.get("totalDevNumber"))
            .and_then(Value::as_i64)
            .unwrap_or_default() as i32,
    })
}

fn resolve_group_id(groups: &[PlatformGroup], preferred_group_id: i32) -> i32 {
    if groups.iter().any(|group| group.id == preferred_group_id) {
        preferred_group_id
    } else {
        groups.first().map(|group| group.id).unwrap_or_default()
    }
}
