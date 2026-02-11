use anyhow::Context;
use ldk_server_client::client::LdkServerClient;
use ldk_server_protos::api::*;
use ldk_server_protos::types::PageToken;
use log::{debug, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use crate::config::Config;

/// Trait abstracting the LDK Server API surface used by LDKBoss.
///
/// This enables mock-based integration testing without a live server.
#[async_trait::async_trait]
pub trait LdkClient: Send + Sync {
    async fn get_node_info(&self) -> anyhow::Result<GetNodeInfoResponse>;
    async fn get_balances(&self) -> anyhow::Result<GetBalancesResponse>;
    async fn list_channels(&self) -> anyhow::Result<ListChannelsResponse>;
    async fn list_forwarded_payments(
        &self,
        page_token: Option<PageToken>,
    ) -> anyhow::Result<ListForwardedPaymentsResponse>;
    async fn update_channel_config(
        &self,
        request: UpdateChannelConfigRequest,
    ) -> anyhow::Result<UpdateChannelConfigResponse>;
    async fn connect_peer(
        &self,
        request: ConnectPeerRequest,
    ) -> anyhow::Result<ConnectPeerResponse>;
    async fn open_channel(
        &self,
        request: OpenChannelRequest,
    ) -> anyhow::Result<OpenChannelResponse>;
    async fn close_channel(
        &self,
        request: CloseChannelRequest,
    ) -> anyhow::Result<CloseChannelResponse>;
    async fn bolt11_receive(
        &self,
        request: Bolt11ReceiveRequest,
    ) -> anyhow::Result<Bolt11ReceiveResponse>;
    async fn bolt11_send(
        &self,
        request: Bolt11SendRequest,
    ) -> anyhow::Result<Bolt11SendResponse>;
    async fn force_close_channel(
        &self,
        request: ForceCloseChannelRequest,
    ) -> anyhow::Result<ForceCloseChannelResponse>;
}

/// Rate-limited, retrying wrapper around LdkServerClient.
pub struct LdkBossClient {
    inner: LdkServerClient,
    /// Semaphore for rate limiting (1 concurrent request)
    rate_limiter: Arc<Semaphore>,
}

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_MS: u64 = 1000;
const RATE_LIMIT_DELAY_MS: u64 = 100;

impl LdkBossClient {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let cert_pem = std::fs::read(&config.server.tls_cert_path).with_context(|| {
            format!(
                "Failed to read TLS cert at {}",
                config.server.tls_cert_path.display()
            )
        })?;

        let inner = LdkServerClient::new(
            config.server.base_url.clone(),
            config.server.api_key.clone(),
            &cert_pem,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create LDK Server client: {}", e))?;

        Ok(Self {
            inner,
            rate_limiter: Arc::new(Semaphore::new(1)),
        })
    }

    async fn rate_limit(&self) -> anyhow::Result<()> {
        let _permit = self.rate_limiter.acquire().await
            .map_err(|_| anyhow::anyhow!("Rate limiter semaphore closed"))?;
        sleep(Duration::from_millis(RATE_LIMIT_DELAY_MS)).await;
        Ok(())
    }

    async fn with_retry<F, Fut, T>(&self, name: &str, f: F) -> anyhow::Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, ldk_server_client::error::LdkServerError>>,
    {
        for attempt in 0..MAX_RETRIES {
            self.rate_limit().await?;
            match f().await {
                Ok(resp) => {
                    debug!("{}: success", name);
                    return Ok(resp);
                }
                Err(e) => {
                    if attempt < MAX_RETRIES - 1 {
                        let delay = RETRY_BASE_MS * 2u64.pow(attempt);
                        warn!(
                            "{}: attempt {} failed ({}), retrying in {}ms",
                            name,
                            attempt + 1,
                            e,
                            delay
                        );
                        sleep(Duration::from_millis(delay)).await;
                    } else {
                        return Err(anyhow::anyhow!(
                            "{}: all {} attempts failed: {}",
                            name,
                            MAX_RETRIES,
                            e
                        ));
                    }
                }
            }
        }
        unreachable!()
    }
}

#[async_trait::async_trait]
impl LdkClient for LdkBossClient {
    async fn get_node_info(&self) -> anyhow::Result<GetNodeInfoResponse> {
        self.with_retry("GetNodeInfo", || {
            self.inner.get_node_info(GetNodeInfoRequest {})
        })
        .await
    }

    async fn get_balances(&self) -> anyhow::Result<GetBalancesResponse> {
        self.with_retry("GetBalances", || {
            self.inner.get_balances(GetBalancesRequest {})
        })
        .await
    }

    async fn list_channels(&self) -> anyhow::Result<ListChannelsResponse> {
        self.with_retry("ListChannels", || {
            self.inner.list_channels(ListChannelsRequest {})
        })
        .await
    }

    async fn list_forwarded_payments(
        &self,
        page_token: Option<PageToken>,
    ) -> anyhow::Result<ListForwardedPaymentsResponse> {
        self.with_retry("ListForwardedPayments", || {
            self.inner
                .list_forwarded_payments(ListForwardedPaymentsRequest {
                    page_token: page_token.clone(),
                })
        })
        .await
    }

    async fn update_channel_config(
        &self,
        request: UpdateChannelConfigRequest,
    ) -> anyhow::Result<UpdateChannelConfigResponse> {
        self.with_retry("UpdateChannelConfig", || {
            self.inner.update_channel_config(request.clone())
        })
        .await
    }

    async fn connect_peer(
        &self,
        request: ConnectPeerRequest,
    ) -> anyhow::Result<ConnectPeerResponse> {
        self.with_retry("ConnectPeer", || {
            self.inner.connect_peer(request.clone())
        })
        .await
    }

    async fn open_channel(
        &self,
        request: OpenChannelRequest,
    ) -> anyhow::Result<OpenChannelResponse> {
        self.with_retry("OpenChannel", || {
            self.inner.open_channel(request.clone())
        })
        .await
    }

    async fn close_channel(
        &self,
        request: CloseChannelRequest,
    ) -> anyhow::Result<CloseChannelResponse> {
        self.with_retry("CloseChannel", || {
            self.inner.close_channel(request.clone())
        })
        .await
    }

    async fn bolt11_receive(
        &self,
        request: Bolt11ReceiveRequest,
    ) -> anyhow::Result<Bolt11ReceiveResponse> {
        self.with_retry("Bolt11Receive", || {
            self.inner.bolt11_receive(request.clone())
        })
        .await
    }

    async fn bolt11_send(
        &self,
        request: Bolt11SendRequest,
    ) -> anyhow::Result<Bolt11SendResponse> {
        self.with_retry("Bolt11Send", || {
            self.inner.bolt11_send(request.clone())
        })
        .await
    }

    async fn force_close_channel(
        &self,
        request: ForceCloseChannelRequest,
    ) -> anyhow::Result<ForceCloseChannelResponse> {
        self.with_retry("ForceCloseChannel", || {
            self.inner.force_close_channel(request.clone())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Mock client for integration testing
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mock LDK client that returns preset responses and records API calls.
    pub struct MockLdkClient {
        pub node_info: GetNodeInfoResponse,
        pub balances: GetBalancesResponse,
        pub channels: ListChannelsResponse,
        pub forwarded_payments: ListForwardedPaymentsResponse,
        // Call recorders
        pub update_config_calls: Arc<Mutex<Vec<UpdateChannelConfigRequest>>>,
        pub open_channel_calls: Arc<Mutex<Vec<OpenChannelRequest>>>,
        pub close_channel_calls: Arc<Mutex<Vec<CloseChannelRequest>>>,
        pub connect_peer_calls: Arc<Mutex<Vec<ConnectPeerRequest>>>,
        pub force_close_calls: Arc<Mutex<Vec<ForceCloseChannelRequest>>>,
    }

    impl MockLdkClient {
        pub fn new() -> Self {
            Self {
                node_info: GetNodeInfoResponse {
                    node_id: "mock_node_id_0000000000000000000000000000000000000000000000000000000000000000".to_string(),
                    ..Default::default()
                },
                balances: GetBalancesResponse::default(),
                channels: ListChannelsResponse::default(),
                forwarded_payments: ListForwardedPaymentsResponse::default(),
                update_config_calls: Arc::new(Mutex::new(Vec::new())),
                open_channel_calls: Arc::new(Mutex::new(Vec::new())),
                close_channel_calls: Arc::new(Mutex::new(Vec::new())),
                connect_peer_calls: Arc::new(Mutex::new(Vec::new())),
                force_close_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl LdkClient for MockLdkClient {
        async fn get_node_info(&self) -> anyhow::Result<GetNodeInfoResponse> {
            Ok(self.node_info.clone())
        }

        async fn get_balances(&self) -> anyhow::Result<GetBalancesResponse> {
            Ok(self.balances.clone())
        }

        async fn list_channels(&self) -> anyhow::Result<ListChannelsResponse> {
            Ok(self.channels.clone())
        }

        async fn list_forwarded_payments(
            &self,
            _page_token: Option<PageToken>,
        ) -> anyhow::Result<ListForwardedPaymentsResponse> {
            Ok(self.forwarded_payments.clone())
        }

        async fn update_channel_config(
            &self,
            request: UpdateChannelConfigRequest,
        ) -> anyhow::Result<UpdateChannelConfigResponse> {
            self.update_config_calls.lock().unwrap().push(request);
            Ok(UpdateChannelConfigResponse {})
        }

        async fn connect_peer(
            &self,
            request: ConnectPeerRequest,
        ) -> anyhow::Result<ConnectPeerResponse> {
            self.connect_peer_calls.lock().unwrap().push(request);
            Ok(ConnectPeerResponse {})
        }

        async fn open_channel(
            &self,
            request: OpenChannelRequest,
        ) -> anyhow::Result<OpenChannelResponse> {
            let user_channel_id = format!("mock_user_channel_{}", request.node_pubkey);
            self.open_channel_calls.lock().unwrap().push(request);
            Ok(OpenChannelResponse {
                user_channel_id,
            })
        }

        async fn close_channel(
            &self,
            request: CloseChannelRequest,
        ) -> anyhow::Result<CloseChannelResponse> {
            self.close_channel_calls.lock().unwrap().push(request);
            Ok(CloseChannelResponse {})
        }

        async fn bolt11_receive(
            &self,
            _request: Bolt11ReceiveRequest,
        ) -> anyhow::Result<Bolt11ReceiveResponse> {
            Ok(Bolt11ReceiveResponse {
                invoice: "lnbcrt1mock_invoice".to_string(),
            })
        }

        async fn bolt11_send(
            &self,
            _request: Bolt11SendRequest,
        ) -> anyhow::Result<Bolt11SendResponse> {
            Ok(Bolt11SendResponse {
                payment_id: "mock_payment_id".to_string(),
            })
        }

        async fn force_close_channel(
            &self,
            request: ForceCloseChannelRequest,
        ) -> anyhow::Result<ForceCloseChannelResponse> {
            self.force_close_calls.lock().unwrap().push(request);
            Ok(ForceCloseChannelResponse {})
        }
    }
}
