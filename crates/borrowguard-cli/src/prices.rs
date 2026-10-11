use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One OHLC bar. `time` is the bar's open time in unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Binance caps klines responses at 1,000 rows.
const PAGE: usize = 1_000;

pub fn load_csv(path: &Path) -> Result<Vec<Candle>> {
    let mut reader =
        csv::Reader::from_path(path).with_context(|| format!("open {}", path.display()))?;
    let candles = reader
        .deserialize()
        .collect::<Result<Vec<Candle>, _>>()
        .with_context(|| format!("parse {}", path.display()))?;
    if candles.windows(2).any(|w| w[1].time <= w[0].time) {
        bail!("{}: candles must be sorted by time", path.display());
    }
    Ok(candles)
}

pub fn write_csv(path: &Path, candles: &[Candle]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut writer =
        csv::Writer::from_path(path).with_context(|| format!("create {}", path.display()))?;
    for candle in candles {
        writer.serialize(candle)?;
    }
    writer.flush()?;
    Ok(())
}

/// Download `[start_ms, end_ms)` klines from a Binance-compatible API.
pub fn fetch_binance(
    base_url: &str,
    symbol: &str,
    interval: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<Candle>> {
    let client = reqwest::blocking::Client::new();
    let url = format!("{}/api/v3/klines", base_url.trim_end_matches('/'));
    let (end, limit) = (end_ms.to_string(), PAGE.to_string());
    let mut candles: Vec<Candle> = Vec::new();
    let mut cursor = start_ms;

    while cursor < end_ms {
        let start = cursor.to_string();
        let rows: Vec<Vec<Value>> = client
            .get(&url)
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("startTime", &start),
                ("endTime", &end),
                ("limit", &limit),
            ])
            .send()
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .json()?;
        for row in &rows {
            candles.push(parse_kline(row)?);
        }
        match candles.last() {
            Some(last) if rows.len() == PAGE => cursor = last.time + 1,
            _ => break,
        }
    }
    Ok(candles)
}

/// Kline row: `[open_time, "open", "high", "low", "close", ...]`.
fn parse_kline(row: &[Value]) -> Result<Candle> {
    let price = |i: usize| -> Result<f64> {
        row.get(i)
            .and_then(Value::as_str)
            .with_context(|| format!("kline field {i} missing"))?
            .parse()
            .with_context(|| format!("kline field {i} not a number"))
    };
    Ok(Candle {
        time: row.first().and_then(Value::as_i64).context("kline open time missing")?,
        open: price(1)?,
        high: price(2)?,
        low: price(3)?,
        close: price(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_kline() {
        let row: Vec<Value> = serde_json::from_str(
            r#"[1700000000000,"58.10","59.00","57.50","58.70","123.4",1700003599999]"#,
        )
        .unwrap();
        assert_eq!(
            parse_kline(&row).unwrap(),
            Candle { time: 1_700_000_000_000, open: 58.1, high: 59.0, low: 57.5, close: 58.7 }
        );
    }

    #[test]
    fn csv_round_trips() {
        let path = std::env::temp_dir().join(format!("borrowguard-{}.csv", std::process::id()));
        let candles = vec![
            Candle { time: 1, open: 1.0, high: 2.0, low: 0.5, close: 1.5 },
            Candle { time: 2, open: 1.5, high: 1.6, low: 1.4, close: 1.45 },
        ];
        write_csv(&path, &candles).unwrap();
        assert_eq!(load_csv(&path).unwrap(), candles);
        std::fs::remove_file(path).unwrap();
    }
}
