//! The GA4 client's optional features: keeping platform cookies, restoring
//! click ids, visitor info and bot flagging. All are off unless set. They
//! live in the client's `data` (camelCase, as templates read it), in a site's
//! settings and in environment variables for the built-in container
//! (snake_case), and this is where both forms are checked.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Cookie groups the GA4 client knows the cookies and lifetimes of.
pub const COOKIE_GROUPS: &[&str] = &[
    "google_analytics",
    "google_ads",
    "meta",
    "tiktok",
    "pinterest",
    "microsoft",
    "snapchat",
    "linkedin",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Features {
    /// Cookie groups from [`COOKIE_GROUPS`], or cookie names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keep_cookies: Vec<String>,
    /// Click id parameter to the alias it's restored from, e.g. gclid → gc_id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub click_id_aliases: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub visitor_info: bool,
    /// `drop` (the default when empty) or `flag`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub bot_handling: String,
}

/// A cookie or query parameter name: letters, digits, `_ . -`.
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

impl Features {
    /// Trims and checks every field.
    pub fn validate(&mut self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        self.keep_cookies = self
            .keep_cookies
            .iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty() && seen.insert(c.clone()))
            .collect();
        if let Some(bad) = self.keep_cookies.iter().find(|c| !token(c)) {
            return Err(format!(
                "keep_cookies: \"{bad}\" isn't a cookie group ({}) or a cookie name",
                COOKIE_GROUPS.join(", ")
            ));
        }
        self.click_id_aliases = self
            .click_id_aliases
            .iter()
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        for (real, alias) in &self.click_id_aliases {
            if !token(real) || !token(alias) {
                return Err(format!(
                    "click_id_aliases: \"{real}\" → \"{alias}\" must be plain parameter names"
                ));
            }
            if real == alias {
                return Err(format!("click_id_aliases: {real} can't be its own alias"));
            }
        }
        self.bot_handling = self.bot_handling.trim().to_ascii_lowercase();
        if !matches!(self.bot_handling.as_str(), "" | "drop" | "flag") {
            return Err("bot_handling must be drop or flag".into());
        }
        if self.bot_handling == "drop" {
            self.bot_handling.clear();
        }
        Ok(())
    }

    /// The fields as the GA4 client's data keys. Empty fields are left out.
    pub fn client_data(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if !self.keep_cookies.is_empty() {
            m.insert("keepCookies".into(), json!(self.keep_cookies));
        }
        if !self.click_id_aliases.is_empty() {
            m.insert("clickIdAliases".into(), json!(self.click_id_aliases));
        }
        if self.visitor_info {
            m.insert("visitorInfo".into(), json!(true));
        }
        if !self.bot_handling.is_empty() {
            m.insert("botHandling".into(), json!(self.bot_handling));
        }
        m
    }

    /// Reads and checks the fields from a GA4 client's data.
    pub fn from_client_data(data: &Value) -> Result<Self, String> {
        let field = |k: &str| data.get(k).filter(|v| !v.is_null());
        let mut f = Features::default();
        if let Some(v) = field("keepCookies") {
            f.keep_cookies = serde_json::from_value(v.clone())
                .map_err(|_| "keepCookies must be a list of names".to_string())?;
        }
        if let Some(v) = field("clickIdAliases") {
            f.click_id_aliases = serde_json::from_value(v.clone()).map_err(|_| {
                "clickIdAliases must map click id names to aliases, e.g. {\"gclid\": \"gc_id\"}"
                    .to_string()
            })?;
        }
        if let Some(v) = field("visitorInfo") {
            f.visitor_info = v.as_bool().ok_or("visitorInfo must be true or false")?;
        }
        if let Some(v) = field("botHandling") {
            f.bot_handling = v
                .as_str()
                .ok_or("botHandling must be drop or flag")?
                .to_string();
        }
        f.validate()?;
        Ok(f)
    }

    /// Reads the built-in container's environment variables: KEEP_COOKIES
    /// (comma-separated), CLICK_ID_ALIASES (`gclid=gc_id,msclkid=ms_id`),
    /// VISITOR_INFO (1) and BOT_HANDLING.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let list = |k: &str| -> Vec<String> {
            get(k)
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let mut f = Features {
            keep_cookies: list("KEEP_COOKIES"),
            visitor_info: matches!(get("VISITOR_INFO").as_deref(), Some("1" | "true")),
            bot_handling: get("BOT_HANDLING").unwrap_or_default(),
            ..Default::default()
        };
        for pair in list("CLICK_ID_ALIASES") {
            let (real, alias) = pair.split_once('=').ok_or_else(|| {
                format!("CLICK_ID_ALIASES: \"{pair}\" should look like gclid=gc_id")
            })?;
            f.click_id_aliases.insert(real.into(), alias.into());
        }
        f.validate()?;
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_and_round_trips_client_data() {
        let data = json!({
            "keepCookies": [" meta ", "meta", "my_id"],
            "clickIdAliases": {"gclid": "gc_id"},
            "visitorInfo": true,
            "botHandling": "FLAG",
            "allowedMeasurementIds": ["G-1"],
        });
        let f = Features::from_client_data(&data).unwrap();
        assert_eq!(f.keep_cookies, ["meta", "my_id"]);
        assert_eq!(f.bot_handling, "flag");
        assert_eq!(
            Value::Object(f.client_data()),
            json!({"keepCookies": ["meta", "my_id"], "clickIdAliases": {"gclid": "gc_id"},
                   "visitorInfo": true, "botHandling": "flag"})
        );
        assert!(
            Features::from_client_data(&json!({}))
                .unwrap()
                .client_data()
                .is_empty()
        );

        for bad in [
            json!({"keepCookies": ["bad cookie"]}),
            json!({"keepCookies": "meta"}),
            json!({"clickIdAliases": {"gclid": "gclid"}}),
            json!({"clickIdAliases": {"gclid": "a&b"}}),
            json!({"visitorInfo": "yes"}),
            json!({"botHandling": "block"}),
        ] {
            assert!(Features::from_client_data(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_environment_variables() {
        let env = |k: &str| match k {
            "KEEP_COOKIES" => Some("google_ads, meta".to_string()),
            "CLICK_ID_ALIASES" => Some("gclid=gc_id,msclkid=ms_id".to_string()),
            "VISITOR_INFO" => Some("1".to_string()),
            _ => None,
        };
        let f = Features::from_env(env).unwrap();
        assert_eq!(f.keep_cookies, ["google_ads", "meta"]);
        assert_eq!(f.click_id_aliases["msclkid"], "ms_id");
        assert!(f.visitor_info);
        assert!(f.bot_handling.is_empty());
        assert!(
            Features::from_env(|k| (k == "CLICK_ID_ALIASES").then(|| "gclid".to_string())).is_err()
        );
    }
}
