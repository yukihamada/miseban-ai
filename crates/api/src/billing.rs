use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

// Stripe API base URL
const STRIPE_API: &str = "https://api.stripe.com/v1";

// ---------------------------------------------------------------------------
// Stripe Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StripeClient {
    secret_key: String,
    http: Client,
}

impl StripeClient {
    pub fn new(secret_key: &str) -> Self {
        Self {
            secret_key: secret_key.to_string(),
            http: Client::new(),
        }
    }

    /// Create a Stripe Checkout Session for subscription.
    pub async fn create_checkout_session(
        &self,
        price_id: &str,
        customer_email: &str,
        store_id: &Uuid,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<CheckoutSession, StripeError> {
        let params = [
            ("mode", "subscription"),
            ("payment_method_types[]", "card"),
            ("line_items[0][price]", price_id),
            ("line_items[0][quantity]", "1"),
            ("customer_email", customer_email),
            ("success_url", success_url),
            ("cancel_url", cancel_url),
            ("metadata[store_id]", &store_id.to_string()),
            ("allow_promotion_codes", "true"),
        ];

        let resp = self
            .http
            .post(&format!("{}/checkout/sessions", STRIPE_API))
            .basic_auth(&self.secret_key, None::<&str>)
            .form(&params)
            .send()
            .await
            .map_err(|e| StripeError::HttpError(e.to_string()))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StripeError::ApiError(body));
        }

        resp.json()
            .await
            .map_err(|e| StripeError::HttpError(e.to_string()))
    }

    /// Create a Customer Portal session for managing subscription.
    pub async fn create_portal_session(
        &self,
        customer_id: &str,
        return_url: &str,
    ) -> Result<PortalSession, StripeError> {
        let params = [("customer", customer_id), ("return_url", return_url)];

        let resp = self
            .http
            .post(&format!("{}/billing_portal/sessions", STRIPE_API))
            .basic_auth(&self.secret_key, None::<&str>)
            .form(&params)
            .send()
            .await
            .map_err(|e| StripeError::HttpError(e.to_string()))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StripeError::ApiError(body));
        }

        resp.json()
            .await
            .map_err(|e| StripeError::HttpError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Response Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CheckoutSession {
    #[allow(dead_code)]
    pub id: String,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PortalSession {
    #[allow(dead_code)]
    pub id: String,
    pub url: String,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum StripeError {
    HttpError(String),
    ApiError(String),
}

impl std::fmt::Display for StripeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StripeError::HttpError(e) => write!(f, "HTTP error: {e}"),
            StripeError::ApiError(e) => write!(f, "Stripe API error: {e}"),
        }
    }
}

impl std::error::Error for StripeError {}

// ---------------------------------------------------------------------------
// Webhook Event Parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StripeEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: StripeEventData,
}

#[derive(Debug, Deserialize)]
pub struct StripeEventData {
    pub object: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Pricing Info (static, returned by GET /api/v1/pricing)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PricingPlan {
    pub tier: String,
    pub name: String,
    pub price_monthly: u32, // JPY
    pub cameras: String,
    pub retention: String,
    pub features: Vec<String>,
    pub stripe_price_id: Option<String>,
}

pub fn get_pricing_plans() -> Vec<PricingPlan> {
    vec![
        PricingPlan {
            tier: "free".into(),
            name: "フリー".into(),
            price_monthly: 0,
            cameras: "1台".into(),
            retention: "7日間".into(),
            features: vec!["基本来客カウント".into(), "日次レポート".into()],
            stripe_price_id: None,
        },
        PricingPlan {
            tier: "starter".into(),
            name: "スターター".into(),
            price_monthly: 9800,
            cameras: "4台まで".into(),
            retention: "30日間".into(),
            features: vec![
                "来客カウント".into(),
                "属性分析".into(),
                "日次レポート".into(),
                "LINEアラート".into(),
            ],
            stripe_price_id: std::env::var("STRIPE_PRICE_STARTER").ok(),
        },
        PricingPlan {
            tier: "pro".into(),
            name: "プロ".into(),
            price_monthly: 29800,
            cameras: "16台まで".into(),
            retention: "90日間".into(),
            features: vec![
                "全分析機能".into(),
                "ヒートマップ".into(),
                "リアルタイムアラート".into(),
                "CSV出力".into(),
                "APIアクセス".into(),
            ],
            stripe_price_id: std::env::var("STRIPE_PRICE_PRO").ok(),
        },
        PricingPlan {
            tier: "enterprise".into(),
            name: "エンタープライズ".into(),
            price_monthly: 49800,
            cameras: "無制限".into(),
            retention: "無制限".into(),
            features: vec![
                "全機能".into(),
                "カスタムモデル".into(),
                "専任サポート".into(),
                "SLA保証".into(),
                "オンプレミス対応".into(),
            ],
            stripe_price_id: std::env::var("STRIPE_PRICE_ENTERPRISE").ok(),
        },
    ]
}

// ---------------------------------------------------------------------------
// DB functions
// ---------------------------------------------------------------------------

/// Get Stripe customer_id for a store.
pub async fn get_stripe_customer_id(pool: &PgPool, store_id: &Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT stripe_customer_id FROM stores WHERE id = $1")
        .bind(store_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Save Stripe customer_id to store.
pub async fn set_stripe_customer_id(
    pool: &PgPool,
    store_id: &Uuid,
    customer_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE stores SET stripe_customer_id = $1 WHERE id = $2")
        .bind(customer_id)
        .bind(store_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update store's plan tier.
pub async fn update_plan_tier(
    pool: &PgPool,
    store_id: &Uuid,
    tier: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE stores SET plan_tier = $1, updated_at = NOW() WHERE id = $2")
        .bind(tier)
        .bind(store_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Get store by Stripe customer_id (for webhook processing).
pub async fn get_store_by_stripe_customer(
    pool: &PgPool,
    customer_id: &str,
) -> Option<(Uuid, String)> {
    sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, plan_tier FROM stores WHERE stripe_customer_id = $1 LIMIT 1",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Get subscription status for a store.
pub async fn get_subscription_status(pool: &PgPool, store_id: &Uuid) -> SubscriptionInfo {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT plan_tier, stripe_customer_id FROM stores WHERE id = $1")
            .bind(store_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    match row {
        Some((tier, customer_id)) => SubscriptionInfo {
            plan_tier: tier,
            stripe_customer_id: customer_id,
            is_active: true,
        },
        None => SubscriptionInfo {
            plan_tier: "free".into(),
            stripe_customer_id: None,
            is_active: false,
        },
    }
}

#[derive(Serialize)]
pub struct SubscriptionInfo {
    pub plan_tier: String,
    pub stripe_customer_id: Option<String>,
    pub is_active: bool,
}
