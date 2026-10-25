//! Ad-platform integrations (Meta, TikTok, Pinterest, Snapchat, Reddit,
//! LinkedIn, X), the BigQuery export,
//! the Shopify order webhook and the enrichment endpoint the ad platforms
//! share. Their settings live in a JSON file the admin page
//! edits and every instance re-reads when it changes, so a pixel id or token
//! is changed without a deploy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::container::{Container, Entry, Tag};
use crate::container_file::ContainerFile;
use crate::sandbox::bigquery_insert_url;
use crate::tenants::Resolved;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Text,
    /// Never shown again after it is saved.
    Secret,
    /// "true" or "false".
    Toggle,
}

pub struct FieldDef {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub required: bool,
    pub help: &'static str,
}

pub struct ProviderDef {
    pub id: &'static str,
    pub name: &'static str,
    /// Built-in template the provider's tag runs.
    pub template: &'static str,
    pub help: &'static str,
    pub fields: &'static [FieldDef],
    /// Gets the enrichment endpoint's settings, to look the visitor up.
    pub enriched: bool,
    /// Runs as a client that takes in requests, rather than as a tag.
    pub client: bool,
}

/// Every ad tag sends only when the hit's Consent Mode state grants both
/// ad_storage and ad_user_data; this lets hits with no consent signals through.
const SEND_WITHOUT_CONSENT: FieldDef = FieldDef {
    key: "send_without_consent",
    label: "Send when the hit has no consent signals",
    kind: Kind::Toggle,
    required: false,
    help: "For sites without Consent Mode, and for Shopify orders, which carry no consent signals. A visitor who denied ad storage or ad user data is never sent.",
};

pub const PROVIDERS: &[ProviderDef] = &[
    ProviderDef {
        id: "meta",
        name: "Meta Conversions API",
        template: "meta_capi",
        help: "Facebook and Instagram ads. Events Manager > your pixel > Settings > Conversions API.",
        fields: &[
            FieldDef {
                key: "pixel_id",
                label: "Pixel (dataset) ID",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "Generated in Events Manager under Conversions API.",
            },
            FieldDef {
                key: "test_event_code",
                label: "Test event code",
                kind: Kind::Text,
                required: false,
                help: "While set, events only show under Test events and aren't used for ads. Clear it to go live.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "tiktok",
        name: "TikTok Events API",
        template: "tiktok_events",
        help: "TikTok ads. Events Manager > your web event source > Settings > Events API.",
        fields: &[
            FieldDef {
                key: "pixel_code",
                label: "Pixel code",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            FieldDef {
                key: "test_event_code",
                label: "Test event code",
                kind: Kind::Text,
                required: false,
                help: "From the Test events tab. Clear it to go live.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "pinterest",
        name: "Pinterest Conversions API",
        template: "pinterest_capi",
        help: "Pinterest ads. Ads Manager > Conversions > Conversion API.",
        fields: &[
            FieldDef {
                key: "ad_account_id",
                label: "Ad account ID",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            FieldDef {
                key: "test_mode",
                label: "Test mode",
                kind: Kind::Toggle,
                required: false,
                help: "Sends events to Pinterest's test endpoint instead of counting them. Turn it off to go live.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "snapchat",
        name: "Snapchat Conversions API",
        template: "snapchat_capi",
        help: "Snapchat ads. Events Manager > your Pixel > Conversions API > Generate token.",
        fields: &[
            FieldDef {
                key: "pixel_id",
                label: "Pixel ID",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            FieldDef {
                key: "test_mode",
                label: "Test mode",
                kind: Kind::Toggle,
                required: false,
                help: "Sends events to Snap's validate endpoint, which checks them without counting them. Turn it off to go live.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "reddit",
        name: "Reddit Conversions API",
        template: "reddit_capi",
        help: "Reddit ads. Events Manager > your pixel > Conversions API > Generate access token.",
        fields: &[
            FieldDef {
                key: "pixel_id",
                label: "Pixel ID",
                kind: Kind::Text,
                required: true,
                help: "Starts with a2_ or t2_, shown next to the pixel in Events Manager.",
            },
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            FieldDef {
                key: "test_event_code",
                label: "Test ID",
                kind: Kind::Text,
                required: false,
                help: "From Events Manager > Test events. While set, events only show there. Clear it to go live.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "linkedin",
        name: "LinkedIn Conversions API",
        template: "linkedin_capi",
        help: "LinkedIn ads. Campaign Manager > Data > Sources > Conversions API; create a conversion rule per event with the Conversions API as its source. LinkedIn needs the visitor's email or the li_fat_id click id (turn on enhanced conversion tracking in the Insight Tag), so other events are skipped.",
        fields: &[
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "A token with the rw_conversions scope.",
            },
            FieldDef {
                key: "conversions",
                label: "Conversion rules",
                kind: Kind::Text,
                required: true,
                help: "Each GA4 event with the id of its conversion rule, separated by commas, such as purchase=12345, generate_lead=67890. Other events are skipped.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "x",
        name: "X Conversions API",
        template: "x_capi",
        help: "X (Twitter) ads. Ads Manager > Tools > Events Manager for the pixel and event ids; developer.x.com for an app with Ads API access and its keys. X needs the visitor's email, phone or twclid click id, so other events are skipped.",
        fields: &[
            FieldDef {
                key: "pixel_id",
                label: "Pixel ID",
                kind: Kind::Text,
                required: true,
                help: "The base pixel id, such as o8z1j.",
            },
            FieldDef {
                key: "event_ids",
                label: "Event ids",
                kind: Kind::Text,
                required: true,
                help: "Each GA4 event with the id of its X event, separated by commas, such as purchase=tw-o8z1j-o8z2m, add_to_cart=tw-o8z1j-o8z3a. Other events are skipped.",
            },
            FieldDef {
                key: "consumer_key",
                label: "API key (consumer key)",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "consumer_secret",
                label: "API key secret",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            FieldDef {
                key: "access_token",
                label: "Access token",
                kind: Kind::Secret,
                required: true,
                help: "For a user with access to the ad account.",
            },
            FieldDef {
                key: "access_token_secret",
                label: "Access token secret",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
    ProviderDef {
        id: "bigquery",
        name: "BigQuery export",
        template: "bigquery_export",
        help: "Writes every event as a row of a BigQuery table, for your own reports. The server's service account needs the BigQuery Data Editor role on the dataset.",
        fields: &[
            FieldDef {
                key: "project_id",
                label: "Project ID",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "dataset_id",
                label: "Dataset ID",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "table_id",
                label: "Table ID",
                kind: Kind::Text,
                required: true,
                help: "Columns the table doesn't have are left out, so it can keep just the ones you need.",
            },
            FieldDef {
                key: "events",
                label: "Events to export",
                kind: Kind::Text,
                required: false,
                help: "GA4 event names separated by commas, such as purchase, generate_lead. Empty exports every event.",
            },
            FieldDef {
                key: "send_without_consent",
                label: "Export when the hit has no consent signals",
                kind: Kind::Toggle,
                required: false,
                help: "For sites without Consent Mode, and for Shopify orders, which carry no consent signals. A visitor who denied analytics storage is never exported.",
            },
        ],
        enriched: false,
        client: false,
    },
    ProviderDef {
        id: "shopify",
        name: "Shopify orders",
        template: "shopify_client",
        help: "Sends every paid Shopify order to the tags above as a purchase, from the server. In Shopify: Settings > Notifications > Webhooks > Create webhook, event \"Order payment\", format JSON, URL https://<your site>/metrics/shopify/orders.",
        fields: &[
            FieldDef {
                key: "webhook_secret",
                label: "Webhook signing secret",
                kind: Kind::Secret,
                required: true,
                help: "Shown under the webhook list: \"Your webhooks will be signed with ...\". For webhooks an app creates, the app's client secret.",
            },
            FieldDef {
                key: "item_id",
                label: "Product id to send",
                kind: Kind::Text,
                required: false,
                help: "variant_id (default, as in Shopify's Meta catalog), product_id or sku: whichever your ad catalogs use.",
            },
        ],
        enriched: false,
        client: true,
    },
    ProviderDef {
        id: "google_ads",
        name: "Google Ads conversions",
        template: "google_ads",
        help: "Google Ads conversions through Google's Data Manager API, with the ad click kept in a first-party cookie. Needs a conversion action in the account, and an OAuth client with a refresh token for the datamanager scope. Use test mode first.",
        fields: &[
            FieldDef {
                key: "customer_id",
                label: "Google Ads account ID",
                kind: Kind::Text,
                required: true,
                help: "The account the conversion action is in, e.g. 123-456-7890.",
            },
            FieldDef {
                key: "conversion_action_id",
                label: "Conversion action ID",
                kind: Kind::Text,
                required: true,
                help: "Goals > Conversions > the action: the ctId in its address.",
            },
            FieldDef {
                key: "conversion_event",
                label: "Event that counts",
                kind: Kind::Text,
                required: false,
                help: "The GA4 event sent as this conversion. Empty means purchase.",
            },
            FieldDef {
                key: "login_customer_id",
                label: "Manager account ID",
                kind: Kind::Text,
                required: false,
                help: "Only when access goes through a manager (MCC) account.",
            },
            FieldDef {
                key: "client_id",
                label: "OAuth client ID",
                kind: Kind::Text,
                required: true,
                help: "",
            },
            FieldDef {
                key: "client_secret",
                label: "OAuth client secret",
                kind: Kind::Secret,
                required: true,
                help: "",
            },
            FieldDef {
                key: "refresh_token",
                label: "Refresh token",
                kind: Kind::Secret,
                required: true,
                help: "For a Google user with access to the account, with the https://www.googleapis.com/auth/datamanager scope.",
            },
            FieldDef {
                key: "test_mode",
                label: "Test mode",
                kind: Kind::Toggle,
                required: false,
                help: "Google checks each conversion but imports nothing. Turn it off to go live.",
            },
            SEND_WITHOUT_CONSENT,
        ],
        enriched: true,
        client: false,
    },
];

/// GA4 events the admin page can send as a test.
pub const TEST_EVENTS: &[&str] = &[
    "purchase",
    "page_view",
    "view_item",
    "add_to_cart",
    "begin_checkout",
    "generate_lead",
    "sign_up",
];

const TEST_CLIENT: &str = include_str!("templates/admin_test_client.js");

/// Test settings that apply to one test event only, so a test never needs the
/// saved settings changed.
#[derive(Debug, Default, Deserialize)]
pub struct TestOverrides {
    /// Meta's and TikTok's test event code.
    #[serde(default)]
    pub test_event_code: String,
    /// Pinterest's test mode.
    #[serde(default)]
    pub test_mode: bool,
}

fn provider(id: &str) -> Option<&'static ProviderDef> {
    PROVIDERS.iter().find(|p| p.id == id)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// An endpoint on the site's own server that tells the tags who a visitor is
/// (hashed or plain email, phone and so on), so conversions match better.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Enrichment {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    /// The event field sent to the endpoint: `user_id` or `client_id`.
    #[serde(default = "user_id")]
    pub key: String,
    #[serde(default)]
    pub header_name: String,
    #[serde(default)]
    pub header_value: String,
    /// Also use the user data the page sent with the event (GA4
    /// user-provided data), hashed before it is sent. Works with or without
    /// an endpoint; where both have a field, the endpoint's answer wins.
    #[serde(default)]
    pub event_user_data: bool,
}

fn user_id() -> String {
    "user_id".into()
}

impl Default for Enrichment {
    fn default() -> Self {
        Enrichment {
            enabled: false,
            url: String::new(),
            key: user_id(),
            header_name: String::new(),
            header_value: String::new(),
            event_user_data: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub enrichment: Enrichment,
}

/// Settings that pair GA4 event names with a platform's ids, such as
/// "purchase=123, generate_lead=456".
const EVENT_MAP_FIELDS: &[&str] = &["conversions", "event_ids"];

fn event_map_ok(v: &str) -> bool {
    let word = |s: &str, extra: &[char]| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || extra.contains(&c))
    };
    v.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .all(|p| {
            p.split_once('=')
                .is_some_and(|(k, id)| word(k.trim(), &[]) && word(id.trim(), &['-', ':', '.']))
        })
}

fn plain(v: &str, max: usize) -> bool {
    v.len() <= max && !v.chars().any(|c| c.is_control())
}

impl Config {
    fn validate(&mut self) -> Result<(), String> {
        for (id, cfg) in &mut self.providers {
            let def = provider(id).ok_or_else(|| format!("unknown provider {id}"))?;
            for (k, v) in &mut cfg.fields {
                let field = def
                    .fields
                    .iter()
                    .find(|f| f.key == k)
                    .ok_or_else(|| format!("{}: unknown field {k}", def.name))?;
                *v = v.trim().to_string();
                if !plain(v, 1024) {
                    return Err(format!("{}: {} has odd characters", def.name, field.label));
                }
                if field.kind == Kind::Toggle && !matches!(v.as_str(), "" | "true" | "false") {
                    return Err(format!(
                        "{}: {} must be true or false",
                        def.name, field.label
                    ));
                }
            }
            if def.id == "shopify"
                && !matches!(
                    cfg.fields.get("item_id").map_or("", String::as_str),
                    "" | "variant_id" | "product_id" | "sku"
                )
            {
                return Err(format!(
                    "{}: product id must be variant_id, product_id or sku",
                    def.name
                ));
            }
            for key in EVENT_MAP_FIELDS {
                if cfg.fields.get(*key).is_some_and(|v| !event_map_ok(v)) {
                    return Err(format!(
                        "{}: write each event as name=id, separated by commas",
                        def.name
                    ));
                }
            }
            if def.id == "bigquery" {
                let get = |k: &str| cfg.fields.get(k).map_or("", String::as_str);
                let ids = [get("project_id"), get("dataset_id"), get("table_id")];
                if ids.iter().all(|v| !v.is_empty())
                    && bigquery_insert_url(ids[0], ids[1], ids[2]).is_none()
                {
                    return Err(format!("{}: odd project, dataset or table ID", def.name));
                }
            }
            if cfg.enabled {
                for f in def.fields.iter().filter(|f| f.required) {
                    if cfg.fields.get(f.key).is_none_or(|v| v.is_empty()) {
                        return Err(format!("{}: {} is required", def.name, f.label));
                    }
                }
            }
        }
        let e = &mut self.enrichment;
        e.url = e.url.trim().to_string();
        e.header_name = e.header_name.trim().to_string();
        e.header_value = e.header_value.trim().to_string();
        if !matches!(e.key.as_str(), "user_id" | "client_id") {
            return Err("enrichment: key must be user_id or client_id".into());
        }
        if !e.url.is_empty()
            && (!e.url.starts_with("https://")
                || e.url.len() > 2048
                || e.url.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err("enrichment: the URL must be an https:// address".into());
        }
        if e.enabled && e.url.is_empty() {
            return Err("enrichment: the URL is required".into());
        }
        if e.header_name.is_empty() != e.header_value.is_empty() {
            return Err("enrichment: header name and value go together".into());
        }
        if !e
            .header_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
            || !plain(&e.header_value, 1024)
        {
            return Err("enrichment: odd header name or value".into());
        }
        Ok(())
    }

    /// The base container plus one tag or client per enabled provider.
    fn container(&self, base: &Container) -> Container {
        let mut c = base.clone();
        c.version = "integrations".into();
        for def in PROVIDERS {
            if let Some(cfg) = self.providers.get(def.id).filter(|c| c.enabled) {
                if def.client {
                    let mut data = self.tag(def, cfg).data;
                    if let (Some(url), Some(o)) = (site_url(base), data.as_object_mut()) {
                        o.insert("siteUrl".into(), Value::String(url));
                    }
                    // Ahead of the base clients, which never claim its path anyway.
                    c.clients.insert(
                        0,
                        Entry {
                            name: def.name.into(),
                            template: def.template.into(),
                            data,
                        },
                    );
                } else {
                    c.tags.push(self.tag(def, cfg));
                }
            }
        }
        c.compile().expect("built-in provider templates exist");
        c
    }

    fn tag(&self, def: &ProviderDef, cfg: &ProviderConfig) -> Tag {
        let mut data = serde_json::Map::new();
        for (k, v) in &cfg.fields {
            data.insert(k.clone(), Value::String(v.clone()));
        }
        let e = &self.enrichment;
        if def.enriched && e.enabled && !e.url.is_empty() {
            data.insert(
                "enrich".into(),
                json!({
                    "url": e.url,
                    "key": e.key,
                    "headerName": e.header_name,
                    "headerValue": e.header_value,
                }),
            );
        }
        if e.event_user_data {
            data.insert("eventUserData".into(), Value::Bool(true));
        }
        Tag {
            name: def.name.into(),
            template: def.template.into(),
            data: Value::Object(data),
            fire_when: vec![],
            enabled: true,
        }
    }

    /// A container that runs one provider's tag, on or off, for the event in
    /// the request body, and nothing else: no GA4 tag, so a test never
    /// reaches Analytics.
    fn test_container(
        &self,
        base: &Container,
        id: &str,
        overrides: &TestOverrides,
    ) -> Result<Container, String> {
        let def = provider(id)
            .filter(|d| !d.client)
            .ok_or("no such provider")?;
        let mut cfg = self.providers.get(id).cloned().unwrap_or_default();
        for f in def.fields.iter().filter(|f| f.required) {
            if cfg.fields.get(f.key).is_none_or(|v| v.is_empty()) {
                return Err(format!("{}: save the {} first", def.name, f.label));
            }
        }
        let has = |key: &str| def.fields.iter().any(|f| f.key == key);
        let code = overrides.test_event_code.trim();
        if !code.is_empty() {
            if !has("test_event_code") || !plain(code, 64) {
                return Err(format!("{}: no test event code here", def.name));
            }
            cfg.fields.insert("test_event_code".into(), code.into());
        }
        if overrides.test_mode {
            if !has("test_mode") {
                return Err(format!("{}: has no test mode", def.name));
            }
            cfg.fields.insert("test_mode".into(), "true".into());
        }
        let mut templates = base.templates.clone();
        templates.insert("admin_test_client".into(), TEST_CLIENT.into());
        let mut c = Container {
            version: "admin-test".into(),
            templates,
            permissions: Default::default(),
            clients: vec![Entry {
                name: "Admin test".into(),
                template: "admin_test_client".into(),
                data: json!({}),
            }],
            tags: vec![self.tag(def, &cfg)],
            transformations: vec![],
        };
        c.compile()?;
        Ok(c)
    }
}

/// A made-up event of the given kind, as the GA4 client would produce it,
/// with every Consent Mode signal granted. The ids start with "test-" so the
/// event is easy to find on the platform's side.
pub fn test_event(name: &str, ip: &str, user_agent: &str, page: &str) -> Result<Value, String> {
    if !TEST_EVENTS.contains(&name) {
        return Err(format!("no test for {name}"));
    }
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut e = json!({
        "event_name": name,
        "event_id": format!("test-{now}"),
        "client_id": format!("{}.{}", now % 1_000_000_000, now / 1000),
        "user_id": "admin-test",
        "ip_override": ip,
        "user_agent": user_agent,
        "page_location": page,
        "page_title": "Tagging server test event",
        "x-ga-gcs": "G111",
        "x-ga-gcd": "13t3t3t3t5l1",
        // LinkedIn and X skip visitors they can't match.
        "user_data": { "email_address": "admin-test@example.com" },
        // Made by the server, so the ad tags use its user data.
        "x-sst-source": "admin_test",
    });
    if matches!(
        name,
        "purchase" | "view_item" | "add_to_cart" | "begin_checkout"
    ) {
        let o = e.as_object_mut().expect("object");
        o.insert("currency".into(), json!("TRY"));
        o.insert("value".into(), json!(1.0));
        o.insert(
            "items".into(),
            json!([{ "item_id": "TEST-1", "item_name": "Test product", "price": 1.0, "quantity": 1 }]),
        );
        if name == "purchase" {
            o.insert("transaction_id".into(), json!(format!("test-{now}")));
        }
    }
    Ok(e)
}

/// The first of the GA4 client's site domains as an address, if the
/// container names any.
fn site_url(base: &Container) -> Option<String> {
    base.clients
        .iter()
        .filter_map(|c| c.data.get("siteDomains")?.as_array()?.first()?.as_str())
        .find(|d| !d.is_empty())
        .map(|d| format!("https://{d}/"))
}

struct State {
    config: Config,
    modified: Option<SystemTime>,
    /// Generation of the base container the tags were added to.
    base_generation: u64,
    resolved: Resolved,
}

pub struct Integrations {
    path: PathBuf,
    /// The container file (or built-in container) the platform tags are
    /// added on top of; an edit to it rebuilds the container.
    base: Arc<ContainerFile>,
    state: RwLock<State>,
    generation: AtomicU64,
    /// Serialises admin writes.
    write: Mutex<()>,
}

impl Integrations {
    pub fn open(path: &Path, base: Arc<ContainerFile>) -> Result<Arc<Self>, String> {
        let first = base.resolved();
        let me = Arc::new(Integrations {
            path: path.to_path_buf(),
            state: RwLock::new(State {
                config: Config::default(),
                modified: None,
                base_generation: first.generation,
                resolved: Resolved {
                    id: "default".into(),
                    generation: 1,
                    ..first
                },
            }),
            base,
            generation: AtomicU64::new(2),
            write: Mutex::default(),
        });
        if !path.exists() {
            me.save(&Config::default())?;
        }
        me.reload()?;
        Ok(me)
    }

    fn read_file(&self) -> Result<(Config, Option<SystemTime>), String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let mut config: Config =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", self.path.display()))?;
        config.validate()?;
        Ok((config, modified))
    }

    /// Re-reads the file. A file that doesn't validate is refused and the
    /// settings already running stay as they are.
    pub fn reload(&self) -> Result<(), String> {
        let (config, modified) = self.read_file()?;
        self.install(config, modified);
        Ok(())
    }

    pub fn reload_if_changed(&self) {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified == self.state.read().expect("integrations").modified {
            return;
        }
        match self.reload() {
            Ok(()) => tracing::info!("integrations reloaded from {}", self.path.display()),
            Err(e) => {
                // Remember the bad version so the warning isn't repeated.
                self.state.write().expect("integrations").modified = modified;
                tracing::warn!("integrations file not reloaded: {e}");
            }
        }
    }

    fn install(&self, config: Config, modified: Option<SystemTime>) {
        let base = self.base.resolved();
        let mut state = self.state.write().expect("integrations");
        state.modified = modified;
        if state.config == config
            && state.resolved.generation > 1
            && state.base_generation == base.generation
        {
            return;
        }
        // Workers keep their compiled runtime until the generation changes.
        state.resolved = Resolved {
            id: "default".into(),
            generation: self.generation.fetch_add(1, Relaxed),
            container: Arc::new(config.container(&base.container)),
            storage: base.storage,
            stats: base.stats,
        };
        state.base_generation = base.generation;
        state.config = config;
    }

    /// What a request should run against right now: rebuilt first when the
    /// container file changed underneath.
    pub fn resolved(&self) -> Resolved {
        let base = self.base.generation();
        {
            let state = self.state.read().expect("integrations");
            if state.base_generation == base {
                return state.resolved.clone();
            }
        }
        let (config, modified) = {
            let state = self.state.read().expect("integrations");
            (state.config.clone(), state.modified)
        };
        self.install(config, modified);
        self.state.read().expect("integrations").resolved.clone()
    }

    /// `base` with the platform tags added, as it would run.
    pub fn with_base(&self, base: &Container) -> Container {
        self.state
            .read()
            .expect("integrations")
            .config
            .container(base)
    }

    /// The container file the platform tags are added to.
    pub fn base(&self) -> &Arc<ContainerFile> {
        &self.base
    }

    /// What a test event for one provider runs against: its own runtime,
    /// storage and counters, so a test leaves the site's numbers and the
    /// enrichment cache alone.
    pub fn test_target(&self, id: &str, overrides: &TestOverrides) -> Result<Resolved, String> {
        let container = self
            .state
            .read()
            .expect("integrations")
            .config
            .test_container(&self.base.resolved().container, id, overrides)?;
        Ok(Resolved {
            id: format!("admin-test:{id}"),
            generation: self.generation.fetch_add(1, Relaxed),
            container: Arc::new(container),
            storage: Default::default(),
            stats: Default::default(),
        })
    }

    /// The site's address for test events: the first of the GA4 client's
    /// site domains, if the base container names any.
    pub fn site_url(&self) -> Option<String> {
        site_url(&self.base.resolved().container)
    }

    /// Everything the admin page needs. Secrets are never sent back, only
    /// whether they are set.
    pub fn view(&self) -> Value {
        let state = self.state.read().expect("integrations");
        let providers: Vec<Value> = PROVIDERS
            .iter()
            .map(|def| {
                let cfg = state
                    .config
                    .providers
                    .get(def.id)
                    .cloned()
                    .unwrap_or_default();
                let fields: Vec<Value> = def
                    .fields
                    .iter()
                    .map(|f| {
                        let v = cfg.fields.get(f.key).cloned().unwrap_or_default();
                        json!({
                            "key": f.key,
                            "label": f.label,
                            "kind": match f.kind {
                                Kind::Text => "text",
                                Kind::Secret => "secret",
                                Kind::Toggle => "toggle",
                            },
                            "required": f.required,
                            "help": f.help,
                            "value": if f.kind == Kind::Secret { String::new() } else { v.clone() },
                            "set": !v.is_empty(),
                        })
                    })
                    .collect();
                json!({
                    "id": def.id,
                    "name": def.name,
                    "help": def.help,
                    "enabled": cfg.enabled,
                    "client": def.client,
                    "fields": fields,
                })
            })
            .collect();
        let e = &state.config.enrichment;
        json!({
            "providers": providers,
            "test_events": TEST_EVENTS,
            "enrichment": {
                "enabled": e.enabled,
                "url": e.url,
                "key": e.key,
                "header_name": e.header_name,
                "header_value_set": !e.header_value.is_empty(),
                "event_user_data": e.event_user_data,
            },
        })
    }

    /// Saves one provider. A secret sent empty keeps the stored one.
    pub fn set_provider(
        &self,
        id: &str,
        enabled: bool,
        fields: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let def = provider(id).ok_or("no such provider")?;
        let _w = self.write.lock().expect("write");
        let mut config = self.state.read().expect("integrations").config.clone();
        let old = config.providers.remove(id).unwrap_or_default();
        let mut next = ProviderConfig {
            enabled,
            fields: BTreeMap::new(),
        };
        for f in def.fields {
            let given = fields.get(f.key).cloned().unwrap_or_default();
            let v = if f.kind == Kind::Secret && given.trim().is_empty() {
                old.fields.get(f.key).cloned().unwrap_or_default()
            } else {
                given
            };
            next.fields.insert(f.key.to_string(), v);
        }
        for k in fields.keys() {
            if !def.fields.iter().any(|f| f.key == k) {
                return Err(format!("{}: unknown field {k}", def.name));
            }
        }
        config.providers.insert(id.to_string(), next);
        self.commit(config)
    }

    /// Saves the enrichment endpoint. A header value sent empty keeps the
    /// stored one, unless the header name is cleared too.
    pub fn set_enrichment(&self, mut e: Enrichment) -> Result<(), String> {
        let _w = self.write.lock().expect("write");
        let mut config = self.state.read().expect("integrations").config.clone();
        if e.header_value.trim().is_empty() && !e.header_name.trim().is_empty() {
            e.header_value = config.enrichment.header_value.clone();
        }
        config.enrichment = e;
        self.commit(config)
    }

    fn commit(&self, mut config: Config) -> Result<(), String> {
        config.validate()?;
        self.save(&config)?;
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        self.install(config, modified);
        Ok(())
    }

    fn save(&self, config: &Config) -> Result<(), String> {
        let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        crate::store::save(&self.path, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Arc<ContainerFile> {
        ContainerFile::fixed(Container::builtin(), Default::default(), Default::default())
    }

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("integrations-{}-{name}.json", std::process::id()))
    }

    fn open(path: &Path) -> Arc<Integrations> {
        std::fs::remove_file(path).ok();
        Integrations::open(path, builtin()).unwrap()
    }

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn enabling_adds_a_tag_and_bumps_the_generation() {
        let path = temp("enable");
        let i = open(&path);
        let before = i.resolved();
        assert_eq!(before.container.tags.len(), 1);
        i.set_provider(
            "meta",
            true,
            fields(&[("pixel_id", "123"), ("access_token", "tok")]),
        )
        .unwrap();
        let after = i.resolved();
        assert!(after.generation > before.generation);
        assert_eq!(after.container.tags.len(), 2);
        assert_eq!(after.container.tags[1].template, "meta_capi");
        assert_eq!(after.container.tags[1].data["pixel_id"], "123");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn secrets_are_hidden_and_kept_when_left_empty() {
        let path = temp("secret");
        let i = open(&path);
        i.set_provider(
            "meta",
            true,
            fields(&[("pixel_id", "123"), ("access_token", "s3cr3t")]),
        )
        .unwrap();
        let view = i.view().to_string();
        assert!(!view.contains("s3cr3t"), "{view}");
        i.set_provider(
            "meta",
            true,
            fields(&[("pixel_id", "456"), ("access_token", "")]),
        )
        .unwrap();
        let tag = &i.resolved().container.tags[1];
        assert_eq!(tag.data["pixel_id"], "456");
        assert_eq!(tag.data["access_token"], "s3cr3t");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn required_fields_and_unknown_names_are_refused() {
        let path = temp("invalid");
        let i = open(&path);
        let e = i
            .set_provider("meta", true, fields(&[("pixel_id", "123")]))
            .unwrap_err();
        assert!(e.contains("Access token is required"), "{e}");
        assert!(i.set_provider("nope", true, fields(&[])).is_err());
        assert!(
            i.set_provider("meta", false, fields(&[("bogus", "x")]))
                .is_err()
        );
        // A provider can be saved half-filled while it is off.
        i.set_provider("meta", false, fields(&[("pixel_id", "123")]))
            .unwrap();
        assert_eq!(i.resolved().container.tags.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn bigquery_ids_are_checked_and_get_no_enrichment() {
        let path = temp("bigquery");
        let i = open(&path);
        let ids = |table: &str| {
            fields(&[
                ("project_id", "my-project"),
                ("dataset_id", "analytics"),
                ("table_id", table),
            ])
        };
        let e = i
            .set_provider("bigquery", true, ids("events/x"))
            .unwrap_err();
        assert!(e.contains("odd project, dataset or table ID"), "{e}");
        i.set_enrichment(Enrichment {
            enabled: true,
            url: "https://shop.example/lookup".into(),
            ..Default::default()
        })
        .unwrap();
        i.set_provider("bigquery", true, ids("events")).unwrap();
        let tag = &i.resolved().container.tags[1];
        assert_eq!(tag.template, "bigquery_export");
        assert_eq!(tag.data["table_id"], "events");
        assert!(tag.data.get("enrich").is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn enrichment_is_validated_and_reaches_the_tags() {
        let path = temp("enrich");
        let i = open(&path);
        let bad = |url: &str| Enrichment {
            enabled: true,
            url: url.into(),
            ..Default::default()
        };
        assert!(i.set_enrichment(bad("http://shop.example/lookup")).is_err());
        assert!(i.set_enrichment(bad("")).is_err());
        i.set_provider(
            "tiktok",
            true,
            fields(&[("pixel_code", "P1"), ("access_token", "t")]),
        )
        .unwrap();
        i.set_enrichment(Enrichment {
            enabled: true,
            url: "https://shop.example/lookup".into(),
            key: "user_id".into(),
            header_name: "x-api-key".into(),
            header_value: "s3cret".into(),
            event_user_data: false,
        })
        .unwrap();
        let enrich = i.resolved().container.tags[1].data["enrich"].clone();
        assert_eq!(enrich["url"], "https://shop.example/lookup");
        assert_eq!(enrich["headerValue"], "s3cret");
        // Saving again without the header value keeps it.
        i.set_enrichment(Enrichment {
            enabled: true,
            url: "https://shop.example/lookup".into(),
            key: "client_id".into(),
            header_name: "x-api-key".into(),
            header_value: String::new(),
            event_user_data: false,
        })
        .unwrap();
        let enrich = i.resolved().container.tags[1].data["enrich"].clone();
        assert_eq!(enrich["headerValue"], "s3cret");
        assert_eq!(enrich["key"], "client_id");
        assert!(
            i.resolved().container.tags[1]
                .data
                .get("eventUserData")
                .is_none()
        );
        // The event's own user data needs no endpoint.
        i.set_enrichment(Enrichment {
            event_user_data: true,
            ..Default::default()
        })
        .unwrap();
        let data = &i.resolved().container.tags[1].data;
        assert_eq!(data["eventUserData"], true);
        assert!(data.get("enrich").is_none());
        assert_eq!(i.view()["enrichment"]["event_user_data"], true);
        assert!(!i.view().to_string().contains("s3cret"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_edit_to_the_container_file_rebuilds_the_platform_container() {
        let dir = std::env::temp_dir().join(format!("int-base-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("container.json");
        std::fs::write(
            &file,
            r#"{"clients":[],"tags":[{"name":"GA4","template":"ga4_tag"}]}"#,
        )
        .unwrap();
        let base = ContainerFile::open(&file, Default::default(), Default::default()).unwrap();
        let i = Integrations::open(&dir.join("integrations.json"), base.clone()).unwrap();
        i.set_provider(
            "meta",
            true,
            fields(&[("pixel_id", "1"), ("access_token", "t")]),
        )
        .unwrap();
        let before = i.resolved();
        base.delete(&base.revision().unwrap(), "GA4").unwrap();
        let after = i.resolved();
        assert!(after.generation > before.generation);
        let names: Vec<&str> = after
            .container
            .tags
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["Meta Conversions API"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn another_instance_picks_up_changes_and_bad_files_are_ignored() {
        let path = temp("reload");
        let a = open(&path);
        let b = Integrations::open(&path, builtin()).unwrap();
        a.set_provider(
            "pinterest",
            true,
            fields(&[("ad_account_id", "1"), ("access_token", "t")]),
        )
        .unwrap();
        assert_eq!(b.resolved().container.tags.len(), 1);
        b.reload().unwrap();
        assert_eq!(b.resolved().container.tags.len(), 2);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(b.reload().is_err());
        assert_eq!(b.resolved().container.tags.len(), 2);
        std::fs::remove_file(&path).ok();
    }

    /// Answers every send with 200 and keeps what was sent.
    struct Recorder(std::sync::Mutex<Vec<crate::sandbox::Outgoing>>);

    impl crate::sandbox::Transport for Recorder {
        fn send_all(
            &self,
            requests: Vec<crate::sandbox::Outgoing>,
        ) -> Vec<Result<crate::sandbox::SendResult, String>> {
            let mut sent = self.0.lock().unwrap();
            requests
                .into_iter()
                .map(|r| {
                    sent.push(r);
                    Ok(crate::sandbox::SendResult {
                        status: 200,
                        headers: vec![],
                        body: r#"{"events_received":1}"#.into(),
                    })
                })
                .collect()
        }
    }

    fn run_test(
        i: &Integrations,
        id: &str,
        event: &str,
        overrides: &TestOverrides,
    ) -> (crate::sandbox::RunResult, Vec<crate::sandbox::Outgoing>) {
        let Ok(target) = i.test_target(id, overrides) else {
            panic!("no test target for {id}");
        };
        let rec = Arc::new(Recorder(Default::default()));
        let engine = crate::sandbox::Engine::new(
            target.container.clone(),
            target.storage.clone(),
            rec.clone(),
            std::time::Duration::from_secs(2),
            32 * 1024 * 1024,
        )
        .unwrap();
        let body = test_event(
            event,
            "203.0.113.9",
            "Mozilla/5.0 test",
            "https://shop.example/",
        )
        .unwrap()
        .to_string();
        let r = engine.run(crate::request::IncomingRequest {
            method: "POST".into(),
            path: "/admin/test".into(),
            body,
            ..Default::default()
        });
        let sent = rec.0.lock().unwrap().clone();
        (r, sent)
    }

    #[test]
    fn a_test_event_runs_only_that_providers_tag_even_when_off() {
        let path = temp("test-event");
        let i = open(&path);
        i.set_provider(
            "meta",
            false,
            fields(&[("pixel_id", "123"), ("access_token", "tok")]),
        )
        .unwrap();
        let overrides = TestOverrides {
            test_event_code: "TEST123".into(),
            test_mode: false,
        };
        let (r, sent) = run_test(&i, "meta", "purchase", &overrides);
        assert_eq!(r.claimed_by.as_deref(), Some("Admin test"));
        assert_eq!(r.tags.len(), 1, "no GA4 tag: {:?}", r.tags);
        assert_eq!(r.tags[0].outcome, "success", "{:?}", r);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].url.contains("graph.facebook.com"));
        let body: Value = serde_json::from_str(&sent[0].body).unwrap();
        assert_eq!(body["test_event_code"], "TEST123");
        assert_eq!(body["data"][0]["event_name"], "Purchase");
        assert_eq!(body["data"][0]["custom_data"]["content_ids"][0], "TEST-1");
        assert_eq!(
            body["data"][0]["user_data"]["client_ip_address"],
            "203.0.113.9"
        );
        // Meta's answer to the POST is kept for the event log.
        assert_eq!(
            r.sends[0].detail.as_deref(),
            Some(r#"{"events_received":1}"#)
        );
        // The saved settings are untouched.
        assert!(i.resolved().container.tags.len() == 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_test_event_reports_a_skip_and_refuses_bad_requests() {
        let path = temp("test-skip");
        let i = open(&path);
        let none = TestOverrides::default();
        let Err(e) = i.test_target("tiktok", &none) else {
            panic!("needs settings");
        };
        assert!(e.contains("save the Pixel code first"), "{e}");
        assert!(i.test_target("nope", &none).is_err());
        i.set_provider(
            "tiktok",
            true,
            fields(&[("pixel_code", "C1"), ("access_token", "tok")]),
        )
        .unwrap();
        // TikTok has no page_view event.
        let (r, sent) = run_test(&i, "tiktok", "page_view", &none);
        assert_eq!(r.tags[0].outcome, "skipped");
        assert_eq!(r.tags[0].skip.as_deref(), Some("unmapped_event"));
        assert!(sent.is_empty());
        let mode = TestOverrides {
            test_event_code: String::new(),
            test_mode: true,
        };
        assert!(i.test_target("tiktok", &mode).is_err());
        assert!(test_event("refund", "", "", "").is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn event_maps_are_checked_and_test_events_reach_linkedin() {
        let path = temp("event-map");
        let i = open(&path);
        for bad in ["purchase", "purchase=", "=123", "purchase=1 2", "a=1;b=2"] {
            assert!(
                i.set_provider(
                    "linkedin",
                    false,
                    fields(&[("access_token", "tok"), ("conversions", bad)]),
                )
                .is_err(),
                "{bad}"
            );
        }
        i.set_provider(
            "linkedin",
            false,
            fields(&[
                ("access_token", "tok"),
                ("conversions", " purchase=123 , generate_lead=urn-4.5 ,"),
            ]),
        )
        .unwrap();
        // The test event carries an email, so LinkedIn can match it.
        let (r, sent) = run_test(&i, "linkedin", "purchase", &TestOverrides::default());
        assert_eq!(r.tags[0].outcome, "success", "{:?}", r);
        let body: Value = serde_json::from_str(&sent[0].body).unwrap();
        assert_eq!(body["conversion"], "urn:lla:llaPartnerConversion:123");
        assert_eq!(body["user"]["userIds"][0]["idType"], "SHA256_EMAIL");
        assert_eq!(body["conversionValue"]["currencyCode"], "TRY");
        std::fs::remove_file(&path).ok();
    }

    const SHOPIFY_SECRET: &str = "shpss_test_secret";

    fn shopify_order(test: bool) -> String {
        json!({
            "id": 5512345678901u64,
            "name": "#1001",
            "test": test,
            "email": "Buyer@Example.com",
            "phone": "+90 532 123 45 67",
            "currency": "TRY",
            "total_price": "250.00",
            "total_tax": "41.67",
            "browser_ip": "203.0.113.7",
            "client_details": { "user_agent": "Mozilla/5.0 shopper" },
            "customer": { "id": 7001 },
            "billing_address": {
                "first_name": "Ada", "last_name": "Yilmaz", "city": "Istanbul",
                "province_code": "34", "zip": "34000", "country_code": "TR"
            },
            "line_items": [
                { "product_id": 11, "variant_id": 22, "sku": "SKU-1", "title": "Mug",
                  "vendor": "Acme", "price": "100.00", "quantity": 2 },
                { "product_id": 12, "variant_id": 23, "sku": "", "title": "Tea",
                  "price": "50.00", "quantity": 1 }
            ]
        })
        .to_string()
    }

    fn shopify_signature(body: &str, secret: &str) -> String {
        use base64::Engine as _;
        use hmac::{Hmac, KeyInit, Mac};
        let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    /// Integrations with Shopify and Meta (sending without consent signals)
    /// on, and an engine for their live container.
    fn shopify_engine(
        path: &Path,
        item_id: &str,
    ) -> (Arc<Integrations>, crate::sandbox::Engine, Arc<Recorder>) {
        let i = open(path);
        i.set_provider(
            "shopify",
            true,
            fields(&[("webhook_secret", SHOPIFY_SECRET), ("item_id", item_id)]),
        )
        .unwrap();
        i.set_provider(
            "meta",
            true,
            fields(&[
                ("pixel_id", "123"),
                ("access_token", "tok"),
                ("send_without_consent", "true"),
            ]),
        )
        .unwrap();
        let rec = Arc::new(Recorder(Default::default()));
        let target = i.resolved();
        let engine = crate::sandbox::Engine::new(
            target.container.clone(),
            target.storage.clone(),
            rec.clone(),
            std::time::Duration::from_secs(2),
            32 * 1024 * 1024,
        )
        .unwrap();
        (i, engine, rec)
    }

    fn webhook(body: &str, signature: &str, topic: &str) -> crate::request::IncomingRequest {
        crate::request::IncomingRequest {
            method: "POST".into(),
            path: "/shopify/orders".into(),
            headers: vec![
                ("host".into(), "example.com".into()),
                ("x-shopify-hmac-sha256".into(), signature.into()),
                ("x-shopify-topic".into(), topic.into()),
                ("x-shopify-shop-domain".into(), "demo.myshopify.com".into()),
            ],
            body: body.into(),
            remote_address: "34.1.2.3".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_paid_shopify_order_reaches_meta_as_a_purchase() {
        let path = temp("shopify-paid");
        let (i, engine, rec) = shopify_engine(&path, "sku");
        let clients = &i.resolved().container.clients;
        assert_eq!(clients[0].template, "shopify_client");
        assert!(clients[0].data["webhook_secret"] == SHOPIFY_SECRET);

        let body = shopify_order(false);
        let sig = shopify_signature(&body, SHOPIFY_SECRET);
        let r = engine.run(webhook(&body, &sig, "orders/paid"));
        assert_eq!(r.claimed_by.as_deref(), Some("Shopify orders"));
        assert_eq!(r.response.status, 200, "{r:?}");
        let ga4 = r.tags.iter().find(|t| t.tag == "GA4").unwrap();
        assert_eq!(ga4.skip.as_deref(), Some("not_a_ga4_hit"));
        let sent = rec.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{r:?}");
        let body: Value = serde_json::from_str(&sent[0].body).unwrap();
        let e = &body["data"][0];
        assert_eq!(e["event_name"], "Purchase");
        assert_eq!(e["event_id"], "5512345678901");
        assert_eq!(e["custom_data"]["order_id"], "5512345678901");
        assert_eq!(e["custom_data"]["value"], 250.0);
        assert_eq!(e["custom_data"]["currency"], "TRY");
        // sku, falling back to the variant id when a line has none.
        assert_eq!(e["custom_data"]["content_ids"], json!(["SKU-1", "23"]));
        assert_eq!(e["custom_data"]["num_items"], 3);
        assert_eq!(e["user_data"]["client_ip_address"], "203.0.113.7");
        assert_eq!(e["user_data"]["client_user_agent"], "Mozilla/5.0 shopper");
        let sha = |v: &str| {
            use sha2::Digest;
            sha2::Sha256::digest(v.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(e["user_data"]["em"][0], sha("buyer@example.com"));
        assert_eq!(e["user_data"]["ph"][0], sha("905321234567"));
        assert_eq!(e["user_data"]["fn"][0], sha("ada"));
        assert_eq!(e["user_data"]["country"][0], sha("tr"));
        assert_eq!(e["user_data"]["external_id"][0], sha("7001"));
        assert_eq!(e["event_source_url"], "https://demo.myshopify.com/");

        // Shopify delivering the same order again sends nothing more.
        let r = engine.run(webhook(&shopify_order(false), &sig, "orders/paid"));
        assert_eq!(r.response.status, 200);
        assert!(r.tags.is_empty());
        assert!(r.logs.iter().any(|l| l.contains("shopify=duplicate")));
        assert_eq!(rec.0.lock().unwrap().len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn shopify_refuses_bad_signatures_and_skips_other_orders() {
        let path = temp("shopify-refuse");
        let (_i, engine, rec) = shopify_engine(&path, "");
        let body = shopify_order(false);
        let sig = shopify_signature(&body, SHOPIFY_SECRET);

        let wrong = shopify_signature(&body, "another secret");
        for given in [wrong.as_str(), ""] {
            let r = engine.run(webhook(&body, given, "orders/paid"));
            assert_eq!(r.response.status, 401);
            assert!(r.tags.is_empty());
        }
        // A body changed after signing.
        let tampered = body.replace("250.00", "1.00");
        assert_eq!(
            engine
                .run(webhook(&tampered, &sig, "orders/paid"))
                .response
                .status,
            401
        );

        let r = engine.run(webhook(&body, &sig, "orders/create"));
        assert_eq!(r.response.status, 200);
        assert!(r.tags.is_empty());

        let test = shopify_order(true);
        let r = engine.run(webhook(
            &test,
            &shopify_signature(&test, SHOPIFY_SECRET),
            "orders/paid",
        ));
        assert_eq!(r.response.status, 200);
        assert!(r.logs.iter().any(|l| l.contains("shopify=test_order")));

        let junk = "[1,2]";
        let r = engine.run(webhook(
            junk,
            &shopify_signature(junk, SHOPIFY_SECRET),
            "orders/paid",
        ));
        assert_eq!(r.response.status, 400);

        // Other paths are left to the other clients.
        let mut other = webhook(&body, &sig, "orders/paid");
        other.path = "/shopify/other".into();
        assert_ne!(
            engine.run(other).claimed_by.as_deref(),
            Some("Shopify orders")
        );
        assert!(rec.0.lock().unwrap().is_empty());

        // The default item id is the variant id.
        let r = engine.run(webhook(&body, &sig, "orders/paid"));
        assert_eq!(r.response.status, 200);
        let sent = rec.0.lock().unwrap().clone();
        let meta: Value = serde_json::from_str(&sent[0].body).unwrap();
        assert_eq!(
            meta["data"][0]["custom_data"]["content_ids"],
            json!(["22", "23"])
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn shopify_settings_are_checked_and_have_no_test_event() {
        let path = temp("shopify-settings");
        let i = open(&path);
        let e = i.set_provider("shopify", true, fields(&[])).unwrap_err();
        assert!(e.contains("Webhook signing secret is required"), "{e}");
        let e = i
            .set_provider(
                "shopify",
                true,
                fields(&[("webhook_secret", "s"), ("item_id", "handle")]),
            )
            .unwrap_err();
        assert!(e.contains("variant_id, product_id or sku"), "{e}");
        i.set_provider("shopify", true, fields(&[("webhook_secret", "s3cr3t")]))
            .unwrap();
        assert!(!i.view().to_string().contains("s3cr3t"));
        assert!(i.test_target("shopify", &TestOverrides::default()).is_err());
        // A client, not a tag.
        assert_eq!(i.resolved().container.tags.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn shopify_orders_name_the_site_from_its_site_domains() {
        let path = temp("shopify-site");
        std::fs::remove_file(&path).ok();
        let base = Container::builtin_with(&crate::container::Settings {
            site_domains: vec!["example.com".into()],
            ..Default::default()
        });
        let base = ContainerFile::fixed(base, Default::default(), Default::default());
        let i = Integrations::open(&path, base).unwrap();
        i.set_provider("shopify", true, fields(&[("webhook_secret", "s")]))
            .unwrap();
        let client = &i.resolved().container.clients[0];
        assert_eq!(client.template, "shopify_client");
        assert_eq!(client.data["siteUrl"], "https://example.com/");
        std::fs::remove_file(&path).ok();
    }
}
