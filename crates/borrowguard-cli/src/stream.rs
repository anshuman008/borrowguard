//! Push notifications for the accounts positions depend on, so `watch` can
//! re-check a position as soon as its price moves instead of on a timer.
//!
//! Three sources, from free to fastest:
//! - `poll`: no stream, the timer alone (any RPC);
//! - `websocket`: `accountSubscribe` on the RPC's websocket endpoint (most
//!   RPCs, free tiers included);
//! - `grpc`: a Yellowstone gRPC stream (Helius LaserStream, Triton, …), built
//!   with `--features grpc`.
//!
//! The stream runs on its own thread with a small tokio runtime, reconnects
//! after errors, and reports each changed account on a channel.

use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use anyhow::{bail, Result};
use solana_sdk::pubkey::Pubkey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Poll,
    Websocket,
    Grpc,
}

#[derive(Debug, Clone)]
pub enum Source {
    Poll,
    Websocket {
        url: String,
    },
    // Read only by the gRPC client, which `--features grpc` compiles in.
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    Grpc {
        url: String,
        token: Option<String>,
    },
}

impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Source::Poll => "polling",
            Source::Websocket { .. } => "websocket",
            Source::Grpc { .. } => "gRPC",
        }
    }
}

/// The websocket endpoint that goes with an HTTP RPC URL. Local validators
/// (and Surfpool) serve it one port above the RPC.
pub fn websocket_url(rpc_url: &str) -> String {
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        return format!("wss://{rest}");
    }
    let Some(rest) = rpc_url.strip_prefix("http://") else { return rpc_url.to_string() };
    let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    match host.rsplit_once(':') {
        Some((name, port)) if ["127.0.0.1", "localhost"].contains(&name) => {
            let port: u16 = port.parse().unwrap_or(8899);
            let slash = if path.is_empty() { "" } else { "/" };
            format!("ws://{name}:{}{slash}{path}", port + 1)
        }
        _ => format!("ws://{rest}"),
    }
}

/// First wait before reconnecting; doubles while failing, up to the max.
const RECONNECT: Duration = Duration::from_secs(5);
const MAX_RECONNECT: Duration = Duration::from_secs(60);

/// A stream error that retrying won't fix, e.g. a plan without gRPC access.
#[derive(Debug)]
struct Fatal(String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

/// Start streaming changes to `accounts`. Returns `None` for polling.
pub fn start(source: &Source, accounts: Vec<Pubkey>) -> Result<Option<Receiver<Pubkey>>> {
    if matches!(source, Source::Poll) || accounts.is_empty() {
        return Ok(None);
    }
    if matches!(source, Source::Grpc { .. }) && !cfg!(feature = "grpc") {
        bail!(
            "this build has no gRPC support; rebuild with `cargo build --release --features grpc`"
        );
    }
    let (tx, rx) = mpsc::channel();
    let source = source.clone();
    thread::Builder::new().name("stream".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(err) => return eprintln!("stream: can't start runtime: {err}"),
        };
        runtime.block_on(async move {
            let mut wait = RECONNECT;
            loop {
                let mut delivered = false;
                let result = match &source {
                    Source::Poll => return,
                    Source::Websocket { url } => {
                        websocket(url, &accounts, &tx, &mut delivered).await
                    }
                    #[cfg(feature = "grpc")]
                    Source::Grpc { url, token } => {
                        grpc(url, token.as_deref(), &accounts, &tx, &mut delivered).await
                    }
                    #[cfg(not(feature = "grpc"))]
                    Source::Grpc { .. } => return,
                };
                let name = source.name();
                match result {
                    // The watch loop is gone.
                    Ok(Closed) => return,
                    Err(err) if err.downcast_ref::<Fatal>().is_some() => {
                        return eprintln!("stream ({name}): {err:#}; giving up, checking on the timer only");
                    }
                    Err(err) => {
                        if delivered {
                            wait = RECONNECT;
                        }
                        eprintln!(
                            "stream ({name}): {err:#}; reconnecting in {}s (positions are still polled)",
                            wait.as_secs()
                        );
                    }
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_RECONNECT);
            }
        });
    })?;
    Ok(Some(rx))
}

/// The receiving side hung up; stop streaming.
struct Closed;

async fn websocket(
    url: &str,
    accounts: &[Pubkey],
    tx: &Sender<Pubkey>,
    delivered: &mut bool,
) -> Result<Closed> {
    use futures_util::StreamExt;
    use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
    use solana_client::{
        nonblocking::pubsub_client::PubsubClient, rpc_config::RpcAccountInfoConfig,
    };
    use solana_sdk::commitment_config::CommitmentConfig;

    let client = PubsubClient::new(url).await?;
    let config = RpcAccountInfoConfig {
        // Same commitment the snapshots read at, so a notification is
        // never ahead of the state a re-check sees.
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: Some(UiAccountEncoding::Base64),
        // Only the fact of a change matters; skip the account data.
        data_slice: Some(UiDataSliceConfig { offset: 0, length: 0 }),
        ..Default::default()
    };
    let mut streams = Vec::with_capacity(accounts.len());
    for key in accounts {
        let (stream, _unsubscribe) = client.account_subscribe(key, Some(config.clone())).await?;
        streams.push(stream.map(move |_| *key));
    }
    let mut changes = futures_util::stream::select_all(streams);
    while let Some(key) = changes.next().await {
        if tx.send(key).is_err() {
            return Ok(Closed);
        }
        *delivered = true;
    }
    bail!("websocket closed")
}

#[cfg(feature = "grpc")]
async fn grpc(
    url: &str,
    token: Option<&str>,
    accounts: &[Pubkey],
    tx: &Sender<Pubkey>,
    delivered: &mut bool,
) -> Result<Closed> {
    use std::collections::HashMap;

    use anyhow::Context;
    use tonic::{
        metadata::{Ascii, MetadataValue},
        transport::{ClientTlsConfig, Endpoint},
        Code, Status,
    };
    use yellowstone_grpc_proto::geyser::{
        geyser_client::GeyserClient, subscribe_update::UpdateOneof, CommitmentLevel,
        SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestPing,
    };

    let mut endpoint = Endpoint::from_shared(url.to_string())?;
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }
    let channel = endpoint.connect().await.with_context(|| format!("connect to {url}"))?;
    let token: Option<MetadataValue<Ascii>> =
        token.map(str::parse).transpose().context("gRPC token isn't valid header text")?;
    let mut client = GeyserClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
        if let Some(token) = &token {
            req.metadata_mut().insert("x-token", token.clone());
        }
        Ok(req)
    })
    .max_decoding_message_size(64 * 1024 * 1024);

    let (requests, request_stream) = tokio::sync::mpsc::channel(8);
    let filter = SubscribeRequestFilterAccounts {
        account: accounts.iter().map(Pubkey::to_string).collect(),
        ..Default::default()
    };
    requests
        .send(SubscribeRequest {
            accounts: HashMap::from([("borrowguard".to_string(), filter)]),
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        })
        .await?;
    // Credentials or plan problems won't fix themselves by reconnecting.
    let classify = |status: Status| -> anyhow::Error {
        match status.code() {
            Code::PermissionDenied | Code::Unauthenticated | Code::Unimplemented => {
                Fatal(format!("{} ({:?})", status.message(), status.code())).into()
            }
            _ => anyhow::Error::new(status),
        }
    };
    let mut updates = client
        .subscribe(tokio_stream::wrappers::ReceiverStream::new(request_stream))
        .await
        .map_err(classify)
        .context("subscribe")?
        .into_inner();
    while let Some(update) = updates.message().await.map_err(classify)? {
        match update.update_oneof {
            Some(UpdateOneof::Account(account)) => {
                let Some(info) = account.account else { continue };
                let Ok(key) = Pubkey::try_from(info.pubkey.as_slice()) else { continue };
                if tx.send(key).is_err() {
                    return Ok(Closed);
                }
                *delivered = true;
            }
            // Servers drop clients that stop answering pings.
            Some(UpdateOneof::Ping(_)) => {
                requests
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    })
                    .await?;
            }
            _ => {}
        }
    }
    bail!("gRPC stream closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_follows_the_rpc_url() {
        assert_eq!(
            websocket_url("https://mainnet.helius-rpc.com/?api-key=k"),
            "wss://mainnet.helius-rpc.com/?api-key=k"
        );
        assert_eq!(websocket_url("http://127.0.0.1:8899"), "ws://127.0.0.1:8900");
        assert_eq!(websocket_url("http://node.example:80/rpc"), "ws://node.example:80/rpc");
    }

    #[test]
    fn polling_starts_no_stream() {
        assert!(start(&Source::Poll, vec![Pubkey::new_unique()]).unwrap().is_none());
        let ws = Source::Websocket { url: "ws://127.0.0.1:1".into() };
        assert!(start(&ws, vec![]).unwrap().is_none());
    }
}
