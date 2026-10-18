use std::time::Duration;

use serde_json::json;

/// Sends alerts to Telegram and/or Discord. Delivery failures are logged,
/// never fatal: a broken webhook must not stop BorrowGuard.
pub struct Notifier {
    client: reqwest::blocking::Client,
    telegram: Option<(String, String)>,
    discord: Option<String>,
}

impl Notifier {
    pub fn new(
        telegram_token: Option<String>,
        telegram_chat: Option<String>,
        discord_webhook: Option<String>,
    ) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("static reqwest config");
        // Blank values (e.g. `DISCORD_WEBHOOK_URL=` from .env.example) mean "off".
        let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        Self {
            client,
            telegram: set(telegram_token).zip(set(telegram_chat)),
            discord: set(discord_webhook),
        }
    }

    pub fn channels(&self) -> Vec<&'static str> {
        [(self.telegram.is_some(), "telegram"), (self.discord.is_some(), "discord")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect()
    }

    pub fn send(&self, text: &str) {
        if let Some((token, chat)) = &self.telegram {
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            self.post("telegram", &url, json!({ "chat_id": chat, "text": text }));
        }
        if let Some(url) = &self.discord {
            self.post("discord", url, json!({ "content": text }));
        }
    }

    fn post(&self, channel: &str, url: &str, body: serde_json::Value) {
        let result = self.client.post(url).json(&body).send().and_then(|r| r.error_for_status());
        if let Err(err) = result {
            // reqwest errors can echo the URL, which holds the bot token / webhook secret.
            eprintln!("alert to {channel} failed: {}", err.without_url());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_credentials_disable_channels() {
        let blank = || Some(String::new());
        assert!(Notifier::new(blank(), blank(), blank()).channels().is_empty());
        let on = Notifier::new(Some("t".into()), Some("1".into()), Some("https://x".into()));
        assert_eq!(on.channels(), vec!["telegram", "discord"]);
        assert!(Notifier::new(Some("t".into()), None, None).channels().is_empty());
    }
}
