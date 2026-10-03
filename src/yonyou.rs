//! 用友云客户端 (等价 app/core/yonyou_client.py)

use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::Mutex;
use std::time::Duration;

use crate::config;
use crate::pyutil::{beijing_tz, py_str, quote, quote_plus};
use crate::webdav::{describe_reqwest_error, log_http_request};

/// Token 失效/非法时需要刷新重试的错误码
const TOKEN_ERROR_CODES: [&str; 2] = ["1090003500065", "310036"];

struct TokenCache {
    access_token: String,
    expires_at: DateTime<FixedOffset>,
}

pub struct YonYouClient {
    app_key: String,
    app_secret: String,
    auth_url: String,
    upload_url: String,
    business_type: String,
    token_cache: Mutex<Option<TokenCache>>,
    http: reqwest::Client,
}

/// 上传结果
#[derive(Debug, Clone)]
pub struct UploadResult {
    pub success: bool,
    pub data: Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// 发货单详情结果
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DeliveryDetail {
    pub success: bool,
    pub logistics: Value,
    pub customer_name: Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// 发货列表结果
#[derive(Debug, Clone)]
pub struct DeliveryList {
    pub success: bool,
    pub page_count: Value,
    pub records: Vec<Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

fn now_bj() -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&beijing_tz())
}

/// str(result.get("code")) 的等价
fn code_str(result: &Value) -> String {
    py_str(result.get("code").unwrap_or(&Value::Null))
}

/// result.get("message", "未知错误") 的字符串形式
fn message_of(result: &Value) -> String {
    match result.get("message") {
        None => "未知错误".to_string(),
        Some(v) => py_str(v),
    }
}

impl YonYouClient {
    pub fn new() -> Self {
        let s = config::get();
        let timeout = Duration::from_secs(s.request_timeout.max(0) as u64);
        YonYouClient {
            app_key: s.yonyou_app_key.clone(),
            app_secret: s.yonyou_app_secret.clone(),
            auth_url: s.yonyou_auth_url.clone(),
            upload_url: s.yonyou_upload_url.clone(),
            business_type: s.yonyou_business_type.clone(),
            token_cache: Mutex::new(None),
            http: reqwest::Client::builder()
                .connect_timeout(timeout)
                .read_timeout(timeout)
                .build()
                .expect("构建HTTP客户端失败"),
        }
    }

    /// 生成HMAC-SHA256签名: quote(base64(hmac(secret, "appKey{k}timestamp{t}")))
    fn generate_signature(&self, timestamp: &str) -> String {
        let string_to_sign = format!("appKey{}timestamp{}", self.app_key, timestamp);
        let mut mac = Hmac::<Sha256>::new_from_slice(self.app_secret.as_bytes()).expect("hmac key");
        mac.update(string_to_sign.as_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        quote(&b64, "/")
    }

    fn base_url(&self) -> String {
        // urlparse(auth_url): scheme://netloc
        match self.auth_url.split_once("://") {
            Some((scheme, rest)) => {
                let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
                format!("{}://{}", scheme, netloc)
            }
            None => "://".to_string(),
        }
    }

    async fn get_json(&self, method: &str, url: &str, req: reqwest::RequestBuilder) -> Result<Value, String> {
        let resp = req.send().await.map_err(|e| describe_reqwest_error(&e))?;
        log_http_request(method, url, &resp);
        let body = resp.bytes().await.map_err(|e| describe_reqwest_error(&e))?;
        serde_json::from_slice::<Value>(&body).map_err(|e| {
            // 与 Python json.JSONDecodeError 的提示一致: "<msg>: line L column C (char N)"
            let pos = crate::http::json_error_pos(&body, &e);
            let text = String::from_utf8_lossy(&body);
            let before: String = text.chars().take(pos).collect();
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) + 1;
            format!("{}: line {} column {} (char {})", crate::http::python_json_error_msg(&e), line, col, pos)
        })
    }

    /// 获取access_token，支持缓存和签名错误重试一次
    pub async fn get_access_token(&self, force_refresh: bool) -> Result<String, String> {
        self.get_access_token_inner(force_refresh, 0).await
    }

    fn get_access_token_inner<'a>(
        &'a self,
        force_refresh: bool,
        retry_count: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            if !force_refresh {
                if let Some(c) = self.token_cache.lock().unwrap().as_ref() {
                    if now_bj() < c.expires_at {
                        return Ok(c.access_token.clone());
                    }
                }
            }

            let timestamp = Utc::now().timestamp_millis().to_string();
            let signature = self.generate_signature(&timestamp);
            let url = format!(
                "{}?appKey={}&timestamp={}&signature={}",
                self.auth_url, self.app_key, timestamp, signature
            );
            let result = self.get_json("GET", &url, self.http.get(&url)).await?;

            if result.get("code") == Some(&Value::String("00000".into())) {
                let data = result.get("data").cloned().unwrap_or(Value::Null);
                let access_token = match data.get("access_token") {
                    Some(v) => py_str(v),
                    None => return Err("'access_token'".to_string()),
                };
                let expires_in = data.get("expires_in").and_then(|v| v.as_f64()).unwrap_or(3600.0);
                let expires_at = now_bj() + ChronoDuration::milliseconds(((expires_in - 60.0) * 1000.0) as i64);
                *self.token_cache.lock().unwrap() = Some(TokenCache { access_token: access_token.clone(), expires_at });
                return Ok(access_token);
            }

            let error_code = py_str(result.get("code").unwrap_or(&Value::String(String::new())));
            let error_code = if result.get("code").is_none() { String::new() } else { error_code };
            let error_message = match result.get("message") {
                None => String::new(),
                Some(v) => py_str(v),
            }
            .to_lowercase();
            let is_signature_error =
                error_code == "50000" || error_message.contains("签名") || error_message.contains("signature");

            if is_signature_error && retry_count == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                return self.get_access_token_inner(true, retry_count + 1).await;
            }
            Err(format!("获取Token失败: {}", message_of(&result)))
        })
    }

    /// 上传文件到用友云
    pub async fn upload_file(
        &self,
        file_content: &[u8],
        file_name: &str,
        business_id: &str,
        business_type: Option<&str>,
    ) -> UploadResult {
        self.upload_file_inner(file_content, file_name, business_id, 0, business_type).await
    }

    fn upload_file_inner<'a>(
        &'a self,
        file_content: &'a [u8],
        file_name: &'a str,
        business_id: &'a str,
        retry_count: u32,
        business_type: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = UploadResult> + Send + 'a>> {
        Box::pin(async move {
            let r: Result<UploadResult, String> = async {
                let access_token = self.get_access_token(false).await?;
                let encoded_token = quote(&access_token, "");
                let effective = match business_type {
                    Some(b) if !b.is_empty() => b.to_string(),
                    _ => self.business_type.clone(),
                };
                let url = format!(
                    "{}?access_token={}&businessType={}&businessId={}",
                    self.upload_url, encoded_token, effective, business_id
                );

                let part = reqwest::multipart::Part::bytes(file_content.to_vec())
                    .file_name(html5_form_escape(file_name))
                    .mime_str("application/octet-stream")
                    .map_err(|e| e.to_string())?;
                let form = reqwest::multipart::Form::new().percent_encode_noop().part("files", part);
                let result = self.get_json("POST", &url, self.http.post(&url).multipart(form)).await?;

                if result.get("code") == Some(&Value::String("200".into())) {
                    let item = result
                        .get("data")
                        .and_then(|d| d.get("data"))
                        .and_then(|d| d.get(0))
                        .cloned()
                        .ok_or_else(|| "'data'".to_string())?;
                    return Ok(UploadResult { success: true, data: item, error_code: None, error_message: None });
                }

                let error_code = code_str(&result);
                if TOKEN_ERROR_CODES.contains(&error_code.as_str()) && retry_count == 0 {
                    self.get_access_token(true).await?;
                    return Ok(self
                        .upload_file_inner(file_content, file_name, business_id, retry_count + 1, business_type)
                        .await);
                }
                Ok(UploadResult {
                    success: false,
                    data: Value::Null,
                    error_code: Some(error_code),
                    error_message: Some(message_of(&result)),
                })
            }
            .await;

            r.unwrap_or_else(|e| UploadResult {
                success: false,
                data: Value::Null,
                error_code: Some("NETWORK_ERROR".into()),
                error_message: Some(e),
            })
        })
    }

    /// 查询销售发货单详情并提取物流公司名称(RX003_name)、客户名称(agentId_name)
    pub async fn get_delivery_detail(&self, delivery_id: &str) -> DeliveryDetail {
        self.get_delivery_detail_inner(delivery_id, 0).await
    }

    fn get_delivery_detail_inner<'a>(
        &'a self,
        delivery_id: &'a str,
        retry_count: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DeliveryDetail> + Send + 'a>> {
        Box::pin(async move {
            let r: Result<DeliveryDetail, String> = async {
                let access_token = self.get_access_token(false).await?;
                let encoded_token = quote(&access_token, "");
                let detail_url = format!("{}/iuap-api-gateway/yonbip/sd/voucherdelivery/detail", self.base_url());
                // 原实现把已编码的token再交给 httpx params 编码一次, 这里保持相同的请求形式
                let url = format!(
                    "{}?access_token={}&id={}",
                    detail_url,
                    quote_plus(&encoded_token),
                    quote_plus(delivery_id)
                );
                let result = self.get_json("GET", &url, self.http.get(&url)).await?;

                if code_str(&result) == "200" {
                    let data = match result.get("data") {
                        Some(d) if crate::pyutil::py_truthy(d) => d.clone(),
                        _ => json!({}),
                    };
                    let define = match data.get("deliveryVoucherDefineCharacter") {
                        Some(d) if crate::pyutil::py_truthy(d) => d.clone(),
                        _ => json!({}),
                    };
                    return Ok(DeliveryDetail {
                        success: true,
                        logistics: define.get("RX003_name").cloned().unwrap_or(Value::Null),
                        customer_name: data.get("agentId_name").cloned().unwrap_or(Value::Null),
                        error_code: None,
                        error_message: None,
                    });
                }

                let error_code = code_str(&result);
                let error_message = message_of(&result);
                if TOKEN_ERROR_CODES.contains(&error_code.as_str()) && retry_count == 0 {
                    self.get_access_token(true).await?;
                    return Ok(self.get_delivery_detail_inner(delivery_id, retry_count + 1).await);
                }
                Ok(DeliveryDetail {
                    success: false,
                    logistics: Value::Null,
                    customer_name: Value::Null,
                    error_code: Some(error_code),
                    error_message: Some(error_message),
                })
            }
            .await;

            r.unwrap_or_else(|e| DeliveryDetail {
                success: false,
                logistics: Value::Null,
                customer_name: Value::Null,
                error_code: Some("NETWORK_ERROR".into()),
                error_message: Some(e),
            })
        })
    }

    /// 查询销售发货列表(表头级, isSum=true)
    pub async fn get_delivery_list(
        &self,
        page_index: i64,
        page_size: i64,
        vouchdate_begin: &str,
        vouchdate_end: &str,
    ) -> DeliveryList {
        self.get_delivery_list_inner(page_index, page_size, vouchdate_begin, vouchdate_end, 0).await
    }

    fn get_delivery_list_inner<'a>(
        &'a self,
        page_index: i64,
        page_size: i64,
        vouchdate_begin: &'a str,
        vouchdate_end: &'a str,
        retry_count: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DeliveryList> + Send + 'a>> {
        Box::pin(async move {
            let r: Result<DeliveryList, String> = async {
                let access_token = self.get_access_token(false).await?;
                let encoded_token = quote(&access_token, "");
                let list_url = format!(
                    "{}/iuap-api-gateway/yonbip/sd/voucherdelivery/list?access_token={}",
                    self.base_url(),
                    encoded_token
                );
                let body = json!({
                    "isDefault": true,
                    "pageIndex": page_index,
                    "pageSize": page_size,
                    "open_vouchdate_begin": vouchdate_begin,
                    "open_vouchdate_end": vouchdate_end,
                    "isSum": true,
                    "simpleVOs": [],
                    "queryOrders": [{"field": "vouchdate", "order": "desc"}]
                });
                let result = self
                    .get_json("POST", &list_url, self.http.post(&list_url).json(&body))
                    .await?;

                if code_str(&result) == "200" {
                    let data = match result.get("data") {
                        Some(d) if crate::pyutil::py_truthy(d) => d.clone(),
                        _ => json!({}),
                    };
                    let records = match data.get("recordList") {
                        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
                        _ => vec![],
                    };
                    return Ok(DeliveryList {
                        success: true,
                        page_count: data.get("pageCount").cloned().unwrap_or(json!(0)),
                        records,
                        error_code: None,
                        error_message: None,
                    });
                }

                let error_code = code_str(&result);
                let error_message = message_of(&result);
                if TOKEN_ERROR_CODES.contains(&error_code.as_str()) && retry_count == 0 {
                    self.get_access_token(true).await?;
                    return Ok(self
                        .get_delivery_list_inner(page_index, page_size, vouchdate_begin, vouchdate_end, retry_count + 1)
                        .await);
                }
                Ok(DeliveryList {
                    success: false,
                    page_count: json!(0),
                    records: vec![],
                    error_code: Some(error_code),
                    error_message: Some(error_message),
                })
            }
            .await;

            r.unwrap_or_else(|e| DeliveryList {
                success: false,
                page_count: json!(0),
                records: vec![],
                error_code: Some("NETWORK_ERROR".into()),
                error_message: Some(e),
            })
        })
    }
}

/// httpx 对 multipart 参数值的 HTML5 转义: `"` -> %22, `\` -> `\\`, 控制字符 -> %XX
fn html5_form_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '"' => out.push_str("%22"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 && c != '\u{1b}' => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_python_algorithm() {
        // 期望值: base64(hmac_sha256("secret", "appKeykeytimestamp1700000000000")) 再 quote
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"appKeykeytimestamp1700000000000");
        let b64 = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(quote(&b64, "/"), b64.replace('+', "%2B").replace('=', "%3D"));
    }
}
