use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
    u64,
};

use crate::{
    anyhow, await_send, balance,
    db::{
        NostrRelaysKey, NostrRelaysKeyPrefix, NostrWalletConnectConfig, NostrWalletConnectKey,
        NostrWalletConnectKeyPrefix,
    },
    error_to_flutter, federations, info_to_flutter,
    multimint::{FederationSelector, LightningSendOutcome},
    payment_preview, send,
};
use anyhow::bail;
use bitcoin::Network;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::{secret::RootSecretStrategy, Client};
use fedimint_core::{
    config::FederationId,
    db::{Database, IDatabaseTransactionOpsCoreTyped},
    encoding::Encodable,
    invite_code::InviteCode,
    task::TaskGroup,
    util::{retry, FmtCompact, SafeUrl},
};
use fedimint_derive_secret::ChildId;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{oneshot, RwLock},
    time::Instant,
};

pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://nostr.bitcoiner.social",
    "wss://relay.nostr.band",
    "wss://relay.damus.io",
    "wss://nostr.zebedee.cloud",
    "wss://relay.plebstr.com",
    "wss://relayer.fiatjaf.com",
    "wss://nostr-01.bolt.observer",
    "wss://nostr-relay.wlvs.space",
    "wss://relay.nostr.info",
    "wss://nostr-pub.wellorder.net",
    "wss://nostr1.tunnelsats.com",
];

pub const NWC_SUPPORTED_METHODS: &[&str] = &["get_info", "get_balance", "pay_invoice"];

#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum WalletConnectRequest {
    #[serde(rename = "pay_invoice")]
    PayInvoice { invoice: String },

    #[serde(rename = "get_balance")]
    GetBalance {},

    #[serde(rename = "get_info")]
    GetInfo {},
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result_type", content = "result")]
pub enum WalletConnectResponse {
    #[serde(rename = "get_info")]
    GetInfo {
        network: String,
        methods: Vec<String>,
    },

    #[serde(rename = "get_balance")]
    GetBalance { balance: u64 },

    #[serde(rename = "pay_invoice")]
    PayInvoice { preimage: String },
}

#[derive(Clone)]
pub(crate) struct NostrClient {
    nostr_client: nostr_sdk::Client,
    public_federations: Arc<RwLock<Vec<PublicFederation>>>,
    task_group: TaskGroup,
    db: Database,
    nwc_listeners: Arc<RwLock<BTreeMap<FederationId, oneshot::Sender<()>>>>,
    keys: nostr_sdk::Keys,
}

impl NostrClient {
    pub async fn new(db: Database, recover_relays: Vec<String>) -> anyhow::Result<NostrClient> {
        let start = Instant::now();
        // We need to derive a Nostr key from the Fedimint secret.
        // Currently we are using 1/0 as the derivation path, as it does not clash with anything used internally in
        // Fedimint.
        let entropy = Client::load_decodable_client_secret::<Vec<u8>>(&db).await?;
        let mnemonic = Mnemonic::from_entropy(&entropy)?;
        let global_root_secret = Bip39RootSecretStrategy::<12>::to_root_secret(&mnemonic);
        let nostr_root_secret = global_root_secret.child_key(ChildId(1));
        let nostr_key_secret = nostr_root_secret.child_key(ChildId(0));
        let keypair = nostr_key_secret.to_secp_key(fedimint_core::secp256k1::SECP256K1);
        let keys = nostr_sdk::Keys::new(keypair.secret_key().into());

        let client = nostr_sdk::Client::builder().signer(keys.clone()).build();

        let mut nostr_client = NostrClient {
            nostr_client: client,
            public_federations: Arc::new(RwLock::new(vec![])),
            task_group: TaskGroup::new(),
            db: db.clone(),
            nwc_listeners: Arc::new(RwLock::new(BTreeMap::new())),
            keys,
        };

        let mut background_nostr = nostr_client.clone();
        nostr_client
            .task_group
            .spawn_cancellable("update nostr feds", async move {
                info_to_flutter("Initializing Nostr relays...").await;
                background_nostr.add_relays_from_db(recover_relays).await;

                info_to_flutter("Updating federations from nostr in the background...").await;
                background_nostr.update_federations_from_nostr().await;
            });

        let mut dbtx = db.begin_transaction_nc().await;
        let federation_configs = dbtx
            .find_by_prefix(&NostrWalletConnectKeyPrefix)
            .await
            .collect::<Vec<_>>()
            .await;
        for (key, nwc_config) in federation_configs {
            nostr_client
                .spawn_listen_for_nwc(key.federation_id, nwc_config)
                .await;
        }

        info_to_flutter(format!("Initialized Nostr client in {:?}", start.elapsed())).await;
        Ok(nostr_client)
    }

    async fn add_relays_from_db(&self, mut recover_relays: Vec<String>) {
        info_to_flutter(format!("Recovery relays: {:?}", recover_relays)).await;
        let mut relays = Self::get_or_insert_default_relays(self.db.clone()).await;
        recover_relays.append(&mut relays);

        for relay in recover_relays {
            match self.nostr_client.add_relay(relay.as_str()).await {
                Ok(added) => {
                    if added {
                        info_to_flutter(format!("Successfully added relay: {relay}")).await;
                    }
                }
                Err(err) => {
                    error_to_flutter(format!(
                        "Could not add relay {}: {}",
                        relay,
                        err.fmt_compact()
                    ))
                    .await;
                }
            }
        }
    }

    pub async fn insert_relay(&self, relay_uri: String) -> anyhow::Result<()> {
        let added = self.nostr_client.add_relay(relay_uri.clone()).await?;
        if !added {
            bail!("Relay already added");
        }

        let Ok(relay) = self.nostr_client.relay(relay_uri.clone()).await else {
            bail!("Could not get relay");
        };

        relay.connect();
        relay.wait_for_connection(Duration::from_secs(15)).await;

        let status = relay.status();
        match status {
            nostr_sdk::RelayStatus::Connected => {
                info_to_flutter(format!("Connected to relay {}", relay_uri.clone())).await;

                let mut dbtx = self.db.begin_transaction().await;
                dbtx.insert_entry(&NostrRelaysKey { uri: relay_uri }, &SystemTime::now())
                    .await;
                dbtx.commit_tx().await;

                Ok(())
            }
            status => Err(anyhow!("Could not connect to relay: {status:?}")),
        }
    }

    pub async fn remove_relay(&self, relay_uri: String) -> anyhow::Result<()> {
        self.nostr_client.remove_relay(relay_uri.clone()).await?;
        let mut dbtx = self.db.begin_transaction().await;
        dbtx.remove_entry(&NostrRelaysKey { uri: relay_uri }).await;
        dbtx.commit_tx().await;

        Ok(())
    }

    async fn get_or_insert_default_relays(db: Database) -> Vec<String> {
        let mut dbtx = db.begin_transaction().await;
        let relays = dbtx
            .find_by_prefix(&NostrRelaysKeyPrefix)
            .await
            .map(|(k, _)| k.uri)
            .collect::<Vec<_>>()
            .await;
        if !relays.is_empty() {
            return relays;
        }

        for relay in DEFAULT_RELAYS {
            dbtx.insert_new_entry(
                &NostrRelaysKey {
                    uri: relay.to_string(),
                },
                &SystemTime::now(),
            )
            .await;
        }
        dbtx.commit_tx().await;
        DEFAULT_RELAYS.into_iter().map(|s| s.to_string()).collect()
    }

    async fn broadcast_nwc_info(nostr_client: &nostr_sdk::Client, federation_id: &FederationId) {
        let supported_methods = NWC_SUPPORTED_METHODS.join(" ");
        let event_builder =
            nostr_sdk::EventBuilder::new(nostr_sdk::Kind::WalletConnectInfo, supported_methods);
        match nostr_client.send_event_builder(event_builder).await {
            Ok(event_id) => {
                let hexid = event_id.to_hex();
                let success = event_id.success;
                let failed = event_id.failed;
                info_to_flutter(format!("FederationId: {federation_id} Successfully broadcasted WalletConnectInfo: {hexid} Success: {success:?} Failed: {failed:?}")).await;
            }
            Err(e) => {
                info_to_flutter(format!("Error sending WalletConnectInfo event: {e:?}")).await;
            }
        }
    }

    async fn spawn_listen_for_nwc(
        &mut self,
        federation_id: FederationId,
        nwc_config: NostrWalletConnectConfig,
    ) {
        let mut listeners = self.nwc_listeners.write().await;
        if let Some(listener) = listeners.remove(&federation_id) {
            info_to_flutter("Sending shutdown signal to previous listening thread").await;
            let _ = listener.send(());
        }
        let (sender, receiver) = oneshot::channel::<()>();
        listeners.insert(federation_id, sender);
        self.task_group
            .spawn_cancellable("nostr wallet connect", async move {
                Self::listen_for_nwc(&federation_id, nwc_config, receiver).await;
            });
    }

    async fn listen_for_nwc(
        federation_id: &FederationId,
        nwc_config: NostrWalletConnectConfig,
        mut receiver: oneshot::Receiver<()>,
    ) {
        let secret_key = nostr_sdk::SecretKey::from_slice(&nwc_config.secret_key)
            .expect("Could not create secret key");
        let keys =
            nostr_sdk::Keys::new_with_ctx(fedimint_core::secp256k1::SECP256K1, secret_key.clone());
        let nostr_client = nostr_sdk::Client::builder().signer(keys.clone()).build();

        let relay = nwc_config.relay.clone();
        if let Err(e) = nostr_client.add_relay(relay.clone()).await {
            info_to_flutter(format!(
                "Could not add NWC relay to NWC client {} {e:?}",
                nwc_config.relay
            ))
            .await;
            return;
        }

        let Ok(relay) = nostr_client.relay(relay).await else {
            info_to_flutter("Could not get relay").await;
            return;
        };

        let status = relay.status();
        info_to_flutter(format!("Relay connection status: {status:?}")).await;
        relay.connect();
        info_to_flutter("Waiting for connection to relay...").await;
        relay
            .wait_for_connection(Duration::from_secs(u64::MAX))
            .await;
        info_to_flutter("Connected to relay!").await;

        let filter = nostr_sdk::Filter::new().kind(nostr_sdk::Kind::WalletConnectRequest);
        let Ok(subscription_id) = nostr_client.subscribe(filter, None).await else {
            info_to_flutter("Error subscribing to WalletConnectRequest").await;
            return;
        };

        Self::broadcast_nwc_info(&nostr_client, federation_id).await;

        let mut notifications = nostr_client.notifications();
        info_to_flutter(format!(
            "FederationId: {federation_id} Listening for NWC Requests..."
        ))
        .await;
        loop {
            tokio::select! {
                _ = &mut receiver => {
                    info_to_flutter(format!("Received shutdown signal for {federation_id}")).await;
                    break;
                }
                notification = notifications.recv() => {
                    let Ok(notification) = notification else {
                        info_to_flutter(format!("Received shutdown signal from notifications stream for {federation_id}")).await;
                        break;
                    };

                    let nostr_sdk::RelayPoolNotification::Event { event, .. } = notification else {
                        continue;
                    };

                    if event.kind == nostr_sdk::Kind::WalletConnectRequest {
                        let sender_pubkey = event.pubkey;
                        let Ok(decrypted) = nostr_sdk::nips::nip04::decrypt(&secret_key, &sender_pubkey, &event.content) else {
                            continue;
                        };

                        let Ok(request) = serde_json::from_str::<WalletConnectRequest>(&decrypted) else {
                            info_to_flutter("Error deserializing WalletConnectRequest").await;
                            continue;
                        };

                        info_to_flutter(format!("WalletConnectRequest: {request:?}")).await;
                        if let Err(err) = Self::handle_request(federation_id, &nostr_client, &keys, request, sender_pubkey, event.id).await {
                            info_to_flutter(format!("Error handling WalletConnectRequest: {err:?}")).await;
                        }
                    } else {
                        info_to_flutter(format!("Event was not a WalletConnectRequest, continuing... {}", event.kind)).await;
                    }
                }
            }
        }

        nostr_client.unsubscribe(&subscription_id).await;

        info_to_flutter(format!("FederationId: {federation_id} NWC Done listening")).await;
    }

    async fn broadcast_response(
        response: WalletConnectResponse,
        nostr_client: &nostr_sdk::Client,
        keys: &nostr_sdk::Keys,
        sender_pubkey: &nostr_sdk::PublicKey,
        request_event_id: nostr_sdk::EventId,
    ) -> anyhow::Result<()> {
        let content = serde_json::to_string(&response)?;
        let encrypted_content =
            nostr_sdk::nips::nip04::encrypt(&keys.secret_key(), sender_pubkey, content.clone())?;

        let event_builder =
            nostr_sdk::EventBuilder::new(nostr_sdk::Kind::WalletConnectResponse, encrypted_content)
                .tag(nostr_sdk::Tag::public_key(keys.public_key))
                .tag(nostr_sdk::Tag::event(request_event_id));

        retry(
            "broadcast wallet response",
            fedimint_core::util::backoff_util::background_backoff(),
            || async {
                match nostr_client.send_event_builder(event_builder.clone()).await {
                    Ok(event_id) => {
                        info_to_flutter(format!("Broadcasted WalletConnectResponse: {event_id:?}"))
                            .await;
                        if event_id.failed.is_empty() && !event_id.success.is_empty() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        info_to_flutter(format!(
                            "Error broadcasting WalletConnect response: {e:?}"
                        ))
                        .await;
                    }
                }

                Err(anyhow!("Error broadcasting WalletConnect response"))
            },
        )
        .await?;
        Ok(())
    }

    async fn handle_request(
        federation_id: &FederationId,
        nostr_client: &nostr_sdk::Client,
        keys: &nostr_sdk::Keys,
        request: WalletConnectRequest,
        sender_pubkey: nostr_sdk::PublicKey,
        request_event_id: nostr_sdk::EventId,
    ) -> anyhow::Result<()> {
        match request {
            WalletConnectRequest::GetInfo {} => {
                let all_federations = federations().await;
                let federation_selector = all_federations
                    .iter()
                    .find(|fed| fed.0.federation_id == *federation_id);
                if let Some((selector, _)) = federation_selector {
                    let network = selector.network.clone().expect("Network is not set");
                    let supported_methods = NWC_SUPPORTED_METHODS
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>();
                    let response = WalletConnectResponse::GetInfo {
                        network,
                        methods: supported_methods,
                    };
                    Self::broadcast_response(
                        response,
                        nostr_client,
                        keys,
                        &sender_pubkey,
                        request_event_id,
                    )
                    .await?;
                }
            }
            WalletConnectRequest::GetBalance {} => {
                let balance = balance(federation_id).await;
                let response = WalletConnectResponse::GetBalance { balance };
                Self::broadcast_response(
                    response,
                    nostr_client,
                    keys,
                    &sender_pubkey,
                    request_event_id,
                )
                .await?;
            }
            WalletConnectRequest::PayInvoice { invoice } => {
                let payment_preview = payment_preview(federation_id, invoice.clone()).await?;
                info_to_flutter(format!(
                    "Processing NWC PayInvoice. PaymentPreview Gateway: {} IsLNv2: {}",
                    payment_preview.gateway, payment_preview.is_lnv2
                ))
                .await;
                let operation_id = send(
                    federation_id,
                    invoice,
                    payment_preview.gateway,
                    payment_preview.is_lnv2,
                    payment_preview.amount_with_fees,
                    None,
                )
                .await?;
                let final_state = await_send(federation_id, operation_id).await;
                match final_state {
                    LightningSendOutcome::Success(preimage) => {
                        let response = WalletConnectResponse::PayInvoice { preimage };
                        Self::broadcast_response(
                            response,
                            nostr_client,
                            keys,
                            &sender_pubkey,
                            request_event_id,
                        )
                        .await?;
                    }
                    LightningSendOutcome::Failure => {
                        info_to_flutter(format!("NWC Payment Failure")).await;
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn get_public_federations(&mut self, force_update: bool) -> Vec<PublicFederation> {
        let update = {
            let public_federations = self.public_federations.read().await;
            public_federations.is_empty() || force_update
        };

        if update {
            self.update_federations_from_nostr().await;
        }

        self.public_federations.read().await.clone()
    }

    async fn update_federations_from_nostr(&mut self) {
        self.nostr_client.connect().await;

        let filter = nostr_sdk::Filter::new().kind(nostr_sdk::Kind::from(38173));
        match self
            .nostr_client
            .fetch_events(filter, Duration::from_secs(3))
            .await
        {
            Ok(events) => {
                let all_events = events.to_vec();
                let events = all_events
                    .iter()
                    .filter_map(|event| {
                        match PublicFederation::parse_network(&event.tags) {
                            Ok(network) if network == Network::Regtest => {
                                // Skip over regtest advertisements
                                return None;
                            }
                            _ => {}
                        }

                        PublicFederation::try_from(event.clone()).ok()
                    })
                    .collect::<Vec<_>>();

                let mut public_federations = self.public_federations.write().await;
                *public_federations = events;
            }
            Err(e) => {
                error_to_flutter(format!("Failed to fetch events from nostr: {e}")).await;
            }
        }
    }

    pub async fn get_backup_invite_codes(&self) -> Vec<String> {
        let pubkey = self.keys.public_key;
        info_to_flutter(format!("Getting backup invite codes for {}", pubkey)).await;
        self.nostr_client.connect().await;

        let filter = nostr_sdk::Filter::new()
            .author(pubkey)
            .kind(nostr_sdk::Kind::from(30000))
            .custom_tag(
                nostr_sdk::SingleLetterTag {
                    character: nostr_sdk::Alphabet::D,
                    uppercase: false,
                },
                "fedimint-backup",
            );
        let mut invite_codes: Vec<String> = Vec::new();
        match self
            .nostr_client
            .fetch_events(filter, Duration::from_secs(60))
            .await
        {
            Ok(events) => {
                let all_events = events.to_vec();
                for event in all_events {
                    if let Ok(decrypted) = nostr_sdk::nips::nip04::decrypt(
                        self.keys.secret_key(),
                        &pubkey,
                        event.content,
                    ) {
                        let codes = decrypted.split(",");
                        for code in codes {
                            if let Ok(_) = InviteCode::from_str(code) {
                                invite_codes.push(code.to_string());
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error_to_flutter(format!(
                    "Failed to fetch replaceable events from nostr: {e}"
                ))
                .await;
            }
        }

        invite_codes
    }

    pub async fn get_nwc_connection_info(&self) -> Vec<(FederationSelector, NWCConnectionInfo)> {
        let feds = federations().await;
        let mut dbtx = self.db.begin_transaction().await;
        let federation_configs = dbtx
            .find_by_prefix(&NostrWalletConnectKeyPrefix)
            .await
            .collect::<Vec<_>>()
            .await;
        federation_configs
            .iter()
            .map(|(key, config)| {
                let secret_key = nostr_sdk::SecretKey::from_slice(&config.secret_key)
                    .expect("Could not create secret key");
                let keys =
                    nostr_sdk::Keys::new_with_ctx(fedimint_core::secp256k1::SECP256K1, secret_key);
                let public_key = keys.public_key.to_hex();
                let selector = feds
                    .iter()
                    .find(|fed| fed.0.federation_id == key.federation_id)
                    .expect("Federation should exist")
                    .0
                    .clone();
                (
                    selector,
                    NWCConnectionInfo {
                        public_key,
                        relay: config.relay.clone(),
                        secret: config.secret_key.consensus_encode_to_hex(),
                    },
                )
            })
            .collect::<Vec<_>>()
    }

    pub async fn set_nwc_connection_info(
        &mut self,
        federation_id: FederationId,
        relay: String,
    ) -> NWCConnectionInfo {
        let mut dbtx = self.db.begin_transaction().await;
        let keys = nostr_sdk::Keys::generate();
        let nwc_config = NostrWalletConnectConfig {
            secret_key: keys
                .secret_key()
                .as_secret_bytes()
                .try_into()
                .expect("Could not serialize secret key"),
            relay: relay.clone(),
        };
        dbtx.insert_entry(&NostrWalletConnectKey { federation_id }, &nwc_config)
            .await;

        dbtx.commit_tx().await;

        let public_key = keys.public_key.to_hex();
        self.spawn_listen_for_nwc(federation_id, nwc_config).await;
        NWCConnectionInfo {
            public_key,
            relay,
            secret: keys.secret_key().to_secret_hex(),
        }
    }

    pub async fn get_relays(&self) -> Vec<(String, bool)> {
        let relays = Self::get_or_insert_default_relays(self.db.clone()).await;
        let mut relays_and_status = Vec::new();
        for uri in relays {
            if let Ok(relay) = self.nostr_client.relay(uri.clone()).await {
                relays_and_status.push((uri, relay.status() == nostr_sdk::RelayStatus::Connected));
            } else {
                relays_and_status.push((uri, false));
            }
        }

        relays_and_status
    }

    pub async fn backup_invite_codes(&self, invite_codes: Vec<String>) -> anyhow::Result<()> {
        self.nostr_client.connect().await;

        let pubkey = self.keys.public_key;
        let serialized_invite_codes = invite_codes.join(",");
        let encrypted_content = nostr_sdk::nips::nip04::encrypt(
            &self.keys.secret_key(),
            &pubkey,
            serialized_invite_codes,
        )?;

        let event_builder =
            nostr_sdk::EventBuilder::new(nostr_sdk::Kind::from(30000), encrypted_content)
                .tag(nostr_sdk::Tag::public_key(pubkey))
                .tag(nostr_sdk::Tag::custom(
                    nostr_sdk::TagKind::d(),
                    ["fedimint-backup"],
                ));

        retry(
            "broadcast fedimint backoff",
            fedimint_core::util::backoff_util::background_backoff(),
            || async {
                match self
                    .nostr_client
                    .send_event_builder(event_builder.clone())
                    .await
                {
                    Ok(event_id) => {
                        info_to_flutter(format!("Broadcasted Fedimint Backup: {event_id:?}")).await;
                        return Ok(());
                    }
                    Err(e) => {
                        info_to_flutter(format!("Error broadcasting Fedimint backup: {e:?}")).await;
                    }
                }

                Err(anyhow!("Error broadcasting Fedimint backup"))
            },
        )
        .await?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NWCConnectionInfo {
    pub public_key: String,
    pub relay: String,
    pub secret: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Debug)]
pub struct PublicFederation {
    pub federation_name: String,
    pub federation_id: FederationId,
    pub invite_codes: Vec<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub modules: Vec<String>,
    pub network: String,
}

impl TryFrom<nostr_sdk::Event> for PublicFederation {
    type Error = anyhow::Error;

    fn try_from(event: nostr_sdk::Event) -> Result<Self, Self::Error> {
        let tags = event.tags;
        let network = Self::parse_network(&tags)?;
        let (federation_name, about, picture) = Self::parse_content(event.content)?;
        let federation_id = Self::parse_federation_id(&tags)?;
        let invite_codes = Self::parse_invite_codes(&tags)?;
        let modules = Self::parse_modules(&tags)?;
        Ok(PublicFederation {
            federation_name,
            federation_id,
            invite_codes,
            about,
            picture,
            modules,
            network: network.to_string(),
        })
    }
}

impl PublicFederation {
    fn parse_network(tags: &nostr_sdk::Tags) -> anyhow::Result<Network> {
        let n_tag = tags
            .find(nostr_sdk::TagKind::SingleLetter(
                nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::N),
            ))
            .ok_or(anyhow::anyhow!("n_tag not present"))?;
        let network = n_tag
            .content()
            .ok_or(anyhow::anyhow!("n_tag has no content"))?;
        match network {
            "mainnet" => Ok(Network::Bitcoin),
            network_str => {
                let network = Network::from_str(network_str)?;
                Ok(network)
            }
        }
    }

    fn parse_content(content: String) -> anyhow::Result<(String, Option<String>, Option<String>)> {
        let json: Result<serde_json::Value, serde_json::Error> = serde_json::from_str(&content);
        match json {
            Ok(json) => {
                let federation_name = Self::parse_federation_name(&json)?;
                let about = json
                    .get("about")
                    .map(|val| val.as_str().expect("about is not a string").to_string());

                let picture = Self::parse_picture(&json);
                Ok((federation_name, about, picture))
            }
            Err(_) => {
                // Just interpret the entire content as the federation name
                Ok((content, None, None))
            }
        }
    }

    fn parse_federation_name(json: &serde_json::Value) -> anyhow::Result<String> {
        // First try to parse using the "name" key
        let federation_name = json.get("name");
        match federation_name {
            Some(name) => Ok(name
                .as_str()
                .ok_or(anyhow!("name is not a string"))?
                .to_string()),
            None => {
                // Try to parse using "federation_name" key
                let federation_name = json
                    .get("federation_name")
                    .ok_or(anyhow!("Could not get federation name"))?;
                Ok(federation_name
                    .as_str()
                    .ok_or(anyhow!("federation name is not a string"))?
                    .to_string())
            }
        }
    }

    fn parse_picture(json: &serde_json::Value) -> Option<String> {
        let picture = json.get("picture");
        match picture {
            Some(picture) => {
                match picture.as_str() {
                    Some(pic_url) => {
                        // Verify that the picture is a URL
                        let safe_url = SafeUrl::parse(pic_url).ok()?;
                        return Some(safe_url.to_string());
                    }
                    None => {}
                }
            }
            None => {}
        }
        None
    }

    fn parse_federation_id(tags: &nostr_sdk::Tags) -> anyhow::Result<FederationId> {
        let d_tag = tags.identifier().ok_or(anyhow!("d_tag is not present"))?;
        let federation_id = FederationId::from_str(d_tag)?;
        Ok(federation_id)
    }

    fn parse_invite_codes(tags: &nostr_sdk::Tags) -> anyhow::Result<Vec<String>> {
        let u_tag = tags
            .find(nostr_sdk::TagKind::SingleLetter(
                nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::U),
            ))
            .ok_or(anyhow!("u_tag does not exist"))?;
        let invite = u_tag
            .content()
            .ok_or(anyhow!("No content for u_tag"))?
            .to_string();
        Ok(vec![invite])
    }

    fn parse_modules(tags: &nostr_sdk::Tags) -> anyhow::Result<Vec<String>> {
        let modules = tags
            .find(nostr_sdk::TagKind::custom("modules".to_string()))
            .ok_or(anyhow!("No modules tag"))?
            .content()
            .ok_or(anyhow!("modules should have content"))?
            .split(",")
            .map(|m| m.to_string())
            .collect::<Vec<_>>();
        Ok(modules)
    }
}
