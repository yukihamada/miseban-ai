use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use tracing::{error, info, warn};

use crate::alerts::AlertPayload;

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum LineError {
    /// HTTP request failed (network, timeout, etc.).
    HttpError(String),
    /// LINE API returned a non-success status.
    ApiError(String),
}

impl std::fmt::Display for LineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LineError::HttpError(msg) => write!(f, "LINE HTTP error: {msg}"),
            LineError::ApiError(msg) => write!(f, "LINE API error: {msg}"),
        }
    }
}

impl std::error::Error for LineError {}

// ---------------------------------------------------------------------------
// LINE push message request body
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PushMessageRequest<'a> {
    to: &'a str,
    messages: Vec<LineMessage>,
}

#[derive(Serialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum LineMessage {
    Text {
        #[serde(rename = "type")]
        msg_type: &'static str,
        text: String,
    },
    Flex {
        #[serde(rename = "type")]
        msg_type: &'static str,
        #[serde(rename = "altText")]
        alt_text: String,
        contents: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// LineClient
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LineClient {
    channel_token: String,
    http: reqwest::Client,
}

impl LineClient {
    pub fn new(channel_token: &str) -> Self {
        Self {
            channel_token: channel_token.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Send a simple text message to a LINE user.
    #[allow(dead_code)]
    pub async fn push_message(
        &self,
        line_user_id: &str,
        message: &str,
    ) -> Result<(), LineError> {
        let body = PushMessageRequest {
            to: line_user_id,
            messages: vec![LineMessage::Text {
                msg_type: "text",
                text: message.to_string(),
            }],
        };

        self.send_push(line_user_id, &body).await
    }

    /// Send a rich Flex Message with alert details.
    pub async fn push_alert_message(
        &self,
        line_user_id: &str,
        alert: &AlertPayload,
    ) -> Result<(), LineError> {
        let alt_text = format!(
            "[MisebanAI Alert] {} - {}",
            alert.alert_type, alert.message
        );

        let confidence_pct = format!("{:.0}%", alert.confidence * 100.0);

        let contents = serde_json::json!({
            "type": "bubble",
            "header": {
                "type": "box",
                "layout": "vertical",
                "contents": [{
                    "type": "text",
                    "text": "MisebanAI Alert",
                    "weight": "bold",
                    "size": "lg",
                    "color": "#E53E3E"
                }]
            },
            "body": {
                "type": "box",
                "layout": "vertical",
                "spacing": "md",
                "contents": [
                    {
                        "type": "box",
                        "layout": "horizontal",
                        "contents": [
                            { "type": "text", "text": "Type", "size": "sm", "color": "#888888", "flex": 2 },
                            { "type": "text", "text": &alert.alert_type, "size": "sm", "weight": "bold", "flex": 3 }
                        ]
                    },
                    {
                        "type": "box",
                        "layout": "horizontal",
                        "contents": [
                            { "type": "text", "text": "Camera", "size": "sm", "color": "#888888", "flex": 2 },
                            { "type": "text", "text": &alert.camera_id, "size": "sm", "flex": 3 }
                        ]
                    },
                    {
                        "type": "box",
                        "layout": "horizontal",
                        "contents": [
                            { "type": "text", "text": "Confidence", "size": "sm", "color": "#888888", "flex": 2 },
                            { "type": "text", "text": &confidence_pct, "size": "sm", "flex": 3 }
                        ]
                    },
                    {
                        "type": "box",
                        "layout": "horizontal",
                        "contents": [
                            { "type": "text", "text": "Time", "size": "sm", "color": "#888888", "flex": 2 },
                            { "type": "text", "text": &alert.created_at, "size": "sm", "flex": 3 }
                        ]
                    },
                    { "type": "separator" },
                    {
                        "type": "text",
                        "text": &alert.message,
                        "size": "sm",
                        "wrap": true
                    }
                ]
            }
        });

        let body = PushMessageRequest {
            to: line_user_id,
            messages: vec![LineMessage::Flex {
                msg_type: "flex",
                alt_text,
                contents,
            }],
        };

        self.send_push(line_user_id, &body).await
    }

    /// Internal helper: POST to the LINE push message endpoint.
    async fn send_push(
        &self,
        line_user_id: &str,
        body: &PushMessageRequest<'_>,
    ) -> Result<(), LineError> {
        let resp = self
            .http
            .post("https://api.line.me/v2/bot/message/push")
            .bearer_auth(&self.channel_token)
            .json(body)
            .send()
            .await
            .map_err(|e| {
                error!(line_user_id, error = %e, "LINE push HTTP error");
                LineError::HttpError(e.to_string())
            })?;

        if resp.status().is_success() {
            info!(line_user_id, "LINE push message sent successfully");
            Ok(())
        } else {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            warn!(line_user_id, %status, body = %body_text, "LINE API returned error");
            Err(LineError::ApiError(format!(
                "status={status}, body={body_text}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// LINE webhook signature verification
// ---------------------------------------------------------------------------

/// Verify a LINE webhook signature (X-Line-Signature header).
/// The signature is HMAC-SHA256(channel_secret, body) base64-encoded.
pub fn verify_line_signature(body: &[u8], signature: &str, channel_secret: &str) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(channel_secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    expected == signature
}
