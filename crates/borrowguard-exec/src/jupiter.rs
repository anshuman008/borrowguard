//! Jupiter swap API: `GET /quote` then `POST /swap-instructions`, so the
//! swap can be embedded in our own transaction between other instructions.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

pub const DEFAULT_URL: &str = "https://lite-api.jup.ag/swap/v1";

pub struct Jupiter {
    http: reqwest::blocking::Client,
    base: String,
    api_key: Option<String>,
    /// Only route through these DEXes (Jupiter labels, comma-separated).
    dexes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Quote {
    raw: Value,
    pub in_amount: u64,
    pub out_amount: u64,
    /// Output guaranteed by the swap instruction (out_amount minus slippage).
    pub min_out: u64,
    pub price_impact_pct: f64,
}

pub struct SwapIxs {
    pub setup: Vec<Instruction>,
    pub swap: Instruction,
    pub cleanup: Option<Instruction>,
    pub lookup_tables: Vec<Pubkey>,
}

impl Jupiter {
    pub fn new(base: &str, api_key: Option<String>, dexes: Option<String>) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("static reqwest config");
        let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            api_key: set(api_key),
            dexes: set(dexes),
        }
    }

    fn request(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let req = match &self.api_key {
            Some(key) => req.header("x-api-key", key),
            None => req,
        };
        // The keyless API rate-limits bursts, and a close or rebalance makes
        // a few calls in a row: back off briefly instead of failing.
        let mut resp = req.try_clone().context("Jupiter request")?.send()?;
        for wait in [1, 2, 4] {
            if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                break;
            }
            std::thread::sleep(Duration::from_secs(wait));
            resp = req.try_clone().context("Jupiter request")?.send()?;
        }
        let status = resp.status();
        let body: Value = resp.json().context("Jupiter returned non-JSON")?;
        if !status.is_success() || body.get("error").is_some() {
            bail!("Jupiter {status}: {body}");
        }
        Ok(body)
    }

    /// Exact-in quote. `max_accounts` keeps the route small enough to share
    /// a transaction with the lending instructions.
    pub fn quote(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount: u64,
        slippage_bps: u16,
        max_accounts: u8,
    ) -> Result<Quote> {
        let mut query = vec![
            ("inputMint", input.to_string()),
            ("outputMint", output.to_string()),
            ("amount", amount.to_string()),
            ("slippageBps", slippage_bps.to_string()),
            ("swapMode", "ExactIn".to_string()),
            ("maxAccounts", max_accounts.to_string()),
        ];
        if let Some(dexes) = &self.dexes {
            query.push(("dexes", dexes.clone()));
        }
        let raw = self.request(self.http.get(format!("{}/quote", self.base)).query(&query))?;
        let num = |key: &str| -> Result<u64> {
            raw[key]
                .as_str()
                .with_context(|| format!("quote missing {key}"))?
                .parse()
                .map_err(Into::into)
        };
        Ok(Quote {
            in_amount: num("inAmount")?,
            out_amount: num("outAmount")?,
            min_out: num("otherAmountThreshold")?,
            price_impact_pct: raw["priceImpactPct"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            raw,
        })
    }

    /// Exact-in quote selling as little `input` as needed for the guaranteed
    /// output to reach `need`, starting from the estimate `sell`. One
    /// re-quote corrects for the route's actual price. Fails past `max_sell`.
    #[allow(clippy::too_many_arguments)]
    pub fn quote_covering(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        need: u64,
        mut sell: u64,
        max_sell: u64,
        slippage_bps: u16,
        max_accounts: u8,
    ) -> Result<Quote> {
        for _ in 0..3 {
            if sell > max_sell {
                bail!("selling {sell} of {max_sell} available still wouldn't cover {need}");
            }
            let quote = self.quote(input, output, sell, slippage_bps, max_accounts)?;
            if quote.min_out >= need {
                return Ok(quote);
            }
            sell = (sell as f64 * need as f64 / quote.min_out.max(1) as f64 * 1.002).ceil() as u64;
        }
        bail!("swap quotes kept moving; no quote covered {need}, retry")
    }

    /// Instructions for `quote`, signed by `user`. Jupiter's compute-budget
    /// instructions are dropped: the composer sets its own.
    pub fn swap_instructions(&self, quote: &Quote, user: &Pubkey) -> Result<SwapIxs> {
        let body = self.request(
            self.http.post(format!("{}/swap-instructions", self.base)).json(&json!({
                "quoteResponse": quote.raw,
                "userPublicKey": user.to_string(),
                // Keep wSOL as a token account: the lending instructions around
                // the swap move wSOL, not native SOL.
                "wrapAndUnwrapSol": false,
            })),
        )?;
        let ix = |v: &Value| -> Result<Instruction> {
            let accounts = v["accounts"]
                .as_array()
                .context("instruction without accounts")?
                .iter()
                .map(|a| {
                    let key: Pubkey = a["pubkey"].as_str().context("account pubkey")?.parse()?;
                    Ok(if a["isWritable"].as_bool().unwrap_or(false) {
                        AccountMeta::new(key, a["isSigner"].as_bool().unwrap_or(false))
                    } else {
                        AccountMeta::new_readonly(key, a["isSigner"].as_bool().unwrap_or(false))
                    })
                })
                .collect::<Result<_>>()?;
            Ok(Instruction {
                program_id: v["programId"].as_str().context("programId")?.parse()?,
                accounts,
                data: STANDARD.decode(v["data"].as_str().context("instruction data")?)?,
            })
        };
        Ok(SwapIxs {
            setup: body["setupInstructions"]
                .as_array()
                .map(|a| a.iter().map(ix).collect::<Result<_>>())
                .transpose()?
                .unwrap_or_default(),
            swap: ix(&body["swapInstruction"])?,
            cleanup: body.get("cleanupInstruction").filter(|v| !v.is_null()).map(ix).transpose()?,
            lookup_tables: body["addressLookupTableAddresses"]
                .as_array()
                .map(|a| a.iter().filter_map(|k| k.as_str()?.parse().ok()).collect())
                .unwrap_or_default(),
        })
    }
}
